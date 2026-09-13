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
        pyright::{
            PyrightProfile, PyrightProfileError, PyrightProfileIdentity, PyrightProtocolChild,
            PyrightViewAdmission, PyrightWorktree,
        },
        rust::{
            RustProfile, RustProfileError, RustProfileIdentity, RustProtocolChild,
            RustViewAdmission, RustViews, RustWorktree,
        },
        session::{DiagnosticSnapshot, GoEnv, ProviderSettings, SessionOptions, with_session},
    },
};
use std::path::Path;

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
    /// Created private socket identity, captured before this backend accepts forwarders.
    socket: Option<OwnedProviderSocket>,
    /// Monotonic backend generation used in semantic provenance.
    generation: u64,
}

/// Retains one created provider socket's filesystem identity for replacement-safe cleanup.
#[derive(Debug)]
struct OwnedProviderSocket {
    /// Private endpoint path generated for this daemon boot and backend profile.
    path: std::path::PathBuf,
    /// Filesystem device containing the created endpoint.
    device: u64,
    /// Filesystem inode of the created endpoint.
    inode: u64,
}

impl OwnedProviderSocket {
    /// Captures the object currently at `path`; missing or unreadable metadata is an identity failure.
    fn capture(path: std::path::PathBuf) -> std::io::Result<Self> {
        let metadata = std::fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: std::os::unix::fs::MetadataExt::dev(&metadata),
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }

