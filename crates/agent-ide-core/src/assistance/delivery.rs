//! Worker-side wiring of the session outbox (OUTBOX-r3 §2-§3, §6, §12).
//!
//! The worker's `Shared` state is the daemon's
//! [`SessionOutbox`](crate::assistance::outbox::SessionOutbox): it owns the in-memory
//! [`Outbox`](crate::assistance::session_outbox::Outbox) store and adds what only the worker knows,
//! the job ledger behind `defer`, the test-run registry behind `test-finished` facts, and the
//! translation of settled replies into `job-ready` fact drafts. Everything here is synchronous
//! bookkeeping under short locks, except the idle wake, which runs detached after its grace;
//! nothing awaits while a lock is held.

use std::{sync::Arc, time::Duration};

use serde_json::Value;

use super::super::{
    content::{self, Envelope},
    facade::AssistanceTool,
    host_binding::BindingRef,
    outbox::Carrier,
    outbox::FencedPath,
    outbox::{
        DeferSpec, FactDraft, FactKind, HandoffRequest, JobEpoch, MAX_FACT_BYTES,
        MAX_SUBJECT_BYTES, MAX_SUMMARY_BYTES, MAX_TARGET_BYTES, Reservation, SessionOutbox,
    },
    reply::{EditDiagnostics, PeerReply, ResultKind},
    session_outbox::{AgentState, Outbox, compose_block, render_fact},
    test_fact::terminal_fact,
    tests::{RunResult, TestRuns},
    wake::{MAX_WAKE_BYTES, SessionWaker, WakeOutcome, WakeTarget},
};
use super::{INLINE_REPLY_WAIT, Job, JobStage, Shared, Worker, edit_operation};
use crate::changes::edit::EditResult;

/// Slack added to a run's budget before its holding registration expires: settlement, output
/// draining and the settle hook itself.
const RUN_SETTLE_SLACK: Duration = Duration::from_secs(5);
/// How long before the caller's inline window ends an edit still awaiting its project check is
/// answered with `diagnostics: pending` (OUTBOX §8.6), leaving room for the reply to travel. The
/// multi-file edit (`batch_edit`) answers early by the same margin.
pub(super) const EARLY_EDIT_REPLY_MARGIN: Duration = Duration::from_millis(1500);
/// Most diagnostic `new:` lines one `edit-checked` fact carries (OUTBOX §3.3).
const MAX_NEW_LINES: usize = 8;
/// How long a holding fact waits before an idle session is woken, so an imminent hook takes it
/// instead (OUTBOX §8.4).
const WAKE_GRACE: Duration = Duration::from_secs(2);
/// The sentence that ends every wake message (OUTBOX §8.4).
const WAKE_TAIL: &str = "Results you requested from Agent IDE arrived while you were idle.";
/// Room in a wake message for the envelope tags and the overflow line around the facts.
const WAKE_FRAME: usize = 96;

/// Returns the fact subject of test run `id`: `tests #N`, the handle every `ide.test` line names.
pub(super) fn test_subject(id: u64) -> String {
    format!("tests #{id}")
}

/// Cuts `text` to at most `limit` bytes on a character boundary, dropping control characters
/// other than newlines when `keep_newlines`, and all of them otherwise.
fn bounded(text: &str, limit: usize, keep_newlines: bool) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if character.is_control() && !(keep_newlines && character == '\n') {
            continue;
        }
        if out.len() + character.len_utf8() > limit {
            break;
        }
        out.push(character);
    }
    out
}

/// Returns the call's main target for a `job-ready` subject: the symbol, path, root, operation
/// id or detail reference the parameters name, in that order, bounded to [`MAX_TARGET_BYTES`];
/// `call` when the parameters name none.
pub(super) fn call_target(tool: AssistanceTool, parameters: &Value) -> String {
    let named = ["symbol", "path", "root", "operation_id", "detail_ref"]
        .iter()
        .find_map(|key| parameters.get(*key).and_then(Value::as_str))
        .map(|value| bounded(value, MAX_TARGET_BYTES, false))
        .filter(|value| !value.trim().is_empty());
    named.unwrap_or_else(|| {
        if tool == AssistanceTool::Test {
            "run".to_owned()
        } else {
            "call".to_owned()
        }
    })
}

/// What one call's reply leaves owed to its caller (OUTBOX §2: the request got no answer for
/// this subject): the fact kind and subject that will answer it, and the reference that reads it
/// back. `None` when the reply answered the call.
pub(crate) fn owed_fact(
    tool: AssistanceTool,
    parameters: &Value,
    reply: &PeerReply,
) -> Option<(FactKind, String, Option<String>)> {
    match reply {
        PeerReply::Pending { detail_ref }
            if !matches!(
                tool,
                AssistanceTool::Start | AssistanceTool::Stop | AssistanceTool::Inspect
            ) =>
        {
            let subject = format!("{} {}", tool.mcp_name(), call_target(tool, parameters));
            Some((
                FactKind::JobReady,
                bounded(&subject, MAX_SUBJECT_BYTES, false),
                Some(detail_ref.clone()),
            ))
        }
        PeerReply::Edit {
            result,
            diagnostics: EditDiagnostics::Pending { detail_ref },
            ..
        } if tool == AssistanceTool::Edit => Some((
            FactKind::EditChecked,
            bounded(
                &format!("edit {}", result.operation_id),
                MAX_SUBJECT_BYTES,
                false,
            ),
            Some(detail_ref.clone()),
        )),
        PeerReply::Complete {
            kind: ResultKind::Test,
            text,
            ..
        } if tool == AssistanceTool::Test => match run_line(text) {
            Some((id, "started")) => Some((FactKind::TestFinished, test_subject(id), None)),
            _ => None,
        },
        _ => None,
    }
}

/// The run a test reply reports settled: its text starts `tests #N:` and the run is neither
/// started nor running.
pub(crate) fn settled_run(reply: &PeerReply) -> Option<u64> {
    let PeerReply::Complete {
        kind: ResultKind::Test,
        text,
        ..
    } = reply
    else {
        return None;
    };
    run_line(text)
        .filter(|(_, state)| !matches!(*state, "started" | "running"))
        .map(|(id, _)| id)
}

/// Splits a test line `tests #N: <word> ...` into the run id and its first word.
fn run_line(text: &str) -> Option<(u64, &str)> {
    let (handle, rest) = text.strip_prefix("tests #")?.split_once(": ")?;
    let id = handle.parse().ok()?;
    let word = rest.split([' ', ';', ',']).next().unwrap_or_default();
    Some((id, word))
}

