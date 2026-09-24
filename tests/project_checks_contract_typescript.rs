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
    assert!(!specs[0].args.contains(&"--incremental".into()));
    assert!(!specs[0].args.contains(&"--composite".into()));
    assert!(specs[0].args.windows(2).any(|pair| {
        pair[0] == "--tsBuildInfoFile" && pair[1] == root.join("cache/check.tsbuildinfo")
    }));
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

/// A poisoned adapter destination cannot redirect the private-cache write.
#[tokio::test]
async fn adapter_write_replaces_symlink_without_following_it() {
    let root = project("adapter-symlink", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "export const a = 1;\n").unwrap();
    let sentinel = root.join("sentinel.js");
    std::fs::write(&sentinel, "untouched").unwrap();
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    std::os::unix::fs::symlink(&sentinel, cache.join("typescript-check.js")).unwrap();
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let checker = TypeScriptChecker::new(
        Arc::new(FakeRunner::new(vec![Ok(report(&[&source], "", 0))])),
        node,
        cli,
        Duration::from_secs(10),
    );
    let snapshot = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: cache.clone(),
            input_generation: 1,
            read_denies: Vec::new(),
        })
        .await;
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "untouched");
    assert!(
        std::fs::symlink_metadata(cache.join("typescript-check.js"))
            .unwrap()
            .is_file()
    );
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

/// A compiler refusal takes precedence over any partial output and maps to `ReadRestricted`.
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
    let runner = Arc::new(FakeRunner::new(vec![
        Ok(RunOutput {
            status: Some(77),
            ..RunOutput::default()
        }),
        Ok(RunOutput {
            status: Some(77),
            truncated: true,
            ..RunOutput::default()
        }),
    ]));
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
    assert_eq!(runner.specs().len(), 1);
    let truncated = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 2,
            read_denies: vec![ReadDeny::Path(root.join("src"))],
        })
        .await;
    assert_eq!(
        truncated.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

/// A denied symlink target reported by the adapter cannot become a clean snapshot.
#[tokio::test]
async fn nonintersecting_path_deny_rejects_worktree_alias() {
    let root = project("outside-deny-alias", "tsconfig.json");
    std::fs::write(root.join("a.ts"), "const x: number = 1;\n").unwrap();
    std::os::unix::fs::symlink("/Users/pluto/.ssh", root.join("private-link")).unwrap();
    let node = root.join("node");
    let cli = root.join("tsc.js");
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let runner = Arc::new(FakeRunner::new(vec![Ok(RunOutput {
        status: Some(77),
        ..RunOutput::default()
    })]));
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
    assert_eq!(runner.specs().len(), 1);
}

/// Existing irrelevant credentials and safe aliases remain usable; messages stay redacted.
#[tokio::test]
async fn credential_glob_messages_stay_redacted() {
    let root = project("credential-glob", "tsconfig.json");
    let source = root.join("a.ts");
    std::fs::write(&source, "bad\n").unwrap();
    let node = root.join("node-install/bin/node");
    let cli = root.join("ts-install/lib/tsc.js");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    std::fs::write(&node, "node").unwrap();
    std::fs::write(&cli, "cli").unwrap();
    let output = report(
        &[&source],
        &format!(
            "a.ts(1,1): error TS2322: secret at {}/hidden.key\na.ts(1,1): error TS2322: distinct private text\n",
            root.display()
        ),
        2,
    );
    let runner = Arc::new(FakeRunner::new(vec![
        Ok(output.clone()),
        Ok(output.clone()),
        Ok(output),
    ]));
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
        CheckState::Ready
    );
    std::fs::remove_file(root.join("hidden.key")).unwrap();
    std::os::unix::fs::symlink("a.ts", root.join("source-link.ts")).unwrap();
    assert_eq!(checker.check(request).await.state, CheckState::Ready);
    assert_eq!(runner.specs().len(), 3);
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

/// Exercises ordinary credentials, a safe package alias, and one denied extends under Seatbelt.
#[tokio::test]
#[ignore = "requires the local pinned Node/tsc files and macOS sandbox-exec"]
async fn real_confined_tsc_smoke() {
    let root = project("confined", "tsconfig.json");
    std::fs::write(
        root.join("a.ts"),
        "import { x } from 'pkg'; export const a: number = x;\n",
    )
    .unwrap();
    for name in [".env", "secret.key", "secret.pem"] {
        std::fs::write(root.join(name), "credential").unwrap();
    }
    let package = root.join("node_modules/.pnpm/pkg/node_modules/pkg");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("index.d.ts"), "export const x: number;\n").unwrap();
    std::os::unix::fs::symlink(".pnpm/pkg/node_modules/pkg", root.join("node_modules/pkg"))
        .unwrap();
    let denies = vec![
        ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh")),
        ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Env,
        },
        ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Key,
        },
        ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Pem,
        },
    ];
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
            read_denies: denies.clone(),
        })
        .await;
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!((snapshot.errors, snapshot.warnings), (0, 0));
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"extends":"./secret.key","files":["a.ts"]}"#,
    )
    .unwrap();
    let denied = checker
        .check(CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 2,
            read_denies: denies,
        })
        .await;
    assert_eq!(
        denied.state,
        CheckState::Unavailable(UnavailableReason::ReadRestricted)
    );
    assert!(denied.problems.is_empty());
}

