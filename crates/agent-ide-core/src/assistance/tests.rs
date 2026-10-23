//! Explicit, daemon-owned background test jobs and their bounded runner output.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use super::host_binding::BindingRef;
use crate::lang::{Language, TestReport};

/// Several targets as one run of a worktree (batch-api-v1 §6).
#[path = "test_batch.rs"]
mod batch;
pub use batch::{BatchCommand, BatchLaunch, BatchPlan, BatchTarget};

/// Maximum combined stdout and stderr retained for `ide.test` output continuation.
const MAX_OUTPUT: usize = 256 * 1024;
/// Explicit prefix when the bounded combined runner capture discarded earlier bytes.
pub(super) const OUTPUT_CUT: &str = "[cut]\n";
/// Maximum output tail retained beside the paged full result.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;
/// Completed results stay queryable for ten minutes unless observed earlier.
const COMPLETED_TTL: Duration = Duration::from_secs(600);
/// Pipe drain grace after a budget kill; never extends a run indefinitely for an orphan reader.
const READER_DRAIN_GRACE: Duration = Duration::from_millis(500);
/// At most this many completed jobs remain per worktree.
const COMPLETED_PER_WORKTREE: usize = 4;

/// Reserved evidence from a trusted runner prelude that refused before loading any tests.
const ENVIRONMENT_REFUSAL: &str = "error: agent-ide test environment unavailable:";

impl RunResult {
    /// Returns a captured environment refusal only with its reserved exit and absent counts.
    /// The real exit, stderr tail and replay remain available after this classification.
    pub(super) fn environment_unavailable(&self) -> Option<&str> {
        if self.exit != Some(78)
            || !self.report.incomplete
            || self.report.passed != 0
            || self.report.failed != 0
            || self.report.ignored != 0
            || !self.report.failures.is_empty()
        {
            return None;
        }
        self.output
            .lines()
            .find_map(|line| line.strip_prefix(ENVIRONMENT_REFUSAL).map(str::trim))
    }
}

/// One daemon's monotonically numbered test jobs, independent of host binding lifetimes.
#[derive(Clone)]
pub struct TestRuns(
    /// Registry state shared between the daemon worker and detached process tasks.
    Arc<Mutex<State>>,
    /// The daemon's admission controller every run is an Execution-owned job of.
    Arc<Mutex<crate::execution::AdmissionController>>,
);

impl Default for TestRuns {
    /// A registry with its own controller of the daemon's limits.
    fn default() -> Self {
        Self::new(Arc::new(Mutex::new(super::worker::admission_controller())))
    }
}

/// Mutable run registry protected only while job metadata is read or changed.
#[derive(Default)]
struct State {
    /// Exact typed support origins, separate from main-agent calls and same-root exclusion.
    support_origins: BTreeMap<u64, SupportOrigin>,
    /// Highest allocated daemon-local identifier; identifiers start at one and never repeat.
    next_id: u64,
    /// Retained jobs keyed by daemon-local id and partitioned by `Job::root`.
    jobs: BTreeMap<u64, Job>,
    /// Called with a run's id right after its result is recorded (the session outbox's
    /// `test-finished` producer); absent until the worker installs it.
    on_settle: Option<Arc<dyn Fn(u64) + Send + Sync>>,
}

/// Internal authority of one optional test; never accepted through model/tool arguments.
#[derive(Clone)]
pub(crate) struct SupportOrigin {
    /// Exact host binding generation owning cancellation and result disclosure.
    pub binding: BindingRef,
    /// Durable workspace authority generation proved immediately before admission.
    pub authority_epoch: u64,
    /// Stable configured task digest; does not confer authority by itself.
    pub task: [u8; 32],
    /// Exact native/edit source epoch at optional admission; later differences label history.
    #[cfg(feature = "jev-pilot")]
    pub source_epoch: super::outbox::JobEpoch,
}

/// One job's worktree, command, timing, and optional completed report/output.
struct Job {
    /// Authorized worktree from which the process runs.
    root: PathBuf,
    /// Binding that owns the original output detail reference.
    owner: [u8; 32],
    /// Actor/channel identity of that binding; a run-handle lookup answers it across generations,
    /// so the status outlives `ide.stop`.
    channel: [u8; 32],
    /// Detail reference retained for this run's paged output while the run is retained.
    detail_ref: String,
    /// Exact argv the process runs; known from the start, so an uncollected running run can
    /// still be named in an `ide.stop` reply.
    command: Vec<String>,
    /// True only when the caller supplied an exact command rather than selecting a test runner.
    explicit_command: bool,
    /// Monotonic start time used for status ages.
    started: tokio::time::Instant,
    /// Absent while active, present after exit, spawn failure, or budget expiry.
    result: Option<RunResult>,
    /// Monotonic completion time for the bounded result lifetime.
    completed_at: Option<tokio::time::Instant>,
    /// True after an explicit result read or retirement by stop; Bridge handoff alone is not
    /// evidence that the model consumed a result.
    observed: bool,
    /// Owner-requested cancellation, observed by the runner before releasing its process group.
    cancel: tokio_util::sync::CancellationToken,
}

/// Captured parser summary, bounded output, and whether the budget killed the child.
#[derive(Clone)]
pub struct RunResult {
    /// Counts and failures explicitly parsed from runner output.
    pub report: TestReport,
    /// Last at most 256 KiB of combined stdout/stderr, prefixed `[cut]` inside that cap when
    /// earlier bytes were discarded. Once paged, this copy keeps only its bounded runner line.
    pub output: String,
    /// `true` once the owner's retained detail took over the whole output as its pages and this
    /// copy shrank to the runner line; later replies then point at that detail instead of
    /// quoting a head.
    pub output_paged: bool,
    /// Elapsed wall-clock process duration.
    pub elapsed: Duration,
    /// True when the process was stopped by budget, owner cancellation or bounded pipe drain.
    pub stopped: bool,
    /// True only when an explicit owner cancellation won before child completion.
    pub cancelled: bool,
    /// Process exit code; absent after a budget kill, a signal, or a spawn failure.
    pub exit: Option<i32>,
    /// Budget configured for this process.
    pub budget: Duration,
    /// Detail reference retained under the binding that started the process.
    pub detail_ref: String,
    /// Replay argv: actual program/arguments, explicit overrides and effective PATH. Admitted
    /// recipes use `env -i` with their complete allowlisted environment; unrelated daemon
    /// variables are never copied into the receipt.
    pub command: Vec<String>,
    /// Worktree-relative directory the run used when it was not the worktree root, shown before the
    /// rerun command so the line reproduces the run.
    pub rerun_dir: Option<String>,
    /// Environment chosen at launch, shown only when multiple candidates or a selection exist.
    pub environment_label: Option<String>,
    /// One line per target of a several-target run (`target 2 path tests/cli.rs: run 2 — 3
    /// passed, 0 failed`), shown under the result; empty for a single-target run.
    pub targets: Vec<String>,
}

/// Result of attempting to start a job in one worktree.
pub enum StartResult {
    /// The command spawned and its daemon-owned job id.
    Started(u64),
    /// An existing live job id and its age; no second command was spawned.
    Running(u64, Duration),
    /// A local configuration or operating-system error prevented command spawn. `not_found` is
    /// the missing-executable kind, so the reply can teach the fix instead of quoting errno.
    Failed { error: String, not_found: bool },
}

/// Read-only age and optional completed result returned by a status lookup.
pub struct JobStatus {
    /// Daemon-local run number the caller's `tests #N` line names.
    pub id: u64,
    /// Monotonic elapsed age of the job.
    pub age: Duration,
    /// Binding that owns the retained output detail reference.
    pub owner: [u8; 32],
    /// Parsed report and bounded output after the process settles.
    pub result: Option<RunResult>,
    /// The starting job's own detail reference; the run it started can be found back by it.
    pub detail_ref: String,
    /// Whether the run came from the explicit command form of `ide.test`.
    pub explicit_command: bool,
}

/// Execution settings for one exact test command after facade validation.
pub struct TestCommandOptions {
    /// Working directory already verified beneath the worktree root by the caller.
    pub cwd: PathBuf,
    /// Validated environment overrides, applied over the inherited environment.
    pub env: Vec<(String, String)>,
    /// Language whose parser is tried before the registered runner parsers.
    pub language: Language,
    /// Language that built argv; None for an explicit command whose program selects a resolver.
    pub command_language: Option<Language>,
    /// Maximum wall-clock runtime before the process group is killed.
    pub budget: Duration,
    /// Owner-scoped detail reference retaining the command's full output.
    pub detail_ref: String,
    /// The command's environment, resolved beforehand ([`resolve_command`]).
    pub resolution: CommandResolution,
}

/// How a test command starts inside its language environment, resolved before the run in
/// process or by the language's module.
#[derive(Clone, Debug, Default)]
pub struct CommandResolution {
    /// The resolver's command environment (argv prefix, PATH prefix, variables).
    pub resolved: Option<crate::lang::environment::CommandEnv>,
    /// The environment label the reply names.
    pub environment_label: Option<String>,
}

