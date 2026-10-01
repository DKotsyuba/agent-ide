//! The language-server seam between the provider layer and each language's server integration.
//!
//! A language contributes one static [`LanguageServer`](crate::intelligence::server::LanguageServer) describing its launcher identity, cache
//! layout, routing and capabilities, and creates one stateful [`ServerBackend`](crate::intelligence::server::ServerBackend) per worker that
//! owns its processes, provider views and live sessions. The worker drives backends through the
//! narrow [`ProviderHost`](crate::intelligence::server::ProviderHost) (daemon-wide admission, authority, caches and accounting) and
//! [`ProviderJob`](crate::intelligence::server::ProviderJob) (the one operation being served) seams, so no backend reaches into worker
//! state and the worker never branches on a language.

use std::{
    any::Any,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Duration,
};

use tokio::sync::watch;

use crate::{
    assistance::{
        host_binding::{ActiveBindingUse, BindingRef},
        launcher::{AcceptedExecutable, LauncherError, ProviderLaunch},
        reply::FailureCode,
    },
    checks::BoxFuture,
    execution::{
        AdmissionController, AdmissionLease, ControlledCommand, OwnerId, ProcessError,
        ProviderLeaseRegistry, ValidatedExecutionRequest, WorkspaceAuthority,
    },
    intelligence::{
        context::{ContextQuery, ContextResult},
        freshness::DiagnosticReadiness,
        session::{DiagnosticSnapshot, LiveSession, SessionOptions},
    },
    lang::Language,
    telemetry::{CacheState, DiagnosticState, Telemetry, adapters},
    workspace::{authority::AuthorityStamp, observation::SourceObservation},
};

/// Couples one semantic context result to diagnostics observed by that exact provider session.
pub struct ProviderContext {
    /// Source and semantic locations returned for the synchronized document generation.
    pub context: ContextResult,
    /// Latest bounded diagnostic push retained by the same session before shutdown.
    pub diagnostics: DiagnosticSnapshot,
}

/// Static description of one language's server integration.
///
/// Implementations are stateless `'static` values; everything that changes at runtime lives in
/// the [`ServerBackend`] each worker creates with [`LanguageServer::new_backend`].
pub trait LanguageServer: Send + Sync + 'static {
    /// The language this server belongs to; also recorded in its provider observations.
    fn language(&self) -> Language;

    /// Closed launcher `settings` identifier that selects this server in a provider declaration.
    fn settings_key(&self) -> &'static str;

    /// Declaration fields this server accepts beyond the common executable, settings, toolchain,
    /// trust and cache namespace. Empty (the default) when it needs none. A field listed by
    /// another registered server is refused on this server's declarations.
    fn option_fields(&self) -> &'static [&'static str] {
        &[]
    }

    /// Decodes this server's declaration fields into its typed options.
    ///
    /// `fields` holds only keys from [`LanguageServer::option_fields`] with non-null values. A
    /// shape error is the launcher's `Invalid`. The default accepts only an empty object.
    fn parse_options(
        &self,
        fields: serde_json::Map<String, serde_json::Value>,
    ) -> Result<Arc<dyn Any + Send + Sync>, serde_json::Error> {
        if let Some(key) = fields.keys().next() {
            return Err(serde::de::Error::custom(format_args!(
                "unknown field `{key}`"
            )));
        }
        Ok(Arc::new(()))
    }

    /// Server-specific declaration rules (toolchain shape, required fields, identity matches);
    /// `false` is the launcher's `Rejected`. Runs without filesystem access.
    fn validate_launch(&self, launch: &ProviderLaunch) -> bool;

    /// Additional accepted executables of `launch` (interpreters, companion tools) whose bytes
    /// startup verifies alongside the server executable. Empty by default.
    fn launch_executables<'a>(&self, launch: &'a ProviderLaunch) -> Vec<&'a AcceptedExecutable> {
        let _ = launch;
        Vec::new()
    }

    /// Further startup verification after every executable was verified, honouring `cancel`.
    /// The default has nothing more to verify.
    fn verify_launch(
        &self,
        launch: &ProviderLaunch,
        cancel: &AtomicBool,
    ) -> Result<(), LauncherError> {
        let _ = (launch, cancel);
        Ok(())
    }

    /// Toolchain programs `doctor` probes for `launch`, in declaration order, each with the
    /// interpreter that runs it when it is not directly executable.
    fn toolchain_programs(&self, launch: &ProviderLaunch) -> Vec<(PathBuf, Option<PathBuf>)> {
        vec![(launch.executable.path.clone(), None)]
    }

    /// Short server name used in replies that explain a missing capability.
    fn name(&self) -> &'static str;

    /// Immutable settings identity used by cache compatibility and namespace derivation.
    fn cache_settings(&self) -> &'static str;

    /// Exact initialization configuration identity the provider command will use.
    fn effective_configuration(&self) -> &'static str;

    /// Private subdirectories the provider needs inside its retained per-worktree namespace.
    fn cache_directories(&self) -> &'static [&'static str];

    /// Subdirectories of the one shared native namespace this server keeps across worktrees, or
    /// `None` (the default) when it has no shared namespace.
    fn shared_cache_directories(&self) -> Option<&'static [&'static str]> {
        None
    }

    /// File extensions (without the dot) whose semantic context this server answers.
    fn context_extensions(&self) -> &'static [&'static str];

    /// File extensions whose symbol tools use this server's long-lived session; empty (the
    /// default) when the server has no live session and symbol tools are unavailable for it.
    fn session_extensions(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether the server's call hierarchy is reliable enough to render callers and graphs.
    /// Defaults to `true`.
    fn call_hierarchy(&self) -> bool {
        true
    }

    /// Whether an empty reference answer is itself a fact worth reporting (the server answers
    /// references with an empty list when nothing names the symbol explicitly). Defaults to
    /// `false`.
    fn reports_empty_references(&self) -> bool {
        false
    }

    /// Creates this server's per-worker state; no process starts until a request needs one.
    fn new_backend(&self) -> Box<dyn ServerBackend>;
}

