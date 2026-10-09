//! Daemon-side routing of language computations: in process through the language's own traits,
//! or to its bundled module through one supervised instance per `(language, worktree, role)`.
//!
//! A language runs in module mode only when its module ships in this release, the fallback switch
//! does not send it back in process, and the daemon pinned its own executable; nothing else
//! changes the choice, and a module fault never falls back silently. Every routed operation is
//! async: callers await a module reply instead of blocking a worker thread.

use std::{
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::de::DeserializeOwned;

use super::{
    contract::Outcome,
    contract::{
        Capability, Cause, HelloOffer, Limits, ModuleConfig, ModuleId, ModuleUnavailable, PROTOCOL,
        Role, Stage, VERSION,
    },
    host::{Call, NoEffects},
    launch::{ExecutionLauncher, MODULE_ENV, ModuleExecutable},
    mode::{LanguageModes, Mode},
    payload::{decode, encode},
    runtime::Supervisor,
    wire::Attachment,
};
use crate::{
    execution::{AdmissionController, OwnerId},
    lang::Language,
};

/// Test seam naming languages whose module counts as shipped (`alpha,beta`); honoured only in
/// `test-seams` builds.
pub const SHIPPED_SEAM: &str = "AGENT_IDE_TEST_MODULE_LANGUAGES";
/// Spawn-to-`hello` ceiling of one instance.
const STARTUP_BUDGET: Duration = Duration::from_secs(30);
/// Default budget of one ordinary request.
pub const REQUEST_BUDGET: Duration = Duration::from_secs(20);
/// Test seam overriding [`REQUEST_BUDGET`] in milliseconds (100..=60000); honoured only in
/// `test-seams` builds.
pub const BUDGET_SEAM: &str = "AGENT_IDE_TEST_MODULE_BUDGET_MS";

/// The ordinary request budget, or the seam's.
fn request_budget() -> Duration {
    crate::test_seams::var(BUDGET_SEAM)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(REQUEST_BUDGET, |ms| {
            Duration::from_millis(ms.clamp(100, 60_000))
        })
}

/// One supervised slot.
type Slot = Arc<tokio::sync::Mutex<Supervisor<ExecutionLauncher>>>;

/// The daemon's module routing state.
pub struct ModuleHost {
    /// The pinned executable, `None` when it could not be measured (everything stays in process).
    executable: Option<Arc<ModuleExecutable>>,
    /// The fallback switch, read once.
    modes: LanguageModes,
    /// Languages whose module ships default-on in this release.
    shipped: BTreeSet<String>,
    /// The daemon's single admission controller.
    admission: Arc<Mutex<AdmissionController>>,
    /// Live slots.
    slots: tokio::sync::Mutex<HashMap<(String, PathBuf, Role), Slot>>,
    /// Extra variables every module receives (test seams).
    extra_env: Vec<(String, String)>,
    /// Budget of one ordinary request.
    budget: Duration,
}

/// Languages whose module ships default-on in this release, declared once by the root.
static SHIPPED: std::sync::OnceLock<&'static [&'static str]> = std::sync::OnceLock::new();

/// Declares the languages whose module ships default-on (root composition data); later calls
/// keep the first declaration.
pub fn ship(languages: &'static [&'static str]) {
    let _ = SHIPPED.set(languages);
}

