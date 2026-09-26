//! Admission and owned-child supervision for controlled Agent IDE effects.
//!
//! This module deliberately accepts only peer-validated authority and controlled commands. Owned
//! children run as ordinary processes of the daemon's user: the IDE's only path policy is the
//! configured `allowed_roots` list, which Assistance enforces before a request reaches here.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    task::{AbortHandle, JoinHandle},
    time::timeout,
};

use crate::assistance::host_binding::{ActiveBindingUse, BindingRef};
use crate::workspace::git::discovery::validate_current_git_metadata;

/// Identifies a host invocation already proven by Assistance's host-binding adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedHostInvocation {
    /// Opaque Assistance-validated association between actor, invocation, and host channel.
    binding: String,
    /// Assistance binding that requires a fresh consume before a delayed owned spawn.
    active_binding: Option<BindingRef>,
}

impl ValidatedHostInvocation {
    /// Creates a token for a nonempty binding that Assistance has already validated.
    ///
    /// Construction carries no independent host proof: Assistance must reject unverified
    /// hook/MCP pairs before creating this token. Synthetic/test invocations have no Assistance
    /// binding and need no fresh use at spawn.
    pub fn from_verified_binding(binding: impl Into<String>) -> Result<Self, RequestError> {
        let binding = binding.into();
        if binding.is_empty() {
            return Err(RequestError::MissingBinding);
        }
        Ok(Self {
            binding,
            active_binding: None,
        })
    }

    /// Admits one consumed active use into Execution.
    ///
    /// The use is consumed at this boundary: it proves a fresh liveness check before this local
    /// step begins, but cannot become durable authority. A later owned spawn must present another
    /// use for the same immutable generation.
    pub fn from_active_use(active_use: ActiveBindingUse) -> Self {
        let binding = active_use.binding_ref().clone();
        Self {
            binding: blake3::Hash::from_bytes(binding.fingerprint())
                .to_hex()
                .to_string(),
            active_binding: Some(binding),
        }
    }

    /// Returns the stable, opaque binding identifier for operation evidence.
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Consumes a freshly checked binding use for an immediate owned child spawn.
    ///
    /// Synthetic/test invocations have no Assistance binding and need no use. An invocation from
    /// an Assistance use requires a use for the same immutable generation, preventing queue
    /// delay from authorizing a post-stop effect.
    fn consume_active_use(&self, active_use: ActiveBindingUse) -> Result<(), RequestError> {
        match &self.active_binding {
            Some(binding) if active_use.binding_ref() == binding => Ok(()),
            _ => Err(RequestError::MissingActiveBindingUse),
        }
    }
}

/// Identifies the canonical worktree and epoch validated by Workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAuthority {
    /// Opaque Workspace worktree identity; root path alone cannot distinguish reincarnations.
    worktree_id: String,
    /// Workspace incarnation that changes when identity evidence requires reconciliation.
    incarnation: String,
    /// Canonical root whose ownership and lifecycle Workspace has already validated.
    root: PathBuf,
    /// Workspace-verified canonical common Git directory, including linked-worktree metadata.
    git_common_dir: Option<PathBuf>,
    /// Monotonic Workspace authority epoch that rejects stale effects at peer boundaries.
    epoch: u64,
}

impl WorkspaceAuthority {
    /// Creates an authority token without Git metadata for disabled-host or test-only work.
    ///
    /// Managed execution refuses this token. Product callers use
    /// `from_workspace_with_git_common_dir` with durable Workspace identity instead.
    pub fn from_workspace(
        worktree_id: impl Into<String>,
        incarnation: impl Into<String>,
        root: PathBuf,
        epoch: u64,
    ) -> Result<Self, RequestError> {
        let worktree_id = worktree_id.into();
        let incarnation = incarnation.into();
        if worktree_id.is_empty() || incarnation.is_empty() || !is_normal_absolute(&root) {
            return Err(RequestError::InvalidWorktree);
        }
        Ok(Self {
            worktree_id,
            incarnation,
            git_common_dir: None,
            root,
            epoch,
        })
    }

    /// Carries Workspace's already-verified canonical common Git directory into Execution.
    ///
    /// The directory and its `.git` backpointers are rechecked at managed admission and spawn.
    pub fn from_workspace_with_git_common_dir(
        worktree_id: impl Into<String>,
        incarnation: impl Into<String>,
        root: PathBuf,
        git_common_dir: PathBuf,
        epoch: u64,
    ) -> Result<Self, RequestError> {
        let mut authority = Self::from_workspace(worktree_id, incarnation, root, epoch)?;
        if !is_normal_absolute(&git_common_dir) {
            return Err(RequestError::InvalidWorktree);
        }
        authority.git_common_dir = Some(git_common_dir);
        Ok(authority)
    }

    /// Returns the canonical worktree root supplied by Workspace.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the canonical common Git directory supplied by Workspace, if present.
    pub fn git_common_dir(&self) -> Option<&Path> {
        self.git_common_dir.as_deref()
    }

    /// Returns the opaque worktree identity supplied by Workspace.
    pub fn worktree_id(&self) -> &str {
        &self.worktree_id
    }

    /// Returns the Workspace incarnation required to distinguish recreated worktrees.
    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }

    /// Returns the authority epoch that peers use to reject stale effects.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Distinguishes Workspace-controlled Git from an Intelligence-declared provider process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandKind {
    /// A raw Git argv already parsed and policy-checked by Workspace.
    Git,
    /// A provider executable selected by Intelligence's declared profile.
    Provider,
    /// A bounded job selected by an accepted profile, never model text.
    Job,
}

/// Carries a concrete executable invocation supplied by a responsible peer rather than a model.
#[derive(Clone, Debug)]
pub struct ControlledCommand {
    /// Responsible peer's command category, used for evidence but not model policy inference.
    kind: CommandKind,
    /// Absolute executable path constrained by local allowlist before process construction.
    program: PathBuf,
    /// Already parsed argv values supplied by Workspace or an accepted provider profile.
    args: Vec<OsString>,
    /// Absolute cwd constrained to the validated worktree and unchanged sandbox cwd.
    cwd: PathBuf,
    /// Exact environment entries after local cardinality validation; parent environment is cleared.
    env: BTreeMap<OsString, OsString>,
    /// Optional already-open private input file; no repository path is passed as stdin.
    stdin_file: Option<Arc<std::fs::File>>,
    /// Executable identity measured at declaration; construction fails closed without it.
    program_identity: ExecutableIdentity,
}

impl PartialEq for ControlledCommand {
    /// Compares command semantics and measured executable identity, never descriptor numbers.
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.program == other.program
            && self.args == other.args
            && self.cwd == other.cwd
            && self.env == other.env
            && match (&self.stdin_file, &other.stdin_file) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
            && self.program_identity == other.program_identity
    }
}

impl Eq for ControlledCommand {}

/// Immutable executable object and content identity used to reject path replacement before spawn.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ExecutableIdentity {
    /// Canonical path of the opened executable object.
    canonical_path: PathBuf,
    /// Unix device containing the opened executable object.
    device: u64,
    /// Unix inode of the opened executable object.
    inode: u64,
    /// Complete BLAKE3 digest of the regular executable bytes.
    digest: blake3::Hash,
}

impl ControlledCommand {
    /// Builds a command from a peer-validated executable, argv, cwd, and environment.
    ///
    /// The caller must have validated command semantics for `kind`; Execution rejects relative
    /// paths and later intersects this value with local policy and workspace authority. The
    /// executable must already be a stable regular file with readable bytes at declaration time;
    /// an absent, unreadable, or non-regular path fails construction closed rather than deferring
    /// identity to a later, unmeasured spawn attempt.
    pub fn from_validated_peer(
        kind: CommandKind,
        program: PathBuf,
        args: Vec<OsString>,
        cwd: PathBuf,
        env: BTreeMap<OsString, OsString>,
    ) -> Result<Self, RequestError> {
        if !is_normal_absolute(&program) || !is_normal_absolute(&cwd) {
            return Err(RequestError::InvalidCommandPath);
        }
        let program_identity = executable_identity(&program)?;
        Ok(Self {
            kind,
            program,
            args,
            cwd,
            env,
            stdin_file: None,
            program_identity,
        })
    }

    /// Binds a private, already-open bounded input file to one peer-built command. Clones retain
    /// the same descriptor; the owning snapshot intent keeps its 0700 directory until reap.
    pub(crate) fn with_private_stdin(mut self, file: std::fs::File) -> Self {
        self.stdin_file = Some(Arc::new(file));
        self
    }

    /// Reports whether this command's construction-time program digest equals `expected`.
    ///
    /// This performs no I/O and does not revalidate the executable; callers use it to bind a newly
    /// captured command to an independently accepted digest before later spawn-time revalidation.
    pub(crate) fn has_program_digest(&self, expected: &blake3::Hash) -> bool {
        self.program_identity.digest == *expected
    }

    /// Rebuilds this exact command for a verified foreground helper's inherited sandbox.
    ///
    /// The executable object and bytes are rechecked immediately before spawn. The returned
    /// command has a cleared environment, fixed cwd/argv, kill-on-drop and its own process group;
    /// the caller still owns pipe selection, bounded cancellation and direct-child wait evidence.
    pub(crate) fn inherited_process(&self) -> Result<Command, RequestError> {
        if executable_identity(&self.program)? != self.program_identity {
            return Err(RequestError::ExecutableUnavailable);
        }
        let mut process = Command::new(&self.program);
        process
            .args(&self.args)
            .current_dir(&self.cwd)
            .env_clear()
            .envs(&self.env);
        configure_process_group(&mut process);
        Ok(process)
    }
}

/// Identifies one Workspace discovery operation without granting worktree authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryOperationRef(String);

impl DiscoveryOperationRef {
    /// Creates a nonempty stable operation reference supplied by Workspace for result correlation.
    pub fn new(value: impl Into<String>) -> Result<Self, RequestError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 || value.contains(char::from(0)) {
            return Err(RequestError::InvalidDiscoveryOperation);
        }
        Ok(Self(value))
    }
}

/// Selects one fixed read-only Git discovery command; callers cannot extend its argv.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitDiscoveryQuery {
    /// Returns the worktree root as one terminal-LF raw path result.
    ShowTopLevel,
    /// Returns the absolute common Git directory as one terminal-LF raw path result (Git >= 2.31).
    GitCommonDir,
    /// Returns every worktree record as NUL-delimited porcelain bytes.
    WorktreeListPorcelainZ,
}

/// Holds the fresh binding use and raw candidate path for pre-authority Git discovery.
#[derive(Clone, Debug)]
pub struct DiscoverWorktreeRequest {
    /// Invocation that consumed current Assistance liveness and retained the matching generation.
    invocation: ValidatedHostInvocation,
    /// Raw Unix candidate path used only as the literal value after Git's `-C` argument.
    candidate_cwd: OsString,
    /// Workspace operation reference copied to every raw result without interpreting it.
    operation: DiscoveryOperationRef,
}

impl DiscoverWorktreeRequest {
    /// Accepts a fresh Assistance use, the raw candidate path, and the operation ref.
    ///
    /// This constructor has no WorkspaceAuthority output and never normalizes `candidate_cwd`.
    /// A later queued execution must present another fresh use to `run` before a child can launch.
    pub fn from_active_use(
        active_use: ActiveBindingUse,
        candidate_cwd: OsString,
        operation: DiscoveryOperationRef,
    ) -> Result<Self, RequestError> {
        if os_bytes(&candidate_cwd) == 0 {
            return Err(RequestError::InvalidDiscoveryCandidate);
        }
        Ok(Self {
            invocation: ValidatedHostInvocation::from_active_use(active_use),
            candidate_cwd,
            operation,
        })
    }

    /// Validates exactly one fixed Git query under local discovery policy.
    pub fn validate_query(
        self,
        query: GitDiscoveryQuery,
        policy: &GitDiscoveryPolicy,
    ) -> Result<ValidatedGitDiscovery, RequestError> {
        if !is_normal_absolute(&policy.git_program) || policy.output_cap == 0 {
            return Err(RequestError::InvalidDiscoveryPolicy);
        }
        let mut args = vec![OsString::from("-C"), self.candidate_cwd.clone()];
        match query {
            GitDiscoveryQuery::ShowTopLevel => {
                args.extend([
                    OsString::from("rev-parse"),
                    OsString::from("--show-toplevel"),
                ]);
            }
            GitDiscoveryQuery::GitCommonDir => {
                args.extend([
                    OsString::from("rev-parse"),
                    OsString::from("--path-format=absolute"),
                    OsString::from("--git-common-dir"),
                ]);
            }
            GitDiscoveryQuery::WorktreeListPorcelainZ => {
                args.extend([
                    OsString::from("worktree"),
                    OsString::from("list"),
                    OsString::from("--porcelain"),
                    OsString::from("-z"),
                ]);
            }
        }
        // The raw candidate is the directory this discovery actually runs in; a relative or
        // non-normal candidate fails command construction closed.
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            policy.git_program.clone(),
            args,
            PathBuf::from(&self.candidate_cwd),
            BTreeMap::new(),
        )?;
        Ok(ValidatedGitDiscovery {
            invocation: self.invocation,
            command,
            operation: self.operation,
            query,
            output_cap: policy.output_cap,
        })
    }
}

/// Defines the only local policy values available to fixed pre-authority Git discovery.
#[derive(Clone, Debug)]
pub struct GitDiscoveryPolicy {
    /// Absolute configured Git executable; no request can replace it.
    git_program: PathBuf,
    /// Bounded retained bytes for each stdout/stderr stream while both pipes continue draining.
    output_cap: usize,
}

impl GitDiscoveryPolicy {
    /// Creates a fixed discovery policy after rejecting an invalid executable or zero output budget.
    pub fn new(git_program: PathBuf, output_cap: usize) -> Result<Self, RequestError> {
        if !is_normal_absolute(&git_program)
            || output_cap == 0
            || output_cap > MAX_GIT_DISCOVERY_BYTES
        {
            return Err(RequestError::InvalidDiscoveryPolicy);
        }
        Ok(Self {
            git_program,
            output_cap,
        })
    }
}

