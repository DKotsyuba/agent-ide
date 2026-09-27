//! Exclusive TypeScript profile, bounded project-resolution evidence, and owned stdio lifecycle.
//!
//! The profile admits one immutable Node/bridge/TypeScript closure for one worktree incarnation.
//! It probes only fixed ancestor metadata names through Workspace and never lists directories or
//! discovers packages, plugins, configuration names, executables, or ancestor dependency trees
//! from ambient state.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use crate::{
    assistance::host_binding::ActiveBindingUse,
    execution::{
        AdmissionClass, AdmissionController, BackendReapCapability, BackendRelease, CommandKind,
        ControlledCommand, OwnedProtocolChild, OwnerId, ProcessError, ProviderLeaseAdmission,
        ProviderLeaseError, ProviderLeaseRegistry, ProviderViewLease, QueueTicket,
        ReapedProtocolProcess, ValidatedExecutionRequest, WaitedProtocolChild, WorkspaceAuthority,
    },
    workspace::{
        authority::WorktreeRef,
        observation::{
            MAX_RESOLUTION_INPUT_BYTES, ObservationError, read_authorized_resolution_input,
        },
    },
};

/// Maximum immutable closure members beyond the separately identified bridge and `tsserver.js`.
const MAX_BUNDLE_CLOSURE_FILES: usize = 64;
/// Maximum bytes accepted for any one bridge or TypeScript closure file.
const MAX_BUNDLE_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum project-resolution inputs retained for one document.
const MAX_RESOLUTION_FILES: usize = 16;
/// Maximum bytes represented by all project-resolution inputs for one document.
const MAX_RESOLUTION_BYTES: u64 = MAX_RESOLUTION_INPUT_BYTES as u64;
/// Exact filenames tsserver may consult while selecting the document's ancestor project.
const RESOLUTION_FILENAMES: [&str; 6] = [
    "jsconfig.json",
    "package-lock.json",
    "package.json",
    "pnpm-lock.yaml",
    "tsconfig.json",
    "yarn.lock",
];

/// One launcher-accepted regular file in the immutable provider bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptBundleFileV1 {
    /// Absolute normalized path selected by trusted restart-only configuration.
    pub path: PathBuf,
    /// BLAKE3 digest of the complete accepted bytes.
    pub blake3: blake3::Hash,
    /// Exact accepted byte length, bounded before hashing.
    pub bytes: u64,
}

/// Complete accepted identity for the release-pinned TypeScript provider bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptProviderBundleV1Identity {
    /// Absolute accepted Node executable used as the sole launched program.
    pub node: PathBuf,
    /// BLAKE3 digest of the accepted executable Node bytes.
    pub node_blake3: blake3::Hash,
    /// Exact accepted Node release identity.
    pub node_version: String,
    /// Accepted bundled TypeScript Language Server entry module.
    pub bridge: TypeScriptBundleFileV1,
    /// Exact accepted TypeScript Language Server release identity.
    pub bridge_version: String,
    /// Accepted `tsserver.js` entry selected explicitly during initialization.
    pub tsserver: TypeScriptBundleFileV1,
    /// Exact accepted TypeScript release identity.
    pub typescript_version: String,
    /// Sorted complete bridge/TypeScript runtime closure excluding `bridge` and `tsserver`.
    pub closure: Vec<TypeScriptBundleFileV1>,
}

/// Immutable, remeasurable TypeScript provider bundle accepted for one release.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptProviderBundleV1 {
    /// Complete accepted identity retained for pre-spawn remeasurement and compatibility hashing.
    identity: TypeScriptProviderBundleV1Identity,
    /// Opaque stable digest of all paths, lengths, contents, and public release identities.
    bundle_id: blake3::Hash,
}

impl TypeScriptProviderBundleV1 {
    /// Accepts one complete, sorted bundle only when every current file matches its declared bytes.
    ///
    /// Paths must be absolute and lexically normalized; closure paths must be unique and strictly
    /// sorted. The closure must contain between one and 64 files. Every non-Node file is a regular
    /// file of at most 64 MiB, while Node additionally must be executable. No path is discovered or
    /// added by this constructor. Missing, changed, oversized, duplicate, or malformed inputs return
    /// [`TypeScriptProfileError::InvalidBundle`].
    pub fn new(
        identity: TypeScriptProviderBundleV1Identity,
    ) -> Result<Self, TypeScriptProfileError> {
        if !normal_absolute(&identity.node)
            || !crate::execution::measured_executable_digest(&identity.node)
                .is_ok_and(|digest| digest == identity.node_blake3)
            || !bounded_identity(&identity.node_version)
            || !bounded_identity(&identity.bridge_version)
            || !bounded_identity(&identity.typescript_version)
            || identity.closure.is_empty()
            || identity.closure.len() > MAX_BUNDLE_CLOSURE_FILES
            || measure_bundle_file(&identity.bridge).is_err()
            || measure_bundle_file(&identity.tsserver).is_err()
        {
            return Err(TypeScriptProfileError::InvalidBundle);
        }
        let mut previous: Option<&Path> = None;
        for file in &identity.closure {
            if measure_bundle_file(file).is_err()
                || previous.is_some_and(|path| path >= file.path.as_path())
                || file.path == identity.bridge.path
                || file.path == identity.tsserver.path
            {
                return Err(TypeScriptProfileError::InvalidBundle);
            }
            previous = Some(&file.path);
        }
        let bundle_id = bundle_identity(&identity);
        Ok(Self {
            identity,
            bundle_id,
        })
    }

    /// Remeasures the complete accepted closure without discovering any additional dependency.
    ///
    /// Returns `InvalidBundle` on the first missing, replaced, non-regular, oversized, or
    /// content-mismatched member. This is called immediately before the one-time spawn capability
    /// is consumed; Execution separately rechecks Node at its final spawn boundary.
    pub fn verify(&self) -> Result<(), TypeScriptProfileError> {
        if !crate::execution::measured_executable_digest(&self.identity.node)
            .is_ok_and(|digest| digest == self.identity.node_blake3)
            || measure_bundle_file(&self.identity.bridge).is_err()
            || measure_bundle_file(&self.identity.tsserver).is_err()
            || self
                .identity
                .closure
                .iter()
                .any(|file| measure_bundle_file(file).is_err())
        {
            Err(TypeScriptProfileError::InvalidBundle)
        } else {
            Ok(())
        }
    }

    /// Returns the exact accepted Node executable used by the controlled command.
    pub fn node(&self) -> &Path {
        &self.identity.node
    }

    /// Returns the exact accepted bridge module passed as Node's first argument.
    pub fn bridge(&self) -> &Path {
        &self.identity.bridge.path
    }

    /// Returns the exact accepted `tsserver.js` initialization path.
    pub fn tsserver(&self) -> &Path {
        &self.identity.tsserver.path
    }

    /// Returns the stable full-bundle compatibility identity.
    pub const fn bundle_id(&self) -> blake3::Hash {
        self.bundle_id
    }
}

/// The per-path native-read proof required before any resolution input is read (T36B).
///
/// `true` means the caller's live host profile covers exactly this relative path; any
/// `false` fails the whole resolution closed. Implementations without a sandbox model (the
/// Claude helper) admit every path; the Codex session routes each candidate through
/// Execution's cwd-bound per-path read proof.
pub type ResolutionPathProof<'a> = dyn Fn(&Path) -> bool + 'a;

/// One already-observed project-resolution file identity; this type performs no filesystem read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectResolutionFileV1 {
    /// Normal relative path below the canonical worktree root.
    pub path: PathBuf,
    /// Digest of the exact bytes already observed by Workspace.
    pub blake3: blake3::Hash,
    /// Exact observed byte length counted against the resolution-input budget.
    pub bytes: u64,
}

/// Bounded canonical project-resolution evidence for one supported TypeScript-family document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectResolutionInputsV1 {
    /// Canonical worktree identity and incarnation that own every relative input.
    worktree: WorktreeRef,
    /// Normal document path relative to the worktree root.
    document: PathBuf,
    /// Fixed LSP language identifier derived solely from the supported extension.
    language_id: &'static str,
    /// Stable identity of the accepted immutable provider bundle.
    bundle_id: blake3::Hash,
    /// Strictly sorted unique supported configuration/package inputs.
    files: Vec<ProjectResolutionFileV1>,
    /// Opaque identity of the complete bounded resolution snapshot.
    identity: blake3::Hash,
}

impl ProjectResolutionInputsV1 {
    /// Reports whether two documents share the same worktree, immutable bundle, and captured project files.
    ///
    /// The document path and its JS/TS language identifier may differ; both remain valid documents
    /// for one TypeScript server session. A changed configuration, package input, worktree, or
    /// provider bundle selects a different project identity and requires a new session.
    pub fn same_project(&self, other: &Self) -> bool {
        self.worktree == other.worktree
            && self.bundle_id == other.bundle_id
            && self.files == other.files
    }

