//! Explicit, daemon-owned background test jobs and their bounded runner output.

use std::{
    collections::{BTreeMap, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::lang::{Language, TestReport};

/// Maximum combined stdout and stderr retained for `ide.inspect`.
const MAX_OUTPUT: usize = 256 * 1024;
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
    /// Last no-feed status plate delivered per worktree.
    delivered_status: BTreeMap<PathBuf, String>,
}

/// One job's worktree, command, timing, and optional completed report/output.
struct Job {
    /// Authorized worktree from which the process runs.
    root: PathBuf,
    /// Binding that owns the original output detail reference.
    owner: [u8; 32],
    /// Monotonic start time used for status ages.
    started: tokio::time::Instant,
    /// Absent while active, present after exit, spawn failure, or budget expiry.
    result: Option<RunResult>,
    /// Monotonic completion time for the bounded result lifetime.
    completed_at: Option<tokio::time::Instant>,
    /// True after an explicit `status` request has retrieved the completed result.
    observed: bool,
}

/// Captured parser summary, bounded output, and whether the budget killed the child.
#[derive(Clone)]
pub struct RunResult {
    /// Counts and failures explicitly parsed from runner output.
    pub report: TestReport,
    /// Last at most 256 KiB of combined stdout and stderr.
    pub output: String,
    /// Elapsed wall-clock process duration.
    pub elapsed: Duration,
    /// True when the process was killed at its time budget.
    pub stopped: bool,
    /// Budget configured for this process.
    pub budget: Duration,
    /// Detail reference retained under the binding that started the process.
    pub detail_ref: String,
    /// Original argv shown for manual reruns.
    pub command: Vec<String>,
}

/// Result of attempting to start a job in one worktree.
pub enum StartResult {
    /// The command spawned and its daemon-owned job id.
    Started(u64),
    /// An existing live job id and its age; no second command was spawned.
    Running(u64, Duration),
    /// A local configuration or operating-system error prevented command spawn.
    Failed(String),
}

/// Read-only age and optional completed result returned by a status lookup.
pub struct JobStatus {
    /// Monotonic elapsed age of the job.
    pub age: Duration,
    /// Binding that owns the retained output detail reference.
    pub owner: [u8; 32],
    /// Parsed report and bounded output after the process settles.
    pub result: Option<RunResult>,
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
    pub fn start(
        &self,
        root: PathBuf,
        argv: Vec<String>,
        language: Language,
        budget: Duration,
        detail_ref: String,
        owner: [u8; 32],
    ) -> StartResult {
        let mut state = match self.0.lock() {
            Ok(state) => state,
            Err(error) => return StartResult::Failed(error.to_string()),
        };
        if let Some((id, job)) = state
            .jobs
            .iter()
            .find(|(_, job)| job.root == root && job.result.is_none())
        {
            return StartResult::Running(*id, job.started.elapsed());
        }
        let child = match spawn_command(&root, &argv) {
            Ok(child) => child,
            Err(error) => return StartResult::Failed(error.to_string()),
        };
        state.next_id = state.next_id.saturating_add(1);
        let id = state.next_id;
        let started = tokio::time::Instant::now();
        state.jobs.insert(
            id,
            Job {
                root: root.clone(),
                owner,
                started,
                result: None,
                completed_at: None,
                observed: false,
            },
        );
        let registry = self.0.clone();
        tokio::spawn(async move {
            let mut result = run_child(language, budget, child).await;
            result.detail_ref = detail_ref;
            result.command = argv;
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

    /// Returns the same-worktree job's age, owner, and result; only its starting binding's status
    /// lookup marks retained output observed for daemon idleness.
    pub fn get(&self, root: &PathBuf, id: u64, binding: &[u8; 32]) -> Option<JobStatus> {
        let mut state = self.0.lock().ok()?;
        let job = state.jobs.get_mut(&id).filter(|job| &job.root == root)?;
        if job.result.is_some() && &job.owner == binding {
            job.observed = true;
        }
        Some(JobStatus {
            age: job.started.elapsed(),
            owner: job.owner,
            result: job.result.clone(),
        })
    }

    /// Returns one daemon-global job's status for its starting binding, without a worktree: the
    /// handle an agent invented for `ide.inspect` (`tests #N`) names a run, not a detail.
    pub fn find(&self, id: u64, owner: &[u8; 32]) -> Option<JobStatus> {
        let mut state = self.0.lock().ok()?;
        let job = state.jobs.get_mut(&id).filter(|job| &job.owner == owner)?;
        if job.result.is_some() {
            job.observed = true;
        }
        Some(JobStatus {
            age: job.started.elapsed(),
            owner: job.owner,
            result: job.result.clone(),
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

    /// Drops buffered output after its paged copy is retained by the owner's detail ledger.
    pub fn clear_output(&self, root: &PathBuf, id: u64, binding: &[u8; 32]) {
        if let Ok(mut state) = self.0.lock()
            && let Some(job) = state.jobs.get_mut(&id)
            && &job.root == root
            && &job.owner == binding
            && let Some(result) = &mut job.result
        {
            result.output.clear();
            result.output.shrink_to_fit();
        }
    }

    /// Returns the current compact status line for the newest run in `root`.
    pub fn status_line(&self, root: &PathBuf) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (id, job) = state.jobs.iter().rev().find(|(_, job)| &job.root == root)?;
        render_status_line(*id, job)
    }

    /// Returns an undelivered current status line owned by one binding.
    pub fn status_line_for_binding(&self, binding: &[u8; 32]) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (_, owner_job) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.owner == binding)?;
        let root = &owner_job.root;
        let (id, job) = state.jobs.iter().rev().find(|(_, job)| &job.root == root)?;
        let line = render_status_line(*id, job)?;
        (state.delivered_status.get(root) != Some(&line)).then_some(line)
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
        let root = owner_job.root.clone();
        let owner_id = *owner_id;
        let is_current = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| job.root == root)
            .and_then(|(id, job)| render_status_line(*id, job))
            .as_deref()
            == Some(line);
        state.delivered_status.insert(root, line.to_owned());
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
        let Some(id) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.root == root && job.result.is_some())
            .map(|(id, _)| *id)
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
        state.delivered_status.insert(root.clone(), line.to_owned());
        if let Some(job) = state.jobs.get_mut(&id) {
            job.observed = true;
        }
        true
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
        Some(result) if result.stopped => format!(
            "tests #{id}: stopped at budget {} s — {} passed, {} failed so far",
            result.budget.as_secs(),
            result.report.passed,
            result.report.failed
        ),
        Some(result)
            if result.report.passed == 0
                && result.report.failed == 0
                && result.report.incomplete =>
        {
            format!(
                "tests #{id}: no summary parsed, {} s — inspect the runner's full output with ide.inspect",
                result.elapsed.as_secs()
            )
        }
        Some(result) => format!(
            "tests #{id}: {} passed, {} failed, {} s",
            result.report.passed,
            result.report.failed,
            result.elapsed.as_secs()
        ),
        None => format!(
            "tests #{id}: running {} s; poll: ide.test {{\"status\": {id}}}",
            job.started.elapsed().as_secs()
        ),
    })
}

/// Runs one command with inherited environment, bounded output, a process-group budget kill,
/// and language-native output parsing.
#[cfg(test)]
async fn run(root: &PathBuf, argv: &[String], language: Language, budget: Duration) -> RunResult {
    match spawn_command(root, argv) {
        Ok(child) => run_child(language, budget, child).await,
        Err(error) => failed_run(budget, error.to_string()),
    }
}

/// Spawns an exact argv from the worktree with inherited environment and a private process group.
///
/// When a registered language pins the toolchain of the command's program (see
/// [`LanguageSupport::test_toolchain`](crate::lang::LanguageSupport::test_toolchain)), that
/// executable runs instead and its directory leads `PATH`, so the tools it starts resolve from
/// the same toolchain rather than through a version-manager proxy.
fn spawn_command(root: &PathBuf, argv: &[String]) -> io::Result<tokio::process::Child> {
    let Some((program, args)) = argv.split_first() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty argv"));
    };
    let toolchain = crate::lang::registered()
        .iter()
        .find_map(|language| language.support().test_toolchain(program));
    let executable = toolchain.as_ref().map_or_else(
        || PathBuf::from(program),
        |(executable, _)| executable.clone(),
    );
    let mut command = tokio::process::Command::new(executable);
    if let Some((_, bin)) = &toolchain {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut joined = std::ffi::OsString::from(bin);
        joined.push(":");
        joined.push(path);
        command.env("PATH", joined);
    }
    command
        .args(args)
        .current_dir(root)
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
    let mut report = language.support().parse_test_output(&output, "");
    report.incomplete |= stopped || status.is_none();
    RunResult {
        report,
        output,
        elapsed: started.elapsed(),
        stopped,
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
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
        elapsed: Duration::ZERO,
        stopped: false,
        budget,
        detail_ref: String::new(),
        command: Vec::new(),
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
                [1; 32]
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
                [2; 32]
            ),
            StartResult::Running(1, _)
        ));
    }
}
