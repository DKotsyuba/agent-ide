//! Scoped language-provider adapters for the single product worker and its owned process accounting.

use super::*;
use crate::assistance::launcher::{AcceptedProviderSettings, ProviderLaunch};
use crate::{
    app::cache::{CacheNamespaceId, CacheRoot},
    execution::{
        AdmissionClass, BackendRelease, OwnerId, ProcessError, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderLeaseLimits, ProviderLeaseRegistry, ProviderViewLease,
    },
    intelligence::{
        context::{ContextQuery, ContextResult},
        freshness::{CacheIdentity, CacheLifecycle, ViewGeneration},
        gopls::{GoplsProfile, SharedGopls},
        rust::{
            RustProfile, RustProfileError, RustProfileIdentity, RustProtocolChild,
            RustViewAdmission, RustViews, RustWorktree,
        },
        session::{DiagnosticSnapshot, ProviderSettings, SessionOptions, with_session},
    },
};

/// Couples one semantic context result to diagnostics observed by that exact provider session.
pub(super) struct ProviderContext {
    /// Source and semantic locations returned for the synchronized document generation.
    pub(super) context: ContextResult,
    /// Latest bounded diagnostic push retained by the same session before shutdown.
    pub(super) diagnostics: DiagnosticSnapshot,
}

/// Keeps a shared listener owned until the final logical view is released and reaped.
struct GoBackend {
    /// Exact accepted heavy listener and its per-connection bookkeeping.
    shared: SharedGopls,
    /// Private socket owned by this boot and backend; never reused across daemon restarts.
    socket: std::path::PathBuf,
    /// Monotonic backend generation used in semantic provenance.
    generation: u64,
}

/// Holds the reusable logical view while individual stdio forwarders are closed after each request.
#[derive(Clone)]
struct GoLease {
    /// Full immutable backend compatibility key.
    backend: String,
    /// One exact registry view retained until explicit binding stop.
    lease: ProviderViewLease,
}

/// Provider state owned by the sole worker; no client holds executable or settlement capabilities.
pub(super) struct Providers {
    /// Central typed backend/view accounting; physical limits live in the worker admission controller.
    registry: ProviderLeaseRegistry,
    /// Compatible heavy gopls listeners, counted once across worktree views.
    go: BTreeMap<String, GoBackend>,
    /// One logical Go view for each bound actor/worktree.
    go_views: BTreeMap<BindingRef, GoLease>,
    /// Exclusive Rust generation and source bookkeeping.
    rust: RustViews,
    /// Strictly increasing protocol/backend generation within this boot.
    generation: u64,
    /// Worktree/provider cache owners retained independently from actor bindings.
    caches: BTreeMap<String, CacheLifecycle>,
    /// Cache keys currently used by each actor and quiesced when that actor stops.
    binding_caches: BTreeMap<BindingRef, Vec<String>>,
}
impl Providers {
    /// Creates fixed finite provider bookkeeping without launching processes.
    pub(super) fn new() -> Self {
        Self {
            registry: ProviderLeaseRegistry::new(ProviderLeaseLimits {
                total_views: 64,
                per_backend_views: 64,
            })
            .expect("fixed view limits"),
            go: BTreeMap::new(),
            go_views: BTreeMap::new(),
            rust: RustViews::default(),
            generation: 0,
            caches: BTreeMap::new(),
            binding_caches: BTreeMap::new(),
        }
    }
    /// Mints one checked protocol generation without using timing/PID as actor identity.
    fn next(&mut self) -> Result<u64, FailureCode> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        Ok(self.generation)
    }
}

