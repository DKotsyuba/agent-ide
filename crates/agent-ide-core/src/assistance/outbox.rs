//! Completion-fact vocabulary and the producer seam of Bridge-only delivery.
//!
//! Daemon producers describe the state changes the requesting call did not receive in its reply
//! (a finished test run, an edit's project check, the final answer of a job that went `pending`)
//! as *facts* and hand them through the [`SessionOutbox`](crate::assistance::outbox::SessionOutbox)
//! trait. The worker's implementation transfers each fact to Bridge, the only delivery owner;
//! nothing here queues a fact for a hook, a Stop hold, a wake message or an `ide.inspect` carrier.
//!
//! This file holds the shared vocabulary only: the draft and fact types, their byte budgets and
//! validation, the trait, and the test-only `RecordingOutbox` that consumers use in boundary
//! tests. It performs no I/O and holds no lock across an await.

#[cfg(test)]
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Mutex;

use super::facade::AssistanceTool;
use super::host_binding::BindingRef;

/// Longest fact subject, in UTF-8 bytes (OUTBOX §3.1).
pub const MAX_SUBJECT_BYTES: usize = 128;
/// Longest one-line fact summary, in UTF-8 bytes (OUTBOX §3.3).
pub const MAX_SUMMARY_BYTES: usize = 256;
/// Longest fact text: summary plus detail, in UTF-8 bytes (OUTBOX §3.3).
pub const MAX_FACT_BYTES: usize = 2048;
/// Longest compact `test-finished` body, retaining every bounded target outcome below 4 KiB.
pub const MAX_TEST_FACT_BYTES: usize = 3584;
/// Largest self-contained source result handed to Bridge; remains below wire/native limits.
pub const MAX_SOURCE_FACT_BYTES: usize = 32 * 1024;
/// Most fenced paths one fact or deferred job may name (OUTBOX §3.1).
pub const MAX_FENCE_PATHS: usize = 16;
/// Longest `defer` target, in UTF-8 bytes; the subject is `"<tool> <target>"` (OUTBOX §3.1).
pub const MAX_TARGET_BYTES: usize = 120;
/// Longest detail reference a fact may carry, in UTF-8 bytes (the `ide.inspect` bound).
pub const MAX_DETAIL_REF_BYTES: usize = 128;
/// Longest wait one `ide.inspect` may ask for, in milliseconds; larger values are clamped
/// (OUTBOX §8.5). It stays under the Codex 300 s default tool timeout.
pub const MAX_WAIT_MS: u64 = 240_000;
/// Most references one `ide.inspect {"detail_refs": …}` call names (OUTBOX §8.5).
pub const MAX_DETAIL_REFS: usize = 8;

/// The closed set of fact kinds (OUTBOX §2). Adding a kind is a contract revision.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum FactKind {
    /// A settled test run; text from M-024's `terminal_fact`, subject `tests #N`.
    TestFinished,
    /// The project check of one edit operation, subject `edit <operation_id>`.
    EditChecked,
    /// The final reply or failure of a deferred job, subject `<tool> <target>`.
    JobReady,
    /// One notice after a daemon replacement that earlier requested work may be lost.
    LostAfterRestart,
}

impl FactKind {
    /// Returns the fixed label rendered at the start of a fact line (`test-finished`, …).
    pub const fn label(self) -> &'static str {
        match self {
            FactKind::TestFinished => "test-finished",
            FactKind::EditChecked => "edit-checked",
            FactKind::JobReady => "job-ready",
            FactKind::LostAfterRestart => "lost-after-restart",
        }
    }
}

/// One path a fact depends on, with the content digest the producer observed, if any.
///
/// `path` is worktree-relative. A `digest` lets the source-access re-proof compare the
/// current bytes (OUTBOX §7); without one, staleness falls back to the session's `ide.edit`
/// counter and native epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FencedPath {
    /// Worktree-relative path; at most [`MAX_SUBJECT_BYTES`] bytes, no control characters.
    pub path: String,
    /// BLAKE3 digest of the bytes the result was built from, when the producer knows it.
    pub digest: Option<[u8; 32]>,
}

