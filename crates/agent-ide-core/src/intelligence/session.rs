//! A bounded, read-only production LSP session over Execution-owned protocol pipes.

use super::{
    context::{self, ContextMode, ContextQuery, ContextResult, MAX_CONTEXT_ITEMS},
    freshness::{DiagnosticReadiness, Freshness, SourceBinding, ViewGeneration},
    wire::{WireLimits, WireSafety},
};
use crate::workspace::{
    authority::{AuthorityStamp, WorktreeRef},
    observation::SourceObservation,
};
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

/// Readiness one provider status notification reports.
///
/// Only a [`SessionProfile`] that names a [`SessionProfile::status_method`] produces these values;
/// every other profile is usable right after the handshake and never gates requests on readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderStatus {
    /// Background workspace work is still running, or the status makes no quiescence claim.
    Busy,
    /// The workspace is quiescent and the provider answers semantic requests. A degraded but
    /// usable workspace (for example some failed build scripts) also reports this.
    Ready,
    /// The workspace is quiescent but failed to load, so waiting any longer cannot make semantic
    /// operations available.
    Failed,
}

/// Language-specific behaviour of one production LSP session.
///
/// A language server module implements this for its accepted, immutable profile. The session asks
/// it for the fixed initialize payload, the accepted server identity, status-based readiness and
/// the few diagnostic-synchronization differences between servers; the transport, document
/// lifecycle and every request stay language-independent. Implementations must be cheap to query
/// and perform no I/O except where a method says so.
pub trait SessionProfile: std::any::Any + Send + Sync + std::fmt::Debug {
    /// Returns the fixed `workspace/configuration` answer, also the base of the initialization
    /// options; no dynamic settings or model keys are accepted.
    fn workspace_configuration(&self) -> serde_json::Value;

    /// Returns the initialize `initializationOptions` for a session rooted at `worktree_root`.
    ///
    /// Defaults to [`SessionProfile::workspace_configuration`]. A profile may add facts it reads
    /// from the worktree (bounded manifest discovery); that read is the only I/O allowed here.
    fn initialization_options(&self, worktree_root: &std::path::Path) -> serde_json::Value {
        let _ = worktree_root;
        self.workspace_configuration()
    }

    /// Returns whether the initialize reply's server identity matches this accepted profile.
    ///
    /// `info` is `None` when the provider omitted `serverInfo`; returning `false` fails the
    /// handshake with an invalid-data error before `initialized` is sent.
    fn accepts_server(&self, info: Option<&lsp::ServerInfo>) -> bool;

    /// Returns provider-specific `experimental` client capabilities; `None` (the default) sends
    /// none.
    fn experimental_capabilities(&self) -> Option<serde_json::Value> {
        None
    }

    /// Names the provider notification that carries readiness, or `None` (the default) when the
    /// provider is usable right after the handshake and requests never wait for readiness.
    fn status_method(&self) -> Option<&'static str> {
        None
    }

    /// Maps the params of one [`SessionProfile::status_method`] notification onto a
    /// [`ProviderStatus`]. Called only for that method; a decoding error stops the session's
    /// protocol driver exactly as a malformed typed notification does.
    fn status(&self, params: serde_json::Value) -> Result<ProviderStatus, serde_json::Error> {
        let _ = params;
        Ok(ProviderStatus::Busy)
    }

    /// Returns the ceiling of one diagnostics wait for this provider, or `None` (the default) for
    /// no provider-specific ceiling beyond the request and shutdown-reserve deadlines.
    fn diagnostic_wait_cap(&self) -> Option<Duration> {
        None
    }

    /// Whether a nonempty unversioned diagnostics push after the initial `didOpen` may stand for
    /// the opened bytes until the first `didChange`. Defaults to `false`: only versioned pushes
    /// bind a document.
    fn accepts_unversioned_initial_report(&self) -> bool {
        false
    }

    /// Returns the LSP language identifier sent with `didOpen` for `path`. Extensions the profile
    /// does not own must answer `plaintext`, so only configured provider routing adds semantics.
    fn language_id(&self, path: &std::path::Path) -> &'static str;
}

/// The accepted profile one session runs with, shared by the session and its protocol callbacks.
///
/// Cloning shares the same immutable profile; the wrapped profile never changes for the life of a
/// session.
#[derive(Clone, Debug)]
pub struct ProviderSettings(Arc<dyn SessionProfile>);

impl ProviderSettings {
    /// Wraps one accepted language-server profile for a new session.
    pub fn new(profile: impl SessionProfile) -> Self {
        Self(Arc::new(profile))
    }

    /// Returns the profile behaviour this session was opened with.
    pub fn profile(&self) -> &dyn SessionProfile {
        &*self.0
    }

    /// Returns the concrete profile when it is a `P`; `None` for any other profile type.
    pub fn downcast_ref<P: SessionProfile>(&self) -> Option<&P> {
        (&*self.0 as &dyn std::any::Any).downcast_ref::<P>()
    }

