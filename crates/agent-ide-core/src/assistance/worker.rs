//! One daemon-owned worker with bounded jobs/details, durable authorization and revocable work.

use super::{
    content,
    facade::{AssistanceTool, FeedbackDelta, render_reply},
    host_binding::{ActiveBindingUse, BindingRef, HostBindingGuard, ValidatedInvocation},
    launcher::{LaunchTarget, LauncherConfig},
    problems::{ProblemSource, ProjectProblemFeed, parse_language, problems_text_with_rechecks},
    reply::{
        EditDiagnostics, ExecutionProfileCause, FailureCode, MAX_NO_SUCH_FILE_PATH_BYTES,
        PeerReply, ResultKind, bounded_utf8_prefix,
    },
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
    lang::{Language, LanguageProject},
    project::{self as project_card, ServerState as CardServerState},
    workspace::{
        authority::{AuthorityStamp, StopBindingHandoff},
        durable::{DurableWorkspace, StartReceipt},
        store::WorkspaceStore,
    },
};
#[path = "links.rs"]
mod links;
#[path = "providers.rs"]
mod providers;
#[path = "snapshots.rs"]
pub(super) mod snapshots;
#[path = "symbols.rs"]
mod symbols;

use self::symbols::Located;

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
/// Whole budget for the `ide.start` project card (language detection, git plumbing, the tree
/// walk, and the render). The work is blocking, so it runs off the runtime under this deadline;
/// a card that exceeds it, or any panic inside the computation, degrades to the plain activation
/// text instead of failing activation.
const PROJECT_CARD_BUDGET: Duration = Duration::from_secs(5);
/// Maximum time an initial tool call waits for its job before returning its retained detail.
pub const INLINE_REPLY_WAIT: Duration = Duration::from_secs(8);
/// The honest per-language server state the start card prints while no language server has been
/// launched: which tools already answer from source, and which ones only the server can answer
/// (see [`project::not_started_state`]).
/// Why an activation baseline never claims complete coverage: Git metadata and source bytes are
/// captured as separate bounded steps, so no joint Git/source window is ever proven.
const BASELINE_PARTIAL_REASON: &str = "git metadata and source bytes are captured in separate steps, so no atomic window is proven and coverage cannot be claimed complete";
/// Longest an explicit `ide.test` command run is held inside that inline window so the starting
/// call can answer the settled result itself; a longer run falls back to the started line.
const TEST_INLINE_COMPLETION_WAIT: Duration = Duration::from_secs(6);

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
    /// Set by a symbol or line-range edit whose project formatter moved lines: the reply then
    /// states the movement so the next line-addressed edit does not reuse stale line numbers.
    format_note: Option<String>,
    /// `true` once the edit scheduled its project check itself (post-edit diagnostics), so the
    /// reply path must not schedule a second run that would shift the worktree's generation.
    check_scheduled: bool,
    /// Earliest retry time when an external provider/check condition is not ready.
    park_until: Option<tokio::time::Instant>,
    /// Retained in-memory continuation for work that cannot safely be repeated from its start.
    stage: Option<JobStage>,
    /// The binding that owns this job's provider sessions: the worktree's writer, else the reader
    /// that owns the namespace, while this job's own activation is a reader borrowing them; absent
    /// when the invocation binding owns them itself.
    /// Set only while a provider call runs; see [`Worker::resolve_session_owner`].
    session_binding: Option<BindingRef>,
}

/// Resumable state for operations that have already performed an externally visible edit.
enum JobStage {
    /// The edit is settled; only its target project-check snapshot and reply remain.
    EditAwaitingCheck {
        /// Settled Changes result returned to the caller after diagnostics are resolved.
        result: EditResult,
        /// Fresh post-edit observation retained for the source reference and detail ledger.
        refreshed: Option<SourceObservation>,
        /// Durable authority under which the edit and its diagnostics were checked.
        authority: AuthorityStamp,
        /// Edited relative path, retained alongside the check cursor.
        path: String,
        /// Exact provider answer retained if the project feed disappears before resume.
        fallback: EditDiagnostics,
        /// Project-check generation created by the edit.
        generation: u64,
        /// Worktree used to normalize checker-reported paths.
        worktree: PathBuf,
        /// Normalized edited path matched against checker output.
        wanted: String,
        /// Exact target-file problem identities present in the latest check before the edit.
        preexisting: BTreeMap<String, u32>,
        /// Language-specific project feed slot.
        language: crate::checks::Language,
        /// Last time a matching check may affect the edit reply.
        deadline: tokio::time::Instant,
    },
}

/// A retained outcome requiring exact binding ownership and fresh durable authorization on access.
pub(super) struct Detail {
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
    /// Additional per-file observations a batch `ide.read` covered under this one detail: the
    /// first file's observation stays `source`; every other delivered file rides here so the
    /// same reference authorizes an edit of any file the read included. Path disambiguates.
    extra_sources: Vec<crate::workspace::observation::SourceObservation>,
    /// Net line-count change and first line changed by a successful edit, if it moved lines.
    line_movement: Option<(u32, i64)>,
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
    /// Offset in `text` no page before it may extend past: the end of a result's first part (a
    /// card whose list was cut), so that part reads as before and the retained rest follows on
    /// later pages. `0` when the text has one part.
    first_part_end: usize,
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
            first_part_end: 0,
        }
    }

    /// Pages `head` and then `tail`: no page mixes the two, so `head` keeps its own first page
    /// and `ide.inspect` delivers the retained `tail` after it. Without a tail this is
    /// [`ContextPageState::new`] over `head`.
    fn with_tail(head: String, tail: Option<String>, kind: ResultKind) -> Self {
        let mut state = Self::new(head, 0, false, kind);
        if let Some(tail) = tail {
            state.first_part_end = state.text.len();
            state.text.push_str(&tail);
        }
        state
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
        let mut len = if self.delivered < self.first_part_end {
            self.first_part_end - self.delivered
        } else {
            remaining.len()
        };
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

#[cfg(test)]
mod context_page_tests {
    use super::*;

    /// Text and continuation flag of a complete reply.
    fn page(reply: &PeerReply) -> (&str, bool) {
        match reply {
            PeerReply::Complete {
                text, continuation, ..
            } => (text, *continuation),
            other => panic!("not a page: {other:?}"),
        }
    }

    /// A head that fits one page keeps a page of its own and the tail follows on the next one;
    /// without a tail the head is one plain page.
    #[test]
    fn a_tail_never_shares_the_head_page() {
        let (first, rest) =
            ContextPageState::with_tail("head\n".into(), Some("tail\n".into()), ResultKind::Symbol)
                .next("ref")
                .unwrap();
        assert_eq!(page(&first), ("page 1; bytes 0-5 of 10\nhead\n", true));
        let (second, rest) = rest.unwrap().next("ref").unwrap();
        assert_eq!(
            page(&second),
            ("page 2 (last); bytes 5-10 of 10; complete\ntail\n", false)
        );
        assert!(rest.is_none());
        let (only, rest) = ContextPageState::with_tail("head\n".into(), None, ResultKind::Symbol)
            .next("ref")
            .unwrap();
        assert_eq!(page(&only), ("head\n", false));
        assert!(rest.is_none());
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
///
/// Each registered language may retain one server slot, with one owner slot left for worker
/// operations such as Git snapshots and tests. A shared-server forwarder consumes a server slot
/// too. Before language registration, capacity reserves one server slot for core-only callers.
pub(super) fn admission_controller() -> crate::execution::AdmissionController {
    let server_slots = crate::lang::registered().len().max(1);
    crate::execution::AdmissionController::new(crate::execution::AdmissionLimits {
        total_running: 16,
        per_owner_running: server_slots.saturating_add(1),
        per_owner_queued: 1,
        total_queued: 64,
        interactive_burst: 8,
    })
    .expect("fixed process limits")
}

/// Keeps the controller's owner budget in sync with the registered language set.
#[cfg(test)]
#[test]
fn admission_reserves_a_server_slot_per_registered_language_and_one_free_slot() {
    // Fix the registry before reading it twice: a parallel test installing between the two reads
    // would otherwise change the expected size.
    crate::lang::testing::install();
    let server_slots = crate::lang::registered().len().max(1);
    let limits = admission_controller().inspect();
    assert_eq!(limits.per_owner_running_limit, server_slots + 1);
}

/// Shared bounded transport-side bookkeeping; no lock survives an I/O await.
struct Ledger {
    /// Jobs popped for execution and not yet settled; maintained in the same locked section that
    /// moves a job off `queue`, so an observer can never see both counts zero while a job runs.
    /// A queued or in-flight job defers idle shutdown (T26B) until it reaches its terminal state.
    in_flight: usize,
    /// FIFO ordinary jobs; explicit stop is prioritized at the front.
    queue: VecDeque<Job>,
    /// Retained results, released only by an explicit stop or by the settled-fact eviction
    /// policy of [`evict_settled_details`] (which journals every batch).
    details: BTreeMap<String, Detail>,
    /// Stable start requests under each immutable binding generation.
    starts: BTreeMap<(BindingRef, String, bool, [u8; 32]), String>,
    /// One cancellation sender per currently active binding.
    cancellation: BTreeMap<BindingRef, watch::Sender<bool>>,
    /// Last accepted tool activity for each active binding, used to reclaim abandoned result shares.
    last_activity: BTreeMap<BindingRef, tokio::time::Instant>,
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
            last_activity: BTreeMap::new(),
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
    /// Undelivered one-shot `git: HEAD moved …` plate lines keyed by binding fingerprint.
    git_notices: Mutex<BTreeMap<[u8; 32], String>>,
    /// Shared environment identities and one-shot notices.
    environments: Mutex<super::environment::EnvironmentState>,
    /// Binding fingerprints whose channel currently holds an activation, so the hook ingress can
    /// stay silent for a channel that never started (or already stopped) instead of emitting
    /// native hints nothing can consume.
    activated: Mutex<BTreeSet<[u8; 32]>>,
}
impl Shared {
    /// Refreshes file-resolved identities and invalidates checks for changed languages.
    fn refresh_environments(&self, worktree: &Path) {
        if let Ok(mut state) = self.environments.lock() {
            let changed = state.refresh(worktree);
            if let Some(feed) = &self.project_feed {
                for language in changed {
                    feed.environment_changed(worktree, language);
                }
            }
        }
    }

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
                detail: None,
            });
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.reply = reply;
            detail.authority = authority;
            detail.source = source;
            detail.native_epoch = native_epoch;
            detail.line_movement = reply_line_movement(&detail.reply);
        }
    }
    /// Answers one settled run's terminal text and, for the owning binding, hands the run's
    /// whole raw output to its retained detail as pages exactly once: the pager starts
    /// undelivered, so the first `ide.inspect` of that detail cuts page one itself, and the
    /// run's buffered copy is dropped only after the detail owns the pages. A later lookup
    /// finds the copy empty and leaves the retained pages as they are.
    fn settled_test_reply(
        &self,
        id: u64,
        binding: &BindingRef,
        status: &super::tests::JobStatus,
    ) -> (String, Option<String>) {
        let result = status
            .result
            .as_ref()
            .expect("a settled run always carries its result");
        let owns_detail = status.owner == binding.fingerprint();
        let text = test_result_text(id, result, owns_detail, status.explicit_command);
        // Paged once: after paging the run keeps only its bounded runner line, which must never
        // replace the full output page.
        if owns_detail && !result.output_paged && !result.output.is_empty() {
            let retained = if let Ok(mut ledger) = self.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&result.detail_ref)
            {
                detail.context_page = Some(ContextPageState::new(
                    result.output.clone(),
                    0,
                    false,
                    ResultKind::Test,
                ));
                detail.context_page_fresh = false;
                true
            } else {
                false
            };
            if retained {
                self.test_runs.clear_output(id, &binding.fingerprint());
            }
        }
        (text, owns_detail.then_some(result.detail_ref.clone()))
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

    /// Records that the caller already received its retained page, so the next `ide.inspect`
    /// advances instead of re-serving it (T16B).
    fn mark_page_delivered(&self, reference: &str) {
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.context_page_fresh = false;
            detail.diff_page_fresh = false;
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
    /// Retains the additional per-file edit bases a batch `ide.read` covered under one detail
    /// reference (the in-job write precedent is [`Self::set_context_page`]). Each observation
    /// keeps its own path, so `admitted_edit_source` can match an edit of any included file by
    /// path alone.
    fn add_edit_sources(&self, reference: &str, sources: Vec<SourceObservation>) {
        if sources.is_empty() {
            return;
        }
        if let Ok(mut ledger) = self.ledger.lock()
            && let Some(detail) = ledger.details.get_mut(reference)
        {
            detail.extra_sources = sources;
        }
    }
}

/// Longest the activation card waits for a running name-index build to report its summary.
const CARD_INDEX_WAIT: Duration = Duration::from_millis(200);

