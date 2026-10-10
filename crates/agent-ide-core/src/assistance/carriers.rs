//! Liveness evidence of an activation's holder and the stale-writer takeover rule
//! (start-index-state r2 §4).
//!
//! A *carrier* is the process that carried a binding's MCP calls to the daemon: the MCP front of
//! the holder's host session, a child of that session. The daemon learns its process id from the
//! kernel for each call connection and for each long-lived client lease, and learns when the
//! front's lease reached EOF. A process id is liveness evidence only, never identity or
//! authority: a reused id can only make a gone holder look alive, which keeps its activation.
//!
//! [`assess`] decides from that evidence, the holder's last hook and what it has in flight
//! whether another actor's `ide.start` may take the writer over:
//! - **session ended**: the holder's carrier lease closed and nothing arrived from the holder
//!   afterwards (within [`CARRIER_SLACK`]);
//! - **inactive**: the holder has no tool call and no IDE work in flight and sent nothing for
//!   [`STALE_SILENCE`].
//!
//! Anything else is a live holder and is never displaced.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::host_binding::{BindingRef, TakeoverReason};

/// Silence after which a holder with nothing in flight is taken over as inactive.
pub(crate) const STALE_SILENCE: Duration = Duration::from_secs(30 * 60);

/// Grace after a carrier's lease closed during which a late hook of the same holder is still
/// counted as sent before the end (a post-hook racing the front's exit).
pub(crate) const CARRIER_SLACK: Duration = Duration::from_secs(5);

/// Closed-lease records kept; the oldest record goes first past this bound.
const MAX_CLOSED: usize = 256;

/// Most bindings whose latest carrier is remembered; the binding table itself holds at most 64.
const MAX_NOTED: usize = 256;

/// Carrier bookkeeping of one daemon boot. Owned by the host-binding guard, so it shares the
/// guard's lock; every method is synchronous and allocation-bounded.
#[derive(Debug, Default)]
pub(crate) struct Carriers {
    /// Open client leases per process id.
    open: BTreeMap<u32, usize>,
    /// When the last open lease of a process id reached EOF.
    closed: BTreeMap<u32, Instant>,
    /// Process id that carried each binding's latest validated call.
    by_binding: BTreeMap<BindingRef, u32>,
}

impl Carriers {
    /// Records that process `pid` opened a client lease; it is a live carrier from now on.
    pub(crate) fn opened(&mut self, pid: u32) {
        *self.open.entry(pid).or_default() += 1;
        self.closed.remove(&pid);
    }

    /// Records that one lease of process `pid` reached EOF at `now`. When it was the process's
    /// last open lease, the process is recorded as ended at `now`. An EOF for a process with no
    /// recorded open lease is ignored.
    pub(crate) fn closed(&mut self, pid: u32, now: Instant) {
        let Some(count) = self.open.get_mut(&pid) else {
            return;
        };
        *count -= 1;
        if *count > 0 {
            return;
        }
        self.open.remove(&pid);
        if self.closed.len() >= MAX_CLOSED
            && let Some(oldest) = self
                .closed
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(pid, _)| *pid)
        {
            self.closed.remove(&oldest);
        }
        self.closed.insert(pid, now);
    }

    /// Records that process `pid` carried a validated call of `binding` (the latest wins).
    pub(crate) fn note(&mut self, binding: &BindingRef, pid: u32) {
        if self.by_binding.len() >= MAX_NOTED && !self.by_binding.contains_key(binding) {
            return;
        }
        self.by_binding.insert(binding.clone(), pid);
    }

    /// Forgets `binding`'s carrier (its generation ended).
    pub(crate) fn forget(&mut self, binding: &BindingRef) {
        self.by_binding.remove(binding);
    }

    /// When the carrier of `binding`'s latest call ended: `Some` only when that process held a
    /// lease here, has none open now and its last lease closed at the returned instant. `None`
    /// when no carrier was noted, the carrier never held a lease here (a front without one), or
    /// it is still attached.
    pub(crate) fn ended_at(&self, binding: &BindingRef) -> Option<Instant> {
        let pid = self.by_binding.get(binding)?;
        if self.open.contains_key(pid) {
            return None;
        }
        self.closed.get(pid).copied()
    }
}

/// Everything [`assess`] weighs about one activation's holder.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HolderEvidence {
    /// When the holder's carrier ended ([`Carriers::ended_at`]).
    pub carrier_ended: Option<Instant>,
    /// When the holder last sent a hook or started; `None` when nothing was recorded.
    pub last_seen: Option<Instant>,
    /// The holder has a tool call in flight (a pre-hook without its post, a settling call).
    pub call_in_flight: bool,
    /// What IDE work of the holder is pending (a queued or parked job, a running test, an
    /// unsettled edit, uncertain cleanup), in plain words; `None` when nothing is.
    pub pending_work: Option<&'static str>,
}

