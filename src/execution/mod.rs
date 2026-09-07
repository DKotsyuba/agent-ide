//! Admission and owned-child supervision for controlled Agent IDE effects.
//!
//! This module deliberately accepts only peer-validated authority and controlled commands. It
//! preserves a managed Codex sandbox state as opaque JSON and refuses profiles whose filesystem
//! authority cannot be replayed exactly.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    time::Duration,
};

use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    task::JoinHandle,
    time::timeout,
};

/// Classifies the host permission profile whose complete state accompanies a request.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ProfileClass {
    /// A managed profile is replayed by `codex sandbox --sandbox-state-json`.
    Managed,
    /// A disabled profile has no outer sandbox and requires explicit local acceptance.
    Disabled,
}

/// Reports why opaque host sandbox state cannot authorize an owned child.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SandboxStateError {
    /// The host did not provide the experimental state payload.
    Missing,
    /// The payload is not an object with the required fields.
    Malformed,
    /// An external profile has no concrete filesystem scope to replay.
    ExternalUnsupported,
    /// The profile class is not one Execution knows how to preserve.
    UnsupportedProfile,
}

/// Names the tested host mechanism/profile class that Execution supports across worktrees.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionProfileTemplate {
    /// Stable Execution-owned profile name, never an invocation input.
    id: String,
    /// Execution-owned revision of the tested mechanism and profile shape.
    version: u32,
    /// Host profile class covered by this template.
    class: ProfileClass,
    /// Normalized opaque-state shape proven by this template without pinning one worktree path.
    shape_digest: blake3::Hash,
}

/// Holds Execution-owned templates whose real evidence permits physical effects.
#[derive(Clone, Debug)]
pub struct ExecutionProfileCatalog {
    /// Exactly one current tested template per host profile class.
    templates: BTreeMap<ProfileClass, ExecutionProfileTemplate>,
}

/// Correlates one invocation's full opaque state with its supporting profile template.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionProfilePermit {
    /// Tested template that authorizes this class of execution, not a fixture directory.
    template: ExecutionProfileTemplate,
    /// Full-state digest retained only for operation evidence and later lookup correlation.
    state_digest: blake3::Hash,
}

/// Holds an entire host-provided sandbox state without expanding its roots or rewriting policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSandboxState {
    /// Complete unmodified state value passed as one Codex CLI argument for managed execution.
    raw: Value,
    /// Accepted high-level profile class derived from the state envelope.
    class: ProfileClass,
    /// Host-selected sandbox cwd that the controlled command must preserve exactly.
    cwd: PathBuf,
}

impl HostSandboxState {
    /// Validates the envelope shape while retaining the supplied state for exact replay.
    ///
    /// `raw` must be the complete value advertised by `codex/sandbox-state-meta`; a missing,
    /// external, proxy, or unknown profile is rejected. This function does not infer authority
    /// from a path or expand special path syntax.
    pub fn parse(raw: Option<Value>) -> Result<Self, SandboxStateError> {
        let raw = raw.ok_or(SandboxStateError::Missing)?;
        let object = raw.as_object().ok_or(SandboxStateError::Malformed)?;
        let permission_profile = object
            .get("permissionProfile")
            .and_then(Value::as_object)
            .ok_or(SandboxStateError::Malformed)?;
        let profile_type = permission_profile
            .get("type")
            .and_then(Value::as_str)
            .ok_or(SandboxStateError::Malformed)?;
        let cwd = object
            .get("sandboxCwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from)
            .ok_or(SandboxStateError::Malformed)?;
        let class = match profile_type {
            "managed"
                if permission_profile.contains_key("file_system")
                    && permission_profile.contains_key("network")
                    && !has_multiple_roots(permission_profile) =>
            {
                ProfileClass::Managed
            }
            "managed" => return Err(SandboxStateError::UnsupportedProfile),
            "disabled" => ProfileClass::Disabled,
            "external" => return Err(SandboxStateError::ExternalUnsupported),
            _ => return Err(SandboxStateError::UnsupportedProfile),
        };
        Ok(Self { raw, class, cwd })
    }

    /// Returns the accepted profile class.
    pub const fn class(&self) -> ProfileClass {
        self.class
    }

    /// Returns the host-selected working directory without resolving or broadening it.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Serializes the complete original state for the Codex sandbox command.
    fn json_argument(&self) -> String {
        self.raw.to_string()
    }

    /// Returns a shape digest that preserves JSON structure and array cardinality, not scalar values.
    fn shape_digest(&self) -> blake3::Hash {
        blake3::hash(normalize_shape(&self.raw).to_string().as_bytes())
    }
}

impl ExecutionProfileTemplate {
    /// Defines a nonempty, tested Execution profile template and its owned revision.
    pub(crate) fn from_execution_evidence(
        id: impl Into<String>,
        version: u32,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        let id = id.into();
        if id.is_empty() || version == 0 {
            return Err(RequestError::ExecutionProfileDenied);
        }
        Ok(Self {
            id,
            version,
            class: state.class,
            shape_digest: state.shape_digest(),
        })
    }
}

impl ExecutionProfileCatalog {
    /// Builds the catalog from Execution-accepted D03 or disabled-host evidence, not Application policy.
    pub(crate) fn from_execution_evidence(
        templates: Vec<ExecutionProfileTemplate>,
    ) -> Result<Self, RequestError> {
        let mut entries = BTreeMap::new();
        for template in templates {
            if entries.insert(template.class, template).is_some() {
                return Err(RequestError::ExecutionProfileDenied);
            }
        }
        Ok(Self { templates: entries })
    }

