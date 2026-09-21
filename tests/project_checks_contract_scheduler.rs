//! Contract tests for [`agent_ide::checks::scheduler::Scheduler`] (EYES-r1 §5, EYES-r2 §5).
//!
//! `FakeChecker` (in `agent_ide::checks`) does not record concurrency or reflect the dispatched
//! `input_generation`, so these tests use `RecordingChecker` below instead.

use agent_ide::checks::scheduler::{FingerprintFn, RustCacheClone, Scheduler, sweep_stale_caches};
use agent_ide::checks::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason,
};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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

/// Builds a scratch worktree that is present for `language` (T10B), so tests exercising the
/// scheduler through `RecordingChecker` are not short-circuited by the presence gate meant for
/// real per-language checkers.
fn scratch_worktree(name: &str, language: Language) -> PathBuf {
    let dir = scratch_dir(name);
    match language {
        Language::Rust => std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap(),
        Language::Python => std::fs::write(dir.join("pyproject.toml"), "").unwrap(),
    }
    dir
}

/// Builds a scratch worktree present for both languages (T10B).
fn scratch_worktree_both_languages(name: &str) -> PathBuf {
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

/// Ten triggers within one debounce window collapse into exactly one check, whose cache dir is 0700.
#[tokio::test(start_paused = true)]
async fn scheduler_burst_of_triggers_debounces_to_one_check() {
    let checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("burst-cache");
    let worktree = scratch_worktree("burst-worktree", Language::Python);
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
    let worktree = scratch_worktree("dirty-worktree", Language::Python);
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

/// (T28B) A trigger that lands while a check runs marks the pair dirty, but the follow-up run is
/// subject to the same T20B skip-unchanged rule: with inputs unchanged from the completed `Ready`
/// run, the follow-up spawns no checker process at all and leaves the stored snapshot current.
#[tokio::test(start_paused = true)]
async fn scheduler_dirty_rerun_with_unchanged_fingerprint_is_skipped() {
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_millis(200));
    let cache_root = scratch_dir("dirty-skip-cache");
    let worktree = scratch_worktree("dirty-skip-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root,
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
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_millis(200));
    let cell = Arc::new(AtomicU64::new(7));
    let cache_root = scratch_dir("dirty-changed-cache");
    let worktree = scratch_worktree("dirty-changed-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(50),
        2,
        cache_root,
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
    let checker = RecordingChecker::with_delay(Language::Python, Duration::from_millis(200));
    let cache_root = scratch_dir("latest-cache");
    let worktree = scratch_worktree("latest-worktree", Language::Python);
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
    let worktree = scratch_worktree("shutdown-worktree", Language::Python);
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
    let worktree_a = scratch_worktree("clone-worktree-a", Language::Rust);
    let worktree_b = scratch_worktree("clone-worktree-b", Language::Rust);
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
    let worktree = scratch_worktree("fatal-worktree", Language::Python);
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
    let worktree = scratch_worktree("cooldown-worktree", Language::Python);
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

/// A worktree with only a `Cargo.toml`: Rust is checked, Python is never dispatched and reports
/// `Unavailable(Disabled)` without creating a cache directory (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_rust_only_worktree_checks_rust_and_reports_python_disabled() {
    let rust_checker = RecordingChecker::new(Language::Rust);
    let python_checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("presence-rust-only-cache");
    let worktree = scratch_worktree("presence-rust-only-worktree", Language::Rust);
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root.clone(),
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
        .find(|snapshot| snapshot.language == Language::Python)
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
    let rust_checker = RecordingChecker::new(Language::Rust);
    let python_checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("presence-python-only-cache");
    let worktree = scratch_worktree("presence-python-only-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root,
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
        .find(|snapshot| snapshot.language == Language::Rust)
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
    let rust_checker = RecordingChecker::new(Language::Rust);
    let python_checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("presence-both-cache");
    let worktree = scratch_worktree_both_languages("presence-both-worktree");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root,
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

/// An empty worktree (neither manifest present): neither checker is ever dispatched and both
/// languages report `Unavailable(Disabled)` (T10B).
#[tokio::test(start_paused = true)]
async fn scheduler_empty_worktree_checks_neither_language() {
    let rust_checker = RecordingChecker::new(Language::Rust);
    let python_checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("presence-empty-cache");
    let worktree = scratch_dir("presence-empty-worktree");
    let scheduler = Scheduler::new(
        vec![
            Arc::new(rust_checker.clone()),
            Arc::new(python_checker.clone()),
        ],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let rust_checker = RecordingChecker::new(Language::Rust);
    let cache_root = scratch_dir("presence-late-cache");
    let worktree = scratch_dir("presence-late-worktree");
    let scheduler = Scheduler::new(
        vec![Arc::new(rust_checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("skip-unchanged-cache");
    let worktree = scratch_worktree("skip-unchanged-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    assert_eq!(
        scheduler.latest(&worktree),
        stored,
        "the stored snapshot must stay exactly as the skipped-over completion left it"
    );
}

/// (T20B) A trigger whose fingerprint differs from the last completion's inputs runs.
#[tokio::test(start_paused = true)]
async fn scheduler_trigger_with_a_changed_fingerprint_reruns_the_check() {
    let checker = RecordingChecker::new(Language::Python);
    let cell = Arc::new(AtomicU64::new(7));
    let cache_root = scratch_dir("skip-changed-cache");
    let worktree = scratch_worktree("skip-changed-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("skip-activate-cache");
    let worktree = scratch_worktree("skip-activate-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::new(Language::Python);
    let cache_root = scratch_dir("skip-unknown-cache");
    let worktree = scratch_worktree("skip-unknown-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
    let checker = RecordingChecker::with_states(
        Language::Python,
        vec![
            CheckState::Unavailable(UnavailableReason::Fatal),
            CheckState::Ready,
        ],
    );
    let cache_root = scratch_dir("skip-fatal-cache");
    let worktree = scratch_worktree("skip-fatal-worktree", Language::Python);
    let scheduler = Scheduler::new(
        vec![Arc::new(checker.clone())],
        Duration::from_millis(10),
        2,
        cache_root,
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
