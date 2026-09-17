//! Debounced project check scheduler for EYES-r1 §5, updated for the EYES-r2 §5 cooldown and
//! failure/cache-layout refinements.
//!
//! [`Scheduler`] owns, per `(worktree, language)`, a debounce timer, at most one running
//! [`Checker`] invocation, and the latest completed [`ProblemSnapshot`]. It never blocks a
//! caller: [`Scheduler::trigger`] only restarts a timer, and [`Scheduler::latest`] only reads
//! the last stored result. A new worktree's first Rust check is preceded by a best-effort
//! copy-on-write clone of a sibling worktree's `target/` directory. Between completions, a run
//! is further throttled by a cooldown of `max(debounce, previous run duration)`, and a
//! transient `Fatal`/`Timeout` completion never overwrites an existing `Ready`/`Partial` result.
//! Cache directories live under a caller-supplied `cache_root` (for example
//! `$HOME/.agent-ide/checks`) and record enough to let [`sweep_stale_caches`] reclaim caches for
//! worktrees that no longer exist.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::AbortHandle;
// `tokio::time::Instant`, not `std::time::Instant`: the EYES-r2 cooldown must track the same
// (possibly paused/advanced) clock that `tokio::time::sleep` uses, so tests can drive both with
// `tokio::time::advance` deterministically.
use tokio::time::Instant;

use super::{CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason};

/// Name of the marker file written in each worktree-level cache directory, recording the
/// worktree's canonical path so [`sweep_stale_caches`] can find directories to remove.
const WORKTREE_MARKER_FILE_NAME: &str = "worktree.path";

/// Outcome of the copy-on-write `target/` clone attempted before a worktree's first Rust check.
///
/// Exposed so tests can observe the clone decision without depending on filesystem timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RustCacheClone {
    /// No Rust check has reached the clone decision for this worktree yet.
    NotAttempted,
    /// `cp -c -R` completed successfully; the destination `target/` was populated from `source`.
    Cloned,
    /// No sibling worktree of the same repository had a completed Rust cache dir to clone from.
    SkippedNoSource,
    /// A source `target/` existed but the clone process failed; the check proceeds cold.
    Failed,
}

/// Cheaply clonable handle to the scheduler; every clone shares the same underlying state.
///
/// Construct one [`Scheduler`] per daemon and share clones across trigger sources (`ide.start`,
/// post-hooks, `ide.edit`). Dropping every clone stops new work from being scheduled but does
/// not cancel work already in flight; call [`Scheduler::shutdown`] for that.
#[derive(Clone)]
pub struct Scheduler {
    inner: Arc<Inner>,
}

/// Shared state and configuration behind every [`Scheduler`] clone.
struct Inner {
    /// Checkers keyed by the language they run; fixed for the scheduler's lifetime.
    checkers: HashMap<Language, Arc<dyn Checker>>,
    /// Debounce delay applied to every `(worktree, language)` timer.
    debounce: Duration,
    /// Global cap on concurrently running checks across every worktree and language.
    semaphore: Arc<Semaphore>,
    /// Root directory under which per-worktree, per-language cache directories are created.
    cache_root: PathBuf,
    /// Mutable scheduling state, locked only for the duration of a synchronous read or write.
    state: Mutex<State>,
    /// Optional observer called once with every snapshot a [`Checker`] run completes with.
    on_complete: Option<CompletionHook>,
}

/// Observer of completed check runs, installed through [`Scheduler::with_completion_hook`].
///
/// Called synchronously on the scheduler task right after a run completes and before the
/// snapshot is stored, never while scheduler state is locked. It must return promptly and must
/// not call back into the scheduler.
pub type CompletionHook = Arc<dyn Fn(&ProblemSnapshot) + Send + Sync>;

/// Mutable scheduler state, guarded by [`Inner::state`].
///
/// Never held across an `.await` point: every lock scope in this module ends before the next
/// await, so no caller can block on scheduler-internal work while holding this lock.
#[derive(Default)]
struct State {
    /// Set once by [`Scheduler::shutdown`]; new triggers and timer firings become no-ops after.
    shutting_down: bool,
    /// Per-worktree state, keyed by the worktree's canonical path.
    worktrees: HashMap<PathBuf, WorktreeState>,
    /// Per-repository state shared by every worktree reporting the same repository key.
    repositories: HashMap<String, RepositoryState>,
}