    /// Mints an Execution-owned permit when the invocation's supported class has real evidence.
    fn permit(&self, state: &HostSandboxState) -> Result<ExecutionProfilePermit, RequestError> {
        let template = self
            .templates
            .get(&state.class)
            .cloned()
            .ok_or(RequestError::ExecutionProfileDenied)?;
        if template.shape_digest != state.shape_digest() {
            return Err(RequestError::ExecutionProfileDenied);
        }
        Ok(ExecutionProfilePermit {
            template,
            state_digest: blake3::hash(state.json_argument().as_bytes()),
        })
    }
}

/// Identifies a host invocation already proven by Assistance's host-binding adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedHostInvocation {
    /// Opaque Assistance-validated association between actor, invocation, and host channel.
    binding: String,
    /// Complete host sandbox state associated with that specific binding.
    sandbox: HostSandboxState,
}

impl ValidatedHostInvocation {
    /// Creates a token for a nonempty binding that Assistance has already validated.
    ///
    /// Construction carries no independent host proof: Assistance must reject unverified
    /// hook/MCP pairs before creating this token.
    pub fn from_verified_binding(
        binding: impl Into<String>,
        sandbox: HostSandboxState,
    ) -> Result<Self, RequestError> {
        let binding = binding.into();
        if binding.is_empty() {
            return Err(RequestError::MissingBinding);
        }
        Ok(Self { binding, sandbox })
    }

    /// Returns the stable, opaque binding identifier for operation evidence.
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Returns the validated sandbox state associated with this invocation.
    pub fn sandbox(&self) -> &HostSandboxState {
        &self.sandbox
    }
}

/// Identifies the canonical worktree and epoch validated by Workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceAuthority {
    /// Canonical root whose ownership and lifecycle Workspace has already validated.
    root: PathBuf,
    /// Monotonic Workspace authority epoch that rejects stale effects at peer boundaries.
    epoch: u64,
}

impl WorkspaceAuthority {
    /// Creates the narrow authority token after Workspace has checked ownership and lifecycle.
    pub fn from_workspace(root: PathBuf, epoch: u64) -> Result<Self, RequestError> {
        if !is_normal_absolute(&root) {
            return Err(RequestError::InvalidWorktree);
        }
        Ok(Self { root, epoch })
    }

    /// Returns the canonical worktree root supplied by Workspace.
    pub fn root(&self) -> &Path {
        &self.root
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
#[derive(Clone, Debug, Eq, PartialEq)]
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
}

impl ControlledCommand {
    /// Builds a command from a peer-validated executable, argv, cwd, and environment.
    ///
    /// The caller must have validated command semantics for `kind`; Execution rejects relative
    /// paths and later intersects this value with local policy and workspace authority.
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
        Ok(Self {
            kind,
            program,
            args,
            cwd,
            env,
        })
    }
}

/// Applies local ceilings to already validated peer input without widening host permissions.
#[derive(Clone, Debug)]
pub struct LocalExecutionPolicy {
    /// Absolute executable paths permitted by local policy after host/scope intersection.
    allowed_programs: BTreeSet<PathBuf>,
    /// Maximum platform-byte size of all argv values, excluding executable path.
    max_argv_bytes: usize,
    /// Maximum number of explicitly forwarded environment entries.
    max_environment_entries: usize,
    /// Whether this policy explicitly permits a host that declared no outer sandbox.
    allow_explicit_disabled_host: bool,
}

