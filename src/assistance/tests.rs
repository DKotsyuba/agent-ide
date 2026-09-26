//! Explicit, daemon-owned background test jobs and their bounded runner output.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::lang::{Language, TestReport, support};

/// Maximum combined stdout and stderr retained for `ide.inspect`.
const MAX_OUTPUT: usize = 256 * 1024;

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
    /// Retained jobs, partitioned by each job's canonical worktree root.
    jobs: BTreeMap<u64, Job>,
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
    /// True after an explicit `status` request has retrieved the completed result.
    observed: bool,
    /// Last status line delivered to the owner when no project check feed exists.
    delivered_status: Option<String>,
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
    /// Newly admitted job id.
    Started(u64),
    /// Existing active job id and age.
    Running(u64, Duration),
    /// Command could not be started.
    Failed,
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
            let _ = std::process::Command::new("kill")
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
            Err(_) => return StartResult::Failed,
        };
        if let Some((id, job)) = state
            .jobs
            .iter()
            .find(|(_, job)| job.root == root && job.result.is_none())
        {
            return StartResult::Running(*id, job.started.elapsed());
        }
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
                observed: false,
                delivered_status: None,
            },
        );
        let registry = self.0.clone();
        tokio::spawn(async move {
            let mut result = run(&root, &argv, language, budget).await;
            result.detail_ref = detail_ref;
            result.command = argv;
            if let Ok(mut state) = registry.lock()
                && let Some(job) = state.jobs.get_mut(&id)
            {
                job.result = Some(result);
            }
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

    /// Returns the current compact status line for the newest run in `root`.
    pub fn status_line(&self, root: &PathBuf) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (id, job) = state.jobs.iter().rev().find(|(_, job)| &job.root == root)?;
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
                    "tests #{id}: no summary parsed, {} s",
                    result.elapsed.as_secs()
                )
            }
            Some(result) => format!(
                "tests #{id}: {} passed, {} failed, {} s",
                result.report.passed,
                result.report.failed,
                result.elapsed.as_secs()
            ),
            None => format!("tests #{id}: running {} s", job.started.elapsed().as_secs()),
        })
    }

    /// Returns an undelivered current status line owned by one binding.
    pub fn status_line_for_binding(&self, binding: &[u8; 32]) -> Option<String> {
        let state = self.0.lock().ok()?;
        let (id, job) = state
            .jobs
            .iter()
            .rev()
            .find(|(_, job)| &job.owner == binding)?;
        let line = render_status_line(*id, job)?;
        (job.delivered_status.as_deref() != Some(&line)).then_some(line)
    }

    /// Marks one exact current status line delivered for its starting binding.
    pub fn mark_status_delivered(&self, binding: &[u8; 32], line: &str) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        let Some((id, job)) = state
            .jobs
            .iter_mut()
            .rev()
            .find(|(_, job)| &job.owner == binding)
        else {
            return false;
        };
        if render_status_line(*id, job).as_deref() != Some(line) {
            return false;
        }
        job.delivered_status = Some(line.to_owned());
        job.observed |= job.result.is_some();
        true
    }

    /// Marks a completed result observed after its worktree feed delivered that exact line.
    pub fn mark_feed_status_delivered(&self, root: &PathBuf, line: &str) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        let Some((id, job)) = state
            .jobs
            .iter_mut()
            .rev()
            .find(|(_, job)| &job.root == root)
        else {
            return false;
        };
        if render_status_line(*id, job).as_deref() != Some(line) || job.result.is_none() {
            return false;
        }
        job.observed = true;
        true
    }

    /// Reports whether any daemon-owned test child is still running.
    pub fn is_busy(&self) -> bool {
        self.0.lock().is_ok_and(|state| {
            state
                .jobs
                .values()
                .any(|job| job.result.is_none() || !job.observed)
        })
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
                "tests #{id}: no summary parsed, {} s",
                result.elapsed.as_secs()
            )
        }
        Some(result) => format!(
            "tests #{id}: {} passed, {} failed, {} s",
            result.report.passed,
            result.report.failed,
            result.elapsed.as_secs()
        ),
        None => format!("tests #{id}: running {} s", job.started.elapsed().as_secs()),
    })
}

/// Runs one command with inherited environment, bounded output, a process-group budget kill,
/// and language-native output parsing.
async fn run(root: &PathBuf, argv: &[String], language: Language, budget: Duration) -> RunResult {
    let started = tokio::time::Instant::now();
    let mut report = TestReport {
        incomplete: true,
        ..TestReport::default()
    };
    let mut output = String::new();
    let mut stopped = false;
    if let Some((program, args)) = argv.split_first() {
        let executable = (program == "cargo")
            .then(|| {
                std::env::var_os("AGENT_IDE_RUST_TOOLCHAIN_DIR")
                    .map(PathBuf::from)
                    .map(|root| root.join("bin/cargo"))
            })
            .flatten()
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from(program));
        let mut command = tokio::process::Command::new(executable);
        command
            .args(args)
            .current_dir(root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        if let Ok(mut child) = command.spawn() {
            #[cfg(unix)]
            let mut kill_on_drop = child.id().map(|pid| KillOnDrop { pid, armed: true });
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let combined = Arc::new(tokio::sync::Mutex::new(VecDeque::with_capacity(MAX_OUTPUT)));
            let out_task = tokio::spawn(read_into_tail(stdout, combined.clone()));
            let err_task = tokio::spawn(read_into_tail(stderr, combined.clone()));
            let status = match tokio::time::timeout(budget, child.wait()).await {
                Ok(status) => status.ok(),
                Err(_) => {
                    stopped = true;
                    #[cfg(unix)]
                    if let Some(pid) = child.id() {
                        kill_group(pid).await;
                    }
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    #[cfg(unix)]
                    if let Some(guard) = &mut kill_on_drop {
                        guard.armed = false;
                    }
                    None
                }
            };
            #[cfg(unix)]
            if status.is_some()
                && let Some(guard) = &mut kill_on_drop
            {
                guard.armed = false;
            }
            let _ = (out_task.await, err_task.await);
            let bytes = combined.lock().await.iter().copied().collect::<Vec<_>>();
            output = String::from_utf8_lossy(&bytes).into_owned();
            if let Some(parser) = support(language) {
                report = parser.parse_test_output(&output, "");
            }
            if stopped || status.is_none() {
                report.incomplete = true;
            }
        }
    }
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
async fn kill_group(pid: u32) {
    let _ = tokio::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
}

#[cfg(test)]
mod runner_tests {
    //! Minimal process and parser checks for the test runner.
    use super::*;

    /// Uses a Rust test transcript to prove captured output reaches the language parser.
    #[tokio::test]
    async fn fake_rust_test_command_is_parsed() {
        let root = std::env::temp_dir();
        let script = "printf 'running 1 test\\ntest demo ... ok\\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'";
        let result = run(
            &root,
            &["/bin/sh".into(), "-c".into(), script.into()],
            Language::Rust,
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
            format!("echo $$ > {}; exec sleep 30", pid_file.display()),
        ];
        let result = run(
            &std::env::temp_dir(),
            &command,
            Language::Rust,
            Duration::from_secs(1),
        )
        .await;
        assert!(result.stopped);
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_owned();
        let alive = tokio::process::Command::new("kill")
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
                Language::Rust,
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
                Language::Rust,
                Duration::from_secs(3),
                "r2".into(),
                [2; 32]
            ),
            StartResult::Running(1, _)
        ));
    }
}