/// Replays a real pinned CLI report through admission with default-style credential denies.
/// The CLI process runs directly to isolate parser and admission checks from sandbox availability.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_cli_with_default_style_denies() {
    let root = project("real-default-denies", "tsconfig.json");
    std::fs::write(root.join("tsconfig.json"), r#"{"compilerOptions":{"incremental":true,"composite":true,"extendedDiagnostics":true,"explainFiles":true,"traceResolution":true}}"#).unwrap();
    std::fs::write(
        root.join("a.ts"),
        "import { x } from 'pkg'; export const a: number = x;\n",
    )
    .unwrap();
    for name in [".env", "secret.key", "secret.pem", ".env.local"] {
        std::fs::write(root.join(name), "credential").unwrap();
    }
    let package = root.join("node_modules/.pnpm/pkg/node_modules/pkg");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(root.join("node_modules/.bin")).unwrap();
    std::fs::write(package.join("index.d.ts"), "export const x: number;\n").unwrap();
    std::os::unix::fs::symlink(".pnpm/pkg/node_modules/pkg", root.join("node_modules/pkg"))
        .unwrap();
    std::os::unix::fs::symlink("../pkg/index.d.ts", root.join("node_modules/.bin/pkg")).unwrap();
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
    // The checker embeds its first-party adapter in the private cache before execution.
    let _ = checker.check(request.clone()).await;
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
        "stdout: {}; stderr: {}",
        String::from_utf8_lossy(&process.stdout),
        String::from_utf8_lossy(&process.stderr)
    );
    assert!(!root.join("tsconfig.tsbuildinfo").exists());
    assert!(request.cache_dir.join("check.tsbuildinfo").exists());
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

/// Runs the embedded adapter directly with the pinned compiler after staging it through the checker.
async fn pinned_adapter_output(request: &CheckRequest) -> RunOutput {
    let node = PathBuf::from("/Users/pluto/.nvm/versions/node/v24.4.0/bin/node");
    let cli = PathBuf::from(
        "/Users/pluto/.nvm/versions/node/v24.4.0/lib/node_modules/typescript/lib/tsc.js",
    );
    let checker = TypeScriptChecker::new(
        Arc::new(FakeRunner::default()),
        node,
        cli,
        Duration::from_secs(30),
    );
    let _ = checker.check(request.clone()).await;
    let config = if request.worktree.join("tsconfig.json").exists() {
        request.worktree.join("tsconfig.json")
    } else {
        request.worktree.join("jsconfig.json")
    };
    let spec = checker.run_spec(request, &config);
    let process = std::process::Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .output()
        .unwrap();
    RunOutput {
        status: process.status.code(),
        stdout: process.stdout,
        stderr: process.stderr,
        ..RunOutput::default()
    }
}