    /// Validates an already-observed canonical input set against a fresh Workspace observation.
    ///
    /// `document` must be an absolute `.js`, `.jsx`, `.ts`, or `.tsx` path below `worktree`.
    /// `files` may contain at most 16 strictly sorted, unique normal relative paths whose basenames
    /// are one of the supported TypeScript/JavaScript project or package inputs. Their declared
    /// sizes may total at most 8 MiB, and at least one `tsconfig.json` or `jsconfig.json` is
    /// required because inferred projects are unsupported. The selected config must explicitly
    /// admit the current document through a bounded top-level `files` entry or a literal `include`
    /// file/directory entry; wildcard includes and `exclude` are refused. Entries are UTF-8,
    /// normalized relative paths. `compilerOptions.types` must be `[]` or exactly `["vite/client"]`,
    /// and `moduleResolution` must be `node10` or `bundler`; a JavaScript or JSX document additionally
    /// requires `compilerOptions.allowJs` to be exactly `true`. `include` is supported only for
    /// literal paths so membership remains decidable. `compilerOptions.outDir` and `declarationDir`
    /// are refused because output membership is not observed. The optional `checkJs` and
    /// `noImplicitAny` options are closed to the exact boolean value `true` when present. Workspace
    /// reads only bounded project metadata without directory listing; package resolution is
    /// confined to real `node_modules` directories below the worktree, and ambient ancestor
    /// `node_modules` entries are refused. Missing declarations, changed bytes, unsupported
    /// composite project shapes, and any bound mismatch return
    /// [`TypeScriptProfileError::InvalidResolution`].
    pub fn new(
        worktree: WorktreeRef,
        document: PathBuf,
        bundle: &TypeScriptProviderBundleV1,
        files: Vec<ProjectResolutionFileV1>,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<Self, TypeScriptProfileError> {
        let relative = document
            .strip_prefix(worktree.worktree_path())
            .ok()
            .filter(|path| normal_relative(path))
            .ok_or(TypeScriptProfileError::InvalidResolution)?
            .to_path_buf();
        let language_id =
            typescript_language_id(&relative).ok_or(TypeScriptProfileError::InvalidResolution)?;
        validate_resolution_files(&relative, &files)?;
        if observe_resolution_files(&worktree, &relative, path_proof)? != files {
            return Err(TypeScriptProfileError::InvalidResolution);
        }
        let bundle_id = bundle.bundle_id();
        let identity = resolution_identity(&worktree, &relative, language_id, bundle_id, &files);
        Ok(Self {
            worktree,
            document: relative,
            language_id,
            bundle_id,
            files,
            identity,
        })
    }

    /// Observes the complete closed ancestor candidate set through Workspace's bounded reader.
    ///
    /// This is the shared Codex/future-Claude construction path. It derives only the six supported
    /// exact basenames at the document directory and each ancestor through the worktree root; it
    /// never lists a directory or scans packages. It also checks the exact `node_modules` name at
    /// each document ancestor through the filesystem root: a real directory inside the worktree is
    /// accepted, any other entry (ambient parent, symlink, file) is refused. At most 16 present
    /// files and 8 MiB total are accepted. Malformed JSON, a document the nearest configs do not
    /// admit, or a config/package shape that can redirect tsserver to undeclared config, plugin,
    /// workspace or path inputs fails closed with a [`ResolutionRejection`] naming the file and
    /// reason; [`Self::new`]'s bound failures on the freshly observed files map to a chain-level
    /// rejection.
    pub fn observe(
        worktree: WorktreeRef,
        document: PathBuf,
        bundle: &TypeScriptProviderBundleV1,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<Self, ResolutionRejection> {
        let relative = document
            .strip_prefix(worktree.worktree_path())
            .ok()
            .filter(|path| normal_relative(path))
            .ok_or_else(|| {
                ResolutionRejection::chain(format!(
                    "{} is not a normal path below the worktree",
                    document.display()
                ))
            })?
            .to_path_buf();
        let files = observe_resolution_files(&worktree, &relative, path_proof)?;
        Self::new(worktree, document, bundle, files, path_proof).map_err(|_| {
            ResolutionRejection::chain(
                "project inputs changed or exceeded their bounds while being observed",
            )
        })
    }

    /// Remeasures every present and missing ancestor candidate against this exact snapshot.
    ///
    /// Any addition, removal, replacement, byte change, unsupported shape, root replacement, or
    /// bound failure returns `InvalidResolution`. No caller receives a partially updated identity.
    pub fn verify(
        &self,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<(), TypeScriptProfileError> {
        (observe_resolution_files(&self.worktree, &self.document, path_proof)? == self.files)
            .then_some(())
            .ok_or(TypeScriptProfileError::InvalidResolution)
    }

    /// Returns the fixed LSP language identifier derived from the document extension.
    pub const fn language_id(&self) -> &'static str {
        self.language_id
    }

    /// Returns the canonical worktree identity that owns the resolution snapshot.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the normal document path relative to the worktree root.
    pub fn document(&self) -> &Path {
        &self.document
    }

    /// Returns the strictly sorted observed project-resolution files.
    pub fn files(&self) -> &[ProjectResolutionFileV1] {
        &self.files
    }
}

/// One immutable exclusive TypeScript profile for an exact worktree resolution snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptProfile {
    /// Release-pinned executable and dependency closure.
    bundle: TypeScriptProviderBundleV1,
    /// Exact bounded project-resolution evidence.
    resolution: ProjectResolutionInputsV1,
    /// Explicit operator trust identity included in compatibility.
    trust: String,
    /// Absolute private cache/temp namespace for this worktree profile.
    cache_namespace: PathBuf,
}

impl TypeScriptProfile {
    /// Builds one exact profile after matching bundle, worktree, trust, and cache invariants.
    pub fn new(
        bundle: TypeScriptProviderBundleV1,
        resolution: ProjectResolutionInputsV1,
        trust: String,
        cache_namespace: PathBuf,
    ) -> Result<Self, TypeScriptProfileError> {
        if resolution.bundle_id != bundle.bundle_id()
            || !bounded_identity(&trust)
            || !normal_absolute(&cache_namespace)
        {
            return Err(TypeScriptProfileError::InvalidProfile);
        }
        Ok(Self {
            bundle,
            resolution,
            trust,
            cache_namespace,
        })
    }

    /// Builds fixed `node <bridge> --stdio` with a private temp and no caller-visible `PATH`.
    ///
    /// The private temp is `TMPDIR`-redirected to `<host temp>/<cache namespace id>/tmp`, not into
    /// the daemon runtime directory that owns the cache namespace: host command sandboxes (the
    /// macOS 27 Claude Bash seatbelt refused every `/private/tmp/ai-r-*` write, T38B acceptance
    /// evidence) admit the per-user temp directory but not the daemon's private runtime, and the
    /// bridge creates its own directories under `TMPDIR` before its first protocol byte.
    pub fn command(
        &self,
        worktree: &TypeScriptWorktree,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<ControlledCommand, TypeScriptProfileError> {
        if self.resolution.worktree() != worktree.worktree() {
            return Err(TypeScriptProfileError::WorktreeMismatch);
        }
        self.bundle.verify()?;
        self.resolution.verify(path_proof)?;
        let namespace = self
            .cache_namespace
            .file_name()
            .ok_or(TypeScriptProfileError::InvalidProfile)?;
        let temp = std::env::temp_dir().join(namespace).join("tmp");
        std::fs::create_dir_all(&temp).map_err(|_| TypeScriptProfileError::InvalidProfile)?;
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.bundle.node().to_path_buf(),
            vec![
                self.bundle.bridge().as_os_str().to_owned(),
                OsString::from("--stdio"),
            ],
            worktree.worktree().worktree_path().to_path_buf(),
            BTreeMap::from([(OsString::from("TMPDIR"), temp.into_os_string())]),
        )
        .map_err(|_| TypeScriptProfileError::InvalidProfile)?;
        command
            .has_program_digest(&self.bundle.identity.node_blake3)
            .then_some(command)
            .ok_or(TypeScriptProfileError::InvalidBundle)
    }

    /// Remeasures the complete immutable provider bundle at the final profile boundary.
    pub fn verify_bundle(&self) -> Result<(), TypeScriptProfileError> {
        self.bundle.verify()
    }

    /// Remeasures the complete document-to-worktree resolution boundary.
    pub fn verify_resolution(
        &self,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<(), TypeScriptProfileError> {
        self.resolution.verify(path_proof)
    }

    /// Returns fixed initialization options disabling typing acquisition, plugins, package auto
    /// imports, syntax servers, logs, and traces while selecting only the accepted `tsserver.js`.
    pub fn initialization_options(&self) -> serde_json::Value {
        serde_json::json!({
            "disableAutomaticTypingAcquisition": true,
            "plugins": [],
            "preferences": {"includePackageJsonAutoImports": "off"},
            "tsserver": {
                "path": self.bundle.tsserver().display().to_string(),
                "useSyntaxServer": "never",
                "logVerbosity": "off",
                "trace": "off"
            }
        })
    }

    /// Returns the stable exact-profile key, including worktree and resolution identities.
    fn compatibility_key(&self, worktree: &TypeScriptWorktree) -> TypeScriptCompatibilityKey {
        let mut hash = blake3::Hasher::new();
        for value in [
            self.bundle.bundle_id().to_hex().to_string(),
            self.resolution.identity.to_hex().to_string(),
            self.trust.clone(),
            self.cache_namespace.display().to_string(),
            worktree.worktree().id().to_owned(),
            worktree.worktree().incarnation().to_string(),
        ] {
            hash.update(value.as_bytes());
            hash.update(&[0]);
        }
        TypeScriptCompatibilityKey {
            digest: hash.finalize().to_hex().to_string(),
        }
    }
}

impl crate::intelligence::session::SessionProfile for TypeScriptProfile {
    /// Answers configuration requests with the fixed initialization options.
    fn workspace_configuration(&self) -> serde_json::Value {
        self.initialization_options()
    }

