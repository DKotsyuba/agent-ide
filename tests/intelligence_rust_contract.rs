//! Contract checks for the exclusive rust-analyzer v0.1 profile.

#[path = "support/provider.rs"]
#[allow(dead_code)]
mod provider;

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::{
    execution::{
        AdmissionClass, AdmissionController, AdmissionLimits, OwnerId, ProviderLeaseLimits,
        ProviderLeaseRegistry, WorkspaceAuthority,
    },
    intelligence::rust::{
        RustAvailability, RustProfile, RustProfileIdentity, RustProtocolChild, RustViewAdmission,
        RustViews, RustWorktree,
    },
    workspace::authority::WorktreeRef,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

/// Exact analyzer build observed from the selected toolchain and required in `initialize` replies.
const ANALYZER_VERSION: &str = "1.98.1 (48a229ce 2026-09-01)";

/// Builds one canonical worktree and matching Execution authority for an isolated profile test.
fn worktree(path: &str, incarnation: u64) -> RustWorktree {
    let path = PathBuf::from(path);
    let worktree = WorktreeRef::from_discovery(
        path.clone(),
        path.clone(),
        PathBuf::from(".git"),
        incarnation,
    )
    .unwrap();
    let authority =
        WorkspaceAuthority::from_workspace(worktree.id(), incarnation.to_string(), path, 1)
            .unwrap();
    RustWorktree::new(worktree, authority).unwrap()
}

/// Returns the operator-verified absolute path of one binary inside the accepted rustup toolchain,
/// honoring `AGENT_IDE_RUST_TOOLCHAIN_DIR` when the harness points at a non-default rustup home.
fn toolchain_bin(tool: &str) -> PathBuf {
    let root = std::env::var("AGENT_IDE_RUST_TOOLCHAIN_DIR")
        .unwrap_or_else(|_| "/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin".into());
    PathBuf::from(root).join("bin").join(tool)
}

/// Returns the operator-verified rust-analyzer path, with the local accepted binary as fallback.
fn analyzer_bin() -> PathBuf {
    std::env::var("AGENT_IDE_RUST_ANALYZER")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/Users/pluto/.local/bin/rust-analyzer"))
}

/// Builds the complete fixed Rust compatibility identity used by every exclusive request.
/// `cache_namespace` must be a verified absolute directory, never a bare relative label, so
/// `command`'s derived `CARGO_HOME`/`CARGO_TARGET_DIR`/`TMPDIR` resolve without depending on the
/// spawned process's working directory.
fn profile(cache_namespace: &Path) -> RustProfile {
    RustProfile::new(RustProfileIdentity {
        binary: analyzer_bin(),
        rust_analyzer_version: format!("rust-analyzer {ANALYZER_VERSION}"),
        cargo: toolchain_bin("cargo"),
        cargo_home: None,
        cargo_version: "cargo 1.98.1".into(),
        rustc: toolchain_bin("rustc"),
        rustc_version: "rustc 1.98.1".into(),
        rustup_toolchain: "1.98.1-aarch64-apple-darwin".into(),
        configuration: "cache-priming-check-on-save-and-proc-macro-disabled-v1".into(),
        trust: "local-trusted-v1".into(),
        transport: "stdio-v1".into(),
        cache_namespace: cache_namespace.display().to_string(),
    })
    .unwrap()
}

/// Creates a private Cargo project and its profile-local writable cache directories.
/// `item` returns the given Rust type and expression; returns the project root, its `src/lib.rs`
/// URI, and the verified absolute cache namespace prepared under that same root. The successful
/// caller removes the root, which also reclaims the namespace.
fn project(label: &str, result: &str, expression: &str) -> (PathBuf, String, PathBuf) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "agent-ide-rust-contract-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(root.join("src")).unwrap();
    let cache_namespace = root.join("rust-native-v1");
    for directory in ["cargo", "target", "tmp"] {
        fs::create_dir_all(cache_namespace.join(directory)).unwrap();
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"contract\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        format!(
            "pub fn item() -> {result} {{\n    let answer: {result} = {expression};\n    answer\n}}\n"
        ),
    )
    .unwrap();
    let uri = format!("file://{}", root.join("src/lib.rs").display());
    (root, uri, cache_namespace)
}

