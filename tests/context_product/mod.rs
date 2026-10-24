//! Full MCP task-context delivery, with external inference restricted to an explicit ignored run.
use super::*;

/// Operator-only full Voyage smoke (explicit configuration and evidence; ignored by the gate).
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
mod live;

/// Rule line the fixture's `AGENTS.md` carries; the packet must deliver it verbatim.
const FIXTURE_RULE: &str = "- Keep tokenizer changes covered by a unit test in the same crate.";

/// Executes start → exactly one `ide.prepare` → edit → stop and checks the one-call contract.
///
/// The packet must arrive in that single reply: no pending state, no continuation or page, no
/// pointer to `ide.inspect`, every numbered source row equal to the fixture file's line, the
/// project rule and manifest delivered verbatim, and a delivered file edited with no read in
/// between. Returns a summary: status, delivered paths, usage line and the packet text.
async fn roundtrip(
    models: Value,
    evidence: Option<&Path>,
    env: &[(&str, &str)],
    budget: u64,
) -> Value {
    roundtrip_with(models, offline_indexing(), evidence, env, budget).await
}

/// Background indexing that never calls a model: the local, lexical index only, published fast.
fn offline_indexing() -> Value {
    json!({"enabled":true,"max_new_calls":0,"max_input_bytes":0,"debounce_ms":50})
}

