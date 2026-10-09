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
        "main.txt".into(),
        Some(SourceBytes::from_bytes(text.as_bytes())),
        SourceRevision::new(format!("revision-{sequence}")).unwrap(),
        SourceCoverage::Complete,
        ObservedState::Present,
    )
    .unwrap()
}

/// Neutral language-server profile for session mechanism tests.
///
/// The server is named `fake-server`. Without a status barrier any omitted or `fake-server`
/// identity is accepted; with one, only `fake-server` at version `contract-1` is, readiness arrives
/// as `fake/status` notifications whose `state` is `ready`, `failed` or `busy`, and `.fake` files
/// open as `fake`.
#[derive(Debug)]
struct FakeProfile {
    /// Whether requests wait for a `fake/status` readiness report.
    status: bool,
}

impl SessionProfile for FakeProfile {
    /// A fixed marker object.
    fn workspace_configuration(&self) -> serde_json::Value {
        json!({"fake": {"enabled": true}})
    }

    /// See [`FakeProfile`].
    fn accepts_server(&self, info: Option<&lsp::ServerInfo>) -> bool {
        if self.status {
            info.is_some_and(|info| {
                info.name == "fake-server" && info.version.as_deref() == Some("contract-1")
            })
        } else {
            info.is_none_or(|info| info.name == "fake-server")
        }
    }

    /// Asks for status notifications only with a barrier.
    fn experimental_capabilities(&self) -> Option<serde_json::Value> {
        self.status.then(|| json!({"fakeStatus": true}))
    }

    /// `fake/status` with a barrier.
    fn status_method(&self) -> Option<&'static str> {
        self.status.then_some("fake/status")
    }

    /// Maps `{"state": ...}`; anything else fails decoding.
    fn status(&self, params: serde_json::Value) -> Result<ProviderStatus, serde_json::Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "lowercase")]
        enum State {
            Ready,
            Failed,
            Busy,
        }
        #[derive(serde::Deserialize)]
        struct Status {
            state: State,
        }
        let status: Status = serde_json::from_value(params)?;
        Ok(match status.state {
            State::Ready => ProviderStatus::Ready,
            State::Failed => ProviderStatus::Failed,
            State::Busy => ProviderStatus::Busy,
        })
    }

    /// `.fake` files open as `fake`.
    fn language_id(&self, path: &std::path::Path) -> &'static str {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("fake") => "fake",
            _ => "plaintext",
        }
    }
}

/// A profile usable right after the handshake.
fn plain_settings() -> ProviderSettings {
    ProviderSettings::new(FakeProfile { status: false })
}

/// A profile whose requests wait for a reported readiness.
fn status_settings() -> ProviderSettings {
    ProviderSettings::new(FakeProfile { status: true })
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
    assert_eq!(
        result.text, large,
        "the whole observed text is returned (T16B)"
    );
}

