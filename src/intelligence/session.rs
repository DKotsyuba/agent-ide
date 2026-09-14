//! A bounded, read-only production LSP session over Execution-owned protocol pipes.

use super::{
    context::{self, ContextMode, ContextQuery, ContextResult, MAX_CONTEXT_ITEMS},
    freshness::{DiagnosticReadiness, Freshness, SourceBinding, ViewGeneration},
    rust::RustProfile,
    wire::{WireLimits, WireSafety},
};
use crate::workspace::{authority::WorktreeRef, observation::SourceObservation};
use async_lsp::{
    MainLoop, ServerSocket,
    lsp_types::{self as lsp, request},
    router::Router,
};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    ops::ControlFlow,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Maximum accepted provider JSON message, validated before async-lsp deserialization.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Maximum provider header including its terminating CRLF pair.
const MAX_HEADER: usize = 4096;

/// Cumulative serialized client output accepted per session, including conservative envelope overhead.
const MAX_SESSION_OUTBOUND_BYTES: usize = 8 * 1024 * 1024;
/// Cumulative client messages accepted before retiring this session's unbounded async-lsp sender.
const MAX_SESSION_OUTBOUND_MESSAGES: usize = 256;

/// Per-worktree Go build/module/temp namespace delivered only through this session's view
/// configuration.
///
/// The shared listener process never receives these as process environment (see
/// `GoplsProfile::command`): its `GOPLSCACHE`/`TMPDIR` belong to the one *shared* native namespace
/// every compatible worktree uses, while `GOCACHE`/`GOMODCACHE`/`GOTMPDIR` are worktree-owned. A
/// session that cannot supply them fails closed instead of silently inheriting another worktree's
/// build cache, so the fields are private and only `GoEnv::new` can produce a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoEnv {
    /// Absolute per-worktree `GOCACHE` directory.
    go_cache: std::path::PathBuf,
    /// Absolute per-worktree `GOMODCACHE` directory.
    go_mod_cache: std::path::PathBuf,
    /// Absolute per-worktree `GOTMPDIR` directory.
    go_tmp_dir: std::path::PathBuf,
}

impl GoEnv {
    /// Accepts only three absolute, normal, non-empty per-worktree cache directories.
    ///
    /// Returns `None` for an empty, relative, or `..`-containing path: such a value would be
    /// resolved by the provider process against its own cwd and could therefore escape the private
    /// namespace this session is accounted for. Callers pass paths derived from the retained
    /// `CacheLifecycle`, which are absolute by construction, so a rejection is a real defect.
    ///
    /// This constructor only validates: it never touches the filesystem, so the three directories
    /// may still be missing. A caller whose session reaches a real provider must use
    /// [`GoEnv::prepare`] instead, which additionally creates them.
    pub fn new(
        go_cache: std::path::PathBuf,
        go_mod_cache: std::path::PathBuf,
        go_tmp_dir: std::path::PathBuf,
    ) -> Option<Self> {
        [&go_cache, &go_mod_cache, &go_tmp_dir]
            .iter()
            .all(|path| {
                path.is_absolute()
                    && path.components().all(|component| {
                        matches!(
                            component,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
            })
            .then_some(Self {
                go_cache,
                go_mod_cache,
                go_tmp_dir,
            })
    }

    /// Validates the three paths exactly like [`GoEnv::new`] and creates them on disk.
    ///
    /// Every session that is about to reach a real `gopls` view must use this constructor rather
    /// than [`GoEnv::new`]. `go` creates a missing `GOCACHE`/`GOMODCACHE` itself, but it refuses a
    /// missing `GOTMPDIR` with `creating work dir: stat <path>: no such file or directory`, which
    /// gopls reports back only as `no package metadata for file ... (jsonrpc error 0)`; the view
    /// then silently degrades to lexical context instead of failing. The namespace root retained by
    /// `CacheLifecycle` exists, but the `go-build`/`go-mod`/`tmp` directories under it are this
    /// session's own, so nothing else creates them.
    ///
    /// The directories are created recursively with owner-only `0o700` permissions, matching the
    /// private cache root they live under; an already existing directory is accepted unchanged and
    /// no file inside one is ever read or removed here. Returns `None` for a path [`GoEnv::new`]
    /// refuses and for any directory that cannot be created, because a view whose private namespace
    /// is unusable must fail closed rather than inherit another worktree's cache.
    pub fn prepare(
        go_cache: std::path::PathBuf,
        go_mod_cache: std::path::PathBuf,
        go_tmp_dir: std::path::PathBuf,
    ) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let env = Self::new(go_cache, go_mod_cache, go_tmp_dir)?;
        [&env.go_cache, &env.go_mod_cache, &env.go_tmp_dir]
            .iter()
            .all(|path| {
                path.is_dir()
                    || std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(path)
                        .is_ok()
            })
            .then_some(env)
    }

    /// Returns this view's private `GOCACHE` directory.
    pub fn go_cache(&self) -> &std::path::Path {
        &self.go_cache
    }

    /// Returns this view's private `GOMODCACHE` directory.
    pub fn go_mod_cache(&self) -> &std::path::Path {
        &self.go_mod_cache
    }

    /// Returns this view's private `GOTMPDIR` directory.
    pub fn go_tmp_dir(&self) -> &std::path::Path {
        &self.go_tmp_dir
    }
}

/// Closed provider configurations accepted by the production pipe client.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ProviderSettings {
    /// The accepted gopls configuration, carrying this session's exact private Go cache namespace;
    /// never represents a Rust profile.
    GoplsDefaults(GoEnv),
    /// Exact accepted Rust analyzer/toolchain/configuration identity retained through the session.
    Rust(RustProfile),
    /// The fixed Pyright configuration for one exclusive Python stdio session.
    Pyright(crate::intelligence::pyright::PyrightProfile),
}
impl ProviderSettings {
    /// Returns the fixed initialize/configuration payload; no dynamic settings or model keys are accepted.
    fn configuration(&self) -> serde_json::Value {
        match self {
            Self::GoplsDefaults(env) => serde_json::json!({
                "env": {
                    "GOCACHE": env.go_cache().display().to_string(),
                    "GOMODCACHE": env.go_mod_cache().display().to_string(),
                    "GOTMPDIR": env.go_tmp_dir().display().to_string(),
                }
            }),
            Self::Rust(profile) => serde_json::json!({
                "cachePriming":{"enable":false},
                "procMacro":{"enable":!profile.proc_macros_disabled()}
            }),
            Self::Pyright(_) => serde_json::json!({}),
        }
    }

