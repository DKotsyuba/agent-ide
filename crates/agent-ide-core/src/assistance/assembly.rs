//! Explicit Codex/Claude ingress and finite dispatch into one daemon-owned product worker.

/// Stable closed result types shared by existing callers of the assembly boundary.
pub use super::reply::{HostBindingCause, MissingPeer, PeerReply};
use super::{
    host_binding::{
        BindingStatus, HookPhase, HostBindingGuard, HostKind, parse_candidate,
        parse_channel_session, parse_claude_call_id, parse_claude_hook_event, parse_hook_event,
        parse_host_kind,
    },
    launcher::LauncherConfig,
    problems::{ProjectProblemFeed, may_write, triggers_check},
    worker::WorkerHandle,
};
use crate::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, AssistanceMethod, OpaqueJson,
};
use crate::errorlog;
use crate::telemetry::{CacheState, DiagnosticState, adapters};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::Read,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
};

/// Owns exact host binding and at most one configured worker for one daemon boot.
/// Private launcher values never come from method arguments; no binding lock crosses an I/O await.
pub struct ProductDispatcher {
    /// Serializes exact hook/MCP correlation, liveness consumes and stop linearization.
    bindings: Arc<Mutex<HostBindingGuard>>,
    /// Absent in discovery-only mode, where peer operations remain explicitly unavailable.
    worker: Option<WorkerHandle>,
    /// Private nonce distinguishes effective channel/binding generations across daemon restarts.
    scope: Option<[u8; 32]>,
    /// The daemon's single physical-effect admission owner, shared with the worker.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Enables direct trusted Codex metadata binding only for an owned managed-MCP daemon.
    managed_codex: bool,
    /// Enables lease-registered Claude worktrees only for the managed shared daemon.
    managed_claude: bool,
    /// Managed shared Claude daemon serving this release's contract: one channel for every
    /// admitted attachment, actor tags, recovery and identity ingress, pre-attachment targets.
    shared_claude_channel: bool,
    /// Rate window for hooks of sessions that never activated; one line per ten minutes.
    hook_noise: Mutex<crate::errorlog::RateWindow>,
    /// Managed Claude only: the attachment each still-pending pre-hook arrived through, keyed by
    /// its exact actor and call, so a start activates the worktree its own hooks run in.
    /// Recorded before the guard sees the pre and pruned only to still-pending pres, so its size
    /// follows the guard's own pending bounds and live evidence is never dropped. Lock order:
    /// this map before `bindings`, never the reverse.
    pre_attachments: Mutex<std::collections::BTreeMap<(String, String), String>>,
}

/// Names the closed host-binding cause of an ingress step that failed (QW-6), so no early exit of
/// the dispatcher answers a bare `unavailable: host_binding`.
trait OrCause<T> {
    /// Keeps the success value, or fails with `cause`.
    fn or_cause(self, cause: HostBindingCause) -> Result<T, HostBindingCause>;
}

/// An absent value fails with the supplied cause.
impl<T> OrCause<T> for Option<T> {
    /// `Some(value)` keeps `value`; `None` fails with `cause`.
    fn or_cause(self, cause: HostBindingCause) -> Result<T, HostBindingCause> {
        self.ok_or(cause)
    }
}

/// Any error fails with the supplied cause; the error itself is dropped, because it may carry
/// caller-supplied text that must never reach a reply or the journal.
impl<T, E> OrCause<T> for Result<T, E> {
    /// `Ok(value)` keeps `value`; any `Err` fails with `cause`.
    fn or_cause(self, cause: HostBindingCause) -> Result<T, HostBindingCause> {
        self.map_err(|_| cause)
    }
}

/// Closed journal context a call's ingress establishes on its way through [`ProductDispatcher::
/// handle_tagged`] (QW-4); every field stays empty for a call that never got that far.
#[derive(Default)]
struct CallFacts {
    /// The validated host binding the call belongs to.
    binding: Option<super::host_binding::BindingRef>,
    /// The binding's activation role *before* the call ran (a stop removes it).
    role: Option<errorlog::Role>,
    /// Set by the inspection this call performs when it hands a retained terminal result to the
    /// call's caller; this call's own flag, shared with no other inspection.
    delivery: Arc<std::sync::atomic::AtomicBool>,
}

/// The closed cause a guard refusal is reported with.
fn binding_cause(reason: super::host_binding::BindingUnavailable) -> HostBindingCause {
    HostBindingCause::from_binding(reason).unwrap_or(HostBindingCause::InvalidMetadata)
}

/// Most actor tags one `claudecode/recover` announcement may carry.
const MAX_RECOVER_TAGS: usize = 32;

/// Parses a `claudecode/recover` announcement: an array of at most [`MAX_RECOVER_TAGS`] actor tags,
/// each exactly 64 lowercase hex digits; anything else is malformed host metadata.
fn actor_tags(tags: &Value) -> Option<Vec<&str>> {
    let tags = tags.as_array()?;
    if tags.len() > MAX_RECOVER_TAGS {
        return None;
    }
    tags.iter()
        .map(|tag| {
            tag.as_str()
                .filter(|tag| super::host_binding::valid_actor_tag(tag))
        })
        .collect()
}

/// Size above which the managed Claude hook path prunes attachment records of settled pres.
const PRE_ATTACHMENT_PRUNE: usize = 256;

/// Journals one hook of a session that never activated the IDE (T15B hook-noise follow-up).
///
/// Such a hook is bookkeeping, not a failure: `info`/`skipped` with the closed `hook_inactive`
/// detail, at most one line per daemon per ten minutes, counting the suppressed repetitions. A
/// channel with a binding — active or stopped — did activate once, so its real hook failures keep
/// their `warn` lines.
fn log_hook_inactive(noise: &Mutex<crate::errorlog::RateWindow>, host: HostKind) {
    let Ok(mut noise) = noise.lock() else {
        return;
    };
    let Some(suppressed) = noise.record(crate::errorlog::now_ms(), HOOK_INACTIVE_WINDOW_MS) else {
        return;
    };
    errorlog::record(
        errorlog::Method::Hook,
        errorlog::Outcome::Skipped,
        errorlog::Fields {
            host: Some(host),
            detail: Some("hook_inactive"),
            count: (suppressed > 0).then_some(suppressed),
            ..Default::default()
        },
    );
}

/// One journal line per daemon per ten minutes for never-activated hook traffic.
const HOOK_INACTIVE_WINDOW_MS: u64 = 600_000;
/// Bounded arrival window for a Claude pre-hook that has not reached the guard yet.
///
/// Generous against the observed ~405 ms submission stall, short enough to fit the client's
/// one-second first-reply budget with room for the call itself.
const PRE_ARRIVAL_WAIT: std::time::Duration = std::time::Duration::from_millis(600);
/// Arrival window for an `ide.start` alone: a first start lost the race with its own pre-hook
/// after 606 ms while the hooks flowed normally a moment later, and agents never repeat a call
/// (E013 item 3). A start is idempotent by activation id, so waiting longer can only help.
const START_PRE_ARRIVAL_WAIT: std::time::Duration = std::time::Duration::from_millis(2_500);
/// Poll interval of the pre-arrival window.
const PRE_ARRIVAL_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Selects the bounded host pre-hook arrival window for one dispatched tool method.
fn pre_arrival_wait(method: AssistanceMethod) -> std::time::Duration {
    if method == AssistanceMethod::Start {
        START_PRE_ARRIVAL_WAIT
    } else {
        PRE_ARRIVAL_WAIT
    }
}

#[test]
fn first_start_waits_longer_for_its_pre_hook_than_other_calls() {
    assert_eq!(
        pre_arrival_wait(AssistanceMethod::Start),
        std::time::Duration::from_millis(2_500)
    );
    assert_eq!(
        pre_arrival_wait(AssistanceMethod::Context),
        std::time::Duration::from_millis(600)
    );
}

/// Returns the host contract one sanitized hook observation names, when it names a supported one.
fn observed_host(object: &serde_json::Map<String, Value>) -> Option<HostKind> {
    match object.get("host")?.as_str()? {
        "codex" => Some(HostKind::Codex),
        "claude" => Some(HostKind::Claude),
        _ => None,
    }
}

/// Logs the specific closed [`BindingUnavailable`](super::host_binding::BindingUnavailable) reason
/// a host/MCP correlation was refused for (T107), right before it is collapsed into the coarse
/// `MissingPeer::HostBinding` reply every peer actually sees. `correlation_id` is the opaque
/// `tool_use_id`/`call_id` the model already holds.
fn log_binding_unavailable(
    tool: super::facade::AssistanceTool,
    host: HostKind,
    correlation_id: &str,
    reason: super::host_binding::BindingUnavailable,
) {
    let method = match tool {
        super::facade::AssistanceTool::Start => errorlog::Method::Start,
        super::facade::AssistanceTool::Context => errorlog::Method::Context,
        super::facade::AssistanceTool::Diff => errorlog::Method::Diff,
        super::facade::AssistanceTool::Inspect => errorlog::Method::Inspect,
        super::facade::AssistanceTool::Stop => errorlog::Method::Stop,
        super::facade::AssistanceTool::Edit => errorlog::Method::Edit,
        super::facade::AssistanceTool::Outline => errorlog::Method::Outline,
        super::facade::AssistanceTool::Read => errorlog::Method::Read,
        super::facade::AssistanceTool::Symbol => errorlog::Method::Symbol,
        super::facade::AssistanceTool::Graph => errorlog::Method::Graph,
        super::facade::AssistanceTool::Test => errorlog::Method::Test,
    };
    errorlog::record(
        method,
        errorlog::Outcome::Unavailable,
        errorlog::Fields {
            reason: Some(reason.into()),
            host: Some(host),
            correlation: Some(correlation_id),
            ..Default::default()
        },
    );
}

