//! Static release contracts for versions, quality gates, evidence, packaging, and host wiring.

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use serde_json::Value;

/// Keeps the Rust package and both plugin manifests on the exact release version.
#[test]
fn release_versions_are_synchronized() {
    assert!(include_str!("../Cargo.toml").contains("version = \"0.4.1\""));
    assert!(include_str!("../Cargo.lock").contains("name = \"agent-ide\"\nversion = \"0.4.1\""));
    for manifest in [
        include_str!("../.codex-plugin/plugin.json"),
        include_str!("../.claude-plugin/plugin.json"),
    ] {
        let manifest: Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(manifest["version"], "0.4.1");
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

/// Requires every relevant CI, product-acceptance, evidence, package, and artifact-smoke gate
/// to precede the only GitHub Release publication command without a continue-on-error escape.
/// Go and gopls are outside the release scope, so no Go toolchain is installed and the workspace
/// gate skips exactly the three real-gopls toolchain contracts while running every other test.
#[test]
fn release_workflow_requires_complete_gates_before_publication() {
    let workflow = include_str!("../.github/workflows/release.yml");
    let publish = workflow.find("gh release create").unwrap();
    for gate in [
        "fetch-depth: 0",
        "node-version: \"24.4.0\"",
        "rustup component add rustfmt clippy rust-analyzer rust-src",
        "pyright@1.1.413",
        "typescript-language-server@6.0.0",
        "typescript@5.9.3",
        "cargo fmt --check",
        "cargo test --locked --workspace -- --test-threads=1 --skip real_gopls_production_context_tracks_exact_observed_bytes --skip shared_gopls_isolates_divergent_worktrees_and_detaches_one_view --skip dropping_live_gopls_owner_closes_its_owned_listener",
        "cargo clippy --locked --workspace --all-targets -- -D warnings",
        "cargo doc --locked --workspace --no-deps",
        "scripts/macos-acceptance.sh --route product",
        "scripts/validate-release-evidence.sh \"$GITHUB_SHA\"",
        "cargo build --locked --release --bin agent-ide",
        "scripts/package-release.sh",
        "scripts/release-smoke.sh \"$ASSET\"",
        // The bootstrap installer ships as a release asset, checksummed with the tarball and
        // covered by build-provenance attestation before publication.
        "shasum -a 256 \"$ASSET\" install.sh > SHA256SUMS",
        "actions/attest-build-provenance@",
        "subject-checksums: SHA256SUMS",
        "id-token: write",
        "attestations: write",
    ] {
        assert!(workflow[..publish].contains(gate), "missing gate: {gate}");
    }
    assert!(
        workflow[publish..]
            .starts_with("gh release create \"$GITHUB_REF_NAME\" \"$ASSET\" install.sh SHA256SUMS"),
        "the release must attach the tarball, install.sh, and SHA256SUMS"
    );
    assert!(!workflow.contains("continue-on-error"));
    assert!(!workflow.contains("setup-go"));
    assert!(!workflow.contains("go-version"));
    assert!(!workflow.contains("go install"));
    assert!(!workflow.contains("AGENT_IDE_GO"));
}

/// Pins the publication evidence gate to all five complete macOS arm64 candidate rows carrying
/// the accepted Rust, Python, and TypeScript/JavaScript toolchain versions with honestly
/// `not_tested` Go/gopls rows, while rejecting partial scenario values, mixed revisions, and
/// non-ancestors.
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
        "\"go\": \"not_tested\"",
        "\"gopls\": \"not_tested\"",
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
    assert_eq!(claude["plugins"][0]["version"], "0.4.1");
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
/// bundle's four copied directories plus its two contract files, the baked absolute-path hook
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
    for part in [".claude-plugin", ".codex-plugin", "hooks", "skills"] {
        assert!(
            version_dir.join(part).is_dir(),
            "missing installed bundle part: {part}"
        );
    }
    assert!(version_dir.join("hooks/hooks.json").is_file());
    assert!(version_dir.join("skills/agent-ide/SKILL.md").is_file());

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
    let release_bin = repo_root.join("target/release/agent-ide");
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