/// [`roundtrip`] with an explicit `task_context.indexing` configuration.
async fn roundtrip_with(
    models: Value,
    indexing: Value,
    evidence: Option<&Path>,
    env: &[(&str, &str)],
    budget: u64,
) -> Value {
    let fixture = ProductFixture::new(json!([]));
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("crates/agent-ide-lang-css/src");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file()
            && entry.path().extension().is_some_and(|ext| ext == "rs")
        {
            std::fs::copy(
                entry.path(),
                fixture.root.join("src").join(entry.file_name()),
            )
            .unwrap();
        }
    }
    std::fs::write(
        fixture.root.join("AGENTS.md"),
        format!("# Rules\n{FIXTURE_RULE}\n"),
    )
    .unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = json!({"enabled":true,"models":models,"indexing":indexing});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    // A fixture toolchain lets the edit step format and check without the operator's Rust.
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, env)
        .await;
    let mut actor = ProductActor::new(&fixture, "context-example").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let task = "Correct handling of braces inside quoted strings in the stylesheet tokenizer while preserving comments, URLs and interpolation. Identify the implementation and relevant tests before editing.";
    let request = json!({"task":task,"token_budget":budget});
    // `ide.prepare` never builds the index: wait until the background indexer published it.
    let ready_by = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let probe = actor
            .call_raw(&fixture, "ide.prepare", request.clone())
            .await;
        let text = probe["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !text.starts_with("prepare: not_ready") {
            break;
        }
        assert!(
            std::time::Instant::now() < ready_by,
            "index not ready: {text}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // Exactly one ordinary call: no settle, no inspect.
    let calls_before = actor.next;
    let began = std::time::Instant::now();
    let raw = actor
        .call_raw(&fixture, "ide.prepare", request.clone())
        .await;
    assert_eq!(actor.next, calls_before + 1, "exactly one MCP request");
    let reply = raw["result"]["structuredContent"].clone();
    assert_eq!(reply["state"], "complete", "one call must answer: {raw}");
    assert_ne!(reply["continuation"], true, "{reply}");
    assert_ne!(reply["truncated"], true, "{reply}");
    assert!(
        reply["detail_ref"].is_null(),
        "no continuation reference: {reply}"
    );
    let text = reply["text"].as_str().unwrap().to_owned();
    assert!(
        !text.contains("ide.inspect") && !text.starts_with("page "),
        "{text}"
    );
    let status = text
        .strip_prefix("prepare: ")
        .and_then(|rest| rest.split(';').next())
        .unwrap_or_else(|| panic!("status line: {text}"))
        .to_owned();
    assert!(
        ["complete", "partial", "needs_evidence"].contains(&status.as_str()),
        "{text}"
    );
    // Rules and manifest are delivered verbatim unless named as omitted.
    assert!(
        (text.contains("--- AGENTS.md") && text.contains(FIXTURE_RULE))
            || text.contains("- AGENTS.md: omitted"),
        "project rule delivered verbatim: {text}"
    );
    assert!(
        text.contains("--- Cargo.toml") || text.contains("- Cargo.toml: omitted"),
        "manifest delivered: {text}"
    );
    // Every numbered row of every file block is the fixture file's own line.
    let mut paths = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for row in text.lines() {
        if let Some(header) = row.strip_prefix("## ") {
            let path = header.split(" — ").next().unwrap().to_owned();
            lines = std::fs::read_to_string(fixture.root.join(&path))
                .unwrap()
                .lines()
                .map(str::to_owned)
                .collect();
            paths.push(path);
        } else if !paths.is_empty()
            && let Some((number, line)) = row.split_once('\t')
            && let Ok(number) = number.trim().parse::<usize>()
        {
            assert_eq!(
                lines[number - 1],
                line,
                "{} line {number}",
                paths.last().unwrap()
            );
        }
    }
    assert_eq!(status == "needs_evidence", paths.is_empty(), "{text}");
    assert_eq!(
        status == "complete",
        !text.contains("\nomitted:\n"),
        "complete only without omissions: {text}"
    );
    // A delivered file is editable at once: no read between prepare and edit.
    if let Some(edited) = paths.last().cloned() {
        let first_line = std::fs::read_to_string(fixture.root.join(&edited))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let edit = actor
            .call(
                &fixture,
                "ide.edit",
                json!({"operation_id":"after-prepare","files":[{"path":edited,"changes":[
                    {"old":first_line,"new":format!("{first_line} ")}
                ]}]}),
            )
            .await;
        assert_eq!(
            edit["files"][0]["state"], "replaced",
            "edit without a read: {edit}"
        );
    }
    let usage = text
        .lines()
        .find(|row| row.starts_with("usage: "))
        .unwrap_or_else(|| panic!("usage line: {text}"))
        .to_owned();
    let summary = json!({
        "status":status, "paths":paths, "usage":usage, "text":text,
        "prepare_ms": u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX)
    });
    if let Some(evidence) = evidence {
        std::fs::create_dir_all(evidence).unwrap();
        std::fs::set_permissions(evidence, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (name, value) in [
            ("request.json", request),
            ("mcp-responses.json", json!([raw])),
            ("delivered-packet.json", summary.clone()),
        ] {
            let path = evidence.join(name);
            std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let packet = evidence.join("delivered-packet.txt");
        std::fs::write(&packet, &text).unwrap();
        std::fs::set_permissions(packet, std::fs::Permissions::from_mode(0o600)).unwrap();
        let journal = fixture.root.join(".agent-ide/requests");
        if journal.is_dir() {
            let destination = evidence.join("requests");
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700)).unwrap();
            for entry in std::fs::read_dir(journal).unwrap() {
                let entry = entry.unwrap();
                std::fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
            }
        }
    }
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);
    summary
}

/// The public tool works with both model backends disabled, using source facts only.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_mcp_offline_roundtrip() {
    let evidence = std::env::var_os("AGENT_IDE_CONTEXT_OFFLINE_EVIDENCE").map(PathBuf::from);
    let packet = roundtrip(
        json!({"embedding":null,"jev":null,"max_new_calls":0,"include_threshold":0.5}),
        evidence.as_deref(),
        &[],
        12000,
    )
    .await;
    assert!(
        packet["usage"]
            .as_str()
            .unwrap()
            .starts_with("usage: embeddings off, jev off; model calls 0 "),
        "{packet}"
    );
}

/// A preparation slower than the ordinary eight-second inline wait still answers in that one
/// call: no pending state, no `ide.inspect`, the whole packet.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_mcp_slow_preparation_answers_in_one_call() {
    let packet = roundtrip(
        json!({"embedding":null,"jev":null,"max_new_calls":0,"include_threshold":0.5}),
        None,
        &[("AGENT_IDE_TEST_PREPARE_DELAY_MS", "12000")],
        12000,
    )
    .await;
    assert!(packet["prepare_ms"].as_u64().unwrap() >= 12_000, "{packet}");
    assert_ne!(packet["status"], "needs_evidence", "{packet}");
}

