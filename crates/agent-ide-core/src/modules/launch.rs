//! The production [`Launcher`]: admits a module instance through the daemon's central Execution
//! admission and starts the daemon's own pinned executable in hidden `module <language> <role>`
//! mode with a cleared environment.
//!
//! The executable is measured once at daemon start ([`ModuleExecutable::current`]); every spawn
//! re-measures the file and refuses a mismatch (`incompatible`, which blocks the slot until the
//! inputs change). Stop gives the module a moment to exit on its own, then tears its owned
//! process group down and reaps it within 5 s; only a proven reap releases the admission slot.
//! On Linux, once the leader has exited, remaining group members receive TERM and then KILL
//! without a grace period. Otherwise the TERM-to-KILL grace is 1 s.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use super::{
    contract::{Cause, Role, Stage},
    runtime::{Launcher, Spawned},
};
use crate::execution::{
    Admission, AdmissionClass, AdmissionController, CommandKind, ControlledCommand,
    OwnedProtocolChild, OwnerId, measured_executable_digest,
};

/// Environment variable names each language's module receives, declared by the root.
static DECLARED_ENV: std::sync::RwLock<Vec<(&'static str, &'static [&'static str])>> =
    std::sync::RwLock::new(Vec::new());

/// Declares the environment variable names languages' modules receive (root composition data);
/// a language already declared keeps its first declaration.
pub fn declare_env(names: &'static [(&'static str, &'static [&'static str])]) {
    let mut declared = DECLARED_ENV
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (language, names) in names {
        if !declared.iter().any(|(known, _)| known == language) {
            declared.push((language, names));
        }
    }
}

/// The environment `language`'s module receives — in its cleared process environment and in
/// `hello.config.env` — : each name its descriptor declares that the daemon has set. Nothing else
/// is inherited; the real user home travels as `hello.config.home`.
pub fn module_env(language: &str) -> BTreeMap<String, String> {
    DECLARED_ENV
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|(declared, _)| *declared == language)
        .flat_map(|(_, names)| names.iter())
        .filter_map(|name| Some(((*name).to_owned(), std::env::var(name).ok()?)))
        .collect()
}

/// Retained stderr bytes per instance.
const STDERR_CAPTURE: usize = super::runtime::STDERR_CAPTURE;
/// How long a stopping module may take to exit on its own before its group is signalled.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// TERM-to-KILL grace of the group teardown.
const TERM_GRACE: Duration = Duration::from_secs(1);
/// Ceiling of the direct reap and stderr drain.
const REAP_DEADLINE: Duration = Duration::from_secs(5);

/// The daemon's own executable, pinned by digest.
#[derive(Clone, Debug)]
pub struct ModuleExecutable {
    /// Absolute path.
    pub path: PathBuf,
    /// Digest measured when pinned.
    pub digest: blake3::Hash,
}

impl ModuleExecutable {
    /// Pins the running executable; `None` when it cannot be resolved or measured.
    pub fn current() -> Option<Self> {
        let path = std::env::current_exe().ok()?.canonicalize().ok()?;
        Self::pin(&path)
    }

    /// Pins `path` by its current digest.
    pub fn pin(path: &Path) -> Option<Self> {
        Some(Self {
            path: path.to_path_buf(),
            digest: measured_executable_digest(path).ok()?,
        })
    }
}

/// Starts instances of one `(language, scope, role)` slot.
pub struct ExecutionLauncher {
    /// The pinned executable.
    executable: Arc<ModuleExecutable>,
    /// The daemon's single admission controller; locked only in synchronous blocks.
    admission: Arc<Mutex<AdmissionController>>,
    /// Accounting owner of this slot.
    owner: OwnerId,
    /// Admission class: interactive for analyzers, background for checkers.
    class: AdmissionClass,
    /// Language id.
    language: String,
    /// Role.
    role: Role,
    /// Working directory (the worktree).
    cwd: PathBuf,
    /// The cleared environment the module receives.
    env: BTreeMap<OsString, OsString>,
    /// Accepted configuration fingerprint (beside the executable digest).
    config: String,
}

impl ExecutionLauncher {
    /// A launcher for `language`'s `role` instance in `cwd`, accounted to `owner`, receiving only
    /// its declared environment ([`module_env`]).
    pub fn new(
        executable: Arc<ModuleExecutable>,
        admission: Arc<Mutex<AdmissionController>>,
        owner: OwnerId,
        language: &str,
        role: Role,
        cwd: PathBuf,
        config: String,
    ) -> Self {
        let env = module_env(language)
            .into_iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect();
        Self {
            executable,
            admission,
            owner,
            class: match role {
                Role::Analyzer => AdmissionClass::Interactive,
                Role::Checker => AdmissionClass::Background,
            },
            language: language.to_owned(),
            role,
            cwd,
            env,
            config,
        }
    }

    /// Adds one test-seam variable to the module's environment.
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }
}

impl Launcher for ExecutionLauncher {
    type Process = OwnedProtocolChild;

