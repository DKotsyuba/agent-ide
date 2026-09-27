//! Shared `gopls` profile primitives built exclusively on Execution-owned children.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_ide_core::assistance::host_binding::ActiveBindingUse;
use agent_ide_core::execution::{
    CommandKind, CompletedProcess, ControlledCommand, OwnedChild, OwnedProtocolChild, ProcessError,
    ProviderForwarderSpawnLease, ProviderSpawnLease, ProviderViewLease, ValidatedExecutionRequest,
    WorkspaceAuthority,
};
pub use agent_ide_core::workspace::authority::WorktreeRef;

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
        let binary_digest = agent_ide_core::execution::measured_executable_digest(&binary)
            .map_err(|_| {
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
    /// explicit Unix listener with a ten-minute idle orphan ceiling and never selects `-remote=auto`.
    pub fn listener_command(
        &self,
        authority: &WorkspaceAuthority,
        socket: &Path,
    ) -> io::Result<ControlledCommand> {
        self.command(
            authority,
            vec![
                OsString::from(format!("-listen=unix;{}", socket.display())),
                OsString::from("-listen.timeout=10m"),
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

    /// Observes whether the owned listener has already exited without blocking or signaling.
    ///
    /// `Ok(None)` means the listener still appears alive and must keep its existing cancellation and
    /// deadline bounds; `Ok(Some(status))` is definite, stable readiness evidence that no future
    /// signal can revive. It never substitutes for `stop`: the caller must still route the owned
    /// listener through `stop` to drain output and release its Execution admission slot exactly once.
    /// The underlying nonblocking wait may reap the OS child while retaining that logical ownership.
    pub fn listener_exit_status(&mut self) -> io::Result<Option<std::process::ExitStatus>> {
        self.listener.try_exit_status()
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

/// Per-worktree Go build/module/temp namespace delivered only through this session's view
/// configuration.
///
/// The shared listener process never receives these as process environment (see
/// `GoplsProfile::command`): its `GOPLSCACHE`/`TMPDIR` belong to the one *shared* native namespace
/// every compatible worktree uses, while `GOCACHE`/`GOMODCACHE`/`GOTMPDIR` are worktree-owned. A
/// session that cannot supply them fails closed instead of silently inheriting another worktree's
/// build cache, so the fields are private and only `GoEnv::new` can produce a value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoEnv {
    /// Absolute per-worktree `GOCACHE` directory.
    go_cache: std::path::PathBuf,
    /// Absolute per-worktree `GOMODCACHE` directory.
    go_mod_cache: std::path::PathBuf,
    /// Absolute per-worktree `GOTMPDIR` directory.
    go_tmp_dir: std::path::PathBuf,
}

impl GoEnv {
    /// Accepts only three absolute, normal, non-empty per-worktree cache directories.
    ///
    /// Returns `None` for an empty, relative, or `..`-containing path: such a value would be
    /// resolved by the provider process against its own cwd and could therefore escape the private
    /// namespace this session is accounted for. Callers pass paths derived from the retained
    /// `CacheLifecycle`, which are absolute by construction, so a rejection is a real defect.
    ///
    /// This constructor only validates: it never touches the filesystem, so the three directories
    /// may still be missing. A caller whose session reaches a real provider must use
    /// [`GoEnv::prepare`] instead, which additionally creates them.
    pub fn new(
        go_cache: std::path::PathBuf,
        go_mod_cache: std::path::PathBuf,
        go_tmp_dir: std::path::PathBuf,
    ) -> Option<Self> {
        [&go_cache, &go_mod_cache, &go_tmp_dir]
            .iter()
            .all(|path| {
                path.is_absolute()
                    && path.components().all(|component| {
                        matches!(
                            component,
                            std::path::Component::RootDir | std::path::Component::Normal(_)
                        )
                    })
            })
            .then_some(Self {
                go_cache,
                go_mod_cache,
                go_tmp_dir,
            })
    }

    /// Validates the three paths exactly like [`GoEnv::new`] and creates them on disk.
    ///
    /// Every session that is about to reach a real `gopls` view must use this constructor rather
    /// than [`GoEnv::new`]. `go` creates a missing `GOCACHE`/`GOMODCACHE` itself, but it refuses a
    /// missing `GOTMPDIR` with `creating work dir: stat <path>: no such file or directory`, which
    /// gopls reports back only as `no package metadata for file ... (jsonrpc error 0)`; the view
    /// then silently degrades to lexical context instead of failing. The namespace root retained by
    /// `CacheLifecycle` exists, but the `go-build`/`go-mod`/`tmp` directories under it are this
    /// session's own, so nothing else creates them.
    ///
    /// The directories are created recursively with owner-only `0o700` permissions, matching the
    /// private cache root they live under; an already existing directory is accepted unchanged and
    /// no file inside one is ever read or removed here. Returns `None` for a path [`GoEnv::new`]
    /// refuses and for any directory that cannot be created, because a view whose private namespace
    /// is unusable must fail closed rather than inherit another worktree's cache.
    pub fn prepare(
        go_cache: std::path::PathBuf,
        go_mod_cache: std::path::PathBuf,
        go_tmp_dir: std::path::PathBuf,
    ) -> Option<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let env = Self::new(go_cache, go_mod_cache, go_tmp_dir)?;
        [&env.go_cache, &env.go_mod_cache, &env.go_tmp_dir]
            .iter()
            .all(|path| {
                path.is_dir()
                    || std::fs::DirBuilder::new()
                        .recursive(true)
                        .mode(0o700)
                        .create(path)
                        .is_ok()
            })
            .then_some(env)
    }

    /// Returns this view's private `GOCACHE` directory.
    pub fn go_cache(&self) -> &std::path::Path {
        &self.go_cache
    }

    /// Returns this view's private `GOMODCACHE` directory.
    pub fn go_mod_cache(&self) -> &std::path::Path {
        &self.go_mod_cache
    }

    /// Returns this view's private `GOTMPDIR` directory.
    pub fn go_tmp_dir(&self) -> &std::path::Path {
        &self.go_tmp_dir
    }
}

impl agent_ide_core::intelligence::session::SessionProfile for GoEnv {
    /// Delivers this view's private Go build/module/temp namespace through the `env` setting.
    fn workspace_configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "env": {
                "GOCACHE": self.go_cache().display().to_string(),
                "GOMODCACHE": self.go_mod_cache().display().to_string(),
                "GOTMPDIR": self.go_tmp_dir().display().to_string(),
            }
        })
    }

    /// Accepts an omitted identity or one named `gopls`; never a Rust or other provider identity.
    fn accepts_server(&self, info: Option<&async_lsp::lsp_types::ServerInfo>) -> bool {
        info.is_none_or(|info| info.name == "gopls")
    }

    /// Opens `.go` files as `go`; everything else stays `plaintext`.
    fn language_id(&self, path: &Path) -> &'static str {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("go") => "go",
            _ => "plaintext",
        }
    }
}

