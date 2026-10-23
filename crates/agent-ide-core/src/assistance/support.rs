//! Restart-only, fail-open Jev pilot: validated prepared context at activation, or one
//! post-edit episode borrowing existing observations and admission. Neither private path
//! fabricates main-agent read-memory or a model invocation.

use super::*;

#[cfg(feature = "jev-pilot")]
use crate::jev as client;

#[cfg(feature = "jev-pilot")]
#[path = "support/packet.rs"]
mod packet;

/// Optional helper owner; OFF allocates neither a client nor an asynchronous task.
pub(super) struct Pilot {
    /// Safe startup disposition; configuration errors never reject core startup.
    reason: &'static str,
    /// Validated opt-in, absent for OFF, malformed settings or an unavailable build feature.
    #[cfg(feature = "jev-pilot")]
    config: Option<Config>,
    /// Single episode state, never held over asynchronous I/O.
    #[cfg(feature = "jev-pilot")]
    state: Mutex<State>,
    /// Existing test settle hook wakes the optional owner after actual child cleanup.
    #[cfg(feature = "jev-pilot")]
    settled: Notify,
    /// Offline handshake after the real admitted child has registered.
    #[cfg(all(test, feature = "jev-pilot"))]
    running: Notify,
}

impl Pilot {
    /// Validates only helper configuration; never reads credentials, constructs HTTP or sends.
    pub(super) fn new(raw: Option<&Value>) -> Self {
        #[cfg(feature = "jev-pilot")]
        let (config, reason) = match parse_config(raw) {
            Ok(Some(config)) => (Some(config), "enabled"),
            Ok(None) => (None, "off"),
            Err(reason) => (None, reason),
        };
        #[cfg(not(feature = "jev-pilot"))]
        let reason =
            if raw.is_some_and(|raw| raw.get("mode").and_then(Value::as_str) == Some("jev")) {
                "feature_disabled"
            } else {
                "off"
            };
        Self {
            reason,
            #[cfg(feature = "jev-pilot")]
            config,
            #[cfg(feature = "jev-pilot")]
            state: Mutex::default(),
            #[cfg(feature = "jev-pilot")]
            settled: Notify::new(),
            #[cfg(all(test, feature = "jev-pilot"))]
            running: Notify::new(),
        }
    }

    /// Post-edit inference requests verified write geometry independently of main read memory.
    /// Prepared early context needs no helper-only write-base read.
    /// OFF retains the ordinary no-context fast path and performs no helper-only base read.
    pub(super) fn needs_geometry(&self) -> bool {
        #[cfg(feature = "jev-pilot")]
        return self
            .config
            .as_ref()
            .is_some_and(|config| config.prepared_context_file.is_none());
        #[cfg(not(feature = "jev-pilot"))]
        false
    }

    /// Reports an owned optional task, so idle shutdown does not abandon cleanup.
    pub(super) fn is_busy(&self) -> bool {
        #[cfg(feature = "jev-pilot")]
        return self
            .state
            .lock()
            .is_ok_and(|state| state.task.as_ref().is_some_and(|task| !task.is_finished()));
        #[cfg(not(feature = "jev-pilot"))]
        false
    }

    /// Safe operator-only startup reason, containing no settings, source or credential paths.
    pub(super) fn reason(&self) -> &'static str {
        self.reason
    }

    /// Extends the existing single settle hook; notification itself never means child exit.
    pub(super) fn on_settle(&self, _id: u64) {
        #[cfg(feature = "jev-pilot")]
        self.settled.notify_one();
    }

    /// Requests optional-work cleanup on an abrupt handle drop; only async shutdown proves joining.
    pub(super) fn request_stop(&self) {
        #[cfg(feature = "jev-pilot")]
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.cancel.cancel();
            state.note = None;
        }
    }
}

/// Cancels only this helper's exact binding (or all at shutdown), then joins its task.
/// A running test is cancelled by its owner and joined through the existing settle channel.
/// Main requests wait for actual cleanup; `cause` is the internal main_test/stop/shutdown label.
/// Every owned wait records exact milliseconds; a delay above two seconds is a violation.
pub(super) async fn interrupt(
    shared: &Arc<Shared>,
    binding: Option<&BindingRef>,
    cause: &'static str,
) {
    #[cfg(feature = "jev-pilot")]
    {
        let began = std::time::Instant::now();
        let task = shared.support.state.lock().ok().and_then(|mut state| {
            if binding.is_some_and(|binding| state.owner.as_ref() != Some(binding)) {
                return None;
            }
            state.closed = true;
            state.note = None;
            state.cancel.cancel();
            state.task.take()
        });
        if let Some(task) = task {
            let joined = task.await;
            let metric = format!(
                "jev_pilot:owned_cleanup; cause={cause}; wait_ms={:.6}",
                began.elapsed().as_secs_f64() * 1000.0
            );
            crate::errorlog::record(
                crate::errorlog::Method::Daemon,
                crate::errorlog::Outcome::Completed,
                crate::errorlog::Fields {
                    detail: Some(&metric),
                    ..Default::default()
                },
            );
            if joined.is_err() || began.elapsed() > Duration::from_secs(2) {
                operator_reason(if joined.is_err() {
                    "task_failed"
                } else {
                    "main_priority_delay_violation"
                });
            }
        }
    }
    #[cfg(not(feature = "jev-pilot"))]
    let _ = (shared, binding, cause);
}

/// Observes an already authorized real source change, coalescing edits for 250 milliseconds.
/// Only retained same-binding facts are considered; no source is registered or marked delivered.
pub(super) fn changed(
    shared: &Arc<Shared>,
    authority: &AuthorityStamp,
    source: &SourceObservation,
    target: &LaunchTarget,
) {
    #[cfg(feature = "jev-pilot")]
    {
        if shared
            .support
            .config
            .as_ref()
            .is_none_or(|config| config.prepared_context_file.is_some())
            || authority.role() != crate::workspace::authority::StartRole::Writer
        {
            return;
        }
        let input = Input::capture(shared, authority, source, target);
        let Ok(mut state) = shared.support.state.lock() else {
            return;
        };
        if state.closed {
            return;
        }
        if state
            .owner
            .as_ref()
            .is_some_and(|owner| owner != authority.binding())
        {
            return;
        }
        if state.last_change.as_ref().is_some_and(|previous| {
            previous.path() == source.path()
                && previous.bytes() == source.bytes()
                && previous.authority_epoch() == source.authority_epoch()
        }) {
            return;
        }
        state.last_change = Some(source.clone());
        if state.claimed {
            state.cancel.cancel();
            state.note = None;
            return;
        }
        state.pending = Some(input);
        if state.task.is_some() {
            return;
        }
        state.owner = Some(authority.binding().clone());
        let shared = shared.clone();
        let cancel = state.cancel.clone();
        state.task = Some(tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
            let input = shared.support.state.lock().ok().and_then(|mut state| {
                state.claimed = true;
                state.pending.take()
            });
            if let Some(input) = input {
                episode(&shared, input, cancel).await;
            }
        }));
    }
    #[cfg(not(feature = "jev-pilot"))]
    let _ = (shared, authority, source, target);
}

