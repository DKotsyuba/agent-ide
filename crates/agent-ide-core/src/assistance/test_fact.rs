//! The terminal text of one settled `ide.test` run: a deterministic, byte-bounded fact.
//!
//! This module is the canonical implementation of `docs/contracts/reply-hygiene-v1.md` §1
//! (boundary `M024-M026-TEST-FACT`). The same text is the settled reply body of the run and the
//! body of the pushed `test-finished` fact that the session outbox delivers, so a reply and a
//! pushed fact never disagree. A run that is still running has no terminal text at all.

use super::{
    tests::{RunResult, runner_excerpt, summary_absent},
    worker::rerun_text,
};

/// Longest summary line: the outcome in plain words (counts or failure reason, duration and
/// environment label).
const MAX_SUMMARY_BYTES: usize = 256;

/// Longest `summary` plus `detail` of one fact: the `test-finished` body limit of the session
/// outbox contract.
const MAX_FACT_BYTES: usize = 3584;

/// Most `FAIL` entries a fact lists; the rest are counted in one `+K more` line.
const MAX_FAILURES: usize = 8;

/// Longest failing-test name on its `FAIL` line.
const MAX_NAME_BYTES: usize = 120;

/// Longest `file:line  message` line of one `FAIL` entry.
const MAX_MESSAGE_LINE_BYTES: usize = 160;

/// Longest path shown in a `FAIL` entry's location before the line number.
const MAX_PATH_BYTES: usize = 128;

/// Longest displayed rerun command; clipped or escaped commands are explicitly previews.
const MAX_RERUN_BYTES: usize = 300;

/// Longest runner line quoted in the summary of a run that counted no test.
const MAX_EXCERPT_BYTES: usize = 200;

/// Most output lines an exit-only fact quotes from the end of the runner output.
const MAX_TAIL_LINES: usize = 10;

/// Largest total size of the quoted output lines of an exit-only fact.
const MAX_TAIL_BYTES: usize = 1024;

/// Longest single quoted output line of an exit-only fact.
const MAX_TAIL_LINE_BYTES: usize = 160;

/// Longest per-target line of a several-target run (batch-api-v1 §6).
const MAX_TARGET_LINE_BYTES: usize = 160;

/// The terminal outcome of one settled run, split the way the outbox carries it.
///
/// Both parts are plain text without control characters. `summary` is one line of at most
/// [`MAX_SUMMARY_BYTES`] bytes and `summary` plus `detail` never exceed [`MAX_FACT_BYTES`] bytes.
/// `detail` holds every further line, each already indented by two spaces, joined by `\n`; it is
/// never empty because the `rerun:` line is always present.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalFact {
    /// The outcome in one line, for example `3 passed, 1 failed, 12 s`.
    pub summary: String,
    /// The `FAIL` entries, the `+K more` line, one line per target of a several-target run, the
    /// rerun or preview line, the owner-only full replay link when needed and the output link.
    pub detail: String,
}

impl TerminalFact {
    /// Renders the settled `ide.test` reply body of run `id`: `tests #<id>: <summary>`, a
    /// newline, then the detail lines. The outbox renders the same two parts under the
    /// `test-finished tests #<id>:` label.
    pub(crate) fn reply_text(&self, id: u64) -> String {
        format!("tests #{id}: {}\n{}", self.summary, self.detail)
    }
}

/// The sentence that ends a started line: the long run stays retained under its reference and
/// Bridge delivers it automatically (session outbox contract §8.7).
const RESULT_ARRIVES: &str =
    "; the result is retained under its reference; Bridge delivers it automatically";

/// Renders the reply to a successful `ide.test` start of run `id`: the command the run executes
/// (`command`, already shell-quoted for display), the optional count of selected tests
/// (`selected`, for example `4 tests selected`) and the run's time budget in seconds. The line
/// carries neither elapsed time nor an instruction to call anything.
pub(crate) fn started_line(
    id: u64,
    command: &str,
    selected: Option<&str>,
    budget_seconds: u64,
) -> String {
    let selected = selected.map_or_else(String::new, |summary| format!(" ({summary})"));
    format!(
        "tests #{id}: started — {command}{selected} (budget {budget_seconds} s){RESULT_ARRIVES}"
    )
}

