//! One daemon-owned worker with bounded jobs/details, durable authorization and revocable work.

use super::{
    facade::{AssistanceTool, FeedbackDelta},
    host_binding::{
        ActiveBindingUse, BindingRef, HostBindingGuard, ObservedSandboxState, ValidatedInvocation,
    },
    launcher::{LaunchTarget, LauncherConfig},
    reply::{FailureCode, PeerReply, ResultKind, call_tool_result_fits, render_call_tool_result},
};
use crate::workspace::observation::SourceObservation;
use crate::{
    app::{
        config::EffectiveConfig,
        store::{MigrationAdmission, OperationId, Store},
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
mod snapshots;

use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// A bounded asynchronous operation whose identity never includes the transient MCP call ID.
struct Job {
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
    /// Exact source observation whose positive provider version produced the delta.
    source: SourceObservation,
    /// Bounded fact/evidence/action rendering; source text and diagnostics are excluded.
    text: String,
    /// Native lifecycle epoch at which the provider result was accepted.
    native_epoch: u64,
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
    /// Creates finite channels only; Store and Workspace are opened later under the daemon lock.
    pub fn new(
        bindings: Arc<Mutex<HostBindingGuard>>,
        launcher: LauncherConfig,
        nonce: [u8; 32],
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
            let _ = ready.send(Ok(()));
            Worker {
                shared,
                workspace,
                observations,
                grants: BTreeMap::new(),
                registered: BTreeMap::new(),
                baselines: BTreeMap::new(),
                source_sequence: 0,
                uncertain: std::collections::BTreeSet::new(),
                uncertain_snapshots: Vec::new(),
                runtime,
                providers: providers::Providers::new(),
                admission: crate::execution::AdmissionController::new(
                    crate::execution::AdmissionLimits {
                        total_running: 16,
                        per_owner_running: 2,
                        per_owner_queued: 1,
                        total_queued: 64,
                        interactive_burst: 8,
                    },
                )
                .expect("fixed process limits"),
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
            self.enqueue(invocation, observed, tool, parameters, attachment, None)
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

    /// Invalidates cached results on native hints; reads wait for a current MCP sandbox observation.
    pub fn native_hint(&self, binding: BindingRef) {
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            let epoch = ledger.native_epoch.entry(binding).or_default();
            *epoch = epoch.saturating_add(1);
        }
        self.shared.notify.notify_one();
    }

    /// Returns and consumes one same-binding delta only after a newer native epoch and source recheck.
    ///
    /// Missing, stopped, unchanged-epoch, stale, or unversioned feedback returns `None`. The check
    /// performs no provider execution and does not interpret a Claude permission mode as authority.
    pub async fn take_current_feedback(&self, binding: BindingRef) -> Option<String> {
        self.shared.active(&binding).ok()?;
        let feedback = {
            let mut ledger = self.shared.ledger.lock().ok()?;
            let current_epoch = ledger.native_epoch.get(&binding).copied().unwrap_or(0);
            let feedback = ledger.feedback.remove(&binding)?;
            (current_epoch > feedback.native_epoch && source_matches(&feedback.source))
                .then_some(feedback)?
        };
        self.shared.active(&binding).ok()?;
        Some(feedback.text)
    }

    /// Atomically bounds and publishes one operation, without file, database or child-process I/O.
    fn enqueue(
        &self,
        invocation: ValidatedInvocation,
        observed: Option<ObservedSandboxState>,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
        stop_reply: Option<oneshot::Sender<PeerReply>>,
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

/// One boot's sequential durable owner; provider operations may be interrupted by inspection service.
struct Worker<'a> {
    /// Shared bounded ingress and liveness state.
    shared: Arc<Shared>,
    /// Exactly one boot-fenced authority owner, opened once above.
    workspace: DurableWorkspace<'a>,
    /// Workspace-owned persistence for registered source observations.
    observations: WorkspaceStore<'a>,
    /// Recoverable committed activation receipts, at most one for each live host binding.
    grants: BTreeMap<BindingRef, StartReceipt>,
    /// Only explicitly requested paths are polled; no directory scanning is performed.
    registered: BTreeMap<BindingRef, std::collections::BTreeSet<std::path::PathBuf>>,
    /// Durable partial activation baselines retained for same-binding diff provenance.
    baselines: BTreeMap<BindingRef, crate::workspace::git::BaselineContext>,
    /// Boot-unique source observation operation sequence.
    source_sequence: u64,
    /// Finite physical-effect admission, shared by discovery, snapshots and language providers.
    admission: crate::execution::AdmissionController,
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
        self.shared.complete(
            &job.reference,
            reply.clone(),
            authority,
            source,
            job.native_epoch,
        );
        if let Some(sender) = job.stop_reply.take() {
            let _ = sender.send(reply);
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
            self.admission
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
            Err(crate::workspace::durable::DurableError::OperationConflict) => {
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
                let _ = self.revoke(&binding, &job.reference).await;
                return Err(error);
            }
        };
        let baseline = self
            .capture_activation_baseline(job, &authority, &activation_operation)
            .await;
        let launches = job.target.providers.clone();
        let cache_retained = self.retain_worktree_caches(&binding, &authority, &launches);
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
                    "Workspace activated; authority_epoch: {}; baseline: {baseline}; worktree_cache: {}. Provider readiness is not implied.",
                    authority.epoch(),
                    if cache_retained {
                        "retained"
                    } else {
                        "unknown"
                    },
                ),
                detail_ref: Some(job.reference.clone()),
                truncated: false,
            },
            Some(authority),
            None,
        ))
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
        match self.admission.submit(owner, AdmissionClass::Interactive) {
            Admission::Granted(lease) => Ok(lease),
            Admission::Queued(ticket) => {
                self.admission.cancel_ticket(ticket);
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
            if self.admission.settle_never_started(settlement).is_err() {
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
                ledger.feedback.insert(
                    binding.clone(),
                    NativeFeedback {
                        source: observed.clone(),
                        text: feedback.render(),
                        native_epoch: epoch,
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

    /// Revokes a recoverable receipt after host stop; absent grants are explicitly harmless.
    async fn revoke(
        &mut self,
        binding: &BindingRef,
        _reference: &str,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        self.close_provider(binding).await?;
        self.quiesce_worktree_caches(binding);
        self.registered.remove(binding);
        self.baselines.remove(binding);
        if let Some(receipt) = self.grants.get(binding).cloned() {
            self.workspace
                .revoke(
                    OperationId::new(format!(
                        "stop-{}",
                        blake3::Hash::from_bytes(binding.fingerprint()).to_hex()
                    ))
                    .map_err(|_| FailureCode::Internal)?,
                    &receipt,
                    StopBindingHandoff::Confirmed,
                )
                .await
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
            self.grants.remove(binding);
        }
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
    let _ = request
        .reply
        .send(result.unwrap_or_else(|code| PeerReply::Error { code }));
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