    /// Unlinks this exact object, accepts prior removal, and refuses a replacement at the same path.
    fn remove(self) -> std::io::Result<()> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if std::os::unix::fs::MetadataExt::dev(&metadata) != self.device
            || std::os::unix::fs::MetadataExt::ino(&metadata) != self.inode
        {
            return Err(std::io::Error::other(
                "provider socket path no longer identifies the owned object",
            ));
        }
        match std::fs::remove_file(&self.path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
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
    /// Per-backend socket path generation, bumped whenever a reap could not prove the on-disk
    /// socket's fate; a bumped generation forces the next spawn for that backend onto a fresh path.
    socket_generation: BTreeMap<String, u64>,
    /// Exclusive Rust generation and source bookkeeping.
    rust: RustViews,
    /// Strictly increasing protocol/backend generation within this boot.
    generation: u64,
    /// Worktree/provider cache owners retained independently from actor bindings.
    caches: BTreeMap<String, CacheLifecycle>,
    /// Cache keys currently used by each actor and quiesced when that actor stops.
    binding_caches: BTreeMap<BindingRef, Vec<String>>,
    /// Concurrent active bindings retaining each *shared* native cache key. `CacheLifecycle` itself
    /// models one exclusive owner at a time (handoff only succeeds once quiescent), which is right
    /// for a per-worktree namespace but wrong for the one shared namespace several divergent
    /// worktrees legitimately use at once. This count is the source of truth for when the shared
    /// entry may actually transition to quiescent.
    shared_cache_refs: BTreeMap<String, usize>,
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
            socket_generation: BTreeMap::new(),
            rust: RustViews::default(),
            generation: 0,
            caches: BTreeMap::new(),
            binding_caches: BTreeMap::new(),
            shared_cache_refs: BTreeMap::new(),
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

    /// Gives one captured provider socket exactly one disposition: identity-checked removal when the
    /// identity was captured, otherwise a bumped `socket_generation` for `backend` so the next spawn
    /// cannot adopt whatever, if anything, is left at the deterministic path. Never guess-deletes a
    /// replacement. The generation is finite and saturates rather than wrapping back to a reused value.
    /// Returns whether the socket was proven removed (or never existed to remove).
    fn dispose_socket(&mut self, backend: &str, socket: Option<OwnedProviderSocket>) -> bool {
        if let Some(identity) = socket
            && identity.remove().is_ok()
        {
            return true;
        }
        let generation = self
            .socket_generation
            .entry(backend.to_string())
            .or_insert(0);
        *generation = generation.saturating_add(1);
        false
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
    /// wrongly. `shared_go` retains the extra listener namespace only for the managed shared-gopls
    /// path; one-shot Claude helpers pass false and retain only their worktree namespace. No failure
    /// deletes or quiesces a retained namespace.
    pub(super) fn retain_worktree_caches(
        &mut self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launches: &[ProviderLaunch],
        managed_sandbox: bool,
        shared_go: bool,
        rights: &str,
    ) -> Result<(), FailureCode> {
        let root = CacheRoot::prepare(self.runtime.join("cache"))
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let worktree_state = format!(
            "{}:{}",
            authority.worktree().id(),
            authority.worktree().incarnation()
        );
        let mut plan = Vec::with_capacity(launches.len() * 2);
        for launch in launches {
            let settings = provider_cache_settings(launch.settings);
            let configuration = effective_configuration(launch.settings, managed_sandbox);
            let trust = effective_trust(launch, rights);
            plan.push(CacheRequest {
                key: provider_cache_key(&worktree_state, launch, settings, &trust),
                identity: CacheIdentity::new(
                    launch.executable.identity.clone(),
                    settings,
                    configuration,
                    launch.toolchain.clone(),
                    trust.clone(),
                    worktree_state.clone(),
                )
                .ok_or(FailureCode::ProviderUnavailable)?,
                required: match launch.settings {
                    AcceptedProviderSettings::GoplsDefaults => {
                        &["go-build", "go-mod", "gopls", "tmp"][..]
                    }
                    AcceptedProviderSettings::RustCachePrimingDisabledV1 => {
                        &["cargo", "target", "tmp"][..]
                    }
                    AcceptedProviderSettings::PyrightDefaultsV1 => &["tmp"][..],
                },
                shared: false,
            });
            if shared_go && matches!(launch.settings, AcceptedProviderSettings::GoplsDefaults) {
                plan.push(CacheRequest {
                    key: provider_cache_key(SHARED_NATIVE_CACHE_STATE, launch, settings, &trust),
                    identity: CacheIdentity::new(
                        launch.executable.identity.clone(),
                        settings,
                        configuration,
                        launch.toolchain.clone(),
                        trust,
                        SHARED_NATIVE_CACHE_STATE,
                    )
                    .ok_or(FailureCode::ProviderUnavailable)?,
                    required: &["gopls", "tmp"][..],
                    shared: true,
                });
            }
        }
        let keys = retain_cache_plan(
            &mut self.providers.caches,
            &self.providers.shared_cache_refs,
            &root,
            authority.worktree(),
            &plan,
        )?;
        for request in &plan {
            if request.shared {
                *self
                    .providers
                    .shared_cache_refs
                    .entry(request.key.clone())
                    .or_insert(0) += 1;
            }
        }
        self.providers.binding_caches.insert(binding.clone(), keys);
        Ok(())
    }

    /// Resolves each configured provider's already-retained worktree namespace for Claude jobs.
    ///
    /// Paths come from live `CacheLifecycle` entries and the same rights-aware key used during
    /// retention; launcher labels and caller input are never exposed as filesystem locations.
    pub(super) fn helper_cache_namespaces(
        &self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launches: &[ProviderLaunch],
        rights: &str,
    ) -> Result<Vec<(AcceptedProviderSettings, String)>, FailureCode> {
        launches
            .iter()
            .map(|launch| {
                let trust = effective_trust(launch, rights);
                self.provider_cache_namespace(binding, authority, launch, &trust)
                    .map(|path| (launch.settings, path))
            })
            .collect()
    }

    /// Quiesces the stopped actor's cache owners without deleting their worktree namespaces.
    ///
    /// A shared native namespace key is instead reference-counted: it becomes quiescent only once
    /// every divergent worktree currently sharing that one heavy listener has stopped, so a still
    /// live shared entry is never falsely retired or handed off to an unrelated actor.
    pub(super) fn quiesce_worktree_caches(&mut self, binding: &BindingRef) {
        for key in self
            .providers
            .binding_caches
            .remove(binding)
            .unwrap_or_default()
        {
            if let Some(refs) = self.providers.shared_cache_refs.get_mut(&key) {
                *refs = refs.saturating_sub(1);
                if *refs > 0 {
                    continue;
                }
                self.providers.shared_cache_refs.remove(&key);
            }
            if let Some(cache) = self.providers.caches.get_mut(&key) {
                cache.quiesce();
            }
        }
    }

    /// Test-only direct installer for one real `CacheLifecycle`/binding association, bypassing the
    /// launcher/provider plumbing `retain_worktree_caches` otherwise needs, so revoke-retry tests can
    /// exercise real quiescence bookkeeping without spawning a provider.
    #[cfg(test)]
    pub(super) fn install_test_cache(
        &mut self,
        binding: &BindingRef,
        key: &str,
        cache: CacheLifecycle,
    ) {
        self.providers.caches.insert(key.to_owned(), cache);
        self.providers
            .binding_caches
            .insert(binding.clone(), vec![key.to_owned()]);
    }

    /// Test-only read of one cache's quiescence, or `None` if no such key is retained.
    #[cfg(test)]
    pub(super) fn test_cache_quiescent(&self, key: &str) -> Option<bool> {
        self.providers
            .caches
            .get(key)
            .map(CacheLifecycle::quiescent)
    }

    /// Test-only read of whether `binding` still owns any cache keys.
    #[cfg(test)]
    pub(super) fn test_binding_owns_caches(&self, binding: &BindingRef) -> bool {
        self.providers.binding_caches.contains_key(binding)
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
            Some("py") | Some("pyi") => AcceptedProviderSettings::PyrightDefaultsV1,
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
            AcceptedProviderSettings::PyrightDefaultsV1 => self
                .pyright_context(job, &profile, source, bytes, query)
                .await
                .map(Some),
        }
    }

    /// Starts one exclusive accepted Pyright session and settles it only after direct-child reap.
    async fn pyright_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        let cache_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let node = launch.node.as_ref().ok_or(FailureCode::ExecutionProfile)?;
        let accepted_script_digest = blake3::Hash::from_hex(&launch.executable.blake3)
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let accepted_node_digest =
            blake3::Hash::from_hex(&node.blake3).map_err(|_| FailureCode::ExecutionProfile)?;
        let profile = PyrightProfile::new(PyrightProfileIdentity {
            binary: launch.executable.path.clone(),
            accepted_script_digest,
            version: launch.executable.identity.clone(),
            node: node.path.clone(),
            accepted_node_digest,
            node_identity: node.identity.clone(),
            trust: launch.trust.clone(),
            cache_namespace,
        })
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let worktree = PyrightWorktree::new(
            authority.worktree().clone(),
            execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = profile
            .command(&worktree)
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let request = self
            .execution_request(job, &authority, command, node)
            .await?;
        let active = self.shared.active(&binding)?;
        let generation = self.providers.next()?;
        let view = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match PyrightProfile::request_view(
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                owner(&binding)?,
                AdmissionClass::Interactive,
                generation,
            ) {
                PyrightViewAdmission::Granted(view) => view,
                PyrightViewAdmission::Queued(ticket) => {
                    self.providers
                        .registry
                        .cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let child = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            PyrightProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                view.lease(),
                Some(active),
                &job.target.codex.path,
                self.shared.launcher.limits.output_bytes,
            )
        };
        let mut child = match child {
            Ok(child) => child,
            Err(PyrightProfileError::InvalidProfile) => {
                return Err(FailureCode::ProviderUnavailable);
            }
            Err(error) => {
                let _ = view.release(&mut self.providers.registry);
                if let PyrightProfileError::Process(error) = error {
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
                ProviderSettings::Pyright(profile),
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
        let capability = view
            .release(&mut self.providers.registry)
            .map_err(|_| FailureCode::Internal)?;
        let admission = self.admission.clone();
        let mut admission = admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.providers
            .registry
            .complete_reap(&mut admission, capability, reaped.proof)
            .map_err(|_| FailureCode::Internal)?;
        self.shared.active(&binding)?;
        result
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
        let cache_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &launch.trust)?;
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
        // The shared controller guard is confined to this block: it is a `std` mutex the helper
        // socket task also locks, so it must never reach the awaits below or this worker future
        // stops being `Send`.
        let view = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.providers.rust.request(
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                owner(&binding)?,
                AdmissionClass::Interactive,
            ) {
                RustViewAdmission::Granted(view) => view,
                RustViewAdmission::Queued(ticket) => {
                    self.providers
                        .registry
                        .cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
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
        let admission = self.admission.clone();
        let mut admission = admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.providers
            .registry
            .complete_reap(&mut admission, capability, reaped.proof)
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
        let trust = effective_trust(launch, &effective_rights_from_job(job)?);
        let worktree_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &trust)?;
        let shared_namespace = self.provider_shared_cache_namespace(&binding, launch, &trust)?;
        let go_env = GoEnv::prepare(
            Path::new(&worktree_namespace).join("go-build"),
            Path::new(&worktree_namespace).join("go-mod"),
            Path::new(&worktree_namespace).join("tmp"),
        )
        .ok_or(FailureCode::ProviderUnavailable)?;
        let profile = GoplsProfile::new(
            launch.executable.path.clone(),
            launch.executable.identity.clone(),
            "gopls-v1".into(),
            "gopls-defaults-v1".into(),
            launch.toolchain.clone(),
            trust,
            shared_namespace,
        )
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let backend = profile.compatibility_key();
        let socket_generation = self
            .providers
            .socket_generation
            .get(&backend)
            .copied()
            .unwrap_or(0);
        let socket_key = blake3::hash(
            format!(
                "{}{}{}",
                blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
                backend,
                socket_generation
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
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lease = match self.providers.registry.request(
                &mut admission,
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
                        .cancel_pending(&mut admission, ticket);
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
                        socket: None,
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
        loop {
            let already_captured = self
                .providers
                .go
                .get(&backend)
                .ok_or(FailureCode::Internal)?
                .socket
                .is_some();
            if already_captured {
                break;
            }
            match OwnedProviderSocket::capture(socket.clone()) {
                Ok(identity) => {
                    self.providers
                        .go
                        .get_mut(&backend)
                        .ok_or(FailureCode::Internal)?
                        .socket = Some(identity);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = self.close_provider(&binding).await;
                    return Err(FailureCode::Internal);
                }
            }
            // A listener that has already exited cannot make further progress toward binding on
            // its own; settle this request immediately instead of polling it all the way to its
            // full operation deadline. This only shortens how long *this* request waits; it does
            // not change the established, unmodified socket cleanup this triggers via
            // `close_provider`/`reap_owned_backend` below.
            let listener_exited = self
                .providers
                .go
                .get_mut(&backend)
                .ok_or(FailureCode::Internal)?
                .shared
                .listener_exit_status()
                .ok()
                .flatten()
                .is_some();
            let failure = if listener_exited {
                Some(FailureCode::ProviderUnavailable)
            } else if *job.cancel.borrow() || self.shared.active(&binding).is_err() {
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
        let lease = self.admit(&binding)?;
        // The shared controller guard is confined to this block: it is a `std` mutex the helper
        // socket task also locks, so it must never reach the awaits below or this worker future
        // stops being `Send`.
        let capability = {
            let controller = self.admission.clone();
            let mut controller = controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.providers.registry.take_forwarder_spawn_lease(
                &mut controller,
                logical.lease,
                &request,
                lease,
            ) {
                Ok(capability) => capability,
                Err((_, unused)) => {
                    controller
                        .release(unused)
                        .map_err(|_| FailureCode::Internal)?;
                    return Err(FailureCode::Internal);
                }
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
                ProviderSettings::GoplsDefaults(go_env),
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
        let admission = self.admission.clone();
        let mut admission = admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.providers
            .registry
            .complete_forwarder_reap(&mut admission, reaped.proof)
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
                .settle_never_started(
                    &mut self
                        .admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    settlement,
                )
                .is_err()
            {
                self.uncertain.insert(binding.clone());
            }
        } else {
            self.uncertain.insert(binding.clone());
        }
    }

    /// Releases the stopped actor's logical view; compatible peers retain their listener and cache identity.
    ///
    /// Every step below the removed `go_views` mapping is the only remaining reference to that state,
    /// so any failure past this point is recorded in `uncertain` rather than silently discarded: the
    /// binding's view and backend accounting are gone either way, and losing the failure signal would
    /// let a caller believe cleanup fully succeeded when it did not. When this release makes the worker
    /// the backend's sole owner, the entire remaining disposition is delegated to `reap_owned_backend`.
    pub(super) async fn close_provider(&mut self, binding: &BindingRef) -> Result<(), FailureCode> {
        let Some(view) = self.providers.go_views.remove(binding) else {
            return Ok(());
        };
        let release = match self.providers.registry.release(view.lease) {
            Ok(release) => release,
            Err(_) => {
                self.uncertain.insert(binding.clone());
                return Err(FailureCode::Internal);
            }
        };
        if let BackendRelease::ReapOwned(capability) = release {
            let backend = match self.providers.go.remove(&view.backend) {
                Some(backend) => backend,
                None => {
                    self.uncertain.insert(binding.clone());
                    return Err(FailureCode::Internal);
                }
            };
            reap_owned_backend(
                &mut self.providers,
                &self.admission,
                &mut self.uncertain,
                binding,
                &view.backend,
                backend,
                capability,
            )
            .await?;
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
        trust: &str,
    ) -> Result<String, FailureCode> {
        let worktree_state = format!(
            "{}:{}",
            authority.worktree().id(),
            authority.worktree().incarnation()
        );
        self.retained_cache_namespace(
            binding,
            &provider_cache_key(
                &worktree_state,
                launch,
                provider_cache_settings(launch.settings),
                trust,
            ),
        )
    }

    /// Resolves the already-retained *shared* native namespace backing every worktree's gopls
    /// listener for this compatible executable/settings/toolchain/effective-rights identity.
    fn provider_shared_cache_namespace(
        &self,
        binding: &BindingRef,
        launch: &ProviderLaunch,
        trust: &str,
    ) -> Result<String, FailureCode> {
        self.retained_cache_namespace(
            binding,
            &provider_cache_key(
                SHARED_NATIVE_CACHE_STATE,
                launch,
                provider_cache_settings(launch.settings),
                trust,
            ),
        )
    }

    /// Returns the retained namespace path for `key` only if this binding still owns a live lifecycle.
    ///
    /// The path is read from the retained `CacheLifecycle` itself rather than recomputed from the
    /// runtime root, so the directory a provider is told to use is by construction the one whose
    /// retention this worker accounts for; the two cannot drift apart. Missing lifecycle ownership,
    /// an already-retired namespace, and a non-UTF-8 path are all rejected as
    /// `ProviderUnavailable` before a provider command can be constructed.
    fn retained_cache_namespace(
        &self,
        binding: &BindingRef,
        key: &str,
    ) -> Result<String, FailureCode> {
        if !self
            .providers
            .binding_caches
            .get(binding)
            .is_some_and(|keys| keys.iter().any(|existing| existing == key))
        {
            return Err(FailureCode::ProviderUnavailable);
        }
        self.providers
            .caches
            .get(key)
            .and_then(CacheLifecycle::namespace_path)
            .and_then(|path| path.to_str())
            .map(str::to_owned)
            .ok_or(FailureCode::ProviderUnavailable)
    }
}

/// One prepared namespace: its key, the lifecycle retaining it, and the directories this
/// operation is proven to have created for it and may therefore roll back.
type PreparedCache = (String, CacheLifecycle, Vec<std::path::PathBuf>);

/// One launch's fully derived retention request, prepared before any lifecycle map is touched.
pub(super) struct CacheRequest {
    /// Opaque namespace component derived from durable worktree and accepted provider identities.
    pub(super) key: String,
    /// Compatibility identity an existing quiescent lifecycle must match to be handed off.
    pub(super) identity: CacheIdentity,
    /// Private subdirectories the provider command needs inside the retained namespace.
    pub(super) required: &'static [&'static str],
    /// Whether this namespace is the one shared native namespace several concurrently active
    /// worktrees legitimately hold at once rather than a single-owner worktree namespace.
    pub(super) shared: bool,
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
    shared_refs: &BTreeMap<String, usize>,
    root: &CacheRoot,
    worktree: &crate::workspace::authority::WorktreeRef,
    plan: &[CacheRequest],
) -> Result<Vec<String>, FailureCode> {
    let mut prepared: Vec<PreparedCache> = Vec::with_capacity(plan.len());
    let mut keys = Vec::with_capacity(plan.len());
    for request in plan {
        keys.push(request.key.clone());
        if request.shared && shared_refs.get(&request.key).is_some_and(|refs| *refs > 0) {
            // Another active binding already keeps this shared listener's namespace live. An equal
            // key already implies a compatible identity (it is a hash of the same
            // executable/settings/toolchain/effective-rights components the identity is built
            // from), so this reference is retained by counting it, never by a handoff that would
            // require the still-live peer to have quiesced first.
            if !caches
                .get(&request.key)
                .is_some_and(CacheLifecycle::retained)
            {
                return Err(FailureCode::ProviderUnavailable);
            }
            continue;
        }
        let mut partial = None;
        match prepare_cache_request(caches, root, worktree, request, &prepared, &mut partial) {
            Ok(entry) => prepared.push(entry),
            Err(code) => {
                // The failing request's own partially created directories are rolled back on the
                // same terms as the earlier ones, so a request that fails after creating its
                // namespace cannot leak it either.
                prepared.extend(partial);
                roll_back_prepared(caches, prepared);
                return Err(code);
            }
        }
    }
    for (key, cache, _) in prepared {
        // A freshly retained lifecycle is non-quiescent, so it correctly represents the incoming
        // owner both for a first retention and for an accepted handoff of an existing namespace.
        caches.insert(key, cache);
    }
    Ok(keys)
}

/// Validates and creates one request's namespace on disk, reporting the directories it created.
///
/// Every existence check happens before the matching creation, so the returned list contains only
/// directories this call is proven to have created: an already-present namespace or subdirectory
/// is reported as pre-existing and therefore never becomes rollback-eligible.
fn prepare_cache_request(
    caches: &BTreeMap<String, CacheLifecycle>,
    root: &CacheRoot,
    worktree: &crate::workspace::authority::WorktreeRef,
    request: &CacheRequest,
    prepared: &[PreparedCache],
    partial: &mut Option<PreparedCache>,
) -> Result<PreparedCache, FailureCode> {
    if let Some(existing) = caches.get(&request.key) {
        if !existing.quiescent() {
            return Err(FailureCode::Conflict);
        }
        if !existing.handoff_allowed(&request.identity) {
            return Err(FailureCode::ProviderUnavailable);
        }
    } else if !prepared.iter().any(|(key, _, _)| key == &request.key)
        && caches.len() + prepared.len() >= MAX_CACHE_NAMESPACES
    {
        return Err(FailureCode::Capacity);
    }
    let Some(namespace) = CacheNamespaceId::new(request.key.clone()) else {
        return Err(FailureCode::Internal);
    };
    let namespace_existed = root.contains(&namespace);
    let Ok(cache) = CacheLifecycle::retain(root, namespace, request.identity.clone(), worktree)
    else {
        return Err(FailureCode::ProviderUnavailable);
    };
    let Some(path) = cache.namespace_path().map(std::path::Path::to_path_buf) else {
        return Err(FailureCode::Internal);
    };
    let mut created = Vec::new();
    if !namespace_existed {
        created.push(path.clone());
    }
    for directory in request.required {
        let directory = path.join(directory);
        let existed = directory.symlink_metadata().is_ok();
        if CacheRoot::prepare(&directory).is_err() {
            *partial = Some((request.key.clone(), cache, created));
            return Err(FailureCode::ProviderUnavailable);
        }
        if !existed && !namespace_existed {
            created.push(directory);
        }
    }
    Ok((request.key.clone(), cache, created))
}

/// Removes only the directories a failed activation is proven to have created, never retained state.
///
/// Rollback is attempted solely for namespaces whose own directory did not exist before this
/// operation, and each removal is an identity-checked *empty*-directory removal, so any content a
/// peer wrote concurrently stops the removal instead of destroying it. A namespace that cannot be
/// fully removed is not silently leaked: its quiescent lifecycle is recorded in `caches`, where it
/// counts against `MAX_CACHE_NAMESPACES` and is reusable by an identical retry, so repeated failed
/// unique activations refuse growth rather than growing the disk without bound.
fn roll_back_prepared(caches: &mut BTreeMap<String, CacheLifecycle>, prepared: Vec<PreparedCache>) {
    for (key, mut cache, created) in prepared.into_iter().rev() {
        if created.is_empty() {
            continue;
        }
        if created
            .iter()
            .rev()
            .all(|path| crate::app::cache::discard_empty_namespace_directory(path))
        {
            continue;
        }
        cache.quiesce();
        caches.entry(key).or_insert(cache);
    }
}

/// Returns the immutable settings identity used by cache compatibility and namespace derivation.
fn provider_cache_settings(settings: AcceptedProviderSettings) -> &'static str {
    match settings {
        AcceptedProviderSettings::GoplsDefaults => "gopls-defaults-v1",
        AcceptedProviderSettings::RustCachePrimingDisabledV1 => "rust-cache-priming-disabled-v1",
        AcceptedProviderSettings::PyrightDefaultsV1 => "pyright-defaults-v1",
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
        AcceptedProviderSettings::PyrightDefaultsV1 => "pyright-defaults-v1",
    }
}

/// Returns whether the job's observed sandbox permission profile is the managed Claude profile.
pub(super) fn managed_sandbox_from_job(job: &Job) -> bool {
    job.observed
        .as_ref()
        .and_then(|observed| observed.state().as_json()["permissionProfile"]["type"].as_str())
        == Some("managed")
}

/// Fixed compatibility-identity marker used only by the shared native namespace.
///
/// It deliberately excludes any worktree so every divergent worktree with a compatible
/// executable/settings/toolchain/effective-rights identity resolves to the same shared key and,
/// through it, to the same one heavy `gopls` listener.
pub(super) const SHARED_NATIVE_CACHE_STATE: &str = "shared-native-v1";

/// Returns the one canonical effective-rights identity used by the cache key, the `CacheIdentity`,
/// the `GoplsProfile` compatibility key, and the shared refcount.
///
/// `gopls` backends are shared, so their identity binds the launch trust to the effective rights
/// the observed sandbox state actually grants (see `HostSandboxState::effective_rights_identity`);
/// an exclusive Rust view keeps its launch trust unchanged. Deriving all four from this one value
/// is what prevents a partially normalized hash from admitting an actor to a listener whose cache
/// key it does not actually match.
fn effective_trust(launch: &ProviderLaunch, rights: &str) -> String {
    match launch.settings {
        AcceptedProviderSettings::GoplsDefaults => format!("{}|{}", launch.trust, rights),
        AcceptedProviderSettings::RustCachePrimingDisabledV1 => launch.trust.clone(),
        AcceptedProviderSettings::PyrightDefaultsV1 => launch.trust.clone(),
    }
}

/// Returns the job's canonical effective-rights identity, or the finite missing-state failure.
pub(super) fn effective_rights_from_job(job: &Job) -> Result<String, FailureCode> {
    Ok(crate::execution::effective_rights_identity(
        job.observed
            .as_ref()
            .ok_or(FailureCode::SandboxState)?
            .state()
            .as_json(),
    ))
}

/// Derives one opaque namespace component from durable worktree and accepted provider identities.
fn provider_cache_key(
    worktree_state: &str,
    launch: &ProviderLaunch,
    settings: &str,
    trust: &str,
) -> String {
    blake3::hash(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}",
            worktree_state,
            launch.cache_namespace,
            launch.executable.identity,
            settings,
            launch.toolchain,
            trust
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

/// Stops and reaps a backend this worker just became the sole owner of, giving its captured socket
/// (if any) exactly one disposition on every exit: identity-checked removal, or a bumped
/// `socket_generation` so the next spawn for `view_backend` cannot adopt a stale path. This is the
/// sole production route `close_provider` uses for its `BackendRelease::ReapOwned` case; a `stop` or
/// `complete_reap` failure still records `uncertain` and still disposes the socket before returning.
///
/// `providers` supplies `dispose_socket` and the `registry` that completes the reap, and is mutated
/// in place; `admission` is the physical-effect controller `complete_reap` releases the settled
/// reservation into. `uncertain` receives `binding` on every error exit — physical/registry
/// completion could not be proven, so the caller (`close_provider`) must not report success even
/// though the view/backend accounting is already gone either way. `binding` identifies the actor
/// solely for that uncertainty bookkeeping; it plays no role in `backend`'s own identity. `view_backend`
/// is the backend compatibility key already removed from `providers.go`, used only to key
/// `socket_generation`. `backend` is consumed: its `shared` listener is stopped and its `socket` (if
/// captured) is disposed. `capability` is the one-time `BackendReapCapability` obtained from the
/// `registry.release` call that produced `BackendRelease::ReapOwned`; it is consumed by
/// `complete_reap` regardless of whether that call succeeds.
///
/// Returns `Ok(())` only when `stop` and `complete_reap` both succeed and the socket disposition
/// proves a clean removal (or no socket was ever captured is not itself an error here — capture
/// failures are handled by the caller before this backend is ever reaped). Returns
/// `Err(FailureCode::Deadline)` when `stop` itself fails (the listener could not be confirmed
/// terminated within its bounded grace/output deadlines, e.g. a forwarder view left open). Returns
/// `Err(FailureCode::Internal)` when `stop` succeeds but either `complete_reap` rejects the
/// settlement (a capability/proof mismatch) or the socket disposition could not prove removal; both
/// conditions still run to completion (the socket is always disposed, `complete_reap` is always
/// attempted once `stop` succeeds) before the error is returned. `backend.socket` being `None` is
/// never upgraded to a proof of absence here, including when the listener is already confirmed
/// exited: the only identity this function ever disposes is one this worker itself already
/// captured, so a `None` always takes the conservative, unproved `dispose_socket` disposition —
/// deliberately, since a fresh post-exit capture at this point would grant deletion authority over
/// whatever object currently occupies the path, not necessarily the one this worker's own listener
/// created.
async fn reap_owned_backend(
    providers: &mut Providers,
    admission: &std::sync::Mutex<crate::execution::AdmissionController>,
    uncertain: &mut std::collections::BTreeSet<BindingRef>,
    binding: &BindingRef,
    view_backend: &str,
    backend: GoBackend,
    capability: crate::execution::BackendReapCapability,
) -> Result<(), FailureCode> {
    let socket = backend.socket;
    let stop_result = backend
        .shared
        .stop(Duration::from_millis(100), Duration::from_millis(500))
        .await;
    let completed = match stop_result {
        Ok(completed) => completed,
        Err(_) => {
            uncertain.insert(binding.clone());
            providers.dispose_socket(view_backend, socket);
            return Err(FailureCode::Deadline);
        }
    };
    let reaped = providers.registry.complete_reap(
        &mut admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        capability,
        completed.settlement,
    );
    let disposed = providers.dispose_socket(view_backend, socket);
    if reaped.is_err() || !disposed {
        uncertain.insert(binding.clone());
        return Err(FailureCode::Internal);
    }
    Ok(())
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
            if matches!(session.settings(), ProviderSettings::Pyright(_)) {
                session.wait_for_matching_diagnostics().await;
            }
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
mod tests {
    use super::{FailureCode, GoBackend, OwnedProviderSocket, Providers, reap_owned_backend};
    use crate::assistance::host_binding::{
        BindingRef, BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        parse_hook_event,
    };
    use crate::execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLimits, BackendRelease,
        ControlledCommand, ExecutionProfileCatalog, ExecutionProfileTemplate, HostSandboxState,
        LocalExecutionPolicy, OwnerId, ProviderBackendKind, ProviderLeaseAdmission,
        ProviderViewLease, ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
    };
    use crate::intelligence::gopls::{GoplsProfile, SharedGopls};
    use crate::workspace::authority::WorktreeRef;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Builds one accepted no-op `BindingRef` for uncertainty bookkeeping only; no daemon involved.
    fn test_binding(label: &str) -> BindingRef {
        let mut guard = HostBindingGuard::default();
        let channel = parse_channel_session(label.as_bytes()).unwrap();
        let actor = format!("actor-{label}");
        let hook = parse_hook_event(
            json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":"spawn"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            json!({"threadId":actor,"callId":"spawn","x-codex-turn-metadata":{"turn":"provider-contract"}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("valid fixture binding")
        };
        invocation.binding_ref().clone()
    }

    /// Builds one accepted execution request for a real `/usr/bin/true`-backed command, isolated by label.
    fn test_request(
        label: &str,
        authority: &WorkspaceAuthority,
        command: ControlledCommand,
    ) -> ValidatedExecutionRequest {
        let root = std::env::temp_dir();
        let sandbox = HostSandboxState::parse(Some(json!({
            "permissionProfile":{"type":"disabled"},
            "codexLinuxSandboxExe":null,
            "sandboxCwd":root,
            "useLegacyLandlock":false
        })))
        .unwrap();
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence(label, 1, &sandbox).unwrap(),
        ])
        .unwrap();
        ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_verified_binding(label, sandbox).unwrap(),
            authority.clone(),
            command,
            &LocalExecutionPolicy::new(
                BTreeSet::from([PathBuf::from("/usr/bin/true")]),
                4096,
                16,
                true,
            )
            .unwrap(),
            &catalog,
        )
        .unwrap()
    }

    /// Spawns one real owned `/usr/bin/true` gopls listener and returns it with the registry view lease
    /// that made this worker its sole owner, plus a real owned socket identity at `socket`.
    ///
    /// `providers` and `admission` are mutated in place to register the granted view, take its spawn
    /// lease and account the started process; both must outlive the returned values, since releasing
    /// the view or completing its reap later requires the same `providers.registry`/`admission` pair.
    /// `label` seeds the owner id, execution-request binding, and (via `test-{label}` trust) the
    /// `GoplsProfile` compatibility key, so two calls with different labels never collide on the same
    /// backend. `socket` is the path a real file is written to and then captured as this backend's
    /// `OwnedProviderSocket` identity; the real `/usr/bin/true` listener process itself never creates
    /// a socket file there.
    ///
    /// Returns, in order: the `GoBackend` (owning the started `SharedGopls` listener and the captured
    /// socket identity, not yet registered in `providers.go`); the `WorkspaceAuthority` and matching
    /// `WorktreeRef` used to admit the view (needed to build a forwarder request or `open_view` call
    /// against the same backend); the `ProviderViewLease` granted for this backend, still live in the
    /// registry (the caller must `release` it to obtain a `BackendReapCapability`, or use it to admit
    /// a forwarder); and the `GoplsProfile` used to start it, needed to build a matching
    /// `forwarder_command` against the same compatibility key and socket path.
    fn spawn_owned_backend(
        providers: &mut Providers,
        admission: &mut AdmissionController,
        label: &str,
        socket: &Path,
    ) -> (
        GoBackend,
        WorkspaceAuthority,
        WorktreeRef,
        ProviderViewLease,
        GoplsProfile,
    ) {
        let root = std::env::temp_dir();
        let tree =
            WorktreeRef::from_discovery(root.clone(), root.clone(), PathBuf::from(".git"), 1)
                .unwrap();
        let authority = WorkspaceAuthority::from_workspace(
            tree.id().to_string(),
            tree.incarnation().to_string(),
            tree.worktree_path().to_path_buf(),
            1,
        )
        .unwrap();
        let profile = GoplsProfile::new(
            "/usr/bin/true".into(),
            "fixture".into(),
            "v1".into(),
            "default".into(),
            "/usr/bin/true".into(),
            format!("test-{label}"),
            root.join(format!("agent-ide-fixture-cache-{label}"))
                .to_string_lossy()
                .into_owned(),
        )
        .unwrap();
        let backend_key = profile.compatibility_key();
        let view = match providers.registry.request(
            admission,
            OwnerId::new(label).unwrap(),
            AdmissionClass::Interactive,
            backend_key,
            ProviderBackendKind::OwnedShared,
            &authority,
        ) {
            ProviderLeaseAdmission::Granted(view) => view,
            _ => panic!("gopls view for {label}"),
        };
        let listener_request = test_request(
            label,
            &authority,
            profile.listener_command(&authority, socket).unwrap(),
        );
        let shared = SharedGopls::start(
            &profile,
            &listener_request,
            providers.registry.take_spawn_lease(view).unwrap(),
            None,
            Path::new("/unused"),
            64,
        )
        .unwrap();
        std::fs::write(socket, b"owned").unwrap();
        let identity = OwnedProviderSocket::capture(socket.to_path_buf()).unwrap();
        (
            GoBackend {
                shared,
                socket: Some(identity),
                generation: 1,
            },
            authority,
            tree,
            view,
            profile,
        )
    }

    /// Forces `SharedGopls::stop`'s deterministic, in-memory `!views.is_empty()` failure by leaving one
    /// forwarder view open (never closed) on the backend passed into `reap_owned_backend` — the same
    /// route `close_provider` uses for its `BackendRelease::ReapOwned` case. Removing the disposal call
    /// from `reap_owned_backend`'s stop-error branch would leave `socket` on disk and fail this test.
    #[tokio::test]
    async fn reap_owned_backend_stop_error_still_disposes_socket_and_stays_conservative() {
        let mut providers = Providers::new();
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 4,
            per_owner_running: 4,
            per_owner_queued: 4,
            total_queued: 4,
            interactive_burst: 1,
        })
        .unwrap();
        let mut uncertain = BTreeSet::new();
        let binding = test_binding("stop-error");
        let socket = std::env::temp_dir().join(format!(
            "agent-ide-reap-owned-stop-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let (mut backend, authority, tree, view, profile) = spawn_owned_backend(
            &mut providers,
            &mut admission,
            "stop-error-backend",
            &socket,
        );
        let forwarder_request = test_request(
            "stop-error-forwarder",
            &authority,
            profile.forwarder_command(&authority, &socket).unwrap(),
        );
        let slot = match admission.submit(
            OwnerId::new("forwarder").unwrap(),
            AdmissionClass::Interactive,
        ) {
            Admission::Granted(lease) => lease,
            _ => panic!("forwarder slot"),
        };
        let forwarder_capability = providers
            .registry
            .take_forwarder_spawn_lease(&mut admission, view, &forwarder_request, slot)
            .unwrap();
        // Left open: never released, so `backend.shared.views` stays non-empty for `stop()`.
        let open_view = backend
            .shared
            .open_view(
                tree.clone(),
                1,
                &forwarder_request,
                forwarder_capability,
                None,
                Path::new("/unused"),
                64,
            )
            .unwrap();

        let BackendRelease::ReapOwned(capability) = providers.registry.release(view).unwrap()
        else {
            panic!("expected sole ownership")
        };

        let admission = std::sync::Mutex::new(admission);
        let result = reap_owned_backend(
            &mut providers,
            &admission,
            &mut uncertain,
            &binding,
            "stop-error-backend",
            backend,
            capability,
        )
        .await;

        assert!(
            matches!(result, Err(FailureCode::Deadline)),
            "a stop error with an open view must surface as Deadline: {result:?}"
        );
        assert!(
            uncertain.contains(&binding),
            "an unresolved stop must be recorded as uncertain, not silently dropped"
        );
        assert!(
            !socket.exists(),
            "the captured socket must still be removed on a stop failure"
        );
        assert_eq!(
            providers.socket_generation.get("stop-error-backend"),
            None,
            "a proven removal must not force the next spawn onto a new generation"
        );

        // Give the auxiliary forwarder child positive settlement: reap its real process and complete
        // its registry reservation, instead of letting `open_view` drop it with no reap evidence.
        let reaped_forwarder = open_view.child.reap(Duration::from_secs(1)).await.unwrap();
        providers
            .registry
            .complete_forwarder_reap(
                &mut admission
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                reaped_forwarder.proof,
            )
            .unwrap();
    }

    /// Forces `complete_reap` to fail by pairing a real, successfully stopped listener's settlement proof
    /// with a mismatched capability captured from a second, independent backend. Removing the disposal
    /// call from `reap_owned_backend`'s post-`complete_reap` branch would leave `socket` on disk and fail
    /// this test even though the reap itself is reported as failed.
    #[tokio::test]
    async fn reap_owned_backend_complete_reap_error_still_disposes_socket_and_stays_conservative() {
        let mut providers = Providers::new();
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 4,
            per_owner_running: 4,
            per_owner_queued: 4,
            total_queued: 4,
            interactive_burst: 1,
        })
        .unwrap();
        let mut uncertain = BTreeSet::new();
        let binding = test_binding("reap-error");
        let socket_a = std::env::temp_dir().join(format!(
            "agent-ide-reap-owned-a-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let socket_b = std::env::temp_dir().join(format!(
            "agent-ide-reap-owned-b-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let (backend_a, _authority_a, _tree_a, view_a, _profile_a) = spawn_owned_backend(
            &mut providers,
            &mut admission,
            "reap-error-backend-a",
            &socket_a,
        );
        let (backend_b, _authority_b, _tree_b, view_b, _profile_b) = spawn_owned_backend(
            &mut providers,
            &mut admission,
            "reap-error-backend-b",
            &socket_b,
        );

        let BackendRelease::ReapOwned(_capability_a) = providers.registry.release(view_a).unwrap()
        else {
            panic!("expected sole ownership for a")
        };
        let BackendRelease::ReapOwned(capability_b) = providers.registry.release(view_b).unwrap()
        else {
            panic!("expected sole ownership for b")
        };

        // `backend_a` stops cleanly (no open views), but is paired with backend b's mismatched
        // capability, so `complete_reap`'s process/backend identity check must fail.
        let admission = std::sync::Mutex::new(admission);
        let result = reap_owned_backend(
            &mut providers,
            &admission,
            &mut uncertain,
            &binding,
            "reap-error-backend-a",
            backend_a,
            capability_b,
        )
        .await;

        assert!(
            matches!(result, Err(FailureCode::Internal)),
            "a mismatched capability must surface as an honest complete_reap failure: {result:?}"
        );
        assert!(
            uncertain.contains(&binding),
            "a failed complete_reap must be recorded as uncertain, not silently dropped"
        );
        assert!(
            !socket_a.exists(),
            "the captured socket must still be disposed even though complete_reap failed"
        );
        assert_eq!(
            providers.socket_generation.get("reap-error-backend-a"),
            None,
            "a proven removal must not force the next spawn onto a new generation"
        );

        // `capability_b` was deliberately spent above against backend a's settlement proof, so
        // backend b's own registry admission can never be completed; a registry-only capability for
        // an unstarted backend cannot substitute here because `complete_reap` validates the exact
        // spawned process identity, not just a backend key. Still reap its real process directly
        // through `SharedGopls::stop` (positive evidence a live child is not left behind), rather than
        // relying on `Drop` to do it.
        backend_b
            .shared
            .stop(Duration::from_millis(100), Duration::from_millis(500))
            .await
            .unwrap();
        let _ = std::fs::remove_file(&socket_b);
    }

    /// Regression proving `reap_owned_backend` never grants itself deletion authority over whatever
    /// object currently occupies the deterministic path just because `backend.socket` is `None` and
    /// the direct child is confirmed exited. `None` only ever means this worker never retained a
    /// deletion identity there — not that the path is empty, and not that anything found there now
    /// was created by this worker's own listener. Fresh metadata readback proves only the current
    /// inode, never ownership by the dead provider. This leaves a real object at the socket path that
    /// this worker's own capture never ran against, waits for the real spawned listener to actually
    /// exit, and proves the object survives byte-for-byte, the endpoint generation is still fenced
    /// forward exactly as any other unproved `None` disposal, and the outcome is the existing honest
    /// `Internal`/uncertain result — never a new success proof for a clean no-socket exit.
    #[tokio::test]
    async fn reap_owned_backend_never_deletes_an_uncaptured_preexisting_object() {
        let mut providers = Providers::new();
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 4,
            per_owner_running: 4,
            per_owner_queued: 4,
            total_queued: 4,
            interactive_burst: 1,
        })
        .unwrap();
        let mut uncertain = BTreeSet::new();
        let binding = test_binding("preexisting-object");
        let socket = std::env::temp_dir().join(format!(
            "agent-ide-reap-owned-preexisting-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let (mut backend, _authority, _tree, view, _profile) = spawn_owned_backend(
            &mut providers,
            &mut admission,
            "preexisting-object-backend",
            &socket,
        );
        // `spawn_owned_backend` captures its own identity for its own fixture bookkeeping; discard
        // it so `backend.socket` matches production reality after a genuinely missed poll: `None`,
        // with no retained deletion identity for anything at this path.
        backend.socket = None;
        // Wait for the real spawned listener to actually exit: a confirmed-dead direct child is
        // exactly the condition the rejected shortcut used to justify a late, unauthorized capture.
        tokio::time::timeout(Duration::from_secs(5), async {
            while backend.shared.listener_exit_status().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        // Only now does a real, pre-existing object occupy this exact path — never captured by this
        // backend, and not created by the fixture's already-exited listener either.
        let preexisting_bytes = b"pre-existing-object-not-owned-by-this-listener";
        std::fs::write(&socket, preexisting_bytes).unwrap();

        let BackendRelease::ReapOwned(capability) = providers.registry.release(view).unwrap()
        else {
            panic!("expected sole ownership")
        };

        let admission = std::sync::Mutex::new(admission);
        let result = reap_owned_backend(
            &mut providers,
            &admission,
            &mut uncertain,
            &binding,
            "preexisting-object-backend",
            backend,
            capability,
        )
        .await;

        assert!(
            matches!(result, Err(FailureCode::Internal)),
            "an uncaptured object must stay unproved/uncertain, never a new clean success: {result:?}"
        );
        assert!(
            uncertain.contains(&binding),
            "an unproved disposition must still be recorded as uncertain, not silently dropped"
        );
        assert!(
            socket.exists(),
            "an object this worker never captured must never be deleted"
        );
        assert_eq!(
            std::fs::read(&socket).unwrap(),
            preexisting_bytes,
            "the pre-existing object's bytes must be completely untouched"
        );
        assert_eq!(
            providers
                .socket_generation
                .get("preexisting-object-backend")
                .copied(),
            Some(1),
            "an unproved disposition must still fence the next spawn onto a fresh generation"
        );

        std::fs::remove_file(&socket).unwrap();
    }

    /// Forces the exact post-backend-removal failure this fix targets: a captured `Some` socket whose
    /// identity-checked removal fails (as it would after a `stop`/`complete_reap` error left the file's
    /// identity unprovable). Proves the socket still gets disposed exactly once, `dispose_socket`
    /// reports it was not proven removed, and the backend's socket generation advances so a later
    /// spawn for the same backend key would not reuse the stale path.
    #[test]
    fn dispose_socket_bumps_generation_on_unproved_removal_and_leaves_no_stale_identity() {
        let root = std::env::temp_dir().join(format!(
            "agent-ide-dispose-socket-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let replaced = root.join("replaced.sock");
        let original = root.join("original.sock");
        std::fs::write(&replaced, b"owned").unwrap();
        let identity = OwnedProviderSocket::capture(replaced.clone()).unwrap();
        // Simulate a foreign replacement racing in during the unproved window, exactly like the
        // `OwnedProviderSocket::remove` identity check already refuses.
        std::fs::rename(&replaced, &original).unwrap();
        std::fs::write(&replaced, b"foreign").unwrap();

        let mut providers = Providers::new();
        assert_eq!(providers.socket_generation.get("go-backend-a"), None);

        let disposed = providers.dispose_socket("go-backend-a", Some(identity));
        assert!(!disposed, "an unproved removal must not be reported clean");
        assert_eq!(
            providers.socket_generation.get("go-backend-a").copied(),
            Some(1),
            "the backend's next spawn must be forced onto a fresh generation"
        );
        assert_eq!(
            std::fs::read(&replaced).unwrap(),
            b"foreign",
            "a replacement must never be guess-deleted"
        );

        // A concurrent None-socket disposal (identity never captured) for a different backend must
        // advance only that backend's own generation, keeping map growth bounded per backend.
        let disposed_none = providers.dispose_socket("go-backend-b", None);
        assert!(!disposed_none);
        assert_eq!(
            providers.socket_generation.get("go-backend-a").copied(),
            Some(1)
        );
        assert_eq!(
            providers.socket_generation.get("go-backend-b").copied(),
            Some(1)
        );

        // A second unproved disposal for the same backend advances the generation again, so a stale
        // path from generation 0 can never be adopted by a spawn using the current generation.
        let stale_identity = OwnedProviderSocket::capture(original.clone()).unwrap();
        let disposed_again = providers.dispose_socket("go-backend-a", Some(stale_identity));
        assert!(
            disposed_again,
            "the untouched original file is genuinely owned and removable"
        );
        assert_eq!(
            providers.socket_generation.get("go-backend-a").copied(),
            Some(1)
        );

        std::fs::remove_file(&replaced).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }

    /// Verifies absent sockets succeed, replacements survive, and real unlink failures propagate.
    #[test]
    fn owned_provider_socket_cleanup_enforces_identity_and_errors() {
        let root = std::env::temp_dir().join(format!(
            "agent-ide-provider-socket-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();

        let missing = root.join("missing.sock");
        std::fs::write(&missing, b"owned").unwrap();
        let missing_identity = OwnedProviderSocket::capture(missing.clone()).unwrap();
        std::fs::remove_file(&missing).unwrap();
        missing_identity.remove().unwrap();

        let replaced = root.join("replaced.sock");
        let original = root.join("original.sock");
        std::fs::write(&replaced, b"owned").unwrap();
        let replaced_identity = OwnedProviderSocket::capture(replaced.clone()).unwrap();
        std::fs::rename(&replaced, &original).unwrap();
        std::fs::write(&replaced, b"foreign").unwrap();
        assert!(replaced_identity.remove().is_err());
        assert_eq!(std::fs::read(&replaced).unwrap(), b"foreign");

        let directory = root.join("directory.sock");
        std::fs::create_dir(&directory).unwrap();
        let directory_identity = OwnedProviderSocket::capture(directory.clone()).unwrap();
        assert!(directory_identity.remove().is_err());
        assert!(directory.is_dir());

        std::fs::remove_file(replaced).unwrap();
        std::fs::remove_file(original).unwrap();
        std::fs::remove_dir(directory).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod providers_tests;