/// Names the closed cause of one host-binding refusal for the model-facing reply (T15B).
///
/// A consumed or rejected call identity is its own replay evidence. When instead this exact
/// pre/binding is missing, the deeper question is whether the calling channel ever delivered a
/// hook to this daemon: a silent channel whose bound project also resolves below no allowed root
/// is the moved-scratch session of T15B (`outside_allowed_roots`), a silent channel with an
/// admitted project never saw its hooks (`hooks_not_delivered`), and a live channel simply lacks
/// this one observation. Every other guard refusal keeps its own closed tag.
fn host_binding_cause(
    worker: Option<&WorkerHandle>,
    hooks_delivered: bool,
    attachment: &str,
    reason: super::host_binding::BindingUnavailable,
) -> Option<HostBindingCause> {
    if !hooks_delivered
        && matches!(
            reason,
            super::host_binding::BindingUnavailable::MissingPre
                | super::host_binding::BindingUnavailable::InactiveBinding
        )
    {
        let outside_roots = worker.is_some_and(|worker| {
            worker.target(attachment).is_some_and(|target| {
                crate::assistance::launcher::admit_worktree(
                    worker.allowed_roots(),
                    &target.candidate,
                )
                .is_err()
            })
        });
        return Some(if outside_roots {
            HostBindingCause::OutsideAllowedRoots
        } else {
            HostBindingCause::HooksNotDelivered
        });
    }
    HostBindingCause::from_binding(reason)
}

/// Reports whether one validated context call names the in-memory `kind: "problems"` feed.
///
/// EYES-r2: such calls are answered entirely from the daemon's in-memory problem source for the
/// caller's bound worktree. On Claude they must short-circuit before foreground-helper routing,
/// and on managed Codex they skip native read-boundary reconciliation. A call carrying a detail
/// reference is retrieval of an already-settled result and keeps its existing route.
fn is_problems_context(method: AssistanceMethod, parameters: &Value) -> bool {
    method == AssistanceMethod::Context
        && parameters.get("detail_ref").is_none()
        && parameters.get("kind").and_then(Value::as_str) == Some("problems")
}

/// Returns the due `<agent-ide>` status plate for one hook-delivering host's post phase, or `None`.
///
/// Refreshes file-resolved environment identities, then reads in-memory snapshots without waiting
/// for a running check. The plate is skipped, and
/// stays due for a later hook, whenever it could not fit beside `feedback` inside one bounded hook
/// context (EYES-r2 §5/§6). Hosts whose [`super::host_binding::FeedDelivery`] is
/// [`super::host_binding::FeedDelivery::Replies`] never take
/// this path; their plates ride terminal `ide.*` replies instead (`attach_reply_plate`).
async fn due_plate(
    feed: Option<&Arc<ProjectProblemFeed>>,
    worker: Option<&WorkerHandle>,
    fingerprint: &[u8; 32],
    feedback: Option<&str>,
) -> Option<String> {
    if let Some(worker) = worker {
        let _ = worker.git_notice(fingerprint).await;
    }
    let reserved = feedback.map_or(0, |text| text.len() + 1);
    if reserved + crate::feed::MAX_BLOCK_BYTES > super::reply::MAX_FEEDBACK_BYTES {
        return None;
    }
    if let Some(feed) = feed {
        return feed.next_block(fingerprint);
    }
    let worker = worker?;
    let line = worker.test_status_line(fingerprint)?;
    let plate = format!("<agent-ide>\n{line}\n</agent-ide>");
    (reserved + plate.len() <= super::reply::MAX_FEEDBACK_BYTES
        && worker.mark_test_status_delivered(fingerprint, &line))
    .then_some(plate)
}

/// Leads `plate` with due one-shot git and environment notices when the
/// merged plate still `fits`; the line is consumed only when delivered, otherwise it stays due
/// and `plate` is returned unchanged. A due line with no other plate becomes a plate of its own.
async fn with_git_notice(
    worker: &WorkerHandle,
    fingerprint: &[u8; 32],
    plate: Option<String>,
    fits: impl FnOnce(&str) -> bool,
) -> Option<String> {
    let Some(notice) = worker.git_notice(fingerprint).await else {
        return plate;
    };
    let merged = match &plate {
        Some(plate) => plate.replacen("<agent-ide>\n", &format!("<agent-ide>\n{notice}\n"), 1),
        None => format!("<agent-ide>\n{notice}\n</agent-ide>"),
    };
    if fits(&merged) && worker.consume_git_notice(fingerprint, &notice).await {
        Some(merged)
    } else {
        plate
    }
}

/// Attaches the due status plate to one reply-delivered host's terminal reply (T28B).
///
/// The plate is marked delivered only once it was actually attached: the whole reply is fitted
/// into the exact final MCP carrier the host reads (`Envelope::WithStructured`), shrinking only
/// owner text, and a plate that cannot fit beside any (possibly shrunk) body stays due for the
/// next terminal reply. The fitting decision and the delivery record are one atomic feed step.
fn attach_reply_plate(
    feed: Option<&Arc<ProjectProblemFeed>>,
    fingerprint: &[u8; 32],
    reply: &mut PeerReply,
    test_status_snapshot: Option<&str>,
) -> Option<String> {
    let feed = feed?;
    let mut fitting = reply.clone();
    let plate =
        feed.next_block_when_with_test_status(fingerprint, test_status_snapshot, |plate| {
            loop {
                if super::content::fits_with_status(
                    &fitting,
                    plate,
                    super::content::Envelope::WithStructured,
                ) {
                    return true;
                }
                if !fitting.shrink_text() {
                    return false;
                }
            }
        })?;
    *reply = fitting;
    Some(plate)
}

/// Attaches one explicit-test status plate when project checks are not configured.
fn attach_test_plate(
    worker: &WorkerHandle,
    binding: &[u8; 32],
    reply: &mut PeerReply,
    test_status_snapshot: Option<&str>,
) -> Option<String> {
    let current = worker.test_status_line(binding);
    // The reply's own snapshot wins a completion race, but only while that line is still
    // undelivered: a repeated status lookup of a finished run must not plate it again.
    let line = match test_status_snapshot {
        Some(snapshot) => (current.as_deref() == Some(snapshot)).then(|| snapshot.to_owned()),
        None => current,
    }?;
    let plate = format!("<agent-ide>\n{line}\n</agent-ide>");
    let mut fitting = reply.clone();
    loop {
        if super::content::fits_with_status(
            &fitting,
            &plate,
            super::content::Envelope::WithStructured,
        ) {
            if worker.mark_test_status_delivered(binding, &line) {
                *reply = fitting;
                return Some(plate);
            }
            return None;
        }
        if !fitting.shrink_text() {
            return None;
        }
    }
}

/// Reuses the test status captured in the worker's reply to avoid a completion race before plating.
fn test_status_snapshot(reply: &PeerReply) -> Option<String> {
    let PeerReply::Complete {
        kind: super::reply::ResultKind::Test,
        text,
        ..
    } = reply
    else {
        return None;
    };
    let line = super::tests::plate_line(text.lines().next()?);
    (line.starts_with("tests #") && !line.contains("started —") && !line.contains("still running"))
        .then(|| line.to_owned())
}

impl std::fmt::Debug for ProductDispatcher {
    /// Omits private channel nonces, host identities and all worker state.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProductDispatcher(..)")
    }
}

