//! Worker-side TypeScript integration: one long-lived bridge session per binding and project.
//!
//! A session is bound to the exact project inputs (tsconfig and package files) observed when it
//! started. Every request re-observes them; a changed project retires the session and answers
//! `resolution_unverified` with the reason, so a reply never mixes two project configurations.

use std::{collections::BTreeMap, path::Path, time::Duration};

use crate::{
    assistance::{host_binding::BindingRef, launcher::ProviderLaunch, reply::FailureCode},
    checks::BoxFuture,
    execution::AdmissionClass,
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ProviderSettings},
        typescript::{
            ProjectResolutionInputsV1, TypeScriptProfile, TypeScriptProfileError,
            TypeScriptProfiles, TypeScriptProtocolChild, TypeScriptProviderBundleV1,
            TypeScriptView, TypeScriptViewAdmission, TypeScriptWorktree,
        },
    },
    telemetry::Language,
    workspace::{authority::WorktreeRef, observation::SourceObservation},
};

/// Failure detail when TypeScript project inputs no longer match the snapshot a session was
/// started from; the next request observes them afresh.
const TYPESCRIPT_INPUTS_CHANGED: &str =
    "TypeScript project inputs (tsconfig/package files) changed since the session started";

/// The TypeScript language server bridge integration.
pub struct TypeScriptServer;

impl LanguageServer for TypeScriptServer {
    /// TypeScript provider observations.
    fn language(&self) -> Language {
        Language::Typescript
    }

    /// The bridge's own name.
    fn name(&self) -> &'static str {
        "typescript-language-server"
    }

    /// TypeScript defaults, versioned.
    fn cache_settings(&self) -> &'static str {
        "typescript-defaults-v1"
    }

    /// TypeScript defaults, versioned.
    fn effective_configuration(&self) -> &'static str {
        "typescript-defaults-v1"
    }

    /// Only a private temporary directory.
    fn cache_directories(&self) -> &'static [&'static str] {
        &["tmp"]
    }

    /// `.js`, `.jsx`, `.ts` and `.tsx` sources.
    fn context_extensions(&self) -> &'static [&'static str] {
        &["js", "jsx", "ts", "tsx"]
    }

    /// Symbol tools use the live bridge session for every JavaScript/TypeScript module extension.
    fn session_extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"]
    }

    /// Starts with no sessions and an empty quarantine.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(TypeScriptBackend::default())
    }
}

/// One binding's live bridge: child, exclusive view, the project inputs that selected it and the
/// session driver.
struct TypeScriptLive {
    /// Bridge process owned until reap.
    child: TypeScriptProtocolChild,
    /// Exclusive view admitted for this child.
    view: TypeScriptView,
    /// Resolution inputs observed when the session started.
    inputs: Box<ProjectResolutionInputsV1>,
    /// Transport driver and synchronized document state.
    live: LiveSession,
}

/// Exclusive TypeScript generations, the owner-lifetime profile quarantine and every binding's
/// retained bridge session.
#[derive(Default)]
struct TypeScriptBackend {
    /// Exclusive TypeScript generations and owner-lifetime exact-profile quarantine.
    profiles: TypeScriptProfiles,
    /// Live sessions, one per binding, kept until stop, project change or transport failure.
    live: BTreeMap<BindingRef, TypeScriptLive>,
}

/// Observes bounded TypeScript config and package files away from the single worker thread.
///
/// `job` receives the refusal text as its failure detail when observation rejects the document, so
/// the `resolution_unverified` reply can name the tsconfig consulted and the reason. `document`
/// is the absolute source path; `bundle` and `roots` are moved into the blocking task. A join
/// failure maps to `Internal`, a rejection to `ResolutionUnverified`.
async fn observe_typescript_inputs(
    job: &mut dyn ProviderJob,
    worktree: WorktreeRef,
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
        job.set_failure_detail(rejection.to_string());
        FailureCode::ResolutionUnverified
    })
}