/// Scheduling state for one worktree.
struct WorktreeState {
    /// Repository identity shared with sibling worktrees for Rust cache cloning.
    repository_key: String,
    /// Counter incremented on every [`Scheduler::trigger`] call for this worktree.
    input_generation: u64,
    /// Per-language state, created lazily on first trigger or debounce firing.
    languages: HashMap<Language, LanguageState>,
    /// Outcome of this worktree's Rust `target/` clone decision, for [`Scheduler::rust_cache_clone_outcome`].
    rust_clone_outcome: RustCacheClone,
}

impl WorktreeState {
    /// Builds the initial state for a worktree first seen by [`Scheduler::trigger`].
    fn new(repository_key: &str) -> Self {
        Self {
            repository_key: repository_key.to_string(),
            input_generation: 0,
            languages: HashMap::new(),
            rust_clone_outcome: RustCacheClone::NotAttempted,
        }
    }
}

/// Scheduling state for one `(worktree, language)` pair.
#[derive(Default)]
struct LanguageState {
    /// Cancellation handle for a debounce timer that has not fired yet, if one is pending.
    timer_abort: Option<AbortHandle>,
    /// Cancellation handle for the task driving the currently running check, if one is running.
    run_abort: Option<AbortHandle>,
    /// `true` from the moment a check is dispatched until its run loop exits.
    running: bool,
    /// `true` when a debounce timer fired while a check was already running; consumed by the
    /// running check's completion to trigger exactly one latest-wins rerun.
    dirty: bool,
    /// Latest completed snapshot, absent until this pair's first check completes.
    latest_snapshot: Option<ProblemSnapshot>,
    /// `input_generation` of [`LanguageState::latest_snapshot`], used to discard stale completions.
    last_stored_generation: u64,
    /// Wall-clock instant the most recent run completed, for the EYES-r2 cooldown.
    last_completion: Option<Instant>,
    /// Wall-clock duration of the most recent run, for the EYES-r2 cooldown.
    last_duration: Duration,
}

/// Per-repository state shared across sibling worktrees.
#[derive(Default)]
struct RepositoryState {
    /// Cache directory of the most recently completed Rust check among this repository's worktrees.
    most_recent_rust_cache_dir: Option<PathBuf>,
}

impl Scheduler {
    /// Builds a scheduler with no worktrees registered yet.
    ///
    /// `checkers` supplies one [`Checker`] per language the scheduler runs; a duplicate language
    /// keeps the last entry. `debounce` is the quiet period awaited after the last trigger for a
    /// `(worktree, language)` pair before its check starts. `max_concurrent` bounds how many
    /// checks run at once across every worktree and language (EYES-r1 §5 default: 2); a value of
    /// `0` is treated as `1` so the scheduler always makes progress. `cache_root` is the parent
    /// directory under which per-worktree, per-language cache directories are created.
    pub fn new(
        checkers: Vec<Arc<dyn Checker>>,
        debounce: Duration,
        max_concurrent: usize,
        cache_root: PathBuf,
    ) -> Self {
        let checkers = checkers
            .into_iter()
            .map(|checker| (checker.language(), checker))
            .collect();
        Self {
            inner: Arc::new(Inner {
                checkers,
                debounce,
                semaphore: Arc::new(Semaphore::new(max_concurrent.max(1))),
                cache_root,
                state: Mutex::new(State::default()),
                on_complete: None,
            }),
        }
    }

