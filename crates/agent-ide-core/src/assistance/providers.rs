//! Scoped language-provider adapters for the single product worker and its owned process accounting.
//!
//! The worker never branches on a language here: each configured language server is a
//! [`LanguageServer`] with one [`ServerBackend`] per worker, and this module routes requests to
//! them, retains their cache namespaces, and hands them the worker's facilities through
//! [`ProviderHost`] and [`ProviderJob`].

use super::*;
use crate::assistance::launcher::ProviderLaunch;
use crate::{
    app::cache::{CacheNamespaceId, CacheRoot},
    execution::{
        ControlledCommand, ProcessError, ProviderLeaseLimits, ProviderLeaseRegistry,
        ValidatedExecutionRequest,
    },
    intelligence::{
        context::ContextQuery,
        freshness::{CacheIdentity, CacheLifecycle},
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ReadinessError},
    },
    telemetry::Telemetry,
};
use std::path::Path;

/// Every registered language server in registration order, which is also release order.
fn servers() -> Vec<&'static dyn LanguageServer> {
    crate::lang::registered()
        .iter()
        .filter_map(|language| language.server())
        .collect()
}

/// The clause a failed-session stage appends to say what still answers without the server:
/// `ide.outline` and `ide.read` from the source outline for a language that opted into
/// [`LanguageSupport::outline_while_loading`](crate::lang::LanguageSupport::outline_while_loading),
/// native reads for every other language. The reply template keys its recovery sentence on
/// exactly this clause, so the producer — the side that knows the language — decides it.
pub(super) fn session_fallback_clause(source_outlines: bool) -> &'static str {
    if source_outlines {
        "; outline and read answer from source"
    } else {
        "; use native reads"
    }
}

/// Bounds the in-memory cache-lifecycle map so an unbounded stream of distinct worktree
/// incarnations cannot grow it or the retained on-disk namespaces without limit. Matches the fixed
/// `total_views`/`per_backend_views` provider-lease ceiling; a full map fails new namespaces closed
/// rather than evicting an unretired one without closure proof.
const MAX_CACHE_NAMESPACES: usize = 64;

/// One language server and its worker-owned backend.
struct ServerSlot {
    /// Static description used for routing, caches and capabilities.
    server: &'static dyn LanguageServer,
    /// The backend; `None` only while one of its operations is running. The worker is a single
    /// sequential task and never drops an operation future midway (only a whole-worker abort does),
    /// so a slot is always refilled before the next operation.
    backend: Option<Box<dyn ServerBackend>>,
}

/// Provider state owned by the sole worker; no client holds executable or settlement capabilities.
pub(super) struct Providers {
    /// Directory holding every retained native cache namespace: `~/.agent-ide/providers` so a
    /// restarted daemon (or a rebooted machine) adopts them, the runtime-local `cache` only when
    /// the per-user state root is unusable.
    cache_root: std::path::PathBuf,
    /// Central typed backend/view accounting; physical limits live in the worker admission controller.
    registry: ProviderLeaseRegistry,
    /// One slot per registered language server, in registration order.
    slots: Vec<ServerSlot>,
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
    /// Why the last [`ProviderHost`] namespace lookup refused, so the caller that sees only a bare
    /// `ProviderUnavailable` from a backend can still name the stage. Set by
    /// [`Worker::retained_cache_namespace`], taken by [`Providers::name_bare_failure`].
    refusal: std::sync::Mutex<Option<&'static str>>,
    /// Bindings whose reader-to-writer upgrade failed while releasing their own reader-owned
    /// state. The grant already reads `Writer`, so the retry cannot tell from its role that the
    /// release is still owed; this marker keeps the debt until the release succeeds or the
    /// binding's namespace is quiesced ([`Worker::release_reader_owners`]).
    pending_handover: std::collections::BTreeSet<BindingRef>,
    /// Sessions that failed (workspace load, or a call that used them failed), by owning binding
    /// and server slot, with what they were started against. A session stays here, serving its
    /// staged refusal, until a success clears it or [`Worker::retire_changed_session`] finds its
    /// basis changed and retires it; an unchanged basis never restarts it.
    failed: BTreeMap<(BindingRef, usize), SessionBasis>,
    /// The owner and server slot of the session the running job used, so the worker can settle
    /// that session's health when the job ends ([`Worker::settle_session_health`]).
    pub(super) current: Option<(BindingRef, usize)>,
    /// What the running job's provider calls showed about each session it used, by owner and
    /// server slot. Recorded where the provider answers or fails, not from the tool's final
    /// result: a tool that falls back to a source outline after the session failed still succeeds,
    /// and the session is still failed. One session's outcome never marks or clears another's.
    health: BTreeMap<(BindingRef, usize), SessionHealth>,
}

/// What a job's provider calls showed about the session it used.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionHealth {
    /// A provider call completed against the session and nothing failed.
    Healthy,
    /// A provider call failed against the session; sticky for the rest of the job.
    Failed,
}

/// What a failed provider session was running against: Git's `HEAD` and the content stamp of the
/// server's project input files. A change of either is the only reason to retire it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SessionBasis {
    /// `HEAD` when the worktree's Git metadata validated, else `None`.
    head: Option<crate::workspace::git::head::HeadState>,
    /// Digest of the (relative path, length, modification time) of every project input file.
    inputs: [u8; 32],
}

/// Chooses where a daemon whose runtime directory is `runtime` keeps its provider cache
/// namespaces: `<state root>/providers` when that is a private directory, else `<runtime>/cache`.
pub(super) fn cache_root(runtime: &std::path::Path) -> std::path::PathBuf {
    crate::retention::state_root()
        .and_then(|state| crate::retention::providers_root(&state))
        .unwrap_or_else(|| runtime.join("cache"))
}

