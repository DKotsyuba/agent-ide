//! Restart-only trusted launcher configuration, separate from host metadata and model arguments.

use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Maximum complete launcher file; limits memory before JSON decoding.
const MAX_CONFIG_BYTES: usize = 64 * 1024;
/// Maximum absolute allowed roots; bounds the operator declaration and admission work.
const MAX_ALLOWED_ROOTS: usize = 16;
/// Maximum executable bytes hashed during a pre-spawn identity check.
const MAX_EXECUTABLE_BYTES: u64 = 256 * 1024 * 1024;
/// Compiled Codex release record prefix for the exact TypeScript r3 macOS bundle cell.
const TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1: &str =
    "macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r3-2026-09-14";
/// Compiled Claude release record prefix for the same exact TypeScript r3 macOS bundle cell.
const TYPESCRIPT_CLAUDE_MACOS_EVIDENCE_V1: &str =
    "macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-claude-r3-2026-09-14";
/// Exact Node release admitted by the compiled Codex TypeScript record.
const TYPESCRIPT_NODE_VERSION_V1: &str = "24.4.0";
/// BLAKE3 identity of the accepted macOS Node 24.4.0 executable bytes.
const TYPESCRIPT_NODE_BLAKE3_V1: &str =
    "f3d5f7b7c7296b22889c5ca6a62fbfebc6f263190cefec97255d98a286e92ce9";
/// Exact TypeScript Language Server release admitted by the compiled Codex record.
const TYPESCRIPT_BRIDGE_VERSION_V1: &str = "6.0.0";
/// BLAKE3 identity of the accepted TypeScript Language Server 6.0.0 bridge bytes.
const TYPESCRIPT_BRIDGE_BLAKE3_V1: &str =
    "541877f06eff230f60b5ca90332d2db54128d0dd8b88bd7fdc987976e22a8c9b";
/// Exact accepted TypeScript Language Server bridge byte length.
const TYPESCRIPT_BRIDGE_BYTES_V1: u64 = 917_064;
/// Exact TypeScript release admitted by the compiled Codex record.
const TYPESCRIPT_VERSION_V1: &str = "5.9.3";
/// BLAKE3 identity of the accepted TypeScript 5.9.3 `tsserver.js` bytes.
const TYPESCRIPT_TSSERVER_BLAKE3_V1: &str =
    "fd205df6b7930ede592846b8aeabc046f75a76f8f4eaf74a2b6dba9b3bd6a1a8";
