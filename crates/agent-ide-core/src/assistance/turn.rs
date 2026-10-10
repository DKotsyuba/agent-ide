//! Agent turn state, the Stop/SubagentStop hold and the compaction signal (OUTBOX-r3 §8.3, §9).
//!
//! The host tells the daemon when an agent ends its turn (`Stop`, `SubagentStop`) and when its
//! context was compacted. A turn end while the agent's own requested work is still in flight is
//! held, inside the hook, until the first holding fact is ready or the bounded hold deadline
//! passes; a fact continues the same turn as the host's block reason, which asks the agent to
//! restate its complete final answer (agent runners keep the last assistant text as the answer).
//! Without a fact the hold releases and the session counts as idle.

use super::super::{
    host_binding::{BindingRef, HostKind},
    outbox::{Carrier, HOLD_CAP},
    reply::MAX_FEEDBACK_BYTES,
    session_outbox::AgentState,
    wake::{claude_config_dir, interactive_claude_session},
};
use super::WorkerHandle;

/// The sentence that ends every hold's block reason (OUTBOX §8.3 rule 4).
pub(crate) const RESTATE: &str = "These results you requested arrived as you ended your turn. Take \
     them into account, then restate your complete final answer.";

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

    /// The restate sentence leaves room for facts inside the hook budget.
    #[test]
    fn the_restate_sentence_fits_the_hook_budget() {
        assert!(RESTATE.len() < MAX_FEEDBACK_BYTES / 8);
        assert!(!RESTATE.contains("poll"));
    }
}