    /// Installs `hook`, called with every snapshot a check run completes with (for example to
    /// record telemetry), including completions later discarded by the storage guards.
    ///
    /// Must be called on the freshly built scheduler before it is cloned or triggered.
    ///
    /// # Panics
    ///
    /// Panics when another clone of this scheduler already exists.
    pub fn with_completion_hook(mut self, hook: CompletionHook) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("completion hook must be installed before the scheduler is shared")
            .on_complete = Some(hook);
        self
    }

    /// Records a trigger for `worktree` and restarts the debounce timer for every configured
    /// language.
    ///
    /// Non-blocking and never awaits: it only updates in-memory state and spawns the timer
    /// tasks that will later run checks. `repository_key` identifies the repository this
    /// worktree belongs to, for Rust cache cloning between sibling worktrees; it is refreshed on
    /// every call in case a worktree's repository identity changes. Increments this worktree's
    /// `input_generation` once regardless of how many languages are configured. A trigger
    /// received after [`Scheduler::shutdown`] has started is silently ignored.
    pub fn trigger(&self, repository_key: &str, worktree: &Path) {
        let worktree = canonical_worktree(worktree);
        let mut state = self.inner.lock_state();
        if state.shutting_down {
            return;
        }
        let wt = state
            .worktrees
            .entry(worktree.clone())
            .or_insert_with(|| WorktreeState::new(repository_key));
        wt.repository_key = repository_key.to_string();
        wt.input_generation += 1;
        for language in self.inner.checkers.keys().copied().collect::<Vec<_>>() {
            let lang = wt.languages.entry(language).or_default();
            if let Some(previous) = lang.timer_abort.take() {
                previous.abort();
            }
            let inner = Arc::clone(&self.inner);
            let worktree_for_task = worktree.clone();
            let handle = tokio::spawn(async move {
                tokio::time::sleep(inner.debounce).await;
                Inner::on_debounce_fire(inner, worktree_for_task, language).await;
            });
            lang.timer_abort = Some(handle.abort_handle());
        }
    }

    /// Returns the latest completed snapshot for every language of `worktree` that has finished
    /// at least once, ordered by [`Language`].
    ///
    /// Non-blocking and synchronous: it only reads the in-memory latest-snapshot table and never
    /// waits for a running check. A language whose first check has not completed yet, or that is
    /// not configured on this scheduler, is absent from the result rather than represented by a
    /// placeholder.
    pub fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot> {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        let mut snapshots: Vec<ProblemSnapshot> = state
            .worktrees
            .get(&worktree)
            .map(|wt| {
                wt.languages
                    .values()
                    .filter_map(|lang| lang.latest_snapshot.clone())
                    .collect()
            })
            .unwrap_or_default();
        snapshots.sort_by_key(|snapshot| snapshot.language);
        snapshots
    }

    /// Reports whether any `(worktree, language)` pair has a check running or a debounce timer
    /// pending.
    ///
    /// Intended for the idle controller: while `true`, the daemon should not consider itself
    /// idle. Non-blocking and synchronous.
    pub fn is_busy(&self) -> bool {
        let state = self.inner.lock_state();
        state.worktrees.values().any(|wt| {
            wt.languages
                .values()
                .any(|lang| lang.running || lang.timer_abort.is_some())
        })
    }

    /// Returns the outcome of `worktree`'s Rust `target/` clone decision.
    ///
    /// [`RustCacheClone::NotAttempted`] until this worktree's first Rust check has reached the
    /// clone decision point; a debug-visible field kept mainly for tests.
    pub fn rust_cache_clone_outcome(&self, worktree: &Path) -> RustCacheClone {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        state
            .worktrees
            .get(&worktree)
            .map(|wt| wt.rust_clone_outcome)
            .unwrap_or(RustCacheClone::NotAttempted)
    }

    /// Cancels every pending debounce timer and drops every running check's future, then
    /// returns.
    ///
    /// A dropped check future is the [`Checker`] contract's cancellation signal, so any confined
    /// process it owns is killed as part of the drop. Returns promptly: it does not wait for a
    /// cancelled task's own unwind beyond one scheduling yield. Triggers received after this call
    /// starts are ignored; already-registered timers and runs are aborted even if this call races
    /// their completion.
    pub async fn shutdown(&self) {
        {
            let mut state = self.inner.lock_state();
            state.shutting_down = true;
            for wt in state.worktrees.values_mut() {
                for lang in wt.languages.values_mut() {
                    if let Some(timer) = lang.timer_abort.take() {
                        timer.abort();
                    }
                    if let Some(run) = lang.run_abort.take() {
                        run.abort();
                    }
                    lang.running = false;
                    lang.dirty = false;
                }
            }
        }
        tokio::task::yield_now().await;
    }
}

