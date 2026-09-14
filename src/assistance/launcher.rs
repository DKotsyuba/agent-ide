//! Restart-only trusted launcher configuration, separate from host metadata and model arguments.

use super::claude_worker::ClaudeOperatorProfile;
use crate::execution::{ExecutionProfileCatalog, HostSandboxState, PersistedProfileRecord};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
};

/// Maximum complete launcher file; limits memory before JSON decoding.
const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Maximum executable bytes hashed during a pre-spawn identity check.
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
/// Compiled Codex release record accepted for the exact TypeScript r2 macOS bundle cell.
const TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1: &str =
    "macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r2-2026-09-14";

/// Fixed launcher failure categories; no paths, attachments or accepted evidence are rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherError {
    /// File cannot be read under its byte bound or JSON is not the supported closed schema.
    Invalid,
    /// An attachment, path, identity, profile record or limit is absent, ambiguous or unsupported.
    Rejected,
    /// A configured executable no longer has its accepted content fingerprint.
    ExecutableChanged,
    /// Daemon shutdown cancelled bounded startup verification.
    Cancelled,
}

/// An operator-accepted executable path and immutable measured identity.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedExecutable {
    /// Absolute normalized executable selected by the trusted launcher, never a model parameter.
    pub path: PathBuf,
    /// Nonempty accepted binary/version identity matched to Execution profile evidence.
    pub identity: String,
    /// BLAKE3 digest checked before worker readiness; executable bytes must remain immutable until restart.
    pub blake3: String,
}
impl AcceptedExecutable {
    /// Rejects malformed paths/identities without opening or launching the executable.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.path)
            || !identifier(&self.identity)
            || self.blake3.len() != 64
            || !self.blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }
    /// Checks bounded current executable contents without starting a process.
    pub fn verify(&self) -> Result<(), LauncherError> {
        self.verify_cancellable(&std::sync::atomic::AtomicBool::new(false))
    }
    /// Verifies one regular bounded executable, checking daemon cancellation between read chunks.
    pub(crate) fn verify_cancellable(
        &self,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), LauncherError> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(LauncherError::Cancelled);
        }
        let metadata =
            std::fs::metadata(&self.path).map_err(|_| LauncherError::ExecutableChanged)?;
        if !metadata.is_file() || metadata.len() > MAX_EXECUTABLE_BYTES {
            return Err(LauncherError::ExecutableChanged);
        }
        let mut file = File::open(&self.path)
            .map_err(|_| LauncherError::ExecutableChanged)?
            .take(MAX_EXECUTABLE_BYTES + 1);
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 32 * 1024];
        let mut total = 0;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(LauncherError::Cancelled);
            }
            let count = file
                .read(&mut buffer)
                .map_err(|_| LauncherError::ExecutableChanged)?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_EXECUTABLE_BYTES {
                return Err(LauncherError::ExecutableChanged);
            }
            hash.update(&buffer[..count]);
        }
        if !hash
            .finalize()
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(&self.blake3)
        {
            return Err(LauncherError::ExecutableChanged);
        }
        Ok(())
    }
    /// Builds one accepted executable identity by measuring `path`'s current regular-file bytes.
    ///
    /// Reuses the same content digest `crate::execution::measured_executable_digest` computes
    /// before a controlled child spawns, so the emitted `path`/`identity`/`blake3` triple is
    /// byte-identical to what [`Self::verify`] later accepts for the same file. `path` must be
    /// absolute; `identity` must be a nonempty bounded identifier. Returns
    /// [`LauncherError::Rejected`] for a malformed path or identity, or
    /// [`LauncherError::ExecutableChanged`] when `path` cannot be opened, hashed, or is not a
    /// regular executable file.
    pub fn from_path(path: PathBuf, identity: impl Into<String>) -> Result<Self, LauncherError> {
        let digest = crate::execution::measured_executable_digest(&path)
            .map_err(|_| LauncherError::ExecutableChanged)?;
        let executable = Self {
            path,
            identity: identity.into(),
            blake3: digest.to_hex().to_string(),
        };
        executable.validate()?;
        Ok(executable)
    }
}

/// Closed effective provider configuration; arbitrary settings JSON is never accepted.
#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedProviderSettings {
    /// Accepted gopls defaults on a separately owned logical view.
    GoplsDefaults,
    /// Accepted Rust profile with cache priming disabled and server-status synchronization.
    RustCachePrimingDisabledV1,
    /// Accepted Pyright defaults over one exclusive, worktree-isolated stdio child.
    PyrightDefaultsV1,
    /// Accepted release-pinned TypeScript bundle over one exclusive stdio bridge child.
    #[serde(rename = "typescript_defaults_v1")]
    TypeScriptDefaultsV1,
}

