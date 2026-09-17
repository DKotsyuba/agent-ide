//! Contract checks for confined Seatbelt check execution (EYES-r1 §3).
//!
//! Every test drives the single confined spawn path against real `sandbox-exec` fixtures under
//! a short disposable scratch root. The suite is run with `--test-threads=1` so the fixture
//! directories and the process-tree survivor evidence stay deterministic.

#![cfg(target_os = "macos")]

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use agent_ide::execution::seatbelt::{
    ConfinedOutput, SeatbeltPolicy, render_profile, run_confined,
};

/// Shell used by the scripted confinement fixtures.
const SH: &str = "/bin/sh";

/// Reader used by the read-access fixtures.
const CAT: &str = "/bin/cat";

/// Network probe used by the egress-denial fixture.
const NC: &str = "/usr/bin/nc";

/// Check environment rebuilt from an allowlist, as the confinement contract mandates.
fn check_env() -> Vec<(String, String)> {
    vec![("PATH".to_string(), "/usr/bin:/bin".to_string())]
}

/// Allocates a fresh disposable scratch root below the short task-local path prefix.
///
/// The prefix stays short and unique per test process; any leftover from an earlier run is
/// replaced instead of reused.
fn scratch(name: &str) -> Scratch {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let base = PathBuf::from(format!(
        "/private/tmp/aiv3-t095-{}-{}",
        process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    let root = base.join(name);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&root).expect("scratch root is created");
    Scratch { base, root }
}

/// Removes one fixture tree with its content and the short base path when the test ends.
struct Scratch {
    /// Short base path holding the fixture root, removed on drop.
    base: PathBuf,
    /// Absolute path of the freshly created fixture root.
    root: PathBuf,
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
        let _ = fs::remove_dir_all(&self.base);
    }
}

impl Scratch {
    /// Creates the read-only root, the writable root, and the outside-sentinel directory.
    fn fixtures(&self) -> Fixtures {
        let read_root = self.root.join("r1");
        let write_root = self.root.join("rw");
        let outside = self.root.join("outside");
        fs::create_dir_all(&read_root).expect("read root is created");
        fs::create_dir_all(&write_root).expect("write root is created");
        fs::create_dir_all(&outside).expect("outside directory is created");
        Fixtures {
            read_root,
            write_root,
            outside,
        }
    }
}

/// Disposable confinement fixtures for one test.
struct Fixtures {
    /// Directory the confined check may read but never write.
    read_root: PathBuf,
    /// Directory the confined check may read and write.
    write_root: PathBuf,
    /// Directory outside every policy root; every access must fail.
    outside: PathBuf,
}

impl Fixtures {
    /// Builds the standard policy: one read-only root plus one private writable root.
    fn policy(&self) -> SeatbeltPolicy {
        SeatbeltPolicy {
            read_roots: vec![self.read_root.clone()],
            write_roots: vec![self.write_root.clone()],
        }
    }
}

/// Runs one confined fixture command with a generous byte budget.
async fn run(
    program: &str,
    args: &[&str],
    cwd: &Path,
    fixtures: &Fixtures,
    timeout: Duration,
) -> ConfinedOutput {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    run_confined(
        Path::new(program),
        &args,
        cwd,
        &check_env(),
        &fixtures.policy(),
        timeout,
        1 << 20,
    )
    .await
    .expect("confined run completes")
}

/// Script recording the shell and background-sleep pids, then sleeping past every deadline.
fn tree_script(pid_file: &Path) -> String {
    let pid_file = pid_file.display();
    format!("echo $$ > '{pid_file}'; sleep 300 & echo $! >> '{pid_file}'; sleep 300")
}

/// Reads the pids a fixture tree recorded before it was killed.
fn recorded_pids(pid_file: &Path) -> Vec<u32> {
    fs::read_to_string(pid_file)
        .expect("process tree recorded its pids")
        .split_whitespace()
        .map(|record| record.parse().expect("recorded pid parses"))
        .collect()
}

