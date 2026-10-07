//! Shared project problem snapshot types and the confined [`Checker`](crate::checks::Checker) contract for EYES-r1 §4.
//!
//! Every parallel v0.3 project-check task builds against these types: checkers produce one
//! [`ProblemSnapshot`](crate::checks::ProblemSnapshot) per run, the scheduler stores the latest completed snapshot per
//! `(worktree, language)`, and the `<agent-ide>` block and `ide.context` problems kind render
//! from it. Counts always describe the full deduplicated result even when the retained
//! [`ProblemSnapshot::problems`](crate::checks::ProblemSnapshot::problems) list is capped; messages are untrusted checker output.

pub mod runner;

/// Cheap whole-worktree input fingerprint backing the scheduler's skip-unchanged rule (T20B).
pub mod fingerprint;

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Debounced scheduler that drives [`Checker`] runs per `(worktree, language)`, per EYES-r1 §5.
pub mod scheduler;

/// Maximum number of problems retained in one [`ProblemSnapshot`].
///
/// `errors`/`warnings` keep counting the full deduplicated result beyond this cap; the cap only
/// bounds the retained per-problem list that `ide.context` pages through.
pub const MAX_PROBLEMS: usize = 500;

/// Maximum retained characters in one [`Problem::message`].
///
/// Longer checker messages are cut to `MAX_MESSAGE_CHARS - 1` characters plus one trailing `…`
/// so a truncated message is visible as such; the contract bound is on characters, not bytes,
/// so multi-byte text keeps up to 200 characters.
pub const MAX_MESSAGE_CHARS: usize = 200;

/// Language a project check runs for: the registered language handle.
pub use crate::lang::Language;

/// A language's confined project-check integration, reached through its registered descriptor.
///
/// Implementations are stateless `'static` values. The scheduler asks them whether a worktree is
/// a project of the language and which cache to seed from a sibling worktree; the launcher asks
/// them to decode the language's `project_checks` section.
pub trait LanguageChecks: Send + Sync + 'static {
    /// Reports whether `worktree` looks like a project of this language, per the cheap and
    /// deterministic presence rule (T10B): a handful of `stat` calls at the worktree root, never
    /// a tree walk, cheap enough to re-evaluate on every trigger so a worktree that later gains
    /// its manifest starts being checked on its next trigger.
    fn is_present(&self, worktree: &Path) -> bool;

    /// The check tool named in model-facing tool descriptions (for example a compiler's check
    /// command).
    fn tool_name(&self) -> &'static str;

    /// Subdirectory of this language's check cache to seed, before a worktree's first check, with
    /// a copy-on-write clone of the most recently completed sibling worktree's cache for the same
    /// repository and read policy; `None` (the default) never seeds.
    fn sibling_cache(&self) -> Option<&'static str> {
        None
    }

    /// Why this language's project check does not analyse the worktree-relative `path` — for
    /// example a source file no build target reaches — or `None` (the default) when it may. An
    /// edit whose completed check named no problem in such a file answers `not_analysed` with this
    /// reason instead of `current_clean`. The answer is a pure function of the worktree's files;
    /// a wrong `Some` costs one redundant notice, never a false clean.
    fn not_analysed(&self, worktree: &Path, path: &Path) -> Option<&'static str> {
        let _ = (worktree, path);
        None
    }

    /// Decodes this language's closed `project_checks` launcher section. A shape error is the
    /// launcher's `Invalid`; path rules are checked later by [`CheckConfig::validate`].
    fn parse_config(
        &self,
        section: serde_json::Value,
    ) -> Result<Arc<dyn CheckConfig>, serde_json::Error>;
}

/// One language's decoded project-check declaration from the launcher configuration.
pub trait CheckConfig: std::any::Any + Send + Sync {
    /// Whether every declared path is absolute and lexically normal; `false` is the launcher's
    /// `Rejected`.
    fn validate(&self) -> bool;

    /// Builds this language's checker, confined through `runner`, with `timeout` as the total
    /// wall-clock ceiling of one run.
    fn checker(
        &self,
        runner: Arc<dyn runner::ConfinedRunner>,
        timeout: Duration,
    ) -> Arc<dyn Checker>;