impl Inner {
    /// Locks [`Inner::state`], panicking only if a prior holder panicked while holding it.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("scheduler state mutex is not poisoned")
    }

    /// Runs once a `(worktree, language)` debounce timer's sleep completes.
    ///
    /// Clears the timer handle first so a subsequent trigger starts an independent new timer
    /// rather than perceiving this firing one as still pending. If a check is already running for
    /// this pair, marks it dirty and returns; otherwise marks the pair running and spawns
    /// [`Inner::run_check_loop`] to perform it.
    async fn on_debounce_fire(inner: Arc<Self>, worktree: PathBuf, language: Language) {
        let should_run = {
            let mut state = inner.lock_state();
            if state.shutting_down {
                return;
            }
            let Some(wt) = state.worktrees.get_mut(&worktree) else {
                return;
            };
            let Some(lang) = wt.languages.get_mut(&language) else {
                return;
            };
            lang.timer_abort = None;
            if lang.running {
                lang.dirty = true;
                false
            } else {
                lang.running = true;
                true
            }
        };
        if !should_run {
            return;
        }
        let handle = tokio::spawn(Inner::run_check_loop(
            Arc::clone(&inner),
            worktree.clone(),
            language,
        ));
        let mut state = inner.lock_state();
        if let Some(lang) = state
            .worktrees
            .get_mut(&worktree)
            .and_then(|wt| wt.languages.get_mut(&language))
        {
            lang.run_abort = Some(handle.abort_handle());
        }
    }

    /// Drives one or more sequential check runs for `(worktree, language)` until no rerun is
    /// pending.
    ///
    /// Each iteration first waits out any pending EYES-r2 cooldown (see
    /// [`Inner::cooldown_remaining`]), then prepares the cache directory (cloning Rust's
    /// `target/` on the worktree's first Rust check when possible), acquires the shared
    /// concurrency permit, dispatches the configured [`Checker`], reports the completion to the
    /// optional [`CompletionHook`], stores the resulting snapshot
    /// (subject to the generation and Fatal/Timeout guards in [`Inner::store_snapshot`]),
    /// records this completion's timing for the next iteration's cooldown, and records Rust
    /// cache completion for sibling worktrees. If the pair was marked dirty while this run was
    /// in flight, one more iteration follows with the latest `input_generation`; the [`Checker`]
    /// contract's cancellation is dropping its future, which happens automatically when
    /// [`Scheduler::shutdown`] aborts this task mid-await (including while it is waiting out a
    /// cooldown).
    async fn run_check_loop(inner: Arc<Self>, worktree: PathBuf, language: Language) {
        loop {
            if inner.is_shutting_down() {
                inner.finish_run(&worktree, language, false);
                return;
            }
            let cooldown = inner.cooldown_remaining(&worktree, language);
            if !cooldown.is_zero() {
                tokio::time::sleep(cooldown).await;
            }
            if inner.is_shutting_down() {
                inner.finish_run(&worktree, language, false);
                return;
            }
            let cache_dir = inner.prepare_cache_dir(&worktree, language).await;
            let permit = match Arc::clone(&inner.semaphore).acquire_owned().await {
                Ok(permit) => permit,
                Err(_closed) => {
                    inner.finish_run(&worktree, language, false);
                    return;
                }
            };
            if inner.is_shutting_down() {
                drop(permit);
                inner.finish_run(&worktree, language, false);
                return;
            }
            let Some(checker) = inner.checkers.get(&language).cloned() else {
                drop(permit);
                inner.finish_run(&worktree, language, false);
                return;
            };
            let generation = inner.current_generation(&worktree);
            let request = CheckRequest {
                worktree: worktree.clone(),
                cache_dir: cache_dir.clone(),
                input_generation: generation,
            };
            let started = Instant::now();
            let snapshot = checker.check(request).await;
            let duration = started.elapsed();
            drop(permit);
            if let Some(hook) = &inner.on_complete {
                hook(&snapshot);
            }
            inner.store_snapshot(&worktree, snapshot);
            inner.record_completion(&worktree, language, duration);
            if language == Language::Rust {
                inner.record_completed_rust_cache(&worktree, &cache_dir);
            }
            if !inner.finish_run(&worktree, language, true) {
                return;
            }
        }
    }

    /// Reports whether [`Scheduler::shutdown`] has started.
    fn is_shutting_down(&self) -> bool {
        self.lock_state().shutting_down
    }

    /// Reads `worktree`'s current `input_generation`, or `0` if it has no state yet.
    fn current_generation(&self, worktree: &Path) -> u64 {
        self.lock_state()
            .worktrees
            .get(worktree)
            .map(|wt| wt.input_generation)
            .unwrap_or(0)
    }

    /// Stores `snapshot` as the latest result for its `(worktree, language)` pair, unless a
    /// newer completion (by `input_generation`) is already stored, or `snapshot` is a transient
    /// `Fatal`/`Timeout` failure (EYES-r2 §5) that would overwrite an existing usable
    /// `Ready`/`Partial` result. Every other `Unavailable` reason still replaces the stored
    /// result, since those describe a durable condition (disabled, outside roots, tool/env
    /// missing) rather than one bad run.
    fn store_snapshot(&self, worktree: &Path, snapshot: ProblemSnapshot) {
        let mut state = self.lock_state();
        let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .and_then(|wt| wt.languages.get_mut(&snapshot.language))
        else {
            return;
        };
        if is_transient_failure(&snapshot.state)
            && matches!(
                lang.latest_snapshot
                    .as_ref()
                    .map(|existing| &existing.state),
                Some(CheckState::Ready) | Some(CheckState::Partial)
            )
        {
            return;
        }
        if lang.latest_snapshot.is_none()
            || snapshot.input_generation >= lang.last_stored_generation
        {
            lang.last_stored_generation = snapshot.input_generation;
            lang.latest_snapshot = Some(snapshot);
        }
    }

    /// Records `duration` as the wall-clock time `(worktree, language)`'s most recently
    /// completed run took, together with the completion instant, for the next iteration's
    /// [`Inner::cooldown_remaining`] check.
    fn record_completion(&self, worktree: &Path, language: Language, duration: Duration) {
        let mut state = self.lock_state();
        if let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .and_then(|wt| wt.languages.get_mut(&language))
        {
            lang.last_completion = Some(Instant::now());
            lang.last_duration = duration;
        }
    }

    /// Computes how much longer `(worktree, language)` must wait before its next run starts
    /// (EYES-r2 §5): no earlier than `max(debounce, previous run duration)` after the previous
    /// completion. Returns [`Duration::ZERO`] before any run has ever completed, or once that
    /// interval has already elapsed.
    fn cooldown_remaining(&self, worktree: &Path, language: Language) -> Duration {
        let state = self.lock_state();
        let Some(lang) = state
            .worktrees
            .get(worktree)
            .and_then(|wt| wt.languages.get(&language))
        else {
            return Duration::ZERO;
        };
        let Some(last_completion) = lang.last_completion else {
            return Duration::ZERO;
        };
        let required = self.debounce.max(lang.last_duration);
        required.saturating_sub(last_completion.elapsed())
    }

    /// Records `cache_dir` as `worktree`'s repository's most recently completed Rust cache
    /// directory, for sibling worktrees' future clone attempts.
    fn record_completed_rust_cache(&self, worktree: &Path, cache_dir: &Path) {
        let mut state = self.lock_state();
        let Some(repository_key) = state
            .worktrees
            .get(worktree)
            .map(|wt| wt.repository_key.clone())
        else {
            return;
        };
        state
            .repositories
            .entry(repository_key)
            .or_default()
            .most_recent_rust_cache_dir = Some(cache_dir.to_path_buf());
    }

    /// Ends the current iteration of a `(worktree, language)` run loop and reports whether
    /// another iteration should follow.
    ///
    /// Consumes a pending `dirty` flag into one more iteration when `allow_rerun` is set and the
    /// scheduler is not shutting down; otherwise clears `running`, `dirty`, and `run_abort` so the
    /// pair is idle again.
    fn finish_run(&self, worktree: &Path, language: Language, allow_rerun: bool) -> bool {
        let mut state = self.lock_state();
        let shutting_down = state.shutting_down;
        let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .and_then(|wt| wt.languages.get_mut(&language))
        else {
            return false;
        };
        if allow_rerun && lang.dirty && !shutting_down {
            lang.dirty = false;
            return true;
        }
        lang.running = false;
        lang.dirty = false;
        lang.run_abort = None;
        false
    }

    /// Creates `worktree`'s `language` cache directory if it is missing, attempting a Rust
    /// `target/` clone from a sibling worktree beforehand when this is the worktree's first Rust
    /// check.
    ///
    /// The directory layout is `<cache_root>/<hash(repository_key)>/<hash(canonical
    /// worktree)>/<language>` (EYES-r2 §5), so caches from different repositories never collide
    /// even if two unrelated worktrees hash to the same worktree-level segment by coincidence of
    /// path reuse. The first time a worktree's cache directory is created, a
    /// [`WORKTREE_MARKER_FILE_NAME`] file recording its canonical path is written alongside it,
    /// for [`sweep_stale_caches`] to later identify caches whose worktree no longer exists.
    async fn prepare_cache_dir(&self, worktree: &Path, language: Language) -> PathBuf {
        let repository_key = self
            .lock_state()
            .worktrees
            .get(worktree)
            .map(|wt| wt.repository_key.clone())
            .unwrap_or_default();
        let worktree_dir = self
            .cache_root
            .join(hash16(repository_key.as_bytes()))
            .join(hash16(worktree.to_string_lossy().as_bytes()));
        if !worktree_dir.exists() {
            if let Err(error) = create_private_dir(&worktree_dir) {
                eprintln!(
                    "agent-ide: scheduler failed to create cache dir {}: {error}",
                    worktree_dir.display()
                );
            }
            let marker = worktree_dir.join(WORKTREE_MARKER_FILE_NAME);
            if let Err(error) = std::fs::write(&marker, worktree.to_string_lossy().as_bytes()) {
                eprintln!(
                    "agent-ide: scheduler failed to write {}: {error}",
                    marker.display()
                );
            }
        }
        let dir = worktree_dir.join(language.as_str());
        let existed = dir.exists();
        if !existed && let Err(error) = create_private_dir(&dir) {
            eprintln!(
                "agent-ide: scheduler failed to create cache dir {}: {error}",
                dir.display()
            );
        }
        if language == Language::Rust && !existed {
            let outcome = self.try_clone_rust_cache(worktree, &dir).await;
            let mut state = self.lock_state();
            if let Some(wt) = state.worktrees.get_mut(worktree) {
                wt.rust_clone_outcome = outcome;
            }
        }
        dir
    }

    /// Attempts an APFS copy-on-write clone of a sibling worktree's `target/` into `dst_dir`.
    ///
    /// Looks up the most recently completed Rust cache directory recorded for `worktree`'s
    /// repository; if none exists, or its `target/` is missing, the check proceeds cold. The
    /// clone itself runs `/bin/cp -c -R <source>/target <dst_dir>/target`; any failure is logged
    /// and treated as a cold start rather than propagated.
    async fn try_clone_rust_cache(&self, worktree: &Path, dst_dir: &Path) -> RustCacheClone {
        let repository_key = {
            let state = self.lock_state();
            state
                .worktrees
                .get(worktree)
                .map(|wt| wt.repository_key.clone())
        };
        let Some(repository_key) = repository_key else {
            return RustCacheClone::NotAttempted;
        };
        let source_dir = {
            let state = self.lock_state();
            state
                .repositories
                .get(&repository_key)
                .and_then(|repository| repository.most_recent_rust_cache_dir.clone())
        };
        let Some(source_dir) = source_dir else {
            return RustCacheClone::SkippedNoSource;
        };
        let source_target = source_dir.join("target");
        if !source_target.exists() {
            return RustCacheClone::SkippedNoSource;
        }
        let destination_target = dst_dir.join("target");
        match tokio::process::Command::new("/bin/cp")
            .arg("-c")
            .arg("-R")
            .arg(&source_target)
            .arg(&destination_target)
            .output()
            .await
        {
            Ok(output) if output.status.success() => RustCacheClone::Cloned,
            Ok(output) => {
                eprintln!(
                    "agent-ide: scheduler clonefile of {} failed: {}",
                    source_target.display(),
                    String::from_utf8_lossy(&output.stderr)
                );
                RustCacheClone::Failed
            }
            Err(error) => {
                eprintln!(
                    "agent-ide: scheduler could not run /bin/cp for {}: {error}",
                    source_target.display()
                );
                RustCacheClone::Failed
            }
        }
    }
}

