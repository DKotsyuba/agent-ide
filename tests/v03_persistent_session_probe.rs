//! v0.3 feasibility probe P2: whether `src/intelligence/session.rs`'s bounded `Session` can become
//! a persistent (warm) language-server connection.
//!
//! The `probe` module below is an isolated prototype built directly on `async-lsp`; it is not a
//! rewrite of `crate::intelligence::session::Session` and is never wired into the product. It only
//! demonstrates, against a deterministic fake LSP peer, the accounting shapes a warm session needs
//! instead of the production one: per-request (in-flight) admission instead of a cumulative,
//! never-replenished outbound budget; a bounded admission queue that fails closed with a typed
//! error instead of growing memory; cancellation fenced by a generation counter instead of
//! retiring the whole connection; and tracking more than one open document at a time.
//!
//! The real pinned-Pyright warm run at the bottom of this file drives the process directly with a
//! minimal JSON-RPC client (also built on `async-lsp`, bypassing `with_session` entirely) because
//! the production `Session` cannot outlive its 300-second lifetime cap without a code change. See
//! `docs/evidence/v03-warm-session-probe.md` for findings, measurements and the verdict.

use async_lsp::{
    MainLoop, ServerSocket,
    lsp_types::{self as lsp, request},
    router::Router,
};
use std::{
    collections::HashMap,
    future::Future,
    ops::ControlFlow,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Isolated persistent-session prototype: per-request budget, generation fencing, multi-document.
mod probe {
    use super::*;

    /// Returned by [`InFlightBudget::try_admit`] when the concurrent-request cap is already full.
    #[derive(Debug)]
    pub struct Overload;

    /// Bounded concurrent-admission counter: caps requests in flight, not cumulative session traffic.
    ///
    /// Unlike the production `OutboundBudget` (cumulative, deliberately never replenished — see
    /// `src/intelligence/session.rs:264-289`), a permit here returns to the pool as soon as its
    /// request completes, so a session can serve unbounded sequential traffic without ever being
    /// retired.
    pub struct InFlightBudget {
        /// Maximum concurrently admitted requests.
        cap: usize,
        /// Currently admitted request count.
        current: AtomicUsize,
    }

    impl InFlightBudget {
        /// Creates a shared budget that admits at most `cap` concurrent requests.
        pub fn new(cap: usize) -> Arc<Self> {
            Arc::new(Self {
                cap,
                current: AtomicUsize::new(0),
            })
        }

        /// Admits one request if under the cap, else fails closed with [`Overload`] instead of
        /// queuing it: the caller's memory never grows past `cap` in-flight requests.
        pub fn try_admit(self: &Arc<Self>) -> Result<InFlightGuard, Overload> {
            loop {
                let current = self.current.load(Ordering::Acquire);
                if current >= self.cap {
                    return Err(Overload);
                }
                if self
                    .current
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Ok(InFlightGuard {
                        budget: self.clone(),
                    });
                }
            }
        }

        /// Returns the number of requests currently admitted.
        pub fn in_flight(&self) -> usize {
            self.current.load(Ordering::Acquire)
        }
    }

    /// Releases one in-flight admission slot when its request finishes, errors, or is dropped.
    pub struct InFlightGuard {
        /// Budget this guard was admitted from.
        budget: Arc<InFlightBudget>,
    }

    impl Drop for InFlightGuard {
        /// Returns the slot to the pool so an equivalent future request can be admitted.
        fn drop(&mut self) {
            self.budget.current.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Monotonic session generation; bumping it fences any request already in flight.
    #[derive(Clone)]
    pub struct Generation(Arc<AtomicU64>);

    impl Generation {
        /// Starts a new generation counter at zero.
        pub fn new() -> Self {
            Self(Arc::new(AtomicU64::new(0)))
        }

        /// Returns the current generation value.
        pub fn current(&self) -> u64 {
            self.0.load(Ordering::Acquire)
        }

        /// Advances the generation, as a document version bump or an explicit cancel would.
        pub fn bump(&self) -> u64 {
            self.0.fetch_add(1, Ordering::AcqRel) + 1
        }
    }

    /// Multi-document, in-flight-budgeted persistent session prototype over one async-lsp socket.
    ///
    /// Contrast with `crate::intelligence::session::Session`: that type retains exactly one
    /// `Document` (`src/intelligence/session.rs:374-375`) and is constructed fresh per bounded
    /// `with_session` call (`src/assistance/providers.rs:1740-1760`); this prototype keeps a map of
    /// open documents and is meant to be constructed once and reused across many operations.
    pub struct PersistentProbeSession {
        /// Underlying async-lsp client socket to the long-lived provider peer.
        server: ServerSocket,
        /// Concurrent request admission, replacing the production cumulative budget.
        budget: Arc<InFlightBudget>,
        /// Fences stale results from publishing after a cancel or document version bump.
        generation: Generation,
        /// Open documents and their last sent version, unlike production's single `Document`.
        documents: HashMap<lsp::Url, i32>,
    }

    impl PersistentProbeSession {
        /// Wraps a socket with a bounded in-flight budget and a fresh generation/document map.
        pub fn new(server: ServerSocket, concurrent_cap: usize) -> Self {
            Self {
                server,
                budget: InFlightBudget::new(concurrent_cap),
                generation: Generation::new(),
                documents: HashMap::new(),
            }
        }

        /// Returns the generation fence this session currently uses to reject stale results.
        pub fn generation(&self) -> Generation {
            self.generation.clone()
        }

        /// Returns the number of requests this session currently has admitted and in flight.
        pub fn in_flight(&self) -> usize {
            self.budget.in_flight()
        }

        /// Issues one round trip, admitting it under the in-flight budget and fencing its result by
        /// generation: a reply that outlives a generation bump on this session returns `Ok(None)`
        /// instead of publishing stale data. Returns [`Overload`] immediately, without ever reaching
        /// the network, once the concurrent cap is exhausted.
        pub async fn hover(
            &self,
            uri: lsp::Url,
        ) -> Result<Result<Option<lsp::Hover>, async_lsp::Error>, Overload> {
            let _permit = self.budget.try_admit()?;
            let sent_generation = self.generation.current();
            let reply = self
                .server
                .request::<lsp::request::HoverRequest>(lsp::HoverParams {
                    text_document_position_params: lsp::TextDocumentPositionParams {
                        text_document: lsp::TextDocumentIdentifier { uri },
                        position: lsp::Position::new(0, 0),
                    },
                    work_done_progress_params: Default::default(),
                })
                .await;
            if self.generation.current() != sent_generation {
                return Ok(Ok(None));
            }
            Ok(reply)
        }

        /// Opens a new document or replaces the content of one already open, tracking each URI's
        /// monotonic version independently so more than one document can stay open at once.
        pub fn open_or_change(&mut self, uri: lsp::Url, text: &str) -> std::io::Result<()> {
            match self.documents.get_mut(&uri) {
                Some(version) => {
                    *version += 1;
                    self.server
                        .notify::<lsp::notification::DidChangeTextDocument>(
                            lsp::DidChangeTextDocumentParams {
                                text_document: lsp::VersionedTextDocumentIdentifier {
                                    uri,
                                    version: *version,
                                },
                                content_changes: vec![lsp::TextDocumentContentChangeEvent {
                                    range: None,
                                    range_length: None,
                                    text: text.into(),
                                }],
                            },
                        )
                        .map_err(std::io::Error::other)
                }
                None => {
                    self.documents.insert(uri.clone(), 1);
                    self.server
                        .notify::<lsp::notification::DidOpenTextDocument>(
                            lsp::DidOpenTextDocumentParams {
                                text_document: lsp::TextDocumentItem {
                                    uri,
                                    language_id: "plaintext".into(),
                                    version: 1,
                                    text: text.into(),
                                },
                            },
                        )
                        .map_err(std::io::Error::other)
                }
            }
        }

        /// Closes one document, removing it from this session's multi-document map.
        pub fn close(&mut self, uri: lsp::Url) -> std::io::Result<()> {
            self.documents.remove(&uri);
            self.server
                .notify::<lsp::notification::DidCloseTextDocument>(
                    lsp::DidCloseTextDocumentParams {
                        text_document: lsp::TextDocumentIdentifier { uri },
                    },
                )
                .map_err(std::io::Error::other)
        }

        /// Returns the number of documents currently tracked open in this session.
        pub fn open_document_count(&self) -> usize {
            self.documents.len()
        }
    }
}

/// Spins up a fake LSP peer over an in-memory duplex pipe: replies to `Hover` via `responder` and
/// appends one line per open/change/close notification it receives to `log`.
fn spawn_fake_peer<F, Fut>(
    log: Arc<Mutex<Vec<String>>>,
    responder: F,
) -> (
    tokio::io::DuplexStream,
    tokio::task::JoinHandle<Result<(), async_lsp::Error>>,
)
where
    F: Fn(lsp::HoverParams) -> Fut + Send + 'static,
    Fut: Future<Output = Result<Option<lsp::Hover>, async_lsp::ResponseError>> + Send + 'static,
{
    let (client, peer) = tokio::io::duplex(65536);
    let (peer_input, peer_output) = tokio::io::split(peer);
    let (server, keepalive) = MainLoop::new_server(move |_client| {
        let mut router = Router::new(());
        router.request::<request::HoverRequest, _>(move |_, params| responder(params));
        let open_log = log.clone();
        router.notification::<lsp::notification::DidOpenTextDocument>(move |_, params| {
            open_log
                .lock()
                .unwrap()
                .push(format!("open {}", params.text_document.uri));
            ControlFlow::Continue(())
        });
        let change_log = log.clone();
        router.notification::<lsp::notification::DidChangeTextDocument>(move |_, params| {
            change_log
                .lock()
                .unwrap()
                .push(format!("change {}", params.text_document.uri));
            ControlFlow::Continue(())
        });
        let close_log = log.clone();
        router.notification::<lsp::notification::DidCloseTextDocument>(move |_, params| {
            close_log
                .lock()
                .unwrap()
                .push(format!("close {}", params.text_document.uri));
            ControlFlow::Continue(())
        });
        router.unhandled_notification(|_, _| ControlFlow::Continue(()));
        router
    });
    // async-lsp's driver loop panics ("Sender is alive") once every clone of the peer socket it
    // pairs with is dropped, even mid-run; this keepalive must outlive the spawned driver task.
    let handle = tokio::spawn(async move {
        let _keepalive = keepalive;
        server
            .run_buffered(peer_input.compat(), peer_output.compat_write())
            .await
    });
    (client, handle)
}

/// Drives an async-lsp client mainloop over one duplex half and returns the socket to issue calls.
fn spawn_client<R, W>(
    input: R,
    output: W,
) -> (
    ServerSocket,
    tokio::task::JoinHandle<Result<(), async_lsp::Error>>,
)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mainloop, server) = MainLoop::new_client(|_| Router::new(()));
    let handle = tokio::spawn(mainloop.run_buffered(input.compat(), output.compat_write()));
    (server, handle)
}

