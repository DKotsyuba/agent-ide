//! One daemon-owned worker with bounded jobs/details, durable authorization and revocable work.

use super::{
    content,
    facade::{AssistanceTool, FeedbackDelta, render_reply},
    host_binding::{ActiveBindingUse, BindingRef, HostBindingGuard, ValidatedInvocation},
    launcher::{LaunchTarget, LauncherConfig},
    problems::{ProblemSource, ProjectProblemFeed, parse_language, problems_text_with_rechecks},
    reply::{EditDiagnostics, ExecutionProfileCause, FailureCode, PeerReply, ResultKind},
    tests::{StartResult, TestRuns},
};
use crate::telemetry::{
    AdmissionState, CancellationState, DescendantSettlement, OutputSizeClass, Telemetry,
    TelemetryConfig, adapters,
};
use crate::workspace::observation::SourceObservation;
use crate::{
    app::{
        config::EffectiveConfig,
        store::{MigrationAdmission, OperationId, Store},
    },
    changes::edit::{
        EditOutcome as ChangesEditOutcome, EditReceiptStore, EditRequest, EditResult,
        PrepareAdmission,
    },
    workspace::{
        authority::{AuthorityStamp, StopBindingHandoff},
        durable::{DurableWorkspace, StartReceipt},
        store::WorkspaceStore,
    },
};
#[path = "providers.rs"]
mod providers;
#[path = "snapshots.rs"]
pub(super) mod snapshots;
#[path = "symbols.rs"]
mod symbols;

use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// Time reserved after an edit diagnostic attempt for reaping, receipt settlement, and reply
/// delivery before the operation or foreground-helper ticket expires.
const EDIT_SETTLEMENT_RESERVE: Duration = Duration::from_secs(2);
/// Longest an edit reply waits for the project check it scheduled before answering `unknown`.
const EDIT_CHECK_WAIT: Duration = Duration::from_secs(90);

/// A bounded asynchronous operation whose identity never includes the transient MCP call ID.
struct Job {
    /// Same-binding opaque result key retained in the result ledger.
    reference: String,
    /// Validated host invocation for this exact operation.
    invocation: ValidatedInvocation,
    /// Closed current tool operation.
    tool: AssistanceTool,
    /// Closed validated model parameters, containing no target/profile/authority information.
    parameters: Value,
    /// Trusted attachment-selected immutable target.
    target: LaunchTarget,
    /// Absolute lifetime includes time spent queued.
    deadline: tokio::time::Instant,
    /// Revocation channel; cancellation is never interpreted as reap evidence.
    cancel: watch::Receiver<bool>,
    /// Stop may return its bounded exact revocation outcome directly after cleanup.
    stop_reply: Option<oneshot::Sender<PeerReply>>,
    /// Native lifecycle revision captured when execution begins, not when it was queued.
    native_epoch: u64,
    /// Closed failing-stage tag for the terminal error log (T27B); never repository paths or
    /// child output, only fixed tags such as `diff:deadline` or `diff:child_exit:cat-file`.
    failure_detail: Option<String>,
    /// `true` once the edit scheduled its project check itself (post-edit diagnostics), so the
    /// reply path must not schedule a second run that would shift the worktree's generation.
    check_scheduled: bool,
}

/// A retained outcome requiring exact binding ownership and fresh durable authorization on access.
struct Detail {
    /// Immutable owner binding, checked separately from the opaque reference.
    binding: BindingRef,
    /// Current pending, error or owner-generated result.
    reply: PeerReply,
    /// Exact method/query selection, excluding only the opaque detail handle itself.
    selection: (AssistanceTool, [u8; 32]),
    /// Current Workspace stamp, required before any source/activation result can be delivered.
    authority: Option<AuthorityStamp>,
    /// Exact registered source facts rechecked before context delivery.
    source: Option<crate::workspace::observation::SourceObservation>,
    /// Native lifecycle revision associated with this result.
    native_epoch: u64,
    /// Retained bounded Changes state for the next Diff page; absent once fully delivered.
    /// Never serialized into a `PeerReply`; it exists only to resume the same owner cursor.
    diff_page: Option<snapshots::DiffPageState>,
    /// `true` exactly when `reply` holds a Diff page whose text was already composed (by the
    /// completed job or a prior expansion) but never yet handed to any caller. The first ready
    /// `ide.inspect` on such a detail must return that already-composed page unchanged instead of
    /// eagerly expanding past it; every later call then advances.
    diff_page_fresh: bool,
    /// Retained complete Context text for the next chunk; absent once fully delivered. Mirrors
    /// `diff_page` (T09B): never serialized into a `PeerReply`, exists only to resume the same
    /// owner cursor without a second Claude foreground-helper run or daemon source re-read.
    context_page: Option<ContextPageState>,
    /// Same first-page semantics as `diff_page_fresh`, for `context_page`.
    context_page_fresh: bool,
    /// Bounded paths a completed managed Diff or semantic Context detail represents: diff paths
    /// and rename sources, or definition/reference paths. Cached delivery proves every path
    /// under the live profile before releasing any composed page; missing diff provenance never
    /// falls through to an empty-path success.
    diff_provenance: Option<BTreeSet<PathBuf>>,
}

/// Retained bounded state needed to resume one Context or Claude-Diff detail cursor from
/// `ide.inspect` (T09B, extended to Diff by T13B).
///
/// Unlike [`snapshots::DiffPageState`], no Git re-selection is needed: the complete text was
/// already composed once by the job (managed read, settled Claude Context evidence, or a settled
/// Claude Diff capture), so later pages are pure byte slices of it. Never serialized into a
/// `PeerReply`.
#[derive(Clone)]
struct ContextPageState {
    /// Complete composed text for this result (fixed header plus full observed source or diff);
    /// every page is a line-bounded UTF-8 slice of this buffer, so pages always join byte-exactly.
    text: String,
    /// Exact byte offset into `text` already handed to a caller.
    delivered: usize,
    /// `true` when the upstream capture itself already lost bytes (Workspace's own source cap, a
    /// Diff selection budget's own overflow, or the Claude helper's output budget), independent of
    /// this pagination. Carried onto the final page's `truncated` field once every captured byte
    /// has been paged out, so completing pagination is never confused with having recovered bytes
    /// that were never captured.
    source_truncated: bool,
    /// Result kind rendered onto every page cut from `text`, so a Claude-captured Diff resumes as
    /// `ResultKind::Diff` and never masquerades as a Context result.
    kind: ResultKind,
    /// Offset in `text` where the counted body starts: the exact source bytes of a Context result
    /// (its fixed header precedes them), `0` for a Diff. Position markers count from here, so the
    /// total of a Context result equals the file's byte length (T16B).
    body_start: usize,
    /// One-based number of the page `delivered` is about to produce.
    page: usize,
}

impl ContextPageState {
    /// Starts paging `text` at its first page.
    fn new(text: String, body_start: usize, source_truncated: bool, kind: ResultKind) -> Self {
        Self {
            text,
            delivered: 0,
            source_truncated,
            kind,
            body_start,
            page: 1,
        }
    }

    /// Cuts the next line-bounded, byte-exact chunk that provably fits the bounded reply envelope
    /// (T09B), returning its [`PeerReply`] and the state for the following page, `None` once the
    /// last page was cut. `self` is never mutated, so repeated chunks join byte-exactly back into
    /// `text`.
    ///
    /// Mirrors `snapshots::fit_diff_page`'s fitting discipline: the same [`content::fits`]
    /// predicate that gates the real final MCP envelope decides acceptance, so a chunk is never
    /// handed out only to have the facade re-cut it later. Starts by trying the complete remainder
    /// as the final chunk, then halves the candidate length, snapping each candidate back to the
    /// preceding newline so no line is split across two pages, until one fits.
    ///
    /// Every page of a multi-page result starts with one position marker line, `page N; bytes
    /// A-B of TOTAL`, the last one `page N (last); ...; complete` (`incomplete: capture truncated`
    /// when the upstream capture itself lost bytes). A Diff page that starts inside a file's hunks
    /// is prefixed with `file: <path> (continued)` so every hunk stays attributable (T16B). A
    /// result that fits one page carries neither.
    ///
    /// # Errors
    ///
    /// [`FailureCode::Capacity`] when even one byte cannot fit the serialized envelope.
    fn next(&self, reference: &str) -> Result<(PeerReply, Option<Self>), FailureCode> {
        let remaining = &self.text[self.delivered..];
        let total = self.text.len() - self.body_start;
        let continued_file = (self.kind == ResultKind::Diff && self.delivered > 0)
            .then(|| {
                self.text[..self.delivered]
                    .lines()
                    .rev()
                    .find(|line| line.starts_with("file: "))
            })
            .flatten();
        let mut len = remaining.len();
        loop {
            let mut cut = len.min(remaining.len());
            while !remaining.is_char_boundary(cut) {
                cut -= 1;
            }
            let more_after = cut < remaining.len();
            let snapped = if more_after {
                remaining[..cut].rfind('\n').map_or(cut, |index| index + 1)
            } else {
                cut
            };
            let continuation = snapped < remaining.len();
            let mut text = String::new();
            if self.delivered > 0 || continuation {
                let start = self.delivered.saturating_sub(self.body_start);
                let end = (self.delivered + snapped).saturating_sub(self.body_start);
                let (last, status) = match (continuation, self.source_truncated) {
                    (true, _) => ("", ""),
                    (false, false) => (" (last)", "; complete"),
                    (false, true) => (" (last)", "; incomplete: capture truncated"),
                };
                text.push_str(&format!(
                    "page {}{last}; bytes {start}-{end} of {total}{status}\n",
                    self.page
                ));
                if let Some(line) = continued_file
                    && !remaining.starts_with("file: ")
                {
                    text.push_str(line);
                    text.push_str(" (continued)\n");
                }
            }
            text.push_str(&remaining[..snapped]);
            let reply = PeerReply::Complete {
                kind: self.kind,
                text,
                detail_ref: Some(reference.to_owned()),
                truncated: continuation || self.source_truncated,
                continuation,
            };
            // Same host-unaware conservative sizing as `snapshots::fit_diff_page` (T14B): this
            // daemon path never learns which MCP host will receive the page, so it stays sized to
            // fit even alongside the structured JSON copy.
            if content::fits(&reply, content::Envelope::WithStructured) {
                let next = continuation.then(|| Self {
                    delivered: self.delivered + snapped,
                    page: self.page + 1,
                    ..self.clone()
                });
                return Ok((reply, next));
            }
            if snapped == 0 {
                return Err(FailureCode::Capacity);
            }
            len = snapped / 2;
        }
    }
}

/// Retains one versioned provider delta until a later native post-hook may deliver it.
struct NativeFeedback {
    /// Exact source observation for daemon-managed feedback; absent for a helper-owned snapshot.
    /// A source-backed fact is never delivered through the hook path: the hook boundary carries
    /// no host sandbox state, so the capture-time authority can never be re-proved against the
    /// *current* binding state and the freshness reread would be an unauthorized native read
    /// (T36B-r). Source-backed facts stay hook-suppressed; only their inline Context/Inspect
    /// delivery, which re-proves the source under the live profile, discloses them.
    source: Option<SourceObservation>,
    /// Bounded fact/evidence/action rendering; source text and diagnostics are excluded.
    text: String,
    /// Native lifecycle epoch at which the provider result was accepted.
    native_epoch: u64,
    /// `true` once this exact fact was actually handed to a live caller inside a submitted
    /// Context/Inspect reply — never set merely because the owning job finished computing it.
    /// A completed job whose reply nobody has retrieved yet (still `Pending` to its caller, or
    /// lost to a deadline) leaves this `false`, so the fact remains eligible for exactly one
    /// later hook delivery. This never asserts the model read the text, only that this process
    /// handed it to the transport once; see `take_current_feedback`.
    inline_delivered: bool,
    /// Exact detail reference of the job that produced this fact.
    ///
    /// The per-binding slot is single-slot: a second Context job at the *same* native epoch can
    /// overwrite it with a different fact before any caller retrieves either one. Acknowledgement
    /// must name this exact producer, not just the epoch, so retrieving an older, already
    /// superseded detail can never mark the *current* (different) fact as delivered.
    producer: String,
    /// Bounded content identity of the underlying issue, used only to recognize a later,
    /// redundant Context job for the exact same unchanged issue; see `DeliveredIssue`.
    identity: DeliveredIssue,
}

/// Bounded content identity of one diagnostic issue, stable across repeated, unrelated
/// re-observation of unchanged bytes.
///
/// Deliberately excludes every volatile field that changes on mere re-observation without any
/// real change to the issue itself: the job's `detail_ref`, the source's per-read
/// `source_sequence`, and the native-hook epoch. It is never inferred by parsing rendered model
/// text; both producers build it from the same typed source digest and raw diagnostic messages
/// they already hold before rendering. Two identities are equal only when the observed source
/// path and content digest and the exact ordered diagnostic message set all match — any change to
/// source bytes, provider/document generation (which gates whether diagnostics attach at all), or
/// the issue set (added, removed or reworded messages, even at an unchanged count) yields a
/// different identity.
#[derive(Clone, Eq, PartialEq)]
struct DeliveredIssue {
    /// Exact registered path the issue was raised against.
    source_path: std::path::PathBuf,
    /// Content digest of the exact source bytes the issue was raised against.
    source_digest: [u8; 32],
    /// Content fingerprint of the exact ordered diagnostic message set.
    diagnostic_fingerprint: [u8; 32],
}

/// Builds the daemon's single physical-effect admission controller with its fixed process limits.
///
/// Exactly one of these exists per daemon boot. Every physical-effect route — discovery, snapshots,
/// language providers and Claude's foreground helper claims — draws from this one budget, so the
/// limits below are the real global ceiling and not a per-route hint.
pub(super) fn admission_controller() -> crate::execution::AdmissionController {
    crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
        total_running: 16,
        per_owner_running: 2,
        per_owner_queued: 1,
        total_queued: 64,
        interactive_burst: 8,
    })
    .expect("fixed process limits")
}

/// Shared bounded transport-side bookkeeping; no lock survives an I/O await.
struct Ledger {
    /// Jobs popped for execution and not yet settled; maintained in the same locked section that
    /// moves a job off `queue`, so an observer can never see both counts zero while a job runs.
    /// A queued or in-flight job defers idle shutdown (T26B) until it reaches its terminal state.
    in_flight: usize,
    /// FIFO ordinary jobs; explicit stop is prioritized at the front.
    queue: VecDeque<Job>,
    /// Retained results, never silently evicted to admit more work.
    details: BTreeMap<String, Detail>,
    /// Stable start requests under each immutable binding generation.
    starts: BTreeMap<(BindingRef, String), String>,
    /// One cancellation sender per currently active binding.
    cancellation: BTreeMap<BindingRef, watch::Sender<bool>>,
    /// Nonzero monotonic detail identifiers within this daemon boot.
    next: u64,
    /// One bounded monotonic invalidation counter per active binding.
    native_epoch: BTreeMap<BindingRef, u64>,
    /// At most one undelivered current delta per active binding.
    feedback: BTreeMap<BindingRef, NativeFeedback>,
    /// Identity of the single most recently delivered (inline-submitted or hook-consumed) issue
    /// per active binding, retained across replacement or removal of `feedback`'s pending slot so
    /// a later, redundant Context job for the exact same unchanged issue is never re-armed as a
    /// fresh undelivered fact. Cleared at the same points `feedback` is cleared.
    delivered: BTreeMap<BindingRef, DeliveredIssue>,
}
impl Default for Ledger {
    /// Creates empty finite bookkeeping; no file or process work occurs.
    fn default() -> Self {
        Self {
            in_flight: 0,
            queue: VecDeque::new(),
            details: BTreeMap::new(),
            starts: BTreeMap::new(),
            cancellation: BTreeMap::new(),
            next: 0,
            native_epoch: BTreeMap::new(),
            feedback: BTreeMap::new(),
            delivered: BTreeMap::new(),
        }
    }
}

impl Ledger {
    /// Retains one newly produced fact unless this binding already received the same issue.
    ///
    /// `feedback` is the current producer's bounded fact. An equal delivered identity removes any
    /// older pending slot and returns `false`; a new identity replaces that slot and returns
    /// `true`. This does not mark a fact delivered: only a successful inline send or hook
    /// consumption may update `delivered`.
    fn retain_feedback(&mut self, binding: &BindingRef, feedback: NativeFeedback) -> bool {
        if self.delivered.get(binding) == Some(&feedback.identity) {
            self.feedback.remove(binding);
            false
        } else {
            self.feedback.insert(binding.clone(), feedback);
            true
        }
    }
}

/// A short read request serviced by the same worker even while provider warmup is pending.
struct Inspection {
    /// Freshly host-validated caller binding.
    binding: BindingRef,
    /// Opaque result handle supplied by the model; it does not confer ownership.
    reference: String,
    /// Optional owner/path constraint for method-specific detail retrieval.
    expected: Option<(AssistanceTool, [u8; 32])>,
    /// Finite IPC caller waiting for the current authorized result.
    reply: oneshot::Sender<PeerReply>,
}

/// Cloneable state shared by ingress and the single worker task, never by independent worker loops.
struct Shared {
    /// Exact revocable host binding owner.
    bindings: Arc<Mutex<HostBindingGuard>>,
    /// Bounded queue, cancellation and detail ledger.
    ledger: Mutex<Ledger>,
    /// Wakes the worker for jobs, stops or native-change hints.
    notify: Notify,
    /// Immutable trusted restart configuration.
    launcher: LauncherConfig,
    /// Additional Claude targets admitted by live managed MCP leases for this repository.
    claude_targets: Mutex<BTreeMap<String, LaunchTarget>>,
    /// The original Claude target is retired once a later lease observes its directory vanished.
    retired_initial_claude_target: std::sync::atomic::AtomicBool,
    /// Boot-unique opaque prefix prevents detail/SQLite operation collisions after restart.
    nonce: [u8; 32],
    /// True after daemon shutdown fences admission and requests owned cleanup.
    shutting_down: std::sync::atomic::AtomicBool,
    /// First provider cleanup failure retained until the shutdown caller observes it.
    shutdown_failure: Mutex<Option<FailureCode>>,
    /// The daemon's single finite physical-effect admission owner.
    ///
    /// It is shared rather than owned by the worker because Claude's foreground helper claims its
    /// capacity from the private helper socket task, off the worker's own queue. One controller is
    /// the whole point: ordinary worker work and Claude helper children contend for exactly the
    /// same configured global budget, and no second counter can widen it.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Replaceable fail-open sink receiving only sanitized typed telemetry facts.
    telemetry: Arc<dyn super::telemetry::EditTelemetry>,
    /// Optional project-problem snapshot source behind the `ide.context` problems kind.
    ///
    /// Absent (the default) renders the honest `checks disabled` outcome. It is installed
    /// before startup through [`WorkerHandle::with_problem_source`] and only reports typed
    /// snapshots for the authorized worktree; it never runs a check or touches authority.
    problem_source: Option<Arc<dyn ProblemSource>>,
    /// Optional project problem feed receiving start/edit triggers and forgetting stopped bindings.
    ///
    /// Installed through [`WorkerHandle::with_project_feed`], which also makes it the
    /// [`Shared::problem_source`]. Absent when project checks are not configured.
    project_feed: Option<Arc<ProjectProblemFeed>>,
    /// Explicitly requested background test processes, retained until daemon shutdown.
    test_runs: TestRuns,
}
impl Shared {
    /// Acquires a new transient binding use at one exact admission/return boundary.
    fn active(&self, binding: &BindingRef) -> Result<ActiveBindingUse, FailureCode> {
        self.bindings
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .consume_active(binding)
            .map_err(|_| FailureCode::Cancelled)
    }
    /// Stores a bounded result after its owner has performed current liveness/authority checks.
    fn complete(
        &self,
        reference: &str,
        reply: PeerReply,
        authority: Option<AuthorityStamp>,
        source: Option<SourceObservation>,
        native_epoch: u64,
    ) {
        let reply = reply
            .encode()
            .and_then(|value| PeerReply::decode(value.as_str()))
            .unwrap_or(PeerReply::Error {
                code: FailureCode::Internal,
            });
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.reply = reply;
            detail.authority = authority;
            detail.source = source;
            detail.native_epoch = native_epoch;
        }
    }
    /// Marks `binding`'s retained feedback as already submitted to a live caller, but only when
    /// three things hold: the retained fact is still the exact one produced by `reference` (never
    /// a different, later fact that overwrote the same single-slot binding entry), `reply` is the
    /// exact value that was actually handed to a still-live receiver, and that fact's rendered
    /// text still survives the real, final MCP/IPC fitting (`facade::render_reply`) applied to
    /// `reply` — the same shrink/encode boundary the real transport response goes through, so a
    /// trimmed or capped carrier is never mislabeled as delivered.
    ///
    /// This is the sole write side of cross-channel dedup: callers must invoke it only after
    /// confirming the reply actually reached a live receiver, never merely because a job finished
    /// or a reply merely encodes to *some* fitted value. A producer mismatch, closed receiver, or
    /// a carrier whose fitting dropped the fact is a no-op — it never resurrects, replaces or
    /// fabricates a fact.
    fn mark_feedback_inline_delivered(
        &self,
        binding: &BindingRef,
        reference: &str,
        reply: &PeerReply,
    ) {
        // First pass: cheaply confirm there is still a same-producer entry worth checking, and
        // take the exact fact text under the lock, before doing any rendering work outside it.
        let fact = {
            let Ok(ledger) = self.ledger.lock() else {
                return;
            };
            let Some(feedback) = ledger.feedback.get(binding) else {
                return;
            };
            if feedback.producer != reference {
                return;
            }
            feedback.text.clone()
        };
        // Rendering runs the real shrink loop; keep it off the ledger lock so a large reply never
        // holds up unrelated bindings. This traces the typed structured value regardless of which
        // host actually receives the call, so it always renders with the structured envelope (T14B).
        let survives = render_reply(reply.clone(), content::Envelope::WithStructured)
            .structured_content
            .as_ref()
            .and_then(|value| value.get("text"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.contains(&fact));
        if !survives {
            return;
        }
        // Second pass: re-take the lock and recheck the exact same producer identity before
        // writing. Nothing awaited between the two locks, but another task (a concurrent hook
        // consuming this same entry, or a newer Context job overwriting the single slot) could
        // have mutated it meanwhile; a stale write here would either resurrect a fact the hook
        // path already consumed or wrongly stamp a different, newer fact as delivered.
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        let identity = {
            let Some(feedback) = ledger.feedback.get_mut(binding) else {
                return;
            };
            if feedback.producer != reference {
                return;
            }
            feedback.inline_delivered = true;
            feedback.identity.clone()
        };
        ledger.delivered.insert(binding.clone(), identity);
    }
    /// Retains or clears the bounded Diff pagination state for one same-binding detail reference.
    /// Clearing a page also disables continuation on its retained Diff reply, so a later inspect
    /// cannot reuse a continuation after permanent invalidation; only `serve_inspection` may
    /// advance or drop this state otherwise.
    /// A newly stashed page is marked "fresh" because its text (in `reply`) was already composed
    /// by the caller and not yet handed to any inspector. Refuses to retain a *new* page past the
    /// aggregate retained-evidence ceiling and reports that honestly through its `bool` result;
    /// it never evicts a page that is not being replaced by this exact call.
    fn set_diff_page(&self, reference: &str, page: Option<snapshots::DiffPageState>) -> bool {
        let Ok(mut ledger) = self.ledger.lock() else {
            return false;
        };
        if page.is_some() {
            let already_retaining = ledger
                .details
                .get(reference)
                .is_some_and(|detail| detail.diff_page.is_some());
            if !already_retaining {
                let retained = ledger
                    .details
                    .values()
                    .filter(|detail| detail.diff_page.is_some())
                    .count();
                if !admits_new_diff_page(retained) {
                    if let Some(detail) = ledger.details.get_mut(reference) {
                        detail.diff_page = None;
                        detail.diff_page_fresh = false;
                    }
                    return false;
                }
            }
        }
        let retained = page.is_some();
        if let Some(detail) = ledger.details.get_mut(reference) {
            detail.diff_page = page;
            detail.diff_page_fresh = retained;
            if !retained
                && let PeerReply::Complete {
                    kind: ResultKind::Diff,
                    continuation,
                    ..
                } = &mut detail.reply
            {
                *continuation = false;
            }
        }
        retained
    }
    /// Retains the bounded per-path provenance of one managed Diff or semantic Context.
    ///
    /// Stored once at capture time, independently of the disposable pagination state, so every
    /// later cached delivery of this detail — composed page or freshness reread — can prove
    /// each represented path under the live profile.
    fn set_diff_provenance(&self, reference: &str, provenance: BTreeSet<PathBuf>) {
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.diff_provenance = Some(provenance);
        }
    }

    /// Records that the caller already received the retained page-one reply, so the next
    /// `ide.inspect` advances instead of re-serving it (T16B).
    fn mark_context_page_delivered(&self, reference: &str) {
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.context_page_fresh = false;
        }
    }
    /// Retains or clears the bounded Context (or Claude-captured Diff, T13B) pagination state for
    /// one same-binding detail reference (T09B). Mirrors `set_diff_page`'s fresh-page and
    /// continuation-clearing semantics, minus its aggregate byte ceiling: this page's text is
    /// already bounded by the source, Diff selection, or Claude helper capture limits, so the
    /// existing `limits.details` count ledger alone is enough to bound retained memory here.
    fn set_context_page(&self, reference: &str, page: Option<ContextPageState>) {
        let Ok(mut ledger) = self.ledger.lock() else {
            return;
        };
        let retained = page.is_some();
        if let Some(detail) = ledger.details.get_mut(reference) {
            detail.context_page = page;
            detail.context_page_fresh = retained;
            if !retained
                && let PeerReply::Complete {
                    kind: ResultKind::Context | ResultKind::Diff,
                    continuation,
                    ..
                } = &mut detail.reply
            {
                *continuation = false;
            }
        }
    }
}