/// Retains genuine host-bound scope; each actual spawn consumes a new ActiveBindingUse from its guard.
fn request(profile: &RustProfile, worktree: &RustWorktree) -> provider::BoundRequest {
    provider::BoundRequest::new(
        &format!("rust-{}", worktree.worktree().id()),
        worktree.authority().clone(),
        profile.command(worktree).unwrap(),
        profile.binary(),
    )
}

/// Writes one bounded JSON-RPC payload with its exact LSP Content-Length envelope.
async fn send(child: &mut RustProtocolChild, message: Value) -> io::Result<()> {
    let body = serde_json::to_vec(&message).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    timeout(Duration::from_secs(10), async {
        child.stdin_mut().write_all(header.as_bytes()).await?;
        child.stdin_mut().write_all(&body).await?;
        child.stdin_mut().flush().await
    })
    .await
    .map_err(io::Error::other)?
}

/// Reads a frame capped at an 8 KiB header and 1 MiB body under the enclosing session deadline.
/// Malformed frames and EOF return I/O errors so the caller can reap before failing the test.
async fn receive(child: &mut RustProtocolChild) -> io::Result<Value> {
    let mut header = Vec::new();
    async {
        loop {
            let mut byte = [0];
            child.stdout_mut().read_exact(&mut byte).await?;
            header.push(byte[0]);
            if header.len() > 8192 {
                return Err(io::Error::other("oversized LSP header"));
            }
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let header = std::str::from_utf8(&header).map_err(io::Error::other)?;
        let length = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .ok_or_else(|| io::Error::other("missing Content-Length"))?
            .parse::<usize>()
            .map_err(io::Error::other)?;
        if length > 1024 * 1024 {
            return Err(io::Error::other("oversized LSP body"));
        }
        let mut body = vec![0; length];
        child.stdout_mut().read_exact(&mut body).await?;
        serde_json::from_slice(&body).map_err(io::Error::other)
    }
    .await
}

/// Services server requests before matching response IDs, preserving independent RPC ID spaces.
/// `None` waits for workspace quiescence; `Some` waits for that client response ID.
/// Emits received messages for test-failure diagnostics; the enclosing session bounds total time.
async fn response(child: &mut RustProtocolChild, id: Option<u64>) -> io::Result<Value> {
    loop {
        let message = receive(child).await?;
        eprintln!("server: {message}");
        if let (Some(server_id), Some(method)) = (message.get("id"), message.get("method")) {
            let reply = match method.as_str() {
                Some("workspace/configuration") => {
                    let items = message["params"]["items"]
                        .as_array()
                        .ok_or_else(|| io::Error::other("invalid configuration request"))?;
                    json!({"result": vec![json!({"cachePriming": {"enable": false}}); items.len()]})
                }
                Some("window/workDoneProgress/create" | "workspace/diagnostic/refresh") => {
                    json!({"result": null})
                }
                _ => json!({"error": {"code": -32601, "message": "unsupported probe callback"}}),
            };
            let mut reply = reply;
            reply["jsonrpc"] = json!("2.0");
            reply["id"] = server_id.clone();
            send(child, reply).await?;
        } else if id.is_some_and(|id| message.get("id") == Some(&json!(id)))
            || (id.is_none()
                && message["method"] == "experimental/serverStatus"
                && message["params"]["quiescent"] == true)
        {
            return Ok(message);
        }
    }
}

/// Initializes one server, opens the divergent source, and proves hover plus definition semantics.
async fn semantic_session(
    child: &mut RustProtocolChild,
    root: &Path,
    uri: &str,
    expected: &str,
) -> io::Result<()> {
    send(child, json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"processId": null, "rootUri": format!("file://{}", root.display()),
            "initializationOptions": {"cachePriming": {"enable": false}},
            "workspaceFolders": [{"uri": format!("file://{}", root.display()), "name": "contract"}],
            "capabilities": {
                "experimental": {"serverStatusNotification": true},
                "window": {"workDoneProgress": true},
                "workspace": {"configuration": true, "workspaceFolders": true},
                "textDocument": {"hover": {"contentFormat": ["markdown", "plaintext"]}}}}
    }))
    .await?;
    let initialized = response(child, Some(1)).await?;
    if initialized
        .pointer("/result/serverInfo/version")
        .and_then(Value::as_str)
        != Some(ANALYZER_VERSION)
    {
        return Err(io::Error::other(format!(
            "unexpected analyzer version: {initialized}"
        )));
    }
    send(
        child,
        json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )
    .await?;
    send(
        child,
        json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "rust", "version": 1,
                "text": fs::read_to_string(root.join("src/lib.rs")).unwrap()}}
        }),
    )
    .await?;
    let ready = response(child, None).await?;
    if ready["params"]["health"] != "ok" {
        return Err(io::Error::other(format!(
            "workspace is not healthy: {ready}"
        )));
    }
    send(
        child,
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "textDocument/hover",
            "params": {"textDocument": {"uri": uri}, "position": {"line": 2, "character": 5}}
        }),
    )
    .await?;
    let hover = response(child, Some(2)).await?;
    if !hover
        .get("result")
        .filter(|value| !value.is_null())
        .is_some_and(|value| value.to_string().contains(expected))
    {
        return Err(io::Error::other(format!("no useful hover: {hover}")));
    }
    send(
        child,
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "textDocument/definition",
                "params": {"textDocument": {"uri": uri}, "position": {"line": 2, "character": 5}}
        }),
    )
    .await?;
    let definition = response(child, Some(3)).await?;
    if !(definition.to_string().contains(uri)
        && definition
            .pointer("/result/range/start/line")
            .or_else(|| definition.pointer("/result/0/range/start/line"))
            == Some(&json!(1)))
    {
        return Err(io::Error::other(format!(
            "unexpected definition: {definition}"
        )));
    }
    eprintln!("semantic evidence: expected={expected}, hover={hover}, definition={definition}");
    send(
        child,
        json!({"jsonrpc": "2.0", "id": 4, "method": "shutdown", "params": null}),
    )
    .await?;
    let shutdown = response(child, Some(4)).await?;
    if shutdown.get("result") != Some(&Value::Null) {
        return Err(io::Error::other(format!("shutdown failed: {shutdown}")));
    }
    send(
        child,
        json!({"jsonrpc": "2.0", "method": "exit", "params": null}),
    )
    .await
}