    /// Refuses a Rust identity under generic defaults and requires exact analyzer identity for Rust.
    fn validate_server(&self, info: Option<&lsp::ServerInfo>) -> io::Result<()> {
        let valid = match self {
            Self::GoplsDefaults(_) => info.is_none_or(|info| info.name == "gopls"),
            Self::Rust(profile) => info.is_some_and(|info| {
                info.name == "rust-analyzer"
                    && info.version.as_deref() == Some(profile.initialize_version())
            }),
            Self::Pyright(_) => info.is_none_or(|info| info.name == "pyright"),
        };
        if valid {
            Ok(())
        } else {
            Err(context::invalid(
                "provider identity does not match its accepted settings",
            ))
        }
    }
}

/// Provider status minted only from this session's correlated transport observations.
///
/// The private representation prevents callers from constructing readiness evidence. Quiescence
/// remains separate from document diagnostics and never proves a document clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderReadiness(ReadinessState);

/// Internal readiness states accepted from the trusted protocol router.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadinessState {
    /// No exact accepted provider-specific status barrier is available.
    Unknown,
    /// This Rust transport reported both health=ok and quiescent=true.
    RustHealthyQuiescent,
}

impl ProviderReadiness {
    /// Returns whether the trusted Rust status route observed healthy quiescence for this generation.
    pub const fn is_rust_healthy_quiescent(self) -> bool {
        matches!(self.0, ReadinessState::RustHealthyQuiescent)
    }

    /// Returns whether no accepted provider-specific readiness proof is currently retained.
    pub const fn is_unknown(self) -> bool {
        matches!(self.0, ReadinessState::Unknown)
    }
}

/// Readiness value used before or after trusted correlated provider evidence.
const UNKNOWN_READINESS: ProviderReadiness = ProviderReadiness(ReadinessState::Unknown);
/// Readiness value minted only by the accepted Rust status notification callback.
const RUST_HEALTHY_QUIESCENT: ProviderReadiness =
    ProviderReadiness(ReadinessState::RustHealthyQuiescent);

/// Exact rust-analyzer status notification accepted by the versioned profile.
enum RustServerStatus {}
impl lsp::notification::Notification for RustServerStatus {
    /// Only the accepted health/quiescence fields participate in readiness.
    type Params = RustStatus;
    /// Exact versioned rust-analyzer notification name.
    const METHOD: &'static str = "experimental/serverStatus";
}

/// Bounded status fields used for the Rust readiness barrier; optional provider messages are ignored.
#[derive(serde::Deserialize, serde::Serialize)]
struct RustStatus {
    /// Whether the analyzer reports successful workspace health.
    health: RustHealth,
    /// Whether current background workspace activity is quiescent.
    quiescent: bool,
}

/// Closed health values defined by the accepted rust-analyzer status protocol.
#[derive(serde::Deserialize, serde::Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum RustHealth {
    /// Workspace health is reported as successful.
    Ok,
    /// Provider reports a warning; it cannot satisfy the ready barrier.
    Warning,
    /// Provider reports an error; it cannot satisfy the ready barrier.
    Error,
}

/// Conservative cumulative admission before async-lsp's unbounded outbound channel.
/// The allowance is deliberately never replenished; a new bounded session is needed after exhaustion.
#[derive(Default)]
struct OutboundBudget {
    /// Total serialized parameter bytes plus conservative per-message envelope overhead.
    bytes: usize,
    /// Total enqueued client requests/notifications.
    messages: usize,
}
impl OutboundBudget {
    /// Checks the entire next payload before its value can enter the unbounded sender.
    fn reserve(&mut self, method: &str, params: &impl serde::Serialize) -> io::Result<()> {
        let bytes = serde_json::to_vec(params)
            .map_err(io::Error::other)?
            .len()
            .saturating_add(method.len())
            .saturating_add(256);
        if self.messages >= MAX_SESSION_OUTBOUND_MESSAGES
            || self.bytes.saturating_add(bytes) > MAX_SESSION_OUTBOUND_BYTES
        {
            return Err(io::Error::other("session outbound budget exhausted"));
        }
        self.messages += 1;
        self.bytes += bytes;
        Ok(())
    }
}