/// Consumes a source-free suggestion only on the next normal same-binding reply and same epoch.
/// This never creates a wake or alters a source-delivery receipt.
pub(super) fn take_note(shared: &Shared, binding: &BindingRef) -> Option<String> {
    #[cfg(feature = "jev-pilot")]
    {
        let mut state = shared.support.state.lock().ok()?;
        if state.owner.as_ref() != Some(binding) {
            return None;
        }
        let (epoch, note) = state.note.take()?;
        (shared.outbox.current_epoch(binding) == epoch && shared.active(binding).is_ok())
            .then_some(note)
    }
    #[cfg(not(feature = "jev-pilot"))]
    {
        let _ = (shared, binding);
        None
    }
}

/// Validates separately prepared evidence at activation, without network I/O or main read receipts.
impl Worker<'_> {
    /// Returns only a source-free ready/unavailable line; code is retained under normal inspection.
    pub(super) async fn prepared_context(
        &mut self,
        job: &Job,
        authority: &AuthorityStamp,
    ) -> Option<String> {
        #[cfg(feature = "jev-pilot")]
        {
            let config = self.shared.support.config.clone()?;
            config.prepared_context_file.as_ref()?;
            if authority.role() != crate::workspace::authority::StartRole::Writer {
                return None;
            }
            if self
                .shared
                .support
                .state
                .lock()
                .ok()
                .is_none_or(|state| state.claimed || state.closed)
            {
                return None;
            }
            let budget = job
                .deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_millis(200));
            let outcome =
                tokio::time::timeout(budget, self.collect_prepared(job, authority, &config))
                    .await
                    .unwrap_or(Err("prepared_budget"));
            Some(match outcome {
                Ok(reference) => format!(
                    "Jev context ready: {reference}; partial source evidence, unresolved contract."
                ),
                Err(reason) => {
                    operator_reason(reason);
                    format!(
                        "Jev context unavailable: {reason}; ordinary IDE operation remains available."
                    )
                }
            })
        }
        #[cfg(not(feature = "jev-pilot"))]
        {
            let _ = (job, authority);
            None
        }
    }

    /// Observes each exact file through normal Workspace authority; private reads stay private.
    #[cfg(feature = "jev-pilot")]
    async fn collect_prepared(
        &mut self,
        job: &Job,
        authority: &AuthorityStamp,
        config: &Config,
    ) -> Result<String, &'static str> {
        let model = config.selected_route().model();
        let prepared = packet::Prepared::load(
            config
                .prepared_context_file
                .as_deref()
                .ok_or("prepared_file")?,
            &config.trace_dir,
            &config.task,
            model,
        )?;
        let binding = authority.binding();
        let epoch = self.shared.outbox.current_epoch(binding);
        let mut sources = BTreeMap::new();
        for window in &prepared.evidence {
            if sources.contains_key(&window.path) {
                continue;
            }
            let (observed, bytes) = self
                .observe_as(binding, window.path.clone(), false)
                .await
                .map_err(|_| "prepared_source")?;
            if observed.authority_epoch() != authority.epoch()
                || !prepared.matches(&window.path, &bytes)
            {
                return Err("prepared_stale");
            }
            sources.insert(window.path.clone(), observed);
        }
        let active = self.shared.active(binding).map_err(|_| "stopped")?;
        self.workspace
            .authorize(authority, &active)
            .await
            .map_err(|_| "authority_stale")?;
        if self.shared.outbox.current_epoch(binding) != epoch
            || sources.values().any(|source| !source_matches(source))
        {
            return Err("prepared_stale");
        }
        let text = prepared.render()?;
        let input = Input {
            authority: authority.clone(),
            sources: sources
                .values()
                .cloned()
                .map(|s| Snapshot::Observed(Box::new(s)))
                .collect(),
            changed: PathBuf::new(),
            target: job.target.clone(),
            epoch,
            span: None,
        };
        let mut state = self
            .shared
            .support
            .state
            .lock()
            .map_err(|_| "state_unavailable")?;
        if state.claimed
            || state.closed
            || state.owner.as_ref().is_some_and(|owner| owner != binding)
        {
            return Err("already_claimed");
        }
        state.owner = Some(binding.clone());
        state.claimed = true;
        drop(state);
        let reference =
            reserve_result(&self.shared, &input, AssistanceTool::Inspect).ok_or("result_budget")?;
        let mut sources = sources.into_values();
        let primary = sources.next().ok_or("prepared_source")?;
        self.shared.add_edit_sources(&reference, sources.collect());
        self.shared.complete(
            &reference,
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: text.clone(),
                detail_ref: Some(reference.clone()),
                truncated: false,
                continuation: false,
            },
            Some(authority.clone()),
            Some(primary),
            epoch.native_epoch,
        );
        // A normal source-bearing job-ready fact uses the existing re-proof and Bridge owner.
        self.shared.push_or_transfer(
            std::slice::from_ref(binding),
            super::super::outbox::FactDraft {
                kind: super::super::outbox::FactKind::JobReady,
                subject: "support context".into(),
                summary: "ready: partial code evidence; intended contract unresolved".into(),
                detail: Some(text),
                detail_ref: Some(reference.clone()),
                failed: false,
                source_bearing: true,
                fence_paths: input
                    .sources
                    .iter()
                    .map(|source| super::super::outbox::FencedPath {
                        path: source.path().to_string_lossy().into_owned(),
                        digest: source.bytes().map(|bytes| *bytes.digest()),
                    })
                    .collect(),
            },
        );
        operator_reason("prepared_context_ready");
        Ok(reference)
    }
}

/// Records only a fixed safe helper disposition in the existing operator journal.
pub(super) fn operator_reason(reason: &'static str) {
    let detail = format!("jev_pilot:{reason}");
    crate::errorlog::record(
        crate::errorlog::Method::Daemon,
        crate::errorlog::Outcome::Incomplete,
        crate::errorlog::Fields {
            detail: Some(&detail),
            ..Default::default()
        },
    );
}

