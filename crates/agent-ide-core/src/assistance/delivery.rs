//! Worker-side production of completion facts for Bridge-only delivery (OUTBOX-r3 §2-§3, §12).
//!
//! The worker's `Shared` state is the daemon's
//! [`SessionOutbox`](crate::assistance::outbox::SessionOutbox): it owns the in-memory
//! [`Outbox`](crate::assistance::session_outbox::Outbox) ledger and adds what only the worker knows,
//! the job ledger behind `defer`, the test-run registry behind `test-finished` facts, and the
//! translation of settled replies into fact drafts that are transferred to Bridge. Everything here
//! is synchronous bookkeeping under short locks, except the detached Bridge publication; nothing
//! awaits while a lock is held.

use std::sync::Arc;

use serde_json::Value;

use super::super::{
    content::{self, Envelope},
    facade::AssistanceTool,
    host_binding::BindingRef,
    outbox::FencedPath,
    outbox::{
        DeferSpec, FactDraft, FactKind, JobEpoch, MAX_FACT_BYTES, MAX_SUBJECT_BYTES,
        MAX_SUMMARY_BYTES, MAX_TARGET_BYTES, SessionOutbox,
    },
    reply::{EditDiagnostics, PeerReply, ResultKind},
    test_fact::terminal_fact,
    tests::{RunResult, summary_absent},
};
use super::{Job, JobStage, Shared, Worker, edit_operation};
use crate::changes::edit::EditResult;

/// Most diagnostic `new:` lines one `edit-checked` fact carries (OUTBOX §3.3).
const MAX_NEW_LINES: usize = 8;

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
/// this subject): the subject of the fact that will answer it, and the reference that reads it
/// back. `None` when the reply answered the call.
pub(crate) fn owed_fact(
    tool: AssistanceTool,
    parameters: &Value,
    reply: &PeerReply,
) -> Option<(String, Option<String>)> {
    match reply {
        PeerReply::Pending { detail_ref }
            if !matches!(tool, AssistanceTool::Start | AssistanceTool::Stop)
                && (tool != AssistanceTool::Inspect || parameters.get("kind").is_some()) =>
        {
            let subject = format!("{} {}", tool.mcp_name(), call_target(tool, parameters));
            Some((
                bounded(&subject, MAX_SUBJECT_BYTES, false),
                Some(detail_ref.clone()),
            ))
        }
        PeerReply::Edit {
            result,
            diagnostics: EditDiagnostics::Pending { detail_ref },
            ..
        } if tool == AssistanceTool::Edit => Some((
            bounded(
                &format!("edit {}", result.operation_id),
                MAX_SUBJECT_BYTES,
                false,
            ),
            Some(detail_ref.clone()),
        )),
        PeerReply::Complete {
            kind: ResultKind::Graph,
            text,
            detail_ref: Some(reference),
            ..
        } if tool == AssistanceTool::Graph && text.starts_with("graph job ") => Some((
            bounded(
                &format!("ide.graph {}", call_target(tool, parameters)),
                MAX_SUBJECT_BYTES,
                false,
            ),
            Some(reference.clone()),
        )),
        PeerReply::Complete {
            kind: ResultKind::Test,
            text,
            ..
        } if tool == AssistanceTool::Test => match run_line(text) {
            Some((id, "started")) => Some((
                test_subject(id),
                match reply {
                    PeerReply::Complete { detail_ref, .. } => detail_ref.clone(),
                    _ => None,
                },
            )),
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
        source_bearing: false,
        fence_paths: Vec::new(),
    })
}

/// Whether a job's answer is source-derived and needs source-access proof before it is published.
/// Inspect jobs are diagnostic/problem queries; ordinary detail retrieval creates no job.
fn source_bearing(tool: AssistanceTool) -> bool {
    matches!(
        tool,
        AssistanceTool::Context
            | AssistanceTool::Inspect
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
        source_bearing: bearing && !failed,
        fence_paths: spec.fence_paths.clone(),
    }
}

/// Builds the useful deferred body for Bridge after local job authorization. A changed observed
/// source epoch yields an explicit terminal superseded result with no obsolete source body.
/// The public graph page is already bounded by the normal renderer; large continuations remain
/// explicit. This is domain result composition, not a transport queue or freshness grant.
fn bridge_job_fact(
    reference: &str,
    reply: &PeerReply,
    spec: &DeferSpec,
    captured: JobEpoch,
    current: JobEpoch,
) -> FactDraft {
    let mut fact = job_fact(reference, reply, spec);
    if fact.source_bearing && captured != current {
        fact.summary = "superseded: source changed during computation".into();
        fact.detail = Some(format!(
            "result_ref: {reference}; captured source is not current"
        ));
        fact.failed = true;
        fact.source_bearing = false;
        return fact;
    }
    if fact.source_bearing {
        let text = compact_text(reply).unwrap_or_else(|| "result unavailable".into());
        let mut lines = text.lines();
        fact.summary = bounded(
            lines.next().unwrap_or("result unavailable"),
            MAX_SUMMARY_BYTES,
            false,
        );
        let rest = lines.collect::<Vec<_>>().join("\n");
        let ceiling = 6 * 1024 - fact.summary.len() - reference.len() - 128;
        let mut detail = bounded(&rest, ceiling, true);
        if rest.len() > detail.len() {
            detail.push_str(&format!(
                "\npartial: bounded result; continue with {} {}",
                spec.tool.mcp_name(),
                serde_json::json!({"detail_ref":reference})
            ));
        }
        detail.push_str(&format!(
            "\nresult_ref: {reference}; source epoch {}/{}",
            captured.native_epoch, captured.edit_seq
        ));
        fact.detail = Some(detail);
    }
    fact
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
        failed: result.report.failed > 0
            || result.stopped
            || result.exit != Some(0)
            || summary_absent(result),
        source_bearing: false,
        fence_paths: Vec::new(),
    }
}