    /// Toolchain programs `doctor` probes, in declaration order, each with the interpreter that
    /// runs it when it is not directly executable.
    fn programs(&self) -> Vec<(PathBuf, Option<PathBuf>)>;
}

impl dyn CheckConfig {
    /// Returns the concrete declaration when it is a `T`.
    pub fn downcast_ref<T: CheckConfig>(&self) -> Option<&T> {
        (self as &dyn std::any::Any).downcast_ref::<T>()
    }
}

/// Severity of one reported problem.
///
/// The derived ordering places errors before warnings, matching the snapshot sort order.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Severity {
    /// Error-level diagnostic; counted in [`ProblemSnapshot::errors`].
    Error,
    /// Warning-level diagnostic; counted in [`ProblemSnapshot::warnings`].
    Warning,
}

/// Why a language has no check result at all.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum UnavailableReason {
    /// Project checks are disabled by configuration (no `project_checks` or empty `allowed_roots`),
    /// or (T10B) this language is absent from the worktree per [`Language::is_present`]. Every
    /// renderer treats this reason as nothing rather than a fixed phrase (feed §6, problems §7).
    Disabled,
    /// The caller's current sandbox cannot prove a supported check read policy, so no
    /// fingerprint or checker may run and no cached diagnostic may be disclosed.
    ReadRestricted,
    /// The worktree's canonical path is not under any configured allowed root.
    OutsideRoots,
    /// The configured language toolchain binary was not found.
    ToolMissing,
    /// The required project environment (for example the project's interpreter) was not found.
    EnvMissing,
    /// The check tool ran against a resolved environment but analyzed zero files (for example
    /// a checker whose `include`/`exclude` configuration matches no file); distinct from
    /// [`UnavailableReason::EnvMissing`], which means the environment itself could not be
    /// resolved. Carries the same durable-condition replacement semantics as `EnvMissing` (T12B).
    NoFiles,
    /// The check ran and failed unrecoverably (crash, unparseable output, missing completion marker).
    Fatal,
    /// The check exceeded its configured timeout and its process group was killed.
    Timeout,
}

/// Lifecycle state of one language's latest result for a worktree.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CheckState {
    /// Complete result for the configured scope.
    Ready,
    /// Result present but coverage incomplete (for example dependent compilation units skipped).
    Partial,
    /// First result not yet available.
    Checking,
    /// No result; the carried reason explains why.
    Unavailable(UnavailableReason),
}

/// Why a language's stored result is not the current state of its worktree (T18B).
///
/// Drives the `checking (…)` wording of the `<agent-ide>` block and the problems text; a language
/// with no such reason renders its stored result as current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recheck {
    /// No result of this session exists yet: the stored result is absent or predates the
    /// session's activation of the worktree.
    FirstCheck,
    /// A check is running because of a trigger after the session's last result; that result stays
    /// visible as the last known one.
    FilesChanged,
}

/// One reported project problem.
///
/// All textual fields are untrusted checker output and must never be treated as trusted context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Problem {
    /// File the problem was reported for, in the checker's reported form (not canonicalized).
    pub path: String,
    /// Line as reported by the underlying tool; `0` when the tool reported no line.
    pub line: u32,
    /// Column as reported by the underlying tool; `0` when the tool reported no column.
    pub column: u32,
    /// Severity assigned by the checker.
    pub severity: Severity,
    /// Tool-specific diagnostic code (for example `E0308` or `reportUndefinedVariable`).
    pub code: Option<String>,
    /// Diagnostic message, capped at [`MAX_MESSAGE_CHARS`] characters; untrusted text.
    pub message: String,
}

impl Problem {
    /// Builds one problem, truncating `message` to [`MAX_MESSAGE_CHARS`] characters.
    ///
    /// This is the only sanctioned constructor; use it so the message cap holds even before a
    /// snapshot boundary normalizes the value again.
    pub fn new(
        path: String,
        line: u32,
        column: u32,
        severity: Severity,
        code: Option<String>,
        message: String,
    ) -> Self {
        Self {
            path,
            line,
            column,
            severity,
            code,
            message: truncate_message(message),
        }
    }
}