impl TypeScriptBackend {
    /// Starts the accepted TypeScript session for this binding or reuses its project session.
    ///
    /// `job` supplies cancellation and binding ownership; `launch` provides the accepted bundle;
    /// `source` selects and verifies project inputs. A different worktree, bundle, or captured
    /// project file set shuts down the old session before a new one is admitted. Unverified inputs,
    /// authority, capacity, spawn, handshake, and cancellation failures return a bounded code.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        if !launch.typescript_codex_accepted() {
            return Err(FailureCode::ExecutionProfile);
        }
        let binding = job.binding().clone();
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = host.allowed_roots();
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
        if self
            .live
            .get(&binding)
            .is_some_and(|entry| entry.inputs.same_project(&inputs) && entry.live.is_alive())
        {
            return Ok(());
        }
        self.release(host, &binding).await;
        let profile = match TypeScriptProfile::new(
            bundle,
            inputs.clone(),
            launch.trust.clone(),
            Path::new(&cache_namespace).to_path_buf(),
        ) {
            Ok(profile) => profile,
            Err(TypeScriptProfileError::InvalidResolution) => {
                job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                return Err(FailureCode::ResolutionUnverified);
            }
            Err(_) => return Err(FailureCode::ExecutionProfile),
        };
        let worktree = TypeScriptWorktree::new(
            authority.worktree().clone(),
            server::execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let command = match profile.command(&worktree, &path_proof) {
            Ok(command) => command,
            Err(TypeScriptProfileError::InvalidResolution) => {
                job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                return Err(FailureCode::ResolutionUnverified);
            }
            Err(_) => return Err(FailureCode::ExecutionProfile),
        };
        let node = launch.node.as_ref().ok_or(FailureCode::ExecutionProfile)?;
        let request = host
            .execution_request(&*job, &authority, command, node)
            .await?;
        let active = host.active(&binding)?;
        let view = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.profiles.request(
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                server::owner(&binding)?,
                AdmissionClass::Interactive,
            ) {
                TypeScriptViewAdmission::Granted(view) => view,
                TypeScriptViewAdmission::Queued(ticket) => {
                    host.registry().cancel_pending(&mut admission, ticket);
                    return Err(FailureCode::Capacity);
                }
                TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Quarantined) => {
                    return Err(FailureCode::ProviderUnavailable);
                }
                _ => return Err(FailureCode::ProviderUnavailable),
            }
        };
        let output_bytes = host.output_bytes();
        let child = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match TypeScriptProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                host.registry(),
                &mut admission,
                view.lease(),
                Some(active),
                output_bytes,
                &path_proof,
            ) {
                Ok(child) => child,
                Err(error) => {
                    let _ = self.profiles.release(view, host.registry());
                    let failure = if matches!(&error, TypeScriptProfileError::InvalidResolution) {
                        job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
                        FailureCode::ResolutionUnverified
                    } else {
                        FailureCode::ProviderUnavailable
                    };
                    if let TypeScriptProfileError::Process(error) = error {
                        host.spawn_failure(error, &binding);
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
                    ProviderSettings::new(profile.clone()),
                    Duration::from_secs(30),
                );
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.live.insert(
                    binding,
                    TypeScriptLive {
                        child,
                        view,
                        inputs: Box::new(inputs),
                        live,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap(host, &binding, child, view, false).await;
                if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Answers TypeScript context requests through the binding's persistent server session.
    ///
    /// The exchange always waits (bounded) for the diagnostics push. A failed or cancelled
    /// exchange retires the session. The result is then checked against the session's project
    /// snapshot and answers `ResolutionUnverified` (with the failure detail set) when the project
    /// changed.
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
        if !self
            .verify_inputs(host, job, launch, &binding, source)
            .await?
        {
            self.release(host, &binding).await;
            return Err(FailureCode::ResolutionUnverified);
        }
        server::record_provider(
            &*host,
            Language::Typescript,
            server::diagnostic_state(outcome.diagnostics.readiness),
        );
        host.active(&binding)?;
        Ok(outcome)
    }

    /// Re-observes project files after a TypeScript request and checks them against the retained
    /// session's project snapshot.
    ///
    /// `launch` supplies the accepted TypeScript bundle, `binding` selects the live snapshot, and
    /// `source` identifies the requested document. Returns `false` (and sets the job's failure
    /// detail) when project evidence changed; unobservable or invalid evidence returns
    /// `ResolutionUnverified`.
    async fn verify_inputs(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        binding: &BindingRef,
        source: &SourceObservation,
    ) -> Result<bool, FailureCode> {
        let authority = host.authority(binding).await?;
        let bundle = launch
            .typescript_bundle()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let roots = host.allowed_roots();
        let current = observe_typescript_inputs(
            job,
            authority.worktree().clone(),
            authority.worktree().worktree_path().join(source.path()),
            bundle,
            roots,
        )
        .await?;
        let matches = self
            .live
            .get(binding)
            .is_some_and(|entry| entry.inputs.same_project(&current));
        if !matches {
            job.set_failure_detail(TYPESCRIPT_INPUTS_CHANGED.to_owned());
        }
        Ok(matches)
    }

    /// Shuts down and reaps `binding`'s bridge session, if any.
    async fn release(&mut self, host: &mut dyn ProviderHost, binding: &BindingRef) {
        if let Some(TypeScriptLive {
            child, view, live, ..
        }) = self.live.remove(binding)
        {
            let shutdown_completed = live.shutdown().await;
            self.reap(host, binding, child, view, shutdown_completed)
                .await;
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
    async fn reap(
        &mut self,
        host: &mut dyn ProviderHost,
        binding: &BindingRef,
        mut child: TypeScriptProtocolChild,
        view: TypeScriptView,
        shutdown_completed: bool,
    ) {
        let result = if shutdown_completed {
            match child.wait_for_exit(Duration::from_secs(1)).await {
                Ok(waited) => {
                    self.profiles
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
                if let Ok(capability) = self.profiles.release(view, host.registry()) {
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
                let _ = self.profiles.release(view, host.registry());
            }
        }
    }
}

impl ServerBackend for TypeScriptBackend {
    /// Serves context from the binding's bridge session (see [`TypeScriptBackend::answer`]).
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

    /// Starts or keeps the binding's bridge session (see [`TypeScriptBackend::ensure`]).
    fn ensure_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        Box::pin(self.ensure(host, job, launch, source))
    }

    /// Returns the binding's retained bridge session.
    fn live_session(&mut self, binding: &BindingRef) -> Option<&mut LiveSession> {
        self.live.get_mut(binding).map(|entry| &mut entry.live)
    }

    /// Shuts down and reaps the binding's bridge session.
    fn release_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, ()> {
        Box::pin(self.release(host, binding))
    }

    /// Bindings with a retained bridge session.
    fn live_bindings(&self) -> Vec<BindingRef> {
        self.live.keys().cloned().collect()
    }
}
