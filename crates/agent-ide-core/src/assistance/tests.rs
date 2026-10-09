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

/// Maximum combined stdout and stderr retained for `ide.inspect`.
const MAX_OUTPUT: usize = 256 * 1024;
/// Maximum output tail retained beside the paged full result.
const OUTPUT_TAIL_BYTES: usize = 4 * 1024;
/// Completed results stay queryable for ten minutes unless observed earlier.
const COMPLETED_TTL: Duration = Duration::from_secs(600);
/// Pipe drain grace after a budget kill; never extends a run indefinitely for an orphan reader.
const READER_DRAIN_GRACE: Duration = Duration::from_millis(500);
/// At most this many completed jobs remain per worktree.
const COMPLETED_PER_WORKTREE: usize = 4;

/// One daemon's monotonically numbered test jobs, independent of host binding lifetimes.
#[derive(Clone, Default)]
pub struct TestRuns(
    /// Registry state shared between the daemon worker and detached process tasks.
    Arc<Mutex<State>>,
);

/// Mutable run registry protected only while job metadata is read or changed.
#[derive(Default)]
struct State {
    /// Highest allocated daemon-local identifier; identifiers start at one and never repeat.
    next_id: u64,
    /// Retained jobs keyed by daemon-local id and partitioned by `Job::root`.
    jobs: BTreeMap<u64, Job>,
    /// Last no-feed status plate delivered per worktree and starting binding.
    delivered_status: BTreeMap<(PathBuf, [u8; 32]), String>,
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
    /// True after an explicit `status` request has retrieved the completed result.
    observed: bool,
    /// True once the owner's `ide.stop` ended the activation that started the run: its status is
    /// still readable by handle, but no later plate of the same session announces it.
    retired: bool,
}

/// Captured parser summary, bounded output, and whether the budget killed the child.
#[derive(Clone)]
pub struct RunResult {
    /// Counts and failures explicitly parsed from runner output.
    pub report: TestReport,
    /// Last at most 256 KiB of combined stdout and stderr; once paged, only its bounded runner
    /// line.
    pub output: String,
    /// `true` once the owner's retained detail took over the whole output as its pages and this
    /// copy shrank to the runner line; later replies then point at that detail instead of
    /// quoting a head.
    pub output_paged: bool,
    /// Elapsed wall-clock process duration.
    pub elapsed: Duration,
    /// True when the process was killed at its time budget.
    pub stopped: bool,
    /// Process exit code; absent after a budget kill, a signal, or a spawn failure.
    pub exit: Option<i32>,
    /// Budget configured for this process.
    pub budget: Duration,
    /// Detail reference retained under the binding that started the process.
    pub detail_ref: String,
    /// Argv shown for manual reruns: the original argv, behind `env NAME=VALUE …` when the run set
    /// environment overrides.
    pub command: Vec<String>,
    /// Worktree-relative directory the run used when it was not the worktree root, shown before the
    /// rerun command so the line reproduces the run.
    pub rerun_dir: Option<String>,
    /// Environment chosen at launch, shown only when multiple candidates or a selection exist.
    pub environment_label: Option<String>,
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
) -> Result<CommandResolution, crate::modules::contract::ModuleUnavailable> {
    let Some(program) = argv.first() else {
        return Ok(CommandResolution::default());
    };
    let mut environment_language = command_language;
    let mut resolved = None;
    match command_language {
        Some(language) => {
            resolved = crate::modules::calls::command_env(language, root, cwd, program).await?;
        }
        None => {
            for &language in crate::lang::registered() {
                if let Some(env) =
                    crate::modules::calls::command_env(language, root, cwd, program).await?
                {
                    resolved = Some(env);
                    environment_language = Some(language);
                    break;
                }
            }
        }
    }
    let environment_label = match environment_language {
        Some(language) => command_environment_label(root, cwd, language).await?,
        None => None,
    };
    Ok(CommandResolution {
        resolved,
        environment_label,
    })
}

/// Sends a best-effort process-group kill if daemon shutdown drops a running task.
#[cfg(unix)]
struct KillOnDrop {
    /// Process-group identifier assigned by `process_group(0)`.
    pid: u32,
    /// False after the process has been waited and its group is no longer owned.
    armed: bool,
}

