//! Shared `gopls` profile primitives built exclusively on Execution-owned children.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::assistance::host_binding::ActiveBindingUse;
use crate::execution::{
    CommandKind, CompletedProcess, ControlledCommand, OwnedChild, OwnedProtocolChild, ProcessError,
    ProviderForwarderSpawnLease, ProviderSpawnLease, ProviderViewLease, ValidatedExecutionRequest,
    WorkspaceAuthority,
};
pub use crate::workspace::authority::WorktreeRef;

/// Describes the immutable compatibility inputs for one shared `gopls` daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoplsProfile {
    binary: PathBuf,
    /// Measured executable bytes included in every backend compatibility identity.
    binary_digest: blake3::Hash,
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
            || !Path::new(&go_toolchain).is_absolute()
            || !Path::new(&cache_namespace).is_absolute()
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
                "gopls profile requires an absolute binary/toolchain/cache namespace and nonempty identity components",
            ));
        }
        let binary_digest =
            crate::execution::measured_executable_digest(&binary).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "gopls executable unavailable")
            })?;
        Ok(Self {
            binary,
            binary_digest,
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
            "{}|{}|{}|{}|{}|{}|{}|unix|{}",
            self.binary.display(),
            self.binary_digest.to_hex(),
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
    /// explicit Unix listener with a one-minute idle orphan ceiling and never selects `-remote=auto`.
    pub fn listener_command(
        &self,
        authority: &WorkspaceAuthority,
        socket: &Path,
    ) -> io::Result<ControlledCommand> {
        self.command(
            authority,
            vec![
                OsString::from(format!("-listen=unix;{}", socket.display())),
                OsString::from("-listen.timeout=1m"),
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

    /// Declares the fixed daemon session-inspection command used by real sharing acceptance.
    pub fn sessions_command(
        &self,
        authority: &WorkspaceAuthority,
        socket: &Path,
    ) -> io::Result<ControlledCommand> {
        self.command(
            authority,
            vec![
                OsString::from(format!("-remote=unix;{}", socket.display())),
                OsString::from("remote"),
                OsString::from("sessions"),
            ],
        )
    }

    /// Builds a profile-owned provider command with a cleared, finite Go environment.
    ///
    /// `cache_namespace` here is the *shared* native namespace: gopls 0.23.0 binds its on-disk
    /// filecache to `GOPLSCACHE` once per process, so every worktree view on this one listener must
    /// observe the same value, and the listener's own `TMPDIR` is a backend-scoped subdirectory of
    /// that same shared namespace rather than the host temporary directory. Per-worktree
    /// `GOCACHE`/`GOMODCACHE`/`GOTMPDIR` are deliberately never set here: they are delivered per
    /// view through the LSP session's `initializationOptions`/`workspace/configuration` `env`, so a
    /// missing per-view value fails that view closed instead of silently reusing this shared
    /// namespace for worktree-owned build state.
    fn command(
        &self,
        authority: &WorkspaceAuthority,
        args: Vec<OsString>,
    ) -> io::Result<ControlledCommand> {
        let mut environment = BTreeMap::new();
        environment.insert(OsString::from("GOTOOLCHAIN"), OsString::from("local"));
        let go_parent = Path::new(&self.go_toolchain)
            .parent()
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "go toolchain has no parent")
            })?;
        environment.insert(OsString::from("PATH"), go_parent.as_os_str().to_os_string());
        environment.insert(
            OsString::from("GOPLSCACHE"),
            OsString::from(Path::new(&self.cache_namespace).join("gopls")),
        );
        environment.insert(
            OsString::from("TMPDIR"),
            OsString::from(Path::new(&self.cache_namespace).join("tmp")),
        );
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
        .map_err(|_| io::Error::other("Execution rejected gopls command"))
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
    lease: ProviderViewLease,
}

/// Owns one compatible heavy listener and tracks its independent logical views.
pub struct SharedGopls {
    listener: OwnedChild,
    compatibility_key: String,
    views: BTreeMap<WorktreeRef, ViewState>,
    forwarders_started: usize,
}

impl SharedGopls {
    /// Consumes the registry's one-time backend grant to start this profile's sole owned listener.
    /// A different compatibility key or request authority is rejected before any process effect.
    /// Host-bound requests require a newly consumed active use at this physical spawn; no use is cached.
    pub fn start(
        profile: &GoplsProfile,
        listener_request: &ValidatedExecutionRequest,
        listener_lease: ProviderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, ProcessError> {
        if listener_lease.backend() != profile.compatibility_key() {
            return Err(ProcessError::NeverStarted {
                cause: Box::new(ProcessError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "listener profile does not match its admitted backend",
                ))),
                settlement: listener_lease.cancel(),
            });
        }
        Ok(Self {
            listener: OwnedChild::spawn_from_provider_lease(
                listener_request,
                listener_lease,
                active_use,
                codex_executable,
                output_cap,
            )?,
            compatibility_key: profile.compatibility_key(),
            views: BTreeMap::new(),
            forwarders_started: 0,
        })
    }

    /// Returns the immutable key that all views on this heavy listener were checked against.
    pub fn compatibility_key(&self) -> &str {
        &self.compatibility_key
    }

    /// Opens one separately piped forwarder and records independent request, source and lease state.
    ///
    /// Requires the exact canonical worktree identity, incarnation and root in the validated
    /// request, and the registry capability's authority epoch and compatible backend. Mismatch
    /// or duplicate worktree is rejected before spawning; the caller retains release accounting
    /// for the consumed capability's reserved slot on failure. Host-bound requests require a fresh
    /// active use consumed immediately before this delayed forwarder spawn.
    #[allow(clippy::too_many_arguments)]
    pub fn open_view(
        &mut self,
        worktree: WorktreeRef,
        source_sequence: u64,
        forwarder_request: &ValidatedExecutionRequest,
        forwarder_lease: ProviderForwarderSpawnLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<GoplsView, ProcessError> {
        let authority = forwarder_request.authority();
        if worktree.id() != authority.worktree_id()
            || worktree.incarnation().to_string() != authority.incarnation()
            || worktree.worktree_path() != authority.root()
            || forwarder_lease.backend() != self.compatibility_key
        {
            return Err(ProcessError::NeverStarted {
                cause: Box::new(ProcessError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "forwarder worktree or backend does not match its authority",
                ))),
                settlement: forwarder_lease.cancel(),
            });
        }
        if self.views.contains_key(&worktree) {
            return Err(ProcessError::NeverStarted {
                cause: Box::new(ProcessError::Io(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "worktree view is active",
                ))),
                settlement: forwarder_lease.cancel(),
            });
        }
        let lease = forwarder_lease.view();
        let child = OwnedProtocolChild::spawn_from_forwarder_lease(
            forwarder_request,
            forwarder_lease,
            active_use,
            codex_executable,
            output_cap,
        )?;
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

    /// Advances this exact view generation to a newer source observation without resetting IDs.
    /// Equal sequences are idempotent; regressing sequences, stale leases and inactive views fail.
    /// Replies tagged with the previous sequence subsequently fail `result_is_current`.
    pub fn observe_source(
        &mut self,
        worktree: &WorktreeRef,
        lease: ProviderViewLease,
        source_sequence: u64,
    ) -> io::Result<()> {
        let state = self
            .views
            .get_mut(worktree)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "worktree view is inactive"))?;
        if state.lease != lease || source_sequence < state.source_sequence {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "view generation or source sequence is stale",
            ));
        }
        state.source_sequence = source_sequence;
        Ok(())
    }

    /// Accepts a reply only while its exact worktree, logical generation and source sequence are live.
    /// Released or superseded views return false, including a reopened view at the same worktree.
    pub fn result_is_current(
        &self,
        worktree: &WorktreeRef,
        lease: ProviderViewLease,
        source_sequence: u64,
    ) -> bool {
        self.views
            .get(worktree)
            .is_some_and(|state| state.lease == lease && state.source_sequence == source_sequence)
    }

    /// Releases one view after its protocol child has been shut down and reaped by Execution.
    pub fn release_view(
        &mut self,
        worktree: &WorktreeRef,
        lease: ProviderViewLease,
    ) -> io::Result<()> {
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
    /// After reap, final registry-view release returns this listener admission to the controller.
    /// Pass the returned proof and the registry draining capability to complete_reap exactly once.
    pub async fn stop(
        self,
        grace: Duration,
        output_deadline: Duration,
    ) -> Result<CompletedProcess, ProcessError> {
        if !self.views.is_empty() {
            return Err(ProcessError::Io(io::Error::other(
                "cannot stop listener with active views",
            )));
        }
        self.listener.cancel_and_reap(grace, output_deadline).await
    }
}

/// Owns one view's exclusive forwarder pipes until the caller completes LSP shutdown and reaping.
pub struct GoplsView {
    worktree: WorktreeRef,
    lease: ProviderViewLease,
    /// The distinct Execution-owned protocol process and its sole stdin/stdout owners.
    pub child: OwnedProtocolChild,
}

impl GoplsView {
    /// Returns the worktree incarnation whose document buffers and request IDs this view owns.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the opaque logical lease required to release this exact worktree incarnation.
    pub const fn lease(&self) -> ProviderViewLease {
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