/// The session state a job started from, captured by the worker when the job began executing.
///
/// Facts compare it with the session's current state at settlement to decide staleness
/// (OUTBOX §7). Producers never build it from a later observation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct JobEpoch {
    /// The session's native-hook epoch (`Job.native_epoch`) when the job started.
    pub native_epoch: u64,
    /// The session's count of settled `ide.edit` operations when the job started.
    pub edit_seq: u64,
}

/// Producer input for one fact; Bridge owns its sequencing, delivery and state.
///
/// Every text field is model-visible data: no imperative sentences, no `poll`, no elapsed
/// seconds of unfinished work (OUTBOX §3.3, HYGIENE-r1 §2). Untrusted runner text must already
/// be stripped of control characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FactDraft {
    /// Closed fact kind.
    pub kind: FactKind,
    /// Coalescing subject (`tests #3`, `edit op-7`, `ide.symbol src/a.rs#Foo`): one line,
    /// nonempty, at most [`MAX_SUBJECT_BYTES`] bytes.
    pub subject: String,
    /// One-line outcome in plain words, nonempty, at most [`MAX_SUMMARY_BYTES`] bytes.
    pub summary: String,
    /// Optional further lines, rendered verbatim; summary plus detail stays within
    /// [`MAX_FACT_BYTES`] ([`MAX_TEST_FACT_BYTES`] for [`FactKind::TestFinished`]).
    pub detail: Option<String>,
    /// Reference that re-reads the full output or answer with `ide.inspect`, if retained.
    pub detail_ref: Option<String>,
    /// True for a failure (failed tests, new errors, a failed job); failures are ordered first.
    pub failed: bool,
    /// True when `detail` carries source-derived text; such text is re-proved against its source
    /// access and freshness right before it is handed to Bridge.
    pub source_bearing: bool,
    /// Paths the result depends on, at most [`MAX_FENCE_PATHS`]; empty means the whole worktree.
    pub fence_paths: Vec<FencedPath>,
}

/// Why a [`FactDraft`] or [`DeferSpec`] was refused by validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraftError {
    /// The subject or target is empty, multi-line, has control characters or exceeds its bound.
    Subject,
    /// The summary is empty, multi-line, has control characters or exceeds its bound.
    Summary,
    /// Summary plus detail exceeds the kind's byte budget, or detail has control characters
    /// other than newlines.
    Detail,
    /// The detail reference is empty, has control characters or exceeds its bound.
    DetailRef,
    /// More fenced paths than [`MAX_FENCE_PATHS`], or a path that is empty or not one line.
    Fence,
}

/// Returns whether `text` is one nonempty line of at most `limit` bytes without control characters.
fn one_line(text: &str, limit: usize) -> bool {
    !text.trim().is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
}

/// Checks a fence list against the path count and per-path line rules.
fn valid_fence(fence: &[FencedPath]) -> bool {
    fence.len() <= MAX_FENCE_PATHS
        && fence
            .iter()
            .all(|fenced| one_line(&fenced.path, MAX_SUBJECT_BYTES))
}

impl FactDraft {
    /// Checks every byte budget and line rule of OUTBOX §3; the real outbox and the recorder
    /// refuse a draft that fails it.
    ///
    /// Returns the first violated rule. Detail may span lines but carries no other control
    /// characters; the kind decides the summary-plus-detail budget.
    pub fn validate(&self) -> Result<(), DraftError> {
        if !one_line(&self.subject, MAX_SUBJECT_BYTES) {
            return Err(DraftError::Subject);
        }
        if !one_line(&self.summary, MAX_SUMMARY_BYTES) {
            return Err(DraftError::Summary);
        }
        let budget = match self.kind {
            FactKind::TestFinished => MAX_TEST_FACT_BYTES,
            FactKind::JobReady if self.source_bearing => MAX_SOURCE_FACT_BYTES,
            _ => MAX_FACT_BYTES,
        };
        let detail = self.detail.as_deref().unwrap_or("");
        if self.summary.len() + detail.len() > budget
            || detail
                .chars()
                .any(|character| character.is_control() && character != '\n')
        {
            return Err(DraftError::Detail);
        }
        if self
            .detail_ref
            .as_deref()
            .is_some_and(|reference| !one_line(reference, MAX_DETAIL_REF_BYTES))
        {
            return Err(DraftError::DetailRef);
        }
        if !valid_fence(&self.fence_paths) {
            return Err(DraftError::Fence);
        }
        Ok(())
    }
}

