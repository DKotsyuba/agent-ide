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
    /// Nonempty accepted Pyright version identity.
    pub version: String,
    /// Absolute accepted Node executable; it is never inferred from source or environment.
    pub node: PathBuf,
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
    /// Measured executable bytes retained in the compatibility key.
    binary_digest: blake3::Hash,
    /// Accepted Pyright version identity.
    version: String,
    /// Absolute accepted Node executable that runs the Pyright script.
    node: PathBuf,
    /// Measured Node executable bytes retained in the compatibility key.
    node_digest: blake3::Hash,
    /// Accepted Node identity retained in the compatibility key.
    node_identity: String,
    /// Explicit operator trust identity.
    trust: String,
    /// Absolute private cache namespace.
    cache_namespace: String,
}

impl PyrightProfile {
    /// Validates and measures accepted Pyright and Node executables without consulting project Python settings.
    pub fn new(identity: PyrightProfileIdentity) -> Result<Self, PyrightProfileError> {
        let binary_digest = crate::execution::measured_executable_digest(&identity.binary)
            .map_err(|_| PyrightProfileError::InvalidProfile)?;
        if !identity.node.is_absolute() {
            return Err(PyrightProfileError::InvalidProfile);
        }
        let node_digest = crate::execution::measured_executable_digest(&identity.node)
            .map_err(|_| PyrightProfileError::InvalidProfile)?;
        let profile = Self {
            binary: identity.binary,
            binary_digest,
            version: identity.version,
            node: identity.node,
            node_digest,
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
        ControlledCommand::from_validated_peer(
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
        .map_err(|_| PyrightProfileError::InvalidProfile)
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
                self.binary_digest,
                self.version,
                self.node.display(),
                self.node_digest,
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
    /// Consumes the view launch capability to start one authority-bound Pyright child.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        worktree: &PyrightWorktree,
        registry: &mut ProviderLeaseRegistry,
        view: ProviderViewLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, PyrightProfileError> {
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
        OwnedProtocolChild::spawn_from_provider_lease(
            request,
            capability,
            active_use,
            codex_executable,
            output_cap,
        )
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
        (&mut self.child.stdout, &mut self.child.stdin)
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

    /// Builds a canonical worktree/authority pair for profile-only command construction tests.
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

    /// Builds one measured Pyright profile for a supplied operator-declared Node executable and identity.
    fn profile(node: &str) -> Result<PyrightProfile, PyrightProfileError> {
        PyrightProfile::new(PyrightProfileIdentity {
            binary: "/usr/bin/true".into(),
            version: "pyright-test".into(),
            node: node.into(),
            node_identity: "node-test".into(),
            trust: "test".into(),
            cache_namespace: "/private/tmp/agent-ide-pyright-profile-test-cache".into(),
        })
    }

    /// Requires an absolute measured Node executable and runs the script through that exact executable.
    #[test]
    fn profile_requires_node_executable_and_builds_complete_path() {
        assert!(matches!(
            profile("node"),
            Err(PyrightProfileError::InvalidProfile)
        ));

        let worktree = worktree();
        let pyright_profile = profile("/bin/echo").unwrap();
        let expected = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            "/bin/echo".into(),
            vec![OsString::from("/usr/bin/true"), OsString::from("--stdio")],
            worktree.worktree().worktree_path().to_path_buf(),
            BTreeMap::from([
                (
                    OsString::from("PATH"),
                    std::env::join_paths([Path::new("/bin")]).unwrap(),
                ),
                (
                    OsString::from("TMPDIR"),
                    OsString::from(
                        Path::new("/private/tmp/agent-ide-pyright-profile-test-cache").join("tmp"),
                    ),
                ),
            ]),
        )
        .unwrap();
        assert_eq!(pyright_profile.command(&worktree).unwrap(), expected);
        assert_ne!(
            pyright_profile.compatibility_key(&worktree),
            profile("/usr/bin/env")
                .unwrap()
                .compatibility_key(&worktree)
        );
    }
}
