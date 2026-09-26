//! Exclusive Pyright profile and owned stdio-child adapter.
//!
//! The adapter validates the fixed Python provider identity, asks Execution for one exclusive
//! worktree-bound view, and exposes only an Execution-owned protocol child. It never discovers a
//! virtual environment, chooses an interpreter, or shares a Pyright process between worktrees.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    assistance::host_binding::ActiveBindingUse,
    execution::{
        AdmissionClass, AdmissionController, BackendReapCapability, BackendRelease, CommandKind,
        ControlledCommand, OwnedProtocolChild, OwnerId, ProcessError, ProviderBackendKind,
        ProviderLeaseAdmission, ProviderLeaseError, ProviderLeaseRegistry, ProviderViewLease,
        QueueTicket, ReapedProtocolProcess, ValidatedExecutionRequest, WorkspaceAuthority,
    },
    workspace::authority::WorktreeRef,
};

/// Complete accepted inputs for the fixed Pyright defaults profile.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PyrightProfileIdentity {
    /// Absolute accepted `pyright-langserver` executable measured at construction.
    pub binary: PathBuf,
    /// Launcher-accepted BLAKE3 digest of the Pyright script bytes; construction rejects drift.
    pub accepted_script_digest: blake3::Hash,
    /// Nonempty accepted Pyright version identity.
    pub version: String,
    /// Absolute accepted Node executable; it is never inferred from source or environment.
    pub node: PathBuf,
    /// Launcher-accepted BLAKE3 digest of the Node executable bytes; construction rejects drift.
    pub accepted_node_digest: blake3::Hash,
    /// Nonempty accepted Node identity matched to the configured provider toolchain.
    pub node_identity: String,
    /// Explicit operator trust identity retained in the exclusive compatibility key.
    pub trust: String,
    /// Absolute private cache namespace for this worktree/provider binding.
    pub cache_namespace: String,
}

/// Immutable Pyright identity used to construct one fixed `--stdio` command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PyrightProfile {
    /// Absolute accepted `pyright-langserver` executable.
    binary: PathBuf,
    /// Launcher-accepted Pyright script bytes retained in the compatibility key.
    accepted_script_digest: blake3::Hash,
    /// Accepted Pyright version identity.
    version: String,
    /// Absolute accepted Node executable that runs the Pyright script.
    node: PathBuf,
    /// Launcher-accepted Node executable bytes retained in the compatibility key.
    accepted_node_digest: blake3::Hash,
    /// Accepted Node identity retained in the compatibility key.
    node_identity: String,
    /// Explicit operator trust identity.
    trust: String,
    /// Absolute private cache namespace.
    cache_namespace: String,
}

impl PyrightProfile {
    /// Validates current Pyright script and Node bytes against launcher-accepted identities without consulting project Python settings.
    pub fn new(identity: PyrightProfileIdentity) -> Result<Self, PyrightProfileError> {
        let script_digest = crate::execution::measured_executable_digest(&identity.binary)
            .map_err(|_| PyrightProfileError::InvalidProfile)?;
        if script_digest != identity.accepted_script_digest {
            return Err(PyrightProfileError::InvalidProfile);
        }
        if !identity.node.is_absolute() {
            return Err(PyrightProfileError::InvalidProfile);
        }
        let node_digest = crate::execution::measured_executable_digest(&identity.node)
            .map_err(|_| PyrightProfileError::InvalidProfile)?;
        if node_digest != identity.accepted_node_digest {
            return Err(PyrightProfileError::InvalidProfile);
        }
        let profile = Self {
            binary: identity.binary,
            accepted_script_digest: identity.accepted_script_digest,
            version: identity.version,
            node: identity.node,
            accepted_node_digest: identity.accepted_node_digest,
            node_identity: identity.node_identity,
            trust: identity.trust,
            cache_namespace: identity.cache_namespace,
        };
        profile
            .valid()
            .then_some(profile)
            .ok_or(PyrightProfileError::InvalidProfile)
    }