/// Worst-case bytes one retained Diff page's evidence can hold: Workspace bounds per-path source
/// content to `MAX_SNAPSHOT_TOTAL_BYTES` and raw patch bytes to `MAX_SNAPSHOT_PATCH_BYTES`
/// separately, so a single `GitSnapshot` can approach their sum. Charging this fixed worst case
/// per retained page (rather than walking every path/patch on each admission check) keeps the
/// aggregate bound cheap and exact-enough: real usage is always at or under this charge.
const DIFF_PAGE_RETAINED_BYTES: usize = crate::workspace::git::snapshot::MAX_SNAPSHOT_TOTAL_BYTES
    + crate::workspace::git::snapshot::MAX_SNAPSHOT_PATCH_BYTES;
/// Aggregate ceiling for concurrently retained Diff pagination evidence across every detail,
/// independent of and tighter than the unrelated `details` count limit; a client that never pages
/// through its Diff details cannot pin unbounded memory just by leaving many of them retained.
const MAX_RETAINED_DIFF_PAGE_BYTES: usize = 16 * DIFF_PAGE_RETAINED_BYTES;

/// Decides whether one *additional* Diff page may be retained beside `retained` already-charged
/// pages, charging each the fixed worst case `DIFF_PAGE_RETAINED_BYTES`.
///
/// * `retained` — number of details currently holding a page; replacing one of those is not an
///   addition and never consults this rule.
///
/// Returns `true` only while the aggregate charge stays at or under
/// `MAX_RETAINED_DIFF_PAGE_BYTES`. Saturating arithmetic keeps an absurd count from wrapping into
/// a false admission. Pure and total: it performs no I/O and never evicts a live peer's page.
const fn admits_new_diff_page(retained: usize) -> bool {
    retained
        .saturating_add(1)
        .saturating_mul(DIFF_PAGE_RETAINED_BYTES)
        <= MAX_RETAINED_DIFF_PAGE_BYTES
}

/// Proves the exact aggregate retention cap and its refusal boundary, which `set_diff_page` reports
/// honestly instead of evicting another live peer's retained evidence.
#[test]
fn retained_diff_page_admission_caps_sixteen_worst_case_pages() {
    assert_eq!(DIFF_PAGE_RETAINED_BYTES, 9 * 1024 * 1024);
    assert_eq!(MAX_RETAINED_DIFF_PAGE_BYTES, 16 * 9 * 1024 * 1024);
    for (retained, admitted) in [
        (0, true),
        (1, true),
        (15, true),
        (16, false),
        (usize::MAX, false),
    ] {
        assert_eq!(
            admits_new_diff_page(retained),
            admitted,
            "retained={retained}"
        );
    }
}

/// Holds one worker task and its finite ingress channels; dropping it cancels the daemon-owned loop.
pub struct WorkerHandle {
    /// Shared finite state, including the exact binding owner.
    shared: Arc<Shared>,
    /// Inspections are separate from slow jobs so pending work never monopolizes IPC.
    inspect: mpsc::Sender<Inspection>,
    /// Consumed exactly once during daemon startup under Application's exclusive lock.
    receiver: Mutex<Option<mpsc::Receiver<Inspection>>>,
    /// Sole owned worker task; no duplicate boot is allowed.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Cooperative stop flag for the bounded startup fingerprint reader.
    startup_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Optional local-only sink cloned into the sole worker at daemon startup.
    telemetry: Arc<Mutex<Option<Telemetry>>>,
    /// Fixed-byte native fallback receiver drained before the telemetry writer stops.
    fallback_ingress: Arc<Mutex<Option<super::codex_hook::NativeFallbackIngress>>>,
}
impl std::fmt::Debug for WorkerHandle {
    /// Omits all host, target, profile and result contents.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("WorkerHandle(..)")
    }
}
impl Drop for WorkerHandle {
    /// Cancels owned work without inventing child-reap or admission-release evidence.
    fn drop(&mut self) {
        self.startup_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(task) = self.task.get_mut()
            && let Some(task) = task.take()
        {
            task.abort();
        }
    }
}
impl WorkerHandle {
    /// Returns whether this worker owns the exact launcher or lease-registered attachment.
    /// Discovery-only dispatchers have no worker and retain their unavailable behavior.
    pub fn accepts_attachment(&self, attachment: &str) -> bool {
        self.target(attachment).is_some()
    }

    /// Registers a separate target for a host-selected Claude project without changing live peers.
    pub fn register_claude_candidate(&self, candidate: &Path) -> Option<String> {
        let template = self.shared.launcher.sole_target()?.clone();
        let resolved = std::fs::canonicalize(candidate).ok()?;
        if resolved != candidate {
            return None;
        }
        let candidate = resolved;
        if !template.candidate.is_dir() {
            self.shared
                .retired_initial_claude_target
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let mut targets = self.shared.claude_targets.lock().ok()?;
        targets.retain(|_, target| target.candidate.is_dir());
        if let Some((attachment, _)) = targets
            .iter()
            .find(|(_, target)| target.candidate == candidate)
        {
            return Some(attachment.clone());
        }
        if targets.len() >= 63 {
            return None;
        }
        let mut nonce = [0_u8; 32];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").ok()?, &mut nonce)
            .ok()?;
        let attachment = nonce
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        targets.insert(
            attachment.clone(),
            LaunchTarget {
                candidate,
                ..template
            },
        );
        Some(attachment)
    }

    /// Returns whether a job is queued or currently executing, without blocking.
    ///
    /// This defers idle shutdown (T26B): a job that only ever reaches its terminal state through
    /// the sole worker must not be killed by a daemon that considers itself idle while it runs.
    /// Both counts live behind one short-held lock, so the answer is a consistent snapshot.
    pub fn is_processing(&self) -> bool {
        self.shared.test_runs.is_busy()
            || self
                .shared
                .ledger
                .lock()
                .is_ok_and(|ledger| ledger.in_flight > 0 || !ledger.queue.is_empty())
    }

    /// Creates finite channels only; Store and Workspace are opened later under the daemon lock.
    pub fn new(
        bindings: Arc<Mutex<HostBindingGuard>>,
        launcher: LauncherConfig,
        nonce: [u8; 32],
        admission: Arc<Mutex<crate::execution::AdmissionController>>,
    ) -> Self {
        Self::new_with_telemetry(
            bindings,
            launcher,
            nonce,
            admission,
            Arc::new(super::telemetry::NoopEditTelemetry),
        )
    }

    /// Creates finite channels with a replaceable fail-open telemetry sink.
    ///
    /// The sink receives only typed sanitized facts and must return immediately. It cannot affect
    /// admission, authority, edit settlement, native fallback, or any returned result.
    pub fn new_with_telemetry(
        bindings: Arc<Mutex<HostBindingGuard>>,
        launcher: LauncherConfig,
        nonce: [u8; 32],
        admission: Arc<Mutex<crate::execution::AdmissionController>>,
        telemetry: Arc<dyn super::telemetry::EditTelemetry>,
    ) -> Self {
        let (inspect, receiver) = mpsc::channel(launcher.limits.queued);
        Self {
            shared: Arc::new(Shared {
                bindings,
                ledger: Mutex::new(Ledger::default()),
                notify: Notify::new(),
                launcher,
                claude_targets: Mutex::new(BTreeMap::new()),
                retired_initial_claude_target: std::sync::atomic::AtomicBool::new(false),
                nonce,
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                shutdown_failure: Mutex::new(None),
                admission,
                telemetry,
                problem_source: None,
                project_feed: None,
                test_runs: TestRuns::default(),
            }),
            inspect,
            receiver: Mutex::new(Some(receiver)),
            task: Mutex::new(None),
            startup_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            telemetry: Arc::new(Mutex::new(None)),
            fallback_ingress: Arc::new(Mutex::new(None)),
        }
    }

    /// Attaches the project-problem snapshot source used by the `ide.context` problems kind.
    ///
    /// Must be called before [`WorkerHandle::start`]: the spawned worker task clones the shared
    /// state, so the source can only be installed while this handle still owns it exclusively.
    /// Without a source the problems kind reports the honest `checks disabled` outcome.
    pub fn with_problem_source(mut self, problem_source: Arc<dyn ProblemSource>) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("problem source must be attached before worker startup")
            .problem_source = Some(problem_source);
        self
    }

    /// Attaches the daemon's project problem feed as both trigger sink and problem source.
    ///
    /// Must be called before [`WorkerHandle::start`], like [`WorkerHandle::with_problem_source`].
    /// Successful `ide.start` activations and `ide.edit` results that wrote the file then schedule
    /// checks, and `ide.stop` forgets the binding's feed state.
    pub fn with_project_feed(mut self, feed: Arc<ProjectProblemFeed>) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("project feed must be attached before worker startup");
        shared.problem_source = Some(feed.clone());
        feed.with_test_runs(shared.test_runs.clone());
        shared.project_feed = Some(feed);
        self
    }

    /// Returns the attached project problem feed, if project checks are configured.
    pub fn project_feed(&self) -> Option<&Arc<ProjectProblemFeed>> {
        self.shared.project_feed.as_ref()
    }

    /// Returns an undelivered test status line for the binding that started that job.
    pub fn test_status_line(&self, binding: &[u8; 32]) -> Option<String> {
        self.shared.test_runs.status_line_for_binding(binding)
    }

    /// Marks a test status line delivered only if it is still current and fits the reply.
    pub fn mark_test_status_delivered(&self, binding: &[u8; 32], line: &str) -> bool {
        self.shared.test_runs.mark_status_delivered(binding, line)
    }

    /// Returns the shared slot that holds the telemetry owner once startup has opened it.
    ///
    /// Lets callbacks created before startup (for example project check completion) record
    /// telemetry later without holding a worker reference; the slot stays empty when telemetry
    /// is unavailable.
    pub fn telemetry_slot(&self) -> Arc<Mutex<Option<Telemetry>>> {
        self.telemetry.clone()
    }

    /// Answers a `kind: "problems"` context request from the daemon's in-memory problem source.
    ///
    /// EYES-r2: the request runs as a bounded managed job inside the worker task — never as a
    /// Claude foreground helper — because only the worker owns the durable authority and its
    /// authorized worktree. The caller waits on the job's bounded oneshot exactly like Stop; a
    /// lost wait still leaves the finished result retrievable through the retained detail
    /// reference until the ledger evicts it. A Codex `observed` state is rechecked before any
    /// snapshot lookup; Claude passes `None` because its hook path has no host sandbox metadata.
    /// Its result never advertises an edit source reference because no source was observed.
    pub async fn context_problems(
        &self,
        invocation: ValidatedInvocation,
        parameters: Value,
        attachment: &str,
    ) -> PeerReply {
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            AssistanceTool::Context,
            parameters,
            attachment,
            Some(send),
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(Duration::from_millis(800), wait).await {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
            },
        }
    }

    /// Returns the daemon's sole telemetry owner after startup, if its local schema was available.
    ///
    /// The returned clone shares the Worker's Application Store owner. It is absent before startup
    /// or when telemetry initialization failed, neither of which changes dispatch behaviour.
    pub fn telemetry(&self) -> Option<Telemetry> {
        self.telemetry.lock().ok()?.clone()
    }
    /// Opens one session-local Workspace owner plus an independently locked telemetry sink.
    ///
    /// `runtime` is this daemon's private directory and owns all Workspace/Changes authority state.
    /// An absolute `AGENT_IDE_TELEMETRY_DATABASE` may select durable capture independently; lock
    /// contention disables only telemetry. Startup fails only when the session-local store or its
    /// authority schemas cannot open, and creates the worker task exactly once. When telemetry is
    /// available, fallback ingress accepts only datagrams authenticated by a configured launcher
    /// attachment for this exact runtime.
    pub async fn start(&self, runtime: &Path) -> Result<(), FailureCode> {
        let receiver = self
            .receiver
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .take()
            .ok_or(FailureCode::Internal)?;
        let shared = self.shared.clone();
        let telemetry_owner = self.telemetry.clone();
        let fallback_owner = self.fallback_ingress.clone();
        let runtime = runtime.to_path_buf();
        let (ready, wait) = oneshot::channel();
        let cancel = self.startup_cancel.clone();
        let task = tokio::spawn(async move {
            let verification = shared.clone();
            if !matches!(
                tokio::task::spawn_blocking(move || verification
                    .launcher
                    .verify_executables(&cancel))
                .await,
                Ok(Ok(()))
            ) {
                let _ = ready.send(Err(FailureCode::ExecutionProfile));
                return;
            }

            let database = std::env::var_os("AGENT_IDE_STATE_DATABASE")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| runtime.join("state.sqlite"));
            let store = match Store::open_with_backup_root(
                &database,
                &runtime.join("backups"),
                EffectiveConfig::defaults().store(),
            ) {
                Ok(store) => store,
                Err(_) => {
                    let _ = ready.send(Err(FailureCode::Internal));
                    return;
                }
            };
            // ponytail: one process-lifetime Store Arc leak per daemon boot; replace with explicit
            // task-owned shutdown once in-process daemon restart becomes a supported lifecycle.
            let store: &'static Arc<Store> = Box::leak(Box::new(Arc::new(store)));
            let telemetry_database = std::env::var_os("AGENT_IDE_TELEMETRY_DATABASE")
                .map(std::path::PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| runtime.join("telemetry.sqlite"));
            let telemetry = if telemetry_database == database {
                None
            } else {
                Telemetry::open_database(&telemetry_database, TelemetryConfig::default())
                    .await
                    .ok()
            };
            let workspace = match DurableWorkspace::open(store).await {
                Ok(owner) => owner,
                Err(_) => {
                    let _ = ready.send(Err(FailureCode::WorkspaceActivation));
                    return;
                }
            };
            let observations = WorkspaceStore::new(store);
            if !matches!(
                observations.install_schema().await,
                Ok(MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. })
            ) {
                let _ = ready.send(Err(FailureCode::SourceUnavailable));
                return;
            }
            let edits = EditReceiptStore::new(store);
            if !matches!(
                edits.install_schema().await,
                Ok(MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. })
            ) {
                let _ = ready.send(Err(FailureCode::Internal));
                return;
            }
            if let Ok(mut configured) = telemetry_owner.lock() {
                *configured = telemetry.clone();
            }
            if let Some(telemetry) = telemetry.clone()
                && let Ok(ingress) = super::codex_hook::NativeFallbackIngress::bind(
                    &runtime,
                    telemetry,
                    shared.launcher.attachments(),
                )
                && let Ok(mut configured) = fallback_owner.lock()
            {
                *configured = Some(ingress);
            }
            let _ = ready.send(Ok(()));
            Worker {
                admission: shared.admission.clone(),
                shared,
                workspace,
                observations,
                edits,
                grants: BTreeMap::new(),
                pending_revocations: std::collections::BTreeSet::new(),
                registered: BTreeMap::new(),
                baselines: BTreeMap::new(),
                source_sequence: 0,
                uncertain: std::collections::BTreeSet::new(),
                uncertain_snapshots: Vec::new(),
                runtime,
                providers: providers::Providers::new(),
                telemetry,
            }
            .run(receiver)
            .await;
        });
        *self.task.lock().map_err(|_| FailureCode::Internal)? = Some(task);
        wait.await.map_err(|_| FailureCode::Internal)?
    }
    /// Fences admission, cancels every binding, and waits for the sole worker to reap providers.
    pub async fn shutdown(&self) -> Result<(), FailureCode> {
        self.startup_cancel
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared
            .shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            for sender in ledger.cancellation.values() {
                let _ = sender.send(true);
            }
            ledger.queue.clear();
            ledger.details.clear();
            ledger.starts.clear();
            ledger.feedback.clear();
            ledger.delivered.clear();
        }
        self.shared.notify.notify_one();
        let task = self.task.lock().map_err(|_| FailureCode::Internal)?.take();
        let Some(mut task) = task else {
            return Ok(());
        };
        let result = match tokio::time::timeout(Duration::from_secs(39), &mut task).await {
            Ok(Ok(())) => self
                .shared
                .shutdown_failure
                .lock()
                .map_err(|_| FailureCode::Internal)?
                .take()
                .map_or(Ok(()), Err),
            Ok(Err(_)) => Err(FailureCode::Internal),
            Err(_) => {
                task.abort();
                let _ = task.await;
                Err(FailureCode::Deadline)
            }
        };
        let fallback = self
            .fallback_ingress
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .take();
        if let Some(fallback) = fallback {
            fallback.shutdown().await;
        }
        let telemetry = self.telemetry();
        if let Some(telemetry) = telemetry {
            telemetry.shutdown().await;
        }
        result
    }
    /// Enqueues or resolves the exact query, returning pending without waiting for provider warmup.
    pub async fn submit(
        &self,
        invocation: ValidatedInvocation,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
    ) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        let expected = Some((tool, selection(&parameters)));
        if let Some(reference) = parameters.get("detail_ref").and_then(Value::as_str) {
            return self
                .inspect(binding, reference.to_owned(), attachment, expected)
                .await;
        }
        if self.target(attachment).is_none() {
            return PeerReply::Error {
                code: FailureCode::LauncherConfiguration,
            };
        }
        if tool == AssistanceTool::Test {
            let (send, wait) = oneshot::channel();
            if let Err(code) = self.enqueue(invocation, tool, parameters, attachment, Some(send)) {
                return PeerReply::Error { code };
            }
            return match tokio::time::timeout(Duration::from_secs(5), wait).await {
                Ok(Ok(reply)) => reply,
                _ => PeerReply::Error {
                    code: FailureCode::Deadline,
                },
            };
        }
        match admit_initial_inspection(&self.inspect, || {
            self.enqueue(invocation, tool, parameters, attachment, None)
        }) {
            Ok((reference, permit)) => {
                self.inspect_reserved(binding, reference, expected, permit)
                    .await
            }
            Err(code) => PeerReply::Error { code },
        }
    }

    /// Signals revocation immediately and waits only for the bounded exact stop result.
    ///
    /// The caller has already closed external admission; every queued job for this binding is
    /// cancelled and removed while other bindings' jobs stay queued.
    pub async fn stop(&self, invocation: ValidatedInvocation, attachment: &str) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        self.shared
            .test_runs
            .observe_binding(&binding.fingerprint());
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            if let Some(sender) = ledger.cancellation.remove(&binding) {
                let _ = sender.send(true);
            }
            ledger
                .queue
                .retain(|job| job.invocation.binding_ref() != &binding);
            ledger.details.retain(|_, detail| detail.binding != binding);
            ledger.starts.retain(|(owner, _), _| owner != &binding);
            ledger.native_epoch.remove(&binding);
            ledger.feedback.remove(&binding);
            ledger.delivered.remove(&binding);
        }
        if let Some(feed) = &self.shared.project_feed {
            feed.forget(&binding.fingerprint());
        }
        self.shared.notify.notify_one();
        let (send, wait) = oneshot::channel();
        if let Err(code) = self.enqueue(
            invocation,
            AssistanceTool::Stop,
            serde_json::json!({}),
            attachment,
            Some(send),
        ) {
            return PeerReply::Error { code };
        }
        match tokio::time::timeout(Duration::from_millis(800), wait).await {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
            },
        }
    }

    /// Asks the sole worker for a same-binding, durably authorized result.
    pub async fn inspect(
        &self,
        binding: BindingRef,
        reference: String,
        attachment: &str,
        expected: Option<(AssistanceTool, [u8; 32])>,
    ) -> PeerReply {
        if self.target(attachment).is_none() {
            return PeerReply::Error {
                code: FailureCode::LauncherConfiguration,
            };
        }
        let permit = match reserve_inspection(&self.inspect) {
            Ok(permit) => permit,
            Err(code) => return PeerReply::Error { code },
        };
        self.inspect_reserved(binding, reference, expected, permit)
            .await
    }

    /// Publishes one fully built inspection through a permit that already owns channel capacity.
    async fn inspect_reserved(
        &self,
        binding: BindingRef,
        reference: String,
        expected: Option<(AssistanceTool, [u8; 32])>,
        permit: mpsc::OwnedPermit<Inspection>,
    ) -> PeerReply {
        let (reply, wait) = oneshot::channel();
        permit.send(Inspection {
            binding,
            reference,
            expected,
            reply,
        });
        wait.await.unwrap_or(PeerReply::Error {
            code: FailureCode::Internal,
        })
    }

    /// Returns the launcher or lease-registered target mapped to one opaque attachment.
    /// No model argument, working directory, PID or timing selects the target.
    pub fn target(&self, attachment: &str) -> Option<LaunchTarget> {
        self.shared
            .claude_targets
            .lock()
            .ok()?
            .get(attachment)
            .cloned()
            .or_else(|| {
                self.shared
                    .launcher
                    .target(attachment)
                    .filter(|_| {
                        !self
                            .shared
                            .retired_initial_claude_target
                            .load(std::sync::atomic::Ordering::Acquire)
                    })
                    .cloned()
            })
    }

    /// Returns the shared validated finite worker limits for this daemon boot.
    pub fn limits(&self) -> super::launcher::ProductLimits {
        self.shared.launcher.limits
    }

    /// Invalidates cached results on native hints; reads wait for a current MCP sandbox observation.
    pub fn native_hint(&self, binding: BindingRef) {
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            let epoch = ledger.native_epoch.entry(binding).or_default();
            *epoch = epoch.saturating_add(1);
        }
        self.shared.notify.notify_one();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.shared
                .telemetry
                .native_fallback(super::telemetry::NativeFallbackReason::NativeSelected);
        }));
    }

    /// Coalesces managed-Codex registered-path reconciliation at a trusted read boundary.
    ///
    /// `invalidate` is true for new Context/Diff captures, which must fence older retained output;
    /// Inspect instead performs its existing direct current-byte checks and leaves a hint for the
    /// next worker job without invalidating a pending result merely because it was polled.
    pub fn managed_read_boundary(&self, binding: BindingRef, invalidate: bool) {
        if self.shared.bindings.lock().is_ok_and(|mut guard| {
            guard
                .request_registered_path_reconciliation(&binding)
                .is_ok()
        }) && invalidate
        {
            self.native_hint(binding);
        }
    }

    /// Returns and consumes one same-binding delta only after a newer native epoch.
    ///
    /// Missing, stopped, unchanged-epoch, stale, unversioned, or already-inline-delivered feedback
    /// returns `None`. A fact backed by a source observation is also never delivered here and is
    /// consumed silently — exactly like a stale one: the hook boundary carries no live host
    /// sandbox state, so a capture-time coverage decision can never be re-authorized against the
    /// *current* binding state, and the freshness reread it would require is a native read the
    /// hook path cannot prove (T36B-r). Dropping (rather than retaining) the fact keeps a later
    /// hook from retrying the same unprovable read; such facts still reach callers inline through
    /// Context/Inspect, where every delivery re-proves the source under the live profile. A fact
    /// already handed to a live caller in a submitted Context/Inspect reply is likewise removed
    /// here but its text withheld, because it already reached the transport once. The check
    /// performs no provider execution and does not interpret a Claude permission mode as
    /// authority.
    pub async fn take_current_feedback(&self, binding: BindingRef) -> Option<String> {
        self.shared.active(&binding).ok()?;
        let feedback = {
            let mut ledger = self.shared.ledger.lock().ok()?;
            let current_epoch = ledger.native_epoch.get(&binding).copied().unwrap_or(0);
            let feedback = ledger.feedback.remove(&binding)?;
            // T36B-r: delivery-time authorization, not capture-time. No current binding state
            // exists at the hook boundary, so source-backed feedback is unavailable-authorized:
            // never reread, never released here.
            let feedback = (current_epoch > feedback.native_epoch
                && !feedback.inline_delivered
                && feedback.source.is_none())
            .then_some(feedback)?;
            // A hook consumption counts toward the same dedup state as an inline submission: a
            // later, redundant Context job reproducing this exact unchanged issue must not
            // re-arm it, even though this single-slot entry is gone.
            ledger
                .delivered
                .insert(binding.clone(), feedback.identity.clone());
            feedback
        };
        self.shared.active(&binding).ok()?;
        Some(feedback.text)
    }

    /// Atomically bounds and publishes one operation, without file, database or child-process I/O.
    #[allow(clippy::too_many_arguments)]
    fn enqueue(
        &self,
        invocation: ValidatedInvocation,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
        stop_reply: Option<oneshot::Sender<PeerReply>>,
    ) -> Result<String, FailureCode> {
        if self
            .shared
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(FailureCode::Internal);
        }
        if !self
            .task
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Err(FailureCode::Internal);
        }
        let target = self
            .target(attachment)
            .ok_or(FailureCode::LauncherConfiguration)?;
        let binding = invocation.binding_ref().clone();
        let retain_detail = retains_detail(tool);
        let mut ledger = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?;
        let start = if tool == AssistanceTool::Start {
            Some((
                binding.clone(),
                parameters["activation_id"]
                    .as_str()
                    .ok_or(FailureCode::Internal)?
                    .to_owned(),
            ))
        } else {
            None
        };
        if let Some(key) = &start
            && let Some(reference) = ledger.starts.get(key)
        {
            return Ok(reference.clone());
        }
        let queue_cap = queue_capacity(self.shared.launcher.limits.queued, tool);
        if ledger.queue.len() >= queue_cap {
            return Err(FailureCode::Capacity);
        }
        if retain_detail && ledger.details.len() >= self.shared.launcher.limits.details {
            return Err(FailureCode::Capacity);
        }
        ledger.next = ledger.next.checked_add(1).ok_or(FailureCode::Capacity)?;
        let reference = format!(
            "{}-{}",
            blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
            ledger.next
        );
        let cancel = if tool == AssistanceTool::Stop {
            watch::channel(false).1
        } else {
            ledger
                .cancellation
                .entry(binding.clone())
                .or_insert_with(|| watch::channel(false).0)
                .subscribe()
        };
        if retain_detail {
            ledger.details.insert(
                reference.clone(),
                Detail {
                    binding: binding.clone(),
                    reply: PeerReply::Pending {
                        detail_ref: reference.clone(),
                        // Daemon-executed work needs no foreground helper instruction.
                    },
                    selection: (tool, selection(&parameters)),
                    authority: None,
                    source: None,
                    native_epoch: 0,
                    diff_page: None,
                    diff_page_fresh: false,
                    context_page: None,
                    context_page_fresh: false,
                    diff_provenance: None,
                },
            );
        }
        if let Some(key) = start {
            ledger.starts.insert(key, reference.clone());
        }
        let job = Job {
            reference: reference.clone(),
            invocation,
            tool,
            parameters,
            target,
            deadline: tokio::time::Instant::now()
                + Duration::from_millis(self.shared.launcher.limits.operation_ms),
            cancel,
            stop_reply,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        if tool == AssistanceTool::Stop {
            ledger.queue.push_front(job);
        } else {
            ledger.queue.push_back(job);
        }
        drop(ledger);
        self.shared.notify.notify_one();
        Ok(reference)
    }
}

