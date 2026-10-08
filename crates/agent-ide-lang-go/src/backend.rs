//! Worker-side gopls integration: one shared heavy listener per compatible profile, one logical
//! view per binding, and one short-lived stdio forwarder per request.
//!
//! Divergent worktrees with a compatible executable/settings/toolchain/trust identity share one
//! listener (and its shared native cache namespace); each binding keeps an isolated logical view
//! with its own private Go build namespace. The listener is stopped and its socket disposed only
//! when the last view is released.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use agent_ide_core::{
    assistance::{host_binding::BindingRef, launcher::ProviderLaunch, reply::FailureCode},
    checks::BoxFuture,
    execution::{
        AdmissionClass, AdmissionError, BackendRelease, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderLeaseError, ProviderLeaseRegistry, ProviderViewLease,
    },
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{ProviderSettings, SessionOptions, with_session},
    },
    lang::Language,
    telemetry::{CacheState, DiagnosticState, Telemetry, adapters},
    workspace::observation::SourceObservation,
};

use crate::profile::{GoEnv, GoplsProfile, SharedGopls};

/// The shared-listener gopls server integration.
pub struct GoplsServer;

impl LanguageServer for GoplsServer {
    /// The Go language.
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    /// gopls defaults.
    fn settings_key(&self) -> &'static str {
        "gopls_defaults"
    }

    /// Requires an absolute Go toolchain executable as the declared toolchain.
    fn validate_launch(&self, launch: &ProviderLaunch) -> bool {
        agent_ide_core::assistance::launcher::absolute(Path::new(&launch.toolchain))
    }

    /// The server's own name.
    fn name(&self) -> &'static str {
        "gopls"
    }

    /// gopls defaults, versioned.
    fn cache_settings(&self) -> &'static str {
        "gopls-defaults-v1"
    }

    /// gopls defaults, versioned.
    fn effective_configuration(&self) -> &'static str {
        "gopls-defaults-v1"
    }

    /// Private Go build, module and temporary directories plus the view's gopls state.
    fn cache_directories(&self) -> &'static [&'static str] {
        &["go-build", "go-mod", "gopls", "tmp"]
    }

    /// The listener's own cache and temporary directory in the shared native namespace.
    fn shared_cache_directories(&self) -> Option<&'static [&'static str]> {
        Some(&["gopls", "tmp"])
    }

    /// `.go` sources.
    fn context_extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    /// The Go module and workspace files the server loads.
    fn project_inputs(&self) -> &'static [&'static str] {
        &["go.mod", "go.sum", "go.work"]
    }

    /// Starts with no listeners and no views.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(GoplsBackend::default())
    }
}

