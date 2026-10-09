//! Static release contracts for versions, quality gates, evidence, packaging, and host wiring.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::Value;

/// Keeps the Rust package, its lock entry and the three plugin manifests on one release version
/// (the version `cargo xtask release prepare` edits together).
#[test]
fn release_versions_are_synchronized() {
    let version = env!("CARGO_PKG_VERSION");
    assert!(include_str!("../Cargo.toml").contains(&format!("version = \"{version}\"")));
    assert!(
        include_str!("../Cargo.lock")
            .contains(&format!("name = \"agent-ide\"\nversion = \"{version}\""))
    );
    for manifest in [
        include_str!("../.codex-plugin/plugin.json"),
        include_str!("../.claude-plugin/plugin.json"),
    ] {
        let manifest: Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(manifest["version"], version);
    }
}

/// Every version spelling prints the exact version line and exits 0 before any other mode runs.
#[test]
fn version_flag_prints_the_package_version_and_exits_zero() {
    let expected = format!("agent-ide {}\n", env!("CARGO_PKG_VERSION"));
    for spelling in ["-v", "-V", "--version", "version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .arg(spelling)
            .output()
            .unwrap();
        assert!(output.status.success(), "{spelling} did not exit 0");
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            expected,
            "{spelling} printed an unexpected line"
        );
        assert!(output.stderr.is_empty(), "{spelling} wrote to stderr");
    }
}

/// Pins the release workflow to the family shape: a read-only workflow token; a build job that
/// runs the complete gate (whose last step is the one release build), packages and verifies that
/// exact payload, runs the product acceptance route against the packaged executable, validates the
/// accepted host evidence and writes the release manifest before uploading the payload; and a
/// tag-only publish job with the release environment that verifies hashes before any chmod,
/// attests, and hands publication to `xtask release publish`.
#[test]
fn release_workflow_builds_once_then_publishes_a_verified_draft() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let publish_job = workflow.find("\n  publish:\n").unwrap();
    let (build, publish) = workflow.split_at(publish_job);
    assert!(workflow.contains("permissions:\n  contents: read\n\nconcurrency:"));
    assert!(workflow.contains("workflow_dispatch:"));
    assert!(workflow.contains("dry_run:"));
    let mut cursor = 0;
    for step in [
        "fetch-depth: 0",
        "node-version: \"24.4.0\"",
        "test \"$GITHUB_REF_NAME\" = \"v$version\"",
        "test \"$DRY_RUN\" = true",
        "jq -r .plugins[0].version .claude-plugin/marketplace.json",
        "rustup component add rustfmt clippy rust-analyzer rust-src",
        "pyright@1.1.413",
        "typescript-language-server@6.0.0",
        "typescript@5.9.3",
        "cargo fetch --locked",
        "cargo xtask check",
        "cargo xtask package target/release/agent-ide \"$RELEASE_TAG\" \"$PAYLOAD\"",
        "cargo xtask package verify \"$asset\"",
        "cp install.sh \"$PAYLOAD/install.sh\"",
        "scripts/macos-acceptance.sh --route product --payload \"$ASSET\" --evidence \"$PAYLOAD/acceptance.json\"",
        "scripts/validate-release-evidence.sh \"$GITHUB_SHA\"",
        "cargo xtask release manifest \"$PAYLOAD\"",
        "cargo xtask package verify \"$ASSET\"",
        "actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7.0.1",
    ] {
        let at = build[cursor..]
            .find(step)
            .unwrap_or_else(|| panic!("build job lacks, or misorders, {step}"));
        cursor += at + step.len();
    }
    assert_eq!(
        build.matches("cargo build").count(),
        0,
        "the release binary is built once, by the gate"
    );
    let mut cursor = 0;
    for step in [
        "needs: build",
        "if: github.event_name == 'push'",
        "contents: write",
        "id-token: write",
        "attestations: write",
        "environment: release",
        "actions/download-artifact@3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c # v8.0.1",
        "cargo build --locked -p xtask",
        "target/debug/xtask package verify",
        "chmod 755",
        "if: ${{ !github.event.repository.private }}",
        "actions/attest-build-provenance@4d101475d8b20a2381f78447822ac1eab6504dd8 # v4.2.2",
        "subject-checksums: ${{ runner.temp }}/payload/SHA256SUMS",
        "GH_TOKEN: ${{ github.token }}",
        "target/debug/xtask release publish \"$RUNNER_TEMP/payload\"",
    ] {
        let at = publish[cursor..]
            .find(step)
            .unwrap_or_else(|| panic!("publish job lacks, or misorders, {step}"));
        cursor += at + step.len();
    }
    assert!(!build.contains("contents: write") && !build.contains("id-token"));
    assert!(!workflow.contains("gh release create"));
    assert!(!workflow.contains("continue-on-error"));
    assert!(!workflow.contains("setup-go"));
    assert!(!workflow.contains("go-version"));
    assert!(!workflow.contains("go install"));
    assert!(!workflow.contains("AGENT_IDE_GO"));
    assert!(!workflow.to_lowercase().contains("gopls"));
    // Publication itself: a draft with every asset and generated notes, refused when any release
    // or draft exists, verified after download, published, then checked once more when visible.
    let release = include_str!("../xtask/src/release.rs");
    for rule in [
        "\"--verify-tag\"",
        "\"--draft\"",
        "\"--generate-notes\"",
        "already exists; refusing to overwrite",
        "left unpublished for inspection",
        "\"--draft=false\"",
        "does not carry exactly the verified assets",
    ] {
        assert!(release.contains(rule), "missing publish rule: {rule}");
    }
}

