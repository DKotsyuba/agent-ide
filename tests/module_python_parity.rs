//! The Python module against the in-process path on one real Pyright fixture: the same MCP
//! transcript in module mode (the shipped default) and with `AGENT_IDE_LANGUAGE_MODE=python=
//! in_process`; analyzer and checker faults answered with a typed refusal on the same daemon and
//! recovered by a fresh module; a widened checker run refused by the core; a check report above
//! 2 MiB landing; and a measurement harness. They need an accepted Pyright, Node and Python
//! (`AGENT_IDE_PYRIGHT`, `AGENT_IDE_NODE`, `AGENT_IDE_PYTHON`) and, for the fault cases, a
//! `test-seams` build.

#[path = "support/parity.rs"]
mod parity;

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use parity::{Daemon, Fixture, LANGUAGE_MODE, Node, ProcessIdentity, Session, line_for};
use serde_json::{Value, json};

/// The in-process fallback of the Python module.
const IN_PROCESS: (&str, &str) = (LANGUAGE_MODE, "python=in_process");
/// The module fault seam (`agent_ide_core::modules::serve::FAULT_SEAM`).
const FAULT_SEAM: &str = "AGENT_IDE_TEST_MODULE_FAULT";
/// The module request budget seam (`agent_ide_core::modules::router::BUDGET_SEAM`).
const BUDGET_SEAM: &str = "AGENT_IDE_TEST_MODULE_BUDGET_MS";

/// An accepted program entry of the launcher configuration.
fn accepted_program(path: &str, identity: &str) -> Value {
    json!({"path":path,"identity":identity,"blake3":blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()})
}

/// The accepted Pyright provider from the environment's exact paths.
fn pyright_provider(cache_namespace: &str) -> Value {
    let pyright = std::env::var("AGENT_IDE_PYRIGHT").unwrap();
    let node = std::env::var("AGENT_IDE_NODE").unwrap();
    json!({
        "executable":accepted_program(&pyright,"pyright 1.1.413"),
        "settings":"pyright_defaults_v1",
        "toolchain":"node-fixture",
        "node":accepted_program(&node,"node-fixture"),
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":cache_namespace
    })
}

/// A `.venv` in `dir` whose `bin/python` links the accepted `AGENT_IDE_PYTHON` interpreter.
fn python_venv_at(dir: &Path) {
    python_venv_named(dir, ".venv");
}

/// A virtual environment `name` in `dir` whose `bin/python` links the accepted
/// `AGENT_IDE_PYTHON` interpreter.
fn python_venv_named(dir: &Path, name: &str) {
    let python = PathBuf::from(std::env::var_os("AGENT_IDE_PYTHON").unwrap());
    assert!(python.is_file(), "approved Python interpreter is available");
    let venv = dir.join(name);
    let interpreter = venv.join("bin/python");
    std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&python, &interpreter).unwrap();
    std::fs::write(
        venv.join("pyvenv.cfg"),
        format!(
            "home = {}\ninclude-system-site-packages = false\nversion = 3.14.3\n",
            python.parent().unwrap().display()
        ),
    )
    .unwrap();
}

/// The accepted Pyright's own CLI module (beside its language server).
fn pyright_cli() -> PathBuf {
    let pyright = std::env::var("AGENT_IDE_PYRIGHT").unwrap();
    Path::new(&pyright)
        .parent()
        .and_then(Path::parent)
        .map(|bin| bin.join("lib/node_modules/pyright/dist/pyright.js"))
        .map(|cli| std::fs::canonicalize(&cli).unwrap_or(cli))
        .expect("the pyright CLI module sits beside the language server")
}

/// A Python fixture with a cross-file call, a class method, a type error, a `.venv`, the real
/// Pyright analyzer and confined Pyright checks (`main` replaces `main.py` when given).
fn python_fixture(cache: &str, main: Option<&str>) -> Fixture {
    let main = main.unwrap_or(
        "from helper import double\n\n\nclass Greeter:\n    def greet(self, name: str) -> str:\n        return \"hi \" + name\n\n\ndef run() -> int:\n    return double(2)\n\n\ndef bad() -> int:\n    return \"bad\"\n",
    );
    let fixture = Fixture::new(
        &[
            ("pyproject.toml", "[project]\nname = \"parity\"\n"),
            (
                "helper.py",
                "def double(x: int) -> int:\n    return x * 2\n",
            ),
            ("main.py", main),
            ("style.css", ".btn { color: red; }\n"),
            (
                "pyrightconfig.json",
                "{\"include\": [\"main.py\", \"helper.py\"]}\n",
            ),
        ],
        json!([pyright_provider(cache)]),
    );
    let pyright_cli = pyright_cli();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["project_checks"] = json!({
        "debounce_ms":100,
        "check_timeout_s":10,
        "python":{"node":std::env::var("AGENT_IDE_NODE").unwrap(),"pyright_cli":pyright_cli}
    });
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    python_venv_at(&fixture.root);
    fixture
}

