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
    /// Linker-bypass environment (`CARGO_TARGET_<TRIPLE>_LINKER`/`RUSTFLAGS`, `CC`, `CXX`,
    /// `SDKROOT`) resolved once at construction (see [`resolve_linker_env`]); empty when no
    /// toolchain clang could be found, leaving `/usr/bin/cc` (the `xcrun` shim) as the linker.
    linker_env: Vec<(String, String)>,
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
    /// resolves once at construction. The same resolved primary developer directory feeds the
    /// linker-bypass environment (T06B), so a build script's linker step bypasses the `xcrun`
    /// shim whenever a toolchain clang is found under it.
    pub fn new(
        runner: Arc<dyn ConfinedRunner>,
        toolchain_dir: PathBuf,
        cargo_home: Option<PathBuf>,
        timeout: Duration,
        developer_dir: Option<PathBuf>,
    ) -> Self {
        let primary_developer_dir = resolve_primary_developer_dir(developer_dir);
        let linker_env = resolve_linker_env(primary_developer_dir.as_deref(), &toolchain_dir);
        Self {
            runner,
            toolchain_dir,
            cargo_home,
            timeout,
            developer_roots: resolve_developer_roots(primary_developer_dir),
            linker_env,
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
    /// `HOME`, `TMPDIR`/`CARGO_TARGET_DIR` under the private cache, `CARGO_NET_OFFLINE=true`, and
    /// this checker's resolved linker-bypass environment (T06B): compiling still needs read
    /// access to the Apple developer directory for headers and libraries, but the link step of a
    /// build script (for example `blake3`'s) is pointed straight at the toolchain `clang` instead
    /// of `/usr/bin/cc`, because that `cc` is an `xcrun` shim that fails under this Seatbelt
    /// profile. Read roots cover the worktree, the toolchain, the
    /// cargo home, the derived rustup home, `/private/etc` and this checker's resolved Apple
    /// developer directory roots; the private cache is the only write root; each output stream is
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
            env: [
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
            ]
            .into_iter()
            .chain(self.linker_env.iter().cloned())
            .collect(),
            read_roots: [
                request.worktree.clone(),
                self.toolchain_dir.clone(),
                cargo_home,
                rustup_home,
                PathBuf::from(ETC_READ_ROOT),
            ]
            .into_iter()
            .chain(self.developer_roots.iter().cloned())
            .chain(ancestor_manifest_roots(&request.worktree))
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

/// Resolves the ancestor manifest and cargo-config files a confined `cargo check` needs to read
/// when the worktree is nested inside another Cargo project (T07B).
///
/// Cargo walks every ancestor of the current directory looking for a workspace root, reading
/// each ancestor's `Cargo.toml` (and `.cargo/config.toml`/`.cargo/config`, which also affect
/// workspace discovery) even when that ancestor turns out not to be a workspace root at all —
/// for example the parent checkout at `/Users/pluto/projects/agent-ide/Cargo.toml` when the
/// checked worktree is `/Users/pluto/projects/agent-ide/.claude/worktrees/<name>`. Without read
/// access to those files the walk itself fails with `Operation not permitted` before cargo ever
/// resolves a workspace, regardless of whether the ancestor is a plain package (an implicit
/// single-package workspace, which does not claim the nested worktree as a member — cargo then
/// treats the worktree as its own workspace root, same as if the parent didn't exist) or an
/// explicit `[workspace]` that also doesn't list the worktree (which cargo rejects with "current
/// package believes it's in a workspace when it's not", a genuine project misconfiguration this
/// checker must surface, not hide, via the [`UnavailableReason::Fatal`] detail on a missing
/// `build-finished` event).
///
/// Only the ancestors of `worktree` are walked, not `worktree` itself (already covered by the
/// worktree's own read root), starting from the canonical path (Seatbelt matches canonical
/// paths) up to the filesystem root. Only files that exist are returned; ancestor directories
/// themselves are never added as roots, keeping the added read access limited to the exact
/// manifest and config files cargo consults.
fn ancestor_manifest_roots(worktree: &Path) -> Vec<PathBuf> {
    let canonical = std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let mut ancestors = canonical.ancestors();
    ancestors.next();
    let mut roots = Vec::new();
    for ancestor in ancestors {
        for relative in ["Cargo.toml", ".cargo/config.toml", ".cargo/config"] {
            let candidate = ancestor.join(relative);
            if candidate.is_file() {
                roots.push(candidate);
            }
        }
    }
    roots
}

/// Returns the real user home directory from the password database (never `$HOME`, which a host
/// may substitute), honoring only the `AGENT_IDE_HOME` override; when neither resolves the system
/// temp dir is substituted so path construction stays absolute.
fn real_home() -> PathBuf {
    crate::userhome::user_home().unwrap_or_else(env::temp_dir)
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

/// Resolves the primary Apple developer directory: `override_dir` when it names an existing
/// directory, else the result of [`xcode_select_developer_dir`].
///
/// Split out from [`resolve_developer_roots`] so both the read-root list and [`resolve_linker_env`]
/// derive their toolchain layout from the exact same primary directory.
fn resolve_primary_developer_dir(override_dir: Option<PathBuf>) -> Option<PathBuf> {
    override_dir
        .filter(|dir| dir.is_dir())
        .or_else(xcode_select_developer_dir)
}

/// Resolves the Apple developer directory read roots for the confined cargo run.
///
/// A build script that compiles native code (for example `blake3`'s) runs `/usr/bin/cc`, which
/// is an `xcrun` shim: without read access to the active Xcode/Command Line Tools installation it
/// fails immediately, before producing any `compiler-message`, which [`parse_cargo_messages`]
/// would otherwise have no choice but to treat as a build failure with no diagnostics.
///
/// `primary` is the directory resolved by [`resolve_primary_developer_dir`]; when present it is
/// the first root. Beyond it, `/private/var/db/xcode_select_link` and
/// `/Library/Developer/CommandLineTools` are always appended when they exist (deduplicated
/// against the primary root), because `/usr/bin/cc` resolves through that symlink database
/// independently of which developer directory `xcode-select` currently reports. Only roots that
/// exist on this filesystem are ever returned, so a machine with neither Xcode nor the Command
/// Line Tools installed adds no read root at all (a build script that needs a C compiler still
/// fails, just as it would unconfined).
fn resolve_developer_roots(primary: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
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

/// Resolves the linker-bypass environment for the confined cargo run: `CC`, optionally `CXX`,
/// optionally `AR`, optionally `RANLIB`, optionally `SDKROOT`, and either
/// `CARGO_TARGET_<TRIPLE>_LINKER` or `RUSTFLAGS`.
///
/// `/usr/bin/cc` is Apple's `xcrun` shim: it writes an `xcrun_db` cache into the real Darwin user
/// temp directory (via `confstr(_CS_DARWIN_USER_TEMP_DIR)`, ignoring `TMPDIR`) and dyld-loads
/// Xcode frameworks outside `Contents/Developer`, both forbidden by the Seatbelt profile, so every
/// build-script link step fails with exit status 71 even though compilation itself succeeds. This
/// resolves the toolchain's own `clang` under `developer_dir` and points cargo/cc at it directly,
/// skipping the shim entirely. `/usr/bin/ar` is the same kind of shim, used by the `cc` crate to
/// archive object files into a static library (for example `blake3`'s `libblake3_neon.a`), so it
/// fails the same way; the toolchain's own `ar` (and `ranlib`, which the `cc` crate also invokes
/// as a separate step) sit next to `clang` and bypass it the same way.
///
/// `developer_dir` is the primary directory from [`resolve_primary_developer_dir`]; `None` (no
/// Xcode or Command Line Tools resolved at all) leaves the environment untouched, and the T05B
/// fatal-detail path reports the resulting failure. Otherwise [`resolve_clang`] locates the
/// toolchain `clang`; when none is found the environment is also left untouched — clang missing
/// entirely is reported the same way an unconfined build would fail. When clang is found: `CC` is
/// always set; `CXX` is set only when a sibling `clang++` exists; `AR` and `RANLIB` are set only
/// when the corresponding sibling binaries exist next to `clang`; `SDKROOT` is set only when
/// [`resolve_sdk`] finds the platform SDK under `developer_dir`; and the linker variable is
/// `CARGO_TARGET_<TRIPLE>_LINKER` when [`derive_target_env_var`] can read a target triple out of
/// `toolchain_dir`'s own name, else `RUSTFLAGS=-Clinker=<clang>`.
fn resolve_linker_env(developer_dir: Option<&Path>, toolchain_dir: &Path) -> Vec<(String, String)> {
    let mut env = Vec::new();
    let Some(developer_dir) = developer_dir else {
        return env;
    };
    let Some(clang) = resolve_clang(developer_dir) else {
        return env;
    };
    let clang_path = clang.to_string_lossy().into_owned();
    match derive_target_env_var(toolchain_dir) {
        Some(target) => env.push((format!("CARGO_TARGET_{target}_LINKER"), clang_path.clone())),
        None => env.push(("RUSTFLAGS".to_owned(), format!("-Clinker={clang_path}"))),
    }
    env.push(("CC".to_owned(), clang_path));
    if let Some(clangxx) = sibling_clangxx(&clang) {
        env.push(("CXX".to_owned(), clangxx.to_string_lossy().into_owned()));
    }
    if let Some(ar) = sibling_tool(&clang, "ar") {
        env.push(("AR".to_owned(), ar.to_string_lossy().into_owned()));
    }
    if let Some(ranlib) = sibling_tool(&clang, "ranlib") {
        env.push(("RANLIB".to_owned(), ranlib.to_string_lossy().into_owned()));
    }
    if let Some(sdk) = resolve_sdk(developer_dir) {
        env.push(("SDKROOT".to_owned(), sdk.to_string_lossy().into_owned()));
    }
    env
}

/// Locates the toolchain `clang` under an Apple developer directory.
///
/// Tries the Xcode layout (`<dir>/Toolchains/XcodeDefault.xctoolchain/usr/bin/clang`) first, then
/// the Command Line Tools layout (`<dir>/usr/bin/clang`); returns the first that exists as a file.
fn resolve_clang(developer_dir: &Path) -> Option<PathBuf> {
    let xcode_clang = developer_dir.join("Toolchains/XcodeDefault.xctoolchain/usr/bin/clang");
    if xcode_clang.is_file() {
        return Some(xcode_clang);
    }
    let clt_clang = developer_dir.join("usr/bin/clang");
    clt_clang.is_file().then_some(clt_clang)
}

/// Locates the macOS platform SDK under an Apple developer directory.
///
/// Tries the Xcode layout (`<dir>/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk`) first,
/// then the Command Line Tools layout (`<dir>/SDKs/MacOSX.sdk`); returns the first that exists as
/// a directory.
fn resolve_sdk(developer_dir: &Path) -> Option<PathBuf> {
    let xcode_sdk = developer_dir.join("Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk");
    if xcode_sdk.is_dir() {
        return Some(xcode_sdk);
    }
    let clt_sdk = developer_dir.join("SDKs/MacOSX.sdk");
    clt_sdk.is_dir().then_some(clt_sdk)
}

/// Returns `clang`'s sibling `clang++` when it exists as a file, else `None`.
fn sibling_clangxx(clang: &Path) -> Option<PathBuf> {
    let name = clang.file_name()?.to_str()?;
    let candidate = clang.with_file_name(format!("{name}++"));
    candidate.is_file().then_some(candidate)
}

/// Returns `clang`'s sibling `<tool>` (in the same `usr/bin` directory) when it exists as a file,
/// else `None`.
fn sibling_tool(clang: &Path, tool: &str) -> Option<PathBuf> {
    let candidate = clang.with_file_name(tool);
    candidate.is_file().then_some(candidate)
}

/// Derives a `CARGO_TARGET_<TRIPLE>_LINKER` suffix from a toolchain directory's own name.
///
/// Rustup toolchain directory names embed the target triple after the channel/version, for
/// example `1.98.1-aarch64-apple-darwin`, `stable-x86_64-apple-darwin` or
/// `nightly-2026-08-14-aarch64-apple-darwin`. This splits the name on `-` and looks for the
/// `apple-darwin` pair (the only vendor/OS this checker targets); the element immediately before
/// it is the architecture. Returns `None` when the name carries no such pair (for example a
/// standalone toolchain directory not named after its triple), so the caller falls back to
/// `RUSTFLAGS` instead of guessing a target triple.
fn derive_target_env_var(toolchain_dir: &Path) -> Option<String> {
    let name = toolchain_dir.file_name()?.to_str()?;
    let parts: Vec<&str> = name.split('-').collect();
    for index in 1..parts.len().saturating_sub(1) {
        if parts[index] == "apple" && parts[index + 1] == "darwin" {
            return Some(format!("{}_apple_darwin", parts[index - 1]).to_uppercase());
        }
    }
    None
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
/// completed the run, which is [`UnavailableReason::Fatal`], carrying the first `error:` line of
/// `stderr` (T07B) as its [`ProblemSnapshot::detail`] when one exists — for example cargo's own
/// `error: failed searching for potential workspace` when an ancestor manifest it needed to read
/// was outside the confined read roots. A terminal `build-finished.success:
/// false` with zero deduplicated errors — a build failure with no diagnostics to show, for
/// example a build-script link failure (T06B: `cc` exiting with a nonzero status has no primary
/// span, so it is never counted as a diagnostic) — is also [`UnavailableReason::Fatal`],
/// carrying [`ProblemSnapshot::detail`] when one is available: the `message.message` of the first
/// `compiler-message` at `error` level, even without a primary span, takes priority over cargo's
/// own stderr summary; only when no such message exists does the first `error:` line of `stderr`
/// stand in (both trimmed to 160 bytes). Counts must never be fabricated as `Ready` for a build
/// that did not actually compile the workspace. Otherwise the snapshot is
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
    let mut first_error_message: Option<String> = None;

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
                if let Some(message) = event.message {
                    if first_error_message.is_none() && message.level == "error" {
                        first_error_message = Some(message.message.clone());
                    }
                    if let Some(problem) = diagnostic_problem(&message) {
                        problems.push(problem);
                    }
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
        return ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            input_generation,
            first_error_line(stderr),
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
        let detail = first_error_message
            .map(|message| truncate_bytes(&message, 160))
            .or_else(|| first_error_line(stderr));
        return ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            input_generation,
            detail,
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
/// Fallback source of [`ProblemSnapshot::detail`] for an [`UnavailableReason::Fatal`] snapshot
/// produced by a failed build with no error-level `compiler-message` at all (so
/// [`parse_cargo_messages`] has no rustc-authored text to prefer): cargo's own summary line (for
/// example `error: failed to run custom build command for \`blake3 v1.5.0\``) is the most
/// actionable cause available without running an unbounded, untrusted stderr stream through the
/// feed. Returns `None` when no line starts with `error:` after trimming, so a build failure with
/// no such line simply carries no detail.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves a versioned rustup toolchain name (`<version>-<triple>`) yields the matching
    /// `CARGO_TARGET_<TRIPLE>_LINKER` suffix.
    #[test]
    fn derive_target_env_var_handles_versioned_toolchain_name() {
        assert_eq!(
            derive_target_env_var(Path::new("1.98.1-aarch64-apple-darwin")),
            Some("AARCH64_APPLE_DARWIN".to_owned())
        );
    }

    /// Proves a channel rustup toolchain name (`<channel>-<triple>`) with an underscore in the
    /// architecture yields the matching suffix.
    #[test]
    fn derive_target_env_var_handles_channel_toolchain_name() {
        assert_eq!(
            derive_target_env_var(Path::new("stable-x86_64-apple-darwin")),
            Some("X86_64_APPLE_DARWIN".to_owned())
        );
    }

    /// Proves a dated nightly rustup toolchain name (`nightly-<date>-<triple>`) yields the
    /// matching suffix, ignoring the embedded date's own hyphens.
    #[test]
    fn derive_target_env_var_handles_dated_nightly_toolchain_name() {
        assert_eq!(
            derive_target_env_var(Path::new("nightly-2026-08-14-aarch64-apple-darwin")),
            Some("AARCH64_APPLE_DARWIN".to_owned())
        );
    }

    /// Proves a toolchain directory name with no `apple-darwin` pair derives no target env var,
    /// so [`resolve_linker_env`] falls back to `RUSTFLAGS`.
    #[test]
    fn derive_target_env_var_returns_none_for_unrecognized_name() {
        assert_eq!(derive_target_env_var(Path::new("tc")), None);
    }
}
