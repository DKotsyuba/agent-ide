//! One daemon-owned worker with bounded jobs/details, durable authorization and revocable work.

use super::{
    facade::AssistanceTool,
    host_binding::{
        ActiveBindingUse, BindingRef, HostBindingGuard, ObservedSandboxState, ValidatedInvocation,
    },
    launcher::{LaunchTarget, LauncherConfig},
    reply::{FailureCode, PeerReply, ResultKind},
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
    /// Current Workspace stamp, required before any source/activation result can be delivered.
    authority: Option<AuthorityStamp>,
    /// Exact registered source facts rechecked before context delivery.
    source: Option<crate::workspace::observation::SourceObservation>,
    /// Native lifecycle revision associated with this result.
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
        }
    }
}

/// A short read request serviced by the same worker even while provider warmup is pending.
struct Inspection {
    /// Freshly host-validated caller binding.
    binding: BindingRef,
    /// Opaque result handle supplied by the model; it does not confer ownership.
    reference: String,
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
            }),
            inspect,
            receiver: Mutex::new(Some(receiver)),
            task: Mutex::new(None),
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
        let task = tokio::spawn(async move {
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
            let workspace = match DurableWorkspace::open(&store).await {
                Ok(owner) => owner,
                Err(_) => {
                    let _ = ready.send(Err(FailureCode::WorkspaceActivation));
                    return;
                }
            };
            let observations = WorkspaceStore::new(&store);
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
                source_sequence: 0,
            }
            .run(receiver)
            .await;
        });
        *self.task.lock().map_err(|_| FailureCode::Internal)? = Some(task);
        wait.await.map_err(|_| FailureCode::Internal)?
    }
    /// Enqueues one bounded operation and returns immediately; duplicate starts share their result key.
    pub fn submit(
        &self,
        invocation: ValidatedInvocation,
        observed: Option<ObservedSandboxState>,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
    ) -> PeerReply {
        match self.enqueue(invocation, observed, tool, parameters, attachment, None) {
            Ok(reference) => PeerReply::Pending {
                detail_ref: reference,
            },
            Err(code) => PeerReply::Error { code },
        }
    }
    /// Revokes queued/running work immediately and awaits only the finite stop outcome.
    /// Host binding revocation must already have happened; this method never restores it.
    pub async fn stop(&self, invocation: ValidatedInvocation, attachment: &str) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        if let Ok(mut ledger) = self.shared.ledger.lock()
            && let Some(sender) = ledger.cancellation.remove(&binding)
        {
            let _ = sender.send(true);
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
    /// Routes a same-binding inspection to the sole worker for fresh durable authorization.
    pub async fn inspect(&self, binding: BindingRef, reference: String) -> PeerReply {
        let (reply, wait) = oneshot::channel();
        if self
            .inspect
            .try_send(Inspection {
                binding,
                reference,
                reply,
            })
            .is_err()
        {
            return PeerReply::Error {
                code: FailureCode::Capacity,
            };
        }
        wait.await.unwrap_or(PeerReply::Error {
            code: FailureCode::Internal,
        })
    }
    /// Wakes bounded registered-path reconciliation after an active native hook lifecycle.
    pub fn native_hint(&self, binding: BindingRef) {
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            let epoch = ledger.native_epoch.entry(binding).or_default();
            *epoch = epoch.saturating_add(1);
        }
        self.shared.notify.notify_one();
    }

    /// Validates bounds/ownership and publishes one queue entry atomically without any I/O.
    fn enqueue(
        &self,
        invocation: ValidatedInvocation,
        observed: Option<ObservedSandboxState>,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
        stop_reply: Option<oneshot::Sender<PeerReply>>,
    ) -> Result<String, FailureCode> {
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
        if tool != AssistanceTool::Stop && ledger.queue.len() >= self.shared.launcher.limits.queued
        {
            return Err(FailureCode::Capacity);
        }
        if ledger.details.len() >= self.shared.launcher.limits.details
            && tool != AssistanceTool::Stop
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
                    authority: None,
                    source: None,
                    native_epoch: 0,
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
    /// Boot-unique source observation operation sequence.
    source_sequence: u64,
}
impl Worker<'_> {
    /// Processes one slow operation at a time while servicing short same-worker inspections.
    async fn run(mut self, mut inspections: mpsc::Receiver<Inspection>) {
        loop {
            self.reconcile_hints().await;
            let shared = self.shared.clone();
            let wake = shared.notify.notified();
            let job = shared
                .ledger
                .lock()
                .ok()
                .and_then(|mut ledger| ledger.queue.pop_front());
            if let Some(job) = job {
                let workspace = self.workspace;
                let service = self.perform(job);
                tokio::pin!(service);
                loop {
                    tokio::select! {_= &mut service=>break,Some(request)=inspections.recv()=>serve_inspection(workspace,&shared,request).await}
                }
            } else {
                tokio::select! {_=wake=>{},Some(request)=inspections.recv()=>serve_inspection(self.workspace,&shared,request).await}
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
            match job.tool {
                AssistanceTool::Start => self.activate(&job).await,
                AssistanceTool::Context => self.context(&job).await,
                AssistanceTool::Diff => Err(FailureCode::SourceUnavailable),
                _ => Err(FailureCode::Internal),
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
    /// Stops at the missing validated Git-discovery peer; no caller path can mint durable authority.
    async fn activate(
        &mut self,
        job: &Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let _ = (
            &self.observations,
            &job.parameters,
            &job.target,
            &job.observed,
        );
        Err(FailureCode::WorkspaceActivation)
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
    ) -> Result<(SourceObservation, Vec<u8>), FailureCode> {
        use crate::workspace::{
            observation::{
                ObservationError, ObservationRef, SourceCoverage, SourceReadLimits, SourceRevision,
                read_authorized_source,
            },
            store::{ObservationAdmission, ObservationDraft},
        };
        let authority = self.authority(binding).await?;
        self.shared.active(binding)?;
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

    /// Reconciles only registered paths after native hints, without inferring command effects.
    async fn reconcile_hints(&mut self) {
        let bindings = self.registered.keys().cloned().collect::<Vec<_>>();
        for binding in bindings {
            let hinted = self
                .shared
                .bindings
                .lock()
                .ok()
                .and_then(|mut guard| guard.take_native_change_hint(&binding).ok())
                .unwrap_or(false);
            if hinted {
                let paths = self.registered.get(&binding).cloned().unwrap_or_default();
                for path in paths {
                    if self.observe(&binding, path).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    /// Produces exact-file lexical context until accepted provider service supplies semantic facts.
    async fn context(
        &mut self,
        job: &Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        use crate::intelligence::context::{ContextQuery, lexical_context};
        let binding = job.invocation.binding_ref();
        let path = job.parameters["path"]
            .as_str()
            .ok_or(FailureCode::SourceUnavailable)?;
        let (observed, bytes) = self.observe(binding, path.into()).await?;
        let query = job
            .parameters
            .get("byte_offset")
            .and_then(Value::as_u64)
            .map_or(ContextQuery::File, |byte_offset| ContextQuery::Symbol {
                byte_offset: byte_offset as usize,
            });
        let context = lexical_context(
            &observed,
            &bytes,
            query,
            "accepted semantic provider is not connected",
        )
        .map_err(|_| FailureCode::SourceUnavailable)?;
        let authority = self.authority(binding).await?;
        if !source_matches(&observed) {
            return Err(FailureCode::SourceUnavailable);
        }
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch {
            return Err(FailureCode::SourceUnavailable);
        }
        self.shared.active(binding)?;
        let text = format!(
            "mode: lexical\npath: {path}\nsource_sequence: {}\nauthority_epoch: {}\ncoverage: complete registered path\nreason: accepted semantic provider is not connected\n\n{}",
            observed.sequence(),
            authority.epoch(),
            context.text
        );
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
        reference: &str,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        self.registered.remove(binding);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            ledger.native_epoch.remove(binding);
        }
        if let Some(receipt) = self.grants.remove(binding) {
            self.workspace
                .revoke(
                    OperationId::new(format!("stop-{reference}"))
                        .map_err(|_| FailureCode::Internal)?,
                    &receipt,
                    StopBindingHandoff::Confirmed,
                )
                .await
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text: "Host binding revoked; no owned provider work remains".into(),
                detail_ref: None,
                truncated: false,
            },
            None,
            None,
        ))
    }
}

/// Delivers only a same-binding result after fresh durable authorization, with a final liveness check.
async fn serve_inspection(workspace: DurableWorkspace<'_>, shared: &Shared, request: Inspection) {
    let result = async {
        let active = shared.active(&request.binding)?;
        let (reply, authority, source, native_epoch) = {
            let ledger = shared.ledger.lock().map_err(|_| FailureCode::Internal)?;
            let detail = ledger
                .details
                .get(&request.reference)
                .filter(|detail| detail.binding == request.binding)
                .ok_or(FailureCode::InvalidDetail)?;
            (
                detail.reply.clone(),
                detail.authority.clone(),
                detail.source.clone(),
                detail.native_epoch,
            )
        };
        if let Some(authority) = authority {
            workspace
                .authorize(&authority, &active)
                .await
                .map_err(|_| FailureCode::WorkspaceAuthority)?;
        }
        if let Some(source) = source
            && !source_matches(&source)
        {
            return Err(FailureCode::SourceUnavailable);
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
            return Err(FailureCode::SourceUnavailable);
        }
        shared.active(&request.binding)?;
        Ok::<_, FailureCode>(reply)
    }
    .await;
    let _ = request
        .reply
        .send(result.unwrap_or_else(|code| PeerReply::Error { code }));
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
