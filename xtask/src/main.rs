//! Development automation for Agent IDE: one gate (`check`), the declarative
//! family standard checks (`standard check`), the exported tool-contract
//! snapshot (`contract check|update`), the daily fault report (`fault-report`, see
//! `faults.rs`) and the release flow (`package`,
//! `package verify`, `release prepare|manifest|publish|wait`, see `release.rs`).
//! Std and `serde_json` only — no runtime interpreter; the only remote writes
//! are `release publish` inside the release workflow.
#![allow(clippy::print_stdout, reason = "Developer CLI, not MCP")]
mod faults;
mod release;

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use serde_json::Value;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Wall-clock ceiling for one MCP handshake plus `tools/list`.
const CONTRACT_DEADLINE: Duration = Duration::from_secs(120);
/// The three real-gopls toolchain contracts CI skips by name: no Go toolchain is installed.
const GOPLS_SKIPS: [&str; 3] = [
    "real_gopls_production_context_tracks_exact_observed_bytes",
    "shared_gopls_isolates_divergent_worktrees_and_detaches_one_view",
    "dropping_live_gopls_owner_closes_its_owned_listener",
];
/// The four ignored real-provider product tests, run exactly as CI does. The fourth name fixes a
/// pre-existing CI typo (`claude_helper_…`) that matched no test and silently ran nothing.
const PROVIDER_TESTS: [&str; 4] = [
    "configured_product_rust_resolves_definition_across_a_crate_boundary",
    "configured_product_returns_real_typescript_family_context_and_reaps",
    "configured_product_returns_real_pyright_semantic_context_and_reaps",
    "configured_product_claude_returns_real_pyright_semantic_context_diff_and_stop",
];
/// Non-Rust tooling source is prohibited outside these declared product assets: the embedded
/// TypeScript adapter ships inside the binary via `include_str!`, and `record.mjs` is the
/// rust-analyzer outline-corpus regeneration utility (both LANG-03 exceptions).
const ALLOWED_NON_RUST: [&str; 2] = [
    "crates/agent-ide-lang-typescript/src/typescript_adapter.js",
    "crates/agent-ide-lang-rust/tests/fixtures/lexical/record.mjs",
];

fn root() -> Result<PathBuf> {
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("workspace root missing")?
        .to_path_buf())
}

/// Cargo's build directory: `$CARGO_TARGET_DIR` when set, the workspace `target/` otherwise.
fn target_dir(root: &Path) -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

fn run(root: &Path, program: &str, args: &[&str]) -> Result<()> {
    eprintln!("xtask: {program} {}", args.join(" "));
    let status = Command::new(program)
        .current_dir(root)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed", args.join(" ")).into())
    }
}

fn run_with_env(root: &Path, program: &str, args: &[&str], env: &[(String, String)]) -> Result<()> {
    eprintln!("xtask: {program} {}", args.join(" "));
    let mut command = Command::new(program);
    command.current_dir(root).args(args);
    for (name, value) in env {
        command.env(name, value);
    }
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed", args.join(" ")).into())
    }
}

/// Returns the right-hand side of `key = value` inside `[section]` (top level for ""), with
/// whitespace, quotes and array brackets removed; `None` when the key is absent. Only scalar and
/// flat string-array values are read this way — every checked value is framing-free text.
fn table_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut current = "";
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            current = name.trim();
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if current == section && name.trim() == key {
            return Some(
                value
                    .trim()
                    .chars()
                    .filter(|c| !matches!(c, '"' | '[' | ']'))
                    .collect::<String>()
                    .trim()
                    .to_owned(),
            );
        }
    }
    None
}

fn read(root: &Path, relative: &str) -> Result<String> {
    Ok(fs::read_to_string(root.join(relative))?)
}