#[cfg(unix)]
impl Drop for KillOnDrop {
    /// Kills remaining descendants without blocking the daemon task shutdown path.
    fn drop(&mut self) {
        if self.armed {
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{}", self.pid)])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
}

impl TestRuns {
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
        let child = match spawn_resolved(&root, &cwd, &argv, &env, resolution.resolved.as_ref()) {
            Ok(child) => child,
            Err(error) => {
                lease.settled();
                return StartResult::Failed {
                    not_found: error.kind() == std::io::ErrorKind::NotFound,
                    error: error.to_string(),
                };
            }
        };
        // The rerun line reproduces the run: its directory and environment overrides included.
        let rerun_dir = cwd
            .strip_prefix(&root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.display().to_string());
        let rerun_command: Vec<String> = if env.is_empty() {
            argv.clone()
        } else {
            std::iter::once("env".to_owned())
                .chain(env.iter().map(|(name, value)| format!("{name}={value}")))
                .chain(argv.iter().cloned())
                .collect()
        };
        state.next_id = state.next_id.saturating_add(1);
        let id = state.next_id;
        let started = tokio::time::Instant::now();
        state.jobs.insert(
            id,
            Job {
                root: root.clone(),
                owner: owner.fingerprint(),
                channel: owner.channel_identity().fingerprint(),
                detail_ref: detail_ref.clone(),
                command: rerun_command.clone(),
                explicit_command: false,
                started,
                result: None,
                completed_at: None,
                observed: false,
                retired: false,
            },
        );
        let environment_label = resolution.environment_label;
        let registry = self.0.clone();
        tokio::spawn(async move {
            let mut result = run_child(language, budget, child, &root).await;
            lease.settled();
            result.detail_ref = detail_ref;
            result.command = rerun_command;
            result.rerun_dir = rerun_dir;
            result.environment_label = environment_label;
            if let Ok(mut state) = registry.lock()
                && let Some(job) = state.jobs.get_mut(&id)
            {
                job.result = Some(result);
                job.completed_at = Some(tokio::time::Instant::now());
            }
            prune_completed(&registry, &root);
        });
        StartResult::Started(id)
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

    /// Marks every job started by `binding` observed so stopping that actor cannot pin idle exit,
    /// and retired so a later activation of the same session is not shown its status.
    pub fn observe_binding(&self, binding: &[u8; 32]) {
        if let Ok(mut state) = self.0.lock() {
            for job in state.jobs.values_mut().filter(|job| &job.owner == binding) {
                job.observed = true;
                job.retired = true;
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
    /// running, or a settled one no status, handle or plate delivered — as `(id, argv, running)`.
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

    /// Returns the current compact status line for the newest run in `root`.
    pub fn status_line(&self, root: &PathBuf) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (id, job) = state.jobs.iter().rev().find(|(_, job)| &job.root == root)?;
        render_status_line(*id, job)
    }

    /// Returns an undelivered current status line for the newest run owned by one binding. A run
    /// its `ide.stop` retired ([`TestRuns::observe_binding`]) belongs to the ended activation: a
    /// later activation of the same session is not told about it.
    pub fn status_line_for_binding(&self, binding: &[u8; 32]) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (id, job) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.owner == binding)?;
        if job.retired {
            return None;
        }
        let line = render_status_line(*id, job)?;
        (state.delivered_status.get(&(job.root.clone(), *binding)) != Some(&line)).then_some(line)
    }

    /// Marks one exact current status line delivered for its starting binding.
    pub fn mark_status_delivered(&self, binding: &[u8; 32], line: &str) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        let Some((owner_id, owner_job)) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.owner == binding)
        else {
            return false;
        };
        let key = (owner_job.root.clone(), *binding);
        let owner_id = *owner_id;
        let is_current = state
            .jobs
            .get(&owner_id)
            .and_then(|job| render_status_line(owner_id, job))
            .as_deref()
            == Some(line);
        state.delivered_status.insert(key, line.to_owned());
        if is_current
            && let Some(job) = state.jobs.get_mut(&owner_id)
            && job.result.is_some()
        {
            job.observed = true;
        }
        true
    }

    /// Marks a completed result observed after its worktree feed delivered that exact line.
    pub fn mark_feed_status_delivered(&self, root: &PathBuf, line: &str) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        let Some((id, owner)) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.root == root && job.result.is_some())
            .map(|(id, job)| (*id, job.owner))
        else {
            return false;
        };
        if state
            .jobs
            .get(&id)
            .and_then(|job| render_status_line(id, job))
            .as_deref()
            != Some(line)
        {
            return false;
        }
        state
            .delivered_status
            .insert((root.clone(), owner), line.to_owned());
        if let Some(job) = state.jobs.get_mut(&id) {
            job.observed = true;
        }
        true
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
            state.jobs.remove(&completed.remove(0));
        }
    }
}

