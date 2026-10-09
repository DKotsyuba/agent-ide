//! Contract tests for [`agent_ide::checks::scheduler::Scheduler`] (EYES-r1 §5, EYES-r2 §5).
//!
//! `FakeChecker` (in `agent_ide::checks`) does not record concurrency or reflect the dispatched
//! `input_generation`, so these tests use `RecordingChecker` below instead.

use agent_ide::checks::scheduler::{CacheClone, FingerprintFn, Scheduler};
use agent_ide::checks::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason,
};
use agent_ide::execution::seatbelt::ReadDeny;
use agent_ide::retention::{Fate, Reason, Report, sweep_with};
use support::Scratch;
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "support/scratch.rs"]
mod support;

/// Scripted [`Checker`] that records every request, tracks live concurrency through an RAII
/// guard held across its (optional) delay, and echoes the request's `input_generation` into the
/// returned snapshot so tests can tell which trigger produced which result. Each call consumes
/// one entry from an optional scripted `CheckState` queue (defaulting to `Ready` once exhausted),
/// so a test can script a sequence of outcomes for one `(worktree, language)` pair.
#[derive(Clone)]
struct RecordingChecker {
    language: Language,
    delay: Option<Duration>,
    calls: Arc<Mutex<Vec<CheckRequest>>>,
    concurrent: Arc<AtomicUsize>,
    max_concurrent_seen: Arc<AtomicUsize>,
    states: Arc<Mutex<VecDeque<CheckState>>>,
}

/// RAII guard incrementing a shared concurrency counter (and its running maximum) on creation
/// and decrementing it on drop, including when the owning future is cancelled mid-await.
struct ConcurrencyGuard {
    concurrent: Arc<AtomicUsize>,
}

impl ConcurrencyGuard {
    /// Increments `concurrent`, updates `max_seen` if the new value is a new high, and returns
    /// the guard that will decrement `concurrent` again on drop.
    fn new(concurrent: Arc<AtomicUsize>, max_seen: Arc<AtomicUsize>) -> Self {
        let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        max_seen.fetch_max(now, Ordering::SeqCst);
        Self { concurrent }
    }
}

impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.concurrent.fetch_sub(1, Ordering::SeqCst);
    }
}

