//! Debounced project check scheduler for EYES-r1 §5, updated for the EYES-r2 §5 cooldown and
//! failure/cache-layout refinements.
//!
//! [`Scheduler`](crate::checks::scheduler::Scheduler) owns, per `(worktree, language)`, a debounce timer, at most one running
//! [`Checker`] invocation, and the latest completed [`ProblemSnapshot`]. It never blocks a
//! caller: [`Scheduler::trigger`](crate::checks::scheduler::Scheduler::trigger) only restarts a timer, and
//! [`Scheduler::latest`](crate::checks::scheduler::Scheduler::latest) only reads
//! the last stored result. A new worktree's first check of a language whose checks seed a
//! sibling cache (see [`LanguageChecks::sibling_cache`](crate::checks::LanguageChecks::sibling_cache))
//! is preceded by a best-effort copy-on-write clone of a sibling worktree's cache subdirectory.
//! Between completions, a run
//! is further throttled by a cooldown of `max(debounce, previous run duration)`, and a
//! transient `Fatal`/`Timeout` completion never overwrites an existing `Ready`/`Partial` result.
//! (T20B) A debounce firing whose worktree inputs are unchanged since the pair's last completed
//! `Ready` run skips the run entirely; an activation-requested check is never skipped.
//! Cache directories live under a caller-supplied `cache_root` (for example
//! `$HOME/.agent-ide/checks`) and record enough to let [`sweep_stale_caches`](crate::checks::scheduler::sweep_stale_caches) reclaim caches for
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

use super::fingerprint::git_worktree_fingerprint;
use super::{CheckRequest, CheckState, Checker, Language, ProblemSnapshot, UnavailableReason};
use crate::execution::seatbelt::ReadDeny;

/// Name of the marker file written in each worktree-level cache directory, recording the
/// worktree's canonical path so [`sweep_stale_caches`] can find directories to remove.
const WORKTREE_MARKER_FILE_NAME: &str = "worktree.path";

/// Fingerprint of one worktree's check-relevant inputs: `Some(hash)` when the inputs could be
/// fingerprinted cheaply, `None` when unknown (the scheduler then assumes "changed" and runs).
pub type FingerprintFn = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Outcome of the copy-on-write sibling-cache clone attempted before a worktree's first check of a
/// language whose checks seed one (see
/// [`LanguageChecks::sibling_cache`](crate::checks::LanguageChecks::sibling_cache)).
///
/// Exposed so tests can observe the clone decision without depending on filesystem timing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CacheClone {
    /// No check of the language has reached the clone decision for this worktree yet.
    NotAttempted,
    /// `cp -c -R` completed successfully; the destination cache subdirectory was populated from
    /// the sibling's.
    Cloned,
    /// No sibling worktree of the same repository had a completed cache of the language to
    /// clone from.
    SkippedNoSource,
    /// A source subdirectory existed but the clone process failed; the check proceeds cold.
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
    /// Optional observer called for completions accepted under the current policy generation.
    on_complete: Option<CompletionHook>,
    /// Worktree input fingerprint consulted when a debounce fires for a pair whose last
    /// completed run was `Ready` (T20B); always called off the state lock.
    fingerprint: FingerprintFn,
    /// Test barrier after checker completion and before publication takes the state lock.
    #[cfg(test)]
    before_publish: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Observer of completed check runs, installed through [`Scheduler::with_completion_hook`].
///
/// Called synchronously after the policy-fenced publication lock is released. It must return promptly and must
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
    /// Repository identity shared with sibling worktrees for sibling cache cloning.
    repository_key: String,
    /// Counter incremented on every [`Scheduler::trigger`] call for this worktree.
    input_generation: u64,
    /// `input_generation` at the most recent [`Scheduler::activate`]; a snapshot older than it
    /// predates the current session's activation.
    activation_generation: u64,
    /// Per-language state, created lazily on first trigger or debounce firing.
    languages: HashMap<Language, LanguageState>,
    /// `true` while the currently armed debounce timers were armed by [`Scheduler::activate`];
    /// their firings must run even when the worktree inputs are unchanged (T20B). Cleared by
    /// the next ordinary [`Scheduler::trigger`].
    activation_armed: bool,
    /// Outcome of this worktree's sibling-cache clone decision per language, for
    /// [`Scheduler::cache_clone_outcome`]; absent means not attempted.
    clone_outcomes: HashMap<Language, CacheClone>,
    /// Strongest exclusions observed for any check on this worktree.
    read_denies: Vec<ReadDeny>,
    /// Incremented when the effective exclusions change; stale completions cannot publish.
    policy_generation: u64,
    /// Stable digest partitioning persistent caches by the effective exclusion set.
    policy_digest: String,
}