/// Renders what a read of run `id` answers while the run is still active: the bare state, with no
/// elapsed seconds and no hint, because the terminal fact arrives once without being asked for.
pub(crate) fn running_line(id: u64) -> String {
    format!("tests #{id}: running")
}

/// Renders the answer to a second `ide.test` start while run `id` is still active in the worktree.
pub(crate) fn still_running_line(id: u64) -> String {
    format!("tests #{id}: still running")
}

/// One failing test as shown in a fact: its name line and the optional location and message line.
struct Entry {
    /// The failing test's name, cleaned and cut to [`MAX_NAME_BYTES`].
    name: String,
    /// `file:line  message` (or the message alone), cleaned and cut to [`MAX_MESSAGE_LINE_BYTES`];
    /// absent when the runner gave neither location nor message.
    message: Option<String>,
}

/// Renders the terminal text of the settled run `result`.
///
/// `_id` is the daemon-local run number; the text itself carries no run number because the
/// caller adds it (`tests #<id>` in a reply, the fact subject in the outbox). `owns_detail` says
/// whether the recipient started the run: only the owner may read the run's full output, so only
/// the owner's text ends with `full output: ide.test {detail_ref}` (and only when the run kept
/// any output).
///
/// The summary is chosen from the run's state, first match wins:
/// - owner cancellation: `cancelled — P passed, F failed so far (exit C)`;
/// - the budget killed the run: `stopped at budget B s — P passed, F failed so far (exit C)`;
///   either state says `test counts unavailable` or `exit unavailable` when not observed;
/// - the trusted runner prelude refused: `environment unavailable (exit 78)`, with its cause;
/// - no recognised summary, so only the exit status is known: `exit C, S s`, followed by the
///   last output lines;
/// - a non-zero exit with no counted test: `no test results (exit C), S s — runner said: …`;
/// - otherwise `P passed, F failed, S s (exit C)`, or explicit unknown exit after a signal.
///
/// The detail lists at most [`MAX_FAILURES`] failing tests, one line per target of a
/// several-target run (cut at [`MAX_TARGET_LINE_BYTES`]), then the rerun command (cut at
/// [`MAX_RERUN_BYTES`]) and the full-output reference. When the whole text would exceed
/// [`MAX_FACT_BYTES`], quoted output lines go first, then message
/// lines from the last entry backwards, then entries from the end (the `+K more` count grows);
/// the summary, the rerun line and the reference are never dropped. The result depends only on
/// the arguments: no clock, no I/O.
pub(crate) fn terminal_fact(_id: u64, result: &RunResult, owns_detail: bool) -> TerminalFact {
    let summary = summary_text(result);
    let tail = if summary_absent(result) {
        output_tail_lines(&result.output)
    } else {
        Vec::new()
    };
    let entries = failure_entries(result);
    let targets = target_lines(result);
    let closing = closing_lines(result, owns_detail);

    let mut shown = entries.len().min(MAX_FAILURES);
    let mut messages = shown;
    let mut tail_lines = tail.len();
    let target_lines = targets.len();
    loop {
        let mut lines = Vec::new();
        for (index, entry) in entries.iter().take(shown).enumerate() {
            lines.push(format!("  FAIL {}", entry.name));
            if index < messages
                && let Some(message) = &entry.message
            {
                lines.push(format!("       {message}"));
            }
        }
        if entries.len() > shown {
            lines.push(format!(
                "  (+{} more failed tests in the full output)",
                entries.len() - shown
            ));
        }
        lines.extend(targets.iter().take(target_lines).cloned());
        if tail_lines > 0 {
            lines.push("  output (tail):".to_owned());
            lines.extend(
                tail[tail.len() - tail_lines..]
                    .iter()
                    .map(|line| format!("    {line}")),
            );
        }
        lines.extend(closing.iter().cloned());
        let detail = lines.join("\n");
        if summary.len() + detail.len() <= MAX_FACT_BYTES || (shown == 0 && tail_lines == 0) {
            return TerminalFact { summary, detail };
        }
        // Shrink in contract order: quoted output first (it is the least specific), then the
        // message lines from the last entry backwards, then whole entries from the end.
        // Per-target exit outcomes are required and are never dropped.
        if tail_lines > 0 {
            tail_lines -= 1;
        } else if messages > 0 {
            messages -= 1;
        } else {
            shown -= 1;
        }
    }
}

