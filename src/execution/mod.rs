//! Admission and owned-child supervision for controlled Agent IDE effects.
//!
//! This module deliberately accepts only peer-validated authority and controlled commands. It
//! preserves a managed Codex sandbox state as opaque JSON and refuses profiles whose filesystem
//! authority cannot be replayed exactly.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    task::{AbortHandle, JoinHandle},
    time::timeout,
};

use crate::assistance::host_binding::{
    ActiveBindingUse, ObservedSandboxState, SANDBOX_STATE_FIELDS, SandboxStateProvenance,
};

mod profile_shape;

use profile_shape::{ProfileShapeV2, ProfileShapeV3, UnsupportedShape};

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

/// Maximum accepted profiles in one catalog across every host profile class (T25B).
pub const MAX_ACCEPTED_PROFILES: usize = 8;

/// The versioned comparison data one accepted template carries (T35B).
///
/// A template stores what its admission proof needs, not only a digest: v1 compares exact
/// legacy digests, v2 compares the accepted rule structure, and v3 first proves one complete
/// visualization-leaf group. Generations are never mixed inside one proof.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TemplateShape {
    /// Legacy exact-digest admission; behaviour is byte-for-byte unchanged (T35B).
    V1 {
        /// The worktree-portable legacy profile digest; the template's v1 identity.
        profile_digest: blake3::Hash,
    },
    /// Conservative shape-based admission via the seven sufficient narrowing conditions.
    V2 {
        /// The complete accepted rule structure derived from the captured state.
        shape: ProfileShapeV2,
    },
    /// Opt-in task-specific visualization leaf family with unchanged v2 baseline proof.
    V3 {
        /// Captured family and accepted visualization namespace.
        shape: ProfileShapeV3,
    },
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
    /// Versioned comparison data; its digest is the template's identity, so two accepted
    /// templates never share one (each generation has a separate digest domain).
    shape: TemplateShape,
}

/// Holds Execution-owned templates whose real evidence permits physical effects.
#[derive(Clone, Debug)]
pub struct ExecutionProfileCatalog {
    /// Up to [`MAX_ACCEPTED_PROFILES`] tested templates; several may share one class (T25B).
    templates: BTreeMap<ProfileClass, Vec<ExecutionProfileTemplate>>,
}

/// Is the durable, Execution-owned record that Application may store without interpreting it.
///
/// Every identity is an opaque, nonempty value supplied by the verified Execution evidence
/// pipeline.  The record deliberately stores the semantic state digest alongside its separate
/// provider/toolchain/config/trust/transport identities: matching a template name alone never
/// makes a changed profile executable.
///
/// Records are versioned by `shape_version` (T35B): an absent field is the legacy v1 layout and
/// keeps its exact digest algorithms and field set byte-for-byte; explicit `2` or `3` adds the
/// field, stores its domain-separated shape digest in `permission_value`, and stores the
/// domain-separated digest of the complete captured state in `semantic_state`. Any other value,
/// an explicit `1`, or an unknown field fails closed; a v1 record is never silently upgraded or
/// reinterpreted.
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
    /// V1: effective permission-value identity with only worktree-local path prefixes made
    /// portable. V2/V3: the generation-specific profile-shape digest of the captured state.
    pub permission_value: String,
    /// Immutable D03 evidence identity for this tested record.
    pub d03_evidence: String,
    /// V1: semantic state identity with only worktree-local path prefixes made portable.
    /// V2/V3: the domain-separated canonical digest of the complete captured state, including its
    /// actual cwd and all restrictions.
    pub semantic_state: String,
    /// `None` is v1; `Some(2)` and `Some(3)` select the closed shape generations.
    shape_version: Option<u32>,
}

