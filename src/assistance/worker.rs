//! One daemon-owned worker with bounded jobs/details, durable authorization and revocable work.

use super::{
    claude_worker::{
        HelperBaseline, HelperOperation, HelperOutcome, HelperScope, SettledClaudeOperation,
    },
    facade::{AssistanceTool, FeedbackDelta, render_reply},
    host_binding::{
        ActiveBindingUse, BindingRef, HostBindingGuard, ObservedSandboxState, ValidatedInvocation,
    },
    launcher::{AcceptedProviderSettings, LaunchTarget, LauncherConfig},
    reply::{FailureCode, PeerReply, ResultKind},
};
use crate::workspace::observation::SourceObservation;
use crate::{
    app::{
        config::EffectiveConfig,
        store::{MigrationAdmission, OperationId, Store},
    },
    changes::edit::{
        EditOutcome as ChangesEditOutcome, EditReceiptStore, EditRequest, EditResult,
        PrepareAdmission,
    },
    workspace::{
        authority::{AuthorityStamp, StopBindingHandoff},
        durable::{DurableWorkspace, StartReceipt},
        store::WorkspaceStore,
    },
};
#[path = "providers.rs"]
mod providers;
#[path = "snapshots.rs"]
pub(super) mod snapshots;

use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// Separates daemon-executed evidence from settled Claude foreground-helper evidence.
///
/// Private on purpose: the discriminator is how the sole worker knows which evidence a job carries,
/// and it must never be reachable from the wire. [`JobInput::Claude`] can only be built from a
/// [`SettledClaudeOperation`], which itself has no public constructor and no `Deserialize`, so no
/// helper frame can steer a job onto the Claude branch.
enum JobInput {
    /// Ordinary daemon-executed work; evidence is produced by this process under `observed`.
    Managed,
    /// Positively settled Claude helper evidence, already proven correlated, owned and settled.
    Claude(Box<SettledClaudeOperation>),
    /// Daemon-only Changes preparation required before an Edit helper ticket may be minted.
    ClaudeEditPrepare,
    /// Daemon-ledger proof that an Edit expired pre-claim or became uncertain post-claim.
    ClaudeEditTerminal(ChangesEditOutcome),
}

/// A bounded asynchronous operation whose identity never includes the transient MCP call ID.
struct Job {
    /// Which evidence family this job carries.
    input: JobInput,
    /// Same-binding opaque result key retained in the result ledger.
    reference: String,
    /// Validated host invocation for this exact operation.
    invocation: ValidatedInvocation,
    /// Matching measured sandbox state; stop has no new physical admission from this state.
    observed: Option<ObservedSandboxState>,
    /// Closed current tool operation.
    tool: AssistanceTool,
    /// Closed validated model parameters, containing no target/profile/authority information.
    parameters: Value,
    /// Trusted attachment-selected immutable target.
    target: LaunchTarget,
    /// Absolute lifetime includes time spent queued.
    deadline: tokio::time::Instant,
    /// Revocation channel; cancellation is never interpreted as reap evidence.
    cancel: watch::Receiver<bool>,
    /// Stop may return its bounded exact revocation outcome directly after cleanup.
    stop_reply: Option<oneshot::Sender<PeerReply>>,
    /// Native lifecycle revision captured when execution begins, not when it was queued.
    native_epoch: u64,
}

/// A retained outcome requiring exact binding ownership and fresh durable authorization on access.
struct Detail {
    /// Immutable owner binding, checked separately from the opaque reference.
    binding: BindingRef,
    /// Current pending, error or owner-generated result.
    reply: PeerReply,
    /// Exact method/query selection, excluding only the opaque detail handle itself.
    selection: (AssistanceTool, [u8; 32]),
    /// Current Workspace stamp, required before any source/activation result can be delivered.
    authority: Option<AuthorityStamp>,
    /// Exact registered source facts rechecked before context delivery.
    source: Option<crate::workspace::observation::SourceObservation>,
    /// Native lifecycle revision associated with this result.
    native_epoch: u64,
    /// Retained bounded Changes state for the next Diff page; absent once fully delivered.
    /// Never serialized into a `PeerReply`; it exists only to resume the same owner cursor.
    diff_page: Option<snapshots::DiffPageState>,
    /// `true` exactly when `reply` holds a Diff page whose text was already composed (by the
    /// completed job or a prior expansion) but never yet handed to any caller. The first ready
    /// `ide.inspect` on such a detail must return that already-composed page unchanged instead of
    /// eagerly expanding past it; every later call then advances.
    diff_page_fresh: bool,
}

/// Retains one versioned provider delta until a later native post-hook rechecks its exact source.
struct NativeFeedback {
    /// Exact source observation for daemon-managed feedback; absent for a helper-owned snapshot.
    source: Option<SourceObservation>,
    /// Bounded fact/evidence/action rendering; source text and diagnostics are excluded.
    text: String,
    /// Native lifecycle epoch at which the provider result was accepted.
    native_epoch: u64,
    /// `true` once this exact fact was actually handed to a live caller inside a submitted
    /// Context/Inspect reply — never set merely because the owning job finished computing it.
    /// A completed job whose reply nobody has retrieved yet (still `Pending` to its caller, or
    /// lost to a deadline) leaves this `false`, so the fact remains eligible for exactly one
    /// later hook delivery. This never asserts the model read the text, only that this process
    /// handed it to the transport once; see `take_current_feedback`.
    inline_delivered: bool,
    /// Exact detail reference of the job that produced this fact.
    ///
    /// The per-binding slot is single-slot: a second Context job at the *same* native epoch can
    /// overwrite it with a different fact before any caller retrieves either one. Acknowledgement
    /// must name this exact producer, not just the epoch, so retrieving an older, already
    /// superseded detail can never mark the *current* (different) fact as delivered.
    producer: String,
    /// Bounded content identity of the underlying issue, used only to recognize a later,
    /// redundant Context job for the exact same unchanged issue; see `DeliveredIssue`.
    identity: DeliveredIssue,
}

/// Bounded content identity of one diagnostic issue, stable across repeated, unrelated
/// re-observation of unchanged bytes.
///
/// Deliberately excludes every volatile field that changes on mere re-observation without any
/// real change to the issue itself: the job's `detail_ref`, the source's per-read
/// `source_sequence`, and the native-hook epoch. It is never inferred by parsing rendered model
/// text; both producers build it from the same typed source digest and raw diagnostic messages
/// they already hold before rendering. Two identities are equal only when the observed source
/// path and content digest and the exact ordered diagnostic message set all match — any change to
/// source bytes, provider/document generation (which gates whether diagnostics attach at all), or
/// the issue set (added, removed or reworded messages, even at an unchanged count) yields a
/// different identity.
#[derive(Clone, Eq, PartialEq)]
struct DeliveredIssue {
    /// Exact registered path the issue was raised against.
    source_path: std::path::PathBuf,
    /// Content digest of the exact source bytes the issue was raised against.
    source_digest: [u8; 32],
    /// Content fingerprint of the exact ordered diagnostic message set.
    diagnostic_fingerprint: [u8; 32],
}

/// Retains daemon-derived scope, baseline and cache paths for later Claude helper jobs.
#[derive(Clone)]
pub(super) struct ClaudeBindingState {
    /// Durable current scope with replacement-safe root identity.
    pub(super) scope: HelperScope,
    /// Partial stored baseline or explicit unknown baseline.
    pub(super) baseline: HelperBaseline,
    /// Retained worktree cache path for each configured provider profile.
    pub(super) caches: Vec<(AcceptedProviderSettings, String)>,
}

/// Builds the daemon's single physical-effect admission controller with its fixed process limits.
///
/// Exactly one of these exists per daemon boot. Every physical-effect route — discovery, snapshots,
/// language providers and Claude's foreground helper claims — draws from this one budget, so the
/// limits below are the real global ceiling and not a per-route hint.
pub(super) fn admission_controller() -> crate::execution::AdmissionController {
    crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
        total_running: 16,
        per_owner_running: 2,
        per_owner_queued: 1,
        total_queued: 64,
        interactive_burst: 8,
    })
    .expect("fixed process limits")
}

/// Shared bounded transport-side bookkeeping; no lock survives an I/O await.
struct Ledger {
    /// FIFO ordinary jobs; explicit stop is prioritized at the front.
    queue: VecDeque<Job>,
    /// Retained results, never silently evicted to admit more work.
    details: BTreeMap<String, Detail>,
    /// Stable start requests under each immutable binding generation.
    starts: BTreeMap<(BindingRef, String), String>,
    /// One cancellation sender per currently active binding.
    cancellation: BTreeMap<BindingRef, watch::Sender<bool>>,
    /// Nonzero monotonic detail identifiers within this daemon boot.
    next: u64,
    /// One bounded monotonic invalidation counter per active binding.
    native_epoch: BTreeMap<BindingRef, u64>,
    /// At most one undelivered current delta per active binding.
    feedback: BTreeMap<BindingRef, NativeFeedback>,
    /// Identity of the single most recently delivered (inline-submitted or hook-consumed) issue
    /// per active binding, retained across replacement or removal of `feedback`'s pending slot so
    /// a later, redundant Context job for the exact same unchanged issue is never re-armed as a
    /// fresh undelivered fact. Cleared at the same points `feedback` is cleared.
    delivered: BTreeMap<BindingRef, DeliveredIssue>,
    /// Active Claude bindings available to mint post-activation helper jobs.
    claude: BTreeMap<BindingRef, ClaudeBindingState>,
}
impl Default for Ledger {
    /// Creates empty finite bookkeeping; no file or process work occurs.
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            details: BTreeMap::new(),
            starts: BTreeMap::new(),
            cancellation: BTreeMap::new(),
            next: 0,
            native_epoch: BTreeMap::new(),
            feedback: BTreeMap::new(),
            delivered: BTreeMap::new(),
            claude: BTreeMap::new(),
        }
    }
}

impl Ledger {
    /// Retains one newly produced fact unless this binding already received the same issue.
    ///
    /// `feedback` is the current producer's bounded fact. An equal delivered identity removes any
    /// older pending slot and returns `false`; a new identity replaces that slot and returns
    /// `true`. This does not mark a fact delivered: only a successful inline send or hook
    /// consumption may update `delivered`.
    fn retain_feedback(&mut self, binding: &BindingRef, feedback: NativeFeedback) -> bool {
        if self.delivered.get(binding) == Some(&feedback.identity) {
            self.feedback.remove(binding);
            false
        } else {
            self.feedback.insert(binding.clone(), feedback);
            true
        }
    }
}

/// A short read request serviced by the same worker even while provider warmup is pending.
struct Inspection {
    /// Freshly host-validated caller binding.
    binding: BindingRef,
    /// Opaque result handle supplied by the model; it does not confer ownership.
    reference: String,
    /// Current host-correlated sandbox metadata, never a cached prior permission observation.
    observed: ObservedSandboxState,
    /// Exact trusted attachment target used for current read-scope validation.
    target: LaunchTarget,
    /// Optional owner/path constraint for method-specific detail retrieval.
    expected: Option<(AssistanceTool, [u8; 32])>,
    /// Finite IPC caller waiting for the current authorized result.
    reply: oneshot::Sender<PeerReply>,
}

/// Cloneable state shared by ingress and the single worker task, never by independent worker loops.
struct Shared {
    /// Exact revocable host binding owner.
    bindings: Arc<Mutex<HostBindingGuard>>,
    /// Bounded queue, cancellation and detail ledger.
    ledger: Mutex<Ledger>,
    /// Wakes the worker for jobs, stops or native-change hints.
    notify: Notify,
    /// Immutable trusted restart configuration.
    launcher: LauncherConfig,
    /// Boot-unique opaque prefix prevents detail/SQLite operation collisions after restart.
    nonce: [u8; 32],
    /// True after daemon shutdown fences admission and requests owned cleanup.
    shutting_down: std::sync::atomic::AtomicBool,
    /// First provider cleanup failure retained until the shutdown caller observes it.
    shutdown_failure: Mutex<Option<FailureCode>>,
    /// The daemon's single finite physical-effect admission owner.
    ///
    /// It is shared rather than owned by the worker because Claude's foreground helper claims its
    /// capacity from the private helper socket task, off the worker's own queue. One controller is
    /// the whole point: ordinary worker work and Claude helper children contend for exactly the
    /// same configured global budget, and no second counter can widen it.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Replaceable fail-open sink receiving only sanitized typed telemetry facts.
    telemetry: Arc<dyn super::telemetry::EditTelemetry>,
}
impl Shared {
    /// Acquires a new transient binding use at one exact admission/return boundary.
    fn active(&self, binding: &BindingRef) -> Result<ActiveBindingUse, FailureCode> {
        self.bindings
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .consume_active(binding)
            .map_err(|_| FailureCode::Cancelled)
    }
    /// Stores a bounded result after its owner has performed current liveness/authority checks.
    fn complete(
        &self,
        reference: &str,
        reply: PeerReply,
        authority: Option<AuthorityStamp>,
        source: Option<SourceObservation>,
        native_epoch: u64,
    ) {
        let reply = reply
            .encode()
            .and_then(|value| PeerReply::decode(value.as_str()))
            .unwrap_or(PeerReply::Error {
                code: FailureCode::Internal,
            });
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.reply = reply;
            detail.authority = authority;
            detail.source = source;
            detail.native_epoch = native_epoch;
        }
    }
    /// Marks `binding`'s retained feedback as already submitted to a live caller, but only when
    /// three things hold: the retained fact is still the exact one produced by `reference` (never
    /// a different, later fact that overwrote the same single-slot binding entry), `reply` is the
    /// exact value that was actually handed to a still-live receiver, and that fact's rendered
    /// text still survives the real, final MCP/IPC fitting (`facade::render_reply`) applied to
    /// `reply` — the same shrink/encode boundary the real transport response goes through, so a
    /// trimmed or capped carrier is never mislabeled as delivered.
    ///
    /// This is the sole write side of cross-channel dedup: callers must invoke it only after
    /// confirming the reply actually reached a live receiver, never merely because a job finished
    /// or a reply merely encodes to *some* fitted value. A producer mismatch, closed receiver, or
    /// a carrier whose fitting dropped the fact is a no-op — it never resurrects, replaces or
    /// fabricates a fact.
    fn mark_feedback_inline_delivered(
        &self,
        binding: &BindingRef,
        reference: &str,
        reply: &PeerReply,
    ) {
        // First pass: cheaply confirm there is still a same-producer entry worth checking, and
        // take the exact fact text under the lock, before doing any rendering work outside it.
        let fact = {
            let Ok(ledger) = self.ledger.lock() else {
                return;
            };
            let Some(feedback) = ledger.feedback.get(binding) else {
                return;
            };
            if feedback.producer != reference {
                return;
            }
            feedback.text.clone()
        };
        // Rendering runs the real shrink loop; keep it off the ledger lock so a large reply never
        // holds up unrelated bindings.
        let survives = render_reply(reply.clone())
            .structured_content
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains(&fact));
        if !survives {
            return;
        }
        // Second pass: re-take the lock and recheck the exact same producer identity before
        // writing. Nothing awaited between the two locks, but another task (a concurrent hook
        // consuming this same entry, or a newer Context job overwriting the single slot) could
        // have mutated it meanwhile; a stale write here would either resurrect a fact the hook
        // path already consumed or wrongly stamp a different, newer fact as delivered.
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        let identity = {
            let Some(feedback) = ledger.feedback.get_mut(binding) else {
                return;
            };
            if feedback.producer != reference {
                return;
            }
            feedback.inline_delivered = true;
            feedback.identity.clone()
        };
        ledger.delivered.insert(binding.clone(), identity);
    }
    /// Retains or clears the bounded Diff pagination state for one same-binding detail reference.
    /// Never touches the serialized reply; only `serve_inspection` may advance or drop this state.
    /// A newly stashed page is marked "fresh" because its text (in `reply`) was already composed
    /// by the caller and not yet handed to any inspector. Refuses to retain a *new* page past the
    /// aggregate retained-evidence ceiling and reports that honestly through its `bool` result;
    /// it never evicts a page that is not being replaced by this exact call.
    fn set_diff_page(&self, reference: &str, page: Option<snapshots::DiffPageState>) -> bool {
        let Ok(mut ledger) = self.ledger.lock() else {
            return false;
        };
        if page.is_some() {
            let already_retaining = ledger
                .details
                .get(reference)
                .is_some_and(|detail| detail.diff_page.is_some());
            if !already_retaining {
                let retained = ledger
                    .details
                    .values()
                    .filter(|detail| detail.diff_page.is_some())
                    .count();
                if !admits_new_diff_page(retained) {
                    if let Some(detail) = ledger.details.get_mut(reference) {
                        detail.diff_page = None;
                        detail.diff_page_fresh = false;
                    }
                    return false;
                }
            }
        }
        let retained = page.is_some();
        if let Some(detail) = ledger.details.get_mut(reference) {
            detail.diff_page = page;
            detail.diff_page_fresh = retained;
        }
        retained
    }
}