/// Reserves client notification bytes before async-lsp may retain their serialized value.
fn send_notification<N: lsp::notification::Notification>(
    server: &ServerSocket,
    budget: &mut OutboundBudget,
    params: N::Params,
) -> io::Result<()> {
    budget.reserve(N::METHOD, &params)?;
    server.notify::<N>(params).map_err(io::Error::other)
}

/// Finite deadlines for client requests and the complete borrowed-pipe session.
#[derive(Clone, Copy, Debug)]
pub struct SessionOptions {
    /// Per-request deadline; must be positive and at most 60 seconds.
    pub request_timeout: Duration,
    /// Complete session lifetime; must be positive and at most five minutes.
    pub lifetime: Duration,
}
impl Default for SessionOptions {
    /// Provides a 30-second request deadline and a two-minute session deadline.
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(30),
            lifetime: Duration::from_secs(120),
        }
    }
}

/// Capabilities observed from this generation's initialize reply; no readiness is inferred.
#[derive(Clone, Debug)]
pub struct ProviderCapabilities {
    /// Exact standard capability report, including optional provider-specific support.
    pub advertised: lsp::ServerCapabilities,
    /// Negotiated UTF-8/16/32 coordinate encoding (UTF-16 when omitted by the provider).
    pub position_encoding: lsp::PositionEncodingKind,
    /// Initialize server identity, absent if the provider omitted it.
    pub server_info: Option<lsp::ServerInfo>,
}

/// Bounded diagnostic evidence correlated to the latest synchronized provider document.
#[derive(Clone, Debug)]
pub struct DiagnosticSnapshot {
    /// Exact synchronized source binding, absent for unversioned provider pushes.
    pub source: Option<SourceBinding>,
    /// Provider generation that received this message.
    pub generation: ViewGeneration,
    /// Provider's published document version; absent means uncorrelated push.
    pub document_version: Option<i32>,
    /// Provisional for matching pushes, unknown for absence/invalidation; silence never implies clean.
    pub freshness: Freshness,
    /// `Clean` or `Reported` only after a matching versioned provider notification.
    pub readiness: DiagnosticReadiness,
    /// At most 128 diagnostics from one accepted provider push.
    pub diagnostics: Vec<lsp::Diagnostic>,
    /// Whether the diagnostic count ceiling omitted provider items.
    pub truncated: bool,
}

/// One synchronized document; switching files closes the previous document to bound retained state.
struct Document {
    /// Exact URI sent to the provider.
    uri: lsp::Url,
    /// Immutable source identity paired with the provider version.
    source: SourceBinding,
    /// Monotonic positive version within this session, including across file switches.
    version: i32,
}

/// Shared router/session state; locked only for synchronous bounded updates, never across await.
struct State {
    /// Whether this transport generation can still accept new semantic work.
    active: bool,
    /// Explicit terminal handshake fence; later context is rejected, not synchronized.
    terminal: bool,
    /// True only after shutdown response and queued exit; permits the provider's clean EOF.
    shutdown_complete: bool,
    /// Immutable callback configuration for this exact provider profile.
    settings: ProviderSettings,
    /// Current provider-specific status, independently observable by the initialize barrier.
    readiness: watch::Sender<ProviderReadiness>,
    /// Monotonic notification revision advanced only for an accepted, versioned current-document push.
    diagnostic_revision: watch::Sender<u64>,
    /// Current document; only one exact file is retained.
    document: Option<Document>,
    /// Last bounded push evidence, cleared on source changes and invalidation.
    diagnostics: DiagnosticSnapshot,
}
impl State {
    /// Retires this generation and removes diagnostic evidence that could otherwise appear usable.
    fn invalidate(&mut self) {
        self.active = false;
        self.document = None;
        self.diagnostics.source = None;
        self.diagnostics.document_version = None;
        self.diagnostics.truncated = false;
        self.readiness.send_replace(UNKNOWN_READINESS);
        self.diagnostics.freshness = Freshness::Unknown;
        self.diagnostics.readiness = DiagnosticReadiness::Unknown;
        self.diagnostics.diagnostics.clear();
    }
}

/// Ends the async-lsp driver without introducing a second JSON-RPC multiplexer.
struct Stop;