impl Default for ProductDispatcher {
    /// Creates an unconfigured host boundary with a fresh process-independent channel nonce.
    /// Entropy failure leaves binding unavailable rather than reusing a prior daemon scope.
    fn default() -> Self {
        let admission = Arc::new(Mutex::new(super::worker::admission_controller()));
        let mut scope = [0; 32];
        let scope = std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut scope))
            .ok()
            .map(|_| scope);
        Self {
            bindings: Arc::new(Mutex::new(HostBindingGuard::default())),
            worker: None,
            scope,
            admission,
            managed_codex: false,
            managed_claude: false,
            shared_claude_channel: false,
            hook_noise: Mutex::new(crate::errorlog::RateWindow::default()),
            pre_attachments: Mutex::new(std::collections::BTreeMap::new()),
        }
    }
}
impl ProductDispatcher {
    /// Installs one immutable trusted map; peer startup waits for Application's exclusive daemon lock.
    ///
    /// When the launcher enables project checks (EYES-r1 §1), the worker also receives one
    /// [`ProjectProblemFeed`] whose completed checks are recorded as telemetry once the worker's
    /// telemetry owner has started; otherwise the worker is unchanged from v0.2.
    pub fn with_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::default();
        // Language computations route in process or to each shipped module through one host
        // sharing the daemon's admission; ignored fallback-switch entries are journaled once.
        let modules = Arc::new(crate::modules::router::ModuleHost::new(
            dispatcher.admission.clone(),
        ));
        for line in modules.ignored_lines() {
            errorlog::record(
                errorlog::Method::Daemon,
                errorlog::Outcome::Refused,
                errorlog::Fields {
                    detail: Some(&line),
                    ..Default::default()
                },
            );
        }
        crate::modules::calls::install(modules);
        if let Some(scope) = dispatcher.scope {
            let worker = WorkerHandle::new(
                dispatcher.bindings.clone(),
                launcher.clone(),
                scope,
                dispatcher.admission.clone(),
            );
            let telemetry = worker.telemetry_slot();
            let feed = ProjectProblemFeed::from_launcher(
                &launcher,
                Arc::new(move |snapshot: &crate::checks::ProblemSnapshot| {
                    adapters::log_project_check(snapshot);
                    if let Some(telemetry) = telemetry.lock().ok().and_then(|slot| slot.clone()) {
                        adapters::project_check(&telemetry, snapshot);
                    }
                }),
            );
            dispatcher.worker = Some(match feed {
                Some(feed) => worker.with_project_feed(Arc::new(feed)),
                None => worker,
            });
        }
        dispatcher
    }
    /// Installs one launcher for an owned managed-MCP daemon with direct Codex metadata binding.
    ///
    /// The caller must supply the fresh process-private attachment configuration produced by
    /// [`LauncherConfig::bind_one_candidate`]. Legacy daemon construction remains hook-correlated,
    /// and Claude always retains its existing hook/helper lifecycle.
    pub fn with_managed_codex_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::with_launcher(launcher);
        dispatcher.managed_codex = true;
        dispatcher
    }
    /// Installs the shared Claude launcher with per-MCP candidate registration.
    ///
    /// The `AGENT_IDE_TEST_LEGACY_CLAUDE_DAEMON=1` seam serves the 0.10.2 contract instead —
    /// one channel per attachment, no actor tags, recovery announcements, identity queries or
    /// pre-attachment targets — so product tests can exercise a current front against a daemon
    /// without those capabilities. Only a `test-seams` build reads it.
    pub fn with_managed_claude_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::with_launcher(launcher);
        dispatcher.managed_claude = true;
        dispatcher.shared_claude_channel =
            crate::test_seams::var("AGENT_IDE_TEST_LEGACY_CLAUDE_DAEMON").as_deref() != Some("1");
        dispatcher
    }
    /// Derives the same opaque channel for hook/MCP input under this exact daemon nonce.
    ///
    /// A managed shared Claude daemon derives ONE channel for every attachment it admitted: each
    /// worktree's attachment then selects only that worktree's target, while every actor of every
    /// session of the repository meets on one channel, paired exactly by call id and the actor
    /// of its genuine pre-hook. An actor's calls therefore never depend on which worktree's
    /// attachment the MCP dispatched them through. Every other daemon keeps one channel per
    /// attachment.
    fn channel(&self, attachment: &str) -> Option<super::host_binding::ChannelSessionRef> {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.scope?);
        if self.shared_claude_channel {
            hash.update(b"agent-ide managed claude channel");
        } else {
            hash.update(attachment.as_bytes());
        }
        parse_channel_session(hash.finalize().to_hex().as_bytes()).ok()
    }
    /// Returns the attachment the pending pre of this exact actor and call arrived through.
    fn pre_attachment(&self, actor: &str, call: &str) -> Option<String> {
        self.pre_attachments
            .lock()
            .ok()?
            .get(&(actor.to_owned(), call.to_owned()))
            .cloned()
    }

    /// The actor (and its pending call) a 0.10.2 front's re-activation
    /// (`claudecode/reactivation: true`) binds: one with a pending pre delivered through the
    /// calling attachment, which keeps that front's
    /// per-attachment scope now that every managed Claude attachment shares one channel.
    fn legacy_reactivation_actor(
        &self,
        attachment: &str,
        channel: &super::host_binding::ChannelSessionRef,
    ) -> Option<(String, String)> {
        if !self.shared_claude_channel {
            // One channel per attachment: any pending pre on it came through that attachment.
            return self.bindings.lock().ok()?.pending_claude_pre(channel);
        }
        let recorded: Vec<(String, String)> = self
            .pre_attachments
            .lock()
            .ok()?
            .iter()
            .filter(|(_, through)| through.as_str() == attachment)
            .map(|(key, _)| key.clone())
            .collect();
        let bindings = self.bindings.lock().ok()?;
        recorded
            .into_iter()
            .find(|(actor, call)| bindings.has_claude_pre(actor, call, channel))
    }

    /// Parses separated ingress and commits binding transitions before queue, inspection or stop I/O.
    ///
    /// `status` is written only when a terminal `ide.*` reply for a reply-delivered host (T28B)
    /// carries the due status plate on top; the caller renders it ahead of the reply. A native
    /// hook has no current sandbox metadata, so it uses the binding feed's sticky restriction
    /// state before triggering a check or releasing cached feedback. Missing or unsupported
    /// metadata on a validated Codex binding restricts that feed before an error is returned.
    #[cfg(test)]
    async fn handle(
        &self,
        request: &AssistanceDispatch,
        status: &mut Option<String>,
    ) -> Option<PeerReply> {
        self.handle_tagged(request, status, &mut None, &mut CallFacts::default())
            .await
            .ok()
    }

    /// Parses and commits one native hook observation (the `HookSubmit` ingress) and returns its
    /// typed reply, or the closed cause that refused it before any reply could be built.
    ///
    /// Only exact lifecycle correlation settles a hook: the call id must equal the transport
    /// correlation id, the channel must be one this daemon configured, and the binding guard
    /// decides the rest; nothing here pairs by anything looser than that.
    async fn handle_hook(
        &self,
        hook: &crate::app::transport::HookSubmit,
    ) -> Result<PeerReply, HostBindingCause> {
        let observation: Value = serde_json::from_str(hook.sanitized_observation_json().as_str())
            .or_cause(HostBindingCause::InvalidMetadata)?;
        let object = observation
            .as_object()
            .or_cause(HostBindingCause::InvalidMetadata)?;
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.accepts_attachment(hook.opaque_attachment()))
        {
            // This daemon never registered the calling attachment: its session was never
            // activated here, so the refused submission is bookkeeping, not a failure.
            if let Some(host) = observed_host(object) {
                log_hook_inactive(&self.hook_noise, host);
            }
            return Err(HostBindingCause::InvalidAttachment);
        }
        // Six fixed relayed fields plus the optional post-phase `tool_name` and the
        // writer tool's optional `tool_file` (the changed file whose language alone is
        // re-checked); any other field — for example a retired helper field — means the
        // observation does not correlate.
        let relayed = [
            "host",
            "phase",
            "actor_id",
            "call_id",
            "session_id",
            "agent_type",
            "tool_name",
            "tool_file",
        ]
        .iter()
        .filter(|field| object.contains_key(**field))
        .count();
        if relayed != object.len() {
            return Err(HostBindingCause::InvalidMetadata);
        }
        let phase = match object
            .get("phase")
            .and_then(Value::as_str)
            .or_cause(HostBindingCause::MissingField)?
        {
            "pre" => "PreToolUse",
            "post" => "PostToolUse",
            "post_failure" => "PostToolUseFailure",
            "permission_denied" => "PermissionDenied",
            "post_batch" => "PostToolBatch",
            _ => return Err(HostBindingCause::UnsupportedHookPhase),
        };
        let event = (|| {
                    Some(match object.get("host")?.as_str()? {
                    "codex" => parse_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"tool_use_id":object.get("call_id")?,"tool_name":object.get("tool_name")})
                            .to_string().as_bytes(),
                    ),
                    "claude" => parse_claude_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"agent_type":object.get("agent_type")?,"tool_use_id":object.get("call_id")?,"tool_name":object.get("tool_name")})
                            .to_string().as_bytes(),
                    ),
                    _ => return None,
                    })
                })()
                .or_cause(HostBindingCause::MissingField)?
                .map_err(binding_cause)?;
        if event.optional_call_id().unwrap_or("post-tool-batch") != hook.correlation_id() {
            return Err(HostBindingCause::Mismatch);
        }
        let channel = self
            .channel(hook.opaque_attachment())
            .or_cause(HostBindingCause::InvalidAttachment)?;
        let call_id = event.optional_call_id().map(str::to_owned);
        // EYES-r2 §5/§6: a hook-delivering host's paired native post both triggers a
        // project check (`triggers_check` is host-specific; T29B §4) and evaluates the
        // due plate even when it does not trigger. Managed Codex delivers on hooks and
        // replies at once; hosts without hook delivery take replies instead.
        let hook_post = event.host().feed_delivery().allows_hooks()
            && matches!(event.phase(), HookPhase::Post | HookPhase::PostFailure);
        let triggers_check = hook_post && triggers_check(event.host(), event.tool_name());
        // The settling post of an MCP call itself (T22B) carries a due plate only for a
        // host whose replies can never carry one. A reply-capable host already had the
        // first shot on this same call's terminal reply, so its settled post stays a
        // silent carrier: everything still due reaches the next native post or reply
        // exactly once (T29B §5).
        let settled_plate = hook_post && !event.host().feed_delivery().allows_replies();
        // Waiting or reading between `ide.*` calls must not discard a result the agent has
        // not retrieved yet; only a possible writer advances the native epoch.
        let advances_epoch = may_write(event.host(), event.tool_name());
        // Kept for the journal line that names what advanced a native epoch.
        let hint_cause = format!(
            "native_hint phase={:?} tool={}",
            event.phase(),
            event.tool_name().unwrap_or("-")
        );
        let status = {
            // Recorded under both locks (map, then guard) together with the observation
            // itself, so no concurrent prune can drop a record whose pre is about to be
            // pending: a pending managed Claude pre always has its attachment record.
            let mut recorded = self
                .pre_attachments
                .lock()
                .or_cause(HostBindingCause::InternalLock)?;
            let mut bindings = self
                .bindings
                .lock()
                .or_cause(HostBindingCause::InternalLock)?;
            if self.shared_claude_channel
                && event.host() == HostKind::Claude
                && event.phase() == HookPhase::Pre
                && let Some(call) = &call_id
            {
                if recorded.len() >= PRE_ATTACHMENT_PRUNE {
                    recorded
                        .retain(|(actor, call), _| bindings.has_claude_pre(actor, call, &channel));
                }
                recorded.insert(
                    (event.actor_id().to_owned(), call.clone()),
                    hook.opaque_attachment().to_owned(),
                );
            }
            bindings.observe_hook(event.clone(), channel.clone())
        };
        match status {
            BindingStatus::PreObserved => Ok(PeerReply::HookObserved {}),
            BindingStatus::Settled(binding) => {
                // Settlement itself stays silent and never rechecks; but the settled MCP
                // call may just have completed an activation or a check, so a due status
                // plate is still delivered on this post phase (T22B) for hosts whose
                // replies never carry one.
                if settled_plate
                    && let Some(worker) = &self.worker
                    && let Some(block) = with_git_notice(
                        worker,
                        &binding.binding_ref().fingerprint(),
                        due_plate(
                            worker.project_feed(),
                            Some(worker),
                            &binding.binding_ref().fingerprint(),
                            None,
                        )
                        .await,
                        |plate| plate.len() <= super::reply::MAX_FEEDBACK_BYTES,
                    )
                    .await
                {
                    return Ok(PeerReply::Feedback { text: block });
                }
                Ok(PeerReply::HookSettled {})
            }
            BindingStatus::NativeObserved(binding) => {
                if let Some(worker) = &self.worker {
                    // A channel with no activation has no cached result a hint could
                    // invalidate; emitting one per native tool call is pure noise (0.6.5).
                    if advances_epoch && worker.channel_activated(&binding) {
                        worker.native_hint(binding.clone());
                        errorlog::record(
                            errorlog::Method::Hook,
                            errorlog::Outcome::Completed,
                            errorlog::Fields {
                                correlation: call_id.as_deref(),
                                detail: Some(&hint_cause),
                                ..Default::default()
                            },
                        );
                    }
                    let fingerprint = binding.fingerprint();
                    let feed = worker.project_feed().filter(|_| hook_post);
                    if triggers_check && let Some(feed) = feed {
                        // A writer tool that named its file makes that file's language
                        // the forced one; `Bash` and tools without a path keep every
                        // language on the same footing.
                        feed.changed_file(
                            &fingerprint,
                            object.get("tool_file").and_then(|value| value.as_str()),
                        );
                    }
                    let feedback = if feed.is_some_and(|feed| feed.is_read_restricted(&fingerprint))
                    {
                        None
                    } else {
                        worker.take_current_feedback(binding).await
                    };
                    // The block is taken from in-memory snapshots only. It is skipped, and
                    // stays due for a later hook, whenever it could not fit beside the
                    // feedback inside one bounded hook context.
                    let reserved = feedback.as_ref().map_or(0, |text| text.len() + 1);
                    let block = with_git_notice(
                        worker,
                        &fingerprint,
                        due_plate(feed, Some(worker), &fingerprint, feedback.as_deref()).await,
                        |plate| reserved + plate.len() <= super::reply::MAX_FEEDBACK_BYTES,
                    )
                    .await;
                    let text = match (block, feedback) {
                        (Some(block), Some(feedback)) => Some(format!("{block}\n{feedback}")),
                        (block, feedback) => block.or(feedback),
                    };
                    if let Some(text) = text {
                        return Ok(PeerReply::Feedback { text });
                    }
                }
                Ok(PeerReply::NativeHookObserved {})
            }
            _ => {
                // Refused: the cause names why, and `log_refused_hook` journals it once
                // (a per-call warn on a channel that already holds a binding).
                Err(match status {
                    BindingStatus::Unavailable(reason) => binding_cause(reason),
                    _ => HostBindingCause::Mismatch,
                })
            }
        }
    }

    /// Journals one refused hook (QW-4) as a per-call `hook` warn when its channel already holds
    /// a binding, because the call it belongs to is then refused with `missing_pre` and nothing
    /// else would say why. Every refusal exit of [`Self::handle_hook`] is covered here, once:
    /// a hook of a channel that never bound anything stays rate-limited bookkeeping, and a
    /// hook whose channel cannot be resolved (unknown attachment) has no channel to be active.
    ///
    /// The line carries the closed refusal cause, the host, the version and the call's opaque id;
    /// the hook payload itself is never read here.
    fn log_refused_hook(&self, hook: &crate::app::transport::HookSubmit, cause: &HostBindingCause) {
        let Some(channel) = self.channel(hook.opaque_attachment()) else {
            return;
        };
        let Some(bound) = self
            .bindings
            .lock()
            .ok()
            .map(|bindings| bindings.channel_bound(&channel))
        else {
            return;
        };
        let host = serde_json::from_str::<Value>(hook.sanitized_observation_json().as_str())
            .ok()
            .and_then(|object| match object.get("host")?.as_str()? {
                "codex" => Some(HostKind::Codex),
                "claude" => Some(HostKind::Claude),
                _ => None,
            });
        if !bound {
            if let Some(host) = host {
                log_hook_inactive(&self.hook_noise, host);
            }
            return;
        }
        errorlog::record(
            errorlog::Method::Hook,
            errorlog::Outcome::Unavailable,
            errorlog::Fields {
                host,
                correlation: Some(hook.correlation_id()),
                detail: Some(&format!("hook_refused:{}", cause.cause_tag())),
                version: Some(env!("CARGO_PKG_VERSION")),
                request: Some(hook.correlation_id()),
                ..Default::default()
            },
        );
    }

    /// Parses and commits one ingress request, additionally writing `tag` with the private actor
    /// tag a current managed Claude front receives beside the reply (see
    /// [`super::host_binding::actor_tag`]).
    ///
    /// Returns the typed reply, or `Err(cause)` when an ingress step refused before any reply
    /// could be built (malformed or oversized envelope, unknown host or phase, invalid
    /// parameters, an unknown attachment, a call id that differs from its correlation id, a
    /// poisoned daemon lock, a failed guard). `dispatch` answers `Err(cause)` as
    /// `unavailable: host_binding` carrying that closed cause, so no early exit is cause-less
    /// (QW-6). The cause is a closed enum value: nothing from the request is echoed back.
    ///
    /// `facts` is written, as far as the request got, with the closed journal context the ingress
    /// established (QW-4): the validated binding and its activation role.
    async fn handle_tagged(
        &self,
        request: &AssistanceDispatch,
        status: &mut Option<String>,
        tag: &mut Option<String>,
        facts: &mut CallFacts,
    ) -> Result<PeerReply, HostBindingCause> {
        match request {
            AssistanceDispatch::HookSubmit(hook) => {
                let result = self.handle_hook(hook).await;
                if let Err(cause) = &result {
                    self.log_refused_hook(hook, cause);
                }
                result
            }
            AssistanceDispatch::MethodDispatch(method) => {
                let envelope: Value = serde_json::from_str(method.params_json().as_str())
                    .or_cause(HostBindingCause::InvalidMetadata)?;
                let object = envelope
                    .as_object()
                    .or_cause(HostBindingCause::InvalidMetadata)?;
                if object.len() != 2 {
                    return Err(HostBindingCause::InvalidMetadata);
                }
                let meta = object
                    .get("host_meta")
                    .and_then(Value::as_object)
                    .or_cause(HostBindingCause::MissingField)?;
                let host = parse_host_kind(meta).map_err(binding_cause)?;
                let tool = match method.method() {
                    AssistanceMethod::Start => super::facade::AssistanceTool::Start,
                    AssistanceMethod::Context => super::facade::AssistanceTool::Context,
                    AssistanceMethod::Diff => super::facade::AssistanceTool::Diff,
                    AssistanceMethod::Inspect => super::facade::AssistanceTool::Inspect,
                    AssistanceMethod::Stop => super::facade::AssistanceTool::Stop,
                    AssistanceMethod::Edit => super::facade::AssistanceTool::Edit,
                    AssistanceMethod::Outline => super::facade::AssistanceTool::Outline,
                    AssistanceMethod::Read => super::facade::AssistanceTool::Read,
                    AssistanceMethod::Symbol => super::facade::AssistanceTool::Symbol,
                    AssistanceMethod::Graph => super::facade::AssistanceTool::Graph,
                    AssistanceMethod::Test => super::facade::AssistanceTool::Test,
                    AssistanceMethod::HookSubmit => return Err(HostBindingCause::InvalidMetadata),
                };
                let call = super::facade::validate_call(
                    tool,
                    object
                        .get("parameters")
                        .or_cause(HostBindingCause::InvalidParameters)?
                        .clone(),
                )
                .or_cause(HostBindingCause::InvalidParameters)?;
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(method.opaque_attachment()))
                {
                    // This daemon never registered the calling attachment, so it has also never
                    // received a hook on its channel (T15B).
                    return Ok(PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: Some(HostBindingCause::HooksNotDelivered),
                    });
                }
                let channel = self
                    .channel(method.opaque_attachment())
                    .or_cause(HostBindingCause::InvalidAttachment)?;
                // Trusted re-activation ingress (T15B restart recovery): the managed Claude MCP
                // marks the start that re-runs a remembered activation after the daemon it had
                // activated on was replaced. The marker rides host metadata, never model
                // arguments, and only the managed shared daemon accepts it. A current front names
                // the exact call it is about to dispatch, whose pre's actor is the one to bind; a
                // 0.10.2 front sends `true` and binds any actor whose pre arrived through its own
                // attachment.
                let reactivation = host == HostKind::Claude
                    && method.method() == AssistanceMethod::Start
                    && self.managed_claude
                    && meta.contains_key("claudecode/reactivation");
                let reactivation_call = meta
                    .get("claudecode/reactivation")
                    .and_then(Value::as_str)
                    .filter(|_| reactivation && self.shared_claude_channel);
                // A merely-late pre-hook must not be recorded as MCP-before-pre replay
                // evidence: the host fires each pre exactly once, and a daemon busy with a
                // sibling call can observe its submission hundreds of milliseconds late
                // (~405 ms in the T15B evidence, against the hook's own 250 ms deadline).
                // Give an absent Claude pre a bounded arrival window before the guard op; a
                // re-activation start instead waits for the pre whose actor it takes without
                // consuming it.
                // A front that names no expected actor recovers its anonymous 0.10.2 slot: the
                // named call's pre must then have come through that front's own attachment.
                let anonymous = !meta.contains_key("claudecode/actor");
                let reactivation_actor = || match reactivation_call {
                    Some(call) => {
                        let actor = self
                            .bindings
                            .lock()
                            .ok()?
                            .claude_pre_actor(call, &channel)?;
                        (!anonymous
                            || self.pre_attachment(&actor, call).as_deref()
                                == Some(method.opaque_attachment()))
                        .then(|| (actor, call.to_owned()))
                    }
                    None => self.legacy_reactivation_actor(method.opaque_attachment(), &channel),
                };
                // Private managed-Claude ingress, never model arguments: `claudecode/recover`
                // announces a current front and, after a daemon replacement, the tags of the
                // actors it remembers activations for; `claudecode/whois` asks for the tag of one
                // real call's pending pre without touching that pre.
                let managed_claude = host == HostKind::Claude && self.shared_claude_channel;
                let recover: Option<Vec<&str>> = match managed_claude
                    .then(|| meta.get("claudecode/recover"))
                    .flatten()
                {
                    None => None,
                    Some(tags) => match actor_tags(tags) {
                        Some(tags) => Some(tags),
                        None => {
                            return Ok(PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause: Some(HostBindingCause::InvalidMetadata),
                            });
                        }
                    },
                };
                let recovering = recover.as_ref().is_some_and(|tags| !tags.is_empty());
                let whois = managed_claude
                    .then(|| meta.get("claudecode/whois")?.as_str())
                    .flatten();
                let evidence = || {
                    if reactivation {
                        reactivation_actor().is_some()
                    } else {
                        self.bindings.lock().is_ok_and(|bindings| {
                            bindings.has_pre(whois.unwrap_or(method.correlation_id()), &channel)
                        })
                    }
                };
                if host == HostKind::Claude && !evidence() {
                    // After a replacement the first call of an actor may be the one that restores
                    // it, so it gets the same arrival window as the start it stands in for.
                    let wait = if recovering {
                        pre_arrival_wait(AssistanceMethod::Start)
                    } else {
                        pre_arrival_wait(method.method())
                    };
                    let deadline = tokio::time::Instant::now() + wait;
                    while tokio::time::Instant::now() < deadline {
                        tokio::time::sleep(PRE_ARRIVAL_POLL).await;
                        if evidence() {
                            break;
                        }
                    }
                }
                if let Some(real) = whois {
                    let owner = self
                        .bindings
                        .lock()
                        .or_cause(HostBindingCause::InternalLock)?
                        .claude_pre_owner(real, &channel);
                    *tag = Some(
                        owner
                            .as_deref()
                            .map(super::host_binding::actor_tag)
                            .unwrap_or_default(),
                    );
                    return Ok(PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: owner.err().and_then(HostBindingCause::from_binding),
                    });
                }
                // A remembered actor of a replaced daemon is restored by its front before this
                // call consumes its pre: answer without touching the guard, naming the actor.
                if recovering
                    && !matches!(
                        method.method(),
                        AssistanceMethod::Start | AssistanceMethod::Stop
                    )
                {
                    let bindings = self
                        .bindings
                        .lock()
                        .or_cause(HostBindingCause::InternalLock)?;
                    if let Some(actor) =
                        bindings.claude_pre_actor(method.correlation_id(), &channel)
                        && bindings.claude_never_bound(&actor, &channel)
                        && let actor = super::host_binding::actor_tag(&actor)
                        && recover
                            .as_ref()
                            .is_some_and(|tags| tags.contains(&actor.as_str()))
                    {
                        *tag = Some(actor);
                        return Ok(PeerReply::Unavailable {
                            reason: MissingPeer::HostBinding,
                            cause: Some(HostBindingCause::RecoveryNeeded),
                        });
                    }
                }
                let reactivation_actor = reactivation.then(reactivation_actor).flatten();
                // A front that already named the actor (a stop after its identity query, a
                // re-activation after `recovery_needed`) carries the expected tag; a pending pre
                // that now belongs to anyone else is refused with nothing consumed. A
                // re-activation binds exactly the selected actor, whose pre the guard re-checks;
                // every other call re-checks under the guard lock that validates it.
                let expected = managed_claude
                    .then(|| meta.get("claudecode/actor")?.as_str())
                    .flatten();
                let mismatch = PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(HostBindingCause::Mismatch),
                };
                if let Some(expected) = expected
                    && reactivation
                    && reactivation_actor
                        .as_ref()
                        .map(|(actor, _)| super::host_binding::actor_tag(actor))
                        .as_deref()
                        != Some(expected)
                {
                    return Ok(mismatch);
                }
                // A start activates the worktree its own pre arrived through (managed Claude);
                // every other call, and every other daemon, keeps the calling attachment. The
                // target is resolved before any guard transition, so a vanished worktree refuses
                // with nothing consumed.
                let start_actor = match (&reactivation_actor, reactivation) {
                    (Some((actor, _)), _) => Some(actor.clone()),
                    (None, false)
                        if self.shared_claude_channel
                            && method.method() == AssistanceMethod::Start =>
                    {
                        self.bindings
                            .lock()
                            .or_cause(HostBindingCause::InternalLock)?
                            .claude_pre_actor(method.correlation_id(), &channel)
                    }
                    _ => None,
                };
                // The guard transition below re-checks that this actor still owns the call's
                // pending pre, so the target chosen for it is never applied to another actor.
                let fenced_start_actor = start_actor.clone().filter(|_| !reactivation);
                // A current front remembers a start under its actor's tag.
                if recover.is_some() && !reactivation {
                    *tag = start_actor.as_deref().map(super::host_binding::actor_tag);
                }
                // A resolved start actor without its pre's record fails closed rather than
                // activating the calling (home) worktree; a 0.10.2 re-activation's actor was
                // already scoped to the calling attachment, which therefore is its pre's.
                let target_attachment = match start_actor {
                    Some(_) if reactivation_call.is_none() && reactivation => {
                        Some(method.opaque_attachment().to_owned())
                    }
                    Some(actor) => self.pre_attachment(
                        &actor,
                        reactivation_call.unwrap_or(method.correlation_id()),
                    ),
                    None => Some(method.opaque_attachment().to_owned()),
                };
                let Some(target_attachment) = target_attachment.filter(|attachment| {
                    self.worker
                        .as_ref()
                        .is_none_or(|worker| worker.target(attachment).is_some())
                }) else {
                    return Ok(PeerReply::Error {
                        code: super::reply::FailureCode::LauncherConfiguration,
                        detail: None,
                    });
                };
                // A test-run handle read by `ide.inspect` only answers that run's status, already
                // owned by this actor and channel, so it stays readable after `ide.stop`; every
                // other call still needs the active generation.
                let read_only = method.method() == AssistanceMethod::Inspect
                    && call.parameters()["detail_ref"]
                        .as_str()
                        .and_then(super::worker::test_run_handle)
                        .is_some();
                let stopped = |invocation: &super::host_binding::ValidatedInvocation| {
                    read_only
                        && invocation.binding_ref() == &invocation.binding_ref().channel_identity()
                };
                let invocation = {
                    let mut bindings = self
                        .bindings
                        .lock()
                        .or_cause(HostBindingCause::InternalLock)?;
                    if let Some(expected) = expected
                        && !reactivation
                        && bindings
                            .claude_pre_actor(method.correlation_id(), &channel)
                            .as_deref()
                            .map(super::host_binding::actor_tag)
                            .as_deref()
                            != Some(expected)
                    {
                        return Ok(mismatch);
                    }
                    // This exact call's own genuine pre is hook evidence too, even once the
                    // guard below consumes it: a refusal then names the missing binding, never
                    // undelivered hooks.
                    let own_pre = host == HostKind::Claude
                        && bindings.has_pre(method.correlation_id(), &channel);
                    if managed_claude && method.method() == AssistanceMethod::Stop {
                        bindings.fence_claude_stop(method.correlation_id(), &channel);
                    }
                    if let Some(actor) = &fenced_start_actor
                        && bindings
                            .claude_pre_actor(method.correlation_id(), &channel)
                            .as_ref()
                            != Some(actor)
                    {
                        return Ok(mismatch);
                    }
                    let status = match host {
                        HostKind::Codex => {
                            let candidate = parse_candidate(meta).map_err(binding_cause)?;
                            if candidate.call_id() != method.correlation_id() {
                                return Err(HostBindingCause::Mismatch);
                            }
                            if self.managed_codex && method.method() == AssistanceMethod::Start {
                                bindings.establish_managed_codex_start(candidate, channel.clone())
                            } else if self.managed_codex && read_only {
                                bindings
                                    .validate_managed_codex_read_only(candidate, channel.clone())
                            } else if self.managed_codex {
                                bindings.validate_managed_codex_active(candidate, channel.clone())
                            } else if method.method() == AssistanceMethod::Start {
                                bindings.establish_start(candidate, channel.clone())
                            } else if read_only {
                                bindings.validate_read_only(candidate, channel.clone())
                            } else {
                                bindings.validate_active(candidate, channel.clone())
                            }
                        }
                        HostKind::Claude => {
                            let call_id = parse_claude_call_id(meta).map_err(binding_cause)?;
                            if call_id != method.correlation_id() {
                                return Err(HostBindingCause::Mismatch);
                            }
                            if method.method() == AssistanceMethod::Start && reactivation {
                                match reactivation_actor.clone() {
                                    Some((actor, pre_call)) => bindings.reactivate_start_claude(
                                        &call_id,
                                        channel.clone(),
                                        actor,
                                        &pre_call,
                                    ),
                                    None => BindingStatus::Unavailable(
                                        super::host_binding::BindingUnavailable::MissingPre,
                                    ),
                                }
                            } else if method.method() == AssistanceMethod::Start {
                                bindings.establish_start_claude(&call_id, channel.clone())
                            } else if read_only {
                                bindings.validate_read_only_claude(&call_id, channel.clone())
                            } else {
                                bindings.validate_active_claude(&call_id, channel.clone())
                            }
                        }
                    };
                    let BindingStatus::Validated(invocation) = status else {
                        if let BindingStatus::Unavailable(reason) = status {
                            log_binding_unavailable(tool, host, method.correlation_id(), reason);
                            // Channel activity is read under the guard; the root admission probe
                            // below touches the filesystem only after the lock is released.
                            let hooks_delivered =
                                own_pre || bindings.channel_observed_hook(&channel);
                            drop(bindings);
                            return Ok(PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause: host_binding_cause(
                                    self.worker.as_ref(),
                                    hooks_delivered,
                                    method.opaque_attachment(),
                                    reason,
                                ),
                            });
                        }
                        return Err(HostBindingCause::Mismatch);
                    };
                    if method.method() == AssistanceMethod::Stop {
                        bindings
                            .stop_binding(invocation.binding_ref())
                            .map_err(binding_cause)?;
                    } else if !stopped(&invocation) {
                        bindings
                            .consume_active(invocation.binding_ref())
                            .map_err(binding_cause)?;
                    }
                    invocation
                };
                // Only a read-only call answers a stopped generation: its reply stays plate-free
                // and touches no worktree state, like `ide.stop`'s own.
                let stopped = stopped(&invocation);
                facts.binding = Some(invocation.binding_ref().clone());
                facts.role = self
                    .worker
                    .as_ref()
                    .and_then(|worker| worker.role_of(invocation.binding_ref()));
                let Some(worker) = &self.worker else {
                    return Ok(if method.method() == AssistanceMethod::Stop {
                        PeerReply::HostStopped {}
                    } else {
                        PeerReply::Unavailable {
                            reason: MissingPeer::WorkspaceActivation,
                            cause: Some(HostBindingCause::WorkerUnavailable),
                        }
                    });
                };
                if self.managed_codex
                    && !stopped
                    && matches!(
                        method.method(),
                        AssistanceMethod::Context
                            | AssistanceMethod::Diff
                            | AssistanceMethod::Inspect
                    )
                    && !is_problems_context(method.method(), call.parameters())
                {
                    // Managed Codex has no native hook stream. Treat every read boundary as a
                    // possible native edit and reuse the worker's registered-path reconciliation
                    // and stale-detail fencing instead of adding a watcher or trusting tool args.
                    // A Context/Diff call carrying a detail reference is retrieval of its existing
                    // capture, not a new read boundary, so it must not invalidate itself first.
                    worker.managed_read_boundary(
                        invocation.binding_ref().clone(),
                        method.method() != AssistanceMethod::Inspect
                            && call.parameters().get("detail_ref").is_none(),
                    );
                }
                let fingerprint = invocation.binding_ref().fingerprint();
                let mut reply = match method.method() {
                    AssistanceMethod::Stop => {
                        worker.stop(invocation, method.opaque_attachment()).await
                    }
                    AssistanceMethod::Inspect => {
                        let inspection = worker.inspect(
                            invocation.binding_ref().clone(),
                            call.parameters()["detail_ref"]
                                .as_str()
                                .or_cause(HostBindingCause::InvalidParameters)?
                                .to_owned(),
                            method.opaque_attachment(),
                            None,
                        );
                        worker
                            .with_delivery(facts.delivery.clone(), inspection)
                            .await
                    }
                    AssistanceMethod::Context
                        if is_problems_context(method.method(), call.parameters()) =>
                    {
                        worker
                            .context_problems(
                                invocation,
                                call.parameters().clone(),
                                method.opaque_attachment(),
                            )
                            .await
                    }
                    _ => {
                        let submission = worker.submit(
                            invocation,
                            tool,
                            call.parameters().clone(),
                            &target_attachment,
                            Some(method.correlation_id()),
                        );
                        worker
                            .with_delivery(facts.delivery.clone(), submission)
                            .await
                    }
                };
                // T28B/T29B: reply-carrying hosts lead every terminal `ide.*` reply with the due
                // plate. Every such call first reconciles the bound worktree's inputs (free while
                // unchanged, T20B) — for managed Codex this stays even though hooks may now
                // deliver natively, because installed hooks can be untrusted or stop running and
                // the reply path must keep working unchanged. `ide.stop` ends the binding and
                // stays plate-free, and a `pending` placeholder is not terminal — the plate goes
                // with the answer that resolves it. The feed deduplicates by binding fingerprint,
                // so a plate the native post already delivered is never repeated here.
                if host.feed_delivery().allows_replies()
                    && method.method() != AssistanceMethod::Stop
                    && !stopped
                {
                    let _ = worker.git_notice(&fingerprint).await;
                    let test_status = test_status_snapshot(&reply);
                    if let Some(feed) = worker.project_feed() {
                        feed.changed(&fingerprint);
                    }
                    if !matches!(reply, PeerReply::Pending { .. }) {
                        *status = match worker.project_feed() {
                            Some(feed) => attach_reply_plate(
                                Some(feed),
                                &fingerprint,
                                &mut reply,
                                test_status.as_deref(),
                            ),
                            None => attach_test_plate(
                                worker,
                                &fingerprint,
                                &mut reply,
                                test_status.as_deref(),
                            ),
                        };
                        // The facade decodes a carried plate up to the hook feedback ceiling.
                        *status = with_git_notice(worker, &fingerprint, status.take(), |plate| {
                            plate.len() <= super::reply::MAX_FEEDBACK_BYTES
                                && super::content::fits_with_status(
                                    &reply,
                                    plate,
                                    super::content::Envelope::WithStructured,
                                )
                        })
                        .await;
                    }
                }
                Ok(reply)
            }
        }
    }
}
impl AssistanceDispatcher for ProductDispatcher {
    /// Gives a managed Claude MCP its own target on this repository's shared daemon.
    fn register_claude_candidate(&self, candidate: &Path) -> Option<String> {
        if !self.managed_claude {
            return None;
        }
        self.worker.as_ref()?.register_claude_candidate(candidate)
    }
    /// Opens configured peers once, only after Application owns the daemon endpoint lock.
    fn initialize<'a>(
        &'a self,
        runtime_dir: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>> {
        Box::pin(async move {
            match &self.worker {
                Some(worker) => worker
                    .start(runtime_dir)
                    .await
                    .map_err(|_| AssistanceDispatchUnavailable),
                None => Ok(()),
            }
        })
    }
    /// Cancels all queued and active work and waits for the worker to reap owned providers.
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + '_>> {
        Box::pin(async move {
            match &self.worker {
                Some(worker) => {
                    // Checks are cancelled first so no confined process outlives the worker.
                    if let Some(feed) = worker.project_feed() {
                        feed.shutdown().await;
                    }
                    worker
                        .shutdown()
                        .await
                        .map_err(|_| AssistanceDispatchUnavailable)
                }
                None => Ok(()),
            }
        })
    }
    /// Reports pending/running assistance work (a queued or executing job, or a project check),
    /// which keeps the idle daemon alive until the work reaches its terminal state (T26B).
    fn is_busy(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(WorkerHandle::is_processing)
            || self
                .worker
                .as_ref()
                .and_then(WorkerHandle::project_feed)
                .is_some_and(|feed| feed.is_busy())
    }
    /// Reports whether the worker's execution machinery failed (a caught job panic, or the worker
    /// or inspection task ended outside shutdown), so Application answers `restarting` and exits.
    fn is_failed(&self) -> bool {
        self.worker.as_ref().is_some_and(WorkerHandle::is_failed)
    }
    /// Resolves when [`Self::is_failed`] turns true; never for a discovery-only dispatcher.
    fn failed(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            match &self.worker {
                Some(worker) => worker.failed().await,
                None => std::future::pending().await,
            }
        })
    }
    /// Returns bounded closed outcomes; slow jobs become pending while short inspections stay finite.
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let started = std::time::Instant::now();
            let mut status = None;
            let mut tag = None;
            let mut facts = CallFacts::default();
            let mut result = self
                .handle_tagged(&request, &mut status, &mut tag, &mut facts)
                .await
                .unwrap_or_else(|cause| PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(cause),
                });
            // Hook payloads are intentionally never accepted by telemetry adapters or the log.
            if let AssistanceDispatch::MethodDispatch(method) = &request {
                let tool = match method.method() {
                    AssistanceMethod::Start => Some(super::facade::AssistanceTool::Start),
                    AssistanceMethod::Context => Some(super::facade::AssistanceTool::Context),
                    AssistanceMethod::Diff => Some(super::facade::AssistanceTool::Diff),
                    AssistanceMethod::Inspect => Some(super::facade::AssistanceTool::Inspect),
                    AssistanceMethod::Stop => Some(super::facade::AssistanceTool::Stop),
                    AssistanceMethod::Edit => Some(super::facade::AssistanceTool::Edit),
                    AssistanceMethod::Outline => Some(super::facade::AssistanceTool::Outline),
                    AssistanceMethod::Read => Some(super::facade::AssistanceTool::Read),
                    AssistanceMethod::Symbol => Some(super::facade::AssistanceTool::Symbol),
                    AssistanceMethod::Graph => Some(super::facade::AssistanceTool::Graph),
                    AssistanceMethod::Test => Some(super::facade::AssistanceTool::Test),
                    AssistanceMethod::HookSubmit => None,
                };
                if let Some(tool) = tool {
                    let envelope =
                        serde_json::from_str::<Value>(method.params_json().as_str()).ok();
                    let parameters = envelope
                        .as_ref()
                        .and_then(|envelope| envelope.get("parameters").cloned());
                    let host = envelope
                        .as_ref()
                        .and_then(|envelope| envelope.get("host_meta")?.as_object())
                        .and_then(|meta| parse_host_kind(meta).ok());
                    let requested = parameters.as_ref().and_then(|parameters| {
                        parameters
                            .get("detail_ref")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    });
                    // A provider refusal with no more specific stage derives its default here, so
                    // the daemon journal and the model-facing reply name the same `ext=` file type.
                    if let (Some(parameters), PeerReply::Error { code, detail }) =
                        (&parameters, &mut result)
                        && detail.is_none()
                    {
                        *detail = Some(super::facade::staged_detail(tool, code, parameters));
                    }
                    // The role is read before the call ran; a start has none yet, so it reads the
                    // role the activation just took.
                    let role = facts.role.or_else(|| {
                        let (worker, binding) = (self.worker.as_ref()?, facts.binding.as_ref()?);
                        worker.role_of(binding)
                    });
                    // An inspection names the call that queued the result it asks for, and a
                    // finished answer takes the degraded mark its job left for this call (a
                    // `pending` answer leaves it for the job's completion record).
                    let origin = requested
                        .as_deref()
                        .and_then(|reference| self.worker.as_ref()?.request_of(reference));
                    let degraded = !matches!(result, PeerReply::Pending { .. })
                        && self.worker.as_ref().is_some_and(|worker| {
                            worker.take_degraded(method.correlation_id())
                                || requested
                                    .as_deref()
                                    .is_some_and(|reference| worker.reference_degraded(reference))
                        });
                    // The inspection this very call performed says whether it delivered a retained
                    // terminal result (a flag of this call alone); only a call that retrieves a
                    // result by reference carries it.
                    let delivered = requested
                        .as_ref()
                        .map(|_| facts.delivery.load(std::sync::atomic::Ordering::Acquire));
                    // The front's actor query is the product's own probe, not an agent's call.
                    let probe = envelope
                        .as_ref()
                        .and_then(|envelope| envelope.get("host_meta")?.get("claudecode/whois"))
                        .map(|_| "whois");
                    adapters::log_tool_reply(
                        tool,
                        &result,
                        started.elapsed(),
                        requested.as_deref(),
                        &adapters::DispatchContext {
                            host,
                            role,
                            request: Some(method.correlation_id()),
                            origin: origin.as_deref(),
                            degraded,
                            delivered,
                            probe,
                            parameters: parameters.as_ref(),
                        },
                    );
                    if let Some(telemetry) = self.worker.as_ref().and_then(WorkerHandle::telemetry)
                    {
                        adapters::tool_reply(
                            &telemetry,
                            tool,
                            &result,
                            started.elapsed(),
                            None,
                            CacheState::NotApplicable,
                            DiagnosticState::NotApplicable,
                        );
                    }
                }
            }
            // A carried status plate (T28B) rides on top of the closed reply in the wrapped wire
            // form; without one the encoding stays byte-identical to earlier releases.
            let reply = match &status {
                Some(plate) => PeerReply::encode_with_status(&result, plate),
                None => result.encode(),
            }
            .ok_or(AssistanceDispatchUnavailable)?;
            // Only a front that sent `claudecode/recover` receives a tag, so older fronts keep
            // decoding byte-identical replies.
            // An empty tag (an identity query whose call no actor owns) is sent as `null`: the
            // wrapper itself tells the front that this daemon answers identity queries.
            let reply = match tag {
                Some(tag) => OpaqueJson::from_value(
                    &json!({"actor": (!tag.is_empty()).then_some(tag), "reply": serde_json::from_str::<Value>(reply.as_str()).ok()}),
                    crate::app::MAX_ASSISTANCE_JSON_BYTES,
                )
                .ok_or(AssistanceDispatchUnavailable)?,
                None => reply,
            };
            Ok(match request {
                AssistanceDispatch::HookSubmit(_) => AssistanceDispatchReply::HookSubmit(reply),
                AssistanceDispatch::MethodDispatch(_) => {
                    AssistanceDispatchReply::MethodDispatch(reply)
                }
            })
        })
    }
}