/// The read-only tool calls whose replies must not depend on where Python computes.
fn calls() -> Vec<(&'static str, Value)> {
    vec![
        ("ide.outline", json!({"path":"main.py"})),
        ("ide.outline", json!({"path":"helper.py"})),
        ("ide.symbol", json!({"symbol":"helper.py#double"})),
        ("ide.symbol", json!({"symbol":"main.py#run","callees":1})),
        ("ide.symbol", json!({"symbol":"double"})),
        ("ide.read", json!({"symbol":"main.py#Greeter/greet"})),
        ("ide.graph", json!({"symbol":"helper.py#double"})),
        ("ide.context", json!({"path":"main.py","byte_offset":141})),
    ]
}

/// Polls Python's problems page until its check lands and `done` holds (or 120 s pass).
async fn problems(session: &mut Session, fixture: &Fixture, done: impl Fn(&str) -> bool) -> String {
    let mut text = String::new();
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let reply = session
            .call(
                fixture,
                "ide.context",
                json!({"kind":"problems","language":"python"}),
            )
            .await;
        text = reply["text"].as_str().unwrap_or_default().to_owned();
        if !text.contains("python: checking") && text.contains("python: ") && done(&text) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    text
}

/// The `module python analyzer` child that hosts Pyright (it has a language-server child).
fn provider_module(daemon: &mut Daemon) -> Option<Node> {
    daemon.tree().children.into_iter().find(|node| {
        node.command.ends_with("module python analyzer")
            && node
                .children
                .iter()
                .any(|(_, command)| command.contains("langserver"))
    })
}

/// After a Python fault: the same daemon still runs, answers its own health check `ok`, and an
/// unrelated language (CSS) still answers on it.
async fn assert_daemon_healthy(
    session: &mut Session,
    fixture: &Fixture,
    daemon: &Daemon,
    pid: libc::pid_t,
    fault: &str,
) {
    assert_eq!(daemon.pid(), pid, "{fault}: same daemon");
    assert_eq!(
        parity::health(fixture).await,
        "ok",
        "{fault}: daemon health"
    );
    let css = session
        .call(fixture, "ide.outline", json!({"path":"style.css"}))
        .await;
    assert!(
        css["kind"] == "outline" && css["text"].as_str().unwrap_or_default().contains(".btn"),
        "{fault}: an unrelated language answers on the same daemon: {css}"
    );
}

/// The `module python checker` child, if one runs.
fn checker_module(daemon: &mut Daemon) -> Option<Node> {
    daemon
        .tree()
        .children
        .into_iter()
        .find(|node| node.command.ends_with("module python checker"))
}

/// The transcript of [`calls`] plus the landed problems page on a fresh daemon with `env`, the
/// Pyright-hosting module seen, whether a language server ran as a direct daemon child, and the
/// daemon.
async fn run_transcript(
    fixture: &Fixture,
    env: &[(&str, &str)],
) -> (Vec<String>, Option<Node>, bool, Daemon) {
    let mut daemon = Daemon::start(fixture, env).await;
    let mut session = Session::start(fixture).await;
    let mut replies = Vec::new();
    for (tool, arguments) in calls() {
        let reply = session.call(fixture, tool, arguments.clone()).await;
        replies.push(line_for(fixture, tool, &arguments, &reply));
    }
    replies.push(problems(&mut session, fixture, |_| true).await);
    let module = provider_module(&mut daemon);
    let direct = daemon.tree().direct_child_runs("langserver");
    session.close(fixture).await;
    (replies, module, direct, daemon)
}