impl RecordingChecker {
    /// Builds a checker for `language` that resolves immediately with `Ready` snapshots and a
    /// fresh concurrency tracker.
    fn new(language: Language) -> Self {
        Self::with_shared(
            language,
            None,
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    /// Builds a checker for `language` that awaits `delay` before resolving with `Ready`.
    fn with_delay(language: Language, delay: Duration) -> Self {
        Self::with_shared(
            language,
            Some(delay),
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )
    }

    /// Builds a checker for `language` that resolves immediately, returning each of `states` in
    /// order (one per call), then `Ready` once the script is exhausted.
    fn with_states(language: Language, states: Vec<CheckState>) -> Self {
        let checker = Self::new(language);
        *checker.states.lock().unwrap() = states.into_iter().collect();
        checker
    }

    /// Builds a checker for `language` sharing `concurrent`/`max_concurrent_seen` with another
    /// checker instance, so scheduler-wide (cross-language) concurrency can be observed.
    fn with_shared(
        language: Language,
        delay: Option<Duration>,
        concurrent: Arc<AtomicUsize>,
        max_concurrent_seen: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            language,
            delay,
            calls: Arc::new(Mutex::new(Vec::new())),
            concurrent,
            max_concurrent_seen,
            states: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Returns the requests this checker has been asked to run, in call order.
    fn calls(&self) -> Vec<CheckRequest> {
        self.calls.lock().unwrap().clone()
    }

    /// Returns the highest number of concurrently live `check` futures observed so far.
    fn max_concurrent(&self) -> usize {
        self.max_concurrent_seen.load(Ordering::SeqCst)
    }

    /// Returns the number of currently live `check` futures.
    fn live(&self) -> usize {
        self.concurrent.load(Ordering::SeqCst)
    }
}

impl Checker for RecordingChecker {
    fn language(&self) -> Language {
        self.language
    }

    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        self.calls.lock().unwrap().push(request.clone());
        let concurrent = Arc::clone(&self.concurrent);
        let max_seen = Arc::clone(&self.max_concurrent_seen);
        let delay = self.delay;
        let language = self.language;
        let generation = request.input_generation;
        let state = self
            .states
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(CheckState::Ready);
        Box::pin(async move {
            let _guard = ConcurrencyGuard::new(concurrent, max_seen);
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            match state {
                CheckState::Unavailable(reason) => {
                    ProblemSnapshot::unavailable(language, reason, generation)
                }
                other => ProblemSnapshot::from_problems(language, other, Vec::new(), generation, 0),
            }
        })
    }
}

/// Creates a fresh empty scratch directory under the system temporary directory, suitable for
/// use both as a scheduler cache root and as a fake worktree path (it only needs to exist so
/// `std::fs::canonicalize` succeeds).
fn scratch_dir(name: &str) -> Scratch {
    // Below one private per-call root: a cache root's parent holds the retention leases and must
    // not be group or world writable, whatever the system temporary directory is. The root is
    // the unit the guard removes, so the lease state beside the cache root goes with it.
    /// Makes each call's root unique within this process.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir().join(format!(
        "agent-ide-scheduler-contract-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    let dir = base.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    Scratch::own_tree(base, dir)
}

/// Builds a scratch worktree that is present for `language` (T10B), so tests exercising the
/// scheduler through `RecordingChecker` are not short-circuited by the presence gate meant for
/// real per-language checkers.
fn scratch_worktree(name: &str, language: Language) -> Scratch {
    let dir = scratch_dir(name);
    if language == agent_ide::languages::RUST {
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    } else if language == agent_ide::languages::PYTHON {
        std::fs::write(dir.join("pyproject.toml"), "").unwrap();
    } else if language == agent_ide::languages::TYPESCRIPT {
        std::fs::write(dir.join("tsconfig.json"), "{}").unwrap();
    }
    dir
}

/// Builds a scratch worktree present for both languages (T10B).
fn scratch_worktree_both_languages(name: &str) -> Scratch {
    let dir = scratch_dir(name);
    std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
    std::fs::write(dir.join("pyproject.toml"), "").unwrap();
    dir
}

/// Builds a fingerprint seam reporting `value` for every worktree (T20B): `None` means unknown,
/// which the scheduler treats as changed and runs.
fn constant_fingerprint(value: Option<u64>) -> FingerprintFn {
    Arc::new(move |_| value)
}

/// Builds a fingerprint seam reading its value from `cell` on every call, so a test can flip the
/// worktree between unchanged and changed inputs.
fn shared_fingerprint(cell: Arc<AtomicU64>) -> FingerprintFn {
    Arc::new(move |_| Some(cell.load(Ordering::SeqCst)))
}

/// Advances the paused tokio clock by `step` and yields several times so tasks woken by expired
/// timers actually get polled before the next assertion.
///
/// Yields *before* the jump too: a task spawned just before this call (a fresh debounce or
/// cooldown timer) needs its first poll - which registers its `sleep()` deadline - at the
/// current virtual time, before the clock jumps forward, or its deadline would be computed from
/// the post-jump time instead.
async fn advance(step: Duration) {
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    tokio::time::advance(step).await;
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
}

/// Advances the paused tokio clock by `total`, in `step`-sized increments.
///
/// EYES-r2 chains multiple sequential timers per run (debounce, then the check's own delay, then
/// the cooldown sleep before the next run); each is only registered once its owning task is
/// first polled, which [`advance`] only guarantees for timers pending *before* that call starts.
/// Taking many small steps - rather than one large [`advance`] - lets each newly-registered
/// timer in the chain get crossed by a later step within the same `settle` call.
async fn settle(total: Duration, step: Duration) {
    let mut remaining = total;
    while !remaining.is_zero() {
        let this_step = remaining.min(step);
        advance(this_step).await;
        remaining -= this_step;
    }
}

/// Advances the paused clock by `step` at a time, yielding the real thread between steps so the
/// blocking pool can finish, until `ready` holds (at most 5,000 steps, then the caller's own
/// assertion reports the failure). Use it instead of a fixed [`advance`] whenever the awaited
/// state is produced by `spawn_blocking`, whose completion depends on real scheduling.
async fn settle_until(ready: impl Fn() -> bool, step: Duration) {
    for _ in 0..5_000 {
        if ready() {
            return;
        }
        advance(step).await;
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Ten triggers within one debounce window collapse into exactly one check, whose cache dir is 0700.
#[tokio::test(start_paused = true)]
async fn scheduler_burst_of_triggers_debounces_to_one_check() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("burst-cache");
    let worktree = scratch_worktree("burst-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root.path(),
    );

    for _ in 0..10 {
        scheduler.trigger("repo", &worktree);
        advance(Duration::from_millis(5)).await;
    }
    settle(Duration::from_millis(300), Duration::from_millis(5)).await;

    assert_eq!(checker.calls().len(), 1);
    let latest = scheduler.latest(&worktree);
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].language, agent_ide::languages::PYTHON);
    assert!(!scheduler.is_busy());

    let cache_dir = &checker.calls()[0].cache_dir;
    let mode = std::fs::metadata(cache_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// A trigger that lands while a check is running marks it dirty for exactly one latest-wins rerun.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_during_running_check_causes_one_extra_run_with_newest_generation() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_millis(200));
    let cache_root = scratch_dir("dirty-cache");
    let worktree = scratch_worktree("dirty-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1, "first check should have started");

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "second trigger's timer fired while the first check was still running: it must mark dirty, not start a second run yet"
    );

    // Crosses the remainder of the first check's own 200ms delay, then (EYES-r2) the cooldown
    // of max(debounce, previous run duration) before the dirty rerun is allowed to dispatch.
    settle(Duration::from_millis(500), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 2, "dirty rerun should have started");

    settle(Duration::from_millis(300), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "latest-wins allows exactly one extra run"
    );
    assert!(!scheduler.is_busy());

    let latest = scheduler.latest(&worktree);
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].input_generation, 2);
}