    /// Accepts an omitted identity or one named `typescript-language-server`.
    fn accepts_server(&self, info: Option<&async_lsp::lsp_types::ServerInfo>) -> bool {
        info.is_none_or(|info| info.name == "typescript-language-server")
    }

    /// The bridge pushes unversioned diagnostics; a diagnostics wait never exceeds two seconds.
    fn diagnostic_wait_cap(&self) -> Option<Duration> {
        Some(Duration::from_secs(2))
    }

    /// A nonempty unversioned push after the initial open describes the opened bytes.
    fn accepts_unversioned_initial_report(&self) -> bool {
        true
    }

    /// Opens `.js`/`.jsx`/`.ts`/`.tsx` with their exact identifiers; everything else (including
    /// `.mts`/`.cts`/`.mjs`/`.cjs`) stays `plaintext`.
    fn language_id(&self, path: &Path) -> &'static str {
        typescript_language_id(path).unwrap_or("plaintext")
    }
}

/// Couples one canonical worktree incarnation to its exact Execution authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptWorktree {
    /// Canonical worktree identity and root.
    worktree: WorktreeRef,
    /// Current authority required for provider admission and spawn.
    authority: WorkspaceAuthority,
}

impl TypeScriptWorktree {
    /// Rejects any worktree identity, incarnation, or root mismatch before admission.
    pub fn new(
        worktree: WorktreeRef,
        authority: WorkspaceAuthority,
    ) -> Result<Self, TypeScriptProfileError> {
        (worktree.id() == authority.worktree_id()
            && worktree.incarnation().to_string() == authority.incarnation()
            && worktree.worktree_path() == authority.root())
        .then_some(Self {
            worktree,
            authority,
        })
        .ok_or(TypeScriptProfileError::WorktreeMismatch)
    }

    /// Returns the canonical worktree reference used by resolution and command construction.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the current authority consumed by Execution admission.
    pub fn authority(&self) -> &WorkspaceAuthority {
        &self.authority
    }
}

/// Explains why an exact TypeScript profile cannot serve one operation.
#[derive(Debug)]
pub enum TypeScriptProfileError {
    /// The immutable Node/bridge/TypeScript closure is malformed, missing, or changed.
    InvalidBundle,
    /// The bounded project-resolution snapshot is malformed or unsupported.
    InvalidResolution,
    /// Trust, cache, or cross-profile identity is invalid.
    InvalidProfile,
    /// Execution authority does not describe the profile's canonical worktree.
    WorktreeMismatch,
    /// This exact profile was quarantined for the lifetime of its owner.
    Quarantined,
    /// Execution rejected an exclusive provider lease operation.
    Execution(ProviderLeaseError),
    /// Execution refused admission under current capacity policy.
    Refused(crate::execution::AdmissionError),
    /// Execution could not start, signal, or reap the owned direct bridge child.
    Process(ProcessError),
}

/// Why one bounded project-resolution observation refused the document, in words a caller can
/// act on.
///
/// `file` is the worktree-relative project file consulted when the refusal concerns one (a
/// `tsconfig.json`, `jsconfig.json` or `package.json`); it is `None` for chain-level refusals such
/// as a missing config or an ambient `node_modules` entry. `reason` is one short fragment without a
/// trailing period. The closed public [`TypeScriptProfileError::InvalidResolution`] carries no
/// reason; callers that render error text keep this value instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolutionRejection {
    /// Worktree-relative project file the refusal is attributed to, when there is one.
    pub file: Option<PathBuf>,
    /// Short plain-words reason.
    pub reason: String,
}

impl ResolutionRejection {
    /// Builds a refusal attributed to one consulted project file.
    fn at(file: &Path, reason: impl Into<String>) -> Self {
        Self {
            file: Some(file.to_path_buf()),
            reason: reason.into(),
        }
    }

    /// Builds a chain-level refusal not attributable to one project file.
    fn chain(reason: impl Into<String>) -> Self {
        Self {
            file: None,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ResolutionRejection {
    /// Renders `<file>: <reason>`, or the bare reason for chain-level refusals.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(file) => write!(formatter, "{}: {}", file.display(), self.reason),
            None => formatter.write_str(&self.reason),
        }
    }
}

impl From<ResolutionRejection> for TypeScriptProfileError {
    /// Collapses an explained refusal into the closed public category.
    fn from(_: ResolutionRejection) -> Self {
        Self::InvalidResolution
    }
}

/// Reports every bounded outcome of requesting an exclusive TypeScript view.
#[derive(Debug)]
pub enum TypeScriptViewAdmission {
    /// One fresh exclusive view owns a one-time bridge launch capability.
    Granted(TypeScriptView),
    /// Admission queued without launching or reserving a view.
    Queued(QueueTicket),
    /// Central capacity policy refused the operation.
    Refused(crate::execution::AdmissionError),
    /// Profile validation, quarantine, or registry rules refused the operation.
    Unavailable(TypeScriptProfileError),
}

/// Opaque key of every compatibility input for one exact worktree profile.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct TypeScriptCompatibilityKey {
    /// Stable hexadecimal BLAKE3 digest of the exact profile inputs.
    digest: String,
}

/// One exclusive TypeScript view and its owner-lifetime quarantine identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypeScriptView {
    /// One Execution view lease consumed by spawn and release.
    lease: ProviderViewLease,
    /// Exact profile key retained if this operation becomes abnormal.
    key: TypeScriptCompatibilityKey,
    /// Monotonic provider generation used for response correlation.
    generation: u64,
}

impl TypeScriptView {
    /// Returns the sole Execution lease for this exclusive profile operation.
    pub const fn lease(&self) -> ProviderViewLease {
        self.lease
    }

    /// Returns the monotonic protocol generation assigned by the owner.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// Owner-lifetime exclusive-view and exact-profile quarantine state.
#[derive(Debug, Default)]
pub struct TypeScriptProfiles {
    /// Strictly increasing generation for newly granted views.
    next_generation: u64,
    /// Exact profile keys that no later operation may reuse in this owner lifetime.
    quarantined: BTreeSet<TypeScriptCompatibilityKey>,
}

impl TypeScriptProfiles {
    /// Requests one exclusive view unless this exact profile was previously quarantined.
    pub fn request(
        &mut self,
        profile: &TypeScriptProfile,
        worktree: &TypeScriptWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        owner: OwnerId,
        class: AdmissionClass,
    ) -> TypeScriptViewAdmission {
        let key = profile.compatibility_key(worktree);
        if self.quarantined.contains(&key) {
            return TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Quarantined);
        }
        match registry.request(
            admission,
            owner,
            class,
            &key.digest,
            crate::execution::ProviderBackendKind::OwnedExclusive,
            worktree.authority(),
        ) {
            ProviderLeaseAdmission::Granted(lease) => {
                self.next_generation = self
                    .next_generation
                    .checked_add(1)
                    .expect("TypeScript generation exhausted");
                TypeScriptViewAdmission::Granted(TypeScriptView {
                    lease,
                    key,
                    generation: self.next_generation,
                })
            }
            ProviderLeaseAdmission::Queued(ticket) => TypeScriptViewAdmission::Queued(ticket),
            ProviderLeaseAdmission::Refused(error) => TypeScriptViewAdmission::Refused(error),
            ProviderLeaseAdmission::Rejected(error) => {
                TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Execution(error))
            }
        }
    }

    /// Quarantines the view's exact profile key for this state owner's complete lifetime.
    ///
    /// This transition is monotonic and does not release the Execution view; callers still must
    /// perform abnormal cleanup and complete direct-child accounting independently.
    pub fn quarantine(&mut self, view: &TypeScriptView) {
        self.quarantined.insert(view.key.clone());
    }

    /// Quarantines a view when its actual direct-child wait proves a nonzero bridge exit.
    ///
    /// Returns `true` exactly for the failure branch so the provider can map that observed exit to
    /// its public unavailable result while retaining the same owner-lifetime profile fence.
    pub(crate) fn quarantine_after_unsuccessful_wait(
        &mut self,
        view: &TypeScriptView,
        waited: &WaitedProtocolChild,
    ) -> bool {
        if waited.success() {
            false
        } else {
            self.quarantine(view);
            true
        }
    }

    /// Releases one exclusive view and returns the capability required for direct-child settlement.
    pub fn release(
        &mut self,
        view: TypeScriptView,
        registry: &mut ProviderLeaseRegistry,
    ) -> Result<BackendReapCapability, TypeScriptProfileError> {
        let BackendRelease::ReapOwned(capability) = registry
            .release(view.lease)
            .map_err(TypeScriptProfileError::Execution)?
        else {
            return Err(TypeScriptProfileError::Execution(
                ProviderLeaseError::InvalidBackend,
            ));
        };
        Ok(capability)
    }
}

