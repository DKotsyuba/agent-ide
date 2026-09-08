//! Exclusive rust-analyzer v0.1 profile and per-worktree semantic-view bookkeeping.
//!
//! This module does not spawn arbitrary commands.  A caller first obtains a validated Execution
//! request and an admission lease, then this module may hand those exact capabilities to
//! `OwnedProtocolChild` for one exclusive Rust view.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    execution::{
        AdmissionClass, AdmissionController, AdmissionError, AdmissionPromotion, BackendRelease,
        CommandKind, ControlledCommand, OwnedProtocolChild, OwnerId, ProcessError,
        ProviderLeaseAdmission, ProviderLeaseError, ProviderLeaseRegistry, ProviderViewLease,
        QueueTicket, ValidatedExecutionRequest, WorkspaceAuthority,
    },
    workspace::authority::WorktreeRef,
};

/// The fixed v0.1 rust-analyzer profile revision.
pub const RUST_PROFILE_REVISION: u32 = 1;

/// Complete observed inputs for the immutable rust-analyzer v0.1 profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustProfileIdentity {
    /// Absolute rust-analyzer executable selected by accepted local policy.
    pub binary: PathBuf,
    /// Observed rust-analyzer binary version.
    pub rust_analyzer_version: String,
    /// Observed Cargo version paired with this profile.
    pub cargo_version: String,
    /// Observed rustc version paired with this profile.
    pub rustc_version: String,
    /// Effective provider configuration identity.
    pub configuration: String,
    /// Effective local trust identity.
    pub trust: String,
    /// Owned transport identity.
    pub transport: String,
    /// Native cache namespace identity.
    pub cache_namespace: String,
}

/// Immutable identity inputs required before Rust semantic facts may be reused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustProfile {
    binary: PathBuf,
    rust_analyzer_version: String,
    cargo_version: String,
    rustc_version: String,
    configuration: String,
    trust: String,
    transport: String,
    cache_namespace: String,
}

impl RustProfile {
    /// Creates the sole supported v0.1 Rust profile from complete nonempty observed identities.
    pub fn new(identity: RustProfileIdentity) -> Result<Self, RustProfileError> {
        let profile = Self {
            binary: identity.binary,
            rust_analyzer_version: identity.rust_analyzer_version,
            cargo_version: identity.cargo_version,
            rustc_version: identity.rustc_version,
            configuration: identity.configuration,
            trust: identity.trust,
            transport: identity.transport,
            cache_namespace: identity.cache_namespace,
        };
        profile
            .valid()
            .then_some(profile)
            .ok_or(RustProfileError::InvalidProfile)
    }

    /// Returns a controlled rust-analyzer stdio command constrained to this exact worktree root.
    pub fn command(&self, worktree: &RustWorktree) -> Result<ControlledCommand, RustProfileError> {
        ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.binary.clone(),
            Vec::new(),
            worktree.worktree.worktree_path().to_path_buf(),
            BTreeMap::new(),
        )
        .map_err(|_| RustProfileError::InvalidProfile)
    }

    /// Produces the exclusive backend identity, including the canonical worktree incarnation.
    pub fn compatibility_key(&self, worktree: &RustWorktree) -> RustCompatibilityKey {
        let mut identity = String::new();
        for value in [
            self.binary.to_string_lossy().as_ref(),
            &self.rust_analyzer_version,
            &RUST_PROFILE_REVISION.to_string(),
            &self.cargo_version,
            &self.rustc_version,
            &self.configuration,
            &self.trust,
            &self.transport,
            &self.cache_namespace,
            worktree.worktree.id(),
            &worktree.worktree.incarnation().to_string(),
        ] {
            identity.push_str(value);
            identity.push('\0');
        }
        RustCompatibilityKey(blake3::hash(identity.as_bytes()).to_hex().to_string())
    }

    /// Returns the configured rust-analyzer path for local Execution policy allowlisting.
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// Returns whether every fixed compatibility input is present and the executable path is absolute.
    fn valid(&self) -> bool {
        self.binary.is_absolute()
            && [
                &self.rust_analyzer_version,
                &self.cargo_version,
                &self.rustc_version,
                &self.configuration,
                &self.trust,
                &self.transport,
                &self.cache_namespace,
            ]
            .iter()
            .all(|value| !value.is_empty())
    }
}

/// Opaque hash of the immutable exclusive-profile compatibility inputs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RustCompatibilityKey(String);

impl RustCompatibilityKey {
    /// Returns the stable opaque backend identity passed to Execution's lease registry.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Couples Workspace's canonical worktree incarnation with the Execution authority for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustWorktree {
    worktree: WorktreeRef,
    authority: WorkspaceAuthority,
}

