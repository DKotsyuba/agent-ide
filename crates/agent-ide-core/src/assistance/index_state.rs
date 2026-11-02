//! The start card's per-language servers line (start-index-state r2 §2).
//!
//! `ide.start` launches no language server: the first `ide.symbol` or `ide.graph` call of a
//! language starts it. The card reports each detected server language's state as the worktree's
//! session owner holds it at render time: `ready`, `warming`, `not started` or
//! `unavailable (<cause>)`. Nothing is probed or started for the card.

use super::*;
use crate::intelligence::session::ReadinessError;

impl Worker<'_> {
    /// The servers-line states of `languages` (detected languages; those without a server are
    /// skipped) for `binding`'s worktree, in the given order.
    ///
    /// The serving session is the binding's own, or the one of the writer (or reading owner) a
    /// reader borrows from. A live session reports `ready` or `warming`; a failed one
    /// `unavailable (workspace failed to load)` or `unavailable (transport gone)`; none at all
    /// `not started`.
    pub(super) async fn server_states(
        &mut self,
        binding: &BindingRef,
        languages: &[crate::lang::Language],
    ) -> Vec<CardServerState> {
        let mut states = Vec::new();
        for &language in languages {
            let Some(server) = language.server() else {
                continue;
            };
            let state = match self.session_readiness(binding, server).await {
                None => project_card::not_started_state(language).to_owned(),
                Some(Ok(())) => project_card::READY_STATE.to_owned(),
                Some(Err(ReadinessError::Loading)) => {
                    project_card::warming_state(language).to_owned()
                }
                Some(Err(ReadinessError::WorkspaceError)) => {
                    project_card::unavailable_state("workspace failed to load")
                }
                Some(Err(ReadinessError::Gone)) => {
                    project_card::unavailable_state("transport gone")
                }
            };
            states.push(CardServerState { language, state });
        }
        states
    }
}
