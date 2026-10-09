//! Static contract checks for the reusable macOS acceptance runner and public evidence schema.

use std::collections::BTreeSet;

use serde_json::Value;

/// Ensures the public evidence schema is closed, bounded, and contains only approved field names.
#[test]
fn evidence_schema_is_closed_and_privacy_explicit() {
    let schema: Value = serde_json::from_str(include_str!(
        "../docs/contracts/macos-acceptance-evidence-v0.2.schema.json"
    ))
    .unwrap();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["route"]["enum"],
        serde_json::json!([
            "product",
            "codex",
            "claude",
            "agent_run_claude",
            "agent_run_codex"
        ])
    );
    let fields = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    assert_eq!(
        fields,
        BTreeSet::from([
            "host",
            "payload",
            "platform",
            "privacy",
            "revision",
            "route",
            "scenarios",
            "schema",
            "status",
            "toolchains",
        ])
    );
    for boundary in [
        "platform",
        "host",
        "payload",
        "toolchains",
        "scenarios",
        "privacy",
    ] {
        assert_eq!(
            schema["properties"][boundary]["additionalProperties"],
            false
        );
    }
    // The payload binding is optional: checked-in host evidence never carries it.
    assert!(
        !schema["required"]
            .as_array()
            .unwrap()
            .contains(&Value::from("payload"))
    );
    let privacy = schema["properties"]["privacy"]["properties"]
        .as_object()
        .unwrap();
    for field in [
        "source",
        "prompts",
        "credentials",
        "commands",
        "paths",
        "diagnostic_messages",
        "private_ids",
    ] {
        assert_eq!(privacy[field]["const"], false);
    }
}

/// Pins worktree isolation, route extensibility, exact gates, and the absence of private ID fields.
#[test]
fn runner_covers_all_cells_without_embedding_private_run_identifiers() {
    let runner = include_str!("../scripts/macos-acceptance.sh");
    for route in [
        "product",
        "codex",
        "claude",
        "agent-run-claude",
        "agent-run-codex",
    ] {
        assert!(runner.contains(route));
    }
    for gate in [
        "configured_product_acceptance_edit_diagnostics_telemetry_and_fallback",
        "configured_product_returns_real_typescript_family_context_and_reaps",
        "configured_product_returns_real_pyright_semantic_context_and_reaps",
        "configured_product_rust_resolves_definition_across_a_crate_boundary",
        "configured_product_isolates_typescript_across_two_divergent_worktree_actors",
        "configured_product_claude_returns_real_pyright_semantic_context_diff_and_stop",
        "configured_product_claude_returns_real_typescript_semantic_context_and_reaps",
    ] {
        assert!(runner.contains(gate));
    }
    // Go and gopls are outside the release scope: the runner neither requires nor executes them
    // and their toolchain evidence rows stay honestly `not_tested`.
    assert!(runner.contains("ACCEPTANCE_GO_VERSION=not_tested"));
    assert!(runner.contains("ACCEPTANCE_GOPLS_VERSION=not_tested"));
    assert!(!runner.contains("AGENT_IDE_GO"));
    // The fixture worktrees come from a private clone, so the managed Claude rendezvous (keyed by
    // the git common directory) never attaches to a live session's daemon.
    assert!(runner.contains(
        "git clone --quiet --shared --no-checkout \"$ACCEPTANCE_ROOT\" \"$ACCEPTANCE_REPO\""
    ));
    assert!(
        runner
            .contains("-C \"$ACCEPTANCE_REPO\" worktree add --quiet --detach \"$ACCEPTANCE_LEFT\"")
    );
    assert!(
        runner.contains(
            "-C \"$ACCEPTANCE_REPO\" worktree add --quiet --detach \"$ACCEPTANCE_RIGHT\""
        )
    );
    assert!(runner.contains("ACCEPTANCE_MAX_EVIDENCE_BYTES=16384"));
    // The optional payload binding is product-only and runs the archive's own executable.
    assert!(runner.contains("export AGENT_IDE_PRODUCT_BINARY"));
    assert!(runner.contains("'only the product route accepts a payload'"));
    for forbidden in ["run_id", "session_id", "thread_id", "transcript"] {
        assert!(!runner.contains(forbidden));
    }
}

