//! Guard for the reply hygiene rule of `docs/contracts/reply-hygiene-v1.md` §2: no text the model
//! reads from Agent IDE invites it to poll.
//!
//! The rule has two tiers. Tier A is text the model reads after a call (replies, plates, facts,
//! lines of a test run, the `ide.stop` reply, the servers line, batch reply lines): none of the
//! banned phrases may appear. Tier B is teaching text (tool descriptions, server instructions,
//! the skill and the bundled agent files): the phrases other than `poll` are banned, and `poll`
//! may appear only when it is negated (`never poll`). A line about a running, pending or
//! still-active job must also carry no seconds count; a started line's budget is a limit, not
//! progress, and a finished job's duration is an outcome, so neither is flagged.
//!
//! Every module that adds reply or teaching text adds its surface to [`surfaces`], so one test
//! covers the integrated candidate.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    batch::{
        ReadCounts, ReadForm, ReadItem, edit_checked_draft, read_follow_up, read_summary,
        summarize_diagnostics, unchanged_line,
    },
    facade::{
        AssistanceTool, CatalogHost, SERVER_INSTRUCTIONS, StdioFacade, tool_schemas, validate_call,
    },
    reply::EditDiagnostics,
    signature_card::signature_block,
    test_fact::{running_line, started_line, still_running_line},
    worker::stop_text,
};
use crate::project::{READY_STATE, not_started_state, unavailable_state, warming_state};

/// The phrases that tell the agent to wait and ask again, lowercase.
const BANNED: [&str; 5] = [
    "poll",
    "every few seconds",
    "in a few seconds",
    "until it completes",
    "again every",
];

/// The negated forms in which Tier B text may still contain `poll`.
const NEGATED_POLL: [&str; 5] = [
    "do not poll",
    "don't poll",
    "never poll",
    "no polling",
    "not poll",
];

/// The old forms no model-facing text may teach once the IDE is symbol-first (M-030 T-012),
/// lowercase and matched after whitespace is collapsed: waiting advice, the file-version
/// reference, labelled replies and `Next:` lines, the old truncation trailers, the retired
/// `ide.context` read habit and line-first examples.
const STALE_FORMS: [&str; 18] = [
    "wait_ms",
    "source_ref",
    "nothing else to do",
    "wait once",
    "next: use",
    "complete activation",
    "complete context",
    "complete diff",
    "complete stop",
    "output is truncated",
    "output is incomplete",
    "fresh bytes",
    "retry with the new",
    "ide.context",
    "\"lines\":\"",
    "\\\"lines\\\":\\\"",
    "bounded diagnostic messages in the latest context",
    "latest context result",
];

/// Which half of the rule a surface falls under.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Tier {
    /// Text the model reads after a call: no banned phrase at all.
    ReplyText,
    /// Teaching text: `poll` only in a negated form.
    Teaching,
}

/// One named text the model reads, with the tier that governs it.
struct Surface {
    /// Where the text comes from, shown in a failure.
    name: String,
    /// The rule that applies to it.
    tier: Tier,
    /// The text itself.
    text: String,
    /// Whether the text speaks of a job that is running, pending or still active, so it must
    /// carry no seconds count.
    unfinished: bool,
}

/// Builds a Tier A surface named `name`; `unfinished` marks text about unfinished work.
fn reply_surface(name: &str, text: impl Into<String>, unfinished: bool) -> Surface {
    Surface {
        name: name.to_owned(),
        tier: Tier::ReplyText,
        text: text.into(),
        unfinished,
    }
}

/// Collapses runs of whitespace to one space and lowercases, so a phrase wrapped over two lines
/// of a doc comment still matches.
fn normalised(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Returns the banned phrases `text` contains under `tier`.
fn violations(tier: Tier, text: &str) -> Vec<&'static str> {
    let mut text = normalised(text);
    if tier == Tier::Teaching {
        for negated in NEGATED_POLL {
            text = text.replace(negated, " ");
        }
    }
    BANNED
        .iter()
        .copied()
        .filter(|phrase| text.contains(phrase))
        .collect()
}

