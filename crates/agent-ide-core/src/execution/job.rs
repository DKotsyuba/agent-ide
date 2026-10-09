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
    let lease = admit(admission, owner, class)?;
    let child = OwnedChild::spawn_job(command, lease, output_cap)
        .map_err(|error| settle(admission, error))?;
    // The job is owned by its own task so that dropping the caller (`cancelled` resolves) still
    // kills and reaps it and releases the slot.
    let (_alive, mut cancelled) = tokio::sync::oneshot::channel::<()>();
    let admission = admission.clone();
    let task = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + timeout;
        let finished = until_exit(child.process.identity.pid, deadline, &mut cancelled).await;
        // The group is torn down and the leader reaped only now, while its exited-but-unreaped
        // leader still proves the group is this job's own: never a later signal to a reused id.
        let completed = child
            .cancel_and_reap(TERM_GRACE, REAP_DEADLINE)
            .await
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

/// How often a job's leader is checked for an exit that leaves it unreaped.
const EXIT_POLL: Duration = Duration::from_millis(10);

/// Waits until the job leader `pid` has exited (left unreaped, so its group id stays owned) —
/// `true` — or `deadline` passes or `cancelled` resolves — `false`.
// ponytail: polls waitid(WNOWAIT) every 10 ms; a kqueue EVFILT_PROC wait if latency matters.
async fn until_exit(
    pid: u32,
    deadline: tokio::time::Instant,
    cancelled: &mut (impl std::future::Future + Unpin),
) -> bool {
    loop {
        if super::leader_exited_unreaped(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::select! {
            _ = tokio::time::sleep(EXIT_POLL) => {}
            _ = &mut *cancelled => return false,
        }
    }
}

/// One immediately granted slot of `owner` in `class`; a request that would queue is busy.
fn admit(
    admission: &Mutex<AdmissionController>,
    owner: OwnerId,
    class: AdmissionClass,
) -> io::Result<super::AdmissionLease> {
    let mut admission = admission
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match admission.submit(owner, class) {
        Admission::Granted(lease) => Ok(lease),
        Admission::Queued(ticket) => {
            admission.cancel_ticket(ticket);
            Err(io::Error::new(io::ErrorKind::WouldBlock, "busy"))
        }
        Admission::Refused(_) => Err(io::Error::new(io::ErrorKind::WouldBlock, "busy")),
    }
}

/// A long job whose output the caller streams itself (a test run): Execution owns the admitted
/// slot, the process group and the reap; the caller reads the pipes.
pub struct StreamedJob {
    /// The owned direct child.
    process: super::ChildOwnership,
    /// The admitted slot, released after the reap.
    lease: super::AdmissionLease,
    /// The reservation's role.
    target: super::SpawnTarget,
    /// The controller the slot belongs to.
    admission: Arc<Mutex<AdmissionController>>,
    /// Whether the direct child has exited (still unreaped).
    exited: bool,
}

impl StreamedJob {
    /// Admits and launches `command` for `owner` in `class`; returns the job with its stdout and
    /// stderr pipes. A full queue is `WouldBlock` (busy) and an absent program `NotFound`; nothing
    /// starts then.
    pub fn start(
        admission: &Arc<Mutex<AdmissionController>>,
        owner: OwnerId,
        class: AdmissionClass,
        command: &ControlledCommand,
    ) -> io::Result<(
        Self,
        tokio::process::ChildStdout,
        tokio::process::ChildStderr,
    )> {
        if command.kind != CommandKind::Job {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "only a core-built job runs here",
            ));
        }
        let settlement = SpawnNeverStarted::ordinary(admit(admission, owner, class)?);
        let (mut child, identity) = match super::launch_child(command, &settlement, 0, false) {
            Ok(launched) => launched,
            Err(error) => return Err(settle(admission, settlement.error(error))),
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            unreachable!("launch_child pipes stdout and stderr");
        };
        Ok((
            Self {
                process: super::ChildOwnership {
                    identity,
                    launch_identity: Some(super::ProcessIdentity(identity)),
                    child,
                    drainers: Vec::new(),
                    cancellation: None,
                },
                lease: settlement.lease,
                target: settlement.target,
                admission: admission.clone(),
                exited: false,
            },
            stdout,
            stderr,
        ))
    }

    /// Waits up to `budget` for the direct child to exit, leaving it unreaped; `false` while it
    /// still runs.
    pub async fn wait(&mut self, budget: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + budget;
        self.exited = until_exit(
            self.process.identity.pid,
            deadline,
            &mut std::future::pending::<()>(),
        )
        .await;
        self.exited
    }

    /// Tears the group down (TERM, grace, KILL) while the leader — exited or still running — is
    /// unreaped, then reaps it and releases the slot; returns the leader's exit status.
    pub async fn finish(mut self) -> io::Result<std::process::ExitStatus> {
        let status = super::cancel_owned(&mut self.process, TERM_GRACE, REAP_DEADLINE)
            .await
            .map_err(|error| io::Error::other(format!("{error:?}")))?;
        let proof = super::DirectChildReap {
            lease: self.lease,
            target: self.target,
            identity: self.process.identity,
        };
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release_reaped(proof)
            .map(|_| status)
            .map_err(|error| io::Error::other(format!("{error:?}")))
    }
}

