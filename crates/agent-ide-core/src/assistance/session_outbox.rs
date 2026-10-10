//! The daemon's in-memory session outbox store (OUTBOX-r3 §5-§7, §9).
//!
//! [`Outbox`](crate::assistance::session_outbox::Outbox) keeps, per active session, the undelivered facts producers pushed, a short
//! delivered history, the session's change counters used for staleness, its agent state and
//! compaction counter, and the in-flight work that may still produce a holding fact. It
//! implements the queue half of [`SessionOutbox`](crate::assistance::outbox::SessionOutbox); the worker adds `defer`, which needs its job
//! ledger. Everything is guarded by one short `std::sync::Mutex`; no lock is held across an
//! await, and waiting uses a [`tokio::sync::Notify`] that every state change signals.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use tokio::sync::Notify;
use tokio::time::Instant;

use super::host_binding::BindingRef;
use super::outbox::{
    Carrier, FactDraft, FactKind, HandoffRequest, JobEpoch, MAX_HISTORY, MAX_UNDELIVERED,
    PendingFact, Reservation,
};

/// Most `ide.edit` path records one session keeps for staleness labels.
const MAX_EDIT_RECORDS: usize = 128;
/// Most changed paths a staleness label names before `+K`.
const LABEL_PATHS: usize = 3;
/// Most bytes of one `ide.stop` flush (OUTBOX §11).
pub const STOP_FLUSH_BYTES: usize = 2048;
/// Most turn ends one session blocks in a row without a tool call in between. Claude 2.1.294
/// overrides the 9th consecutive Stop block and ends the turn with an empty answer; a tool call
/// resets its count (probe Q1).
pub const MAX_HOLDS_IN_ROW: u8 = 7;

/// What the host last told the daemon about the agent's turn (OUTBOX §9).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentState {
    /// Activated, or a Pre/Post hook or MCP call arrived, or a Stop hold blocked.
    #[default]
    Working,
    /// A Stop or SubagentStop hook is being held.
    Ending,
    /// A Stop or SubagentStop hook released without a block.
    Idle,
}

/// One stored fact with its delivery bookkeeping.
struct Stored {
    /// Per-session sequence number.
    seq: u64,
    /// The validated draft.
    draft: FactDraft,
    /// State the producing job started from.
    epoch: JobEpoch,
    /// Reservation token while a carrier holds it.
    reserved: Option<u64>,
}

/// One unit of in-flight work that may still produce a holding fact.
struct InFlight {
    /// Session that will receive the fact.
    owner: BindingRef,
    /// Further sessions that asked for the same work (a second `ide.test` on a busy worktree).
    subscribers: Vec<BindingRef>,
    /// Subject the fact will carry, listed by `ide.inspect {}` while nothing is ready.
    subject: String,
    /// Latest time the work can still settle (its budget end).
    until: Instant,
    /// The owner's counters when the work began, the fence epoch of its fact.
    epoch: JobEpoch,
}

impl InFlight {
    /// Returns whether `binding` receives this work's fact (owner or subscriber).
    fn reaches(&self, binding: &BindingRef) -> bool {
        &self.owner == binding || self.subscribers.contains(binding)
    }
}

/// In-flight work removed by [`Outbox::take_holding`]: who receives its fact and from which epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakenWork {
    /// The session that started the work.
    pub owner: BindingRef,
    /// Sessions that subscribed to the same work.
    pub subscribers: Vec<BindingRef>,
    /// The owner's counters when the work began.
    pub epoch: JobEpoch,
}

/// Per-session outbox state.
struct Session {
    /// Next sequence number to assign.
    next_seq: u64,
    /// Undelivered facts, including reserved ones, in push order.
    queue: Vec<Stored>,
    /// Summary lines of the last delivered facts, newest last.
    history: VecDeque<String>,
    /// Facts dropped by the bound since the last commit.
    dropped: usize,
    /// Compaction counter; `None` until the channel delivers a native hook.
    context_epoch: Option<u64>,
    /// Native-hook epoch mirror (advanced by possible-writer native tools).
    native_epoch: u64,
    /// Count of settled `ide.edit` operations.
    edit_seq: u64,
    /// `(edit_seq, worktree-relative path)` of recent edits, oldest first.
    edits: VecDeque<(u64, String)>,
    /// Agent turn state.
    agent: AgentState,
    /// True once a wake was attempted in the current idle period.
    woken: bool,
    /// Turn ends blocked since the agent last worked (a hook or MCP call).
    holds_in_row: u8,
    /// The host session identifier the hooks named (Claude `session_id`, Codex thread).
    host_session: Option<String>,
}

impl Session {
    /// A fresh session with nothing queued.
    fn new() -> Self {
        Self {
            next_seq: 1,
            queue: Vec::new(),
            history: VecDeque::new(),
            dropped: 0,
            context_epoch: None,
            native_epoch: 0,
            edit_seq: 0,
            edits: VecDeque::new(),
            agent: AgentState::Working,
            woken: false,
            holds_in_row: 0,
            host_session: None,
        }
    }