/// Returns whether every active worktree key is distinct, for acceptance assertions without exposing buffers.
pub fn isolated_views(worktrees: impl IntoIterator<Item = WorktreeRef>) -> bool {
    let mut seen = BTreeSet::new();
    worktrees.into_iter().all(|worktree| seen.insert(worktree))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_ide_core::intelligence::session::SessionProfile;

    /// The session profile accepts only an omitted or `gopls` identity and opens only `.go` files
    /// as `go`.
    #[test]
    fn go_session_profile_is_closed() {
        let env = GoEnv::new(
            PathBuf::from("/private/tmp/agent-ide-go-env/go-build"),
            PathBuf::from("/private/tmp/agent-ide-go-env/go-mod"),
            PathBuf::from("/private/tmp/agent-ide-go-env/tmp"),
        )
        .unwrap();
        assert!(env.accepts_server(None));
        let named = |name: &str| async_lsp::lsp_types::ServerInfo {
            name: name.into(),
            version: None,
        };
        assert!(env.accepts_server(Some(&named("gopls"))));
        assert!(!env.accepts_server(Some(&named("rust-analyzer"))));
        assert_eq!(env.language_id(Path::new("main.go")), "go");
        assert_eq!(env.language_id(Path::new("module.py")), "plaintext");
        assert_eq!(
            env.workspace_configuration(),
            serde_json::json!({"env": {
                "GOCACHE": "/private/tmp/agent-ide-go-env/go-build",
                "GOMODCACHE": "/private/tmp/agent-ide-go-env/go-mod",
                "GOTMPDIR": "/private/tmp/agent-ide-go-env/tmp",
            }})
        );
    }

    /// A per-worktree Go namespace must be an absolute normal path or the session refuses to exist.
    ///
    /// The provider process resolves a relative value against its own cwd, so accepting one would let
    /// worktree-owned build state escape the private namespace this session is accounted for.
    #[test]
    fn go_env_rejects_paths_that_could_escape_the_private_namespace() {
        let good =
            |name: &str| std::path::PathBuf::from("/private/tmp/agent-ide-go-env").join(name);
        assert!(GoEnv::new(good("go-build"), good("go-mod"), good("tmp")).is_some());
        for bad in [
            std::path::PathBuf::new(),
            std::path::PathBuf::from("relative/go-build"),
            std::path::PathBuf::from("/private/tmp/../escape"),
        ] {
            assert!(
                GoEnv::new(bad.clone(), good("go-mod"), good("tmp")).is_none(),
                "{bad:?} must be refused"
            );
        }
    }

    /// `prepare` must materialize every private Go directory a real view is configured to use.
    ///
    /// A missing `GOTMPDIR` makes `go` refuse to create its work directory, which gopls surfaces only
    /// as `no package metadata for file ... (jsonrpc error 0)`, silently demoting the view to lexical
    /// context; the namespace root exists but these three subdirectories belong to the session alone.
    /// The same refusals as `new` still apply before anything is created.
    #[test]
    fn go_env_prepare_creates_the_private_namespace_directories() {
        let root = std::path::PathBuf::from("/private/tmp").join(format!(
            "agent-ide-go-env-prepare-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths = ["go-build", "go-mod", "tmp"].map(|name| root.join("nested").join(name));
        let env = GoEnv::prepare(paths[0].clone(), paths[1].clone(), paths[2].clone())
            .expect("an absolute private namespace is creatable");
        for path in &paths {
            assert!(path.is_dir(), "{path:?} must exist before a view uses it");
        }
        assert_eq!(env.go_tmp_dir(), paths[2]);
        assert!(
            GoEnv::prepare(
                std::path::PathBuf::from("relative/go-build"),
                paths[1].clone(),
                paths[2].clone(),
            )
            .is_none(),
            "prepare must keep every rejection new performs"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
