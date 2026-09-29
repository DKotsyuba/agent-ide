//! Execution adapter for Workspace's filter-free snapshot collector; no Git syntax is interpreted here.

use super::*;
use crate::{
    assistance::content,
    execution::{
        CapturedProcessEvidence, ControlledCommand, LocalExecutionPolicy, OwnedChild,
        ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
    },
    workspace::{
        authority::AuthorityStamp,
        git::{
            BaselineContext, BaselineCoverage, DiffMode, GitError, GitReadIntent, GitReadQuery,
            GitScope, RawGitEvidence,
            snapshot::{SnapshotIntent, SnapshotRunner, collect_snapshot},
        },
    },
};
use std::collections::BTreeSet;

/// Borrows the sole worker's admission controller while retaining exact job/authority scope.
struct ProductSnapshotRunner<'w, 'store> {
    /// Sole owner of physical admission and durable authorization.
    worker: &'w mut Worker<'store>,
    /// Original current host invocation plus absolute deadline and stop signal.
    job: &'w mut Job,
    /// Exact durable stamp being captured, reauthorized before every physical command.
    authority: AuthorityStamp,
    /// Safe typed failure retained separately from Workspace's parsing errors.
    failure: Option<FailureCode>,
    /// Closed stage label of the most recent intent this runner ran.
    stage: Option<&'static str>,
    /// Closed terminal detail recorded by the runner's own failure branches.
    detail: Option<String>,
}
impl SnapshotRunner for ProductSnapshotRunner<'_, '_> {
    /// Executes only the peer-owned intent, settles direct-child proof, then returns immutable data.
    async fn run(&mut self, intent: SnapshotIntent) -> Result<CapturedProcessEvidence, GitError> {
        self.stage = Some(intent.label());
        match self.run_owned(intent).await {
            Ok(evidence) => Ok(evidence),
            Err(code) => {
                self.failure = Some(code);
                Err(GitError::IncompleteIdentity)
            }
        }
    }
    /// Every snapshot path is worktree-relative below the worktree admitted at activation, so no
    /// per-path proof is needed; the descriptor-relative reader still refuses escapes.
    async fn authorize_read_path(&mut self, _path: &Path) -> Result<(), GitError> {
        Ok(())
    }
    /// Correlates this exact path with its current durable revision/sequence, when one exists.
    /// Never fabricates a token: an absent row, a store error, or a binding that stopped being
    /// live across the awaited store round-trip all fall through to `None`, exactly like a path
    /// Workspace never registered. The whole-snapshot pre/post metadata bracket and the per-path
    /// re-read consistency check still independently catch a source that changed underfoot; this
    /// only adds the optional revision/sequence correlation when it can be durably confirmed.
    async fn current_observation(
        &mut self,
        authority: &AuthorityStamp,
        path: &Path,
    ) -> Option<crate::workspace::store::CurrentObservation> {
        if authority.worktree() != self.authority.worktree()
            || authority.epoch() != self.authority.epoch()
        {
            return None;
        }
        let binding = self.job.invocation.binding_ref().clone();
        let lookup =
            crate::app::store::OperationId::new(format!("snapshot-observe-{}", self.job.reference))
                .ok()?;
        let latest = self
            .worker
            .observations
            .load_latest(
                lookup.clone(),
                authority.worktree().clone(),
                path.to_path_buf(),
            )
            .await
            .ok()??;
        // Re-verify the binding is still live after the awaited durable I/O before trusting or
        // acting on what it returned.
        self.worker.shared.active(&binding).ok()?;
        // A row a previous authority grant recorded (an earlier session's ide.context or
        // ide.edit on a path still modified on disk) is not this grant's observation: the
        // capture would reject it as an unstable snapshot although nothing moved. Fall through
        // to the plain read exactly like a path Workspace never registered.
        if latest.authority_epoch() != authority.epoch() {
            return None;
        }
        let confirmed = self
            .worker
            .observations
            .confirm_current(lookup, latest)
            .await
            .ok()??;
        // The second await is itself an I/O suspension point; a revocation racing with this
        // exact call must not let its already-fetched token escape as if still authorized.
        self.worker.shared.active(&binding).ok()?;
        Some(confirmed)
    }
}
impl ProductSnapshotRunner<'_, '_> {
    /// Names the bounded wait outcome: the job deadline, or one child's own 60-second ceiling.
    fn timeout_detail(&self) -> String {
        let stage = self.stage.unwrap_or("unknown");
        if tokio::time::Instant::now() >= self.job.deadline {
            "diff:deadline".to_owned()
        } else {
            format!("diff:child_timeout:{stage}")
        }
    }

