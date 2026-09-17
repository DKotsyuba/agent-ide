//! Rust project checker contract tests: confined run-spec shape, cargo JSON stream parsing,
//! and snapshot state mapping over recorded real cargo streams.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_ide::checks::runner::{FakeRunner, RunOutput};
use agent_ide::checks::rust::{RustChecker, parse_cargo_messages};
use agent_ide::checks::{CheckRequest, CheckState, Checker, Language, Severity, UnavailableReason};

/// Recorded real cargo JSON stream (probe fixture, failing variant): probe-b fails with one
/// E0308 duplicated across its lib and lib-test units, probe-a carries one duplicated warning,
/// the dependent probe-c bin is skipped entirely, and `build-finished.success` is `false`.
const FAILURE_STREAM: &str = include_str!("fixtures/checks/rust/rust_failure.jsonl");

/// Recorded real cargo JSON stream (probe fixture, clean variant): every workspace unit
/// produces a `compiler-artifact`, the single probe-a warning stays duplicated per unit, and
/// `build-finished.success` is `true`.
const CLEAN_STREAM: &str = include_str!("fixtures/checks/rust/rust_clean.jsonl");

/// Recorded real cargo JSON stream (probe fixture, truncated variant): the failing stream cut
/// before the terminal `build-finished` event.
const TRUNCATED_STREAM: &str = include_str!("fixtures/checks/rust/rust_truncated.jsonl");

/// Creates a fresh scratch root for one test under the system temp directory.
///
/// Any leftover from an earlier run of the same process is removed first; tests run
/// single-threaded by contract and remove their own scratch root before returning.
fn rust_scratch(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "agent-ide-project-checks-{}-{tag}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("scratch root creates");
    root
}

/// Builds a check request over `<root>/wt` with input generation 42.
///
/// `with_lockfile` writes an empty `<worktree>/Cargo.lock`; the built spec passes `--locked`
/// either way.
fn rust_request(root: &Path, with_lockfile: bool) -> CheckRequest {
    let worktree = root.join("wt");
    fs::create_dir_all(&worktree).expect("worktree creates");
    if with_lockfile {
        fs::write(worktree.join("Cargo.lock"), b"").expect("lockfile writes");
    }
    CheckRequest {
        worktree,
        cache_dir: root.join("cache"),
        input_generation: 42,
    }
}

/// Builds a checker with a 300 s timeout over `runner`.
///
/// The toolchain directory sits at `<root>/toolchains/tc` with a placeholder `bin/cargo`, so
/// the derived rustup home is `<root>`; no real process ever runs because `FakeRunner` replays
/// scripted outputs. The developer directory is overridden to a scratch directory so spec
/// assertions never depend on whether this machine has Xcode or the Command Line Tools installed.
fn rust_checker(root: &Path, runner: FakeRunner) -> RustChecker {
    let toolchain_dir = rust_toolchain(root, true);
    RustChecker::new(
        Arc::new(runner),
        toolchain_dir,
        None,
        Duration::from_secs(300),
        Some(rust_developer_dir(root)),
    )
}

/// Creates and returns `<root>/developer`, a deterministic stand-in for the Apple developer
/// directory (EYES-r2 §3, T05B). It contains no toolchain layout, so it never yields a linker
/// bypass environment (T06B).
fn rust_developer_dir(root: &Path) -> PathBuf {
    let dir = root.join("developer");
    fs::create_dir_all(&dir).expect("developer dir creates");
    dir
}

/// Creates and returns `<root>/xcode-developer`, a deterministic stand-in for an Xcode developer
/// directory carrying a toolchain `clang`, sibling `clang++`, and the macOS platform SDK (T06B).
fn rust_xcode_developer_dir(root: &Path) -> PathBuf {
    let dir = root.join("xcode-developer");
    let bin_dir = dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin");
    fs::create_dir_all(&bin_dir).expect("xcode clang bin dir creates");
    fs::write(bin_dir.join("clang"), b"placeholder").expect("clang stub writes");
    fs::write(bin_dir.join("clang++"), b"placeholder").expect("clang++ stub writes");
    fs::write(bin_dir.join("ar"), b"placeholder").expect("ar stub writes");
    fs::write(bin_dir.join("ranlib"), b"placeholder").expect("ranlib stub writes");
    let sdk_dir = dir.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk");
    fs::create_dir_all(&sdk_dir).expect("sdk dir creates");
    dir
}

