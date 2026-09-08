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
    state.lock().unwrap().invalidate();
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
        SessionOptions::default(),
        |_| async { panic!("EOF must not initialize") },
    )
    .await;
    assert!(result.is_err());
}