    /// Builds fixed `node <absolute-pyright-script> --stdio` with only Node's parent on `PATH`.
    ///
    /// The command is admitted only when its captured Node identity equals the launcher's accepted
    /// digest, so replacement after profile construction fails before admission.
    pub fn command(
        &self,
        worktree: &PyrightWorktree,
    ) -> Result<ControlledCommand, PyrightProfileError> {
        let node_parent = self
            .node
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or(PyrightProfileError::InvalidProfile)?;
        let path =
            std::env::join_paths([node_parent]).map_err(|_| PyrightProfileError::InvalidProfile)?;
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.node.clone(),
            vec![
                self.binary.clone().into_os_string(),
                OsString::from("--stdio"),
            ],
            worktree.worktree().worktree_path().to_path_buf(),
            BTreeMap::from([
                (OsString::from("PATH"), path),
                (
                    OsString::from("TMPDIR"),
                    OsString::from(Path::new(&self.cache_namespace).join("tmp")),
                ),
            ]),
        )
        .map_err(|_| PyrightProfileError::InvalidProfile)?;
        command
            .has_program_digest(&self.accepted_node_digest)
            .then_some(command)
            .ok_or(PyrightProfileError::InvalidProfile)
    }

    /// Remeasures the Pyright script and rejects replacement before a provider spawn lease is taken.
    ///
    /// The final Execution Node recheck immediately before spawn leaves only its already acknowledged
    /// narrow same-user race after those final pre-spawn checks.
    pub fn verify_script(&self) -> Result<(), PyrightProfileError> {
        (crate::execution::measured_executable_digest(&self.binary)
            .map_err(|_| PyrightProfileError::InvalidProfile)?
            == self.accepted_script_digest)
            .then_some(())
            .ok_or(PyrightProfileError::InvalidProfile)
    }

    /// Requests one exclusive Execution view; queued and rejected outcomes retain no child.
    pub fn request_view(
        &self,
        worktree: &PyrightWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        owner: OwnerId,
        class: AdmissionClass,
        generation: u64,
    ) -> PyrightViewAdmission {
        match registry.request(
            admission,
            owner,
            class,
            self.compatibility_key(worktree),
            ProviderBackendKind::OwnedExclusive,
            worktree.authority(),
        ) {
            ProviderLeaseAdmission::Granted(lease) => {
                PyrightViewAdmission::Granted(PyrightView { lease, generation })
            }
            ProviderLeaseAdmission::Queued(ticket) => PyrightViewAdmission::Queued(ticket),
            ProviderLeaseAdmission::Refused(error) => PyrightViewAdmission::Refused(error),
            ProviderLeaseAdmission::Rejected(error) => {
                PyrightViewAdmission::Unavailable(PyrightProfileError::Execution(error))
            }
        }
    }

    /// Returns a worktree-incarnation-scoped key so Pyright processes are never shared.
    fn compatibility_key(&self, worktree: &PyrightWorktree) -> String {
        blake3::hash(
            format!(
                "{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
                self.binary.display(),
                self.accepted_script_digest,
                self.version,
                self.node.display(),
                self.accepted_node_digest,
                self.node_identity,
                self.trust,
                self.cache_namespace,
                worktree.worktree().incarnation(),
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string()
    }

    /// Checks the closed profile invariants before any admission or command construction.
    fn valid(&self) -> bool {
        self.binary.is_absolute()
            && self.node.is_absolute()
            && Path::new(&self.cache_namespace).is_absolute()
            && [
                &self.version,
                &self.node_identity,
                &self.trust,
                &self.cache_namespace,
            ]
            .iter()
            .all(|value| !value.is_empty() && value.len() <= 4096)
    }
}

/// Couples a canonical worktree incarnation with its exact Execution authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PyrightWorktree {
    /// Canonical workspace identity and root for command construction.
    worktree: WorktreeRef,
    /// Exact execution authority required for admission and spawning.
    authority: WorkspaceAuthority,
}

impl PyrightWorktree {
    /// Rejects a worktree identity, root, or incarnation mismatch before provider admission.
    pub fn new(
        worktree: WorktreeRef,
        authority: WorkspaceAuthority,
    ) -> Result<Self, PyrightProfileError> {
        (worktree.id() == authority.worktree_id()
            && worktree.incarnation().to_string() == authority.incarnation()
            && worktree.worktree_path() == authority.root())
        .then_some(Self {
            worktree,
            authority,
        })
        .ok_or(PyrightProfileError::WorktreeMismatch)
    }

    /// Returns the authority required for this exact provider admission and child spawn.
    pub fn authority(&self) -> &WorkspaceAuthority {
        &self.authority
    }

    /// Returns the immutable worktree reference used as the controlled command directory.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }
}

/// Explains why the fixed Pyright adapter could not provide an exclusive semantic view.
#[derive(Debug)]
pub enum PyrightProfileError {
    /// The closed identity is incomplete or its accepted executable is unavailable.
    InvalidProfile,
    /// The supplied authority does not identify the canonical worktree.
    WorktreeMismatch,
    /// Execution rejected an exclusive provider lease operation.
    Execution(ProviderLeaseError),
    /// Execution refused an admission request under current capacity policy.
    Refused(crate::execution::AdmissionError),
    /// Execution could not start or reap the owned protocol child.
    Process(ProcessError),
}