/// Returns the fixed extra Apple developer read roots (`/private/var/db/xcode_select_link`,
/// `/Library/Developer/CommandLineTools`) that are only present when they exist on this real
/// filesystem, mirroring [`RustChecker::cargo_check_spec`]'s own existence check.
fn existing_literal_developer_roots() -> Vec<PathBuf> {
    [
        PathBuf::from("/private/var/db/xcode_select_link"),
        PathBuf::from("/Library/Developer/CommandLineTools"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

/// Creates the toolchain directory `<root>/toolchains/tc`, with a placeholder `bin/cargo` file
/// when `with_cargo` (existence only; the fake runner never executes it).
fn rust_toolchain(root: &Path, with_cargo: bool) -> PathBuf {
    let dir = root.join("toolchains").join("tc");
    fs::create_dir_all(dir.join("bin")).expect("toolchain bin dir creates");
    if with_cargo {
        fs::write(dir.join("bin").join("cargo"), b"placeholder").expect("cargo stub writes");
    }
    dir
}

/// The user home directory, mirroring the checker's `HOME` resolution.
fn rust_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
}

/// Proves the confined spec matches the contract: cargo program, fixed argument list with
/// `--locked` for a lockfile worktree, allowlisted environment, read/write roots, timeout, and
/// the 64 MiB output cap.
#[test]
fn rust_cargo_check_spec_matches_confined_contract() {
    let root = rust_scratch("spec");
    let request = rust_request(&root, true);
    let checker = rust_checker(&root, FakeRunner::default());
    let spec = checker.cargo_check_spec(&request);
    let toolchain = root.join("toolchains").join("tc");
    assert_eq!(spec.program, toolchain.join("bin").join("cargo"));
    assert_eq!(
        spec.args,
        [
            "check",
            "--workspace",
            "--all-targets",
            "--message-format=json",
            "--offline",
            "--keep-going",
            "--locked",
        ]
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    );
    assert_eq!(spec.cwd, request.worktree);
    assert_eq!(
        spec.env,
        vec![
            (
                "PATH".to_owned(),
                format!("{}/bin:/usr/bin:/bin", toolchain.display())
            ),
            (
                "HOME".to_owned(),
                rust_home().to_string_lossy().into_owned()
            ),
            (
                "TMPDIR".to_owned(),
                root.join("cache")
                    .join("tmp")
                    .to_string_lossy()
                    .into_owned()
            ),
            (
                "CARGO_TARGET_DIR".to_owned(),
                root.join("cache")
                    .join("target")
                    .to_string_lossy()
                    .into_owned()
            ),
            ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
        ]
    );
    let mut expected_read_roots = vec![
        request.worktree.clone(),
        toolchain.clone(),
        rust_home().join(".cargo"),
        root.clone(),
        PathBuf::from("/private/etc"),
        rust_developer_dir(&root),
    ];
    expected_read_roots.extend(existing_literal_developer_roots());
    assert_eq!(spec.read_roots, expected_read_roots);
    assert_eq!(spec.write_roots, vec![root.join("cache")]);
    assert_eq!(spec.timeout, Duration::from_secs(300));
    assert_eq!(spec.max_output_bytes, 64 * 1024 * 1024);
    let _ = fs::remove_dir_all(&root);
}

/// Proves `--locked` is passed even when the worktree carries no `Cargo.lock` (EYES-r2 §4).
#[test]
fn rust_cargo_check_spec_passes_locked_without_lockfile() {
    let root = rust_scratch("spec-unlocked");
    let request = rust_request(&root, false);
    let checker = rust_checker(&root, FakeRunner::default());
    let spec = checker.cargo_check_spec(&request);
    let names: Vec<String> = spec
        .args
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        vec![
            "check",
            "--workspace",
            "--all-targets",
            "--message-format=json",
            "--offline",
            "--keep-going",
            "--locked",
        ]
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves an explicit cargo home overrides the default read root.
#[test]
fn rust_cargo_check_spec_honors_explicit_cargo_home() {
    let root = rust_scratch("spec-cargo-home");
    let request = rust_request(&root, true);
    let toolchain_dir = root.join("standalone-tc");
    fs::create_dir_all(&toolchain_dir).expect("toolchain dir creates");
    let cargo_home = root.join("custom-cargo");
    fs::create_dir_all(&cargo_home).expect("cargo home creates");
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        toolchain_dir.clone(),
        Some(cargo_home.clone()),
        Duration::from_secs(300),
        Some(rust_developer_dir(&root)),
    );
    let spec = checker.cargo_check_spec(&request);
    assert_eq!(spec.read_roots[2], cargo_home);
    let _ = fs::remove_dir_all(&root);
}

/// Proves the rustup home read root falls back to `$HOME/.rustup` when the toolchain directory
/// has no `toolchains` ancestor.
#[test]
fn rust_cargo_check_spec_falls_back_to_home_rustup() {
    let root = rust_scratch("spec-rustup-fallback");
    let request = rust_request(&root, true);
    let toolchain_dir = root.join("standalone-tc");
    fs::create_dir_all(&toolchain_dir).expect("toolchain dir creates");
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        toolchain_dir.clone(),
        None,
        Duration::from_secs(300),
        Some(rust_developer_dir(&root)),
    );
    let spec = checker.cargo_check_spec(&request);
    let mut expected_read_roots = vec![
        request.worktree.clone(),
        toolchain_dir.clone(),
        rust_home().join(".cargo"),
        rust_home().join(".rustup"),
        PathBuf::from("/private/etc"),
        rust_developer_dir(&root),
    ];
    expected_read_roots.extend(existing_literal_developer_roots());
    assert_eq!(spec.read_roots, expected_read_roots);
    let _ = fs::remove_dir_all(&root);
}

/// Proves the checker reports the Rust language.
#[test]
fn rust_checker_reports_rust_language() {
    let root = rust_scratch("language");
    let checker = rust_checker(&root, FakeRunner::default());
    assert_eq!(checker.language(), Language::Rust);
    let _ = fs::remove_dir_all(&root);
}

/// Proves a toolchain without `bin/cargo` yields `Unavailable(ToolMissing)` before any process
/// is started.
#[tokio::test]
async fn rust_check_reports_tool_missing_without_cargo_binary() {
    let root = rust_scratch("tool-missing");
    let request = rust_request(&root, true);
    let runner = FakeRunner::default();
    let checker = RustChecker::new(
        Arc::new(runner.clone()),
        rust_toolchain(&root, false),
        None,
        Duration::from_secs(300),
        Some(rust_developer_dir(&root)),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::ToolMissing)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    assert!(snapshot.problems.is_empty());
    assert_eq!(snapshot.input_generation, 42);
    assert_eq!(snapshot.duration_ms, 0);
    assert!(runner.specs().is_empty());
    let _ = fs::remove_dir_all(&root);
}

/// Proves the failing recorded stream maps to `Partial` with lib/lib-test duplicates collapsed,
/// exact problem content, cache subdirectories created before the run, and the generation
/// fenced through.
#[tokio::test]
async fn rust_check_failure_stream_is_partial_with_deduped_problems() {
    let root = rust_scratch("failure");
    let request = rust_request(&root, true);
    let checker = rust_checker(
        &root,
        FakeRunner::with_stdout(101, FAILURE_STREAM.as_bytes()),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Partial);
    assert_eq!(snapshot.errors, 1);
    assert_eq!(snapshot.warnings, 1);
    assert_eq!(snapshot.input_generation, 42);
    assert!(!snapshot.truncated);
    assert_eq!(snapshot.problems.len(), 2);
    let error = &snapshot.problems[0];
    assert_eq!(error.path, "crates/b/src/broken.rs");
    assert_eq!(error.line, 3);
    assert_eq!(error.column, 18);
    assert_eq!(error.severity, Severity::Error);
    assert_eq!(error.code.as_deref(), Some("E0308"));
    assert_eq!(error.message, "mismatched types");
    let warning = &snapshot.problems[1];
    assert_eq!(warning.path, "crates/a/src/lib.rs");
    assert_eq!(warning.line, 6);
    assert_eq!(warning.column, 9);
    assert_eq!(warning.severity, Severity::Warning);
    assert_eq!(warning.code.as_deref(), Some("unused_variables"));
    assert_eq!(warning.message, "unused variable: `unused_variable`");
    assert!(root.join("cache").join("tmp").is_dir());
    assert!(root.join("cache").join("target").is_dir());
    let _ = fs::remove_dir_all(&root);
}

/// Proves the clean recorded stream maps to `Ready` with the duplicated warning collapsed to
/// one.
#[tokio::test]
async fn rust_check_clean_stream_is_ready_with_deduped_warning() {
    let root = rust_scratch("clean");
    let request = rust_request(&root, true);
    let checker = rust_checker(&root, FakeRunner::with_stdout(0, CLEAN_STREAM.as_bytes()));
    let snapshot = checker.check(request).await;
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 1);
    assert_eq!(snapshot.problems.len(), 1);
    assert_eq!(snapshot.problems[0].path, "crates/a/src/lib.rs");
    assert_eq!(snapshot.problems[0].line, 6);
    assert_eq!(snapshot.problems[0].column, 9);
    assert!(!snapshot.truncated);
    let _ = fs::remove_dir_all(&root);
}

