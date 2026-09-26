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
            RustProfile, RustProfileError, RustProfileIdentity, RustProtocolChild, RustView,
            RustViewAdmission, RustViews, RustWorktree,
        },
        session::{
            DiagnosticSnapshot, GoEnv, LiveSession, ProviderSettings, ReadinessError,
            SessionOptions, with_session,
        },
        typescript::{
            ProjectResolutionInputsV1, TypeScriptProfile, TypeScriptProfileError,
            TypeScriptProfiles, TypeScriptProtocolChild, TypeScriptProviderBundleV1,
            TypeScriptViewAdmission, TypeScriptWorktree,
        },
    },
    telemetry::{CacheState, DiagnosticState, Language, Telemetry, adapters},
};
use std::path::Path;

/// Failure detail when TypeScript project inputs no longer match the snapshot a session was
/// started from; the next request observes them afresh.
const TYPESCRIPT_INPUTS_CHANGED: &str =
    "TypeScript project inputs (tsconfig/package files) changed since the session started";

/// Observes bounded TypeScript config and package files away from the single worker thread.
///
/// `job` receives the refusal text in `failure_detail` when observation rejects the document, so
/// the `resolution_unverified` reply can name the tsconfig consulted and the reason. `document`
/// is the absolute source path; `bundle` and `roots` are moved into the blocking task. A join
/// failure maps to `Internal`, a rejection to `ResolutionUnverified`.
async fn observe_typescript_inputs(
    job: &mut Job,
    worktree: crate::workspace::authority::WorktreeRef,
    document: std::path::PathBuf,
    bundle: TypeScriptProviderBundleV1,
    roots: Vec<std::path::PathBuf>,
) -> Result<ProjectResolutionInputsV1, FailureCode> {
    let observed = tokio::task::spawn_blocking(move || {
        let worktree_root = worktree.worktree_path().to_path_buf();
        let path_proof = |path: &Path| {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                worktree_root.join(path)
            };
            crate::assistance::launcher::admit_path(&roots, &absolute).is_ok()
        };
        ProjectResolutionInputsV1::observe(worktree, document, &bundle, &path_proof)
    })
    .await
    .map_err(|_| FailureCode::Internal)?;
    observed.map_err(|rejection| {
        job.failure_detail = Some(rejection.to_string());
        FailureCode::ResolutionUnverified
    })
}

/// Couples one semantic context result to diagnostics observed by that exact provider session.
pub(super) struct ProviderContext {
    /// Source and semantic locations returned for the synchronized document generation.
    pub(super) context: ContextResult,
    /// Latest bounded diagnostic push retained by the same session before shutdown.
    pub(super) diagnostics: DiagnosticSnapshot,
}

/// Owns the protocol child for one binding's long-lived language-server session.
enum LiveChild {
    /// Rust analyzer process and its exclusive view.
    Rust(RustProtocolChild, RustView),
    /// Pyright process and its exclusive view.
    Pyright(
        PyrightProtocolChild,
        crate::intelligence::pyright::PyrightView,
    ),
    /// TypeScript bridge, its exclusive view, and the resolution inputs that selected its project.
    TypeScript(
        TypeScriptProtocolChild,
        crate::intelligence::typescript::TypeScriptView,
        Box<ProjectResolutionInputsV1>,
    ),
}

/// One live session and the process/view resources retained until binding release.
struct LiveEntry {
    /// Child process and provider admission view owned by this session.
    child: LiveChild,
    /// Transport driver and synchronized document state shared by context and symbol requests.
    live: LiveSession,
}

/// Keeps each language server independent within one actor/worktree binding.
///
/// `T` is the owned session resource for one language slot; removing a slot does not affect its
/// siblings.
struct LiveSessions<T> {
    /// Rust analyzer session, retained across requests in other languages.
    rust: Option<T>,
    /// Pyright session, retained across requests in other languages.
    pyright: Option<T>,
    /// TypeScript session, retained across requests in other languages.
    typescript: Option<T>,
}

impl<T> Default for LiveSessions<T> {
    /// Creates three empty language slots without requiring the session entry type to be default.
    fn default() -> Self {
        Self {
            rust: None,
            pyright: None,
            typescript: None,
        }
    }
}

impl<T> LiveSessions<T> {
    /// Returns the slot owned by a provider language; Go has no per-binding session and gets `None`.
    fn slot_mut(&mut self, language: Language) -> Option<&mut Option<T>> {
        match language {
            Language::Rust => Some(&mut self.rust),
            Language::Python => Some(&mut self.pyright),
            Language::Typescript => Some(&mut self.typescript),
            Language::Go => None,
        }
    }

    /// Returns the session slot selected by a provider language.
    fn get(&self, language: Language) -> Option<&T> {
        match language {
            Language::Rust => self.rust.as_ref(),
            Language::Python => self.pyright.as_ref(),
            Language::Typescript => self.typescript.as_ref(),
            Language::Go => None,
        }
    }