/// Selected denied inputs and an alias outside all read grants fail without publishing text.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_denied_inputs_are_read_restricted() {
    for case in ["extends", "source", "directory", "alias", "outside-grant"] {
        let root = project(&format!("real-denied-{case}"), "tsconfig.json");
        let source = root.join("a.ts");
        std::fs::write(&source, "export const a: number = 1;\n").unwrap();
        let denied = match case {
            "extends" => {
                std::fs::write(root.join("secret.key"), "{}").unwrap();
                std::fs::write(
                    root.join("tsconfig.json"),
                    r#"{"extends":"./secret.key","files":["a.ts"]}"#,
                )
                .unwrap();
                ReadDeny::Glob {
                    base: root.clone(),
                    suffix: CredentialGlob::Key,
                }
            }
            "source" => {
                std::fs::write(root.join("tsconfig.json"), r#"{"files":["a.ts"]}"#).unwrap();
                ReadDeny::Path(source)
            }
            "directory" => {
                let private = root.join("private");
                std::fs::create_dir_all(&private).unwrap();
                std::fs::write(private.join("hidden.ts"), "export const hidden = 1;\n").unwrap();
                std::fs::write(
                    root.join("tsconfig.json"),
                    r#"{"include":["private/**/*.ts"]}"#,
                )
                .unwrap();
                ReadDeny::Path(private)
            }
            "alias" => {
                std::os::unix::fs::symlink("a.ts", root.join("alias.ts")).unwrap();
                std::fs::write(root.join("tsconfig.json"), r#"{"files":["alias.ts"]}"#).unwrap();
                ReadDeny::Path(source)
            }
            "outside-grant" => {
                let outside = root.with_extension("outside.ts");
                std::fs::write(&outside, "export const outside = 1;\n").unwrap();
                std::os::unix::fs::symlink(outside, root.join("alias.ts")).unwrap();
                std::fs::write(root.join("tsconfig.json"), r#"{"files":["alias.ts"]}"#).unwrap();
                ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh"))
            }
            _ => unreachable!(),
        };
        let request = CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: vec![denied],
        };
        let output = pinned_adapter_output(&request).await;
        assert_eq!(output.status, Some(77), "{case}");
        let runner = Arc::new(FakeRunner::new(vec![Ok(output)]));
        let checker = TypeScriptChecker::new(
            runner,
            PathBuf::from("/Users/pluto/.nvm/versions/node/v24.4.0/bin/node"),
            PathBuf::from(
                "/Users/pluto/.nvm/versions/node/v24.4.0/lib/node_modules/typescript/lib/tsc.js",
            ),
            Duration::from_secs(30),
        );
        let snapshot = checker.check(request).await;
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::ReadRestricted),
            "{case}"
        );
        assert!(snapshot.problems.is_empty(), "{case}");
    }
}

/// Composite option errors match the pinned CLI while build metadata stays in the private cache.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_composite_diagnostics_match_cli() {
    let root = project("real-composite", "tsconfig.json");
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions":{"composite":true,"declaration":false},"files":["a.ts"]}"#,
    )
    .unwrap();
    std::fs::write(root.join("a.ts"), "export const a: number = 1;\n").unwrap();
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: Vec::new(),
    };
    let adapted = pinned_adapter_output(&request).await;
    let node = "/Users/pluto/.nvm/versions/node/v24.4.0/bin/node";
    let cli = "/Users/pluto/.nvm/versions/node/v24.4.0/lib/node_modules/typescript/lib/tsc.js";
    let baseline = std::process::Command::new(node)
        .args([
            cli,
            "--project",
            "tsconfig.json",
            "--pretty",
            "false",
            "--diagnostics",
            "--listFiles",
            "--noEmit",
            "--tsBuildInfoFile",
        ])
        .arg(request.cache_dir.join("baseline.tsbuildinfo"))
        .args([
            "--extendedDiagnostics",
            "false",
            "--explainFiles",
            "false",
            "--traceResolution",
            "false",
        ])
        .current_dir(&root)
        .output()
        .unwrap();
    assert_eq!(adapted.status, baseline.status.code());
    let adapted_text = String::from_utf8_lossy(&adapted.stdout);
    let baseline_text = String::from_utf8_lossy(&baseline.stdout);
    let diagnostic = "error TS6304: Composite projects may not disable declaration emit.";
    assert!(adapted_text.contains(diagnostic), "{adapted_text}");
    assert!(baseline_text.contains(diagnostic), "{baseline_text}");
    assert!(!root.join("tsconfig.tsbuildinfo").exists());
}