/// Creates bounded router state with one synchronized document and unknown diagnostics.
fn diagnostic_state() -> Arc<Mutex<State>> {
    let observed = observation("package main", 1);
    Arc::new(Mutex::new(State {
        active: true,
        terminal: false,
        shutdown_complete: false,
        settings: plain_settings(),
        readiness: watch::channel(UNKNOWN_READINESS).0,
        diagnostic_revision: watch::channel(0).0,
        document: Some(Document {
            uri: context::observation_uri(&observed).unwrap(),
            source: SourceBinding::from_observation(&observed),
            version: 2,
            accepts_unversioned_report: false,
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

/// For a profile that accepts unversioned initial reports, a one-shot open ignores an empty syntax
/// push, binds a later nonempty report, and refuses to rebind unversioned evidence after a
/// full-document change to different bytes.
#[tokio::test]
async fn unversioned_diagnostics_require_unchanged_initial_open() {
    let state = diagnostic_state();
    state
        .lock()
        .unwrap()
        .document
        .as_mut()
        .unwrap()
        .accepts_unversioned_report = true;
    let mut router = client_router(state.clone());
    let uri = context::observation_uri(&observation("package main", 1)).unwrap();
    let empty = serde_json::from_value(
        json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"diagnostics":[]}}),
    )
    .unwrap();
    assert!(matches!(router.notify(empty), ControlFlow::Continue(())));
    assert_eq!(
        state.lock().unwrap().diagnostics.readiness,
        DiagnosticReadiness::Unknown
    );
    assert!(!wait_for_matching_diagnostics(&state, Instant::now()).await);

    let reported = serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"severity":1,"code":2322,"message":"Type 'string' is not assignable to type 'number'."}]}})).unwrap();
    assert!(matches!(router.notify(reported), ControlFlow::Continue(())));
    assert!(wait_for_matching_diagnostics(&state, Instant::now()).await);
    {
        let state = state.lock().unwrap();
        assert_eq!(state.diagnostics.readiness, DiagnosticReadiness::Reported);
        assert_eq!(state.diagnostics.freshness, Freshness::Provisional);
        assert_eq!(state.diagnostics.document_version, None);
        assert_eq!(
            state.diagnostics.source.as_ref(),
            Some(&state.document.as_ref().unwrap().source)
        );
        assert_eq!(state.diagnostics.diagnostics.len(), 1);
    }

    // The real full-document didChange path advances identity and disables unversioned binding.
    let (_driver, server) = MainLoop::new_client({
        let state = state.clone();
        move |_| client_router(state)
    });
    let mut session = Session {
        server,
        worktree: tree(),
        epoch: 1,
        generation: ViewGeneration::default(),
        settings: plain_settings(),
        budget: OutboundBudget::default(),
        state: state.clone(),
        capabilities: Some(ProviderCapabilities {
            advertised: lsp::ServerCapabilities {
                text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                    lsp::TextDocumentSyncKind::FULL,
                )),
                ..Default::default()
            },
            position_encoding: lsp::PositionEncodingKind::UTF16,
            server_info: None,
        }),
        options: SessionOptions::default(),
        deadline: Instant::now() + Duration::from_secs(1),
        sequence: 1,
        version: 2,
        remote: None,
    };
    assert_eq!(
        session
            .synchronize(&observation("changed bytes", 2), "changed bytes")
            .unwrap(),
        Some(3)
    );
    assert!(
        !state
            .lock()
            .unwrap()
            .document
            .as_ref()
            .unwrap()
            .accepts_unversioned_report
    );
    assert_eq!(
        state.lock().unwrap().diagnostics.readiness,
        DiagnosticReadiness::Unknown
    );
    let late = serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"diagnostics":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"code":2322,"message":"stale TS2322"}]}})).unwrap();
    assert!(matches!(router.notify(late), ControlFlow::Continue(())));
    assert!(state.lock().unwrap().diagnostics.source.is_none());
    assert!(!wait_for_matching_diagnostics(&state, Instant::now()).await);
}

/// A clean document with no diagnostic push must leave time for shutdown and transport EOF.
#[tokio::test]
async fn missing_diagnostics_do_not_consume_shutdown_deadline() {
    let (client, peer) = tokio::io::duplex(16384);
    let (input, output) = tokio::io::split(client);
    let (peer_input, peer_output) = tokio::io::split(peer);
    let (server, _) = MainLoop::new_server(|client| {
        let mut router = Router::new(client);
        router.request::<request::Initialize, _>(|_, _| async {
            Ok(lsp::InitializeResult {
                capabilities: lsp::ServerCapabilities {
                    text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                        lsp::TextDocumentSyncKind::FULL,
                    )),
                    ..Default::default()
                },
                server_info: None,
            })
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
        plain_settings(),
        SessionOptions {
            request_timeout: Duration::from_millis(600),
            lifetime: Duration::from_millis(600),
        },
        |mut session| async move {
            session
                .context(
                    &observation("package main", 1),
                    b"package main",
                    ContextQuery::File,
                )
                .await?;
            session.wait_for_matching_diagnostics().await;
            assert_eq!(
                session.diagnostics().readiness,
                DiagnosticReadiness::Unknown
            );
            session.shutdown().await
        },
    );
    let (result, _) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            run,
            server.run_buffered(peer_input.compat(), peer_output.compat_write())
        )
    })
    .await
    .expect("mock server settles");
    result.expect("diagnostic silence must preserve shutdown time");
}

/// Confirms an accepted versioned diagnostic callback wakes the bounded exact-document wait.
#[tokio::test]
async fn matching_diagnostics_notification_wakes_waiter() {
    let state = diagnostic_state();
    let waiting = tokio::spawn({
        let state = state.clone();
        async move {
            wait_for_matching_diagnostics(&state, Instant::now() + Duration::from_secs(1)).await
        }
    });
    tokio::task::yield_now().await;
    let mut router = client_router(state.clone());
    let uri = context::observation_uri(&observation("package main", 1)).unwrap();
    let notification = serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"version":2,"diagnostics":[]}})).unwrap();
    assert!(matches!(
        router.notify(notification),
        ControlFlow::Continue(())
    ));
    assert!(waiting.await.unwrap());
    assert_eq!(
        state.lock().unwrap().diagnostics.readiness,
        DiagnosticReadiness::Clean
    );
}