impl LocalExecutionPolicy {
    /// Creates a policy whose program allowlist and numeric limits are independently validated.
    pub fn new(
        allowed_programs: BTreeSet<PathBuf>,
        max_argv_bytes: usize,
        max_environment_entries: usize,
        allow_explicit_disabled_host: bool,
    ) -> Result<Self, RequestError> {
        if allowed_programs.is_empty() || max_argv_bytes == 0 {
            return Err(RequestError::InvalidPolicy);
        }
        Ok(Self {
            allowed_programs,
            max_argv_bytes,
            max_environment_entries,
            allow_explicit_disabled_host,
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
    /// The policy has no executable allowlist or no argv capacity.
    InvalidPolicy,
    /// The command program is outside the local allowlist.
    ProgramDenied,
    /// The command cwd is not the current authoritative worktree.
    WorktreeDenied,
    /// The sandbox's cwd would be changed rather than preserved.
    SandboxCwdMismatch,
    /// The argv exceeds the local byte ceiling.
    ArgvTooLarge,
    /// The environment exceeds the local entry ceiling.
    EnvironmentTooLarge,
    /// A disabled host sandbox was not explicitly accepted by local policy.
    DisabledHostDenied,
    /// Execution has no accepted template for this host profile class or shape.
    ExecutionProfileDenied,
}

/// Couples the only inputs permitted to reach an owned operating-system spawn.
#[derive(Clone, Debug)]
pub struct ValidatedExecutionRequest {
    /// Host binding whose class must match an Execution-owned profile template.
    invocation: ValidatedHostInvocation,
    /// Workspace ownership token constraining this child to one current worktree epoch.
    authority: WorkspaceAuthority,
    /// Execution-minted profile permit carrying a per-invocation correlation digest.
    permit: ExecutionProfilePermit,
    /// Fully controlled executable invocation ready for admission and later owned spawn.
    command: ControlledCommand,
}

impl ValidatedExecutionRequest {
    /// Intersects host state, local policy, and Workspace authority before an admission request.
    pub fn validate(
        invocation: ValidatedHostInvocation,
        authority: WorkspaceAuthority,
        command: ControlledCommand,
        policy: &LocalExecutionPolicy,
        catalog: &ExecutionProfileCatalog,
    ) -> Result<Self, RequestError> {
        if !policy.allowed_programs.contains(&command.program) {
            return Err(RequestError::ProgramDenied);
        }
        if command.cwd != authority.root {
            return Err(RequestError::WorktreeDenied);
        }
        if command.cwd != invocation.sandbox.cwd {
            return Err(RequestError::SandboxCwdMismatch);
        }
        if command.args.iter().map(os_bytes).sum::<usize>() > policy.max_argv_bytes {
            return Err(RequestError::ArgvTooLarge);
        }
        if command.env.len() > policy.max_environment_entries {
            return Err(RequestError::EnvironmentTooLarge);
        }
        if invocation.sandbox.class == ProfileClass::Disabled
            && !policy.allow_explicit_disabled_host
        {
            return Err(RequestError::DisabledHostDenied);
        }
        let permit = catalog.permit(&invocation.sandbox)?;
        Ok(Self {
            invocation,
            authority,
            permit,
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
}

/// Identifies a queued request that may be inspected or cancelled without terminating a process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueTicket(u64);

/// Reserves one admitted execution slot until it is explicitly released after reaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionLease(u64);

/// Returns the only three observable outcomes of asking the centralized admission controller.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// A slot is reserved and the caller may proceed to an owned spawn.
    Granted(AdmissionLease),
    /// The request awaits a fair future grant and has no resource reservation yet.
    Queued(QueueTicket),
    /// A finite ceiling refused the request.
    Refused(AdmissionError),
}

/// Schedules finite requests with per-owner round-robin selection and bounded class preference.
#[derive(Debug)]
pub struct AdmissionController {
    /// Finite queue/running ceilings shared by every owner.
    limits: AdmissionLimits,
    /// Number of leases currently held by each owner.
    running: BTreeMap<OwnerId, usize>,
    /// Active lease identity to owner mapping used to reject stale/double release.
    leases: BTreeMap<u64, OwnerId>,
    /// Requests awaiting a slot; entries intentionally consume no running resource.
    queue: VecDeque<QueuedRequest>,
    /// Next nonzero internal identity for tickets and leases.
    next_id: u64,
    /// Owner of the most recent grant, used to rotate eligible owners.
    last_owner: Option<OwnerId>,
    /// Consecutive interactive grants since the most recent background grant.
    interactive_streak: usize,
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
        Ok(Self {
            limits,
            running: BTreeMap::new(),
            leases: BTreeMap::new(),
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
        Admission::Queued(QueueTicket(id))
    }

    /// Cancels a queued request and releases no running resource because tickets reserve nothing.
    pub fn cancel_ticket(&mut self, ticket: QueueTicket) -> bool {
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
        let owner = self
            .leases
            .remove(&lease.0)
            .ok_or(AdmissionError::UnknownLease)?;
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
            grants.push(self.grant(queued.owner, queued.class));
        }
        Ok(grants)
    }

    /// Returns the number of globally admitted slots for resource-accounting evidence.
    pub fn running_count(&self) -> usize {
        self.total_running()
    }

    /// Returns whether a ticket remains queued without changing its priority or lifetime.
    pub fn contains_ticket(&self, ticket: QueueTicket) -> bool {
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
        AdmissionLease(id)
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

/// Records a direct-child exit plus bounded stream and cancellation evidence.
#[derive(Clone, Debug)]
pub struct ReapedProcess {
    /// Direct child exit status observed by `wait`, not a claim about every descendant.
    pub status: ExitStatus,
    /// TERM/KILL requests made before direct-child reap, if cancellation was requested.
    pub cancellation: Option<CancellationEvidence>,
    /// Bounded standard output drain result.
    pub stdout: CapturedOutput,
    /// Bounded standard error drain result.
    pub stderr: CapturedOutput,
    /// Descendant termination confidence that intentionally remains conservative.
    pub descendants: DescendantEvidence,
    /// The lease released only after this direct child was reaped.
    pub lease: AdmissionLease,
}

/// Reports failures in owned-child launch, signal delivery, wait, or output collection.
#[derive(Debug)]
pub enum ProcessError {
    /// The OS refused to launch or manage the direct child.
    Io(io::Error),
    /// A process protocol requested stdout, so captured-output APIs cannot be used.
    ProtocolStdoutReserved,
}

impl From<io::Error> for ProcessError {
    /// Converts an OS process error without losing its error kind or message.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Owns one launched direct child, its admission lease, and exclusive stdout/stderr drain tasks.
pub struct OwnedChild {
    /// Direct child that Execution launched and therefore may wait/reap.
    child: Child,
    /// Direct-child PID used only for the current owned process-group signal attempt.
    pid: u32,
    /// Sole stdout drainer retaining bounded diagnostic output.
    stdout: JoinHandle<io::Result<CapturedOutput>>,
    /// Sole stderr drainer retaining bounded diagnostic output.
    stderr: JoinHandle<io::Result<CapturedOutput>>,
    /// Admission slot retained until direct-child reap produces evidence.
    lease: AdmissionLease,
}

impl OwnedChild {
    /// Starts a capture-mode child only after central admission reserved `lease`.
    ///
    /// Managed host state is passed intact as one `--sandbox-state-json` argv element; disabled
    /// state launches directly only because `ValidatedExecutionRequest` required explicit policy.
    pub fn spawn_captured(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let mut command = build_command(request, codex_executable);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("spawned child has no PID"))?;
        let stdout = child.stdout.take().expect("piped stdout exists");
        let stderr = child.stderr.take().expect("piped stderr exists");
        Ok(Self {
            child,
            pid,
            stdout: tokio::spawn(drain(stdout, output_cap)),
            stderr: tokio::spawn(drain(stderr, output_cap)),
            lease,
        })
    }

    /// Requests TERM, waits `grace`, requests KILL if necessary, then reaps the direct child.
    ///
    /// Signal acknowledgement proves only delivery attempt. The returned result proves direct-child
    /// reaping and explicitly leaves descendant termination unverified.
    pub async fn cancel_and_reap(
        mut self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<ReapedProcess, ProcessError> {
        let term_requested = signal_group(self.pid, libc::SIGTERM).is_ok();
        let status = match timeout(grace, self.child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let kill_requested = signal_group(self.pid, libc::SIGKILL).is_ok();
                self.child.start_kill()?;
                let status = self.child.wait().await?;
                return self
                    .finish(
                        status,
                        Some(CancellationEvidence {
                            term_requested,
                            kill_requested,
                        }),
                        output_deadline,
                    )
                    .await;
            }
        };
        self.finish(
            status,
            Some(CancellationEvidence {
                term_requested,
                kill_requested: false,
            }),
            output_deadline,
        )
        .await
    }

    /// Waits for ordinary completion and then gathers bounded stream evidence.
    pub async fn reap(self, output_deadline: Duration) -> Result<ReapedProcess, ProcessError> {
        let mut this = self;
        let status = this.child.wait().await?;
        this.finish(status, None, output_deadline).await
    }

    /// Completes drain tasks within a separate deadline after the child is already reaped.
    async fn finish(
        self,
        status: ExitStatus,
        cancellation: Option<CancellationEvidence>,
        output_deadline: Duration,
    ) -> Result<ReapedProcess, ProcessError> {
        let stdout = collect_drain(self.stdout, output_deadline).await;
        let stderr = collect_drain(self.stderr, output_deadline).await;
        Ok(ReapedProcess {
            status,
            cancellation,
            stdout,
            stderr,
            descendants: DescendantEvidence::Unverified,
            lease: self.lease,
        })
    }
}

/// Gives Intelligence sole ownership of a protocol child's stdin/stdout while Execution drains stderr.
pub struct OwnedProtocolChild {
    /// The sole stdin writer for the selected protocol client.
    pub stdin: ChildStdin,
    /// The sole stdout reader for the selected protocol client; Execution never drains it.
    pub stdout: ChildStdout,
    /// Direct owned protocol child that Execution reaps after pipe owner completion.
    child: Child,
    /// Sole stderr drainer; stdout is deliberately unavailable to Execution.
    stderr: JoinHandle<io::Result<CapturedOutput>>,
    /// Admission slot retained until direct protocol-child reap.
    lease: AdmissionLease,
}

impl OwnedProtocolChild {
    /// Starts a protocol child with stdout reserved exclusively for its logical protocol owner.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let mut command = build_command(request, codex_executable);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin exists");
        let stdout = child.stdout.take().expect("piped stdout exists");
        let stderr = child.stderr.take().expect("piped stderr exists");
        Ok(Self {
            stdin,
            stdout,
            child,
            stderr: tokio::spawn(drain(stderr, output_cap)),
            lease,
        })
    }

    /// Reaps the direct protocol child after its owner has finished with the exclusive pipes.
    pub async fn reap(
        self,
        output_deadline: Duration,
    ) -> Result<(ExitStatus, CapturedOutput, AdmissionLease), ProcessError> {
        let mut this = self;
        drop(this.stdin);
        drop(this.stdout);
        let status = this.child.wait().await?;
        let stderr = collect_drain(this.stderr, output_deadline).await;
        Ok((status, stderr, this.lease))
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

/// Constructs a direct or sandbox-wrapped command entirely from validated typed inputs.
fn build_command(request: &ValidatedExecutionRequest, codex_executable: &Path) -> Command {
    let command = &request.command;
    let mut process = match request.invocation.sandbox.class {
        ProfileClass::Managed => {
            let mut sandbox = Command::new(codex_executable);
            sandbox
                .arg("sandbox")
                .arg("--sandbox-state-json")
                .arg(request.invocation.sandbox.json_argument())
                .arg("--")
                .arg(&command.program);
            sandbox
        }
        ProfileClass::Disabled => Command::new(&command.program),
    };
    process.args(&command.args);
    process
        .current_dir(&command.cwd)
        .env_clear()
        .envs(&command.env);
    process
}

/// Configures a distinct Unix process group so signals may target owned descendants conservatively.
fn configure_process_group(command: &mut Command) {
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

/// Replaces opaque scalar values while preserving JSON keys, types, and array cardinality for profile matching.
fn normalize_shape(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), normalize_shape(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(normalize_shape).collect()),
        Value::String(_) => Value::String("<string>".into()),
        Value::Number(_) => Value::String("<number>".into()),
        Value::Bool(_) => Value::String("<bool>".into()),
        Value::Null => Value::Null,
    }
}

/// Detects unsupported multi-root profile fields without resolving or expanding any root path.
fn has_multiple_roots(value: &serde_json::Map<String, Value>) -> bool {
    value.iter().any(|(key, value)| {
        (key == "roots" || key.ends_with("_roots"))
            && value.as_array().is_some_and(|roots| roots.len() > 1)
    })
}
