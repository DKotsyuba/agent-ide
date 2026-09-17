//! Confined Rust project checks through `cargo check`.
//!
//! The checker turns one [`CheckRequest`] into a confined `cargo check --workspace
//! --all-targets --message-format=json` run ([`RustChecker::cargo_check_spec`]), executes it
//! through the injected [`ConfinedRunner`] seam, and maps the JSON event stream plus the run
//! outcome onto one [`ProblemSnapshot`] ([`RustChecker::check`]). Diagnostics are counted from
//! the primary span of `error`/`warning` compiler messages, deduplicated by the shared snapshot
//! builder, and the state heuristic reflects cargo's completion marker and per-package coverage
//! ([`parse_cargo_messages`]). No process is started outside the runner seam; the real Seatbelt
//! runner is wired separately from `crate::execution`.

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::runner::{ConfinedRunner, RunOutput, RunSpec};
use super::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, Problem, ProblemSnapshot, Severity,
    UnavailableReason,
};

/// Per-stream capture limit for one confined cargo run: 64 MiB.
///
/// A larger stream is truncated by the runner and reported as [`UnavailableReason::Fatal`], so a
/// hostile or runaway build can neither wedge the daemon nor silently drop diagnostics.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// System configuration read root shared by every confined check process.
const ETC_READ_ROOT: &str = "/private/etc";

/// Runs confined `cargo check` for one worktree and maps the result to a snapshot.
///
/// The checker owns only configuration and the runner seam; every per-run value (worktree,
/// cache, generation) arrives in the [`CheckRequest`]. Instances are `Send + Sync` and may be
/// shared by concurrent scheduler tasks.
pub struct RustChecker {
    /// Executes the confined cargo process for every [`RustChecker::check`] call.
    runner: Arc<dyn ConfinedRunner>,
    /// Rust toolchain root providing `<dir>/bin/cargo`; missing cargo is [`UnavailableReason::ToolMissing`].
    toolchain_dir: PathBuf,
    /// Explicit cargo home override; `None` derives `$HOME/.cargo`.
    cargo_home: Option<PathBuf>,
    /// Wall-clock limit handed to the runner for each cargo run.
    timeout: Duration,
}

