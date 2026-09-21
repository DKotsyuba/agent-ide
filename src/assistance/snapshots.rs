//! Execution adapter for Workspace's filter-free snapshot collector; no Git syntax is interpreted here.

use super::*;
use crate::{
    assistance::content,
    execution::{
        CapturedProcessEvidence, ControlledCommand, ControlledTrampoline, LocalExecutionPolicy,
        OwnedChild, ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
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
}
impl SnapshotRunner for ProductSnapshotRunner<'_, '_> {
    /// Executes only the peer-owned intent, settles direct-child proof, then returns immutable data.
    async fn run(&mut self, intent: SnapshotIntent) -> Result<CapturedProcessEvidence, GitError> {
        match self.run_owned(intent).await {
            Ok(evidence) => Ok(evidence),
            Err(code) => {
                self.failure = Some(code);
                Err(GitError::IncompleteIdentity)
            }
        }
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
    /// Keeps private snapshot files alive through wait/cancellation/reap; uncertain reaps retain the intent.
    async fn run_owned(
        &mut self,
        intent: SnapshotIntent,
    ) -> Result<CapturedProcessEvidence, FailureCode> {
        let binding = self.job.invocation.binding_ref().clone();
        if intent.scope().worktree() != self.authority.worktree()
            || intent.scope().authority_epoch() != self.authority.epoch()
        {
            return Err(FailureCode::WorkspaceAuthority);
        }
        let request = self
            .worker
            .execution_request(
                self.job,
                &self.authority,
                intent
                    .command()
                    .map_err(|_| FailureCode::SourceUnavailable)?,
                &self.job.target.git,
            )
            .await?;
        let active = self.worker.shared.active(&binding)?;
        let lease = self.worker.admit(&binding)?;
        let mut child = match OwnedChild::spawn_captured(
            &request,
            lease,
            Some(active),
            &self.job.target.codex.path,
            self.worker.shared.launcher.limits.output_bytes,
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
            let Some(identity) = child.take_process_identity() else {
                self.worker.uncertain.insert(binding);
                self.worker.uncertain_snapshots.push(intent);
                return Err(FailureCode::Internal);
            };
            if intent.bind_process(identity).is_err() {
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
                self.worker.uncertain.insert(binding);
                self.worker.uncertain_snapshots.push(intent);
                return Err(FailureCode::Deadline);
            }
        };
        self.worker
            .admission()
            .release_reaped(completed.settlement)
            .map_err(|_| FailureCode::Internal)?;
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
        intent
            .acknowledge_reap(&completed.evidence)
            .map_err(|_| FailureCode::Internal)?;
        self.worker.shared.active(&binding)?;
        if interrupted {
            return Err(if *self.job.cancel.borrow() {
                FailureCode::Cancelled
            } else {
                FailureCode::Deadline
            });
        }
        Ok(completed.evidence)
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
    /// Builds the next retained page state, or `None` once selection no longer overflows.
    pub(super) fn advance(&self, result: &crate::changes::DiffResult) -> Option<Self> {
        result.detail_cursor().map(|cursor| Self {
            scope: self.scope.clone(),
            comparison: self.comparison.clone(),
            evidence: self.evidence.clone(),
            budget: self.budget,
            cursor: cursor.clone(),
            mode: self.mode,
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
/// envelope predicate [`content::render`] uses for the complete MCP result the host receives. Using
/// anything narrower here — such as the raw serialized [`PeerReply`] with an approximate fixed
/// reserve — could accept a page that the facade then has to cut after its cursor advanced.
///
/// * `mode` — compare mode rendered into the page text.
/// * `authority_epoch` — current durable epoch rendered as provenance.
/// * `reference` — same-binding detail handle echoed as `detail_ref`.
/// * `max_hunks` — largest count to attempt; halved on each retry and clamped to at least one.
/// * `retain_continuation` — whether this caller will retain the accepted cursor for later
///   `ide.inspect`; helper results pass `false` because their settled ticket has no page state.
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
        let text = render_diff_text(
            mode,
            &candidate,
            authority_epoch,
            retain_continuation && candidate.detail_cursor().is_some(),
        );
        let reply = PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            detail_ref: Some(reference.to_owned()),
            truncated: candidate.truncated_output()
                || candidate.overflow_hunks() > 0
                || candidate.overflow_bytes() > 0,
            continuation: retain_continuation && candidate.detail_cursor().is_some(),
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
pub(crate) fn render_diff_text(
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

/// Flips the first (header) `more_available: false` of a rendered Diff text to `true`, for a text
/// composed without a pager that the daemon then pages (T16B). A no-op when it is already `true`.
pub(crate) fn mark_more_available(text: &str) -> String {
    text.replacen("more_available: false\n", "more_available: true\n", 1)
}

impl Worker<'_> {
    /// Persists the fixed baseline reads a settled Start helper collected under inherited sandbox.
    ///
    /// The daemon performs no Git or source read: it checks the exact closed query order, rebuilds
    /// scoped `RawGitEvidence`, reauthorizes the durable grant, and lets Workspace commit the same
    /// partial/unverified v0.1 baseline shape as the managed route.
    pub(super) async fn capture_claude_baseline(
        &mut self,
        binding: &BindingRef,
        authority: &AuthorityStamp,
        activation_operation: &str,
        frames: &[crate::assistance::claude_worker::HelperBaselineFrame],
    ) -> Result<BaselineContext, FailureCode> {
        use crate::assistance::claude_worker::HelperBaselineQuery;
        let expected = [
            (HelperBaselineQuery::HeadTree, GitReadQuery::HeadTree),
            (
                HelperBaselineQuery::UntrackedPaths,
                GitReadQuery::UntrackedPaths,
            ),
            (
                HelperBaselineQuery::HeadIdentity,
                GitReadQuery::HeadIdentity,
            ),
        ];
        if frames.len() != expected.len() {
            return Err(FailureCode::SourceUnavailable);
        }
        let scope = GitScope::from_authority(authority, DiffMode::Head);
        let git = frames
            .iter()
            .zip(expected)
            .map(|(frame, (reported, query))| {
                if frame.query != reported {
                    return Err(FailureCode::SourceUnavailable);
                }
                RawGitEvidence::new(
                    format!("baseline-{activation_operation}-{query:?}"),
                    scope.clone(),
                    query,
                    frame.stdout.clone(),
                    frame.stderr.clone(),
                    frame.exit_code,
                    frame.truncated,
                    frame.truncated,
                )
                .map_err(|_| FailureCode::SourceUnavailable)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.workspace
            .authorize(authority, &self.shared.active(binding)?)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        self.workspace
            .capture_baseline(
                OperationId::new(format!("baseline-{activation_operation}"))
                    .map_err(|_| FailureCode::Internal)?,
                authority,
                &self.shared.active(binding)?,
                git,
                Vec::new(),
            )
            .await
            .map_err(|_| FailureCode::SourceUnavailable)
    }

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
            let mut child = match OwnedChild::spawn_captured(
                &request,
                lease,
                Some(active),
                &job.target.codex.path,
                self.shared.launcher.limits.output_bytes,
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
        job: &Job,
        authority: &AuthorityStamp,
        command: ControlledCommand,
        program: &crate::assistance::launcher::AcceptedExecutable,
    ) -> Result<ValidatedExecutionRequest, FailureCode> {
        let binding = job.invocation.binding_ref();
        let active = self.shared.active(binding)?;
        self.workspace
            .authorize(authority, &active)
            .await
            .map_err(|_| FailureCode::WorkspaceAuthority)?;
        if tokio::time::Instant::now() >= job.deadline || *job.cancel.borrow() {
            return Err(FailureCode::Cancelled);
        }
        let invocation = ValidatedHostInvocation::from_active_observation(
            self.shared.active(binding)?,
            job.observed.clone().ok_or(FailureCode::SandboxState)?,
        )
        .map_err(|_| FailureCode::SandboxState)?;
        let authority = WorkspaceAuthority::from_workspace(
            authority.worktree().id(),
            authority.worktree().incarnation().to_string(),
            authority.worktree().worktree_path().to_path_buf(),
            authority.epoch(),
        )
        .map_err(|_| FailureCode::WorkspaceAuthority)?;
        // An operator-declared `env` trampoline is the only way a command may run in this
        // worktree while the managed host still reports its own inherited `sandboxCwd`; its
        // absence simply leaves that case unavailable. The seal is pinned to the digest the
        // operator declared and startup verified, so a per-request build cannot re-baseline an
        // executable that changed after the daemon became ready.
        let trampoline = job
            .target
            .cwd_trampoline
            .as_ref()
            .map(|accepted| ControlledTrampoline::accept(accepted.path.clone(), &accepted.blake3))
            .transpose()
            .map_err(|_| FailureCode::ExecutionProfile)?;
        let programs = BTreeSet::from([program.path.clone()]);
        let policy = match trampoline {
            Some(trampoline) => LocalExecutionPolicy::with_env_trampoline(
                programs,
                64 * 1024,
                16,
                job.target.allow_disabled_host,
                trampoline,
            ),
            None => {
                LocalExecutionPolicy::new(programs, 64 * 1024, 16, job.target.allow_disabled_host)
            }
        }
        .map_err(|_| FailureCode::ExecutionProfile)?;
        ValidatedExecutionRequest::validate(
            invocation,
            authority,
            command,
            &policy,
            &job.target.catalog,
        )
        .map_err(|_| FailureCode::ExecutionProfile)
    }

    /// Captures one mode through safe raw Git peers and renders only Changes-owned snapshot evidence.
    pub(super) async fn diff(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        validate_read_scope(
            &self.shared,
            &binding,
            job.observed.as_ref().ok_or(FailureCode::SandboxState)?,
            &job.target,
            &authority,
            errorlog_method(job.tool),
        )?;
        let mode = match job.parameters["mode"].as_str() {
            Some("head") => DiffMode::Head,
            Some("staged") => DiffMode::Staged,
            Some("unstaged") => DiffMode::Unstaged,
            _ => return Err(FailureCode::Internal),
        };
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
            Err(error) => {
                return Err(runner.failure.unwrap_or(match error {
                    GitError::UnsupportedSnapshotGit => FailureCode::UnsupportedGit,
                    _ => FailureCode::SourceUnavailable,
                }));
            }
        };
        let comparison = evidence.comparison().clone();
        let scope = GitScope::from_authority(&authority, mode);
        let budget = crate::changes::DiffSelectionBudget::bounded(32, 48 * 1024);
        let authority = self.authority(&binding).await?;
        let epoch = self
            .shared
            .ledger
            .lock()
            .map_err(|_| FailureCode::Internal)?
            .native_epoch
            .get(&binding)
            .copied()
            .unwrap_or(0);
        if epoch != job.native_epoch || tokio::time::Instant::now() >= job.deadline {
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
            |max_hunks| {
                crate::changes::compose_diff(
                    &scope,
                    &comparison,
                    evidence.clone(),
                    crate::changes::DiffSelectionBudget::bounded(max_hunks, budget.max_bytes),
                )
            },
        )?;
        let diff_page = result.detail_cursor().map(|cursor| DiffPageState {
            scope: scope.clone(),
            comparison: comparison.clone(),
            evidence,
            budget,
            cursor: cursor.clone(),
            mode,
        });
        // A page that reports further hunks must be resumable. If the aggregate retention ceiling
        // refuses to hold that continuation, the omitted hunks are unreachable, so this fails with
        // an explicit finite budget error rather than delivering a page that claims completeness.
        let continues = diff_page.is_some();
        if !self.shared.set_diff_page(&job.reference, diff_page) && continues {
            return Err(FailureCode::Capacity);
        }
        Ok((reply, Some(authority), None))
    }
}
