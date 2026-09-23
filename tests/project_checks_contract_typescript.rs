//! Contract checks for root-config TypeScript and JavaScript project snapshots.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_ide::assistance::problems::{parse_language, problems_text};
use agent_ide::checks::runner::{FakeRunner, RunOutput, SeatbeltRunner};
use agent_ide::checks::typescript::{TypeScriptChecker, parse_tsc_output};
use agent_ide::checks::{
    CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason,
};
use agent_ide::execution::seatbelt::{CredentialGlob, ReadDeny};
use agent_ide::feed::{FeedKey, FeedState};

/// Creates a distinct disposable project root with the selected root config.
fn project(name: &str, config: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("agent-ide-ts-check-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    std::fs::write(root.join(config), "{}").unwrap();
    root
}

/// Builds one complete pinned-CLI report with `files` absolute paths and optional diagnostics.
fn report(files: &[&Path], diagnostics: &str, status: i32) -> RunOutput {
    let mut stdout = diagnostics.to_owned();
    for file in files {
        stdout.push_str(&format!("{}\n", file.display()));
    }
    stdout.push_str(&format!("Files: {}\nLines: 1\nIdentifiers: 1\nSymbols: 1\nTypes: 1\nInstantiations: 0\nMemory used: 1K\nI/O read: 0.00s\nI/O write: 0.00s\nParse time: 0.00s\nBind time: 0.00s\nCheck time: 0.00s\nEmit time: 0.00s\nTotal time: 0.00s\n", files.len()));
    RunOutput {
        status: Some(status),
        stdout: stdout.into_bytes(),
        ..RunOutput::default()
    }
}

/// Parses one output against the selected root config with no host exclusions.
fn parsed(root: &Path, config: &str, output: &RunOutput) -> ProblemSnapshot {
    parse_tsc_output(output, root, &root.join(config), &[], 7, 3)
}

/// A complete clean report with a project source file proves exact `Ready 0`.
#[tokio::test]
async fn clean_project_is_ready_zero() {
    let root = project("clean", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "const x: number = 1;\n").unwrap();
    let node = root.join("node-install/bin/node");
    let cli = root.join("ts-install/lib/tsc.js");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::new(vec![Ok(report(&[&source], "", 0))]));
    let checker = TypeScriptChecker::new(
        runner.clone(),
        node.clone(),
        cli.clone(),
        Duration::from_secs(10),
    );
    let snapshot = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 7,
            read_denies: Vec::new(),
        })
        .await;
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!((snapshot.errors, snapshot.warnings), (0, 0));
    assert!(snapshot.problems.is_empty());
    let specs = runner.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].program, node);
    assert_eq!(specs[0].write_roots, vec![root.join("cache")]);
    assert!(specs[0].read_roots.contains(&node));
    assert!(!specs[0].read_roots.contains(&root.join("node-install")));
    assert!(specs[0].read_roots.contains(&root.join("ts-install")));
    assert!(specs[0].args.contains(&"--noEmit".into()));
    for option in [
        "--incremental",
        "--composite",
        "--extendedDiagnostics",
        "--explainFiles",
        "--traceResolution",
    ] {
        assert!(
            specs[0]
                .args
                .windows(2)
                .any(|pair| pair[0] == option && pair[1] == "false"),
            "{option} must override project config"
        );
    }
    let blocked = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 8,
            read_denies: vec![ReadDeny::Path(root.join("ts-install/lib"))],
        })
        .await;
    assert_eq!(
        blocked.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    assert_eq!(runner.specs().len(), 1);
}

/// TS and checkJs diagnostics retain their project-relative path, position, and TS code.
#[test]
fn typescript_and_checkjs_errors_are_exact() {
    for (name, config, source, column) in [
        ("ts", "tsconfig.json", "a.ts", 7),
        ("js", "jsconfig.json", "a.js", 12),
    ] {
        let root = project(name, config);
        let file = root.join(source);
        std::fs::write(&file, "bad\n").unwrap();
        let output = report(
            &[&file],
            &format!(
                "{source}(1,{column}): error TS2322: Type 'string' is not assignable to type 'number'.\n"
            ),
            2,
        );
        let snapshot = parsed(&root, config, &output);
        assert_eq!(snapshot.state, CheckState::Ready);
        assert_eq!((snapshot.errors, snapshot.warnings), (1, 0));
        let problem = &snapshot.problems[0];
        assert_eq!(
            (
                &problem.path,
                problem.line,
                problem.column,
                problem.code.as_deref()
            ),
            (&source.to_string(), 1, column, Some("TS2322"))
        );
    }
}