/// Latest completed check result for one `(worktree, language)` pair.
///
/// `errors`/`warnings` count all deduplicated problems even when [`ProblemSnapshot::problems`]
/// is capped. `Unavailable` and `Checking` states carry zero counts that renderers must never
/// print as numbers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProblemSnapshot {
    /// Language this snapshot belongs to.
    pub language: Language,
    /// Lifecycle state of this result.
    pub state: CheckState,
    /// Total deduplicated error count, independent of the retained problem list.
    pub errors: u32,
    /// Total deduplicated warning count, independent of the retained problem list.
    pub warnings: u32,
    /// Retained problems, capped at [`MAX_PROBLEMS`], sorted errors first, then path, line, column.
    pub problems: Vec<Problem>,
    /// `true` when at least one deduplicated problem was dropped by the [`MAX_PROBLEMS`] cap.
    pub truncated: bool,
    /// Workspace input generation this result was produced against, for fencing stale results.
    pub input_generation: u64,
    /// Wall-clock duration of the completed check in milliseconds; `0` before any run.
    pub duration_ms: u64,
    /// Optional bounded explanation of an `Unavailable` cause (for example the first `error:`
    /// line of a failed build's stderr); untrusted checker text, rendered in full by the
    /// `ide.context` problems page; the `<agent-ide>` block carries only its first 80 bytes, for a
    /// failed check. `None` for every other
    /// state and for an `Unavailable` snapshot with no cheaply available explanation.
    #[serde(default)]
    pub detail: Option<String>,
}

impl ProblemSnapshot {
    /// Builds a snapshot from a raw problem list.
    ///
    /// Messages are first re-truncated to [`MAX_MESSAGE_CHARS`], then problems are deduplicated
    /// by `(severity, code, path, line, column, message)`; duplicates count once. `errors` and
    /// `warnings` count the full deduplicated result. Problems are sorted errors first, then by
    /// path, line, column (code and message break remaining ties deterministically). Only the
    /// first [`MAX_PROBLEMS`] deduplicated problems are retained; [`ProblemSnapshot::truncated`]
    /// reports whether any were dropped.
    pub fn from_problems(
        language: Language,
        state: CheckState,
        problems: Vec<Problem>,
        input_generation: u64,
        duration_ms: u64,
    ) -> Self {
        let mut seen: HashSet<(Severity, Option<String>, String, u32, u32, String)> =
            HashSet::new();
        let mut unique: Vec<Problem> = Vec::new();
        for mut problem in problems {
            problem.message = truncate_message(problem.message);
            let key = (
                problem.severity,
                problem.code.clone(),
                problem.path.clone(),
                problem.line,
                problem.column,
                problem.message.clone(),
            );
            if seen.insert(key) {
                unique.push(problem);
            }
        }
        let errors = unique
            .iter()
            .filter(|problem| problem.severity == Severity::Error)
            .count() as u32;
        let warnings = unique
            .iter()
            .filter(|problem| problem.severity == Severity::Warning)
            .count() as u32;
        unique.sort_by(|a, b| problem_sort_key(a).cmp(&problem_sort_key(b)));
        let truncated = unique.len() > MAX_PROBLEMS;
        unique.truncate(MAX_PROBLEMS);
        Self {
            language,
            state,
            errors,
            warnings,
            problems: unique,
            truncated,
            input_generation,
            duration_ms,
            detail: None,
        }
    }

    /// Builds the zero-count snapshot for a language that cannot produce any result.
    ///
    /// No process ran, so the snapshot's duration is honestly zero; a run that did happen must
    /// report its measured duration through [`ProblemSnapshot::unavailable_with_detail`].
    pub fn unavailable(
        language: Language,
        reason: UnavailableReason,
        input_generation: u64,
    ) -> Self {
        Self::unavailable_with_detail(language, reason, input_generation, 0, None)
    }

    /// Builds the zero-count snapshot for a language that cannot produce any result, carrying an
    /// optional bounded explanation of the cause and the measured duration of the run that
    /// produced the failure.
    ///
    /// `detail` is untrusted checker text (for example the first `error:` line of a failed
    /// build's stderr); callers that have no cheap explanation should use
    /// [`ProblemSnapshot::unavailable`] instead of passing `None` here explicitly. `duration_ms`
    /// is the wall-clock time the failed run actually took, so a fast fatal (`exit 71` from a
    /// refused sandbox apply, ~10 ms) is never reported as an instant one.
    pub fn unavailable_with_detail(
        language: Language,
        reason: UnavailableReason,
        input_generation: u64,
        duration_ms: u64,
        detail: Option<String>,
    ) -> Self {
        Self {
            language,
            state: CheckState::Unavailable(reason),
            errors: 0,
            warnings: 0,
            problems: Vec::new(),
            truncated: false,
            input_generation,
            duration_ms,
            detail,
        }
    }

