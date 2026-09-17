//! Private helper endpoint: the daemon's claim/finish socket and the foreground helper that runs.
//!
//! The daemon side (`serve`) owns a private Unix listener inside the runtime directory. It
//! releases one closed `HelperJob` per successful atomic claim and records the helper's final
//! frame. It performs no Git, source or provider work of its own for a Claude operation.
//!
//! The helper side (`run`) is a short foreground process the model launches through its ordinary
//! `Bash` tool, so every child it starts inherits the host's own real sandbox. It claims its
//! operation once, performs fixed discovery/baseline/snapshot work and an optional one-shot
//! provider session, reports bounded evidence with real child-settlement counts, and exits. It
//! never re-enters the managed Codex execution path or presents a synthetic sandbox observation.

use super::claude_worker::{
    ChildSettlement, ClaimOutcome, DiscoveryFrame, HelperBaselineFrame, HelperBaselineQuery,
    HelperJob, HelperLanguage, HelperOperation, HelperOutcome, HelperPayload, HelperQuery,
    HelperResult, HelperSource, LaunchLedger, MAX_HELPER_FRAME_BYTES, MAX_RESULT_TEXT_BYTES,
};
use super::host_binding::BindingRef;
use super::reply::{EditDiagnostics, FailureCode, PeerReply};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
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
/// Leaves room to reap diagnostics and report a known edit result before its ticket expires.
const EDIT_SETTLEMENT_RESERVE: Duration = Duration::from_secs(2);

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
/// `SESSION_DEADLINE`; a stuck or hostile peer can never block another helper or the daemon.
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
/// evidence is `DescendantEvidence::Unverified`, no duration is asserted, and the operation
/// identity is derived from the daemon's own handle rather than from anything the helper sent. The
/// evidence is built through `GitDiscoveryEvidence::new`, its own validating constructor, so a
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
    let (outcome, children, discovery, payload) = perform(&job, &detail_ref).await;
    let settled = children.settled();
    let result = HelperResult {
        protocol: HELPER_PROTOCOL,
        detail_ref,
        outcome,
        children,
        discovery,
        payload,
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
async fn perform_discovery(
    job: &HelperJob,
    deadline: tokio::time::Instant,
) -> (HelperOutcome, ChildSettlement, Vec<DiscoveryFrame>) {
    use crate::execution::{
        GitDiscoveryQuery, InheritedChildFailure, inherited_git_arguments, run_inherited_child,
    };
    let mut spawned = 0;
    let mut reaped = 0;
    let mut discovery = Vec::new();
    // Activation reports the complete fixed triple the daemon's discovery validator requires.
    // Context and diff still confirm the worktree root they are about to read within. Stop is
    // daemon-owned and never constructs a helper job.
    let queries: &[(GitDiscoveryQuery, HelperQuery)] = match job.operation {
        HelperOperation::Start => &[
            (GitDiscoveryQuery::ShowTopLevel, HelperQuery::ShowTopLevel),
            (GitDiscoveryQuery::GitCommonDir, HelperQuery::GitCommonDir),
            (
                GitDiscoveryQuery::WorktreeListPorcelainZ,
                HelperQuery::WorktreeListPorcelainZ,
            ),
        ],
        HelperOperation::Context | HelperOperation::Diff | HelperOperation::Edit => {
            &[(GitDiscoveryQuery::ShowTopLevel, HelperQuery::ShowTopLevel)]
        }
    };
    for (query, reported) in queries {
        spawned += 1;
        match run_inherited_child(
            &job.git,
            inherited_git_arguments(*query, &job.candidate),
            &job.candidate,
            job.budgets.output_bytes,
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(60)),
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

/// Executes operation-specific work after discovery using the claimed detail reference for fitting.
///
/// `detail_ref` is the exact ticket handle carried by the enclosing helper result. It is used only
/// to measure the final Diff envelope; it never grants authority or becomes a continuation.
async fn perform(
    job: &HelperJob,
    detail_ref: &str,
) -> (
    HelperOutcome,
    ChildSettlement,
    Vec<DiscoveryFrame>,
    Option<HelperPayload>,
) {
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(job.budgets.deadline_ms.min(300_000));
    let (discovery_outcome, mut children, discovery) = perform_discovery(job, deadline).await;
    if let HelperOutcome::Failed { code } = discovery_outcome {
        if job.operation == HelperOperation::Edit && children.settled() {
            let outcome = match code {
                FailureCode::Deadline => crate::changes::edit::EditOutcome::DeadlineNoEffect,
                FailureCode::Cancelled => crate::changes::edit::EditOutcome::CancelledNoEffect,
                _ => crate::changes::edit::EditOutcome::UnavailableBeforeDispatch,
            };
            return (
                HelperOutcome::Complete {
                    text: format!("edit outcome: {}", outcome.as_str()),
                },
                children,
                discovery,
                Some(HelperPayload::Edit {
                    outcome,
                    source: None,
                    diagnostics: EditDiagnostics::Unknown {},
                }),
            );
        }
        return failed(code, children.spawned, children.reaped, discovery);
    }
    if job.operation == HelperOperation::Start {
        let baseline = match collect_baseline(
            job,
            deadline,
            &mut children.spawned,
            &mut children.reaped,
        )
        .await
        {
            Ok(baseline) => baseline,
            Err(code) => return failed(code, children.spawned, children.reaped, discovery),
        };
        return (
            HelperOutcome::Complete {
                text: "fixed discovery and activation baseline evidence settled; Workspace authority is not implied by helper evidence alone".into(),
            },
            children,
            discovery,
            Some(HelperPayload::Start { baseline }),
        );
    }
    let Some(scope) = job.scope.as_ref() else {
        return failed(
            FailureCode::WorkspaceAuthority,
            children.spawned,
            children.reaped,
            discovery,
        );
    };
    if discovery
        .first()
        .and_then(|frame| crate::workspace::git::parse_terminal_path(&frame.stdout).ok())
        .as_deref()
        != Some(scope.root.as_path())
    {
        return failed(
            FailureCode::WorkspaceAuthority,
            children.spawned,
            children.reaped,
            discovery,
        );
    }
    let worktree = match crate::workspace::authority::WorktreeRef::from_inherited_scope(
        scope.worktree_id.clone(),
        scope.incarnation,
        scope.root.clone(),
        scope.repository_root.clone(),
        scope.git_common_dir.clone(),
        scope.native_root_identity,
    ) {
        Ok(worktree) => worktree,
        Err(_) => {
            return failed(
                FailureCode::WorkspaceAuthority,
                children.spawned,
                children.reaped,
                discovery,
            );
        }
    };
    let (outcome, payload) = match job.operation {
        HelperOperation::Context => {
            context(
                job,
                deadline,
                worktree,
                &mut children.spawned,
                &mut children.reaped,
            )
            .await
        }
        HelperOperation::Diff => {
            diff(
                job,
                detail_ref,
                deadline,
                worktree,
                &mut children.spawned,
                &mut children.reaped,
            )
            .await
        }
        HelperOperation::Edit => {
            edit(
                job,
                deadline,
                worktree,
                &mut children.spawned,
                &mut children.reaped,
            )
            .await
        }
        HelperOperation::Start => (
            HelperOutcome::Failed {
                code: FailureCode::Internal,
            },
            None,
        ),
    };
    (outcome, children, discovery, payload)
}

/// Applies one claimed full-content edit and refreshes configured diagnostics after known success.
///
/// The helper uses only daemon-selected inherited scope and expected source facts. Deadline is
/// checked immediately before the descriptor operation. Provider refresh is best-effort after an
/// exact Workspace post-read, contributes only matching diagnostic evidence to the helper frame,
/// and cannot change a known filesystem result.
async fn edit(
    job: &HelperJob,
    deadline: tokio::time::Instant,
    worktree: crate::workspace::authority::WorktreeRef,
    spawned: &mut u32,
    reaped: &mut u32,
) -> (HelperOutcome, Option<HelperPayload>) {
    use crate::workspace::observation::{
        ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceObservation,
        SourceRevision,
    };
    let Some(scope) = job.scope.as_ref() else {
        return failed_edit(crate::changes::edit::EditOutcome::UnavailableBeforeDispatch);
    };
    let Some(expected) = job.edit_source.as_ref() else {
        return failed_edit(crate::changes::edit::EditOutcome::UnavailableBeforeDispatch);
    };
    let Ok(request) =
        serde_json::from_value::<crate::changes::edit::EditRequest>(job.parameters.clone())
    else {
        return failed_edit(crate::changes::edit::EditOutcome::UnavailableBeforeDispatch);
    };
    if request.validate().is_err() || request.path != expected.path {
        return failed_edit(crate::changes::edit::EditOutcome::UnavailableBeforeDispatch);
    }
    if tokio::time::Instant::now() >= deadline {
        return failed_edit(crate::changes::edit::EditOutcome::DeadlineNoEffect);
    }
    let expected_bytes = match (expected.present, expected.digest) {
        (true, Some(digest)) => match SourceBytes::from_reported(digest, expected.length) {
            Ok(bytes) => Some(bytes),
            Err(_) => return failed_edit(crate::changes::edit::EditOutcome::UnsafeTarget),
        },
        (false, None) if expected.length == 0 => None,
        _ => return failed_edit(crate::changes::edit::EditOutcome::UnsafeTarget),
    };
    let workspace = crate::workspace::edit::replace_inherited_if_current(
        &worktree,
        scope.authority_epoch,
        &request.operation_id,
        std::path::Path::new(&request.path),
        expected_bytes,
        request.content.as_bytes(),
        || tokio::time::Instant::now() < deadline,
    );
    let (outcome, post) = match workspace {
        crate::workspace::edit::EditOutcome::Created(read) => {
            (crate::changes::edit::EditOutcome::Created, Some(read))
        }
        crate::workspace::edit::EditOutcome::Replaced(read) => {
            (crate::changes::edit::EditOutcome::Replaced, Some(read))
        }
        crate::workspace::edit::EditOutcome::Unchanged(read) => {
            (crate::changes::edit::EditOutcome::Unchanged, Some(read))
        }
        crate::workspace::edit::EditOutcome::StaleSource => {
            (crate::changes::edit::EditOutcome::StaleSource, None)
        }
        crate::workspace::edit::EditOutcome::UnsafeTarget => {
            (crate::changes::edit::EditOutcome::UnsafeTarget, None)
        }
        crate::workspace::edit::EditOutcome::CancelledNoEffect => {
            (crate::changes::edit::EditOutcome::DeadlineNoEffect, None)
        }
        crate::workspace::edit::EditOutcome::DeadlineNoEffect => {
            (crate::changes::edit::EditOutcome::DeadlineNoEffect, None)
        }
        crate::workspace::edit::EditOutcome::CapacityNoEffect => {
            (crate::changes::edit::EditOutcome::CapacityNoEffect, None)
        }
        crate::workspace::edit::EditOutcome::OutcomeUnknown { .. } => {
            (crate::changes::edit::EditOutcome::OutcomeUnknown, None)
        }
    };
    let mut diagnostics = EditDiagnostics::Unknown {};
    if let Some(read) = &post {
        let observation = SourceObservation::new(
            worktree,
            scope.authority_epoch,
            expected.sequence.saturating_add(1),
            ObservationRef::new("claude-helper-edit").expect("fixed observation reference"),
            read.path().to_path_buf(),
            Some(read.bytes().clone()),
            SourceRevision::new(blake3::hash(read.contents()).to_hex().to_string())
                .expect("digest revision is bounded"),
            SourceCoverage::Complete,
            ObservedState::Present,
        );
        if let (Ok(observation), Some(diagnostic_deadline)) =
            (observation, edit_diagnostic_deadline(deadline))
            && let Ok(Some((context, Some(snapshot)))) = provider_context(
                job,
                diagnostic_deadline,
                &observation,
                read.contents(),
                crate::intelligence::context::ContextQuery::File,
                spawned,
                reaped,
            )
            .await
        {
            diagnostics = EditDiagnostics::from_snapshot(&context, &snapshot);
        }
    }
    let source = post.map(|read| HelperSource {
        path: request.path,
        present: true,
        digest: Some(*read.bytes().digest()),
        length: read.bytes().length(),
    });
    (
        HelperOutcome::Complete {
            text: format!("edit outcome: {}", outcome.as_str()),
        },
        Some(HelperPayload::Edit {
            outcome,
            source,
            diagnostics,
        }),
    )
}

/// Returns the latest deadline a best-effort edit diagnostic refresh may consume.
///
/// `None` leaves the completed Workspace effect intact and publishes `unknown` diagnostics so
/// the helper can settle its receipt and ticket before their shared operation deadline.
fn edit_diagnostic_deadline(deadline: tokio::time::Instant) -> Option<tokio::time::Instant> {
    let diagnostic_deadline = deadline.checked_sub(EDIT_SETTLEMENT_RESERVE)?;
    (diagnostic_deadline > tokio::time::Instant::now()).then_some(diagnostic_deadline)
}

/// Builds a settled edit payload for a known pre-effect or unavailable helper outcome.
fn failed_edit(
    outcome: crate::changes::edit::EditOutcome,
) -> (HelperOutcome, Option<HelperPayload>) {
    (
        HelperOutcome::Complete {
            text: format!("edit outcome: {}", outcome.as_str()),
        },
        Some(HelperPayload::Edit {
            outcome,
            source: None,
            diagnostics: EditDiagnostics::Unknown {},
        }),
    )
}

/// Returns one closed failure while preserving actual child settlement counts and discovery bytes.
fn failed(
    code: FailureCode,
    spawned: u32,
    reaped: u32,
    discovery: Vec<DiscoveryFrame>,
) -> (
    HelperOutcome,
    ChildSettlement,
    Vec<DiscoveryFrame>,
    Option<HelperPayload>,
) {
    (
        HelperOutcome::Failed { code },
        ChildSettlement { spawned, reaped },
        discovery,
        None,
    )
}

/// Updates child counts from one inherited-child failure and returns its closed product code.
fn account_failure(
    failure: crate::execution::InheritedChildFailure,
    spawned: &mut u32,
    reaped: &mut u32,
) -> FailureCode {
    match failure {
        crate::execution::InheritedChildFailure::NeverStarted => {
            *spawned = spawned.saturating_sub(1);
            FailureCode::SourceUnavailable
        }
        crate::execution::InheritedChildFailure::Reaped => {
            *reaped = reaped.saturating_add(1);
            FailureCode::Deadline
        }
        crate::execution::InheritedChildFailure::Unsettled => FailureCode::Deadline,
    }
}

/// Runs the three Workspace-owned baseline commands and returns only bounded raw evidence.
async fn collect_baseline(
    job: &HelperJob,
    deadline: tokio::time::Instant,
    spawned: &mut u32,
    reaped: &mut u32,
) -> Result<Vec<HelperBaselineFrame>, FailureCode> {
    use crate::workspace::git::GitReadQuery;
    let queries = [
        (GitReadQuery::HeadTree, HelperBaselineQuery::HeadTree),
        (
            GitReadQuery::UntrackedPaths,
            HelperBaselineQuery::UntrackedPaths,
        ),
        (
            GitReadQuery::HeadIdentity,
            HelperBaselineQuery::HeadIdentity,
        ),
    ];
    let mut frames = Vec::with_capacity(queries.len());
    for (query, reported) in queries {
        if *spawned >= job.budgets.processes {
            return Err(FailureCode::Capacity);
        }
        let command =
            crate::workspace::git::inherited_baseline_command(&job.git, &job.candidate, query)
                .map_err(|_| FailureCode::UnsupportedGit)?;
        *spawned += 1;
        let completed = match crate::execution::run_inherited_controlled_child(
            &command,
            job.budgets.output_bytes.min(8 * 1024),
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(60)),
        )
        .await
        {
            Ok(completed) => completed,
            Err(failure) => return Err(account_failure(failure, spawned, reaped)),
        };
        *reaped += 1;
        let stdout = completed.evidence.stdout();
        let stderr = completed.evidence.stderr();
        frames.push(HelperBaselineFrame {
            query: reported,
            stdout: stdout.bytes.clone(),
            stderr: stderr.bytes.clone(),
            exit_code: completed.evidence.status().code(),
            truncated: stdout.truncated || stderr.truncated || !stdout.complete || !stderr.complete,
        });
    }
    Ok(frames)
}

/// Produces bounded source context inside the inherited sandbox.
async fn context(
    job: &HelperJob,
    deadline: tokio::time::Instant,
    worktree: crate::workspace::authority::WorktreeRef,
    spawned: &mut u32,
    reaped: &mut u32,
) -> (HelperOutcome, Option<HelperPayload>) {
    use crate::{
        intelligence::{
            context::{ContextMode, ContextQuery, lexical_context},
            freshness::Freshness,
        },
        workspace::observation::{
            ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceObservation,
            SourceReadLimits, SourceRevision, read_authorized_source,
        },
    };
    let Some(scope) = job.scope.as_ref() else {
        return (
            HelperOutcome::Failed {
                code: FailureCode::WorkspaceAuthority,
            },
            None,
        );
    };
    let Some(path) = job.parameters["path"].as_str() else {
        return (
            HelperOutcome::Failed {
                code: FailureCode::SourceUnavailable,
            },
            None,
        );
    };
    let relative = PathBuf::from(path);
    let read = read_authorized_source(
        &worktree,
        &relative,
        match SourceReadLimits::new(1024, job.budgets.output_bytes) {
            Ok(limits) => limits,
            Err(_) => {
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::Capacity,
                    },
                    None,
                );
            }
        },
    );
    let (bytes, source_bytes, state, revision) = match read {
        Ok(read) => {
            let bytes = read.contents().to_vec();
            let metadata = read.bytes().clone();
            let revision = blake3::hash(&bytes).to_hex().to_string();
            (bytes, Some(metadata), ObservedState::Present, revision)
        }
        Err(crate::workspace::observation::ObservationError::Missing) => {
            (Vec::new(), None, ObservedState::Missing, "missing".into())
        }
        Err(_) => {
            return (
                HelperOutcome::Failed {
                    code: FailureCode::SourceUnavailable,
                },
                None,
            );
        }
    };
    let observation = match SourceObservation::new(
        worktree,
        scope.authority_epoch,
        1,
        match ObservationRef::new("claude-helper-context") {
            Ok(reference) => reference,
            Err(_) => {
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::Internal,
                    },
                    None,
                );
            }
        },
        relative,
        source_bytes.clone(),
        match SourceRevision::new(revision) {
            Ok(revision) => revision,
            Err(_) => {
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::Internal,
                    },
                    None,
                );
            }
        },
        SourceCoverage::Complete,
        state,
    ) {
        Ok(observation) => observation,
        Err(_) => {
            return (
                HelperOutcome::Failed {
                    code: FailureCode::SourceUnavailable,
                },
                None,
            );
        }
    };
    let query = job
        .parameters
        .get("byte_offset")
        .and_then(serde_json::Value::as_u64)
        .map_or(ContextQuery::File, |byte_offset| ContextQuery::Symbol {
            byte_offset: byte_offset as usize,
        });
    let semantic = if source_bytes.is_some() && job.provider.is_some() {
        provider_context(job, deadline, &observation, &bytes, query, spawned, reaped).await
    } else {
        Ok(None)
    };
    let (context, diagnostics) = match semantic {
        Ok(Some(result)) => result,
        Ok(None) => match lexical_context(
            &observation,
            &bytes,
            query,
            "no accepted provider is configured for this source, or the registered path is missing",
        ) {
            Ok(context) => (context, None),
            Err(_) => {
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::SourceUnavailable,
                    },
                    None,
                );
            }
        },
        Err(FailureCode::ProviderUnavailable) => match lexical_context(
            &observation,
            &bytes,
            query,
            "accepted semantic provider is unavailable",
        ) {
            Ok(context) => (context, None),
            Err(_) => {
                return (
                    HelperOutcome::Failed {
                        code: FailureCode::SourceUnavailable,
                    },
                    None,
                );
            }
        },
        Err(code) => return (HelperOutcome::Failed { code }, None),
    };
    if tokio::time::Instant::now() >= deadline {
        return (
            HelperOutcome::Failed {
                code: FailureCode::Deadline,
            },
            None,
        );
    }
    let mode = match &context.mode {
        ContextMode::Semantic => "semantic".to_owned(),
        ContextMode::Lexical { reason } => format!("lexical ({reason})"),
    };
    let diagnostics = diagnostics.and_then(|diagnostics| {
        (context.freshness == Freshness::Current
            && diagnostics.freshness == Freshness::Provisional
            && diagnostics.source.as_ref() == Some(&context.source)
            && Some(diagnostics.generation) == context.generation
            && diagnostics.document_version == context.document_version
            && diagnostics
                .document_version
                .is_some_and(|version| version > 0))
        .then_some(diagnostics)
    });
    let feedback = diagnostics
        .as_ref()
        .filter(|diagnostics| !diagnostics.diagnostics.is_empty())
        .and_then(|diagnostics| {
            super::facade::FeedbackDelta::new(
                format!(
                    "Provider reported {} diagnostics for the exact source bytes observed by the Claude helper.",
                    diagnostics.diagnostics.len()
                ),
                format!(
                    "source_digest={}; provider_generation={:?}; document_version={:?}",
                    source_bytes
                        .as_ref()
                        .map(|bytes| blake3::Hash::from_bytes(*bytes.digest()).to_hex().to_string())
                        .unwrap_or_else(|| "missing".into()),
                    diagnostics.generation,
                    diagnostics.document_version
                ),
                "Review the bounded diagnostic messages in the latest context result.",
                "provisional helper snapshot; no delivery-time daemon source read",
                None,
            )
        })
        .map(|feedback| feedback.render());
    // Built from the same typed diagnostics as `feedback`, never from its rendered text, so a
    // later unchanged repeat of this exact issue can be recognized without re-parsing the fact.
    let diagnostic_fingerprint = diagnostics
        .as_ref()
        .filter(|diagnostics| !diagnostics.diagnostics.is_empty())
        .map(|diagnostics| {
            let messages: Vec<&str> = diagnostics
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect();
            super::facade::diagnostic_fingerprint(&messages)
        });
    let diagnostic_text = diagnostics.as_ref().map_or_else(
        || "diagnostics_freshness: unknown\ndiagnostic_count: unknown\nfeedback_delta: none".to_owned(),
        |diagnostics| {
            let messages = diagnostics
                .diagnostics
                .iter()
                .take(8)
                .map(|diagnostic| diagnostic.message.chars().take(256).collect::<String>())
                .collect::<Vec<_>>();
            format!(
                "diagnostics_freshness: {:?}\ndiagnostic_readiness: {:?}\ndiagnostic_count: {}\ndiagnostics_truncated: {}\ndiagnostic_messages: {}\nfeedback_delta: {}",
                diagnostics.freshness,
                diagnostics.readiness,
                diagnostics.diagnostics.len(),
                diagnostics.truncated || diagnostics.diagnostics.len() > messages.len(),
                serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()),
                feedback.as_deref().unwrap_or("none"),
            )
        },
    );
    let rendered = format!(
        "mode: {mode}\npath: {path}\nsource_state: {:?}\ncoverage: complete helper-observed path\nposition_encoding: {:?}\nprovider_generation: {:?}\ndocument_version: {:?}\n{diagnostic_text}\ndefinitions: {}\nreferences: {}\nlexical_matches: {}\n\n{}",
        observation.state(),
        context.position_encoding,
        context.generation,
        context.document_version,
        serde_json::to_string(&context.definitions).unwrap_or_else(|_| "null".into()),
        serde_json::to_string(&context.references).unwrap_or_else(|_| "null".into()),
        serde_json::to_string(&context.lexical_matches).unwrap_or_else(|_| "[]".into()),
        context.text,
    );
    let (text, clipped) = fit_result_text(rendered);
    let payload = HelperPayload::Context {
        source: HelperSource {
            path: path.to_owned(),
            present: source_bytes.is_some(),
            digest: source_bytes.as_ref().map(|bytes| *bytes.digest()),
            length: source_bytes.as_ref().map_or(0, SourceBytes::length),
        },
        feedback,
        diagnostic_fingerprint,
        truncated: context.truncated || clipped,
    };
    (HelperOutcome::Complete { text }, Some(payload))
}

