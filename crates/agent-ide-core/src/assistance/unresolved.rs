//! The MCP front's memory of requested work it relayed unresolved (OUTBOX-r3 §10).
//!
//! The session ledger lives in the daemon and dies with it. The MCP front, one per host session,
//! survives a daemon replacement, so it remembers per actor up to
//! [`MAX_UNRESOLVED`](crate::assistance::unresolved::MAX_UNRESOLVED) subjects of work whose
//! caller got no answer: a `pending` reply, a `tests #N: started` line, an edit with
//! `diagnostics: pending`. A subject is resolved when the front reads its reference back or relays
//! the settled run. When the actor is re-activated on a new daemon,
//! the subjects travel with the re-activation and the new daemon attempts one
//! `lost-after-restart` Bridge publication. The front clears them only after accepted or
//! uncertain custody; a definite no-effect refusal retains them for a later eligible restart.
//! Model observation remains unknown, so the fact says "may be lost".

use std::sync::Mutex;

use serde_json::Value;

use super::{
    facade::AssistanceTool,
    reply::{EditDiagnostics, PeerReply},
    worker::{owed_fact, settled_run},
};

/// Most unresolved subjects remembered per actor (OUTBOX §10).
pub const MAX_UNRESOLVED: usize = 16;
/// Most actors remembered; the oldest is dropped first.
const MAX_ACTORS: usize = 32;

/// One relayed request whose result the front has not seen.
struct Entry {
    /// Fact subject (`tests #3`, `edit op-7`, `ide.symbol src/a.rs#Foo`).
    subject: String,
    /// Reference that reads the result back, when the reply named one.
    reference: Option<String>,
}

/// Per-actor unresolved subjects of one MCP front; the actor key is the daemon's private actor
/// tag, or empty for an untagged (older or Codex) daemon's single activation.
#[derive(Default)]
pub struct Unresolved(Mutex<Vec<(String, Vec<Entry>)>>);

impl Unresolved {
    /// Updates `actor`'s memory with one relayed reply to `tool` called with `parameters`: every
    /// singular or plural continuation reference the reply resolves is forgotten, and what the
    /// reply leaves owed is remembered.
    pub fn observe(
        &self,
        actor: &str,
        tool: AssistanceTool,
        parameters: &Value,
        reply: &PeerReply,
    ) {
        let answered = !matches!(
            reply,
            PeerReply::Pending { .. }
                | PeerReply::Edit {
                    diagnostics: EditDiagnostics::Pending { .. },
                    ..
                }
        );
        let reads = matches!(
            tool,
            AssistanceTool::Inspect
                | AssistanceTool::Read
                | AssistanceTool::Graph
                | AssistanceTool::Diff
                | AssistanceTool::Test
        ) && answered;
        let reads = if reads {
            parameters
                .get("detail_ref")
                .and_then(Value::as_str)
                .into_iter()
                .chain(
                    parameters
                        .get("detail_refs")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str),
                )
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let settled = (parameters.get("history").is_none() && parameters.get("cancel").is_none())
            .then(|| settled_run(reply))
            .flatten()
            .map(|id| format!("tests #{id}"));
        let owed = owed_fact(tool, parameters, reply);
        let Ok(mut actors) = self.0.lock() else {
            return;
        };
        let index = match actors.iter().position(|(known, _)| known == actor) {
            Some(index) => index,
            None if owed.is_some() => {
                if actors.len() >= MAX_ACTORS {
                    actors.remove(0);
                }
                actors.push((actor.to_owned(), Vec::new()));
                actors.len() - 1
            }
            None => return,
        };
        let entries = &mut actors[index].1;
        entries.retain(|entry| {
            let read_back = reads
                .iter()
                .any(|read| entry.reference.as_deref() == Some(*read) || entry.subject == *read);
            !(read_back || settled.as_deref() == Some(entry.subject.as_str()))
        });
        if let Some((subject, reference)) = owed {
            entries.retain(|entry| entry.subject != subject);
            if entries.len() >= MAX_UNRESOLVED {
                entries.remove(0);
            }
            entries.push(Entry { subject, reference });
        }
    }