/// Replaces `M-011 pilot_python_module_matches_in_process_answers`: Python's outline, symbol,
/// graph, read, context and problems answers are the same in module mode and in process; in
/// module mode one `module python analyzer` child owns the only Pyright process and no language
/// server is a direct daemon child; in process Pyright is a direct child and no module hosts it.
/// Nothing either daemon started survives it.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
async fn python_module_matches_in_process_answers() {
    let fixture = python_fixture("module-parity-cache", None);
    let (in_process, module, direct, mut daemon) = run_transcript(&fixture, &[IN_PROCESS]).await;
    assert!(
        module.is_none() && direct && checker_module(&mut daemon).is_none(),
        "in process: Pyright is a direct daemon child and no module runs"
    );
    let owned = daemon.tree().all();
    drop(daemon);
    let (moduled, module, direct, mut daemon) = run_transcript(&fixture, &[]).await;
    let module = module.expect("a module hosts Pyright");
    assert!(
        matches!(module.children.as_slice(), [(_, command)]
            if command.contains("langserver") && command.ends_with("--stdio")),
        "the analyzer module owns one Pyright process: {module:?}"
    );
    assert!(!direct, "no language server is a direct daemon child");
    let problems = in_process.last().unwrap();
    assert!(
        problems.contains("python: ready") || problems.contains("python: partial"),
        "the in-process check landed:\n{problems}"
    );
    assert!(moduled.iter().any(|reply| reply.contains("mode: semantic")));
    assert!(
        moduled
            .iter()
            .any(|reply| reply.contains("main.py:10  return double(2)"))
    );
    parity::assert_parity(&in_process, &moduled);
    let owned: Vec<ProcessIdentity> = owned.into_iter().chain(daemon.tree().all()).collect();
    drop(daemon);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived its daemon");
    }
}

/// A stand-in for pytest run as `<venv>/bin/python -m pytest` from the worktree root: it imports
/// each selected test file, calls its `test_*` functions and prints pytest's failure lines and
/// summary, deterministically (a fixed duration), so the transcript compares the core's test
/// selection, run and parse in both modes without depending on an installed pytest.
const PYTEST_STAND_IN: &str = r#"import importlib.util, sys
files, failed, passed = [], [], 0
for arg in sys.argv[1:]:
    if arg.startswith("-") or arg == "no:cacheprovider":
        continue
    path = arg.split("::")[0]
    if path not in files:
        files.append(path)