/// Registration that turns a job's eventual final reply into one `job-ready` fact (OUTBOX §3.2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeferSpec {
    /// The tool whose job is deferred; it names the subject.
    pub tool: AssistanceTool,
    /// Target of the call (`src/a.rs#Foo`, `batch <ref>: items 1,3`): one line, nonempty, at
    /// most [`MAX_TARGET_BYTES`] bytes. The subject is `"<tool> <target>"`.
    pub target: String,
    /// Paths the answer depends on, at most [`MAX_FENCE_PATHS`]; M-025 names only definition
    /// files so usage-file edits never discard its answer.
    pub fence_paths: Vec<FencedPath>,
}

impl DeferSpec {
    /// Checks the target and fence rules; the real outbox and the recorder refuse a spec that
    /// fails it.
    pub fn validate(&self) -> Result<(), DraftError> {
        if !one_line(&self.target, MAX_TARGET_BYTES) {
            return Err(DraftError::Subject);
        }
        if !valid_fence(&self.fence_paths) {
            return Err(DraftError::Fence);
        }
        Ok(())
    }

    /// Returns the fact subject this deferred job produces: `"<mcp name> <target>"`.
    pub fn subject(&self) -> String {
        format!("{} {}", self.tool.mcp_name(), self.target)
    }
}

/// The only in-daemon seam between fact producers and Bridge delivery (OUTBOX §3.1).
///
/// Implementations are shared across tasks (`Send + Sync`), never block on I/O, and never hold
/// a lock across an await. All methods are fail-open: a refusal returns `false`/`None` and never
/// panics or affects the caller's own reply. A fact is handed to Bridge at most once; with no
/// Bridge route the result stays retained under its reference and nothing else delivers it.
pub trait SessionOutbox: Send + Sync {
    /// Hands one fact to Bridge for each recipient session.
    ///
    /// `epoch` is the state the producing job started from. Returns `true` only when at least one
    /// recipient accepted the local handoff; this never means Bridge published it or the model
    /// observed it. Invalid drafts, empty recipients and an unavailable delivery owner return
    /// `false`.
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool;

    /// Makes the final reply (or final failure) of the job behind `reference` one `job-ready`
    /// fact for the job's owning session when the job settles.
    ///
    /// Idempotent per reference: a second call returns `false` and changes nothing. Returns
    /// `false` as well for an invalid spec or an unknown or already settled-and-read reference.
    fn defer(&self, reference: &str, spec: DeferSpec) -> bool;

    /// Drops every deferral and in-flight registration of `binding` silently.
    ///
    /// Called on displacement (`release_binding_state`) and by `ide.stop`.
    fn forget(&self, binding: &BindingRef);

    /// Returns the session's compaction counter (OUTBOX §9.1), or `None` when its channel never
    /// delivered a native hook and compaction therefore cannot be observed.
    fn context_epoch(&self, binding: &BindingRef) -> Option<u64>;
}

/// One call observed by [`RecordingOutbox`], in call order.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutboxCall {
    /// A [`SessionOutbox::push`] with its result.
    Push {
        /// Recipients as passed.
        recipients: Vec<BindingRef>,
        /// Draft as passed.
        draft: FactDraft,
        /// Epoch as passed.
        epoch: JobEpoch,
        /// Value returned to the caller.
        accepted: bool,
    },
    /// A [`SessionOutbox::defer`] with its result.
    Defer {
        /// Reference as passed.
        reference: String,
        /// Spec as passed.
        spec: DeferSpec,
        /// Value returned to the caller.
        accepted: bool,
    },
    /// A [`SessionOutbox::forget`].
    Forget {
        /// Binding as passed.
        binding: BindingRef,
    },
}