/// The one `lost-after-restart` fact for `subjects`, the requested work an actor's front relayed
/// unresolved before the daemon was replaced (OUTBOX §10). Run numbers restart with the daemon,
/// so run subjects say "before restart". Subjects that do not fit the summary are counted.
// ponytail: one summary line of at most 256 bytes; move the list into detail lines if long
// subject lists prove common.
fn lost_fact(subjects: &[String]) -> Option<FactDraft> {
    const HEAD: &str = "the IDE restarted; results not delivered before it may be lost: ";
    let named = subjects
        .iter()
        .map(|subject| {
            if subject.starts_with("tests #") {
                format!("{subject} (before restart)")
            } else {
                subject.clone()
            }
        })
        .collect::<Vec<_>>();
    let mut summary = HEAD.to_owned();
    for (index, subject) in named.iter().enumerate() {
        let rest = named.len() - index - 1;
        let more = if rest > 0 {
            format!(" +{rest} more")
        } else {
            String::new()
        };
        let separator = if index == 0 { "" } else { ", " };
        if summary.len() + separator.len() + subject.len() + more.len() > MAX_SUMMARY_BYTES {
            summary.push_str(&format!(" +{} more", named.len() - index));
            break;
        }
        summary.push_str(separator);
        summary.push_str(subject);
    }
    (!named.is_empty()).then(|| FactDraft {
        kind: FactKind::LostAfterRestart,
        subject: "restart".to_owned(),
        summary,
        detail: None,
        detail_ref: None,
        failed: false,
        holds_turn: false,
        source_bearing: false,
        fence_paths: Vec::new(),
    })
}

/// Whether a tool's answer is derived from source text and so must never ride a hook carrier.
fn source_bearing(tool: AssistanceTool) -> bool {
    matches!(
        tool,
        AssistanceTool::Context
            | AssistanceTool::Diff
            | AssistanceTool::Outline
            | AssistanceTool::Read
            | AssistanceTool::Symbol
            | AssistanceTool::Graph
    )
}

/// Renders a reply to its compact model text, or `None` when it cannot be projected.
fn compact_text(reply: &PeerReply) -> Option<String> {
    let rendered = content::render_with_status(reply.clone(), None, Envelope::TextOnly)?;
    rendered
        .content
        .first()
        .and_then(|block| block.as_text())
        .map(|text| text.text.clone())
}

/// Builds the `job-ready` draft for a deferred job's final reply (OUTBOX §2, §3.2).
///
/// A failure (an error, unavailable or invalid-parameters reply) carries its compact first line
/// and `failed = true`. A successful source-bearing answer carries only `ready` and its
/// reference, so the body is read (and re-proved) through `ide.inspect`; any other successful
/// answer carries its compact text, which holds no source.
fn job_fact(reference: &str, reply: &PeerReply, spec: &DeferSpec) -> FactDraft {
    let failed = matches!(
        reply,
        PeerReply::Error { .. }
            | PeerReply::Unavailable { .. }
            | PeerReply::InvalidParameters { .. }
    );
    let bearing = source_bearing(spec.tool);
    let (summary, detail) = if bearing && !failed {
        ("ready".to_owned(), None)
    } else {
        let text = compact_text(reply).unwrap_or_else(|| "settled".to_owned());
        let mut lines = text.lines();
        let first = lines.next().unwrap_or("settled");
        let summary = bounded(first, MAX_SUMMARY_BYTES, false);
        let rest = lines.collect::<Vec<_>>().join("\n");
        let detail = bounded(&rest, MAX_FACT_BYTES - summary.len(), true);
        (summary, (!detail.trim().is_empty()).then_some(detail))
    };
    FactDraft {
        kind: FactKind::JobReady,
        subject: bounded(&spec.subject(), MAX_SUBJECT_BYTES, false),
        summary: if summary.trim().is_empty() {
            "settled".to_owned()
        } else {
            summary
        },
        detail,
        detail_ref: Some(reference.to_owned()),
        failed,
        holds_turn: !spec.approximate_given,
        source_bearing: bearing && !failed,
        fence_paths: spec.fence_paths.clone(),
    }
}

/// Builds the `test-finished` draft of settled run `id` for one recipient.
///
/// The text is M-024's [`terminal_fact`] (`M024-M026-TEST-FACT` r3): a summary of at most 256
/// bytes plus a detail that stays inside the `test-finished` budget, rendered verbatim.
fn test_fact(id: u64, result: &RunResult, owns_detail: bool) -> FactDraft {
    let fact = terminal_fact(id, result, owns_detail);
    FactDraft {
        kind: FactKind::TestFinished,
        subject: test_subject(id),
        summary: fact.summary,
        detail: Some(fact.detail),
        detail_ref: owns_detail.then(|| result.detail_ref.clone()),
        failed: result.report.failed > 0 || result.stopped || result.exit != Some(0),
        holds_turn: true,
        source_bearing: false,
        fence_paths: Vec::new(),
    }
}

/// Builds the `edit-checked` draft of one single-file edit's project check (OUTBOX §3.3).
///
/// Reported diagnostics are compiler output about the file, so their `new:` lines ride only
/// reply-shaped carriers (`source_bearing`); hooks carry the counts summary alone. `reference` is
/// the edit job's own result reference: reading it delivers the fact.
fn edit_checked_fact(
    reference: &str,
    result: &EditResult,
    diagnostics: &EditDiagnostics,
) -> FactDraft {
    let path = bounded(&result.path, MAX_SUBJECT_BYTES, false);
    let (summary, detail, failed) = match diagnostics {
        EditDiagnostics::CurrentClean { project_errors } => (
            "1 file, clean".to_owned(),
            (*project_errors > 0).then(|| {
                format!(
                    "  {path}: clean; the project check reports {project_errors} errors in other files"
                )
            }),
            false,
        ),
        EditDiagnostics::CurrentReported {
            messages,
            delta,
            truncated,
        } => {
            let mut lines = messages
                .iter()
                .take(MAX_NEW_LINES)
                .map(|message| format!("  new: {path}: {}", bounded(message, 160, false)))
                .collect::<Vec<_>>();
            if *truncated || messages.len() > MAX_NEW_LINES {
                lines.push("  (more in ide.context with kind problems)".to_owned());
            }
            (
                bounded(&format!("1 file: {delta}"), MAX_SUMMARY_BYTES, false),
                Some(bounded(
                    &lines.join("\n"),
                    MAX_FACT_BYTES - MAX_SUMMARY_BYTES,
                    true,
                )),
                true,
            )
        }
        EditDiagnostics::NotAnalysed { reason } => (
            bounded(
                &format!("1 file, not analysed ({reason})"),
                MAX_SUMMARY_BYTES,
                false,
            ),
            None,
            false,
        ),
        EditDiagnostics::Unknown {} | EditDiagnostics::Pending { .. } => {
            ("1 file, diagnostics unknown".to_owned(), None, false)
        }
    };
    let source_bearing = detail.is_some() && failed;
    FactDraft {
        kind: FactKind::EditChecked,
        subject: bounded(
            &format!("edit {}", result.operation_id),
            MAX_SUBJECT_BYTES,
            false,
        ),
        summary,
        detail,
        detail_ref: Some(reference.to_owned()),
        failed,
        holds_turn: true,
        source_bearing,
        fence_paths: vec![FencedPath {
            path: result.path.clone(),
            digest: None,
        }],
    }
}