/// The record's eleven v1 identity fields, in the v1 JSON layout order.
const RECORD_IDENTITY_FIELDS: &[&str] = &[
    "profile_id",
    "revision",
    "class",
    "provider_binary",
    "toolchain",
    "configuration",
    "trust",
    "transport",
    "permission_value",
    "d03_evidence",
    "semantic_state",
];

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
    /// never from a model request or a persisted record being replayed. This constructor mints
    /// the legacy v1 layout; see [`PersistedProfileRecord::from_execution_evidence_versioned`].
    pub fn from_execution_evidence(
        profile_id: impl Into<String>,
        revision: u32,
        evidence: D03ProfileEvidence,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        Self::from_execution_evidence_versioned(profile_id, revision, evidence, state, 1)
    }

    /// Creates a v2 record from one verified D03 result and the exact captured state (T35B).
    ///
    /// `permission_value` becomes the domain-separated profile-shape digest and
    /// `semantic_state` the separately domain-separated canonical digest of the complete
    /// captured state, including its actual cwd and all restrictions. The state must derive a
    /// supported profile-shape v2 value; unsupported shapes are denied instead of downgraded.
    pub fn from_execution_evidence_v2(
        profile_id: impl Into<String>,
        revision: u32,
        evidence: D03ProfileEvidence,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        Self::from_execution_evidence_versioned(profile_id, revision, evidence, state, 2)
    }

    /// Builds one versioned record; `shape_version` is 1, 2, or opt-in visualization v3.
    ///
    /// Minting v3 reads the current visualization namespace and leaf metadata and refuses
    /// unsupported or redirected paths. Restoration later skips old visualization filesystem
    /// checks while retaining the existing v2 cwd binding.
    pub fn from_execution_evidence_versioned(
        profile_id: impl Into<String>,
        revision: u32,
        evidence: D03ProfileEvidence,
        state: &HostSandboxState,
        shape_version: u32,
    ) -> Result<Self, RequestError> {
        let (permission_value, semantic_state, shape_version) = match shape_version {
            1 => (
                state.profile_digest().to_hex().to_string(),
                semantic_state_identity(state),
                None,
            ),
            2 => {
                let shape = state
                    .shape_v2(state.cwd())
                    .map_err(|_| RequestError::ExecutionProfileDenied)?;
                (
                    shape.digest().to_hex().to_string(),
                    captured_state_identity_v2(state),
                    Some(2),
                )
            }
            3 => {
                let shape = state
                    .shape_v3(state.cwd())
                    .map_err(|_| RequestError::ExecutionProfileDenied)?;
                (
                    shape.digest().to_hex().to_string(),
                    captured_state_identity_v2(state),
                    Some(3),
                )
            }
            _ => return Err(RequestError::ExecutionProfileDenied),
        };
        let record = Self {
            profile_id: profile_id.into(),
            revision,
            class: state.class(),
            provider_binary: evidence.provider_binary,
            toolchain: evidence.toolchain,
            configuration: evidence.configuration,
            trust: evidence.trust,
            transport: evidence.transport,
            permission_value,
            d03_evidence: evidence.d03_evidence,
            semantic_state,
            shape_version,
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
    /// This is the version-aware, closed record parser (T35B): the field set must be exactly the
    /// eleven v1 identities, or exactly those plus `shape_version: 2` or `3`. Empty identities, zero
    /// revisions, malformed JSON, unknown or missing fields, an explicit `shape_version: 1`
    /// (a mixed layout: v1 records never carried the field), or any unsupported version value are
    /// unavailable. Application only persists the returned JSON; permit minting stays in
    /// `ExecutionProfileCatalog`.
    pub fn from_json(json: &str) -> Result<Self, RequestError> {
        let value: Value =
            serde_json::from_str(json).map_err(|_| RequestError::ExecutionProfileDenied)?;
        let object = value
            .as_object()
            .ok_or(RequestError::ExecutionProfileDenied)?;
        let shape_version = match object.get("shape_version") {
            None => None,
            Some(value) => {
                if !matches!(value.as_u64(), Some(2 | 3)) {
                    return Err(RequestError::ExecutionProfileDenied);
                }
                Some(value.as_u64().unwrap() as u32)
            }
        };
        let expected_len = RECORD_IDENTITY_FIELDS.len() + usize::from(shape_version.is_some());
        if object.len() != expected_len
            || !object
                .keys()
                .all(|key| RECORD_IDENTITY_FIELDS.contains(&key.as_str()) || key == "shape_version")
        {
            return Err(RequestError::ExecutionProfileDenied);
        }
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
            shape_version,
        })
    }

    /// Serializes this complete record in a stable field layout for Application's opaque store.
    ///
    /// A v1 record emits exactly eleven fields; v2 and v3 add their shape version.
    pub fn to_json(&self) -> String {
        let mut record = serde_json::json!({
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
        });
        if let Some(shape_version) = self.shape_version {
            record["shape_version"] = Value::from(shape_version);
        }
        record.to_string()
    }

    /// Returns whether this durable record is exactly applicable to the supplied observed state.
    ///
    /// v1 compares the legacy portable identities exactly. v2 and v3 verify both of their
    /// identities against the supplied capture: the domain-separated shape digest and the
    /// domain-separated digest of the complete captured state (T35B). Restoration uses exact
    /// evidence matching; v3 checks the captured visualization structure without requiring its
    /// old namespace to still exist. Live filesystem checks occur later in `permit` and at spawn.
    pub fn matches_state(&self, state: &HostSandboxState) -> bool {
        match self.shape_version {
            None => {
                self.semantic_state == semantic_state_identity(state)
                    && self.permission_value == state.profile_digest().to_hex().to_string()
            }
            Some(2) => match state.shape_v2(state.cwd()) {
                Ok(shape) => {
                    self.semantic_state == captured_state_identity_v2(state)
                        && self.permission_value == shape.digest().to_hex().to_string()
                }
                Err(_) => false,
            },
            Some(3) => match state.shape_v3_stored(state.cwd()) {
                Ok(shape) => {
                    self.semantic_state == captured_state_identity_v2(state)
                        && self.permission_value == shape.digest().to_hex().to_string()
                }
                Err(_) => false,
            },
            Some(_) => false,
        }
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

impl ExecutionProfilePermit {
    /// Rechecks a v3 live leaf and namespace against this exact accepted family before spawn.
    ///
    /// `trusted_cwd` is the same candidate used at permit admission. V1/v2 require no extra
    /// filesystem access; malformed paths, a missing namespace, redirection, or widening refuse.
    fn recheck_live_v3(
        &self,
        state: &HostSandboxState,
        trusted_cwd: &Path,
    ) -> Result<(), RequestError> {
        if let TemplateShape::V3 { shape } = &self.template.shape {
            let live = state.shape_v3(trusted_cwd).map_err(|_| {
                RequestError::ExecutionProfileShapeUnsupported(ProfileClass::Managed)
            })?;
            if !live.prove_narrower(shape) {
                return Err(RequestError::ExecutionProfileShapeNotNarrower(
                    ProfileClass::Managed,
                ));
            }
        }
        Ok(())
    }
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

    /// Reports whether this exact state already grants read access to the whole filesystem root.
    ///
    /// Only a `managed` class whose own `permissionProfile.file_system` is a recognized closed
    /// restricted profile qualifies; see [`grants_read_of_all_roots`] for the recognized shape.
    /// This is a read-authority classification of the replayed state, never a widening of it: the
    /// raw JSON is untouched and no path is resolved, expanded, or added.
    fn grants_read_of_all_roots(&self) -> bool {
        self.class == ProfileClass::Managed && grants_read_of_all_roots(&self.raw)
    }

    /// Reports whether the live host itself declares whole-tree read coverage for a project
    /// check: managed profiles need a recognized root grant and no denies; a disabled host has
    /// no outer sandbox. Binding, Workspace authority, and catalog admission remain separate.
    pub(crate) fn declares_whole_tree_read(&self) -> bool {
        self.class == ProfileClass::Disabled
            || (self.grants_read_of_all_roots() && !self.has_deny_entries())
    }

    /// Serializes the complete original state for the Codex sandbox command.
    fn json_argument(&self) -> &str {
        &self.raw_json
    }

    /// Returns the exact captured JSON argument retained for managed Codex sandbox replay.
    pub fn sandbox_state_json(&self) -> &str {
        &self.raw_json
    }

    /// Returns a profile digest retaining all permissions except equivalent worktree path prefixes.
    fn profile_digest(&self) -> blake3::Hash {
        blake3::hash(profile_template_value(self).to_string().as_bytes())
    }

    /// Derives the closed profile-shape v2 value of this state, or reports why none exists (T35B).
    ///
    /// `trusted_cwd` is the trusted candidate or Workspace worktree the operation will actually
    /// touch: derivation binds the portable cwd-derived authority to it and refuses any state
    /// whose `sandboxCwd` is a different directory (T35B-r). Restoration- and minting-side
    /// callers pass the captured state's own cwd, which is self-consistent by construction. The
    /// derivation is a read-only closed parse of the raw state; the replay JSON is never
    /// rewritten and nothing is normalized on the host's behalf. An unsupported-shape state
    /// keeps replaying byte-for-byte and can then only be admitted by a legacy v1 template's
    /// exact digest — never by a looser fallback.
    pub(crate) fn shape_v2(&self, trusted_cwd: &Path) -> Result<ProfileShapeV2, UnsupportedShape> {
        ProfileShapeV2::derive(self, trusted_cwd)
    }

    /// Derives a live v3 family, checking current namespace and leaf filesystem metadata.
    ///
    /// Malformed structure, missing namespace, symlinks, or authority outside the family refuse;
    /// this does not change v2 admission or daemon read proofs.
    pub(crate) fn shape_v3(&self, trusted_cwd: &Path) -> Result<ProfileShapeV3, UnsupportedShape> {
        ProfileShapeV3::derive(self, trusted_cwd)
    }

    /// Reconstructs only a stored v3 capture's structure; no old visualization path is opened.
    pub(crate) fn shape_v3_stored(
        &self,
        trusted_cwd: &Path,
    ) -> Result<ProfileShapeV3, UnsupportedShape> {
        ProfileShapeV3::derive_stored(self, trusted_cwd)
    }

    /// Proves one worktree-relative path readable under this state's live cwd-bound shape (T36B).
    ///
    /// The shape is derived against `trusted_cwd` — the authoritative worktree the read will
    /// touch — so a state whose `sandboxCwd` is a different directory refuses derivation and
    /// reports the conservative `Unproven` outcome here like any other unprovable path.
    /// This is a pure permission proof of the already-validated state: no filesystem I/O, no
    /// admission change, and no widening of the replayed state.
    pub(crate) fn proves_read_path(&self, trusted_cwd: &Path, relative_path: &Path) -> bool {
        match self.shape_v2(trusted_cwd) {
            Ok(shape) => {
                profile_shape::read_proof(&shape, trusted_cwd, relative_path)
                    == profile_shape::ReadProof::Proven
            }
            Err(_) => false,
        }
    }

    /// Reports whether any filesystem entry of the replayed profile denies access (T35B-r).
    ///
    /// Every `deny` entry counts, whatever its selector: a deny-bearing state keeps native reads
    /// and cached delivery unavailable, because whole-tree read coverage cannot be proven for a
    /// state whose own entries subtract from its grants.
    fn has_deny_entries(&self) -> bool {
        self.raw
            .get("permissionProfile")
            .and_then(|profile| profile.get("file_system"))
            .and_then(|file_system| file_system.get("entries"))
            .and_then(Value::as_array)
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|entry| entry.get("access").and_then(Value::as_str) == Some("deny"))
            })
    }
}