/// One restart-configured regular file in the immutable TypeScript runtime closure.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTypeScriptFileV1 {
    /// Absolute normalized file path selected only by trusted launcher configuration.
    pub path: PathBuf,
    /// Complete hexadecimal BLAKE3 digest of the accepted bytes.
    pub blake3: String,
    /// Exact accepted byte length, bounded and rechecked with the digest.
    pub bytes: u64,
}

impl AcceptedTypeScriptFileV1 {
    /// Rejects malformed paths, digests, and files above the TypeScript bundle member ceiling.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.path)
            || self.blake3.len() != 64
            || !self.blake3.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.bytes > 64 * 1024 * 1024
        {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }

    /// Converts the validated launcher identity into Intelligence's immutable bundle member.
    fn bundle_file(
        &self,
    ) -> Result<crate::intelligence::typescript::TypeScriptBundleFileV1, LauncherError> {
        Ok(crate::intelligence::typescript::TypeScriptBundleFileV1 {
            path: self.path.clone(),
            blake3: blake3::Hash::from_hex(&self.blake3).map_err(|_| LauncherError::Rejected)?,
            bytes: self.bytes,
        })
    }
}

/// Closed TypeScript-specific portion of one fourth launcher provider declaration.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedTypeScriptBundleV1 {
    /// Exact byte length of `ProviderLaunch::executable`, the bridge entry module.
    pub bridge_bytes: u64,
    /// Exact accepted TypeScript Language Server release identity.
    pub bridge_version: String,
    /// Explicit accepted `tsserver.js` entry module.
    pub tsserver: AcceptedTypeScriptFileV1,
    /// Exact accepted TypeScript release identity.
    pub typescript_version: String,
    /// Strictly sorted complete loaded runtime closure excluding bridge and `tsserver.js`.
    pub closure: Vec<AcceptedTypeScriptFileV1>,
    /// Exact compiled Codex macOS release record required to enable this provider for Codex.
    pub codex_macos_evidence: String,
    /// Optional separate Claude macOS record; no value is accepted until that cell passes.
    pub claude_macos_evidence: Option<String>,
}

impl AcceptedTypeScriptBundleV1 {
    /// Validates the bounded closed declaration without granting either host execution authority.
    fn validate(&self) -> Result<(), LauncherError> {
        if self.bridge_bytes > 64 * 1024 * 1024
            || !identifier(&self.bridge_version)
            || !identifier(&self.typescript_version)
            || self.closure.is_empty()
            || self.closure.len() > 64
            || self.codex_macos_evidence != TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1
            || self.claude_macos_evidence.is_some()
        {
            return Err(LauncherError::Rejected);
        }
        self.tsserver.validate()?;
        let mut previous: Option<&Path> = None;
        for file in &self.closure {
            file.validate()?;
            if previous.is_some_and(|path| path >= file.path.as_path())
                || file.path == self.tsserver.path
            {
                return Err(LauncherError::Rejected);
            }
            previous = Some(&file.path);
        }
        Ok(())
    }
}

/// Trusted provider identity; populated only by the restart-loaded launcher file.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLaunch {
    /// Absolute executable and measured binary fingerprint.
    pub executable: AcceptedExecutable,
    /// Closed settings contract whose exact identifier participates in compatibility checks.
    pub settings: AcceptedProviderSettings,
    /// Accepted toolchain identity; gopls and Pyright use absolute executables, Rust a rustup selector.
    pub toolchain: String,
    /// Absolute operator-declared Node executable for Pyright; absent for Go and Rust. Its measured
    /// identity must match `toolchain`, and it is the only program permitted to start Pyright.
    pub node: Option<AcceptedExecutable>,
    /// Immutable TypeScript closure and host-specific release records; present only for TypeScript.
    pub typescript: Option<AcceptedTypeScriptBundleV1>,
    /// Absolute operator-declared `cargo` executable for Rust; absent for gopls. Never chosen by
    /// model or project input; its measured identity must match `cargo_version`.
    pub cargo: Option<AcceptedExecutable>,
    /// Accepted Cargo identity for Rust; absent for gopls.
    pub cargo_version: Option<String>,
    /// Absolute operator-declared `rustc` executable for Rust; absent for gopls. Never chosen by
    /// model or project input; its measured identity must match `rustc_version`.
    pub rustc: Option<AcceptedExecutable>,
    /// Accepted rustc identity for Rust; absent for gopls.
    pub rustc_version: Option<String>,
    /// Explicit operator trust identity, never derived from a sandbox observation.
    pub trust: String,
    /// Persistent compatible cache namespace, retained after stopping a view.
    pub cache_namespace: String,
}