/// Requires both agent-run routes to use schema-2 providers and the shared host-neutral prompt family.
#[test]
fn agent_run_drivers_use_route_matched_schema_two_providers() {
    let driver = include_str!("../scripts/acceptance-drivers/agent-run-claude.sh");
    let wrapper = include_str!("../scripts/acceptance-drivers/agent-run-codex.sh");
    assert!(wrapper.contains("exec \"$DRIVER_DIR/agent-run-claude.sh\""));
    for required in [
        "agent-run-codex)",
        "DEFAULT_PROVIDER=claude",
        "DEFAULT_PROVIDER=codex",
        "start --provider $PROVIDER",
        "start --provider \"$PROVIDER\"",
        "[ \"$PROVIDER\" = \"$DEFAULT_PROVIDER\" ]",
        "DEFAULT_MODEL=gpt-6-luna",
        "PROMPT_FAMILY=prompts",
        "run_scenario l1b \"$LEFT\" l1b.txt verify_l1b",
        "run_scenario r5b \"$RIGHT\" r5b.txt verify_r5b",
        "if [ \"$AGENT_IDE_ACCEPTANCE_ROUTE\" = agent-run-claude ]; then",
        "E_WORKTREE_OUTSIDE_LAUNCHER_ROOT",
        "require_agent_run_compact_replies \"$DIAG_DIR/transcript-$label.json\"",
        "utf8bytelength <= $bound",
        "for marker in left-python-bad left-typescript-bad; do",
    ] {
        assert!(
            driver.contains(required),
            "missing Codex route behavior: {required}"
        );
    }
    assert!(!driver.contains("start --runtime"));
}

/// Keeps direct Codex acceptance on the captured named workspace profile rather than the
/// legacy workspace-write override, which omits the explicit Git metadata read restriction.
#[test]
fn direct_codex_driver_uses_named_workspace_profile() {
    let driver = include_str!("../scripts/acceptance-drivers/codex.sh");
    assert!(driver.contains("default_permissions = \":workspace\""));
    assert!(!driver.contains("-s workspace-write"));
}

/// Pins the symbol-tools acceptance scenario (`l5`) across the plain-text host-cell contract, the
/// shared prompt, and every host driver that exercises the Rust fixture crate live.
#[test]
fn symbol_tools_scenario_is_wired_into_every_host_driver() {
    let runner = include_str!("../scripts/macos-acceptance.sh");
    assert!(runner.contains("symbol_tools=real_pass"));
    assert!(runner.contains("-le 10"));
    assert!(runner.contains("name = \"acceptance-fixture\""));

    let common = include_str!("../scripts/acceptance-drivers/driver-common.sh");
    assert!(common.contains("'symbol_tools=real_pass' \\"));
    assert!(common.contains("verify_symbol_tools_left_clean"));

    let prompt = include_str!("../scripts/acceptance-drivers/prompts/l5.txt");
    assert!(prompt.contains("acceptance-left-5"));
    assert!(prompt.contains("Counter/get"));
    assert!(prompt.contains("LEFT_SYMBOLS_OK"));

    let claude = include_str!("../scripts/acceptance-drivers/claude.sh");
    let codex = include_str!("../scripts/acceptance-drivers/codex.sh");
    for driver in [claude, codex] {
        assert!(driver.contains("verify_l5"));
        assert!(driver.contains("$PROMPT_FAMILY/l5.txt"));
        assert!(driver.contains("verify_symbol_tools_left_clean"));
        assert!(driver.contains("reset_left_rust_fixture"));
    }

    let agent_run = include_str!("../scripts/acceptance-drivers/agent-run-claude.sh");
    assert!(agent_run.contains("run_scenario l5 \"$LEFT\" l5.txt verify_l5"));
    assert!(agent_run.contains("LEFT_SYMBOLS_OK"));
    assert!(agent_run.contains("verify_symbol_tools_left_clean"));
}

/// Ensures every fallible toolchain check for the accepted Rust, Python, and TypeScript/JavaScript
/// toolchains propagates failure from route-guarded shell functions.
#[test]
fn toolchain_checks_return_explicitly_inside_guarded_routes() {
    let runner = include_str!("../scripts/macos-acceptance.sh");
    let verify = runner
        .split("verify_toolchains() {")
        .nth(1)
        .unwrap()
        .split("\n}")
        .next()
        .unwrap();
    let checks = verify
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("require_file ")
                || line.starts_with("require_directory ")
                || line.starts_with("[ \"$(")
        })
        .collect::<Vec<_>>();
    assert_eq!(checks.len(), 14, "{checks:?}");
    assert!(
        checks.iter().all(|line| line.ends_with("|| return 1")),
        "{checks:?}"
    );
}