impl ExecutionProfileTemplate {
    /// Defines a nonempty, tested Execution profile template and its owned revision.
    ///
    /// This constructor mints the legacy v1 exact-digest template; behaviour is unchanged.
    /// See [`ExecutionProfileTemplate::from_execution_evidence_v2`] for the conservative
    /// shape-based generation (T35B).
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
            shape: TemplateShape::V1 {
                profile_digest: state.profile_digest(),
            },
        })
    }

    /// Defines a v2 template carrying the captured state's derived rule structure (T35B).
    ///
    /// The state must derive a supported v2 shape; an unsupported shape is denied
    /// outright instead of being stored as a looser comparison. The template's identity is the
    /// domain-separated shape digest, so a v2 and a v1 template over the same state remain two
    /// distinct accepted identities.
    pub fn from_execution_evidence_v2(
        id: impl Into<String>,
        version: u32,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        let id = id.into();
        if id.is_empty() || version == 0 {
            return Err(RequestError::ExecutionProfileDenied);
        }
        let shape = state
            .shape_v2(state.cwd())
            .map_err(|_| RequestError::ExecutionProfileDenied)?;
        Ok(Self {
            id,
            version,
            class: state.class,
            shape: TemplateShape::V2 { shape },
        })
    }

    /// Defines one explicitly accepted v3 visualization family from its exact D03 capture.
    ///
    /// The capture's live namespace and existing path components are checked before minting;
    /// empty identity, zero version, or unsupported/unsafe profile states refuse.
    pub fn from_execution_evidence_v3(
        id: impl Into<String>,
        version: u32,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        let id = id.into();
        if id.is_empty() || version == 0 {
            return Err(RequestError::ExecutionProfileDenied);
        }
        let shape = state
            .shape_v3(state.cwd())
            .map_err(|_| RequestError::ExecutionProfileDenied)?;
        Ok(Self {
            id,
            version,
            class: state.class,
            shape: TemplateShape::V3 { shape },
        })
    }

    /// Builds this template from one version-aware persisted record and its captured state.
    ///
    /// `record.matches_state` has already verified both identities against `state`; this only
    /// re-derives the comparison data the permit proof needs. V3 derives stored structure even
    /// after its old visualization namespace is removed; malformed captures still refuse.
    fn from_record(
        record: &PersistedProfileRecord,
        state: &HostSandboxState,
    ) -> Result<Self, RequestError> {
        let shape = match record.shape_version {
            None => TemplateShape::V1 {
                profile_digest: state.profile_digest(),
            },
            Some(2) => TemplateShape::V2 {
                shape: state
                    .shape_v2(state.cwd())
                    .map_err(|_| RequestError::ExecutionProfileDenied)?,
            },
            Some(3) => TemplateShape::V3 {
                shape: state
                    .shape_v3_stored(state.cwd())
                    .map_err(|_| RequestError::ExecutionProfileDenied)?,
            },
            Some(_) => return Err(RequestError::ExecutionProfileDenied),
        };
        Ok(Self {
            id: record.profile_id.clone(),
            version: record.revision,
            class: record.class,
            shape,
        })
    }

    /// Returns the template's authority identity: two accepted templates never share one.
    ///
    /// Version digests are domain-separated, so a legacy digest and a shape digest can never
    /// collide or be cross-compared by accident.
    fn identity(&self) -> blake3::Hash {
        match &self.shape {
            TemplateShape::V1 { profile_digest } => *profile_digest,
            TemplateShape::V2 { shape } => shape.digest(),
            TemplateShape::V3 { shape } => shape.digest(),
        }
    }
}

impl ExecutionProfileCatalog {
    /// Builds the catalog from Execution-accepted D03 or disabled-host evidence, not Application policy.
    ///
    /// The catalog accepts at most [`MAX_ACCEPTED_PROFILES`] templates, and several templates may
    /// cover one profile class (T25B): a read-only and a workspace-write managed state can both be
    /// accepted. A template's identity is its `profile_digest`, so an exact duplicate digest is
    /// denied exactly as before; admitting it twice would add no tested evidence.
    pub fn from_execution_evidence(
        templates: Vec<ExecutionProfileTemplate>,
    ) -> Result<Self, RequestError> {
        if templates.len() > MAX_ACCEPTED_PROFILES {
            return Err(RequestError::ExecutionProfileDenied);
        }
        let mut entries: BTreeMap<ProfileClass, Vec<ExecutionProfileTemplate>> = BTreeMap::new();
        for template in templates {
            if entries
                .values()
                .flatten()
                .any(|accepted| accepted.identity() == template.identity())
            {
                return Err(RequestError::ExecutionProfileDenied);
            }
            entries.entry(template.class).or_default().push(template);
        }
        Ok(Self { templates: entries })
    }

    /// Rebuilds a usable catalog only from records matching trusted expected D03 evidence.
    ///
    /// `expected` comes from Execution-owned trusted configuration/evidence, never the durable
    /// store or a model request. The caller supplies each current D01-bound state; extra, missing,
    /// stale, corrupt, duplicate-digest, value-mismatched, unknown-version, or mixed-layout
    /// records are unavailable. Records restore by exact evidence matching: a shape record rebuilds
    /// the captured rule structure from its own stored capture, never by subtyping (T35B).
    pub fn from_persisted_records(
        records: Vec<(PersistedProfileRecord, HostSandboxState)>,
        expected: &[PersistedProfileRecord],
    ) -> Result<Self, RequestError> {
        if records.len() != expected.len() || records.len() > MAX_ACCEPTED_PROFILES {
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
            templates.push(ExecutionProfileTemplate::from_record(&record, &state)?);
        }
        Self::from_execution_evidence(templates)
    }

    /// Mints an Execution-owned permit when one accepted template of the observed class admits
    /// the live state, matching one complete template and never combining grants (T25B, T35B).
    ///
    /// `trusted_cwd` is the trusted candidate or Workspace worktree the operation will actually
    /// touch: v2/v3 candidates derive with the portable cwd bound to it, so a live state whose
    /// `sandboxCwd` is not this directory never derives either shape here and can only be admitted
    /// by a v1 template's exact legacy digest (T35B-r).
    ///
    /// The decision is ordered and fail-closed:
    ///
    /// 1. no template for the class → [`RequestError::ExecutionProfileNoTemplate`];
    /// 2. an exact v2 or v3 shape match is preferred;
    /// 3. otherwise a v1 template whose legacy digest equals the live state's keeps its
    ///    unchanged exact-digest admission;
    /// 4. otherwise a v2 or v3 template proving the live shape a narrower authority admits, chosen
    ///    deterministically by template identity;
    /// 5. no passing candidate refuses with the most specific closed reason: a live state that
    ///    derives no applicable shape is [`RequestError::ExecutionProfileShapeUnsupported`], a state all
    ///    shape templates fail to prove narrower is
    ///    [`RequestError::ExecutionProfileShapeNotNarrower`], and a class with only v1 templates
    ///    whose digests all differ stays [`RequestError::ExecutionProfileDigestMismatch`].
    ///
    /// Unsupported semantics never fall back to a looser comparison, and the permit keeps the
    /// live raw-state correlation digest: it establishes *which* exact state ran, never subset
    /// containment.
    fn permit(
        &self,
        state: &HostSandboxState,
        trusted_cwd: &Path,
    ) -> Result<ExecutionProfilePermit, RequestError> {
        let class_templates = self
            .templates
            .get(&state.class)
            .ok_or(RequestError::ExecutionProfileNoTemplate(state.class))?;
        let live_digest = state.profile_digest();
        let live_shape = state.shape_v2(trusted_cwd).ok();
        let live_v3 = class_templates
            .iter()
            .any(|template| matches!(&template.shape, TemplateShape::V3 { .. }))
            .then(|| state.shape_v3(trusted_cwd).ok())
            .flatten();
        let mut v1_match = None;
        let mut v2_exact = None;
        let mut v3_exact = None;
        let mut v2_narrower: Vec<&ExecutionProfileTemplate> = Vec::new();
        let mut has_v2 = false;
        let mut has_v3 = false;
        for template in class_templates {
            match &template.shape {
                TemplateShape::V1 { profile_digest } => {
                    if *profile_digest == live_digest {
                        v1_match = Some(template);
                    }
                }
                TemplateShape::V2 { shape } => {
                    has_v2 = true;
                    let Some(live) = &live_shape else {
                        continue;
                    };
                    if *live == *shape {
                        v2_exact = Some(template);
                    } else if live.prove_narrower(shape) {
                        v2_narrower.push(template);
                    }
                }
                TemplateShape::V3 { shape } => {
                    has_v3 = true;
                    let Some(live) = &live_v3 else {
                        continue;
                    };
                    if live == shape {
                        v3_exact = Some(template);
                    } else if live.prove_narrower(shape) {
                        v2_narrower.push(template);
                    }
                }
            }
        }
        let template = if let Some(template) = v2_exact.or(v3_exact) {
            template.clone()
        } else if let Some(template) = v1_match {
            template.clone()
        } else {
            v2_narrower.sort_by_key(|template| (template.id.clone(), template.version));
            match v2_narrower.first() {
                Some(template) => (*template).clone(),
                None => {
                    return Err(
                        match (has_v2 || has_v3, live_shape.is_some() || live_v3.is_some()) {
                            (false, _) => RequestError::ExecutionProfileDigestMismatch(state.class),
                            (true, false) => {
                                RequestError::ExecutionProfileShapeUnsupported(state.class)
                            }
                            (true, true) => {
                                RequestError::ExecutionProfileShapeNotNarrower(state.class)
                            }
                        },
                    );
                }
            }
        };
        Ok(ExecutionProfilePermit {
            template,
            state_digest: blake3::hash(state.json_argument().as_bytes()),
        })
    }
}