/// Carries a validated fixed Git discovery until a fresh active use permits its owned spawn.
/// One validated attempt cannot be cloned to repeat-mint no-child settlement:
/// ```compile_fail
/// use agent_ide::execution::ValidatedGitDiscovery;
/// fn duplicate(request:ValidatedGitDiscovery) { let copied=request.clone(); }
/// ```
#[derive(Debug)]
pub struct ValidatedGitDiscovery {
    /// Assistance-correlated invocation retained for the final liveness recheck.
    invocation: ValidatedHostInvocation,
    /// Fixed Git command with raw candidate only after `-C`.
    command: ControlledCommand,
    /// Opaque Workspace operation correlation reference.
    operation: DiscoveryOperationRef,
    /// Fixed query kind whose output remains raw.
    query: GitDiscoveryQuery,
    /// Fixed per-stream retained-output cap from local discovery policy.
    output_cap: usize,
}

/// Hard per-stream retained byte ceiling for fixed discovery evidence.
pub const MAX_GIT_DISCOVERY_BYTES: usize = 1024 * 1024;

/// Immutable bounded fixed-query evidence, intentionally containing no execution settlement capability.
#[derive(Clone, Debug)]
pub struct GitDiscoveryEvidence {
    /// Exact Workspace operation correlation reference.
    operation: DiscoveryOperationRef,
    /// Fixed query whose bytes were captured.
    query: GitDiscoveryQuery,
    /// Bounded raw stdout and its complete/truncated/drained metadata.
    stdout: CapturedOutput,
    /// Bounded raw stderr and its complete/truncated/drained metadata.
    stderr: CapturedOutput,
    /// Direct-child status observed by the Execution owner.
    exit_status: ExitStatus,
    /// Monotonic elapsed capture interval.
    elapsed: Duration,
    /// Explicit cancellation requests, never inferred from a nonzero exit.
    cancellation: Option<CancellationEvidence>,
    /// Direct-child reap does not prove termination of every descendant.
    descendants: DescendantEvidence,
}
impl GitDiscoveryEvidence {
    /// Validates bounded fixture/capture data without minting authority or a process settlement proof.
    /// Exit status must describe exit/signal; drained bytes cannot contradict retained/truncated bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation: DiscoveryOperationRef,
        query: GitDiscoveryQuery,
        stdout: CapturedOutput,
        stderr: CapturedOutput,
        exit_status: ExitStatus,
        elapsed: Duration,
        cancellation: Option<CancellationEvidence>,
        descendants: DescendantEvidence,
    ) -> Result<Self, RequestError> {
        use std::os::unix::process::ExitStatusExt;
        if (!exit_status.success()
            && exit_status.code().is_none()
            && exit_status.signal().is_none())
            || [&stdout, &stderr].iter().any(|output| {
                output.bytes.len() > MAX_GIT_DISCOVERY_BYTES
                    || output.drained_bytes < output.bytes.len() as u64
                    || (!output.truncated && output.drained_bytes != output.bytes.len() as u64)
            })
        {
            return Err(RequestError::InvalidDiscoveryEvidence);
        }
        Ok(Self {
            operation,
            query,
            stdout,
            stderr,
            exit_status,
            elapsed,
            cancellation,
            descendants,
        })
    }
    /// Returns the original immutable operation reference.
    pub fn operation(&self) -> &DiscoveryOperationRef {
        &self.operation
    }
    /// Returns the fixed discovery query.
    pub const fn query(&self) -> GitDiscoveryQuery {
        self.query
    }
    /// Returns bounded raw stdout without exposing mutable capture metadata.
    pub fn stdout(&self) -> &CapturedOutput {
        &self.stdout
    }
    /// Returns bounded raw stderr without exposing mutable capture metadata.
    pub fn stderr(&self) -> &CapturedOutput {
        &self.stderr
    }
    /// Returns observed direct-child status, not a worktree/authority claim.
    pub const fn exit_status(&self) -> ExitStatus {
        self.exit_status
    }
    /// Returns monotonic elapsed execution/capture time.
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
    /// Returns exact cancellation request evidence when cancellation was requested.
    pub const fn cancellation(&self) -> Option<CancellationEvidence> {
        self.cancellation
    }
    /// Returns the deliberately conservative descendant status.
    pub const fn descendants(&self) -> DescendantEvidence {
        self.descendants
    }
}

/// Separates immutable discovery bytes from one-time physical settlement, allowing immediate slot release.
#[derive(Debug)]
pub struct CompletedGitDiscovery {
    /// Bounded immutable value safe to retain for Workspace's correlated three-query validation.
    pub evidence: GitDiscoveryEvidence,
    /// Consume immediately through admission.release_reaped; never clone or pass into Workspace parsing.
    pub settlement: DirectChildReap,
}

impl ValidatedGitDiscovery {
    /// Returns an owned fixed-query handle after checking fresh liveness at spawn.
    ///
    /// Failure returns the unique no-child settlement with a closed request error. The handle
    /// retains operation/query provenance and can be cancelled while a borrowed wait runs.
    pub fn spawn(
        self,
        lease: AdmissionLease,
        active_use: ActiveBindingUse,
    ) -> Result<OwnedGitDiscovery, ProcessError> {
        let started = std::time::Instant::now();
        let settlement = SpawnNeverStarted::ordinary(lease);
        if let Err(error) = self.invocation.consume_active_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        let process = OwnedChild::spawn_parts(&self.command, settlement, self.output_cap)?;
        Ok(OwnedGitDiscovery {
            process,
            operation: self.operation,
            query: self.query,
            started,
        })
    }
}

/// Owns one fixed Git discovery and its immutable evidence scope until verified physical settlement.
pub struct OwnedGitDiscovery {
    /// Execution-owned process and exclusive captured streams.
    process: OwnedChild,
    /// Original stable Workspace operation reference.
    operation: DiscoveryOperationRef,
    /// Exact fixed discovery query; cancellation never changes its interpretation.
    query: GitDiscoveryQuery,
    /// Monotonic start instant for elapsed evidence.
    started: std::time::Instant,
}
impl OwnedGitDiscovery {
    /// Borrows the child for a bounded wait; cancelling this wait retains the owning handle.
    /// No admission proof is released until reap/cancel_and_reap completes.
    pub async fn wait(&mut self, deadline: Duration) -> Result<ExitStatus, ProcessError> {
        self.process.wait(deadline).await
    }

    /// Reaps under separate exit/drain deadlines and returns exact fixed-query evidence.
    pub async fn reap(
        self,
        deadline: Duration,
        output_deadline: Duration,
    ) -> Result<CompletedGitDiscovery, ProcessError> {
        let Self {
            process,
            operation,
            query,
            started,
            ..
        } = self;
        let result = process.reap(deadline, output_deadline).await?;
        Ok(discovery_result(operation, query, started, result))
    }

    /// Requests bounded TERM/KILL and returns release evidence only after direct-child reap.
    pub async fn cancel_and_reap(
        self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<CompletedGitDiscovery, ProcessError> {
        let Self {
            process,
            operation,
            query,
            started,
            ..
        } = self;
        let result = process.cancel_and_reap(grace, output_deadline).await?;
        Ok(discovery_result(operation, query, started, result))
    }
}

/// Binds the physical settlement to its original query/operation without decoding any Git bytes.
fn discovery_result(
    operation: DiscoveryOperationRef,
    query: GitDiscoveryQuery,
    started: std::time::Instant,
    result: CompletedProcess,
) -> CompletedGitDiscovery {
    CompletedGitDiscovery {
        evidence: GitDiscoveryEvidence {
            operation,
            query,
            stdout: result.evidence.stdout,
            stderr: result.evidence.stderr,
            exit_status: result.evidence.status,
            elapsed: started.elapsed(),
            cancellation: result.evidence.cancellation,
            descendants: result.evidence.descendants,
        },
        settlement: result.settlement,
    }
}

/// Product argv ceiling handed to [`LocalExecutionPolicy`]: the bound snapshot batch planners
/// must fail closed against, so the ceiling lives here once instead of a duplicated literal.
pub const MAX_PRODUCT_ARGV_BYTES: usize = 64 * 1024;

/// Applies local ceilings to already validated peer input.
#[derive(Clone, Debug)]
pub struct LocalExecutionPolicy {
    /// Absolute executable paths permitted by local policy.
    allowed_programs: BTreeSet<PathBuf>,
    /// Maximum platform-byte size of all argv values, excluding executable path.
    max_argv_bytes: usize,
    /// Maximum number of explicitly forwarded environment entries.
    max_environment_entries: usize,
}

impl LocalExecutionPolicy {
    /// Creates a policy whose program allowlist and numeric limits are independently validated.
    pub fn new(
        allowed_programs: BTreeSet<PathBuf>,
        max_argv_bytes: usize,
        max_environment_entries: usize,
    ) -> Result<Self, RequestError> {
        if allowed_programs.is_empty() || max_argv_bytes == 0 {
            return Err(RequestError::InvalidPolicy);
        }
        Ok(Self {
            allowed_programs,
            max_argv_bytes,
            max_environment_entries,
        })
    }
}

/// Reports a rejected controlled execution request before it can reserve or spawn resources.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestError {
    /// Assistance did not provide a verified nonempty binding.
    MissingBinding,
    /// Workspace supplied a noncanonical or relative worktree path.
    InvalidWorktree,
    /// A controlled command used a relative or lexically escaping program/cwd path.
    InvalidCommandPath,
    /// The declared executable is not one stable regular executable object with readable bytes.
    ExecutableUnavailable,
    /// The policy has no executable allowlist or no argv capacity.
    InvalidPolicy,
    /// The command program is outside the local allowlist.
    ProgramDenied,
    /// The command cwd is not the current authoritative worktree.
    WorktreeDenied,
    /// The argv exceeds the local byte ceiling.
    ArgvTooLarge,
    /// The environment exceeds the local entry ceiling.
    EnvironmentTooLarge,
    /// A delayed owned spawn did not present a newly consumed matching active binding use.
    MissingActiveBindingUse,
    /// Workspace supplied an empty discovery operation reference.
    InvalidDiscoveryOperation,
    /// Workspace supplied an empty raw candidate path for fixed Git `-C` execution.
    InvalidDiscoveryCandidate,
    /// Local fixed Git discovery policy has no absolute executable or output budget.
    InvalidDiscoveryPolicy,
    /// Fixed-query fixture/capture metadata is oversized or contradicts its exit/drain evidence.
    InvalidDiscoveryEvidence,
    /// The worktree's live `.git` backpointers no longer describe the discovered repository.
    GitMetadataInvalid,
}

/// Couples the only inputs permitted to reach an owned operating-system spawn.
#[derive(Clone, Debug)]
pub struct ValidatedExecutionRequest {
    /// Host binding whose fresh liveness use is rechecked before a delayed owned spawn.
    invocation: ValidatedHostInvocation,
    /// Workspace ownership token constraining this child to one current worktree epoch.
    authority: WorkspaceAuthority,
    /// Fully controlled executable invocation ready for admission and later owned spawn.
    command: ControlledCommand,
}

impl ValidatedExecutionRequest {
    /// Intersects local policy and Workspace authority before an admission request.
    ///
    /// The command always runs in the current authoritative worktree. A Git command additionally
    /// requires the worktree's live `.git` backpointers to still describe the discovered
    /// repository, so a queued operation never silently reads a different one.
    pub fn validate(
        invocation: ValidatedHostInvocation,
        authority: WorkspaceAuthority,
        command: ControlledCommand,
        policy: &LocalExecutionPolicy,
    ) -> Result<Self, RequestError> {
        if !policy.allowed_programs.contains(&command.program) {
            return Err(RequestError::ProgramDenied);
        }
        if command.cwd != authority.root {
            return Err(RequestError::WorktreeDenied);
        }
        if command.args.iter().map(os_bytes).sum::<usize>() > policy.max_argv_bytes {
            return Err(RequestError::ArgvTooLarge);
        }
        if command.env.len() > policy.max_environment_entries {
            return Err(RequestError::EnvironmentTooLarge);
        }
        git_metadata_current(&authority, &command)?;
        Ok(Self {
            invocation,
            authority,
            command,
        })
    }

    /// Returns the command class for accounting and durable operation records.
    pub const fn kind(&self) -> CommandKind {
        self.command.kind
    }

    /// Returns the peer-validated Workspace authority used for this request.
    pub fn authority(&self) -> &WorkspaceAuthority {
        &self.authority
    }

    /// Consumes a freshly checked active use immediately before an owned physical spawn.
    ///
    /// Requests constructed from an Assistance use retain its binding generation so a queue
    /// delay cannot convert pre-stop liveness into a later effect; a Git command also rechecks
    /// the live `.git` backpointers immediately before spawn. Synthetic/test requests have no
    /// active binding and reject no optional use.
    fn consume_spawn_use(&self, active_use: Option<ActiveBindingUse>) -> Result<(), RequestError> {
        git_metadata_current(&self.authority, &self.command)?;
        match (&self.invocation.active_binding, active_use) {
            (Some(_), Some(active_use)) => self.invocation.consume_active_use(active_use),
            (Some(_), None) => Err(RequestError::MissingActiveBindingUse),
            (None, _) => Ok(()),
        }
    }
}

/// Rechecks a Git command's worktree against its live `.git` backpointers.
///
/// Workspace validated the `.git` -> admin -> common cycle at discovery; this repeats that check
/// at validation and immediately before spawn so a queued Git operation never silently reads a
/// different repository after `.git` changed. Provider and job commands run no Git and skip it.
fn git_metadata_current(
    authority: &WorkspaceAuthority,
    command: &ControlledCommand,
) -> Result<(), RequestError> {
    match (command.kind, authority.git_common_dir()) {
        (CommandKind::Git, Some(common)) => validate_current_git_metadata(&authority.root, common)
            .map_err(|_| RequestError::GitMetadataInvalid),
        _ => Ok(()),
    }
}

