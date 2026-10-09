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
    contract::{
        Capability, Cause, HelloOffer, Limits, ModuleConfig, ModuleId, ModuleUnavailable, PROTOCOL,
        Role, Stage, VERSION,
    },
    contract::{Outcome, QUEUE_DEPTH},
    host::{Call, EffectRunner, NoEffects},
    launch::{ExecutionLauncher, ModuleExecutable, module_env},
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
    budget_or(REQUEST_BUDGET)
}

/// `default`, or the [`BUDGET_SEAM`] budget in a `test-seams` build that sets it: the per-call
/// budget of any module request (a hosted provider session's, a check's planning margin).
pub fn budget_or(default: Duration) -> Duration {
    crate::test_seams::var(BUDGET_SEAM)
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(default, |ms| Duration::from_millis(ms.clamp(100, 60_000)))
}

/// One supervised slot: its supervisor (one request in flight), the callers waiting for it and
/// the stop signal that cancels them.
struct SlotState {
    /// The instance's supervisor; holding it is having the one in-flight request.
    supervisor: tokio::sync::Mutex<Supervisor<ExecutionLauncher>>,
    /// Callers waiting for the supervisor, bounded by [`QUEUE_DEPTH`].
    waiting: std::sync::atomic::AtomicU32,
    /// Cancelled when the slot stops: in-flight and waiting calls end at once.
    stopping: tokio_util::sync::CancellationToken,
}

/// One supervised slot.
type Slot = Arc<SlotState>;

/// One caller counted as waiting for a slot until dropped, so a cancelled caller never leaks its
/// place in the bounded queue.
struct Waiter<'a> {
    /// The slot's waiter count.
    count: &'a std::sync::atomic::AtomicU32,
    /// Callers already waiting when this one entered.
    ahead: u32,
}

impl<'a> Waiter<'a> {
    /// Counts one more waiter on `count`.
    fn enter(count: &'a std::sync::atomic::AtomicU32) -> Self {
        let ahead = count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self { count, ahead }
    }
}

