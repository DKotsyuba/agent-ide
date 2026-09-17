//! Confined execution of project checks under the macOS Seatbelt sandbox.
//!
//! This module is the single spawn path for the confined background checks of the v0.3
//! problem-feed contract (EYES-r1 §3): every check process runs under `/usr/bin/sandbox-exec`
//! with a freshly generated profile, in its own process group, with a bounded runtime and a
//! bounded retained output. Timeout, cancellation, and post-exit sweeping kill and reap the
//! whole group through its process id only; no pattern-based process matching exists here.

use std::{
    ffi::OsString,
    fmt, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use tokio::{process::Command, task::JoinHandle};

/// Absolute path of the macOS Seatbelt launcher used for every confined spawn.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Time a group terminated with `SIGTERM` still has to exit before the runner escalates to
/// `SIGKILL`, matching the confinement contract's terminate grace.
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Time the retained-output drains may still run after the direct child has been reaped before
/// their partial data is abandoned as incomplete.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Fixed confinement preamble shared by every generated profile.
///
/// Beyond the deny rules and the process/mach/signal allowances demanded by the contract, the
/// macOS 27 bootstrap allowances are load-bearing: ordinary dynamic binaries read
/// `security.mac.lockdown_mode_state`/`kern.bootargs` via `sysctl` and stat the filesystem
/// root while starting, and denying either aborts children with `SIGABRT` instead of producing
/// a clean permission error (v03-service-sandbox probe evidence).
const PROFILE_PREAMBLE: &str = r#"(version 1)
(deny default)
(deny network*)

(allow process-fork)
(allow process-exec)
(allow signal (target self))
(allow mach-lookup)

; macOS 27 process bootstrap reads lockdown/boot arguments via sysctl and stats the
; filesystem root before any child instruction runs; denying either aborts children
; with SIGABRT instead of a clean permission error (service-sandbox probe evidence).
(allow sysctl-read)
(allow file-read-data (literal "/"))
(allow file-read-metadata (subpath "/"))

; system paths required to execute and run dynamic binaries, shells, and time data
(allow file-read*
  (subpath "/usr/lib")
  (subpath "/usr/bin")
  (subpath "/usr/libexec")
  (subpath "/bin")
  (subpath "/System/Library")
  (subpath "/private/var/db/timezone")
  (subpath "/private/etc")
  (literal "/dev/null")
  (literal "/dev/urandom")
  (literal "/dev/dtracehelper"))

(allow file-write-data (literal "/dev/null") (literal "/dev/tty"))
"#;

/// Declares the filesystem roots one confined check may read and write.
///
/// Roots are additive to the fixed system allowances of every generated profile. Each root is
/// rendered as one `subpath` filter; a root that cannot be represented as an SBPL string
/// literal makes the whole profile unavailable instead of being silently narrowed or skipped.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SeatbeltPolicy {
    /// Roots the check may read; the admitted worktree root belongs here.
    pub read_roots: Vec<PathBuf>,
    /// Roots the check may read and write; the check's private cache directory belongs here.
    pub write_roots: Vec<PathBuf>,
}

/// Explains why a seatbelt policy cannot be rendered into a safe profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeatbeltProfileError {
    /// The root has no safe single-line SBPL string-literal form: it is not representable as
    /// UTF-8, or it contains a newline character that would corrupt the profile grammar.
    UnrepresentableRoot {
        /// The rejected root path, kept for caller diagnostics.
        path: PathBuf,
    },
}

impl fmt::Display for SeatbeltProfileError {
    /// Formats the rejection without interpreting the path content.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnrepresentableRoot { path } => write!(
                formatter,
                "seatbelt policy root is not representable in an SBPL string literal: {path:?}"
            ),
        }
    }
}

impl std::error::Error for SeatbeltProfileError {}

