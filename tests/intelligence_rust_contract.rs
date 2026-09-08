//! Contract checks for the exclusive rust-analyzer v0.1 profile.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::{
    execution::{
        AdmissionClass, AdmissionController, AdmissionLimits, ExecutionProfileCatalog,
        ExecutionProfileTemplate, HostSandboxState, LocalExecutionPolicy, OwnerId, ProfileClass,
        ProviderLeaseLimits, ProviderLeaseRegistry, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
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

/// Builds the complete fixed Rust compatibility identity used by every exclusive request.
fn profile() -> RustProfile {
    RustProfile::new(RustProfileIdentity {
        binary: PathBuf::from("/Users/pluto/.local/bin/rust-analyzer"),
        rust_analyzer_version: format!("rust-analyzer {ANALYZER_VERSION}"),
        cargo_version: "cargo 1.98.1".into(),
        rustc_version: "rustc 1.98.1".into(),
        rustup_toolchain: "1.98.1-aarch64-apple-darwin".into(),
        configuration: "cache-priming-disabled-v1".into(),
        trust: "local-trusted-v1".into(),
        transport: "stdio-v1".into(),
        cache_namespace: "rust-native-v1".into(),
    })
    .unwrap()
}

/// Creates a private Cargo project whose `item` returns the given Rust type and expression.
/// Returns its root and source URI; the successful caller removes the generated project.
fn project(label: &str, result: &str, expression: &str) -> (PathBuf, String) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "agent-ide-rust-contract-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(root.join("src")).unwrap();
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
    (root, uri)
}

/// Builds the disabled-host validated request that is the sole child-spawn input in this probe.
fn request(profile: &RustProfile, worktree: &RustWorktree) -> ValidatedExecutionRequest {
    let sandbox = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": worktree.worktree().worktree_path(),
        "useLegacyLandlock": false
    })))
    .unwrap();
    assert_eq!(sandbox.class(), ProfileClass::Disabled);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("rust-contract", 1, &sandbox).unwrap(),
    ])
    .unwrap();
    let policy = LocalExecutionPolicy::new(
        BTreeSet::from([profile.binary().to_path_buf()]),
        4096,
        1,
        true,
    )
    .unwrap();
    ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("rust-contract", sandbox).unwrap(),
        worktree.authority().clone(),
        profile.command(worktree).unwrap(),
        &policy,
        &catalog,
    )
    .unwrap()
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
async fn semantic_probe(mut child: RustProtocolChild, root: &Path, uri: &str, expected: &str) {
    let result = timeout(
        Duration::from_secs(60),
        semantic_session(&mut child, root, uri, expected),
    )
    .await;
    let stderr = child.reap(Duration::from_secs(10)).await.unwrap();
    assert!(
        matches!(result, Ok(Ok(()))),
        "semantic probe failed: {result:?}; stderr={} (truncated={}, complete={})",
        String::from_utf8_lossy(&stderr.bytes),
        stderr.truncated,
        stderr.complete
    );
}

/// Proves divergent worktrees never share an exclusive Rust backend and the second demand queues.
#[test]
fn exclusive_rust_requests_are_distinct_and_source_results_are_generation_scoped() {
    let first_worktree = worktree("/private/tmp/agent-ide-rust-first", 1);
    let second_worktree = worktree("/private/tmp/agent-ide-rust-second", 1);
    let profile = profile();
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
    let analyzer = PathBuf::from(
        std::env::var("AGENT_IDE_RUST_ANALYZER")
            .expect("AGENT_IDE_RUST_ANALYZER must name the verified rust-analyzer binary"),
    );
    let profile = RustProfile::new(RustProfileIdentity {
        binary: analyzer,
        rust_analyzer_version: format!("rust-analyzer {ANALYZER_VERSION}"),
        cargo_version: "cargo 1.98.1".into(),
        rustc_version: "rustc 1.98.1".into(),
        rustup_toolchain: "1.98.1-aarch64-apple-darwin".into(),
        configuration: "cache-priming-disabled-v1".into(),
        trust: "local-trusted-v1".into(),
        transport: "stdio-v1".into(),
        cache_namespace: "rust-native-v1".into(),
    })
    .unwrap();
    let (first_root, first_uri) = project("first", "u32", "42");
    let (second_root, second_uri) = project("second", "String", "String::new()");
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
        &profile,
        &first_worktree,
        &mut registry,
        &mut admission,
        owner.clone(),
        AdmissionClass::Interactive,
    ) {
        RustViewAdmission::Granted(view) => view,
        outcome => panic!("first Rust request was not admitted: {outcome:?}"),
    };
    let first_child = RustProtocolChild::spawn(
        &request(&profile, &first_worktree),
        &first_worktree,
        &mut registry,
        first.lease(),
        Path::new("/usr/bin/true"),
        8192,
    )
    .unwrap();
    assert!(matches!(
        registry.take_spawn_lease(first.lease()),
        Err(agent_ide::execution::ProviderLeaseError::SpawnUnavailable)
    ));
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
    semantic_probe(first_child, &first_root, &first_uri, "u32").await;
    let release = views
        .release(&mut registry, &mut admission, first.lease())
        .unwrap();
    assert!(matches!(
        release.backend,
        agent_ide::execution::BackendRelease::ReapOwned { .. }
    ));
    assert_eq!(release.promotions.len(), 1);
    let second = views
        .promote(
            &profile,
            &second_worktree,
            &mut registry,
            &mut admission,
            release.promotions.into_iter().next().unwrap(),
        )
        .unwrap();
    assert_eq!(admission.running_count(), 1);
    let second_child = RustProtocolChild::spawn(
        &request(&profile, &second_worktree),
        &second_worktree,
        &mut registry,
        second.lease(),
        Path::new("/usr/bin/true"),
        8192,
    )
    .unwrap();
    semantic_probe(second_child, &second_root, &second_uri, "String").await;
    let release = views
        .release(&mut registry, &mut admission, second.lease())
        .unwrap();
    assert!(matches!(
        release.backend,
        agent_ide::execution::BackendRelease::ReapOwned { .. }
    ));
    assert!(release.promotions.is_empty());
    assert_eq!(admission.running_count(), 0);
    assert_ne!(
        profile.compatibility_key(&first_worktree),
        profile.compatibility_key(&second_worktree)
    );
    fs::remove_dir_all(first_root).unwrap();
    fs::remove_dir_all(second_root).unwrap();
}
