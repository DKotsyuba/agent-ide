//! Restart-only trusted launcher configuration, separate from host metadata and model arguments.

use crate::{
    checks::CheckConfig,
    intelligence::server::LanguageServer,
    lang::{Language, registered},
};
use serde::Deserialize;
use serde_json::Value;
use std::any::Any;
use std::sync::Arc;
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
    pub fn validate(&self) -> Result<(), LauncherError> {
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

/// A launcher object decoded as ordered `(key, value)` pairs that refuses a repeated key, exactly
/// as a closed struct refuses a duplicate field.
///
/// Used for the language-owned remainder of provider declarations and project-check sections,
/// whose keys are only known to the registered languages.
struct UniqueEntries(Vec<(String, Value)>);

impl<'de> Deserialize<'de> for UniqueEntries {
    /// Accepts any JSON object; a duplicate key is a decoding error.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Visits one object, collecting its entries in document order.
        struct Entries;
        impl<'de> serde::de::Visitor<'de> for Entries {
            type Value = UniqueEntries;
            /// Names the accepted shape in decoding errors.
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object")
            }
            /// Collects entries, refusing a key seen before.
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entries: Vec<(String, Value)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, Value>()? {
                    if entries.iter().any(|(existing, _)| *existing == key) {
                        return Err(serde::de::Error::custom(format_args!(
                            "duplicate field `{key}`"
                        )));
                    }
                    entries.push((key, value));
                }
                Ok(UniqueEntries(entries))
            }
        }
        deserializer.deserialize_map(Entries)
    }
}

/// Trusted provider identity; populated only by the restart-loaded launcher file.
///
/// The closed `settings` identifier selects the language whose server the declaration configures;
/// fields beyond the common ones below belong to that language's server, which decodes and
/// validates them (see [`LanguageServer::option_fields`]). A field only another registered server
/// accepts is refused at validation, and a field no registered server accepts at decoding.
#[derive(Clone)]
pub struct ProviderLaunch {
    /// Absolute executable and measured binary fingerprint.
    pub executable: AcceptedExecutable,
    /// Language whose server this declaration configures, selected by its settings identifier.
    pub language: Language,
    /// Accepted toolchain identity, validated by the server (an absolute executable, a rustup
    /// selector, or an interpreter identity).
    pub toolchain: String,
    /// Explicit operator trust identity, never derived from a sandbox observation.
    pub trust: String,
    /// Persistent compatible cache namespace, retained after stopping a view.
    pub cache_namespace: String,
    /// The server's decoded declaration fields; its concrete type belongs to the server.
    options: Arc<dyn Any + Send + Sync>,
    /// Whether the declaration carries a field that only another registered server accepts.
    foreign_fields: bool,
}

impl ProviderLaunch {
    /// Returns the server this declaration configures.
    pub fn server(&self) -> &'static dyn LanguageServer {
        self.language
            .server()
            .expect("a provider declaration resolves only to a language with a server")
    }

    /// Returns the server's decoded declaration fields when they are a `T`.
    pub fn options<T: Any>(&self) -> Option<&T> {
        self.options.downcast_ref::<T>()
    }
}

/// The common fields of one provider declaration plus the language-owned remainder.
#[derive(Deserialize)]
struct RawProviderLaunch {
    /// Absolute executable and measured binary fingerprint.
    executable: AcceptedExecutable,
    /// Closed settings identifier of one registered server.
    settings: String,
    /// Accepted toolchain identity.
    toolchain: String,
    /// Explicit operator trust identity.
    trust: String,
    /// Persistent compatible cache namespace.
    cache_namespace: String,
    /// Every other field, decoded by the servers that declare it.
    #[serde(flatten)]
    fields: UniqueEntries,
}

/// A settings identifier whose server was removed from the product, with the doctor sentence that
/// tells the operator its provider entry is ignored. Supplied once by the root application.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetiredSettings {
    /// The retired closed settings identifier.
    pub key: &'static str,
    /// One-line doctor explanation of the ignored entry.
    pub notice: &'static str,
}

/// Retired settings identifiers for this process; set once by [`install_retired_settings`].
static RETIRED: std::sync::OnceLock<&'static [RetiredSettings]> = std::sync::OnceLock::new();

/// Registers the settings identifiers whose provider entries are dropped before validation
/// instead of failing the whole launcher. Only the first call installs; call it before parsing a
/// launcher configuration.
pub fn install_retired_settings(retired: &'static [RetiredSettings]) {
    RETIRED.get_or_init(|| retired);
}