/// A legacy `token_budget` no longer limits the packet: even 100, which the old validator
/// refused (1024..32000), is accepted, answered with an explicit warning, and the packet is
/// several times larger than that budget with nothing omitted for size.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_mcp_legacy_budget_never_cuts_the_packet() {
    let packet = roundtrip(
        json!({"embedding":null,"jev":null,"max_new_calls":0,"include_threshold":0.5}),
        None,
        &[],
        100,
    )
    .await;
    let text = packet["text"].as_str().unwrap();
    assert!(text.len() > 4 * 1024, "{} bytes: {text}", text.len());
    assert!(
        text.contains("token_budget is ignored: the packet carries every selected unit whole"),
        "{text}"
    );
    assert!(!text.contains("omitted to fit"), "{text}");
}

/// With background indexing disabled no index is ever built and `ide.prepare` answers a final
/// `not_ready` in that one call: no pending state, nothing delivered, the cause named.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_mcp_without_index_is_not_ready() {
    let fixture = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = json!({"enabled":true,"models":{
        "embedding":null,"jev":null,"max_new_calls":0,"include_threshold":0.5}});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "context-not-ready").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let raw = actor
        .call_raw(
            &fixture,
            "ide.prepare",
            json!({"task":"Fix the tokenizer."}),
        )
        .await;
    let reply = &raw["result"]["structuredContent"];
    assert_eq!(reply["state"], "complete", "{raw}");
    let text = reply["text"].as_str().unwrap();
    assert!(
        text.starts_with("prepare: not_ready; 0 files")
            && text.contains("background indexing is disabled"),
        "{text}"
    );
    assert!(
        !fixture.root.join(".agent-ide").exists(),
        "prepare must not create an index"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);
}

/// Local embedding stub: answers every request with fixed two-dimensional vectors and records
/// each request body; it listens on loopback only and stops with the test.
struct EmbeddingStub {
    url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddingStub {
    /// Starts the stub on an ephemeral loopback port.
    fn start() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/embeddings", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (output, stopping) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let (start, length) = loop {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                        let length = header
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map(|value| value.trim().parse::<usize>().unwrap())
                            .unwrap();
                        break (end + 4, length);
                    }
                };
                while bytes.len() < start + length {
                    let mut chunk = [0; 4096];
                    let n = stream.read(&mut chunk).unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let request: Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
                let data: Vec<Value> = (0..request["input"].as_array().unwrap().len())
                    .map(|index| json!({"index":index,"embedding":[1.0,0.0]}))
                    .collect();
                output.lock().unwrap().push(request);
                let body =
                    json!({"model":"fixture","data":data,"usage":{"prompt_tokens":10}}).to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// Every document input the stub received so far, in arrival order.
    fn document_inputs(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request["input_type"] == "document")
            .flat_map(|request| request["input"].as_array().unwrap().clone())
            .map(|input| input.as_str().unwrap().to_owned())
            .collect()
    }
}

impl Drop for EmbeddingStub {
    /// Stops and joins the stub thread.
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}