/// Confirms stale versioned notifications do not wake the waiter and deadline preserves unknown evidence.
#[tokio::test]
async fn stale_diagnostics_notification_does_not_wake_waiter() {
    let state = diagnostic_state();
    let mut router = client_router(state.clone());
    let uri = context::observation_uri(&observation("package main", 1)).unwrap();
    let notification = serde_json::from_value(json!({"method":"textDocument/publishDiagnostics", "params":{"uri":uri,"version":1,"diagnostics":[]}})).unwrap();
    assert!(matches!(
        router.notify(notification),
        ControlFlow::Continue(())
    ));
    assert!(!wait_for_matching_diagnostics(&state, Instant::now()).await);
    assert_eq!(
        state.lock().unwrap().diagnostics.freshness,
        Freshness::Unknown
    );
}

/// Proves only a matching versioned empty result can claim clean, late versions cannot replace it,
/// unversioned evidence remains unknown, invalidation discards evidence, applyEdit is rejected,
/// and prompts have no affirmative default.
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
        assert_eq!(
            snapshot.readiness,
            if version.is_none() {
                DiagnosticReadiness::Unknown
            } else {
                DiagnosticReadiness::Clean
            }
        );
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
                    // The configuration request is answered with the profile's own settings.
                    assert_eq!(
                        settings,
                        vec![serde_json::json!({"fake": {"enabled": true}})]
                    );
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
            plain_settings(),
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
        plain_settings(),
        SessionOptions::default(),
        |_| async { panic!("EOF must not initialize") },
    )
    .await;
    assert!(result.is_err());
}