/// Directory below the real `.agent-ide` home holding captured rejected sandbox states (T25B).
pub const REJECTED_PROFILES_DIR: &str = "rejected-profiles";

/// Soft cap on captured rejected sandbox states retained for operator review (T25B).
///
/// There is no locking: concurrent daemons on one machine can transiently exceed this cap. The
/// check is best-effort housekeeping, never a security boundary.
const MAX_REJECTED_PROFILES: usize = 16;

/// Maximum captured rejected-state JSON bytes, matching `evidence record --sandbox-state` (T25B).
const MAX_REJECTED_STATE_BYTES: usize = 64 * 1024;

/// Outcome of one best-effort rejected-state capture attempt (T25B).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RejectedCapture {
    /// A new private capture file was written; the stem is its 16-hex file name.
    Captured(String),
    /// The state carried a top-level field outside the documented envelope; nothing was written.
    UnknownFields,
    /// Any other silent skip (unsafe directory, soft cap, existing file, I/O); nothing was written.
    Unavailable,
}

/// One captured rejected host sandbox state offered for operator review (T25B).
#[derive(Clone, Debug)]
pub struct RejectedProfileCapture {
    /// 16-hex file-name stem, the first half of the state's profile digest.
    pub name: String,
    /// Profile class parsed back from the captured state.
    pub class: ProfileClass,
    /// Exact `sandboxCwd` value retained inside the captured state.
    pub sandbox_cwd: String,
    /// File modification time, when the filesystem reports one.
    pub modified: Option<std::time::SystemTime>,
}

/// Captures one rejected observed sandbox state for operator review; best-effort, never authorizing.
///
/// The file holds the exact bounded JSON envelope `agent-ide evidence record --sandbox-state`
/// reads, written private (mode 0600 at creation, `O_NOFOLLOW`) below the real user home under
/// [`REJECTED_PROFILES_DIR`]. The directory is created private (0700) at birth and is used only
/// when `symlink_metadata` confirms a real directory owned by the current uid with no group/other
/// permission bits; an existing directory is never chmod'ed. A capture is refused with
/// [`RejectedCapture::UnknownFields`] for any state carrying a top-level field outside the
/// documented sandbox-state envelope: the profile digest covers unknown fields, so a filtered
/// copy would be useless and the unfiltered state could carry secrets. An existing capture is
/// never overwritten and every failure is silent: a capture never changes an admission reply or
/// becomes an authority input.
pub fn capture_rejected_profile(state_json: &Value) -> RejectedCapture {
    let Some(home) = crate::userhome::user_home() else {
        return RejectedCapture::Unavailable;
    };
    capture_rejected_profile_in(&home, state_json)
}

/// [`capture_rejected_profile`] below an explicit home; the seam exists only for tests.
pub(crate) fn capture_rejected_profile_in(home: &Path, state_json: &Value) -> RejectedCapture {
    let Ok(state) = HostSandboxState::parse(Some(state_json.clone())) else {
        return RejectedCapture::Unavailable;
    };
    if !state_json.as_object().is_some_and(|object| {
        object
            .keys()
            .all(|key| SANDBOX_STATE_FIELDS.contains(&key.as_str()))
    }) {
        return RejectedCapture::UnknownFields;
    }
    let json = state_json.to_string();
    if json.len() > MAX_REJECTED_STATE_BYTES {
        return RejectedCapture::Unavailable;
    }
    let dir = home.join(".agent-ide").join(REJECTED_PROFILES_DIR);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if builder.create(&dir).is_err() {
        return RejectedCapture::Unavailable;
    }
    #[cfg(unix)]
    if !usable_private_directory(&dir) {
        return RejectedCapture::Unavailable;
    }
    if std::fs::read_dir(&dir).is_ok_and(|entries| entries.count() >= MAX_REJECTED_PROFILES) {
        return RejectedCapture::Unavailable;
    }
    let hex = state.profile_digest().to_hex().to_string();
    let stem = hex[..16].to_owned();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let Ok(mut file) = options.open(dir.join(format!("{stem}.json"))) else {
        return RejectedCapture::Unavailable;
    };
    if file.write_all(json.as_bytes()).is_err() {
        return RejectedCapture::Unavailable;
    }
    RejectedCapture::Captured(stem)
}

/// Reports whether `dir` is a real, privately owned directory fit for captures (T25B).
///
/// `symlink_metadata` never follows links, so a symlinked directory fails `is_dir`; a directory
/// this process does not own, or one readable or writable by group or other, is equally refused.
#[cfg(unix)]
fn usable_private_directory(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) => {
            // SAFETY: `getuid` merely reports the calling thread's real uid.
            metadata.is_dir()
                && metadata.uid() == unsafe { libc::getuid() }
                && metadata.permissions().mode() & 0o077 == 0
        }
        Err(_) => false,
    }
}

/// Lists the captured rejected sandbox states below the real user home, ordered by name (T25B).
///
/// Only strictly well-formed captures are listed: a name of exactly 16 lowercase hex digits plus
/// `.json`, a regular file of at most 64 KiB opened `O_NOFOLLOW`, and parseable JSON. Symlinks,
/// special files, oversized or foreign entries are ignored silently, and at most 16 entries are
/// returned.
pub fn list_rejected_profiles() -> Vec<RejectedProfileCapture> {
    let Some(home) = crate::userhome::user_home() else {
        return Vec::new();
    };
    list_rejected_profiles_in(&home)
}

/// [`list_rejected_profiles`] below an explicit home; the seam exists only for tests.
pub(crate) fn list_rejected_profiles_in(home: &Path) -> Vec<RejectedProfileCapture> {
    let Ok(entries) = std::fs::read_dir(home.join(".agent-ide").join(REJECTED_PROFILES_DIR)) else {
        return Vec::new();
    };
    let mut captures = Vec::new();
    for entry in entries.flatten() {
        let Some(capture) = parse_rejected_entry(&entry) else {
            continue;
        };
        captures.push(capture);
    }
    captures.sort_by(|first, second| first.name.cmp(&second.name));
    captures.truncate(MAX_REJECTED_PROFILES);
    captures
}