/// File-less project errors attach to the chosen config at line and column zero.
#[test]
fn project_diagnostic_uses_config_line_zero() {
    let root = project("project-error", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "x\n").unwrap();
    let snapshot = parsed(
        &root,
        "tsconfig.json",
        &report(&[&source], "error TS5096: Bad project option.\n", 2),
    );
    assert_eq!(snapshot.state, CheckState::Ready);
    let problem = &snapshot.problems[0];
    assert_eq!(
        (
            &problem.path,
            problem.line,
            problem.column,
            problem.code.as_deref()
        ),
        (&"tsconfig.json".to_string(), 0, 0, Some("TS5096"))
    );
}

/// Empty and solution-style roots carry `NoFiles`, even with a config diagnostic or exit zero.
#[test]
fn zero_file_roots_are_unavailable() {
    let root = project("no-files", "tsconfig.json");
    for output in [
        report(&[], "", 0),
        report(
            &[],
            "tsconfig.json(1,1): error TS18002: Empty files list.\n",
            2,
        ),
    ] {
        assert_eq!(
            parsed(&root, "tsconfig.json", &output).state,
            CheckState::Unavailable(UnavailableReason::NoFiles)
        );
    }
}

/// A malformed, truncated, or abnormal result cannot become clean.
#[test]
fn incomplete_and_abnormal_output_fails_closed() {
    let root = project("fatal", "tsconfig.json");
    let file = root.join("a.ts");
    std::fs::write(&file, "x\n").unwrap();
    let good = report(&[&file], "", 0);
    let mut cases = vec![report(&[&file], "unexpected\n", 0), report(&[&file], "", 2)];
    let mut truncated = good.clone();
    truncated.truncated = true;
    cases.push(truncated);
    let mut cut = good.clone();
    cut.stdout.truncate(cut.stdout.len() - 14);
    cases.push(cut);
    let mut malformed_footer = good.clone();
    malformed_footer.stdout = String::from_utf8(malformed_footer.stdout)
        .unwrap()
        .replace("Lines: 1", "Lines: wrong")
        .into_bytes();
    cases.push(malformed_footer);
    for output in cases {
        assert_eq!(
            parsed(&root, "tsconfig.json", &output).state,
            CheckState::Unavailable(UnavailableReason::Fatal)
        );
    }
}

/// A denied diagnostic file invalidates the whole result instead of dropping an error count.
#[test]
fn denied_problem_path_fails_closed() {
    let root = project("denied-problem", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "bad\n").unwrap();
    let output = report(&[&source], "a.ts(1,1): error TS2322: Bad type.\n", 2);
    let snapshot = parse_tsc_output(
        &output,
        &root,
        &root.join("tsconfig.json"),
        &[ReadDeny::Path(source)],
        1,
        0,
    );
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

/// A denied config or tool returns unavailable before the fake runner is invoked.
#[tokio::test]
async fn denied_config_or_tool_is_unavailable() {
    let root = project("denied", "tsconfig.json");
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    for (denied, reason) in [
        (
            root.join("tsconfig.json"),
            UnavailableReason::ReadRestricted,
        ),
        (node.clone(), UnavailableReason::ReadRestricted),
    ] {
        let runner = Arc::new(FakeRunner::default());
        let checker = TypeScriptChecker::new(
            runner.clone(),
            node.clone(),
            cli.clone(),
            Duration::from_secs(10),
        );
        let request = CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: vec![ReadDeny::Path(denied)],
        };
        assert_eq!(
            checker.check(request).await.state,
            CheckState::Unavailable(reason)
        );
        assert!(runner.specs().is_empty());
    }
}

