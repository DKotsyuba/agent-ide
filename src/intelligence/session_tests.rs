//! Focused production-client tests using exact Workspace observations and controlled server peers.

use super::*;
use crate::workspace::observation::{
    ObservationRef, ObservedState, SourceBytes, SourceCoverage, SourceRevision,
};
use async_lsp::LspService;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Builds a canonical in-memory worktree identity without consulting mutable source files.
fn tree() -> WorktreeRef {
    WorktreeRef::from_discovery(
        "/tmp/context test 🦀".into(),
        "/tmp/context test 🦀".into(),
        ".git".into(),
        1,
    )
    .unwrap()
}

/// Constructs Workspace-owned observation facts for exact test bytes at a monotonic sequence.
fn observation(text: &str, sequence: u64) -> SourceObservation {
    SourceObservation::new(
        tree(),
        1,
        sequence,
        ObservationRef::new(format!("source-{sequence}")).unwrap(),
        "main.go".into(),
        Some(SourceBytes::from_bytes(text.as_bytes())),
        SourceRevision::new(format!("revision-{sequence}")).unwrap(),
        SourceCoverage::Complete,
        ObservedState::Present,
    )
    .unwrap()
}

/// Checks raw URI encoding, Unicode coordinate units, invalid offsets, exact bytes, and lexical limits.
#[test]
fn exact_lexical_context_and_positions() {
    let text = "// 🦀\nfunc Hello() { Hello() }\n";
    let observed = observation(text, 1);
    let result = context::lexical_context(
        &observed,
        text.as_bytes(),
        ContextQuery::Symbol {
            byte_offset: text.find("Hello").unwrap(),
        },
        "offline",
    )
    .unwrap();
    assert_eq!(result.lexical_matches.len(), 2);
    assert!(
        result
            .uri
            .as_str()
            .contains("context%20test%20%F0%9F%A6%80")
    );
    assert!(result.definitions.is_none());
    assert_eq!(
        context::position("🦀a", 4, &lsp::PositionEncodingKind::UTF16).unwrap(),
        lsp::Position::new(0, 2)
    );
    assert_eq!(
        context::position("🦀a", 4, &lsp::PositionEncodingKind::UTF8).unwrap(),
        lsp::Position::new(0, 4)
    );
    assert_eq!(
        context::position("🦀a", 4, &lsp::PositionEncodingKind::UTF32).unwrap(),
        lsp::Position::new(0, 1)
    );
    assert!(context::position("🦀", 1, &lsp::PositionEncodingKind::UTF16).is_err());
    assert!(
        context::lexical_context(&observed, b"different", ContextQuery::File, "offline").is_err()
    );
    let large = "word ".repeat(20_000);
    let result = context::lexical_context(
        &observation(&large, 2),
        large.as_bytes(),
        ContextQuery::Symbol { byte_offset: 0 },
        "offline",
    )
    .unwrap();
    assert!(result.truncated);
    assert_eq!(result.lexical_matches.len(), MAX_CONTEXT_ITEMS);
    assert_eq!(result.text.len(), context::MAX_CONTEXT_BYTES);
}

/// Creates bounded router state with one synchronized document and unknown diagnostics.
fn diagnostic_state() -> Arc<Mutex<State>> {
    let observed = observation("package main", 1);
    Arc::new(Mutex::new(State {
        active: true,
        terminal: false,
        shutdown_complete: false,
        settings: ProviderSettings::GoplsDefaults,
        readiness: watch::channel(UNKNOWN_READINESS).0,
        document: Some(Document {
            uri: context::observation_uri(&observed).unwrap(),
            source: SourceBinding::from_observation(&observed),
            version: 2,
        }),
        diagnostics: DiagnosticSnapshot {
            source: None,
            generation: ViewGeneration {
                backend: 1,
                view: 1,
                ..Default::default()
            },
            document_version: None,
            freshness: Freshness::Unknown,
            readiness: DiagnosticReadiness::Unknown,
            diagnostics: vec![],
            truncated: false,
        },
    }))
}

