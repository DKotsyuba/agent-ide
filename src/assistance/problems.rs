//! Project problem feed for `ide.context` with `kind: "problems"` (EYES-r1 §7, EYES-r2).
//!
//! This module defines the confined [`ProblemSource`](crate::assistance::problems::ProblemSource) seam the daemon answers the problems kind
//! from, plus the deterministic compact page text. The source never runs a check; it only
//! reports the latest completed snapshots for one authorized worktree, and every rendered
//! textual field is treated as untrusted checker output: single line, control characters
//! stripped, never interpreted as markdown or HTML.
//!
//! [`ProjectProblemFeed`](crate::assistance::problems::ProjectProblemFeed) is the daemon-owned
//! wiring behind that seam: it admits bound worktrees against the allowed roots, forwards
//! triggers to the check [`Scheduler`](crate::checks::scheduler::Scheduler), and renders the
//! per-binding `<agent-ide>` block from in-memory state only.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::launcher::{LauncherConfig, admit_worktree};
use crate::checks::python::PythonChecker;
use crate::checks::runner::{ConfinedRunner, SeatbeltRunner};
use crate::checks::rust::RustChecker;
use crate::checks::scheduler::{CompletionHook, Scheduler, sweep_stale_caches};
use crate::checks::{
    CheckState, Checker, Language, MAX_PROBLEMS, Problem, ProblemSnapshot, Recheck, Severity,
    UnavailableReason,
};
use crate::feed::{FeedKey, FeedState, MAX_FEED_KEYS};

/// Checks run concurrently across the daemon (EYES-r1 §5).
const MAX_CONCURRENT_CHECKS: usize = 2;

/// Native Claude tools whose completed post-hook triggers a project check (EYES-r2 §5).
pub const CHECK_TRIGGER_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];

/// Maximum problems rendered on one `ide.context` problems page.
pub const PROBLEMS_PAGE_SIZE: u32 = 20;

/// Maximum retained characters in one rendered problem's diagnostic code.
///
/// A checker code is short by convention (e.g. `E0308`); this only bounds a pathological or
/// adversarial value, matching [`untrusted_line`]'s single-line, control-free guarantee.
const MAX_CODE_CHARS: usize = 64;

/// Supplies the latest completed project check snapshots for one authorized worktree.
///
/// Implementers must be usable from the daemon worker concurrently (`Send + Sync`) and must
/// return promptly without running checks, executing processes, or blocking: the caller answers
/// a bounded `ide.context` request from this lookup alone. The returned list holds at most one
/// snapshot per configured language, in the fixed feed order (Rust before Python).
pub trait ProblemSource: Send + Sync {
    /// Returns the latest completed snapshot per configured language for `worktree`.
    ///
    /// `worktree` is the caller's authorized worktree path; a source must never report
    /// snapshots for a different worktree. An empty result means no language is configured,
    /// which the renderer reports as `checks disabled`.
    fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot>;

    /// Returns why a language's [`ProblemSource::latest`] snapshot is not the current state of
    /// the worktree: [`Recheck::FirstCheck`] when it predates the session's activation and its
    /// re-check is pending or running, [`Recheck::FilesChanged`] while a check for newer inputs
    /// is running. Defaults to none.
    fn rechecks(&self, _worktree: &Path) -> Vec<(Language, Recheck)> {
        Vec::new()
    }
}

/// Worktree bound to one activated actor binding, as recorded at `ide.start`.
struct BoundWorktree {
    /// Canonical worktree path Workspace resolved for the binding.
    worktree: PathBuf,
    /// Canonical git common dir of the worktree's repository, the scheduler repository key.
    repository_key: String,
    /// Whether the worktree was admitted under an allowed root; `false` renders `outside_roots`.
    admitted: bool,
}

/// Mutable feed wiring state, locked only for synchronous in-memory reads and writes.
#[derive(Default)]
struct FeedWiring {
    /// Bound worktrees keyed by binding fingerprint; at most [`MAX_FEED_KEYS`] entries.
    bindings: HashMap<[u8; 32], BoundWorktree>,
    /// Delivered-block state per `(binding, worktree)`.
    feed: FeedState,
}

/// Daemon-owned project problem feed: trigger routing, admission, and block delivery state.
///
/// One instance exists per daemon when project checks are configured. Every method is
/// synchronous and non-blocking except [`ProjectProblemFeed::shutdown`]; none waits for a check,
/// so hook and IPC paths can consult it inside their existing deadlines. Bindings are identified
/// by their opaque fingerprint and must be forgotten on `ide.stop`.
pub struct ProjectProblemFeed {
    /// Debounced check scheduler that owns every check run and completed snapshot.
    scheduler: Scheduler,
    /// Operator-declared allowed roots used for worktree admission.
    allowed_roots: Vec<PathBuf>,
    /// Configured languages in feed order; the scheduler runs exactly these.
    languages: Vec<Language>,
    /// Binding and delivery state.
    state: Mutex<FeedWiring>,
}

