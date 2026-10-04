//! Contract tests for [`agent_ide::checks::python::PythonChecker`] (T098, EYES-r2 §4 "Python").
//!
//! Every test name is prefixed `python_` per the module task's naming convention. Fixtures live
//! under `tests/fixtures/checks/`; the `errors`/`clean`/`badconfig` JSON fixtures were
//! recorded from a real pinned-pyright run against the sibling fixture source and are replayed
//! here through [`FakeRunner`], never re-invoking pyright. The real-runner (non-fake) end-to-end
//! test is out of scope for this task; it is added at integration (T102).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_ide::checks::python::{PythonChecker, parse_pyright_output, resolve_interpreter};
use agent_ide::checks::runner::{ConfinedRunner, FakeRunner, RunOutput};
use agent_ide::checks::{CheckRequest, CheckState, Checker, Severity, UnavailableReason};

/// Returns the absolute path of one fixture under `tests/fixtures/checks/`.
fn fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/checks")
        .join(relative)
}

/// Creates a fresh, empty temporary directory unique to this process and call, for tests that
/// need a worktree or cache directory the fixture tree does not provide.
fn unique_temp_dir(label: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "agent-ide-python-check-{}-{}-{}",
        std::process::id(),
        label,
        n
    ));
    fs::create_dir_all(&dir).expect("unique temp dir created");
    dir
}

/// Returns the committed placeholder `(node, pyright_cli)` pair used by checker-level tests.
///
/// Neither file is ever executed: [`FakeRunner`] intercepts every run before a process is
/// spawned. Their layout (`.../bin/node`, `.../node_modules/pyright/index.js`) mirrors a real
/// npm-installed toolchain closely enough for `read_roots` assertions to be meaningful, at a
/// fixture depth the execution profile's conservative glob cap can prove (T37B).
fn toolchain_paths() -> (PathBuf, PathBuf) {
    (
        fixture("toolchain/node/bin/node"),
        fixture("toolchain/pyright/node_modules/pyright/index.js"),
    )
}

// -- parse_pyright_output ----------------------------------------------------------------------

#[test]
fn python_parses_clean_project_json_as_ready_with_zero_counts() {
    let stdout = fs::read(fixture("clean/pyright_output.json")).expect("clean fixture readable");
    let snapshot = parse_pyright_output(Some(0), &stdout, b"", 5, 42);
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    assert!(snapshot.problems.is_empty());
    assert_eq!(snapshot.input_generation, 5);
    assert_eq!(snapshot.duration_ms, 42);
}

#[test]
fn python_parses_errors_and_warnings_including_never_opened_file() {
    let dir = fixture("errors");
    let template =
        fs::read_to_string(dir.join("pyright_output.json")).expect("errors fixture readable");
    let stdout = template.replace("{{FIXTURE_DIR}}", &dir.display().to_string());
    let snapshot = parse_pyright_output(Some(1), stdout.as_bytes(), b"", 7, 179);
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(snapshot.errors, 2);
    assert_eq!(snapshot.warnings, 2);
    assert!(!snapshot.truncated);
    // c.py is never "opened" in an editor; the project-wide CLI run must still report it.
    assert!(snapshot.problems.iter().any(|problem| {
        problem.path.ends_with("src/pkg/c.py")
            && problem.severity == Severity::Error
            && problem.code.as_deref() == Some("reportCallIssue")
    }));
    let b_problems: Vec<_> = snapshot
        .problems
        .iter()
        .filter(|problem| problem.path.ends_with("src/pkg/b.py"))
        .collect();
    assert_eq!(b_problems.len(), 3);
}

