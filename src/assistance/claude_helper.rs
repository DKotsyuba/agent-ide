//! Private helper endpoint: the daemon's claim/finish socket and the foreground helper that runs.
//!
//! The daemon side ([`serve`]) owns a private Unix listener inside the runtime directory. It
//! releases one closed [`HelperJob`] per successful atomic claim and records the helper's final
//! frame. It performs no Git, source or provider work of its own for a Claude operation.
//!
//! The helper side ([`run`]) is a short foreground process the model launches through its ordinary
//! `Bash` tool, so every child it starts inherits the host's own real sandbox. It claims its
//! operation once, performs the fixed Git discovery itself, reports bounded evidence with real
//! child-settlement counts, and exits. It never re-enters the Codex execution path and never
//! presents a synthetic sandbox observation.

use super::claude_worker::{
    ChildSettlement, ClaimOutcome, HelperJob, HelperOperation, HelperOutcome, HelperResult,
    LaunchLedger, MAX_HELPER_FRAME_BYTES,
};
use super::reply::FailureCode;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Fixed private endpoint name inside the daemon runtime directory.
pub const HELPER_SOCKET: &str = "claude-helper.sock";
/// Bounds one complete helper session, independent of the operation's own budget.
const SESSION_DEADLINE: Duration = Duration::from_secs(120);
/// Bounds a single frame read before any decoding occurs.
const MAX_FRAME: u32 = MAX_HELPER_FRAME_BYTES as u32;

/// One helper's request to claim the operation bound to a handle it was launched for.
///
/// Presenting this proves nothing on its own: the daemon still requires a native launch already
/// recognized for this exact actor, a matching channel, a live binding generation and an unexpired
/// single-use ticket before any job is released. The helper is deliberately never asked for its own
/// actor identity, which it could only repeat back from a value it was handed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    /// Closed wire revision; only the current helper protocol is accepted.
    pub protocol: u32,
    /// Action-scoped handle the helper was told to claim.
    pub detail_ref: String,
    /// Opaque private transport attachment the ticket was minted on.
    pub attachment: String,
}

/// The daemon's answer to one claim attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "claim", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClaimReply {
    /// The claim was first, in time and correctly correlated; the closed job follows.
    Granted(Box<HelperJob>),
    /// The claim was refused; no job, Git, source or provider effect occurred.
    Refused {
        /// Stable actionable failure category.
        code: FailureCode,
    },
}

/// Writes one length-prefixed bounded frame.
///
/// Returns an error when the payload exceeds the frame bound or the peer went away; the caller
/// treats either as a finite failure rather than retrying.
async fn write_frame<W: AsyncWriteExt + Unpin>(stream: &mut W, payload: &str) -> Result<(), ()> {
    let bytes = payload.as_bytes();
    let length: u32 = bytes.len().try_into().map_err(|_| ())?;
    if length > MAX_FRAME {
        return Err(());
    }
    stream.write_u32(length).await.map_err(|_| ())?;
    stream.write_all(bytes).await.map_err(|_| ())?;
    stream.flush().await.map_err(|_| ())
}

/// Reads one length-prefixed bounded frame, refusing an over-bound declared length before reading.
async fn read_frame<R: AsyncReadExt + Unpin>(stream: &mut R) -> Result<String, ()> {
    let length = stream.read_u32().await.map_err(|_| ())?;
    if length > MAX_FRAME {
        return Err(());
    }
    let mut bytes = vec![0; length as usize];
    stream.read_exact(&mut bytes).await.map_err(|_| ())?;
    String::from_utf8(bytes).map_err(|_| ())
}

/// Serves the private claim/finish endpoint until the returned task is dropped.
///
/// Binds `runtime_dir/claude-helper.sock` with owner-only permissions, removing a stale path from
/// a previous boot first. Each connection is handled independently and bounded by
/// [`SESSION_DEADLINE`]; a stuck or hostile peer can never block another helper or the daemon.
/// Returns the bound path, or `None` when the endpoint could not be created, which simply leaves
/// the Claude path unavailable.
pub fn serve(runtime_dir: &Path, ledger: Arc<Mutex<LaunchLedger>>) -> Option<HelperEndpoint> {
    let path = runtime_dir.join(HELPER_SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).ok()?;
    }
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let ledger = ledger.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(SESSION_DEADLINE, session(stream, ledger)).await;
            });
        }
    });
    Some(HelperEndpoint { path, task })
}

