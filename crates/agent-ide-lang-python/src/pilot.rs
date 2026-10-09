//! M-011 phase-2b pilot: the Python analyzer (Pyright session) and checker (Pyright CLI) as
//! external module processes behind the core's throw-away framed-stdio boundary
//! (`agent_ide_core::intelligence::pilot`). Selected only by `AGENT_IDE_PILOT_MODULE=python`.
//!
//! Both modules are this same sealed binary in the hidden `module-pilot` mode. The analyzer is
//! spawned through the core's Execution-owned provider child path and starts Pyright itself; the
//! checker runs the unchanged [`PythonChecker`] whose every confined process is handed back to
//! the core, so Seatbelt confinement and the effect policy stay core-owned.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    io,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use agent_ide_core::{
    assistance::launcher::AcceptedExecutable,
    checks::{
        BoxFuture, CheckRequest, Checker, ProblemSnapshot, UnavailableReason,
        runner::{ConfinedRunner, RunOutput, RunSpec},
    },
    execution::{CommandKind, ControlledCommand},
    intelligence::{
        pilot::{self, read_frame, write_frame},
        session::ProviderSettings,
    },
    lang::Language,
};

use crate::{
    checks::PythonChecker,
    profile::{PyrightProfile, PyrightProfileIdentity, PyrightWorktree},
};

/// Whether the pilot flag selects Python.
pub(crate) fn enabled() -> bool {
    pilot::enabled(crate::DESCRIPTOR.id)
}

/// Pyright provider parameters the analyzer module receives in its `hello`; the module
/// re-verifies both digests before it starts anything.
#[derive(Serialize, Deserialize)]
pub(crate) struct ProviderParams {
    /// Accepted Pyright language-server script.
    pub(crate) binary: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub(crate) script_digest: String,
    /// Accepted Pyright version identity.
    pub(crate) version: String,
    /// Accepted Node executable.
    pub(crate) node: PathBuf,
    /// Its accepted BLAKE3 digest, hex.
    pub(crate) node_digest: String,
    /// Accepted Node identity.
    pub(crate) node_identity: String,
    /// Operator trust identity.
    pub(crate) trust: String,
    /// Private per-worktree cache namespace.
    pub(crate) cache_namespace: String,
    /// Interpreter resolved by the core for this worktree.
    pub(crate) interpreter: Option<PathBuf>,
    /// That resolution's identity.
    pub(crate) environment: String,
    /// Import roots beyond the worktree root.
    pub(crate) extra_paths: Vec<PathBuf>,
}

impl ProviderParams {
    /// Rebuilds and re-verifies the Pyright profile.
    fn profile(&self) -> io::Result<PyrightProfile> {
        let digest =
            |hex: &str| blake3::Hash::from_hex(hex).map_err(|_| io::Error::other("invalid digest"));
        Ok(PyrightProfile::new(PyrightProfileIdentity {
            binary: self.binary.clone(),
            accepted_script_digest: digest(&self.script_digest)?,
            version: self.version.clone(),
            node: self.node.clone(),
            accepted_node_digest: digest(&self.node_digest)?,
            node_identity: self.node_identity.clone(),
            trust: self.trust.clone(),
            cache_namespace: self.cache_namespace.clone(),
        })
        .map_err(|_| io::Error::other("pyright profile refused"))?
        .with_interpreter(self.interpreter.clone(), self.environment.clone())
        .with_extra_paths(self.extra_paths.clone()))
    }
}

/// This binary as the accepted module executable, measured once per daemon; `None` when it
/// cannot be measured.
pub(crate) fn module_executable() -> Option<&'static AcceptedExecutable> {
    static EXECUTABLE: OnceLock<Option<AcceptedExecutable>> = OnceLock::new();
    EXECUTABLE
        .get_or_init(|| {
            let path = std::env::current_exe().ok()?;
            let digest = agent_ide_core::execution::measured_executable_digest(&path).ok()?;
            Some(AcceptedExecutable {
                path,
                identity: "agent-ide-module-pilot".to_owned(),
                blake3: digest.to_hex().to_string(),
            })
        })
        .as_ref()
}