/// A single exclusive client API; mutable methods bound outgoing requests to one at a time.
/// Construct with `with_session`; callers retain process ownership and must reap after it returns.
pub struct Session {
    /// The async-lsp socket owns request IDs and response correlation.
    server: ServerSocket,
    /// Immutable worktree incarnation supplied by the admitted provider view.
    worktree: WorktreeRef,
    /// Workspace authority epoch supplied by the admitted provider view.
    epoch: u64,
    /// Immutable backend/configuration/toolchain/view fences for this connection.
    generation: ViewGeneration,
    /// Exact closed provider settings used for initialize, callbacks, and identity checks.
    settings: ProviderSettings,
    /// Hard cumulative admission before client messages reach async-lsp.
    budget: OutboundBudget,
    /// Shared bounded diagnostic and liveness state.
    state: Arc<Mutex<State>>,
    /// Observed initialize response, present only after successful handshake.
    capabilities: Option<ProviderCapabilities>,
    /// Validated finite deadlines.
    options: SessionOptions,
    /// Single absolute lifetime fence shared by handshake, operation, shutdown, and driver drain.
    deadline: Instant,
    /// Last synchronized worktree source sequence; older observations are rejected.
    sequence: u64,
    /// Last allocated document version, retained after closing a missing document.
    version: i32,
}

/// Drives initialize/initialized, a caller operation and bounded transport cleanup.
/// Pipes may be borrowed from an Execution-owned child; this function never spawns or reaps it.
/// `worktree`, `epoch`, and `generation` must come from the caller's admitted current view. The
/// caller remains responsible for live authority checks and must cancel this future on revocation.
/// Handshake/transport/deadline errors return `io::Error`; cancellation drops the driver and all IDs.
#[allow(clippy::too_many_arguments)]
pub async fn with_session<R, W, F, Fut, T>(
    input: R,
    output: W,
    worktree: WorktreeRef,
    epoch: u64,
    generation: ViewGeneration,
    settings: ProviderSettings,
    options: SessionOptions,
    operation: F,
) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: FnOnce(Session) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    if epoch == 0
        || options.request_timeout.is_zero()
        || options.request_timeout > Duration::from_secs(60)
        || options.lifetime.is_zero()
        || options.lifetime > Duration::from_secs(300)
    {
        return Err(context::invalid(
            "invalid session authority epoch or deadline",
        ));
    }
    let state = Arc::new(Mutex::new(State {
        active: true,
        terminal: false,
        shutdown_complete: false,
        settings: settings.clone(),
        readiness: watch::channel(UNKNOWN_READINESS).0,
        diagnostic_revision: watch::channel(0).0,
        document: None,
        diagnostics: DiagnosticSnapshot {
            source: None,
            generation,
            document_version: None,
            freshness: Freshness::Unknown,
            readiness: DiagnosticReadiness::Unknown,
            diagnostics: vec![],
            truncated: false,
        },
    }));
    let deadline = Instant::now() + options.lifetime;
    let router_state = state.clone();
    let (mainloop, server) = MainLoop::new_client(|_| client_router(router_state));
    let stop = server.clone();
    let mut cleanup = SessionCleanup {
        state: state.clone(),
        server: stop.clone(),
        completed: false,
    };
    // async-lsp requires a sender to stay alive while graceful EOF is still being drained.
    let _driver_keepalive = server.clone();
    let mut session = Session {
        server,
        worktree,
        epoch,
        generation,
        state: state.clone(),
        capabilities: None,
        settings,
        budget: OutboundBudget::default(),
        options,
        deadline,
        sequence: 0,
        version: 0,
    };
    let driver_state = state.clone();
    let driver = async move {
        let result = mainloop
            .run_buffered(
                BoundedInput::new(input, generation.backend).compat(),
                output.compat_write(),
            )
            .await;
        driver_state.lock().expect("session lock").invalidate();
        result
    };
    let exchange_state = state.clone();
    let exchange = async move {
        let result = match session.initialize().await {
            Ok(()) => operation(session).await,
            Err(error) => Err(error),
        };
        // The operation owns Session; graceful shutdown is explicit, and all paths stop the driver.
        if !exchange_state
            .lock()
            .expect("session lock")
            .shutdown_complete
        {
            let _ = stop.emit(Stop);
        }
        result
    };
    let outcome = tokio::time::timeout_at(deadline, async { tokio::join!(exchange, driver) }).await;
    state.lock().expect("session lock").invalidate();
    let result = match outcome {
        Ok((result, driver)) => {
            let value = result?;
            match driver {
                Ok(()) => Ok(value),
                Err(async_lsp::Error::Eof)
                    if state.lock().expect("session lock").shutdown_complete =>
                {
                    Ok(value)
                }
                Err(error) => Err(io::Error::other(error)),
            }
        }

        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "LSP session lifetime expired",
        )),
    };
    cleanup.completed = true;
    result
}

impl Session {
    /// Returns the earlier of the per-exchange allowance and the session's one absolute deadline.
    fn exchange_deadline(&self) -> Instant {
        (Instant::now() + self.options.request_timeout).min(self.deadline)
    }

