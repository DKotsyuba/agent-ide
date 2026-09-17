//! Project problem feed for `ide.context` with `kind: "problems"` (EYES-r1 §7, EYES-r2).
//!
//! This module defines the confined [`ProblemSource`] seam the daemon answers the problems kind
//! from, plus the deterministic compact page text. The source never runs a check; it only
//! reports the latest completed snapshots for one authorized worktree, and every rendered
//! textual field is treated as untrusted checker output: single line, control characters
//! stripped, never interpreted as markdown or HTML.

use std::path::Path;

use crate::checks::{CheckState, Language, Problem, ProblemSnapshot, Severity, UnavailableReason};

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
/// counts — followed by up to [`PROBLEMS_PAGE_SIZE`] problem lines
/// `path:line:column severity [code] message`. `next_offset: <offset + page>` is appended
/// exactly when more problems remain after the page. Empty input, or a filter matching no
/// configured language, renders the single line `checks disabled`. Every rendered textual field
/// is stripped of control characters so each problem stays one plain, untrusted line.
pub fn problems_text(
    snapshots: &[ProblemSnapshot],
    language: Option<Language>,
    offset: u32,
) -> String {
    let selected: Vec<&ProblemSnapshot> = snapshots
        .iter()
        .filter(|snapshot| language.is_none_or(|selected| snapshot.language == selected))
        .collect();
    if selected.is_empty() {
        return "checks disabled".to_owned();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut skipped = offset as usize;
    let mut rendered: u32 = 0;
    let mut more = false;
    for snapshot in &selected {
        lines.push(state_line(snapshot));
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
/// those must never render as numbers, so only `ready` and `partial` name counts.
fn state_line(snapshot: &ProblemSnapshot) -> String {
    let language = snapshot.language.as_str();
    match &snapshot.state {
        CheckState::Ready => format!(
            "{language}: ready; errors: {}; warnings: {}",
            snapshot.errors, snapshot.warnings
        ),
        CheckState::Partial => format!(
            "{language}: partial; errors: {}; warnings: {}",
            snapshot.errors, snapshot.warnings
        ),
        CheckState::Checking => format!("{language}: checking"),
        CheckState::Unavailable(reason) => {
            format!("{language}: unavailable:{}", unavailable_reason(*reason))
        }
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
            text.lines().any(|line| line == "python: checking"),
            "{text}"
        );
        assert!(!text.contains("python: checking;"), "{text}");

        for (reason, rendered) in [
            (UnavailableReason::Disabled, "rust: unavailable:disabled"),
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
            (UnavailableReason::Fatal, "rust: unavailable:fatal"),
            (UnavailableReason::Timeout, "rust: unavailable:timeout"),
        ] {
            let snapshots = [ProblemSnapshot::unavailable(Language::Rust, reason, 1)];
            assert_eq!(problems_text(&snapshots, None, 0), rendered);
        }
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
}