/// Returns whether an operation needs a retained result detail after it completes.
///
/// Stop returns directly to its waiting caller; every other operation's captured text may span
/// several `ide.inspect` pages, so it needs an addressable, capacity-bounded slot.
fn retains_detail(tool: AssistanceTool) -> bool {
    tool != AssistanceTool::Stop
}

/// Returns the finite queue ceiling, reserving bounded cleanup headroom for terminal work.
///
/// Regular work may use only `ordinary`; stop may use the fixed reserve because it releases
/// existing lifecycle state rather than creating new details.
fn queue_capacity(ordinary: usize, tool: AssistanceTool) -> usize {
    ordinary + usize::from(tool == AssistanceTool::Stop) * 64
}

/// Bounds the in-memory pending-revocation set so an unbounded stream of failed durable revokes
/// cannot grow it. A full set refuses to record a further pending binding rather than evicting one;
/// the caller still learns the failure, and a daemon restart boot-fences every old grant anyway.
const MAX_PENDING_REVOCATIONS: usize = 64;

/// One boot's sequential durable owner; provider operations may be interrupted by inspection service.
struct Worker<'a> {
    /// Shared bounded ingress and liveness state.
    shared: Arc<Shared>,
    /// Exactly one boot-fenced authority owner, opened once above.
    workspace: DurableWorkspace<'a>,
    /// Workspace-owned persistence for registered source observations.
    observations: WorkspaceStore<'a>,
    /// Changes-owned durable one-file edit receipts sharing Application's sole Store owner.
    edits: EditReceiptStore<'a>,
    /// Recoverable committed activation receipts, at most one for each live host binding.
    grants: BTreeMap<BindingRef, StartReceipt>,
    /// Bindings whose provider settlement succeeded but whose durable revoke failed, so their
    /// receipt, caches and registrations are deliberately retained for a bounded cleanup-only
    /// retry. It never carries a physical-process uncertainty, which stays in `uncertain`.
    pending_revocations: std::collections::BTreeSet<BindingRef>,
    /// Only explicitly requested paths are polled; no directory scanning is performed.
    registered: BTreeMap<BindingRef, std::collections::BTreeSet<std::path::PathBuf>>,
    /// Durable partial activation baselines retained for same-binding diff provenance.
    baselines: BTreeMap<BindingRef, crate::workspace::git::BaselineContext>,
    /// Boot-unique source observation operation sequence.
    source_sequence: u64,
    /// Finite physical-effect admission, shared by discovery, snapshots and language providers.
    ///
    /// This is a handle to the daemon's single [`Shared::admission`] controller, not a private
    /// second one, so a Claude helper claim taken on the helper socket task removes capacity this
    /// worker can no longer grant, and vice versa.
    admission: Arc<Mutex<crate::execution::AdmissionController>>,
    /// Bindings with uncertain physical/durable completion cannot report successful cleanup.
    uncertain: std::collections::BTreeSet<BindingRef>,
    /// Retains private scratch files when a child has no positive reap evidence. Entries are never
    /// deleted automatically: without a positive reap we cannot prove the child is no longer
    /// writing, so quarantine is the only honest outcome. This list has no fixed capacity today;
    /// its operational ceiling is one entry per genuinely uncertain reap for this boot, expected to
    /// be rare, bounded in practice by the daemon's own physical-admission limits rather than by an
    /// explicit cap on this field.
    uncertain_snapshots: Vec<crate::workspace::git::snapshot::SnapshotIntent>,
    /// Private runtime namespace for current-boot provider sockets.
    runtime: std::path::PathBuf,
    /// Exact provider/backend/view ownership and generations.
    providers: providers::Providers,
    /// Optional closed telemetry sink shared by Assistance producer boundaries.
    telemetry: Option<Telemetry>,
}