    /// Re-measures the executable, takes a direct admission slot without queueing, and spawns.
    async fn launch(&mut self, _instance: u64) -> Result<Spawned<Self::Process>, (Stage, Cause)> {
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Job,
            self.executable.path.clone(),
            vec![
                OsString::from("module"),
                OsString::from(&self.language),
                OsString::from(self.role.name()),
            ],
            self.cwd.clone(),
            self.env.clone(),
        )
        .map_err(|_| (Stage::Spawn, Cause::Incompatible))?;
        if !command.has_program_digest(&self.executable.digest) {
            return Err((Stage::Spawn, Cause::Incompatible));
        }
        let lease = {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match admission.submit(self.owner.clone(), self.class) {
                Admission::Granted(lease) => lease,
                Admission::Queued(ticket) => {
                    admission.cancel_ticket(ticket);
                    return Err((Stage::Admission, Cause::ResourceLimit));
                }
                Admission::Refused(_) => return Err((Stage::Admission, Cause::ResourceLimit)),
            }
        };
        let mut child = OwnedProtocolChild::spawn_module(
            &command,
            lease,
            &self.executable.digest,
            STDERR_CAPTURE,
        )
        .map_err(|error| {
            crate::execution::job::settle(&self.admission, error);
            (Stage::Spawn, Cause::Exited)
        })?;
        let Some((stdin, stdout)) = child.take_pipes() else {
            return Err((Stage::Spawn, Cause::Exited));
        };
        Ok(Spawned {
            stdout: Box::new(stdout),
            stdin: Box::new(stdin),
            stderr: None,
            process: child,
        })
    }

    /// Waits up to `EXIT_GRACE` for a voluntary exit (without reaping), then tears the owned
    /// group down and reaps; a proven reap releases the admission slot. On Linux an observed
    /// exited leader permits immediate escalation against any remaining group members while its
    /// unreaped PID still pins the group; an unobserved exit retains the normal TERM grace.
    async fn reap(&mut self, process: Self::Process) -> Result<(), Cause> {
        let until = tokio::time::Instant::now() + EXIT_GRACE;
        while !process.exited() && tokio::time::Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        #[cfg(target_os = "linux")]
        let grace = if process.exited() {
            Duration::ZERO
        } else {
            TERM_GRACE
        };
        #[cfg(not(target_os = "linux"))]
        let grace = TERM_GRACE;
        let reaped = process
            .cancel_and_reap(grace, REAP_DEADLINE)
            .await
            .map_err(|_| Cause::ReapUnverified)?;
        self.admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .release_reaped(reaped.proof)
            .map(|_| ())
            .map_err(|_| Cause::ReapUnverified)
    }

    /// The executable digest and the configuration fingerprint.
    fn inputs(&self) -> String {
        format!("{}:{}", self.executable.digest.to_hex(), self.config)
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// A module receives exactly the variables its language declares that the daemon has set;
    /// an undeclared language receives none.
    #[test]
    fn modules_receive_only_their_declared_environment() {
        declare_env(&[(
            "env-test-language",
            &["HOME", "AGENT_IDE_NEVER_SET_FOR_TESTS"],
        )]);
        let env = module_env("env-test-language");
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            ["HOME"],
            "declared and set only"
        );
        assert!(module_env("env-test-undeclared").is_empty());
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_reap_tests {
    use super::*;
    use crate::execution::AdmissionLimits;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    /// Starts a real owned process with the same admission and reap path as a module.
    fn fixture(script: &str) -> (ExecutionLauncher, OwnedProtocolChild) {
        let executable = Arc::new(ModuleExecutable::pin(Path::new("/bin/sh")).unwrap());
        let admission = Arc::new(Mutex::new(
            AdmissionController::new(AdmissionLimits {
                total_running: 1,
                per_owner_running: 1,
                per_owner_queued: 1,
                total_queued: 1,
                interactive_burst: 1,
            })
            .unwrap(),
        ));
        let owner = OwnerId::new("module-reap-test").unwrap();
        let lease = match admission
            .lock()
            .unwrap()
            .submit(owner.clone(), AdmissionClass::Interactive)
        {
            Admission::Granted(lease) => lease,
            _ => panic!("fixture admission"),
        };
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Job,
            executable.path.clone(),
            vec!["-c".into(), script.into()],
            PathBuf::from("/"),
            BTreeMap::new(),
        )
        .unwrap();
        let child =
            OwnedProtocolChild::spawn_module(&command, lease, &executable.digest, 64).unwrap();
        let launcher = ExecutionLauncher::new(
            executable,
            admission,
            owner,
            "fixture",
            Role::Analyzer,
            PathBuf::from("/"),
            String::new(),
        );
        (launcher, child)
    }

    /// Waits for WNOWAIT evidence without allowing the leader's PID to be reused.
    async fn await_exit(child: &OwnedProtocolChild) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !child.exited() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture leader exited");
    }

    /// An already exited module settles before the service stop deadline.
    #[tokio::test]
    async fn exited_module_reaps_without_zombie_group_delay() {
        let (mut launcher, child) = fixture("exit 0");
        await_exit(&child).await;
        tokio::time::timeout(Duration::from_millis(700), launcher.reap(child))
            .await
            .expect("exited module reap should be prompt")
            .unwrap();
        assert_eq!(launcher.admission.lock().unwrap().running_count(), 0);
    }

    /// A TERM-resistant child remains in the pinned group and loses its output pipe on KILL.
    #[tokio::test]
    async fn exited_module_kills_term_resistant_descendant() {
        let (mut launcher, mut child) = fixture(
            "/bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\"; while :; do /bin/sleep 1; done' & exit 0",
        );
        let (stdin, stdout) = child.take_pipes().unwrap();
        drop(stdin);
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(2), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let descendant: u32 = line.trim().parse().unwrap();
        assert_ne!(descendant, 0);
        await_exit(&child).await;
        tokio::time::timeout(Duration::from_millis(700), launcher.reap(child))
            .await
            .expect("descendant cleanup should be prompt")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), stdout.read_to_end(&mut Vec::new()))
            .await
            .expect("descendant retained its output pipe")
            .unwrap();
    }
}