/// Exact accepted TypeScript 5.9.3 `tsserver.js` byte length.
const TYPESCRIPT_TSSERVER_BYTES_V1: u64 = 272;
/// Ordered basename, BLAKE3 digest, and length of the accepted loaded runtime closure.
const TYPESCRIPT_CLOSURE_V1: [(&str, &str, u64); 4] = [
    (
        "_tsserver.js",
        "2f5f9a981943299237ca1a8f566aff95814508abf919027c4f5bb82dc9c5762f",
        27_888,
    ),
    (
        "typescript.js",
        "90519822fe3575779770b1e3a921528d30777e2be3c97cb68457caf2c22393e9",
        9_112_572,
    ),
    (
        "package.json",
        "822486c3f526033cfa7e628d2725e1ee968850b281194496ef733dd6b1d9096d",
        3_620,
    ),
    (
        "package.json",
        "d93faca38a6da90246cddcb64eaf2ec7537a1dd3973f74034fd33e6c667ceca7",
        2_542,
    ),
];

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
    /// Optional separate Claude macOS record; `None` keeps the provider unavailable to Claude.
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
            || !identifier(&self.codex_macos_evidence)
            || self
                .claude_macos_evidence
                .as_deref()
                .is_some_and(|evidence| !identifier(evidence))
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
    /// Derives the only Codex macOS record accepted for this exact declared TypeScript bundle.
    ///
    /// The public release prefix is accepted only with Node 24.4.0, TypeScript Language Server
    /// 6.0.0, TypeScript 5.9.3, the compiled accepted Node/bridge/tsserver/closure byte identities,
    /// an exact `tsserver.js` basename, and a BLAKE3 suffix over every declared path, digest, byte
    /// length, and identity. Returns `None` for any other provider kind, release, malformed bundle,
    /// or absent Node. This performs no filesystem read and grants no execution authority; startup
    /// and pre-spawn remeasurement remain separate mandatory checks.
    pub fn expected_typescript_codex_macos_evidence(&self) -> Option<String> {
        let node = self.node.as_ref()?;
        let bundle = self.typescript.as_ref()?;
        if self.settings != AcceptedProviderSettings::TypeScriptDefaultsV1
            || node.validate().is_err()
            || self.executable.validate().is_err()
            || bundle.validate().is_err()
            || self.toolchain != TYPESCRIPT_NODE_VERSION_V1
            || node.identity != TYPESCRIPT_NODE_VERSION_V1
            || !node.blake3.eq_ignore_ascii_case(TYPESCRIPT_NODE_BLAKE3_V1)
            || self.executable.identity != TYPESCRIPT_BRIDGE_VERSION_V1
            || !self
                .executable
                .blake3
                .eq_ignore_ascii_case(TYPESCRIPT_BRIDGE_BLAKE3_V1)
            || bundle.bridge_bytes != TYPESCRIPT_BRIDGE_BYTES_V1
            || bundle.bridge_version != TYPESCRIPT_BRIDGE_VERSION_V1
            || bundle.typescript_version != TYPESCRIPT_VERSION_V1
            || !bundle
                .tsserver
                .blake3
                .eq_ignore_ascii_case(TYPESCRIPT_TSSERVER_BLAKE3_V1)
            || bundle.tsserver.bytes != TYPESCRIPT_TSSERVER_BYTES_V1
            || bundle
                .tsserver
                .path
                .file_name()
                .and_then(|name| name.to_str())
                != Some("tsserver.js")
            || bundle.closure.len() != TYPESCRIPT_CLOSURE_V1.len()
            || bundle.closure.iter().zip(TYPESCRIPT_CLOSURE_V1).any(
                |(file, (name, digest, bytes))| {
                    file.path.file_name().and_then(|value| value.to_str()) != Some(name)
                        || !file.blake3.eq_ignore_ascii_case(digest)
                        || file.bytes != bytes
                },
            )
        {
            return None;
        }
        let mut hash = blake3::Hasher::new();
        evidence_frame(&mut hash, b"typescript-codex-macos-bundle-v1");
        for value in [
            self.toolchain.as_bytes(),
            node.path.as_os_str().as_encoded_bytes(),
            node.identity.as_bytes(),
            node.blake3.as_bytes(),
            self.executable.path.as_os_str().as_encoded_bytes(),
            self.executable.identity.as_bytes(),
            self.executable.blake3.as_bytes(),
            bundle.bridge_version.as_bytes(),
            bundle.tsserver.path.as_os_str().as_encoded_bytes(),
            bundle.tsserver.blake3.as_bytes(),
            bundle.typescript_version.as_bytes(),
        ] {
            evidence_frame(&mut hash, value);
        }
        evidence_frame(&mut hash, &bundle.bridge_bytes.to_le_bytes());
        evidence_frame(&mut hash, &bundle.tsserver.bytes.to_le_bytes());
        for file in &bundle.closure {
            evidence_frame(&mut hash, file.path.as_os_str().as_encoded_bytes());
            evidence_frame(&mut hash, file.blake3.as_bytes());
            evidence_frame(&mut hash, &file.bytes.to_le_bytes());
        }
        Some(format!(
            "{TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1}:{}",
            hash.finalize().to_hex()
        ))
    }

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
        self.expected_typescript_codex_macos_evidence()
            .is_some_and(|expected| {
                self.typescript
                    .as_ref()
                    .is_some_and(|bundle| bundle.codex_macos_evidence == expected)
            })
    }

    /// Derives the Claude record for the same exact bundle already bound by the Codex digest.
    ///
    /// The distinct prefix names the independently exercised Claude host cell. Reusing the exact
    /// bundle suffix keeps both records bound to identical declared paths, bytes, and releases;
    /// malformed or non-Codex-accepted declarations return `None` without filesystem I/O.
    pub fn expected_typescript_claude_macos_evidence(&self) -> Option<String> {
        let codex = self.expected_typescript_codex_macos_evidence()?;
        let (_, bundle_digest) = codex.rsplit_once(':')?;
        Some(format!(
            "{TYPESCRIPT_CLAUDE_MACOS_EVIDENCE_V1}:{bundle_digest}"
        ))
    }

    /// Returns whether the independent Claude release record accepts this exact declaration.
    pub fn typescript_claude_accepted(&self) -> bool {
        self.expected_typescript_claude_macos_evidence()
            .is_some_and(|expected| {
                self.typescript.as_ref().is_some_and(|bundle| {
                    bundle.claude_macos_evidence.as_deref() == Some(expected.as_str())
                })
            })
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

/// Contract default milliseconds between a worktree change and a check start.
const fn default_debounce_ms() -> u64 {
    1500
}
/// Contract default seconds of zero leases and no running check before the daemon stops.
const fn default_idle_timeout_s() -> u64 {
    300
}
/// Contract default total wall-clock ceiling of one check run.
const fn default_check_timeout_s() -> u64 {
    300
}

/// Accepted Rust toolchain declaration for confined background project checks.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRustChecksConfig {
    /// Absolute normalized rustup toolchain directory used to run confined Rust checks.
    toolchain_dir: PathBuf,
    /// Optional absolute normalized cargo home; `None` falls back to the default `$HOME/.cargo`.
    #[serde(default)]
    cargo_home: Option<PathBuf>,
    /// Optional absolute normalized Apple developer directory override; `None` resolves it from
    /// `/usr/bin/xcode-select -p` with the fixed fallbacks (T05B, EYES-r2 §3).
    #[serde(default)]
    developer_dir: Option<PathBuf>,
}
impl ProjectRustChecksConfig {
    /// Returns the declared absolute toolchain directory; never resolved from model or project input.
    pub fn toolchain_dir(&self) -> &Path {
        &self.toolchain_dir
    }
    /// Returns the declared absolute cargo home, or `None` for the default `$HOME/.cargo` (EYES-r2 §1).
    pub fn cargo_home(&self) -> Option<&Path> {
        self.cargo_home.as_deref()
    }
    /// Returns the declared absolute Apple developer directory override, or `None` to resolve it
    /// from `/usr/bin/xcode-select -p` (T05B).
    pub fn developer_dir(&self) -> Option<&Path> {
        self.developer_dir.as_deref()
    }
    /// Rejects a relative or lexically non-normal toolchain directory, cargo home or developer
    /// directory at parse time.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.toolchain_dir) {
            return Err(LauncherError::Rejected);
        }
        if let Some(cargo_home) = &self.cargo_home
            && !absolute(cargo_home)
        {
            return Err(LauncherError::Rejected);
        }
        if let Some(developer_dir) = &self.developer_dir
            && !absolute(developer_dir)
        {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }
}