impl<'a> Worker<'a> {
    /// Handles an explicit test start or same-worktree status request.
    async fn test(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        let root = authority.worktree().worktree_path().to_path_buf();
        let budget = Duration::from_secs(
            job.parameters
                .get("budget_s")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600),
        );
        let (result, detail_ref) = if let Some(id) =
            job.parameters.get("status").and_then(Value::as_u64)
        {
            let Some(job_status) = self.shared.test_runs.get(&root, id, &binding.fingerprint())
            else {
                return Ok((
                    PeerReply::Complete {
                        kind: ResultKind::Test,
                        text: format!("tests #{id}: unknown job"),
                        detail_ref: None,
                        truncated: false,
                        continuation: false,
                    },
                    Some(authority),
                    None,
                ));
            };
            if let Some(result) = job_status.result {
                let owns_detail = job_status.owner == binding.fingerprint();
                let text = test_result_text(id, &result, owns_detail);
                if owns_detail {
                    let (first_page, following_pages) =
                        ContextPageState::new(result.output.clone(), 0, false, ResultKind::Test)
                            .next(&result.detail_ref)?;
                    let retained = if let Ok(mut ledger) = self.shared.ledger.lock()
                        && let Some(detail) = ledger.details.get_mut(&result.detail_ref)
                    {
                        detail.reply = first_page;
                        true
                    } else {
                        false
                    };
                    if retained {
                        self.shared
                            .set_context_page(&result.detail_ref, following_pages);
                        self.shared
                            .test_runs
                            .clear_output(&root, id, &binding.fingerprint());
                    }
                }
                (text, owns_detail.then_some(result.detail_ref))
            } else {
                (
                    format!("tests #{id}: running {} s", job_status.age.as_secs()),
                    None,
                )
            }
        } else {
            let (argv, language, selected_count) = if let Some(path) =
                job.parameters.get("path").and_then(Value::as_str)
            {
                match test_selection(&root, crate::lang::TestTarget::File(PathBuf::from(path))) {
                    Ok(selection) => selection,
                    Err(crate::lang::LangError::Unsupported(message)) => return Ok((
                        PeerReply::InvalidParameters {
                            message:
                                crate::assistance::facade::ParameterError::TestTargetUnsupported(
                                    message,
                                )
                                .message(AssistanceTool::Test),
                        },
                        Some(authority),
                        None,
                    )),
                    Err(_) => return Err(FailureCode::ProviderUnavailable),
                }
            } else if let Some(pattern) = job.parameters.get("pattern").and_then(Value::as_str) {
                match test_selection(&root, crate::lang::TestTarget::Pattern(pattern.to_owned())) {
                    Ok(selection) => selection,
                    Err(crate::lang::LangError::Unsupported(message)) => return Ok((
                        PeerReply::InvalidParameters {
                            message:
                                crate::assistance::facade::ParameterError::TestTargetUnsupported(
                                    message,
                                )
                                .message(AssistanceTool::Test),
                        },
                        Some(authority),
                        None,
                    )),
                    Err(_) => return Err(FailureCode::ProviderUnavailable),
                }
            } else if let Some(args) = job.parameters.get("command").and_then(Value::as_array) {
                let language = detect_test_language(&root).unwrap_or(crate::lang::Language::Rust);
                (
                    args.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    language,
                    None,
                )
            } else if let Some(symbol) = job
                .parameters
                .get("symbol")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let (referencing_tests, language) =
                    self.tests_referencing_symbol(job, &symbol).await?;
                if referencing_tests.is_empty() {
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: format!(
                                "tests: no tests reference {symbol}; run by path or pattern"
                            ),
                            detail_ref: None,
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
                let path = crate::lang::SymbolPath::parse(&symbol)
                    .map_err(|_| FailureCode::UnknownSymbol)?;
                let target = crate::lang::TestTarget::Symbol {
                    path,
                    referencing_tests,
                };
                let support =
                    crate::lang::support(language).ok_or(FailureCode::ProviderUnavailable)?;
                let project = support
                    .detect(&root)
                    .ok_or(FailureCode::ProviderUnavailable)?;
                let selection = match support.test_selection(&project, &target) {
                    Ok(selection) => selection,
                    Err(crate::lang::LangError::Unsupported(message)) => return Ok((
                        PeerReply::InvalidParameters {
                            message:
                                crate::assistance::facade::ParameterError::TestTargetUnsupported(
                                    message,
                                )
                                .message(AssistanceTool::Test),
                        },
                        Some(authority),
                        None,
                    )),
                    Err(_) => return Err(FailureCode::ProviderUnavailable),
                };
                let count = Some(selection.tests.len());
                (selection.command, language, count)
            } else {
                return Ok((
                    PeerReply::Complete {
                        kind: ResultKind::Test,
                        text: "symbol test selection unavailable".into(),
                        detail_ref: None,
                        truncated: false,
                        continuation: false,
                    },
                    Some(authority),
                    None,
                ));
            };
            match self.shared.test_runs.start(
                root,
                argv.clone(),
                language,
                budget,
                job.reference.clone(),
                binding.fingerprint(),
            ) {
                StartResult::Started(id) => {
                    let selected = selected_count
                        .map_or_else(String::new, |count| format!(" ({count} tests selected)"));
                    let line = format!(
                        "tests #{id}: started — {}{selected} (budget {} s)",
                        display_argv(&argv),
                        budget.as_secs()
                    );
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: line,
                            detail_ref: Some(job.reference.clone()),
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
                StartResult::Running(id, age) => (
                    format!(
                        "tests #{id}: still running ({} s); ide.test {{\"status\": {id}}}",
                        age.as_secs()
                    ),
                    None,
                ),
                StartResult::Failed(error) => {
                    let program = argv.first().map(String::as_str).unwrap_or("");
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: format!(
                                "tests: could not start {}: {}",
                                test_text_line(program, 160),
                                test_text_line(&error, 240)
                            ),
                            detail_ref: None,
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
            }
        };
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Test,
                text: result,
                detail_ref,
                truncated: false,
                continuation: false,
            },
            Some(authority),
            None,
        ))
    }

    /// Records one already-settled owned-child completion using closed output and lifecycle facts.
    ///
    /// `output_bytes` is the saturated sum of existing bounded captures and never contains their
    /// bytes. `truncated` and `cancelled` are existing process facts; the event is dropped when no
    /// sink is attached and cannot affect admission release or the caller's result.
    pub(super) fn record_execution(
        &self,
        elapsed: Duration,
        output_bytes: usize,
        truncated: bool,
        cancelled: bool,
    ) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let output = if truncated {
            OutputSizeClass::Truncated
        } else if output_bytes == 0 {
            OutputSizeClass::Empty
        } else if output_bytes <= 4 * 1024 {
            OutputSizeClass::Small
        } else if output_bytes <= 64 * 1024 {
            OutputSizeClass::Medium
        } else {
            OutputSizeClass::Large
        };
        adapters::execution_summary(
            telemetry,
            elapsed,
            output,
            AdmissionState::Admitted,
            if cancelled {
                CancellationState::Requested
            } else {
                CancellationState::NotRequested
            },
            DescendantSettlement::Unverified,
        );
    }

    /// Processes one slow operation at a time; inspections run on an independently scheduled task
    /// (see `inspection_loop`) so a non-yielding poll of the current operation cannot starve
    /// `ide.inspect`. Shutdown first cancels the current operation, allowing its Rust or forwarder
    /// child to reap, then this loop closes retained providers before returning.
    async fn run(mut self, inspections: mpsc::Receiver<Inspection>)
    where
        'a: 'static,
    {
        let inspector = tokio::spawn(inspection_loop(
            self.workspace.clone(),
            self.shared.clone(),
            inspections,
        ));
        let _inspector = AbortOnDrop(inspector);
        loop {
            if self
                .shared
                .shutting_down
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.release_all_live_rust().await;
                if let Err(code) = self.close_all_providers().await
                    && let Ok(mut failure) = self.shared.shutdown_failure.lock()
                {
                    *failure = Some(code);
                }
                return;
            }
            let shared = self.shared.clone();
            let wake = shared.notify.notified();
            // The in-flight count moves in the same locked section as the pop, so "queue empty
            // and nothing in flight" is never observed while a job is between the two (T26B).
            let job = shared.ledger.lock().ok().and_then(|mut ledger| {
                ledger
                    .queue
                    .pop_front()
                    .inspect(|_| ledger.in_flight = ledger.in_flight.saturating_add(1))
            });
            match job {
                Some(job) => {
                    self.perform(job).await;
                    if let Ok(mut ledger) = shared.ledger.lock() {
                        ledger.in_flight = ledger.in_flight.saturating_sub(1);
                    }
                }
                None => wake.await,
            }
        }
    }
    /// Rechecks queued liveness, executes only the selected owner operation, and fences every result.
    async fn perform(&mut self, mut job: Job) {
        let binding = job.invocation.binding_ref().clone();
        job.native_epoch = self
            .shared
            .ledger
            .lock()
            .ok()
            .and_then(|ledger| ledger.native_epoch.get(&binding).copied())
            .unwrap_or(0);
        let result = if job.tool == AssistanceTool::Stop {
            self.revoke(&binding, &job.reference).await
        } else if *job.cancel.borrow() || self.shared.active(&binding).is_err() {
            Err(FailureCode::Cancelled)
        } else if tokio::time::Instant::now() >= job.deadline {
            Err(FailureCode::Deadline)
        } else {
            if job.tool != AssistanceTool::Test {
                self.reconcile_hints(&job).await;
            }
            if tokio::time::Instant::now() >= job.deadline {
                Err(FailureCode::Deadline)
            } else {
                match job.tool {
                    AssistanceTool::Edit => self.edit(&mut job).await,
                    AssistanceTool::Start => self.activate(&mut job).await,
                    AssistanceTool::Context => self.context(&mut job).await,
                    AssistanceTool::Diff => self.diff(&mut job).await,
                    AssistanceTool::Outline => self.outline(&mut job).await,
                    AssistanceTool::Read => self.read(&mut job).await,
                    AssistanceTool::Symbol => self.symbol(&mut job).await,
                    AssistanceTool::Test => self.test(&mut job).await,
                    _ => Err(FailureCode::Internal),
                }
            }
        };
        let (reply, authority, source) = match result {
            Ok(result) => result,
            Err(code) => (PeerReply::Error { code }, None, None),
        };
        if let PeerReply::Error { code } = &reply {
            // T26B: a queued job's terminal failure must reach the error log with its closed
            // reason even when no caller view ever does — the dispatch path only logs the initial
            // `pending` placeholder a slow job returns, and daemon shutdown drops retained
            // details, so this line is otherwise the only record the job ever failed.
            let lifetime = Duration::from_millis(self.shared.launcher.limits.operation_ms);
            let started = job.deadline.checked_sub(lifetime).unwrap_or(job.deadline);
            crate::errorlog::record(
                errorlog_method(job.tool),
                job_failure_outcome(*code),
                crate::errorlog::Fields {
                    reason: Some((*code).into()),
                    correlation: Some(job.reference.as_str()),
                    detail: job.failure_detail.as_deref(),
                    duration_ms: u32::try_from(started.elapsed().as_millis()).ok(),
                    ..Default::default()
                },
            );
        }
        if let Some(feed) = &self.shared.project_feed {
            match (&reply, &authority) {
                (
                    PeerReply::Complete {
                        kind: ResultKind::Activation,
                        ..
                    },
                    Some(authority),
                ) if job.tool == AssistanceTool::Start => {
                    feed.activated_with_denies(
                        binding.fingerprint(),
                        authority.worktree().worktree_path(),
                        authority.worktree().git_common_dir(),
                        false,
                        Vec::new(),
                    );
                }
                (PeerReply::Edit { result, .. }, _)
                    if result.outcome.has_post_source() && !job.check_scheduled =>
                {
                    feed.changed(&binding.fingerprint());
                }
                _ => {}
            }
        }
        if let PeerReply::Edit { result, .. } = &reply {
            let lifetime = Duration::from_millis(self.shared.launcher.limits.operation_ms);
            let started = job.deadline.checked_sub(lifetime).unwrap_or(job.deadline);
            let duration_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
            let diagnostics = if result.outcome.has_post_source() && source.is_some() {
                super::telemetry::EditDiagnosticState::Refreshed
            } else if result.outcome.has_post_source()
                || result.outcome == ChangesEditOutcome::OutcomeUnknown
            {
                super::telemetry::EditDiagnosticState::Unknown
            } else {
                super::telemetry::EditDiagnosticState::NotApplicable
            };
            let bytes = serde_json::to_vec(result).map_or(0, |value| value.len());
            let fact = super::telemetry::EditTelemetryFact {
                outcome: result.outcome,
                duration_ms,
                diagnostics,
                output_size: super::telemetry::output_size(bytes),
            };
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.shared.telemetry.edit_completed(fact);
            }));
        }
        self.shared.complete(
            &job.reference,
            reply.clone(),
            authority,
            source,
            job.native_epoch,
        );
        if let Some(sender) = job.stop_reply.take() {
            // The oneshot send is the actual submission boundary for this synchronous-wait path
            // (Claude Start/Context/Diff/Stop): it only succeeds while the caller's own `wait`
            // has not already been dropped by its deadline. Only a reply that both reached a live
            // receiver and still carries its fact after `mark_feedback_inline_delivered` traces
            // the same final MCP/IPC fitting the real transport applies may be marked delivered;
            // a closed receiver or a carrier that fitting trimmed leaves the fact eligible for
            // exactly one later hook delivery instead of being marked as already submitted.
            let is_context = matches!(
                reply,
                PeerReply::Complete {
                    kind: ResultKind::Context,
                    ..
                }
            );
            let mark_reply = is_context.then(|| reply.clone());
            let paged = matches!(
                reply,
                PeerReply::Complete {
                    kind: ResultKind::Context | ResultKind::Diff,
                    continuation: true,
                    ..
                }
            );
            if sender.send(reply).is_ok() {
                if let Some(mark_reply) = mark_reply {
                    self.shared.mark_feedback_inline_delivered(
                        &binding,
                        &job.reference,
                        &mark_reply,
                    );
                }
                if paged {
                    // Page one just reached the caller through this settlement, so the first
                    // `ide.inspect` of its `detail_ref` must serve page two, not repeat page one
                    // (T16B). Only a lost receiver leaves the page undelivered and fresh.
                    self.shared.mark_context_page_delivered(&job.reference);
                }
            }
        }
    }
    /// Runs only the fixed discovery commands, settling each child before parsing.
    ///
    /// The activation root (the model's `root` or the launcher candidate) and the discovered Git
    /// worktree root and common directory must all lie below a configured allowed root.
    async fn activate(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        use crate::execution::{
            DiscoverWorktreeRequest, DiscoveryOperationRef, GitDiscoveryPolicy, GitDiscoveryQuery,
        };
        let binding = job.invocation.binding_ref().clone();
        let candidate = activation_root(job, self.shared.launcher.allowed_roots())?;
        let operation = DiscoveryOperationRef::new(format!("discover-{}", job.reference))
            .map_err(|_| FailureCode::Internal)?;
        let policy = GitDiscoveryPolicy::new(
            job.target.git.path.clone(),
            self.shared.launcher.limits.output_bytes,
        )
        .map_err(|_| {
            record_execution_profile(errorlog_method(job.tool), "git_policy");
            FailureCode::ExecutionProfileCause(ExecutionProfileCause::GitPolicy)
        })?;
        let mut evidence = Vec::with_capacity(3);
        for query in [
            GitDiscoveryQuery::ShowTopLevel,
            GitDiscoveryQuery::GitCommonDir,
            GitDiscoveryQuery::WorktreeListPorcelainZ,
        ] {
            let request = DiscoverWorktreeRequest::from_active_use(
                self.shared.active(&binding)?,
                candidate.clone().into_os_string(),
                operation.clone(),
            )
            .map_err(|_| FailureCode::Internal)?;
            let request = match request.validate_query(query, &policy) {
                Ok(request) => request,
                Err(_) => {
                    record_execution_profile(errorlog_method(job.tool), "query_policy");
                    job.failure_detail = Some("query_policy".to_owned());
                    return Err(FailureCode::ExecutionProfileCause(
                        ExecutionProfileCause::QueryPolicy,
                    ));
                }
            };
            if *job.cancel.borrow() {
                return Err(FailureCode::Cancelled);
            }
            if tokio::time::Instant::now() >= job.deadline {
                return Err(FailureCode::Deadline);
            }
            let active = self.shared.active(&binding)?;
            let lease = self.admit(&binding)?;
            let mut child = match request.spawn(lease, active) {
                Ok(child) => child,
                Err(error) => {
                    return Err(self.spawn_failure(error, &binding, errorlog_method(job.tool)));
                }
            };
            let remaining = job
                .deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .min(Duration::from_secs(60));
            let interrupted = tokio::select! {result=child.wait(remaining)=>result.is_err(),_=job.cancel.changed()=>true};
            let result = if interrupted {
                child
                    .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
                    .await
            } else {
                child
                    .reap(Duration::from_millis(500), Duration::from_millis(100))
                    .await
            };
            let completed = match result {
                Ok(completed) => completed,
                Err(_) => {
                    self.uncertain.insert(binding.clone());
                    return Err(FailureCode::Deadline);
                }
            };
            self.admission()
                .release_reaped(completed.settlement)
                .map_err(|_| FailureCode::Internal)?;
            self.record_execution(
                completed.evidence.elapsed(),
                completed
                    .evidence
                    .stdout()
                    .bytes
                    .len()
                    .saturating_add(completed.evidence.stderr().bytes.len()),
                completed.evidence.stdout().truncated || completed.evidence.stderr().truncated,
                completed.evidence.cancellation().is_some(),
            );
            if interrupted {
                return Err(if *job.cancel.borrow() {
                    FailureCode::Cancelled
                } else {
                    FailureCode::Deadline
                });
            }
            self.shared.active(&binding)?;
            evidence.push(completed.evidence);
        }
        let discovered =
            crate::workspace::git::discovery::validate_discovery(&candidate, &operation, &evidence)
                .map_err(|error| match error {
                    crate::workspace::git::GitError::UnsupportedDiscoveryGit => {
                        record_execution_profile(errorlog_method(job.tool), "git_unsupported");
                        FailureCode::ExecutionProfileCause(ExecutionProfileCause::GitUnsupported)
                    }
                    _ => FailureCode::WorkspaceActivation,
                })?;
        admit_discovered(
            self.shared.launcher.allowed_roots(),
            discovered.root(),
            discovered.common_dir(),
        )?;
        self.shared.active(&binding)?;
        let tree = self
            .workspace
            .resolve_worktree(
                discovered.root().to_path_buf(),
                discovered.repository_root().to_path_buf(),
                discovered.common_dir().to_path_buf(),
            )
            .await
            .map_err(|_| FailureCode::WorkspaceActivation)?;
        self.reconcile_pending_revocations(&tree, job.invocation.actor_id())
            .await;
        let mut identity = blake3::Hasher::new();
        identity.update(&binding.fingerprint());
        identity.update(
            job.parameters["activation_id"]
                .as_str()
                .ok_or(FailureCode::Internal)?
                .as_bytes(),
        );
        let operation = identity.finalize().to_hex().to_string();
        let request = crate::workspace::authority::ActivationRequest::new(
            operation,
            job.invocation.clone(),
            self.shared.active(&binding)?,
            tree,
        )
        .map_err(|_| FailureCode::WorkspaceActivation)?;
        // Do not cancel an in-flight durable commit: preserve its recoverable receipt before fencing output.
        let receipt = match self.workspace.activate(request).await {
            Ok(receipt) => receipt,
            Err(
                crate::workspace::durable::DurableError::OperationConflict
                | crate::workspace::durable::DurableError::Authority(
                    crate::workspace::authority::AuthorityError::WorktreeOwned
                    | crate::workspace::authority::AuthorityError::ActorAlreadyOwnsWorktree,
                ),
            ) => {
                return Err(FailureCode::Conflict);
            }
            Err(
                crate::workspace::durable::DurableError::Application(_)
                | crate::workspace::durable::DurableError::CorruptState,
            ) => {
                self.uncertain.insert(binding.clone());
                return Err(FailureCode::WorkspaceActivation);
            }
            Err(_) => return Err(FailureCode::WorkspaceActivation),
        };
        let activation_operation = receipt.operation().to_owned();
        self.grants.insert(binding.clone(), receipt);
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(error) => {
                if let Ok(mut guard) = self.shared.bindings.lock() {
                    let _ = guard.stop_binding(&binding);
                }
                return Err(self
                    .settle_revocation(&binding)
                    .await
                    .err()
                    .unwrap_or(error));
            }
        };
        let baseline = self
            .capture_activation_baseline(job, &authority, &activation_operation)
            .await;
        let launches = job.target.providers.clone();
        // A second concurrent actor on the same physical worktree cannot share a single-owner
        // namespace: fail its activation with the finite reason and roll its own grant back, so the
        // actor that already owns the cache keeps running and can hand off after it stops.
        if let Err(code) = self.retain_worktree_caches(&binding, &authority, &launches, true) {
            if let Ok(mut guard) = self.shared.bindings.lock() {
                let _ = guard.stop_binding(&binding);
            }
            return Err(self.settle_revocation(&binding).await.err().unwrap_or(code));
        }
        self.shared.active(&binding)?;
        let baseline = match baseline {
            Ok(baseline) => {
                let description = format!(
                    "partial ({:?}; durable capture {})",
                    baseline.window(),
                    baseline.capture_digest().is_some()
                );
                self.baselines.insert(binding.clone(), baseline);
                description
            }
            Err(_) => "unknown (durable capture unavailable)".to_owned(),
        };
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text: format!(
                    "Workspace activated; authority_epoch: {}; baseline: {baseline}; worktree_cache: retained. Provider readiness is not implied.",
                    authority.epoch(),
                ),
                detail_ref: Some(job.reference.clone()),
                truncated: false,
                continuation: false,
            },
            Some(authority),
            None,
        ))
    }

    /// Locks the daemon's single admission controller for one synchronous accounting call.
    ///
    /// The guard must never be held across an await: it is a `std` mutex shared with the helper
    /// socket task, and the worker task must stay `Send`. Every caller therefore takes it inside
    /// one statement.
    fn admission(&self) -> std::sync::MutexGuard<'_, crate::execution::AdmissionController> {
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Allocates one physical slot without waiting behind a retained idle backend; queued tickets are cancelled.
    fn admit(
        &mut self,
        binding: &BindingRef,
    ) -> Result<crate::execution::AdmissionLease, FailureCode> {
        use crate::execution::{Admission, AdmissionClass, OwnerId};
        let owner = OwnerId::new(
            blake3::Hash::from_bytes(binding.fingerprint())
                .to_hex()
                .to_string(),
        )
        .map_err(|_| FailureCode::Internal)?;
        match self.admission().submit(owner, AdmissionClass::Interactive) {
            Admission::Granted(lease) => Ok(lease),
            Admission::Queued(ticket) => {
                self.admission().cancel_ticket(ticket);
                Err(FailureCode::Capacity)
            }
            Admission::Refused(_) => Err(FailureCode::Capacity),
        }
    }

    /// Settles only a definite pre-child failure; every uncertain process error retains its reservation.
    fn spawn_failure(
        &mut self,
        error: crate::execution::ProcessError,
        binding: &BindingRef,
        method: crate::errorlog::Method,
    ) -> FailureCode {
        let detail = spawn_detail(&error);
        if let crate::execution::ProcessError::NeverStarted { settlement, .. } = error {
            if self.admission().settle_never_started(settlement).is_err() {
                self.uncertain.insert(binding.clone());
            }
        } else {
            self.uncertain.insert(binding.clone());
        }
        record_execution_profile(method, &detail);
        FailureCode::ExecutionProfileCause(
            ExecutionProfileCause::from_log_tag(&detail)
                .expect("spawn refusal has a closed execution-profile tag"),
        )
    }

    /// Returns only a current boot-fenced stamp after a fresh binding consume.
    async fn authority(&self, binding: &BindingRef) -> Result<AuthorityStamp, FailureCode> {
        let receipt = self
            .grants
            .get(binding)
            .ok_or(FailureCode::WorkspaceAuthority)?;
        let active = self.shared.active(binding)?;
        self.workspace
            .authority(receipt, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)
    }

    /// Reads and persists one registered file under fresh durable authority, preserving missing state.
    async fn observe(
        &mut self,
        binding: &BindingRef,
        path: std::path::PathBuf,
    ) -> Result<(SourceObservation, Vec<u8>), FailureCode> {
        use crate::workspace::{
            observation::{
                MAX_SOURCE_BYTES, ObservationError, ObservationRef, SourceCoverage,
                SourceReadLimits, SourceRevision, read_authorized_source,
            },
            store::{ObservationAdmission, ObservationDraft},
        };
        let authority = self.authority(binding).await?;
        // The source read ceiling is the v0.1 reader's own bound, not the launcher's discovery and
        // check-process output budget: `limits.output_bytes` sizes bounded command captures and is
        // far smaller than a source file may legitimately be.
        let read = read_authorized_source(
            authority.worktree(),
            &path,
            SourceReadLimits::new(1024, MAX_SOURCE_BYTES).map_err(|_| FailureCode::Internal)?,
        );
        self.source_sequence = self
            .source_sequence
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        let key = format!(
            "source-{}-{}",
            blake3::Hash::from_bytes(self.shared.nonce).to_hex(),
            self.source_sequence
        );
        let operation = OperationId::new(key.clone()).map_err(|_| FailureCode::Internal)?;
        let reference = ObservationRef::new(key).map_err(|_| FailureCode::Internal)?;
        let (draft, bytes) = match read {
            Ok(read) => {
                let revision =
                    SourceRevision::new(blake3::hash(read.contents()).to_hex().to_string())
                        .map_err(|_| FailureCode::Internal)?;
                (
                    ObservationDraft::present(
                        authority.worktree().clone(),
                        authority.epoch(),
                        operation,
                        reference,
                        path.clone(),
                        read.bytes().clone(),
                        revision,
                        SourceCoverage::Complete,
                    )
                    .map_err(|_| FailureCode::SourceUnavailable)?,
                    read.contents().to_vec(),
                )
            }
            Err(ObservationError::Missing) => (
                ObservationDraft::missing(
                    authority.worktree().clone(),
                    authority.epoch(),
                    operation,
                    reference,
                    path.clone(),
                    SourceRevision::new("missing").map_err(|_| FailureCode::Internal)?,
                    SourceCoverage::Complete,
                )
                .map_err(|_| FailureCode::SourceUnavailable)?,
                Vec::new(),
            ),
            Err(ObservationError::TooLarge { size }) => {
                return Err(FailureCode::SourceTooLarge {
                    size,
                    ceiling: MAX_SOURCE_BYTES as u64,
                });
            }
            Err(_) => return Err(FailureCode::SourceUnavailable),
        };
        let active = self.shared.active(binding)?;
        self.workspace
            .authorize(&authority, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let ObservationAdmission::Recorded(observed) = self
            .observations
            .record(draft)
            .await
            .map_err(|_| FailureCode::SourceUnavailable)?
        else {
            return Err(FailureCode::SourceUnavailable);
        };
        self.shared.active(binding)?;
        let paths = self.registered.entry(binding.clone()).or_default();
        if paths.len() >= self.shared.launcher.limits.details && !paths.contains(&path) {
            return Err(FailureCode::Capacity);
        }
        paths.insert(path);
        Ok((observed, bytes))
    }

    /// Reconciles native-hinted registered paths only when a new MCP call supplies current sandbox state.
    /// Hooks themselves never provide new read authority or authorize a scope from tool payload text.
    async fn reconcile_hints(&mut self, job: &Job) {
        let binding = job.invocation.binding_ref();
        let hinted = self
            .shared
            .bindings
            .lock()
            .ok()
            .and_then(|mut guard| guard.take_native_change_hint(binding).ok())
            .unwrap_or(false);
        if hinted {
            let paths = self.registered.get(binding).cloned().unwrap_or_default();
            for path in paths {
                if *job.cancel.borrow() || tokio::time::Instant::now() >= job.deadline {
                    break;
                }
                if self.observe(binding, path).await.is_err() {
                    break;
                }
            }
        }
    }

    /// Returns path-proven source context with explicit semantic or lexical provenance and bounded
    /// source text, falling back to lexical evidence when an accepted provider cannot run or cannot
    /// verify project resolution; source and freshness refusals remain errors. Managed semantic
    /// locations are proved per path and retained for later Inspect reauthorization under the live
    /// profile.
    async fn context(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        use crate::intelligence::context::{ContextMode, ContextQuery, lexical_context};
        let binding = job.invocation.binding_ref().clone();
        if job.parameters.get("kind").and_then(Value::as_str) == Some("problems") {
            return self.context_problems_job(job, &binding).await;
        }
        let path = job.parameters["path"]
            .as_str()
            .ok_or(FailureCode::SourceUnavailable)?
            .to_owned();
        let (observed, bytes) = self.observe(&binding, path.clone().into()).await?;
        let query = job
            .parameters
            .get("byte_offset")
            .and_then(Value::as_u64)
            .map_or(ContextQuery::File, |byte_offset| ContextQuery::Symbol {
                byte_offset: byte_offset as usize,
            });
        let semantic = if observed.bytes().is_none() {
            Ok(None)
        } else {
            self.semantic_context(job, &observed, &bytes, query).await
        };
        let (context, diagnostics)=match semantic {
            Ok(Some(result))=>(result.context, Some(result.diagnostics)),
            Ok(None)=>(lexical_context(&observed,&bytes,query,"no accepted provider is configured for this source, or the registered path is missing").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(FailureCode::ProviderUnavailable)=>(lexical_context(&observed,&bytes,query,"accepted semantic provider is unavailable").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(FailureCode::ProviderLoading)=>(lexical_context(&observed,&bytes,query,"semantic provider is still loading the workspace; repeat the call in a few seconds").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(FailureCode::ResolutionUnverified)=>(lexical_context(&observed,&bytes,query,"semantic project resolution is unverified").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(FailureCode::ExecutionProfile)=>(lexical_context(&observed,&bytes,query,"accepted semantic provider cannot run under the current execution profile").map_err(|_|FailureCode::SourceUnavailable)?, None),
            Err(code)=>return Err(code),
        };
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch {
            return Err(FailureCode::SourceUnavailable);
        }
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::Deadline);
        }
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        if !source_matches(&observed) {
            return Err(FailureCode::SourceUnavailable);
        }
        let mode = match &context.mode {
            ContextMode::Semantic => "semantic".to_owned(),
            ContextMode::Lexical { reason } => format!("lexical ({reason})"),
        };
        let diagnostics = diagnostics.and_then(|diagnostics| {
            (context.freshness == crate::intelligence::freshness::Freshness::Current
                && diagnostics.freshness == crate::intelligence::freshness::Freshness::Provisional
                && diagnostics.source.as_ref() == Some(&context.source)
                && Some(diagnostics.generation) == context.generation
                && context.document_version.is_some_and(|version| version > 0)
                && (diagnostics.document_version == context.document_version
                    || (diagnostics.document_version.is_none()
                        && diagnostics.readiness
                            == crate::intelligence::freshness::DiagnosticReadiness::Reported
                        && !diagnostics.diagnostics.is_empty())))
            .then_some(diagnostics)
        });
        let feedback = diagnostics
            .as_ref()
            .filter(|diagnostics| !diagnostics.diagnostics.is_empty())
            .map(|diagnostics| {
                let count = if diagnostics.document_version.is_none() {
                    format!("at least {}", diagnostics.diagnostics.len())
                } else {
                    diagnostics.diagnostics.len().to_string()
                };
                FeedbackDelta::new(
                    format!("Provider reported {count} diagnostics for this source generation."),
                    format!(
                        "source_sequence={}; provider_generation={:?}; document_version={:?}",
                        observed.sequence(),
                        diagnostics.generation,
                        diagnostics.document_version
                    ),
                    "Review the bounded diagnostic messages in the latest context result.",
                    "provisional push; source and generation matched; unversioned counts are lower bounds",
                    Some(job.reference.clone()),
                )
                .expect("fixed feedback envelope is bounded")
            });
        let diagnostic_text = diagnostics.as_ref().map_or_else(
            || "diagnostics_freshness: unknown\ndiagnostic_count: unknown\nfeedback_delta: none".to_owned(),
            |diagnostics| {
                let messages = diagnostics
                    .diagnostics
                    .iter()
                    .take(8)
                    .map(|diagnostic| diagnostic.message.chars().take(256).collect::<String>())
                    .collect::<Vec<_>>();
                let feedback = feedback
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), FeedbackDelta::render);
                let count = if diagnostics.document_version.is_none() {
                    format!("at_least_{}", diagnostics.diagnostics.len())
                } else {
                    diagnostics.diagnostics.len().to_string()
                };
                format!(
                    "diagnostics_freshness: {:?}\ndiagnostic_readiness: {:?}\ndiagnostic_count: {}\ndiagnostics_truncated: {}\ndiagnostic_messages: {}\nfeedback_delta: {feedback}",
                    diagnostics.freshness,
                    diagnostics.readiness,
                    count,
                    diagnostics.truncated || diagnostics.diagnostics.len() > messages.len(),
                    serde_json::to_string(&messages).unwrap_or_else(|_| "[]".into()),
                )
            },
        );
        // Provider locations are part of the reply too: retain and prove every secondary path.
        let mut provenance = BTreeSet::new();
        for location in context
            .definitions
            .iter()
            .flatten()
            .chain(context.references.iter().flatten())
        {
            let absolute = location
                .uri
                .to_file_path()
                .map_err(|_| FailureCode::SourceUnavailable)?;
            let relative = absolute
                .strip_prefix(authority.worktree().worktree_path())
                .map_err(|_| FailureCode::SourceUnavailable)?;
            provenance.insert(relative.to_path_buf());
        }
        let text = format!(
            "mode: {mode}\npath: {path}\nsource_state: {:?}\nsource_sequence: {}\nauthority_epoch: {}\ncoverage: complete registered path\nposition_encoding: {:?}\nprovider_generation: {:?}\ndocument_version: {:?}\n{diagnostic_text}\ndefinitions: {}\nreferences: {}\nlexical_matches: {}\n\n{}",
            observed.state(),
            observed.sequence(),
            authority.epoch(),
            context.position_encoding,
            context.generation,
            context.document_version,
            serde_json::to_string(&context.definitions).map_err(|_| FailureCode::Internal)?,
            serde_json::to_string(&context.references).map_err(|_| FailureCode::Internal)?,
            serde_json::to_string(&context.lexical_matches).map_err(|_| FailureCode::Internal)?,
            context.text
        );
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            if let Some(feedback) = feedback {
                let messages: Vec<&str> = diagnostics
                    .as_ref()
                    .map(|diagnostics| {
                        diagnostics
                            .diagnostics
                            .iter()
                            .map(|diagnostic| diagnostic.message.as_str())
                            .collect()
                    })
                    .unwrap_or_default();
                let identity = DeliveredIssue {
                    source_path: observed.path().to_path_buf(),
                    source_digest: observed
                        .bytes()
                        .map(|bytes| *bytes.digest())
                        .unwrap_or([0; 32]),
                    diagnostic_fingerprint: super::facade::diagnostic_fingerprint(&messages),
                };
                // A redundant Context job for the exact same unchanged issue already reached a
                // caller (inline or via the hook) — never re-arm it as a fresh undelivered fact
                // just because this job happened to run again.
                ledger.retain_feedback(
                    &binding,
                    NativeFeedback {
                        source: Some(observed.clone()),
                        text: feedback.render(),
                        native_epoch: epoch,
                        // Computing this job's reply is not submitting it: the caller may still
                        // only hold `Pending` until a later Inspect, or lose it to a deadline.
                        inline_delivered: false,
                        producer: job.reference.clone(),
                        identity,
                    },
                );
            } else {
                ledger.feedback.remove(&binding);
            }
        }
        let body_start = text.len() - context.text.len();
        let (reply, context_page) =
            ContextPageState::new(text, body_start, context.truncated, ResultKind::Context)
                .next(&job.reference)?;
        self.shared.set_context_page(&job.reference, context_page);
        self.shared.set_diff_provenance(&job.reference, provenance);
        Ok((reply, Some(authority), Some(observed)))
    }

    /// Returns the project problem feed for `kind: "problems"` from the configured snapshot source.
    ///
    /// The worktree is the fresh durable authority's worktree — the same active-binding/authority
    /// lookup every other context use requires — and no source file is read and no observation is
    /// recorded. Without an attached source, or when the requested language is not configured,
    /// the reply is the honest single line `checks disabled`. A Codex observation or accepted
    /// Claude operator profile without whole-tree read proof returns only
    /// `unavailable: read_restricted` language states and never consults cached problem snapshots.
    /// It carries no edit source reference.
    async fn context_problems_job(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::Deadline);
        }
        let authority = self.authority(binding).await?;
        let language = job
            .parameters
            .get("language")
            .and_then(Value::as_str)
            .and_then(parse_language);
        let offset = job
            .parameters
            .get("offset")
            .and_then(Value::as_u64)
            .map_or(0, |offset| u32::try_from(offset).unwrap_or(u32::MAX));
        let text = match self.shared.problem_source.as_ref() {
            Some(source) => {
                let restricted = self
                    .shared
                    .project_feed
                    .as_ref()
                    .is_some_and(|feed| feed.is_read_restricted(&binding.fingerprint()));
                let snapshots = if restricted {
                    self.shared
                        .project_feed
                        .as_ref()
                        .map_or_else(Vec::new, |feed| feed.read_restricted_snapshots())
                } else {
                    source.latest(authority.worktree().worktree_path())
                };
                let rechecks = if restricted {
                    Vec::new()
                } else {
                    source.rechecks(authority.worktree().worktree_path())
                };
                problems_text_with_rechecks(&snapshots, &rechecks, language, offset)
            }
            None => "checks disabled".to_owned(),
        };
        self.shared.active(binding)?;
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Context,
                text,
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Some(authority),
            None,
        ))
    }

    /// Executes one managed-Codex full-content edit through Changes receipts and Workspace permits.
    ///
    /// The source reference must name a completed same-binding Context detail whose exact source
    /// observation still matches `path`. A new durable prepare is the only route to Workspace; an
    /// exact prepared receipt recovered after ambiguity returns unknown and is never dispatched
    /// again. Known effects are followed by source observation and a deadline-bounded provider
    /// diagnostic refresh attached to the same reply only when it matches that post-read source;
    /// provider failure never changes a known filesystem outcome or implies cleanliness. The
    /// current host must prove this path readable before Workspace opens it during preparation.
    async fn edit(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        if job.parameters.get("symbol").is_some() || job.parameters.get("lines").is_some() {
            return self.edit_by_symbol(job).await;
        }
        let request: EditRequest =
            serde_json::from_value(job.parameters.clone()).map_err(|_| FailureCode::Internal)?;
        request.validate().map_err(|_| FailureCode::Internal)?;
        let binding = job.invocation.binding_ref().clone();
        let prepared = match self.edits.prepare(request.clone()).await {
            Ok(PrepareAdmission::Prepared(prepared)) => prepared,
            Ok(
                PrepareAdmission::Settled(result)
                | PrepareAdmission::ConflictingDuplicate(result)
                | PrepareAdmission::OutcomeUnknown(result),
            ) => {
                let authority = self.authority(&binding).await.ok();
                return Ok((
                    PeerReply::Edit {
                        result,
                        diagnostics: EditDiagnostics::Unknown {},
                    },
                    authority,
                    None,
                ));
            }
            Err(_) => {
                return Ok((
                    PeerReply::Edit {
                        result: EditResult {
                            operation_id: request.operation_id,
                            path: request.path,
                            outcome: ChangesEditOutcome::UnavailableBeforeDispatch,
                            source_ref: None,
                        },
                        diagnostics: EditDiagnostics::Unknown {},
                    },
                    None,
                    None,
                ));
            }
        };
        let source = self.shared.ledger.lock().ok().and_then(|ledger| {
            ledger.details.get(&request.source_ref).and_then(|detail| {
                admitted_edit_source(detail, &binding, &request.source_ref, &request.path)
            })
        });
        let Some(source) = source else {
            return self
                .settle_edit(
                    prepared,
                    &request,
                    crate::workspace::edit::EditOutcome::StaleSource,
                    None,
                    None,
                )
                .await;
        };
        self.edit_with_source(job, request, prepared, source).await
    }

    /// Writes one prepared edit whose base observation is already known: the full-file form
    /// resolves it from a retained detail, the symbol forms observe the file themselves.
    pub(super) async fn edit_with_source(
        &mut self,
        job: &mut Job,
        request: EditRequest,
        prepared: crate::changes::edit::PreparedEdit,
        source: SourceObservation,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = match self.authority(&binding).await {
            Ok(authority) => authority,
            Err(_) => {
                return self
                    .settle_edit(
                        prepared,
                        &request,
                        crate::workspace::edit::EditOutcome::CancelledNoEffect,
                        None,
                        None,
                    )
                    .await;
            }
        };
        let edit_source = match crate::workspace::edit::EditSourceRef::from_observation(&source) {
            Ok(source) => source,
            Err(outcome) => {
                return self
                    .settle_edit(prepared, &request, outcome, Some(authority), None)
                    .await;
            }
        };
        let active = match self.shared.active(&binding) {
            Ok(active) => active,
            Err(_) => {
                return self
                    .settle_edit(
                        prepared,
                        &request,
                        crate::workspace::edit::EditOutcome::CancelledNoEffect,
                        Some(authority),
                        None,
                    )
                    .await;
            }
        };
        let (permit, target) = match self
            .workspace
            .prepare_edit(
                &authority,
                &active,
                request.operation_id.clone(),
                request.path.clone().into(),
                edit_source.clone(),
            )
            .await
        {
            Ok(prepared) => prepared,
            Err(outcome) => {
                return self
                    .settle_edit(prepared, &request, outcome, Some(authority), None)
                    .await;
            }
        };
        let outcome = if *job.cancel.borrow() {
            crate::workspace::edit::EditOutcome::CancelledNoEffect
        } else if tokio::time::Instant::now() >= job.deadline {
            crate::workspace::edit::EditOutcome::DeadlineNoEffect
        } else {
            match self.shared.active(&binding) {
                Ok(active) => {
                    self.workspace
                        .replace_edit(
                            &authority,
                            &active,
                            permit,
                            target,
                            &edit_source,
                            request.content.as_bytes(),
                            || !*job.cancel.borrow() && tokio::time::Instant::now() < job.deadline,
                        )
                        .await
                }
                Err(_) => crate::workspace::edit::EditOutcome::CancelledNoEffect,
            }
        };
        let known = matches!(
            outcome,
            crate::workspace::edit::EditOutcome::Created(_)
                | crate::workspace::edit::EditOutcome::Replaced(_)
                | crate::workspace::edit::EditOutcome::Unchanged(_)
        );
        let post_read = match &outcome {
            crate::workspace::edit::EditOutcome::Created(read)
            | crate::workspace::edit::EditOutcome::Replaced(read)
            | crate::workspace::edit::EditOutcome::Unchanged(read) => Some(read),
            _ => None,
        };
        let (refreshed, diagnostics) = if known {
            match self.observe(&binding, request.path.clone().into()).await {
                Ok((observed, bytes)) => {
                    let current_epoch = self
                        .shared
                        .ledger
                        .lock()
                        .ok()
                        .and_then(|ledger| ledger.native_epoch.get(&binding).copied())
                        .unwrap_or(0);
                    if !post_read.is_some_and(|read| {
                        exact_post_read_observation(
                            read,
                            &observed,
                            &bytes,
                            job.native_epoch,
                            current_epoch,
                        )
                    }) {
                        (None, EditDiagnostics::Unknown {})
                    } else if let Some(deadline) = edit_diagnostic_deadline(job.deadline) {
                        let original = std::mem::replace(&mut job.deadline, deadline);
                        let result = self
                            .semantic_context(
                                job,
                                &observed,
                                &bytes,
                                crate::intelligence::context::ContextQuery::File,
                            )
                            .await;
                        job.deadline = original;
                        let diagnostics = match result {
                            Ok(Some(provider)) => EditDiagnostics::from_snapshot(
                                &provider.context,
                                &provider.diagnostics,
                            ),
                            Ok(None) | Err(_) => EditDiagnostics::Unknown {},
                        };
                        (Some(observed), diagnostics)
                    } else {
                        (Some(observed), EditDiagnostics::Unknown {})
                    }
                }
                Err(_) => (None, EditDiagnostics::Unknown {}),
            }
        } else {
            (None, EditDiagnostics::Unknown {})
        };
        // A provider report for the exact version is real and instant; keep it. Anything else is
        // verified by the project check the write scheduled: an empty provider publish is not
        // proof of cleanliness (rust-analyzer publishes its own diagnostics before cargo's), and
        // only the project check sees breakage the edit caused in other files.
        let diagnostics = match diagnostics {
            EditDiagnostics::CurrentReported { .. } => diagnostics,
            other if refreshed.is_some() => self
                .check_diagnostics(job, &authority, &request.path)
                .await
                .unwrap_or(other),
            other => other,
        };
        let post_reference = refreshed.as_ref().map(|_| job.reference.clone());
        let expected = EditResult::from_workspace(&request, outcome, |_| post_reference);
        let result = self
            .settle_prepared_edit(prepared, &request, expected.clone())
            .await;
        let diagnostics = if result == expected && result.outcome.has_post_source() {
            diagnostics
        } else {
            EditDiagnostics::Unknown {}
        };
        let source = (result.outcome.has_post_source())
            .then_some(refreshed)
            .flatten();
        Ok((
            PeerReply::Edit {
                result,
                diagnostics,
            },
            Some(authority),
            source,
        ))
    }

    /// Schedules the project check for the edit and waits (bounded) for its result, reporting
    /// the edited file's problems from it: `current_reported` with `path:line:col severity
    /// [code] message` lines, `current_clean` when the completed check names none, `unknown`
    /// when no check at the edit's generation completes in time. `None` when checks are not
    /// configured for this worktree or file type, so the caller keeps the provider's answer.
    async fn check_diagnostics(
        &self,
        job: &mut Job,
        authority: &AuthorityStamp,
        path: &str,
    ) -> Option<EditDiagnostics> {
        use crate::checks::{CheckState, Language as CheckLanguage, Severity};
        let feed = self.shared.project_feed.as_ref()?;
        let language = match std::path::Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
        {
            Some("rs") => CheckLanguage::Rust,
            Some("py" | "pyi") => CheckLanguage::Python,
            Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs") => {
                CheckLanguage::TypeScript
            }
            _ => return None,
        };
        let generation = feed.changed_generation(&job.invocation.binding_ref().fingerprint())?;
        job.check_scheduled = true;
        let worktree = authority.worktree().worktree_path().to_path_buf();
        let wanted = path.trim_start_matches("./");
        let deadline = job
            .deadline
            .checked_sub(EDIT_SETTLEMENT_RESERVE)?
            .min(tokio::time::Instant::now() + EDIT_CHECK_WAIT);
        loop {
            if *job.cancel.borrow() {
                return None;
            }
            let snapshot = feed
                .latest(&worktree)
                .into_iter()
                .find(|snapshot| snapshot.language == language);
            if let Some(snapshot) = snapshot
                && snapshot.input_generation >= generation
            {
                if !matches!(snapshot.state, CheckState::Ready | CheckState::Partial) {
                    return Some(EditDiagnostics::Unknown {});
                }
                let mut errors = 0u32;
                let mut warnings = 0u32;
                let mut messages = Vec::new();
                let mut truncated = snapshot.truncated;
                for problem in &snapshot.problems {
                    let reported = problem.path.trim_start_matches("./");
                    let reported = std::path::Path::new(reported)
                        .strip_prefix(&worktree)
                        .map(|relative| relative.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| reported.to_owned());
                    if reported != wanted {
                        continue;
                    }
                    match problem.severity {
                        Severity::Error => errors += 1,
                        Severity::Warning => warnings += 1,
                    }
                    if messages.len() < 8 {
                        let code = problem
                            .code
                            .as_deref()
                            .map(|code| format!("[{code}] "))
                            .unwrap_or_default();
                        let severity = match problem.severity {
                            Severity::Error => "error",
                            Severity::Warning => "warning",
                        };
                        let line = format!(
                            "{wanted}:{}:{} {severity} {code}{}",
                            problem.line, problem.column, problem.message
                        );
                        messages.push(line.chars().take(256).collect());
                    } else {
                        truncated = true;
                    }
                }
                return Some(if messages.is_empty() && !truncated {
                    EditDiagnostics::CurrentClean {}
                } else {
                    EditDiagnostics::CurrentReported {
                        messages,
                        delta: format!(
                            "project check {:.1}s: {errors} errors, {warnings} warnings in this file",
                            snapshot.duration_ms as f64 / 1000.0
                        ),
                        truncated,
                    }
                });
            }
            if tokio::time::Instant::now() >= deadline {
                return Some(EditDiagnostics::Unknown {});
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Durably settles one typed pre-effect Workspace outcome without dispatching a write.
    async fn settle_edit(
        &self,
        prepared: crate::changes::edit::PreparedEdit,
        request: &EditRequest,
        outcome: crate::workspace::edit::EditOutcome,
        authority: Option<AuthorityStamp>,
        source: Option<SourceObservation>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let result = EditResult::from_workspace(request, outcome, |_| None);
        let result = self.settle_prepared_edit(prepared, request, result).await;
        Ok((
            PeerReply::Edit {
                result,
                diagnostics: EditDiagnostics::Unknown {},
            },
            authority,
            source,
        ))
    }

    /// Settles a consumed receipt and reconciles ambiguity without discarding an already durable result.
    async fn settle_prepared_edit(
        &self,
        prepared: crate::changes::edit::PreparedEdit,
        request: &EditRequest,
        result: EditResult,
    ) -> EditResult {
        match self.edits.settle(prepared, result).await {
            Ok(result) => result,
            Err(_) => match self.edits.prepare(request.clone()).await {
                Ok(
                    PrepareAdmission::Settled(result)
                    | PrepareAdmission::ConflictingDuplicate(result)
                    | PrepareAdmission::OutcomeUnknown(result),
                ) => result,
                _ => EditResult {
                    operation_id: request.operation_id.clone(),
                    path: request.path.clone(),
                    outcome: ChangesEditOutcome::OutcomeUnknown,
                    source_ref: None,
                },
            },
        }
    }

    /// Settles one stop by positively closing this binding's providers first and only then durably
    /// revoking its grant, so a failed durable half keeps full retry authority.
    ///
    /// On durable failure the receipt, this binding's cache keys (still non-quiescent), and its
    /// registered paths are all deliberately retained and the binding is marked pending, so a later
    /// fresh start can commit the same revoke before minting a new grant. Nothing here restores the
    /// stopped binding's source or provider authority and no stop is ever replayed: the host
    /// binding was already stopped by the ingress path and stays unusable either way. A daemon
    /// restart boot-fences old grants independently, so pending state is intentionally in-memory.
    async fn settle_revocation(&mut self, binding: &BindingRef) -> Result<(), FailureCode> {
        self.close_provider(binding).await?;
        let Some(receipt) = self.grants.get(binding).cloned() else {
            self.pending_revocations.remove(binding);
            self.release_binding_state(binding);
            return Ok(());
        };
        let operation = OperationId::new(format!(
            "stop-{}",
            blake3::Hash::from_bytes(binding.fingerprint()).to_hex()
        ))
        .map_err(|_| FailureCode::Internal)?;
        if self
            .workspace
            .revoke(operation, &receipt, StopBindingHandoff::Confirmed)
            .await
            .is_err()
        {
            if self.pending_revocations.len() < MAX_PENDING_REVOCATIONS {
                self.pending_revocations.insert(binding.clone());
            }
            return Err(FailureCode::WorkspaceAuthority);
        }
        self.grants.remove(binding);
        self.pending_revocations.remove(binding);
        self.release_live_rust(binding).await;
        self.release_binding_state(binding);
        Ok(())
    }

    /// Releases the binding-owned state that only a committed durable revoke makes safe to clear.
    fn release_binding_state(&mut self, binding: &BindingRef) {
        self.quiesce_worktree_caches(binding);
        self.registered.remove(binding);
        self.baselines.remove(binding);
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            ledger.feedback.remove(binding);
            ledger.delivered.remove(binding);
        }
    }

    /// Commits a still-pending revoke that this fresh start would otherwise race, before any new
    /// grant is minted for the same canonical worktree or the same incoming actor.
    ///
    /// This is cleanup-only receipt recovery: it can only retry the exact stop operation the failed
    /// settlement already derived from that binding, never mint or restore authority.
    async fn reconcile_pending_revocations(
        &mut self,
        tree: &crate::workspace::authority::WorktreeRef,
        actor: &str,
    ) {
        let pending = self
            .pending_revocations
            .iter()
            .filter(|binding| {
                self.grants.get(*binding).is_some_and(|receipt| {
                    receipt.worktree().id() == tree.id() || receipt.actor() == actor
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for binding in pending {
            let _ = self.settle_revocation(&binding).await;
        }
    }

    /// Revokes a recoverable receipt after host stop; absent grants are explicitly harmless.
    async fn revoke(
        &mut self,
        binding: &BindingRef,
        _reference: &str,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        self.settle_revocation(binding).await?;
        if self.uncertain.contains(binding) {
            return Err(FailureCode::Internal);
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text:
                    "Assistance stopped for this binding; compatible shared peers remain eligible"
                        .into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            None,
            None,
        ))
    }
}

/// Aborts the independently scheduled inspection task if the owning worker task exits or panics,
/// so no detached inspector can outlive the worker it was coupled to.
struct AbortOnDrop(tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    /// Aborts the held inspection task; runs on scope exit, unwind, or the outer worker task's own abort.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Services queued inspections on its own schedule, decoupled from the worker's job loop, so a
/// synchronous, non-yielding job poll cannot delay a pending `ide.inspect` reply.
async fn inspection_loop(
    workspace: DurableWorkspace<'static>,
    shared: Arc<Shared>,
    mut inspections: mpsc::Receiver<Inspection>,
) {
    while let Some(request) = inspections.recv().await {
        serve_inspection(&workspace, &shared, request).await;
    }
}

/// Delivers only a same-binding result after fresh durable authorization and liveness checks.
/// An Activation status or completed Diff with recorded empty provenance contains no worktree
/// source paths or bytes and needs no read-path proof; other path-less details retain the
/// whole-tree proof requirement. A semantic Context also proves each retained definition and
/// reference path, so a newly denied secondary file invalidates its cached page.
async fn serve_inspection(workspace: &DurableWorkspace<'_>, shared: &Shared, request: Inspection) {
    let result = async {
        let active = shared.active(&request.binding)?;
        let (
            reply,
            authority,
            source,
            native_epoch,
            diff_page,
            diff_page_fresh,
            context_page,
            context_page_fresh,
            diff_provenance,
        ) = {
            let ledger = shared.ledger.lock().map_err(|_| FailureCode::Internal)?;
            let detail = ledger
                .details
                .get(&request.reference)
                .filter(|detail| detail.binding == request.binding)
                .ok_or(FailureCode::InvalidDetail)?;
            if request
                .expected
                .as_ref()
                .is_some_and(|expected| expected != &detail.selection)
            {
                return Err(FailureCode::InvalidDetail);
            }
            (
                detail.reply.clone(),
                detail.authority.clone(),
                detail.source.clone(),
                detail.native_epoch,
                detail.diff_page.clone(),
                detail.diff_page_fresh,
                detail.context_page.clone(),
                detail.context_page_fresh,
                detail.diff_provenance.clone(),
            )
        };
        // Ownership of this exact reference is established above, so releasing its retained page is
        // safe here and nowhere earlier. Every permanent invalidation below releases that heavy
        // evidence: the authority/native epoch it was captured under can never return, so retaining
        // megabytes of snapshot bytes would only pin memory against the aggregate ceiling. Only the
        // compact retry outcome the caller receives survives.
        let invalidate = |code: FailureCode| {
            shared.set_diff_page(&request.reference, None);
            shared.set_context_page(&request.reference, None);
            code
        };
        if let Some(authority) = &authority {
            workspace
                .authorize(authority, &active)
                .await
                .map_err(|_| invalidate(FailureCode::WorkspaceAuthority))?;
            // Cached disclosure adds the conservative lstat preflight: a symlink component below
            // the worktree root refuses disclosure of cached bytes. The real guard for later reads
            // stays the descriptor-relative `O_NOFOLLOW` reader; a preflight can never secure a
            // later read.
            for path in source
                .as_ref()
                .map(|source| source.path())
                .into_iter()
                .chain(
                    diff_provenance
                        .as_ref()
                        .map(|paths| paths.iter().map(PathBuf::as_path))
                        .into_iter()
                        .flatten(),
                )
            {
                symlink_disclosure_preflight(authority.worktree(), path).map_err(invalidate)?;
            }
        }
        if let Some(source) = source
            && !source_matches(&source)
        {
            return Err(invalidate(FailureCode::SourceUnavailable));
        }
        if matches!(
            reply,
            PeerReply::Complete {
                kind: ResultKind::Context | ResultKind::Diff,
                ..
            }
        ) && shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&request.binding)
            .copied()
            .unwrap_or(0)
            != native_epoch
        {
            // Same wire code as a changed source; the journal names which fence fired.
            crate::errorlog::record(
                crate::errorlog::Method::Inspect,
                crate::errorlog::Outcome::Failed,
                crate::errorlog::Fields {
                    reason: Some(FailureCode::SourceUnavailable.into()),
                    correlation: Some(request.reference.as_str()),
                    detail: Some("native_epoch_advanced"),
                    ..Default::default()
                },
            );
            return Err(invalidate(FailureCode::SourceUnavailable));
        }
        let active = shared.active(&request.binding)?;
        if let Some(authority) = &authority {
            workspace
                .authorize(authority, &active)
                .await
                .map_err(|_| invalidate(FailureCode::WorkspaceAuthority))?;
        }
        shared.active(&request.binding)?;
        if let Some(page) = diff_page {
            if diff_page_fresh {
                // `reply` already holds this exact page's composed text, produced by the job
                // (page 1) or a prior expansion, and no caller has retrieved it yet. Hand it over
                // unchanged; only a later inspection may advance past it.
                if let Ok(mut ledger) = shared.ledger.lock()
                    && let Some(detail) = ledger.details.get_mut(&request.reference)
                {
                    detail.diff_page_fresh = false;
                }
                return Ok::<_, FailureCode>(reply);
            }
            let Some(authority) = &authority else {
                return Err(FailureCode::WorkspaceAuthority);
            };
            let expected_scope =
                crate::workspace::git::GitScope::from_authority(authority, page.mode());
            // Revalidate the retained evidence's working-tree material against the current
            // worktree before trusting it: an out-of-band edit with no native hook never bumps
            // native_epoch, so that check alone cannot catch it. Staged-only comparisons never
            // depend on working-tree bytes, so this is skipped rather than used as unrelated
            // "proof" for them.
            if !page.working_tree_bytes_unchanged(authority.worktree()) {
                return Err(invalidate(FailureCode::SourceUnavailable));
            }
            // Expansion uses exactly the same whole-page fitting path as the initial composition,
            // so reply text always serializes under the bounded envelope without
            // `PeerReply::encode` needing to shrink it: a shrink cuts at a UTF-8 boundary, not a
            // hunk boundary, which would silently deliver a partial hunk while the cursor advanced
            // past it as if it were whole.
            let (advanced, next) = snapshots::fit_diff_page(
                page.mode(),
                authority.epoch(),
                &request.reference,
                page.budget().max_hunks,
                true,
                |max_hunks| page.expand_with_max_hunks(&expected_scope, max_hunks),
            )
            .map_err(|code| match code {
                // A structurally unavailable or failed selection can never be repaired by a later
                // page.
                FailureCode::SourceUnavailable => invalidate(code),
                // A budget refusal delivered nothing, so the retained evidence stays: dropping the
                // continuation here would lose hunks the caller can still reach later.
                code => code,
            })?;
            let encoded = next
                .clone()
                .encode()
                .and_then(|value| PeerReply::decode(value.as_str()));
            let Some(next) = encoded else {
                shared.set_diff_page(&request.reference, None);
                return Err(FailureCode::Internal);
            };
            if let Ok(mut ledger) = shared.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&request.reference)
            {
                detail.reply = next.clone();
                detail.diff_page = page.advance(&advanced);
                detail.diff_page_fresh = false;
            }
            return Ok::<_, FailureCode>(next);
        }
        if let Some(page) = context_page {
            if context_page_fresh {
                // Same first-page semantics as the Diff branch above: page one was already
                // composed and stored by the job; hand it over unchanged and let a later call
                // advance past it.
                if let Ok(mut ledger) = shared.ledger.lock()
                    && let Some(detail) = ledger.details.get_mut(&request.reference)
                {
                    detail.context_page_fresh = false;
                }
                return Ok::<_, FailureCode>(reply);
            }
            // Staleness is already fully covered above (source bytes and native epoch). A
            // Context detail always retains `source`, so the generic `source_matches` check
            // already ran; a Claude-captured Diff page is a frozen text snapshot with no `source`
            // and no re-derivable Git cursor (unlike the managed `diff_page` branch above), so
            // later pages of it need no further working-tree re-check either — exactly like Context.
            let (next, next_page) = page.next(&request.reference)?;
            if let Ok(mut ledger) = shared.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&request.reference)
            {
                detail.reply = next.clone();
                detail.context_page = next_page;
                detail.context_page_fresh = false;
            }
            return Ok::<_, FailureCode>(next);
        }
        Ok::<_, FailureCode>(reply)
    }
    .await;
    let reply = result.unwrap_or_else(|code| PeerReply::Error { code });
    // This is the actual submission boundary for the managed path: `reply` is about to be handed
    // to the real caller of either the initial `submit()` or a later `ide.inspect`. Marking must
    // wait for the send's own outcome — a request whose receiving side already closed must not
    // have its fact treated as delivered — and must trace the exact reference that produced the
    // retained fact, since a same-epoch, different-detail Context job can have overwritten the
    // single per-binding slot before this reply was ever composed.
    let is_context = matches!(
        reply,
        PeerReply::Complete {
            kind: ResultKind::Context,
            ..
        }
    );
    if request.reply.send(reply.clone()).is_ok() && is_context {
        shared.mark_feedback_inline_delivered(&request.binding, &request.reference, &reply);
    }
}

/// Reserves live inspection capacity, distinguishing saturation from service termination.
fn reserve_inspection(
    sender: &mpsc::Sender<Inspection>,
) -> Result<mpsc::OwnedPermit<Inspection>, FailureCode> {
    sender
        .clone()
        .try_reserve_owned()
        .map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => FailureCode::Capacity,
            mpsc::error::TrySendError::Closed(_) => FailureCode::Internal,
        })
}