/// Background indexing end to end against a local embedding stub: `ide.start` builds and
/// publishes the index in the background, one `ide.prepare` answers from it, a source edit
/// re-embeds only the changed fragments, and `ide.stop` ends indexing (no later requests).
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn background_indexing_reembeds_only_changed_fragments() {
    let stub = EmbeddingStub::start();
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(
        fixture.root.join("src/other.rs"),
        "/// Stable helper.\npub fn other() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = json!({
        "enabled": true,
        "models": {
            "embedding": {
                "backend": "local", "endpoint": stub.url, "model": "fixture",
                "response_model": null, "revision": "v1", "dimensions": 2,
                "credential_file": null, "egress_allowed": false, "timeout_ms": 3000,
                "max_batch": 128, "max_input_bytes": 4096,
                "query_input_type": "query", "document_input_type": "document"
            },
            "jev": null, "max_new_calls": 8, "include_threshold": 0.5
        },
        "indexing": {
            "enabled": true, "max_new_calls": 8, "max_input_bytes": 1048576,
            "initial_allowed": true, "debounce_ms": 50
        }
    });
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "background-index").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let request = json!({"task":"Change what value returns.","token_budget":4000});
    let ready_by = std::time::Instant::now() + Duration::from_secs(60);
    let text = loop {
        let raw = actor
            .call_raw(&fixture, "ide.prepare", request.clone())
            .await;
        let text = raw["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !text.starts_with("prepare: not_ready") {
            break text;
        }
        assert!(
            std::time::Instant::now() < ready_by,
            "index not ready: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(text.contains("usage: embeddings used"), "{text}");
    let first = stub.document_inputs();
    // A ready `ide.prepare` never embeds source documents. Repeating the same task replays its
    // journaled query embedding (no request); a different task sends exactly one query.
    let before = stub.requests.lock().unwrap().len();
    for (task, new_requests) in [
        ("Change what value returns.", 0),
        ("Explain how the helper module is documented and tested.", 1),
    ] {
        let raw = actor
            .call_raw(
                &fixture,
                "ide.prepare",
                json!({"task":task,"token_budget":4000}),
            )
            .await;
        assert!(
            !raw["result"]["structuredContent"]["text"]
                .as_str()
                .unwrap_or_default()
                .starts_with("prepare: not_ready"),
            "{raw}"
        );
        assert_eq!(
            stub.document_inputs().len(),
            first.len(),
            "prepare embedded documents"
        );
        let requests = stub.requests.lock().unwrap();
        assert_eq!(requests.len(), before + new_requests, "{task}");
        assert!(
            requests[before..]
                .iter()
                .all(|request| request["input_type"] == "query"),
            "only query requests"
        );
    }
    assert!(
        first.iter().any(|input| input.contains("fn other")),
        "every file embedded once: {first:?}"
    );
    // Edit one function; only fragments of the edited file are embedded again.
    let edit = actor
        .call(
            &fixture,
            "ide.edit",
            json!({"operation_id":"index-edit","files":[{"path":"src/lib.rs","changes":[
                {"old":"pub fn value() -> i32 { 7 }","new":"pub fn value() -> i32 { 8 }"}
            ]}]}),
        )
        .await;
    assert_eq!(edit["files"][0]["state"], "replaced", "{edit}");
    let edited_by = std::time::Instant::now() + Duration::from_secs(30);
    let again = loop {
        let all = stub.document_inputs();
        if all.len() > first.len() {
            break all[first.len()..].to_vec();
        }
        assert!(
            std::time::Instant::now() < edited_by,
            "edit was not re-indexed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        again.iter().any(|input| input.contains("{ 8 }")),
        "the changed fragment is embedded: {again:?}"
    );
    assert!(
        again.iter().all(|input| !input.contains("fn other")),
        "unchanged files are not embedded again: {again:?}"
    );
    assert!(again.len() < first.len(), "{again:?} vs {first:?}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    // Stop ended the activation's indexing: a later change triggers no embedding request.
    let after_stop = stub.requests.lock().unwrap().len();
    std::fs::write(
        fixture.root.join("src/other.rs"),
        "pub fn other() -> u8 {\n    2\n}\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(stub.requests.lock().unwrap().len(), after_stop);
    drop(actor);
    drop(daemon);
}

/// `task_context` with the embedding stub at `url`, Jev off and the given `indexing`.
fn stub_task_context(url: &str, indexing: Value) -> Value {
    json!({
        "enabled": true,
        "models": {
            "embedding": {
                "backend": "local", "endpoint": url, "model": "fixture",
                "response_model": null, "revision": "v1", "dimensions": 2,
                "credential_file": null, "egress_allowed": false, "timeout_ms": 3000,
                "max_batch": 128, "max_input_bytes": 4096,
                "query_input_type": "query", "document_input_type": "document"
            },
            "jev": null, "max_new_calls": 8, "include_threshold": 0.5
        },
        "indexing": indexing
    })
}

/// A change written straight to disk, with no IDE edit and so no hook, is picked up by the
/// periodic reconcile: quiet reconciles send nothing, the changed file's fragment alone is
/// embedded again, and after `ide.stop` no reconcile runs at all.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn background_reconcile_picks_up_external_edits() {
    let stub = EmbeddingStub::start();
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(
        fixture.root.join("src/other.rs"),
        "/// Stable helper.\npub fn other() -> u8 {\n    1\n}\n",
    )
    .unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = stub_task_context(
        &stub.url,
        json!({
            "enabled": true, "max_new_calls": 8, "max_input_bytes": 1048576,
            "initial_allowed": true, "debounce_ms": 50, "reconcile_ms": 200
        }),
    );
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "background-reconcile").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let request = json!({"task":"Change what value returns.","token_budget":4000});
    let ready_by = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let raw = actor
            .call_raw(&fixture, "ide.prepare", request.clone())
            .await;
        let text = raw["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if !text.starts_with("prepare: not_ready") {
            break;
        }
        assert!(
            std::time::Instant::now() < ready_by,
            "index not ready: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let first = stub.document_inputs();
    assert!(
        first.iter().any(|input| input.contains("fn other")),
        "every file embedded once: {first:?}"
    );
    // Several reconciles over unchanged sources embed nothing.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(stub.document_inputs().len(), first.len(), "quiet reconcile");
    // A change made outside the IDE: no hook fires, the reconcile finds it.
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> i32 { 9 }\npub fn caller() -> i32 { value() }\n",
    )
    .unwrap();
    let found_by = std::time::Instant::now() + Duration::from_secs(30);
    let again = loop {
        let all = stub.document_inputs();
        if all.len() > first.len() {
            break all[first.len()..].to_vec();
        }
        assert!(
            std::time::Instant::now() < found_by,
            "external edit was not reconciled"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        again.iter().any(|input| input.contains("{ 9 }")),
        "the changed fragment is embedded: {again:?}"
    );
    assert!(
        again.iter().all(|input| !input.contains("fn other")),
        "unchanged files are not embedded again: {again:?}"
    );
    assert!(again.len() < first.len(), "{again:?} vs {first:?}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    // After stop no reconcile runs: a later external change sends nothing.
    let after_stop = stub.requests.lock().unwrap().len();
    std::fs::write(
        fixture.root.join("src/other.rs"),
        "pub fn other() -> u8 {\n    2\n}\n",
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(stub.requests.lock().unwrap().len(), after_stop);
    drop(actor);
    drop(daemon);
}

/// Probes `ide.prepare` until its answer starts with something other than `not_ready`
/// (`ready`) or contains `marker` while still not ready; returns the last text.
async fn probe_prepare(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    request: &Value,
    marker: Option<&str>,
) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let raw = actor
            .call_raw(fixture, "ide.prepare", request.clone())
            .await;
        let text = raw["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let not_ready = text.starts_with("prepare: not_ready");
        match marker {
            None if !not_ready => return text,
            Some(marker) if not_ready && text.contains(marker) => return text,
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "prepare never reached it: {text}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The default safe configuration: an embedding model is configured but a first full model
/// indexing is not allowed. The background pass sends no request at all, and `ide.prepare`
/// answers `not_ready` naming the free cost estimate the operator decides on.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn first_full_indexing_waits_for_the_operator_and_sends_nothing() {
    let stub = EmbeddingStub::start();
    let fixture = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = stub_task_context(
        &stub.url,
        json!({"enabled": true, "max_new_calls": 8, "max_input_bytes": 1048576, "debounce_ms": 50}),
    );
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "index-preflight").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let request = json!({"task":"Change what value returns.","token_budget":4000});
    let text = probe_prepare(
        &mut actor,
        &fixture,
        &request,
        Some("awaits the operator's preflight"),
    )
    .await;
    assert!(
        text.contains("input tokens") && text.contains("requests"),
        "the estimate is named: {text}"
    );
    assert!(
        stub.requests.lock().unwrap().is_empty(),
        "no model request before the operator allows a first full indexing"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);
    assert!(stub.requests.lock().unwrap().is_empty());
}

/// A daemon restart reuses the published index: after a new `ide.start` the background pass
/// finds nothing changed and sends no document request; `ide.prepare` answers at once.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn restarted_daemon_reuses_the_published_index() {
    let stub = EmbeddingStub::start();
    let fixture = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = stub_task_context(
        &stub.url,
        json!({
            "enabled": true, "max_new_calls": 8, "max_input_bytes": 1048576,
            "initial_allowed": true, "debounce_ms": 50
        }),
    );
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let request = json!({"task":"Change what value returns.","token_budget":4000});
    let daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "index-before-restart").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    probe_prepare(&mut actor, &fixture, &request, None).await;
    let indexed = stub.document_inputs().len();
    assert!(indexed > 0, "the first daemon indexed the project");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);

    let daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "index-after-restart").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let text = probe_prepare(&mut actor, &fixture, &request, None).await;
    assert!(text.contains("usage: embeddings used"), "{text}");
    // Give the restarted activation's background pass time to run (debounce 50 ms).
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        stub.document_inputs().len(),
        indexed,
        "restart re-embedded unchanged documents"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);
}