/// Reads one directory entry as a well-formed rejected-state capture, or [`None`] to skip it.
fn parse_rejected_entry(entry: &std::fs::DirEntry) -> Option<RejectedProfileCapture> {
    let file_name = entry.file_name();
    let name = file_name.to_str()?.strip_suffix(".json")?;
    // Capture stems are always the lowercase hex prefix of a profile digest.
    if name.len() != 16
        || !name
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    // Symlink metadata never follows links: symlinks and special files fail `is_file`.
    let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_REJECTED_STATE_BYTES as u64 {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(entry.path()).ok()?;
    let mut bytes = Vec::new();
    let mut bounded = file.take(MAX_REJECTED_STATE_BYTES as u64 + 1);
    bounded.read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_REJECTED_STATE_BYTES {
        return None;
    }
    let state = HostSandboxState::parse_json(std::str::from_utf8(&bytes).ok()?).ok()?;
    Some(RejectedProfileCapture {
        name: name.to_owned(),
        class: state.class(),
        sandbox_cwd: state.sandbox_cwd().to_owned(),
        modified: metadata.modified().ok(),
    })
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
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            policy.git_program.clone(),
            args,
            self.invocation.sandbox.cwd().to_path_buf(),
            BTreeMap::new(),
        )?;
        // The raw candidate is the trusted directory this discovery will actually run in: a v2
        // candidate binds the portable sandbox-cwd authority to it, so discovery with a
        // candidate other than the state's own `sandboxCwd` never derives a v2 shape (T35B-r).
        let permit = catalog.permit(&self.invocation.sandbox, Path::new(&self.candidate_cwd))?;
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
        if !is_normal_absolute(&git_program)
            || output_cap == 0
            || output_cap > MAX_GIT_DISCOVERY_BYTES
        {
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
    /// Execution profile evidence associated with this operation.
    permit: ExecutionProfilePermit,
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
    /// Returns an owned fixed-query handle after checking v3 paths and fresh liveness at spawn.
    ///
    /// The fixed `-C` candidate is rechecked against the same v3 family used at admission;
    /// failure returns the unique no-child settlement with a closed request error. The handle
    /// retains operation/query provenance and can be cancelled while a borrowed wait runs.
    pub fn spawn(
        self,
        lease: AdmissionLease,
        active_use: ActiveBindingUse,
        codex_executable: &Path,
    ) -> Result<OwnedGitDiscovery, ProcessError> {
        let started = std::time::Instant::now();
        let settlement = SpawnNeverStarted::ordinary(lease);
        // validate_query fixes argv as `-C <candidate> <query>`; the candidate is the permit cwd.
        if let Err(error) = self
            .permit
            .recheck_live_v3(&self.invocation.sandbox, Path::new(&self.command.args[1]))
        {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        if let Err(error) = self.invocation.consume_active_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        let process = OwnedChild::spawn_parts(
            &self.command,
            &self.invocation.sandbox,
            settlement,
            codex_executable,
            None,
            self.output_cap,
        )?;
        Ok(OwnedGitDiscovery {
            process,
            operation: self.operation,
            query: self.query,
            started,
            _permit: self.permit,
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
    /// Catalog evidence retained through physical execution.
    _permit: ExecutionProfilePermit,
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

/// The only trampoline program this contract accepts; an arbitrary script is never accepted.
const TRAMPOLINE_PROGRAM: &str = "/usr/bin/env";

/// Carries the operator-accepted `/usr/bin/env` used to run a validated command in the
/// authoritative worktree while the host sandbox state is replayed byte-for-byte.
///
/// The trampoline exists for exactly one case: a managed host whose `sandboxCwd` is the parent's
/// inherited directory rather than this worktree. `codex sandbox` sets the child cwd from that
/// state, so the utility is wrapped as `env -C <authority root> <program> <args>` *inside* the
/// unchanged sandbox argv. It grants nothing: the sandbox policy, its raw JSON, and the program
/// allowlist are all unchanged, and `env` execs the same child, so process, group, and lease
/// accounting still describe one direct child.
///
/// Acceptance is pinned to the operator's own declaration, never to whatever bytes happen to be on
/// disk when a request is built: the measured digest must equal the declared BLAKE3 accepted at
/// configuration load, and only then is the measured object identity sealed. That sealed identity
/// is rechecked immediately before spawn, so an executable replaced at any point — before or after
/// acceptance — fails closed with [`RequestError::ExecutableUnavailable`] instead of being
/// re-sealed as trusted and launched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlledTrampoline {
    /// Absolute accepted trampoline path; always exactly [`TRAMPOLINE_PROGRAM`].
    path: PathBuf,
    /// Executable object and byte identity measured when the operator's declaration was accepted.
    identity: ExecutableIdentity,
}

impl ControlledTrampoline {
    /// Seals one operator-declared trampoline against the exact digest that declaration accepted.
    ///
    /// `path` must be exactly `/usr/bin/env`; `declared_blake3` is the operator's configured
    /// hexadecimal BLAKE3 of that executable, the same value verified once at daemon startup. The
    /// bytes are measured here and compared with it, so a build that happens later in the daemon's
    /// life can never adopt a changed executable as a new baseline.
    ///
    /// Returns [`RequestError::TrampolineUnavailable`] for any other path and on every platform
    /// other than macOS, where this contract's `env -C` behaviour and the nested-sandbox
    /// constraints were actually established; Linux acceptance is deliberately not offered rather
    /// than assumed. Returns [`RequestError::ExecutableUnavailable`] when the path is not one
    /// stable readable regular executable object, or when its measured bytes do not match
    /// `declared_blake3`.
    pub fn accept(path: PathBuf, declared_blake3: &str) -> Result<Self, RequestError> {
        if path != Path::new(TRAMPOLINE_PROGRAM) || !cfg!(target_os = "macos") {
            return Err(RequestError::TrampolineUnavailable);
        }
        let identity = executable_identity(&path)?;
        if !identity
            .digest
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(declared_blake3)
        {
            return Err(RequestError::ExecutableUnavailable);
        }
        Ok(Self { path, identity })
    }

    /// Returns the accepted path after rechecking that the executable object is byte-unchanged.
    ///
    /// The comparison is against the identity sealed at acceptance, which itself had to match the
    /// operator's declared digest. Returns [`RequestError::ExecutableUnavailable`] when the object
    /// or its bytes changed since then, so a swapped trampoline can never be executed.
    fn verified_path(&self) -> Result<&Path, RequestError> {
        if executable_identity(&self.path)? != self.identity {
            return Err(RequestError::ExecutableUnavailable);
        }
        Ok(&self.path)
    }

    /// Rejects a program path BSD `env` would consume as an environment assignment.
    ///
    /// `env` treats its first non-flag argument containing `=` as `NAME=VALUE` rather than the
    /// utility to exec, so such a program is refused before spawn instead of silently launching a
    /// different process.
    fn accepts_program(program: &Path) -> Result<(), RequestError> {
        if program.as_os_str().as_encoded_bytes().contains(&b'=') {
            return Err(RequestError::TrampolineProgramRejected);
        }
        Ok(())
    }

    /// Returns the extra argv bytes this trampoline adds, so local ceilings still bound the spawn.
    fn additional_argv_bytes(command: &ControlledCommand) -> usize {
        os_bytes(&OsString::from("-C"))
            + os_bytes(&command.cwd.clone().into_os_string())
            + os_bytes(&command.program.clone().into_os_string())
    }
}

/// Product argv ceiling handed to [`LocalExecutionPolicy`]: the bound snapshot batch planners
/// must fail closed against, so the ceiling lives here once instead of a duplicated literal.
pub const MAX_PRODUCT_ARGV_BYTES: usize = 64 * 1024;

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
    /// Operator-accepted `env` trampoline; `None` leaves a differing sandbox cwd unavailable.
    trampoline: Option<ControlledTrampoline>,
}

impl LocalExecutionPolicy {
    /// Creates a policy whose program allowlist and numeric limits are independently validated.
    ///
    /// The resulting policy has no trampoline, so a command whose worktree differs from the host's
    /// own `sandboxCwd` remains rejected; use [`LocalExecutionPolicy::with_env_trampoline`] to
    /// accept one.
    pub fn new(
        allowed_programs: BTreeSet<PathBuf>,
        max_argv_bytes: usize,
        max_environment_entries: usize,
        allow_explicit_disabled_host: bool,
    ) -> Result<Self, RequestError> {
        Self::build(
            allowed_programs,
            max_argv_bytes,
            max_environment_entries,
            allow_explicit_disabled_host,
            None,
        )
    }

    /// Creates a policy that may additionally run a validated command in an authoritative worktree
    /// other than the managed host's own `sandboxCwd`, through `trampoline`.
    ///
    /// The trampoline is used only when the observed managed profile already grants read of `/`;
    /// it never relaxes the program allowlist, the worktree authority check, or the replayed
    /// sandbox state.
    pub fn with_env_trampoline(
        allowed_programs: BTreeSet<PathBuf>,
        max_argv_bytes: usize,
        max_environment_entries: usize,
        allow_explicit_disabled_host: bool,
        trampoline: ControlledTrampoline,
    ) -> Result<Self, RequestError> {
        Self::build(
            allowed_programs,
            max_argv_bytes,
            max_environment_entries,
            allow_explicit_disabled_host,
            Some(trampoline),
        )
    }

    /// Validates the shared ceilings both constructors require.
    fn build(
        allowed_programs: BTreeSet<PathBuf>,
        max_argv_bytes: usize,
        max_environment_entries: usize,
        allow_explicit_disabled_host: bool,
        trampoline: Option<ControlledTrampoline>,
    ) -> Result<Self, RequestError> {
        if allowed_programs.is_empty() || max_argv_bytes == 0 {
            return Err(RequestError::InvalidPolicy);
        }
        Ok(Self {
            allowed_programs,
            max_argv_bytes,
            max_environment_entries,
            allow_explicit_disabled_host,
            trampoline,
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
    /// Execution has no accepted template for the observed host profile class (T24B).
    ExecutionProfileNoTemplate(ProfileClass),
    /// The observed profile digest differs from every accepted template for this class (T24B).
    ExecutionProfileDigestMismatch(ProfileClass),
    /// The live managed state derives no profile-shape v2 value, so no accepted v2 template
    /// could even evaluate it; only a v1 template's exact legacy digest could admit it (T35B).
    ExecutionProfileShapeUnsupported(ProfileClass),
    /// The live state derives a v2 shape but no accepted template proves it a narrower authority
    /// under the seven sufficient conditions, and no v1 digest matched (T35B).
    ExecutionProfileShapeNotNarrower(ProfileClass),
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
    /// Fixed-query fixture/capture metadata is oversized or contradicts its exit/drain evidence.
    InvalidDiscoveryEvidence,
    /// A differing-cwd managed launch has no operator-accepted `/usr/bin/env` trampoline, or this
    /// platform is not one where the trampoline contract is supported.
    TrampolineUnavailable,
    /// A trampolined program path would be misread by BSD `env` as an environment assignment.
    TrampolineProgramRejected,
    /// A managed state without whole-tree read coverage could not prove one exact path under
    /// the conservative per-path read proof (T36B). Distinct from a cwd mismatch: the binding
    /// was sound, but grants, denies, or matcher ambiguity leave this path unprovable.
    ReadPathUnproven,
    /// A managed state could not prove that every path in the worktree is readable.
    ReadWholeTreeUnproven,
}

/// The read scope one workspace-read recheck must prove (T36B).
///
/// `Path` names one exact worktree-relative path whose native read or cached disclosure is
/// being authorized; `WholeTree` is the legacy restrictive behavior for callers that cannot
/// name every path they may touch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadScope<'a> {
    /// Prove this exact worktree-relative path under the live cwd-bound shape.
    Path(&'a Path),
    /// Prove whole-tree read authority; deny-bearing managed states always refuse.
    WholeTree,
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
    /// Accepted `env` trampoline retained only when the worktree differs from the host sandbox cwd.
    trampoline: Option<ControlledTrampoline>,
}

/// Rechecks current host state for a durable-authorized Workspace read or cached-result delivery.
/// Consumes fresh binding liveness, requires the accepted current profile, and applies local
/// disabled-host policy without fabricating a command, child, or new Workspace grant.
///
/// A managed state must independently prove read authority of the live state, even when its
/// `sandboxCwd` already is this worktree root (T35B-r): admission through a narrower v2 shape
/// must never confer read authority. [`ReadScope::WholeTree`] keeps the restrictive T35B
/// behavior — the recognized whole-root read grant AND no deny entries at all (path or glob).
/// [`ReadScope::Path`] first tries that same deny-free fast path (it skips only policy
/// matching, never containment or no-follow enforcement) and otherwise derives the live
/// cwd-bound shape v2 and requires the conservative per-path read proof (T36B): the
/// derivation already refuses any state whose `sandboxCwd` is not the authoritative root, so
/// a relocated grant cannot vouch for this directory. A disabled host keeps the legacy strict
/// cwd equality. Nothing else is relaxed: a stale binding, a mismatched authority, or an
/// unaccepted profile still fails, and no permission is rewritten or widened.
pub fn validate_workspace_read(
    active_use: ActiveBindingUse,
    observed: ObservedSandboxState,
    authority: &WorkspaceAuthority,
    catalog: &ExecutionProfileCatalog,
    allow_explicit_disabled_host: bool,
    scope: ReadScope<'_>,
) -> Result<ExecutionProfilePermit, RequestError> {
    let invocation = ValidatedHostInvocation::from_active_observation(active_use, observed)?;
    let whole_tree = invocation.sandbox.declares_whole_tree_read();
    let read_proven = match invocation.sandbox.class() {
        ProfileClass::Managed => match scope {
            ReadScope::WholeTree => whole_tree,
            ReadScope::Path(path) => {
                whole_tree || invocation.sandbox.proves_read_path(authority.root(), path)
            }
        },
        ProfileClass::Disabled => invocation.sandbox.cwd() == authority.root(),
    };
    if !read_proven {
        // Whole-tree refusal is not a cwd mismatch: a deny-bearing managed profile can have
        // the exact workspace cwd while still lacking authority for every path.
        return Err(match (invocation.sandbox.class(), scope) {
            (ProfileClass::Managed, ReadScope::Path(_)) => RequestError::ReadPathUnproven,
            (ProfileClass::Managed, ReadScope::WholeTree) => RequestError::ReadWholeTreeUnproven,
            (ProfileClass::Disabled, _) => RequestError::SandboxCwdMismatch,
        });
    }
    if invocation.sandbox.class() == ProfileClass::Disabled && !allow_explicit_disabled_host {
        return Err(RequestError::DisabledHostDenied);
    }
    catalog.permit(&invocation.sandbox, authority.root())
}

impl ValidatedExecutionRequest {
    /// Intersects host state, local policy, and Workspace authority before an admission request.
    ///
    /// The command always runs in the current authoritative worktree. When the managed host's own
    /// `sandboxCwd` is a different inherited directory, that is accepted only when the observed
    /// state already grants read of the whole filesystem root and the policy carries an accepted
    /// `env` trampoline; the request then retains that trampoline so the spawn can enter the
    /// worktree inside the unchanged sandbox argv. Without such a profile the cwd must match
    /// exactly ([`RequestError::SandboxCwdMismatch`]); with such a profile but no accepted
    /// trampoline the request is unavailable ([`RequestError::TrampolineUnavailable`]). The extra
    /// trampoline argv bytes count against `policy.max_argv_bytes`.
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
        let trampoline = if command.cwd == invocation.sandbox.cwd {
            None
        } else if invocation.sandbox.grants_read_of_all_roots() {
            let trampoline = policy
                .trampoline
                .clone()
                .ok_or(RequestError::TrampolineUnavailable)?;
            ControlledTrampoline::accepts_program(&command.program)?;
            Some(trampoline)
        } else {
            return Err(RequestError::SandboxCwdMismatch);
        };
        let argv_bytes = command.args.iter().map(os_bytes).sum::<usize>()
            + trampoline
                .as_ref()
                .map_or(0, |_| ControlledTrampoline::additional_argv_bytes(&command));
        if argv_bytes > policy.max_argv_bytes {
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
        // The Workspace worktree is the trusted directory this child will run in: a v2
        // candidate binds the portable sandbox-cwd authority to it (T35B-r).
        let permit = catalog.permit(&invocation.sandbox, &authority.root)?;
        Ok(Self {
            invocation,
            authority,
            permit,
            command,
            trampoline,
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
    /// queue delay cannot convert pre-stop liveness into a later effect. A v3 request also
    /// rechecks its leaf path immediately before spawn so a queued symlink replacement
    /// refuses. Synthetic/test requests have no active binding and reject no optional use.
    fn consume_spawn_use(&self, active_use: Option<ActiveBindingUse>) -> Result<(), RequestError> {
        self.permit
            .recheck_live_v3(&self.invocation.sandbox, &self.authority.root)?;
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
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        if let Err(error) = capability.validate_request(request) {
            return Err(capability.cancel().error(error));
        }
        let settlement = capability.cancel();
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(
            &request.command,
            &request.invocation.sandbox,
            settlement,
            codex_executable,
            request.trampoline.as_ref(),
            output_cap,
        )
    }

    /// Consumes a direct Git/job reservation; provider commands require a typed registry capability.
    /// Every pre-child failure returns the unique NeverStarted proof for the consumed reservation.
    pub fn spawn_captured(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let settlement = SpawnNeverStarted::ordinary(lease);
        if request.kind() == CommandKind::Provider {
            return Err(settlement.error(provider_capability_required()));
        }
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(
            &request.command,
            &request.invocation.sandbox,
            settlement,
            codex_executable,
            request.trampoline.as_ref(),
            output_cap,
        )
    }

    /// Launches from one linear reservation, returning it only on definite pre-child failure.
    fn spawn_parts(
        command: &ControlledCommand,
        sandbox: &HostSandboxState,
        settlement: SpawnNeverStarted,
        codex_executable: &Path,
        trampoline: Option<&ControlledTrampoline>,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let (mut child, identity) = match launch_child(
            command,
            sandbox,
            &settlement,
            codex_executable,
            trampoline,
            output_cap,
            false,
        ) {
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
    /// Typed reservation target retained unchanged from launch through direct reap.
    target: SpawnTarget,
}

impl OwnedProtocolChild {
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
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        if let Err(error) = capability.validate_request(request) {
            return Err(capability.cancel().error(error));
        }
        let settlement = capability.cancel();
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(request, settlement, codex_executable, output_cap)
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

    /// Consumes a direct Git/job reservation with exclusive protocol stdout; raw providers are refused.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        lease: AdmissionLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let settlement = SpawnNeverStarted::ordinary(lease);
        if request.kind() == CommandKind::Provider {
            return Err(settlement.error(provider_capability_required()));
        }
        if let Err(error) = request.consume_spawn_use(active_use) {
            return Err(settlement.error(ProcessError::Request(error)));
        }
        Self::spawn_parts(request, settlement, codex_executable, output_cap)
    }

    /// Launches one typed or direct reservation while retaining its target and exact child identity.
    fn spawn_parts(
        request: &ValidatedExecutionRequest,
        settlement: SpawnNeverStarted,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        let (mut child, identity) = match launch_child(
            &request.command,
            &request.invocation.sandbox,
            &settlement,
            codex_executable,
            request.trampoline.as_ref(),
            output_cap,
            true,
        ) {
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
            stdin,
            stdout,
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
    sandbox: &HostSandboxState,
    settlement: &SpawnNeverStarted,
    codex_executable: &Path,
    trampoline: Option<&ControlledTrampoline>,
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
    let mut process = build_command(command, sandbox, codex_executable, trampoline)?;
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

/// Constructs a direct or sandbox-wrapped command entirely from validated typed inputs.
///
/// Managed Codex wrappers receive their own directory plus only explicitly configured search roots
/// as `PATH`; an absolute provider program never contributes its directory implicitly.
///
/// `trampoline` is `Some` only for a managed request whose authoritative worktree differs from the
/// host's own `sandboxCwd` (see [`ValidatedExecutionRequest::validate`]). The sandbox argv and its
/// replayed state are identical in both cases; the trampoline only inserts
/// `/usr/bin/env -C <worktree>` before the validated program, because `codex sandbox` otherwise
/// overrides the child cwd with `sandboxCwd`. A same-cwd managed command therefore produces
/// byte-identical argv to before this contract existed. A disabled profile never receives a
/// trampoline and ignores one.
///
/// Returns [`RequestError::ExecutableUnavailable`] through [`ProcessError::Request`] when the
/// accepted trampoline's bytes changed since acceptance.
fn build_command(
    command: &ControlledCommand,
    sandbox: &HostSandboxState,
    codex_executable: &Path,
    trampoline: Option<&ControlledTrampoline>,
) -> Result<Command, ProcessError> {
    let mut process = match sandbox.class {
        ProfileClass::Managed => {
            let mut sandbox_command = Command::new(codex_executable);
            sandbox_command
                .arg("sandbox")
                .arg("--sandbox-state-json")
                .arg(sandbox.json_argument())
                .arg("--");
            if let Some(trampoline) = trampoline {
                let path = trampoline.verified_path().map_err(ProcessError::Request)?;
                ControlledTrampoline::accepts_program(&command.program)
                    .map_err(ProcessError::Request)?;
                sandbox_command.arg(path).arg("-C").arg(&command.cwd);
            }
            sandbox_command.arg(&command.program);
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
        let mut search = vec![parent.to_path_buf()];
        if let Some(configured) = command.env.get(&OsString::from("PATH")) {
            search.extend(std::env::split_paths(configured));
        }
        process.env(
            "PATH",
            std::env::join_paths(search).map_err(|_| {
                ProcessError::Io(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "managed executable search path is invalid",
                ))
            })?,
        );
    }
    Ok(process)
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

/// Projects a sandbox state into a worktree-portable profile template.
///
/// The sandbox cwd must be a supported local absolute path. Its template value becomes the stable
/// `<workspace-cwd>` marker, as does the prefix of every ordinary filesystem path entry equal to
/// or below that cwd; each descendant suffix is retained. Sibling and outside paths, special-path
/// entries, access modes, network policy, unknown fields, and the caller's original value remain
/// unchanged. `state` has already passed [`HostSandboxState`] validation, so its local cwd is
/// absolute and free of current- or parent-directory components.
fn profile_template_value(state: &HostSandboxState) -> Value {
    let mut template = state.raw.clone();
    let Some(object) = template.as_object_mut() else {
        return template;
    };
    object.insert("sandboxCwd".into(), Value::String("<workspace-cwd>".into()));
    if let Some(entries) = object
        .get_mut("permissionProfile")
        .and_then(|profile| profile.get_mut("file_system"))
        .and_then(|file_system| file_system.get_mut("entries"))
        .and_then(Value::as_array_mut)
    {
        for entry in entries {
            let Some(path) = entry.get_mut("path").and_then(Value::as_object_mut) else {
                continue;
            };
            if path.get("type").and_then(Value::as_str) != Some("path") {
                continue;
            }
            let Some(raw_path) = path.get("path").and_then(Value::as_str).map(str::to_owned) else {
                continue;
            };
            let entry_path = Path::new(&raw_path);
            let Ok(suffix) = entry_path.strip_prefix(&state.cwd) else {
                continue;
            };
            if !is_normal_absolute(entry_path) {
                continue;
            }
            path.insert(
                "path".into(),
                Value::String(
                    Path::new("<workspace-cwd>")
                        .join(suffix)
                        .to_string_lossy()
                        .into_owned(),
                ),
            );
        }
    }
    template
}

/// Hashes the worktree-portable semantic host state without relying on whitespace or key order.
fn semantic_state_identity(state: &HostSandboxState) -> String {
    blake3::hash(canonical_json(&profile_template_value(state)).as_bytes())
        .to_hex()
        .to_string()
}

/// Hashes the complete captured state for a v2 record's `semantic_state` identity (T35B).
///
/// `BLAKE3("agent-ide/captured-state/v2\0" || canonical_json(raw))`: domain-separated from every
/// other digest in this module, key-order and whitespace independent, and deliberately *not*
/// worktree-portable — it pins the capture itself, cwd included, so a v2 record is only ever
/// restored against its own reviewed evidence while shape portability lives in the shape digest.
fn captured_state_identity_v2(state: &HostSandboxState) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"agent-ide/captured-state/v2\0");
    hasher.update(canonical_json(&state.raw).as_bytes());
    hasher.finalize().to_hex().to_string()
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

/// Recognizes the one closed managed filesystem profile that already grants read access to `/`.
///
/// `raw` is a complete host sandbox state. The profile qualifies only when every part of its
/// `permissionProfile` value is understood here:
///
/// * the profile holds exactly the three known managed keys `type`, `file_system`, and `network`;
///   an unknown profile-level key may carry a permission this build cannot interpret, so it
///   disqualifies recognition even though the opaque state still parses and replays normally;
/// * `file_system` holds exactly `type` and `entries`, and `type` is `restricted`;
/// * every entry holds only `access`, `path`, and the optional `missing_path_behavior`, whose only
///   accepted value is `skip`;
/// * every `access` is `read` or `write` — an `access` of `none`, or any other value, disqualifies
///   the whole profile because it can subtract from an otherwise total read grant;
/// * every `path` is either `{"type":"path","path":<normal absolute path>}` or
///   `{"type":"special","value":{"kind":<root|slash_tmp|tmpdir>}}`; a relative or cwd-derived path
///   and any unknown key, type, or special kind disqualify the profile;
/// * at least one `read` entry grants the `root` special path, which is the total read grant itself.
///
/// Any unrecognized shape returns `false`, which keeps the strict sandbox-cwd equality in force.
/// This classifier answers one question about declared read authority; it is deliberately not a
/// general permissions evaluator, and it never decides write, network, or execution authority.
fn grants_read_of_all_roots(raw: &Value) -> bool {
    let Some(profile) = raw.get("permissionProfile").and_then(Value::as_object) else {
        return false;
    };
    // The envelope itself must be closed, not merely its filesystem section: a profile-level key
    // this build has never seen may carry a permission that subtracts from the declared read
    // grant, so an unknown key disqualifies the whole recognition. Opaque parsing and replay
    // elsewhere still accept and preserve such a state untouched; only this read-scope
    // recognizer is strict, and it reads neither `type` nor `network` as authority.
    if profile.len() != 3
        || profile
            .keys()
            .any(|key| !matches!(key.as_str(), "type" | "file_system" | "network"))
    {
        return false;
    }
    let Some(file_system) = profile.get("file_system").and_then(Value::as_object) else {
        return false;
    };
    if file_system.len() != 2
        || file_system.get("type").and_then(Value::as_str) != Some("restricted")
    {
        return false;
    }
    let Some(entries) = file_system.get("entries").and_then(Value::as_array) else {
        return false;
    };
    let mut root_granted = false;
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            return false;
        };
        if entry
            .keys()
            .any(|key| !matches!(key.as_str(), "access" | "path" | "missing_path_behavior"))
        {
            return false;
        }
        if let Some(behavior) = entry.get("missing_path_behavior")
            && behavior.as_str() != Some("skip")
        {
            return false;
        }
        let Some(access) = entry.get("access").and_then(Value::as_str) else {
            return false;
        };
        if !matches!(access, "read" | "write") {
            return false;
        }
        let Some(path) = entry.get("path").and_then(Value::as_object) else {
            return false;
        };
        if path.len() != 2 {
            return false;
        }
        match path.get("type").and_then(Value::as_str) {
            Some("path") => {
                let Some(value) = path.get("path").and_then(Value::as_str) else {
                    return false;
                };
                if !is_normal_absolute(Path::new(value)) {
                    return false;
                }
            }
            Some("special") => {
                let Some(special) = path.get("value").and_then(Value::as_object) else {
                    return false;
                };
                if special.len() != 1 {
                    return false;
                }
                match special.get("kind").and_then(Value::as_str) {
                    Some("root") if access == "read" => root_granted = true,
                    Some("root") => {}
                    Some("slash_tmp" | "tmpdir") => {}
                    _ => return false,
                }
            }
            _ => return false,
        }
    }
    root_granted
}

/// Detects unsupported multi-root profile fields without resolving or expanding any root path.
fn has_multiple_roots(value: &serde_json::Map<String, Value>) -> bool {
    value.iter().any(|(key, value)| {
        (key == "roots" || key.ends_with("_roots"))
            && value.as_array().is_some_and(|roots| roots.len() > 1)
    })
}

/// Returns the canonical effective-rights identity two actors must match to share one provider
/// backend, its native cache namespace, and its refcount.
///
/// The identity covers the whole observed state, never a hand-picked subset, and drops
/// `sandboxCwd` only once the rights the state actually grants are proven equal without it:
///
/// * a `disabled` profile applies no cwd-derived filesystem restriction, so its rights are already
///   cwd-independent and the cwd is omitted;
/// * a `managed` profile omits the cwd only when every declared root (`roots`/`*_roots`) resolves
///   to a *normal absolute* path — a cwd-relative root is resolved against this state's own
///   absolute sandbox cwd first, so the identity describes absolute rights rather than a relative
///   policy string;
/// * every other state — a managed profile that declares no root at all, an unknown or
///   unparseable envelope, an unsupported cwd, or a root shape this function cannot walk — keeps
///   `sandboxCwd` in the digest.
///
/// That last case is deliberately fail-closed: an identical relative policy under a different cwd
/// then yields a *different* identity and is never shared, and no sandbox policy is ever broadened
/// to make two actors match.
pub fn effective_rights_identity(state: &Value) -> String {
    let mut canonical = state.clone();
    if let Some(object) = canonical.as_object_mut() {
        let profile_type = object
            .get("permissionProfile")
            .and_then(Value::as_object)
            .and_then(|profile| profile.get("type"))
            .and_then(Value::as_str);
        let cwd = object
            .get("sandboxCwd")
            .and_then(Value::as_str)
            .and_then(|cwd| local_sandbox_cwd(cwd).ok());
        let shareable = match (profile_type, cwd) {
            (Some("disabled"), _) => true,
            (Some("managed"), Some(cwd)) => object
                .get("permissionProfile")
                .and_then(|profile| {
                    let mut proven = false;
                    let normalized = normalize_rights_roots(profile, &cwd, 0, &mut proven)?;
                    proven.then_some(normalized)
                })
                .is_some_and(|normalized| {
                    object.insert("permissionProfile".into(), normalized);
                    true
                }),
            _ => false,
        };
        if shareable {
            object.remove("sandboxCwd");
        }
    }
    blake3::hash(canonical.to_string().as_bytes())
        .to_hex()
        .to_string()
}

/// Bounds the permission-profile nesting this module is willing to claim it understands.
const MAX_RIGHTS_DEPTH: usize = 8;

/// Rewrites every declared sandbox root to its effective absolute path, or refuses the whole value.
///
/// Returns `None` as soon as any part of the profile cannot be proven: a root key whose value is
/// not an array of strings, a root that does not resolve to a normal absolute path, or nesting
/// deeper than `MAX_RIGHTS_DEPTH`. `proven` is set once at least one root was actually normalized,
/// so a managed profile that declares no root at all is never treated as cwd-independent.
fn normalize_rights_roots(
    value: &Value,
    cwd: &Path,
    depth: usize,
    proven: &mut bool,
) -> Option<Value> {
    if depth > MAX_RIGHTS_DEPTH {
        return None;
    }
    match value {
        Value::Object(object) => {
            let mut normalized = serde_json::Map::with_capacity(object.len());
            for (key, child) in object {
                if key == "roots" || key.ends_with("_roots") {
                    let roots = child.as_array()?;
                    let mut absolute = Vec::with_capacity(roots.len());
                    for root in roots {
                        absolute.push(Value::String(absolute_rights_root(root.as_str()?, cwd)?));
                    }
                    if !absolute.is_empty() {
                        *proven = true;
                    }
                    normalized.insert(key.clone(), Value::Array(absolute));
                } else if key == "type" || key == "network" || key == "file_system" {
                    normalized.insert(
                        key.clone(),
                        normalize_rights_roots(child, cwd, depth + 1, proven)?,
                    );
                } else {
                    // An unrecognized key may hide cwd-dependent data (e.g. `entries[].path`) this
                    // module does not understand how to normalize; refuse rather than pass it through
                    // unproven, which would let an unrelated `*_roots` key wrongly mark the state
                    // cwd-independent while this field silently keeps its relative meaning.
                    return None;
                }
            }
            Some(Value::Object(normalized))
        }
        Value::Array(items) => items
            .iter()
            .map(|item| normalize_rights_roots(item, cwd, depth + 1, proven))
            .collect::<Option<Vec<_>>>()
            .map(Value::Array),
        other => Some(other.clone()),
    }
}

/// Resolves one declared root against this state's absolute sandbox cwd without widening it.
///
/// An already-absolute root is kept, a relative root is joined onto `cwd`, and the result is
/// accepted only when it is a normal absolute path, so `..` traversal or an unsupported `file://`
/// spelling refuses the identity instead of inventing a broader right.
fn absolute_rights_root(raw: &str, cwd: &Path) -> Option<String> {
    let path = if let Some(path) = raw.strip_prefix("file://") {
        if path.contains('%') || !path.starts_with('/') {
            return None;
        }
        PathBuf::from(path)
    } else {
        PathBuf::from(raw)
    };
    let resolved = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    is_normal_absolute(&resolved)
        .then(|| resolved.to_str().map(str::to_owned))
        .flatten()
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
