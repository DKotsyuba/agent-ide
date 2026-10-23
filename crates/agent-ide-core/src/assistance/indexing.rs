//! Supervised background indexing of task-context indexes, one owned thread per daemon.
//!
//! The supervisor keeps one entry per active writer worktree root and runs at most one indexing
//! job at a time on its own thread, outside the sequential worker. A job runs only when its root
//! is dirty (activation, a committed edit or a bounded periodic reconcile), its debounce has
//! elapsed and it has not failed: a failure is never retried blindly, only a new activation
//! clears it. A spent activation budget leaves later runs model-free (local facts only). The
//! root directory's device and inode are captured at activation; a root later replaced (a
//! symlink or another directory at that path) is refused before any job reads it. Deactivation and shutdown cancel the running job and the
//! owner waits for it; the thread is joined on drop, so no indexing outlives its owner.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Trusted launcher settings of background indexing; disabled unless the operator enables it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IndexingConfig {
    /// Whether the daemon indexes in the background at all.
    #[serde(default)]
    pub enabled: bool,
    /// Physical model requests one activation may spend across all its indexing runs.
    #[serde(default)]
    pub max_new_calls: u64,
    /// Input bytes one activation may send to the embedding model across all its runs.
    #[serde(default)]
    pub max_input_bytes: u64,
    /// Whether a run may embed an index that has no ready model documents yet (a first full
    /// indexing). Off by default: the operator decides after a cost preflight.
    #[serde(default)]
    pub initial_allowed: bool,
    /// Quiet time after the last trigger before a run starts, in milliseconds.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Interval of the bounded periodic reconcile that catches edits made outside the IDE.
    #[serde(default = "default_reconcile_ms")]
    pub reconcile_ms: u64,
}

/// Default quiet time after a trigger.
fn default_debounce_ms() -> u64 {
    750
}

/// Default reconcile interval.
fn default_reconcile_ms() -> u64 {
    30_000
}

impl Default for IndexingConfig {
    /// Disabled, no budget, default timing.
    fn default() -> Self {
        Self {
            enabled: false,
            max_new_calls: 0,
            max_input_bytes: 0,
            initial_allowed: false,
            debounce_ms: default_debounce_ms(),
            reconcile_ms: default_reconcile_ms(),
        }
    }
}

/// How one indexing run ended, as the job reports it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RunOutcome {
    /// The index is published and current for the sources the run saw.
    Ready,
    /// The run stopped because its call budget ran out; the published index may lag.
    BudgetExhausted,
    /// The run was cancelled by deactivation or shutdown.
    Cancelled,
    /// The run failed; the closed reason is shown to the agent and never retried blindly.
    Failed(String),
    /// A first full model indexing awaits the operator's cost preflight; local facts are
    /// prepared, the semantic index is not ready.
    NeedsPreflight,
}

/// What one activation may still spend; a run never exceeds it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Budget {
    /// Physical model requests left.
    pub calls: u64,
    /// Embedding input bytes left.
    pub input_bytes: u64,
}

/// What one run reports back: its outcome and what it actually spent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunReport {
    pub outcome: RunOutcome,
    pub new_calls: u64,
    pub input_bytes: u64,
    /// Short operator-facing summary (generation, pending fragments, estimate), no secrets.
    pub summary: String,
}

/// Indexing state of one root, as `ide.prepare` shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum IndexState {
    /// Waiting for its debounce or its first run.
    Pending,
    /// A run is in progress.
    Building,
    /// The last run ended; carries its report.
    Done(RunReport),
}

/// One indexing job: `(root, remaining budget, cancel) -> report`. Runs on the supervisor thread;
/// a zero budget still lets it do local, model-free work (structure and lexical index).
pub(crate) type Job = dyn Fn(&Path, Budget, &CancellationToken) -> RunReport + Send + Sync;

/// Per-root bookkeeping.
struct Entry {
    remaining: Budget,
    dirty_since: Option<Instant>,
    last_reconcile: Instant,
    failed: bool,
    state: IndexState,
    /// The root directory's identity at activation; a job runs only while it still holds.
    identity: Option<RootIdentity>,
}

/// Device and inode of a canonical, real (non-symlink) root directory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootIdentity {
    device: u64,
    inode: u64,
}

