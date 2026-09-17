//! Static release contracts for versions, quality gates, evidence, packaging, and host wiring.

use serde_json::Value;

/// Keeps the Rust package and both plugin manifests on the exact v0.2 release version.
#[test]
fn release_versions_are_synchronized() {
    assert!(include_str!("../Cargo.toml").contains("version = \"0.2.0\""));
    assert!(include_str!("../Cargo.lock").contains("name = \"agent-ide\"\nversion = \"0.2.0\""));
    for manifest in [
        include_str!("../.codex-plugin/plugin.json"),
        include_str!("../.claude-plugin/plugin.json"),
    ] {
        let manifest: Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(manifest["version"], "0.2.0");
    }
}

/// Requires every relevant CI, product-acceptance, evidence, package, and artifact-smoke gate
/// to precede the only GitHub Release publication command without a continue-on-error escape.
#[test]
fn release_workflow_requires_complete_gates_before_publication() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let publish = workflow.find("gh release create").unwrap();
    for gate in [
        "fetch-depth: 0",
        "go-version: \"1.25.0\"",
        "node-version: \"24.4.0\"",
        "rustup component add rustfmt clippy rust-analyzer rust-src",
        "gopls@v0.23.0",
        "pyright@1.1.413",
        "typescript-language-server@6.0.0",
        "typescript@5.9.3",
        "cargo fmt --check",
        "cargo test --locked --workspace -- --test-threads=1",
        "cargo clippy --locked --workspace --all-targets -- -D warnings",
        "cargo doc --locked --workspace --no-deps",
        "scripts/macos-acceptance.sh --route product",
        "scripts/validate-release-evidence.sh \"$GITHUB_SHA\"",
        "cargo build --locked --release --bin agent-ide",
        "scripts/package-release.sh",
        "scripts/release-smoke.sh \"$ASSET\"",
    ] {
        assert!(workflow[..publish].contains(gate), "missing gate: {gate}");
    }
    assert!(!workflow.contains("continue-on-error"));
}

/// Pins the publication evidence gate to all four complete macOS arm64 candidate rows while
/// rejecting partial scenario values, untested toolchains, mixed revisions, and non-ancestors.
#[test]
fn release_evidence_gate_requires_the_complete_candidate_matrix() {
    let gate = include_str!("../scripts/validate-release-evidence.sh");
    for requirement in [
        "macos-v0.2-product.json|product|product_pass",
        "macos-v0.2-direct-codex.json|codex|real_pass",
        "macos-v0.2-direct-claude.json|claude|real_pass",
        "macos-v0.2-agent-run-claude.json|agent_run_claude|real_pass",
        ".platform.os == \"macos\"",
        ".platform.architecture == \"arm64\"",
        "([.scenarios[]] | all(. == $status))",
        "([.privacy[]] | all(. == false))",
        "merge-base --is-ancestor",
    ] {
        assert!(
            gate.contains(requirement),
            "missing evidence rule: {requirement}"
        );
    }
    for version in [
        "1.25.0", "0.23.0", "1.98.1", "24.4.0", "1.1.413", "6.0.0", "5.9.3",
    ] {
        assert!(
            gate.contains(version),
            "missing accepted version: {version}"
        );
    }
}

/// Ensures the release archive carries the complete two-host plugin surface and its smoke test
/// executes the extracted binary directly without Cargo's integration-test executable shortcut.
#[test]
fn release_archive_and_smoke_use_the_packaged_executable() {
    let package = include_str!("../scripts/package-release.sh");
    let smoke = include_str!("../scripts/release-smoke.sh");
    for path in [
        ".agents/plugins/marketplace.json",
        ".claude-plugin/marketplace.json",
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "docs/release.md",
        "hooks/claude-hook.sh",
        "hooks/hooks.json",
        "skills/agent-ide/SKILL.md",
        "skills/agent-ide/agents/openai.yaml",
    ] {
        assert!(package.contains(path), "missing package input: {path}");
        assert!(smoke.contains(path), "missing smoke assertion: {path}");
    }
    assert!(smoke.contains("\"$RELEASE_BIN\" evidence executable"));
    assert!(!smoke.contains("CARGO_BIN_EXE"));
}

/// Verifies Claude hooks cannot select an ambient executable, the Claude plugin manifest relies
/// on Claude Code auto-loading the standard `hooks/hooks.json` instead of a duplicate explicit
/// reference, and the release guide binds their absolute executable setting to the same stable
/// path used by the MCP command after updates.
#[test]
fn claude_hook_and_mcp_share_the_installed_binary() {
    let hook = include_str!("../hooks/claude-hook.sh");
    let guide = include_str!("../docs/release.md");
    let claude_manifest: Value =
        serde_json::from_str(include_str!("../.claude-plugin/plugin.json")).unwrap();
    assert!(hook.contains("[ -n \"${AGENT_IDE_BIN:-}\" ] || exit 0"));
    assert!(hook.contains("exec \"$AGENT_IDE_BIN\" claude-hook"));
    assert!(!hook.contains("command -v"));
    assert!(claude_manifest.get("hooks").is_none());
    assert!(guide.contains("use that identical path"));
    assert!(guide.contains("as the MCP `command`"));
    assert!(guide.contains("claude plugin update agent-ide@agent-ide"));
}

/// Checks the Claude marketplace selects the repository-root plugin and carries the release version.
#[test]
fn claude_marketplace_installs_the_root_plugin() {
    let claude: Value =
        serde_json::from_str(include_str!("../.claude-plugin/marketplace.json")).unwrap();
    assert_eq!(claude["plugins"][0]["name"], "agent-ide");
    assert_eq!(claude["plugins"][0]["source"], "./");
    assert_eq!(claude["plugins"][0]["version"], "0.2.0");
}