/// Names an owner whose queued and running work receives an independent ceiling.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OwnerId(String);

impl OwnerId {
    /// Creates a nonempty opaque owner identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, AdmissionError> {
        let value = value.into();
        if value.is_empty() {
            return Err(AdmissionError::InvalidLimits);
        }
        Ok(Self(value))
    }
}

/// Separates latency-sensitive requests from background work without permitting starvation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionClass {
    /// A request that may use the configured interactive burst.
    Interactive,
    /// A request that must receive service after the configured interactive burst.
    Background,
}

/// Defines finite global/per-owner queue and running ceilings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLimits {
    /// Maximum simultaneously admitted owned operations.
    pub total_running: usize,
    /// Maximum simultaneously admitted operations for one owner.
    pub per_owner_running: usize,
    /// Maximum queued operations for one owner.
    pub per_owner_queued: usize,
    /// Maximum queued operations across every owner.
    pub total_queued: usize,
    /// Maximum consecutive interactive grants while eligible background work waits.
    pub interactive_burst: usize,
}

/// Explains why admission cannot create a lease or queue ticket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionError {
    /// Limits contain an impossible zero capacity.
    InvalidLimits,
    /// The global queue is full.
    GlobalQueueFull,
    /// The requesting owner's queue is full.
    OwnerQueueFull,
    /// A lease does not belong to this controller or was already released.
    UnknownLease,
    /// A provider reservation requires the registry's one-time direct-child reap completion.
    ProviderReapRequired,
}

/// Identifies a queued request that may be inspected or cancelled without terminating a process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueTicket(u64, u64);

/// Reserves one admitted execution slot until it is explicitly released after reaping.
///
/// Abnormal owned-child drop retains this reservation because it supplies no reap evidence.
/// A forwarded admission cannot be copied into a second raw child:
/// ```compile_fail
/// use agent_ide::execution::*;
/// use std::path::Path;
/// fn duplicate(registry:&mut ProviderLeaseRegistry, admission:&mut AdmissionController,
///     view:ProviderViewLease, request:&ValidatedExecutionRequest, lease:AdmissionLease) {
///     let capability=registry.take_forwarder_spawn_lease(admission,view,request,lease).unwrap();
///     let second=OwnedChild::spawn_captured(request,lease,None,Path::new("/unused"),64);
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct AdmissionLease(u64, u64);

/// Non-authorizing controller/slot key retained by registries; cannot be converted into a lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AdmissionKey(u64, u64);
impl AdmissionLease {
    /// Copies accounting identity without duplicating admission or spawn authority.
    fn key(&self) -> AdmissionKey {
        AdmissionKey(self.0, self.1)
    }
}

/// Returns the only three observable outcomes of asking the centralized admission controller.
#[derive(Debug, Eq, PartialEq)]
pub enum Admission {
    /// A slot is reserved and the caller may proceed to an owned spawn.
    Granted(AdmissionLease),
    /// The request awaits a fair future grant and has no resource reservation yet.
    Queued(QueueTicket),
    /// A finite ceiling refused the request.
    Refused(AdmissionError),
}

/// Couples a central queue ticket to the exact lease that promoted it.
/// The promotion and its newly granted lease each transfer exactly once:
/// ```compile_fail
/// use agent_ide::execution::AdmissionPromotion;
/// fn duplicate(promotion:AdmissionPromotion) { let first=promotion.lease(); let second=promotion.lease(); }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct AdmissionPromotion {
    /// Ticket removed from the controller queue by this promotion.
    ticket: QueueTicket,
    /// Newly reserved central lease which must be consumed or released exactly once.
    lease: AdmissionLease,
}

impl AdmissionPromotion {
    /// Returns the queued identity this promotion settles.
    pub const fn ticket(&self) -> QueueTicket {
        self.ticket
    }
    /// Returns the central reservation created for this exact ticket.
    pub const fn lease(self) -> AdmissionLease {
        self.lease
    }
}

/// Schedules finite requests with per-owner round-robin selection and bounded class preference.
#[derive(Debug)]
pub struct AdmissionController {
    /// Process-local unique controller identity preventing equal numeric IDs from crossing owners.
    identity: u64,
    /// Finite queue/running ceilings shared by every owner.
    limits: AdmissionLimits,
    /// Number of leases currently held by each owner.
    running: BTreeMap<OwnerId, usize>,
    /// Active lease identity to owner mapping used to reject stale/double release.
    leases: BTreeMap<u64, OwnerId>,
    /// Live slots already bound to provider spawn capabilities across this controller's registries.
    provider_slots: BTreeSet<u64>,
    /// Requests awaiting a slot; entries intentionally consume no running resource.
    queue: VecDeque<QueuedRequest>,
    /// Next nonzero internal identity for tickets and leases.
    next_id: u64,
    /// Owner of the most recent grant, used to rotate eligible owners.
    last_owner: Option<OwnerId>,
    /// Consecutive interactive grants since the most recent background grant.
    interactive_streak: usize,
}

/// Truthful bounded snapshot of centralized process admission and retained reservations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionSnapshot {
    /// Configured maximum simultaneous reservations.
    pub total_running_limit: usize,
    /// Configured maximum simultaneous reservations for one owner.
    pub per_owner_running_limit: usize,
    /// Reservations still held, including launched, draining, and cleanup-uncertain children.
    pub reserved: usize,
    /// Requests waiting without a reservation.
    pub queued: usize,
    /// Remaining global reservation capacity; owner-specific limits can still prevent admission.
    pub globally_available: usize,
}

/// Stores the immutable details needed to promote one queued request fairly.
#[derive(Clone, Debug)]
struct QueuedRequest {
    /// Stable ticket identity used for cancellation and promotion.
    id: u64,
    /// Owner subject to per-owner running/queue ceilings.
    owner: OwnerId,
    /// Fairness class whose burst policy applies when this entry is promoted.
    class: AdmissionClass,
}

/// Allocates nonzero controller identities; process-local proofs are never deserialized across restarts.
static NEXT_ADMISSION_CONTROLLER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

impl AdmissionController {
    /// Creates an empty centralized scheduler after rejecting unusable ceiling combinations.
    pub fn new(limits: AdmissionLimits) -> Result<Self, AdmissionError> {
        if limits.total_running == 0
            || limits.per_owner_running == 0
            || limits.per_owner_queued == 0
            || limits.total_queued == 0
            || limits.interactive_burst == 0
        {
            return Err(AdmissionError::InvalidLimits);
        }
        let identity = NEXT_ADMISSION_CONTROLLER
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |value| value.checked_add(1),
            )
            .map_err(|_| AdmissionError::InvalidLimits)?;
        Ok(Self {
            identity,
            limits,
            running: BTreeMap::new(),
            leases: BTreeMap::new(),
            provider_slots: BTreeSet::new(),
            queue: VecDeque::new(),
            next_id: 1,
            last_owner: None,
            interactive_streak: 0,
        })
    }

    /// Grants an immediately eligible request or creates a bounded queue ticket without bypassing waiters.
    pub fn submit(&mut self, owner: OwnerId, class: AdmissionClass) -> Admission {
        if self.queue.is_empty() && self.can_run(&owner) {
            return Admission::Granted(self.grant(owner, class));
        }
        if self.queue.len() >= self.limits.total_queued {
            return Admission::Refused(AdmissionError::GlobalQueueFull);
        }
        if self
            .queue
            .iter()
            .filter(|entry| entry.owner == owner)
            .count()
            >= self.limits.per_owner_queued
        {
            return Admission::Refused(AdmissionError::OwnerQueueFull);
        }
        let id = self.next_id();
        self.queue.push_back(QueuedRequest { id, owner, class });
        Admission::Queued(QueueTicket(id, self.identity))
    }

    /// Cancels a queued request and releases no running resource because tickets reserve nothing.
    pub fn cancel_ticket(&mut self, ticket: QueueTicket) -> bool {
        if ticket.1 != self.identity {
            return false;
        }
        self.queue
            .iter()
            .position(|entry| entry.id == ticket.0)
            .and_then(|position| self.queue.remove(position))
            .is_some()
    }

    /// Releases a reaped owned process and returns every newly eligible fair lease for callers to spawn.
    pub fn release(
        &mut self,
        lease: AdmissionLease,
    ) -> Result<Vec<AdmissionLease>, AdmissionError> {
        Ok(self
            .release_with_promotions(lease)?
            .into_iter()
            .map(|promotion| promotion.lease)
            .collect())
    }

    /// Releases a lease and preserves ticket-to-lease identity for dependent bounded registries.
    pub fn release_with_promotions(
        &mut self,
        lease: AdmissionLease,
    ) -> Result<Vec<AdmissionPromotion>, AdmissionError> {
        if self.provider_slots.contains(&lease.0) {
            return Err(AdmissionError::ProviderReapRequired);
        }
        self.release_settled(lease)
    }

    /// Releases an ordinary owned process using its non-forgeable direct-child reap proof.
    pub fn release_reaped(
        &mut self,
        proof: DirectChildReap,
    ) -> Result<Vec<AdmissionPromotion>, AdmissionError> {
        if proof.target != SpawnTarget::Ordinary {
            return Err(AdmissionError::ProviderReapRequired);
        }
        self.release_with_promotions(proof.lease)
    }

    /// Consumes a definite no-child proof for an ordinary reservation; provider proofs require their registry.
    pub fn settle_never_started(
        &mut self,
        settlement: SpawnNeverStarted,
    ) -> Result<Vec<AdmissionPromotion>, AdmissionError> {
        if !matches!(settlement.target, SpawnTarget::Ordinary) {
            return Err(AdmissionError::ProviderReapRequired);
        }
        self.release_with_promotions(settlement.lease)
    }

    /// Performs the final accounting mutation after an ordinary release or registry-verified reap.
    fn release_settled(
        &mut self,
        lease: AdmissionLease,
    ) -> Result<Vec<AdmissionPromotion>, AdmissionError> {
        if lease.1 != self.identity {
            return Err(AdmissionError::UnknownLease);
        }
        let owner = self
            .leases
            .remove(&lease.0)
            .ok_or(AdmissionError::UnknownLease)?;
        self.provider_slots.remove(&lease.0);
        let running = self
            .running
            .get_mut(&owner)
            .expect("lease owner is running");
        *running -= 1;
        if *running == 0 {
            self.running.remove(&owner);
        }
        let mut grants = Vec::new();
        while self.total_running() < self.limits.total_running {
            let Some(position) = self.next_eligible_position() else {
                break;
            };
            let queued = self
                .queue
                .remove(position)
                .expect("position comes from queue");
            let ticket = QueueTicket(queued.id, self.identity);
            grants.push(AdmissionPromotion {
                ticket,
                lease: self.grant(queued.owner, queued.class),
            });
        }
        Ok(grants)
    }

    /// Returns reserved slots, including uncertain dropped children; this is not a live-process count.
    pub fn running_count(&self) -> usize {
        self.total_running()
    }

    /// Returns configured capacity and current reservations without inferring process liveness.
    pub fn inspect(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            total_running_limit: self.limits.total_running,
            per_owner_running_limit: self.limits.per_owner_running,
            reserved: self.total_running(),
            queued: self.queue.len(),
            globally_available: self
                .limits
                .total_running
                .saturating_sub(self.total_running()),
        }
    }

    /// Returns whether a ticket remains queued without changing its priority or lifetime.
    pub fn contains_ticket(&self, ticket: QueueTicket) -> bool {
        if ticket.1 != self.identity {
            return false;
        }
        self.queue.iter().any(|entry| entry.id == ticket.0)
    }

    /// Returns whether an owner can receive one more running slot under both finite ceilings.
    fn can_run(&self, owner: &OwnerId) -> bool {
        self.total_running() < self.limits.total_running
            && self.running.get(owner).copied().unwrap_or_default() < self.limits.per_owner_running
    }

    /// Returns the number of globally running leases.
    fn total_running(&self) -> usize {
        self.leases.len()
    }

    /// Assigns a lease and records the class/owner service decision.
    fn grant(&mut self, owner: OwnerId, class: AdmissionClass) -> AdmissionLease {
        let id = self.next_id();
        *self.running.entry(owner.clone()).or_default() += 1;
        self.leases.insert(id, owner.clone());
        self.last_owner = Some(owner);
        self.interactive_streak = match class {
            AdmissionClass::Interactive => self.interactive_streak.saturating_add(1),
            AdmissionClass::Background => 0,
        };
        AdmissionLease(id, self.identity)
    }

    /// Picks an eligible queue entry while alternating owners and bounding interactive bursts.
    fn next_eligible_position(&self) -> Option<usize> {
        let wants_background = self.interactive_streak >= self.limits.interactive_burst
            && self.queue.iter().any(|entry| {
                entry.class == AdmissionClass::Background && self.can_run(&entry.owner)
            });
        let preferred = if wants_background {
            AdmissionClass::Background
        } else {
            AdmissionClass::Interactive
        };
        let mut candidates: Vec<usize> = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.class == preferred && self.can_run(&entry.owner))
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            candidates = self
                .queue
                .iter()
                .enumerate()
                .filter(|(_, entry)| self.can_run(&entry.owner))
                .map(|(index, _)| index)
                .collect();
        }
        candidates
            .iter()
            .copied()
            .find(|index| self.last_owner.as_ref() != Some(&self.queue[*index].owner))
            .or_else(|| candidates.into_iter().next())
    }

    /// Generates a nonzero operation/ticket identity without reusing an active identifier.
    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("operation identifier exhausted");
        id
    }
}

/// Identifies one Execution-owned logical provider view.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProviderViewLease(u64);

/// Is a one-time capability to start the owned backend attached to one newly admitted view.
#[derive(Debug)]
pub struct ProviderSpawnLease {
    /// View that created this backend and owns this one-time start capability.
    view: ProviderViewLease,
    /// Central reservation consumed only by Execution's owned-child setup.
    lease: AdmissionLease,
    /// Exact Workspace authority observed when this view was admitted, including root and epoch.
    authority: WorkspaceAuthority,
    /// Compatible backend identity selected by the responsible provider module.
    backend: String,
    /// Shared write-once actual-child identity, also retained by the registry.
    launched: Arc<Mutex<ProviderLaunchState>>,
    /// True only for a separately reserved shared forwarder.
    forwarder: bool,
}

