//! Shared project problem snapshot types and the confined [`Checker`] contract for EYES-r1 §4.
//!
//! Every parallel v0.3 project-check task builds against these types: checkers produce one
//! [`ProblemSnapshot`] per run, the scheduler stores the latest completed snapshot per
//! `(worktree, language)`, and the `<agent-ide>` block and `ide.context` problems kind render
//! from it. Counts always describe the full deduplicated result even when the retained
//! [`ProblemSnapshot::problems`] list is capped; messages are untrusted checker output.

pub mod runner;

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Maximum number of problems retained in one [`ProblemSnapshot`].
///
/// `errors`/`warnings` keep counting the full deduplicated result beyond this cap; the cap only
/// bounds the retained per-problem list that `ide.context` pages through.
pub const MAX_PROBLEMS: usize = 500;

/// Maximum retained characters in one [`Problem::message`].
///
/// Longer checker messages are truncated to this cap; the contract bound is on characters, not
/// bytes, so multi-byte text keeps up to 200 characters.
pub const MAX_MESSAGE_CHARS: usize = 200;

/// Language a project check runs for.
///
/// The declaration order is the fixed feed order (Rust before Python); the derived ordering
/// relies on it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Language {
    /// Rust checks (`cargo check --workspace --all-targets …`).
    Rust,
    /// Python checks (`pyright --outputjson …`).
    Python,
}

impl Language {
    /// Canonical lowercase identifier used by the feed and `ide.context` problems kind:
    /// `"rust"` or `"python"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::Python => "python",
        }
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
    /// Project checks are disabled by configuration (no `project_checks` or empty `allowed_roots`).
    Disabled,
    /// The worktree's canonical path is not under any configured allowed root.
    OutsideRoots,
    /// The configured language toolchain binary was not found.
    ToolMissing,
    /// The required project environment (for example the Python interpreter) was not found.
    EnvMissing,
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
        }
    }

    /// Builds the zero-count snapshot for a language that cannot produce any result.
    pub fn unavailable(
        language: Language,
        reason: UnavailableReason,
        input_generation: u64,
    ) -> Self {
        Self {
            language,
            state: CheckState::Unavailable(reason),
            errors: 0,
            warnings: 0,
            problems: Vec::new(),
            truncated: false,
            input_generation,
            duration_ms: 0,
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
        }
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

/// Truncates a message to [`MAX_MESSAGE_CHARS`] characters, leaving shorter messages intact.
fn truncate_message(message: String) -> String {
    if message.chars().count() <= MAX_MESSAGE_CHARS {
        message
    } else {
        message.chars().take(MAX_MESSAGE_CHARS).collect()
    }
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
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, problems, 7, 10);
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
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, differing_code, 1, 1);
        assert_eq!(snapshot.problems.len(), 2);
        assert_eq!(snapshot.errors, 2);

        // Same location, message, and `code`: the duplicate merges into one problem.
        let same_code = vec![
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0308"), "boom"),
            problem_with_code("src/lib.rs", 1, 1, Severity::Error, Some("E0308"), "boom"),
        ];
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, same_code, 1, 1);
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
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, problems, 1, 5);
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
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, problems, 2, 20);
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
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, exact, 1, 1);
        assert_eq!(snapshot.errors, MAX_PROBLEMS as u32);
        assert_eq!(snapshot.problems.len(), MAX_PROBLEMS);
        assert!(!snapshot.truncated);

        // One more unique problem: it is dropped from the retained list, but counts stay complete.
        let over: Vec<Problem> = (0..=MAX_PROBLEMS as u32)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, over, 1, 1);
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
            Language::Python,
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
        let snapshot = ProblemSnapshot::checking(Language::Python, 3);
        let checker =
            FakeChecker::with_delay(Language::Python, snapshot.clone(), Duration::from_millis(1));
        assert_eq!(checker.language(), Language::Python);
        let first = CheckRequest {
            worktree: PathBuf::from("/wt"),
            cache_dir: PathBuf::from("/cache"),
            input_generation: 3,
        };
        let second = CheckRequest {
            worktree: PathBuf::from("/wt2"),
            cache_dir: PathBuf::from("/cache2"),
            input_generation: 4,
        };
        let returned = checker.check(first.clone()).await;
        assert_eq!(returned, snapshot);
        assert!(checker.check(second.clone()).await == snapshot);
        assert_eq!(checker.requests(), vec![first, second]);
    }

    #[test]
    fn snapshot_constructors_and_wire_format_round_trip() {
        assert_eq!(Language::Rust.as_str(), "rust");
        assert_eq!(Language::Python.as_str(), "python");
        assert!(Language::Rust < Language::Python);
        let unavailable =
            ProblemSnapshot::unavailable(Language::Rust, UnavailableReason::ToolMissing, 11);
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
