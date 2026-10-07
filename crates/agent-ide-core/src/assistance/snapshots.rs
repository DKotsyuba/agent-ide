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
            snapshot::{
                SnapshotIntent, SnapshotRunner, collect_snapshot, confine_plain_diff_paths,
            },
        },
    },
};
use std::collections::BTreeSet;

/// Renders the capture time on page one and the bounded rechecks and recovery on later pages.
///
/// Later non-staged pages recheck tracked worktree bytes only. Untracked additions remain a
/// captured snapshot; staged pages use immutable blobs and never recheck worktree contents.
fn current_tree_line(mode: DiffMode, later_page: bool) -> String {
    let text = if !later_page {
        "current_tree: captured just now"
    } else if mode == DiffMode::Staged {
        "current_tree: staged contents are not rechecked; untracked paths are name-only; commits, staging, and untracked names since the first page are not — if you committed, staged, or added an untracked file since, call ide.diff again"
    } else {
        "current_tree: tracked file contents rechecked; untracked contents remain a captured snapshot; commits, staging, and untracked names since the first page are not — if you committed, staged, or added an untracked file since, call ide.diff again"
    };
    text.to_owned()
}

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
    /// Selects validated literal paths before reads and budgets; omitted paths include the tree.
    fn includes_path(&self, path: &Path) -> bool {
        self.job
            .parameters
            .get("paths")
            .and_then(Value::as_array)
            .is_none_or(|paths| {
                paths
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|selected| path.starts_with(selected))
            })
    }

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
    /// Snapshot evidence or the bounded plain patch and source bytes needed to resume task pages.
    evidence: DiffPageEvidence,
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

/// Retained source used by the common inspector for snapshot or plain-patch pagination.
#[derive(Clone)]
enum DiffPageEvidence {
    /// Exact typed evidence from the standard snapshot comparisons.
    Snapshot {
        /// Comparison identities bound to the captured snapshot.
        comparison: Box<crate::workspace::git::GitComparison>,
        /// Complete per-path raw Git evidence retained for selection.
        snapshot: Box<crate::workspace::git::snapshot::GitSnapshot>,
    },
    /// Task or degraded plain patch, frozen untracked additions, and tracked-source fingerprints.
    Plain {
        /// Bounded controlled plain/task patch plus best-effort captured untracked additions.
        stdout: Vec<u8>,
        /// Confined untracked names captured with Git's standard ignore rules.
        untracked: Vec<crate::workspace::git::PathStatus>,
        /// Bounded reasons untracked entries were not captured as text.
        notes: Vec<(PathBuf, String)>,
        /// Number of untracked paths represented in stdout, excluded from tracked inventory.
        patch_paths: usize,
        /// Fixed capture limitation carried through every degraded fallback page.
        degraded: Option<&'static str>,
        /// Non-staged tracked fingerprints only; absent for staged mode, None for deleted paths.
        worktree_sources: Vec<(PathBuf, Option<crate::workspace::observation::SourceBytes>)>,
        /// Owner operation reference retained with the patch.
        operation: String,
        /// Capture generation retained with the patch.
        generation: u64,
    },
}