impl ProviderSpawnLease {
    /// Cancels this unconsumed launch capability before any spawn attempt, producing a one-time no-child proof.
    pub fn cancel(self) -> SpawnNeverStarted {
        SpawnNeverStarted::provider(self)
    }

    /// Returns the backend identity to which this one-time spawn is restricted.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Rejects authority substitution before any process effect or active-binding consumption.
    fn validate_request(&self, request: &ValidatedExecutionRequest) -> Result<(), ProcessError> {
        if request.kind() != CommandKind::Provider || self.authority != *request.authority() {
            return Err(ProcessError::Io(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "provider spawn authority does not match its admitted view",
            )));
        }
        Ok(())
    }
}

/// Consumes one distinct admitted process slot to start one shared backend's logical forwarder.
/// This non-cloneable capability is minted once per view and cannot start the heavy backend.
#[derive(Debug)]
pub struct ProviderForwarderSpawnLease {
    /// Authority-bound process reservation; the listener's admission lease is never reused here.
    spawn: ProviderSpawnLease,
}

impl ProviderForwarderSpawnLease {
    /// Cancels this unconsumed forwarder launch without inventing a child or reap event.
    pub fn cancel(self) -> SpawnNeverStarted {
        self.spawn.cancel()
    }

    /// Returns the exact logical view generation allowed to own this forwarder.
    pub const fn view(&self) -> ProviderViewLease {
        self.spawn.view
    }

    /// Returns the shared backend identity which the forwarder is permitted to join.
    pub fn backend(&self) -> &str {
        self.spawn.backend()
    }
}

/// Classifies the ownership and sharing rule of a provider backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderBackendKind {
    /// One owned backend can serve compatible logical views and holds one admission lease.
    OwnedShared,
    /// One owned backend serves exactly one logical view and holds one admission lease.
    OwnedExclusive,
    /// An observed peer endpoint has no Execution signal or reap capability.
    Borrowed,
}

/// Reports why a provider view cannot be attached to a backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderLeaseError {
    /// A view limit is zero or cannot bound the requested registry.
    InvalidLimits,
    /// The requested backend identity is empty or attempts to alter established ownership.
    InvalidBackend,
    /// An exclusive backend already has a logical view.
    ExclusiveInUse,
    /// A stale logical-view lease was released twice or belongs to another registry.
    UnknownView,
    /// The supplied authority lacks a nonzero Workspace incarnation and cannot be revocation-scoped.
    InvalidAuthority,
    /// The central controller rejected release of the registry's owned admission lease.
    Admission(AdmissionError),
    /// A promotion was not pending, was already consumed, or mismatched its current authority.
    InvalidPromotion,
    /// A bounded view ceiling would be exceeded.
    ViewCapacity,
    /// This view has no unconsumed owned-backend spawn capability.
    SpawnUnavailable,
    /// This backend has no live views and retains its slot until verified physical reap.
    BackendDraining,
    /// Reap proof/capability belongs to another reservation or was already completed.
    InvalidReap,
}

/// Reports one bounded request for a logical provider view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderLeaseAdmission {
    /// A logical view was attached and any owned backend admission is already reserved.
    Granted(ProviderViewLease),
    /// Central admission queued the new owned backend; this ticket reserves no backend or view.
    Queued(QueueTicket),
    /// Central admission refused the new owned backend.
    Refused(AdmissionError),
    /// The requested backend cannot safely accept this view.
    Rejected(ProviderLeaseError),
}

/// Defines finite logical-view ceilings; physical heavy-process ceilings remain central admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderLeaseLimits {
    /// Maximum logical views across all backends.
    pub total_views: usize,
    /// Maximum logical views sharing one backend identity.
    pub per_backend_views: usize,
}

/// Bounded provider registry snapshot that keeps logical and physical lifecycle facts separate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderLeaseSnapshot {
    /// Configured maximum logical views across all backends, including pending requests.
    pub total_view_limit: usize,
    /// Configured maximum logical views for one compatible backend, including pending requests.
    pub per_backend_view_limit: usize,
    /// Attached logical views.
    pub active_views: usize,
    /// Queued logical views that hold no process reservation.
    pub pending_views: usize,
    /// Owned backend reservations, including draining and not-yet-spawned entries.
    pub owned_backends: usize,
    /// Owned backends waiting for exact direct-child reap settlement.
    pub draining_backends: usize,
    /// Backend launch capabilities not yet taken by the trusted spawn path.
    pub spawnable_backends: usize,
    /// Separately reserved shared forwarder processes awaiting settlement.
    pub forwarders: usize,
}

/// One concrete logical detach outcome; an owned last view carries its one-time draining capability.
#[derive(Debug, Eq, PartialEq)]
pub enum BackendRelease {
    /// A compatible shared peer still owns the live backend.
    SharedPeerSurvives,
    /// Physical admission remains reserved until this capability meets matching direct-child proof.
    ReapOwned(BackendReapCapability),
    /// A borrowed endpoint was detached without signal/reap rights.
    BorrowedDetached,
}

/// One-time, non-cloneable permission to settle one draining backend after direct-child reap.
#[derive(Debug)]
pub struct BackendReapCapability {
    /// Exact compatible backend held in draining state.
    backend: String,
    /// Exact central reservation which cannot be freed by logical view release.
    process: ProviderProcess,
}

impl PartialEq for BackendReapCapability {
    /// Compares the exact registry reservation without duplicating its settlement right.
    fn eq(&self, other: &Self) -> bool {
        self.backend == other.backend
            && self.process.key == other.process.key
            && self.process.target == other.process.target
            && Arc::ptr_eq(&self.process.launched, &other.process.launched)
    }
}
impl Eq for BackendReapCapability {}

/// Non-cloneable proof that validation/build/spawn returned before any owned Child handle existed.
/// ```compile_fail
/// use agent_ide::execution::{AdmissionController,SpawnNeverStarted};
/// fn twice(admission:&mut AdmissionController,proof:SpawnNeverStarted) {
///     admission.settle_never_started(proof); admission.settle_never_started(proof);
/// }
/// ```
#[derive(Debug)]
pub struct SpawnNeverStarted {
    /// Exact reservation whose spawn did not create an owned child.
    lease: AdmissionLease,
    /// Registry scope when this was a provider capability, otherwise an ordinary reservation.
    target: SpawnTarget,
    /// Registry-owned launch correlation for provider reservations; absent for direct jobs.
    launched: Option<Arc<Mutex<ProviderLaunchState>>>,
}

/// Closed reservation role carried unchanged through Execution-owned spawn and settlement.
#[derive(Clone, Debug, Eq, PartialEq)]
enum SpawnTarget {
    /// Ordinary non-provider reservation.
    Ordinary,
    /// Heavy backend and the logical view that owned its one-time launch capability.
    Backend {
        backend: String,
        view: ProviderViewLease,
    },
    /// Separately counted shared forwarder view.
    Forwarder {
        backend: String,
        view: ProviderViewLease,
    },
}

impl SpawnNeverStarted {
    /// Wraps one untouched direct admission for an ordinary Git/job launch attempt.
    fn ordinary(lease: AdmissionLease) -> Self {
        Self {
            lease,
            target: SpawnTarget::Ordinary,
            launched: None,
        }
    }
    /// Returns this unique no-child settlement with its exact pre-spawn failure.
    fn error(self, cause: ProcessError) -> ProcessError {
        ProcessError::NeverStarted {
            cause: Box::new(cause),
            settlement: self,
        }
    }
    /// Consumes the unique provider reservation into a definite pre-child settlement or launch attempt.
    fn provider(capability: ProviderSpawnLease) -> Self {
        Self {
            lease: capability.lease,
            launched: Some(capability.launched),
            target: if capability.forwarder {
                SpawnTarget::Forwarder {
                    backend: capability.backend.clone(),
                    view: capability.view,
                }
            } else {
                SpawnTarget::Backend {
                    backend: capability.backend.clone(),
                    view: capability.view,
                }
            },
        }
    }
}

/// Serializes logical revocation against the exact physical spawn boundary.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum ProviderLaunchState {
    /// An issued capability may still create exactly one child.
    #[default]
    Pending,
    /// The logical view was revoked before any child existed.
    Revoked,
    /// A single OS child owns the reservation until direct reap.
    Launched(ProcessIdentityData),
}

/// Registry-owned non-authorizing correlation of reservation, provider target, and actual direct process.
#[derive(Clone, Debug)]
struct ProviderProcess {
    /// Exact central controller and slot identity; never owns a spawn right.
    key: AdmissionKey,
    /// Exact backend/listener or forwarder-view role of this reservation.
    target: SpawnTarget,
    /// Set exactly once by the typed spawn path after OS child creation succeeds.
    launched: Arc<Mutex<ProviderLaunchState>>,
}
impl ProviderProcess {
    /// Creates a target-bound correlation before transferring the sole lease into a spawn capability.
    fn new(lease: &AdmissionLease, target: SpawnTarget) -> Self {
        Self {
            key: lease.key(),
            target,
            launched: Arc::default(),
        }
    }
    /// Invalidates an outstanding capability while preserving an already launched child's identity.
    fn revoke_pending(&self) {
        if let Ok(mut state) = self.launched.lock()
            && matches!(*state, ProviderLaunchState::Pending)
        {
            *state = ProviderLaunchState::Revoked;
        }
    }
    /// Accepts only actual direct-wait proof for this reservation, exact role, and launched process.
    fn matches(&self, proof: &DirectChildReap) -> bool {
        self.key == proof.lease.key() && self.target == proof.target && self.launched.lock().is_ok_and(|state| matches!(*state,ProviderLaunchState::Launched(identity) if identity==proof.identity))
    }
    /// Accepts no-child settlement only when this exact target never registered a launched process.
    fn never_started(&self, proof: &SpawnNeverStarted) -> bool {
        self.key == proof.lease.key()
            && self.target == proof.target
            && self
                .launched
                .lock()
                .is_ok_and(|state| !matches!(*state, ProviderLaunchState::Launched(_)))
    }
}

/// Opaque, non-cloneable launch correlation that may be transferred once to an armed resource guard.
pub struct ProcessIdentity(ProcessIdentityData);
/// Non-authorizing, cloneable identity evidence constructible only after Execution successfully waits.
#[derive(Clone)]
pub struct ReapedChildIdentity(ProcessIdentityData);
/// Private collision-resistant launch generation and direct PID; neither is exposed as signal authority.
#[derive(Clone, Copy, Eq, PartialEq)]
struct ProcessIdentityData {
    /// OS-random generation allocated before attempting the spawn.
    generation: [u8; 32],
    /// Direct PID from the newly created Child handle.
    pid: u32,
}
impl std::fmt::Debug for ProcessIdentityData {
    /// Keeps private PID/generation data out of derived capability debug output.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessIdentityData(..)")
    }
}
impl std::fmt::Debug for ProcessIdentity {
    /// Redacts raw launch/PID data while retaining the opaque correlation type.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProcessIdentity(..)")
    }
}
impl std::fmt::Debug for ReapedChildIdentity {
    /// Redacts raw launch/PID data; this value grants no release/signal capability.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ReapedChildIdentity(..)")
    }
}
impl ReapedChildIdentity {
    /// Checks whether actual direct-wait evidence matches the exact child which armed a resource guard.
    pub fn matches(&self, launched: &ProcessIdentity) -> bool {
        self.0 == launched.0
    }
}

/// One-time proof minted only by Execution after a successful direct-child wait.
#[derive(Debug)]
pub struct DirectChildReap {
    /// Exact reservation whose owned child was reaped.
    lease: AdmissionLease,
    /// Exact typed provider target or direct-job role attached before spawning.
    target: SpawnTarget,
    /// Actual direct child identity copied only after a successful wait.
    identity: ProcessIdentityData,
}

/// Receipts returned after one finite Workspace authority revocation reaches Execution.
#[derive(Debug)]
pub struct AuthorityDrainReceipt {
    /// Number of matching logical views removed before any physical reap decision.
    pub drained_views: usize,
    /// Per-backend decisions that preserve peers and borrowed ownership.
    pub backend_releases: Vec<BackendRelease>,
    /// Physical signal delivery/reap is separate work and remains unclaimed here.
    pub reap_uncertain: bool,
}

/// Tracks bounded logical provider views while keeping physical admission on the shared controller.
///
/// A queued request holds no resource reservation. An owned shared backend counts once as a
/// heavy backend; each attached view is counted separately. Owned shared forwarder processes
/// require their own central slots, bound once to these views by `take_forwarder_spawn_lease`.
/// Borrowed endpoints count as views only and are never candidates for a kill. The registry does
/// not infer Intelligence compatibility: its caller supplies a stable backend identity after that decision.
#[derive(Debug)]
pub struct ProviderLeaseRegistry {
    /// Immutable logical-view ceilings enforced before any attachment or pending record.
    limits: ProviderLeaseLimits,
    /// Next nonzero logical view identity.
    next_view: u64,
    /// Active views keyed by their opaque logical lease.
    views: BTreeMap<u64, ProviderView>,
    /// Physical backend state keyed by caller-supplied compatible identity.
    backends: BTreeMap<String, ProviderBackend>,
    /// Bounded request metadata retained only until its matching controller promotion settles.
    pending: BTreeMap<u64, PendingProviderLease>,
    /// One-time spawn capabilities keyed by their newly created owned backend's first view.
    spawnable: BTreeMap<u64, AdmissionLease>,
    /// Distinct process reservations bound once to currently attached shared forwarder views.
    forwarders: BTreeMap<u64, ProviderProcess>,
}

impl Drop for ProviderLeaseRegistry {
    /// Fences every unissued or outstanding launch capability when its logical owner disappears.
    fn drop(&mut self) {
        for backend in self.backends.values() {
            if let Some(process) = &backend.admission {
                process.revoke_pending();
            }
        }
        for forwarder in self.forwarders.values() {
            forwarder.revoke_pending();
        }
    }
}

