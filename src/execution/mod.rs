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
    task::{AbortHandle, JoinHandle},
    time::timeout,
};

use crate::assistance::host_binding::{
    ActiveBindingUse, ObservedSandboxState, SandboxStateProvenance,
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
    /// The supplied sandbox cwd is neither an absolute path nor a supported local file URI.
    UnsupportedCwd,
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
    /// Effective permission/trust value proven by this template, excluding only sandbox cwd identity.
    profile_digest: blake3::Hash,
}

/// Holds Execution-owned templates whose real evidence permits physical effects.
#[derive(Clone, Debug)]
pub struct ExecutionProfileCatalog {
    /// Exactly one current tested template per host profile class.
    templates: BTreeMap<ProfileClass, ExecutionProfileTemplate>,
}

/// Is the durable, Execution-owned record that Application may store without interpreting it.
///
/// Every identity is an opaque, nonempty value supplied by the verified Execution evidence
/// pipeline.  The record deliberately stores the semantic state digest alongside its separate
/// provider/toolchain/config/trust/transport identities: matching a template name alone never
/// makes a changed profile executable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedProfileRecord {
    /// Stable profile-template identity selected by Execution code.
    pub profile_id: String,
    /// Monotonic Execution-owned template revision.
    pub revision: u32,
    /// Supported host class recorded with the evidence; unknown values are unavailable.
    pub class: ProfileClass,
    /// Exact provider binary identity observed by the D03 run.
    pub provider_binary: String,
    /// Exact toolchain identity observed by the D03 run.
    pub toolchain: String,
    /// Effective provider configuration identity observed by the D03 run.
    pub configuration: String,
    /// Effective trust decision identity observed by the D03 run.
    pub trust: String,
    /// Sandbox transport/mechanism identity observed by the D03 run.
    pub transport: String,
    /// Effective permission-value identity, never a path-erasing shape match.
    pub permission_value: String,
    /// Immutable D03 evidence identity for this tested record.
    pub d03_evidence: String,
    /// Semantic (not textual) complete-state identity for the accepted profile value.
    pub semantic_state: String,
}

/// Carries the non-state identities captured by one verified D03 profile experiment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct D03ProfileEvidence {
    /// Exact provider binary identity from the experiment.
    pub provider_binary: String,
    /// Exact toolchain identity from the experiment.
    pub toolchain: String,
    /// Effective provider configuration identity from the experiment.
    pub configuration: String,
    /// Effective local trust identity from the experiment.
    pub trust: String,
    /// Sandbox transport identity from the experiment.
    pub transport: String,
    /// Immutable D03 result identity from the experiment.
    pub d03_evidence: String,
}

impl PersistedProfileRecord {
    /// Creates a complete record from one verified D03 result and the exact observed host state.
    ///
    /// Each supplied identity must be nonempty and comes from the Execution verification path,
    /// never from a model request or a persisted record being replayed.
    pub fn from_execution_evidence(
        profile_id: impl Into<String>,
        revision: u32,
        evidence: D03ProfileEvidence,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        let record = Self {
            profile_id: profile_id.into(),
            revision,
            class: state.class(),
            provider_binary: evidence.provider_binary,
            toolchain: evidence.toolchain,
            configuration: evidence.configuration,
            trust: evidence.trust,
            transport: evidence.transport,
            permission_value: state.profile_digest().to_hex().to_string(),
            d03_evidence: evidence.d03_evidence,
            semantic_state: semantic_state_identity(state),
        };
        (record.revision != 0
            && [
                &record.profile_id,
                &record.provider_binary,
                &record.toolchain,
                &record.configuration,
                &record.trust,
                &record.transport,
                &record.permission_value,
                &record.d03_evidence,
                &record.semantic_state,
            ]
            .iter()
            .all(|value| !value.is_empty()))
        .then_some(record)
        .ok_or(RequestError::ExecutionProfileDenied)
    }