/// Returns the stale forms `text` of the source `name` teaches. The `ide.context` tool may name
/// itself, and nothing else may name it.
fn stale_forms(name: &str, text: &str) -> Vec<&'static str> {
    let text = normalised(text);
    STALE_FORMS
        .iter()
        .copied()
        .filter(|form| !(*form == "ide.context" && name.contains("ide.context")))
        .filter(|form| text.contains(form))
        .collect()
}

/// Every text a model reads while using the IDE that the symbol-first audit governs: both
/// server paragraphs, both advertised catalogs (descriptions and schemas, as served), the
/// signature block, both skills with their agent files, the reviewer agent, the plugin
/// manifests and the reply template, plus every governed reply and teaching surface.
fn symbol_first_sources() -> Vec<(String, String)> {
    let mut sources = vec![
        (
            "SERVER_INSTRUCTIONS".to_owned(),
            SERVER_INSTRUCTIONS.to_owned(),
        ),
        (
            "SERVER_INSTRUCTIONS_CODEX".to_owned(),
            CatalogHost::Codex.instructions().to_owned(),
        ),
        (
            "ide.start signature block".to_owned(),
            signature_block().to_owned(),
        ),
    ];
    for host in [CatalogHost::Claude, CatalogHost::Codex] {
        for tool in host.catalog(StdioFacade::described_tool_router().list_all()) {
            sources.push((
                format!("{host:?} {} as served", tool.name),
                serde_json::to_string(&tool).expect("a tool serializes"),
            ));
        }
    }
    for path in [
        "skills/agent-ide/SKILL.md",
        "skills/agent-ide/agents/openai.yaml",
        "skills/codex/agent-ide/SKILL.md",
        "skills/codex/agent-ide/agents/openai.yaml",
        "agents/ide-reviewer.md",
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "assets/mcp/reply.jinja",
    ] {
        sources.push((path.to_owned(), file_surface(path, Tier::Teaching).text));
    }
    // The governed reply surfaces; the raw schemas are covered above as served.
    sources.extend(
        surfaces()
            .into_iter()
            .filter(|surface| !surface.name.ends_with("input schema"))
            .map(|surface| (surface.name, surface.text)),
    );
    sources
}

/// Returns the repository root, two levels above this crate.
fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Reads one repository file as a surface of `tier`.
fn file_surface(relative: &str, tier: Tier) -> Surface {
    let path = repository_root().join(relative);
    Surface {
        name: relative.to_owned(),
        tier,
        text: std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path:?}: {error}")),
        unfinished: false,
    }
}

/// Collects every surface of this module's and its callers' text that is governed today.
fn surfaces() -> Vec<Surface> {
    let mut surfaces = vec![Surface {
        name: "SERVER_INSTRUCTIONS".to_owned(),
        tier: Tier::Teaching,
        text: SERVER_INSTRUCTIONS.to_owned(),
        unfinished: false,
    }];
    for tool in StdioFacade::described_tool_router().list_all() {
        surfaces.push(Surface {
            name: format!("{} description", tool.name),
            tier: Tier::Teaching,
            text: tool.description.as_deref().unwrap_or_default().to_owned(),
            unfinished: false,
        });
    }
    for schema in tool_schemas() {
        surfaces.push(Surface {
            name: format!("{} input schema", schema.name),
            tier: Tier::Teaching,
            text: schema.input_schema.to_string(),
            unfinished: false,
        });
    }
    // A started line states the run's budget, a limit and not progress, so it is not unfinished
    // work in the sense of the seconds rule; the two lines about a live run are.
    surfaces.push(reply_surface(
        "started line",
        started_line(1, "cargo test", Some("2 tests selected"), 120),
        false,
    ));
    surfaces.push(reply_surface("running line", running_line(1), true));
    surfaces.push(reply_surface(
        "still-running line",
        still_running_line(1),
        true,
    ));
    surfaces.push(reply_surface(
        "ide.start signature block",
        signature_block(),
        false,
    ));
    surfaces.extend(stop_surfaces());
    surfaces.extend(servers_line_surfaces());
    surfaces.extend(batch_surfaces());
    surfaces.extend(batch_test_line_surfaces());
    // The reply template holds every fixed reply sentence, including the loading, takeover and
    // pending texts of the availability and outbox modules.
    surfaces.push(file_surface("assets/mcp/reply.jinja", Tier::ReplyText));
    surfaces.push(file_surface("skills/agent-ide/SKILL.md", Tier::Teaching));
    surfaces.push(file_surface("agents/ide-reviewer.md", Tier::Teaching));
    surfaces.push(file_surface(
        "skills/agent-ide/agents/openai.yaml",
        Tier::Teaching,
    ));
    surfaces
}