/// `(files, facts)` of a completed build of `index`; `None` while it is still building (the card
/// then omits the summary). With `wait` (the shared fact cache can serve the build) it waits up to
/// [`CARD_INDEX_WAIT`] for the build to complete; otherwise it never delays the card.
fn built_summary(
    index: &Arc<Mutex<crate::intelligence::names::NameIndex>>,
    wait: bool,
) -> Option<(usize, usize)> {
    let deadline = std::time::Instant::now()
        + if wait {
            CARD_INDEX_WAIT
        } else {
            Duration::ZERO
        };
    loop {
        if let Ok(index) = index.try_lock()
            && index.is_built()
        {
            return Some(index.summary());
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
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

    /// Returns the validated roots this daemon admits activation and checks under.
    pub fn allowed_roots(&self) -> &[std::path::PathBuf] {
        self.shared.launcher.allowed_roots()
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
                git_notices: Mutex::new(BTreeMap::new()),
                environments: Mutex::default(),
                activated: Mutex::new(BTreeSet::new()),
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

    /// Refreshes environment identities before plate rendering and returns due git/environment notices.
    pub fn git_notice(&self, binding: &[u8; 32]) -> Option<String> {
        let mut lines = Vec::new();
        if let Some(root) = self
            .shared
            .environments
            .lock()
            .ok()
            .and_then(|state| state.root(binding))
        {
            self.shared.refresh_environments(&root);
            if let Some(notice) = self.shared.environments.lock().ok()?.notice(&root, binding) {
                lines.push(notice);
            }
        }
        if let Some(notice) = self.shared.git_notices.lock().ok()?.get(binding) {
            lines.push(notice.clone());
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// Consumes the exact delivered notice; a newer notice remains due for this binding.
    pub fn consume_git_notice(&self, binding: &[u8; 32], line: &str) -> bool {
        if self.git_notice(binding).as_deref() != Some(line) {
            return false;
        }
        if let Some(root) = self
            .shared
            .environments
            .lock()
            .ok()
            .and_then(|state| state.root(binding))
            && let Ok(mut state) = self.shared.environments.lock()
        {
            state.consume(&root, binding, line);
        }
        if let Ok(mut notices) = self.shared.git_notices.lock()
            && notices
                .get(binding)
                .is_some_and(|notice| line.contains(notice))
        {
            notices.remove(binding);
        }
        true
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
    /// authorized worktree. The caller waits up to four seconds on its bounded oneshot, leaving
    /// room for the worker's brief refresh of a changed project check; a
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
            return PeerReply::Error {
                detail: (code.code == FailureCode::Capacity).then_some(code.stage),
                code: code.code,
            };
        }
        match tokio::time::timeout(Duration::from_secs(4), wait).await {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
                detail: None,
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
                leases: BTreeMap::new(),
                pending_revocations: std::collections::BTreeSet::new(),
                stop_cause: None,
                stop_attempts: Arc::default(),
                revoke_retry_rounds: 0,
                next_revoke_retry: None,
                registered: BTreeMap::new(),
                baselines: BTreeMap::new(),
                heads: BTreeMap::new(),
                source_sequence: 0,
                uncertain: std::collections::BTreeSet::new(),
                uncertain_snapshots: Vec::new(),
                runtime,
                providers: providers::Providers::new(),
                names: Default::default(),
                telemetry,
                activity: BTreeMap::new(),
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
    /// Enqueues an exact query and waits up to [`INLINE_REPLY_WAIT`] for its completed reply.
    ///
    /// A job still running at the limit returns its retained pending detail; callers can retrieve
    /// later completion with `ide.inspect`. The initial inspection permit is reserved before
    /// enqueueing so timeout always has capacity to return the current detail. A completed reply
    /// uses the worker's normal delivery path, preserving continuation and feedback semantics.
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
                detail: None,
            };
        }
        let (send, wait) = oneshot::channel();
        match admit_initial_inspection(&self.inspect, || {
            self.enqueue(invocation, tool, parameters, attachment, Some(send))
        }) {
            Ok((reference, permit)) => match tokio::time::timeout(INLINE_REPLY_WAIT, wait).await {
                Ok(Ok(reply)) => reply,
                _ => {
                    self.inspect_reserved(binding, reference, expected, permit)
                        .await
                }
            },
            Err(code) => PeerReply::Error {
                detail: (code.code == FailureCode::Capacity).then_some(code.stage),
                code: code.code,
            },
        }
    }

    /// Signals revocation immediately and waits up to 800 ms for the exact stop result. A timeout
    /// reports that cleanup may still be running; callers should check with `ide.start` before editing.
    ///
    /// The caller has already closed external admission; every queued job for this session is
    /// cancelled and removed while other sessions' jobs stay queued.
    pub async fn stop(&self, invocation: ValidatedInvocation, attachment: &str) -> PeerReply {
        let binding = invocation.binding_ref().clone();
        // Captured before `observe_binding` marks every run of this binding read, so the stop
        // reply can still name the runs whose results no caller ever collected.
        let never_read = self.shared.test_runs.uncollected(&binding.fingerprint());
        let mut uncollected = never_read
            .iter()
            .take(8)
            .map(|(id, argv, running)| {
                format!(
                    "#{} {}{}",
                    id,
                    display_argv(argv),
                    if *running { " (still running)" } else { "" }
                )
            })
            .collect::<Vec<_>>();
        if never_read.len() > 8 {
            uncollected.push(format!("(+{} more)", never_read.len() - 8));
        }
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
            ledger
                .starts
                .retain(|(owner, _, _, _), _| owner != &binding);
            ledger.native_epoch.remove(&binding);
            ledger.last_activity.remove(&binding);
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
            serde_json::json!({ "uncollected_test_runs": uncollected }),
            attachment,
            Some(send),
        ) {
            return PeerReply::Error {
                detail: (code.code == FailureCode::Capacity).then_some(code.stage),
                code: code.code,
            };
        }
        match tokio::time::timeout(Duration::from_millis(800), wait).await {
            Ok(Ok(reply)) => reply,
            _ => PeerReply::Error {
                code: FailureCode::Deadline,
                detail: Some("stop:deadline".to_owned()),
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
                detail: None,
            };
        }
        let permit = match reserve_inspection(&self.inspect) {
            Ok(permit) => permit,
            Err(code) => {
                return PeerReply::Error {
                    detail: (code == FailureCode::Capacity)
                        .then(|| "inspect:queue_full".to_owned()),
                    code,
                };
            }
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
        if let Ok(mut ledger) = self.shared.ledger.lock() {
            ledger
                .last_activity
                .insert(binding.clone(), tokio::time::Instant::now());
        }
        let (reply, wait) = oneshot::channel();
        permit.send(Inspection {
            binding,
            reference,
            expected,
            reply,
        });
        wait.await.unwrap_or(PeerReply::Error {
            code: FailureCode::Internal,
            detail: None,
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

    /// Reports whether this binding's channel currently holds an activation, so a hook ingress
    /// can stay silent for a channel that a failed (or stopped) start left with nothing to
    /// invalidate.
    pub fn channel_activated(&self, binding: &BindingRef) -> bool {
        self.shared
            .activated
            .lock()
            .is_ok_and(|activated| activated.contains(&binding.fingerprint()))
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

    /// Atomically bounds and publishes one operation without I/O; failures name the queue,
    /// result store or protected actor shares that prevented admission.
    #[allow(clippy::too_many_arguments)]
    fn enqueue(
        &self,
        invocation: ValidatedInvocation,
        tool: AssistanceTool,
        parameters: Value,
        attachment: &str,
        stop_reply: Option<oneshot::Sender<PeerReply>>,
    ) -> Result<String, InspectFailure> {
        if self
            .shared
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(FailureCode::Internal.into());
        }
        if !self
            .task
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return Err(FailureCode::Internal.into());
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
        if tool != AssistanceTool::Stop {
            ledger
                .last_activity
                .insert(binding.clone(), tokio::time::Instant::now());
        }
        let start = if tool == AssistanceTool::Start {
            Some((
                binding.clone(),
                parameters["activation_id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| default_activation_id(&binding)),
                parameters["read_only"].as_bool().unwrap_or(false),
                selection(&parameters["environment"]),
            ))
        } else {
            None
        };
        if let Some(key) = &start
            && let Some(reference) = ledger.starts.get(key).cloned()
        {
            if ledger
                .details
                .get(&reference)
                .is_some_and(|detail| matches!(detail.reply, PeerReply::Pending { .. }))
            {
                return Ok(reference);
            }
            // Coalesce only concurrent submissions. Once settled, a repeated start must run
            // admission again because a reader may upgrade or a writer may downgrade.
            ledger.starts.remove(key);
        }
        let queue_cap = queue_capacity(self.shared.launcher.limits.queued, tool);
        let now = tokio::time::Instant::now();
        if ledger
            .queue
            .iter()
            .filter(|job| job.park_until.is_none_or(|until| until <= now))
            .count()
            >= queue_cap
        {
            return Err(InspectFailure::stage(
                FailureCode::Capacity,
                "worker:queue_full",
            ));
        }
        if retain_detail && ledger.details.len() >= self.shared.launcher.limits.details {
            evict_settled_details(
                &mut ledger,
                self.shared.launcher.limits.details,
                &binding,
                tool,
                &self.shared.test_runs.detail_refs(),
                now,
            );
            if ledger.details.len() >= self.shared.launcher.limits.details {
                return Err(InspectFailure::stage(
                    FailureCode::Capacity,
                    if ledger
                        .details
                        .values()
                        .all(|detail| !matches!(detail.reply, PeerReply::Pending { .. }))
                    {
                        "worker:actor_share_full"
                    } else {
                        "worker:result_store_full"
                    },
                ));
            }
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
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
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

/// Removes the oldest runnable job and returns the next parked wake time, if any.
///
/// Jobs still awaiting an external condition rotate to the back without changing their ownership
/// or counting as in-flight; runnable jobs preserve their arrival order relative to one another.
fn pop_ready_job(
    queue: &mut VecDeque<Job>,
    now: tokio::time::Instant,
) -> (Option<Job>, Option<tokio::time::Instant>) {
    let mut earliest = None;
    for _ in 0..queue.len() {
        let job = queue.pop_front().expect("queue length was captured");
        if let Some(until) = job.park_until.filter(|until| *until > now) {
            earliest =
                Some(earliest.map_or(until, |current: tokio::time::Instant| current.min(until)));
            queue.push_back(job);
        } else {
            return (Some(job), earliest);
        }
    }
    (None, earliest)
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

/// Display word naming the symbol-edit operation an edit reply reports, from the call's `op`
/// argument: `inserted` and `deleted` replace the durable `replaced` outcome word so the reply
/// names what happened. `None` (replace, the line-range form, every plain edit) keeps the
/// outcome word; rename builds its own summary reply.
fn edit_operation(parameters: &Value) -> Option<String> {
    match parameters.get("op").and_then(Value::as_str) {
        Some("insert") => Some("inserted".to_owned()),
        Some("delete") => Some("deleted".to_owned()),
        _ => None,
    }
}

/// Settled details another live binding keeps before its oldest facts may be evicted.
const FAIR_DETAILS_PER_BINDING: usize = 8;
/// An active binding with no tool activity this long may have its settled results reclaimed.
const IDLE_BINDING_DETAILS_TTL: Duration = Duration::from_secs(15 * 60);

/// Returns whether a terminal detail is unpinned and therefore safe to evict.
fn detail_evictable(reference: &str, reply: &PeerReply, pinned: &BTreeSet<String>) -> bool {
    !matches!(reply, PeerReply::Pending { .. }) && !pinned.contains(reference)
}

/// Extracts the monotonic per-boot counter from a detail reference, which determines its age.
fn detail_sequence(reference: &str) -> u64 {
    reference
        .rsplit_once('-')
        .and_then(|(_, suffix)| suffix.parse().ok())
        .unwrap_or(0)
}

/// Returns up to eight newest result handles that are the latest source observation for a file.
/// This bounds a binding's source-protected share and counts it inside its fair floor.
fn newest_source_details(ledger: &Ledger, owner: &BindingRef) -> BTreeSet<String> {
    let mut newest: Vec<(u64, String)> = ledger
        .details
        .iter()
        .filter(|(reference, detail)| {
            detail.binding == *owner && is_newest_source_detail(ledger, reference, detail)
        })
        .map(|(reference, _)| (detail_sequence(reference), reference.clone()))
        .collect();
    newest.sort_unstable_by(|left, right| right.cmp(left));
    newest
        .into_iter()
        .take(FAIR_DETAILS_PER_BINDING)
        .map(|(_, reference)| reference)
        .collect()
}

/// Evicts the owner's oldest settled, unpinned facts until the ledger has room below `limit` or
/// only `floor` eligible facts remain. The floor counts protected source references too;
/// `protect_sources` exempts up to eight latest source details, for other actors only. Returns the
/// number removed. Pending details and references in `pinned` are never removed.
fn evict_binding_oldest(
    ledger: &mut Ledger,
    limit: usize,
    owner: &BindingRef,
    pinned: &BTreeSet<String>,
    floor: usize,
    protect_sources: bool,
) -> usize {
    let protected = if protect_sources {
        newest_source_details(ledger, owner)
    } else {
        BTreeSet::new()
    };
    let mut candidates: Vec<(u64, String)> = ledger
        .details
        .iter()
        .filter(|(reference, detail)| {
            detail.binding == *owner
                && detail_evictable(reference, &detail.reply, pinned)
                && !protected.contains(*reference)
        })
        .map(|(reference, _)| (detail_sequence(reference), reference.clone()))
        .collect();
    candidates.sort_unstable();
    let mut held = ledger
        .details
        .iter()
        .filter(|(reference, detail)| {
            detail.binding == *owner && detail_evictable(reference, &detail.reply, pinned)
        })
        .count();
    let mut removed = 0;
    for (_, reference) in candidates {
        if ledger.details.len() < limit || held <= floor {
            break;
        }
        ledger.details.remove(&reference);
        held -= 1;
        removed += 1;
    }
    removed
}

/// Keeps the newest fully delivered source detail for each file available for an edit retry.
fn is_newest_source_detail(ledger: &Ledger, reference: &str, detail: &Detail) -> bool {
    detail
        .source
        .iter()
        .chain(detail.extra_sources.iter())
        .filter_map(|source| source.path().to_str())
        .any(|path| newest_edit_source(ledger, &detail.binding, path).as_deref() == Some(reference))
}

/// Frees room below `limit` in a full result ledger: stopped or idle bindings first, the
/// requester next, then other bindings above their eight-result floor. `now` measures activity
/// against the 15-minute idle threshold; `pinned` protects active test outputs. Pending details
/// are always preserved, and each eviction batch is journaled.
fn evict_settled_details(
    ledger: &mut Ledger,
    limit: usize,
    requesting: &BindingRef,
    tool: AssistanceTool,
    pinned: &BTreeSet<String>,
    now: tokio::time::Instant,
) {
    let mut freed = 0;
    let inactive: Vec<String> = ledger
        .details
        .iter()
        .filter(|(reference, detail)| {
            let idle = ledger
                .last_activity
                .get(&detail.binding)
                .is_some_and(|last| {
                    now.saturating_duration_since(*last) > IDLE_BINDING_DETAILS_TTL
                });
            detail_evictable(reference, &detail.reply, pinned)
                && (!ledger.cancellation.contains_key(&detail.binding) || idle)
        })
        .map(|(reference, _)| reference.clone())
        .collect();
    for reference in &inactive {
        ledger.details.remove(reference);
    }
    freed += inactive.len();
    if ledger.details.len() >= limit {
        freed += evict_binding_oldest(ledger, limit, requesting, pinned, 0, false);
    }
    if ledger.details.len() >= limit {
        let others: BTreeSet<BindingRef> = ledger
            .details
            .values()
            .filter(|detail| &detail.binding != requesting)
            .map(|detail| detail.binding.clone())
            .collect();
        for binding in others {
            if ledger.details.len() < limit {
                break;
            }
            freed += evict_binding_oldest(
                ledger,
                limit,
                &binding,
                pinned,
                FAIR_DETAILS_PER_BINDING,
                true,
            );
        }
    }
    if freed > 0 {
        let detail = format!("details_evicted:{freed}");
        crate::errorlog::record(
            errorlog_method(tool),
            crate::errorlog::Outcome::Completed,
            crate::errorlog::Fields {
                detail: Some(&detail),
                ..Default::default()
            },
        );
    }
}

/// Bounds the in-memory pending-revocation set so an unbounded stream of failed durable revokes
/// cannot grow it. A full set refuses to record a further pending binding rather than evicting one;
/// the caller still learns the failure, and a daemon restart boot-fences every old grant anyway.
const MAX_PENDING_REVOCATIONS: usize = 64;

/// Least time between two `HEAD` probes of one binding: rapid tool calls reuse the last reading
/// instead of re-reading refs and `packed-refs`, so an outside switch is noticed at most this late.
const HEAD_PROBE_INTERVAL: Duration = Duration::from_secs(30);

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
    /// Retention lease of each granted binding's worktree, taken before its activation commits
    /// and released only with its binding state, so no sweep claims an activated worktree's caches.
    leases: BTreeMap<BindingRef, crate::retention::Lease>,
    /// Bindings whose provider settlement succeeded but whose durable revoke failed, so their
    /// receipt, caches and registrations are deliberately retained for a bounded cleanup-only
    /// retry. It never carries a physical-process uncertainty, which stays in `uncertain`.
    pending_revocations: std::collections::BTreeSet<BindingRef>,
    /// Typed cause of the last failed durable stop (`stop:busy`, `stop:store_full`, ...), taken by
    /// the stop job to name its failure instead of a generic authority error.
    stop_cause: Option<&'static str>,
    /// Durable revoke attempts this worker has started, across all stops; read by tests that
    /// release a held store lock only once the daemon's own retry began.
    stop_attempts: Arc<std::sync::atomic::AtomicUsize>,
    /// Consecutive background retry rounds that left a revoke pending; drives the retry backoff.
    revoke_retry_rounds: u32,
    /// When the next background retry of the pending revokes is due; `None` while none is pending.
    next_revoke_retry: Option<tokio::time::Instant>,
    /// Only explicitly requested paths are polled; no directory scanning is performed.
    registered: BTreeMap<BindingRef, RegisteredPaths>,
    /// Durable partial activation baselines retained for same-binding diff provenance.
    baselines: BTreeMap<BindingRef, crate::workspace::git::BaselineContext>,
    /// Per binding: when `HEAD` was last probed, successfully or not, and the checked-out branch or
    /// detached commit last read (first by its start); a failed read keeps the earlier reading.
    heads: BTreeMap<
        BindingRef,
        (
            tokio::time::Instant,
            Option<crate::workspace::git::head::HeadState>,
        ),
    >,
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
    /// Cross-language name indexes of the worktrees this daemon's bindings use (LRU of four).
    names: crate::intelligence::names::NameIndexes,
    /// Optional closed telemetry sink shared by Assistance producer boundaries.
    telemetry: Option<Telemetry>,
    /// Wall-clock milliseconds of each active binding's last completed job, keyed by binding
    /// fingerprint, so a refused activation can name the holder's last observed activity.
    activity: BTreeMap<[u8; 32], u64>,
}

impl<'a> Worker<'a> {
    /// Handles an explicit test start or a status request for a run the caller's own actor and
    /// channel started in this worktree; an unknown run id returns `invalid_detail` with the
    /// `test:unknown_run` stage.
    /// Runs inside the queued job. The `symbol` branch resolves references through the live
    /// language server before selecting tests, so its caller already holds a `pending` reply (see
    /// [`WorkerHandle::submit`]) and reads the started/no-tests line through `ide.inspect`; the other
    /// branches are answered inline by the waiting submit. A summary-less explicit command waits
    /// briefly for completion and then includes its exit code and bounded output in that reply.
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
            let Some(job_status) = self.shared.test_runs.get(&root, id, &binding) else {
                return Ok((
                    PeerReply::Error {
                        code: FailureCode::InvalidDetail,
                        detail: Some(format!("test:unknown_run:{id}")),
                    },
                    Some(authority),
                    None,
                ));
            };
            if job_status.result.is_some() {
                self.shared.settled_test_reply(id, &binding, &job_status)
            } else {
                (
                    format!(
                        "tests #{id}: running {} s; poll: call ide.test with {{\"status\": {id}}}",
                        job_status.age.as_secs()
                    ),
                    None,
                )
            }
        } else {
            let mut explicit_command = false;
            let mut command_cwd = root.clone();
            let mut command_env = Vec::new();
            let (argv, language, selected_count) = if let Some(path) =
                job.parameters.get("path").and_then(Value::as_str)
            {
                // In a language whose tests live only in test files, a file the runner's naming
                // convention does not count as a test file answers the same `no tests` hint the
                // symbol path gives, instead of being handed to the runner as a target (which may
                // import the module top-level or just fail). Directories keep selecting the test
                // files inside them.
                let target = PathBuf::from(path);
                if !root.join(&target).is_dir()
                    && let Some(language) = crate::lang::Language::for_path(&target)
                    && language.support().tests_only_in_test_files()
                    && !language.support().is_test_file(&target)
                {
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: format!("tests: no tests in {path}; the file has no tests"),
                            detail_ref: None,
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
                match test_selection(&root, crate::lang::TestTarget::File(target)) {
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
                if let Some(cwd) = job.parameters.get("cwd").and_then(Value::as_str) {
                    let root_path = root
                        .canonicalize()
                        .map_err(|_| FailureCode::ProviderUnavailable)?;
                    let requested = root.join(cwd).canonicalize();
                    let Ok(requested) = requested else {
                        return Ok((
                            PeerReply::InvalidParameters { message: "invalid bounded parameters: \"cwd\" must resolve to a directory inside the worktree".to_owned() },
                            Some(authority),
                            None,
                        ));
                    };
                    if !requested.starts_with(&root_path) || !requested.is_dir() {
                        return Ok((
                            PeerReply::InvalidParameters { message: "invalid bounded parameters: \"cwd\" must resolve to a directory inside the worktree".to_owned() },
                            Some(authority),
                            None,
                        ));
                    }
                    command_cwd = requested;
                }
                if let Some(env) = job.parameters.get("env").and_then(Value::as_object) {
                    command_env.extend(env.iter().filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.clone(), value.to_owned()))
                    }));
                }
                let Some(language) = detect_test_language(&root)
                    .or_else(|| crate::lang::registered().first().copied())
                else {
                    return Err(FailureCode::ProviderUnavailable);
                };
                explicit_command = true;
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
                let path = crate::lang::SymbolPath::parse(&symbol)
                    .map_err(|_| FailureCode::UnknownSymbol)?;
                // A bare name resolves through the same workspace-symbol path the symbol card
                // uses (T114); ambiguity answers with the candidates instead of guessing.
                let resolved = match path.file() {
                    Some(_) => None,
                    None => match self
                        .locate_by_name(job, &binding, path.name().unwrap_or_default())
                        .await?
                    {
                        Located::One(file) => Some(file),
                        Located::Many(candidates) => {
                            return self.ambiguous(job, &binding, &symbol, candidates).await;
                        }
                    },
                };
                // The selection target and the no-tests hint always name the definition file.
                let path = match path.file() {
                    Some(_) => path,
                    None => crate::lang::SymbolPath::parse(&format!(
                        "{}#{}",
                        resolved
                            .as_ref()
                            .ok_or(FailureCode::UnknownSymbol)?
                            .display(),
                        path.name().unwrap_or_default()
                    ))
                    .map_err(|_| FailureCode::UnknownSymbol)?,
                };
                let (referencing_tests, language, file_test_count) = self
                    .tests_referencing_symbol(job, &symbol, resolved.as_ref())
                    .await?;
                if referencing_tests.is_empty() {
                    let file = path
                        .file()
                        .and_then(|file| file.to_str())
                        .ok_or(FailureCode::UnknownSymbol)?;
                    let path_argument = serde_json::json!({"path": file});
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: format!(
                                "tests: no tests reference {symbol}; {}",
                                if file_test_count == 0 {
                                    "the file has no tests".to_owned()
                                } else {
                                    format!(
                                        "the file has {file_test_count} tests — ide.test {path_argument}"
                                    )
                                }
                            ),
                            detail_ref: None,
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
                let target = crate::lang::TestTarget::Symbol {
                    path,
                    referencing_tests,
                };
                let support = language.support();
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
                let bins: std::collections::BTreeSet<_> = selection
                    .tests
                    .iter()
                    .filter_map(|test| support.test_binary(&test.file))
                    .collect();
                let count = Some(if bins.len() > 1 {
                    format!(
                        "{} tests in {} binaries; running the workspace filter",
                        selection.tests.len(),
                        bins.len()
                    )
                } else {
                    format!("{} tests selected", selection.tests.len())
                });
                (selection.command, language, count)
            } else {
                return Ok((
                    PeerReply::Error {
                        code: FailureCode::ProviderUnavailable,
                        detail: Some("test:selection_unavailable".to_owned()),
                    },
                    Some(authority),
                    None,
                ));
            };
            // An explicit `command` is often a stand-in shell line rather than a runner, so the
            // job that starts one holds itself open inside the inline reply window: the starting
            // call itself then answers the settled result — one call instead of a
            // start/poll/inspect round trip — and only a run that outlasts the window keeps the
            // started line and the poll protocol. A resumed job finds its own run back by its
            // detail reference, never spawning the command a second time.
            let mut own_run = if explicit_command {
                self.shared.test_runs.started_run(&job.reference, &binding)
            } else {
                None
            };
            let mut started_id = None;
            if own_run.is_none() {
                let start = if explicit_command {
                    self.shared.test_runs.start_with_options(
                        root.clone(),
                        argv.clone(),
                        &binding,
                        super::tests::TestCommandOptions {
                            cwd: command_cwd,
                            env: command_env,
                            language,
                            command_language: None,
                            budget,
                            detail_ref: job.reference.clone(),
                        },
                    )
                } else {
                    self.shared.test_runs.start(
                        root.clone(),
                        argv.clone(),
                        language,
                        budget,
                        job.reference.clone(),
                        &binding,
                    )
                };
                match start {
                    StartResult::Started(id) => {
                        if explicit_command {
                            self.shared.test_runs.mark_explicit_command(id, &binding);
                        }
                        started_id = Some(id);
                        own_run = self.shared.test_runs.get(&root, id, &binding);
                    }
                    StartResult::Running(id, age) => {
                        return Ok((
                            PeerReply::Complete {
                                kind: ResultKind::Test,
                                text: format!(
                                    "tests #{id}: still running ({} s); poll: call ide.test with \
                                     {{\"status\": {id}}}",
                                    age.as_secs()
                                ),
                                detail_ref: None,
                                truncated: false,
                                continuation: false,
                            },
                            Some(authority),
                            None,
                        ));
                    }
                    StartResult::Failed { error, not_found } => {
                        let program = argv.first().map(String::as_str).unwrap_or("");
                        return Ok((
                            PeerReply::Complete {
                                kind: ResultKind::Test,
                                text: missing_runner_text(
                                    program,
                                    &error,
                                    not_found && !explicit_command,
                                    &root,
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
            }
            match own_run {
                Some(status) if status.result.is_some() => {
                    let (text, detail_ref) =
                        self.shared.settled_test_reply(status.id, &binding, &status);
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text,
                            detail_ref,
                            truncated: false,
                            continuation: false,
                        },
                        Some(authority),
                        None,
                    ));
                }
                Some(status)
                    if explicit_command
                        && status.age < TEST_INLINE_COMPLETION_WAIT
                        && tokio::time::Instant::now() + TEST_INLINE_COMPLETION_WAIT
                            < job.deadline =>
                {
                    // Parks instead of blocking the single worker loop; the submit-side inline
                    // window keeps waiting, and the job re-enters this branch on every wake.
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                    return Err(FailureCode::ProviderLoading);
                }
                running => {
                    let Some(id) = running.map(|status| status.id).or(started_id) else {
                        return Err(FailureCode::Internal);
                    };
                    let selected =
                        selected_count.map_or_else(String::new, |summary| format!(" ({summary})"));
                    return Ok((
                        PeerReply::Complete {
                            kind: ResultKind::Test,
                            text: format!(
                                "tests #{id}: started — {}{selected} (budget {} s); poll: call \
                                 ide.test with {{\"status\": {id}}}",
                                display_argv(&argv),
                                budget.as_secs()
                            ),
                            detail_ref: Some(job.reference.clone()),
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
    /// `ide.inspect`. Shutdown first cancels the current operation, allowing its provider or forwarder
    /// child to reap, then this loop closes retained providers before returning.
    ///
    /// A job that panics does not end the loop: its call settles `internal` (see
    /// `settle_panicked`), the error journal gets the panic's source location and the call's
    /// method but never the payload text, and the next queued job runs normally.
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
                self.release_all_live().await;
                if let Err(code) = self.close_all_providers().await
                    && let Ok(mut failure) = self.shared.shutdown_failure.lock()
                {
                    *failure = Some(code);
                }
                return;
            }
            // Failed stops stay pending and the daemon retries them itself (F-12), between jobs
            // as well as when idle, on a doubling schedule.
            let retry = self.schedule_revoke_retry();
            if retry.is_some_and(|due| tokio::time::Instant::now() >= due) {
                self.retry_pending_revocations().await;
            }
            let shared = self.shared.clone();
            let wake = shared.notify.notified();
            // The in-flight count moves in the same locked section as the pop, so "queue empty
            // and nothing in flight" is never observed while a job is between the two (T26B).
            let (job, earliest) = shared
                .ledger
                .lock()
                .ok()
                .map_or((None, None), |mut ledger| {
                    let (job, earliest) =
                        pop_ready_job(&mut ledger.queue, tokio::time::Instant::now());
                    if job.is_some() {
                        ledger.in_flight = ledger.in_flight.saturating_add(1);
                    }
                    (job, earliest)
                });
            match job {
                Some(job) => {
                    let mut job = job;
                    // One job's panic answers that call `internal` and journals where it happened
                    // (never the payload); the worker is the daemon's only job task, so it must
                    // outlive every job.
                    if catch_panic(self.perform(&mut job)).await.is_err() {
                        self.settle_panicked(&mut job);
                    }
                    if let Ok(mut ledger) = shared.ledger.lock() {
                        ledger.in_flight = ledger.in_flight.saturating_sub(1);
                        if job.park_until.is_some() {
                            ledger.queue.push_back(job);
                        }
                    }
                }
                None => {
                    // The idle worker also wakes when the next background retry is due.
                    let retry = self.schedule_revoke_retry();
                    let until = match (earliest, retry) {
                        (Some(job), Some(retry)) => Some(job.min(retry)),
                        (job, retry) => job.or(retry),
                    };
                    match until {
                        Some(until) => tokio::select! {
                            _ = wake => {},
                            _ = tokio::time::sleep_until(until) => {},
                        },
                        None => wake.await,
                    }
                }
            }
        }
    }
    /// Settles a job whose execution panicked: the caller's call answers `internal` under the
    /// tool's default stage (never the panic text), the error journal gets one line under the job's
    /// correlation id naming the panic's source location and the call's method, and no
    /// later run of the job is queued. The shared worker state it leaves behind is whatever the
    /// unwind stopped at; every later job re-derives its own inputs, so the worker keeps serving.
    fn settle_panicked(&mut self, job: &mut Job) {
        job.park_until = None;
        let detail = crate::telemetry::adapters::default_stage(job.tool, &FailureCode::Internal);
        let reply = PeerReply::Error {
            code: FailureCode::Internal,
            detail: Some(detail),
        };
        let method = errorlog_method(job.tool);
        let place = crate::errorlog::take_panic_place()
            .unwrap_or_else(|| "panic at an unknown location".to_owned());
        crate::errorlog::record(
            method,
            job_failure_outcome(FailureCode::Internal),
            crate::errorlog::Fields {
                reason: Some(FailureCode::Internal.into()),
                correlation: Some(job.reference.as_str()),
                detail: Some(&format!("{place} during {}", method.as_str())),
                ..Default::default()
            },
        );
        self.shared
            .complete(&job.reference, reply.clone(), None, None, job.native_epoch);
        if let Some(sender) = job.stop_reply.take() {
            let _ = sender.send(reply);
        }
    }
    /// Rechecks queued liveness, executes only the selected owner operation, and fences every result.
    async fn perform(&mut self, job: &mut Job) {
        let binding = job.invocation.binding_ref().clone();
        let was_parked = job.park_until.take().is_some();
        // Read/query jobs can restart from the top after readiness probes: observe records a fresh
        // source snapshot, and ensure_live_* reuses the same alive per-binding session.
        // A resumed job uses the latest epoch: Context observes and promises exact current bytes,
        // while Diff is a job-time working-tree snapshot and is intentionally not epoch-fenced.
        job.native_epoch = self
            .shared
            .ledger
            .lock()
            .ok()
            .and_then(|ledger| ledger.native_epoch.get(&binding).copied())
            .unwrap_or(0);
        // Mutating tools are refused before they reach edit preparation or process admission when
        // this activation is read-only. Every edit form shares the same tool and refusal path.
        let reader_refusal = if matches!(job.tool, AssistanceTool::Edit | AssistanceTool::Test)
            && self.grants.get(&binding).is_some_and(|receipt| {
                receipt.role() == crate::workspace::authority::StartRole::Reader
            }) {
            let tool = if job.tool == AssistanceTool::Edit {
                "ide.edit"
            } else {
                "ide.test"
            };
            let current_writer = self
                .grants
                .get(&binding)
                .map(|receipt| receipt.worktree().clone());
            let holder = match current_writer {
                Some(tree) => self.current_writer_facts(&tree).await,
                None => "none".to_owned(),
            };
            job.failure_detail = Some(format!("read_only:{tool}:{holder}"));
            Some(FailureCode::Conflict)
        } else {
            None
        };
        let result = if let Some(code) = reader_refusal {
            Err(code)
        } else if job.tool == AssistanceTool::Stop {
            let uncollected = job
                .parameters
                .get("uncollected_test_runs")
                .and_then(Value::as_array)
                .map(|runs| {
                    runs.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let revoked = self.revoke(&binding, &uncollected).await;
            if revoked.is_err() {
                job.failure_detail = self.stop_cause.take().map(str::to_owned);
            }
            revoked
        } else if *job.cancel.borrow() || self.shared.active(&binding).is_err() {
            if job.stage.is_some() {
                self.edit(job).await
            } else {
                Err(FailureCode::Cancelled)
            }
        } else if tokio::time::Instant::now() >= job.deadline {
            if job.stage.is_some() {
                self.edit(job).await
            } else if was_parked {
                Err(FailureCode::ProviderLoading)
            } else {
                Err(FailureCode::Deadline)
            }
        } else {
            if job.tool != AssistanceTool::Test {
                self.reconcile_hints(job).await;
            }
            if tokio::time::Instant::now() >= job.deadline {
                if job.stage.is_some() {
                    self.edit(job).await
                } else if was_parked {
                    Err(FailureCode::ProviderLoading)
                } else {
                    Err(FailureCode::Deadline)
                }
            } else {
                match job.tool {
                    AssistanceTool::Edit => self.edit(job).await,
                    AssistanceTool::Start => self.activate(job).await,
                    AssistanceTool::Context => self.context(job).await,
                    AssistanceTool::Diff => self.diff(job).await,
                    AssistanceTool::Outline => self.outline(job).await,
                    AssistanceTool::Read => {
                        panic_seam(job);
                        self.read(job).await
                    }
                    AssistanceTool::Symbol => self.symbol(job).await,
                    AssistanceTool::Graph => self.graph(job).await,
                    AssistanceTool::Test => self.test(job).await,
                    _ => Err(FailureCode::Internal),
                }
            }
        };
        if job.park_until.is_some() {
            return;
        }
        let (reply, authority, source) = match result {
            Ok(result) => result,
            Err(code) => {
                // Every terminal failure names its stage: the failing path's own tag when it set
                // one, else the derived `<tool>:<reason>` default — never a bare reason.
                if job.failure_detail.is_none() {
                    job.failure_detail =
                        Some(crate::telemetry::adapters::default_stage(job.tool, &code));
                }
                (
                    PeerReply::Error {
                        code,
                        detail: job.failure_detail.clone(),
                    },
                    None,
                    None,
                )
            }
        };
        // A stale source has already failed byte validation, so no retained reference can be
        // promised as a valid retry; the reply leaves the caller to obtain a fresh read.
        let reply = match reply {
            PeerReply::Edit {
                result,
                diagnostics,
                operation,
                note,
            } if result.outcome == ChangesEditOutcome::StaleSource => PeerReply::Edit {
                note,
                result,
                diagnostics,
                operation,
            },
            other => other,
        };
        if let PeerReply::Error { code, .. } = &reply {
            // T26B: a queued job's terminal failure must reach the error log with its closed
            // reason even when no caller view ever does — the dispatch path only logs the initial
            // `pending` placeholder a slow job returns, and daemon shutdown drops retained
            // details, so this line is otherwise the only record the job ever failed.
            let lifetime = Duration::from_millis(self.shared.launcher.limits.operation_ms);
            let started = job.deadline.checked_sub(lifetime).unwrap_or(job.deadline);
            crate::errorlog::record(
                errorlog_method(job.tool),
                job_failure_outcome(code.clone()),
                crate::errorlog::Fields {
                    reason: Some(code.clone().into()),
                    correlation: Some(job.reference.as_str()),
                    detail: job.failure_detail.as_deref(),
                    duration_ms: u32::try_from(started.elapsed().as_millis()).ok(),
                    ..Default::default()
                },
            );
        }
        if let Some(authority) = &authority {
            self.observe_head(&binding, authority);
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
                    // Name the edited file so only its language is forced; an edit whose
                    // parameters carry no path keeps the every-language behaviour.
                    feed.changed_file(
                        &binding.fingerprint(),
                        job.parameters.get("path").and_then(Value::as_str),
                    );
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
        // Holder facts name the holder's last observed activity: record every settled job of an
        // activated binding (E013 item 1).
        if self.grants.contains_key(&binding) {
            self.activity
                .insert(binding.fingerprint(), crate::errorlog::now_ms());
        }
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
                    // (T16B) — for every paged kind, symbol cards and outlines included. Only a
                    // lost receiver leaves the page undelivered and fresh.
                    self.shared.mark_page_delivered(&job.reference);
                }
            }
        }
    }
    /// Discovers and activates a worktree, settling fixed discovery commands before parsing.
    /// Repeats reuse durable authority and cache ownership. Validated environment choices are
    /// stored only after cache admission; cache refusals preserve an existing binding and choice.
    ///
    /// Provider namespace ownership: a reader retains nothing here — it claims the worktree's
    /// namespace on its first semantic call (`Worker::resolve_session_owner`). A writer (a new
    /// one, or a reader upgrading in place) first releases every reader owner of the worktree
    /// (`Worker::release_reader_owners`: sessions, then non-session views, then quiescence) and
    /// only then retains the namespace, so the namespace never has two live owners. A cleanup
    /// failure of a reader owner refuses the writer's start (`start:reader_provider_handover`)
    /// and leaves that reader as the namespace's sole owner; a retried start redoes the handover.
    /// A writer downgrading to a reader releases its own sessions and namespace.
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
        let read_only = job
            .parameters
            .get("read_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if read_only && job.parameters.get("environment").is_some() {
            let holder = match self.grants.get(&binding) {
                Some(receipt) => self.current_writer_facts(receipt.worktree()).await,
                None => "none".to_owned(),
            };
            job.failure_detail = Some(format!("read_only:ide.start:{holder}"));
            return Err(FailureCode::Conflict);
        }
        let previous_role = self.grants.get(&binding).map(StartReceipt::role);
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
        let discovered = match crate::workspace::git::discovery::validate_discovery(
            &candidate, &operation, &evidence,
        ) {
            Ok(discovered) => Some(discovered),
            // Git proved there is no repository at all only when no ancestor carries a `.git`;
            // the folder then activates as a plain directory with no Git data.
            Err(crate::workspace::git::GitError::InvalidDiscovery)
                if plain_directory_without_git(&candidate) =>
            {
                None
            }
            Err(crate::workspace::git::GitError::UnsupportedDiscoveryGit) => {
                record_execution_profile(errorlog_method(job.tool), "git_unsupported");
                return Err(FailureCode::ExecutionProfileCause(
                    ExecutionProfileCause::GitUnsupported,
                ));
            }
            Err(error) => {
                // Name the closed cause category so a broken discovery is distinguishable in the
                // reply and the journal.
                job.failure_detail = Some(discovery_failure_detail(&evidence, &error));
                return Err(FailureCode::WorkspaceActivation);
            }
        };
        // A plain directory is its own root, repository root and Git common dir.
        let (root, repository, common) = match &discovered {
            Some(discovered) => (
                discovered.root().to_path_buf(),
                discovered.repository_root().to_path_buf(),
                discovered.common_dir().to_path_buf(),
            ),
            None => (candidate.clone(), candidate.clone(), candidate.clone()),
        };
        admit_discovered(self.shared.launcher.allowed_roots(), &root, &common).map_err(|_| {
            job.failure_detail = Some(
                "start:worktree_unresolved: discovered root or Git common directory is outside allowed roots"
                    .to_owned(),
            );
            FailureCode::OutsideAllowedRoots
        })?;
        self.shared.active(&binding)?;
        let discovered = (root, repository, common);
        let tree = match self.resolve_worktree_named(&discovered).await {
            Ok(tree) => tree,
            Err((detail, holder)) => {
                // An active start whose session already died can still own a replaced directory's
                // previous incarnation, permanently refusing every later start of that root until
                // a daemon restart (E013 item 7: 22 refused starts on 03.10). Its durable revoke
                // needs no live binding, so settle it here and resolve once more.
                if let Some(holder) = holder
                    && let Some(binding) = self.binding_of_holder(&holder)
                    && self.shared.active(&binding).is_err()
                    && self.settle_revocation(&binding).await.is_ok()
                    && let Ok(tree) = self.resolve_worktree_named(&discovered).await
                {
                    tree
                } else {
                    job.failure_detail = Some(detail);
                    return Err(FailureCode::WorkspaceActivation);
                }
            }
        };
        self.shared.refresh_environments(tree.worktree_path());
        let choices = match validate_environment(
            job.parameters.get("environment"),
            tree.worktree_path(),
            self.shared.launcher.allowed_roots(),
        ) {
            Ok(choices) => choices,
            Err((code, detail)) => {
                return Ok((
                    PeerReply::Error {
                        code,
                        detail: Some(detail),
                    },
                    None,
                    None,
                ));
            }
        };
        let plain_directory = tree.is_plain_directory();
        self.reconcile_pending_revocations(&tree, job.invocation.actor_id())
            .await;
        let mut identity = blake3::Hasher::new();
        identity.update(&binding.fingerprint());
        // `activation_id` is optional: a start that names none derives a stable id from its
        // binding, so a repeated start without one returns the same activation (E013 item 4).
        let activation_id = job.parameters["activation_id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| default_activation_id(&binding));
        identity.update(activation_id.as_bytes());
        let operation = identity.finalize().to_hex().to_string();
        let requested_operation = operation.clone();
        let requested_tree = tree.clone();
        let request = crate::workspace::authority::ActivationRequest::new(
            operation,
            read_only,
            job.invocation.clone(),
            self.shared.active(&binding)?,
            tree,
        )
        .map_err(|_| {
            job.failure_detail = Some(
                "start:durable_state: the activation request could not be recorded".to_owned(),
            );
            FailureCode::WorkspaceActivation
        })?;
        // The lease exists before the activation can: a sweep never sees an activated worktree
        // without one, and an activation that cannot take it is refused.
        let Some(lease) = crate::retention::Lease::for_worktree(requested_tree.worktree_path())
        else {
            job.failure_detail = Some(
                "start:cache_lease: the worktree's cache retention lease could not be taken"
                    .to_owned(),
            );
            return Err(FailureCode::WorkspaceActivation);
        };
        // Do not cancel an in-flight durable commit: preserve its recoverable receipt before fencing output.
        let receipt = match self.workspace.activate(request).await {
            Ok(receipt) => receipt,
            Err(
                error @ (crate::workspace::durable::DurableError::OperationConflict
                | crate::workspace::durable::DurableError::Authority(
                    crate::workspace::authority::AuthorityError::WorktreeOwned
                    | crate::workspace::authority::AuthorityError::ActorAlreadyOwnsWorktree,
                )),
            ) => {
                // Name who holds what and the way out; a bare `conflict` left three agents without
                // the IDE for a whole task.
                job.failure_detail = Some(
                    self.conflict_detail(&requested_tree, job.invocation.actor_id(), &error)
                        .await,
                );
                return Err(FailureCode::Conflict);
            }
            Err(
                crate::workspace::durable::DurableError::Application(_)
                | crate::workspace::durable::DurableError::CorruptState,
            ) => {
                self.uncertain.insert(binding.clone());
                job.failure_detail = Some(
                    "start:durable_state: the durable activation state refused or failed"
                        .to_owned(),
                );
                return Err(FailureCode::WorkspaceActivation);
            }
            Err(_) => {
                job.failure_detail = Some(
                    "start:durable_state: the durable activation state refused or failed"
                        .to_owned(),
                );
                return Err(FailureCode::WorkspaceActivation);
            }
        };
        // A receipt whose operation differs from the one this call requested is the same session's
        // existing activation, returned idempotently; the reply must report that activation.
        let reused_activation = receipt.operation() != requested_operation;
        let next_role = receipt.role();
        let activation_operation = receipt.operation().to_owned();
        self.grants.insert(binding.clone(), receipt);
        self.leases.insert(binding.clone(), lease);
        // The one shared fact a hook ingress can check before emitting a native hint: this
        // channel's binding now holds an activation.
        if let Ok(mut activated) = self.shared.activated.lock() {
            activated.insert(binding.fingerprint());
        }
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
        if let Ok(mut state) = self.shared.environments.lock() {
            state.bind(binding.fingerprint(), authority.worktree().worktree_path());
        }
        if previous_role == Some(crate::workspace::authority::StartRole::Writer)
            && next_role == crate::workspace::authority::StartRole::Reader
        {
            self.release_live(&binding).await;
            if let Err(code) = self.close_provider(&binding).await {
                job.failure_detail = Some("start:read_only_provider_cleanup".to_owned());
                return Err(code);
            }
            self.quiesce_worktree_caches(&binding);
        }
        let launches = job.target.providers.clone();
        // A reader retains no namespace at start: it borrows the writer's sessions for semantic
        // reads, or claims the namespace itself on its first semantic call when no writer is
        // beside it (see `resolve_session_owner`). A writer takes the namespace over from any
        // reader owner of the worktree first, so it never meets a second live owner. A second
        // concurrent *writer* on the same physical worktree still cannot share one namespace:
        // fail its activation with the finite reason and roll its own grant back, so the actor
        // that already owns the cache keeps running and can hand off.
        if authority.role() == crate::workspace::authority::StartRole::Writer
            && let Err(code) = match self
                .release_reader_owners(
                    &binding,
                    previous_role == Some(crate::workspace::authority::StartRole::Reader),
                    &authority,
                )
                .await
            {
                Ok(()) => self.retain_worktree_caches(&binding, &authority, &launches, true),
                Err(code) => {
                    job.failure_detail = Some("start:reader_provider_handover".to_owned());
                    Err(code)
                }
            }
        {
            if code == FailureCode::Conflict {
                job.failure_detail = Some("start:provider_cache_namespace_conflict".to_owned());
            }
            if previous_role.is_some() {
                return Err(code);
            }
            if let Ok(mut guard) = self.shared.bindings.lock() {
                let _ = guard.stop_binding(&binding);
            }
            return Err(self.settle_revocation(&binding).await.err().unwrap_or(code));
        }
        // Cache admission must succeed before a choice becomes durable. A repeated activation
        // reuses the caller's ownership; a cache refusal never stops its existing binding.
        self.shared.active(&binding)?;
        if !choices.is_empty() {
            self.workspace
                .set_environment(authority.worktree(), choices)
                .await
                .map_err(|_| FailureCode::Internal)?;
        }
        self.shared
            .refresh_environments(authority.worktree().worktree_path());
        // A plain directory has no Git state to capture, so no baseline run happens at all.
        let baseline = if plain_directory {
            None
        } else {
            Some(
                self.capture_activation_baseline(job, &authority, &activation_operation)
                    .await,
            )
        };
        let git_metadata_captured = baseline.as_ref().is_some_and(|result| {
            result
                .as_ref()
                .ok()
                .and_then(|baseline| baseline.task_head())
                .is_some()
        });
        let mut baseline = match baseline {
            Some(Ok(baseline)) => {
                let description = format!(
                    "partial ({:?}; durable capture {}; {})",
                    baseline.window(),
                    baseline.capture_digest().is_some(),
                    BASELINE_PARTIAL_REASON
                );
                self.baselines.insert(binding.clone(), baseline);
                description
            }
            Some(Err(_)) => format!(
                "unknown (durable capture unavailable; {})",
                BASELINE_PARTIAL_REASON
            ),
            None => crate::workspace::authority::NO_GIT_DATA.to_owned(),
        };
        // The project card is a bounded best-effort addition: detection, git plumbing, and the
        // tree walk block, so they run on the blocking pool under [`PROJECT_CARD_BUDGET`]. Any
        // timeout, panic, or join failure yields an empty card and the plain activation text;
        // the card must never fail an activation that already succeeded.
        // Prewarm the name index off the reply path when files of a language that defines names
        // are present; nothing is built in other repositories. A worktree whose files the daemon
        // already indexed elsewhere builds from the shared cache in moments, so the card waits
        // briefly for it and reports the summary.
        let bridged = {
            let root = authority.worktree().worktree_path().to_path_buf();
            tokio::task::spawn_blocking(move || links::bridged_files_present(&root))
                .await
                .unwrap_or(false)
        };
        let reusable = bridged && self.names.has_cached_facts();
        if bridged {
            self.prewarm_names(authority.worktree());
        }
        let names = self.names.get(authority.worktree());
        let card = {
            let root = authority.worktree().worktree_path().to_path_buf();
            let walk = tokio::task::spawn_blocking(move || {
                let languages: Vec<LanguageProject> = crate::lang::registered()
                    .iter()
                    .filter_map(|language| language.support().detect(&root))
                    .collect();
                // The daemon does not probe language servers at start; every detected language's
                // server state is the honest "not started" until a later tool observes otherwise,
                // and it names what already works from source so a heavy user does not wait for
                // semantic tools that only the server can answer. Only languages that have a
                // server get a server state.
                let servers = languages
                    .iter()
                    .filter(|project| project.language.server().is_some())
                    .map(|project| CardServerState {
                        language: project.language,
                        state: project_card::not_started_state(project.language).to_owned(),
                    })
                    .collect();
                let links = project_card::links_line(&languages);
                let project = project_card::collect(&root, languages, servers, None);
                let clean_git = project.git.as_ref().and_then(|git| {
                    if git.clean {
                        git.last_commit
                            .as_ref()
                            .map(|(sha, _)| format!("git {sha} (clean)"))
                    } else {
                        None
                    }
                });
                let mut card = project_card::render(&project);
                if let Some(links) = links {
                    card.push('\n');
                    card.push_str(&links);
                    // The index summary, once a build of this worktree has completed.
                    if let Some((files, facts)) = names
                        .as_ref()
                        .and_then(|index| built_summary(index, reusable))
                    {
                        card.push_str(&format!(" (indexed {files} files, {facts} facts)"));
                    }
                }
                (card, clean_git)
            });
            match tokio::time::timeout(PROJECT_CARD_BUDGET, walk).await {
                Ok(Ok(card)) => card,
                Ok(Err(_)) | Err(_) => (String::new(), None),
            }
        };
        if git_metadata_captured && let Some(clean_git) = card.1 {
            baseline = clean_git;
        }
        let card = card.0;
        let reused = if reused_activation {
            format!("existing activation {activation_operation}; ")
        } else {
            String::new()
        };
        let mut text = if authority.role() == crate::workspace::authority::StartRole::Reader {
            let holder = self.current_writer_facts(authority.worktree()).await;
            format!(
                "activated: epoch {}; mode: read-only; {reused}current writer: {holder}; \
                 read tools answer; call ide.start without read_only to edit or run tests; \
                 baseline: {baseline}",
                authority.epoch(),
            )
        } else {
            format!(
                "activated: epoch {}; mode: writer; {reused}baseline: {baseline}",
                authority.epoch(),
            )
        };
        if !card.is_empty() {
            text.push_str("\n\n");
            text.push_str(&card);
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text,
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

    /// Allocates one physical slot for `binding`'s owner without waiting behind a retained idle
    /// backend: a request that would queue has its ticket cancelled at once, so nothing stays
    /// queued for this owner.
    ///
    /// Submission and cancellation run under one admission guard. The guard of a `match`
    /// scrutinee lives until the end of the `match`, so locking the (non-reentrant) mutex a second
    /// time inside an arm deadlocked the worker task's thread forever once the owner's running
    /// slots were full — e.g. two live language servers of one binding (0.6.1 `ide.diff` hang).
    ///
    /// # Errors
    ///
    /// * [`FailureCode::Capacity`] when the owner or the daemon has no free running slot. Language
    ///   servers never hold an owner's last slot (see [`admission_controller`]), so for one owner
    ///   this means its other non-server processes hold it.
    /// * [`FailureCode::Internal`] when the binding fingerprint is not a valid owner identity.
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
        let mut admission = self.admission();
        match admission.submit(owner, AdmissionClass::Interactive) {
            Admission::Granted(lease) => Ok(lease),
            Admission::Queued(ticket) => {
                admission.cancel_ticket(ticket);
                Err(FailureCode::Capacity)
            }
            Admission::Refused(_) => Err(FailureCode::Capacity),
        }
    }

    /// Names the holder a refused activation collided with and the remedy that frees it.
    ///
    /// A same-actor holder never reaches this method as the same binding: [`DurableWorkspace::activate`]
    /// answers that case idempotently with the existing activation. Holder facts ride the closed
    /// tag (actor, activation id, role, since when, last activity) so the refusal names who holds
    /// what and since when; the reply template supplies the plain-words remedy for each tag.
    async fn conflict_detail(
        &self,
        tree: &crate::workspace::authority::WorktreeRef,
        actor: &str,
        error: &crate::workspace::durable::DurableError,
    ) -> String {
        use crate::workspace::durable::DurableError;
        match error {
            DurableError::OperationConflict => "start:activation_conflict".to_owned(),
            DurableError::Authority(
                crate::workspace::authority::AuthorityError::ActorAlreadyOwnsWorktree,
            ) => match self.workspace.start_holder(tree, actor).await {
                Ok(Some(holder)) => format!(
                    "start:actor_owns_another_worktree: {}",
                    self.holder_facts(&holder)
                ),
                Ok(None) | Err(_) => "start:actor_owns_another_worktree".to_owned(),
            },
            _ => match self.workspace.start_holder(tree, actor).await {
                Ok(Some(holder)) if holder.same_actor => format!(
                    "start:worktree_held_by_this_actor: {}",
                    self.holder_facts(&holder)
                ),
                Ok(Some(holder)) => format!(
                    "start:worktree_held_by_another_actor: {}",
                    self.holder_facts(&holder)
                ),
                Ok(None) | Err(_) => "start:conflict".to_owned(),
            },
        }
    }

    /// Renders one holder's closed facts: actor, activation id, role, since when, last activity.
    /// The last activity comes from this worker's own completion records of the holder's binding,
    /// which every tool call of this daemon flows through.
    fn holder_facts(&self, holder: &crate::workspace::durable::StartHolder) -> String {
        let since = timestamp_line(holder.started_ms);
        let last = self
            .activity
            .get(&holder.binding)
            .map(|ms| timestamp_line(*ms))
            .unwrap_or_else(|| "unknown".to_owned());
        format!(
            "actor {} (activation {}, {}, since {}, last activity {})",
            holder.actor,
            holder.activation,
            holder.role.as_str(),
            since,
            last
        )
    }

    /// Reports the current writer's activation and activity facts for a read-only refusal.
    async fn current_writer_facts(
        &self,
        tree: &crate::workspace::authority::WorktreeRef,
    ) -> String {
        match self.workspace.start_holder(tree, "").await {
            Ok(Some(holder)) if holder.role == crate::workspace::authority::StartRole::Writer => {
                let last = self
                    .activity
                    .get(&holder.binding)
                    .map(|ms| timestamp_line(*ms))
                    .unwrap_or_else(|| "unknown".to_owned());
                format!(
                    "activation {}, since {}, last activity {}",
                    holder.activation,
                    timestamp_line(holder.started_ms),
                    last
                )
            }
            _ => "none".to_owned(),
        }
    }

    /// Maps one durable holder record onto this daemon's live binding for it, when the worker
    /// still holds its grant.
    fn binding_of_holder(
        &self,
        holder: &crate::workspace::durable::StartHolder,
    ) -> Option<BindingRef> {
        self.grants
            .iter()
            .find(|(binding, receipt)| {
                binding.fingerprint() == holder.binding && receipt.actor() == holder.actor
            })
            .map(|(binding, _)| binding.clone())
    }

    /// Resolves the discovered worktree, collapsing only the refusal's own facts: the closed
    /// failing step and, when an active start blocks a replaced identity, that start's facts.
    async fn resolve_worktree_named(
        &self,
        discovered: &(std::path::PathBuf, std::path::PathBuf, std::path::PathBuf),
    ) -> Result<
        crate::workspace::authority::WorktreeRef,
        (String, Option<Box<crate::workspace::durable::StartHolder>>),
    > {
        match self
            .workspace
            .resolve_worktree(
                discovered.0.clone(),
                discovered.1.clone(),
                discovered.2.clone(),
            )
            .await
        {
            Ok(tree) => Ok(tree),
            Err(crate::workspace::durable::DurableError::IdentityUnavailable { step, holder }) => {
                let detail = match holder.as_deref() {
                    Some(holder) => format!(
                        "start:worktree_unresolved:{step}: {}",
                        self.holder_facts(holder)
                    ),
                    None => format!("start:worktree_unresolved:{step}"),
                };
                Err((detail, holder))
            }
            Err(_) => Err(("start:worktree_unresolved:identity_commit".to_owned(), None)),
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

    /// Reads and persists one registered file under fresh durable authority, preserving missing
    /// state. The read counts as a use of the path in the registered-path working set.
    async fn observe(
        &mut self,
        binding: &BindingRef,
        path: std::path::PathBuf,
    ) -> Result<(SourceObservation, Vec<u8>), FailureCode> {
        self.observe_as(binding, path, true).await
    }

    /// [`Self::observe`], with `used` saying whether the read answers a request (and so refreshes
    /// the path's last-use time) or is maintenance — a native-hint refresh of every registered
    /// path — that must keep the path's real age, so paths nobody asks for still retire.
    async fn observe_as(
        &mut self,
        binding: &BindingRef,
        path: std::path::PathBuf,
        used: bool,
    ) -> Result<(SourceObservation, Vec<u8>), FailureCode> {
        use crate::workspace::{
            observation::{
                MAX_SOURCE_BYTES, ObservationError, ObservationRef, SourceCoverage,
                SourceReadLimits, SourceRevision, read_authorized_source,
            },
            store::{ObservationAdmission, ObservationDraft},
        };
        let authority = self.authority(binding).await?;
        // A new path takes a registered-path slot first, evicting the least recently used path
        // when the budget is full; a read is never refused for it.
        self.admit_registered_path(binding, &path);
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
        if used {
            paths.insert(path);
        } else {
            paths.keep(path);
        }
        Ok((observed, bytes))
    }

    /// Admits `path` into the binding's registered-path working set, evicting the least recently
    /// used path when the set is at its [`MAX_REGISTERED_PATHS`] budget (F-25).
    ///
    /// The budget is the registered paths' own: it neither shares nor shrinks with the retained
    /// results limit, and evicting results does not touch it. A path already registered is always
    /// admitted, and a new path always is: the evicted path only stops being refreshed on native
    /// hints. A later edit built on its retained read is judged by the file's bytes as always —
    /// still matching, it applies; changed, it meets the ordinary stale-source answer and the
    /// agent re-reads. Nothing here refuses or blocks a read.
    fn admit_registered_path(&mut self, binding: &BindingRef, path: &std::path::Path) {
        let paths = self.registered.entry(binding.clone()).or_default();
        if !paths.contains(path) && paths.len() >= MAX_REGISTERED_PATHS {
            paths.evict_least_recently_used();
        }
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
            let paths = self
                .registered
                .get(binding)
                .map(RegisteredPaths::paths)
                .unwrap_or_default();
            for path in paths {
                if *job.cancel.borrow() || tokio::time::Instant::now() >= job.deadline {
                    break;
                }
                // Maintenance, not a request: the refresh keeps each path's real last-use time.
                if self.observe_as(binding, path, false).await.is_err() {
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
        let path_detail = super::reply::bounded_utf8_prefix(&format!("{path:?}"), 256);
        let (observed, bytes) = self.observe(&binding, path.clone().into()).await?;
        let byte_offset = job.parameters.get("byte_offset").and_then(Value::as_u64);
        let query = byte_offset.map_or(ContextQuery::File, |byte_offset| ContextQuery::Symbol {
            byte_offset: byte_offset as usize,
        });
        let semantic = if observed.bytes().is_none() {
            Ok(None)
        } else {
            self.semantic_context(job, &observed, &bytes, query).await
        };
        let lexical = |job: &mut Job, reason: &'static str| {
            lexical_context(&observed, &bytes, query, reason).map_err(|_| {
                job.failure_detail = Some(format!("context:observation_failed:{path_detail}"));
                FailureCode::SourceUnavailable
            })
        };
        let (context, diagnostics) = match semantic {
            Ok(Some(result)) => (result.context, Some(result.diagnostics)),
            Ok(None) => (
                lexical(
                    job,
                    "no accepted provider is configured for this source, or the registered path is missing",
                )?,
                None,
            ),
            Err(FailureCode::ProviderUnavailable) => (
                lexical(job, "accepted semantic provider is unavailable")?,
                None,
            ),
            Err(FailureCode::ProviderLoading) => (
                lexical(
                    job,
                    "semantic provider is still loading the workspace; repeat the call in a few seconds",
                )?,
                None,
            ),
            Err(FailureCode::ResolutionUnverified) => {
                let reason = job
                    .failure_detail
                    .clone()
                    .unwrap_or_else(|| "semantic project resolution is unverified".to_owned());
                (
                    lexical_context(&observed, &bytes, query, &reason).map_err(|_| {
                        job.failure_detail =
                            Some(format!("context:observation_failed:{path_detail}"));
                        FailureCode::SourceUnavailable
                    })?,
                    None,
                )
            }
            Err(FailureCode::ExecutionProfile) => (
                lexical(
                    job,
                    "accepted semantic provider cannot run under the current execution profile",
                )?,
                None,
            ),
            Err(code) => return Err(code),
        };
        // A finished context job is no longer fenced by the native epoch: pages are frozen byte
        // slices of the page-1 snapshot and the exact-byte check below is the whole staleness
        // contract (T15B follow-up). The epoch still bumps for every non-inert native post and
        // every managed read boundary, which previously discarded jobs whose bytes never changed.
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&binding)
            .copied()
            .unwrap_or(0);
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::Deadline);
        }
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        if !source_matches(&observed) {
            job.failure_detail = Some(format!("context:source_changed:{path_detail}"));
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
                    .map(|diagnostic| {
                        super::reply::diagnostic_line(
                            &observed.path().to_string_lossy(),
                            diagnostic,
                        )
                    })
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
        // T163 (W5, revised): the whole-file path mode keeps serving exactly as before for
        // compatibility (real hosts and the acceptance scripts rely on its source_ref); a
        // one-line hint steers new callers at the bounded `ide.outline`/`ide.read` alternative
        // instead of retiring the mode outright.
        let hint = if byte_offset.is_none() {
            "hint: ide.outline {\"path\"} gives the skeleton and ide.read {\"path\",\"lines\"} a bounded region; this whole-file view stays for compatibility\n"
        } else {
            ""
        };
        let text = format!(
            "{hint}mode: {mode}\npath: {path}\nsource_state: {:?}\nsource_sequence: {}\nauthority_epoch: {}\ncoverage: complete registered path\nposition_encoding: {:?}\nprovider_generation: {:?}\ndocument_version: {:?}\n{diagnostic_text}\ndefinitions: {}\nreferences: {}\nlexical_matches: {}\n\n{}",
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
    /// observation still matches `path`; a full-file call without one is a creation request
    /// handled by `create_file`. A new durable prepare is the only route to Workspace; an
    /// exact prepared receipt recovered after ambiguity returns unknown and is never dispatched
    /// again. Known effects are followed by source observation and a deadline-bounded provider
    /// diagnostic refresh attached to the same reply only when it matches that post-read source;
    /// provider failure never changes a known filesystem outcome or implies cleanliness. The
    /// current host must prove this path readable before Workspace opens it during preparation.
    async fn edit(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        if let Some(stage @ JobStage::EditAwaitingCheck { .. }) = job.stage.take() {
            let deadline = match &stage {
                JobStage::EditAwaitingCheck { deadline, .. } => *deadline,
            };
            let diagnostics = if *job.cancel.borrow() || tokio::time::Instant::now() >= deadline {
                Some(EditDiagnostics::Unknown {})
            } else {
                self.check_diagnostics(&stage)
            };
            let Some(diagnostics) = diagnostics else {
                job.stage = Some(stage);
                job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                return Err(FailureCode::ProviderLoading);
            };
            let JobStage::EditAwaitingCheck {
                result,
                refreshed,
                authority,
                path,
                ..
            } = stage;
            debug_assert_eq!(result.path, path);
            let source = result
                .outcome
                .has_post_source()
                .then_some(refreshed)
                .flatten();
            // The parked stage resumes the same job that formatted the candidate, so its
            // movement note still belongs to this reply.
            let note = job
                .format_note
                .take()
                .filter(|_| result.outcome.has_post_source());
            return Ok((
                PeerReply::Edit {
                    result,
                    diagnostics,
                    note,
                    operation: edit_operation(&job.parameters),
                },
                Some(authority),
                source,
            ));
        }
        if job.parameters.get("changes").is_some() {
            return self.edit_changes(job).await;
        }
        if job.parameters.get("symbol").is_some() || job.parameters.get("lines").is_some() {
            return self.edit_by_symbol(job).await;
        }
        if job.parameters.get("source_ref").is_none() {
            return self.create_file(job).await;
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
                        note: None,
                        operation: None,
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
                        note: None,
                        operation: None,
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
        self.edit_with_source(job, request, prepared, source, true)
            .await
    }

    /// Writes one prepared edit whose base observation is already known: the full-file form
    /// resolves it from a retained detail, the symbol forms observe the file themselves.
    pub(super) async fn edit_with_source(
        &mut self,
        job: &mut Job,
        request: EditRequest,
        prepared: crate::changes::edit::PreparedEdit,
        source: SourceObservation,
        await_check: bool,
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
        let preexisting = self.pre_edit_problem_keys(&authority, &request.path);
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
                                request.path.trim_start_matches("./"),
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
        let post_reference = refreshed.as_ref().map(|_| job.reference.clone());
        let expected = EditResult::from_workspace(&request, outcome, |_| post_reference);
        let result = self
            .settle_prepared_edit(prepared, &request, expected.clone())
            .await;
        let mut diagnostics = if result == expected && result.outcome.has_post_source() {
            if crate::lang::Language::for_path(Path::new(&request.path)).is_none() {
                // No language checks this file type (a template, a manifest): nothing will ever
                // report on it, so `unknown` would send the caller looking for a result.
                EditDiagnostics::NotAnalysed {
                    reason: "no IDE language checks this file type".to_owned(),
                }
            } else {
                diagnostics
            }
        } else {
            EditDiagnostics::Unknown {}
        };
        // A scheduled project check classifies new problems against the pre-write file result;
        // its durable receipt is safe while this job is parked.
        // A multi-file operation (rename) never parks: it must write every file and answer once,
        // so it skips the wait and reports diagnostics as unknown until the next check lands.
        if await_check
            && result == expected
            && result.outcome.has_post_source()
            && let Some(stage) = self.edit_check_stage(
                job,
                &authority,
                &request.path,
                result.clone(),
                refreshed.clone(),
                (diagnostics.clone(), preexisting),
            )
        {
            match self.check_diagnostics(&stage) {
                Some(checked) => diagnostics = checked,
                None if tokio::time::Instant::now()
                    < match &stage {
                        JobStage::EditAwaitingCheck { deadline, .. } => *deadline,
                    } =>
                {
                    job.stage = Some(stage);
                    job.park_until = Some(tokio::time::Instant::now() + Duration::from_millis(250));
                    return Err(FailureCode::ProviderLoading);
                }
                None => diagnostics = EditDiagnostics::Unknown {},
            }
        }
        let source = (result.outcome.has_post_source())
            .then_some(refreshed)
            .flatten();
        // Only a settled write keeps the formatter's line movement meaningful.
        let note = job
            .format_note
            .take()
            .filter(|_| result.outcome.has_post_source());
        Ok((
            PeerReply::Edit {
                result,
                diagnostics,
                note,
                operation: edit_operation(&job.parameters),
            },
            Some(authority),
            source,
        ))
    }

    /// Captures the matching check generation and reply state for a settled edit.
    ///
    /// Returns `None` when checks are unavailable, the file type is unsupported, or no generation
    /// exists, leaving the provider answer untouched. Otherwise the saved stage retains the edit
    /// result, source, authority, path, provider fallback, and absolute check deadline so the
    /// worker can probe snapshots without repeating the write or sleeping inside the job.
    fn edit_check_stage(
        &self,
        job: &mut Job,
        authority: &AuthorityStamp,
        path: &str,
        result: EditResult,
        refreshed: Option<SourceObservation>,
        (fallback, preexisting): (EditDiagnostics, BTreeMap<String, u32>),
    ) -> Option<JobStage> {
        let feed = self.shared.project_feed.as_ref()?;
        let language = Language::for_path(std::path::Path::new(path))
            .filter(|language| language.checks().is_some())?;
        // The edit's own language is the only one forced: a `.py` edit must not also start a
        // cargo check in a mixed worktree, while every other configured language still re-arms
        // through the ordinary fingerprint-gated trigger.
        let generation =
            feed.changed_generation(&job.invocation.binding_ref().fingerprint(), Some(path))?;
        job.check_scheduled = true;
        let worktree = authority.worktree().worktree_path().to_path_buf();
        let deadline = job
            .deadline
            .checked_sub(EDIT_SETTLEMENT_RESERVE)?
            .min(tokio::time::Instant::now() + EDIT_CHECK_WAIT);
        Some(JobStage::EditAwaitingCheck {
            result,
            refreshed,
            authority: authority.clone(),
            path: path.to_owned(),
            fallback,
            generation,
            worktree,
            wanted: path.trim_start_matches("./").to_owned(),
            preexisting,
            language,
            deadline,
        })
    }

    /// Captures problems already reported for the edited file before its write is dispatched.
    fn pre_edit_problem_keys(
        &self,
        authority: &AuthorityStamp,
        path: &str,
    ) -> BTreeMap<String, u32> {
        let Some(feed) = self.shared.project_feed.as_ref() else {
            return BTreeMap::new();
        };
        let worktree = authority.worktree().worktree_path();
        let Some(language) = Language::for_path(std::path::Path::new(path)) else {
            return BTreeMap::new();
        };
        if language.checks().is_none() {
            return BTreeMap::new();
        }
        feed.latest(worktree)
            .into_iter()
            .find(|snapshot| {
                snapshot.language == language
                    && matches!(
                        snapshot.state,
                        crate::checks::CheckState::Ready | crate::checks::CheckState::Partial
                    )
            })
            .map(|snapshot| {
                problem_counts(snapshot.problems.iter().filter(|problem| {
                    normalized_problem_path(&problem.path, worktree)
                        == path.trim_start_matches("./")
                }))
            })
            .unwrap_or_default()
    }

    /// Returns the matching generation's diagnostics without waiting; absence means the job parks.
    /// Missing feed state preserves the provider fallback, while a completed non-ready snapshot
    /// and a matching ready snapshot map to the same states used by the former bounded wait.
    fn check_diagnostics(&self, stage: &JobStage) -> Option<EditDiagnostics> {
        use crate::checks::{CheckState, Severity};
        let JobStage::EditAwaitingCheck {
            fallback,
            generation,
            worktree,
            wanted,
            preexisting,
            language,
            ..
        } = stage;
        let Some(feed) = self.shared.project_feed.as_ref() else {
            return Some(fallback.clone());
        };
        let snapshot = feed.latest(worktree).into_iter().find(|snapshot| {
            snapshot.language == *language && snapshot.input_generation >= *generation
        })?;
        let not_analysed = || {
            language
                .checks()
                .and_then(|checks| checks.not_analysed(worktree, std::path::Path::new(wanted)))
        };
        if !matches!(snapshot.state, CheckState::Ready | CheckState::Partial) {
            return Some(unchecked_file_diagnostics(fallback, not_analysed()));
        }
        let mut preexisting = preexisting.clone();
        let mut errors = 0u32;
        let mut warnings = 0u32;
        let mut messages = Vec::new();
        let mut old_messages = Vec::new();
        let mut old_errors = 0u32;
        let mut old_warnings = 0u32;
        let mut truncated = snapshot.truncated;
        for problem in &snapshot.problems {
            let reported = normalized_problem_path(&problem.path, worktree);
            if reported != *wanted {
                continue;
            }
            let identity = problem_identity(problem);
            let severity = match problem.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            let code = problem
                .code
                .as_deref()
                .map(|code| format!("[{code}] "))
                .unwrap_or_default();
            let message = super::reply::bounded_utf8_prefix(
                &format!(
                    "{wanted}:{}:{} {severity} {code}{}",
                    problem.line, problem.column, problem.message
                ),
                256,
            );
            if take_preexisting_problem(&identity, &mut preexisting) {
                match problem.severity {
                    Severity::Error => old_errors += 1,
                    Severity::Warning => old_warnings += 1,
                }
                if old_messages.len() < 3 {
                    old_messages.push(message);
                }
            } else {
                match problem.severity {
                    Severity::Error => errors += 1,
                    Severity::Warning => warnings += 1,
                }
                if messages.len() < 8 {
                    messages.push(message);
                } else {
                    truncated = true;
                }
            }
        }
        Some(if messages.is_empty() && old_errors + old_warnings == 0 {
            // A capped snapshot that kept no problem of this file says nothing about it: the
            // file's problems may be among the dropped ones, so it is neither clean nor reported.
            if truncated {
                EditDiagnostics::Unknown {}
            } else if let Some(reason) = not_analysed() {
                // Naming no problem in a file the check never compiled proves nothing about it.
                unchecked_file_diagnostics(fallback, Some(reason))
            } else {
                // The file is clean; every error the check reported lies elsewhere.
                EditDiagnostics::CurrentClean {
                    project_errors: snapshot.errors,
                }
            }
        } else {
            let old_summary = if old_errors + old_warnings > 3 {
                String::new()
            } else if old_errors + old_warnings > 0 {
                format!("; pre-existing: {}", old_messages.join("; "))
            } else {
                String::new()
            };
            let mut displayed = messages
                .into_iter()
                .map(|message| format!("new: {message}"))
                .collect::<Vec<_>>();
            displayed.extend(
                old_messages
                    .iter()
                    .take(8usize.saturating_sub(displayed.len()))
                    .map(|message| format!("pre-existing: {message}")),
            );
            truncated |= old_errors + old_warnings > old_messages.len() as u32;
            EditDiagnostics::CurrentReported {
                messages: displayed,
                delta: format!(
                    "project check {:.1}s: {errors} new errors, {warnings} new warnings; \
                     {old_errors} pre-existing errors, {old_warnings} pre-existing warnings{old_summary}",
                    snapshot.duration_ms as f64 / 1000.0,
                ),
                truncated,
            }
        })
        .or_else(|| Some(fallback.clone()))
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
                note: None,
                operation: None,
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
    /// Returns whether a grant this stop actually revoked was still held (`true`), or nothing was
    /// active to revoke (`false`: no grant, or its authority was already released). On durable
    /// failure the receipt, this binding's cache keys (still non-quiescent), and its
    /// registered paths are all deliberately retained and the binding is marked pending, so a later
    /// fresh start can commit the same revoke before minting a new grant. Nothing here restores the
    /// stopped binding's source or provider authority and no stop is ever replayed: the host
    /// binding was already stopped by the ingress path and stays unusable either way. A daemon
    /// restart boot-fences old grants independently, so pending state is intentionally in-memory.
    async fn settle_revocation(&mut self, binding: &BindingRef) -> Result<bool, FailureCode> {
        self.close_provider(binding).await?;
        let Some(receipt) = self.grants.get(binding).cloned() else {
            self.pending_revocations.remove(binding);
            self.release_binding_state(binding);
            return Ok(false);
        };
        let operation = OperationId::new(format!(
            "stop-{}",
            blake3::Hash::from_bytes(binding.fingerprint()).to_hex()
        ))
        .map_err(|_| FailureCode::Internal)?;
        // The revoke is idempotent by operation id, so the daemon retries a transient store
        // failure itself; the agent never has to repeat the stop for it.
        let mut attempt = 1;
        let revoked = loop {
            self.stop_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let outcome = self
                .workspace
                .revoke(operation.clone(), &receipt, StopBindingHandoff::Confirmed)
                .await;
            match outcome {
                Err(error)
                    if attempt < STOP_REVOKE_ATTEMPTS && is_transient_stop_failure(&error) =>
                {
                    attempt += 1;
                    tokio::time::sleep(STOP_REVOKE_RETRY_PAUSE).await;
                }
                other => break other,
            }
        };
        if let Err(error) = revoked {
            // Authority that durable state already retired (a newer boot fenced it, or the same
            // grant was revoked before) is exactly what this stop asks for: answer the benign
            // nothing-active outcome instead of demanding a fresh start just to stop (E013
            // item 6). Only an uncertain application failure stays retryable.
            if matches!(
                error,
                crate::workspace::durable::DurableError::Authority(
                    crate::workspace::authority::AuthorityError::StaleAuthority
                )
            ) {
                self.grants.remove(binding);
                self.pending_revocations.remove(binding);
                self.release_binding_state(binding);
                return Ok(false);
            }
            if self.pending_revocations.len() < MAX_PENDING_REVOCATIONS {
                self.pending_revocations.insert(binding.clone());
            }
            let (code, cause) = stop_failure(&error);
            self.stop_cause = Some(cause);
            return Err(code);
        }
        self.grants.remove(binding);
        self.pending_revocations.remove(binding);
        self.release_live(binding).await;
        self.release_binding_state(binding);
        Ok(true)
    }

    /// Compares the worktree's checked-out branch or detached commit with the one this binding
    /// last saw (first read by its start) and leaves one plate notice when it changed outside the
    /// IDE. Notice only: nothing is invalidated or restarted.
    ///
    /// At most one probe per [`HEAD_PROBE_INTERVAL`] per binding reads Git's files (the live
    /// `.git` validation, `HEAD`, a loose ref and possibly `packed-refs`); a call inside the
    /// interval returns without reading, and a failed read waits out the interval as well.
    fn observe_head(&mut self, binding: &BindingRef, authority: &AuthorityStamp) {
        let probed = tokio::time::Instant::now();
        if self
            .heads
            .get(binding)
            .is_some_and(|(last, _)| probed.duration_since(*last) < HEAD_PROBE_INTERVAL)
        {
            return;
        }
        let now = crate::workspace::git::head::HeadState::read(
            authority.worktree().worktree_path(),
            authority.worktree().git_common_dir(),
        );
        let (last, seen) = self.heads.entry(binding.clone()).or_insert((probed, None));
        *last = probed;
        let Some(now) = now else {
            return;
        };
        if let Some(before) = seen.replace(now.clone())
            && let Some(notice) = before.moved_notice(&now)
            && let Ok(mut notices) = self.shared.git_notices.lock()
        {
            notices.insert(binding.fingerprint(), notice);
        }
    }

    /// Releases binding-owned state only after a committed revoke, including environment notice
    /// baselines, so stopped bindings cannot retain roots or suppress a future binding's notices.
    fn release_binding_state(&mut self, binding: &BindingRef) {
        self.quiesce_worktree_caches(binding);
        self.leases.remove(binding);
        self.registered.remove(binding);
        self.baselines.remove(binding);
        self.heads.remove(binding);
        if let Ok(mut activated) = self.shared.activated.lock() {
            activated.remove(&binding.fingerprint());
        }
        if let Ok(mut notices) = self.shared.git_notices.lock() {
            notices.remove(&binding.fingerprint());
        }
        if let Ok(mut environments) = self.shared.environments.lock() {
            environments.forget(&binding.fingerprint());
        }
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

    /// Returns when the next background retry of the pending revokes is due, scheduling it one
    /// backoff step ahead when none is scheduled yet, or `None` while no revoke is pending.
    fn schedule_revoke_retry(&mut self) -> Option<tokio::time::Instant> {
        if self.unrevoked_bindings().is_empty() {
            self.revoke_retry_rounds = 0;
            self.next_revoke_retry = None;
            return None;
        }
        let rounds = self.revoke_retry_rounds;
        Some(
            *self
                .next_revoke_retry
                .get_or_insert_with(|| tokio::time::Instant::now() + revoke_retry_delay(rounds)),
        )
    }

    /// Lists every binding whose durable grant outlives its stop: the recorded pending revokes
    /// plus any stopped binding that still owns a grant. The second part is derived from the
    /// grants themselves, so a stop that failed while the bounded pending set was full still has
    /// a retry owner.
    fn unrevoked_bindings(&self) -> Vec<BindingRef> {
        let mut found = self.pending_revocations.clone();
        found.extend(
            self.grants
                .keys()
                .filter(|binding| self.shared.active(binding).is_err())
                .cloned(),
        );
        found.into_iter().collect()
    }

    /// Retries every unrevoked stop with its original operation id, as the daemon's own
    /// background cleanup: each success releases the grant, caches, paths and lease; each failure
    /// stays for the next, later round.
    async fn retry_pending_revocations(&mut self) {
        for binding in self.unrevoked_bindings() {
            let _ = self.settle_revocation(&binding).await;
        }
        self.next_revoke_retry = None;
        self.revoke_retry_rounds = if self.unrevoked_bindings().is_empty() {
            0
        } else {
            self.revoke_retry_rounds.saturating_add(1)
        };
    }

    /// Revokes a recoverable receipt after host stop; absent grants are explicitly harmless.
    ///
    /// The reply also names the test runs this binding started but never collected — passed in by
    /// the stop submitter before it marked them read — because a stopped agent otherwise reports
    /// results it never saw (ag-20260930-210040 +403…+465 s).
    async fn revoke(
        &mut self,
        binding: &BindingRef,
        uncollected: &[String],
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        self.stop_cause = None;
        let revoked = self.settle_revocation(binding).await?;
        if self.uncertain.contains(binding) {
            return Err(FailureCode::Internal);
        }
        let mut text = if revoked {
            "Assistance stopped for this binding; compatible shared peers remain eligible"
                .to_owned()
        } else {
            // Nothing was active to stop: no grant was ever held, or its authority was already
            // released (E013 item 6) — starting just to stop would be waste.
            "nothing active for this binding; its workspace authority was already released"
                .to_owned()
        };
        if !uncollected.is_empty() {
            text.push_str(
                "\nuncollected test runs in this binding — their results were never read; each \
                 still answers ide.inspect {\"detail_ref\":\"tests #N\"}:",
            );
            for run in uncollected {
                text.push_str("\n  ");
                text.push_str(run);
            }
        }
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text,
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

/// One failed ingress or inspection check, with a closed code and resource-specific stage.
#[derive(Debug)]
struct InspectFailure {
    /// Stable protocol failure category.
    code: FailureCode,
    /// Privacy-safe stage or bounded path-specific recovery.
    stage: String,
}

impl From<FailureCode> for InspectFailure {
    /// Keeps the closed code and derives its default stage when an ingress check has no finer cause.
    fn from(code: FailureCode) -> Self {
        if code == FailureCode::Capacity {
            Self::stage(code, "inspect:queue_full")
        } else {
            Self::new(code)
        }
    }
}

impl InspectFailure {
    /// Derives the default `inspect:<reason>` stage for a path with nothing more specific.
    fn new(code: FailureCode) -> Self {
        Self {
            stage: crate::telemetry::adapters::default_stage(AssistanceTool::Inspect, &code),
            code,
        }
    }
    /// Names the exact stage this path failed at.
    fn stage(code: FailureCode, stage: &'static str) -> Self {
        Self {
            code,
            stage: stage.to_owned(),
        }
    }
}

/// Delivers only a same-binding result after fresh durable authorization and liveness checks.
/// An Activation status or completed Diff with recorded empty provenance contains no worktree
/// source paths or bytes and needs no read-path proof; other path-less details retain the
/// whole-tree proof requirement. A semantic Context also proves each retained definition and
/// reference path, so a newly denied secondary file invalidates its cached page.
async fn serve_inspection(workspace: &DurableWorkspace<'_>, shared: &Shared, request: Inspection) {
    let result: Result<PeerReply, InspectFailure> = async {
        // A known test-run handle (`tests #N`, `tests-N`, `#N`, `N`) names a background
        // run rather than a retained detail. Its status belongs to the actor and channel, so it
        // answers before the liveness check, including after `ide.stop`; an unknown id is a
        // distinct invalid-detail refusal.
        if let Some(id) = test_run_handle(&request.reference) {
            let Some(status) = shared.test_runs.find(id, &request.binding) else {
                return Ok(PeerReply::Error {
                    code: FailureCode::InvalidDetail,
                    detail: Some(format!("test:unknown_run:{id}")),
                });
            };
            let text = if status.result.is_some() {
                shared
                    .settled_test_reply(status.id, &request.binding, &status)
                    .0
            } else {
                format!(
                    "tests #{id}: running {} s; poll: call ide.test with {{\"status\": {id}}}",
                    status.age.as_secs()
                )
            };
            return Ok(PeerReply::Complete {
                kind: ResultKind::Test,
                text,
                detail_ref: None,
                truncated: false,
                continuation: false,
            });
        }
        // A poll-hint string (`… ide.test … {"status": N}`) is what every running-test line ends
        // with; a first-time caller can mistake it for a detail_ref. Answer with the exact call
        // it names instead of a generic invalid_detail.
        if let Some(id) = poll_hint_run(&request.reference) {
            return Ok(PeerReply::Complete {
                kind: ResultKind::Test,
                text: format!(
                    "tests #{id}: that text is the poll hint, not a detail_ref; call ide.test \
                     with {{\"status\": {id}}} to read this run's status"
                ),
                detail_ref: None,
                truncated: false,
                continuation: false,
            });
        }
        let active = shared
            .active(&request.binding)
            .map_err(InspectFailure::new)?;
        let (
            reply,
            authority,
            source,
            _native_epoch,
            diff_page,
            diff_page_fresh,
            context_page,
            context_page_fresh,
            diff_provenance,
        ) = {
            let ledger = shared
                .ledger
                .lock()
                .map_err(|_| InspectFailure::new(FailureCode::Internal))?;
            let detail = ledger
                .details
                .get(&request.reference)
                .filter(|detail| detail.binding == request.binding)
                .ok_or_else(|| {
                    InspectFailure::stage(
                        FailureCode::InvalidDetail,
                        // A reference this daemon never minted is unknown; one it minted and no
                        // longer retains (or issued to another binding) has expired.
                        if never_issued(&request.reference, &shared.nonce, ledger.next) {
                            "inspect:detail_unknown"
                        } else {
                            "inspect:detail_expired"
                        },
                    )
                })?;
            if request
                .expected
                .as_ref()
                .is_some_and(|expected| expected != &detail.selection)
            {
                // An inspected ide.start whose parameters differ may name another root or choice:
                // the retained reference describes the earlier request,
                // so say what actually happened instead of a generic mismatch.
                return Err(InspectFailure::stage(
                    FailureCode::InvalidDetail,
                    if detail.selection.0 == AssistanceTool::Start {
                        "start:activation_conflict"
                    } else {
                        "inspect:detail_mismatch"
                    },
                ));
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
            workspace.authorize(authority, &active).await.map_err(|_| {
                InspectFailure::stage(
                    invalidate(FailureCode::WorkspaceAuthority),
                    "inspect:authority_stale",
                )
            })?;
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
                symlink_disclosure_preflight(authority.worktree(), path).map_err(|code| {
                    InspectFailure::stage(invalidate(code), "inspect:symlink_preflight")
                })?;
            }
        }
        if let Some(source) = source
            && !source_matches(&source)
        {
            let path = super::reply::bounded_utf8_prefix(&format!("{:?}", source.path()), 256);
            return Err(InspectFailure {
                code: invalidate(FailureCode::SourceUnavailable),
                stage: format!("inspect:source_changed:{path}"),
            });
        }
        // Context pages are frozen byte slices of the page-1 snapshot, so the exact-byte check
        // above is the whole staleness contract; the native epoch no longer discards a page
        // whose bytes are unchanged (T15B follow-up). Diff keeps its own snapshot re-check in
        // its paging branch below.
        let active = shared
            .active(&request.binding)
            .map_err(InspectFailure::new)?;
        if let Some(authority) = &authority {
            workspace.authorize(authority, &active).await.map_err(|_| {
                InspectFailure::stage(
                    invalidate(FailureCode::WorkspaceAuthority),
                    "inspect:authority_stale",
                )
            })?;
        }
        shared
            .active(&request.binding)
            .map_err(InspectFailure::new)?;
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
                return Ok::<_, InspectFailure>(reply);
            }
            let Some(authority) = &authority else {
                return Err(InspectFailure::stage(
                    FailureCode::WorkspaceAuthority,
                    "inspect:authority_stale",
                ));
            };
            let expected_scope =
                crate::workspace::git::GitScope::from_authority(authority, page.mode());
            // Revalidate the retained evidence's working-tree material against the current
            // worktree before trusting it: an out-of-band edit with no native hook never bumps
            // native_epoch. Only non-staged tracked bytes are rechecked: staged blobs are
            // independent of the worktree, and untracked additions are best-effort captured
            // snapshots. Their later edits must not make tracked review unavailable.
            if !page.working_tree_bytes_unchanged(authority.worktree()) {
                return Err(InspectFailure::stage(
                    invalidate(FailureCode::SourceUnavailable),
                    "inspect:source_stale",
                ));
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
                page.provenance(),
                true,
                |max_hunks| page.expand_with_max_hunks(&expected_scope, max_hunks),
            )
            .map_err(|code| match code {
                // A structurally unavailable or failed selection can never be repaired by a later
                // page.
                FailureCode::SourceUnavailable => {
                    InspectFailure::stage(invalidate(code), "inspect:page_unavailable")
                }
                FailureCode::Capacity => InspectFailure {
                    code: FailureCode::Capacity,
                    stage: snapshots::diff_capacity_detail(
                        page.mode(),
                        authority.epoch(),
                        &request.reference,
                        page.provenance(),
                        true,
                        &page.expand_with_max_hunks(&expected_scope, 1),
                    ),
                },
                code => InspectFailure::new(code),
            })?;
            let encoded = next
                .clone()
                .encode()
                .and_then(|value| PeerReply::decode(value.as_str()));
            let Some(next) = encoded else {
                shared.set_diff_page(&request.reference, None);
                return Err(InspectFailure::new(FailureCode::Internal));
            };
            if let Ok(mut ledger) = shared.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&request.reference)
            {
                detail.reply = next.clone();
                detail.diff_page = page.advance(&advanced);
                detail.diff_page_fresh = false;
            }
            return Ok::<_, InspectFailure>(next);
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
                return Ok::<_, InspectFailure>(reply);
            }
            // Staleness is already fully covered above (source bytes and native epoch). A
            // Context detail always retains `source`, so the generic `source_matches` check
            // already ran; a Claude-captured Diff page is a frozen text snapshot with no `source`
            // and no re-derivable Git cursor (unlike the managed `diff_page` branch above), so
            // later pages of it need no further working-tree re-check either — exactly like Context.
            let (next, next_page) = page.next(&request.reference).map_err(InspectFailure::new)?;
            if let Ok(mut ledger) = shared.ledger.lock()
                && let Some(detail) = ledger.details.get_mut(&request.reference)
            {
                detail.reply = next.clone();
                detail.context_page = next_page;
                detail.context_page_fresh = false;
            }
            return Ok::<_, InspectFailure>(next);
        }
        Ok::<_, InspectFailure>(reply)
    }
    .await;
    let reply = result.unwrap_or_else(|failure| PeerReply::Error {
        code: failure.code,
        detail: Some(failure.stage),
    });
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
fn admit_initial_inspection<E: From<FailureCode>>(
    sender: &mpsc::Sender<Inspection>,
    enqueue: impl FnOnce() -> Result<String, E>,
) -> Result<(String, mpsc::OwnedPermit<Inspection>), E> {
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

/// Parses a test-run handle — `tests #N`, `tests-N`, `#N` or `N` — to its run number. A minted
/// detail reference (`<64-hex>-<n>`) never parses as one, so the alias cannot shadow a detail.
pub(crate) fn test_run_handle(reference: &str) -> Option<u64> {
    let rest = reference.strip_prefix("tests").unwrap_or(reference);
    let rest = rest.trim_start_matches(' ');
    let rest = rest
        .strip_prefix('#')
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    (rest.bytes().all(|byte| byte.is_ascii_digit()) && !rest.is_empty())
        .then(|| rest.parse::<u64>().ok())
        .flatten()
        .filter(|id| *id > 0)
}

/// Recognizes a poll-hint string an agent mistakenly passed as a `detail_ref` — text naming
/// `ide.test` and a `"status": N` argument, whatever words surround them — and returns that run
/// number, so `ide.inspect` can answer with the exact call instead of `invalid_detail`.
fn poll_hint_run(reference: &str) -> Option<u64> {
    const MARKER: &str = "\"status\":";
    if !reference.contains("ide.test") {
        return None;
    }
    let rest = &reference[reference.find(MARKER)? + MARKER.len()..];
    let digits: &str = rest.trim_start();
    let end = digits
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit())
        .map_or(digits.len(), |(at, _)| at);
    (end > 0)
        .then(|| digits[..end].parse::<u64>().ok())
        .flatten()
        .filter(|id| *id > 0)
}

/// Reports whether this daemon could never have minted `reference`: another boot's nonce, a
/// number beyond the ledger counter, or a shape no minted reference has.
fn never_issued(reference: &str, nonce: &[u8; 32], next: u64) -> bool {
    let Some((prefix, number)) = reference.rsplit_once('-') else {
        return true;
    };
    prefix != blake3::Hash::from_bytes(*nonce).to_hex().to_string()
        || number.bytes().any(|byte| !byte.is_ascii_digit())
        || number.parse::<u64>().map_or(true, |minted| minted > next)
}

/// Reads the line movement recorded in a successful edit reply for its retained source detail.
fn reply_line_movement(reply: &PeerReply) -> Option<(u32, i64)> {
    let PeerReply::Edit {
        note: Some(note), ..
    } = reply
    else {
        return None;
    };
    note.lines().find_map(|line| {
        let rest = line.strip_prefix("lines after ")?;
        let (anchor, delta) = rest.split_once(" moved ")?;
        Some((anchor.parse().ok()?, delta.parse().ok()?))
    })
}

/// Returns the exact same-binding source eligible to authorize a replacement edit.
///
/// Context, a completed Read and a successful prior Edit are the only source-producing details. A
/// prior Edit must name this exact reference as its post-read source and retain a source
/// observation for the same requested path; every other detail, missing observation, mismatched
/// binding, or failed edit is rejected as stale rather than being used to authorize bytes the
/// caller has not observed. A Context or Read whose pages are not all delivered yet is likewise
/// rejected: its reference denotes the full observed source, but the caller has only seen part of
/// it (T16B).
fn admitted_edit_source(
    detail: &Detail,
    binding: &BindingRef,
    reference: &str,
    path: &str,
) -> Option<SourceObservation> {
    let admitted = detail.binding == *binding
        // A Context or Read source_ref names the complete observed source, but its pages are the
        // only view the caller has: while any page is still undelivered the caller has not
        // observed the whole file, so a full-content replace built on it could silently truncate
        // it (T16B).
        && detail.context_page.is_none()
        && matches!(
            detail.selection.0,
            AssistanceTool::Context | AssistanceTool::Read | AssistanceTool::Edit
        )
        && match &detail.reply {
            PeerReply::Complete {
                kind: ResultKind::Context | ResultKind::Read,
                ..
            } => true,
            PeerReply::Edit { result, .. } => {
                result.source_ref.as_deref() == Some(reference) && result.outcome.has_post_source()
            }
            _ => false,
        };
    // The first file's observation, then the batch read's additional files: path disambiguates,
    // so the same reference authorizes an edit of any file the read covered.
    admitted
        .then(|| {
            detail
                .source
                .iter()
                .chain(detail.extra_sources.iter())
                .find(|source| source.path().to_str() == Some(path))
                .cloned()
        })
        .flatten()
}

/// Returns a prior edit's line shift only when it is admitted as this binding's source for `path`.
pub(super) fn admitted_edit_line_movement(
    detail: &Detail,
    binding: &BindingRef,
    reference: &str,
    path: &str,
) -> Option<(u32, i64)> {
    admitted_edit_source(detail, binding, reference, path)?;
    detail.line_movement.filter(|(_, delta)| *delta != 0)
}

/// Returns the newest retained detail reference that authorizes a full-file edit of `path` for
/// this binding, so eviction can protect that read or successful edit from removal.
fn newest_edit_source(ledger: &Ledger, binding: &BindingRef, path: &str) -> Option<String> {
    ledger
        .details
        .iter()
        .filter(|(reference, detail)| {
            admitted_edit_source(detail, binding, reference, path).is_some()
        })
        .max_by_key(|(reference, _)| {
            reference
                .rsplit_once('-')
                .and_then(|(_, number)| number.parse::<u64>().ok())
                .unwrap_or(0)
        })
        .map(|(reference, _)| reference.clone())
}

/// Normalizes a checker path to the edited file's worktree-relative form.
fn normalized_problem_path(reported: &str, worktree: &Path) -> String {
    let reported = reported.trim_start_matches("./");
    Path::new(reported)
        .strip_prefix(worktree)
        .map(|relative| relative.to_string_lossy().into_owned())
        .unwrap_or_else(|_| reported.to_owned())
}

/// Identity for diagnostic matching; positions stay display data because edits move lines.
fn problem_identity(problem: &crate::checks::Problem) -> String {
    format!(
        "{:?}:{:?}:{}",
        problem.severity, problem.code, problem.message
    )
}

/// Counts each diagnostic identity so duplicates retain their before-edit multiplicity.
fn problem_counts<'a>(
    problems: impl Iterator<Item = &'a crate::checks::Problem>,
) -> BTreeMap<String, u32> {
    let mut counts = BTreeMap::new();
    for problem in problems {
        *counts.entry(problem_identity(problem)).or_default() += 1;
    }
    counts
}

/// Consumes one matching before-edit occurrence; surplus current occurrences are new problems.
fn take_preexisting_problem(identity: &str, counts: &mut BTreeMap<String, u32>) -> bool {
    let Some(count) = counts.get_mut(identity) else {
        return false;
    };
    if *count == 0 {
        false
    } else {
        *count -= 1;
        true
    }
}

/// Answers an edit whose project check did not analyse the file (the check could not run, or it
/// skipped the file for the language's `reason`): the language server's exact post-edit report
/// stands, labelled as such; otherwise `not_analysed` with the reason, or `unknown` without one.
fn unchecked_file_diagnostics(fallback: &EditDiagnostics, reason: Option<&str>) -> EditDiagnostics {
    match (fallback, reason) {
        (
            EditDiagnostics::CurrentReported {
                messages,
                delta,
                truncated,
            },
            reason,
        ) => EditDiagnostics::CurrentReported {
            messages: messages.clone(),
            delta: match reason {
                Some(reason) => {
                    format!("{delta}; project check did not analyse this file: {reason}")
                }
                None => format!("{delta}; project check unavailable"),
            },
            truncated: *truncated,
        },
        (_, Some(reason)) => EditDiagnostics::NotAnalysed {
            reason: reason.to_owned(),
        },
        _ => EditDiagnostics::Unknown {},
    }
}

/// Checks edit diagnostic attribution against the file's prior project-check snapshot.
#[cfg(test)]
mod edit_diagnostics_tests {
    use super::*;
    use crate::checks::{Problem, Severity};

    /// An unanalysed file keeps the language server's report, never `unknown`.
    #[test]
    fn unchecked_file_keeps_the_language_server_report() {
        let reported = EditDiagnostics::CurrentReported {
            messages: vec!["pkg/mod.py:2:12 error bad".into()],
            delta: "language server reported 1 diagnostics".into(),
            truncated: false,
        };
        let EditDiagnostics::CurrentReported { delta, .. } =
            unchecked_file_diagnostics(&reported, Some("no environment"))
        else {
            panic!("the report stands");
        };
        assert!(
            delta.ends_with("did not analyse this file: no environment"),
            "{delta}"
        );
        assert!(matches!(
            unchecked_file_diagnostics(&reported, None),
            EditDiagnostics::CurrentReported { delta, .. } if delta.ends_with("project check unavailable")
        ));
        assert_eq!(
            unchecked_file_diagnostics(&EditDiagnostics::Unknown {}, Some("no environment")),
            EditDiagnostics::NotAnalysed {
                reason: "no environment".into()
            }
        );
        assert_eq!(
            unchecked_file_diagnostics(
                &EditDiagnostics::CurrentClean { project_errors: 0 },
                Some("no environment")
            ),
            EditDiagnostics::NotAnalysed {
                reason: "no environment".into()
            }
        );
        assert_eq!(
            unchecked_file_diagnostics(&EditDiagnostics::CurrentClean { project_errors: 0 }, None),
            EditDiagnostics::Unknown {}
        );
    }

    /// Keeps shifted old errors pre-existing after inserted or deleted lines.
    #[test]
    fn inserted_and_deleted_lines_keep_old_problems_preexisting() {
        let old = (1..=3)
            .map(|line| {
                Problem::new(
                    "src/lib.rs".into(),
                    line,
                    1,
                    Severity::Error,
                    Some("E0308".into()),
                    "mismatched types".into(),
                )
            })
            .collect::<Vec<_>>();
        let inserted = old
            .iter()
            .map(|problem| {
                Problem::new(
                    problem.path.clone(),
                    problem.line + 5,
                    problem.column,
                    problem.severity,
                    problem.code.clone(),
                    problem.message.clone(),
                )
            })
            .chain(std::iter::once(Problem::new(
                "src/lib.rs".into(),
                9,
                2,
                Severity::Error,
                Some("E0308".into()),
                "expected `u32`, found `String`".into(),
            )))
            .collect::<Vec<_>>();
        let before = problem_counts(old.iter());
        let (old_count, new): (Vec<_>, Vec<_>) = classify(&inserted, before.clone());
        assert_eq!(old_count.len(), 3);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].line, 9);
        assert_eq!(new[0].column, 2);

        let before_deletion = old
            .iter()
            .map(|problem| {
                Problem::new(
                    problem.path.clone(),
                    problem.line + 5,
                    problem.column,
                    problem.severity,
                    problem.code.clone(),
                    problem.message.clone(),
                )
            })
            .collect::<Vec<_>>();
        let (old_count, new) = classify(&old, problem_counts(before_deletion.iter()));
        assert_eq!(old_count.len(), 3);
        assert!(new.is_empty());
    }

    /// Treats only occurrences beyond the prior duplicate count as newly introduced.
    #[test]
    fn duplicate_diagnostics_count_only_surplus_as_new() {
        let make = |line| {
            Problem::new(
                "src/lib.rs".into(),
                line,
                1,
                Severity::Error,
                Some("E0308".into()),
                "same diagnostic".into(),
            )
        };
        let before = [make(10), make(11)];
        let after = vec![make(1), make(2), make(3)];
        let (old, new) = classify(&after, problem_counts(before.iter()));
        assert_eq!(old.len(), 2);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].line, 3);
    }

    /// Partitions current problems by consuming the corresponding prior multiset counts.
    fn classify(
        current: &[Problem],
        mut before: BTreeMap<String, u32>,
    ) -> (Vec<&Problem>, Vec<&Problem>) {
        let mut old = Vec::new();
        let mut new = Vec::new();
        for problem in current {
            if take_preexisting_problem(&problem_identity(problem), &mut before) {
                old.push(problem);
            } else {
                new.push(problem);
            }
        }
        (old, new)
    }
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
/// root outside every configured entry, or an unresolvable root refuses activation. A requested
/// root that does not exist yet gets its own cause and names the nearest existing ancestor below
/// an allowed root, so an agent told to create that directory knows where it would land.
fn activation_root(job: &mut Job, allowed_roots: &[PathBuf]) -> Result<PathBuf, FailureCode> {
    let requested = job
        .parameters
        .get("root")
        .and_then(Value::as_str)
        .map_or_else(|| job.target.candidate.clone(), PathBuf::from);
    match crate::assistance::launcher::admit_worktree(allowed_roots, &requested) {
        Ok(canonical) => Ok(canonical),
        Err(_) => {
            job.failure_detail = absent_root_detail(allowed_roots, &requested);
            Err(FailureCode::OutsideAllowedRoots)
        }
    }
}

/// Names a requested activation root that does not exist yet, and the nearest existing ancestor
/// below an allowed root, bounded to the reply's cause-path budget.
///
/// `None` keeps the historical bare refusal: either the root exists (the refusal really is about
/// allowed roots), or no ancestor below an allowed root exists to name.
fn absent_root_detail(allowed_roots: &[PathBuf], requested: &Path) -> Option<String> {
    if allowed_roots.is_empty() || std::fs::symlink_metadata(requested).is_ok() {
        return None;
    }
    let mut ancestor = requested.parent()?;
    loop {
        if crate::assistance::launcher::admit_worktree(allowed_roots, ancestor).is_ok() {
            return Some(format!(
                "start:root_absent; nearest existing ancestor below an allowed root: {}",
                super::reply::bounded_utf8_prefix(&ancestor.to_string_lossy(), 256)
            ));
        }
        ancestor = ancestor.parent()?;
    }
}

/// Names the closed cause category of a failed Git discovery for the reply and the journal.
///
/// Git's own bounded stderr distinguishes a directory that is no worktree at all from any other
/// failed or malformed discovery; the rest names the closed [`GitError`] tag, never raw output.
fn discovery_failure_detail(
    evidence: &[crate::execution::GitDiscoveryEvidence],
    error: &crate::workspace::git::GitError,
) -> String {
    let not_a_worktree = evidence.iter().any(|output| {
        output
            .stderr()
            .bytes
            .windows(b"not a git".len())
            .any(|part| part == b"not a git")
            || output
                .stderr()
                .bytes
                .windows(b"not a working tree".len())
                .any(|part| part == b"not a working tree")
    });
    if not_a_worktree {
        "start:git_discovery_failed: not a Git worktree".to_owned()
    } else {
        format!("start:git_discovery_failed: {error:?}")
    }
}

/// Validates choices before shared state changes. Every non-auto selector is admitted as a
/// project-relative or absolute path; symlink and relative escapes retain the closed refusal.
/// Project-root suffixes stay in the worktree, and language refusals preserve their reasons.
fn validate_environment(
    value: Option<&Value>,
    worktree: &Path,
    allowed_roots: &[PathBuf],
) -> Result<
    Vec<(
        crate::lang::Language,
        crate::lang::environment::EnvSelection,
    )>,
    (FailureCode, String),
> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let invalid = |reason: String| (FailureCode::InvalidDetail, format!("environment: {reason}"));
    let choices = value
        .as_object()
        .ok_or_else(|| invalid("expected an object".to_owned()))?;
    if choices.len() > 8 {
        return Err(invalid("at most 8 selections".to_owned()));
    }
    let mut result = Vec::new();
    let mut roots = allowed_roots.to_vec();
    roots.push(worktree.to_path_buf());
    for (key, value) in choices {
        let (id, root) = key.split_once(':').unwrap_or((key.as_str(), ""));
        let language = crate::lang::Language::by_id(id)
            .ok_or_else(|| invalid(format!("unknown language {id}")))?;
        let root = PathBuf::from(root);
        if root.is_absolute()
            || root
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid(format!(
                "{key}: root must be relative to the worktree"
            )));
        }
        let selector = value
            .as_str()
            .filter(|value| {
                !value.is_empty() && value.chars().count() <= 1024 && !value.contains('\0')
            })
            .ok_or_else(|| invalid(format!("{key}: invalid selector")))?;
        let project_root = worktree.join(&root);
        super::launcher::admit_path(&[worktree.to_path_buf()], &project_root)
            .map_err(|_| invalid(format!("{key}: root leaves the worktree")))?;
        if selector != "auto" {
            super::launcher::admit_path(&roots, &project_root.join(selector)).map_err(|_| {
                (
                    FailureCode::OutsideAllowedRoots,
                    // The key is JSON-quoted so a root containing spaces stays unambiguous.
                    format!(
                        "environment {} {selector}",
                        serde_json::Value::String(key.clone())
                    ),
                )
            })?;
            language
                .support()
                .check_selection(worktree, &root, selector)
                .map_err(|reason| invalid(format!("{key}: {reason}")))?;
        }
        result.push((
            language,
            crate::lang::environment::EnvSelection {
                root,
                selector: selector.to_owned(),
            },
        ));
    }
    Ok(result)
}

/// Most distinct source paths one binding keeps registered for refresh at once (F-25): the
/// registered-path working set's own budget, apart from the retained results limit.
const MAX_REGISTERED_PATHS: usize = 256;

/// One binding's registered source paths with the moment each was last read.
///
/// Registration makes a path part of the native-hint refresh set; reading a path again counts as
/// a use. When the set is full, [`Self::evict_least_recently_used`] drops the path nobody read for
/// longest, so a long session's working set follows what it reads instead of growing for its
/// whole life.
#[derive(Clone, Debug, Default)]
struct RegisteredPaths(BTreeMap<std::path::PathBuf, tokio::time::Instant>);

impl RegisteredPaths {
    /// Registers `path` (or marks it used again) as of now.
    fn insert(&mut self, path: std::path::PathBuf) {
        self.insert_used_at(path, tokio::time::Instant::now());
    }

    /// Registers `path` as last used at `used`.
    fn insert_used_at(&mut self, path: std::path::PathBuf, used: tokio::time::Instant) {
        self.0.insert(path, used);
    }

    /// Keeps `path` registered without touching its last-use time (registering it as used now
    /// only when it was not registered).
    fn keep(&mut self, path: std::path::PathBuf) {
        self.0.entry(path).or_insert_with(tokio::time::Instant::now);
    }

    /// Reports whether `path` is registered.
    fn contains(&self, path: &std::path::Path) -> bool {
        self.0.contains_key(path)
    }

    /// Returns how many paths are registered.
    fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns the registered paths, ordered.
    fn paths(&self) -> Vec<std::path::PathBuf> {
        self.0.keys().cloned().collect()
    }

    /// Drops the registered path whose last use is oldest.
    fn evict_least_recently_used(&mut self) {
        if let Some(oldest) = self
            .0
            .iter()
            .min_by_key(|(_, used)| **used)
            .map(|(path, _)| path.clone())
        {
            self.0.remove(&oldest);
        }
    }
}

/// First pause before the daemon's own background retry of a pending revoke; each further round
/// doubles it, up to [`PENDING_REVOKE_RETRY_MAX`].
const PENDING_REVOKE_RETRY_FIRST: Duration = Duration::from_secs(1);
/// Longest pause between two background retry rounds of a pending revoke.
const PENDING_REVOKE_RETRY_MAX: Duration = Duration::from_secs(16);

/// Returns the pause before background retry round `rounds` (0 first): 1 s doubling to 16 s.
fn revoke_retry_delay(rounds: u32) -> Duration {
    PENDING_REVOKE_RETRY_FIRST
        .saturating_mul(1_u32.checked_shl(rounds).unwrap_or(u32::MAX))
        .min(PENDING_REVOKE_RETRY_MAX)
}

/// Attempts of one durable revoke before a transient store failure is reported (F-12).
const STOP_REVOKE_ATTEMPTS: usize = 2;
/// Pause between two attempts of the same idempotent revoke.
const STOP_REVOKE_RETRY_PAUSE: Duration = Duration::from_millis(100);

/// Reports whether a store failure is SQLite's own "database is locked/busy": another connection
/// held the write lock past the busy timeout, which the store surfaces as an infrastructure error
/// carrying SQLite's message rather than as [`crate::app::store::StoreError::Busy`].
fn is_locked_store(error: &crate::app::store::StoreError) -> bool {
    matches!(
        error,
        crate::app::store::StoreError::Infrastructure(message)
            if message.contains("locked") || message.contains("busy")
    )
}

/// Reports whether a durable stop failure is transient: the store was busy or locked, its queue
/// was full, or the wait expired after the work was accepted. Retrying the same operation id is
/// then safe and returns the committed outcome if the first attempt had landed.
fn is_transient_stop_failure(error: &crate::workspace::durable::DurableError) -> bool {
    use crate::app::store::StoreError;
    use crate::workspace::durable::DurableError;
    match error {
        DurableError::Application(error) => {
            matches!(
                error,
                StoreError::Busy | StoreError::QueueFull | StoreError::OutcomeUnknown { .. }
            ) || is_locked_store(error)
        }
        _ => false,
    }
}

/// Maps one durable stop failure to its failure code and typed `stop:` cause, so a store
/// problem is never reported as an authority problem (F-12).
///
/// Busy, locked or full queue: `capacity` / `stop:busy`. Receipt store full: `capacity` /
/// `stop:store_full`. Wait expired with the outcome unknown: `deadline` / `stop:store_deadline`.
/// Every other store or state failure: `workspace_authority` / `stop:store_unavailable`; an
/// authority refusal or operation conflict: `workspace_authority` / `stop:authority`.
fn stop_failure(error: &crate::workspace::durable::DurableError) -> (FailureCode, &'static str) {
    use crate::app::store::StoreError;
    use crate::workspace::durable::DurableError;
    match error {
        DurableError::Application(StoreError::Busy | StoreError::QueueFull) => {
            (FailureCode::Capacity, "stop:busy")
        }
        DurableError::Application(error) if is_locked_store(error) => {
            (FailureCode::Capacity, "stop:busy")
        }
        DurableError::Application(StoreError::ReceiptCapacityExhausted) => {
            (FailureCode::Capacity, "stop:store_full")
        }
        DurableError::Application(StoreError::OutcomeUnknown { .. }) => {
            (FailureCode::Deadline, "stop:store_deadline")
        }
        DurableError::Authority(_) | DurableError::OperationConflict => {
            (FailureCode::WorkspaceAuthority, "stop:authority")
        }
        _ => (FailureCode::WorkspaceAuthority, "stop:store_unavailable"),
    }
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

/// Reports whether Git discovery failed because no repository exists at all, not because one is
/// broken.
///
/// Only called after Git itself failed: the walk looks for a `.git` (directory or gitfile,
/// without following a symlinked name) in the candidate and every ancestor. None anywhere means
/// the directory is plain and activates without Git data; an existing `.git` keeps the activation
/// refusal — that is a real repository with broken Git. Any inspection failure other than a
/// missing name is unproven, so it refuses as well.
fn plain_directory_without_git(candidate: &Path) -> bool {
    candidate
        .ancestors()
        .all(|dir| match std::fs::symlink_metadata(dir.join(".git")) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(_) => false,
        })
}

/// Proves only the complete absence of a `.git` makes a directory plain: one at the candidate,
/// one anywhere above it, or an unproven inspection each keep the activation refusal.
#[test]
fn only_a_directory_without_any_git_is_plain() {
    let base = std::env::temp_dir().join(format!("plain-probe-{}", std::process::id()));
    let folder = base.join("folder");
    std::fs::create_dir_all(&folder).unwrap();
    assert!(plain_directory_without_git(&folder));
    std::fs::create_dir_all(folder.join(".git")).unwrap();
    assert!(!plain_directory_without_git(&folder));
    std::fs::remove_dir_all(folder.join(".git")).unwrap();
    assert!(plain_directory_without_git(&folder));
    std::fs::write(folder.join(".git"), "gitdir: elsewhere\n").unwrap();
    assert!(!plain_directory_without_git(&folder));
    std::fs::remove_file(folder.join(".git")).unwrap();
    let nested = folder.join("child/grandchild");
    std::fs::create_dir_all(&nested).unwrap();
    assert!(plain_directory_without_git(&nested));
    std::fs::create_dir_all(folder.join("child/.git")).unwrap();
    assert!(!plain_directory_without_git(&nested));
    std::fs::remove_dir_all(&folder).unwrap();
}

/// Renders one wall-clock millisecond instant as the journal's UTC second line, or `unknown`
/// for the zero no-time-yet case.
fn timestamp_line(ms: u64) -> String {
    if ms == 0 {
        return "unknown".to_owned();
    }
    crate::errorlog::format_rfc3339(ms / 1000)
}

/// The stable default activation id for one binding: a start that names no `activation_id`
/// derives its operation from the binding alone, so repeating that start returns the same
/// activation (E013 item 4).
fn default_activation_id(binding: &BindingRef) -> String {
    format!(
        "binding-{}",
        &blake3::Hash::from_bytes(binding.fingerprint())
            .to_hex()
            .as_str()[..24]
    )
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
        AssistanceTool::Graph => crate::errorlog::Method::Graph,
        AssistanceTool::Test => crate::errorlog::Method::Test,
    }
}

/// Drives `future` to completion, returning the payload of a panic raised while polling it
/// instead of unwinding through the caller. The future is dropped once it has panicked.
async fn catch_panic<F: std::future::Future>(
    future: F,
) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|cx| {
        let polled = crate::errorlog::catch_job_panic(|| {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx)))
        });
        match polled {
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Ok(std::task::Poll::Ready(output)) => std::task::Poll::Ready(Ok(output)),
            Err(payload) => std::task::Poll::Ready(Err(payload)),
        }
    })
    .await
}

/// Test seam: panics a `test-seams` build's `ide.read` of the path named by
/// `AGENT_IDE_TEST_PANIC_READ_PATH`, so the product tests can prove the worker survives a job
/// panic. The panic does not exist in a build without the feature.
#[cfg(feature = "test-seams")]
fn panic_seam(job: &Job) {
    if let Some(path) = crate::test_seams::var("AGENT_IDE_TEST_PANIC_READ_PATH")
        && job.parameters.get("path").and_then(Value::as_str) == Some(path.as_str())
    {
        panic!("agent-ide test seam: deliberate job panic for {path} token=example-secret");
    }
}

/// Without the `test-seams` feature there is no panic seam.
#[cfg(not(feature = "test-seams"))]
fn panic_seam(_job: &Job) {}

/// Projects detected at the worktree root in registration order, those with a root manifest
/// first: a language present only by its files never shadows one the root declares.
fn test_projects(root: &Path) -> Vec<(crate::lang::Language, LanguageProject)> {
    let mut projects: Vec<_> = crate::lang::registered()
        .iter()
        .filter_map(|&language| Some((language, language.support().detect(root)?)))
        .collect();
    projects.sort_by_key(|(_, project)| project.manifests.is_empty());
    projects
}

/// Chooses the first language of [`test_projects`].
fn detect_test_language(root: &Path) -> Option<crate::lang::Language> {
    test_projects(root).first().map(|(language, _)| *language)
}

/// Resolves a target through the detected runner, returning argv, language, and an optional
/// user-facing selected-test summary (`None` when the runner cannot enumerate tests). A target
/// that names a file runs that file's language's runner — in a mixed worktree a `.py` test path
/// selects pytest, never the first detected project's cargo — falling back to the first detected
/// project for a bare pattern or an undetected language. Returns
/// [`crate::lang::LangError::Unsupported`] when no runner supports the target.
fn test_selection(
    root: &Path,
    target: crate::lang::TestTarget,
) -> Result<(Vec<String>, crate::lang::Language, Option<String>), crate::lang::LangError> {
    let projects = test_projects(root);
    let target_file = match &target {
        crate::lang::TestTarget::Symbol { path, .. } => path.file(),
        crate::lang::TestTarget::File(path) => Some(path.as_path()),
        crate::lang::TestTarget::Pattern(_) => None,
    };
    let Some((language, project)) = target_file
        .and_then(crate::lang::Language::for_path)
        .and_then(|language| projects.iter().find(|(detected, _)| *detected == language))
        .or_else(|| projects.first())
    else {
        return Err(crate::lang::LangError::Unsupported(
            "no supported test runner was detected".to_owned(),
        ));
    };
    let selection = language.support().test_selection(project, &target)?;
    let count = (!selection.tests.is_empty())
        .then_some(format!("{} tests selected", selection.tests.len()));
    Ok((selection.command, *language, count))
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

/// The reply line for a runner that could not start. A missing executable of the language's own
/// runner selection says what is missing and the next step — the project environment, or an
/// exact command — instead of quoting errno; every other spawn failure (and every explicit
/// command, whose program the caller named themselves) keeps the program and the error.
fn missing_runner_text(program: &str, error: &str, not_found: bool, root: &Path) -> String {
    if not_found {
        let program = test_text_line(program, 160);
        if program.contains('/') {
            return format!(
                "tests: could not start {program} — not found; recreate the project \
                 environment, or run an exact command with ide.test {{\"command\":[...]}}"
            );
        }
        // A project virtual environment would have put this program on an absolute path, so a
        // bare name that is missing also says none was found.
        let venv = if [".venv", "venv"].iter().any(|dir| root.join(dir).is_dir()) {
            ""
        } else {
            " and the project has no .venv"
        };
        return format!(
            "tests: could not start {program} — not found on PATH{venv}; create the project \
             environment, or run an exact command with ide.test {{\"command\":[...]}}"
        );
    }
    format!(
        "tests: could not start {}: {}",
        test_text_line(program, 160),
        test_text_line(error, 240)
    )
}

/// Failed tests listed in a run's reply; the rest are counted and live in the full output.
const MAX_LISTED_FAILURES: usize = 16;

/// The rerun line's command: `cd <dir> && ` first when the run used a working directory.
fn rerun_text(result: &super::tests::RunResult) -> String {
    let command = display_argv(&result.command);
    match &result.rerun_dir {
        Some(dir) => format!(
            "cd {} && {command}",
            display_argv(std::slice::from_ref(dir))
        ),
        None => command,
    }
}

/// Renders the bounded parsed test result and actionable rerun/detail references.
fn test_result_text(
    id: u64,
    result: &super::tests::RunResult,
    owns_detail: bool,
    _explicit_command: bool,
) -> String {
    // Without recognized test counts, show the process result and its bounded output tail.
    if super::tests::summary_absent(result) {
        return command_result_text(id, result, owns_detail);
    }
    let report = &result.report;
    let mut text = super::tests::result_line(id, result);
    for failure in report.failures.iter().take(MAX_LISTED_FAILURES) {
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
    if report.failures.len() > MAX_LISTED_FAILURES {
        text.push_str(&format!(
            "\n  (+{} more failed tests in the full output)",
            report.failures.len() - MAX_LISTED_FAILURES
        ));
    }
    text.push_str(&format!("\n  rerun: {}", rerun_text(result)));
    if owns_detail {
        text.push_str(&format!(
            "\n  full output: ide.inspect {}",
            result.detail_ref
        ));
    }
    text
}

/// Renders one summary-less run: exit code, chosen environment, bounded output tail, rerun, and —
/// only when that tail cut something, or the whole output already lives in the run's
/// paged detail — the pointer to the full output.
fn command_result_text(id: u64, result: &super::tests::RunResult, owns_detail: bool) -> String {
    let seconds = result.elapsed.as_secs();
    let exit = result
        .exit
        .map_or_else(|| "unknown".to_owned(), |code| code.to_string());
    let mut text = format!("tests #{id}: exit {exit}, {seconds} s");
    if let Some(label) = &result.environment_label {
        text.push_str(&format!(" · env {label}"));
    }
    let tail = super::tests::output_tail(&result.output);
    if !tail.is_empty() {
        text.push_str("\n  output (tail):\n");
        text.push_str(&tail);
    }
    text.push_str(&format!("\n  rerun: {}", rerun_text(result)));
    if owns_detail && (tail.len() < result.output.len() || result.output_paged) {
        text.push_str(&format!(
            "\n  full output: ide.inspect {}",
            result.detail_ref
        ));
    }
    text
}

/// Checks the bounded command reply, poll-hint parsing and the user-facing relationship limits.
#[cfg(test)]
mod tool_reply_fix_tests {
    use super::*;
    use crate::lang::TestReport;

    /// Builds a minimal explicit-command result for reply-rendering checks.
    fn command_result(output: String) -> super::super::tests::RunResult {
        super::super::tests::RunResult {
            report: TestReport {
                incomplete: true,
                ..TestReport::default()
            },
            output,
            output_paged: false,
            elapsed: Duration::from_secs(1),
            stopped: false,
            exit: Some(0),
            budget: Duration::from_secs(30),
            detail_ref: "test-detail".into(),
            command: vec!["echo".into(), "hello".into()],
            rerun_dir: None,
            environment_label: None,
        }
    }

    /// Quotes output when no known runner summary exists, regardless of how the runner was launched.
    #[test]
    fn summaryless_output_is_labeled_for_any_launch_form() {
        let result = command_result("hello\n".into());
        let command = test_result_text(3, &result, true, true);
        assert!(command.contains("exit 0, 1 s"), "{command}");
        assert!(command.contains("output (tail):\nhello\n"), "{command}");
        assert!(command.contains("rerun: echo hello"), "{command}");

        let runner = test_result_text(3, &result, true, false);
        assert!(runner.contains("exit 0, 1 s"), "{runner}");
        assert!(runner.contains("output (tail):\nhello\n"), "{runner}");
        let mut selected = result;
        selected.environment_label = Some("two".into());
        assert!(
            test_result_text(3, &selected, true, false)
                .starts_with("tests #3: exit 0, 1 s · env two")
        );
    }

    /// A missing runner says what is missing and the next step instead of quoting errno — a bare
    /// program also says no project environment was found — while any other spawn failure keeps
    /// the program and the error.
    #[test]
    fn a_missing_runner_teaches_the_next_step() {
        let empty = Path::new("/definitely/empty");
        assert_eq!(
            missing_runner_text(
                "pytest",
                "No such file or directory (os error 2)",
                true,
                empty
            ),
            "tests: could not start pytest — not found on PATH and the project has no .venv; \
             create the project environment, or run an exact command with ide.test \
             {\"command\":[...]}"
        );
        assert_eq!(
            missing_runner_text(
                "/repo/.venv/bin/runner",
                "No such file or directory (os error 2)",
                true,
                empty
            ),
            "tests: could not start /repo/.venv/bin/runner — not found; recreate the project \
             environment, or run an exact command with ide.test {\"command\":[...]}"
        );
        assert_eq!(
            missing_runner_text("pytest", "Permission denied (os error 13)", false, empty),
            "tests: could not start pytest: Permission denied (os error 13)"
        );
    }

    /// A failed summary-less command surfaces the exit and bounded output with an explicit label.
    #[test]
    fn explicit_failed_command_labels_output_instead_of_claiming_runner_reason() {
        let mut result = command_result("noise\nERROR: no collectors\n".into());
        result.exit = Some(4);
        let reply = command_result_text(4, &result, true);
        assert!(
            reply.starts_with(
                "tests #4: exit 4, 1 s\n  output (tail):\nnoise\nERROR: no collectors\n"
            ),
            "{reply}"
        );
        assert!(!reply.contains("runner said"), "{reply}");
    }

    /// Limits inline output at a UTF-8 boundary and points to the retained full output.
    #[test]
    fn command_reply_bounds_output_and_names_full_detail() {
        let line = format!("{}\n", "é".repeat(4096 / 2 + 50));
        let result = command_result(line);
        let reply = command_result_text(4, &result, true);
        let quoted = reply
            .split("  output (tail):\n")
            .nth(1)
            .unwrap()
            .split("\n  rerun:")
            .next()
            .unwrap();
        assert!(quoted.len() <= 4096, "{} bytes", quoted.len());
        assert!(quoted.is_char_boundary(quoted.len()));
        assert!(
            reply.contains("full output: ide.inspect test-detail"),
            "{reply}"
        );
    }

    /// Recognizes the prose poll hint without mistaking other `status` text for a run handle.
    #[test]
    fn poll_hint_points_to_the_status_call() {
        assert_eq!(
            poll_hint_run("poll: call ide.test with {\"status\": 12}"),
            Some(12)
        );
        assert_eq!(poll_hint_run("ide.test {\"status\": 0}"), None);
        assert_eq!(poll_hint_run("ide.inspect {\"status\": 12}"), None);
    }

    /// Keeps 0–3 visible in descriptions as well as JSON schema bounds for hosts that drop bounds.
    #[test]
    fn symbol_relationship_descriptions_state_their_range() {
        let schemas = super::super::facade::tool_schemas();
        for field in ["callers", "callees"] {
            let description = schemas[8].input_schema["properties"][field]["description"]
                .as_str()
                .unwrap();
            assert!(description.contains("0–3"), "{description}");
        }
    }
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
        assert_eq!(job_failure_outcome(code.clone()), expected, "{code:?}");
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
        checks::{CheckState, Problem, ProblemSnapshot, Severity},
        intelligence::freshness::{CacheIdentity, CacheLifecycle},
    };
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

    /// A missing requested start root names the nearest existing directory still under policy.
    #[test]
    fn absent_start_root_names_its_nearest_allowed_ancestor() {
        let fixture = Fixture::new();
        let allowed = vec![fixture.root.clone()];
        let requested = fixture.root.join("not-created/child");
        let detail = absent_root_detail(&allowed, &requested).unwrap();
        assert!(detail.starts_with("start:root_absent;"));
        assert!(detail.contains(&fixture.root.display().to_string()));
    }

    /// A native hook has a hint target only after the exact channel has an activation.
    #[test]
    fn inactive_channel_has_no_native_hint_target() {
        let launcher = LauncherConfig::parse(
            br#"{"version":1,"limits":{"queued":1,"details":1,"operation_ms":1000,"output_bytes":1024},"targets":[]}"#,
        )
        .unwrap();
        let bindings = Arc::new(Mutex::new(HostBindingGuard::default()));
        let invocation = validated_call(&bindings, "native-hint-actor", "native-hint-call");
        let binding = invocation.binding_ref().clone();
        let handle = WorkerHandle::new(
            bindings,
            launcher,
            [4; 32],
            Arc::new(Mutex::new(admission_controller())),
        );
        assert!(!handle.channel_activated(&binding));
        handle
            .shared
            .activated
            .lock()
            .unwrap()
            .insert(binding.fingerprint());
        assert!(handle.channel_activated(&binding));
    }

    /// Creates one current host binding for a managed job.
    fn production_call(worker: &Worker<'_>, actor: &str, id: &str) -> ValidatedInvocation {
        validated_call(&worker.shared.bindings, actor, id)
    }

    /// Validates one real host binding against one binding guard, shared by the `Worker`
    /// fixture and the `WorkerHandle` enqueue tests. Bindings are keyed by the actor alone,
    /// so repeated calls with one actor return invocations of the same binding.
    fn validated_call(
        bindings: &Arc<Mutex<HostBindingGuard>>,
        actor: &str,
        id: &str,
    ) -> ValidatedInvocation {
        let mut guard = bindings.lock().unwrap();
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

    /// Builds the fixture launcher with the worktree's temporary base as its allowed root and
    /// the requested details ceiling.
    fn production_launcher(root: &std::path::Path, details: usize) -> LauncherConfig {
        production_launcher_with(root, details, serde_json::json!([]))
    }

    /// [`production_launcher`] with the given accepted provider declarations on its one target.
    fn production_launcher_with(
        root: &std::path::Path,
        details: usize,
        providers: Value,
    ) -> LauncherConfig {
        let git = std::path::Path::new("/usr/bin/git");
        let executable = serde_json::json!({
            "path":git,
            "identity":"fixture-git",
            "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
        });
        let config = serde_json::json!({
            "version":1,
            "limits":{"queued":8,"details":details,"operation_ms":5000,"output_bytes":1024},
            "allowed_roots":[root.parent().unwrap()],
            "targets":[{
                "attachment":"stop-retry",
                "candidate":root,
                "git":executable,
                "providers":providers
            }]
        });
        LauncherConfig::parse(config.to_string().as_bytes()).unwrap()
    }

    /// Returns the target of a launcher that accepts the recording fixture provider of the
    /// `.epsilon` test language, so provider ownership runs through the production worker paths.
    fn provider_target(root: &std::path::Path) -> LaunchTarget {
        let git = std::path::Path::new("/usr/bin/git");
        production_launcher_with(
            root,
            8,
            serde_json::json!([{
                "executable":{
                    "path":git,
                    "identity":"fixture-provider",
                    "blake3":blake3::hash(&std::fs::read(git).unwrap()).to_hex().to_string()
                },
                "settings":"fixture_epsilon",
                "toolchain":"fixture-toolchain",
                "trust":"fixture-trust",
                "cache_namespace":"fixture-epsilon-cache"
            }]),
        )
        .target("stop-retry")
        .unwrap()
        .clone()
    }

    /// Runs the production `Worker::activate` with the fixture provider configured, for `root`
    /// when given (a sibling worktree) and the fixture worktree otherwise, in the requested mode.
    /// Returns the binding.
    async fn provider_start(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
        read_only: bool,
        root: Option<&std::path::Path>,
    ) -> Result<BindingRef, FailureCode> {
        crate::lang::testing::install();
        let invocation = production_call(worker, actor, id);
        let binding = invocation.binding_ref().clone();
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut parameters = serde_json::json!({"activation_id":actor,"read_only":read_only});
        if let Some(root) = root {
            parameters["root"] = serde_json::json!(root);
        }
        let mut job = Job {
            reference: format!("provider-{id}"),
            invocation,
            tool: AssistanceTool::Start,
            parameters,
            target: provider_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
        };
        worker.activate(&mut job).await?;
        Ok(binding)
    }

    /// Resolves `file` (an `.epsilon` source in `worktree`) through the production
    /// `Worker::live_session_for` as `actor`'s symbol call, returning how it answered. The
    /// recording fixture provider answers `ProviderLoading` once it holds the retained namespace.
    async fn provider_symbol_call(
        worker: &mut Worker<'_>,
        actor: &str,
        call: &str,
        file: &std::path::Path,
    ) -> Result<(), FailureCode> {
        let invocation = production_call(worker, actor, call);
        let binding = invocation.binding_ref().clone();
        let (observed, _) = worker.observe(&binding, file.to_path_buf()).await?;
        let (cancel_sender, cancel) = watch::channel(false);
        let _keep = cancel_sender;
        let mut job = Job {
            reference: format!("provider-{call}"),
            invocation,
            tool: AssistanceTool::Symbol,
            parameters: serde_json::json!({}),
            target: provider_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
        };
        let outcome = worker
            .live_session_for(&mut job, &observed)
            .await
            .map(|_| ());
        assert!(
            job.session_binding.is_none(),
            "the borrowed owner is cleared after the provider call"
        );
        outcome
    }

    /// Returns the target selected by the fixture's trusted launcher attachment.
    fn production_target(root: &std::path::Path) -> LaunchTarget {
        production_launcher(root, 8)
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
        production_start_mode(worker, actor, id, false).await
    }

    /// Runs `Worker::activate` using the requested explicit read-only mode.
    async fn production_start_mode(
        worker: &mut Worker<'_>,
        actor: &str,
        id: &str,
        read_only: bool,
    ) -> (BindingRef, StartReceipt) {
        let invocation = production_call(worker, actor, id);
        let binding = invocation.binding_ref().clone();
        let (_cancel_sender, cancel) = watch::channel(false);
        let mut job = Job {
            reference: format!("production-{id}"),
            invocation,
            tool: AssistanceTool::Start,
            parameters: serde_json::json!({"activation_id":id,"read_only":read_only}),
            target: production_target(&worker.runtime),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
        };
        worker.activate(&mut job).await.unwrap();
        let receipt = worker.grants.get(&binding).cloned().unwrap();
        (binding, receipt)
    }

    /// Failed cache admission preserves the caller's existing activation and never persists a choice.
    #[tokio::test]
    async fn environment_repeat_cache_failure_preserves_activation_and_selection() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("env.fixture"), "one\ntwo\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, receipt) =
            production_start(&mut worker, "cache-failure-env", "cache-env-start").await;
        let cache = worker.runtime.join("cache");
        std::fs::remove_dir_all(&cache).unwrap();
        std::fs::write(&cache, "not a cache directory").unwrap();
        let (mut job, _cancel) = start_job(
            &worker,
            "cache-failure-env",
            "cache-env-again",
            serde_json::json!({"activation_id":"cache-env-start","environment":{"alpha":"two"}}),
        );
        assert_eq!(
            worker.activate(&mut job).await.unwrap_err(),
            FailureCode::ProviderUnavailable
        );
        assert!(worker.shared.active(&binding).is_ok());
        assert_eq!(
            worker.authority(&binding).await.unwrap().epoch(),
            receipt.epoch()
        );
        assert!(
            crate::lang::environment::selections(&fixture.root, crate::lang::testing::ALPHA)
                .is_empty()
        );
        worker
            .workspace
            .load_environment(receipt.worktree())
            .await
            .unwrap();
        assert!(
            crate::lang::environment::selections(&fixture.root, crate::lang::testing::ALPHA)
                .is_empty()
        );
    }

    /// Relative escapes and symlink escapes are refused before language resolution; plain labels work.
    #[test]
    fn review_environment_selector_admission() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.base.join("outside")).unwrap();
        std::os::unix::fs::symlink(fixture.base.join("outside"), fixture.root.join("linked"))
            .unwrap();
        std::fs::write(
            fixture.root.join("env.fixture"),
            "one\n../outside\nlinked\n",
        )
        .unwrap();
        let allowed = vec![fixture.root.clone()];
        for selector in ["../outside", "linked"] {
            let error = validate_environment(
                Some(&serde_json::json!({"alpha":selector})),
                &fixture.root,
                &allowed,
            )
            .unwrap_err();
            assert_eq!(
                error,
                (
                    FailureCode::OutsideAllowedRoots,
                    format!("environment \"alpha\" {selector}")
                )
            );
        }
        assert!(
            validate_environment(
                Some(&serde_json::json!({"alpha":"one"})),
                &fixture.root,
                &[]
            )
            .is_ok()
        );
        for key in ["alpha:../outside", "alpha:/absolute"] {
            assert_eq!(
                validate_environment(
                    Some(&serde_json::json!({key:"one"})),
                    &fixture.root,
                    &allowed
                )
                .unwrap_err()
                .0,
                FailureCode::InvalidDetail
            );
        }
    }

    /// Choosing the discovered winner leaves its identity and check count unchanged.
    #[tokio::test]
    async fn review_environment_winner_selection_is_a_noop() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("env.fixture"), "one\ntwo\n").unwrap();
        std::fs::write(fixture.root.join("alpha.toml"), "").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let language = crate::lang::testing::ALPHA;
        let checker = Arc::new(crate::checks::FakeChecker::new(
            language,
            crate::checks::ProblemSnapshot::from_problems(
                language,
                crate::checks::CheckState::Ready,
                Vec::new(),
                1,
                0,
            ),
        ));
        let scheduler = crate::checks::scheduler::Scheduler::new(
            vec![checker.clone()],
            Duration::from_millis(1),
            1,
            fixture.base.join("cache"),
        );
        let feed = Arc::new(ProjectProblemFeed::new(
            scheduler.clone(),
            vec![fixture.root.clone()],
            vec![language],
        ));
        Arc::get_mut(&mut worker.shared).unwrap().project_feed = Some(feed.clone());
        let (binding, _) = production_start(&mut worker, "review-env", "review-env-start").await;
        feed.activated(
            binding.fingerprint(),
            &fixture.root,
            Path::new("repo"),
            false,
        );
        tokio::time::timeout(Duration::from_secs(3), async {
            while scheduler.latest(&fixture.root).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let before = checker.requests().len();
        let (mut job, _cancel) = start_job(
            &worker,
            "review-env",
            "review-env-select",
            serde_json::json!({"activation_id":"review-env-start","environment":{"alpha":"one"}}),
        );
        worker.activate(&mut job).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(checker.requests().len(), before);
        worker.settle_revocation(&binding).await.unwrap();
        assert!(
            worker
                .shared
                .environments
                .lock()
                .unwrap()
                .root(&binding.fingerprint())
                .is_none()
        );
        scheduler.shutdown().await;
    }

    /// Stop cleanup releases both the binding's root and its environment delivery bookkeeping.
    #[tokio::test]
    async fn review_environment_binding_is_pruned_on_stop() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let invocation = production_call(&worker, "review-prune", "review-prune-call");
        let binding = invocation.binding_ref();
        worker
            .shared
            .environments
            .lock()
            .unwrap()
            .bind(binding.fingerprint(), &fixture.root);
        worker.release_binding_state(binding);
        assert!(
            worker
                .shared
                .environments
                .lock()
                .unwrap()
                .root(&binding.fingerprint())
                .is_none()
        );
    }

    /// Selection validation retains language reasons, admission policy and the uniform reader refusal.
    #[tokio::test]
    async fn environment_start_validation_and_same_activation_changes() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("env.fixture"), "one\ntwo\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, receipt) = production_start(&mut worker, "env-actor", "env-start").await;
        for selector in ["two", "one", "auto"] {
            let (mut job, _cancel) = start_job(
                &worker,
                "env-actor",
                &format!("env-call-{selector}"),
                serde_json::json!({"activation_id":"env-start","environment":{"alpha":selector}}),
            );
            let (reply, authority, _) = worker.activate(&mut job).await.unwrap();
            assert_eq!(
                authority.unwrap().epoch(),
                receipt.epoch(),
                "environment is excluded from activation identity"
            );
            let PeerReply::Complete { text, .. } = reply else {
                panic!("{reply:?}")
            };
            if selector == "auto" {
                assert!(
                    crate::lang::environment::selections(
                        &fixture.root,
                        crate::lang::testing::ALPHA
                    )
                    .is_empty()
                );
                assert!(text.contains("alpha one (1.2.3, discovered)"), "{text}");
            } else {
                assert!(
                    text.contains(&format!("alpha {selector} (1.2.3, selected)")),
                    "{text}"
                );
            }
        }
        for (index, (environment, code, reason)) in [
            (
                serde_json::json!({"unknown":"two"}),
                FailureCode::InvalidDetail,
                "unknown language",
            ),
            (
                serde_json::json!({"alpha":"absent"}),
                FailureCode::InvalidDetail,
                "candidate absent; choose a listed environment",
            ),
            (
                serde_json::json!({"beta":"two"}),
                FailureCode::InvalidDetail,
                "candidate absent",
            ),
            (
                serde_json::json!({"alpha":"/outside/environment"}),
                FailureCode::OutsideAllowedRoots,
                "environment \"alpha\" /outside/environment",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let (mut job, _cancel) = start_job(
                &worker,
                "env-actor",
                &format!("env-invalid-{index}"),
                serde_json::json!({"activation_id":"env-start","environment":environment}),
            );
            let (reply, _, _) = worker.activate(&mut job).await.unwrap();
            let PeerReply::Error {
                code: actual,
                detail: Some(detail),
            } = reply
            else {
                panic!("{reply:?}")
            };
            assert_eq!(actual, code);
            assert!(detail.contains(reason), "{detail}");
        }
        let (mut reader, _cancel) = start_job(
            &worker,
            "env-actor",
            "env-reader-call",
            serde_json::json!({"activation_id":"env-start","read_only":true,"environment":{}}),
        );
        assert_eq!(
            worker.activate(&mut reader).await.unwrap_err(),
            FailureCode::Conflict
        );
        assert!(
            reader
                .failure_detail
                .unwrap()
                .starts_with("read_only:ide.start:")
        );
        assert_eq!(
            worker.grants[&binding].role(),
            crate::workspace::authority::StartRole::Writer
        );
    }

    /// Explicit readers coexist with one writer, and same-id starts upgrade or downgrade roles.
    #[tokio::test]
    async fn explicit_readers_coexist_and_writer_slot_upgrades_and_downgrades() {
        use crate::workspace::authority::StartRole;
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();

        let (writer_binding, writer_receipt) =
            production_start(&mut worker, "writer-actor", "writer-start").await;
        assert_eq!(writer_receipt.role(), StartRole::Writer);

        let (reader_binding, reader_receipt) =
            production_start_mode(&mut worker, "reader-actor", "reader-start", true).await;
        assert_eq!(reader_receipt.role(), StartRole::Reader);
        assert_eq!(
            reader_receipt.epoch(),
            writer_receipt.epoch(),
            "a reader shares the writer's epoch and advances no authority clock"
        );

        let (_, second_reader) =
            production_start_mode(&mut worker, "reader-two", "reader-two-start", true).await;
        assert_eq!(second_reader.role(), StartRole::Reader);

        // An explicit reader stays a reader when repeated in read-only mode.
        let (mut repeat, _repeat_cancel) = start_job(
            &worker,
            "reader-actor",
            "reader-repeat",
            serde_json::json!({"activation_id":"reader-start","read_only":true}),
        );
        worker.activate(&mut repeat).await.unwrap();
        let repeated = worker.grants.get(&reader_binding).cloned().unwrap();
        assert_eq!(
            repeated.role(),
            StartRole::Reader,
            "an idempotent reader retry must not mint writer authority"
        );

        // A second writer is refused while the first holds the slot; the holder is named.
        let (mut refused_writer, _refused_cancel) = start_job(
            &worker,
            "third-actor",
            "third-writer-refused",
            serde_json::json!({"activation_id":"third-writer"}),
        );
        assert_eq!(
            worker.activate(&mut refused_writer).await,
            Err(FailureCode::Conflict)
        );
        assert!(
            refused_writer
                .failure_detail
                .as_deref()
                .is_some_and(|detail| detail.starts_with("start:worktree_held_by_another_actor:")),
            "second writer names the existing writer: {:?}",
            refused_writer.failure_detail
        );

        // The writer can downgrade in place, freeing its index slot for a third actor.
        let (mut downgrade, _downgrade_cancel) = start_job(
            &worker,
            "writer-actor",
            "writer-downgrade",
            serde_json::json!({"activation_id":"writer-start","read_only":true}),
        );
        worker.activate(&mut downgrade).await.unwrap();
        let downgraded = worker.grants.get(&writer_binding).cloned().unwrap();
        assert_eq!(downgraded.role(), StartRole::Reader);
        let (third_binding, third_writer) =
            production_start(&mut worker, "third-actor", "third-writer").await;
        assert_eq!(third_writer.role(), StartRole::Writer);

        // The same reader now asks for writer authority and upgrades when the slot is released.
        assert!(worker.settle_revocation(&third_binding).await.unwrap());
        let (mut upgrade, _upgrade_cancel) = start_job(
            &worker,
            "reader-actor",
            "reader-upgrade",
            serde_json::json!({"activation_id":"reader-start","read_only":false}),
        );
        worker.activate(&mut upgrade).await.unwrap();
        let upgraded = worker.grants.get(&reader_binding).cloned().unwrap();
        assert_eq!(upgraded.role(), StartRole::Writer);
        assert!(
            upgraded.epoch() > writer_receipt.epoch(),
            "an upgrade mints a fresh authority epoch"
        );
        assert_eq!(
            upgraded.operation(),
            reader_receipt.operation(),
            "the upgrade keeps the reader's activation operation"
        );
        assert!(worker.settle_revocation(&writer_binding).await.unwrap());
    }

    /// Writes the fixture worktree's `.epsilon` source that provider calls resolve and returns its
    /// worktree-relative path, the form every tool call observes sources by. Registers the test
    /// languages first: the registry is fixed by its first caller and sizes the worker's
    /// admission ceiling, so a test must register before it builds its worker.
    fn epsilon_source(fixture: &Fixture) -> std::path::PathBuf {
        crate::lang::testing::install();
        std::fs::write(fixture.root.join("a.epsilon"), "one\n").unwrap();
        std::path::PathBuf::from("a.epsilon")
    }

    /// The `ensure:<binding>:<namespace>` lines of the recording fixture provider for `bindings`,
    /// reduced to `ensure:<tag>` / `release:<tag>` plus the distinct namespaces they named.
    fn provider_events(bindings: &[&BindingRef]) -> (Vec<String>, Vec<String>) {
        let mut namespaces = Vec::new();
        let events = crate::lang::testing::fixture_events(bindings)
            .into_iter()
            .map(|line| {
                let mut parts = line.splitn(3, ':');
                let (event, tag) = (parts.next().unwrap(), parts.next().unwrap());
                if let Some(namespace) = parts.next()
                    && !namespaces.iter().any(|seen| seen == namespace)
                {
                    namespaces.push(namespace.to_owned());
                }
                format!("{event}:{tag}")
            })
            .collect();
        (events, namespaces)
    }

    /// QW-1: a read-only activation with no writer beside it owns the provider namespace, so its
    /// semantic calls reach the provider (0.10.5 refused every one with a stage-less
    /// `provider_unavailable` because only a writer retained the namespace). A second reader
    /// borrows the first reader's session, and the owner's stop hands the namespace to the
    /// survivor's next call.
    #[tokio::test]
    async fn writerless_reader_owns_the_provider_namespace_and_hands_it_to_a_surviving_reader() {
        use crate::lang::testing::fixture_tag;
        let fixture = Fixture::new();
        let file = epsilon_source(&fixture);
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let first = provider_start(&mut worker, "wl-first", "wl-first-start", true, None)
            .await
            .unwrap();
        let second = provider_start(&mut worker, "wl-second", "wl-second-start", true, None)
            .await
            .unwrap();
        assert!(
            !worker.test_binding_owns_caches(&first),
            "a reader retains nothing at start"
        );

        // The first reader's semantic call claims the namespace and reaches the provider.
        assert_eq!(
            provider_symbol_call(&mut worker, "wl-first", "wl-first-1", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&first));
        // The second reader borrows the first reader's session instead of claiming a second one.
        assert_eq!(
            provider_symbol_call(&mut worker, "wl-second", "wl-second-1", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(!worker.test_binding_owns_caches(&second));
        let (events, namespaces) = provider_events(&[&first, &second]);
        let (first_tag, second_tag) = (fixture_tag(&first), fixture_tag(&second));
        assert_eq!(
            events,
            [format!("ensure:{first_tag}"), format!("ensure:{first_tag}")],
            "both readers' calls were served by the first reader's session"
        );
        assert_eq!(namespaces.len(), 1, "{namespaces:?}");

        // The owner stops: its session is released, and the survivor claims the same namespace.
        assert!(worker.settle_revocation(&first).await.unwrap());
        assert!(!worker.test_binding_owns_caches(&first));
        assert_eq!(
            provider_symbol_call(&mut worker, "wl-second", "wl-second-2", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&second));
        let (events, namespaces) = provider_events(&[&first, &second]);
        assert_eq!(
            events[2..],
            [
                format!("release:{first_tag}"),
                format!("ensure:{second_tag}")
            ]
        );
        assert_eq!(
            namespaces.len(),
            1,
            "the worktree's namespace is handed over: {namespaces:?}"
        );
    }

    /// A writer that starts beside a reader-owned session takes the namespace over only after the
    /// reader's session is released, the reader keeps answering through the writer, and the
    /// reader claims the namespace back when the writer departs. A rejected second writer leaves
    /// the first writer's ownership untouched.
    #[tokio::test]
    async fn writer_takes_the_namespace_from_a_reader_owner_and_returns_it_on_departure() {
        use crate::lang::testing::fixture_tag;
        let fixture = Fixture::new();
        let file = epsilon_source(&fixture);
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let reader = provider_start(&mut worker, "wa-reader", "wa-reader-start", true, None)
            .await
            .unwrap();
        assert_eq!(
            provider_symbol_call(&mut worker, "wa-reader", "wa-reader-1", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&reader));

        // Writer arrival: the reader's session is released before the writer retains.
        let writer = provider_start(&mut worker, "wa-writer", "wa-writer-start", false, None)
            .await
            .unwrap();
        assert!(!worker.test_binding_owns_caches(&reader));
        assert!(worker.test_binding_owns_caches(&writer));
        assert_eq!(
            provider_symbol_call(&mut worker, "wa-reader", "wa-reader-2", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(
            !worker.test_binding_owns_caches(&reader),
            "a reader beside a writer borrows and never owns"
        );

        // A second writer is refused and the first writer keeps the namespace and the reader.
        let mut refused = start_job(
            &worker,
            "wa-third",
            "wa-third-start",
            serde_json::json!({"activation_id":"wa-third"}),
        );
        assert_eq!(
            worker.activate(&mut refused.0).await,
            Err(FailureCode::Conflict)
        );
        assert!(worker.test_binding_owns_caches(&writer));
        assert_eq!(
            provider_symbol_call(&mut worker, "wa-reader", "wa-reader-3", &file).await,
            Err(FailureCode::ProviderLoading)
        );

        // Writer departure: the reader's next call claims the namespace again.
        assert!(worker.settle_revocation(&writer).await.unwrap());
        assert!(!worker.test_binding_owns_caches(&writer));
        assert_eq!(
            provider_symbol_call(&mut worker, "wa-reader", "wa-reader-4", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&reader));

        let (events, namespaces) = provider_events(&[&reader, &writer]);
        let (reader_tag, writer_tag) = (fixture_tag(&reader), fixture_tag(&writer));
        assert_eq!(
            events,
            [
                format!("ensure:{reader_tag}"),
                format!("release:{reader_tag}"),
                format!("ensure:{writer_tag}"),
                format!("ensure:{writer_tag}"),
                format!("release:{writer_tag}"),
                format!("ensure:{reader_tag}"),
            ],
            "reader session, handover, writer sessions, departure, reader session again"
        );
        assert_eq!(
            namespaces.len(),
            1,
            "one namespace changes owners: {namespaces:?}"
        );
    }

    /// A reader upgrading to a writer releases its own reader-owned session first; a writer
    /// downgrading to a reader releases its session and lets its next call claim the namespace as
    /// a reader again.
    #[tokio::test]
    async fn reader_upgrade_and_writer_downgrade_keep_one_owner_of_the_namespace() {
        use crate::lang::testing::fixture_tag;
        let fixture = Fixture::new();
        let file = epsilon_source(&fixture);
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let actor = provider_start(&mut worker, "ud-actor", "ud-start-reader", true, None)
            .await
            .unwrap();
        assert_eq!(
            provider_symbol_call(&mut worker, "ud-actor", "ud-1", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&actor));

        // Upgrade in place: the reader-owned session is released, the writer retains afresh.
        provider_start(&mut worker, "ud-actor", "ud-start-writer", false, None)
            .await
            .unwrap();
        assert!(worker.test_binding_owns_caches(&actor));
        assert_eq!(
            provider_symbol_call(&mut worker, "ud-actor", "ud-2", &file).await,
            Err(FailureCode::ProviderLoading)
        );

        // Downgrade in place: nothing is owned until the next call claims it as a reader.
        provider_start(&mut worker, "ud-actor", "ud-start-reader-again", true, None)
            .await
            .unwrap();
        assert!(!worker.test_binding_owns_caches(&actor));
        assert_eq!(
            provider_symbol_call(&mut worker, "ud-actor", "ud-3", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        assert!(worker.test_binding_owns_caches(&actor));

        let (events, namespaces) = provider_events(&[&actor]);
        let tag = fixture_tag(&actor);
        assert_eq!(
            events,
            [
                format!("ensure:{tag}"),
                format!("release:{tag}"),
                format!("ensure:{tag}"),
                format!("release:{tag}"),
                format!("ensure:{tag}"),
            ]
        );
        assert_eq!(namespaces.len(), 1, "{namespaces:?}");
    }

    /// A reader owner whose cleanup fails during a writer's handover leaves the writer's start
    /// refused with no second owner: the writer holds nothing, the reader keeps the namespace and
    /// its next call is still answered, and a retried writer start then takes the namespace over.
    #[tokio::test]
    async fn failed_reader_cleanup_refuses_the_writer_without_a_second_owner_and_retry_succeeds() {
        use crate::lang::testing::{fixture_fail_next_close, fixture_tag};
        let fixture = Fixture::new();
        let file = epsilon_source(&fixture);
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let reader = provider_start(&mut worker, "fc-reader", "fc-reader-start", true, None)
            .await
            .unwrap();
        assert_eq!(
            provider_symbol_call(&mut worker, "fc-reader", "fc-reader-1", &file).await,
            Err(FailureCode::ProviderLoading)
        );
        fixture_fail_next_close(&reader);

        let refused = provider_start(&mut worker, "fc-writer", "fc-writer-start-1", false, None)
            .await
            .err();
        assert_eq!(refused, Some(FailureCode::Internal));
        assert!(
            worker.test_binding_owns_caches(&reader),
            "the reader keeps the namespace its cleanup could not release"
        );
        assert_eq!(
            provider_symbol_call(&mut worker, "fc-reader", "fc-reader-2", &file).await,
            Err(FailureCode::ProviderLoading),
            "the reader still answers; the refused writer holds nothing"
        );

        // Retry: the cleanup succeeds and the writer takes the namespace over.
        let writer = provider_start(&mut worker, "fc-writer", "fc-writer-start-2", false, None)
            .await
            .unwrap();
        assert!(!worker.test_binding_owns_caches(&reader));
        assert!(worker.test_binding_owns_caches(&writer));
        let (events, namespaces) = provider_events(&[&reader, &writer]);
        let (reader_tag, writer_tag) = (fixture_tag(&reader), fixture_tag(&writer));
        assert!(
            events.windows(2).all(|pair| !(pair[0].starts_with("ensure")
                && pair[1].starts_with("ensure")
                && pair[0] != pair[1])),
            "no writer session starts while the reader still owns: {events:?}"
        );
        assert!(
            !events.contains(&format!("ensure:{writer_tag}")),
            "{events:?}"
        );
        assert_eq!(events.first(), Some(&format!("ensure:{reader_tag}")));
        assert_eq!(namespaces.len(), 1, "{namespaces:?}");
    }

    /// Readers of sibling worktrees own independent namespaces: a writer arriving on one worktree
    /// releases only that worktree's reader owner.
    #[tokio::test]
    async fn sibling_worktrees_keep_independent_reader_owned_namespaces() {
        let fixture = Fixture::new();
        let sibling = fixture.base.join("sibling-root");
        std::fs::create_dir_all(&sibling).unwrap();
        let file = epsilon_source(&fixture);
        std::fs::write(sibling.join("b.epsilon"), "two\n").unwrap();
        let sibling_file = std::path::PathBuf::from("b.epsilon");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let here = provider_start(&mut worker, "sw-here", "sw-here-start", true, None)
            .await
            .unwrap();
        let there = provider_start(
            &mut worker,
            "sw-there",
            "sw-there-start",
            true,
            Some(&sibling),
        )
        .await
        .unwrap();
        for (actor, call, path) in [
            ("sw-here", "sw-here-1", &file),
            ("sw-there", "sw-there-1", &sibling_file),
        ] {
            assert_eq!(
                provider_symbol_call(&mut worker, actor, call, path).await,
                Err(FailureCode::ProviderLoading),
                "{actor}"
            );
        }
        assert!(worker.test_binding_owns_caches(&here));
        assert!(worker.test_binding_owns_caches(&there));
        let (_, here_namespaces) = provider_events(&[&here]);
        let (_, there_namespaces) = provider_events(&[&there]);
        assert_ne!(here_namespaces, there_namespaces);

        provider_start(&mut worker, "sw-writer", "sw-writer-start", false, None)
            .await
            .unwrap();
        assert!(
            !worker.test_binding_owns_caches(&here),
            "released by the writer"
        );
        assert!(
            worker.test_binding_owns_caches(&there),
            "the sibling worktree's reader owner is untouched"
        );
    }

    /// A second stop after authority release is a benign completion, not a workspace refusal.
    #[tokio::test]
    async fn stopping_after_lost_authority_is_nothing_active() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let (binding, _) = production_start(&mut worker, "stopped-actor", "stopped-start").await;
        assert!(worker.settle_revocation(&binding).await.unwrap());
        let (reply, _, _) = worker.revoke(&binding, &[]).await.unwrap();
        assert!(matches!(
            reply,
            PeerReply::Complete { text, .. }
                if text == "nothing active for this binding; its workspace authority was already released"
        ));
    }

    /// A start that names no `activation_id` keeps a stable binding-derived default, so the
    /// repeat returns the same activation instead of an invalid-parameters refusal (E013 item 4).
    #[tokio::test]
    async fn start_without_activation_id_is_stable_for_the_binding() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let (mut first, _first_cancel) = start_job(
            &worker,
            "default-id-actor",
            "call-one",
            serde_json::json!({}),
        );
        worker.activate(&mut first).await.unwrap();
        let first_binding = first.invocation.binding_ref().clone();
        let first = worker.grants.get(&first_binding).cloned().unwrap();
        let (mut second, _second_cancel) = start_job(
            &worker,
            "default-id-actor",
            "call-two",
            serde_json::json!({}),
        );
        worker.activate(&mut second).await.unwrap();
        let second = worker.grants.get(&first_binding).cloned().unwrap();
        assert_eq!(
            second.operation(),
            first.operation(),
            "a start without activation_id keeps one stable default for its binding"
        );
        assert_ne!(
            first.operation(),
            default_activation_id(&first_binding),
            "the stored operation stays the hashed activation id"
        );
        assert_eq!(
            default_activation_id(&first_binding),
            default_activation_id(&first_binding),
            "the default id is stable for one binding"
        );
    }

    /// A start whose actor already owns another worktree is refused naming that activation's
    /// holder facts — actor, activation, role, since when, last activity (E013 item 1).
    #[tokio::test]
    async fn actor_owned_refusal_names_the_holder_facts() {
        let fixture = Fixture::new();
        let other = fixture.base.join("other-root");
        std::fs::create_dir_all(&other).unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let (holder_binding, _holder) =
            production_start(&mut worker, "holding-actor", "holding-start").await;
        worker
            .activity
            .insert(holder_binding.fingerprint(), crate::errorlog::now_ms() - 1);

        let (mut refused, _cancel) = start_job(
            &worker,
            "holding-actor",
            "second-start",
            serde_json::json!({"activation_id":"second-start","root":other}),
        );
        assert!(matches!(
            worker.activate(&mut refused).await,
            Err(FailureCode::Conflict)
        ));
        let detail = refused.failure_detail.unwrap();
        assert!(
            detail
                .starts_with("start:actor_owns_another_worktree: actor holding-actor (activation "),
            "{detail}"
        );
        assert!(
            detail.contains("writer, since 20"),
            "the holder facts name the role and the start time: {detail}"
        );
        assert!(
            detail.contains("last activity 20"),
            "the holder facts name the last observed activity: {detail}"
        );
    }

    /// A replaced directory whose active start belongs to an already-dead session no longer
    /// refuses every later start: the stale grant is settled and the resolve retried once
    /// (E013 item 7).
    #[tokio::test]
    async fn replaced_directory_start_recovers_after_the_dead_holder_settles() {
        let fixture = Fixture::new();
        let plain = fixture.base.join("plain-root");
        std::fs::create_dir_all(&plain).unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();

        let (mut plain_job, _plain_cancel) = start_job(
            &worker,
            "plain-actor",
            "plain-start",
            serde_json::json!({"activation_id":"plain-start","root":plain}),
        );
        worker.activate(&mut plain_job).await.unwrap();
        let plain_binding = plain_job.invocation.binding_ref().clone();
        // Kill that session without a stop: its binding disappears while its grant stays active.
        {
            let mut guard = worker.shared.bindings.lock().unwrap();
            guard.stop_binding(&plain_binding).unwrap();
        }
        // Replace the directory: a fresh identity at the same path, held by the dead session.
        std::fs::remove_dir_all(&plain).unwrap();
        std::fs::create_dir_all(&plain).unwrap();

        // A second actor's start must succeed by settling the dead holder's stale grant instead
        // of refusing every start until a daemon restart.
        let (mut fresh, _cancel) = start_job(
            &worker,
            "fresh-actor",
            "fresh-start",
            serde_json::json!({"activation_id":"fresh-start","root":plain}),
        );
        worker
            .activate(&mut fresh)
            .await
            .expect("a dead holder's stale grant must not refuse a fresh start forever");
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
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
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
                git_notices: Mutex::new(BTreeMap::new()),
                environments: Mutex::default(),
                activated: Mutex::new(BTreeSet::new()),
            }),
            workspace,
            observations: WorkspaceStore::new(store),
            edits: EditReceiptStore::new(store),
            grants: BTreeMap::new(),
            leases: BTreeMap::new(),
            pending_revocations: std::collections::BTreeSet::new(),
            stop_cause: None,
            stop_attempts: Arc::default(),
            revoke_retry_rounds: 0,
            next_revoke_retry: None,
            registered: BTreeMap::new(),
            baselines: BTreeMap::new(),
            heads: BTreeMap::new(),
            source_sequence: 0,
            admission: Arc::new(Mutex::new(admission_controller())),
            uncertain: std::collections::BTreeSet::new(),
            uncertain_snapshots: Vec::new(),
            runtime,
            providers: providers::Providers::new(),
            names: Default::default(),
            telemetry: None,
            activity: BTreeMap::new(),
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
            parameters: serde_json::json!({"path":"main.rs","byte_offset":2}),
            target: production_target(&fixture.root),
            deadline: tokio::time::Instant::now() + Duration::from_secs(5),
            cancel,
            stop_reply: None,
            native_epoch: 0,
            failure_detail: None,
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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

    /// A symbol edit resolves the symbol on one observation and must both splice and base the
    /// write on that same observation: the observation counter pins exactly one pre-write read
    /// (the resolution that also became the base) plus the post-write refresh, so a reintroduced
    /// second "observe again" before the splice — which would splice the resolved line numbers
    /// into later bytes — fails here. The write path itself refuses a base whose bytes no longer
    /// match the file, so one observation leaves no cross-version window at all.
    #[tokio::test]
    async fn symbol_edit_splices_on_the_observation_it_resolved() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join("main.gamma"),
            "sym first\n1\nend\n\nsym second\n2\nend\n",
        )
        .unwrap();
        git_commit(&fixture.root, "symbol edit fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();
        activate_worktree(&mut worker, "symbol-edit-actor", "symbol-edit-start").await;
        let invocation = production_call(&worker, "symbol-edit-actor", "symbol-edit-call");
        let (mut job, _cancel) = tool_job(
            &fixture.root,
            invocation,
            "symbol-edit",
            AssistanceTool::Edit,
            serde_json::json!({
                "operation_id":"symbol-edit-1",
                "symbol":"main.gamma#second",
                "content":"sym second\n3\nend"
            }),
        );
        let before = worker.source_sequence;
        let (reply, _, _) = loop {
            match worker.edit(&mut job).await {
                Ok(complete) => break complete,
                Err(FailureCode::ProviderLoading) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(code) => panic!("symbol edit failed: {code:?}"),
            }
        };
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    ..
                },
                ..
            }
        ));
        // The splice landed exactly on the resolved symbol's lines of the resolved text.
        assert_eq!(
            std::fs::read(fixture.root.join("main.gamma")).unwrap(),
            b"sym first\n1\nend\n\nsym second\n3\nend\n"
        );
        assert_eq!(
            worker.source_sequence,
            before + 2,
            "the symbol form must observe once to resolve and splice, once to refresh"
        );
    }

    /// Runs one Read job to completion, retrying while it parks, and retains its completed
    /// detail with the observation `ide.edit` later names as `source_ref`.
    async fn read_and_retain(
        worker: &mut Worker<'_>,
        root: &std::path::Path,
        actor: &str,
        binding: &BindingRef,
        reference: &str,
        parameters: Value,
    ) {
        let invocation = production_call(worker, actor, &format!("{reference}-call"));
        let (mut job, _cancel) = tool_job(
            root,
            invocation,
            reference,
            AssistanceTool::Read,
            parameters.clone(),
        );
        let (reply, authority, source) = loop {
            match worker.read(&mut job).await {
                Ok(complete) => break complete,
                Err(FailureCode::ProviderLoading) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(code) => panic!("read {reference} failed: {code:?}"),
            }
        };
        worker.shared.ledger.lock().unwrap().details.insert(
            reference.to_owned(),
            Detail {
                binding: binding.clone(),
                reply,
                selection: (AssistanceTool::Read, selection(&parameters)),
                authority,
                source,
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

    /// Runs one `ide.edit` job to completion and returns its reply.
    async fn run_edit(
        worker: &mut Worker<'_>,
        root: &std::path::Path,
        reference: &str,
        parameters: Value,
    ) -> PeerReply {
        let invocation = production_call(worker, "line-actor", reference);
        let binding = invocation.binding_ref().clone();
        let (mut job, _cancel) = tool_job(
            root,
            invocation,
            reference,
            AssistanceTool::Edit,
            parameters,
        );
        loop {
            match worker.edit(&mut job).await {
                Ok((reply, authority, source)) => {
                    worker.shared.ledger.lock().unwrap().details.insert(
                        reference.to_owned(),
                        Detail {
                            binding,
                            selection: (AssistanceTool::Edit, selection(&job.parameters)),
                            line_movement: reply_line_movement(&reply),
                            reply: reply.clone(),
                            authority,
                            source,
                            native_epoch: 0,
                            diff_page: None,
                            diff_page_fresh: false,
                            context_page: None,
                            context_page_fresh: false,
                            diff_provenance: None,
                            extra_sources: Vec::new(),
                        },
                    );
                    return reply;
                }
                Err(FailureCode::ProviderLoading) => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(code) => panic!("edit {reference} failed: {code:?}"),
            }
        }
    }

    /// A line-range edit applies only on a retained same-path observation whose bytes are still
    /// current, reports its net line movement, and never writes on a refused base; the
    /// symbol form validates an explicit `source_ref` the same way and stays optional.
    #[tokio::test]
    async fn line_and_symbol_edits_gate_on_a_fresh_retained_source() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("fmt.toml"), "gamma formatter marker\n").unwrap();
        let source = "sym card\nsym btn\nmark\nend\nend\n";
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        git_commit(&fixture.root, "line-edit fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();
        let (binding, _authority) =
            activate_worktree(&mut worker, "line-actor", "line-start").await;

        // A completed read of the exact lines is an admitted base, and the formatter's added
        // line is stated so a later line edit does not reuse the pre-format numbers.
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "line-read",
            serde_json::json!({"path":"a.gamma","lines":"2-3"}),
        )
        .await;
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "line-edit",
            serde_json::json!({
                "operation_id":"line-1",
                "path":"a.gamma",
                "lines":"2-3",
                "source_ref":"line-read",
                "content":"sym btn\nmark,x\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::Replaced,
                        source_ref: Some(reference),
                        ..
                    },
                    note: Some(note),
                    ..
                } if reference == "line-edit"
                    && note == "lines after 2 moved +1"
            ),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            "sym card\nsym btn\nmark\nx\nend\nend\n"
        );

        let shifted = std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap();
        let reply = run_edit(
            &mut worker, &fixture.root, "shifted-single",
            serde_json::json!({"operation_id":"shifted-single-op","path":"a.gamma","lines":"5-5","source_ref":"line-edit","content":"end"}),
        ).await;
        assert!(
            matches!(&reply, PeerReply::Edit { result: EditResult { outcome: ChangesEditOutcome::StaleSource, .. }, note: Some(note), .. } if note.contains("edit:lines_moved") && note.contains("after line 2 by +1") && note.contains("ide.read {path, lines}")),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            shifted
        );
        let reply = run_edit(
            &mut worker, &fixture.root, "shifted-batch",
            serde_json::json!({"operation_id":"shifted-batch-op","path":"a.gamma","source_ref":"line-edit","changes":[{"lines":"2-2","content":"changed"}]}),
        ).await;
        assert!(
            matches!(&reply, PeerReply::Edit { result: EditResult { outcome: ChangesEditOutcome::StaleSource, .. }, note: Some(note), .. } if note.contains("edit:lines_moved")),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            shifted
        );
        let reply = run_edit(
            &mut worker, &fixture.root, "old-read-after-edit",
            serde_json::json!({"operation_id":"old-read-op","path":"a.gamma","lines":"2-3","source_ref":"line-read","content":"wrong"}),
        ).await;
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::StaleSource,
                    ..
                },
                ..
            }
        ));
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            shifted
        );

        // Batch replies carry the same net shift as single line edits.
        std::fs::remove_file(fixture.root.join("fmt.toml")).unwrap();
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "batch-shift-read",
            serde_json::json!({"path":"a.gamma","lines":"2-3"}),
        )
        .await;
        let reply = run_edit(&mut worker, &fixture.root, "batch-shift-edit", serde_json::json!({"operation_id":"batch-shift-op","path":"a.gamma","source_ref":"batch-shift-read","changes":[{"lines":"2-3","content":"sym btn\nmark\nx"}]})).await;
        assert!(
            matches!(&reply, PeerReply::Edit { note: Some(note), .. } if note.contains("lines after 2 moved +1")),
            "{reply:?}"
        );

        // Equal-line-count edits remain valid line-range bases.
        std::fs::remove_file(fixture.root.join("fmt.toml")).ok();
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "same-count-read",
            serde_json::json!({"path":"a.gamma","lines":"2-3"}),
        )
        .await;
        let reply = run_edit(&mut worker, &fixture.root, "same-count-edit", serde_json::json!({"operation_id":"same-count-op","path":"a.gamma","lines":"2-3","source_ref":"same-count-read","content":"sym btn\nmark,y"})).await;
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    ..
                },
                ..
            }
        ));
        let reply = run_edit(&mut worker, &fixture.root, "same-count-next", serde_json::json!({"operation_id":"same-count-next-op","path":"a.gamma","lines":"3-3","source_ref":"same-count-edit","content":"mark,z"})).await;
        assert!(matches!(
            reply,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    ..
                },
                ..
            }
        ));

        // The file changed after the read: the edit is refused with no write at all.
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "stale-read",
            serde_json::json!({"path":"a.gamma","lines":"2-3"}),
        )
        .await;
        std::fs::write(
            fixture.root.join("a.gamma"),
            "sym card\nsym other\nmark\nend\nend\n",
        )
        .unwrap();
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "stale-edit",
            serde_json::json!({
                "operation_id":"line-stale",
                "path":"a.gamma",
                "lines":"2-3",
                "source_ref":"stale-read",
                "content":"sym btn\nmark\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::StaleSource,
                        source_ref: None,
                        ..
                    },
                    note: None,
                    ..
                }
            ),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            "sym card\nsym other\nmark\nend\nend\n"
        );

        // A reference that was never issued is refused the same way, before any write.
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "garbled-edit",
            serde_json::json!({
                "operation_id":"line-garbled",
                "path":"a.gamma",
                "lines":"2-3",
                "source_ref":"no-such-observation",
                "content":"sym btn\nmark\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::StaleSource,
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.gamma")).unwrap(),
            "sym card\nsym other\nmark\nend\nend\n"
        );

        // The symbol form resolves its own range, so its source_ref is optional — but when one
        // is given it is held to the same retained-and-current rule.
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "symbol-no-ref",
            serde_json::json!({
                "operation_id":"symbol-1",
                "op":"replace",
                "symbol":"a.gamma#card/btn",
                "content":"sym btn\nend\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::Replaced,
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
        let symbol_source = std::fs::read(fixture.root.join("a.gamma")).unwrap();
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "symbol-read",
            serde_json::json!({"symbol":"a.gamma#card/btn"}),
        )
        .await;
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "symbol-bad-ref",
            serde_json::json!({
                "operation_id":"symbol-2",
                "op":"replace",
                "symbol":"a.gamma#card/btn",
                "source_ref":"also-not-issued",
                "content":"sym btn\nend\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::StaleSource,
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
        assert_eq!(
            std::fs::read(fixture.root.join("a.gamma")).unwrap(),
            symbol_source
        );
    }

    /// F-25: the registered-path budget is its own and never refuses: filling it evicts the least
    /// recently used path to admit a new one, a path already registered is admitted without
    /// evicting anything, and nothing the retained-results limit does touches it.
    #[tokio::test]
    async fn registered_paths_have_their_own_budget_and_evict_the_least_recently_used() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (binding, _) = production_start(&mut worker, "actor-1", "call-1").await;
        let base = tokio::time::Instant::now();
        let paths = worker.registered.entry(binding.clone()).or_default();
        for index in 0..MAX_REGISTERED_PATHS {
            paths.insert_used_at(
                std::path::PathBuf::from(format!("used-{index}.txt")),
                base + Duration::from_millis(index as u64),
            );
        }
        assert_eq!(paths.len(), MAX_REGISTERED_PATHS);

        // A path that is already registered is admitted without evicting anything.
        worker.admit_registered_path(&binding, std::path::Path::new("used-7.txt"));
        assert_eq!(worker.registered[&binding].len(), MAX_REGISTERED_PATHS);
        assert!(worker.registered[&binding].contains(std::path::Path::new("used-0.txt")));

        // A new path is admitted at the full budget: the oldest path makes room, no refusal.
        worker.admit_registered_path(&binding, std::path::Path::new("new.txt"));
        let kept = worker.registered[&binding].paths();
        assert!(
            !kept.contains(&std::path::PathBuf::from("used-0.txt")),
            "{kept:?}"
        );
        assert!(kept.contains(&std::path::PathBuf::from("used-1.txt")));
        assert_eq!(
            kept.len(),
            MAX_REGISTERED_PATHS - 1,
            "the freed slot awaits the new path"
        );
    }

    /// F-25: a native-hint refresh re-reads every registered path but keeps each path's real age,
    /// so a path nobody asks for is still the first to make room however often hooks fire.
    #[tokio::test]
    async fn maintenance_refresh_does_not_keep_unused_registered_paths_alive() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = production_start(&mut worker, "actor-1", "call-1").await;
        std::fs::write(fixture.root.join("stale.txt"), "stale\n").unwrap();
        std::fs::write(fixture.root.join("asked.txt"), "asked\n").unwrap();
        let base = tokio::time::Instant::now();
        let paths = worker.registered.entry(binding.clone()).or_default();
        paths.insert_used_at(std::path::PathBuf::from("stale.txt"), base);
        paths.insert_used_at(std::path::PathBuf::from("asked.txt"), base);
        for index in 0..MAX_REGISTERED_PATHS - 2 {
            paths.insert_used_at(
                std::path::PathBuf::from(format!("used-{index}.txt")),
                base + Duration::from_secs(1),
            );
        }
        assert_eq!(paths.len(), MAX_REGISTERED_PATHS);

        // An agent's own read of another path refreshes that path's age...
        worker
            .observe(&binding, std::path::PathBuf::from("asked.txt"))
            .await
            .expect("the requested read succeeds");
        // ...and only then the hook-driven refresh re-reads the stale path; if it wrongly counted
        // as a use, the stale path would now be newer than the asked one.
        worker
            .observe_as(&binding, std::path::PathBuf::from("stale.txt"), false)
            .await
            .expect("the maintenance read succeeds");

        // At the full budget the stale path is the oldest and makes room; the asked one stays.
        worker.admit_registered_path(&binding, std::path::Path::new("new.txt"));
        let kept = worker.registered[&binding].paths();
        assert!(
            !kept.contains(&std::path::PathBuf::from("stale.txt")),
            "{kept:?}"
        );
        assert!(kept.contains(&std::path::PathBuf::from("asked.txt")));
        assert_eq!(
            kept.len(),
            MAX_REGISTERED_PATHS - 1,
            "the freed slot awaits the new path"
        );
    }

    /// F-25: with the budget full, a read of a new path is admitted (no refusal). Eviction only
    /// stops the oldest path being refreshed on native hints: an edit built on its still-matching
    /// retained read keeps working, and one whose bytes no longer match gets the ordinary
    /// stale-source answer, asking for a re-read — never a hard refusal of work.
    #[tokio::test]
    async fn eviction_never_refuses_work_and_an_edit_on_the_evicted_path_asks_for_a_reread() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("fmt.toml"), "gamma formatter marker\n").unwrap();
        let source = "sym card\nsym btn\nmark\nend\nend\n";
        std::fs::write(fixture.root.join("a.gamma"), source).unwrap();
        std::fs::write(fixture.root.join("b.gamma"), source).unwrap();
        git_commit(&fixture.root, "eviction fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        worker.edits.install_schema().await.unwrap();
        let (binding, _authority) =
            activate_worktree(&mut worker, "line-actor", "line-start").await;

        // The agent reads a.gamma first: it is the oldest registered path.
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "line-read",
            serde_json::json!({"path":"a.gamma","lines":"2-3"}),
        )
        .await;
        let base = tokio::time::Instant::now() + Duration::from_secs(60);
        let paths = worker.registered.get_mut(&binding).unwrap();
        for index in 0..MAX_REGISTERED_PATHS - 1 {
            paths.insert_used_at(
                std::path::PathBuf::from(format!("filler-{index}.txt")),
                base,
            );
        }
        assert_eq!(paths.len(), MAX_REGISTERED_PATHS);

        // The 257th path is admitted: the read succeeds and a.gamma, the oldest, is evicted.
        read_and_retain(
            &mut worker,
            &fixture.root,
            "line-actor",
            &binding,
            "other-read",
            serde_json::json!({"path":"b.gamma","lines":"1-2"}),
        )
        .await;
        let kept = worker.registered[&binding].paths();
        assert!(kept.contains(&std::path::PathBuf::from("b.gamma")));
        assert!(
            !kept.contains(&std::path::PathBuf::from("a.gamma")),
            "{kept:?}"
        );
        assert_eq!(kept.len(), MAX_REGISTERED_PATHS);

        // a.gamma is no longer refreshed, but its retained read still matches the file, so an
        // edit built on it applies: eviction never refuses work.
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "after-eviction",
            serde_json::json!({
                "operation_id":"after-eviction-op",
                "path":"a.gamma",
                "lines":"2-3",
                "source_ref":"line-read",
                "content":"sym btn\nmark,x\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::Replaced,
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
        // The file changed under that read: a second edit on the same old reference is answered
        // `stale_source`, which asks the agent to read again.
        let reply = run_edit(
            &mut worker,
            &fixture.root,
            "after-eviction-again",
            serde_json::json!({
                "operation_id":"after-eviction-again-op",
                "path":"a.gamma",
                "lines":"2-3",
                "source_ref":"line-read",
                "content":"sym btn\nmark,y\n"
            }),
        )
        .await;
        assert!(
            matches!(
                &reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::StaleSource,
                        ..
                    },
                    ..
                }
            ),
            "{reply:?}"
        );
    }

    /// F-12: the daemon retries a transiently busy stop itself, by its operation id, so the agent
    /// never has to: a store lock that clears inside the retry window lets the stop commit.
    #[tokio::test]
    async fn daemon_retries_a_busy_stop_itself() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (binding, _) = production_start(&mut worker, "actor-1", "call-1").await;
        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&binding)
            .unwrap();
        // Hold the exact write lock the durable revoke needs: the first attempt fails "database
        // is locked" once SQLite's busy wait ends. The lock is released only after the daemon's
        // own second attempt has started, so only that retry can commit the stop.
        let lock = rusqlite::Connection::open(fixture.base.join("state.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let attempts = Arc::clone(&worker.stop_attempts);
        let release = async {
            let _ = tokio::time::timeout(Duration::from_secs(10), async {
                while attempts.load(Ordering::Relaxed) < 2 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            lock.execute_batch("ROLLBACK;").unwrap();
        };
        let (outcome, ()) = tokio::join!(worker.revoke(&binding, &[]), release);
        assert!(outcome.is_ok(), "{outcome:?}");
        assert!(worker.grants.is_empty());
        assert!(worker.pending_revocations.is_empty());
        assert_eq!(worker.stop_cause, None);
    }

    /// F-12: a stop whose store stays locked past the inline attempts stays pending, is scheduled
    /// for the daemon's own background retry, and is committed by that retry once the store
    /// answers — with no further tool call from the agent.
    #[tokio::test]
    async fn pending_stop_is_retried_by_the_daemon_until_the_store_answers() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (binding, _) = production_start(&mut worker, "actor-1", "call-1").await;
        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&binding)
            .unwrap();

        let lock = rusqlite::Connection::open(fixture.base.join("state.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let failed = worker.revoke(&binding, &[]).await;
        assert!(matches!(failed, Err(FailureCode::Capacity)), "{failed:?}");
        assert!(worker.pending_revocations.contains(&binding));
        assert!(
            worker.schedule_revoke_retry().is_some(),
            "a pending revoke is scheduled for a background retry"
        );

        // A background round while the store is still locked changes nothing and backs off.
        worker.retry_pending_revocations().await;
        assert!(worker.pending_revocations.contains(&binding));
        assert_eq!(worker.revoke_retry_rounds, 1);

        // The store answers again: the next background round commits the same revoke.
        lock.execute_batch("ROLLBACK;").unwrap();
        drop(lock);
        worker.retry_pending_revocations().await;
        assert!(worker.pending_revocations.is_empty());
        assert!(worker.grants.is_empty());
        assert_eq!(worker.revoke_retry_rounds, 0);
        assert!(worker.schedule_revoke_retry().is_none());
    }

    /// F-12: a stop that failed while the bounded pending set was full still has a retry owner:
    /// the stopped binding's own grant. With the pending set emptied by hand (the overflow), the
    /// daemon still schedules and performs the retry once the store answers.
    #[tokio::test]
    async fn stopped_grant_is_retried_even_when_the_pending_set_overflowed() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let (binding, _) = production_start(&mut worker, "actor-1", "call-1").await;
        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&binding)
            .unwrap();
        let lock = rusqlite::Connection::open(fixture.base.join("state.sqlite")).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert!(worker.revoke(&binding, &[]).await.is_err());
        // The overflow: the pending set never recorded this binding.
        worker.pending_revocations.clear();
        assert!(worker.grants.contains_key(&binding));
        assert!(
            worker.schedule_revoke_retry().is_some(),
            "the stopped grant alone schedules the retry"
        );
        lock.execute_batch("ROLLBACK;").unwrap();
        drop(lock);
        worker.retry_pending_revocations().await;
        assert!(worker.grants.is_empty(), "the retry released the grant");
        assert!(worker.schedule_revoke_retry().is_none());
    }

    /// F-12: the background retry pause doubles from one second to a ceiling of sixteen.
    #[test]
    fn background_revoke_retry_backs_off_to_a_ceiling() {
        let seconds = |rounds| revoke_retry_delay(rounds).as_secs();
        assert_eq!(
            [seconds(0), seconds(1), seconds(2), seconds(3), seconds(4)],
            [1, 2, 4, 8, 16]
        );
        assert_eq!(
            [seconds(5), seconds(31), seconds(32), seconds(u32::MAX)],
            [16; 4]
        );
    }

    /// F-12: every durable stop failure names its closed typed cause.
    #[test]
    fn stop_failures_name_typed_causes() {
        use crate::app::store::{OperationId, StoreError};
        use crate::workspace::authority::AuthorityError;
        use crate::workspace::durable::DurableError;
        let unknown = StoreError::OutcomeUnknown {
            operation: OperationId::new("stop-x").unwrap(),
        };
        for (error, expected, transient) in [
            (
                DurableError::Application(StoreError::Busy),
                (FailureCode::Capacity, "stop:busy"),
                true,
            ),
            (
                DurableError::Application(StoreError::QueueFull),
                (FailureCode::Capacity, "stop:busy"),
                true,
            ),
            (
                DurableError::Application(StoreError::ReceiptCapacityExhausted),
                (FailureCode::Capacity, "stop:store_full"),
                false,
            ),
            (
                DurableError::Application(unknown),
                (FailureCode::Deadline, "stop:store_deadline"),
                true,
            ),
            (
                DurableError::Application(StoreError::Infrastructure(
                    "database is locked".to_owned(),
                )),
                (FailureCode::Capacity, "stop:busy"),
                true,
            ),
            (
                DurableError::Application(StoreError::Infrastructure("disk I/O error".to_owned())),
                (FailureCode::WorkspaceAuthority, "stop:store_unavailable"),
                false,
            ),
            (
                DurableError::Application(StoreError::Unavailable),
                (FailureCode::WorkspaceAuthority, "stop:store_unavailable"),
                false,
            ),
            (
                DurableError::CorruptState,
                (FailureCode::WorkspaceAuthority, "stop:store_unavailable"),
                false,
            ),
            (
                DurableError::OperationConflict,
                (FailureCode::WorkspaceAuthority, "stop:authority"),
                false,
            ),
            (
                DurableError::Authority(AuthorityError::StaleAuthority),
                (FailureCode::WorkspaceAuthority, "stop:authority"),
                false,
            ),
        ] {
            assert_eq!(stop_failure(&error), expected, "{error:?}");
            assert_eq!(is_transient_stop_failure(&error), transient, "{error:?}");
        }
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
            "server-a",
            "server-a-settings-v1",
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

        let outcome = worker.revoke(&old_binding, &[]).await;
        assert!(
            matches!(outcome, Err(FailureCode::Capacity)),
            "a busy store is a capacity cause, not an authority one"
        );
        assert_eq!(worker.stop_cause, Some("stop:busy"));
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

        let settled = worker.revoke(&new_binding, &[]).await;
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

    /// Returns an old snapshot while reporting a running recheck once, then the refreshed result.
    struct RefreshingProblems {
        /// Snapshot visible before the changed-input check finishes.
        before: ProblemSnapshot,
        /// Snapshot visible after the changed-input check finishes.
        after: ProblemSnapshot,
        /// Number of `latest` queries made by the context request.
        latest_calls: AtomicUsize,
        /// Number of recheck-state queries made by the context request.
        recheck_calls: AtomicUsize,
    }

    impl ProblemSource for RefreshingProblems {
        /// Returns `before` on the first read and `after` once the worker refreshes its snapshot.
        fn latest(&self, _worktree: &std::path::Path) -> Vec<ProblemSnapshot> {
            if self.latest_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![self.before.clone()]
            } else {
                vec![self.after.clone()]
            }
        }

        /// Reports one changed-file recheck, then no recheck after the worker's bounded wait.
        fn rechecks(
            &self,
            _worktree: &std::path::Path,
        ) -> Vec<(crate::checks::Language, crate::checks::Recheck)> {
            if self.recheck_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![(
                    crate::lang::testing::ALPHA,
                    crate::checks::Recheck::FilesChanged,
                )]
            } else {
                Vec::new()
            }
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
            format_note: None,
            check_scheduled: false,
            park_until: None,
            stage: None,
            session_binding: None,
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
        crate::lang::testing::install();
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
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
            serde_json::json!({"kind":"problems","language":"alpha","offset":0}),
        )
        .await;
        let PeerReply::Complete { text, .. } = &reply else {
            panic!("problems context must complete: {reply:?}")
        };
        assert!(
            text.contains("alpha: ready; errors: 1; warnings: 0"),
            "{text}"
        );
        assert!(
            text.contains("src/main.rs:10:5 error [E0308] mismatched types!"),
            "{text}"
        );
        assert_eq!(fake.queried(), vec![fixture.root.clone()]);
    }

    /// A changed-check reply returns its current running state without sleeping in the worker.
    #[tokio::test]
    async fn context_problems_returns_running_changed_check_immediately() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        production_start(&mut worker, "refresh-actor", "refresh-start").await;
        crate::lang::testing::install();
        let fake = Arc::new(RefreshingProblems {
            before: ProblemSnapshot::from_problems(
                crate::lang::testing::ALPHA,
                CheckState::Ready,
                Vec::new(),
                1,
                0,
            ),
            after: ProblemSnapshot::from_problems(
                crate::lang::testing::ALPHA,
                CheckState::Ready,
                Vec::new(),
                0,
                0,
            ),
            latest_calls: AtomicUsize::new(0),
            recheck_calls: AtomicUsize::new(0),
        });
        Arc::get_mut(&mut worker.shared)
            .expect("fixture worker owns its shared state exclusively")
            .problem_source = Some(fake.clone());

        let reply = run_problems_context(
            &mut worker,
            "refresh-actor",
            "refresh-page",
            serde_json::json!({"kind":"problems","language":"alpha"}),
        )
        .await;
        let PeerReply::Complete { text, .. } = reply else {
            panic!("problems context must complete: {reply:?}")
        };
        assert!(text.contains("alpha: checking (files changed)"), "{text}");
        assert_eq!(fake.latest_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.recheck_calls.load(Ordering::SeqCst), 1);
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
                format_note: None,
                check_scheduled: false,
                park_until: None,
                stage: None,
                session_binding: None,
            },
            cancel_sender,
        )
    }

    /// An address missing from a server outline is unknown; one missing from a lexical outline
    /// waits while the server loads (a read-only tool parked for a retry, an edit answering
    /// `provider_loading` at once and never parked), and refuses `provider_unavailable` at once
    /// when the server is unavailable: nothing is parked for a server that will not answer.
    #[tokio::test]
    async fn a_lexical_miss_waits_for_the_server_instead_of_unknown_symbol() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let worker = worker(&store, workspace, fixture.root.clone());
        let call = production_call(&worker, "lexical-actor", "lexical-miss");
        let (mut job, _) = context_job(&fixture.root, call);
        job.tool = AssistanceTool::Read;

        let code = super::symbols::missing_symbol(&mut job, None);
        assert_eq!(code, FailureCode::UnknownSymbol);
        assert!(job.park_until.is_none());

        let code = super::symbols::missing_symbol(&mut job, Some(super::symbols::Lexical::Loading));
        assert_eq!(code, FailureCode::ProviderLoading);
        assert!(job.park_until.is_some(), "a read waits for the server");

        job.park_until = None;
        job.tool = AssistanceTool::Edit;
        let code = super::symbols::missing_symbol(&mut job, Some(super::symbols::Lexical::Loading));
        assert_eq!(code, FailureCode::ProviderLoading);
        assert!(job.park_until.is_none(), "an edit is never parked");

        job.tool = AssistanceTool::Read;
        let code =
            super::symbols::missing_symbol(&mut job, Some(super::symbols::Lexical::Unavailable));
        assert_eq!(code, FailureCode::ProviderUnavailable);
        assert!(
            job.park_until.is_none(),
            "an unavailable server is never waited for"
        );
    }

    /// A parked job yields the worker slot to the next runnable arrival.
    #[tokio::test]
    async fn parked_job_does_not_block_later_runnable_job() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let worker = worker(&store, workspace, fixture.root.clone());
        let first = production_call(&worker, "park-actor", "park-first");
        let second = production_call(&worker, "park-actor", "park-second");
        let (mut parked, _) = context_job(&fixture.root, first);
        let (ready, _) = context_job(&fixture.root, second);
        parked.park_until = Some(tokio::time::Instant::now() + Duration::from_secs(5));
        let parked_ref = parked.reference.clone();
        let ready_ref = ready.reference.clone();
        let mut queue = VecDeque::from([parked, ready]);

        let (selected, next_wake) = pop_ready_job(&mut queue, tokio::time::Instant::now());

        assert_eq!(selected.unwrap().reference, ready_ref);
        assert!(next_wake.is_some());
        assert_eq!(queue.front().unwrap().reference, parked_ref);
        let binding = queue.front().unwrap().invocation.binding_ref().clone();
        queue.retain(|job| job.invocation.binding_ref() != &binding);
        assert!(queue.is_empty(), "stop must drop parked binding work too");
    }

    /// Builds a `WorkerHandle` with the fixture's real target and a chosen details ceiling.
    ///
    /// No worker loop is spawned: a never-finishing dummy task satisfies `enqueue`'s liveness
    /// gate, so enqueued jobs stay queued and their details stay `Pending`, keeping the eviction
    /// assertions free of completion races.
    fn detail_handle(root: &std::path::Path, details: usize) -> WorkerHandle {
        let handle = WorkerHandle::new(
            Arc::new(Mutex::new(HostBindingGuard::default())),
            production_launcher(root, details),
            [9; 32],
            Arc::new(Mutex::new(admission_controller())),
        );
        *handle.task.lock().unwrap() = Some(tokio::spawn(std::future::pending()));
        handle
    }

    /// Plants one detail owned by `binding`, shaped exactly as a settled or pending job leaves
    /// it behind in the ledger.
    fn plant_detail(
        handle: &WorkerHandle,
        reference: &str,
        binding: &BindingRef,
        reply: PeerReply,
    ) {
        handle.shared.ledger.lock().unwrap().details.insert(
            reference.to_owned(),
            Detail {
                binding: binding.clone(),
                reply,
                selection: (
                    AssistanceTool::Context,
                    selection(&serde_json::json!({"path":"main.rs"})),
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

    /// Concurrent start retries coalesce only when their environment selections are identical.
    #[tokio::test]
    async fn environment_start_coalescing_preserves_new_choices() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 8);
        let mut refs = Vec::new();
        for (index, choice) in ["one", "two", "two"].into_iter().enumerate() {
            let invocation = validated_call(
                &handle.shared.bindings,
                "env-coalesce",
                &format!("call-{index}"),
            );
            refs.push(
                handle
                    .enqueue(
                        invocation,
                        AssistanceTool::Start,
                        serde_json::json!({"activation_id":"same","environment":{"alpha":choice}}),
                        "stop-retry",
                        None,
                    )
                    .unwrap(),
            );
        }
        assert_ne!(refs[0], refs[1]);
        assert_eq!(refs[1], refs[2]);
        assert_eq!(handle.shared.ledger.lock().unwrap().queue.len(), 2);
    }

    /// Builds one settled `Complete{Context}` reply so a planted detail looks job-settled.
    fn settled_detail(reference: &str) -> PeerReply {
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: "fact".into(),
            detail_ref: Some(reference.to_owned()),
            truncated: false,
            continuation: false,
        }
    }

    /// Enqueues one Context request through the production admission path, exactly as the
    /// facade would for the fixture's trusted attachment.
    fn enqueue_context(
        handle: &WorkerHandle,
        invocation: ValidatedInvocation,
    ) -> Result<String, FailureCode> {
        handle
            .enqueue(
                invocation,
                AssistanceTool::Context,
                serde_json::json!({"path":"main.rs"}),
                "stop-retry",
                None,
            )
            .map_err(|failure| failure.code)
    }

    /// A full ledger evicts a dead binding's settled facts for a new binding instead of
    /// refusing `capacity`.
    #[tokio::test]
    async fn full_ledger_evicts_inactive_binding_settled_details_for_a_new_binding() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 4);
        let old = validated_call(&handle.shared.bindings, "old-actor", "old-start")
            .binding_ref()
            .clone();
        for n in 1..=4 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &old, settled_detail(&reference));
        }
        let invocation = validated_call(&handle.shared.bindings, "new-actor", "new-start");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        for n in 1..=4 {
            assert!(
                !ledger.details.contains_key(&format!("detail-{n}")),
                "inactive binding's settled detail-{n} must be evicted"
            );
        }
        assert!(ledger.details.contains_key(&reference));
    }

    /// A pending fact is never an eviction candidate: the request is refused while it is the
    /// only thing left, and it survives the eviction that admits a later request.
    #[tokio::test]
    async fn pending_detail_is_never_evicted() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 2);
        let held = validated_call(&handle.shared.bindings, "hold-actor", "hold-start")
            .binding_ref()
            .clone();
        plant_detail(
            &handle,
            "detail-1",
            &held,
            PeerReply::Pending {
                detail_ref: "detail-1".into(),
            },
        );
        plant_detail(
            &handle,
            "detail-2",
            &held,
            PeerReply::Pending {
                detail_ref: "detail-2".into(),
            },
        );
        let invocation = validated_call(&handle.shared.bindings, "next-actor", "next-start");
        assert!(matches!(
            enqueue_context(&handle, invocation),
            Err(FailureCode::Capacity)
        ));
        assert!(
            handle
                .shared
                .ledger
                .lock()
                .unwrap()
                .details
                .contains_key("detail-1")
        );
        handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .details
            .get_mut("detail-2")
            .unwrap()
            .reply = settled_detail("detail-2");
        let invocation = validated_call(&handle.shared.bindings, "next-actor", "next-retry");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(
            ledger.details.contains_key("detail-1"),
            "pending must survive"
        );
        assert!(!ledger.details.contains_key("detail-2"));
        assert!(ledger.details.contains_key(&reference));
    }

    /// A sole active binding at the ceiling evicts its own oldest settled detail for its next
    /// request, keeping the newer history.
    #[tokio::test]
    async fn sole_active_binding_evicts_its_own_oldest_settled_details() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 10);
        let sole = validated_call(&handle.shared.bindings, "sole-actor", "sole-start")
            .binding_ref()
            .clone();
        for n in 1..=10 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &sole, settled_detail(&reference));
        }
        handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .cancellation
            .insert(sole.clone(), watch::channel(false).0);
        let invocation = validated_call(&handle.shared.bindings, "sole-actor", "sole-next");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(
            !ledger.details.contains_key("detail-1"),
            "the requesting binding's oldest settled detail must be evicted"
        );
        for n in 2..=10 {
            assert!(ledger.details.contains_key(&format!("detail-{n}")));
        }
        assert!(ledger.details.contains_key(&reference));
    }

    /// A source detail is pinned while newer jobs evict the binding's settled history.
    #[tokio::test]
    async fn newest_read_source_survives_more_than_eight_settled_details() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 10);
        let binding = validated_call(&handle.shared.bindings, "read-actor", "read-start")
            .binding_ref()
            .clone();
        let worktree = crate::workspace::authority::WorktreeRef::from_discovery(
            fixture.root.clone(),
            fixture.root.clone(),
            ".git".into(),
            1,
        )
        .unwrap();
        for n in 1..=10 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &binding, settled_detail(&reference));
            handle
                .shared
                .ledger
                .lock()
                .unwrap()
                .details
                .get_mut(&reference)
                .unwrap()
                .source = Some(
                SourceObservation::new(
                    worktree.clone(),
                    1,
                    n,
                    crate::workspace::observation::ObservationRef::new(format!("source-{n}"))
                        .unwrap(),
                    "main.rs".into(),
                    Some(crate::workspace::observation::SourceBytes::from_bytes(b"x")),
                    crate::workspace::observation::SourceRevision::new(format!("rev-{n}")).unwrap(),
                    crate::workspace::observation::SourceCoverage::Complete,
                    crate::workspace::observation::ObservedState::Present,
                )
                .unwrap(),
            );
        }
        let mut ledger = handle.shared.ledger.lock().unwrap();
        assert_eq!(
            evict_binding_oldest(&mut ledger, 10, &binding, &BTreeSet::new(), 8, true),
            1
        );
        assert!(!ledger.details.contains_key("detail-1"));
        assert!(ledger.details.contains_key("detail-10"));
        assert_eq!(
            newest_edit_source(&ledger, &binding, "main.rs").as_deref(),
            Some("detail-10")
        );
        assert!(
            admitted_edit_source(
                &ledger.details["detail-10"],
                &binding,
                "detail-10",
                "main.rs"
            )
            .is_some()
        );
    }

    /// Admits a binding holding sixteen settled results, including eight distinct source bases.
    #[tokio::test]
    async fn active_binding_at_floor_can_evict_own_source_details() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 16);
        let actor_a = validated_call(&handle.shared.bindings, "actor-a", "a-start")
            .binding_ref()
            .clone();
        let worktree = crate::workspace::authority::WorktreeRef::from_discovery(
            fixture.root.clone(),
            fixture.root.clone(),
            ".git".into(),
            1,
        )
        .unwrap();
        for n in 1..=16 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &actor_a, settled_detail(&reference));
            if n <= 8 {
                handle
                    .shared
                    .ledger
                    .lock()
                    .unwrap()
                    .details
                    .get_mut(&reference)
                    .unwrap()
                    .source = Some(
                    SourceObservation::new(
                        worktree.clone(),
                        1,
                        n,
                        crate::workspace::observation::ObservationRef::new(format!("source-{n}"))
                            .unwrap(),
                        format!("file-{n}.rs").into(),
                        Some(crate::workspace::observation::SourceBytes::from_bytes(b"x")),
                        crate::workspace::observation::SourceRevision::new(format!("rev-{n}"))
                            .unwrap(),
                        crate::workspace::observation::SourceCoverage::Complete,
                        crate::workspace::observation::ObservedState::Present,
                    )
                    .unwrap(),
                );
            }
        }
        let now = tokio::time::Instant::now();
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger
                .cancellation
                .insert(actor_a.clone(), watch::channel(false).0);
            ledger.last_activity.insert(actor_a.clone(), now);
        }
        let invocation = validated_call(&handle.shared.bindings, "actor-a", "a-next");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(!ledger.details.contains_key("detail-1"));
        assert!(ledger.details.contains_key("detail-2"));
        assert!(ledger.details.contains_key("detail-9"));
        assert!(ledger.details.contains_key(&reference));
    }

    /// Lets one active binding replace its oldest result when both bindings hold eight settled
    /// results and the shared result store is full.
    #[tokio::test]
    async fn active_bindings_at_floor_evict_requesters_oldest() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 16);
        let a = validated_call(&handle.shared.bindings, "actor-a", "a-start")
            .binding_ref()
            .clone();
        let b = validated_call(&handle.shared.bindings, "actor-b", "b-start")
            .binding_ref()
            .clone();
        for n in 1..=16 {
            let reference = format!("detail-{n}");
            plant_detail(
                &handle,
                &reference,
                if n <= 8 { &a } else { &b },
                settled_detail(&reference),
            );
        }
        let now = tokio::time::Instant::now();
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            for binding in [&a, &b] {
                ledger
                    .cancellation
                    .insert(binding.clone(), watch::channel(false).0);
                ledger.last_activity.insert(binding.clone(), now);
            }
        }
        let invocation = validated_call(&handle.shared.bindings, "actor-a", "a-next");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(!ledger.details.contains_key("detail-1"));
        assert!(ledger.details.contains_key("detail-2"));
        assert!(ledger.details.contains_key("detail-9"));
        assert!(ledger.details.contains_key(&reference));
    }

    /// Reclaims idle settled results for admission while retaining an idle binding's pending work.
    #[tokio::test]
    async fn idle_binding_settled_details_are_reclaimed_but_pending_survives() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 4);
        let idle = validated_call(&handle.shared.bindings, "idle-actor", "idle-start")
            .binding_ref()
            .clone();
        let now = tokio::time::Instant::now();
        for n in 1..=4 {
            let reference = format!("detail-{n}");
            let reply = if n == 4 {
                PeerReply::Pending {
                    detail_ref: reference.clone(),
                }
            } else {
                settled_detail(&reference)
            };
            plant_detail(&handle, &reference, &idle, reply);
        }
        {
            let mut ledger = handle.shared.ledger.lock().unwrap();
            ledger
                .cancellation
                .insert(idle.clone(), watch::channel(false).0);
            ledger.last_activity.insert(
                idle.clone(),
                now - IDLE_BINDING_DETAILS_TTL - Duration::from_secs(1),
            );
        }
        let invocation = validated_call(&handle.shared.bindings, "active-actor", "active-start");
        let new_reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(!ledger.details.contains_key("detail-1"));
        assert!(!ledger.details.contains_key("detail-2"));
        assert!(!ledger.details.contains_key("detail-3"));
        assert!(
            ledger.details.contains_key("detail-4"),
            "pending results remain protected"
        );
        assert!(ledger.details.contains_key(&new_reference));
    }

    /// A retained test run's output detail is never evicted, so its full output stays readable
    /// between polls: the next-oldest settled detail yields instead.
    #[tokio::test]
    async fn retained_test_output_detail_is_never_evicted() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 10);
        let sole = validated_call(&handle.shared.bindings, "sole-actor", "sole-start")
            .binding_ref()
            .clone();
        for n in 1..=10 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &sole, settled_detail(&reference));
        }
        handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .cancellation
            .insert(sole.clone(), watch::channel(false).0);
        assert!(matches!(
            handle.shared.test_runs.start(
                fixture.root.clone(),
                vec!["/bin/echo".into(), "pass".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(30),
                "detail-1".into(),
                &sole,
            ),
            StartResult::Started(_)
        ));
        let invocation = validated_call(&handle.shared.bindings, "sole-actor", "sole-next");
        let reference = enqueue_context(&handle, invocation).unwrap();
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(
            ledger.details.contains_key("detail-1"),
            "the test run's output detail must survive"
        );
        assert!(!ledger.details.contains_key("detail-2"));
        assert!(ledger.details.contains_key(&reference));
    }

    /// An explicit stop still clears that binding's details and its cancellation entry.
    #[tokio::test]
    async fn stop_still_clears_the_binding_details() {
        let fixture = Fixture::new();
        let handle = detail_handle(&fixture.root, 8);
        let binding = validated_call(&handle.shared.bindings, "stop-actor", "stop-start")
            .binding_ref()
            .clone();
        for n in 1..=4 {
            let reference = format!("detail-{n}");
            plant_detail(&handle, &reference, &binding, settled_detail(&reference));
        }
        handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .cancellation
            .insert(binding.clone(), watch::channel(false).0);
        let invocation = validated_call(&handle.shared.bindings, "stop-actor", "stop-call");
        let _ = handle.stop(invocation, "stop-retry").await;
        let ledger = handle.shared.ledger.lock().unwrap();
        assert!(
            ledger
                .details
                .values()
                .all(|detail| detail.binding != binding),
            "stop must clear the binding's details"
        );
        assert!(!ledger.cancellation.contains_key(&binding));
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
        job.parameters["byte_offset"] = serde_json::json!(3);

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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
        job.parameters["byte_offset"] = serde_json::json!(3);
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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
        job.parameters["byte_offset"] = serde_json::json!(0);
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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
        let PeerReply::Error { code, detail } = reply_rx.await.unwrap() else {
            panic!("a stale reference must fail invalid_detail")
        };
        assert_eq!(code, FailureCode::InvalidDetail);
        assert_eq!(detail.as_deref(), Some("inspect:detail_unknown"));

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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
        let PeerReply::Error { code, detail } = reply_rx.await.unwrap() else {
            panic!("a foreign reference must fail invalid_detail")
        };
        assert_eq!(code, FailureCode::InvalidDetail);
        assert_eq!(detail.as_deref(), Some("inspect:detail_unknown"));
    }

    /// Every handle shape an agent reaches for names its run; a minted detail reference never
    /// does, so the alias cannot shadow a retained detail.
    #[test]
    fn test_run_handles_parse_every_shape_but_no_minted_reference() {
        assert_eq!(test_run_handle("tests #3"), Some(3));
        assert_eq!(test_run_handle("tests-3"), Some(3));
        assert_eq!(test_run_handle("#3"), Some(3));
        assert_eq!(test_run_handle("3"), Some(3));
        assert_eq!(test_run_handle("tests 3"), Some(3));
        assert_eq!(test_run_handle("0"), None);
        assert_eq!(test_run_handle("tests"), None);
        assert_eq!(test_run_handle("sym-14"), None);
        assert_eq!(
            test_run_handle(&format!(
                "{}-7",
                blake3::Hash::from_bytes([0_u8; 32]).to_hex()
            )),
            None
        );
        assert!(never_issued(
            &format!("{}-7", blake3::Hash::from_bytes([1_u8; 32]).to_hex()),
            &[0_u8; 32],
            10
        ));
        assert!(!never_issued(
            &format!("{}-7", blake3::Hash::from_bytes([0_u8; 32]).to_hex()),
            &[0_u8; 32],
            10
        ));
        // A number this daemon has not minted yet was never issued.
        assert!(never_issued(
            &format!("{}-11", blake3::Hash::from_bytes([0_u8; 32]).to_hex()),
            &[0_u8; 32],
            10
        ));
    }

    /// A branch switched outside the IDE leaves one `git:` notice naming both commits and
    /// branches; once consumed it is not repeated, and the refreshed baseline stays quiet.
    #[tokio::test]
    async fn head_moved_outside_the_ide_leaves_one_notice() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        git_commit(&fixture.root, "head fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, authority) = activate_worktree(&mut worker, "head-actor", "head-start").await;
        let owner = binding.fingerprint();
        let notice = |worker: &Worker<'_>| {
            worker
                .shared
                .git_notices
                .lock()
                .unwrap()
                .get(&owner)
                .cloned()
        };
        worker.observe_head(&binding, &authority);
        expire_head_probe(&mut worker, &binding);
        worker.observe_head(&binding, &authority);
        assert_eq!(notice(&worker), None, "an unchanged head says nothing");
        let status = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["checkout", "--quiet", "--detach"])
            .status()
            .unwrap();
        assert!(status.success());
        expire_head_probe(&mut worker, &binding);
        worker.observe_head(&binding, &authority);
        let line = notice(&worker).expect("a switched head leaves a notice");
        assert!(
            line.starts_with("git: HEAD moved ")
                && line.contains(
                    " → detached) outside Agent IDE; earlier indexed answers may be stale"
                ),
            "{line}"
        );
        worker.shared.git_notices.lock().unwrap().remove(&owner);
        expire_head_probe(&mut worker, &binding);
        worker.observe_head(&binding, &authority);
        assert_eq!(notice(&worker), None, "the refreshed baseline stays quiet");
    }

    /// Ages `binding`'s last `HEAD` probe past the interval, so its next probe reads Git again.
    fn expire_head_probe(worker: &mut Worker<'_>, binding: &BindingRef) {
        if let Some((last, _)) = worker.heads.get_mut(binding) {
            *last -= HEAD_PROBE_INTERVAL;
        }
    }

    /// Rapid tool calls do not re-read Git's refs: a probe inside the per-binding interval is
    /// skipped, so a head switched meanwhile is noticed by the first probe after the interval.
    #[tokio::test]
    async fn head_probe_is_rate_limited_per_binding() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        git_commit(&fixture.root, "rate fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, authority) = activate_worktree(&mut worker, "rate-actor", "rate-start").await;
        let owner = binding.fingerprint();
        let notice = |worker: &Worker<'_>| {
            worker
                .shared
                .git_notices
                .lock()
                .unwrap()
                .get(&owner)
                .cloned()
        };
        worker.observe_head(&binding, &authority);
        let status = std::process::Command::new("/usr/bin/git")
            .arg("-C")
            .arg(&fixture.root)
            .args(["checkout", "--quiet", "--detach"])
            .status()
            .unwrap();
        assert!(status.success());
        worker.observe_head(&binding, &authority);
        assert_eq!(
            notice(&worker),
            None,
            "a probe inside the interval does not read Git again"
        );
        expire_head_probe(&mut worker, &binding);
        worker.observe_head(&binding, &authority);
        assert!(
            notice(&worker).is_some_and(|line| line.contains(" → detached) outside Agent IDE")),
            "the first probe after the interval notices the switch"
        );
    }

    /// Once a binding holds every running slot of its owner (two live language servers under the
    /// fixed limits), the next admission — the Git spawn of `ide.diff` — is refused with
    /// `Capacity` and leaves no queued ticket, instead of relocking the admission mutex it still
    /// holds (the 0.6.1 hang). The body runs on its own thread, so a deadlock fails after a
    /// bounded wait.
    #[test]
    fn admission_at_the_owner_ceiling_is_refused_not_deadlocked() {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let fixture = Fixture::new();
                let store = fixture.store();
                let workspace = DurableWorkspace::open(&store).await.unwrap();
                let mut worker = worker(&store, workspace, fixture.root.clone());
                let binding = BindingRef::fixture("ceiling-actor", "ceiling-channel", 1);
                let limit = worker.admission().inspect().per_owner_running_limit;
                let held: Vec<_> = (0..limit).map(|_| worker.admit(&binding)).collect();
                assert!(held.iter().all(Result::is_ok), "the owner fills its slots");
                let third = worker.admit(&binding);
                let queued = worker.admission().inspect().queued;
                let _ = sender.send((third.err(), queued));
            });
        });
        let (third, queued) = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("admission at the owner ceiling deadlocked the worker");
        assert_eq!(third, Some(FailureCode::Capacity));
        assert_eq!(queued, 0, "the refused request leaves no queued ticket");
    }

    /// Language servers hold at most `per_owner_running − 1` of one binding's slots: with that
    /// many live, one more server is refused while its slot stays free, and a worker operation
    /// (the Git spawn of `ide.diff`) is admitted into it; only then is the owner full.
    #[tokio::test]
    async fn servers_leave_one_owner_slot_for_worker_operations() {
        use crate::execution::{
            AdmissionClass, AdmissionError, ProviderBackendKind, ProviderLeaseAdmission,
            ProviderLeaseLimits, ProviderLeaseRegistry, WorkspaceAuthority,
        };
        let fixture = Fixture::new();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        let binding = BindingRef::fixture("share-actor", "share-channel", 1);
        let owner = crate::intelligence::server::owner(&binding).unwrap();
        let authority =
            WorkspaceAuthority::from_workspace("tree", "1", fixture.root.clone(), 1).unwrap();
        let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
            total_views: 8,
            per_backend_views: 1,
        })
        .unwrap();
        let mut server = |admission: &mut crate::execution::AdmissionController, name: String| {
            registry.request(
                admission,
                owner.clone(),
                AdmissionClass::Interactive,
                name,
                ProviderBackendKind::OwnedExclusive,
                &authority,
            )
        };
        let limit = worker.admission().inspect().per_owner_running_limit;
        assert_eq!(
            limit,
            crate::lang::registered().len().max(1) + 1,
            "one slot per registered language plus one"
        );
        for index in 1..limit {
            let granted = server(&mut worker.admission(), format!("server-{index}"));
            assert!(matches!(granted, ProviderLeaseAdmission::Granted(_)));
        }
        assert_eq!(
            server(&mut worker.admission(), "one-server-too-many".to_owned()),
            ProviderLeaseAdmission::Refused(AdmissionError::OwnerProviderLimit)
        );
        assert_eq!(
            worker.admission().inspect().reserved,
            limit - 1,
            "the refused server leaves the last slot free"
        );
        assert!(
            worker.admit(&binding).is_ok(),
            "the kept slot admits the operation"
        );
        assert_eq!(
            worker.admit(&binding).err(),
            Some(FailureCode::Capacity),
            "only now is the owner full"
        );
    }

    /// After `ide.stop` the actor/channel identity still reads its test run's status through the
    /// handle — without the dropped `full output` detail — while any other reference is refused.
    #[tokio::test]
    async fn test_handle_answers_after_stop_and_other_references_do_not() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        git_commit(&fixture.root, "stop fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = activate_worktree(&mut worker, "stop-actor", "stop-start").await;
        assert!(matches!(
            worker.shared.test_runs.start(
                fixture.root.clone(),
                vec!["/bin/echo".into(), "pass".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(30),
                "run-detail".into(),
                &binding,
            ),
            StartResult::Started(1)
        ));
        while worker
            .shared
            .test_runs
            .status_line(&fixture.root)
            .is_some_and(|line| line.contains("running"))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        worker
            .shared
            .bindings
            .lock()
            .unwrap()
            .stop_binding(&binding)
            .unwrap();
        let identity = binding.channel_identity();
        let inspect = |binding: BindingRef, reference: String| {
            let (reply_tx, reply_rx) = oneshot::channel();
            let request = Inspection {
                binding,
                reference,
                expected: None,
                reply: reply_tx,
            };
            let (workspace, shared) = (&worker.workspace, &worker.shared);
            async move {
                serve_inspection(workspace, shared, request).await;
                reply_rx.await.unwrap()
            }
        };
        let PeerReply::Complete { text, .. } = inspect(identity.clone(), "tests #1".into()).await
        else {
            panic!("a test handle must answer after stop")
        };
        assert!(
            text.starts_with("tests #1: 1 passed, 0 failed") && !text.contains("full output"),
            "{text}"
        );
        // Another actor's identity on the same channel cannot inspect the run.
        let stranger = BindingRef::fixture("other-actor", "stop-channel", 1).channel_identity();
        assert_eq!(
            inspect(stranger, "tests #1".into()).await,
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
                detail: Some("test:unknown_run:1".to_owned()),
            }
        );
        // Any other reference still requires the active generation.
        assert!(matches!(
            inspect(identity, "run-detail".into()).await,
            PeerReply::Error {
                code: FailureCode::Cancelled,
                ..
            }
        ));
    }

    /// `ide.inspect` answers a test-run handle with that run's status, and an unknown detail
    /// reference is split into never-issued and expired.
    #[tokio::test]
    async fn inspect_answers_test_handles_and_splits_unknown_from_expired() {
        crate::lang::testing::install();
        let gamma = crate::lang::Language::by_id("gamma").expect("test language");
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        git_commit(&fixture.root, "inspect fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, _) = activate_worktree(&mut worker, "inspect-actor", "inspect-start").await;

        let inspect = |reference: String| async {
            let (reply_tx, reply_rx) = oneshot::channel();
            serve_inspection(
                &worker.workspace,
                &worker.shared,
                Inspection {
                    binding: binding.clone(),
                    reference,
                    expected: None,
                    reply: reply_tx,
                },
            )
            .await;
            reply_rx.await.unwrap()
        };

        // A finished run's handle answers with its parsed result.
        match worker.shared.test_runs.start(
            fixture.root.clone(),
            vec!["/bin/echo".into(), "pass".into()],
            gamma,
            Duration::from_secs(30),
            "echo-run-detail".into(),
            &binding,
        ) {
            StartResult::Started(id) => assert_eq!(id, 1),
            StartResult::Running(..) | StartResult::Failed { .. } => {
                panic!("echo run must start")
            }
        }
        let completed = loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let reply = inspect("tests #1".into()).await;
            let PeerReply::Complete { text, .. } = &reply else {
                panic!("handle alias must answer, got {reply:?}")
            };
            if text.contains("1 passed") {
                break text.clone();
            }
        };
        assert!(
            completed.starts_with("tests #1: 1 passed, 0 failed")
                && completed.contains("full output: ide.inspect echo-run-detail"),
            "{completed}"
        );

        // A still-running run's handle answers with the running line and the poll hint.
        let sleep_started = loop {
            match worker.shared.test_runs.start(
                fixture.root.clone(),
                vec!["/bin/sleep".into(), "30".into()],
                gamma,
                Duration::from_secs(60),
                "sleep-run-detail".into(),
                &binding,
            ) {
                StartResult::Started(id) => break id,
                StartResult::Running(..) => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                StartResult::Failed { .. } => panic!("sleep run must start"),
            }
        };
        assert_eq!(sleep_started, 2);
        let running = inspect("#2".into()).await;
        let PeerReply::Complete { text, .. } = &running else {
            panic!("handle alias must answer, got {running:?}")
        };
        assert!(
            text.starts_with("tests #2: running")
                && text.ends_with("poll: call ide.test with {\"status\": 2}"),
            "{text}"
        );

        // A run that cannot be found gets a distinct invalid-detail stage, not a useless poll hint.
        let unknown = inspect("tests-9".into()).await;
        assert_eq!(
            unknown,
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
                detail: Some("test:unknown_run:9".to_owned()),
            }
        );

        // A reference this daemon minted but no longer retains reads expired; one it could
        // never have minted stays unknown.
        worker.shared.ledger.lock().unwrap().next = 10;
        let minted = format!(
            "{}-5",
            blake3::Hash::from_bytes(worker.shared.nonce).to_hex()
        );
        let PeerReply::Error { detail, .. } = inspect(minted).await else {
            panic!("a dropped reference must fail")
        };
        assert_eq!(detail.as_deref(), Some("inspect:detail_expired"));
        let PeerReply::Error { detail, .. } = inspect("never-retained".into()).await else {
            panic!("an unminted reference must fail")
        };
        assert_eq!(detail.as_deref(), Some("inspect:detail_unknown"));
    }

    /// A repeated ide.start under one activation_id with a different root reaches the earlier
    /// start's retained reference; the mismatch names its own stage instead of a generic one.
    #[tokio::test]
    async fn repeated_start_with_another_root_names_its_own_stage() {
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("main.rs"), "let value = 1;\n").unwrap();
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (binding, authority) =
            activate_worktree(&mut worker, "conflict-actor", "conflict-start").await;
        // The enqueue dedup key is (binding, activation_id); the retained detail is the first
        // start's, and the retried call differs only in its root.
        let first = serde_json::json!({"activation_id":"same-id"});
        worker.shared.ledger.lock().unwrap().details.insert(
            "start-first".into(),
            Detail {
                binding: binding.clone(),
                reply: PeerReply::Complete {
                    kind: ResultKind::Activation,
                    text: "activated".into(),
                    detail_ref: None,
                    truncated: false,
                    continuation: false,
                },
                selection: (AssistanceTool::Start, selection(&first)),
                authority: Some(authority),
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
        let retry = serde_json::json!({"activation_id":"same-id","root":"/another/root"});
        let (reply_tx, reply_rx) = oneshot::channel();
        serve_inspection(
            &worker.workspace,
            &worker.shared,
            Inspection {
                binding,
                reference: "start-first".into(),
                expected: Some((AssistanceTool::Start, selection(&retry))),
                reply: reply_tx,
            },
        )
        .await;
        let PeerReply::Error { code, detail } = reply_rx.await.unwrap() else {
            panic!("a conflicting activation retry must fail")
        };
        assert_eq!(code, FailureCode::InvalidDetail);
        assert_eq!(detail.as_deref(), Some("start:activation_conflict"));
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
        job.parameters["byte_offset"] = serde_json::json!(2);
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
                line_movement: None,
                diff_page: None,
                diff_page_fresh: false,
                context_page: None,
                context_page_fresh: false,
                diff_provenance: None,
                extra_sources: Vec::new(),
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
                format_note: None,
                check_scheduled: false,
                park_until: None,
                stage: None,
                session_binding: None,
            },
            cancel_sender,
        )
    }
    #[tokio::test]
    async fn diff_streams_drain_at_the_workspace_boundary_not_the_launcher_budget() {
        // The launcher's 1 KiB child-capture budget no longer truncates fixed Git metadata:
        // a tree whose listings exceed it still diffs.
        {
            let fixture = Fixture::new();
            for n in 0..30 {
                std::fs::write(fixture.root.join(format!("file-{n:02}.txt")), "base\n").unwrap();
            }
            git_commit(&fixture.root, "stream baseline");
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
            assert!(
                worker.diff(&mut job).await.is_ok(),
                "the launcher output budget must not cap git evidence"
            );
        }
        // The Workspace evidence boundary itself still caps: a worktree file above
        // `MAX_SOURCE_BYTES` fails the capture with the explicit finite-budget error.
        {
            let fixture = Fixture::new();
            std::fs::write(fixture.root.join("large.txt"), vec![b'a'; 4096]).unwrap();
            git_commit(&fixture.root, "stream baseline");
            std::fs::write(
                fixture.root.join("large.txt"),
                vec![b'b'; crate::workspace::observation::MAX_SOURCE_BYTES + 1],
            )
            .unwrap();
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
                FailureCode::Capacity
            );
            assert_eq!(job.failure_detail.as_deref(), Some("diff:too_large"));
        }
    }

    /// Builds one symbol-tool job for `tool` with `parameters` under a live binding.
    fn tool_job(
        root: &std::path::Path,
        invocation: ValidatedInvocation,
        reference: &str,
        tool: AssistanceTool,
        parameters: Value,
    ) -> (Job, watch::Sender<bool>) {
        let (cancel_sender, cancel) = watch::channel(false);
        (
            Job {
                reference: reference.into(),
                invocation,
                tool,
                parameters,
                target: production_target(root),
                deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                cancel,
                stop_reply: None,
                native_epoch: 0,
                failure_detail: None,
                format_note: None,
                check_scheduled: false,
                park_until: None,
                stage: None,
                session_binding: None,
            },
            cancel_sender,
        )
    }

    /// A language no server owns but that outlines from source answers outline, read, symbol
    /// and graph from that outline, callers reported unavailable by its own id and the names it
    /// uses linked from the index; a sigil address answers a name card from the index and
    /// `ide.read` reads its definition; a language with neither keeps the provider-unavailable
    /// refusal.
    #[tokio::test]
    async fn serverless_language_answers_symbol_tools_from_its_source_outline() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(
            fixture.root.join("a.gamma"),
            "sym card\n  sym btn\n  #top\n  end\nend\n",
        )
        .unwrap();
        std::fs::write(fixture.root.join("b.alpha"), "#top @btn\n").unwrap();
        std::fs::write(fixture.root.join("b.delta"), "sym card\nend\n").unwrap();
        git_commit(&fixture.root, "serverless fixture");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        activate_worktree(&mut worker, "serverless-actor", "serverless-start").await;
        let mut calls = 0;
        // The activation prewarms the name index; a job that finds it building is parked and
        // retried, exactly as the worker loop does.
        let mut run = async |tool: AssistanceTool, parameters: Value| loop {
            calls += 1;
            let invocation = production_call(
                &worker,
                "serverless-actor",
                &format!("serverless-call-{calls}"),
            );
            let (mut job, _cancel) = tool_job(
                &fixture.root,
                invocation,
                "serverless",
                tool,
                parameters.clone(),
            );
            let result = match tool {
                AssistanceTool::Outline => worker.outline(&mut job).await,
                AssistanceTool::Read => worker.read(&mut job).await,
                AssistanceTool::Symbol => worker.symbol(&mut job).await,
                _ => worker.graph(&mut job).await,
            };
            if job.park_until.is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            break match result.map(|(reply, _, _)| reply) {
                Ok(PeerReply::Complete { text, .. }) => Ok(text),
                Ok(other) => panic!("not a complete reply: {other:?}"),
                Err(code) => Err(code),
            };
        };
        assert_eq!(
            run(
                AssistanceTool::Outline,
                serde_json::json!({"path":"a.gamma"})
            )
            .await,
            Ok(
                "a.gamma  (5 lines, gamma)\n    1  sym card\n    2    sym btn\n  (2 symbols)\n"
                    .into()
            )
        );
        let read = run(
            AssistanceTool::Read,
            serde_json::json!({"symbol":"a.gamma#card/btn"}),
        )
        .await
        .unwrap();
        assert!(
            read.starts_with("a.gamma#card/btn  (lines 2–4)\n2\t  sym btn\n3\t  #top\n4\t  end\n"),
            "{read}"
        );
        assert_eq!(
            run(
                AssistanceTool::Symbol,
                serde_json::json!({"symbol":"a.gamma#card/btn"})
            )
            .await,
            Ok(
                "symbol: btn — symbol, a.gamma#card/btn (lines 2–4)\nsignature: sym btn\n\
                definition a.gamma#card/btn  (lines 2–4)\n\
                links: 1 element id used here\n  ##top  → b.alpha:1 #top @btn\n\
                unavailable for: delta\n\
                callers: unavailable (gamma has no call hierarchy)\n"
                    .into()
            )
        );
        assert_eq!(
            run(
                AssistanceTool::Symbol,
                serde_json::json!({"symbol":"##top"})
            )
            .await,
            Ok(
                "symbol: ##top — element id, 1 element, 1 usage in 1 files (alpha, gamma)\n\
                definitions:\n  b.alpha:1  [alpha] #top @btn\n\
                usages: 1 indexed in 1 files (src 1, tests 0)\n  a.gamma:3  [gamma] #top\n\
                unavailable for: delta\n"
                    .into()
            )
        );
        assert_eq!(
            run(AssistanceTool::Read, serde_json::json!({"symbol":"##top"})).await,
            Ok("b.alpha:1  (lines 1)\n1\t#top @btn\nsource_ref: serverless\n".into())
        );
        assert_eq!(
            run(
                AssistanceTool::Graph,
                serde_json::json!({"symbol":"a.gamma#card"})
            )
            .await,
            Ok(
                "graph: callers/callees unavailable for gamma (gamma has no call hierarchy)\n"
                    .into()
            )
        );
        assert_eq!(
            run(
                AssistanceTool::Outline,
                serde_json::json!({"path":"b.delta"})
            )
            .await,
            Err(FailureCode::ProviderUnavailable)
        );
    }

    /// A bare name never refreshes the name index of a worktree without files of a language
    /// with name facts; once such a file exists, the same lookup builds it.
    #[tokio::test]
    async fn bare_names_skip_the_index_without_bridged_files() {
        crate::lang::testing::install();
        let fixture = Fixture::new();
        std::fs::write(fixture.root.join("b.delta"), "sym card\nend\n").unwrap();
        git_commit(&fixture.root, "no bridged files");
        let store = fixture.store();
        let workspace = DurableWorkspace::open(&store).await.unwrap();
        let mut worker = worker(&store, workspace, fixture.root.clone());
        worker.observations.install_schema().await.unwrap();
        let (_, authority) = activate_worktree(&mut worker, "guard-actor", "guard-start").await;
        for (call, expect_index) in [("guard-1", false), ("guard-2", true)] {
            if expect_index {
                std::fs::write(fixture.root.join("a.alpha"), "@card\n").unwrap();
            }
            let invocation = production_call(&worker, "guard-actor", call);
            let (mut job, _cancel) = tool_job(
                &fixture.root,
                invocation,
                call,
                AssistanceTool::Symbol,
                serde_json::json!({"symbol":"card"}),
            );
            let _ = worker.symbol(&mut job).await;
            assert_eq!(
                worker.names.contains(authority.worktree()),
                expect_index,
                "{call}"
            );
        }
    }

    /// Every terminal failure carries a stage tag — the exact string the journal records in
    /// `detail` — either the failing path's own tag or the derived `<tool>:<reason>` default.
    /// Drives the four failure shapes the journal audit called out: a bare symbol name on a
    /// worktree with no anchor, an inspection of an unknown detail, a context on a missing
    /// path, and a diff on a non-Git root.
    #[tokio::test]
    async fn every_failed_reply_names_its_stage_for_the_journal() {
        fn stage_of(reply: &PeerReply) -> String {
            match reply {
                PeerReply::Error { detail, .. } => detail.clone().expect("a stage tag"),
                other => panic!("expected an error reply, got {other:?}"),
            }
        }

        // A bare symbol name with no source anchor anywhere in the worktree.
        {
            let fixture = Fixture::new();
            let store = fixture.store();
            let workspace = DurableWorkspace::open(&store).await.unwrap();
            let mut worker = worker(&store, workspace, fixture.root.clone());
            worker.observations.install_schema().await.unwrap();
            let (_binding, _authority) =
                activate_worktree(&mut worker, "stage-actor", "stage-start").await;
            let invocation = production_call(&worker, "stage-actor", "stage-symbol");
            let (mut job, _cancel) = {
                let (cancel_sender, cancel) = watch::channel(false);
                (
                    Job {
                        reference: "stage-symbol".into(),
                        invocation,
                        tool: AssistanceTool::Symbol,
                        parameters: serde_json::json!({"symbol":"never_defined_anywhere"}),
                        target: production_target(&fixture.root),
                        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
                        cancel,
                        stop_reply: None,
                        native_epoch: 0,
                        failure_detail: None,
                        format_note: None,
                        check_scheduled: false,
                        park_until: None,
                        stage: None,
                        session_binding: None,
                    },
                    cancel_sender,
                )
            };
            let error = worker.symbol(&mut job).await.unwrap_err();
            assert_eq!(error, FailureCode::ProviderUnavailable);
            assert_eq!(job.failure_detail.as_deref(), Some("symbol:anchor_missing"));
        }

        // A context on a source over the read ceiling derives the default `<tool>:<reason>` stage.
        {
            let fixture = Fixture::new();
            std::fs::write(
                fixture.root.join("main.rs"),
                vec![b'a'; crate::workspace::observation::MAX_SOURCE_BYTES + 1],
            )
            .unwrap();
            let store = fixture.store();
            let workspace = DurableWorkspace::open(&store).await.unwrap();
            let mut worker = worker(&store, workspace, fixture.root.clone());
            worker.observations.install_schema().await.unwrap();
            let (_binding, _authority) =
                activate_worktree(&mut worker, "stage-actor", "stage-start").await;
            let invocation = production_call(&worker, "stage-actor", "stage-context");
            let (mut job, _cancel) = context_job(&fixture.root, invocation);
            job.parameters["byte_offset"] = serde_json::json!(0);
            let error = worker.context(&mut job).await.unwrap_err();
            assert!(matches!(error, FailureCode::SourceTooLarge { .. }));
            assert_eq!(
                crate::telemetry::adapters::default_stage(AssistanceTool::Context, &error),
                "context:source_too_large"
            );
        }

        // A diff on a non-Git root names the failing git read.
        {
            let fixture = Fixture::new();
            let store = fixture.store();
            let workspace = DurableWorkspace::open(&store).await.unwrap();
            let mut worker = worker(&store, workspace, fixture.root.clone());
            worker.observations.install_schema().await.unwrap();
            let (binding, authority) =
                activate_worktree(&mut worker, "stage-actor", "stage-start").await;
            let invocation = production_call(&worker, "stage-actor", "stage-diff");
            let (mut job, _cancel) = diff_job(&fixture.root, invocation, "stage-diff");
            retain_detail(
                &worker,
                &binding,
                "stage-diff",
                AssistanceTool::Diff,
                &authority,
            );
            let error = worker.diff(&mut job).await.unwrap_err();
            assert_eq!(error, FailureCode::SourceUnavailable);
            assert!(
                job.failure_detail
                    .as_deref()
                    .is_some_and(|detail| detail.starts_with("diff:")),
                "{:?}",
                job.failure_detail
            );
        }

        // An inspection of an unknown detail names the exact stage.
        {
            let fixture = Fixture::new();
            let store = fixture.store();
            let workspace = DurableWorkspace::open(&store).await.unwrap();
            let mut worker = worker(&store, workspace, fixture.root.clone());
            worker.observations.install_schema().await.unwrap();
            let (binding, _authority) =
                activate_worktree(&mut worker, "stage-actor", "stage-start").await;
            let (reply_tx, reply_rx) = oneshot::channel();
            serve_inspection(
                &worker.workspace,
                &worker.shared,
                Inspection {
                    binding,
                    reference: "never-issued".to_owned(),
                    expected: None,
                    reply: reply_tx,
                },
            )
            .await;
            assert_eq!(stage_of(&reply_rx.await.unwrap()), "inspect:detail_unknown");
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
            detail: None,
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