#[test]
fn python_bad_config_exit_3_is_fatal_regardless_of_json() {
    let stdout =
        fs::read(fixture("badconfig/pyright_output.json")).expect("badconfig fixture readable");
    let stderr = b"error: invalid pyrightconfig.json\n";
    let snapshot = parse_pyright_output(Some(3), &stdout, stderr, 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    // The failure's cause is never dropped: the first `error:` line of stderr becomes the
    // snapshot detail the plate and journal render.
    assert_eq!(
        snapshot.detail.as_deref(),
        Some("error: invalid pyrightconfig.json")
    );
    assert_eq!(snapshot.duration_ms, 1);
}

/// A run that failed with no `error:` line still says why: the first non-empty stderr line
/// (a wrapper refusal such as the nested-sandbox `sandbox_apply` message) stands in, and a run
/// that said nothing at all reports its exit status.
#[test]
fn python_fatal_without_error_line_keeps_the_cause_or_exit_status() {
    let refusal = b"sandbox-exec: sandbox_apply: Operation not permitted\n";
    let snapshot = parse_pyright_output(Some(71), b"", refusal, 1, 11);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(
        snapshot.detail.as_deref(),
        Some("sandbox-exec: sandbox_apply: Operation not permitted")
    );
    assert_eq!(snapshot.duration_ms, 11);

    let silent = parse_pyright_output(Some(71), b"", b"", 1, 11);
    assert_eq!(
        silent.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(silent.detail.as_deref(), Some("exit 71"));
}

#[test]
fn python_unparseable_stdout_is_fatal() {
    let snapshot = parse_pyright_output(Some(0), b"not json", b"", 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

#[test]
fn python_signal_killed_exit_is_fatal() {
    let snapshot = parse_pyright_output(None, b"{}", b"", 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

#[test]
fn python_exit_code_two_is_fatal() {
    let snapshot = parse_pyright_output(Some(2), b"{}", b"", 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

#[test]
fn python_files_analyzed_zero_is_no_files() {
    let json = br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 0}}"#;
    let snapshot = parse_pyright_output(Some(0), json, b"", 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::NoFiles)
    );
    assert_eq!(
        snapshot.detail.as_deref(),
        Some(
            "pyright analyzed 0 files; check \"include\"/\"exclude\" in pyrightconfig.json or \
             [tool.pyright]"
        )
    );
}

#[test]
fn python_count_mismatch_against_summary_is_fatal() {
    let json = br#"{"generalDiagnostics": [], "summary": {"errorCount": 1, "warningCount": 0, "filesAnalyzed": 3}}"#;
    let snapshot = parse_pyright_output(Some(0), json, b"", 1, 1);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
}

// -- resolve_interpreter ------------------------------------------------------------------------

#[test]
fn python_resolve_interpreter_from_pyrightconfig_json() {
    let worktree = fixture("interpreter_pyrightconfig");
    let resolved =
        resolve_interpreter(&worktree).expect("interpreter resolved from pyrightconfig.json");
    assert_eq!(resolved, worktree.join("env/myenv/bin/python"));
}

#[test]
fn python_resolve_interpreter_from_pyproject_toml() {
    let worktree = fixture("interpreter_pyproject");
    let resolved = resolve_interpreter(&worktree)
        .expect("interpreter resolved from pyproject.toml [tool.pyright]");
    assert_eq!(resolved, worktree.join("env/myenv/bin/python"));
}

#[test]
fn python_resolve_interpreter_default_venv() {
    let worktree = fixture("interpreter_venv");
    let resolved = resolve_interpreter(&worktree).expect("interpreter resolved from default .venv");
    assert_eq!(resolved, worktree.join(".venv/bin/python"));
}

#[test]
fn python_resolve_interpreter_none_when_nothing_present() {
    let worktree = unique_temp_dir("resolve-none");
    assert_eq!(resolve_interpreter(&worktree), None);
    let _ = fs::remove_dir_all(&worktree);
}

/// A suffixed environment directory beside the manifest (`.venv-py314`) is discovered exactly
/// like the conventional `.venv`, and `session_interpreter` finds an environment that lives
/// beside a nested root when the worktree root has none.
#[test]
fn python_resolve_interpreter_from_suffixed_and_nested_venv() {
    let worktree = fixture("interpreter_suffixed_venv");
    let resolved = resolve_interpreter(&worktree).expect("suffixed .venv-py314 resolves");
    assert_eq!(resolved, worktree.join(".venv-py314/bin/python"));

    let root = unique_temp_dir("nested-session-interpreter");
    for package in ["alpha", "beta"] {
        fs::create_dir_all(root.join(format!("packages/{package}/src"))).unwrap();
        fs::write(root.join(format!("packages/{package}/pyproject.toml")), "").unwrap();
        fs::write(root.join(format!("packages/{package}/src/lib.py")), "").unwrap();
    }
    let python = root.join("packages/beta/.venv/bin/python");
    fs::create_dir_all(python.parent().unwrap()).unwrap();
    fs::write(&python, "").unwrap();
    assert_eq!(
        agent_ide::checks::python::session_interpreter(&root),
        Some(python),
        "the first nested root with an environment serves the session"
    );
    let _ = fs::remove_dir_all(&root);
}

/// `not_analysed` names a file below no discovered root, and a file whose only covering root has
/// no environment (its pyright run is skipped); a covered root with an environment analyzes it.
#[test]
fn python_not_analysed_names_uncovered_and_environmentless_roots() {
    use agent_ide::checks::LanguageChecks;
    use agent_ide::checks::python::PythonChecks;

    let root = unique_temp_dir("not-analysed");
    for package in ["alpha", "beta"] {
        fs::create_dir_all(root.join(format!("packages/{package}/src"))).unwrap();
        fs::write(root.join(format!("packages/{package}/pyproject.toml")), "").unwrap();
        fs::write(root.join(format!("packages/{package}/src/lib.py")), "").unwrap();
    }
    fs::create_dir_all(root.join("tools")).unwrap();
    fs::write(root.join("tools/one_off.py"), "").unwrap();
    let python = root.join("packages/alpha/.venv/bin/python");
    fs::create_dir_all(python.parent().unwrap()).unwrap();
    fs::write(&python, "").unwrap();

    assert_eq!(
        PythonChecks.not_analysed(&root, &root.join("tools/one_off.py")),
        Some("no Python project root covers this file, so pyright skips it")
    );
    // The edit reply names files worktree-relative; the answer must be the same.
    assert_eq!(
        PythonChecks.not_analysed(&root, Path::new("tools/one_off.py")),
        Some("no Python project root covers this file, so pyright skips it")
    );
    assert_eq!(
        PythonChecks.not_analysed(&root, &root.join("packages/beta/src/lib.py")),
        Some(
            "the Python project root beside this file has no environment; create one or pick it with ide.start environment"
        )
    );
    assert_eq!(
        PythonChecks.not_analysed(&root, &root.join("packages/alpha/src/lib.py")),
        None
    );
    let _ = fs::remove_dir_all(&root);
}

// -- PythonChecker::check / pyright_spec -------------------------------------------------------

#[tokio::test]
async fn python_checker_missing_tool_is_unavailable_tool_missing() {
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::new(Vec::new()));
    let checker = PythonChecker::new(
        runner,
        PathBuf::from("/nonexistent/node"),
        PathBuf::from("/nonexistent/pyright"),
        Duration::from_secs(60),
    );
    let request = CheckRequest {
        worktree: fixture("interpreter_venv"),
        cache_dir: unique_temp_dir("tool-missing"),
        input_generation: 1,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::ToolMissing)
    );
}

#[tokio::test]
async fn python_checker_missing_interpreter_is_unavailable_env_missing() {
    let (node, pyright_cli) = toolchain_paths();
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::new(Vec::new()));
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let worktree = unique_temp_dir("env-missing-worktree");
    let request = CheckRequest {
        worktree: worktree.clone(),
        cache_dir: unique_temp_dir("env-missing-cache"),
        input_generation: 3,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::EnvMissing)
    );
    let _ = fs::remove_dir_all(&worktree);
}

#[tokio::test]
async fn python_checker_runner_error_is_unavailable_fatal() {
    let (node, pyright_cli) = toolchain_paths();
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::new(vec![Err(
        std::io::Error::other("spawn failed"),
    )]));
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let request = CheckRequest {
        worktree: fixture("interpreter_venv"),
        cache_dir: unique_temp_dir("runner-error"),
        input_generation: 4,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    // The runner's io::Error text is the snapshot's cause, so the plate says why instead of
    // "checker supplied no reason".
    assert_eq!(snapshot.detail.as_deref(), Some("spawn failed"));
}

#[tokio::test]
async fn python_checker_timed_out_run_is_unavailable_timeout() {
    let (node, pyright_cli) = toolchain_paths();
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::new(vec![Ok(RunOutput {
        timed_out: true,
        ..RunOutput::default()
    })]));
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let request = CheckRequest {
        worktree: fixture("interpreter_venv"),
        cache_dir: unique_temp_dir("timeout"),
        input_generation: 5,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Timeout)
    );
}