/// Trusted restart-only helper schema; unknown or incomplete helper fields disable only it.
#[cfg(feature = "jev-pilot")]
#[derive(serde::Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Config {
    /// Closed mode, default OFF; no model argument can opt in.
    #[serde(default)]
    mode: String,
    /// Optional operator accounting label; it never selects a coded provider preset.
    #[serde(default)]
    route: String,
    /// Credential-free HTTPS base URL; combined with api_path.
    #[serde(default)]
    base_url: String,
    /// Relative System One request path, allowing compatible providers without rebuilding.
    #[serde(default)]
    api_path: String,
    /// Exact model identifier to send.
    #[serde(default)]
    model: String,
    /// Exact response identifier; defaults to model when omitted.
    #[serde(default)]
    response_model: Option<String>,
    /// Optional operator tariff estimate; actual usage.cost is retained independently.
    #[serde(default)]
    input_usd_per_million: Option<f64>,
    /// Validated transport cached after parsing; never deserialized from an actor input.
    #[serde(skip)]
    transport: Option<client::Transport>,
    /// Nonempty task text (at most 1024 bytes), also the stable one-shot task identity.
    #[serde(default)]
    task: String,
    /// Absolute private credential file; opened only after exclusive episode claim.
    #[serde(default)]
    credential_file: PathBuf,
    /// Explicit operator permission to send bounded task/source context to the configured endpoint.
    #[serde(default)]
    egress_allowed: bool,
    /// Private precomputed context; when present it replaces post-edit inference for this pilot.
    #[serde(default)]
    prepared_context_file: Option<PathBuf>,
    /// Optional preferred observed paths; alternatives remain selectable when this scope is wrong.
    #[serde(default)]
    initial_scope: Vec<PathBuf>,
    /// Existing canonical owner-only directory holding exact private episode traces and claims.
    #[serde(default)]
    trace_dir: PathBuf,
}

#[cfg(feature = "jev-pilot")]
impl Config {
    /// Borrows the transport established by successful configuration parsing.
    fn selected_route(&self) -> &client::Transport {
        self.transport
            .as_ref()
            .expect("enabled configuration has validated transport")
    }
}

/// Parses configuration without any I/O; empty or OFF mode ignores credential-related values.
#[cfg(feature = "jev-pilot")]
fn parse_config(raw: Option<&Value>) -> Result<Option<Config>, &'static str> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let mut config: Config = serde_json::from_value(raw.clone()).map_err(|_| "config_invalid")?;
    if config.mode.is_empty() || config.mode == "off" {
        return Ok(None);
    }
    if config.mode != "jev"
        || !config.egress_allowed
        || config.task.trim().is_empty()
        || config.task.len() > 1024
        || !config.credential_file.is_absolute()
        || !config.trace_dir.is_absolute()
        || config
            .prepared_context_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        || config.initial_scope.len() > 8
        || config
            .initial_scope
            .iter()
            .any(|path| !crate::workspace::observation::valid_relative_path(path))
    {
        return Err("config_incomplete");
    }
    config.transport = Some(client::Transport::new(
        &config.route,
        &config.base_url,
        &config.api_path,
        &config.model,
        config.response_model.as_deref(),
        config.input_usd_per_million,
    )?);
    Ok(Some(config))
}

/// Single task activation; once claimed or closed it never admits another episode.
#[cfg(feature = "jev-pilot")]
#[derive(Default)]
struct State {
    /// Latest coalesced source observation before the debounce deadline.
    pending: Option<Input>,
    /// Last real version triggering work, suppressing the same early/terminal receipt replay.
    last_change: Option<SourceObservation>,
    /// At most eight latest verified write spans, retaining multi-file change geometry only.
    spans: Vec<Span>,
    /// Exact generation owning all optional work.
    owner: Option<BindingRef>,
    /// Sole owned async episode, joined at main-test priority, stop or shutdown.
    task: Option<tokio::task::JoinHandle<()>>,
    /// Sticky one-episode admission fence.
    claimed: bool,
    /// Sticky main-test/stop admission fence.
    closed: bool,
    /// Cancellation observed during debounce, HTTP, computation and test wait.
    cancel: tokio_util::sync::CancellationToken,
    /// Source-free next-natural-reply recommendation with its source epoch.
    note: Option<(super::super::outbox::JobEpoch, String)>,
    /// Offline decisions and an already-admitted fixture recipe; absent in production builds.
    #[cfg(test)]
    fixture: Option<Fixture>,
}

/// Immutable local observation binding source versions, generation, task and alternatives.
#[cfg(feature = "jev-pilot")]
#[derive(Clone)]
struct Input {
    /// Durable writer authority; every request and action reauthorizes it.
    authority: AuthorityStamp,
    /// Exact source observations, newest version per path, never more than eight.
    sources: Vec<Snapshot>,
    /// Confirmed changed file, used when no preferred observed scope matches.
    changed: PathBuf,
    /// Existing trusted launch target used by normal recipe planning.
    target: LaunchTarget,
    /// Native/edit epoch; an intervening source change invalidates a decision.
    epoch: super::super::outbox::JobEpoch,
    /// Exact confirmed write metadata when the normal edit guard proved before/after bytes.
    span: Option<Span>,
}

/// Byte identities and first changed byte of one confirmed write, scoped to its exact owner.
#[cfg(feature = "jev-pilot")]
#[derive(Clone)]
struct Span {
    /// Exact binding generation owning the verified edit.
    binding: BindingRef,
    /// Worktree-relative changed file.
    path: PathBuf,
    /// Digest of the verified pre-write base.
    before: [u8; 32],
    /// Digest of the exact post-write bytes.
    after: [u8; 32],
    /// First differing byte, or the shared prefix length for append/delete.
    offset: usize,
}

/// Retains bounded change geometry for the latest eight verified files, never their bodies.
/// This performs no read, registration, model call or delivery bookkeeping.
pub(super) fn note_span(
    shared: &Shared,
    binding: &BindingRef,
    observed: &SourceObservation,
    before: &[u8],
    after: &[u8],
) {
    #[cfg(feature = "jev-pilot")]
    if shared.support.config.is_some()
        && before != after
        && let Ok(mut state) = shared.support.state.lock()
    {
        state
            .spans
            .retain(|span| span.binding != *binding || span.path != observed.path());
        if state.spans.len() == 8 {
            state.spans.remove(0);
        }
        state.spans.push(Span {
            binding: binding.clone(),
            path: observed.path().to_owned(),
            before: *blake3::hash(before).as_bytes(),
            after: *blake3::hash(after).as_bytes(),
            offset: before
                .iter()
                .zip(after)
                .position(|(a, b)| a != b)
                .unwrap_or(before.len().min(after.len())),
        });
    }
    #[cfg(not(feature = "jev-pilot"))]
    let _ = (shared, binding, observed, before, after);
}

