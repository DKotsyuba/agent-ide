//! Contract tests for [`agent_ide::checks::scheduler::Scheduler`] (EYES-r1 §5, EYES-r2 §5).
//!
//! `FakeChecker` (in `agent_ide::checks`) does not record concurrency or reflect the dispatched
//! `input_generation`, so these tests use `RecordingChecker` below instead.

use agent_ide::checks::scheduler::{RustCacheClone, Scheduler, sweep_stale_caches};
use agent_ide::checks::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason,
};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "agent-ide-scheduler-contract-{}-{name}-{}",
        std::process::id(),
        name.len()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

/// Ten triggers within one debounce window collapse into exactly one check, whose cache dir is 0700.
#[tokio::test(start_paused = true)]
async fn scheduler_burst_of_triggers_debounces_to_one_check() {
    let checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("burst-cache");
    let worktree = scratch_dir("burst-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root,
    );

    for _ in 0..10 {
        scheduler.trigger("repo", &worktree);
        advance(Duration::from_millis(5)).await;
    }
    settle(Duration::from_millis(300), Duration::from_millis(5)).await;

    assert_eq!(checker.calls().len(), 1);
    let latest = scheduler.latest(&worktree);
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].language, Language::Python);
    assert!(!scheduler.is_busy());

    let cache_dir = &checker.calls()[0].cache_dir;
    let mode = std::fs::metadata(cache_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

/// A trigger that lands while a check is running marks it dirty for exactly one latest-wins rerun.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_during_running_check_causes_one_extra_run_with_newest_generation() {
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_millis(200));
    let cache_root = scratch_dir("dirty-cache");
    let worktree = scratch_dir("dirty-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root,
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

/// Five worktrees times two languages never exceed the configured global concurrency cap.
#[tokio::test(start_paused = true)]
async fn scheduler_never_runs_more_than_max_concurrent_checks_across_worktrees_and_languages() {
    let concurrent = Arc::new(AtomicUsize::new(0));
    let max_seen = Arc::new(AtomicUsize::new(0));
    let rust_checker = RecordingChecker::with_shared(
        Language::Rust,
        Some(Duration::from_millis(100)),
        Arc::clone(&concurrent),
        Arc::clone(&max_seen),
    );
    let python_checker = RecordingChecker::with_shared(
        Language::Python,
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
        cache_root,
    );

    let worktrees: Vec<PathBuf> = (0..5)
        .map(|i| scratch_dir(&format!("fanout-wt-{i}")))
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
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_millis(200));
    let cache_root = scratch_dir("latest-cache");
    let worktree = scratch_dir("latest-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_secs(3600));
    let cache_root = scratch_dir("shutdown-cache");
    let worktree = scratch_dir("shutdown-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::new(Language::Rust);
    let cache_root = scratch_dir("clone-cache");
    let worktree_a = scratch_dir("clone-worktree-a");
    let worktree_b = scratch_dir("clone-worktree-b");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
    );

    scheduler.trigger("shared-repo", &worktree_a);
    advance(Duration::from_millis(30)).await;
    assert_eq!(checker.calls().len(), 1);
    assert_eq!(
        scheduler.rust_cache_clone_outcome(&worktree_a),
        RustCacheClone::SkippedNoSource,
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

    let outcome = scheduler.rust_cache_clone_outcome(&worktree_b);
    let cache_dir_b = checker.calls()[1].cache_dir.clone();
    match outcome {
        RustCacheClone::Cloned => {
            assert_eq!(
                std::fs::read(cache_dir_b.join("target/marker.txt")).unwrap(),
                b"built"
            );
        }
        RustCacheClone::Failed => {
            // Non-APFS filesystems reject `cp -c`; the scheduler must fall back to a cold
            // check rather than fail, which this arm confirms without asserting file content.
        }
        other => panic!("expected Cloned or a Failed fallback, got {other:?}"),
    }
}

/// A transient Fatal completion keeps the prior Ready snapshot; a durable Unavailable reason replaces it.
#[tokio::test(start_paused = true)]
async fn scheduler_fatal_or_timeout_completion_never_replaces_a_ready_snapshot() {
    let checker = RecordingChecker::with_states(
        Language::Python,
        vec![
            CheckState::Ready,
            CheckState::Unavailable(UnavailableReason::Fatal),
            CheckState::Unavailable(UnavailableReason::ToolMissing),
        ],
    );
    let cache_root = scratch_dir("fatal-cache");
    let worktree = scratch_dir("fatal-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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

/// `sweep_stale_caches` removes only cache directories whose recorded worktree path is gone.
#[test]
fn scheduler_sweep_stale_caches_removes_only_worktrees_that_no_longer_exist() {
    let cache_root = scratch_dir("sweep-cache-root");
    let live_worktree = scratch_dir("sweep-live-worktree");

    // A worktree that still exists on disk: its cache directory must survive the sweep.
    let live_dir = cache_root.join("repo-hash").join("live-worktree-hash");
    std::fs::create_dir_all(live_dir.join("python")).unwrap();
    std::fs::write(
        live_dir.join("worktree.path"),
        live_worktree.to_string_lossy().as_bytes(),
    )
    .unwrap();

    // A worktree that has since been removed: its cache directory must be swept away.
    let gone_worktree = scratch_dir("sweep-gone-worktree");
    let gone_dir = cache_root.join("repo-hash").join("gone-worktree-hash");
    std::fs::create_dir_all(gone_dir.join("python")).unwrap();
    std::fs::write(
        gone_dir.join("worktree.path"),
        gone_worktree.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::remove_dir_all(&gone_worktree).unwrap();

    // A different repository whose only worktree is also gone: the whole repository-level
    // directory must be removed once it has no worktree subdirectories left.
    let other_gone_worktree = scratch_dir("sweep-other-repo-gone-worktree");
    let other_repo_dir = cache_root
        .join("other-repo-hash")
        .join("other-worktree-hash");
    std::fs::create_dir_all(other_repo_dir.join("rust")).unwrap();
    std::fs::write(
        other_repo_dir.join("worktree.path"),
        other_gone_worktree.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::fs::remove_dir_all(&other_gone_worktree).unwrap();

    sweep_stale_caches(&cache_root);

    assert!(
        live_dir.exists(),
        "a cache for an existing worktree must survive the sweep"
    );
    assert!(
        !gone_dir.exists(),
        "a cache for a removed worktree must be swept away"
    );
    assert!(
        !cache_root.join("other-repo-hash").exists(),
        "a repository directory left with no worktrees must be removed too"
    );
}

/// The next run waits max(debounce, previous run duration) after the previous completion, not just the debounce.
#[tokio::test(start_paused = true)]
async fn scheduler_enforces_a_cooldown_of_max_debounce_and_previous_duration() {
    let debounce = Duration::from_millis(10);
    let run_duration = Duration::from_millis(150);
    let checker = RecordingChecker::with_delay(Language::Python, run_duration);
    let cache_root = scratch_dir("cooldown-cache");
    let worktree = scratch_dir("cooldown-worktree");
    let scheduler = Scheduler::new(vec![Arc::new(checker.clone())], debounce, 2, cache_root);

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
