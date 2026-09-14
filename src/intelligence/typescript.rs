//! Exclusive TypeScript profile, bounded project-resolution evidence, and owned stdio lifecycle.
//!
//! The profile admits one immutable Node/bridge/TypeScript closure for one worktree incarnation.
//! It does not discover packages, plugins, configuration, or executables from ambient state.

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
        ReapedProtocolProcess, ValidatedExecutionRequest, WorkspaceAuthority,
    },
    workspace::authority::WorktreeRef,
};

/// Maximum immutable closure members beyond the separately identified bridge and `tsserver.js`.
const MAX_BUNDLE_CLOSURE_FILES: usize = 64;
/// Maximum bytes accepted for any one bridge or TypeScript closure file.
const MAX_BUNDLE_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum project-resolution inputs retained for one document.
const MAX_RESOLUTION_FILES: usize = 16;
/// Maximum bytes represented by all project-resolution inputs for one document.
const MAX_RESOLUTION_BYTES: u64 = 8 * 1024 * 1024;

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
    /// Validates an already-observed canonical input set without searching the worktree.
    ///
    /// `document` must be an absolute `.js`, `.jsx`, `.ts`, or `.tsx` path below `worktree`.
    /// `files` may contain at most 16 strictly sorted, unique normal relative paths whose basenames
    /// are one of the supported TypeScript/JavaScript project or package inputs. Their declared
    /// sizes may total at most 8 MiB. Empty input sets represent a deliberately observed inferred
    /// project; callers must return `resolution_unverified` if an input the project needs was not
    /// observed. The constructor reads no path and returns `InvalidResolution` on any mismatch.
    pub fn new(
        worktree: WorktreeRef,
        document: PathBuf,
        bundle: &TypeScriptProviderBundleV1,
        files: Vec<ProjectResolutionFileV1>,
    ) -> Result<Self, TypeScriptProfileError> {
        let relative = document
            .strip_prefix(worktree.worktree_path())
            .ok()
            .filter(|path| normal_relative(path))
            .ok_or(TypeScriptProfileError::InvalidResolution)?
            .to_path_buf();
        let language_id =
            typescript_language_id(&relative).ok_or(TypeScriptProfileError::InvalidResolution)?;
        if files.len() > MAX_RESOLUTION_FILES {
            return Err(TypeScriptProfileError::InvalidResolution);
        }
        let mut total = 0_u64;
        let mut previous: Option<&Path> = None;
        for file in &files {
            total = total
                .checked_add(file.bytes)
                .ok_or(TypeScriptProfileError::InvalidResolution)?;
            if !normal_relative(&file.path)
                || !supported_resolution_file(&file.path)
                || file.bytes > MAX_RESOLUTION_BYTES
                || total > MAX_RESOLUTION_BYTES
                || previous.is_some_and(|path| path >= file.path.as_path())
            {
                return Err(TypeScriptProfileError::InvalidResolution);
            }
            previous = Some(&file.path);
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

    /// Builds fixed `node <bridge> --stdio` with only the accepted Node directory and private temp.
    pub fn command(
        &self,
        worktree: &TypeScriptWorktree,
    ) -> Result<ControlledCommand, TypeScriptProfileError> {
        if self.resolution.worktree() != worktree.worktree() {
            return Err(TypeScriptProfileError::WorktreeMismatch);
        }
        self.bundle.verify()?;
        let node_parent = self
            .bundle
            .node()
            .parent()
            .ok_or(TypeScriptProfileError::InvalidProfile)?;
        let path = std::env::join_paths([node_parent])
            .map_err(|_| TypeScriptProfileError::InvalidProfile)?;
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            self.bundle.node().to_path_buf(),
            vec![
                self.bundle.bridge().as_os_str().to_owned(),
                OsString::from("--stdio"),
            ],
            worktree.worktree().worktree_path().to_path_buf(),
            BTreeMap::from([
                (OsString::from("PATH"), path),
                (
                    OsString::from("TMPDIR"),
                    self.cache_namespace.join("tmp").into_os_string(),
                ),
            ]),
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

    /// Returns the fixed TypeScript Language Server initialization options.
    pub fn initialization_options(&self) -> serde_json::Value {
        serde_json::json!({
            "disableAutomaticTypingAcquisition": true,
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
    /// Remeasures the bundle, then starts one authority-bound bridge from the view capability.
    ///
    /// A bundle mismatch cancels the unstarted view through `registry` and `admission`. Authority or
    /// spawn failures return the linear no-child/process evidence from Execution without inventing
    /// descendant settlement.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        request: &ValidatedExecutionRequest,
        profile: &TypeScriptProfile,
        worktree: &TypeScriptWorktree,
        registry: &mut ProviderLeaseRegistry,
        admission: &mut AdmissionController,
        view: ProviderViewLease,
        active_use: Option<ActiveBindingUse>,
        codex_executable: &Path,
        output_cap: usize,
    ) -> Result<Self, TypeScriptProfileError> {
        if profile.verify_bundle().is_err() {
            registry
                .cancel_unstarted(admission, view)
                .map_err(TypeScriptProfileError::Execution)?;
            return Err(TypeScriptProfileError::InvalidBundle);
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
        OwnedProtocolChild::spawn_from_provider_lease(
            request,
            capability,
            active_use,
            codex_executable,
            output_cap,
        )
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
        (&mut self.child.stdout, &mut self.child.stdin)
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

    /// Runs the fixed group TERM/grace/group-and-direct KILL/direct-reap abnormal sequence.
    pub async fn terminate_abnormally(
        self,
        grace: Duration,
        deadline: Duration,
    ) -> Result<ReapedProtocolProcess, TypeScriptProfileError> {
        self.child
            .terminate_typescript_abnormally(grace, deadline)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{AdmissionLimits, ProviderLeaseLimits};
    use std::os::unix::fs::PermissionsExt;

    /// Owns one unique directory and removes its files after each profile test.
    struct Fixture {
        /// Unique test-owned root containing every bundle and cache fixture.
        root: PathBuf,
    }

    impl Fixture {
        /// Creates one unique absolute test directory below the system temporary root.
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "agent-ide-typescript-profile-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self { root: path }
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
            let node = self.file("node", b"#!/bin/sh\nexit 0\n", true);
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
            let bundle = self.bundle();
            let (worktree, authority) = self.worktree();
            let resolution = ProjectResolutionInputsV1::new(
                worktree.clone(),
                self.root.join(format!("fixture.{extension}")),
                &bundle,
                vec![],
            )
            .unwrap();
            let profile =
                TypeScriptProfile::new(bundle, resolution, "test".into(), self.root.join("cache"))
                    .unwrap();
            let worktree = TypeScriptWorktree::new(worktree, authority).unwrap();
            (profile, worktree)
        }
    }

    impl Drop for Fixture {
        /// Removes only this test-owned directory after no child can retain its files.
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).unwrap();
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
            let resolution = ProjectResolutionInputsV1::new(
                worktree,
                fixture.root.join(format!("src/file.{extension}")),
                &bundle,
                vec![ProjectResolutionFileV1 {
                    path: PathBuf::from("tsconfig.json"),
                    blake3: blake3::hash(b"{}"),
                    bytes: 2,
                }],
            )
            .unwrap();
            assert_eq!(resolution.language_id(), language);
        }
        let fixture = Fixture::new();
        let bundle = fixture.bundle();
        let (worktree, _) = fixture.worktree();
        assert!(matches!(
            ProjectResolutionInputsV1::new(worktree, fixture.root.join("file.md"), &bundle, vec![]),
            Err(TypeScriptProfileError::InvalidResolution)
        ));
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
        let command = profile.command(&worktree).unwrap();
        assert!(command.has_program_digest(&profile.bundle.identity.node_blake3));
        assert_eq!(
            profile.initialization_options(),
            serde_json::json!({
                "disableAutomaticTypingAcquisition":true,
                "tsserver":{"path":profile.bundle.tsserver().display().to_string(),"useSyntaxServer":"never","logVerbosity":"off","trace":"off"}
            })
        );
    }

    /// Quarantine prevents exact-profile reuse while leaving a different worktree profile eligible.
    #[test]
    fn exact_profile_quarantine_lasts_for_owner_lifetime() {
        let fixture = Fixture::new();
        let (profile, worktree) = fixture.profile("ts");
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
        profiles.quarantine(&view);
        registry
            .cancel_unstarted(&mut admission, view.lease())
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
}