/// Rejects arbitrary daemon state or extra result fields instead of manufacturing peer readiness.
#[test]
fn peer_reply_accepts_only_the_closed_host_shapes() {
    for reply in [
        r#"{"state":"ready"}"#,
        r#"{"state":"unavailable","reason":"unknown"}"#,
        r#"{"state":"host_stopped","source":"forged"}"#,
    ] {
        assert!(serde_json::from_str::<PeerReply>(reply).is_err());
    }
}

/// Same attachment is stable within a daemon but cannot recreate a prior boot binding fingerprint.
#[test]
fn daemon_scope_is_fresh_without_actor_or_timing_inference() {
    let first = ProductDispatcher::default();
    let second = ProductDispatcher::default();
    assert_eq!(
        first.channel("same").unwrap(),
        first.channel("same").unwrap()
    );
    assert_ne!(
        first.channel("same").unwrap(),
        second.channel("same").unwrap()
    );
    assert!(!format!("{first:?}").contains("scope"));
}

/// A managed shared Claude daemon pairs every admitted worktree attachment on one channel, so an
/// actor's call never depends on which worktree's attachment carried it; other daemons keep one
/// channel per attachment.
#[test]
fn managed_claude_attachments_share_one_channel() {
    let claude = ProductDispatcher {
        managed_claude: true,
        shared_claude_channel: true,
        ..ProductDispatcher::default()
    };
    assert_eq!(
        claude.channel("home").unwrap(),
        claude.channel("sibling").unwrap()
    );
    let plain = ProductDispatcher::default();
    assert_ne!(
        plain.channel("home").unwrap(),
        plain.channel("sibling").unwrap()
    );
    let other = ProductDispatcher {
        managed_claude: true,
        shared_claude_channel: true,
        ..ProductDispatcher::default()
    };
    assert_ne!(
        claude.channel("home").unwrap(),
        other.channel("home").unwrap()
    );
}