/// A selected source symlink whose target stays inside the grant remains a complete check.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_safe_source_alias_is_ready() {
    let root = project("real-safe-alias", "tsconfig.json");
    std::fs::write(root.join("tsconfig.json"), r#"{"files":["alias.ts"]}"#).unwrap();
    let mut source = vec![0xff, 0xfe];
    for unit in "export const a: number = 1;\n".encode_utf16() {
        source.extend(unit.to_le_bytes());
    }
    std::fs::write(root.join("a.ts"), source).unwrap();
    std::os::unix::fs::symlink("a.ts", root.join("alias.ts")).unwrap();
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Env,
        }],
    };
    let output = pinned_adapter_output(&request).await;
    assert_eq!(
        output.status,
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let snapshot = parse_tsc_output(
        &output,
        &root,
        &root.join("tsconfig.json"),
        &request.read_denies,
        1,
        0,
    );
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
}

/// Excluded file and directory aliases need no target probe; selecting either refuses the read.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_excluded_aliases_do_not_restrict() {
    for kind in ["file", "directory"] {
        let root = project(&format!("real-excluded-alias-{kind}"), "tsconfig.json");
        std::fs::write(root.join("a.ts"), "const a: number = 'bad';\n").unwrap();
        let denied = if kind == "file" {
            let outside = root.with_extension("outside.ts");
            std::fs::write(&outside, "export const outside = 1;\n").unwrap();
            std::os::unix::fs::symlink(outside, root.join("ignored.ts")).unwrap();
            ReadDeny::Path(PathBuf::from("/Users/pluto/.ssh"))
        } else {
            let hidden = root.join("hidden");
            std::fs::create_dir_all(&hidden).unwrap();
            std::fs::write(hidden.join("secret.ts"), "export const secret = 1;\n").unwrap();
            std::os::unix::fs::symlink(&hidden, root.join("ignored")).unwrap();
            ReadDeny::Path(hidden)
        };
        let excluded = if kind == "file" {
            "ignored.ts"
        } else {
            "ignored/**"
        };
        let config = root.join("tsconfig.json");
        std::fs::write(
            &config,
            format!(r#"{{"include":["**/*.ts"],"exclude":["{excluded}","hidden/**"]}}"#),
        )
        .unwrap();
        let request = CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: vec![denied],
        };
        let output = pinned_adapter_output(&request).await;
        assert_eq!(
            output.status,
            Some(2),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let snapshot = parse_tsc_output(&output, &root, &config, &request.read_denies, 1, 0);
        assert_eq!(snapshot.state, CheckState::Ready, "{kind}: {snapshot:?}");
        assert_eq!((snapshot.errors, snapshot.warnings), (1, 0), "{kind}");
        std::fs::write(
            &config,
            r#"{"include":["**/*.ts"],"exclude":["hidden/**"]}"#,
        )
        .unwrap();
        assert_eq!(
            pinned_adapter_output(&request).await.status,
            Some(77),
            "{kind}"
        );
    }
}

