//! Process-execution seam for project checkers.
//!
//! A checker describes one confined process as a [`RunSpec`] and hands it to a
//! [`ConfinedRunner`]. The production runner wraps the Seatbelt launcher in
//! `crate::execution::seatbelt`; tests use [`FakeRunner`] with scripted outputs so checker
//! logic (argument building, parsing, state mapping) runs without `sandbox-exec`.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::BoxFuture;
use crate::execution::seatbelt::{ReadDeny, SeatbeltPolicy, run_confined};

/// One confined process invocation requested by a checker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpec {
    /// Absolute executable path.
    pub program: PathBuf,
    /// Arguments after the program name.
    pub args: Vec<OsString>,
    /// Working directory; normally the admitted worktree.
    pub cwd: PathBuf,
    /// Complete environment; the runner passes nothing else to the child.
    pub env: Vec<(String, String)>,
    /// Roots the process may read.
    pub read_roots: Vec<PathBuf>,
    /// Roots the process may read and write (private cache and temp directories).
    pub write_roots: Vec<PathBuf>,
    /// Host read exclusions, intersected with all grants by Seatbelt.
    pub read_denies: Vec<ReadDeny>,
    /// Wall-clock limit after which the whole process group is killed.
    pub timeout: Duration,
    /// Per-stream capture limit in bytes; longer output is truncated, never buffered.
    pub max_output_bytes: usize,
}

/// Captured result of one confined run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunOutput {
    /// Exit status code; `None` when the process was killed by a signal or the timeout.
    pub status: Option<i32>,
    /// Captured standard output, possibly truncated.
    pub stdout: Vec<u8>,
    /// Captured standard error, possibly truncated.
    pub stderr: Vec<u8>,
    /// `true` when the timeout expired and the process group was killed.
    pub timed_out: bool,
    /// `true` when either stream exceeded `max_output_bytes`.
    pub truncated: bool,
}

/// Executes checker process specifications.
pub trait ConfinedRunner: Send + Sync {
    /// Runs one specification to completion; dropping the returned future cancels the process.
    fn run(&self, spec: RunSpec) -> BoxFuture<'_, io::Result<RunOutput>>;
}

/// Production runner: executes every specification under the generated Seatbelt profile of
/// [`run_confined`], so a project check reads only its declared roots, writes only its private
/// cache, has no network, and is killed as a whole process group on timeout or cancellation.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeatbeltRunner;

impl ConfinedRunner for SeatbeltRunner {
    fn run(&self, spec: RunSpec) -> BoxFuture<'_, io::Result<RunOutput>> {
        Box::pin(async move {
            let policy = SeatbeltPolicy {
                read_roots: spec.read_roots,
                write_roots: spec.write_roots,
                read_denies: spec.read_denies,
            };
            let output = run_confined(
                &spec.program,
                &spec.args,
                &spec.cwd,
                &spec.env,
                &policy,
                spec.timeout,
                spec.max_output_bytes,
            )
            .await?;
            Ok(RunOutput {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                timed_out: output.timed_out,
                truncated: output.truncated,
            })
        })
    }
}

/// Test substitute that replays scripted outputs in order and records every specification.
///
/// Once the scripted outputs are exhausted every further run fails with `NotFound`, so a test
/// that triggers more processes than it scripted fails loudly instead of passing by accident.
#[derive(Clone, Default)]
pub struct FakeRunner {
    /// Remaining scripted results, consumed front to back.
    outputs: Arc<Mutex<VecDeque<io::Result<RunOutput>>>>,
    /// Every specification received, in call order.
    specs: Arc<Mutex<Vec<RunSpec>>>,
}

impl FakeRunner {
    /// Creates a runner that yields `outputs` in order.
    pub fn new(outputs: Vec<io::Result<RunOutput>>) -> Self {
        Self {
            outputs: Arc::new(Mutex::new(outputs.into())),
            specs: Arc::default(),
        }
    }

    /// Creates a runner scripted with exactly one completed run of the given status and stdout.
    pub fn with_stdout(status: i32, stdout: &[u8]) -> Self {
        Self::new(vec![Ok(RunOutput {
            status: Some(status),
            stdout: stdout.to_vec(),
            ..RunOutput::default()
        })])
    }

    /// Returns every specification received so far, in call order.
    pub fn specs(&self) -> Vec<RunSpec> {
        self.specs.lock().expect("fake runner specs lock").clone()
    }
}