/// Renders the one-line summary of the settled run `result`, cut to [`MAX_SUMMARY_BYTES`].
///
/// The settled reply body of a run without a recognised summary uses the same line, so the reply
/// and the pushed fact open identically.
pub(super) fn summary_text(result: &RunResult) -> String {
    let report = &result.report;
    let seconds = result.elapsed.as_secs();
    let counted = report.passed != 0 || report.failed != 0;
    let partial_counts = if summary_absent(result) {
        "test counts unavailable".to_owned()
    } else {
        format!("{} passed, {} failed so far", report.passed, report.failed)
    };
    let exit = result
        .exit
        .map_or_else(|| "unavailable".to_owned(), |code| code.to_string());
    let mut base = if result.cancelled {
        format!("cancelled — {partial_counts} (exit {exit})")
    } else if result.stopped {
        format!(
            "stopped at budget {} s — {partial_counts} (exit {exit})",
            result.budget.as_secs(),
        )
    } else if let Some(cause) = result.environment_unavailable() {
        format!(
            "environment unavailable (exit {exit}), {seconds} s — {}",
            clean(cause, MAX_EXCERPT_BYTES)
        )
    } else if summary_absent(result) {
        let exit = result
            .exit
            .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
        format!("exit {exit}, {seconds} s")
    } else if let Some(code) = result.exit.filter(|code| *code != 0 && !counted) {
        format!(
            "no test results (exit {code}), {seconds} s — runner said: {}",
            clean(&runner_excerpt(&result.output), MAX_EXCERPT_BYTES)
        )
    } else {
        format!(
            "{} passed, {} failed, {seconds} s (exit {})",
            report.passed,
            report.failed,
            result
                .exit
                .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        )
    };
    if report.incomplete
        && counted
        && result
            .targets
            .iter()
            .any(|target| target.contains("environment unavailable (exit 78)"))
    {
        base.push_str("; partial counts, another target's environment unavailable");
    }
    if let Some(label) = &result.environment_label {
        base.push_str(" · env ");
        base.push_str(label);
    }
    clean(&base, MAX_SUMMARY_BYTES)
}

/// Formats one one-based target (1–8) while keeping its terminal outcome intact. Labels and
/// outcomes are separate inputs, so colons in commands or failure reasons cannot change the
/// boundary. Untrusted text is sanitized and long labels yield space to the bounded outcome.
pub(super) fn target_outcome(index: usize, label: &str, outcome: &str) -> String {
    let head = format!("target {index} ");
    let outcome = clean(outcome, MAX_TARGET_LINE_BYTES - 32);
    let label = clean(
        label,
        MAX_TARGET_LINE_BYTES - head.len() - outcome.len() - 2,
    );
    format!("{head}{label}: {outcome}")
}

/// Indents the retained target outcomes, defensively sanitizing/clipping older or synthetic
/// rows to the same per-line budget. Production rows come from [`target_outcome`].
pub(super) fn target_lines(result: &RunResult) -> Vec<String> {
    result
        .targets
        .iter()
        .map(|line| format!("  {}", clean(line, MAX_TARGET_LINE_BYTES)))
        .collect()
}

