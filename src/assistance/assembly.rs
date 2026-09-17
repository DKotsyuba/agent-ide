//! Explicit Codex/Claude ingress and finite dispatch into one daemon-owned product worker.

/// Stable closed result types shared by existing callers of the assembly boundary.
pub use super::reply::{MissingPeer, PeerReply};
use super::{
    claude_worker::{
        AcceptedIdentity, ClaudeOperatorProfile, HelperActor, HelperBudgets, HelperJob,
        HelperLanguage, HelperOperation, HelperProvider, HelperPyrightProfile,
        HelperTypeScriptFileV1, HelperTypeScriptProfileV1, LaunchLedger, RustEffectiveSettings,
    },
    host_binding::{
        BindingStatus, HostBindingGuard, HostKind, ValidatedInvocation, parse_candidate,
        parse_channel_session, parse_claude_call_id, parse_claude_hook_event, parse_hook_event,
        parse_host_kind, parse_observed_sandbox_state,
    },
    launcher::{AcceptedProviderSettings, LaunchTarget, LauncherConfig},
    reply::FailureCode,
    worker::WorkerHandle,
};
use crate::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, AssistanceMethod,
};
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
    /// Outstanding Claude foreground-helper tickets; empty and unused for every Codex operation.
    launches: Arc<Mutex<LaunchLedger>>,
    /// Absolute path of this executable, used to render the exact helper command.
    ///
    /// `None` when the running binary could not be resolved, which leaves the Claude execution
    /// path unavailable rather than guessing a command the recognizer could never match.
    helper_binary: Option<std::path::PathBuf>,
    /// Daemon runtime directory, captured at initialize for the helper's socket argument.
    runtime_dir: Arc<Mutex<Option<std::path::PathBuf>>>,
    /// Private helper claim/finish endpoint, bound once at initialize and unlinked on drop.
    endpoint: Mutex<Option<super::claude_helper::HelperEndpoint>>,
    /// The daemon's single physical-effect admission owner.
    ///
    /// Held here so the same controller reaches both the worker and the Claude launch ledger:
    /// ordinary operations and foreground helper children draw from one configured global budget.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Enables direct trusted Codex metadata binding only for an owned managed-MCP daemon.
    managed_codex: bool,
}

