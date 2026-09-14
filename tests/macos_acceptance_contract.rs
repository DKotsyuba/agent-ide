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
    for route in ["product", "codex", "claude", "agent-run-claude"] {
        assert!(runner.contains(route));
    }
    for gate in [
        "configured_product_acceptance_edit_diagnostics_telemetry_and_fallback",
        "configured_product_returns_real_typescript_family_context_and_reaps",
        "configured_product_returns_real_pyright_semantic_context_and_reaps",
        "configured_product_returns_real_go_and_rust_semantic_context",
        "configured_product_isolates_go_across_two_divergent_worktree_actors",
        "configured_product_claude_helper_returns_real_pyright_semantic_context_diff_and_stop",
        "configured_product_claude_helper_returns_real_typescript_semantic_context_and_reaps",
    ] {
        assert!(runner.contains(gate));
    }
    assert!(runner.contains("worktree add --quiet --detach \"$ACCEPTANCE_LEFT\""));
    assert!(runner.contains("worktree add --quiet --detach \"$ACCEPTANCE_RIGHT\""));
    assert!(runner.contains("ACCEPTANCE_MAX_EVIDENCE_BYTES=16384"));
    for forbidden in ["run_id", "session_id", "thread_id", "transcript"] {
        assert!(!runner.contains(forbidden));
    }
}

/// Ensures every fallible toolchain check propagates failure from route-guarded shell functions.
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