/// Owns one scratch directory of the L2 oracle test and removes it on every exit path.
struct OracleScratch(std::path::PathBuf);

impl Drop for OracleScratch {
    /// Removes the directory even when an assertion failed.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Runs the driver's real `require_stale_edit_result` over stream-json `events` and reports
/// whether it accepted the transcript.
fn l2_stale_probe_accepts(scratch: &OracleScratch, name: &str, events: &[Value]) -> bool {
    let transcript = scratch.0.join(format!("{name}.jsonl"));
    let lines = events.iter().map(Value::to_string).collect::<Vec<_>>();
    std::fs::write(&transcript, lines.join("\n")).unwrap();
    let common = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/scripts/acceptance-drivers/driver-common.sh"
    );
    std::process::Command::new("sh")
        .arg("-c")
        .arg(". \"$1\"; require_stale_edit_result \"$2\"")
        .args(["sh", common])
        .arg(&transcript)
        .env("AGENT_IDE_ACCEPTANCE_DIAG_LOG", scratch.0.join("diag.log"))
        .status()
        .expect("sh runs the driver function")
        .success()
}

/// One assistant message with `content` blocks, as in `claude -p --output-format stream-json`.
fn assistant(content: Value) -> Value {
    serde_json::json!({"type":"assistant","message":{"content":content}})
}

/// One user message carrying the `result` text of tool call `id`.
fn tool_result(id: &str, text: &str) -> Value {
    serde_json::json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":id,"content":[{"type":"text","text":text}]}]}})
}

/// The L2 oracle keeps the literal `stale_source` but takes it only from the result of a real
/// `ide.edit` call: a skipped step 7 fails even when the model narrates the literal, and another
/// tool's result carrying it does not stand in for the skipped call.
///
/// Before the fix `verify_l2` scanned every event for the literal, so a transcript with no
/// `ide.edit` at all passed on narration alone.
#[test]
fn l2_stale_oracle_requires_the_stale_edit_result_not_narration() {
    let scratch = OracleScratch(std::env::temp_dir().join(format!(
        "agent-ide-l2-oracle-{}",
        std::process::id()
    )));
    std::fs::create_dir_all(&scratch.0).unwrap();
    let edit = "mcp__agent-ide__ide_edit";
    let stale = "edit: stale_source; path acceptance-fixture/fixture.py. No write occurred";
    let narration = assistant(serde_json::json!([
        {"type":"text","text":"Step 7 skipped; a stale_source refusal would have followed. LEFT_FALLBACK_OK"}]));
    let call = |id: &str, name: &str| {
        assistant(serde_json::json!([{"type":"tool_use","id":id,"name":name,"input":{}}]))
    };

    assert!(l2_stale_probe_accepts(
        &scratch,
        "real",
        &[call("e1", edit), tool_result("e1", stale), narration.clone()]
    ));
    assert!(
        !l2_stale_probe_accepts(&scratch, "skipped", &[narration.clone()]),
        "narration alone must not pass"
    );
    assert!(
        !l2_stale_probe_accepts(
            &scratch,
            "other-tool",
            &[
                call("c1", "mcp__agent-ide__ide_context"),
                tool_result("c1", stale),
                narration.clone()
            ]
        ),
        "another tool's result carrying the literal must not pass"
    );
    assert!(
        !l2_stale_probe_accepts(
            &scratch,
            "wrong-result",
            &[
                call("e1", edit),
                tool_result("e1", "edit: applied"),
                call("c1", "mcp__agent-ide__ide_context"),
                tool_result("c1", stale),
                narration
            ]
        ),
        "an ide.edit that was not refused as stale must not pass"
    );

    let claude = include_str!("../scripts/acceptance-drivers/claude.sh");
    assert!(claude.contains("require_stale_edit_result \"$t\""));
    assert!(!claude.contains("require_transcript_text \"$t\" \"stale_source\""));
}