/// Bounds the full semantic session and always reaps the child to retain capped stderr on failure.
async fn semantic_probe(
    mut child: RustProtocolChild,
    root: &Path,
    uri: &str,
    expected: &str,
) -> agent_ide::execution::DirectChildReap {
    let result = timeout(
        Duration::from_secs(60),
        semantic_session(&mut child, root, uri, expected),
    )
    .await;
    let reaped = if matches!(result, Ok(Ok(()))) {
        child.reap(Duration::from_secs(10)).await
    } else {
        child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_secs(10))
            .await
    }
    .unwrap();
    let stderr = &reaped.stderr;
    assert!(
        matches!(result, Ok(Ok(()))),
        "semantic probe failed: {result:?}; stderr={} (truncated={}, complete={})",
        String::from_utf8_lossy(&stderr.bytes),
        stderr.truncated,
        stderr.complete
    );
    assert!(
        reaped.status.success(),
        "Rust provider exit: {:?}",
        reaped.status
    );
    reaped.proof
}

/// Proves divergent worktrees never share an exclusive Rust backend and the second demand queues.
#[test]
fn exclusive_rust_requests_are_distinct_and_source_results_are_generation_scoped() {
    let first_worktree = worktree("/private/tmp/agent-ide-rust-first", 1);
    let second_worktree = worktree("/private/tmp/agent-ide-rust-second", 1);
    let profile = profile(Path::new("/private/tmp/agent-ide-rust-cache-native-v1"));
    assert_ne!(
        profile.compatibility_key(&first_worktree),
        profile.compatibility_key(&second_worktree)
    );

    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 2,
        per_backend_views: 1,
    })
    .unwrap();
    let mut views = RustViews::default();
    let owner = OwnerId::new("rust-contract").unwrap();

    let first = match views.request(
        &profile,
        &first_worktree,
        &mut registry,
        &mut admission,
        owner.clone(),
        AdmissionClass::Interactive,
    ) {
        RustViewAdmission::Granted(view) => view,
        outcome => panic!("first exclusive Rust request was not admitted: {outcome:?}"),
    };
    assert_eq!(admission.running_count(), 1);
    assert_eq!(
        views
            .result_state(first.lease(), first.generation(), 0)
            .unwrap(),
        RustAvailability::Ready
    );
    views.observe_source(first.lease(), 1).unwrap();
    assert_eq!(
        views
            .result_state(first.lease(), first.generation(), 0)
            .unwrap(),
        RustAvailability::Stale
    );
    assert_eq!(
        views
            .result_state(first.lease(), first.generation(), 1)
            .unwrap(),
        RustAvailability::Ready
    );

    assert!(matches!(
        views.request(
            &profile,
            &second_worktree,
            &mut registry,
            &mut admission,
            owner,
            AdmissionClass::Interactive,
        ),
        RustViewAdmission::Queued(_)
    ));
    assert_eq!(admission.running_count(), 1);
}

