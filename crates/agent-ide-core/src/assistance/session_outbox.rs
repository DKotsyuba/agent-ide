//! The daemon's in-memory per-session ledger behind Bridge-only delivery (OUTBOX-r3 §7).
//!
//! [`Outbox`](crate::assistance::session_outbox::Outbox) keeps, per active session, the change
//! counters that fence a result against later edits and native writes, and the in-flight work whose
//! completion fact the worker hands to Bridge. It queues no fact: delivery, retry and wake belong to
//! Bridge. Everything is guarded by one short
//! `std::sync::Mutex`, and no lock is held across an await.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::host_binding::BindingRef;
use super::outbox::JobEpoch;

/// One unit of in-flight work whose completion fact is still owed to its owner.
struct InFlight {
    /// Session that will receive the fact.
    owner: BindingRef,
    /// Further sessions that asked for the same work (a second `ide.test` on a busy worktree).
    subscribers: Vec<BindingRef>,
    /// The owner's counters when the work began, the fence epoch of its fact.
    epoch: JobEpoch,
}

impl InFlight {
    /// Returns whether `binding` receives this work's fact (owner or subscriber).
    fn reaches(&self, binding: &BindingRef) -> bool {
        &self.owner == binding || self.subscribers.contains(binding)
    }
}

/// In-flight work removed by [`Outbox::take_work`]: who receives its fact and from which epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakenWork {
    /// The session that started the work.
    pub owner: BindingRef,
    /// Sessions that subscribed to the same work.
    pub subscribers: Vec<BindingRef>,
    /// The owner's counters when the work began.
    pub epoch: JobEpoch,
}

/// Per-session ledger state.
#[derive(Default)]
struct Session {
    /// Native-hook epoch mirror (advanced by possible-writer native tools).
    native_epoch: u64,
    /// Count of settled `ide.edit` operations.
    edit_seq: u64,
}

impl Session {
    /// Returns the counters a job starting now captures.
    fn epoch(&self) -> JobEpoch {
        JobEpoch {
            native_epoch: self.native_epoch,
            edit_seq: self.edit_seq,
        }
    }
}

/// Mutable store state behind the single lock.
#[derive(Default)]
struct State {
    /// Active sessions.
    sessions: BTreeMap<BindingRef, Session>,
    /// In-flight work keyed by an opaque key (`detail_ref`, `tests #N`, `edit <op>`).
    in_flight: BTreeMap<String, InFlight>,
}

/// The daemon's session ledger; see the module documentation.
#[derive(Default)]
pub struct Outbox {
    /// All mutable state.
    state: Mutex<State>,
}

impl Outbox {
    /// Opens (or keeps) the ledger of a newly activated session.
    pub fn open(&self, binding: &BindingRef) {
        if let Ok(mut state) = self.state.lock() {
            state.sessions.entry(binding.clone()).or_default();
        }
    }