/// Test-only [`SessionOutbox`] that records producer calls and applies the same validation.
///
/// It keeps no queue, so producer tests assert on [`RecordingOutbox::calls`]. `push` accepts a valid draft with at
/// least one recipient; `defer` accepts a valid spec once per reference; `context_epoch` returns
/// what [`RecordingOutbox::set_context_epoch`] stored.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingOutbox {
    /// Every producer call in order.
    calls: Mutex<Vec<OutboxCall>>,
    /// Compaction counters set by the test.
    epochs: Mutex<BTreeMap<BindingRef, u64>>,
}

#[cfg(test)]
impl RecordingOutbox {
    /// Returns a copy of every producer call recorded so far, in call order.
    pub fn calls(&self) -> Vec<OutboxCall> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }

    /// Returns only the accepted pushes, as `(recipients, draft)` pairs in call order.
    pub fn pushed(&self) -> Vec<(Vec<BindingRef>, FactDraft)> {
        self.calls()
            .into_iter()
            .filter_map(|call| match call {
                OutboxCall::Push {
                    recipients,
                    draft,
                    accepted: true,
                    ..
                } => Some((recipients, draft)),
                _ => None,
            })
            .collect()
    }

    /// Sets the compaction counter `context_epoch` reports for `binding`.
    pub fn set_context_epoch(&self, binding: &BindingRef, epoch: u64) {
        if let Ok(mut epochs) = self.epochs.lock() {
            epochs.insert(binding.clone(), epoch);
        }
    }

    /// Appends one call to the record.
    fn record(&self, call: OutboxCall) {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call);
        }
    }
}

#[cfg(test)]
impl SessionOutbox for RecordingOutbox {
    /// Records the push; accepts a valid draft with at least one recipient.
    fn push(&self, recipients: &[BindingRef], draft: FactDraft, epoch: JobEpoch) -> bool {
        let accepted = !recipients.is_empty() && draft.validate().is_ok();
        self.record(OutboxCall::Push {
            recipients: recipients.to_vec(),
            draft,
            epoch,
            accepted,
        });
        accepted
    }

    /// Records the deferral; accepts a valid spec the first time a reference is deferred.
    fn defer(&self, reference: &str, spec: DeferSpec) -> bool {
        let repeated = self.calls().iter().any(|call| {
            matches!(call, OutboxCall::Defer { reference: earlier, accepted: true, .. } if earlier == reference)
        });
        let accepted = !repeated && spec.validate().is_ok();
        self.record(OutboxCall::Defer {
            reference: reference.to_owned(),
            spec,
            accepted,
        });
        accepted
    }

    /// Records the forget.
    fn forget(&self, binding: &BindingRef) {
        self.record(OutboxCall::Forget {
            binding: binding.clone(),
        });
    }

