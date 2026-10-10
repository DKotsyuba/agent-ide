//! Worker side of the stale-writer takeover at `ide.start` (start-index-state r2 §4).
//!
//! When a writer start collides with another actor's writer, the holder's liveness evidence
//! (see [`crate::assistance::carriers`]) decides: a holder whose session ended, or one that has
//! been inactive with nothing in flight for 30 minutes, is stopped exactly as `ide.stop` would
//! stop it and the start goes ahead; a live holder keeps its writer and the start is refused as
//! before. The displaced actor's next call is told `taken_over`.

use super::*;
use crate::assistance::carriers;
use crate::assistance::host_binding::TakeoverReason;
use crate::workspace::{authority::StartRole, durable::DurableError};

impl Worker<'_> {
    /// Takes over `tree`'s writer for this start when its holder is another actor that is gone or
    /// long inactive, and re-runs the activation.
    ///
    /// `operation` and `read_only` are this start's own activation request, as for
    /// `hand_over`. Returns `Ok(None)` and changes nothing when the writer belongs to this actor,
    /// is no longer held, has no live grant in this daemon, belongs to a live holder, or its
    /// revocation could not settle; the caller then keeps its original refusal. Otherwise returns
    /// the re-run activation together with the reply phrase
    /// `took over the writer from actor <actor> (activation <id>, last activity <time>;
    /// <evidence>)`. The holder is fenced against new work while the takeover runs, revoked
    /// durably, its sessions and retained results are released, its queued work can no longer
    /// run, and its scope is recorded as displaced so its next call answers `taken_over`. The
    /// takeover is journaled with its evidence.
    ///
    /// # Errors
    ///
    /// Only what the re-run activation request itself refuses (this start's binding is no
    /// longer active).
    pub(super) async fn take_over_stale_writer(
        &mut self,
        job: &mut Job,
        tree: &crate::workspace::authority::WorktreeRef,
        operation: &str,
        read_only: bool,
    ) -> Result<Option<(Result<StartReceipt, DurableError>, String)>, FailureCode> {
        let holder = match self
            .workspace
            .start_holder(tree, job.invocation.actor_id())
            .await
        {
            Ok(Some(holder)) if !holder.same_actor && holder.role == StartRole::Writer => holder,
            _ => return Ok(None),
        };
        let Some(holder_binding) = self.binding_of_holder(&holder) else {
            return Ok(None);
        };
        let Some(evidence) = self
            .shared
            .bindings
            .lock()
            .ok()
            .map(|guard| guard.holder_evidence(&holder_binding))
        else {
            return Ok(None);
        };
        let Some(reason) = self.fence_stale_holder(&holder_binding, evidence, &job.reference)
        else {
            return Ok(None);
        };
        let last = self
            .activity
            .get(&holder.binding)
            .map(|ms| timestamp_line(*ms))
            .unwrap_or_else(|| "unknown".to_owned());
        let line = format!(
            "took over the writer from actor {} (activation {}, last activity {last}; {})",
            holder.actor,
            holder.activation,
            reason.evidence()
        );
        let moved = self
            .hand_over(job, &holder_binding, tree, operation, read_only)
            .await;
        self.end_handover_fence(&holder_binding);
        let activation = match moved {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(_)) | Err(_) => return Ok(None),
        };
        if let Ok(mut guard) = self.shared.bindings.lock() {
            let _ = guard.displace(&holder_binding, reason);
        }
        crate::errorlog::record(
            errorlog_method(job.tool),
            crate::errorlog::Outcome::Completed,
            crate::errorlog::Fields {
                actor: Some(job.invocation.actor_id()),
                worktree: Some(tree.worktree_path()),
                correlation: Some(job.reference.as_str()),
                detail: Some(&format!(
                    "takeover: actor {} ({})",
                    holder.actor,
                    takeover_tag(reason)
                )),
                ..Default::default()
            },
        );
        Ok(Some((Ok(activation), line)))
    }

    /// Decides whether `holder` may be taken over and, if so, fences it against new IDE work and
    /// returns why; `None` leaves no fence.
    ///
    /// `evidence` is the holder's liveness snapshot read under the bindings lock; `current` is the
    /// start job that asks. The pending-work verdict, [`carriers::assess`] and the fence are one
    /// critical section of the ledger, as for a handover, so work the holder submits afterwards is
    /// refused at ingress. Hooks take only the bindings lock, so the holder's evidence is read again
    /// once the fence stands: a hook, a newly started call or a carrier change that landed after
    /// the snapshot lifts the fence and returns `None`, and a holder that showed life is never
    /// displaced. Whatever the holder sends after that second read is ordered after the takeover.
    pub(super) fn fence_stale_holder(
        &self,
        holder: &BindingRef,
        mut evidence: carriers::HolderEvidence,
        current: &str,
    ) -> Option<TakeoverReason> {
        let reason = {
            let mut ledger = self.shared.ledger.lock().ok()?;
            evidence.pending_work = self.pending_work(&ledger, holder, current);
            let reason = carriers::assess(&evidence, std::time::Instant::now())?;
            ledger.handing_over.insert(holder.clone());
            reason
        };
        let unchanged = self.shared.bindings.lock().ok().is_some_and(|guard| {
            let fresh = guard.holder_evidence(holder);
            fresh.last_seen == evidence.last_seen
                && fresh.call_in_flight == evidence.call_in_flight
                && fresh.carrier_ended == evidence.carrier_ended
        });
        if unchanged {
            return Some(reason);
        }
        self.end_handover_fence(holder);
        None
    }

    /// What IDE work of `holder` is still pending, in plain words, or `None`: uncertain cleanup,
    /// an edit with `outcome_unknown`, a running test, a queued job (an edit or a staged job named
    /// as a pending edit), or a pending retained result other than `current` (the start job that
    /// asks). `ledger` is the caller's own lock of the shared ledger, so the verdict and any fence
    /// the caller then sets are one critical section.
    pub(super) fn pending_work(
        &self,
        ledger: &Ledger,
        holder: &BindingRef,
        current: &str,
    ) -> Option<&'static str> {
        if self.uncertain.contains(holder) {
            return Some("an unsettled operation");
        }
        if self.unsettled_edits.contains_key(holder) {
            return Some("an edit with outcome_unknown");
        }
        if self
            .shared
            .test_runs
            .uncollected(&holder.fingerprint())
            .iter()
            .any(|(_, _, running)| *running)
        {
            return Some("a running test");
        }
        if let Some(job) = ledger
            .queue
            .iter()
            .find(|job| job.invocation.binding_ref() == holder)
        {
            return Some(if job.tool == AssistanceTool::Edit || job.stage.is_some() {
                "a pending edit"
            } else {
                "queued work"
            });
        }
        ledger
            .details
            .iter()
            .any(|(reference, detail)| {
                &detail.binding == holder
                    && matches!(detail.reply, PeerReply::Pending { .. })
                    && reference != current
            })
            .then_some("a pending job")
    }
}

/// The journal tag of a takeover's evidence: `session_ended` or `inactive`.
fn takeover_tag(reason: TakeoverReason) -> &'static str {
    match reason {
        TakeoverReason::SessionEnded => "session_ended",
        TakeoverReason::Inactive { .. } => "inactive",
    }
}