    /// Refuses a provider whose initialize identity the profile does not accept.
    fn validate_server(&self, info: Option<&lsp::ServerInfo>) -> io::Result<()> {
        if self.0.accepts_server(info) {
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
    /// This transport reported [`ProviderStatus::Ready`]: quiescent and answering definition and
    /// reference requests, exactly as it does for a human editor.
    Ready,
    /// This transport reported [`ProviderStatus::Failed`]: the workspace failed to load, so
    /// waiting any longer cannot make semantic operations available.
    WorkspaceError,
}

impl ProviderReadiness {
    /// Returns whether the trusted status route observed usable quiescence for this generation.
    pub const fn is_ready(self) -> bool {
        matches!(self.0, ReadinessState::Ready)
    }

    /// Returns whether the trusted status route reported a quiescent workspace error.
    pub const fn is_workspace_error(self) -> bool {
        matches!(self.0, ReadinessState::WorkspaceError)
    }

    /// Returns whether no accepted provider-specific readiness proof is currently retained.
    pub const fn is_unknown(self) -> bool {
        matches!(self.0, ReadinessState::Unknown)
    }

    /// Mints the readiness one accepted status notification establishes.
    const fn from_status(status: ProviderStatus) -> Self {
        match status {
            ProviderStatus::Busy => UNKNOWN_READINESS,
            ProviderStatus::Ready => ProviderReadiness(ReadinessState::Ready),
            ProviderStatus::Failed => ProviderReadiness(ReadinessState::WorkspaceError),
        }
    }
}

/// Readiness value used before or after trusted correlated provider evidence.
const UNKNOWN_READINESS: ProviderReadiness = ProviderReadiness(ReadinessState::Unknown);

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
    /// Synchronized source binding; an accepted unversioned push may carry provisional evidence.
    pub source: Option<SourceBinding>,
    /// Provider generation that received this message.
    pub generation: ViewGeneration,
    /// Provider's published document version; absent on a bound unversioned one-shot report.
    pub document_version: Option<i32>,
    /// Provisional for matching pushes, unknown for absence/invalidation; silence never implies clean.
    pub freshness: Freshness,
    /// `Clean` needs a matching version; an accepted unversioned push can only report items.
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
    /// An initial open may bind a nonempty unversioned push until didChange, when the profile
    /// accepts unversioned initial reports.
    accepts_unversioned_report: bool,
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
    /// Monotonic revision for versioned pushes or a bound nonempty unversioned push.
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
    /// Authority epoch admitted for the current source; initially the provider view's epoch.
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
    /// M-011 pilot: the external module channel; when present every read-only request is
    /// forwarded there and `server` is a closed socket.
    remote: Option<Box<super::pilot::Channel>>,
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
    let state = fresh_state(&settings, generation);
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
        remote: None,
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

/// Builds the shared router/session state for one transport generation.
fn fresh_state(settings: &ProviderSettings, generation: ViewGeneration) -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State {
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
    }))
}

/// A session whose protocol driver runs on its own task, so it outlives any single request.
///
/// The child process stays with the caller; this owns the pipes' driver and the negotiated
/// `Session`. Readiness is awaited per request with [`LiveSession::wait_ready`], never at open,
/// so a slow workspace load (a large project on a status-gated server) does not block the handshake.
pub struct LiveSession {
    /// Negotiated exclusive client; requests reset their budget per call.
    pub session: Session,
    /// The async-lsp driver; finished means the transport is gone and the session must be replaced.
    driver: tokio::task::JoinHandle<Result<(), async_lsp::Error>>,
}