/// Proves versioned/unversioned pushes cannot claim clean, late versions cannot replace current evidence,
/// invalidation discards evidence, applyEdit is rejected, and prompts have no affirmative default.
#[tokio::test]
async fn callbacks_and_diagnostics_fail_closed() {
    use tower_service::Service;
    let state = diagnostic_state();
    let mut router = client_router(state.clone());
    let uri = context::observation_uri(&observation("package main", 1)).unwrap();
    for version in [Some(2), Some(1), None] {
        let notification = serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"version":version,"diagnostics":[]}})).unwrap();
        assert!(matches!(
            router.notify(notification),
            ControlFlow::Continue(())
        ));
        let snapshot = &state.lock().unwrap().diagnostics;
        assert_eq!(snapshot.readiness, DiagnosticReadiness::Unknown);
        assert_eq!(snapshot.freshness, Freshness::Provisional);
        if version.is_none() {
            assert!(snapshot.source.is_none());
        } else {
            assert_eq!(snapshot.document_version, Some(2));
        }
    }
    let response = router
        .call(
            serde_json::from_value(
                json!({"id":7,"method":"workspace/applyEdit","params":{"edit":{}}}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response["applied"], false);
    let response = router.call(serde_json::from_value(json!({"id":8,"method":"window/showMessageRequest","params":{"type":3,"message":"execute?","actions":[{"title":"yes"}]}})).unwrap()).await.unwrap();
    assert!(response.is_null());
    {
        let mut state = state.lock().unwrap();
        state.diagnostics.source = Some(SourceBinding::from_observation(&observation(
            "package main",
            1,
        )));
        state.diagnostics.document_version = Some(2);
        state.diagnostics.truncated = true;
        state.invalidate();
        assert!(state.diagnostics.source.is_none());
        assert!(state.diagnostics.document_version.is_none());
        assert!(!state.diagnostics.truncated);
        assert!(state.document.is_none());
    }

    assert_eq!(
        state.lock().unwrap().diagnostics.freshness,
        Freshness::Unknown
    );
}

/// Rejects oversized headers before the JSON decoder sees bytes and accepts fragmented frames.
#[tokio::test]
async fn bounded_transport_enforces_framing() {
    let (mut writer, reader) = tokio::io::duplex(16384);
    writer.write_all(&vec![b'x'; MAX_HEADER + 1]).await.unwrap();
    let error = BoundedInput::new(reader, 1).read_u8().await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    let (mut writer, reader) = tokio::io::duplex(16384);
    let frame = b"Content-Length: 2\r\n\r\n{}";
    writer.write_all(&frame[..9]).await.unwrap();
    writer.write_all(&frame[9..]).await.unwrap();
    drop(writer);
    let mut received = Vec::new();
    BoundedInput::new(reader, 2)
        .read_to_end(&mut received)
        .await
        .unwrap();
    assert_eq!(received, frame);
}

/// Runs a controlled initialized server whose definition never finishes before the deadline.
/// Overlapping server/client request IDs remain independent; timeout and cancellation retire the
/// whole mapping generation and keep future context explicitly lexical.
#[tokio::test]
async fn request_timeout_retires_generation_and_late_results() {
    for cancelled in [false, true] {
        let (client, peer) = tokio::io::duplex(16384);
        let (input, output) = tokio::io::split(client);
        let (peer_input, peer_output) = tokio::io::split(peer);
        let (server, _client) = MainLoop::new_server(|client| {
            let mut router = Router::new(client);
            router.request::<request::Initialize, _>(|client, _| {
                let client = client.clone();
                async move {
                    let settings = client
                        .request::<request::WorkspaceConfiguration>(lsp::ConfigurationParams {
                            items: vec![lsp::ConfigurationItem {
                                scope_uri: None,
                                section: None,
                            }],
                        })
                        .await
                        .unwrap();
                    assert_eq!(settings, vec![serde_json::Value::Null]);
                    Ok(lsp::InitializeResult {
                        capabilities: lsp::ServerCapabilities {
                            text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                                lsp::TextDocumentSyncKind::FULL,
                            )),
                            definition_provider: Some(lsp::OneOf::Left(true)),
                            ..Default::default()
                        },
                        server_info: None,
                    })
                }
            });
            router.request::<request::GotoDefinition, _>(|_, _| async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok(None)
            });
            router.unhandled_notification(|_, _| ControlFlow::Continue(()));
            router
        });
        let run = with_session(
            input,
            output,
            tree(),
            1,
            ViewGeneration {
                backend: 7,
                ..Default::default()
            },
            ProviderSettings::GoplsDefaults,
            SessionOptions {
                request_timeout: Duration::from_millis(50),
                lifetime: Duration::from_secs(2),
            },
            move |mut session| async move {
                let text = "package main\nfunc Name() {}\n";
                let observed = observation(text, 1);
                let query = ContextQuery::Symbol {
                    byte_offset: text.find("Name").unwrap(),
                };
                if cancelled {
                    assert!(
                        tokio::time::timeout(
                            Duration::from_millis(5),
                            session.context(&observed, text.as_bytes(), query)
                        )
                        .await
                        .is_err()
                    );
                } else {
                    let result = session.context(&observed, text.as_bytes(), query).await?;
                    assert!(matches!(result.mode, ContextMode::Lexical { .. }));
                    assert!(result.definitions.is_none());
                    assert!(result.generation.is_none());
                    assert!(result.document_version.is_none());
                }
                assert!(!session.state.lock().unwrap().active);
                let again = session.context(&observed, text.as_bytes(), query).await?;
                assert!(again.document_version.is_none());
                assert_eq!(
                    session.diagnostics().readiness,
                    DiagnosticReadiness::Unknown
                );
                Ok(())
            },
        );
        let (result, _) = tokio::join!(
            run,
            server.run_buffered(peer_input.compat(), peer_output.compat_write())
        );
        result.unwrap();
    }
}