impl ProviderLaunch {
    /// Reconstructs the exact immutable TypeScript bundle from this validated launcher provider.
    pub fn typescript_bundle(
        &self,
    ) -> Result<crate::intelligence::typescript::TypeScriptProviderBundleV1, LauncherError> {
        let configured = self.typescript.as_ref().ok_or(LauncherError::Rejected)?;
        let node = self.node.as_ref().ok_or(LauncherError::Rejected)?;
        crate::intelligence::typescript::TypeScriptProviderBundleV1::new(
            crate::intelligence::typescript::TypeScriptProviderBundleV1Identity {
                node: node.path.clone(),
                node_blake3: blake3::Hash::from_hex(&node.blake3)
                    .map_err(|_| LauncherError::Rejected)?,
                node_version: node.identity.clone(),
                bridge: crate::intelligence::typescript::TypeScriptBundleFileV1 {
                    path: self.executable.path.clone(),
                    blake3: blake3::Hash::from_hex(&self.executable.blake3)
                        .map_err(|_| LauncherError::Rejected)?,
                    bytes: configured.bridge_bytes,
                },
                bridge_version: configured.bridge_version.clone(),
                tsserver: configured.tsserver.bundle_file()?,
                typescript_version: configured.typescript_version.clone(),
                closure: configured
                    .closure
                    .iter()
                    .map(AcceptedTypeScriptFileV1::bundle_file)
                    .collect::<Result<Vec<_>, _>>()?,
            },
        )
        .map_err(|_| LauncherError::ExecutableChanged)
    }

    /// Returns whether the compiled Codex release record accepts this exact provider declaration.
    pub fn typescript_codex_accepted(&self) -> bool {
        self.settings == AcceptedProviderSettings::TypeScriptDefaultsV1
            && self.typescript.as_ref().is_some_and(|bundle| {
                bundle.codex_macos_evidence == TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1
            })
    }

    /// Returns whether a separately compiled Claude release record accepts this exact declaration.
    ///
    /// No Claude TypeScript record is accepted in this release; the wired helper path therefore
    /// remains unavailable instead of inheriting the Codex record.
    pub const fn typescript_claude_accepted(&self) -> bool {
        false
    }
}

/// Finite worker limits supplied once at daemon launch.
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProductLimits {
    /// Maximum queued operations, 1..=64.
    pub queued: usize,
    /// Maximum retained operation/detail results, 1..=128.
    pub details: usize,
    /// Total lifetime of one asynchronous operation, 1..=300000 milliseconds.
    pub operation_ms: u64,
    /// Maximum captured stdout/stderr bytes per stream, 1..=1 MiB.
    pub output_bytes: usize,
}
impl ProductLimits {
    /// Checks positive finite ceilings before any worker, store or child can start.
    fn validate(&self) -> Result<(), LauncherError> {
        if !(1..=64).contains(&self.queued)
            || !(1..=128).contains(&self.details)
            || !(1..=300_000).contains(&self.operation_ms)
            || !(1..=1024 * 1024).contains(&self.output_bytes)
        {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }
}

/// Trusted raw Execution evidence loaded from the launcher, never from an invocation or stored reply.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptedProfile {
    /// Execution-owned serialized accepted profile record.
    record: Value,
    /// Exact state captured for that accepted record; current invocation state is checked separately.
    sandbox_state: Value,
}

