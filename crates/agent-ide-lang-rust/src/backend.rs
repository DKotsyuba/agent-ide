//! Worker-side rust-analyzer integration: one long-lived analyzer session per binding.
//!
//! The backend owns the exclusive Rust view bookkeeping and every binding's live session. A
//! session starts on the first request, is reused while its transport lives, and is shut down and
//! reaped when the binding stops or an exchange fails.

use std::{any::Any, collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};

use agent_ide_core::modules::launch::ModuleExecutable;

use serde::Deserialize;

use agent_ide_core::{
    assistance::{
        host_binding::BindingRef,
        launcher::{AcceptedExecutable, ProviderLaunch, absolute, identifier},
        reply::FailureCode,
    },
    checks::BoxFuture,
    execution::AdmissionClass,
    intelligence::{
        context::ContextQuery,
        freshness::ViewGeneration,
        server::{self, LanguageServer, ProviderContext, ProviderHost, ProviderJob, ServerBackend},
        session::{LiveSession, ProviderSettings, ReadinessError},
    },
    lang::Language,
    telemetry::DiagnosticState,
    workspace::observation::SourceObservation,
};

use crate::profile::{
    RustCompatibilityKey, RustProfile, RustProfileError, RustProfileIdentity, RustProtocolChild,
    RustView, RustViewAdmission, RustViews, RustWorktree,
};

/// rust-analyzer declaration fields beyond the common ones.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustLaunchOptions {
    /// Absolute operator-declared `cargo` executable. Never chosen by model or project input; its
    /// measured identity must match `cargo_version`.
    pub cargo: Option<AcceptedExecutable>,
    /// Absolute operator-declared Cargo home serving the analyzer's registry; `None` uses the
    /// real home's `.cargo`. Never chosen by model or project input.
    pub cargo_home: Option<PathBuf>,
    /// Accepted Cargo identity.
    pub cargo_version: Option<String>,
    /// Absolute operator-declared `rustc` executable. Never chosen by model or project input; its
    /// measured identity must match `rustc_version`.
    pub rustc: Option<AcceptedExecutable>,
    /// Accepted rustc identity.
    pub rustc_version: Option<String>,
}

/// The rust-analyzer server integration.
pub struct RustServer;

impl LanguageServer for RustServer {
    /// The Rust language.
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    /// Cache priming disabled, versioned.
    fn settings_key(&self) -> &'static str {
        "rust_cache_priming_disabled_v1"
    }

    /// The Cargo and rustc executables, the Cargo home, and their identities.
    fn option_fields(&self) -> &'static [&'static str] {
        &[
            "cargo",
            "cargo_home",
            "cargo_version",
            "rustc",
            "rustc_version",
        ]
    }

    /// Decodes [`RustLaunchOptions`].
    fn parse_options(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn Any + Send + Sync>, serde_json::Error> {
        let options: RustLaunchOptions = serde_json::from_value(serde_json::Value::Object(fields))?;
        Ok(Arc::new(options))
    }

    /// Requires identifier-shaped Cargo and rustc versions and valid `cargo`/`rustc` executables
    /// whose identities equal those versions.
    fn validate_launch(&self, launch: &ProviderLaunch) -> bool {
        let Some(options) = launch.options::<RustLaunchOptions>() else {
            return false;
        };
        options.cargo_version.as_deref().is_some_and(identifier)
            && options.cargo_home.as_deref().is_none_or(absolute)
            && options.rustc_version.as_deref().is_some_and(identifier)
            && options.cargo.as_ref().is_some_and(|cargo| {
                cargo.validate().is_ok()
                    && Some(cargo.identity.as_str()) == options.cargo_version.as_deref()
            })
            && options.rustc.as_ref().is_some_and(|rustc| {
                rustc.validate().is_ok()
                    && Some(rustc.identity.as_str()) == options.rustc_version.as_deref()
            })
    }

    /// The declared `cargo` and `rustc`.
    fn launch_executables<'a>(&self, launch: &'a ProviderLaunch) -> Vec<&'a AcceptedExecutable> {
        launch
            .options::<RustLaunchOptions>()
            .map(|options| options.cargo.iter().chain(options.rustc.iter()).collect())
            .unwrap_or_default()
    }

    /// The analyzer's own name.
    fn name(&self) -> &'static str {
        "rust-analyzer"
    }

    /// Rust-analyzer cache priming and check-on-save disabled, versioned.
    fn cache_settings(&self) -> &'static str {
        "rust-cache-priming-check-on-save-disabled-v1"
    }

    /// The configuration identity [`RustProfile`] accepts for managed sessions.
    fn effective_configuration(&self) -> &'static str {
        "cache-priming-check-on-save-disabled-v1"
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

    /// Manifests, lock file, toolchain pins and Cargo configuration rust-analyzer loads.
    fn project_inputs(&self) -> &'static [&'static str] {
        &[
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
            "rust-project.json",
            "config.toml",
        ]
    }

    /// Starts with no views and no sessions.
    fn new_backend(&self) -> Box<dyn ServerBackend> {
        Box::new(RustBackend::default())
    }
}