#[cfg(feature = "jev-pilot")]
impl Input {
    /// Captures bounded existing observations without new reads, authority or delivery receipts.
    fn capture(
        shared: &Shared,
        authority: &AuthorityStamp,
        source: &SourceObservation,
        target: &LaunchTarget,
    ) -> Self {
        let mut sources = BTreeMap::new();
        if let Ok(ledger) = shared.ledger.lock() {
            for detail in ledger
                .details
                .values()
                .filter(|detail| detail.binding == *authority.binding())
            {
                for observed in detail.source.iter().chain(&detail.extra_sources) {
                    if observed.authority_epoch() == authority.epoch()
                        && observed.worktree() == authority.worktree()
                    {
                        let kept = sources
                            .entry(observed.path().to_owned())
                            .or_insert_with(|| observed.clone());
                        if observed.sequence() > kept.sequence() {
                            *kept = observed.clone();
                        }
                    }
                }
            }
        }
        sources.insert(source.path().to_owned(), source.clone());
        let mut bounded = vec![source.clone()];
        bounded.extend(
            sources
                .into_values()
                .filter(|other| other.path() != source.path())
                .take(7),
        );
        bounded.sort_by(|a, b| a.path().cmp(b.path()));
        Self {
            authority: authority.clone(),
            sources: bounded
                .into_iter()
                .map(|source| Snapshot::Observed(Box::new(source)))
                .collect(),
            changed: source.path().to_owned(),
            target: target.clone(),
            epoch: shared.outbox.current_epoch(authority.binding()),
            span: shared.support.state.lock().ok().and_then(|state| {
                state
                    .spans
                    .iter()
                    .rev()
                    .find(|span| {
                        span.binding == *authority.binding()
                            && span.path == source.path()
                            && source
                                .bytes()
                                .is_some_and(|bytes| bytes.digest() == &span.after)
                    })
                    .cloned()
            }),
        }
    }
}

/// Local source evidence: durable main observations or a private, descriptor-rooted scope read.
/// A scoped snapshot is never persisted as a source receipt and claims no durable sequence.
#[cfg(feature = "jev-pilot")]
#[derive(Clone)]
enum Snapshot {
    /// Previously authorized source with its real durable file version.
    Observed(Box<SourceObservation>),
    /// Trusted initial scope read under current writer authority; only byte identity is claimed.
    Scoped {
        /// Existing authorized relative path from restart-only launcher input.
        path: PathBuf,
        /// Actual bounded bytes' identity, rechecked before every decision and effect.
        bytes: crate::workspace::observation::SourceBytes,
        /// BLAKE3 byte version, explicitly private rather than a durable source sequence.
        version: String,
    },
}

#[cfg(feature = "jev-pilot")]
impl Snapshot {
    /// Returns the captured relative path without treating it as a new permission.
    fn path(&self) -> &Path {
        match self {
            Self::Observed(source) => source.path(),
            Self::Scoped { path, .. } => path,
        }
    }
    /// Returns actual read byte metadata, absent for a recorded missing source.
    fn bytes(&self) -> Option<&crate::workspace::observation::SourceBytes> {
        match self {
            Self::Observed(source) => source.bytes(),
            Self::Scoped { bytes, .. } => Some(bytes),
        }
    }
    /// Distinguishes durable source revisions from private byte snapshots in model state.
    fn version(&self) -> &str {
        match self {
            Self::Observed(source) => source.source_revision().as_str(),
            Self::Scoped { version, .. } => version,
        }
    }
    /// Only complete observed/read bytes may support a fresh decision.
    fn complete(&self) -> bool {
        match self {
            Self::Observed(source) => source.coverage().is_complete(),
            Self::Scoped { .. } => true,
        }
    }
}

/// Aggregate read and model-call accounting; checking freshness also consumes read bytes.
#[cfg(feature = "jev-pilot")]
#[derive(Default)]
struct Budget {
    /// Conservative read-byte charge, including freshness rereads; at most 2 MiB per episode.
    /// Successful reads charge actual bytes; failures charge their reader cap plus overflow byte.
    read_bytes: usize,
    /// Admitted model decisions, including choosing a test; at most two.
    calls: usize,
    /// Physical send intents durably recorded before transport admission.
    attempts: usize,
    /// Attempts lacking a terminal response with reported usage, including interruptions.
    unknown_attempts: usize,
    /// Sum of exact serialized request bytes admitted to transport.
    request_bytes: usize,
    /// Sum of actual received HTTP body bytes.
    response_bytes: usize,
    /// Completed transport durations in monotonic milliseconds; interrupted timings stay unknown.
    latencies_ms: Vec<f64>,
    /// Sum of known tariff estimates, never represented as actual provider charges.
    estimated_usd: f64,
    /// Reported usage for completed physical attempts; missing/interrupted coverage stays unknown.
    usages: Vec<Option<Value>>,
    /// Monotonic start of this episode, used only for duration evidence.
    began: Option<std::time::Instant>,
}

/// Validates binding, durable generation, epoch and every exact file version before/after use.
/// Reads only the eight captured files within the aggregate 2 MiB limit and existing reader cap.
#[cfg(feature = "jev-pilot")]
async fn fresh(
    shared: &Shared,
    input: &Input,
    budget: &mut Budget,
) -> Result<Vec<String>, &'static str> {
    if shared
        .shutting_down
        .load(std::sync::atomic::Ordering::Acquire)
        || shared.outbox.current_epoch(input.authority.binding()) != input.epoch
    {
        return Err("stale");
    }
    let workspace = shared
        .publication_workspace
        .lock()
        .ok()
        .and_then(|workspace| workspace.clone())
        .ok_or("authority_unavailable")?;
    let active = shared
        .active(input.authority.binding())
        .map_err(|_| "stopped")?;
    workspace
        .authorize(&input.authority, &active)
        .await
        .map_err(|_| "authority_stale")?;
    let mut bodies = Vec::new();
    for source in &input.sources {
        let read = read_snapshot(input.authority.worktree(), source.path(), budget)?;
        if source.bytes() != Some(read.bytes()) || !source.complete() {
            return Err("source_stale");
        }
        bodies.push(String::from_utf8(read.contents().to_vec()).map_err(|_| "source_encoding")?);
    }
    let active = shared
        .active(input.authority.binding())
        .map_err(|_| "stopped")?;
    workspace
        .authorize(&input.authority, &active)
        .await
        .map_err(|_| "authority_stale")?;
    if shared.outbox.current_epoch(input.authority.binding()) != input.epoch {
        return Err("stale");
    }
    Ok(bodies)
}