    /// Keeps private snapshot files alive through wait/cancellation/reap; uncertain reaps retain the intent.
    async fn run_owned(
        &mut self,
        intent: SnapshotIntent,
    ) -> Result<CapturedProcessEvidence, FailureCode> {
        let binding = self.job.invocation.binding_ref().clone();
        if intent.scope().worktree() != self.authority.worktree()
            || intent.scope().authority_epoch() != self.authority.epoch()
        {
            self.detail = Some("diff:authority".to_owned());
            return Err(FailureCode::WorkspaceAuthority);
        }
        let command = match intent.command() {
            Ok(command) => command,
            Err(_) => {
                self.detail = Some("diff:scratch".to_owned());
                return Err(FailureCode::SourceUnavailable);
            }
        };
        let request = self
            .worker
            .execution_request(self.job, &self.authority, command, &self.job.target.git)
            .await?;
        let active = self.worker.shared.active(&binding)?;
        let lease = self.worker.admit(&binding)?;
        // Git evidence drains at its own Workspace boundary — [`RawGitEvidence`] accepts at most
        // `MAX_GIT_STDOUT_BYTES` per stream and rejects anything larger — never at the launcher's
        // general `output_bytes` budget. Installed configs set that budget to 64 KiB, which
        // truncates the fixed whole-tree metadata listings (`ls-tree`, `ls-files --stage`,
        // `ls-files --debug`) of any real repository and fails every clean-tree diff with
        // `EvidenceTooLarge` even though the evidence boundary itself would have accepted them.
        let mut child = match OwnedChild::spawn_captured(
            &request,
            lease,
            Some(active),
            crate::workspace::git::MAX_GIT_STDOUT_BYTES,
        ) {
            Ok(child) => child,
            Err(error) => {
                return Err(self.worker.spawn_failure(
                    error,
                    &binding,
                    errorlog_method(self.job.tool),
                ));
            }
        };
        if intent.snapshot_directory().is_some() {
            let identity_correlated = match child.take_process_identity() {
                Some(identity) => intent.bind_process(identity).is_ok(),
                None => false,
            };
            if !identity_correlated {
                self.detail = Some("diff:internal".to_owned());
                self.worker.uncertain.insert(binding);
                self.worker.uncertain_snapshots.push(intent);
                return Err(FailureCode::Internal);
            }
        }
        let remaining = self
            .job
            .deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .min(Duration::from_secs(60));
        let interrupted = tokio::select! {result=child.wait(remaining)=>result.is_err(),_=self.job.cancel.changed()=>true};
        let reaped = if interrupted {
            child
                .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
                .await
        } else {
            child
                .reap(Duration::from_millis(500), Duration::from_millis(100))
                .await
        };
        let completed = match reaped {
            Ok(completed) => completed,
            Err(_) => {
                self.detail = Some(self.timeout_detail());
                self.worker.uncertain.insert(binding);
                self.worker.uncertain_snapshots.push(intent);
                return Err(FailureCode::Deadline);
            }
        };
        if self
            .worker
            .admission()
            .release_reaped(completed.settlement)
            .is_err()
        {
            self.detail = Some("diff:internal".to_owned());
            return Err(FailureCode::Internal);
        }
        self.worker.record_execution(
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
        if intent.acknowledge_reap(&completed.evidence).is_err() {
            self.detail = Some("diff:internal".to_owned());
            return Err(FailureCode::Internal);
        }
        self.worker.shared.active(&binding)?;
        if interrupted {
            if *self.job.cancel.borrow() {
                self.detail = Some("diff:cancelled".to_owned());
                return Err(FailureCode::Cancelled);
            }
            self.detail = Some(self.timeout_detail());
            return Err(FailureCode::Deadline);
        }
        Ok(completed.evidence)
    }
}

/// Names the closed snapshot stage behind a diff failure for the bounded error-log detail.
///
/// Runner-level codes carry their own recorded detail; collector errors are mapped from the
/// closed GitError variant plus the label of the last intent that ran. Never repository paths
/// or child stderr — only these fixed tags.
fn git_failure_detail(
    error: &GitError,
    stage: Option<&str>,
    failure: Option<FailureCode>,
) -> String {
    let stage = stage.unwrap_or("unknown");
    if let Some(code) = failure {
        return match code {
            FailureCode::Cancelled => "diff:cancelled".to_owned(),
            FailureCode::Deadline => "diff:deadline".to_owned(),
            FailureCode::Capacity => "diff:capacity".to_owned(),
            FailureCode::WorkspaceAuthority => "diff:authority".to_owned(),
            FailureCode::ExecutionProfile => "diff:execution_profile".to_owned(),
            FailureCode::Internal => "diff:internal".to_owned(),
            _ => format!("diff:execution:{stage}"),
        };
    }
    match error {
        GitError::UnsupportedSnapshotGit => "diff:unsupported_git".to_owned(),
        GitError::SnapshotIo => "diff:scratch".to_owned(),
        GitError::UnstableSnapshot => "diff:unstable".to_owned(),
        GitError::UnsupportedSnapshot => "diff:unsupported_entry".to_owned(),
        GitError::EvidenceTooLarge => "diff:too_large".to_owned(),
        GitError::ObjectHashMismatch => "diff:hash_mismatch".to_owned(),
        GitError::UnbornHead => "diff:unborn_head".to_owned(),
        _ => format!("diff:child_exit:{stage}"),
    }
}

/// Names the closed pre-spawn wait failure at the execution-request boundary: a real
/// cancellation stays `cancelled`, while deadline expiry alone is `deadline` — never reported
/// as a cancellation just because the two checks used to share one arm (T27B).
fn request_wait_failure(cancel: bool, past_deadline: bool) -> Option<FailureCode> {
    if cancel {
        Some(FailureCode::Cancelled)
    } else if past_deadline {
        Some(FailureCode::Deadline)
    } else {
        None
    }
}

/// Retained bounded state needed to resume one Diff detail cursor from `ide.inspect`.
/// Never serialized; only the same worker that captured `evidence` may expand it further.
#[derive(Clone)]
pub(super) struct DiffPageState {
    /// Exact worktree/incarnation/epoch/mode this page's evidence was captured under; expansion
    /// fails closed the instant the caller's current authority no longer matches it.
    scope: GitScope,
    /// Exact comparison identities and baseline context bound to `evidence`; `expand_diff` rejects
    /// any mismatch against a differently captured comparison.
    comparison: crate::workspace::git::GitComparison,
    /// Complete retained per-path raw Git evidence this and every later page are selected from.
    /// This is the heavy payload `MAX_RETAINED_DIFF_PAGE_BYTES` bounds in aggregate across details.
    evidence: crate::workspace::git::snapshot::GitSnapshot,
    /// Byte/hunk ceiling this comparison was originally captured with; later pages may request a
    /// smaller ceiling (see `expand_with_max_bytes`) but never a larger one.
    budget: crate::changes::DiffSelectionBudget,
    /// Exact scope/generation/operation-bound cursor for the next unselected hunk.
    cursor: crate::changes::DiffDetailCursor,
    /// Compare mode used both to re-derive the expected scope and to render this page's text.
    mode: DiffMode,
    /// Whether the page that minted this cursor was rendered with `provenance: true`; a later
    /// `ide.inspect` page keeps the same rendering the caller originally asked for.
    provenance: bool,
}

impl DiffPageState {
    /// Expands the next bounded page with a reduced *hunk count* while preserving the byte ceiling
    /// this comparison was captured with, so a caller can shrink the page until it proves to fit
    /// the actual serialized reply envelope.
    ///
    /// The captured `max_bytes` is deliberately never lowered: `Changes::select_hunks` treats a
    /// hunk larger than the current `max_bytes` as one that can never fit any page and advances
    /// permanently past it, so a shrinking byte budget would silently drop a hunk that fits the
    /// original ceiling. Reducing only `max_hunks` can never do that — an unselected hunk always
    /// parks the cursor on itself and is delivered by a later page. Never splits a hunk: a smaller
    /// count only ever selects fewer whole hunks.
    pub(super) fn expand_with_max_hunks(
        &self,
        expected_scope: &GitScope,
        max_hunks: usize,
    ) -> crate::changes::DiffResult {
        let budget = crate::changes::DiffSelectionBudget::bounded(max_hunks, self.budget.max_bytes);
        crate::changes::expand_diff(
            expected_scope,
            &self.comparison,
            self.evidence.clone(),
            &self.cursor,
            budget,
        )
    }
    /// Returns the byte/hunk budget originally captured for this comparison.
    pub(super) const fn budget(&self) -> crate::changes::DiffSelectionBudget {
        self.budget
    }
    /// Returns the compare mode used to render this page's text.
    pub(super) const fn mode(&self) -> DiffMode {
        self.mode
    }
    /// Returns whether this page's originating request asked for `provenance: true`.
    pub(super) const fn provenance(&self) -> bool {
        self.provenance
    }
    /// Builds the next retained page state, or `None` once selection no longer overflows.
    pub(super) fn advance(&self, result: &crate::changes::DiffResult) -> Option<Self> {
        result.detail_cursor().map(|cursor| Self {
            scope: self.scope.clone(),
            comparison: self.comparison.clone(),
            evidence: self.evidence.clone(),
            budget: self.budget,
            cursor: cursor.clone(),
            mode: self.mode,
            provenance: self.provenance,
        })
    }
    /// Re-verifies every retained tracked path's working-tree bytes against the current worktree
    /// using the same no-follow reader Workspace itself captured them with. `Staged` never depends
    /// on working-tree content, so it is not a meaningful freshness proof there and this always
    /// reports unchanged; `Head`/`Unstaged` genuinely compare against the working tree, so a
    /// silent out-of-band edit (no native hook, so `native_epoch` never advanced) is caught here.
    pub(super) fn working_tree_bytes_unchanged(
        &self,
        worktree: &crate::workspace::authority::WorktreeRef,
    ) -> bool {
        if self.mode == DiffMode::Staged {
            return true;
        }
        use crate::workspace::observation::{
            ObservationError, SourceReadLimits, read_authorized_source,
        };
        for path in self.evidence.paths() {
            let Some(source) = path.source() else {
                continue;
            };
            let limits = SourceReadLimits::new(
                4096,
                crate::workspace::git::snapshot::MAX_SNAPSHOT_BLOB_BYTES,
            )
            .expect("fixed source limits");
            let current = read_authorized_source(worktree, path.status().path(), limits);
            let matches = match current {
                Ok(read) => source.bytes() == Some(read.bytes()),
                Err(ObservationError::Missing) => source.bytes().is_none(),
                Err(_) => false,
            };
            if !matches {
                return false;
            }
        }
        true
    }
}

/// Runs one single-pass plain `git diff` directly in the worktree as a degraded fallback, used
/// only when the exact two-pass capture proved unstable, and composes it under `budget`. `None` on
/// any failure — spawn, wait, a rejected exit, or output `compose_plain_diff` cannot attribute
/// exactly — so the caller reports the original capture failure instead of a confusing second one
/// or an understated page.
async fn plain_diff_fallback(
    runner: &mut ProductSnapshotRunner<'_, '_>,
    program: &Path,
    scope: &GitScope,
    budget: crate::changes::DiffSelectionBudget,
) -> Option<crate::changes::DiffResult> {
    let intent = SnapshotIntent::plain_diff(scope.clone(), program).ok()?;
    let evidence = runner.run_owned(intent.clone()).await.ok()?;
    let stdout = intent.accept(evidence).ok()?;
    crate::changes::compose_plain_diff(scope, &stdout, budget)
}

/// Hex-encodes raw comparison-side identity bytes for safe inclusion in rendered text.
fn hex_encode(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// Composes one whole-hunk Diff page that provably fits the actual serialized reply envelope.
///
/// This is the single fitting path shared by the initial composition in [`Worker::diff`] and by
/// every later expansion in `serve_inspection`, so both obey the same rule: shrink the page by
/// selecting *fewer whole hunks*, never by lowering the captured byte ceiling and never by cutting
/// rendered text. `compose` is invoked with a candidate hunk count and must return the selection
/// for exactly that count under the originally captured byte budget.
///
/// Fitting is measured through [`content::fits`], the same compact projection and final serialized
/// envelope predicate [`content::render_with_status`] uses for the complete MCP result the host receives. Using
/// anything narrower here — such as the raw serialized [`PeerReply`] with an approximate fixed
/// reserve — could accept a page that the facade then has to cut after its cursor advanced.
///
/// * `mode` — compare mode rendered into the page text.
/// * `authority_epoch` — current durable epoch rendered as provenance.
/// * `reference` — same-binding detail handle echoed as `detail_ref`.
/// * `max_hunks` — largest count to attempt; halved on each retry and clamped to at least one.
/// * `retain_continuation` — whether this caller will retain the accepted cursor for later
///   `ide.inspect`; helper results pass `false` because their settled ticket has no page state.
/// * `provenance` — whether to render today's exact hash-bearing header (`render_diff_provenance`)
///   instead of the compact §2.7 default (`render_diff_compact`).
/// * `compose` — pure selection callback; it must not mutate retained state, because it is called
///   repeatedly and only the returned result of the accepted attempt is retained.
///
/// The rendered `more_available` marker and typed `continuation` flag are both true only when the
/// caller retains a cursor for a later `ide.inspect`.
///
/// Returns the accepted selection together with the exact [`PeerReply`] rendered from it; the
/// caller retains continuation state derived from that same selection.
///
/// # Errors
///
/// * [`FailureCode::SourceUnavailable`] when a candidate selection is structurally unavailable or
///   failed, which no smaller page can repair.
/// * [`FailureCode::Capacity`] when even a single whole hunk cannot fit the serialized envelope.
///   This is deliberately an explicit finite budget failure: the alternative would be delivering a
///   silently cut hunk or claiming an undelivered hunk was delivered.
pub(crate) fn fit_diff_page(
    mode: DiffMode,
    authority_epoch: u64,
    reference: &str,
    max_hunks: usize,
    retain_continuation: bool,
    provenance: bool,
    compose: impl Fn(usize) -> crate::changes::DiffResult,
) -> Result<(crate::changes::DiffResult, PeerReply), FailureCode> {
    let mut max_hunks = max_hunks.max(1);
    loop {
        let candidate = compose(max_hunks);
        if matches!(
            candidate.state(),
            crate::changes::DiffResultState::Unavailable | crate::changes::DiffResultState::Failed
        ) {
            return Err(FailureCode::SourceUnavailable);
        }
        let more_available = retain_continuation && candidate.detail_cursor().is_some();
        let text = if provenance {
            render_diff_provenance(mode, &candidate, authority_epoch, more_available)
        } else {
            let continuation = if more_available {
                DiffContinuationNote::Inspect(reference)
            } else {
                DiffContinuationNote::None
            };
            render_diff_compact(mode, &candidate, continuation, None)
        };
        let reply = PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            detail_ref: Some(reference.to_owned()),
            truncated: candidate.truncated_output()
                || candidate.overflow_hunks() > 0
                || candidate.overflow_bytes() > 0,
            continuation: more_available,
        };
        // The daemon composing this page has no host-kind signal of its own (T14B): only the MCP
        // facade, at final per-call render time, knows whether the caller is Claude or Codex. This
        // stays conservative for both hosts, sized to fit even alongside the structured JSON copy.
        if content::fits(&reply, content::Envelope::WithStructured) {
            return Ok((candidate, reply));
        }
        if max_hunks == 1 {
            return Err(FailureCode::Capacity);
        }
        max_hunks = (max_hunks / 2).max(1);
    }
}

/// Renders the exact typed freshness/coverage/provenance/status facts for one Diff page.
/// Provenance always carries the scope/comparison/operation fields required to interpret this
/// page independent of any other request: worktree identity/incarnation, authority epoch,
/// operation reference, capture generation and both raw comparison-side identities.
///
/// Delivery freshness is always rendered as [`crate::changes::DiffFreshness::Unknown`], including
/// the first ready delivery of a freshly captured page. A retained Diff result is an immutable
/// captured snapshot, not proof of the repository's state at delivery time: the short `ide.inspect`
/// service deliberately performs no heavyweight Git recapture, so HEAD/index identities, untracked
/// and conflict sets and durable current-observation tokens are never revalidated before a page is
/// handed over. Only tracked working-tree bytes are rechecked (see
/// [`DiffPageState::working_tree_bytes_unchanged`]), which cannot establish complete currentness.
/// The freshness computed by Changes at capture time is preserved verbatim as `captured_freshness`
/// alongside the untouched captured comparison identities and provenance, so callers keep the exact
/// capture-time facts without any claim that they still hold now.
fn render_diff_provenance(
    mode: DiffMode,
    result: &crate::changes::DiffResult,
    authority_epoch: u64,
    more_available: bool,
) -> String {
    let provenance = result.provenance();
    let mut text = format!(
        "mode: {:?}\nstate: {:?}\ncoverage: {:?}\nfreshness: {:?}\ncaptured_freshness: {:?}\nauthority_epoch: {}\nworktree_id: {}\nworktree_incarnation: {}\noperation_reference: {}\ncapture_generation: {}\ncomparison_left: {}\ncomparison_right: {}\nbaseline_reference: {}\nbaseline_coverage: {:?}\nbaseline_window: {:?}\ntracked: {}; untracked: {}; conflicted: {}\nomitted_hunks: {}; omitted_bytes: {}; more_available: {}\n",
        mode,
        result.state(),
        result.coverage(),
        crate::changes::DiffFreshness::Unknown,
        result.freshness(),
        authority_epoch,
        result.worktree_id(),
        provenance
            .scope()
            .map_or(0, |scope| scope.worktree().incarnation()),
        provenance.operation_reference().unwrap_or("none"),
        provenance
            .capture_generation()
            .map_or_else(|| "none".to_owned(), |generation| generation.to_string()),
        hex_encode(result.identities().left()),
        hex_encode(result.identities().right()),
        provenance.baseline_reference().unwrap_or("none"),
        provenance.baseline_coverage(),
        provenance.baseline_window(),
        result.counts().tracked(),
        result.counts().untracked(),
        result.counts().conflicted(),
        result.overflow_hunks(),
        result.overflow_bytes(),
        more_available,
    );
    for path in result.tracked() {
        text.push_str(&format!("tracked_path: {:?}\n", path.path()));
    }
    for path in result.untracked() {
        text.push_str(&format!("untracked_path: {:?}\n", path.path()));
    }
    for path in result.conflicts() {
        text.push_str(&format!("conflicted_path: {:?}\n", path.path()));
    }
    // Every file's first hunk is preceded by its `file:` line, so no hunk on any page depends on
    // the header's path list for attribution (T16B).
    let mut current: Option<&std::path::PathBuf> = None;
    for hunk in result.selected_hunks() {
        if current != Some(hunk.path()) {
            text.push_str(&format!("file: {:?}\n", hunk.path()));
            current = Some(hunk.path());
        }
        match std::str::from_utf8(hunk.patch()) {
            Ok(patch) => text.push_str(patch),
            Err(_) => text.push_str(&format!("raw_patch_hex: {:02x?}\n", hunk.patch())),
        }
    }
    text
}

/// Names how a compact Diff page tells its reader that more hunks exist beyond this page.
enum DiffContinuationNote<'a> {
    /// Nothing was omitted; no trailer line.
    None,
    /// A cursor is retained: `ide.inspect` reaches the rest.
    Inspect(&'a str),
    /// Nothing is retained (the aggregate retention ceiling refused it, or this page never
    /// retains a cursor at all): only recapturing with `ide.diff` reaches the rest.
    Recapture,
}

/// Lowercases one compare mode for the compact §2.7 header (`head`/`staged`/`unstaged`).
const fn mode_label(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Head => "head",
        DiffMode::Staged => "staged",
        DiffMode::Unstaged => "unstaged",
    }
}