impl WorktreeState {
    /// Builds the initial state for a worktree first seen by [`Scheduler::trigger`].
    fn new(repository_key: &str) -> Self {
        Self {
            repository_key: repository_key.to_string(),
            input_generation: 0,
            activation_generation: 0,
            languages: HashMap::new(),
            activation_armed: false,
            clone_outcomes: HashMap::new(),
            read_denies: Vec::new(),
            policy_generation: 0,
            policy_digest: policy_digest(&[]),
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
    /// Worktree fingerprint captured at the current (or most recent) run's start (T20B); moved
    /// into `completed_fingerprint` when that run completes, so an edit made mid-run is not
    /// mistaken for unchanged input.
    run_start_fingerprint: Option<u64>,
    /// Fingerprint the last completed run started with (T20B); the skip comparison baseline.
    completed_fingerprint: Option<u64>,
    /// `true` when an edit reply is waiting for the next run: that run skips unchanged-input
    /// elision and the EYES-r2 cooldown so it checks the requested post-edit source.
    urgent: bool,
    /// `true` only when the last completed run ended `Ready` (T20B): any other outcome —
    /// `Partial`, `Checking`, or an `Unavailable` failure or condition — must be re-checked on
    /// the next trigger, so only a `Ready` completion arms the skip-unchanged rule.
    skip_eligible: bool,
}

/// Per-repository state shared across sibling worktrees.
#[derive(Default)]
struct RepositoryState {
    /// Latest completed seedable cache for each language and exact effective exclusion set.
    most_recent_cache_dir: HashMap<(Language, String), PathBuf>,
}

impl Scheduler {
    /// Retains host read exclusions before activation; later profiles cannot weaken them.
    /// A stronger policy cancels existing runs and cached snapshots for this worktree.
    pub fn add_read_denies(&self, worktree: &Path, denies: &[ReadDeny]) {
        let worktree = canonical_worktree(worktree);
        let mut state = self.inner.lock_state();
        let wt = state
            .worktrees
            .entry(worktree)
            .or_insert_with(|| WorktreeState::new(""));
        // ponytail: one union per worktree can underreport for a later wider host; use
        // per-binding check keys only if independent same-worktree policies are needed.
        let old_len = wt.read_denies.len();
        for deny in denies {
            if !wt.read_denies.contains(deny) {
                wt.read_denies.push(deny.clone());
            }
        }
        if wt.read_denies.len() != old_len {
            wt.input_generation += 1;
            wt.policy_generation += 1;
            wt.policy_digest = policy_digest(&wt.read_denies);
            wt.clone_outcomes.clear();
            for lang in wt.languages.values_mut() {
                if let Some(timer) = lang.timer_abort.take() {
                    timer.abort();
                }
                if let Some(run) = lang.run_abort.take() {
                    run.abort();
                }
                lang.running = false;
                lang.latest_snapshot = None;
                lang.completed_fingerprint = None;
                lang.skip_eligible = false;
            }
        }
    }
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
                fingerprint: Arc::new(git_worktree_fingerprint),
                #[cfg(test)]
                before_publish: None,
            }),
        }
    }

    /// Installs `fingerprint` as the worktree input fingerprint (T20B), replacing the
    /// git-based `git_worktree_fingerprint` default; used by tests to script unchanged and
    /// changed inputs (`None` = unknown = run).
    ///
    /// Must be called on the freshly built scheduler before it is cloned or triggered.
    ///
    /// # Panics
    ///
    /// Panics when another clone of this scheduler already exists.
    pub fn with_fingerprint(mut self, fingerprint: FingerprintFn) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("fingerprint function must be installed before the scheduler is shared")
            .fingerprint = fingerprint;
        self
    }

    /// Installs a test-only barrier at the exact completion/publication race boundary.
    #[cfg(test)]
    fn with_before_publish(mut self, hook: Arc<dyn Fn() + Send + Sync>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("new scheduler has one owner")
            .before_publish = Some(hook);
        self
    }

    /// Installs `hook` for every completion accepted under the current policy generation,
    /// including transient outcomes suppressed by the prior-ready snapshot guard.
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
    /// worktree belongs to, for cache cloning between sibling worktrees; it is refreshed on
    /// every call in case a worktree's repository identity changes. Increments this worktree's
    /// `input_generation` once regardless of how many languages are configured. A trigger
    /// received after [`Scheduler::shutdown`] has started is silently ignored.
    pub fn trigger(&self, repository_key: &str, worktree: &Path) {
        self.trigger_inner(repository_key, worktree, None, false, false);
    }

    /// Like [`Scheduler::trigger`], naming the changed file when the trigger knows it: only that
    /// file's language is re-armed, so a `.py` edit never starts a cargo check. A path no
    /// registered language owns (for example `Cargo.toml` or `README.md`) re-arms every
    /// configured language, matching a path-less trigger.
    pub fn trigger_for_path(&self, repository_key: &str, worktree: &Path, path: &Path) {
        self.trigger_inner(repository_key, worktree, Some(path), false, false);
    }

    /// Like [`Scheduler::trigger`], for a check a caller is waiting on (an edit reply): it forces
    /// a check of the requested inputs even when the unchanged-input fingerprint matches, and
    /// skips the cooldown after the previous run. `path` names the changed file when the caller
    /// knows it; see [`Scheduler::trigger_for_path`] for how it narrows the armed languages.
    pub fn trigger_urgent(&self, repository_key: &str, worktree: &Path, path: Option<&Path>) {
        self.trigger_inner(repository_key, worktree, path, false, true);
    }

    /// Like [`Scheduler::trigger`], for a session activating `worktree`: also records the new
    /// `input_generation` as the activation generation, so [`Scheduler::stale`] flags every
    /// snapshot produced before this activation until a check completed after it replaces it.
    pub fn activate(&self, repository_key: &str, worktree: &Path) {
        self.trigger_inner(repository_key, worktree, None, true, false);
    }

    /// Cancels this worktree's pending and running checks when its caller loses whole-tree read
    /// authority. No cached result is deleted; the caller's feed separately hides it.
    pub fn cancel_worktree(&self, worktree: &Path) {
        let worktree = canonical_worktree(worktree);
        let mut state = self.inner.lock_state();
        if let Some(wt) = state.worktrees.get_mut(&worktree) {
            wt.input_generation += 1;
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

    /// Shared body of the trigger entry points. `path` names the changed file when the caller
    /// knows it: only that file's language is re-armed, so one language's edit never starts
    /// another language's check. A path whose language is registered but has no configured
    /// checker arms nothing (the all-language path would not check it either); a path no
    /// registered language owns — `Cargo.toml`, `README.md` — and a path-less trigger (Bash,
    /// activation) keep the previous every-configured-language behaviour.
    fn trigger_inner(
        &self,
        repository_key: &str,
        worktree: &Path,
        path: Option<&Path>,
        activation: bool,
        urgent: bool,
    ) {
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
        wt.activation_armed = activation;
        if activation {
            wt.activation_generation = wt.input_generation;
        }
        let armed: Vec<Language> = match path.and_then(Language::for_path) {
            Some(language) if self.inner.checkers.contains_key(&language) => vec![language],
            Some(_) => Vec::new(),
            None => self.inner.checkers.keys().copied().collect(),
        };
        for language in armed {
            let lang = wt.languages.entry(language).or_default();
            if urgent {
                lang.urgent = true;
            }
            if activation {
                // An ordinary trigger inside the debounce window clears `activation_armed`; the
                // dropped baseline keeps the session's first check from being skipped anyway.
                lang.skip_eligible = false;
                lang.completed_fingerprint = None;
            }
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

    /// Returns the languages whose stored snapshot for `worktree` predates the latest
    /// [`Scheduler::activate`] while a debounce timer is pending or a check is running, in
    /// language order.
    ///
    /// Such a snapshot may describe another session's inputs; the pending or running check will
    /// replace it. Ordinary [`Scheduler::trigger`]s do not make a snapshot stale: within a session
    /// the last completed snapshot stays current until a newer one lands. Non-blocking and
    /// synchronous.
    pub fn stale(&self, worktree: &Path) -> Vec<Language> {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        let mut stale: Vec<Language> = state
            .worktrees
            .get(&worktree)
            .map(|wt| {
                wt.languages
                    .iter()
                    .filter(|(_, lang)| {
                        (lang.running || lang.timer_abort.is_some())
                            && lang.latest_snapshot.as_ref().is_some_and(|snapshot| {
                                snapshot.input_generation < wt.activation_generation
                            })
                    })
                    .map(|(language, _)| *language)
                    .collect()
            })
            .unwrap_or_default();
        stale.sort();
        stale
    }

    /// Returns the languages of `worktree` with a check running right now, in language order.
    ///
    /// Returns `worktree`'s current input generation: the generation a check started after the
    /// most recent trigger reports in its snapshot. `0` for an unknown worktree.
    pub fn generation(&self, worktree: &Path) -> u64 {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        state
            .worktrees
            .get(&worktree)
            .map_or(0, |wt| wt.input_generation)
    }

    /// Only a started check counts, not an armed debounce timer: a trigger that changes nothing
    /// (a no-op tool) then never flips the status before its check actually runs (T18B).
    /// Non-blocking and synchronous.
    pub fn running(&self, worktree: &Path) -> Vec<Language> {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        let mut running: Vec<Language> = state
            .worktrees
            .get(&worktree)
            .map(|wt| {
                wt.languages
                    .iter()
                    .filter(|(_, lang)| lang.running)
                    .map(|(language, _)| *language)
                    .collect()
            })
            .unwrap_or_default();
        running.sort();
        running
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

    /// Returns the outcome of `worktree`'s sibling-cache clone decision for `language`.
    ///
    /// [`CacheClone::NotAttempted`] until this worktree's first check of `language` has reached
    /// the clone decision point; a debug-visible field kept mainly for tests.
    pub fn cache_clone_outcome(&self, worktree: &Path, language: Language) -> CacheClone {
        let worktree = canonical_worktree(worktree);
        let state = self.inner.lock_state();
        state
            .worktrees
            .get(&worktree)
            .and_then(|wt| wt.clone_outcomes.get(&language).copied())
            .unwrap_or(CacheClone::NotAttempted)
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
    /// Returns the host exclusions retained for a worktree without touching its files.
    fn read_denies(&self, worktree: &Path) -> Vec<ReadDeny> {
        self.lock_state()
            .worktrees
            .get(worktree)
            .map_or_else(Vec::new, |wt| wt.read_denies.clone())
    }

    /// Captures one worktree's policy generation and persistent-cache partition together.
    fn policy(&self, worktree: &Path) -> (u64, String) {
        self.lock_state().worktrees.get(worktree).map_or_else(
            || (0, policy_digest(&[])),
            |wt| (wt.policy_generation, wt.policy_digest.clone()),
        )
    }

    /// Captures request inputs only while the policy prepared for this run is still current.
    fn check_inputs(
        &self,
        worktree: &Path,
        policy_generation: u64,
    ) -> Option<(u64, Vec<ReadDeny>)> {
        let state = self.lock_state();
        let wt = state.worktrees.get(worktree)?;
        (wt.policy_generation == policy_generation)
            .then(|| (wt.input_generation, wt.read_denies.clone()))
    }
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
    /// this pair, marks it dirty and returns. Otherwise (T20B) the worktree input fingerprint is
    /// computed *off* the state lock, and the run is skipped entirely when it equals the
    /// fingerprint the pair's last completed `Ready` run started with and the timers were not
    /// armed by an activation: no checker process, no `running` flag, no snapshot, no completion
    /// hook. During the fingerprint computation the pair is momentarily neither running nor
    /// timer-armed, so `is_busy` can briefly report idle; a trigger arriving in that window
    /// re-arms a timer whose firing owns the decision instead (this firing sees the newer timer
    /// or run on re-locking and stands down).
    async fn on_debounce_fire(inner: Arc<Self>, worktree: PathBuf, language: Language) {
        let (force_run, skip_eligible, completed_fingerprint, generation) = {
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
                return;
            }
            (
                wt.activation_armed || lang.urgent,
                lang.skip_eligible,
                lang.completed_fingerprint,
                wt.input_generation,
            )
        };
        // Only a fingerprint that could actually cause a skip is worth computing; git may take
        // up to its own budget, so this never happens under the state lock.
        let fingerprint = if !force_run && skip_eligible {
            inner.fingerprint_value(&worktree).await
        } else {
            None
        };
        {
            let mut state = inner.lock_state();
            if state.shutting_down {
                return;
            }
            let Some(wt) = state.worktrees.get_mut(&worktree) else {
                return;
            };
            if wt.input_generation != generation {
                return;
            }
            if wt
                .languages
                .get(&language)
                .is_some_and(|lang| lang.running || lang.timer_abort.is_some())
            {
                // A trigger since the fingerprint was taken re-armed a timer or started a run;
                // that newer decision supersedes this firing.
                return;
            }
            let Some(lang) = wt.languages.get_mut(&language) else {
                return;
            };
            if !force_run && fingerprint.is_some() && completed_fingerprint == fingerprint {
                // Inputs unchanged since the retained result's run began, so that result is
                // current for this generation too: a waiter keyed on the generation (an edit
                // reply) must see it instead of waiting for a run that never starts.
                if let Some(snapshot) = lang.latest_snapshot.as_mut() {
                    snapshot.input_generation = generation;
                    lang.last_stored_generation = generation;
                }
                return;
            } else {
                lang.running = true;
            }
            // Spawn and publish the abort handle under the same lock: a concurrent restriction
            // can then cancel the task before its first source stat or checker dispatch.
            let handle = tokio::spawn(Inner::run_check_loop(
                Arc::clone(&inner),
                worktree.clone(),
                language,
            ));
            lang.run_abort = Some(handle.abort_handle());
        }
    }

    /// Drives one or more sequential check runs for `(worktree, language)` until no rerun is
    /// pending.
    ///
    /// Each iteration first re-evaluates [`Language::is_present`] for `worktree` (T10B): an
    /// absent language spawns no confined process, creates no cache directory, and reports no
    /// telemetry, it only stores an `Unavailable(Disabled)` snapshot so the feed and `ide.context`
    /// omit it. Presence is re-checked on every iteration rather than cached, so a worktree that
    /// gains its manifest between triggers is checked again on the next one. Otherwise, this
    /// waits out any pending EYES-r2 cooldown (see [`Inner::cooldown_remaining`]), then prepares
    /// the cache directory (cloning a sibling's cache subdirectory on the worktree's first check
    /// of a seeding language when possible), acquires the shared concurrency permit, dispatches the configured [`Checker`],
    /// stores the resulting snapshot only if its policy generation is current (and subject to
    /// the Fatal/Timeout guard), then reports accepted completion to [`CompletionHook`],
    /// records this completion's timing for the next iteration's cooldown, and records a seeding
    /// language's cache completion for sibling worktrees. If the pair was marked dirty while this run was
    /// in flight, one more iteration follows with the latest `input_generation`; the [`Checker`]
    /// contract's cancellation is dropping its future, which happens automatically when
    /// [`Scheduler::shutdown`] aborts this task mid-await (including while it is waiting out a
    /// cooldown).
    async fn run_check_loop(inner: Arc<Self>, worktree: PathBuf, language: Language) {
        let mut dirty_rerun = false;
        loop {
            let (policy_generation, policy_digest) = inner.policy(&worktree);
            if inner.is_shutting_down() {
                inner.finish_run(&worktree, language, false, policy_generation);
                return;
            }
            if !language.is_present(&worktree) {
                let generation = inner.current_generation(&worktree);
                let snapshot =
                    ProblemSnapshot::unavailable(language, UnavailableReason::Disabled, generation);
                if !inner.store_snapshot(&worktree, snapshot, policy_generation) {
                    return;
                }
                inner.mark_run_ineligible(&worktree, language, policy_generation);
                dirty_rerun = inner.finish_run(&worktree, language, true, policy_generation);
                if !dirty_rerun {
                    return;
                }
                continue;
            }
            // (T28B) A dirty follow-up run obeys the same skip-unchanged rule as a debounce
            // firing (T20B): reply-delivered hosts trigger on every `ide.*` call, so triggers
            // arrive while a check runs even though nothing changed, and the follow-up must
            // then not spawn a checker process at all.
            if dirty_rerun && inner.rerun_skip_candidate(&worktree, language) {
                let fingerprint = inner.fingerprint_value(&worktree).await;
                if inner.skip_dirty_rerun(&worktree, language, fingerprint, policy_generation) {
                    inner.finish_run(&worktree, language, false, policy_generation);
                    return;
                }
            }
            let cooldown = inner.cooldown_remaining(&worktree, language);
            if !cooldown.is_zero() {
                tokio::time::sleep(cooldown).await;
            }
            if inner.is_shutting_down() {
                inner.finish_run(&worktree, language, false, policy_generation);
                return;
            }
            let cache_dir = inner
                .prepare_cache_dir(&worktree, language, &policy_digest, policy_generation)
                .await;
            let permit = match Arc::clone(&inner.semaphore).acquire_owned().await {
                Ok(permit) => permit,
                Err(_closed) => {
                    inner.finish_run(&worktree, language, false, policy_generation);
                    return;
                }
            };
            if inner.is_shutting_down() {
                drop(permit);
                inner.finish_run(&worktree, language, false, policy_generation);
                return;
            }
            let Some(checker) = inner.checkers.get(&language).cloned() else {
                drop(permit);
                inner.finish_run(&worktree, language, false, policy_generation);
                return;
            };
            let Some((generation, read_denies)) = inner.check_inputs(&worktree, policy_generation)
            else {
                return;
            };
            let request = CheckRequest {
                worktree: worktree.clone(),
                cache_dir: cache_dir.clone(),
                input_generation: generation,
                read_denies,
            };
            // Fingerprint at the START of the run (T20B), so an edit made while the check is in
            // flight is recorded as the completed run's baseline only if it happened before the
            // run began; computed off the state lock, just before dispatch.
            let run_start_fingerprint = inner.fingerprint_value(&worktree).await;
            {
                let mut state = inner.lock_state();
                if let Some(lang) = state
                    .worktrees
                    .get_mut(&worktree)
                    .filter(|wt| wt.policy_generation == policy_generation)
                    .and_then(|wt| wt.languages.get_mut(&language))
                {
                    lang.run_start_fingerprint = run_start_fingerprint;
                }
            }
            crate::errorlog::record(
                crate::errorlog::Method::Check,
                crate::errorlog::Outcome::Started,
                crate::errorlog::Fields {
                    worktree: Some(&worktree),
                    detail: Some(language.as_str()),
                    ..Default::default()
                },
            );
            let started = Instant::now();
            let read_denies = request.read_denies.clone();
            let mut snapshot = checker.check(request).await;
            filter_denied_problems(&mut snapshot, &worktree, &read_denies);
            let duration = started.elapsed();
            drop(permit);
            #[cfg(test)]
            if let Some(before_publish) = &inner.before_publish {
                before_publish();
            }
            let completed_state = snapshot.state.clone();
            if !inner.store_snapshot(&worktree, snapshot.clone(), policy_generation) {
                return;
            }
            inner.record_completion(
                &worktree,
                language,
                duration,
                &completed_state,
                policy_generation,
            );
            if language
                .checks()
                .is_some_and(|checks| checks.sibling_cache().is_some())
            {
                inner.record_completed_cache(
                    &worktree,
                    language,
                    &cache_dir,
                    policy_generation,
                    &policy_digest,
                );
            }
            if let Some(hook) = &inner.on_complete {
                hook(&snapshot);
            }
            if !inner.finish_run(&worktree, language, true, policy_generation) {
                return;
            }
            dirty_rerun = true;
        }
    }

    /// Reports whether a dirty follow-up run for this pair can still be skipped by the T20B
    /// fingerprint comparison: the baseline exists and was armed by a completed `Ready` run;
    /// urgent edit checks always execute for their exact post-edit source.
    fn rerun_skip_candidate(&self, worktree: &Path, language: Language) -> bool {
        let state = self.lock_state();
        state
            .worktrees
            .get(worktree)
            .and_then(|wt| wt.languages.get(&language))
            .is_some_and(|lang| {
                !lang.urgent && lang.skip_eligible && lang.completed_fingerprint.is_some()
            })
    }

    /// Applies the T20B skip to a dirty follow-up run (T28B): returns `true` when the current
    /// worktree input fingerprint still equals the last completed `Ready` run's baseline, in
    /// which case the pair stops running — no checker process, the stored snapshot stays
    /// current — and a later changed-input trigger starts a fresh run. A changed policy
    /// generation leaves the newer policy's run state untouched.
    fn skip_dirty_rerun(
        &self,
        worktree: &Path,
        language: Language,
        fingerprint: Option<u64>,
        policy_generation: u64,
    ) -> bool {
        let mut state = self.lock_state();
        let Some(wt) = state
            .worktrees
            .get_mut(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
        else {
            return false;
        };
        let generation = wt.input_generation;
        let Some(lang) = wt.languages.get_mut(&language) else {
            return false;
        };
        let skip = lang.skip_eligible
            && fingerprint.is_some()
            && lang.completed_fingerprint == fingerprint;
        if skip {
            // Same rule as the debounce skip: the retained result is current for this generation.
            if let Some(snapshot) = lang.latest_snapshot.as_mut() {
                snapshot.input_generation = generation;
                lang.last_stored_generation = generation;
            }
            lang.running = false;
            lang.dirty = false;
            lang.run_abort = None;
        }
        skip
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

    /// Stores `snapshot` as the latest result for its `(worktree, language)` pair only while
    /// `policy_generation` still matches under this publication lock. Returns `false` for a
    /// stale policy and `true` for a current completion, including transient suppression. A
    /// newer completion (by `input_generation`) is retained; a transient `Fatal`/`Timeout`
    /// failure (EYES-r2 §5) does not overwrite an existing usable `Ready`/`Partial` result.
    /// Every other `Unavailable` reason still replaces the stored
    /// result, since those describe a durable condition (disabled, outside roots, tool/env
    /// missing, or T12B's `NoFiles`, a project misconfiguration that stays true run after run)
    /// rather than one bad run.
    fn store_snapshot(
        &self,
        worktree: &Path,
        snapshot: ProblemSnapshot,
        policy_generation: u64,
    ) -> bool {
        let mut state = self.lock_state();
        let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
            .and_then(|wt| wt.languages.get_mut(&snapshot.language))
        else {
            return false;
        };
        if is_transient_failure(&snapshot.state)
            && matches!(
                lang.latest_snapshot
                    .as_ref()
                    .map(|existing| &existing.state),
                Some(CheckState::Ready) | Some(CheckState::Partial)
            )
        {
            return true;
        }
        if lang.latest_snapshot.is_none()
            || snapshot.input_generation >= lang.last_stored_generation
        {
            lang.last_stored_generation = snapshot.input_generation;
            lang.latest_snapshot = Some(snapshot);
        }
        true
    }

    /// Records `duration` as the wall-clock time `(worktree, language)`'s most recently
    /// completed run took, together with the completion instant, for the next iteration's
    /// [`Inner::cooldown_remaining`] check, and moves that run's start fingerprint into the
    /// skip-unchanged baseline (T20B). Only a `Ready` completion arms the skip rule: a `Partial`
    /// result still has incomplete coverage and every `Unavailable` outcome — transient failure
    /// or durable condition — must be re-checked on the next trigger, so any non-`Ready` state
    /// (and the stale baseline with it) is dropped here. A changed policy generation rejects
    /// this bookkeeping update.
    fn record_completion(
        &self,
        worktree: &Path,
        language: Language,
        duration: Duration,
        completed_state: &CheckState,
        policy_generation: u64,
    ) {
        let mut state = self.lock_state();
        if let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
            .and_then(|wt| wt.languages.get_mut(&language))
        {
            lang.last_completion = Some(Instant::now());
            lang.last_duration = duration;
            lang.completed_fingerprint = lang.run_start_fingerprint.take();
            lang.skip_eligible = matches!(completed_state, CheckState::Ready);
        }
    }

    /// Drops the skip-unchanged baseline for a `(worktree, language)` pair whose latest run
    /// completed without dispatching a checker (T20B: a language found absent stores
    /// `Unavailable(Disabled)` directly); the next trigger must not compare against an
    /// out-of-date baseline. A changed policy generation leaves newer bookkeeping untouched.
    fn mark_run_ineligible(&self, worktree: &Path, language: Language, policy_generation: u64) {
        let mut state = self.lock_state();
        if let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
            .and_then(|wt| wt.languages.get_mut(&language))
        {
            lang.run_start_fingerprint = None;
            lang.completed_fingerprint = None;
            lang.skip_eligible = false;
        }
    }

    /// Computes `worktree`'s input fingerprint through [`Inner::fingerprint`], off the state
    /// lock, on the blocking thread pool (the git-based implementation spawns a process and may
    /// take up to its own budget). A panicking or cancelled computation is `None`: unknown, run.
    async fn fingerprint_value(&self, worktree: &Path) -> Option<u64> {
        if !self.read_denies(worktree).is_empty() {
            return None;
        }
        let fingerprint = Arc::clone(&self.fingerprint);
        let worktree = worktree.to_path_buf();
        tokio::task::spawn_blocking(move || fingerprint(&worktree))
            .await
            .unwrap_or(None)
    }

    /// Computes how much longer `(worktree, language)` must wait before its next run starts
    /// (EYES-r2 §5): no earlier than `max(debounce, previous run duration)` after the previous
    /// completion. Returns [`Duration::ZERO`] before any run has ever completed, or once that
    /// interval has already elapsed.
    fn cooldown_remaining(&self, worktree: &Path, language: Language) -> Duration {
        let mut state = self.lock_state();
        let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .and_then(|wt| wt.languages.get_mut(&language))
        else {
            return Duration::ZERO;
        };
        if std::mem::take(&mut lang.urgent) {
            return Duration::ZERO;
        }
        let Some(last_completion) = lang.last_completion else {
            return Duration::ZERO;
        };
        let required = self.debounce.max(lang.last_duration);
        required.saturating_sub(last_completion.elapsed())
    }

    /// Records `cache_dir` for sibling clones of `language` only under the same live policy
    /// generation and digest; stale completions cannot seed a newer policy's cache.
    fn record_completed_cache(
        &self,
        worktree: &Path,
        language: Language,
        cache_dir: &Path,
        policy_generation: u64,
        policy_digest: &str,
    ) {
        let mut state = self.lock_state();
        let Some(repository_key) = state
            .worktrees
            .get(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
            .map(|wt| wt.repository_key.clone())
        else {
            return;
        };
        state
            .repositories
            .entry(repository_key)
            .or_default()
            .most_recent_cache_dir
            .insert(
                (language, policy_digest.to_owned()),
                cache_dir.to_path_buf(),
            );
    }

    /// Ends the current iteration only for its policy generation and reports whether another
    /// iteration should follow; an older task cannot clear a stricter policy's run state.
    ///
    /// Consumes a pending `dirty` flag into one more iteration when `allow_rerun` is set and the
    /// scheduler is not shutting down; otherwise clears `running`, `dirty`, and `run_abort` so the
    /// pair is idle again.
    fn finish_run(
        &self,
        worktree: &Path,
        language: Language,
        allow_rerun: bool,
        policy_generation: u64,
    ) -> bool {
        let mut state = self.lock_state();
        let shutting_down = state.shutting_down;
        let Some(lang) = state
            .worktrees
            .get_mut(worktree)
            .filter(|wt| wt.policy_generation == policy_generation)
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

    /// Creates `worktree`'s `language` cache directory if it is missing, attempting a sibling
    /// cache clone beforehand when this is the worktree's first check of a language whose checks
    /// seed one.
    ///
    /// The directory layout is `<cache_root>/<hash(repository_key)>/<hash(canonical
    /// worktree)>/<policy_digest>/<language>`, so caches from different policies never collide
    /// even if two unrelated worktrees hash to the same worktree-level segment by coincidence of
    /// path reuse. The first time a worktree's cache directory is created, a
    /// [`WORKTREE_MARKER_FILE_NAME`] file recording its canonical path is written alongside it,
    /// for [`sweep_stale_caches`] to later identify caches whose worktree no longer exists.
    async fn prepare_cache_dir(
        &self,
        worktree: &Path,
        language: Language,
        policy_digest: &str,
        policy_generation: u64,
    ) -> PathBuf {
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
        let dir = worktree_dir.join(policy_digest).join(language.as_str());
        let existed = dir.exists();
        if !existed && let Err(error) = create_private_dir(&dir) {
            eprintln!(
                "agent-ide: scheduler failed to create cache dir {}: {error}",
                dir.display()
            );
        }
        if let Some(subdirectory) = language.checks().and_then(|checks| checks.sibling_cache())
            && !existed
        {
            let outcome = self
                .try_clone_cache(worktree, language, subdirectory, &dir, policy_digest)
                .await;
            let mut state = self.lock_state();
            if let Some(wt) = state
                .worktrees
                .get_mut(worktree)
                .filter(|wt| wt.policy_generation == policy_generation)
            {
                wt.clone_outcomes.insert(language, outcome);
            }
        }
        dir
    }

    /// Attempts an APFS copy-on-write clone of a sibling worktree's `subdirectory` of its
    /// `language` cache into `dst_dir`.
    ///
    /// Looks up the most recently completed cache of `language` for this repository and exact
    /// deny-policy digest; if none exists, or its `subdirectory` is missing, the check proceeds
    /// cold. The clone itself runs `/bin/cp -c -R <source>/<subdirectory> <dst_dir>/<subdirectory>`;
    /// any failure is logged and treated as a cold start rather than propagated.
    async fn try_clone_cache(
        &self,
        worktree: &Path,
        language: Language,
        subdirectory: &str,
        dst_dir: &Path,
        policy_digest: &str,
    ) -> CacheClone {
        let repository_key = {
            let state = self.lock_state();
            state
                .worktrees
                .get(worktree)
                .map(|wt| wt.repository_key.clone())
        };
        let Some(repository_key) = repository_key else {
            return CacheClone::NotAttempted;
        };
        let source_dir = {
            let state = self.lock_state();
            state
                .repositories
                .get(&repository_key)
                .and_then(|repository| {
                    repository
                        .most_recent_cache_dir
                        .get(&(language, policy_digest.to_owned()))
                        .cloned()
                })
        };
        let Some(source_dir) = source_dir else {
            return CacheClone::SkippedNoSource;
        };
        let source_target = source_dir.join(subdirectory);
        if !source_target.exists() {
            return CacheClone::SkippedNoSource;
        }
        let destination_target = dst_dir.join(subdirectory);
        match tokio::process::Command::new("/bin/cp")
            .arg("-c")
            .arg("-R")
            .arg(&source_target)
            .arg(&destination_target)
            .output()
            .await
        {
            Ok(output) if output.status.success() => CacheClone::Cloned,
            Ok(_) | Err(_) => {
                crate::errorlog::record(
                    crate::errorlog::Method::Check,
                    crate::errorlog::Outcome::Failed,
                    crate::errorlog::Fields {
                        reason: Some(crate::errorlog::ReasonCode::SchedulerCacheCloneFailed),
                        ..Default::default()
                    },
                );
                CacheClone::Failed
            }
        }
    }
}

/// Removes denied or malformed diagnostic paths before a result enters the shared cache.
/// Counts fall back to retained allowed paths only if a checker failed to filter before its cap.
fn filter_denied_problems(snapshot: &mut ProblemSnapshot, worktree: &Path, denies: &[ReadDeny]) {
    if denies.is_empty() {
        return;
    }
    let before = snapshot.problems.len();
    snapshot
        .problems
        .retain(|problem| super::check_problem_path_allowed(worktree, &problem.path, denies));
    if snapshot.problems.len() != before {
        snapshot.errors = snapshot
            .problems
            .iter()
            .filter(|problem| problem.severity == super::Severity::Error)
            .count() as u32;
        snapshot.warnings = snapshot
            .problems
            .iter()
            .filter(|problem| problem.severity == super::Severity::Warning)
            .count() as u32;
        snapshot.truncated = false;
    }
    snapshot.detail = None;
}

/// Resolves a worktree's stable key, retaining its supplied path if resolution fails.
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

/// Hashes the sorted effective deny set for stable, policy-isolated persistent cache paths.
fn policy_digest(denies: &[ReadDeny]) -> String {
    let mut rules = denies
        .iter()
        .map(|deny| serde_json::to_vec(deny).expect("validated read deny serializes"))
        .collect::<Vec<_>>();
    rules.sort_unstable();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"agent-ide/check-policy/v1\0");
    for rule in rules {
        hasher.update(&(rule.len() as u64).to_le_bytes());
        hasher.update(&rule);
    }
    hasher.finalize().to_hex().to_string()
}

/// Removes cache directories under `cache_root` whose worktree no longer exists on disk
/// (EYES-r2 §5 follow-up), for a daemon to call periodically outside any live [`Scheduler`].
///
/// Walks `<cache_root>/<repository hash>/<worktree hash>/`, reading each worktree-level
/// directory's `WORKTREE_MARKER_FILE_NAME` to recover the worktree path it was created for; a
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

#[cfg(test)]
mod deny_tests {
    use super::*;
    use crate::checks::{FakeChecker, Problem, Severity};
    use crate::execution::seatbelt::CredentialGlob;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An edit-triggered check must run when the prior ready result has the same cheap fingerprint:
    /// carrying its warning would falsely report `current_reported`, and carrying its clean result
    /// would falsely report `current_clean` for the edited source.
    #[tokio::test]
    async fn urgent_edit_checks_do_not_promote_prior_generation_results() {
        for (old_problem, current_problem) in
            [(Some("removed warning"), None), (None, Some("new warning"))]
        {
            let root = std::env::temp_dir().join(format!(
                "agent-ide-urgent-check-{}-{}",
                std::process::id(),
                old_problem.is_some()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(root).unwrap();
            std::fs::write(root.join("beta.toml"), "").unwrap();
            let old = ProblemSnapshot::from_problems(
                crate::lang::testing::BETA,
                CheckState::Ready,
                old_problem
                    .map(|message| {
                        vec![Problem::new(
                            "src/lib.rs".into(),
                            1,
                            1,
                            Severity::Warning,
                            Some("dead_code".into()),
                            message.into(),
                        )]
                    })
                    .unwrap_or_default(),
                1,
                1,
            );
            let current = ProblemSnapshot::from_problems(
                crate::lang::testing::BETA,
                CheckState::Ready,
                current_problem
                    .map(|message| {
                        vec![Problem::new(
                            "src/lib.rs".into(),
                            1,
                            1,
                            Severity::Warning,
                            Some("dead_code".into()),
                            message.into(),
                        )]
                    })
                    .unwrap_or_default(),
                2,
                1,
            );
            let checker = Arc::new(FakeChecker::new(crate::lang::testing::BETA, current));
            let fingerprint_calls = Arc::new(AtomicUsize::new(0));
            let scheduler = Scheduler::new(
                vec![checker.clone()],
                Duration::from_millis(1),
                1,
                root.join("cache"),
            )
            .with_fingerprint({
                let fingerprint_calls = Arc::clone(&fingerprint_calls);
                Arc::new(move |_| {
                    fingerprint_calls.fetch_add(1, Ordering::SeqCst);
                    Some(7)
                })
            });
            assert!(crate::lang::testing::BETA.is_present(&root));
            {
                let mut worktree = WorktreeState::new("repo");
                worktree.input_generation = 1;
                let language = LanguageState {
                    latest_snapshot: Some(old),
                    last_stored_generation: 1,
                    completed_fingerprint: Some(7),
                    skip_eligible: true,
                    ..Default::default()
                };
                worktree
                    .languages
                    .insert(crate::lang::testing::BETA, language);
                scheduler
                    .inner
                    .lock_state()
                    .worktrees
                    .insert(root.clone(), worktree);
            }

            scheduler.trigger_urgent("repo", &root, None);
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if checker.requests().len() == 1
                        && scheduler
                            .latest(&root)
                            .first()
                            .is_some_and(|snapshot| snapshot.input_generation == 2)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("edit-triggered check must replace the earlier generation");
            assert_eq!(fingerprint_calls.load(Ordering::SeqCst), 1);
            let snapshot = scheduler.latest(&root).remove(0);
            assert_eq!(
                snapshot.problems.len(),
                usize::from(current_problem.is_some())
            );
            if let Some(message) = current_problem {
                assert_eq!(snapshot.problems[0].message, message);
            }
            scheduler.shutdown().await;
            let _ = std::fs::remove_dir_all(root);
        }
    }

    /// A trigger that names the changed file re-arms only that file's language: a `.beta` edit
    /// never starts the alpha check. A path no registered language owns (`Cargo.toml`) and a
    /// path-less trigger (`Bash`, activation) keep arming every configured language.
    #[tokio::test]
    async fn path_trigger_arms_only_the_changed_files_language() {
        crate::lang::testing::install();
        let root =
            std::env::temp_dir().join(format!("agent-ide-path-trigger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(root.join("alpha.toml"), "").unwrap();
        std::fs::write(root.join("beta.toml"), "").unwrap();
        let ready = |language| {
            ProblemSnapshot::from_problems(language, CheckState::Ready, Vec::new(), 1, 0)
        };
        let alpha = Arc::new(FakeChecker::new(
            crate::lang::testing::ALPHA,
            ready(crate::lang::testing::ALPHA),
        ));
        let beta = Arc::new(FakeChecker::new(
            crate::lang::testing::BETA,
            ready(crate::lang::testing::BETA),
        ));
        let scheduler = Scheduler::new(
            vec![alpha.clone(), beta.clone()],
            Duration::from_millis(1),
            1,
            root.join("cache"),
        );

        scheduler.trigger_for_path("repo", &root, Path::new("src/module.beta"));
        tokio::time::timeout(Duration::from_secs(5), async {
            while beta.requests().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the changed file's language is checked");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            alpha.requests().is_empty(),
            "a .beta edit must not start the alpha check"
        );

        for (label, path) in [
            ("unowned extension", Some(Path::new("Cargo.toml"))),
            ("path-less trigger", None),
        ] {
            let (alpha_before, beta_before) = (alpha.requests().len(), beta.requests().len());
            match path {
                Some(path) => scheduler.trigger_for_path("repo", &root, path),
                None => scheduler.trigger("repo", &root),
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                while alpha.requests().len() <= alpha_before || beta.requests().len() <= beta_before
                {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap_or_else(|_| panic!("{label} arms every configured language"));
        }

        scheduler.shutdown().await;
        let _ = std::fs::remove_dir_all(root);
    }

    /// A check already running when an edit arrives must also run its dirty follow-up even when
    /// the cheap fingerprint matches the pre-edit baseline.
    #[test]
    fn urgent_dirty_follow_up_cannot_skip_on_matching_fingerprint() {
        let root = PathBuf::from("/tmp/agent-ide-urgent-dirty-rerun");
        let scheduler = Scheduler::new(Vec::new(), Duration::ZERO, 1, PathBuf::new());
        let mut worktree = WorktreeState::new("repo");
        let mut language = LanguageState {
            running: true,
            dirty: true,
            completed_fingerprint: Some(7),
            skip_eligible: true,
            urgent: true,
            ..Default::default()
        };
        language.latest_snapshot = Some(ProblemSnapshot::checking(crate::lang::testing::BETA, 1));
        worktree
            .languages
            .insert(crate::lang::testing::BETA, language);
        scheduler
            .inner
            .lock_state()
            .worktrees
            .insert(root.clone(), worktree);

        assert!(
            !scheduler
                .inner
                .rerun_skip_candidate(&root, crate::lang::testing::BETA)
        );
    }

    /// A check result cannot disclose a denied path or count it in its cached plate.
    #[test]
    fn denied_check_diagnostics_are_removed_before_caching() {
        let root = Path::new("/tmp/check-project");
        let mut snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::BETA,
            CheckState::Ready,
            vec![
                Problem::new(
                    "good.src".into(),
                    1,
                    1,
                    Severity::Error,
                    None,
                    "good".into(),
                ),
                Problem::new(
                    "secret.key".into(),
                    1,
                    1,
                    Severity::Error,
                    None,
                    "hidden".into(),
                ),
            ],
            1,
            1,
        );
        filter_denied_problems(
            &mut snapshot,
            root,
            &[ReadDeny::Glob {
                base: root.to_path_buf(),
                suffix: CredentialGlob::Key,
            }],
        );
        assert_eq!(snapshot.errors, 1);
        assert_eq!(snapshot.problems.len(), 1);
        assert_eq!(snapshot.problems[0].path, "good.src");
    }

    /// A deny-bearing worktree never invokes even an installed fingerprint callback.
    #[tokio::test]
    async fn deny_glob_skips_worktree_fingerprint() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-deny-fingerprint-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let scheduler = Scheduler::new(Vec::new(), Duration::from_millis(1), 1, root.join("cache"))
            .with_fingerprint(Arc::new(|_| panic!("denied worktree was fingerprinted")));
        scheduler.add_read_denies(
            &root,
            &[ReadDeny::Glob {
                base: root.clone(),
                suffix: CredentialGlob::Key,
            }],
        );
        assert_eq!(scheduler.inner.fingerprint_value(&root).await, None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A completion already past its checker cannot refill the cache after a stricter policy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn policy_change_drops_completion_waiting_at_publication() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-policy-barrier-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        std::fs::write(root.join("beta.toml"), "").unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let completed = Arc::new(AtomicUsize::new(0));
        let scheduler = Scheduler::new(
            vec![Arc::new(FakeChecker::with_delay(
                crate::lang::testing::BETA,
                ProblemSnapshot::checking(crate::lang::testing::BETA, 1),
                Duration::ZERO,
            ))],
            Duration::from_millis(1),
            1,
            root.join("cache"),
        )
        .with_before_publish({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move || {
                entered.wait();
                release.wait();
            })
        })
        .with_completion_hook({
            let completed = Arc::clone(&completed);
            Arc::new(move |_| {
                completed.fetch_add(1, Ordering::SeqCst);
            })
        });
        scheduler.activate("repo", &root);
        entered.wait();
        scheduler.add_read_denies(&root, &[ReadDeny::Path(root.join("secret.key"))]);
        release.wait();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(scheduler.latest(&root).is_empty());
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        scheduler.shutdown().await;
        let _ = std::fs::remove_dir_all(root);
    }
}