impl LiveSession {
    /// Performs initialize/initialized on the given pipes and returns a long-lived session.
    ///
    /// `request_timeout` bounds every later request; the session itself has no lifetime deadline.
    pub async fn open<R, W>(
        input: R,
        output: W,
        worktree: WorktreeRef,
        epoch: u64,
        generation: ViewGeneration,
        settings: ProviderSettings,
        request_timeout: Duration,
    ) -> io::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        if epoch == 0 || request_timeout.is_zero() || request_timeout > Duration::from_secs(60) {
            return Err(context::invalid(
                "invalid session authority epoch or request deadline",
            ));
        }
        let state = fresh_state(&settings, generation);
        let router_state = state.clone();
        let (mainloop, server) = MainLoop::new_client(|_| client_router(router_state));
        let driver_state = state.clone();
        let driver_keepalive = server.clone();
        let driver = tokio::spawn(async move {
            let _keepalive = driver_keepalive;
            let result = mainloop
                .run_buffered(
                    BoundedInput::new(input, generation.backend).compat(),
                    output.compat_write(),
                )
                .await;
            driver_state.lock().expect("session lock").invalidate();
            result
        });
        let session = Session {
            server,
            worktree,
            epoch,
            generation,
            state,
            capabilities: None,
            settings,
            budget: OutboundBudget::default(),
            options: SessionOptions {
                request_timeout,
                lifetime: Duration::from_secs(300),
            },
            // No lifetime fence: a live session ends by shutdown, transport loss or reap.
            deadline: Instant::now() + Duration::from_secs(60 * 60 * 24 * 3650),
            sequence: 0,
            version: 0,
            remote: None,
        };
        let mut live = Self { session, driver };
        if let Err(error) = live.session.handshake().await {
            live.driver.abort();
            return Err(error);
        }
        Ok(live)
    }

    /// Whether the transport driver is still running; a finished driver means the server is gone.
    pub fn is_alive(&self) -> bool {
        !self.driver.is_finished() && self.session.state.lock().expect("session lock").active
    }

    /// Waits up to `budget` for the provider to become usable: a profile with a
    /// [`SessionProfile::status_method`] waits for its reported quiescence, every other provider is
    /// usable right after the handshake.
    pub async fn wait_ready(&mut self, budget: Duration) -> Result<(), ReadinessError> {
        if self.session.settings.profile().status_method().is_none() {
            return Ok(());
        }
        let mut ready = self
            .session
            .state
            .lock()
            .expect("session lock")
            .readiness
            .subscribe();
        let outcome = tokio::time::timeout(budget, async {
            loop {
                if !self.session.state.lock().expect("session lock").active {
                    return Err(ReadinessError::Gone);
                }
                let readiness = *ready.borrow_and_update();
                if readiness.is_ready() {
                    return Ok(());
                }
                if readiness.is_workspace_error() {
                    return Err(ReadinessError::WorkspaceError);
                }
                if ready.changed().await.is_err() {
                    return Err(ReadinessError::Gone);
                }
            }
        })
        .await;
        match outcome {
            Ok(result) => result,
            Err(_) => Err(ReadinessError::Loading),
        }
    }

    /// Gracefully sends shutdown/exit and reaps the process separately; reports whether shutdown
    /// completed, which lets a backend distinguish server exit failures from client teardown.
    pub async fn shutdown(mut self) -> bool {
        // A server busy loading its workspace may not answer shutdown promptly; the caller reaps
        // the process anyway, so the graceful exchange gets one second and no more.
        let completed = tokio::time::timeout(Duration::from_secs(1), self.session.shutdown())
            .await
            .is_ok_and(|result| result.is_ok());
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut self.driver).await;
        self.driver.abort();
        completed
    }
}

/// Why a live provider is not usable yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessError {
    /// Still loading the workspace; retry later, the session keeps loading in the background.
    Loading,
    /// The provider reported that the workspace failed to load.
    WorkspaceError,
    /// The transport is gone; the session must be replaced.
    Gone,
}

impl Session {
    /// Admits `observation` under its invoking actor's freshly validated durable `authority`.
    ///
    /// The worker must validate the stamp before calling. Readers may borrow a writer's
    /// transport while retaining their own epoch; the exact worktree and source epoch must match
    /// the stamp or this returns an invalid-input error without changing the session. Source
    /// bytes and sequence remain checked by each request, and provider ownership is unchanged.
    pub(crate) fn authorize_source(
        &mut self,
        authority: &AuthorityStamp,
        observation: &SourceObservation,
    ) -> io::Result<()> {
        if authority.worktree() != &self.worktree
            || observation.worktree() != authority.worktree()
            || observation.authority_epoch() != authority.epoch()
        {
            return Err(context::invalid("source does not match current authority"));
        }
        self.epoch = authority.epoch();
        Ok(())
    }

    /// Resets the outbound budget: every live request starts with a fresh allowance, while the
    /// one-shot `context` path keeps its single per-session budget.
    fn refill_budget(&mut self) {
        self.budget = OutboundBudget::default();
    }

    /// Returns the earlier of the per-exchange allowance and the session's one absolute deadline.
    fn exchange_deadline(&self) -> Instant {
        (Instant::now() + self.options.request_timeout).min(self.deadline)
    }

    /// Handshake plus, for a profile with a status notification, the readiness barrier: the
    /// one-shot session's entry point.
    async fn initialize(&mut self) -> io::Result<()> {
        self.handshake().await?;
        if self.settings.profile().status_method().is_some() {
            self.wait_for_readiness().await?;
        }
        Ok(())
    }

