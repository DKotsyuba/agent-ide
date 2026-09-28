//! Exclusive rust-analyzer v0.1 profile and per-worktree semantic-view bookkeeping.
//!
//! This module does not spawn arbitrary commands.  A caller first obtains a validated Execution
//! request and an admission lease, then this module may hand those exact capabilities to
//! `OwnedProtocolChild` for one exclusive Rust view.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};

use agent_ide_core::assistance::host_binding::ActiveBindingUse;
use agent_ide_core::{
    execution::{
        AdmissionClass, AdmissionController, AdmissionError, AdmissionPromotion,
        BackendReapCapability, BackendRelease, CommandKind, ControlledCommand, OwnedProtocolChild,
        OwnerId, ProcessError, ProviderLeaseAdmission, ProviderLeaseError, ProviderLeaseRegistry,
        ProviderViewLease, QueueTicket, ReapedProtocolProcess, ValidatedExecutionRequest,
        WorkspaceAuthority,
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
    /// Absolute operator-declared `cargo` executable; never chosen by model or project input.
    pub cargo: PathBuf,
    /// Absolute operator-declared Cargo home serving the analyzer's registry; `None` uses the
    /// real home's `.cargo`. Never chosen by model or project input.
    pub cargo_home: Option<PathBuf>,
    /// Observed Cargo version paired with this profile.
    pub cargo_version: String,
    /// Absolute operator-declared `rustc` executable; never chosen by model or project input.
    pub rustc: PathBuf,
    /// Observed rustc version paired with this profile.
    pub rustc_version: String,
    /// Explicit rustup toolchain selector used by the analyzer and its Cargo/rustc subprocesses.
    pub rustup_toolchain: String,
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
    /// Measured analyzer bytes included in the exclusive backend compatibility identity.
    binary_digest: blake3::Hash,
    rust_analyzer_version: String,
    cargo: PathBuf,
    /// Operator-declared Cargo home; `None` derives the real home's `.cargo` (see `command`).
    cargo_home: Option<PathBuf>,
    /// Measured `cargo` bytes; binds the declared `cargo_version` to the exact executed binary.
    cargo_digest: blake3::Hash,
    cargo_version: String,
    rustc: PathBuf,
    /// Measured `rustc` bytes; binds the declared `rustc_version` to the exact executed binary.
    rustc_digest: blake3::Hash,
    rustc_version: String,
    /// Toolchain selector included in command environment and compatibility identity.
    rustup_toolchain: String,
    configuration: String,
    trust: String,
    transport: String,
    cache_namespace: String,
}

/// Manifests rust-analyzer must load explicitly, or `None` to leave its own discovery alone.
///
/// rust-analyzer discovers the root `Cargo.toml` only; a crate nested under a root that is
/// a single package (no `[workspace]` table) or no Cargo project at all is opened as a detached
/// file: hover works, references across the crate's own tests do not. For such roots this lists
/// the root manifest (when present) and every nested manifest found up to
/// two directories deep, skipping `target`, `node_modules`, hidden
/// directories and the conventional test-material directories (`tests`, `fixtures`, `examples`,
/// `benches`), whose crates are fixtures rather than projects. A root with a `[workspace]`
/// table keeps auto-discovery for its members, but a nested manifest that declares its own
/// `[workspace]` table is an independent project cargo refuses to fold into the root (a member
/// can never carry that table), so such roots are linked explicitly too; without that, their
/// files open detached and lose cross-file references.
pub fn linked_projects(root: &Path) -> Option<Vec<String>> {
    let root_manifest = root.join("Cargo.toml");
    let root_is_workspace = root_manifest.is_file() && {
        let text = std::fs::read_to_string(&root_manifest).ok()?;
        declares_workspace(&text)
    };
    let mut found = Vec::new();
    collect_manifests(root, LINKED_PROJECT_DEPTH, &mut found);
    found.retain(|path| *path != root_manifest);
    if root_is_workspace {
        found.retain(|path| {
            std::fs::read_to_string(path).is_ok_and(|text| declares_workspace(&text))
        });
    }
    if found.is_empty() {
        return None;
    }
    found.sort();
    let mut projects = Vec::with_capacity(found.len() + 1);
    if root_manifest.is_file() {
        projects.push(root_manifest.display().to_string());
    }
    projects.extend(found.into_iter().map(|path| path.display().to_string()));
    Some(projects)
}

/// Whether a manifest carries a `[workspace]` table, i.e. is a workspace root of its own.
fn declares_workspace(manifest: &str) -> bool {
    manifest.lines().any(|line| line.trim() == "[workspace]")
}

/// Directory depth [`linked_projects`] searches for nested manifests.
const LINKED_PROJECT_DEPTH: usize = 2;

fn collect_manifests(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    if depth == 0 || found.len() >= 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.')
            || matches!(
                name,
                "target" | "node_modules" | "tests" | "fixtures" | "examples" | "benches"
            )
        {
            continue;
        }
        if path.is_dir() {
            let manifest = path.join("Cargo.toml");
            if manifest.is_file() {
                found.push(manifest);
            }
            collect_manifests(&path, depth - 1, found);
        }
    }
}