/// Worst-case bytes one retained Diff page's evidence can hold: Workspace bounds per-path source
/// content to `MAX_SNAPSHOT_TOTAL_BYTES` and raw patch bytes to `MAX_SNAPSHOT_PATCH_BYTES`
/// separately, so a single `GitSnapshot` can approach their sum. Charging this fixed worst case
/// per retained page (rather than walking every path/patch on each admission check) keeps the
/// aggregate bound cheap and exact-enough: real usage is always at or under this charge.
const DIFF_PAGE_RETAINED_BYTES: usize = crate::workspace::git::snapshot::MAX_SNAPSHOT_TOTAL_BYTES
    + crate::workspace::git::snapshot::MAX_SNAPSHOT_PATCH_BYTES;
/// Aggregate ceiling for concurrently retained Diff pagination evidence across every detail,
/// independent of and tighter than the unrelated `details` count limit; a client that never pages
/// through its Diff details cannot pin unbounded memory just by leaving many of them retained.
const MAX_RETAINED_DIFF_PAGE_BYTES: usize = 16 * DIFF_PAGE_RETAINED_BYTES;

/// Decides whether one *additional* Diff page may be retained beside `retained` already-charged
/// pages, charging each the fixed worst case `DIFF_PAGE_RETAINED_BYTES`.
///
/// * `retained` — number of details currently holding a page; replacing one of those is not an
///   addition and never consults this rule.
///
/// Returns `true` only while the aggregate charge stays at or under
/// `MAX_RETAINED_DIFF_PAGE_BYTES`. Saturating arithmetic keeps an absurd count from wrapping into
/// a false admission. Pure and total: it performs no I/O and never evicts a live peer's page.
const fn admits_new_diff_page(retained: usize) -> bool {
    retained
        .saturating_add(1)
        .saturating_mul(DIFF_PAGE_RETAINED_BYTES)
        <= MAX_RETAINED_DIFF_PAGE_BYTES
}

/// Proves the exact aggregate retention cap and its refusal boundary, which `set_diff_page` reports
/// honestly instead of evicting another live peer's retained evidence.
#[test]
fn retained_diff_page_admission_caps_sixteen_worst_case_pages() {
    assert_eq!(DIFF_PAGE_RETAINED_BYTES, 9 * 1024 * 1024);
    assert_eq!(MAX_RETAINED_DIFF_PAGE_BYTES, 16 * 9 * 1024 * 1024);
    for (retained, admitted) in [
        (0, true),
        (1, true),
        (15, true),
        (16, false),
        (usize::MAX, false),
    ] {
        assert_eq!(
            admits_new_diff_page(retained),
            admitted,
            "retained={retained}"
        );
    }
}

/// Holds one worker task and its finite ingress channels; dropping it cancels the daemon-owned loop.
pub struct WorkerHandle {
    /// Shared finite state, including the exact binding owner.
    shared: Arc<Shared>,
    /// Inspections are separate from slow jobs so pending work never monopolizes IPC.
    inspect: mpsc::Sender<Inspection>,
    /// Consumed exactly once during daemon startup under Application's exclusive lock.
    receiver: Mutex<Option<mpsc::Receiver<Inspection>>>,
    /// Sole owned worker task; no duplicate boot is allowed.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Cooperative stop flag for the bounded startup fingerprint reader.
    startup_cancel: Arc<std::sync::atomic::AtomicBool>,
}
impl std::fmt::Debug for WorkerHandle {
    /// Omits all host, target, profile and result contents.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WorkerHandle(..)")
    }
}
impl Drop for WorkerHandle {
    /// Cancels owned work without inventing child-reap or admission-release evidence.
    fn drop(&mut self) {
        self.startup_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}
impl WorkerHandle {
    /// Returns whether this configured worker owns the exact trusted launcher attachment.
    ///
    /// The immutable launcher map is consulted without allocating, performing I/O, or changing a
    /// binding. Discovery-only dispatchers have no worker and retain their existing unavailable
    /// behavior.
    pub fn accepts_attachment(&self, attachment: &str) -> bool {
        self.shared.launcher.target(attachment).is_some()
    }

    /// Creates finite channels only; Store and Workspace are opened later under the daemon lock.
    pub fn new(
        bindings: Arc<Mutex<HostBindingGuard>>,
        launcher: LauncherConfig,
        nonce: [u8; 32],
        admission: Arc<Mutex<crate::execution::AdmissionController>>,
    ) -> Self {
        Self::new_with_telemetry(
            bindings,
            launcher,
            nonce,
            admission,
            Arc::new(super::telemetry::NoopEditTelemetry),
        )
    }

    /// Creates finite channels with a replaceable fail-open telemetry sink.
    ///
    /// The sink receives only typed sanitized facts and must return immediately. It cannot affect
    /// admission, authority, edit settlement, native fallback, or any returned result.
    pub fn new_with_telemetry(
        bindings: Arc<Mutex<HostBindingGuard>>,
        launcher: LauncherConfig,
        nonce: [u8; 32],
        admission: Arc<Mutex<crate::execution::AdmissionController>>,
        telemetry: Arc<dyn super::telemetry::EditTelemetry>,
    ) -> Self {
        let (inspect, receiver) = mpsc::channel(launcher.limits.queued);
        Self {
            shared: Arc::new(Shared {
                bindings,
                ledger: Mutex::new(Ledger::default()),
                notify: Notify::new(),
                launcher,
                nonce,
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                shutdown_failure: Mutex::new(None),
                admission,
                telemetry,
            }),
            inspect,
            receiver: Mutex::new(Some(receiver)),
            task: Mutex::new(None),
            startup_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
    /// Opens exactly one durable Workspace owner and observation schema for this daemon boot.
    pub async fn start(&self, runtime: &Path) -> Result<(), FailureCode> {
        let receiver = self
            .receiver
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .take()
            .ok_or(FailureCode::Internal)?;
        let shared = self.shared.clone();
        let runtime = runtime.to_path_buf();
        let (ready, wait) = oneshot::channel();
        let cancel = self.startup_cancel.clone();
        let task = tokio::spawn(async move {
            let verification = shared.clone();
            if !matches!(
                tokio::task::spawn_blocking(move || verification
                    .launcher
                    .verify_executables(&cancel))
                .await,
                Ok(Ok(()))
            ) {
                let _ = ready.send(Err(FailureCode::ExecutionProfile));
                return;
            }

            let store = match Store::open_with_backup_root(
                &runtime.join("state.sqlite"),
                &runtime.join("backups"),
                EffectiveConfig::defaults().store(),
            ) {
                Ok(store) => store,
                Err(_) => {
                    let _ = ready.send(Err(FailureCode::Internal));
                    return;
                }
            };
            // ponytail: one process-lifetime Store leak per daemon boot; replace with Arc ownership
            // only if in-process daemon restart becomes a supported lifecycle.
            let store: &'static Store = Box::leak(Box::new(store));
            let workspace = match DurableWorkspace::open(store).await {
                Ok(owner) => owner,
                Err(_) => {
                    let _ = ready.send(Err(FailureCode::WorkspaceActivation));
                    return;
                }
            };
            let observations = WorkspaceStore::new(store);
            if !matches!(
                observations.install_schema().await,
                Ok(MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. })
            ) {
                let _ = ready.send(Err(FailureCode::SourceUnavailable));
                return;
            }
            let edits = EditReceiptStore::new(store);
            if !matches!(
                edits.install_schema().await,
                Ok(MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. })
            ) {
                let _ = ready.send(Err(FailureCode::Internal));
                return;
            }
            let _ = ready.send(Ok(()));
            Worker {
                admission: shared.admission.clone(),
                shared,
                workspace,
                observations,
                edits,
                prepared_edits: BTreeMap::new(),
                grants: BTreeMap::new(),
                pending_revocations: std::collections::BTreeSet::new(),
                registered: BTreeMap::new(),
                baselines: BTreeMap::new(),
                source_sequence: 0,
                uncertain: std::collections::BTreeSet::new(),
                uncertain_snapshots: Vec::new(),
                runtime,
                providers: providers::Providers::new(),
            }
            .run(receiver)
            .await;
        });
        *self.task.lock().map_err(|_| FailureCode::Internal)? = Some(task);
        wait.await.map_err(|_| FailureCode::Internal)?
    }
    /// Fences admission, cancels every binding, and waits for the sole worker to reap providers.
    pub async fn shutdown(&self) -> Result<(), FailureCode> {
        self.startup_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            for sender in ledger.cancellation.values() {
                let _ = sender.send(true);
            }
            ledger.queue.clear();
            ledger.details.clear();
            ledger.starts.clear();
            ledger.feedback.clear();
            ledger.delivered.clear();
        }
        self.shared.notify.notify_one();
        let task = self.task.lock().map_err(|_| FailureCode::Internal)?.take();
        let Some(mut task) = task else {
            return Ok(());
        };
        match tokio::time::timeout(Duration::from_secs(39), &mut task).await {
            Ok(Ok(())) => self
                .shared
                .shutdown_failure
                .lock()
                .map_err(|_| FailureCode::Internal)?
                .take()
                .map_or(Ok(()), Err),
            Ok(Err(_)) => Err(FailureCode::Internal),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(FailureCode::Deadline)
            }
        }
    }
    /// Enqueues or resolves the exact query, returning pending without waiting for provider warmup.
    pub async fn submit(
        &self,
        invocation: ValidatedInvocation,
        observed: Option<ObservedSandboxState>,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
    ) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        let Some(current) = observed.clone() else {
            return PeerReply::Error {
                code: FailureCode::SandboxState,
            };
        };
        let expected = Some((tool, selection(&parameters)));
        if let Some(reference) = parameters.get("detail_ref").and_then(Value::as_str) {
            return self
                .inspect(binding, reference.to_owned(), current, attachment, expected)
                .await;
        }
        let Some(target) = self.shared.launcher.target(attachment).cloned() else {
            return PeerReply::Error {
                code: FailureCode::LauncherConfiguration,
            };
        };
        match admit_initial_inspection(&self.inspect, || {
            self.enqueue(
                invocation,
                observed,
                tool,
                parameters,
                attachment,
                None,
                JobInput::Managed,
            )
        }) {
            Ok((reference, permit)) => {
                self.inspect_reserved(binding, reference, current, target, expected, permit)
                    .await
            }
            Err(code) => PeerReply::Error { code },
        }
    }

    /// Signals revocation immediately and waits only for the bounded exact stop result.
    /// The caller has already revoked the binding; this method never restores its authority.
    pub async fn stop(&self, invocation: ValidatedInvocation, attachment: &str) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            if let Some(sender) = ledger.cancellation.remove(&binding) {
                let _ = sender.send(true);
            }
            ledger
                .queue
                .retain(|job| job.invocation.binding_ref() != &binding);
            ledger.details.retain(|_, detail| detail.binding != binding);
            ledger.starts.retain(|(owner, _), _| owner != &binding);
            ledger.native_epoch.remove(&binding);
            ledger.feedback.remove(&binding);
            ledger.delivered.remove(&binding);
        }
        self.shared.notify.notify_one();
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            None,
            AssistanceTool::Stop,
            serde_json::json!({}),
            attachment,
            Some(send),
            JobInput::Managed,
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(Duration::from_millis(800), wait).await {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
            },
        }
    }

    /// Routes one positively settled Claude helper operation through the sole worker queue.
    ///
    /// This is the only entry by which helper evidence can reach durable state, and it accepts
    /// only a [`SettledClaudeOperation`], which cannot be built from wire data. It adds only one
    /// job on the existing queue; Start mints authority/baseline, Context records source metadata,
    /// and Diff publishes only helper-composed current evidence.
    ///
    /// Start delivery is idempotent by the existing `(binding, activation_id)` ledger. Context,
    /// Diff and Edit consume one settled helper token each and still require current authority;
    /// Edit additionally requires its already-retained durable Changes prepare token.
    pub async fn complete_claude(
        &self,
        invocation: ValidatedInvocation,
        attachment: &str,
        settled: SettledClaudeOperation,
    ) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        let parameters = settled.job().parameters.clone();
        let tool = match settled.operation() {
            HelperOperation::Start => AssistanceTool::Start,
            HelperOperation::Context => AssistanceTool::Context,
            HelperOperation::Diff => AssistanceTool::Diff,
            HelperOperation::Edit => AssistanceTool::Edit,
        };
        // Repeated inspection of an already completed activation is retrieval, never a second
        // activation: answer from the retained detail rather than re-entering the queue.
        if tool == AssistanceTool::Start {
            let Some(activation) = parameters.get("activation_id").and_then(Value::as_str) else {
                return PeerReply::Error {
                    code: FailureCode::Internal,
                };
            };
            if let Some(retained) = self.retained_start(&binding, activation) {
                return retained;
            }
        }
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            None,
            tool,
            parameters,
            attachment,
            Some(send),
            JobInput::Claude(Box::new(settled)),
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(
            Duration::from_millis(self.shared.launcher.limits.operation_ms),
            wait,
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
            },
        }
    }

    /// Durably prepares one Claude edit before the foreground-helper ticket is exposed.
    ///
    /// `HookObserved` is an internal readiness signal consumed only by the assembly layer. Any
    /// duplicate, unavailable or closed edit result is returned directly and no helper is minted.
    pub async fn prepare_claude_edit(
        &self,
        invocation: ValidatedInvocation,
        parameters: Value,
        attachment: &str,
    ) -> PeerReply {
        let cleanup_invocation = invocation.clone();
        let cleanup_parameters = parameters.clone();
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            None,
            AssistanceTool::Edit,
            parameters,
            attachment,
            Some(send),
            JobInput::ClaudeEditPrepare,
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(
            Duration::from_millis(self.shared.launcher.limits.operation_ms),
            wait,
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            _ => {
                // The prepare job remains ordered before this terminal job, so a timed-out caller
                // cannot strand a durable prepared receipt or consume Claude ticket capacity.
                self.complete_claude_edit_terminal(
                    cleanup_invocation,
                    cleanup_parameters,
                    attachment,
                    ChangesEditOutcome::DeadlineNoEffect,
                )
                .await
            }
        }
    }

    /// Settles an edit from daemon-owned helper lifecycle proof without accepting helper JSON.
    pub async fn complete_claude_edit_terminal(
        &self,
        invocation: ValidatedInvocation,
        parameters: Value,
        attachment: &str,
        outcome: ChangesEditOutcome,
    ) -> PeerReply {
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            None,
            AssistanceTool::Edit,
            parameters,
            attachment,
            Some(send),
            JobInput::ClaudeEditTerminal(outcome),
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(
            Duration::from_millis(self.shared.launcher.limits.operation_ms),
            wait,
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
            },
        }
    }

    /// Returns daemon-selected exact Context facts for one same-binding Claude edit ticket.
    pub fn claude_edit_source(
        &self,
        binding: &BindingRef,
        reference: &str,
        path: &str,
    ) -> Option<super::claude_worker::HelperEditSource> {
        self.shared.active(binding).ok()?;
        let ledger = self.shared.ledger.lock().ok()?;
        let detail = ledger.details.get(reference)?;
        if detail.binding != *binding
            || detail.selection.0 != AssistanceTool::Context
            || !matches!(
                detail.reply,
                PeerReply::Complete {
                    kind: ResultKind::Context,
                    ..
                }
            )
        {
            return None;
        }
        let source = detail.source.as_ref()?;
        if source.path().to_str() != Some(path) {
            return None;
        }
        Some(super::claude_worker::HelperEditSource {
            path: path.to_owned(),
            present: source.bytes().is_some(),
            digest: source.bytes().map(|bytes| *bytes.digest()),
            length: source.bytes().map_or(0, |bytes| bytes.length()),
            sequence: source.sequence(),
            source_revision: source.source_revision().as_str().to_owned(),
            observation_ref: source.reference().as_str().to_owned(),
        })
    }

    /// Returns the retained result of an already completed start for this binding and activation.
    ///
    /// Returns `None` while no start exists for the pair, or while its retained reply is still the
    /// original `Pending` placeholder, so a first call proceeds to the queue exactly once.
    fn retained_start(&self, binding: &BindingRef, activation: &str) -> Option<PeerReply> {
        let ledger = self.shared.ledger.lock().ok()?;
        let reference = ledger
            .starts
            .get(&(binding.clone(), activation.to_owned()))?;
        let detail = ledger.details.get(reference)?;
        matches!(detail.reply, PeerReply::Pending { .. })
            .then_some(())
            .map_or_else(|| Some(detail.reply.clone()), |()| None)
    }

    /// Asks the sole worker for a same-binding, current-profile, durably authorized result.
    pub async fn inspect(
        &self,
        binding: BindingRef,
        reference: String,
        observed: ObservedSandboxState,
        attachment: &str,
        expected: Option<(AssistanceTool, [u8; 32])>,
    ) -> PeerReply {
        let Some(target) = self.shared.launcher.target(attachment).cloned() else {
            return PeerReply::Error {
                code: FailureCode::LauncherConfiguration,
            };
        };
        let permit = match reserve_inspection(&self.inspect) {
            Ok(permit) => permit,
            Err(code) => return PeerReply::Error { code },
        };
        self.inspect_reserved(binding, reference, observed, target, expected, permit)
            .await
    }

    /// Publishes one fully built inspection through a permit that already owns channel capacity.
    async fn inspect_reserved(
        &self,
        binding: BindingRef,
        reference: String,
        observed: ObservedSandboxState,
        target: LaunchTarget,
        expected: Option<(AssistanceTool, [u8; 32])>,
        permit: mpsc::OwnedPermit<Inspection>,
    ) -> PeerReply {
        let (reply, wait) = oneshot::channel();
        permit.send(Inspection {
            binding,
            reference,
            observed,
            target,
            expected,
            reply,
        });
        wait.await.unwrap_or(PeerReply::Error {
            code: FailureCode::Internal,
        })
    }

    /// Returns the trusted immutable target mapped to one opaque attachment, if any.
    ///
    /// The mapping comes only from restart-loaded launcher configuration; no model argument,
    /// working directory, PID or timing contributes to it.
    pub fn target(&self, attachment: &str) -> Option<LaunchTarget> {
        self.shared.launcher.target(attachment).cloned()
    }

    /// Returns the daemon-derived current scope and retained cache paths for one Claude binding.
    ///
    /// Missing state means Start has not committed, Stop removed it, or the generation is stale.
    /// This is pure bounded bookkeeping and performs no Git, source, provider or Store I/O.
    pub(super) fn claude_state(&self, binding: &BindingRef) -> Option<ClaudeBindingState> {
        self.shared.active(binding).ok()?;
        self.shared.ledger.lock().ok()?.claude.get(binding).cloned()
    }

    /// Returns the shared validated finite worker limits for this daemon boot.
    pub fn limits(&self) -> super::launcher::ProductLimits {
        self.shared.launcher.limits
    }

    /// Invalidates cached results on native hints; reads wait for a current MCP sandbox observation.
    pub fn native_hint(&self, binding: BindingRef) {
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            let epoch = ledger.native_epoch.entry(binding).or_default();
            *epoch = epoch.saturating_add(1);
        }
        self.shared.notify.notify_one();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.shared
                .telemetry
                .native_fallback(super::telemetry::NativeFallbackReason::NativeSelected);
        }));
    }

    /// Coalesces managed-Codex registered-path reconciliation at a trusted read boundary.
    ///
    /// `invalidate` is true for new Context/Diff captures, which must fence older retained output;
    /// Inspect instead performs its existing direct current-byte checks and leaves a hint for the
    /// next worker job without invalidating a pending result merely because it was polled.
    pub fn managed_read_boundary(&self, binding: BindingRef, invalidate: bool) {
        if self.shared.bindings.lock().is_ok_and(|mut guard| {
            guard
                .request_registered_path_reconciliation(&binding)
                .is_ok()
        }) && invalidate
        {
            self.native_hint(binding);
        }
    }

    /// Returns and consumes one same-binding delta only after a newer native epoch and source recheck.
    ///
    /// Missing, stopped, unchanged-epoch, stale, unversioned, or already-inline-delivered feedback
    /// returns `None`. A fact already handed to a live caller in a submitted Context/Inspect reply
    /// is still removed here (so it can never resurrect on a later hook) but its text is withheld,
    /// because it already reached the transport once. The check performs no provider execution and
    /// does not interpret a Claude permission mode as authority.
    pub async fn take_current_feedback(&self, binding: BindingRef) -> Option<String> {
        self.shared.active(&binding).ok()?;
        let feedback = {
            let mut ledger = self.shared.ledger.lock().ok()?;
            let current_epoch = ledger.native_epoch.get(&binding).copied().unwrap_or(0);
            let feedback = ledger.feedback.remove(&binding)?;
            let feedback = (current_epoch > feedback.native_epoch
                && !feedback.inline_delivered
                && feedback.source.as_ref().is_none_or(source_matches))
            .then_some(feedback)?;
            // A hook consumption counts toward the same dedup state as an inline submission: a
            // later, redundant Context job reproducing this exact unchanged issue must not
            // re-arm it, even though this single-slot entry is gone.
            ledger
                .delivered
                .insert(binding.clone(), feedback.identity.clone());
            feedback
        };
        self.shared.active(&binding).ok()?;
        Some(feedback.text)
    }

    /// Atomically bounds and publishes one operation, without file, database or child-process I/O.
    #[allow(clippy::too_many_arguments)]
    fn enqueue(
        &self,
        invocation: ValidatedInvocation,
        observed: Option<ObservedSandboxState>,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
        stop_reply: Option<oneshot::Sender<PeerReply>>,
        input: JobInput,
    ) -> Result<String, FailureCode> {
        if self
            .shared
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(FailureCode::Internal);
        }
        if !self
            .task
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Err(FailureCode::Internal);
        }
        let target = self
            .shared
            .launcher
            .target(attachment)
            .cloned()
            .ok_or(FailureCode::LauncherConfiguration)?;
        let binding = invocation.binding_ref().clone();
        let mut ledger = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?;
        let start = if tool == AssistanceTool::Start {
            Some((
                binding.clone(),
                parameters["activation_id"]
                    .as_str()
                    .ok_or(FailureCode::Internal)?
                    .to_owned(),
            ))
        } else {
            None
        };
        if let Some(key) = &start
            && let Some(reference) = ledger.starts.get(key)
        {
            return Ok(reference.clone());
        }
        let queue_cap =
            self.shared.launcher.limits.queued + if tool == AssistanceTool::Stop { 64 } else { 0 };
        if ledger.queue.len() >= queue_cap {
            return Err(FailureCode::Capacity);
        }
        if tool != AssistanceTool::Stop
            && ledger.details.len() >= self.shared.launcher.limits.details
        {
            return Err(FailureCode::Capacity);
        }
        ledger.next = ledger.next.checked_add(1).ok_or(FailureCode::Capacity)?;
        let reference = format!(
            "{}-{}",
            blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
            ledger.next
        );
        let cancel = if tool == AssistanceTool::Stop {
            watch::channel(false).1
        } else {
            ledger
                .cancellation
                .entry(binding.clone())
                .or_insert_with(|| watch::channel(false).0)
                .subscribe()
        };
        if tool != AssistanceTool::Stop {
            ledger.details.insert(
                reference.clone(),
                Detail {
                    binding,
                    reply: PeerReply::Pending {
                        detail_ref: reference.clone(),
                        // Daemon-executed work needs no foreground helper instruction.
                        helper: None,
                    },
                    selection: (tool, selection(&parameters)),
                    authority: None,
                    source: None,
                    native_epoch: 0,
                    diff_page: None,
                    diff_page_fresh: false,
                },
            );
        }
        if let Some(key) = start {
            ledger.starts.insert(key, reference.clone());
        }
        let job = Job {
            input,
            reference: reference.clone(),
            invocation,
            observed,
            tool,
            parameters,
            target,
            deadline: tokio::time::Instant::now()
                + Duration::from_millis(self.shared.launcher.limits.operation_ms),
            cancel,
            stop_reply,
            native_epoch: 0,
        };
        if tool == AssistanceTool::Stop {
            ledger.queue.push_front(job);
        } else {
            ledger.queue.push_back(job);
        }
        drop(ledger);
        self.shared.notify.notify_one();
        Ok(reference)
    }
}