/// One raw attachment mapping; a vector permits detecting duplicate attachments instead of overwriting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    /// Opaque host channel/session attachment supplied only in trusted launch environments.
    attachment: String,
    /// Absolute candidate worktree path; discovery must still prove its native Git identity.
    candidate: PathBuf,
    /// Accepted Git executable for fixed Workspace discovery and read-only queries.
    git: AcceptedExecutable,
    /// Accepted Codex wrapper executable used for managed sandbox replay.
    codex: AcceptedExecutable,
    /// Operator-declared `/usr/bin/env` trampoline; absent leaves a differing worktree unavailable.
    ///
    /// Present only to run a validated command in this target's worktree when the managed host's
    /// own `sandboxCwd` is an inherited parent directory. Its path must be exactly `/usr/bin/env`;
    /// an arbitrary script is rejected, and the field is never inferred from a host observation.
    #[serde(default)]
    cwd_trampoline: Option<AcceptedExecutable>,
    /// At most the two current language profiles; duplicate settings/languages are rejected.
    providers: Vec<ProviderLaunch>,
    /// Trusted Execution records and exact evidence states, limited to two supported profile classes.
    profiles: Vec<AcceptedProfile>,
    /// Explicit policy acceptance for an observed disabled host; false never weakens sandboxing.
    allow_disabled_host: bool,
    /// Operator-declared strict Claude configuration; absent leaves Claude execution unavailable.
    ///
    /// Codex targets omit this field entirely and keep their existing behaviour and configuration
    /// unchanged. It is never inferred from a host observation, a permission mode or process state.
    #[serde(default)]
    claude_profile: Option<ClaudeOperatorProfile>,
}

/// Decodes only the versioned, closed launcher schema.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    /// Only version 1 is accepted; reloading requires restarting the daemon.
    version: u32,
    /// Shared finite worker limits for all configured attachment mappings.
    limits: ProductLimits,
    /// Bounded attachment mappings, with at most 64 distinct targets.
    targets: Vec<RawTarget>,
}

/// One validated trusted target; metadata alone never creates one of these values.
#[derive(Clone)]
pub struct LaunchTarget {
    /// Raw absolute candidate for controlled Git discovery; not itself Workspace authority.
    pub candidate: PathBuf,
    /// Trusted accepted Git program identity.
    pub git: AcceptedExecutable,
    /// Trusted accepted Codex sandbox wrapper identity.
    pub codex: AcceptedExecutable,
    /// Validated `/usr/bin/env` trampoline; `None` keeps a differing sandbox cwd unavailable.
    ///
    /// Presence is required before a command may run in this target's worktree while the managed
    /// host reports a different inherited `sandboxCwd`. It widens no sandbox policy and is never a
    /// fallback to unrestricted execution.
    pub cwd_trampoline: Option<AcceptedExecutable>,
    /// Accepted closed provider profiles for this candidate.
    pub providers: Vec<ProviderLaunch>,
    /// Execution-minted catalog reconstructed only from trusted matching profile evidence.
    pub catalog: ExecutionProfileCatalog,
    /// Explicit trusted policy for disabled host observations.
    pub allow_disabled_host: bool,
    /// Validated strict Claude operator profile; `None` keeps Claude execution unavailable.
    ///
    /// Presence is required before any Claude helper may be minted for this target. Its absence is
    /// never a fallback to unrestricted execution and never affects the Codex path on this target.
    pub claude_profile: Option<ClaudeOperatorProfile>,
}