/// Accepted Python toolchain declaration for confined background project checks.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPythonChecksConfig {
    /// Absolute normalized Node executable; the only program allowed to start confined Pyright.
    node: PathBuf,
    /// Absolute normalized Pyright CLI entry module executed by `node`.
    pyright_cli: PathBuf,
}
/// Accepted TypeScript CLI declaration for confined background project checks.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTypeScriptChecksConfig {
    /// Absolute normalized pinned Node executable.
    node: PathBuf,
    /// Absolute normalized pinned TypeScript `tsc.js` module.
    tsc_cli: PathBuf,
}
impl ProjectTypeScriptChecksConfig {
    /// Returns the declared Node executable path.
    pub fn node(&self) -> &Path {
        &self.node
    }
    /// Returns the declared TypeScript CLI module path.
    pub fn tsc_cli(&self) -> &Path {
        &self.tsc_cli
    }
    /// Rejects either tool path unless absolute and lexically normalized.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.node) || !absolute(&self.tsc_cli) {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }
}
impl ProjectPythonChecksConfig {
    /// Returns the declared absolute Node executable path.
    pub fn node(&self) -> &Path {
        &self.node
    }
    /// Returns the declared absolute Pyright CLI entry module path.
    pub fn pyright_cli(&self) -> &Path {
        &self.pyright_cli
    }
    /// Rejects a relative or lexically non-normal tool path at parse time.
    fn validate(&self) -> Result<(), LauncherError> {
        if !absolute(&self.node) || !absolute(&self.pyright_cli) {
            return Err(LauncherError::Rejected);
        }
        Ok(())
    }
}

