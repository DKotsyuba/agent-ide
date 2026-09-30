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
    AssistanceDispatcher, AssistanceMethod,
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
    /// Rate window for hooks of sessions that never activated; one line per ten minutes.
    hook_noise: Mutex<crate::errorlog::RateWindow>,
}

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
/// Poll interval of the pre-arrival window.
const PRE_ARRIVAL_POLL: std::time::Duration = std::time::Duration::from_millis(20);

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
/// Reads only in-memory snapshots and never waits for a running check. The plate is skipped, and
/// stays due for a later hook, whenever it could not fit beside `feedback` inside one bounded hook
/// context (EYES-r2 §5/§6). Hosts whose [`super::host_binding::FeedDelivery`] is
/// [`super::host_binding::FeedDelivery::Replies`] never take
/// this path; their plates ride terminal `ide.*` replies instead (`attach_reply_plate`).
fn due_plate(
    feed: Option<&Arc<ProjectProblemFeed>>,
    worker: Option<&WorkerHandle>,
    fingerprint: &[u8; 32],
    feedback: Option<&str>,
) -> Option<String> {
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

/// Leads `plate` with the binding's one-shot `git: HEAD moved …` line when one is due and the
/// merged plate still `fits`; the line is consumed only when delivered, otherwise it stays due
/// and `plate` is returned unchanged. A due line with no other plate becomes a plate of its own.
fn with_git_notice(
    worker: &WorkerHandle,
    fingerprint: &[u8; 32],
    plate: Option<String>,
    fits: impl FnOnce(&str) -> bool,
) -> Option<String> {
    let Some(notice) = worker.git_notice(fingerprint) else {
        return plate;
    };
    let merged = match &plate {
        Some(plate) => plate.replacen("<agent-ide>\n", &format!("<agent-ide>\n{notice}\n"), 1),
        None => format!("<agent-ide>\n{notice}\n</agent-ide>"),
    };
    if fits(&merged) && worker.consume_git_notice(fingerprint, &notice) {
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
    let line = text.lines().next()?;
    (line.starts_with("tests #")
        && !line.contains("started —")
        && !line.contains("still running")
        && !line.ends_with("unknown job"))
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
            hook_noise: Mutex::new(crate::errorlog::RateWindow::default()),
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
    pub fn with_managed_claude_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::with_launcher(launcher);
        dispatcher.managed_claude = true;
        dispatcher
    }
    /// Derives the same opaque channel for hook/MCP input under this exact daemon nonce.
    fn channel(&self, attachment: &str) -> Option<super::host_binding::ChannelSessionRef> {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.scope?);
        hash.update(attachment.as_bytes());
        parse_channel_session(hash.finalize().to_hex().as_bytes()).ok()
    }
    /// Parses separated ingress and commits binding transitions before queue, inspection or stop I/O.
    ///
    /// `status` is written only when a terminal `ide.*` reply for a reply-delivered host (T28B)
    /// carries the due status plate on top; the caller renders it ahead of the reply. A native
    /// hook has no current sandbox metadata, so it uses the binding feed's sticky restriction
    /// state before triggering a check or releasing cached feedback. Missing or unsupported
    /// metadata on a validated Codex binding restricts that feed before an error is returned.
    async fn handle(
        &self,
        request: &AssistanceDispatch,
        status: &mut Option<String>,
    ) -> Option<PeerReply> {
        match request {
            AssistanceDispatch::HookSubmit(hook) => {
                let observation: Value =
                    serde_json::from_str(hook.sanitized_observation_json().as_str()).ok()?;
                let object = observation.as_object()?;
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
                    return None;
                }
                // Six fixed relayed fields plus the optional `tool_name`; a hook from a release
                // that still relays the retired helper fields does not correlate.
                if object.len() != 6 + usize::from(object.contains_key("tool_name")) {
                    return None;
                }
                let phase = match object.get("phase")?.as_str()? {
                    "pre" => "PreToolUse",
                    "post" => "PostToolUse",
                    "post_failure" => "PostToolUseFailure",
                    "permission_denied" => "PermissionDenied",
                    "post_batch" => "PostToolBatch",
                    _ => return None,
                };
                let event = match object.get("host")?.as_str()? {
                    "codex" => parse_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"tool_use_id":object.get("call_id")?,"tool_name":object.get("tool_name")})
                            .to_string().as_bytes(),
                    ),
                    "claude" => parse_claude_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"agent_type":object.get("agent_type")?,"tool_use_id":object.get("call_id")?,"tool_name":object.get("tool_name")})
                            .to_string().as_bytes(),
                    ),
                    _ => return None,
                }
                .ok()?;
                if event.optional_call_id().unwrap_or("post-tool-batch") != hook.correlation_id() {
                    return None;
                }
                let channel = self.channel(hook.opaque_attachment())?;
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
                let status = self
                    .bindings
                    .lock()
                    .ok()?
                    .observe_hook(event.clone(), channel.clone());
                match status {
                    BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
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
                                ),
                                |plate| plate.len() <= super::reply::MAX_FEEDBACK_BYTES,
                            )
                        {
                            return Some(PeerReply::Feedback { text: block });
                        }
                        Some(PeerReply::HookSettled {})
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
                                feed.changed(&fingerprint);
                            }
                            let feedback =
                                if feed.is_some_and(|feed| feed.is_read_restricted(&fingerprint)) {
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
                                due_plate(feed, Some(worker), &fingerprint, feedback.as_deref()),
                                |plate| reserved + plate.len() <= super::reply::MAX_FEEDBACK_BYTES,
                            );
                            let text = match (block, feedback) {
                                (Some(block), Some(feedback)) => {
                                    Some(format!("{block}\n{feedback}"))
                                }
                                (block, feedback) => block.or(feedback),
                            };
                            if let Some(text) = text {
                                return Some(PeerReply::Feedback { text });
                            }
                        }
                        Some(PeerReply::NativeHookObserved {})
                    }
                    _ => {
                        // A hook that cannot correlate for a channel that never activated is
                        // bookkeeping, not a failure; a channel that did activate keeps its warn.
                        if self
                            .bindings
                            .lock()
                            .is_ok_and(|bindings| !bindings.channel_bound(&channel))
                        {
                            log_hook_inactive(&self.hook_noise, event.host());
                        }
                        None
                    }
                }
            }
            AssistanceDispatch::MethodDispatch(method) => {
                let envelope: Value = serde_json::from_str(method.params_json().as_str()).ok()?;
                let object = envelope.as_object()?;
                if object.len() != 2 {
                    return None;
                }
                let meta = object.get("host_meta")?.as_object()?;
                let host = parse_host_kind(meta).ok()?;
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
                    AssistanceMethod::HookSubmit => return None,
                };
                let call =
                    super::facade::validate_call(tool, object.get("parameters")?.clone()).ok()?;
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(method.opaque_attachment()))
                {
                    // This daemon never registered the calling attachment, so it has also never
                    // received a hook on its channel (T15B).
                    return Some(PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: Some(HostBindingCause::HooksNotDelivered),
                    });
                }
                let channel = self.channel(method.opaque_attachment())?;
                // Trusted re-activation ingress (T15B restart recovery): the managed Claude MCP
                // marks the start that re-runs a remembered activation after the daemon it had
                // activated on was replaced. The marker rides host metadata, never model
                // arguments, and only the managed shared daemon accepts it.
                let reactivation = host == HostKind::Claude
                    && method.method() == AssistanceMethod::Start
                    && self.managed_claude
                    && meta.contains_key("claudecode/reactivation");
                // A merely-late pre-hook must not be recorded as MCP-before-pre replay
                // evidence: the host fires each pre exactly once, and a daemon busy with a
                // sibling call can observe its submission hundreds of milliseconds late
                // (~405 ms in the T15B evidence, against the hook's own 250 ms deadline).
                // Give an absent Claude pre a bounded arrival window before the guard op; a
                // re-activation start instead waits for any pre on its channel, whose actor it
                // takes without consuming it.
                if host == HostKind::Claude {
                    let evidence = |bindings: &std::sync::MutexGuard<'_, HostBindingGuard>| {
                        if reactivation {
                            bindings.pending_pre_actor(&channel).is_some()
                        } else {
                            bindings.has_pre(method.correlation_id(), &channel)
                        }
                    };
                    if !self
                        .bindings
                        .lock()
                        .is_ok_and(|bindings| evidence(&bindings))
                    {
                        let deadline = tokio::time::Instant::now() + PRE_ARRIVAL_WAIT;
                        while tokio::time::Instant::now() < deadline {
                            tokio::time::sleep(PRE_ARRIVAL_POLL).await;
                            if self
                                .bindings
                                .lock()
                                .is_ok_and(|bindings| evidence(&bindings))
                            {
                                break;
                            }
                        }
                    }
                }
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
                    let mut bindings = self.bindings.lock().ok()?;
                    let status = match host {
                        HostKind::Codex => {
                            let candidate = parse_candidate(meta).ok()?;
                            if candidate.call_id() != method.correlation_id() {
                                return None;
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
                            let call_id = parse_claude_call_id(meta).ok()?;
                            if call_id != method.correlation_id() {
                                return None;
                            }
                            if method.method() == AssistanceMethod::Start && reactivation {
                                bindings.reactivate_start_claude(&call_id, channel.clone())
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
                            let hooks_delivered = bindings.channel_observed_hook(&channel);
                            drop(bindings);
                            return Some(PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause: host_binding_cause(
                                    self.worker.as_ref(),
                                    hooks_delivered,
                                    method.opaque_attachment(),
                                    reason,
                                ),
                            });
                        }
                        return None;
                    };
                    if method.method() == AssistanceMethod::Stop {
                        bindings.stop_binding(invocation.binding_ref()).ok()?;
                    } else if !stopped(&invocation) {
                        bindings.consume_active(invocation.binding_ref()).ok()?;
                    }
                    invocation
                };
                // Only a read-only call answers a stopped generation: its reply stays plate-free
                // and touches no worktree state, like `ide.stop`'s own.
                let stopped = stopped(&invocation);
                let Some(worker) = &self.worker else {
                    return Some(if method.method() == AssistanceMethod::Stop {
                        PeerReply::HostStopped {}
                    } else {
                        PeerReply::Unavailable {
                            reason: MissingPeer::WorkspaceActivation,
                            cause: None,
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
                        worker
                            .inspect(
                                invocation.binding_ref().clone(),
                                call.parameters()["detail_ref"].as_str()?.to_owned(),
                                method.opaque_attachment(),
                                None,
                            )
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
                        worker
                            .submit(
                                invocation,
                                tool,
                                call.parameters().clone(),
                                method.opaque_attachment(),
                            )
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
                        *status = with_git_notice(worker, &fingerprint, status.take(), |plate| {
                            super::content::fits_with_status(
                                &reply,
                                plate,
                                super::content::Envelope::WithStructured,
                            )
                        });
                    }
                }
                Some(reply)
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
            let mut result =
                self.handle(&request, &mut status)
                    .await
                    .unwrap_or(PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: None,
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
                    let parameters = serde_json::from_str::<Value>(method.params_json().as_str())
                        .ok()
                        .and_then(|envelope| envelope.get("parameters").cloned());
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
                    adapters::log_tool_reply(
                        tool,
                        &result,
                        started.elapsed(),
                        requested.as_deref(),
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
            cause: None
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
            cause: None
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