/// Resolves `worktree` to its canonical path, falling back to the given path unchanged when
/// canonicalization fails (for example a worktree removed since it was last triggered).
fn canonical_worktree(worktree: &Path) -> PathBuf {
    std::fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf())
}

/// Reports whether `state` is a transient failure (EYES-r2 §5: `Fatal` or `Timeout`) that must
/// not overwrite an existing usable result, as opposed to a durable `Unavailable` condition.
fn is_transient_failure(state: &CheckState) -> bool {
    matches!(
        state,
        CheckState::Unavailable(UnavailableReason::Fatal)
            | CheckState::Unavailable(UnavailableReason::Timeout)
    )
}

/// Derives a 16 hex character cache key from `bytes`: the first 8 bytes of its blake3 digest,
/// hex-encoded. Used for both the repository-key and canonical-worktree-path path segments.
fn hash16(bytes: &[u8]) -> String {
    let digest = blake3::hash(bytes);
    digest.as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Removes cache directories under `cache_root` whose worktree no longer exists on disk
/// (EYES-r2 §5 follow-up), for a daemon to call periodically outside any live [`Scheduler`].
///
/// Walks `<cache_root>/<repository hash>/<worktree hash>/`, reading each worktree-level
/// directory's [`WORKTREE_MARKER_FILE_NAME`] to recover the worktree path it was created for; a
/// directory whose recorded worktree path no longer exists is removed entirely, and a
/// repository-level directory left with no worktree subdirectories is removed too. A
/// worktree-level directory with no marker file (never written by this scheduler, or from an
/// older cache layout) is left untouched rather than guessed at. Best-effort: individual
/// filesystem errors are swallowed so one unreadable entry does not abort the sweep.
pub fn sweep_stale_caches(cache_root: &Path) {
    let Ok(repository_entries) = std::fs::read_dir(cache_root) else {
        return;
    };
    for repository_entry in repository_entries.flatten() {
        let repository_dir = repository_entry.path();
        if !repository_dir.is_dir() {
            continue;
        }
        let Ok(worktree_entries) = std::fs::read_dir(&repository_dir) else {
            continue;
        };
        let mut remaining = 0usize;
        for worktree_entry in worktree_entries.flatten() {
            let worktree_dir = worktree_entry.path();
            if !worktree_dir.is_dir() {
                continue;
            }
            let marker = worktree_dir.join(WORKTREE_MARKER_FILE_NAME);
            let Ok(recorded_path) = std::fs::read_to_string(&marker) else {
                remaining += 1;
                continue;
            };
            if Path::new(&recorded_path).exists() {
                remaining += 1;
            } else {
                let _ = std::fs::remove_dir_all(&worktree_dir);
            }
        }
        if remaining == 0 {
            let _ = std::fs::remove_dir_all(&repository_dir);
        }
    }
}

/// Creates `dir` (and its parents) if missing, then restricts `dir` itself to owner-only
/// `0700` permissions.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut permissions = std::fs::metadata(dir)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(dir, permissions)
}