    /// Staleness label for a fact under this session's current counters (OUTBOX §7).
    fn stale_label(&self, stored: &Stored, carrier: Carrier) -> Option<String> {
        let fence = &stored.draft.fence_paths;
        let mut changed: Vec<&str> = Vec::new();
        for (_, path) in self
            .edits
            .iter()
            .filter(|(seq, _)| *seq > stored.epoch.edit_seq)
        {
            let normalized = path.trim_start_matches("./");
            let fenced = fence.is_empty()
                || fence
                    .iter()
                    .any(|fenced| fenced.path.trim_start_matches("./") == normalized);
            if fenced && !changed.contains(&normalized) {
                changed.push(normalized);
            }
        }
        let native = self.native_epoch > stored.epoch.native_epoch;
        if changed.is_empty() && !native {
            return None;
        }
        if stored.draft.source_bearing {
            // Body re-proof happens on reply-shaped carriers; hooks only label.
            return (!carrier.carries_source() || !changed.is_empty())
                .then(|| "(files changed since)".to_owned());
        }
        if changed.is_empty() {
            return Some("(ran before later changes)".to_owned());
        }
        let shown = changed
            .iter()
            .take(LABEL_PATHS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let more = changed.len().saturating_sub(LABEL_PATHS);
        Some(if more > 0 {
            format!("(ran before your later edits to {shown} +{more})")
        } else {
            format!("(ran before your later edits to {shown})")
        })
    }

    /// Undelivered, unreserved facts in delivery order: failures first, then ascending `seq`.
    fn ordered(&self) -> Vec<&Stored> {
        let mut facts = self
            .queue
            .iter()
            .filter(|stored| stored.reserved.is_none())
            .collect::<Vec<_>>();
        facts.sort_by_key(|stored| (!stored.draft.failed, stored.seq));
        facts
    }

    /// Moves one delivered fact's summary line into the bounded history.
    fn remember(&mut self, stored: &Stored) {
        let mut line = format!(
            "{} {}: {}",
            stored.draft.kind.label(),
            stored.draft.subject,
            stored.draft.summary
        );
        if let Some(reference) = &stored.draft.detail_ref {
            line.push_str(&format!(" (detail_ref {reference})"));
        }
        self.history.push_back(sanitize(&line));
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
    }
}

/// Replaces the framing characters `<` and `>` so data can never forge an `<agent-ide>` block.
pub fn sanitize(text: &str) -> String {
    text.replace(['<', '>'], "?")
}

/// Renders one fact for a carrier: the label line, then verbatim detail lines (unless the fact is
/// source-bearing and the carrier may not carry source), then a reference line when the detail
/// does not already name the reference.
pub fn render_fact(fact: &PendingFact) -> String {
    let mut text = format!(
        "{} {}: {}",
        fact.kind.label(),
        sanitize(&fact.subject),
        sanitize(&fact.summary)
    );
    if let Some(label) = &fact.stale {
        text.push(' ');
        text.push_str(label);
    }
    if let Some(detail) = fact.detail.as_deref().filter(|detail| !detail.is_empty()) {
        text.push('\n');
        text.push_str(&sanitize(detail));
    }
    if let Some(reference) = &fact.detail_ref
        && !fact
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains(reference.as_str()))
    {
        let label = if matches!(fact.kind, FactKind::JobReady | FactKind::EditChecked) {
            "result"
        } else {
            "full output"
        };
        text.push_str(&format!("\n  {label}: ide.inspect {}", sanitize(reference)));
    }
    text
}

/// Joins rendered facts into one `<agent-ide>` block after optional ambient plate lines.
///
/// `ambient` is a complete `<agent-ide>…</agent-ide>` plate or `None`; the facts are inserted
/// before its closing tag so the model reads one block. Returns `None` when there is nothing.
pub fn compose_block(ambient: Option<&str>, facts: &[String], overflow: usize) -> Option<String> {
    let mut lines = facts.to_vec();
    if overflow > 0 {
        lines.push(format!(
            "(+{overflow} more facts; ide.inspect {{}} lists them)"
        ));
    }
    match (ambient, lines.is_empty()) {
        (None, true) => None,
        (Some(plate), true) => Some(plate.to_owned()),
        (ambient, false) => {
            let head = ambient
                .and_then(|plate| plate.strip_suffix("</agent-ide>"))
                .map(|head| head.trim_end().to_owned())
                .unwrap_or_else(|| "<agent-ide>".to_owned());
            Some(format!("{head}\n{}\n</agent-ide>", lines.join("\n")))
        }
    }
}