impl RustWorktree {
    /// Rejects a root, identity, incarnation, or authority-epoch mismatch before admission.
    pub fn new(
        worktree: WorktreeRef,
        authority: WorkspaceAuthority,
    ) -> Result<Self, RustProfileError> {
        (worktree.id() == authority.worktree_id()
            && worktree.incarnation().to_string() == authority.incarnation()
            && worktree.worktree_path() == authority.root())
        .then_some(Self {
            worktree,
            authority,
        })
        .ok_or(RustProfileError::WorktreeMismatch)
    }

    /// Returns the exact authority that must be supplied to Execution admission.
    pub fn authority(&self) -> &WorkspaceAuthority {
        &self.authority
    }

    /// Returns the canonical worktree reference, including its incarnation.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }
}

/// Explains why the fixed Rust profile cannot serve a semantic view.
#[derive(Debug)]
pub enum RustProfileError {
    /// A profile input is empty or the analyzer path is not absolute.
    InvalidProfile,
    /// Execution authority did not describe the supplied canonical worktree incarnation.
    WorktreeMismatch,
    /// A view was unknown, released, or belongs to another Rust lifecycle.
    UnknownView,
    /// A source observation regressed or did not name the active view sequence.
    InvalidSourceSequence,
    /// A validated request was for a different current worktree authority.
    RequestAuthorityMismatch,
    /// Execution rejected provider lease admission or release.
    Execution(ProviderLeaseError),
    /// Execution refused the central heavy-process request.
    Refused(AdmissionError),
    /// Execution could not create or reap the direct owned child.
    Process(ProcessError),
}

/// Reports the only possible exclusive view-admission outcomes.
#[derive(Debug)]
pub enum RustViewAdmission {
    /// One exclusive logical view was admitted; no other view may attach to its backend.
    Granted(RustView),
    /// Execution queued a distinct heavy-process request and reserved no Rust view or cache.
    Queued(QueueTicket),
    /// Execution refused the distinct heavy-process request.
    Refused(AdmissionError),
    /// A local profile or Execution registry rule rejected the request.
    Unavailable(RustProfileError),
}

/// Opaque logical Rust view plus its backend generation and source sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RustView {
    lease: ProviderViewLease,
    key: RustCompatibilityKey,
    generation: u64,
    source_sequence: u64,
    availability: RustAvailability,
}

impl RustView {
    /// Returns the opaque Execution lease needed for release and source updates.
    pub fn lease(&self) -> ProviderViewLease {
        self.lease
    }

    /// Returns the exclusive backend generation for response correlation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the latest source sequence accepted for this one view.
    pub const fn source_sequence(&self) -> u64 {
        self.source_sequence
    }

    /// Returns the honest semantic availability state.
    pub const fn availability(&self) -> RustAvailability {
        self.availability
    }
}

/// States whether a reply can be considered current for its source observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustAvailability {
    /// The provider generation is available for its current source sequence.
    Ready,
    /// The requested source sequence changed after the request was sent.
    Stale,
    /// The exclusive child stopped or was revoked before a usable reply.
    Unavailable,
}

/// Returns the direct-child reap decision and exact queued promotions after one view release.
#[derive(Debug)]
pub struct RustRelease {
    /// Execution's ownership consequence; only `ReapOwned` may be reaped by the caller.
    pub backend: BackendRelease,
    /// Exact central promotions that can be consumed once through `promote`.
    pub promotions: Vec<AdmissionPromotion>,
}

/// Holds per-view sequence state; one instance owns no shared Rust document buffers or caches.
#[derive(Debug, Default)]
pub struct RustViews {
    next_generation: u64,
    views: BTreeMap<ProviderViewLease, RustView>,
}

impl RustViews {
    /// Requests a new exclusive Rust view through Execution's provider lease registry.
    pub fn request(
        &mut self,
        profile: &RustProfile,
        worktree: &RustWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        owner: OwnerId,
        class: AdmissionClass,
    ) -> RustViewAdmission {
        let key = profile.compatibility_key(worktree);
        match registry.request(
            admission,
            owner,
            class,
            key.as_str(),
            crate::execution::ProviderBackendKind::OwnedExclusive,
            worktree.authority(),
        ) {
            ProviderLeaseAdmission::Granted(lease) => {
                let view = self.track(lease, key);
                RustViewAdmission::Granted(view)
            }
            ProviderLeaseAdmission::Queued(ticket) => RustViewAdmission::Queued(ticket),
            ProviderLeaseAdmission::Refused(error) => RustViewAdmission::Refused(error),
            ProviderLeaseAdmission::Rejected(error) => {
                RustViewAdmission::Unavailable(RustProfileError::Execution(error))
            }
        }
    }