/// Verifies the declarative family invariants: Cargo identity, edition/resolver, shared lints,
/// the pinned toolchain, `family.toml` fields, required files and the non-Rust tooling ban.
fn standard(root: &Path) -> Result<()> {
    let manifest = read(root, "Cargo.toml")?;
    let family = read(root, "family.toml")?;
    let workspace = |key: &str| table_value(&manifest, "workspace", key);
    let inherited = |key: &str| table_value(&manifest, "workspace.package", key);
    let package = |key: &str| table_value(&manifest, "package", key);
    let lints = table_value(&manifest, "lints", "workspace");
    if workspace("resolver").as_deref() != Some("3")
        || !manifest.contains("\"xtask\"")
        || inherited("edition").as_deref() != Some("2024")
        || inherited("license").as_deref() != Some("MIT")
        || package("name").as_deref() != Some("agent-ide")
        || package("publish").as_deref() != Some("false")
        || lints.as_deref() != Some("true")
    {
        return Err(
            "Rust workspace invariant failed (resolver, edition, publish or shared lints)".into(),
        );
    }
    let toolchain = read(root, "rust-toolchain.toml")?;
    if table_value(&toolchain, "toolchain", "channel") != inherited("rust-version") {
        return Err("candidate compiler baseline mismatch".into());
    }
    if family_values(
        &family,
        &[
            ("", "schema_version", "1"),
            ("", "product", "agent-ide"),
            ("", "repository", "DKotsyuba/agent-ide"),
            ("", "env_prefix", "AGENT_IDE_"),
            ("", "standard_version", "1.0.0-rc.2"),
            ("", "response_profile", "rust-minijinja-v1"),
            ("", "qualification", "verified"),
            ("profiles", "process", "resident"),
            ("profiles", "state", "local"),
            ("profiles", "transports", "stdio"),
            ("profiles", "host_adapter", "true"),
            ("compatibility", "qualified_targets", "aarch64-apple-darwin"),
            (
                "compatibility",
                "supported_protocol_revisions",
                "2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25, 2026-07-28",
            ),
            ("release", "workflow", "release.yml"),
            ("release", "delivery_profile", "archive-bundle-v1"),
            ("release", "trust_profile", "github-authenticated"),
        ],
    )? != 0
    {
        return Err("family identity/standard mismatch in family.toml".into());
    }
    for file in [
        "Cargo.lock",
        "AGENTS.md",
        "CLAUDE.md",
        "SECURITY.md",
        "CHANGELOG.md",
        "LICENSE",
        "deny.toml",
        "family.toml",
        ".family/origin.json",
        "schemas/tools.json",
        "docs/MCP_RESPONSE_STANDARD.md",
        "docs/FAMILY_CONTRACT.md",
        "docs/qualification.md",
        ".github/dependabot.yml",
    ] {
        if !root.join(file).is_file() {
            return Err(format!("required file absent: {file}").into());
        }
    }
    for directory in ["src", "xtask", "crates"] {
        let mut scripts = Vec::new();
        collect_non_rust(&root.join(directory), &mut scripts)?;
        for path in scripts {
            if !ALLOWED_NON_RUST.contains(&path.as_str()) {
                return Err(format!("non-Rust tooling source: {path}").into());
            }
        }
    }
    println!("standard: structural checks passed (not a semantic or security certificate)");
    Ok(())
}

/// Counts `family.toml` entries whose value differs from `expected`; also fails on absence.
fn family_values(family: &str, expected: &[(&str, &str, &str)]) -> Result<usize> {
    let mut wrong = 0;
    for (section, key, value) in expected {
        if table_value(family, section, key).as_deref() != Some(*value) {
            wrong += 1;
        }
    }
    Ok(wrong)
}

/// Collects repository-relative paths of `.py`/`.js`/`.mjs`/`.ts`/`.rb`/`.sh` files below `dir`.
fn collect_non_rust(dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let Some(base) = dir.parent() else {
        return Ok(());
    };
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(base, &path, out)?;
            } else if kind.is_file()
                && matches!(
                    path.extension().and_then(|e| e.to_str()),
                    Some("py" | "js" | "mjs" | "ts" | "rb" | "sh")
                )
            {
                out.push(path.strip_prefix(base)?.display().to_string());
            }
        }
        Ok(())
    }
    if dir.is_dir() {
        walk(base, dir, out)?;
    }
    Ok(())
}