    /// Returns the mutable session slot selected by a provider language.
    fn get_mut(&mut self, language: Language) -> Option<&mut T> {
        self.slot_mut(language).and_then(Option::as_mut)
    }

    /// Replaces only the session slot selected by a provider language; a Go entry is dropped.
    fn insert(&mut self, language: Language, entry: T) {
        if let Some(slot) = self.slot_mut(language) {
            *slot = Some(entry);
        }
    }

    /// Removes only the session slot selected by a provider language.
    fn take(&mut self, language: Language) -> Option<T> {
        self.slot_mut(language).and_then(Option::take)
    }

    /// Reports whether all language sessions have been removed.
    fn is_empty(&self) -> bool {
        self.rust.is_none() && self.pyright.is_none() && self.typescript.is_none()
    }
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
    /// Long-lived language sessions, one per binding and language, kept until stop or transport failure.
    live: BTreeMap<BindingRef, LiveSessions<LiveEntry>>,
    /// Exclusive TypeScript generations and owner-lifetime exact-profile quarantine.
    typescript: TypeScriptProfiles,
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
            live: BTreeMap::new(),
            typescript: TypeScriptProfiles::default(),
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
        shared_go: bool,
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
            let configuration = effective_configuration(launch.settings);
            let trust = effective_trust(launch);
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
                    AcceptedProviderSettings::TypeScriptDefaultsV1 => &["tmp"][..],
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