/// Renders one confinement policy into the `sandbox-exec` profile applied to a check.
///
/// The profile denies everything by default, denies networking, allows process fork/exec,
/// self-signalling, and mach lookups, carries the macOS 27 bootstrap allowances, read-only
/// access to the fixed system paths, read-only access to every `SeatbeltPolicy::read_roots`
/// entry, and read-write access to every `SeatbeltPolicy::write_roots` entry. Root paths are
/// canonicalized when they exist (Seatbelt evaluates canonical paths) and escaped into safe SBPL
/// string literals; a root that cannot be represented safely fails the whole render rather than
/// weakening the profile.
///
/// # Errors
///
/// Returns `SeatbeltProfileError::UnrepresentableRoot` when any root has no safe literal form.
pub fn render_profile(policy: &SeatbeltPolicy) -> Result<String, SeatbeltProfileError> {
    let mut profile = String::from(PROFILE_PREAMBLE);
    append_roots(&mut profile, "read-only", "file-read*", &policy.read_roots)?;
    append_roots(
        &mut profile,
        "writable",
        "file-read* file-write*",
        &policy.write_roots,
    )?;
    Ok(profile)
}

/// Appends one `(allow ...)` entry per caller root, or nothing at all for an empty list.
///
/// An empty root list must stay silent: a filter-less `(allow ...)` entry in SBPL would allow
/// the operation on the whole filesystem instead of nowhere.
///
/// # Errors
///
/// Propagates `SeatbeltProfileError::UnrepresentableRoot` from the root rendering.
fn append_roots(
    profile: &mut String,
    role: &str,
    operation: &str,
    roots: &[PathBuf],
) -> Result<(), SeatbeltProfileError> {
    if roots.is_empty() {
        return Ok(());
    }
    profile.push_str(&format!(
        "\n; caller-provided {role} roots\n(allow {operation}"
    ));
    for root in roots {
        // Seatbelt matches `subpath` filters against canonical paths, so a root spelled through
        // a symlink (`/var/folders/...`, `/tmp/...`) would silently deny everything beneath it.
        // A root that cannot be canonicalized (not yet created) is rendered as given.
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        profile.push_str(&format!(" (subpath \"{}\")", sbpl_path(&canonical)?));
    }
    profile.push_str(")\n");
    Ok(())
}

/// Renders one root path as an escaped SBPL string literal.
///
/// Double quotes and backslashes are escaped with a leading backslash; the escaping form was
/// verified to compile with `sandbox-exec` and to keep the path meaning unchanged. Paths that
/// cannot be represented in a single-line UTF-8 literal are rejected.
///
/// # Errors
///
/// Returns `SeatbeltProfileError::UnrepresentableRoot` for non-UTF-8 paths and for paths
/// containing a carriage return or line feed.
fn sbpl_path(path: &Path) -> Result<String, SeatbeltProfileError> {
    let reject = || SeatbeltProfileError::UnrepresentableRoot {
        path: path.to_path_buf(),
    };
    let raw = path.as_os_str().to_str().ok_or_else(reject)?;
    let mut escaped = String::with_capacity(raw.len());
    for character in raw.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' | '\r' => return Err(reject()),
            _ => escaped.push(character),
        }
    }
    Ok(escaped)
}

/// The bounded result of one confined check run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfinedOutput {
    /// Exit code of the direct child; `None` when the child died from a signal, including the
    /// terminate/kill sequence this runner delivers on timeout.
    pub status: Option<i32>,
    /// Retained stdout bytes, capped at the configured byte budget.
    pub stdout: Vec<u8>,
    /// Retained stderr bytes, capped at the configured byte budget.
    pub stderr: Vec<u8>,
    /// Whether the runtime deadline expired and this runner killed the process group.
    pub timed_out: bool,
    /// Whether retained stdout or stderr omitted drained bytes beyond the byte budget.
    pub truncated: bool,
}