/// Reserves the initial inspection slot before invoking the closure that publishes a job/detail.
fn admit_initial_inspection(
    sender: &mpsc::Sender<Inspection>,
    enqueue: impl FnOnce() -> Result<String, FailureCode>,
) -> Result<(String, mpsc::OwnedPermit<Inspection>), FailureCode> {
    let permit = reserve_inspection(sender)?;
    let reference = enqueue()?;
    Ok((reference, permit))
}

/// Rechecks one exact registered path; a missing source never establishes worktree closure.
fn source_matches(source: &SourceObservation) -> bool {
    use crate::workspace::observation::{
        MAX_SOURCE_BYTES, ObservationError, ObservedState, SourceReadLimits, read_authorized_source,
    };
    match read_authorized_source(
        source.worktree(),
        source.path(),
        SourceReadLimits::new(1024, MAX_SOURCE_BYTES).expect("fixed source limits"),
    ) {
        Ok(read) => source.bytes() == Some(read.bytes()),
        Err(ObservationError::Missing) => source.state() == ObservedState::Missing,
        Err(_) => false,
    }
}

/// Returns the exact same-binding source eligible to authorize a replacement edit.
///
/// Context and a successful prior Edit are the only source-producing details. A prior Edit must
/// name this exact reference as its post-read source and retain a source observation for the same
/// requested path; every other detail, missing observation, mismatched binding, or failed edit is
/// rejected as stale rather than being used to authorize bytes the caller has not observed. A
/// Context whose pages are not all delivered yet is likewise rejected: its reference denotes the
/// full observed source, but the caller has only seen part of it (T16B).
fn admitted_edit_source(
    detail: &Detail,
    binding: &BindingRef,
    reference: &str,
    path: &str,
) -> Option<SourceObservation> {
    (detail.binding == *binding
        // A Context source_ref names the complete observed source, but its pages are the only
        // view the caller has: while any page is still undelivered the caller has not observed
        // the whole file, so a full-content replace built on it could silently truncate it (T16B).
        && detail.context_page.is_none()
        && matches!(
            detail.selection.0,
            AssistanceTool::Context | AssistanceTool::Edit
        )
        && match &detail.reply {
            PeerReply::Complete {
                kind: ResultKind::Context,
                ..
            } => true,
            PeerReply::Edit { result, .. } => {
                result.source_ref.as_deref() == Some(reference) && result.outcome.has_post_source()
            }
            _ => false,
        })
    .then(|| detail.source.clone())
    .flatten()
    .filter(|source| source.path().to_str() == Some(path))
}

