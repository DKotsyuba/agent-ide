//! Project-private context storage, builder reuse and honest source freshness.
use agent_ide::project::{
    context::{ContextIndex, PacketOptions},
    structure::{Detail, StructureOptions, build_structure},
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
/// Owns one disposable operator project without shared environment changes.
struct Fixture(PathBuf);
impl Fixture {
    /// Creates unique source roots and installs already linked language adapters.
    fn new() -> Self {
        agent_ide::languages::install();
        let root = std::env::temp_dir().join(format!(
            "context-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Fixture {
    /// Removes only the fixture created by this test.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Scans the complete tiny fixture without requiring Git installation or a repository.
fn options() -> StructureOptions {
    StructureOptions {
        max_depth: 64,
        respect_gitignore: false,
        include_symbols: true,
        ..Default::default()
    }
}

/// Initialization preserves ignore bytes, avoids duplicates and excludes its own data unconditionally.
#[test]
fn index_is_private_ignored_and_never_indexes_itself() {
    let f = Fixture::new();
    fs::write(f.0.join(".gitignore"), "existing").unwrap();
    fs::write(f.0.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    {
        let mut index = ContextIndex::open(&f.0).unwrap();
        index.refresh(&options()).unwrap();
        assert_eq!(index.search("alpha", 10).unwrap().len(), 1);
    }
    let _again = ContextIndex::open(&f.0).unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join(".gitignore")).unwrap(),
        "existing\n/.agent-ide/\n"
    );
    let tree = build_structure(
        &f.0,
        &StructureOptions {
            exclude_generated: false,
            ..options()
        },
    )
    .unwrap();
    assert!(
        !tree
            .tree
            .children
            .iter()
            .any(|node| node.path == std::path::Path::new(".agent-ide"))
    );
    let file = tree
        .tree
        .children
        .iter()
        .find(|node| node.path == std::path::Path::new("lib.rs"))
        .unwrap();
    assert!(matches!(&file.symbols,Some(Detail::Available(symbols)) if symbols[0].name=="alpha"));
}

/// Edits refuse stale delivery; a refreshed generation removes deleted files and returns exact bytes.
#[test]
fn changed_and_deleted_sources_do_not_leak_stale_packets() {
    let f = Fixture::new();
    fs::write(f.0.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    let mut index = ContextIndex::open(&f.0).unwrap();
    index.refresh(&options()).unwrap();
    assert_eq!(index.refresh(&options()).unwrap().reused_files, 1);
    let packet = index.prepare("alpha", &PacketOptions::default()).unwrap();
    assert_eq!(packet.windows[0].text, "pub fn alpha() {}\n");
    fs::write(f.0.join("lib.rs"), "pub fn beta() {}\n").unwrap();
    let stale = index.prepare("alpha", &PacketOptions::default()).unwrap();
    assert!(stale.windows.is_empty());
    assert!(
        stale
            .gaps
            .iter()
            .any(|gap| gap.contains("changed since indexing"))
    );
    index.refresh(&options()).unwrap();
    assert!(index.search("alpha", 10).unwrap().is_empty());
    assert_eq!(index.search("beta", 10).unwrap().len(), 1);
    fs::remove_file(f.0.join("lib.rs")).unwrap();
    index.refresh(&options()).unwrap();
    assert!(index.search("beta", 10).unwrap().is_empty());
}

/// A symlinked data directory is not followed and a collapsed walk is never called complete.
#[test]
fn symlink_and_partial_discovery_remain_explicit() {
    let f = Fixture::new();
    let other = Fixture::new();
    std::os::unix::fs::symlink(&other.0, f.0.join(".agent-ide")).unwrap();
    assert!(ContextIndex::open(&f.0).is_err());
    assert!(!other.0.join("index.sqlite").exists());
    fs::remove_file(f.0.join(".agent-ide")).unwrap();
    let mut index = ContextIndex::open(&f.0).unwrap();
    let partial = index
        .refresh(&StructureOptions {
            max_depth: 0,
            ..options()
        })
        .unwrap();
    assert!(!partial.discovery_complete);
    assert!(
        index
            .prepare(
                "task",
                &PacketOptions {
                    context_tokens: 1,
                    reserved_tokens: 2,
                    ..Default::default()
                }
            )
            .is_err()
    );
}

/// Vector namespaces and generations prevent old or wrong-model hits from being returned.
#[test]
fn semantic_cache_is_scoped_and_generation_checked() {
    use agent_ide::project::context::DocumentOptions;
    let f = Fixture::new();
    fs::write(f.0.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
    let mut index = ContextIndex::open(&f.0).unwrap();
    index.refresh(&options()).unwrap();
    let batch = index.documents(&DocumentOptions::default()).unwrap();
    assert!(!batch.documents.is_empty());
    let document = &batch.documents[0];
    index
        .store_vector("model-a", &document.id, &[1.0, 0.0])
        .unwrap();
    assert!(
        index
            .semantic_search(&batch, "model-b", &[1.0, 0.0], 10)
            .unwrap()
            .is_empty()
    );
    let hits = index
        .semantic_search(&batch, "model-a", &[1.0, 0.0], 10)
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!((hits[0].similarity - 1.0).abs() < 0.0001);
    assert!(index.vector("model-a", &document.id, 3).is_err());
    assert!(index.store_vector("model-a", "bad", &[0.0, 0.0]).is_err());
    index.refresh(&options()).unwrap();
    assert!(
        index
            .semantic_search(&batch, "model-a", &[1.0, 0.0], 10)
            .is_err()
    );
}

/// Exercises real HTTP serialization, indexing, semantic retrieval and durable replay without inference.
#[cfg(feature = "jev-pilot")]
#[tokio::test]
async fn preparation_uses_embeddings_and_replays_without_network() {
    use agent_ide::project::context::{
        DocumentOptions,
        embedding::{Backend, EmbeddingConfig},
        service::{ContextService, ModelConfig},
    };
    use std::io::{Read, Write};
    let fixture = Fixture::new();
    let source = "//! Request body contract.\nuse std::borrow::Cow;\npub fn retain_body() -> bool { true }\n";
    fs::write(fixture.0.join("cache.rs"), source).unwrap();
    let large = format!(
        "//! Container context.\npub struct Store;\nimpl Store {{\n    /// The complete first method must survive a split embedding document.\n    pub fn first(&self) -> usize {{\n        let _padding = \"{}\";\n        7\n    }}\n    pub fn second(&self) -> usize {{ 9 }}\n}}\n",
        "x".repeat(9500)
    );
    fs::write(fixture.0.join("container.rs"), &large).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/embeddings", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let mut roles = Vec::new();
        while roles.len() < 2 {
            let (mut stream, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "missing embedding request"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let mut bytes = Vec::new();
            let (offset, length) = loop {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]);
                    assert!(!headers.to_lowercase().contains("authorization:"));
                    let length: usize = headers
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    break (end + 4, length);
                }
            };
            while bytes.len() < offset + length {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).unwrap();
                assert!(read > 0);
                bytes.extend_from_slice(&chunk[..read]);
            }
            let request: serde_json::Value =
                serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
            roles.push(request["input_type"].as_str().unwrap().to_owned());
            let data: Vec<_> = request["input"]
                .as_array()
                .unwrap()
                .iter()
                .enumerate()
                .rev()
                .map(|(i, _)| serde_json::json!({"index":i,"embedding":[3.0,4.0]}))
                .collect();
            let body =
                serde_json::json!({"model":"fixture","data":data,"usage":{"prompt_tokens":7}})
                    .to_string();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
        roles
    });
    let service = ContextService::new(ModelConfig {
        embedding: Some(EmbeddingConfig {
            backend: Backend::Local,
            endpoint,
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
        }),
        jev: None,
        max_new_calls: 2,
        include_threshold: 0.5,
    })
    .unwrap();
    let mut index = ContextIndex::open(&fixture.0).unwrap();
    index.refresh(&options()).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let indexed = service
        .index_project(
            &mut index,
            &options(),
            &DocumentOptions::default(),
            &agent_ide::project::context::indexer::IndexBudget {
                max_new_calls: 2,
                max_input_bytes: 1024 * 1024,
            },
            &cancel,
        )
        .await
        .unwrap();
    assert_eq!(indexed.outcome, "ok");
    assert_eq!(indexed.new_calls, 1);
    let first = service
        .prepare(
            &index,
            "сохранить тело запроса",
            &PacketOptions::default(),
            &DocumentOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(first.embedding_used);
    assert!(!first.jev_used);
    let small: Vec<_> = first
        .packet
        .windows
        .iter()
        .filter(|window| window.path == std::path::Path::new("cache.rs"))
        .collect();
    assert_eq!(small.len(), 1);
    assert_eq!(
        small[0].text, source,
        "semantic selection must retain small-file imports and header"
    );
    assert!(small[0].complete_file && small[0].unit_complete);
    let methods: Vec<_> = first
        .packet
        .windows
        .iter()
        .filter(|window| window.path == std::path::Path::new("container.rs"))
        .collect();
    assert!(methods.iter().any(|window| {
        window
            .symbol
            .as_deref()
            .is_some_and(|symbol| symbol.ends_with("/first"))
    }));
    for window in &methods {
        assert!(window.required);
        if window.symbol.is_some() {
            assert!(window.unit_complete);
        }
        if !window.unit_complete {
            assert!(
                first
                    .packet
                    .gaps
                    .iter()
                    .any(|gap| gap.contains("are not a complete declaration")),
                "unlocalized imports/header must remain an explicit gap"
            );
        }
    }
    for window in methods {
        assert_eq!(window.text, large[window.start_byte..window.end_byte]);
    }
    assert!(first.packet.file_facts.iter().any(|facts| {
        facts
            .declarations
            .iter()
            .any(|item| item.signature.contains("struct Store"))
    }));
    let wire = serde_json::to_string_pretty(&first).unwrap();
    let estimate =
        agent_ide::project::token_estimate::estimate_tokens(&wire, 4.0, 12000, 2000).unwrap();
    assert_eq!(first.packet.estimated_tokens, estimate.estimated_tokens);
    assert!(estimate.estimated_fits);
    assert_eq!(first.calls.len(), 1);
    assert!(first.calls.iter().all(|call| !call.replayed));
    assert_eq!(server.join().unwrap(), ["document", "query"]);
    // The listener is gone. A successful second packet proves no physical HTTP replay.
    let second = service
        .prepare(
            &index,
            "сохранить тело запроса",
            &PacketOptions::default(),
            &DocumentOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    assert!(second.embedding_used);
    assert_eq!(second.calls.len(), 1);
    assert!(second.calls[0].replayed);
    assert_eq!(second.packet.windows[0].text, first.packet.windows[0].text);
    assert!(
        !fixture
            .0
            .join(".agent-ide/requests")
            .read_dir()
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "next"))
    );
}

/// Packet facts expand a clipped header locally without changing the cached indexing input.
#[test]
fn packet_restores_the_complete_module_header() {
    let fixture = Fixture::new();
    let header = (0..20)
        .map(|line| format!("//! Contract line {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        fixture.0.join("header.rs"),
        format!("{header}\nuse std::collections::BTreeMap;\npub fn ready() {{}}\n"),
    )
    .unwrap();
    let mut index = ContextIndex::open(&fixture.0).unwrap();
    index.refresh(&options()).unwrap();
    let packet = index.prepare("header", &PacketOptions::default()).unwrap();
    let facts = &packet.file_facts[0];
    let header = facts.header.as_ref().unwrap();
    assert!(header.text.contains("Contract line 19"));
    assert!(!header.text.contains("//!"));
    assert!(!header.truncated);
    assert!(!facts.truncated);
    assert!(packet.windows[0].unit_complete);
}

/// Cancellation during discovery never publishes a partial replacement for the prior generation.
#[test]
fn cancelled_refresh_preserves_the_previous_generation() {
    use std::sync::atomic::AtomicUsize;
    let fixture = Fixture::new();
    fs::write(fixture.0.join("old.rs"), "pub fn original() {}\n").unwrap();
    let mut index = ContextIndex::open(&fixture.0).unwrap();
    let prior = index.refresh(&options()).unwrap();
    for n in 0..50 {
        fs::write(fixture.0.join(format!("new{n}.rs")), "pub fn later() {}\n").unwrap();
    }
    let calls = AtomicUsize::new(0);
    let result =
        index.refresh_cancellable(&options(), &|| calls.fetch_add(1, Ordering::Relaxed) > 10);
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Interrupted);
    assert_eq!(
        index.snapshot().unwrap().unwrap().generation,
        prior.generation
    );
    assert!(index.search("later", 10).unwrap().is_empty());
}

/// Plain UTF-8 project documents join the index without a parser; secrets and binaries do not.
#[test]
fn plaintext_project_files_are_indexed_but_secret_containers_are_not() {
    use agent_ide::project::context::DocumentOptions;
    let fixture = Fixture::new();
    for (path, text) in [
        ("README.md", "A useful project description\n"),
        ("Cargo.toml", "[package]\nname = \"demo\"\n"),
        ("setup.sh", "#!/bin/sh\necho ready\n"),
        (".env.local", "SECRET=do-not-embed\n"),
        ("credentials.json", "{\"token\":\"do-not-embed\"}"),
    ] {
        fs::write(fixture.0.join(path), text).unwrap();
    }
    fs::write(fixture.0.join("opaque.bin"), [0, 255, 1]).unwrap();
    use std::os::unix::fs::PermissionsExt;
    for name in ["runner", "id_rsa"] {
        fs::write(fixture.0.join(name), "#!/bin/sh\necho script\n").unwrap();
        fs::set_permissions(fixture.0.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(fixture.0.join("reply.jinja"), "Hello {{ value }}\n").unwrap();
    let mut index = ContextIndex::open(&fixture.0).unwrap();
    index.refresh(&options()).unwrap();
    let docs = index.documents(&DocumentOptions::default()).unwrap();
    for path in [
        "README.md",
        "Cargo.toml",
        "setup.sh",
        "runner",
        "reply.jinja",
    ] {
        assert!(
            docs.documents
                .iter()
                .any(|d| d.path == std::path::Path::new(path)),
            "{path}"
        );
    }
    assert!(
        docs.documents
            .iter()
            .all(|d| !d.text.contains("do-not-embed")
                && d.path != std::path::Path::new("opaque.bin")
                && d.path != std::path::Path::new("id_rsa"))
    );
}