impl ConfinedRunner for FakeRunner {
    fn run(&self, spec: RunSpec) -> BoxFuture<'_, io::Result<RunOutput>> {
        self.specs
            .lock()
            .expect("fake runner specs lock")
            .push(spec);
        let next = self
            .outputs
            .lock()
            .expect("fake runner outputs lock")
            .pop_front();
        Box::pin(async move {
            next.unwrap_or_else(|| {
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "fake runner has no scripted output left",
                ))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves scripted outputs replay in order, specifications are recorded, and exhaustion fails.
    #[tokio::test]
    async fn fake_runner_replays_outputs_and_records_specs() {
        let runner = FakeRunner::new(vec![
            Ok(RunOutput {
                status: Some(0),
                stdout: b"first".to_vec(),
                ..RunOutput::default()
            }),
            Err(io::Error::other("scripted failure")),
        ]);
        let spec = RunSpec {
            program: PathBuf::from("/usr/bin/true"),
            args: vec![OsString::from("--flag")],
            cwd: PathBuf::from("/tmp"),
            env: vec![("PATH".to_owned(), "/usr/bin".to_owned())],
            read_roots: vec![PathBuf::from("/tmp")],
            write_roots: vec![],
            read_denies: vec![],
            timeout: Duration::from_secs(1),
            max_output_bytes: 16,
        };
        let first = runner.run(spec.clone()).await.unwrap();
        assert_eq!(first.stdout, b"first");
        assert!(runner.run(spec.clone()).await.is_err());
        let exhausted = runner.run(spec.clone()).await.unwrap_err();
        assert_eq!(exhausted.kind(), io::ErrorKind::NotFound);
        assert_eq!(runner.specs(), vec![spec.clone(), spec.clone(), spec]);
    }

    /// Proves the production adapter maps a specification onto a real confined run: the declared
    /// roots, not the caller's environment, decide what the child may read; a glob read deny
    /// narrows paths spelled through a TMPDIR alias and overrides a writable grant; and reads
    /// outside every declared root stay denied. The temp paths come from `TMPDIR` unnormalized
    /// (`/var/folders/...`), while the rendered profile evaluates canonical paths
    /// (`/private/var/...`), so an unnormalized deny base would silently miss.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_runner_runs_a_confined_child_and_denies_outside_reads() {
        let root = std::env::temp_dir().join(format!("aiv3-runner-{}", std::process::id()));
        let inside = root.join("inside");
        let outside = root.join("outside");
        let writable = root.join("writable");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&writable).unwrap();
        std::fs::write(inside.join("ok.txt"), "hello\n").unwrap();
        std::fs::write(inside.join("secret.key"), "secret\n").unwrap();
        std::fs::write(outside.join("secret.txt"), "secret\n").unwrap();
        std::fs::write(writable.join("notes.txt"), "notes\n").unwrap();
        std::fs::write(writable.join("secret.key"), "secret\n").unwrap();
        let spec = |target: PathBuf| RunSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![target.into_os_string()],
            cwd: inside.clone(),
            env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            read_roots: vec![inside.clone()],
            write_roots: vec![],
            read_denies: vec![],
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        };
        let allowed = SeatbeltRunner
            .run(spec(inside.join("ok.txt")))
            .await
            .unwrap();
        // A caller already confined by Seatbelt (for example an agent's own sandboxed shell)
        // cannot apply a nested profile; that is an environment limit, not an adapter defect.
        if allowed.stderr.windows(13).any(|w| w == b"sandbox_apply") {
            eprintln!("skipping: nested sandbox-exec is not permitted in this environment");
            let _ = std::fs::remove_dir_all(&root);
            return;
        }
        assert_eq!(
            allowed.status,
            Some(0),
            "read inside the read root must succeed: {allowed:?}"
        );
        assert_eq!(
            allowed.stdout, b"hello\n",
            "read inside the read root must capture stdout: {allowed:?}"
        );
        let mut denied_glob = spec(inside.join("secret.key"));
        denied_glob.read_denies = vec![ReadDeny::Glob {
            base: inside.clone(),
            suffix: crate::execution::seatbelt::CredentialGlob::Key,
        }];
        let denied_glob = SeatbeltRunner.run(denied_glob).await.unwrap();
        assert_ne!(
            denied_glob.status,
            Some(0),
            "glob read deny must block the granted inside/secret.key: {denied_glob:?}"
        );
        assert!(
            denied_glob.stdout.is_empty(),
            "glob read deny must leak no stdout for inside/secret.key: {denied_glob:?}"
        );
        let mut writable_allowed = spec(writable.join("notes.txt"));
        writable_allowed.write_roots = vec![writable.clone()];
        let writable_allowed = SeatbeltRunner.run(writable_allowed).await.unwrap();
        assert_eq!(
            writable_allowed.status,
            Some(0),
            "writable grant must allow the non-denied writable/notes.txt: {writable_allowed:?}"
        );
        assert_eq!(
            writable_allowed.stdout, b"notes\n",
            "writable grant must capture stdout for writable/notes.txt: {writable_allowed:?}"
        );
        let mut denied_writable = spec(writable.join("secret.key"));
        denied_writable.write_roots = vec![writable.clone()];
        denied_writable.read_denies = vec![ReadDeny::Glob {
            base: writable.clone(),
            suffix: crate::execution::seatbelt::CredentialGlob::Key,
        }];
        let denied_writable = SeatbeltRunner.run(denied_writable).await.unwrap();
        assert_ne!(
            denied_writable.status,
            Some(0),
            "glob read deny must override the writable grant for writable/secret.key: {denied_writable:?}"
        );
        assert!(
            denied_writable.stdout.is_empty(),
            "glob read deny must leak no stdout for writable/secret.key: {denied_writable:?}"
        );
        let denied = SeatbeltRunner
            .run(spec(outside.join("secret.txt")))
            .await
            .unwrap();
        assert_ne!(
            denied.status,
            Some(0),
            "read outside every declared root must stay denied: {denied:?}"
        );
        assert!(
            denied.stdout.is_empty(),
            "read outside every declared root must leak no stdout: {denied:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Proves a path deny still narrows when its final component is a path alias (`/tmp`) or
    /// an existing symlink: Seatbelt evaluates the resolved canonical path, so denying only
    /// the spelled path would miss reads that resolve through it. A positive control read
    /// under the same grants proves allows work before the negative cases, and a deny of a
    /// plain non-symlinked directory must keep its existing behavior.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_runner_denies_path_deny_through_alias_and_symlink() {
        let root = std::env::temp_dir().join(format!("aiv3-runner-link-{}", std::process::id()));
        let inside = root.join("inside");
        let real = inside.join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("secret.txt"), "secret\n").unwrap();
        std::fs::write(real.join("plain.txt"), "plain\n").unwrap();
        std::os::unix::fs::symlink("real", inside.join("link")).unwrap();
        let tmp_file = std::path::Path::new("/tmp")
            .join(format!("aiv3-runner-alias-{}.txt", std::process::id()));
        std::fs::write(&tmp_file, "secret\n").unwrap();
        let spec = |target: PathBuf, deny: PathBuf| RunSpec {
            program: PathBuf::from("/bin/cat"),
            args: vec![target.into_os_string()],
            cwd: inside.clone(),
            env: vec![("PATH".to_owned(), "/usr/bin:/bin".to_owned())],
            read_roots: vec![inside.clone(), PathBuf::from("/tmp")],
            write_roots: vec![],
            read_denies: vec![ReadDeny::Path(deny)],
            timeout: Duration::from_secs(10),
            max_output_bytes: 4096,
        };
        let control = SeatbeltRunner
            .run(spec(real.join("plain.txt"), real.join("nonexistent")))
            .await
            .unwrap();
        // A caller already confined by Seatbelt cannot apply a nested profile; that is an
        // environment limit, not an adapter defect.
        if control.stderr.windows(13).any(|w| w == b"sandbox_apply") {
            eprintln!("skipping: nested sandbox-exec is not permitted in this environment");
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(&tmp_file);
            return;
        }
        assert_eq!(
            control.status,
            Some(0),
            "positive control read of inside/real/plain.txt must succeed under the grants: {control:?}"
        );
        assert_eq!(
            control.stdout, b"plain\n",
            "positive control read must capture stdout: {control:?}"
        );
        let denied_symlink = SeatbeltRunner
            .run(spec(inside.join("link/secret.txt"), inside.join("link")))
            .await
            .unwrap();
        assert_ne!(
            denied_symlink.status,
            Some(0),
            "path deny of inside/link must deny the read resolving through the symlink: {denied_symlink:?}"
        );
        assert!(
            denied_symlink.stdout.is_empty(),
            "path deny of inside/link must leak no stdout: {denied_symlink:?}"
        );
        let denied_plain = SeatbeltRunner
            .run(spec(real.join("secret.txt"), real.clone()))
            .await
            .unwrap();
        assert_ne!(
            denied_plain.status,
            Some(0),
            "path deny of the plain directory inside/real must keep denying: {denied_plain:?}"
        );
        assert!(
            denied_plain.stdout.is_empty(),
            "path deny of the plain directory inside/real must leak no stdout: {denied_plain:?}"
        );
        let tmp_allowed = SeatbeltRunner
            .run(spec(tmp_file.clone(), real.join("nonexistent")))
            .await
            .unwrap();
        assert_eq!(
            tmp_allowed.status,
            Some(0),
            "positive control read of the /tmp file must succeed under the /tmp grant: {tmp_allowed:?}"
        );
        assert_eq!(
            tmp_allowed.stdout, b"secret\n",
            "positive control read of the /tmp file must capture stdout: {tmp_allowed:?}"
        );
        let denied_alias = SeatbeltRunner
            .run(spec(tmp_file.clone(), PathBuf::from("/tmp")))
            .await
            .unwrap();
        assert_ne!(
            denied_alias.status,
            Some(0),
            "path deny spelled /tmp must deny the read resolving to /private/tmp: {denied_alias:?}"
        );
        assert!(
            denied_alias.stdout.is_empty(),
            "path deny spelled /tmp must leak no stdout: {denied_alias:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&tmp_file);
    }
}