/// The identity of `root` when it is a real directory reached by its own canonical path;
/// `None` for a symlink, a non-directory, a missing path or a path that resolves elsewhere.
fn root_identity(root: &Path) -> Option<RootIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(root).ok()?;
    if !metadata.is_dir() || std::fs::canonicalize(root).ok()? != root {
        return None;
    }
    Some(RootIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// The finished state of a root whose directory is gone or replaced: no job ran.
fn refused(reason: &str) -> IndexState {
    IndexState::Done(RunReport {
        outcome: RunOutcome::Failed(reason.to_owned()),
        new_calls: 0,
        input_bytes: 0,
        summary: format!(
            "indexing refused ({reason}): the worktree directory is not the one activated; \
             nothing was read; not retried until the next ide.start"
        ),
    })
}

/// State shared by the supervisor handle and its thread.
struct Shared {
    roots: BTreeMap<PathBuf, Entry>,
    running: Option<(PathBuf, CancellationToken)>,
    shutdown: bool,
}

/// Owner handle of the indexing thread; dropping it cancels and joins.
pub(crate) struct Supervisor {
    config: IndexingConfig,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Starts the supervisor thread when `config.enabled`; a disabled supervisor owns no thread
    /// and every call is a no-op.
    pub(crate) fn start(config: IndexingConfig, job: Arc<Job>) -> Self {
        let shared = Arc::new((
            Mutex::new(Shared {
                roots: BTreeMap::new(),
                running: None,
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let thread = config.enabled.then(|| {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("agent-ide-indexing".into())
                .spawn(move || run(config, &shared, &*job))
                .expect("spawn the indexing supervisor thread")
        });
        Self {
            config,
            shared,
            thread,
        }
    }

    /// A writer activation of `root`: a fresh budget, failure cleared, a run scheduled.
    pub(crate) fn activate(&self, root: &Path) {
        if !self.config.enabled {
            return;
        }
        let (lock, wake) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = Instant::now();
        let identity = root_identity(root);
        shared.roots.insert(
            root.to_owned(),
            Entry {
                failed: identity.is_none(),
                state: if identity.is_some() {
                    IndexState::Pending
                } else {
                    refused("root_unavailable")
                },
                identity,
                remaining: Budget {
                    calls: self.config.max_new_calls,
                    input_bytes: self.config.max_input_bytes,
                },
                dirty_since: Some(now),
                last_reconcile: now,
            },
        );
        wake.notify_all();
    }

    /// A committed source change under `root`: schedules a debounced run; unknown roots ignore it.
    pub(crate) fn dirty(&self, root: &Path) {
        let (lock, wake) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = shared.roots.get_mut(root) {
            entry.dirty_since = Some(Instant::now());
            wake.notify_all();
        }
    }

    /// A supervisor that owns no thread and ignores every call.
    pub(crate) fn disabled() -> Self {
        Self::start(
            IndexingConfig::default(),
            Arc::new(|_: &Path, _: Budget, _: &CancellationToken| RunReport {
                outcome: RunOutcome::Cancelled,
                new_calls: 0,
                input_bytes: 0,
                summary: String::new(),
            }),
        )
    }

    /// Ends `root`'s activation: forgets it and cancels its running job without blocking; the
    /// caller waits for [`Self::running`] to turn false before releasing the root's authority.
    pub(crate) fn deactivate(&self, root: &Path) {
        let (lock, wake) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.roots.remove(root);
        if let Some((running, cancel)) = &shared.running
            && running == root
        {
            cancel.cancel();
        }
        wake.notify_all();
    }

    /// Cancels every running job without blocking (daemon shutdown); [`Drop`] joins the thread.
    pub(crate) fn cancel_all(&self) {
        let (lock, wake) = &*self.shared;
        let mut shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.roots.clear();
        if let Some((_, cancel)) = &shared.running {
            cancel.cancel();
        }
        wake.notify_all();
    }

    /// Whether a job for `root` (any root when `None`) is still running.
    pub(crate) fn running(&self, root: Option<&Path>) -> bool {
        let (lock, _) = &*self.shared;
        let shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared
            .running
            .as_ref()
            .is_some_and(|(running, _)| root.is_none_or(|root| running == root))
    }

    /// The current indexing state of `root`, `None` when it is not tracked.
    pub(crate) fn state(&self, root: &Path) -> Option<IndexState> {
        let (lock, _) = &*self.shared;
        let shared = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.roots.get(root).map(|entry| entry.state.clone())
    }
}

impl Drop for Supervisor {
    /// Cancels any running job and joins the thread.
    fn drop(&mut self) {
        {
            let (lock, wake) = &*self.shared;
            let mut shared = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            shared.shutdown = true;
            if let Some((_, cancel)) = &shared.running {
                cancel.cancel();
            }
            wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The supervisor loop: picks the next due root, runs its job unlocked, records the report.
fn run(config: IndexingConfig, shared: &(Mutex<Shared>, Condvar), job: &Job) {
    let debounce = Duration::from_millis(config.debounce_ms);
    let reconcile = Duration::from_millis(config.reconcile_ms.max(1));
    let (lock, wake) = shared;
    let mut guard = lock
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if guard.shutdown {
            return;
        }
        let now = Instant::now();
        let mut next_wake = now + reconcile;
        let mut due = None;
        for (root, entry) in &mut guard.roots {
            if entry.failed {
                continue;
            }
            if entry.dirty_since.is_none() && now.duration_since(entry.last_reconcile) >= reconcile
            {
                entry.dirty_since = Some(now.checked_sub(debounce).unwrap_or(now));
            }
            match entry.dirty_since {
                Some(since) if now.duration_since(since) >= debounce => {
                    due.get_or_insert_with(|| root.clone());
                }
                Some(since) => next_wake = next_wake.min(since + debounce),
                None => next_wake = next_wake.min(entry.last_reconcile + reconcile),
            }
        }
        let Some(root) = due else {
            let wait = next_wake.saturating_duration_since(Instant::now());
            guard = wake
                .wait_timeout(guard, wait.max(Duration::from_millis(1)))
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
            continue;
        };
        let cancel = CancellationToken::new();
        let remaining = {
            let entry = guard.roots.get_mut(&root).expect("due root is tracked");
            // The directory must still be the one activated: a replaced or symlinked root is
            // refused before the job can read anything under it.
            if entry.identity.is_none() || root_identity(&root) != entry.identity {
                entry.failed = true;
                entry.dirty_since = None;
                entry.state = refused("root_replaced");
                continue;
            }
            entry.dirty_since = None;
            entry.last_reconcile = now;
            entry.state = IndexState::Building;
            entry.remaining
        };
        guard.running = Some((root.clone(), cancel.clone()));
        drop(guard);
        // A panicking job must not leave `running` set: deactivation and shutdown wait for it.
        // Its spend is unknown, so the whole remaining activation budget counts as used; the
        // panic payload is never copied into the report.
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            job(&root, remaining, &cancel)
        }))
        .unwrap_or_else(|_| RunReport {
            outcome: RunOutcome::Failed("worker_panic".into()),
            new_calls: remaining.calls,
            input_bytes: remaining.input_bytes,
            summary: "indexing job failed internally; spend unknown, the remaining activation \
                      budget counts as used; not retried until the next ide.start"
                .into(),
        });
        guard = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.running = None;
        if let Some(entry) = guard.roots.get_mut(&root) {
            entry.remaining.calls = entry.remaining.calls.saturating_sub(report.new_calls);
            entry.remaining.input_bytes = entry
                .remaining
                .input_bytes
                .saturating_sub(report.input_bytes);
            entry.failed = matches!(report.outcome, RunOutcome::Failed(_));
            // One pass may stop at its own per-pass cap while the activation still has budget:
            // continue soon instead of waiting for the reconcile, as long as the pass made progress.
            if report.outcome == RunOutcome::BudgetExhausted
                && report.new_calls > 0
                && entry.remaining.calls > 0
                && entry.remaining.input_bytes > 0
            {
                entry.dirty_since = Some(Instant::now());
            }
            entry.state = IndexState::Done(report);
        }
        wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Enabled, small budget, fast timing for tests.
    fn config(max_new_calls: u64) -> IndexingConfig {
        IndexingConfig {
            enabled: true,
            max_new_calls,
            max_input_bytes: 1_000,
            initial_allowed: false,
            debounce_ms: 20,
            reconcile_ms: 600_000,
        }
    }

    /// Waits until `root` reaches a finished state, panicking after two seconds.
    fn finished(supervisor: &Supervisor, root: &Path) -> RunReport {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(IndexState::Done(report)) = supervisor.state(root) {
                return report;
            }
            assert!(Instant::now() < deadline, "indexing did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Disabled config: no thread, no runs, no state.
    #[test]
    fn disabled_supervisor_never_runs() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&runs);
        let supervisor = Supervisor::start(
            IndexingConfig::default(),
            Arc::new(move |_: &Path, _, _: &CancellationToken| {
                counter.fetch_add(1, Ordering::SeqCst);
                RunReport {
                    outcome: RunOutcome::Ready,
                    new_calls: 0,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        supervisor.activate(Path::new("/r"));
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert_eq!(supervisor.state(Path::new("/r")), None);
    }

    /// The activation budget shrinks by each run's actual calls and is handed to the next run;
    /// several triggers inside the debounce merge into one run.
    #[test]
    fn budget_is_per_activation_and_triggers_merge() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let supervisor = Supervisor::start(
            config(10),
            Arc::new(move |_: &Path, remaining, _: &CancellationToken| {
                log.lock().unwrap().push(remaining.calls);
                RunReport {
                    outcome: RunOutcome::Ready,
                    new_calls: 4,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        let scratch = crate::scratch::ScratchDir::new("index-root");
        let canonical = std::fs::canonicalize(&*scratch).unwrap();
        let root = canonical.as_path();
        supervisor.activate(root);
        supervisor.dirty(root);
        supervisor.dirty(root);
        finished(&supervisor, root);
        supervisor.dirty(root);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            *seen.lock().unwrap(),
            [10, 6],
            "one merged run, then the rest"
        );
    }

    /// A pass that stops at its per-pass cap while the activation still has budget continues
    /// with the rest at once, and stops once the activation budget is spent.
    #[test]
    fn capped_pass_continues_while_activation_budget_remains() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let supervisor = Supervisor::start(
            config(10),
            Arc::new(move |_: &Path, remaining: Budget, _: &CancellationToken| {
                log.lock().unwrap().push(remaining.calls);
                let spent = remaining.calls.min(4);
                RunReport {
                    outcome: if spent > 0 {
                        RunOutcome::BudgetExhausted
                    } else {
                        RunOutcome::Ready
                    },
                    new_calls: spent,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        let scratch = crate::scratch::ScratchDir::new("index-root");
        supervisor.activate(&std::fs::canonicalize(&*scratch).unwrap());
        std::thread::sleep(Duration::from_millis(300));
        // Spent to zero: no further immediate pass (later triggers run model-free).
        assert_eq!(*seen.lock().unwrap(), [10, 6, 2]);
    }

    /// A root replaced after activation (by a symlink to another directory, or by another
    /// directory at the same path) is refused before the job runs; nothing is read there.
    #[test]
    fn replaced_root_is_refused_before_any_job() {
        for symlink in [true, false] {
            let runs = Arc::new(AtomicU64::new(0));
            let counter = Arc::clone(&runs);
            let supervisor = Supervisor::start(
                config(5),
                Arc::new(move |_: &Path, _, _: &CancellationToken| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    RunReport {
                        outcome: RunOutcome::Ready,
                        new_calls: 0,
                        input_bytes: 0,
                        summary: String::new(),
                    }
                }),
            );
            let scratch = crate::scratch::ScratchDir::new("index-replace");
            let base = std::fs::canonicalize(&*scratch).unwrap();
            let root = base.join("worktree");
            let elsewhere = base.join("elsewhere");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&elsewhere).unwrap();
            supervisor.activate(&root);
            finished(&supervisor, &root);
            assert_eq!(runs.load(Ordering::SeqCst), 1);
            std::fs::rename(&root, base.join("moved")).unwrap();
            if symlink {
                std::os::unix::fs::symlink(&elsewhere, &root).unwrap();
            } else {
                std::fs::create_dir(&root).unwrap();
            }
            supervisor.dirty(&root);
            std::thread::sleep(Duration::from_millis(100));
            assert_eq!(runs.load(Ordering::SeqCst), 1, "no job after replacement");
            assert!(
                matches!(
                    supervisor.state(&root),
                    Some(IndexState::Done(RunReport { outcome: RunOutcome::Failed(ref reason), .. }))
                        if reason == "root_replaced"
                ),
                "{:?}",
                supervisor.state(&root)
            );
        }
    }

    /// A root that is not a real canonical directory at activation never runs a job.
    #[test]
    fn unavailable_root_never_runs() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&runs);
        let supervisor = Supervisor::start(
            config(5),
            Arc::new(move |_: &Path, _, _: &CancellationToken| {
                counter.fetch_add(1, Ordering::SeqCst);
                RunReport {
                    outcome: RunOutcome::Ready,
                    new_calls: 0,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        let root = Path::new("/nonexistent/agent-ide-root");
        supervisor.activate(root);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        assert!(matches!(
            supervisor.state(root),
            Some(IndexState::Done(RunReport {
                outcome: RunOutcome::Failed(_),
                ..
            }))
        ));
    }

    /// A panicking job ends as a failed run: `running` clears, the remaining budget counts as
    /// spent, deactivation and drop do not hang, and nothing retries it.
    #[test]
    fn panicking_job_is_contained() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&runs);
        let supervisor = Supervisor::start(
            config(5),
            Arc::new(
                move |_: &Path, _: Budget, _: &CancellationToken| -> RunReport {
                    counter.fetch_add(1, Ordering::SeqCst);
                    panic!("deliberate indexing job panic");
                },
            ),
        );
        let scratch = crate::scratch::ScratchDir::new("index-panic");
        let canonical = std::fs::canonicalize(&*scratch).unwrap();
        let root = canonical.as_path();
        supervisor.activate(root);
        let report = finished(&supervisor, root);
        assert_eq!(report.outcome, RunOutcome::Failed("worker_panic".into()));
        assert_eq!((report.new_calls, report.input_bytes), (5, 1_000));
        assert!(!report.summary.contains("deliberate"), "{}", report.summary);
        assert!(!supervisor.running(None));
        supervisor.dirty(root);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no retry after a panic");
        supervisor.deactivate(root);
        assert!(!supervisor.running(Some(root)));
        drop(supervisor);
    }

    /// A failure is not retried by later edits; a new activation clears it with a fresh budget.
    #[test]
    fn failure_is_not_retried_until_reactivation() {
        let runs = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&runs);
        let supervisor = Supervisor::start(
            config(5),
            Arc::new(move |_: &Path, _, _: &CancellationToken| {
                counter.fetch_add(1, Ordering::SeqCst);
                RunReport {
                    outcome: RunOutcome::Failed("model_error".into()),
                    new_calls: 1,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        let scratch = crate::scratch::ScratchDir::new("index-root");
        let canonical = std::fs::canonicalize(&*scratch).unwrap();
        let root = canonical.as_path();
        supervisor.activate(root);
        finished(&supervisor, root);
        supervisor.dirty(root);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 1, "no blind retry");
        supervisor.activate(root);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// An exhausted budget leaves later runs model-free (zero budget) instead of stopping the
    /// local, lexical preparation.
    #[test]
    fn exhausted_budget_leaves_model_free_runs() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        let supervisor = Supervisor::start(
            config(3),
            Arc::new(move |_: &Path, remaining: Budget, _: &CancellationToken| {
                log.lock().unwrap().push(remaining);
                RunReport {
                    outcome: RunOutcome::BudgetExhausted,
                    new_calls: remaining.calls,
                    input_bytes: remaining.input_bytes,
                    summary: String::new(),
                }
            }),
        );
        let scratch = crate::scratch::ScratchDir::new("index-root");
        let canonical = std::fs::canonicalize(&*scratch).unwrap();
        let root = canonical.as_path();
        supervisor.activate(root);
        finished(&supervisor, root);
        supervisor.dirty(root);
        std::thread::sleep(Duration::from_millis(80));
        let zero = Budget {
            calls: 0,
            input_bytes: 0,
        };
        assert_eq!(
            *seen.lock().unwrap(),
            [
                Budget {
                    calls: 3,
                    input_bytes: 1_000
                },
                zero
            ]
        );
    }

    /// Deactivation cancels the running job without blocking; it returns soon after and drop joins.
    #[test]
    fn deactivation_cancels_and_waits_for_the_job() {
        let ended = Arc::new(AtomicU64::new(0));
        let flag = Arc::clone(&ended);
        let supervisor = Supervisor::start(
            config(5),
            Arc::new(move |_: &Path, _, cancel: &CancellationToken| {
                while !cancel.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                flag.store(1, Ordering::SeqCst);
                RunReport {
                    outcome: RunOutcome::Cancelled,
                    new_calls: 0,
                    input_bytes: 0,
                    summary: String::new(),
                }
            }),
        );
        let scratch = crate::scratch::ScratchDir::new("index-root");
        let canonical = std::fs::canonicalize(&*scratch).unwrap();
        let root = canonical.as_path();
        supervisor.activate(root);
        let deadline = Instant::now() + Duration::from_secs(2);
        while supervisor.state(root) != Some(IndexState::Building) {
            assert!(Instant::now() < deadline, "run did not start");
            std::thread::sleep(Duration::from_millis(2));
        }
        supervisor.deactivate(root);
        let deadline = Instant::now() + Duration::from_secs(2);
        while supervisor.running(Some(root)) {
            assert!(Instant::now() < deadline, "cancelled job did not return");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            ended.load(Ordering::SeqCst),
            1,
            "job returned before running() cleared"
        );
        assert_eq!(supervisor.state(root), None);
        drop(supervisor);
    }
}