/// EOF during initialize produces failure rather than initialized capabilities or a current result.
#[tokio::test]
async fn eof_never_becomes_provider_readiness() {
    let result: io::Result<()> = with_session(
        tokio::io::empty(),
        tokio::io::sink(),
        tree(),
        1,
        ViewGeneration::default(),
        ProviderSettings::GoplsDefaults,
        SessionOptions::default(),
        |_| async { panic!("EOF must not initialize") },
    )
    .await;
    assert!(result.is_err());
}

/// Returns the accepted immutable Rust identity for controlled protocol peers without spawning a server.
fn rust_settings() -> ProviderSettings {
    rust_settings_with_configuration("cache-priming-disabled-v1")
}

/// Returns a Rust identity with one explicit accepted initialization configuration.
fn rust_settings_with_configuration(configuration: &str) -> ProviderSettings {
    ProviderSettings::Rust(
        RustProfile::new(super::super::rust::RustProfileIdentity {
            binary: "/usr/bin/true".into(),
            rust_analyzer_version: "rust-analyzer contract-1".into(),
            cargo_version: "cargo-test".into(),
            rustc_version: "rustc-test".into(),
            rustup_toolchain: "test-toolchain".into(),
            configuration: configuration.into(),
            trust: "test".into(),
            transport: "stdio-v1".into(),
            cache_namespace: "/private/tmp/agent-ide-session-test-cache".into(),
        })
        .unwrap(),
    )
}

/// Managed sandbox initialization disables proc macros while retaining cache-priming suppression.
#[test]
fn managed_rust_settings_disable_proc_macro_expansion() {
    let settings = rust_settings_with_configuration("cache-priming-and-proc-macro-disabled-v1");
    assert_eq!(
        settings.configuration(),
        serde_json::json!({
            "cachePriming":{"enable":false},
            "procMacro":{"enable":false}
        })
    );
    assert_eq!(
        rust_settings().configuration(),
        serde_json::json!({
            "cachePriming":{"enable":false},
            "procMacro":{"enable":true}
        })
    );
}