/// The Execution command that starts the analyzer module in `worktree`, with only `TMPDIR` (and
/// the fault seam in test builds) in its environment. Refused when the binary changed since it
/// was measured.
pub(crate) fn analyzer_command(
    executable: &AcceptedExecutable,
    worktree: &PyrightWorktree,
    cache_namespace: &str,
) -> Option<ControlledCommand> {
    let mut env = BTreeMap::from([(
        OsString::from("TMPDIR"),
        OsString::from(std::path::Path::new(cache_namespace).join("tmp")),
    )]);
    if let Some(seam) = agent_ide_core::test_seams::var(pilot::FAULT_SEAM) {
        env.insert(OsString::from(pilot::FAULT_SEAM), OsString::from(seam));
    }
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        executable.path.clone(),
        vec![OsString::from("module-pilot"), OsString::from("analyzer")],
        worktree.worktree().worktree_path().to_path_buf(),
        env,
    )
    .ok()?;
    let digest = blake3::Hash::from_hex(&executable.blake3).ok()?;
    command.has_program_digest(&digest).then_some(command)
}

/// Entry point of `agent-ide module-pilot <analyzer|checker>`: serves one module over this
/// process's stdin/stdout until stdin closes.
pub async fn run_module(role: &str) -> io::Result<()> {
    match role {
        "analyzer" => {
            pilot::serve_analyzer(|provider| {
                let params: ProviderParams = pilot::decode(provider.clone())?;
                let profile = params.profile()?;
                let node_parent = params
                    .node
                    .parent()
                    .ok_or_else(|| io::Error::other("node has no parent"))?;
                let mut command = tokio::process::Command::new(&params.node);
                command
                    .arg(&params.binary)
                    .arg("--stdio")
                    .env_clear()
                    .env("PATH", node_parent)
                    .env(
                        "TMPDIR",
                        std::path::Path::new(&params.cache_namespace).join("tmp"),
                    );
                Ok((ProviderSettings::new(profile), command))
            })
            .await
        }
        "checker" => serve_checker().await,
        _ => Err(io::Error::other("unknown module role")),
    }
}

/// Module side of the checker: answers each `check` with the [`PythonChecker`] snapshot, its
/// confined runs requested from the core through `run` frames.
async fn serve_checker() -> io::Result<()> {
    let channel = Arc::new(Mutex::new((tokio::io::stdin(), tokio::io::stdout())));
    loop {
        let request = {
            let mut channel = channel.lock().await;
            match read_frame(&mut channel.0).await {
                Ok(request) => request,
                Err(_) => return Ok(()),
            }
        };
        if let Some(kind) = pilot::fault_seam("check") {
            let mut channel = channel.lock().await;
            pilot::act_fault(&kind, &mut channel.1).await?;
            continue;
        }
        let params = &request["params"];
        let check: CheckRequest = pilot::decode(params["request"].clone())?;
        let node: PathBuf = pilot::decode(params["node"].clone())?;
        let cli: PathBuf = pilot::decode(params["cli"].clone())?;
        let timeout = Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(1));
        let runner = Arc::new(CallbackRunner(channel.clone()));
        let snapshot = PythonChecker::new(runner, node, cli, timeout)
            .check(check)
            .await;
        let mut channel = channel.lock().await;
        write_frame(
            &mut channel.1,
            &json!({"id": request["id"], "result": snapshot}),
        )
        .await?;
    }
}

/// Module-side runner that asks the core to execute each confined run.
struct CallbackRunner(Arc<Mutex<(tokio::io::Stdin, tokio::io::Stdout)>>);

impl ConfinedRunner for CallbackRunner {
    /// Sends `run` and waits for the core's `ran` reply. The `widen:run` fault seam proposes a
    /// write root that escapes the check cache, which the core must refuse.
    fn run(&self, mut spec: RunSpec) -> BoxFuture<'_, io::Result<RunOutput>> {
        Box::pin(async move {
            if pilot::fault_seam("run").is_some() {
                let escaped = spec.write_roots[0].join("../outside");
                spec.write_roots.push(escaped);
            }
            let mut channel = self.0.lock().await;
            write_frame(&mut channel.1, &json!({"method": "run", "params": spec})).await?;
            let mut reply = read_frame(&mut channel.0).await?;
            if let Some(error) = reply.get("error") {
                return Err(io::Error::other(
                    error.as_str().unwrap_or("run refused").to_owned(),
                ));
            }
            pilot::decode(reply["result"].take())
        })
    }
}