impl RustProfile {
    /// Creates the sole supported v0.1 Rust profile from complete nonempty observed identities.
    pub fn new(identity: RustProfileIdentity) -> Result<Self, RustProfileError> {
        let binary_digest = agent_ide_core::execution::measured_executable_digest(&identity.binary)
            .map_err(|_| RustProfileError::InvalidProfile)?;
        let cargo_digest = agent_ide_core::execution::measured_executable_digest(&identity.cargo)
            .map_err(|_| RustProfileError::InvalidProfile)?;
        let rustc_digest = agent_ide_core::execution::measured_executable_digest(&identity.rustc)
            .map_err(|_| RustProfileError::InvalidProfile)?;
        let profile = Self {
            binary: identity.binary,
            binary_digest,
            rust_analyzer_version: identity.rust_analyzer_version,
            cargo: identity.cargo,
            cargo_home: identity.cargo_home,
            cargo_digest,
            cargo_version: identity.cargo_version,
            rustc: identity.rustc,
            rustc_digest,
            rustc_version: identity.rustc_version,
            rustup_toolchain: identity.rustup_toolchain,
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

    /// Returns the worktree-bound stdio command with the selected toolchain and the same
    /// environment a human editor gives rust-analyzer: `CARGO`/`RUSTC` name the exact
    /// operator-verified executables, `PATH` lets build scripts find the system linker, and the
    /// operator's own Cargo home serves the registry so nothing is downloaded twice — resolved
    /// exactly as the confined project check resolves it, from the real operator home rather
    /// than a substituted `HOME` or an inherited `CARGO_HOME` (see
    /// the shared `crate::home` helpers), with only the build artifacts and temporary files staying in the
    /// private namespace.
    pub fn command(&self, worktree: &RustWorktree) -> Result<ControlledCommand, RustProfileError> {
        ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.binary.clone(),
            Vec::new(),
            worktree.worktree.worktree_path().to_path_buf(),
            self.environment(),
        )
        .map_err(|_| RustProfileError::InvalidProfile)
    }

    /// Builds the complete server environment: the toolchain `PATH` plus `HOME`/`CARGO_HOME`
    /// from the real operator home (the declared override winning when one is configured) and
    /// the private-namespace target/temp directories. When no real cargo home exists the private
    /// namespace copy is used so the analyzer never writes registry state outside it.
    fn environment(&self) -> BTreeMap<OsString, OsString> {
        let home = crate::home::real_home();
        let derived = crate::home::effective_cargo_home(self.cargo_home.as_deref(), &home);
        let cargo_home = if self.cargo_home.is_some() || derived.is_dir() {
            derived.into_os_string()
        } else {
            Path::new(&self.cache_namespace)
                .join("cargo")
                .into_os_string()
        };
        let mut path = OsString::new();
        for directory in [
            self.cargo.parent(),
            self.binary.parent(),
            Some(Path::new("/usr/bin")),
            Some(Path::new("/bin")),
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_empty() {
                path.push(":");
            }
            path.push(directory);
        }
        BTreeMap::from([
            (OsString::from("PATH"), path),
            (
                OsString::from("RUSTUP_TOOLCHAIN"),
                OsString::from(&self.rustup_toolchain),
            ),
            (OsString::from("CARGO"), OsString::from(&self.cargo)),
            (OsString::from("RUSTC"), OsString::from(&self.rustc)),
            (OsString::from("CARGO_HOME"), cargo_home),
            (
                OsString::from("CARGO_TARGET_DIR"),
                OsString::from(Path::new(&self.cache_namespace).join("target")),
            ),
            (
                OsString::from("TMPDIR"),
                OsString::from(Path::new(&self.cache_namespace).join("tmp")),
            ),
            (OsString::from("HOME"), home.into_os_string()),
        ])
    }

    /// Produces the exclusive backend identity, including the canonical worktree incarnation.
    pub fn compatibility_key(&self, worktree: &RustWorktree) -> RustCompatibilityKey {
        let mut identity = String::new();
        for value in [
            self.binary.to_string_lossy().as_ref(),
            self.binary_digest.to_hex().as_str(),
            &self.rust_analyzer_version,
            &RUST_PROFILE_REVISION.to_string(),
            self.cargo.to_string_lossy().as_ref(),
            self.cargo_digest.to_hex().as_str(),
            &self.cargo_version,
            &self
                .cargo_home
                .as_deref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            self.rustc.to_string_lossy().as_ref(),
            self.rustc_digest.to_hex().as_str(),
            &self.rustc_version,
            &self.rustup_toolchain,
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

    /// Returns the exact initialize server version expected from the observed analyzer CLI identity.
    pub fn initialize_version(&self) -> &str {
        self.rust_analyzer_version
            .strip_prefix("rust-analyzer ")
            .unwrap_or(&self.rust_analyzer_version)
    }

    /// Returns the immutable accepted Rust configuration identity.
    pub fn configuration(&self) -> &str {
        &self.configuration
    }

    /// Returns whether every fixed compatibility input is present, the executable path is
    /// absolute, and the cache namespace is a verified absolute private directory rather than a
    /// bare label resolved relative to the spawned child's working directory.
    fn valid(&self) -> bool {
        self.binary.is_absolute()
            && self.cargo.is_absolute()
            && self
                .cargo_home
                .as_deref()
                .is_none_or(agent_ide_core::assistance::launcher::absolute)
            && self.rustc.is_absolute()
            && Path::new(&self.cache_namespace).is_absolute()
            && matches!(
                self.configuration.as_str(),
                "cache-priming-disabled-v1" | "cache-priming-and-proc-macro-disabled-v1"
            )
            && self.transport == "stdio-v1"
            && [
                &self.rust_analyzer_version,
                &self.cargo_version,
                &self.rustc_version,
                &self.rustup_toolchain,
                &self.configuration,
                &self.trust,
                &self.transport,
                &self.cache_namespace,
            ]
            .iter()
            .all(|value| !value.is_empty() && value.len() <= 4096)
    }

    /// Returns whether this execution profile must suppress rust-analyzer's proc-macro server.
    /// Disabling it prevents Darwin proc-macro helper writes outside the verified namespace, at the
    /// cost of semantic expansion generated by procedural macros for that managed sandbox session.
    pub fn proc_macros_disabled(&self) -> bool {
        self.configuration == "cache-priming-and-proc-macro-disabled-v1"
    }
}

/// Exact rust-analyzer status notification accepted by the versioned profile.
const RUST_STATUS_METHOD: &str = "experimental/serverStatus";

/// Bounded status fields used for the Rust readiness barrier; optional provider messages are ignored.
#[derive(serde::Deserialize)]
struct RustStatus {
    /// Whether the analyzer reports successful workspace health.
    health: RustHealth,
    /// Whether current background workspace activity is quiescent.
    quiescent: bool,
}

/// Closed health values defined by the accepted rust-analyzer status protocol.
#[derive(serde::Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum RustHealth {
    /// Workspace health is reported as successful.
    Ok,
    /// Provider reports a warning (for example failed build scripts); still usable when quiescent.
    Warning,
    /// Provider reports an error: the workspace did not load, semantic operations stay unavailable.
    Error,
}

impl agent_ide_core::intelligence::session::SessionProfile for RustProfile {
    /// Disables cache priming and enables proc-macro expansion unless this profile suppresses it.
    fn workspace_configuration(&self) -> serde_json::Value {
        serde_json::json!({
            "cachePriming":{"enable":false},
            "procMacro":{"enable":!self.proc_macros_disabled()}
        })
    }

    /// The fixed configuration plus the crates rust-analyzer would not find on its own: nested
    /// manifests under a root that is not a Cargo workspace (see [`linked_projects`]).
    fn initialization_options(&self, worktree_root: &Path) -> serde_json::Value {
        let mut options = self.workspace_configuration();
        if let Some(projects) = linked_projects(worktree_root)
            && let Some(object) = options.as_object_mut()
        {
            object.insert("linkedProjects".into(), serde_json::Value::from(projects));
        }
        options
    }

    /// Requires the exact analyzer identity: name `rust-analyzer` and the accepted version.
    fn accepts_server(&self, info: Option<&async_lsp::lsp_types::ServerInfo>) -> bool {
        info.is_some_and(|info| {
            info.name == "rust-analyzer"
                && info.version.as_deref() == Some(self.initialize_version())
        })
    }

    /// Asks rust-analyzer for its server-status notification, the readiness barrier.
    fn experimental_capabilities(&self) -> Option<serde_json::Value> {
        Some(serde_json::json!({"serverStatusNotification":true}))
    }

    /// Readiness arrives as `experimental/serverStatus`.
    fn status_method(&self) -> Option<&'static str> {
        Some(RUST_STATUS_METHOD)
    }

    /// Quiescent with health `ok` or `warning` is ready, quiescent with `error` failed, anything
    /// not quiescent still busy; params that are not the accepted status shape fail decoding.
    fn status(
        &self,
        params: serde_json::Value,
    ) -> Result<agent_ide_core::intelligence::session::ProviderStatus, serde_json::Error> {
        use agent_ide_core::intelligence::session::ProviderStatus;
        let status: RustStatus = serde_json::from_value(params)?;
        Ok(match (status.quiescent, status.health) {
            (true, RustHealth::Ok | RustHealth::Warning) => ProviderStatus::Ready,
            (true, RustHealth::Error) => ProviderStatus::Failed,
            (false, _) => ProviderStatus::Busy,
        })
    }

    /// Opens `.rs` files as `rust`; everything else stays `plaintext`.
    fn language_id(&self, path: &Path) -> &'static str {
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") => "rust",
            _ => "plaintext",
        }
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
            agent_ide_core::execution::ProviderBackendKind::OwnedExclusive,
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