/// Negotiates exact gopls/Rust settings and refuses wrong Rust identity or an incomplete/unhealthy barrier.
#[tokio::test]
async fn closed_settings_and_rust_status_barrier_match_the_actual_provider() {
    for (settings, name, version, health, quiescent, success) in [
        (
            ProviderSettings::GoplsDefaults,
            "gopls",
            "test",
            "ok",
            true,
            true,
        ),
        (
            rust_settings(),
            "rust-analyzer",
            "contract-1",
            "ok",
            true,
            true,
        ),
        (
            rust_settings(),
            "rust-analyzer",
            "wrong-version",
            "ok",
            true,
            false,
        ),
        (
            rust_settings(),
            "rust-analyzer",
            "contract-1",
            "warning",
            true,
            false,
        ),
        (
            rust_settings(),
            "rust-analyzer",
            "contract-1",
            "ok",
            false,
            false,
        ),
        (
            ProviderSettings::GoplsDefaults,
            "rust-analyzer",
            "contract-1",
            "ok",
            true,
            false,
        ),
    ] {
        let (client, peer) = tokio::io::duplex(16384);
        let (input, output) = tokio::io::split(client);
        let (peer_input, peer_output) = tokio::io::split(peer);
        let expected = settings.configuration();
        let rust = matches!(settings, ProviderSettings::Rust(_));
        let (server, _) = MainLoop::new_server(move |client| {
            let mut router = Router::new(client);
            router.request::<request::Initialize, _>(move |client, params| {
                assert_eq!(
                    params
                        .capabilities
                        .workspace
                        .as_ref()
                        .unwrap()
                        .configuration,
                    Some(true)
                );
                assert_eq!(
                    params
                        .capabilities
                        .workspace
                        .as_ref()
                        .unwrap()
                        .workspace_folders,
                    Some(true)
                );
                assert_eq!(
                    params
                        .capabilities
                        .window
                        .as_ref()
                        .unwrap()
                        .work_done_progress,
                    Some(true)
                );
                assert_eq!(
                    params
                        .initialization_options
                        .clone()
                        .unwrap_or(serde_json::Value::Null),
                    expected
                );
                assert_eq!(
                    params
                        .capabilities
                        .experimental
                        .as_ref()
                        .and_then(|value| value.get("serverStatusNotification"))
                        .and_then(serde_json::Value::as_bool),
                    rust.then_some(true)
                );
                let expected = expected.clone();
                let client = client.clone();
                async move {
                    let settings = client
                        .request::<request::WorkspaceConfiguration>(lsp::ConfigurationParams {
                            items: vec![
                                lsp::ConfigurationItem {
                                    scope_uri: None,
                                    section: None
                                };
                                2
                            ],
                        })
                        .await
                        .unwrap();
                    assert_eq!(settings, vec![expected.clone(), expected]);
                    Ok(lsp::InitializeResult {
                        capabilities: lsp::ServerCapabilities::default(),
                        server_info: Some(lsp::ServerInfo {
                            name: name.into(),
                            version: Some(version.into()),
                        }),
                    })
                }
            });
            router.notification::<lsp::notification::Initialized>(move |client, _| {
                client
                    .notify::<RustServerStatus>(RustStatus {
                        health: match health {
                            "ok" => RustHealth::Ok,
                            _ => RustHealth::Warning,
                        },
                        quiescent,
                    })
                    .unwrap();
                ControlFlow::Continue(())
            });
            router.request::<request::Shutdown, _>(|_, _| async { Ok(()) });
            router.notification::<lsp::notification::Exit>(|_, _| ControlFlow::Break(Ok(())));
            router.unhandled_notification(|_, _| ControlFlow::Continue(()));
            router
        });
        let run = with_session(
            input,
            output,
            tree(),
            1,
            ViewGeneration::default(),
            settings,
            SessionOptions {
                request_timeout: Duration::from_millis(60),
                lifetime: Duration::from_secs(1),
            },
            move |mut session| async move {
                assert!(success, "unsupported settings/status reached the operation");
                assert_eq!(
                    session.provider_readiness(),
                    if rust {
                        RUST_HEALTHY_QUIESCENT
                    } else {
                        UNKNOWN_READINESS
                    }
                );
                session.shutdown().await?;
                assert!(
                    session
                        .context(
                            &observation("package main", 1),
                            b"package main",
                            ContextQuery::File
                        )
                        .await
                        .is_err()
                );
                assert!(session.provider_readiness().is_unknown());
                Ok(())
            },
        );
        let (result, _) = tokio::join!(
            run,
            server.run_buffered(peer_input.compat(), peer_output.compat_write())
        );
        assert_eq!(
            result.is_ok(),
            success,
            "unexpected settings/barrier outcome: {result:?}"
        );
    }
}

/// Reads one small exact frame in controlled tests; production framing remains owned by BoundedInput.
async fn read_peer_frame(reader: &mut (impl AsyncRead + Unpin)) -> serde_json::Value {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(reader.read_u8().await.unwrap());
        assert!(header.len() < 4096);
    }
    let text = std::str::from_utf8(&header).unwrap();
    let length = text
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    assert!(length <= MAX_BODY);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}
/// Writes a bounded fixed test response without implementing a production RPC multiplexer.
async fn write_peer_frame(writer: &mut (impl AsyncWrite + Unpin), message: serde_json::Value) {
    let body = serde_json::to_vec(&message).unwrap();
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await
        .unwrap();
    writer.write_all(&body).await.unwrap();
    writer.flush().await.unwrap();
}
/// Performs one controlled initialize handshake, advertising only the requested test sync capability.
async fn peer_initialize(peer: &mut tokio::io::DuplexStream, sync: bool) {
    let request = read_peer_frame(peer).await;
    assert_eq!(request["method"], "initialize");
    write_peer_frame(peer,json!({"jsonrpc":"2.0","id":request["id"],"result":{"capabilities":{"textDocumentSync":if sync {1}else{0}},"serverInfo":{"name":"gopls","version":"test"}}})).await;
}