impl Worker<'_> {
    /// Answers an edit whose project check is still running with `diagnostics: pending` shortly
    /// before its caller's inline window ends (OUTBOX §8.6).
    ///
    /// Only a caller still waiting is answered, and only when the session's outbox can hold the
    /// check as in-flight work under the job's reference: the job stays parked on its check and
    /// its settlement pushes one `edit-checked` fact ([`Shared::job_settled`]). The write is
    /// counted for staleness before the registration, so the fact never labels its own edit.
    pub(super) fn reply_edit_early(&self, job: &mut Job, stage: &JobStage) {
        // A multi-file edit answers early through its own path (`batch_edit`).
        let JobStage::EditAwaitingCheck {
            result, deadline, ..
        } = stage
        else {
            return;
        };
        let started = job.deadline.checked_sub(Duration::from_millis(
            self.shared.launcher.limits.operation_ms,
        ));
        let due = started.is_none_or(|started| {
            tokio::time::Instant::now() + EARLY_EDIT_REPLY_MARGIN >= started + INLINE_REPLY_WAIT
        });
        let waiting = job
            .stop_reply
            .as_ref()
            .is_some_and(|sender| !sender.is_closed());
        let binding = job.invocation.binding_ref().clone();
        if !due || !waiting || !self.shared.outbox.is_open(&binding) {
            return;
        }
        if result.outcome.has_post_source() {
            self.shared
                .outbox
                .note_edit(&binding, std::slice::from_ref(&result.path));
        }
        let subject = format!("edit {}", result.operation_id);
        if !self
            .shared
            .outbox
            .begin_holding(&job.reference, &binding, &subject, *deadline)
        {
            return;
        }
        let reply = PeerReply::Edit {
            result: result.clone(),
            diagnostics: EditDiagnostics::Pending {
                detail_ref: job.reference.clone(),
            },
            note: job
                .format_note
                .clone()
                .filter(|_| result.outcome.has_post_source()),
            operation: edit_operation(&job.parameters),
        };
        if let Some(sender) = job.stop_reply.take()
            && sender.send(reply).is_err()
        {
            let _ = self.shared.outbox.take_holding(&job.reference);
        }
    }
}

impl SessionOutbox for Shared {
    /// Queues the draft for every open recipient session (the store's validation applies).
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool {
        self.outbox.push_draft(recipients, draft, epoch)
    }

    /// Registers a pending job for its `job-ready` fact, or pushes the fact at once when the job
    /// already settled unread. The ledger lock is held across the pending check and the
    /// registration, so a concurrent settlement either sees the registration or is seen here.
    fn defer(&self, reference: &str, spec: DeferSpec) -> bool {
        if spec.validate().is_err() {
            return false;
        }
        let Ok(mut ledger) = self.ledger.lock() else {
            return false;
        };
        if ledger.deferred.contains_key(reference) {
            return false;
        }
        let Some(detail) = ledger.details.get(reference) else {
            return false;
        };
        let owner = detail.binding.clone();
        let epoch = self.outbox.current_epoch(&owner);
        if matches!(detail.reply, PeerReply::Pending { .. }) {
            ledger
                .deferred
                .insert(reference.to_owned(), (spec.clone(), epoch));
            drop(ledger);
            if !spec.approximate_given {
                let until = tokio::time::Instant::now()
                    + Duration::from_millis(self.launcher.limits.operation_ms);
                self.outbox
                    .begin_holding(reference, &owner, &spec.subject(), until);
            }
            return true;
        }
        let reply = detail.reply.clone();
        drop(ledger);
        self.outbox
            .push_draft(&[owner], job_fact(reference, &reply, &spec), epoch)
    }

    /// Drops the session's facts, holds and deferrals silently.
    fn forget(&self, binding: &BindingRef) {
        if let Ok(mut ledger) = self.ledger.lock() {
            let owned = ledger
                .details
                .iter()
                .filter(|(_, detail)| &detail.binding == binding)
                .map(|(reference, _)| reference.clone())
                .collect::<Vec<_>>();
            for reference in owned {
                ledger.deferred.remove(&reference);
            }
        }
        self.outbox.forget_session(binding);
    }

    /// The session's compaction counter.
    fn context_epoch(&self, binding: &BindingRef) -> Option<u64> {
        self.outbox.session_context_epoch(binding)
    }

    /// Reserves facts for a carrier.
    fn reserve(&self, binding: &BindingRef, request: HandoffRequest) -> Option<Reservation> {
        self.outbox.reserve_facts(binding, request)
    }

    /// Commits a reservation and marks the owner's settled test runs among it collected.
    fn commit(&self, binding: &BindingRef, token: u64) -> usize {
        commit_collected(&self.outbox, &self.test_runs, binding, token)
    }

    /// Releases a reservation unchanged.
    fn release(&self, binding: &BindingRef, token: u64) {
        self.outbox.release_reservation(binding, token);
    }
}

/// Commits reservation `token` of `binding` and marks the owner's settled test runs among it
/// collected; returns the number of facts delivered.
fn commit_collected(
    outbox: &Outbox,
    test_runs: &TestRuns,
    binding: &BindingRef,
    token: u64,
) -> usize {
    let committed = outbox.commit_taken(binding, token);
    for (kind, subject) in &committed {
        if *kind == FactKind::TestFinished
            && let Some(id) = subject
                .strip_prefix("tests #")
                .and_then(|id| id.parse::<u64>().ok())
        {
            test_runs.mark_observed(id, &binding.fingerprint());
        }
    }
    committed.len()
}

