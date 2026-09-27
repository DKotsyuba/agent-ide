//! Worker-side Pyright integration: one long-lived, worktree-isolated Pyright session per binding.
//!
//! Every exchange waits for the versioned diagnostics push of the synchronized document, so a
//! context reply carries the checker's own verdict on the exact bytes.

use std::{any::Any, collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use serde::Deserialize;

use agent_ide_core::{
    assistance::{
        host_binding::BindingRef,
        launcher::{AcceptedExecutable, ProviderLaunch},
        reply::FailureCode,
    },
    checks::BoxFuture,
    execution::AdmissionClass,
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ProviderSettings},
    },
    lang::Language,
    workspace::observation::SourceObservation,
};

use crate::profile::{
    PyrightProfile, PyrightProfileError, PyrightProfileIdentity, PyrightProtocolChild, PyrightView,
    PyrightViewAdmission, PyrightWorktree,
};

/// Pyright declaration fields beyond the common ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PyrightLaunchOptions {
    /// Absolute operator-declared Node executable, the only program permitted to start Pyright.
    /// Its measured identity must match the declaration's `toolchain`.
    pub node: Option<AcceptedExecutable>,
}

/// The Pyright server integration.
pub struct PyrightServer;

impl LanguageServer for PyrightServer {
    /// The Python language.
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    /// Pyright defaults, versioned.
    fn settings_key(&self) -> &'static str {
        "pyright_defaults_v1"
    }

    /// The Node executable.
    fn option_fields(&self) -> &'static [&'static str] {
        &["node"]
    }

    /// Decodes [`PyrightLaunchOptions`].
    fn parse_options(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn Any + Send + Sync>, serde_json::Error> {
        let options: PyrightLaunchOptions =
            serde_json::from_value(serde_json::Value::Object(fields))?;
        Ok(Arc::new(options))
    }

    /// Requires a valid Node executable whose identity is the declared toolchain.
    fn validate_launch(&self, launch: &ProviderLaunch) -> bool {
        launch
            .options::<PyrightLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .is_some_and(|node| node.validate().is_ok() && launch.toolchain == node.identity)
    }

    /// The declared Node executable.
    fn launch_executables<'a>(&self, launch: &'a ProviderLaunch) -> Vec<&'a AcceptedExecutable> {
        launch
            .options::<PyrightLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .into_iter()
            .collect()
    }

    /// The Pyright script (run by Node) and Node itself.
    fn toolchain_programs(&self, launch: &ProviderLaunch) -> Vec<(PathBuf, Option<PathBuf>)> {
        let node = launch
            .options::<PyrightLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .map(|node| node.path.clone());
        let mut programs = vec![(launch.executable.path.clone(), node.clone())];
        programs.extend(node.map(|node| (node, None)));
        programs
    }

    /// The server's own name, used where a reply explains its missing call hierarchy.
    fn name(&self) -> &'static str {
        "pyright"
    }

    /// Pyright defaults, versioned.
    fn cache_settings(&self) -> &'static str {
        "pyright-defaults-v1"
    }

    /// Pyright defaults, versioned.
    fn effective_configuration(&self) -> &'static str {
        "pyright-defaults-v1"
    }

    /// Only a private temporary directory.
    fn cache_directories(&self) -> &'static [&'static str] {
        &["tmp"]
    }

    /// `.py` and `.pyi` sources.
    fn context_extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    /// Symbol tools use the live Pyright session for `.py` and `.pyi` sources.
    fn session_extensions(&self) -> &'static [&'static str] {
        &["py", "pyi"]
    }

    /// Pyright's call hierarchy answers nothing for constructors and partially for everything
    /// else, so callers and graphs are reported unavailable rather than misleadingly partial.
    fn call_hierarchy(&self) -> bool {
        false
    }

    /// Pyright answers references with an empty list when nothing names the symbol explicitly
    /// (a constructor is only ever called through its class); that zero must be reported.
    fn reports_empty_references(&self) -> bool {
        true
    }

    /// Starts with no sessions.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(PyrightBackend::default())
    }
}

/// One binding's live Pyright: the protocol child, its exclusive view and the session driver.
struct PyrightLive {
    /// Pyright process owned until reap.
    child: PyrightProtocolChild,
    /// Exclusive view admitted for this child.
    view: PyrightView,
    /// Transport driver and synchronized document state.
    live: LiveSession,
}