/// EOF and malformed input after initialize remain transport errors even when the operation returns Ok.
#[tokio::test]
async fn post_initialize_transport_failures_are_not_swallowed() {
    for malformed in [false, true] {
        let (client, mut peer) = tokio::io::duplex(16384);
        let (input, output) = tokio::io::split(client);
        let peer = tokio::spawn(async move {
            peer_initialize(&mut peer, false).await;
            let initialized = read_peer_frame(&mut peer).await;
            assert_eq!(initialized["method"], "initialized");
            if malformed {
                peer.write_all(b"Content-Length: 1\r\n\r\n!").await.unwrap();
            }
        });
        let result = with_session(
            input,
            output,
            tree(),
            1,
            ViewGeneration::default(),
            ProviderSettings::GoplsDefaults,
            SessionOptions {
                request_timeout: Duration::from_millis(100),
                lifetime: Duration::from_secs(1),
            },
            |_| async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(())
            },
        )
        .await;
        peer.await.unwrap();
        assert!(
            result.is_err(),
            "post-init transport failure became success"
        );
    }
}

/// Injects a post-handshake writer failure while retaining the real async-lsp transport pipeline.
struct FailingWriter<W> {
    /// Real test transport before failure is enabled.
    inner: W,
    /// Enabled by the operation only after initialize succeeded.
    failed: Arc<std::sync::atomic::AtomicBool>,
}
impl<W: AsyncWrite + Unpin> AsyncWrite for FailingWriter<W> {
    /// Refuses all post-handshake writes with a concrete BrokenPipe error.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.failed.load(std::sync::atomic::Ordering::SeqCst) {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "test writer failed",
            )))
        } else {
            Pin::new(&mut self.inner).poll_write(cx, bytes)
        }
    }
    /// Delegates flush, whose associated write failure remains observable by the driver.
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    /// Closes only the test-owned writer.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Output I/O failure cannot be hidden by a successful user operation.
#[tokio::test]
async fn post_initialize_output_error_is_propagated() {
    let (client, mut peer) = tokio::io::duplex(16384);
    let (input, output) = tokio::io::split(client);
    let peer = tokio::spawn(async move {
        peer_initialize(&mut peer, false).await;
        std::future::pending::<()>().await;
    });
    let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = failed.clone();
    let result = with_session(
        input,
        FailingWriter {
            inner: output,
            failed,
        },
        tree(),
        1,
        ViewGeneration::default(),
        ProviderSettings::GoplsDefaults,
        SessionOptions {
            request_timeout: Duration::from_millis(100),
            lifetime: Duration::from_secs(1),
        },
        move |_| async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    peer.abort();
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("test writer failed")
    );
}

/// A non-reading peer cannot cause repeated full-document updates to grow async-lsp's queue indefinitely.
#[tokio::test]
async fn outbound_budget_retires_full_document_flood_before_unbounded_queueing() {
    let (client, mut peer) = tokio::io::duplex(1024);
    let (input, output) = tokio::io::split(client);
    let peer = tokio::spawn(async move {
        peer_initialize(&mut peer, true).await;
        std::future::pending::<()>().await;
    });
    let exhausted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = exhausted.clone();
    let result = with_session(
        input,
        output,
        tree(),
        1,
        ViewGeneration::default(),
        ProviderSettings::GoplsDefaults,
        SessionOptions {
            request_timeout: Duration::from_millis(100),
            lifetime: Duration::from_millis(400),
        },
        move |mut session| async move {
            let text = "a".repeat(crate::workspace::observation::MAX_SOURCE_BYTES - 64);
            for sequence in 1..=12 {
                let source = observation(&text, sequence);
                let _ = session
                    .context(&source, text.as_bytes(), ContextQuery::File)
                    .await?;
            }
            assert!(!session.state.lock().unwrap().active);
            assert!(session.budget.bytes <= MAX_SESSION_OUTBOUND_BYTES);
            assert!(session.budget.messages <= MAX_SESSION_OUTBOUND_MESSAGES);
            exhausted.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        },
    )
    .await;
    peer.abort();
    assert!(observed.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        result.is_err(),
        "blocked output requires bounded transport failure"
    );
}