/// Renders one job's current compact status without consuming its delivery state.
fn render_status_line(id: u64, job: &Job) -> Option<String> {
    Some(match &job.result {
        Some(result) => settled_line(id, result, false),
        None => format!(
            "tests #{id}: running {} s; poll: call ide.test with {{\"status\": {id}}}",
            job.started.elapsed().as_secs()
        ),
    })
}

/// Renders one settled run's first status line for the full result.
///
/// A run that counted no test is never shown as `0 passed, 0 failed`: a non-zero exit says the
/// runner could not run (`no test results (exit N)` plus a bounded output excerpt and detail ref),
/// and a missing summary says so.
pub fn result_line(id: u64, result: &RunResult) -> String {
    settled_line(id, result, true)
}

/// Suffix that names a run's full output in reply lines; status plates never carry it.
const FULL_OUTPUT: &str = "; full output: ide.inspect ";

/// The status-plate form of a reply's first line: the line without its full-output reference.
/// The plate is length-bounded, and a cut reference would name a detail that was never issued.
pub fn plate_line(line: &str) -> &str {
    line.split_once(FULL_OUTPUT).map_or(line, |(head, _)| head)
}

/// Renders the terminal status, optionally with a full-output reference and launch environment.
fn settled_line(id: u64, result: &RunResult, with_ref: bool) -> String {
    let report = &result.report;
    let seconds = result.elapsed.as_secs();
    let counted = report.passed != 0 || report.failed != 0;
    let reference = if with_ref {
        format!("{FULL_OUTPUT}{}", result.detail_ref)
    } else {
        String::new()
    };
    let mut line = if result.stopped {
        format!(
            "tests #{id}: stopped at budget {} s — {} passed, {} failed so far",
            result.budget.as_secs(),
            report.passed,
            report.failed
        )
    } else if let Some(code) = result.exit.filter(|code| *code != 0 && !counted) {
        format!(
            "tests #{id}: no test results (exit {code}), {seconds} s — runner said: {}{reference}",
            runner_excerpt(&result.output)
        )
    } else if !counted && report.incomplete {
        let summary = format!("tests #{id}: no summary parsed, {seconds} s");
        if let Some(code) = result.exit.filter(|code| *code != 0) {
            format!(
                "{summary} (exit {code}) — runner said: {}{reference}",
                runner_excerpt(&result.output)
            )
        } else {
            format!("{summary} — inspect the runner's full output with ide.inspect")
        }
    } else {
        format!(
            "tests #{id}: {} passed, {} failed, {seconds} s",
            report.passed, report.failed
        )
    };
    if let Some(label) = &result.environment_label {
        let at = line.find(FULL_OUTPUT).unwrap_or(line.len());
        line.insert_str(at, &format!(" · env {label}"));
    }
    line
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

/// Reports whether a run lacks a recognized summary with test counts and was not budget-stopped.
/// Explicit commands and selected runners both use this to return the exit status and output tail.
pub fn summary_absent(result: &RunResult) -> bool {
    !result.stopped
        && result.report.incomplete
        && result.report.passed == 0
        && result.report.failed == 0
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
        Ok(child) => run_child(language, budget, child, root).await,
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
async fn command_environment_label(
    worktree: &std::path::Path,
    cwd: &std::path::Path,
    language: Language,
) -> Result<Option<String>, crate::modules::contract::ModuleUnavailable> {
    let environments = crate::modules::calls::environments(language, worktree).await?;
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
) -> io::Result<tokio::process::Child> {
    let resolved = argv.first().and_then(|program| match language {
        Some(language) => language.support().command_env(worktree, cwd, program),
        None => crate::lang::registered()
            .iter()
            .find_map(|language| language.support().command_env(worktree, cwd, program)),
    });
    spawn_resolved(worktree, cwd, argv, env, resolved.as_ref())
}

/// Spawns argv in its resolved command environment. The worktree and cwd are already admitted.
/// Resolver variables override caller values and its prefix leads PATH. Stdin is closed,
/// stdout/stderr captured and a private process group permits cancellation; empty argv and OS
/// failures return errors without a child.
fn spawn_resolved(
    _worktree: &std::path::Path,
    cwd: &std::path::Path,
    argv: &[String],
    env: &[(String, String)],
    resolved: Option<&crate::lang::environment::CommandEnv>,
) -> io::Result<tokio::process::Child> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    };
    let mut prefix = resolved
        .as_ref()
        .map_or_else(|| vec![program.into()], |env| env.argv_prefix.clone());
    if prefix.is_empty() {
        prefix.push(program.into());
    }
    let mut command = tokio::process::Command::new(&prefix[0]);
    command
        .args(&prefix[1..])
        .envs(env.iter().map(|(key, value)| (key, value)));
    if let Some(resolved) = resolved {
        command.envs(resolved.vars.iter().map(|(key, value)| (key, value)));
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
            command.env("PATH", prepend_toolchain_path(bin, &caller_path)?);
        }
    }
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    command.spawn()
}

