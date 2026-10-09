//! Core-built bounded jobs — a formatter or syntax probe over a candidate, a project check — run
//! as Execution-owned children: admitted before the spawn, launched from a measured executable
//! with a cleared environment in their own process group, captured under their class ceiling,
//! torn down as a group on timeout or cancellation and reaped before the slot is released.

use std::{
    io,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{
    Admission, AdmissionClass, AdmissionController, CommandKind, ControlledCommand, OwnedChild,
    OwnerId, SpawnNeverStarted,
};

/// Per-stream capture ceiling of a job: project checks keep their existing 64 MiB per stream.
pub const MAX_JOB_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
/// TERM-to-KILL grace of a job being torn down.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// Ceiling of the exit wait after the kill, and of the output drain after the exit.
const REAP_DEADLINE: Duration = Duration::from_secs(5);
/// Longest single wait Execution accepts; longer timeouts wait in slices.
const WAIT_SLICE: Duration = Duration::from_secs(60);

/// What a completed job produced.
#[derive(Debug, Default)]
pub struct JobOutput {
    /// Exit code; `None` when killed by a signal or the timeout.
    pub status: Option<i32>,
    /// Captured stdout, a prefix when truncated.
    pub stdout: Vec<u8>,
    /// Captured stderr, a prefix when truncated.
    pub stderr: Vec<u8>,
    /// The timeout expired and the group was killed.
    pub timed_out: bool,
    /// A stream reached the capture ceiling.
    pub truncated: bool,
}

impl OwnedChild {
    /// Launches a core-built [`CommandKind::Job`] from one reservation, returning the reservation
    /// on a definite pre-child failure.
    pub fn spawn_job(
        command: &ControlledCommand,
        lease: super::AdmissionLease,
        output_cap: usize,
    ) -> Result<Self, super::ProcessError> {
        let settlement = SpawnNeverStarted::ordinary(lease);
        if command.kind != CommandKind::Job {
            return Err(settlement.error(super::ProcessError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only a core-built job runs here",
            ))));
        }
        Self::spawn_parts(command, settlement, output_cap)
    }
}

/// Runs `command` for `owner` in admission `class` within `timeout`, capturing at most
/// `output_cap` bytes per stream. A full admission queue answers `WouldBlock` (busy) and starts
/// nothing. Dropping the returned future tears the job down and still reaps it before its slot
/// is released.
pub async fn run_job(
    admission: &Arc<Mutex<AdmissionController>>,
    owner: OwnerId,
    class: AdmissionClass,
    command: &ControlledCommand,
    output_cap: usize,
    timeout: Duration,
) -> io::Result<JobOutput> {
    let lease = {
        let mut admission = admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match admission.submit(owner, class) {
            Admission::Granted(lease) => lease,
            Admission::Queued(ticket) => {
                admission.cancel_ticket(ticket);
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"));
            }
            Admission::Refused(_) => {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"));
            }
        }
    };
    let mut child = OwnedChild::spawn_job(command, lease, output_cap)
        .map_err(|error| settle(admission, error))?;
    // The job is owned by its own task so that dropping the caller (`cancelled` resolves) still
    // kills and reaps it and releases the slot.
    let (_alive, mut cancelled) = tokio::sync::oneshot::channel::<()>();
    let admission = admission.clone();
    let task = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        let finished = loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break false;
            }
            tokio::select! {
                waited = child.wait(left.min(WAIT_SLICE)) => {
                    if waited.is_ok() {
                        break true;
                    }
                }
                _ = &mut cancelled => break false,
            }
        };
        let completed = if finished {
            // The direct child is gone; a surviving group member would only hold the output
            // pipes open, so the group is swept exactly as a confined check's always was.
            let _ = super::signal_group(child.process.identity.pid, libc::SIGKILL);
            child.reap(REAP_DEADLINE, REAP_DEADLINE).await
        } else {
            child.cancel_and_reap(TERM_GRACE, REAP_DEADLINE).await
        }
        .map_err(|error| io::Error::other(format!("{error:?}")))?;
        admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release_reaped(completed.settlement)
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
        let evidence = completed.evidence;
        Ok(JobOutput {
            status: evidence.status.code(),
            // An incomplete drain lost bytes it cannot account for: explicitly truncated.
            truncated: [&evidence.stdout, &evidence.stderr]
                .iter()
                .any(|output| output.truncated || !output.complete),
            stdout: evidence.stdout.bytes,
            stderr: evidence.stderr.bytes,
            timed_out: !finished,
        })
    });
    task.await.map_err(io::Error::other)?
}

/// Releases the slot of a spawn that definitely started nothing, and names the failure.
pub(crate) fn settle(
    admission: &Mutex<AdmissionController>,
    error: super::ProcessError,
) -> io::Error {
    match error {
        super::ProcessError::NeverStarted { cause, settlement } => {
            let _ = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .settle_never_started(settlement);
            match *cause {
                super::ProcessError::Io(error) => error,
                other => io::Error::other(format!("{other:?}")),
            }
        }
        other => io::Error::other(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

    use super::*;
    use crate::execution::AdmissionLimits;

    /// One admission controller with room for two jobs.
    fn admission() -> Arc<Mutex<AdmissionController>> {
        Arc::new(Mutex::new(
            AdmissionController::new(AdmissionLimits {
                total_running: 2,
                per_owner_running: 2,
                per_owner_queued: 1,
                total_queued: 1,
                interactive_burst: 1,
            })
            .unwrap(),
        ))
    }

    /// `/bin/sh -c script` as a job.
    fn shell(script: &str) -> ControlledCommand {
        ControlledCommand::from_validated_peer(
            CommandKind::Job,
            PathBuf::from("/bin/sh"),
            vec![OsString::from("-c"), OsString::from(script)],
            PathBuf::from("/"),
            BTreeMap::new(),
        )
        .unwrap()
    }

    /// A job's status and streams come back, a stream past the ceiling is truncated explicitly,
    /// a job past its timeout is killed as a group, and every slot is released after the reap.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jobs_capture_time_out_and_release_their_slot() {
        let admission = admission();
        let owner = || OwnerId::new("job-test").unwrap();
        let output = run_job(
            &admission,
            owner(),
            AdmissionClass::Interactive,
            &shell("cat; printf err >&2; exit 3"),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(
            (
                output.status,
                output.stdout.as_slice(),
                output.stderr.as_slice()
            ),
            (Some(3), &b""[..], &b"err"[..]),
            "stdin is closed, not inherited"
        );
        let output = run_job(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell("head -c 3000 /dev/zero"),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert!(output.truncated && output.stdout.len() == 1024);
        let started = std::time::Instant::now();
        let output = run_job(
            &admission,
            owner(),
            AdmissionClass::Interactive,
            &shell("sleep 30 & wait"),
            1024,
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert!(output.timed_out && output.status.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
        let cancelled = tokio::time::timeout(
            Duration::from_millis(300),
            run_job(
                &admission,
                owner(),
                AdmissionClass::Interactive,
                &shell("sleep 30"),
                1024,
                Duration::from_secs(30),
            ),
        )
        .await;
        assert!(cancelled.is_err());
        for _ in 0..100 {
            if admission.lock().unwrap().running_count() == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(admission.lock().unwrap().running_count(), 0);
    }
}