impl Providers {
    /// Creates fixed finite provider bookkeeping and one idle backend per server without launching
    /// processes.
    pub(super) fn new(cache_root: std::path::PathBuf) -> Self {
        Self {
            cache_root,
            registry: ProviderLeaseRegistry::new(ProviderLeaseLimits {
                total_views: 64,
                per_backend_views: 64,
            })
            .expect("fixed view limits"),
            slots: servers()
                .into_iter()
                .map(|server| ServerSlot {
                    server,
                    backend: Some(server.new_backend()),
                })
                .collect(),
            generation: 0,
            caches: BTreeMap::new(),
            binding_caches: BTreeMap::new(),
            shared_cache_refs: BTreeMap::new(),
            refusal: std::sync::Mutex::new(None),
            pending_handover: std::collections::BTreeSet::new(),
            failed: BTreeMap::new(),
            current: None,
            health: BTreeMap::new(),
        }
    }

    /// Starts a job's session-health tracking: no session used, nothing observed.
    pub(super) fn begin_job(&mut self) {
        self.current = None;
        self.health.clear();
    }

    /// Records that a provider call failed against the session the job used last (the one
    /// [`Worker::live_session_for`] or [`Worker::semantic_context`] handed out immediately
    /// before the call); later successes of the same job never undo it.
    pub(super) fn note_session_fault(&mut self) {
        if let Some(key) = self.current.clone() {
            self.health.insert(key, SessionHealth::Failed);
        }
    }

    /// Records that a provider call completed against the session the job used last, unless one
    /// already failed against it.
    fn note_session_healthy(&mut self) {
        if let Some(key) = self.current.clone() {
            self.health.entry(key).or_insert(SessionHealth::Healthy);
        }
    }

    /// Forgets the recorded namespace refusal; called before a backend step so a stale cause of an
    /// earlier call never names a later failure.
    fn clear_refusal(&self) {
        *self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Gives a backend's bare `ProviderUnavailable` its stage: the namespace refusal recorded
    /// during the step, else `step`, prefixed by the server name and followed by what still
    /// answers without the server. A failure that already set its own detail, and every other
    /// code, is left alone.
    ///
    /// `step` is a closed phrase naming the failing step, never a path or payload.
    fn name_bare_failure(
        &self,
        job: &mut Job,
        server: &'static dyn LanguageServer,
        step: &str,
        code: &FailureCode,
    ) {
        let refusal = self
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if *code != FailureCode::ProviderUnavailable || job.failure_detail.is_some() {
            return;
        }
        job.set_stage_failure(
            code,
            &format!(
                "{}: {}{}",
                server.name(),
                refusal.unwrap_or(step),
                session_fallback_clause(server.language().support().outline_while_loading())
            ),
        );
    }

    /// Mints one checked protocol generation without using timing/PID as actor identity.
    fn next(&mut self) -> Result<u64, FailureCode> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        Ok(self.generation)
    }

    /// Returns the slot index of `server`.
    fn slot_of(&self, server: &'static dyn LanguageServer) -> Result<usize, FailureCode> {
        self.slots
            .iter()
            .position(|slot| slot.server.language() == server.language())
            .ok_or(FailureCode::Internal)
    }

    /// Takes the backend of slot `index` out for one operation; `Internal` if it is already out.
    fn take_backend(&mut self, index: usize) -> Result<Box<dyn ServerBackend>, FailureCode> {
        self.slots
            .get_mut(index)
            .and_then(|slot| slot.backend.take())
            .ok_or(FailureCode::Internal)
    }

    /// Gives every slot whose backend a panic dropped mid-operation a fresh idle backend.
    ///
    /// A panic unwinds through an operation that had taken its backend out of the slot, so the
    /// backend (and the sessions it held) is gone and the slot would answer `internal` for every
    /// later call of that language. The replacement holds no session; dropping the old backend
    /// already requested its owned children's cleanup. The daemon is marked failed right after
    /// and replaced, so this only keeps the remaining shutdown path total.
    pub(super) fn repair_after_panic(&mut self) {
        for slot in &mut self.slots {
            if slot.backend.is_none() {
                slot.backend = Some(slot.server.new_backend());
            }
        }
    }

    /// Returns a backend taken with [`Providers::take_backend`] to its slot.
    fn put_backend(&mut self, index: usize, backend: Box<dyn ServerBackend>) {
        if let Some(slot) = self.slots.get_mut(index) {
            slot.backend = Some(backend);
        }
    }

    /// Returns the server whose symbol-tool session answers `path`, if any.
    pub(super) fn session_server(&self, path: &Path) -> Option<&'static dyn LanguageServer> {
        let extension = path.extension().and_then(|value| value.to_str())?;
        self.slots
            .iter()
            .map(|slot| slot.server)
            .find(|server| server.session_extensions().contains(&extension))
    }
}