/// (T28B) A trigger that lands while a check runs marks the pair dirty, but the follow-up run is
/// subject to the same T20B skip-unchanged rule: with inputs unchanged from the completed `Ready`
/// run, the follow-up spawns no checker process at all and leaves the stored snapshot current.
#[tokio::test(start_paused = true)]
async fn scheduler_dirty_rerun_with_unchanged_fingerprint_is_skipped() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_millis(200));
    let cache_root = scratch_dir("dirty-skip-cache");
    let worktree = scratch_worktree("dirty-skip-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root.path(),
    )
    .with_fingerprint(constant_fingerprint(Some(7)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1, "first check runs");

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "the trigger during the run must mark dirty, not start a second run"
    );

    settle(Duration::from_millis(700), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "the dirty follow-up with unchanged inputs must be skipped entirely"
    );
    assert!(
        scheduler.running(&worktree).is_empty(),
        "a skipped follow-up must not leave the running flag set"
    );
    assert!(!scheduler.is_busy());
    assert_eq!(scheduler.latest(&worktree)[0].state, CheckState::Ready);
}

/// (T28B) A dirty follow-up run whose inputs changed while the first run was in flight still
/// runs: latest wins.
#[tokio::test(start_paused = true)]
async fn scheduler_dirty_rerun_with_changed_fingerprint_runs() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_millis(200));
    let cell = Arc::new(AtomicU64::new(7));
    let cache_root = scratch_dir("dirty-changed-cache");
    let worktree = scratch_worktree("dirty-changed-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root.path(),
    )
    .with_fingerprint(shared_fingerprint(Arc::clone(&cell)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1);

    // An edit lands while the first check is in flight.
    cell.store(8, Ordering::SeqCst);
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(80), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "the trigger during the run must mark dirty, not start a second run yet"
    );

    settle(Duration::from_millis(700), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "the dirty follow-up with changed inputs must run"
    );
}

/// Five worktrees times two languages never exceed the configured global concurrency cap.
#[tokio::test(start_paused = true)]
async fn scheduler_never_runs_more_than_max_concurrent_checks_across_worktrees_and_languages() {
    agent_ide::languages::install();
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let rust_checker = RecordingChecker::with_shared(
        agent_ide::languages::RUST,
        Some(Duration::from_millis(100)),
        Arc::clone(&concurrent),
        Arc::clone(&max_seen),
    );
    let python_checker = RecordingChecker::with_shared(
        agent_ide::languages::PYTHON,
        Some(Duration::from_millis(100)),
        Arc::clone(&concurrent),
        Arc::clone(&max_seen),
    );
    let cache_root = scratch_dir("fanout-cache");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    let worktrees: Vec<Scratch> = (0..5)
        .map(|i| scratch_worktree_both_languages(&format!("fanout-wt-{i}")))
        .collect();
    for worktree in &worktrees {
        scheduler.trigger("repo", worktree);
    }

    // Fixed total settle time, no early break: an early break on `calls().len()` would race the
    // last wave's completion, since `calls()` records dispatch, not completion. Each worktree
    // only runs once here, so EYES-r2's cooldown never gates a second run within this test.
    settle(Duration::from_millis(1200), Duration::from_millis(10)).await;

    assert_eq!(rust_checker.calls().len(), 5);
    assert_eq!(python_checker.calls().len(), 5);
    assert!(
        rust_checker
            .max_concurrent()
            .max(python_checker.max_concurrent())
            <= 2,
        "observed more than 2 concurrent checks"
    );
    assert_eq!(concurrent.load(Ordering::SeqCst), 0);
    assert!(!scheduler.is_busy());
}

/// `latest()` keeps returning the previous snapshot, non-blocking, until a newer run completes.
#[tokio::test(start_paused = true)]
async fn scheduler_latest_reflects_previous_snapshot_while_a_newer_check_runs() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_millis(200));
    let cache_root = scratch_dir("latest-cache");
    let worktree = scratch_worktree("latest-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    // Settle just past the expected completion (debounce 10ms + 200ms delay), with only a
    // small margin: EYES-r2's cooldown is measured from completion, so overshooting here would
    // silently consume part of the cooldown budget the next check on this test relies on.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(250), Duration::from_millis(5)).await;
    let first = scheduler.latest(&worktree);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].input_generation, 1);

    // EYES-r2 cooldown after run 1 (max(debounce, ~200ms delay)) plus this trigger's own
    // debounce must both elapse before run 2 dispatches; settle only partway so it has not yet.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "run 2 must wait out the post-completion cooldown before dispatching"
    );
    let while_waiting = scheduler.latest(&worktree);
    assert_eq!(
        while_waiting[0].input_generation, 1,
        "latest() must keep returning the previous snapshot while a newer run is pending or running"
    );

    settle(Duration::from_millis(600), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "run 2 should have dispatched by now"
    );
    let after = scheduler.latest(&worktree);
    assert_eq!(after[0].input_generation, 2);
}

