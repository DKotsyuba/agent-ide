//! Contract checks for the `ide.context` problems kind: static schema, the confined snapshot
//! source seam, and deterministic untrusted page rendering (EYES-r1 §7, EYES-r2).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use agent_ide::{
    assistance::{
        facade::{AssistanceTool, ParameterError, tool_schemas, validate_call},
        problems::{PROBLEMS_PAGE_SIZE, ProblemSource, parse_language, problems_text},
    },
    checks::{CheckState, Language, Problem, ProblemSnapshot, Severity},
};
use serde_json::json;

/// Fake source that records each queried worktree and returns fixed cloned snapshots.
///
/// It never runs a check and exists to prove the daemon answers the problems kind from this seam
/// alone: one query per call, receiving exactly the authorized worktree path.
struct FakeSource {
    /// Snapshots returned on every query, cloned per call.
    snapshots: Vec<ProblemSnapshot>,
    /// Recorded query worktrees in call order.
    queried: Mutex<Vec<PathBuf>>,
}

impl FakeSource {
    /// Builds a substitute returning the same snapshots for every worktree.
    fn new(snapshots: Vec<ProblemSnapshot>) -> Self {
        Self {
            snapshots,
            queried: Mutex::new(Vec::new()),
        }
    }

    /// Returns the recorded query worktrees in call order.
    fn queried(&self) -> Vec<PathBuf> {
        self.queried
            .lock()
            .expect("query log is not poisoned")
            .clone()
    }
}

impl ProblemSource for FakeSource {
    fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot> {
        self.queried
            .lock()
            .expect("fake source query log is not poisoned")
            .push(worktree.to_path_buf());
        self.snapshots.clone()
    }
}

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

/// Advertises the closed problems fields alongside the unchanged v0.2 context properties.
#[test]
fn context_schema_advertises_bounded_problems_fields() {
    let schema = tool_schemas()
        .into_iter()
        .find(|schema| schema.name == "ide.context")
        .expect("context schema stays present")
        .input_schema;
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["kind"],
        json!({"type":"string","enum":["problems"]})
    );
    assert_eq!(
        schema["properties"]["language"],
        json!({"type":"string","enum":["rust","python","typescript"]})
    );
    assert_eq!(schema["properties"]["offset"]["type"], "integer");
    assert_eq!(schema["properties"]["offset"]["minimum"], 0);
    assert_eq!(schema["properties"]["offset"]["maximum"], u32::MAX);
    // The either-or rule is enforced by the handler, so the schema names no requirement at all.
    assert!(schema.get("required").is_none());
}

/// Accepts the problems kind without a path while rejecting every unbounded argument shape.
#[test]
fn validation_accepts_problems_without_path_and_keeps_bounds_closed() {
    assert!(validate_call(AssistanceTool::Context, json!({"kind":"problems"})).is_ok());
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"python","offset":40})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"typescript"})
        )
        .is_ok()
    );
    for invalid in [
        json!({"kind":"problems","language":"ruby"}),
        json!({"kind":"problems","offset":-2}),
        json!({"kind":"problems","offset":"0"}),
        json!({"kind":"problems","unknown":"field"}),
    ] {
        assert!(
            validate_call(AssistanceTool::Context, invalid.clone()).is_err(),
            "{invalid}"
        );
    }
}

/// A context request naming neither `path` nor `kind: "problems"` gets the dedicated typed error.
#[test]
fn context_without_path_or_problems_kind_is_a_context_target_error() {
    for invalid in [
        json!({}),
        json!({"byte_offset":0}),
        json!({"detail_ref":"detail-1"}),
        json!({"kind":"other"}),
    ] {
        assert_eq!(
            validate_call(AssistanceTool::Context, invalid.clone()).unwrap_err(),
            ParameterError::ContextTarget,
            "{invalid}"
        );
    }
    assert!(validate_call(AssistanceTool::Context, json!({"path":"src/main.rs"})).is_ok());
}

/// The source seam stays usable as a shared trait object behind the worker builder.
#[test]
fn problem_source_is_object_safe_send_and_sync() {
    fn require_send_sync(source: Arc<dyn ProblemSource>) -> Arc<dyn ProblemSource> {
        source
    }
    let source = require_send_sync(Arc::new(FakeSource::new(Vec::new())));
    assert!(source.latest(Path::new("/wt")).is_empty());
    // The concrete substitute records each queried worktree in call order.
    let concrete = FakeSource::new(Vec::new());
    assert!(concrete.latest(Path::new("/wt")).is_empty());
    assert_eq!(concrete.queried(), vec![PathBuf::from("/wt")]);
    assert_eq!(PROBLEMS_PAGE_SIZE, 20);
    assert_eq!(parse_language("python"), Some(Language::Python));
    assert_eq!(parse_language("typescript"), Some(Language::TypeScript));
}

