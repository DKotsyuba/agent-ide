//! Restart-only trusted launcher configuration, separate from host metadata and model arguments.

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

/// Fixed launcher failure categories; no paths, attachments or accepted evidence are rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LauncherError {
    /// File cannot be read under its byte bound or JSON is not the supported closed schema.
    Invalid,
    /// An attachment, path, identity, profile record or limit is absent, ambiguous or unsupported.
    Rejected,
    /// A configured executable no longer has its accepted content fingerprint.
    ExecutableChanged,
}

/// An operator-accepted executable path and immutable measured identity.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptedExecutable {
    /// Absolute normalized executable selected by the trusted launcher, never a model parameter.
    pub path: PathBuf,
    /// Nonempty accepted binary/version identity matched to Execution profile evidence.
    pub identity: String,
    /// BLAKE3 digest of the accepted executable bytes; rechecked before each physical spawn.
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
    /// Rechecks bounded executable contents immediately before use; it never starts a process.
    pub fn verify(&self) -> Result<(), LauncherError> {
        let mut file = File::open(&self.path)
            .map_err(|_| LauncherError::ExecutableChanged)?
            .take(MAX_EXECUTABLE_BYTES + 1);
        let mut hash = blake3::Hasher::new();
        let mut buffer = [0; 32 * 1024];
        let mut total = 0;
        loop {
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
}

/// Closed effective provider configuration; arbitrary settings JSON is never accepted.
#[derive(Clone, Copy, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedProviderSettings {
    /// Accepted gopls defaults on a separately owned logical view.
    GoplsDefaults,
    /// Accepted Rust profile with cache priming disabled and server-status synchronization.
    RustCachePrimingDisabledV1,
}

/// Trusted provider identity; populated only by the restart-loaded launcher file.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderLaunch {
    /// Absolute executable and measured binary fingerprint.
    pub executable: AcceptedExecutable,
    /// Closed settings contract whose exact identifier participates in compatibility checks.
    pub settings: AcceptedProviderSettings,
    /// Accepted toolchain identity; gopls uses an absolute Go executable, Rust a rustup selector.
    pub toolchain: String,
    /// Accepted Cargo identity for Rust; absent for gopls.
    pub cargo_version: Option<String>,
    /// Accepted rustc identity for Rust; absent for gopls.
    pub rustc_version: Option<String>,
    /// Explicit operator trust identity, never derived from a sandbox observation.
    pub trust: String,
    /// Persistent compatible cache namespace, retained after stopping a view.
    pub cache_namespace: String,
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
    /// At most the two current language profiles; duplicate settings/languages are rejected.
    providers: Vec<ProviderLaunch>,
    /// Trusted Execution records and exact evidence states, limited to two supported profile classes.
    profiles: Vec<AcceptedProfile>,
    /// Explicit policy acceptance for an observed disabled host; false never weakens sandboxing.
    allow_disabled_host: bool,
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
    /// Accepted closed provider profiles for this candidate.
    pub providers: Vec<ProviderLaunch>,
    /// Execution-minted catalog reconstructed only from trusted matching profile evidence.
    pub catalog: ExecutionProfileCatalog,
    /// Explicit trusted policy for disabled host observations.
    pub allow_disabled_host: bool,
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
    /// Validates bounded trusted JSON without host inference, network access or process effects.
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
                || target.providers.len() > 2
            {
                return Err(LauncherError::Rejected);
            }
            target.git.validate()?;
            target.codex.validate()?;
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
                            || provider.cargo_version.is_some()
                            || provider.rustc_version.is_some() =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    AcceptedProviderSettings::RustCachePrimingDisabledV1
                        if !provider.cargo_version.as_deref().is_some_and(identifier)
                            || !provider.rustc_version.as_deref().is_some_and(identifier) =>
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
            let launch = LaunchTarget {
                candidate: target.candidate,
                git: target.git,
                codex: target.codex,
                providers: target.providers,
                catalog,
                allow_disabled_host: target.allow_disabled_host,
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