for path in files:
    spec = importlib.util.spec_from_file_location(path.replace("/", "_")[:-3], path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    for name in sorted(dir(module)):
        if name.startswith("test_"):
            try:
                getattr(module, name)()
                passed += 1
            except AssertionError as error:
                failed.append(f"FAILED {path}::{name} - AssertionError: {error}")
print("\n".join(failed))
print(f"=== {len(failed)} failed, {passed} passed in 0.01s ===" if failed else f"=== {passed} passed in 0.01s ===")
sys.exit(1 if failed else 0)
"#;

/// A stand-in for black run as `<venv>/bin/python -m black -q -` (or over files): it strips
/// trailing whitespace from every line, so a formatted edit is visibly formatted.
const BLACK_STAND_IN: &str = r#"import sys
def fmt(text):
    return "".join(line.rstrip() + "\n" for line in text.splitlines())
if "-" in sys.argv[1:]:
    sys.stdout.write(fmt(sys.stdin.read()))
else:
    for path in [a for a in sys.argv[1:] if not a.startswith("-")]:
        with open(path) as f:
            text = f.read()
        with open(path, "w") as f:
            f.write(fmt(text))
"#;

/// [`python_fixture`] plus a test file, black configured as the formatter, and the
/// [`PYTEST_STAND_IN`] and [`BLACK_STAND_IN`] modules, committed.
fn python_edit_fixture(cache: &str) -> Fixture {
    let fixture = python_fixture(cache, None);
    for (path, text) in [
        (
            "pyproject.toml",
            "[project]\nname = \"parity\"\n\n[tool.black]\nline-length = 88\n",
        ),
        (
            "tests/test_helper.py",
            "from helper import double\n\n\ndef test_double():\n    assert double(2) == 4\n",
        ),
        (".gitignore", "__pycache__/\n.venv/\n.venv-alt/\n"),
        ("pytest/__init__.py", ""),
        ("pytest/__main__.py", PYTEST_STAND_IN),
        ("black/__init__.py", ""),
        ("black/__main__.py", BLACK_STAND_IN),
    ] {
        let path = fixture.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    fixture.git(&[
        "add",
        "--",
        ".gitignore",
        "pyproject.toml",
        "tests",
        "pytest",
        "black",
    ]);
    fixture.git(&["commit", "--quiet", "-m", "edit fixture"]);
    // A second environment the transcript selects explicitly over the discovered `.venv`.
    python_venv_named(&fixture.root, ".venv-alt");
    fixture
}

/// The changing calls whose replies must not depend on where Python computes: a start that
/// selects the second environment `.venv-alt` over the discovered `.venv` (its card), a formatted replace, an insert, a test run in the selected
/// environment, a project-wide rename, the read that shows the results and the task diff.
fn edit_calls() -> Vec<(&'static str, Value)> {
    vec![
        (
            "ide.start",
            json!({"activation_id":"parity-start-env","environment":{"python":".venv-alt"}}),
        ),
        (
            "ide.edit",
            json!({"operation_id":"parity-replace","op":"replace","symbol":"helper.py#double",
                "content":"def double(x: int) -> int:   \n    return x + x   \n"}),
        ),
        ("ide.read", json!({"path":"helper.py"})),
        (
            "ide.edit",
            json!({"operation_id":"parity-insert","op":"insert","symbol":"main.py#run","where":"after",
                "content":"def added() -> int:\n    return double(3)\n"}),
        ),
        ("ide.test", json!({"path":"tests/test_helper.py"})),
        (
            "ide.edit",
            json!({"operation_id":"parity-rename","op":"rename","symbol":"helper.py#double","new_name":"twice"}),
        ),
        ("ide.read", json!({"path":"main.py"})),
        ("ide.diff", json!({"mode":"head"})),
    ]
}

/// The transcript of [`edit_calls`] on a fresh [`python_edit_fixture`] and daemon with `env`,
/// with the Pyright-hosting module seen and the daemon.
async fn run_edit_transcript(
    cache: &str,
    env: &[(&str, &str)],
) -> (Vec<String>, Option<Node>, Daemon, Fixture) {
    let fixture = python_edit_fixture(cache);
    let mut daemon = Daemon::start(&fixture, env).await;
    let mut session = Session::start(&fixture).await;
    // Edits wait for a warm server, as an agent would after reading.
    let warm = session
        .call(&fixture, "ide.symbol", json!({"symbol":"helper.py#double"}))
        .await;
    assert_eq!(warm["kind"], "symbol", "{warm}");
    let mut replies = Vec::new();
    for (tool, arguments) in edit_calls() {
        let mut reply = session.call(&fixture, tool, arguments.clone()).await;
        // A test run starts in the background: its settled status is the answer.
        if tool == "ide.test" {
            let deadline = Instant::now() + Duration::from_secs(60);
            while reply["text"]
                .as_str()
                .is_some_and(|text| text.contains(": started") || text.contains(": running"))
            {
                assert!(Instant::now() < deadline, "the test run settles: {reply}");
                tokio::time::sleep(Duration::from_millis(250)).await;
                reply = session
                    .call(&fixture, "ide.test", json!({"status": 1}))
                    .await;
            }
        }
        replies.push(line_for(&fixture, tool, &arguments, &reply));
    }
    // The start card names where Python computes; every other byte must agree.
    let mode = if env.is_empty() {
        "\nmodules: python module"
    } else {
        "\nmodules: python in process (fallback)"
    };
    assert!(replies[0].contains(mode), "{}", replies[0]);
    replies[0] = replies[0].replace(mode, "\nmodules: <mode>");
    let module = provider_module(&mut daemon);
    session.close(&fixture).await;
    (replies, module, daemon, fixture)
}

/// Python's changing answers are the same in module mode and in process, each on its own fresh
/// copy of one committed fixture: an explicitly selected second environment (the start card, and
/// the test run that uses its interpreter), a replace formatted by
/// the project formatter, an insert, a project-wide rename (an edit proposal the core applies),
/// the reads of their results, a pytest run in the selected environment and the task diff. The
/// module run hosts Pyright in a module; nothing either daemon started survives it.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
async fn python_module_edits_match_in_process() {
    let (in_process, module, daemon, fixture) =
        run_edit_transcript("module-edit-parity-cache", &[IN_PROCESS]).await;
    assert!(module.is_none(), "in process no module hosts Pyright");
    drop(daemon);
    drop(fixture);
    let (moduled, module, mut daemon, fixture) =
        run_edit_transcript("module-edit-parity-cache", &[]).await;
    assert!(module.is_some(), "a module hosts Pyright");
    let joined = moduled.join("\n");
    for expected in [
        "def added() -> int:",
        "from helper import twice",
        "tests #1: 1 passed, 0 failed",
        "environment: python .venv-alt",
        "rerun: <root>/.venv-alt/bin/python -m pytest",
    ] {
        assert!(joined.contains(expected), "{expected}:\n{joined}");
    }
    assert!(
        joined.contains("2\t    return x + x\nsource_ref"),
        "the formatter stripped the trailing blanks:\n{joined}"
    );
    parity::assert_parity(&in_process, &moduled);
    let owned = daemon.tree().all();
    drop(daemon);
    drop(fixture);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived its daemon");
    }
}

/// Replaces `M-011 pilot_python_analyzer_faults_are_typed_and_restart`: a stall past the module
/// budget, a malformed reply and a `kill -9` of the Pyright-hosting module in the middle of a
/// call each answer that call with a typed `provider_unavailable` naming
/// `module_unavailable (bundled.python:…)`, and the next call starts a fresh module and answers,
/// on the same daemon; a `kill -9` while idle is noticed before the next call, and a `kill -9`
/// of the Pyright inside an idle module answers the next call typed as the module's provider
/// failure. Neither the failed module nor its Pyright survives.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments and a test-seams build"]
async fn python_analyzer_faults_are_typed_and_restart() {
    let fixture = python_fixture("module-fault-cache", None);
    let usages = json!({"symbol":"helper.py#double"});
    for fault in ["stall", "malformed", "kill", "kill-idle", "kill-pyright"] {
        let flag = fixture.base.join(format!("module-fault-{fault}"));
        let seam = match fault {
            "malformed" => format!("malformed:semantic:{}", flag.display()),
            _ => format!("stall:semantic:{}", flag.display()),
        };
        let budget = if fault == "stall" { "3000" } else { "20000" };
        let mut daemon =
            Daemon::start(&fixture, &[(FAULT_SEAM, &seam), (BUDGET_SEAM, budget)]).await;
        let daemon_pid = daemon.pid();
        let mut session = Session::start(&fixture).await;
        let warm = session
            .call(&fixture, "ide.context", json!({"path":"main.py"}))
            .await;
        assert_eq!(warm["state"], "complete", "{fault}: {warm}");
        let first = provider_module(&mut daemon).expect("the Pyright module runs");
        // Armed only now: the warm-up's own semantic requests (its readiness query included)
        // must not consume the one-time fault meant for the call below.
        std::fs::write(&flag, "").unwrap();
        let started = Instant::now();
        let failed = match fault {
            "kill" => {
                let id = first.id;
                let killer = tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    if id.exists() {
                        // SAFETY: `id` is the exact module identity captured as a child of this
                        // test's daemon a moment ago.
                        unsafe { libc::kill(id.pid, libc::SIGKILL) };
                    }
                });
                let reply = session.call(&fixture, "ide.symbol", usages.clone()).await;
                killer.await.unwrap();
                Some(reply)
            }
            "kill-idle" => {
                // SAFETY: as above, the exact module identity of this test's daemon.
                unsafe { libc::kill(first.id.pid, libc::SIGKILL) };
                assert!(first.id.gone().await, "the killed module is reaped");
                std::fs::remove_file(&flag).unwrap();
                None
            }
            "kill-pyright" => {
                // The module's own Pyright dies while the module idles: the next call that needs
                // it answers typed, naming the module's provider.
                let pyright = first.children[0].0;
                // SAFETY: the exact Pyright identity captured as a child of this test's module.
                unsafe { libc::kill(pyright.pid, libc::SIGKILL) };
                assert!(pyright.gone().await, "the killed Pyright is reaped");
                std::fs::remove_file(&flag).unwrap();
                let reply = session.call(&fixture, "ide.symbol", usages.clone()).await;
                assert!(
                    reply
                        .to_string()
                        .contains("module_unavailable (bundled.python:provider:"),
                    "the refusal names the module's provider: {reply}"
                );
                Some(reply)
            }
            _ => Some(session.call(&fixture, "ide.symbol", usages.clone()).await),
        };
        let failed_after = started.elapsed();
        if let Some(failed) = &failed {
            assert_eq!(failed["code"], "provider_unavailable", "{fault}: {failed}");
        }
        match fault {
            "stall" => assert!(
                failed_after >= Duration::from_millis(3000)
                    && failed_after < Duration::from_secs(15),
                "stall answered after {failed_after:?}"
            ),
            "kill" | "malformed" => assert!(
                failed_after < Duration::from_secs(10),
                "{fault} answered after {failed_after:?}"
            ),
            _ => {}
        }
        let restarted_at = Instant::now();
        let mut recovered = session.call(&fixture, "ide.symbol", usages.clone()).await;
        assert_eq!(recovered["kind"], "symbol", "{fault}: {recovered}");
        while !recovered["text"]
            .as_str()
            .unwrap_or_default()
            .contains("usages: 2 in 1 files")
            && restarted_at.elapsed() < Duration::from_secs(20)
        {
            tokio::time::sleep(Duration::from_millis(200)).await;
            recovered = session.call(&fixture, "ide.symbol", usages.clone()).await;
        }
        assert!(
            recovered["text"]
                .as_str()
                .unwrap_or_default()
                .contains("usages: 2 in 1 files"),
            "{fault}: full usages after the restart: {recovered}"
        );
        eprintln!(
            "module-fault {fault}: refusal after {} ms, full usages after {} ms",
            failed_after.as_millis(),
            restarted_at.elapsed().as_millis()
        );
        let second = provider_module(&mut daemon).expect("a fresh Pyright module runs");
        assert_ne!(
            first.id, second.id,
            "{fault}: the failed module was replaced"
        );
        assert!(first.id.gone().await, "{fault}: the failed module survived");
        for (id, _) in &first.children {
            assert!(
                id.gone().await,
                "{fault}: the failed module's Pyright survived"
            );
        }
        assert_daemon_healthy(&mut session, &fixture, &daemon, daemon_pid, fault).await;
        session.close(&fixture).await;
        drop(daemon);
    }
}