/// Pages 45 fake problems 20 at a time with exact counts, ordering and next_offset markers.
#[test]
fn fake_source_pages_45_problems_with_next_offset() {
    let problems: Vec<Problem> = (1..=45)
        .map(|line| {
            problem(
                "src/lib.rs",
                line,
                1,
                if line % 2 == 0 {
                    Severity::Warning
                } else {
                    Severity::Error
                },
                "boom",
            )
        })
        .collect();
    let source = FakeSource::new(vec![ready(Language::Rust, problems)]);
    let snapshots = source.latest(Path::new("/private/tmp/worktree"));
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        source.queried(),
        vec![PathBuf::from("/private/tmp/worktree")]
    );

    // Snapshot counts always describe the full result, and errors sort before warnings.
    let first = problems_text(&snapshots, None, 0);
    assert_eq!(
        first.lines().next(),
        Some("rust: ready; errors: 23; warnings: 22")
    );
    let first_lines: Vec<&str> = first.lines().collect();
    assert_eq!(first_lines.len(), 22, "{first}");
    assert!(first_lines[1].starts_with("src/lib.rs:1:1 error [E0001] boom"));
    assert!(first_lines[20].starts_with("src/lib.rs:39:1"));
    assert_eq!(first_lines[21], "next_offset: 20");

    let second = problems_text(&snapshots, None, 20);
    let second_lines: Vec<&str> = second.lines().collect();
    assert_eq!(second_lines.len(), 22, "{second}");
    assert!(second_lines[1].starts_with("src/lib.rs:41:1"));
    assert_eq!(second_lines[21], "next_offset: 40");

    let last = problems_text(&snapshots, None, 40);
    let last_lines: Vec<&str> = last.lines().collect();
    assert_eq!(last_lines.len(), 6, "{last}");
    // 23 errors sort first, so the 5-problem last page holds the final warnings only.
    assert!(last_lines[1].starts_with("src/lib.rs:36:1"));
    assert!(last_lines[5].starts_with("src/lib.rs:44:1"));
    assert!(!last.contains("next_offset"));
}

/// A language filter renders only the matching configured language's state and problems.
#[test]
fn language_filter_selects_only_the_matching_configured_language() {
    let source = FakeSource::new(vec![
        ready(
            Language::Rust,
            vec![problem("a.rs", 1, 1, Severity::Error, "rust one")],
        ),
        ready(
            Language::Python,
            vec![problem("b.py", 2, 1, Severity::Warning, "python one")],
        ),
    ]);
    let snapshots = source.latest(Path::new("/wt"));

    let all = problems_text(&snapshots, None, 0);
    assert!(all.contains("rust: ready; errors: 1; warnings: 0"), "{all}");
    assert!(
        all.contains("python: ready; errors: 0; warnings: 1"),
        "{all}"
    );
    assert!(all.contains("a.rs:1:1 error [E0001] rust one"));
    assert!(all.contains("b.py:2:1 warning [E0001] python one"));

    let python = problems_text(&snapshots, Some(Language::Python), 0);
    assert!(python.contains("python: ready"), "{python}");
    assert!(python.contains("b.py:2:1 warning [E0001] python one"));
    assert!(!python.contains("rust:"), "{python}");
    assert!(!python.contains("a.rs"));
}

/// Untrusted checker text renders on exactly one line with all control characters stripped.
#[test]
fn untrusted_text_is_single_line_without_control_characters() {
    let source = FakeSource::new(vec![ready(
        Language::Rust,
        vec![Problem::new(
            "a\nb.rs".to_owned(),
            1,
            1,
            Severity::Error,
            Some("E0001".to_owned()),
            "line1\r\nline2\u{0}\u{7}tail".to_owned(),
        )],
    )]);
    let text = problems_text(&source.latest(Path::new("/wt")), None, 0);
    assert_eq!(text.lines().count(), 2, "{text}");
    // The newline separators are the only permitted control positions: every rendered line is
    // itself free of control characters, so no problem can forge additional lines or framing.
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
    let source = FakeSource::new(vec![ready(Language::Rust, Vec::new())]);
    let snapshots = source.latest(Path::new("/wt"));
    assert_eq!(
        problems_text(&snapshots, Some(Language::Python), 0),
        "checks disabled"
    );
}