    /// Negotiates supported encodings and records the actual provider capability report.
    async fn handshake(&mut self) -> io::Result<()> {
        let root = lsp::Url::from_file_path(self.worktree.worktree_path())
            .map_err(|_| context::invalid("invalid worktree URI"))?;
        let reply = self
            .request::<request::Initialize>(lsp::InitializeParams {
                workspace_folders: Some(vec![lsp::WorkspaceFolder {
                    uri: root,
                    name: "workspace".into(),
                }]),
                initialization_options: Some(
                    self.settings
                        .profile()
                        .initialization_options(self.worktree.worktree_path()),
                ),
                capabilities: lsp::ClientCapabilities {
                    text_document: Some(lsp::TextDocumentClientCapabilities {
                        publish_diagnostics: Some(lsp::PublishDiagnosticsClientCapabilities {
                            related_information: Some(false),
                            ..Default::default()
                        }),
                        // Nested document symbols carry the owner/member structure the symbol
                        // paths are built from; a flat list would lose it.
                        document_symbol: Some(lsp::DocumentSymbolClientCapabilities {
                            hierarchical_document_symbol_support: Some(true),
                            ..Default::default()
                        }),
                        call_hierarchy: Some(lsp::CallHierarchyClientCapabilities {
                            dynamic_registration: Some(false),
                        }),
                        hover: Some(lsp::HoverClientCapabilities {
                            content_format: Some(vec![
                                lsp::MarkupKind::PlainText,
                                lsp::MarkupKind::Markdown,
                            ]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    workspace: Some(lsp::WorkspaceClientCapabilities {
                        configuration: Some(true),
                        workspace_folders: Some(true),
                        ..Default::default()
                    }),
                    window: Some(lsp::WindowClientCapabilities {
                        work_done_progress: Some(true),
                        ..Default::default()
                    }),
                    experimental: self.settings.profile().experimental_capabilities(),
                    general: Some(lsp::GeneralClientCapabilities {
                        position_encodings: Some(vec![
                            lsp::PositionEncodingKind::UTF8,
                            lsp::PositionEncodingKind::UTF16,
                            lsp::PositionEncodingKind::UTF32,
                        ]),
                        ..Default::default()
                    }),
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
        Ok(())
    }

    /// Waits under the request deadline for reported provider quiescence; transport loss cannot
    /// satisfy it and a quiescent workspace error fails at once instead of burning the whole deadline.
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
                let readiness = *ready.borrow_and_update();
                if readiness.is_ready() {
                    return Ok(());
                }
                if readiness.is_workspace_error() {
                    return Err(io::Error::other("provider workspace failed to load"));
                }
                ready.changed().await.map_err(io::Error::other)?;
            }
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "provider readiness barrier timed out",
            )
        })?
    }

    /// Returns the exact accepted profile settings retained by this connection.
    pub fn settings(&self) -> &ProviderSettings {
        &self.settings
    }

    /// Returns only the latest exact provider status barrier; it is separate from document
    /// diagnostics.
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

    /// Waits under the request deadline — further capped by the profile's
    /// [`SessionProfile::diagnostic_wait_cap`] — for a versioned result or a bound nonempty
    /// unversioned report, reserving two seconds of session lifetime for shutdown and EOF. Silence
    /// and empty unversioned pushes leave readiness unknown; semantic context already computed by
    /// the caller is unaffected.
    pub(crate) async fn wait_for_matching_diagnostics(&self) {
        // A pilot module already waited inside its own context exchange.
        if self.remote.is_some() {
            return;
        }
        let now = Instant::now();
        let shutdown_reserve = Duration::from_secs(2);
        if self.deadline.saturating_duration_since(now) <= shutdown_reserve {
            return;
        }
        let deadline = self
            .exchange_deadline()
            .min(self.deadline - shutdown_reserve);
        let deadline = match self.settings.profile().diagnostic_wait_cap() {
            Some(cap) => deadline.min(now + cap),
            None => deadline,
        };
        let _ = wait_for_matching_diagnostics(&self.state, deadline).await;
    }

    /// Synchronizes exact observation bytes, then requests advertised definition/reference methods.
    /// Path-only queries return file text; use `ide.symbol` or `ide.read {symbol}` for semantic operations.
    /// Unsupported or failed semantic operations return lexical context over the same bytes.
    /// Invalid source, wrong worktree/epoch, old sequence, and invalid query coordinates are rejected.
    pub async fn context(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        query: ContextQuery,
    ) -> io::Result<ContextResult> {
        if self.remote.is_some() {
            return self.remote_context(observation, bytes, query).await;
        }
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
            result.mode = ContextMode::Lexical {
                reason: "path context returns file text only; use ide.symbol or ide.read with a symbol for definitions and references".into(),
            };
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

    /// Synchronizes the source and returns the provider's document symbols (nested form).
    ///
    /// Flat responses become childless top-level symbols; container names are only hints and may
    /// be ambiguous, so the fallback does not invent a hierarchy or selection range.
    pub async fn document_symbols(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
    ) -> io::Result<Vec<lsp::DocumentSymbol>> {
        if self.remote.is_some() {
            return self
                .remote_at("document_symbols", observation, bytes, None)
                .await;
        }
        let uri = self.sync_for_request(observation, bytes).await?;
        let reply = self
            .request::<request::DocumentSymbolRequest>(lsp::DocumentSymbolParams {
                text_document: lsp::TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?;
        Ok(match reply {
            Some(lsp::DocumentSymbolResponse::Nested(symbols)) => symbols,
            Some(lsp::DocumentSymbolResponse::Flat(symbols)) => symbols
                .into_iter()
                .map(|symbol| {
                    #[allow(deprecated)]
                    lsp::DocumentSymbol {
                        name: symbol.name,
                        detail: None,
                        kind: symbol.kind,
                        tags: symbol.tags,
                        deprecated: None,
                        range: symbol.location.range,
                        selection_range: symbol.location.range,
                        children: None,
                    }
                })
                .collect(),
            None => Vec::new(),
        })
    }

    /// Hover text at a byte offset: the provider's signature/documentation rendering, if any.
    pub async fn hover(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Option<String>> {
        if self.remote.is_some() {
            return self
                .remote_at("hover", observation, bytes, Some(byte_offset))
                .await;
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        let reply = self
            .request::<request::HoverRequest>(lsp::HoverParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
            })
            .await?;
        Ok(reply.map(|hover| match hover.contents {
            lsp::HoverContents::Scalar(marked) => marked_string(marked),
            lsp::HoverContents::Array(items) => items
                .into_iter()
                .map(marked_string)
                .collect::<Vec<_>>()
                .join("\n"),
            lsp::HoverContents::Markup(markup) => markup.value,
        }))
    }

    /// Definition locations for the symbol at a byte offset.
    pub async fn definitions(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::Location>> {
        if self.remote.is_some() {
            return self
                .remote_at("definitions", observation, bytes, Some(byte_offset))
                .await;
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        let reply = self
            .request::<request::GotoDefinition>(lsp::GotoDefinitionParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?;
        Ok(context::definitions(reply).0)
    }

    /// Reference locations for the symbol at a byte offset, declaration included.
    pub async fn references(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::Location>> {
        if self.remote.is_some() {
            return self
                .remote_at("references", observation, bytes, Some(byte_offset))
                .await;
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        Ok(self
            .request::<request::References>(lsp::ReferenceParams {
                text_document_position: params,
                context: lsp::ReferenceContext {
                    include_declaration: true,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?
            .unwrap_or_default())
    }

    /// Prepares call-hierarchy items at a byte offset. An empty vector means the server found no callable; an error means preparing the request failed.
    pub async fn prepare_call_hierarchy(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::CallHierarchyItem>> {
        if self.remote.is_some() {
            return self
                .remote_at(
                    "prepare_call_hierarchy",
                    observation,
                    bytes,
                    Some(byte_offset),
                )
                .await;
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        Ok(self
            .request::<request::CallHierarchyPrepare>(lsp::CallHierarchyPrepareParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
            })
            .await?
            .unwrap_or_default())
    }

    /// Returns callers of one prepared callable; an empty vector means it has no incoming calls.
    pub async fn incoming_calls_for(
        &mut self,
        item: lsp::CallHierarchyItem,
    ) -> io::Result<Vec<lsp::CallHierarchyIncomingCall>> {
        if self.remote.is_some() {
            return self
                .remote_typed("incoming_calls_for", serde_json::json!({"item": item}))
                .await;
        }
        Ok(self
            .request::<request::CallHierarchyIncomingCalls>(lsp::CallHierarchyIncomingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?
            .unwrap_or_default())
    }

    /// Outgoing calls of one call hierarchy item, one level (what it calls).
    pub async fn outgoing_calls_for(
        &mut self,
        item: lsp::CallHierarchyItem,
    ) -> io::Result<Vec<lsp::CallHierarchyOutgoingCall>> {
        if self.remote.is_some() {
            return Err(io::Error::other("not available through the pilot module"));
        }
        Ok(self
            .request::<request::CallHierarchyOutgoingCalls>(lsp::CallHierarchyOutgoingCallsParams {
                item,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?
            .unwrap_or_default())
    }

    /// Incoming calls of the callable at a byte offset (who calls it), one level.
    pub async fn incoming_calls(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::CallHierarchyIncomingCall>> {
        if self.remote.is_some() {
            return self
                .remote_at("incoming_calls", observation, bytes, Some(byte_offset))
                .await;
        }
        let Some(item) = self
            .prepare_call_hierarchy(observation, bytes, byte_offset)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(Vec::new());
        };
        self.incoming_calls_for(item).await
    }

    /// Outgoing calls of the callable at a byte offset (what it calls), one level.
    pub async fn outgoing_calls(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<Vec<lsp::CallHierarchyOutgoingCall>> {
        if self.remote.is_some() {
            return self
                .remote_at("outgoing_calls", observation, bytes, Some(byte_offset))
                .await;
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        let items = self
            .request::<request::CallHierarchyPrepare>(lsp::CallHierarchyPrepareParams {
                text_document_position_params: params,
                work_done_progress_params: Default::default(),
            })
            .await?
            .unwrap_or_default();
        let mut calls = Vec::new();
        for item in items.into_iter().take(1) {
            calls.extend(
                self.request::<request::CallHierarchyOutgoingCalls>(
                    lsp::CallHierarchyOutgoingCallsParams {
                        item,
                        work_done_progress_params: Default::default(),
                        partial_result_params: Default::default(),
                    },
                )
                .await?
                .unwrap_or_default(),
            );
        }
        Ok(calls)
    }

    /// Project-wide symbols matching `query`, as flat symbol information.
    pub async fn workspace_symbols(
        &mut self,
        query: &str,
    ) -> io::Result<Vec<lsp::SymbolInformation>> {
        if !self.state.lock().expect("session lock").active {
            return Err(io::Error::other("provider generation unavailable"));
        }
        if self.remote.is_some() {
            return self
                .remote_typed("workspace_symbols", serde_json::json!({"query": query}))
                .await;
        }
        self.refill_budget();
        let reply = self
            .request::<request::WorkspaceSymbolRequest>(lsp::WorkspaceSymbolParams {
                query: query.to_owned(),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            })
            .await?;
        Ok(match reply {
            Some(lsp::WorkspaceSymbolResponse::Flat(symbols)) => symbols,
            Some(lsp::WorkspaceSymbolResponse::Nested(symbols)) => symbols
                .into_iter()
                .filter_map(|symbol| match symbol.location {
                    lsp::OneOf::Left(location) =>
                    {
                        #[allow(deprecated)]
                        Some(lsp::SymbolInformation {
                            name: symbol.name,
                            kind: symbol.kind,
                            tags: symbol.tags,
                            deprecated: None,
                            location,
                            container_name: symbol.container_name,
                        })
                    }
                    lsp::OneOf::Right(_) => None,
                })
                .collect(),
            None => Vec::new(),
        })
    }

    /// Asks the provider for a project-wide rename of the symbol at a byte offset; nothing is
    /// written here, the caller applies the returned edit. A pilot module session (M-011) is
    /// read-only and refuses.
    pub async fn rename(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
        new_name: &str,
    ) -> io::Result<Option<lsp::WorkspaceEdit>> {
        if self.remote.is_some() {
            return Err(io::Error::other("the pilot module is read-only"));
        }
        let params = self
            .position_params(observation, bytes, byte_offset)
            .await?;
        self.request::<request::Rename>(lsp::RenameParams {
            text_document_position: params,
            new_name: new_name.to_owned(),
            work_done_progress_params: Default::default(),
        })
        .await
    }

    /// Synchronizes one source for a request and returns its URI; rejects a stale sequence.
    async fn sync_for_request(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
    ) -> io::Result<lsp::Url> {
        if self.state.lock().expect("session lock").terminal {
            return Err(io::Error::other("LSP session is shut down"));
        }
        if observation.worktree() != &self.worktree
            || observation.authority_epoch() != self.epoch
            || observation.sequence() < self.sequence
        {
            return Err(context::invalid(
                "source does not match the current provider view",
            ));
        }
        self.refill_budget();
        self.sequence = observation.sequence();
        let text = context::observed_text(observation, bytes)?;
        if !self.state.lock().expect("session lock").active {
            return Err(io::Error::other("provider generation unavailable"));
        }
        self.synchronize(observation, text)?
            .ok_or_else(|| io::Error::other("Workspace observed a missing path"))?;
        context::observation_uri(observation)
    }

    /// Synchronizes the source and converts a byte offset into provider position parameters.
    async fn position_params(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: usize,
    ) -> io::Result<lsp::TextDocumentPositionParams> {
        let uri = self.sync_for_request(observation, bytes).await?;
        let text = context::observed_text(observation, bytes)?;
        let encoding = self.capabilities().position_encoding.clone();
        Ok(lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier { uri },
            position: context::position(text, byte_offset, &encoding)?,
        })
    }

    /// Sends didOpen, full-document didChange, or close/open while retaining one exact document.
    /// Missing observations close the document and return `None`; present sources return their
    /// monotonic version. Every identity change clears diagnostic rows and resets readiness to
    /// unknown until matching provider evidence arrives. Requires advertised
    /// synchronization; source versions never reset.
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
            state.diagnostics.readiness = DiagnosticReadiness::Unknown;
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
        let changing_document = state
            .document
            .as_ref()
            .is_some_and(|document| document.uri == uri);
        if changing_document {
            // An unversioned push after didChange cannot identify which bytes it diagnosed.
            if let Some(document) = &mut state.document {
                document.accepts_unversioned_report = false;
            }
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
            let language_id = self.settings.profile().language_id(observation.path());
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
            accepts_unversioned_report: !changing_document
                && self.settings.profile().accepts_unversioned_initial_report(),
        });
        state.diagnostics.source = None;
        state.diagnostics.document_version = None;
        state.diagnostics.freshness = Freshness::Unknown;
        state.diagnostics.readiness = DiagnosticReadiness::Unknown;
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
    /// A pilot module session (M-011) succeeds only when the module acknowledges `shutdown`.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        {
            let mut state = self.state.lock().expect("session lock");
            if !state.active || state.terminal {
                return Err(io::Error::other("LSP session is not active"));
            }
            state.terminal = true;
            state.invalidate();
        }
        // M-011 pilot: the module acknowledges `shutdown` before it stops its provider; without
        // that acknowledgement the stop is unconfirmed and only the caller's reap proves it.
        if let Some(remote) = self.remote.as_mut() {
            remote
                .call("shutdown", serde_json::json!({}), Duration::from_secs(1))
                .await?;
            self.state.lock().expect("session lock").shutdown_complete = true;
            return Ok(());
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

impl LiveSession {
    /// M-011 pilot: opens a session whose provider runs inside an external module process
    /// reached over `input`/`output` (the module's stdout/stdin; the caller owns and reaps the
    /// process). The module receives `provider` opaquely in its `hello` and answers with the
    /// capabilities of its own provider session; every later read-only request is forwarded
    /// under [`super::pilot::call_budget`], and any module fault retires this session.
    #[allow(clippy::too_many_arguments)]
    pub async fn open_remote<R, W>(
        input: R,
        output: W,
        worktree: WorktreeRef,
        epoch: u64,
        generation: ViewGeneration,
        settings: ProviderSettings,
        request_timeout: Duration,
        provider: serde_json::Value,
    ) -> io::Result<Self>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + Sync + 'static,
    {
        if epoch == 0 || request_timeout.is_zero() || request_timeout > Duration::from_secs(60) {
            return Err(context::invalid(
                "invalid session authority epoch or request deadline",
            ));
        }
        let (channel, driver) = super::pilot::Channel::spawn(input, output);
        let hello = super::pilot::AnalyzerHello {
            worktree: super::pilot::WireWorktree::of(&worktree),
            epoch,
            generation: [
                generation.backend,
                generation.configuration,
                generation.toolchain,
                generation.view,
            ],
            request_timeout_ms: request_timeout.as_millis() as u64,
            provider,
        };
        let session = Session {
            server: ServerSocket::new_closed(),
            worktree,
            epoch,
            generation,
            state: fresh_state(&settings, generation),
            capabilities: None,
            settings,
            budget: OutboundBudget::default(),
            options: SessionOptions {
                request_timeout: super::pilot::call_budget(),
                lifetime: Duration::from_secs(300),
            },
            deadline: Instant::now() + Duration::from_secs(60 * 60 * 24 * 3650),
            sequence: 0,
            version: 0,
            remote: Some(Box::new(channel)),
        };
        let mut live = Self { session, driver };
        // The module starts its provider and initializes it inside `hello`: a startup budget,
        // not the per-call one.
        let budget = request_timeout.max(live.session.options.request_timeout);
        let reply = match live.session.remote.as_mut() {
            Some(remote) => remote.call("hello", serde_json::json!(hello), budget).await,
            None => Err(io::Error::other("remote session")),
        };
        let capabilities = reply.and_then(|reply| {
            Ok(ProviderCapabilities {
                advertised: super::pilot::decode(reply["advertised"].clone())?,
                position_encoding: super::pilot::decode(reply["position_encoding"].clone())?,
                server_info: super::pilot::decode(reply["server_info"].clone())?,
            })
        });
        match capabilities.and_then(|capabilities| {
            live.session
                .settings
                .validate_server(capabilities.server_info.as_ref())?;
            Ok(capabilities)
        }) {
            Ok(capabilities) => {
                live.session.capabilities = Some(capabilities);
                Ok(live)
            }
            Err(error) => {
                live.driver.abort();
                Err(error)
            }
        }
    }
}

impl LiveSession {
    /// M-011 pilot: the fault that retired this session's module channel, if any.
    pub fn remote_fault(&self) -> Option<&str> {
        self.session
            .remote
            .as_ref()
            .and_then(|remote| remote.fault())
    }
}

impl Session {
    /// Forwards one request to the pilot module; a module fault retires this generation.
    async fn remote_call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> io::Result<serde_json::Value> {
        let budget = self.options.request_timeout;
        let remote = self
            .remote
            .as_mut()
            .ok_or_else(|| io::Error::other("not a pilot session"))?;
        let result = remote.call(method, params, budget).await;
        if remote.fault().is_some() {
            self.state.lock().expect("session lock").invalidate();
        }
        result
    }

    /// [`Session::remote_call`] decoding the result; an ill-typed reply also retires the session.
    async fn remote_typed<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> io::Result<T> {
        let value = self.remote_call(method, params).await?;
        super::pilot::decode(value).inspect_err(|error| self.remote_retire(error))
    }

    /// Retires this session for a well-framed reply it cannot accept: the generation goes
    /// inactive and the channel is poisoned with `error`, so the owner sees a module fault.
    fn remote_retire(&mut self, error: &io::Error) {
        self.state.lock().expect("session lock").invalidate();
        if let Some(remote) = self.remote.as_mut() {
            remote.poison(error.to_string());
        }
    }

    /// Applies the local fences of `sync_for_request` (shut down, worktree, epoch, sequence,
    /// exact UTF-8 bytes, live generation) before any byte leaves the core.
    fn remote_source(
        &mut self,
        observation: &SourceObservation,
        bytes: &[u8],
    ) -> io::Result<super::pilot::WireSource> {
        if self.state.lock().expect("session lock").terminal {
            return Err(io::Error::other("LSP session is shut down"));
        }
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
            return Err(io::Error::other("provider generation unavailable"));
        }
        Ok(super::pilot::WireSource::of(observation, text))
    }

    /// Forwards one source-scoped request, optionally at a byte offset.
    async fn remote_at<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        observation: &SourceObservation,
        bytes: &[u8],
        byte_offset: Option<usize>,
    ) -> io::Result<T> {
        let source = self.remote_source(observation, bytes)?;
        self.remote_typed(
            method,
            serde_json::json!({"source": source, "byte_offset": byte_offset}),
        )
        .await
    }

    /// The remote form of [`Session::context`]: the core builds the result over its own
    /// observation and overlays only the module's provider evidence, and binds the module's
    /// diagnostics to that observation only when the module saw the same sequence.
    async fn remote_context(
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
        let source = match self.remote_source(observation, bytes) {
            Ok(source) => source,
            Err(error) if error.to_string() == "provider generation unavailable" => {
                result.mode = ContextMode::Lexical {
                    reason: "provider generation unavailable".into(),
                };
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        let byte_offset = match query {
            ContextQuery::Symbol { byte_offset } => Some(byte_offset),
            ContextQuery::File => None,
        };
        let reply = self
            .remote_call(
                "context",
                serde_json::json!({"source": source, "byte_offset": byte_offset}),
            )
            .await
            .and_then(|mut reply| {
                Ok((
                    super::pilot::decode::<super::pilot::WireContext>(reply["context"].take())?,
                    super::pilot::decode::<super::pilot::WireDiagnostics>(
                        reply["diagnostics"].take(),
                    )?,
                ))
            });
        let reply = reply.and_then(|(evidence, diagnostics)| {
            evidence.apply(&mut result, self.generation)?;
            Ok(diagnostics)
        });
        if let Err(error) = &reply
            && error.kind() == io::ErrorKind::InvalidData
        {
            self.remote_retire(error);
        }
        let mut state = self.state.lock().expect("session lock");
        match reply {
            Ok(diagnostics) => diagnostics.apply(&mut state.diagnostics, observation),
            Err(error) => {
                state.diagnostics.source = None;
                state.diagnostics.document_version = None;
                state.diagnostics.freshness = Freshness::Unknown;
                state.diagnostics.readiness = DiagnosticReadiness::Unknown;
                state.diagnostics.diagnostics.clear();
                result.mode = ContextMode::Lexical {
                    reason: context::prefix(&error.to_string(), 256).into(),
                };
            }
        }
        Ok(result)
    }
}

/// Waits for correlated versioned evidence or a bound nonempty unversioned report. The revision
/// subscription precedes the snapshot check so a callback cannot be lost; silence, an empty
/// unversioned push, and stale pushes never establish readiness.
async fn wait_for_matching_diagnostics(state: &Arc<Mutex<State>>, deadline: Instant) -> bool {
    let (source, version, mut revisions) = {
        let state = state.lock().expect("session lock");
        let Some(document) = &state.document else {
            return false;
        };
        let revisions = state.diagnostic_revision.subscribe();
        if state.diagnostics.source.as_ref() == Some(&document.source)
            && (state.diagnostics.document_version == Some(document.version)
                || (state.diagnostics.document_version.is_none()
                    && state.diagnostics.readiness == DiagnosticReadiness::Reported
                    && document.accepts_unversioned_report))
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
                && (state.diagnostics.document_version == Some(version)
                    || (state.diagnostics.document_version.is_none()
                        && state.diagnostics.readiness == DiagnosticReadiness::Reported
                        && state.document.as_ref().is_some_and(|document| {
                            document.version == version && document.accepts_unversioned_report
                        })))
            {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Renders one hover marked string as plain text.
fn marked_string(marked: lsp::MarkedString) -> String {
    match marked {
        lsp::MarkedString::String(text) => text,
        lsp::MarkedString::LanguageString(code) => code.value,
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
        let settings = state
            .lock()
            .expect("session lock")
            .settings
            .profile()
            .workspace_configuration();
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
        let unversioned_report = params.version.is_none()
            && document.accepts_unversioned_report
            && !params.diagnostics.is_empty();
        if params.version.is_none()
            && document.accepts_unversioned_report
            && params.diagnostics.is_empty()
        {
            return ControlFlow::Continue(());
        }
        let binding =
            (params.version.is_some() || unversioned_report).then(|| document.source.clone());
        let document_version = document.version;
        let readiness = if params.version == Some(document_version) {
            if params.diagnostics.is_empty() {
                DiagnosticReadiness::Clean
            } else {
                DiagnosticReadiness::Reported
            }
        } else if unversioned_report {
            DiagnosticReadiness::Reported
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
        if params.version == Some(document_version) || unversioned_report {
            let revision = *state.diagnostic_revision.borrow();
            state
                .diagnostic_revision
                .send_replace(revision.wrapping_add(1));
        }
        ControlFlow::Continue(())
    });
    // The profile's own status notification (if any) is decoded by the profile; every other
    // notification stays inert.
    router.unhandled_notification(|state, notification| {
        let state = state.lock().expect("session lock");
        let profile = state.settings.profile();
        if profile.status_method() != Some(notification.method.as_str()) {
            return ControlFlow::Continue(());
        }
        match profile.status(notification.params) {
            Ok(status) => {
                if state.active {
                    state
                        .readiness
                        .send_replace(ProviderReadiness::from_status(status));
                }
                ControlFlow::Continue(())
            }
            Err(error) => ControlFlow::Break(Err(error.into())),
        }
    });
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