/// Builds the `edit-checked` draft of one single-file edit's project check (OUTBOX §3.3).
///
/// Reported diagnostics become at most eight `new:` lines under the counts summary; the fact is
/// never source-bearing. `reference` is the edit job's own result
/// reference: reading it delivers the fact.
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
                lines.push("  (more: ide.inspect {\"kind\":\"problems\"})".to_owned());
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
            true,
        ),
        EditDiagnostics::Unknown {} | EditDiagnostics::Pending { .. } => {
            ("1 file, diagnostics unknown".to_owned(), None, false)
        }
    };
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
        // Diagnostics name the file and the problem, never its source text (OUTBOX §2).
        source_bearing: false,
        fence_paths: vec![FencedPath {
            path: result.path.clone(),
            digest: None,
        }],
    }
}

/// Closes a check that owed a terminal result but could not produce matching diagnostics.
/// The edit already settled; this failed job notice never claims the write failed or vanished.
pub(super) fn incomplete_check_fact(reference: &str, operation_id: &str) -> FactDraft {
    FactDraft {
        kind: FactKind::JobReady,
        subject: bounded(&format!("edit {operation_id}"), MAX_SUBJECT_BYTES, false),
        summary: "edit settled; checks unavailable: matching diagnostics did not finish".into(),
        detail: None,
        detail_ref: Some(reference.to_owned()),
        failed: true,
        source_bearing: false,
        fence_paths: Vec::new(),
    }
}

/// Selects a terminal edit completion without discarding required failures.
/// This runs only after accepted work was registered: even an unavailable check must settle it.
/// An unknown matching result closes its outstanding wait with a failed job notice.
/// Every other terminal job outcome still produces a fact so an early caller is not stranded.
fn edit_completion_fact(reference: &str, reply: &PeerReply) -> FactDraft {
    match reply {
        PeerReply::Edit {
            result,
            diagnostics: EditDiagnostics::Unknown {},
            ..
        } => incomplete_check_fact(reference, &result.operation_id),
        PeerReply::Edit {
            result,
            diagnostics,
            ..
        } => edit_checked_fact(reference, result, diagnostics),
        _ => job_fact(
            reference,
            reply,
            &DeferSpec {
                tool: AssistanceTool::Edit,
                target: reference.to_owned(),
                fence_paths: Vec::new(),
            },
        ),
    }
}

impl Worker<'_> {
    /// Answers an edit whose project check is still running with `diagnostics: pending` at once,
    /// right after the write is observed, without waiting for the check (symbol-read-edit-v1 §3.3).
    ///
    /// Only a caller still waiting is answered, and only when the session ledger can track the
    /// check as in-flight work under the job's reference: the job stays parked on its check and
    /// its settlement hands one `edit-checked` fact to Bridge ([`Shared::job_settled`]). The write
    /// is counted before the registration, so the fact's fence epoch includes its own edit.
    pub(super) fn reply_edit_early(&self, job: &mut Job, stage: &JobStage) {
        // A multi-file edit answers early through its own path (`batch_edit`).
        let JobStage::EditAwaitingCheck { result, .. } = stage else {
            return;
        };
        let waiting = job
            .stop_reply
            .as_ref()
            .is_some_and(|sender| !sender.is_closed());
        let binding = job.invocation.binding_ref().clone();
        if !waiting || !self.shared.outbox.is_open(&binding) {
            return;
        }
        if result.outcome.has_post_source() {
            self.shared.outbox.note_edit(&binding);
        }
        if !self.shared.outbox.begin_work(&job.reference, &binding) {
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
            let _ = self.shared.outbox.take_work(&job.reference);
        }
    }
}

impl SessionOutbox for Shared {
    /// Hands the draft to Bridge for each recipient; nothing else carries it. Returns `true` only
    /// when at least one recipient reserved Bridge capacity and its local handoff task was
    /// spawned, never as evidence of native observation.
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, _epoch: JobEpoch) -> bool {
        if draft.validate().is_err() {
            return false;
        }
        self.push_or_transfer(recipients, draft)
    }

    /// Registers a pending job for its `job-ready` fact, or hands the fact to Bridge at once when
    /// the job already settled unread. The ledger lock is held across the pending check and the
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
        // The state the job started from (OUTBOX §3.1); a job still queued has not started, so
        // the session's state now is the closest earlier bound.
        let epoch = ledger
            .started
            .get(reference)
            .copied()
            .unwrap_or_else(|| self.outbox.current_epoch(&owner));
        if matches!(detail.reply, PeerReply::Pending { .. }) {
            ledger
                .deferred
                .insert(reference.to_owned(), (spec.clone(), epoch));
            drop(ledger);
            #[cfg(test)]
            tests::DEFER_GAP.with(|gap| {
                if let Some(gap) = gap.borrow().as_ref() {
                    gap();
                }
            });
            return true;
        }
        let reply = detail.reply.clone();
        drop(ledger);
        let draft = bridge_job_fact(
            reference,
            &reply,
            &spec,
            epoch,
            self.outbox.current_epoch(&owner),
        );
        self.push_or_transfer(&[owner], draft);
        true
    }

    /// Drops the session's in-flight registrations and deferrals silently.
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
        self.current_context_epoch(binding)
    }
}