/// Proves a refreshed observation is the exact Workspace post-read and no native write intervened.
///
/// The raw bytes, bounded source metadata, and relative path must all equal the descriptor-bound
/// post-read. The current native epoch must still be the epoch captured when this edit began; a
/// post-hook that reports an intervening native write makes the observation unusable even if the
/// bytes happen to match again. Callers must return unknown diagnostics and no source reference
/// when this returns `false`.
fn exact_post_read_observation(
    post_read: &crate::workspace::edit::EditPostRead,
    observed: &SourceObservation,
    bytes: &[u8],
    edit_epoch: u64,
    current_epoch: u64,
) -> bool {
    post_read.path() == observed.path()
        && observed.bytes() == Some(post_read.bytes())
        && exact_post_read_bytes(post_read.contents(), bytes, edit_epoch, current_epoch)
}

/// Compares the exact Workspace post-read bytes and the native-write fence without reopening I/O.
///
/// This small pure predicate keeps the race boundary deterministic in tests: an intervening native
/// write changes either the refreshed bytes or the hook epoch, and neither case can chain a source
/// reference even if a later writer restores the original content.
fn exact_post_read_bytes(
    post_read: &[u8],
    refreshed: &[u8],
    edit_epoch: u64,
    current_epoch: u64,
) -> bool {
    post_read == refreshed && current_epoch == edit_epoch
}

/// Returns the latest deadline a best-effort edit diagnostic refresh may consume.
///
/// `None` means the remaining operation lifetime belongs to known-result settlement, so callers
/// must skip diagnostic work and return `unknown` without changing the completed edit outcome.
fn edit_diagnostic_deadline(deadline: tokio::time::Instant) -> Option<tokio::time::Instant> {
    let diagnostic_deadline = deadline.checked_sub(EDIT_SETTLEMENT_RESERVE)?;
    (diagnostic_deadline > tokio::time::Instant::now()).then_some(diagnostic_deadline)
}

/// Proves an intervening native write cannot reuse a post-edit source reference or diagnostics.
#[test]
fn intervening_native_write_rejects_post_read_source_chaining() {
    assert!(exact_post_read_bytes(
        b"workspace post-read",
        b"workspace post-read",
        7,
        7
    ));
    assert!(!exact_post_read_bytes(
        b"workspace post-read",
        b"native write",
        7,
        8
    ));
    assert!(
        !exact_post_read_bytes(b"workspace post-read", b"workspace post-read", 7, 8),
        "a native post between Workspace post-read and observation fences even restored bytes"
    );
}

/// Proves diagnostics surrender the settlement reserve instead of consuming a known edit deadline.
#[test]
fn diagnostics_reserve_known_edit_settlement_time() {
    let now = tokio::time::Instant::now();
    let deadline = now + EDIT_SETTLEMENT_RESERVE + Duration::from_millis(50);
    assert!(edit_diagnostic_deadline(deadline).is_some());
    assert!(edit_diagnostic_deadline(now + Duration::from_millis(1)).is_none());
}

/// Resolves the directory an activation works from and admits it against `allowed_roots`.
///
/// The model's `root` parameter wins over the launcher candidate. The result is the canonical
/// path, so discovery and every later comparison use one spelling. An empty `allowed_roots`, a
/// root outside every configured entry, or an unresolvable root refuses activation.
fn activation_root(job: &Job, allowed_roots: &[PathBuf]) -> Result<PathBuf, FailureCode> {
    let requested = job
        .parameters
        .get("root")
        .and_then(Value::as_str)
        .map_or_else(|| job.target.candidate.clone(), PathBuf::from);
    crate::assistance::launcher::admit_worktree(allowed_roots, &requested)
        .map_err(|_| FailureCode::OutsideAllowedRoots)
}

/// Refuses a discovered Git worktree root or common directory outside every allowed root.
///
/// Git may resolve a candidate to a repository above it, or a linked worktree to a common
/// directory elsewhere; neither implicitly authorizes the other location.
fn admit_discovered(
    allowed_roots: &[PathBuf],
    root: &Path,
    common: &Path,
) -> Result<(), FailureCode> {
    for path in [root, common] {
        crate::assistance::launcher::admit_worktree(allowed_roots, path)
            .map_err(|_| FailureCode::OutsideAllowedRoots)?;
    }
    Ok(())
}

/// Refuses any existing symlink component of one relative path below the worktree root (T36B).
///
/// This is a cached-disclosure preflight only — it cannot secure a later read against
/// replacement races, which stays the descriptor-relative `O_NOFOLLOW` reader's job. The walk
/// checks each existing prefix, stops at the first missing component (nothing deeper can
/// exist, and a missing target discloses nothing), and treats every other inspection failure
/// as unproven.
fn symlink_disclosure_preflight(
    worktree: &crate::workspace::authority::WorktreeRef,
    path: &Path,
) -> Result<(), FailureCode> {
    let mut checked = worktree.worktree_path().to_path_buf();
    for component in path.components() {
        checked.push(component);
        match std::fs::symlink_metadata(&checked) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(FailureCode::ExecutionProfile);
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(FailureCode::ExecutionProfile),
        }
    }
    Ok(())
}

/// Names one owned-child launch failure by its closed [`crate::execution::ProcessError`] variant
/// tag; a `NeverStarted` wrapper is unwrapped to its definite cause. Never the OS error string.
fn spawn_detail(error: &crate::execution::ProcessError) -> String {
    use crate::execution::ProcessError;
    format!(
        "spawn:{}",
        match error {
            ProcessError::Io(_) => "io",
            ProcessError::Request(_) => "request",
            ProcessError::ProtocolStdoutReserved => "protocol_stdout_reserved",
            ProcessError::ReapTimedOut => "reap_timed_out",
            ProcessError::NeverStarted { cause, .. } => {
                return spawn_detail(cause);
            }
        }
    )
}

/// Maps one assistance tool onto the closed error-log method tag (T24B).
fn errorlog_method(tool: AssistanceTool) -> crate::errorlog::Method {
    match tool {
        AssistanceTool::Start => crate::errorlog::Method::Start,
        AssistanceTool::Context => crate::errorlog::Method::Context,
        AssistanceTool::Diff => crate::errorlog::Method::Diff,
        AssistanceTool::Inspect => crate::errorlog::Method::Inspect,
        AssistanceTool::Stop => crate::errorlog::Method::Stop,
        AssistanceTool::Edit => crate::errorlog::Method::Edit,
        AssistanceTool::Outline => crate::errorlog::Method::Outline,
        AssistanceTool::Read => crate::errorlog::Method::Read,
        AssistanceTool::Symbol => crate::errorlog::Method::Symbol,
        AssistanceTool::Test => crate::errorlog::Method::Test,
    }
}

/// Chooses the first implemented language project detected at the worktree root.
fn detect_test_language(root: &Path) -> Option<crate::lang::Language> {
    [
        crate::lang::Language::Rust,
        crate::lang::Language::Python,
        crate::lang::Language::TypeScript,
        crate::lang::Language::Go,
    ]
    .into_iter()
    .find(|language| {
        crate::lang::support(*language)
            .and_then(|support| support.detect(root))
            .is_some()
    })
}

/// Resolves a file or filter target through the detected language's existing runner contract.
fn test_selection(
    root: &Path,
    target: crate::lang::TestTarget,
) -> Result<(Vec<String>, crate::lang::Language, Option<usize>), crate::lang::LangError> {
    for language in [
        crate::lang::Language::Rust,
        crate::lang::Language::Python,
        crate::lang::Language::TypeScript,
        crate::lang::Language::Go,
    ] {
        let Some(support) = crate::lang::support(language) else {
            continue;
        };
        let Some(project) = support.detect(root) else {
            continue;
        };
        let selection = support.test_selection(&project, &target)?;
        let count = (!selection.tests.is_empty()).then_some(selection.tests.len());
        return Ok((selection.command, language, count));
    }
    Err(crate::lang::LangError::Unsupported(
        "no supported test runner was detected".to_owned(),
    ))
}