/// Optional timing and language declarations for confined background project checks.
///
/// Presence enables project checks only together with a nonempty [`LauncherConfig::allowed_roots`];
/// an absent language subsection means that language is never checked and never appears in a feed.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectChecksConfig {
    /// Minimum milliseconds between an observed worktree change and a check start, 100..=10000.
    #[serde(default = "default_debounce_ms")]
    debounce_ms: u64,
    /// Seconds of zero leases and no running check before the shared daemon stops, 30..=3600.
    #[serde(default = "default_idle_timeout_s")]
    idle_timeout_s: u64,
    /// Total wall-clock ceiling of one check run including its tool startup, 10..=900.
    #[serde(default = "default_check_timeout_s")]
    check_timeout_s: u64,
    /// Rust check toolchain declaration; `None` keeps Rust unchecked.
    #[serde(default)]
    rust: Option<ProjectRustChecksConfig>,
    /// Python check toolchain declaration; `None` keeps Python unchecked.
    #[serde(default)]
    python: Option<ProjectPythonChecksConfig>,
    /// TypeScript/JavaScript check toolchain declaration; `None` keeps it unchecked.
    #[serde(default)]
    typescript: Option<ProjectTypeScriptChecksConfig>,
}
impl ProjectChecksConfig {
    /// Returns the debounce window between a change and the check it triggers.
    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms)
    }
    /// Returns the shared daemon idle timeout with zero leases and no running check.
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_s)
    }
    /// Returns the total wall-clock ceiling of one check run.
    pub fn check_timeout(&self) -> Duration {
        Duration::from_secs(self.check_timeout_s)
    }
    /// Returns the Rust declaration; `None` means Rust is never checked.
    pub fn rust(&self) -> Option<&ProjectRustChecksConfig> {
        self.rust.as_ref()
    }
    /// Returns the Python declaration; `None` means Python is never checked.
    pub fn python(&self) -> Option<&ProjectPythonChecksConfig> {
        self.python.as_ref()
    }
    /// Returns the TypeScript declaration; `None` means it is never checked.
    pub fn typescript(&self) -> Option<&ProjectTypeScriptChecksConfig> {
        self.typescript.as_ref()
    }
    /// Checks the contract timing ranges and every declared path before any check can run.
    fn validate(&self) -> Result<(), LauncherError> {
        if !(100..=10_000).contains(&self.debounce_ms)
            || !(30..=3600).contains(&self.idle_timeout_s)
            || !(10..=900).contains(&self.check_timeout_s)
        {
            return Err(LauncherError::Rejected);
        }
        if let Some(rust) = &self.rust {
            rust.validate()?;
        }
        if let Some(python) = &self.python {
            python.validate()?;
        }
        if let Some(typescript) = &self.typescript {
            typescript.validate()?;
        }
        Ok(())
    }
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
    /// Legacy 0.3.16 fields accepted for compatibility and ignored: the IDE no longer replays a
    /// host sandbox, so it needs no Codex wrapper, `env` trampoline, accepted profiles, or
    /// disabled-host acceptance.
    #[serde(default, rename = "codex")]
    _codex: Option<Value>,
    #[serde(default, rename = "cwd_trampoline")]
    _cwd_trampoline: Option<Value>,
    /// At most the two current language profiles; duplicate settings/languages are rejected.
    providers: Vec<ProviderLaunch>,
    #[serde(default, rename = "profiles")]
    _profiles: Vec<Value>,
    #[serde(default, rename = "allow_disabled_host")]
    _allow_disabled_host: bool,
    /// Retired Claude helper profile; accepted and ignored so older files keep loading.
    #[serde(default, rename = "claude_profile")]
    _claude_profile: Option<Value>,
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
    /// Absolute normalized directory roots inside which confined project checks may run.
    #[serde(default)]
    allowed_roots: Vec<PathBuf>,
    /// Confined background project-check declarations; absent disables project checks.
    #[serde(default)]
    project_checks: Option<ProjectChecksConfig>,
}

/// One validated trusted target; metadata alone never creates one of these values.
#[derive(Clone)]
pub struct LaunchTarget {
    /// Raw absolute candidate for controlled Git discovery; not itself Workspace authority.
    pub candidate: PathBuf,
    /// Trusted accepted Git program identity.
    pub git: AcceptedExecutable,
    /// Accepted closed provider profiles for this candidate.
    pub providers: Vec<ProviderLaunch>,
}