/// Owns the private endpoint path and its accept task for one daemon boot.
///
/// Dropping it aborts the accept loop and unlinks the socket, so no endpoint outlives the daemon
/// that created it. The daemon never treats a connected helper as a child it may reap or kill.
#[derive(Debug)]
pub struct HelperEndpoint {
    /// Bound endpoint path, unlinked on drop.
    path: PathBuf,
    /// Accept loop, aborted on drop.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HelperEndpoint {
    /// Aborts the accept loop and removes the endpoint path.
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.path);
    }
}

impl HelperEndpoint {
    /// Returns the bound private endpoint path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Handles one helper connection: exactly one claim, then at most one result frame.
///
/// A refused claim closes the connection without releasing a job, so a wrong actor, wrong channel,
/// replayed handle, stale generation or expired ticket produces no effect whatsoever.
async fn session(mut stream: tokio::net::UnixStream, ledger: Arc<Mutex<LaunchLedger>>) {
    let Ok(frame) = read_frame(&mut stream).await else {
        return;
    };
    let Ok(request) = serde_json::from_str::<ClaimRequest>(&frame) else {
        return;
    };
    let outcome = claim(&request, &ledger);
    let granted = matches!(outcome, ClaimReply::Granted(_));
    let Ok(encoded) = serde_json::to_string(&outcome) else {
        return;
    };
    if write_frame(&mut stream, &encoded).await.is_err() || !granted {
        return;
    }
    // A disconnect here leaves the ticket claimed and unsettled, which expiry turns into a
    // quarantined uncertain outcome rather than a silent success.
    let Ok(frame) = read_frame(&mut stream).await else {
        return;
    };
    let Ok(result) = HelperResult::decode(&frame) else {
        return;
    };
    if let Ok(mut ledger) = ledger.lock() {
        let _ = ledger.settle_frame(result);
    }
}

/// Applies one claim against the ledger under its lock, translating the outcome to a wire reply.
fn claim(request: &ClaimRequest, ledger: &Arc<Mutex<LaunchLedger>>) -> ClaimReply {
    use super::claude_worker::HELPER_PROTOCOL;
    let refuse = |code| ClaimReply::Refused { code };
    if request.protocol != HELPER_PROTOCOL {
        return refuse(FailureCode::ExecutionProfile);
    }
    let Ok(mut ledger) = ledger.lock() else {
        return refuse(FailureCode::Internal);
    };
    let Some(binding) = ledger.binding_of(&request.detail_ref) else {
        return refuse(FailureCode::InvalidDetail);
    };
    match ledger.claim(
        &request.detail_ref,
        binding,
        &request.attachment,
        super::assembly::monotonic_ms(),
    ) {
        ClaimOutcome::Granted(job) => ClaimReply::Granted(job),
        ClaimOutcome::Rejected(code) => refuse(code),
    }
}

/// Runs one foreground helper operation and exits; every failure path is finite and silent.
///
/// Called from the fixed `claude-worker` command the model was told to run. It connects to the
/// private endpoint, claims its operation once, performs the work under the sandbox inherited from
/// `Bash`, reports a bounded result with real child-settlement counts, and returns. It prints a
/// single short status line so the tool call has visible output, and never prints source,
/// diagnostics, configuration or host payloads.
pub async fn run(runtime_dir: &Path, attachment: Option<String>, detail_ref: Option<String>) {
    let outcome = execute(runtime_dir, attachment, detail_ref).await;
    println!("agent-ide claude-worker: {outcome}");
}

/// Performs the claim/work/report sequence, returning one fixed status word for the tool output.
async fn execute(
    runtime_dir: &Path,
    attachment: Option<String>,
    detail_ref: Option<String>,
) -> &'static str {
    use super::claude_worker::HELPER_PROTOCOL;
    let (Some(attachment), Some(detail_ref)) = (attachment, detail_ref) else {
        return "unavailable";
    };
    let Ok(mut stream) = tokio::net::UnixStream::connect(runtime_dir.join(HELPER_SOCKET)).await
    else {
        return "unavailable";
    };
    let request = ClaimRequest {
        protocol: HELPER_PROTOCOL,
        detail_ref: detail_ref.clone(),
        attachment,
    };
    let Ok(encoded) = serde_json::to_string(&request) else {
        return "unavailable";
    };
    if write_frame(&mut stream, &encoded).await.is_err() {
        return "unavailable";
    }
    let Ok(frame) = read_frame(&mut stream).await else {
        return "unavailable";
    };
    let job = match serde_json::from_str::<ClaimReply>(&frame) {
        Ok(ClaimReply::Granted(job)) => *job,
        _ => return "refused",
    };
    let (outcome, children) = perform(&job).await;
    let settled = children.settled();
    let result = HelperResult {
        protocol: HELPER_PROTOCOL,
        detail_ref,
        outcome,
        children,
    };
    let Ok(encoded) = result.encode() else {
        return "unavailable";
    };
    if write_frame(&mut stream, &encoded).await.is_err() {
        return "unavailable";
    }
    if settled { "complete" } else { "uncertain" }
}