/// Decides whether a writer whose holder shows `evidence` may be taken over at `now`.
///
/// Returns `Some(SessionEnded)` when the holder's carrier ended and the holder sent nothing
/// later than [`CARRIER_SLACK`] after that; `Some(Inactive { minutes })` when the holder has no
/// call and no IDE work in flight and its last hook or start is at least [`STALE_SILENCE`] old;
/// `None` for a live holder (or one with no recorded activity at all), which keeps its writer.
/// Pending IDE work of an ended session does not block: it can never be collected and is
/// cancelled by the takeover exactly as `ide.stop` cancels it.
pub(crate) fn assess(evidence: &HolderEvidence, now: Instant) -> Option<TakeoverReason> {
    if let Some(ended) = evidence.carrier_ended
        && evidence
            .last_seen
            .is_none_or(|seen| seen <= ended + CARRIER_SLACK)
    {
        return Some(TakeoverReason::SessionEnded);
    }
    let silence = now.saturating_duration_since(evidence.last_seen?);
    (!evidence.call_in_flight && evidence.pending_work.is_none() && silence >= STALE_SILENCE)
        .then_some(TakeoverReason::Inactive {
            minutes: silence.as_secs() / 60,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One holder binding for the ledger tests.
    fn holder() -> BindingRef {
        BindingRef::fixture("holder", "carrier-channel", 1)
    }

    /// A lease's process is a live carrier until its last lease closes; only then does a noted
    /// binding's carrier report its end, and a reopened lease makes it live again.
    #[test]
    fn a_carrier_ends_when_its_last_lease_closes() {
        let start = Instant::now();
        let mut carriers = Carriers::default();
        let binding = holder();
        carriers.note(&binding, 41);
        assert_eq!(carriers.ended_at(&binding), None, "never leased here");
        carriers.opened(41);
        carriers.opened(41);
        carriers.closed(41, start);
        assert_eq!(carriers.ended_at(&binding), None, "one lease still open");
        let later = start + Duration::from_secs(3);
        carriers.closed(41, later);
        assert_eq!(carriers.ended_at(&binding), Some(later));
        carriers.opened(41);
        assert_eq!(carriers.ended_at(&binding), None, "reused or reattached");
        carriers.closed(42, later);
        carriers.forget(&binding);
        assert_eq!(carriers.ended_at(&binding), None);
    }

    /// The takeover matrix: an ended session or a long-silent idle holder is taken over; a live
    /// carrier, recent activity, a call or IDE work in flight keep the writer.
    #[test]
    fn only_a_gone_or_long_idle_holder_is_taken_over() {
        let now = Instant::now() + STALE_SILENCE + Duration::from_secs(3600);
        let long_ago = now - STALE_SILENCE - Duration::from_secs(60);
        let recent = now - Duration::from_secs(60);
        let ended = now - Duration::from_secs(10);
        let case = |carrier_ended, last_seen, call_in_flight, pending_work| HolderEvidence {
            carrier_ended,
            last_seen,
            call_in_flight,
            pending_work,
        };
        let taken = [
            (
                case(Some(ended), Some(ended), false, None),
                "ended, quiet since",
            ),
            (
                case(Some(ended), Some(recent), true, Some("queued work")),
                "ended mid-work",
            ),
            (case(Some(ended), None, false, None), "ended, never heard"),
            (case(None, Some(long_ago), false, None), "idle 31 min"),
        ];
        for (evidence, label) in taken {
            assert!(assess(&evidence, now).is_some(), "{label}");
        }
        assert_eq!(
            assess(&case(Some(ended), Some(ended), false, None), now),
            Some(TakeoverReason::SessionEnded)
        );
        assert_eq!(
            assess(&case(None, Some(long_ago), false, None), now),
            Some(TakeoverReason::Inactive { minutes: 31 })
        );
        let kept = [
            (case(None, Some(recent), false, None), "live, recent hook"),
            (
                case(None, Some(long_ago), true, None),
                "silent but in a tool call",
            ),
            (
                case(None, Some(long_ago), false, Some("a running test")),
                "silent with a test",
            ),
            (case(None, None, false, None), "nothing recorded"),
            (
                case(
                    Some(ended),
                    Some(ended + CARRIER_SLACK + Duration::from_secs(1)),
                    false,
                    None,
                ),
                "hooks after the carrier closed (front restarted)",
            ),
        ];
        for (evidence, label) in kept {
            assert_eq!(assess(&evidence, now), None, "{label}");
        }
    }
}