/// Immutable attachment map owned by one daemon generation; Debug always redacts its contents.
#[derive(Clone)]
pub struct LauncherConfig {
    /// Private attachment-to-target map, never populated from model args, cwd, PID or timing.
    targets: BTreeMap<String, LaunchTarget>,
    /// Shared validated bounds for the daemon's single worker.
    pub limits: ProductLimits,
}
impl std::fmt::Debug for LauncherConfig {
    /// Emits no attachment, path, accepted profile or executable identity.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LauncherConfig(..)")
    }
}
impl LauncherConfig {
    /// Loads one bounded closed file at daemon startup; runtime mutation is intentionally unsupported.
    pub fn read(path: &Path) -> Result<Self, LauncherError> {
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| {
                file.take((MAX_CONFIG_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| LauncherError::Invalid)?;
        Self::parse(&bytes)
    }
    /// Clones one trusted single-target template onto a fresh attachment and host-selected candidate.
    ///
    /// `path` is read under the normal launcher byte bound. The template must already satisfy the
    /// complete launcher schema and contain exactly one target; only that target's `attachment` and
    /// `candidate` values are replaced. `attachment` is the private process-generated channel and
    /// `candidate` is the absolute local directory captured by managed MCP startup. The returned
    /// bytes are the exact validated configuration suitable for the owned daemon subprocess.
    /// Multi-target parsing and legacy launcher loading remain unchanged. Returns `Rejected` for a
    /// non-single-target template or malformed replacement and `Invalid` for unreadable/invalid JSON.
    pub fn bind_one_candidate(
        path: &Path,
        attachment: &str,
        candidate: &Path,
    ) -> Result<(Self, Vec<u8>), LauncherError> {
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| {
                file.take((MAX_CONFIG_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| LauncherError::Invalid)?;
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(LauncherError::Invalid);
        }
        let mut template: Value =
            serde_json::from_slice(&bytes).map_err(|_| LauncherError::Invalid)?;
        let targets = template
            .as_object_mut()
            .and_then(|object| object.get_mut("targets"))
            .and_then(Value::as_array_mut)
            .ok_or(LauncherError::Invalid)?;
        let [target] = targets.as_mut_slice() else {
            return Err(LauncherError::Rejected);
        };
        let target = target.as_object_mut().ok_or(LauncherError::Invalid)?;
        if !target.contains_key("attachment") || !target.contains_key("candidate") {
            return Err(LauncherError::Rejected);
        }
        target.insert("attachment".into(), Value::String(attachment.to_owned()));
        target.insert(
            "candidate".into(),
            Value::String(
                candidate
                    .to_str()
                    .ok_or(LauncherError::Rejected)?
                    .to_owned(),
            ),
        );
        let bytes = serde_json::to_vec(&template).map_err(|_| LauncherError::Invalid)?;
        let launcher = Self::parse(&bytes)?;
        Ok((launcher, bytes))
    }
    /// Validates bounded trusted JSON, including the ceiling of three provider languages per target,
    /// without host inference, network access, or process effects.
    pub fn parse(bytes: &[u8]) -> Result<Self, LauncherError> {
        if bytes.len() > MAX_CONFIG_BYTES {
            return Err(LauncherError::Invalid);
        }
        let raw: RawConfig = serde_json::from_slice(bytes).map_err(|_| LauncherError::Invalid)?;
        raw.limits.validate()?;
        if raw.version != 1 || raw.targets.len() > 64 {
            return Err(LauncherError::Rejected);
        }
        let mut targets = BTreeMap::new();
        for target in raw.targets {
            if !identifier(&target.attachment)
                || target.attachment.len() > 128
                || !absolute(&target.candidate)
                || target.profiles.is_empty()
                || target.profiles.len() > 2
                || target.providers.len() > 4
            {
                return Err(LauncherError::Rejected);
            }
            target.git.validate()?;
            target.codex.validate()?;
            // The trampoline contract accepts exactly one program: a declaration naming any other
            // path is rejected outright rather than accepted as an arbitrary wrapper script.
            if let Some(trampoline) = &target.cwd_trampoline {
                trampoline.validate()?;
                if trampoline.path != Path::new("/usr/bin/env") {
                    return Err(LauncherError::Rejected);
                }
            }
            let mut provider_kinds = Vec::new();
            for provider in &target.providers {
                provider.executable.validate()?;
                if provider_kinds.contains(&provider.settings)
                    || !identifier(&provider.toolchain)
                    || !identifier(&provider.trust)
                    || !identifier(&provider.cache_namespace)
                {
                    return Err(LauncherError::Rejected);
                }
                provider_kinds.push(provider.settings);
                match provider.settings {
                    AcceptedProviderSettings::GoplsDefaults
                        if !absolute(Path::new(&provider.toolchain))
                            || provider.node.is_some()
                            || provider.typescript.is_some()
                            || provider.cargo.is_some()
                            || provider.cargo_version.is_some()
                            || provider.rustc.is_some()
                            || provider.rustc_version.is_some() =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    AcceptedProviderSettings::PyrightDefaultsV1
                        if !provider.node.as_ref().is_some_and(|node| {
                            node.validate().is_ok() && provider.toolchain == node.identity
                        }) || provider.typescript.is_some()
                            || provider.cargo.is_some()
                            || provider.cargo_version.is_some()
                            || provider.rustc.is_some()
                            || provider.rustc_version.is_some() =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    AcceptedProviderSettings::RustCachePrimingDisabledV1
                        if provider.node.is_some()
                            || provider.typescript.is_some()
                            || !provider.cargo_version.as_deref().is_some_and(identifier)
                            || !provider.rustc_version.as_deref().is_some_and(identifier)
                            || !provider.cargo.as_ref().is_some_and(|cargo| {
                                cargo.validate().is_ok()
                                    && Some(cargo.identity.as_str())
                                        == provider.cargo_version.as_deref()
                            }) =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    AcceptedProviderSettings::RustCachePrimingDisabledV1
                        if !provider.rustc.as_ref().is_some_and(|rustc| {
                            rustc.validate().is_ok()
                                && Some(rustc.identity.as_str())
                                    == provider.rustc_version.as_deref()
                        }) =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    AcceptedProviderSettings::TypeScriptDefaultsV1
                        if !provider.node.as_ref().is_some_and(|node| {
                            node.validate().is_ok() && provider.toolchain == node.identity
                        }) || !provider.typescript.as_ref().is_some_and(|bundle| {
                            bundle.validate().is_ok()
                                && provider.executable.identity == bundle.bridge_version
                        }) || provider.cargo.is_some()
                            || provider.cargo_version.is_some()
                            || provider.rustc.is_some()
                            || provider.rustc_version.is_some() =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    _ => {}
                }
            }
            let mut records = Vec::new();
            for profile in target.profiles {
                if profile
                    .record
                    .as_object()
                    .is_none_or(|record| record.len() != 11)
                {
                    return Err(LauncherError::Rejected);
                }
                let record = PersistedProfileRecord::from_json(&profile.record.to_string())
                    .map_err(|_| LauncherError::Rejected)?;
                let state = HostSandboxState::parse(Some(profile.sandbox_state))
                    .map_err(|_| LauncherError::Rejected)?;
                records.push((record, state));
            }
            let expected = records
                .iter()
                .map(|(record, _)| record.clone())
                .collect::<Vec<_>>();
            let catalog = ExecutionProfileCatalog::from_persisted_records(records, &expected)
                .map_err(|_| LauncherError::Rejected)?;
            // A declared Claude profile must be complete and strict before it is retained; a
            // weakened declaration is rejected outright rather than downgraded to "unavailable",
            // so an operator never believes a partially strict configuration was accepted.
            if let Some(profile) = target.claude_profile
                && profile.validate().is_err()
            {
                return Err(LauncherError::Rejected);
            }
            let launch = LaunchTarget {
                candidate: target.candidate,
                git: target.git,
                codex: target.codex,
                cwd_trampoline: target.cwd_trampoline,
                providers: target.providers,
                catalog,
                allow_disabled_host: target.allow_disabled_host,
                claude_profile: target.claude_profile,
            };
            if targets.insert(target.attachment, launch).is_some() {
                return Err(LauncherError::Rejected);
            }
        }
        Ok(Self {
            targets,
            limits: raw.limits,
        })
    }
    /// Verifies each immutable configured executable once before the worker becomes visible.
    /// Duplicate paths share verification; conflicting fingerprints and cancellation fail closed.
    pub(crate) fn verify_executables(
        &self,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<(), LauncherError> {
        let mut programs: BTreeMap<&Path, &AcceptedExecutable> = BTreeMap::new();
        for target in self.targets.values() {
            for program in std::iter::once(&target.git)
                .chain(std::iter::once(&target.codex))
                .chain(target.cwd_trampoline.iter())
                .chain(target.providers.iter().map(|provider| &provider.executable))
                .chain(
                    target
                        .providers
                        .iter()
                        .filter_map(|provider| provider.node.as_ref()),
                )
                .chain(
                    target
                        .providers
                        .iter()
                        .filter_map(|provider| provider.cargo.as_ref()),
                )
                .chain(
                    target
                        .providers
                        .iter()
                        .filter_map(|provider| provider.rustc.as_ref()),
                )
            {
                if let Some(existing) = programs.insert(program.path.as_path(), program)
                    && !existing.blake3.eq_ignore_ascii_case(&program.blake3)
                {
                    return Err(LauncherError::Rejected);
                }
            }
        }
        for program in programs.values() {
            program.verify_cancellable(cancel)?;
        }
        for provider in self
            .targets
            .values()
            .flat_map(|target| target.providers.iter())
            .filter(|provider| provider.settings == AcceptedProviderSettings::TypeScriptDefaultsV1)
        {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err(LauncherError::Cancelled);
            }
            provider.typescript_bundle()?;
        }
        Ok(())
    }
    /// Verifies every configured executable's current bytes without starting a daemon or worker.
    ///
    /// Delegates to `Self::verify_executables` with a fresh, never-cancelled flag, so a
    /// startup-only caller (such as the `agent-ide launcher check` command) observes the exact
    /// same content check the daemon performs before its worker becomes visible.
    pub fn verify(&self) -> Result<(), LauncherError> {
        self.verify_executables(&std::sync::atomic::AtomicBool::new(false))
    }
    /// Returns only the target keyed by an exact separately supplied launcher attachment.
    pub fn target(&self, attachment: &str) -> Option<&LaunchTarget> {
        self.targets.get(attachment)
    }
}

/// Accepts bounded nonempty identity strings without control bytes.
fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}
/// Rejects relative or lexically non-normal launcher paths without deriving them from cwd.
fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Validates trusted mapping/evidence/limits and refuses ambiguous mappings or unknown settings.
#[test]
fn launcher_mapping_is_closed_bounded_and_restart_only() {
    use crate::execution::D03ProfileEvidence;
    use serde_json::json;
    let state = HostSandboxState::parse(Some(json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":false}))).unwrap();
    let record = PersistedProfileRecord::from_execution_evidence(
        "accepted-disabled",
        1,
        D03ProfileEvidence {
            provider_binary: "accepted-git".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "direct".into(),
            d03_evidence: "accepted-d03".into(),
        },
        &state,
    )
    .unwrap();
    let executable = json!({"path":"/private/tmp/accepted-program","identity":"accepted-git","blake3":"0".repeat(64)});
    let target = json!({"attachment":"private-attachment","candidate":"/private/tmp/worktree","git":executable,"codex":executable,"providers":[],"profiles":[{"record":serde_json::from_str::<Value>(&record.to_json()).unwrap(),"sandbox_state":serde_json::from_str::<Value>(state.sandbox_state_json()).unwrap()}],"allow_disabled_host":true});
    let config = json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[target.clone()]});
    let loaded = LauncherConfig::parse(config.to_string().as_bytes()).unwrap();
    assert_eq!(
        loaded.target("private-attachment").unwrap().candidate,
        PathBuf::from("/private/tmp/worktree")
    );
    assert!(loaded.target("different").is_none());
    assert!(!format!("{loaded:?}").contains("private-attachment"));
    let template_path = std::env::temp_dir().join(format!(
        "agent-ide-bind-one-template-{}.json",
        std::process::id()
    ));
    std::fs::write(&template_path, config.to_string()).unwrap();
    let (bound, bound_bytes) = LauncherConfig::bind_one_candidate(
        &template_path,
        "fresh-managed-attachment",
        Path::new("/private/tmp/captured-worktree"),
    )
    .unwrap();
    assert_eq!(
        bound.target("fresh-managed-attachment").unwrap().candidate,
        PathBuf::from("/private/tmp/captured-worktree")
    );
    let rebound: Value = serde_json::from_slice(&bound_bytes).unwrap();
    let mut expected = config.clone();
    expected["targets"][0]["attachment"] = Value::String("fresh-managed-attachment".into());
    expected["targets"][0]["candidate"] = Value::String("/private/tmp/captured-worktree".into());
    assert_eq!(rebound, expected);
    std::fs::remove_file(template_path).unwrap();
    for changed in [
        json!({"version":2,"limits":config["limits"],"targets":[]}),
        json!({"version":1,"limits":config["limits"],"targets":[target.clone(),target.clone()]}),
        json!({"version":1,"limits":config["limits"],"targets":[],"cwd":"forbidden"}),
    ] {
        assert!(LauncherConfig::parse(changed.to_string().as_bytes()).is_err());
    }
    let mut invalid = config.clone();
    invalid["limits"]["queued"] = json!(65);
    assert!(LauncherConfig::parse(invalid.to_string().as_bytes()).is_err());
    let mut invalid = config;
    invalid["targets"][0]["profiles"][0]["record"]["semantic_state"] = json!("forged");
    assert!(LauncherConfig::parse(invalid.to_string().as_bytes()).is_err());

    let provider = |settings: &str| json!({"executable":executable,"settings":settings,"toolchain":"accepted-git","node":executable,"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"pyright-cache"});
    let mut python_target = target.clone();
    python_target["providers"] = json!([
        provider("pyright_defaults_v1"),
        json!({"executable":executable,"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"go-cache"}),
        json!({"executable":executable,"settings":"rust_cache_priming_disabled_v1","toolchain":"rust-test","cargo":executable,"cargo_version":"accepted-git","rustc":executable,"rustc_version":"accepted-git","trust":"accepted-local","cache_namespace":"rust-cache"}),
        json!({"executable":{"path":"/private/tmp/bridge.mjs","identity":"6.0.0","blake3":"0".repeat(64)},"settings":"typescript_defaults_v1","toolchain":"24.4.0","node":{"path":"/private/tmp/node","identity":"24.4.0","blake3":"0".repeat(64)},"typescript":{"bridge_bytes":1,"bridge_version":"6.0.0","tsserver":{"path":"/private/tmp/tsserver.js","blake3":"1".repeat(64),"bytes":1},"typescript_version":"5.9.3","closure":[{"path":"/private/tmp/typescript.js","blake3":"2".repeat(64),"bytes":1}],"codex_macos_evidence":TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1,"claude_macos_evidence":null},"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"typescript-cache"})
    ]);
    let python_config = json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target.clone()]});
    assert!(LauncherConfig::parse(python_config.to_string().as_bytes()).is_ok());
    let loaded = LauncherConfig::parse(python_config.to_string().as_bytes()).unwrap();
    let typescript = &loaded.target("private-attachment").unwrap().providers[3];
    assert!(typescript.typescript_codex_accepted());
    assert!(!typescript.typescript_claude_accepted());
    let mut invented_claude = python_config.clone();
    invented_claude["targets"][0]["providers"][3]["typescript"]["claude_macos_evidence"] =
        json!("invented");
    assert!(LauncherConfig::parse(invented_claude.to_string().as_bytes()).is_err());
    let mut relative_node = python_config.clone();
    relative_node["targets"][0]["providers"][0]["toolchain"] = json!("node");
    assert!(LauncherConfig::parse(relative_node.to_string().as_bytes()).is_err());
    python_target["providers"][0]["cargo"] = executable;
    assert!(LauncherConfig::parse(json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target]}).to_string().as_bytes()).is_err());
}

/// Detects a changed executable without launching it or consulting cwd/environment identities.
#[test]
fn accepted_executable_requires_exact_current_bytes() {
    let path =
        std::env::temp_dir().join(format!("agent-ide-executable-check-{}", std::process::id()));
    std::fs::write(&path, b"accepted bytes").unwrap();
    let executable = AcceptedExecutable {
        path: path.clone(),
        identity: "accepted".into(),
        blake3: blake3::hash(b"accepted bytes").to_hex().to_string(),
    };
    executable.verify().unwrap();
    std::fs::write(&path, b"changed bytes").unwrap();
    assert_eq!(executable.verify(), Err(LauncherError::ExecutableChanged));
    std::fs::remove_file(path).unwrap();
}

/// `AcceptedExecutable::from_path` measures real bytes into a triple that round-trips through
/// `LauncherConfig::verify`, and `verify` reports the exact same executable's later corruption
/// without starting a daemon.
#[test]
fn launcher_verify_checks_current_executable_bytes_without_a_daemon() {
    use crate::execution::D03ProfileEvidence;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let path =
        std::env::temp_dir().join(format!("agent-ide-launcher-check-{}", std::process::id()));
    std::fs::write(&path, b"#!/bin/sh\necho ok\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = AcceptedExecutable::from_path(path.clone(), "accepted-git").unwrap();
    let executable_json = json!({"path": executable.path, "identity": executable.identity, "blake3": executable.blake3});
    let state = HostSandboxState::parse(Some(json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":false}))).unwrap();
    let record = PersistedProfileRecord::from_execution_evidence(
        "accepted-disabled",
        1,
        D03ProfileEvidence {
            provider_binary: "accepted-git".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "direct".into(),
            d03_evidence: "accepted-d03".into(),
        },
        &state,
    )
    .unwrap();
    let target = json!({"attachment":"verify-attachment","candidate":"/private/tmp/worktree","git":executable_json,"codex":executable_json,"providers":[],"profiles":[{"record":serde_json::from_str::<Value>(&record.to_json()).unwrap(),"sandbox_state":serde_json::from_str::<Value>(state.sandbox_state_json()).unwrap()}],"allow_disabled_host":true});
    let config = LauncherConfig::parse(
        json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[target]})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    config.verify().unwrap();
    std::fs::write(&path, b"#!/bin/sh\necho changed\n").unwrap();
    assert_eq!(config.verify(), Err(LauncherError::ExecutableChanged));
    std::fs::remove_file(path).unwrap();
}

/// Cancelled startup verification performs no read even when the configured path does not exist.
#[test]
fn startup_fingerprint_verification_is_cooperatively_cancellable() {
    let program = AcceptedExecutable {
        path: "/private/tmp/nonexistent-cancelled-executable".into(),
        identity: "cancelled".into(),
        blake3: "0".repeat(64),
    };
    let cancel = std::sync::atomic::AtomicBool::new(true);
    assert_eq!(
        program.verify_cancellable(&cancel),
        Err(LauncherError::Cancelled)
    );
}