impl Worker<'_> {
    /// Closes every retained provider view and quiesces its cache before daemon shutdown completes.
    /// Returns the first cleanup failure after still attempting every independently owned backend.
    pub(super) async fn close_all_providers(&mut self) -> Result<(), FailureCode> {
        let bindings = self
            .providers
            .slots
            .iter()
            .filter_map(|slot| slot.backend.as_ref())
            .flat_map(|backend| backend.bound_bindings())
            .chain(self.providers.binding_caches.keys().cloned())
            .chain(self.grants.keys().cloned())
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
    /// Repeating the same binding/plan keeps its live ownership and shared reference counts.
    /// A quiescent compatible lifecycle is handed to a different incoming binding. Preparation is
    /// transactional (see `retain_cache_plan`), and `binding_caches` records the incoming binding's
    /// keys only once every launch succeeded. Returns the finite reason instead of a bare failure:
    /// `Conflict` when another actor still actively owns this worktree's namespace, `Capacity` when
    /// in-memory lifecycle ownership is full, and `ProviderUnavailable` for a local cache-directory
    /// or identity failure. `Internal` reports a namespace component this module itself derived
    /// wrongly. `shared_native` retains the extra shared native namespace of servers that have one (see
    /// [`LanguageServer::shared_cache_directories`]) only for the managed shared-listener path; one-shot Claude helpers pass false and retain only their worktree namespace. No failure
    /// deletes or quiesces a retained namespace.
    pub(super) fn retain_worktree_caches(
        &mut self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launches: &[ProviderLaunch],
        shared_native: bool,
    ) -> Result<(), FailureCode> {
        let root = CacheRoot::prepare(&self.providers.cache_root)
            .map_err(|_| FailureCode::ProviderUnavailable)?;
        let worktree_state = cache_state(authority.worktree());
        let mut plan = Vec::with_capacity(launches.len() * 2);
        for launch in launches {
            let server = launch.server();
            let settings = server.cache_settings();
            let configuration = server.effective_configuration();
            let trust = server::effective_trust(launch);
            plan.push(CacheRequest {
                key: provider_cache_key(&worktree_state, launch, settings, configuration, &trust),
                identity: CacheIdentity::new(
                    launch.executable.identity.clone(),
                    settings,
                    configuration,
                    launch.toolchain.clone(),
                    trust.clone(),
                    worktree_state.clone(),
                )
                .ok_or(FailureCode::ProviderUnavailable)?,
                required: server.cache_directories(),
                shared: false,
            });
            if shared_native && let Some(required) = server.shared_cache_directories() {
                plan.push(CacheRequest {
                    key: provider_cache_key(
                        SHARED_NATIVE_CACHE_STATE,
                        launch,
                        settings,
                        configuration,
                        &trust,
                    ),
                    identity: CacheIdentity::new(
                        launch.executable.identity.clone(),
                        settings,
                        configuration,
                        launch.toolchain.clone(),
                        trust,
                        SHARED_NATIVE_CACHE_STATE,
                    )
                    .ok_or(FailureCode::ProviderUnavailable)?,
                    required,
                    shared: true,
                });
            }
        }
        if let Some(owned) = self.providers.binding_caches.get(binding) {
            let keys: Vec<_> = plan.iter().map(|request| request.key.clone()).collect();
            if owned != &keys {
                return Err(FailureCode::Conflict);
            }
            return if owned.iter().all(|key| {
                self.providers
                    .caches
                    .get(key)
                    .is_some_and(|cache| cache.retained() && !cache.quiescent())
            }) {
                Ok(())
            } else {
                Err(FailureCode::ProviderUnavailable)
            };
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
        self.providers.pending_handover.remove(binding);
        self.providers
            .failed
            .retain(|(owner, _), _| owner != binding);
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

    /// Test-only count of sessions currently marked failed.
    #[cfg(test)]
    pub(super) fn test_failed_sessions(&self) -> usize {
        self.providers.failed.len()
    }

    /// Test-only: records a failed provider call against the session the running job used, the
    /// way a lexical fallback that swallowed a provider failure would have.
    #[cfg(test)]
    pub(super) fn test_note_session_fault(&mut self) {
        self.providers.note_session_fault();
    }

    /// Test-only read of whether `binding` still owns any cache keys.
    #[cfg(test)]
    pub(super) fn test_binding_owns_caches(&self, binding: &BindingRef) -> bool {
        self.providers.binding_caches.contains_key(binding)
    }

    /// Selects an accepted provider after proving the source path under the observed host profile.
    /// Managed children replay that same profile, so its OS sandbox confines all provider reads;
    /// absent profiles and unproven source paths stay lexical.
    ///
    /// The server is chosen by `source`'s extension ([`LanguageServer::context_extensions`]);
    /// `Ok(None)` when no server owns the extension or the target configures none for it.
    /// Persistent sessions admit the source under fresh invoking-actor authority, even when
    /// borrowing the worktree's session owner's transport (the writer, else the reader that owns
    /// the namespace; see [`Worker::resolve_session_owner`]); a mismatched source returns
    /// `WorkspaceAuthority`. As for [`Worker::live_session_for`], a bare `ProviderUnavailable` is
    /// named before it returns and a failed session whose inputs or `HEAD` changed is retired first.
    pub(super) async fn semantic_context(
        &mut self,
        job: &mut Job,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<Option<ProviderContext>, FailureCode> {
        let Some(extension) = source.path().extension().and_then(|value| value.to_str()) else {
            return Ok(None);
        };
        let Some(index) = self
            .providers
            .slots
            .iter()
            .position(|slot| slot.server.context_extensions().contains(&extension))
        else {
            return Ok(None);
        };
        let server = self.providers.slots[index].server;
        let Some(launch) = job
            .target
            .providers
            .iter()
            .find(|launch| launch.language == server.language())
            .cloned()
        else {
            return Ok(None);
        };
        let source_authority = self.authority(job.invocation.binding_ref()).await?;
        // A reader's semantic call serves from the worktree's session owner for this one backend
        // call; a writer-less reader becomes that owner first.
        self.resolve_session_owner(job, &source_authority).await?;
        let owner = job
            .session_binding
            .clone()
            .unwrap_or_else(|| job.invocation.binding_ref().clone());
        self.retire_changed_session(&owner, index, &source_authority)
            .await;
        self.providers.current = Some((owner, index));
        let mut backend = self.providers.take_backend(index)?;
        self.providers.clear_refusal();
        let result = async {
            if server.session_extensions().contains(&extension) {
                backend.ensure_live(self, job, &launch, source).await?;
                backend
                    .live_session(job.binding())
                    .ok_or(FailureCode::Internal)?
                    .session
                    .authorize_source(&source_authority, source)
                    .map_err(|_| FailureCode::WorkspaceAuthority)?;
            }
            backend
                .context(self, job, &launch, source, bytes, query)
                .await
        }
        .await;
        job.session_binding = None;
        self.providers.put_backend(index, backend);
        match &result {
            Err(code) => {
                self.providers
                    .name_bare_failure(job, server, "session request failed", code);
                if *code == FailureCode::ProviderUnavailable {
                    self.providers.note_session_fault();
                }
            }
            Ok(_) => self.providers.note_session_healthy(),
        }
        result.map(Some)
    }

    /// Returns the binding's ready session selected by the source extension.
    ///
    /// `job` supplies accepted provider settings, cancellation, and a bounded readiness deadline;
    /// `source` must have an extension some server's live session answers
    /// ([`LanguageServer::session_extensions`]). Unsupported extensions and missing provider
    /// settings return `ProviderUnavailable`; loading returns `ProviderLoading` and parks eligible
    /// jobs, while edit diagnostics return `ProviderLoading` without parking so a prior write can
    /// settle. Workspace failure and dead transport remain errors; a dead transport retires the
    /// session. The invoking actor's fresh authority admits `source` into the selected transport;
    /// borrowing the owner's session (the writer's, or the owning reader's when no writer holds the
    /// worktree) does not replace the invoking reader's source identity or its epoch. A reader with
    /// no writer and no reading owner beside it claims the namespace first
    /// ([`Worker::resolve_session_owner`]).
    ///
    /// Every `ProviderUnavailable` leaves `job` with a parenthesised stage: the backend's own, else
    /// `<server>: <step>` ([`Providers::name_bare_failure`]), else the no-server form for a path
    /// no accepted server serves. A session this job's owner had failed is retired first when its
    /// project inputs changed or Git's `HEAD` moved since ([`Worker::retire_changed_session`]);
    /// the session used is recorded so the job's end settles its health.
    ///
    /// The slot's backend is taken out for the `ensure_live` await. A panic inside it is caught
    /// here only to put that backend back (with the sessions it already retained, which the
    /// shutdown reap still releases explicitly) before the unwind continues to the worker's
    /// per-job guard, so a contained panic never leaves the slot empty for later calls. The
    /// panic is never swallowed: the worker marks the daemon failed and it is replaced.
    pub(super) async fn live_session_for(
        &mut self,
        job: &mut Job,
        source: &SourceObservation,
    ) -> Result<&mut LiveSession, FailureCode> {
        let source_authority = self.authority(job.invocation.binding_ref()).await?;
        // A reader's semantic tools serve from the worktree's session owner for this whole
        // resolution (a writer-less reader becomes that owner first); the mapping is cleared
        // again below so no non-provider path of the same job ever sees it.
        self.resolve_session_owner(job, &source_authority).await?;
        let binding = job
            .session_binding
            .clone()
            .unwrap_or_else(|| job.invocation.binding_ref().clone());
        let server = self
            .providers
            .session_server(source.path())
            .ok_or_else(|| {
                job.session_binding = None;
                set_no_server_stage(job);
                FailureCode::ProviderUnavailable
            })?;
        let index = match self.providers.slot_of(server) {
            Ok(index) => index,
            Err(code) => {
                job.session_binding = None;
                return Err(code);
            }
        };
        let launch = job
            .target
            .providers
            .iter()
            .find(|launch| launch.language == server.language())
            .cloned()
            .ok_or_else(|| {
                job.session_binding = None;
                set_no_server_stage(job);
                FailureCode::ProviderUnavailable
            })?;
        self.retire_changed_session(&binding, index, &source_authority)
            .await;
        self.providers.current = Some((binding.clone(), index));
        let mut backend = self.providers.take_backend(index)?;
        self.providers.clear_refusal();
        // A panic while the backend is out of its slot must not strand it: the backend returns
        // to the slot (keeping the sessions it already retained reachable for the shutdown reap)
        // and the unwind then continues to the worker's per-job guard.
        let ensured = catch_panic(async {
            if super::fault_seam("ensure") {
                panic!("agent-ide test seam: deliberate provider ensure panic");
            }
            backend.ensure_live(self, job, &launch, source).await
        })
        .await;
        self.providers.put_backend(index, backend);
        job.session_binding = None;
        let ensured = match ensured {
            Ok(ensured) => ensured,
            Err(payload) => std::panic::resume_unwind(payload),
        };
        if let Err(code) = &ensured {
            self.providers
                .name_bare_failure(job, server, "session could not start", code);
            if *code == FailureCode::ProviderUnavailable {
                self.providers.note_session_fault();
            }
        }
        ensured?;
        let budget = Duration::from_millis(100).min(
            job.deadline
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        let readiness = {
            let live = self.providers.slots[index]
                .backend
                .as_mut()
                .and_then(|backend| backend.live_session(&binding))
                .ok_or(FailureCode::Internal)?;
            tokio::select! {
                readiness = live.wait_ready(budget) => readiness,
                _ = job.cancel.changed() => return Err(FailureCode::Cancelled),
            }
        };
        match readiness {
            Ok(()) => {}
            Err(ReadinessError::Loading) => {
                park_while_loading(job);
                return Err(FailureCode::ProviderLoading);
            }
            Err(ReadinessError::WorkspaceError) => {
                self.providers.note_session_fault();
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!(
                        "{}: workspace load failed{}",
                        server.name(),
                        session_fallback_clause(
                            server.language().support().outline_while_loading()
                        )
                    ),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
            Err(ReadinessError::Gone) => {
                let cancelled = *job.cancel.borrow();
                let mut backend = self.providers.take_backend(index)?;
                backend.release_live(self, &binding).await;
                self.providers.put_backend(index, backend);
                if cancelled {
                    return Err(FailureCode::Cancelled);
                }
                self.providers.note_session_fault();
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!(
                        "{}: transport gone{}",
                        server.name(),
                        session_fallback_clause(
                            server.language().support().outline_while_loading()
                        )
                    ),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
        }
        let live = self.providers.slots[index]
            .backend
            .as_mut()
            .and_then(|backend| backend.live_session(&binding))
            .ok_or(FailureCode::Internal)?;
        live.session
            .authorize_source(&source_authority, source)
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        self.providers.note_session_healthy();
        let live = self.providers.slots[index]
            .backend
            .as_mut()
            .and_then(|backend| backend.live_session(&binding))
            .ok_or(FailureCode::Internal)?;
        Ok(live)
    }

    /// Returns the language server whose live session answers `path`, for capability checks.
    pub(super) fn session_server(&self, path: &Path) -> Option<&'static dyn LanguageServer> {
        self.providers.session_server(path)
    }

    /// Names the binding whose live sessions and cache namespace serve `binding`'s provider
    /// calls, or `None` when `binding` serves itself.
    ///
    /// Only a reader borrows. While a writer holds the worktree it owns every session and the
    /// namespace; with no writer, the first reader that needed the provider owns them
    /// ([`Worker::resolve_session_owner`]) and the other readers of that worktree borrow from it.
    /// A writer, an unknown binding and a reader nobody owns for yet serve themselves.
    fn session_owner_of(&self, binding: &BindingRef) -> Option<BindingRef> {
        use crate::workspace::authority::StartRole;
        let receipt = self
            .grants
            .get(binding)
            .filter(|receipt| receipt.role() == StartRole::Reader)?;
        let same_tree = |other: &StartReceipt| other.worktree().id() == receipt.worktree().id();
        self.grants
            .iter()
            .find(|(_, other)| other.role() == StartRole::Writer && same_tree(other))
            .or_else(|| {
                self.grants.iter().find(|(other, grant)| {
                    *other != binding
                        && same_tree(grant)
                        && self.providers.binding_caches.contains_key(*other)
                })
            })
            .map(|(owner, _)| owner.clone())
    }

    /// Points `job` at the binding that serves its provider calls and makes sure that binding
    /// owns the worktree's cache namespace, so a writer-less reader gets the same semantic tools
    /// as a writer (QW-1).
    ///
    /// `job.session_binding` is set to the borrowed owner or left `None` when the invoking binding
    /// serves itself; callers clear it once their provider call ends. A reader with neither a
    /// writer nor a reading owner beside it claims the namespace now, retaining it exactly like a
    /// writer's start would; a writer's later start releases it again
    /// ([`Worker::release_reader_owners`]) and the next reader call claims it back after the
    /// writer leaves. Fails `ProviderUnavailable` with a named stage when the claim is refused.
    pub(super) async fn resolve_session_owner(
        &mut self,
        job: &mut Job,
        authority: &AuthorityStamp,
    ) -> Result<(), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        job.session_binding = self.session_owner_of(&binding);
        let reader = self.grants.get(&binding).is_some_and(|receipt| {
            receipt.role() == crate::workspace::authority::StartRole::Reader
        });
        if job.session_binding.is_some() || !reader {
            return Ok(());
        }
        let launches = job.target.providers.clone();
        self.retain_worktree_caches(&binding, authority, &launches, true)
            .map_err(|code| {
                let stage = match code {
                    FailureCode::Conflict => "provider: cache namespace held by another session",
                    FailureCode::Capacity => "provider: cache namespaces exhausted",
                    _ => "provider: cache namespace unavailable",
                };
                job.set_stage_failure(&FailureCode::ProviderUnavailable, stage);
                FailureCode::ProviderUnavailable
            })
    }

    /// Reads what a session of the server in slot `index` would run against now.
    fn session_basis(&self, index: usize, authority: &AuthorityStamp) -> SessionBasis {
        let worktree = authority.worktree();
        SessionBasis {
            head: crate::workspace::git::head::HeadState::read(
                worktree.worktree_path(),
                worktree.git_common_dir(),
            ),
            inputs: project_inputs_stamp(
                worktree.worktree_path(),
                self.providers.slots[index].server.project_inputs(),
            ),
        }
    }

    /// Retires `owner`'s failed session of slot `index` when its project inputs changed or Git's
    /// `HEAD` moved since it failed, so the call that follows starts a fresh one.
    ///
    /// A session that is not marked failed, and one whose basis is unchanged, is left alone: an
    /// invalid project is never restarted on a timer or per call. The mark is dropped with the
    /// session; a session that fails again is marked afresh against the new basis.
    async fn retire_changed_session(
        &mut self,
        owner: &BindingRef,
        index: usize,
        authority: &AuthorityStamp,
    ) {
        let key = (owner.clone(), index);
        let Some(basis) = self.providers.failed.get(&key) else {
            return;
        };
        if *basis == self.session_basis(index, authority) {
            return;
        }
        self.providers.failed.remove(&key);
        if let Ok(mut backend) = self.providers.take_backend(index) {
            backend.release_live(self, owner).await;
            self.providers.put_backend(index, backend);
        }
    }

    /// Settles the health of the session the finished job used, if any, from what its provider
    /// calls observed ([`SessionHealth`]), not from the tool's final result: a failed provider
    /// call marks the session failed against the basis it failed on even when the tool then
    /// answered from a source outline, and a job whose provider calls all completed clears the mark.
    pub(super) async fn settle_session_health(&mut self, job: &Job) {
        self.providers.current = None;
        let health = std::mem::take(&mut self.providers.health);
        if health.is_empty() {
            return;
        }
        let authority = self.authority(job.invocation.binding_ref()).await.ok();
        for (key, health) in health {
            match health {
                SessionHealth::Healthy => {
                    self.providers.failed.remove(&key);
                }
                SessionHealth::Failed => {
                    if let Some(authority) = &authority {
                        let basis = self.session_basis(key.1, authority);
                        self.providers.failed.insert(key, basis);
                    }
                }
            }
        }
    }

    /// Releases every reader-owned session and namespace of the worktree `writer` is about to
    /// own, so the writer's retention never meets a second live owner.
    ///
    /// Each reader owner's live sessions are shut down and reaped, its non-session provider views
    /// closed and its namespace quiesced, in that order, before this returns; the readers keep
    /// working and borrow the writer's sessions from their next call on. `writer_was_reader` is
    /// true when the incoming writer upgrades its own reader activation, which then releases its
    /// own reader-owned state too; a retry after a failed upgrade is recognised by the recorded
    /// debt, not by the role (the grant already says `Writer`), so the failed cleanup is attempted
    /// again. Fails with the first cleanup failure; the namespace of a reader whose cleanup failed
    /// stays non-quiescent, so the writer's start refuses instead of sharing it.
    pub(super) async fn release_reader_owners(
        &mut self,
        writer: &BindingRef,
        writer_was_reader: bool,
        authority: &AuthorityStamp,
    ) -> Result<(), FailureCode> {
        use crate::workspace::authority::StartRole;
        let upgrading = writer_was_reader || self.providers.pending_handover.contains(writer);
        let owners: Vec<BindingRef> = self
            .providers
            .binding_caches
            .keys()
            .filter(|owner| {
                if *owner == writer {
                    return upgrading;
                }
                self.grants.get(*owner).is_some_and(|grant| {
                    grant.role() == StartRole::Reader
                        && grant.worktree().id() == authority.worktree().id()
                })
            })
            .cloned()
            .collect();
        for owner in owners {
            self.release_live(&owner).await;
            if let Err(code) = self.close_provider(&owner).await {
                if owner == *writer {
                    self.providers.pending_handover.insert(writer.clone());
                }
                return Err(code);
            }
            self.quiesce_worktree_caches(&owner);
        }
        self.providers.pending_handover.remove(writer);
        Ok(())
    }

    /// Shuts down and reaps every live language session owned by one binding, in server order.
    pub(super) async fn release_live(&mut self, binding: &BindingRef) {
        // A released session has nothing left to retire; its failed mark would only outlive it.
        self.providers
            .failed
            .retain(|(owner, _), _| owner != binding);
        for index in 0..self.providers.slots.len() {
            let Ok(mut backend) = self.providers.take_backend(index) else {
                continue;
            };
            backend.release_live(self, binding).await;
            self.providers.put_backend(index, backend);
        }
    }

    /// Shuts down and reaps all retained language sessions during worker shutdown.
    pub(super) async fn release_all_live(&mut self) {
        let bindings = self
            .providers
            .slots
            .iter()
            .filter_map(|slot| slot.backend.as_ref())
            .flat_map(|backend| backend.live_bindings())
            .collect::<std::collections::BTreeSet<_>>();
        for binding in bindings {
            self.release_live(&binding).await;
        }
    }

    /// Releases the stopped actor's non-session provider resources (logical views on shared
    /// listeners) in server order; compatible peers retain their listener and cache identity.
    ///
    /// Every backend is attempted; the first failure is returned. A failure means cleanup could
    /// not be proven and the binding is recorded as uncertain by the backend.
    pub(super) async fn close_provider(&mut self, binding: &BindingRef) -> Result<(), FailureCode> {
        let mut failure = None;
        for index in 0..self.providers.slots.len() {
            let Ok(mut backend) = self.providers.take_backend(index) else {
                continue;
            };
            let result = backend.close_binding(self, binding).await;
            self.providers.put_backend(index, backend);
            if let Err(code) = result {
                failure.get_or_insert(code);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Remembers why a namespace lookup refused, for [`Providers::name_bare_failure`].
    fn record_refusal(&self, cause: &'static str) {
        *self
            .providers
            .refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cause);
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
        let worktree_state = cache_state(authority.worktree());
        self.retained_cache_namespace(
            binding,
            &provider_cache_key(
                &worktree_state,
                launch,
                launch.server().cache_settings(),
                launch.server().effective_configuration(),
                trust,
            ),
        )
    }

    /// Resolves the already-retained *shared* native namespace backing every worktree's shared
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
                launch.server().cache_settings(),
                launch.server().effective_configuration(),
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
            self.record_refusal("cache namespace not owned by this session");
            return Err(FailureCode::ProviderUnavailable);
        }
        self.providers
            .caches
            .get(key)
            .and_then(CacheLifecycle::namespace_path)
            .and_then(|path| path.to_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                self.record_refusal("cache namespace unavailable");
                FailureCode::ProviderUnavailable
            })
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

/// Names the stage of a provider refusal that is a real "no language server for this file type"
/// answer: `<tool>:provider_unavailable ext=<extension> (provider: no server for this file type)`.
///
/// The leading `ext=` shape is what the reply template renders as `no language server is
/// configured for .<extension> files`; the trailing parenthesised stage keeps the journal line and
/// the structured detail uniform with every other provider refusal. Only a path with no accepted
/// server may use it; every other `ProviderUnavailable` carries its own parenthesised stage.
fn set_no_server_stage(job: &mut Job) {
    let shape = crate::assistance::facade::staged_detail(
        job.tool,
        &FailureCode::ProviderUnavailable,
        &job.parameters,
    );
    job.failure_detail = Some(format!("{shape} (provider: no server for this file type)"));
}

/// Deepest directory level below the worktree root that [`project_inputs_stamp`] searches.
const INPUT_SCAN_DEPTH: usize = 4;
/// Most directory entries [`project_inputs_stamp`] reads in total; a larger tree is stamped from
/// the entries read so far.
const INPUT_SCAN_ENTRIES: usize = 5_000;
/// Directories [`project_inputs_stamp`] never enters: VCS metadata and generated or vendored trees
/// that hold copies of manifests the project does not own.
const INPUT_SCAN_SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    "build",
    "vendor",
];

/// Most bytes of one input file that [`project_inputs_stamp`] digests; a longer file is stamped
/// by its length and this prefix.
const INPUT_SCAN_FILE_BYTES: u64 = 256 * 1024;

/// Digests the relative path, length, modification time and first [`INPUT_SCAN_FILE_BYTES`] bytes
/// of every file named in `names` below `root`.
///
/// A file of up to that size is digested whole, so any edit of it changes the stamp. A longer file
/// is covered by its length, its modification time and its prefix: an ordinary edit moves the
/// modification time, but a same-length edit past the prefix that also restores the timestamp is
/// not seen (a stated ceiling; the next `HEAD` move or session restart recovers it).
///
/// Coverage is bounded and honest: at most [`INPUT_SCAN_DEPTH`] levels below `root` and
/// [`INPUT_SCAN_ENTRIES`] directory entries read in total (entries are streamed, never collected,
/// so a huge directory costs at most the remaining budget); a manifest deeper than that or past the
/// budget is not seen, and only a moved `HEAD` then revives a failed session. The per-file digests
/// are summed, so the result does not depend on the order the filesystem lists entries. An
/// unreadable directory or file contributes nothing; empty `names` digest to a constant. Only
/// regular files are opened: a named pipe, socket, device or symlink named like an input is skipped
/// (opening a pipe would block the worker), so a symlinked manifest is not stamped.
fn project_inputs_stamp(root: &Path, names: &[&str]) -> [u8; 32] {
    let mut sum = [0_u8; 32];
    let mut budget = INPUT_SCAN_ENTRIES;
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                return sum;
            }
            budget -= 1;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < INPUT_SCAN_DEPTH && !INPUT_SCAN_SKIP.contains(&name) {
                    pending.push((entry.path(), depth + 1));
                }
            } else if kind.is_file()
                && names.contains(&name)
                && let Ok(file) = std::fs::File::open(entry.path())
            {
                use std::io::Read;
                let relative = entry.path();
                let relative = relative.strip_prefix(root).unwrap_or(&relative);
                let mut content = Vec::new();
                if file
                    .take(INPUT_SCAN_FILE_BYTES)
                    .read_to_end(&mut content)
                    .is_err()
                {
                    continue;
                }
                let mut hasher = blake3::Hasher::new();
                hasher.update(relative.as_os_str().as_encoded_bytes());
                let metadata = entry.metadata().ok();
                let modified = metadata
                    .as_ref()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |elapsed| elapsed.as_nanos());
                hasher.update(&metadata.map_or(0, |metadata| metadata.len()).to_le_bytes());
                hasher.update(&modified.to_le_bytes());
                hasher.update(&content);
                for (total, byte) in sum.iter_mut().zip(hasher.finalize().as_bytes()) {
                    *total = total.wrapping_add(*byte);
                }
            }
        }
    }
    sum
}