    /// Returns `actor`'s unresolved subjects, oldest first.
    pub fn subjects(&self, actor: &str) -> Vec<String> {
        self.0
            .lock()
            .ok()
            .and_then(|actors| {
                actors
                    .iter()
                    .find(|(known, _)| known == actor)
                    .map(|(_, entries)| entries.iter().map(|entry| entry.subject.clone()).collect())
            })
            .unwrap_or_default()
    }

    /// Forgets `actor`: its restart notice obtained accepted or uncertain Bridge custody, or the
    /// actor stopped.
    pub fn forget(&self, actor: &str) {
        if let Ok(mut actors) = self.0.lock() {
            actors.retain(|(known, _)| known != actor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistance::reply::ResultKind;

    /// A settled test reply or line.
    fn test_reply(text: &str) -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Test,
            text: text.to_owned(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        }
    }

    /// A plain answered read.
    fn read_reply(text: &str) -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: text.to_owned(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        }
    }

    /// A started test retains its actual job locator, so reading saved output resolves that
    /// run without guessing a tests-subject string from an unrelated detail reference.
    #[test]
    fn test_output_reference_resolves_the_started_run() {
        let memory = Unresolved::default();
        let started = PeerReply::Complete {
            kind: ResultKind::Test,
            text: "tests #3: started".into(),
            detail_ref: Some("opaque-output-ref".into()),
            truncated: false,
            continuation: false,
        };
        memory.observe("a", AssistanceTool::Test, &Value::Null, &started);
        assert_eq!(memory.subjects("a"), ["tests #3"]);
        memory.observe(
            "a",
            AssistanceTool::Test,
            &serde_json::json!({"detail_ref":"opaque-output-ref"}),
            &read_reply("captured output"),
        );
        assert!(memory.subjects("a").is_empty());
    }

    /// History/cancel acknowledgements do not pretend to deliver a test result; successful
    /// originating-tool continuation resolves only the exact actor's owed reference.
    #[test]
    fn domain_queries_only_resolve_results_they_deliver() {
        let memory = Unresolved::default();
        memory.observe(
            "a",
            AssistanceTool::Test,
            &Value::Null,
            &test_reply("tests #3: started"),
        );
        memory.observe(
            "a",
            AssistanceTool::Test,
            &serde_json::json!({"history":true}),
            &test_reply("tests #3: 2 passed"),
        );
        memory.observe(
            "a",
            AssistanceTool::Test,
            &serde_json::json!({"cancel":3}),
            &test_reply("tests #3: already settled"),
        );
        assert_eq!(memory.subjects("a"), ["tests #3"]);
        for tool in [
            AssistanceTool::Read,
            AssistanceTool::Graph,
            AssistanceTool::Test,
        ] {
            let reference = format!("reference-{}", tool.mcp_name());
            memory.observe(
                "b",
                tool,
                &Value::Null,
                &PeerReply::Pending {
                    detail_ref: reference.clone(),
                },
            );
            assert_eq!(memory.subjects("b").len(), 1);
            memory.observe(
                "b",
                tool,
                &serde_json::json!({"detail_ref":reference}),
                &read_reply("body"),
            );
            assert!(memory.subjects("b").is_empty());
        }
    }

    /// One plural Inspect continuation clears every exact reference it delivered and leaves an
    /// unrelated owed result intact.
    #[test]
    fn plural_inspect_resolves_every_explicit_reference() {
        let memory = Unresolved::default();
        for reference in ["detail-a", "detail-b", "detail-c"] {
            memory.observe(
                "a",
                AssistanceTool::Symbol,
                &serde_json::json!({"symbol":format!("src/lib.rs#{reference}")}),
                &PeerReply::Pending {
                    detail_ref: reference.to_owned(),
                },
            );
        }
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_refs":["detail-a","detail-c"]}),
            &read_reply("two retained results"),
        );
        assert_eq!(memory.subjects("a"), ["ide.symbol src/lib.rs#detail-b"]);
    }