/// Renders a runnable short rerun or an explicitly clipped/escaped preview, and owner-only
/// recovery links. Full replay JSON has its own retained cursor, including for empty stdout;
/// raw output remains on its existing cursor. No invocation started means no rerun is invented.
pub(super) fn closing_lines(result: &RunResult, owns_detail: bool) -> Vec<String> {
    let rerun = rerun_text(result);
    let preview = rerun.len() > MAX_RERUN_BYTES
        || result
            .command
            .iter()
            .chain(result.rerun_dir.iter())
            .any(|arg| arg.chars().any(char::is_control));
    let mut closing = vec![format!(
        "  rerun{}: {}",
        if preview { " preview" } else { "" },
        if result.command.is_empty() {
            "unavailable (no invocation started)".into()
        } else {
            cut_with_ellipsis(&rerun, MAX_RERUN_BYTES)
        }
    )];
    if preview && owns_detail && !result.command.is_empty() {
        closing.push(format!(
            "  full replay: ide.test {}",
            serde_json::json!({"output":format!("{}/replay/output/1",result.detail_ref)})
        ));
    }
    if owns_detail && (!result.output.is_empty() || result.output_paged) {
        closing.push(format!(
            "  full output: ide.test {}",
            serde_json::json!({"detail_ref":format!("{}/output/1", result.detail_ref)})
        ));
    }
    closing
}

/// Collects the failing tests of `result` that a fact may show: the first [`MAX_FAILURES`] ones
/// are rendered, and the count of all of them feeds the `+K more` line.
fn failure_entries(result: &RunResult) -> Vec<Entry> {
    result
        .report
        .failures
        .iter()
        .map(|failure| {
            let text = match &failure.location {
                Some((path, line)) => format!(
                    "{}:{}  {}",
                    clean(&path.display().to_string(), MAX_PATH_BYTES),
                    line,
                    clean(&failure.message, MAX_MESSAGE_LINE_BYTES)
                ),
                None => clean(&failure.message, MAX_MESSAGE_LINE_BYTES),
            };
            let text = clean(&text, MAX_MESSAGE_LINE_BYTES);
            Entry {
                name: clean(&failure.name, MAX_NAME_BYTES),
                message: (!text.trim().is_empty()).then_some(text),
            }
        })
        .collect()
}

/// Returns the last non-empty lines of the runner `output` that an exit-only fact quotes, oldest
/// first: at most [`MAX_TAIL_LINES`] lines, [`MAX_TAIL_BYTES`] bytes in total, each cleaned and
/// cut to [`MAX_TAIL_LINE_BYTES`].
fn output_tail_lines(output: &str) -> Vec<String> {
    let mut kept = Vec::new();
    let mut bytes = 0;
    for line in output.lines().rev() {
        let line = clean(line.trim_end(), MAX_TAIL_LINE_BYTES);
        if line.trim().is_empty() {
            continue;
        }
        if kept.len() == MAX_TAIL_LINES || bytes + line.len() > MAX_TAIL_BYTES {
            break;
        }
        bytes += line.len();
        kept.push(line);
    }
    kept.reverse();
    kept
}

/// Removes control characters from untrusted runner `value` and cuts it to at most `max_bytes`
/// bytes on a character boundary.
fn clean(value: &str, max_bytes: usize) -> String {
    let mut cleaned = String::new();
    for character in value.chars().filter(|character| !character.is_control()) {
        if cleaned.len() + character.len_utf8() > max_bytes {
            break;
        }
        cleaned.push(character);
    }
    cleaned
}