#[tokio::test]
async fn python_checker_ready_snapshot_counts_from_real_fixture_json_with_relative_paths() {
    let (node, pyright_cli) = toolchain_paths();
    let dir = fixture("errors");
    let template = fs::read_to_string(dir.join("pyright_output.json")).expect("errors fixture");
    let stdout = template.replace("{{FIXTURE_DIR}}", &dir.display().to_string());
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::with_stdout(1, stdout.as_bytes()));
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let request = CheckRequest {
        worktree: dir.clone(),
        cache_dir: unique_temp_dir("ready-counts"),
        input_generation: 9,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(snapshot.errors, 2);
    assert_eq!(snapshot.warnings, 2);
    assert!(
        snapshot
            .problems
            .iter()
            .all(|problem| !Path::new(&problem.path).is_absolute()),
        "paths must be rewritten relative to the worktree: {:?}",
        snapshot.problems
    );
    assert!(
        snapshot
            .problems
            .iter()
            .any(|problem| problem.path == "src/pkg/c.py")
    );
}

#[tokio::test]
async fn python_checker_runs_one_pyright_per_nested_root_and_merges() {
    let (node, pyright_cli) = toolchain_paths();
    let worktree = unique_temp_dir("nested-roots");
    for package in ["alpha", "beta"] {
        fs::create_dir_all(worktree.join(format!("packages/{package}/src"))).unwrap();
        fs::write(
            worktree.join(format!("packages/{package}/pyproject.toml")),
            "[project]\n",
        )
        .unwrap();
        fs::write(worktree.join(format!("packages/{package}/src/lib.py")), "").unwrap();
        let python = worktree.join(format!("packages/{package}/.venv/bin/python"));
        fs::create_dir_all(python.parent().unwrap()).unwrap();
        fs::write(python, "").unwrap();
    }
    let clean = br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 1}}"#;
    let error_file = worktree.join("packages/beta/src/lib.py");
    let error_run = format!(
        r#"{{"generalDiagnostics": [{{"file": "{}", "severity": "error", "message": "boom", "range": {{"start": {{"line": 0, "character": 0}}}}}}], "summary": {{"errorCount": 1, "warningCount": 0, "filesAnalyzed": 1}}}}"#,
        error_file.display()
    );
    let fake = Arc::new(FakeRunner::new(vec![
        Ok(RunOutput {
            status: Some(0),
            stdout: clean.to_vec(),
            ..RunOutput::default()
        }),
        Ok(RunOutput {
            status: Some(1),
            stdout: error_run.into_bytes(),
            ..RunOutput::default()
        }),
    ]));
    let runner: Arc<dyn ConfinedRunner> = fake.clone();
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let request = CheckRequest {
        worktree: worktree.clone(),
        cache_dir: unique_temp_dir("nested-roots-cache"),
        input_generation: 11,
        read_denies: Vec::new(),
    };
    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Ready, "{snapshot:?}");
    assert_eq!(snapshot.errors, 1);
    assert_eq!(
        snapshot.problems,
        vec![agent_ide::checks::Problem::new(
            "packages/beta/src/lib.py".into(),
            1,
            1,
            Severity::Error,
            None,
            "boom".into(),
        )],
        "the nested root's own report is relativized and merged"
    );

    let specs = fake.specs();
    assert_eq!(specs.len(), 2, "one pyright run per discovered root");
    let project = |spec: usize| specs[spec].args[3].to_str().unwrap().to_owned();
    let interpreter = |spec: usize| specs[spec].args[5].to_str().unwrap().to_owned();
    assert_eq!(
        project(0),
        worktree.join("packages/alpha").display().to_string()
    );
    assert_eq!(
        project(1),
        worktree.join("packages/beta").display().to_string()
    );
    assert_eq!(
        interpreter(0),
        worktree
            .join("packages/alpha/.venv/bin/python")
            .display()
            .to_string()
    );
    assert_eq!(
        interpreter(1),
        worktree
            .join("packages/beta/.venv/bin/python")
            .display()
            .to_string()
    );
    let _ = fs::remove_dir_all(&worktree);
}

