//! Agent turn state, the Stop/SubagentStop hold and the compaction signal (OUTBOX-r3 §8.3, §9).
//!
//! The host tells the daemon when an agent ends its turn (`Stop`, `SubagentStop`) and when its
//! context was compacted. A turn end while the agent's own requested work is still in flight is
//! held, inside the hook, until the first holding fact is ready or the bounded hold deadline
//! passes; a fact continues the same turn as the host's block reason, which asks the agent to
//! restate its complete final answer (agent runners keep the last assistant text as the answer).
//! Without a fact the hold releases and the session counts as idle.

use std::path::{Path, PathBuf};

use super::super::{
    host_binding::{BindingRef, HostKind},
    outbox::{Carrier, HOLD_CAP},
    reply::MAX_FEEDBACK_BYTES,
    session_outbox::AgentState,
};
use super::WorkerHandle;

/// The sentence that ends every hold's block reason (OUTBOX §8.3 rule 4).
pub(crate) const RESTATE: &str = "These results you requested arrived as you ended your turn. Take \
     them into account, then restate your complete final answer.";
/// Most registry descriptors read while looking for one Claude session.
const MAX_REGISTRY_FILES: usize = 512;
/// Largest registry descriptor read.
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;

/// Returns the Claude configuration directory: `CLAUDE_CONFIG_DIR`, else `$HOME/.claude`.
pub(crate) fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

/// Reads the Claude session-registry descriptor (`<registry>/<pid>.json`) whose `sessionId` is
/// `session_id`, as a JSON object; `None` when there is none, or it is unreadable or oversized.
///
/// The registry is read-only here and bounded to [`MAX_REGISTRY_FILES`] descriptors of at most
/// [`MAX_DESCRIPTOR_BYTES`]; malformed entries are skipped.
pub(crate) fn registry_entry(
    registry: &Path,
    session_id: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut paths = std::fs::read_dir(registry)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().take(MAX_REGISTRY_FILES).find_map(|path| {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_DESCRIPTOR_BYTES {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        let object = value.as_object()?;
        (object.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id))
            .then(|| object.clone())
    })
}

/// Whether `session_id` names an interactive Claude session in the registry under `registry`:
/// only such a session may hold a `SubagentStop` (OUTBOX §8.3 rule 6), because a headless
/// session's background subagent outliving its parent's result breaks agent runners.
pub(crate) fn interactive_claude_session(registry: &Path, session_id: &str) -> bool {
    registry_entry(registry, session_id).is_some_and(|entry| {
        entry.get("kind").and_then(serde_json::Value::as_str) == Some("interactive")
    })
}

impl WorkerHandle {
    /// Decides one turn end of `binding` (OUTBOX §8.3): returns the block reason that continues
    /// the turn with the session's due facts, or `None` to let the turn end.
    ///
    /// `subagent` marks a Claude `SubagentStop`, held only for a session the registry proves
    /// interactive. `session_id` is the host session the hook named; it is remembered for the
    /// idle wake. With an undelivered holding fact the turn is blocked at once; with holding work
    /// in flight the hold waits until the first holding fact or the earlier of [`HOLD_CAP`] and the
    /// work's budget end; otherwise it releases. A released session is idle; a blocked one keeps
    /// working. The hold never blocks without a fact, and a fact is never handed over twice.
    pub async fn hold_turn(
        &self,
        binding: &BindingRef,
        subagent: bool,
        session_id: Option<&str>,
    ) -> Option<String> {
        let outbox = &self.shared.outbox;
        if !outbox.is_open(binding) {
            return None;
        }
        if let Some(session_id) = session_id {
            outbox.note_host_session(binding, session_id);
        }
        let interactive = || {
            session_id.is_some_and(|session_id| {
                claude_config_dir().is_some_and(|config| {
                    interactive_claude_session(&config.join("sessions"), session_id)
                })
            })
        };
        if subagent && (binding.host() != HostKind::Claude || !interactive()) {
            outbox.set_agent_state(binding, AgentState::Idle);
            return None;
        }
        outbox.set_agent_state(binding, AgentState::Ending);
        let now = tokio::time::Instant::now();
        let deadline = outbox
            .holding_until(binding)
            .map_or(now, |until| until.min(now + HOLD_CAP));
        let ready = outbox.has_undelivered(binding, true)
            || outbox.wait_ready(binding, true, deadline).await;
        if ready
            && let Some(block) = self.take_facts(
                binding,
                Carrier::StopHold,
                None,
                MAX_FEEDBACK_BYTES - RESTATE.len() - 1,
                |_| true,
            )
        {
            outbox.set_agent_state(binding, AgentState::Working);
            return Some(format!("{block}\n{RESTATE}"));
        }
        outbox.set_agent_state(binding, AgentState::Idle);
        None
    }

    /// Records a compaction or clear of `binding`'s host session: every session on its channel
    /// raises its compaction counter (OUTBOX §9.1).
    pub fn note_compaction(&self, binding: &BindingRef) {
        self.shared.outbox.note_hook(binding);
        self.shared.outbox.note_compaction(binding);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a descriptor naming the session with kind `interactive` proves an interactive session.
    #[test]
    fn registry_proves_interactive_sessions_only() {
        let registry =
            std::path::PathBuf::from(format!("/private/tmp/m026-registry-{}", std::process::id()));
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            registry.join("100.json"),
            r#"{"pid":100,"sessionId":"live","kind":"interactive","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(
            registry.join("200.json"),
            r#"{"pid":200,"sessionId":"headless","kind":"sdk","status":"busy"}"#,
        )
        .unwrap();
        std::fs::write(registry.join("300.json"), "not json").unwrap();
        assert!(interactive_claude_session(&registry, "live"));
        assert!(!interactive_claude_session(&registry, "headless"));
        assert!(!interactive_claude_session(&registry, "absent"));
        assert_eq!(
            registry_entry(&registry, "live").and_then(|entry| entry.get("status").cloned()),
            Some(serde_json::json!("idle"))
        );
        let _ = std::fs::remove_dir_all(&registry);
    }

    /// The restate sentence leaves room for facts inside the hook budget.
    #[test]
    fn the_restate_sentence_fits_the_hook_budget() {
        assert!(RESTATE.len() < MAX_FEEDBACK_BYTES / 8);
        assert!(!RESTATE.contains("poll"));
    }
}