/// Uses host-shaped daemon frames to reject mixed Codex and Claude metadata before correlation.
#[tokio::test]
async fn host_shaped_mixed_metadata_is_unavailable_at_daemon_ingress() {
    use crate::app::transport::{MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let parameters = OpaqueJson::from_value(
        &json!({
            "parameters": {"activation_id":"activate"},
            "host_meta": {
                "threadId":"actor",
                "callId":"call",
                "x-codex-turn-metadata":{},
                "claudecode/toolUseId":"call"
            }
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let request = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "call",
            "attachment",
            AssistanceMethod::Start,
            parameters,
        )
        .expect("test dispatch is valid"),
    );
    assert_eq!(dispatcher.handle(&request, &mut None).await, None);
}

/// QW-6: no early exit of the dispatcher answers a bare `unavailable: host_binding`; each names
/// its closed cause (malformed envelope, invalid parameters, unknown hook phase, mismatched call).
#[tokio::test]
async fn ingress_exits_name_a_typed_cause() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let method = |params: Value, name: AssistanceMethod| {
        AssistanceDispatch::MethodDispatch(
            MethodDispatch::new(
                "request",
                "call",
                "attachment",
                name,
                OpaqueJson::from_value(&params, 64 * 1024).unwrap(),
            )
            .unwrap(),
        )
    };
    // The serialized reply the front receives from `dispatch`, decoded: a refused ingress is
    // `unavailable: host_binding`, and the test fails on a bare one (no cause).
    let cause_of = |request: AssistanceDispatch| {
        let dispatcher = &dispatcher;
        async move {
            let reply = match dispatcher.dispatch(request).await.unwrap() {
                AssistanceDispatchReply::HookSubmit(reply)
                | AssistanceDispatchReply::MethodDispatch(reply) => reply,
            };
            match PeerReply::decode(reply.as_str()).unwrap() {
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(cause),
                } => cause,
                other => panic!("expected a typed host_binding refusal, got {other:?}"),
            }
        }
    };
    // Envelope with a stray key: invalid metadata.
    assert_eq!(
        cause_of(method(
            json!({"parameters":{},"host_meta":{},"extra":1}),
            AssistanceMethod::Context
        ))
        .await,
        HostBindingCause::InvalidMetadata
    );
    // No `host_meta` object: a missing field.
    assert_eq!(
        cause_of(method(
            json!({"parameters":{},"x":1}),
            AssistanceMethod::Context
        ))
        .await,
        HostBindingCause::MissingField
    );
    // A hook with an unsupported phase.
    let hook = |phase: &str| {
        AssistanceDispatch::HookSubmit(
            HookSubmit::new(
                "request",
                "call",
                "attachment",
                OpaqueJson::from_value(
                    &json!({"host":"claude","phase":phase,"actor_id":"s","call_id":"call","session_id":"s","agent_type":null}),
                    64 * 1024,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    };
    assert_eq!(
        cause_of(hook("bogus")).await,
        HostBindingCause::UnsupportedHookPhase
    );
    // A daemon lock poisoned by an earlier fault: a valid Claude pre can no longer be recorded.
    let poisoned = ProductDispatcher {
        managed_claude: true,
        shared_claude_channel: true,
        ..ProductDispatcher::default()
    };
    let _ = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _guard = poisoned.pre_attachments.lock().unwrap();
                panic!("poison the pre-attachment map");
            })
            .join()
    });
    let reply = match poisoned.dispatch(hook("pre")).await.unwrap() {
        AssistanceDispatchReply::HookSubmit(reply)
        | AssistanceDispatchReply::MethodDispatch(reply) => {
            PeerReply::decode(reply.as_str()).unwrap()
        }
    };
    assert_eq!(
        reply,
        PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause: Some(HostBindingCause::InternalLock),
        }
    );
    // Valid envelope, but parameters the daemon's validator rejects: invalid parameters.
    assert_eq!(
        cause_of(method(
            json!({"parameters":{"nope":1},"host_meta":{"claudecode/toolUseId":"call"}}),
            AssistanceMethod::Read
        ))
        .await,
        HostBindingCause::InvalidParameters
    );
}

