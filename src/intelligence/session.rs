//! A bounded, read-only production LSP session over Execution-owned protocol pipes.

use super::{
    context::{self, ContextMode, ContextQuery, ContextResult, MAX_CONTEXT_ITEMS},
    freshness::{DiagnosticReadiness, Freshness, SourceBinding, ViewGeneration},
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
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Maximum accepted provider JSON message, validated before async-lsp deserialization.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Maximum provider header including its terminating CRLF pair.
const MAX_HEADER: usize = 4096;

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

/// Bounded diagnostic evidence; push-only results always retain unknown readiness.
#[derive(Clone, Debug)]
pub struct DiagnosticSnapshot {
    /// Exact synchronized source binding, absent for unversioned provider pushes.
    pub source: Option<SourceBinding>,
    /// Provider generation that received this message.
    pub generation: ViewGeneration,
    /// Provider's published document version; absent means uncorrelated push.
    pub document_version: Option<i32>,
    /// Provisional for matching pushes, unknown for absence/invalidation; never clean by inference.
    pub freshness: Freshness,
    /// No pull/barrier proof is implemented, so this remains `Unknown`.
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
    /// Current document; only one exact file is retained.
    document: Option<Document>,
    /// Last bounded push evidence, cleared on source changes and invalidation.
    diagnostics: DiagnosticSnapshot,
}
impl State {
    /// Retires this generation and removes diagnostic evidence that could otherwise appear usable.
    fn invalidate(&mut self) {
        self.active = false;
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
    /// Shared bounded diagnostic and liveness state.
    state: Arc<Mutex<State>>,
    /// Observed initialize response, present only after successful handshake.
    capabilities: Option<ProviderCapabilities>,
    /// Validated finite deadlines.
    options: SessionOptions,
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
    let router_state = state.clone();
    let (mainloop, server) = MainLoop::new_client(|_| client_router(router_state));
    let stop = server.clone();
    let mut session = Session {
        server,
        worktree,
        epoch,
        generation,
        state: state.clone(),
        capabilities: None,
        options,
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
    let exchange = async move {
        let result = match session.initialize().await {
            Ok(()) => operation(session).await,
            Err(error) => Err(error),
        };
        // The operation owns Session; graceful shutdown is explicit, and all paths stop the driver.
        let _ = stop.emit(Stop);
        result
    };
    let outcome =
        tokio::time::timeout(options.lifetime, async { tokio::join!(exchange, driver) }).await;
    state.lock().expect("session lock").invalidate();
    match outcome {
        Ok((result, _driver)) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "LSP session lifetime expired",
        )),
    }
}

impl Session {
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
                capabilities: lsp::ClientCapabilities {
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
        self.server
            .notify::<lsp::notification::Initialized>(lsp::InitializedParams {})
            .map_err(io::Error::other)
    }

    /// Returns only capabilities observed during this connection's successful handshake.
    pub fn capabilities(&self) -> &ProviderCapabilities {
        self.capabilities.as_ref().expect("initialized session")
    }

    /// Returns bounded diagnostic observations; no push or missing message establishes cleanliness.
    pub fn diagnostics(&self) -> DiagnosticSnapshot {
        self.state.lock().expect("session lock").diagnostics.clone()
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
                self.server
                    .notify::<lsp::notification::DidCloseTextDocument>(
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
            self.server
                .notify::<lsp::notification::DidCloseTextDocument>(
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
            self.server
                .notify::<lsp::notification::DidChangeTextDocument>(
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
            let language_id = match observation
                .path()
                .extension()
                .and_then(|extension| extension.to_str())
            {
                Some("go") => "go",
                Some("rs") => "rust",
                _ => "plaintext",
            };
            self.server
                .notify::<lsp::notification::DidOpenTextDocument>(lsp::DidOpenTextDocumentParams {
                    text_document: lsp::TextDocumentItem {
                        uri: uri.clone(),
                        language_id: language_id.into(),
                        version,
                        text: text.into(),
                    },
                })
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
        let mut guard = RequestGuard {
            state: self.state.clone(),
            server: self.server.clone(),
            completed: false,
        };
        let result = tokio::time::timeout(
            self.options.request_timeout,
            self.server.request::<R>(params),
        )
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

    /// Completes the read-only LSP shutdown/exit handshake; caller still owns child reaping.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.request::<request::Shutdown>(()).await?;
        self.server
            .notify::<lsp::notification::Exit>(())
            .map_err(io::Error::other)
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
    router.request::<request::WorkspaceConfiguration, _>(|_, params| async move {
        if params.items.len() > MAX_CONTEXT_ITEMS {
            return Err(async_lsp::ResponseError::new(
                async_lsp::ErrorCode::INVALID_PARAMS,
                "configuration item limit exceeded",
            ));
        }
        Ok(vec![serde_json::Value::Null; params.items.len()])
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
        state.diagnostics.source = binding;
        state.diagnostics.document_version = params.version;
        state.diagnostics.freshness = Freshness::Provisional;
        state.diagnostics.truncated = params.diagnostics.len() > MAX_CONTEXT_ITEMS;
        state.diagnostics.diagnostics = params
            .diagnostics
            .into_iter()
            .take(MAX_CONTEXT_ITEMS)
            .collect();
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