    /// Builds the zero-count placeholder for a language whose first result is still pending.
    pub fn checking(language: Language, input_generation: u64) -> Self {
        Self {
            language,
            state: CheckState::Checking,
            errors: 0,
            warnings: 0,
            problems: Vec::new(),
            truncated: false,
            input_generation,
            duration_ms: 0,
            detail: None,
        }
    }
}

/// Maximum bytes of a checker-run failure cause retained in a [`ProblemSnapshot::detail`].
pub const MAX_CAUSE_BYTES: usize = 160;

/// Truncates `value` to at most `max_bytes` UTF-8 bytes, cutting only on a whole character.
pub fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

/// Builds the bounded cause line for a check run that failed without a usable result.
///
/// Tiers, first match wins: the first `error:`-prefixed line of `stderr` (the tool's own summary,
/// for example `error: failed to run custom build command for …`), else the first non-empty
/// `stderr` line (a wrapper's refusal such as `sandbox-exec: sandbox_apply: Operation not
/// permitted` carries no `error:` prefix), else `exit <status>` when the run died with no output
/// at all. Every tier is untrusted checker output, so it is cut to [`MAX_CAUSE_BYTES`] and later
/// rendered through the single-line untrusted filter. Returns `None` only when the run left no
/// evidence whatsoever (empty stderr and no exit status).
pub fn run_failure_cause(stderr: &[u8], status: Option<i32>) -> Option<String> {
    let text = String::from_utf8_lossy(stderr);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("error:"))
        .or_else(|| text.lines().map(str::trim).find(|line| !line.is_empty()));
    match line {
        Some(line) => Some(truncate_bytes(line, MAX_CAUSE_BYTES)),
        None => status.map(|code| format!("exit {code}")),
    }
}

/// Sort key implementing the feed order: errors first, then path, line, column.
///
/// Code and message close the key so the order is total for a deduplicated list; `None` code
/// sorts as an empty code.
fn problem_sort_key(problem: &Problem) -> (Severity, &str, u32, u32, &str, &str) {
    (
        problem.severity,
        problem.path.as_str(),
        problem.line,
        problem.column,
        problem.code.as_deref().unwrap_or(""),
        problem.message.as_str(),
    )
}

/// Truncates a message to at most [`MAX_MESSAGE_CHARS`] characters, leaving shorter messages
/// intact (T19B).
///
/// A message longer than the cap is cut to `MAX_MESSAGE_CHARS - 1` characters and suffixed with
/// one `…`, so an agent can see where the text was cut. Idempotent: any message that already
/// fits — including one previously truncated to exactly the cap ending in `…` — passes through
/// unchanged, so the second truncation at the snapshot boundary is a no-op.
fn truncate_message(message: String) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        return message;
    }
    let mut truncated: String = message.chars().take(MAX_MESSAGE_CHARS - 1).collect();
    truncated.push('…');
    truncated
}

/// One confined check invocation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckRequest {
    /// Canonical path of the worktree the check runs in.
    pub worktree: PathBuf,
    /// Private per-`(worktree, language)` cache directory handed to the check process
    /// (for example as `CARGO_TARGET_DIR`); the checker may read and write it freely.
    pub cache_dir: PathBuf,
    /// Workspace input generation that triggered this check.
    pub input_generation: u64,
    /// Host read exclusions enforced by the checker and its confined child.
    #[serde(default)]
    pub read_denies: Vec<crate::execution::seatbelt::ReadDeny>,
}