/// Wraps only the Execution-owned direct bridge child for TypeScript-specific settlement choices.
pub struct TypeScriptProtocolChild {
    /// Sole owned protocol child; arbitrary descendants never become waitable capabilities.
    child: OwnedProtocolChild,
}

impl TypeScriptProtocolChild {
    /// Remeasures the bundle and project inputs, then starts one authority-bound bridge.
    ///
    /// A bundle or resolution mismatch cancels the unstarted view through `registry` and
    /// `admission`. Authority or spawn failures return the linear no-child/process evidence from
    /// Execution without inventing descendant settlement.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        profile: &TypeScriptProfile,
        worktree: &TypeScriptWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
        active_use: Option<ActiveBindingUse>,
        output_cap: usize,
        path_proof: &ResolutionPathProof<'_>,
    ) -> Result<Self, TypeScriptProfileError> {
        if let Err(error) = profile
            .verify_bundle()
            .and_then(|()| profile.verify_resolution(path_proof))
        {
            registry
                .cancel_unstarted(admission, view)
                .map_err(TypeScriptProfileError::Execution)?;
            return Err(error);
        }
        let capability = registry
            .take_spawn_lease(view)
            .map_err(TypeScriptProfileError::Execution)?;
        if request.authority() != worktree.authority() {
            return Err(TypeScriptProfileError::Process(
                ProcessError::NeverStarted {
                    cause: Box::new(ProcessError::Request(
                        crate::execution::RequestError::WorktreeDenied,
                    )),
                    settlement: capability.cancel(),
                },
            ));
        }
        OwnedProtocolChild::spawn_from_provider_lease(request, capability, active_use, output_cap)
            .map(|child| Self { child })
            .map_err(TypeScriptProfileError::Process)
    }

    /// Borrows the sole stdout reader and stdin writer for the bounded LSP session.
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

    /// Reaps a normally shut down bridge without requesting TERM or KILL.
    pub async fn reap(
        self,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, TypeScriptProfileError> {
        self.child
            .reap(deadline)
            .await
            .map_err(TypeScriptProfileError::Process)
    }

    /// Waits without consuming or signalling so a timeout can enter ordered abnormal cleanup.
    pub(crate) async fn wait_for_exit(
        &mut self,
        deadline: Duration,
    ) -> Result<WaitedProtocolChild, TypeScriptProfileError> {
        self.child
            .wait_for_exit(deadline)
            .await
            .map_err(TypeScriptProfileError::Process)
    }

    /// Converts an already waited direct bridge child into its sole Execution reap proof.
    pub(crate) async fn finish_reap(
        self,
        waited: WaitedProtocolChild,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, TypeScriptProfileError> {
        self.child
            .finish_reap(waited, deadline)
            .await
            .map_err(TypeScriptProfileError::Process)
    }

    /// Runs the fixed group TERM/grace/group-and-direct KILL/direct-reap abnormal sequence.
    pub async fn terminate_abnormally(
        self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, TypeScriptProfileError> {
        self.child
            .terminate_abnormally(grace, deadline)
            .await
            .map_err(TypeScriptProfileError::Process)
    }
}

/// Hashes one accepted regular file under the fixed per-file byte ceiling.
fn measure_bundle_file(file: &TypeScriptBundleFileV1) -> Result<(), TypeScriptProfileError> {
    if !normal_absolute(&file.path) || file.bytes > MAX_BUNDLE_FILE_BYTES {
        return Err(TypeScriptProfileError::InvalidBundle);
    }
    let metadata =
        std::fs::metadata(&file.path).map_err(|_| TypeScriptProfileError::InvalidBundle)?;
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Err(TypeScriptProfileError::InvalidBundle);
    }
    let mut input = File::open(&file.path).map_err(|_| TypeScriptProfileError::InvalidBundle)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|_| TypeScriptProfileError::InvalidBundle)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    (hash.finalize() == file.blake3)
        .then_some(())
        .ok_or(TypeScriptProfileError::InvalidBundle)
}

/// Produces the stable bundle identity from every accepted path, size, digest, and release string.
fn bundle_identity(identity: &TypeScriptProviderBundleV1Identity) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for value in [
        identity.node.display().to_string(),
        identity.node_blake3.to_hex().to_string(),
        identity.node_version.clone(),
        identity.bridge_version.clone(),
        identity.typescript_version.clone(),
    ] {
        hash.update(value.as_bytes());
        hash.update(&[0]);
    }
    for file in std::iter::once(&identity.bridge)
        .chain(std::iter::once(&identity.tsserver))
        .chain(identity.closure.iter())
    {
        hash.update(file.path.as_os_str().as_encoded_bytes());
        hash.update(&file.bytes.to_le_bytes());
        hash.update(file.blake3.as_bytes());
    }
    hash.finalize()
}

/// Produces one opaque identity for all bounded project-resolution facts.
fn resolution_identity(
    worktree: &WorktreeRef,
    document: &Path,
    language_id: &str,
    bundle_id: blake3::Hash,
    files: &[ProjectResolutionFileV1],
) -> blake3::Hash {
    let mut hash = blake3::Hasher::new();
    for value in [
        worktree.id().as_bytes(),
        worktree.incarnation().to_string().as_bytes(),
        document.as_os_str().as_encoded_bytes(),
        language_id.as_bytes(),
        bundle_id.as_bytes(),
    ] {
        hash.update(value);
        hash.update(&[0]);
    }
    for file in files {
        hash.update(file.path.as_os_str().as_encoded_bytes());
        hash.update(&file.bytes.to_le_bytes());
        hash.update(file.blake3.as_bytes());
    }
    hash.finalize()
}

/// Returns the fixed LSP language ID for the closed TypeScript-family extension set.
fn typescript_language_id(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("js") => Some("javascript"),
        Some("jsx") => Some("javascriptreact"),
        Some("ts") => Some("typescript"),
        Some("tsx") => Some("typescriptreact"),
        _ => None,
    }
}

/// Returns whether a relative path is nonempty and contains only normal components.
fn normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

/// Returns whether an absolute path contains only its root and normal components.
fn normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Returns whether a bounded identity is nonempty and contains no control character.
fn bounded_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Returns whether one relative input basename participates in the closed resolution profile.
fn supported_resolution_file(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            "tsconfig.json"
                | "jsconfig.json"
                | "package.json"
                | "package-lock.json"
                | "yarn.lock"
                | "pnpm-lock.yaml"
        )
    )
}

/// Validates canonical bounds and requires every input to be on the document's ancestor chain.
///
/// Returns a chain-level [`ResolutionRejection`] when no `tsconfig.json`/`jsconfig.json` is present
/// or the file count is over the ceiling, and a file-attributed one for a member that is
/// unsupported, off the ancestor chain, unsorted or over the byte budget.
fn validate_resolution_files(
    document: &Path,
    files: &[ProjectResolutionFileV1],
) -> Result<(), ResolutionRejection> {
    let parent = document
        .parent()
        .ok_or_else(|| ResolutionRejection::chain("document has no parent directory"))?;
    if files.len() > MAX_RESOLUTION_FILES {
        return Err(ResolutionRejection::chain(format!(
            "more than {MAX_RESOLUTION_FILES} project files on the ancestor chain"
        )));
    }
    if !files.iter().any(|file| {
        matches!(
            file.path.file_name().and_then(|name| name.to_str()),
            Some("tsconfig.json" | "jsconfig.json")
        )
    }) {
        let scope = if parent.as_os_str().is_empty() {
            "in the worktree root".to_owned()
        } else {
            format!("from {} up to the worktree root", parent.display())
        };
        return Err(ResolutionRejection::chain(format!(
            "no tsconfig.json or jsconfig.json was found {scope}"
        )));
    }
    let directories = parent.ancestors().collect::<BTreeSet<_>>();
    let mut total = 0_u64;
    let mut previous: Option<&Path> = None;
    for file in files {
        total = total.saturating_add(file.bytes);
        if file.bytes > MAX_RESOLUTION_BYTES || total > MAX_RESOLUTION_BYTES {
            return Err(ResolutionRejection::at(
                &file.path,
                format!("project files exceed the {MAX_RESOLUTION_BYTES} byte resolution budget"),
            ));
        }
        if !normal_relative(&file.path)
            || !supported_resolution_file(&file.path)
            || !file
                .path
                .parent()
                .is_some_and(|path| directories.contains(path))
            || previous.is_some_and(|path| path >= file.path.as_path())
        {
            return Err(ResolutionRejection::at(
                &file.path,
                "is not a supported project file on the document's ancestor chain",
            ));
        }
        previous = Some(&file.path);
    }
    Ok(())
}

