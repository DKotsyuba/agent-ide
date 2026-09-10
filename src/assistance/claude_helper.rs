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
    ChildSettlement, ClaimOutcome, DiscoveryFrame, HelperJob, HelperOperation, HelperOutcome,
    HelperQuery, HelperResult, LaunchLedger, MAX_HELPER_FRAME_BYTES,
};
use super::host_binding::BindingRef;
use super::reply::FailureCode;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Reports whether one binding generation is live *right now*.
///
/// Supplied by the daemon, which closes over its own [`HostBindingGuard`](super::host_binding::HostBindingGuard).
/// The endpoint deliberately holds only this narrow question rather than the guard itself: the
/// claim path needs current liveness of the ticket's own generation and nothing else, and cannot
/// be allowed to establish, roll back or stop a binding.
pub type BindingLiveness = Arc<dyn Fn(&BindingRef) -> bool + Send + Sync>;

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
pub fn serve(
    runtime_dir: &Path,
    ledger: Arc<Mutex<LaunchLedger>>,
    live: BindingLiveness,
) -> Option<HelperEndpoint> {
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
            let live = live.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(SESSION_DEADLINE, session(stream, ledger, live)).await;
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
async fn session(
    mut stream: tokio::net::UnixStream,
    ledger: Arc<Mutex<LaunchLedger>>,
    live: BindingLiveness,
) {
    let Ok(frame) = read_frame(&mut stream).await else {
        return;
    };
    let Ok(request) = serde_json::from_str::<ClaimRequest>(&frame) else {
        return;
    };
    let outcome = claim(&request, &ledger, live.as_ref());
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
    // Native and Git-administrative validation of these bytes belongs to the helper, which ran it
    // before reporting `Complete`; the daemon deliberately does not repeat it here, because doing
    // so would read Git administrative files daemon-side on the Claude route. What the daemon does
    // with the bytes later is pure parsing plus its own durable directory identity checks.
    if let Ok(mut ledger) = ledger.lock() {
        let _ = ledger.settle_frame(result);
    }
}

/// Applies one claim against the ledger under its lock, translating the outcome to a wire reply.
///
/// `bindings` is the daemon's live binding guard. It is consulted about the ticket's *own* retained
/// generation, which is the only independent generation check available here: a fingerprint read
/// out of the same ticket and compared back against it proves nothing. Ownership of the launch is
/// established separately, by the exact native pre-hook recognition the ledger already performed.
fn claim(
    request: &ClaimRequest,
    ledger: &Arc<Mutex<LaunchLedger>>,
    live: &(dyn Fn(&BindingRef) -> bool + Send + Sync),
) -> ClaimReply {
    use super::claude_worker::HELPER_PROTOCOL;
    let refuse = |code| ClaimReply::Refused { code };
    if request.protocol != HELPER_PROTOCOL {
        return refuse(FailureCode::ExecutionProfile);
    }
    let Ok(mut ledger) = ledger.lock() else {
        return refuse(FailureCode::Internal);
    };
    match ledger.claim(
        &request.detail_ref,
        &request.attachment,
        super::assembly::monotonic_ms(),
        live,
    ) {
        ClaimOutcome::Granted(job) => ClaimReply::Granted(job),
        ClaimOutcome::Rejected(code) => refuse(code),
    }
}

/// Reconstructs canonical Git discovery from a helper's raw bytes and validates the worktree.
///
/// This is where a helper's report stops being a claim and becomes evidence. The helper supplies
/// only raw per-query bytes; the exact same closed validator the managed path uses derives the
/// worktree, repository root and common directory from them, so a helper cannot assert an identity
/// it did not actually observe. Returns the validated worktree, or a closed failure when the
/// triple is incomplete, the bytes are unusable, or the configured Git lacks required discovery
/// support.
///
/// Performs no process, source or provider work: it only interprets bytes already captured.
pub fn validated_worktree(
    candidate: &Path,
    detail_ref: &str,
    frames: &[DiscoveryFrame],
) -> Result<crate::workspace::git::discovery::DiscoveredWorktree, FailureCode> {
    let (operation, evidence) = discovery_evidence(detail_ref, frames)?;
    crate::workspace::git::discovery::validate_discovery(candidate, &operation, &evidence).map_err(
        |error| match error {
            crate::workspace::git::GitError::UnsupportedDiscoveryGit => FailureCode::UnsupportedGit,
            _ => FailureCode::WorkspaceActivation,
        },
    )
}

/// Rebuilds the closed Execution discovery evidence triple from a helper's raw reported bytes.
///
/// Every field the daemon did not observe itself is reconstructed conservatively: descendant
/// evidence is [`DescendantEvidence::Unverified`], no duration is asserted, and the operation
/// identity is derived from the daemon's own handle rather than from anything the helper sent. The
/// evidence is built through [`GitDiscoveryEvidence::new`], its own validating constructor, so a
/// malformed frame is refused before it can be interpreted.
///
/// Refuses a triple that is not exactly three frames, an over-bound frame, and any truncated
/// stream: truncated discovery output cannot be completed later, so a prefix is never parsed.
/// Performs no process, source or provider work.
pub fn discovery_evidence(
    detail_ref: &str,
    frames: &[DiscoveryFrame],
) -> Result<
    (
        crate::execution::DiscoveryOperationRef,
        Vec<crate::execution::GitDiscoveryEvidence>,
    ),
    FailureCode,
> {
    use crate::execution::{
        CapturedOutput, DescendantEvidence, DiscoveryOperationRef, GitDiscoveryEvidence,
        GitDiscoveryQuery,
    };
    use std::os::unix::process::ExitStatusExt;
    if frames.len() != 3 {
        return Err(FailureCode::WorkspaceActivation);
    }
    let operation = DiscoveryOperationRef::new(format!("claude-discover-{detail_ref}"))
        .map_err(|_| FailureCode::Internal)?;
    let mut evidence = Vec::with_capacity(3);
    for frame in frames {
        frame.validate()?;
        let captured = |bytes: &[u8]| CapturedOutput {
            bytes: bytes.to_vec(),
            truncated: frame.truncated,
            drained_bytes: bytes.len() as u64,
            complete: !frame.truncated,
        };
        // Truncated discovery output cannot be completed later; refuse rather than parse a prefix.
        if frame.truncated {
            return Err(FailureCode::WorkspaceActivation);
        }
        evidence.push(
            GitDiscoveryEvidence::new(
                operation.clone(),
                match frame.query {
                    HelperQuery::ShowTopLevel => GitDiscoveryQuery::ShowTopLevel,
                    HelperQuery::GitCommonDir => GitDiscoveryQuery::GitCommonDir,
                    HelperQuery::WorktreeListPorcelainZ => {
                        GitDiscoveryQuery::WorktreeListPorcelainZ
                    }
                },
                captured(&frame.stdout),
                captured(&frame.stderr),
                std::process::ExitStatus::from_raw(frame.exit_code.unwrap_or(1) << 8),
                Duration::ZERO,
                None,
                DescendantEvidence::Unverified,
            )
            .map_err(|_| FailureCode::WorkspaceActivation)?,
        );
    }
    Ok((operation, evidence))
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
    let (outcome, children, discovery) = perform(&job).await;
    let settled = children.settled();
    let result = HelperResult {
        protocol: HELPER_PROTOCOL,
        detail_ref,
        outcome,
        children,
        discovery,
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
/// Returns the bounded outcome, measured direct-child settlement, and the raw bytes of every fixed
/// discovery query it ran. Counts are observed, never assumed: a child that was started but could
/// not be reaped is reported as unreaped so the daemon can refuse to call the operation settled.
/// This function interprets no discovery output; canonical interpretation stays with the daemon.
async fn perform(job: &HelperJob) -> (HelperOutcome, ChildSettlement, Vec<DiscoveryFrame>) {
    use crate::execution::{
        GitDiscoveryQuery, InheritedChildFailure, inherited_git_arguments, run_inherited_child,
    };
    let mut spawned = 0;
    let mut reaped = 0;
    let mut discovery = Vec::new();
    // Activation reports the complete fixed triple the daemon's discovery validator requires.
    // Context and diff still confirm the worktree root they are about to read within. Stop
    // performs no discovery; it exists only to settle and reap.
    let queries: &[(GitDiscoveryQuery, HelperQuery)] = match job.operation {
        HelperOperation::Start => &[
            (GitDiscoveryQuery::ShowTopLevel, HelperQuery::ShowTopLevel),
            (GitDiscoveryQuery::GitCommonDir, HelperQuery::GitCommonDir),
            (
                GitDiscoveryQuery::WorktreeListPorcelainZ,
                HelperQuery::WorktreeListPorcelainZ,
            ),
        ],
        HelperOperation::Context | HelperOperation::Diff => {
            &[(GitDiscoveryQuery::ShowTopLevel, HelperQuery::ShowTopLevel)]
        }
        HelperOperation::Stop => &[],
    };
    let deadline = Duration::from_millis(job.budgets.deadline_ms.min(60_000));
    for (query, reported) in queries {
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
                discovery.push(DiscoveryFrame {
                    query: *reported,
                    stdout: child.stdout,
                    stderr: Vec::new(),
                    exit_code: Some(if child.success { 0 } else { 1 }),
                    truncated: child.truncated,
                });
            }
            Err(failure) => {
                // Each case charges child accounting differently: a child that never started was
                // never a child, a positively killed-and-reaped child is settled but failed, and an
                // unprovable cleanup stays unreaped so the daemon refuses to call this complete.
                let code = match failure {
                    InheritedChildFailure::NeverStarted => {
                        spawned -= 1;
                        FailureCode::SourceUnavailable
                    }
                    InheritedChildFailure::Reaped => {
                        reaped += 1;
                        FailureCode::Deadline
                    }
                    InheritedChildFailure::Unsettled => FailureCode::Deadline,
                };
                return (
                    HelperOutcome::Failed { code },
                    ChildSettlement { spawned, reaped },
                    discovery,
                );
            }
        }
    }
    if discovery.iter().any(|frame| frame.exit_code != Some(0)) {
        return (
            HelperOutcome::Failed {
                code: FailureCode::UnsupportedGit,
            },
            ChildSettlement { spawned, reaped },
            discovery,
        );
    }
    // The helper owns native and Git-administrative validation: it runs inside the host sandbox
    // where reading those files is permitted, and it must not report `Complete` for evidence the
    // canonical validator would reject. Discovery that cannot be validated is a closed failure,
    // never a partial success.
    let mut text = format!(
        "operation={:?}; {} of {} fixed Git queries observed under the inherited host sandbox",
        job.operation,
        reaped,
        queries.len()
    );
    if job.operation == HelperOperation::Start {
        match validated_worktree(&job.candidate, "helper", &discovery) {
            Ok(worktree) => {
                text = format!(
                    "{text}; canonical worktree {} (common dir {}). \
                     Workspace authority is not implied by discovery alone.",
                    worktree.root().display(),
                    worktree.common_dir().display(),
                );
            }
            Err(code) => {
                return (
                    HelperOutcome::Failed { code },
                    ChildSettlement { spawned, reaped },
                    discovery,
                );
            }
        }
    }
    (
        HelperOutcome::Complete { text },
        ChildSettlement { spawned, reaped },
        discovery,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistance::claude_worker::{
        AcceptedIdentity, Delivery, HELPER_PROTOCOL, HelperActor, HelperBudgets, HelperOperation,
        LaunchRecognition,
    };
    use crate::assistance::host_binding::BindingRef;
    use crate::assistance::reply::FailureCode;

    /// Returns the fixed binding generation every helper fixture in this module mints under.
    fn binding_fixture() -> BindingRef {
        BindingRef::fixture("agent", "attach", 1)
    }

    /// Returns a liveness probe that reports the fixture generation as currently live.
    fn live() -> BindingLiveness {
        Arc::new(|binding: &BindingRef| binding == &binding_fixture())
    }

    /// Returns an accepted identity fixture with a well-formed digest.
    fn identity(path: &str) -> AcceptedIdentity {
        AcceptedIdentity::new(PathBuf::from(path), "fixture", &"ab".repeat(32))
            .expect("fixture identity is well formed")
    }

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
        let ledger = Arc::new(Mutex::new(LaunchLedger::new(Arc::new(Mutex::new(
            crate::assistance::worker::admission_controller(),
        )))));
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
                binding_fixture(),
                actor.clone(),
                "attach",
                command.clone(),
                job(candidate),
                u64::MAX,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
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
        let endpoint = serve(&runtime, ledger.clone(), live()).expect("endpoint binds");
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
        assert!(result.children.settled() && result.children.spawned == 3);
        let HelperOutcome::Complete { text } = &result.outcome else {
            panic!("real git discovery completed");
        };
        // Proves the child actually ran in the worktree rather than returning a fabricated root.
        assert!(text.contains(candidate.to_str().unwrap()));
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }

    /// The real helper process is the only thing that can mint a settled token for a real worktree.
    ///
    /// Integrated rather than a private ledger exercise: a real `Bash`-shaped foreground helper
    /// claims over the real private socket, runs three real Git children against a real worktree,
    /// reaps them and reports its own frame. Only after that, and after the matching successful
    /// post, does a token exist. A copied frame, a replayed frame, a frame from a generation that
    /// is not live, and revoked work all mint nothing.
    #[tokio::test]
    async fn only_a_real_settled_helper_process_mints_a_settled_token() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let (ledger, reference) = armed(&candidate, &runtime);
        let endpoint = serve(&runtime, ledger.clone(), live()).expect("endpoint binds");
        let owner = binding_fixture().fingerprint();

        // A raw frame for work no helper ever claimed changes nothing and mints nothing.
        let raw = HelperResult {
            protocol: HELPER_PROTOCOL,
            detail_ref: reference.clone(),
            outcome: HelperOutcome::Complete {
                text: "forged".into(),
            },
            children: ChildSettlement {
                spawned: 0,
                reaped: 0,
            },
            discovery: Vec::new(),
        };
        assert_eq!(
            ledger.lock().unwrap().settle_frame(raw.clone()),
            Err(FailureCode::InvalidDetail),
            "an unclaimed handle accepts no frame"
        );
        assert!(ledger.lock().unwrap().settled(&reference, owner).is_none());

        assert_eq!(
            execute(&runtime, Some("attach".to_owned()), Some(reference.clone())).await,
            "complete"
        );
        // The frame arrives on the endpoint task; the post is the second required half.
        ledger
            .lock()
            .unwrap()
            .settle_post("call", true)
            .expect("helper post settles");
        let mut settled = None;
        for _ in 0..200 {
            settled = ledger.lock().unwrap().settled(&reference, owner);
            if settled.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let token = settled.expect("a positively settled real helper operation mints its token");
        assert_eq!(token.operation(), HelperOperation::Start);
        assert_eq!(token.discovery().len(), 3);
        assert!(token.result().children.settled());

        // A wrong generation owns nothing, and the frame cannot be replayed onto the same handle.
        assert!(
            ledger
                .lock()
                .unwrap()
                .settled(&reference, [9; 32])
                .is_none()
        );
        assert_eq!(
            ledger.lock().unwrap().settle_frame(raw),
            Err(FailureCode::Conflict),
            "a replayed frame never overwrites settled evidence"
        );

        // Revocation permanently suppresses the token while retaining cleanup correlation, and the
        // lease taken at claim is released only once cleanup settlement is positively proven.
        {
            let mut guard = ledger.lock().unwrap();
            assert_eq!(guard.active_claims(), 1);
            guard.revoke(owner);
            assert!(
                guard.settled(&reference, owner).is_none(),
                "revoked work can never mint a settled token"
            );
            assert_eq!(guard.active_claims(), 0);
            assert_eq!(
                guard.delivery(&reference),
                Delivery::Failed(FailureCode::Deadline)
            );
        }

        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }

    /// A handle with no recognized native launch is refused without any Git or source effect.
    #[tokio::test]
    async fn unlaunched_handle_is_refused_over_the_socket() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let ledger = Arc::new(Mutex::new(LaunchLedger::new(Arc::new(Mutex::new(
            crate::assistance::worker::admission_controller(),
        )))));
        ledger
            .lock()
            .unwrap()
            .mint(
                "detail-1",
                binding_fixture(),
                HelperActor::new("agent", None).unwrap(),
                "attach",
                "unused-command".to_owned(),
                job(&candidate),
                u64::MAX,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            )
            .unwrap();
        let endpoint = serve(&runtime, ledger.clone(), live()).expect("endpoint binds");

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
    /// A claimed helper that disconnects without a frame leaves quarantined uncertainty.
    #[tokio::test]
    async fn claimed_helper_that_disconnects_is_quarantined_rather_than_completed() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let (ledger, reference) = armed(&candidate, &runtime);
        let endpoint = serve(&runtime, ledger.clone(), live()).expect("endpoint binds");

        // Claim exactly as a helper would, then drop the connection before reporting anything.
        {
            let mut stream = tokio::net::UnixStream::connect(runtime.join(HELPER_SOCKET))
                .await
                .expect("helper connects");
            let request = ClaimRequest {
                protocol: crate::assistance::claude_worker::HELPER_PROTOCOL,
                detail_ref: reference.clone(),
                attachment: "attach".to_owned(),
            };
            write_frame(&mut stream, &serde_json::to_string(&request).unwrap())
                .await
                .expect("claim is written");
            let reply = read_frame(&mut stream).await.expect("claim is answered");
            assert!(reply.contains("granted"));
        }

        // The handle is now consumed: a second helper cannot claim it.
        assert_eq!(
            execute(&runtime, Some("attach".into()), Some(reference.clone())).await,
            "refused"
        );
        let mut guard = ledger.lock().unwrap();
        assert_eq!(guard.delivery(&reference), Delivery::Waiting);
        // Expiry converts unsettled claimed work into a retained uncertain outcome.
        guard.expire(u64::MAX);
        assert_eq!(guard.len(), 1);
        assert_eq!(
            guard.delivery(&reference),
            Delivery::Failed(crate::assistance::reply::FailureCode::Deadline)
        );
        drop(guard);
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }

    /// A launch that the host never ran expires with no effect and no retained work.
    #[tokio::test]
    async fn denied_launch_expires_without_any_effect() {
        let candidate = worktree();
        let runtime = candidate.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let ledger = Arc::new(Mutex::new(LaunchLedger::new(Arc::new(Mutex::new(
            crate::assistance::worker::admission_controller(),
        )))));
        ledger
            .lock()
            .unwrap()
            .mint(
                "detail-1",
                binding_fixture(),
                HelperActor::new("agent", None).unwrap(),
                "attach",
                "never-run".to_owned(),
                job(&candidate),
                10,
                identity("/usr/local/bin/agent-ide"),
                vec![identity("/usr/bin/git")],
            )
            .unwrap();
        let endpoint = serve(&runtime, ledger.clone(), live()).expect("endpoint binds");
        ledger.lock().unwrap().expire(11);
        assert!(ledger.lock().unwrap().is_empty());
        assert_eq!(
            execute(&runtime, Some("attach".into()), Some("detail-1".into())).await,
            "refused"
        );
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }
}