impl ProjectProblemFeed {
    /// Builds the feed around an already configured `scheduler`.
    ///
    /// `allowed_roots` must be nonempty for any worktree to be admitted; `languages` lists the
    /// languages `scheduler` has checkers for and is sorted into feed order here.
    pub fn new(
        scheduler: Scheduler,
        allowed_roots: Vec<PathBuf>,
        mut languages: Vec<Language>,
    ) -> Self {
        languages.sort();
        languages.dedup();
        Self {
            scheduler,
            allowed_roots,
            languages,
            state: Mutex::new(FeedWiring::default()),
        }
    }

    /// Builds the production feed from the launcher configuration, or `None` when disabled.
    ///
    /// Project checks are enabled only with nonempty `allowed_roots`, a `project_checks` section
    /// and at least one configured language (EYES-r1 §1); otherwise `None` leaves v0.2 behaviour
    /// unchanged. Checkers run through the Seatbelt runner with the configured timeout, the
    /// scheduler uses the configured debounce and the cache root `$HOME/.agent-ide/checks`
    /// (created `0700` best-effort), and stale caches of removed worktrees are swept first.
    /// `on_complete` observes every completed check run. Returns `None` when `HOME` is unset.
    pub fn from_launcher(launcher: &LauncherConfig, on_complete: CompletionHook) -> Option<Self> {
        let checks = launcher.project_checks()?;
        if launcher.allowed_roots().is_empty() {
            return None;
        }
        let runner: Arc<dyn ConfinedRunner> = Arc::new(SeatbeltRunner);
        let mut checkers: Vec<Arc<dyn Checker>> = Vec::new();
        if let Some(rust) = checks.rust() {
            checkers.push(Arc::new(RustChecker::new(
                runner.clone(),
                rust.toolchain_dir().to_path_buf(),
                rust.cargo_home().map(Path::to_path_buf),
                checks.check_timeout(),
                rust.developer_dir().map(Path::to_path_buf),
            )));
        }
        if let Some(python) = checks.python() {
            checkers.push(Arc::new(PythonChecker::new(
                runner.clone(),
                python.node().to_path_buf(),
                python.pyright_cli().to_path_buf(),
                checks.check_timeout(),
            )));
        }
        if checkers.is_empty() {
            return None;
        }
        let cache_root = crate::userhome::user_home()?
            .join(".agent-ide")
            .join("checks");
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cache_root);
        }
        sweep_stale_caches(&cache_root);
        let languages = checkers.iter().map(|checker| checker.language()).collect();
        let scheduler = Scheduler::new(
            checkers,
            checks.debounce(),
            MAX_CONCURRENT_CHECKS,
            cache_root,
        )
        .with_completion_hook(on_complete);
        Some(Self::new(
            scheduler,
            launcher.allowed_roots().to_vec(),
            languages,
        ))
    }

    /// Records a successful `ide.start` for `binding` and schedules the initial warm check.
    ///
    /// `worktree` is Workspace's canonical worktree path and `repository_key` its canonical git
    /// common dir. A worktree outside every allowed root is recorded as not admitted — its
    /// snapshots then report `outside_roots` — and no check is scheduled. Replaces any previous
    /// record for the same binding; when the bound set is full an arbitrary other binding is
    /// evicted first.
    pub fn activated(&self, binding: [u8; 32], worktree: &Path, repository_key: &Path) {
        let admitted = admit_worktree(&self.allowed_roots, worktree).is_ok();
        let repository_key = repository_key.to_string_lossy().into_owned();
        if admitted {
            self.scheduler.activate(&repository_key, worktree);
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if state.bindings.len() >= MAX_FEED_KEYS
            && !state.bindings.contains_key(&binding)
            && let Some(evicted) = state.bindings.keys().next().copied()
        {
            state.bindings.remove(&evicted);
        }
        state.bindings.insert(
            binding,
            BoundWorktree {
                worktree: worktree.to_path_buf(),
                repository_key,
                admitted,
            },
        );
    }

    /// Schedules a check for `binding`'s admitted worktree after a native edit or `ide.edit`.
    ///
    /// An unknown binding or a worktree outside the allowed roots schedules nothing.
    pub fn changed(&self, binding: &[u8; 32]) {
        let target = self.state.lock().ok().and_then(|state| {
            let bound = state.bindings.get(binding)?;
            bound
                .admitted
                .then(|| (bound.repository_key.clone(), bound.worktree.clone()))
        });
        if let Some((repository_key, worktree)) = target {
            self.scheduler.trigger(&repository_key, &worktree);
        }
    }

    /// Returns the `<agent-ide>` block due for `binding`, marking it delivered, or `None`.
    ///
    /// Reads only in-memory snapshots and never waits for a running check. `None` means the
    /// binding is unknown, no language has a completed result yet, or the item set equals the
    /// last block delivered to this binding for its worktree (EYES-r1 §6).
    pub fn next_block(&self, binding: &[u8; 32]) -> Option<String> {
        let mut guard = self.state.lock().ok()?;
        let state = &mut *guard;
        let bound = state.bindings.get(binding)?;
        let key = FeedKey {
            binding: hex(binding),
            worktree: bound.worktree.clone(),
        };
        let snapshots = self.snapshots(&bound.worktree, bound.admitted);
        let rechecks = self.rechecks_for(&bound.worktree);
        let block = state.feed.next_block(&key, &snapshots, &rechecks)?;
        let languages = snapshots
            .iter()
            .filter(|snapshot| {
                !matches!(
                    snapshot.state,
                    CheckState::Unavailable(UnavailableReason::Disabled)
                )
            })
            .map(|snapshot| snapshot.language.as_str())
            .collect::<Vec<_>>()
            .join(",");
        crate::errorlog::record(
            crate::errorlog::Method::Feed,
            crate::errorlog::Outcome::Completed,
            crate::errorlog::Fields {
                worktree: Some(&bound.worktree),
                detail: Some(&format!("languages={languages} bytes={}", block.len())),
                ..Default::default()
            },
        );
        Some(block)
    }

    /// Drops `binding`'s worktree record and delivery state; called on `ide.stop`.
    pub fn forget(&self, binding: &[u8; 32]) {
        if let Ok(mut state) = self.state.lock()
            && let Some(bound) = state.bindings.remove(binding)
        {
            state.feed.forget(&FeedKey {
                binding: hex(binding),
                worktree: bound.worktree,
            });
        }
    }

    /// Reports whether any check is pending or running, for the daemon idle controller.
    pub fn is_busy(&self) -> bool {
        self.scheduler.is_busy()
    }

    /// Cancels every pending and running check; triggers afterwards are ignored.
    pub async fn shutdown(&self) {
        self.scheduler.shutdown().await;
    }

    /// Returns one snapshot per configured language for `worktree`, in feed order.
    ///
    /// Not-admitted worktrees report `unavailable(outside_roots)`; a configured language without
    /// a completed result reports `checking`.
    fn snapshots(&self, worktree: &Path, admitted: bool) -> Vec<ProblemSnapshot> {
        if !admitted {
            return self
                .languages
                .iter()
                .map(|language| {
                    ProblemSnapshot::unavailable(*language, UnavailableReason::OutsideRoots, 0)
                })
                .collect();
        }
        let completed = self.scheduler.latest(worktree);
        // A language absent from the worktree never gets a `checking` placeholder: before its
        // first result a Python-only project would otherwise announce `rust: checking` (T10B).
        self.languages
            .iter()
            .filter_map(|language| {
                completed
                    .iter()
                    .find(|snapshot| snapshot.language == *language)
                    .cloned()
                    .or_else(|| {
                        language
                            .is_present(worktree)
                            .then(|| ProblemSnapshot::checking(*language, 0))
                    })
            })
            .collect()
    }

    /// Returns the languages of `worktree` whose stored result is not its current state (T18B).
    ///
    /// A result predating this session's activation is [`Recheck::FirstCheck`]; otherwise a
    /// language with a check running is [`Recheck::FilesChanged`]. A pending debounce timer alone
    /// changes nothing, so a quiet or no-op session stays silent.
    fn rechecks_for(&self, worktree: &Path) -> Vec<(Language, Recheck)> {
        let stale = self.scheduler.stale(worktree);
        let running = self.scheduler.running(worktree);
        self.languages
            .iter()
            .filter_map(|language| {
                if stale.contains(language) {
                    Some((*language, Recheck::FirstCheck))
                } else {
                    running
                        .contains(language)
                        .then_some((*language, Recheck::FilesChanged))
                }
            })
            .collect()
    }
}

