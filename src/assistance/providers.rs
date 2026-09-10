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

/// Bounds the in-memory cache-lifecycle map so an unbounded stream of distinct worktree
/// incarnations cannot grow it or the retained on-disk namespaces without limit. Matches the fixed
/// `total_views`/`per_backend_views` provider-lease ceiling; a full map fails new namespaces closed
/// rather than evicting an unretired one without closure proof.
const MAX_CACHE_NAMESPACES: usize = 64;

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
    /// A quiescent compatible lifecycle is handed to the incoming binding. Preparation is
    /// transactional (see `retain_cache_plan`), and `binding_caches` records the incoming binding's
    /// keys only once every launch succeeded. Returns the finite reason instead of a bare failure:
    /// `Conflict` when another actor still actively owns this worktree's namespace, `Capacity` when
    /// in-memory lifecycle ownership is full, and `ProviderUnavailable` for a local cache-directory
    /// or identity failure. `Internal` reports a namespace component this module itself derived
    /// wrongly. No failure deletes or quiesces a retained namespace.
    pub(super) fn retain_worktree_caches(
        &mut self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launches: &[ProviderLaunch],
        managed_sandbox: bool,
    ) -> Result<(), FailureCode> {
        let root = CacheRoot::prepare(self.runtime.join("cache"))
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let worktree_state = format!(
            "{}:{}",
            authority.worktree().id(),
            authority.worktree().incarnation()
        );
        let mut plan = Vec::with_capacity(launches.len());
        for launch in launches {
            let settings = provider_cache_settings(launch.settings);
            let identity = CacheIdentity::new(
                launch.executable.identity.clone(),
                settings,
                effective_configuration(launch.settings, managed_sandbox),
                launch.toolchain.clone(),
                launch.trust.clone(),
                worktree_state.clone(),
            )
            .ok_or(FailureCode::ProviderUnavailable)?;
            plan.push(CacheRequest {
                key: provider_cache_key(&worktree_state, launch, settings),
                identity,
                required: match launch.settings {
                    AcceptedProviderSettings::GoplsDefaults => {
                        &["gopls", "go-build", "go-mod", "tmp"][..]
                    }
                    AcceptedProviderSettings::RustCachePrimingDisabledV1 => {
                        &["cargo", "target", "tmp"][..]
                    }
                },
            });
        }
        let keys = retain_cache_plan(
            &mut self.providers.caches,
            &root,
            authority.worktree(),
            &plan,
        )?;
        self.providers.binding_caches.insert(binding.clone(), keys);
        Ok(())
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
        let cache_namespace = self.provider_cache_namespace(&binding, &authority, launch)?;
        let managed_sandbox = managed_sandbox_from_job(job);
        let profile = RustProfile::new(RustProfileIdentity {
            binary: launch.executable.path.clone(),
            rust_analyzer_version: launch.executable.identity.clone(),
            cargo: launch
                .cargo
                .as_ref()
                .ok_or(FailureCode::ExecutionProfile)?
                .path
                .clone(),
            cargo_version: launch
                .cargo_version
                .clone()
                .ok_or(FailureCode::ExecutionProfile)?,
            rustc: launch
                .rustc
                .as_ref()
                .ok_or(FailureCode::ExecutionProfile)?
                .path
                .clone(),
            rustc_version: launch
                .rustc_version
                .clone()
                .ok_or(FailureCode::ExecutionProfile)?,
            rustup_toolchain: launch.toolchain.clone(),
            configuration: effective_configuration(launch.settings, managed_sandbox).into(),
            trust: launch.trust.clone(),
            transport: "stdio-v1".into(),
            cache_namespace,
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
        let cache_namespace = self.provider_cache_namespace(&binding, &authority, launch)?;
        let profile = GoplsProfile::new(
            launch.executable.path.clone(),
            launch.executable.identity.clone(),
            "gopls-v1".into(),
            "gopls-defaults-v1".into(),
            launch.toolchain.clone(),
            trust,
            cache_namespace,
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

    /// Resolves the already-retained namespace for this exact durable worktree and provider.
    ///
    /// The path is read from the retained `CacheLifecycle` itself rather than recomputed from the
    /// runtime root, so the directory a provider is told to use is by construction the one whose
    /// retention this worker accounts for; the two cannot drift apart. Missing lifecycle ownership,
    /// an already-retired namespace, and a non-UTF-8 path are all rejected as
    /// `ProviderUnavailable` before a provider command can be constructed.
    fn provider_cache_namespace(
        &self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launch: &ProviderLaunch,
    ) -> Result<String, FailureCode> {
        let worktree_state = format!(
            "{}:{}",
            authority.worktree().id(),
            authority.worktree().incarnation()
        );
        let key = provider_cache_key(
            &worktree_state,
            launch,
            provider_cache_settings(launch.settings),
        );
        if !self
            .providers
            .binding_caches
            .get(binding)
            .is_some_and(|keys| keys.contains(&key))
        {
            return Err(FailureCode::ProviderUnavailable);
        }
        self.providers
            .caches
            .get(&key)
            .and_then(CacheLifecycle::namespace_path)
            .and_then(|path| path.to_str())
            .map(str::to_owned)
            .ok_or(FailureCode::ProviderUnavailable)
    }
}

/// One launch's fully derived retention request, prepared before any lifecycle map is touched.
pub(super) struct CacheRequest {
    /// Opaque namespace component derived from durable worktree and accepted provider identities.
    pub(super) key: String,
    /// Compatibility identity an existing quiescent lifecycle must match to be handed off.
    pub(super) identity: CacheIdentity,
    /// Private subdirectories the provider command needs inside the retained namespace.
    pub(super) required: &'static [&'static str],
}

/// Retains every requested namespace as one transaction over `caches`, returning the retained keys.
///
/// Each request is validated and its namespace created or reopened on disk first; `caches` is
/// mutated only after the last request succeeded. A late failure therefore leaves every earlier
/// lifecycle exactly as it was — still owned by whichever binding already held it, still quiescent
/// if it was, and still reusable by an identical retry — instead of stranding an unowned,
/// permanently non-quiescent entry that would poison every later activation of this worktree.
///
/// Failures are finite: `Conflict` when a live actor still owns a requested namespace, `Capacity`
/// when the bounded in-memory lifecycle map is full, `ProviderUnavailable` for an incompatible
/// identity or a local cache-directory failure, and `Internal` for a namespace component this
/// module derived wrongly. Nothing is deleted or evicted to make space: bounded ownership fails
/// closed because retiring IDE-owned state requires a verified Workspace closure this call
/// does not have. Directories created for earlier requests are deliberately retained on failure.
fn retain_cache_plan(
    caches: &mut BTreeMap<String, CacheLifecycle>,
    root: &CacheRoot,
    worktree: &crate::workspace::authority::WorktreeRef,
    plan: &[CacheRequest],
) -> Result<Vec<String>, FailureCode> {
    let mut prepared: Vec<(String, CacheLifecycle)> = Vec::with_capacity(plan.len());
    for request in plan {
        if let Some(existing) = caches.get(&request.key) {
            if !existing.quiescent() {
                return Err(FailureCode::Conflict);
            }
        } else if !prepared.iter().any(|(key, _)| key == &request.key)
            && caches.len() + prepared.len() >= MAX_CACHE_NAMESPACES
        {
            return Err(FailureCode::Capacity);
        }
        if let Some(existing) = caches.get_mut(&request.key)
            && !existing.handoff(&request.identity)
        {
            return Err(FailureCode::ProviderUnavailable);
        }
        let namespace = CacheNamespaceId::new(request.key.clone()).ok_or(FailureCode::Internal)?;
        let cache = CacheLifecycle::retain(root, namespace, request.identity.clone(), worktree)
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let path = cache.namespace_path().ok_or(FailureCode::Internal)?;
        if request
            .required
            .iter()
            .any(|directory| CacheRoot::prepare(path.join(directory)).is_err())
        {
            return Err(FailureCode::ProviderUnavailable);
        }
        prepared.push((request.key.clone(), cache));
    }
    let mut keys = Vec::with_capacity(prepared.len());
    for (key, cache) in prepared {
        caches.insert(key.clone(), cache);
        keys.push(key);
    }
    Ok(keys)
}

/// Returns the immutable settings identity used by cache compatibility and namespace derivation.
fn provider_cache_settings(settings: AcceptedProviderSettings) -> &'static str {
    match settings {
        AcceptedProviderSettings::GoplsDefaults => "gopls-defaults-v1",
        AcceptedProviderSettings::RustCachePrimingDisabledV1 => "rust-cache-priming-disabled-v1",
    }
}

/// Returns the exact initialization configuration identity a provider command will use, so the
/// retained `CacheIdentity` never claims compatibility across a managed/non-managed sandbox change
/// it never actually observed. `gopls` has no managed variant and keeps its one fixed identity.
fn effective_configuration(
    settings: AcceptedProviderSettings,
    managed_sandbox: bool,
) -> &'static str {
    match settings {
        AcceptedProviderSettings::GoplsDefaults => "gopls-defaults-v1",
        AcceptedProviderSettings::RustCachePrimingDisabledV1 if managed_sandbox => {
            "cache-priming-and-proc-macro-disabled-v1"
        }
        AcceptedProviderSettings::RustCachePrimingDisabledV1 => "cache-priming-disabled-v1",
    }
}

/// Returns whether the job's observed sandbox permission profile is the managed Claude profile.
pub(super) fn managed_sandbox_from_job(job: &Job) -> bool {
    job.observed
        .as_ref()
        .and_then(|observed| observed.state().as_json()["permissionProfile"]["type"].as_str())
        == Some("managed")
}

/// Derives one opaque namespace component from durable worktree and accepted provider identities.
fn provider_cache_key(worktree_state: &str, launch: &ProviderLaunch, settings: &str) -> String {
    blake3::hash(
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
    .to_string()
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

#[cfg(test)]
#[path = "providers_tests.rs"]
mod providers_tests;