/// Proves the pure parser maps the failing stream to `Partial`, passing the generation and
/// duration through verbatim.
#[test]
fn rust_parser_failure_stream_maps_partial_with_dedup() {
    let snapshot = parse_cargo_messages(FAILURE_STREAM.as_bytes(), &[], 7, 1234);
    assert_eq!(snapshot.state, CheckState::Partial);
    assert_eq!(snapshot.errors, 1);
    assert_eq!(snapshot.warnings, 1);
    assert_eq!(snapshot.input_generation, 7);
    assert_eq!(snapshot.duration_ms, 1234);
}

/// Proves the pure parser maps the clean stream to `Ready` with the duplicated warning
/// collapsed.
#[test]
fn rust_parser_clean_stream_maps_ready() {
    let snapshot = parse_cargo_messages(CLEAN_STREAM.as_bytes(), &[], 8, 20);
    assert_eq!(snapshot.state, CheckState::Ready);
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 1);
    assert_eq!(snapshot.problems.len(), 1);
}

/// Proves a `build-finished.success: false` stream with zero error-level diagnostics is
/// `Unavailable(Fatal)`, never fabricated as `Ready` (T05B): a build-script failure such as
/// `blake3`'s produces exactly this shape, and the first `error:` line of stderr becomes the
/// snapshot's detail.
#[test]
fn rust_parser_failed_build_with_no_errors_is_fatal_with_stderr_detail() {
    let stream = br#"{"reason":"build-finished","success":false}"#;
    let stderr = b"Compiling blake3 v1.5.0\nerror: failed to run custom build command for `blake3 v1.5.0`\n\nCaused by:\n  process didn't exit successfully\n";
    let snapshot = parse_cargo_messages(stream, stderr, 11, 99);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    assert!(snapshot.problems.is_empty());
    assert_eq!(snapshot.duration_ms, 0);
    assert_eq!(
        snapshot.detail.as_deref(),
        Some("error: failed to run custom build command for `blake3 v1.5.0`")
    );
}

