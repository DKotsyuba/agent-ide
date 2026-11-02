//! Native runner settings remain language-owned and preserve selected test arguments.
use agent_ide::execution::test_workers::{TestExecutionConfig, WorkerSetting};
use std::{num::NonZeroU16, path::Path};

/// Keeps each argument, including paths with spaces, as one token.
fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// Supported engines receive their native setting and unrelated programs stay unclaimed.
#[test]
fn declared_options_match_engines() {
    agent_ide::languages::install();
    for (file, argv, expected) in [
        ("a.rs", words(&["cargo", "test", "--workspace"]), "4"),
        (
            "a.py",
            words(&["/env/bin/python", "-m", "pytest", "tests/a b.py"]),
            "-n 4",
        ),
        ("a.ts", words(&["vitest", "run", "a b.test.ts"]), "4"),
        ("a.ts", words(&["jest", "a b.test.ts"]), "4"),
        ("a.ts", words(&["node", "--test", "a b.test.js"]), "4"),
    ] {
        let language = agent_ide::lang::Language::for_path(Path::new(file)).unwrap();
        let setting = language
            .support()
            .test_worker_setting(&argv, NonZeroU16::new(4).unwrap())
            .unwrap();
        assert_eq!(setting.value(), expected);
        let env = setting.apply(&argv, None);
        let tail: Vec<_> = argv[1..].iter().map(std::ffi::OsString::from).collect();
        let actual: Vec<_> = env.argv_prefix.into_iter().chain(tail.clone()).collect();
        assert!(actual.ends_with(&tail));
        assert!(
            language
                .support()
                .test_worker_setting(&words(&["formatter", "file"]), NonZeroU16::new(4).unwrap())
                .is_none()
        );
    }
}

/// Invalid settings never turn into an automatic or unlimited worker pool.
#[test]
fn operator_config_accepts_fixed_or_max() {
    for text in [
        r#"{"workers":1}"#,
        r#"{"workers":4}"#,
        r#"{"workers":"max"}"#,
    ] {
        let config: TestExecutionConfig = serde_json::from_str(text).unwrap();
        assert!((1..=256).contains(&config.workers.resolve().get()));
    }
    for text in [
        r#"{"workers":0}"#,
        r#"{"workers":257}"#,
        r#"{"workers":"auto"}"#,
    ] {
        assert!(serde_json::from_str::<TestExecutionConfig>(text).is_err());
    }
}

/// Updating the worker setting retains unrelated activation variables and executable binding.
#[test]
fn environment_setting_replaces_only_its_own_key() {
    let env = agent_ide::lang::environment::CommandEnv {
        argv_prefix: vec!["/configured/python".into()],
        path_prefix: Some("/configured".into()),
        vars: vec![
            ("OTHER".into(), "kept".into()),
            ("PYTEST_ADDOPTS".into(), "old".into()),
        ],
    };
    let env = WorkerSetting::EnvironmentOptions {
        name: "PYTEST_ADDOPTS",
        value: "-n 2".into(),
    }
    .apply(&words(&["python", "-m", "pytest"]), Some(env));
    assert_eq!(
        env.vars,
        vec![
            ("OTHER".into(), "kept".into()),
            ("PYTEST_ADDOPTS".into(), "old -n 2".into())
        ]
    );
    assert_eq!(
        env.argv_prefix,
        vec![std::ffi::OsString::from("/configured/python")]
    );
}

#[path = "support/parity.rs"]
mod parity;