/// `shutdown()` drops a long-running check's future promptly and ignores triggers received after.
#[tokio::test(start_paused = true)]
async fn scheduler_shutdown_cancels_a_long_running_check_promptly() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_secs(3600));
    let cache_root = scratch_dir("shutdown-cache");
    let worktree = scratch_worktree("shutdown-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    advance(Duration::from_millis(20)).await;
    assert_eq!(checker.live(), 1, "check should be running before shutdown");

    let started = tokio::time::Instant::now();
    scheduler.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "shutdown must not wait for the check's own delay to elapse"
    );
    assert_eq!(
        checker.live(),
        0,
        "the check future must have been dropped by abort"
    );
    assert!(!scheduler.is_busy());

    scheduler.trigger("repo", &worktree);
    advance(Duration::from_millis(20)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "triggers received after shutdown starts must be ignored"
    );
}

/// A worktree's first Rust check clones `target/` copy-on-write from a completed sibling worktree.
#[tokio::test(start_paused = true)]
async fn scheduler_clones_rust_target_from_sibling_worktree_of_the_same_repository() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::RUST);
    let cache_root = scratch_dir("clone-cache");
    let worktree_a = scratch_worktree("clone-worktree-a", agent_ide::languages::RUST);
    let worktree_b = scratch_worktree("clone-worktree-b", agent_ide::languages::RUST);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("shared-repo", &worktree_a);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 1);
    assert_eq!(
        scheduler.cache_clone_outcome(&worktree_a, agent_ide::languages::RUST),
        CacheClone::SkippedNoSource,
        "the first worktree of a repository has no sibling cache to clone from"
    );

    let cache_dir_a = checker.calls()[0].cache_dir.clone();
    let target_a = cache_dir_a.join("target");
    std::fs::create_dir_all(&target_a).unwrap();
    std::fs::write(target_a.join("marker.txt"), b"built").unwrap();

    // The clone itself spawns a real `/bin/cp` child process, whose completion is a real OS I/O
    // event independent of the paused virtual clock; wait for it in real (short) time slices
    // rather than virtual `advance()`.
    scheduler.trigger("shared-repo", &worktree_b);
    advance(Duration::from_millis(15)).await;
    for _ in 0..200 {
        if checker.calls().len() == 2 {
            break;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(checker.calls().len(), 2);

    let outcome = scheduler.cache_clone_outcome(&worktree_b, agent_ide::languages::RUST);
    let cache_dir_b = checker.calls()[1].cache_dir.clone();
    match outcome {
        CacheClone::Cloned => {
            assert_eq!(
                std::fs::read(cache_dir_b.join("target/marker.txt")).unwrap(),
                b"built"
            );
        }
        CacheClone::Failed => {
            // Non-APFS filesystems reject `cp -c`; the scheduler must fall back to a cold
            // check rather than fail, which this arm confirms without asserting file content.
        }
        other => panic!("expected Cloned or a Failed fallback, got {other:?}"),
    }
}

/// Without the sibling source's retention lease the clone is skipped and the check builds cold.
#[tokio::test(start_paused = true)]
async fn scheduler_skips_the_sibling_clone_when_the_source_lease_is_unavailable() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::RUST);
    let cache_root = scratch_dir("clone-no-lease-cache");
    let worktree_a = scratch_worktree("clone-no-lease-a", agent_ide::languages::RUST);
    let worktree_b = scratch_worktree("clone-no-lease-b", agent_ide::languages::RUST);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );
    scheduler.trigger("no-lease-repo", &worktree_a);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 1);
    let target_a = checker.calls()[0].cache_dir.join("target");
    std::fs::create_dir_all(&target_a).unwrap();
    std::fs::write(target_a.join("marker.txt"), b"built").unwrap();

    // A directory where the source's lease file belongs cannot be opened as a lock.
    let key = agent_ide::retention::worktree_key(&std::fs::canonicalize(&worktree_a).unwrap());
    let lease = cache_root
        .parent()
        .unwrap()
        .join("locks")
        .join(format!("{key}.lock"));
    std::fs::remove_file(&lease).unwrap();
    std::fs::create_dir(&lease).unwrap();

    scheduler.trigger("no-lease-repo", &worktree_b);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 2, "the check still runs, cold");
    assert_eq!(
        scheduler.cache_clone_outcome(&worktree_b, agent_ide::languages::RUST),
        CacheClone::SkippedNoSource
    );
    assert!(
        !checker.calls()[1]
            .cache_dir
            .join("target/marker.txt")
            .exists()
    );
    std::fs::remove_dir(&lease).unwrap();
}