/// Formats an argv vector for the compact test status line without shell interpretation.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let printable = arg
                .chars()
                .map(|character| {
                    if character.is_control() {
                        character.escape_default().to_string()
                    } else {
                        character.to_string()
                    }
                })
                .collect::<String>();
            if printable
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_./:-".contains(&byte))
            {
                printable
            } else {
                format!("'{}'", printable.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keeps runner-provided failure labels and messages on bounded control-free reply lines.
fn test_text_line(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect()
}

/// Renders the bounded parsed test result and actionable rerun/detail references.
fn test_result_text(id: u64, result: &super::tests::RunResult, owns_detail: bool) -> String {
    let report = &result.report;
    let mut text =
        if report.passed == 0 && report.failed == 0 && report.incomplete && !result.stopped {
            format!(
                "tests #{id}: no summary parsed, {} s",
                result.elapsed.as_secs()
            )
        } else if result.stopped {
            format!(
                "tests #{id}: stopped at budget {} s — {} passed, {} failed so far",
                result.budget.as_secs(),
                report.passed,
                report.failed
            )
        } else {
            format!(
                "tests #{id}: {} passed, {} failed, {} s",
                report.passed,
                report.failed,
                result.elapsed.as_secs()
            )
        };
    for failure in report.failures.iter().take(8) {
        text.push_str(&format!("\n  FAIL {}", test_text_line(&failure.name, 160)));
        if let Some((path, line)) = &failure.location {
            text.push_str(&format!(
                "\n       {}:{}  {}",
                test_text_line(&path.display().to_string(), 128),
                line,
                test_text_line(&failure.message, 240)
            ));
        } else {
            text.push_str(&format!(
                "\n       {}",
                test_text_line(&failure.message, 240)
            ));
        }
    }
    text.push_str(&format!("\n  rerun: {}", display_argv(&result.command)));
    if owns_detail {
        text.push_str(&format!(
            "\n  full output: ide.inspect {}",
            result.detail_ref
        ));
    }
    text
}

/// Records one execution-profile refusal with its closed condition detail, best-effort.
fn record_execution_profile(method: crate::errorlog::Method, detail: &str) {
    crate::errorlog::record(
        method,
        crate::errorlog::Outcome::Failed,
        crate::errorlog::Fields {
            reason: Some(crate::errorlog::ReasonCode::ExecutionProfile),
            detail: Some(detail),
            ..Default::default()
        },
    );
}

/// Classifies one job's terminal error reply into its closed error-log outcome, mirroring the
/// caller-view classification in `telemetry::adapters` so a background job failure and the same
/// failure observed through a later poll can never disagree (T26B).
fn job_failure_outcome(code: FailureCode) -> crate::errorlog::Outcome {
    match code {
        FailureCode::Cancelled => crate::errorlog::Outcome::Cancelled,
        FailureCode::Deadline => crate::errorlog::Outcome::Incomplete,
        _ => crate::errorlog::Outcome::Failed,
    }
}

/// Binds detail reuse to the exact closed query (including diff mode and context byte offset).
fn selection(parameters: &Value) -> [u8; 32] {
    let mut selected = parameters.clone();
    if let Some(object) = selected.as_object_mut() {
        object.remove("detail_ref");
    }
    *blake3::hash(selected.to_string().as_bytes()).as_bytes()
}

/// Saturated initial inspection admission must not invoke job/detail publication, while a closed
/// inspection service is an internal lifecycle fault rather than live capacity exhaustion.
#[test]
fn initial_inspection_admission_is_atomic_and_distinguishes_closure() {
    let (sender, receiver) = mpsc::channel(1);
    let held = sender.clone().try_reserve_owned().unwrap();
    let mut published = false;
    let result = admit_initial_inspection(&sender, || {
        published = true;
        Ok("lost-detail".to_owned())
    });
    assert!(matches!(result, Err(FailureCode::Capacity)));
    assert!(!published);

    drop(held);
    drop(receiver);
    let result = admit_initial_inspection(&sender, || panic!("closed service published detail"));
    assert!(matches!(result, Err(FailureCode::Internal)));
}

/// A queued or executing job defers idle shutdown: `is_processing` must report both counts from
/// one consistent ledger snapshot, and report idle again once neither applies (T26B).
#[test]
fn processing_reports_in_flight_and_queued_jobs() {
    let launcher = LauncherConfig::parse(
        br#"{"version":1,"limits":{"queued":1,"details":1,"operation_ms":1000,"output_bytes":1024},"targets":[]}"#,
    )
    .unwrap();
    let handle = WorkerHandle::new(
        Arc::new(Mutex::new(HostBindingGuard::default())),
        launcher,
        [3; 32],
        Arc::new(Mutex::new(admission_controller())),
    );
    assert!(
        !handle.is_processing(),
        "an idle worker must not defer idle shutdown"
    );
    handle.shared.ledger.lock().unwrap().in_flight = 1;
    assert!(
        handle.is_processing(),
        "an executing job must defer idle shutdown"
    );
    handle.shared.ledger.lock().unwrap().in_flight = 0;
    assert!(!handle.is_processing());
    // No task was ever started, so dropping the handle cancels nothing.
    drop(handle);
}

/// A job's terminal error line must classify every closed failure the way the caller-view adapter
/// does, so a background failure and its polled view can never disagree (T26B).
#[test]
fn job_failure_outcome_covers_every_terminal_error_class() {
    for (code, expected) in [
        (FailureCode::Cancelled, crate::errorlog::Outcome::Cancelled),
        (FailureCode::Deadline, crate::errorlog::Outcome::Incomplete),
        (
            FailureCode::SourceUnavailable,
            crate::errorlog::Outcome::Failed,
        ),
        (FailureCode::Internal, crate::errorlog::Outcome::Failed),
        (
            FailureCode::WorkspaceAuthority,
            crate::errorlog::Outcome::Failed,
        ),
    ] {
        assert_eq!(job_failure_outcome(code), expected, "{code:?}");
    }
}

/// KSC3: a real SQLite writer-lock failure on stop must retain the receipt/pending-recovery marker
/// and non-quiescent cache instead of returning a client `Deadline`, and a later fresh Start for the
/// same worktree/actor must commit that pending revoke before minting its own grant.
#[cfg(test)]
mod stop_retry_tests {
    use super::*;
    use crate::{
        app::{
            cache::{CacheNamespaceId, CacheRoot},
            config::StoreConfig,
        },
        assistance::host_binding::{
            BindingStatus, parse_candidate, parse_channel_session, parse_hook_event,
        },
        checks::{CheckState, Language, Problem, ProblemSnapshot, Severity},
        intelligence::freshness::{CacheIdentity, CacheLifecycle},
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Separates disposable fixture roots within the current process.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// Owns the exact temporary worktree/database pair used by one stop-retry test.
    struct Fixture {
        base: std::path::PathBuf,
        root: std::path::PathBuf,
    }
    impl Fixture {
        /// Creates real directories beneath `/private/tmp`, avoiding Darwin's `/tmp` symlink alias.
        fn new() -> Self {
            let base = std::path::PathBuf::from(format!(
                "/private/tmp/worker-stop-retry-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let root = base.join("worktree");
            std::fs::create_dir_all(&root).unwrap();
            assert!(
                std::process::Command::new("/usr/bin/git")
                    .args(["init", "--quiet"])
                    .current_dir(&root)
                    .status()
                    .unwrap()
                    .success()
            );
            Self { base, root }
        }
        /// Opens the real fixture Store through the existing effective-config path, with a
        /// store_busy_timeout well below the 800ms host stop window used by `WorkerHandle::stop`.
        fn store(&self) -> Store {
            Store::open_with_backup_root(
                &self.base.join("state.sqlite"),
                &self.base.join("backups"),
                StoreConfig {
                    queue_capacity: 16,
                    busy_timeout: Duration::from_millis(150),
                    request_deadline: Duration::from_secs(2),
                    receipt_capacity: 64,
                },
            )
            .unwrap()
        }
    }
    impl Drop for Fixture {
        /// Removes only this fixture's uniquely owned directory tree.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// Creates one current host binding for a managed job.
    fn production_call(worker: &Worker<'_>, actor: &str, id: &str) -> ValidatedInvocation {
        let mut guard = worker.shared.bindings.lock().unwrap();
        let channel = parse_channel_session(b"stop-retry").unwrap();
        let hook = parse_hook_event(
            serde_json::json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":id})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            serde_json::json!({"threadId":actor,"callId":id,"x-codex-turn-metadata":{"turn":"stop-retry"}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("fixture binding must validate")
        };
        invocation
    }

    /// Builds the fixture launcher with the worktree's temporary base as its allowed root.
    fn production_launcher(root: &std::path::Path) -> LauncherConfig {
        let git = std::path::Path::new("/usr/bin/git");
        let executable = serde_json::json!({
            "path":git,
            "identity":"fixture-git",
            "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
        });
        let config = serde_json::json!({
            "version":1,
            "limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":1024},
            "allowed_roots":[root.parent().unwrap()],
            "targets":[{
                "attachment":"stop-retry",
                "candidate":root,
                "git":executable,
                "providers":[]
            }]
        });
        LauncherConfig::parse(config.to_string().as_bytes()).unwrap()
    }

    /// Returns the target selected by the fixture's trusted launcher attachment.
    fn production_target(root: &std::path::Path) -> LaunchTarget {
        production_launcher(root)
            .target("stop-retry")
            .unwrap()
            .clone()
    }

    /// Runs the actual production `Worker::activate` entry point and returns its durable receipt.
    async fn production_start(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
    ) -> (BindingRef, StartReceipt) {
        let invocation = production_call(worker, actor, id);
        let binding = invocation.binding_ref().clone();
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut job = Job {
            reference: format!("production-{id}"),
            invocation,
            tool: AssistanceTool::Start,
            parameters: serde_json::json!({"activation_id":id}),
            target: production_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        worker.activate(&mut job).await.unwrap();
        let receipt = worker.grants.get(&binding).cloned().unwrap();
        (binding, receipt)
    }

    /// Activation rejects an out-of-root override and accepts an explicit in-root override.
    #[tokio::test]
    async fn activation_enforces_allowed_roots_and_accepts_root_parameter() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let (mut outside, _outside_cancel) = start_job(
            &worker,
            "outside-actor",
            "outside-start",
            serde_json::json!({"activation_id":"outside","root":"/private/tmp"}),
        );
        assert!(matches!(
            worker.activate(&mut outside).await,
            Err(FailureCode::OutsideAllowedRoots)
        ));

        let (mut inside, _inside_cancel) = start_job(
            &worker,
            "inside-actor",
            "inside-start",
            serde_json::json!({"activation_id":"inside","root":fixture.root}),
        );
        worker.activate(&mut inside).await.unwrap();
    }

    /// Builds a direct managed Start job for the fixture's trusted launcher target.
    ///
    /// The cancellation sender is returned so the caller keeps it alive: a dropped sender makes
    /// the job's `cancel.changed()` resolve at once, which activation reads as an interruption.
    fn start_job(
        worker: &Worker<'_>,
        actor: &str,
        call: &str,
        parameters: serde_json::Value,
    ) -> (Job, watch::Sender<bool>) {
        let invocation = production_call(worker, actor, call);
        let (cancel_sender, cancel) = watch::channel(false);
        let job = Job {
            reference: format!("start-{call}"),
            invocation,
            tool: AssistanceTool::Start,
            parameters,
            target: production_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        (job, cancel_sender)
    }

    /// Builds a Worker with real Store, binding and admission state for activation/revoke checks.
    /// Each test job supplies its own configured target and runs actual Git discovery; no background
    /// dispatcher is spawned by this fixture constructor.
    fn worker<'a>(
        store: &'a Store,
        workspace: DurableWorkspace<'a>,
        runtime: std::path::PathBuf,
    ) -> Worker<'a> {
        worker_with_output_bytes(store, workspace, runtime, 1024)
    }

    /// Same as [`worker`], but with a configurable source-read/output-capture ceiling: several
    /// Context pagination tests (T09B) need a fixture file larger than the fixed 1024-byte default.
    fn worker_with_output_bytes<'a>(
        store: &'a Store,
        workspace: DurableWorkspace<'a>,
        runtime: std::path::PathBuf,
        output_bytes: usize,
    ) -> Worker<'a> {
        let launcher = LauncherConfig::parse(
            serde_json::json!({
                "version":1,
                "limits":{"queued":8,"details":8,"operation_ms":5000,"output_bytes":output_bytes},
                "allowed_roots":[runtime.parent().unwrap()],
                "targets":[]
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        Worker {
            shared: Arc::new(Shared {
                bindings: Arc::new(Mutex::new(HostBindingGuard::default())),
                ledger: Mutex::new(Ledger::default()),
                notify: Notify::new(),
                launcher,
                claude_targets: Mutex::new(BTreeMap::new()),
                retired_initial_claude_target: std::sync::atomic::AtomicBool::new(false),
                nonce: [7; 32],
                shutting_down: std::sync::atomic::AtomicBool::new(false),
                shutdown_failure: Mutex::new(None),
                admission: Arc::new(Mutex::new(admission_controller())),
                telemetry: Arc::new(crate::assistance::telemetry::NoopEditTelemetry),
                problem_source: None,
                project_feed: None,
                test_runs: TestRuns::default(),
            }),
            workspace,
            observations: WorkspaceStore::new(store),
            edits: EditReceiptStore::new(store),
            grants: BTreeMap::new(),
            pending_revocations: std::collections::BTreeSet::new(),
            registered: BTreeMap::new(),
            baselines: BTreeMap::new(),
            source_sequence: 0,
            admission: Arc::new(Mutex::new(admission_controller())),
            uncertain: std::collections::BTreeSet::new(),
            uncertain_snapshots: Vec::new(),
            runtime,
            providers: providers::Providers::new(),
            telemetry: None,
        }
    }

    /// Proves managed edits chain completed source references with exact effects.
    #[tokio::test]
    async fn managed_edit_chains_replace_once_and_conflict_changed_duplicate() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "fn old() {}\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "edit-actor", "edit-start").await;

        let invocation = production_call(&worker, "edit-actor", "edit-context");
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut context_job = Job {
            reference: "context-source".into(),
            invocation,
            tool: AssistanceTool::Context,
            parameters: serde_json::json!({"path":"main.rs"}),
            target: production_target(&fixture.root),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        let (context_reply, authority, source) = worker.context(&mut context_job).await.unwrap();
        worker.shared.ledger.lock().unwrap().details.insert(
            context_job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: context_reply,
                selection: (AssistanceTool::Context, selection(&context_job.parameters)),
                authority,
                source,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );

        let invocation = production_call(&worker, "edit-actor", "edit-call");
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut edit_job = Job {
            reference: "edit-result".into(),
            invocation,
            tool: AssistanceTool::Edit,
            parameters: serde_json::json!({
                "operation_id":"operation-1",
                "path":"main.rs",
                "source_ref":"context-source",
                "content":"fn new() {}\n"
            }),
            target: production_target(&fixture.root),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        let (reply, authority, source) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    source_ref: Some(ref source_ref),
                    ..
                },
                ..
            } if source_ref == "edit-result"
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn new() {}\n"
        );
        assert_eq!(
            source.as_ref().unwrap().bytes(),
            Some(&crate::workspace::observation::SourceBytes::from_bytes(
                b"fn new() {}\n"
            ))
        );

        // A successful Edit is itself a usable post-read source detail for the next Edit.
        worker.shared.ledger.lock().unwrap().details.insert(
            edit_job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: reply.clone(),
                selection: (AssistanceTool::Edit, selection(&edit_job.parameters)),
                authority,
                source,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        edit_job.reference = "edit-result-2".into();
        edit_job.parameters = serde_json::json!({
            "operation_id":"operation-2",
            "path":"main.rs",
            "source_ref":"edit-result",
            "content":"fn newest() {}\n"
        });
        let (reply, _, _) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    source_ref: Some(ref source_ref),
                    ..
                },
                ..
            } if source_ref == "edit-result-2"
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn newest() {}\n"
        );

        edit_job.parameters["content"] = serde_json::json!("fn conflicting() {}\n");
        let (reply, _, _) = worker.edit(&mut edit_job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::ConflictingDuplicate,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read(fixture.root.join("main.rs")).unwrap(),
            b"fn newest() {}\n"
        );
    }

    #[tokio::test]
    async fn failed_stop_retains_pending_revoke_and_fresh_start_commits_it_first() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let tree = workspace
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                std::path::PathBuf::from(".git"),
            )
            .await
            .unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (old_binding, old_receipt) = production_start(&mut worker, "actor-1", "call-1").await;
        worker
            .registered
            .entry(old_binding.clone())
            .or_default()
            .insert(std::path::PathBuf::from("src/lib.rs"));

        // A real, freshly-retained `CacheLifecycle` owned by `old_binding`: freshly retained means
        // non-quiescent, i.e. actively owned by the still-live binding, not yet eligible for handoff.
        let cache_root = CacheRoot::prepare(fixture.base.join("cache")).unwrap();
        let cache_identity = CacheIdentity::new(
            "rust-analyzer",
            "rust-cache-priming-disabled-v1",
            "config",
            "1.98.1",
            "trusted",
            "tree-state",
        )
        .unwrap();
        let cache_key = "cache-1";
        let cache = CacheLifecycle::retain(
            &cache_root,
            CacheNamespaceId::new(cache_key).unwrap(),
            cache_identity,
            &tree,
        )
        .unwrap();
        assert!(!cache.quiescent(), "a freshly retained cache starts owned");
        worker.install_test_cache(&old_binding, cache_key, cache);

        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&old_binding)
            .unwrap();
        assert!(
            worker.shared.active(&old_binding).is_err(),
            "the stopped binding is unusable"
        );

        // A second real connection to the same fixture database holds the exact write lock the
        // durable revoke transaction needs, comfortably inside the reduced busy-timeout window.
        let lock = rusqlite::Connection::open(fixture.base.join("state.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let outcome = worker.revoke(&old_binding, "stop-1").await;
        assert!(matches!(outcome, Err(FailureCode::WorkspaceAuthority)));
        assert_eq!(worker.grants.get(&old_binding), Some(&old_receipt));
        assert!(worker.pending_revocations.contains(&old_binding));
        assert!(worker.registered.contains_key(&old_binding));
        assert_eq!(
            worker.test_cache_quiescent(cache_key),
            Some(false),
            "a failed durable revoke must retain the cache non-quiescent, not release it early"
        );
        assert!(
            worker.test_binding_owns_caches(&old_binding),
            "the stopped binding still owns its cache keys until the revoke actually commits"
        );

        lock.execute_batch("ROLLBACK;").unwrap();
        drop(lock);

        // This enters `Worker::activate` itself. Its reconciliation call must commit the pending
        // revoke before the real durable activation below can mint a new grant.
        let (new_binding, new_receipt) = production_start(&mut worker, "actor-1", "call-2").await;
        assert!(!worker.grants.contains_key(&old_binding));
        assert!(worker.pending_revocations.is_empty());
        assert!(!worker.registered.contains_key(&old_binding));
        assert_eq!(
            worker.test_cache_quiescent(cache_key),
            Some(true),
            "commiting the pending revoke must quiesce the old binding's cache for reuse/retirement"
        );
        assert!(
            !worker.test_binding_owns_caches(&old_binding),
            "the old binding must no longer own any cache keys; its source binding is unusable"
        );
        assert_eq!(worker.grants.len(), 1);
        assert!(worker.grants.contains_key(&new_binding));
        assert_eq!(worker.grants.get(&new_binding), Some(&new_receipt));
        assert!(!worker.grants.contains_key(&old_binding));

        let settled = worker.revoke(&new_binding, "stop-2").await;
        assert!(settled.is_ok());
        assert!(!worker.grants.contains_key(&new_binding));
        assert!(worker.pending_revocations.is_empty());
    }

    /// A recorded fake problem source used only by the problems-kind tests below; it never runs
    /// a check and only reports cloned fixed snapshots for whichever worktree it is asked about.
    struct RecordedProblems {
        /// Snapshots returned on every query, cloned per call.
        snapshots: Vec<ProblemSnapshot>,
        /// Recorded query worktrees in call order.
        queried: Mutex<Vec<std::path::PathBuf>>,
    }

    impl RecordedProblems {
        /// Builds a substitute returning the same snapshots for every worktree.
        fn new(snapshots: Vec<ProblemSnapshot>) -> Self {
            Self {
                snapshots,
                queried: Mutex::new(Vec::new()),
            }
        }

        /// Returns the recorded query worktrees in call order.
        fn queried(&self) -> Vec<std::path::PathBuf> {
            self.queried.lock().unwrap().clone()
        }
    }

    impl ProblemSource for RecordedProblems {
        fn latest(&self, worktree: &std::path::Path) -> Vec<ProblemSnapshot> {
            self.queried.lock().unwrap().push(worktree.to_path_buf());
            self.snapshots.clone()
        }
    }

    /// Runs one problems-kind context job through the real production context entry point with
    /// the fixture Codex binding's current accepted unrestricted observation.
    async fn run_problems_context(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
        parameters: serde_json::Value,
    ) -> PeerReply {
        let invocation = production_call(worker, actor, id);
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut job = Job {
            reference: format!("problems-{id}"),
            invocation,
            tool: AssistanceTool::Context,
            parameters,
            target: production_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            check_scheduled: false,
        };
        let (reply, _, source) = worker.context(&mut job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Complete {
                kind: ResultKind::Context,
                detail_ref: None,
                ..
            }
        ));
        // The problems kind reads no source file, so it never records an observation.
        assert!(source.is_none());
        reply
    }

    /// Proves the problems kind answers from the attached in-memory source under the fixture's
    /// unrestricted Codex observation and reports the honest disabled line without a source.
    /// No helper, provider, or source file is involved on either path.
    #[tokio::test]
    async fn context_problems_answers_from_source_and_reports_disabled_without_one() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        production_start(&mut worker, "problems-actor", "problems-start").await;

        // Without an attached source the reply is the honest single disabled line.
        let reply = run_problems_context(
            &mut worker,
            "problems-actor",
            "problems-disabled",
            serde_json::json!({"kind":"problems"}),
        )
        .await;
        let PeerReply::Complete { text, .. } = &reply else {
            panic!("problems context must complete: {reply:?}")
        };
        assert_eq!(text, "checks disabled");

        // With an attached source the page renders from the authorized worktree's snapshots.
        let snapshot = ProblemSnapshot::from_problems(
            Language::Rust,
            CheckState::Ready,
            vec![Problem::new(
                "src/main.rs".to_owned(),
                10,
                5,
                Severity::Error,
                Some("E0308".to_owned()),
                "mismatched types\u{7}!".to_owned(),
            )],
            1,
            5,
        );
        let fake = Arc::new(RecordedProblems::new(vec![snapshot]));
        Arc::get_mut(&mut worker.shared)
            .expect("fixture worker owns its shared state exclusively")
            .problem_source = Some(fake.clone());
        let reply = run_problems_context(
            &mut worker,
            "problems-actor",
            "problems-page",
            serde_json::json!({"kind":"problems","language":"rust","offset":0}),
        )
        .await;
        let PeerReply::Complete { text, .. } = &reply else {
            panic!("problems context must complete: {reply:?}")
        };
        assert!(
            text.contains("rust: ready; errors: 1; warnings: 0"),
            "{text}"
        );
        assert!(
            text.contains("src/main.rs:10:5 error [E0308] mismatched types!"),
            "{text}"
        );
        assert_eq!(fake.queried(), vec![fixture.root.clone()]);
    }

    /// Builds a bounded, easily reasoned-about test job for the fixture worktree's `main.rs`.
    fn context_job(
        root: &std::path::Path,
        invocation: ValidatedInvocation,
    ) -> (Job, watch::Sender<bool>) {
        let (cancel_sender, cancel) = watch::channel(false);
        (
            Job {
                reference: "continuation-detail".into(),
                invocation,
                tool: AssistanceTool::Context,
                parameters: serde_json::json!({"path":"main.rs"}),
                target: production_target(root),
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                cancel,
                stop_reply: None,
                native_epoch: 0,
                failure_detail: None,
                check_scheduled: false,
            },
            cancel_sender,
        )
    }

    /// Drops the position marker line every page of a multi-page result starts with (T16B).
    fn strip_page_marker(text: &str) -> &str {
        assert!(text.starts_with("page "), "missing position marker: {text}");
        text.split_once('\n').expect("marker line ends").1
    }

    /// A source file larger than one reply envelope is fully recoverable by repeating
    /// `ide.inspect` (`serve_inspection`) with the same detail_ref until a chunk reports
    /// `continuation: false`; on the managed path the first inspect delivers the still undelivered
    /// page one (T16B), the marker-stripped chunks equal the exact composed text byte-for-byte,
    /// every non-final chunk ends on a line boundary, and the final chunk honestly reports
    /// `truncated: false` because nothing was lost (T09B).
    #[tokio::test]
    async fn context_continuation_serves_the_complete_text_through_repeated_inspect() {
        let fixture = Fixture::new();
        // 60800 bytes, well above any plausible single reply envelope, so at least two
        // continuation pages are required regardless of exact overhead.
        let content = "let value = 1;\n".repeat(3800);
        std::fs::write(fixture.root.join("main.rs"), &content).unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        // The default 1024-byte fixture read cap would itself truncate this file before Context
        // pagination ever sees it; raise it well past `content.len()`.
        let mut worker = worker_with_output_bytes(&store, workspace, fixture.root.clone(), 200_000);
        worker.observations.install_schema().await.unwrap();
        let (binding, _) =
            production_start(&mut worker, "continuation-actor", "continuation-start").await;

        let invocation = production_call(&worker, "continuation-actor", "continuation-call");
        let (mut job, _cancel_sender) = context_job(&fixture.root, invocation);

        // Pre-insert the placeholder detail exactly as `enqueue` would: `context()`'s own
        // `set_context_page` call only mutates an *already retained* detail, matching production.
        worker.shared.ledger.lock().unwrap().details.insert(
            job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: PeerReply::Pending {
                    detail_ref: job.reference.clone(),
                },
                selection: (AssistanceTool::Context, selection(&job.parameters)),
                authority: None,
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        let (first_reply, authority, source) = worker.context(&mut job).await.unwrap();
        worker
            .shared
            .complete(&job.reference, first_reply.clone(), authority, source, 0);

        let PeerReply::Complete {
            text: first_text,
            truncated: true,
            continuation: true,
            ..
        } = &first_reply
        else {
            panic!("fixture file must overflow one reply envelope: {first_reply:?}");
        };
        assert!(first_text.ends_with('\n'), "must cut on a line boundary");
        assert!(
            first_text.starts_with("page 1; bytes 0-"),
            "page one carries its position marker: {}",
            &first_text[..80]
        );
        let mut collected = String::new();

        let mut pages = 0;
        loop {
            let (reply_tx, reply_rx) = oneshot::channel();
            serve_inspection(
                &worker.workspace,
                &worker.shared,
                Inspection {
                    binding: binding.clone(),
                    reference: job.reference.clone(),
                    expected: None,
                    reply: reply_tx,
                },
            )
            .await;
            let reply = reply_rx.await.unwrap();
            let PeerReply::Complete {
                text,
                truncated,
                continuation,
                ..
            } = &reply
            else {
                panic!("continuation must stay a Context Complete reply: {reply:?}")
            };
            pages += 1;
            assert!(
                pages < 50,
                "continuation must terminate in a bounded page count"
            );
            if pages == 1 {
                assert_eq!(
                    &reply, &first_reply,
                    "the managed first inspect must deliver the undelivered page one"
                );
            }
            assert!(
                text.starts_with(&format!("page {pages}")),
                "page {pages} marker: {}",
                &text[..text.len().min(80)]
            );
            collected.push_str(strip_page_marker(text));
            if *continuation {
                assert!(text.ends_with('\n'), "must cut on a line boundary");
                assert!(*truncated, "a page with more to come must report truncated");
            } else {
                assert!(!truncated, "fully recovered text must not claim truncation");
                assert!(
                    text.lines().next().unwrap().ends_with("; complete"),
                    "the last page states completion: {text}"
                );
                break;
            }
        }
        assert!(pages >= 2, "the fixture file must force multiple pages");
        assert!(
            collected.ends_with(&content),
            "concatenated chunks must end with the exact source bytes"
        );
    }

    /// A source file far larger than the launcher's tiny `output_bytes` discovery/output-capture
    /// budget, and over the former 64 KiB render cap, is paged out completely instead of failing
    /// closed as `source_unavailable` or being cut to a prefix: the read ceiling is
    /// `MAX_SOURCE_BYTES`, never the unrelated `output_bytes` budget (T13B, T16B).
    #[tokio::test]
    async fn managed_context_reads_a_source_larger_than_the_output_budget() {
        let fixture = Fixture::new();
        // 114000 bytes: matches the reported live-stability failure size, well over both the
        // fixture's default 1024-byte `output_bytes` and the former 64 KiB render cap, but
        // comfortably under the 1 MiB `MAX_SOURCE_BYTES` read ceiling.
        let content = "let value = 1;\n".repeat(7600);
        std::fs::write(fixture.root.join("main.rs"), &content).unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        // The fixture default (1024 bytes) is deliberately left unchanged here: it must never
        // bound the source read itself.
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "large-actor", "large-start").await;
        let invocation = production_call(&worker, "large-actor", "large-call");
        let (mut job, _cancel_sender) = context_job(&fixture.root, invocation);
        worker.shared.ledger.lock().unwrap().details.insert(
            job.reference.clone(),
            Detail {
                binding: binding.clone(),
                reply: PeerReply::Pending {
                    detail_ref: job.reference.clone(),
                },
                selection: (AssistanceTool::Context, selection(&job.parameters)),
                authority: None,
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        let (first_reply, authority, source) = worker.context(&mut job).await.unwrap();
        worker
            .shared
            .complete(&job.reference, first_reply.clone(), authority, source, 0);
        let PeerReply::Complete {
            truncated: true,
            continuation: true,
            ..
        } = &first_reply
        else {
            panic!("a file over one reply envelope must page: {first_reply:?}");
        };
        let mut collected = String::new();
        let mut pages = 0;
        loop {
            let (reply_tx, reply_rx) = oneshot::channel();
            serve_inspection(
                &worker.workspace,
                &worker.shared,
                Inspection {
                    binding: binding.clone(),
                    reference: job.reference.clone(),
                    expected: None,
                    reply: reply_tx,
                },
            )
            .await;
            let reply = reply_rx.await.unwrap();
            let PeerReply::Complete {
                text,
                truncated,
                continuation,
                ..
            } = &reply
            else {
                panic!("continuation must stay a Context Complete reply: {reply:?}")
            };
            pages += 1;
            assert!(
                pages < 50,
                "continuation must terminate in a bounded page count"
            );
            collected.push_str(strip_page_marker(text));
            if !continuation {
                assert!(
                    !truncated,
                    "the whole file was delivered, so the last page is not truncated"
                );
                break;
            }
        }
        assert!(pages >= 2, "the fixture file must force multiple pages");
        assert!(
            collected.ends_with(&content),
            "the pages must cover the whole source, not a prefix"
        );
    }

    /// A source file over the v0.1 reader's `MAX_SOURCE_BYTES` ceiling reports the exact size and
    /// ceiling through `FailureCode::SourceTooLarge`, never the generic `source_unavailable`, on
    /// the managed path (T13B).
    #[tokio::test]
    async fn managed_context_reports_source_too_large_above_the_ceiling() {
        let fixture = Fixture::new();
        let size = crate::workspace::observation::MAX_SOURCE_BYTES + 1;
        std::fs::write(fixture.root.join("main.rs"), vec![b'y'; size]).unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "huge-actor", "huge-start").await;
        let invocation = production_call(&worker, "huge-actor", "huge-call");
        let (mut job, _cancel_sender) = context_job(&fixture.root, invocation);
        worker.shared.ledger.lock().unwrap().details.insert(
            job.reference.clone(),
            Detail {
                binding,
                reply: PeerReply::Pending {
                    detail_ref: job.reference.clone(),
                },
                selection: (AssistanceTool::Context, selection(&job.parameters)),
                authority: None,
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        let Err(code) = worker.context(&mut job).await else {
            panic!("a source over MAX_SOURCE_BYTES must never be observed")
        };
        assert_eq!(
            code,
            FailureCode::SourceTooLarge {
                size: size as u64,
                ceiling: crate::workspace::observation::MAX_SOURCE_BYTES as u64,
            }
        );
    }

    /// A reference absent from the ledger, and one that belongs to a different binding, both fail
    /// closed as `invalid_detail` instead of leaking another binding's retained continuation state
    /// (T09B).
    #[tokio::test]
    async fn context_continuation_rejects_a_stale_or_foreign_reference() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "stale-actor", "stale-start").await;
        let _invocation = production_call(&worker, "stale-actor", "stale-call");

        // No reference was ever retained under this or any other binding.
        let (reply_tx, reply_rx) = oneshot::channel();
        serve_inspection(
            &worker.workspace,
            &worker.shared,
            Inspection {
                binding: binding.clone(),
                reference: "never-retained".into(),
                expected: None,
                reply: reply_tx,
            },
        )
        .await;
        assert!(matches!(
            reply_rx.await.unwrap(),
            PeerReply::Error {
                code: FailureCode::InvalidDetail
            }
        ));

        // A reference retained under a genuinely *different* binding (a distinct actor, since
        // `establish_start` reuses the existing generation for a repeated actor) must not be
        // reachable either.
        let other_invocation = production_call(&worker, "foreign-actor", "other-call");
        let other_binding = other_invocation.binding_ref().clone();
        worker.shared.ledger.lock().unwrap().details.insert(
            "foreign-detail".into(),
            Detail {
                binding: other_binding,
                reply: PeerReply::Complete {
                    kind: ResultKind::Context,
                    text: "owned by a different binding".into(),
                    detail_ref: Some("foreign-detail".into()),
                    truncated: false,
                    continuation: false,
                },
                selection: (
                    AssistanceTool::Context,
                    selection(&serde_json::json!({"path":"main.rs"})),
                ),
                authority: None,
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        serve_inspection(
            &worker.workspace,
            &worker.shared,
            Inspection {
                binding,
                reference: "foreign-detail".into(),
                expected: None,
                reply: reply_tx,
            },
        )
        .await;
        assert!(matches!(
            reply_rx.await.unwrap(),
            PeerReply::Error {
                code: FailureCode::InvalidDetail
            }
        ));
    }

    /// A small file's Context reply is byte-for-byte unchanged by the chunking path: it fits one
    /// page, so no `context_page` is retained and `continuation` stays `false`, exactly as before
    /// T09B introduced pagination.
    #[tokio::test]
    async fn small_context_reply_is_unchanged_by_chunking() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "fn small() {}\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "small-actor", "small-start").await;
        let invocation = production_call(&worker, "small-actor", "small-call");
        let (mut job, _cancel_sender) = context_job(&fixture.root, invocation);
        worker.shared.ledger.lock().unwrap().details.insert(
            job.reference.clone(),
            Detail {
                binding,
                reply: PeerReply::Pending {
                    detail_ref: job.reference.clone(),
                },
                selection: (AssistanceTool::Context, selection(&job.parameters)),
                authority: None,
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
        let (reply, _authority, _source) = worker.context(&mut job).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Complete {
                truncated: false,
                continuation: false,
                ..
            }
        ));
        assert!(
            worker
                .shared
                .ledger
                .lock()
                .unwrap()
                .details
                .get(&job.reference)
                .unwrap()
                .context_page
                .is_none(),
            "a whole-page result must retain no continuation state"
        );
    }
    /// Stages and commits the fixture worktree with a fixed identity so HEAD exists for
    /// managed Diff captures.
    fn git_commit(root: &std::path::Path, message: &str) {
        let run = |args: &[&str]| {
            let output = std::process::Command::new("/usr/bin/git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_AUTHOR_NAME", "fixture")
                .env("GIT_AUTHOR_EMAIL", "fixture@example")
                .env("GIT_COMMITTER_NAME", "fixture")
                .env("GIT_COMMITTER_EMAIL", "fixture@example")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "fixture git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run(&["add", "."]);
        run(&["commit", "--quiet", "-m", message]);
    }

    /// Activates the fixture worktree and returns its binding and durable authority.
    async fn activate_worktree(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
    ) -> (BindingRef, AuthorityStamp) {
        let (binding, _) = production_start(worker, actor, id).await;
        let authority = worker.authority(&binding).await.unwrap();
        (binding, authority)
    }

    /// Inserts the placeholder detail exactly as `enqueue` would, so a capture's own
    /// retained-page and provenance writes land in an already-retained row.
    fn retain_detail(
        worker: &Worker<'_>,
        binding: &BindingRef,
        reference: &str,
        tool: AssistanceTool,
        authority: &AuthorityStamp,
    ) {
        worker.shared.ledger.lock().unwrap().details.insert(
            reference.to_owned(),
            Detail {
                binding: binding.clone(),
                reply: PeerReply::Pending {
                    detail_ref: reference.to_owned(),
                },
                selection: (tool, [0; 32]),
                authority: Some(authority.clone()),
                source: None,
                native_epoch: 0,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
            },
        );
    }

    /// Builds an unstaged managed Diff job over the fixture worktree.
    fn diff_job(
        _root: &std::path::Path,
        invocation: ValidatedInvocation,
        reference: &str,
    ) -> (Job, watch::Sender<bool>) {
        let (cancel_sender, cancel) = watch::channel(false);
        (
            Job {
                reference: reference.to_owned(),
                invocation,
                tool: AssistanceTool::Diff,
                parameters: serde_json::json!({"mode":"unstaged"}),
                target: production_target(_root),
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                cancel,
                stop_reply: None,
                native_epoch: 0,
                failure_detail: None,
                check_scheduled: false,
            },
            cancel_sender,
        )
    }
    #[tokio::test]
    async fn diff_stream_caps_report_capacity_for_metadata_and_blob() {
        for case in ["metadata", "blob"] {
            let fixture = Fixture::new();
            if case == "metadata" {
                for n in 0..30 {
                    std::fs::write(fixture.root.join(format!("file-{n:02}.txt")), "base\n")
                        .unwrap();
                }
            } else {
                std::fs::write(fixture.root.join("large.txt"), vec![b'a'; 4096]).unwrap();
            }
            git_commit(&fixture.root, "stream baseline");
            if case == "blob" {
                std::fs::write(fixture.root.join("large.txt"), vec![b'b'; 4096]).unwrap();
            }
            let store = fixture.store();
            let workspace = DurableWorkspace::open(&store).await.unwrap();
            let mut worker = worker(&store, workspace, fixture.root.clone());
            worker.observations.install_schema().await.unwrap();
            let (binding, authority) =
                activate_worktree(&mut worker, "stream-actor", "stream-start").await;
            let invocation = production_call(&worker, "stream-actor", "stream-diff-call");
            let (mut job, _cancel) = diff_job(&fixture.root, invocation, "stream-diff");
            retain_detail(
                &worker,
                &binding,
                "stream-diff",
                AssistanceTool::Diff,
                &authority,
            );
            assert_eq!(
                worker.diff(&mut job).await.unwrap_err(),
                FailureCode::Capacity,
                "{case}"
            );
            assert_eq!(
                job.failure_detail.as_deref(),
                Some("diff:too_large"),
                "{case}"
            );
        }
    }
}

/// Cross-channel feedback delivery is tracked directly without a durable Workspace or provider.
#[cfg(test)]
mod feedback_dedup_tests {
    use super::*;
    use crate::assistance::host_binding::{
        BindingStatus, parse_candidate, parse_channel_session, parse_hook_event,
    };

    /// Establishes one real Codex binding directly through the host-binding guard; these tests
    /// only exercise the feedback ledger, so no sandbox, worktree or provider setup is needed.
    fn active_binding() -> (Arc<Mutex<HostBindingGuard>>, BindingRef) {
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let mut guard = bindings.lock().unwrap();
        let channel = parse_channel_session(b"feedback-dedup").unwrap();
        let hook = parse_hook_event(
            serde_json::json!({"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call-1"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(
            serde_json::json!({"threadId":"actor","callId":"call-1","x-codex-turn-metadata":{}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("fixture binding must validate")
        };
        let binding = invocation.binding_ref().clone();
        guard.consume_active(&binding).unwrap();
        drop(guard);
        (bindings, binding)
    }

    /// Builds a bare `WorkerHandle` with no configured targets; these tests touch only the ledger
    /// and never call `enqueue`/`submit`/`start`.
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

    /// Builds the exact submitted-carrier shape `mark_feedback_inline_delivered` traces: a
    /// `Complete{Context}` reply whose composed text embeds (or, for the trimmed case, omits)
    /// the fact under test, exactly as the real `feedback_delta:` header does in production.
    fn context_reply(detail_ref: &str, text: impl Into<String>) -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: text.into(),
            detail_ref: Some(detail_ref.into()),
            truncated: false,
            continuation: false,
        }
    }

    /// Builds one bounded issue identity for a fixed tag; distinct tags never compare equal.
    /// These unit tests exercise the delivery-marking and consumption boundary directly, so the
    /// identity's own content only needs to vary by tag — real content derivation is exercised by
    /// the production `context`/`context_claude` regressions in `tests/product_mcp_contract.rs`.
    fn identity(tag: &str) -> DeliveredIssue {
        DeliveredIssue {
            source_path: std::path::PathBuf::from("main.rs"),
            source_digest: *blake3::hash(tag.as_bytes()).as_bytes(),
            diagnostic_fingerprint: *blake3::hash(tag.as_bytes()).as_bytes(),
        }
    }

    /// An issue already delivered under one detail reference is not rearmed by another producer,
    /// while a changed issue remains eligible for one later hook delivery.
    #[tokio::test]
    async fn repeated_issue_is_suppressed_but_changed_issue_remains_deliverable() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        let repeated = identity("same");
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.delivered.insert(binding.clone(), repeated.clone());
            assert!(!ledger.retain_feedback(
                &binding,
                NativeFeedback {
                    source: None,
                    text: "Fact: repeated".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-2".into(),
                    identity: repeated,
                }
            ));
            assert!(ledger.retain_feedback(
                &binding,
                NativeFeedback {
                    source: None,
                    text: "Fact: changed".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-3".into(),
                    identity: identity("changed"),
                }
            ));
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: changed")
        );
    }

    /// A background job's fact that finished but was never handed to a live caller (still
    /// `Pending`/lost to a deadline from the caller's view) stays a pending new fact: the first
    /// eligible ordinary hook may deliver it once, and a second post must not resurrect it.
    #[tokio::test]
    async fn undelivered_fact_is_deliverable_exactly_once() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t1"),
                },
            );
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle
                .take_current_feedback(binding.clone())
                .await
                .as_deref(),
            Some("Fact: one bounded fact")
        );
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }

    /// A fact already handed to a live caller inside a submitted Context/Inspect reply must never
    /// be echoed by a later ordinary hook. This inverts the old assertion that pinned the
    /// cross-channel duplicate, and proves the mark is honored only when set at the real
    /// submission boundary (`mark_feedback_inline_delivered`), never at job production.
    #[tokio::test]
    async fn inline_delivered_fact_is_never_echoed_by_a_later_hook() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t2"),
                },
            );
        }
        let reply = context_reply(
            "detail-1",
            "diagnostic_count: 1\nfeedback_delta: Fact: one bounded fact",
        );
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &reply);
        handle.native_hint(binding.clone());
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }

    /// Two different Context jobs at the *same* native epoch (no intervening native edit) can
    /// each overwrite the single per-binding slot with a different fact. Retrieving the older,
    /// already-superseded detail (A) must never mark the *current*, different fact (B) delivered
    /// — acknowledgement is bound to the exact producing detail, never just the epoch. Regresses
    /// the defect where `mark_feedback_inline_delivered` matched on epoch alone.
    #[tokio::test]
    async fn retrieving_an_old_detail_cannot_consume_a_different_same_epoch_fact() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            // Detail A's fact is inserted, then Detail B's Context job overwrites the same slot
            // at the same native epoch — exactly the production single-slot race.
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: A's fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-A".into(),
                    identity: identity("A"),
                },
            );
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: B's fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-B".into(),
                    identity: identity("B"),
                },
            );
        }
        // A caller retrieves the stale detail A (e.g. a retry against an old detail_ref) and A's
        // own reply is handed to it; A's producer no longer matches the retained (B) entry.
        let reply_a = context_reply("detail-A", "feedback_delta: Fact: A's fact");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-A", &reply_a);
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: B's fact"),
            "B's fact must still be eligible: only A's stale detail was retrieved, not B's"
        );
    }

    /// A same-producer *value* that never actually carries the fact — a shrunk/trimmed text that
    /// no longer contains it, or an `Error` value passed in place of `Complete` (standing in for
    /// a capped/failed transport) — must never be mislabeled as delivered: the fact stays
    /// eligible. This exercises only `mark_feedback_inline_delivered`'s own content-survival
    /// check on the reply value it is given; it does not exercise closed-receiver ordering, which
    /// is a property of its callers (`serve_inspection`/`perform` only invoke it after their own
    /// `send()` already succeeded) rather than of this function.
    #[tokio::test]
    async fn a_carrier_that_dropped_the_fact_is_not_treated_as_delivered() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t3"),
                },
            );
        }
        // Same producer, but the rendered text this specific carrier actually holds omits the
        // fact (a trimmed/shrunk or otherwise unrelated body).
        let trimmed = context_reply("detail-1", "diagnostic_count: 1\nfeedback_delta: none");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &trimmed);
        // A closed/failed transport for the same producer.
        let closed = PeerReply::Error {
            code: FailureCode::Deadline,
        };
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &closed);
        handle.native_hint(binding.clone());
        assert_eq!(
            handle.take_current_feedback(binding).await.as_deref(),
            Some("Fact: one bounded fact")
        );
    }

    /// A source-backed fact is never delivered through the hook path — and its source is
    /// never reread there — even when the file still exists with byte-identical content, so
    /// a reread would succeed: the hook boundary has no live host sandbox state, so capture-
    /// time coverage can never be re-authorized against the current binding state (T36B-r).
    /// This is the reviewed attack's strongest form: capture under an unrestricted root-read
    /// state, narrow the binding to deny the file, trigger a native hook — nothing is read
    /// and nothing is disclosed. Helper-owned facts without a source observation keep
    /// delivering, because they never reread anything.
    #[tokio::test]
    async fn source_backed_feedback_is_never_reread_or_delivered_at_hook_time() {
        let dir = std::env::temp_dir().join(format!("t36b-feedback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        // Darwin's /tmp is a symlink alias: the no-follow reader needs the canonical root.
        let dir = std::fs::canonicalize(&dir).unwrap();
        let worktree = crate::workspace::authority::WorktreeRef::from_discovery(
            dir.clone(),
            dir.clone(),
            ".git".into(),
            1,
        )
        .unwrap();
        let source = SourceObservation::new(
            worktree,
            1,
            1,
            crate::workspace::observation::ObservationRef::new("source-1").unwrap(),
            "main.rs".into(),
            Some(crate::workspace::observation::SourceBytes::from_bytes(
                b"fn main() {}\n",
            )),
            crate::workspace::observation::SourceRevision::new("revision-1").unwrap(),
            crate::workspace::observation::SourceCoverage::Complete,
            crate::workspace::observation::ObservedState::Present,
        )
        .unwrap();
        // The captured bytes still match the file exactly: only the missing live
        // authorization, never staleness, can explain a refusal below.
        assert_eq!(
            std::fs::read(dir.join("main.rs")).unwrap(),
            b"fn main() {}\n"
        );
        for (label, source) in [
            ("source-backed", Some(source.clone())),
            ("helper-owned", None),
        ] {
            let (bindings, binding) = active_binding();
            let handle = handle(bindings);
            {
                let mut ledger = handle.shared.ledger.lock().unwrap();
                ledger.feedback.insert(
                    binding.clone(),
                    NativeFeedback {
                        source,
                        text: "Fact: one bounded fact".into(),
                        native_epoch: 0,
                        inline_delivered: false,
                        producer: "detail-1".into(),
                        identity: identity("t36b"),
                    },
                );
            }
            handle.native_hint(binding.clone());
            let delivered = handle.take_current_feedback(binding).await;
            if label == "source-backed" {
                assert_eq!(
                    delivered, None,
                    "a source-backed fact must never be delivered or reread at hook time"
                );
            } else {
                assert_eq!(
                    delivered.as_deref(),
                    Some("Fact: one bounded fact"),
                    "helper-owned facts never reread a source and keep delivering"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Repeated Context/Inspect retrieval of the same observation (the same native epoch) must
    /// not resurrect a fact this process already consumed via the hook channel: the single-slot
    /// remove in `take_current_feedback` is the identity, not a string comparison of raw text.
    #[tokio::test]
    async fn hook_consumed_fact_is_not_resurrected_by_a_later_delivery_mark() {
        let (bindings, binding) = active_binding();
        let handle = handle(bindings);
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger.feedback.insert(
                binding.clone(),
                NativeFeedback {
                    source: None,
                    text: "Fact: one bounded fact".into(),
                    native_epoch: 0,
                    inline_delivered: false,
                    producer: "detail-1".into(),
                    identity: identity("t4"),
                },
            );
        }
        handle.native_hint(binding.clone());
        assert_eq!(
            handle
                .take_current_feedback(binding.clone())
                .await
                .as_deref(),
            Some("Fact: one bounded fact")
        );
        // The entry is already gone; marking the same producer delivered afterward is a no-op and
        // must not fabricate a new retained fact.
        let reply = context_reply("detail-1", "feedback_delta: Fact: one bounded fact");
        handle
            .shared
            .mark_feedback_inline_delivered(&binding, "detail-1", &reply);
        assert_eq!(handle.take_current_feedback(binding).await, None);
    }
}
