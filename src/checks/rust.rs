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
    /// Apple developer directory read roots resolved once at construction (see
    /// [`resolve_developer_roots`]); `/usr/bin/cc` is an `xcrun` shim and cannot run without them.
    developer_roots: Vec<PathBuf>,
}

impl RustChecker {
    /// Builds a checker.
    ///
    /// `toolchain_dir` is the toolchain root whose `bin/cargo` is executed; when the binary is
    /// missing, [`RustChecker::check`] reports [`UnavailableReason::ToolMissing`]. `cargo_home`
    /// overrides the cargo home read root; `None` means `$HOME/.cargo`. `timeout` bounds each
    /// cargo run; expiry yields [`UnavailableReason::Timeout`] (the runner kills the group).
    /// `developer_dir` is the operator-declared `project_checks.rust.developer_dir` override; when
    /// absent (or the override does not exist) it is resolved from `/usr/bin/xcode-select -p`,
    /// falling back to `/Applications/Xcode.app/Contents/Developer` then
    /// `/Library/Developer/CommandLineTools`, plus `/private/var/db/xcode_select_link` and
    /// `/Library/Developer/CommandLineTools` when they exist, on the read roots this checker
    /// resolves once at construction.
    pub fn new(
        runner: Arc<dyn ConfinedRunner>,
        toolchain_dir: PathBuf,
        cargo_home: Option<PathBuf>,
        timeout: Duration,
        developer_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            runner,
            toolchain_dir,
            cargo_home,
            timeout,
            developer_roots: resolve_developer_roots(developer_dir),
        }
    }

    /// Builds the exact confined cargo invocation for `request`.
    ///
    /// Program is `<toolchain_dir>/bin/cargo` with `check --workspace --all-targets
    /// --message-format=json --offline --keep-going --locked` in the worktree. `--locked` is
    /// always passed (EYES-r2 §4): the worktree is read-only for the check, so a missing or
    /// outdated lockfile cannot be written and cargo's refusal maps to
    /// [`UnavailableReason::EnvMissing`] in [`RustChecker::check`]. The environment is
    /// rebuilt from the allowlist: `PATH` limited to toolchain bins plus the system dirs,
    /// `HOME`, `TMPDIR`/`CARGO_TARGET_DIR` under the private cache, and `CARGO_NET_OFFLINE=true`.
    /// Read roots cover the worktree, the toolchain, the cargo home, the derived rustup home,
    /// `/private/etc` and this checker's resolved Apple developer directory roots (build scripts
    /// such as `blake3`'s invoke `/usr/bin/cc`, an `xcrun` shim that needs the Apple developer
    /// directory to run at all); the private cache is the only write root; each output stream is
    /// capped at `MAX_OUTPUT_BYTES`. The construction is pure with respect to the process
    /// environment: its only inputs are the checker configuration and `request`.
    pub fn cargo_check_spec(&self, request: &CheckRequest) -> RunSpec {
        let home = real_home();
        let cargo_home = self.effective_cargo_home(&home);
        let rustup_home = derived_rustup_home(&self.toolchain_dir, &home);
        let args: Vec<OsString> = vec![
            "check",
            "--workspace",
            "--all-targets",
            "--message-format=json",
            "--offline",
            "--keep-going",
            "--locked",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
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
            read_roots: [
                request.worktree.clone(),
                self.toolchain_dir.clone(),
                cargo_home,
                rustup_home,
                PathBuf::from(ETC_READ_ROOT),
            ]
            .into_iter()
            .chain(self.developer_roots.iter().cloned())
            .collect(),
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

/// Resolves the Apple developer directory read roots for the confined cargo run.
///
/// A build script that compiles native code (for example `blake3`'s) runs `/usr/bin/cc`, which
/// is an `xcrun` shim: without read access to the active Xcode/Command Line Tools installation it
/// fails immediately, before producing any `compiler-message`, which [`parse_cargo_messages`]
/// would otherwise have no choice but to treat as a build failure with no diagnostics.
///
/// `override_dir` is the operator-declared `project_checks.rust.developer_dir`; when it names an
/// existing directory it is the sole primary root and no `xcode-select` query runs. Otherwise the
/// primary root comes from [`xcode_select_developer_dir`]. Beyond the primary root,
/// `/private/var/db/xcode_select_link` and `/Library/Developer/CommandLineTools` are always
/// appended when they exist (deduplicated against the primary root), because `/usr/bin/cc`
/// resolves through that symlink database independently of which developer directory
/// `xcode-select` currently reports. Only roots that exist on this filesystem are ever returned,
/// so a machine with neither Xcode nor the Command Line Tools installed adds no read root at all
/// (a build script that needs a C compiler still fails, just as it would unconfined).
fn resolve_developer_roots(override_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let primary = override_dir
        .filter(|dir| dir.is_dir())
        .or_else(xcode_select_developer_dir);
    if let Some(primary) = primary {
        roots.push(primary);
    }
    for candidate in [
        PathBuf::from("/private/var/db/xcode_select_link"),
        PathBuf::from("/Library/Developer/CommandLineTools"),
    ] {
        if candidate.exists() && !roots.contains(&candidate) {
            roots.push(candidate);
        }
    }
    roots
}

/// Runs `/usr/bin/xcode-select -p` and returns its trimmed stdout as an existing directory.
///
/// Falls back in order to `/Applications/Xcode.app/Contents/Developer` then
/// `/Library/Developer/CommandLineTools` when the query fails, exits non-zero, prints non-UTF-8
/// or non-existent output; returns `None` when neither fallback exists either.
fn xcode_select_developer_dir() -> Option<PathBuf> {
    std::process::Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| PathBuf::from(stdout.trim()))
        .filter(|path| path.is_dir())
        .or_else(|| existing_dir("/Applications/Xcode.app/Contents/Developer"))
        .or_else(|| existing_dir("/Library/Developer/CommandLineTools"))
}

/// Returns `path` as a [`PathBuf`] when it names an existing directory, otherwise `None`.
fn existing_dir(path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(path);
    path.is_dir().then_some(path)
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
    parse_cargo_messages(
        &output.stdout,
        &output.stderr,
        request.input_generation,
        duration_ms,
    )
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
/// completed the run, which is [`UnavailableReason::Fatal`]. A terminal `build-finished.success:
/// false` with zero deduplicated errors — a build failure with no diagnostics to show, for
/// example a build-script failure such as `blake3`'s invoking a `cc` it cannot read — is also
/// [`UnavailableReason::Fatal`], carrying the first `error:` line of `stderr` (trimmed to 160
/// bytes) as [`ProblemSnapshot::detail`] when one is present; counts must never be fabricated as
/// `Ready` for a build that did not actually compile the workspace. Otherwise the snapshot is
/// [`CheckState::Partial`] exactly when `build-finished.success` is `false`, at least one
/// deduplicated error exists, and at least one package that produced `compiler-message` events
/// produced no `compiler-artifact`. With `--keep-going` that set is the failed package plus any
/// dependents cargo skipped; because skipped dependents never appear in the stream at all, the
/// failed package's own missing artifact is the observable marker of skipped coverage. Every
/// other outcome — a successful build, or a failed build whose errors are all covered by that
/// `Partial` rule — is [`CheckState::Ready`].
pub fn parse_cargo_messages(
    stdout: &[u8],
    stderr: &[u8],
    input_generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    let mut problems: Vec<Problem> = Vec::new();
    let mut message_packages: HashSet<String> = HashSet::new();
    let mut artifact_packages: HashSet<String> = HashSet::new();
    let mut build_finished: Option<bool> = None;

    for line in stdout.split(|&byte| byte == b'\n') {
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
    if !success && base.errors == 0 {
        return ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            input_generation,
            first_error_line(stderr),
        );
    }
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

/// Extracts the first `error:`-prefixed line of cargo stderr, trimmed to at most 160 bytes.
///
/// Used only to explain an [`UnavailableReason::Fatal`] snapshot produced by a failed build with
/// no error-level diagnostics: cargo's own summary line (for example `error: failed to run custom
/// build command for \`blake3 v1.5.0\``) is the most actionable cause available without running
/// an unbounded, untrusted stderr stream through the feed. Returns `None` when no line starts
/// with `error:` after trimming, so a build failure with no such line simply carries no detail.
fn first_error_line(stderr: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("error:"))?;
    Some(truncate_bytes(line, 160))
}

/// Truncates `value` to at most `max_bytes` UTF-8 bytes, cutting only on a whole character.
fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
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