/// The full CI gate in one command: standard checks, fmt, the workspace tests (serialized, with
/// the three gopls skips and `--no-fail-fast`, built with the `test-seams` feature the seam-driven
/// product tests need), one default-build run proving a release build ignores those seams, the
/// four ignored provider tests, clippy, rustdoc, the release build and the contract snapshot check.
fn check(root: &Path) -> Result<()> {
    standard(root)?;
    run(root, "cargo", &["fmt", "--all", "--check"])?;
    let mut skips = Vec::new();
    for name in GOPLS_SKIPS {
        skips.push("--skip".to_string());
        skips.push(name.to_string());
    }
    let skip_refs: Vec<&str> = skips.iter().map(String::as_str).collect();
    // The seam-driven product tests need the `test-seams` feature; the next run proves a release
    // build ignores every seam.
    #[rustfmt::skip]
    let workspace_tests = vec!["test", "--locked", "--workspace", "--no-fail-fast", "--features", "test-seams", "--"];
    run(
        root,
        "cargo",
        &[workspace_tests, vec!["--test-threads=1"], skip_refs].concat(),
    )?;
    // A release build (no `test-seams` feature) must ignore every environment seam: the version
    // seams and the job-panic seam.
    run(
        root,
        "cargo",
        &[
            "test",
            "--locked",
            "--test",
            "service_lifecycle_contract",
            "release_build_ignores_the_version_seams",
        ],
    )?;
    run(
        root,
        "cargo",
        &[
            "test",
            "--locked",
            "--test",
            "product_mcp_contract",
            "release_build_ignores_the_panic_seam",
        ],
    )?;
    // The provider tests read their toolchains from the AGENT_IDE_* environment exactly as CI
    // prepares it; nothing here defaults to a developer's local interpreter paths.
    for name in PROVIDER_TESTS {
        run(
            root,
            "cargo",
            &[
                "test",
                "--locked",
                "--test",
                "product_mcp_contract",
                name,
                "--",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ],
        )?;
    }
    run(
        root,
        "cargo",
        &[
            "clippy",
            "--locked",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    )?;
    run_with_env(
        root,
        "cargo",
        &["doc", "--locked", "--workspace", "--no-deps"],
        &[("RUSTDOCFLAGS".to_owned(), "-D warnings".to_owned())],
    )?;
    run(
        root,
        "cargo",
        &["build", "--locked", "--release", "--bin", "agent-ide"],
    )?;
    contract(root, false)
}

/// Builds the binary, performs the MCP handshake and returns the raw `tools/list` response line.
fn tools_list_line(root: &Path) -> Result<String> {
    run(root, "cargo", &["build", "--locked", "--bin", "agent-ide"])?;
    let binary = target_dir(root).join("debug/agent-ide");
    let runtime =
        std::env::temp_dir().join(format!("agent-ide-xtask-contract-{}", std::process::id()));
    let mut command = Command::new(&binary);
    command
        .args(["mcp", "--runtime-dir"])
        .arg(&runtime)
        .env("TOKIO_WORKER_THREADS", "1")
        .env_remove("AGENT_IDE_HOST_ATTACHMENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn()?;
    let mut input = child.stdin.take().ok_or("mcp stdin unavailable")?;
    let output = child.stdout.take().ok_or("mcp stdout unavailable")?;
    let (lines, receiver) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(|line| line.ok()) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });
    let result = (|| -> Result<String> {
        send(
            &mut input,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"xtask-contract","version":"1"}}}"#,
        )?;
        let initialized = receiver.recv_timeout(CONTRACT_DEADLINE)?;
        if !initialized.contains("\"result\"") {
            return Err(format!("initialize failed: {initialized}").into());
        }
        send(
            &mut input,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        )?;
        send(
            &mut input,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        )?;
        loop {
            // Skip any notification lines; the tools/list reply carries id 2.
            let reply = receiver.recv_timeout(CONTRACT_DEADLINE)?;
            if reply.contains("\"id\":2") && reply.contains("\"result\"") {
                return Ok(reply);
            }
        }
    })();
    drop(input);
    reap(child);
    let _ = fs::remove_dir_all(&runtime);
    result
}

/// Writes one newline-terminated JSON-RPC frame.
fn send(input: &mut impl Write, frame: &str) -> Result<()> {
    input.write_all(frame.as_bytes())?;
    input.write_all(b"\n")?;
    input.flush()?;
    Ok(())
}