/// Bounds the in-memory pending-revocation set so an unbounded stream of failed durable revokes
/// cannot grow it. A full set refuses to record a further pending binding rather than evicting one;
/// the caller still learns the failure, and a daemon restart boot-fences every old grant anyway.
const MAX_PENDING_REVOCATIONS: usize = 64;

/// One boot's sequential durable owner; provider operations may be interrupted by inspection service.
struct Worker<'a> {
    /// Shared bounded ingress and liveness state.
    shared: Arc<Shared>,
    /// Exactly one boot-fenced authority owner, opened once above.
    workspace: DurableWorkspace<'a>,
    /// Workspace-owned persistence for registered source observations.
    observations: WorkspaceStore<'a>,
    /// Changes-owned durable one-file edit receipts sharing Application's sole Store owner.
    edits: EditReceiptStore<'a>,
    /// Newly prepared Claude requests awaiting exact helper or no-effect expiry settlement.
    prepared_edits: BTreeMap<(BindingRef, String), crate::changes::edit::PreparedEdit>,
    /// Recoverable committed activation receipts, at most one for each live host binding.
    grants: BTreeMap<BindingRef, StartReceipt>,
    /// Bindings whose provider settlement succeeded but whose durable revoke failed, so their
    /// receipt, caches and registrations are deliberately retained for a bounded cleanup-only
    /// retry. It never carries a physical-process uncertainty, which stays in `uncertain`.
    pending_revocations: std::collections::BTreeSet<BindingRef>,
    /// Only explicitly requested paths are polled; no directory scanning is performed.
    registered: BTreeMap<BindingRef, std::collections::BTreeSet<std::path::PathBuf>>,
    /// Durable partial activation baselines retained for same-binding diff provenance.
    baselines: BTreeMap<BindingRef, crate::workspace::git::BaselineContext>,
    /// Boot-unique source observation operation sequence.
    source_sequence: u64,
    /// Finite physical-effect admission, shared by discovery, snapshots and language providers.
    ///
    /// This is a handle to the daemon's single [`Shared::admission`] controller, not a private
    /// second one, so a Claude helper claim taken on the helper socket task removes capacity this
    /// worker can no longer grant, and vice versa.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Bindings with uncertain physical/durable completion cannot report successful cleanup.
    uncertain: std::collections::BTreeSet<BindingRef>,
    /// Retains private scratch files when a child has no positive reap evidence. Entries are never
    /// deleted automatically: without a positive reap we cannot prove the child is no longer
    /// writing, so quarantine is the only honest outcome. This list has no fixed capacity today;
    /// its operational ceiling is one entry per genuinely uncertain reap for this boot, expected to
    /// be rare, bounded in practice by the daemon's own physical-admission limits rather than by an
    /// explicit cap on this field.
    uncertain_snapshots: Vec<crate::workspace::git::snapshot::SnapshotIntent>,
    /// Private runtime namespace for current-boot provider sockets.
    runtime: std::path::PathBuf,
    /// Exact provider/backend/view ownership and generations.
    providers: providers::Providers,
}
impl<'a> Worker<'a> {
    /// Processes one slow operation at a time; inspections run on an independently scheduled task
    /// (see `inspection_loop`) so a non-yielding poll of the current operation cannot starve
    /// `ide.inspect`. Shutdown first cancels the current operation, allowing its Rust or forwarder
    /// child to reap, then this loop closes retained providers before returning.
    async fn run(mut self, inspections: mpsc::Receiver<Inspection>)
    where
        'a: 'static,
    {
        let inspector = tokio::spawn(inspection_loop(
            self.workspace.clone(),
            self.shared.clone(),
            inspections,
        ));
        let _inspector = AbortOnDrop(inspector);
        loop {
            if self
                .shared
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                if let Err(code) = self.close_all_providers().await
                    && let Ok(mut failure) = self.shared.shutdown_failure.lock()
                {
                    *failure = Some(code);
                }
                return;
            }
            let shared = self.shared.clone();
            let wake = shared.notify.notified();
            let job = shared
                .ledger
                .lock()
                .ok()
                .and_then(|mut ledger| ledger.queue.pop_front());
            match job {
                Some(job) => self.perform(job).await,
                None => wake.await,
            }
        }
    }
    /// Rechecks queued liveness, executes only the selected owner operation, and fences every result.
    async fn perform(&mut self, mut job: Job) {
        let binding = job.invocation.binding_ref().clone();
        job.native_epoch = self
            .shared
            .ledger
            .lock()
            .ok()
            .and_then(|ledger| ledger.native_epoch.get(&binding).copied())
            .unwrap_or(0);
        let result = if job.tool == AssistanceTool::Stop {
            self.revoke(&binding, &job.reference).await
        } else if matches!(job.input, JobInput::ClaudeEditTerminal(_)) {
            // A terminal receipt cleanup is safe after stop: it dispatches no helper or target
            // write and must not be blocked by the binding cancellation that triggered cleanup.
            self.settle_claude_edit_terminal(&mut job).await
        } else if *job.cancel.borrow() || self.shared.active(&binding).is_err() {
            Err(FailureCode::Cancelled)
        } else if tokio::time::Instant::now() >= job.deadline {
            Err(FailureCode::Deadline)
        } else {
            self.reconcile_hints(&job).await;
            if tokio::time::Instant::now() >= job.deadline {
                Err(FailureCode::Deadline)
            } else {
                match job.tool {
                    AssistanceTool::Start if matches!(job.input, JobInput::Claude(_)) => {
                        self.activate_claude(&mut job).await
                    }
                    AssistanceTool::Context if matches!(job.input, JobInput::Claude(_)) => {
                        self.context_claude(&mut job).await
                    }
                    AssistanceTool::Diff if matches!(job.input, JobInput::Claude(_)) => {
                        self.diff_claude(&mut job).await
                    }
                    AssistanceTool::Edit if matches!(job.input, JobInput::Managed) => {
                        self.edit(&mut job).await
                    }
                    AssistanceTool::Edit if matches!(job.input, JobInput::ClaudeEditPrepare) => {
                        self.prepare_claude_edit_job(&mut job).await
                    }
                    AssistanceTool::Edit if matches!(job.input, JobInput::Claude(_)) => {
                        self.edit_claude(&mut job).await
                    }
                    AssistanceTool::Start => self.activate(&mut job).await,
                    AssistanceTool::Context => self.context(&mut job).await,
                    AssistanceTool::Diff => self.diff(&mut job).await,
                    _ => Err(FailureCode::Internal),
                }
            }
        };
        let (reply, authority, source) = match result {
            Ok(result) => result,
            Err(code) => (PeerReply::Error { code }, None, None),
        };
        if let PeerReply::Edit { result } = &reply {
            let lifetime = Duration::from_millis(self.shared.launcher.limits.operation_ms);
            let started = job.deadline.checked_sub(lifetime).unwrap_or(job.deadline);
            let duration_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let diagnostics = if result.outcome.has_post_source() && source.is_some() {
                super::telemetry::EditDiagnosticState::Refreshed
            } else if result.outcome.has_post_source()
                || result.outcome == ChangesEditOutcome::OutcomeUnknown
            {
                super::telemetry::EditDiagnosticState::Unknown
            } else {
                super::telemetry::EditDiagnosticState::NotApplicable
            };
            let bytes = serde_json::to_vec(result).map_or(0, |value| value.len());
            let fact = super::telemetry::EditTelemetryFact {
                outcome: result.outcome,
                duration_ms,
                diagnostics,
                output_size: super::telemetry::output_size(bytes),
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.shared.telemetry.edit_completed(fact);
            }));
        }
        self.shared.complete(
            &job.reference,
            reply.clone(),
            authority,
            source,
            job.native_epoch,
        );
        if let Some(sender) = job.stop_reply.take() {
            // The oneshot send is the actual submission boundary for this synchronous-wait path
            // (Claude Start/Context/Diff/Stop): it only succeeds while the caller's own `wait`
            // has not already been dropped by its deadline. Only a reply that both reached a live
            // receiver and still carries its fact after `mark_feedback_inline_delivered` traces
            // the same final MCP/IPC fitting the real transport applies may be marked delivered;
            // a closed receiver or a carrier that fitting trimmed leaves the fact eligible for
            // exactly one later hook delivery instead of being marked as already submitted.
            let is_context = matches!(
                reply,
                PeerReply::Complete {
                    kind: ResultKind::Context,
                    ..
                }
            );
            let mark_reply = is_context.then(|| reply.clone());
            if sender.send(reply).is_ok()
                && let Some(mark_reply) = mark_reply
            {
                self.shared
                    .mark_feedback_inline_delivered(&binding, &job.reference, &mark_reply);
            }
        }
    }
    /// Runs only the fixed catalog-admitted discovery commands, settling each child before parsing.
    async fn activate(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        use crate::execution::{
            DiscoverWorktreeRequest, DiscoveryOperationRef, GitDiscoveryPolicy, GitDiscoveryQuery,
        };
        let binding = job.invocation.binding_ref().clone();
        let observed = job.observed.clone().ok_or(FailureCode::SandboxState)?;
        let operation = DiscoveryOperationRef::new(format!("discover-{}", job.reference))
            .map_err(|_| FailureCode::Internal)?;
        let policy = GitDiscoveryPolicy::new(
            job.target.git.path.clone(),
            self.shared.launcher.limits.output_bytes,
            job.target.allow_disabled_host,
        )
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let mut evidence = Vec::with_capacity(3);
        for query in [
            GitDiscoveryQuery::ShowTopLevel,
            GitDiscoveryQuery::GitCommonDir,
            GitDiscoveryQuery::WorktreeListPorcelainZ,
        ] {
            let request = DiscoverWorktreeRequest::from_active_observation(
                self.shared.active(&binding)?,
                observed.clone(),
                job.target.candidate.clone().into_os_string(),
                operation.clone(),
            )
            .map_err(|_| FailureCode::SandboxState)?;
            let request = request
                .validate_query(query, &policy, &job.target.catalog)
                .map_err(|_| FailureCode::ExecutionProfile)?;
            if *job.cancel.borrow() {
                return Err(FailureCode::Cancelled);
            }
            if tokio::time::Instant::now() >= job.deadline {
                return Err(FailureCode::Deadline);
            }
            let active = self.shared.active(&binding)?;
            let lease = self.admit(&binding)?;
            let mut child = match request.spawn(lease, active, &job.target.codex.path) {
                Ok(child) => child,
                Err(error) => return Err(self.spawn_failure(error, &binding)),
            };
            let remaining = job
                .deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(60));
            let interrupted = tokio::select! {result=child.wait(remaining)=>result.is_err(),_=job.cancel.changed()=>true};
            let result = if interrupted {
                child
                    .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
                    .await
            } else {
                child
                    .reap(Duration::from_millis(500), Duration::from_millis(100))
                    .await
            };
            let completed = match result {
                Ok(completed) => completed,
                Err(_) => {
                    self.uncertain.insert(binding.clone());
                    return Err(FailureCode::Deadline);
                }
            };
            self.admission()
                .release_reaped(completed.settlement)
                .map_err(|_| FailureCode::Internal)?;
            if interrupted {
                return Err(if *job.cancel.borrow() {
                    FailureCode::Cancelled
                } else {
                    FailureCode::Deadline
                });
            }
            self.shared.active(&binding)?;
            evidence.push(completed.evidence);
        }
        let discovered = crate::workspace::git::discovery::validate_discovery(
            &job.target.candidate,
            &operation,
            &evidence,
        )
        .map_err(|error| match error {
            crate::workspace::git::GitError::UnsupportedDiscoveryGit => {
                FailureCode::ExecutionProfile
            }
            _ => FailureCode::WorkspaceActivation,
        })?;
        self.shared.active(&binding)?;
        let tree = self
            .workspace
            .resolve_worktree(
                discovered.root().to_path_buf(),
                discovered.repository_root().to_path_buf(),
                discovered.common_dir().to_path_buf(),
            )
            .await
            .map_err(|_| FailureCode::WorkspaceActivation)?;
        self.reconcile_pending_revocations(&tree, job.invocation.actor_id())
            .await;
        let mut identity = blake3::Hasher::new();
        identity.update(&binding.fingerprint());
        identity.update(
            job.parameters["activation_id"]
                .as_str()
                .ok_or(FailureCode::Internal)?
                .as_bytes(),
        );
        let operation = identity.finalize().to_hex().to_string();
        let request = crate::workspace::authority::ActivationRequest::new(
            operation,
            job.invocation.clone(),
            self.shared.active(&binding)?,
            tree,
        )
        .map_err(|_| FailureCode::WorkspaceActivation)?;
        // Do not cancel an in-flight durable commit: preserve its recoverable receipt before fencing output.
        let receipt = match self.workspace.activate(request).await {
            Ok(receipt) => receipt,
            Err(
                crate::workspace::durable::DurableError::OperationConflict
                | crate::workspace::durable::DurableError::Authority(
                    crate::workspace::authority::AuthorityError::WorktreeOwned
                    | crate::workspace::authority::AuthorityError::ActorAlreadyOwnsWorktree,
                ),
            ) => {
                return Err(FailureCode::Conflict);
            }
            Err(
                crate::workspace::durable::DurableError::Application(_)
                | crate::workspace::durable::DurableError::CorruptState,
            ) => {
                self.uncertain.insert(binding.clone());
                return Err(FailureCode::WorkspaceActivation);
            }
            Err(_) => return Err(FailureCode::WorkspaceActivation),
        };
        let activation_operation = receipt.operation().to_owned();
        self.grants.insert(binding.clone(), receipt);
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(error) => {
                if let Ok(mut guard) = self.shared.bindings.lock() {
                    let _ = guard.stop_binding(&binding);
                }
                return Err(self
                    .settle_revocation(&binding)
                    .await
                    .err()
                    .unwrap_or(error));
            }
        };
        let baseline = self
            .capture_activation_baseline(job, &authority, &activation_operation)
            .await;
        let launches = job.target.providers.clone();
        let managed_sandbox = providers::managed_sandbox_from_job(job);
        let rights = providers::effective_rights_from_job(job)?;
        // A second concurrent actor on the same physical worktree cannot share a single-owner
        // namespace: fail its activation with the finite reason and roll its own grant back, so the
        // actor that already owns the cache keeps running and can hand off after it stops.
        if let Err(code) = self.retain_worktree_caches(
            &binding,
            &authority,
            &launches,
            managed_sandbox,
            true,
            &rights,
        ) {
            if let Ok(mut guard) = self.shared.bindings.lock() {
                let _ = guard.stop_binding(&binding);
            }
            return Err(self.settle_revocation(&binding).await.err().unwrap_or(code));
        }
        self.shared.active(&binding)?;
        let baseline = match baseline {
            Ok(baseline) => {
                let description = format!(
                    "partial ({:?}; durable capture {})",
                    baseline.window(),
                    baseline.capture_digest().is_some()
                );
                self.baselines.insert(binding.clone(), baseline);
                description
            }
            Err(_) => "unknown (durable capture unavailable)".to_owned(),
        };
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text: format!(
                    "Workspace activated; authority_epoch: {}; baseline: {baseline}; worktree_cache: retained. Provider readiness is not implied.",
                    authority.epoch(),
                ),
                detail_ref: Some(job.reference.clone()),
                truncated: false,
            },
            Some(authority),
            None,
        ))
    }

    /// Activates a worktree from settled Claude helper evidence, without daemon Git or source work.
    ///
    /// The daemon interprets only bytes: it rebuilds the closed `GitDiscoveryEvidence` triple
    /// through its validating constructor under the operation identity stored in the daemon ticket,
    /// then runs the *pure* discovery parser. It never executes Git, reads repository source or
    /// reads a Git administrative file on this route; the helper already performed the full native
    /// and administrative validation before reporting `Complete`. `DurableWorkspace` still performs
    /// its own descriptor-only directory identity checks, and it alone mints the durable nonce,
    /// native key and incarnation.
    ///
    /// Liveness is reconsumed before and after every await, and immediately before the durable
    /// commit. A commit that raced stop keeps its receipt in `grants` for recoverable revocation
    /// and publishes no success.
    ///
    /// The helper also supplies the fixed baseline Git reads. They become a durable partial baseline
    /// only after activation and a fresh authority check; invalid evidence leaves baseline coverage
    /// unknown without undoing an already-committed grant.
    ///
    /// Returns [`FailureCode::Conflict`] when another actor already owns this worktree, leaving the
    /// first owner's activation usable, and [`FailureCode::WorkspaceActivation`] when the evidence
    /// cannot be parsed or the durable row cannot be committed.
    async fn activate_claude(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let JobInput::Claude(settled) = &job.input else {
            return Err(FailureCode::Internal);
        };
        // Operation kind and generation ownership are rechecked here, not assumed from the token.
        if settled.operation() != HelperOperation::Start
            || settled.binding().fingerprint() != binding.fingerprint()
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        let Some(super::claude_worker::HelperPayload::Start { baseline }) =
            settled.result().payload.as_ref()
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        let activation = job.parameters["activation_id"]
            .as_str()
            .ok_or(FailureCode::Internal)?
            .to_owned();
        let (operation, evidence) =
            super::claude_helper::discovery_evidence(&job.reference, settled.discovery())?;
        let parsed =
            crate::workspace::git::discovery::parse_discovery_evidence(&operation, &evidence)
                .map_err(|error| match error {
                    crate::workspace::git::GitError::UnsupportedDiscoveryGit => {
                        FailureCode::UnsupportedGit
                    }
                    _ => FailureCode::WorkspaceActivation,
                })?;
        let listing = parsed.listing().to_vec();
        let (top, common) = (parsed.top().to_path_buf(), parsed.common().to_path_buf());
        self.shared.active(&binding)?;
        let tree = self
            .workspace
            .resolve_worktree(top.clone(), top, common)
            .await
            .map_err(|_| FailureCode::WorkspaceActivation)?;
        // Reconsume the exact binding after the awaited Workspace call, before using its result.
        self.shared.active(&binding)?;
        // The listing is validated against the canonical root Workspace resolved, not against a
        // root the helper asserted. This is byte comparison only; no listed worktree is opened.
        crate::workspace::git::discovery::validate_listing(&listing, tree.worktree_path())
            .map_err(|_| FailureCode::WorkspaceActivation)?;
        let mut identity = blake3::Hasher::new();
        identity.update(&binding.fingerprint());
        identity.update(activation.as_bytes());
        let request = crate::workspace::authority::ActivationRequest::new(
            identity.finalize().to_hex().to_string(),
            job.invocation.clone(),
            // Reauthorize immediately before the durable commit.
            self.shared.active(&binding)?,
            tree,
        )
        .map_err(|_| FailureCode::WorkspaceActivation)?;
        let receipt = match self.workspace.activate(request).await {
            Ok(receipt) => receipt,
            // A second actor for the same worktree loses; the first owner stays usable.
            Err(
                crate::workspace::durable::DurableError::OperationConflict
                | crate::workspace::durable::DurableError::Authority(
                    crate::workspace::authority::AuthorityError::WorktreeOwned
                    | crate::workspace::authority::AuthorityError::ActorAlreadyOwnsWorktree,
                ),
            ) => {
                return Err(FailureCode::Conflict);
            }
            Err(
                crate::workspace::durable::DurableError::Application(_)
                | crate::workspace::durable::DurableError::CorruptState,
            ) => {
                self.uncertain.insert(binding.clone());
                return Err(FailureCode::WorkspaceActivation);
            }
            Err(_) => return Err(FailureCode::WorkspaceActivation),
        };
        // Retain the receipt before checking whether stop won the race, so an activation that
        // committed durably is always recoverably revocable rather than orphaned.
        let activation_operation = receipt.operation().to_owned();
        self.grants.insert(binding.clone(), receipt);
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(error) => {
                if let Ok(mut guard) = self.shared.bindings.lock() {
                    let _ = guard.stop_binding(&binding);
                }
                let _ = self.revoke(&binding, &job.reference).await;
                return Err(error);
            }
        };
        let launches = job.target.providers.clone();
        let profile = job
            .target
            .claude_profile
            .ok_or(FailureCode::ExecutionProfile)?;
        let rights = profile.rights_identity()?;
        if let Err(code) =
            self.retain_worktree_caches(&binding, &authority, &launches, true, false, rights)
        {
            if let Ok(mut guard) = self.shared.bindings.lock() {
                let _ = guard.stop_binding(&binding);
            }
            return Err(self.settle_revocation(&binding).await.err().unwrap_or(code));
        }
        let caches = self.helper_cache_namespaces(&binding, &authority, &launches, rights)?;
        let baseline = self
            .capture_claude_baseline(&binding, &authority, &activation_operation, baseline)
            .await;
        let helper_baseline = match baseline {
            Ok(baseline) => {
                let digest = baseline.capture_digest().copied();
                let value = HelperBaseline {
                    reference: baseline.reference().to_owned(),
                    captured: digest.is_some(),
                    digest,
                };
                self.baselines.insert(binding.clone(), baseline);
                value
            }
            Err(_) => HelperBaseline {
                reference: format!("baseline-{activation_operation}"),
                captured: false,
                digest: None,
            },
        };
        let worktree = authority.worktree();
        let scope = HelperScope {
            worktree_id: worktree.id().to_owned(),
            incarnation: worktree.incarnation(),
            root: worktree.worktree_path().to_path_buf(),
            repository_root: worktree.repository_root().to_path_buf(),
            git_common_dir: worktree.git_common_dir().to_path_buf(),
            native_root_identity: worktree
                .native_root_identity()
                .ok_or(FailureCode::WorkspaceAuthority)?,
            authority_epoch: authority.epoch(),
        };
        self.shared.active(&binding)?;
        self.shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .claude
            .insert(
                binding.clone(),
                ClaudeBindingState {
                    scope,
                    baseline: helper_baseline.clone(),
                    caches,
                },
            );
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text: format!(
                    "Workspace activated; authority_epoch: {}; baseline: {}; worktree_cache: retained. Provider readiness is not implied.",
                    authority.epoch(),
                    if helper_baseline.captured {
                        "partial (Unverified; durable capture true)"
                    } else {
                        "unknown (durable capture unavailable)"
                    },
                ),
                detail_ref: Some(job.reference.clone()),
                truncated: false,
            },
            Some(authority),
            None,
        ))
    }

    /// Records helper-observed source metadata without reopening the source in the daemon.
    async fn record_claude_source(
        &mut self,
        binding: &BindingRef,
        source: &super::claude_worker::HelperSource,
    ) -> Result<SourceObservation, FailureCode> {
        use crate::workspace::{
            observation::{ObservationRef, SourceBytes, SourceCoverage, SourceRevision},
            store::{ObservationAdmission, ObservationDraft},
        };
        let authority = self.authority(binding).await?;
        let path = std::path::PathBuf::from(&source.path);
        self.source_sequence = self
            .source_sequence
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        let key = format!(
            "source-{}-{}",
            blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
            self.source_sequence
        );
        let operation = OperationId::new(key.clone()).map_err(|_| FailureCode::Internal)?;
        let reference = ObservationRef::new(key).map_err(|_| FailureCode::Internal)?;
        let draft = match (source.present, source.digest) {
            (true, Some(digest)) => ObservationDraft::present(
                authority.worktree().clone(),
                authority.epoch(),
                operation,
                reference,
                path.clone(),
                SourceBytes::from_reported(digest, source.length)
                    .map_err(|_| FailureCode::SourceUnavailable)?,
                SourceRevision::new(blake3::Hash::from_bytes(digest).to_hex().to_string())
                    .map_err(|_| FailureCode::Internal)?,
                SourceCoverage::Complete,
            ),
            (false, None) if source.length == 0 => ObservationDraft::missing(
                authority.worktree().clone(),
                authority.epoch(),
                operation,
                reference,
                path.clone(),
                SourceRevision::new("missing").map_err(|_| FailureCode::Internal)?,
                SourceCoverage::Complete,
            ),
            _ => return Err(FailureCode::SourceUnavailable),
        }
        .map_err(|_| FailureCode::SourceUnavailable)?;
        self.workspace
            .authorize(&authority, &self.shared.active(binding)?)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let ObservationAdmission::Recorded(observed) = self
            .observations
            .record(draft)
            .await
            .map_err(|_| FailureCode::SourceUnavailable)?
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        self.shared.active(binding)?;
        let paths = self.registered.entry(binding.clone()).or_default();
        if paths.len() >= self.shared.launcher.limits.details && !paths.contains(&path) {
            return Err(FailureCode::Capacity);
        }
        paths.insert(path);
        Ok(observed)
    }

    /// Publishes one settled helper Context after durable authority and source-metadata admission.
    async fn context_claude(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let JobInput::Claude(settled) = &job.input else {
            return Err(FailureCode::Internal);
        };
        if settled.operation() != HelperOperation::Context
            || settled.binding().fingerprint() != binding.fingerprint()
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        let Some(super::claude_worker::HelperPayload::Context {
            source,
            feedback,
            diagnostic_fingerprint,
            truncated,
        }) = settled.result().payload.as_ref()
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        if job.parameters["path"].as_str() != Some(source.path.as_str()) {
            return Err(FailureCode::SourceUnavailable);
        }
        let HelperOutcome::Complete { text } = &settled.result().outcome else {
            return Err(FailureCode::SourceUnavailable);
        };
        let observed = self.record_claude_source(&binding, source).await?;
        let authority = self.authority(&binding).await?;
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch || tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::SourceUnavailable);
        }
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            match (feedback, diagnostic_fingerprint, source.digest) {
                (Some(feedback), Some(diagnostic_fingerprint), Some(source_digest)) => {
                    let identity = DeliveredIssue {
                        source_path: std::path::PathBuf::from(&source.path),
                        source_digest,
                        diagnostic_fingerprint: *diagnostic_fingerprint,
                    };
                    // A redundant helper run for the exact same unchanged issue already reached a
                    // caller (inline or via the hook) — never re-arm it as a fresh undelivered
                    // fact just because this job happened to run again.
                    ledger.retain_feedback(
                        &binding,
                        NativeFeedback {
                            source: None,
                            text: feedback.clone(),
                            native_epoch: epoch,
                            // Computing this job's reply is not submitting it: the caller may
                            // still only hold `Pending` until a later Inspect, or lose it to a
                            // deadline.
                            inline_delivered: false,
                            producer: job.reference.clone(),
                            identity,
                        },
                    );
                }
                _ => {
                    ledger.feedback.remove(&binding);
                }
            }
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: format!(
                    "source_sequence: {}\nauthority_epoch: {}\n{}",
                    observed.sequence(),
                    authority.epoch(),
                    text
                ),
                detail_ref: Some(job.reference.clone()),
                truncated: *truncated,
            },
            Some(authority),
            Some(observed),
        ))
    }

    /// Publishes one settled helper-composed Diff after current authority and scope rechecks.
    async fn diff_claude(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let JobInput::Claude(settled) = &job.input else {
            return Err(FailureCode::Internal);
        };
        if settled.operation() != HelperOperation::Diff
            || settled.binding().fingerprint() != binding.fingerprint()
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        let Some(super::claude_worker::HelperPayload::Diff { truncated }) =
            settled.result().payload.as_ref()
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        let HelperOutcome::Complete { text } = &settled.result().outcome else {
            return Err(FailureCode::SourceUnavailable);
        };
        let authority = self.authority(&binding).await?;
        let state = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .claude
            .get(&binding)
            .cloned()
            .ok_or(FailureCode::WorkspaceAuthority)?;
        if state.scope.authority_epoch != authority.epoch()
            || state.scope.worktree_id != authority.worktree().id()
            || tokio::time::Instant::now() >= job.deadline
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        self.shared.active(&binding)?;
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: text.clone(),
                detail_ref: Some(job.reference.clone()),
                truncated: *truncated,
            },
            Some(authority),
            None,
        ))
    }

    /// Locks the daemon's single admission controller for one synchronous accounting call.
    ///
    /// The guard must never be held across an await: it is a `std` mutex shared with the helper
    /// socket task, and the worker task must stay `Send`. Every caller therefore takes it inside
    /// one statement.
    fn admission(&self) -> std::sync::MutexGuard<'_, crate::execution::AdmissionController> {
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Allocates one physical slot without waiting behind a retained idle backend; queued tickets are cancelled.
    fn admit(
        &mut self,
        binding: &BindingRef,
    ) -> Result<crate::execution::AdmissionLease, FailureCode> {
        use crate::execution::{Admission, AdmissionClass, OwnerId};
        let owner = OwnerId::new(
            blake3::Hash::from_bytes(binding.fingerprint())
                .to_hex()
                .to_string(),
        )
        .map_err(|_| FailureCode::Internal)?;
        match self.admission().submit(owner, AdmissionClass::Interactive) {
            Admission::Granted(lease) => Ok(lease),
            Admission::Queued(ticket) => {
                self.admission().cancel_ticket(ticket);
                Err(FailureCode::Capacity)
            }
            Admission::Refused(_) => Err(FailureCode::Capacity),
        }
    }

    /// Settles only a definite pre-child failure; every uncertain process error retains its reservation.
    fn spawn_failure(
        &mut self,
        error: crate::execution::ProcessError,
        binding: &BindingRef,
    ) -> FailureCode {
        if let crate::execution::ProcessError::NeverStarted { settlement, .. } = error {
            if self.admission().settle_never_started(settlement).is_err() {
                self.uncertain.insert(binding.clone());
            }
        } else {
            self.uncertain.insert(binding.clone());
        }
        FailureCode::ExecutionProfile
    }

    /// Returns only a current boot-fenced stamp after a fresh binding consume.
    async fn authority(&self, binding: &BindingRef) -> Result<AuthorityStamp, FailureCode> {
        let receipt = self
            .grants
            .get(binding)
            .ok_or(FailureCode::WorkspaceAuthority)?;
        let active = self.shared.active(binding)?;
        self.workspace
            .authority(receipt, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)
    }

    /// Reads and persists one registered file under fresh durable authority, preserving missing state.
    async fn observe(
        &mut self,
        binding: &BindingRef,
        path: std::path::PathBuf,
        observed_scope: &ObservedSandboxState,
        target: &LaunchTarget,
    ) -> Result<(SourceObservation, Vec<u8>), FailureCode> {
        use crate::workspace::{
            observation::{
                ObservationError, ObservationRef, SourceCoverage, SourceReadLimits, SourceRevision,
                read_authorized_source,
            },
            store::{ObservationAdmission, ObservationDraft},
        };
        let authority = self.authority(binding).await?;
        validate_read_scope(&self.shared, binding, observed_scope, target, &authority)?;
        let read = read_authorized_source(
            authority.worktree(),
            &path,
            SourceReadLimits::new(1024, self.shared.launcher.limits.output_bytes)
                .map_err(|_| FailureCode::Internal)?,
        );
        self.source_sequence = self
            .source_sequence
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        let key = format!(
            "source-{}-{}",
            blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
            self.source_sequence
        );
        let operation = OperationId::new(key.clone()).map_err(|_| FailureCode::Internal)?;
        let reference = ObservationRef::new(key).map_err(|_| FailureCode::Internal)?;
        let (draft, bytes) = match read {
            Ok(read) => {
                let revision =
                    SourceRevision::new(blake3::hash(read.contents()).to_hex().to_string())
                        .map_err(|_| FailureCode::Internal)?;
                (
                    ObservationDraft::present(
                        authority.worktree().clone(),
                        authority.epoch(),
                        operation,
                        reference,
                        path.clone(),
                        read.bytes().clone(),
                        revision,
                        SourceCoverage::Complete,
                    )
                    .map_err(|_| FailureCode::SourceUnavailable)?,
                    read.contents().to_vec(),
                )
            }
            Err(ObservationError::Missing) => (
                ObservationDraft::missing(
                    authority.worktree().clone(),
                    authority.epoch(),
                    operation,
                    reference,
                    path.clone(),
                    SourceRevision::new("missing").map_err(|_| FailureCode::Internal)?,
                    SourceCoverage::Complete,
                )
                .map_err(|_| FailureCode::SourceUnavailable)?,
                Vec::new(),
            ),
            Err(_) => return Err(FailureCode::SourceUnavailable),
        };
        let active = self.shared.active(binding)?;
        self.workspace
            .authorize(&authority, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let ObservationAdmission::Recorded(observed) = self
            .observations
            .record(draft)
            .await
            .map_err(|_| FailureCode::SourceUnavailable)?
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        self.shared.active(binding)?;
        let paths = self.registered.entry(binding.clone()).or_default();
        if paths.len() >= self.shared.launcher.limits.details && !paths.contains(&path) {
            return Err(FailureCode::Capacity);
        }
        paths.insert(path);
        Ok((observed, bytes))
    }

    /// Reconciles native-hinted registered paths only when a new MCP call supplies current sandbox state.
    /// Hooks themselves never provide new read authority or authorize a scope from tool payload text.
    async fn reconcile_hints(&mut self, job: &Job) {
        let binding = job.invocation.binding_ref();
        let Some(scope) = &job.observed else {
            return;
        };
        let hinted = self
            .shared
            .bindings
            .lock()
            .ok()
            .and_then(|mut guard| guard.take_native_change_hint(binding).ok())
            .unwrap_or(false);
        if hinted {
            let paths = self.registered.get(binding).cloned().unwrap_or_default();
            for path in paths {
                if *job.cancel.borrow() || tokio::time::Instant::now() >= job.deadline {
                    break;
                }
                if self
                    .observe(binding, path, scope, &job.target)
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    /// Returns current owner context with explicit semantic or lexical provenance and bounded source text.
    async fn context(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        use crate::intelligence::context::{ContextMode, ContextQuery, lexical_context};
        let binding = job.invocation.binding_ref().clone();
        let path = job.parameters["path"]
            .as_str()
            .ok_or(FailureCode::SourceUnavailable)?
            .to_owned();
        let (observed, bytes) = self
            .observe(
                &binding,
                path.clone().into(),
                job.observed.as_ref().ok_or(FailureCode::SandboxState)?,
                &job.target,
            )
            .await?;
        let query = job
            .parameters
            .get("byte_offset")
            .and_then(Value::as_u64)
            .map_or(ContextQuery::File, |byte_offset| ContextQuery::Symbol {
                byte_offset: byte_offset as usize,
            });
        let semantic = if observed.bytes().is_none() {
            Ok(None)
        } else {
            self.semantic_context(job, &observed, &bytes, query).await
        };
        let (context, diagnostics)=match semantic {
            Ok(Some(result))=>(result.context, Some(result.diagnostics)),
            Ok(None)=>(lexical_context(&observed,&bytes,query,"no accepted provider is configured for this source, or the registered path is missing").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(FailureCode::ProviderUnavailable)=>(lexical_context(&observed,&bytes,query,"accepted semantic provider is unavailable").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(code)=>return Err(code),
        };
        if !source_matches(&observed) {
            return Err(FailureCode::SourceUnavailable);
        }
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch {
            return Err(FailureCode::SourceUnavailable);
        }
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::Deadline);
        }
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        let mode = match &context.mode {
            ContextMode::Semantic => "semantic".to_owned(),
            ContextMode::Lexical { reason } => format!("lexical ({reason})"),
        };
        let diagnostics = diagnostics.and_then(|diagnostics| {
            (context.freshness == crate::intelligence::freshness::Freshness::Current
                && diagnostics.freshness == crate::intelligence::freshness::Freshness::Provisional
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
            .map(|diagnostics| {
                FeedbackDelta::new(
                    format!(
                        "Provider reported {} diagnostics for this exact source generation.",
                        diagnostics.diagnostics.len()
                    ),
                    format!(
                        "source_sequence={}; provider_generation={:?}; document_version={:?}",
                        observed.sequence(),
                        diagnostics.generation,
                        diagnostics.document_version
                    ),
                    "Review the bounded diagnostic messages in the latest context result.",
                    "provisional push; exact source and positive provider version matched",
                    Some(job.reference.clone()),
                )
                .expect("fixed feedback envelope is bounded")
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
                let feedback = feedback
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), FeedbackDelta::render);
                format!(
                    "diagnostics_freshness: {:?}\ndiagnostic_readiness: {:?}\ndiagnostic_count: {}\ndiagnostics_truncated: {}\ndiagnostic_messages: {}\nfeedback_delta: {feedback}",
                    diagnostics.freshness,
                    diagnostics.readiness,
                    diagnostics.diagnostics.len(),
                    diagnostics.truncated || diagnostics.diagnostics.len() > messages.len(),
                    serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()),
                )
            },
        );
        let text = format!(
            "mode: {mode}\npath: {path}\nsource_state: {:?}\nsource_sequence: {}\nauthority_epoch: {}\ncoverage: complete registered path\nposition_encoding: {:?}\nprovider_generation: {:?}\ndocument_version: {:?}\n{diagnostic_text}\ndefinitions: {}\nreferences: {}\nlexical_matches: {}\n\n{}",
            observed.state(),
            observed.sequence(),
            authority.epoch(),
            context.position_encoding,
            context.generation,
            context.document_version,
            serde_json::to_string(&context.definitions).map_err(|_| FailureCode::Internal)?,
            serde_json::to_string(&context.references).map_err(|_| FailureCode::Internal)?,
            serde_json::to_string(&context.lexical_matches).map_err(|_| FailureCode::Internal)?,
            context.text
        );
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            if let Some(feedback) = feedback {
                let messages: Vec<&str> = diagnostics
                    .as_ref()
                    .map(|diagnostics| {
                        diagnostics
                            .diagnostics
                            .iter()
                            .map(|diagnostic| diagnostic.message.as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                let identity = DeliveredIssue {
                    source_path: observed.path().to_path_buf(),
                    source_digest: observed
                        .bytes()
                        .map(|bytes| *bytes.digest())
                        .unwrap_or([0; 32]),
                    diagnostic_fingerprint: super::facade::diagnostic_fingerprint(&messages),
                };
                // A redundant Context job for the exact same unchanged issue already reached a
                // caller (inline or via the hook) — never re-arm it as a fresh undelivered fact
                // just because this job happened to run again.
                ledger.retain_feedback(
                    &binding,
                    NativeFeedback {
                        source: Some(observed.clone()),
                        text: feedback.render(),
                        native_epoch: epoch,
                        // Computing this job's reply is not submitting it: the caller may still
                        // only hold `Pending` until a later Inspect, or lose it to a deadline.
                        inline_delivered: false,
                        producer: job.reference.clone(),
                        identity,
                    },
                );
            } else {
                ledger.feedback.remove(&binding);
            }
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Context,
                text,
                detail_ref: Some(job.reference.clone()),
                truncated: context.truncated,
            },
            Some(authority),
            Some(observed),
        ))
    }

    /// Executes one managed-Codex full-content edit through Changes receipts and Workspace permits.
    ///
    /// The source reference must name a completed same-binding Context detail whose exact source
    /// observation still matches `path`. A new durable prepare is the only route to Workspace; an
    /// exact prepared receipt recovered after ambiguity returns unknown and is never dispatched
    /// again. Known effects are followed by source observation and best-effort provider diagnostic
    /// refresh; provider failure never changes a known filesystem outcome.
    async fn edit(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let request: EditRequest =
            serde_json::from_value(job.parameters.clone()).map_err(|_| FailureCode::Internal)?;
        request.validate().map_err(|_| FailureCode::Internal)?;
        let binding = job.invocation.binding_ref().clone();
        let prepared = match self.edits.prepare(request.clone()).await {
            Ok(PrepareAdmission::Prepared(prepared)) => prepared,
            Ok(
                PrepareAdmission::Settled(result)
                | PrepareAdmission::ConflictingDuplicate(result)
                | PrepareAdmission::OutcomeUnknown(result),
            ) => {
                let authority = self.authority(&binding).await.ok();
                return Ok((PeerReply::Edit { result }, authority, None));
            }
            Err(_) => {
                return Ok((
                    PeerReply::Edit {
                        result: EditResult {
                            operation_id: request.operation_id,
                            path: request.path,
                            outcome: ChangesEditOutcome::UnavailableBeforeDispatch,
                            source_ref: None,
                        },
                    },
                    None,
                    None,
                ));
            }
        };
        let source = self.shared.ledger.lock().ok().and_then(|ledger| {
            ledger
                .details
                .get(&request.source_ref)
                .filter(|detail| {
                    detail.binding == binding
                        && matches!(
                            detail.selection.0,
                            AssistanceTool::Context | AssistanceTool::Edit
                        )
                        && match &detail.reply {
                            PeerReply::Complete {
                                kind: ResultKind::Context,
                                ..
                            } => true,
                            PeerReply::Edit { result } => {
                                result.source_ref.as_deref() == Some(request.source_ref.as_str())
                                    && result.outcome.has_post_source()
                            }
                            _ => false,
                        }
                })
                .and_then(|detail| detail.source.clone())
        });
        let Some(source) = source.filter(|source| source.path().to_str() == Some(&request.path))
        else {
            return self
                .settle_edit(
                    prepared,
                    &request,
                    crate::workspace::edit::EditOutcome::StaleSource,
                    None,
                    None,
                )
                .await;
        };
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(_) => {
                return self
                    .settle_edit(
                        prepared,
                        &request,
                        crate::workspace::edit::EditOutcome::CancelledNoEffect,
                        None,
                        None,
                    )
                    .await;
            }
        };
        let edit_source = match crate::workspace::edit::EditSourceRef::from_observation(&source) {
            Ok(source) => source,
            Err(outcome) => {
                return self
                    .settle_edit(prepared, &request, outcome, Some(authority), None)
                    .await;
            }
        };
        let active = match self.shared.active(&binding) {
            Ok(active) => active,
            Err(_) => {
                return self
                    .settle_edit(
                        prepared,
                        &request,
                        crate::workspace::edit::EditOutcome::CancelledNoEffect,
                        Some(authority),
                        None,
                    )
                    .await;
            }
        };
        let (permit, target) = match self
            .workspace
            .prepare_edit(
                &authority,
                &active,
                request.operation_id.clone(),
                request.path.clone().into(),
                edit_source.clone(),
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => {
                return self
                    .settle_edit(prepared, &request, outcome, Some(authority), None)
                    .await;
            }
        };
        let outcome = if *job.cancel.borrow() {
            crate::workspace::edit::EditOutcome::CancelledNoEffect
        } else if tokio::time::Instant::now() >= job.deadline {
            crate::workspace::edit::EditOutcome::DeadlineNoEffect
        } else {
            match self.shared.active(&binding) {
                Ok(active) => {
                    self.workspace
                        .replace_edit(
                            &authority,
                            &active,
                            permit,
                            target,
                            &edit_source,
                            request.content.as_bytes(),
                            || !*job.cancel.borrow() && tokio::time::Instant::now() < job.deadline,
                        )
                        .await
                }
                Err(_) => crate::workspace::edit::EditOutcome::CancelledNoEffect,
            }
        };
        let unchanged = matches!(outcome, crate::workspace::edit::EditOutcome::Unchanged(_));
        let known = matches!(
            outcome,
            crate::workspace::edit::EditOutcome::Created(_)
                | crate::workspace::edit::EditOutcome::Replaced(_)
                | crate::workspace::edit::EditOutcome::Unchanged(_)
        );
        let refreshed = if known && job.observed.is_some() {
            match self
                .observe(
                    &binding,
                    request.path.clone().into(),
                    job.observed.as_ref().expect("checked present"),
                    &job.target,
                )
                .await
            {
                Ok((observed, bytes)) => {
                    let _ = self
                        .semantic_context(
                            job,
                            &observed,
                            &bytes,
                            crate::intelligence::context::ContextQuery::File,
                        )
                        .await;
                    Some(observed)
                }
                Err(_) => None,
            }
        } else {
            None
        };
        let post_reference = refreshed
            .as_ref()
            .map(|_| job.reference.clone())
            .or_else(|| unchanged.then(|| request.source_ref.clone()));
        let result = EditResult::from_workspace(&request, outcome, |_| post_reference);
        let result = self.settle_prepared_edit(prepared, &request, result).await;
        Ok((PeerReply::Edit { result }, Some(authority), refreshed))
    }

    /// Durably settles one typed pre-effect Workspace outcome without dispatching a write.
    async fn settle_edit(
        &self,
        prepared: crate::changes::edit::PreparedEdit,
        request: &EditRequest,
        outcome: crate::workspace::edit::EditOutcome,
        authority: Option<AuthorityStamp>,
        source: Option<SourceObservation>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let result = EditResult::from_workspace(request, outcome, |_| None);
        let result = self.settle_prepared_edit(prepared, request, result).await;
        Ok((PeerReply::Edit { result }, authority, source))
    }

    /// Settles a consumed receipt and reconciles ambiguity without discarding an already durable result.
    async fn settle_prepared_edit(
        &self,
        prepared: crate::changes::edit::PreparedEdit,
        request: &EditRequest,
        result: EditResult,
    ) -> EditResult {
        match self.edits.settle(prepared, result).await {
            Ok(result) => result,
            Err(_) => match self.edits.prepare(request.clone()).await {
                Ok(
                    PrepareAdmission::Settled(result)
                    | PrepareAdmission::ConflictingDuplicate(result)
                    | PrepareAdmission::OutcomeUnknown(result),
                ) => result,
                _ => EditResult {
                    operation_id: request.operation_id.clone(),
                    path: request.path.clone(),
                    outcome: ChangesEditOutcome::OutcomeUnknown,
                    source_ref: None,
                },
            },
        }
    }

    /// Settles one stop by positively closing this binding's providers first and only then durably
    /// revoking its grant, so a failed durable half keeps full retry authority.
    ///
    /// On durable failure the receipt, this binding's cache keys (still non-quiescent), and its
    /// registered paths are all deliberately retained and the binding is marked pending, so a later
    /// fresh start can commit the same revoke before minting a new grant. Nothing here restores the
    /// stopped binding's source or provider authority and no stop is ever replayed: the host
    /// binding was already stopped by the ingress path and stays unusable either way. A daemon
    /// restart boot-fences old grants independently, so pending state is intentionally in-memory.
    async fn settle_revocation(&mut self, binding: &BindingRef) -> Result<(), FailureCode> {
        self.close_provider(binding).await?;
        let Some(receipt) = self.grants.get(binding).cloned() else {
            self.pending_revocations.remove(binding);
            self.release_binding_state(binding);
            return Ok(());
        };
        let operation = OperationId::new(format!(
            "stop-{}",
            blake3::Hash::from_bytes(binding.fingerprint()).to_hex()
        ))
        .map_err(|_| FailureCode::Internal)?;
        if self
            .workspace
            .revoke(operation, &receipt, StopBindingHandoff::Confirmed)
            .await
            .is_err()
        {
            if self.pending_revocations.len() < MAX_PENDING_REVOCATIONS {
                self.pending_revocations.insert(binding.clone());
            }
            return Err(FailureCode::WorkspaceAuthority);
        }
        self.grants.remove(binding);
        self.pending_revocations.remove(binding);
        self.release_binding_state(binding);
        Ok(())
    }

    /// Releases the binding-owned state that only a committed durable revoke makes safe to clear.
    fn release_binding_state(&mut self, binding: &BindingRef) {
        self.quiesce_worktree_caches(binding);
        self.registered.remove(binding);
        self.baselines.remove(binding);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            ledger.feedback.remove(binding);
            ledger.delivered.remove(binding);
            ledger.claude.remove(binding);
        }
    }

    /// Commits a still-pending revoke that this fresh start would otherwise race, before any new
    /// grant is minted for the same canonical worktree or the same incoming actor.
    ///
    /// This is cleanup-only receipt recovery: it can only retry the exact stop operation the failed
    /// settlement already derived from that binding, never mint or restore authority.
    async fn reconcile_pending_revocations(
        &mut self,
        tree: &crate::workspace::authority::WorktreeRef,
        actor: &str,
    ) {
        let pending = self
            .pending_revocations
            .iter()
            .filter(|binding| {
                self.grants.get(*binding).is_some_and(|receipt| {
                    receipt.worktree().id() == tree.id() || receipt.actor() == actor
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for binding in pending {
            let _ = self.settle_revocation(&binding).await;
        }
    }

    /// Revokes a recoverable receipt after host stop; absent grants are explicitly harmless.
    async fn revoke(
        &mut self,
        binding: &BindingRef,
        _reference: &str,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        self.settle_revocation(binding).await?;
        if self.uncertain.contains(binding) {
            return Err(FailureCode::Internal);
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text:
                    "Assistance stopped for this binding; compatible shared peers remain eligible"
                        .into(),
                detail_ref: None,
                truncated: false,
            },
            None,
            None,
        ))
    }

    /// Durably prepares a Claude edit and retains its one-use Changes token before helper minting.
    async fn prepare_claude_edit_job(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let request: EditRequest =
            serde_json::from_value(job.parameters.clone()).map_err(|_| FailureCode::Internal)?;
        request.validate().map_err(|_| FailureCode::Internal)?;
        let binding = job.invocation.binding_ref().clone();
        let prepared = match self.edits.prepare(request.clone()).await {
            Ok(PrepareAdmission::Prepared(prepared)) => prepared,
            Ok(
                PrepareAdmission::Settled(result)
                | PrepareAdmission::ConflictingDuplicate(result)
                | PrepareAdmission::OutcomeUnknown(result),
            ) => return Ok((PeerReply::Edit { result }, None, None)),
            Err(_) => {
                return Ok((
                    PeerReply::Edit {
                        result: EditResult {
                            operation_id: request.operation_id,
                            path: request.path,
                            outcome: ChangesEditOutcome::UnavailableBeforeDispatch,
                            source_ref: None,
                        },
                    },
                    None,
                    None,
                ));
            }
        };
        let source = {
            let ledger = self
                .shared
                .ledger
                .lock()
                .map_err(|_| FailureCode::Internal)?;
            ledger
                .details
                .get(&request.source_ref)
                .filter(|detail| {
                    detail.binding == binding
                        && detail.selection.0 == AssistanceTool::Context
                        && matches!(
                            detail.reply,
                            PeerReply::Complete {
                                kind: ResultKind::Context,
                                ..
                            }
                        )
                })
                .and_then(|detail| detail.source.clone())
                .filter(|source| source.path().to_str() == Some(request.path.as_str()))
        };
        let Some(source) = source else {
            return self
                .settle_edit(
                    prepared,
                    &request,
                    crate::workspace::edit::EditOutcome::StaleSource,
                    None,
                    None,
                )
                .await;
        };
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(_) => {
                return self
                    .settle_edit(
                        prepared,
                        &request,
                        crate::workspace::edit::EditOutcome::CancelledNoEffect,
                        None,
                        Some(source),
                    )
                    .await;
            }
        };
        if self.prepared_edits.len() >= self.shared.launcher.limits.details {
            return self
                .settle_edit(
                    prepared,
                    &request,
                    crate::workspace::edit::EditOutcome::CapacityNoEffect,
                    Some(authority),
                    Some(source),
                )
                .await;
        }
        self.prepared_edits
            .insert((binding, request.operation_id), prepared);
        Ok((PeerReply::HookObserved {}, Some(authority), Some(source)))
    }

    /// Settles one exact claimed-helper Edit result against its pre-helper Changes token.
    async fn edit_claude(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let JobInput::Claude(settled) = &job.input else {
            return Err(FailureCode::Internal);
        };
        if settled.operation() != HelperOperation::Edit
            || settled.binding().fingerprint() != binding.fingerprint()
            || !matches!(settled.result().outcome, HelperOutcome::Complete { .. })
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        let request: EditRequest =
            serde_json::from_value(job.parameters.clone()).map_err(|_| FailureCode::Internal)?;
        let prepared = self
            .prepared_edits
            .remove(&(binding.clone(), request.operation_id.clone()))
            .ok_or(FailureCode::Internal)?;
        let Some(super::claude_worker::HelperPayload::Edit {
            mut outcome,
            source,
        }) = settled.result().payload.clone()
        else {
            return Err(FailureCode::Internal);
        };
        if source
            .as_ref()
            .is_some_and(|source| source.path != request.path)
        {
            outcome = ChangesEditOutcome::OutcomeUnknown;
        }
        let mut observed = None;
        if outcome.has_post_source() {
            observed = match source.as_ref() {
                Some(source) => self.record_claude_source(&binding, source).await.ok(),
                None => None,
            };
            if observed.is_none() {
                outcome = ChangesEditOutcome::OutcomeUnknown;
            }
        }
        let source_ref = outcome.has_post_source().then(|| job.reference.clone());
        let result = EditResult::new(
            request.operation_id.clone(),
            request.path.clone(),
            outcome,
            source_ref,
        )
        .map_err(|_| FailureCode::Internal)?;
        let result = self
            .edits
            .settle(prepared, result)
            .await
            .unwrap_or_else(|_| EditResult {
                operation_id: request.operation_id,
                path: request.path,
                outcome: ChangesEditOutcome::OutcomeUnknown,
                source_ref: None,
            });
        let authority = self.authority(&binding).await.ok();
        Ok((PeerReply::Edit { result }, authority, observed))
    }

    /// Settles daemon-proven pre-claim no-effect or post-claim unknown helper lifecycle state.
    async fn settle_claude_edit_terminal(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let JobInput::ClaudeEditTerminal(outcome) = &job.input else {
            return Err(FailureCode::Internal);
        };
        let outcome = *outcome;
        if !matches!(
            outcome,
            ChangesEditOutcome::DeadlineNoEffect
                | ChangesEditOutcome::OutcomeUnknown
                | ChangesEditOutcome::UnavailableBeforeDispatch
        ) {
            return Err(FailureCode::Internal);
        }
        let request: EditRequest =
            serde_json::from_value(job.parameters.clone()).map_err(|_| FailureCode::Internal)?;
        let binding = job.invocation.binding_ref().clone();
        let prepared = match self
            .prepared_edits
            .remove(&(binding.clone(), request.operation_id.clone()))
        {
            Some(prepared) => prepared,
            None => match self.edits.prepare(request.clone()).await {
                Ok(PrepareAdmission::Settled(result)) => {
                    return Ok((
                        PeerReply::Edit { result },
                        self.authority(&binding).await.ok(),
                        None,
                    ));
                }
                _ => return Err(FailureCode::Internal),
            },
        };
        let result = EditResult::new(
            request.operation_id.clone(),
            request.path.clone(),
            outcome,
            None,
        )
        .map_err(|_| FailureCode::Internal)?;
        let result = self
            .edits
            .settle(prepared, result)
            .await
            .unwrap_or_else(|_| EditResult {
                operation_id: request.operation_id,
                path: request.path,
                outcome: ChangesEditOutcome::OutcomeUnknown,
                source_ref: None,
            });
        let authority = self.authority(&binding).await.ok();
        Ok((PeerReply::Edit { result }, authority, None))
    }
}

/// Aborts the independently scheduled inspection task if the owning worker task exits or panics,
/// so no detached inspector can outlive the worker it was coupled to.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    /// Aborts the held inspection task; runs on scope exit, unwind, or the outer worker task's own abort.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Services queued inspections on its own schedule, decoupled from the worker's job loop, so a
/// synchronous, non-yielding job poll cannot delay a pending `ide.inspect` reply.
async fn inspection_loop(
    workspace: DurableWorkspace<'static>,
    shared: Arc<Shared>,
    mut inspections: mpsc::Receiver<Inspection>,
) {
    while let Some(request) = inspections.recv().await {
        serve_inspection(&workspace, &shared, request).await;
    }
}

/// Delivers only a same-binding result after fresh durable authorization, with a final liveness check.
async fn serve_inspection(workspace: &DurableWorkspace<'_>, shared: &Shared, request: Inspection) {
    let result = async {
        let active = shared.active(&request.binding)?;
        let (reply, authority, source, native_epoch, diff_page, diff_page_fresh) = {
            let ledger = shared.ledger.lock().map_err(|_| FailureCode::Internal)?;
            let detail = ledger
                .details
                .get(&request.reference)
                .filter(|detail| detail.binding == request.binding)
                .ok_or(FailureCode::InvalidDetail)?;
            if request
                .expected
                .as_ref()
                .is_some_and(|expected| expected != &detail.selection)
            {
                return Err(FailureCode::InvalidDetail);
            }
            (
                detail.reply.clone(),
                detail.authority.clone(),
                detail.source.clone(),
                detail.native_epoch,
                detail.diff_page.clone(),
                detail.diff_page_fresh,
            )
        };
        // Ownership of this exact reference is established above, so releasing its retained page is
        // safe here and nowhere earlier. Every permanent invalidation below releases that heavy
        // evidence: the authority/native epoch it was captured under can never return, so retaining
        // megabytes of snapshot bytes would only pin memory against the aggregate ceiling. Only the
        // compact retry outcome the caller receives survives.
        let invalidate = |code: FailureCode| {
            shared.set_diff_page(&request.reference, None);
            code
        };
        if let Some(authority) = &authority {
            workspace
                .authorize(authority, &active)
                .await
                .map_err(|_| invalidate(FailureCode::WorkspaceAuthority))?;
            validate_read_scope(
                shared,
                &request.binding,
                &request.observed,
                &request.target,
                authority,
            )
            .map_err(invalidate)?;
        }
        if let Some(source) = source
            && !source_matches(&source)
        {
            return Err(invalidate(FailureCode::SourceUnavailable));
        }
        if matches!(
            reply,
            PeerReply::Complete {
                kind: ResultKind::Context | ResultKind::Diff,
                ..
            }
        ) && shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&request.binding)
            .copied()
            .unwrap_or(0)
            != native_epoch
        {
            return Err(invalidate(FailureCode::SourceUnavailable));
        }
        let active = shared.active(&request.binding)?;
        if let Some(authority) = &authority {
            workspace
                .authorize(authority, &active)
                .await
                .map_err(|_| invalidate(FailureCode::WorkspaceAuthority))?;
        }
        shared.active(&request.binding)?;
        let Some(page) = diff_page else {
            return Ok::<_, FailureCode>(reply);
        };
        if diff_page_fresh {
            // `reply` already holds this exact page's composed text, produced by the job (page 1)
            // or a prior expansion, and no caller has retrieved it yet. Hand it over unchanged;
            // only a later inspection may advance past it.
            if let Ok(mut ledger) = shared.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&request.reference)
            {
                detail.diff_page_fresh = false;
            }
            return Ok::<_, FailureCode>(reply);
        }
        let Some(authority) = &authority else {
            return Err(FailureCode::WorkspaceAuthority);
        };
        let expected_scope =
            crate::workspace::git::GitScope::from_authority(authority, page.mode());
        // Revalidate the retained evidence's working-tree material against the current worktree
        // before trusting it: an out-of-band edit with no native hook never bumps native_epoch, so
        // that check alone cannot catch it. Staged-only comparisons never depend on working-tree
        // bytes, so this is skipped rather than used as unrelated "proof" for them.
        if !page.working_tree_bytes_unchanged(authority.worktree()) {
            return Err(invalidate(FailureCode::SourceUnavailable));
        }
        // Expansion uses exactly the same whole-page fitting path as the initial composition, so
        // reply text always serializes under the bounded envelope without `PeerReply::encode`
        // needing to shrink it: a shrink cuts at a UTF-8 boundary, not a hunk boundary, which would
        // silently deliver a partial hunk while the cursor advanced past it as if it were whole.
        let (advanced, next) = snapshots::fit_diff_page(
            page.mode(),
            authority.epoch(),
            &request.reference,
            page.budget().max_hunks,
            |max_hunks| page.expand_with_max_hunks(&expected_scope, max_hunks),
        )
        .map_err(|code| match code {
            // A structurally unavailable or failed selection can never be repaired by a later page.
            FailureCode::SourceUnavailable => invalidate(code),
            // A budget refusal delivered nothing, so the retained evidence stays: dropping the
            // continuation here would lose hunks the caller can still reach later.
            code => code,
        })?;
        let encoded = next
            .clone()
            .encode()
            .and_then(|value| PeerReply::decode(value.as_str()));
        let Some(next) = encoded else {
            shared.set_diff_page(&request.reference, None);
            return Err(FailureCode::Internal);
        };
        if let Ok(mut ledger) = shared.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(&request.reference)
        {
            detail.reply = next.clone();
            detail.diff_page = page.advance(&advanced);
            detail.diff_page_fresh = false;
        }
        Ok::<_, FailureCode>(next)
    }
    .await;
    let reply = result.unwrap_or_else(|code| PeerReply::Error { code });
    // This is the actual submission boundary for the managed path: `reply` is about to be handed
    // to the real caller of either the initial `submit()` or a later `ide.inspect`. Marking must
    // wait for the send's own outcome — a request whose receiving side already closed must not
    // have its fact treated as delivered — and must trace the exact reference that produced the
    // retained fact, since a same-epoch, different-detail Context job can have overwritten the
    // single per-binding slot before this reply was ever composed.
    let is_context = matches!(
        reply,
        PeerReply::Complete {
            kind: ResultKind::Context,
            ..
        }
    );
    if request.reply.send(reply.clone()).is_ok() && is_context {
        shared.mark_feedback_inline_delivered(&request.binding, &request.reference, &reply);
    }
}