/// Reports the bounded outcomes of requesting a Pyright exclusive view.
#[derive(Debug)]
pub enum PyrightViewAdmission {
    /// A fresh exclusive view owns one launch capability.
    Granted(PyrightView),
    /// Admission queued a request without launching a process.
    Queued(QueueTicket),
    /// Admission refused the request under current capacity policy.
    Refused(crate::execution::AdmissionError),
    /// Profile or registry validation refused the request before launch.
    Unavailable(PyrightProfileError),
}

/// Opaque exclusive Pyright view carrying its lease and response-correlation generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PyrightView {
    /// One Execution view lease whose launch capability can be consumed exactly once.
    lease: ProviderViewLease,
    /// Monotonic provider generation set by the worker for response correlation.
    generation: u64,
}

impl PyrightView {
    /// Returns the one lease that may be consumed to spawn then release this view's child.
    pub fn lease(&self) -> ProviderViewLease {
        self.lease
    }

    /// Returns the monotonic provider generation assigned by the owner worker.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Detaches the exclusive view and returns the capability required to complete child reap.
    pub fn release(
        self,
        registry: &mut ProviderLeaseRegistry,
    ) -> Result<BackendReapCapability, PyrightProfileError> {
        let BackendRelease::ReapOwned(capability) = registry
            .release(self.lease)
            .map_err(PyrightProfileError::Execution)?
        else {
            return Err(PyrightProfileError::Execution(
                ProviderLeaseError::InvalidBackend,
            ));
        };
        Ok(capability)
    }
}

/// Wraps one Execution-owned Pyright stdio child and exposes no shared process handle.
pub struct PyrightProtocolChild {
    /// Direct child ownership, pipes, cancellation, output drain, and reap proof from Execution.
    child: OwnedProtocolChild,
}

impl PyrightProtocolChild {
    /// Starts one authority-bound Pyright child from the view capability after rechecking the
    /// accepted script. A script mismatch uses `admission` to cancel the still-unconsumed view and
    /// release its backend slot; successful verification consumes the capability exactly once.
    #[allow(clippy::too_many_arguments)] // The owned-spawn boundary carries the independently validated capabilities.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        profile: &PyrightProfile,
        worktree: &PyrightWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, PyrightProfileError> {
        if profile.verify_script().is_err() {
            registry
                .cancel_unstarted(admission, view)
                .map_err(PyrightProfileError::Execution)?;
            return Err(PyrightProfileError::InvalidProfile);
        }
        let capability = registry
            .take_spawn_lease(view)
            .map_err(PyrightProfileError::Execution)?;
        if request.authority() != worktree.authority() {
            return Err(PyrightProfileError::Process(ProcessError::NeverStarted {
                cause: Box::new(ProcessError::Request(
                    crate::execution::RequestError::WorktreeDenied,
                )),
                settlement: capability.cancel(),
            }));
        }
        OwnedProtocolChild::spawn_from_provider_lease(request, capability, active_use, output_cap)
            .map(|child| Self { child })
            .map_err(PyrightProfileError::Process)
    }

    /// Borrows the sole stdout reader and stdin writer for the bounded session exchange.
    pub fn pipes(
        &mut self,
    ) -> (
        &mut tokio::process::ChildStdout,
        &mut tokio::process::ChildStdin,
    ) {
        (
            self.child.stdout.as_mut().expect("protocol stdout taken"),
            self.child.stdin.as_mut().expect("protocol stdin taken"),
        )
    }

    /// Transfers the protocol pipes to one live session while retaining child ownership for reap.
    pub fn take_pipes(
        &mut self,
    ) -> Option<(tokio::process::ChildStdin, tokio::process::ChildStdout)> {
        Some((self.child.stdin.take()?, self.child.stdout.take()?))
    }

    /// Cancels and reaps this child, returning Execution's direct-child settlement proof.
    pub async fn cancel_and_reap(
        self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, PyrightProfileError> {
        self.child
            .cancel_and_reap(grace, deadline)
            .await
            .map_err(PyrightProfileError::Process)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        AdmissionLimits, LocalExecutionPolicy, ProviderLeaseLimits, ValidatedHostInvocation,
    };
    use std::collections::BTreeSet;

    /// Builds a canonical worktree/authority pair for fixed-profile command construction tests.
    fn worktree() -> PyrightWorktree {
        let root = std::env::temp_dir();
        let worktree =
            WorktreeRef::from_discovery(root.clone(), root.clone(), root.join(".git"), 1).unwrap();
        let authority = WorkspaceAuthority::from_workspace(
            worktree.id(),
            worktree.incarnation().to_string(),
            root,
            1,
        )
        .unwrap();
        PyrightWorktree::new(worktree, authority).unwrap()
    }

    /// Creates one unique temporary directory for a test-owned executable fixture.
    fn temporary_directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "agent-ide-pyright-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&directory).unwrap();
        directory
    }