/// Largest number of untracked/conflicted names shown inline before "+N more".
const MAX_INLINE_DIFF_NAMES: usize = 5;

/// Renders one bounded `label: name, name (+N more)` line, or an empty string for no paths.
fn bounded_names_line(label: &str, paths: &[crate::workspace::git::PathStatus]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let names: Vec<String> = paths
        .iter()
        .take(MAX_INLINE_DIFF_NAMES)
        .map(|path| path.path().display().to_string())
        .collect();
    let hidden = paths.len().saturating_sub(MAX_INLINE_DIFF_NAMES);
    let mut line = format!("{label}: {}", names.join(", "));
    if hidden > 0 {
        line.push_str(&format!(" (+{hidden} more)"));
    }
    line.push('\n');
    line
}

/// Renders the compact §2.7 default reply: one summary line, the untracked/conflicted names Git
/// itself never diffs, per-file hunk text, and a bounded continuation marker — no hash-bearing or
/// bookkeeping fields. `degraded`, when set, is appended in parentheses on the summary line, for
/// the single-pass plain `git diff` fallback that never claims the two-pass capture's exactness.
fn render_diff_compact(
    mode: DiffMode,
    result: &crate::changes::DiffResult,
    continuation: DiffContinuationNote<'_>,
    degraded: Option<&str>,
) -> String {
    let mut text = format!(
        "diff ({}): {} files, +{} \u{2212}{}",
        mode_label(mode),
        result.counts().tracked(),
        result.additions(),
        result.deletions(),
    );
    if let Some(note) = degraded {
        text.push_str(&format!(" ({note})"));
    }
    text.push('\n');
    text.push_str(&bounded_names_line("untracked", result.untracked()));
    text.push_str(&bounded_names_line("conflicted", result.conflicts()));
    let mut current: Option<&std::path::PathBuf> = None;
    for hunk in result.selected_hunks() {
        if current != Some(hunk.path()) {
            text.push_str(&format!("file: {:?}\n", hunk.path()));
            current = Some(hunk.path());
        }
        match std::str::from_utf8(hunk.patch()) {
            Ok(patch) => text.push_str(patch),
            Err(_) => text.push_str(&format!("raw_patch_hex: {:02x?}\n", hunk.patch())),
        }
    }
    match continuation {
        DiffContinuationNote::None => {}
        DiffContinuationNote::Inspect(reference) => {
            text.push_str(&format!(
                "hunks: {} more (ide.inspect {reference})\n",
                result.overflow_hunks()
            ));
        }
        DiffContinuationNote::Recapture => {
            if result.overflow_hunks() > 0 {
                text.push_str(&format!(
                    "hunks: {} more; recapture with ide.diff\n",
                    result.overflow_hunks()
                ));
            }
        }
    }
    text
}