/// Returns a monotonic millisecond reading for ticket deadlines.
///
/// The value is only ever compared against other readings from this function within one daemon
/// boot; it is not a wall clock and carries no host, actor or timing information off the daemon.
pub(crate) fn monotonic_ms() -> u64 {
    static ORIGIN: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ORIGIN
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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
            launches: Arc::new(Mutex::new(LaunchLedger::new(admission.clone()))),
            admission,
            helper_binary: std::env::current_exe()
                .ok()
                .filter(|path| path.is_absolute()),
            runtime_dir: Arc::new(Mutex::new(None)),
            endpoint: Mutex::new(None),
            managed_codex: false,
        }
    }
}
impl ProductDispatcher {
    /// Installs one immutable trusted map; peer startup waits for Application's exclusive daemon lock.
    pub fn with_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::default();
        if let Some(scope) = dispatcher.scope {
            dispatcher.worker = Some(WorkerHandle::new(
                dispatcher.bindings.clone(),
                launcher,
                scope,
                dispatcher.admission.clone(),
            ));
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
    /// Derives the same opaque channel for hook/MCP input under this exact daemon nonce.
    fn channel(&self, attachment: &str) -> Option<super::host_binding::ChannelSessionRef> {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.scope?);
        hash.update(attachment.as_bytes());
        parse_channel_session(hash.finalize().to_hex().as_bytes()).ok()
    }
    /// Mints one single-use foreground-helper ticket and returns its bounded pending instruction.
    ///
    /// Performs no Git, source, provider or process work: it only records what a later helper
    /// would be permitted to do. Returns a closed `Error` when the target has no accepted strict
    /// Claude operator profile, when this executable or runtime directory could not be resolved,
    /// or when bounded ticket capacity is full. Immediately before minting, the exact binding is
    /// revalidated while the launch ledger lock is held, so Stop linearizes before or after mint
    /// without allowing a ticket onto a binding already closed to external admission. Absence of
    /// a profile is always unavailability and never a fallback to unrestricted execution.
    fn mint_claude(
        &self,
        invocation: &ValidatedInvocation,
        tool: super::facade::AssistanceTool,
        parameters: &Value,
        attachment: &str,
    ) -> PeerReply {
        let error = |code| PeerReply::Error { code };
        let Some(worker) = &self.worker else {
            return PeerReply::Unavailable {
                reason: MissingPeer::WorkspaceActivation,
            };
        };
        let Some(target) = worker.target(attachment) else {
            return error(FailureCode::LauncherConfiguration);
        };
        // An absent or unaccepted operator profile leaves the Claude path unavailable.
        let Some(profile) = target.claude_profile else {
            return error(FailureCode::ExecutionProfile);
        };
        if let Err(code) = ClaudeOperatorProfile::validate(&profile) {
            return error(code);
        }
        let (Some(binary), Ok(runtime_dir)) = (&self.helper_binary, self.runtime_dir.lock()) else {
            return error(FailureCode::Internal);
        };
        let Some(runtime_dir) = runtime_dir.clone() else {
            return error(FailureCode::Internal);
        };
        let Some(scope) = self.scope else {
            return error(FailureCode::Internal);
        };
        let operation = match tool {
            super::facade::AssistanceTool::Start => HelperOperation::Start,
            super::facade::AssistanceTool::Context => HelperOperation::Context,
            super::facade::AssistanceTool::Diff => HelperOperation::Diff,
            super::facade::AssistanceTool::Edit => HelperOperation::Edit,
            _ => return error(FailureCode::Internal),
        };
        let Ok(mut launches) = self.launches.lock() else {
            return error(FailureCode::Internal);
        };
        // The handle is derived from the private daemon nonce and the exact binding generation, so
        // it is unguessable and cannot be replayed into a different boot or generation.
        let mut hash = blake3::Hasher::new();
        hash.update(&scope);
        hash.update(&invocation.binding_ref().fingerprint());
        hash.update(invocation.call_id().as_bytes());
        let detail_ref = hash.finalize().to_hex().to_string();
        let command = LaunchLedger::helper_command(binary, &runtime_dir, attachment, &detail_ref);
        let state = match operation {
            HelperOperation::Start => None,
            _ => match worker.claude_state(invocation.binding_ref()) {
                Some(state) => Some(state),
                None => return error(FailureCode::WorkspaceAuthority),
            },
        };
        let provider = if matches!(operation, HelperOperation::Context | HelperOperation::Edit) {
            let required = match parameters["path"]
                .as_str()
                .and_then(|path| path.rsplit('.').next())
            {
                Some("go") => Some(AcceptedProviderSettings::GoplsDefaults),
                Some("rs") => Some(AcceptedProviderSettings::RustCachePrimingDisabledV1),
                Some("py") | Some("pyi") => Some(AcceptedProviderSettings::PyrightDefaultsV1),
                Some("js") | Some("jsx") | Some("ts") | Some("tsx") => {
                    Some(AcceptedProviderSettings::TypeScriptDefaultsV1)
                }
                _ => None,
            };
            required.and_then(|settings| {
                let cache = state.as_ref()?.caches.iter().find_map(|(accepted, path)| {
                    (*accepted == settings).then_some(path.as_str())
                })?;
                Self::helper_provider(&target, settings, cache)
            })
        } else {
            None
        };
        let edit_source = if operation == HelperOperation::Edit {
            let (Some(reference), Some(path)) = (
                parameters.get("source_ref").and_then(Value::as_str),
                parameters.get("path").and_then(Value::as_str),
            ) else {
                return error(FailureCode::SourceUnavailable);
            };
            match worker.claude_edit_source(invocation.binding_ref(), reference, path) {
                Some(source) => Some(source),
                None => return error(FailureCode::SourceUnavailable),
            }
        } else {
            None
        };
        let job = HelperJob {
            protocol: super::claude_worker::HELPER_PROTOCOL,
            operation,
            candidate: target.candidate.clone(),
            git: target.git.path.clone(),
            canonical_root: state.as_ref().map(|state| state.scope.root.clone()),
            scope: state.as_ref().map(|state| state.scope.clone()),
            baseline: (operation == HelperOperation::Diff)
                .then(|| state.as_ref().expect("Diff state exists").baseline.clone()),
            provider,
            edit_source,
            parameters: parameters.clone(),
            budgets: HelperBudgets {
                // A Diff helper walks every tracked path of the snapshot scope, and each distinct
                // blob costs one captured read child plus one verification child. The snapshot
                // itself is bounded by MAX_SNAPSHOT_PATHS and MAX_SNAPSHOT_BLOB_BYTES, so the
                // Diff ceilings below cover that whole bounded walk (a small fixed margin for the
                // metadata queries) instead of the per-operation default, which real repositories
                // with more than a few dozen tracked files would otherwise exceed and fail closed
                // as `capacity` without any write.
                output_bytes: if operation == HelperOperation::Diff {
                    worker
                        .limits()
                        .output_bytes
                        .max(crate::workspace::git::snapshot::MAX_SNAPSHOT_BLOB_BYTES)
                } else {
                    worker.limits().output_bytes
                },
                processes: match operation {
                    HelperOperation::Start => 6,
                    HelperOperation::Context => 2,
                    HelperOperation::Diff => 1024,
                    HelperOperation::Edit => 2,
                },
                deadline_ms: worker.limits().operation_ms,
            },
        };
        let Ok(actor) = HelperActor::new(invocation.actor_id(), None) else {
            return error(FailureCode::Internal);
        };
        // The helper is itself an accepted executable, so its own identity is carried and its
        // current bytes are rechecked at this launch boundary rather than being trusted from a
        // `current_exe()` path alone. The children the job may start keep their accepted
        // fingerprints alongside it. Neither is OS attestation: it is the same accepted-executable
        // contract the managed path already applies, extended to this launch.
        let helper = match Self::helper_identity(binary) {
            Ok(helper) => helper,
            Err(code) => return error(code),
        };
        let mut children = Vec::new();
        for accepted in std::iter::once(&target.git)
            .chain(target.providers.iter().map(|provider| &provider.executable))
        {
            match AcceptedIdentity::new(accepted.path.clone(), &accepted.identity, &accepted.blake3)
            {
                Ok(identity) => children.push(identity),
                Err(code) => return error(code),
            }
        }
        let deadline = monotonic_ms().saturating_add(worker.limits().operation_ms);
        let Ok(bindings) = self.bindings.lock() else {
            return error(FailureCode::Internal);
        };
        if bindings.check_active(invocation.binding_ref()).is_err() {
            return error(FailureCode::WorkspaceAuthority);
        }
        match launches.mint(
            &detail_ref,
            invocation.binding_ref().clone(),
            actor,
            attachment,
            command.clone(),
            job,
            deadline,
            helper,
            children,
        ) {
            Ok(()) | Err(FailureCode::Conflict) => PeerReply::Pending {
                detail_ref,
                helper: Some(command),
            },
            Err(code) => error(code),
        }
    }