/// One decoded provider declaration: a launch, or an entry of a retired server that is ignored.
enum ProviderEntry {
    /// A registered server's declaration.
    Launch(Box<ProviderLaunch>),
    /// A declaration whose settings identifier was retired; its other fields are not decoded.
    Retired(&'static RetiredSettings),
}

impl<'de> Deserialize<'de> for ProviderEntry {
    /// Drops a retired server's declaration before registered-provider decoding, whatever its
    /// other fields hold; every non-retired entry is replayed from its exact text through
    /// [`ProviderLaunch`], so for those an unknown settings key and a repeated key at any depth stay invalid.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        /// Reads only the settings identifier of one declaration and ignores everything else.
        #[derive(Deserialize)]
        struct Probe {
            /// Closed settings identifier of the declaration.
            settings: Option<String>,
        }
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let probe: Probe = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        let retired = probe.settings.as_deref().and_then(|settings| {
            RETIRED
                .get()
                .copied()
                .unwrap_or(&[])
                .iter()
                .find(|retired| retired.key == settings)
        });
        match retired {
            Some(retired) => Ok(Self::Retired(retired)),
            None => serde_json::from_str(raw.get())
                .map(|launch| Self::Launch(Box::new(launch)))
                .map_err(D::Error::custom),
        }
    }
}