/// Proves the same failed-build-with-no-errors shape without any `error:` line in stderr still
/// reports `Unavailable(Fatal)`, with no detail rather than a guessed one.
#[test]
fn rust_parser_failed_build_with_no_errors_and_no_stderr_has_no_detail() {
    let stream = br#"{"reason":"build-finished","success":false}"#;
    let snapshot = parse_cargo_messages(stream, b"", 11, 99);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.detail, None);
}

/// Proves a stderr `error:` line longer than 160 bytes is truncated on a UTF-8 boundary.
#[test]
fn rust_parser_failed_build_detail_is_truncated_to_160_bytes() {
    let stream = br#"{"reason":"build-finished","success":false}"#;
    let long_suffix = "x".repeat(200);
    let stderr = format!("error: {long_suffix}\n");
    let snapshot = parse_cargo_messages(stream, stderr.as_bytes(), 1, 1);
    let detail = snapshot.detail.expect("detail present");
    assert!(detail.len() <= 160, "{}", detail.len());
    assert!(stderr.starts_with(&detail));
}

/// Proves a failed-build-with-no-errors stream whose only diagnostic is a spanless rustc `error`
/// message (T06B: a linker failure such as `cc` exiting nonzero has no primary span, so it is
/// never counted by [`parse_cargo_messages`]) reports that message as the snapshot detail in
/// preference to cargo's own stderr summary line.
#[test]
fn rust_parser_prefers_spanless_compiler_error_message_over_stderr() {
    let stream = concat!(
        r#"{"reason":"compiler-message","package_id":"pastey 0.2.3","message":{"level":"error","message":"linking with `cc` failed: exit status: 71","spans":[]}}"#,
        "\n",
        r#"{"reason":"build-finished","success":false}"#,
    );
    let stderr = b"error: could not compile `pastey` (build script)\n";
    let snapshot = parse_cargo_messages(stream.as_bytes(), stderr, 3, 42);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert!(snapshot.problems.is_empty());
    assert_eq!(
        snapshot.detail.as_deref(),
        Some("linking with `cc` failed: exit status: 71")
    );
}