    /// Retrieves an already settled helper result without any daemon source or provider read.
    ///
    /// Only the exact owning binding generation can retrieve a handle. A still-unsettled operation
    /// answers `Pending` without re-issuing the command, and every failed, expired, denied or
    /// uncertain outcome becomes a closed typed error, or for Edit its required typed no-effect /
    /// unknown Changes result, rather than a fabricated success.
    async fn retrieve_claude(
        &self,
        invocation: &ValidatedInvocation,
        detail_ref: &str,
        attachment: &str,
    ) -> PeerReply {
        use super::claude_worker::{Delivery, HelperOutcome};
        let owner = invocation.binding_ref().fingerprint();
        // The settled token is minted under the ledger lock and the lock is released before any
        // await: no daemon lock crosses the Worker call below.
        let (delivery, settled) = {
            let Ok(launches) = self.launches.lock() else {
                return PeerReply::Error {
                    code: FailureCode::Internal,
                };
            };
            if !launches.owned_by(detail_ref, owner) {
                return PeerReply::Error {
                    code: FailureCode::InvalidDetail,
                };
            }
            (
                launches.delivery(detail_ref),
                launches.settled(detail_ref, owner),
            )
        };
        match delivery {
            Delivery::Ready(result) => match result.outcome {
                // A settled helper frame is pre-authority *evidence*, never authority. It is
                // routed through the sole Worker queue, which resolves a durable worktree, mints
                // the `StartReceipt` and retains the grant. Without a settled token — which
                // arbitrary helper JSON cannot produce — nothing is published at all.
                HelperOutcome::Complete { .. } => {
                    let Some(settled) = settled else {
                        return PeerReply::Error {
                            code: FailureCode::WorkspaceAuthority,
                        };
                    };
                    let Some(worker) = &self.worker else {
                        return PeerReply::Unavailable {
                            reason: MissingPeer::WorkspaceActivation,
                        };
                    };
                    let reply = worker
                        .complete_claude(invocation.clone(), attachment, settled)
                        .await;
                    // The bounded lease is released only once the daemon has actually consumed the
                    // settled evidence, and only on the same positive proof that minted it.
                    if matches!(reply, PeerReply::Complete { .. } | PeerReply::Edit { .. })
                        && let Ok(mut launches) = self.launches.lock()
                    {
                        launches.release_settled(detail_ref, owner);
                    }
                    reply
                }
                HelperOutcome::Failed { code } => PeerReply::Error { code },
            },
            Delivery::Waiting => PeerReply::Pending {
                detail_ref: detail_ref.to_owned(),
                helper: None,
            },
            Delivery::Failed(code) => PeerReply::Error { code },
            Delivery::DeadlineNoEffect(job) => {
                let Some(worker) = &self.worker else {
                    return PeerReply::Unavailable {
                        reason: MissingPeer::WorkspaceActivation,
                    };
                };
                let reply = worker
                    .complete_claude_edit_terminal(
                        invocation.clone(),
                        job.parameters.clone(),
                        attachment,
                        crate::changes::edit::EditOutcome::DeadlineNoEffect,
                    )
                    .await;
                if matches!(reply, PeerReply::Edit { .. })
                    && let Ok(mut launches) = self.launches.lock()
                {
                    launches.retire_no_effect_edit(detail_ref, owner);
                }
                reply
            }
            Delivery::OutcomeUnknown(job) => {
                let Some(worker) = &self.worker else {
                    return PeerReply::Unavailable {
                        reason: MissingPeer::WorkspaceActivation,
                    };
                };
                worker
                    .complete_claude_edit_terminal(
                        invocation.clone(),
                        job.parameters.clone(),
                        attachment,
                        crate::changes::edit::EditOutcome::OutcomeUnknown,
                    )
                    .await
            }
        }
    }