/// Reads one private snapshot through the existing no-follow reader and shared aggregate budget.
/// Reserves the reader's one overflow-detection byte; failed reads consume their conservative cap.
#[cfg(feature = "jev-pilot")]
fn read_snapshot(
    tree: &crate::workspace::authority::WorktreeRef,
    path: &Path,
    budget: &mut Budget,
) -> Result<crate::workspace::observation::SourceRead, &'static str> {
    use crate::workspace::observation::{
        MAX_SOURCE_BYTES, SourceReadLimits, read_authorized_source,
    };
    let remaining = (2 * 1024 * 1024_usize)
        .checked_sub(budget.read_bytes)
        .ok_or("read_budget")?;
    let cap = remaining.saturating_sub(1).min(MAX_SOURCE_BYTES);
    if cap == 0 {
        return Err("read_budget");
    }
    match read_authorized_source(
        tree,
        path,
        SourceReadLimits::new(4096, cap).map_err(|_| "read_budget")?,
    ) {
        Ok(read) => {
            budget.read_bytes += read.contents().len();
            Ok(read)
        }
        Err(_) => {
            budget.read_bytes += cap + 1;
            Err("source_unavailable")
        }
    }
}

/// One closed local capability. IDs refer to this enum and never become model-generated paths/argv.
#[cfg(feature = "jev-pilot")]
enum Action {
    /// Remain inactive.
    Inactive,
    /// Retain a source-free recommendation for a normal reply.
    Suggest,
    /// Inspect, expand or reopen a captured file by index.
    Focus(usize),
    /// Start the sole current admitted test recipe.
    Run,
}

/// Existing admitted recipe plus its original display argv and parser language.
#[cfg(feature = "jev-pilot")]
struct Plan {
    /// Exact existing test target; source-to-test relation remains explicitly unverified.
    path: PathBuf,
    /// Core-admitted executable, arguments, environment and recipe bounds.
    spec: crate::checks::runner::RunSpec,
    /// Existing display argv (never used to spawn).
    argv: Vec<String>,
    /// Registered output parser.
    language: Language,
}

/// Offers at most twelve options with stable IDs/order and one optional run capability.
/// A current window is already visible; private reads require a later decision to use them.
#[cfg(feature = "jev-pilot")]
fn options(
    input: &Input,
    focus: usize,
    decisions_remaining: usize,
    plan: Option<&Plan>,
) -> (Vec<client::Candidate>, Vec<Action>) {
    let mut entries = vec![
        ("no_action: remain inactive".to_owned(), Action::Inactive),
        (
            "suggest: ask the main agent to review the observed scope on its next reply".into(),
            Action::Suggest,
        ),
    ];
    for (index, source) in input.sources.iter().enumerate() {
        entries.push((
            format!(
                "{} observed file {:?}; relation certainty: observed only",
                if index == focus {
                    "inspect/reopen"
                } else {
                    "expand/reopen"
                },
                source.path()
            ),
            Action::Focus(index),
        ));
    }
    if let Some(plan) = plan {
        entries.push((
            format!("run the existing admitted test for {:?}", plan.path),
            Action::Run,
        ));
    }
    let mut candidates = Vec::new();
    let mut actions = Vec::new();
    for (index, (description, action)) in entries.into_iter().enumerate() {
        if matches!(action, Action::Focus(source) if source == focus || decisions_remaining < 2) {
            continue;
        }
        candidates.push(client::Candidate {
            id: format!("a{index}"),
            description,
            auto_test: matches!(action, Action::Run),
        });
        actions.push(action);
    }
    (candidates, actions)
}

/// Exact private JSON-lines trace and durable exclusive one-shot claim for a task/worktree pair.
#[cfg(feature = "jev-pilot")]
struct Trace(std::fs::File);

#[cfg(feature = "jev-pilot")]
impl Trace {
    /// Claims before any request using create_new + file/directory fsync, refusing restart replay.
    /// The existing canonical directory must belong to this UID and deny all group/world access.
    fn claim(config: &Config, input: &Input) -> Result<Self, &'static str> {
        use std::io::Write;
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        };
        let directory = &config.trace_dir;
        let metadata = std::fs::symlink_metadata(directory).map_err(|_| "trace_unavailable")?;
        // SAFETY: geteuid only reads the current effective user identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || directory.canonicalize().map_err(|_| "trace_unavailable")? != *directory
        {
            return Err("trace_not_private");
        }
        let mut identity = blake3::Hasher::new();
        identity.update(config.task.as_bytes());
        identity.update(&[0]);
        identity.update(
            input
                .authority
                .worktree()
                .worktree_path()
                .as_os_str()
                .as_bytes(),
        );
        let key = identity.finalize().to_hex();
        let mut claim = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(format!("{key}.claim")))
            .map_err(|_| "already_claimed_or_trace_unavailable")?;
        claim
            .write_all(b"claimed; never replay this task\n")
            .and_then(|_| claim.sync_all())
            .map_err(|_| "trace_unavailable")?;
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| "trace_unavailable")?;
        let trace = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join(format!("{key}.jsonl")))
            .map_err(|_| "trace_unavailable")?;
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| "trace_unavailable")?;
        Ok(Self(trace))
    }

    /// Writes bounded exact decision evidence without credentials; failure disables the helper.
    fn record(&mut self, value: Value) -> Result<(), &'static str> {
        use std::io::Write;
        let mut bytes = serde_json::to_vec(&value).map_err(|_| "trace_invalid")?;
        if bytes.len() > 128 * 1024 {
            return Err("trace_budget");
        }
        bytes.push(b'\n');
        self.0
            .write_all(&bytes)
            .and_then(|_| self.0.sync_data())
            .map_err(|_| "trace_unavailable")
    }
}