/// Mutable store state behind the single lock.
#[derive(Default)]
struct State {
    /// Active sessions.
    sessions: BTreeMap<BindingRef, Session>,
    /// In-flight holding work keyed by an opaque key (`detail_ref`, `tests #N`, `edit <op>`).
    in_flight: BTreeMap<String, InFlight>,
    /// Last reservation token issued.
    next_token: u64,
}

/// The daemon's session outbox store; see the module documentation.
#[derive(Default)]
pub struct Outbox {
    /// All mutable state.
    state: Mutex<State>,
    /// Signalled on every push, commit, forget and in-flight change, for holds and waits.
    changed: Notify,
}

impl Outbox {
    /// Opens (or keeps) the outbox of a newly activated session.
    pub fn open(&self, binding: &BindingRef) {
        if let Ok(mut state) = self.state.lock() {
            state
                .sessions
                .entry(binding.clone())
                .or_insert_with(Session::new);
        }
    }

    /// Returns whether `binding` has an open outbox.
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

    /// Records one settled `ide.edit` operation of this session over `paths`.
    pub fn note_edit(&self, binding: &BindingRef, paths: &[String]) {
        self.with_session(binding, |session| {
            session.edit_seq = session.edit_seq.saturating_add(1);
            for path in paths {
                session.edits.push_back((session.edit_seq, path.clone()));
            }
            while session.edits.len() > MAX_EDIT_RECORDS {
                session.edits.pop_front();
            }
        });
    }

    /// Returns the session's current counters, the epoch a job starting now captures.
    pub fn current_epoch(&self, binding: &BindingRef) -> JobEpoch {
        self.with_session(binding, |session| JobEpoch {
            native_epoch: session.native_epoch,
            edit_seq: session.edit_seq,
        })
        .unwrap_or_default()
    }

    /// Records that the session's channel delivered a native hook: compaction is observable.
    pub fn note_hook(&self, binding: &BindingRef) {
        self.with_session(binding, |session| {
            session.context_epoch.get_or_insert(0);
        });
    }

    /// Raises the compaction counter of every session on the same channel as `binding`
    /// (OUTBOX §9.1), the main actor and its subagents alike.
    pub fn note_compaction(&self, binding: &BindingRef) {
        if let Ok(mut state) = self.state.lock() {
            for (other, session) in state.sessions.iter_mut() {
                if other.same_channel(binding) {
                    let epoch = session.context_epoch.get_or_insert(0);
                    *epoch = epoch.saturating_add(1);
                }
            }
        }
    }

    /// Records the agent's turn state; entering `Working` starts a new idle period for wakes.
    pub fn set_agent_state(&self, binding: &BindingRef, agent: AgentState) {
        self.with_session(binding, |session| {
            if agent == AgentState::Working {
                session.woken = false;
                session.holds_in_row = 0;
            }
            session.agent = agent;
        });
        self.changed.notify_waiters();
    }

    /// Whether one more turn end of the session may be blocked: fewer than [`MAX_HOLDS_IN_ROW`]
    /// were blocked since the agent last worked.
    pub fn may_hold(&self, binding: &BindingRef) -> bool {
        self.with_session(binding, |session| session.holds_in_row < MAX_HOLDS_IN_ROW)
            .unwrap_or(false)
    }

    /// Records one blocked turn end: the agent works on, and the blocks in a row grow.
    pub fn note_hold(&self, binding: &BindingRef) {
        self.with_session(binding, |session| {
            session.holds_in_row = session.holds_in_row.saturating_add(1);
            session.agent = AgentState::Working;
        });
        self.changed.notify_waiters();
    }

    /// Returns the session's agent state, or `None` when it is not open.
    pub fn agent_state(&self, binding: &BindingRef) -> Option<AgentState> {
        self.with_session(binding, |session| session.agent)
    }

    /// Records the host session identifier the hooks named for this session.
    pub fn note_host_session(&self, binding: &BindingRef, host_session: &str) {
        self.with_session(binding, |session| {
            session.host_session = Some(host_session.to_owned());
        });
    }

    /// Claims the one wake attempt of the current idle period; returns the host session to wake.
    ///
    /// Returns `None` unless the session is idle, unwoken in this period and has an undelivered
    /// holding fact.
    pub fn claim_wake(&self, binding: &BindingRef) -> Option<String> {
        self.with_session(binding, |session| {
            let holding = session
                .queue
                .iter()
                .any(|stored| stored.reserved.is_none() && stored.draft.holds_turn);
            if session.agent != AgentState::Idle || session.woken || !holding {
                return None;
            }
            session.woken = true;
            session.host_session.clone()
        })
        .flatten()
    }

    /// Gives back a wake claimed by [`Self::claim_wake`] that sent nothing, so the idle period
    /// keeps its one attempt.
    pub fn unclaim_wake(&self, binding: &BindingRef) {
        self.with_session(binding, |session| session.woken = false);
    }