/// Reports whether a zero-signal probe still reaches the process.
fn probe_alive(pid: u32) -> bool {
    // SAFETY: kill with signal zero only probes delivery permissions.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Waits until no recorded pid answers a zero-signal probe, failing on any survivor.
///
/// Killed descendants are reaped by the system after reparenting, so they may answer the probe
/// for a few scheduling ticks; a live descendant cannot outlast this window.
async fn assert_no_survivors(pids: &[u32]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pids.iter().all(|pid| !probe_alive(*pid)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("processes survived the confined group kill: {pids:?}");
}

/// A read inside a read root succeeds and is captured.
#[tokio::test]
async fn read_inside_read_root_succeeds() {
    let scratch = scratch("read-inside");
    let fixtures = scratch.fixtures();
    fs::write(fixtures.read_root.join("inside.txt"), "inside\n").expect("fixture is written");
    let target = fixtures.read_root.join("inside.txt");

    let output = run(
        CAT,
        &[target.to_str().unwrap()],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_eq!(output.status, Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "inside\n");
    assert!(!output.timed_out);
    assert!(!output.truncated);
}

/// A read of a sentinel outside every root fails with a permission error, not a crash.
#[tokio::test]
async fn read_outside_sentinel_fails() {
    let scratch = scratch("read-outside");
    let fixtures = scratch.fixtures();
    fs::write(fixtures.outside.join("secret.txt"), "secret\n").expect("sentinel is written");
    let sentinel = fixtures.outside.join("secret.txt");

    let output = run(
        CAT,
        &[sentinel.to_str().unwrap()],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_ne!(output.status, Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Operation not permitted"),
        "denial must surface as a permission error: {:?}",
        output.stderr
    );
}

/// A write into the read-only root fails and leaves no file behind.
#[tokio::test]
async fn write_inside_read_only_root_fails() {
    let scratch = scratch("write-denied");
    let fixtures = scratch.fixtures();
    let guarded = fixtures.read_root.join("guarded.txt");
    let script = format!("echo pwned >> '{}'", guarded.display());

    let output = run(
        SH,
        &["-c", &script],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_ne!(output.status, Some(0));
    assert!(!guarded.exists(), "denied write must not create the file");
}

/// A write into the writable root succeeds and lands on disk.
#[tokio::test]
async fn write_inside_write_root_succeeds() {
    let scratch = scratch("write-allowed");
    let fixtures = scratch.fixtures();
    let written = fixtures.write_root.join("wrote.txt");
    let script = format!("echo cached_ok > '{}'", written.display());

    let output = run(
        SH,
        &["-c", &script],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_eq!(output.status, Some(0));
    assert_eq!(fs::read_to_string(&written).unwrap(), "cached_ok\n");
}

/// A root path containing quotes is escaped into a working SBPL literal, not rejected silently.
#[tokio::test]
async fn read_inside_escaped_quoted_root_succeeds() {
    let scratch = scratch("escaped-root");
    let fixtures = scratch.fixtures();
    let quoted = fixtures.read_root.join("quote\"dir");
    fs::create_dir_all(&quoted).expect("quoted directory is created");
    fs::write(quoted.join("q.txt"), "quoted\n").expect("fixture is written");
    let target = quoted.join("q.txt");

    let output = run(
        CAT,
        &[target.to_str().unwrap()],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_eq!(output.status, Some(0));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "quoted\n");
}

/// Outbound networking is denied by the kernel immediately instead of timing out.
#[tokio::test]
async fn network_connection_is_denied_without_timeout() {
    let scratch = scratch("network-denied");
    let fixtures = scratch.fixtures();
    let started = Instant::now();

    let output = run(
        NC,
        &["-z", "-G", "2", "1.1.1.1", "443"],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(20),
    )
    .await;

    assert_ne!(output.status, Some(0));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "network denial must fail fast, took {:?}",
        started.elapsed()
    );
}

/// A deadline expiring terminates, then kills, the whole process group, leaving no survivors.
#[tokio::test]
async fn timeout_kills_whole_process_tree() {
    let scratch = scratch("timeout-tree");
    let fixtures = scratch.fixtures();
    let pid_file = fixtures.write_root.join("pids");
    let script = tree_script(&pid_file);

    let output = run(
        SH,
        &["-c", &script],
        &fixtures.read_root,
        &fixtures,
        Duration::from_secs(2),
    )
    .await;

    assert!(output.timed_out);
    let pids = recorded_pids(&pid_file);
    assert_eq!(pids.len(), 2, "shell and background sleep must be recorded");
    assert_no_survivors(&pids).await;
}

/// Dropping the run future mid-flight kills the whole process group, leaving no survivors.
#[tokio::test]
async fn dropping_future_kills_whole_process_tree() {
    let scratch = scratch("dropped-future");
    let fixtures = scratch.fixtures();
    let pid_file = fixtures.write_root.join("pids");
    let script = tree_script(&pid_file);

    let handle = tokio::spawn({
        let program = PathBuf::from(SH);
        let args = vec![OsString::from("-c"), OsString::from(script)];
        let cwd = fixtures.read_root.clone();
        let env = check_env();
        let policy = fixtures.policy();
        async move {
            run_confined(
                &program,
                &args,
                &cwd,
                &env,
                &policy,
                Duration::from_secs(60),
                1 << 20,
            )
            .await
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "confined tree did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();
    let outcome = handle.await;
    assert!(
        matches!(&outcome, Err(error) if error.is_cancelled()),
        "the run future must have been cancelled, got {outcome:?}"
    );

    let pids = recorded_pids(&pid_file);
    assert_eq!(pids.len(), 2, "shell and background sleep must be recorded");
    assert_no_survivors(&pids).await;
}

/// Output beyond the byte budget is drained but retained only up to the cap, flagged truncated.
#[tokio::test]
async fn output_beyond_cap_is_truncated() {
    let scratch = scratch("output-cap");
    let fixtures = scratch.fixtures();
    let args = vec![
        OsString::from("-c"),
        OsString::from("yes overflow | head -c 50000"),
    ];

    let output = run_confined(
        Path::new(SH),
        &args,
        &fixtures.read_root,
        &check_env(),
        &fixtures.policy(),
        Duration::from_secs(20),
        128,
    )
    .await
    .expect("confined run completes");

    assert_eq!(output.status, Some(0));
    assert!(output.truncated);
    assert_eq!(output.stdout.len(), 128);
}

/// The generated profile carries every fixed allowance demanded by the confinement contract.
#[test]
fn profile_renders_fixed_confinement() {
    let profile = render_profile(&SeatbeltPolicy::default()).expect("empty policy renders");

    for expected in [
        "(version 1)",
        "(deny default)",
        "(deny network*)",
        "(allow process-fork)",
        "(allow process-exec)",
        "(allow signal (target self))",
        "(allow mach-lookup)",
        "(allow sysctl-read)",
        "(allow file-read-data (literal \"/\"))",
        "(allow file-read-metadata (subpath \"/\"))",
        "(subpath \"/usr/lib\")",
        "(subpath \"/private/etc\")",
        "(literal \"/dev/urandom\")",
        "(allow file-write-data (literal \"/dev/null\") (literal \"/dev/tty\"))",
    ] {
        assert!(
            profile.contains(expected),
            "profile is missing {expected}:\n{profile}"
        );
    }
}

/// An empty root list must not render a filter-less allow entry, which would allow everywhere.
#[test]
fn profile_with_empty_roots_has_no_unfiltered_allow() {
    let profile = render_profile(&SeatbeltPolicy::default()).expect("empty policy renders");

    assert!(!profile.contains("(allow file-read*)"));
    assert!(!profile.contains("(allow file-read* file-write*)"));
}

/// Quotes and backslashes in roots are escaped into safe SBPL string literals.
#[test]
fn profile_escapes_quotes_and_backslashes_in_roots() {
    let policy = SeatbeltPolicy {
        read_roots: vec![PathBuf::from("/tmp/quote\"dir\\dir")],
        write_roots: Vec::new(),
    };

    let profile = render_profile(&policy).expect("escaped roots render");

    assert!(
        profile.contains("(subpath \"/tmp/quote\\\"dir\\\\dir\")"),
        "escaped literal missing:\n{profile}"
    );
}

/// Roots without a safe single-line SBPL literal form reject the whole render.
#[test]
fn profile_rejects_unrepresentable_roots() {
    let newline = SeatbeltPolicy {
        read_roots: vec![PathBuf::from("/tmp/line\nbreak")],
        write_roots: Vec::new(),
    };
    assert!(render_profile(&newline).is_err());

    let carriage_return = SeatbeltPolicy {
        read_roots: Vec::new(),
        write_roots: vec![PathBuf::from("/tmp/line\rbreak")],
    };
    assert!(render_profile(&carriage_return).is_err());

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let policy = SeatbeltPolicy {
            read_roots: vec![PathBuf::from(OsString::from_vec(
                b"/tmp/not-utf8-\xff".to_vec(),
            ))],
            write_roots: Vec::new(),
        };
        assert!(render_profile(&policy).is_err());
    }
}