/// An anonymous (0.10.2-slot) re-activation names the real call and binds its actor only when
/// that call's pre came through the calling front's own attachment: a subagent's pending start
/// that arrived through another attachment never lends its actor to the slot.
#[tokio::test]
async fn anonymous_reactivation_binds_only_its_own_attachments_named_call() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher {
        managed_claude: true,
        shared_claude_channel: true,
        ..ProductDispatcher::default()
    };
    for (actor, call, attachment) in [
        ("child", "child-start", "nested"),
        ("parent", "parent-read", "home"),
    ] {
        let observation = OpaqueJson::from_value(
            &json!({"host":"claude","phase":"pre","actor_id":actor,"call_id":call,"session_id":"session","agent_type":null}),
            64 * 1024,
        )
        .unwrap();
        let hook = AssistanceDispatch::HookSubmit(
            HookSubmit::new("request", call, attachment, observation).unwrap(),
        );
        assert_eq!(
            dispatcher.handle(&hook, &mut None).await,
            Some(PeerReply::HookObserved {})
        );
    }
    let reactivate = |named: &str, synthetic: &str| {
        AssistanceDispatch::MethodDispatch(
            MethodDispatch::new(
                "request",
                synthetic,
                "home",
                AssistanceMethod::Start,
                OpaqueJson::from_value(
                    &json!({
                        "parameters":{"activation_id":"slot"},
                        "host_meta":{"claudecode/toolUseId":synthetic,"claudecode/reactivation":named}
                    }),
                    64 * 1024,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    };
    // The child's pre came through another attachment: nothing is bound for it.
    assert_eq!(
        dispatcher
            .handle(&reactivate("child-start", "synthetic-1"), &mut None)
            .await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause: Some(HostBindingCause::MissingPre),
        })
    );
    // The front's own call binds its own actor (no worker here, so activation stops after it).
    assert_eq!(
        dispatcher
            .handle(&reactivate("parent-read", "synthetic-2"), &mut None)
            .await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::WorkspaceActivation,
            cause: Some(HostBindingCause::WorkerUnavailable),
        })
    );
}