/// Persistent caches and sibling Rust clones are isolated by the effective deny set.
#[tokio::test(start_paused = true)]
async fn scheduler_partitions_caches_and_clones_by_policy() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::RUST);
    let cache_root = scratch_dir("policy-cache");
    let worktree_a = scratch_worktree("policy-worktree-a", agent_ide::languages::RUST);
    let worktree_b = scratch_worktree("policy-worktree-b", agent_ide::languages::RUST);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );
    scheduler.add_read_denies(
        &worktree_a,
        &[ReadDeny::Path(PathBuf::from("/private/tmp/policy-a"))],
    );
    scheduler.trigger("shared-repo", &worktree_a);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 1);
    let first_cache = checker.calls()[0].cache_dir.clone();
    std::fs::create_dir_all(first_cache.join("target")).unwrap();
    std::fs::write(first_cache.join("target/marker.txt"), "old policy").unwrap();

    scheduler.add_read_denies(
        &worktree_b,
        &[ReadDeny::Path(PathBuf::from("/private/tmp/policy-b"))],
    );
    scheduler.trigger("shared-repo", &worktree_b);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 2);
    let second_cache = checker.calls()[1].cache_dir.clone();
    assert_ne!(
        first_cache.parent().unwrap().file_name(),
        second_cache.parent().unwrap().file_name()
    );
    assert_eq!(
        scheduler.cache_clone_outcome(&worktree_b, agent_ide::languages::RUST),
        CacheClone::SkippedNoSource
    );
    assert!(!second_cache.join("target/marker.txt").exists());

    scheduler.add_read_denies(
        &worktree_a,
        &[ReadDeny::Path(PathBuf::from("/private/tmp/policy-b"))],
    );
    scheduler.trigger("shared-repo", &worktree_a);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 3);
    let stricter_cache = checker.calls()[2].cache_dir.clone();
    assert_ne!(first_cache, stricter_cache);
    assert!(!stricter_cache.join("target/marker.txt").exists());
    assert_eq!(
        scheduler.cache_clone_outcome(&worktree_a, agent_ide::languages::RUST),
        CacheClone::SkippedNoSource
    );
}

/// A transient Fatal completion keeps the prior Ready snapshot; a durable Unavailable reason replaces it.
#[tokio::test(start_paused = true)]
async fn scheduler_fatal_or_timeout_completion_never_replaces_a_ready_snapshot() {
    agent_ide::languages::install();
    let checker = RecordingChecker::with_states(
        agent_ide::languages::PYTHON,
        vec![
            CheckState::Ready,
            CheckState::Unavailable(UnavailableReason::Fatal),
            CheckState::Unavailable(UnavailableReason::ToolMissing),
        ],
    );
    let cache_root = scratch_dir("fatal-cache");
    let worktree = scratch_worktree("fatal-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1);
    assert_eq!(scheduler.latest(&worktree)[0].state, CheckState::Ready);

    // A `Fatal` completion must not overwrite the still-usable `Ready` result.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 2);
    assert_eq!(
        scheduler.latest(&worktree)[0].state,
        CheckState::Ready,
        "a transient Fatal completion must not replace the existing Ready snapshot"
    );

    // A non-transient `Unavailable` reason (here `ToolMissing`) still replaces it.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 3);
    assert_eq!(
        scheduler.latest(&worktree)[0].state,
        CheckState::Unavailable(UnavailableReason::ToolMissing),
        "a durable Unavailable reason must still replace the stored snapshot"
    );
}

/// Returns the fate the retention sweep gave `dir`, or `None` when the policy did not select it.
fn fate_of(report: &Report, dir: &Path) -> Option<(Reason, Fate)> {
    report
        .verdicts
        .iter()
        .find(|verdict| verdict.path == dir)
        .map(|verdict| (verdict.reason, verdict.fate))
}

/// The cache directory of one worktree below a scheduler's `<home>/checks` cache root.
fn worktree_cache_dir(request_cache_dir: &Path) -> PathBuf {
    // `<home>/checks/<repo>/<worktree>/<policy digest>/<language>`
    request_cache_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// A running check holds its worktree's retention lease, so a sweep that finds the cache idle
/// still keeps it; once the run completes the sweep reports it removed.
#[tokio::test(start_paused = true)]
async fn scheduler_check_lease_keeps_retention_off_a_running_check_until_it_completes() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_secs(5));
    let home_dir = scratch_dir("retention-home");
    let home = std::fs::canonicalize(&home_dir).unwrap();
    let worktree = scratch_worktree("retention-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        home.join("checks"),
    );
    let later = std::time::SystemTime::now() + Duration::from_secs(30 * 86_400);
    let nobody = || Some(Vec::new());

    scheduler.trigger("repo", &worktree);
    advance(Duration::from_millis(20)).await;
    assert_eq!(checker.live(), 1, "the check is running");
    let language_dir = checker.calls()[0].cache_dir.clone();
    let dir = worktree_cache_dir(&language_dir);
    std::fs::write(language_dir.join("artifact"), vec![1u8; 4096]).unwrap();

    let report = sweep_with(&home, true, later, &nobody);
    assert_eq!(fate_of(&report, &dir), Some((Reason::Idle, Fate::InUse)));
    assert!(dir.exists(), "a running check's cache is never claimed");

    settle(Duration::from_secs(6), Duration::from_millis(100)).await;
    assert_eq!(checker.live(), 0, "the check completed");
    let report = sweep_with(&home, true, later, &nobody);
    assert_eq!(fate_of(&report, &dir), Some((Reason::Idle, Fate::Removed)));
    assert!(!dir.exists());
    scheduler.shutdown().await;
}