    /// Registers in-flight work under `key` that may produce a holding fact for `owner` by `until`,
    /// capturing the owner's current counters as the fact's fence epoch.
    ///
    /// Returns whether it was registered: the owner's outbox must be open and `key` unused.
    pub fn begin_holding(
        &self,
        key: &str,
        owner: &BindingRef,
        subject: &str,
        until: Instant,
    ) -> bool {
        let registered = self.state.lock().is_ok_and(|mut state| {
            let Some(session) = state.sessions.get(owner) else {
                return false;
            };
            let epoch = JobEpoch {
                native_epoch: session.native_epoch,
                edit_seq: session.edit_seq,
            };
            if state.in_flight.contains_key(key) {
                return false;
            }
            state.in_flight.insert(
                key.to_owned(),
                InFlight {
                    owner: owner.clone(),
                    subscribers: Vec::new(),
                    subject: subject.to_owned(),
                    until,
                    epoch,
                },
            );
            true
        });
        self.changed.notify_waiters();
        registered
    }

    /// Adds `binding` as a subscriber of the in-flight work under `key`; returns whether it was
    /// added (the work exists, the binding's outbox is open and it is not already a recipient).
    pub fn subscribe(&self, key: &str, binding: &BindingRef) -> bool {
        let added = self.state.lock().is_ok_and(|mut state| {
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
        });
        self.changed.notify_waiters();
        added
    }

    /// Removes the in-flight work under `key` and returns who receives its fact, exactly once:
    /// a second call (or a call for unknown work) returns `None`.
    pub fn take_holding(&self, key: &str) -> Option<TakenWork> {
        let taken = self.state.lock().ok()?.in_flight.remove(key);
        self.changed.notify_waiters();
        taken.map(|work| TakenWork {
            owner: work.owner,
            subscribers: work.subscribers,
            epoch: work.epoch,
        })
    }

    /// Returns the latest budget end of the in-flight holding work this session waits for.
    pub fn holding_until(&self, binding: &BindingRef) -> Option<Instant> {
        let state = self.state.lock().ok()?;
        state
            .in_flight
            .values()
            .filter(|work| work.reaches(binding))
            .map(|work| work.until)
            .max()
    }