/// Runs the single bounded compute episode outside the Worker loop and every runtime lock.
/// Dropping the bounded HTTP future never retries; started tests retain their historical result.
#[cfg(feature = "jev-pilot")]
async fn episode(
    shared: &Arc<Shared>,
    mut input: Input,
    cancel: tokio_util::sync::CancellationToken,
) {
    let Some(config) = shared.support.config.as_ref() else {
        return;
    };
    let mut trace = match Trace::claim(config, &input) {
        Ok(trace) => trace,
        Err(reason) => {
            operator_reason(reason);
            return;
        }
    };
    if trace.record(serde_json::json!({"route":config.selected_route().name(),"task_digest":blake3::hash(config.task.as_bytes()).to_hex().to_string(),"activation":input.authority.activation_id(),"generation":input.authority.epoch()})).is_err() { return; }
    let mut budget = Budget {
        began: Some(std::time::Instant::now()),
        ..Default::default()
    };
    let computed = tokio::select! {
        _ = cancel.cancelled() => Err("cancelled"),
        result = tokio::time::timeout(Duration::from_secs(5), async {
            fresh(shared, &input, &mut budget).await?;
            for path in &config.initial_scope {
                if input.sources.iter().any(|source| source.path() == path) { continue; }
                if input.sources.len() >= 8 { return Err("scope_file_budget"); }
                let read = read_snapshot(input.authority.worktree(),path,&mut budget)?;
                input.sources.push(Snapshot::Scoped { path: path.clone(), bytes: read.bytes().clone(), version: format!("private:{}", blake3::hash(read.contents()).to_hex()) });
            }
            input.sources.sort_by(|a,b| a.path().cmp(b.path()));
            compute(shared, &input, config, &mut budget, &mut trace).await
        }) => result.unwrap_or(Err("compute_timeout")),
    };
    match computed {
        Ok(Some(plan)) => run(shared, &input, plan, cancel, &mut budget, &mut trace).await,
        Ok(None) => {}
        Err(reason) => {
            operator_reason(reason);
            let _ = trace.record(serde_json::json!({"inactive":reason,"calls":budget.calls,"read_bytes":budget.read_bytes,"usage":"unknown if request interrupted"}));
        }
    }
    let _ = trace.record(serde_json::json!({"event":"episode_summary","decisions":budget.calls,"physical_send_intents":budget.attempts,"unknown_attempts":budget.unknown_attempts,"tokens":token_totals(&budget),"request_bytes":budget.request_bytes,"response_bytes":budget.response_bytes,"latencies_ms":budget.latencies_ms,"known_estimated_usd":budget.estimated_usd,"actual_provider_charge":Value::Null,"read_bytes":budget.read_bytes}));
}

/// Retains transport/usage facts, a tariff estimate and a validated provider-reported charge.
/// Missing usage remains unknown even for a free route; cache tokens have no invented tariff.
#[cfg(feature = "jev-pilot")]
#[allow(
    clippy::too_many_arguments,
    reason = "one bounded transport fact, no separate analytics model"
)]
fn record_attempt(
    trace: &mut Trace,
    budget: &mut Budget,
    decision: usize,
    route: &client::Transport,
    attempt: &client::Attempt,
    usage: Option<&Value>,
    outcome: &str,
    request: Value,
    response: Option<Value>,
) -> Result<(), &'static str> {
    let estimate = usage
        .and_then(|usage| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .and_then(|input| {
            route
                .input_usd_per_million
                .map(|price| input as f64 * price / 1_000_000.0)
        });
    if attempt.physical_attempt {
        budget.usages.push(usage.cloned());
        budget.response_bytes += attempt.response_bytes;
        budget.latencies_ms.push(attempt.total_ms);
        if let Some(cost) = estimate {
            budget.unknown_attempts = budget.unknown_attempts.saturating_sub(1);
            budget.estimated_usd += cost;
        }
    }
    let reported_charge = usage
        .and_then(|usage| usage.get("cost"))
        .and_then(Value::as_f64)
        .filter(|cost| cost.is_finite() && *cost >= 0.0);
    trace.record(serde_json::json!({"event":"request_terminal","decision":decision,"physical_attempt":attempt.physical_attempt,"outcome":outcome,"http_status":attempt.status,"total_ms":attempt.total_ms,"headers_ms":Value::Null,"body_ms":Value::Null,"response_bytes":attempt.response_bytes,"request":request,"response":response,"usage":usage,"usage_unknown":usage.is_none(),"estimated_usd":estimate,"actual_provider_charge":reported_charge,"tariff":{"observed_date":"2026-10-21","model":route.model(),"input_usd_per_million":route.input_usd_per_million,"output_usd_per_million":0.0,"cache_tariff":Value::Null,"source":"operator_configuration"}}))
}

/// Aggregates reported token counters with exact decimal sums and explicit per-field coverage.
/// Missing or interrupted attempt counters remain unknown rather than contributing invented zeroes.
#[cfg(feature = "jev-pilot")]
fn token_totals(budget: &Budget) -> Value {
    let mut fields: BTreeMap<String, (u128, usize)> =
        ["input_tokens", "output_tokens", "cache_tokens"]
            .into_iter()
            .map(|key| (key.into(), (0, 0)))
            .collect();
    for usage in budget.usages.iter().flatten() {
        collect_tokens(usage, "", &mut fields);
    }
    Value::Object(fields.into_iter().map(|(key,(sum,known))|(key,serde_json::json!({"known_sum_decimal":if known==0 {None} else {Some(sum.to_string())},"known_attempts":known,"unknown_attempts":budget.attempts.saturating_sub(known)}))).collect())
}

/// Collects actual numeric token fields, preserving nested cache-counter paths and no guessed units.
#[cfg(feature = "jev-pilot")]
fn collect_tokens(value: &Value, prefix: &str, fields: &mut BTreeMap<String, (u128, usize)>) {
    if let Some(object) = value.as_object() {
        for (name, value) in object {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            if name.contains("token")
                && let Some(count) = value.as_u64()
            {
                let entry = fields.entry(key.clone()).or_default();
                entry.0 += u128::from(count);
                entry.1 += 1;
            }
            if value.is_object() {
                collect_tokens(value, &key, fields);
            }
        }
    }
}

/// Returns a bounded UTF-8 current-source window around an actual byte offset, with partial coverage.
#[cfg(feature = "jev-pilot")]
fn fragment(source: &str, offset: usize) -> Value {
    let mut start = offset.min(source.len()).saturating_sub(768);
    while !source.is_char_boundary(start) {
        start += 1;
    }
    let mut end = (start + 2048).min(source.len());
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    serde_json::json!({"text":&source[start..end],"start_byte":start,"end_byte":end,"first_line":source[..start].bytes().filter(|byte|*byte==b'\n').count()+1,"coverage":"partial current source window"})
}