/// Runs one already-spawned child, enforcing its wall-clock budget and bounded pipe drain.
async fn run_child(
    language: Language,
    budget: Duration,
    mut child: tokio::process::Child,
    root: &std::path::Path,
) -> RunResult {
    let started = tokio::time::Instant::now();
    let mut stopped = false;
    let process_group = child.id();
    #[cfg(unix)]
    let mut kill_on_drop = process_group.map(|pid| KillOnDrop { pid, armed: true });
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let combined = Arc::new(tokio::sync::Mutex::new(VecDeque::with_capacity(MAX_OUTPUT)));
    let mut out_task = tokio::spawn(read_into_tail(stdout, combined.clone()));
    let mut err_task = tokio::spawn(read_into_tail(stderr, combined.clone()));
    let status = match tokio::time::timeout(budget, child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            stopped = true;
            #[cfg(unix)]
            if let Some(pid) = process_group {
                let _ = kill_group(pid).await;
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            None
        }
    };
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
        #[cfg(unix)]
        if let Some(pid) = process_group {
            let _ = kill_group(pid).await;
        }
        out_task.abort();
        err_task.abort();
    }
    #[cfg(unix)]
    if let Some(guard) = &mut kill_on_drop {
        guard.armed = false;
    }
    let bytes = combined.lock().await.iter().copied().collect::<Vec<_>>();
    let output = String::from_utf8_lossy(&bytes).into_owned();
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
    report.incomplete |= stopped || status.is_none();
    RunResult {
        report,
        output,
        output_paged: false,
        elapsed: started.elapsed(),
        stopped,
        exit: status.and_then(|status| status.code()),
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
        rerun_dir: None,
        environment_label: None,
    }
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
        exit: None,
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
        rerun_dir: None,
        environment_label: None,
    }
}

/// Reads one pipe to EOF, appending into a shared combined tail buffer.
async fn read_into_tail<R: tokio::io::AsyncRead + Unpin>(
    reader: Option<R>,
    retained: Arc<tokio::sync::Mutex<VecDeque<u8>>>,
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
                if retained.len() > MAX_OUTPUT {
                    let excess = retained.len() - MAX_OUTPUT;
                    retained.drain(..excess);
                }
            }
        }
    }
}

/// Sends SIGKILL to the isolated child process group through the platform utility.
#[cfg(unix)]
async fn kill_group(pid: u32) -> io::Result<std::process::ExitStatus> {
    tokio::process::Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
}

#[cfg(test)]
mod runner_tests {
    //! Minimal process and parser checks for the test runner.
    use super::*;

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

    /// Delivery for one actor does not make another actor's unchanged plate due again.
    #[test]
    fn status_delivery_is_partitioned_by_root_and_binding() {
        let runs = TestRuns::default();
        let root = PathBuf::from("/tmp/shared-worktree");
        let first = [1; 32];
        let second = [2; 32];
        {
            let mut state = runs.0.lock().unwrap();
            for (id, owner) in [(1, first), (2, second)] {
                state.jobs.insert(
                    id,
                    Job {
                        root: root.clone(),
                        owner,
                        channel: [id as u8; 32],
                        detail_ref: format!("detail-{id}"),
                        command: vec!["check".to_owned()],
                        explicit_command: true,
                        started: tokio::time::Instant::now(),
                        result: None,
                        completed_at: None,
                        observed: false,
                        retired: false,
                    },
                );
            }
        }
        let first_line = runs.status_line_for_binding(&first).unwrap();
        assert!(runs.mark_status_delivered(&first, &first_line));
        let second_line = runs.status_line_for_binding(&second).unwrap();
        assert!(runs.mark_status_delivered(&second, &second_line));
        assert_eq!(runs.status_line_for_binding(&first), None);
        assert_eq!(runs.status_line_for_binding(&second), None);
    }