/// Resolves `argv` from `cwd` in `root`: the building language's command environment, or for
/// an explicit command the first language whose resolver claims its program, and the label of
/// the environment it runs in.
pub async fn resolve_command(
    root: &std::path::Path,
    cwd: &std::path::Path,
    argv: &[String],
    command_language: Option<Language>,
    launcher: &super::launcher::LauncherConfig,
    target: &super::launcher::LaunchTarget,
) -> Result<
    Result<CommandResolution, crate::lang::LangError>,
    crate::modules::contract::ModuleUnavailable,
> {
    if argv.is_empty() {
        return Ok(Ok(CommandResolution::default()));
    }
    let mut environment_language = command_language;
    let mut resolved = None;
    let languages = command_language.map_or_else(
        || crate::lang::registered().to_vec(),
        |language| vec![language],
    );
    for language in languages {
        let mut config = launcher.module_config(language, root, target);
        if command_language.is_none() {
            config.test_workers = None;
        }
        match crate::modules::calls::command_env_argv(language, root, cwd, argv, &config).await? {
            Err(error) => return Ok(Err(error)),
            Ok(Some(env)) => {
                resolved = Some(env);
                environment_language = Some(language);
                break;
            }
            Ok(None) => {}
        }
    }
    let environment_label = match environment_language {
        Some(language) => {
            command_environment_label(
                root,
                cwd,
                language,
                &launcher.module_config(language, root, target),
            )
            .await?
        }
        None => None,
    };
    Ok(Ok(CommandResolution {
        resolved,
        environment_label,
    }))
}

impl TestRuns {
    /// An empty registry whose runs are Execution-owned jobs of `admission`.
    pub fn new(admission: Arc<Mutex<crate::execution::AdmissionController>>) -> Self {
        Self(Arc::default(), admission)
    }

    /// Starts `argv` from `root` unless that worktree already has a live job.
    #[cfg(test)]
    pub fn start(
        &self,
        root: PathBuf,
        argv: Vec<String>,
        language: Language,
        budget: Duration,
        detail_ref: String,
        owner: &BindingRef,
    ) -> StartResult {
        let resolved = argv
            .first()
            .and_then(|program| language.support().command_env(&root, &root, program));
        let resolution = CommandResolution {
            resolved,
            environment_label: None,
        };
        self.start_with_options(
            root.clone(),
            argv,
            owner,
            TestCommandOptions {
                cwd: root,
                env: Vec::new(),
                language,
                command_language: Some(language),
                budget,
                detail_ref,
                resolution,
            },
        )
    }

    /// Starts the command described by `options`, reserving one live job for `root`.
    ///
    /// `root` partitions job exclusion and status; `argv` is the exact command; `owner` scopes
    /// status and output access. `options.cwd` is the execution directory already resolved beneath
    /// `root`; its `env` replaces inherited values with the same names, its `language` selects the
    /// primary output parser, its `budget` bounds runtime, and its `detail_ref` retains full output.
    /// Returns whether the command started, an existing job is running, or spawn failed; facade
    /// bounds and worktree containment are the caller's responsibility.
    pub fn start_with_options(
        &self,
        root: PathBuf,
        argv: Vec<String>,
        owner: &BindingRef,
        options: TestCommandOptions,
    ) -> StartResult {
        let TestCommandOptions {
            cwd,
            env,
            language,
            command_language: _,
            budget,
            detail_ref,
            resolution,
        } = options;
        let mut state = match self.0.lock() {
            Ok(state) => state,
            Err(error) => {
                return StartResult::Failed {
                    error: error.to_string(),
                    not_found: false,
                };
            }
        };
        if let Some((id, job)) = state
            .jobs
            .iter()
            .find(|(_, job)| job.root == root && job.result.is_none())
        {
            return StartResult::Running(*id, job.started.elapsed());
        }
        // Held until the child settles, independent of `ide.stop`; no lease, no run.
        let Some(lease) = crate::retention::Lease::for_worktree(&root) else {
            return StartResult::Failed {
                error: "the worktree's cache retention lease could not be taken".to_owned(),
                not_found: false,
            };
        };
        let lease = crate::retention::SettledLease::new(Some(lease));
        let (child, rerun_command) = match spawn_resolved(
            &self.1,
            &root,
            &cwd,
            &argv,
            &env,
            resolution.resolved.as_ref(),
        ) {
            Ok(child) => child,
            Err(error) => {
                lease.settled();
                return StartResult::Failed {
                    not_found: error.kind() == std::io::ErrorKind::NotFound,
                    error: error.to_string(),
                };
            }
        };
        // The receipt comes from the same resolved command that was started, including the
        // actual effective PATH, without copying unrelated ambient environment values.
        let rerun_dir = cwd
            .strip_prefix(&root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.display().to_string());
        let launch = Launch {
            language,
            budget,
            cap: MAX_OUTPUT,
            detail_ref,
            rerun_command,
            rerun_dir,
            environment_label: resolution.environment_label,
        };
        self.register(&mut state, root, owner, child, lease, launch, None)
    }