/// Completes one generated run through the same MCP surface the agent uses.
async fn settled(
    session: &mut parity::Session,
    fixture: &parity::Fixture,
    args: serde_json::Value,
) -> serde_json::Value {
    let first = session.call(fixture, "ide.test", args).await;
    let text = first["text"].as_str().unwrap_or_default();
    let Some(id) = text
        .strip_prefix("tests #")
        .and_then(|s| s.split(':').next())
        .and_then(|s| s.parse::<u64>().ok())
    else {
        return first;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let reply = session
            .call(fixture, "ide.test", serde_json::json!({"status":id}))
            .await;
        let text = reply["text"].as_str().unwrap_or_default();
        if !text.contains("running") && !text.contains("started") {
            return reply;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "test did not settle: {reply}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

/// Operator benchmark of real engines in both native and bundled-module routes.
/// It uses synthetic independent delay tests: timings prove the setting works, not project speedup.
#[tokio::test]
#[ignore = "requires prepared Node, Rust and PYTEST_PARALLEL_ENV; writes TEST_PARALLEL_REPORT"]
async fn real_engines_preserve_quality_and_report_timing() {
    use serde_json::{Value, json};
    let python = std::env::var("PYTEST_PARALLEL_ENV").unwrap();
    let report = std::env::var("TEST_PARALLEL_REPORT").unwrap();
    let mut rows = Vec::new();
    for language in ["python", "rust", "typescript"] {
        for native in [false, true] {
            let mut files: Vec<(String,String)> = match language {
                "python" => vec![("pyproject.toml".into(), "[project]\nname='parallel-fixture'\nversion='0.1.0'\n".into()),
                    ("tests/test_parallel.py".into(), "import time\nimport pytest\n@pytest.mark.parametrize('item',range(8))\ndef test_item(item):\n    time.sleep(0.15)\n    assert item >= 0\n".into())],
                "rust" => vec![("Cargo.toml".into(), "[package]\nname='parallel-fixture'\nversion='0.1.0'\nedition='2024'\n".into()),
                    ("src/lib.rs".into(), (0..8).map(|i| format!("#[test] fn test_{i}() {{ std::thread::sleep(std::time::Duration::from_millis(150)); assert_eq!(1+1,2); }}\n")).collect())],
                _ => vec![("package.json".into(), "{\"name\":\"parallel-fixture\",\"type\":\"module\"}".into()),("tsconfig.json".into(),"{\"compilerOptions\":{\"allowJs\":true,\"checkJs\":false},\"include\":[\"tests\"]}".into())],
            };
            if language == "typescript" {
                for i in 0..8 {
                    files.push((format!("tests/item{i}.test.mjs"), "import test from 'node:test';\nimport assert from 'node:assert/strict';\ntest('item', async()=>{await new Promise(r=>setTimeout(r,150));assert.equal(1+1,2)});\n".into()));
                }
            }
            files.push((".gitignore".into(), ".venv\ntarget\nCargo.lock\n".into()));
            let refs: Vec<_> = files
                .iter()
                .map(|(p, t)| (p.as_str(), t.as_str()))
                .collect();
            let fixture = parity::Fixture::new(&refs, json!([]));
            if language == "python" {
                std::os::unix::fs::symlink(&python, fixture.root.join(".venv")).unwrap();
            }
            let original: Value =
                serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
            // Reuse the same fixture/target to remove compile warmup from comparisons.
            for workers in [Value::Null, json!(1), json!(4), json!("max")] {
                let mut config = original.clone();
                if !workers.is_null() {
                    config["test_execution"] = json!({"workers":workers});
                }
                if language == "rust" {
                    config["project_checks"] = json!({"debounce_ms":100,"check_timeout_s":30,
                        "rust":{"toolchain_dir":std::env::var("AGENT_IDE_RUST_TOOLCHAIN_DIR").unwrap()}});
                }
                if language == "typescript" {
                    let node = std::env::var("AGENT_IDE_NODE").unwrap();
                    let tsserver = std::env::var("AGENT_IDE_TSSERVER").unwrap();
                    config["project_checks"] = json!({"debounce_ms":100,"check_timeout_s":30,"typescript":{"node":node,"tsc_cli":Path::new(&tsserver).with_file_name("tsc.js")}});
                }
                std::fs::write(&fixture.config, config.to_string()).unwrap();
                let mode = format!("{language}=in_process");
                let env = if native {
                    vec![(parity::LANGUAGE_MODE, mode.as_str())]
                } else {
                    vec![]
                };
                let daemon = parity::Daemon::start(&fixture, &env).await;
                let mut session = parity::Session::start(&fixture).await;
                let args = match language {
                    "rust" => json!({"path":"src/lib.rs"}),
                    "python" => json!({"path":"tests/test_parallel.py"}),
                    _ => {
                        json!({"targets":(0..8).map(|i|json!({"path":format!("tests/item{i}.test.mjs")})).collect::<Vec<_>>()})
                    }
                };
                let warm = settled(&mut session, &fixture, args.clone()).await;
                let warm_text = warm["text"].as_str().unwrap_or_default();
                assert!(
                    warm_text.contains("8 passed, 0 failed") && warm_text.contains("(exit 0)"),
                    "warmup {language}: {warm}"
                );
                for repetition in 0..3 {
                    let started = std::time::Instant::now();
                    let result = settled(&mut session, &fixture, args.clone()).await;
                    let elapsed = started.elapsed().as_secs_f64();
                    let text = result["text"].as_str().unwrap_or_default();
                    assert!(
                        text.contains("8 passed, 0 failed") && text.contains("(exit 0)"),
                        "{language} native={native} workers={workers}: {result}"
                    );
                    let reference = text
                        .lines()
                        .find_map(|line| line.strip_prefix("  full replay: ide.test "))
                        .map(|line| serde_json::from_str::<Value>(line).unwrap());
                    let replay = if let Some(reference) = reference {
                        parity::recover_test_replay(
                            &mut session,
                            &fixture,
                            reference["output"].as_str().unwrap(),
                        )
                        .await
                    } else {
                        Value::Null
                    };
                    rows.push(json!({"language":language,"native":native,"workers":workers,"repetition":repetition,"seconds":elapsed,"result":result,"replay":replay}));
                    std::fs::write(&report, serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
                }
                session.close(&fixture).await;
                drop(daemon);
            }
        }
    }
}