    /// Empty summaries surface the first error line, falling back to the final output line.
    #[test]
    fn empty_summary_quotes_bounded_runner_reason() {
        let mut result = failed_run(
            Duration::from_secs(10),
            "noise\nERROR: missing collectors\nlast\n".into(),
        );
        result.exit = Some(4);
        assert_eq!(
            result_line(3, &result),
            format!(
                "tests #3: no test results (exit 4), 0 s — runner said: ERROR: missing collectors; full output: ide.inspect {}",
                result.detail_ref
            )
        );
        // The status plate keeps the reason but never a reference it might cut, and the reason
        // survives the output being paged into the detail ledger.
        let plate =
            "tests #3: no test results (exit 4), 0 s — runner said: ERROR: missing collectors";
        assert_eq!(settled_line(3, &result, false), plate);
        // The reply's first line, as the dispatcher snapshots it for the plate, agrees.
        assert_eq!(plate_line(&result_line(3, &result)), plate);
        result.output = runner_excerpt(&result.output);
        assert_eq!(settled_line(3, &result, false), plate);
        result.output = "noise\nlast line\n".into();
        assert!(
            result_line(3, &result).contains("runner said: last line; full output: ide.inspect")
        );
        result.output = "x".repeat(300);
        assert!(runner_excerpt(&result.output).len() <= 200);
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
        assert_eq!(result.command, ["env", "RUN_MARKER=x", "/bin/echo", "done"]);
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
        let line = result_line(1, &result);
        assert!(
            line.starts_with("tests #1: no test results (exit 2), "),
            "{line}"
        );
        let counted_zero = RunResult {
            report: TestReport::default(),
            exit: Some(0),
            ..result.clone()
        };
        assert!(
            result_line(1, &counted_zero).starts_with("tests #1: 0 passed, 0 failed, "),
            "{}",
            result_line(1, &counted_zero)
        );
        let failed = RunResult {
            report: TestReport {
                failed: 1,
                ..TestReport::default()
            },
            ..result
        };
        assert!(result_line(1, &failed).starts_with("tests #1: 0 passed, 1 failed, "));
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
    /// command stay with the actor that started it. Plates continue to show the owner's run when
    /// a later activation starts another actor's job in the same worktree.
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
        for _ in 0..500 {
            if runs
                .get(&root, 2, &other)
                .is_some_and(|status| status.result.is_some())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let owner_plate = runs.status_line_for_binding(&owner.fingerprint()).unwrap();
        let other_plate = runs.status_line_for_binding(&other.fingerprint()).unwrap();
        assert!(owner_plate.starts_with("tests #1:"), "{owner_plate}");
        assert!(other_plate.starts_with("tests #2:"), "{other_plate}");
    }

    /// Reports an uncollected run with its command until the owner reads the settled result.
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
            vec![(1, vec!["/bin/echo".into(), "done".into()], true)]
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

    /// A run settled after its owner's `ide.stop` is never announced to a later activation of the
    /// same session (same binding), while a run that activation starts is.
    #[tokio::test]
    async fn stop_retires_runs_from_later_status_plates() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir().to_path_buf();
        let owner = BindingRef::fixture("retired-actor", "retired-channel", 1);
        let binding = owner.fingerprint();
        let start = |detail: &str| {
            runs.start(
                root.clone(),
                vec!["/bin/echo".into(), "done".into()],
                crate::lang::testing::ALPHA,
                Duration::from_secs(10),
                detail.into(),
                &owner,
            )
        };
        assert!(matches!(start("retired-old"), StartResult::Started(1)));
        runs.observe_binding(&binding);
        // Waits through the worktree status line, which reads nothing on the owner's behalf.
        for _ in 0..100 {
            if runs
                .status_line(&root)
                .is_some_and(|line| !line.contains("running"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!runs.status_line(&root).unwrap().contains("running"));
        assert_eq!(runs.status_line_for_binding(&binding), None);
        assert!(matches!(start("retired-new"), StartResult::Started(2)));
        let fresh = runs.status_line_for_binding(&binding).unwrap();
        assert!(fresh.starts_with("tests #2:"), "{fresh}");
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
        let error =
            spawn_command(&root, &root, &argv, &[], Some(crate::lang::testing::BETA)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        let mut result = failed_run(Duration::from_secs(1), String::new());
        result.report.passed = 1;
        result.environment_label = Some("two".into());
        assert_eq!(
            result_line(1, &result),
            "tests #1: 1 passed, 0 failed, 0 s · env two"
        );
    }
}