/// Negotiates the profile's exact settings and identity, and holds the one-shot session behind a
/// status barrier: a ready report proceeds, a wrong identity, a busy report and a failed workspace
/// do not; a profile without a barrier proceeds with unknown readiness.
#[tokio::test]
async fn closed_settings_and_status_barrier_match_the_actual_provider() {
    for (settings, name, version, state, success) in [
        (plain_settings(), "fake-server", "test", "ready", true),
        (
            status_settings(),
            "fake-server",
            "contract-1",
            "ready",
            true,
        ),
        (
            status_settings(),
            "fake-server",
            "wrong-version",
            "ready",
            false,
        ),
        (
            status_settings(),
            "fake-server",
            "contract-1",
            "failed",
            false,
        ),
        (
            status_settings(),
            "fake-server",
            "contract-1",
            "busy",
            false,
        ),
        (
            plain_settings(),
            "other-server",
            "contract-1",
            "ready",
            false,
        ),
    ] {
        let (client, peer) = tokio::io::duplex(16384);
        let (input, output) = tokio::io::split(client);
        let (peer_input, peer_output) = tokio::io::split(peer);
        let expected = settings.profile().workspace_configuration();
        let barrier = settings.profile().status_method().is_some();
        let experimental = settings.profile().experimental_capabilities();
        let (server, _) = MainLoop::new_server(move |client| {
            let mut router = Router::new(client);
            router.request::<request::Initialize, _>(move |client, params| {
                assert_eq!(
                    params
                        .capabilities
                        .text_document
                        .as_ref()
                        .and_then(|caps| caps.publish_diagnostics.as_ref())
                        .and_then(|caps| caps.related_information),
                    Some(false)
                );
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
                assert_eq!(params.capabilities.experimental, experimental);
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
                    .notify::<ServerStatus>(json!({"state": state}))
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
                    if barrier {
                        ProviderReadiness::from_status(ProviderStatus::Ready)
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

/// The status notification a controlled peer sends; params stay raw so each case can shape them.
enum ServerStatus {}
impl lsp::notification::Notification for ServerStatus {
    /// Raw status fields, decoded only by the session's profile.
    type Params = serde_json::Value;
    /// The status method the barrier profile listens to.
    const METHOD: &'static str = "fake/status";
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
    write_peer_frame(peer,json!({"jsonrpc":"2.0","id":request["id"],"result":{"capabilities":{"textDocumentSync":if sync {1}else{0}},"serverInfo":{"name":"fake-server","version":"test"}}})).await;
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
            plain_settings(),
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
        plain_settings(),
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
        plain_settings(),
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

/// A live session keeps its driver across requests: handshake without a readiness wait, then
/// readiness arrives later, then document symbols and hover answer on the same transport, and a
/// second request after the first proves the budget is per request.
#[tokio::test]
async fn live_session_outlives_requests_and_waits_for_readiness_per_request() {
    let (client, peer) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(client);
    let (peer_input, peer_output) = tokio::io::split(peer);
    let (server, _) = MainLoop::new_server(move |client| {
        let mut router = Router::new(client);
        router.request::<request::Initialize, _>(|_, _| async {
            Ok(lsp::InitializeResult {
                capabilities: lsp::ServerCapabilities {
                    text_document_sync: Some(lsp::TextDocumentSyncCapability::Kind(
                        lsp::TextDocumentSyncKind::FULL,
                    )),
                    document_symbol_provider: Some(lsp::OneOf::Left(true)),
                    hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                    ..Default::default()
                },
                server_info: Some(lsp::ServerInfo {
                    name: "fake-server".into(),
                    version: Some("contract-1".into()),
                }),
            })
        });
        router.notification::<lsp::notification::Initialized>(move |client, _| {
            // Readiness arrives a little after the handshake, like a real workspace load.
            let client = client.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = client.notify::<ServerStatus>(json!({"state": "ready"}));
            });
            ControlFlow::Continue(())
        });
        router.notification::<lsp::notification::DidOpenTextDocument>(|_, _| {
            ControlFlow::Continue(())
        });
        router.notification::<lsp::notification::DidChangeTextDocument>(|_, _| {
            ControlFlow::Continue(())
        });
        router.request::<request::DocumentSymbolRequest, _>(|_, _| async {
            #[allow(deprecated)]
            Ok(Some(lsp::DocumentSymbolResponse::Nested(vec![
                lsp::DocumentSymbol {
                    name: "main".into(),
                    detail: None,
                    kind: lsp::SymbolKind::FUNCTION,
                    tags: None,
                    deprecated: None,
                    range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 12)),
                    selection_range: lsp::Range::new(
                        lsp::Position::new(0, 3),
                        lsp::Position::new(0, 7),
                    ),
                    children: None,
                },
            ])))
        });
        router.request::<request::HoverRequest, _>(|_, _| async {
            Ok(Some(lsp::Hover {
                contents: lsp::HoverContents::Markup(lsp::MarkupContent {
                    kind: lsp::MarkupKind::PlainText,
                    value: "fn main()".into(),
                }),
                range: None,
            }))
        });
        router.request::<request::Shutdown, _>(|_, _| async { Ok(()) });
        router.notification::<lsp::notification::Exit>(|_, _| ControlFlow::Break(Ok(())));
        router.unhandled_notification(|_, _| ControlFlow::Continue(()));
        router
    });
    let peer_task =
        tokio::spawn(server.run_buffered(peer_input.compat(), peer_output.compat_write()));
    let mut live = LiveSession::open(
        input,
        output,
        tree(),
        1,
        ViewGeneration::default(),
        status_settings(),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(live.is_alive());
    // Not ready yet with a tiny budget, then ready once the status arrives.
    assert_eq!(
        live.wait_ready(Duration::from_millis(1)).await,
        Err(ReadinessError::Loading)
    );
    assert_eq!(live.wait_ready(Duration::from_secs(2)).await, Ok(()));
    let text = "fn main() {}\n";
    let symbols = live
        .session
        .document_symbols(&observation(text, 1), text.as_bytes())
        .await
        .unwrap();
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0].name, "main");
    let hover = live
        .session
        .hover(&observation(text, 1), text.as_bytes(), 3)
        .await
        .unwrap();
    assert_eq!(hover.as_deref(), Some("fn main()"));
    assert!(live.is_alive());
    live.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(2), peer_task).await;
}

/// Opens an M-011 pilot (remote) session against an in-memory module that answers `hello`, then
/// answers every later request with `reply` and records its method.
async fn remote_session(reply: serde_json::Value) -> (LiveSession, Arc<Mutex<Vec<String>>>) {
    use super::super::pilot::{read_frame, write_frame};
    let (core_out, mut module_in) = tokio::io::duplex(1 << 16);
    let (mut module_out, core_in) = tokio::io::duplex(1 << 16);
    let log = Arc::new(Mutex::new(Vec::new()));
    let seen = log.clone();
    tokio::spawn(async move {
        let hello = read_frame(&mut module_in).await.unwrap();
        let capabilities =
            json!({"advertised": {}, "position_encoding": "utf-16", "server_info": null});
        write_frame(
            &mut module_out,
            &json!({"id": hello["id"], "result": capabilities}),
        )
        .await
        .unwrap();
        while let Ok(request) = read_frame(&mut module_in).await {
            seen.lock()
                .unwrap()
                .push(request["method"].as_str().unwrap().to_owned());
            write_frame(
                &mut module_out,
                &json!({"id": request["id"], "result": reply}),
            )
            .await
            .unwrap();
        }
    });
    let generation = ViewGeneration {
        backend: 1,
        configuration: 1,
        toolchain: 1,
        view: 1,
    };
    let live = LiveSession::open_remote(
        core_in,
        core_out,
        tree(),
        1,
        generation,
        plain_settings(),
        Duration::from_secs(5),
        json!({}),
    )
    .await
    .unwrap();
    (live, log)
}

/// A remote session applies the local source fences before any byte reaches the module: another
/// worktree, another epoch, bytes that do not match the observation and a stale sequence are
/// refused, and only the one valid request is forwarded.
#[tokio::test]
async fn remote_session_fences_source_before_forwarding() {
    let (mut live, log) = remote_session(json!([])).await;
    let other_tree =
        WorktreeRef::from_discovery("/tmp/other".into(), "/tmp/other".into(), ".git".into(), 1)
            .unwrap();
    let foreign = |worktree: WorktreeRef, epoch: u64| {
        SourceObservation::new(
            worktree,
            epoch,
            5,
            ObservationRef::new("source-5").unwrap(),
            "main.txt".into(),
            Some(SourceBytes::from_bytes(b"a")),
            SourceRevision::new("revision-5").unwrap(),
            SourceCoverage::Complete,
            ObservedState::Present,
        )
        .unwrap()
    };
    let session = &mut live.session;
    let error = session
        .document_symbols(&foreign(other_tree, 1), b"a")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    let error = session
        .document_symbols(&foreign(tree(), 2), b"a")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(
        session
            .document_symbols(&observation("a", 2), b"b")
            .await
            .is_err()
    );
    assert!(
        session
            .document_symbols(&observation("a", 3), b"a")
            .await
            .unwrap()
            .is_empty()
    );
    let error = session
        .document_symbols(&observation("a", 2), b"a")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(
        session
            .rename(&observation("a", 4), b"a", 0, "b")
            .await
            .is_err()
    );
    assert_eq!(*log.lock().unwrap(), ["document_symbols"]);
}

/// The module's context evidence counts only for the core's own generation, and its
/// diagnostics bind to the core's observation only when the module saw the same sequence.
#[tokio::test]
async fn remote_context_checks_generation_and_diagnostic_binding() {
    let diagnostic = json!({"range": {"start": {"line": 0, "character": 0},
        "end": {"line": 0, "character": 1}}, "message": "m"});
    let reply = |generation: u64, sequence: u64| {
        let generation = (generation > 0).then_some([generation, 1, 1, generation]);
        json!({
            "context": {"generation": generation, "document_version": 1,
                "position_encoding": "utf-16", "lexical": null, "definitions": [],
                "references": [], "truncated": false},
            "diagnostics": {"source_sequence": sequence, "document_version": 1,
                "readiness": "reported", "freshness": "provisional",
                "diagnostics": [diagnostic], "truncated": false},
        })
    };
    let query = ContextQuery::Symbol { byte_offset: 0 };

    let (mut live, _) = remote_session(reply(1, 2)).await;
    let result = live
        .session
        .context(&observation("a", 2), b"a", query)
        .await
        .unwrap();
    assert_eq!(result.mode, ContextMode::Semantic);
    assert_eq!(
        result.source,
        SourceBinding::from_observation(&observation("a", 2))
    );
    let diagnostics = live.session.diagnostics();
    assert_eq!(diagnostics.readiness, DiagnosticReadiness::Reported);
    assert_eq!(diagnostics.diagnostics.len(), 1);

    let (mut live, _) = remote_session(reply(1, 9)).await;
    live.session
        .context(&observation("a", 2), b"a", query)
        .await
        .unwrap();
    let diagnostics = live.session.diagnostics();
    assert_eq!(diagnostics.readiness, DiagnosticReadiness::Unknown);
    assert!(diagnostics.source.is_none() && diagnostics.diagnostics.is_empty());

    // Another generation, and semantic evidence without any generation (0 = null here).
    for generation in [7, 0] {
        let (mut live, _) = remote_session(reply(generation, 2)).await;
        let result = live
            .session
            .context(&observation("a", 2), b"a", query)
            .await
            .unwrap();
        assert_eq!(
            result.mode,
            ContextMode::Lexical {
                reason: "pilot module answered for another generation".into()
            }
        );
        assert!(result.generation.is_none() && result.definitions.is_none());
        assert!(
            !live.is_alive(),
            "a reply for another generation retires the session"
        );
        assert_eq!(
            live.remote_fault(),
            Some("pilot module answered for another generation"),
            "and is a module fault its owner turns into a typed refusal"
        );
    }
}
