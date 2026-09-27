//! Worker-side rust-analyzer integration: one long-lived analyzer session per binding.
//!
//! The backend owns the exclusive Rust view bookkeeping and every binding's live session. A
//! session starts on the first request, is reused while its transport lives, and is shut down and
//! reaped when the binding stops or an exchange fails.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    assistance::{host_binding::BindingRef, launcher::ProviderLaunch, reply::FailureCode},
    checks::BoxFuture,
    execution::AdmissionClass,
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        rust::{
            RustProfile, RustProfileError, RustProfileIdentity, RustProtocolChild, RustView,
            RustViewAdmission, RustViews, RustWorktree,
        },
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ProviderSettings, ReadinessError},
    },
    telemetry::{DiagnosticState, Language},
    workspace::observation::SourceObservation,
};

/// The rust-analyzer server integration.
pub struct RustServer;

impl LanguageServer for RustServer {
    /// Rust provider observations.
    fn language(&self) -> Language {
        Language::Rust
    }

    /// The analyzer's own name.
    fn name(&self) -> &'static str {
        "rust-analyzer"
    }

    /// Cache priming disabled, versioned.
    fn cache_settings(&self) -> &'static str {
        "rust-cache-priming-disabled-v1"
    }

    /// The configuration identity [`RustProfile`] accepts for managed sessions.
    fn effective_configuration(&self) -> &'static str {
        "cache-priming-disabled-v1"
    }

    /// Cargo home, target directory and temporary files live in the worktree namespace.
    fn cache_directories(&self) -> &'static [&'static str] {
        &["cargo", "target", "tmp"]
    }

    /// `.rs` sources.
    fn context_extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    /// Symbol tools use the live analyzer session for `.rs` sources.
    fn session_extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    /// Starts with no views and no sessions.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(RustBackend::default())
    }
}

/// One binding's live analyzer: the protocol child, its exclusive view and the session driver.
struct RustLive {
    /// Analyzer process owned until reap.
    child: RustProtocolChild,
    /// Exclusive view admitted for this child.
    view: RustView,
    /// Transport driver and synchronized document state.
    live: LiveSession,
}

/// Exclusive Rust generations and every binding's retained analyzer session.
#[derive(Default)]
struct RustBackend {
    /// Exclusive Rust generation and source bookkeeping.
    views: RustViews,
    /// Live sessions, one per binding, kept until stop or transport failure.
    live: BTreeMap<BindingRef, RustLive>,
}