/// Nested roots without any environment stay `EnvMissing`: pyright is never run half-blind, and
/// the durable condition arms the scheduler's skip rule instead of a checker process per hook.
#[tokio::test]
async fn python_checker_nested_roots_without_environments_are_env_missing() {
    let (node, pyright_cli) = toolchain_paths();
    let worktree = unique_temp_dir("nested-no-env");
    fs::create_dir_all(worktree.join("packages/alpha/src")).unwrap();
    fs::write(worktree.join("packages/alpha/pyproject.toml"), "").unwrap();
    fs::write(worktree.join("packages/alpha/src/lib.py"), "").unwrap();
    let runner: Arc<dyn ConfinedRunner> = Arc::new(FakeRunner::new(Vec::new()));
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));
    let snapshot = checker
        .check(CheckRequest {
            worktree: worktree.clone(),
            cache_dir: unique_temp_dir("nested-no-env-cache"),
            input_generation: 12,
            read_denies: Vec::new(),
        })
        .await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::EnvMissing)
    );
    let _ = fs::remove_dir_all(&worktree);
}

#[tokio::test]
async fn python_checker_builds_exact_run_spec_for_default_venv_interpreter() {
    let (node, pyright_cli) = toolchain_paths();
    let ready_json =
        br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 1}}"#;
    let fake = Arc::new(FakeRunner::with_stdout(0, ready_json));
    let runner: Arc<dyn ConfinedRunner> = fake.clone();
    let checker = PythonChecker::new(
        runner,
        node.clone(),
        pyright_cli.clone(),
        Duration::from_secs(120),
    );
    let worktree = fixture("interpreter_venv");
    let cache_dir = unique_temp_dir("run-spec");
    let request = CheckRequest {
        worktree: worktree.clone(),
        cache_dir: cache_dir.clone(),
        input_generation: 2,
        read_denies: Vec::new(),
    };

    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Ready);

    let specs = fake.specs();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];

    let interpreter = worktree.join(".venv/bin/python");
    let canonical_interpreter =
        fs::canonicalize(&interpreter).expect("fixture interpreter canonicalizes");

    assert_eq!(spec.program, node);
    assert_eq!(
        spec.args,
        vec![
            pyright_cli.clone().into_os_string(),
            OsString::from("--outputjson"),
            OsString::from("--project"),
            worktree.clone().into_os_string(),
            OsString::from("--pythonpath"),
            interpreter.clone().into_os_string(),
        ]
    );
    assert_eq!(spec.cwd, worktree);

    let node_bin_dir = node.parent().expect("node has a parent bin dir");
    assert_eq!(
        spec.env,
        vec![
            (
                "PATH".to_string(),
                format!("{}:/usr/bin:/bin", node_bin_dir.display())
            ),
            (
                "HOME".to_string(),
                agent_ide::userhome::user_home()
                    .map(|home| home.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            (
                "TMPDIR".to_string(),
                cache_dir.join("tmp").display().to_string()
            ),
        ]
    );

    let expected_read_roots = vec![
        worktree.clone(),
        node_bin_dir
            .parent()
            .expect("node bin dir has a parent")
            .to_path_buf(),
        pyright_cli
            .parent()
            .expect("pyright cli has a parent")
            .to_path_buf(),
        interpreter
            .parent()
            .and_then(Path::parent)
            .expect("interpreter venv root")
            .to_path_buf(),
        canonical_interpreter
            .parent()
            .and_then(Path::parent)
            .expect("interpreter base prefix")
            .to_path_buf(),
        PathBuf::from("/private/etc"),
    ];
    assert_eq!(spec.read_roots, expected_read_roots);
    assert_eq!(spec.write_roots, vec![cache_dir.clone()]);
    assert_eq!(spec.timeout, Duration::from_secs(120));
    assert_eq!(spec.max_output_bytes, 64 * 1024 * 1024);

    assert!(
        cache_dir.join("tmp").is_dir(),
        "check() must create <cache_dir>/tmp before running"
    );
}

#[tokio::test]
async fn python_checker_uses_pyrightconfig_json_as_project_when_present() {
    let (node, pyright_cli) = toolchain_paths();
    let dir = fixture("errors");
    let template = fs::read_to_string(dir.join("pyright_output.json")).expect("errors fixture");
    let stdout = template.replace("{{FIXTURE_DIR}}", &dir.display().to_string());
    let fake = Arc::new(FakeRunner::with_stdout(1, stdout.as_bytes()));
    let runner: Arc<dyn ConfinedRunner> = fake.clone();
    let checker = PythonChecker::new(runner, node, pyright_cli.clone(), Duration::from_secs(60));
    let cache_dir = unique_temp_dir("project-arg");
    let request = CheckRequest {
        worktree: dir.clone(),
        cache_dir: cache_dir.clone(),
        input_generation: 1,
        read_denies: Vec::new(),
    };
    let interpreter = fixture("interpreter_venv/.venv/bin/python");
    let spec = checker.pyright_spec(&request, &interpreter);
    assert_eq!(spec.args[2], OsString::from("--project"));
    assert_eq!(
        spec.args[3],
        dir.join("pyrightconfig.json").into_os_string()
    );
}

/// A uv-managed venv's `bin/python` is a symlink to a base Python installation elsewhere; T11B
/// requires `--pythonpath` to receive that symlink path unresolved (so Python's own venv
/// detection via `pyvenv.cfg` next to it applies), while `read_roots` still cover both the venv
/// root and the base installation's prefix (needed because pyright execs the interpreter, which
/// under Seatbelt follows the symlink to the real binary and its stdlib).
#[tokio::test]
async fn python_checker_pythonpath_is_the_venv_symlink_not_its_canonical_base() {
    let (node, pyright_cli) = toolchain_paths();
    let ready_json =
        br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 1}}"#;
    let fake = Arc::new(FakeRunner::with_stdout(0, ready_json));
    let runner: Arc<dyn ConfinedRunner> = fake.clone();
    let checker = PythonChecker::new(runner, node, pyright_cli, Duration::from_secs(60));

    let base_root = unique_temp_dir("uv-base");
    let base_bin = base_root.join("bin");
    fs::create_dir_all(&base_bin).expect("base bin dir created");
    let base_python = base_bin.join("python3.14");
    fs::write(&base_python, b"#!/bin/sh\n").expect("base interpreter written");

    let worktree = unique_temp_dir("uv-worktree");
    let venv_bin = worktree.join(".venv").join("bin");
    fs::create_dir_all(&venv_bin).expect("venv bin dir created");
    let venv_python = venv_bin.join("python");
    std::os::unix::fs::symlink(&base_python, &venv_python).expect("venv symlink created");

    let cache_dir = unique_temp_dir("uv-cache");
    let request = CheckRequest {
        worktree: worktree.clone(),
        cache_dir: cache_dir.clone(),
        input_generation: 1,
        read_denies: Vec::new(),
    };
    let interpreter = resolve_interpreter(&worktree).expect("uv venv interpreter resolved");
    assert_eq!(interpreter, venv_python);

    let spec = checker.pyright_spec(&request, &interpreter);

    let pythonpath_index = spec
        .args
        .iter()
        .position(|arg| arg == "--pythonpath")
        .expect("--pythonpath present")
        + 1;
    assert_eq!(
        spec.args[pythonpath_index],
        venv_python.clone().into_os_string()
    );

    let venv_root = worktree.join(".venv");
    let base_prefix = fs::canonicalize(&base_python)
        .expect("base interpreter canonicalizes")
        .parent()
        .and_then(Path::parent)
        .expect("base interpreter has an installation prefix")
        .to_path_buf();
    assert!(
        spec.read_roots.contains(&venv_root),
        "read_roots must contain the venv root: {:?}",
        spec.read_roots
    );
    assert!(
        spec.read_roots.contains(&base_prefix),
        "read_roots must contain the base installation prefix: {:?}",
        spec.read_roots
    );

    let _ = fs::remove_dir_all(&base_root);
    let _ = fs::remove_dir_all(&worktree);
    let _ = fs::remove_dir_all(&cache_dir);
}
