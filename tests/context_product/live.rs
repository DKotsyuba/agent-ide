//! Explicit operator-only real embedding lifecycle proof; never runs in ordinary gates.
use super::*;
use agent_ide::project::context::{ContextIndex, embedding::EmbeddingConfig};

/// Reads only header-free request records from the fixture, never its credential configuration.
fn journals(root: &Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(root.join(".agent-ide/requests")) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|b| serde_json::from_slice(&b).ok())
        .collect()
}

/// Keeps request outcomes even when an operator assertion fails before fixture teardown.
struct EvidenceOnExit {
    root: PathBuf,
    evidence: PathBuf,
}
impl Drop for EvidenceOnExit {
    /// Runs after daemon cleanup and before the fixture removes its private source tree.
    fn drop(&mut self) {
        if let Ok(bytes) = serde_json::to_vec_pretty(&journals(&self.root)) {
            let _ = std::fs::write(self.evidence.join("requests-on-exit.json"), bytes);
        }
    }
}

/// Distinguishes source payloads from the deliberately path-free task query.
fn document_inputs(root: &Path) -> Vec<String> {
    let mut inputs: Vec<String> = journals(root)
        .iter()
        .flat_map(|v| v["request"]["input"].as_array().into_iter().flatten())
        .filter_map(Value::as_str)
        .filter(|s| s.starts_with("path: "))
        .map(str::to_owned)
        .collect();
    inputs.sort();
    inputs
}