impl ModuleHost {
    /// Routing for the shipped languages ([`ship`]) with the daemon's `admission`, pinning the
    /// running executable and reading the fallback switch once.
    pub fn new(admission: Arc<Mutex<AdmissionController>>) -> Self {
        let mut shipped: BTreeSet<String> = SHIPPED
            .get()
            .copied()
            .unwrap_or_default()
            .iter()
            .map(|id| (*id).to_owned())
            .collect();
        if let Some(seam) = crate::test_seams::var(SHIPPED_SEAM) {
            shipped.extend(
                seam.split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned),
            );
        }
        let extra_env = [super::serve::FAULT_SEAM]
            .into_iter()
            .filter_map(|seam| Some((seam.to_owned(), crate::test_seams::var(seam)?)))
            .collect();
        Self {
            executable: ModuleExecutable::current().map(Arc::new),
            modes: LanguageModes::from_env(),
            shipped,
            admission,
            slots: tokio::sync::Mutex::default(),
            extra_env,
            budget: request_budget(),
        }
    }

    /// Routing with every part explicit: the module `executable`, the `shipped` languages, extra
    /// module environment and the request `budget` (conformance tests drive real modules this
    /// way without a daemon). The fallback switch is not read.
    pub fn with_parts(
        executable: ModuleExecutable,
        admission: Arc<Mutex<AdmissionController>>,
        shipped: &[&str],
        extra_env: Vec<(String, String)>,
        budget: Duration,
    ) -> Self {
        Self {
            executable: Some(Arc::new(executable)),
            modes: LanguageModes::default(),
            shipped: shipped.iter().map(|id| (*id).to_owned()).collect(),
            admission,
            slots: tokio::sync::Mutex::default(),
            extra_env,
            budget,
        }
    }

    /// The journal lines of ignored switch entries (`language_mode_ignored:<id>`).
    pub fn ignored_lines(&self) -> Vec<String> {
        self.modes.ignored_lines()
    }

    /// Where `language` computes in this daemon.
    pub fn mode(&self, language: Language) -> Mode {
        if self.executable.is_some()
            && self.shipped.contains(language.name())
            && self.modes.mode(language.name()) == Mode::Module
        {
            Mode::Module
        } else {
            Mode::InProcess
        }
    }

    /// The slot of `(language, worktree, role)`, created stopped on first use.
    async fn slot(
        &self,
        language: Language,
        worktree: &Path,
        role: Role,
    ) -> Result<Slot, ModuleUnavailable> {
        let module_id = ModuleId::bundled(language.name());
        let version = env!("CARGO_PKG_VERSION");
        let Some(executable) = self.executable.clone() else {
            return Err(ModuleUnavailable {
                module_id,
                module_version: version.to_owned(),
                role,
                stage: Stage::Spawn,
                cause: Cause::Incompatible,
                instance: None,
                retry_after_ms: None,
            });
        };
        let key = (language.name().to_owned(), worktree.to_path_buf(), role);
        let mut slots = self.slots.lock().await;
        if let Some(slot) = slots.get(&key) {
            return Ok(slot.clone());
        }
        let config = ModuleConfig {
            worktree: Some(worktree.to_path_buf()),
            provider: None,
            checks: None,
            env: MODULE_ENV
                .iter()
                .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
                .collect(),
            home: crate::userhome::user_home(),
        };
        let owner = OwnerId::new(format!(
            "module:{}:{}",
            language.name(),
            blake3::hash(worktree.as_os_str().as_encoded_bytes()).to_hex()
        ))
        .expect("a nonempty owner");
        let mut launcher = ExecutionLauncher::new(
            executable.clone(),
            self.admission.clone(),
            owner,
            language.name(),
            role,
            worktree.to_path_buf(),
            encode(&config).to_string(),
        );
        for (key, value) in &self.extra_env {
            launcher = launcher.with_env(key, value);
        }
        let offer = HelloOffer {
            protocol: PROTOCOL.to_owned(),
            versions: vec![VERSION],
            module_id,
            package_version: version.to_owned(),
            executable_digest: executable.digest.to_hex().to_string(),
            instance: 0,
            role,
            limits: Limits::default(),
            requested_caps: Vec::new(),
            config,
        };
        let slot = Arc::new(tokio::sync::Mutex::new(Supervisor::new(
            launcher,
            offer,
            STARTUP_BUDGET,
        )));
        slots.insert(key, slot.clone());
        Ok(slot)
    }

    /// Sends one `capability` request with `payload` for `language` in `worktree` and decodes the
    /// typed result. A module refusal (`unsupported`, `warming`, ...) is returned as the module's
    /// typed failure at the request stage with its sanitized cause.
    pub async fn request<T: DeserializeOwned>(
        &self,
        language: Language,
        worktree: &Path,
        capability: Capability,
        payload: serde_json::Value,
        attachments: Vec<Attachment>,
    ) -> Result<T, ModuleUnavailable> {
        let slot = self.slot(language, worktree, Role::Analyzer).await?;
        let mut supervisor = slot.lock().await;
        let call = Call {
            capability,
            scope_key: worktree.display().to_string(),
            revision_key: String::new(),
            payload,
            attachments,
        };
        let reply = supervisor.call(call, self.budget, &mut NoEffects).await?;
        let failure = |cause| ModuleUnavailable {
            module_id: ModuleId::bundled(language.name()),
            module_version: env!("CARGO_PKG_VERSION").to_owned(),
            role: Role::Analyzer,
            stage: Stage::Decode,
            cause,
            instance: None,
            retry_after_ms: None,
        };
        match reply.outcome {
            Outcome::Result(value) => decode(value).map_err(|_| failure(Cause::Malformed)),
            Outcome::Error(error) => Err(ModuleUnavailable {
                stage: Stage::Request,
                ..failure(match error.code {
                    super::contract::ErrorCode::ToolMissing => Cause::ToolMissing,
                    super::contract::ErrorCode::InvalidRequest => Cause::Malformed,
                    super::contract::ErrorCode::Busy => Cause::ResourceLimit,
                    _ => Cause::PolicyRefused,
                })
            }),
        }
    }

    /// Stops every slot of `worktree` (view release, binding stop) in order.
    pub async fn stop_worktree(&self, worktree: &Path) {
        let stopping: Vec<Slot> = {
            let mut slots = self.slots.lock().await;
            let keys: Vec<_> = slots
                .keys()
                .filter(|(_, root, _)| root == worktree)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| slots.remove(&key))
                .collect()
        };
        for slot in stopping {
            let _ = slot.lock().await.stop().await;
        }
    }

    /// Stops every slot (daemon shutdown).
    pub async fn stop_all(&self) {
        let stopping: Vec<Slot> = self
            .slots
            .lock()
            .await
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        for slot in stopping {
            let _ = slot.lock().await.stop().await;
        }
    }
}