    /// Negotiates supported encodings and records the actual provider capability report.
    async fn initialize(&mut self) -> io::Result<()> {
        let root = lsp::Url::from_file_path(self.worktree.worktree_path())
            .map_err(|_| context::invalid("invalid worktree URI"))?;
        let reply = self
            .request::<request::Initialize>(lsp::InitializeParams {
                workspace_folders: Some(vec![lsp::WorkspaceFolder {
                    uri: root,
                    name: "workspace".into(),
                }]),
                initialization_options: Some(self.settings.configuration()),
                capabilities: lsp::ClientCapabilities {
                    workspace: Some(lsp::WorkspaceClientCapabilities {
                        configuration: Some(true),
                        workspace_folders: Some(true),
                        ..Default::default()
                    }),
                    window: Some(lsp::WindowClientCapabilities {
                        work_done_progress: Some(true),
                        ..Default::default()
                    }),
                    experimental: matches!(&self.settings, ProviderSettings::Rust(_))
                        .then(|| serde_json::json!({"serverStatusNotification":true})),
                    general: Some(lsp::GeneralClientCapabilities {
                        position_encodings: Some(vec![
                            lsp::PositionEncodingKind::UTF8,
                            lsp::PositionEncodingKind::UTF16,
                            lsp::PositionEncodingKind::UTF32,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await?;
        self.settings.validate_server(reply.server_info.as_ref())?;
        let encoding = reply
            .capabilities
            .position_encoding
            .clone()
            .unwrap_or(lsp::PositionEncodingKind::UTF16);
        context::position("", 0, &encoding)?;
        self.capabilities = Some(ProviderCapabilities {
            advertised: reply.capabilities,
            position_encoding: encoding,
            server_info: reply.server_info,
        });
        send_notification::<lsp::notification::Initialized>(
            &self.server,
            &mut self.budget,
            lsp::InitializedParams {},
        )
        .map_err(io::Error::other)?;
        if matches!(&self.settings, ProviderSettings::Rust(_)) {
            self.wait_for_readiness().await?;
        }
        Ok(())
    }

    /// Waits under the request deadline for exact Rust health/quiescence; transport loss cannot satisfy it.
    async fn wait_for_readiness(&mut self) -> io::Result<()> {
        let mut ready = self
            .state
            .lock()
            .expect("session lock")
            .readiness
            .subscribe();
        tokio::time::timeout_at(self.exchange_deadline(), async {
            loop {
                if !self.state.lock().expect("session lock").active {
                    return Err(io::Error::other("provider generation unavailable"));
                }
                if ready.borrow_and_update().is_rust_healthy_quiescent() {
                    return Ok(());
                }
                ready.changed().await.map_err(io::Error::other)?;
            }
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Rust readiness barrier timed out"))?
    }

    /// Returns the exact accepted profile settings retained by this connection.
    pub fn settings(&self) -> &ProviderSettings {
        &self.settings
    }

    /// Returns only the latest exact Rust status barrier; it is separate from document diagnostics.
    pub fn provider_readiness(&self) -> ProviderReadiness {
        *self.state.lock().expect("session lock").readiness.borrow()
    }

    /// Returns only capabilities observed during this connection's successful handshake.
    pub fn capabilities(&self) -> &ProviderCapabilities {
        self.capabilities.as_ref().expect("initialized session")
    }

    /// Returns bounded diagnostic observations; no push or missing message establishes cleanliness.
    pub fn diagnostics(&self) -> DiagnosticSnapshot {
        self.state.lock().expect("session lock").diagnostics.clone()
    }

    /// Waits under the current request deadline for the current document's first versioned
    /// diagnostic result. A timeout deliberately leaves diagnostic evidence unknown and does not
    /// affect already-computed semantic context.
    pub(crate) async fn wait_for_matching_diagnostics(&self) {
        let _ = wait_for_matching_diagnostics(&self.state, self.exchange_deadline()).await;
    }

    /// Synchronizes exact observation bytes, then requests advertised definition/reference methods.
    /// Unsupported or failed semantic operations return explicitly lexical context over the same bytes.
    /// Invalid source, wrong worktree/epoch, old sequence, and invalid query coordinates are rejected.
    pub async fn context(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> io::Result<ContextResult> {
        if self.state.lock().expect("session lock").terminal {
            return Err(io::Error::other("LSP session is shut down"));
        }
        let mut result =
            context::lexical_context(observation, bytes, query, "semantic operations unavailable")?;
        if observation.worktree() != &self.worktree
            || observation.authority_epoch() != self.epoch
            || observation.sequence() < self.sequence
        {
            return Err(context::invalid(
                "source does not match the current provider view",
            ));
        }
        self.sequence = observation.sequence();
        let text = context::observed_text(observation, bytes)?;
        if !self.state.lock().expect("session lock").active {
            result.mode = ContextMode::Lexical {
                reason: "provider generation unavailable".into(),
            };
            return Ok(result);
        }
        let version = match self.synchronize(observation, text) {
            Ok(version) => version,
            Err(error) => {
                self.state.lock().expect("session lock").invalidate();
                let _ = self.server.emit(Stop);
                result.mode = ContextMode::Lexical {
                    reason: context::prefix(&error.to_string(), 256).into(),
                };
                return Ok(result);
            }
        };
        result.generation = Some(self.generation);
        result.document_version = version;
        if version.is_none() {
            result.mode = ContextMode::Lexical {
                reason: "Workspace observed a missing path".into(),
            };
            return Ok(result);
        }
        let ContextQuery::Symbol { byte_offset } = query else {
            return Ok(result);
        };
        if result.symbol.is_none() {
            return Ok(result);
        }
        let capabilities = self.capabilities();
        let definition = capabilities
            .advertised
            .definition_provider
            .as_ref()
            .is_some_and(|value| !matches!(value, lsp::OneOf::Left(false)));
        let references = capabilities
            .advertised
            .references_provider
            .as_ref()
            .is_some_and(|value| !matches!(value, lsp::OneOf::Left(false)));
        let encoding = capabilities.position_encoding.clone();
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: result.uri.clone(),
            },
            position: context::position(text, byte_offset, &encoding)?,
        };
        if !definition && !references {
            return Ok(result);
        }
        let semantic = async {
            if definition {
                let reply = self
                    .request::<request::GotoDefinition>(lsp::GotoDefinitionParams {
                        text_document_position_params: params.clone(),
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    })
                    .await?;
                let (locations, truncated) = context::definitions(reply);
                result.definitions = Some(locations);
                result.truncated |= truncated;
            }
            if references {
                let mut locations = self
                    .request::<request::References>(lsp::ReferenceParams {
                        text_document_position: params,
                        context: lsp::ReferenceContext {
                            include_declaration: true,
                        },
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    })
                    .await?
                    .unwrap_or_default();
                result.truncated |= locations.len() > MAX_CONTEXT_ITEMS;
                locations.truncate(MAX_CONTEXT_ITEMS);
                result.references = Some(locations);
            }
            Ok::<_, io::Error>(())
        }
        .await;
        let semantic = if self.state.lock().expect("session lock").active {
            semantic
        } else {
            result.generation = None;
            result.document_version = None;
            Err(io::Error::other("provider generation unavailable"))
        };
        match semantic {
            Ok(()) => {
                result.mode = ContextMode::Semantic;
                result.position_encoding = encoding;
                result.lexical_matches.clear();
            }
            Err(error) => {
                result.mode = ContextMode::Lexical {
                    reason: context::prefix(&error.to_string(), 256).into(),
                };
                result.definitions = None;
                result.references = None;
            }
        }
        Ok(result)
    }

    /// Sends didOpen, full-document didChange, or close/open while retaining one exact document.
    /// Missing observations close the document and return `None`; present sources return their
    /// monotonic version. Requires advertised synchronization; source versions never reset.
    fn synchronize(
        &mut self,
        observation: &SourceObservation,
        text: &str,
    ) -> io::Result<Option<i32>> {
        let sync = self.capabilities().advertised.text_document_sync.as_ref();
        let kind = match sync {
            Some(lsp::TextDocumentSyncCapability::Kind(kind)) => *kind,
            Some(lsp::TextDocumentSyncCapability::Options(options))
                if options.open_close == Some(true) =>
            {
                options.change.unwrap_or(lsp::TextDocumentSyncKind::NONE)
            }
            _ => lsp::TextDocumentSyncKind::NONE,
        };
        if kind == lsp::TextDocumentSyncKind::NONE {
            return Err(io::Error::other(
                "provider does not support document synchronization",
            ));
        }
        let uri = context::observation_uri(observation)?;
        let binding = SourceBinding::from_observation(observation);
        let mut state = self.state.lock().expect("session lock");
        if observation.bytes().is_none() {
            if let Some(document) = state.document.take() {
                send_notification::<lsp::notification::DidCloseTextDocument>(
                    &self.server,
                    &mut self.budget,
                    lsp::DidCloseTextDocumentParams {
                        text_document: lsp::TextDocumentIdentifier { uri: document.uri },
                    },
                )
                .map_err(io::Error::other)?;
            }
            self.sequence = observation.sequence();
            state.diagnostics.source = None;
            state.diagnostics.document_version = None;
            state.diagnostics.freshness = Freshness::Unknown;
            state.diagnostics.diagnostics.clear();
            state.diagnostics.truncated = false;
            return Ok(None);
        }
        if let Some(document) = &state.document
            && document.uri == uri
            && document.source == binding
        {
            return Ok(Some(document.version));
        }
        let version = self
            .version
            .checked_add(1)
            .ok_or_else(|| io::Error::other("document version exhausted"))?;
        if let Some(document) = &state.document
            && document.uri != uri
        {
            send_notification::<lsp::notification::DidCloseTextDocument>(
                &self.server,
                &mut self.budget,
                lsp::DidCloseTextDocumentParams {
                    text_document: lsp::TextDocumentIdentifier {
                        uri: document.uri.clone(),
                    },
                },
            )
            .map_err(io::Error::other)?;
        }
        if state
            .document
            .as_ref()
            .is_some_and(|document| document.uri == uri)
        {
            send_notification::<lsp::notification::DidChangeTextDocument>(
                &self.server,
                &mut self.budget,
                lsp::DidChangeTextDocumentParams {
                    text_document: lsp::VersionedTextDocumentIdentifier {
                        uri: uri.clone(),
                        version,
                    },
                    content_changes: vec![lsp::TextDocumentContentChangeEvent {
                        range: None,
                        range_length: None,
                        text: text.into(),
                    }],
                },
            )
            .map_err(io::Error::other)?;
        } else {
            let language_id = language_id(observation.path());
            send_notification::<lsp::notification::DidOpenTextDocument>(
                &self.server,
                &mut self.budget,
                lsp::DidOpenTextDocumentParams {
                    text_document: lsp::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: language_id.into(),
                        version,
                        text: text.into(),
                    },
                },
            )
            .map_err(io::Error::other)?;
        }
        self.sequence = observation.sequence();
        self.version = version;
        state.document = Some(Document {
            uri,
            source: binding,
            version,
        });
        state.diagnostics.source = None;
        state.diagnostics.document_version = None;
        state.diagnostics.freshness = Freshness::Unknown;
        state.diagnostics.diagnostics.clear();
        state.diagnostics.truncated = false;
        Ok(Some(version))
    }

    /// Issues one typed request; timeout or caller cancellation retires this complete generation.
    /// async-lsp owns IDs; retiring its driver disposes all pending mappings and late replies.
    async fn request<R: lsp::request::Request>(
        &mut self,
        params: R::Params,
    ) -> io::Result<R::Result> {
        if !self.state.lock().expect("session lock").active {
            return Err(io::Error::other("provider generation unavailable"));
        }
        if let Err(error) = self.budget.reserve(R::METHOD, &params) {
            self.state.lock().expect("session lock").invalidate();
            let _ = self.server.emit(Stop);
            return Err(error);
        }
        let mut guard = RequestGuard {
            state: self.state.clone(),
            server: self.server.clone(),
            completed: false,
        };
        let result =
            tokio::time::timeout_at(self.exchange_deadline(), self.server.request::<R>(params))
                .await;
        match result {
            Ok(reply) => {
                guard.completed = true;
                reply.map_err(io::Error::other)
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "LSP request timed out; generation retired",
            )),
        }
    }

    /// Fences new context immediately, then completes bounded shutdown/exit; caller still owns reap.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        {
            let mut state = self.state.lock().expect("session lock");
            if !state.active || state.terminal {
                return Err(io::Error::other("LSP session is not active"));
            }
            state.terminal = true;
            state.invalidate();
        }
        let mut guard = RequestGuard {
            state: self.state.clone(),
            server: self.server.clone(),
            completed: false,
        };
        self.budget
            .reserve(<request::Shutdown as request::Request>::METHOD, &())?;
        tokio::time::timeout_at(
            self.exchange_deadline(),
            self.server.request::<request::Shutdown>(()),
        )
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "LSP shutdown timed out"))?
        .map_err(io::Error::other)?;
        send_notification::<lsp::notification::Exit>(&self.server, &mut self.budget, ())?;
        self.state.lock().expect("session lock").shutdown_complete = true;
        guard.completed = true;
        Ok(())
    }
}

/// Waits for the current document's exact versioned diagnostic snapshot without treating silence
/// as clean readiness. The revision subscription is installed before the snapshot check, so
/// accepted callback updates cannot be lost between checking state and waiting; deadline or stale
/// pushes return `false` without changing session state.
async fn wait_for_matching_diagnostics(state: &Arc<Mutex<State>>, deadline: Instant) -> bool {
    let (source, version, mut revisions) = {
        let state = state.lock().expect("session lock");
        let Some(document) = &state.document else {
            return false;
        };
        let revisions = state.diagnostic_revision.subscribe();
        if state.diagnostics.source.as_ref() == Some(&document.source)
            && state.diagnostics.document_version == Some(document.version)
        {
            return true;
        }
        (document.source.clone(), document.version, revisions)
    };
    tokio::time::timeout_at(deadline, async {
        loop {
            if revisions.changed().await.is_err() {
                return false;
            }
            let state = state.lock().expect("session lock");
            if state.diagnostics.source.as_ref() == Some(&source)
                && state.diagnostics.document_version == Some(version)
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Maps one observed filename to the fixed LSP language identifier used for document open.
/// Unknown extensions deliberately remain plaintext so only configured provider routing can add
/// semantic behavior.
fn language_id(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("go") => "go",
        Some("rs") => "rust",
        Some("py") | Some("pyi") => "python",
        _ => "plaintext",
    }
}

/// Fences cancellation even if a consumer drops a request future before its deadline.
struct RequestGuard {
    /// Generation liveness and diagnostic evidence to invalidate on cancellation.
    state: Arc<Mutex<State>>,
    /// Socket used solely to stop the corresponding async-lsp driver.
    server: ServerSocket,
    /// True only after receiving a correlated response, including a provider error response.
    completed: bool,
}

/// Stops the protocol driver and invalidates evidence when the complete session future is dropped.
struct SessionCleanup {
    /// Generation state invalidated before the transport stop request.
    state: Arc<Mutex<State>>,
    /// Socket used only to stop this session's async-lsp driver.
    server: ServerSocket,
    /// True after `with_session` has joined both exchange and driver paths.
    completed: bool,
}

impl Drop for SessionCleanup {
    /// Guarantees caller cancellation cannot leave the driver or readiness evidence active.
    fn drop(&mut self) {
        if !self.completed {
            self.state.lock().expect("session lock").invalidate();
            let _ = self.server.emit(Stop);
        }
    }
}
impl Drop for RequestGuard {
    /// Retires all IDs and evidence when a request is cancelled or times out.
    fn drop(&mut self) {
        if !self.completed {
            self.state.lock().expect("session lock").invalidate();
            let _ = self.server.emit(Stop);
        }
    }
}

/// Builds finite noninteractive callbacks; edits, settings changes and unknown commands stay inert.
fn client_router(state: Arc<Mutex<State>>) -> Router<Arc<Mutex<State>>> {
    let mut router = Router::new(state);
    router.request::<request::ApplyWorkspaceEdit, _>(|_, _| async {
        Ok(lsp::ApplyWorkspaceEditResponse {
            applied: false,
            failure_reason: Some("source writes are unavailable".into()),
            failed_change: None,
        })
    });
    router.request::<request::WorkspaceConfiguration, _>(|state, params| {
        let settings = state.lock().expect("session lock").settings.configuration();
        async move {
            if params.items.len() > MAX_CONTEXT_ITEMS {
                return Err(async_lsp::ResponseError::new(
                    async_lsp::ErrorCode::INVALID_PARAMS,
                    "configuration item limit exceeded",
                ));
            }
            Ok(vec![settings; params.items.len()])
        }
    });
    router.request::<request::WorkDoneProgressCreate, _>(|_, _| async { Ok(()) });
    router.request::<request::ShowMessageRequest, _>(|_, _| async { Ok(None) });
    router.notification::<lsp::notification::PublishDiagnostics>(|state, params| {
        let mut state = state.lock().expect("session lock");
        if !state.active {
            return ControlFlow::Continue(());
        }
        let Some(document) = &state.document else {
            return ControlFlow::Continue(());
        };
        if document.uri != params.uri
            || params
                .version
                .is_some_and(|version| version != document.version)
        {
            return ControlFlow::Continue(());
        }
        let binding = params.version.map(|_| document.source.clone());
        let document_version = document.version;
        let readiness = if params.version == Some(document_version) {
            if params.diagnostics.is_empty() {
                DiagnosticReadiness::Clean
            } else {
                DiagnosticReadiness::Reported
            }
        } else {
            DiagnosticReadiness::Unknown
        };
        state.diagnostics.source = binding;
        state.diagnostics.document_version = params.version;
        state.diagnostics.freshness = Freshness::Provisional;
        state.diagnostics.readiness = readiness;
        state.diagnostics.truncated = params.diagnostics.len() > MAX_CONTEXT_ITEMS;
        state.diagnostics.diagnostics = params
            .diagnostics
            .into_iter()
            .take(MAX_CONTEXT_ITEMS)
            .collect();
        if params.version == Some(document_version) {
            let revision = *state.diagnostic_revision.borrow();
            state
                .diagnostic_revision
                .send_replace(revision.wrapping_add(1));
        }
        ControlFlow::Continue(())
    });
    router.notification::<RustServerStatus>(|state, status| {
        let state = state.lock().expect("session lock");
        if state.active && matches!(&state.settings, ProviderSettings::Rust(_)) {
            state
                .readiness
                .send_replace(if status.health == RustHealth::Ok && status.quiescent {
                    RUST_HEALTHY_QUIESCENT
                } else {
                    UNKNOWN_READINESS
                });
        }
        ControlFlow::Continue(())
    });
    router.unhandled_notification(|_, _| ControlFlow::Continue(()));
    router.event(|_, _: Stop| ControlFlow::Break(Ok(())));
    router
}

/// Validates complete bounded frames before exposing any byte to async-lsp's JSON decoder.
struct BoundedInput<R> {
    /// Execution-owned or borrowed provider stdout reader.
    inner: R,
    /// Existing production framing boundary; malformed frames invalidate its generation.
    safety: WireSafety,
    /// Validated output awaiting consumption, bounded by one read chunk plus one complete frame.
    ready: VecDeque<u8>,
}
impl<R> BoundedInput<R> {
    /// Wraps a reader with an 8 MiB body, 4 KiB header and one-request profile.
    fn new(inner: R, generation: u64) -> Self {
        Self {
            inner,
            safety: WireSafety::new(
                WireLimits::new(MAX_HEADER, MAX_BODY, 1, MAX_BODY + MAX_HEADER + 8192)
                    .expect("finite wire limits"),
                generation,
            ),
            ready: VecDeque::new(),
        }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for BoundedInput<R> {
    /// Reads and validates fragmented/coalesced frames; malformed envelopes fail before JSON parsing.
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if !self.ready.is_empty() {
                let count = output.remaining().min(self.ready.len());
                output.put_slice(&self.ready.make_contiguous()[..count]);
                self.ready.drain(..count);
                return Poll::Ready(Ok(()));
            }
            let mut bytes = [0; 8192];
            let mut input = ReadBuf::new(&mut bytes);
            match Pin::new(&mut self.inner).poll_read(cx, &mut input) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
            if input.filled().is_empty() {
                self.safety.eof();
                return Poll::Ready(Ok(()));
            }
            match self.safety.ingest(input.filled()) {
                Ok(frames) => {
                    for frame in frames {
                        self.ready.extend(frame);
                    }
                }
                Err(error) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid LSP frame: {error:?}"),
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