/// Runs one accepted provider over exact helper-observed bytes and reaps it before returning.
/// Pyright awaits matching versioned diagnostics under the inherited deadline; Go, Rust, and
/// TypeScript retain their immediate snapshot, and TypeScript also rechecks its closed resolution.
async fn provider_context(
    job: &HelperJob,
    deadline: tokio::time::Instant,
    source: &crate::workspace::observation::SourceObservation,
    bytes: &[u8],
    query: crate::intelligence::context::ContextQuery,
    spawned: &mut u32,
    reaped: &mut u32,
) -> Result<
    Option<(
        crate::intelligence::context::ContextResult,
        Option<crate::intelligence::session::DiagnosticSnapshot>,
    )>,
    FailureCode,
> {
    use crate::{
        execution::WorkspaceAuthority,
        intelligence::{
            freshness::ViewGeneration,
            gopls::GoplsProfile,
            pyright::{PyrightProfile, PyrightProfileIdentity, PyrightWorktree},
            rust::{RustProfile, RustProfileIdentity, RustWorktree},
            session::{GoEnv, ProviderSettings, SessionOptions, with_session},
            typescript::{ProjectResolutionInputsV1, TypeScriptProfile, TypeScriptWorktree},
        },
    };
    let Some(provider) = job.provider.as_ref() else {
        return Ok(None);
    };
    if *spawned >= job.budgets.processes {
        return Err(FailureCode::Capacity);
    }
    let authority = WorkspaceAuthority::from_workspace(
        source.worktree().id(),
        source.worktree().incarnation().to_string(),
        source.worktree().worktree_path().to_path_buf(),
        source.authority_epoch(),
    )
    .map_err(|_| FailureCode::WorkspaceAuthority)?;
    let cache = Path::new(&provider.cache_namespace);
    let (command, settings, pyright_profile, typescript_profile) = match provider.language {
        HelperLanguage::Go => {
            let env = GoEnv::prepare(
                cache.join("go-build"),
                cache.join("go-mod"),
                cache.join("tmp"),
            )
            .ok_or(FailureCode::ProviderUnavailable)?;
            let profile = GoplsProfile::new(
                provider.executable.clone(),
                provider.version.clone(),
                "gopls-v1".into(),
                "gopls-defaults-v1".into(),
                provider.toolchain.clone(),
                provider.trust.clone(),
                provider.cache_namespace.clone(),
            )
            .map_err(|_| FailureCode::ExecutionProfile)?;
            (
                profile
                    .standalone_command(&authority)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                ProviderSettings::GoplsDefaults(env),
                None,
                None,
            )
        }
        HelperLanguage::Rust => {
            let profile = RustProfile::new(RustProfileIdentity {
                binary: provider.executable.clone(),
                rust_analyzer_version: provider.version.clone(),
                cargo: provider
                    .cargo
                    .clone()
                    .ok_or(FailureCode::ExecutionProfile)?,
                cargo_version: provider
                    .cargo_version
                    .clone()
                    .ok_or(FailureCode::ExecutionProfile)?,
                rustc: provider
                    .rustc
                    .clone()
                    .ok_or(FailureCode::ExecutionProfile)?,
                rustc_version: provider
                    .rustc_version
                    .clone()
                    .ok_or(FailureCode::ExecutionProfile)?,
                rustup_toolchain: provider.toolchain.clone(),
                configuration: "cache-priming-and-proc-macro-disabled-v1".into(),
                trust: provider.trust.clone(),
                transport: "stdio-v1".into(),
                cache_namespace: provider.cache_namespace.clone(),
            })
            .map_err(|_| FailureCode::ExecutionProfile)?;
            let worktree = RustWorktree::new(source.worktree().clone(), authority)
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
            (
                profile
                    .command(&worktree)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                ProviderSettings::Rust(profile),
                None,
                None,
            )
        }
        HelperLanguage::Python => {
            let identity = provider
                .pyright
                .as_ref()
                .ok_or(FailureCode::ExecutionProfile)?;
            let profile = PyrightProfile::new(PyrightProfileIdentity {
                binary: identity.script.clone(),
                accepted_script_digest: blake3::Hash::from_hex(&identity.script_blake3)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                version: identity.script_identity.clone(),
                node: identity.node.clone(),
                accepted_node_digest: blake3::Hash::from_hex(&identity.node_blake3)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                node_identity: identity.node_identity.clone(),
                trust: provider.trust.clone(),
                cache_namespace: provider.cache_namespace.clone(),
            })
            .map_err(|_| FailureCode::ExecutionProfile)?;
            let worktree = PyrightWorktree::new(source.worktree().clone(), authority)
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
            let command = profile
                .command(&worktree)
                .map_err(|_| FailureCode::ExecutionProfile)?;
            (
                command,
                ProviderSettings::Pyright(profile.clone()),
                Some(profile),
                None,
            )
        }
        HelperLanguage::TypeScript => {
            let identity = provider
                .typescript
                .as_ref()
                .ok_or(FailureCode::ExecutionProfile)?;
            let bundle = identity.bundle()?;
            let resolution = ProjectResolutionInputsV1::observe(
                source.worktree().clone(),
                source.worktree().worktree_path().join(source.path()),
                &bundle,
            )
            .map_err(|_| FailureCode::ResolutionUnverified)?;
            let profile = TypeScriptProfile::new(
                bundle,
                resolution,
                provider.trust.clone(),
                Path::new(&provider.cache_namespace).to_path_buf(),
            )
            .map_err(|_| FailureCode::ExecutionProfile)?;
            let worktree = TypeScriptWorktree::new(source.worktree().clone(), authority)
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
            (
                profile
                    .command(&worktree)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                ProviderSettings::TypeScript(profile.clone()),
                None,
                Some(profile),
            )
        }
    };
    if pyright_profile.is_some_and(|profile| profile.verify_script().is_err()) {
        return Err(FailureCode::ProviderUnavailable);
    }
    let mut process = command
        .inherited_process()
        .map_err(|_| FailureCode::ExecutionProfile)?;
    process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    *spawned += 1;
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(_) => {
            *spawned = spawned.saturating_sub(1);
            return Err(FailureCode::ProviderUnavailable);
        }
    };
    let (Some(mut input), Some(mut output)) = (child.stdout.take(), child.stdin.take()) else {
        let settled = child.kill().await.is_ok() && child.wait().await.is_ok();
        *reaped += u32::from(settled);
        return Err(if settled {
            FailureCode::ProviderUnavailable
        } else {
            FailureCode::Deadline
        });
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        let settled = child.kill().await.is_ok() && child.wait().await.is_ok();
        *reaped += u32::from(settled);
        return Err(FailureCode::Deadline);
    }
    let operation = with_session(
        &mut input,
        &mut output,
        source.worktree().clone(),
        source.authority_epoch(),
        ViewGeneration {
            backend: 1,
            configuration: 1,
            toolchain: 1,
            view: 1,
        },
        settings,
        SessionOptions {
            request_timeout: remaining.min(Duration::from_secs(60)),
            lifetime: remaining,
        },
        |mut session| async move {
            let context = session.context(source, bytes, query).await?;
            if matches!(session.settings(), ProviderSettings::Pyright(_)) {
                session.wait_for_matching_diagnostics().await;
            }
            let diagnostics = session.diagnostics();
            session.shutdown().await?;
            Ok((context, Some(diagnostics)))
        },
    )
    .await;
    let typescript = provider.language == HelperLanguage::TypeScript;
    let status = if typescript && operation.is_err() {
        crate::execution::terminate_typescript_child_abnormally(
            &mut child,
            Duration::from_millis(100),
            Duration::from_secs(2),
        )
        .await
        .map(|(status, _)| status)
    } else {
        match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
            Ok(status) => status.map_err(crate::execution::ProcessError::Io),
            Err(_) if typescript => crate::execution::terminate_typescript_child_abnormally(
                &mut child,
                Duration::from_millis(100),
                Duration::from_secs(2),
            )
            .await
            .map(|(status, _)| status),
            Err(_) => Err(crate::execution::ProcessError::ReapTimedOut),
        }
    };
    match status {
        Ok(status) => {
            *reaped += 1;
            if typescript_profile
                .as_ref()
                .is_some_and(|profile| profile.verify_resolution().is_err())
            {
                return Err(FailureCode::ResolutionUnverified);
            }
            if !status.success() || operation.is_err() {
                return Err(FailureCode::ProviderUnavailable);
            }
        }
        Err(_) => {
            if !typescript {
                let settled = child.kill().await.is_ok() && child.wait().await.is_ok();
                *reaped += u32::from(settled);
            }
            return Err(FailureCode::Deadline);
        }
    }
    operation
        .map(Some)
        .map_err(|_| FailureCode::ProviderUnavailable)
}