/// Reserves live inspection capacity, distinguishing saturation from service termination.
fn reserve_inspection(
    sender: &mpsc::Sender<Inspection>,
) -> Result<mpsc::OwnedPermit<Inspection>, FailureCode> {
    sender
        .clone()
        .try_reserve_owned()
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => FailureCode::Capacity,
            mpsc::error::TrySendError::Closed(_) => FailureCode::Internal,
        })
}

/// Reserves the initial inspection slot before invoking the closure that publishes a job/detail.
fn admit_initial_inspection(
    sender: &mpsc::Sender<Inspection>,
    enqueue: impl FnOnce() -> Result<String, FailureCode>,
) -> Result<(String, mpsc::OwnedPermit<Inspection>), FailureCode> {
    let permit = reserve_inspection(sender)?;
    let reference = enqueue()?;
    Ok((reference, permit))
}

/// Rechecks one exact registered path; a missing source never establishes worktree closure.
fn source_matches(source: &SourceObservation) -> bool {
    use crate::workspace::observation::{
        MAX_SOURCE_BYTES, ObservationError, ObservedState, SourceReadLimits, read_authorized_source,
    };
    match read_authorized_source(
        source.worktree(),
        source.path(),
        SourceReadLimits::new(1024, MAX_SOURCE_BYTES).expect("fixed source limits"),
    ) {
        Ok(read) => source.bytes() == Some(read.bytes()),
        Err(ObservationError::Missing) => source.state() == ObservedState::Missing,
        Err(_) => false,
    }
}