/// Executes the claimed operation's own work under the inherited sandbox.
///
/// Returns the bounded outcome together with measured direct-child settlement. Counts are observed,
/// never assumed: a child that was started but could not be reaped is reported as unreaped so the
/// daemon can refuse to call the operation settled.
async fn perform(job: &HelperJob) -> (HelperOutcome, ChildSettlement) {
    use crate::execution::{GitDiscoveryQuery, inherited_git_arguments, run_inherited_child};
    let mut spawned = 0;
    let mut reaped = 0;
    let mut lines = Vec::new();
    let queries: &[GitDiscoveryQuery] = match job.operation {
        // Activation proves the candidate's real Git identity before any authority is minted.
        HelperOperation::Start => &[
            GitDiscoveryQuery::ShowTopLevel,
            GitDiscoveryQuery::GitCommonDir,
        ],
        // Context and diff still confirm the worktree root they are about to read within.
        HelperOperation::Context | HelperOperation::Diff => &[GitDiscoveryQuery::ShowTopLevel],
        // Stop performs no discovery; it exists only to settle and reap.
        HelperOperation::Stop => &[],
    };
    let deadline = Duration::from_millis(job.budgets.deadline_ms.min(60_000));
    for query in queries {
        spawned += 1;
        match run_inherited_child(
            &job.git,
            inherited_git_arguments(*query, &job.candidate),
            &job.candidate,
            job.budgets.output_bytes,
            deadline,
        )
        .await
        {
            Ok(child) => {
                reaped += 1;
                if !child.success {
                    return (
                        HelperOutcome::Failed {
                            code: FailureCode::UnsupportedGit,
                        },
                        ChildSettlement { spawned, reaped },
                    );
                }
                lines.push(String::from_utf8_lossy(&child.stdout).trim().to_owned());
            }
            Err(settled) => {
                // A child that never started was never a child; anything else stays unreaped.
                if settled {
                    spawned -= 1;
                }
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::SourceUnavailable,
                    },
                    ChildSettlement { spawned, reaped },
                );
            }
        }
    }
    let text = format!(
        "operation={:?}; git discovery observed {} of {} fixed queries; roots: {}",
        job.operation,
        reaped,
        queries.len(),
        lines.join(" | ")
    );
    (
        HelperOutcome::Complete { text },
        ChildSettlement { spawned, reaped },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistance::claude_worker::{
        Delivery, HELPER_PROTOCOL, HelperActor, HelperBudgets, HelperOperation, LaunchRecognition,
    };

    /// Creates one real temporary Git worktree using the installed Git binary.
    fn worktree() -> PathBuf {
        // Deliberately short and directly under /tmp: a Unix socket path is capped near 104
        // bytes on macOS, and the platform temp directory alone already consumes most of that.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let root = PathBuf::from(format!(
            "/tmp/aih-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temporary worktree is creatable");
        let status = std::process::Command::new("/usr/bin/git")
            .args(["-C", root.to_str().unwrap(), "init", "-q"])
            .status()
            .expect("git init runs");
        assert!(status.success());
        std::fs::canonicalize(&root).expect("worktree canonicalizes")
    }

    /// Builds one job describing Git-only discovery over a real worktree.
    fn job(candidate: &Path) -> HelperJob {
        HelperJob {
            protocol: HELPER_PROTOCOL,
            operation: HelperOperation::Start,
            candidate: candidate.to_path_buf(),
            git: PathBuf::from("/usr/bin/git"),
            canonical_root: None,
            provider: None,
            parameters: serde_json::json!({"activation_id": "activate"}),
            budgets: HelperBudgets {
                output_bytes: 4096,
                processes: 4,
                deadline_ms: 30_000,
            },
        }
    }

    /// Mints and arms one ticket exactly as the daemon ingress and native pre-hook would.
    fn armed(candidate: &Path, runtime: &Path) -> (Arc<Mutex<LaunchLedger>>, String) {
        let ledger = Arc::new(Mutex::new(LaunchLedger::default()));
        let actor = HelperActor::new("agent", Some("session")).unwrap();
        let command = LaunchLedger::helper_command(
            Path::new("/usr/local/bin/agent-ide"),
            runtime,
            "attach",
            "detail-1",
        );
        let mut guard = ledger.lock().unwrap();
        guard
            .mint(
                "detail-1",
                [3; 32],
                actor.clone(),
                "attach",
                command.clone(),
                job(candidate),
                u64::MAX,
            )
            .expect("ticket mints");
        assert_eq!(
            guard.recognize(&command, false, "call", &actor, 0),
            LaunchRecognition::Recognized
        );
        drop(guard);
        (ledger, "detail-1".to_owned())
    }

    /// A launched helper claims once, runs real Git children, reaps them, and settles its frame.
    #[tokio::test]
    async fn launched_helper_runs_real_git_discovery_and_settles_its_children() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let (ledger, reference) = armed(&candidate, &runtime);
        let endpoint = serve(&runtime, ledger.clone()).expect("endpoint binds");
        assert!(endpoint.path().exists());

        let status = execute(&runtime, Some("attach".to_owned()), Some(reference.clone())).await;
        assert_eq!(status, "complete");

        // The exact successful post is the second required half. The helper's final frame is
        // recorded by the endpoint task, so wait for it instead of assuming a completion order.
        ledger
            .lock()
            .unwrap()
            .settle_post("call", true)
            .expect("helper post settles");
        let mut delivery = Delivery::Waiting;
        for _ in 0..200 {
            delivery = ledger.lock().unwrap().delivery(&reference);
            if matches!(delivery, Delivery::Ready(_)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let Delivery::Ready(result) = delivery else {
            panic!("both settlement halves arrived");
        };
        assert!(result.children.settled() && result.children.spawned == 2);
        let HelperOutcome::Complete { text } = &result.outcome else {
            panic!("real git discovery completed");
        };
        // Proves the child actually ran in the worktree rather than returning a fabricated root.
        assert!(text.contains(candidate.to_str().unwrap()));
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }

    /// A handle with no recognized native launch is refused without any Git or source effect.
    #[tokio::test]
    async fn unlaunched_handle_is_refused_over_the_socket() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let ledger = Arc::new(Mutex::new(LaunchLedger::default()));
        ledger
            .lock()
            .unwrap()
            .mint(
                "detail-1",
                [3; 32],
                HelperActor::new("agent", None).unwrap(),
                "attach",
                "unused-command".to_owned(),
                job(&candidate),
                u64::MAX,
            )
            .unwrap();
        let endpoint = serve(&runtime, ledger.clone()).expect("endpoint binds");

        assert_eq!(
            execute(&runtime, Some("attach".into()), Some("detail-1".into())).await,
            "refused"
        );
        // A copied handle from another channel is refused for the same reason.
        assert_eq!(
            execute(&runtime, Some("other".into()), Some("detail-1".into())).await,
            "refused"
        );
        assert_eq!(
            ledger.lock().unwrap().delivery("detail-1"),
            Delivery::Waiting
        );
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }
}