impl<'de> Deserialize<'de> for ProviderLaunch {
    /// Decodes the closed declaration schema of the registered servers.
    ///
    /// Fails (the launcher's `Invalid`) for an unknown settings identifier, a field no registered
    /// server declares, a repeated field, or a value the declaring server cannot decode. A JSON
    /// `null` field is treated as absent. A field another server declares is decoded by that
    /// server for shape only and marks the declaration for refusal at validation.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let raw = RawProviderLaunch::deserialize(deserializer)?;
        let language = registered()
            .iter()
            .copied()
            .find(|language| {
                language
                    .server()
                    .is_some_and(|server| server.settings_key() == raw.settings)
            })
            .ok_or_else(|| {
                D::Error::custom(format_args!("unknown provider settings `{}`", raw.settings))
            })?;
        let own = language.server().expect("selected by its server");
        let mut fields = serde_json::Map::new();
        let mut foreign_fields = false;
        for (key, value) in raw.fields.0 {
            if value.is_null() {
                continue;
            }
            let Some(owner) = registered()
                .iter()
                .filter_map(|language| language.server())
                .find(|server| server.option_fields().contains(&key.as_str()))
            else {
                return Err(D::Error::custom(format_args!("unknown field `{key}`")));
            };
            if own.option_fields().contains(&key.as_str()) {
                fields.insert(key, value);
            } else {
                let mut single = serde_json::Map::new();
                single.insert(key, value);
                owner.parse_options(single).map_err(D::Error::custom)?;
                foreign_fields = true;
            }
        }
        let options = own.parse_options(fields).map_err(D::Error::custom)?;
        Ok(Self {
            executable: raw.executable,
            language,
            toolchain: raw.toolchain,
            trust: raw.trust,
            cache_namespace: raw.cache_namespace,
            options,
            foreign_fields,
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

/// Optional timing and language declarations for confined background project checks.
///
/// Presence enables project checks only together with a nonempty [`LauncherConfig::allowed_roots`];
/// an absent language section means that language is never checked and never appears in a feed.
/// Each language section is keyed by the language's identifier and decoded by its
/// [`LanguageChecks`](crate::checks::LanguageChecks) integration.
#[derive(Clone)]
pub struct ProjectChecksConfig {
    /// Minimum milliseconds between an observed worktree change and a check start, 100..=10000.
    debounce_ms: u64,
    /// Seconds of zero leases and no running check before the shared daemon stops, 30..=3600.
    idle_timeout_s: u64,
    /// Total wall-clock ceiling of one check run including its tool startup, 10..=900.
    check_timeout_s: u64,
    /// Declared language sections in registration order.
    sections: Vec<(Language, Arc<dyn CheckConfig>)>,
    /// The same sections as written, for a language that interprets its section in its module.
    raw: Vec<(Language, serde_json::Value)>,
}

/// The closed timing fields of `project_checks` plus the language sections.
#[derive(Deserialize)]
struct RawProjectChecksConfig {
    /// See [`ProjectChecksConfig`].
    #[serde(default = "default_debounce_ms")]
    debounce_ms: u64,
    /// See [`ProjectChecksConfig`].
    #[serde(default = "default_idle_timeout_s")]
    idle_timeout_s: u64,
    /// See [`ProjectChecksConfig`].
    #[serde(default = "default_check_timeout_s")]
    check_timeout_s: u64,
    /// Language sections keyed by language identifier.
    #[serde(flatten)]
    sections: UniqueEntries,
}

impl<'de> Deserialize<'de> for ProjectChecksConfig {
    /// Decodes the timing fields and every section of a registered language with project checks.
    ///
    /// Fails (the launcher's `Invalid`) for a key that is neither a timing field nor such a
    /// language, a repeated key, or a section its language cannot decode. A `null` section is
    /// treated as absent.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let fields = RawProjectChecksConfig::deserialize(deserializer)?;
        let mut sections = Vec::new();
        let mut raw = Vec::new();
        for (key, value) in fields.sections.0 {
            let Some((language, checks)) = Language::by_id(&key)
                .and_then(|language| language.checks().map(|checks| (language, checks)))
            else {
                return Err(D::Error::custom(format_args!("unknown field `{key}`")));
            };
            if value.is_null() {
                continue;
            }
            raw.push((language, value.clone()));
            sections.push((
                language,
                checks.parse_config(value).map_err(D::Error::custom)?,
            ));
        }
        sections.sort_by_key(|(language, _)| *language);
        Ok(Self {
            debounce_ms: fields.debounce_ms,
            idle_timeout_s: fields.idle_timeout_s,
            check_timeout_s: fields.check_timeout_s,
            sections,
            raw,
        })
    }
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
    /// Returns `language`'s declared section; `None` means the language is never checked.
    pub fn section(&self, language: Language) -> Option<&dyn CheckConfig> {
        self.sections
            .iter()
            .find(|(declared, _)| *declared == language)
            .map(|(_, config)| &**config)
    }
    /// Returns `language`'s declared section as written.
    pub fn raw_section(&self, language: Language) -> Option<&serde_json::Value> {
        self.raw
            .iter()
            .find(|(declared, _)| *declared == language)
            .map(|(_, section)| section)
    }
    /// Iterates the declared sections in registration order.
    pub fn sections(&self) -> impl Iterator<Item = (Language, &dyn CheckConfig)> {
        self.sections
            .iter()
            .map(|(language, config)| (*language, &**config))
    }
    /// Checks the contract timing ranges and every declared section before any check can run.
    fn validate(&self) -> Result<(), LauncherError> {
        if !(100..=10_000).contains(&self.debounce_ms)
            || !(30..=3600).contains(&self.idle_timeout_s)
            || !(10..=900).contains(&self.check_timeout_s)
        {
            return Err(LauncherError::Rejected);
        }
        if self.sections.iter().any(|(_, config)| !config.validate()) {
            return Err(LauncherError::Rejected);
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
    /// At most one declaration per registered server; duplicate settings/languages are rejected.
    providers: Vec<ProviderEntry>,
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
    /// Retired settings identifiers whose provider entries were dropped, each once.
    retired: Vec<&'static RetiredSettings>,
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
    /// Validates bounded trusted JSON, including the ceiling of one provider per registered server,
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
        let mut retired: Vec<&'static RetiredSettings> = Vec::new();
        for target in raw.targets {
            let mut providers = Vec::new();
            for entry in target.providers {
                match entry {
                    ProviderEntry::Launch(launch) => providers.push(*launch),
                    ProviderEntry::Retired(entry) if !retired.contains(&entry) => {
                        retired.push(entry);
                    }
                    ProviderEntry::Retired(_) => {}
                }
            }
            if !identifier(&target.attachment)
                || target.attachment.len() > 128
                || !absolute(&target.candidate)
                || providers.len() > server_count()
            {
                return Err(LauncherError::Rejected);
            }
            target.git.validate()?;
            let mut provider_kinds = Vec::new();
            for provider in &providers {
                provider.executable.validate()?;
                if provider_kinds.contains(&provider.language)
                    || !identifier(&provider.toolchain)
                    || !identifier(&provider.trust)
                    || !identifier(&provider.cache_namespace)
                {
                    return Err(LauncherError::Rejected);
                }
                provider_kinds.push(provider.language);
                if provider.foreign_fields || !provider.server().validate_launch(provider) {
                    return Err(LauncherError::Rejected);
                }
            }
            let launch = LaunchTarget {
                candidate: target.candidate,
                git: target.git,
                providers,
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
            retired,
        })
    }
    /// The retired settings identifiers this file still declares; their entries were ignored.
    pub fn retired_settings(&self) -> &[&'static RetiredSettings] {
        &self.retired
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
                        .flat_map(|provider| provider.server().launch_executables(provider)),
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
        {
            provider.server().verify_launch(provider, cancel)?;
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
pub fn identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

/// Returns how many registered languages have a server: the ceiling of provider declarations per
/// target, since each server may be declared at most once.
fn server_count() -> usize {
    registered()
        .iter()
        .filter(|language| language.server().is_some())
        .count()
}

/// Rejects relative or lexically non-normal launcher paths without deriving them from cwd.
pub fn absolute(path: &Path) -> bool {
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

/// Legacy host execution fields remain accepted but do not appear in the launch target.
#[test]
fn launcher_ignores_legacy_execution_fields() {
    crate::lang::testing::install();
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
    crate::lang::testing::install();
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