    /// Consumes one exact central promotion into a newly admitted exclusive Rust view.
    pub fn promote(
        &mut self,
        profile: &RustProfile,
        worktree: &RustWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        promotion: AdmissionPromotion,
    ) -> Result<RustView, RustProfileError> {
        let lease = registry
            .promote(admission, promotion, worktree.authority())
            .map_err(RustProfileError::Execution)?;
        Ok(self.track(lease, profile.compatibility_key(worktree)))
    }

    /// Advances one view's source observation sequence and marks older reply evidence stale.
    pub fn observe_source(
        &mut self,
        lease: ProviderViewLease,
        source_sequence: u64,
    ) -> Result<(), RustProfileError> {
        let view = self
            .views
            .get_mut(&lease)
            .ok_or(RustProfileError::UnknownView)?;
        if source_sequence <= view.source_sequence {
            return Err(RustProfileError::InvalidSourceSequence);
        }
        view.source_sequence = source_sequence;
        view.availability = RustAvailability::Ready;
        Ok(())
    }

    /// Classifies a result as current only when its exact source sequence and generation still match.
    pub fn result_state(
        &self,
        lease: ProviderViewLease,
        generation: u64,
        source_sequence: u64,
    ) -> Result<RustAvailability, RustProfileError> {
        let view = self
            .views
            .get(&lease)
            .ok_or(RustProfileError::UnknownView)?;
        Ok(
            if view.generation == generation && view.source_sequence == source_sequence {
                view.availability
            } else {
                RustAvailability::Stale
            },
        )
    }

    /// Releases one logical exclusive view and returns only Execution's own reap decision.
    pub fn release(
        &mut self,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        lease: ProviderViewLease,
    ) -> Result<RustRelease, RustProfileError> {
        self.views
            .get(&lease)
            .ok_or(RustProfileError::UnknownView)?;
        let release = registry
            .release(admission, lease)
            .map_err(RustProfileError::Execution)?;
        self.views.remove(&lease);
        Ok(RustRelease {
            backend: release.0,
            promotions: release.2,
        })
    }

    /// Marks one view unavailable after its owned protocol pipes hit EOF or a protocol failure.
    pub fn mark_unavailable(&mut self, lease: ProviderViewLease) -> Result<(), RustProfileError> {
        self.views
            .get_mut(&lease)
            .ok_or(RustProfileError::UnknownView)?
            .availability = RustAvailability::Unavailable;
        Ok(())
    }

    /// Stores one newly admitted view with a fresh monotonically increasing provider generation.
    fn track(&mut self, lease: ProviderViewLease, key: RustCompatibilityKey) -> RustView {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("generation exhausted");
        let view = RustView {
            lease,
            key,
            generation: self.next_generation,
            source_sequence: 0,
            availability: RustAvailability::Ready,
        };
        self.views.insert(lease, view.clone());
        view
    }
}

/// Wraps only an Execution-owned protocol child; this type cannot signal borrowed endpoints.
pub struct RustProtocolChild {
    child: OwnedProtocolChild,
}

impl RustProtocolChild {
    /// Spawns one Rust protocol child through the validated Execution request and its exact lease.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        worktree: &RustWorktree,
        registry: &mut ProviderLeaseRegistry,
        view: ProviderViewLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, RustProfileError> {
        if request.authority() != worktree.authority() {
            return Err(RustProfileError::RequestAuthorityMismatch);
        }
        let capability = registry
            .take_spawn_lease(view)
            .map_err(RustProfileError::Execution)?;
        OwnedProtocolChild::spawn_from_provider_lease(
            request,
            capability,
            None,
            codex_executable,
            output_cap,
        )
        .map(|child| Self { child })
        .map_err(RustProfileError::Process)
    }

    /// Returns the sole stdin writer owned by this protocol lifecycle.
    pub fn stdin_mut(&mut self) -> &mut tokio::process::ChildStdin {
        &mut self.child.stdin
    }

    /// Returns the sole stdout reader owned by this protocol lifecycle.
    pub fn stdout_mut(&mut self) -> &mut tokio::process::ChildStdout {
        &mut self.child.stdout
    }

    /// Reaps only this direct owned protocol child after its pipes are dropped.
    pub async fn reap(self, output_deadline: Duration) -> Result<(), RustProfileError> {
        self.child
            .reap(output_deadline)
            .await
            .map(|_| ())
            .map_err(RustProfileError::Process)
    }
}