/// Runs two divergent real rust-analyzer projects serially under one heavy-process slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rust_analyzer_is_exclusive_across_divergent_worktrees() {
    let analyzer = analyzer_bin();
    let (first_root, first_uri, first_cache_namespace) = project("first", "u32", "42");
    let (second_root, second_uri, second_cache_namespace) =
        project("second", "String", "String::new()");
    // Each worktree gets its own verified absolute cache namespace: divergent worktrees never
    // share a physical Cargo/target directory, even under an otherwise identical profile.
    let build_profile = |cache_namespace: &Path| {
        RustProfile::new(RustProfileIdentity {
            binary: analyzer.clone(),
            rust_analyzer_version: format!("rust-analyzer {ANALYZER_VERSION}"),
            cargo: toolchain_bin("cargo"),
            cargo_home: None,
            cargo_version: "cargo 1.98.1".into(),
            rustc: toolchain_bin("rustc"),
            rustc_version: "rustc 1.98.1".into(),
            rustup_toolchain: "1.98.1-aarch64-apple-darwin".into(),
            configuration: "cache-priming-check-on-save-and-proc-macro-disabled-v1".into(),
            trust: "local-trusted-v1".into(),
            transport: "stdio-v1".into(),
            cache_namespace: cache_namespace.display().to_string(),
        })
        .unwrap()
    };
    let first_profile = build_profile(&first_cache_namespace);
    let second_profile = build_profile(&second_cache_namespace);
    let first_worktree = worktree(first_root.to_str().unwrap(), 1);
    let second_worktree = worktree(second_root.to_str().unwrap(), 1);
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 2,
        per_backend_views: 1,
    })
    .unwrap();
    let mut views = RustViews::default();
    let owner = OwnerId::new("real-rust-contract").unwrap();
    let first = match views.request(
        &first_profile,
        &first_worktree,
        &mut registry,
        &mut admission,
        owner.clone(),
        AdmissionClass::Interactive,
    ) {
        RustViewAdmission::Granted(view) => view,
        outcome => panic!("first Rust request was not admitted: {outcome:?}"),
    };
    let mut first_request = request(&first_profile, &first_worktree);
    let first_active = first_request.fresh();
    let first_child = RustProtocolChild::spawn(
        &first_request.request,
        &first_worktree,
        &mut registry,
        first.lease(),
        Some(first_active),
        8192,
    )
    .unwrap();
    assert!(matches!(
        registry.take_spawn_lease(first.lease()),
        Err(agent_ide::execution::ProviderLeaseError::SpawnUnavailable)
    ));
    assert!(matches!(
        views.request(
            &second_profile,
            &second_worktree,
            &mut registry,
            &mut admission,
            owner,
            AdmissionClass::Interactive,
        ),
        RustViewAdmission::Queued(_)
    ));
    assert_eq!(admission.running_count(), 1);
    let first_reaped = semantic_probe(first_child, &first_root, &first_uri, "u32").await;
    let release = views.release(&mut registry, first.lease()).unwrap();

    assert_eq!(admission.running_count(), 1);
    let promotions = registry
        .complete_reap(&mut admission, release, first_reaped)
        .unwrap();
    assert_eq!(promotions.len(), 1);
    let second = views
        .promote(
            &second_profile,
            &second_worktree,
            &mut registry,
            &mut admission,
            promotions.into_iter().next().unwrap(),
        )
        .unwrap();
    assert_eq!(admission.running_count(), 1);
    let mut second_request = request(&second_profile, &second_worktree);
    let second_active = second_request.fresh();
    let second_child = RustProtocolChild::spawn(
        &second_request.request,
        &second_worktree,
        &mut registry,
        second.lease(),
        Some(second_active),
        8192,
    )
    .unwrap();
    let second_reaped = semantic_probe(second_child, &second_root, &second_uri, "String").await;
    let release = views.release(&mut registry, second.lease()).unwrap();

    assert!(
        registry
            .complete_reap(&mut admission, release, second_reaped)
            .unwrap()
            .is_empty()
    );
    assert_eq!(admission.running_count(), 0);
    assert_ne!(
        first_profile.compatibility_key(&first_worktree),
        second_profile.compatibility_key(&second_worktree)
    );
    assert_ne!(first_cache_namespace, second_cache_namespace);
    fs::remove_dir_all(first_root).unwrap();
    fs::remove_dir_all(second_root).unwrap();
}