/// Waits for a published background generation without issuing a query or inference itself.
async fn ready(root: &Path, space: &str, after: u64, needle: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(Some(index)) = ContextIndex::open_existing(root)
            && let Ok(Some(report)) = index.index_report(space)
            && report["generation"].as_u64().unwrap_or_default() > after
            && report["outcome"] == "ok"
            && report["fragments_pending"] == 0
            && let Ok(batch) = index.published_documents(space)
            && batch.documents.iter().any(|d| d.text.contains(needle))
        {
            return report;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background did not publish a complete generation"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Proves start, ready-only prepare, IDE/native incremental edits, restart reuse and stop against Voyage.
#[tokio::test]
#[ignore = "operator-only real API; explicit private config and evidence directory required"]
async fn voyage_background_lifecycle() {
    let path = std::env::var_os("AGENT_IDE_CONTEXT_EXAMPLE_CONFIG").expect("explicit config");
    let evidence = PathBuf::from(
        std::env::var_os("AGENT_IDE_CONTEXT_EXAMPLE_EVIDENCE").expect("explicit evidence"),
    );
    assert!(!evidence.exists(), "evidence must be fresh");
    std::fs::create_dir_all(&evidence).unwrap();
    let mut models: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    models["jev"] = Value::Null;
    models["max_new_calls"] = json!(4);
    let embedding: EmbeddingConfig = serde_json::from_value(models["embedding"].clone()).unwrap();
    assert_eq!(embedding.model, "voyageai/voyage-code-4");
    let space = embedding.space_id().unwrap();
    let fixture = ProductFixture::new(json!([]));
    let _evidence_guard = EvidenceOnExit {
        root: fixture.root.clone(),
        evidence: evidence.clone(),
    };
    let other = "/// Stable helper.\npub fn other() -> u8 { 1 }\n";
    std::fs::write(fixture.root.join("src/other.rs"), other).unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["task_context"] = json!({"enabled":true,"models":models,"indexing":{
        "enabled":true,"initial_allowed":true,"max_new_calls":8,"max_input_bytes":65536,
        "debounce_ms":50,"reconcile_ms":200}});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "live-background").await;
    let started = actor.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let first_report = ready(&fixture.root, &space, 0, "{ 7 }").await;
    let first = document_inputs(&fixture.root);
    assert!(first.iter().any(|s| s.contains("fn other")));
    let raw = actor.call_raw(&fixture, "ide.prepare", json!({"task":"Change what value returns and inspect its implementation.","token_budget":4000})).await;
    let text = raw["result"]["structuredContent"]["text"].as_str().unwrap();
    assert!(text.contains("usage: embeddings used"), "{text}");
    assert!(text.contains("pub fn value"), "{text}");
    assert_eq!(
        document_inputs(&fixture.root),
        first,
        "prepare embedded documents"
    );
    std::fs::write(
        evidence.join("prepare-before.json"),
        serde_json::to_vec_pretty(&raw).unwrap(),
    )
    .unwrap();
    let edit = actor
        .call(
            &fixture,
            "ide.edit",
            json!({"operation_id":"live-index-edit","files":[{"path":"src/lib.rs","changes":[
        {"old":"pub fn value() -> i32 { 7 }","new":"pub fn value() -> i32 { 8 }"}]}]}),
        )
        .await;
    assert_eq!(edit["files"][0]["state"], "replaced", "{edit}");
    let edit_report = ready(
        &fixture.root,
        &space,
        first_report["generation"].as_u64().unwrap(),
        "{ 8 }",
    )
    .await;
    let after_edit = document_inputs(&fixture.root);
    let delta: Vec<_> = after_edit.iter().filter(|s| !first.contains(s)).collect();
    assert_eq!(delta.len(), 1, "only the changed fragment: {delta:?}");
    assert!(delta[0].contains("{ 8 }") && !delta[0].contains("fn other"));
    let raw2 = actor
        .call_raw(
            &fixture,
            "ide.prepare",
            json!({"task":"Inspect the updated value implementation.","token_budget":4000}),
        )
        .await;
    assert!(
        raw2["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap()
            .contains("{ 8 }")
    );
    assert_eq!(document_inputs(&fixture.root), after_edit);
    std::fs::write(
        evidence.join("prepare-after.json"),
        serde_json::to_vec_pretty(&raw2).unwrap(),
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("src/other.rs"),
        other.replace("{ 1 }", "{ 2 }"),
    )
    .unwrap();
    let native_report = ready(
        &fixture.root,
        &space,
        edit_report["generation"].as_u64().unwrap(),
        "pub fn other() -> u8 { 2 }",
    )
    .await;
    let after_native = document_inputs(&fixture.root);
    let delta2: Vec<_> = after_native
        .iter()
        .filter(|s| !after_edit.contains(s))
        .collect();
    assert_eq!(delta2.len(), 1, "native edit changed exactly one fragment");
    assert!(delta2[0].contains("fn other") && delta2[0].contains("{ 2 }"));
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor.settle(&fixture, stopped).await["kind"], "stop");
    drop(actor);
    drop(daemon);
    let daemon2 = fixture.daemon_with_home(Some(&home)).await;
    let mut actor2 = ProductActor::new(&fixture, "live-background-restart").await;
    let started2 = actor2.call(&fixture, "ide.start", json!({})).await;
    assert_eq!(
        actor2.settle(&fixture, started2).await["kind"],
        "activation"
    );
    let restart_report = ready(
        &fixture.root,
        &space,
        native_report["generation"].as_u64().unwrap(),
        "pub fn other() -> u8 { 2 }",
    )
    .await;
    assert_eq!(
        document_inputs(&fixture.root),
        after_native,
        "restart re-embedded unchanged sources"
    );
    let stopped2 = actor2.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(actor2.settle(&fixture, stopped2).await["kind"], "stop");
    let before_stop_change = journals(&fixture.root).len();
    std::fs::write(
        fixture.root.join("src/other.rs"),
        other.replace("{ 1 }", "{ 3 }"),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(journals(&fixture.root).len(), before_stop_change);
    let entries = journals(&fixture.root);
    assert!(entries.iter().all(|v| v["state"] == "complete"));
    std::fs::write(
        evidence.join("requests.json"),
        serde_json::to_vec_pretty(&entries).unwrap(),
    )
    .unwrap();
    std::fs::write(evidence.join("summary.json"),serde_json::to_vec_pretty(&json!({"first":first_report,"edit":edit_report,
        "native":native_report,"restart":restart_report,"initial_source_inputs":first.len(),"ide_edit_new_fragments":delta.len(),
        "native_edit_new_fragments":delta2.len(),"restart_new_fragments":0,"stop_new_requests":0,"prepare_document_calls":0})).unwrap()).unwrap();
    drop(actor2);
    drop(daemon2);
}