/// A denied source subtree refuses execution even when an allowed sibling could type-check clean.
#[tokio::test]
async fn source_overlapping_path_deny_is_read_restricted() {
    let root = project("source-deny", "tsconfig.json");
    let source = root.join("src/hidden.ts");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "const hidden = 1;\n").unwrap();
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::default());
    let checker = TypeScriptChecker::new(runner.clone(), node, cli, Duration::from_secs(10));
    let snapshot = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: vec![ReadDeny::Path(root.join("src"))],
        })
        .await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    assert!(runner.specs().is_empty());
}

/// A symlink into a nonintersecting host path deny prevents a false clean snapshot.
#[tokio::test]
async fn nonintersecting_path_deny_rejects_worktree_alias() {
    let root = project("outside-deny-alias", "tsconfig.json");
    std::fs::write(root.join("a.ts"), "const x: number = 1;\n").unwrap();
    std::os::unix::fs::symlink("/Users/pluto/.ssh", root.join("private-link")).unwrap();
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::default());
    let checker = TypeScriptChecker::new(runner.clone(), node, cli, Duration::from_secs(10));
    let snapshot = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: vec![ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh"))],
        })
        .await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    assert!(runner.specs().is_empty());
}

/// An empty credential glob remains usable, redacts messages, and rejects later matches or links.
#[tokio::test]
async fn credential_glob_preflight_and_message_redaction() {
    let root = project("credential-glob", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "bad\n").unwrap();
    let node = root.join("node-install/bin/node");
    let cli = root.join("ts-install/lib/tsc.js");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::new(vec![Ok(report(
        &[&source],
        &format!(
            "a.ts(1,1): error TS2322: secret at {}/hidden.key\na.ts(1,1): error TS2322: distinct private text\n",
            root.display()
        ),
        2,
    ))]));
    let checker = TypeScriptChecker::new(runner.clone(), node, cli, Duration::from_secs(10));
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![
            ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh")),
            ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::Key,
            },
        ],
    };
    let snapshot = checker.check(request.clone()).await;
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(
        snapshot.errors, 2,
        "redaction must preserve distinct diagnostic counts"
    );
    assert_eq!(snapshot.problems.len(), 2);
    assert_eq!(snapshot.problems[0].path, "a.ts");
    assert_eq!(snapshot.problems[0].code.as_deref(), Some("TS2322"));
    assert_eq!(
        snapshot.problems[0].message,
        "[redacted by host read policy]"
    );
    std::fs::write(root.join("hidden.key"), "credential").unwrap();
    assert_eq!(
        checker.check(request.clone()).await.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    std::fs::remove_file(root.join("hidden.key")).unwrap();
    std::os::unix::fs::symlink("a.ts", root.join("source-link.ts")).unwrap();
    assert_eq!(
        checker.check(request).await.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    assert_eq!(runner.specs().len(), 1);
}

/// A symlinked tsconfig defers to a regular jsconfig; a lone link never proves a project.
#[tokio::test]
async fn config_selection_prefers_regular_root_file() {
    let root = project("config-choice", "jsconfig.json");
    std::os::unix::fs::symlink("jsconfig.json", root.join("tsconfig.json")).unwrap();
    let source = root.join("a.js");
    std::fs::write(&source, "let x = 1;\n").unwrap();
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::new(vec![Ok(report(&[&source], "", 0))]));
    let checker = TypeScriptChecker::new(runner.clone(), node, cli, Duration::from_secs(10));
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: Vec::new(),
    };
    assert_eq!(
        checker.check(request.clone()).await.state,
        CheckState::Ready
    );
    assert!(
        runner.specs()[0]
            .args
            .contains(&root.join("jsconfig.json").into_os_string())
    );
    std::fs::remove_file(root.join("jsconfig.json")).unwrap();
    assert_eq!(
        checker.check(request).await.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
}