/// QW-4: a hook the daemon refuses on a channel that already holds a binding leaves one warn per
/// call with a closed reason and the call's opaque id, while the same refusal on a channel that
/// never bound anything stays bookkeeping (no per-call warn).
#[tokio::test]
async fn refused_hook_of_an_active_channel_leaves_a_per_call_warn() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let hook = |phase: &str, call: &str, attachment: &str| {
        AssistanceDispatch::HookSubmit(
            HookSubmit::new(
                "request-1",
                call,
                attachment,
                OpaqueJson::from_value(
                    &json!({"host":"claude","phase":phase,"actor_id":"session","call_id":call,"session_id":"session","agent_type":null}),
                    64 * 1024,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    };
    // A channel that never bound anything: refusals are bookkeeping, not warns.
    errorlog::capture_start();
    assert_eq!(
        dispatcher
            .handle(&hook("post", "ghost", "idle"), &mut None)
            .await,
        None
    );
    assert!(
        errorlog::capture_take()
            .iter()
            .all(|event| event.detail.as_deref() != Some("hook_refused:active_channel"))
    );

    // Bind the channel: a pre, then the start it paired.
    assert_eq!(
        dispatcher
            .handle(&hook("pre", "c1", "home"), &mut None)
            .await,
        Some(PeerReply::HookObserved {})
    );
    let start = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request-2",
            "c1",
            "home",
            AssistanceMethod::Start,
            OpaqueJson::from_value(
                &json!({"parameters":{},"host_meta":{"claudecode/toolUseId":"c1"}}),
                64 * 1024,
            )
            .unwrap(),
        )
        .unwrap(),
    );
    dispatcher.handle(&start, &mut None).await;
    // The same unmatched post on the now-bound channel is a per-call warn, and so is every
    // repeated refused pre (a payload call id that differs from the transport correlation id):
    // one warn per refusal, none rate limited, none doubled.
    let mismatched_pre = |payload_call: &str, transport_call: &str| {
        AssistanceDispatch::HookSubmit(
            HookSubmit::new(
                "request-1",
                transport_call,
                "home",
                OpaqueJson::from_value(
                    &json!({"host":"claude","phase":"pre","actor_id":"session","call_id":payload_call,"session_id":"session","agent_type":null}),
                    64 * 1024,
                )
                .unwrap(),
            )
            .unwrap(),
        )
    };
    errorlog::capture_start();
    assert_eq!(
        dispatcher
            .handle(&hook("post", "ghost", "home"), &mut None)
            .await,
        None
    );
    for transport_call in ["t-1", "t-2"] {
        assert_eq!(
            dispatcher
                .handle(&mismatched_pre("p-1", transport_call), &mut None)
                .await,
            None
        );
    }
    let events = errorlog::capture_take();
    let warns = events
        .iter()
        .filter(|event| event.method == "hook")
        .collect::<Vec<_>>();
    assert_eq!(warns.len(), 3, "one warn per refusal: {events:?}");
    assert!(warns.iter().all(|warn| warn.level == "warn"));
    assert_eq!(warns[0].correlation.as_deref(), Some("ghost"));
    assert_eq!(warns[0].request.as_deref(), Some("ghost"));
    assert_eq!(warns[0].detail.as_deref(), Some("hook_refused:mismatch"));
    assert_eq!(warns[1].detail.as_deref(), Some("hook_refused:mismatch"));
    assert_eq!(warns[2].detail.as_deref(), Some("hook_refused:mismatch"));
    assert_eq!(warns[1].correlation.as_deref(), Some("t-1"));
    assert_eq!(warns[2].correlation.as_deref(), Some("t-2"));
}