impl RustChecker {
    /// Builds a checker.
    ///
    /// `toolchain_dir` is the toolchain root whose `bin/cargo` is executed; when the binary is
    /// missing, [`RustChecker::check`] reports [`UnavailableReason::ToolMissing`]. `cargo_home`
    /// overrides the cargo home read root; `None` means `$HOME/.cargo`. `timeout` bounds each
    /// cargo run; expiry yields [`UnavailableReason::Timeout`] (the runner kills the group).
    pub fn new(
        runner: Arc<dyn ConfinedRunner>,
        toolchain_dir: PathBuf,
        cargo_home: Option<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            toolchain_dir,
            cargo_home,
            timeout,
        }
    }

    /// Builds the exact confined cargo invocation for `request`.
    ///
    /// Program is `<toolchain_dir>/bin/cargo` with `check --workspace --all-targets
    /// --message-format=json --offline --keep-going` in the worktree, plus `--locked` exactly
    /// when `<worktree>/Cargo.lock` exists (the lockfile must exist to be enforceable; without
    /// it cargo would re-resolve the dependency graph on every check). The environment is
    /// rebuilt from the allowlist: `PATH` limited to toolchain bins plus the system dirs,
    /// `HOME`, `TMPDIR`/`CARGO_TARGET_DIR` under the private cache, and `CARGO_NET_OFFLINE=true`.
    /// Read roots cover the worktree, the toolchain, the cargo home, the derived rustup home and
    /// `/private/etc`; the private cache is the only write root; each output stream is capped at
    /// [`MAX_OUTPUT_BYTES`]. The construction is pure with respect to the process environment:
    /// its only inputs are the checker configuration and `request` (including the on-disk
    /// `Cargo.lock` presence in the request's worktree).
    pub fn cargo_check_spec(&self, request: &CheckRequest) -> RunSpec {
        let home = real_home();
        let cargo_home = self.effective_cargo_home(&home);
        let rustup_home = derived_rustup_home(&self.toolchain_dir, &home);
        let mut args: Vec<OsString> = vec![
            "check",
            "--workspace",
            "--all-targets",
            "--message-format=json",
            "--offline",
            "--keep-going",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        if request.worktree.join("Cargo.lock").is_file() {
            args.push(OsString::from("--locked"));
        }
        RunSpec {
            program: self.toolchain_dir.join("bin").join("cargo"),
            args,
            cwd: request.worktree.clone(),
            env: vec![
                (
                    "PATH".to_owned(),
                    format!("{}/bin:/usr/bin:/bin", self.toolchain_dir.display()),
                ),
                ("HOME".to_owned(), home.to_string_lossy().into_owned()),
                (
                    "TMPDIR".to_owned(),
                    request.cache_dir.join("tmp").to_string_lossy().into_owned(),
                ),
                (
                    "CARGO_TARGET_DIR".to_owned(),
                    request
                        .cache_dir
                        .join("target")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("CARGO_NET_OFFLINE".to_owned(), "true".to_owned()),
            ],
            read_roots: vec![
                request.worktree.clone(),
                self.toolchain_dir.clone(),
                cargo_home,
                rustup_home,
                PathBuf::from(ETC_READ_ROOT),
            ],
            write_roots: vec![request.cache_dir.clone()],
            timeout: self.timeout,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }

    /// Returns the effective cargo home: the override, else `$HOME/.cargo`.
    ///
    /// `home` is injected so the derivation is a pure function of configuration plus the home
    /// dir, keeping it testable without process environment mutation.
    fn effective_cargo_home(&self, home: &Path) -> PathBuf {
        self.cargo_home
            .clone()
            .unwrap_or_else(|| home.join(".cargo"))
    }
}

/// Returns the real user home directory.
///
/// Reads `HOME` from the process environment; when the variable is unset (not expected on the
/// supported macOS target) the system temp dir is substituted so path construction stays
/// absolute.
fn real_home() -> PathBuf {
    env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::temp_dir())
}

/// Derives the rustup home from the toolchain directory.
///
/// A rustup-managed toolchain directory looks like `<rustup home>/toolchains/<name>`; walking
/// the ancestors for a `toolchains` component therefore yields the owning rustup home. When no
/// such ancestor exists (a standalone toolchain outside rustup), the default `$HOME/.rustup` is
/// returned so the read root still covers the standard cargo/rustup bookkeeping paths.
fn derived_rustup_home(toolchain_dir: &Path, home: &Path) -> PathBuf {
    let mut current = toolchain_dir;
    while let Some(parent) = current.parent() {
        if current
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new("toolchains"))
        {
            return parent.to_path_buf();
        }
        current = parent;
    }
    home.join(".rustup")
}

impl Checker for RustChecker {
    fn language(&self) -> Language {
        Language::Rust
    }

    /// Runs one confined `cargo check` and maps it to a snapshot.
    ///
    /// Missing cargo reports [`UnavailableReason::ToolMissing`] before any process starts. The
    /// cache `tmp` and `target` subdirectories are created inside the private cache before the
    /// run so the confined process never needs to create them. Runner failure and output
    /// truncation report [`UnavailableReason::Fatal`], expiry reports
    /// [`UnavailableReason::Timeout`], and cargo stderr admitting the lockfile cannot be updated
    /// or written (the worktree is read-only for checks) reports
    /// [`UnavailableReason::EnvMissing`]. Every other completed run is parsed by
    /// [`parse_cargo_messages`]. Dropping the future cancels the confined process through the
    /// runner seam.
    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(self.run_check(request))
    }
}

impl RustChecker {
    /// Executes the check body behind [`RustChecker::check`].
    ///
    /// Split from the trait method only to keep the boxed future type uniform.
    async fn run_check(&self, request: CheckRequest) -> ProblemSnapshot {
        let started = Instant::now();
        let cargo = self.toolchain_dir.join("bin").join("cargo");
        if !cargo.is_file() {
            return ProblemSnapshot::unavailable(
                Language::Rust,
                UnavailableReason::ToolMissing,
                request.input_generation,
            );
        }
        let cache_tmp = request.cache_dir.join("tmp");
        let cache_target = request.cache_dir.join("target");
        if fs::create_dir_all(&cache_tmp).is_err() || fs::create_dir_all(&cache_target).is_err() {
            return ProblemSnapshot::unavailable(
                Language::Rust,
                UnavailableReason::Fatal,
                request.input_generation,
            );
        }
        let spec = self.cargo_check_spec(&request);
        match self.runner.run(spec).await {
            Err(_) => ProblemSnapshot::unavailable(
                Language::Rust,
                UnavailableReason::Fatal,
                request.input_generation,
            ),
            Ok(output) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                map_run_output(&request, &output, duration_ms)
            }
        }
    }
}