impl Shared {
    /// Transfers one completion fact to Bridge, the only delivery owner, for each recipient.
    ///
    /// A recipient without a reserved Bridge route is skipped: the domain result stays retained
    /// under its reference and nothing else delivers it. A fact that carries source or names an
    /// authority is re-proved on the detached task right before publication and is replaced by an
    /// explicit superseded result when access or freshness changed. A retired binding emits
    /// nothing. Returns whether at least one local Bridge handoff was reserved and spawned; an
    /// accepted or uncertain async publication is never retried through another carrier.
    pub(super) fn push_or_transfer(&self, recipients: &[BindingRef], draft: FactDraft) -> bool {
        let bridge = self.bridge.lock().ok().and_then(|bridge| bridge.clone());
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let mut handed_off = false;
        for binding in recipients {
            let reserved = bridge
                .as_ref()
                .and_then(|bridge| bridge.reserve(binding, &draft));
            let Some((route, permit)) = reserved else {
                if self.direct_delivery() {
                    crate::errorlog::record(
                        crate::errorlog::Method::Daemon,
                        crate::errorlog::Outcome::Failed,
                        crate::errorlog::Fields {
                            detail: Some("bridge:handoff_unavailable; domain result retained"),
                            ..Default::default()
                        },
                    );
                }
                continue;
            };
            let check_result = draft.kind == FactKind::EditChecked;
            let source_proof = if check_result || draft.source_bearing || draft.detail.is_some() {
                draft.detail_ref.as_ref().and_then(|reference| {
                    self.ledger.lock().ok().and_then(|ledger| {
                        ledger
                            .details
                            .get(reference)
                            .filter(|detail| &detail.binding == binding)
                            .map(|detail| {
                                (
                                    detail.authority.clone(),
                                    detail.source.clone(),
                                    detail.diff_provenance.clone(),
                                    detail.extra_sources.clone(),
                                )
                            })
                    })
                })
            } else {
                None
            };
            let workspace = self
                .publication_workspace
                .lock()
                .ok()
                .and_then(|owner| owner.clone());
            let bridge = Arc::clone(bridge.as_ref().expect("reserved owner"));
            let bindings = Arc::clone(&self.bindings);
            let binding = binding.clone();
            let mut draft = draft.clone();
            runtime.spawn(async move {
                let _permit = permit;
                if !bindings
                    .lock()
                    .ok()
                    .is_some_and(|guard| guard.check_active(&binding).is_ok())
                {
                    bridge.end_scope(&binding);
                    return;
                }
                // Source-bearing data always needs proof. Source-free lifecycle/error facts
                // without an authority retain the already checked active-binding boundary.
                let has_authority = source_proof
                    .as_ref()
                    .is_some_and(|(authority, _, _, _)| authority.is_some());
                if check_result || draft.source_bearing || has_authority {
                    let permitted =
                        if let (Some(workspace), Some((Some(authority), source, paths, extras))) =
                            (workspace, source_proof)
                        {
                            let mut permitted = super::reprove_disclosure(
                                &workspace,
                                &bindings,
                                &binding,
                                Some(&authority),
                                source.as_ref(),
                                paths.as_ref(),
                            )
                            .await
                            .is_ok();
                            for extra in &extras {
                                if !permitted {
                                    break;
                                }
                                permitted = super::reprove_disclosure(
                                    &workspace,
                                    &bindings,
                                    &binding,
                                    Some(&authority),
                                    Some(extra),
                                    None,
                                )
                                .await
                                .is_ok();
                            }
                            permitted
                        } else {
                            false
                        };
                    if !permitted {
                        draft.summary = "superseded: source access or freshness changed".into();
                        draft.detail = Some(
                            "No obsolete source body was disclosed; result requires a fresh query."
                                .into(),
                        );
                        draft.failed = true;
                        draft.source_bearing = false;
                    }
                }
                bridge.publish(binding.clone(), &draft, route).await;
            });
            handed_off = true;
        }
        handed_off
    }

    /// Records the session state the job behind `reference` started from, once: the epoch its
    /// `job-ready` fact is fenced at (OUTBOX §3.1). Records of results no longer retained go.
    pub(super) fn note_job_started(&self, reference: &str, binding: &BindingRef) {
        let epoch = self.outbox.current_epoch(binding);
        if let Ok(mut ledger) = self.ledger.lock() {
            let super::Ledger {
                started, details, ..
            } = &mut *ledger;
            started.retain(|kept, _| details.contains_key(kept));
            if details.contains_key(reference) {
                started.entry(reference.to_owned()).or_insert(epoch);
            }
        }
    }

    /// Defers the job behind `reference` after its caller left with `pending` (the inline-wait
    /// timeout): its final answer becomes a `job-ready` fact handed to Bridge.
    pub(super) fn defer_pending(&self, reference: &str, tool: AssistanceTool, target: String) {
        let spec = DeferSpec {
            tool,
            target,
            fence_paths: Vec::new(),
        };
        let _ = SessionOutbox::defer(self, reference, spec);
    }