    /// Returns the counter set by the test, or `None`.
    fn context_epoch(&self, binding: &BindingRef) -> Option<u64> {
        self.epochs.lock().ok()?.get(binding).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid test-finished draft shaped like M-024's `terminal_fact` mapping.
    fn test_draft() -> FactDraft {
        FactDraft {
            kind: FactKind::TestFinished,
            subject: "tests #3".to_owned(),
            summary: "41 passed, 2 failed, 12 s".to_owned(),
            detail: Some("  FAIL a::b\n  rerun: cargo test a::b".to_owned()),
            detail_ref: Some("r-12".to_owned()),
            failed: true,
            source_bearing: false,
            fence_paths: Vec::new(),
        }
    }

    /// A binding fixture for recipient lists.
    fn binding(actor: &str) -> BindingRef {
        BindingRef::fixture(actor, "channel-a", 1)
    }

    /// Validation enforces each OUTBOX §3 budget, including the 3584-byte test-fact limit.
    #[test]
    fn drafts_are_validated_against_the_contract_budgets() {
        assert_eq!(test_draft().validate(), Ok(()));
        let mut draft = test_draft();
        draft.subject = "two\nlines".to_owned();
        assert_eq!(draft.validate(), Err(DraftError::Subject));
        let mut draft = test_draft();
        draft.summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
        assert_eq!(draft.validate(), Err(DraftError::Summary));
        let mut draft = test_draft();
        draft.detail = Some("y".repeat(MAX_TEST_FACT_BYTES));
        assert_eq!(draft.validate(), Err(DraftError::Detail));
        draft.detail = Some("y".repeat(MAX_TEST_FACT_BYTES - draft.summary.len()));
        assert_eq!(draft.validate(), Ok(()));
        draft.kind = FactKind::JobReady;
        assert_eq!(draft.validate(), Err(DraftError::Detail));
        draft.detail = Some("y".repeat(MAX_FACT_BYTES - draft.summary.len()));
        assert_eq!(
            draft.validate(),
            Ok(()),
            "job-ready keeps the 2048-byte budget"
        );
        let mut draft = test_draft();
        draft.detail = Some("bell\u{7}".to_owned());
        assert_eq!(draft.validate(), Err(DraftError::Detail));
        let mut draft = test_draft();
        draft.detail_ref = Some(String::new());
        assert_eq!(draft.validate(), Err(DraftError::DetailRef));
        let mut draft = test_draft();
        draft.fence_paths = (0..=MAX_FENCE_PATHS)
            .map(|index| FencedPath {
                path: format!("src/{index}.rs"),
                digest: None,
            })
            .collect();
        assert_eq!(draft.validate(), Err(DraftError::Fence));
    }

    /// The recorder accepts valid pushes with recipients and records refusals too.
    #[test]
    fn recorder_records_pushes_and_refuses_invalid_ones() {
        let outbox = RecordingOutbox::default();
        assert!(outbox.push(&[binding("a")], test_draft(), JobEpoch::default()));
        assert!(!outbox.push(&[], test_draft(), JobEpoch::default()));
        let mut invalid = test_draft();
        invalid.summary.clear();
        assert!(!outbox.push(&[binding("a")], invalid, JobEpoch::default()));
        assert_eq!(outbox.calls().len(), 3);
        assert_eq!(outbox.pushed(), vec![(vec![binding("a")], test_draft())]);
    }

    /// `defer` is idempotent per reference and validates its spec.
    #[test]
    fn recorder_defer_is_idempotent_per_reference() {
        let outbox = RecordingOutbox::default();
        let spec = DeferSpec {
            tool: AssistanceTool::Symbol,
            target: "src/a.rs#Foo".to_owned(),
            fence_paths: vec![FencedPath {
                path: "src/a.rs".to_owned(),
                digest: Some([7; 32]),
            }],
        };
        assert_eq!(spec.subject(), "ide.symbol src/a.rs#Foo");
        assert!(outbox.defer("r-1", spec.clone()));
        assert!(
            !outbox.defer("r-1", spec.clone()),
            "second defer is a no-op"
        );
        assert!(outbox.defer("r-2", spec.clone()));
        let mut invalid = spec;
        invalid.target = String::new();
        assert!(!outbox.defer("r-3", invalid));
    }

    /// The recorder reports test-set compaction counters and keeps no queue.
    #[test]
    fn recorder_reports_context_epoch_and_records_forget() {
        let outbox = RecordingOutbox::default();
        let session = binding("a");
        assert_eq!(outbox.context_epoch(&session), None);
        outbox.set_context_epoch(&session, 2);
        assert_eq!(outbox.context_epoch(&session), Some(2));
        outbox.forget(&session);
        assert_eq!(
            outbox.calls(),
            vec![OutboxCall::Forget { binding: session }]
        );
    }

    /// Fact kinds keep their fixed rendered labels.
    #[test]
    fn fact_kinds_keep_their_labels() {
        assert_eq!(FactKind::TestFinished.label(), "test-finished");
    }
}