/// One worker's stateful integration of one language server.
///
/// Every method runs on the single worker task; a backend is never called concurrently. Methods
/// that start processes or sessions return a bounded [`FailureCode`] and leave the backend usable.
/// `host` carries daemon-wide facilities and `job` the operation being served; a backend keeps no
/// reference to either beyond one call.
pub trait ServerBackend: Send + Sync {
    /// Answers one semantic context request for `source` with the exact observed `bytes`.
    ///
    /// `launch` is the accepted provider declaration selected for this server. Cancellation maps
    /// to `Cancelled`; transport or protocol failure to `ProviderUnavailable`.
    fn context<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
        bytes: &'a [u8],
        query: ContextQuery,
    ) -> BoxFuture<'a, Result<ProviderContext, FailureCode>>;

    /// Starts the job binding's long-lived session for `source` unless a usable one is retained.
    ///
    /// Only servers with [`LanguageServer::session_extensions`] implement this; the default
    /// answers `ProviderUnavailable`.
    fn ensure_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        job: &'a mut dyn ProviderJob,
        launch: &'a ProviderLaunch,
        source: &'a SourceObservation,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        let _ = (host, job, launch, source);
        Box::pin(async { Err(FailureCode::ProviderUnavailable) })
    }

    /// Returns the retained live session of `binding`, if any.
    fn live_session(&mut self, binding: &BindingRef) -> Option<&mut LiveSession> {
        let _ = binding;
        None
    }

    /// Shuts down and reaps `binding`'s live session, if any; failures mark the binding
    /// uncertain through `host` instead of returning.
    fn release_live<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, ()> {
        let _ = (host, binding);
        Box::pin(async {})
    }

    /// Bindings that currently retain a live session, in ascending order.
    fn live_bindings(&self) -> Vec<BindingRef> {
        Vec::new()
    }

    /// Releases every non-session provider resource `binding` holds (for example a logical view
    /// on a shared listener). Runs before the binding's durable revoke; an error keeps the stop
    /// retryable. The default holds nothing and succeeds.
    fn close_binding<'a>(
        &'a mut self,
        host: &'a mut dyn ProviderHost,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, Result<(), FailureCode>> {
        let _ = (host, binding);
        Box::pin(async { Ok(()) })
    }

    /// Bindings holding resources [`ServerBackend::close_binding`] must release, ascending.
    fn bound_bindings(&self) -> Vec<BindingRef> {
        Vec::new()
    }
}