/// Wakes `binding`'s idle host session with its holding facts as one user message (OUTBOX §8.4).
///
/// At most one attempt per idle period: the session must be idle, unwoken since it last worked,
/// and hold an undelivered holding fact. Only holding facts ride the wake, never source text. An
/// accepted or ambiguous wake delivers them; a refused or unavailable one returns them unchanged.
/// Returns `None` when no wake was attempted.
async fn wake_session(
    outbox: &Outbox,
    test_runs: &TestRuns,
    waker: &dyn SessionWaker,
    binding: &BindingRef,
) -> Option<WakeOutcome> {
    let session_id = outbox.claim_wake(binding)?;
    let reservation = outbox.reserve_facts(
        binding,
        HandoffRequest {
            carrier: Carrier::Wake,
            budget: MAX_WAKE_BYTES - WAKE_TAIL.len() - WAKE_FRAME,
            holding_only: true,
        },
    )?;
    let lines = reservation
        .facts
        .iter()
        .map(render_fact)
        .collect::<Vec<_>>();
    let text = compose_block(None, &lines, reservation.remaining)
        .map(|block| format!("{block}\n{WAKE_TAIL}"))
        .filter(|text| text.len() <= MAX_WAKE_BYTES);
    let target = WakeTarget {
        host: binding.host(),
        actor_is_main: binding.actor_is(&session_id),
        session_id,
    };
    let outcome = match &text {
        Some(text) => waker.wake(&target, text).await,
        None => WakeOutcome::Refused,
    };
    if matches!(outcome, WakeOutcome::Accepted | WakeOutcome::Ambiguous) {
        commit_collected(outbox, test_runs, binding, reservation.token);
    } else {
        outbox.release_reservation(binding, reservation.token);
    }
    Some(outcome)
}

impl Shared {
    /// Schedules the idle wake (OUTBOX §8.4) of each of `recipients` whose agent is idle; the
    /// attempt runs after [`WAKE_GRACE`] and re-checks every condition then.
    pub(super) fn schedule_wake(&self, recipients: &[BindingRef]) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        for binding in recipients {
            if self.outbox.agent_state(binding) != Some(AgentState::Idle) {
                continue;
            }
            let outbox = Arc::clone(&self.outbox);
            let test_runs = self.test_runs.clone();
            let waker = Arc::clone(&self.waker);
            let binding = binding.clone();
            runtime.spawn(async move {
                tokio::time::sleep(WAKE_GRACE).await;
                let _ = wake_session(&outbox, &test_runs, waker.as_ref(), &binding).await;
            });
        }
    }

    /// Defers the job behind `reference` after its caller left with `pending` (the inline-wait
    /// timeout): its final answer becomes a holding `job-ready` fact.
    pub(super) fn defer_pending(&self, reference: &str, tool: AssistanceTool, target: String) {
        let spec = DeferSpec {
            tool,
            target,
            fence_paths: Vec::new(),
            approximate_given: false,
        };
        let _ = SessionOutbox::defer(self, reference, spec);
    }

    /// Settles the outbox side of a job: a written edit advances the owner's edit counter, and a
    /// deferred job's final reply becomes its `job-ready` fact.
    ///
    /// An edit already answered early with `diagnostics: pending` (its holding registration is
    /// keyed by the job's reference) pushes its `edit-checked` fact instead, and its write was
    /// already counted when it was answered.
    pub(super) fn job_settled(
        &self,
        reference: &str,
        owner: &BindingRef,
        reply: &PeerReply,
        deferred: Option<(DeferSpec, JobEpoch)>,
    ) {
        let held = self.outbox.take_holding(reference);
        if let PeerReply::Edit { result, .. } = reply
            && result.outcome.has_post_source()
            && (held.is_none() || deferred.is_some())
        {
            self.outbox
                .note_edit(owner, std::slice::from_ref(&result.path));
        }
        if let Some((spec, epoch)) = deferred {
            self.outbox.push_draft(
                std::slice::from_ref(owner),
                job_fact(reference, reply, &spec),
                epoch,
            );
            self.schedule_wake(std::slice::from_ref(owner));
            return;
        }
        if let (
            Some(work),
            PeerReply::Edit {
                result,
                diagnostics,
                ..
            },
        ) = (held, reply)
        {
            self.outbox.push_draft(
                std::slice::from_ref(&work.owner),
                edit_checked_fact(reference, result, diagnostics),
                work.epoch,
            );
            self.schedule_wake(std::slice::from_ref(&work.owner));
        }
    }

    /// The job behind `reference` reached `binding` through its own reference: its deferral ends
    /// and any pushed fact for it is delivered.
    pub(super) fn reference_read(&self, binding: &BindingRef, reference: &str) {
        let deferred = self
            .ledger
            .lock()
            .ok()
            .and_then(|mut ledger| ledger.deferred.remove(reference));
        if deferred.is_some() {
            let _ = self.outbox.take_holding(reference);
        }
        self.outbox.mark_reference_read(binding, reference);
    }

    /// Registers run `id`, whose caller left with a started line, as holding work of `binding`
    /// until its budget ends; a run that already settled pushes its fact at once.
    pub(super) fn test_started(&self, id: u64, binding: &BindingRef, budget: Duration) {
        let subject = test_subject(id);
        let until = tokio::time::Instant::now() + budget + RUN_SETTLE_SLACK;
        if self
            .outbox
            .begin_holding(&subject, binding, &subject, until)
            && self.test_runs.settled_result(id).is_some()
        {
            self.test_settled(id);
        }
    }

    /// Adds `binding` as a recipient of run `id`'s fact (a second `ide.test` on a busy worktree).
    pub(super) fn test_subscribed(&self, id: u64, binding: &BindingRef) {
        let _ = self.outbox.subscribe(&test_subject(id), binding);
    }

    /// Pushes settled run `id`'s `test-finished` fact to its owner and subscribers, exactly once:
    /// only the call that takes the run's holding registration pushes.
    pub(super) fn test_settled(&self, id: u64) {
        let Some(result) = self.test_runs.settled_result(id) else {
            return;
        };
        let Some(work) = self.outbox.take_holding(&test_subject(id)) else {
            return;
        };
        self.outbox.push_draft(
            std::slice::from_ref(&work.owner),
            test_fact(id, &result, true),
            work.epoch,
        );
        if !work.subscribers.is_empty() {
            self.outbox
                .push_draft(&work.subscribers, test_fact(id, &result, false), work.epoch);
        }
        self.schedule_wake(std::slice::from_ref(&work.owner));
        self.schedule_wake(&work.subscribers);
    }

    /// Installs the test-run settle hook that produces `test-finished` facts; it holds only a weak
    /// reference, so a stopped worker is never kept alive by a running test.
    pub(super) fn install_test_settle_hook(self: &Arc<Self>) {
        let shared = Arc::downgrade(self);
        self.test_runs.on_settle(Arc::new(move |id| {
            if let Some(shared) = shared.upgrade() {
                shared.test_settled(id);
            }
        }));
    }
}

