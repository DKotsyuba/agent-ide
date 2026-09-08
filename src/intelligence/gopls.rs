//! Shared `gopls` profile primitives built exclusively on Execution-owned children.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::execution::{
    AdmissionLease, CommandKind, ControlledCommand, OwnedChild, OwnedProtocolChild, ProcessError,
    ValidatedExecutionRequest, WorkspaceAuthority,
};

/// Describes the immutable compatibility inputs for one shared `gopls` daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoplsProfile {
    binary: PathBuf,
    version: String,
    revision: String,
    configuration: String,
    go_toolchain: String,
    trust_identity: String,
    cache_namespace: String,
}

impl GoplsProfile {
    /// Creates a profile only when every identity component is present and the binary is absolute.
    ///
    /// The values become part of `compatibility_key`; callers must create a different profile when
    /// any component changes rather than attaching incompatible views to a running listener.
    pub fn new(
        binary: PathBuf,
        version: String,
        revision: String,
        configuration: String,
        go_toolchain: String,
        trust_identity: String,
        cache_namespace: String,
    ) -> io::Result<Self> {
        if !binary.is_absolute()
            || [
                &version,
                &revision,
                &configuration,
                &go_toolchain,
                &trust_identity,
                &cache_namespace,
            ]
            .iter()
            .any(|value| value.is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gopls profile requires absolute binary and nonempty identity components",
            ));
        }
        Ok(Self {
            binary,
            version,
            revision,
            configuration,
            go_toolchain,
            trust_identity,
            cache_namespace,
        })
    }

    /// Returns the full sharing compatibility key, excluding only the isolated worktree view key.
    pub fn compatibility_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|unix|{}",
            self.binary.display(),
            self.version,
            self.revision,
            self.configuration,
            self.go_toolchain,
            self.trust_identity,
            self.cache_namespace,
        )
    }

    /// Declares the fixed listener command for Execution validation and controlled spawning.
    ///
    /// `socket` must be absolute and owned by the caller. The command deliberately contains one
    /// explicit Unix listener and never selects `-remote=auto`.
    pub fn listener_command(
        &self,
        authority: &WorkspaceAuthority,
        socket: &Path,
    ) -> io::Result<ControlledCommand> {
        self.command(
            authority,
            vec![
                OsString::from("serve"),
                OsString::from(format!("-listen=unix;{}", socket.display())),
            ],
        )
    }

    /// Declares a fixed stdio forwarder command for one isolated logical LSP view.
    pub fn forwarder_command(
        &self,
        authority: &WorkspaceAuthority,
        socket: &Path,
    ) -> io::Result<ControlledCommand> {
        self.command(
            authority,
            vec![OsString::from(format!("-remote=unix;{}", socket.display()))],
        )
    }

    /// Builds a profile-owned provider command with a cleared, finite Go environment.
    fn command(
        &self,
        authority: &WorkspaceAuthority,
        args: Vec<OsString>,
    ) -> io::Result<ControlledCommand> {
        let mut environment = BTreeMap::new();
        environment.insert(OsString::from("GOTOOLCHAIN"), OsString::from("local"));
        environment.insert(
            OsString::from("AGENT_IDE_GOPLS_PROFILE"),
            OsString::from(&self.revision),
        );
        ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.binary.clone(),
            args,
            authority.root().to_path_buf(),
            environment,
        )
        .map_err(|error| io::Error::other(error.to_string()))
    }
}

/// Identifies one Workspace worktree incarnation without making it a daemon compatibility input.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorktreeRef {
    /// Opaque Workspace worktree identity.
    pub id: String,
    /// Workspace incarnation that distinguishes a recreated worktree.
    pub incarnation: String,
}

impl WorktreeRef {
    /// Creates a nonempty isolated view key for a current Workspace worktree incarnation.
    pub fn new(id: String, incarnation: String) -> io::Result<Self> {
        if id.is_empty() || incarnation.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "worktree id and incarnation are required",
            ));
        }
        Ok(Self { id, incarnation })
    }
}

/// Records the per-view state that must never be shared with a peer worktree.
#[derive(Debug)]
struct ViewState {
    /// Next client request identifier for this view alone.
    next_request_id: u64,
    /// Latest source sequence that may satisfy this view's freshness checks.
    source_sequence: u64,
    /// Opaque lease identity for release accounting.
    lease: u64,
}

/// Owns one compatible heavy listener and tracks its independent logical views.
pub struct SharedGopls {
    listener: OwnedChild,
    compatibility_key: String,
    views: BTreeMap<WorktreeRef, ViewState>,
    next_lease: u64,
    forwarders_started: usize,
}