/// A correlated Claude request remains unavailable when this dispatcher has no configured worker.
#[tokio::test]
async fn claude_start_without_worker_is_unavailable_after_correlation() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"call",
            "session_id":"session",
            "agent_type":null
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "call", "attachment", observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&hook, &mut None).await,
        Some(PeerReply::HookObserved {})
    );

    let parameters = OpaqueJson::from_value(
        &json!({
            "parameters":{"activation_id":"activate"},
            "host_meta":{"claudecode/toolUseId":"call"}
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let method = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "call",
            "attachment",
            AssistanceMethod::Start,
            parameters,
        )
        .expect("test method dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&method, &mut None).await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::WorkspaceActivation,
            cause: Some(HostBindingCause::WorkerUnavailable)
        })
    );

    let next_observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"next",
            "session_id":"session",
            "agent_type":null
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let next_hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "next", "attachment", next_observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&next_hook, &mut None).await,
        Some(PeerReply::HookObserved {})
    );
    let next_parameters = OpaqueJson::from_value(
        &json!({
            "parameters":{"path":"tracked.rs"},
            "host_meta":{"claudecode/toolUseId":"next"}
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let next_method = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "next",
            "attachment",
            AssistanceMethod::Context,
            next_parameters,
        )
        .expect("test method dispatch is valid"),
    );
    // A follow-up Claude operation on the same attachment also has no worker to handle it.
    assert_eq!(
        dispatcher.handle(&next_method, &mut None).await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::WorkspaceActivation,
            cause: Some(HostBindingCause::WorkerUnavailable)
        })
    );
}

/// Builds a managed-Codex dispatcher with one established start binding for the read-boundary
/// short-circuit test, mirroring [`claude_race_fixture`] for the Codex host contract.
#[cfg(test)]
fn codex_problems_fixture() -> (ProductDispatcher, super::host_binding::BindingRef, String) {
    let git = "/usr/bin/git";
    let executable = json!({
        "path":git,
        "identity":"codex-git",
        "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
    });
    let launcher = LauncherConfig::parse(
        json!({
            "version":1,
            "limits":{"queued":8,"details":8,"operation_ms":1000,"output_bytes":4096},
            "allowed_roots":["/private/tmp"],
            "targets":[{
                "attachment":"codex-attachment",
                "candidate":"/private/tmp/codex-worktree",
                "git":executable,
                "providers":[]
            }]
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let dispatcher = ProductDispatcher::with_managed_codex_launcher(launcher);
    let channel = dispatcher.channel("codex-attachment").unwrap();
    let actor_id = "codex-actor".to_owned();
    let meta = json!({
        "threadId": actor_id,
        "callId": "codex-start",
        "x-codex-turn-metadata": {}
    });
    let candidate = parse_candidate(meta.as_object().unwrap()).unwrap();
    let BindingStatus::Validated(invocation) = dispatcher
        .bindings
        .lock()
        .unwrap()
        .establish_managed_codex_start(candidate, channel)
    else {
        panic!("codex fixture binding must establish");
    };
    let binding = invocation.binding_ref().clone();
    (dispatcher, binding, actor_id)
}

/// EYES-r2: a `kind: "problems"` context call on the managed-Codex path never runs native
/// read-boundary reconciliation. It must route directly into the worker's in-memory problem
/// source — with no started worker task that path fails at enqueue (`internal`) — while the
/// removed short-circuit would have first requested reconciliation via
/// [`super::worker::WorkerHandle::managed_read_boundary`], which coalesces into a native change
/// hint this test can observe directly on the binding guard.
#[tokio::test]
async fn managed_codex_problems_context_short_circuits_before_read_boundary_reconciliation() {
    let (dispatcher, binding, actor_id) = codex_problems_fixture();
    let call_id = "codex-problems-call-1";
    let request = crate::app::transport::MethodDispatch::new(
        "codex-problems-request".to_owned(),
        call_id.to_owned(),
        "codex-attachment".to_owned(),
        AssistanceMethod::Context,
        crate::app::transport::OpaqueJson::from_value(
            &json!({
                "parameters": {"kind":"problems"},
                "host_meta": {
                    "threadId": actor_id,
                    "callId": call_id,
                    "x-codex-turn-metadata": {},
                    "codex/sandbox-state-meta": {
                        "permissionProfile":{"type":"disabled"},
                        "codexLinuxSandboxExe":null,
                        "sandboxCwd":"/private/tmp",
                        "useLegacyLandlock":false
                    }
                }
            }),
            64 * 1024,
        )
        .unwrap(),
    )
    .unwrap();
    let reply = dispatcher
        .handle(&AssistanceDispatch::MethodDispatch(request), &mut None)
        .await
        .expect("codex problems dispatch must produce a typed reply");
    assert_eq!(
        reply,
        PeerReply::Error {
            code: super::reply::FailureCode::Internal,
            detail: None,
        }
    );
    // The removed short-circuit would have called `managed_read_boundary`, which coalesces a
    // registered-path reconciliation request into this exact native-hint slot.
    assert_eq!(
        dispatcher
            .bindings
            .lock()
            .unwrap()
            .take_native_change_hint(&binding),
        Ok(false),
        "problems short-circuit must skip managed-Codex read-boundary reconciliation"
    );
}