/// Irrelevant aliases stay outside TypeScript's source search; selected aliases remain restricted.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_irrelevant_aliases_do_not_restrict() {
    let root = project("real-irrelevant-aliases", "tsconfig.json");
    let config = root.join("tsconfig.json");
    std::fs::write(&config, r#"{"include":["a.ts"]}"#).unwrap();
    std::fs::write(root.join("a.ts"), "const a: number = 'bad';\n").unwrap();
    let outside_env = root.with_extension("outside.env");
    let outside_image = root.with_extension("outside.png");
    std::fs::write(&outside_env, "credential").unwrap();
    std::fs::write(&outside_image, "image").unwrap();
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::os::unix::fs::symlink(&outside_env, root.join(".env")).unwrap();
    std::os::unix::fs::symlink(&outside_image, root.join("assets/logo.png")).unwrap();
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Env,
        }],
    };
    let output = pinned_adapter_output(&request).await;
    assert_eq!(output.status, Some(2));
    let snapshot = parse_tsc_output(&output, &root, &config, &request.read_denies, 1, 0);
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!((snapshot.errors, snapshot.warnings), (1, 0));
    std::os::unix::fs::symlink(&outside_image, root.join("selected.ts")).unwrap();
    std::fs::write(&config, r#"{"include":["a.ts","selected.ts"]}"#).unwrap();
    assert_eq!(pinned_adapter_output(&request).await.status, Some(77));
    std::fs::remove_file(root.join("selected.ts")).unwrap();
    std::fs::remove_file(root.join("assets/logo.png")).unwrap();
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(root.join("named.ts"))
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(&config, r#"{"include":["**/*.ts"]}"#).unwrap();
    let broad = pinned_adapter_output(&request).await;
    assert_eq!(broad.status, Some(2));
    let snapshot = parse_tsc_output(&broad, &root, &config, &request.read_denies, 1, 0);
    assert_eq!((snapshot.errors, snapshot.warnings), (1, 0));
    // With a broad include, this name could be a directory containing TS sources.
    std::os::unix::fs::symlink(&outside_image, root.join("assets/logo.png")).unwrap();
    assert_eq!(pinned_adapter_output(&request).await.status, Some(77));
}

/// A root-based credential glob still excludes a selected `.env` config extension.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_root_credential_glob_restricts() {
    let root = project("real-root-glob", "tsconfig.json");
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"extends":"./.env","files":["a.ts"]}"#,
    )
    .unwrap();
    std::fs::write(root.join(".env"), "{}").unwrap();
    std::fs::write(root.join("a.ts"), "export const a: number = 1;\n").unwrap();
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![ReadDeny::Glob {
            base: PathBuf::from("/"),
            suffix: CredentialGlob::Env,
        }],
    };
    assert_eq!(pinned_adapter_output(&request).await.status, Some(77));
}

/// A JavaScript root retains checkJs diagnostics with unrelated credential files present.
#[tokio::test]
#[ignore = "requires the local pinned Node v24.4.0 and TypeScript 5.9.3"]
async fn real_pinned_jsconfig_reports_checkjs_errors() {
    let root = project("real-js", "jsconfig.json");
    std::fs::write(
        root.join("jsconfig.json"),
        r#"{"compilerOptions":{"checkJs":true},"files":["a.js"]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.js"),
        "/** @type {number} */ const a = 'bad';\n",
    )
    .unwrap();
    std::fs::write(root.join(".env"), "credential").unwrap();
    let request = CheckRequest {
        worktree: root.clone(),
        cache_dir: root.join("cache"),
        input_generation: 1,
        read_denies: vec![ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Env,
        }],
    };
    let output = pinned_adapter_output(&request).await;
    let snapshot = parse_tsc_output(
        &output,
        &root,
        &root.join("jsconfig.json"),
        &request.read_denies,
        1,
        0,
    );
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!((snapshot.errors, snapshot.warnings), (1, 0));
    assert_eq!(snapshot.problems[0].code.as_deref(), Some("TS2322"));
}