    /// Detaches the exclusive Rust view and returns its one concrete draining capability.
    pub fn release(
        &mut self,
        registry: &mut ProviderLeaseRegistry,
        lease: ProviderViewLease,
    ) -> Result<BackendReapCapability, RustProfileError> {
        self.views
            .get(&lease)
            .ok_or(RustProfileError::UnknownView)?;
        let BackendRelease::ReapOwned(capability) = registry
            .release(lease)
            .map_err(RustProfileError::Execution)?
        else {
            return Err(RustProfileError::Execution(
                ProviderLeaseError::InvalidBackend,
            ));
        };
        self.views.remove(&lease);
        Ok(capability)
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
    /// Moves the protocol pipes out for a long-lived session driver; `None` once taken.
    pub fn take_pipes(
        &mut self,
    ) -> Option<(tokio::process::ChildStdin, tokio::process::ChildStdout)> {
        self.child.take_pipes()
    }

    /// Spawns one Rust protocol child through its exact lease and a newly consumed host-binding use.
    /// Host-bound requests reject missing/mismatched uses; this wrapper never retains liveness.
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        worktree: &RustWorktree,
        registry: &mut ProviderLeaseRegistry,
        view: ProviderViewLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
    ) -> Result<Self, RustProfileError> {
        let capability = registry
            .take_spawn_lease(view)
            .map_err(RustProfileError::Execution)?;
        if request.authority() != worktree.authority() {
            return Err(RustProfileError::Process(ProcessError::NeverStarted {
                cause: Box::new(ProcessError::Request(
                    agent_ide_core::execution::RequestError::WorktreeDenied,
                )),
                settlement: capability.cancel(),
            }));
        }

        OwnedProtocolChild::spawn_from_provider_lease(request, capability, active_use, output_cap)
            .map(|child| Self { child })
            .map_err(RustProfileError::Process)
    }

