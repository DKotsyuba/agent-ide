//! Execution adapter for Workspace's filter-free snapshot collector; no Git syntax is interpreted here.

use super::*;
use crate::{
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
        self.worker
            .observations
            .confirm_current(lookup, latest)
            .await
            .ok()?
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
            Err(error) => return Err(self.worker.spawn_failure(error, &binding)),
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
            .admission
            .release_reaped(completed.settlement)
            .map_err(|_| FailureCode::Internal)?;
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
    scope: GitScope,
    comparison: crate::workspace::git::GitComparison,
    evidence: crate::workspace::git::snapshot::GitSnapshot,
    budget: crate::changes::DiffSelectionBudget,
    cursor: crate::changes::DiffDetailCursor,
    mode: DiffMode,
}

impl DiffPageState {
    /// Expands exactly the next bounded page from the retained evidence and comparison.
    pub(super) fn expand(&self, expected_scope: &GitScope) -> crate::changes::DiffResult {
        crate::changes::expand_diff(
            expected_scope,
            &self.comparison,
            self.evidence.clone(),
            &self.cursor,
            self.budget,
        )
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
}

/// Renders the exact typed freshness/coverage/provenance/status facts for one Diff page.
pub(super) fn render_diff_text(
    mode: DiffMode,
    result: &crate::changes::DiffResult,
    authority_epoch: u64,
) -> String {
    let mut text = format!(
        "mode: {:?}\nstate: {:?}\ncoverage: {:?}\nfreshness: {:?}\nauthority_epoch: {}\nbaseline_reference: {}\nbaseline_coverage: {:?}\nbaseline_window: {:?}\ntracked: {}; untracked: {}; conflicted: {}\nomitted_hunks: {}; omitted_bytes: {}; more_available: {}\n",
        mode,
        result.state(),
        result.coverage(),
        result.freshness(),
        authority_epoch,
        result.provenance().baseline_reference().unwrap_or("none"),
        result.provenance().baseline_coverage(),
        result.provenance().baseline_window(),
        result.counts().tracked(),
        result.counts().untracked(),
        result.counts().conflicted(),
        result.overflow_hunks(),
        result.overflow_bytes(),
        result.detail_cursor().is_some(),
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
    for hunk in result.selected_hunks() {
        match std::str::from_utf8(hunk.patch()) {
            Ok(patch) => text.push_str(patch),
            Err(_) => text.push_str(&format!("raw_patch_hex: {:02x?}\n", hunk.patch())),
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
            let mut child = match OwnedChild::spawn_captured(
                &request,
                lease,
                Some(active),
                &job.target.codex.path,
                self.shared.launcher.limits.output_bytes,
            ) {
                Ok(child) => child,
                Err(error) => return Err(self.spawn_failure(error, &binding)),
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
            self.admission
                .release_reaped(completed.settlement)
                .map_err(|_| FailureCode::Internal)?;
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
        let policy = LocalExecutionPolicy::new(
            BTreeSet::from([program.path.clone()]),
            64 * 1024,
            16,
            job.target.allow_disabled_host,
        )
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
        let result = crate::changes::compose_diff(&scope, &comparison, evidence.clone(), budget);
        if matches!(
            result.state(),
            crate::changes::DiffResultState::Unavailable | crate::changes::DiffResultState::Failed
        ) {
            return Err(FailureCode::SourceUnavailable);
        }
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
        let diff_page = result.detail_cursor().map(|cursor| DiffPageState {
            scope: scope.clone(),
            comparison: comparison.clone(),
            evidence,
            budget,
            cursor: cursor.clone(),
            mode,
        });
        self.shared.set_diff_page(&job.reference, diff_page);
        let text = render_diff_text(mode, &result, authority.epoch());
        let truncated =
            result.truncated_output() || result.overflow_hunks() > 0 || result.overflow_bytes() > 0;
        Ok((
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text,
                detail_ref: Some(reference),
                truncated,
            },
            Some(authority),
            None,
        ))
    }
}