/// Stores the scope that a direct Workspace revocation may fence.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderView {
    /// Backend to which this view is attached.
    backend: String,
    /// Complete authority copied at attachment time, including identity, incarnation, root and epoch.
    authority: WorkspaceAuthority,
}

/// Stores physical ownership, shared-view count, and the one central admission reservation.
#[derive(Clone, Debug)]
struct ProviderBackend {
    /// Ownership and sharing rule frozen when the backend is first attached.
    kind: ProviderBackendKind,
    /// Admission reservation retained only for an owned physical backend.
    admission: Option<ProviderProcess>,
    /// Number of logical views currently routed to this backend.
    views: usize,
    /// Last-view removal fences new attachments until direct-child reap completes.
    draining: bool,
}

/// Stores no resource reservation, only the identity needed to consume one exact promotion.
#[derive(Clone, Debug)]
struct PendingProviderLease {
    /// Backend identity requested before central admission queued it.
    backend: String,
    /// Ownership/share rule that must still be compatible at promotion time.
    kind: ProviderBackendKind,
    /// Authority scope which must still be current when the view attaches.
    authority: ProviderView,
}

impl ProviderLeaseRegistry {
    /// Creates an empty registry with finite logical-view ceilings.
    pub fn new(limits: ProviderLeaseLimits) -> Result<Self, ProviderLeaseError> {
        if limits.total_views == 0 || limits.per_backend_views == 0 {
            return Err(ProviderLeaseError::InvalidLimits);
        }
        Ok(Self {
            limits,
            next_view: 1,
            views: BTreeMap::new(),
            backends: BTreeMap::new(),
            pending: BTreeMap::new(),
            spawnable: BTreeMap::new(),
            forwarders: BTreeMap::new(),
        })
    }

    /// Requests a view under a current Workspace authority and centralized admission policy.
    ///
    /// Each attached view is one forwarder for accounting; only a newly created owned backend
    /// consumes an admission lease, so compatible shared peers do not double-count a heavy
    /// process. Queue tickets reserve neither a view nor a backend.
    pub fn request(
        &mut self,
        admission: &mut AdmissionController,
        owner: OwnerId,
        class: AdmissionClass,
        backend: impl Into<String>,
        kind: ProviderBackendKind,
        authority: &WorkspaceAuthority,
    ) -> ProviderLeaseAdmission {
        let backend = backend.into();
        if backend.is_empty() {
            return ProviderLeaseAdmission::Rejected(ProviderLeaseError::InvalidBackend);
        }
        if authority.incarnation().parse::<u64>().unwrap_or_default() == 0 {
            return ProviderLeaseAdmission::Rejected(ProviderLeaseError::InvalidAuthority);
        }
        if let Some(existing) = self.backends.get(&backend) {
            if existing.draining {
                return ProviderLeaseAdmission::Rejected(ProviderLeaseError::BackendDraining);
            }
            if existing.kind != kind {
                return ProviderLeaseAdmission::Rejected(ProviderLeaseError::InvalidBackend);
            }
            if existing.kind == ProviderBackendKind::OwnedExclusive {
                return ProviderLeaseAdmission::Rejected(ProviderLeaseError::ExclusiveInUse);
            }
            if !self.can_attach(&backend) {
                return ProviderLeaseAdmission::Rejected(ProviderLeaseError::ViewCapacity);
            }
            return ProviderLeaseAdmission::Granted(self.attach(backend, authority));
        }
        if !self.can_request(&backend) {
            return ProviderLeaseAdmission::Rejected(ProviderLeaseError::ViewCapacity);
        }
        let reserved = match kind {
            ProviderBackendKind::Borrowed => None,
            ProviderBackendKind::OwnedShared | ProviderBackendKind::OwnedExclusive => {
                match admission.submit(owner, class) {
                    Admission::Granted(lease) => Some(lease),
                    Admission::Queued(ticket) => {
                        self.pending.insert(
                            ticket.0,
                            PendingProviderLease {
                                backend,
                                kind,
                                authority: ProviderView::from_authority("", authority),
                            },
                        );
                        return ProviderLeaseAdmission::Queued(ticket);
                    }
                    Admission::Refused(error) => return ProviderLeaseAdmission::Refused(error),
                }
            }
        };
        let process = reserved.as_ref().map(|lease| {
            ProviderProcess::new(
                lease,
                SpawnTarget::Backend {
                    backend: backend.clone(),
                    view: ProviderViewLease(self.next_view),
                },
            )
        });
        self.backends.insert(
            backend.clone(),
            ProviderBackend {
                kind,
                admission: process,
                views: 0,
                draining: false,
            },
        );
        let view = self.attach(backend, authority);
        if let Some(lease) = reserved {
            admission.provider_slots.insert(lease.0);
            self.spawnable.insert(view.0, lease);
        }
        ProviderLeaseAdmission::Granted(view)
    }

    /// Cancels a pending provider request and its central ticket without releasing any resource.
    pub fn cancel_pending(
        &mut self,
        admission: &mut AdmissionController,
        ticket: QueueTicket,
    ) -> bool {
        self.pending.remove(&ticket.0).is_some() && admission.cancel_ticket(ticket)
    }

    /// Consumes one exact central promotion once, revalidating its stored scope and backend limits.
    pub fn promote(
        &mut self,
        admission: &mut AdmissionController,
        promotion: AdmissionPromotion,
        authority: &WorkspaceAuthority,
    ) -> Result<ProviderViewLease, ProviderLeaseError> {
        if promotion.lease.1 != admission.identity || promotion.ticket.1 != admission.identity {
            return Err(ProviderLeaseError::InvalidPromotion);
        }
        let pending = self
            .pending
            .remove(&promotion.ticket.0)
            .ok_or(ProviderLeaseError::InvalidPromotion)?;
        if pending.authority != ProviderView::from_authority("", authority)
            || !self.can_attach(&pending.backend)
            || self.backends.contains_key(&pending.backend)
        {
            admission
                .release(promotion.lease)
                .map_err(ProviderLeaseError::Admission)?;
            return Err(ProviderLeaseError::InvalidPromotion);
        }
        self.backends.insert(
            pending.backend.clone(),
            ProviderBackend {
                kind: pending.kind,
                admission: Some(ProviderProcess::new(
                    &promotion.lease,
                    SpawnTarget::Backend {
                        backend: pending.backend.clone(),
                        view: ProviderViewLease(self.next_view),
                    },
                )),
                views: 0,
                draining: false,
            },
        );
        let view = self.attach(pending.backend, authority);
        admission.provider_slots.insert(promotion.lease.0);
        self.spawnable.insert(view.0, promotion.lease);
        Ok(view)
    }

    /// Takes this view's one-time backend spawn capability bound to its complete admitted authority.
    pub fn take_spawn_lease(
        &mut self,
        view: ProviderViewLease,
    ) -> Result<ProviderSpawnLease, ProviderLeaseError> {
        let authority = self
            .views
            .get(&view.0)
            .ok_or(ProviderLeaseError::UnknownView)?;
        self.spawnable
            .remove(&view.0)
            .map(|lease| ProviderSpawnLease {
                view,
                lease,
                authority: authority.authority.clone(),
                backend: authority.backend.clone(),
                launched: self.backends[&authority.backend]
                    .admission
                    .as_ref()
                    .expect("owned backend")
                    .launched
                    .clone(),
                forwarder: false,
            })
            .ok_or(ProviderLeaseError::SpawnUnavailable)
    }

    /// Consumes one distinct direct reservation into a typed shared-forwarder launch capability.
    /// On refusal returns the unchanged linear lease with the error, allowing safe retry or cancellation.
    pub fn take_forwarder_spawn_lease(
        &mut self,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
    ) -> Result<ProviderForwarderSpawnLease, (ProviderLeaseError, AdmissionLease)> {
        let Some(scope) = self.views.get(&view.0) else {
            return Err((ProviderLeaseError::UnknownView, lease));
        };
        if scope.authority != *request.authority() {
            return Err((ProviderLeaseError::InvalidAuthority, lease));
        }
        if request.kind() != CommandKind::Provider
            || self.backends[&scope.backend].kind != ProviderBackendKind::OwnedShared
            || lease.1 != admission.identity
            || !admission.leases.contains_key(&lease.0)
            || admission.provider_slots.contains(&lease.0)
            || self.forwarders.contains_key(&view.0)
        {
            return Err((ProviderLeaseError::SpawnUnavailable, lease));
        }
        let process = ProviderProcess::new(
            &lease,
            SpawnTarget::Forwarder {
                backend: scope.backend.clone(),
                view,
            },
        );
        let launched = process.launched.clone();
        admission.provider_slots.insert(lease.0);
        self.forwarders.insert(view.0, process);
        Ok(ProviderForwarderSpawnLease {
            spawn: ProviderSpawnLease {
                view,
                lease,
                authority: scope.authority.clone(),
                backend: scope.backend.clone(),
                forwarder: true,
                launched,
            },
        })
    }

    /// Detaches one view; owned last views stay draining and never promote work before physical settlement.
    pub fn release(
        &mut self,
        view: ProviderViewLease,
    ) -> Result<BackendRelease, ProviderLeaseError> {
        let view_id = view.0;
        let view = self
            .views
            .remove(&view_id)
            .ok_or(ProviderLeaseError::UnknownView)?;
        self.spawnable.remove(&view_id);
        if let Some(process) = self.forwarders.get(&view_id) {
            process.revoke_pending();
        }
        let backend = self
            .backends
            .get_mut(&view.backend)
            .expect("view backend exists");
        if let Some(process) = &backend.admission
            && matches!(&process.target,SpawnTarget::Backend{view,..} if view.0==view_id)
        {
            process.revoke_pending();
        }
        backend.views -= 1;
        if backend.views != 0 {
            return Ok(BackendRelease::SharedPeerSurvives);
        }
        if backend.kind == ProviderBackendKind::Borrowed {
            self.backends.remove(&view.backend);
            return Ok(BackendRelease::BorrowedDetached);
        }
        backend.draining = true;
        Ok(BackendRelease::ReapOwned(BackendReapCapability {
            backend: view.backend,
            process: backend.admission.clone().expect("owned reservation"),
        }))
    }

    /// Consumes matching one-time draining/reap proofs, frees the slot, and returns promotions once.
    /// Wrong, stale, or already settled identities fail without freeing any reservation.
    pub fn complete_reap(
        &mut self,
        admission: &mut AdmissionController,
        capability: BackendReapCapability,
        proof: DirectChildReap,
    ) -> Result<Vec<AdmissionPromotion>, ProviderLeaseError> {
        let valid = self
            .backends
            .get(&capability.backend)
            .is_some_and(|backend| {
                backend.draining
                    && backend.views == 0
                    && backend.admission.as_ref().is_some_and(|process| {
                        process.key == capability.process.key
                            && process.target == capability.process.target
                            && Arc::ptr_eq(&process.launched, &capability.process.launched)
                    })
                    && capability.process.matches(&proof)
            });
        if !valid || !admission.leases.contains_key(&proof.lease.0) {
            return Err(ProviderLeaseError::InvalidReap);
        }
        let promotions = admission
            .release_settled(proof.lease)
            .map_err(ProviderLeaseError::Admission)?;
        self.backends.remove(&capability.backend);
        Ok(promotions)
    }

    /// Consumes a definite no-child proof once; only its exact backend/forwarder reservation is cancelled.
    /// Backend failure invalidates attached logical views because no listener ever existed; separately
    /// running forwarders retain their own reservations until their respective settlement proofs arrive.
    pub fn settle_never_started(
        &mut self,
        admission: &mut AdmissionController,
        settlement: SpawnNeverStarted,
    ) -> Result<Vec<AdmissionPromotion>, ProviderLeaseError> {
        match &settlement.target {
            SpawnTarget::Backend { backend, view } => {
                let valid = self.backends.get(backend).is_some_and(|entry| {
                    entry
                        .admission
                        .as_ref()
                        .is_some_and(|process| process.never_started(&settlement))
                }) && self
                    .views
                    .get(&view.0)
                    .is_none_or(|entry| &entry.backend == backend);
                if !valid {
                    return Err(ProviderLeaseError::InvalidReap);
                }
                let promotions = admission
                    .release_settled(settlement.lease)
                    .map_err(ProviderLeaseError::Admission)?;
                self.views.retain(|_, entry| &entry.backend != backend);
                self.backends.remove(backend);
                self.spawnable.remove(&view.0);
                Ok(promotions)
            }
            SpawnTarget::Forwarder { view, .. } => {
                if !self
                    .forwarders
                    .get(&view.0)
                    .is_some_and(|process| process.never_started(&settlement))
                {
                    return Err(ProviderLeaseError::InvalidReap);
                }
                let promotions = admission
                    .release_settled(settlement.lease)
                    .map_err(ProviderLeaseError::Admission)?;
                self.forwarders.remove(&view.0);
                Ok(promotions)
            }
            SpawnTarget::Ordinary => Err(ProviderLeaseError::InvalidReap),
        }
    }

    /// Cancels a backend whose one-time capability has never left the registry; spawned/issued capabilities fail.
    pub fn cancel_unstarted(
        &mut self,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
    ) -> Result<Vec<AdmissionPromotion>, ProviderLeaseError> {
        let proof = self.take_spawn_lease(view)?.cancel();
        self.settle_never_started(admission, proof)
    }

    /// Releases one distinct shared forwarder only after its direct child was reaped.
    /// Logical view/backend release does not settle this independently counted process slot.
    pub fn complete_forwarder_reap(
        &mut self,
        admission: &mut AdmissionController,
        proof: DirectChildReap,
    ) -> Result<Vec<AdmissionPromotion>, ProviderLeaseError> {
        let SpawnTarget::Forwarder { view, .. } = &proof.target else {
            return Err(ProviderLeaseError::InvalidReap);
        };
        let view = view.0;
        if !self
            .forwarders
            .get(&view)
            .is_some_and(|process| process.matches(&proof))
        {
            return Err(ProviderLeaseError::InvalidReap);
        }
        let promotions = admission
            .release_settled(proof.lease)
            .map_err(ProviderLeaseError::Admission)?;
        self.forwarders.remove(&view);
        Ok(promotions)
    }