impl SharedGopls {
    /// Starts exactly one Execution-owned Unix listener for this compatible profile.
    pub fn start(
        profile: &GoplsProfile,
        listener_request: &ValidatedExecutionRequest,
        listener_lease: AdmissionLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        Ok(Self {
            listener: OwnedChild::spawn_captured(
                listener_request,
                listener_lease,
                None,
                codex_executable,
                output_cap,
            )?,
            compatibility_key: profile.compatibility_key(),
            views: BTreeMap::new(),
            next_lease: 1,
            forwarders_started: 0,
        })
    }

    /// Returns the immutable key that all views on this heavy listener were checked against.
    pub fn compatibility_key(&self) -> &str {
        &self.compatibility_key
    }

    /// Opens one separately piped forwarder and records independent request, source and lease state.
    ///
    /// A duplicate worktree incarnation or an exhausted lease counter is rejected before spawning.
    pub fn open_view(
        &mut self,
        worktree: WorktreeRef,
        source_sequence: u64,
        forwarder_request: &ValidatedExecutionRequest,
        forwarder_lease: AdmissionLease,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<GoplsView, io::Error> {
        if self.views.contains_key(&worktree) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "worktree view is active",
            ));
        }
        let lease = self.next_lease;
        self.next_lease = self
            .next_lease
            .checked_add(1)
            .ok_or_else(|| io::Error::other("view lease identifiers exhausted"))?;
        let child = OwnedProtocolChild::spawn(
            forwarder_request,
            forwarder_lease,
            None,
            codex_executable,
            output_cap,
        )
        .map_err(|_| io::Error::other("Execution rejected gopls forwarder"))?;
        self.views.insert(
            worktree.clone(),
            ViewState {
                next_request_id: 1,
                source_sequence,
                lease,
            },
        );
        self.forwarders_started += 1;
        Ok(GoplsView {
            worktree,
            lease,
            child,
        })
    }

    /// Allocates a request identifier only for `worktree` when its source sequence remains current.
    pub fn begin_request(
        &mut self,
        worktree: &WorktreeRef,
        source_sequence: u64,
    ) -> io::Result<u64> {
        let state = self
            .views
            .get_mut(worktree)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "worktree view is inactive"))?;
        if state.source_sequence != source_sequence {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "view source sequence is stale",
            ));
        }
        let request = state.next_request_id;
        state.next_request_id = state
            .next_request_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("view request identifiers exhausted"))?;
        Ok(request)
    }

    /// Releases one view after its protocol child has been shut down and reaped by Execution.
    pub fn release_view(&mut self, worktree: &WorktreeRef, lease: u64) -> io::Result<()> {
        let state = self
            .views
            .get(worktree)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "worktree view is inactive"))?;
        if state.lease != lease {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "view lease does not match",
            ));
        }
        self.views.remove(worktree);
        Ok(())
    }

    /// Returns `(heavy_listener_count, active_view_count, forwarders_started)` for acceptance evidence.
    pub fn process_counts(&self) -> (usize, usize, usize) {
        (1, self.views.len(), self.forwarders_started)
    }

    /// Cancels and reaps the owned heavy listener through Execution after all views are released.
    ///
    /// The returned admission lease is released by the same centralized controller that granted it.
    pub async fn stop(
        self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<AdmissionLease, ProcessError> {
        if !self.views.is_empty() {
            return Err(ProcessError::Io(io::Error::other(
                "cannot stop listener with active views",
            )));
        }
        self.listener
            .cancel_and_reap(grace, output_deadline)
            .await
            .map(|reaped| reaped.lease)
    }
}

/// Owns one view's exclusive forwarder pipes until the caller completes LSP shutdown and reaping.
pub struct GoplsView {
    worktree: WorktreeRef,
    lease: u64,
    /// The distinct Execution-owned protocol process and its sole stdin/stdout owners.
    pub child: OwnedProtocolChild,
}

impl GoplsView {
    /// Returns the worktree incarnation whose document buffers and request IDs this view owns.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the opaque logical lease required to release this exact worktree incarnation.
    pub const fn lease(&self) -> u64 {
        self.lease
    }

    /// Transfers the exclusive protocol child to the LSP client that owns its stdin and stdout.
    ///
    /// The caller must shut down the LSP session and reap the returned child through Execution
    /// before releasing the matching logical lease from `SharedGopls`.
    pub fn into_child(self) -> OwnedProtocolChild {
        self.child
    }
}

/// Returns whether every active worktree key is distinct, for acceptance assertions without exposing buffers.
pub fn isolated_views(worktrees: impl IntoIterator<Item = WorktreeRef>) -> bool {
    let mut seen = BTreeSet::new();
    worktrees.into_iter().all(|worktree| seen.insert(worktree))
}