    /// Returns the subjects of the in-flight work this session waits for, in key order.
    pub fn in_flight_subjects(&self, binding: &BindingRef) -> Vec<String> {
        self.state
            .lock()
            .map(|state| {
                state
                    .in_flight
                    .values()
                    .filter(|work| work.reaches(binding))
                    .map(|work| work.subject.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns the open session whose fingerprint is `fingerprint`, if any.
    pub fn session_by_fingerprint(&self, fingerprint: &[u8; 32]) -> Option<BindingRef> {
        let state = self.state.lock().ok()?;
        state
            .sessions
            .keys()
            .find(|binding| &binding.fingerprint() == fingerprint)
            .cloned()
    }

    /// Returns whether the session has an undelivered, unreserved fact (a holding one when asked).
    pub fn has_undelivered(&self, binding: &BindingRef, holding_only: bool) -> bool {
        self.with_session(binding, |session| {
            session.queue.iter().any(|stored| {
                stored.reserved.is_none() && (!holding_only || stored.draft.holds_turn)
            })
        })
        .unwrap_or(false)
    }

    /// Waits until the session has an undelivered fact (a holding one when asked), the session
    /// closes, or `deadline` passes; returns whether a fact is ready.
    ///
    /// With `holding_only`, it also returns early (with `false`) once no holding work is in flight
    /// and none is undelivered, so a Stop hold never waits for nothing.
    pub async fn wait_ready(
        &self,
        binding: &BindingRef,
        holding_only: bool,
        deadline: Instant,
    ) -> bool {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.has_undelivered(binding, holding_only) {
                return true;
            }
            if !self.is_open(binding)
                || (holding_only && self.holding_until(binding).is_none())
                || Instant::now() >= deadline
            {
                return false;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.has_undelivered(binding, holding_only);
            }
        }
    }

    /// Marks the undelivered facts of `binding` whose detail reference is `reference` delivered:
    /// the job's result reached the agent through its own reference (OUTBOX §6).
    pub fn mark_reference_read(&self, binding: &BindingRef, reference: &str) {
        self.mark_where(binding, |draft| {
            draft.detail_ref.as_deref() == Some(reference)
        });
    }

    /// Marks the undelivered facts of `binding` with exactly `subject` delivered (a settled test
    /// run read by `ide.test {status}` or `ide.inspect "tests #N"`).
    pub fn mark_subject_read(&self, binding: &BindingRef, subject: &str) {
        self.mark_where(binding, |draft| draft.subject == subject);
    }

    /// Moves every unreserved undelivered fact matching `matches` into the history.
    fn mark_where(&self, binding: &BindingRef, matches: impl Fn(&FactDraft) -> bool) {
        self.with_session(binding, |session| {
            let (read, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut session.queue)
                .into_iter()
                .partition(|stored| stored.reserved.is_none() && matches(&stored.draft));
            session.queue = kept;
            for stored in &read {
                session.remember(stored);
            }
        });
        self.changed.notify_waiters();
    }

    /// Returns the summary lines of the session's last delivered facts, oldest first.
    pub fn history(&self, binding: &BindingRef) -> Vec<String> {
        self.with_session(binding, |session| session.history.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Renders every undelivered fact as summary and reference lines within
    /// [`STOP_FLUSH_BYTES`] for the `ide.stop` reply, then drops the session (OUTBOX §11).
    pub fn flush_for_stop(&self, binding: &BindingRef) -> Option<String> {
        let reservation = self.reserve_facts(
            binding,
            HandoffRequest {
                carrier: Carrier::StopReply,
                budget: STOP_FLUSH_BYTES,
                holding_only: false,
            },
        );
        self.drop_session(binding);
        let reservation = reservation?;
        let lines = reservation
            .facts
            .iter()
            .map(|fact| {
                render_fact(&PendingFact {
                    detail: None,
                    ..fact.clone()
                })
            })
            .collect::<Vec<_>>();
        let mut text = format!("undelivered facts:\n{}", lines.join("\n"));
        if reservation.remaining > 0 {
            text.push_str(&format!("\n(+{} more facts)", reservation.remaining));
        }
        Some(text)
    }

    /// Removes the session and its in-flight work.
    fn drop_session(&self, binding: &BindingRef) {
        if let Ok(mut state) = self.state.lock() {
            state.sessions.remove(binding);
            state.in_flight.retain(|_, work| &work.owner != binding);
            for work in state.in_flight.values_mut() {
                work.subscribers.retain(|subscriber| subscriber != binding);
            }
        }
        self.changed.notify_waiters();
    }

    /// Queues `draft` for every open recipient (the store half of [`SessionOutbox::push`](crate::assistance::outbox::SessionOutbox::push)).
    pub fn push_draft(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool {
        if draft.validate().is_err() {
            return false;
        }
        let mut queued = false;
        if let Ok(mut state) = self.state.lock() {
            for recipient in recipients {
                let Some(session) = state.sessions.get_mut(recipient) else {
                    continue;
                };
                session.queue.retain(|stored| {
                    stored.reserved.is_some()
                        || stored.draft.kind != draft.kind
                        || stored.draft.subject != draft.subject
                });
                if session
                    .queue
                    .iter()
                    .filter(|stored| stored.reserved.is_none())
                    .count()
                    >= MAX_UNDELIVERED
                {
                    let victim = session
                        .queue
                        .iter()
                        .position(|stored| stored.reserved.is_none() && !stored.draft.holds_turn)
                        .or_else(|| {
                            session
                                .queue
                                .iter()
                                .position(|stored| stored.reserved.is_none())
                        });
                    if let Some(victim) = victim {
                        session.queue.remove(victim);
                        session.dropped += 1;
                    }
                }
                let seq = session.next_seq;
                session.next_seq = session.next_seq.saturating_add(1);
                session.queue.push(Stored {
                    seq,
                    draft: draft.clone(),
                    epoch,
                    reserved: None,
                });
                queued = true;
            }
        }
        if queued {
            self.changed.notify_waiters();
        }
        queued
    }

    /// Selects and reserves facts for one carrier (the store half of [`SessionOutbox::reserve`](crate::assistance::outbox::SessionOutbox::reserve)).
    pub fn reserve_facts(
        &self,
        binding: &BindingRef,
        request: HandoffRequest,
    ) -> Option<Reservation> {
        let mut state = self.state.lock().ok()?;
        state.next_token = state.next_token.saturating_add(1);
        let token = state.next_token;
        let session = state.sessions.get_mut(binding)?;
        let mut used = 0usize;
        let mut chosen = Vec::new();
        let mut remaining = session.dropped;
        let mut full = false;
        for stored in session.ordered() {
            if request.holding_only && !stored.draft.holds_turn {
                remaining += 1;
                continue;
            }
            if full {
                remaining += 1;
                continue;
            }
            let fact = pending(session, stored, request.carrier);
            let mut size = render_fact(&fact).len() + 1;
            let fact = if used + size > request.budget && chosen.is_empty() {
                // A single fact larger than the budget goes as its summary and reference.
                let short = PendingFact {
                    detail: None,
                    ..fact
                };
                size = render_fact(&short).len() + 1;
                short
            } else {
                fact
            };
            if used + size > request.budget {
                full = true;
                remaining += 1;
                continue;
            }
            used += size;
            chosen.push(fact);
        }
        if chosen.is_empty() {
            return None;
        }
        let seqs = chosen.iter().map(|fact| fact.seq).collect::<Vec<_>>();
        for stored in session
            .queue
            .iter_mut()
            .filter(|stored| seqs.contains(&stored.seq))
        {
            stored.reserved = Some(token);
        }
        Some(Reservation {
            token,
            facts: chosen,
            remaining,
        })
    }

    /// Marks a reservation delivered (the store half of [`SessionOutbox::commit`](crate::assistance::outbox::SessionOutbox::commit)); returns the
    /// number of facts marked.
    pub fn commit_reservation(&self, binding: &BindingRef, token: u64) -> usize {
        self.commit_taken(binding, token).len()
    }

    /// Marks a reservation delivered and returns the kind and subject of every fact it marked, so
    /// the caller can settle per-kind bookkeeping (a delivered `test-finished` collects its run).
    pub fn commit_taken(&self, binding: &BindingRef, token: u64) -> Vec<(FactKind, String)> {
        let marked = self
            .with_session(binding, |session| {
                let (taken, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut session.queue)
                    .into_iter()
                    .partition(|stored| stored.reserved == Some(token));
                session.queue = kept;
                if !taken.is_empty() {
                    session.dropped = 0;
                }
                for stored in &taken {
                    session.remember(stored);
                }
                taken
                    .iter()
                    .map(|stored| (stored.draft.kind, stored.draft.subject.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.changed.notify_waiters();
        marked
    }

    /// Returns a reservation's facts unchanged (the store half of [`SessionOutbox::release`](crate::assistance::outbox::SessionOutbox::release)).
    pub fn release_reservation(&self, binding: &BindingRef, token: u64) {
        self.with_session(binding, |session| {
            for stored in session
                .queue
                .iter_mut()
                .filter(|stored| stored.reserved == Some(token))
            {
                stored.reserved = None;
            }
        });
        self.changed.notify_waiters();
    }

    /// Drops the session silently (the store half of [`SessionOutbox::forget`](crate::assistance::outbox::SessionOutbox::forget)).
    pub fn forget_session(&self, binding: &BindingRef) {
        self.drop_session(binding);
    }

    /// Returns the session's compaction counter (the store half of
    /// [`SessionOutbox::context_epoch`](crate::assistance::outbox::SessionOutbox::context_epoch)).
    pub fn session_context_epoch(&self, binding: &BindingRef) -> Option<u64> {
        self.with_session(binding, |session| session.context_epoch)
            .flatten()
    }
}

/// Builds the carrier view of one stored fact: staleness label, and no source-derived detail on
/// carriers that may not carry source.
fn pending(session: &Session, stored: &Stored, carrier: Carrier) -> PendingFact {
    let draft = &stored.draft;
    PendingFact {
        seq: stored.seq,
        kind: draft.kind,
        subject: draft.subject.clone(),
        summary: draft.summary.clone(),
        detail: draft
            .detail
            .clone()
            .filter(|_| !draft.source_bearing || carrier.carries_source()),
        detail_ref: draft.detail_ref.clone(),
        failed: draft.failed,
        holds_turn: draft.holds_turn,
        source_bearing: draft.source_bearing,
        stale: session.stale_label(stored, carrier),
    }
}

#[cfg(test)]
mod tests {
    use super::super::outbox::FencedPath;
    use super::*;

    /// A session binding fixture.
    fn binding(actor: &str) -> BindingRef {
        BindingRef::fixture(actor, "channel-a", 1)
    }

    /// A valid draft of `kind` with `subject`, holding and failing as asked.
    fn draft(kind: FactKind, subject: &str, holds: bool, failed: bool) -> FactDraft {
        FactDraft {
            kind,
            subject: subject.to_owned(),
            summary: format!("{subject} done"),
            detail: Some("  line one".to_owned()),
            detail_ref: Some(format!("ref-{subject}")),
            failed,
            holds_turn: holds,
            source_bearing: false,
            fence_paths: Vec::new(),
        }
    }

    /// A hook-sized request.
    fn hook(budget: usize) -> HandoffRequest {
        HandoffRequest {
            carrier: Carrier::Hook,
            budget,
            holding_only: false,
        }
    }

    /// A fact is handed off once: the first committed reservation wins and later carriers see
    /// nothing; a released reservation returns the fact.
    #[test]
    fn a_fact_is_handed_off_once_across_carriers() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        assert!(outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::TestFinished, "tests #1", true, false),
            JobEpoch::default()
        ));
        let first = outbox
            .reserve_facts(&session, hook(4096))
            .expect("reserved");
        assert!(
            outbox.reserve_facts(&session, hook(4096)).is_none(),
            "a reserved fact is invisible to a second carrier"
        );
        outbox.release_reservation(&session, first.token);
        let again = outbox
            .reserve_facts(&session, hook(4096))
            .expect("released");
        assert_eq!(outbox.commit_reservation(&session, again.token), 1);
        assert!(outbox.reserve_facts(&session, hook(4096)).is_none());
        assert_eq!(
            outbox.history(&session),
            vec!["test-finished tests #1: tests #1 done (detail_ref ref-tests #1)".to_owned()]
        );
    }

    /// Failures come first, then ascending sequence; coalescing replaces an undelivered fact
    /// with the same kind and subject; closed sessions receive nothing.
    #[test]
    fn ordering_coalescing_and_recipients() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        let recipients = [session.clone(), binding("closed")];
        assert!(outbox.push_draft(
            &recipients,
            draft(FactKind::JobReady, "ide.read a", true, false),
            JobEpoch::default()
        ));
        assert!(outbox.push_draft(
            &recipients,
            draft(FactKind::TestFinished, "tests #2", true, true),
            JobEpoch::default()
        ));
        let mut newer = draft(FactKind::JobReady, "ide.read a", true, false);
        newer.summary = "newer".to_owned();
        assert!(outbox.push_draft(&recipients, newer, JobEpoch::default()));
        assert!(!outbox.push_draft(
            &[binding("closed")],
            draft(FactKind::JobReady, "x", true, false),
            JobEpoch::default()
        ));
        let reservation = outbox.reserve_facts(&session, hook(4096)).unwrap();
        let subjects = reservation
            .facts
            .iter()
            .map(|fact| (fact.subject.as_str(), fact.summary.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            subjects,
            vec![("tests #2", "tests #2 done"), ("ide.read a", "newer")]
        );
    }

    /// The budget selects a strict prefix; an oversized single fact goes as summary and reference.
    #[test]
    fn budget_selects_a_prefix_and_shortens_an_oversized_first_fact() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        let mut big = draft(FactKind::JobReady, "big", true, false);
        big.detail = Some("x".repeat(1500));
        outbox.push_draft(std::slice::from_ref(&session), big, JobEpoch::default());
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::JobReady, "small", true, false),
            JobEpoch::default(),
        );
        let reservation = outbox.reserve_facts(&session, hook(100)).unwrap();
        assert_eq!(reservation.facts.len(), 1);
        assert_eq!(reservation.facts[0].detail, None);
        assert_eq!(reservation.remaining, 1);
    }

    /// Source-bearing detail never reaches hook-shaped carriers; reply carriers get it.
    #[test]
    fn source_text_stays_off_hook_carriers() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        let mut answer = draft(FactKind::JobReady, "ide.symbol a#F", false, false);
        answer.source_bearing = true;
        answer.detail = Some("fn f() {}".to_owned());
        outbox.push_draft(std::slice::from_ref(&session), answer, JobEpoch::default());
        let on_hook = outbox.reserve_facts(&session, hook(4096)).unwrap();
        assert_eq!(on_hook.facts[0].detail, None);
        assert!(!render_fact(&on_hook.facts[0]).contains("fn f"));
        outbox.release_reservation(&session, on_hook.token);
        let on_reply = outbox
            .reserve_facts(
                &session,
                HandoffRequest {
                    carrier: Carrier::Reply,
                    budget: 4096,
                    holding_only: false,
                },
            )
            .unwrap();
        assert_eq!(on_reply.facts[0].detail.as_deref(), Some("fn f() {}"));
    }

    /// Later edits label non-source facts with the fenced paths; native writes label generically;
    /// unrelated paths do not label a fenced fact.
    #[test]
    fn staleness_labels_follow_the_fence() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        let epoch = outbox.current_epoch(&session);
        let mut fenced = draft(FactKind::EditChecked, "edit op-1", true, false);
        fenced.fence_paths = vec![FencedPath {
            path: "src/a.rs".to_owned(),
            digest: None,
        }];
        outbox.push_draft(std::slice::from_ref(&session), fenced, epoch);
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::TestFinished, "tests #1", true, false),
            epoch,
        );
        outbox.note_edit(&session, &["src/b.rs".to_owned()]);
        let reservation = outbox.reserve_facts(&session, hook(4096)).unwrap();
        let labels = reservation
            .facts
            .iter()
            .map(|fact| (fact.subject.as_str(), fact.stale.clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec![
                ("edit op-1", None),
                (
                    "tests #1",
                    Some("(ran before your later edits to src/b.rs)".to_owned())
                )
            ]
        );
        outbox.release_reservation(&session, reservation.token);
        outbox.note_native(&session);
        let reservation = outbox.reserve_facts(&session, hook(4096)).unwrap();
        assert_eq!(
            reservation.facts[0].stale.as_deref(),
            Some("(ran before later changes)")
        );
    }

    /// Reading by reference or subject delivers matching facts; forget drops everything.
    #[test]
    fn reads_mark_delivered_and_forget_drops() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::JobReady, "ide.read a", true, false),
            JobEpoch::default(),
        );
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::TestFinished, "tests #4", true, false),
            JobEpoch::default(),
        );
        outbox.mark_reference_read(&session, "ref-ide.read a");
        outbox.mark_subject_read(&session, "tests #4");
        assert!(!outbox.has_undelivered(&session, false));
        assert_eq!(outbox.history(&session).len(), 2);
        outbox.forget_session(&session);
        assert!(!outbox.is_open(&session));
    }

    /// The bound keeps 32 undelivered facts, dropping a non-holding one first, and reports drops.
    #[test]
    fn the_bound_drops_non_holding_facts_first() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::JobReady, "quiet", false, false),
            JobEpoch::default(),
        );
        for index in 0..MAX_UNDELIVERED {
            outbox.push_draft(
                std::slice::from_ref(&session),
                draft(FactKind::JobReady, &format!("job {index}"), true, false),
                JobEpoch::default(),
            );
        }
        let reservation = outbox.reserve_facts(&session, hook(1_000_000)).unwrap();
        assert_eq!(reservation.facts.len(), MAX_UNDELIVERED);
        assert!(reservation.facts.iter().all(|fact| fact.holds_turn));
        assert_eq!(reservation.remaining, 1, "the drop is reported");
    }

    /// Compaction raises every session on the channel; the counter is None before any hook.
    #[test]
    fn compaction_reaches_every_session_of_the_channel() {
        let outbox = Outbox::default();
        let main = BindingRef::fixture("main", "channel-a", 1);
        let sub = BindingRef::fixture("sub", "channel-a", 1);
        let other = BindingRef::fixture("main", "channel-b", 1);
        for session in [&main, &sub, &other] {
            outbox.open(session);
        }
        assert_eq!(outbox.session_context_epoch(&main), None);
        outbox.note_hook(&main);
        outbox.note_compaction(&main);
        assert_eq!(outbox.session_context_epoch(&main), Some(1));
        assert_eq!(outbox.session_context_epoch(&sub), Some(1));
        assert_eq!(outbox.session_context_epoch(&other), None);
    }

    /// A wake is claimed once per idle period, only for an idle session with a holding fact.
    #[test]
    fn wake_is_claimed_once_per_idle_period() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        outbox.note_host_session(&session, "host-1");
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::TestFinished, "tests #1", true, false),
            JobEpoch::default(),
        );
        assert_eq!(
            outbox.claim_wake(&session),
            None,
            "working agents are not woken"
        );
        outbox.set_agent_state(&session, AgentState::Idle);
        assert_eq!(outbox.claim_wake(&session), Some("host-1".to_owned()));
        assert_eq!(
            outbox.claim_wake(&session),
            None,
            "one attempt per idle period"
        );
        outbox.set_agent_state(&session, AgentState::Working);
        outbox.set_agent_state(&session, AgentState::Idle);
        assert_eq!(outbox.claim_wake(&session), Some("host-1".to_owned()));
    }

    /// The stop flush lists undelivered facts without detail and closes the session.
    #[test]
    fn stop_flush_lists_summaries_and_closes() {
        let outbox = Outbox::default();
        let session = binding("a");
        outbox.open(&session);
        outbox.push_draft(
            std::slice::from_ref(&session),
            draft(FactKind::TestFinished, "tests #1", true, false),
            JobEpoch::default(),
        );
        let flush = outbox.flush_for_stop(&session).unwrap();
        assert!(flush.contains("test-finished tests #1: tests #1 done"));
        assert!(!flush.contains("line one"));
        assert!(!outbox.is_open(&session));
    }

    /// A hold-style wait returns at once without in-flight work, and wakes on a push.
    #[tokio::test]
    async fn waits_return_on_a_fact_or_without_work() {
        let outbox = std::sync::Arc::new(Outbox::default());
        let session = binding("a");
        outbox.open(&session);
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        assert!(!outbox.wait_ready(&session, true, deadline).await);
        outbox.begin_holding("tests #1", &session, "tests #1", deadline);
        let pusher = {
            let outbox = outbox.clone();
            let session = session.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                outbox.push_draft(
                    &[session],
                    draft(FactKind::TestFinished, "tests #1", true, false),
                    JobEpoch::default(),
                );
            })
        };
        assert!(outbox.wait_ready(&session, true, deadline).await);
        pusher.await.unwrap();
    }

    /// Blocks compose after the ambient plate inside one envelope.
    #[test]
    fn blocks_compose_after_the_plate() {
        assert_eq!(compose_block(None, &[], 0), None);
        assert_eq!(
            compose_block(Some("<agent-ide>\nrust: ok\n</agent-ide>"), &["f".to_owned()], 2),
            Some(
                "<agent-ide>\nrust: ok\nf\n(+2 more facts; ide.inspect {} lists them)\n</agent-ide>"
                    .to_owned()
            )
        );
        assert_eq!(
            compose_block(None, &["f".to_owned()], 0),
            Some("<agent-ide>\nf\n</agent-ide>".to_owned())
        );
    }
}