/// A cancelled check may leave its confined child running, so its lease outlives the run.
#[tokio::test(start_paused = true)]
async fn scheduler_cancelled_check_keeps_its_lease_for_the_process_lifetime() {
    agent_ide::languages::install();
    let checker =
        RecordingChecker::with_delay(agent_ide::languages::PYTHON, Duration::from_secs(3600));
    let home_dir = scratch_dir("retention-cancel-home");
    let home = std::fs::canonicalize(&home_dir).unwrap();
    let worktree = scratch_worktree("retention-cancel-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        home.join("checks"),
    );
    scheduler.trigger("repo", &worktree);
    // A readiness barrier, not a fixed advance: the run reaches the checker through
    // `spawn_blocking`, whose completion paused virtual time cannot order, so a loaded machine
    // needs more than one 20 ms step before the check is live.
    settle_until(|| checker.live() == 1, Duration::from_millis(10)).await;
    assert_eq!(checker.live(), 1);
    let dir = worktree_cache_dir(&checker.calls()[0].cache_dir);

    scheduler.shutdown().await;
    assert_eq!(checker.live(), 0, "the check future was dropped");
    let later = std::time::SystemTime::now() + Duration::from_secs(30 * 86_400);
    let report = sweep_with(&home, true, later, &|| Some(Vec::new()));
    assert_eq!(fate_of(&report, &dir), Some((Reason::Idle, Fate::InUse)));
    assert!(dir.exists());
}

/// The next run waits max(debounce, previous run duration) after the previous completion, not just the debounce.
#[tokio::test(start_paused = true)]
async fn scheduler_enforces_a_cooldown_of_max_debounce_and_previous_duration() {
    agent_ide::languages::install();
    let debounce = Duration::from_millis(10);
    let run_duration = Duration::from_millis(150);
    let checker = RecordingChecker::with_delay(agent_ide::languages::PYTHON, run_duration);
    let cache_root = scratch_dir("cooldown-cache");
    let worktree = scratch_worktree("cooldown-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(vec![Arc::new(checker.clone())], debounce, 2, cache_root.path());

    // Settle just past the expected completion (debounce + 150ms delay), with only a small
    // margin, for the same reason as above: the cooldown budget this test exercises is measured
    // from that completion instant.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(200), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1, "first run should have completed");

    // Cooldown is max(debounce=10ms, previous run duration=150ms) = 150ms after completion.
    // 60ms clears the plain debounce but not the cooldown, so run 2 must still be pending.
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "60ms clears the debounce alone but not the max(debounce, previous duration) cooldown"
    );

    settle(Duration::from_millis(300), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "run 2 should dispatch once the full cooldown has elapsed"
    );
}

/// A worktree with only a `Cargo.toml`: Rust is checked, Python is never dispatched and reports
/// `Unavailable(Disabled)` without creating a cache directory (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_rust_only_worktree_checks_rust_and_reports_python_disabled() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let python_checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("presence-rust-only-cache");
    let worktree = scratch_worktree("presence-rust-only-worktree", agent_ide::languages::RUST);
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;

    assert_eq!(
        rust_checker.calls().len(),
        1,
        "rust is present and must run"
    );
    assert_eq!(
        python_checker.calls().len(),
        0,
        "python is absent and must never be dispatched"
    );
    let latest = scheduler.latest(&worktree);
    let python_snapshot = latest
        .iter()
        .find(|snapshot| snapshot.language == agent_ide::languages::PYTHON)
        .expect("python still reports a snapshot");
    assert_eq!(
        python_snapshot.state,
        CheckState::Unavailable(UnavailableReason::Disabled)
    );
    assert!(
        !any_entry_named(&cache_root, "python"),
        "no python cache directory may be created for an absent language"
    );
}

/// Reports whether any file or directory named `name` exists anywhere under `root`, walking
/// exactly two levels deep (the cache layout's repository and worktree segments).
fn any_entry_named(root: &Path, name: &str) -> bool {
    let Ok(repo_entries) = std::fs::read_dir(root) else {
        return false;
    };
    for repo_entry in repo_entries.flatten() {
        let Ok(worktree_entries) = std::fs::read_dir(repo_entry.path()) else {
            continue;
        };
        for worktree_entry in worktree_entries.flatten() {
            if worktree_entry.file_name() == name || worktree_entry.path().join(name).exists() {
                return true;
            }
        }
    }
    false
}