/// Runs one check program under a generated Seatbelt profile in its own process group.
///
/// The profile is written to a fresh `0600` private file and removed when the run ends,
/// including on failure and cancellation. The child is spawned as
/// `/usr/bin/sandbox-exec -f <profile> program args...` with a cleared environment rebuilt
/// from `env` alone, the given working directory, piped stdout/stderr, and a null stdin. Both
/// output pipes are drained concurrently; each stream retains at most `max_output_bytes` and
/// keeps draining beyond the cap so a chatty child cannot block on pipe backpressure.
///
/// When `timeout` expires, the whole process group receives `SIGTERM`, has `TERMINATE_GRACE`
/// to exit, and is then killed with `SIGKILL`; the direct child is reaped either way. After
/// the direct child is reaped, surviving group members (which can only be descendants still
/// holding an output pipe open) are swept with `SIGKILL` so pipes close promptly. Dropping the
/// returned future at any await point kills the whole group through a drop guard, so
/// cancellation never leaks sandboxed descendants. A killed direct child is reaped by the
/// tokio runtime (`kill_on_drop`); descendants are reparented and reaped by the system. Group
/// kills address only the recorded group process id; no pattern-based process matching is
/// used, and macOS does not recycle the process id within these bounded windows.
///
/// # Errors
///
/// Returns `io::ErrorKind::InvalidInput` when the policy cannot be rendered, and the
/// underlying spawn error when the profile file cannot be written or the child cannot be
/// spawned (including an unusable `cwd`).
pub async fn run_confined(
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    env: &[(String, String)],
    policy: &SeatbeltPolicy,
    timeout: Duration,
    max_output_bytes: usize,
) -> io::Result<ConfinedOutput> {
    let profile = render_profile(policy)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let profile_path = write_profile(&profile)?;
    let _profile_remover = ProfileFileRemover {
        path: profile_path.clone(),
    };

    let mut command = Command::new(SANDBOX_EXEC);
    command
        .arg("-f")
        .arg(&profile_path)
        .arg(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    super::configure_process_group(&mut command);

    let mut child = command.spawn()?;
    let group_pid = child.id().unwrap_or_default();
    let mut guard = GroupKillGuard { group_pid: None };
    guard.arm(group_pid);

    let stdout_drain = child
        .stdout
        .take()
        .map(|pipe| tokio::spawn(super::drain(pipe, max_output_bytes)));
    let stderr_drain = child
        .stderr
        .take()
        .map(|pipe| tokio::spawn(super::drain(pipe, max_output_bytes)));

    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let mut timed_out = false;
    let reap = tokio::select! {
        reap = child.wait() => reap,
        _ = &mut deadline => {
            timed_out = true;
            signal_group(group_pid, libc::SIGTERM);
            match tokio::time::timeout(TERMINATE_GRACE, child.wait()).await {
                Ok(reap) => reap,
                Err(_elapsed) => {
                    signal_group(group_pid, libc::SIGKILL);
                    child.wait().await
                }
            }
        }
    };

    // The direct child is reaped; any surviving group member is a descendant still holding an
    // output pipe open. Sweeping the group bounds such leaks and lets the drains reach EOF.
    signal_group(group_pid, libc::SIGKILL);
    guard.disarm();

    let (stdout, stdout_truncated) = join_drain(stdout_drain).await;
    let (stderr, stderr_truncated) = join_drain(stderr_drain).await;

    Ok(ConfinedOutput {
        status: exit_code(reap),
        stdout,
        stderr,
        timed_out,
        truncated: stdout_truncated || stderr_truncated,
    })
}

/// Converts one reap result into the child's exit code, reporting signal deaths as absent.
fn exit_code(reap: io::Result<ExitStatus>) -> Option<i32> {
    reap.ok().and_then(|status| status.code())
}

/// Joins one output-drain task within its grace period.
///
/// Returns the retained bytes and whether they are complete. A drain that fails, panics, or
/// outlives the drain grace loses its retained bytes and is reported as truncated, because the
/// group has been swept by then and the runner cannot distinguish a partial capture from a
/// complete one.
async fn join_drain(
    drain: Option<JoinHandle<io::Result<super::CapturedOutput>>>,
) -> (Vec<u8>, bool) {
    let Some(task) = drain else {
        return (Vec::new(), false);
    };
    tokio::pin!(task);
    match tokio::time::timeout(DRAIN_GRACE, &mut task).await {
        Ok(Ok(Ok(captured))) => (captured.bytes, captured.truncated),
        // Failed, panicked, or hung drains leave completeness unverifiable; aborting the hung
        // case releases the pipe reader promptly.
        Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
            task.abort();
            (Vec::new(), true)
        }
    }
}