/// Every binding's retained Pyright session.
#[derive(Default)]
struct PyrightBackend {
    /// Live sessions, one per binding, kept until stop or transport failure.
    live: BTreeMap<BindingRef, PyrightLive>,
}

impl PyrightBackend {
    /// Starts the accepted Pyright session for this binding, or keeps its live session.
    ///
    /// `job` supplies cancellation and binding ownership; `launch` is the accepted executable
    /// profile; `source` fixes the worktree and authority epoch. A replaced child is shut down
    /// before another is admitted. Profile, authority, capacity, spawn, handshake, and
    /// cancellation failures return their bounded `FailureCode`.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.binding().clone();
        if self
            .live
            .get(&binding)
            .is_some_and(|entry| entry.live.is_alive())
        {
            return Ok(());
        }
        self.release(host, &binding).await;
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let node = launch
            .options::<PyrightLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .ok_or(FailureCode::ExecutionProfile)?;
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
            server::execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = profile
            .command(&worktree)
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let request = host
            .execution_request(&*job, &authority, command, node)
            .await?;
        let active = host.active(&binding)?;
        let generation = host.next_generation()?;
        let view = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match PyrightProfile::request_view(
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                server::owner(&binding)?,
                AdmissionClass::Interactive,
                generation,
            ) {
                PyrightViewAdmission::Granted(view) => view,
                PyrightViewAdmission::Queued(ticket) => {
                    host.registry().cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let output_bytes = host.output_bytes();
        let mut child = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match PyrightProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                view.lease(),
                Some(active),
                output_bytes,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let _ = view.release(host.registry());
                    if let PyrightProfileError::Process(error) = error {
                        host.spawn_failure(error, &binding);
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
                    ProviderSettings::new(profile),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.live.insert(binding, PyrightLive { child, view, live });
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

    /// Answers Python context requests through the binding's persistent Pyright session.
    ///
    /// The exchange always waits (bounded) for the document's diagnostics push. A failed or
    /// cancelled exchange retires the session; cancellation maps to `Cancelled`, any other
    /// failure to `ProviderUnavailable`.
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
        let (result, diagnostics) = {
            let entry = self.live.get_mut(&binding).ok_or(FailureCode::Internal)?;
            server::exchange_context(&mut entry.live, job, source, bytes, query, true).await
        };
        let context = match result {
            Ok(context) => context,
            Err(_) => {
                self.release(host, &binding).await;
                return if job.cancelled() {
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
        server::record_provider(
            &*host,
            crate::LANGUAGE,
            server::diagnostic_state(outcome.diagnostics.readiness),
        );
        host.active(&binding)?;
        Ok(outcome)
    }

    /// Shuts down and reaps `binding`'s Pyright session, if any.
    async fn release(&mut self, host: &mut dyn ProviderHost, binding: &BindingRef) {
        if let Some(PyrightLive { child, view, live }) = self.live.remove(binding) {
            let _ = live.shutdown().await;
            self.reap(host, binding, child, view).await;
        }
    }

    /// Reaps a Pyright child and returns its exclusive view to provider accounting.
    async fn reap(
        &mut self,
        host: &mut dyn ProviderHost,
        binding: &BindingRef,
        child: PyrightProtocolChild,
        view: PyrightView,
    ) {
        match child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
            .await
        {
            Ok(reaped) => {
                if let Ok(capability) = view.release(host.registry()) {
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
                let _ = view.release(host.registry());
            }
        }
    }
}

impl ServerBackend for PyrightBackend {
    /// Serves context from the binding's Pyright session (see [`PyrightBackend::answer`]).
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

    /// Starts or keeps the binding's Pyright session (see [`PyrightBackend::ensure`]).
    fn ensure_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        Box::pin(self.ensure(host, job, launch, source))
    }

    /// Returns the binding's retained Pyright session.
    fn live_session(&mut self, binding: &BindingRef) -> Option<&mut LiveSession> {
        self.live.get_mut(binding).map(|entry| &mut entry.live)
    }

    /// Shuts down and reaps the binding's Pyright session.
    fn release_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.release(host, binding))
    }

    /// Bindings with a retained Pyright session.
    fn live_bindings(&self) -> Vec<BindingRef> {
        self.live.keys().cloned().collect()
    }
}