/// Intersects a current durable stamp and fresh invocation metadata before any native/cached source read.
fn validate_read_scope(
    shared: &Shared,
    binding: &BindingRef,
    observed: &ObservedSandboxState,
    target: &LaunchTarget,
    authority: &AuthorityStamp,
) -> Result<(), FailureCode> {
    let scoped = crate::execution::WorkspaceAuthority::from_workspace(
        authority.worktree().id(),
        authority.worktree().incarnation().to_string(),
        authority.worktree().worktree_path().to_path_buf(),
        authority.epoch(),
    )
    .map_err(|_| FailureCode::WorkspaceAuthority)?;
    crate::execution::validate_workspace_read(
        shared.active(binding)?,
        observed.clone(),
        &scoped,
        &target.catalog,
        target.allow_disabled_host,
    )
    .map(|_| ())
    .map_err(|_| FailureCode::ExecutionProfile)
}

/// Binds detail reuse to the exact closed query (including diff mode and context byte offset).
fn selection(parameters: &Value) -> [u8; 32] {
    let mut selected = parameters.clone();
    if let Some(object) = selected.as_object_mut() {
        object.remove("detail_ref");
    }
    *blake3::hash(selected.to_string().as_bytes()).as_bytes()
}

/// Saturated initial inspection admission must not invoke job/detail publication, while a closed
/// inspection service is an internal lifecycle fault rather than live capacity exhaustion.
#[test]
fn initial_inspection_admission_is_atomic_and_distinguishes_closure() {
    let (sender, receiver) = mpsc::channel(1);
    let held = sender.clone().try_reserve_owned().unwrap();
    let mut published = false;
    let result = admit_initial_inspection(&sender, || {
        published = true;
        Ok("lost-detail".to_owned())
    });
    assert!(matches!(result, Err(FailureCode::Capacity)));
    assert!(!published);

    drop(held);
    drop(receiver);
    let result = admit_initial_inspection(&sender, || panic!("closed service published detail"));
    assert!(matches!(result, Err(FailureCode::Internal)));
}