/// Proves a stream without the terminal `build-finished` event is `Unavailable(Fatal)` with
/// zero counts.
#[test]
fn rust_parser_stream_without_build_finished_is_fatal() {
    let snapshot = parse_cargo_messages(TRUNCATED_STREAM.as_bytes(), &[], 9, 55);
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    assert!(snapshot.problems.is_empty());
    // The sanctioned zero-count constructor pins duration to 0, like every unavailable outcome.
    assert_eq!(snapshot.duration_ms, 0);
}

/// Proves a runner-level failure maps to `Unavailable(Fatal)`.
#[tokio::test]
async fn rust_check_runner_error_is_fatal() {
    let root = rust_scratch("runner-error");
    let request = rust_request(&root, true);
    let checker = rust_checker(
        &root,
        FakeRunner::new(vec![Err(io::Error::other("spawn refused"))]),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    let _ = fs::remove_dir_all(&root);
}

/// Proves a timed-out run maps to `Unavailable(Timeout)`.
#[tokio::test]
async fn rust_check_timed_out_run_is_timeout() {
    let root = rust_scratch("timeout");
    let request = rust_request(&root, true);
    let checker = rust_checker(
        &root,
        FakeRunner::new(vec![Ok(RunOutput {
            timed_out: true,
            ..RunOutput::default()
        })]),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Timeout)
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves a truncated output stream maps to `Unavailable(Fatal)` even when the captured bytes
/// would parse.
#[tokio::test]
async fn rust_check_truncated_output_is_fatal() {
    let root = rust_scratch("truncated-output");
    let request = rust_request(&root, true);
    let checker = rust_checker(
        &root,
        FakeRunner::new(vec![Ok(RunOutput {
            status: Some(0),
            stdout: FAILURE_STREAM.as_bytes().to_vec(),
            truncated: true,
            ..RunOutput::default()
        })]),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves cargo lockfile refusals and `Cargo.lock` write failures map to `EnvMissing` while
/// unrelated stderr does not mask a partial parse.
#[tokio::test]
async fn rust_check_lockfile_stderr_is_env_missing() {
    let refusal_root = rust_scratch("env-missing-refusal");
    let refusal_request = rust_request(&refusal_root, true);
    let refusal_checker = rust_checker(
        &refusal_root,
        FakeRunner::new(vec![Ok(RunOutput {
            status: Some(101),
            stderr: b"error: the lock file /wt/Cargo.lock needs to be updated but --locked was passed to prevent this"
                .to_vec(),
            ..RunOutput::default()
        })]),
    );
    let snapshot = refusal_checker.check(refusal_request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::EnvMissing)
    );
    let _ = fs::remove_dir_all(&refusal_root);

    let write_root = rust_scratch("env-missing-write");
    let write_request = rust_request(&write_root, true);
    let write_checker = rust_checker(
        &write_root,
        FakeRunner::new(vec![Ok(RunOutput {
            status: Some(101),
            stderr: b"error: failed to write /wt/Cargo.lock".to_vec(),
            ..RunOutput::default()
        })]),
    );
    let snapshot = write_checker.check(write_request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::EnvMissing)
    );
    let _ = fs::remove_dir_all(&write_root);

    let control_root = rust_scratch("env-missing-control");
    let control_request = rust_request(&control_root, true);
    let control_checker = rust_checker(
        &control_root,
        FakeRunner::new(vec![Ok(RunOutput {
            status: Some(101),
            stdout: FAILURE_STREAM.as_bytes().to_vec(),
            stderr: b"warning: build failed, waiting for other jobs to finish...".to_vec(),
            ..RunOutput::default()
        })]),
    );
    let snapshot = control_checker.check(control_request).await;
    assert_eq!(snapshot.state, CheckState::Partial);
    let _ = fs::remove_dir_all(&control_root);
}

/// Proves a completed run whose build failed with no error-level diagnostics is
/// `Unavailable(Fatal)` end to end (T05B), never fabricated as `Ready`, and carries the first
/// `error:` line of stderr as the snapshot detail.
#[tokio::test]
async fn rust_check_failed_build_with_no_errors_is_fatal_not_ready() {
    let root = rust_scratch("build-failed-no-errors");
    let request = rust_request(&root, true);
    let checker = rust_checker(
        &root,
        FakeRunner::new(vec![Ok(RunOutput {
            status: Some(101),
            stdout: br#"{"reason":"build-finished","success":false}"#.to_vec(),
            stderr: b"error: failed to run custom build command for `blake3 v1.5.0`\n".to_vec(),
            ..RunOutput::default()
        })]),
    );
    let snapshot = checker.check(request).await;
    assert_eq!(
        snapshot.state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );
    assert_eq!(snapshot.errors, 0);
    assert_eq!(snapshot.warnings, 0);
    assert_eq!(
        snapshot.detail.as_deref(),
        Some("error: failed to run custom build command for `blake3 v1.5.0`")
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves the confined spec's read roots include the configured developer directory override
/// (T05B, EYES-r2 §3), so a build script's `cc` invocation can resolve through it.
#[test]
fn rust_cargo_check_spec_includes_configured_developer_dir() {
    let root = rust_scratch("developer-dir-configured");
    let request = rust_request(&root, true);
    let developer_dir = root.join("custom-xcode");
    fs::create_dir_all(&developer_dir).expect("developer dir creates");
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        rust_toolchain(&root, true),
        None,
        Duration::from_secs(300),
        Some(developer_dir.clone()),
    );
    let spec = checker.cargo_check_spec(&request);
    assert!(
        spec.read_roots.contains(&developer_dir),
        "{:?}",
        spec.read_roots
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves a `developer_dir` override that does not exist on disk is not trusted verbatim: the
/// checker falls back to its own resolution instead of admitting a nonexistent read root.
#[test]
fn rust_cargo_check_spec_ignores_nonexistent_developer_dir_override() {
    let root = rust_scratch("developer-dir-missing-override");
    let request = rust_request(&root, true);
    let bogus_developer_dir = root.join("does-not-exist");
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        rust_toolchain(&root, true),
        None,
        Duration::from_secs(300),
        Some(bogus_developer_dir.clone()),
    );
    let spec = checker.cargo_check_spec(&request);
    assert!(
        !spec.read_roots.contains(&bogus_developer_dir),
        "{:?}",
        spec.read_roots
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves the confined spec sets the linker-bypass environment (T06B) when the developer
/// directory carries an Xcode toolchain clang: `CARGO_TARGET_<TRIPLE>_LINKER` derived from the
/// toolchain directory name, `CC`, `CXX` and `SDKROOT` all point at the resolved Xcode paths, so
/// a build script's link step never reaches the `/usr/bin/cc` `xcrun` shim.
#[test]
fn rust_cargo_check_spec_sets_linker_env_when_xcode_clang_present() {
    let root = rust_scratch("linker-env-xcode");
    let request = rust_request(&root, true);
    let developer_dir = rust_xcode_developer_dir(&root);
    let toolchain_dir = root.join("toolchains").join("1.98.1-aarch64-apple-darwin");
    fs::create_dir_all(toolchain_dir.join("bin")).expect("toolchain bin dir creates");
    fs::write(toolchain_dir.join("bin").join("cargo"), b"placeholder").expect("cargo stub writes");
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        toolchain_dir,
        None,
        Duration::from_secs(300),
        Some(developer_dir.clone()),
    );
    let spec = checker.cargo_check_spec(&request);
    let clang = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/clang");
    let clangxx = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/clang++");
    let ar = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/ar");
    let ranlib = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/ranlib");
    let sdk = developer_dir.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk");
    let env: std::collections::HashMap<_, _> = spec.env.into_iter().collect();
    assert_eq!(
        env.get("CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER"),
        Some(&clang.to_string_lossy().into_owned())
    );
    assert_eq!(env.get("CC"), Some(&clang.to_string_lossy().into_owned()));
    assert_eq!(
        env.get("CXX"),
        Some(&clangxx.to_string_lossy().into_owned())
    );
    assert_eq!(env.get("AR"), Some(&ar.to_string_lossy().into_owned()));
    assert_eq!(
        env.get("RANLIB"),
        Some(&ranlib.to_string_lossy().into_owned())
    );
    assert_eq!(
        env.get("SDKROOT"),
        Some(&sdk.to_string_lossy().into_owned())
    );
    let _ = fs::remove_dir_all(&root);
}

/// Proves the confined spec falls back to `RUSTFLAGS=-Clinker=<clang>` (T06B) when a clang is
/// found but the toolchain directory's own name carries no recognizable target triple.
#[test]
fn rust_cargo_check_spec_falls_back_to_rustflags_when_triple_unknown() {
    let root = rust_scratch("linker-env-unknown-triple");
    let request = rust_request(&root, true);
    let developer_dir = rust_xcode_developer_dir(&root);
    let checker = RustChecker::new(
        Arc::new(FakeRunner::default()),
        rust_toolchain(&root, true),
        None,
        Duration::from_secs(300),
        Some(developer_dir.clone()),
    );
    let spec = checker.cargo_check_spec(&request);
    let clang = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/clang");
    let env: std::collections::HashMap<_, _> = spec.env.into_iter().collect();
    assert_eq!(
        env.get("RUSTFLAGS"),
        Some(&format!("-Clinker={}", clang.display()))
    );
    assert!(
        !env.keys().any(|key| key.ends_with("_LINKER")),
        "{:?}",
        env.keys().collect::<Vec<_>>()
    );
    let _ = fs::remove_dir_all(&root);
}