/// Reads the complete deterministic ancestor candidate set and returns sorted present identities.
///
/// `path_proof` must authorize each auxiliary resolution input before Workspace reads it
/// (T36B): proving the requested source document never authorizes these files. An unproven
/// candidate fails the resolution closed — it is never skipped as if missing. Every refusal names
/// the file and reason through [`ResolutionRejection`].
fn observe_resolution_files(
    worktree: &WorktreeRef,
    document: &Path,
    path_proof: &ResolutionPathProof<'_>,
) -> Result<Vec<ProjectResolutionFileV1>, ResolutionRejection> {
    validate_node_modules_ancestry(worktree, document)?;
    let language_id = typescript_language_id(document)
        .ok_or_else(|| ResolutionRejection::chain("unsupported source extension"))?;
    let parent = document
        .parent()
        .ok_or_else(|| ResolutionRejection::chain("document has no parent directory"))?;
    let mut candidates = BTreeSet::new();
    for directory in parent.ancestors() {
        for filename in RESOLUTION_FILENAMES {
            candidates.insert(directory.join(filename));
        }
    }
    let mut files = Vec::new();
    let mut remaining = MAX_RESOLUTION_BYTES as usize;
    for path in candidates {
        if !path_proof(&path) {
            return Err(ResolutionRejection::at(
                &path,
                "is outside the allowed roots",
            ));
        }
        let observed = match read_authorized_resolution_input(worktree, &path, remaining) {
            Ok(observed) => observed,
            Err(ObservationError::Missing) => continue,
            Err(_) => {
                return Err(ResolutionRejection::at(
                    &path,
                    "could not be read within the resolution bounds",
                ));
            }
        };
        validate_resolution_shape(observed.path(), observed.contents(), language_id, document)?;
        remaining = remaining
            .checked_sub(observed.length() as usize)
            .ok_or_else(|| {
                ResolutionRejection::at(
                    &path,
                    format!(
                        "project files exceed the {MAX_RESOLUTION_BYTES} byte resolution budget"
                    ),
                )
            })?;
        files.push(ProjectResolutionFileV1 {
            path: observed.path().to_path_buf(),
            blake3: observed.digest(),
            bytes: observed.length(),
        });
        if files.len() > MAX_RESOLUTION_FILES {
            return Err(ResolutionRejection::chain(format!(
                "more than {MAX_RESOLUTION_FILES} project files on the ancestor chain"
            )));
        }
    }
    validate_resolution_files(document, &files)?;
    Ok(files)
}

/// Allows only real `node_modules` directories inside the worktree, never an ambient parent.
///
/// Checks the exact `node_modules` name at the document directory and every ancestor through the
/// filesystem root. A symlink, a file, an unreadable entry, or any entry above the worktree root
/// is refused because tsserver could resolve unobserved packages through it.
fn validate_node_modules_ancestry(
    worktree: &WorktreeRef,
    document: &Path,
) -> Result<(), ResolutionRejection> {
    let root = worktree.worktree_path();
    let mut directory = root.join(document).parent().map(Path::to_path_buf);
    while let Some(path) = directory {
        let is_inside = path.starts_with(root);
        let entry = path.join("node_modules");
        match std::fs::symlink_metadata(&entry) {
            Ok(metadata)
                if is_inside && metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(ResolutionRejection::chain(format!(
                    "{} is not a real directory inside the worktree; ambient dependency directories are refused",
                    entry.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(ResolutionRejection::chain(format!(
                    "{} could not be inspected",
                    entry.display()
                )));
            }
        }
        directory = path.parent().map(Path::to_path_buf);
    }
    Ok(())
}

/// Rejects project metadata that can redirect tsserver beyond the exact observed ancestor inputs.
///
/// `language_id` is the fixed ID for the opened document and `document` is its worktree-relative
/// path. Configs must admit the document through a bounded top-level `files` entry or a literal
/// `include` file/directory entry (see [`validate_configured_membership`]); `exclude` is refused
/// because glob membership is not observed. `compilerOptions.types` must be `[]` or exactly
/// `["vite/client"]`, `moduleResolution` must be `node10` or `bundler`, JavaScript-family documents
/// require boolean `allowJs: true`, path-mapping/plugin/output options are refused because their
/// inputs are not observed, and `checkJs`/`noImplicitAny` accept only `true` when present.
/// `package.json` may declare dependencies but not `imports` or `workspaces`. Lockfiles are only
/// fingerprinted. Every refusal is attributed to `path` with its reason.
fn validate_resolution_shape(
    path: &Path,
    contents: &[u8],
    language_id: &str,
    document: &Path,
) -> Result<(), ResolutionRejection> {
    let reject = |reason: String| ResolutionRejection::at(path, reason);
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Err(reject("has no file name".into()));
    };
    if matches!(name, "yarn.lock" | "pnpm-lock.yaml") {
        return Ok(());
    }
    let value: serde_json::Value =
        serde_json::from_slice(contents).map_err(|_| reject("is not valid JSON".into()))?;
    let object = value
        .as_object()
        .ok_or_else(|| reject("is not a JSON object".into()))?;
    if matches!(name, "tsconfig.json" | "jsconfig.json") {
        if let Some(key) = ["extends", "references", "typeAcquisition"]
            .iter()
            .find(|key| object.contains_key(**key))
        {
            return Err(reject(format!("`{key}` is unsupported")));
        }
        let options = object
            .get("compilerOptions")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| reject("`compilerOptions` must be an object".into()))?;
        let configured_files = object.get("files").and_then(serde_json::Value::as_array);
        let configured_includes = object.get("include").and_then(serde_json::Value::as_array);
        validate_configured_membership(
            path,
            document,
            configured_files.map(Vec::as_slice),
            configured_includes.map(Vec::as_slice),
            options.get("allowJs") == Some(&serde_json::Value::Bool(true)),
        )?;
        if !closed_types_option(options) {
            return Err(reject(
                "`compilerOptions.types` must be `[]` or `[\"vite/client\"]`".into(),
            ));
        }
        if !closed_module_resolution(options) {
            return Err(reject(
                "`compilerOptions.moduleResolution` must be `node10` or `bundler`, and `module` must not be node16/nodenext/preserve".into(),
            ));
        }
        if matches!(language_id, "javascript" | "javascriptreact")
            && options.get("allowJs") != Some(&serde_json::Value::Bool(true))
        {
            return Err(reject(
                "JavaScript documents require `compilerOptions.allowJs: true`".into(),
            ));
        }
        if let Some(key) = [
            "baseUrl",
            "paths",
            "plugins",
            "rootDirs",
            "typeRoots",
            "outDir",
            "declarationDir",
        ]
        .iter()
        .find(|key| options.contains_key(**key))
        {
            return Err(reject(format!("`compilerOptions.{key}` is unsupported")));
        }
        if let Some(key) = ["checkJs", "noImplicitAny"].iter().find(|key| {
            options
                .get(**key)
                .is_some_and(|value| value != &serde_json::Value::Bool(true))
        }) {
            return Err(reject(format!(
                "`compilerOptions.{key}` must be `true` when present"
            )));
        }
        if object.contains_key("exclude") {
            return Err(reject(
                "`exclude` is unsupported; membership must be literal".into(),
            ));
        }
    } else if name == "package.json"
        && let Some(key) = ["imports", "workspaces"]
            .iter()
            .find(|key| object.contains_key(**key))
    {
        return Err(reject(format!("`{key}` is unsupported")));
    }
    Ok(())
}

/// Validates one config's bounded literal membership list against the current document.
///
/// `files` entries must match the current relative document exactly. Literal `include` directories
/// (entries without an extension) recursively admit only the supported source extensions
/// (JavaScript only with `allowJs`); literal file includes match exactly. Wildcards, backslashes,
/// duplicates, non-normal paths, more than 16 entries per list and implicit all-files membership
/// are refused. The refusal is attributed to `config` and names the worktree-relative document.
fn validate_configured_membership(
    config: &Path,
    document: &Path,
    files: Option<&[serde_json::Value]>,
    includes: Option<&[serde_json::Value]>,
    allow_js: bool,
) -> Result<(), ResolutionRejection> {
    let reject = |reason: String| ResolutionRejection::at(config, reason);
    if files.is_none() && includes.is_none_or(<[_]>::is_empty)
        || files.is_some_and(|entries| entries.len() > MAX_RESOLUTION_FILES)
        || includes
            .is_some_and(|entries| entries.is_empty() || entries.len() > MAX_RESOLUTION_FILES)
    {
        return Err(reject(format!(
            "needs `files` or a nonempty literal `include` list with at most {MAX_RESOLUTION_FILES} entries"
        )));
    }
    let config_directory = config
        .parent()
        .ok_or_else(|| reject("has no directory".into()))?;
    let relative = document
        .strip_prefix(config_directory)
        .ok()
        .filter(|path| normal_relative(path))
        .ok_or_else(|| {
            reject(format!(
                "{} is not below the config directory",
                document.display()
            ))
        })?;
    let mut seen = BTreeSet::new();
    let mut parse_entries = |entries: &[serde_json::Value]| {
        for entry in entries {
            let value = entry
                .as_str()
                .ok_or_else(|| reject("has a non-string `files`/`include` entry".into()))?;
            let path = Path::new(value);
            if value.contains('\\')
                || value.chars().any(|character| {
                    matches!(
                        character,
                        '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '!'
                    )
                })
                || !normal_relative(path)
                || value.split('/').any(str::is_empty)
                || !seen.insert(path.to_path_buf())
            {
                return Err(reject(format!(
                    "entry `{value}` must be a unique normalized relative path without wildcards"
                )));
            }
        }
        Ok(())
    };
    if let Some(files) = files {
        parse_entries(files)?;
    }
    if let Some(includes) = includes {
        parse_entries(includes)?;
    }
    let source_family = matches!(
        typescript_language_id(relative),
        Some("typescript" | "typescriptreact")
    ) || (allow_js
        && matches!(
            typescript_language_id(relative),
            Some("javascript" | "javascriptreact")
        ));
    let exact = files.is_some_and(|files| {
        files.iter().any(|entry| {
            entry
                .as_str()
                .is_some_and(|value| Path::new(value) == relative)
        })
    });
    let included = includes.is_some_and(|includes| {
        includes.iter().any(|entry| {
            entry.as_str().is_some_and(|value| {
                let path = Path::new(value);
                relative == path
                    || (path.extension().is_none() && relative.starts_with(path) && source_family)
            })
        })
    });
    if exact || included {
        Ok(())
    } else {
        Err(reject(format!(
            "{} is not listed in `files` or under a literal `include` entry",
            document.display()
        )))
    }
}