    /// Validates an opaque durable record before Execution may use it to rebuild a catalog.
    ///
    /// Empty identities, zero revisions, malformed JSON, and records for a different semantic
    /// state are unavailable.  Application only persists the returned JSON; permit minting stays
    /// in `ExecutionProfileCatalog`.
    pub fn from_json(json: &str) -> Result<Self, RequestError> {
        let value: Value =
            serde_json::from_str(json).map_err(|_| RequestError::ExecutionProfileDenied)?;
        let object = value
            .as_object()
            .ok_or(RequestError::ExecutionProfileDenied)?;
        let string = |name: &str| {
            object
                .get(name)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or(RequestError::ExecutionProfileDenied)
        };
        let revision = object
            .get("revision")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value != 0)
            .ok_or(RequestError::ExecutionProfileDenied)?;
        let class = match object.get("class").and_then(Value::as_str) {
            Some("managed") => ProfileClass::Managed,
            Some("disabled") => ProfileClass::Disabled,
            _ => return Err(RequestError::ExecutionProfileDenied),
        };
        Ok(Self {
            profile_id: string("profile_id")?,
            revision,
            class,
            provider_binary: string("provider_binary")?,
            toolchain: string("toolchain")?,
            configuration: string("configuration")?,
            trust: string("trust")?,
            transport: string("transport")?,
            permission_value: string("permission_value")?,
            d03_evidence: string("d03_evidence")?,
            semantic_state: string("semantic_state")?,
        })
    }

    /// Serializes this complete record in a stable field layout for Application's opaque store.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "profile_id": self.profile_id,
            "revision": self.revision,
            "class": match self.class { ProfileClass::Managed => "managed", ProfileClass::Disabled => "disabled" },
            "provider_binary": self.provider_binary,
            "toolchain": self.toolchain,
            "configuration": self.configuration,
            "trust": self.trust,
            "transport": self.transport,
            "permission_value": self.permission_value,
            "d03_evidence": self.d03_evidence,
            "semantic_state": self.semantic_state,
        })
        .to_string()
    }

    /// Returns whether this durable record is exactly applicable to the supplied observed state.
    pub fn matches_state(&self, state: &HostSandboxState) -> bool {
        self.semantic_state == semantic_state_identity(state)
            && self.permission_value == state.profile_digest().to_hex().to_string()
    }
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
    /// Parsed state copy used only for validation and profile-shape comparison.
    raw: Value,
    /// Original JSON text retained byte-for-byte for the managed Codex sandbox argument.
    raw_json: String,
    /// Accepted high-level profile class derived from the state envelope.
    class: ProfileClass,
    /// Exact host-selected sandbox cwd retained unchanged inside `raw` for Codex replay.
    sandbox_cwd: String,
    /// Local filesystem cwd derived only for `Command::current_dir`, never written back to `raw`.
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
        let raw_json = raw.to_string();
        Self::parse_value(raw, raw_json)
    }

    /// Parses captured JSON while retaining its exact original bytes for sandbox replay.
    pub fn parse_json(raw_json: &str) -> Result<Self, SandboxStateError> {
        let raw = serde_json::from_str(raw_json).map_err(|_| SandboxStateError::Malformed)?;
        Self::parse_value(raw, raw_json.to_owned())
    }

    /// Validates one parsed state while associating it with the exact JSON argument to replay.
    fn parse_value(raw: Value, raw_json: String) -> Result<Self, SandboxStateError> {
        let object = raw.as_object().ok_or(SandboxStateError::Malformed)?;
        let permission_profile = object
            .get("permissionProfile")
            .and_then(Value::as_object)
            .ok_or(SandboxStateError::Malformed)?;
        let profile_type = permission_profile
            .get("type")
            .and_then(Value::as_str)
            .ok_or(SandboxStateError::Malformed)?;
        let sandbox_cwd = object
            .get("sandboxCwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(str::to_owned)
            .ok_or(SandboxStateError::Malformed)?;
        let cwd = local_sandbox_cwd(&sandbox_cwd)?;
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
        Ok(Self {
            raw,
            raw_json,
            class,
            sandbox_cwd,
            cwd,
        })
    }

    /// Returns the accepted profile class.
    pub const fn class(&self) -> ProfileClass {
        self.class
    }

    /// Returns the local path used as the child cwd while leaving the raw sandbox URI unchanged.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Returns the exact host-provided cwd representation retained inside the replayed state.
    pub fn sandbox_cwd(&self) -> &str {
        &self.sandbox_cwd
    }

    /// Serializes the complete original state for the Codex sandbox command.
    fn json_argument(&self) -> &str {
        &self.raw_json
    }

    /// Returns the exact captured JSON argument retained for managed Codex sandbox replay.
    pub fn sandbox_state_json(&self) -> &str {
        &self.raw_json
    }

    /// Returns a profile digest that retains effective permission values but excludes cwd identity.
    fn profile_digest(&self) -> blake3::Hash {
        blake3::hash(profile_template_value(&self.raw).to_string().as_bytes())
    }
}

