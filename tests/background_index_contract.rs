//! Background build/query separation and incremental source payloads over a real loopback HTTP stub.
#![cfg(feature = "jev-pilot")]
use agent_ide::project::{
    context::{
        ContextIndex, DocumentOptions, PacketOptions,
        embedding::{Backend, EmbeddingConfig},
        indexer::IndexBudget,
        service::{ContextService, ModelConfig},
    },
    structure::StructureOptions,
};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Owns the local request recorder and its bounded server thread; it never listens externally.
struct BackendStub {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    fail_next: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl BackendStub {
    /// Starts a no-auth fixture that returns normalized-compatible vectors and explicit usage.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/embeddings", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let output = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let fail_next = Arc::new(AtomicBool::new(false));
        let failing = fail_next.clone();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !stopping.load(Ordering::SeqCst) {
                assert!(Instant::now() < deadline, "fixture deadline");
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
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
                    assert!(bytes.len() < 1024 * 1024);
                    if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        assert!(!header.to_lowercase().contains("authorization:"));
                        let length = header
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
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
                let vectors: Vec<_> = request["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .enumerate()
                    .map(|(index, _)| json!({"index":index,"embedding":[1.0,0.0]}))
                    .collect();
                output.lock().unwrap().push(request);
                let failed = failing.swap(false, Ordering::SeqCst);
                let status = if failed {
                    "500 Internal Server Error"
                } else {
                    "200 OK"
                };
                let body = if failed {
                    json!({"error":"fixture failure"})
                } else {
                    json!({"model":"fixture","data":vectors,"usage":{"prompt_tokens":10}})
                }
                .to_string();
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
            fail_next,
            thread: Some(thread),
        }
    }
}
impl Drop for BackendStub {
    /// Stops and joins the only owned server, preserving panic evidence outside unwinding.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
/// Owns only this test's approved temporary directory.
struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The first pass embeds; repeat and query do not embed code; one method edit sends only that fragment.
#[tokio::test]
async fn background_only_embeddings_reuse_unchanged_fragments_and_survive_restart() {
    agent_ide::languages::install();
    let root =
        Fixture(std::env::temp_dir().join(format!("background-index-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    assert!(ContextIndex::open_existing(&root.0).unwrap().is_none());
    assert!(!root.0.join(".agent-ide").exists());
    assert!(!root.0.join(".gitignore").exists());
    let original = "//! Module contract.\npub struct Demo;\nimpl Demo {\n    pub fn change() -> &'static str { \"red\" }\n    pub fn stable() -> usize { 7 }\n}\n";
    std::fs::write(root.0.join("lib.rs"), original).unwrap();
    std::fs::write(root.0.join("other.rs"), "pub fn other() {}\n").unwrap();
    let server = BackendStub::start();
    let embedding = EmbeddingConfig {
        backend: Backend::Local,
        endpoint: server.url.clone(),
        model: "fixture".into(),
        response_model: None,
        revision: "v1".into(),
        dimensions: 2,
        credential_file: None,
        egress_allowed: false,
        timeout_ms: 3000,
        max_batch: 128,
        max_input_bytes: 4096,
        query_input_type: Some("query".into()),
        document_input_type: Some("document".into()),
    };
    let space = embedding.space_id().unwrap();
    let service = ContextService::new(ModelConfig {
        embedding: Some(embedding),
        jev: None,
        max_new_calls: 8,
        include_threshold: 0.5,
    })
    .unwrap();
    let scope = StructureOptions {
        max_depth: 64,
        respect_gitignore: false,
        ..Default::default()
    };
    let docs = DocumentOptions::default();
    let budget = IndexBudget {
        max_new_calls: 8,
        max_input_bytes: 1024 * 1024,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut index = ContextIndex::open(&root.0).unwrap();
    let first = service
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(first.outcome, "ok", "{first:?}");
    assert_eq!(first.new_calls, 1);
    assert!(first.fragments_embedded > 2);
    assert_eq!(first.fragments_pending, 0);
    let initial_inputs = server.requests.lock().unwrap()[0]["input"]
        .as_array()
        .unwrap()
        .clone();
    let second = service
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(second.new_calls, 0);
    assert_eq!(second.changed_files, 0);
    assert_eq!(second.fragments_embedded, 0);
    drop(index);
    let read = ContextIndex::open_existing(&root.0).unwrap().unwrap();
    let prepared = service
        .prepare_for_delivery(
            &read,
            "Demo change",
            &PacketOptions::default(),
            &docs,
            &cancel,
        )
        .await
        .unwrap();
    assert!(prepared.embedding_used);
    assert_eq!(prepared.calls.len(), 1);
    assert_eq!(server.requests.lock().unwrap()[1]["input_type"], "query");
    // The query's pinned read transaction must not block publication in another connection.
    let mut index = ContextIndex::open(&root.0).unwrap();
    std::fs::write(root.0.join("lib.rs"), original.replace("red", "blue")).unwrap();
    let changed = service
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(changed.outcome, "ok");
    assert_eq!(changed.changed_files, 1);
    assert_eq!(changed.new_calls, 1);
    assert_eq!(changed.fragments_embedded, 1);
    {
        let requests = server.requests.lock().unwrap();
        let sent = requests[2]["input"].as_array().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].as_str().unwrap().contains("blue"));
        assert!(!initial_inputs.contains(&sent[0]));
    }
    drop(read);
    std::fs::remove_file(root.0.join("other.rs")).unwrap();
    let deleted = service
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(deleted.new_calls, 0);
    assert_eq!(deleted.changed_files, 1);
    assert!(
        !index
            .published_documents(&space)
            .unwrap()
            .documents
            .iter()
            .any(|d| d.path == std::path::Path::new("other.rs"))
    );
    drop(index);
    let ready = ContextIndex::open_existing(&root.0).unwrap().unwrap();
    let before = server.requests.lock().unwrap().len();
    let again = service
        .prepare_for_delivery(
            &ready,
            "Demo change",
            &PacketOptions::default(),
            &docs,
            &cancel,
        )
        .await
        .unwrap();
    assert!(again.calls.iter().all(|c| c.replayed));
    assert_eq!(server.requests.lock().unwrap().len(), before);
}

/// An exhausted budget publishes no partly embedded file; resumption sends only the missing payloads.
#[tokio::test]
async fn capped_indexing_resumes_without_reembedding_completed_fragments() {
    agent_ide::languages::install();
    let root =
        Fixture(std::env::temp_dir().join(format!("background-budget-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(
        root.0.join("lib.rs"),
        "//! Contract\npub fn first() -> usize { 1 }\npub fn second() -> usize { 2 }\n",
    )
    .unwrap();
    let server = BackendStub::start();
    let config = EmbeddingConfig {
        backend: Backend::Local,
        endpoint: server.url.clone(),
        model: "fixture".into(),
        response_model: None,
        revision: "v1".into(),
        dimensions: 2,
        credential_file: None,
        egress_allowed: false,
        timeout_ms: 3000,
        max_batch: 1,
        max_input_bytes: 4096,
        query_input_type: Some("query".into()),
        document_input_type: Some("document".into()),
    };
    let space = config.space_id().unwrap();
    let service = ContextService::new(ModelConfig {
        embedding: Some(config),
        jev: None,
        max_new_calls: 32,
        include_threshold: 0.5,
    })
    .unwrap();
    let mut index = ContextIndex::open(&root.0).unwrap();
    let scope = StructureOptions {
        max_depth: 64,
        respect_gitignore: false,
        ..Default::default()
    };
    let docs = DocumentOptions::default();
    let cancel = tokio_util::sync::CancellationToken::new();
    let first = service
        .index_project(
            &mut index,
            &scope,
            &docs,
            &IndexBudget {
                max_new_calls: 1,
                max_input_bytes: 1024 * 1024,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert!(first.budget_exhausted);
    assert_eq!(first.new_calls, 1);
    assert!(first.fragments_pending > 0);
    assert!(
        index.published_documents(&space).is_err(),
        "one vector is not a complete published file"
    );
    drop(index);
    let mut index = ContextIndex::open(&root.0).unwrap();
    let rest = service
        .index_project(
            &mut index,
            &scope,
            &docs,
            &IndexBudget {
                max_new_calls: 32,
                max_input_bytes: 1024 * 1024,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(rest.outcome, "ok");
    assert_eq!(rest.fragments_pending, 0);
    let requests = server.requests.lock().unwrap();
    let inputs: Vec<_> = requests
        .iter()
        .map(|r| r["input"][0].as_str().unwrap())
        .collect();
    let unique: std::collections::BTreeSet<_> = inputs.iter().copied().collect();
    assert_eq!(
        inputs.len(),
        unique.len(),
        "no completed fragment is sent twice"
    );
}

/// Full preflight can use an API route with a nonexistent key because it constructs no clients.
#[test]
fn api_cost_preflight_does_not_open_credentials_or_send_requests() {
    use agent_ide::project::context::indexer::estimate_indexing;
    agent_ide::languages::install();
    let root =
        Fixture(std::env::temp_dir().join(format!("background-preflight-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(root.0.join("lib.rs"), "pub fn ready() {}\n").unwrap();
    let config = EmbeddingConfig {
        backend: Backend::Api,
        endpoint: "https://example.invalid/embeddings".into(),
        model: "fixture".into(),
        response_model: None,
        revision: "v1".into(),
        dimensions: 2,
        credential_file: Some(root.0.join("missing.key")),
        egress_allowed: false,
        timeout_ms: 3000,
        max_batch: 16,
        max_input_bytes: 4096,
        query_input_type: None,
        document_input_type: None,
    };
    assert!(
        agent_ide::project::context::embedding::EmbeddingClient::new(config.clone()).is_err(),
        "preflight permission must not permit sending"
    );
    let mut index = ContextIndex::open(&root.0).unwrap();
    let estimate = estimate_indexing(
        &mut index,
        &StructureOptions::default(),
        &DocumentOptions::default(),
        Some(&config),
        &tokio_util::sync::CancellationToken::new(),
    )
    .unwrap();
    assert!(estimate.missing_fragments > 0 && estimate.input_bytes > 0);
    assert!(!root.0.join("missing.key").exists());
    assert!(!root.0.join(".agent-ide/requests").exists());
}

/// Unknown fragments stay reserved after restart even when batching changes; independent work continues.
#[tokio::test]
async fn unknown_document_calls_are_not_resent_in_different_batches() {
    agent_ide::languages::install();
    let root =
        Fixture(std::env::temp_dir().join(format!("background-unknown-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(
        root.0.join("lib.rs"),
        "pub fn first() -> usize { 1 }\npub fn second() -> usize { 2 }\n",
    )
    .unwrap();
    let server = BackendStub::start();
    server.fail_next.store(true, Ordering::SeqCst);
    let mut config = EmbeddingConfig {
        backend: Backend::Local,
        endpoint: server.url.clone(),
        model: "fixture".into(),
        response_model: None,
        revision: "v1".into(),
        dimensions: 2,
        credential_file: None,
        egress_allowed: false,
        timeout_ms: 3000,
        max_batch: 2,
        max_input_bytes: 4096,
        query_input_type: Some("query".into()),
        document_input_type: Some("document".into()),
    };
    let make = |embedding| {
        ContextService::new(ModelConfig {
            embedding: Some(embedding),
            jev: None,
            max_new_calls: 32,
            include_threshold: 0.5,
        })
        .unwrap()
    };
    let mut index = ContextIndex::open(&root.0).unwrap();
    let scope = StructureOptions {
        max_depth: 64,
        respect_gitignore: false,
        ..Default::default()
    };
    let docs = DocumentOptions::default();
    let budget = IndexBudget {
        max_new_calls: 32,
        max_input_bytes: 1024 * 1024,
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let failed = make(config.clone())
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(failed.outcome, "failed");
    assert_eq!(failed.new_calls, 1);
    assert!(failed.input_tokens.is_none());
    drop(index);
    config.max_batch = 1;
    let mut index = ContextIndex::open(&root.0).unwrap();
    let continued = make(config.clone())
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(continued.outcome, "partial");
    assert!(continued.new_calls > 0);
    assert_eq!(continued.fragments_pending, 2);
    {
        let requests = server.requests.lock().unwrap();
        let unknown = requests[0]["input"].as_array().unwrap();
        for request in requests.iter().skip(1) {
            for input in request["input"].as_array().unwrap() {
                assert!(
                    !unknown.contains(input),
                    "unknown paid payload was resent in a new batch"
                );
            }
        }
    }
    let released =
        agent_ide::project::context::indexer::release_unresolved(&mut index, &config, true)
            .unwrap();
    assert_eq!(released["released_fragments"], 2);
    assert_eq!(released["network_requests"], 0);
    assert!(std::path::Path::new(released["archived_requests"][0].as_str().unwrap()).exists());
    let retried = make(config)
        .index_project(&mut index, &scope, &docs, &budget, &cancel)
        .await
        .unwrap();
    assert_eq!(retried.outcome, "ok");
    assert_eq!(retried.new_calls, 2);
    assert_eq!(retried.fragments_pending, 0);
}

/// Hitting a source-document limit cannot publish the leading part of a file as complete.
#[tokio::test]
async fn truncated_document_files_never_become_ready() {
    agent_ide::languages::install();
    let root =
        Fixture(std::env::temp_dir().join(format!("background-truncated-{}", std::process::id())));
    std::fs::create_dir(&root.0).unwrap();
    std::fs::write(
        root.0.join("lib.rs"),
        format!("pub fn long() {{ let _ = \"{}\"; }}\n", "x".repeat(10000)),
    )
    .unwrap();
    let service = ContextService::new(ModelConfig {
        embedding: None,
        jev: None,
        max_new_calls: 0,
        include_threshold: 0.5,
    })
    .unwrap();
    let mut index = ContextIndex::open(&root.0).unwrap();
    let scope = StructureOptions {
        max_depth: 64,
        respect_gitignore: false,
        exclude_paths: vec![".gitignore".into()],
        ..Default::default()
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let budget = IndexBudget {
        max_new_calls: 0,
        max_input_bytes: 0,
    };
    for docs in [
        DocumentOptions {
            max_documents: 1,
            ..Default::default()
        },
        DocumentOptions {
            max_document_bytes: 256,
            ..Default::default()
        },
    ] {
        let report = service
            .index_project(&mut index, &scope, &docs, &budget, &cancel)
            .await
            .unwrap();
        assert_eq!(report.outcome, "partial");
        assert!(index.published_documents("lexical").is_err());
    }
    assert_eq!(
        service
            .index_project(
                &mut index,
                &scope,
                &DocumentOptions::default(),
                &budget,
                &cancel
            )
            .await
            .unwrap()
            .outcome,
        "ok"
    );
    assert!(
        !index
            .published_documents("lexical")
            .unwrap()
            .documents
            .is_empty()
    );
    assert!(
        !root.0.join(".agent-ide/requests").exists(),
        "model-free indexing creates no request journal"
    );
    let ready = ContextIndex::open_existing(&root.0).unwrap().unwrap();
    service
        .prepare_for_delivery(
            &ready,
            "long",
            &PacketOptions::default(),
            &DocumentOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        !root.0.join(".agent-ide/requests").exists(),
        "model-free query creates no request journal"
    );
}