/// Compatible shared listeners, each binding's logical view, and socket path generations.
#[derive(Default)]
struct GoplsBackend {
    /// Compatible heavy gopls listeners keyed by backend compatibility key, counted once across
    /// worktree views.
    listeners: BTreeMap<String, GoBackend>,
    /// One logical Go view for each bound actor/worktree.
    views: BTreeMap<BindingRef, GoLease>,
    /// Per-backend socket path generation, bumped whenever a reap could not prove the on-disk
    /// socket's fate; a bumped generation forces the next spawn for that backend onto a fresh path.
    socket_generation: BTreeMap<String, u64>,
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

/// Gives one captured provider socket exactly one disposition: identity-checked removal when the
/// identity was captured, otherwise a bumped entry in `socket_generation` for `backend` so the next
/// spawn cannot adopt whatever, if anything, is left at the deterministic path. Never guess-deletes a
/// replacement. The generation is finite and saturates rather than wrapping back to a reused value.
/// Returns whether the socket was proven removed (or never existed to remove).
fn dispose_socket(
    socket_generation: &mut BTreeMap<String, u64>,
    backend: &str,
    socket: Option<OwnedProviderSocket>,
) -> bool {
    if let Some(identity) = socket
        && identity.remove().is_ok()
    {
        return true;
    }
    let generation = socket_generation.entry(backend.to_string()).or_insert(0);
    *generation = generation.saturating_add(1);
    false
}

impl GoplsBackend {
    /// Reuses one compatible listener but gives each request its own independently accounted forwarder.
    async fn answer(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let generation = host.next_generation()?;
        let binding = job.binding().clone();
        let authority = host.authority(&binding).await?;
        let trust = server::effective_trust(launch);
        let worktree_namespace = host.cache_namespace(&binding, &authority, launch, &trust)?;
        let shared_namespace = host.shared_cache_namespace(&binding, launch, &trust)?;
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
        let socket_generation = self.socket_generation.get(&backend).copied().unwrap_or(0);
        let socket_key = blake3::hash(
            format!(
                "{}{}{}",
                blake3::Hash::from_bytes(host.nonce()).to_hex(),
                backend,
                socket_generation
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        let socket = host
            .runtime_dir()
            .join(format!("g-{}.sock", &socket_key[..16]));
        let scoped = server::execution_authority(&authority)?;
        let logical = if let Some(view) = self.views.get(&binding).cloned() {
            if view.backend != backend {
                return Err(FailureCode::ExecutionProfile);
            }
            view
        } else {
            let command = if self.listeners.contains_key(&backend) {
                profile.forwarder_command(&scoped, &socket)
            } else {
                profile.listener_command(&scoped, &socket)
            }
            .map_err(|_| FailureCode::ExecutionProfile)?;
            let request = host
                .execution_request(&*job, &authority, command, &launch.executable)
                .await?;
            let active = host.active(&binding)?;
            let output_bytes = host.output_bytes();
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let lease = match host.registry().request(
                &mut admission,
                server::owner(&binding)?,
                AdmissionClass::Interactive,
                backend.clone(),
                ProviderBackendKind::OwnedShared,
                request.authority(),
            ) {
                ProviderLeaseAdmission::Granted(lease) => lease,
                ProviderLeaseAdmission::Queued(ticket) => {
                    host.registry().cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            };
            if !self.listeners.contains_key(&backend) {
                let capability = host
                    .registry()
                    .take_spawn_lease(lease)
                    .map_err(|_| FailureCode::Internal)?;
                let listener = match SharedGopls::start(
                    &profile,
                    &request,
                    capability,
                    Some(active),
                    output_bytes,
                ) {
                    Ok(listener) => listener,
                    Err(error) => {
                        host.spawn_failure(error, &binding);
                        return Err(FailureCode::ProviderUnavailable);
                    }
                };
                self.listeners.insert(
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
            self.views.insert(binding.clone(), view.clone());
            view
        };
        loop {
            let already_captured = self
                .listeners
                .get(&backend)
                .ok_or(FailureCode::Internal)?
                .socket
                .is_some();
            if already_captured {
                break;
            }
            match OwnedProviderSocket::capture(socket.clone()) {
                Ok(identity) => {
                    self.listeners
                        .get_mut(&backend)
                        .ok_or(FailureCode::Internal)?
                        .socket = Some(identity);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    let _ = self.close(host, &binding).await;
                    return Err(FailureCode::Internal);
                }
            }
            // A listener that has already exited cannot make further progress toward binding on
            // its own; settle this request immediately instead of polling it all the way to its
            // full operation deadline. This only shortens how long *this* request waits; it does
            // not change the established, unmodified socket cleanup this triggers via
            // `close`/`reap_owned_backend` below.
            let listener_exited = self
                .listeners
                .get_mut(&backend)
                .ok_or(FailureCode::Internal)?
                .shared
                .listener_exit_status()
                .ok()
                .flatten()
                .is_some();
            let failure = if listener_exited {
                Some(FailureCode::ProviderUnavailable)
            } else if job.cancelled() || host.active(&binding).is_err() {
                Some(FailureCode::Cancelled)
            } else if tokio::time::Instant::now() >= job.deadline() {
                Some(FailureCode::Deadline)
            } else {
                None
            };
            if let Some(code) = failure {
                let _ = self.close(host, &binding).await;
                return Err(code);
            }
            let cancelled = tokio::select! {_=tokio::time::sleep(Duration::from_millis(10))=>false,_=job.cancel().changed()=>true};
            if cancelled {
                let _ = self.close(host, &binding).await;
                return Err(FailureCode::Cancelled);
            }
        }
        let request = host
            .execution_request(
                &*job,
                &authority,
                profile
                    .forwarder_command(&scoped, &socket)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                &launch.executable,
            )
            .await?;
        let active = host.active(&binding)?;
        let lease = host.admit(&binding)?;
        let output_bytes = host.output_bytes();
        // The shared controller guard is confined to this block: it is a `std` mutex the helper
        // socket task also locks, so it must never reach the awaits below or this worker future
        // stops being `Send`.
        let capability = {
            let controller = host.admission();
            let mut controller = controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match host.registry().take_forwarder_spawn_lease(
                &mut controller,
                logical.lease,
                &request,
                lease,
            ) {
                Ok(capability) => capability,
                Err((error, unused)) => {
                    controller
                        .release(unused)
                        .map_err(|_| FailureCode::Internal)?;
                    // A binding whose servers already fill their share of its slots is refused
                    // like any other provider request beyond that share.
                    return Err(
                        if error
                            == ProviderLeaseError::Admission(AdmissionError::OwnerProviderLimit)
                        {
                            FailureCode::ProviderUnavailable
                        } else {
                            FailureCode::Internal
                        },
                    );
                }
            }
        };
        let backend_state = self
            .listeners
            .get_mut(&backend)
            .ok_or(FailureCode::Internal)?;
        let view = match backend_state.shared.open_view(
            authority.worktree().clone(),
            source.sequence(),
            &request,
            capability,
            Some(active),
            output_bytes,
        ) {
            Ok(view) => view,
            Err(error) => {
                host.spawn_failure(error, &binding);
                return Err(FailureCode::ProviderUnavailable);
            }
        };
        let backend_generation = backend_state.generation;
        let mut child = view.into_child();
        let result = {
            let telemetry = host.telemetry().cloned();
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
                ProviderSettings::new(go_env),
                server::remaining_options(&*job),
                telemetry.as_ref(),
            );
            tokio::pin!(operation);
            tokio::select! {result=&mut operation=>result,_=job.cancel().changed()=>Err(FailureCode::Cancelled)}
        };
        let reaped = match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => reaped,
            Err(_) => {
                host.mark_uncertain(&binding);
                return Err(FailureCode::Deadline);
            }
        };
        let admission = host.admission();
        let mut admission = admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        host.registry()
            .complete_forwarder_reap(&mut admission, reaped.proof)
            .map_err(|_| FailureCode::Internal)?;
        self.listeners
            .get_mut(&backend)
            .ok_or(FailureCode::Internal)?
            .shared
            .release_view(authority.worktree(), logical.lease)
            .map_err(|_| FailureCode::Internal)?;
        host.active(&binding)?;
        result
    }

    /// Releases the stopped actor's logical view; compatible peers retain their listener and cache identity.
    ///
    /// Every step below the removed `go_views` mapping is the only remaining reference to that state,
    /// so any failure past this point is recorded in `uncertain` rather than silently discarded: the
    /// binding's view and backend accounting are gone either way, and losing the failure signal would
    /// let a caller believe cleanup fully succeeded when it did not. When this release makes the worker
    /// the backend's sole owner, the entire remaining disposition is delegated to `reap_owned_backend`.
    async fn close(
        &mut self,
        host: &mut dyn ProviderHost,
        binding: &BindingRef,
    ) -> Result<(), FailureCode> {
        let Some(view) = self.views.remove(binding) else {
            return Ok(());
        };
        let release = match host.registry().release(view.lease) {
            Ok(release) => release,
            Err(_) => {
                host.mark_uncertain(binding);
                return Err(FailureCode::Internal);
            }
        };
        if let BackendRelease::ReapOwned(capability) = release {
            let backend = match self.listeners.remove(&view.backend) {
                Some(backend) => backend,
                None => {
                    host.mark_uncertain(binding);
                    return Err(FailureCode::Internal);
                }
            };
            let admission = host.admission();
            let mut uncertain = BTreeSet::new();
            let result = reap_owned_backend(
                &mut self.socket_generation,
                host.registry(),
                &admission,
                &mut uncertain,
                binding,
                &view.backend,
                backend,
                capability,
            )
            .await;
            for binding in &uncertain {
                host.mark_uncertain(binding);
            }
            result?;
        }
        Ok(())
    }
}

impl ServerBackend for GoplsBackend {
    /// Serves one request through a fresh forwarder on the binding's view (see
    /// [`GoplsBackend::answer`]).
    fn context<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
        bytes: &'a [u8],
        query: ContextQuery,
    ) -> BoxFuture<'a, Result<ProviderContext, FailureCode>> {
        Box::pin(self.answer(host, job, launch, source, bytes, query))
    }

    /// Releases the binding's logical view (see [`GoplsBackend::close`]).
    fn close_binding<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        Box::pin(self.close(host, binding))
    }