impl ProblemSource for ProjectProblemFeed {
    /// Answers from the scheduler's completed snapshots, reporting `outside_roots` when a binding
    /// recorded `worktree` as not admitted and `checking` for languages without a result.
    fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot> {
        let admitted = self.state.lock().map_or(true, |state| {
            !state
                .bindings
                .values()
                .any(|bound| bound.worktree == worktree && !bound.admitted)
        });
        self.snapshots(worktree, admitted)
    }

    fn rechecks(&self, worktree: &Path) -> Vec<(Language, Recheck)> {
        self.rechecks_for(worktree)
    }
}

/// Renders a binding fingerprint as lowercase hex, the stable [`FeedKey::binding`] form.
fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses one closed `language` parameter value into its snapshot language.
///
/// Accepts exactly `"rust"` or `"python"` — the values the context schema advertises; any other
/// value returns `None` for the caller to treat as a filter-less request or a validation error.
pub fn parse_language(value: &str) -> Option<Language> {
    match value {
        "rust" => Some(Language::Rust),
        "python" => Some(Language::Python),
        _ => None,
    }
}

/// Renders the compact problems page text for the requested language filter and offset.
///
/// `language` selects one configured language, or `None` for all configured languages in feed
/// order. `offset` is the zero-based start into the combined, language-ordered problem list.
/// Each selected language contributes one state line — `ready`/`partial` carry the full
/// `errors`/`warnings` counts, while `checking` and `unavailable:<reason>` never render numeric
/// counts, though an `unavailable` line with a carried [`ProblemSnapshot::detail`] appends it in
/// parentheses — followed by up to [`PROBLEMS_PAGE_SIZE`] problem lines
/// `path:line:column severity [code] message`. A snapshot that dropped problems to the
/// [`MAX_PROBLEMS`] cap (T19B) adds one header line directly after its state line —
/// `<language>: list truncated to first MAX_PROBLEMS problems; counts above are complete` —
/// rendered on every page, so paging can never silently drop it. `next_offset: <offset + page>`
/// is appended exactly when more problems remain after the page. Empty input, or a filter matching no
/// configured language, renders the single line `checks disabled`. A language absent from the
/// worktree (T10B: `Unavailable(Disabled)`) contributes no state line and no problems, exactly as
/// it is omitted from the `<agent-ide>` block; when every matched language is absent this way,
/// the single line `no supported project detected` renders instead. Every rendered textual field
/// is stripped of control characters so each problem stays one plain, untrusted line.
pub fn problems_text(
    snapshots: &[ProblemSnapshot],
    language: Option<Language>,
    offset: u32,
) -> String {
    problems_text_with_rechecks(snapshots, &[], language, offset)
}