impl Drop for Waiter<'_> {
    /// Gives the place back.
    fn drop(&mut self) {
        self.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

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

/// The languages whose module ships ([`ship`]), plus the test seam's.
fn shipped_languages() -> BTreeSet<String> {
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
    shipped
}

/// One `<language> <mode>` entry per language of `languages` whose module ships — `module`, or
/// `in process (fallback)` when the switch sends it back —
/// joined with ` · `; `None` when none of them ships a module. `ignored` journal entries of the
/// switch follow. This is what a process started with this environment would do (`agent-ide
/// doctor`); a running daemon reports its own routing through [`ModuleHost::modes_line`].
pub fn effective_modes(languages: &[Language]) -> Option<String> {
    let shipped = shipped_languages();
    let modes = LanguageModes::from_env();
    modes_line(languages, &shipped, |language| modes.mode(language.name())).map(|line| {
        let ignored = modes.ignored_lines();
        if ignored.is_empty() {
            line
        } else {
            format!("{line} ({})", ignored.join(", "))
        }
    })
}

/// The modes line of `languages` that ship in `shipped`, with `mode` deciding each.
fn modes_line(
    languages: &[Language],
    shipped: &BTreeSet<String>,
    mode: impl Fn(Language) -> Mode,
) -> Option<String> {
    let entries: Vec<String> = languages
        .iter()
        .filter(|language| shipped.contains(language.name()))
        .map(|language| match mode(*language) {
            Mode::Module => format!("{language} module"),
            Mode::InProcess => format!("{language} in process (fallback)"),
        })
        .collect();
    (!entries.is_empty()).then(|| entries.join(" · "))
}

impl ModuleHost {
    /// Routing for the shipped languages ([`ship`]) with the daemon's `admission`, pinning the
    /// running executable and reading the fallback switch once.
    pub fn new(admission: Arc<Mutex<AdmissionController>>) -> Self {
        let shipped = shipped_languages();
        let extra_env = [super::serve::FAULT_SEAM, super::serve::FIXTURE_SEAM]
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

    /// The pinned module executable of `language`, or its typed refusal when the executable
    /// could not be measured.
    pub fn executable(
        &self,
        language: Language,
    ) -> Result<Arc<ModuleExecutable>, ModuleUnavailable> {
        self.executable.clone().ok_or_else(|| ModuleUnavailable {
            module_id: ModuleId::bundled(language.name()),
            module_version: env!("CARGO_PKG_VERSION").to_owned(),
            role: Role::Analyzer,
            stage: Stage::Spawn,
            cause: Cause::Incompatible,
            instance: None,
            retry_after_ms: None,
        })
    }

    /// This daemon's [`effective_modes`] line for `languages`.
    pub fn modes_line(&self, languages: &[Language]) -> Option<String> {
        modes_line(languages, &self.shipped, |language| self.mode(language))
    }

    /// Where `language` computes in this daemon.
    pub fn mode(&self, language: Language) -> Mode {
        // Only the shipped set and the explicit switch decide; a module that cannot be pinned or
        // started stays in module mode and answers a typed `module_unavailable`.
        if self.shipped.contains(language.name())
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
            env: module_env(language.name()),
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
        let slot = Arc::new(SlotState {
            supervisor: tokio::sync::Mutex::new(Supervisor::new(launcher, offer, STARTUP_BUDGET)),
            waiting: std::sync::atomic::AtomicU32::new(0),
            stopping: tokio_util::sync::CancellationToken::new(),
        });
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
        let call = Call {
            capability,
            scope_key: worktree.display().to_string(),
            revision_key: String::new(),
            payload,
            attachments,
        };
        self.call(
            language,
            worktree,
            Role::Analyzer,
            call,
            self.budget,
            &mut NoEffects,
        )
        .await
    }

    /// Sends one `call` to the `role` instance of `language` in `worktree` within `budget`,
    /// serving its effects with `effects`, and decodes the typed result like [`Self::request`].
    pub async fn call<T: DeserializeOwned>(
        &self,
        language: Language,
        worktree: &Path,
        role: Role,
        call: Call,
        budget: Duration,
        effects: &mut dyn EffectRunner,
    ) -> Result<T, ModuleUnavailable> {
        let failure = |cause| ModuleUnavailable {
            module_id: ModuleId::bundled(language.name()),
            module_version: env!("CARGO_PKG_VERSION").to_owned(),
            role,
            stage: Stage::Decode,
            cause,
            instance: None,
            retry_after_ms: None,
        };
        // One deadline covers the wait for the instance and the call itself.
        let deadline = tokio::time::Instant::now() + budget;
        let slot = self.slot(language, worktree, role).await?;
        let waiter = Waiter::enter(&slot.waiting);
        let waited = if waiter.ahead >= QUEUE_DEPTH {
            Err(Stage::Admission)
        } else {
            tokio::select! {
                supervisor = tokio::time::timeout_at(deadline, slot.supervisor.lock()) => {
                    supervisor.map_err(|_| Stage::Request)
                }
                _ = slot.stopping.cancelled() => Err(Stage::Request),
            }
        };
        drop(waiter);
        let mut supervisor = match waited {
            Ok(supervisor) => supervisor,
            Err(Stage::Admission) => {
                return Err(ModuleUnavailable {
                    stage: Stage::Admission,
                    ..failure(Cause::ResourceLimit)
                });
            }
            Err(_) if slot.stopping.is_cancelled() => {
                return Err(ModuleUnavailable {
                    stage: Stage::Request,
                    ..failure(Cause::Exited)
                });
            }
            Err(stage) => {
                return Err(ModuleUnavailable {
                    stage,
                    ..failure(Cause::Timeout)
                });
            }
        };
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let reply = tokio::select! {
            reply = supervisor.call(call, remaining, effects) => reply?,
            _ = slot.stopping.cancelled() => {
                return Err(ModuleUnavailable {
                    stage: Stage::Request,
                    ..failure(Cause::Exited)
                });
            }
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

    /// The linkage coverage the live analyzer instance of `language` in `worktree` declared in
    /// its `hello`; `None` while none is live.
    pub async fn linkage(
        &self,
        language: Language,
        worktree: &Path,
    ) -> Option<Vec<super::payload::LinkageCoverage>> {
        let key = (
            language.name().to_owned(),
            worktree.to_path_buf(),
            Role::Analyzer,
        );
        let slot = self.slots.lock().await.get(&key)?.clone();
        let supervisor = slot.supervisor.lock().await;
        supervisor.linkage().map(<[_]>::to_vec)
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
            // Cancel the in-flight and waiting calls first, so the stop never queues behind them.
            slot.stopping.cancel();
            let _ = slot.supervisor.lock().await.stop().await;
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
            // Cancel the in-flight and waiting calls first, so the stop never queues behind them.
            slot.stopping.cancel();
            let _ = slot.supervisor.lock().await.stop().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::testing::{ALPHA, BETA};

    /// Only languages whose module ships are named, each with where it computes; with none
    /// shipped there is no line.
    #[test]
    fn modes_name_shipped_languages_only() {
        crate::lang::testing::install();
        let shipped: BTreeSet<String> = ["alpha".to_owned()].into();
        assert_eq!(
            modes_line(&[ALPHA, BETA], &shipped, |_| Mode::Module).as_deref(),
            Some("alpha module")
        );
        assert_eq!(
            modes_line(&[ALPHA, BETA], &shipped, |_| Mode::InProcess).as_deref(),
            Some("alpha in process (fallback)")
        );
        assert_eq!(modes_line(&[BETA], &shipped, |_| Mode::Module), None);
    }
}