/// The `ide.stop` reply with and without uncollected test runs, and with nothing active.
fn stop_surfaces() -> Vec<Surface> {
    let runs = [
        "#1 cargo test (still running)".to_owned(),
        "#2 other runner".to_owned(),
    ];
    vec![
        reply_surface("ide.stop reply", stop_text(true, &runs, None), true),
        reply_surface(
            "ide.stop reply, nothing active",
            stop_text(false, &[], None),
            false,
        ),
    ]
}

/// Every state of the start card's servers line (start-index-state r3), for a language whose
/// lexical outline is exact and for one whose is not. The line speaks of servers still loading
/// or not yet started, so it is unfinished work.
fn servers_line_surfaces() -> Vec<Surface> {
    use crate::lang::testing::{ALPHA, EPSILON};
    crate::lang::testing::install();
    let states = [
        (
            "not started, exact outline",
            EPSILON,
            not_started_state(EPSILON).to_owned(),
        ),
        ("not started", ALPHA, not_started_state(ALPHA).to_owned()),
        (
            "warming, exact outline",
            EPSILON,
            warming_state(EPSILON).to_owned(),
        ),
        ("warming", ALPHA, warming_state(ALPHA).to_owned()),
        (
            "unavailable",
            ALPHA,
            unavailable_state("workspace failed to load"),
        ),
        ("ready", ALPHA, READY_STATE.to_owned()),
    ];
    states
        .into_iter()
        .map(|(name, language, state)| {
            reply_surface(
                &format!("servers line, {name}"),
                format!("servers: {language} {state}"),
                true,
            )
        })
        .collect()
}

/// The batch read and edit reply lines of batch-api-v1: summaries, footers, the unchanged line and
/// the pending and settled diagnostics wording. The pending ones are unfinished work.
fn batch_surfaces() -> Vec<Surface> {
    let counts = ReadCounts {
        items: 3,
        files: 2,
        delivered: 1,
        unchanged: 1,
        cut: 1,
        failed: 0,
    };
    let cut = ReadItem::Symbol("src/c.rs#C".to_owned());
    let pending = vec![(
        "src/a.rs".to_owned(),
        EditDiagnostics::Pending {
            detail_ref: "ref-2".to_owned(),
        },
    )];
    let (words, per_file, _) = summarize_diagnostics(&pending);
    let draft = edit_checked_draft("op-1", "ref-2", &pending, Vec::new());
    let mut surfaces = vec![
        reply_surface("batch read summary", read_summary(&counts), false),
        reply_surface(
            "batch unchanged line",
            unchanged_line("ref-1", "src/a.rs", "1  a\n2  b\n"),
            false,
        ),
        reply_surface(
            "batch read follow-up",
            read_follow_up(ReadForm::Items, &[&cut]),
            false,
        ),
        reply_surface("batch pending diagnostics", words, true),
        reply_surface("batch edit-checked summary", draft.summary, false),
        reply_surface(
            "batch edit-checked detail",
            draft.detail.unwrap_or_default(),
            false,
        ),
    ];
    for line in per_file {
        surfaces.push(reply_surface("batch pending file line", line, true));
    }
    surfaces
}