/// How one binding's analyzer starts.
enum Start {
    /// rust-analyzer as a direct daemon child, from the measured in-process profile.
    InProcess(RustProfile),
    /// rust-analyzer inside the Rust module, which builds its profile from the granted settings.
    Module {
        /// The pinned module executable.
        executable: Arc<ModuleExecutable>,
        /// The declaration's settings in the allocated namespace.
        settings: crate::module::RustProviderSettings,
        /// The accepted inputs (module digest and settings) the restart policy is keyed by.
        inputs: String,
    },
}

/// The measured in-process analyzer profile of `launch` in `cache_namespace`.
fn profile_of(launch: &ProviderLaunch, cache_namespace: &str) -> Result<RustProfile, FailureCode> {
    let options = launch
        .options::<RustLaunchOptions>()
        .ok_or(FailureCode::ExecutionProfile)?;
    RustProfile::new(RustProfileIdentity {
        binary: launch.executable.path.clone(),
        rust_analyzer_version: launch.executable.identity.clone(),
        cargo: options
            .cargo
            .as_ref()
            .ok_or(FailureCode::ExecutionProfile)?
            .path
            .clone(),
        cargo_home: options.cargo_home.clone(),
        cargo_version: options
            .cargo_version
            .clone()
            .ok_or(FailureCode::ExecutionProfile)?,
        rustc: options
            .rustc
            .as_ref()
            .ok_or(FailureCode::ExecutionProfile)?
            .path
            .clone(),
        rustc_version: options
            .rustc_version
            .clone()
            .ok_or(FailureCode::ExecutionProfile)?,
        rustup_toolchain: launch.toolchain.clone(),
        configuration: RustServer.effective_configuration().into(),
        trust: launch.trust.clone(),
        transport: "stdio-v1".into(),
        cache_namespace: cache_namespace.to_owned(),
    })
    .map_err(|_| FailureCode::ExecutionProfile)
}

/// Roots the core admits for the paths of the module-started analyzer's environment beside the
/// worktree and the accepted files' directories: the private namespace, the declared Cargo home,
/// the user home (its `.cargo`) and the system tool directories.
pub(crate) fn provider_roots(settings: &crate::module::RustProviderSettings) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(&settings.cache_namespace)];
    roots.extend(settings.cargo_home.clone());
    roots.extend(agent_ide_core::userhome::user_home());
    roots.extend(["/usr/bin", "/bin"].map(PathBuf::from));
    roots
}

/// Counts one failure of a Rust analyzer module started with `inputs` against the shared restart
/// policy (`exited` when no typed failure is known).
fn record_module_failure(
    worktree: &std::path::Path,
    inputs: &str,
    failure: Option<agent_ide_core::modules::contract::ModuleUnavailable>,
) {
    agent_ide_core::modules::analyzer::record_failure(
        crate::DESCRIPTOR.id,
        worktree,
        inputs,
        &module_failure(failure),
    );
}