/// Immutable attachment map owned by one daemon generation; Debug always redacts its contents.
#[derive(Clone)]
pub struct LauncherConfig {
    /// Private attachment-to-target map, never populated from model args, cwd, PID or timing.
    targets: BTreeMap<String, LaunchTarget>,
    /// Shared validated bounds for the daemon's single worker.
    pub limits: ProductLimits,
    /// Absolute normalized roots admitted for confined project checks; empty disables them.
    allowed_roots: Vec<PathBuf>,
    /// Validated project-check declarations; `None` disables project checks.
    project_checks: Option<ProjectChecksConfig>,
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
        if raw.allowed_roots.len() > MAX_ALLOWED_ROOTS
            || raw.allowed_roots.iter().any(|root| !allowed_root(root))
        {
            return Err(LauncherError::Rejected);
        }
        if let Some(checks) = &raw.project_checks {
            checks.validate()?;
        }
        if raw.version != 1 || raw.targets.len() > 64 {
            return Err(LauncherError::Rejected);
        }
        let mut targets = BTreeMap::new();
        for target in raw.targets {
            if !identifier(&target.attachment)
                || target.attachment.len() > 128
                || !absolute(&target.candidate)
                || target.providers.len() > 4
            {
                return Err(LauncherError::Rejected);
            }
            target.git.validate()?;
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
                        }) || !provider.typescript_codex_accepted()
                            || provider.typescript.as_ref().is_some_and(|bundle| {
                                bundle.claude_macos_evidence.is_some()
                                    && !provider.typescript_claude_accepted()
                            })
                            || provider.cargo.is_some()
                            || provider.cargo_version.is_some()
                            || provider.rustc.is_some()
                            || provider.rustc_version.is_some() =>
                    {
                        return Err(LauncherError::Rejected);
                    }
                    _ => {}
                }
            }
            let launch = LaunchTarget {
                candidate: target.candidate,
                git: target.git,
                providers: target.providers,
            };
            if targets.insert(target.attachment, launch).is_some() {
                return Err(LauncherError::Rejected);
            }
        }
        Ok(Self {
            targets,
            limits: raw.limits,
            allowed_roots: raw.allowed_roots,
            project_checks: raw.project_checks,
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

    /// Returns the sole managed launch target as a template for another host-selected worktree.
    pub fn sole_target(&self) -> Option<&LaunchTarget> {
        (self.targets.len() == 1)
            .then(|| self.targets.values().next())
            .flatten()
    }

    /// Returns the configured absolute allowed roots; an empty slice disables project checks.
    ///
    /// Values are the operator's declared lexical forms; canonicalization against the live
    /// filesystem happens only in [`admit_worktree`], never at parse time.
    pub fn allowed_roots(&self) -> &[PathBuf] {
        &self.allowed_roots
    }

    /// Returns the validated project-check declarations; `None` disables project checks.
    pub fn project_checks(&self) -> Option<&ProjectChecksConfig> {
        self.project_checks.as_ref()
    }

    /// Iterates the configured private attachments for runtime-bound fallback authentication.
    ///
    /// Values remain borrowed from this immutable launcher generation and must never be persisted,
    /// logged, or rendered by the consumer.
    pub(crate) fn attachments(&self) -> impl Iterator<Item = &str> {
        self.targets.keys().map(String::as_str)
    }
}

/// Fixed worktree admission failure categories for confined project checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootAdmissionError {
    /// No allowed root is configured, so project checks are disabled.
    NoRoots,
    /// The worktree's canonical path is not equal to or below any canonical allowed root.
    ///
    /// This also covers a symlink inside a root whose target resolves outside every root.
    OutsideRoots,
    /// The worktree itself cannot be canonicalized on the current filesystem.
    ///
    /// A configured root that fails to canonicalize is skipped instead of aborting admission, so
    /// one broken operator declaration never hides a different, valid root; this variant is
    /// returned only when the worktree cannot be resolved at all.
    Unresolvable,
}

/// Admits one worktree for confined project checks when it resolves under one allowed root.
///
/// The worktree and every root are resolved with `std::fs::canonicalize`, so admission compares
/// real filesystem locations: a symlinked worktree spelling is compared in its target location,
/// and a symlink inside a root that points outside is rejected. Containment is component-wise,
/// not a string prefix: `/a/bc` is not below `/a/b`. A configured root that fails to canonicalize
/// is skipped and the remaining roots are still tried, so admission never depends on the order of
/// `allowed_roots`; it is [`RootAdmissionError::OutsideRoots`], not `Unresolvable`, once every
/// root has been tried and none both resolves and contains the worktree. Returns the canonical
/// worktree path on success; admission performs no write and grants no execution authority by
/// itself.
pub fn admit_worktree(
    allowed_roots: &[PathBuf],
    worktree: &Path,
) -> Result<PathBuf, RootAdmissionError> {
    if allowed_roots.is_empty() {
        return Err(RootAdmissionError::NoRoots);
    }
    let canonical_worktree =
        std::fs::canonicalize(worktree).map_err(|_| RootAdmissionError::Unresolvable)?;
    contained_by_root(allowed_roots, canonical_worktree)
}