impl ExecutionProfileTemplate {
    /// Defines a nonempty, tested Execution profile template and its owned revision.
    pub fn from_execution_evidence(
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
            profile_digest: state.profile_digest(),
        })
    }
}

impl ExecutionProfileCatalog {
    /// Builds the catalog from Execution-accepted D03 or disabled-host evidence, not Application policy.
    pub fn from_execution_evidence(
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

    /// Rebuilds a usable catalog only from records matching trusted expected D03 evidence.
    ///
    /// `expected` comes from Execution-owned trusted configuration/evidence, never the durable
    /// store or a model request. The caller supplies each current D01-bound state; extra, missing,
    /// stale, corrupt, duplicate-class, or value-mismatched records are unavailable.
    pub fn from_persisted_records(
        records: Vec<(PersistedProfileRecord, HostSandboxState)>,
        expected: &[PersistedProfileRecord],
    ) -> Result<Self, RequestError> {
        if records.len() != expected.len() {
            return Err(RequestError::ExecutionProfileDenied);
        }
        let mut templates = Vec::with_capacity(records.len());
        for (record, state) in records {
            if !expected.contains(&record)
                || record.class != state.class()
                || !record.matches_state(&state)
            {
                return Err(RequestError::ExecutionProfileDenied);
            }
            templates.push(ExecutionProfileTemplate {
                id: record.profile_id,
                version: record.revision,
                class: record.class,
                profile_digest: state.profile_digest(),
            });
        }
        Self::from_execution_evidence(templates)
    }

    /// Mints an Execution-owned permit when the invocation's supported class has real evidence.
    fn permit(&self, state: &HostSandboxState) -> Result<ExecutionProfilePermit, RequestError> {
        let template = self
            .templates
            .get(&state.class)
            .cloned()
            .ok_or(RequestError::ExecutionProfileDenied)?;
        if template.profile_digest != state.profile_digest() {
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
    /// Assistance binding that requires a fresh consume before a delayed owned spawn.
    active_binding: Option<crate::assistance::host_binding::BindingRef>,
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
        Ok(Self {
            binding,
            sandbox,
            active_binding: None,
        })
    }

    /// Admits one consumed active use and its matching observed state into Execution.
    ///
    /// The use is consumed at this boundary: it proves a fresh liveness check before this local
    /// step begins, but cannot become durable authority. Execution retains every semantic JSON
    /// field from the opaque observation for its own bounded profile parser.
    pub fn from_active_observation(
        active_use: ActiveBindingUse,
        observed: ObservedSandboxState,
    ) -> Result<Self, RequestError> {
        if active_use.binding_ref() != observed.binding_ref()
            || observed.provenance() != SandboxStateProvenance::AdvertisedAndReturned
        {
            return Err(RequestError::BindingMismatch);
        }
        let sandbox = HostSandboxState::parse(Some(observed.state().as_json().clone()))
            .map_err(RequestError::ObservedStateUnavailable)?;
        Ok(Self {
            binding: observed.call_id().to_owned(),
            sandbox,
            active_binding: Some(observed.binding_ref().clone()),
        })
    }

    /// Returns the stable, opaque binding identifier for operation evidence.
    pub fn binding(&self) -> &str {
        &self.binding
    }

    /// Returns the validated sandbox state associated with this invocation.
    pub fn sandbox(&self) -> &HostSandboxState {
        &self.sandbox
    }

    /// Consumes a freshly checked binding use for an immediate owned child spawn.
    ///
    /// Synthetic/test invocations have no Assistance binding and need no use. An invocation from
    /// observed host state requires a use for the same immutable generation, preventing queue
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
    /// Monotonic Workspace authority epoch that rejects stale effects at peer boundaries.
    epoch: u64,
}

impl WorkspaceAuthority {
    /// Creates the narrow authority token after Workspace has checked ownership and lifecycle.
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
            root,
            epoch,
        })
    }

    /// Returns the canonical worktree root supplied by Workspace.
    pub fn root(&self) -> &Path {
        &self.root
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

/// Identifies one Workspace discovery operation without granting worktree authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryOperationRef(String);

impl DiscoveryOperationRef {
    /// Creates a nonempty stable operation reference supplied by Workspace for result correlation.
    pub fn new(value: impl Into<String>) -> Result<Self, RequestError> {
        let value = value.into();
        if value.is_empty() {
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
    /// Returns the common Git directory as one terminal-LF raw path result.
    GitCommonDir,
    /// Returns every worktree record as NUL-delimited porcelain bytes.
    WorktreeListPorcelainZ,
}

/// Holds the fresh observed binding/state and raw candidate path for pre-authority Git discovery.
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
    /// Accepts a fresh Assistance use, its correlated observed state, raw candidate path, and operation ref.
    ///
    /// This constructor has no WorkspaceAuthority output and never normalizes `candidate_cwd`.
    /// A later queued execution must present another fresh use to `run` before a child can launch.
    pub fn from_active_observation(
        active_use: ActiveBindingUse,
        observed: ObservedSandboxState,
        candidate_cwd: OsString,
        operation: DiscoveryOperationRef,
    ) -> Result<Self, RequestError> {
        if os_bytes(&candidate_cwd) == 0 {
            return Err(RequestError::InvalidDiscoveryCandidate);
        }
        Ok(Self {
            invocation: ValidatedHostInvocation::from_active_observation(active_use, observed)?,
            candidate_cwd,
            operation,
        })
    }

    /// Validates exactly one fixed Git query under Execution's tested profile catalog and local policy.
    pub fn validate_query(
        self,
        query: GitDiscoveryQuery,
        policy: &GitDiscoveryPolicy,
        catalog: &ExecutionProfileCatalog,
    ) -> Result<ValidatedGitDiscovery, RequestError> {
        if !is_normal_absolute(&policy.git_program) || policy.output_cap == 0 {
            return Err(RequestError::InvalidDiscoveryPolicy);
        }
        if self.invocation.sandbox.class == ProfileClass::Disabled
            && !policy.allow_explicit_disabled_host
        {
            return Err(RequestError::DisabledHostDenied);
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
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            policy.git_program.clone(),
            args,
            self.invocation.sandbox.cwd().to_path_buf(),
            BTreeMap::new(),
        )?;
        let permit = catalog.permit(&self.invocation.sandbox)?;
        Ok(ValidatedGitDiscovery {
            invocation: self.invocation,
            command,
            operation: self.operation,
            query,
            permit,
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
    /// Whether a separately accepted explicit disabled host profile may perform this read-only operation.
    allow_explicit_disabled_host: bool,
}

impl GitDiscoveryPolicy {
    /// Creates a fixed discovery policy after rejecting an invalid executable or zero output budget.
    pub fn new(
        git_program: PathBuf,
        output_cap: usize,
        allow_explicit_disabled_host: bool,
    ) -> Result<Self, RequestError> {
        if !is_normal_absolute(&git_program) || output_cap == 0 {
            return Err(RequestError::InvalidDiscoveryPolicy);
        }
        Ok(Self {
            git_program,
            output_cap,
            allow_explicit_disabled_host,
        })
    }
}

/// Carries a catalog-admitted fixed Git discovery until a fresh active use permits its owned spawn.
#[derive(Clone, Debug)]
pub struct ValidatedGitDiscovery {
    /// Assistance-correlated invocation retained for the final liveness recheck.
    invocation: ValidatedHostInvocation,
    /// Fixed Git command with raw candidate only after `-C`.
    command: ControlledCommand,
    /// Opaque Workspace operation correlation reference.
    operation: DiscoveryOperationRef,
    /// Fixed query kind whose output remains raw.
    query: GitDiscoveryQuery,
    /// Execution profile evidence associated with this operation.
    permit: ExecutionProfilePermit,
    /// Fixed per-stream retained-output cap from local discovery policy.
    output_cap: usize,
}

/// Returns one raw fixed-query Git result without parsing or minting worktree authority.
#[derive(Clone, Debug)]
pub struct RawGitDiscovery {
    /// Original Workspace operation reference.
    pub operation: DiscoveryOperationRef,
    /// Fixed query whose raw output is returned.
    pub query: GitDiscoveryQuery,
    /// Raw stdout bytes; terminal LF and NUL encoding remain Workspace's responsibility.
    pub stdout: CapturedOutput,
    /// Raw stderr bytes without text decoding.
    pub stderr: CapturedOutput,
    /// Direct child exit status observed after reap.
    pub exit_status: ExitStatus,
    /// Elapsed wall time from child spawn to direct-child reap and drain collection.
    pub elapsed: Duration,
    /// Admission lease released by the caller only after it records or otherwise handles this evidence.
    pub lease: AdmissionLease,
}

impl ValidatedGitDiscovery {
    /// Rechecks active binding immediately before spawning the fixed Git child and returns raw evidence.
    pub async fn run(
        self,
        lease: AdmissionLease,
        active_use: ActiveBindingUse,
        codex_executable: &Path,
    ) -> Result<RawGitDiscovery, ProcessError> {
        self.invocation
            .consume_active_use(active_use)
            .map_err(ProcessError::Request)?;
        let started = std::time::Instant::now();
        let result = OwnedChild::spawn_parts(
            &self.command,
            &self.invocation.sandbox,
            lease,
            codex_executable,
            self.output_cap,
        )?
        .reap(Duration::from_secs(10))
        .await?;
        let _ = &self.permit;
        Ok(RawGitDiscovery {
            operation: self.operation,
            query: self.query,
            stdout: result.stdout,
            stderr: result.stderr,
            exit_status: result.status,
            elapsed: started.elapsed(),
            lease: result.lease,
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
    /// The consumed active binding does not match the observed sandbox-state generation.
    BindingMismatch,
    /// The observed host state cannot satisfy Execution's bounded parser.
    ObservedStateUnavailable(SandboxStateError),
    /// A delayed owned spawn did not present a newly consumed matching active binding use.
    MissingActiveBindingUse,
    /// Workspace supplied an empty discovery operation reference.
    InvalidDiscoveryOperation,
    /// Workspace supplied an empty raw candidate path for fixed Git `-C` execution.
    InvalidDiscoveryCandidate,
    /// Local fixed Git discovery policy has no absolute executable or output budget.
    InvalidDiscoveryPolicy,
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

    /// Returns the Execution-minted profile permit retained as operation evidence after admission.
    pub fn profile_permit(&self) -> &ExecutionProfilePermit {
        &self.permit
    }

    /// Consumes a freshly checked active use immediately before an owned physical spawn.
    ///
    /// Requests constructed from an Assistance observation retain its binding generation so a
    /// queue delay cannot convert pre-stop liveness into a later effect. Synthetic/test requests
    /// have no active binding and reject no optional use.
    fn consume_spawn_use(&self, active_use: Option<ActiveBindingUse>) -> Result<(), RequestError> {
        match (&self.invocation.active_binding, active_use) {
            (Some(_), Some(active_use)) => self.invocation.consume_active_use(active_use),
            (Some(_), None) => Err(RequestError::MissingActiveBindingUse),
            (None, _) => Ok(()),
        }
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
///
/// Abnormal owned-child drop retains this reservation because it supplies no reap evidence.
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

/// Couples a central queue ticket to the exact lease that promoted it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdmissionPromotion {
    /// Ticket removed from the controller queue by this promotion.
    ticket: QueueTicket,
    /// Newly reserved central lease which must be consumed or released exactly once.
    lease: AdmissionLease,
}

impl AdmissionPromotion {
    /// Returns the queued identity this promotion settles.
    pub const fn ticket(self) -> QueueTicket {
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
            let ticket = QueueTicket(queued.id);
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
}

impl ProviderSpawnLease {
    /// Returns the backend identity to which this one-time spawn is restricted.
    pub fn backend(&self) -> &str {
        &self.backend
    }

    /// Rejects authority substitution before any process effect or active-binding consumption.
    fn validate_request(&self, request: &ValidatedExecutionRequest) -> Result<(), ProcessError> {
        if self.authority != *request.authority() {
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

/// States whether the final release of a logical view can reap a physical backend.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendRelease {
    /// A compatible shared peer still uses this owned backend, so it remains alive.
    SharedPeerSurvives,
    /// The last view released an owned backend; its owner may begin bounded reap separately.
    ReapOwned { backend: String },
    /// Borrowed endpoints remain visible but are never signalled or reaped by Execution.
    BorrowedDetached,
}

/// Receipts returned after one finite Workspace authority revocation reaches Execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityDrainReceipt {
    /// Number of matching logical views removed before any physical reap decision.
    pub drained_views: usize,
    /// Per-backend decisions that preserve peers and borrowed ownership.
    pub backend_releases: Vec<BackendRelease>,
    /// Admission leases released because an owned backend lost its final view.
    pub released_admissions: Vec<AdmissionLease>,
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
    forwarders: BTreeMap<u64, AdmissionLease>,
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
    admission: Option<AdmissionLease>,
    /// Number of logical views currently routed to this backend.
    views: usize,
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
        if !self.can_attach(&backend) {
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
        self.backends.insert(
            backend.clone(),
            ProviderBackend {
                kind,
                admission: reserved,
                views: 0,
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
                admission: Some(promotion.lease),
                views: 0,
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
            })
            .ok_or(ProviderLeaseError::SpawnUnavailable)
    }

    /// Binds a separately admitted process slot once to a shared view's exact request authority.
    /// Rejects unknown/stale views, authority changes, backend slots, released slots, and any
    /// previously bound view or slot without consuming them. The caller retains the slot on error
    /// and releases it only after forwarder reap on success; logical release never releases it.
    pub fn take_forwarder_spawn_lease(
        &mut self,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
    ) -> Result<ProviderForwarderSpawnLease, ProviderLeaseError> {
        let scope = self
            .views
            .get(&view.0)
            .ok_or(ProviderLeaseError::UnknownView)?;
        if scope.authority != *request.authority() {
            return Err(ProviderLeaseError::InvalidAuthority);
        }
        if self.backends[&scope.backend].kind != ProviderBackendKind::OwnedShared
            || !admission.leases.contains_key(&lease.0)
            || admission.provider_slots.contains(&lease.0)
            || self.forwarders.contains_key(&view.0)
        {
            return Err(ProviderLeaseError::SpawnUnavailable);
        }
        admission.provider_slots.insert(lease.0);
        self.forwarders.insert(view.0, lease);
        Ok(ProviderForwarderSpawnLease {
            spawn: ProviderSpawnLease {
                view,
                lease,
                authority: scope.authority.clone(),
                backend: scope.backend.clone(),
            },
        })
    }

    /// Releases exactly one logical view and returns the physical ownership consequence.
    pub fn release(
        &mut self,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
    ) -> Result<
        (
            BackendRelease,
            Option<AdmissionLease>,
            Vec<AdmissionPromotion>,
        ),
        ProviderLeaseError,
    > {
        let view_id = view.0;
        let view = self
            .views
            .remove(&view_id)
            .ok_or(ProviderLeaseError::UnknownView)?;
        self.spawnable.remove(&view_id);
        self.forwarders.remove(&view_id);
        let backend = self
            .backends
            .get_mut(&view.backend)
            .expect("view backend exists");
        backend.views -= 1;
        if backend.views != 0 {
            return Ok((BackendRelease::SharedPeerSurvives, None, Vec::new()));
        }
        let backend = self
            .backends
            .remove(&view.backend)
            .expect("view backend exists");
        match backend.kind {
            ProviderBackendKind::Borrowed => {
                Ok((BackendRelease::BorrowedDetached, None, Vec::new()))
            }
            ProviderBackendKind::OwnedShared | ProviderBackendKind::OwnedExclusive => {
                let lease = backend.admission.expect("owned backend has admission");
                let promotions = admission
                    .release_with_promotions(lease)
                    .map_err(ProviderLeaseError::Admission)?;
                Ok((
                    BackendRelease::ReapOwned {
                        backend: view.backend,
                    },
                    Some(lease),
                    promotions,
                ))
            }
        }
    }

    /// Fences views matching one finite Workspace revocation without touching surviving peers.
    ///
    /// The receipt distinguishes logical removal and admission release from actual signal delivery
    /// and reaping, which remain uncertain until the owner reports direct-child evidence.
    pub fn revoke_authority(
        &mut self,
        admission: &mut AdmissionController,
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
        let mut backend_releases = Vec::with_capacity(matching.len());
        let mut released_admissions = Vec::new();
        for view in &matching {
            if let Ok((release, admission_lease, _)) = self.release(admission, *view) {
                if let Some(admission_lease) = admission_lease {
                    released_admissions.push(admission_lease);
                }
                backend_releases.push(release);
            }
        }
        AuthorityDrainReceipt {
            drained_views: matching.len(),
            reap_uncertain: backend_releases
                .iter()
                .any(|release| matches!(release, BackendRelease::ReapOwned { .. })),
            backend_releases,
            released_admissions,
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
    /// A request failed its final active-binding admission check before any child launched.
    Request(RequestError),
    /// A process protocol requested stdout, so captured-output APIs cannot be used.
    ProtocolStdoutReserved,
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
    /// Live direct-child handle; its ID becomes absent after wait has reaped it.
    child: Child,
    /// Cancellation handles for the child's exclusively owned output drain tasks.
    drainers: Vec<AbortHandle>,
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
    /// Starts a capture-mode provider backend using its one-time, authority-bound registry grant.
    /// Authority mismatch returns an error before consuming active use or spawning a process.
    pub fn spawn_from_provider_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        capability.validate_request(request)?;
        Self::spawn_captured(
            request,
            capability.lease,
            active_use,
            codex_executable,
            output_cap,
        )
    }

    /// Starts a capture-mode child only after central admission reserved `lease`.
    ///
    /// Managed host state is passed intact as one `--sandbox-state-json` argv element; disabled
    /// state launches directly only because `ValidatedExecutionRequest` required explicit policy.
    pub fn spawn_captured(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        request
            .consume_spawn_use(active_use)
            .map_err(ProcessError::Request)?;
        Self::spawn_parts(
            &request.command,
            &request.invocation.sandbox,
            lease,
            codex_executable,
            output_cap,
        )
    }

    /// Launches a capture-mode child from already validated command and sandbox components.
    fn spawn_parts(
        command: &ControlledCommand,
        sandbox: &HostSandboxState,
        lease: AdmissionLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let mut process = build_command(command, sandbox, codex_executable)?;
        process.stdout(Stdio::piped()).stderr(Stdio::piped());
        configure_process_group(&mut process);
        let mut child = process.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("spawned child has no PID"))?;
        let stdout = child.stdout.take().expect("piped stdout exists");
        let stderr = child.stderr.take().expect("piped stderr exists");
        let stdout = tokio::spawn(drain(stdout, output_cap));
        let stderr = tokio::spawn(drain(stderr, output_cap));
        Ok(Self {
            process: ChildOwnership {
                child,
                drainers: vec![stdout.abort_handle(), stderr.abort_handle()],
            },
            pid,
            stdout,
            stderr,
            lease,
        })
    }

    /// Requests TERM, waits `grace`, requests KILL if necessary, then reaps the direct child.
    ///
    /// Signal acknowledgement proves only delivery attempt. The returned result proves direct-child
    /// reaping and explicitly leaves descendant termination unverified. Cancelling this future
    /// invokes immediate drop cleanup and produces no admission-release evidence.
    pub async fn cancel_and_reap(
        mut self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<ReapedProcess, ProcessError> {
        let term_requested = signal_group(self.pid, libc::SIGTERM).is_ok();
        let status = match timeout(grace, self.process.child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let kill_requested = signal_group(self.pid, libc::SIGKILL).is_ok();
                self.process.child.start_kill()?;
                let status = self.process.child.wait().await?;
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

    /// Waits for ordinary completion and bounded stream evidence; cancellation invokes drop cleanup.
    pub async fn reap(self, output_deadline: Duration) -> Result<ReapedProcess, ProcessError> {
        let mut this = self;
        let status = this.process.child.wait().await?;
        this.finish(status, None, output_deadline).await
    }

    /// Completes drain tasks within a separate deadline after the child is already reaped.
    async fn finish(
        self,
        status: ExitStatus,
        cancellation: Option<CancellationEvidence>,
        output_deadline: Duration,
    ) -> Result<ReapedProcess, ProcessError> {
        let _ownership = self.process;
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
///
/// Dropping the handle or consuming reap future requests owned group/direct kill and aborts stderr;
/// it produces no exit evidence and does not free the admission reservation.
pub struct OwnedProtocolChild {
    /// The sole stdin writer for the selected protocol client.
    pub stdin: ChildStdin,
    /// The sole stdout reader for the selected protocol client; Execution never drains it.
    pub stdout: ChildStdout,
    /// Cancellation-safe child ownership retained after the protocol pipes move or close.
    process: ChildOwnership,
    /// Sole stderr drainer; stdout is deliberately unavailable to Execution.
    stderr: JoinHandle<io::Result<CapturedOutput>>,
    /// Admission slot retained until direct protocol-child reap.
    lease: AdmissionLease,
}

impl OwnedProtocolChild {
    /// Starts an owned protocol child by consuming its authority-bound one-time backend capability.
    /// Authority mismatch returns an error before consuming active use or spawning a process.
    pub fn spawn_from_provider_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        capability.validate_request(request)?;
        Self::spawn(
            request,
            capability.lease,
            active_use,
            codex_executable,
            output_cap,
        )
    }

    /// Starts one forwarder using its distinct process slot and exact registry-view authority.
    /// The consumed capability cannot be reused; reap returns this forwarder's slot, never its listener's.
    pub fn spawn_from_forwarder_lease(
        request: &ValidatedExecutionRequest,
        capability: ProviderForwarderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        Self::spawn_from_provider_lease(
            request,
            capability.spawn,
            active_use,
            codex_executable,
            output_cap,
        )
    }

    /// Starts a protocol child with stdout reserved exclusively for its logical protocol owner.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        request
            .consume_spawn_use(active_use)
            .map_err(ProcessError::Request)?;
        let mut command = build_command(
            &request.command,
            &request.invocation.sandbox,
            codex_executable,
        )?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin exists");
        let stdout = child.stdout.take().expect("piped stdout exists");
        let stderr = child.stderr.take().expect("piped stderr exists");
        let stderr = tokio::spawn(drain(stderr, output_cap));
        Ok(Self {
            stdin,
            stdout,
            process: ChildOwnership {
                child,
                drainers: vec![stderr.abort_handle()],
            },
            stderr,
            lease,
        })
    }

    /// Closes the exclusive pipes and reaps the direct child; cancellation invokes drop cleanup.
    pub async fn reap(
        self,
        output_deadline: Duration,
    ) -> Result<(ExitStatus, CapturedOutput, AdmissionLease), ProcessError> {
        let mut this = self;
        drop(this.stdin);
        drop(this.stdout);
        let status = this.process.child.wait().await?;
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
///
/// Managed Codex wrappers receive only the executable's own directory as `PATH` so an env-based
/// Node launcher can start without inheriting arbitrary parent environment entries.
fn build_command(
    command: &ControlledCommand,
    sandbox: &HostSandboxState,
    codex_executable: &Path,
) -> Result<Command, ProcessError> {
    let mut process = match sandbox.class {
        ProfileClass::Managed => {
            let mut sandbox_command = Command::new(codex_executable);
            sandbox_command
                .arg("sandbox")
                .arg("--sandbox-state-json")
                .arg(sandbox.json_argument())
                .arg("--")
                .arg(&command.program);
            sandbox_command
        }
        ProfileClass::Disabled => Command::new(&command.program),
    };
    process.args(&command.args);
    process
        .current_dir(&command.cwd)
        .env_clear()
        .envs(&command.env);
    if sandbox.class == ProfileClass::Managed {
        let parent = codex_executable
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                ProcessError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "managed Codex executable must have an absolute parent directory",
                ))
            })?;
        process.env("PATH", parent);
    }
    Ok(process)
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

/// Keeps all semantic profile fields/values while excluding only sandbox cwd identity from a template digest.
fn profile_template_value(value: &Value) -> Value {
    let mut template = value.clone();
    if let Some(object) = template.as_object_mut() {
        object.insert("sandboxCwd".into(), Value::String("<workspace-cwd>".into()));
    }
    template
}

/// Hashes the complete semantic host state without relying on JSON whitespace or key order.
fn semantic_state_identity(state: &HostSandboxState) -> String {
    blake3::hash(canonical_json(&profile_template_value(&state.raw)).as_bytes())
        .to_hex()
        .to_string()
}

/// Produces a key-order-independent JSON representation while retaining every value and array order.
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(object) => {
            let mut fields: Vec<_> = object.iter().collect();
            fields.sort_unstable_by_key(|(key, _)| *key);
            let body = fields
                .into_iter()
                .map(|(key, value)| {
                    format!("{}:{}", Value::String(key.clone()), canonical_json(value))
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => value.to_string(),
    }
}

/// Detects unsupported multi-root profile fields without resolving or expanding any root path.
fn has_multiple_roots(value: &serde_json::Map<String, Value>) -> bool {
    value.iter().any(|(key, value)| {
        (key == "roots" || key.ends_with("_roots"))
            && value.as_array().is_some_and(|roots| roots.len() > 1)
    })
}

/// Derives a local process cwd from a host cwd without modifying the opaque sandbox state.
fn local_sandbox_cwd(raw: &str) -> Result<PathBuf, SandboxStateError> {
    let path = if let Some(path) = raw.strip_prefix("file://") {
        if path.contains('%') || !path.starts_with('/') {
            return Err(SandboxStateError::UnsupportedCwd);
        }
        PathBuf::from(path)
    } else {
        PathBuf::from(raw)
    };
    if is_normal_absolute(&path) {
        Ok(path)
    } else {
        Err(SandboxStateError::UnsupportedCwd)
    }
}