    /// Fences views matching one finite Workspace revocation without touching surviving peers.
    ///
    /// The receipt distinguishes logical removal and pending reap capability from actual signal delivery
    /// and reaping, which remain uncertain until the owner reports direct-child evidence.
    pub fn revoke_authority(
        &mut self,
        revoked: &crate::workspace::authority::AuthorityRevoked,
    ) -> AuthorityDrainReceipt {
        let matching: Vec<_> = self
            .views
            .iter()
            .filter_map(|(id, view)| {
                (view.authority.worktree_id() == revoked.worktree().id()
                    && view.authority.incarnation().parse::<u64>().ok()
                        == Some(revoked.worktree().incarnation())
                    && view.authority.epoch() == revoked.old_epoch())
                .then_some(ProviderViewLease(*id))
            })
            .collect();
        let forwarders_pending = matching
            .iter()
            .any(|view| self.forwarders.contains_key(&view.0));
        let mut backend_releases = Vec::with_capacity(matching.len());

        for view in &matching {
            if let Ok(release) = self.release(*view) {
                backend_releases.push(release);
            }
        }
        AuthorityDrainReceipt {
            drained_views: matching.len(),
            reap_uncertain: forwarders_pending
                || backend_releases
                    .iter()
                    .any(|release| matches!(release, BackendRelease::ReapOwned(_))),
            backend_releases,
        }
    }

    /// Returns owned-backend and logical-view counts; `forwarder_count` reports separate process slots.
    pub fn counts(&self) -> (usize, usize) {
        (
            self.backends
                .values()
                .filter(|backend| backend.kind != ProviderBackendKind::Borrowed)
                .count(),
            self.views.len(),
        )
    }

    /// Returns separately admitted forwarder slots still bound to active registry views.
    pub fn forwarder_count(&self) -> usize {
        self.forwarders.len()
    }

    /// Reports exact bounded logical and reservation state without claiming readiness or liveness.
    pub fn inspect(&self) -> ProviderLeaseSnapshot {
        ProviderLeaseSnapshot {
            total_view_limit: self.limits.total_views,
            per_backend_view_limit: self.limits.per_backend_views,
            active_views: self.views.len(),
            pending_views: self.pending.len(),
            owned_backends: self
                .backends
                .values()
                .filter(|backend| backend.kind != ProviderBackendKind::Borrowed)
                .count(),
            draining_backends: self
                .backends
                .values()
                .filter(|backend| backend.draining)
                .count(),
            spawnable_backends: self.spawnable.len(),
            forwarders: self.forwarders.len(),
        }
    }

    /// Attaches one view after the caller has safely established/reused the backend.
    fn attach(&mut self, backend: String, authority: &WorkspaceAuthority) -> ProviderViewLease {
        let id = self.next_view;
        self.next_view = id
            .checked_add(1)
            .expect("provider view identifier exhausted");
        self.backends
            .get_mut(&backend)
            .expect("backend exists")
            .views += 1;
        self.views
            .insert(id, ProviderView::from_authority(backend, authority));
        ProviderViewLease(id)
    }

    /// Checks both finite forwarder ceilings before mutating backend/view state.
    fn can_attach(&self, backend: &str) -> bool {
        self.views.len() < self.limits.total_views
            && self
                .backends
                .get(backend)
                .is_none_or(|entry| entry.views < self.limits.per_backend_views)
    }

    /// Checks view ceilings while counting already queued requests that can later be promoted.
    fn can_request(&self, backend: &str) -> bool {
        self.views.len().saturating_add(self.pending.len()) < self.limits.total_views
            && self
                .views
                .values()
                .filter(|view| view.backend == backend)
                .count()
                .saturating_add(
                    self.pending
                        .values()
                        .filter(|pending| pending.backend == backend)
                        .count(),
                )
                < self.limits.per_backend_views
    }
}

impl ProviderView {
    /// Copies the current Execution authority into a revocation-comparable logical-view scope.
    fn from_authority(backend: impl Into<String>, authority: &WorkspaceAuthority) -> Self {
        Self {
            backend: backend.into(),
            authority: authority.clone(),
        }
    }
}

/// Holds bounded bytes from one drained stream and states whether the reader reached EOF.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedOutput {
    /// Bytes retained for diagnostics, capped independently from bytes drained from the pipe.
    pub bytes: Vec<u8>,
    /// Whether retained output omitted bytes after reaching the configured cap.
    pub truncated: bool,
    /// Total bytes consumed from the pipe before EOF or drain deadline.
    pub drained_bytes: u64,
    /// Whether the drainer observed EOF; false means output effects remain incomplete.
    pub complete: bool,
}

/// Identifies whether Execution owns a physical process or merely observes a peer endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EndpointOwnership {
    /// Execution launched this process and may signal its direct child/process group.
    Owned,
    /// A peer owns this endpoint; Execution must not send it a signal.
    Borrowed,
}

/// Represents the only cancellation facts Execution may honestly expose before and after reaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancellationEvidence {
    /// Whether TERM was successfully requested for the owned process group.
    pub term_requested: bool,
    /// Whether KILL was successfully requested after grace elapsed.
    pub kill_requested: bool,
}

/// States the unavoidable uncertainty about descendants that may have escaped a process group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescendantEvidence {
    /// Only the direct child has been reaped; descendants are not claimed terminated.
    Unverified,
}

/// Hard per-stream capture bound for completed ordinary/provider processes.
pub const MAX_CAPTURED_PROCESS_BYTES: usize = 8 * 1024 * 1024;

/// Immutable bounded process output with no lease or settlement capability.
#[derive(Clone, Debug)]
pub struct CapturedProcessEvidence {
    /// Present only on actual Execution direct-wait evidence; fixture constructors cannot mint it.
    reap_identity: Option<ReapedChildIdentity>,
    /// Direct-child status observed by the Execution owner.
    status: ExitStatus,
    /// TERM/KILL attempts, absent for ordinary completion.
    cancellation: Option<CancellationEvidence>,
    /// Bounded captured stdout, never a protocol child's reserved stdout.
    stdout: CapturedOutput,
    /// Bounded captured stderr.
    stderr: CapturedOutput,
    /// Direct-child exit does not prove descendant termination.
    descendants: DescendantEvidence,
    /// Monotonic interval from successful child spawn through bounded drain completion.
    elapsed: Duration,
}
impl CapturedProcessEvidence {
    /// Validates bounded fixture evidence without minting settlement rights.
    pub fn new(
        status: ExitStatus,
        cancellation: Option<CancellationEvidence>,
        stdout: CapturedOutput,
        stderr: CapturedOutput,
        descendants: DescendantEvidence,
    ) -> Result<Self, ProcessError> {
        use std::os::unix::process::ExitStatusExt;
        if (status.code().is_none() && status.signal().is_none())
            || [&stdout, &stderr].iter().any(|output| {
                output.bytes.len() > MAX_CAPTURED_PROCESS_BYTES
                    || output.drained_bytes < output.bytes.len() as u64
                    || (!output.truncated && output.drained_bytes != output.bytes.len() as u64)
            })
        {
            return Err(ProcessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bounded process evidence",
            )));
        }
        Ok(Self {
            reap_identity: None,
            status,
            cancellation,
            stdout,
            stderr,
            descendants,
            elapsed: Duration::ZERO,
        })
    }
    /// Returns non-authorizing actual-wait identity for exact scratch/resource lifetime correlation.
    pub fn reap_identity(&self) -> Option<&ReapedChildIdentity> {
        self.reap_identity.as_ref()
    }

    /// Returns observed direct-child status, without release rights.
    pub const fn status(&self) -> ExitStatus {
        self.status
    }
    /// Returns exact cancellation request evidence.
    pub const fn cancellation(&self) -> Option<CancellationEvidence> {
        self.cancellation
    }
    /// Returns immutable bounded stdout/drain metadata.
    pub fn stdout(&self) -> &CapturedOutput {
        &self.stdout
    }
    /// Returns immutable bounded stderr/drain metadata.
    pub fn stderr(&self) -> &CapturedOutput {
        &self.stderr
    }
    /// Returns conservative descendant evidence.
    pub const fn descendants(&self) -> DescendantEvidence {
        self.descendants
    }
    /// Returns the measured owned-child interval, or zero for fixture-only evidence.
    pub const fn elapsed(&self) -> Duration {
        self.elapsed
    }
}

/// Owns bounded immutable output separately from the one-time physical settlement capability.
#[derive(Debug)]
pub struct CompletedProcess {
    /// Safe to retain or pass to Workspace after the reservation is settled.
    pub evidence: CapturedProcessEvidence,
    /// Consume through ordinary release_reaped or provider complete_reap exactly once.
    pub settlement: DirectChildReap,
}

/// Reports failures in owned-child launch, signal delivery, wait, or output collection.
#[derive(Debug)]
pub enum ProcessError {
    /// The OS refused to launch or manage the direct child.
    Io(io::Error),
    /// A request failed its final active-binding admission check before any child launched.
    Request(RequestError),
    /// A process protocol requested stdout, so captured-output APIs cannot be used.
    ProtocolStdoutReserved,
    /// The bounded direct-child exit/reap wait expired; no release evidence was produced.
    ReapTimedOut,
    /// The cause is definite pre-child failure; its one-time proof may settle only that reservation.
    NeverStarted {
        cause: Box<ProcessError>,
        settlement: SpawnNeverStarted,
    },
}

impl From<io::Error> for ProcessError {
    /// Converts an OS process error without losing its error kind or message.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Keeps kill and drainer-abort responsibility alive across moves into a cancellable reap future.
///
/// This private guard owns only a child launched by Execution. It never releases admission or
/// claims reap/descendant completion. Tokio retains best-effort direct-child reaping on drop.
struct ChildOwnership {
    /// Private identity retained until direct wait can mint non-authorizing evidence.
    identity: ProcessIdentityData,
    /// Single launch correlation transferable to a scratch/resource owner.
    launch_identity: Option<ProcessIdentity>,
    /// Live direct-child handle; its ID becomes absent after wait has reaped it.
    child: Child,
    /// Cancellation handles for the child's exclusively owned output drain tasks.
    drainers: Vec<AbortHandle>,
    /// Cancellation requests retained across borrowed waits and supervisor handoff.
    cancellation: Option<CancellationEvidence>,
}

impl Drop for ChildOwnership {
    /// Requests immediate group/direct kill only while the child remains unreaped, then aborts drains.
    ///
    /// Signal failures leave cleanup uncertain. No async runtime is required to request the kills;
    /// this path yields no release evidence and never signals a PID retained after successful reap.
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            let _ = signal_group(pid, libc::SIGKILL);
            let _ = self.child.start_kill();
        }
        for drainer in &self.drainers {
            drainer.abort();
        }
    }
}

/// Owns one launched direct child, its admission lease, and exclusive stdout/stderr drain tasks.
///
/// Dropping the handle or a consuming reap future requests group/direct kill and aborts drains.
/// The admission reservation remains uncertain until separately verified recovery or explicit reap.
pub struct OwnedChild {
    /// Cancellation-safe direct-child and output-task ownership retained through reap/drain awaits.
    process: ChildOwnership,
    /// Sole stdout drainer retaining bounded diagnostic output.
    stdout: JoinHandle<io::Result<CapturedOutput>>,
    /// Sole stderr drainer retaining bounded diagnostic output.
    stderr: JoinHandle<io::Result<CapturedOutput>>,
    /// Admission slot retained until direct-child reap produces evidence.
    lease: AdmissionLease,
    /// Typed reservation target retained unchanged from launch through direct reap.
    target: SpawnTarget,
    /// Monotonic instant captured immediately after this owned child successfully spawned.
    started: Instant,
}

impl OwnedChild {
    /// Transfers this child's opaque launch identity once; subsequent calls return None.
    pub fn take_process_identity(&mut self) -> Option<ProcessIdentity> {
        self.process.launch_identity.take()
    }