    /// Bindings holding a logical view.
    fn bound_bindings(&self) -> Vec<BindingRef> {
        self.views.keys().cloned().collect()
    }
}

/// Stops and reaps a backend this worker just became the sole owner of, giving its captured socket
/// (if any) exactly one disposition on every exit: identity-checked removal, or a bumped
/// `socket_generation` so the next spawn for `view_backend` cannot adopt a stale path. This is the
/// sole production route `GoplsBackend::close` uses for its `BackendRelease::ReapOwned` case; a `stop` or
/// `complete_reap` failure still records `uncertain` and still disposes the socket before returning.
///
/// `socket_generation` is the per-backend endpoint generation map [`dispose_socket`] advances, and
/// `registry` completes the reap; both are mutated in place; `admission` is the physical-effect controller `complete_reap` releases the settled
/// reservation into. `uncertain` receives `binding` on every error exit — physical/registry
/// completion could not be proven, so the caller (`GoplsBackend::close`) must not report success even
/// though the view/backend accounting is already gone either way. `binding` identifies the actor
/// solely for that uncertainty bookkeeping; it plays no role in `backend`'s own identity. `view_backend`
/// is the backend compatibility key already removed from the listener map, used only to key
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
#[allow(clippy::too_many_arguments)]
async fn reap_owned_backend(
    socket_generation: &mut BTreeMap<String, u64>,
    registry: &mut ProviderLeaseRegistry,
    admission: &std::sync::Mutex<agent_ide_core::execution::AdmissionController>,
    uncertain: &mut std::collections::BTreeSet<BindingRef>,
    binding: &BindingRef,
    view_backend: &str,
    backend: GoBackend,
    capability: agent_ide_core::execution::BackendReapCapability,
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
            dispose_socket(socket_generation, view_backend, socket);
            return Err(FailureCode::Deadline);
        }
    };
    let reaped = registry.complete_reap(
        &mut admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        capability,
        completed.settlement,
    );
    let disposed = dispose_socket(socket_generation, view_backend, socket);
    if reaped.is_err() || !disposed {
        uncertain.insert(binding.clone());
        return Err(FailureCode::Internal);
    }
    Ok(())
}