/// Daemon-wide facilities a backend may use while serving one call.
///
/// Implemented by the worker. Every method is synchronous and bounded except the two that return
/// futures, which consult durable Workspace authority.
pub trait ProviderHost: Send + Sync {
    /// Returns a current boot-fenced authority stamp for `binding` after a fresh binding consume.
    fn authority<'a>(
        &'a self,
        binding: &'a BindingRef,
    ) -> BoxFuture<'a, Result<AuthorityStamp, FailureCode>>;

    /// Resolves the already-retained per-worktree cache namespace for `launch` under `trust`.
    /// Fails `ProviderUnavailable` when `binding` does not own a live retained lifecycle.
    fn cache_namespace(
        &self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        launch: &ProviderLaunch,
        trust: &str,
    ) -> Result<String, FailureCode>;

    /// Resolves the retained shared native namespace for `launch` under `trust`.
    fn shared_cache_namespace(
        &self,
        binding: &BindingRef,
        launch: &ProviderLaunch,
        trust: &str,
    ) -> Result<String, FailureCode>;

    /// Combines startup-verified `program` selection with current durable authority, the job's
    /// cancellation and deadline, and the sandbox state into one validated execution request.
    fn execution_request<'a>(
        &'a self,
        job: &'a dyn ProviderJob,
        authority: &'a AuthorityStamp,
        command: ControlledCommand,
        program: &'a AcceptedExecutable,
    ) -> BoxFuture<'a, Result<ValidatedExecutionRequest, FailureCode>>;

    /// Consumes one transient active use of `binding`; `Cancelled` once it stopped.
    fn active(&self, binding: &BindingRef) -> Result<ActiveBindingUse, FailureCode>;

    /// Returns the daemon's single physical-effect admission controller. Lock it only in
    /// synchronous blocks: the guard must never be held across an await.
    fn admission(&self) -> Arc<Mutex<AdmissionController>>;

    /// Returns the central typed backend/view accounting.
    fn registry(&mut self) -> &mut ProviderLeaseRegistry;

    /// Mints one checked, strictly increasing protocol/backend generation for this boot.
    fn next_generation(&mut self) -> Result<u64, FailureCode>;

    /// Settles a capability-backed never-started spawn; any other failure marks `binding`
    /// uncertain.
    fn spawn_failure(&mut self, error: ProcessError, binding: &BindingRef);

    /// Records that `binding`'s physical cleanup could not be proven.
    fn mark_uncertain(&mut self, binding: &BindingRef);

    /// Allocates one physical slot for `binding` without waiting; `Capacity` when none is free.
    fn admit(&mut self, binding: &BindingRef) -> Result<AdmissionLease, FailureCode>;

    /// Maximum captured bytes per provider output stream.
    fn output_bytes(&self) -> usize;

    /// Configured absolute allowed roots.
    fn allowed_roots(&self) -> Vec<PathBuf>;

    /// Optional closed telemetry sink.
    fn telemetry(&self) -> Option<&Telemetry>;

    /// Private per-boot runtime directory for provider sockets.
    fn runtime_dir(&self) -> &Path;

    /// Per-boot random nonce, used to derive private socket names.
    fn nonce(&self) -> [u8; 32];
}

/// The one operation a backend call serves.
pub trait ProviderJob: Send + Sync {
    /// Binding the operation runs for.
    fn binding(&self) -> &BindingRef;

    /// Revocation channel; `changed()` resolves when the job is cancelled or stopped.
    fn cancel(&mut self) -> &mut watch::Receiver<bool>;

    /// Whether the job has been cancelled.
    fn cancelled(&self) -> bool;

    /// Absolute operation deadline, including time spent queued.
    fn deadline(&self) -> tokio::time::Instant;

    /// Whether the job is an edit whose written change must settle rather than restart.
    fn is_edit(&self) -> bool;

    /// Parks the job until `at`, letting the worker serve other work while a provider loads.
    fn park_until(&mut self, at: tokio::time::Instant);