    /// Returns the sole stdin writer owned by this protocol lifecycle.
    pub fn stdin_mut(&mut self) -> &mut tokio::process::ChildStdin {
        self.child.stdin.as_mut().expect("protocol stdin taken")
    }

    /// Returns the sole stdout reader owned by this protocol lifecycle.
    pub fn stdout_mut(&mut self) -> &mut tokio::process::ChildStdout {
        self.child.stdout.as_mut().expect("protocol stdout taken")
    }

    /// Borrows the sole protocol reader/writer together for the production Session driver.
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

    /// Reaps the direct child and stderr under a bounded deadline without dropping accounting proof.
    pub async fn reap(self, deadline: Duration) -> Result<ReapedProtocolProcess, RustProfileError> {
        self.child
            .reap(deadline)
            .await
            .map_err(RustProfileError::Process)
    }

    /// Retains this owned protocol handle across a bounded cancellation attempt for supervisor handoff.
    pub async fn cancel_bounded(
        &mut self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<std::process::ExitStatus, RustProfileError> {
        self.child
            .cancel_bounded(grace, deadline)
            .await
            .map_err(RustProfileError::Process)
    }

    /// Cancels only this Execution-owned Rust process and returns its one-time direct-child reap proof.
    pub async fn cancel_and_reap(
        self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, RustProfileError> {
        self.child
            .cancel_and_reap(grace, deadline)
            .await
            .map_err(RustProfileError::Process)
    }
}

#[cfg(test)]
mod linked_project_tests {
    use super::{RustProfile, RustProfileIdentity, linked_projects};
    use std::ffi::OsStr;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-linked-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nested_crate_under_a_single_package_root_is_linked_and_fixtures_are_not() {
        let dir = scratch("nested");
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"root\"\n").unwrap();
        std::fs::create_dir_all(dir.join("nested/src")).unwrap();
        std::fs::write(
            dir.join("nested/Cargo.toml"),
            "[package]\nname = \"nested\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("tests/fixtures/x")).unwrap();
        std::fs::write(dir.join("tests/fixtures/x/Cargo.toml"), "[package]\n").unwrap();
        std::fs::create_dir_all(dir.join("target/y")).unwrap();
        std::fs::write(dir.join("target/y/Cargo.toml"), "[package]\n").unwrap();
        let projects = linked_projects(&dir).expect("nested crate found");
        assert_eq!(
            projects,
            vec![
                dir.join("Cargo.toml").display().to_string(),
                dir.join("nested/Cargo.toml").display().to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspace_root_and_flat_root_keep_auto_discovery() {
        let dir = scratch("workspace");
        std::fs::write(dir.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n").unwrap();
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::write(dir.join("a/Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(linked_projects(&dir), None);
        // A nested crate that is its own workspace root (an acceptance fixture placed inside a
        // workspace checkout) is linked next to the root; the member stays auto-discovered.
        std::fs::create_dir_all(dir.join("fixture-crate")).unwrap();
        std::fs::write(
            dir.join("fixture-crate/Cargo.toml"),
            "[package]\nname = \"fixture-crate\"\n\n[workspace]\n",
        )
        .unwrap();
        assert_eq!(
            linked_projects(&dir),
            Some(vec![
                dir.join("Cargo.toml").display().to_string(),
                dir.join("fixture-crate/Cargo.toml").display().to_string(),
            ])
        );
        let flat = scratch("flat");
        std::fs::write(flat.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(linked_projects(&flat), None);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&flat);
    }

    /// Returns a Rust profile over a harmless measured executable with one accepted configuration.
    fn session_profile(configuration: &str) -> RustProfile {
        session_profile_with_cargo_home(configuration, None)
    }

    /// [`session_profile`] with a caller-declared Cargo home override.
    fn session_profile_with_cargo_home(
        configuration: &str,
        cargo_home: Option<PathBuf>,
    ) -> RustProfile {
        RustProfile::new(RustProfileIdentity {
            binary: "/usr/bin/true".into(),
            rust_analyzer_version: "rust-analyzer contract-1".into(),
            cargo: "/usr/bin/true".into(),
            cargo_home,
            cargo_version: "cargo-test".into(),
            rustc: "/usr/bin/true".into(),
            rustc_version: "rustc-test".into(),
            rustup_toolchain: "test-toolchain".into(),
            configuration: configuration.into(),
            trust: "test".into(),
            transport: "stdio-v1".into(),
            cache_namespace: "/private/tmp/agent-ide-session-test-cache".into(),
        })
        .unwrap()
    }

    /// Managed sandbox initialization disables proc macros while retaining cache-priming suppression.
    #[test]
    fn managed_rust_settings_disable_proc_macro_expansion() {
        use agent_ide_core::intelligence::session::SessionProfile;
        assert_eq!(
            session_profile("cache-priming-and-proc-macro-disabled-v1").workspace_configuration(),
            serde_json::json!({
                "cachePriming":{"enable":false},
                "procMacro":{"enable":false}
            })
        );
        assert_eq!(
            session_profile("cache-priming-disabled-v1").workspace_configuration(),
            serde_json::json!({
                "cachePriming":{"enable":false},
                "procMacro":{"enable":true}
            })
        );
    }

    /// Only the exact analyzer identity is accepted, `.rs` opens as `rust`, and the server-status
    /// barrier is requested.
    #[test]
    fn rust_session_profile_requires_the_exact_analyzer_identity() {
        use agent_ide_core::intelligence::session::SessionProfile;
        let profile = session_profile("cache-priming-disabled-v1");
        let info = |name: &str, version: &str| async_lsp::lsp_types::ServerInfo {
            name: name.into(),
            version: Some(version.into()),
        };
        assert!(profile.accepts_server(Some(&info("rust-analyzer", "contract-1"))));
        assert!(!profile.accepts_server(Some(&info("rust-analyzer", "wrong-version"))));
        assert!(!profile.accepts_server(Some(&info("gopls", "contract-1"))));
        assert!(!profile.accepts_server(None));
        assert_eq!(
            profile.experimental_capabilities(),
            Some(serde_json::json!({"serverStatusNotification":true}))
        );
        assert_eq!(profile.status_method(), Some("experimental/serverStatus"));
        assert_eq!(profile.language_id(Path::new("lib.rs")), "rust");
        assert_eq!(profile.language_id(Path::new("main.go")), "plaintext");
    }

    /// Quiescent `ok` or `warning` (failed build scripts) is ready, quiescent `error` failed,
    /// anything not quiescent busy; a malformed status fails decoding.
    #[test]
    fn server_status_maps_quiescent_health_to_readiness() {
        use agent_ide_core::intelligence::session::{ProviderStatus, SessionProfile};
        let profile = session_profile("cache-priming-disabled-v1");
        let status = |health: &str, quiescent: bool| {
            profile
                .status(serde_json::json!({"health": health, "quiescent": quiescent}))
                .unwrap()
        };
        assert_eq!(status("ok", true), ProviderStatus::Ready);
        assert_eq!(status("warning", true), ProviderStatus::Ready);
        assert_eq!(status("error", true), ProviderStatus::Failed);
        assert_eq!(status("ok", false), ProviderStatus::Busy);
        assert!(
            profile
                .status(serde_json::json!({"health": "unknown"}))
                .is_err()
        );
    }

    /// A host that substitutes `HOME` (an empty `.cargo`, as `agent-run` runtimes ship) must not
    /// move the server environment: `HOME`/`CARGO_HOME` resolve from the real operator home —
    /// exactly the derivation the confined project check performs through the shared
    /// the shared `crate::home` helpers helpers — and a declared cargo home override wins for the server too.
    #[test]
    fn server_environment_resolves_home_and_cargo_home_like_the_project_check() {
        let root = scratch("cargo-home");
        let hostile = root.join("hostile-home");
        let real = root.join("real-home");
        std::fs::create_dir_all(hostile.join(".cargo")).unwrap();
        std::fs::create_dir_all(real.join(".cargo")).unwrap();
        let previous_override = std::env::var_os(agent_ide_core::userhome::HOME_OVERRIDE_ENV);
        let previous_home = std::env::var_os("HOME");
        // SAFETY: this workspace gate runs with --test-threads=1 and this test restores both
        // variables; nothing else in this test binary reads them meanwhile.
        unsafe {
            std::env::set_var("HOME", &hostile);
            std::env::set_var(agent_ide_core::userhome::HOME_OVERRIDE_ENV, &real);
        }
        let expected_cargo_home =
            crate::home::effective_cargo_home(None, &crate::home::real_home());
        // The derived server environment, exactly as a spawn would hand it to rust-analyzer.
        let environment = session_profile("cache-priming-disabled-v1").environment();
        assert_eq!(
            environment.get(OsStr::new("HOME")),
            Some(&OsString::from(&real))
        );
        assert_eq!(
            environment.get(OsStr::new("CARGO_HOME")),
            Some(&OsString::from(&expected_cargo_home))
        );
        // A declared cargo home override wins for the server environment, as for the check.
        let configured = session_profile_with_cargo_home(
            "cache-priming-disabled-v1",
            Some(hostile.join("configured-cargo")),
        )
        .environment();
        assert_eq!(
            configured.get(OsStr::new("CARGO_HOME")),
            Some(&OsString::from(hostile.join("configured-cargo")))
        );
        // SAFETY: restoring the exact pre-test environment for the remaining checks.
        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_override {
                Some(value) => {
                    std::env::set_var(agent_ide_core::userhome::HOME_OVERRIDE_ENV, value)
                }
                None => std::env::remove_var(agent_ide_core::userhome::HOME_OVERRIDE_ENV),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