    /// Settles the ledger side of a job: a written edit advances the owner's edit counter, and a
    /// deferred job's final reply becomes its `job-ready` fact for Bridge.
    ///
    /// An edit already answered early with `diagnostics: pending` (its in-flight registration is
    /// keyed by the job's reference) hands its `edit-checked` fact over instead, and its write was
    /// already counted when it was answered.
    pub(super) fn job_settled(
        &self,
        reference: &str,
        owner: &BindingRef,
        reply: &PeerReply,
        deferred: Option<(DeferSpec, JobEpoch)>,
    ) {
        if let Some(status) = self.test_runs.started_run(reference, owner)
            && let Some(result) = status.result.as_ref()
        {
            self.retain_test_output(status.id, owner, result);
        }
        let held = self.outbox.take_work(reference);
        if let PeerReply::Edit { result, .. } = reply
            && result.outcome.has_post_source()
            && (held.is_none() || deferred.is_some())
        {
            self.outbox.note_edit(owner);
        }
        if let Some((spec, epoch)) = deferred {
            let draft = bridge_job_fact(
                reference,
                reply,
                &spec,
                epoch,
                self.outbox.current_epoch(owner),
            );
            self.push_or_transfer(std::slice::from_ref(owner), draft);
            return;
        }
        if let Some(work) = held {
            let draft = edit_completion_fact(reference, reply);
            let mut recipients = vec![work.owner];
            recipients.extend(work.subscribers);
            self.push_or_transfer(&recipients, draft);
        }
    }

    /// The job behind `reference` reached `binding` through its own reference: its deferral ends.
    pub(super) fn reference_read(&self, reference: &str) {
        let deferred = self
            .ledger
            .lock()
            .ok()
            .and_then(|mut ledger| ledger.deferred.remove(reference));
        if deferred.is_some() {
            let _ = self.outbox.take_work(reference);
        }
    }

    /// Registers run `id`, whose caller left with a started line, as in-flight work of `binding`;
    /// a run that already settled hands its fact over at once.
    pub(super) fn test_started(&self, id: u64, binding: &BindingRef) {
        let subject = test_subject(id);
        if self.outbox.begin_work(&subject, binding) && self.test_runs.settled_result(id).is_some()
        {
            self.test_settled(id);
        }
    }

    /// Adds `binding` as a recipient of run `id`'s fact (a second `ide.test` on a busy worktree).
    pub(super) fn test_subscribed(&self, id: u64, binding: &BindingRef) {
        let _ = self.outbox.subscribe(&test_subject(id), binding);
    }

