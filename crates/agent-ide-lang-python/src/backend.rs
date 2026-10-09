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

    /// Project metadata Pyright reads when it resolves imports.
    fn project_inputs(&self) -> &'static [&'static str] {
        &[
            "pyproject.toml",
            "pyrightconfig.json",
            "setup.cfg",
            "setup.py",
            "requirements.txt",
            "Pipfile",
            "poetry.lock",
        ]
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
    /// Interpreter the session was started with, `None` when no environment resolved.
    interpreter: Option<PathBuf>,
    /// Shared-resolver identity of that environment; a different identity at use restarts.
    environment: String,
}

/// Whether a live session started for `started` may keep serving a worktree whose environment
/// now resolves to `current`: only while it is alive and the identity is unchanged, the same
/// reuse rule as TypeScript's `same_project`.
fn keeps_session(alive: bool, started: &str, current: &str) -> bool {
    alive && started == current
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
    /// profile; `source` fixes the worktree and authority epoch. The environment is resolved at
    /// every use: a session whose interpreter identity no longer matches is released and
    /// restarted with the new one. A replaced child is shut down before another is admitted.
    /// Profile, authority, capacity, spawn, handshake, and cancellation failures return their
    /// bounded `FailureCode`; the view, spawn and handshake refusals name their stage on `job`
    /// (`python: view refused`, `spawn failed` or `initialize failed`) and a bare
    /// `ProviderUnavailable` from the cache-namespace lookup is named by the caller.
    async fn ensure(
        &mut self,
        host: &mut dyn ProviderHost,
        job: &mut dyn ProviderJob,
        launch: &ProviderLaunch,
        source: &SourceObservation,
    ) -> Result<(), FailureCode> {
        let binding = job.binding().clone();
        let (interpreter, environment) =
            crate::environment::session(source.worktree().worktree_path());
        if self.live.get(&binding).is_some_and(|entry| {
            keeps_session(entry.live.is_alive(), &entry.environment, &environment)
        }) {
            return Ok(());
        }
        self.release(host, &binding).await;
        let authority = host.authority(&binding).await?;
        let cache_namespace = host.cache_namespace(&binding, &authority, launch, &launch.trust)?;
        let node = launch
            .options::<PyrightLaunchOptions>()
            .and_then(|options| options.node.as_ref())
            .ok_or(FailureCode::ExecutionProfile)?;
        let extra_paths = crate::support::import_roots(source.worktree().worktree_path());
        // M-011 pilot: the same accepted identities, handed to the external analyzer module.
        let pilot = crate::pilot::enabled().then(|| crate::pilot::ProviderParams {
            binary: launch.executable.path.clone(),
            script_digest: launch.executable.blake3.clone(),
            version: launch.executable.identity.clone(),
            node: node.path.clone(),
            node_digest: node.blake3.clone(),
            node_identity: node.identity.clone(),
            trust: launch.trust.clone(),
            cache_namespace: cache_namespace.clone(),
            interpreter: interpreter.clone(),
            environment: environment.clone(),
            extra_paths: extra_paths.clone(),
        });
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
        .map_err(|_| FailureCode::ExecutionProfile)?
        .with_interpreter(interpreter.clone(), environment.clone())
        .with_extra_paths(extra_paths);
        let worktree = PyrightWorktree::new(
            authority.worktree().clone(),
            server::execution_authority(&authority)?,
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let (command, program) = match &pilot {
            Some(params) => {
                let module =
                    crate::pilot::module_executable().ok_or(FailureCode::ExecutionProfile)?;
                let command =
                    crate::pilot::analyzer_command(module, &worktree, &params.cache_namespace)
                        .ok_or(FailureCode::ExecutionProfile)?;
                (command, module)
            }
            None => (
                profile
                    .command(&worktree)
                    .map_err(|_| FailureCode::ExecutionProfile)?,
                node,
            ),
        };
        let request = host
            .execution_request(&*job, &authority, command, program)
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
                _ => {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        "python: view refused",
                    );
                    return Err(FailureCode::ProviderUnavailable);
                }
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
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        "python: spawn failed",
                    );
                    return Err(FailureCode::ProviderUnavailable);
                }
            }
        };
        let opened = match child.take_pipes() {
            Some((stdin, stdout)) => {
                let generation = ViewGeneration {
                    backend: generation,
                    configuration: 1,
                    toolchain: 1,
                    view: generation,
                };
                let open = async {
                    match pilot {
                        Some(params) => {
                            LiveSession::open_remote(
                                stdout,
                                stdin,
                                source.worktree().clone(),
                                source.authority_epoch(),
                                generation,
                                ProviderSettings::new(profile),
                                Duration::from_secs(30),
                                serde_json::json!(params),
                            )
                            .await
                        }
                        None => {
                            LiveSession::open(
                                stdout,
                                stdin,
                                source.worktree().clone(),
                                source.authority_epoch(),
                                generation,
                                ProviderSettings::new(profile),
                                Duration::from_secs(30),
                            )
                            .await
                        }
                    }
                };
                tokio::pin!(open);
                tokio::select! { result = &mut open => result, _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")), }
            }
            None => Err(std::io::Error::other("protocol pipes already taken")),
        };
        match opened {
            Ok(live) => {
                self.live.insert(
                    binding,
                    PyrightLive {
                        child,
                        view,
                        live,
                        interpreter,
                        environment,
                    },
                );
                Ok(())
            }
            Err(_) => {
                self.reap(host, &binding, child, view).await;
                if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        "python: initialize failed",
                    );
                    Err(FailureCode::ProviderUnavailable)
                }
            }
        }
    }

    /// Answers Python context requests through the binding's persistent Pyright session.
    ///
    /// The exchange always waits (bounded) for the document's diagnostics push. A failed or
    /// cancelled exchange retires the session; cancellation maps to `Cancelled`, any other
    /// failure to `ProviderUnavailable` after naming the stage `python: request failed` on `job`.
    /// When the worktree has no Python environment at all, the
    /// push's per-import `Import "..." could not be resolved` flood is collapsed into the single
    /// line [`MISSING_ENVIRONMENT_IMPORTS`] (see [`summarize_missing_environment`]); every other
    /// diagnostic survives untouched.
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
        let (result, mut diagnostics, no_environment) = {
            let entry = self.live.get_mut(&binding).ok_or(FailureCode::Internal)?;
            let (result, diagnostics) =
                server::exchange_context(&mut entry.live, job, source, bytes, query, true).await;
            // M-011 pilot: a module fault is a typed refusal naming it, never a quiet lexical
            // answer; the next call starts a fresh module.
            if let Some(fault) = entry.live.remote_fault().map(str::to_owned) {
                self.release(host, &binding).await;
                job.set_stage_failure(
                    &FailureCode::ProviderUnavailable,
                    &format!("python: {fault}"),
                );
                return Err(FailureCode::ProviderUnavailable);
            }
            (result, diagnostics, entry.interpreter.is_none())
        };
        let context = match result {
            Ok(context) => context,
            Err(_) => {
                self.release(host, &binding).await;
                return if job.cancelled() {
                    Err(FailureCode::Cancelled)
                } else {
                    job.set_stage_failure(
                        &FailureCode::ProviderUnavailable,
                        "python: request failed",
                    );
                    Err(FailureCode::ProviderUnavailable)
                };
            }
        };
        // The session's own interpreter, not a fresh resolution: the summary must describe what
        // the server analysed with.
        if no_environment {
            summarize_missing_environment(&mut diagnostics);
        }
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
        if let Some(PyrightLive {
            child, view, live, ..
        }) = self.live.remove(binding)
        {
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

/// The single line that replaces pyright's per-import resolution flood when the worktree has no
/// Python environment: without one, no import is checked at all, so thirty `Import "numpy"
/// could not be resolved` messages carry no signal.
const MISSING_ENVIRONMENT_IMPORTS: &str = "python environment not found — imports are not checked";

/// Pyright rules that only report an import the configured environment could not resolve.
const IMPORT_RESOLUTION_RULES: [&str; 2] = ["reportMissingImports", "reportMissingModuleSource"];

/// Collapses the import-resolution diagnostics of `diagnostics` into the one
/// [`MISSING_ENVIRONMENT_IMPORTS`] line; the caller has already established that the worktree
/// has no Python environment. Diagnostics of every other rule survive untouched, and the line is
/// appended after them, so an edit reply says why its imports were not checked exactly once.
fn summarize_missing_environment(
    diagnostics: &mut agent_ide_core::intelligence::session::DiagnosticSnapshot,
) {
    if !diagnostics.diagnostics.iter().any(is_import_resolution) {
        return;
    }
    let first = diagnostics
        .diagnostics
        .iter()
        .find(|diagnostic| is_import_resolution(diagnostic))
        .cloned();
    diagnostics
        .diagnostics
        .retain(|diagnostic| !is_import_resolution(diagnostic));
    let mut line = first.unwrap_or_default();
    line.message = MISSING_ENVIRONMENT_IMPORTS.to_owned();
    line.code = None;
    diagnostics.diagnostics.push(line);
    diagnostics.truncated = false;
}

/// Reports whether one pyright diagnostic only names an import the environment could not resolve.
fn is_import_resolution(diagnostic: &async_lsp::lsp_types::Diagnostic) -> bool {
    diagnostic
        .code
        .as_ref()
        .and_then(|code| match code {
            async_lsp::lsp_types::NumberOrString::String(rule) => Some(rule.as_str()),
            _ => None,
        })
        .is_some_and(|rule| IMPORT_RESOLUTION_RULES.contains(&rule))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use agent_ide_core::lang::environment::{EnvSelection, replace_selections};

    use super::*;

    /// Writes an empty `bin/python` for the environment `name` under `root`.
    fn venv(root: &Path, name: &str) {
        let bin = root.join(name).join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("python"), "").unwrap();
    }

    /// The live session is kept while the resolved environment is unchanged and released when
    /// a selection switches the interpreter, or when it died.
    #[test]
    fn session_restarts_when_the_resolved_interpreter_changes() {
        let root = std::env::temp_dir().join(format!(
            "agent-ide-pyright-session-env-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("pyproject.toml"), "").unwrap();
        venv(&root, ".venv");
        venv(&root, ".venv-py314");

        let (interpreter, started) = crate::environment::session(&root);
        assert_eq!(interpreter, Some(root.join(".venv/bin/python")));
        assert!(keeps_session(
            true,
            &started,
            &crate::environment::session(&root).1
        ));

        replace_selections(
            &root,
            crate::LANGUAGE,
            vec![EnvSelection {
                root: PathBuf::new(),
                selector: ".venv-py314".to_owned(),
            }],
        );
        let (interpreter, current) = crate::environment::session(&root);
        assert_eq!(interpreter, Some(root.join(".venv-py314/bin/python")));
        assert!(!keeps_session(true, &started, &current));
        assert!(!keeps_session(false, &current, &current));

        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }
}