/// The typed failure of a module session that could not open: the module's own when it gave
/// one, else a `hello` failure (`timeout` when the exchange ran out of time, `exited` otherwise).
fn hello_failure(error: &std::io::Error) -> agent_ide_core::modules::contract::ModuleUnavailable {
    use agent_ide_core::modules::contract::{Cause, ModuleUnavailable, Stage};
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ModuleUnavailable>())
        .cloned()
        .unwrap_or_else(|| ModuleUnavailable {
            stage: Stage::Hello,
            cause: if error.kind() == std::io::ErrorKind::TimedOut {
                Cause::Timeout
            } else {
                Cause::Exited
            },
            ..module_failure(None)
        })
}

/// The Rust analyzer module's typed failure, `request`/`exited` when none was observed.
fn module_failure(
    failure: Option<agent_ide_core::modules::contract::ModuleUnavailable>,
) -> agent_ide_core::modules::contract::ModuleUnavailable {
    use agent_ide_core::modules::contract::{Cause, ModuleId, ModuleUnavailable, Role, Stage};
    failure.unwrap_or(ModuleUnavailable {
        module_id: ModuleId::bundled(crate::DESCRIPTOR.id),
        module_version: env!("CARGO_PKG_VERSION").to_owned(),
        role: Role::Analyzer,
        stage: Stage::Request,
        cause: Cause::Exited,
        instance: None,
        retry_after_ms: None,
    })
}

/// Opens the analyzer session on the started child's pipes: directly on rust-analyzer, or on the
/// Rust module that hosts it (`hello` grants only the declaration's accepted files, the admitted
/// roots and the settings; the daemon keeps only the static session shape).
async fn open_live(
    start: &Start,
    launch: &ProviderLaunch,
    stdout: tokio::process::ChildStdout,
    stdin: tokio::process::ChildStdin,
    source: &SourceObservation,
    generation: ViewGeneration,
) -> std::io::Result<LiveSession> {
    let (executable, settings) = match start {
        Start::InProcess(profile) => {
            return LiveSession::open(
                stdout,
                stdin,
                source.worktree().clone(),
                source.authority_epoch(),
                generation,
                ProviderSettings::new(profile.clone()),
                Duration::from_secs(30),
            )
            .await;
        }
        Start::Module {
            executable,
            settings,
            ..
        } => (executable, settings),
    };
    let options = launch
        .options::<RustLaunchOptions>()
        .ok_or_else(|| std::io::Error::other("rust provider declaration"))?;
    let accepted = std::iter::once(&launch.executable)
        .chain(options.cargo.iter())
        .chain(options.rustc.iter())
        .map(|file| (file.path.clone(), file.blake3.clone()))
        .collect();
    let offer = agent_ide_core::modules::analyzer::analyzer_offer(
        executable,
        crate::DESCRIPTOR.id,
        generation.view,
        source.worktree(),
        accepted,
        provider_roots(settings),
        Duration::from_secs(30),
        serde_json::to_value(settings).map_err(std::io::Error::other)?,
    );
    agent_ide_core::modules::analyzer::open_session(
        stdout,
        stdin,
        offer,
        source.worktree().clone(),
        source.authority_epoch(),
        generation,
        ProviderSettings::new(crate::profile::RustModuleSession),
        agent_ide_core::modules::router::budget_or(Duration::from_secs(30)),
    )
    .await
}