/// Cuts `value` to at most `max_bytes` bytes; a longer value keeps a prefix and ends with `…`,
/// so the shortened text is never mistaken for the whole.
fn cut_with_ellipsis(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut cut = clean(value, max_bytes - '…'.len_utf8());
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use super::*;
    use crate::{
        assistance::outbox::{FactDraft, FactKind},
        lang::{TestFailure, TestReport},
    };

    /// A settled run with the given counts and failures, one second long, rerunnable as
    /// `cargo test`, owning a retained output.
    fn run(passed: u32, failed: u32, failures: Vec<TestFailure>) -> RunResult {
        RunResult {
            report: TestReport {
                passed,
                failed,
                ignored: 0,
                failures,
                incomplete: false,
            },
            output: "runner output\n".to_owned(),
            output_paged: false,
            elapsed: Duration::from_secs(12),
            stopped: false,
            cancelled: false,
            exit: Some(if failed == 0 { 0 } else { 101 }),
            budget: Duration::from_secs(120),
            detail_ref: "ref-3".to_owned(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            rerun_dir: None,
            environment_label: None,
            targets: Vec::new(),
        }
    }

    /// A failing test named `name` at `src/a.rs:<line>` with `message`.
    fn failure(name: &str, line: u32, message: &str) -> TestFailure {
        TestFailure {
            name: name.to_owned(),
            location: Some((PathBuf::from("src/a.rs"), line)),
            message: message.to_owned(),
        }
    }

    /// Every fact stays in budget, has one summary line and ends with the rerun line, and the
    /// session outbox accepts it as a `test-finished` draft the way M-026 builds one.
    fn assert_in_contract(fact: &TerminalFact) {
        let draft = FactDraft {
            kind: FactKind::TestFinished,
            subject: "tests #3".to_owned(),
            summary: fact.summary.clone(),
            detail: Some(fact.detail.clone()),
            detail_ref: Some("ref-3".to_owned()),
            failed: true,
            source_bearing: false,
            fence_paths: Vec::new(),
        };
        assert_eq!(draft.validate(), Ok(()), "{fact:?}");
        assert!(fact.summary.len() <= MAX_SUMMARY_BYTES, "{fact:?}");
        assert!(!fact.summary.contains('\n'), "{fact:?}");
        assert!(
            fact.summary.len() + fact.detail.len() <= MAX_FACT_BYTES,
            "{} bytes: {fact:?}",
            fact.summary.len() + fact.detail.len()
        );
        assert!(
            fact.detail
                .lines()
                .any(|line| line.starts_with("  rerun: ") || line.starts_with("  rerun preview: "))
        );
        for banned in [
            "poll",
            "every few seconds",
            "in a few seconds",
            "until it completes",
        ] {
            assert!(
                !fact.summary.contains(banned) && !fact.detail.contains(banned),
                "{banned}: {fact:?}"
            );
        }
    }

    /// A run with only counts reads as one summary line, a rerun line and the output reference.
    #[test]
    fn counted_run_is_summary_rerun_and_reference() {
        let fact = terminal_fact(3, &run(3, 0, Vec::new()), true);
        assert_eq!(fact.summary, "3 passed, 0 failed, 12 s (exit 0)");
        assert_eq!(
            fact.detail,
            "  rerun: cargo test\n  full output: ide.test {\"detail_ref\":\"ref-3/output/1\"}"
        );
        assert_eq!(
            fact.reply_text(3),
            "tests #3: 3 passed, 0 failed, 12 s (exit 0)\n  rerun: cargo test\n  full output: ide.test {\"detail_ref\":\"ref-3/output/1\"}"
        );
        assert_in_contract(&fact);
    }

    /// One failure shows its name, its location and message, then the rerun and reference lines.
    #[test]
    fn one_failure_lists_name_location_and_message() {
        let fact = terminal_fact(
            3,
            &run(2, 1, vec![failure("tests::fails", 40, "assertion failed")]),
            true,
        );
        assert_eq!(fact.summary, "2 passed, 1 failed, 12 s (exit 101)");
        assert_eq!(
            fact.detail,
            "  FAIL tests::fails\n       src/a.rs:40  assertion failed\n  rerun: cargo test\n  full output: ide.test {\"detail_ref\":\"ref-3/output/1\"}"
        );
        assert_in_contract(&fact);
    }

    /// Twenty long failures keep eight entries at most, count the rest and stay in budget while
    /// the rerun and reference lines survive.
    #[test]
    fn many_long_failures_are_capped_counted_and_bounded() {
        let long_name = "module::".repeat(40);
        let long_message = "left: Vec<u8> right: Vec<u8> ".repeat(20);
        let failures = (0..20)
            .map(|index| failure(&format!("{long_name}{index}"), index, &long_message))
            .collect();
        let fact = terminal_fact(3, &run(0, 20, failures), true);
        assert_in_contract(&fact);
        let entries = fact.detail.matches("\n  FAIL ").count()
            + usize::from(fact.detail.starts_with("  FAIL "));
        assert!(entries <= MAX_FAILURES, "{fact:?}");
        assert!(entries >= 1, "{fact:?}");
        assert!(
            fact.detail.contains(&format!(
                "(+{} more failed tests in the full output)",
                20 - entries
            )),
            "{fact:?}"
        );
        assert!(fact.detail.contains("  rerun: cargo test"), "{fact:?}");
        assert!(
            fact.detail
                .ends_with("  full output: ide.test {\"detail_ref\":\"ref-3/output/1\"}"),
            "{fact:?}"
        );
    }

    /// Eight short failures all fit with their message lines, and nine leave one counted.
    #[test]
    fn eight_short_failures_fit_and_a_ninth_is_counted() {
        let eight = (0..8)
            .map(|index| failure(&format!("t{index}"), index, "boom"))
            .collect();
        let fact = terminal_fact(1, &run(0, 8, eight), true);
        assert_eq!(fact.detail.matches("  FAIL ").count(), 8, "{fact:?}");
        assert_eq!(fact.detail.matches("boom").count(), 8, "{fact:?}");
        assert!(!fact.detail.contains("more failed tests"), "{fact:?}");
        let nine = (0..9)
            .map(|index| failure(&format!("t{index}"), index, "boom"))
            .collect();
        let fact = terminal_fact(1, &run(0, 9, nine), true);
        assert_eq!(fact.detail.matches("  FAIL ").count(), 8, "{fact:?}");
        assert!(
            fact.detail
                .contains("(+1 more failed tests in the full output)"),
            "{fact:?}"
        );
    }

    /// A budget kill says so, with the counts so far and the failures found.
    #[test]
    fn stopped_run_names_the_budget_and_the_counts_so_far() {
        let mut result = run(4, 1, vec![failure("tests::slow", 9, "timed out")]);
        result.stopped = true;
        result.exit = None;
        result.report.incomplete = true;
        let fact = terminal_fact(2, &result, true);
        assert_eq!(
            fact.summary,
            "stopped at budget 120 s — 4 passed, 1 failed so far (exit unavailable)"
        );
        assert!(fact.detail.starts_with("  FAIL tests::slow"), "{fact:?}");
        assert_in_contract(&fact);
    }

    /// A non-zero exit with a parsed but empty summary quotes the runner's own error line.
    #[test]
    fn nonzero_exit_without_counts_quotes_the_runner_line() {
        let mut result = run(0, 0, Vec::new());
        result.exit = Some(2);
        result.output = "warming up\nerror: could not compile `x`\n".to_owned();
        let fact = terminal_fact(5, &result, true);
        assert_eq!(
            fact.summary,
            "no test results (exit 2), 12 s — runner said: error: could not compile `x`"
        );
        assert_in_contract(&fact);
    }

    /// A run without a recognised summary reports its exit status and the last output lines,
    /// oldest first, never more than the tail limits.
    #[test]
    fn exit_only_run_quotes_the_output_tail_within_limits() {
        let mut result = run(0, 0, Vec::new());
        result.report.incomplete = true;
        result.exit = Some(0);
        result.output = (0..40).map(|index| format!("line {index}\n")).collect();
        let fact = terminal_fact(6, &result, true);
        assert_eq!(fact.summary, "exit 0, 12 s");
        assert!(
            fact.detail.starts_with("  output (tail):\n    line 30\n"),
            "{fact:?}"
        );
        assert!(fact.detail.contains("\n    line 39\n"), "{fact:?}");
        assert!(!fact.detail.contains("line 29"), "{fact:?}");
        assert_in_contract(&fact);
        result.exit = None;
        assert_eq!(
            terminal_fact(6, &result, true).summary,
            "exit unknown, 12 s"
        );
    }

    /// Only the run's owner is pointed at the full output, and only when it kept some.
    #[test]
    fn full_output_reference_is_for_the_owner_of_a_retained_output() {
        let result = run(1, 0, Vec::new());
        assert!(
            terminal_fact(1, &result, true)
                .detail
                .contains("full output:")
        );
        assert!(
            !terminal_fact(1, &result, false)
                .detail
                .contains("full output:")
        );
        let mut empty = run(1, 0, Vec::new());
        empty.output.clear();
        assert!(
            !terminal_fact(1, &empty, true)
                .detail
                .contains("full output:")
        );
        empty.output_paged = true;
        assert!(
            terminal_fact(1, &empty, true)
                .detail
                .contains("full output:")
        );
    }

    /// An environment label follows the summary, and a very long one is cut with the summary.
    #[test]
    fn environment_label_is_part_of_the_bounded_summary() {
        let mut result = run(1, 0, Vec::new());
        result.environment_label = Some("tool env".to_owned());
        assert_eq!(
            terminal_fact(1, &result, true).summary,
            "1 passed, 0 failed, 12 s (exit 0) · env tool env"
        );
        result.environment_label = Some("é".repeat(400));
        let fact = terminal_fact(1, &result, true);
        assert!(fact.summary.len() <= MAX_SUMMARY_BYTES);
        assert!(fact.summary.is_char_boundary(fact.summary.len()));
    }

    /// A clipped command is explicitly a preview with an owner-only full replay path, even
    /// with no stdout. Preview and raw-output links remain within the ordinary fact budget.
    #[test]
    fn oversized_rerun_command_is_cut_with_an_ellipsis() {
        let mut result = run(1, 0, Vec::new());
        result.command = vec!["cargo".to_owned(), "test".to_owned(), "x".repeat(900)];
        let fact = terminal_fact(1, &result, true);
        let rerun = fact
            .detail
            .lines()
            .find(|line| line.starts_with("  rerun preview: "))
            .unwrap();
        assert!(
            rerun.len() <= "  rerun preview: ".len() + MAX_RERUN_BYTES,
            "{rerun}"
        );
        assert!(rerun.ends_with('…'), "{rerun}");
        assert!(fact.detail.contains("full output:"), "{fact:?}");
        assert!(
            fact.detail
                .contains("full replay: ide.test {\"output\":\"ref-3/replay/output/1\"}")
        );
        result.output.clear();
        let empty = terminal_fact(1, &result, true);
        assert!(empty.detail.contains("full replay:"));
        assert!(!empty.detail.contains("full output:"));
        assert!(
            !terminal_fact(1, &result, false)
                .detail
                .contains("full replay:")
        );
        assert_in_contract(&fact);
    }

    /// Control characters in runner text are removed and multi-byte names are never split.
    #[test]
    fn runner_text_is_cleaned_and_cut_on_character_boundaries() {
        let name = format!("t\u{1b}[31m\n{}", "ü".repeat(100));
        let message = format!("bad\tmessage\r\n{}", "日".repeat(80));
        let fact = terminal_fact(1, &run(0, 1, vec![failure(&name, 1, &message)]), true);
        assert!(!fact.detail.contains('\u{1b}'), "{fact:?}");
        assert!(
            !fact.detail.contains('\t') && !fact.detail.contains('\r'),
            "{fact:?}"
        );
        for line in fact
            .detail
            .lines()
            .filter(|line| line.starts_with("  FAIL "))
        {
            assert!(line.len() <= "  FAIL ".len() + MAX_NAME_BYTES, "{line}");
        }
        assert_in_contract(&fact);
    }

    /// The lines of a run that has not finished carry no seconds, no hint and no instruction.
    #[test]
    fn non_terminal_lines_carry_no_seconds_and_no_instruction() {
        assert_eq!(running_line(4), "tests #4: running");
        assert_eq!(still_running_line(4), "tests #4: still running");
        assert_eq!(
            started_line(4, "cargo test --lib", Some("3 tests selected"), 120),
            "tests #4: started — cargo test --lib (3 tests selected) (budget 120 s); \
             the result is retained under its reference; Bridge delivers it automatically"
        );
        assert_eq!(
            started_line(5, "make test", None, 30),
            "tests #5: started — make test (budget 30 s); the result is retained under its \
             reference; Bridge delivers it automatically"
        );
        for line in [
            running_line(4),
            still_running_line(4),
            started_line(4, "x", None, 1),
        ] {
            for banned in [
                "poll",
                "every few seconds",
                "in a few seconds",
                "until it completes",
            ] {
                assert!(!line.contains(banned), "{banned}: {line}");
            }
        }
    }

    /// A several-target run lists one line per target between the failures and the rerun line;
    /// all target outcomes survive a crowded failure list.
    #[test]
    fn target_lines_sit_before_the_rerun_line_and_survive_failures() {
        let mut result = run(3, 1, vec![failure("tests::fails", 4, "boom")]);
        result.targets = vec![
            "target 1 symbol a.rs#f: run 1 — 2 passed, 0 failed".to_owned(),
            "target 2 path b.rs: run 1 — 1 passed, 1 failed".to_owned(),
        ];
        let fact = terminal_fact(8, &result, true);
        assert_eq!(
            fact.detail,
            "  FAIL tests::fails\n       src/a.rs:4  boom\n  target 1 symbol a.rs#f: run 1 — 2 passed, 0 failed\n  target 2 path b.rs: run 1 — 1 passed, 1 failed\n  rerun: cargo test\n  full output: ide.test {\"detail_ref\":\"ref-3/output/1\"}"
        );
        assert_in_contract(&fact);
        let failures = (0..8)
            .map(|index| {
                failure(
                    &format!("{}{index}", "n".repeat(100)),
                    index,
                    &"m".repeat(150),
                )
            })
            .collect();
        let mut crowded = run(0, 8, failures);
        crowded.targets = (0..8)
            .map(|index| format!("target {index} {}", "t".repeat(150)))
            .collect();
        let fact = terminal_fact(8, &crowded, true);
        assert_in_contract(&fact);
        assert_eq!(fact.detail.matches("  FAIL ").count(), 8, "{fact:?}");
        assert_eq!(fact.detail.matches("\n  target ").count(), 8, "{fact:?}");
    }

    /// Target labels containing colons cannot consume the outcome's exit or unrunnable state.
    #[test]
    fn target_outcomes_survive_long_colon_bearing_labels() {
        let label = "command tool: argument ".repeat(40);
        let counted = target_outcome(1, &label, "run 1 — 2 passed, 1 failed (exit 101)");
        assert!(counted.len() <= MAX_TARGET_LINE_BYTES);
        assert!(counted.ends_with("run 1 — 2 passed, 1 failed (exit 101)"));
        let skipped = target_outcome(2, &label, "not runnable: no matching runner");
        assert!(skipped.ends_with(": not runnable: no matching runner"));
    }

    /// Cancellation or budget expiry before any parsed summary reports absent counts/exits;
    /// parsed zero counts and observed process exits remain distinguishable.
    #[test]
    fn unparsed_stopped_runs_never_invent_counts_or_exits() {
        let mut result = run(0, 0, Vec::new());
        result.report.incomplete = true;
        result.stopped = true;
        result.exit = None;
        for cancelled in [false, true] {
            result.cancelled = cancelled;
            let fact = terminal_fact(1, &result, true);
            assert!(fact.summary.contains("test counts unavailable"), "{fact:?}");
            assert!(fact.summary.contains("exit unavailable"), "{fact:?}");
            assert!(!fact.summary.contains("0 passed"), "{fact:?}");
        }
        result.cancelled = false;
        result.stopped = false;
        result.report.incomplete = false;
        result.exit = Some(7);
        assert!(summary_text(&result).contains("exit 7"));
        result.exit = Some(0);
        assert!(summary_text(&result).contains("0 passed, 0 failed"));
    }

    /// The same run always renders the same text.
    #[test]
    fn rendering_is_deterministic() {
        let result = run(1, 1, vec![failure("a", 1, "m")]);
        assert_eq!(
            terminal_fact(1, &result, true),
            terminal_fact(1, &result, true)
        );
    }

    /// Runtime environment refusals retain exit/output/replay without inventing test counts.
    #[test]
    fn environment_refusal_is_distinct_from_test_failure() {
        let mut result = run(0, 0, vec![]);
        result.report.incomplete = true;
        result.exit = Some(78);
        result.output = "error: agent-ide test environment unavailable: incompatible Node\n".into();
        let fact = terminal_fact(1, &result, true);
        assert!(
            fact.summary
                .starts_with("environment unavailable (exit 78)"),
            "{fact:?}"
        );
        assert!(!fact.summary.contains("passed") && !fact.summary.contains("failed"));
        assert!(
            fact.detail.contains("incompatible Node")
                && fact.detail.contains("full output")
                && fact.detail.contains("rerun:")
        );
        result.exit = Some(1);
        assert!(!summary_text(&result).starts_with("environment unavailable"));
    }
}