    /// Attaches a human-readable refusal detail to the job's failure reply.
    fn set_failure_detail(&mut self, detail: String);

    /// Attaches the failed session stage to the job's failure reply: the default
    /// `<tool>:<reason>` tag composed with the backend-reported stage, e.g.
    /// `outline:provider_unavailable (<language>: workspace load failed; outline and read
    /// answer from source)`. The stage is closed words naming the failing step and what still
    /// answers without the server — never paths or payloads.
    fn set_stage_failure(&mut self, code: &FailureCode, stage: &str);
}

/// Produces only an Execution scope from an already fresh durable stamp; it grants nothing itself.
pub fn execution_authority(authority: &AuthorityStamp) -> Result<WorkspaceAuthority, FailureCode> {
    WorkspaceAuthority::from_workspace_with_git_common_dir(
        authority.worktree().id(),
        authority.worktree().incarnation().to_string(),
        authority.worktree().worktree_path().to_path_buf(),
        authority.worktree().git_common_dir().to_path_buf(),
        authority.epoch(),
    )
    .map_err(|_| FailureCode::WorkspaceAuthority)
}

/// Uses the opaque exact binding identity solely for resource accounting, never as host proof.
pub fn owner(binding: &BindingRef) -> Result<OwnerId, FailureCode> {
    OwnerId::new(
        blake3::Hash::from_bytes(binding.fingerprint())
            .to_hex()
            .to_string(),
    )
    .map_err(|_| FailureCode::Internal)
}

/// Carries the job's remaining lifetime into an independent bounded one-shot session policy.
pub fn remaining_options(job: &dyn ProviderJob) -> SessionOptions {
    let remaining = job
        .deadline()
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_millis(1));
    SessionOptions {
        request_timeout: remaining.min(Duration::from_secs(60)),
        lifetime: remaining.min(Duration::from_secs(300)),
    }
}

/// Maps a session's diagnostic readiness onto the closed telemetry diagnostic state.
pub fn diagnostic_state(readiness: DiagnosticReadiness) -> DiagnosticState {
    match readiness {
        DiagnosticReadiness::Clean => DiagnosticState::Clean,
        DiagnosticReadiness::Reported => DiagnosticState::Changed,
        DiagnosticReadiness::Unknown => DiagnosticState::Unavailable,
    }
}

/// Records one provider observation (cache state unavailable) when telemetry is configured.
pub fn record_provider(host: &dyn ProviderHost, language: Language, diagnostics: DiagnosticState) {
    if let Some(telemetry) = host.telemetry() {
        adapters::provider_summary(telemetry, language, CacheState::Unavailable, diagnostics);
    }
}

/// Runs one context request on a retained live session and snapshots its diagnostics.
///
/// The request races the job's cancellation. When it succeeded and `wait_diagnostics` is set, the
/// diagnostic push for the synchronized version is awaited for at most `min(3 s, remaining job
/// deadline)`, again racing cancellation (which turns the result into an error). Returns the
/// request result together with the session's diagnostics snapshot; the caller retires the
/// session on error.
pub async fn exchange_context(
    live: &mut LiveSession,
    job: &mut dyn ProviderJob,
    source: &SourceObservation,
    bytes: &[u8],
    query: ContextQuery,
    wait_diagnostics: bool,
) -> (std::io::Result<ContextResult>, DiagnosticSnapshot) {
    let mut result = {
        let operation = live.session.context(source, bytes, query);
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => result,
            _ = job.cancel().changed() => Err(std::io::Error::other("cancelled")),
        }
    };
    if result.is_ok() && wait_diagnostics {
        let budget = Duration::from_secs(3).min(
            job.deadline()
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        tokio::select! {
            _ = tokio::time::timeout(budget, live.session.wait_for_matching_diagnostics()) => {}
            _ = job.cancel().changed() => result = Err(std::io::Error::other("cancelled")),
        }
    }
    (result, live.session.diagnostics())
}

/// Returns the one canonical trust identity used by the cache key, the cache compatibility
/// identity, a shared listener's compatibility key and the shared-namespace reference count.
pub fn effective_trust(launch: &ProviderLaunch) -> String {
    launch.trust.clone()
}