    /// Starts a capture provider only through its unique registry capability and fresh binding use.
    pub fn spawn_from_provider_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        if let Err(error) = capability.validate_request(request) {
            return Err(capability.cancel().error(error));
        }
        let settlement = capability.cancel();
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(&request.command, settlement, output_cap)
    }

    /// Consumes a direct Git/job reservation; provider commands require a typed registry capability.
    /// Every pre-child failure returns the unique NeverStarted proof for the consumed reservation.
    pub fn spawn_captured(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let settlement = SpawnNeverStarted::ordinary(lease);
        if request.kind() == CommandKind::Provider {
            return Err(settlement.error(provider_capability_required()));
        }
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(&request.command, settlement, output_cap)
    }

    /// Launches from one linear reservation, returning it only on definite pre-child failure.
    fn spawn_parts(
        command: &ControlledCommand,
        settlement: SpawnNeverStarted,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let (mut child, identity) = match launch_child(command, &settlement, output_cap, false) {
            Ok(child) => child,
            Err(error) => return Err(settlement.error(error)),
        };
        let stdout = tokio::spawn(drain(
            child.stdout.take().expect("piped stdout"),
            output_cap,
        ));
        let stderr = tokio::spawn(drain(
            child.stderr.take().expect("piped stderr"),
            output_cap,
        ));
        Ok(Self {
            process: ChildOwnership {
                identity,
                launch_identity: Some(ProcessIdentity(identity)),
                child,
                drainers: vec![stdout.abort_handle(), stderr.abort_handle()],
                cancellation: None,
            },
            stdout,
            stderr,
            lease: settlement.lease,
            target: settlement.target,
            started: Instant::now(),
        })
    }

    /// Borrows cancellation so timeout or future cancellation retains the owned handle for supervisor handoff.
    /// Successful exit still requires consuming reap to produce settlement/output; borrowed endpoints
    /// cannot enter this API. Both waits are bounded and previous signal evidence is preserved.
    pub async fn cancel_bounded(
        &mut self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ExitStatus, ProcessError> {
        cancel_owned(&mut self.process, grace, deadline).await
    }

    /// Requests bounded TERM/KILL, reaps the direct child, and completes bounded stream collection.
    /// Timeout/drop yields no settlement proof; retain the handle with cancel_bounded when scratch or
    /// other resources must survive transfer to a supervised reaper.
    pub async fn cancel_and_reap(
        mut self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<CompletedProcess, ProcessError> {
        let status = self.cancel_bounded(grace, output_deadline).await?;
        let cancellation = self.process.cancellation;
        self.finish(status, cancellation, output_deadline).await
    }

    /// Peeks whether the direct child has already exited, without blocking, signaling, or consuming
    /// ownership. `Ok(None)` means the process still appears alive; a definite `Ok(Some(status))` is
    /// stable and repeatable once observed, since Tokio retains the reaped status internally. Callers
    /// still must route through `cancel_bounded`/`cancel_and_reap`/`wait` to drain output and release
    /// admission. Tokio may reap the OS child here; this observation never produces Execution's
    /// settlement proof or releases the logical ownership.
    pub fn try_exit_status(&mut self) -> io::Result<Option<ExitStatus>> {
        self.process.child.try_wait()
    }

    /// Borrows the direct child for a bounded, cancellation-safe wait without consuming ownership.
    /// A successful wait is retained by Tokio; consuming reap still owns output/proof completion.
    pub async fn wait(&mut self, deadline: Duration) -> Result<ExitStatus, ProcessError> {
        validate_reap_deadline(deadline)?;
        timeout(deadline, self.process.child.wait())
            .await
            .map_err(|_| ProcessError::ReapTimedOut)?
            .map_err(ProcessError::Io)
    }

    /// Reaps under separate bounded exit/drain deadlines; timeout yields no settlement proof.
    pub async fn reap(
        mut self,
        exit_deadline: Duration,
        output_deadline: Duration,
    ) -> Result<CompletedProcess, ProcessError> {
        let status = self.wait(exit_deadline).await?;
        let cancellation = self.process.cancellation;
        self.finish(status, cancellation, output_deadline).await
    }

    /// Completes drain tasks within a separate deadline after the child is already reaped.
    async fn finish(
        self,
        status: ExitStatus,
        cancellation: Option<CancellationEvidence>,
        output_deadline: Duration,
    ) -> Result<CompletedProcess, ProcessError> {
        let reap_identity = ReapedChildIdentity(self.process.identity);
        let _ownership = self.process;
        let stdout = collect_drain(self.stdout, output_deadline).await;
        let stderr = collect_drain(self.stderr, output_deadline).await;
        Ok(CompletedProcess {
            evidence: CapturedProcessEvidence {
                reap_identity: Some(reap_identity.clone()),
                status,
                cancellation,
                stdout,
                stderr,
                descendants: DescendantEvidence::Unverified,
                elapsed: self.started.elapsed(),
            },
            settlement: DirectChildReap {
                lease: self.lease,
                target: self.target,
                identity: reap_identity.0,
            },
        })
    }
}

/// Direct protocol-child reap evidence, including cancellation and conservative descendant status.
#[derive(Debug)]
pub struct ReapedProtocolProcess {
    /// Exit status returned only after the direct child was waited successfully.
    pub status: ExitStatus,
    /// TERM/KILL delivery attempts, absent for an ordinary exit wait.
    pub cancellation: Option<CancellationEvidence>,
    /// Exclusive bounded stderr drain; protocol stdout is never captured by Execution.
    pub stderr: CapturedOutput,
    /// Direct-child reap never proves complete descendant termination.
    pub descendants: DescendantEvidence,
    /// One-time direct-child settlement proof required by provider accounting.
    pub proof: DirectChildReap,
}

/// Opaque identity-bound result of waiting one protocol child without consuming its owner.
pub(crate) struct WaitedProtocolChild {
    /// Exit status returned by the direct wait.
    status: ExitStatus,
    /// Exact launch identity of the child that produced `status`.
    identity: ProcessIdentityData,
}

impl WaitedProtocolChild {
    /// Returns whether the waited direct child reported successful exit.
    pub(crate) fn success(&self) -> bool {
        self.status.success()
    }
}

/// Gives Intelligence sole ownership of a protocol child's stdin/stdout while Execution drains stderr.
///
/// Dropping the handle or consuming reap future requests owned group/direct kill and aborts stderr;
/// it produces no exit evidence and does not free the admission reservation.
pub struct OwnedProtocolChild {
    /// The sole stdin writer for the selected protocol client; `None` once a live session took it.
    pub stdin: Option<ChildStdin>,
    /// The sole stdout reader for the selected protocol client; Execution never drains it.
    pub stdout: Option<ChildStdout>,
    /// Cancellation-safe child ownership retained after the protocol pipes move or close.
    process: ChildOwnership,
    /// Sole stderr drainer; stdout is deliberately unavailable to Execution.
    stderr: JoinHandle<io::Result<CapturedOutput>>,
    /// Admission slot retained until direct protocol-child reap.
    lease: AdmissionLease,
    /// Typed reservation target retained unchanged from launch through direct reap.
    target: SpawnTarget,
}

impl OwnedProtocolChild {
    /// Moves both protocol pipes out once, for a driver that must own them; later calls return None.
    pub fn take_pipes(&mut self) -> Option<(ChildStdin, ChildStdout)> {
        Some((self.stdin.take()?, self.stdout.take()?))
    }

    /// Transfers this child's opaque launch identity once; subsequent calls return None.
    pub fn take_process_identity(&mut self) -> Option<ProcessIdentity> {
        self.process.launch_identity.take()
    }

    /// Starts an owned protocol child by consuming its authority-bound one-time backend capability.
    /// Authority mismatch returns an error before consuming active use or spawning a process.
    pub fn spawn_from_provider_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        if let Err(error) = capability.validate_request(request) {
            return Err(capability.cancel().error(error));
        }
        let settlement = capability.cancel();
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(request, settlement, output_cap)
    }

    /// Starts one forwarder using its distinct process slot and exact registry-view authority.
    /// The consumed capability cannot be reused; reap returns this forwarder's slot, never its listener's.
    pub fn spawn_from_forwarder_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderForwarderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        Self::spawn_from_provider_lease(request, capability.spawn, active_use, output_cap)
    }

    /// Consumes a direct Git/job reservation with exclusive protocol stdout; raw providers are refused.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let settlement = SpawnNeverStarted::ordinary(lease);
        if request.kind() == CommandKind::Provider {
            return Err(settlement.error(provider_capability_required()));
        }
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(request, settlement, output_cap)
    }

    /// Launches one typed or direct reservation while retaining its target and exact child identity.
    fn spawn_parts(
        request: &ValidatedExecutionRequest,
        settlement: SpawnNeverStarted,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let (mut child, identity) =
            match launch_child(&request.command, &settlement, output_cap, true) {
                Ok(child) => child,
                Err(error) => return Err(settlement.error(error)),
            };
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = tokio::spawn(drain(
            child.stderr.take().expect("piped stderr"),
            output_cap,
        ));
        Ok(Self {
            stdin: Some(stdin),
            stdout: Some(stdout),
            process: ChildOwnership {
                identity,
                launch_identity: Some(ProcessIdentity(identity)),
                child,
                drainers: vec![stderr.abort_handle()],
                cancellation: None,
            },
            stderr,
            lease: settlement.lease,
            target: settlement.target,
        })
    }

    /// Returns ordinary direct-child reap evidence under a positive deadline of at most 60 seconds.
    pub async fn reap(self, deadline: Duration) -> Result<ReapedProtocolProcess, ProcessError> {
        let mut this = self;
        let status = this.wait_for_exit(deadline).await?;
        this.finish_reap(status, deadline).await
    }

    /// Borrows bounded cancellation while keeping ownership available for a supervised reaper handoff.
    pub async fn cancel_bounded(
        &mut self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ExitStatus, ProcessError> {
        cancel_owned(&mut self.process, grace, deadline).await
    }

    /// Closes pipes and requests bounded TERM/KILL; only successful direct wait produces proof.
    /// A timeout/drop retains uncertain admission; use cancel_bounded to keep ownership for handoff.
    pub async fn cancel_and_reap(
        mut self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, ProcessError> {
        self.cancel_bounded(grace, deadline).await?;
        self.reap(deadline).await
    }

    /// Performs the fixed abnormal TypeScript cleanup sequence while retaining direct-child ownership.
    ///
    /// This crate-private path requires positive `grace` and `deadline` values of at most 60
    /// seconds. It requests group TERM, waits the complete grace without polling or reaping the
    /// child, then requests group KILL and a direct-child kill before the
    /// sole direct wait. Signal failures do not skip later cleanup steps; only a successful bounded
    /// wait returns direct-child settlement, always with unverified descendant evidence. It must
    /// never be used for normal TypeScript shutdown, whose successful path calls [`Self::reap`]
    /// without requesting a signal.
    pub(crate) async fn terminate_typescript_abnormally(
        mut self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, ProcessError> {
        let (status, evidence) =
            terminate_typescript_child_abnormally(&mut self.process.child, grace, deadline).await?;
        self.process.cancellation = Some(evidence);
        let waited = WaitedProtocolChild {
            status,
            identity: self.process.identity,
        };
        self.finish_reap(waited, deadline).await
    }

    /// Waits for the direct bridge child without consuming ownership or requesting a signal.
    ///
    /// A timeout leaves this handle available for the ordered abnormal TypeScript cleanup path.
    pub(crate) async fn wait_for_exit(
        &mut self,
        deadline: Duration,
    ) -> Result<WaitedProtocolChild, ProcessError> {
        validate_reap_deadline(deadline)?;
        let status = timeout(deadline, self.process.child.wait())
            .await
            .map_err(|_| ProcessError::ReapTimedOut)?
            .map_err(ProcessError::Io)?;
        Ok(WaitedProtocolChild {
            status,
            identity: self.process.identity,
        })
    }

    /// Consumes an already reaped direct child into its sole accounting proof and stderr evidence.
    pub(crate) async fn finish_reap(
        self,
        waited: WaitedProtocolChild,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, ProcessError> {
        if waited.identity != self.process.identity {
            return Err(ProcessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "protocol wait identity does not match owned child",
            )));
        }
        let stderr = collect_drain(self.stderr, deadline).await;
        Ok(ReapedProtocolProcess {
            status: waited.status,
            cancellation: self.process.cancellation,
            stderr,
            descendants: DescendantEvidence::Unverified,
            proof: DirectChildReap {
                lease: self.lease,
                target: self.target,
                identity: self.process.identity,
            },
        })
    }
}

/// Applies the one fixed abnormal TypeScript signal order to an unreaped direct child.
///
/// The caller retains the child handle. Positive `grace` and `deadline` values may not exceed 60
/// seconds. This function requests group TERM, sleeps the complete grace without polling or
/// reaping, requests group KILL and direct-child kill, then performs the sole direct wait. It never
/// signals after that wait and makes no descendant-settlement or process-group-containment claim.
pub(crate) async fn terminate_typescript_child_abnormally(
    child: &mut Child,
    grace: Duration,
    deadline: Duration,
) -> Result<(ExitStatus, CancellationEvidence), ProcessError> {
    validate_reap_deadline(deadline)?;
    if grace.is_zero() || grace > Duration::from_secs(60) {
        return Err(ProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid TypeScript cleanup grace",
        )));
    }
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("owned TypeScript child has no live PID"))?;
    let mut evidence = CancellationEvidence {
        term_requested: signal_group(pid, libc::SIGTERM).is_ok(),
        kill_requested: false,
    };
    tokio::time::sleep(grace).await;
    let group_kill = signal_group(pid, libc::SIGKILL).is_ok();
    let direct_kill = child.start_kill().is_ok();
    evidence.kill_requested = group_kill || direct_kill;
    let status = timeout(deadline, child.wait())
        .await
        .map_err(|_| ProcessError::ReapTimedOut)??;
    Ok((status, evidence))
}

/// Refuses using direct-job admission to launch an Intelligence provider process.
fn provider_capability_required() -> ProcessError {
    ProcessError::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "provider spawn requires a typed registry capability",
    ))
}

/// Serializes provider revocation through physical spawn and records that exact direct child once.
fn launch_child(
    command: &ControlledCommand,
    settlement: &SpawnNeverStarted,
    output_cap: usize,
    protocol: bool,
) -> Result<(Child, ProcessIdentityData), ProcessError> {
    let mut launch = settlement
        .launched
        .as_ref()
        .map(|state| {
            state
                .lock()
                .map_err(|_| io::Error::other("provider launch state unavailable"))
        })
        .transpose()?;
    if launch
        .as_ref()
        .is_some_and(|state| !matches!(**state, ProviderLaunchState::Pending))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "provider launch capability was revoked or consumed",
        )
        .into());
    }
    validate_output_cap(output_cap)?;
    if executable_identity(&command.program).map_err(ProcessError::Request)?
        != command.program_identity
    {
        return Err(ProcessError::Request(RequestError::ExecutableUnavailable));
    }
    let mut process = build_command(command);
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(file) = &command.stdin_file {
        process.stdin(Stdio::from(file.try_clone()?));
    } else if protocol {
        process.stdin(Stdio::piped());
    }
    configure_process_group(&mut process);
    let generation = launch_generation()?;
    let child = process.spawn()?;
    let identity = ProcessIdentityData {
        generation,
        pid: child.id().expect("new child PID"),
    };
    if let Some(state) = launch.as_mut() {
        **state = ProviderLaunchState::Launched(identity);
    }
    Ok((child, identity))
}