/// Parks `job` for a retry in 300 ms because its language server is still loading; the caller
/// then answers `provider_loading`, which the worker turns into a requeue instead of a reply.
/// An edit is never parked (post-edit provider data is optional, and a write whose receipt must
/// settle is not restarted), nor a job whose deadline is a second away or less: both answer
/// `provider_loading` at once.
pub(super) fn park_while_loading(job: &mut Job) {
    let now = tokio::time::Instant::now();
    if job.tool != AssistanceTool::Edit
        && job.deadline.saturating_duration_since(now) > Duration::from_secs(1)
    {
        job.park_until = Some(now + Duration::from_millis(300));
    }
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
    // A per-worktree namespace names its worktree so retention can tell when it is gone and claim
    // it through the worktree's lease. Without the marker the namespace is merely protected by the
    // machine-wide lease (retention keeps it while any lease is held), so a failure is harmless.
    for request in plan.iter().filter(|request| !request.shared) {
        if let Some(path) = caches
            .get(&request.key)
            .and_then(CacheLifecycle::namespace_path)
        {
            let _ = crate::retention::publish_namespace_marker(path, worktree.worktree_path());
        }
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

/// Fixed compatibility-identity marker used only by the shared native namespace.
///
/// It deliberately excludes any worktree so every divergent worktree with a compatible
/// executable/settings/toolchain/effective-rights identity resolves to the same shared key and,
/// through it, to the same one heavy shared listener.
pub(super) const SHARED_NATIVE_CACHE_STATE: &str = "shared-native-v1";

/// Names the worktree a provider namespace belongs to in a way that survives a daemon restart.
///
/// The durable `WorktreeRef::id()` is bound to the Application database's nonce, which a new
/// daemon mints afresh, so it cannot name a namespace a restart should adopt. The canonical path
/// and the directory's inode and creation time can: the same directory yields the same state in
/// every boot, while a deleted and recreated one (new creation time) or another path yields a new
/// one and never reaches the old namespace. The device number is left out because it is not
/// stable across reboots. A reference whose directory cannot be inspected keeps the boot-local
/// identity, so its namespace is never adopted by anyone else.
fn cache_state(worktree: &crate::workspace::authority::WorktreeRef) -> String {
    use std::os::unix::{ffi::OsStrExt as _, fs::MetadataExt as _};
    let path = worktree.worktree_path();
    let created = std::fs::symlink_metadata(path).ok().and_then(|metadata| {
        let created = metadata
            .created()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        (metadata.is_dir() && !created.is_zero()).then_some((metadata.ino(), created))
    });
    let Some((inode, created)) = created else {
        return format!("{}:{}", worktree.id(), worktree.incarnation());
    };
    let mut hash = blake3::Hasher::new();
    hash.update(b"provider-cache-state-v1");
    hash.update(path.as_os_str().as_bytes());
    hash.update(&inode.to_le_bytes());
    hash.update(&created.as_secs().to_le_bytes());
    hash.update(&created.subsec_nanos().to_le_bytes());
    format!("tree-{}", hash.finalize().to_hex())
}

/// Fingerprints every program a launch runs: the server executable and the language's own
/// toolchain executables (compilers, interpreters), each by path, identity and measured BLAKE3.
/// A selector such as `stable` or an unchanged identity label cannot hide a replaced binary.
fn programs(launch: &ProviderLaunch) -> String {
    std::iter::once(&launch.executable)
        .chain(launch.server().launch_executables(launch))
        .map(|program| {
            format!(
                "{}\0{}\0{}",
                program.path.display(),
                program.identity,
                program.blake3
            )
        })
        .collect::<Vec<_>>()
        .join("\u{1}")
}

/// Derives one opaque namespace component from durable worktree and accepted provider identities.
///
/// Every input that makes an existing native cache unsafe to reuse is hashed: the accepted
/// executables (server and toolchain: path, identity and measured digest), settings, effective initialization configuration, toolchain and effective trust.
/// A restarted daemon whose launch declaration changed in any of them computes a different key,
/// finds no namespace and starts cold; the old one ages out under the retention rules.
fn provider_cache_key(
    worktree_state: &str,
    launch: &ProviderLaunch,
    settings: &str,
    configuration: &str,
    trust: &str,
) -> String {
    blake3::hash(
        format!(
            "{}\0{}\0{}\0{}\0{}\0{}\0{}",
            worktree_state,
            launch.cache_namespace,
            programs(launch),
            settings,
            configuration,
            launch.toolchain,
            trust
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

impl ProviderHost for Worker<'_> {
    /// Delegates to the worker's boot-fenced authority check.
    fn authority<'a>(
        &'a self,
        binding: &'a BindingRef,
    ) -> crate::checks::BoxFuture<'a, Result<AuthorityStamp, FailureCode>> {
        Box::pin(Worker::authority(self, binding))
    }

    /// Delegates to [`Worker::provider_cache_namespace`].
    fn cache_namespace(
        &self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launch: &ProviderLaunch,
        trust: &str,
    ) -> Result<String, FailureCode> {
        self.provider_cache_namespace(binding, authority, launch, trust)
    }

    /// Delegates to [`Worker::provider_shared_cache_namespace`].
    fn shared_cache_namespace(
        &self,
        binding: &BindingRef,
        launch: &ProviderLaunch,
        trust: &str,
    ) -> Result<String, FailureCode> {
        self.provider_shared_cache_namespace(binding, launch, trust)
    }

    /// Delegates to the worker's execution-request validation for the job's binding.
    fn execution_request<'a>(
        &'a self,
        job: &'a dyn ProviderJob,
        authority: &'a AuthorityStamp,
        command: ControlledCommand,
        program: &'a crate::assistance::launcher::AcceptedExecutable,
    ) -> crate::checks::BoxFuture<'a, Result<ValidatedExecutionRequest, FailureCode>> {
        Box::pin(Worker::execution_request(
            self, job, authority, command, program,
        ))
    }

    /// Consumes one transient active use from the shared binding table.
    fn active(&self, binding: &BindingRef) -> Result<ActiveBindingUse, FailureCode> {
        self.shared.active(binding)
    }

    /// Shares the daemon's single admission controller handle.
    fn admission(&self) -> Arc<Mutex<crate::execution::AdmissionController>> {
        self.admission.clone()
    }

    /// Returns the worker's provider lease registry.
    fn registry(&mut self) -> &mut ProviderLeaseRegistry {
        &mut self.providers.registry
    }

    /// Mints the next provider generation.
    fn next_generation(&mut self) -> Result<u64, FailureCode> {
        self.providers.next()
    }

    /// Settles only a capability-backed no-child result; uncertain provider failures remain reserved.
    fn spawn_failure(&mut self, error: ProcessError, binding: &BindingRef) {
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

    /// Records the binding in the worker's uncertain set.
    fn mark_uncertain(&mut self, binding: &BindingRef) {
        self.uncertain.insert(binding.clone());
    }

    /// Delegates to the worker's non-waiting physical admission.
    fn admit(
        &mut self,
        binding: &BindingRef,
    ) -> Result<crate::execution::AdmissionLease, FailureCode> {
        Worker::admit(self, binding)
    }

    /// The launcher's per-stream output bound.
    fn output_bytes(&self) -> usize {
        self.shared.launcher.limits.output_bytes
    }

    /// The launcher's allowed roots.
    fn allowed_roots(&self) -> Vec<std::path::PathBuf> {
        self.shared.launcher.allowed_roots().to_vec()
    }

    /// The worker's telemetry sink.
    fn telemetry(&self) -> Option<&Telemetry> {
        self.telemetry.as_ref()
    }

    /// The worker's private runtime namespace.
    fn runtime_dir(&self) -> &Path {
        &self.runtime
    }

    /// The daemon boot nonce.
    fn nonce(&self) -> [u8; 32] {
        self.shared.nonce
    }
}

impl ProviderJob for Job {
    /// The binding that owns this job's provider sessions: the invocation binding, or the
    /// writer's while a reader's job borrows its sessions (`session_binding`, set only for the
    /// duration of one provider call). Backends key sessions, caches, and spawn authority on it.
    fn binding(&self) -> &BindingRef {
        self.session_binding
            .as_ref()
            .unwrap_or(self.invocation.binding_ref())
    }

    /// The job's revocation channel.
    fn cancel(&mut self) -> &mut watch::Receiver<bool> {
        &mut self.cancel
    }

    /// Whether revocation has been signalled.
    fn cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// The job's absolute deadline.
    fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    /// Whether the job is an edit.
    fn is_edit(&self) -> bool {
        self.tool == AssistanceTool::Edit
    }

    /// Records the earliest retry time.
    fn park_until(&mut self, at: tokio::time::Instant) {
        self.park_until = Some(at);
    }

    /// Records the failure detail for the reply.
    fn set_failure_detail(&mut self, detail: String) {
        self.failure_detail = Some(detail);
    }

    /// Records the failed session stage for the reply: the default `<tool>:<reason>` tag
    /// composed with the backend-reported stage.
    fn set_stage_failure(&mut self, code: &FailureCode, stage: &str) {
        self.failure_detail = Some(crate::telemetry::adapters::stage_with_failure(
            self.tool, code, stage,
        ));
    }
}

#[cfg(test)]
#[path = "providers_tests.rs"]
mod providers_tests;