/// KSC3: a real SQLite writer-lock failure on stop must retain the receipt/pending-recovery marker
/// and non-quiescent cache instead of returning a client `Deadline`, and a later fresh Start for the
/// same worktree/actor must commit that pending revoke before minting its own grant.
#[cfg(test)]
mod stop_retry_tests {
    use super::*;
    use crate::{
        app::{
            cache::{CacheNamespaceId, CacheRoot},
            config::StoreConfig,
        },
        assistance::host_binding::{
            BindingStatus, parse_candidate, parse_channel_session, parse_hook_event,
            parse_observed_sandbox_state,
        },
        execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord},
        intelligence::freshness::{CacheIdentity, CacheLifecycle},
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Separates disposable fixture roots within the current process.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Owns the exact temporary worktree/database pair used by one stop-retry test.
    struct Fixture {
        base: std::path::PathBuf,
        root: std::path::PathBuf,
    }
    impl Fixture {
        /// Creates real directories beneath `/private/tmp`, avoiding Darwin's `/tmp` symlink alias.
        fn new() -> Self {
            let base = std::path::PathBuf::from(format!(
                "/private/tmp/worker-stop-retry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = base.join("worktree");
            std::fs::create_dir_all(&root).unwrap();
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .args(["init", "--quiet"])
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
            Self { base, root }
        }
        /// Opens the real fixture Store through the existing effective-config path, with a
        /// store_busy_timeout well below the 800ms host stop window used by `WorkerHandle::stop`.
        fn store(&self) -> Store {
            Store::open_with_backup_root(
                &self.base.join("state.sqlite"),
                &self.base.join("backups"),
                StoreConfig {
                    queue_capacity: 16,
                    busy_timeout: Duration::from_millis(150),
                    request_deadline: Duration::from_secs(2),
                    receipt_capacity: 64,
                },
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        /// Removes only this fixture's uniquely owned directory tree.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// Creates one current host binding and matching disabled sandbox observation for a real start.
    fn production_call(
        worker: &Worker<'_>,
        cwd: &std::path::Path,
        actor: &str,
        id: &str,
    ) -> (ValidatedInvocation, ObservedSandboxState) {
        let mut guard = worker.shared.bindings.lock().unwrap();
        let channel = parse_channel_session(b"stop-retry").unwrap();
        let hook = parse_hook_event(
            serde_json::json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":id})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            serde_json::json!({"threadId":actor,"callId":id,"x-codex-turn-metadata":{"turn":"stop-retry"}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("fixture binding must validate")
        };
        let active = guard.consume_active(invocation.binding_ref()).unwrap();
        let state = serde_json::json!({
            "permissionProfile":{"type":"disabled"},
            "codexLinuxSandboxExe":null,
            "sandboxCwd":cwd,
            "useLegacyLandlock":false
        });
        let meta = serde_json::json!({"codex/sandbox-state-meta":state});
        let observed =
            parse_observed_sandbox_state(meta.as_object().unwrap(), &invocation, &active, true)
                .unwrap();
        (invocation, observed)
    }

    /// Builds the closed disabled-host target consumed by the actual `Worker::activate` path.
    fn production_target(root: &std::path::Path) -> LaunchTarget {
        let state = HostSandboxState::parse(Some(serde_json::json!({
            "permissionProfile":{"type":"disabled"},
            "codexLinuxSandboxExe":null,
            "sandboxCwd":root,
            "useLegacyLandlock":false
        })))
        .unwrap();
        let record = PersistedProfileRecord::from_execution_evidence(
            "stop-retry-disabled",
            1,
            D03ProfileEvidence {
                provider_binary: "fixture-git".into(),
                toolchain: "fixture-toolchain".into(),
                configuration: "default".into(),
                trust: "fixture-local".into(),
                transport: "direct".into(),
                d03_evidence: "fixture-d03".into(),
            },
            &state,
        )
        .unwrap();
        let git = std::path::Path::new("/usr/bin/git");
        let executable = serde_json::json!({
            "path":git,
            "identity":"fixture-git",
            "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
        });
        let config = serde_json::json!({
            "version":1,
            "limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},
            "targets":[{
                "attachment":"stop-retry",
                "candidate":root,
                "git":executable,
                "codex":executable,
                "providers":[],
                "profiles":[{"record":serde_json::from_str::<serde_json::Value>(&record.to_json()).unwrap(),"sandbox_state":serde_json::from_str::<serde_json::Value>(state.sandbox_state_json()).unwrap()}],
                "allow_disabled_host":true
            }]
        });
        LauncherConfig::parse(config.to_string().as_bytes())
            .unwrap()
            .target("stop-retry")
            .unwrap()
            .clone()
    }

    /// Runs the actual production `Worker::activate` entry point and returns its durable receipt.
    async fn production_start(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
    ) -> (BindingRef, StartReceipt) {
        let (invocation, observed) = production_call(worker, &worker.runtime, actor, id);
        let binding = invocation.binding_ref().clone();
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut job = Job {
            input: JobInput::Managed,
            reference: format!("production-{id}"),
            invocation,
            observed: Some(observed),
            tool: AssistanceTool::Start,
            parameters: serde_json::json!({"activation_id":id}),
            target: production_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
        };
        worker.activate(&mut job).await.unwrap();
        let receipt = worker.grants.get(&binding).cloned().unwrap();
        (binding, receipt)
    }

    /// Builds a Worker with real Store, binding and admission state for activation/revoke checks.
    /// Each test job supplies its own configured target and runs actual Git discovery; no background
    /// dispatcher is spawned by this fixture constructor.
    fn worker<'a>(
        store: &'a Store,
        workspace: DurableWorkspace<'a>,
        runtime: std::path::PathBuf,
    ) -> Worker<'a> {
        let launcher = LauncherConfig::parse(
            br#"{"version":1,"limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},"targets":[]}"#,
        )
        .unwrap();
        Worker {
            shared: Arc::new(Shared {
                bindings: Arc::new(Mutex::new(HostBindingGuard::default())),
                ledger: Mutex::new(Ledger::default()),
                notify: Notify::new(),
                launcher,
                nonce: [7; 32],
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                shutdown_failure: Mutex::new(None),
                admission: Arc::new(Mutex::new(admission_controller())),
                telemetry: Arc::new(crate::assistance::telemetry::NoopEditTelemetry),
            }),
            workspace,
            observations: WorkspaceStore::new(store),
            edits: EditReceiptStore::new(store),
            prepared_edits: BTreeMap::new(),
            grants: BTreeMap::new(),
            pending_revocations: std::collections::BTreeSet::new(),
            registered: BTreeMap::new(),
            baselines: BTreeMap::new(),
            source_sequence: 0,
            admission: Arc::new(Mutex::new(admission_controller())),
            uncertain: std::collections::BTreeSet::new(),
            uncertain_snapshots: Vec::new(),
            runtime,
            providers: providers::Providers::new(),
        }
    }

    /// Proves the managed worker loop consumes a completed Context reference and returns exact edit effects.
    #[tokio::test]
    async fn managed_context_to_edit_loop_replaces_once_and_conflicts_changed_duplicate() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "fn old() {}\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "edit-actor", "edit-start").await;

        let (invocation, observed) =
            production_call(&worker, &fixture.root, "edit-actor", "edit-context");
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut context_job = Job {
            input: JobInput::Managed,
            reference: "context-source".into(),
            invocation,
            observed: Some(observed),
            tool: AssistanceTool::Context,
            parameters: serde_json::json!({"path":"main.rs"}),
            target: production_target(&fixture.root),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
        };
        let (context_reply, authority, source) = worker.context(&mut context_job).await.unwrap();
        worker.shared.ledger.lock().unwrap().details.insert(
            context_job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: context_reply,
                selection: (AssistanceTool::Context, selection(&context_job.parameters)),
                authority,
                source,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
            },
        );

        let (invocation, observed) =
            production_call(&worker, &fixture.root, "edit-actor", "edit-call");
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut edit_job = Job {
            input: JobInput::Managed,
            reference: "edit-result".into(),
            invocation,
            observed: Some(observed),
            tool: AssistanceTool::Edit,
            parameters: serde_json::json!({
                "operation_id":"operation-1",
                "path":"main.rs",
                "source_ref":"context-source",
                "content":"fn new() {}\n"
            }),
            target: production_target(&fixture.root),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
        };
        let (reply, authority, source) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    source_ref: Some(ref source_ref),
                    ..
                }
            } if source_ref == "edit-result"
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn new() {}\n"
        );
        assert_eq!(
            source.as_ref().unwrap().bytes(),
            Some(&crate::workspace::observation::SourceBytes::from_bytes(
                b"fn new() {}\n"
            ))
        );

        // A successful Edit is itself a usable post-read source detail for the next Edit.
        worker.shared.ledger.lock().unwrap().details.insert(
            edit_job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: reply.clone(),
                selection: (AssistanceTool::Edit, selection(&edit_job.parameters)),
                authority,
                source,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
            },
        );
        edit_job.reference = "edit-result-2".into();
        edit_job.parameters = serde_json::json!({
            "operation_id":"operation-2",
            "path":"main.rs",
            "source_ref":"edit-result",
            "content":"fn newest() {}\n"
        });
        let (reply, _, _) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    source_ref: Some(ref source_ref),
                    ..
                }
            } if source_ref == "edit-result-2"
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn newest() {}\n"
        );

        edit_job.parameters["content"] = serde_json::json!("fn conflicting() {}\n");
        let (reply, _, _) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::ConflictingDuplicate,
                    ..
                }
            }
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn newest() {}\n"
        );
    }

    #[tokio::test]
    async fn failed_stop_retains_pending_revoke_and_fresh_start_commits_it_first() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let tree = workspace
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                std::path::PathBuf::from(".git"),
            )
            .await
            .unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (old_binding, old_receipt) = production_start(&mut worker, "actor-1", "call-1").await;
        worker
            .registered
            .entry(old_binding.clone())
            .or_default()
            .insert(std::path::PathBuf::from("src/lib.rs"));

        // A real, freshly-retained `CacheLifecycle` owned by `old_binding`: freshly retained means
        // non-quiescent, i.e. actively owned by the still-live binding, not yet eligible for handoff.
        let cache_root = CacheRoot::prepare(fixture.base.join("cache")).unwrap();
        let cache_identity = CacheIdentity::new(
            "rust-analyzer",
            "rust-cache-priming-disabled-v1",
            "config",
            "1.98.1",
            "trusted",
            "tree-state",
        )
        .unwrap();
        let cache_key = "cache-1";
        let cache = CacheLifecycle::retain(
            &cache_root,
            CacheNamespaceId::new(cache_key).unwrap(),
            cache_identity,
            &tree,
        )
        .unwrap();
        assert!(!cache.quiescent(), "a freshly retained cache starts owned");
        worker.install_test_cache(&old_binding, cache_key, cache);

        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&old_binding)
            .unwrap();
        assert!(
            worker.shared.active(&old_binding).is_err(),
            "the stopped binding is unusable"
        );

        // A second real connection to the same fixture database holds the exact write lock the
        // durable revoke transaction needs, comfortably inside the reduced busy-timeout window.
        let lock = rusqlite::Connection::open(fixture.base.join("state.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let outcome = worker.revoke(&old_binding, "stop-1").await;
        assert!(matches!(outcome, Err(FailureCode::WorkspaceAuthority)));
        assert_eq!(worker.grants.get(&old_binding), Some(&old_receipt));
        assert!(worker.pending_revocations.contains(&old_binding));
        assert!(worker.registered.contains_key(&old_binding));
        assert_eq!(
            worker.test_cache_quiescent(cache_key),
            Some(false),
            "a failed durable revoke must retain the cache non-quiescent, not release it early"
        );
        assert!(
            worker.test_binding_owns_caches(&old_binding),
            "the stopped binding still owns its cache keys until the revoke actually commits"
        );

        lock.execute_batch("ROLLBACK;").unwrap();
        drop(lock);

        // This enters `Worker::activate` itself. Its reconciliation call must commit the pending
        // revoke before the real durable activation below can mint a new grant.
        let (new_binding, new_receipt) = production_start(&mut worker, "actor-1", "call-2").await;
        assert!(!worker.grants.contains_key(&old_binding));
        assert!(worker.pending_revocations.is_empty());
        assert!(!worker.registered.contains_key(&old_binding));
        assert_eq!(
            worker.test_cache_quiescent(cache_key),
            Some(true),
            "commiting the pending revoke must quiesce the old binding's cache for reuse/retirement"
        );
        assert!(
            !worker.test_binding_owns_caches(&old_binding),
            "the old binding must no longer own any cache keys; its source binding is unusable"
        );
        assert_eq!(worker.grants.len(), 1);
        assert!(worker.grants.contains_key(&new_binding));
        assert_eq!(worker.grants.get(&new_binding), Some(&new_receipt));
        assert!(!worker.grants.contains_key(&old_binding));

        let settled = worker.revoke(&new_binding, "stop-2").await;
        assert!(settled.is_ok());
        assert!(!worker.grants.contains_key(&new_binding));
        assert!(worker.pending_revocations.is_empty());
    }
}