/// Admits one absolute path that may not exist yet against the configured allowed roots.
///
/// The deepest existing ancestor is canonicalized and the remaining components are appended
/// verbatim, so a file the IDE is about to create or probe is judged by where it would land.
/// Relative paths and `.`/`..` segments are refused as unresolvable. Returns the resolved path.
pub fn admit_path(allowed_roots: &[PathBuf], path: &Path) -> Result<PathBuf, RootAdmissionError> {
    if allowed_roots.is_empty() {
        return Err(RootAdmissionError::NoRoots);
    }
    if !absolute(path) {
        return Err(RootAdmissionError::Unresolvable);
    }
    let mut existing = path;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let canonical = loop {
        match std::fs::canonicalize(existing) {
            Ok(canonical) => break canonical,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tail.push(
                    existing
                        .file_name()
                        .ok_or(RootAdmissionError::Unresolvable)?
                        .to_os_string(),
                );
                existing = existing.parent().ok_or(RootAdmissionError::Unresolvable)?;
            }
            Err(_) => return Err(RootAdmissionError::Unresolvable),
        }
    };
    let resolved = tail
        .iter()
        .rev()
        .fold(canonical, |path, component| path.join(component));
    contained_by_root(allowed_roots, resolved)
}

/// Returns `canonical` when it is equal to or below one resolvable configured root.
///
/// A root that cannot be resolved is treated as non-matching rather than aborting admission, so
/// one broken declaration never hides a different, valid root. `Path::starts_with` compares whole
/// components, so a longer sibling sharing the root's string prefix is never contained.
fn contained_by_root(
    allowed_roots: &[PathBuf],
    canonical: PathBuf,
) -> Result<PathBuf, RootAdmissionError> {
    for root in allowed_roots {
        let Ok(canonical_root) = std::fs::canonicalize(root) else {
            continue;
        };
        if canonical.starts_with(&canonical_root) {
            return Ok(canonical);
        }
    }
    Err(RootAdmissionError::OutsideRoots)
}

#[cfg(test)]
mod path_admission_tests {
    use super::{RootAdmissionError, admit_path, admit_worktree};
    use std::path::PathBuf;