/// Pins the CI workflow to SHA-pinned actions, the serialized PR gate, the fetch-before-gate
/// phase, the supply-chain job, and the xtask gate's exact rule set, so no former inline gate
/// rule is silently dropped by the delegation to `cargo run --package xtask -- check`.
#[test]
fn ci_workflow_runs_the_xtask_gate_and_supply_chain_job() {
    let workflow = include_str!("../.github/workflows/ci.yml");
    for requirement in [
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
        "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020 # v4.4.0",
        "node-version: \"24.4.0\"",
        "test \"$(uname -m)\" = arm64",
        "rustup component add rustfmt clippy rust-analyzer rust-src",
        "pyright@1.1.413",
        "typescript-language-server@6.0.0",
        "typescript@5.9.3",
        "AGENT_IDE_RUST_ANALYZER=",
        "AGENT_IDE_RUST_TOOLCHAIN_DIR=",
        "AGENT_IDE_PYRIGHT=",
        "AGENT_IDE_NODE=",
        "AGENT_IDE_TSSERVER=",
        "AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER=",
        "cargo fetch --locked",
        "cargo xtask check",
        "cancel-in-progress: true",
        "cargo install cargo-deny --version 0.20.2 --locked",
        "cargo deny --locked check",
    ] {
        assert!(
            workflow.contains(requirement),
            "missing CI requirement: {requirement}"
        );
    }
    assert_eq!(
        workflow.matches("persist-credentials: false").count(),
        2,
        "both jobs must check out without persisting credentials"
    );
    assert!(!workflow.contains("@v4"), "no unpinned action references");
    let release = include_str!("../.github/workflows/release.yml");
    for requirement in [
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
        "actions/setup-node@49933ea5288caeca8642d1e84afbd3f7d6820020 # v4.4.0",
        "cancel-in-progress: false",
    ] {
        assert!(
            release.contains(requirement),
            "missing release workflow requirement: {requirement}"
        );
    }
    assert!(!release.contains("@v4"), "no unpinned action references");
    let xtask = include_str!("../xtask/src/main.rs");
    for rule in [
        "\"fmt\", \"--all\", \"--check\"",
        "\"test\", \"--locked\", \"--workspace\", \"--no-fail-fast\"",
        "--test-threads=1",
        "\"--features\", \"test-seams\"",
        "release_build_ignores_the_version_seams",
        "configured_product_rust_resolves_definition_across_a_crate_boundary",
        "configured_product_returns_real_typescript_family_context_and_reaps",
        "configured_product_returns_real_pyright_semantic_context_and_reaps",
        "configured_product_claude_returns_real_pyright_semantic_context_diff_and_stop",
        "--ignored",
        "--nocapture",
        "\"clippy\"",
        "\"--all-targets\"",
        "-D warnings",
        "\"doc\", \"--locked\", \"--workspace\", \"--no-deps\"",
        "\"build\", \"--locked\", \"--release\", \"--bin\", \"agent-ide\"",
    ] {
        assert!(xtask.contains(rule), "missing xtask gate rule: {rule}");
    }
}

/// Pins the publication evidence gate to all five complete macOS arm64 candidate rows carrying
/// the accepted Rust, Python, and TypeScript/JavaScript toolchain versions (older evidence may
/// still carry `not_tested` Go/gopls rows), while rejecting partial scenario values, mixed
/// revisions, and non-ancestors.
#[test]
fn release_evidence_gate_requires_the_complete_candidate_matrix() {
    let gate = include_str!("../scripts/validate-release-evidence.sh");
    for requirement in [
        "macos-v0.2-product.json|product|product_pass",
        "macos-v0.2-direct-codex.json|codex|real_pass",
        "macos-v0.2-direct-claude.json|claude|real_pass",
        "macos-v0.2-agent-run-claude.json|agent_run_claude|real_pass",
        "macos-v0.2-agent-run-codex.json|agent_run_codex|real_pass",
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
        "with_entries(select(.value != \"not_tested\"))",
        "1.98.1",
        "24.4.0",
        "1.1.413",
        "6.0.0",
        "5.9.3",
    ] {
        assert!(
            gate.contains(version),
            "missing accepted version: {version}"
        );
    }
}