    /// A Diff continuation is an explicit read of its retained result, so it clears the exact
    /// pre-restart subject instead of reporting already-read work as lost.
    #[test]
    fn diff_continuation_resolves_its_explicit_reference() {
        let memory = Unresolved::default();
        memory.observe(
            "a",
            AssistanceTool::Diff,
            &serde_json::json!({"mode":"head"}),
            &PeerReply::Pending {
                detail_ref: "diff-page".to_owned(),
            },
        );
        assert_eq!(memory.subjects("a"), ["ide.diff call"]);
        memory.observe(
            "a",
            AssistanceTool::Diff,
            &serde_json::json!({"detail_ref":"diff-page"}),
            &read_reply("remaining diff"),
        );
        assert!(memory.subjects("a").is_empty());
    }

    /// Pending answers, started runs and pending checks are remembered per actor until their
    /// fact, their reference or the settled run is relayed; answered calls add nothing.
    #[test]
    fn owed_work_is_remembered_until_relayed() {
        let memory = Unresolved::default();
        let none = Value::Null;
        memory.observe(
            "a",
            AssistanceTool::Symbol,
            &serde_json::json!({"symbol":"src/a.rs#Foo"}),
            &PeerReply::Pending {
                detail_ref: "sym-1".to_owned(),
            },
        );
        memory.observe(
            "a",
            AssistanceTool::Test,
            &none,
            &test_reply("tests #3: started — cargo test (budget 60 s)"),
        );
        memory.observe(
            "b",
            AssistanceTool::Read,
            &serde_json::json!({"path":"src/b.rs"}),
            &PeerReply::Pending {
                detail_ref: "read-2".to_owned(),
            },
        );
        memory.observe("c", AssistanceTool::Read, &none, &read_reply("x"));
        assert_eq!(
            memory.subjects("a"),
            ["ide.symbol src/a.rs#Foo", "tests #3"]
        );
        assert_eq!(memory.subjects("b"), ["ide.read src/b.rs"]);
        assert!(memory.subjects("c").is_empty());
        // A running status resolves nothing.
        memory.observe(
            "a",
            AssistanceTool::Test,
            &serde_json::json!({"status":3}),
            &test_reply("tests #3: running"),
        );
        assert_eq!(
            memory.subjects("a"),
            ["ide.symbol src/a.rs#Foo", "tests #3"]
        );
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"sym-1"}),
            &read_reply("fn foo() {}"),
        );
        assert_eq!(memory.subjects("a"), ["tests #3"]);
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"tests #3"}),
            &test_reply("tests #3: 2 passed"),
        );
        assert!(memory.subjects("a").is_empty());
        // Reading the reference back resolves a pending answer; another actor's read does not.
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"read-2"}),
            &read_reply("fn b() {}"),
        );
        assert_eq!(memory.subjects("b"), ["ide.read src/b.rs"]);
        memory.observe(
            "b",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"read-2"}),
            &read_reply("fn b() {}"),
        );
        assert!(memory.subjects("b").is_empty());
    }

    /// At most sixteen subjects per actor are kept, oldest dropped, and forgetting clears one.
    #[test]
    fn memory_is_bounded_and_forgettable() {
        let memory = Unresolved::default();
        for id in 1..=20 {
            memory.observe(
                "a",
                AssistanceTool::Test,
                &Value::Null,
                &test_reply(&format!("tests #{id}: started")),
            );
        }
        let subjects = memory.subjects("a");
        assert_eq!(subjects.len(), MAX_UNRESOLVED);
        assert_eq!(subjects[0], "tests #5");
        memory.forget("a");
        assert!(memory.subjects("a").is_empty());
    }
}