/// Waits briefly for the MCP process to exit after stdin closes, then kills it.
fn reap(mut child: Child) {
    for _ in 0..100 {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => thread::sleep(Duration::from_millis(100)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Compares (or with `update`, writes) the exported snapshot against the real binary's
/// `tools/list` result. The snapshot is rendered with `serde_json::to_string_pretty`; `check`
/// compares parsed values, so formatting alone is never drift.
fn contract(root: &Path, update: bool) -> Result<()> {
    let reply = tools_list_line(root)?;
    let response: Value = serde_json::from_str(&reply)?;
    let tools = response
        .get("result")
        .and_then(|result| result.get("tools"))
        .cloned()
        .ok_or("tools/list reply has no result.tools")?;
    let path = root.join("schemas/tools.json");
    if update {
        fs::create_dir_all(path.parent().ok_or("snapshot parent missing")?)?;
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&tools)?),
        )?;
        println!("contract: schemas/tools.json updated");
        Ok(())
    } else {
        let text = fs::read_to_string(&path).map_err(
            |_| "contract snapshot absent: run `cargo xtask contract update` and commit it",
        )?;
        let snapshot: Value = serde_json::from_str(&text)
            .map_err(|_| "schemas/tools.json is not valid JSON; regenerate it with `cargo xtask contract update`")?;
        if tools != snapshot {
            return Err(
                "contract drift: the binary's tools/list differs from schemas/tools.json; \
                 run `cargo xtask contract update` and review the diff"
                    .into(),
            );
        }
        println!("contract: schemas/tools.json matches the binary");
        Ok(())
    }
}

fn usage() -> &'static str {
    "usage: cargo xtask check | standard check | contract check|update
       | fault-report [--root DIR] [--since DAY] [--until DAY|RFC3339] [--days N] [--scope field|test|all] [--alert-threshold PERCENT] [--min-calls N]
       | package BINARY TAG [OUTPUT_DIR] | package verify ARCHIVE
       | release prepare VERSION [--apply] | release manifest DIR | release publish DIR
       | release wait --repo OWNER/NAME --tag vX.Y.Z --commit SHA [--run-id N] [--timeout S] [--result-file PATH]"
}

fn main_result() -> Result<()> {
    let root = root()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.as_slice() {
        [task] if task == "check" => check(&root),
        [task, sub] if task == "standard" && sub == "check" => standard(&root),
        [task, sub] if task == "contract" && sub == "check" => contract(&root, false),
        [task, sub] if task == "contract" && sub == "update" => contract(&root, true),
        [task, rest @ ..] if task == "fault-report" => faults::run(&faults::parse(rest)?),
        [task, sub, asset] if task == "package" && sub == "verify" => {
            release::verify(Path::new(asset))
        }
        [task, binary, tag, rest @ ..] if task == "package" && rest.len() <= 1 => {
            let output = rest.first().map_or_else(
                || target_dir(&root).join("package").join(tag),
                PathBuf::from,
            );
            release::package(&root, Path::new(binary), tag, &output).map(|_| ())
        }
        [task, sub, version, rest @ ..]
            if task == "release" && sub == "prepare" && rest.len() <= 1 =>
        {
            let apply = match rest {
                [] => false,
                [flag] if flag == "--apply" => true,
                _ => return Err(usage().into()),
            };
            release::prepare(&root, version, apply, &release::today()?)
        }
        [task, sub, dir] if task == "release" && sub == "manifest" => {
            release::manifest(&root, Path::new(dir))
        }
        [task, sub, dir] if task == "release" && sub == "publish" => {
            release::publish_from_env(&root, Path::new(dir))
        }
        [task, sub, rest @ ..] if task == "release" && sub == "wait" => {
            release::wait(Path::new("gh"), &release::wait_args(&root, rest)?)
        }
        _ => Err(usage().into()),
    }
}

fn main() -> ExitCode {
    match main_result() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_section_values() {
        let text = "top = \"a\"\n[s]\nk = \"v\"\nn = true\narr = [\"stdio\"]\n";
        assert_eq!(table_value(text, "", "top").as_deref(), Some("a"));
        assert_eq!(table_value(text, "s", "k").as_deref(), Some("v"));
        assert_eq!(table_value(text, "s", "n").as_deref(), Some("true"));
        assert_eq!(table_value(text, "s", "arr").as_deref(), Some("stdio"));
        assert_eq!(table_value(text, "s", "missing"), None);
    }

    #[test]
    fn non_rust_inventory_flags_only_declared_files() -> Result<()> {
        let root = root()?;
        let mut found = Vec::new();
        collect_non_rust(&root.join("crates"), &mut found)?;
        assert_eq!(
            found,
            vec![
                "crates/agent-ide-lang-rust/tests/fixtures/lexical/record.mjs".to_owned(),
                "crates/agent-ide-lang-typescript/src/typescript_adapter.js".to_owned(),
            ]
        );
        Ok(())
    }
}