/// (a) Proves a warm session can serve more sequential round trips than the production cumulative
/// cap (`MAX_SESSION_OUTBOUND_MESSAGES = 256`, `src/intelligence/session.rs:38`) without its
/// per-request in-flight budget ever being exhausted, because each permit is released as soon as
/// its own request completes.
#[tokio::test]
async fn sequential_round_trips_exceed_the_production_message_cap_without_exhausting_the_budget() {
    let (peer_client, peer_driver) =
        spawn_fake_peer(Arc::new(Mutex::new(Vec::new())), |_| async { Ok(None) });
    let (input, output) = tokio::io::split(peer_client);
    let (server, client_driver) = spawn_client(input, output);
    let session = probe::PersistentProbeSession::new(server, 8);
    let uri = lsp::Url::parse("file:///probe/a.rs").unwrap();

    const ROUNDS: usize = 300;
    for _ in 0..ROUNDS {
        session
            .hover(uri.clone())
            .await
            .expect("in-flight budget must not exhaust across sequential rounds")
            .expect("peer reply");
    }
    assert_eq!(
        session.in_flight(),
        0,
        "every admitted permit must be released after its own request completes"
    );

    peer_driver.abort();
    client_driver.abort();
}

/// (b) Proves a peer that never responds hits a bounded admission cap and returns a typed
/// [`probe::Overload`] instead of letting concurrent requests accumulate without limit.
#[tokio::test]
async fn a_blocked_peer_hits_the_bounded_admission_cap_instead_of_growing_memory() {
    let received = Arc::new(AtomicUsize::new(0));
    let (peer_client, peer_driver) = spawn_fake_peer(Arc::new(Mutex::new(Vec::new())), {
        let received = received.clone();
        move |_| {
            let received = received.clone();
            async move {
                received.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                unreachable!("the peer never replies in this test")
            }
        }
    });
    let (input, output) = tokio::io::split(peer_client);
    let (server, client_driver) = spawn_client(input, output);
    let session = Arc::new(probe::PersistentProbeSession::new(server, 4));
    let uri = lsp::Url::parse("file:///probe/b.rs").unwrap();

    let mut in_flight = Vec::new();
    for _ in 0..4 {
        let session = session.clone();
        let uri = uri.clone();
        in_flight.push(tokio::spawn(async move { session.hover(uri).await }));
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while received.load(Ordering::SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the peer must observe all four admitted requests");
    assert_eq!(session.in_flight(), 4);

    let overload = session.hover(uri).await;
    assert!(
        matches!(overload, Err(probe::Overload)),
        "the 5th concurrent request must fail closed instead of queuing"
    );

    for handle in in_flight {
        handle.abort();
    }
    peer_driver.abort();
    client_driver.abort();
}

/// (c) Proves a request whose generation is bumped (simulating a document version bump or an
/// explicit cancel) while it is still in flight cannot publish its late result once it arrives.
#[tokio::test]
async fn a_cancelled_request_cannot_publish_after_a_generation_change() {
    let (received_tx, received_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let received_tx = Arc::new(Mutex::new(Some(received_tx)));
    let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
    let (peer_client, peer_driver) = spawn_fake_peer(Arc::new(Mutex::new(Vec::new())), move |_| {
        let received_tx = received_tx.clone();
        let release_rx = release_rx.clone();
        async move {
            if let Some(tx) = received_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if let Some(rx) = release_rx.lock().await.take() {
                let _ = rx.await;
            }
            Ok(None)
        }
    });
    let (input, output) = tokio::io::split(peer_client);
    let (server, client_driver) = spawn_client(input, output);
    let session = probe::PersistentProbeSession::new(server, 4);
    let generation = session.generation();
    let uri = lsp::Url::parse("file:///probe/c.rs").unwrap();

    let hover = tokio::spawn(async move { session.hover(uri).await });
    received_rx
        .await
        .expect("the peer must observe the request before the generation moves on");
    generation.bump();
    release_tx.send(()).unwrap();

    let result = hover.await.unwrap().expect("request was admitted");
    assert!(
        matches!(result, Ok(None)),
        "a stale reply must not publish after the generation moved on: {result:?}"
    );

    peer_driver.abort();
    client_driver.abort();
}

/// (d) Proves one session can hold more than one open document at a time and independently
/// open/change/close each of them, unlike production's single retained `Document`.
#[tokio::test]
async fn one_session_tracks_more_than_one_open_document_at_once() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let (peer_client, peer_driver) = spawn_fake_peer(log.clone(), |_| async { Ok(None) });
    let (input, output) = tokio::io::split(peer_client);
    let (server, client_driver) = spawn_client(input, output);
    let mut session = probe::PersistentProbeSession::new(server, 4);

    let a = lsp::Url::parse("file:///probe/a.py").unwrap();
    let b = lsp::Url::parse("file:///probe/b.py").unwrap();
    let c = lsp::Url::parse("file:///probe/c.py").unwrap();

    session.open_or_change(a.clone(), "a = 1").unwrap();
    session.open_or_change(b.clone(), "b = 1").unwrap();
    session.open_or_change(c.clone(), "c = 1").unwrap();
    assert_eq!(
        session.open_document_count(),
        3,
        "production Session retains only one document at a time"
    );

    session.open_or_change(a.clone(), "a = 2").unwrap();
    session.close(b).unwrap();
    assert_eq!(session.open_document_count(), 2);

    session.open_or_change(c.clone(), "c = 2").unwrap();
    session.close(a).unwrap();
    session.close(c).unwrap();
    assert_eq!(session.open_document_count(), 0);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let seen = log.lock().unwrap().len();
            if seen >= 8 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the peer must observe every buffered notification");
    let log = log.lock().unwrap();
    assert_eq!(
        log.iter().filter(|line| line.starts_with("open ")).count(),
        3
    );
    assert_eq!(
        log.iter()
            .filter(|line| line.starts_with("change "))
            .count(),
        2
    );
    assert_eq!(
        log.iter().filter(|line| line.starts_with("close ")).count(),
        3
    );
    drop(log);

    peer_driver.abort();
    client_driver.abort();
}

/// Builds the client-side router for the real pinned-Pyright connection: answers the handful of
/// server-initiated requests Pyright issues during a normal session and counts `publishDiagnostics`
/// pushes. Mirrors `crate::intelligence::session::client_router`'s handled methods but is a fresh,
/// production-independent implementation.
fn diagnostics_router(diagnostics_revision: Arc<AtomicU64>) -> Router<()> {
    let mut router = Router::new(());
    router.request::<request::WorkspaceConfiguration, _>(|_, params| async move {
        Ok(vec![serde_json::json!({}); params.items.len()])
    });
    router.request::<request::RegisterCapability, _>(|_, _| async { Ok(()) });
    router.request::<request::WorkDoneProgressCreate, _>(|_, _| async { Ok(()) });
    router.request::<request::ShowMessageRequest, _>(|_, _| async { Ok(None) });
    router.request::<request::ApplyWorkspaceEdit, _>(|_, _| async {
        Ok(lsp::ApplyWorkspaceEditResponse {
            applied: false,
            failure_reason: Some("probe client does not apply edits".into()),
            failed_change: None,
        })
    });
    router.notification::<lsp::notification::PublishDiagnostics>(move |_, _| {
        diagnostics_revision.fetch_add(1, Ordering::AcqRel);
        ControlFlow::Continue(())
    });
    router.unhandled_notification(|_, _| ControlFlow::Continue(()));
    router
}

/// Waits until at least one `publishDiagnostics` push has been observed after this call started.
async fn wait_for_next_diagnostics(
    revision: &AtomicU64,
    timeout: Duration,
) -> Result<(), tokio::time::error::Elapsed> {
    let seen = revision.load(Ordering::Acquire);
    tokio::time::timeout(timeout, async {
        while revision.load(Ordering::Acquire) == seen {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
}

/// Reads one process's resident set size in kilobytes via `ps`, for coarse warm/cold comparison.
fn rss_kb(pid: u32) -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss="])
        .arg("-p")
        .arg(pid.to_string())
        .output()
        .expect("ps must run on this platform");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// (Q3) Real warm check: keeps one pinned `pyright-langserver` process alive for about 6 minutes —
/// longer than the production 300-second `SessionOptions::lifetime` cap
/// (`src/intelligence/session.rs:307-308,451-452`) — and issues a `didChange` plus a diagnostics
/// wait every 60 seconds on a small fixture, recording first-diagnostics latency and RSS at each
/// step. Bypasses `with_session` entirely and drives the process with this file's own minimal
/// JSON-RPC client, because the production `Session` cannot exceed its lifetime cap without a code
/// change.
#[tokio::test]
#[ignore = "spawns a real pyright-langserver process and holds it warm for several minutes"]
async fn real_pyright_process_stays_warm_past_the_production_lifetime_cap() {
    let node = std::env::var("AGENT_IDE_NODE")
        .expect("AGENT_IDE_NODE is required for this real pinned-Pyright check");
    let pyright = std::env::var("AGENT_IDE_PYRIGHT")
        .expect("AGENT_IDE_PYRIGHT is required for this real pinned-Pyright check");

    let root = std::env::temp_dir().join(format!("agent-ide-warm-probe-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("probe.py");
    std::fs::write(&file, "def value() -> int:\n    return 1\n").unwrap();

    let mut child = tokio::process::Command::new(&node)
        .arg(&pyright)
        .arg("--stdio")
        .current_dir(&root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the pinned pyright-langserver");
    let pid = child.id().expect("pyright-langserver pid");
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    let diagnostics_revision = Arc::new(AtomicU64::new(0));
    let (mainloop, server) = MainLoop::new_client({
        let diagnostics_revision = diagnostics_revision.clone();
        move |_| diagnostics_router(diagnostics_revision.clone())
    });
    let driver = tokio::spawn(mainloop.run_buffered(stdout.compat(), stdin.compat_write()));

    let root_uri = lsp::Url::from_file_path(&root).unwrap();
    server
        .request::<request::Initialize>(lsp::InitializeParams {
            workspace_folders: Some(vec![lsp::WorkspaceFolder {
                uri: root_uri,
                name: "workspace".into(),
            }]),
            capabilities: lsp::ClientCapabilities {
                workspace: Some(lsp::WorkspaceClientCapabilities {
                    configuration: Some(true),
                    workspace_folders: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .expect("pyright-langserver must initialize");
    server
        .notify::<lsp::notification::Initialized>(lsp::InitializedParams {})
        .expect("initialized notification");

    let file_uri = lsp::Url::from_file_path(&file).unwrap();
    let mut version = 1;
    let mut measurements: Vec<(&'static str, Duration, u64)> = Vec::new();

    let text = std::fs::read_to_string(&file).unwrap();
    let cold_start = tokio::time::Instant::now();
    server
        .notify::<lsp::notification::DidOpenTextDocument>(lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri: file_uri.clone(),
                language_id: "python".into(),
                version,
                text,
            },
        })
        .expect("didOpen");
    wait_for_next_diagnostics(&diagnostics_revision, Duration::from_secs(90))
        .await
        .expect("cold diagnostics never arrived");
    measurements.push(("cold-open", cold_start.elapsed(), rss_kb(pid)));

    for lap in 0..6u32 {
        tokio::time::sleep(Duration::from_secs(60)).await;
        version += 1;
        let flipped = if lap % 2 == 0 {
            "def value() -> int:\n    return \"bad\"\n"
        } else {
            "def value() -> int:\n    return 1\n"
        };
        let start = tokio::time::Instant::now();
        server
            .notify::<lsp::notification::DidChangeTextDocument>(lsp::DidChangeTextDocumentParams {
                text_document: lsp::VersionedTextDocumentIdentifier {
                    uri: file_uri.clone(),
                    version,
                },
                content_changes: vec![lsp::TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: flipped.into(),
                }],
            })
            .expect("didChange");
        wait_for_next_diagnostics(&diagnostics_revision, Duration::from_secs(30))
            .await
            .expect("warm diagnostics never arrived");
        measurements.push(("warm", start.elapsed(), rss_kb(pid)));
    }

    for (label, elapsed, rss) in &measurements {
        println!("v0.3 warm probe: {label} latency={elapsed:?} rss_kb={rss}");
    }
    assert!(
        measurements.len() >= 7,
        "expected the cold open plus six warm laps"
    );

    let _ = child.start_kill();
    driver.abort();
    let _ = std::fs::remove_dir_all(&root);
}