/// Requests signals only for this unreaped owned child and retains evidence on borrowed timeout/cancellation.
async fn cancel_owned(
    process: &mut ChildOwnership,
    grace: Duration,
    deadline: Duration,
) -> Result<ExitStatus, ProcessError> {
    validate_reap_deadline(deadline)?;
    if grace > Duration::from_secs(60) {
        return Err(ProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owned process grace exceeds limit",
        )));
    }
    if let Some(status) = process.child.try_wait()? {
        return Ok(status);
    }
    let pid = process
        .child
        .id()
        .ok_or_else(|| io::Error::other("owned child has no live PID"))?;
    let mut evidence = process.cancellation.unwrap_or(CancellationEvidence {
        term_requested: false,
        kill_requested: false,
    });
    evidence.term_requested |= signal_group(pid, libc::SIGTERM).is_ok();
    process.cancellation = Some(evidence);
    match timeout(grace, process.child.wait()).await {
        Ok(status) => status.map_err(ProcessError::Io),
        Err(_) => {
            evidence.kill_requested |= signal_group(pid, libc::SIGKILL).is_ok();
            process.cancellation = Some(evidence);
            process.child.start_kill()?;
            timeout(deadline, process.child.wait())
                .await
                .map_err(|_| ProcessError::ReapTimedOut)?
                .map_err(ProcessError::Io)
        }
    }
}

/// Reads a private random launch generation before an OS child can exist.
fn launch_generation() -> Result<[u8; 32], ProcessError> {
    use std::io::Read;
    let mut generation = [0; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut generation)?;
    Ok(generation)
}

/// Refuses over-limit capture policy before an OS child can be created; zero retains no bytes.
fn validate_output_cap(cap: usize) -> Result<(), ProcessError> {
    if cap > MAX_CAPTURED_PROCESS_BYTES {
        Err(ProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process capture cap exceeds limit",
        )))
    } else {
        Ok(())
    }
}

/// Rejects deadlines that could leave a protocol process wait effectively unbounded.
fn validate_reap_deadline(deadline: Duration) -> Result<(), ProcessError> {
    if deadline.is_zero() || deadline > Duration::from_secs(60) {
        Err(ProcessError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid protocol reap deadline",
        )))
    } else {
        Ok(())
    }
}

/// Represents an endpoint owned by another module/provider and therefore ineligible for signalling.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BorrowedEndpoint {
    /// Observed provider identity for accounting only; it never conveys signal permission.
    identity: String,
}

impl BorrowedEndpoint {
    /// Records a nonempty observed endpoint identity without gaining a process-control capability.
    pub fn observe(identity: impl Into<String>) -> Result<Self, ProcessError> {
        let identity = identity.into();
        if identity.is_empty() {
            return Err(ProcessError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "borrowed endpoint identity is empty",
            )));
        }
        Ok(Self { identity })
    }

    /// Returns the observed identity for accounting without treating it as signal authority.
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Refuses termination because borrowed endpoints remain owned by their provider.
    pub const fn cancel(&self) -> EndpointOwnership {
        EndpointOwnership::Borrowed
    }
}

/// Builds the owned child's process: the exact program, argv, cwd, and cleared environment.
fn build_command(command: &ControlledCommand) -> Command {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .current_dir(&command.cwd)
        .env_clear()
        .envs(&command.env);
    process
}

/// Opens and hashes one regular executable, binding later launch to the same path object and bytes.
fn executable_identity(path: &Path) -> Result<ExecutableIdentity, RequestError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let canonical_path =
        std::fs::canonicalize(path).map_err(|_| RequestError::ExecutableUnavailable)?;
    let mut file = std::fs::File::open(path).map_err(|_| RequestError::ExecutableUnavailable)?;
    let metadata = file
        .metadata()
        .map_err(|_| RequestError::ExecutableUnavailable)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(RequestError::ExecutableUnavailable);
    }
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| RequestError::ExecutableUnavailable)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(ExecutableIdentity {
        canonical_path,
        device: metadata.dev(),
        inode: metadata.ino(),
        digest: hasher.finalize(),
    })
}

/// Returns the measured executable digest used by trusted provider compatibility identities.
pub(crate) fn measured_executable_digest(path: &Path) -> Result<blake3::Hash, RequestError> {
    executable_identity(path).map(|identity| identity.digest)
}

/// Configures an owned Unix group and direct-child kill-on-drop as a launch/setup failure fallback.
fn configure_process_group(command: &mut Command) {
    command.kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(not(unix))]
    let _ = command;
}

/// Sends a signal to the owned process group and returns only OS delivery acknowledgement.
fn signal_group(pid: u32, signal: libc::c_int) -> io::Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, signal);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process groups are unavailable on this platform",
        ))
    }
}

/// Drains every supplied byte while retaining only the configured bounded prefix.
async fn drain<R>(mut reader: R, cap: usize) -> io::Result<CapturedOutput>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(cap.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut drained_bytes = 0_u64;
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(CapturedOutput {
                bytes,
                truncated,
                drained_bytes,
                complete: true,
            });
        }
        drained_bytes = drained_bytes.saturating_add(read as u64);
        let remaining = cap.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        truncated |= retained < read;
    }
}

/// Joins one drain task without allowing a hung inherited pipe to delay direct-child reaping.
async fn collect_drain(
    mut task: JoinHandle<io::Result<CapturedOutput>>,
    deadline: Duration,
) -> CapturedOutput {
    match timeout(deadline, &mut task).await {
        Ok(Ok(Ok(output))) => output,
        _ => {
            task.abort();
            CapturedOutput {
                bytes: Vec::new(),
                truncated: true,
                drained_bytes: 0,
                complete: false,
            }
        }
    }
}

/// Rejects relative, current-directory, and parent-directory paths before policy comparison.
fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
}

/// Returns a conservative byte size for argv enforcement without decoding platform strings.
fn os_bytes(value: &OsString) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        value.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        value.to_string_lossy().len()
    }
}

/// Returns the fixed argument vector for one Git discovery query run by an inherited-sandbox child.
///
/// This is the same fixed argv the managed Codex path builds, exposed for a caller that already
/// runs inside a sandbox it did not create. It selects nothing from model input: the query is a
/// closed enum and `candidate` is the trusted launcher-configured path.
pub fn inherited_git_arguments(query: GitDiscoveryQuery, candidate: &Path) -> Vec<OsString> {
    let mut args = vec![OsString::from("-C"), candidate.as_os_str().to_owned()];
    match query {
        GitDiscoveryQuery::ShowTopLevel => args.extend([
            OsString::from("rev-parse"),
            OsString::from("--show-toplevel"),
        ]),
        GitDiscoveryQuery::GitCommonDir => args.extend([
            OsString::from("rev-parse"),
            OsString::from("--path-format=absolute"),
            OsString::from("--git-common-dir"),
        ]),
        GitDiscoveryQuery::WorktreeListPorcelainZ => args.extend([
            OsString::from("worktree"),
            OsString::from("list"),
            OsString::from("--porcelain"),
            OsString::from("-z"),
        ]),
    }
    args
}

/// Reports one settled inherited-sandbox child without exposing OS or provider payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InheritedChildOutcome {
    /// Whether the child exited successfully.
    pub success: bool,
    /// Captured stdout, truncated at the caller's byte cap.
    pub stdout: Vec<u8>,
    /// True when the cap discarded trailing output.
    pub truncated: bool,
}

/// Returns one inherited controlled child's exact process evidence and scratch binding token.
pub struct InheritedCapturedOutcome {
    /// Bounded stdout/stderr plus actual direct-wait identity.
    pub evidence: CapturedProcessEvidence,
    /// One-time launch identity consumed by a Workspace snapshot intent.
    pub launch_identity: ProcessIdentity,
}

/// Reports why one inherited-sandbox child produced no usable outcome, and what is known about its
/// physical settlement.
///
/// The three cases are deliberately distinct because they charge child accounting differently. A
/// child that never existed must not be counted as spawned; a child positively killed and reaped is
/// a settled child that merely failed; a child whose cleanup could not be observed must keep the
/// whole operation uncertain and quarantined.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InheritedChildFailure {
    /// The child provably never started, so it was never a child to reap or to count as spawned.
    NeverStarted,
    /// The child outlived its deadline and was killed, and that kill *and* its wait were both
    /// positively observed to succeed. The child is settled and reaped; only its work failed.
    Reaped,
    /// The child started but its physical cleanup could not be observed. The caller must report it
    /// as unreaped; this is never equivalent to an ordinary failed execution.
    Unsettled,
}

/// Runs one Workspace-built controlled command inside the caller's inherited sandbox.
///
/// The surrounding Claude ticket already owns the daemon's shared admission lease, so this path
/// deliberately consumes no second lease. It still rechecks executable identity, clears the
/// environment, bounds both streams, owns the process group, and returns actual direct-wait proof
/// that a snapshot scratch directory can correlate before cleanup.
pub async fn run_inherited_controlled_child(
    command: &ControlledCommand,
    output_cap: usize,
    deadline: std::time::Duration,
) -> Result<InheritedCapturedOutcome, InheritedChildFailure> {
    if output_cap == 0 || output_cap > MAX_CAPTURED_PROCESS_BYTES || deadline.is_zero() {
        return Err(InheritedChildFailure::NeverStarted);
    }
    let mut process = command
        .inherited_process()
        .map_err(|_| InheritedChildFailure::NeverStarted)?;
    process.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(file) = &command.stdin_file {
        process.stdin(Stdio::from(
            file.try_clone()
                .map_err(|_| InheritedChildFailure::NeverStarted)?,
        ));
    } else {
        process.stdin(Stdio::null());
    }
    let generation = launch_generation().map_err(|_| InheritedChildFailure::NeverStarted)?;
    let mut child = process
        .spawn()
        .map_err(|_| InheritedChildFailure::NeverStarted)?;
    let identity = ProcessIdentityData {
        generation,
        pid: child.id().expect("new child PID"),
    };
    let Some(stdout) = child.stdout.take() else {
        return Err(
            if child.kill().await.is_ok() && child.wait().await.is_ok() {
                InheritedChildFailure::Reaped
            } else {
                InheritedChildFailure::Unsettled
            },
        );
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(
            if child.kill().await.is_ok() && child.wait().await.is_ok() {
                InheritedChildFailure::Reaped
            } else {
                InheritedChildFailure::Unsettled
            },
        );
    };
    let stdout = tokio::spawn(drain(stdout, output_cap));
    let stderr = tokio::spawn(drain(stderr, output_cap));
    let status = match timeout(deadline.min(Duration::from_secs(60)), child.wait()).await {
        Ok(Ok(status)) => status,
        _ => {
            stdout.abort();
            stderr.abort();
            return Err(
                if child.kill().await.is_ok() && child.wait().await.is_ok() {
                    InheritedChildFailure::Reaped
                } else {
                    InheritedChildFailure::Unsettled
                },
            );
        }
    };
    let (stdout, stderr) = tokio::join!(
        collect_drain(stdout, Duration::from_secs(1)),
        collect_drain(stderr, Duration::from_secs(1)),
    );
    Ok(InheritedCapturedOutcome {
        evidence: CapturedProcessEvidence {
            reap_identity: Some(ReapedChildIdentity(identity)),
            status,
            cancellation: None,
            stdout,
            stderr,
            descendants: DescendantEvidence::Unverified,
            elapsed: Duration::ZERO,
        },
        launch_identity: ProcessIdentity(identity),
    })
}

/// Runs one direct child that inherits its caller's existing sandbox, then reaps it.
///
/// This is the minimum distinct boundary for a caller that is *itself already inside* the sandbox
/// it intends the child to run under; containment here is inherited from that calling process and
/// is asserted by the operator profile, never observed or attested by this function or by the
/// daemon. It is deliberately separate from the managed Codex path: it takes no
/// `ObservedSandboxState`, mints no permit, consumes no admission lease, and uses no wrapper
/// executable, so no synthetic sandbox observation can ever reach Codex Execution through it.
///
/// `program` must be an accepted absolute executable and `args` a fixed argument vector; neither
/// is ever derived from model input. Output is capped at `output_cap` bytes per the caller's
/// budget and the child is killed and reaped if it outlives `deadline`.
///
/// On failure returns the exact [`InheritedChildFailure`] case, so the caller can distinguish a
/// child that never existed, one positively reaped after a kill, and one whose cleanup is unproven.
pub async fn run_inherited_child(
    program: &Path,
    args: Vec<OsString>,
    cwd: &Path,
    output_cap: usize,
    deadline: std::time::Duration,
) -> Result<InheritedChildOutcome, InheritedChildFailure> {
    if !is_normal_absolute(program) || output_cap == 0 {
        return Err(InheritedChildFailure::NeverStarted);
    }
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|_| InheritedChildFailure::NeverStarted)?;
    let Some(mut stdout) = child.stdout.take() else {
        // The child exists, so its settlement must be positively observed like any other kill path.
        return Err(
            match child.kill().await.is_ok() && child.wait().await.is_ok() {
                true => InheritedChildFailure::Reaped,
                false => InheritedChildFailure::Unsettled,
            },
        );
    };
    let capture = async {
        let mut buffer = Vec::new();
        let _ = tokio::io::AsyncReadExt::take(&mut stdout, output_cap as u64 + 1)
            .read_to_end(&mut buffer)
            .await;
        buffer
    };
    let settled = tokio::time::timeout(deadline, async {
        let buffer = capture.await;
        child.wait().await.map(|status| (status, buffer))
    })
    .await;
    match settled {
        Ok(Ok((status, mut buffer))) => {
            let truncated = buffer.len() > output_cap;
            buffer.truncate(output_cap);
            Ok(InheritedChildOutcome {
                success: status.success(),
                stdout: buffer,
                truncated,
            })
        }
        // Started but unsettled: kill, then claim settlement only where the reap actually succeeded.
        // A positively observed kill+wait is a real reap and is recorded as such; anything less
        // stays uncertain, which is a stronger condition than an ordinary failed execution.
        _ => Err(
            match child.kill().await.is_ok() && child.wait().await.is_ok() {
                true => InheritedChildFailure::Reaped,
                false => InheritedChildFailure::Unsettled,
            },
        ),
    }
}

pub mod seatbelt;

#[cfg(test)]
/// Execution unit tests and their shared portable sandbox fixture loader.
pub(crate) mod linear_tests;