/// One running checker module.
struct CheckerModule {
    /// The module process (own process group, killed on drop).
    child: tokio::process::Child,
    /// Its stdout.
    input: tokio::process::ChildStdout,
    /// Its stdin.
    output: tokio::process::ChildStdin,
    /// Last check id sent to this instance.
    next_id: u64,
    /// True while a check has not settled; a dropped (cancelled) check leaves it set, and the
    /// next check replaces the instance so no late frame can answer it.
    in_flight: bool,
}

/// Most confined runs one check may request (one per Python root).
const MAX_RUNS: u32 = 32;

/// Core-side [`Checker`] that delegates Python checks to a long-lived checker module and runs
/// every confined process the module asks for on the core's own runner, but only when it is
/// exactly a run the core itself would have built for this request (see `permitted`).
pub(crate) struct PilotChecker {
    /// The core's confined runner (Seatbelt with the nested-sandbox fallback).
    runner: Arc<dyn ConfinedRunner>,
    /// Pinned Node executable.
    node: PathBuf,
    /// Pinned Pyright CLI.
    cli: PathBuf,
    /// Per-run timeout.
    timeout: Duration,
    /// The module, started on first use and replaced after any fault.
    module: Mutex<Option<CheckerModule>>,
}

impl PilotChecker {
    /// Builds a checker whose module starts on the first check.
    pub(crate) fn new(
        runner: Arc<dyn ConfinedRunner>,
        node: PathBuf,
        cli: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            node,
            cli,
            timeout,
            module: Mutex::new(None),
        }
    }

    /// Starts the checker module from this binary in its own process group.
    fn spawn() -> io::Result<CheckerModule> {
        let executable = module_executable().ok_or_else(|| io::Error::other("module binary"))?;
        let mut child = tokio::process::Command::new(&executable.path)
            .args(["module-pilot", "checker"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let (Some(input), Some(output)) = (child.stdout.take(), child.stdin.take()) else {
            return Err(io::Error::other("module pipes missing"));
        };
        Ok(CheckerModule {
            child,
            input,
            output,
            next_id: 0,
            in_flight: false,
        })
    }

    /// The only runs this request permits, recomputed by the core: the exact in-process
    /// [`PythonChecker`] spec for every Python root whose environment the core itself resolves.
    /// A module-proposed spec must equal one of them field for field (program, arguments,
    /// environment, read and write roots, host exclusions, timeout, output cap).
    fn permitted(&self, request: &CheckRequest) -> Vec<RunSpec> {
        let checker = PythonChecker::new(
            self.runner.clone(),
            self.node.clone(),
            self.cli.clone(),
            self.timeout,
        );
        crate::environment::resolutions(&request.worktree, &request.read_denies)
            .iter()
            .filter_map(|resolution| {
                let interpreter = crate::environment::interpreter(&resolution.env)?;
                let root = crate::environment::absolute_root(&request.worktree, &resolution.env);
                Some(checker.pyright_spec_for_root(request, &root, &interpreter))
            })
            .collect()
    }

    /// One check exchange; every fault is an error naming it. The whole exchange, run callbacks
    /// included, ends by one absolute deadline (the call budget plus the configured run timeout
    /// for each of at most [`MAX_RUNS`] runs).
    async fn exchange(
        &self,
        slot: &mut Option<CheckerModule>,
        request: &CheckRequest,
    ) -> io::Result<ProblemSnapshot> {
        if slot.as_ref().is_some_and(|module| module.in_flight) {
            kill(slot.take()).await;
        }
        let module = match slot {
            Some(module) => module,
            None => slot.insert(Self::spawn()?),
        };
        module.next_id += 1;
        module.in_flight = true;
        let id = module.next_id;
        let budget = pilot::call_budget();
        let deadline = tokio::time::Instant::now() + budget + self.timeout * MAX_RUNS;
        let stalled =
            || io::Error::other(format!("stalled past its {} ms budget", budget.as_millis()));
        let check = json!({"id": id, "method": "check", "params": {
            "request": request,
            "node": self.node,
            "cli": self.cli,
            "timeout_ms": self.timeout.as_millis() as u64,
        }});
        let frame_deadline = || deadline.min(tokio::time::Instant::now() + budget);
        tokio::time::timeout_at(frame_deadline(), write_frame(&mut module.output, &check))
            .await
            .map_err(|_| stalled())??;
        let permitted = self.permitted(request);
        let mut runs = 0;
        loop {
            let mut frame =
                tokio::time::timeout_at(frame_deadline(), read_frame(&mut module.input))
                    .await
                    .map_err(|_| stalled())?
                    .map_err(|error| match error.kind() {
                        io::ErrorKind::InvalidData => {
                            io::Error::other(format!("sent a bad frame ({error})"))
                        }
                        _ => io::Error::other("exited"),
                    })?;
            if frame["method"] == "run" {
                runs += 1;
                if runs > MAX_RUNS {
                    return Err(io::Error::other("requested too many runs"));
                }
                let spec: RunSpec = pilot::decode(frame["params"].take())?;
                let reply = if permitted.contains(&spec) {
                    match tokio::time::timeout_at(deadline, self.runner.run(spec)).await {
                        Ok(Ok(output)) => json!({"result": output}),
                        Ok(Err(error)) => json!({"error": error.to_string()}),
                        Err(_) => return Err(io::Error::other("check exceeded its deadline")),
                    }
                } else {
                    json!({"error": "run refused by the core policy"})
                };
                tokio::time::timeout_at(frame_deadline(), write_frame(&mut module.output, &reply))
                    .await
                    .map_err(|_| stalled())??;
                continue;
            }
            if frame["id"].as_u64() != Some(id) {
                return Err(io::Error::other("answered out of order"));
            }
            let snapshot: ProblemSnapshot = pilot::decode(frame["result"].take())
                .map_err(|_| io::Error::other("sent an ill-typed snapshot"))?;
            if snapshot.language != crate::LANGUAGE
                || snapshot.input_generation != request.input_generation
            {
                return Err(io::Error::other("answered for another check"));
            }
            module.in_flight = false;
            return Ok(snapshot);
        }
    }
}

/// Kills and reaps a checker module, if any.
async fn kill(module: Option<CheckerModule>) {
    if let Some(mut module) = module {
        let _ = module.child.start_kill();
        let _ = module.child.wait().await;
    }
}

impl Checker for PilotChecker {
    /// Python.
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    /// Runs one check through the module; any module fault kills it (the next check starts a
    /// fresh one) and answers `unavailable (fatal)` naming the fault.
    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            let started = Instant::now();
            let mut slot = self.module.lock().await;
            match self.exchange(&mut slot, &request).await {
                Ok(snapshot) => snapshot,
                Err(fault) => {
                    kill(slot.take()).await;
                    ProblemSnapshot::unavailable_with_detail(
                        crate::LANGUAGE,
                        UnavailableReason::Fatal,
                        request.input_generation,
                        started.elapsed().as_millis() as u64,
                        Some(format!("pilot module {fault}")),
                    )
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agent_ide_core::checks::runner::FakeRunner;

    use super::*;

    /// The core permits exactly the in-process spec of each resolved root and refuses a module
    /// spec that differs in any field, including a write root that escapes the cache lexically.
    #[test]
    fn only_the_core_built_spec_is_permitted() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-pilot-policy-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(".venv/bin")).unwrap();
        fs::write(root.join(".venv/bin/python"), "").unwrap();
        fs::write(root.join("pyproject.toml"), "").unwrap();
        let checker = PilotChecker::new(
            Arc::new(FakeRunner::new(Vec::new())),
            PathBuf::from("/opt/node/bin/node"),
            PathBuf::from("/opt/pyright/dist/pyright.js"),
            Duration::from_secs(60),
        );
        let request = CheckRequest {
            worktree: root.clone(),
            cache_dir: root.join("cache"),
            input_generation: 1,
            read_denies: Vec::new(),
        };
        let permitted = checker.permitted(&request);
        assert_eq!(permitted.len(), 1);
        let mut escaped = permitted[0].clone();
        escaped.write_roots = vec![request.cache_dir.join("../outside")];
        assert!(!permitted.contains(&escaped));
        let mut louder = permitted[0].clone();
        louder.max_output_bytes += 1;
        assert!(!permitted.contains(&louder));
        let mut other = permitted[0].clone();
        other.args.push(OsString::from("--watch"));
        assert!(!permitted.contains(&other));
        fs::remove_dir_all(&root).unwrap();
    }
}