/// Ensures the release archive carries the complete two-host plugin surface, its verification
/// executes the extracted binary directly without Cargo's integration-test executable shortcut,
/// and the shell entry points stay thin wrappers over the one xtask implementation.
#[test]
fn release_archive_and_smoke_use_the_packaged_executable() {
    let release = include_str!("../xtask/src/release.rs");
    for path in [
        ".agents/plugins/marketplace.json",
        ".claude-plugin/marketplace.json",
        ".claude-plugin/plugin.json",
        "agents/ide-reviewer.md",
        ".codex-plugin/plugin.json",
        "docs/release.md",
        "hooks/claude-hook.sh",
        "hooks/hooks.json",
        "skills/agent-ide/SKILL.md",
        "skills/agent-ide/agents/openai.yaml",
    ] {
        assert!(release.contains(path), "missing bundle path: {path}");
    }
    assert!(release.contains("&[\"evidence\", \"executable\", \"--identity\""));
    assert!(release.contains("\"self-install\", \"--release\""));
    assert!(!release.contains("CARGO_BIN_EXE"));
    for (script, task) in [
        (
            include_str!("../scripts/package-release.sh"),
            "package \"$1\" \"$2\" \"$3\"",
        ),
        (
            include_str!("../scripts/release-smoke.sh"),
            "package verify \"$1\"",
        ),
        (
            include_str!("../scripts/wait-release.sh"),
            "release wait \"$@\"",
        ),
    ] {
        assert!(
            script.contains("--package xtask --"),
            "not an xtask wrapper"
        );
        assert!(script.contains(task), "wrapper does not call {task}");
    }
}

/// Verifies Claude hooks cannot select an ambient executable, the Claude plugin manifest
/// registers the standard `hooks/hooks.json` through the explicit `hooks` reference (Claude Code
/// 2.1.274 does not auto-load it for `--plugin-dir` sessions, T33B), and the release guide binds
/// their absolute executable setting to the same stable path used by the MCP command after
/// updates.
#[test]
fn claude_hook_and_mcp_share_the_installed_binary() {
    let hook = include_str!("../hooks/claude-hook.sh");
    let guide = include_str!("../docs/release.md");
    let claude_hooks = include_str!("../hooks/hooks.json");
    let claude_manifest: Value =
        serde_json::from_str(include_str!("../.claude-plugin/plugin.json")).unwrap();
    assert!(hook.contains("[ -n \"${AGENT_IDE_BIN:-}\" ] || exit 0"));
    assert!(hook.contains("exec \"$AGENT_IDE_BIN\" claude-hook"));
    assert!(!hook.contains("command -v"));
    assert_eq!(
        claude_manifest.get("hooks").and_then(Value::as_str),
        Some("./hooks/hooks.json")
    );
    // Inline `--plugin-dir` sessions resolve neither `${CLAUDE_PLUGIN_ROOT}` nor a plugin
    // hook otherwise (T33B); the command stays absolute-safe through the session cwd.
    assert!(claude_hooks.contains("${CLAUDE_PLUGIN_ROOT:-$(pwd)}/hooks/claude-hook.sh"));
    assert!(!claude_hooks.contains("\"${CLAUDE_PLUGIN_ROOT}/"));
    // Claude Code 2.1.280 runs an entry that carries `args` in exec form without a shell, so the
    // `${CLAUDE_PLUGIN_ROOT:-$(pwd)}` command above would never expand and the hook never runs.
    assert!(!claude_hooks.contains("\"args\""));
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
    assert_eq!(claude["plugins"][0]["version"], env!("CARGO_PKG_VERSION"));
}

/// Distinguishes temporary install prefixes across scenarios inside one test-process run.
static NEXT_INSTALL_PREFIX: AtomicUsize = AtomicUsize::new(0);