impl RustBackend {
    /// Starts the binding's long-lived Rust session unless its Rust child is still live.
    ///
    /// `job` supplies binding ownership, cancellation, and spawn authority; `launch` is the
    /// accepted analyzer profile; `source` fixes the worktree and authority epoch. Admission,
    /// profile, spawn, initialization, and cancellation failures return a bounded code.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.binding().clone();
        if let Some(entry) = self.live.get(&binding) {
            if entry.live.is_alive() {
                return Ok(());
            }
            self.release(host, &binding).await;
        }
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
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
            configuration: RustServer.effective_configuration().into(),
            trust: launch.trust.clone(),
            transport: "stdio-v1".into(),
            cache_namespace,
        })
        .map_err(|_| FailureCode::ExecutionProfile)?;
        let scoped = server::execution_authority(&authority)?;
        let worktree = RustWorktree::new(authority.worktree().clone(), scoped)
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = profile
            .command(&worktree)
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let request = host
            .execution_request(&*job, &authority, command, &launch.executable)
            .await?;
        let active = host.active(&binding)?;
        // The shared controller guard is confined to this block: it is a `std` mutex, so it must
        // never reach the awaits below or this worker future stops being `Send`.
        let view = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.views.request(
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                server::owner(&binding)?,
                AdmissionClass::Interactive,
            ) {
                RustViewAdmission::Granted(view) => view,
                RustViewAdmission::Queued(ticket) => {
                    host.registry().cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let output_bytes = host.output_bytes();
        let mut child = match RustProtocolChild::spawn(
            &request,
            &worktree,
            host.registry(),
            view.lease(),
            Some(active),
            output_bytes,
        ) {
            Ok(child) => child,
            Err(error) => {
                let _ = self.views.release(host.registry(), view.lease());
                if let RustProfileError::Process(error) = error {
                    host.spawn_failure(error, &binding);
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
                    ProviderSettings::new(profile),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                // Stop or shutdown must be able to interrupt a handshake the server never answers.
                tokio::select! {
                    opened = &mut open => opened,
                    _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")),
                }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.live.insert(binding, RustLive { child, view, live });
                Ok(())
            }
            Err(_) => {
                self.reap(host, &binding, child, view).await;
                if job.cancelled() {
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
    async fn answer(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> Result<ProviderContext, FailureCode> {
        let binding = job.binding().clone();
        self.ensure(host, job, launch, source).await?;
        let budget = Duration::from_millis(100).min(
            job.deadline()
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        let lease = self
            .live
            .get(&binding)
            .map(|entry| entry.view.lease())
            .ok_or(FailureCode::Internal)?;
        self.views
            .observe_source(lease, source.sequence())
            .map_err(|_| FailureCode::Internal)?;
        let outcome = {
            let entry = self.live.get_mut(&binding).ok_or(FailureCode::Internal)?;
            let readiness = tokio::select! {
                readiness = entry.live.wait_ready(budget) => readiness,
                _ = job.cancel().changed() => return Err(FailureCode::Cancelled),
            };
            match readiness {
                Ok(()) => {
                    // A whole-file query is the post-edit diagnostic read: give the analyzer a
                    // few seconds to publish diagnostics for the synchronized version before
                    // snapshotting, so an edit reply can report `current_clean`/`current_reported`
                    // instead of `unknown`.
                    let (result, diagnostics) = server::exchange_context(
                        &mut entry.live,
                        job,
                        source,
                        bytes,
                        query,
                        matches!(query, ContextQuery::File),
                    )
                    .await;
                    result.map(|context| ProviderContext {
                        context,
                        diagnostics,
                    })
                }
                Err(ReadinessError::Loading) => {
                    // An edit may already have changed the worktree; report provider diagnostics
                    // unknown and let its receipt settle instead of restarting that mutation.
                    if !job.is_edit()
                        && job
                            .deadline()
                            .saturating_duration_since(tokio::time::Instant::now())
                            > Duration::from_secs(1)
                    {
                        job.park_until(tokio::time::Instant::now() + Duration::from_millis(300));
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
                self.release(host, &binding).await;
                if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        };
        let diagnostics = match &result {
            Ok(context) => server::diagnostic_state(context.diagnostics.readiness),
            Err(_) => DiagnosticState::Unavailable,
        };
        server::record_provider(&*host, Language::Rust, diagnostics);
        host.active(&binding)?;
        result
    }

    /// Shuts down and reaps `binding`'s Rust session, if any.
    async fn release(&mut self, host: &mut dyn ProviderHost, binding: &BindingRef) {
        if let Some(RustLive { child, view, live }) = self.live.remove(binding) {
            let _ = live.shutdown().await;
            self.reap(host, binding, child, view).await;
        }
    }

    /// Reaps a Rust child and returns its exclusive view to provider accounting.
    async fn reap(
        &mut self,
        host: &mut dyn ProviderHost,
        binding: &BindingRef,
        child: RustProtocolChild,
        view: RustView,
    ) {
        match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => {
                if let Ok(capability) = self.views.release(host.registry(), view.lease()) {
                    let admission = host.admission();
                    let mut admission = admission
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = host
                        .registry()
                        .complete_reap(&mut admission, capability, reaped.proof);
                }
            }
            Err(_) => {
                host.mark_uncertain(binding);
                let _ = self.views.release(host.registry(), view.lease());
            }
        }
    }
}

impl ServerBackend for RustBackend {
    /// Serves context from the binding's analyzer session (see [`RustBackend::answer`]).
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

    /// Starts or keeps the binding's analyzer session (see [`RustBackend::ensure`]).
    fn ensure_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        Box::pin(self.ensure(host, job, launch, source))
    }

    /// Returns the binding's retained analyzer session.
    fn live_session(&mut self, binding: &BindingRef) -> Option<&mut LiveSession> {
        self.live.get_mut(binding).map(|entry| &mut entry.live)
    }

    /// Shuts down and reaps the binding's analyzer session.
    fn release_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.release(host, binding))
    }

    /// Bindings with a retained analyzer session.
    fn live_bindings(&self) -> Vec<BindingRef> {
        self.live.keys().cloned().collect()
    }
}