/// Truncates one helper result only at a UTF-8 boundary and marks the omission explicitly.
fn fit_result_text(mut text: String) -> (String, bool) {
    if text.len() <= MAX_RESULT_TEXT_BYTES {
        return (text, false);
    }
    let suffix = "\n[helper result truncated]";
    let mut end = MAX_RESULT_TEXT_BYTES.saturating_sub(suffix.len());
    while !text.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    text.truncate(end);
    text.push_str(suffix);
    (text, true)
}

/// Produces one bounded current Git comparison inside the inherited sandbox.
///
/// `detail_ref` is included only in the shared final-envelope fit. Claude helper Diff pages retain
/// no cursor, so an incomplete page never advertises this handle as a continuation.
async fn diff(
    job: &HelperJob,
    detail_ref: &str,
    deadline: tokio::time::Instant,
    worktree: crate::workspace::authority::WorktreeRef,
    spawned: &mut u32,
    reaped: &mut u32,
) -> (HelperOutcome, Option<HelperPayload>) {
    use crate::workspace::git::{BaselineContext, DiffMode, GitScope};
    let Some(helper_scope) = job.scope.as_ref() else {
        return (
            HelperOutcome::Failed {
                code: FailureCode::WorkspaceAuthority,
            },
            None,
        );
    };
    let Some(helper_baseline) = job.baseline.as_ref() else {
        return (
            HelperOutcome::Failed {
                code: FailureCode::WorkspaceAuthority,
            },
            None,
        );
    };
    let mode = match job.parameters["mode"].as_str() {
        Some("head") => DiffMode::Head,
        Some("staged") => DiffMode::Staged,
        Some("unstaged") => DiffMode::Unstaged,
        _ => {
            return (
                HelperOutcome::Failed {
                    code: FailureCode::Internal,
                },
                None,
            );
        }
    };
    let scope = match GitScope::from_inherited(worktree, helper_scope.authority_epoch, mode) {
        Ok(scope) => scope,
        Err(_) => {
            return (
                HelperOutcome::Failed {
                    code: FailureCode::WorkspaceAuthority,
                },
                None,
            );
        }
    };
    let baseline = match BaselineContext::from_inherited(
        helper_baseline.reference.clone(),
        helper_baseline.captured,
        helper_baseline.digest,
        GitScope::from_inherited(
            scope.worktree().clone(),
            scope.authority_epoch(),
            DiffMode::Head,
        )
        .expect("validated helper scope"),
    ) {
        Ok(baseline) => baseline,
        Err(_) => {
            return (
                HelperOutcome::Failed {
                    code: FailureCode::WorkspaceAuthority,
                },
                None,
            );
        }
    };
    let mut runner = HelperSnapshotRunner {
        output_cap: job.budgets.output_bytes,
        remaining_processes: job.budgets.processes.saturating_sub(*spawned),
        deadline,
        spawned: 0,
        reaped: 0,
        failure: None,
    };
    let evidence = crate::workspace::git::snapshot::collect_snapshot_scoped(
        scope.clone(),
        None,
        &job.git,
        1,
        "claude-helper-diff",
        baseline,
        &mut runner,
    )
    .await;
    *spawned = spawned.saturating_add(runner.spawned);
    *reaped = reaped.saturating_add(runner.reaped);
    let evidence = match evidence {
        Ok(evidence) => evidence,
        Err(_) => {
            return (
                HelperOutcome::Failed {
                    code: runner.failure.unwrap_or(FailureCode::SourceUnavailable),
                },
                None,
            );
        }
    };
    if tokio::time::Instant::now() >= deadline {
        return (
            HelperOutcome::Failed {
                code: FailureCode::Deadline,
            },
            None,
        );
    }
    let comparison = evidence.comparison().clone();
    let fitted = crate::assistance::worker::snapshots::fit_diff_page(
        mode,
        helper_scope.authority_epoch,
        detail_ref,
        32,
        false,
        |max_hunks| {
            crate::changes::compose_diff(
                &scope,
                &comparison,
                evidence.clone(),
                crate::changes::DiffSelectionBudget::bounded(max_hunks, 24 * 1024),
            )
        },
    );
    match fitted {
        Ok((
            result,
            PeerReply::Complete {
                text, truncated, ..
            },
        )) => (
            HelperOutcome::Complete { text },
            Some(HelperPayload::Diff {
                truncated: truncated
                    || result.truncated_output()
                    || result.overflow_hunks() > 0
                    || result.overflow_bytes() > 0,
            }),
        ),
        Ok(_) => unreachable!("shared diff fitter always returns a complete reply"),
        Err(code) => (HelperOutcome::Failed { code }, None),
    }
}

