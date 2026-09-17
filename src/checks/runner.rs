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
}