/// Accepts only a normal, no-symlink worktree path outside host exclusions for checks.
/// Empty exclusions preserve the historical parser treatment of tool-reported paths.
pub fn check_problem_path_allowed(
    worktree: &Path,
    reported: &str,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> bool {
    if denies.is_empty() {
        return true;
    }
    let path = Path::new(reported);
    let relative = if path.is_absolute() {
        let Ok(relative) = path.strip_prefix(worktree) else {
            return false;
        };
        relative
    } else {
        path
    };
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let absolute = worktree.join(relative);
    if denies.iter().any(|deny| deny.matches(&absolute)) {
        return false;
    }
    let mut prefix = worktree.to_path_buf();
    for component in relative.components() {
        prefix.push(component.as_os_str());
        match std::fs::symlink_metadata(&prefix) {
            Ok(metadata) if metadata.file_type().is_symlink() => return false,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(_) => return false,
        }
    }
    true
}

/// Owned pinned future returned by [`Checker::check`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs one language's confined project checks.
///
/// Implementers must be usable from multiple scheduler tasks concurrently (`Send + Sync`) and
/// must keep the returned future self-contained: cancellation is dropping the future, and no
/// work it started may outlive it. A completed run reports through [`ProblemSnapshot`]; a
/// dropped future produces no snapshot at all.
pub trait Checker: Send + Sync {
    /// Language this checker runs checks for; fixed for the checker's lifetime.
    fn language(&self) -> Language;

    /// Starts one check for `request` and returns the future completing with its snapshot.
    ///
    /// The future must not require an executor beyond being awaited; implementers that delay or
    /// spawn must tie every resource to the future so a drop cancels them.
    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot>;
}

/// Scripted [`Checker`] substitute used by scheduler and renderer tests; always compiled.
///
/// Returns one fixed scripted snapshot per call (after an optional delay inside the returned
/// future, so a drop cancels the delay) and records every [`CheckRequest`] in call order. It
/// performs no process execution and must never be wired into production paths.
#[derive(Clone)]
pub struct FakeChecker {
    /// Language reported by [`Checker::language`].
    language: Language,
    /// Snapshot returned by every [`Checker::check`] call, cloned per call.
    snapshot: ProblemSnapshot,
    /// Optional delay awaited inside the returned future before the snapshot resolves.
    delay: Option<Duration>,
    /// Recorded requests in call order; shared through cloning.
    requests: Arc<Mutex<Vec<CheckRequest>>>,
}

impl FakeChecker {
    /// Builds a substitute that resolves immediately.
    pub fn new(language: Language, snapshot: ProblemSnapshot) -> Self {
        Self {
            language,
            snapshot,
            delay: None,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Builds a substitute that waits `delay` inside the returned future before resolving.
    pub fn with_delay(language: Language, snapshot: ProblemSnapshot, delay: Duration) -> Self {
        Self {
            language,
            snapshot,
            delay: Some(delay),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns the recorded requests in call order.
    pub fn requests(&self) -> Vec<CheckRequest> {
        self.requests
            .lock()
            .expect("FakeChecker request log is not poisoned")
            .clone()
    }
}

impl Checker for FakeChecker {
    fn language(&self) -> Language {
        self.language
    }

    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        self.requests
            .lock()
            .expect("FakeChecker request log is not poisoned")
            .push(request);
        let snapshot = self.snapshot.clone();
        let delay = self.delay;
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            snapshot
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Case variants of a denied glob base are rejected before a diagnostic path is probed.
    #[test]
    fn denied_glob_base_case_variant_cannot_disclose_a_problem() {
        let root = Path::new("/tmp/check-project");
        let denies = [crate::execution::seatbelt::ReadDeny::Glob {
            base: root.join("sub"),
            suffix: crate::execution::seatbelt::CredentialGlob::Key,
        }];
        assert!(!check_problem_path_allowed(root, "SUB/secret.key", &denies));
    }

    /// Builds one problem with a fixed code for compact test arrangements.
    fn problem(path: &str, line: u32, column: u32, severity: Severity, message: &str) -> Problem {
        Problem::new(
            path.to_string(),
            line,
            column,
            severity,
            Some("E0001".to_string()),
            message.to_string(),
        )
    }

    #[test]
    fn from_problems_deduplicates_identical_diagnostics() {
        let problems = vec![
            problem("src/lib.rs", 1, 1, Severity::Error, "boom"),
            problem("src/lib.rs", 1, 1, Severity::Error, "boom"),
            problem("src/lib.rs", 1, 1, Severity::Error, "different"),
        ];
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            problems,
            7,
            10,
        );
        assert_eq!(snapshot.errors, 2);
        assert_eq!(snapshot.warnings, 0);
        assert_eq!(snapshot.problems.len(), 2);
        assert!(!snapshot.truncated);
    }

    /// Builds one problem with an explicit `code` for tests that must exercise it as a dedup key.
    fn problem_with_code(
        path: &str,
        line: u32,
        column: u32,
        severity: Severity,
        code: Option<&str>,
        message: &str,
    ) -> Problem {
        Problem::new(
            path.to_string(),
            line,
            column,
            severity,
            code.map(str::to_string),
            message.to_string(),
        )
    }

    #[test]
    fn from_problems_dedup_key_includes_code() {
        // Same location and message, different `code`: both survive as distinct problems.
        let differing_code = vec![
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0308"), "boom"),
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0309"), "boom"),
        ];
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            differing_code,
            1,
            1,
        );
        assert_eq!(snapshot.problems.len(), 2);
        assert_eq!(snapshot.errors, 2);

        // Same location, message, and `code`: the duplicate merges into one problem.
        let same_code = vec![
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0308"), "boom"),
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0308"), "boom"),
        ];
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            same_code,
            1,
            1,
        );
        assert_eq!(snapshot.problems.len(), 1);
        assert_eq!(snapshot.errors, 1);
    }

    #[test]
    fn from_problems_sorts_errors_first_then_path_line_column() {
        let problems = vec![
            problem("c.rs", 2, 1, Severity::Warning, "w"),
            problem("a.rs", 5, 3, Severity::Warning, "w"),
            problem("b.rs", 9, 1, Severity::Error, "e"),
            problem("a.rs", 1, 1, Severity::Error, "e"),
            problem("a.rs", 5, 2, Severity::Error, "e"),
        ];
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            problems,
            1,
            5,
        );
        let order: Vec<(String, u32, u32, Severity)> = snapshot
            .problems
            .iter()
            .map(|problem| {
                (
                    problem.path.clone(),
                    problem.line,
                    problem.column,
                    problem.severity,
                )
            })
            .collect();
        assert_eq!(
            order,
            vec![
                ("a.rs".to_string(), 1, 1, Severity::Error),
                ("a.rs".to_string(), 5, 2, Severity::Error),
                ("b.rs".to_string(), 9, 1, Severity::Error),
                ("a.rs".to_string(), 5, 3, Severity::Warning),
                ("c.rs".to_string(), 2, 1, Severity::Warning),
            ]
        );
    }

    #[test]
    fn from_problems_caps_at_max_problems_with_full_counts() {
        let mut problems: Vec<Problem> = (1..=300)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        problems.extend((1..=203).map(|line| problem("w.rs", line, 1, Severity::Warning, "w")));
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            problems,
            2,
            20,
        );
        assert_eq!(snapshot.errors, 300);
        assert_eq!(snapshot.warnings, 203);
        assert_eq!(snapshot.problems.len(), MAX_PROBLEMS);
        assert!(snapshot.truncated);
        // All errors and the first 200 warnings survive the cap, in sort order.
        assert_eq!(snapshot.problems[299].path, "e.rs");
        assert_eq!(snapshot.problems[300].path, "w.rs");
        assert_eq!(snapshot.problems[300].line, 1);
        assert_eq!(snapshot.problems[499].path, "w.rs");
        assert_eq!(snapshot.problems[499].line, 200);
    }

    #[test]
    fn from_problems_truncation_boundary_at_max_problems() {
        // Exactly MAX_PROBLEMS unique problems: nothing is dropped.
        let exact: Vec<Problem> = (0..MAX_PROBLEMS as u32)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            exact,
            1,
            1,
        );
        assert_eq!(snapshot.errors, MAX_PROBLEMS as u32);
        assert_eq!(snapshot.problems.len(), MAX_PROBLEMS);
        assert!(!snapshot.truncated);

        // One more unique problem: it is dropped from the retained list, but counts stay complete.
        let over: Vec<Problem> = (0..=MAX_PROBLEMS as u32)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            over,
            1,
            1,
        );
        assert_eq!(snapshot.errors, MAX_PROBLEMS as u32 + 1);
        assert_eq!(snapshot.problems.len(), MAX_PROBLEMS);
        assert!(snapshot.truncated);
    }

    #[test]
    fn problem_message_is_capped_at_200_characters() {
        let long = Problem::new(
            "m.rs".to_string(),
            1,
            1,
            Severity::Warning,
            None,
            "ё".repeat(250),
        );
        assert_eq!(long.message.chars().count(), MAX_MESSAGE_CHARS);
        let short = Problem::new(
            "m.rs".to_string(),
            1,
            1,
            Severity::Warning,
            None,
            "short".to_string(),
        );
        assert_eq!(short.message, "short");
    }

    /// Truncation (T19B) marks a cut message with one trailing `…`, keeps the total at
    /// [`MAX_MESSAGE_CHARS`] characters for multi-byte text, leaves shorter and exactly-cap
    /// messages intact, and is idempotent: re-truncating an already-truncated message — at the
    /// snapshot boundary, or an arbitrary 200-character message that happens to end in `…` —
    /// changes nothing.
    #[test]
    fn truncate_message_marks_cuts_with_ellipsis_and_is_idempotent() {
        let short = truncate_message("short".to_owned());
        assert_eq!(short, "short");

        let exact: String = "ё".repeat(MAX_MESSAGE_CHARS);
        assert_eq!(truncate_message(exact.clone()), exact);

        let long = truncate_message("ё".repeat(300));
        assert_eq!(long.chars().count(), MAX_MESSAGE_CHARS);
        assert!(long.ends_with('…'));
        assert_eq!(truncate_message(long.clone()), long);

        let capped_ellipsis: String = "a".repeat(MAX_MESSAGE_CHARS - 1) + "…";
        assert_eq!(truncate_message(capped_ellipsis.clone()), capped_ellipsis);
    }

    #[test]
    fn from_problems_retruncates_oversized_struct_literal_messages() {
        let oversized = Problem {
            path: "m.rs".to_string(),
            line: 1,
            column: 1,
            severity: Severity::Warning,
            code: None,
            message: "ё".repeat(300),
        };
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::BETA,
            CheckState::Ready,
            vec![oversized],
            3,
            1,
        );
        assert_eq!(
            snapshot.problems[0].message.chars().count(),
            MAX_MESSAGE_CHARS
        );
    }

    #[tokio::test]
    async fn fake_checker_returns_scripted_snapshot_and_records_requests() {
        let snapshot = ProblemSnapshot::checking(crate::lang::testing::BETA, 3);
        let checker = FakeChecker::with_delay(
            crate::lang::testing::BETA,
            snapshot.clone(),
            Duration::from_millis(1),
        );
        assert_eq!(checker.language(), crate::lang::testing::BETA);
        let first = CheckRequest {
            worktree: PathBuf::from("/wt"),
            cache_dir: PathBuf::from("/cache"),
            input_generation: 3,
            read_denies: Vec::new(),
        };
        let second = CheckRequest {
            worktree: PathBuf::from("/wt2"),
            cache_dir: PathBuf::from("/cache2"),
            input_generation: 4,
            read_denies: Vec::new(),
        };
        let returned = checker.check(first.clone()).await;
        assert_eq!(returned, snapshot);
        assert!(checker.check(second.clone()).await == snapshot);
        assert_eq!(checker.requests(), vec![first, second]);
    }

    #[test]
    fn snapshot_constructors_and_wire_format_round_trip() {
        crate::lang::testing::install();
        assert_eq!(crate::lang::testing::ALPHA.as_str(), "alpha");
        assert_eq!(crate::lang::testing::BETA.as_str(), "beta");
        assert!(crate::lang::testing::ALPHA < crate::lang::testing::BETA);
        let unavailable = ProblemSnapshot::unavailable(
            crate::lang::testing::ALPHA,
            UnavailableReason::ToolMissing,
            11,
        );
        assert_eq!(unavailable.errors, 0);
        assert_eq!(unavailable.warnings, 0);
        assert!(unavailable.problems.is_empty());
        assert!(!unavailable.truncated);
        assert_eq!(unavailable.duration_ms, 0);
        let text = serde_json::to_string(&unavailable).expect("snapshot serializes");
        let parsed: ProblemSnapshot = serde_json::from_str(&text).expect("snapshot parses");
        assert_eq!(parsed, unavailable);
    }
}