    /// Measures the running executable that the exact helper command names.
    ///
    /// `current_exe()` alone is only a path, and a path is not an identity: the bytes behind it can
    /// change between minting a ticket and settling it. This reads the binary's current bytes,
    /// bounded, and records their BLAKE3 digest in the ticket so a later settlement is checked
    /// against a measured identity rather than a filename.
    ///
    /// The honest limitation is stated rather than strengthened: this is a same-user self
    /// measurement taken by the daemon, not third-party attestation, and it cannot close the race
    /// between measuring the file and the host actually exec'ing it. Returns
    /// [`FailureCode::ExecutionProfile`] when the file is missing, is not a regular file, exceeds
    /// the bounded read, or cannot be read — all of which leave the Claude path unavailable.
    fn helper_identity(
        binary: &Path,
    ) -> Result<super::claude_worker::AcceptedIdentity, FailureCode> {
        /// Bounds one helper-binary measurement; larger files are refused, never partially hashed.
        const MAX_HELPER_BYTES: u64 = 512 * 1024 * 1024;
        let metadata = std::fs::metadata(binary).map_err(|_| FailureCode::ExecutionProfile)?;
        if !metadata.is_file() || metadata.len() > MAX_HELPER_BYTES {
            return Err(FailureCode::ExecutionProfile);
        }
        let mut file = std::fs::File::open(binary).map_err(|_| FailureCode::ExecutionProfile)?;
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|_| FailureCode::ExecutionProfile)?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        AcceptedIdentity::new(
            binary.to_path_buf(),
            "agent-ide-claude-worker",
            hash.finalize().to_hex().as_str(),
        )
    }

    /// Selects the single exclusive provider a Claude helper may run for this target.
    ///
    /// Returns `None` when the target configures no provider, which leaves the helper to perform
    /// Git-only work. Rust is always pinned to disabled cache priming and disabled proc-macro
    /// expansion. Shared multi-worktree gopls is deliberately not offered here: a foreground
    /// helper cannot retain a safe shared listener, so Go runs on a helper-private view only.
    /// Pyright likewise remains one helper-private, worktree-isolated stdio child.
    fn helper_provider(
        target: &LaunchTarget,
        settings: AcceptedProviderSettings,
        cache_namespace: &str,
    ) -> Option<HelperProvider> {
        let provider = target
            .providers
            .iter()
            .find(|provider| provider.settings == settings)?;
        let (language, rust_settings, pyright, typescript) = match provider.settings {
            AcceptedProviderSettings::GoplsDefaults => (HelperLanguage::Go, None, None, None),
            AcceptedProviderSettings::RustCachePrimingDisabledV1 => (
                HelperLanguage::Rust,
                Some(RustEffectiveSettings {
                    cache_priming: false,
                    proc_macro: false,
                }),
                None,
                None,
            ),
            AcceptedProviderSettings::PyrightDefaultsV1 => {
                let node = provider.node.as_ref()?;
                (
                    HelperLanguage::Python,
                    None,
                    Some(HelperPyrightProfile {
                        script: provider.executable.path.clone(),
                        script_identity: provider.executable.identity.clone(),
                        script_blake3: provider.executable.blake3.clone(),
                        node: node.path.clone(),
                        node_identity: node.identity.clone(),
                        node_blake3: node.blake3.clone(),
                    }),
                    None,
                )
            }
            AcceptedProviderSettings::TypeScriptDefaultsV1 => {
                if !provider.typescript_claude_accepted() {
                    return None;
                }
                let node = provider.node.as_ref()?;
                let bundle = provider.typescript.as_ref()?;
                let file =
                    |value: &super::launcher::AcceptedTypeScriptFileV1| HelperTypeScriptFileV1 {
                        path: value.path.clone(),
                        blake3: value.blake3.clone(),
                        bytes: value.bytes,
                    };
                (
                    HelperLanguage::TypeScript,
                    None,
                    None,
                    Some(HelperTypeScriptProfileV1 {
                        node: node.path.clone(),
                        node_version: node.identity.clone(),
                        node_blake3: node.blake3.clone(),
                        bridge: HelperTypeScriptFileV1 {
                            path: provider.executable.path.clone(),
                            blake3: provider.executable.blake3.clone(),
                            bytes: bundle.bridge_bytes,
                        },
                        bridge_version: bundle.bridge_version.clone(),
                        tsserver: file(&bundle.tsserver),
                        typescript_version: bundle.typescript_version.clone(),
                        closure: bundle.closure.iter().map(file).collect(),
                    }),
                )
            }
        };
        Some(HelperProvider {
            executable: provider.executable.path.clone(),
            version: provider.executable.identity.clone(),
            language,
            rust_settings,
            pyright,
            typescript,
            toolchain: provider.toolchain.clone(),
            cargo: provider.cargo.as_ref().map(|program| program.path.clone()),
            cargo_version: provider.cargo_version.clone(),
            rustc: provider.rustc.as_ref().map(|program| program.path.clone()),
            rustc_version: provider.rustc_version.clone(),
            trust: provider.trust.clone(),
            cache_namespace: cache_namespace.to_owned(),
        })
    }

    /// Retires overdue helper tickets at every ingress, without a timer task.
    ///
    /// Sweeping on ingress keeps deadline handling finite while the daemon is doing work anyway.
    /// A non-Edit ticket whose launch never happened disappears. Prepared Edit tickets instead
    /// retain typed `deadline_no_effect` for retrieval; claimed unsettled Edit stays quarantined
    /// and retrieves as `outcome_unknown`.
    fn sweep_launches(&self) {
        if let Ok(mut launches) = self.launches.lock() {
            launches.expire(monotonic_ms());
        }
    }
    /// Parses separated ingress and commits binding transitions before queue, inspection or stop I/O.
    async fn handle(&self, request: &AssistanceDispatch) -> Option<PeerReply> {
        self.sweep_launches();
        match request {
            AssistanceDispatch::HookSubmit(hook) => {
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(hook.opaque_attachment()))
                {
                    return None;
                }
                let observation: Value =
                    serde_json::from_str(hook.sanitized_observation_json().as_str()).ok()?;
                let object = observation.as_object()?;
                if object.len() != 9 {
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
                        json!({"hook_event_name":phase,"session_id":object.get("actor_id")?,"tool_use_id":object.get("call_id")?})
                            .to_string().as_bytes(),
                    ),
                    "claude" => parse_claude_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"agent_type":object.get("agent_type")?,"tool_use_id":object.get("call_id")?})
                            .to_string().as_bytes(),
                    ),
                    _ => return None,
                }
                .ok()?;
                // Reattach the relayed shell launch, if the adapter selected one. A malformed or
                // over-bound relay simply yields no launch: the event stays a plain observation.
                let event = match (
                    object.get("launch_command").and_then(Value::as_str),
                    object.get("launch_background").and_then(Value::as_bool),
                ) {
                    (Some(command), Some(background)) => event
                        .clone()
                        .with_launch(command, background)
                        .or(Some(event))?,
                    _ => event,
                };
                if event.optional_call_id().unwrap_or("post-tool-batch") != hook.correlation_id() {
                    return None;
                }
                let channel = self.channel(hook.opaque_attachment())?;
                // Exact-byte recognition of a foreground helper launch. This is silent by
                // construction: no permission decision, no updated input, no rewritten command.
                // Ordinary host permission and sandbox evaluation of the unchanged command is
                // what authorizes the launch; a non-matching payload is discarded here.
                if let (Some(launch), Some(call_id)) = (event.launch(), event.optional_call_id())
                    // The hook actor is already the distinguishing identity: a subagent's exact
                    // agent_id, or the root session_id for a parent. Both sides derive it the
                    // same way, so a parent and its child are never interchangeable.
                    && let Ok(actor) = HelperActor::new(event.actor_id(), event.session_id())
                    && let Ok(mut launches) = self.launches.lock()
                {
                    launches.recognize(
                        launch.command(),
                        launch.run_in_background(),
                        call_id,
                        &actor,
                        monotonic_ms(),
                    );
                }
                let call_id = event.optional_call_id().map(str::to_owned);
                let failed = event.failed();
                let status = self.bindings.lock().ok()?.observe_hook(event, channel);
                match status {
                    BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
                    BindingStatus::Settled(_) => Some(PeerReply::HookSettled {}),
                    BindingStatus::NativeObserved(binding) => {
                        // A helper's own post settles the operation it belongs to. It must not be
                        // treated as a generic native edit, or the helper would invalidate the
                        // very result it just produced; only unrelated posts advance the epoch.
                        if let Some(call_id) = call_id.as_deref()
                            && let Ok(mut launches) = self.launches.lock()
                            && launches.owns_post(call_id)
                        {
                            // A failed helper post settles the operation as failed rather than
                            // leaving it pending: the tool ran, so expiry must not be the only
                            // thing that ever resolves it.
                            let _ = launches.settle_post(call_id, !failed);
                            return Some(PeerReply::NativeHookObserved {});
                        }
                        if let Some(worker) = &self.worker {
                            worker.native_hint(binding.clone());
                            if let Some(text) = worker.take_current_feedback(binding).await {
                                return Some(PeerReply::Feedback { text });
                            }
                        }
                        Some(PeerReply::NativeHookObserved {})
                    }
                    _ => None,
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
                    AssistanceMethod::HookSubmit => return None,
                };
                let call =
                    super::facade::validate_call(tool, object.get("parameters")?.clone()).ok()?;
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(method.opaque_attachment()))
                {
                    return None;
                }
                let channel = self.channel(method.opaque_attachment())?;
                let (invocation, observed) = {
                    let mut bindings = self.bindings.lock().ok()?;
                    let status = match host {
                        HostKind::Codex => {
                            let candidate = parse_candidate(meta).ok()?;
                            if candidate.call_id() != method.correlation_id() {
                                return None;
                            }
                            if self.managed_codex && method.method() == AssistanceMethod::Start {
                                bindings.establish_managed_codex_start(candidate, channel)
                            } else if self.managed_codex {
                                bindings.validate_managed_codex_active(candidate, channel)
                            } else if method.method() == AssistanceMethod::Start {
                                bindings.establish_start(candidate, channel)
                            } else {
                                bindings.validate_active(candidate, channel)
                            }
                        }
                        HostKind::Claude => {
                            let call_id = parse_claude_call_id(meta).ok()?;
                            if call_id != method.correlation_id() {
                                return None;
                            }
                            if method.method() == AssistanceMethod::Start {
                                bindings.establish_start_claude(&call_id, channel)
                            } else {
                                bindings.validate_active_claude(&call_id, channel)
                            }
                        }
                    };
                    let BindingStatus::Validated(invocation) = status else {
                        return None;
                    };
                    if method.method() == AssistanceMethod::Stop && host != HostKind::Claude {
                        bindings.stop_binding(invocation.binding_ref()).ok()?;
                        (invocation, None)
                    } else if host == HostKind::Claude {
                        // Claude never advertises or returns `codex/sandbox-state-meta`. Instead
                        // of inventing sandbox authority it was never given, this operation runs
                        // in a foreground helper that inherits the host's own real sandbox.
                        if method.method() == AssistanceMethod::Stop {
                            bindings.begin_stop(invocation.binding_ref()).ok()?;
                        } else {
                            bindings.consume_active(invocation.binding_ref()).ok()?;
                        }
                        (invocation, None)
                    } else {
                        let active = bindings.consume_active(invocation.binding_ref()).ok()?;
                        // Claude never advertises or returns `codex/sandbox-state-meta`; establishing
                        // its correlation must never invent sandbox authority it was never given.
                        let observed = match parse_observed_sandbox_state(
                            meta,
                            &invocation,
                            &active,
                            host == HostKind::Codex,
                        ) {
                            Ok(observed) => observed,
                            Err(_) => {
                                if method.method() == AssistanceMethod::Start
                                    && invocation.created_binding()
                                {
                                    let _ = bindings.stop_binding(invocation.binding_ref());
                                }
                                return Some(PeerReply::Error {
                                    code: FailureCode::SandboxState,
                                });
                            }
                        };
                        if crate::execution::HostSandboxState::parse(Some(
                            observed.state().as_json().clone(),
                        ))
                        .is_err()
                        {
                            if method.method() == AssistanceMethod::Start
                                && invocation.created_binding()
                            {
                                let _ = bindings.stop_binding(invocation.binding_ref());
                            }
                            return Some(PeerReply::Error {
                                code: FailureCode::SandboxState,
                            });
                        }
                        (invocation, Some(observed))
                    }
                };
                let Some(worker) = &self.worker else {
                    if host == HostKind::Claude
                        && method.method() == AssistanceMethod::Stop
                        && let Ok(mut bindings) = self.bindings.lock()
                    {
                        let _ = bindings.stop_binding(invocation.binding_ref());
                    }
                    return Some(if method.method() == AssistanceMethod::Stop {
                        PeerReply::HostStopped {}
                    } else {
                        PeerReply::Unavailable {
                            reason: MissingPeer::WorkspaceActivation,
                        }
                    });
                };
                if host == HostKind::Claude {
                    let established = method.method() == AssistanceMethod::Start;
                    return Some(match method.method() {
                        // External admission is already closed under the binding lock. Settle ready
                        // Edits through the exact stopping generation, then revoke every remaining
                        // ticket before stopping durable Workspace authority.
                        AssistanceMethod::Stop => {
                            let owner = invocation.binding_ref().fingerprint();
                            let mut settlement_error = None;
                            let (ready_edits, terminals) = match self.launches.lock() {
                                Ok(mut launches) => {
                                    let mut ready = Vec::new();
                                    for detail_ref in launches.ready_edit_references(owner) {
                                        if let Some(settled) = launches.settled(&detail_ref, owner)
                                            && launches.release_settled(&detail_ref, owner)
                                        {
                                            ready.push(settled);
                                        } else {
                                            settlement_error = Some(PeerReply::Error {
                                                code: FailureCode::Internal,
                                            });
                                        }
                                    }
                                    (ready, launches.revoke(owner))
                                }
                                Err(_) => {
                                    settlement_error = Some(PeerReply::Error {
                                        code: FailureCode::Internal,
                                    });
                                    (Vec::new(), Vec::new())
                                }
                            };
                            for settled in ready_edits {
                                let completion = worker
                                    .complete_claude_edit_for_stop(
                                        invocation.clone(),
                                        method.opaque_attachment(),
                                        settled,
                                    )
                                    .await;
                                if !matches!(&completion, PeerReply::Edit { .. })
                                    && settlement_error.is_none()
                                {
                                    settlement_error = Some(completion);
                                }
                            }
                            for (detail_ref, job, outcome) in terminals {
                                let terminal = worker
                                    .complete_claude_edit_terminal(
                                        invocation.clone(),
                                        job.parameters,
                                        method.opaque_attachment(),
                                        outcome,
                                    )
                                    .await;
                                if !matches!(&terminal, PeerReply::Edit { .. })
                                    && settlement_error.is_none()
                                {
                                    settlement_error = Some(terminal);
                                }
                                if outcome == crate::changes::edit::EditOutcome::DeadlineNoEffect
                                    && let Ok(mut launches) = self.launches.lock()
                                {
                                    launches.retire_no_effect_edit(&detail_ref, owner);
                                }
                            }
                            let reply = worker
                                .stop(invocation.clone(), method.opaque_attachment())
                                .await;
                            if let Ok(mut bindings) = self.bindings.lock() {
                                let _ = bindings.stop_binding(invocation.binding_ref());
                            }
                            settlement_error.unwrap_or(reply)
                        }
                        // Context/Diff preserve the shared facade's optional detail retrieval:
                        // the handle names already-settled work and never starts another helper.
                        AssistanceMethod::Context | AssistanceMethod::Diff
                            if call.parameters().get("detail_ref").is_some() =>
                        {
                            self.retrieve_claude(
                                &invocation,
                                call.parameters()["detail_ref"].as_str()?,
                                method.opaque_attachment(),
                            )
                            .await
                        }
                        // Inspect is pure retrieval: same binding/generation, no daemon source read.
                        AssistanceMethod::Inspect => {
                            self.retrieve_claude(
                                &invocation,
                                call.parameters()["detail_ref"].as_str()?,
                                method.opaque_attachment(),
                            )
                            .await
                        }
                        // EYES-r2: the problems kind is answered from the daemon's in-memory
                        // problem source and never mints or dispatches a foreground helper.
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
                        AssistanceMethod::Edit => {
                            let prepared = worker
                                .prepare_claude_edit(
                                    invocation.clone(),
                                    call.parameters().clone(),
                                    method.opaque_attachment(),
                                )
                                .await;
                            if matches!(prepared, PeerReply::HookObserved {}) {
                                let minted = self.mint_claude(
                                    &invocation,
                                    tool,
                                    call.parameters(),
                                    method.opaque_attachment(),
                                );
                                if matches!(
                                    minted,
                                    PeerReply::Error { .. } | PeerReply::Unavailable { .. }
                                ) {
                                    worker
                                        .complete_claude_edit_terminal(
                                            invocation.clone(),
                                            call.parameters().clone(),
                                            method.opaque_attachment(),
                                            crate::changes::edit::EditOutcome::UnavailableBeforeDispatch,
                                        )
                                        .await
                                } else {
                                    minted
                                }
                            } else {
                                prepared
                            }
                        }
                        _ => {
                            let reply = self.mint_claude(
                                &invocation,
                                tool,
                                call.parameters(),
                                method.opaque_attachment(),
                            );
                            // This call created the generation and then refused its own
                            // admission. Remove exactly that generation so no false active
                            // binding remains, without touching a concurrently valid retry.
                            if established
                                && matches!(
                                    reply,
                                    PeerReply::Error { .. } | PeerReply::Unavailable { .. }
                                )
                                && let Ok(mut bindings) = self.bindings.lock()
                            {
                                let _ = bindings.rollback_established(invocation.binding_ref());
                            }
                            reply
                        }
                    });
                }
                if self.managed_codex
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
                Some(match method.method() {
                    AssistanceMethod::Stop => {
                        worker.stop(invocation, method.opaque_attachment()).await
                    }
                    AssistanceMethod::Inspect => {
                        worker
                            .inspect(
                                invocation.binding_ref().clone(),
                                call.parameters()["detail_ref"].as_str()?.to_owned(),
                                observed?,
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
                                observed,
                                tool,
                                call.parameters().clone(),
                                method.opaque_attachment(),
                            )
                            .await
                    }
                })
            }
        }
    }
}
impl AssistanceDispatcher for ProductDispatcher {
    /// Opens configured peers once, only after Application owns the daemon endpoint lock.
    fn initialize<'a>(
        &'a self,
        runtime_dir: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>> {
        Box::pin(async move {
            // Captured once for the helper command; the daemon never derives it from a hook.
            if let Ok(mut captured) = self.runtime_dir.lock() {
                *captured = Some(runtime_dir.to_path_buf());
            }
            // Bound after Application owns the daemon lock, so no two boots share an endpoint.
            // Failure to bind simply leaves the Claude path unavailable; Codex is unaffected.
            if let Ok(mut endpoint) = self.endpoint.lock() {
                let bindings = self.bindings.clone();
                *endpoint = super::claude_helper::serve(
                    runtime_dir,
                    self.launches.clone(),
                    // The endpoint may only ask whether a generation is live; it can never
                    // establish, roll back or stop one.
                    std::sync::Arc::new(move |binding: &super::host_binding::BindingRef| {
                        bindings
                            .lock()
                            .is_ok_and(|mut guard| guard.consume_active(binding).is_ok())
                    }),
                );
            }
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
            // Drops the private endpoint before provider cleanup: no helper may claim during
            // shutdown, and the socket path never outlives the daemon that bound it.
            if let Ok(mut endpoint) = self.endpoint.lock() {
                endpoint.take();
            }
            match &self.worker {
                Some(worker) => worker
                    .shutdown()
                    .await
                    .map_err(|_| AssistanceDispatchUnavailable),
                None => Ok(()),
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
            let result = self
                .handle(&request)
                .await
                .unwrap_or(PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                });
            if let Some(telemetry) = self.worker.as_ref().and_then(WorkerHandle::telemetry) {
                match &request {
                    AssistanceDispatch::HookSubmit(_) => {
                        // Hook payloads are intentionally never accepted by telemetry adapters.
                    }
                    AssistanceDispatch::MethodDispatch(method) => {
                        let tool = match method.method() {
                            AssistanceMethod::Start => Some(super::facade::AssistanceTool::Start),
                            AssistanceMethod::Context => {
                                Some(super::facade::AssistanceTool::Context)
                            }
                            AssistanceMethod::Diff => Some(super::facade::AssistanceTool::Diff),
                            AssistanceMethod::Inspect => {
                                Some(super::facade::AssistanceTool::Inspect)
                            }
                            AssistanceMethod::Stop => Some(super::facade::AssistanceTool::Stop),
                            AssistanceMethod::Edit => Some(super::facade::AssistanceTool::Edit),
                            AssistanceMethod::HookSubmit => None,
                        };
                        if let Some(tool) = tool {
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
            }
            let reply = result.encode().ok_or(AssistanceDispatchUnavailable)?;
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
    assert_eq!(dispatcher.handle(&request).await, None);
}

/// Proves a Claude start reaches no Workspace and invents no sandbox authority for itself.
///
/// Claude never returns `codex/sandbox-state-meta`. Correlation still succeeds, but the operation
/// is never executed daemon-side: without a configured worker and accepted operator profile it is
/// explicitly unavailable rather than being admitted on fabricated sandbox evidence.
#[tokio::test]
async fn host_shaped_claude_start_never_reaches_workspace_without_sandbox_authority() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"call",
            "session_id":"session",
            "agent_type":null,
            "launch_command":null,
            "launch_background":null,
            "failed":false
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "call", "attachment", observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&hook).await,
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
        dispatcher.handle(&method).await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::WorkspaceActivation
        })
    );

    let next_observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"next",
            "session_id":"session",
            "agent_type":null,
            "launch_command":null,
            "launch_background":null,
            "failed":false
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let next_hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "next", "attachment", next_observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&next_hook).await,
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
    // A follow-up Claude operation on the same attachment is routed to the foreground helper, which
    // has no worker here, so it reports the same honest unavailability the refused Start did. What
    // matters is that it is never a grant and never reaches Workspace: no binding, authority or
    // helper ticket survived the sandbox-authority refusal above.
    assert_eq!(
        dispatcher.handle(&next_method).await,
        Some(PeerReply::Unavailable {
            reason: MissingPeer::WorkspaceActivation
        })
    );
}