/// Computes at most two decisions: an optional new evidence window, then an external action.
/// Final private reads and already displayed windows are not useful local capabilities.
/// Only a validated Choice >=0.5 and independent test Noul >=0.8 may return an admitted plan.
#[cfg(feature = "jev-pilot")]
async fn compute(
    shared: &Arc<Shared>,
    input: &Input,
    config: &Config,
    budget: &mut Budget,
    trace: &mut Trace,
) -> Result<Option<Plan>, &'static str> {
    #[cfg(test)]
    let fixture = shared
        .support
        .state
        .lock()
        .is_ok_and(|state| state.fixture.is_some());
    #[cfg(not(test))]
    let fixture = false;
    let route = config.selected_route();
    let client = if fixture {
        None
    } else {
        Some(
            client::JevClient::from_credential_file(&config.credential_file, route)
                .map_err(|failure| failure.category)?,
        )
    };
    let mut focus = input
        .sources
        .iter()
        .position(|source| config.initial_scope.contains(&source.path().to_owned()))
        .or_else(|| {
            input
                .sources
                .iter()
                .position(|source| source.path() == input.changed)
        })
        .ok_or("scope_unavailable")?;
    for _ in 0..2 {
        let bodies = fresh(shared, input, budget).await?;
        if shared
            .test_runs
            .running(input.authority.worktree().worktree_path())
        {
            return Err("busy_foreign_or_main");
        }
        let plan = plan(shared, input, focus).await;
        let (candidates, actions) = options(input, focus, 2 - budget.calls, plan.as_ref());
        let state = serde_json::json!({"task":config.task,"binding":blake3::Hash::from_bytes(input.authority.binding().fingerprint()).to_hex().to_string(),"generation":input.authority.epoch(),"activation":input.authority.activation_id(),"changed":input.changed,"scope":input.sources[focus].path(),"file_version":input.sources[focus].version(),"source":fragment(&bodies[focus],input.span.as_ref().filter(|span|span.path==input.sources[focus].path()).map_or(0,|span|span.offset)),"confirmed_change":input.span.as_ref().and_then(|span|input.sources.iter().position(|source|source.path()==span.path).map(|index|serde_json::json!({"path":span.path,"before_hash":blake3::Hash::from_bytes(span.before).to_hex().to_string(),"after_hash":blake3::Hash::from_bytes(span.after).to_hex().to_string(),"first_differing_byte":span.offset,"current":fragment(&bodies[index],span.offset)}))),"coverage":"bounded observed files only; other relations unavailable","graphs":"unavailable; no ready graph borrowed","budget":{"decisions_remaining":2-budget.calls,"auto_tests_remaining":1,"files":input.sources.len(),"read_bytes":budget.read_bytes},"run_facts":"no support test started"});
        let state = packet::encode(&state)?;
        if candidates.len() > 12 {
            return Err("state_budget");
        }
        let evidence = serde_json::json!({"state":state,"options":candidates.iter().map(|candidate| serde_json::json!({"id":candidate.id,"description":candidate.description,"auto_test":candidate.auto_test})).collect::<Vec<_>>()});
        if fixture {
            trace.record(evidence.clone())?;
        }
        fresh(shared, input, budget).await?;
        budget.calls += 1;
        let decision = budget.calls;
        let before_send = |request: &Value| {
            trace.record(evidence).map_err(|_| ())?;
            let request_bytes = serde_json::to_vec(request).map_err(|_| ())?.len();
            trace.record(serde_json::json!({"event":"request_pending","decision":decision,"task":config.task,"activation":input.authority.activation_id(),"generation":input.authority.epoch(),"route":route.name(),"monotonic_since_episode_ms":budget.began.map(|began|began.elapsed().as_secs_f64()*1000.0),"endpoint":route.endpoint(),"model":route.model(),"wall_unix_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok().map(|time| time.as_millis()),"request":request,"request_bytes":request_bytes,"usage_unknown":true,"estimated_usd":Value::Null,"actual_provider_charge":Value::Null})).map_err(|_| ())?;
            budget.attempts += 1;
            budget.unknown_attempts += 1;
            budget.request_bytes += request_bytes;
            Ok(())
        };
        #[cfg(test)]
        let selection = if fixture {
            fixture_decision(shared).await?
        } else {
            client
                .as_ref()
                .ok_or("client_unavailable")?
                .decide(&state, &candidates, before_send)
                .await
        };
        #[cfg(not(test))]
        let selection = client
            .as_ref()
            .ok_or("client_unavailable")?
            .decide(&state, &candidates, before_send)
            .await;
        let selected = match selection {
            Ok(selection) => {
                record_attempt(
                    trace,
                    budget,
                    decision,
                    route,
                    &selection.attempt,
                    selection.usage.as_ref(),
                    "success",
                    selection.request.clone(),
                    Some(selection.response.clone()),
                )?;
                selection
            }
            Err(failure) => {
                let usage = if !failure.usage_unknown {
                    failure
                        .response
                        .as_ref()
                        .and_then(|response| response.get("usage"))
                } else {
                    None
                };
                record_attempt(
                    trace,
                    budget,
                    decision,
                    route,
                    &failure.attempt,
                    usage,
                    failure.category,
                    failure.request.clone().unwrap_or(Value::Null),
                    failure.response.clone(),
                )?;
                return Err(failure.category);
            }
        };
        fresh(shared, input, budget).await?;
        if !selected.confidence.is_finite()
            || selected.confidence < 0.5
            || selected.confidence > 1.0
        {
            return Err("low_confidence");
        }
        let action = candidates
            .iter()
            .position(|candidate| candidate.id == selected.selected_id)
            .and_then(|index| actions.get(index))
            .ok_or("unknown_id")?;
        trace.record(serde_json::json!({"event":"decision_selected","decision":budget.calls,"selected_id":selected.selected_id,"confidence":selected.confidence,"auto_test_probability":selected.auto_test_probability,"action":match action {Action::Inactive=>"inactive",Action::Suggest=>"suggest",Action::Focus(_)=>"focus",Action::Run=>"test"},"scope":input.sources[focus].path()}))?;
        match action {
            Action::Inactive => return Ok(None),
            Action::Suggest => {
                if let Ok(mut state) = shared.support.state.lock()
                    && !state.closed
                    && !state.cancel.is_cancelled()
                {
                    state.note = Some((input.epoch, "Jev support suggests reviewing the observed scope before selecting further tests.".into()));
                }
                return Ok(None);
            }
            Action::Focus(index) => focus = *index,
            Action::Run => {
                if selected.auto_test_probability.is_some_and(|probability| {
                    probability.is_finite() && (0.8..=1.0).contains(&probability)
                }) {
                    return Ok(plan);
                }
                return Err("test_not_useful");
            }
        }
    }
    Ok(None)
}

/// Uses the normal language recipe planner; in-process or missing-tool plans remain unavailable.
#[cfg(feature = "jev-pilot")]
async fn plan(shared: &Shared, input: &Input, focus: usize) -> Option<Plan> {
    #[cfg(test)]
    if let Some(plan) = shared.support.state.lock().ok().and_then(|state| {
        state
            .fixture
            .as_ref()
            .and_then(|fixture| fixture.plan.clone())
    }) {
        return Some(Plan {
            path: input.sources[focus].path().to_owned(),
            spec: plan.0,
            argv: vec!["offline-fixture".into()],
            language: plan.1,
        });
    }
    let root = input.authority.worktree().worktree_path();
    let path = input
        .sources
        .get(focus)
        .filter(|source| {
            Language::for_path(source.path()).is_some_and(|language| {
                !language.support().tests_only_in_test_files()
                    || language.support().is_test_file(source.path())
            })
        })
        .or_else(|| {
            input.sources.iter().find(|source| {
                Language::for_path(source.path())
                    .is_some_and(|language| language.support().is_test_file(source.path()))
            })
        })?
        .path()
        .to_owned();
    let Ok(Ok((argv, language, _, Some(crate::modules::calls::PlannedStart::Admitted(spec))))) =
        test_selection(
            root,
            crate::lang::TestTarget::File(path.clone()),
            Duration::from_secs(30),
            &shared.launcher,
            &input.target,
        )
        .await
    else {
        return None;
    };
    Some(Plan {
        path,
        spec,
        argv,
        language,
    })
}

/// Starts at most one existing admitted test under exact generation ownership, then waits for its
/// actual exit/reap. Same-root Running is only a mutex refusal, never semantic dedup or reuse.
#[cfg(feature = "jev-pilot")]
async fn run(
    shared: &Arc<Shared>,
    input: &Input,
    plan: Plan,
    cancel: tokio_util::sync::CancellationToken,
    budget: &mut Budget,
    trace: &mut Trace,
) {
    if fresh(shared, input, budget).await.is_err() {
        return;
    }
    let root = input.authority.worktree().worktree_path();
    let started = {
        let Ok(state) = shared.support.state.lock() else {
            return;
        };
        if state.closed
            || cancel.is_cancelled()
            || shared.outbox.current_epoch(input.authority.binding()) != input.epoch
            || shared.active(input.authority.binding()).is_err()
        {
            return;
        }
        let Some(reference) = reserve_result(shared, input, AssistanceTool::Test) else {
            return;
        };
        let Some(config) = shared.support.config.as_ref() else {
            return;
        };
        let origin = super::super::tests::SupportOrigin {
            binding: input.authority.binding().clone(),
            authority_epoch: input.authority.epoch(),
            task: *blake3::hash(config.task.as_bytes()).as_bytes(),
            source_epoch: input.epoch,
        };
        let start = shared.test_runs.start_support_admitted(
            root.to_owned(),
            plan.spec,
            plan.argv,
            input.authority.binding(),
            plan.language,
            Duration::from_secs(30),
            reference.clone(),
            origin,
        );
        match start {
            StartResult::Started(id) => Some(id),
            _ => {
                if let Ok(mut ledger) = shared.ledger.lock() {
                    ledger.details.remove(&reference);
                }
                None
            }
        }
    };
    let Some(id) = started else {
        return;
    };
    shared.test_started(id, input.authority.binding());
    #[cfg(test)]
    shared.support.running.notify_one();
    let _ = trace.record(serde_json::json!({"test_started":id,"origin":"support","generation":input.authority.epoch()}));
    loop {
        let notified = shared.support.settled.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(result) = shared.test_runs.settled_result(id) {
            let _ = trace.record(serde_json::json!({"test_settled":id,"exit":result.exit,"cancelled":result.cancelled,"stopped":result.stopped,"elapsed_ms":result.elapsed.as_millis(),"historical":shared.outbox.current_epoch(input.authority.binding()) != input.epoch,"command":result.command,"output_paged":result.output_paged}));
            break;
        }
        if cancel.is_cancelled() {
            if shared.test_runs.support_origin(id).is_some_and(|origin| {
                origin.binding == *input.authority.binding()
                    && origin.authority_epoch == input.authority.epoch()
                    && shared.support.config.as_ref().is_some_and(|config| {
                        origin.task == *blake3::hash(config.task.as_bytes()).as_bytes()
                    })
            }) {
                let _ = shared.test_runs.cancel(root, id, input.authority.binding());
            }
            notified.await;
        } else {
            tokio::select! { _ = cancel.cancelled() => {}, _ = &mut notified => {} }
        }
    }
}

/// Reserves an ordinary bounded result slot for support output without a forged
/// model invocation, a source receipt, or eviction of any main-agent work. Full means skip.
#[cfg(feature = "jev-pilot")]
fn reserve_result(shared: &Shared, input: &Input, carrier: AssistanceTool) -> Option<String> {
    let mut ledger = shared.ledger.lock().ok()?;
    if ledger.details.len() >= shared.launcher.limits.details {
        return None;
    }
    ledger.next = ledger.next.checked_add(1)?;
    let reference = format!(
        "{}-{}",
        blake3::Hash::from_bytes(shared.nonce).to_hex(),
        ledger.next
    );
    ledger.details.insert(
        reference.clone(),
        Detail {
            progress: Arc::new(Mutex::new(progress::Progress::default())),
            binding: input.authority.binding().clone(),
            reply: PeerReply::Pending {
                detail_ref: reference.clone(),
            },
            selection: (carrier, [0; 32]),
            authority: Some(input.authority.clone()),
            source: None,
            extra_sources: Vec::new(),
            native_epoch: input.epoch.native_epoch,
            line_movement: None,
            diff_page: None,
            diff_page_fresh: false,
            context_page: None,
            test_replay: None,
            context_page_fresh: false,
            diff_provenance: Some(BTreeSet::new()),
        },
    );
    Some(reference)
}

/// Offline-only scripted decisions and already-admitted fixture recipe; unreachable in production.
#[cfg(all(test, feature = "jev-pilot"))]
struct Fixture {
    /// Finite responses consumed without constructing HTTP or reading a credential.
    decisions: VecDeque<Result<client::Selection, client::Failure>>,
    /// Existing executor specification used to prove real exit, output, ownership and cleanup.
    plan: Option<(crate::checks::runner::RunSpec, Language)>,
    /// Optional one-decision handshake for deterministic stale/stop/cancel schedules.
    pause: Option<(Arc<Notify>, Arc<Notify>)>,
}

/// Pops a scripted response and optionally waits outside all locks at the decision boundary.
#[cfg(all(test, feature = "jev-pilot"))]
async fn fixture_decision(
    shared: &Shared,
) -> Result<Result<client::Selection, client::Failure>, &'static str> {
    let (decision, pause) = {
        let mut state = shared.support.state.lock().map_err(|_| "fixture_lock")?;
        let fixture = state.fixture.as_mut().ok_or("fixture_missing")?;
        (
            fixture.decisions.pop_front().ok_or("fixture_exhausted")?,
            fixture.pause.take(),
        )
    };
    if let Some((ready, resume)) = pause {
        ready.notify_one();
        resume.notified().await;
    }
    Ok(decision)
}

/// Offline wiring proofs use real worker admission, edits, child processes and output retention.
#[cfg(all(test, feature = "jev-pilot"))]
#[path = "support/tests.rs"]
mod tests;