/// `program` as an absolute executable: a path (relative to `base`) as given, or a bare name
/// looked up on the `:`-separated `path` like `execvp` would.
pub fn executable_on(
    program: &str,
    base: &std::path::Path,
    path: &str,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let runnable = |candidate: &std::path::Path| {
        std::fs::metadata(candidate)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let candidate = base.join(program);
        return runnable(&candidate).then_some(candidate);
    }
    path.split(':')
        .filter(|dir| std::path::Path::new(dir).is_absolute())
        .map(|dir| std::path::Path::new(dir).join(program))
        .find(|candidate| runnable(candidate))
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

    /// A streamed job hands its pipes to the caller; past its budget the whole group (a
    /// descendant included) is torn down, and every finish releases the slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streamed_jobs_tear_their_group_down_and_release_their_slot() {
        use tokio::io::AsyncReadExt;
        let admission = admission();
        let owner = || OwnerId::new("streamed-test").unwrap();
        let (mut job, mut stdout, _stderr) = StreamedJob::start(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell("echo streamed"),
        )
        .unwrap();
        assert!(job.wait(Duration::from_secs(10)).await);
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.unwrap();
        assert_eq!(text, "streamed\n");
        assert_eq!(job.finish().await.unwrap().code(), Some(0));
        let (mut job, _stdout, _stderr) = StreamedJob::start(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell("sleep 30 & echo $! ; wait"),
        )
        .unwrap();
        assert!(!job.wait(Duration::from_millis(300)).await);
        let started = std::time::Instant::now();
        job.finish().await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(admission.lock().unwrap().running_count(), 0);
        let Admission::Granted(_held) = admission
            .lock()
            .unwrap()
            .submit(owner(), AdmissionClass::Background)
        else {
            panic!("one slot is free");
        };
        let Admission::Granted(_held_too) = admission
            .lock()
            .unwrap()
            .submit(owner(), AdmissionClass::Background)
        else {
            panic!("two slots are free");
        };
        let Err(busy) = StreamedJob::start(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell("true"),
        ) else {
            panic!("a full controller starts nothing");
        };
        assert_eq!(busy.kind(), io::ErrorKind::WouldBlock);
    }

    /// A job whose leader exits normally while a TERM-resistant descendant lives on has its
    /// whole group torn down before the leader is reaped, for an ordinary and a streamed job:
    /// the leader's status is kept and the descendant does not survive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_finished_leader_takes_its_resistant_descendant_along() {
        use tokio::io::AsyncReadExt;
        let admission = admission();
        let owner = || OwnerId::new("descendant-test").unwrap();
        let script = "(trap '' TERM; exec sleep 30) >/dev/null 2>&1 & echo $!; exit 4";
        let gone = |pid: libc::pid_t| async move {
            for _ in 0..200 {
                // SAFETY: signal 0 only checks that the recorded descendant still exists.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            false
        };
        let output = run_job(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell(script),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(output.status, Some(4));
        let pid: libc::pid_t = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .unwrap();
        assert!(gone(pid).await, "the job's descendant survived");
        let (mut job, mut stdout, _stderr) = StreamedJob::start(
            &admission,
            owner(),
            AdmissionClass::Background,
            &shell(script),
        )
        .unwrap();
        assert!(job.wait(Duration::from_secs(10)).await);
        let mut text = String::new();
        stdout.read_to_string(&mut text).await.unwrap();
        assert_eq!(job.finish().await.unwrap().code(), Some(4));
        assert!(
            gone(text.trim().parse().unwrap()).await,
            "the streamed job's descendant survived"
        );
        assert_eq!(admission.lock().unwrap().running_count(), 0);
    }
}