impl super::WorkerHandle {
    /// Queues the one `lost-after-restart` fact of `binding`, just re-activated after a daemon
    /// replacement, for the `subjects` its front relayed unresolved (OUTBOX §10). A later
    /// re-activation with the same list replaces it (coalescing on the fixed subject).
    pub fn queue_lost(&self, binding: &BindingRef, subjects: &[String]) {
        if let Some(draft) = lost_fact(subjects) {
            let outbox = &self.shared.outbox;
            outbox.push_draft(
                std::slice::from_ref(binding),
                draft,
                outbox.current_epoch(binding),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target is the first named argument, bounded, with a fallback.
    #[test]
    fn call_targets_name_the_main_argument() {
        assert_eq!(
            call_target(
                AssistanceTool::Symbol,
                &serde_json::json!({"symbol": "src/a.rs#Foo", "path": "x"})
            ),
            "src/a.rs#Foo"
        );
        assert_eq!(
            call_target(AssistanceTool::Test, &serde_json::json!({})),
            "run"
        );
        let long = "p".repeat(500);
        assert_eq!(
            call_target(AssistanceTool::Read, &serde_json::json!({ "path": long })).len(),
            MAX_TARGET_BYTES
        );
    }

    /// A settled run becomes a `test-finished` draft that is exactly M-024's terminal text, valid
    /// for the outbox, with the owner's reference named once and none for a subscriber.
    #[test]
    fn test_fact_is_the_terminal_text_of_the_run() {
        let result = RunResult {
            report: crate::lang::TestReport {
                passed: 2,
                failed: 1,
                ignored: 0,
                failures: vec![crate::lang::TestFailure {
                    name: "tests::fails".to_owned(),
                    location: None,
                    message: "boom".to_owned(),
                }],
                incomplete: false,
            },
            output: "output\n".to_owned(),
            output_paged: false,
            elapsed: Duration::from_secs(3),
            stopped: false,
            exit: Some(101),
            budget: Duration::from_secs(120),
            detail_ref: "run-detail".to_owned(),
            command: vec!["cargo".to_owned(), "test".to_owned()],
            rerun_dir: None,
            environment_label: None,
            targets: Vec::new(),
        };
        let owner = test_fact(7, &result, true);
        let expected = terminal_fact(7, &result, true);
        assert_eq!(owner.summary, "2 passed, 1 failed, 3 s");
        assert_eq!(owner.detail.as_deref(), Some(expected.detail.as_str()));
        assert_eq!(owner.subject, "tests #7");
        assert!(owner.failed && owner.holds_turn && !owner.source_bearing);
        assert_eq!(owner.detail_ref.as_deref(), Some("run-detail"));
        assert_eq!(owner.validate(), Ok(()));
        assert_eq!(
            owner
                .detail
                .as_deref()
                .unwrap()
                .matches("run-detail")
                .count(),
            1
        );
        let subscriber = test_fact(7, &result, false);
        assert_eq!(subscriber.detail_ref, None);
        assert!(!subscriber.detail.unwrap().contains("run-detail"));
    }

    /// Source-bearing answers carry no text in their fact; failures carry their first line.
    #[test]
    fn job_facts_keep_source_text_out() {
        let spec = DeferSpec {
            tool: AssistanceTool::Symbol,
            target: "src/a.rs#Foo".to_owned(),
            fence_paths: Vec::new(),
            approximate_given: false,
        };
        let ready = PeerReply::Complete {
            kind: super::super::super::reply::ResultKind::Context,
            text: "fn secret() {}".to_owned(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        let fact = job_fact("r-1", &ready, &spec);
        assert_eq!(fact.summary, "ready");
        assert_eq!(fact.detail, None);
        assert!(fact.source_bearing && fact.holds_turn && !fact.failed);
        assert_eq!(fact.subject, "ide.symbol src/a.rs#Foo");
        assert_eq!(fact.validate(), Ok(()));
        let failed = PeerReply::Error {
            code: super::super::super::reply::FailureCode::Deadline,
            detail: None,
        };
        let fact = job_fact("r-1", &failed, &spec);
        assert!(fact.failed && !fact.source_bearing);
        assert_ne!(fact.summary, "ready");
        assert_eq!(fact.validate(), Ok(()));
    }

    /// A pending answer owes its `job-ready` subject (the one its fact carries), a started run
    /// its `tests #N`; answered calls, starts and running or settled runs owe nothing.
    #[test]
    fn owed_facts_match_the_facts_that_answer_them() {
        let symbol = serde_json::json!({"symbol":"src/a.rs#Foo"});
        let pending = PeerReply::Pending {
            detail_ref: "r-1".to_owned(),
        };
        let spec = DeferSpec {
            tool: AssistanceTool::Symbol,
            target: call_target(AssistanceTool::Symbol, &symbol),
            fence_paths: Vec::new(),
            approximate_given: false,
        };
        let Some((FactKind::JobReady, subject, Some(reference))) =
            owed_fact(AssistanceTool::Symbol, &symbol, &pending)
        else {
            panic!("a pending answer owes a job-ready fact")
        };
        assert_eq!(subject, job_fact("r-1", &pending, &spec).subject);
        assert_eq!(reference, "r-1");
        assert_eq!(owed_fact(AssistanceTool::Start, &symbol, &pending), None);
        let line = |text: &str| PeerReply::Complete {
            kind: ResultKind::Test,
            text: text.to_owned(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        assert_eq!(
            owed_fact(
                AssistanceTool::Test,
                &Value::Null,
                &line("tests #4: started — cargo test (budget 60 s)")
            ),
            Some((FactKind::TestFinished, "tests #4".to_owned(), None))
        );
        assert_eq!(
            owed_fact(
                AssistanceTool::Test,
                &Value::Null,
                &line("tests #4: running")
            ),
            None
        );
        assert_eq!(settled_run(&line("tests #4: running")), None);
        assert_eq!(settled_run(&line("tests #4: 3 passed, 2 s")), Some(4));
    }

    /// The restart fact lists every subject, labels runs, fits its summary bound and never holds
    /// a turn; nothing to report queues nothing.
    #[test]
    fn the_lost_fact_lists_subjects_within_its_bound() {
        let fact = lost_fact(&["tests #3".to_owned(), "edit op-7".to_owned()]).unwrap();
        assert_eq!(
            fact.summary,
            "the IDE restarted; results not delivered before it may be lost: tests #3 (before \
             restart), edit op-7"
        );
        assert_eq!(fact.subject, "restart");
        assert!(!fact.holds_turn && fact.validate().is_ok());
        let many = (0..16)
            .map(|index| format!("ide.symbol src/{index}/{}", "x".repeat(40)))
            .collect::<Vec<_>>();
        let fact = lost_fact(&many).unwrap();
        assert!(fact.summary.len() <= MAX_SUMMARY_BYTES, "{}", fact.summary);
        assert!(fact.summary.ends_with(" more"), "{}", fact.summary);
        assert_eq!(fact.validate(), Ok(()));
        assert!(lost_fact(&[]).is_none());
    }
}

#[cfg(test)]
mod worker_tests {
    use super::super::super::host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
    };
    use super::super::super::outbox::Carrier;
    use super::super::*;
    use super::*;

    /// Establishes one real Codex binding for `actor` on `bindings`, as the dedup tests do.
    fn bind(bindings: &Arc<Mutex<HostBindingGuard>>, actor: &str) -> BindingRef {
        let mut guard = bindings.lock().unwrap();
        let channel = parse_channel_session(b"outbox-delivery").unwrap();
        let call = format!("{actor}-call");
        let hook = parse_hook_event(
            serde_json::json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":call})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            serde_json::json!({"threadId":actor,"callId":call,"x-codex-turn-metadata":{}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("fixture binding must validate")
        };
        let binding = invocation.binding_ref().clone();
        guard.consume_active(&binding).unwrap();
        binding
    }

    /// A bare worker handle with no targets; these tests drive the ledger and outbox directly.
    fn handle(bindings: Arc<Mutex<HostBindingGuard>>) -> WorkerHandle {
        let launcher = LauncherConfig::parse(
            br#"{"version":1,"limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},"targets":[]}"#,
        )
        .unwrap();
        WorkerHandle::new(
            bindings,
            launcher,
            [9; 32],
            Arc::new(Mutex::new(admission_controller())),
        )
    }

    /// Plants one ledger detail owned by `binding` with `reply`, as a job leaves it.
    fn plant(handle: &WorkerHandle, reference: &str, binding: &BindingRef, reply: PeerReply) {
        handle.shared.ledger.lock().unwrap().details.insert(
            reference.to_owned(),
            Detail {
                binding: binding.clone(),
                reply,
                selection: (
                    AssistanceTool::Read,
                    selection(&serde_json::json!({"path":"src/a.rs"})),
                ),
                authority: None,
                source: None,
                native_epoch: 0,
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
            },
        );
    }

    /// A settled read answer.
    fn answer() -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: "fn a() {}".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        }
    }

    /// A pending placeholder for `reference`.
    fn pending(reference: &str) -> PeerReply {
        PeerReply::Pending {
            detail_ref: reference.to_owned(),
        }
    }

    /// The deferral spec the inline-wait timeout registers for a read.
    fn read_spec() -> DeferSpec {
        DeferSpec {
            tool: AssistanceTool::Read,
            target: "src/a.rs".to_owned(),
            fence_paths: Vec::new(),
            approximate_given: false,
        }
    }

    /// A hook-shaped hand-off request.
    fn hook() -> HandoffRequest {
        HandoffRequest {
            carrier: Carrier::Hook,
            budget: 4096,
            holding_only: false,
        }
    }

    /// OUTBOX §3.2 boundary: a deferred pending job settles into exactly one holding `job-ready`
    /// fact for its owner, with no source text on a hook carrier, delivered once.
    #[tokio::test]
    async fn a_deferred_job_settles_into_one_job_ready_fact() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = &*handle.shared;
        shared.outbox.open(&owner);
        plant(&handle, "r-1", &owner, pending("r-1"));
        assert!(SessionOutbox::defer(shared, "r-1", read_spec()));
        assert!(
            !SessionOutbox::defer(shared, "r-1", read_spec()),
            "a second defer is a no-op"
        );
        assert!(shared.outbox.holding_until(&owner).is_some());
        assert!(!shared.outbox.has_undelivered(&owner, false));
        shared.complete("r-1", answer(), None, None, 0);
        assert!(shared.outbox.holding_until(&owner).is_none());
        let reservation = shared.reserve(&owner, hook()).expect("one fact");
        assert_eq!(reservation.facts.len(), 1);
        let fact = &reservation.facts[0];
        assert_eq!(fact.kind, FactKind::JobReady);
        assert_eq!(fact.subject, "ide.read src/a.rs");
        assert_eq!(fact.summary, "ready");
        assert_eq!(fact.detail, None);
        assert_eq!(fact.detail_ref.as_deref(), Some("r-1"));
        assert!(fact.holds_turn);
        assert_eq!(shared.commit(&owner, reservation.token), 1);
        assert!(shared.reserve(&owner, hook()).is_none(), "delivered once");
    }

    /// A job read through its own reference is never pushed: before settlement the deferral
    /// ends, after it the pushed fact is delivered; a defer of an already settled job pushes once.
    #[tokio::test]
    async fn reads_through_the_reference_deliver_and_late_defers_push_once() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = &*handle.shared;
        shared.outbox.open(&owner);
        plant(&handle, "r-2", &owner, answer());
        assert!(SessionOutbox::defer(shared, "r-2", read_spec()));
        assert!(shared.outbox.has_undelivered(&owner, true));
        shared.reference_read(&owner, "r-2");
        assert!(!shared.outbox.has_undelivered(&owner, false));
        plant(&handle, "r-3", &owner, pending("r-3"));
        assert!(SessionOutbox::defer(shared, "r-3", read_spec()));
        shared.reference_read(&owner, "r-3");
        shared.complete("r-3", answer(), None, None, 0);
        assert!(!shared.outbox.has_undelivered(&owner, false));
        assert!(shared.outbox.holding_until(&owner).is_none());
    }

    /// A run whose caller left with a started line pushes one `test-finished` fact to its owner
    /// (with the output reference) and one to a subscriber (without), exactly once; handing the
    /// owner's fact over collects the run for `ide.stop`.
    #[tokio::test]
    async fn a_started_run_reaches_owner_and_subscriber_once() {
        crate::lang::testing::install();
        let gamma = crate::lang::Language::by_id("gamma").expect("test language");
        let root = std::path::PathBuf::from(format!(
            "/private/tmp/m026-outbox-run-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let subscriber = bind(&bindings, "subscriber");
        let handle = handle(bindings);
        handle.shared.install_test_settle_hook();
        let shared = &*handle.shared;
        shared.outbox.open(&owner);
        shared.outbox.open(&subscriber);
        let id = match shared.test_runs.start(
            root.clone(),
            vec!["/bin/sleep".into(), "0.3".into()],
            gamma,
            Duration::from_secs(30),
            "run-detail".into(),
            &owner,
        ) {
            StartResult::Started(id) => id,
            StartResult::Running(..) | StartResult::Failed { .. } => panic!("run must start"),
        };
        shared.test_started(id, &owner, Duration::from_secs(30));
        shared.test_subscribed(id, &subscriber);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        assert!(shared.outbox.wait_ready(&owner, true, deadline).await);
        let owned = shared.reserve(&owner, hook()).expect("owner fact");
        assert_eq!(owned.facts.len(), 1);
        assert_eq!(owned.facts[0].subject, test_subject(id));
        assert_eq!(owned.facts[0].detail_ref.as_deref(), Some("run-detail"));
        let subscribed = shared
            .reserve(&subscriber, hook())
            .expect("subscriber fact");
        assert_eq!(subscribed.facts[0].detail_ref, None);
        assert!(
            shared
                .test_runs
                .uncollected(&owner.fingerprint())
                .iter()
                .any(|(run, _, _)| *run == id)
        );
        assert_eq!(shared.commit(&owner, owned.token), 1);
        assert!(
            shared
                .test_runs
                .uncollected(&owner.fingerprint())
                .is_empty()
        );
        shared.test_settled(id);
        assert!(
            shared.reserve(&owner, hook()).is_none(),
            "a second settle pushes nothing"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A draft fenced at the session's current epoch.
    fn finished(shared: &Shared, owner: &BindingRef, subject: &str) -> bool {
        SessionOutbox::push(
            shared,
            std::slice::from_ref(owner),
            FactDraft {
                kind: FactKind::TestFinished,
                subject: subject.to_owned(),
                summary: "1 passed, 1 failed, 2 s".to_owned(),
                detail: Some("  FAIL a::b".to_owned()),
                detail_ref: Some("run-1".to_owned()),
                failed: true,
                holds_turn: true,
                source_bearing: false,
                fence_paths: Vec::new(),
            },
            shared.outbox.current_epoch(owner),
        )
    }

    /// OUTBOX §6 boundary: two carriers racing for one session (a hook and a reply) hand each
    /// fact over exactly once, whatever the interleaving.
    #[test]
    fn concurrent_carriers_hand_a_fact_over_once() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = Arc::new(handle(bindings));
        handle.shared.outbox.open(&owner);
        for round in 0..200 {
            assert!(finished(&handle.shared, &owner, &format!("tests #{round}")));
            let carriers = [Carrier::Hook, Carrier::Reply].map(|carrier| {
                let handle = handle.clone();
                let owner = owner.clone();
                std::thread::spawn(move || handle.take_facts(&owner, carrier, None, 4096, |_| true))
            });
            let delivered = carriers
                .into_iter()
                .filter_map(|carrier| carrier.join().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(delivered.len(), 1, "round {round}: {delivered:?}");
            assert!(delivered[0].contains(&format!("test-finished tests #{round}: ")));
        }
    }

    /// A carrier composes facts after the plate in one block, labels a fact that ran before a
    /// later edit, keeps it when the block does not fit, and never repeats a delivered fact.
    #[test]
    fn carriers_compose_label_and_release() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        assert!(finished(&handle.shared, &owner, "tests #1"));
        handle
            .shared
            .outbox
            .note_edit(&owner, &["src/b.rs".to_owned()]);
        let plate = Some("<agent-ide>\nrust: 0 errors, 0 warnings\n</agent-ide>".to_owned());
        let refused = handle.take_facts(&owner, Carrier::Reply, plate.clone(), 4096, |_| false);
        assert_eq!(refused, plate, "a block that does not fit keeps the facts");
        let block = handle
            .take_facts(&owner, Carrier::Hook, plate.clone(), 4096, |_| true)
            .unwrap();
        assert_eq!(
            block,
            "<agent-ide>\nrust: 0 errors, 0 warnings\ntest-finished tests #1: 1 passed, 1 \
             failed, 2 s (ran before your later edits to src/b.rs)\n  FAIL a::b\n  full output: \
             ide.inspect run-1\n</agent-ide>"
        );
        assert_eq!(
            handle.take_facts(&owner, Carrier::Hook, plate.clone(), 4096, |_| true),
            plate
        );
    }

    /// OUTBOX §8.3 boundary: a turn end with a ready holding fact blocks once with the facts and
    /// the restate sentence; the next turn end, with nothing left and nothing in flight, releases
    /// at once and leaves the session idle.
    #[tokio::test]
    async fn a_hold_blocks_once_with_a_ready_fact_and_releases_without_one() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        assert!(finished(&handle.shared, &owner, "tests #1"));
        let reason = handle
            .hold_turn(&owner, false, Some("owner"))
            .await
            .expect("a ready holding fact blocks");
        assert!(
            reason.starts_with("<agent-ide>\ntest-finished tests #1: 1 passed, 1 failed"),
            "{reason}"
        );
        assert!(
            reason.ends_with(&format!("</agent-ide>\n{}", super::super::turn::RESTATE)),
            "{reason}"
        );
        let started = tokio::time::Instant::now();
        assert_eq!(handle.hold_turn(&owner, false, Some("owner")).await, None);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(
            handle.shared.outbox.agent_state(&owner),
            Some(super::super::super::session_outbox::AgentState::Idle)
        );
    }

    /// A turn end with holding work in flight waits for its fact and blocks with it; work that
    /// never settles releases at its budget end without a block.
    #[tokio::test]
    async fn a_hold_waits_for_in_flight_work_and_releases_at_its_deadline() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = Arc::new(handle(bindings));
        handle.shared.outbox.open(&owner);
        let until = tokio::time::Instant::now() + Duration::from_secs(5);
        assert!(
            handle
                .shared
                .outbox
                .begin_holding("tests #1", &owner, "tests #1", until)
        );
        let settle = {
            let handle = handle.clone();
            let owner = owner.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = handle.shared.outbox.take_holding("tests #1");
                assert!(finished(&handle.shared, &owner, "tests #1"));
            })
        };
        let reason = handle.hold_turn(&owner, false, None).await;
        settle.await.unwrap();
        assert!(reason.is_some_and(|reason| reason.contains("test-finished tests #1")));
        let short = tokio::time::Instant::now() + Duration::from_millis(200);
        assert!(
            handle
                .shared
                .outbox
                .begin_holding("tests #2", &owner, "tests #2", short)
        );
        let started = tokio::time::Instant::now();
        assert_eq!(handle.hold_turn(&owner, false, None).await, None);
        assert!(started.elapsed() >= Duration::from_millis(150));
    }

    /// A Claude subagent's turn end whose session the registry does not prove interactive (a
    /// headless run) releases at once and keeps the fact for another carrier (OUTBOX §8.3 rule 6).
    #[tokio::test]
    async fn a_headless_subagent_stop_releases() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        assert!(finished(&handle.shared, &owner, "tests #1"));
        assert_eq!(
            handle
                .hold_turn(&owner, true, Some("no-registry-session"))
                .await,
            None
        );
        assert!(handle.shared.outbox.has_undelivered(&owner, true));
    }