/// Returns a fresh, unique absolute temp path for one disposable `--prefix`; nothing is created
/// here, and the real `$HOME/.local` is never a candidate.
fn unique_install_prefix(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-install-local-contract-{label}-{}-{}",
        std::process::id(),
        NEXT_INSTALL_PREFIX.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Runs `scripts/install-local.sh --no-build --prefix prefix` and panics with the captured
/// stdout/stderr when it exits non-zero.
fn run_install_local(script: &Path, prefix: &Path) {
    let output = Command::new(script)
        .arg("--no-build")
        .arg("--prefix")
        .arg(prefix)
        // The wrapper keeps immutable releases under the effective home; never the real one.
        .env("AGENT_IDE_HOME", prefix)
        .output()
        .expect("scripts/install-local.sh must execute");
    assert!(
        output.status.success(),
        "install-local.sh failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Asserts the installed layout under `prefix` for `version`: the mode-0755 binary, the plugin
/// bundle's five copied directories plus its two contract files, the baked absolute-path hook
/// naming that exact binary, and the `current` symlink pointing at `version`.
fn assert_install_layout(prefix: &Path, installed_bin: &Path, version: &str) {
    assert!(installed_bin.is_file(), "missing installed binary");
    let bin_mode = std::fs::metadata(installed_bin)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(bin_mode, 0o755, "installed binary must be mode 0755");

    let version_dir = prefix.join("share/agent-ide/plugin").join(version);
    for part in [
        ".claude-plugin",
        ".codex-plugin",
        "agents",
        "hooks",
        "skills",
    ] {
        assert!(
            version_dir.join(part).is_dir(),
            "missing installed bundle part: {part}"
        );
    }
    assert!(version_dir.join("hooks/hooks.json").is_file());
    assert!(version_dir.join("skills/agent-ide/SKILL.md").is_file());
    assert!(version_dir.join("agents/ide-reviewer.md").is_file());

    let hook_path = version_dir.join("hooks/claude-hook.sh");
    let hook_contents = std::fs::read_to_string(&hook_path).unwrap();
    let expected_exec = format!("exec \"{}\" claude-hook", installed_bin.display());
    assert!(
        hook_contents.contains(&expected_exec),
        "generated hook is missing the baked absolute exec line: {hook_contents}"
    );
    let hook_mode = std::fs::metadata(&hook_path).unwrap().permissions().mode();
    assert_ne!(hook_mode & 0o111, 0, "generated hook must be executable");

    let current_link = prefix.join("share/agent-ide/plugin/current");
    let target = std::fs::read_link(&current_link).unwrap();
    assert_eq!(target, PathBuf::from(version));
}

/// Shells out to `scripts/install-local.sh` against disposable temp prefixes, proving the
/// installed layout, the baked hook, the `current` symlink, an idempotent same-version re-run,
/// and backup-file creation ahead of replacing an existing binary — never the real `$HOME/.local`.
/// Skips cleanly when this workspace has no locally built release binary, since the script's own
/// `cargo build` step is exercised separately and this test only shells out with `--no-build`.
#[test]
fn install_local_installs_into_a_disposable_prefix() {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let release_bin = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root.join("target"))
        .join("release/agent-ide");
    if !release_bin.is_file() {
        eprintln!(
            "skipping install_local_installs_into_a_disposable_prefix: no {} \
             (run `cargo build --locked --release` first)",
            release_bin.display()
        );
        return;
    }
    let script = repo_root.join("scripts/install-local.sh");
    let version = env!("CARGO_PKG_VERSION");

    let prefix = unique_install_prefix("layout");
    let installed_bin = prefix.join("bin/agent-ide");
    run_install_local(&script, &prefix);
    assert_install_layout(&prefix, &installed_bin, version);

    // Re-running for the same version must replace the version directory and binary cleanly.
    run_install_local(&script, &prefix);
    assert_install_layout(&prefix, &installed_bin, version);
    let _ = std::fs::remove_dir_all(&prefix);

    // A foreign script at the launcher path is refused: no backup, no replacement.
    let unowned_prefix = unique_install_prefix("unowned");
    let bin_dir = unowned_prefix.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_binary = bin_dir.join("agent-ide");
    std::fs::write(&fake_binary, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake_binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(&script)
        .arg("--no-build")
        .arg("--prefix")
        .arg(&unowned_prefix)
        .output()
        .expect("scripts/install-local.sh must execute");
    assert!(
        !output.status.success(),
        "an unowned launcher must refuse the install:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unowned launcher"),
        "unexpected refusal: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&fake_binary).unwrap(),
        "#!/bin/sh\nexit 1\n",
        "the refused launcher must be untouched"
    );
    let _ = std::fs::remove_dir_all(&unowned_prefix);

    // A previously installed plain binary moves aside exactly once as `agent-ide.bak-*`.
    let backup_prefix = unique_install_prefix("backup");
    let bin_dir = backup_prefix.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let fake_binary = bin_dir.join("agent-ide");
    std::fs::copy(release_bin, &fake_binary).unwrap();
    run_install_local(&script, &backup_prefix);
    let backups = std::fs::read_dir(&bin_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("agent-ide.bak-"))
        })
        .count();
    assert_eq!(
        backups, 1,
        "expected exactly one backup file after reinstall"
    );
    assert!(
        std::fs::read_to_string(backup_prefix.join("bin/agent-ide"))
            .unwrap()
            .starts_with("#!/bin/sh"),
        "the launcher must be the managed shim"
    );
    let _ = std::fs::remove_dir_all(&backup_prefix);
}