    /// Writes executable fixture bytes under `directory` and returns its absolute path.
    fn executable(directory: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join(name);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    /// Builds one profile whose accepted digests match the supplied script and Node fixtures.
    fn profile(script: &Path, node: &Path) -> Result<PyrightProfile, PyrightProfileError> {
        PyrightProfile::new(PyrightProfileIdentity {
            binary: script.to_path_buf(),
            accepted_script_digest: crate::execution::measured_executable_digest(script).unwrap(),
            version: "pyright-test".into(),
            node: node.to_path_buf(),
            accepted_node_digest: crate::execution::measured_executable_digest(node).unwrap(),
            node_identity: "node-test".into(),
            trust: "test".into(),
            cache_namespace: "/private/tmp/agent-ide-pyright-profile-test-cache".into(),
        })
    }

    /// Builds a valid provider request whose command is inert because script verification fails first.
    fn request(authority: &WorkspaceAuthority) -> ValidatedExecutionRequest {
        let root = std::env::temp_dir();
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            "/usr/bin/true".into(),
            Vec::new(),
            root,
            BTreeMap::new(),
        )
        .unwrap();
        ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_verified_binding("pyright-test").unwrap(),
            authority.clone(),
            command,
            &LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/usr/bin/true")]), 4096, 16)
                .unwrap(),
        )
        .unwrap()
    }

    /// Admits a profile whose current script and Node fixture bytes equal their accepted digests.
    #[test]
    fn accepted_profile_builds_command() {
        let directory = temporary_directory();
        let script = executable(&directory, "pyright", b"#!/bin/sh\nexit 0\n");
        let node = executable(&directory, "node", b"#!/bin/sh\nexit 0\n");

        assert!(
            profile(&script, &node)
                .unwrap()
                .command(&worktree())
                .is_ok()
        );

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Rejects command construction when Node is replaced after the profile captured its accepted digest.
    #[test]
    fn replaced_node_after_profile_creation_rejects_command() {
        let directory = temporary_directory();
        let script = executable(&directory, "pyright", b"#!/bin/sh\nexit 0\n");
        let node = executable(&directory, "node", b"#!/bin/sh\nexit 0\n");
        let profile = profile(&script, &node).unwrap();
        std::fs::write(&node, b"#!/bin/sh\nexit 1\n").unwrap();

        assert!(matches!(
            profile.command(&worktree()),
            Err(PyrightProfileError::InvalidProfile)
        ));

        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Rejects a replaced script at the spawn boundary without consuming its view lease.
    #[test]
    fn replaced_script_after_profile_creation_rejects_spawn_before_lease_consumption() {
        let directory = temporary_directory();
        let script = executable(&directory, "pyright", b"#!/bin/sh\nexit 0\n");
        let node = executable(&directory, "node", b"#!/bin/sh\nexit 0\n");
        let profile = profile(&script, &node).unwrap();
        let worktree = worktree();
        let request = request(worktree.authority());
        let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
            total_views: 1,
            per_backend_views: 1,
        })
        .unwrap();
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 1,
            per_owner_running: 1,
            per_owner_queued: 1,
            total_queued: 1,
            interactive_burst: 1,
        })
        .unwrap();
        let PyrightViewAdmission::Granted(view) = profile.request_view(
            &worktree,
            &mut registry,
            &mut admission,
            OwnerId::new("pyright-test").unwrap(),
            AdmissionClass::Interactive,
            1,
        ) else {
            panic!("fixture view")
        };
        std::fs::write(&script, b"#!/bin/sh\nexit 1\n").unwrap();

        assert!(matches!(
            PyrightProtocolChild::spawn(
                &request,
                &profile,
                &worktree,
                &mut registry,
                &mut admission,
                view.lease(),
                None,
                64,
            ),
            Err(PyrightProfileError::InvalidProfile)
        ));
        let PyrightViewAdmission::Granted(retry) = profile.request_view(
            &worktree,
            &mut registry,
            &mut admission,
            OwnerId::new("pyright-retry").unwrap(),
            AdmissionClass::Interactive,
            2,
        ) else {
            panic!("failed pre-spawn verification must release provider capacity")
        };
        registry
            .cancel_unstarted(&mut admission, retry.lease())
            .unwrap();

        std::fs::remove_dir_all(directory).unwrap();
    }
}