    /// A missing file below an allowed root resolves to its would-be location; escapes refuse.
    #[test]
    fn admit_path_resolves_missing_targets_and_refuses_escapes() {
        let base = std::env::temp_dir().join(format!("agent-ide-admit-{}", std::process::id()));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(base.join("other")).unwrap();
        let roots = vec![root.clone()];
        let canonical_root = std::fs::canonicalize(&root).unwrap();

        assert_eq!(
            admit_path(&roots, &root.join("nested/absent/tsconfig.json")).unwrap(),
            canonical_root.join("nested/absent/tsconfig.json")
        );
        assert_eq!(
            admit_worktree(&roots, &root.join("nested")).unwrap(),
            canonical_root.join("nested")
        );
        assert_eq!(
            admit_path(&roots, &base.join("other/file")).unwrap_err(),
            RootAdmissionError::OutsideRoots
        );
        assert_eq!(
            admit_path(&roots, &root.join("nested/../../other")).unwrap_err(),
            RootAdmissionError::Unresolvable
        );
        assert_eq!(
            admit_path(&roots, &PathBuf::from("relative/file")).unwrap_err(),
            RootAdmissionError::Unresolvable
        );
        assert_eq!(
            admit_path(&[], &root.join("x")).unwrap_err(),
            RootAdmissionError::NoRoots
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// Accepts bounded nonempty identity strings without control bytes.
fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Appends one unambiguous raw field to the declared TypeScript evidence digest.
fn evidence_frame(hash: &mut blake3::Hasher, value: &[u8]) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value);
}
/// Rejects relative or lexically non-normal launcher paths without deriving them from cwd.
fn absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Accepts only absolute normalized allowed roots: no `..`, and no trailing separator.
///
/// The lexical `..` check reuses [`absolute`]; the separator check reads the raw path bytes
/// because `Path::components` silently normalizes a trailing slash away. The filesystem root
/// itself is therefore never an allowed root, which keeps whole-filesystem admission undeclarable.
fn allowed_root(path: &Path) -> bool {
    absolute(path) && !path.as_os_str().as_encoded_bytes().ends_with(b"/")
}

/// Validates trusted mappings and limits and refuses ambiguous mappings or unknown settings.
#[test]
fn launcher_mapping_is_closed_bounded_and_restart_only() {
    use serde_json::json;
    let executable = json!({"path":"/private/tmp/accepted-program","identity":"accepted-git","blake3":"0".repeat(64)});
    let target = json!({"attachment":"private-attachment","candidate":"/private/tmp/worktree","git":executable,"codex":executable,"providers":[],"profiles":[],"allow_disabled_host":true});
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
    let mut legacy = config.clone();
    legacy["targets"][0]["profiles"] = json!([{"arbitrary":true}]);
    assert!(LauncherConfig::parse(legacy.to_string().as_bytes()).is_ok());

    let provider = |settings: &str| json!({"executable":executable,"settings":settings,"toolchain":"accepted-git","node":executable,"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"pyright-cache"});
    let mut python_target = target.clone();
    python_target["providers"] = json!([
        provider("pyright_defaults_v1"),
        json!({"executable":executable,"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"go-cache"}),
        json!({"executable":executable,"settings":"rust_cache_priming_disabled_v1","toolchain":"rust-test","cargo":executable,"cargo_version":"accepted-git","rustc":executable,"rustc_version":"accepted-git","trust":"accepted-local","cache_namespace":"rust-cache"}),
        json!({"executable":{"path":"/private/tmp/bridge.mjs","identity":"6.0.0","blake3":TYPESCRIPT_BRIDGE_BLAKE3_V1},"settings":"typescript_defaults_v1","toolchain":"24.4.0","node":{"path":"/private/tmp/node","identity":"24.4.0","blake3":TYPESCRIPT_NODE_BLAKE3_V1},"typescript":{"bridge_bytes":TYPESCRIPT_BRIDGE_BYTES_V1,"bridge_version":"6.0.0","tsserver":{"path":"/private/tmp/tsserver.js","blake3":TYPESCRIPT_TSSERVER_BLAKE3_V1,"bytes":TYPESCRIPT_TSSERVER_BYTES_V1},"typescript_version":"5.9.3","closure":[{"path":"/private/tmp/a/_tsserver.js","blake3":TYPESCRIPT_CLOSURE_V1[0].1,"bytes":TYPESCRIPT_CLOSURE_V1[0].2},{"path":"/private/tmp/a/typescript.js","blake3":TYPESCRIPT_CLOSURE_V1[1].1,"bytes":TYPESCRIPT_CLOSURE_V1[1].2},{"path":"/private/tmp/b/package.json","blake3":TYPESCRIPT_CLOSURE_V1[2].1,"bytes":TYPESCRIPT_CLOSURE_V1[2].2},{"path":"/private/tmp/c/package.json","blake3":TYPESCRIPT_CLOSURE_V1[3].1,"bytes":TYPESCRIPT_CLOSURE_V1[3].2}],"codex_macos_evidence":TYPESCRIPT_CODEX_MACOS_EVIDENCE_V1,"claude_macos_evidence":null},"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"accepted-local","cache_namespace":"typescript-cache"})
    ]);
    let mut python_config = json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target.clone()]});
    let unbound: ProviderLaunch =
        serde_json::from_value(python_config["targets"][0]["providers"][3].clone()).unwrap();
    python_config["targets"][0]["providers"][3]["typescript"]["codex_macos_evidence"] =
        json!(unbound.expected_typescript_codex_macos_evidence().unwrap());
    assert!(LauncherConfig::parse(python_config.to_string().as_bytes()).is_ok());
    let loaded = LauncherConfig::parse(python_config.to_string().as_bytes()).unwrap();
    let typescript = &loaded.target("private-attachment").unwrap().providers[3];
    assert!(typescript.typescript_codex_accepted());
    assert!(!typescript.typescript_claude_accepted());
    let accepted_claude = typescript
        .expected_typescript_claude_macos_evidence()
        .unwrap();
    python_config["targets"][0]["providers"][3]["typescript"]["claude_macos_evidence"] =
        json!(accepted_claude);
    let loaded = LauncherConfig::parse(python_config.to_string().as_bytes()).unwrap();
    assert!(loaded.target("private-attachment").unwrap().providers[3].typescript_claude_accepted());
    let mut invented_claude = python_config.clone();
    invented_claude["targets"][0]["providers"][3]["typescript"]["claude_macos_evidence"] =
        json!("invented");
    assert!(LauncherConfig::parse(invented_claude.to_string().as_bytes()).is_err());
    for field in [
        "toolchain",
        "node.identity",
        "executable.identity",
        "executable.blake3",
        "typescript.typescript_version",
        "typescript.tsserver.blake3",
        "typescript.closure.blake3",
        "typescript.closure.path",
    ] {
        let mut copied = python_config.clone();
        let provider = &mut copied["targets"][0]["providers"][3];
        match field {
            "toolchain" => provider["toolchain"] = json!("24.4.1"),
            "node.identity" => provider["node"]["identity"] = json!("24.4.1"),
            "executable.identity" => provider["executable"]["identity"] = json!("6.0.1"),
            "executable.blake3" => provider["executable"]["blake3"] = json!("e".repeat(64)),
            "typescript.typescript_version" => {
                provider["typescript"]["typescript_version"] = json!("5.9.4")
            }
            "typescript.tsserver.blake3" => {
                provider["typescript"]["tsserver"]["blake3"] = json!("f".repeat(64))
            }
            "typescript.closure.blake3" => {
                provider["typescript"]["closure"][0]["blake3"] = json!("d".repeat(64))
            }
            "typescript.closure.path" => {
                provider["typescript"]["closure"][0]["path"] =
                    json!("/private/tmp/a/a/_tsserver.js")
            }
            _ => unreachable!("closed copied-evidence mutation"),
        }
        assert!(
            LauncherConfig::parse(copied.to_string().as_bytes()).is_err(),
            "copied evidence accepted changed {field}"
        );
    }
    let mut relative_node = python_config.clone();
    relative_node["targets"][0]["providers"][0]["toolchain"] = json!("node");
    assert!(LauncherConfig::parse(relative_node.to_string().as_bytes()).is_err());
    python_target["providers"][0]["cargo"] = executable;
    assert!(LauncherConfig::parse(json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[python_target]}).to_string().as_bytes()).is_err());
}

/// Legacy host execution fields remain accepted but do not appear in the launch target.
#[test]
fn launcher_ignores_legacy_execution_fields() {
    let config = LauncherConfig::parse(
        br#"{"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[{"attachment":"legacy","candidate":"/private/tmp/worktree","git":{"path":"/usr/bin/git","identity":"git","blake3":"0000000000000000000000000000000000000000000000000000000000000000"},"codex":{"path":"/usr/bin/codex"},"cwd_trampoline":"/usr/bin/env","profiles":[{"ignored":true}],"allow_disabled_host":true,"providers":[]}]}"#,
    )
    .unwrap();
    let target = config.target("legacy").unwrap();
    assert_eq!(target.candidate, PathBuf::from("/private/tmp/worktree"));
    assert!(target.providers.is_empty());
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
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    let path =
        std::env::temp_dir().join(format!("agent-ide-launcher-check-{}", std::process::id()));
    std::fs::write(&path, b"#!/bin/sh\necho ok\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executable = AcceptedExecutable::from_path(path.clone(), "accepted-git").unwrap();
    let executable_json = json!({"path": executable.path, "identity": executable.identity, "blake3": executable.blake3});
    let target = json!({"attachment":"verify-attachment","candidate":"/private/tmp/worktree","git":executable_json,"providers":[]});
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

/// Accepts an optional TypeScript checker and rejects non-normal checker paths.
#[test]
fn project_checks_accept_optional_typescript_and_reject_non_normal_paths() {
    use serde_json::json;
    let old: ProjectChecksConfig = serde_json::from_value(
        json!({"python": {"node": "/abs/node", "pyright_cli": "/abs/pyright"}}),
    )
    .unwrap();
    assert!(old.typescript().is_none());
    assert!(old.validate().is_ok());
    let new: ProjectChecksConfig = serde_json::from_value(
        json!({"typescript": {"node": "/abs/node", "tsc_cli": "/abs/typescript/lib/tsc.js"}}),
    )
    .unwrap();
    assert_eq!(
        new.typescript().unwrap().tsc_cli(),
        Path::new("/abs/typescript/lib/tsc.js")
    );
    assert!(new.validate().is_ok());
    let bad: ProjectChecksConfig = serde_json::from_value(
        json!({"typescript": {"node": "/abs/../node", "tsc_cli": "/abs/tsc.js"}}),
    )
    .unwrap();
    assert_eq!(bad.validate(), Err(LauncherError::Rejected));
}