/// Cross-channel feedback dedup: a fact is consumed on submission to a real caller, never on the
/// producing job merely finishing. These tests exercise `NativeFeedback`, `take_current_feedback`
/// and `Shared::mark_feedback_inline_delivered` directly, without a durable Workspace/provider.
#[cfg(test)]
mod feedback_dedup_tests {
    use super::*;
    use crate::assistance::host_binding::{
        BindingStatus, parse_candidate, parse_channel_session, parse_hook_event,
    };

    /// Establishes one real Codex binding directly through the host-binding guard; these tests
    /// only exercise the feedback ledger, so no sandbox, worktree or provider setup is needed.
    fn active_binding() -> (Arc<Mutex<HostBindingGuard>>, BindingRef) {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let mut guard = bindings.lock().unwrap();
        let channel = parse_channel_session(b"feedback-dedup").unwrap();
        let hook = parse_hook_event(
            serde_json::json!({"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call-1"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            serde_json::json!({"threadId":"actor","callId":"call-1","x-codex-turn-metadata":{}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("fixture binding must validate")
        };
        let binding = invocation.binding_ref().clone();
        guard.consume_active(&binding).unwrap();
        drop(guard);
        (bindings, binding)
    }

    /// Builds a bare `WorkerHandle` with no configured targets; these tests touch only the ledger
    /// and never call `enqueue`/`submit`/`start`.
    fn handle(bindings: Arc<Mutex<HostBindingGuard>>) -> WorkerHandle {
        let launcher = LauncherConfig::parse(
            br#"{"version":1,"limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},"targets":[]}"#,
        )
        .unwrap();
        WorkerHandle::new(
            bindings,
            launcher,
            [9; 32],
            Arc::new(Mutex::new(admission_controller())),
        )
    }

    /// Builds the exact submitted-carrier shape `mark_feedback_inline_delivered` traces: a
    /// `Complete{Context}` reply whose composed text embeds (or, for the trimmed case, omits)
    /// the fact under test, exactly as the real `feedback_delta:` header does in production.
    fn context_reply(detail_ref: &str, text: impl Into<String>) -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: text.into(),
            detail_ref: Some(detail_ref.into()),
            truncated: false,
        }
    }

    /// Builds one bounded issue identity for a fixed tag; distinct tags never compare equal.
    /// These unit tests exercise the delivery-marking and consumption boundary directly, so the
    /// identity's own content only needs to vary by tag — real content derivation is exercised by
    /// the production `context`/`context_claude` regressions in `tests/product_mcp_contract.rs`.
    fn identity(tag: &str) -> DeliveredIssue {
        DeliveredIssue {
            source_path: std::path::PathBuf::from("main.rs"),
            source_digest: *blake3::hash(tag.as_bytes()).as_bytes(),
            diagnostic_fingerprint: *blake3::hash(tag.as_bytes()).as_bytes(),
        }
    }

    /// An issue already delivered under one detail reference is not rearmed by another producer,
    /// while a changed issue remains eligible for one later hook delivery.
    #[tokio::test]
    async fn repeated_issue_is_suppressed_but_changed_issue_remains_deliverable() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        let repeated = identity("same");
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.delivered.insert(binding.clone(), repeated.clone());
            assert!(!ledger.retain_feedback(
                &binding,
                NativeFeedback {
                    source: None,
                    text: "Fact: repeated".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-2".into(),
                    identity: repeated,
                }
            ));
            assert!(ledger.retain_feedback(
                &binding,
                NativeFeedback {
                    source: None,
                    text: "Fact: changed".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-3".into(),
                    identity: identity("changed"),
                }
            ));
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: changed")
        );
    }