/// Maps one completed confined run to a snapshot.
///
/// Precedence: timeout, then truncation, then the cargo lockfile failure, then stream parsing.
/// The generation is fenced to the triggering request; the duration measures the confined run
/// (checker-side setup excluded).
fn map_run_output(request: &CheckRequest, output: &RunOutput, duration_ms: u64) -> ProblemSnapshot {
    if output.timed_out {
        return ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::Timeout,
            request.input_generation,
        );
    }
    if output.truncated {
        return ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::Fatal,
            request.input_generation,
        );
    }
    if lockfile_write_failure(&output.stderr) {
        return ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::EnvMissing,
            request.input_generation,
        );
    }
    parse_cargo_messages(&output.stdout, request.input_generation, duration_ms)
}

/// Reports whether cargo stderr says the lockfile could not be updated or written.
///
/// Matches cargo's `--locked` refusal — the shipped wording embeds the absolute lockfile path
/// between "lock file" and "needs to be updated", so the fragments are matched separately
/// alongside the exact contract phrase — and plain `Cargo.lock` write failures.
fn lockfile_write_failure(stderr: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_lowercase();
    text.contains("lock file needs to be updated")
        || (text.contains("cargo.lock") && text.contains("needs to be updated"))
        || (text.contains("cargo.lock") && text.contains("failed to write"))
}

/// Parses a raw `cargo --message-format=json` event stream into a snapshot.
///
/// The stream is a sequence of JSON events, one per line. Counted diagnostics are
/// `compiler-message` events whose `message.level` is exactly `error` or `warning`; each is
/// located by its primary span (`is_primary`), coded by `message.code.code`, and carries the
/// short `message.message` text. Cargo's summary diagnostics (`aborting due to …`,
/// `… warning(s) emitted`, `could not compile …`) and messages without a primary span are
/// ignored, as are lines that are not valid JSON events (a bounded hostile stream therefore
/// degrades to a zero count instead of a parse failure).
///
/// Deduplication, the 500-problem cap and the feed sort order are delegated to
/// [`ProblemSnapshot::from_problems`]; `--all-targets` repeats a diagnostic once per lib and
/// lib-test unit, and the repetition collapses there.
///
/// State heuristic (EYES-r2 §4): a missing terminal `build-finished` event means cargo never
/// completed the run, which is [`UnavailableReason::Fatal`]. Otherwise the snapshot is
/// [`CheckState::Partial`] exactly when `build-finished.success` is `false`, at least one
/// deduplicated error exists, and at least one package that produced `compiler-message` events
/// produced no `compiler-artifact`. With `--keep-going` that set is the failed package plus any
/// dependents cargo skipped; because skipped dependents never appear in the stream at all, the
/// failed package's own missing artifact is the observable marker of skipped coverage. A
/// successful build, or a failed build without error-level diagnostics (for example a
/// build-script failure), is [`CheckState::Ready`].
pub fn parse_cargo_messages(
    stream: &[u8],
    input_generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    let mut problems: Vec<Problem> = Vec::new();
    let mut message_packages: HashSet<String> = HashSet::new();
    let mut artifact_packages: HashSet<String> = HashSet::new();
    let mut build_finished: Option<bool> = None;

    for line in stream.split(|&byte| byte == b'\n') {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<CargoEvent>(line) else {
            continue;
        };
        match event.reason.as_str() {
            "compiler-message" => {
                if let Some(package_id) = event.package_id.clone() {
                    message_packages.insert(package_id);
                }
                if let Some(message) = event.message
                    && let Some(problem) = diagnostic_problem(&message)
                {
                    problems.push(problem);
                }
            }
            "compiler-artifact" => {
                if let Some(package_id) = event.package_id {
                    artifact_packages.insert(package_id);
                }
            }
            "build-finished" => build_finished = event.success,
            _ => {}
        }
    }

    let Some(success) = build_finished else {
        return ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::Fatal,
            input_generation,
        );
    };
    let base = ProblemSnapshot::from_problems(
        Language::Rust,
        CheckState::Ready,
        problems,
        input_generation,
        duration_ms,
    );
    let state = if !success
        && base.errors > 0
        && message_packages
            .iter()
            .any(|package| !artifact_packages.contains(package))
    {
        CheckState::Partial
    } else {
        CheckState::Ready
    };
    ProblemSnapshot { state, ..base }
}