/// Delivers one signal to every member of the confined process group, ignoring delivery errors.
///
/// A missing group (`ESRCH`) is the normal outcome once every member has exited; delivery
/// results are deliberately discarded because confinement is verified through the reap and the
/// pipes, never through signal acknowledgements. A zero group id is skipped: it would target
/// the runner's own group.
fn signal_group(group_pid: u32, signal: libc::c_int) {
    if group_pid == 0 {
        return;
    }
    #[cfg(unix)]
    {
        // SAFETY: `killpg` only delivers a signal; it performs no pointer access.
        unsafe {
            libc::killpg(group_pid as libc::pid_t, signal);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal;
    }
}

/// Kills the whole confined process group when dropped, unless disarmed after final reaping.
///
/// This is the cancellation backstop of the confinement contract: dropping the `run_confined`
/// future at any await point must not leave sandboxed descendants running. The guard stays
/// armed from spawn until the post-reap sweep, the last point where the group is still owned.
struct GroupKillGuard {
    /// Process id of the confined group (the direct child leads its own group) while owned.
    group_pid: Option<u32>,
}

impl GroupKillGuard {
    /// Arms the guard with the confined group's process id.
    fn arm(&mut self, group_pid: u32) {
        self.group_pid = Some(group_pid);
    }

    /// Disarms the guard once no group member can outlive the run.
    fn disarm(&mut self) {
        self.group_pid = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(group_pid) = self.group_pid {
            signal_group(group_pid, libc::SIGKILL);
        }
    }
}

/// Removes the generated profile file when the run ends, including on failure and cancellation.
struct ProfileFileRemover {
    /// Path of the private `0600` profile file this guard owns.
    path: PathBuf,
}

impl Drop for ProfileFileRemover {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Builds a collision-free private path for one generated profile file.
///
/// The name combines the process id with a process-local sequence number, so concurrent runs
/// in one process and parallel runner processes never share a profile path.
fn profile_path() -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        ".agent-ide-seatbelt-{}-{}.sb",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ))
}

/// Writes the generated profile to a fresh private `0600` file and returns its path.
///
/// The file is created with exclusive-create semantics so a colliding path fails closed. The
/// confined child itself cannot reach the file: `sandbox-exec` compiles the profile before the
/// sandbox is applied, and the child's profile grants no access to the temporary directory.
///
/// # Errors
///
/// Propagates the filesystem error when the file cannot be created exclusively or written.
#[cfg(unix)]
fn write_profile(profile: &str) -> io::Result<PathBuf> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};

    let path = profile_path();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(profile.as_bytes())?;
    Ok(path)
}

/// Non-Unix fallback writing the generated profile without a `0600` mode.
///
/// The confined-check path is macOS-only; this variant exists so the module compiles elsewhere
/// and fails naturally when `sandbox-exec` is absent.
///
/// # Errors
///
/// Propagates the filesystem error when the file cannot be created exclusively or written.
#[cfg(not(unix))]
fn write_profile(profile: &str) -> io::Result<PathBuf> {
    use std::io::Write;

    let path = profile_path();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(profile.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves a root spelled through a symlink is rendered canonically: Seatbelt evaluates the
    /// canonical path, so `/var/...` roots would otherwise deny every read beneath them.
    #[cfg(target_os = "macos")]
    #[test]
    fn render_profile_canonicalizes_symlinked_roots() {
        let policy = SeatbeltPolicy {
            read_roots: vec![PathBuf::from("/var/tmp")],
            write_roots: vec![PathBuf::from("/tmp")],
        };
        let profile = render_profile(&policy).unwrap();
        assert!(
            profile.contains("(subpath \"/private/var/tmp\")"),
            "{profile}"
        );
        assert!(profile.contains("(subpath \"/private/tmp\")"), "{profile}");
        assert!(!profile.contains("(subpath \"/var/tmp\")"), "{profile}");
    }

    /// Proves a root that does not exist yet is rendered as given instead of failing the render.
    #[test]
    fn render_profile_keeps_missing_roots_verbatim() {
        let missing = std::env::temp_dir().join(format!("aiv3-missing-{}", std::process::id()));
        let policy = SeatbeltPolicy {
            read_roots: vec![missing.clone()],
            write_roots: vec![],
        };
        let profile = render_profile(&policy).unwrap();
        assert!(
            profile.contains(&format!("(subpath \"{}\")", missing.display())),
            "{profile}"
        );
    }
}