/// Allows only no global type packages or the bounded Vite client declaration entry.
fn closed_types_option(options: &serde_json::Map<String, serde_json::Value>) -> bool {
    match options.get("types").and_then(serde_json::Value::as_array) {
        Some(types) if types.is_empty() => true,
        Some(types) if types.len() == 1 => types[0].as_str() == Some("vite/client"),
        _ => false,
    }
}

/// Returns whether compiler options select only the explicit closed TypeScript resolution mode.
///
/// `node10` and `bundler` are supported with project-root authority and explicit membership.
/// Missing, alternate, or unknown resolution modes are rejected. Node16, NodeNext, and Preserve
/// module modes are also refused because they select incompatible package-resolution semantics.
fn closed_module_resolution(options: &serde_json::Map<String, serde_json::Value>) -> bool {
    options
        .get("moduleResolution")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|mode| {
            mode.eq_ignore_ascii_case("node10") || mode.eq_ignore_ascii_case("bundler")
        })
        && !options
            .get("module")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|module| {
                ["node16", "nodenext", "preserve"]
                    .iter()
                    .any(|forbidden| module.eq_ignore_ascii_case(forbidden))
            })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        AdmissionLimits, LocalExecutionPolicy, ProviderLeaseLimits, ValidatedExecutionRequest,
        ValidatedHostInvocation,
    };
    use std::{collections::BTreeSet, os::unix::fs::PermissionsExt};

    /// Exact closed config bytes accepted by TypeScript resolution fixtures.
    const CLOSED_CONFIG: &[u8] =
        br#"{"compilerOptions":{"types":[],"moduleResolution":"node10"},"files":["fixture.js","fixture.jsx","fixture.ts","fixture.tsx"]}"#;
    /// Exact closed config bytes accepted for JavaScript-family resolution fixtures.
    const JAVASCRIPT_CONFIG: &[u8] = br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["fixture.js","fixture.jsx","fixture.ts","fixture.tsx"]}"#;

    /// Distinguishes parallel fixture roots when the platform clock has coarse resolution.
    static NEXT_FIXTURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Owns one unique directory and removes its files after each profile test.
    struct Fixture {
        /// Unique test-owned root containing every bundle and cache fixture.
        root: PathBuf,
        /// Parent retained so tests can model ambient ancestors above the worktree safely.
        cleanup_root: PathBuf,
    }

    impl Fixture {
        /// Creates one unique absolute test directory below the system temporary root.
        fn new() -> Self {
            let path = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!(
                    "agent-ide-typescript-profile-{}-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos(),
                    NEXT_FIXTURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
            std::fs::create_dir(&path).unwrap();
            let root = path.join("worktree");
            std::fs::create_dir(&root).unwrap();
            Self {
                root,
                cleanup_root: path,
            }
        }

        /// Writes one file and returns its accepted path, length, and digest identity.
        fn file(&self, name: &str, bytes: &[u8], executable: bool) -> TypeScriptBundleFileV1 {
            let path = self.root.join(name);
            std::fs::write(&path, bytes).unwrap();
            if executable {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            TypeScriptBundleFileV1 {
                path,
                blake3: blake3::hash(bytes),
                bytes: bytes.len() as u64,
            }
        }

        /// Builds the accepted immutable bundle fixture with one explicit closure member.
        fn bundle(&self) -> TypeScriptProviderBundleV1 {
            self.bundle_with_node_script(b"#!/bin/sh\nexit 0\n")
        }

        /// Builds the accepted immutable bundle fixture with a test-controlled direct-child script.
        fn bundle_with_node_script(&self, node_script: &[u8]) -> TypeScriptProviderBundleV1 {
            let node = self.file("node", node_script, true);
            let bridge = self.file("bridge.mjs", b"bridge", false);
            let tsserver = self.file("tsserver.js", b"tsserver", false);
            let closure = self.file("typescript.js", b"typescript", false);
            TypeScriptProviderBundleV1::new(TypeScriptProviderBundleV1Identity {
                node: node.path,
                node_blake3: node.blake3,
                node_version: "24.4.0".into(),
                bridge,
                bridge_version: "6.0.0".into(),
                tsserver,
                typescript_version: "5.9.3".into(),
                closure: vec![closure],
            })
            .unwrap()
        }

        /// Builds the canonical worktree and exact Execution authority for this fixture root.
        fn worktree(&self) -> (WorktreeRef, WorkspaceAuthority) {
            let worktree = WorktreeRef::from_discovery(
                self.root.clone(),
                self.root.clone(),
                self.root.join(".git"),
                1,
            )
            .unwrap();
            let authority = WorkspaceAuthority::from_workspace(
                worktree.id(),
                worktree.incarnation().to_string(),
                self.root.clone(),
                1,
            )
            .unwrap();
            (worktree, authority)
        }

        /// Builds one valid exact profile for the requested TypeScript-family extension.
        fn profile(&self, extension: &str) -> (TypeScriptProfile, TypeScriptWorktree) {
            self.profile_with_node_script(extension, b"#!/bin/sh\nexit 0\n")
        }

        /// Builds a closed profile whose accepted Node executable runs `node_script` directly.
        fn profile_with_node_script(
            &self,
            extension: &str,
            node_script: &[u8],
        ) -> (TypeScriptProfile, TypeScriptWorktree) {
            let bundle = self.bundle_with_node_script(node_script);
            let (worktree, authority) = self.worktree();
            let config = if matches!(extension, "js" | "jsx") {
                JAVASCRIPT_CONFIG
            } else {
                CLOSED_CONFIG
            };
            std::fs::write(self.root.join("tsconfig.json"), config).unwrap();
            let resolution = ProjectResolutionInputsV1::new(
                worktree.clone(),
                self.root.join(format!("fixture.{extension}")),
                &bundle,
                vec![ProjectResolutionFileV1 {
                    path: PathBuf::from("tsconfig.json"),
                    blake3: blake3::hash(config),
                    bytes: config.len() as u64,
                }],
                &|_| true,
            )
            .unwrap();
            let profile =
                TypeScriptProfile::new(bundle, resolution, "test".into(), self.root.join("cache"))
                    .unwrap();
            let worktree = TypeScriptWorktree::new(worktree, authority).unwrap();
            (profile, worktree)
        }
    }

    /// Builds one execution request for a fixture's already accepted TypeScript command.
    fn fixture_request(
        profile: &TypeScriptProfile,
        worktree: &TypeScriptWorktree,
        program: PathBuf,
    ) -> ValidatedExecutionRequest {
        let policy = LocalExecutionPolicy::new(BTreeSet::from([program]), 4096, 16).unwrap();
        ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_verified_binding("typescript-test").unwrap(),
            worktree.authority().clone(),
            profile.command(worktree, &|_| true).unwrap(),
            &policy,
        )
        .unwrap()
    }

    impl Drop for Fixture {
        /// Removes only this test-owned directory after no child can retain its files.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.cleanup_root).unwrap();
        }
    }

    /// Accepts exactly four language IDs and bounded sorted project-resolution inputs.
    #[test]
    fn project_resolution_is_closed_bounded_and_language_exact() {
        for (extension, language) in [
            ("js", "javascript"),
            ("jsx", "javascriptreact"),
            ("ts", "typescript"),
            ("tsx", "typescriptreact"),
        ] {
            let fixture = Fixture::new();
            let bundle = fixture.bundle();
            let (worktree, _) = fixture.worktree();
            let config = if matches!(extension, "js" | "jsx") {
                JAVASCRIPT_CONFIG
            } else {
                CLOSED_CONFIG
            };
            std::fs::write(fixture.root.join("tsconfig.json"), config).unwrap();
            let resolution = ProjectResolutionInputsV1::new(
                worktree,
                fixture.root.join(format!("fixture.{extension}")),
                &bundle,
                vec![ProjectResolutionFileV1 {
                    path: PathBuf::from("tsconfig.json"),
                    blake3: blake3::hash(config),
                    bytes: config.len() as u64,
                }],
                &|_| true,
            )
            .unwrap();
            assert_eq!(resolution.language_id(), language);
        }
        let fixture = Fixture::new();
        let bundle = fixture.bundle();
        let (worktree, _) = fixture.worktree();
        assert!(matches!(
            ProjectResolutionInputsV1::new(
                worktree,
                fixture.root.join("file.md"),
                &bundle,
                vec![],
                &|_| true,
            ),
            Err(TypeScriptProfileError::InvalidResolution)
        ));
    }

    /// Refuses count, aggregate-byte, canonical-order, basename, and ancestor-boundary violations.
    #[test]
    fn project_resolution_rejects_every_declared_bound_violation() {
        let document = Path::new("a/b/file.ts");
        let mut files = [Path::new(""), Path::new("a"), Path::new("a/b")]
            .into_iter()
            .flat_map(|directory| {
                RESOLUTION_FILENAMES
                    .into_iter()
                    .map(move |name| ProjectResolutionFileV1 {
                        path: directory.join(name),
                        blake3: blake3::hash(name.as_bytes()),
                        bytes: 1,
                    })
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.path.cmp(&right.path));
        assert!(validate_resolution_files(document, &files[..17]).is_err());

        let mut oversized = files[..2].to_vec();
        oversized[0].bytes = MAX_RESOLUTION_BYTES;
        assert!(validate_resolution_files(document, &oversized).is_err());

        let mut reordered = files[..2].to_vec();
        reordered.reverse();
        assert!(validate_resolution_files(document, &reordered).is_err());

        let unsupported = [ProjectResolutionFileV1 {
            path: PathBuf::from("a/b/tsconfig.build.json"),
            blake3: blake3::hash(b"{}"),
            bytes: 2,
        }];
        assert!(validate_resolution_files(document, &unsupported).is_err());

        let sibling = [ProjectResolutionFileV1 {
            path: PathBuf::from("other/tsconfig.json"),
            blake3: blake3::hash(b"{}"),
            bytes: 2,
        }];
        assert!(validate_resolution_files(document, &sibling).is_err());
    }

    /// Accepts all exact basenames, then rejects changed, missing, and unsupported inputs.
    #[test]
    fn workspace_observed_resolution_identity_refuses_changes_and_unsupported_shapes() {
        let fixture = Fixture::new();
        let bundle = fixture.bundle();
        let (worktree, _) = fixture.worktree();
        let document = fixture.root.join("src/file.ts");
        std::fs::create_dir(fixture.root.join("src")).unwrap();
        let nested_config =
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10"},"files":["src/file.ts"]}"#;
        std::fs::write(fixture.root.join("tsconfig.json"), nested_config).unwrap();
        let resolution = ProjectResolutionInputsV1::observe(
            worktree.clone(),
            document.clone(),
            &bundle,
            &|_| true,
        )
        .unwrap();
        let repeated = ProjectResolutionInputsV1::observe(
            worktree.clone(),
            document.clone(),
            &bundle,
            &|_| true,
        )
        .unwrap();
        assert_eq!(resolution.files(), repeated.files());
        assert_eq!(resolution.identity, repeated.identity);

        for (name, bytes) in [
            ("jsconfig.json", nested_config.as_slice()),
            ("package-lock.json", b"{}".as_slice()),
            ("package.json", b"{}".as_slice()),
            ("pnpm-lock.yaml", b"lock".as_slice()),
            ("yarn.lock", b"lock".as_slice()),
        ] {
            std::fs::write(fixture.root.join(name), bytes).unwrap();
        }
        let complete = ProjectResolutionInputsV1::observe(
            worktree.clone(),
            document.clone(),
            &bundle,
            &|_| true,
        )
        .unwrap();
        assert_eq!(
            complete
                .files()
                .iter()
                .map(|file| file.path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            RESOLUTION_FILENAMES
        );
        for name in RESOLUTION_FILENAMES {
            if name != "tsconfig.json" {
                std::fs::remove_file(fixture.root.join(name)).unwrap();
            }
        }

        std::fs::write(
            fixture.root.join("tsconfig.json"),
            br#"{"compilerOptions":{"types":["ambient"]}}"#,
        )
        .unwrap();
        assert!(matches!(
            resolution.verify(&|_| true),
            Err(TypeScriptProfileError::InvalidResolution)
        ));

        std::fs::remove_file(fixture.root.join("tsconfig.json")).unwrap();
        assert!(
            ProjectResolutionInputsV1::observe(
                worktree.clone(),
                document.clone(),
                &bundle,
                &|_| true
            )
            .is_err()
        );
        std::fs::write(fixture.root.join("tsconfig.json"), nested_config).unwrap();
        std::fs::write(
            fixture.root.join("package.json"),
            br#"{"dependencies":{"outside":"1"}}"#,
        )
        .unwrap();
        assert!(ProjectResolutionInputsV1::observe(worktree, document, &bundle, &|_| true).is_ok());
    }

    /// Rejects unsupported type discovery, escaping entries, and ambient ancestor dependencies.
    #[test]
    fn project_resolution_rejects_ambient_type_and_dependency_reads() {
        let fixture = Fixture::new();
        let bundle = fixture.bundle();
        let (worktree, _) = fixture.worktree();
        let document = fixture.root.join("src/file.ts");
        std::fs::create_dir(fixture.root.join("src")).unwrap();

        for config in [
            br#"{}"#.as_slice(),
            br#"{"compilerOptions":{"types":["ambient"]}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[]},"include":["../outside"]}"#.as_slice(),
            br#"{"compilerOptions":{"types":[]},"files":["/outside.ts"]}"#.as_slice(),
            br#"{"compilerOptions":{"types":[]},"include":["src\\outside"]}"#.as_slice(),
        ] {
            std::fs::write(fixture.root.join("tsconfig.json"), config).unwrap();
            assert!(
                ProjectResolutionInputsV1::observe(
                    worktree.clone(),
                    document.clone(),
                    &bundle,
                    &|_| true
                )
                .is_err()
            );
        }

        std::fs::write(
            fixture.root.join("tsconfig.json"),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10"},"include":["src/**/*.ts"],"files":["src/file.ts"]}"#,
        )
        .unwrap();
        assert!(
            ProjectResolutionInputsV1::observe(
                worktree.clone(),
                document.clone(),
                &bundle,
                &|_| true
            )
            .is_err()
        );

        std::fs::write(
            fixture.root.join("tsconfig.json"),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10"},"files":["src/file.ts"]}"#,
        )
        .unwrap();
        std::fs::create_dir(fixture.root.join("node_modules")).unwrap();
        assert!(
            ProjectResolutionInputsV1::observe(
                worktree.clone(),
                document.clone(),
                &bundle,
                &|_| true
            )
            .is_ok()
        );
        std::fs::create_dir(fixture.cleanup_root.join("node_modules")).unwrap();
        assert!(
            ProjectResolutionInputsV1::observe(worktree, document, &bundle, &|_| true).is_err()
        );
    }

    /// Accepts only the explicit node10 resolution mode and module combinations it preserves.
    #[test]
    fn project_resolution_accepts_closed_module_resolution() {
        for config in [
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10"},"files":["fixture.ts"]}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"commonjs"},"files":["fixture.ts"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":["vite/client"],"moduleResolution":"bundler","module":"esnext","jsx":"react-jsx","allowImportingTsExtensions":true,"noEmit":true},"include":["src","vite.config.ts","fixture.ts"]}"#.as_slice(),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "typescript",
                    Path::new("fixture.ts"),
                )
                .is_ok()
            );
        }
    }

    /// Refuses implicit, alternate, unknown, and contradictory TypeScript resolution settings.
    #[test]
    fn project_resolution_refuses_open_module_resolution() {
        for config in [
            br#"{"compilerOptions":{"types":[]}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"classic"}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node16"}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"nodenext"}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"future"}}"#.as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"node16"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"nodenext"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"preserve"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"Node16"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"NODENEXT"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","module":"PRESERVE"}}"#
                .as_slice(),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "typescript",
                    Path::new("fixture.ts"),
                )
                .is_err()
            );
        }
    }

    /// Requires exact config membership, JavaScript opt-in, and closed output settings.
    #[test]
    fn project_resolution_requires_exact_membership_and_javascript_opt_in() {
        assert!(
            validate_resolution_shape(
                Path::new("tsconfig.json"),
                CLOSED_CONFIG,
                "typescript",
                Path::new("fixture.ts"),
            )
            .is_ok()
        );
        let mut too_many = vec![serde_json::json!("fixture.js")];
        too_many.extend(
            (0..MAX_RESOLUTION_FILES).map(|index| serde_json::json!(format!("fixture{index}.js"))),
        );
        let too_many_config = serde_json::json!({
            "compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},
            "files":too_many,
        });
        let too_many_bytes = serde_json::to_vec(&too_many_config).unwrap();
        assert!(
            validate_resolution_shape(
                Path::new("tsconfig.json"),
                &too_many_bytes,
                "javascript",
                Path::new("fixture.js"),
            )
            .is_err()
        );
        for config in [
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["fixture.ts"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["fixture.js","fixture.js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["*.js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["/fixture.js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["./fixture.js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["../fixture.js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":["fixture\\js"]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":[7]}"#
                .as_slice(),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "javascript",
                    Path::new("fixture.js"),
                )
                .is_err()
            );
        }
        for config in [
            CLOSED_CONFIG,
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":false}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":"true"}}"#
                .as_slice(),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "javascript",
                    Path::new("fixture.js"),
                )
                .is_err()
            );
        }
        assert!(
            validate_resolution_shape(
                Path::new("tsconfig.json"),
                JAVASCRIPT_CONFIG,
                "javascriptreact",
                Path::new("fixture.js"),
            )
            .is_ok()
        );
        for config in [
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"include":[]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"files":[]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true},"exclude":[]}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true,"outDir":"dist"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true,"declarationDir":"types"}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true,"checkJs":false}}"#
                .as_slice(),
            br#"{"compilerOptions":{"types":[],"moduleResolution":"node10","allowJs":true,"noImplicitAny":false}}"#
                .as_slice(),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "javascript",
                    Path::new("fixture.js"),
                )
                .is_err()
            );
        }
    }

    /// Admits literal include directories recursively without admitting prefix siblings.
    #[test]
    fn project_resolution_accepts_literal_include_directories() {
        let config = br#"{"compilerOptions":{"types":["vite/client"],"moduleResolution":"Bundler","module":"esnext","jsx":"react-jsx","allowImportingTsExtensions":true,"noEmit":true},"include":["src","vite.config.ts"]}"#;
        for (document, language) in [
            ("src/api.ts", "typescript"),
            ("src/App.tsx", "typescriptreact"),
            ("vite.config.ts", "typescript"),
        ] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    language,
                    Path::new(document),
                )
                .is_ok(),
                "{document} should be admitted by a literal project include"
            );
        }
        for document in ["outside.ts", "src2/api.ts"] {
            assert!(
                validate_resolution_shape(
                    Path::new("tsconfig.json"),
                    config,
                    "typescript",
                    Path::new(document),
                )
                .is_err()
            );
        }
        let rejection = validate_resolution_shape(
            Path::new("tsconfig.json"),
            config,
            "typescript",
            Path::new("outside.ts"),
        )
        .unwrap_err();
        assert_eq!(
            rejection.to_string(),
            "tsconfig.json: outside.ts is not listed in `files` or under a literal `include` entry"
        );
        let missing = validate_resolution_files(Path::new("src/api.ts"), &[]).unwrap_err();
        assert_eq!(
            missing.to_string(),
            "no tsconfig.json or jsconfig.json was found from src up to the worktree root"
        );
    }

    /// Rechecks every declared bundle member and refuses a changed TypeScript closure file.
    #[test]
    fn bundle_remeasurement_rejects_changed_closure() {
        let fixture = Fixture::new();
        let bundle = fixture.bundle();
        std::fs::write(fixture.root.join("typescript.js"), b"changed").unwrap();
        assert!(matches!(
            bundle.verify(),
            Err(TypeScriptProfileError::InvalidBundle)
        ));
    }

    /// Builds only the fixed Node/bridge stdio command and accepted initialization settings.
    #[test]
    fn profile_command_and_settings_are_closed() {
        let fixture = Fixture::new();
        let (profile, worktree) = fixture.profile("tsx");
        let command = profile.command(&worktree, &|_| true).unwrap();
        assert!(command.has_program_digest(&profile.bundle.identity.node_blake3));
        assert_eq!(
            profile.initialization_options(),
            serde_json::json!({
                "disableAutomaticTypingAcquisition":true,"plugins":[],"preferences":{"includePackageJsonAutoImports":"off"},
                "tsserver":{"path":profile.bundle.tsserver().display().to_string(),"useSyntaxServer":"never","logVerbosity":"off","trace":"off"}
            })
        );
        let process = command.inherited_process().unwrap();
        assert!(
            process
                .as_std()
                .get_envs()
                .all(|(name, _)| name != std::ffi::OsStr::new("PATH")),
            "the exact Node path needs no ambient executable search directory"
        );
        // T38B: the bridge creates directories under `TMPDIR` before its first protocol byte, so
        // the private temp must live under the host temp directory — which host command sandboxes
        // admit — named by the opaque cache namespace id, never inside the daemon runtime.
        let temp = std::env::temp_dir()
            .join(fixture.root.join("cache").file_name().unwrap())
            .join("tmp");
        let redirected = process
            .as_std()
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new("TMPDIR"))
            .and_then(|(_, value)| value)
            .map(std::path::PathBuf::from)
            .unwrap();
        assert_eq!(redirected, temp, "private temp is host-temp namespaced");
        assert!(
            redirected.is_dir(),
            "the private temp is prepared before the bridge starts"
        );
        assert!(
            !redirected.starts_with(fixture.root.join("cache")),
            "the daemon runtime directory is not writable under host command sandboxes"
        );
    }

    /// A real nonzero TypeScript bridge wait quarantines only its exact profile after reap.
    #[tokio::test]
    async fn nonzero_bridge_wait_quarantines_the_exact_profile() {
        let fixture = Fixture::new();
        let (profile, worktree) = fixture.profile_with_node_script("ts", b"#!/bin/sh\nexit 1\n");
        let request = fixture_request(&profile, &worktree, fixture.root.join("node"));
        let mut profiles = TypeScriptProfiles::default();
        let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
            total_views: 2,
            per_backend_views: 1,
        })
        .unwrap();
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 2,
            per_owner_running: 2,
            per_owner_queued: 2,
            total_queued: 2,
            interactive_burst: 1,
        })
        .unwrap();
        let TypeScriptViewAdmission::Granted(view) = profiles.request(
            &profile,
            &worktree,
            &mut registry,
            &mut admission,
            OwnerId::new("typescript").unwrap(),
            AdmissionClass::Interactive,
        ) else {
            panic!("first exact profile must be admitted")
        };
        let mut child = TypeScriptProtocolChild::spawn(
            &request,
            &profile,
            &worktree,
            &mut registry,
            &mut admission,
            view.lease(),
            None,
            64,
            &|_| true,
        )
        .unwrap();
        let waited = child.wait_for_exit(Duration::from_secs(1)).await.unwrap();
        assert!(
            !waited.success(),
            "the fixture bridge must take the !success branch"
        );
        assert!(profiles.quarantine_after_unsuccessful_wait(&view, &waited));
        let reaped = child
            .finish_reap(waited, Duration::from_secs(1))
            .await
            .unwrap();
        let capability = profiles.release(view, &mut registry).unwrap();
        registry
            .complete_reap(&mut admission, capability, reaped.proof)
            .unwrap();
        assert!(matches!(
            profiles.request(
                &profile,
                &worktree,
                &mut registry,
                &mut admission,
                OwnerId::new("typescript").unwrap(),
                AdmissionClass::Interactive,
            ),
            TypeScriptViewAdmission::Unavailable(TypeScriptProfileError::Quarantined)
        ));
    }

    /// A client teardown timeout does not quarantine a live TypeScript profile.
    #[tokio::test]
    async fn client_teardown_wait_timeout_does_not_quarantine_the_profile() {
        let fixture = Fixture::new();
        let (profile, worktree) = fixture.profile_with_node_script("ts", b"#!/bin/sh\nsleep 10\n");
        let request = fixture_request(&profile, &worktree, fixture.root.join("node"));
        let mut profiles = TypeScriptProfiles::default();
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
        let TypeScriptViewAdmission::Granted(view) = profiles.request(
            &profile,
            &worktree,
            &mut registry,
            &mut admission,
            OwnerId::new("wait-timeout").unwrap(),
            AdmissionClass::Interactive,
        ) else {
            panic!("initial exact profile must be admitted")
        };
        let mut child = TypeScriptProtocolChild::spawn(
            &request,
            &profile,
            &worktree,
            &mut registry,
            &mut admission,
            view.lease(),
            None,
            64,
            &|_| true,
        )
        .unwrap();
        assert!(
            child
                .wait_for_exit(Duration::from_millis(10))
                .await
                .is_err(),
            "the fixture bridge must take the wait-timeout branch"
        );
        let reaped = child
            .terminate_abnormally(Duration::from_millis(10), Duration::from_secs(1))
            .await
            .unwrap();
        let capability = profiles.release(view, &mut registry).unwrap();
        registry
            .complete_reap(&mut admission, capability, reaped.proof)
            .unwrap();
        let TypeScriptViewAdmission::Granted(retry) = profiles.request(
            &profile,
            &worktree,
            &mut registry,
            &mut admission,
            OwnerId::new("wait-timeout-retry").unwrap(),
            AdmissionClass::Interactive,
        ) else {
            panic!("teardown timeout must leave this exact project admissible")
        };
        registry
            .cancel_unstarted(&mut admission, retry.lease())
            .unwrap();
    }
}
