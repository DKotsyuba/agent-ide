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
    for boundary in ["platform", "host", "toolchains", "scenarios", "privacy"] {
        assert_eq!(
            schema["properties"][boundary]["additionalProperties"],
            false
        );
    }
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