/// The reply lines the several-target `ide.test` form prints (`batch_tests.rs`). Those functions
/// are private to the batch module, so the guard reads their format strings from the source, with
/// every `{…}` argument replaced by `1`. The started line states the budget, a limit; the line
/// about a live run is unfinished work.
fn batch_test_line_surfaces() -> Vec<Surface> {
    let file = "crates/agent-ide-core/src/assistance/batch_tests.rs";
    vec![
        reply_surface(
            "batch started line",
            format_literals(file, "started_line"),
            false,
        ),
        reply_surface(
            "batch running line",
            format_literals(file, "running_line"),
            true,
        ),
    ]
}

/// Returns the text that function `name` of source file `relative` formats: its string literals
/// with line continuations joined and every `{…}` placeholder replaced by `1`.
///
/// Panics when the function is not in the file, so a rename cannot silently drop a surface.
fn format_literals(relative: &str, name: &str) -> String {
    let source = file_surface(relative, Tier::ReplyText).text;
    let start = source
        .find(&format!("fn {name}("))
        .unwrap_or_else(|| panic!("{relative} has no fn {name}"));
    let body = &source[start..];
    literals_of(&body[..body.find("\n}\n").unwrap_or(body.len())])
}

/// Returns the string literals of the source `body` as one text: line continuations joined, `{{`
/// and `}}` as single braces, and every other `{…}` placeholder replaced by `1`.
fn literals_of(body: &str) -> String {
    let mut text = String::new();
    let (mut in_string, mut chars) = (false, body.chars().peekable());
    while let Some(character) = chars.next() {
        match (in_string, character) {
            (false, '"') => in_string = true,
            (true, '"') => {
                in_string = false;
                text.push(' ');
            }
            (true, '\\') => match chars.next() {
                Some('\n') => while chars.next_if(|next| next.is_whitespace()).is_some() {},
                Some('n') => text.push('\n'),
                Some(other) => text.push(other),
                None => {}
            },
            (true, '{') if chars.next_if_eq(&'{').is_some() => text.push('{'),
            (true, '{') => {
                while chars.next_if(|next| *next != '}').is_some() {}
                chars.next();
                text.push('1');
            }
            (true, '}') => {
                chars.next_if_eq(&'}');
                text.push('}');
            }
            (true, other) => text.push(other),
            (false, _) => {}
        }
    }
    text
}

/// Returns every seconds count in `text`: a number followed by `s`, `sec`, `secs`, `second` or
/// `seconds` as a whole word (`12 s`, `3s`, `30 seconds`). A number that follows `#` or sits
/// inside a word (a run number, a reference) is not a count.
fn seconds_counts(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        if !chars[at].is_ascii_digit() {
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && chars[at].is_ascii_digit() {
            at += 1;
        }
        if start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '#') {
            continue;
        }
        let mut word = at;
        if chars.get(word) == Some(&' ') {
            word += 1;
        }
        let unit: String = chars[word.min(chars.len())..]
            .iter()
            .take_while(|character| character.is_alphabetic())
            .collect();
        if matches!(unit.as_str(), "s" | "sec" | "secs" | "second" | "seconds") {
            found.push(chars[start..word + unit.chars().count()].iter().collect());
        }
    }
    found
}