/// Builds a configured Claude dispatcher and one active start invocation for the mint/Stop race.
#[cfg(test)]
fn claude_race_fixture() -> (ProductDispatcher, ValidatedInvocation) {
    use crate::execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord};

    let sandbox = HostSandboxState::parse(Some(json!({
        "permissionProfile":{"type":"disabled"},
        "codexLinuxSandboxExe":null,
        "sandboxCwd":"/private/tmp",
        "useLegacyLandlock":false
    })))
    .unwrap();
    let record = PersistedProfileRecord::from_execution_evidence(
        "race-profile",
        1,
        D03ProfileEvidence {
            provider_binary: "race-git".into(),
            toolchain: "race-toolchain".into(),
            configuration: "default".into(),
            trust: "race-local".into(),
            transport: "direct".into(),
            d03_evidence: "race-d03".into(),
        },
        &sandbox,
    )
    .unwrap();
    let git = "/usr/bin/git";
    let executable = json!({
        "path":git,
        "identity":"race-git",
        "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
    });
    let launcher = LauncherConfig::parse(
        json!({
            "version":1,
            "limits":{"queued":8,"details":8,"operation_ms":1000,"output_bytes":4096},
            "targets":[{
                "attachment":"race-attachment",
                "candidate":"/private/tmp/race-worktree",
                "git":executable,
                "codex":executable,
                "providers":[],
                "profiles":[{
                    "record":serde_json::from_str::<Value>(&record.to_json()).unwrap(),
                    "sandbox_state":serde_json::from_str::<Value>(sandbox.sandbox_state_json()).unwrap()
                }],
                "allow_disabled_host":true,
                "claude_profile":{
                    "enabled":true,
                    "fail_if_unavailable":true,
                    "allow_unsandboxed_commands":false,
                    "no_matching_excluded_commands":true,
                    "scope_declared":true,
                    "platform":"mac_os"
                }
            }]
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap();
    let dispatcher = ProductDispatcher::with_launcher(launcher);
    *dispatcher.runtime_dir.lock().unwrap() = Some(std::path::PathBuf::from("/private/tmp"));
    let channel = dispatcher.channel("race-attachment").unwrap();
    let mut bindings = dispatcher.bindings.lock().unwrap();
    assert!(matches!(
        bindings.observe_hook(
            parse_claude_hook_event(
                json!({
                    "hook_event_name":"PreToolUse",
                    "session_id":"race-actor",
                    "tool_use_id":"race-start"
                })
                .to_string()
                .as_bytes()
            )
            .unwrap(),
            channel.clone()
        ),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        bindings.establish_start_claude("race-start", channel)
    else {
        panic!("race fixture binding must validate");
    };
    drop(bindings);
    (dispatcher, invocation)
}

/// Proves Stop wins the exact concurrent boundary before Claude mint can publish a ticket.
#[test]
fn claude_mint_rechecks_binding_after_concurrent_stop_begins() {
    let (dispatcher, invocation) = claude_race_fixture();
    let dispatcher = std::sync::Arc::new(dispatcher);
    let binding = invocation.binding_ref().clone();
    let (stopping, ready) = std::sync::mpsc::sync_channel(0);
    let stop_dispatcher = dispatcher.clone();
    let stop = std::thread::spawn(move || {
        stop_dispatcher
            .bindings
            .lock()
            .unwrap()
            .begin_stop(&binding)
            .unwrap();
        stopping.send(()).unwrap();
        stop_dispatcher
            .launches
            .lock()
            .unwrap()
            .revoke(binding.fingerprint());
    });
    ready.recv().unwrap();
    let reply = dispatcher.mint_claude(
        &invocation,
        super::facade::AssistanceTool::Start,
        &json!({"activation_id":"race-start"}),
        "race-attachment",
    );
    stop.join().unwrap();
    assert_eq!(
        reply,
        PeerReply::Error {
            code: FailureCode::WorkspaceAuthority
        }
    );
    assert!(dispatcher.launches.lock().unwrap().is_empty());
}

/// EYES-r2: a `kind: "problems"` context call on the Claude path never mints a foreground-helper
/// ticket. It must route into the worker's in-memory problem-source path — with no started
/// worker task that path fails at enqueue (`internal`), while the removed short-circuit would
/// have fallen through to helper minting and returned `workspace_authority` instead.
#[tokio::test]
async fn claude_problems_context_short_circuits_before_helper_mint() {
    let (dispatcher, _invocation) = claude_race_fixture();
    // Every ordinary Claude MCP call is armed by its own trusted native pre-hook, so the
    // problems call registers a fresh pre-observation for its own call identity on the exact
    // same channel as the still-active start binding.
    let call_id = "problems-call-1";
    let channel = dispatcher.channel("race-attachment").unwrap();
    {
        let mut bindings = dispatcher.bindings.lock().unwrap();
        assert!(matches!(
            bindings.observe_hook(
                parse_claude_hook_event(
                    json!({
                        "hook_event_name":"PreToolUse",
                        "session_id":"race-actor",
                        "tool_use_id":call_id
                    })
                    .to_string()
                    .as_bytes()
                )
                .unwrap(),
                channel.clone()
            ),
            BindingStatus::PreObserved
        ));
    }
    let request = crate::app::transport::MethodDispatch::new(
        "problems-request".to_owned(),
        call_id.to_owned(),
        "race-attachment".to_owned(),
        AssistanceMethod::Context,
        crate::app::transport::OpaqueJson::from_value(
            &json!({
                "parameters":{"kind":"problems"},
                "host_meta":{"claudecode/toolUseId": call_id}
            }),
            64 * 1024,
        )
        .unwrap(),
    )
    .unwrap();
    let reply = dispatcher
        .handle(&AssistanceDispatch::MethodDispatch(request))
        .await
        .expect("problems dispatch must produce a typed reply");
    assert_eq!(
        reply,
        PeerReply::Error {
            code: FailureCode::Internal
        }
    );
    assert!(!matches!(
        reply,
        PeerReply::Pending {
            helper: Some(_),
            ..
        }
    ));
    assert!(dispatcher.launches.lock().unwrap().is_empty());
}