/// Replaces `M-011 pilot_python_checker_faults_are_typed_and_restart`: a stall past the budget,
/// a malformed reply and a `kill -9` of the checker module each leave Python's check
/// `unavailable (fatal)` naming `module_unavailable (bundled.python:…)`, and a run the module
/// widens beyond its recipe is refused by the core (`policy_refused`, nothing runs). After an
/// edit the next check starts a fresh module and lands, on the same daemon.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments and a test-seams build"]
async fn python_checker_faults_are_typed_and_restart() {
    let fixture = python_fixture("module-check-fault-cache", None);
    for (fault, expected) in [
        (
            "stall",
            "module_unavailable (bundled.python:request:timeout)",
        ),
        (
            "malformed",
            "module_unavailable (bundled.python:request:malformed)",
        ),
        ("kill", "module_unavailable (bundled.python:request:exited)"),
        ("widen", "policy_refused"),
    ] {
        let flag = fixture.base.join(format!("module-check-{fault}"));
        std::fs::write(&flag, "").unwrap();
        // Sources no earlier daemon checked: a result recorded for the same inputs would answer
        // without running the faulted check at all.
        std::fs::write(
            fixture.root.join("main.py"),
            format!("def before_{fault}() -> int:\n    return \"bad\"\n"),
        )
        .unwrap();
        let seam = match fault {
            "malformed" => format!("malformed:check_plan:{}", flag.display()),
            "widen" => format!("widen:check_plan:{}", flag.display()),
            _ => format!("stall:check_plan:{}", flag.display()),
        };
        // Only the stall needs a short module budget; a real check of this fixture can take
        // longer than 3 s, which would time the other faults out before they show.
        let budget = if fault == "stall" { "3000" } else { "60000" };
        let mut daemon =
            Daemon::start(&fixture, &[(FAULT_SEAM, &seam), (BUDGET_SEAM, budget)]).await;
        let daemon_pid = daemon.pid();
        let mut session = Session::start(&fixture).await;
        if fault == "kill" {
            let mut first = None;
            let deadline = Instant::now() + Duration::from_secs(10);
            while first.is_none() && Instant::now() < deadline {
                first = checker_module(&mut daemon);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let first = first.expect("the stalled checker module runs");
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(!flag.exists(), "the stall seam fired before the kill");
            // SAFETY: `first` is the exact module identity captured as a child of this test's
            // daemon a moment ago.
            unsafe { libc::kill(first.id.pid, libc::SIGKILL) };
        }
        let failed = problems(&mut session, &fixture, |text| text.contains(expected)).await;
        assert!(
            failed.contains(expected),
            "{fault}: the check names the fault:\n{failed}"
        );
        std::fs::write(
            fixture.root.join("main.py"),
            format!("def edited_{fault}() -> int:\n    return \"bad\"\n"),
        )
        .unwrap();
        let landed = problems(&mut session, &fixture, |text| {
            text.contains("python: ready")
        })
        .await;
        assert!(
            landed.contains("python: ready"),
            "{fault}: the next check lands:\n{landed}"
        );
        assert_daemon_healthy(&mut session, &fixture, &daemon, daemon_pid, fault).await;
        session.close(&fixture).await;
        drop(daemon);
    }
}

/// A Pyright JSON report measured above 2 MiB (and far above the old 8 MiB control-frame shape,
/// as raw attachment bytes) lands through the checker module with exactly its measured error
/// count, and the page equals the in-process one.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
async fn python_check_report_over_two_mib_lands() {
    let mut main = String::from(
        "from helper import double\n\n\ndef run() -> int:\n    return double(2)\n\n\n",
    );
    for index in 0..12_000 {
        main.push_str(&format!(
            "def bad_{index}() -> int:\n    return \"a deliberately long string literal that makes each diagnostic message bigger {index}\"\n\n\n"
        ));
    }
    let fixture = python_fixture("module-large-cache", Some(&main));
    // Checking 12,000 functions takes longer than the small fixtures' 10 s check budget.
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["project_checks"]["check_timeout_s"] = json!(120);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    // The report the checker reads, measured directly with the same CLI and interpreter: well
    // over 2 MiB, with a known nonzero error count both modes must land.
    let report = std::process::Command::new(std::env::var("AGENT_IDE_NODE").unwrap())
        .arg(pyright_cli())
        .args(["--outputjson", "--project"])
        .arg(&fixture.root)
        .arg("--pythonpath")
        .arg(fixture.root.join(".venv/bin/python"))
        .current_dir(&fixture.root)
        .output()
        .unwrap();
    let measured: Value = serde_json::from_slice(&report.stdout).unwrap();
    let errors = measured["summary"]["errorCount"].as_u64().unwrap();
    eprintln!(
        "large report: {} bytes, {errors} errors",
        report.stdout.len()
    );
    assert!(
        report.stdout.len() > 2 * 1024 * 1024,
        "{} bytes",
        report.stdout.len()
    );
    assert!(errors >= 12_000, "{errors} errors");
    let mut pages = Vec::new();
    for env in [&[IN_PROCESS][..], &[]] {
        let daemon = Daemon::start(&fixture, env).await;
        let mut session = Session::start(&fixture).await;
        let page = problems(&mut session, &fixture, |text| {
            text.contains("python: ready")
        })
        .await;
        assert!(
            page.contains(&format!("python: ready; errors: {errors};")),
            "the whole report landed: {page}"
        );
        pages.push(page);
        session.close(&fixture).await;
        drop(daemon);
    }
    parity::assert_parity(&pages[..1], &pages[1..]);
}

/// Resident set size of `id` in KiB, `0` once it is gone.
fn rss_kib(id: ProcessIdentity) -> u64 {
    let listing = std::process::Command::new("/bin/ps")
        .args(["-o", "rss=", "-p", &id.pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&listing.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Milliseconds of one settled call that must answer `kind`.
async fn timed(
    session: &mut Session,
    fixture: &Fixture,
    tool: &str,
    arguments: Value,
    kind: &str,
) -> f64 {
    let started = Instant::now();
    let reply = session.call(fixture, tool, arguments).await;
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(reply["kind"], kind, "{reply}");
    elapsed
}

/// Replaces `M-011 pilot_python_measure` (a measurement, not a contract): for the in-process
/// path and the module it records cold first outlines, warm `ide.outline` and `ide.symbol`
/// latencies and the resident memory of the daemon's process tree, written as JSON to
/// `AGENT_IDE_MODULE_MEASURE_OUT`. Without that variable it measures nothing and passes.
#[tokio::test]
#[ignore = "measurement; requires AGENT_IDE_PYRIGHT, AGENT_IDE_NODE, AGENT_IDE_PYTHON and AGENT_IDE_MODULE_MEASURE_OUT"]
async fn python_module_measure() {
    let Some(out) = std::env::var_os("AGENT_IDE_MODULE_MEASURE_OUT").map(PathBuf::from) else {
        eprintln!("python_module_measure skipped: AGENT_IDE_MODULE_MEASURE_OUT is not set");
        return;
    };
    let fixture = python_fixture("module-measure-cache", None);
    let (cold_runs, warmup, samples) = (15, 20, 200);
    let mut report = serde_json::Map::new();
    for (mode, env) in [("in_process", &[IN_PROCESS][..]), ("module", &[])] {
        let mut daemon = Daemon::start(&fixture, env).await;
        let outline = json!({"path":"main.py"});
        let symbol = json!({"symbol":"helper.py#double"});
        let mut cold = Vec::new();
        let mut session = Session::start(&fixture).await;
        for run in 0..cold_runs {
            if run > 0 {
                session.close(&fixture).await;
                session = Session::start(&fixture).await;
            }
            cold.push(
                timed(
                    &mut session,
                    &fixture,
                    "ide.outline",
                    outline.clone(),
                    "outline",
                )
                .await,
            );
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while !session.call(&fixture, "ide.symbol", symbol.clone()).await["text"]
            .as_str()
            .unwrap_or_default()
            .contains("usages: 2 in 1 files")
        {
            assert!(Instant::now() < deadline, "workspace indexed");
        }
        let mut measured = serde_json::Map::new();
        measured.insert("cold_outline_ms".into(), json!(cold));
        for (name, tool, arguments, kind) in [
            ("warm_outline_ms", "ide.outline", outline.clone(), "outline"),
            ("warm_symbol_ms", "ide.symbol", symbol.clone(), "symbol"),
        ] {
            for _ in 0..warmup {
                timed(&mut session, &fixture, tool, arguments.clone(), kind).await;
            }
            let mut values = Vec::new();
            for _ in 0..samples {
                values.push(timed(&mut session, &fixture, tool, arguments.clone(), kind).await);
            }
            measured.insert(name.into(), json!(values));
        }
        let tree = daemon.tree();
        let mut memory = vec![
            json!({"process":"daemon","rss_kib":rss_kib(ProcessIdentity::of(daemon.pid()).unwrap().0)}),
        ];
        for node in &tree.children {
            memory.push(json!({"process":node.command,"rss_kib":rss_kib(node.id)}));
            for (id, command) in &node.children {
                memory.push(json!({"process":format!("  {command}"),"rss_kib":rss_kib(*id)}));
            }
        }
        measured.insert("rss".into(), json!(memory));
        report.insert(mode.into(), Value::Object(measured));
        session.close(&fixture).await;
        drop(daemon);
    }
    report.insert(
        "method".into(),
        json!({"cold_runs": cold_runs, "warmup": warmup, "calls": samples,
               "clock": "wall time of one settled MCP tools/call round trip measured in the test process"}),
    );
    std::fs::write(
        &out,
        serde_json::to_vec_pretty(&Value::Object(report)).unwrap(),
    )
    .unwrap();
}

/// The Pyright-hosting module shares the supervised slots' restart policy: after the initial
/// start and three restarts within the window, the next call is refused `restart_exhausted`
/// (typed, on the same daemon) instead of starting a fifth module.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
async fn python_analyzer_crash_loop_exhausts_the_restart_budget() {
    let fixture = python_fixture("module-crash-loop-cache", None);
    let mut daemon = Daemon::start(&fixture, &[]).await;
    let daemon_pid = daemon.pid();
    let mut session = Session::start(&fixture).await;
    let usages = json!({"symbol":"helper.py#double"});
    for attempt in 0..4 {
        let reply = session.call(&fixture, "ide.symbol", usages.clone()).await;
        assert_eq!(reply["kind"], "symbol", "attempt {attempt}: {reply}");
        let module = provider_module(&mut daemon).expect("a Pyright module runs");
        // SAFETY: the exact module identity captured as a child of this test's daemon.
        unsafe { libc::kill(module.id.pid, libc::SIGKILL) };
        assert!(module.id.gone().await);
        // Let each backoff (250 ms, 1 s, 4 s) pass so the next start really happens.
        tokio::time::sleep(Duration::from_millis([300, 1100, 4100, 0][attempt])).await;
    }
    let refused = session.call(&fixture, "ide.symbol", usages.clone()).await;
    assert_eq!(refused["code"], "provider_unavailable", "{refused}");
    assert!(
        refused.to_string().contains("restart_exhausted"),
        "the refusal names the exhausted budget: {refused}"
    );
    assert!(
        provider_module(&mut daemon).is_none(),
        "no fifth module started"
    );
    assert_daemon_healthy(&mut session, &fixture, &daemon, daemon_pid, "crash loop").await;
    session.close(&fixture).await;
}