impl Worker<'_> {
    /// Closes every retained provider view and quiesces its cache before daemon shutdown completes.
    /// Returns the first cleanup failure after still attempting every independently owned backend.
    pub(super) async fn close_all_providers(&mut self) -> Result<(), FailureCode> {
        let bindings = self
            .providers
            .go_views
            .keys()
            .chain(self.providers.binding_caches.keys())
            .chain(self.grants.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let mut failure = None;
        for binding in bindings {
            if let Err(code) = self.close_provider(&binding).await {
                failure.get_or_insert(code);
            }
            self.quiesce_worktree_caches(&binding);
        }
        if !self.uncertain.is_empty() {
            failure.get_or_insert(FailureCode::Internal);
        }
        failure.map_or(Ok(()), Err)
    }

    /// Retains or reopens each configured provider cache under canonical worktree identity.
    ///
    /// A quiescent compatible lifecycle is handed to the incoming binding. Failure leaves the
    /// durable activation valid but returns false so the activation reply cannot claim cache reuse.
    pub(super) fn retain_worktree_caches(
        &mut self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launches: &[ProviderLaunch],
    ) -> bool {
        let Ok(root) = CacheRoot::prepare(self.runtime.join("cache")) else {
            return false;
        };
        let worktree_state = format!(
            "{}:{}",
            authority.worktree().id(),
            authority.worktree().incarnation()
        );
        let mut keys = Vec::with_capacity(launches.len());
        for launch in launches {
            let settings = match launch.settings {
                AcceptedProviderSettings::GoplsDefaults => "gopls-defaults-v1",
                AcceptedProviderSettings::RustCachePrimingDisabledV1 => {
                    "rust-cache-priming-disabled-v1"
                }
            };
            let Some(identity) = CacheIdentity::new(
                launch.executable.identity.clone(),
                settings,
                settings,
                launch.toolchain.clone(),
                launch.trust.clone(),
                worktree_state.clone(),
            ) else {
                return false;
            };
            let key = blake3::hash(
                format!(
                    "{}\0{}\0{}\0{}\0{}\0{}",
                    worktree_state,
                    launch.cache_namespace,
                    launch.executable.identity,
                    settings,
                    launch.toolchain,
                    launch.trust
                )
                .as_bytes(),
            )
            .to_hex()
            .to_string();
            let Some(namespace) = CacheNamespaceId::new(key.clone()) else {
                return false;
            };
            if let Some(cache) = self.providers.caches.get_mut(&key)
                && !cache.handoff(&identity)
            {
                return false;
            }
            let Ok(cache) = CacheLifecycle::retain(&root, namespace, identity) else {
                return false;
            };
            self.providers.caches.insert(key.clone(), cache);
            keys.push(key);
        }
        self.providers.binding_caches.insert(binding.clone(), keys);
        true
    }

    /// Quiesces the stopped actor's cache owners without deleting their worktree namespaces.
    pub(super) fn quiesce_worktree_caches(&mut self, binding: &BindingRef) {
        for key in self
            .providers
            .binding_caches
            .remove(binding)
            .unwrap_or_default()
        {
            if let Some(cache) = self.providers.caches.get_mut(&key) {
                cache.quiesce();
            }
        }
    }

    /// Selects only an operator-configured language profile; absent profiles stay explicitly lexical.
    pub(super) async fn semantic_context(
        &mut self,
        job: &mut Job,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<Option<ProviderContext>, FailureCode> {
        let required = match source.path().extension().and_then(|value| value.to_str()) {
            Some("go") => AcceptedProviderSettings::GoplsDefaults,
            Some("rs") => AcceptedProviderSettings::RustCachePrimingDisabledV1,
            _ => return Ok(None),
        };
        let Some(profile) = job
            .target
            .providers
            .iter()
            .find(|profile| profile.settings == required)
            .cloned()
        else {
            return Ok(None);
        };
        match required {
            AcceptedProviderSettings::GoplsDefaults => self
                .go_context(job, &profile, source, bytes, query)
                .await
                .map(Some),
            AcceptedProviderSettings::RustCachePrimingDisabledV1 => self
                .rust_context(job, &profile, source, bytes, query)
                .await
                .map(Some),
        }
    }

    /// Starts one exclusive accepted Rust session and settles it only after bounded direct-child reap.
    async fn rust_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        let profile = RustProfile::new(RustProfileIdentity {
            binary: launch.executable.path.clone(),
            rust_analyzer_version: launch.executable.identity.clone(),
            cargo_version: launch
                .cargo_version
                .clone()
                .ok_or(FailureCode::ExecutionProfile)?,
            rustc_version: launch
                .rustc_version
                .clone()
                .ok_or(FailureCode::ExecutionProfile)?,
            rustup_toolchain: launch.toolchain.clone(),
            configuration: "cache-priming-disabled-v1".into(),
            trust: launch.trust.clone(),
            transport: "stdio-v1".into(),
            cache_namespace: launch.cache_namespace.clone(),
        })
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let scoped = execution_authority(&authority)?;
        let worktree = RustWorktree::new(authority.worktree().clone(), scoped)
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = profile
            .command(&worktree)
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let request = self
            .execution_request(job, &authority, command, &launch.executable)
            .await?;
        let active = self.shared.active(&binding)?;
        let view = match self.providers.rust.request(
            &profile,
            &worktree,
            &mut self.providers.registry,
            &mut self.admission,
            owner(&binding)?,
            AdmissionClass::Interactive,
        ) {
            RustViewAdmission::Granted(view) => view,
            RustViewAdmission::Queued(ticket) => {
                self.providers
                    .registry
                    .cancel_pending(&mut self.admission, ticket);
                return Err(FailureCode::Capacity);
            }
            _ => return Err(FailureCode::ProviderUnavailable),
        };
        self.providers
            .rust
            .observe_source(view.lease(), source.sequence())
            .map_err(|_| FailureCode::Internal)?;
        let mut child = match RustProtocolChild::spawn(
            &request,
            &worktree,
            &mut self.providers.registry,
            view.lease(),
            Some(active),
            &job.target.codex.path,
            self.shared.launcher.limits.output_bytes,
        ) {
            Ok(child) => child,
            Err(error) => {
                let _ = self
                    .providers
                    .rust
                    .release(&mut self.providers.registry, view.lease());
                if let RustProfileError::Process(error) = error {
                    self.provider_spawn_failure(error, &binding);
                }
                return Err(FailureCode::ProviderUnavailable);
            }
        };
        let result = {
            let (input, output) = child.pipes();
            let operation = session_operation(
                input,
                output,
                source.clone(),
                bytes.to_vec(),
                query,
                ViewGeneration {
                    backend: view.generation(),
                    configuration: 1,
                    toolchain: 1,
                    view: view.generation(),
                },
                ProviderSettings::Rust(profile),
                remaining_options(job),
            );
            tokio::pin!(operation);
            tokio::select! {result=&mut operation=>result,_=job.cancel.changed()=>Err(FailureCode::Cancelled)}
        };
        let reaped = match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => reaped,
            Err(_) => {
                self.uncertain.insert(binding);
                return Err(FailureCode::Deadline);
            }
        };
        let release = self
            .providers
            .rust
            .release(&mut self.providers.registry, view.lease())
            .map_err(|_| FailureCode::Internal)?;
        let capability = release;
        self.providers
            .registry
            .complete_reap(&mut self.admission, capability, reaped.proof)
            .map_err(|_| FailureCode::Internal)?;
        self.shared.active(&binding)?;
        result
    }

    /// Reuses one compatible listener but gives each request its own independently accounted forwarder.
    async fn go_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let generation = self.providers.next()?;
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        let mut state = job
            .observed
            .as_ref()
            .ok_or(FailureCode::SandboxState)?
            .state()
            .as_json()
            .clone();
        state
            .as_object_mut()
            .ok_or(FailureCode::SandboxState)?
            .remove("sandboxCwd");
        let trust = format!(
            "{}|{}",
            launch.trust,
            blake3::hash(state.to_string().as_bytes()).to_hex()
        );
        let profile = GoplsProfile::new(
            launch.executable.path.clone(),
            launch.executable.identity.clone(),
            "gopls-v1".into(),
            "gopls-defaults-v1".into(),
            launch.toolchain.clone(),
            trust,
            launch.cache_namespace.clone(),
        )
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let backend = profile.compatibility_key();
        let socket_key = blake3::hash(
            format!(
                "{}{}",
                blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
                backend
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        let socket = self.runtime.join(format!("g-{}.sock", &socket_key[..16]));
        let scoped = execution_authority(&authority)?;
        let logical = if let Some(view) = self.providers.go_views.get(&binding).cloned() {
            if view.backend != backend {
                return Err(FailureCode::ExecutionProfile);
            }
            view
        } else {
            let command = if self.providers.go.contains_key(&backend) {
                profile.forwarder_command(&scoped, &socket)
            } else {
                profile.listener_command(&scoped, &socket)
            }
            .map_err(|_| FailureCode::ExecutionProfile)?;
            let request = self
                .execution_request(job, &authority, command, &launch.executable)
                .await?;
            let active = self.shared.active(&binding)?;
            let lease = match self.providers.registry.request(
                &mut self.admission,
                owner(&binding)?,
                AdmissionClass::Interactive,
                backend.clone(),
                ProviderBackendKind::OwnedShared,
                request.authority(),
            ) {
                ProviderLeaseAdmission::Granted(lease) => lease,
                ProviderLeaseAdmission::Queued(ticket) => {
                    self.providers
                        .registry
                        .cancel_pending(&mut self.admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            };
            if !self.providers.go.contains_key(&backend) {
                let capability = self
                    .providers
                    .registry
                    .take_spawn_lease(lease)
                    .map_err(|_| FailureCode::Internal)?;
                let listener = match SharedGopls::start(
                    &profile,
                    &request,
                    capability,
                    Some(active),
                    &job.target.codex.path,
                    self.shared.launcher.limits.output_bytes,
                ) {
                    Ok(listener) => listener,
                    Err(error) => {
                        self.provider_spawn_failure(error, &binding);
                        return Err(FailureCode::ProviderUnavailable);
                    }
                };
                self.providers.go.insert(
                    backend.clone(),
                    GoBackend {
                        shared: listener,
                        socket: socket.clone(),
                        generation,
                    },
                );
            }
            let view = GoLease {
                backend: backend.clone(),
                lease,
            };
            self.providers
                .go_views
                .insert(binding.clone(), view.clone());
            view
        };
        while !socket.exists() {
            let failure = if *job.cancel.borrow() || self.shared.active(&binding).is_err() {
                Some(FailureCode::Cancelled)
            } else if tokio::time::Instant::now() >= job.deadline {
                Some(FailureCode::Deadline)
            } else {
                None
            };
            if let Some(code) = failure {
                let _ = self.close_provider(&binding).await;
                return Err(code);
            }
            tokio::select! {_=tokio::time::sleep(Duration::from_millis(10))=>{},_=job.cancel.changed()=>{let _=self.close_provider(&binding).await;return Err(FailureCode::Cancelled);}}
        }
        let request = self
            .execution_request(
                job,
                &authority,
                profile
                    .forwarder_command(&scoped, &socket)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                &launch.executable,
            )
            .await?;
        let active = self.shared.active(&binding)?;
        let admission = self.admit(&binding)?;
        let capability = match self.providers.registry.take_forwarder_spawn_lease(
            &mut self.admission,
            logical.lease,
            &request,
            admission,
        ) {
            Ok(capability) => capability,
            Err((_, unused)) => {
                self.admission
                    .release(unused)
                    .map_err(|_| FailureCode::Internal)?;
                return Err(FailureCode::Internal);
            }
        };
        let backend_state = self
            .providers
            .go
            .get_mut(&backend)
            .ok_or(FailureCode::Internal)?;
        let view = match backend_state.shared.open_view(
            authority.worktree().clone(),
            source.sequence(),
            &request,
            capability,
            Some(active),
            &job.target.codex.path,
            self.shared.launcher.limits.output_bytes,
        ) {
            Ok(view) => view,
            Err(error) => {
                self.provider_spawn_failure(error, &binding);
                return Err(FailureCode::ProviderUnavailable);
            }
        };
        let backend_generation = backend_state.generation;
        let mut child = view.into_child();
        let result = {
            let operation = session_operation(
                &mut child.stdout,
                &mut child.stdin,
                source.clone(),
                bytes.to_vec(),
                query,
                ViewGeneration {
                    backend: backend_generation,
                    configuration: 1,
                    toolchain: 1,
                    view: generation,
                },
                ProviderSettings::GoplsDefaults,
                remaining_options(job),
            );
            tokio::pin!(operation);
            tokio::select! {result=&mut operation=>result,_=job.cancel.changed()=>Err(FailureCode::Cancelled)}
        };
        let reaped = match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => reaped,
            Err(_) => {
                self.uncertain.insert(binding);
                return Err(FailureCode::Deadline);
            }
        };
        self.providers
            .registry
            .complete_forwarder_reap(&mut self.admission, reaped.proof)
            .map_err(|_| FailureCode::Internal)?;
        self.providers
            .go
            .get_mut(&backend)
            .ok_or(FailureCode::Internal)?
            .shared
            .release_view(authority.worktree(), logical.lease)
            .map_err(|_| FailureCode::Internal)?;
        self.shared.active(&binding)?;
        result
    }

    /// Settles only a capability-backed no-child result; uncertain provider failures remain reserved.
    fn provider_spawn_failure(&mut self, error: ProcessError, binding: &BindingRef) {
        if let ProcessError::NeverStarted { settlement, .. } = error {
            if self
                .providers
                .registry
                .settle_never_started(&mut self.admission, settlement)
                .is_err()
            {
                self.uncertain.insert(binding.clone());
            }
        } else {
            self.uncertain.insert(binding.clone());
        }
    }

    /// Releases the stopped actor's logical view; compatible peers retain their listener and cache identity.
    pub(super) async fn close_provider(&mut self, binding: &BindingRef) -> Result<(), FailureCode> {
        let Some(view) = self.providers.go_views.remove(binding) else {
            return Ok(());
        };
        let release = self
            .providers
            .registry
            .release(view.lease)
            .map_err(|_| FailureCode::Internal)?;
        if let BackendRelease::ReapOwned(capability) = release {
            let backend = self
                .providers
                .go
                .remove(&view.backend)
                .ok_or(FailureCode::Internal)?;
            let completed = match backend
                .shared
                .stop(Duration::from_millis(100), Duration::from_millis(500))
                .await
            {
                Ok(completed) => completed,
                Err(_) => {
                    self.uncertain.insert(binding.clone());
                    return Err(FailureCode::Deadline);
                }
            };
            self.providers
                .registry
                .complete_reap(&mut self.admission, capability, completed.settlement)
                .map_err(|_| FailureCode::Internal)?;
            let _ = std::fs::remove_file(backend.socket);
        }
        Ok(())
    }
}

/// Produces only an Execution scope from an already fresh durable stamp; it grants nothing itself.
fn execution_authority(
    authority: &AuthorityStamp,
) -> Result<crate::execution::WorkspaceAuthority, FailureCode> {
    crate::execution::WorkspaceAuthority::from_workspace(
        authority.worktree().id(),
        authority.worktree().incarnation().to_string(),
        authority.worktree().worktree_path().to_path_buf(),
        authority.epoch(),
    )
    .map_err(|_| FailureCode::WorkspaceAuthority)
}

/// Uses the opaque exact binding identity solely for resource accounting, never as host proof.
fn owner(binding: &BindingRef) -> Result<OwnerId, FailureCode> {
    OwnerId::new(
        blake3::Hash::from_bytes(binding.fingerprint())
            .to_hex()
            .to_string(),
    )
    .map_err(|_| FailureCode::Internal)
}

/// Carries the remaining operation lifetime into the provider's independent bounded session policy.
fn remaining_options(job: &Job) -> SessionOptions {
    let remaining = job
        .deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_millis(1));
    SessionOptions {
        request_timeout: remaining.min(Duration::from_secs(60)),
        lifetime: remaining.min(Duration::from_secs(300)),
    }
}

/// Runs the accepted settings handshake and one exact-source query, snapshots that Session's
/// bounded diagnostics, then performs graceful protocol shutdown. Transport or protocol failures
/// return `ProviderUnavailable`; the caller still owns and must reap the protocol child.
#[allow(clippy::too_many_arguments)]
async fn session_operation<R: tokio::io::AsyncRead + Unpin, W: tokio::io::AsyncWrite + Unpin>(
    input: R,
    output: W,
    source: SourceObservation,
    bytes: Vec<u8>,
    query: ContextQuery,
    generation: ViewGeneration,
    settings: ProviderSettings,
    options: SessionOptions,
) -> Result<ProviderContext, FailureCode> {
    let tree = source.worktree().clone();
    let epoch = source.authority_epoch();
    with_session(
        input,
        output,
        tree,
        epoch,
        generation,
        settings,
        options,
        |mut session| async move {
            let context = session.context(&source, &bytes, query).await?;
            let diagnostics = session.diagnostics();
            let _ = session.shutdown().await;
            Ok(ProviderContext {
                context,
                diagnostics,
            })
        },
    )
    .await
    .map_err(|_| FailureCode::ProviderUnavailable)
}