impl DiffPageState {
    /// Expands the next bounded page with a reduced *hunk count* while preserving the byte ceiling
    /// this comparison was captured with, so a caller can shrink the page until it proves to fit
    /// the actual serialized reply envelope.
    ///
    /// Selection resumes the cursor's hunk and line offset under the original byte target.
    /// The fitter first reduces whole-hunk count, then cuts exact line parts or emits a read
    /// notice for a lone oversized line. No unseen hunk or line remainder is silently skipped.
    pub(super) fn expand_with_max_hunks(
        &self,
        expected_scope: &GitScope,
        max_hunks: usize,
    ) -> crate::changes::DiffResult {
        let budget = crate::changes::DiffSelectionBudget::bounded(max_hunks, self.budget.max_bytes);
        match &self.evidence {
            DiffPageEvidence::Snapshot {
                comparison,
                snapshot,
            } => crate::changes::expand_diff(
                expected_scope,
                comparison.as_ref(),
                snapshot.as_ref().clone(),
                &self.cursor,
                budget,
            ),
            DiffPageEvidence::Plain {
                stdout,
                untracked,
                notes,
                patch_paths,
                degraded,
                operation,
                generation,
                ..
            } => crate::changes::compose_plain_diff_page(
                expected_scope,
                stdout,
                Some(&self.cursor),
                operation,
                *generation,
                budget,
                |paths| {
                    self.mode == DiffMode::Staged
                        || confine_plain_diff_paths(
                            expected_scope,
                            &paths
                                .iter()
                                .filter(|path| {
                                    !untracked.iter().any(|entry| entry.path() == path.as_path())
                                })
                                .cloned()
                                .collect::<Vec<_>>(),
                        )
                        .is_ok()
                },
            )
            .map(|result| {
                result
                    .with_untracked(untracked.clone())
                    .with_untracked_notes(notes.clone(), *patch_paths)
                    .with_degraded(*degraded)
            })
            .unwrap_or_else(|| crate::changes::DiffResult::unavailable(expected_scope)),
        }
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
            evidence: self.evidence.clone(),
            budget: self.budget,
            cursor: cursor.clone(),
            mode: self.mode,
            provenance: self.provenance,
        })
    }
    /// Re-verifies every retained tracked path's working-tree bytes against the current worktree
    /// using Workspace's no-follow reader. Staged blobs and captured untracked additions are
    /// immutable reply snapshots; neither depends on current worktree contents.
    pub(super) fn working_tree_bytes_unchanged(
        &self,
        worktree: &crate::workspace::authority::WorktreeRef,
    ) -> bool {
        use crate::workspace::observation::{
            ObservationError, SourceReadLimits, read_authorized_source,
        };
        let limits = SourceReadLimits::new(
            4096,
            crate::workspace::git::snapshot::MAX_SNAPSHOT_BLOB_BYTES,
        )
        .expect("fixed source limits");
        if self.mode == DiffMode::Staged {
            return true;
        }
        match &self.evidence {
            DiffPageEvidence::Snapshot { snapshot, .. } => {
                for path in snapshot.paths() {
                    let Some(source) = path.source() else {
                        continue;
                    };
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
            }
            DiffPageEvidence::Plain {
                worktree_sources, ..
            } => {
                for (path, original) in worktree_sources {
                    let matches = match read_authorized_source(worktree, path, limits) {
                        Ok(read) => original.as_ref() == Some(read.bytes()),
                        Err(ObservationError::Missing) => original.is_none(),
                        Err(_) => false,
                    };
                    if !matches {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// Runs one single-pass plain `git diff` directly in the worktree as a degraded fallback, used
/// only when the exact two-pass capture proved unstable. Validates composition under `budget`
/// and returns bounded raw bytes for the shared pager. `None` on
/// any failure — spawn, wait, a rejected exit, output `compose_plain_diff` cannot attribute
/// exactly, or any named path failing the exact capture's per-path confinement
/// (`confine_plain_diff_paths`) — so the caller reports the original capture failure instead of a
/// confusing second one, an understated page, or a page for a path the exact capture refuses.
async fn plain_diff_fallback(
    runner: &mut ProductSnapshotRunner<'_, '_>,
    program: &Path,
    scope: &GitScope,
    budget: crate::changes::DiffSelectionBudget,
) -> Option<Vec<u8>> {
    let selected: Vec<PathBuf> = runner.job.parameters["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect();
    let intent = SnapshotIntent::plain_diff_paths(scope.clone(), program, &selected).ok()?;
    let evidence = runner.run_owned(intent.clone()).await.ok()?;
    let stdout = intent.accept(evidence).ok()?;
    crate::changes::compose_plain_diff(scope, &stdout, budget, |paths| {
        confine_plain_diff_paths(scope, paths).is_ok()
    })
    .map(|_| stdout)
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

/// Composes a whole-hunk page or exact line part under the serialized reply envelope.
///
/// This is the single fitting path shared by the initial composition in [`Worker::diff`] and by
/// every later expansion in `serve_inspection`, so both obey the same rule: shrink the page by
/// selecting fewer whole hunks first, then splitting a lone oversized hunk into exact
/// line-bounded parts without lowering the captured byte ceiling. `compose` receives a hunk
/// count and must return that selection under the originally captured byte budget.
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
/// * `provenance` — whether to render today's exact hash-bearing header (`render_diff_provenance`)
///   instead of the compact §2.7 default (`render_diff_compact`).
/// * `compose` — pure selection callback; it must not mutate retained state, because it is called
///   repeatedly and only the returned result of the accepted attempt is retained.
///
/// The accepted cursor is retained for a later `ide.inspect` when one exists.
///
/// Returns the accepted selection together with the exact [`PeerReply`] rendered from it; the
/// caller retains continuation state derived from that same selection.
///
/// # Errors
///
/// * [`FailureCode::SourceUnavailable`] when a candidate selection is structurally unavailable or
///   failed, which no smaller page can repair.
/// * [`FailureCode::Capacity`] when inventory or the bounded recovery notice cannot fit the
///   envelope. Oversized raw lines are delivered as notices whose cursor advances past the line.
pub(crate) fn fit_diff_page(
    mode: DiffMode,
    authority_epoch: u64,
    reference: &str,
    max_hunks: usize,
    provenance: bool,
    later_page: bool,
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
        let reply = diff_page_reply(
            mode,
            authority_epoch,
            reference,
            provenance,
            later_page,
            &candidate,
            true,
        );
        // The daemon composing this page has no host-kind signal of its own (T14B): only the MCP
        // facade, at final per-call render time, knows whether the caller is Claude or Codex. This
        // stays conservative for both hosts, sized to fit even alongside the structured JSON copy.
        if diff_page_fits(
            mode,
            authority_epoch,
            reference,
            provenance,
            later_page,
            &candidate,
        ) {
            return Ok((candidate, reply));
        }
        if max_hunks == 1 {
            let Some(hunk) = candidate.selected_hunks().first() else {
                return Err(FailureCode::Capacity);
            };
            let mut boundaries: Vec<usize> = hunk
                .patch()
                .iter()
                .enumerate()
                .filter_map(|(index, byte)| {
                    (*byte == b'\n' && index + 1 < hunk.patch().len()).then_some(index + 1)
                })
                .collect();
            // Binary search counts complete lines, while fits measures escaped text in both carriers.
            let mut best = None;
            while !boundaries.is_empty() {
                let middle = boundaries.len() / 2;
                let part = candidate.split_first_hunk(boundaries[middle]);
                let reply = diff_page_reply(
                    mode,
                    authority_epoch,
                    reference,
                    provenance,
                    later_page,
                    &part,
                    true,
                );
                if diff_page_fits(
                    mode,
                    authority_epoch,
                    reference,
                    provenance,
                    later_page,
                    &part,
                ) {
                    best = Some((part, reply));
                    boundaries.drain(..=middle);
                } else {
                    boundaries.truncate(middle);
                }
            }
            if let Some(best) = best {
                return Ok(best);
            }
            if let Some(notice) = candidate.skip_first_line() {
                let reply = diff_page_reply(
                    mode,
                    authority_epoch,
                    reference,
                    provenance,
                    later_page,
                    &notice,
                    true,
                );
                if diff_page_fits(
                    mode,
                    authority_epoch,
                    reference,
                    provenance,
                    later_page,
                    &notice,
                ) {
                    return Ok((notice, reply));
                }
            }
            return Err(FailureCode::Capacity);
        }
        max_hunks = (max_hunks / 2).max(1);
    }
}

/// Renders exactly one selection or line part with the same continuation and envelope fields.
/// Metadata and hunk bytes are measured together by the fitter; this helper has no side effects.
fn diff_page_reply(
    mode: DiffMode,
    authority_epoch: u64,
    reference: &str,
    provenance: bool,
    later_page: bool,
    candidate: &crate::changes::DiffResult,
    retained: bool,
) -> PeerReply {
    let more_available = retained && candidate.detail_cursor().is_some();
    let mut text = if provenance {
        render_diff_provenance(mode, candidate, authority_epoch, more_available, later_page)
    } else {
        render_diff_compact(
            mode,
            candidate,
            if more_available {
                DiffContinuationNote::Inspect(reference)
            } else if candidate.detail_cursor().is_some() {
                DiffContinuationNote::Recapture
            } else {
                DiffContinuationNote::None
            },
            None,
        )
    };
    if provenance && !retained && candidate.detail_cursor().is_some() {
        text.push_str(&recapture_trailer(candidate));
    }
    PeerReply::Complete {
        kind: ResultKind::Diff,
        text,
        detail_ref: Some(reference.to_owned()),
        truncated: candidate.truncated_output()
            || candidate.overflow_hunks() > 0
            || candidate.overflow_bytes() > 0
            || candidate
                .selected_hunks()
                .iter()
                .any(|hunk| hunk.line_notice().is_some()),
        continuation: more_available,
    }
}

/// Measures both retained and retention-full replies before accepting any exact line part.
/// This reserves the actual longer trailer and prevents final MCP rendering from shrinking bytes.
fn diff_page_fits(
    mode: DiffMode,
    epoch: u64,
    reference: &str,
    provenance: bool,
    later: bool,
    candidate: &crate::changes::DiffResult,
) -> bool {
    [true, false].into_iter().all(|retained| {
        content::fits(
            &diff_page_reply(
                mode, epoch, reference, provenance, later, candidate, retained,
            ),
            content::Envelope::WithStructured,
        )
    })
}

/// Names omitted bytes whose cursor could not be retained; used for compact and provenance pages.
fn recapture_trailer(result: &crate::changes::DiffResult) -> String {
    format!(
        "hunks: {} more; diff continuation store full; finish other diff pages or recapture with ide.diff paths to retain less evidence\n",
        result.overflow_hunks()
    )
}

/// Names the reply resource that cannot fit: path inventory or a single exact diff line.
/// The inventory is measured in the same host envelope, so its overflow never blames a source line.
pub(super) fn diff_capacity_detail(
    mode: DiffMode,
    authority_epoch: u64,
    reference: &str,
    provenance: bool,
    later_page: bool,
    candidate: &crate::changes::DiffResult,
) -> String {
    let metadata = diff_page_reply(
        mode,
        authority_epoch,
        reference,
        provenance,
        later_page,
        &candidate.inventory_only(),
        false,
    );
    if content::fits(&metadata, content::Envelope::WithStructured) {
        candidate.line_capacity_detail()
    } else {
        "diff:reply_inventory".to_owned()
    }
}

/// Renders the exact typed freshness/coverage/provenance/status facts for one Diff page.
/// Provenance always carries the scope/comparison/operation fields required to interpret this
/// page independent of any other request: worktree identity/incarnation, authority epoch,
/// operation reference, capture generation and both raw comparison-side identities.
///
/// Delivery makes no independent freshness claim — a retained Diff result is an immutable captured
/// snapshot, not proof of the repository's state at delivery time — so the former separate
/// `freshness`/`captured_freshness` pair is answered as one `current_tree` line: it says when the
/// result was captured, what a later page rechecks, and what to do after Git state changes. The short
/// `ide.inspect` service deliberately performs no heavyweight Git recapture, so HEAD/index
/// identities, untracked and conflict sets and durable current-observation tokens are never
/// revalidated before a page is handed over. Later pages recheck tracked working-tree bytes except
/// for staged mode (see [`DiffPageState::working_tree_bytes_unchanged`]). Untracked text is a
/// frozen best-effort capture, never a claim about current bytes; staging/HEAD are not rechecked.
fn render_diff_provenance(
    mode: DiffMode,
    result: &crate::changes::DiffResult,
    authority_epoch: u64,
    more_available: bool,
    later_page: bool,
) -> String {
    let provenance = result.provenance();
    let mut text = format!(
        "mode: {:?}\nstate: {:?}\ncoverage: {:?}\n{}\nauthority_epoch: {}\nworktree_id: {}\nworktree_incarnation: {}\noperation_reference: {}\ncapture_generation: {}\ncomparison_left: {}\ncomparison_right: {}\nbaseline_reference: {}\nbaseline_coverage: {:?}\nbaseline_window: {:?}\nbaseline_reason: {}\ntracked: {}; untracked: {}; conflicted: {}\nomitted_hunks: {}; omitted_bytes: {}; more_available: {}\n",
        mode,
        result.state(),
        result.coverage(),
        current_tree_line(mode, later_page),
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
        BASELINE_PARTIAL_REASON,
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
    if let Some(note) = result.degraded() {
        text.push_str(&format!("capture: degraded ({note})\n"));
    }
    text.push_str(&render_inventory(result));
    // Every file's first hunk is preceded by its `file:` line, so no hunk on any page depends on
    // the header's path list for attribution (T16B).
    let mut current: Option<&std::path::PathBuf> = None;
    for hunk in result.selected_hunks() {
        if current != Some(hunk.path()) {
            text.push_str(&format!("file: {:?}\n", hunk.path()));
            current = Some(hunk.path());
        }
        text.push_str(&result.hunk_part_label(hunk, more_available));
        if let Some(notice) = hunk.line_notice() {
            text.push_str(notice);
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

/// Lowercases one compare mode for the compact §2.7 header.
const fn mode_label(mode: DiffMode) -> &'static str {
    match mode {
        DiffMode::Head => "head",
        DiffMode::Staged => "staged",
        DiffMode::Unstaged => "unstaged",
        DiffMode::Task => "task",
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

/// Separates capture inventory from delivered hunks and bounds names and omission notes.
/// Quoted names preserve arbitrary Unix path labels without embedding control characters.
fn render_inventory(result: &crate::changes::DiffResult) -> String {
    let mut text = format!(
        "inventory: {} tracked, {} untracked, {} conflicted; hunks delivered: {}\n",
        result.counts().tracked(),
        result.counts().untracked(),
        result.counts().conflicted(),
        result.selected_hunks().len()
    );
    let names: Vec<String> = result
        .inventory()
        .iter()
        .take(MAX_INLINE_DIFF_NAMES)
        .map(|path| format!("{path:?}"))
        .collect();
    if !names.is_empty() {
        text.push_str(&format!("paths: {}", names.join(", ")));
        let hidden = result.inventory().len().saturating_sub(names.len());
        if hidden > 0 {
            text.push_str(&format!(" (+{hidden} more)"));
        }
        text.push('\n');
    }
    for (path, reason) in result.name_only().iter().take(MAX_INLINE_DIFF_NAMES) {
        text.push_str(&format!("name only: {path:?} ({reason})\n"));
    }
    let hidden = result
        .name_only()
        .len()
        .saturating_sub(MAX_INLINE_DIFF_NAMES);
    if hidden > 0 {
        text.push_str(&format!(
            "name only: +{hidden} more; use paths to inspect the remaining inventory\n"
        ));
    }
    text
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
        result.files(),
        result.additions(),
        result.deletions(),
    );
    if let Some(note) = degraded.or(result.degraded()) {
        text.push_str(&format!(" ({note})"));
    }
    text.push('\n');
    text.push_str(&bounded_names_line("untracked", result.untracked()));
    text.push_str(&bounded_names_line("conflicted", result.conflicts()));
    text.push_str(&render_inventory(result));
    let mut current: Option<&std::path::PathBuf> = None;
    for hunk in result.selected_hunks() {
        if current != Some(hunk.path()) {
            text.push_str(&format!("file: {:?}\n", hunk.path()));
            current = Some(hunk.path());
        }
        text.push_str(&result.hunk_part_label(
            hunk,
            matches!(continuation, DiffContinuationNote::Inspect(_)),
        ));
        if let Some(notice) = hunk.line_notice() {
            text.push_str(notice);
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
                text.push_str(&recapture_trailer(result));
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
            .map_or_else(Vec::new, super::RegisteredPaths::paths);
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

    /// Pages a bounded plain Git capture for task mode or the degraded fallback.
    /// Captures selected untracked text and source fingerprints, retains one immutable cursor,
    /// reauthorizes delivery, and names source/patch/line limits. `degraded` is a fixed producer label.
    async fn plain_diff_reply(
        &mut self,
        job: &mut Job,
        authority: AuthorityStamp,
        mode: DiffMode,
        generation: u64,
        mut stdout: Vec<u8>,
        degraded: Option<&'static str>,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let scope = GitScope::from_authority(&authority, mode);
        let reference = job.reference.clone();
        let program = job.target.git.path.clone();
        let provenance = job.parameters["provenance"].as_bool().unwrap_or(false);
        let mut runner = ProductSnapshotRunner {
            worker: self,
            job,
            authority: authority.clone(),
            failure: None,
            stage: None,
            detail: None,
        };
        let untracked_intent = SnapshotIntent::untracked_paths(&scope, &program)
            .map_err(|_| FailureCode::SourceUnavailable)?;
        let untracked_evidence = runner.run_owned(untracked_intent.clone()).await?;
        let untracked_output = untracked_intent
            .accept(untracked_evidence)
            .map_err(|_| FailureCode::SourceUnavailable)?;
        let untracked = crate::workspace::git::snapshot::parse_untracked_paths_selected(
            &untracked_output,
            |path| runner.includes_path(path),
        )
        .map_err(|_| FailureCode::SourceUnavailable)?;
        let inventory: BTreeSet<PathBuf> = crate::changes::plain_diff_paths(&stdout)
            .ok_or(FailureCode::SourceUnavailable)?
            .into_iter()
            .chain(untracked.iter().map(|path| path.path().to_path_buf()))
            .collect();
        if inventory.len() > crate::workspace::git::snapshot::MAX_SNAPSHOT_PATHS
            || inventory
                .iter()
                .map(|path| path.as_os_str().as_encoded_bytes().len())
                .sum::<usize>()
                > crate::workspace::git::snapshot::MAX_SNAPSHOT_PATH_BYTES
        {
            runner.job.failure_detail = Some("diff:too_large".to_owned());
            return Err(FailureCode::Capacity);
        }
        let tracked_paths =
            crate::changes::plain_diff_paths(&stdout).ok_or(FailureCode::SourceUnavailable)?;
        let limits = crate::workspace::observation::SourceReadLimits::new(
            4096,
            crate::workspace::git::snapshot::MAX_SNAPSHOT_BLOB_BYTES,
        )
        .map_err(|_| FailureCode::SourceUnavailable)?;
        let mut worktree_sources = Vec::with_capacity(tracked_paths.len());
        let mut source_bytes = 0usize;
        let unique_paths: BTreeSet<PathBuf> = tracked_paths.iter().cloned().collect();
        for path in unique_paths
            .into_iter()
            .filter(|_| mode != DiffMode::Staged)
        {
            runner
                .authorize_read_path(&path)
                .await
                .map_err(|_| FailureCode::SourceUnavailable)?;
            let source = match crate::workspace::observation::read_authorized_source(
                authority.worktree(),
                &path,
                limits,
            ) {
                Ok(source) => {
                    source_bytes = source_bytes.saturating_add(source.contents().len());
                    if source_bytes > crate::workspace::git::snapshot::MAX_SNAPSHOT_TOTAL_BYTES {
                        runner.job.failure_detail = Some("diff:too_large".to_owned());
                        return Err(FailureCode::Capacity);
                    }
                    Some(crate::workspace::observation::SourceBytes::from_bytes(
                        source.contents(),
                    ))
                }
                Err(crate::workspace::observation::ObservationError::Missing) => None,
                Err(_) => return Err(FailureCode::SourceUnavailable),
            };
            worktree_sources.push((path, source));
        }
        let mut notes = Vec::new();
        let mut patch_paths = 0usize;

        for path in untracked.iter().filter(|_| mode != DiffMode::Staged) {
            runner
                .authorize_read_path(path.path())
                .await
                .map_err(|_| FailureCode::SourceUnavailable)?;
            let mut addition = crate::workspace::git::snapshot::PathSnapshot::untracked(
                &scope,
                generation,
                path.clone(),
                crate::workspace::git::snapshot::MAX_SNAPSHOT_TOTAL_BYTES
                    .saturating_sub(source_bytes),
                crate::workspace::git::snapshot::MAX_SNAPSHOT_PATCH_BYTES
                    .saturating_sub(stdout.len()),
            )
            .map_err(|_| FailureCode::SourceUnavailable)?;
            if addition.source().is_some() {
                runner
                    .authorize_read_path(path.path())
                    .await
                    .map_err(|_| FailureCode::SourceUnavailable)?;
                addition
                    .verify_untracked(authority.worktree())
                    .map_err(|_| FailureCode::SourceUnavailable)?;
            }
            if let Some(reason) = addition.name_only() {
                notes.push((path.path().to_path_buf(), reason.to_owned()));
            }
            let patch = addition.plain_patch();
            if !patch.is_empty() {
                if stdout.len().saturating_add(patch.len())
                    > crate::workspace::git::snapshot::MAX_SNAPSHOT_PATCH_BYTES
                {
                    notes.push((path.path().to_path_buf(), "total patch limit".to_owned()));
                    continue;
                }
                patch_paths += 1;
                source_bytes += addition
                    .source()
                    .and_then(|source| source.bytes())
                    .map_or(0, |bytes| bytes.length() as usize);
                stdout.extend_from_slice(&patch);
            }
        }
        let budget = crate::changes::DiffSelectionBudget::bounded(32, 48 * 1024);
        let composed = crate::changes::compose_plain_diff_page(
            &scope,
            &stdout,
            None,
            &reference,
            generation,
            budget,
            |paths| {
                mode == DiffMode::Staged
                    || confine_plain_diff_paths(
                        &scope,
                        &paths
                            .iter()
                            .filter(|path| {
                                !untracked.iter().any(|entry| entry.path() == path.as_path())
                            })
                            .cloned()
                            .collect::<Vec<_>>(),
                    )
                    .is_ok()
            },
        )
        .ok_or(FailureCode::SourceUnavailable)?
        .with_untracked(untracked.clone())
        .with_untracked_notes(notes.clone(), patch_paths)
        .with_degraded(degraded);
        drop(runner);
        let (result, reply) = fit_diff_page(
            mode,
            authority.epoch(),
            &reference,
            budget.max_hunks,
            provenance,
            false,
            |max_hunks| {
                crate::changes::compose_plain_diff_page(
                    &scope,
                    &stdout,
                    None,
                    &reference,
                    generation,
                    crate::changes::DiffSelectionBudget::bounded(max_hunks, budget.max_bytes),
                    |paths| {
                        mode == DiffMode::Staged
                            || confine_plain_diff_paths(
                                &scope,
                                &paths
                                    .iter()
                                    .filter(|path| {
                                        !untracked
                                            .iter()
                                            .any(|entry| entry.path() == path.as_path())
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>(),
                            )
                            .is_ok()
                    },
                )
                .map(|result| {
                    result
                        .with_untracked(untracked.clone())
                        .with_untracked_notes(notes.clone(), patch_paths)
                        .with_degraded(degraded)
                })
                .unwrap_or_else(|| crate::changes::DiffResult::unavailable(&scope))
            },
        )
        .map_err(|code| {
            if code == FailureCode::Capacity {
                job.failure_detail = Some(diff_capacity_detail(
                    mode,
                    authority.epoch(),
                    &reference,
                    provenance,
                    false,
                    &composed,
                ));
            }
            code
        })?;
        self.shared
            .set_diff_provenance(&reference, tracked_paths.into_iter().collect());
        let diff_page = result.detail_cursor().map(|cursor| DiffPageState {
            scope: scope.clone(),
            evidence: DiffPageEvidence::Plain {
                stdout,
                untracked,
                notes,
                patch_paths,
                degraded,
                worktree_sources,
                operation: reference.clone(),
                generation,
            },
            budget,
            cursor: cursor.clone(),
            mode,
            provenance,
        });
        let continues = diff_page.is_some();
        let retained = self.shared.set_diff_page(&reference, diff_page);
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        if retained || !continues {
            return Ok((reply, Some(authority), None));
        }
        Ok((
            diff_page_reply(
                mode,
                authority.epoch(),
                &reference,
                provenance,
                false,
                &result,
                false,
            ),
            Some(authority),
            None,
        ))
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
        // A plain directory activated without Git data, so there is no baseline, index or worktree
        // state to diff against; the refusal says so instead of running a Git that must fail.
        if authority.worktree().is_plain_directory() {
            job.failure_detail = Some("diff:not_a_git_repository".to_owned());
            return Err(FailureCode::SourceUnavailable);
        }
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
            Some("task") => DiffMode::Task,
            _ => return Err(FailureCode::Internal),
        };
        let provenance = job.parameters["provenance"].as_bool().unwrap_or(false);
        let selected: Vec<PathBuf> = job.parameters["paths"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
        self.source_sequence = self
            .source_sequence
            .checked_add(1)
            .ok_or(FailureCode::Capacity)?;
        let generation = self.source_sequence;
        let program = job.target.git.path.clone();
        let reference = job.reference.clone();
        let baseline = if matches!(mode, DiffMode::Head | DiffMode::Task) {
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
        if mode == DiffMode::Task {
            let Some(task_head) = baseline.task_head() else {
                job.failure_detail =
                    Some("diff:activation_commit_unknown; use mode: head".to_owned());
                return Err(FailureCode::SourceUnavailable);
            };
            let scope = GitScope::from_authority(&authority, mode);
            let intent = SnapshotIntent::task_diff(scope.clone(), &program, task_head, &selected)
                .map_err(|_| FailureCode::SourceUnavailable)?;
            let mut runner = ProductSnapshotRunner {
                worker: self,
                job,
                authority: authority.clone(),
                failure: None,
                stage: None,
                detail: None,
            };
            let evidence = runner.run_owned(intent.clone()).await?;
            let stdout = intent
                .accept(evidence)
                .map_err(|_| FailureCode::SourceUnavailable)?;
            drop(runner);
            return self
                .plain_diff_reply(job, authority, mode, generation, stdout, None)
                .await;
        }
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
            // exact-capture atomicity guarantee for an answer, always marked as such, and only
            // when every path it names passes the exact capture's per-path confinement. Any other
            // capture failure keeps its own explicit, non-degraded error below.
            Err(GitError::UnstableSnapshot) => {
                let scope = GitScope::from_authority(&authority, mode);
                let budget = crate::changes::DiffSelectionBudget::bounded(32, 48 * 1024);
                let plain = plain_diff_fallback(&mut runner, &program, &scope, budget).await;
                let failure = runner.failure.take();
                let stage = runner.stage;
                let detail = runner.detail.clone();
                drop(runner);
                let Some(stdout) = plain else {
                    job.failure_detail = Some(detail.unwrap_or_else(|| {
                        git_failure_detail(&GitError::UnstableSnapshot, stage, failure.clone())
                    }));
                    return Err(failure.unwrap_or(FailureCode::SourceUnavailable));
                };
                return self.plain_diff_reply(job, authority, mode, generation, stdout, Some("plain git diff; exact capture unavailable: snapshot unstable or a file changed since it was observed")).await;
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
            provenance,
            false,
            |max_hunks| {
                crate::changes::compose_diff(
                    &scope,
                    &comparison,
                    evidence.clone(),
                    crate::changes::DiffSelectionBudget::bounded(max_hunks, budget.max_bytes),
                )
            },
        )
        .map_err(|code| {
            if code == FailureCode::Capacity {
                let candidate = crate::changes::compose_diff(
                    &scope,
                    &comparison,
                    evidence.clone(),
                    crate::changes::DiffSelectionBudget::bounded(1, budget.max_bytes),
                );
                job.failure_detail = Some(diff_capacity_detail(
                    mode,
                    authority.epoch(),
                    &reference,
                    provenance,
                    false,
                    &candidate,
                ));
            }
            code
        })?;
        // T36B: retain the bounded provenance of every path this diff represents — each
        // delivered path plus its rename source — independently of the disposable pagination
        // state, so cached delivery must prove each under the live profile before it hands
        // back any composed page. The collector already bounded the path count and bytes.
        // Name-only untracked entries carry no worktree bytes to disclose and must remain
        // inspectable when they are symlinks. Their relative names were validated at capture.
        // Captured untracked text keeps the same no-follow disclosure and freshness checks.
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
                    .conflicts()
                    .iter()
                    .map(|entry| entry.path().to_path_buf()),
            )
            .collect();
        self.shared
            .set_diff_provenance(&job.reference, represented_paths);
        let diff_page = result.detail_cursor().map(|cursor| DiffPageState {
            scope: scope.clone(),
            evidence: DiffPageEvidence::Snapshot {
                comparison: Box::new(comparison.clone()),
                snapshot: Box::new(evidence),
            },
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
        Ok((
            diff_page_reply(
                mode,
                authority.epoch(),
                &reference,
                provenance,
                false,
                &result,
                false,
            ),
            Some(authority),
            None,
        ))
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