/// Exercises the accepted Rust settings/version/status barrier through the production Session API.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rust_production_session_uses_exact_profile_and_barrier() {
    use agent_ide::{
        app::{
            config::StoreConfig,
            store::{OperationId, Store},
        },
        intelligence::{
            context::{ContextMode, ContextQuery},
            freshness::{DiagnosticReadiness, ViewGeneration},
            session::{ProviderSettings, SessionOptions, with_session},
        },
        workspace::{
            observation::{ObservationRef, SourceBytes, SourceCoverage, SourceRevision},
            store::{ObservationAdmission, ObservationDraft, WorkspaceStore},
        },
    };
    let (root, _, cache_namespace) = project("production", "u32", "42");
    let profile = profile(&cache_namespace);
    let worktree = worktree(root.to_str().unwrap(), 1);
    let text = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    let store = Store::open(
        &root.join("observations.sqlite"),
        StoreConfig {
            queue_capacity: 8,
            busy_timeout: Duration::from_secs(1),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 16,
        },
    )
    .unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let observed = match workspace
        .record(
            ObservationDraft::present(
                worktree.worktree().clone(),
                1,
                OperationId::new("rust-source").unwrap(),
                ObservationRef::new("rust-source").unwrap(),
                "src/lib.rs".into(),
                SourceBytes::from_bytes(text.as_bytes()),
                SourceRevision::new("rust-source-v1").unwrap(),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(observed) => observed,
        other => panic!("observation: {other:?}"),
    };
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 1,
        per_backend_views: 1,
    })
    .unwrap();
    let mut views = RustViews::default();
    let view = match views.request(
        &profile,
        &worktree,
        &mut registry,
        &mut admission,
        OwnerId::new("rust-production").unwrap(),
        AdmissionClass::Interactive,
    ) {
        RustViewAdmission::Granted(view) => view,
        other => panic!("admission: {other:?}"),
    };
    let mut bound = request(&profile, &worktree);
    let active = bound.fresh();
    let mut child = RustProtocolChild::spawn(
        &bound.request,
        &worktree,
        &mut registry,
        view.lease(),
        Some(active),
        8192,
    )
    .unwrap();
    let (stdout, stdin) = child.pipes();
    let outcome=with_session(stdout,stdin,worktree.worktree().clone(),1,ViewGeneration {backend:view.generation(),configuration:1,toolchain:1,view:1},ProviderSettings::new(profile.clone()),SessionOptions{request_timeout:Duration::from_secs(40),lifetime:Duration::from_secs(70)},|mut session|async move{
        assert!(session.provider_readiness().is_ready());
        assert_eq!(session.capabilities().server_info.as_ref().unwrap().version.as_deref(),Some(ANALYZER_VERSION));
        assert!(matches!(session.settings().downcast_ref::<RustProfile>(),Some(profile) if profile.configuration()=="cache-priming-check-on-save-and-proc-macro-disabled-v1"));
        let context=session.context(&observed,text.as_bytes(),ContextQuery::Symbol{byte_offset:text.rfind("answer").unwrap()}).await?;
        assert_eq!(context.mode,ContextMode::Semantic,"{context:?}");assert!(!context.definitions.unwrap().is_empty());
        assert_eq!(session.diagnostics().readiness,DiagnosticReadiness::Clean);
        session.shutdown().await?;
        assert!(session.context(&observed,text.as_bytes(),ContextQuery::File).await.is_err());
        Ok(())
    }).await;
    let reaped = if outcome.is_ok() {
        child.reap(Duration::from_secs(10)).await
    } else {
        child
            .cancel_and_reap(Duration::from_millis(100), Duration::from_secs(10))
            .await
    }
    .unwrap();
    let release = views.release(&mut registry, view.lease()).unwrap();
    assert_eq!(admission.running_count(), 1);
    registry
        .complete_reap(&mut admission, release, reaped.proof)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
    assert!(
        outcome.is_ok(),
        "production Rust failed: {outcome:?}; stderr={}",
        String::from_utf8_lossy(&reaped.stderr.bytes)
    );
    fs::remove_dir_all(root).unwrap();
}