/// Names the failed initialize stage on the job's failure reply: a handshake that exhausted its
/// 30-second bound reports the timeout; any other initialize failure (a server that exited or
/// answered invalidly before readiness) reports the failed initialize. Closed stage words only.
fn initialize_stage(job: &mut dyn ProviderJob, error: &std::io::Error) {
    if error.kind() == std::io::ErrorKind::TimedOut {
        job.set_stage_failure(
            &FailureCode::ProviderUnavailable,
            "rust: initialize timeout",
        );
    } else {
        job.set_stage_failure(&FailureCode::ProviderUnavailable, "rust: initialize failed");
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
    /// The accepted inputs of a module-hosted session and its worktree, against which its
    /// failures count.
    module_inputs: Option<(String, PathBuf)>,
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
    /// profile, spawn, initialization, and cancellation failures return a bounded code. The
    /// view, spawn and initialize refusals name their stage on `job` (`rust: view refused`,
    /// `spawn failed`, `initialize failed` or `initialize timeout`); a bare `ProviderUnavailable`
    /// from the cache-namespace lookup is named by the caller after this returns.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.binding().clone();
        let worktree_path = source.worktree().worktree_path();
        if let Some(entry) = self.live.get(&binding) {
            if entry.live.is_alive() {
                return Ok(());
            }
            // A module that died — while a call waited for its analyzer to load, or idle — counts
            // against the shared restart policy, and the demand that finds it dead is settled with
            // its typed fault (never silently answered by a restarted one); the next demand
            // restarts it within the policy.
            if entry.module_inputs.is_some() {
                let failure = module_failure(entry.live.module_unavailable());
                self.release(host, &binding, true).await;
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!("rust: {failure}"),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
            self.release(host, &binding, true).await;
        }
        // In module mode the Rust module is the process the core admits; it plans, verifies and
        // starts rust-analyzer from the granted files. An unpinnable module is a typed refusal,
        // never a silent in-process fallback.
        let module = match agent_ide_core::modules::calls::module_executable(crate::LANGUAGE) {
            None => None,
            Some(Ok(executable)) => Some(executable),
            Some(Err(failure)) => {
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!("rust: {failure}"),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
        };
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let scoped = server::execution_authority(&authority)?;
        let worktree = RustWorktree::new(authority.worktree().clone(), scoped)
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let start = match module {
            None => Start::InProcess(profile_of(launch, &cache_namespace)?),
            Some(executable) => {
                let settings =
                    crate::module::RustProviderSettings::from_launch(launch, &cache_namespace)
                        .ok_or(FailureCode::ExecutionProfile)?;
                let inputs = format!(
                    "{}:{}",
                    executable.digest.to_hex(),
                    serde_json::json!(settings)
                );
                if let Err(failure) = agent_ide_core::modules::analyzer::start_permit(
                    crate::DESCRIPTOR.id,
                    worktree_path,
                    &inputs,
                    tokio::time::Instant::now() + Duration::from_secs(5),
                )
                .await
                {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        &format!("rust: {failure}"),
                    );
                    return Err(FailureCode::ProviderUnavailable);
                }
                Start::Module {
                    executable,
                    settings,
                    inputs,
                }
            }
        };
        let (command, program, key) = match &start {
            Start::InProcess(profile) => (
                profile
                    .command(&worktree)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                launch.executable.clone(),
                profile.compatibility_key(&worktree),
            ),
            Start::Module {
                executable, inputs, ..
            } => (
                agent_ide_core::modules::analyzer::analyzer_command(
                    executable,
                    crate::DESCRIPTOR.id,
                    authority.worktree(),
                )
                .ok_or(FailureCode::ExecutionProfile)?,
                AcceptedExecutable {
                    path: executable.path.clone(),
                    identity: "agent-ide-module".to_owned(),
                    blake3: executable.digest.to_hex().to_string(),
                },
                RustCompatibilityKey::module(inputs, &worktree),
            ),
        };
        let request = host
            .execution_request(&*job, &authority, command, &program)
            .await?;
        let active = host.active(&binding)?;
        // The shared controller guard is confined to this block: it is a `std` mutex, so it must
        // never reach the awaits below or this worker future stops being `Send`.
        let view = {
            let admission = host.admission();
            let mut admission = admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match self.views.request_keyed(
                key,
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
                _ => {
                    job.set_stage_failure(&FailureCode::ProviderUnavailable, "rust: view refused");
                    return Err(FailureCode::ProviderUnavailable);
                }
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
                job.set_stage_failure(&FailureCode::ProviderUnavailable, "rust: spawn failed");
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
                let open = open_live(&start, launch, stdout, stdin, source, generation);
                tokio::pin!(open);
                // Stop or shutdown must be able to interrupt a handshake the server never answers.
                tokio::select! {
                    opened = &mut open => opened,
                    _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")),
                }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        let module_inputs = match start {
            Start::Module { inputs, .. } => Some((inputs, worktree_path.to_path_buf())),
            Start::InProcess(_) => None,
        };
        match opened {
            Ok(live) => {
                self.live.insert(
                    binding,
                    RustLive {
                        child,
                        view,
                        live,
                        module_inputs,
                    },
                );
                Ok(())
            }
            Err(error) => {
                self.reap(host, &binding, child, view).await;
                if job.cancelled() {
                    return Err(FailureCode::Cancelled);
                }
                match &module_inputs {
                    // The module's typed failure names the stage and cause and counts against the
                    // restart policy.
                    Some((inputs, _)) => {
                        let failure = hello_failure(&error);
                        record_module_failure(worktree_path, inputs, Some(failure.clone()));
                        job.set_stage_failure(
                            &FailureCode::ProviderUnavailable,
                            &format!("rust: {failure}"),
                        );
                    }
                    None => initialize_stage(job, &error),
                }
                Err(FailureCode::ProviderUnavailable)
            }
        }
    }

    /// Answers a Rust context request from the binding's long-lived analyzer session, starting
    /// one on first use. Readiness is probed for at most 100 ms; a loading non-edit job records a
    /// 300 ms resume time so the worker can run other work while the analyzer keeps loading.
    /// A `ProviderUnavailable` answer names its stage on `job` (`rust: workspace load failed`, or
    /// `rust: request failed` after which the failed session is retired).
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
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        "rust: workspace load failed",
                    );
                    return Err(FailureCode::ProviderUnavailable);
                }
                Err(ReadinessError::Gone) => Err(std::io::Error::other("transport gone")),
            }
        };
        let result = match outcome {
            Ok(context) => Ok(context),
            Err(_) => {
                // A failed or cancelled exchange retires the session; the next request starts a
                // fresh one. A module's typed fault names it (`module_unavailable (…)`) and
                // counts against the restart policy.
                let fault = self
                    .live
                    .get(&binding)
                    .filter(|entry| entry.module_inputs.is_some())
                    .map(|entry| module_failure(entry.live.module_unavailable()).to_string());
                self.release(host, &binding, !job.cancelled()).await;
                if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        &fault.map_or_else(
                            || "rust: request failed".to_owned(),
                            |fault| format!("rust: {fault}"),
                        ),
                    );
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        };
        let diagnostics = match &result {
            Ok(context) => server::diagnostic_state(context.diagnostics.readiness),
            Err(_) => DiagnosticState::Unavailable,
        };
        server::record_provider(&*host, crate::LANGUAGE, diagnostics);
        host.active(&binding)?;
        result
    }

    /// Shuts down and reaps `binding`'s Rust session, if any. A module session that died
    /// (not a controlled stop or a cancellation, `count` false) counts once against the shared
    /// restart policy, whichever path retires it.
    async fn release(&mut self, host: &mut dyn ProviderHost, binding: &BindingRef, count: bool) {
        if let Some(RustLive {
            child,
            view,
            live,
            module_inputs,
        }) = self.live.remove(binding)
        {
            if count
                && !live.is_alive()
                && let Some((inputs, worktree)) = &module_inputs
            {
                record_module_failure(worktree, inputs, live.module_unavailable());
            }
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
        Box::pin(self.release(host, binding, true))
    }

    /// Bindings with a retained analyzer session.
    fn live_bindings(&self) -> Vec<BindingRef> {
        self.live.keys().cloned().collect()
    }
}