/// Operator-only inference: config and evidence paths are explicit; no normal gate reads them.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
#[ignore = "explicit operator model calls and private evidence output"]
async fn task_context_mcp_live_example() {
    let config = std::env::var_os("AGENT_IDE_CONTEXT_EXAMPLE_CONFIG")
        .expect("explicit model config required");
    let evidence = PathBuf::from(
        std::env::var_os("AGENT_IDE_CONTEXT_EXAMPLE_EVIDENCE")
            .expect("explicit evidence path required"),
    );
    let models: Value = serde_json::from_slice(&std::fs::read(config).unwrap()).unwrap();
    // The operator's explicit run allows the first full indexing within a bounded budget.
    let indexing = json!({
        "enabled": true, "initial_allowed": true, "max_new_calls": 64,
        "max_input_bytes": 1048576, "debounce_ms": 50
    });
    let packet = roundtrip_with(models, indexing, Some(&evidence), &[], 12000).await;
    // Jev stays optional in the operator's model configuration; embeddings must have been used.
    let usage = packet["usage"].as_str().unwrap();
    assert!(
        usage.starts_with("usage: embeddings used, jev "),
        "{packet}"
    );
}

/// Exercises one prepare with either an admitted failure or an unavailable public target.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
async fn public_check_roundtrip(target: Value, expect_failure: bool) {
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> u32 { 1 }\n#[test]\nfn public_failure() { assert_eq!(value(), 2); }\n",
    )
    .unwrap();
    configure_prepared_rust_checks(&fixture);
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = json!({"enabled":true,
        "models":{"embedding":null,"jev":null,"max_new_calls":0,"include_threshold":0.5},
        "indexing":offline_indexing(),
        "evidence":{"enabled":true,"target":target,"budget_ms":30000}});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "public-evidence-one-call").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let ready = agent_ide::project::context::ContextIndex::open_existing(&fixture.root)
            .ok()
            .flatten()
            .is_some_and(|index| index.index_report("lexical").ok().flatten().is_some());
        if ready {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background index did not publish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let before = actor.next;
    let raw = actor
        .call_raw(
            &fixture,
            "ide.prepare",
            json!({"task":if expect_failure { "Restore reliable behavior." } else { "Inspect value in src/lib.rs" },"token_budget":12000}),
        )
        .await;
    assert_eq!(actor.next, before + 1);
    let reply = &raw["result"]["structuredContent"];
    assert_eq!(reply["state"], "complete", "{raw}");
    let text = reply["text"].as_str().unwrap();
    if expect_failure {
        assert!(
            text.contains("public check") && text.contains("failed 1"),
            "{text}"
        );
    } else {
        assert!(text.contains("public check unavailable:"), "{text}");
        assert!(
            !text
                .lines()
                .any(|line| line.starts_with("note: public check ") && line.contains("; passed ")),
            "unavailable is not a clean test: {text}"
        );
    }
    assert!(
        text.contains("src/lib.rs") && text.contains("pub fn value"),
        "{text}"
    );
    assert_ne!(reply["continuation"], true, "{raw}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A single prepare executes the admitted check and returns its source-bound failure context.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_public_check_is_collected_in_one_call() {
    public_check_roundtrip(json!({"files":["src/lib.rs"]}), true).await;
}

/// Unsupported optional checks leave ordinary source context usable and expose the missing evidence.
#[cfg(all(feature = "jev-pilot", feature = "test-seams"))]
#[tokio::test]
async fn task_context_public_check_unavailable_keeps_source_context() {
    public_check_roundtrip(json!({"files":["README.md"]}), false).await;
}