impl Worker<'_> {
    /// Captures the activation baseline through fixed Git metadata commands and durable Workspace storage.
    ///
    /// The result remains partial because v0.1 cannot prove an atomic Git/source window. Any command,
    /// reap, authority, or storage failure is returned so activation can report unknown coverage
    /// without claiming that the already-committed authority grant failed.
    pub(super) async fn capture_activation_baseline(
        &mut self,
        job: &mut Job,
        authority: &AuthorityStamp,
        activation_operation: &str,
    ) -> Result<BaselineContext, FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let mut git = Vec::with_capacity(3);
        for query in [
            GitReadQuery::HeadTree,
            GitReadQuery::UntrackedPaths,
            GitReadQuery::HeadIdentity,
        ] {
            let intent = GitReadIntent::new(authority, job.target.git.path.clone(), query)
                .map_err(|_| FailureCode::UnsupportedGit)?;
            let request = self
                .execution_request(
                    job,
                    authority,
                    intent
                        .controlled_command()
                        .map_err(|_| FailureCode::UnsupportedGit)?,
                    &job.target.git,
                )
                .await?;
            let active = self.shared.active(&binding)?;
            let lease = self.admit(&binding)?;
            // Same Workspace evidence boundary as the diff capture above: the baseline's fixed
            // whole-tree `ls-tree` listing truncates at the launcher's general `output_bytes`
            // budget on real repositories, which would report unknown coverage on every clean tree.
            let mut child = match OwnedChild::spawn_captured(
                &request,
                lease,
                Some(active),
                crate::workspace::git::MAX_GIT_STDOUT_BYTES,
            ) {
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
            let completed = match if interrupted {
                child
                    .cancel_and_reap(Duration::from_millis(100), Duration::from_millis(500))
                    .await
            } else {
                child
                    .reap(Duration::from_millis(500), Duration::from_millis(100))
                    .await
            } {
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
            let evidence = completed.evidence;
            git.push(
                RawGitEvidence::new(
                    format!("baseline-{activation_operation}-{query:?}"),
                    intent.scope().clone(),
                    query,
                    evidence.stdout().bytes.clone(),
                    evidence.stderr().bytes.clone(),
                    evidence.status().code(),
                    evidence.stdout().truncated || !evidence.stdout().complete,
                    evidence.stderr().truncated || !evidence.stderr().complete,
                )
                .map_err(|_| FailureCode::SourceUnavailable)?,
            );
        }
        let paths = self
            .registered
            .get(&binding)
            .map_or_else(Vec::new, |paths| paths.iter().cloned().collect());
        self.workspace
            .capture_baseline(
                OperationId::new(format!("baseline-{activation_operation}"))
                    .map_err(|_| FailureCode::Internal)?,
                authority,
                &self.shared.active(&binding)?,
                git,
                paths,
            )
            .await
            .map_err(|_| FailureCode::SourceUnavailable)
    }

    /// Combines startup-verified executable selection with current durable authority and sandbox state before spawn.
    pub(super) async fn execution_request(
        &self,
        job: &dyn crate::intelligence::server::ProviderJob,
        authority: &AuthorityStamp,
        command: ControlledCommand,
        program: &crate::assistance::launcher::AcceptedExecutable,
    ) -> Result<ValidatedExecutionRequest, FailureCode> {
        let binding = job.binding();
        let active = self.shared.active(binding)?;
        self.workspace
            .authorize(authority, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        if let Some(code) = request_wait_failure(
            job.cancelled(),
            tokio::time::Instant::now() >= job.deadline(),
        ) {
            return Err(code);
        }
        let invocation = ValidatedHostInvocation::from_active_use(self.shared.active(binding)?);
        let authority = WorkspaceAuthority::from_workspace_with_git_common_dir(
            authority.worktree().id(),
            authority.worktree().incarnation().to_string(),
            authority.worktree().worktree_path().to_path_buf(),
            authority.worktree().git_common_dir().to_path_buf(),
            authority.epoch(),
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        let policy = LocalExecutionPolicy::new(
            BTreeSet::from([program.path.clone()]),
            crate::execution::MAX_PRODUCT_ARGV_BYTES,
            16,
        )
        .map_err(|_| FailureCode::ExecutionProfile)?;
        ValidatedExecutionRequest::validate(invocation, authority, command, &policy)
            .map_err(|_| FailureCode::ExecutionProfile)
    }

    /// Captures one mode through safe raw Git peers and renders only Changes-owned snapshot evidence.
    pub(super) async fn diff(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        // T36B: the former entry-level whole-tree read gate is replaced by per-path
        // authorization inside the capture — `SnapshotRunner::authorize_read_path` routes
        // every native read (tracked captures, staged mode, consistency rereads, untracked
        // inspection) through `validate_workspace_read` with `ReadScope::Path`, which still
        // performs the binding, catalog, and disabled-host checks per path. A whole-tree gate
        // here would refuse every deny-bearing state before any per-path proof could run,
        // making proven-path diffs on such states unreachable. A capture that reads no path
        // at all discloses no worktree bytes either.
        let mode = match job.parameters["mode"].as_str() {
            Some("head") => DiffMode::Head,
            Some("staged") => DiffMode::Staged,
            Some("unstaged") => DiffMode::Unstaged,
            _ => return Err(FailureCode::Internal),
        };
        let provenance = job.parameters["provenance"].as_bool().unwrap_or(false);
        self.source_sequence = self
            .source_sequence
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        let generation = self.source_sequence;
        let program = job.target.git.path.clone();
        let reference = job.reference.clone();
        let baseline = if mode == DiffMode::Head {
            self.baselines.get(&binding).cloned()
        } else {
            None
        }
        .map_or_else(
            || {
                BaselineContext::new(format!("baseline-{reference}"), BaselineCoverage::Unknown)
                    .map_err(|_| FailureCode::Internal)
            },
            Ok,
        )?;
        let mut runner = ProductSnapshotRunner {
            worker: self,
            job,
            authority: authority.clone(),
            failure: None,
            stage: None,
            detail: None,
        };
        let capture = collect_snapshot(
            &authority,
            &program,
            mode,
            generation,
            &reference,
            baseline,
            &mut runner,
        )
        .await;
        let evidence = match capture {
            Ok(evidence) => evidence,
            // W4: the two-pass exact capture retries once and then fails closed on genuine
            // instability (e.g. a concurrent checkout); a single-pass plain `git diff` trades the
            // exact-capture atomicity guarantee for an answer, always marked as such. Any other
            // capture failure keeps its own explicit, non-degraded error below.
            Err(GitError::UnstableSnapshot) => {
                let scope = GitScope::from_authority(&authority, mode);
                let budget = crate::changes::DiffSelectionBudget::bounded(32, 48 * 1024);
                let plain = plain_diff_fallback(&mut runner, &program, &scope, budget).await;
                let failure = runner.failure.take();
                let stage = runner.stage;
                let detail = runner.detail.clone();
                drop(runner);
                let Some(result) = plain else {
                    job.failure_detail = Some(detail.unwrap_or_else(|| {
                        git_failure_detail(&GitError::UnstableSnapshot, stage, failure.clone())
                    }));
                    return Err(failure.unwrap_or(FailureCode::SourceUnavailable));
                };
                let authority = self.authority(&binding).await?;
                self.shared.active(&binding)?;
                let continuation = if result.overflow_hunks() > 0 {
                    DiffContinuationNote::Recapture
                } else {
                    DiffContinuationNote::None
                };
                let text = render_diff_compact(
                    mode,
                    &result,
                    continuation,
                    Some(
                        "plain git diff; exact capture unavailable: snapshot unstable or a \
                         file changed since it was observed",
                    ),
                );
                let reply = PeerReply::Complete {
                    kind: ResultKind::Diff,
                    text,
                    detail_ref: Some(reference.clone()),
                    truncated: result.overflow_hunks() > 0,
                    continuation: false,
                };
                return Ok((reply, Some(authority), None));
            }
            Err(error) => {
                // T27B: the terminal diff failure carries the closed failing stage, so a
                // sandboxed capture that can never finish is diagnosable from the error log.
                let failure = runner.failure.clone();
                let fallback = match &error {
                    GitError::UnsupportedSnapshotGit => FailureCode::UnsupportedGit,
                    GitError::EvidenceTooLarge => FailureCode::Capacity,
                    _ => FailureCode::SourceUnavailable,
                };
                let detail = runner.detail.clone().unwrap_or_else(|| {
                    git_failure_detail(&error, runner.stage, runner.failure.clone())
                });
                drop(runner);
                job.failure_detail = Some(detail);
                return Err(failure.unwrap_or(fallback));
            }
        };
        let comparison = evidence.comparison().clone();
        let scope = GitScope::from_authority(&authority, mode);
        let budget = crate::changes::DiffSelectionBudget::bounded(32, 48 * 1024);
        let authority = self.authority(&binding).await?;
        if tokio::time::Instant::now() >= job.deadline {
            return Err(FailureCode::SourceUnavailable);
        }
        self.shared.active(&binding)?;
        // The initial page goes through the same whole-page fitting path as every later expansion,
        // so it is proven to fit the serialized envelope here instead of being cut mid-hunk later
        // by the generic `PeerReply::encode` shrink.
        let (result, reply) = fit_diff_page(
            mode,
            authority.epoch(),
            &reference,
            budget.max_hunks,
            true,
            provenance,
            |max_hunks| {
                crate::changes::compose_diff(
                    &scope,
                    &comparison,
                    evidence.clone(),
                    crate::changes::DiffSelectionBudget::bounded(max_hunks, budget.max_bytes),
                )
            },
        )?;
        // T36B: retain the bounded provenance of every path this diff represents — each
        // delivered path plus its rename source — independently of the disposable pagination
        // state, so cached delivery must prove each under the live profile before it hands
        // back any composed page. The collector already bounded the path count and bytes.
        // T36B-r: rendered pages also name untracked and conflict paths, so those names are
        // provenance too — a name denied after capture refuses cached delivery.
        let represented_paths: BTreeSet<PathBuf> = evidence
            .paths()
            .iter()
            .flat_map(|path| {
                let mut represented = vec![path.status().path().to_path_buf()];
                represented.extend(path.status().original_path().map(Path::to_path_buf));
                represented
            })
            .chain(
                evidence
                    .status()
                    .untracked()
                    .iter()
                    .map(|entry| entry.path().to_path_buf()),
            )
            .chain(
                evidence
                    .status()
                    .conflicts()
                    .iter()
                    .map(|entry| entry.path().to_path_buf()),
            )
            .collect();
        self.shared
            .set_diff_provenance(&job.reference, represented_paths);
        let diff_page = result.detail_cursor().map(|cursor| DiffPageState {
            scope: scope.clone(),
            comparison: comparison.clone(),
            evidence,
            budget,
            cursor: cursor.clone(),
            mode,
            provenance,
        });
        // A page that reports further hunks must be resumable. If the aggregate retention ceiling
        // refuses to hold that continuation, the omitted hunks stay named but only recapture — not
        // `ide.inspect` — reaches them: an honest smaller answer beats erroring the whole call.
        let continues = diff_page.is_some();
        if self.shared.set_diff_page(&job.reference, diff_page) || !continues {
            return Ok((reply, Some(authority), None));
        }
        let text = if provenance {
            render_diff_provenance(mode, &result, authority.epoch(), false)
        } else {
            render_diff_compact(mode, &result, DiffContinuationNote::Recapture, None)
        };
        let reply = PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            detail_ref: Some(reference),
            truncated: true,
            continuation: false,
        };
        Ok((reply, Some(authority), None))
    }
}

/// Maps one collector failure onto its closed failing-stage tag (T27B).
#[cfg(test)]
mod diff_failure_detail_tests {
    use super::git_failure_detail;
    use crate::workspace::git::GitError;

    /// Collector errors map to fixed stage tags only: never repository paths or child output.
    #[test]
    fn collector_errors_name_the_failing_stage() {
        assert_eq!(
            git_failure_detail(&GitError::SnapshotIo, Some("cat-file"), None),
            "diff:scratch"
        );
        assert_eq!(
            git_failure_detail(&GitError::EvidenceTooLarge, None, None),
            "diff:too_large"
        );
        assert_eq!(
            git_failure_detail(&GitError::UnstableSnapshot, Some("ls-tree"), None),
            "diff:unstable"
        );
        assert_eq!(
            git_failure_detail(&GitError::ObjectHashMismatch, Some("hash-object"), None),
            "diff:hash_mismatch"
        );
        assert_eq!(
            git_failure_detail(&GitError::UnbornHead, None, None),
            "diff:unborn_head"
        );
        assert_eq!(
            git_failure_detail(&GitError::UnsupportedSnapshotGit, None, None),
            "diff:unsupported_git"
        );
        assert_eq!(
            git_failure_detail(&GitError::UnsupportedSnapshot, None, None),
            "diff:unsupported_entry"
        );
        // Rejected child evidence on the last-run intent names that exact stage.
        assert_eq!(
            git_failure_detail(&GitError::IncompleteIdentity, Some("cat-file"), None),
            "diff:child_exit:cat-file"
        );
        assert_eq!(
            git_failure_detail(&GitError::InvalidPorcelain, None, None),
            "diff:child_exit:unknown"
        );
    }

    /// Runner-level codes that reached the collector keep their closed tags.
    #[test]
    fn runner_codes_keep_their_closed_tags() {
        use super::FailureCode;
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("diff-no-index"),
                Some(FailureCode::Deadline)
            ),
            "diff:deadline"
        );
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("cat-file"),
                Some(FailureCode::Capacity)
            ),
            "diff:capacity"
        );
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("rev-parse"),
                Some(FailureCode::Cancelled)
            ),
            "diff:cancelled"
        );
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("cat-file"),
                Some(FailureCode::ExecutionProfile)
            ),
            "diff:execution_profile"
        );
    }

    /// At the execution-request boundary, deadline expiry between children yields `deadline`
    /// (surfacing as `diff:deadline`) while a real cancellation stays `cancelled` (T27B).
    #[test]
    fn request_boundary_separates_deadline_expiry_from_cancellation() {
        use super::{FailureCode, request_wait_failure};
        assert_eq!(request_wait_failure(false, false), None);
        // Expiry with the cancellation flag still false is a deadline, not a cancellation.
        assert_eq!(
            request_wait_failure(false, true),
            Some(FailureCode::Deadline)
        );
        assert_eq!(
            request_wait_failure(true, false),
            Some(FailureCode::Cancelled)
        );
        assert_eq!(
            request_wait_failure(true, true),
            Some(FailureCode::Cancelled)
        );
        // Both boundary codes keep their distinct fixed detail tags end to end.
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("rev-parse"),
                request_wait_failure(false, true)
            ),
            "diff:deadline"
        );
        assert_eq!(
            git_failure_detail(
                &GitError::IncompleteIdentity,
                Some("rev-parse"),
                request_wait_failure(true, false)
            ),
            "diff:cancelled"
        );
    }
}