/// Runs the accepted settings handshake and one exact-source query, snapshots the immediately
/// available bounded diagnostics and shuts down. Transport or protocol failures return
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
            Ok(context) => server::diagnostic_state(context.diagnostics.readiness),
            Err(_) => DiagnosticState::Unavailable,
        };
        adapters::provider_summary(
            telemetry,
            crate::LANGUAGE,
            CacheState::Unavailable,
            diagnostics,
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{GoBackend, OwnedProviderSocket, dispose_socket, reap_owned_backend};
    use crate::profile::{GoplsProfile, SharedGopls};
    use agent_ide_core::assistance::host_binding::{
        BindingRef, BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        parse_hook_event,
    };
    use agent_ide_core::assistance::reply::FailureCode;
    use agent_ide_core::execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLimits, BackendRelease,
        ControlledCommand, LocalExecutionPolicy, OwnerId, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderViewLease, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
    };
    use agent_ide_core::workspace::authority::WorktreeRef;
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The registry and socket-generation state a worker's gopls backend works against, owned
    /// directly by each test instead of by a worker.
    struct Providers {
        /// Central typed backend/view accounting with the worker's fixed limits.
        registry: agent_ide_core::execution::ProviderLeaseRegistry,
        /// Per-backend socket path generations advanced by unproved disposals.
        socket_generation: std::collections::BTreeMap<String, u64>,
    }

    impl Providers {
        /// Creates the worker's fixed finite accounting without launching processes.
        fn new() -> Self {
            Self {
                registry: agent_ide_core::execution::ProviderLeaseRegistry::new(
                    agent_ide_core::execution::ProviderLeaseLimits {
                        total_views: 64,
                        per_backend_views: 64,
                    },
                )
                .expect("fixed view limits"),
                socket_generation: std::collections::BTreeMap::new(),
            }
        }

        /// Disposes `socket` for `backend` exactly as the backend's close path does.
        fn dispose_socket(&mut self, backend: &str, socket: Option<OwnedProviderSocket>) -> bool {
            dispose_socket(&mut self.socket_generation, backend, socket)
        }
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
    /// socket identity, not yet registered in a backend's listener map); the `WorkspaceAuthority` and matching
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
    /// route `GoplsBackend::close` uses for its `BackendRelease::ReapOwned` case. Removing the disposal call
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
            &mut providers.socket_generation,
            &mut providers.registry,
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
            &mut providers.socket_generation,
            &mut providers.registry,
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
            &mut providers.socket_generation,
            &mut providers.registry,
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
