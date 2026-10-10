//! The MCP front's memory of requested work it relayed unresolved (OUTBOX-r3 §10).
//!
//! The session outbox lives in the daemon and dies with it. The MCP front, one per host session,
//! survives a daemon replacement, so it remembers per actor up to
//! [`MAX_UNRESOLVED`](crate::assistance::unresolved::MAX_UNRESOLVED) subjects of work whose
//! caller got no answer: a `pending` reply, a `tests #N: started` line, an edit with
//! `diagnostics: pending`. A subject is resolved when the front relays its fact, reads its
//! reference back, or relays the settled run. When the actor is re-activated on a new daemon,
//! the subjects travel with the re-activation and the new daemon queues one `lost-after-restart`
//! fact. A hook that delivered a result is invisible here, so that fact says "may be lost".

use std::sync::Mutex;

use serde_json::Value;

use super::{
    facade::AssistanceTool,
    outbox::FactKind,
    reply::{EditDiagnostics, PeerReply},
    session_outbox::sanitize,
    worker::{owed_fact, settled_run},
};

/// Most unresolved subjects remembered per actor (OUTBOX §10).
pub const MAX_UNRESOLVED: usize = 16;
/// Most actors remembered; the oldest is dropped first.
const MAX_ACTORS: usize = 32;

/// One relayed request whose result the front has not seen.
struct Entry {
    /// Kind of the fact that will answer it.
    kind: FactKind,
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
    /// Updates `actor`'s memory with one relayed reply to `tool` called with `parameters`: what
    /// the reply resolves is forgotten, what it leaves owed is remembered.
    pub fn observe(
        &self,
        actor: &str,
        tool: AssistanceTool,
        parameters: &Value,
        reply: &PeerReply,
        status: Option<&str>,
    ) {
        let text = match reply {
            PeerReply::Complete { text, .. } => text.as_str(),
            _ => "",
        };
        let answered = !matches!(
            reply,
            PeerReply::Pending { .. }
                | PeerReply::Edit {
                    diagnostics: EditDiagnostics::Pending { .. },
                    ..
                }
        );
        let read = (tool == AssistanceTool::Inspect && answered)
            .then(|| parameters.get("detail_ref").and_then(Value::as_str))
            .flatten();
        let settled = settled_run(reply).map(|id| format!("tests #{id}"));
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
            let line = format!("{} {}:", entry.kind.label(), sanitize(&entry.subject));
            let delivered =
                text.contains(&line) || status.is_some_and(|block| block.contains(&line));
            let read_back = read.is_some_and(|read| {
                entry.reference.as_deref() == Some(read) || entry.subject == read
            });
            !(delivered || read_back || settled.as_deref() == Some(entry.subject.as_str()))
        });
        if let Some((kind, subject, reference)) = owed {
            entries.retain(|entry| entry.subject != subject);
            if entries.len() >= MAX_UNRESOLVED {
                entries.remove(0);
            }
            entries.push(Entry {
                kind,
                subject,
                reference,
            });
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

    /// Forgets `actor`: its subjects were reported after a restart, or it stopped.
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
            None,
        );
        memory.observe(
            "a",
            AssistanceTool::Test,
            &none,
            &test_reply("tests #3: started — cargo test (budget 60 s)"),
            None,
        );
        memory.observe(
            "b",
            AssistanceTool::Read,
            &serde_json::json!({"path":"src/b.rs"}),
            &PeerReply::Pending {
                detail_ref: "read-2".to_owned(),
            },
            None,
        );
        memory.observe("c", AssistanceTool::Read, &none, &read_reply("x"), None);
        assert_eq!(
            memory.subjects("a"),
            ["ide.symbol src/a.rs#Foo", "tests #3"]
        );
        assert_eq!(memory.subjects("b"), ["ide.read src/b.rs"]);
        assert!(memory.subjects("c").is_empty());
        // A carried fact resolves its subject; a running status does not.
        memory.observe(
            "a",
            AssistanceTool::Test,
            &serde_json::json!({"status":3}),
            &test_reply("tests #3: running"),
            Some("<agent-ide>\njob-ready ide.symbol src/a.rs#Foo: ready\n</agent-ide>"),
        );
        assert_eq!(memory.subjects("a"), ["tests #3"]);
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"tests #3"}),
            &test_reply("tests #3: 2 passed"),
            None,
        );
        assert!(memory.subjects("a").is_empty());
        // Reading the reference back resolves a pending answer; another actor's read does not.
        memory.observe(
            "a",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"read-2"}),
            &read_reply("fn b() {}"),
            None,
        );
        assert_eq!(memory.subjects("b"), ["ide.read src/b.rs"]);
        memory.observe(
            "b",
            AssistanceTool::Inspect,
            &serde_json::json!({"detail_ref":"read-2"}),
            &read_reply("fn b() {}"),
            None,
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
                None,
            );
        }
        let subjects = memory.subjects("a");
        assert_eq!(subjects.len(), MAX_UNRESOLVED);
        assert_eq!(subjects[0], "tests #5");
        memory.forget("a");
        assert!(memory.subjects("a").is_empty());
    }
}