    /// A compaction raises the compaction counter of the session's channel (OUTBOX §9.1).
    #[test]
    fn compaction_raises_the_context_epoch() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        assert_eq!(handle.outbox().context_epoch(&owner), None);
        handle.note_compaction(&owner);
        assert_eq!(handle.outbox().context_epoch(&owner), Some(1));
        handle.note_compaction(&owner);
        assert_eq!(handle.outbox().context_epoch(&owner), Some(2));
    }

    /// Probe Q1: Claude overrides a ninth consecutive Stop block, so a session's turn ends are
    /// blocked at most seven times in a row; any tool call or hook resets the count.
    #[tokio::test]
    async fn turn_ends_are_blocked_at_most_seven_times_in_a_row() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        for round in 1..=7 {
            assert!(finished(&handle.shared, &owner, &format!("tests #{round}")));
            assert!(
                handle.hold_turn(&owner, false, None).await.is_some(),
                "block {round}"
            );
        }
        assert!(finished(&handle.shared, &owner, "tests #8"));
        assert_eq!(handle.hold_turn(&owner, false, None).await, None);
        assert!(handle.shared.outbox.has_undelivered(&owner, true));
        handle.note_activity(&owner);
        assert!(handle.hold_turn(&owner, false, None).await.is_some());
    }

    /// OUTBOX §11: the stop flush lists every undelivered fact once; afterwards no producer,
    /// hold or wake reaches the stopped session.
    #[tokio::test]
    async fn a_stop_flushes_once_and_nothing_follows() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let outbox = &handle.shared.outbox;
        outbox.open(&owner);
        outbox.note_host_session(&owner, "owner");
        let until = tokio::time::Instant::now() + Duration::from_secs(60);
        assert!(outbox.begin_holding("tests #2", &owner, "tests #2", until));
        assert!(finished(&handle.shared, &owner, "tests #1"));
        let flush = outbox.flush_for_stop(&owner).expect("an undelivered fact");
        assert!(flush.contains("test-finished tests #1: "), "{flush}");
        assert_eq!(outbox.flush_for_stop(&owner), None);
        assert!(
            outbox.take_holding("tests #2").is_none(),
            "in-flight work ends"
        );
        assert!(!finished(&handle.shared, &owner, "tests #2"));
        handle.queue_lost(&owner, &["tests #9".to_owned()]);
        assert!(!outbox.has_undelivered(&owner, false));
        outbox.set_agent_state(&owner, AgentState::Idle);
        assert_eq!(outbox.claim_wake(&owner), None);
        let started = tokio::time::Instant::now();
        assert_eq!(handle.hold_turn(&owner, false, Some("owner")).await, None);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    /// OUTBOX §10: a re-activated session receives one `lost-after-restart` fact, and a repeated
    /// re-activation with the same list replaces it rather than adding another.
    #[test]
    fn lost_after_restart_is_one_fact_per_session() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let other = bind(&bindings, "other");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        handle.shared.outbox.open(&other);
        let lost = ["tests #3".to_owned(), "edit op-7".to_owned()];
        handle.queue_lost(&owner, &lost);
        handle.queue_lost(&owner, &lost);
        let block = handle
            .take_facts(&owner, Carrier::Hook, None, 4096, |_| true)
            .expect("the restart fact");
        assert_eq!(block.matches("lost-after-restart restart: ").count(), 1);
        assert!(
            block.contains("tests #3 (before restart), edit op-7"),
            "{block}"
        );
        assert!(!handle.shared.outbox.has_undelivered(&other, false));
        assert!(!handle.shared.outbox.has_undelivered(&owner, true));
    }

    /// A waker that records each wake and answers a fixed outcome.
    struct FakeWaker {
        /// Outcome every wake answers.
        outcome: WakeOutcome,
        /// Every target and text woken.
        woken: Mutex<Vec<(WakeTarget, String)>>,
    }

    impl SessionWaker for FakeWaker {
        /// Records the wake and answers the fixed outcome.
        fn wake<'a>(
            &'a self,
            target: &'a WakeTarget,
            text: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = WakeOutcome> + Send + 'a>> {
            self.woken
                .lock()
                .unwrap()
                .push((target.clone(), text.to_owned()));
            Box::pin(std::future::ready(self.outcome))
        }
    }

    /// OUTBOX §8.4: only an idle session with an undelivered holding fact is woken, once per idle
    /// period, with its holding facts only; a refused wake keeps them, an accepted one delivers.
    #[tokio::test]
    async fn an_idle_session_is_woken_once_with_its_holding_facts() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = &handle.shared;
        let outbox = &shared.outbox;
        let waker = |outcome| FakeWaker {
            outcome,
            woken: Mutex::default(),
        };
        let accepting = waker(WakeOutcome::Accepted);
        let refusing = waker(WakeOutcome::Refused);
        outbox.open(&owner);
        outbox.note_host_session(&owner, "owner");
        outbox.set_agent_state(&owner, AgentState::Idle);
        let unrequested = FactDraft {
            kind: FactKind::JobReady,
            subject: "ide.read src/a.rs".to_owned(),
            summary: "ready".to_owned(),
            detail: None,
            detail_ref: Some("job-1".to_owned()),
            failed: false,
            holds_turn: false,
            source_bearing: false,
            fence_paths: Vec::new(),
        };
        assert!(SessionOutbox::push(
            shared.as_ref(),
            std::slice::from_ref(&owner),
            unrequested,
            outbox.current_epoch(&owner)
        ));
        let wake = |waker| wake_session(outbox, &shared.test_runs, waker, &owner);
        assert_eq!(wake(&accepting).await, None, "unrequested facts never wake");
        outbox.set_agent_state(&owner, AgentState::Working);
        assert!(finished(shared, &owner, "tests #1"));
        assert_eq!(wake(&accepting).await, None, "a working agent is not woken");
        outbox.set_agent_state(&owner, AgentState::Idle);
        assert_eq!(wake(&refusing).await, Some(WakeOutcome::Refused));
        assert!(outbox.has_undelivered(&owner, true), "a refused wake keeps");
        assert_eq!(wake(&accepting).await, None, "one attempt per idle period");
        outbox.set_agent_state(&owner, AgentState::Working);
        outbox.set_agent_state(&owner, AgentState::Idle);
        assert_eq!(wake(&accepting).await, Some(WakeOutcome::Accepted));
        let woken = accepting.woken.lock().unwrap();
        let [(target, text)] = woken.as_slice() else {
            panic!("exactly one wake: {woken:?}")
        };
        assert_eq!(target.session_id, "owner");
        assert!(target.actor_is_main);
        assert!(
            text.starts_with("<agent-ide>\ntest-finished tests #1"),
            "{text}"
        );
        assert!(
            text.ends_with(&format!("</agent-ide>\n{WAKE_TAIL}")),
            "{text}"
        );
        assert!(!text.contains("ide.read"), "{text}");
        assert!(!outbox.has_undelivered(&owner, true));
        assert!(outbox.has_undelivered(&owner, false));
    }
}