/// Converts one rustc diagnostic to a [`Problem`], or `None` when it must not be counted.
///
/// Only exact `error`/`warning` levels count; summary texts and diagnostics without a primary
/// span are skipped. The primary span supplies path, line and column; the diagnostic code and
/// short text complete the problem. Message length is capped by [`Problem::new`].
fn diagnostic_problem(message: &RustcMessage) -> Option<Problem> {
    let severity = match message.level.as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let summary_markers = ["aborting due to", "warning(s) emitted", "could not compile"];
    let text = format!(
        "{} {}",
        message.message,
        message.rendered.as_deref().unwrap_or("")
    )
    .to_lowercase();
    if summary_markers.iter().any(|marker| text.contains(marker)) {
        return None;
    }
    let span = message.spans.iter().find(|span| span.is_primary)?;
    Some(Problem::new(
        span.file_name.clone(),
        clamp_component(span.line_start),
        clamp_component(span.column_start),
        severity,
        message.code.as_ref().map(|code| code.code.clone()),
        message.message.clone(),
    ))
}

/// Clamps a span coordinate to `u32` for the snapshot schema.
///
/// The `u64` wire value saturates: coordinates beyond `u32::MAX` collapse to `u32::MAX` instead
/// of failing the event parse.
fn clamp_component(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// One cargo JSON event line, reduced to the fields the parser consumes.
///
/// Unknown fields are ignored: cargo adds event kinds freely across releases and the parser
/// only needs the ones named here.
#[derive(Debug, Deserialize)]
struct CargoEvent {
    /// Event discriminator (`compiler-message`, `compiler-artifact`, `build-finished`, …).
    #[serde(default)]
    reason: String,
    /// Embedded rustc diagnostic; absent for non-message events.
    #[serde(default)]
    message: Option<RustcMessage>,
    /// Package identifier of the emitting unit, e.g. `path+file:///…#crate@0.1.0`.
    #[serde(default)]
    package_id: Option<String>,
    /// Terminal build outcome; present only on `build-finished`.
    #[serde(default)]
    success: Option<bool>,
}

/// One rustc diagnostic inside a `compiler-message` event.
#[derive(Debug, Deserialize)]
struct RustcMessage {
    /// Diagnostic level; only exact `error`/`warning` are counted.
    #[serde(default)]
    level: String,
    /// Short diagnostic text used as the snapshot message.
    #[serde(default)]
    message: String,
    /// Rendered human-readable diagnostic, used only for summary-marker filtering.
    #[serde(default)]
    rendered: Option<String>,
    /// Diagnostic code object; `None` when rustc assigned none.
    #[serde(default)]
    code: Option<RustcCode>,
    /// Source spans; the primary one locates the problem.
    #[serde(default)]
    spans: Vec<RustcSpan>,
}

/// The `message.code` object of a rustc diagnostic.
#[derive(Debug, Deserialize)]
struct RustcCode {
    /// Machine-readable code such as `E0308` or `unused_variables`.
    #[serde(default)]
    code: String,
}

/// One source span of a rustc diagnostic.
#[derive(Debug, Deserialize)]
struct RustcSpan {
    /// File path as reported by rustc (workspace-relative for path dependencies).
    #[serde(default)]
    file_name: String,
    /// `true` for the span the diagnostic is anchored to.
    #[serde(default)]
    is_primary: bool,
    /// First line of the span; `0` when rustc reported none.
    #[serde(default)]
    line_start: u64,
    /// First column of the span; `0` when rustc reported none.
    #[serde(default)]
    column_start: u64,
}