    /// Selects an accepted provider after proving the source path under the observed host profile.
    /// Managed children replay that same profile, so its OS sandbox confines all provider reads;
    /// absent profiles and unproven source paths stay lexical.
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
            Some("js") | Some("jsx") | Some("ts") | Some("tsx") => {
                AcceptedProviderSettings::TypeScriptDefaultsV1
            }
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
            AcceptedProviderSettings::TypeScriptDefaultsV1 => self
                .typescript_context(job, &profile, source, bytes, query)
                .await
                .map(Some),
        }
    }

    /// Answers TypeScript context requests through the binding's persistent server session.
    async fn typescript_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        self.ensure_live_typescript(job, launch, source).await?;
        self.live_context(job, &binding, source, bytes, query, Language::Typescript)
            .await
    }

    /// Answers Python context requests through the binding's persistent Pyright session.
    async fn pyright_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        self.ensure_live_pyright(job, launch, source).await?;
        self.live_context(job, &binding, source, bytes, query, Language::Python)
            .await
    }

    /// Runs one context request on a retained session and returns its context with diagnostics.
    ///
    /// `job` supplies cancellation, `binding` selects the retained session, `source` and `bytes`
    /// identify the exact observed document, `query` selects whole-file or symbol context, and
    /// `language` selects diagnostics waiting and telemetry. Python and TypeScript wait for their
    /// diagnostic push, Rust only for whole-file queries; that wait is bounded by
    /// `min(3 s, remaining deadline)` and by `job.cancel`. A failed or cancelled exchange retires
    /// the session; cancellation maps to `Cancelled`, any other failure to `ProviderUnavailable`.
    /// A TypeScript result is additionally checked against the session's project snapshot and
    /// returns `ResolutionUnverified` (with `failure_detail` set) when the project changed.
    async fn live_context(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
        language: Language,
    ) -> Result<ProviderContext, FailureCode> {
        let (result, diagnostics) = {
            let entry = self
                .providers
                .live
                .get_mut(binding)
                .and_then(|sessions| sessions.get_mut(language))
                .ok_or(FailureCode::Internal)?;
            let mut result = {
                let operation = entry.live.session.context(source, bytes, query);
                tokio::pin!(operation);
                tokio::select! { result = &mut operation => result, _ = job.cancel.changed() => Err(std::io::Error::other("cancelled")), }
            };
            if result.is_ok()
                && (matches!(language, Language::Python | Language::Typescript)
                    || matches!(query, ContextQuery::File))
            {
                let budget = Duration::from_secs(3).min(
                    job.deadline
                        .saturating_duration_since(tokio::time::Instant::now()),
                );
                tokio::select! {
                    _ = tokio::time::timeout(budget, entry.live.session.wait_for_matching_diagnostics()) => {}
                    _ = job.cancel.changed() => result = Err(std::io::Error::other("cancelled")),
                }
            }
            (result, entry.live.session.diagnostics())
        };
        let context = match result {
            Ok(context) => context,
            Err(_) => {
                self.release_live_language(binding, language).await;
                return if *job.cancel.borrow() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                };
            }
        };
        let outcome = ProviderContext {
            context,
            diagnostics,
        };
        if language == Language::Typescript
            && !self
                .verify_live_typescript_inputs(job, binding, source)
                .await?
        {
            self.release_live_language(binding, Language::Typescript)
                .await;
            return Err(FailureCode::ResolutionUnverified);
        }
        if let Some(telemetry) = self.telemetry.as_ref() {
            let state = match outcome.diagnostics.readiness {
                crate::intelligence::freshness::DiagnosticReadiness::Clean => {
                    DiagnosticState::Clean
                }
                crate::intelligence::freshness::DiagnosticReadiness::Reported => {
                    DiagnosticState::Changed
                }
                crate::intelligence::freshness::DiagnosticReadiness::Unknown => {
                    DiagnosticState::Unavailable
                }
            };
            adapters::provider_summary(telemetry, language, CacheState::Unavailable, state);
        }
        self.shared.active(binding)?;
        Ok(outcome)
    }

    /// Re-observes project files after a TypeScript request and checks them against the retained view.
    ///
    /// `job` supplies the accepted TypeScript bundle, `binding` selects its live project snapshot,
    /// and `source` identifies the requested document. Returns `false` when project evidence
    /// changed; unobservable or invalid evidence returns `ResolutionUnverified`.
    async fn verify_live_typescript_inputs(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        source: &SourceObservation,
    ) -> Result<bool, FailureCode> {
        let launch = job
            .target
            .providers
            .iter()
            .find(|profile| profile.settings == AcceptedProviderSettings::TypeScriptDefaultsV1)
            .ok_or(FailureCode::ProviderUnavailable)?;
        let authority = self.authority(binding).await?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = self.shared.launcher.allowed_roots().to_vec();
        let current = observe_typescript_inputs(
            job,
            authority.worktree().clone(),
            authority.worktree().worktree_path().join(source.path()),
            bundle,
            roots,
        )
        .await?;
        let matches = self.providers.live.get(binding).is_some_and(|sessions| {
            sessions.get(Language::Typescript).is_some_and(|entry| {
                matches!(&entry.child, LiveChild::TypeScript(_, _, prior) if prior.same_project(&current))
            })
        });
        if !matches {
            job.failure_detail = Some(TYPESCRIPT_INPUTS_CHANGED.to_owned());
        }
        Ok(matches)
    }

    /// Starts the accepted Pyright session for this binding, or keeps its live session.
    ///
    /// `job` supplies cancellation and binding ownership; `launch` is the accepted executable
    /// profile; `source` fixes the worktree and authority epoch. A replaced child is shut down
    /// before another is admitted. Profile, authority, capacity, spawn, handshake, and
    /// cancellation failures return their bounded `FailureCode`.
    async fn ensure_live_pyright(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        if self
            .providers
            .live
            .get(&binding)
            .and_then(|sessions| sessions.get(Language::Python))
            .is_some_and(|entry| {
                matches!(&entry.child, LiveChild::Pyright(..)) && entry.live.is_alive()
            })
        {
            return Ok(());
        }
        self.release_live_language(&binding, Language::Python).await;
        let authority = self.authority(&binding).await?;
        let cache_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let node = launch.node.as_ref().ok_or(FailureCode::ExecutionProfile)?;
        let profile = PyrightProfile::new(PyrightProfileIdentity {
            binary: launch.executable.path.clone(),
            accepted_script_digest: blake3::Hash::from_hex(&launch.executable.blake3)
                .map_err(|_| FailureCode::ExecutionProfile)?,
            version: launch.executable.identity.clone(),
            node: node.path.clone(),
            accepted_node_digest: blake3::Hash::from_hex(&node.blake3)
                .map_err(|_| FailureCode::ExecutionProfile)?,
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
        let mut child = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match PyrightProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                view.lease(),
                Some(active),
                self.shared.launcher.limits.output_bytes,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let _ = view.release(&mut self.providers.registry);
                    if let PyrightProfileError::Process(error) = error {
                        self.provider_spawn_failure(error, &binding);
                    }
                    return Err(FailureCode::ProviderUnavailable);
                }
            }
        };
        let opened = match child.take_pipes() {
            Some((stdin, stdout)) => {
                let open = LiveSession::open(
                    stdout,
                    stdin,
                    source.worktree().clone(),
                    source.authority_epoch(),
                    ViewGeneration {
                        backend: generation,
                        configuration: 1,
                        toolchain: 1,
                        view: generation,
                    },
                    ProviderSettings::Pyright(profile),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel.changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.providers.live.entry(binding).or_default().insert(
                    Language::Python,
                    LiveEntry {
                        child: LiveChild::Pyright(child, view),
                        live,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap_pyright(&binding, child, view).await;
                if *job.cancel.borrow() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Starts the accepted TypeScript session for this binding or reuses its project session.
    ///
    /// `job` supplies cancellation and binding ownership; `launch` provides the accepted bundle;
    /// `source` selects and verifies project inputs. A different worktree, bundle, or captured
    /// project file set shuts down the old session before a new one is admitted. Unverified inputs,
    /// authority, capacity, spawn, handshake, and cancellation failures return a bounded code.
    async fn ensure_live_typescript(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        if !launch.typescript_codex_accepted() {
            return Err(FailureCode::ExecutionProfile);
        }
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        let cache_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = self.shared.launcher.allowed_roots().to_vec();
        let inputs = observe_typescript_inputs(
            job,
            authority.worktree().clone(),
            authority.worktree().worktree_path().join(source.path()),
            bundle.clone(),
            roots.clone(),
        )
        .await?;
        let worktree_root = authority.worktree().worktree_path().to_path_buf();
        let path_proof = |path: &Path| {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                worktree_root.join(path)
            };
            crate::assistance::launcher::admit_path(&roots, &absolute).is_ok()
        };
        if self.providers.live.get(&binding).and_then(|sessions| sessions.get(Language::Typescript)).is_some_and(|entry| {
            matches!(&entry.child, LiveChild::TypeScript(_, _, prior) if prior.same_project(&inputs))
                && entry.live.is_alive()
        }) {
            return Ok(());
        }
        self.release_live_language(&binding, Language::Typescript)
            .await;
        let profile = match TypeScriptProfile::new(
            bundle,
            inputs.clone(),
            launch.trust.clone(),
            Path::new(&cache_namespace).to_path_buf(),
        ) {
            Ok(profile) => profile,
            Err(TypeScriptProfileError::InvalidResolution) => {
                job.failure_detail = Some(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                return Err(FailureCode::ResolutionUnverified);
            }
            Err(_) => return Err(FailureCode::ExecutionProfile),
        };
        let worktree = TypeScriptWorktree::new(
            authority.worktree().clone(),
            execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = profile.command(&worktree, &path_proof).map_err(|error| {
            if matches!(error, TypeScriptProfileError::InvalidResolution) {
                job.failure_detail = Some(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                FailureCode::ResolutionUnverified
            } else {
                FailureCode::ExecutionProfile
            }
        })?;
        let node = launch.node.as_ref().ok_or(FailureCode::ExecutionProfile)?;
        let request = self
            .execution_request(job, &authority, command, node)
            .await?;
        let active = self.shared.active(&binding)?;
        let view = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.providers.typescript.request(
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                owner(&binding)?,
                AdmissionClass::Interactive,
            ) {
                TypeScriptViewAdmission::Granted(view) => view,
                TypeScriptViewAdmission::Queued(ticket) => {
                    self.providers
                        .registry
                        .cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Quarantined) => {
                    return Err(FailureCode::ProviderUnavailable);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let child = {
            let admission = self.admission.clone();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match TypeScriptProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                &mut self.providers.registry,
                &mut admission,
                view.lease(),
                Some(active),
                self.shared.launcher.limits.output_bytes,
                &path_proof,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let _ = self
                        .providers
                        .typescript
                        .release(view, &mut self.providers.registry);
                    let failure = if matches!(&error, TypeScriptProfileError::InvalidResolution) {
                        job.failure_detail = Some(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                        FailureCode::ResolutionUnverified
                    } else {
                        FailureCode::ProviderUnavailable
                    };
                    if let TypeScriptProfileError::Process(error) = error {
                        self.provider_spawn_failure(error, &binding);
                    }
                    return Err(failure);
                }
            }
        };
        let mut child = child;
        let generation = view.generation();
        let opened = match child.take_pipes() {
            Some((stdin, stdout)) => {
                let open = LiveSession::open(
                    stdout,
                    stdin,
                    source.worktree().clone(),
                    source.authority_epoch(),
                    ViewGeneration {
                        backend: generation,
                        configuration: 1,
                        toolchain: 1,
                        view: generation,
                    },
                    ProviderSettings::TypeScript(profile.clone()),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel.changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.providers.live.entry(binding).or_default().insert(
                    Language::Typescript,
                    LiveEntry {
                        child: LiveChild::TypeScript(child, view, Box::new(inputs)),
                        live,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap_typescript(&binding, child, view, false).await;
                if *job.cancel.borrow() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Answers a Rust context request from the binding's long-lived analyzer session, starting
    /// one on first use. Readiness is probed for at most 100 ms; a loading non-edit job records a
    /// 300 ms resume time so the worker can run other work while the analyzer keeps loading.
    async fn rust_context(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        self.ensure_live_rust(job, launch, source).await?;
        let budget = Duration::from_millis(100).min(
            job.deadline
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        let lease = self
            .providers
            .live
            .get(&binding)
            .and_then(|sessions| sessions.get(Language::Rust))
            .and_then(|entry| match &entry.child {
                LiveChild::Rust(_, view) => Some(view.lease()),
                _ => None,
            })
            .ok_or(FailureCode::Internal)?;
        self.providers
            .rust
            .observe_source(lease, source.sequence())
            .map_err(|_| FailureCode::Internal)?;
        let outcome = {
            let entry = self
                .providers
                .live
                .get_mut(&binding)
                .and_then(|sessions| sessions.get_mut(Language::Rust))
                .ok_or(FailureCode::Internal)?;
            let readiness = tokio::select! {
                readiness = entry.live.wait_ready(budget) => readiness,
                _ = job.cancel.changed() => return Err(FailureCode::Cancelled),
            };
            match readiness {
                Ok(()) => {
                    let result = {
                        let operation = entry.live.session.context(source, bytes, query);
                        tokio::pin!(operation);
                        tokio::select! {
                            result = &mut operation => result,
                            _ = job.cancel.changed() => Err(std::io::Error::other("cancelled")),
                        }
                    };
                    // A whole-file query is the post-edit diagnostic read: give the analyzer a
                    // few seconds to publish diagnostics for the synchronized version before
                    // snapshotting, so an edit reply can report `current_clean`/`current_reported`
                    // instead of `unknown`.
                    let mut result = result;
                    if matches!(query, ContextQuery::File) && result.is_ok() {
                        let budget = Duration::from_secs(3).min(
                            job.deadline
                                .saturating_duration_since(tokio::time::Instant::now()),
                        );
                        tokio::select! {
                            _ = tokio::time::timeout(budget, entry.live.session.wait_for_matching_diagnostics()) => {}
                            _ = job.cancel.changed() => result = Err(std::io::Error::other("cancelled")),
                        }
                    }
                    let diagnostics = entry.live.session.diagnostics();
                    result.map(|context| ProviderContext {
                        context,
                        diagnostics,
                    })
                }
                Err(ReadinessError::Loading) => {
                    // An edit may already have changed the worktree; report provider diagnostics
                    // unknown and let its receipt settle instead of restarting that mutation.
                    if job.tool != AssistanceTool::Edit
                        && job
                            .deadline
                            .saturating_duration_since(tokio::time::Instant::now())
                            > Duration::from_secs(1)
                    {
                        job.park_until =
                            Some(tokio::time::Instant::now() + Duration::from_millis(300));
                    }
                    return Err(FailureCode::ProviderLoading);
                }
                Err(ReadinessError::WorkspaceError) => {
                    return Err(FailureCode::ProviderUnavailable);
                }
                Err(ReadinessError::Gone) => Err(std::io::Error::other("transport gone")),
            }
        };
        let result = match outcome {
            Ok(context) => Ok(context),
            Err(_) => {
                // A failed or cancelled exchange retires the session; the next request starts a
                // fresh one.
                self.release_live_language(&binding, Language::Rust).await;
                if *job.cancel.borrow() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        };
        if let Some(telemetry) = self.telemetry.as_ref() {
            let diagnostics = match &result {
                Ok(context) => match context.diagnostics.readiness {
                    crate::intelligence::freshness::DiagnosticReadiness::Clean => {
                        DiagnosticState::Clean
                    }
                    crate::intelligence::freshness::DiagnosticReadiness::Reported => {
                        DiagnosticState::Changed
                    }
                    crate::intelligence::freshness::DiagnosticReadiness::Unknown => {
                        DiagnosticState::Unavailable
                    }
                },
                Err(_) => DiagnosticState::Unavailable,
            };
            adapters::provider_summary(
                telemetry,
                Language::Rust,
                CacheState::Unavailable,
                diagnostics,
            );
        }
        self.shared.active(&binding)?;
        result
    }

    /// Returns the binding's ready session selected by the source extension.
    ///
    /// `job` supplies accepted provider settings, cancellation, and a bounded readiness deadline;
    /// `source` must be a supported Rust, Python, or TypeScript-family observation. Unsupported
    /// extensions and missing provider settings return `ProviderUnavailable`; loading returns an
    /// internal marker and parks eligible jobs, while edit diagnostics return `ProviderLoading`
    /// directly so a prior write can settle. Workspace failure and dead transport remain errors.
    pub(super) async fn live_session_for(
        &mut self,
        job: &mut Job,
        source: &SourceObservation,
    ) -> Result<&mut LiveSession, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let required = match source.path().extension().and_then(|value| value.to_str()) {
            Some("rs") => AcceptedProviderSettings::RustCachePrimingDisabledV1,
            Some("py" | "pyi") => AcceptedProviderSettings::PyrightDefaultsV1,
            Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs") => {
                AcceptedProviderSettings::TypeScriptDefaultsV1
            }
            _ => return Err(FailureCode::ProviderUnavailable),
        };
        let language = match required {
            AcceptedProviderSettings::RustCachePrimingDisabledV1 => Language::Rust,
            AcceptedProviderSettings::PyrightDefaultsV1 => Language::Python,
            AcceptedProviderSettings::TypeScriptDefaultsV1 => Language::Typescript,
            _ => return Err(FailureCode::ProviderUnavailable),
        };
        let launch = job
            .target
            .providers
            .iter()
            .find(|profile| profile.settings == required)
            .cloned()
            .ok_or(FailureCode::ProviderUnavailable)?;
        match required {
            AcceptedProviderSettings::RustCachePrimingDisabledV1 => {
                self.ensure_live_rust(job, &launch, source).await?
            }
            AcceptedProviderSettings::PyrightDefaultsV1 => {
                self.ensure_live_pyright(job, &launch, source).await?
            }
            AcceptedProviderSettings::TypeScriptDefaultsV1 => {
                self.ensure_live_typescript(job, &launch, source).await?
            }
            _ => return Err(FailureCode::ProviderUnavailable),
        }
        let budget = Duration::from_millis(100).min(
            job.deadline
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        let readiness = {
            let entry = self
                .providers
                .live
                .get_mut(&binding)
                .and_then(|sessions| sessions.get_mut(language))
                .ok_or(FailureCode::Internal)?;
            tokio::select! {
                readiness = entry.live.wait_ready(budget) => readiness,
                _ = job.cancel.changed() => return Err(FailureCode::Cancelled),
            }
        };
        match readiness {
            Ok(()) => {}
            Err(ReadinessError::Loading) => {
                // Post-edit provider data is optional: do not restart a write whose receipt must settle.
                if job.tool != AssistanceTool::Edit
                    && job
                        .deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        > Duration::from_secs(1)
                {
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(300));
                }
                return Err(FailureCode::ProviderLoading);
            }
            Err(ReadinessError::WorkspaceError) => return Err(FailureCode::ProviderUnavailable),
            Err(ReadinessError::Gone) => {
                let cancelled = *job.cancel.borrow();
                self.release_live_language(&binding, language).await;
                return Err(if cancelled {
                    FailureCode::Cancelled
                } else {
                    FailureCode::ProviderUnavailable
                });
            }
        }
        self.providers
            .live
            .get_mut(&binding)
            .and_then(|sessions| sessions.get_mut(language))
            .map(|entry| &mut entry.live)
            .ok_or(FailureCode::Internal)
    }

    /// Starts the binding's long-lived Rust session unless its Rust child is still live.
    ///
    /// `job` supplies binding ownership, cancellation, and spawn authority; `launch` is the
    /// accepted analyzer profile; `source` fixes the worktree and authority epoch. Admission,
    /// profile, spawn, initialization, and cancellation failures return a bounded code.
    async fn ensure_live_rust(
        &mut self,
        job: &mut Job,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        if let Some(entry) = self
            .providers
            .live
            .get(&binding)
            .and_then(|sessions| sessions.get(Language::Rust))
        {
            if matches!(&entry.child, LiveChild::Rust(..)) && entry.live.is_alive() {
                return Ok(());
            }
            self.release_live_language(&binding, Language::Rust).await;
        }
        let authority = self.authority(&binding).await?;
        let cache_namespace =
            self.provider_cache_namespace(&binding, &authority, launch, &launch.trust)?;
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
            configuration: effective_configuration(launch.settings).into(),
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
        // The shared controller guard is confined to this block: it is a `std` mutex, so it must
        // never reach the awaits below or this worker future stops being `Send`.
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
        let mut child = match RustProtocolChild::spawn(
            &request,
            &worktree,
            &mut self.providers.registry,
            view.lease(),
            Some(active),
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
        let generation = ViewGeneration {
            backend: view.generation(),
            configuration: 1,
            toolchain: 1,
            view: view.generation(),
        };
        let opened = match child.take_pipes() {
            Some((stdin, stdout)) => {
                let open = LiveSession::open(
                    stdout,
                    stdin,
                    source.worktree().clone(),
                    source.authority_epoch(),
                    generation,
                    ProviderSettings::Rust(profile),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                // Stop or shutdown must be able to interrupt a handshake the server never answers.
                tokio::select! {
                    opened = &mut open => opened,
                    _ = job.cancel.changed() => Err(std::io::Error::other("cancelled")),
                }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.providers.live.entry(binding).or_default().insert(
                    Language::Rust,
                    LiveEntry {
                        child: LiveChild::Rust(child, view),
                        live,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap_rust(&binding, child, view).await;
                if *job.cancel.borrow() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Shuts down and reaps every live language session owned by one binding.
    pub(super) async fn release_live(&mut self, binding: &BindingRef) {
        let Some(mut sessions) = self.providers.live.remove(binding) else {
            return;
        };
        for language in [Language::Rust, Language::Python, Language::Typescript] {
            if let Some(entry) = sessions.take(language) {
                self.release_live_entry(binding, entry).await;
            }
        }
    }

    /// Shuts down and reaps only one language session, preserving its sibling sessions.
    async fn release_live_language(&mut self, binding: &BindingRef, language: Language) {
        let entry = self
            .providers
            .live
            .get_mut(binding)
            .and_then(|sessions| sessions.take(language));
        if self
            .providers
            .live
            .get(binding)
            .is_some_and(LiveSessions::is_empty)
        {
            self.providers.live.remove(binding);
        }
        if let Some(entry) = entry {
            self.release_live_entry(binding, entry).await;
        }
    }

    /// Shuts down and settles one language child and its exact provider view.
    async fn release_live_entry(
        &mut self,
        binding: &BindingRef,
        LiveEntry { child, live }: LiveEntry,
    ) {
        let shutdown_completed = live.shutdown().await;
        match child {
            LiveChild::Rust(child, view) => self.reap_rust(binding, child, view).await,
            LiveChild::Pyright(child, view) => self.reap_pyright(binding, child, view).await,
            LiveChild::TypeScript(child, view, _) => {
                self.reap_typescript(binding, child, view, shutdown_completed)
                    .await
            }
        }
    }

    /// Shuts down and reaps all retained language sessions during worker shutdown.
    pub(super) async fn release_all_live(&mut self) {
        let bindings = self.providers.live.keys().cloned().collect::<Vec<_>>();
        for binding in bindings {
            self.release_live(&binding).await;
        }
    }

    /// Reaps a Rust child and returns its exclusive view to provider accounting.
    async fn reap_rust(&mut self, binding: &BindingRef, child: RustProtocolChild, view: RustView) {
        match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => {
                if let Ok(capability) = self
                    .providers
                    .rust
                    .release(&mut self.providers.registry, view.lease())
                {
                    let admission = self.admission.clone();
                    let mut admission = admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = self.providers.registry.complete_reap(
                        &mut admission,
                        capability,
                        reaped.proof,
                    );
                }
            }
            Err(_) => {
                self.uncertain.insert(binding.clone());
                let _ = self
                    .providers
                    .rust
                    .release(&mut self.providers.registry, view.lease());
            }
        }
    }

    /// Reaps a Pyright child and returns its exclusive view to provider accounting.
    async fn reap_pyright(
        &mut self,
        binding: &BindingRef,
        child: PyrightProtocolChild,
        view: crate::intelligence::pyright::PyrightView,
    ) {
        match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => {
                if let Ok(capability) = view.release(&mut self.providers.registry) {
                    let admission = self.admission.clone();
                    let mut admission = admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = self.providers.registry.complete_reap(
                        &mut admission,
                        capability,
                        reaped.proof,
                    );
                }
            }
            Err(_) => {
                self.uncertain.insert(binding.clone());
                let _ = view.release(&mut self.providers.registry);
            }
        }
    }

    /// Reaps a TypeScript bridge and settles its exact view.
    ///
    /// `shutdown_completed` reports whether the LSP shutdown exchange was answered while the session
    /// was alive. Only then is the child given one second to exit on its own, and only an observed
    /// nonzero exit quarantines the profile key. A shutdown that never completed (client-initiated
    /// teardown after invalidation, a hung server, a failed handshake) and a graceful wait that
    /// times out go straight to abnormal termination without quarantining, because neither proves
    /// the bridge itself is broken. A reap that cannot settle marks the binding uncertain.
    async fn reap_typescript(
        &mut self,
        binding: &BindingRef,
        mut child: TypeScriptProtocolChild,
        view: crate::intelligence::typescript::TypeScriptView,
        shutdown_completed: bool,
    ) {
        let result = if shutdown_completed {
            match child.wait_for_exit(Duration::from_secs(1)).await {
                Ok(waited) => {
                    self.providers
                        .typescript
                        .quarantine_after_unsuccessful_wait(&view, &waited);
                    child.finish_reap(waited, Duration::from_millis(500)).await
                }
                Err(_) => {
                    child
                        .terminate_abnormally(
                            Duration::from_millis(100),
                            Duration::from_millis(500),
                        )
                        .await
                }
            }
        } else {
            child
                .terminate_abnormally(Duration::from_millis(100), Duration::from_millis(500))
                .await
        };
        match result {
            Ok(reaped) => {
                if let Ok(capability) = self
                    .providers
                    .typescript
                    .release(view, &mut self.providers.registry)
                {
                    let admission = self.admission.clone();
                    let mut admission = admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = self.providers.registry.complete_reap(
                        &mut admission,
                        capability,
                        reaped.proof,
                    );
                }
            }
            Err(_) => {
                self.uncertain.insert(binding.clone());
                let _ = self
                    .providers
                    .typescript
                    .release(view, &mut self.providers.registry);
            }
        }
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
        let trust = effective_trust(launch);
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
            let telemetry = self.telemetry.clone();
            let operation = session_operation(
                child.stdout.as_mut().expect("protocol stdout taken"),
                child.stdin.as_mut().expect("protocol stdin taken"),
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
                telemetry.as_ref(),
                Language::Go,
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
        AcceptedProviderSettings::TypeScriptDefaultsV1 => "typescript-defaults-v1",
    }
}

/// Returns the exact initialization configuration identity a provider command will use.
fn effective_configuration(settings: AcceptedProviderSettings) -> &'static str {
    match settings {
        AcceptedProviderSettings::GoplsDefaults => "gopls-defaults-v1",
        AcceptedProviderSettings::RustCachePrimingDisabledV1 => "cache-priming-disabled-v1",
        AcceptedProviderSettings::PyrightDefaultsV1 => "pyright-defaults-v1",
        AcceptedProviderSettings::TypeScriptDefaultsV1 => "typescript-defaults-v1",
    }
}

/// Fixed compatibility-identity marker used only by the shared native namespace.
///
/// It deliberately excludes any worktree so every divergent worktree with a compatible
/// executable/settings/toolchain/effective-rights identity resolves to the same shared key and,
/// through it, to the same one heavy `gopls` listener.
pub(super) const SHARED_NATIVE_CACHE_STATE: &str = "shared-native-v1";

/// Returns the one canonical trust identity used by the cache key, the `CacheIdentity`, the
/// `GoplsProfile` compatibility key, and the shared refcount.
fn effective_trust(launch: &ProviderLaunch) -> String {
    launch.trust.clone()
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
    crate::execution::WorkspaceAuthority::from_workspace_with_git_common_dir(
        authority.worktree().id(),
        authority.worktree().incarnation().to_string(),
        authority.worktree().worktree_path().to_path_buf(),
        authority.worktree().git_common_dir().to_path_buf(),
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

/// Runs the accepted settings handshake and one exact-source query. It waits for Pyright's
/// versioned diagnostics until the inherited deadline minus shutdown reserve, or up to two
/// seconds for a bound nonempty TypeScript report, then snapshots bounded evidence and shuts down.
/// Other profiles preserve their immediate snapshot. Transport or protocol failures return
/// `ProviderUnavailable`; the caller still owns and must reap the protocol child.
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
    telemetry: Option<&Telemetry>,
    language: Language,
) -> Result<ProviderContext, FailureCode> {
    let tree = source.worktree().clone();
    let epoch = source.authority_epoch();
    let result = with_session(
        input,
        output,
        tree,
        epoch,
        generation,
        settings,
        options,
        |mut session| async move {
            let context = session.context(&source, &bytes, query).await?;
            if matches!(
                session.settings(),
                ProviderSettings::Pyright(_) | ProviderSettings::TypeScript(_)
            ) {
                session.wait_for_matching_diagnostics().await;
            }
            let diagnostics = session.diagnostics();
            session.shutdown().await?;
            Ok(ProviderContext {
                context,
                diagnostics,
            })
        },
    )
    .await
    .map_err(|_| FailureCode::ProviderUnavailable);
    if let Some(telemetry) = telemetry {
        let diagnostics = match &result {
            Ok(context) => match context.diagnostics.readiness {
                crate::intelligence::freshness::DiagnosticReadiness::Clean => {
                    DiagnosticState::Clean
                }
                crate::intelligence::freshness::DiagnosticReadiness::Reported => {
                    DiagnosticState::Changed
                }
                crate::intelligence::freshness::DiagnosticReadiness::Unknown => {
                    DiagnosticState::Unavailable
                }
            },
            Err(_) => DiagnosticState::Unavailable,
        };
        adapters::provider_summary(telemetry, language, CacheState::Unavailable, diagnostics);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        FailureCode, GoBackend, LiveSessions, OwnedProviderSocket, Providers, reap_owned_backend,
    };
    use crate::assistance::host_binding::{
        BindingRef, BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        parse_hook_event,
    };
    use crate::execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLimits, BackendRelease,
        ControlledCommand, LocalExecutionPolicy, OwnerId, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderViewLease, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
    };
    use crate::intelligence::gopls::{GoplsProfile, SharedGopls};
    use crate::workspace::authority::WorktreeRef;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Taking the Python slot leaves the same binding's retained Rust session untouched.
    #[test]
    fn language_session_release_is_binding_local() {
        let mut sessions = LiveSessions::default();
        sessions.insert(crate::telemetry::Language::Rust, "rust-session");
        sessions.insert(crate::telemetry::Language::Python, "pyright-session");

        assert_eq!(
            sessions.take(crate::telemetry::Language::Python),
            Some("pyright-session")
        );
        assert_eq!(
            sessions.get(crate::telemetry::Language::Rust),
            Some(&"rust-session")
        );
        assert!(!sessions.is_empty());
    }

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
        ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_verified_binding(label).unwrap(),
            authority.clone(),
            command,
            &LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/usr/bin/true")]), 4096, 16)
                .unwrap(),
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