/// A worktree with only a Python marker file: Python is checked, Rust is never dispatched and
/// reports `Unavailable(Disabled)` (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_python_only_worktree_checks_python_and_reports_rust_disabled() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let python_checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("presence-python-only-cache");
    let worktree = scratch_worktree(
        "presence-python-only-worktree",
        agent_ide::languages::PYTHON,
    );
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;

    assert_eq!(
        python_checker.calls().len(),
        1,
        "python is present and must run"
    );
    assert_eq!(
        rust_checker.calls().len(),
        0,
        "rust is absent and must never be dispatched"
    );
    let latest = scheduler.latest(&worktree);
    let rust_snapshot = latest
        .iter()
        .find(|snapshot| snapshot.language == agent_ide::languages::RUST)
        .expect("rust still reports a snapshot");
    assert_eq!(
        rust_snapshot.state,
        CheckState::Unavailable(UnavailableReason::Disabled)
    );
}

/// A worktree with both a `Cargo.toml` and a Python marker file: both languages are checked
/// (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_worktree_with_both_manifests_checks_both_languages() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let python_checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("presence-both-cache");
    let worktree = scratch_worktree_both_languages("presence-both-worktree");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;

    assert_eq!(rust_checker.calls().len(), 1);
    assert_eq!(python_checker.calls().len(), 1);
    let latest = scheduler.latest(&worktree);
    assert!(
        latest
            .iter()
            .all(|snapshot| snapshot.state == CheckState::Ready)
    );
}

/// A configured TypeScript checker runs for a root config, while package-only roots stay absent.
#[tokio::test(start_paused = true)]
async fn scheduler_typescript_presence_requires_root_config() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let python_checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let typescript_checker = RecordingChecker::new(agent_ide::languages::TYPESCRIPT);
    let cache_root = scratch_dir("presence-typescript-cache");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
            Arc::new(typescript_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );
    let configured = scratch_worktree(
        "presence-typescript-configured",
        agent_ide::languages::TYPESCRIPT,
    );
    scheduler.trigger("repo", &configured);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(typescript_checker.calls().len(), 1);
    assert!(rust_checker.calls().is_empty());
    assert!(python_checker.calls().is_empty());
    let latest = scheduler.latest(&configured);
    assert_eq!(
        latest
            .iter()
            .map(|snapshot| snapshot.language)
            .collect::<Vec<_>>(),
        vec![
            agent_ide::languages::RUST,
            agent_ide::languages::PYTHON,
            agent_ide::languages::TYPESCRIPT
        ]
    );
    assert_eq!(latest[2].state, CheckState::Ready);

    let package_only = scratch_dir("presence-typescript-package-only");
    std::fs::write(package_only.join("package.json"), "{}").unwrap();
    scheduler.trigger("repo", &package_only);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(typescript_checker.calls().len(), 1);
    assert_eq!(
        scheduler.latest(&package_only)[2].state,
        CheckState::Unavailable(UnavailableReason::Disabled)
    );
}

/// An empty worktree (neither manifest present): neither checker is ever dispatched and both
/// languages report `Unavailable(Disabled)` (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_empty_worktree_checks_neither_language() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let python_checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("presence-empty-cache");
    let worktree = scratch_dir("presence-empty-worktree");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;

    assert_eq!(rust_checker.calls().len(), 0);
    assert_eq!(python_checker.calls().len(), 0);
    let latest = scheduler.latest(&worktree);
    assert_eq!(latest.len(), 2);
    assert!(
        latest
            .iter()
            .all(|snapshot| snapshot.state == CheckState::Unavailable(UnavailableReason::Disabled))
    );
}

/// A worktree that gains a `Cargo.toml` between two triggers starts being checked on the next
/// one, without requiring a restart (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_worktree_gaining_cargo_toml_is_checked_on_the_next_trigger() {
    agent_ide::languages::install();
    let rust_checker = RecordingChecker::new(agent_ide::languages::RUST);
    let cache_root = scratch_dir("presence-late-cache");
    let worktree = scratch_dir("presence-late-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(rust_checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(
        rust_checker.calls().len(),
        0,
        "rust must not be dispatched before Cargo.toml exists"
    );
    assert_eq!(
        scheduler.latest(&worktree)[0].state,
        CheckState::Unavailable(UnavailableReason::Disabled)
    );

    std::fs::write(worktree.join("Cargo.toml"), "[package]\n").unwrap();
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;

    assert_eq!(
        rust_checker.calls().len(),
        1,
        "rust must be dispatched once Cargo.toml appears"
    );
    assert_eq!(scheduler.latest(&worktree)[0].state, CheckState::Ready);
}