/// Presence, problems filtering, and feed order add TypeScript after Rust and Python only when configured.
#[test]
fn presence_filter_and_feed_keep_three_language_order() {
    let root = project("presence", "tsconfig.json");
    assert!(Language::TypeScript.is_present(&root));
    let package_only =
        std::env::temp_dir().join(format!("agent-ide-package-only-{}", std::process::id()));
    std::fs::create_dir_all(&package_only).unwrap();
    std::fs::write(package_only.join("package.json"), "{}").unwrap();
    assert!(!Language::TypeScript.is_present(&package_only));
    assert_eq!(parse_language("typescript"), Some(Language::TypeScript));
    let snapshots = [Language::TypeScript, Language::Python, Language::Rust].map(|language| {
        ProblemSnapshot::from_problems(language, CheckState::Ready, Vec::new(), 1, 0)
    });
    assert_eq!(
        problems_text(&snapshots, Some(Language::TypeScript), 0),
        "typescript: ready; errors: 0; warnings: 0"
    );
    let mut feed = FeedState::default();
    let block = feed
        .next_block(
            &FeedKey {
                binding: "actor".into(),
                worktree: root,
            },
            &snapshots,
            &[],
        )
        .unwrap();
    let rust = block.find("rust:").unwrap();
    let python = block.find("python:").unwrap();
    let typescript = block.find("typescript:").unwrap();
    assert!(rust < python && python < typescript, "{block}");
    let old_snapshots = &snapshots[1..];
    let old_block = FeedState::default()
        .next_block(
            &FeedKey {
                binding: "old".into(),
                worktree: package_only,
            },
            old_snapshots,
            &[],
        )
        .unwrap();
    assert!(old_block.contains("rust:") && old_block.contains("python:"));
    assert!(!old_block.contains("typescript:"));
}

/// Exercises the pinned CLI through the production Seatbelt runner on a clean root config.
#[tokio::test]
#[ignore = "requires the local pinned Node/tsc files and macOS sandbox-exec"]
async fn real_confined_tsc_smoke() {
    let root = project("confined", "tsconfig.json");
    std::fs::write(root.join("a.ts"), "const x: number = 1;\n").unwrap();
    let checker = TypeScriptChecker::new(
        Arc::new(SeatbeltRunner),
        PathBuf::from("/Users/pluto/.nvm/versions/node/v24.4.0/bin/node"),
        PathBuf::from(
            "/Users/pluto/.nvm/versions/node/v24.4.0/lib/node_modules/typescript/lib/tsc.js",
        ),
        Duration::from_secs(30),
    );
    let snapshot = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: Vec::new(),
        })
        .await;
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!((snapshot.errors, snapshot.warnings), (0, 0));
}

/// Replays a real pinned CLI report through admission with default-style credential denies.
/// The CLI process runs directly to isolate parser and admission checks from sandbox availability.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_cli_with_default_style_denies() {
    let root = project("real-default-denies", "tsconfig.json");
    std::fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"incremental":true,"composite":true,"extendedDiagnostics":true,"explainFiles":true,"traceResolution":true}}"#).unwrap();
    std::fs::write(root.join("a.ts"), "const x: number = 1;\n").unwrap();
    let node = PathBuf::from("/Users/pluto/.nvm/versions/node/v24.4.0/bin/node");
    let cli = PathBuf::from(
        "/Users/pluto/.nvm/versions/node/v24.4.0/lib/node_modules/typescript/lib/tsc.js",
    );
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![
            ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh")),
            ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::Key,
            },
            ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::Pem,
            },
            ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::Env,
            },
            ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::EnvDot,
            },
        ],
    };
    std::fs::create_dir_all(request.cache_dir.join("tmp")).unwrap();
    let checker = TypeScriptChecker::new(
        Arc::new(FakeRunner::default()),
        node.clone(),
        cli.clone(),
        Duration::from_secs(30),
    );
    let spec = checker.run_spec(&request, &root.join("tsconfig.json"));
    assert!(spec.read_roots.contains(&node));
    assert!(!spec.read_roots.contains(&PathBuf::from("/private/etc")));
    assert!(
        !spec
            .read_roots
            .contains(&node.parent().unwrap().parent().unwrap().to_path_buf())
    );
    let process = std::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    assert_eq!(
        process.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&process.stdout)
    );
    assert!(!root.join("tsconfig.tsbuildinfo").exists());
    let runner = Arc::new(FakeRunner::new(vec![Ok(RunOutput {
        status: process.status.code(),
        stdout: process.stdout,
        stderr: process.stderr,
        ..RunOutput::default()
    })]));
    let checker = TypeScriptChecker::new(runner.clone(), node, cli, Duration::from_secs(30));
    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!((snapshot.errors, snapshot.warnings), (0, 0));
    assert_eq!(runner.specs().len(), 1);
}