    /// A background job's fact that finished but was never handed to a live caller (still
    /// `Pending`/lost to a deadline from the caller's view) stays a pending new fact: the first
    /// eligible ordinary hook may deliver it once, and a second post must not resurrect it.
    #[tokio::test]
    async fn undelivered_fact_is_deliverable_exactly_once() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t1"),
                },
            );
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle
                .take_current_feedback(binding.clone())
                .await
                .as_deref(),
            Some("Fact: one bounded fact")
        );
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }

    /// A fact already handed to a live caller inside a submitted Context/Inspect reply must never
    /// be echoed by a later ordinary hook. This inverts the old assertion that pinned the
    /// cross-channel duplicate, and proves the mark is honored only when set at the real
    /// submission boundary (`mark_feedback_inline_delivered`), never at job production.
    #[tokio::test]
    async fn inline_delivered_fact_is_never_echoed_by_a_later_hook() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t2"),
                },
            );
        }
        let reply = context_reply(
            "detail-1",
            "diagnostic_count: 1\nfeedback_delta: Fact: one bounded fact",
        );
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &reply);
        handle.native_hint(binding.clone());
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }

    /// Two different Context jobs at the *same* native epoch (no intervening native edit) can
    /// each overwrite the single per-binding slot with a different fact. Retrieving the older,
    /// already-superseded detail (A) must never mark the *current*, different fact (B) delivered
    /// — acknowledgement is bound to the exact producing detail, never just the epoch. Regresses
    /// the defect where `mark_feedback_inline_delivered` matched on epoch alone.
    #[tokio::test]
    async fn retrieving_an_old_detail_cannot_consume_a_different_same_epoch_fact() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            // Detail A's fact is inserted, then Detail B's Context job overwrites the same slot
            // at the same native epoch — exactly the production single-slot race.
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: A's fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-A".into(),
                    identity: identity("A"),
                },
            );
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: B's fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-B".into(),
                    identity: identity("B"),
                },
            );
        }
        // A caller retrieves the stale detail A (e.g. a retry against an old detail_ref) and A's
        // own reply is handed to it; A's producer no longer matches the retained (B) entry.
        let reply_a = context_reply("detail-A", "feedback_delta: Fact: A's fact");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-A", &reply_a);
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: B's fact"),
            "B's fact must still be eligible: only A's stale detail was retrieved, not B's"
        );
    }

    /// A same-producer *value* that never actually carries the fact — a shrunk/trimmed text that
    /// no longer contains it, or an `Error` value passed in place of `Complete` (standing in for
    /// a capped/failed transport) — must never be mislabeled as delivered: the fact stays
    /// eligible. This exercises only `mark_feedback_inline_delivered`'s own content-survival
    /// check on the reply value it is given; it does not exercise closed-receiver ordering, which
    /// is a property of its callers (`serve_inspection`/`perform` only invoke it after their own
    /// `send()` already succeeded) rather than of this function.
    #[tokio::test]
    async fn a_carrier_that_dropped_the_fact_is_not_treated_as_delivered() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t3"),
                },
            );
        }
        // Same producer, but the rendered text this specific carrier actually holds omits the
        // fact (a trimmed/shrunk or otherwise unrelated body).
        let trimmed = context_reply("detail-1", "diagnostic_count: 1\nfeedback_delta: none");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &trimmed);
        // A closed/failed transport for the same producer.
        let closed = PeerReply::Error {
            code: FailureCode::Deadline,
        };
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &closed);
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: one bounded fact")
        );
    }

    /// Repeated Context/Inspect retrieval of the same observation (the same native epoch) must
    /// not resurrect a fact this process already consumed via the hook channel: the single-slot
    /// remove in `take_current_feedback` is the identity, not a string comparison of raw text.
    #[tokio::test]
    async fn hook_consumed_fact_is_not_resurrected_by_a_later_delivery_mark() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t4"),
                },
            );
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle
                .take_current_feedback(binding.clone())
                .await
                .as_deref(),
            Some("Fact: one bounded fact")
        );
        // The entry is already gone; marking the same producer delivered afterward is a no-op and
        // must not fabricate a new retained fact.
        let reply = context_reply("detail-1", "feedback_delta: Fact: one bounded fact");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &reply);
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }
}