    /// Returns whether `binding` has an open ledger.
    pub fn is_open(&self, binding: &BindingRef) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| state.sessions.contains_key(binding))
    }

    /// Runs `change` on the session of `binding`, if open; returns its result.
    fn with_session<T>(
        &self,
        binding: &BindingRef,
        change: impl FnOnce(&mut Session) -> T,
    ) -> Option<T> {
        let mut state = self.state.lock().ok()?;
        state.sessions.get_mut(binding).map(change)
    }

    /// Records that a possible-writer native tool ran in this session (native epoch + 1).
    pub fn note_native(&self, binding: &BindingRef) {
        self.with_session(binding, |session| {
            session.native_epoch = session.native_epoch.saturating_add(1);
        });
    }

    /// Records one settled `ide.edit` operation of this session.
    pub fn note_edit(&self, binding: &BindingRef) {
        self.with_session(binding, |session| {
            session.edit_seq = session.edit_seq.saturating_add(1);
        });
    }

    /// Returns the session's current counters, the epoch a job starting now captures.
    pub fn current_epoch(&self, binding: &BindingRef) -> JobEpoch {
        self.with_session(binding, |session| session.epoch())
            .unwrap_or_default()
    }

    /// Registers in-flight work under `key` that owes a completion fact to `owner`, capturing the
    /// owner's current counters as the fact's fence epoch.
    ///
    /// Returns whether it was registered: the owner's ledger must be open and `key` unused.
    pub fn begin_work(&self, key: &str, owner: &BindingRef) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            let Some(session) = state.sessions.get(owner) else {
                return false;
            };
            let epoch = session.epoch();
            if state.in_flight.contains_key(key) {
                return false;
            }
            state.in_flight.insert(
                key.to_owned(),
                InFlight {
                    owner: owner.clone(),
                    subscribers: Vec::new(),
                    epoch,
                },
            );
            true
        })
    }

    /// Adds `binding` as a subscriber of the in-flight work under `key`; returns whether it was
    /// added (the work exists, the binding's ledger is open and it is not already a recipient).
    pub fn subscribe(&self, key: &str, binding: &BindingRef) -> bool {
        self.state.lock().is_ok_and(|mut state| {
            if !state.sessions.contains_key(binding) {
                return false;
            }
            match state.in_flight.get_mut(key) {
                Some(work) if !work.reaches(binding) => {
                    work.subscribers.push(binding.clone());
                    true
                }
                _ => false,
            }
        })
    }

    /// Removes the in-flight work under `key` and returns who receives its fact, exactly once:
    /// a second call (or a call for unknown work) returns `None`.
    pub fn take_work(&self, key: &str) -> Option<TakenWork> {
        let taken = self.state.lock().ok()?.in_flight.remove(key)?;
        Some(TakenWork {
            owner: taken.owner,
            subscribers: taken.subscribers,
            epoch: taken.epoch,
        })
    }

    /// Removes the session and its in-flight work silently.
    pub fn forget_session(&self, binding: &BindingRef) {
        if let Ok(mut state) = self.state.lock() {
            state.sessions.remove(binding);
            state.in_flight.retain(|_, work| &work.owner != binding);
            for work in state.in_flight.values_mut() {
                work.subscribers.retain(|subscriber| subscriber != binding);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session binding fixture.
    fn binding(actor: &str) -> BindingRef {
        BindingRef::fixture(actor, "channel-a", 1)
    }

    /// Edits and native writes advance the epoch a later job captures; an unopened session reports
    /// the default epoch.
    #[test]
    fn counters_advance_the_captured_epoch() {
        let outbox = Outbox::default();
        let session = binding("a");
        assert_eq!(outbox.current_epoch(&session), JobEpoch::default());
        outbox.open(&session);
        outbox.note_edit(&session);
        outbox.note_native(&session);
        outbox.note_edit(&session);
        assert_eq!(
            outbox.current_epoch(&session),
            JobEpoch {
                native_epoch: 1,
                edit_seq: 2
            }
        );
    }

    /// In-flight work is registered once per key, captures the owner's epoch, reaches its
    /// subscribers and is taken exactly once.
    #[test]
    fn in_flight_work_is_taken_exactly_once() {
        let outbox = Outbox::default();
        let owner = binding("a");
        let reader = binding("b");
        outbox.open(&owner);
        outbox.open(&reader);
        outbox.note_edit(&owner);
        assert!(outbox.begin_work("tests #1", &owner));
        assert!(
            !outbox.begin_work("tests #1", &owner),
            "a key is registered once"
        );
        assert!(!outbox.begin_work("tests #2", &binding("closed")));
        assert!(outbox.subscribe("tests #1", &reader));
        assert!(!outbox.subscribe("tests #1", &reader));
        assert!(!outbox.subscribe("tests #1", &owner));
        let work = outbox.take_work("tests #1").expect("registered work");
        assert_eq!(work.owner, owner);
        assert_eq!(work.subscribers, vec![reader]);
        assert_eq!(work.epoch.edit_seq, 1);
        assert_eq!(outbox.take_work("tests #1"), None);
    }

    /// Forgetting a session closes it and drops the work it owned or subscribed to.
    #[test]
    fn forgetting_a_session_drops_its_work() {
        let outbox = Outbox::default();
        let owner = binding("a");
        let reader = binding("b");
        outbox.open(&owner);
        outbox.open(&reader);
        assert!(outbox.begin_work("owned", &owner));
        assert!(outbox.begin_work("shared", &reader));
        assert!(outbox.subscribe("shared", &owner));
        outbox.forget_session(&owner);
        assert!(!outbox.is_open(&owner));
        assert_eq!(outbox.take_work("owned"), None);
        let shared = outbox.take_work("shared").expect("the other owner's work");
        assert!(shared.subscribers.is_empty());
    }
}