/// (T20B) A trigger whose fingerprint equals the last `Ready` completion's inputs skips the run
/// entirely: no second checker call, no `running` flag, and the stored snapshot stays current.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_with_unchanged_fingerprint_after_a_ready_result_skips_the_run() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("skip-unchanged-cache");
    let worktree = scratch_worktree("skip-unchanged-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    )
    .with_fingerprint(constant_fingerprint(Some(7)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1, "the first run always runs");
    let stored = scheduler.latest(&worktree);

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "unchanged inputs must not re-run the checker"
    );
    assert!(
        scheduler.running(&worktree).is_empty(),
        "a skipped run must never flip the running flag"
    );
    assert!(!scheduler.is_busy());
    let mut expected = stored;
    for snapshot in &mut expected {
        // The skip proved the retained result current for the new trigger's generation, so a
        // waiter keyed on that generation (an edit reply) sees it; nothing else changes.
        snapshot.input_generation = scheduler.generation(&worktree);
    }
    assert_eq!(
        scheduler.latest(&worktree),
        expected,
        "the stored snapshot must stay as the skipped-over completion left it, restamped with the evaluated generation"
    );
}

/// (T20B) A trigger whose fingerprint differs from the last completion's inputs runs.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_with_a_changed_fingerprint_reruns_the_check() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cell = Arc::new(AtomicU64::new(7));
    let cache_root = scratch_dir("skip-changed-cache");
    let worktree = scratch_worktree("skip-changed-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    )
    .with_fingerprint(shared_fingerprint(Arc::clone(&cell)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1);

    cell.store(8, Ordering::SeqCst);
    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "changed inputs must re-run the checker"
    );
}

/// (T20B) An activation's check always runs even with unchanged inputs, and a later ordinary
/// trigger with the same unchanged inputs skips again.
#[tokio::test(start_paused = true)]
async fn scheduler_activate_with_unchanged_fingerprint_still_runs() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("skip-activate-cache");
    let worktree = scratch_worktree("skip-activate-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    )
    .with_fingerprint(constant_fingerprint(Some(7)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1);

    scheduler.activate("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "an activation-requested check must never be skipped"
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "an ordinary trigger after the activation's run skips on unchanged inputs again"
    );
}

/// (T20B) An unknown fingerprint (`None`) never skips: unknown means changed means run.
#[tokio::test(start_paused = true)]
async fn scheduler_unknown_fingerprint_always_runs() {
    agent_ide::languages::install();
    let checker = RecordingChecker::new(agent_ide::languages::PYTHON);
    let cache_root = scratch_dir("skip-unknown-cache");
    let worktree = scratch_worktree("skip-unknown-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    )
    .with_fingerprint(constant_fingerprint(None));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(checker.calls().len(), 1);

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "an unknown fingerprint must always run the check"
    );
}

/// (T20B) A run that ended `Fatal` (like any non-`Ready` outcome) is re-checked on the next
/// trigger even when the fingerprint is unchanged since that failed run; only a `Ready`
/// completion arms the skip.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_after_a_fatal_completion_reruns_despite_unchanged_fingerprint() {
    agent_ide::languages::install();
    let checker = RecordingChecker::with_states(
        agent_ide::languages::PYTHON,
        vec![
            CheckState::Unavailable(UnavailableReason::Fatal),
            CheckState::Ready,
        ],
    );
    let cache_root = scratch_dir("skip-fatal-cache");
    let worktree = scratch_worktree("skip-fatal-worktree", agent_ide::languages::PYTHON);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root.path(),
    )
    .with_fingerprint(constant_fingerprint(Some(7)));

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        1,
        "a pair with no completed snapshot runs"
    );
    assert_eq!(
        scheduler.latest(&worktree)[0].state,
        CheckState::Unavailable(UnavailableReason::Fatal)
    );

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(60), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "unchanged inputs must still re-run after a Fatal completion"
    );
    assert_eq!(scheduler.latest(&worktree)[0].state, CheckState::Ready);

    scheduler.trigger("repo", &worktree);
    settle(Duration::from_millis(120), Duration::from_millis(5)).await;
    assert_eq!(
        checker.calls().len(),
        2,
        "only a Ready completion arms the skip"
    );
}

/// A scratch root owns the lease state the scheduler keeps beside its cache root, after a pass
/// and after a failing assertion alike, so nothing is left in the system temporary directory.
#[test]
fn scratch_roots_remove_their_lease_state_after_pass_and_failure() {
    let seen = Mutex::new(Vec::new());
    let body = |fail: bool| {
        let cache_root = scratch_dir("lease-state");
        let root = cache_root.parent().unwrap().to_path_buf();
        // Where the scheduler's retention leases live: `<cache root parent>/locks`.
        std::fs::create_dir_all(root.join("locks")).unwrap();
        std::fs::write(root.join("locks/lease"), b"held").unwrap();
        seen.lock().unwrap().push(root);
        if fail {
            panic!("failing test body");
        }
    };
    body(false);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(true))).is_err());
    let roots = seen.lock().unwrap().clone();
    assert_eq!(roots.len(), 2);
    for root in roots {
        assert!(!root.exists(), "{root:?} survived");
    }
}