    /// Hands settled run `id`'s `test-finished` fact to Bridge for its owner and subscribers,
    /// exactly once: only the call that takes the run's in-flight registration hands it over.
    pub(super) fn test_settled(&self, id: u64) {
        let Some(result) = self.test_runs.settled_result(id) else {
            return;
        };
        let Some(work) = self.outbox.take_work(&test_subject(id)) else {
            return;
        };
        self.retain_test_output(id, &work.owner, &result);
        self.push_or_transfer(
            std::slice::from_ref(&work.owner),
            test_fact(id, &result, true),
        );
        if !work.subscribers.is_empty() {
            self.push_or_transfer(&work.subscribers, test_fact(id, &result, false));
        }
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
    /// Associates a same-call Bridge route only after local managed Codex admission.
    /// Cross-actor/call/malformed tickets are ignored and associate no route.
    pub(crate) fn note_bridge_route(&self, binding: &BindingRef, call: &str, value: &Value) {
        let Some(ticket) = super::super::bridge::RouteTicket::from_private(value, binding, call)
        else {
            return;
        };
        // A concurrent stop may have fenced this call after assembly admission. Keep its
        // guard until the synchronous association ends so a stopped route cannot reappear.
        let Ok(bindings) = self.shared.bindings.lock() else {
            return;
        };
        if bindings.check_active(binding).is_err() {
            return;
        }
        if let Some(bridge) = self
            .shared
            .bridge
            .lock()
            .ok()
            .and_then(|bridge| bridge.clone())
        {
            bridge.note_route(binding, ticket);
        }
    }
    /// Publishes the one `lost-after-restart` fact of `binding`, just re-activated after a daemon
    /// replacement, for the `subjects` its front relayed unresolved (OUTBOX §10).
    ///
    /// Unlike ordinary retained result facts, the restart notice has no readable `detail_ref`, so
    /// this path awaits Bridge's bounded publication result. Returns `true` only for accepted or
    /// uncertain custody, which must never be retried; a missing reservation, retired binding or
    /// definite no-effect refusal returns `false` so the front retains the subjects for a later
    /// eligible restart. No Bridge or binding lock is held across the await.
    pub async fn queue_lost(&self, binding: &BindingRef, subjects: &[String]) -> bool {
        let Some(draft) = lost_fact(subjects) else {
            return false;
        };
        let Some(bridge) = self
            .shared
            .bridge
            .lock()
            .ok()
            .and_then(|bridge| bridge.clone())
        else {
            return false;
        };
        let Some((route, permit)) = bridge.reserve(binding, &draft) else {
            return false;
        };
        let active = self
            .shared
            .bindings
            .lock()
            .ok()
            .is_some_and(|guard| guard.check_active(binding).is_ok());
        if !active {
            bridge.end_scope(binding);
            return false;
        }
        let _permit = permit;
        bridge.publish(binding.clone(), &draft, route).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    thread_local! {
        /// Test seam: runs on the calling thread right after `defer` released the ledger, the
        /// moment a concurrent settlement of the job could land.
        pub(super) static DEFER_GAP: std::cell::RefCell<Option<Box<dyn Fn()>>> =
            const { std::cell::RefCell::new(None) };
    }

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

    /// Every accepted check settles, including postacceptance unavailable diagnostics; known
    /// unavailable checks never register work and therefore never reach this publication path.
    #[test]
    fn unavailable_checks_do_not_hide_required_terminal_failures() {
        let result = EditResult {
            operation_id: "availability".into(),
            path: "src/lib.rs".into(),
            outcome: crate::changes::edit::EditOutcome::Replaced,
            source_ref: None,
        };
        let unchecked = PeerReply::Edit {
            result: result.clone(),
            diagnostics: EditDiagnostics::NotAnalysed {
                reason: "checks unavailable: missing cargo home".into(),
            },
            note: None,
            operation: None,
        };
        let unavailable = edit_completion_fact("edit-ref", &unchecked);
        assert!(unavailable.summary.contains("missing cargo home"));
        assert!(unavailable.validate().is_ok());
        let reply = PeerReply::Edit {
            result,
            diagnostics: EditDiagnostics::CurrentClean { project_errors: 0 },
            note: None,
            operation: None,
        };
        assert_eq!(
            edit_completion_fact("edit-ref", &reply).kind,
            FactKind::EditChecked
        );
        let mut timeout = reply.clone();
        if let PeerReply::Edit { diagnostics, .. } = &mut timeout {
            *diagnostics = EditDiagnostics::Unknown {};
        }
        assert_eq!(
            edit_completion_fact("edit-ref", &timeout).kind,
            FactKind::JobReady
        );
        let fatal = PeerReply::Error {
            code: super::super::super::reply::FailureCode::Internal,
            detail: None,
        };
        let fact = edit_completion_fact("edit-ref", &fatal);
        assert!(fact.failed);
        assert_eq!(fact.kind, FactKind::JobReady);
        assert_eq!(fact.validate(), Ok(()));
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
            cancelled: false,
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
        assert_eq!(owner.summary, "2 passed, 1 failed, 3 s (exit 101)");
        assert_eq!(owner.detail.as_deref(), Some(expected.detail.as_str()));
        assert_eq!(owner.subject, "tests #7");
        assert!(owner.failed && !owner.source_bearing);
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

    /// A run that exits 0 but reported no recognised summary is not a pass: its fact is failed
    /// (reply-hygiene-v1 §1.1), while a counted passing run and a budget stop keep their meaning.
    #[test]
    fn a_zero_exit_without_a_summary_is_a_failed_fact() {
        let mut result = RunResult {
            report: crate::lang::TestReport {
                incomplete: true,
                ..crate::lang::TestReport::default()
            },
            output: "hello\n".to_owned(),
            output_paged: false,
            elapsed: Duration::from_secs(1),
            stopped: false,
            cancelled: false,
            exit: Some(0),
            budget: Duration::from_secs(120),
            detail_ref: "run-detail".to_owned(),
            command: vec!["echo".to_owned(), "hello".to_owned()],
            rerun_dir: None,
            environment_label: None,
            targets: Vec::new(),
        };
        assert!(test_fact(1, &result, true).failed);
        result.report = crate::lang::TestReport {
            passed: 3,
            ..crate::lang::TestReport::default()
        };
        assert!(!test_fact(1, &result, true).failed);
    }

    /// Source-bearing answers carry no text in their fact; failures carry their first line.
    #[test]
    fn job_facts_keep_source_text_out() {
        let spec = DeferSpec {
            tool: AssistanceTool::Symbol,
            target: "src/a.rs#Foo".to_owned(),
            fence_paths: Vec::new(),
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
        assert!(fact.source_bearing && !fact.failed);
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
        };
        let Some((subject, Some(reference))) = owed_fact(AssistanceTool::Symbol, &symbol, &pending)
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
            Some(("tests #4".to_owned(), None))
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

    /// The restart fact lists every subject, labels runs and fits its summary bound; nothing to
    /// report produces no fact.
    #[test]
    fn the_lost_fact_lists_subjects_within_its_bound() {
        let fact = lost_fact(&["tests #3".to_owned(), "edit op-7".to_owned()]).unwrap();
        assert_eq!(
            fact.summary,
            "the IDE restarted; results not delivered before it may be lost: tests #3 (before \
             restart), edit op-7"
        );
        assert_eq!(fact.subject, "restart");
        assert!(fact.validate().is_ok());
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
    use super::super::*;
    use super::*;
    use std::time::Duration;

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
        }
    }

    /// OUTBOX §3.2 boundary: a deferred pending job registers once, and a read through its own
    /// reference ends the deferral; a defer of an already settled job registers nothing. Without
    /// a Bridge route no other carrier takes the settled result.
    #[tokio::test]
    async fn a_deferral_registers_once_and_a_read_ends_it() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = &*handle.shared;
        let deferred = |reference: &str| {
            shared
                .ledger
                .lock()
                .unwrap()
                .deferred
                .contains_key(reference)
        };
        shared.outbox.open(&owner);
        plant(&handle, "r-1", &owner, pending("r-1"));
        assert!(SessionOutbox::defer(shared, "r-1", read_spec()));
        assert!(
            !SessionOutbox::defer(shared, "r-1", read_spec()),
            "a second defer is a no-op"
        );
        assert!(deferred("r-1"));
        shared.complete("r-1", answer(), None, None, 0);
        assert!(!deferred("r-1"), "settlement takes the registration");
        plant(&handle, "r-2", &owner, answer());
        assert!(SessionOutbox::defer(shared, "r-2", read_spec()));
        assert!(!deferred("r-2"), "a settled job registers nothing");
        plant(&handle, "r-3", &owner, pending("r-3"));
        assert!(SessionOutbox::defer(shared, "r-3", read_spec()));
        shared.reference_read("r-3");
        assert!(!deferred("r-3"), "a read ends the deferral");
        shared.complete("r-3", answer(), None, None, 0);
    }

    /// Restart-loss subjects stay with the front when this daemon has no Bridge delivery owner;
    /// an activation reply alone is not a handoff acknowledgment.
    #[tokio::test]
    async fn restart_loss_without_bridge_reports_no_handoff() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        assert!(
            !handle
                .queue_lost(&owner, &["ide.read src/a.rs".to_owned()])
                .await
        );
    }

    /// A reservable restart notice reports Bridge's final bounded custody classification: a
    /// definite no-effect refusal stays retryable, while accepted and uncertain outcomes consume
    /// the front's unresolved subjects and never use another carrier.
    #[tokio::test]
    async fn restart_loss_awaits_bridge_custody_before_handoff() {
        use crate::{
            app::store::Store,
            assistance::bridge::{BridgeConfig, BridgeDelivery, NativeStore},
        };
        use bridge_sdk::bridge_protocol as wire;
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };

        for (index, response, expected) in [
            (
                0,
                Some((
                    wire::ErrorClass::Capacity,
                    wire::MutationEffect::NotCommitted,
                )),
                false,
            ),
            (
                1,
                Some((
                    wire::ErrorClass::OutcomeUnknown,
                    wire::MutationEffect::Unknown,
                )),
                true,
            ),
            (2, None, true),
        ] {
            let scratch = crate::scratch::ScratchDir::new(&format!("lost-handoff-{index}"));
            let config = BridgeConfig {
                registration_socket: scratch.join("adapter.sock"),
                core_socket: scratch.join("core.sock"),
                credential_file: scratch.join("token"),
                native_stores: vec![NativeStore {
                    harness: wire::Host::Codex,
                    host: "local".into(),
                    store: "fixture".into(),
                }],
            };
            std::fs::write(&config.credential_file, "fixture-token").unwrap();
            std::fs::set_permissions(
                &config.credential_file,
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            let store = Arc::new(
                Store::open_with_backup_root(
                    &scratch.join("state.sqlite"),
                    &scratch.join("backups"),
                    crate::app::config::EffectiveConfig::defaults().store(),
                )
                .unwrap(),
            );
            let bridge = BridgeDelivery::open(&config, store, [index as u8 + 1; 32])
                .await
                .unwrap();
            let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
            let owner = bind(&bindings, "owner");
            let handle = handle(bindings);
            *handle.shared.bridge.lock().unwrap() = Some(bridge);

            let listener = UnixListener::bind(&config.core_socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut frame = String::new();
                BufReader::new(&socket).read_line(&mut frame).unwrap();
                let request = wire::decode_request(frame.trim_end().as_bytes()).unwrap();
                let wire::Operation::Publish(params) = &request.operation else {
                    panic!("publish only")
                };
                assert_eq!(params.kind, "ide.job.lost");
                let response = match response {
                    Some((class, effect)) => wire::Response::error(
                        request.request_id,
                        wire::ApiError {
                            class,
                            reason: "fixture".into(),
                            retry_hint: wire::RetryHint::Never,
                            effect: Some(effect),
                            reconciliation: Some(wire::ReconciliationKey::Event {
                                event_id: params.event_id.clone(),
                            }),
                            detail: "fixture".into(),
                        },
                    ),
                    None => {
                        let address = match &params.destination {
                            wire::Destination::Address { address } => address.clone(),
                            _ => panic!("direct address"),
                        };
                        wire::Response::ok(
                            request.request_id.clone(),
                            wire::OperationResult::Publish(wire::PublishResult {
                                receipt: wire::EventReceipt {
                                    producer: "fixture".into(),
                                    event_id: params.event_id.clone(),
                                    intent_hash: wire::publication_intent_fingerprint(params)
                                        .unwrap(),
                                    envelope_hash: "a".repeat(64),
                                    accepted_at_ms: 1,
                                    acceptance_seq: 1,
                                    destination: wire::FrozenDestination {
                                        address,
                                        route_key: None,
                                        caller_scope: None,
                                        destination_scope: None,
                                    },
                                },
                                replayed: false,
                            }),
                        )
                    }
                };
                socket
                    .write_all(&wire::encode_response(&response).unwrap())
                    .unwrap();
                socket.write_all(b"\n").unwrap();
            });

            assert_eq!(
                handle
                    .queue_lost(&owner, &[format!("ide.read src/{index}.rs")])
                    .await,
                expected
            );
            server.join().unwrap();
        }
    }