/// Like [`problems_text`], but a `ready`/`partial` line of a language in `rechecks` names why its
/// counts are not current (T18B): `<language>: checking (files changed); last result: errors: N;
/// warnings: M` while a check for newer inputs runs, and `<language>: checking (first check in this
/// session); previous session result: errors: N; warnings: M` for a result predating this
/// session's activation.
pub fn problems_text_with_rechecks(
    snapshots: &[ProblemSnapshot],
    rechecks: &[(Language, Recheck)],
    language: Option<Language>,
    offset: u32,
) -> String {
    let matched: Vec<&ProblemSnapshot> = snapshots
        .iter()
        .filter(|snapshot| language.is_none_or(|selected| snapshot.language == selected))
        .collect();
    if matched.is_empty() {
        return "checks disabled".to_owned();
    }
    let selected: Vec<&ProblemSnapshot> = matched
        .into_iter()
        .filter(|snapshot| {
            !matches!(
                snapshot.state,
                CheckState::Unavailable(UnavailableReason::Disabled)
            )
        })
        .collect();
    if selected.is_empty() {
        return "no supported project detected".to_owned();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut skipped = offset as usize;
    let mut rendered: u32 = 0;
    let mut more = false;
    for snapshot in &selected {
        let recheck = rechecks
            .iter()
            .find(|(recheck_language, _)| *recheck_language == snapshot.language)
            .map(|(_, recheck)| *recheck);
        lines.push(state_line(snapshot, recheck));
        if snapshot.truncated {
            // The counts in the state line are computed before the [`MAX_PROBLEMS`] cap, so only
            // the list is cut. This sits in the header next to the state line: paging skips
            // per-language problem lines, and the notice must survive every page (T19B).
            lines.push(format!(
                "{}: list truncated to first {MAX_PROBLEMS} problems; counts above are complete",
                snapshot.language.as_str()
            ));
        }
        for problem in &snapshot.problems {
            if skipped > 0 {
                skipped -= 1;
            } else if rendered < PROBLEMS_PAGE_SIZE {
                rendered += 1;
                lines.push(problem_line(problem));
            } else {
                more = true;
            }
        }
    }
    if more {
        lines.push(format!(
            "next_offset: {}",
            offset.saturating_add(PROBLEMS_PAGE_SIZE)
        ));
    }
    lines.join("\n")
}

/// Builds one language's state line, hiding the zero counts of non-reporting states.
///
/// `checking` and `unavailable` snapshots carry zero counts by construction (see `checks`);
/// those must never render as numbers, so only `ready` and `partial` name counts. An
/// `unavailable` snapshot carrying [`ProblemSnapshot::detail`] appends it in parentheses, stripped
/// of control characters like every other untrusted checker text field; a snapshot with no detail
/// renders exactly as before.
fn state_line(snapshot: &ProblemSnapshot, recheck: Option<Recheck>) -> String {
    let language = snapshot.language.as_str();
    let counts = format!(
        "errors: {}; warnings: {}",
        snapshot.errors, snapshot.warnings
    );
    match &snapshot.state {
        CheckState::Ready | CheckState::Partial if recheck == Some(Recheck::FilesChanged) => {
            format!("{language}: checking (files changed); last result: {counts}")
        }
        CheckState::Ready | CheckState::Partial if recheck == Some(Recheck::FirstCheck) => format!(
            "{language}: checking (first check in this session); previous session result: {counts}"
        ),
        CheckState::Ready => format!("{language}: ready; {counts}"),
        CheckState::Partial => format!("{language}: partial; {counts}"),
        CheckState::Checking => format!("{language}: checking (first check in this session)"),
        CheckState::Unavailable(reason) => match &snapshot.detail {
            Some(detail) => format!(
                "{language}: unavailable:{} ({})",
                unavailable_reason(*reason),
                untrusted_line(detail)
            ),
            None => format!("{language}: unavailable:{}", unavailable_reason(*reason)),
        },
    }
}

/// Renders one problem as `path:line:column severity [code] message`.
///
/// The bracketed code segment is omitted when the checker reported no code, and otherwise
/// passes through [`sanitized_code`] rather than [`untrusted_line`] directly, so it also gets a
/// length cap. All other textual fields pass through [`untrusted_line`]; the numeric fields come
/// from the typed snapshot.
fn problem_line(problem: &Problem) -> String {
    let severity = match problem.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let code = problem
        .code
        .as_deref()
        .map(|code| format!(" [{}]", sanitized_code(code)))
        .unwrap_or_default();
    format!(
        "{}:{}:{} {severity}{code} {}",
        untrusted_line(&problem.path),
        problem.line,
        problem.column,
        untrusted_line(&problem.message)
    )
}

/// Maps one unavailable reason to its closed lowercase feed identifier.
fn unavailable_reason(reason: UnavailableReason) -> &'static str {
    match reason {
        UnavailableReason::Disabled => "disabled",
        UnavailableReason::OutsideRoots => "outside_roots",
        UnavailableReason::ToolMissing => "tool_missing",
        UnavailableReason::EnvMissing => "env_missing",
        UnavailableReason::NoFiles => "no_files",
        UnavailableReason::Fatal => "fatal",
        UnavailableReason::Timeout => "timeout",
    }
}