/// No governed surface contains a banned phrase under its tier, and no line about unfinished work
/// carries a seconds count.
#[test]
fn reply_surfaces_never_invite_polling() {
    let mut found = Vec::new();
    for surface in surfaces() {
        for phrase in violations(surface.tier, &surface.text) {
            found.push(format!("{} contains {phrase:?}", surface.name));
        }
        if surface.unfinished {
            for seconds in seconds_counts(&surface.text) {
                found.push(format!(
                    "{} counts {seconds:?} on unfinished work",
                    surface.name
                ));
            }
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// No model-facing text teaches an old form: every source of [`symbol_first_sources`] is free of
/// [`STALE_FORMS`] (the audit of M-030 T-012; `docs/model-text-inventory.md` lists the sources).
#[test]
fn model_text_teaches_only_the_symbol_workflow() {
    let mut found = Vec::new();
    for (name, text) in symbol_first_sources() {
        for form in stale_forms(&name, &text) {
            found.push(format!("{name}: teaches `{form}`"));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
}

/// The stale-form guard flags each retired form, also across a line wrap, lets the `ide.context`
/// tool name itself and passes the symbol-first wording.
#[test]
fn the_stale_form_guard_catches_every_retired_form() {
    for form in STALE_FORMS {
        let wrapped = form.replacen(' ', "\n    ", 1);
        let name = if form == "ide.context" {
            "ide.diff"
        } else {
            "x"
        };
        assert_eq!(
            stale_forms(name, &format!("see {wrapped} here")),
            [form],
            "{form}"
        );
    }
    assert!(stale_forms("Claude ide.context as served", "ide.context reads a path").is_empty());
    assert!(
        stale_forms(
            "x",
            "Read src/a.rs#Type/method:7 (L123) with ide.read; results arrive by themselves"
        )
        .is_empty()
    );
}

/// The seconds detector finds counts of every spelling and ignores run numbers, ids and words
/// that merely start with `s`; the literal extractor reads a format string with its
/// continuations and placeholders.
#[test]
fn the_guard_flags_seconds_on_unfinished_work() {
    assert_eq!(seconds_counts("tests #1: still running (30 s)"), ["30 s"]);
    assert_eq!(
        seconds_counts("12s elapsed, then 3 seconds"),
        ["12s", "3 seconds"]
    );
    assert_eq!(seconds_counts("after 1.5 secs"), ["5 secs"]);
    assert!(seconds_counts("tests #12: still running").is_empty());
    assert!(seconds_counts("4 successes in ref-2 (3 servers)").is_empty());
    assert!(seconds_counts("rerun: cargo test").is_empty());
    let code = r#"fn line(id: u64) { format!("tests #{id}: still running ({} s); {{x}} \
         later", 3) }"#;
    assert_eq!(
        literals_of(code),
        "tests #1: still running (1 s); {x} later "
    );
}

/// The checker flags every banned phrase, also across a line wrap, and lets Tier B negate `poll`.
#[test]
fn the_guard_flags_polling_and_allows_its_negation() {
    assert_eq!(violations(Tier::ReplyText, "poll: call ide.test"), ["poll"]);
    assert_eq!(
        violations(Tier::ReplyText, "Retry every\n    few seconds"),
        ["every few seconds"]
    );
    assert_eq!(
        violations(Tier::ReplyText, "repeat the call in a few seconds"),
        ["in a few seconds"]
    );
    assert_eq!(
        violations(
            Tier::ReplyText,
            "wait until it completes, then again every minute"
        ),
        ["until it completes", "again every"]
    );
    assert!(violations(Tier::ReplyText, "the result arrives by itself").is_empty());
    assert!(violations(Tier::ReplyText, "never poll for it").contains(&"poll"));
    assert!(violations(Tier::Teaching, "so never poll for them; do not poll").is_empty());
    assert!(!violations(Tier::Teaching, "poll ide.inspect").is_empty());
    assert!(!violations(Tier::Teaching, "never poll, then poll again").is_empty());
}

/// Returns the byte length of the balanced JSON object that `text` starts with, ignoring braces
/// inside strings, or `None` when it never closes.
fn balanced_object_len(text: &str) -> Option<usize> {
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (index, character) in text.char_indices() {
        if in_string {
            match (escaped, character) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Collects every `ide.<tool> {…}` example in `text`: the tool and the JSON object that follows
/// its name.
fn examples_in(text: &str) -> Vec<(AssistanceTool, String)> {
    let mut found = Vec::new();
    for schema in tool_schemas() {
        let marker = format!("{} {{", schema.name);
        let mut from = 0;
        while let Some(at) = text[from..].find(&marker) {
            let start = from + at + marker.len() - 1;
            let length = balanced_object_len(&text[start..])
                .unwrap_or_else(|| panic!("an unclosed {} example", schema.name));
            found.push((schema.tool, text[start..start + length].to_owned()));
            from = start + length;
        }
    }
    found
}

/// Reports why an example does not hold, or `None` when it does.
///
/// A complete example must pass the facade's own argument validation. An abbreviated one (it
/// holds `…` in place of an array or an object) must still be an object whose every top-level
/// key is a property of the tool's schema.
fn example_problem(tool: AssistanceTool, example: &str) -> Option<String> {
    let schema = tool_schemas()
        .into_iter()
        .find(|schema| schema.tool == tool)
        .expect("every tool has a schema");
    if let Ok(value) = serde_json::from_str::<Value>(&example.replace('…', "x")) {
        return validate_call(tool, value)
            .err()
            .map(|error| format!("refused: {error:?}"));
    }
    if !example.contains('…') {
        return Some("not JSON".to_owned());
    }
    let sketch = example.replace("[…]", "[]").replace("{…}", "{}");
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(&sketch) else {
        return Some("not a JSON object even with its placeholders emptied".to_owned());
    };
    let properties = schema.input_schema["properties"]
        .as_object()
        .expect("schemas list properties");
    fields
        .keys()
        .find(|key| !properties.contains_key(*key))
        .map(|key| format!("`{key}` is not a property of {}", schema.name))
}

/// Every `ide.<tool> {…}` example the teaching text shows — the server instructions, the tool
/// descriptions, the skill and the `ide.start` signature block — is an argument object of that
/// tool: complete examples pass the facade's validation, abbreviated ones use only schema
/// properties. The schemas validated against are the ones `schemas/tools.json` snapshots.
#[test]
fn teaching_examples_validate_against_the_tool_schemas() {
    let mut teaching = vec![(
        "SERVER_INSTRUCTIONS".to_owned(),
        SERVER_INSTRUCTIONS.to_owned(),
    )];
    for tool in StdioFacade::described_tool_router().list_all() {
        teaching.push((
            format!("{} description", tool.name),
            tool.description.as_deref().unwrap_or_default().to_owned(),
        ));
    }
    teaching.push((
        "ide.start signature block".to_owned(),
        signature_block().to_owned(),
    ));
    teaching.push((
        "SKILL.md".to_owned(),
        file_surface("skills/agent-ide/SKILL.md", Tier::Teaching).text,
    ));
    let mut checked = 0;
    let mut problems = Vec::new();
    for (name, text) in teaching {
        for (tool, example) in examples_in(&text) {
            checked += 1;
            if let Some(problem) = example_problem(tool, &example) {
                problems.push(format!("{name}: {} {example}: {problem}", tool.mcp_name()));
            }
        }
    }
    assert!(checked >= 8, "only {checked} examples found");
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The extractor finds balanced objects across strings and braces in strings, and the checker
/// refuses an unknown key, an invalid value and a non-object.
#[test]
fn the_example_checker_refuses_what_a_live_call_refuses() {
    let text = r#"use ide.read {"items":[{"path":"a{b"}]} then ide.inspect {"wait_ms": 5} now"#;
    let found = examples_in(text);
    assert_eq!(found.len(), 2);
    assert!(found.contains(&(
        AssistanceTool::Read,
        r#"{"items":[{"path":"a{b"}]}"#.to_owned()
    )));
    assert!(found.contains(&(AssistanceTool::Inspect, r#"{"wait_ms": 5}"#.to_owned())));
    assert_eq!(
        example_problem(AssistanceTool::Inspect, r#"{"wait_ms": 5}"#),
        None
    );
    assert!(example_problem(AssistanceTool::Inspect, r#"{"wait": 5}"#).is_some());
    assert!(example_problem(AssistanceTool::Inspect, r#"{"wait_ms": "soon"}"#).is_some());
    assert!(example_problem(AssistanceTool::Edit, r#"{"files":[…],"unknown":1}"#).is_some());
    assert_eq!(
        example_problem(AssistanceTool::Edit, r#"{"files":[…]}"#),
        None
    );
    assert!(example_problem(AssistanceTool::Inspect, "{wait_ms: 5}").is_some());
}