    /// A useful graph body is delivered without an inspect detour; changed source state yields
    /// an explicit terminal superseded event with no outdated graph content.
    #[test]
    fn direct_graph_completion_is_useful_and_source_versioned() {
        let spec = DeferSpec {
            tool: AssistanceTool::Graph,
            target: "src/a.rs#f".into(),
            fence_paths: Vec::new(),
        };
        let reply = PeerReply::Complete {
            kind: ResultKind::Graph,
            text: "graph: f\n  caller: g".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        let epoch = JobEpoch {
            native_epoch: 2,
            edit_seq: 3,
        };
        let fact = bridge_job_fact("graph-ref", &reply, &spec, epoch, epoch);
        assert!(fact.detail.as_deref().unwrap().contains("caller: g"));
        assert!(fact.detail.as_deref().unwrap().contains("source epoch 2/3"));
        assert!(fact.validate().is_ok());
        let stale = bridge_job_fact(
            "graph-ref",
            &reply,
            &spec,
            epoch,
            JobEpoch {
                edit_seq: 4,
                ..epoch
            },
        );
        assert!(stale.failed);
        assert!(stale.summary.starts_with("superseded"));
        assert!(!stale.detail.as_deref().unwrap().contains("caller: g"));
        assert!(stale.validate().is_ok());
    }

    /// A run whose caller left with a started line hands its work over exactly once when it
    /// settles, and nothing but an explicit read collects it for `ide.stop`: no carrier marks it
    /// observed.
    #[tokio::test]
    async fn a_started_run_is_handed_over_once_and_stays_uncollected() {
        crate::lang::testing::install();
        let gamma = crate::lang::Language::by_id("gamma").expect("test language");
        let root = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("m026-outbox-run-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let subscriber = bind(&bindings, "subscriber");
        let handle = handle(bindings);
        let shared = &*handle.shared;
        shared.outbox.open(&owner);
        shared.outbox.open(&subscriber);
        let id = match shared.test_runs.start(
            root.clone(),
            vec!["/bin/echo".into(), "done".into()],
            gamma,
            Duration::from_secs(30),
            "run-detail".into(),
            &owner,
        ) {
            StartResult::Started(id) => id,
            StartResult::Running(..) | StartResult::Failed { .. } => panic!("run must start"),
        };
        shared.test_started(id, &owner);
        shared.test_subscribed(id, &subscriber);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while shared.test_runs.settled_result(id).is_none() {
            assert!(tokio::time::Instant::now() < deadline, "the run settles");
            tokio::task::yield_now().await;
        }
        shared.test_settled(id);
        assert!(
            shared.outbox.take_work(&test_subject(id)).is_none(),
            "settlement consumed the in-flight work"
        );
        assert!(
            shared
                .test_runs
                .uncollected(&owner.fingerprint())
                .iter()
                .any(|(run, _, _)| *run == id),
            "no carrier collects a run"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// Actual direct handoff preserves subscriber/error facts without source authority, while
    /// an unproved source body is replaced before SDK I/O; no historical fallback receives it.
    #[tokio::test]
    async fn direct_push_preserves_lifecycle_errors_and_fences_unproved_source() {
        use crate::{
            app::store::Store,
            assistance::bridge::{BridgeConfig, BridgeDelivery, NativeStore},
        };
        use bridge_sdk::bridge_protocol as wire;
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::{fs::PermissionsExt, net::UnixListener},
        };
        let scratch = crate::scratch::ScratchDir::new("direct-push-regression");
        let config = BridgeConfig {
            registration_socket: scratch.join("adapter.sock"),
            core_socket: scratch.join("core.sock"),
            credential_file: scratch.join("token"),
            native_stores: vec![NativeStore {
                harness: wire::Host::Codex,
                host: "local".into(),
                store: "fixture".into(),
            }],
        };
        std::fs::write(&config.credential_file, "fixture-token").unwrap();
        std::fs::set_permissions(
            &config.credential_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let store = Arc::new(
            Store::open_with_backup_root(
                &scratch.join("state.sqlite"),
                &scratch.join("backups"),
                crate::app::config::EffectiveConfig::defaults().store(),
            )
            .unwrap(),
        );
        let bridge = BridgeDelivery::open(&config, store, [7; 32]).await.unwrap();
        let launcher=LauncherConfig::parse(&serde_json::to_vec(&serde_json::json!({"version":1,"limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},"targets":[],"bridge":{"registration_socket":config.registration_socket,"core_socket":config.core_socket,"credential_file":config.credential_file,"native_stores":[{"harness":"codex","host":"local","store":"fixture"}]}})).unwrap()).unwrap();
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = WorkerHandle::new(
            bindings,
            launcher,
            [9; 32],
            Arc::new(Mutex::new(admission_controller())),
        );
        *handle.shared.bridge.lock().unwrap() = Some(bridge);
        let listener = UnixListener::bind(&config.core_socket).unwrap();
        let (sent, received) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut frame = String::new();
                BufReader::new(&socket).read_line(&mut frame).unwrap();
                let request = wire::decode_request(frame.trim_end().as_bytes()).unwrap();
                let wire::Operation::Publish(params) = &request.operation else {
                    panic!("publish only")
                };
                bodies.push(params.payload.as_str().to_owned());
                let response = wire::Response::error(
                    request.request_id,
                    wire::ApiError {
                        class: wire::ErrorClass::Capacity,
                        reason: "fixture".into(),
                        retry_hint: wire::RetryHint::Never,
                        effect: Some(wire::MutationEffect::NotCommitted),
                        reconciliation: None,
                        detail: "fixture".into(),
                    },
                );
                socket
                    .write_all(&wire::encode_response(&response).unwrap())
                    .unwrap();
                socket.write_all(b"\n").unwrap();
            }
            sent.send(bodies).unwrap();
        });
        for (kind, subject, body, source_bearing) in [
            (
                FactKind::TestFinished,
                "tests #1",
                "subscriber rerun detail",
                false,
            ),
            (
                FactKind::JobReady,
                "failed job",
                "Repair: useful original error",
                false,
            ),
            (
                FactKind::JobReady,
                "source job",
                "SOURCE_MUST_NOT_LEAK",
                true,
            ),
            (
                FactKind::EditChecked,
                "clean check",
                "OLD_CLEAN_MUST_NOT_LEAK",
                false,
            ),
        ] {
            assert!(handle.shared.push_or_transfer(
                std::slice::from_ref(&owner),
                FactDraft {
                    kind,
                    subject: subject.into(),
                    summary: "original result".into(),
                    detail: Some(body.into()),
                    detail_ref: None,
                    failed: false,
                    source_bearing,
                    fence_paths: vec![],
                },
            ));
        }
        let bodies = tokio::time::timeout(Duration::from_secs(5), received)
            .await
            .unwrap()
            .unwrap();
        server.join().unwrap();
        assert!(
            bodies
                .iter()
                .any(|body| body.contains("subscriber rerun detail")),
            "{bodies:?}"
        );
        assert!(
            bodies
                .iter()
                .any(|body| body.contains("Repair: useful original error")),
            "{bodies:?}"
        );
        assert!(
            bodies.iter().any(|body| body.contains("superseded")),
            "{bodies:?}"
        );
        assert!(
            !bodies
                .iter()
                .any(|body| body.contains("SOURCE_MUST_NOT_LEAK"))
        );
        assert!(
            !bodies
                .iter()
                .any(|body| body.contains("OLD_CLEAN_MUST_NOT_LEAK"))
        );
    }

    /// A compaction raises the compaction counter of the session's channel (OUTBOX §9.1).
    #[test]
    fn compaction_raises_the_context_epoch() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        handle.shared.outbox.open(&owner);
        assert_eq!(SessionOutbox::context_epoch(&*handle.shared, &owner), None);
        handle.note_compaction(&owner);
        assert_eq!(
            SessionOutbox::context_epoch(&*handle.shared, &owner),
            Some(1)
        );
        handle.note_compaction(&owner);
        assert_eq!(
            SessionOutbox::context_epoch(&*handle.shared, &owner),
            Some(2)
        );
    }

    /// An edit check that found new diagnostics is not source-bearing, so its `new:` lines reach
    /// Bridge with the fact, never only as counts.
    #[test]
    fn a_failed_edit_check_keeps_its_new_lines() {
        let draft = edit_checked_fact(
            "edit-ref",
            &EditResult {
                operation_id: "op-7".to_owned(),
                path: "src/a.rs".to_owned(),
                outcome: crate::changes::edit::EditOutcome::Replaced,
                source_ref: None,
            },
            &EditDiagnostics::CurrentReported {
                messages: vec!["expected `;`, found `}`".to_owned()],
                delta: "1 new error".to_owned(),
                truncated: false,
            },
        );
        assert!(draft.failed && !draft.source_bearing);
        assert_eq!(draft.validate(), Ok(()));
        assert!(
            draft
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("  new: src/a.rs: expected `;`, found `}`"))
        );
    }

    /// Review fix: a job that settles in the instant after `defer` released the ledger takes the
    /// registration itself, so nothing stays deferred for work that already finished.
    #[tokio::test]
    async fn a_job_settling_right_after_its_defer_leaves_nothing_deferred() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = handle.shared.clone();
        shared.outbox.open(&owner);
        plant(&handle, "r-9", &owner, pending("r-9"));
        let settle = shared.clone();
        tests::DEFER_GAP.with(|gap| {
            *gap.borrow_mut() = Some(Box::new(move || {
                settle.complete("r-9", answer(), None, None, 0);
            }));
        });
        assert!(SessionOutbox::defer(&*shared, "r-9", read_spec()));
        tests::DEFER_GAP.with(|gap| *gap.borrow_mut() = None);
        assert!(!shared.ledger.lock().unwrap().deferred.contains_key("r-9"));
    }

    /// Review fix (OUTBOX §3.1): a deferred job's fact is fenced at the state the job started
    /// from, not at the later defer, so an edit made while it ran is seen as a changed epoch.
    #[tokio::test]
    async fn a_deferred_fact_is_fenced_at_the_job_start() {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let owner = bind(&bindings, "owner");
        let handle = handle(bindings);
        let shared = &*handle.shared;
        shared.outbox.open(&owner);
        plant(&handle, "r-5", &owner, pending("r-5"));
        shared.note_job_started("r-5", &owner);
        shared.outbox.note_edit(&owner);
        assert!(SessionOutbox::defer(shared, "r-5", read_spec()));
        let captured = shared.ledger.lock().unwrap().deferred["r-5"].1;
        assert_eq!(captured.edit_seq, 0);
        assert_eq!(shared.outbox.current_epoch(&owner).edit_seq, 1);
    }
}