/// A relative cache namespace would resolve against the spawned child's working directory (the
/// worktree root) and write Cargo/target cache into the user's repository; the profile must
/// refuse it, exactly the state the interrupted fixture in this file once relied on implicitly.
#[test]
fn rust_profile_rejects_a_relative_cache_namespace() {
    let mut identity = RustProfileIdentity {
        binary: PathBuf::from("/usr/bin/true"),
        rust_analyzer_version: format!("rust-analyzer {ANALYZER_VERSION}"),
        cargo: PathBuf::from("/usr/bin/true"),
        cargo_home: None,
        cargo_version: "cargo 1.98.1".into(),
        rustc: PathBuf::from("/usr/bin/true"),
        rustc_version: "rustc 1.98.1".into(),
        rustup_toolchain: "1.98.1-aarch64-apple-darwin".into(),
        configuration: "cache-priming-check-on-save-and-proc-macro-disabled-v1".into(),
        trust: "local-trusted-v1".into(),
        transport: "stdio-v1".into(),
        cache_namespace: "relative-cache-label".into(),
    };
    assert!(
        RustProfile::new(identity.clone()).is_err(),
        "relative cache_namespace must be rejected"
    );
    identity.cache_namespace = "/private/tmp/agent-ide-rust-cache-namespace".into();
    assert!(
        RustProfile::new(identity).is_ok(),
        "absolute cache_namespace must be accepted"
    );
}