/// Executes Workspace snapshot intents sequentially under the one already-held helper admission.
struct HelperSnapshotRunner {
    /// Per-stream retained-byte cap.
    output_cap: usize,
    /// Maximum additional direct children this helper job may start.
    remaining_processes: u32,
    /// Per-child upper bound within the helper's finite operation lifetime.
    deadline: tokio::time::Instant,
    /// Direct children actually started.
    spawned: u32,
    /// Direct children positively waited or killed and waited.
    reaped: u32,
    /// First product failure retained separately from Workspace parsing.
    failure: Option<FailureCode>,
}

impl crate::workspace::git::snapshot::SnapshotRunner for HelperSnapshotRunner {
    /// Runs one immutable intent and correlates its scratch lifecycle with actual wait evidence.
    async fn run(
        &mut self,
        intent: crate::workspace::git::snapshot::SnapshotIntent,
    ) -> Result<crate::execution::CapturedProcessEvidence, crate::workspace::git::GitError> {
        use crate::{execution::InheritedChildFailure, workspace::git::GitError};
        if self.spawned >= self.remaining_processes {
            self.failure = Some(FailureCode::Capacity);
            return Err(GitError::EvidenceTooLarge);
        }
        let command = intent.command()?;
        let remaining = self
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_secs(60));
        if remaining.is_zero() {
            self.failure = Some(FailureCode::Deadline);
            return Err(GitError::IncompleteIdentity);
        }
        self.spawned += 1;
        let completed = match crate::execution::run_inherited_controlled_child(
            &command,
            self.output_cap,
            remaining,
        )
        .await
        {
            Ok(completed) => completed,
            Err(InheritedChildFailure::NeverStarted) => {
                self.spawned -= 1;
                self.failure = Some(FailureCode::SourceUnavailable);
                return Err(GitError::IncompleteIdentity);
            }
            Err(InheritedChildFailure::Reaped) => {
                self.reaped += 1;
                self.failure = Some(FailureCode::Deadline);
                return Err(GitError::IncompleteIdentity);
            }
            Err(InheritedChildFailure::Unsettled) => {
                self.failure = Some(FailureCode::Deadline);
                return Err(GitError::IncompleteIdentity);
            }
        };
        self.reaped += 1;
        if intent.snapshot_directory().is_some() {
            intent.bind_process(completed.launch_identity)?;
        }
        intent.acknowledge_reap(&completed.evidence)?;
        Ok(completed.evidence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistance::claude_worker::{
        AcceptedIdentity, Delivery, HELPER_PROTOCOL, HelperActor, HelperBaseline, HelperBudgets,
        HelperJob, HelperOperation, HelperScope, LaunchRecognition,
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
            scope: None,
            baseline: None,
            provider: None,
            edit_source: None,
            parameters: serde_json::json!({"activation_id": "activate"}),
            budgets: HelperBudgets {
                output_bytes: 4096,
                processes: 6,
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
    /// Proves the daemon-minted Diff budget covers the snapshot's bounded per-path walk.
    ///
    /// A real repository with more tracked paths than the old per-operation process ceiling
    /// used to exhaust the helper's child budget mid-walk and fail every Claude Diff closed as
    /// `capacity` without any write. This regression runs the real helper Diff over a worktree
    /// with more than sixty-four tracked paths using exactly the minted production ceilings, and
    /// additionally shows the previous small budget fails on the same repository, so the budget
    /// and the snapshot bound can never silently diverge again.
    #[tokio::test]
    async fn diff_helper_completes_the_bounded_snapshot_walk_of_a_real_repository() {
        let candidate = worktree();
        for index in 0..80 {
            std::fs::write(
                candidate.join(format!("file-{index:03}.txt")),
                format!("tracked content {index}\n"),
            )
            .unwrap();
        }
        std::fs::write(candidate.join("changed.txt"), "before\n").unwrap();
        let status = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&candidate)
            .args([
                "-c",
                "user.name=helper",
                "-c",
                "user.email=helper@invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["add", "--", "."])
            .status()
            .unwrap();
        assert!(status.success());
        let status = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&candidate)
            .args([
                "-c",
                "user.name=helper",
                "-c",
                "user.email=helper@invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(["commit", "--quiet", "-m", "many tracked paths"])
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(candidate.join("changed.txt"), "after\n").unwrap();
        let root_identity = crate::workspace::observation::native_directory_identity(
            &std::fs::File::open(&candidate).unwrap(),
        )
        .expect("fixture worktree has a native root identity");
        let diff_job = |processes: u32, output_bytes: usize| HelperJob {
            protocol: HELPER_PROTOCOL,
            operation: HelperOperation::Diff,
            candidate: candidate.clone(),
            git: PathBuf::from("/usr/bin/git"),
            canonical_root: Some(candidate.clone()),
            scope: Some(HelperScope {
                worktree_id: "worktree".into(),
                incarnation: 1,
                root: candidate.clone(),
                repository_root: candidate.clone(),
                git_common_dir: PathBuf::from(".git"),
                native_root_identity: root_identity,
                authority_epoch: 1,
            }),
            baseline: Some(HelperBaseline {
                reference: "baseline".into(),
                captured: false,
                digest: None,
            }),
            provider: None,
            edit_source: None,
            parameters: serde_json::json!({"mode":"head"}),
            budgets: HelperBudgets {
                output_bytes,
                processes,
                deadline_ms: 120_000,
            },
        };
        // The previously minted ceiling cannot finish the walk of this repository and fails
        // closed partway through its child budget instead of delivering any diff.
        let (outcome, small, _, _) = perform(&diff_job(64, 64 * 1024), "detail").await;
        assert!(
            matches!(
                outcome,
                HelperOutcome::Failed {
                    code: FailureCode::Capacity
                } | HelperOutcome::Failed {
                    code: FailureCode::SourceUnavailable
                }
            ),
            "the small legacy budget must fail closed, saw {outcome:?}"
        );
        assert!(
            small.spawned > 8,
            "legacy budget consumed {}",
            small.spawned
        );
        // The minted production ceilings complete the same walk and deliver a real diff.
        let (outcome, children, _, payload) = perform(
            &diff_job(
                1024,
                crate::workspace::git::snapshot::MAX_SNAPSHOT_BLOB_BYTES,
            ),
            "detail",
        )
        .await;
        let HelperOutcome::Complete { text } = outcome else {
            panic!("the bounded snapshot walk completes, saw {outcome:?}")
        };
        assert!(children.settled() && children.spawned > 64);
        assert!(text.contains("changed.txt"), "diff text: {text}");
        assert!(matches!(payload, Some(HelperPayload::Diff { .. })));
        let _ = std::fs::remove_dir_all(&candidate);
    }

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
        assert!(result.children.settled() && result.children.spawned == 6);
        let HelperOutcome::Complete { text } = &result.outcome else {
            panic!("real git discovery completed");
        };
        assert!(text.contains("activation baseline"));
        let Some(HelperPayload::Start { baseline }) = &result.payload else {
            panic!("real baseline evidence completed");
        };
        assert_eq!(result.discovery.len(), 3);
        assert_eq!(baseline.len(), 3);
        drop(endpoint);
        let _ = std::fs::remove_dir_all(&candidate);
    }

    /// The real helper process is the only thing that can mint a settled token for a real worktree.
    ///
    /// Integrated rather than a private ledger exercise: a real `Bash`-shaped foreground helper
    /// claims over the real private socket, runs six real Git children against a real worktree,
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
            payload: None,
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
