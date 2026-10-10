//! Guard for the reply hygiene rule of `docs/contracts/reply-hygiene-v1.md` §2: no text the model
//! reads from Agent IDE invites it to poll.
//!
//! The rule has two tiers. Tier A is text the model reads after a call (replies, plates, facts,
//! lines of a test run): none of the banned phrases may appear. Tier B is teaching text (tool
//! descriptions, server instructions, the skill and the bundled agent files): the phrases other
//! than `poll` are banned, and `poll` may appear only when it is negated (`never poll`).
//!
//! Every module that adds reply or teaching text adds its surface to [`surfaces`], so one test
//! covers the integrated candidate.

use std::path::{Path, PathBuf};

use super::{
    facade::{SERVER_INSTRUCTIONS, StdioFacade, tool_schemas},
    signature_card::signature_block,
    test_fact::{running_line, started_line, still_running_line},
};

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
    }
}

/// Collects every surface of this module's and its callers' text that is governed today.
fn surfaces() -> Vec<Surface> {
    let mut surfaces = vec![Surface {
        name: "SERVER_INSTRUCTIONS".to_owned(),
        tier: Tier::Teaching,
        text: SERVER_INSTRUCTIONS.to_owned(),
    }];
    for tool in StdioFacade::described_tool_router().list_all() {
        surfaces.push(Surface {
            name: format!("{} description", tool.name),
            tier: Tier::Teaching,
            text: tool.description.as_deref().unwrap_or_default().to_owned(),
        });
    }
    for schema in tool_schemas() {
        surfaces.push(Surface {
            name: format!("{} input schema", schema.name),
            tier: Tier::Teaching,
            text: schema.input_schema.to_string(),
        });
    }
    for (name, line) in [
        (
            "started line",
            started_line(1, "cargo test", Some("2 tests selected"), 120),
        ),
        ("running line", running_line(1)),
        ("still-running line", still_running_line(1)),
    ] {
        surfaces.push(Surface {
            name: name.to_owned(),
            tier: Tier::ReplyText,
            text: line,
        });
    }
    surfaces.push(Surface {
        name: "ide.start signature block".to_owned(),
        tier: Tier::ReplyText,
        text: signature_block().to_owned(),
    });
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

/// No governed surface contains a banned phrase under its tier.
#[test]
fn reply_surfaces_never_invite_polling() {
    let mut found = Vec::new();
    for surface in surfaces() {
        for phrase in violations(surface.tier, &surface.text) {
            found.push(format!("{} contains {phrase:?}", surface.name));
        }
    }
    assert!(found.is_empty(), "{found:#?}");
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