    /// Starts the run the core expanded and admitted from a language's declared test recipe:
    /// exactly `spec`'s program, arguments, working directory and complete environment (nothing
    /// inherited, no language command environment), within the smaller of `budget` and the
    /// spec's timeout, retaining at most its capture ceiling of output. It is an Execution-owned
    /// job like every test run and, as today, not read-confined (design §2.6: tests may execute
    /// project code as currently authorized). `command` is the pre-admission display label;
    /// the rerun receipt uses the actual admitted program, arguments and cleared environment.
    #[allow(
        clippy::too_many_arguments,
        reason = "the run's own facts, as start_with_options carries them in its options"
    )]
    pub fn start_admitted(
        &self,
        root: PathBuf,
        spec: crate::checks::runner::RunSpec,
        _command: Vec<String>,
        owner: &BindingRef,
        language: Language,
        budget: Duration,
        detail_ref: String,
        environment_label: Option<String>,
    ) -> StartResult {
        self.start_admitted_origin(
            root,
            spec,
            _command,
            owner,
            language,
            budget,
            detail_ref,
            environment_label,
            None,
        )
    }

    /// Starts an optional test only with explicit internal task/generation ownership. The origin
    /// binding must equal `owner` and its durable epoch be nonzero; all ordinary recipe, process,
    /// output, timeout and same-root exclusion rules remain those of `start_admitted`.
    #[cfg(feature = "jev-pilot")]
    #[allow(
        clippy::too_many_arguments,
        reason = "same admitted recipe boundary plus typed support authority"
    )]
    pub(crate) fn start_support_admitted(
        &self,
        root: PathBuf,
        spec: crate::checks::runner::RunSpec,
        command: Vec<String>,
        owner: &BindingRef,
        language: Language,
        budget: Duration,
        detail_ref: String,
        origin: SupportOrigin,
    ) -> StartResult {
        if origin.binding != *owner || origin.authority_epoch == 0 {
            return StartResult::Failed {
                error: "support authority invalid".into(),
                not_found: false,
            };
        }
        self.start_admitted_origin(
            root,
            spec,
            command,
            owner,
            language,
            budget,
            detail_ref,
            None,
            Some(origin),
        )
    }

    /// Common admitted start; records a typed optional origin atomically with job registration.
    #[allow(
        clippy::too_many_arguments,
        reason = "shared existing recipe boundary and optional internal origin"
    )]
    fn start_admitted_origin(
        &self,
        root: PathBuf,
        spec: crate::checks::runner::RunSpec,
        _command: Vec<String>,
        owner: &BindingRef,
        language: Language,
        budget: Duration,
        detail_ref: String,
        environment_label: Option<String>,
        origin: Option<SupportOrigin>,
    ) -> StartResult {
        if origin.as_ref().is_some_and(|origin| {
            origin.binding != *owner || origin.authority_epoch == 0 || origin.task == [0; 32]
        }) {
            return StartResult::Failed {
                error: "support authority invalid".into(),
                not_found: false,
            };
        }
        let mut state = match self.0.lock() {
            Ok(state) => state,
            Err(error) => {
                return StartResult::Failed {
                    error: error.to_string(),
                    not_found: false,
                };
            }
        };
        if let Some((id, job)) = state
            .jobs
            .iter()
            .find(|(_, job)| job.root == root && job.result.is_none())
        {
            return StartResult::Running(*id, job.started.elapsed());
        }
        let Some(lease) = crate::retention::Lease::for_worktree(&root) else {
            return StartResult::Failed {
                error: "the worktree's cache retention lease could not be taken".to_owned(),
                not_found: false,
            };
        };
        let lease = crate::retention::SettledLease::new(Some(lease));
        let child = match spawn_spec(&self.1, &root, &spec) {
            Ok(child) => child,
            Err(error) => {
                lease.settled();
                return StartResult::Failed {
                    not_found: error.kind() == std::io::ErrorKind::NotFound,
                    error: error.to_string(),
                };
            }
        };
        let launch = Launch {
            language,
            budget: budget.min(spec.timeout),
            cap: spec.max_output_bytes.min(MAX_OUTPUT),
            detail_ref,
            rerun_command: spec_replay(&spec),
            rerun_dir: spec
                .cwd
                .strip_prefix(&root)
                .ok()
                .filter(|relative| !relative.as_os_str().is_empty())
                .map(|relative| relative.display().to_string()),
            environment_label,
        };
        self.register(&mut state, root, owner, child, lease, launch, origin)
    }

    /// Records a spawned run as the next job of `root` and settles it in the background.
    #[allow(
        clippy::too_many_arguments,
        reason = "existing run facts plus explicit internal support origin"
    )]
    fn register(
        &self,
        state: &mut std::sync::MutexGuard<'_, State>,
        root: PathBuf,
        owner: &BindingRef,
        child: Spawned,
        lease: crate::retention::SettledLease,
        launch: Launch,
        origin: Option<SupportOrigin>,
    ) -> StartResult {
        state.next_id = state.next_id.saturating_add(1);
        let id = state.next_id;
        if let Some(origin) = origin {
            state.support_origins.insert(id, origin);
        }
        let started = tokio::time::Instant::now();
        let cancel = tokio_util::sync::CancellationToken::new();
        state.jobs.insert(
            id,
            Job {
                root: root.clone(),
                owner: owner.fingerprint(),
                channel: owner.channel_identity().fingerprint(),
                detail_ref: launch.detail_ref.clone(),
                command: launch.rerun_command.clone(),
                explicit_command: false,
                started,
                cancel: cancel.clone(),
                result: None,
                completed_at: None,
                observed: false,
            },
        );
        let registry = self.0.clone();
        tokio::spawn(async move {
            let mut result = run_child_cancellable(
                launch.language,
                launch.budget,
                child,
                &root,
                launch.cap,
                cancel,
            )
            .await;
            lease.settled();
            result.detail_ref = launch.detail_ref;
            result.command = launch.rerun_command;
            result.rerun_dir = launch.rerun_dir;
            result.environment_label = launch.environment_label;
            let mut on_settle = None;
            if let Ok(mut state) = registry.lock()
                && let Some(job) = state.jobs.get_mut(&id)
            {
                job.result = Some(result);
                job.completed_at = Some(tokio::time::Instant::now());
                on_settle = state.on_settle.clone();
            }
            // Outside the lock: the hook reads the settled run back through this registry.
            if let Some(on_settle) = on_settle {
                on_settle(id);
            }
            prune_completed(&registry, &root);
        });
        StartResult::Started(id)
    }

    /// Requests cancellation only for a run in this root and exact active owner generation.
    /// Returns None for unknown/foreign jobs; Some(false) for a settled run and Some(true) for
    /// a signalled running job. The result settles asynchronously after process-group reaping.
    pub fn cancel(&self, root: &std::path::Path, id: u64, caller: &BindingRef) -> Option<bool> {
        let state = self.0.lock().ok()?;
        let job = state
            .jobs
            .get(&id)
            .filter(|job| job.root == root && job.owner == caller.fingerprint())?;
        if job.result.is_some() {
            return Some(false);
        }
        job.cancel.cancel();
        Some(true)
    }

    /// Installs the hook called with each run's id once its result is recorded, replacing any
    /// earlier one; the hook runs on the run's settle task, outside the registry lock.
    pub fn on_settle(&self, hook: Arc<dyn Fn(u64) + Send + Sync>) {
        if let Ok(mut state) = self.0.lock() {
            state.on_settle = Some(hook);
        }
    }

    /// Returns a settled run's result without observing it, for the outbox producer; `None`
    /// while it runs or once it was pruned.
    pub fn settled_result(&self, id: u64) -> Option<RunResult> {
        self.0.lock().ok()?.jobs.get(&id)?.result.clone()
    }

    /// Returns immutable support origin only for an internally admitted optional test.
    #[cfg(feature = "jev-pilot")]
    pub(crate) fn support_origin(&self, id: u64) -> Option<SupportOrigin> {
        self.0.lock().ok()?.support_origins.get(&id).cloned()
    }

    /// Marks `id` as an explicit command only when it belongs to `owner`; missing or foreign ids
    /// are unchanged. Call immediately after `start` succeeds and before servicing another
    /// request so summary-less output can be returned inline.
    pub fn mark_explicit_command(&self, id: u64, owner: &BindingRef) {
        if let Ok(mut state) = self.0.lock()
            && let Some(job) = state
                .jobs
                .get_mut(&id)
                .filter(|job| job.owner == owner.fingerprint())
        {
            job.explicit_command = true;
        }
    }

    /// Returns the job's age, owner, and result when it ran in `root` and was started by `caller`'s
    /// actor and channel (any binding generation); another actor's run in the same worktree is
    /// `None`, like an unknown id, so its failures and rerun command stay with its own actor. Only
    /// the starting binding's lookup marks retained output observed for daemon idleness.
    pub fn get(&self, root: &PathBuf, id: u64, caller: &BindingRef) -> Option<JobStatus> {
        let channel = caller.channel_identity().fingerprint();
        let mut state = self.0.lock().ok()?;
        let job = state
            .jobs
            .get_mut(&id)
            .filter(|job| &job.root == root && job.channel == channel)?;
        if job.result.is_some() && job.owner == caller.fingerprint() {
            job.observed = true;
        }
        Some(JobStatus {
            id,
            age: job.started.elapsed(),
            owner: job.owner,
            result: job.result.clone(),
            detail_ref: job.detail_ref.clone(),
            explicit_command: job.explicit_command,
        })
    }

    /// Returns bounded newest-first summaries for retained runs of the same actor/channel and
    /// worktree, across activation generations. This read does not observe, start or wait for jobs;
    /// another actor or root never contributes a row. None means the registry is inaccessible,
    /// not an empty measured history; an available registry returns Some, including an empty list.
    pub fn history(&self, root: &std::path::Path, caller: &BindingRef) -> Option<Vec<String>> {
        let channel = caller.channel_identity().fingerprint();
        let state = self.0.lock().ok()?;
        Some(
            state
                .jobs
                .iter()
                .rev()
                .filter(|(_, job)| job.root == root && job.channel == channel)
                .take(COMPLETED_PER_WORKTREE + 1)
                .map(|(id, job)| {
                    job.result.as_ref().map_or_else(
                        || super::test_fact::running_line(*id),
                        |result| {
                            format!(
                                "tests #{id}: {}",
                                super::test_fact::terminal_fact(*id, result, false).summary
                            )
                        },
                    )
                })
                .collect(),
        )
    }

    /// Returns one daemon-global job's status for any generation of its starting actor and
    /// channel, without a worktree: the handle an agent invented for `ide.inspect` (`tests #N`)
    /// names a run, not a detail, and still answers after that actor's `ide.stop`.
    pub fn find(&self, id: u64, caller: &BindingRef) -> Option<JobStatus> {
        let channel = caller.channel_identity().fingerprint();
        let mut state = self.0.lock().ok()?;
        let job = state
            .jobs
            .get_mut(&id)
            .filter(|job| job.channel == channel)?;
        if job.result.is_some() {
            job.observed = true;
        }
        Some(JobStatus {
            id,
            age: job.started.elapsed(),
            owner: job.owner,
            result: job.result.clone(),
            detail_ref: job.detail_ref.clone(),
            explicit_command: job.explicit_command,
        })
    }

    /// Returns the run one starting job's own detail reference began, for the same actor and
    /// channel in any generation, so a job held open for that run resumes it instead of
    /// spawning a second command. Observes a settled own run exactly like `get`.
    pub fn started_run(&self, reference: &str, caller: &BindingRef) -> Option<JobStatus> {
        let channel = caller.channel_identity().fingerprint();
        let mut state = self.0.lock().ok()?;
        let (id, job) = state
            .jobs
            .iter_mut()
            .find(|(_, job)| job.detail_ref == reference && job.channel == channel)?;
        if job.result.is_some() && job.owner == caller.fingerprint() {
            job.observed = true;
        }
        Some(JobStatus {
            id: *id,
            age: job.started.elapsed(),
            owner: job.owner,
            result: job.result.clone(),
            detail_ref: job.detail_ref.clone(),
            explicit_command: job.explicit_command,
        })
    }

    /// Marks every job started by `binding` observed so stopping that actor cannot pin idle exit.
    pub fn observe_binding(&self, binding: &[u8; 32]) {
        if let Ok(mut state) = self.0.lock() {
            for job in state.jobs.values_mut().filter(|job| &job.owner == binding) {
                job.observed = true;
            }
        }
    }

    /// Drops buffered output after its paged copy is retained by the owner's detail ledger,
    /// keeping only a bounded output tail for later status lines.
    /// Identifiers are daemon-unique, so the run needs no worktree to be named again.
    pub fn clear_output(&self, id: u64, binding: &[u8; 32]) {
        if let Ok(mut state) = self.0.lock()
            && let Some(job) = state.jobs.get_mut(&id)
            && &job.owner == binding
            && let Some(result) = &mut job.result
        {
            result.output = output_tail(&result.output);
            result.output.shrink_to_fit();
            result.output_paged = true;
        }
    }

    /// Lists one binding's runs whose result no caller of that binding ever read — a run still
    /// running, or a settled one no status, handle or plate delivered — as `(id, replay, running)`.
    /// `ide.stop` reports them so an agent cannot claim results it never collected.
    pub fn uncollected(&self, binding: &[u8; 32]) -> Vec<(u64, Vec<String>, bool)> {
        self.0
            .lock()
            .map(|state| {
                state
                    .jobs
                    .iter()
                    .filter(|(_, job)| &job.owner == binding)
                    .filter(|(_, job)| job.result.is_none() || !job.observed)
                    .map(|(id, job)| (*id, job.command.clone(), job.result.is_none()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Captures at most eight own runs not explicitly read, for the stopping reply.
    ///
    /// Settled runs carry their actual bounded terminal fact; live runs say running. Bridge
    /// handoff is not treated as proof of consumption. No run is marked observed here; stop
    /// retires the binding separately. Full-output references are omitted because their
    /// activation-scoped ledger is released by stop. Lock failure yields explicit unavailable
    /// state rather than implying that no run needs reporting.
    pub fn stop_summaries(&self, binding: &[u8; 32]) -> Vec<String> {
        let Ok(state) = self.0.lock() else {
            return vec!["test run state unavailable".to_owned()];
        };
        let mut own = state
            .jobs
            .iter()
            .filter(|(_, job)| &job.owner == binding && (job.result.is_none() || !job.observed));
        let mut summaries = own
            .by_ref()
            .take(8)
            .map(|(id, job)| match &job.result {
                Some(result) => super::test_fact::terminal_fact(*id, result, false).reply_text(*id),
                None => format!(
                    "tests #{id}: running — {}",
                    super::worker::display_argv(&job.command)
                ),
            })
            .collect::<Vec<_>>();
        let more = own.count();
        if more > 0 {
            summaries.push(format!("(+{more} more)"));
        }
        summaries
    }

    /// Returns the output detail references of every retained run; the worker never evicts them,
    /// so a run's full output stays readable while its result does.
    pub fn detail_refs(&self) -> BTreeSet<String> {
        self.0.lock().map_or_else(
            |_| BTreeSet::new(),
            |state| {
                state
                    .jobs
                    .values()
                    .map(|job| job.detail_ref.clone())
                    .collect()
            },
        )
    }

    /// Reports whether any daemon-owned test child is still running.
    pub fn is_busy(&self) -> bool {
        self.0.lock().is_ok_and(|state| {
            state.jobs.values().any(|job| {
                job.result.is_none()
                    || (!job.observed
                        && job
                            .completed_at
                            .is_some_and(|completed| completed.elapsed() < COMPLETED_TTL))
            })
        })
    }

    /// Reports only a live same-root child. This is exclusion, never equivalence of test targets
    /// or permission to reuse output or cancel another binding's process.
    #[cfg(feature = "jev-pilot")]
    pub(crate) fn running(&self, root: &std::path::Path) -> bool {
        self.0.lock().map_or(true, |state| {
            state
                .jobs
                .values()
                .any(|job| job.root == root && job.result.is_none())
        })
    }
}

/// Removes the oldest finished runs until each worktree retains at most four.
fn prune_completed(registry: &Arc<Mutex<State>>, root: &PathBuf) {
    if let Ok(mut state) = registry.lock() {
        let mut completed = state
            .jobs
            .iter()
            .filter(|(_, job)| &job.root == root && job.result.is_some())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        while completed.len() > COMPLETED_PER_WORKTREE {
            let id = completed.remove(0);
            state.jobs.remove(&id);
            state.support_origins.remove(&id);
        }
    }
}

/// Selects the most useful bounded runner line for an empty result summary.
pub(super) fn runner_excerpt(output: &str) -> String {
    let line = output
        .lines()
        .find(|line| {
            let line = line.trim_start();
            line.starts_with("ERROR") || line.starts_with("error") || line.starts_with("E ")
        })
        .or_else(|| output.lines().rev().find(|line| !line.trim().is_empty()))
        .unwrap_or("(empty output)");
    let mut excerpt = line.to_owned();
    while excerpt.len() > 200 {
        excerpt.pop();
    }
    excerpt
}

/// Keeps the output's final bounded bytes without splitting a UTF-8 character.
pub(super) fn output_tail(output: &str) -> String {
    let mut start = output.len().saturating_sub(OUTPUT_TAIL_BYTES);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    if start > 0
        && let Some(newline) = output[start..].find('\n')
        && newline + 1 < output.len() - start
    {
        start += newline + 1;
    }
    output[start..].to_owned()
}

/// Reports whether no test counts were parsed, including cancelled or budget-stopped runs.
/// Zero defaults from an incomplete parser never become observed test counts. Explicit commands
/// and selected runners use this to retain lifecycle status with an honest output tail.
pub fn summary_absent(result: &RunResult) -> bool {
    result.report.incomplete && result.report.passed == 0 && result.report.failed == 0
}

/// Runs one command with inherited environment, bounded output, a process-group budget kill,
/// and language-native output parsing.
#[cfg(test)]
async fn run(
    root: &std::path::Path,
    argv: &[String],
    language: Language,
    budget: Duration,
) -> RunResult {
    match spawn_command(root, root, argv, &[], Some(language)) {
        Ok(spawned) => run_child(language, budget, spawned, root).await,
        Err(error) => failed_run(budget, error.to_string()),
    }
}

/// Prepends a pinned toolchain directory while preserving every caller-provided PATH entry.
fn prepend_toolchain_path(
    bin: &std::path::Path,
    caller_path: &std::ffi::OsStr,
) -> io::Result<std::ffi::OsString> {
    std::env::join_paths(
        std::iter::once(bin.to_path_buf()).chain(std::env::split_paths(caller_path)),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

/// Names the deepest cwd environment, or the resolver's primary root for rootless projects,
/// only when alternatives or a stored selection make the choice relevant.
pub(super) async fn command_environment_label(
    worktree: &std::path::Path,
    cwd: &std::path::Path,
    language: Language,
    config: &crate::modules::contract::ModuleConfig,
) -> Result<Option<String>, crate::modules::contract::ModuleUnavailable> {
    let environments = crate::modules::calls::environments(language, worktree, config).await?;
    let Some(env) = environments
        .iter()
        .filter(|env| cwd.starts_with(worktree.join(&env.root)))
        .max_by_key(|env| env.root.components().count())
        .or_else(|| environments.first())
    else {
        return Ok(None);
    };
    let selected = crate::lang::environment::selections(worktree, language)
        .iter()
        .any(|selection| selection.root == env.root);
    Ok((env.candidates.len() > 1 || selected)
        .then(|| env.chosen.as_ref().map(|chosen| chosen.label.clone()))
        .flatten())
}

/// [`spawn_resolved`] with the command environment resolved in process: the building language's,
/// or the first applicable resolver's for an explicit command.
#[cfg(test)]
fn spawn_command(
    worktree: &std::path::Path,
    cwd: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    language: Option<Language>,
) -> io::Result<Spawned> {
    let resolved = argv.first().and_then(|program| match language {
        Some(language) => language.support().command_env(worktree, cwd, program),
        None => crate::lang::registered()
            .iter()
            .find_map(|language| language.support().command_env(worktree, cwd, program)),
    });
    spawn_resolved(
        &Arc::new(Mutex::new(super::worker::admission_controller())),
        worktree,
        cwd,
        argv,
        env,
        resolved.as_ref(),
    )
    .map(|(child, _)| child)
}

/// A started test run: the Execution-owned job and its stdout and stderr pipes.
type Spawned = (
    crate::execution::job::StreamedJob,
    tokio::process::ChildStdout,
    tokio::process::ChildStderr,
);

/// Starts argv in its resolved command environment as an Execution-owned job of `admission`,
/// owned per worktree. The worktree and cwd are already admitted. The daemon's environment is
/// the base; caller values override it, resolver variables override those and its prefix leads
/// PATH. Stdin is closed and stdout/stderr are piped to the caller; empty argv, a program not on
/// PATH (`NotFound`) and a full admission queue (`WouldBlock`) start nothing. Returns the child
/// and a replay receipt of its exact program/arguments, explicit overrides and effective PATH.
fn spawn_resolved(
    admission: &Arc<Mutex<crate::execution::AdmissionController>>,
    worktree: &std::path::Path,
    cwd: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    resolved: Option<&crate::lang::environment::CommandEnv>,
) -> io::Result<(Spawned, Vec<String>)> {
    let (command, replay) = prepare_command(cwd, argv, env, resolved)?;
    let owner = crate::execution::OwnerId::new(format!(
        "test:{}",
        blake3::hash(worktree.as_os_str().as_encoded_bytes()).to_hex()
    ))
    .map_err(|error| io::Error::other(format!("{error:?}")))?;
    crate::execution::job::StreamedJob::start(
        admission,
        owner,
        crate::execution::AdmissionClass::Background,
        &command,
    )
    .map(|child| (child, replay))
}

/// Resolves one native command before admission, returning its controlled command and replay
/// argv. `env` overrides ambient values, then `resolved` overrides/prefixes them. The receipt
/// retains only explicit overrides and effective PATH, never unrelated ambient secrets. Empty
/// argv, invalid PATH components, missing programs or invalid command policy return I/O errors.
pub(super) fn prepare_command(
    cwd: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    resolved: Option<&crate::lang::environment::CommandEnv>,
) -> io::Result<(crate::execution::ControlledCommand, Vec<String>)> {
    use std::ffi::OsString;
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    };
    let mut prefix = resolved
        .as_ref()
        .map_or_else(|| vec![program.into()], |env| env.argv_prefix.clone());
    if prefix.is_empty() {
        prefix.push(program.into());
    }
    let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    vars.extend(
        env.iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    if let Some(resolved) = resolved {
        vars.extend(
            resolved
                .vars
                .iter()
                .map(|(key, value)| (OsString::from(key), value.clone())),
        );
        if let Some(bin) = &resolved.path_prefix {
            let caller_path = resolved
                .vars
                .iter()
                .rev()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| value.clone())
                .or_else(|| {
                    env.iter()
                        .rev()
                        .find(|(key, _)| key == "PATH")
                        .map(|(_, value)| value.into())
                })
                .unwrap_or_else(|| std::env::var_os("PATH").unwrap_or_default());
            vars.insert("PATH".into(), prepend_toolchain_path(bin, &caller_path)?);
        }
    }
    let path = vars
        .get(std::ffi::OsStr::new("PATH"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let executable = crate::execution::job::executable_on(&prefix[0].to_string_lossy(), cwd, &path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "program not found"))?;
    let args: Vec<_> = prefix[1..]
        .iter()
        .cloned()
        .chain(args.iter().map(OsString::from))
        .collect();
    let keys: BTreeSet<OsString> = env
        .iter()
        .map(|(key, _)| key.into())
        .chain(
            resolved
                .into_iter()
                .flat_map(|value| value.vars.iter().map(|(key, _)| key.clone())),
        )
        .chain(std::iter::once("PATH".into()))
        .collect();
    let mut replay = vec!["/usr/bin/env".to_owned()];
    replay.extend(keys.iter().filter_map(|key| {
        vars.get(key)
            .map(|value| format!("{}={}", key.to_string_lossy(), value.to_string_lossy()))
    }));
    replay.push(executable.to_string_lossy().into_owned());
    replay.extend(args.iter().map(|arg| arg.to_string_lossy().into_owned()));
    let command = crate::execution::ControlledCommand::from_validated_peer(
        crate::execution::CommandKind::Job,
        executable,
        args,
        cwd.to_path_buf(),
        vars,
    )
    .map_err(|error| io::Error::new(io::ErrorKind::NotFound, format!("{error:?}")))?;
    Ok((command, replay))
}

/// Replays an admitted recipe with precisely its allowlisted environment, program and arguments.
/// No daemon variables enter this receipt; `env -i` preserves the original cleared boundary.
fn spec_replay(spec: &crate::checks::runner::RunSpec) -> Vec<String> {
    let mut replay = vec!["/usr/bin/env".to_owned(), "-i".to_owned()];
    replay.extend(spec.env.iter().map(|(key, value)| format!("{key}={value}")));
    replay.push(spec.program.to_string_lossy().into_owned());
    replay.extend(
        spec.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    replay
}

/// How one started run settles and is shown.
struct Launch {
    /// Language whose parser is tried first.
    language: Language,
    /// Wall-clock budget.
    budget: Duration,
    /// Most combined output bytes retained.
    cap: usize,
    /// Detail reference retaining the full result.
    detail_ref: String,
    /// The rerun line's command.
    rerun_command: Vec<String>,
    /// The rerun line's directory, relative to the worktree.
    rerun_dir: Option<String>,
    /// The environment the reply names.
    environment_label: Option<String>,
}

/// Starts exactly `spec` as an Execution-owned job of `worktree`'s test owner: its program, its
/// arguments, its working directory and only its environment.
fn spawn_spec(
    admission: &Arc<Mutex<crate::execution::AdmissionController>>,
    worktree: &std::path::Path,
    spec: &crate::checks::runner::RunSpec,
) -> io::Result<Spawned> {
    use std::ffi::OsString;
    let command = crate::execution::ControlledCommand::from_validated_peer(
        crate::execution::CommandKind::Job,
        spec.program.clone(),
        spec.args.clone(),
        spec.cwd.clone(),
        spec.env
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect(),
    )
    .map_err(|error| io::Error::new(io::ErrorKind::NotFound, format!("{error:?}")))?;
    let owner = crate::execution::OwnerId::new(format!(
        "test:{}",
        blake3::hash(worktree.as_os_str().as_encoded_bytes()).to_hex()
    ))
    .map_err(|error| io::Error::other(format!("{error:?}")))?;
    crate::execution::job::StreamedJob::start(
        admission,
        owner,
        crate::execution::AdmissionClass::Background,
        &command,
    )
}

/// Runs one already-spawned child, enforcing its wall-clock budget and bounded pipe drain.
#[cfg(test)]
async fn run_child(
    language: Language,
    budget: Duration,
    spawned: Spawned,
    root: &std::path::Path,
) -> RunResult {
    run_child_capped(language, budget, spawned, root, MAX_OUTPUT).await
}

/// Runs one already-spawned child, enforcing its wall-clock budget and bounded pipe drain, and
/// retains at most the last `cap` bytes of its combined output.
#[cfg(test)]
async fn run_child_capped(
    language: Language,
    budget: Duration,
    spawned: Spawned,
    root: &std::path::Path,
    cap: usize,
) -> RunResult {
    run_child_cancellable(
        language,
        budget,
        spawned,
        root,
        cap,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
}

/// Settles the same bounded runner after either exit, budget or owner cancellation; cancellation
/// kills/reaps the entire group before releasing admission and preserves captured output.
async fn run_child_cancellable(
    language: Language,
    budget: Duration,
    spawned: Spawned,
    root: &std::path::Path,
    cap: usize,
    cancel: tokio_util::sync::CancellationToken,
) -> RunResult {
    let started = tokio::time::Instant::now();
    let (job, stdout, stderr) = spawned;
    let mut job = Some(job);
    let combined = Arc::new(tokio::sync::Mutex::new(VecDeque::with_capacity(MAX_OUTPUT)));
    let truncated = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut out_task = tokio::spawn(read_into_tail(
        Some(stdout),
        combined.clone(),
        cap,
        truncated.clone(),
    ));
    let mut err_task = tokio::spawn(read_into_tail(
        Some(stderr),
        combined.clone(),
        cap,
        truncated.clone(),
    ));
    let mut cancelled = false;
    let exited = match job.as_mut() {
        Some(running) => tokio::select! {
            biased;
            exited = running.wait(budget) => exited,
            _ = cancel.cancelled() => { cancelled = true; false },
        },
        None => false,
    };
    // Past its budget the whole group is torn down and reaped before the pipes are drained.
    let mut stopped = !exited;
    let mut status = None;
    if stopped && let Some(running) = job.take() {
        let _ = running.finish().await;
    }
    let reader_budget = if stopped {
        READER_DRAIN_GRACE
    } else {
        budget.saturating_sub(started.elapsed())
    };
    let drain = async {
        let _ = (&mut out_task).await;
        let _ = (&mut err_task).await;
    };
    if tokio::time::timeout(reader_budget, drain).await.is_err() {
        stopped = true;
        out_task.abort();
        err_task.abort();
    }
    // A finished run's group is torn down before its leader is reaped, so no descendant outlives
    // it, and its slot is released; the leader's status comes from that reap.
    if let Some(finished) = job.take() {
        status = finished.finish().await.ok();
    }
    let bytes = combined.lock().await.iter().copied().collect::<Vec<_>>();
    let raw = String::from_utf8_lossy(&bytes);
    let output = retained_output(
        &raw,
        cap,
        truncated.load(std::sync::atomic::Ordering::Relaxed),
    );
    // A language module that cannot parse leaves the run with no counts, marked incomplete.
    let parse =
        |language: Language| crate::modules::calls::parse_test_output(language, root, &output, "");
    let mut report = parse(language).await.unwrap_or_else(|_| TestReport {
        incomplete: true,
        ..TestReport::default()
    });
    if report.passed == 0 && report.failed == 0 {
        for &candidate in crate::lang::registered() {
            let Ok(parsed) = parse(candidate).await else {
                continue;
            };
            if (report.incomplete && !parsed.incomplete)
                || parsed.passed.saturating_add(parsed.failed)
                    > report.passed.saturating_add(report.failed)
            {
                report = parsed;
            }
        }
    }
    if status.as_ref().and_then(|status| status.code()) == Some(78)
        && output
            .lines()
            .any(|line| line.starts_with(ENVIRONMENT_REFUSAL))
    {
        report = TestReport {
            incomplete: true,
            ..TestReport::default()
        };
    }
    report.incomplete |= stopped || status.is_none();
    address_failures(root, &mut report).await;
    RunResult {
        report,
        output,
        output_paged: false,
        elapsed: started.elapsed(),
        stopped,
        cancelled,
        exit: status.and_then(|status| status.code()),
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
        rerun_dir: None,
        environment_label: None,
        targets: Vec::new(),
    }
}

/// Failures of one run that get a position address.
const ADDRESSED_FAILURES: usize = 8;
/// Wall-clock budget of the address lookups of one settled run.
const ADDRESS_LOOKUP_BUDGET: Duration = Duration::from_millis(1500);

/// Replaces each failure's `file:line` location by its position address with the absolute line
/// (`src/a.rs#tests/it_works:3 (L40)`), put in front of the message: the address names the
/// failing test (or the helper holding the assertion) and the line inside it. Best effort and
/// bounded: it reads at most [`ADDRESSED_FAILURES`] failures through the languages' source
/// outlines, never a language server, and leaves a failure it cannot resolve untouched.
async fn address_failures(root: &std::path::Path, report: &mut TestReport) {
    use crate::lang::address::AddressMap;

    let lookup = async {
        let mut maps: BTreeMap<PathBuf, Option<AddressMap>> = BTreeMap::new();
        for failure in report.failures.iter_mut().take(ADDRESSED_FAILURES) {
            let Some((path, line)) = failure.location.clone() else {
                continue;
            };
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if !maps.contains_key(&relative) {
                let map = async {
                    let language = Language::for_path(&relative)?;
                    let source = std::fs::read_to_string(root.join(&relative)).ok()?;
                    let outline = crate::modules::calls::outline_from_source(
                        language, root, &relative, &source,
                    )
                    .await
                    .ok()??;
                    Some(AddressMap::build(&outline, &source))
                }
                .await;
                maps.insert(relative.clone(), map);
            }
            if let Some(Some(map)) = maps.get(&relative)
                && let Some(address) = map.address_at(&relative, line)
            {
                failure.message = format!("{address} (L{line})  {}", failure.message);
                failure.location = None;
            }
        }
    };
    let _ = tokio::time::timeout(ADDRESS_LOOKUP_BUDGET, lookup).await;
}

/// Builds the empty-output report returned by the async runner helper after spawn failure.
#[cfg(test)]
fn failed_run(budget: Duration, output: String) -> RunResult {
    RunResult {
        report: TestReport {
            incomplete: true,
            ..TestReport::default()
        },
        output,
        output_paged: false,
        elapsed: Duration::ZERO,
        stopped: false,
        cancelled: false,
        exit: None,
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
        rerun_dir: None,
        environment_label: None,
        targets: Vec::new(),
    }
}

/// Bounds the captured UTF-8 tail, preserving an explicit cut marker after upstream truncation.
/// `cap` includes the marker; lossy UTF-8 expansion is bounded too. No I/O or parser work occurs.
pub(super) fn retained_output(text: &str, cap: usize, truncated: bool) -> String {
    let cut = truncated || text.len() > cap;
    let marker = if cut {
        &OUTPUT_CUT[..cap.min(OUTPUT_CUT.len())]
    } else {
        ""
    };
    let mut start = text.len().saturating_sub(cap.saturating_sub(marker.len()));
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{marker}{}", &text[start..])
}

/// Reads one pipe to EOF into a combined bounded tail; atomically marks any discarded prefix.
/// `truncated` is shared by both pipe readers and remains observable after cancellation/drain.
async fn read_into_tail<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    retained: Arc<tokio::sync::Mutex<VecDeque<u8>>>,
    cap: usize,
    truncated: Arc<std::sync::atomic::AtomicBool>,
) {
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = [0; 8192];
    use tokio::io::AsyncReadExt;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let mut retained = retained.lock().await;
                retained.extend(&chunk[..count]);
                if retained.len() > cap {
                    truncated.store(true, std::sync::atomic::Ordering::Relaxed);
                    let excess = retained.len() - cap;
                    retained.drain(..excess);
                }
            }
        }
    }
}

#[cfg(test)]
mod runner_tests {
    //! Minimal process and parser checks for the test runner.
    use super::*;
    use crate::assistance::test_fact::terminal_fact;

    /// Truncated captures keep an explicit marker inside their byte cap even with UTF-8 tails;
    /// exact-sized and empty captures remain unmodified, including a zero-byte configured cap.
    #[test]
    fn retained_capture_marks_truncation_without_exceeding_utf8_cap() {
        assert_eq!(retained_output("abcdef", 6, false), "abcdef");
        assert_eq!(retained_output("", 0, false), "");
        for cap in 0..40 {
            let text = retained_output(&"é".repeat(30), cap, true);
            assert!(text.len() <= cap);
            if cap >= OUTPUT_CUT.len() {
                assert!(text.starts_with(OUTPUT_CUT));
            }
        }
    }

    /// History is actor/worktree scoped across generations; reads neither consume results nor
    /// launch processes. Retained terminal/running rows are newest first.
    #[test]
    fn history_is_scoped_and_does_not_observe_results() {
        let runs = TestRuns::default();
        let root = PathBuf::from("/fixture/one");
        let own = BindingRef::fixture("actor", "channel", 1);
        let foreign = BindingRef::fixture("other", "channel", 1);
        let job = |binding: &BindingRef, path: PathBuf, complete: bool| Job {
            root: path,
            owner: binding.fingerprint(),
            channel: binding.channel_identity().fingerprint(),
            detail_ref: "fixture-ref".into(),
            command: vec!["fixture-command-never-executed".into()],
            explicit_command: true,
            started: tokio::time::Instant::now(),
            result: complete.then(|| failed_run(Duration::from_secs(1), "failure".into())),
            completed_at: complete.then(tokio::time::Instant::now),
            observed: false,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        {
            let mut state = runs.0.lock().unwrap();
            state.jobs.insert(1, job(&own, root.clone(), true));
            state.jobs.insert(2, job(&foreign, root.clone(), true));
            state.jobs.insert(3, job(&own, root.clone(), false));
            state
                .jobs
                .insert(4, job(&own, PathBuf::from("/fixture/two"), true));
        }
        let rows = runs.history(&root, &own).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], "tests #3: running");
        assert!(rows[1].starts_with("tests #1:"));
        assert!(!runs.0.lock().unwrap().jobs[&1].observed);
        assert_eq!(
            rows,
            runs.history(&root, &BindingRef::fixture("actor", "channel", 2))
                .unwrap()
        );
        assert!(
            runs.history(&root, &BindingRef::fixture("absent", "channel", 1))
                .unwrap()
                .is_empty()
        );
    }

    /// Owner cancellation refuses foreign roots/actors/generations, reaps a real child group,
    /// settles once and reports cancellation rather than a fictitious time-budget expiry.
    #[tokio::test]
    async fn owner_cancel_reaps_and_settles_without_polling() {
        let scratch = crate::scratch::ScratchDir::new("test-cancel");
        let root = scratch.to_path_buf();
        let runs = TestRuns::default();
        let owner = BindingRef::fixture("owner", "channel", 1);
        let settled = Arc::new(tokio::sync::Notify::new());
        let note = settled.clone();
        runs.on_settle(Arc::new(move |_| note.notify_one()));
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(60),
                "cancel-ref".into(),
                &owner
            ),
            StartResult::Started(1)
        ));
        assert_eq!(
            runs.cancel(&root, 1, &BindingRef::fixture("foreign", "channel", 1)),
            None
        );
        assert_eq!(
            runs.cancel(&root, 1, &BindingRef::fixture("owner", "channel", 2)),
            None
        );
        assert_eq!(runs.cancel(&root.join("other"), 1, &owner), None);
        assert_eq!(runs.cancel(&root, 1, &owner), Some(true));
        tokio::time::timeout(Duration::from_secs(3), settled.notified())
            .await
            .unwrap();
        let result = runs.settled_result(1).unwrap();
        assert!(result.cancelled && result.stopped && result.report.incomplete);
        assert!(
            terminal_fact(1, &result, true)
                .summary
                .starts_with("cancelled")
        );
        assert_eq!(runs.cancel(&root, 1, &owner), Some(false));
    }

    /// A pinned toolchain leads PATH without discarding the caller's explicit override.
    #[test]
    fn pinned_toolchain_preserves_caller_path_override() {
        let path = prepend_toolchain_path(
            std::path::Path::new("/pinned/bin"),
            std::ffi::OsStr::new("/custom/bin"),
        )
        .unwrap();
        assert_eq!(
            std::env::split_paths(&path).collect::<Vec<_>>(),
            [PathBuf::from("/pinned/bin"), PathBuf::from("/custom/bin")]
        );
    }

    /// An admitted run is exactly its specification: the process sees only the spec's
    /// environment (nothing inherited) and its working directory, and keeps at most its capture
    /// ceiling of output (the tail).
    #[tokio::test]
    async fn an_admitted_run_is_exactly_its_specification() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-admitted-{}", std::process::id()));
        let cwd = root.join("pkg");
        std::fs::create_dir_all(&cwd).unwrap();
        let owner = BindingRef::fixture("admitted-actor", "admitted-channel", 1);
        let spec = |program: &str, args: &[&str], cap: usize| crate::checks::runner::RunSpec {
            program: program.into(),
            args: args.iter().map(std::ffi::OsString::from).collect(),
            cwd: cwd.clone(),
            env: vec![
                ("ONLY_THIS".to_owned(), "x".to_owned()),
                ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
            ],
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            read_denies: Vec::new(),
            timeout: Duration::from_secs(10),
            max_output_bytes: cap,
        };
        let settle = async |id: u64| {
            for _ in 0..200 {
                if let Some(result) = runs.get(&root, id, &owner).and_then(|status| status.result) {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            panic!("run {id} did not settle");
        };
        let start = |spec: crate::checks::runner::RunSpec| {
            let command = vec![spec.program.display().to_string()];
            runs.start_admitted(
                root.clone(),
                spec,
                command,
                &owner,
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "admitted-detail".into(),
                None,
            )
        };
        assert!(matches!(
            start(spec("/usr/bin/env", &[], 4096)),
            StartResult::Started(1)
        ));
        let mut seen: Vec<&str> = Vec::new();
        let env = settle(1).await;
        seen.extend(env.output.lines());
        seen.sort_unstable();
        assert_eq!(seen, ["ONLY_THIS=x", "PATH=/usr/bin:/bin"]);
        assert_eq!(
            env.command,
            [
                "/usr/bin/env",
                "-i",
                "ONLY_THIS=x",
                "PATH=/usr/bin:/bin",
                "/usr/bin/env"
            ]
        );
        let replay = tokio::process::Command::new("/bin/sh")
            .args(["-c", &super::super::worker::rerun_text(&env)])
            .current_dir(&root)
            .env_clear()
            .env("UNRELATED_SENTINEL", "hidden")
            .output()
            .await
            .unwrap();
        assert!(replay.status.success());
        assert_eq!(String::from_utf8(replay.stdout).unwrap(), env.output);
        assert!(matches!(
            start(spec("/bin/pwd", &[], 4096)),
            StartResult::Started(2)
        ));
        assert_eq!(settle(2).await.output.trim(), cwd.display().to_string());
        let long = "y".repeat(100);
        assert!(matches!(
            start(spec("/bin/echo", &[&long], 10)),
            StartResult::Started(3)
        ));
        let capped = settle(3).await;
        assert_eq!(capped.output, "[cut]\nyyy\n");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Empty summaries surface the first error line, falling back to the final output line, and
    /// never more than 200 bytes.
    #[test]
    fn empty_summary_quotes_bounded_runner_reason() {
        assert_eq!(
            runner_excerpt("noise\nERROR: missing collectors\nlast\n"),
            "ERROR: missing collectors"
        );
        assert_eq!(runner_excerpt("noise\nlast line\n"), "last line");
        assert!(runner_excerpt(&"x".repeat(300)).len() <= 200);
    }

    /// Uses a test-language transcript to prove captured output reaches the language parser.
    #[tokio::test]
    async fn fake_test_command_is_parsed() {
        let root = std::env::temp_dir();
        let script = "printf 'pass\\n'";
        let result = run(
            &root,
            &["/bin/sh".into(), "-c".into(), script.into()],
            crate::lang::testing::ALPHA,
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(result.report.passed, 1);
        assert_eq!(result.report.failed, 0);
    }

    /// Applies explicit cwd and environment options to a launched process.
    #[tokio::test]
    async fn explicit_command_uses_working_directory_and_environment() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let child = spawn_command(
            &cwd,
            &cwd,
            &[
                "/bin/sh".into(),
                "-c".into(),
                "printf '%s\\npass\\n' \"$RUN_MARKER\"".into(),
            ],
            &[("RUN_MARKER".into(), "from-env".into())],
            None,
        )
        .unwrap();
        let result = run_child(
            crate::lang::testing::ALPHA,
            Duration::from_secs(2),
            child,
            &std::env::temp_dir(),
        )
        .await;
        assert!(result.output.starts_with("from-env\n"), "{}", result.output);
        assert_eq!((result.report.passed, result.report.failed), (1, 0));
    }

    /// A run with a working directory and environment overrides keeps both for its rerun line.
    #[tokio::test]
    async fn rerun_names_the_working_directory_and_environment() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-rerun-{}", std::process::id()));
        let package = root.join("pkg");
        std::fs::create_dir_all(&package).unwrap();
        let owner = BindingRef::fixture("rerun-actor", "rerun-channel", 1);
        let started = runs.start_with_options(
            root.clone(),
            vec!["/bin/echo".into(), "done".into()],
            &owner,
            TestCommandOptions {
                cwd: package,
                env: vec![("RUN_MARKER".into(), "x".into())],
                language: crate::lang::testing::ALPHA,
                command_language: None,
                budget: Duration::from_secs(10),
                detail_ref: "rerun-detail".into(),
                resolution: CommandResolution::default(),
            },
        );
        assert!(matches!(started, StartResult::Started(1)));
        let mut result = None;
        for _ in 0..100 {
            result = runs.get(&root, 1, &owner).and_then(|status| status.result);
            if result.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let result = result.unwrap();
        assert_eq!(result.rerun_dir.as_deref(), Some("pkg"));
        assert_eq!(result.command.first().unwrap(), "/usr/bin/env");
        assert!(result.command.contains(&"RUN_MARKER=x".to_owned()));
        assert_eq!(
            &result.command[result.command.len() - 2..],
            ["/bin/echo", "done"]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Real replay preserves actual PATH precedence, activation and quoted arguments: caller
    /// PATH when no resolver PATH exists, resolver PATH otherwise, with the prefix in both.
    /// The replay starts with an empty unrelated environment and cannot expand literal dollars
    /// or quote characters in argv. Both original jobs settle and replay children are reaped.
    #[tokio::test]
    async fn rerun_names_the_resolved_environment_activation() {
        use crate::lang::environment::CommandEnv;
        let runs = TestRuns::default();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-rerun-env-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let owner = BindingRef::fixture("rerun-env-actor", "rerun-env-channel", 1);
        let venv_bin = root.join(".venv/bin space");
        std::fs::create_dir_all(&venv_bin).unwrap();
        for (index, resolved_path) in [None, Some("/resolved path:/bin")].into_iter().enumerate() {
            let script = "printf '%s\\n' \"$PATH\" \"$MARKER\" \"$1\"";
            let literal = "literal $value 'quoted'";
            let mut vars = vec![("MARKER".into(), "resolved 'value'".into())];
            if let Some(path) = resolved_path {
                vars.push(("PATH".into(), path.into()));
            }
            let started = runs.start_with_options(
                root.clone(),
                vec![
                    "requested-program".into(),
                    "-c".into(),
                    script.into(),
                    "label".into(),
                    literal.into(),
                ],
                &owner,
                TestCommandOptions {
                    cwd: root.clone(),
                    env: vec![
                        ("PATH".into(), "/caller path:/bin".into()),
                        ("MARKER".into(), "caller".into()),
                    ],
                    language: crate::lang::testing::ALPHA,
                    command_language: None,
                    budget: Duration::from_secs(10),
                    detail_ref: "rerun-env-detail".into(),
                    resolution: CommandResolution {
                        resolved: Some(CommandEnv {
                            argv_prefix: vec!["/bin/sh".into()],
                            path_prefix: Some(venv_bin.clone()),
                            vars,
                        }),
                        environment_label: Some(".venv".into()),
                    },
                },
            );
            assert!(matches!(started, StartResult::Started(_)));
            let id = index as u64 + 1;
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(result) =
                        runs.get(&root, id, &owner).and_then(|status| status.result)
                    {
                        break result;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let expected = format!(
                "{}:{}\nresolved 'value'\n{literal}\n",
                venv_bin.display(),
                resolved_path.unwrap_or("/caller path:/bin")
            );
            assert_eq!(result.output, expected);
            let replay = tokio::process::Command::new("/bin/sh")
                .args(["-c", &super::super::worker::rerun_text(&result)])
                .current_dir(&root)
                .env_clear()
                .output()
                .await
                .unwrap();
            assert!(replay.status.success());
            assert_eq!(String::from_utf8(replay.stdout).unwrap(), expected);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Kills and reaps a sleeping process group when the budget expires.
    #[cfg(unix)]
    #[tokio::test]
    async fn budget_kills_child_process() {
        let pid_file = std::env::temp_dir().join(format!("ide-test-pid-{}", std::process::id()));
        let command = [
            "/bin/sh".into(),
            "-c".into(),
            format!("sleep 30 & echo $! > {}; wait", pid_file.display()),
        ];
        let result = run(
            &std::env::temp_dir(),
            &command,
            crate::lang::testing::ALPHA,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.stopped);
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        let alive = tokio::process::Command::new("/bin/kill")
            .args(["-0", &pid])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap()
            .success();
        assert!(!alive, "test process {pid} must be gone");
        let _ = std::fs::remove_file(pid_file);
    }

    /// A runner that exits non-zero without counting a test (pytest's `no tests ran` from an
    /// unusable environment, a build error) reads `no test results (exit N)`, never
    /// `0 passed, 0 failed`; a zero exit keeps the counted line.
    #[tokio::test]
    async fn uncounted_nonzero_exit_reads_no_test_results() {
        let result = run(
            &std::env::temp_dir(),
            &[
                "/bin/sh".into(),
                "-c".into(),
                "echo 'no tests ran in 0.01s'; exit 2".into(),
            ],
            crate::lang::testing::ALPHA,
            Duration::from_secs(10),
        )
        .await;
        assert_eq!(result.exit, Some(2));
        let line = terminal_fact(1, &result, true).reply_text(1);
        assert!(
            line.starts_with("tests #1: no test results (exit 2), "),
            "{line}"
        );
        let counted_zero = RunResult {
            report: TestReport::default(),
            exit: Some(0),
            ..result.clone()
        };
        let line = terminal_fact(1, &counted_zero, true).reply_text(1);
        assert!(line.starts_with("tests #1: 0 passed, 0 failed, "), "{line}");
        let failed = RunResult {
            report: TestReport {
                failed: 1,
                ..TestReport::default()
            },
            ..result
        };
        assert!(
            terminal_fact(1, &failed, true)
                .reply_text(1)
                .starts_with("tests #1: 0 passed, 1 failed, ")
        );
    }

    /// Refuses a second live test job for the same worktree.
    #[tokio::test]
    async fn second_job_is_refused_while_first_runs() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir().to_path_buf();
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/sh".into(), "-c".into(), "sleep 1".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(3),
                "r1".into(),
                &BindingRef::fixture("actor-1", "channel-1", 1),
            ),
            StartResult::Started(1)
        ));
        assert!(matches!(
            runs.start(
                root,
                vec!["true".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(3),
                "r2".into(),
                &BindingRef::fixture("actor-2", "channel-2", 1),
            ),
            StartResult::Running(1, _)
        ));
    }

    /// A status lookup answers the run's own actor and channel in any binding generation, and
    /// refuses another actor's retained run in the same worktree, whose failures and rerun
    /// command stay with the actor that started it.
    #[tokio::test]
    async fn status_lookup_is_scoped_to_the_starting_actor() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir().to_path_buf();
        let owner = BindingRef::fixture("actor-a", "channel-a", 1);
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/echo".into(), "pass".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "ra".into(),
                &owner,
            ),
            StartResult::Started(1)
        ));
        let later = BindingRef::fixture("actor-a", "channel-a", 2);
        let mut settled = false;
        for _ in 0..500 {
            if runs
                .get(&root, 1, &later)
                .is_some_and(|status| status.result.is_some())
            {
                settled = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(settled, "the owner's next generation reads the settled run");
        let other = BindingRef::fixture("actor-b", "channel-b", 1);
        assert!(
            runs.get(&root, 1, &other).is_none(),
            "another actor in the same worktree does not read the run"
        );
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/echo".into(), "pass".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "rb".into(),
                &other,
            ),
            StartResult::Started(2)
        ));
    }

    /// Reports an uncollected run with its prepared replay until the owner reads the settled result.
    #[tokio::test]
    async fn uncollected_reports_runs_until_the_owner_reads_them() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir().to_path_buf();
        let owner = BindingRef::fixture("uncollected-actor", "uncollected-channel", 1);
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/echo".into(), "done".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "uncollected-detail".into(),
                &owner,
            ),
            StartResult::Started(1)
        ));
        let binding = owner.fingerprint();
        assert_eq!(
            runs.uncollected(&binding),
            vec![(
                1,
                vec![
                    "/usr/bin/env".into(),
                    format!("PATH={}", std::env::var("PATH").unwrap_or_default()),
                    "/bin/echo".into(),
                    "done".into(),
                ],
                true,
            )]
        );
        for _ in 0..100 {
            if runs
                .get(&root, 1, &owner)
                .is_some_and(|status| status.result.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(runs.uncollected(&binding).is_empty());
    }

    /// Stop reports a real failed result without an extra fetch, foreign access or an invalid output link.
    #[tokio::test]
    async fn stop_summaries_preserve_settled_failure_without_explicit_read() {
        let runs = TestRuns::default();
        let owner = BindingRef::fixture("stop-result-owner", "stop-result-channel", 1);
        let other = BindingRef::fixture("stop-result-other", "stop-other-channel", 1);
        let id = match runs.start(
            std::env::temp_dir().to_path_buf(),
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "printf 'retained-failure-sentinel\\n'; exit 7".into(),
            ],
            crate::lang::testing::ALPHA,
            Duration::from_secs(10),
            "stop-result-detail".into(),
            &owner,
        ) {
            StartResult::Started(id) => id,
            _ => panic!("the command must start"),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while runs.settled_result(id).is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "command must settle"
            );
            tokio::task::yield_now().await;
        }
        let summaries = runs.stop_summaries(&owner.fingerprint()).join("\n");
        assert!(summaries.contains("exit 7"), "{summaries}");
        assert!(
            summaries.contains("retained-failure-sentinel"),
            "{summaries}"
        );
        assert!(
            !summaries.contains("full output:"),
            "a stopped activation has no output ledger"
        );
        assert!(runs.stop_summaries(&other.fingerprint()).is_empty());
        assert_eq!(
            runs.uncollected(&owner.fingerprint()).len(),
            1,
            "snapshotting is not a delivery acknowledgement"
        );
        // Exercise the live-state renderer independently of OS scheduling.
        let result = runs
            .0
            .lock()
            .unwrap()
            .jobs
            .get_mut(&id)
            .unwrap()
            .result
            .take();
        let pending = runs.stop_summaries(&owner.fingerprint()).join("\n");
        assert!(
            pending.starts_with(&format!("tests #{id}: running — ")),
            "{pending}"
        );
        assert!(
            pending.contains("retained-failure-sentinel"),
            "the live recipe remains visible without claiming its exit: {pending}"
        );
        runs.0.lock().unwrap().jobs.get_mut(&id).unwrap().result = result;

        runs.observe_binding(&owner.fingerprint());
        assert!(runs.stop_summaries(&owner.fingerprint()).is_empty());
    }

    /// A run its owner's `ide.stop` observed neither stays uncollected after it settles nor keeps
    /// the daemon busy.
    #[tokio::test]
    async fn stop_observes_the_runs_of_its_owner() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir().to_path_buf();
        let owner = BindingRef::fixture("observed-actor", "observed-channel", 1);
        let binding = owner.fingerprint();
        assert!(matches!(
            runs.start(
                root.clone(),
                vec!["/bin/echo".into(), "done".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                "observed-detail".into(),
                &owner,
            ),
            StartResult::Started(1)
        ));
        runs.observe_binding(&binding);
        for _ in 0..100 {
            if runs
                .get(&root, 1, &owner)
                .is_some_and(|status| status.result.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(runs.uncollected(&binding).is_empty());
        assert!(!runs.is_busy());
    }

    /// Both command modes apply prefix argv, resolved variables and PATH ahead of caller values.
    #[tokio::test]
    async fn environment_command_prefix_path_vars_and_language_ownership() {
        crate::lang::testing::install();
        let root = std::env::temp_dir().canonicalize().unwrap();
        let argv = ["env-fixture-runner".into()];
        for language in [None, Some(crate::lang::testing::ALPHA)] {
            let child = spawn_command(
                &root,
                &root,
                &argv,
                &[
                    ("PATH".into(), "/caller/bin".into()),
                    ("ENV_FIXTURE".into(), "caller".into()),
                ],
                language,
            )
            .unwrap();
            let result = run_child(
                crate::lang::testing::ALPHA,
                Duration::from_secs(2),
                child,
                &std::env::temp_dir(),
            )
            .await;
            assert!(
                result.output.starts_with(&format!(
                    "resolved\n{}:/caller/bin\n",
                    root.join("env-bin").display()
                )),
                "{}",
                result.output
            );
            assert_eq!(result.report.passed, 1);
        }
        let Err(error) = spawn_command(&root, &root, &argv, &[], Some(crate::lang::testing::BETA))
        else {
            panic!("a program on no PATH starts nothing");
        };
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let mut result = failed_run(Duration::from_secs(1), String::new());
        result.report.passed = 1;
        result.environment_label = Some("two".into());
        assert_eq!(
            terminal_fact(1, &result, true).summary,
            "1 passed, 0 failed, 0 s (exit unknown) · env two"
        );
    }
}