/// Strips control characters from one untrusted checker text field.
///
/// Checker output is never trusted context: removing control characters keeps every rendered
/// problem on exactly one line and gives the text no markup, escape, or framing structure.
fn untrusted_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Sanitizes one untrusted checker diagnostic code: strips control characters and caps length.
///
/// Applies the same [`untrusted_line`] guarantee as `message`, then bounds the result to
/// [`MAX_CODE_CHARS`] characters so a pathologically long or adversarial code cannot grow the
/// rendered block unbounded.
fn sanitized_code(value: &str) -> String {
    untrusted_line(value).chars().take(MAX_CODE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one problem with a fixed code for compact test arrangements.
    fn problem(path: &str, line: u32, column: u32, severity: Severity, message: &str) -> Problem {
        Problem::new(
            path.to_owned(),
            line,
            column,
            severity,
            Some("E0001".to_owned()),
            message.to_owned(),
        )
    }

    /// Builds a ready snapshot whose counts derive from the supplied problems.
    fn ready(language: Language, problems: Vec<Problem>) -> ProblemSnapshot {
        ProblemSnapshot::from_problems(language, CheckState::Ready, problems, 1, 5)
    }

    /// Pages 45 fake problems 20 at a time with exact next_offset markers and no overrun page.
    #[test]
    fn page_boundaries_with_45_problems_emit_next_offset_exactly_twice() {
        let problems: Vec<Problem> = (1..=45)
            .map(|line| problem("src/lib.rs", line, 1, Severity::Error, "boom"))
            .collect();
        let snapshots = [ready(Language::Rust, problems)];
        assert_eq!(PROBLEMS_PAGE_SIZE, 20);

        let first = problems_text(&snapshots, None, 0);
        let first_lines: Vec<&str> = first.lines().collect();
        assert_eq!(first_lines.len(), 22, "{first}");
        assert_eq!(first_lines[0], "rust: ready; errors: 45; warnings: 0");
        assert!(first_lines[1].starts_with("src/lib.rs:1:1 error [E0001] boom"));
        assert!(first_lines[20].starts_with("src/lib.rs:20:1 error [E0001] boom"));
        assert_eq!(first_lines[21], "next_offset: 20");

        let second = problems_text(&snapshots, None, 20);
        let second_lines: Vec<&str> = second.lines().collect();
        assert_eq!(second_lines.len(), 22, "{second}");
        assert!(second_lines[1].starts_with("src/lib.rs:21:1"));
        assert!(second_lines[20].starts_with("src/lib.rs:40:1"));
        assert_eq!(second_lines[21], "next_offset: 40");

        let last = problems_text(&snapshots, None, 40);
        let last_lines: Vec<&str> = last.lines().collect();
        assert_eq!(last_lines.len(), 6, "{last}");
        assert!(last_lines[5].starts_with("src/lib.rs:45:1"));
        assert!(!last.contains("next_offset"));

        // A page past the end renders only state lines, without inventing a next offset.
        let past = problems_text(&snapshots, None, 45);
        assert_eq!(past.lines().count(), 1, "{past}");
        assert!(!past.contains("next_offset"));
    }

    /// A language filter renders only the matching configured language and its problems.
    #[test]
    fn language_filter_selects_only_the_matching_configured_language() {
        let snapshots = [
            ready(
                Language::Rust,
                vec![problem("a.rs", 1, 1, Severity::Error, "rust one")],
            ),
            ready(
                Language::Python,
                vec![problem("b.py", 2, 1, Severity::Warning, "python one")],
            ),
        ];
        let all = problems_text(&snapshots, None, 0);
        assert!(all.contains("rust: ready; errors: 1; warnings: 0"), "{all}");
        assert!(
            all.contains("python: ready; errors: 0; warnings: 1"),
            "{all}"
        );
        assert!(all.contains("a.rs:1:1 error [E0001] rust one"));
        assert!(all.contains("b.py:2:1 warning [E0001] python one"));

        let python = problems_text(&snapshots, Some(Language::Python), 0);
        assert!(python.contains("b.py:2:1 warning [E0001] python one"));
        assert!(!python.contains("rust:"), "{python}");
        assert!(!python.contains("a.rs"));

        let rust = problems_text(
            &snapshots,
            Some(parse_language("rust").expect("rust parses")),
            0,
        );
        assert!(rust.contains("a.rs:1:1 error [E0001] rust one"));
        assert!(!rust.contains("python:"), "{rust}");
        assert_eq!(parse_language("go"), None);
    }

    /// Untrusted checker text renders on exactly one line with all control characters stripped.
    #[test]
    fn untrusted_text_is_single_line_without_control_characters() {
        let snapshots = [ready(
            Language::Rust,
            vec![Problem::new(
                "a\nb.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some("E0001".to_owned()),
                "line1\r\nline2\u{0}\u{7}tail".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert_eq!(text.lines().count(), 2, "{text}");
        // The newline separators are the only permitted control positions: every rendered line
        // is itself free of control characters, so a problem cannot forge lines or framing.
        for line in text.lines() {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert!(
            text.contains("ab.rs:1:1 error [E0001] line1line2tail"),
            "{text}"
        );
    }

    /// An empty snapshot list and a filter matching no configured language both report disabled.
    #[test]
    fn unconfigured_feed_renders_checks_disabled() {
        assert_eq!(problems_text(&[], None, 0), "checks disabled");
        let snapshots = [ready(Language::Rust, Vec::new())];
        assert_eq!(
            problems_text(&snapshots, Some(Language::Python), 0),
            "checks disabled"
        );
    }

    /// A snapshot over the [`MAX_PROBLEMS`] cap (T19B) announces the truncation in the header
    /// right after its state line — with complete counts, which are computed before the cap — on
    /// every page, so paging cannot silently drop the notice.
    #[test]
    fn truncated_snapshot_announces_the_cap_next_to_the_state_line_on_every_page() {
        let problems: Vec<Problem> = (0..=MAX_PROBLEMS as u32)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        let snapshot =
            ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, problems, 1, 1);
        assert!(snapshot.truncated);
        let notice = format!(
            "rust: list truncated to first {MAX_PROBLEMS} problems; counts above are complete"
        );

        let first = problems_text(std::slice::from_ref(&snapshot), None, 0);
        let first_lines: Vec<&str> = first.lines().collect();
        assert_eq!(
            first_lines[0], "rust: ready; errors: 501; warnings: 0",
            "{first}"
        );
        assert_eq!(first_lines[1], notice, "{first}");
        assert_eq!(first_lines.len(), 23, "{first}");

        // The notice survives paging, including a page past the end of the retained list.
        let paged = problems_text(&[snapshot], None, 40);
        assert_eq!(paged.lines().nth(1), Some(notice.as_str()), "{paged}");
    }

    /// An under-cap snapshot (T19B) renders no truncation notice.
    #[test]
    fn under_cap_snapshot_renders_no_truncation_notice() {
        let snapshots = [ready(
            Language::Rust,
            vec![problem("a.rs", 1, 1, Severity::Error, "e")],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert!(!text.contains("list truncated"), "{text}");
    }

    /// Non-reporting states never render numeric counts and unavailable reasons stay closed.
    #[test]
    fn lifecycle_states_render_without_counts_and_closed_reasons() {
        let snapshots = [
            ProblemSnapshot::from_problems(
                Language::Rust,
                CheckState::Partial,
                vec![problem("a.rs", 1, 1, Severity::Error, "e")],
                1,
                5,
            ),
            ProblemSnapshot::checking(Language::Python, 3),
        ];
        let text = problems_text(&snapshots, None, 0);
        assert!(
            text.contains("rust: partial; errors: 1; warnings: 0"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line == "python: checking (first check in this session)"),
            "{text}"
        );
        assert!(
            !text.contains("python: checking (first check in this session);"),
            "{text}"
        );

        for (reason, rendered) in [
            (
                UnavailableReason::OutsideRoots,
                "rust: unavailable:outside_roots",
            ),
            (
                UnavailableReason::ToolMissing,
                "rust: unavailable:tool_missing",
            ),
            (
                UnavailableReason::EnvMissing,
                "rust: unavailable:env_missing",
            ),
            (UnavailableReason::NoFiles, "rust: unavailable:no_files"),
            (UnavailableReason::Fatal, "rust: unavailable:fatal"),
            (UnavailableReason::Timeout, "rust: unavailable:timeout"),
        ] {
            let snapshots = [ProblemSnapshot::unavailable(Language::Rust, reason, 1)];
            assert_eq!(problems_text(&snapshots, None, 0), rendered);
        }
    }

    /// A language absent from the worktree (T10B: `Unavailable(Disabled)`) contributes no state
    /// line at all; when every matched language is absent this way, the page renders the single
    /// line `no supported project detected` rather than `checks disabled` (which stays reserved
    /// for no configured language / no filter match).
    #[test]
    fn absent_language_is_omitted_and_all_absent_reports_no_supported_project() {
        let rust_only = [ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::Disabled,
            1,
        )];
        assert_eq!(
            problems_text(&rust_only, None, 0),
            "no supported project detected"
        );

        let mixed = [
            ProblemSnapshot::unavailable(Language::Rust, UnavailableReason::Disabled, 1),
            ready(
                Language::Python,
                vec![problem("b.py", 2, 1, Severity::Warning, "python one")],
            ),
        ];
        let text = problems_text(&mixed, None, 0);
        assert!(!text.contains("rust"), "{text}");
        assert!(
            text.contains("python: ready; errors: 0; warnings: 1"),
            "{text}"
        );
    }

    /// An `unavailable` snapshot carrying a detail appends it in parentheses, stripped of control
    /// characters (T05B); a snapshot with no detail renders exactly as before.
    #[test]
    fn unavailable_detail_renders_in_parentheses_and_strips_control_characters() {
        let with_detail = [ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            1,
            Some("error: failed to run custom build command for `blake3 v1.5.0`".to_owned()),
        )];
        assert_eq!(
            problems_text(&with_detail, None, 0),
            "rust: unavailable:fatal (error: failed to run custom build command for `blake3 v1.5.0`)"
        );

        let with_control_chars = [ProblemSnapshot::unavailable_with_detail(
            Language::Rust,
            UnavailableReason::Fatal,
            1,
            Some("error: line one\nline two <agent-ide>x</agent-ide>".to_owned()),
        )];
        let text = problems_text(&with_control_chars, None, 0);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(
            text,
            "rust: unavailable:fatal (error: line oneline two <agent-ide>x</agent-ide>)"
        );

        let without_detail = [ProblemSnapshot::unavailable(
            Language::Rust,
            UnavailableReason::Fatal,
            1,
        )];
        assert_eq!(
            problems_text(&without_detail, None, 0),
            "rust: unavailable:fatal"
        );
    }

    /// A pyright run that analyzed zero files (T12B) renders `unavailable:no_files` with a detail
    /// pointing at the project's `include`/`exclude` configuration, distinct from `env_missing`.
    #[test]
    fn python_no_files_analyzed_renders_with_include_exclude_detail() {
        let snapshots = [ProblemSnapshot::unavailable_with_detail(
            Language::Python,
            UnavailableReason::NoFiles,
            1,
            Some(
                "pyright analyzed 0 files; check \"include\"/\"exclude\" in pyrightconfig.json \
                 or [tool.pyright]"
                    .to_owned(),
            ),
        )];
        assert_eq!(
            problems_text(&snapshots, None, 0),
            "python: unavailable:no_files (pyright analyzed 0 files; check \"include\"/\"exclude\" \
             in pyrightconfig.json or [tool.pyright])"
        );
    }

    /// An untrusted checker code renders on one line, with control characters stripped so an
    /// embedded newline cannot forge a second line or fake framing tags.
    #[test]
    fn untrusted_code_is_single_line_without_control_characters_or_tags() {
        let snapshots = [ready(
            Language::Rust,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some("E0308\n<agent-ide>x</agent-ide>".to_owned()),
                "boom".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert_eq!(text.lines().count(), 2, "{text}");
        for line in text.lines() {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        let problem_line = text.lines().nth(1).expect("problem line present");
        assert!(!problem_line.contains('\n'), "{problem_line}");
        assert_eq!(
            problem_line,
            "a.rs:1:1 error [E0308<agent-ide>x</agent-ide>] boom"
        );
    }

    /// An overlong checker code is capped at [`MAX_CODE_CHARS`] rather than growing the block.
    #[test]
    fn overlong_code_is_capped_at_max_code_chars() {
        let long_code: String = (0..100u32)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect();
        let snapshots = [ready(
            Language::Rust,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some(long_code.clone()),
                "boom".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        let expected_code: String = long_code.chars().take(MAX_CODE_CHARS).collect();
        assert_eq!(
            text.lines().nth(1).expect("problem line present"),
            format!("a.rs:1:1 error [{expected_code}] boom")
        );
    }

    /// A problem without a code renders without an empty bracket segment.
    #[test]
    fn missing_code_omits_the_bracket_segment() {
        let snapshots = [ready(
            Language::Rust,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Warning,
                None,
                "plain".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert!(text.contains("a.rs:1:1 warning plain"), "{text}");
        assert!(!text.contains("[]"), "{text}");
    }

    /// Advances the paused clock in small steps so chained debounce and run timers all fire.
    async fn settle() {
        for _ in 0..40 {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Checker whose result the test swaps between runs and whose runs take 200 ms of paused time,
    /// so a running check is observable; stamps the request's generation like the real checkers.
    struct ScriptedChecker(Arc<Mutex<Vec<Problem>>>);

    impl crate::checks::Checker for ScriptedChecker {
        fn language(&self) -> Language {
            Language::Rust
        }

        fn check(
            &self,
            request: crate::checks::CheckRequest,
        ) -> crate::checks::BoxFuture<'_, ProblemSnapshot> {
            let problems = self.0.lock().unwrap().clone();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                ProblemSnapshot::from_problems(
                    Language::Rust,
                    CheckState::Ready,
                    problems,
                    request.input_generation,
                    1,
                )
            })
        }
    }

    /// Builds a feed over a [`ScriptedChecker`] and returns it with its problem list and root.
    fn scripted_feed(name: &str) -> (ProjectProblemFeed, Arc<Mutex<Vec<Problem>>>, PathBuf) {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/Cargo.toml"), "[package]\n").unwrap();
        let problems = Arc::new(Mutex::new(Vec::new()));
        let scheduler = Scheduler::new(
            vec![Arc::new(ScriptedChecker(Arc::clone(&problems)))],
            std::time::Duration::from_millis(50),
            2,
            root.join("cache"),
        );
        let feed = ProjectProblemFeed::new(scheduler, vec![root.clone()], vec![Language::Rust]);
        (feed, problems, root)
    }

    /// Advances paused time until the feed sees a check running for `worktree`.
    async fn until_running(feed: &ProjectProblemFeed, worktree: &Path) {
        for _ in 0..100 {
            if !feed.scheduler.running(worktree).is_empty() {
                return;
            }
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        }
        panic!("no check started");
    }

    /// Wraps one status line in the block tags.
    fn plate(line: &str) -> Option<String> {
        Some(format!("<agent-ide>\n{line}\n</agent-ide>"))
    }

    /// A first call after a re-activation must not present the previous inputs' counts as current:
    /// the problems text says the first check of this session is running, the plate says so too,
    /// and both follow the normal rules once the new result lands.
    #[tokio::test(start_paused = true)]
    async fn activation_over_an_older_snapshot_is_stale_until_the_recheck_lands() {
        let (feed, _problems, root) = scripted_feed("stale");
        let worktree = root.join("wt");
        feed.activated([1; 32], &worktree, Path::new("repo"));
        settle().await;
        assert!(feed.rechecks(&worktree).is_empty());
        assert!(feed.next_block(&[1; 32]).is_some());

        feed.activated([2; 32], &worktree, Path::new("repo"));
        let rechecks = feed.rechecks(&worktree);
        assert_eq!(rechecks, vec![(Language::Rust, Recheck::FirstCheck)]);
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &rechecks, None, 0),
            "rust: checking (first check in this session); previous session result: errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&[2; 32]),
            plate("rust: checking (first check)"),
            "previous session counts must not be presented as current"
        );

        settle().await;
        assert!(feed.rechecks(&worktree).is_empty());
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &[], None, 0),
            "rust: ready; errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&[2; 32]),
            plate("rust: 0 errors, 0 warnings")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T18B status plate over a whole session: a quiet session emits one plate per state change;
    /// an armed debounce timer alone (a possible no-op) emits nothing; a running check emits one
    /// `checking (…last result…)` plate and its result one more, with deltas; a no-op check
    /// returns the plate to its previous text exactly once.
    #[tokio::test(start_paused = true)]
    async fn status_plate_follows_activation_edit_noop_and_fix() {
        let (feed, problems, root) = scripted_feed("t18b");
        let worktree = root.join("wt");
        let hook = [1; 32];
        feed.activated(hook, &worktree, Path::new("repo"));
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: checking (first check)")
        );
        assert_eq!(feed.next_block(&hook), None);
        settle().await;
        assert_eq!(feed.next_block(&hook), plate("rust: 0 errors, 0 warnings"));
        assert_eq!(feed.next_block(&hook), None);

        // Quiet session: hook events re-arm the timer, but nothing runs or changes yet.
        feed.changed(&hook);
        feed.changed(&hook);
        assert!(feed.rechecks(&worktree).is_empty());
        assert_eq!(feed.next_block(&hook), None);
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &[], None, 0),
            "rust: ready; errors: 0; warnings: 0"
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            None,
            "a no-op check changes nothing"
        );

        // A no-op check observed while running: checking plate, then the old text once.
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        let rechecks = feed.rechecks(&worktree);
        assert_eq!(rechecks, vec![(Language::Rust, Recheck::FilesChanged)]);
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &rechecks, None, 0),
            "rust: checking (files changed); last result: errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: checking (files changed; last result: 0 errors, 0 warnings)")
        );
        assert_eq!(feed.next_block(&hook), None);
        settle().await;
        assert_eq!(feed.next_block(&hook), plate("rust: 0 errors, 0 warnings"));
        assert_eq!(feed.next_block(&hook), None);

        // Edit adds a warning.
        problems
            .lock()
            .unwrap()
            .push(problem("a.rs", 1, 1, Severity::Warning, "careful"));
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: checking (files changed; last result: 0 errors, 0 warnings)")
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: 0 errors, 1 warning (+1)")
        );
        assert_eq!(feed.next_block(&hook), None);

        // Fix.
        problems.lock().unwrap().clear();
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: checking (files changed; last result: 0 errors, 1 warning)")
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            plate("rust: 0 errors, 0 warnings (-1)")
        );
        assert_eq!(feed.next_block(&hook), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
