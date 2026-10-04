//! Project problem feed for `ide.context` with `kind: "problems"` (EYES-r1 §7, EYES-r2).
//!
//! This module defines the confined [`ProblemSource`](crate::assistance::problems::ProblemSource) seam the daemon answers the problems kind
//! from, plus the deterministic compact page text. The source never runs a check; it only
//! reports the latest completed snapshots for one authorized worktree, and every rendered
//! textual field is treated as untrusted checker output: single line, control characters
//! stripped, never interpreted as markdown or markup.
//!
//! [`ProjectProblemFeed`](crate::assistance::problems::ProjectProblemFeed) is the daemon-owned
//! wiring behind that seam: it admits bound worktrees against the allowed roots, forwards
//! triggers to the check [`Scheduler`](crate::checks::scheduler::Scheduler), and renders the
//! per-binding `<agent-ide>` block from in-memory state only.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::host_binding::HostKind;
use super::launcher::{LauncherConfig, admit_worktree};
use crate::checks::runner::{NestedSandboxFallbackRunner, SeatbeltRunner};
use crate::checks::scheduler::{CompletionHook, Scheduler, sweep_stale_caches};
use crate::checks::{
    CheckState, Checker, Language, MAX_PROBLEMS, Problem, ProblemSnapshot, Recheck, Severity,
    UnavailableReason,
};
use crate::execution::seatbelt::ReadDeny;
use crate::feed::{FeedKey, FeedState, MAX_FEED_KEYS};

/// Checks run concurrently across the daemon (EYES-r1 §5).
const MAX_CONCURRENT_CHECKS: usize = 2;

/// Native Claude tools whose completed post-hook triggers a project check (EYES-r2 §5).
pub const CHECK_TRIGGER_TOOLS: [&str; 5] = ["Edit", "Write", "MultiEdit", "NotebookEdit", "Bash"];

/// Native tool-name prefixes of this product's own managed MCP tools (T29B §4).
///
/// An Agent IDE MCP call settles through its validated MCP reply, so its paired native post must
/// never schedule a check that would treat the helper's own result as a foreign change. Both the
/// underscore and hyphen spellings occur in tool naming.
const SELF_MCP_TOOL_PREFIXES: [&str; 2] = ["mcp__agent_ide__", "mcp__agent-ide__"];

/// Reports whether one post-phase tool name is this product's own managed MCP tool (T29B §4).
///
/// A post carrying such a name is the native shadow of an `ide.*` MCP call. When its exact pre
/// is still buffered, the call can never have reached managed admission — an admitted call
/// consumes its pre and records completion — so the pairing proves the call was rejected before
/// admission and must never become a native observation.
pub(crate) fn is_self_mcp_tool_name(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| {
        SELF_MCP_TOOL_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
    })
}

/// Exact native tool names that cannot change the worktree, per host.
///
/// Codex reports namespaced built-ins with the namespace prefixed and no separator: a model
/// waiting on a pending `ide.*` result calls `clock.sleep`, which arrives as `clocksleep`. Claude's
/// entries are its read-only file tools. Only exact names are listed; every other name, including
/// any MCP tool of another server, stays a possible writer.
const INERT_CODEX_TOOLS: [&str; 2] = ["clocksleep", "clockcurr_time"];
/// Claude's read-only file tools; see [`INERT_CODEX_TOOLS`] for the exact-name rule.
const INERT_CLAUDE_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// Reports whether one native post may have changed the worktree, so it must advance the binding's
/// native epoch (invalidating retained Context/Diff results) and may schedule a check.
///
/// A missing or unrecognized name counts as a possible writer; only the exact per-host inert names
/// are excluded, so waiting or reading between `ide.*` calls never discards a result the agent has
/// not retrieved yet.
pub fn may_write(host: HostKind, tool_name: Option<&str>) -> bool {
    let inert: &[&str] = match host {
        HostKind::Claude => &INERT_CLAUDE_TOOLS,
        HostKind::Codex => &INERT_CODEX_TOOLS,
    };
    !tool_name.is_some_and(|name| inert.contains(&name))
}

/// Decides whether one settled native post phase schedules a project check (T29B §4).
///
/// Claude keeps the exact [`CHECK_TRIGGER_TOOLS`] writer allowlist. Codex has no certified writer
/// allowlist yet, so every paired post triggers except this product's own MCP tool names and the
/// inert built-ins of [`may_write`]; other servers' MCP tools are deliberately never excluded
/// because they may edit files. A missing or unrecognized name triggers conservatively — an
/// unchanged-status post still stays silent, and T20B eligibility bounds the repeated-check cost.
pub fn triggers_check(host: HostKind, tool_name: Option<&str>) -> bool {
    match host {
        HostKind::Claude => tool_name.is_some_and(|name| CHECK_TRIGGER_TOOLS.contains(&name)),
        HostKind::Codex => !is_self_mcp_tool_name(tool_name) && may_write(host, tool_name),
    }
}

/// Maximum problems rendered on one `ide.context` problems page.
pub const PROBLEMS_PAGE_SIZE: u32 = 20;

/// Maximum retained characters in one rendered problem's diagnostic code.
///
/// A checker code is short by convention (e.g. `E0308`); this only bounds a pathological or
/// adversarial value, matching [`untrusted_line`]'s single-line, control-free guarantee.
const MAX_CODE_CHARS: usize = 64;

/// Appends one bounded explicit-test status line while preserving the feed's byte ceiling.
fn append_test_status(block: &str, status: &str) -> String {
    let line = status
        .chars()
        .filter(|character| !character.is_control())
        .take(160)
        .collect::<String>();
    let prefix = block
        .strip_suffix("</agent-ide>")
        .unwrap_or(block)
        .trim_end();
    let separator = if prefix.ends_with("<agent-ide>") {
        ""
    } else {
        "\n"
    };
    let mut result = format!("{prefix}{separator}{line}\n</agent-ide>");
    if result.len() > crate::feed::MAX_BLOCK_BYTES {
        let excess = result.len() - crate::feed::MAX_BLOCK_BYTES;
        let start = prefix.find('\n').unwrap_or(prefix.len());
        let body = &prefix[start..];
        let cut = body
            .char_indices()
            .find(|(index, _)| *index >= excess)
            .map_or(body.len(), |(index, _)| index);
        result = format!("<agent-ide>{}\n{line}\n</agent-ide>", &body[cut..]);
    }
    result
}

/// Supplies the latest completed project check snapshots for one authorized worktree.
///
/// Implementers must be usable from the daemon worker concurrently (`Send + Sync`) and must
/// return promptly without running checks, executing processes, or blocking: the caller answers
/// a bounded `ide.context` request from this lookup alone. The returned list holds at most one
/// snapshot per configured language, in feed (registration) order.
pub trait ProblemSource: Send + Sync {
    /// Returns the latest completed snapshot per configured language for `worktree`.
    ///
    /// `worktree` is the caller's authorized worktree path; a source must never report
    /// snapshots for a different worktree. An empty result means no language is configured,
    /// which the renderer reports as `checks disabled`.
    fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot>;

    /// Returns why a language's [`ProblemSource::latest`] snapshot is not the current state of
    /// the worktree: [`Recheck::FirstCheck`] when it predates the session's activation and its
    /// re-check is pending or running, [`Recheck::FilesChanged`] while a check for newer inputs
    /// is running. Defaults to none.
    fn rechecks(&self, _worktree: &Path) -> Vec<(Language, Recheck)> {
        Vec::new()
    }
}

/// Worktree bound to one activated actor binding, as recorded at `ide.start`.
struct BoundWorktree {
    /// Canonical worktree path Workspace resolved for the binding.
    worktree: PathBuf,
    /// Canonical git common dir of the worktree's repository, the scheduler repository key.
    repository_key: String,
    /// Whether the worktree was admitted under an allowed root; `false` renders `outside_roots`.
    admitted: bool,
    /// A current host profile could not prove a supported check read policy; no check or cached
    /// diagnostic for this binding may be scheduled or disclosed.
    read_restricted: bool,
    /// Read exclusions captured at activation for this binding's check replies.
    read_denies: Vec<ReadDeny>,
}

/// Mutable feed wiring state, locked only for synchronous in-memory reads and writes.
#[derive(Default)]
struct FeedWiring {
    /// Bound worktrees keyed by binding fingerprint; at most [`MAX_FEED_KEYS`] entries.
    bindings: HashMap<[u8; 32], BoundWorktree>,
    /// Restrictions observed while Start is still pending; activation consumes them monotonically.
    pending_restricted: HashSet<[u8; 32]>,
    /// Delivered-block state per `(binding, worktree)`.
    feed: FeedState,
    /// Daemon-owned test runner for appending explicit test status to the same plate.
    test_runs: Option<super::tests::TestRuns>,
    /// Last test status actually delivered for each binding/worktree.
    test_delivered: HashMap<FeedKey, String>,
}

/// Daemon-owned project problem feed: trigger routing, admission, and block delivery state.
///
/// One instance exists per daemon when project checks are configured. Every method is
/// synchronous and non-blocking except [`ProjectProblemFeed::shutdown`]; none waits for a check,
/// so hook and IPC paths can consult it inside their existing deadlines. Bindings are identified
/// by their opaque fingerprint and must be forgotten on `ide.stop`.
pub struct ProjectProblemFeed {
    /// Debounced check scheduler that owns every check run and completed snapshot.
    scheduler: Scheduler,
    /// Operator-declared allowed roots used for worktree admission.
    allowed_roots: Vec<PathBuf>,
    /// Configured languages in feed order; the scheduler runs exactly these.
    languages: Vec<Language>,
    /// Binding and delivery state.
    state: Mutex<FeedWiring>,
}

impl ProjectProblemFeed {
    /// Builds the feed around an already configured `scheduler`.
    ///
    /// `allowed_roots` must be nonempty for any worktree to be admitted; `languages` lists the
    /// languages `scheduler` has checkers for and is sorted into feed order here.
    pub fn new(
        scheduler: Scheduler,
        allowed_roots: Vec<PathBuf>,
        mut languages: Vec<Language>,
    ) -> Self {
        languages.sort();
        languages.dedup();
        Self {
            scheduler,
            allowed_roots,
            languages,
            state: Mutex::new(FeedWiring::default()),
        }
    }

    /// Builds the production feed from the launcher configuration, or `None` when disabled.
    ///
    /// Project checks are enabled only with nonempty `allowed_roots`, a `project_checks` section
    /// and at least one configured language (EYES-r1 §1); otherwise `None` leaves v0.2 behaviour
    /// unchanged. Checkers run through the Seatbelt runner (with its one-time nested-sandbox
    /// fallback for a daemon the host itself confines) with the configured timeout, the
    /// scheduler uses the configured debounce and the cache root `$HOME/.agent-ide/checks`
    /// (created `0700` best-effort), and stale caches of removed worktrees are swept first.
    /// `on_complete` observes every completed check run. Returns `None` when `HOME` is unset.
    pub fn from_launcher(launcher: &LauncherConfig, on_complete: CompletionHook) -> Option<Self> {
        let checks = launcher.project_checks()?;
        if launcher.allowed_roots().is_empty() {
            return None;
        }
        let runner = Arc::new(NestedSandboxFallbackRunner::new(Arc::new(SeatbeltRunner)));
        let checkers: Vec<Arc<dyn Checker>> = checks
            .sections()
            .map(|(_, config)| config.checker(runner.clone(), checks.check_timeout()))
            .collect();
        if checkers.is_empty() {
            return None;
        }
        let cache_root = crate::userhome::user_home()?
            .join(".agent-ide")
            .join("checks");
        {
            use std::os::unix::fs::DirBuilderExt;
            let _ = std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&cache_root);
        }
        sweep_stale_caches(&cache_root);
        let languages = checkers.iter().map(|checker| checker.language()).collect();
        let scheduler = Scheduler::new(
            checkers,
            checks.debounce(),
            MAX_CONCURRENT_CHECKS,
            cache_root,
        )
        .with_completion_hook(on_complete);
        Some(Self::new(
            scheduler,
            launcher.allowed_roots().to_vec(),
            languages,
        ))
    }

    /// Records a successful `ide.start` and schedules its initial warm check when the caller
    /// supplied an accepted read policy. Scheduling serializes with later restriction.
    ///
    /// `worktree` is Workspace's canonical worktree path and `repository_key` its canonical git
    /// common dir. A worktree outside every allowed root is recorded as not admitted — its
    /// snapshots then report `outside_roots` — and no check is scheduled. Replaces any previous
    /// record for the same binding; when the bound set is full an arbitrary other binding is
    /// evicted first. `read_restricted` or any restriction observed while Start was pending
    /// suppresses scheduling and all cached diagnostics for this binding without probing files.
    pub fn activated(
        &self,
        binding: [u8; 32],
        worktree: &Path,
        repository_key: &Path,
        read_restricted: bool,
    ) {
        self.activated_with_denies(
            binding,
            worktree,
            repository_key,
            read_restricted,
            Vec::new(),
        );
    }

    /// Activates checks with the current host exclusions installed before any scheduler work.
    pub fn activated_with_denies(
        &self,
        binding: [u8; 32],
        worktree: &Path,
        repository_key: &Path,
        read_restricted: bool,
        read_denies: Vec<ReadDeny>,
    ) {
        let admitted = admit_worktree(&self.allowed_roots, worktree).is_ok();
        let repository_key = repository_key.to_string_lossy().into_owned();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let pending_restricted = state.pending_restricted.remove(&binding);
        let read_restricted = read_restricted
            || pending_restricted
            || state
                .bindings
                .get(&binding)
                .is_some_and(|bound| bound.read_restricted);
        if admitted && !read_restricted {
            self.scheduler.add_read_denies(worktree, &read_denies);
            self.scheduler.activate(&repository_key, worktree);
        }
        if state.bindings.len() >= MAX_FEED_KEYS
            && !state.bindings.contains_key(&binding)
            && let Some(evicted) = state.bindings.keys().next().copied()
        {
            state.bindings.remove(&evicted);
        }
        state.bindings.insert(
            binding,
            BoundWorktree {
                worktree: worktree.to_path_buf(),
                repository_key,
                admitted,
                read_restricted,
                read_denies,
            },
        );
    }

    /// Keeps a binding available only while the current host profile has its activated denies.
    pub fn accepts_read_denies(&self, binding: &[u8; 32], denies: &[ReadDeny]) -> bool {
        self.state
            .lock()
            .ok()
            .and_then(|state| {
                state
                    .bindings
                    .get(binding)
                    .map(|bound| bound.read_denies == denies)
            })
            .unwrap_or(true)
    }

    /// Returns the activated worktree for a binding, or `None` before Start settles.
    pub fn bound_worktree(&self, binding: &[u8; 32]) -> Option<PathBuf> {
        self.state
            .lock()
            .ok()?
            .bindings
            .get(binding)
            .map(|bound| bound.worktree.clone())
    }

    /// Permanently disables this binding's checks and cached diagnostics after a narrower host
    /// profile is observed, cancelling any already queued or running check for its worktree. A
    /// restriction before activation is retained for that pending binding and wins over Start's
    /// older observation. Cancellation serializes with trigger admission under the binding lock;
    /// a new Start binding is required to restore check availability.
    pub fn restrict(&self, binding: &[u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            let Some(bound) = state.bindings.get_mut(binding) else {
                state.pending_restricted.insert(*binding);
                return;
            };
            if bound.read_restricted {
                return;
            }
            bound.read_restricted = true;
            self.scheduler.cancel_worktree(&bound.worktree);
        }
    }

    /// Reports whether this binding must receive only read-restricted status, not snapshots.
    pub fn is_read_restricted(&self, binding: &[u8; 32]) -> bool {
        self.state.lock().map_or(true, |state| {
            state
                .bindings
                .get(binding)
                .is_none_or(|bound| bound.read_restricted)
        })
    }

    /// Returns one content-free unavailable result per configured language without probing files.
    pub fn read_restricted_snapshots(&self) -> Vec<ProblemSnapshot> {
        self.languages
            .iter()
            .map(|language| {
                ProblemSnapshot::unavailable(*language, UnavailableReason::ReadRestricted, 0)
            })
            .collect()
    }

    /// Schedules a check for `binding`'s admitted worktree after a native edit or `ide.edit`
    /// that cannot name the changed file, so every configured language is re-armed.
    ///
    /// An unknown or read-restricted binding, or a worktree outside the allowed roots, schedules
    /// nothing.
    pub fn changed(&self, binding: &[u8; 32]) {
        self.changed_with(binding, None, false, || {});
    }

    /// Like [`ProjectProblemFeed::changed`], naming the changed file when the trigger knows it
    /// (a native `Edit`/`Write`/`MultiEdit`/`NotebookEdit` post hook, `ide.edit`): that file's
    /// language is the one a waiting caller expects, and every other configured language is
    /// re-armed through the ordinary fingerprint-gated trigger, so a `.py` edit never forces a
    /// cargo check. A path no registered language owns keeps the every-language behaviour.
    pub fn changed_file(&self, binding: &[u8; 32], path: Option<&str>) {
        self.changed_with(binding, path.map(Path::new), false, || {});
    }

    /// Like [`ProjectProblemFeed::changed_file`] for a check the caller waits on: the changed
    /// file's own language skips the unchanged-input elision and the cooldown after the
    /// previous run, and the returned input generation lets the caller wait for a snapshot at
    /// or past it. `None` when the trigger was not admitted.
    pub fn changed_generation(&self, binding: &[u8; 32], path: Option<&str>) -> Option<u64> {
        self.changed_with(binding, path.map(Path::new), true, || {})
    }

    /// Admits the trigger while holding the binding lock and returns the resulting input
    /// generation; `before_trigger` is a test seam for proving a concurrent restriction cannot
    /// slip between the decision and scheduler call.
    fn changed_with(
        &self,
        binding: &[u8; 32],
        path: Option<&Path>,
        urgent: bool,
        before_trigger: impl FnOnce(),
    ) -> Option<u64> {
        if let Ok(state) = self.state.lock()
            && let Some(bound) = state.bindings.get(binding)
            && bound.admitted
            && !bound.read_restricted
        {
            before_trigger();
            if urgent {
                self.scheduler
                    .trigger_urgent(&bound.repository_key, &bound.worktree, path);
            } else if let Some(path) = path {
                self.scheduler
                    .trigger_for_path(&bound.repository_key, &bound.worktree, path);
            } else {
                self.scheduler
                    .trigger(&bound.repository_key, &bound.worktree);
            }
            return Some(self.scheduler.generation(&bound.worktree));
        }
        None
    }

    /// Returns the `<agent-ide>` block due for `binding`, marking it delivered, or `None`.
    ///
    /// Reads only in-memory snapshots and never waits for a running check. `None` means the
    /// binding is unknown, no language has a completed result yet, or the item set equals the
    /// last block delivered to this binding for its worktree (EYES-r1 §6). Restricted bindings
    /// receive only content-free unavailable language states.
    pub fn next_block(&self, binding: &[u8; 32]) -> Option<String> {
        self.next_block_when(binding, |_| true)
    }

    /// Like [`ProjectProblemFeed::next_block`], but marks the block delivered only once `fits`
    /// accepts the rendered text (T28B).
    ///
    /// The whole decision — snapshot read, render, `fits`, delivery record — happens under the
    /// one wiring lock, so a block refused by `fits` is never recorded and stays due for the
    /// next call. This is the reply-carried delivery used by hosts without a hook stream: the
    /// plate may only be consumed by a reply that actually carries it whole.
    pub fn next_block_when(
        &self,
        binding: &[u8; 32],
        fits: impl FnOnce(&str) -> bool,
    ) -> Option<String> {
        self.next_block_when_with_test_status(binding, None, fits)
    }

    /// Like [`Self::next_block_when`], using a status snapshot already selected for this reply.
    pub fn next_block_when_with_test_status(
        &self,
        binding: &[u8; 32],
        test_status_snapshot: Option<&str>,
        fits: impl FnOnce(&str) -> bool,
    ) -> Option<String> {
        let mut guard = self.state.lock().ok()?;
        let state = &mut *guard;
        let bound = state.bindings.get(binding)?;
        let key = FeedKey {
            binding: hex(binding),
            worktree: bound.worktree.clone(),
        };
        let snapshots = if bound.read_restricted {
            self.read_restricted_snapshots()
        } else {
            self.snapshots(&bound.worktree, bound.admitted)
        };
        let rechecks = if bound.read_restricted {
            Vec::new()
        } else {
            self.rechecks_for(&bound.worktree)
        };
        let test_status = test_status_snapshot.map(str::to_owned).or_else(|| {
            state
                .test_runs
                .as_ref()
                .and_then(|runs| runs.status_line_for_binding(binding))
        });
        let due_test = test_status
            .as_ref()
            .filter(|status| state.test_delivered.get(&key) != Some(*status));
        let fits = std::cell::RefCell::new(Some(fits));
        let block = state
            .feed
            .next_block_when(&key, &snapshots, &rechecks, |block| {
                let combined = if let Some(status) = due_test {
                    append_test_status(block, status)
                } else {
                    block.to_owned()
                };
                fits.borrow_mut().take().is_some_and(|fits| fits(&combined))
            });
        let block = match (block, due_test) {
            (Some(block), Some(status)) => Some(append_test_status(&block, status)),
            (Some(block), None) => Some(block),
            (None, Some(status)) => {
                let plate = append_test_status("<agent-ide>\n</agent-ide>", status);
                fits.borrow_mut()
                    .take()
                    .is_some_and(|fits| fits(&plate))
                    .then_some(plate)
            }
            (None, None) => None,
        }?;
        if let Some(status) = due_test {
            state.test_delivered.insert(key.clone(), status.clone());
            if let Some(runs) = &state.test_runs {
                runs.mark_feed_status_delivered(&bound.worktree, status);
            }
        }
        let languages = snapshots
            .iter()
            .filter(|snapshot| {
                !matches!(
                    snapshot.state,
                    CheckState::Unavailable(UnavailableReason::Disabled)
                )
            })
            .map(|snapshot| snapshot.language.as_str())
            .collect::<Vec<_>>()
            .join(",");
        crate::errorlog::record(
            crate::errorlog::Method::Feed,
            crate::errorlog::Outcome::Completed,
            crate::errorlog::Fields {
                worktree: Some(&bound.worktree),
                detail: Some(&format!("languages={languages} bytes={}", block.len())),
                ..Default::default()
            },
        );
        Some(block)
    }

    /// Drops `binding`'s pending restriction, worktree record, and delivery state on `ide.stop`.
    pub fn forget(&self, binding: &[u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            state.pending_restricted.remove(binding);
            if let Some(bound) = state.bindings.remove(binding) {
                state.feed.forget(&FeedKey {
                    binding: hex(binding),
                    worktree: bound.worktree.clone(),
                });
                state.test_delivered.remove(&FeedKey {
                    binding: hex(binding),
                    worktree: bound.worktree,
                });
            }
        }
    }

    /// Attaches the daemon's test status source before worker startup.
    pub fn with_test_runs(&self, runs: super::tests::TestRuns) {
        if let Ok(mut state) = self.state.lock() {
            state.test_runs = Some(runs);
        }
    }

    /// Reports whether any check is pending or running, for the daemon idle controller.
    pub fn is_busy(&self) -> bool {
        self.scheduler.is_busy()
    }

    /// Cancels every pending and running check; triggers afterwards are ignored.
    pub async fn shutdown(&self) {
        self.scheduler.shutdown().await;
    }

    /// Returns one snapshot per configured language for `worktree`, in feed order.
    ///
    /// Not-admitted worktrees report `unavailable(outside_roots)`; a configured language without
    /// a completed result reports `checking`.
    fn snapshots(&self, worktree: &Path, admitted: bool) -> Vec<ProblemSnapshot> {
        if !admitted {
            return self
                .languages
                .iter()
                .map(|language| {
                    ProblemSnapshot::unavailable(*language, UnavailableReason::OutsideRoots, 0)
                })
                .collect();
        }
        let completed = self.scheduler.latest(worktree);
        // A language absent from the worktree never gets a `checking` placeholder: before its
        // first result a project of only one language would otherwise announce the others as
        // `checking` (T10B).
        self.languages
            .iter()
            .filter_map(|language| {
                completed
                    .iter()
                    .find(|snapshot| snapshot.language == *language)
                    .cloned()
                    .or_else(|| {
                        language
                            .is_present(worktree)
                            .then(|| ProblemSnapshot::checking(*language, 0))
                    })
            })
            .collect()
    }

    /// Returns the languages of `worktree` whose stored result is not its current state (T18B).
    ///
    /// A result predating this session's activation is [`Recheck::FirstCheck`]; otherwise a
    /// language with a check running is [`Recheck::FilesChanged`]. A pending debounce timer alone
    /// changes nothing, so a quiet or no-op session stays silent.
    fn rechecks_for(&self, worktree: &Path) -> Vec<(Language, Recheck)> {
        let stale = self.scheduler.stale(worktree);
        let running = self.scheduler.running(worktree);
        self.languages
            .iter()
            .filter_map(|language| {
                if stale.contains(language) {
                    Some((*language, Recheck::FirstCheck))
                } else {
                    running
                        .contains(language)
                        .then_some((*language, Recheck::FilesChanged))
                }
            })
            .collect()
    }
}

impl ProblemSource for ProjectProblemFeed {
    /// Answers from the scheduler's completed snapshots, reporting `outside_roots` when a binding
    /// recorded `worktree` as not admitted and `checking` for languages without a result.
    fn latest(&self, worktree: &Path) -> Vec<ProblemSnapshot> {
        let admitted = self.state.lock().map_or(true, |state| {
            !state
                .bindings
                .values()
                .any(|bound| bound.worktree == worktree && !bound.admitted)
        });
        self.snapshots(worktree, admitted)
    }

    fn rechecks(&self, worktree: &Path) -> Vec<(Language, Recheck)> {
        self.rechecks_for(worktree)
    }
}

/// Renders a binding fingerprint as lowercase hex, the stable [`FeedKey::binding`] form.
fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Parses one closed `language` parameter value into its snapshot language.
///
/// Accepts exactly the identifier of a registered language with project checks; any other
/// value returns `None` for the caller to treat as a filter-less request or a validation error.
pub fn parse_language(value: &str) -> Option<Language> {
    Language::by_id(value).filter(|language| language.checks().is_some())
}

/// Renders the compact problems page text for the requested language filter and offset.
///
/// `language` selects one configured language, or `None` for all configured languages in feed
/// order. `offset` is the zero-based start into the combined, language-ordered problem list.
/// Each selected language contributes one state line — `ready`/`partial` carry the full
/// `errors`/`warnings` counts, while `checking` and unavailable states carry no numeric counts.
/// Unavailable states use the same plain phrase as the status plate; `ReadRestricted` retains the
/// content-free `unavailable: read_restricted` phrase. A fatal checker detail or a short fallback
/// `path:line:column severity [code] message`. A snapshot that dropped problems to the
/// [`MAX_PROBLEMS`] cap (T19B) adds one header line directly after its state line —
/// `<language>: list truncated to first MAX_PROBLEMS problems; counts above are complete` —
/// rendered on every page, so paging can never silently drop it. `next_offset: <offset + page>`
/// is appended exactly when more problems remain after the page. Empty input, or a filter matching no
/// configured language, renders the single line `checks disabled`. A language absent from the
/// worktree (T10B: `Unavailable(Disabled)`) contributes no state line and no problems, exactly as
/// it is omitted from the `<agent-ide>` block; when every matched language is absent this way,
/// the single line `no supported project detected` renders instead. Every rendered textual field
/// is stripped of control characters so each problem stays one plain, untrusted line.
pub fn problems_text(
    snapshots: &[ProblemSnapshot],
    language: Option<Language>,
    offset: u32,
) -> String {
    problems_text_with_rechecks(snapshots, &[], language, offset)
}

/// Like [`problems_text`], but a `ready`/`partial` line of a language in `rechecks` names why its
/// counts are not current (T18B): `<language>: checking (files changed); last result: errors: N;
/// warnings: M` while a check for newer inputs runs, and `<language>: checking (first check in this
/// session); previous session result: errors: N; warnings: M` for a result predating this
/// session's activation.
pub fn problems_text_with_rechecks(
    snapshots: &[ProblemSnapshot],
    rechecks: &[(Language, Recheck)],
    language: Option<Language>,
    offset: u32,
) -> String {
    let matched: Vec<&ProblemSnapshot> = snapshots
        .iter()
        .filter(|snapshot| language.is_none_or(|selected| snapshot.language == selected))
        .collect();
    if matched.is_empty() {
        return "checks disabled".to_owned();
    }
    let selected: Vec<&ProblemSnapshot> = matched
        .into_iter()
        .filter(|snapshot| {
            !matches!(
                snapshot.state,
                CheckState::Unavailable(UnavailableReason::Disabled)
            )
        })
        .collect();
    if selected.is_empty() {
        return "no supported project detected".to_owned();
    }
    let mut lines: Vec<String> = Vec::new();
    let mut skipped = offset as usize;
    let mut rendered: u32 = 0;
    let mut more = false;
    for snapshot in &selected {
        let recheck = rechecks
            .iter()
            .find(|(recheck_language, _)| *recheck_language == snapshot.language)
            .map(|(_, recheck)| *recheck);
        lines.push(state_line(snapshot, recheck));
        if snapshot.truncated {
            // The counts in the state line are computed before the [`MAX_PROBLEMS`] cap, so only
            // the list is cut. This sits in the header next to the state line: paging skips
            // per-language problem lines, and the notice must survive every page (T19B).
            lines.push(format!(
                "{}: list truncated to first {MAX_PROBLEMS} problems; counts above are complete",
                snapshot.language.as_str()
            ));
        }
        for problem in &snapshot.problems {
            if skipped > 0 {
                skipped -= 1;
            } else if rendered < PROBLEMS_PAGE_SIZE {
                rendered += 1;
                lines.push(problem_line(problem));
            } else {
                more = true;
            }
        }
    }
    if more {
        lines.push(format!(
            "next_offset: {}",
            offset.saturating_add(PROBLEMS_PAGE_SIZE)
        ));
    }
    lines.join("\n")
}

/// Builds one language's state line, hiding the zero counts of non-reporting states.
///
/// `checking` and `unavailable` snapshots carry zero counts by construction (see `checks`);
/// those must never render as numbers, so only `ready` and `partial` name counts. An
/// `unavailable` snapshots carrying [`ProblemSnapshot::detail`] append it in parentheses after
/// stripping control characters. A fatal check with no detail states that the checker supplied no
/// reason, while `ReadRestricted` renders the content-free `unavailable: read_restricted` phrase.
fn state_line(snapshot: &ProblemSnapshot, recheck: Option<Recheck>) -> String {
    let language = snapshot.language.as_str();
    let counts = format!(
        "errors: {}; warnings: {}",
        snapshot.errors, snapshot.warnings
    );
    match &snapshot.state {
        CheckState::Ready | CheckState::Partial if recheck == Some(Recheck::FilesChanged) => {
            format!("{language}: checking (files changed); last result: {counts}")
        }
        CheckState::Ready | CheckState::Partial if recheck == Some(Recheck::FirstCheck) => format!(
            "{language}: checking (first check in this session); previous session result: {counts}"
        ),
        CheckState::Ready => format!("{language}: ready; {counts}"),
        CheckState::Partial => format!("{language}: partial; {counts}"),
        CheckState::Checking => format!("{language}: checking (first check in this session)"),
        CheckState::Unavailable(UnavailableReason::ReadRestricted) => {
            format!("{language}: unavailable: read_restricted")
        }
        CheckState::Unavailable(UnavailableReason::Fatal) => match &snapshot.detail {
            Some(detail) => format!("{language}: check failed ({})", untrusted_line(detail)),
            None => {
                format!("{language}: check failed (checker supplied no reason)")
            }
        },
        CheckState::Unavailable(reason) => match &snapshot.detail {
            Some(detail) => format!(
                "{language}: {} ({})",
                unavailable_reason(*reason),
                untrusted_line(detail)
            ),
            None => format!("{language}: {}", unavailable_reason(*reason)),
        },
    }
}

/// Renders one problem as `path:line:column severity [code] message`.
///
/// The bracketed code segment is omitted when the checker reported no code, and otherwise
/// passes through [`sanitized_code`] rather than [`untrusted_line`] directly, so it also gets a
/// length cap. All other textual fields pass through [`untrusted_line`]; the numeric fields come
/// from the typed snapshot.
fn problem_line(problem: &Problem) -> String {
    let severity = match problem.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
    };
    let code = problem
        .code
        .as_deref()
        .map(|code| format!(" [{}]", sanitized_code(code)))
        .unwrap_or_default();
    format!(
        "{}:{}:{} {severity}{code} {}",
        untrusted_line(&problem.path),
        problem.line,
        problem.column,
        untrusted_line(&problem.message)
    )
}

/// Maps one unavailable reason to the same plain phrase shown in the status plate.
fn unavailable_reason(reason: UnavailableReason) -> &'static str {
    match reason {
        UnavailableReason::Disabled => "checks disabled",
        UnavailableReason::ReadRestricted => "unavailable: read_restricted",
        UnavailableReason::OutsideRoots => "outside allowed roots",
        UnavailableReason::ToolMissing => "tool not found",
        UnavailableReason::EnvMissing => "environment not found",
        UnavailableReason::NoFiles => "no files analyzed",
        UnavailableReason::Fatal => "check failed",
        UnavailableReason::Timeout => "check timed out",
    }
}

/// Strips control characters from one untrusted checker text field.
///
/// Checker output is never trusted context: removing control characters keeps every rendered
/// problem on exactly one line and gives the text no markup, escape, or framing structure.
fn untrusted_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

/// Sanitizes one untrusted checker diagnostic code: strips control characters and caps length.
///
/// Applies the same [`untrusted_line`] guarantee as `message`, then bounds the result to
/// [`MAX_CODE_CHARS`] characters so a pathologically long or adversarial code cannot grow the
/// rendered block unbounded.
fn sanitized_code(value: &str) -> String {
    untrusted_line(value).chars().take(MAX_CODE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one problem with a fixed code for compact test arrangements.
    fn problem(path: &str, line: u32, column: u32, severity: Severity, message: &str) -> Problem {
        Problem::new(
            path.to_owned(),
            line,
            column,
            severity,
            Some("E0001".to_owned()),
            message.to_owned(),
        )
    }

    /// Builds a ready snapshot whose counts derive from the supplied problems.
    fn ready(language: Language, problems: Vec<Problem>) -> ProblemSnapshot {
        ProblemSnapshot::from_problems(language, CheckState::Ready, problems, 1, 5)
    }

    /// Pages 45 fake problems 20 at a time with exact next_offset markers and no overrun page.
    #[test]
    fn page_boundaries_with_45_problems_emit_next_offset_exactly_twice() {
        let problems: Vec<Problem> = (1..=45)
            .map(|line| problem("src/lib.rs", line, 1, Severity::Error, "boom"))
            .collect();
        let snapshots = [ready(crate::lang::testing::ALPHA, problems)];
        assert_eq!(PROBLEMS_PAGE_SIZE, 20);

        let first = problems_text(&snapshots, None, 0);
        let first_lines: Vec<&str> = first.lines().collect();
        assert_eq!(first_lines.len(), 22, "{first}");
        assert_eq!(first_lines[0], "alpha: ready; errors: 45; warnings: 0");
        assert!(first_lines[1].starts_with("src/lib.rs:1:1 error [E0001] boom"));
        assert!(first_lines[20].starts_with("src/lib.rs:20:1 error [E0001] boom"));
        assert_eq!(first_lines[21], "next_offset: 20");

        let second = problems_text(&snapshots, None, 20);
        let second_lines: Vec<&str> = second.lines().collect();
        assert_eq!(second_lines.len(), 22, "{second}");
        assert!(second_lines[1].starts_with("src/lib.rs:21:1"));
        assert!(second_lines[20].starts_with("src/lib.rs:40:1"));
        assert_eq!(second_lines[21], "next_offset: 40");

        let last = problems_text(&snapshots, None, 40);
        let last_lines: Vec<&str> = last.lines().collect();
        assert_eq!(last_lines.len(), 6, "{last}");
        assert!(last_lines[5].starts_with("src/lib.rs:45:1"));
        assert!(!last.contains("next_offset"));

        // A page past the end renders only state lines, without inventing a next offset.
        let past = problems_text(&snapshots, None, 45);
        assert_eq!(past.lines().count(), 1, "{past}");
        assert!(!past.contains("next_offset"));
    }

    /// Only exact inert names are excluded from native-epoch advances; unknown and missing names,
    /// writers, and other servers' MCP tools still count as possible writers on both hosts.
    #[test]
    fn only_exact_inert_tools_skip_native_epoch_advance() {
        for name in ["clocksleep", "clockcurr_time"] {
            assert!(!may_write(HostKind::Codex, Some(name)), "{name}");
            assert!(may_write(HostKind::Claude, Some(name)), "{name}");
        }
        for name in ["Read", "Grep", "Glob"] {
            assert!(!may_write(HostKind::Claude, Some(name)), "{name}");
            assert!(may_write(HostKind::Codex, Some(name)), "{name}");
        }
        for host in [HostKind::Claude, HostKind::Codex] {
            for name in [
                "Bash",
                "Edit",
                "apply_patch",
                "exec_command",
                "clock",
                "mcp__x__read",
            ] {
                assert!(may_write(host, Some(name)), "{name}");
            }
            assert!(may_write(host, None));
        }
    }

    /// Host-specific check triggers (T29B §4): Claude keeps its exact writer allowlist while
    /// Codex triggers conservatively on every name except this product's own MCP tools.
    #[test]
    fn check_trigger_predicate_is_host_specific_and_conservative() {
        for name in CHECK_TRIGGER_TOOLS {
            assert!(triggers_check(HostKind::Claude, Some(name)), "{name}");
            assert!(triggers_check(HostKind::Codex, Some(name)), "{name}");
        }
        // Claude: exact allowlist, no conservative default.
        assert!(!triggers_check(HostKind::Claude, Some("Grep")));
        assert!(!triggers_check(HostKind::Claude, None));
        // Codex: known and unknown writers trigger, missing names trigger.
        assert!(triggers_check(HostKind::Codex, Some("apply_patch")));
        assert!(triggers_check(HostKind::Codex, Some("exec_command")));
        assert!(triggers_check(
            HostKind::Codex,
            Some("mcp__other-server__write")
        ));
        assert!(triggers_check(HostKind::Codex, None));
        // Codex's clock built-ins never write, so waiting on a pending result triggers nothing.
        assert!(!triggers_check(HostKind::Codex, Some("clocksleep")));
        assert!(!triggers_check(HostKind::Codex, Some("clockcurr_time")));
        // Only this product's own MCP tool names are excluded, in both spellings.
        assert!(!triggers_check(
            HostKind::Codex,
            Some("mcp__agent_ide__context")
        ));
        assert!(!triggers_check(
            HostKind::Codex,
            Some("mcp__agent-ide__edit")
        ));
        assert!(triggers_check(
            HostKind::Codex,
            Some("mcp__agent_ide_extra__edit")
        ));
    }

    /// A language filter renders only the matching configured language and its problems.
    #[test]
    fn language_filter_selects_only_the_matching_configured_language() {
        let snapshots = [
            ready(
                crate::lang::testing::ALPHA,
                vec![problem("a.rs", 1, 1, Severity::Error, "alpha one")],
            ),
            ready(
                crate::lang::testing::BETA,
                vec![problem("b.py", 2, 1, Severity::Warning, "beta one")],
            ),
        ];
        let all = problems_text(&snapshots, None, 0);
        assert!(
            all.contains("alpha: ready; errors: 1; warnings: 0"),
            "{all}"
        );
        assert!(all.contains("beta: ready; errors: 0; warnings: 1"), "{all}");
        assert!(all.contains("a.rs:1:1 error [E0001] alpha one"));
        assert!(all.contains("b.py:2:1 warning [E0001] beta one"));

        let beta = problems_text(&snapshots, Some(crate::lang::testing::BETA), 0);
        assert!(beta.contains("b.py:2:1 warning [E0001] beta one"));
        assert!(!beta.contains("alpha:"), "{beta}");
        assert!(!beta.contains("a.rs"));

        let alpha = problems_text(
            &snapshots,
            Some({
                crate::lang::testing::install();
                parse_language("alpha").expect("alpha parses")
            }),
            0,
        );
        assert!(alpha.contains("a.rs:1:1 error [E0001] alpha one"));
        assert!(!alpha.contains("beta:"), "{alpha}");
        assert_eq!(parse_language("delta"), None);
    }

    /// Untrusted checker text renders on exactly one line with all control characters stripped.
    #[test]
    fn untrusted_text_is_single_line_without_control_characters() {
        let snapshots = [ready(
            crate::lang::testing::ALPHA,
            vec![Problem::new(
                "a\nb.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some("E0001".to_owned()),
                "line1\r\nline2\u{0}\u{7}tail".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert_eq!(text.lines().count(), 2, "{text}");
        // The newline separators are the only permitted control positions: every rendered line
        // is itself free of control characters, so a problem cannot forge lines or framing.
        for line in text.lines() {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        assert!(
            text.contains("ab.rs:1:1 error [E0001] line1line2tail"),
            "{text}"
        );
    }

    /// An empty snapshot list and a filter matching no configured language both report disabled.
    #[test]
    fn unconfigured_feed_renders_checks_disabled() {
        assert_eq!(problems_text(&[], None, 0), "checks disabled");
        let snapshots = [ready(crate::lang::testing::ALPHA, Vec::new())];
        assert_eq!(
            problems_text(&snapshots, Some(crate::lang::testing::BETA), 0),
            "checks disabled"
        );
    }

    /// A snapshot over the [`MAX_PROBLEMS`] cap (T19B) announces the truncation in the header
    /// right after its state line — with complete counts, which are computed before the cap — on
    /// every page, so paging cannot silently drop the notice.
    #[test]
    fn truncated_snapshot_announces_the_cap_next_to_the_state_line_on_every_page() {
        let problems: Vec<Problem> = (0..=MAX_PROBLEMS as u32)
            .map(|line| problem("e.rs", line, 1, Severity::Error, "e"))
            .collect();
        let snapshot = ProblemSnapshot::from_problems(
            crate::lang::testing::ALPHA,
            CheckState::Ready,
            problems,
            1,
            1,
        );
        assert!(snapshot.truncated);
        let notice = format!(
            "alpha: list truncated to first {MAX_PROBLEMS} problems; counts above are complete"
        );

        let first = problems_text(std::slice::from_ref(&snapshot), None, 0);
        let first_lines: Vec<&str> = first.lines().collect();
        assert_eq!(
            first_lines[0], "alpha: ready; errors: 501; warnings: 0",
            "{first}"
        );
        assert_eq!(first_lines[1], notice, "{first}");
        assert_eq!(first_lines.len(), 23, "{first}");

        // The notice survives paging, including a page past the end of the retained list.
        let paged = problems_text(&[snapshot], None, 40);
        assert_eq!(paged.lines().nth(1), Some(notice.as_str()), "{paged}");
    }

    /// An under-cap snapshot (T19B) renders no truncation notice.
    #[test]
    fn under_cap_snapshot_renders_no_truncation_notice() {
        let snapshots = [ready(
            crate::lang::testing::ALPHA,
            vec![problem("a.rs", 1, 1, Severity::Error, "e")],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert!(!text.contains("list truncated"), "{text}");
    }

    /// Non-reporting states never render numeric counts and unavailable reasons stay closed.
    #[test]
    fn lifecycle_states_render_without_counts_and_closed_reasons() {
        let snapshots = [
            ProblemSnapshot::from_problems(
                crate::lang::testing::ALPHA,
                CheckState::Partial,
                vec![problem("a.rs", 1, 1, Severity::Error, "e")],
                1,
                5,
            ),
            ProblemSnapshot::checking(crate::lang::testing::BETA, 3),
        ];
        let text = problems_text(&snapshots, None, 0);
        assert!(
            text.contains("alpha: partial; errors: 1; warnings: 0"),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|line| line == "beta: checking (first check in this session)"),
            "{text}"
        );
        assert!(
            !text.contains("beta: checking (first check in this session);"),
            "{text}"
        );

        for (reason, rendered) in [
            (
                UnavailableReason::OutsideRoots,
                "alpha: outside allowed roots",
            ),
            (UnavailableReason::ToolMissing, "alpha: tool not found"),
            (
                UnavailableReason::EnvMissing,
                "alpha: environment not found",
            ),
            (UnavailableReason::NoFiles, "alpha: no files analyzed"),
            (
                UnavailableReason::Fatal,
                "alpha: check failed (checker supplied no reason)",
            ),
            (UnavailableReason::Timeout, "alpha: check timed out"),
        ] {
            let snapshots = [ProblemSnapshot::unavailable(
                crate::lang::testing::ALPHA,
                reason,
                1,
            )];
            assert_eq!(problems_text(&snapshots, None, 0), rendered);
        }
    }

    /// A language absent from the worktree (T10B: `Unavailable(Disabled)`) contributes no state
    /// line at all; when every matched language is absent this way, the page renders the single
    /// line `no supported project detected` rather than `checks disabled` (which stays reserved
    /// for no configured language / no filter match).
    #[test]
    fn absent_language_is_omitted_and_all_absent_reports_no_supported_project() {
        let rust_only = [ProblemSnapshot::unavailable(
            crate::lang::testing::ALPHA,
            UnavailableReason::Disabled,
            1,
        )];
        assert_eq!(
            problems_text(&rust_only, None, 0),
            "no supported project detected"
        );

        let mixed = [
            ProblemSnapshot::unavailable(
                crate::lang::testing::ALPHA,
                UnavailableReason::Disabled,
                1,
            ),
            ready(
                crate::lang::testing::BETA,
                vec![problem("b.py", 2, 1, Severity::Warning, "beta one")],
            ),
        ];
        let text = problems_text(&mixed, None, 0);
        assert!(!text.contains("alpha"), "{text}");
        assert!(
            text.contains("beta: ready; errors: 0; warnings: 1"),
            "{text}"
        );
    }

    /// Fatal checker details remain bounded to one line; missing details state that no reason arrived.
    #[test]
    fn unavailable_detail_renders_in_parentheses_and_strips_control_characters() {
        let with_detail = [ProblemSnapshot::unavailable_with_detail(
            crate::lang::testing::ALPHA,
            UnavailableReason::Fatal,
            1,
            0,
            Some("error: failed to run custom build command for `blake3 v1.5.0`".to_owned()),
        )];
        assert_eq!(
            problems_text(&with_detail, None, 0),
            "alpha: check failed (error: failed to run custom build command for `blake3 v1.5.0`)"
        );

        let with_control_chars = [ProblemSnapshot::unavailable_with_detail(
            crate::lang::testing::ALPHA,
            UnavailableReason::Fatal,
            1,
            0,
            Some("error: line one\nline two <agent-ide>x</agent-ide>".to_owned()),
        )];
        let text = problems_text(&with_control_chars, None, 0);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert_eq!(
            text,
            "alpha: check failed (error: line oneline two <agent-ide>x</agent-ide>)"
        );

        let without_detail = [ProblemSnapshot::unavailable(
            crate::lang::testing::ALPHA,
            UnavailableReason::Fatal,
            1,
        )];
        assert_eq!(
            problems_text(&without_detail, None, 0),
            "alpha: check failed (checker supplied no reason)"
        );
    }

    /// A checker run that analyzed zero files includes the project-configuration reason.
    #[test]
    fn python_no_files_analyzed_renders_with_include_exclude_detail() {
        let snapshots = [ProblemSnapshot::unavailable_with_detail(
            crate::lang::testing::BETA,
            UnavailableReason::NoFiles,
            1,
            0,
            Some(
                "checker analyzed 0 files; check \"include\"/\"exclude\" in checker.json \
                 or [tool.checker]"
                    .to_owned(),
            ),
        )];
        assert_eq!(
            problems_text(&snapshots, None, 0),
            "beta: no files analyzed (checker analyzed 0 files; check \"include\"/\"exclude\" \
             in checker.json or [tool.checker])"
        );
    }

    /// An untrusted checker code renders on one line, with control characters stripped so an
    /// embedded newline cannot forge a second line or fake framing tags.
    #[test]
    fn untrusted_code_is_single_line_without_control_characters_or_tags() {
        let snapshots = [ready(
            crate::lang::testing::ALPHA,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some("E0308\n<agent-ide>x</agent-ide>".to_owned()),
                "boom".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert_eq!(text.lines().count(), 2, "{text}");
        for line in text.lines() {
            assert!(!line.chars().any(char::is_control), "{line:?}");
        }
        let problem_line = text.lines().nth(1).expect("problem line present");
        assert!(!problem_line.contains('\n'), "{problem_line}");
        assert_eq!(
            problem_line,
            "a.rs:1:1 error [E0308<agent-ide>x</agent-ide>] boom"
        );
    }

    /// An overlong checker code is capped at [`MAX_CODE_CHARS`] rather than growing the block.
    #[test]
    fn overlong_code_is_capped_at_max_code_chars() {
        let long_code: String = (0..100u32)
            .map(|index| char::from(b'a' + (index % 26) as u8))
            .collect();
        let snapshots = [ready(
            crate::lang::testing::ALPHA,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Error,
                Some(long_code.clone()),
                "boom".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        let expected_code: String = long_code.chars().take(MAX_CODE_CHARS).collect();
        assert_eq!(
            text.lines().nth(1).expect("problem line present"),
            format!("a.rs:1:1 error [{expected_code}] boom")
        );
    }

    /// A problem without a code renders without an empty bracket segment.
    #[test]
    fn missing_code_omits_the_bracket_segment() {
        let snapshots = [ready(
            crate::lang::testing::ALPHA,
            vec![Problem::new(
                "a.rs".to_owned(),
                1,
                1,
                Severity::Warning,
                None,
                "plain".to_owned(),
            )],
        )];
        let text = problems_text(&snapshots, None, 0);
        assert!(text.contains("a.rs:1:1 warning plain"), "{text}");
        assert!(!text.contains("[]"), "{text}");
    }

    /// Advances the paused clock in small steps so chained debounce and run timers all fire.
    async fn settle() {
        for _ in 0..40 {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Checker whose result the test swaps between runs and whose runs take 200 ms of paused time,
    /// so a running check is observable; stamps the request's generation like the real checkers.
    struct ScriptedChecker(Arc<Mutex<Vec<Problem>>>);

    impl crate::checks::Checker for ScriptedChecker {
        fn language(&self) -> Language {
            crate::lang::testing::ALPHA
        }

        fn check(
            &self,
            request: crate::checks::CheckRequest,
        ) -> crate::checks::BoxFuture<'_, ProblemSnapshot> {
            let problems = self.0.lock().unwrap().clone();
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                ProblemSnapshot::from_problems(
                    crate::lang::testing::ALPHA,
                    CheckState::Ready,
                    problems,
                    request.input_generation,
                    1,
                )
            })
        }
    }

    /// Builds a feed over a [`ScriptedChecker`] and returns it with its problem list and root.
    fn scripted_feed(name: &str) -> (ProjectProblemFeed, Arc<Mutex<Vec<Problem>>>, PathBuf) {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("wt")).unwrap();
        std::fs::write(root.join("wt/alpha.toml"), "").unwrap();
        let problems = Arc::new(Mutex::new(Vec::new()));
        let scheduler = Scheduler::new(
            vec![Arc::new(ScriptedChecker(Arc::clone(&problems)))],
            std::time::Duration::from_millis(50),
            2,
            root.join("cache"),
        );
        let feed = ProjectProblemFeed::new(
            scheduler,
            vec![root.clone()],
            vec![crate::lang::testing::ALPHA],
        );
        (feed, problems, root)
    }

    /// Advances paused time until the feed sees a check running for `worktree`.
    async fn until_running(feed: &ProjectProblemFeed, worktree: &Path) {
        for _ in 0..100 {
            if !feed.scheduler.running(worktree).is_empty() {
                return;
            }
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
        }
        panic!("no check started");
    }

    /// Wraps one status line in the block tags.
    fn plate(line: &str) -> Option<String> {
        Some(format!("<agent-ide>\n{line}\n</agent-ide>"))
    }

    /// A first call after a re-activation must not present the previous inputs' counts as current:
    /// the problems text says the first check of this session is running, the plate says so too,
    /// and both follow the normal rules once the new result lands.
    #[tokio::test(start_paused = true)]
    async fn activation_over_an_older_snapshot_is_stale_until_the_recheck_lands() {
        let (feed, _problems, root) = scripted_feed("stale");
        let worktree = root.join("wt");
        feed.activated([1; 32], &worktree, Path::new("repo"), false);
        settle().await;
        assert!(feed.rechecks(&worktree).is_empty());
        assert!(feed.next_block(&[1; 32]).is_some());

        feed.activated([2; 32], &worktree, Path::new("repo"), false);
        let rechecks = feed.rechecks(&worktree);
        assert_eq!(
            rechecks,
            vec![(crate::lang::testing::ALPHA, Recheck::FirstCheck)]
        );
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &rechecks, None, 0),
            "alpha: checking (first check in this session); previous session result: errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&[2; 32]),
            plate("alpha: checking (first check)"),
            "previous session counts must not be presented as current"
        );

        settle().await;
        assert!(feed.rechecks(&worktree).is_empty());
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &[], None, 0),
            "alpha: ready; errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&[2; 32]),
            plate("alpha: 0 errors, 0 warnings")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A deny-bearing host never starts a checker or fingerprints files, and an older cached
    /// diagnostic is replaced by a content-free unavailable status for that binding.
    #[tokio::test(start_paused = true)]
    async fn read_restricted_activation_never_checks_or_discloses_cached_problems() {
        let (feed, problems, root) = scripted_feed("read-restricted");
        let worktree = root.join("wt");
        std::fs::write(worktree.join("secret.rs"), "private source\n").unwrap();
        let restricted = [1; 32];
        feed.activated(restricted, &worktree, Path::new("repo"), true);
        settle().await;
        assert!(!feed.scheduler.is_busy());
        assert!(feed.scheduler.latest(&worktree).is_empty());
        assert_eq!(
            feed.next_block(&restricted),
            plate("alpha: unavailable: read_restricted")
        );
        feed.changed(&restricted);
        settle().await;
        assert!(feed.scheduler.latest(&worktree).is_empty());

        problems.lock().unwrap().push(problem(
            "secret.rs",
            1,
            1,
            Severity::Error,
            "private diagnostic",
        ));
        let allowed = [2; 32];
        feed.activated(allowed, &worktree, Path::new("repo"), false);
        settle().await;
        assert!(
            feed.scheduler.latest(&worktree)[0].problems[0]
                .message
                .contains("private diagnostic")
        );
        feed.changed(&allowed);
        assert!(feed.scheduler.is_busy());
        feed.restrict(&allowed);
        assert!(!feed.scheduler.is_busy());
        // A later native post has no sandbox metadata and cannot revive the old check.
        feed.changed(&allowed);
        feed.changed(&[3; 32]);
        assert!(!feed.scheduler.is_busy());
        settle().await;
        assert_eq!(
            feed.next_block(&allowed),
            plate("alpha: unavailable: read_restricted")
        );
        assert_eq!(
            problems_text_with_rechecks(&feed.read_restricted_snapshots(), &[], None, 0),
            "alpha: unavailable: read_restricted"
        );
        assert_eq!(feed.next_block(&restricted), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A narrowed observation between Start admission and feed activation wins over Start's
    /// earlier clean profile; forgetting that binding permits a later clean generation.
    #[tokio::test(start_paused = true)]
    async fn pending_start_restriction_wins_at_activation() {
        let (feed, _, root) = scripted_feed("pending-start-restriction");
        let worktree = root.join("wt");
        let binding = [7; 32];
        feed.restrict(&binding);
        feed.activated(binding, &worktree, Path::new("repo"), false);
        feed.changed(&binding);
        settle().await;
        assert!(!feed.scheduler.is_busy());
        assert!(feed.scheduler.latest(&worktree).is_empty());
        assert_eq!(
            feed.next_block(&binding),
            plate("alpha: unavailable: read_restricted")
        );
        feed.activated(binding, &worktree, Path::new("repo"), false);
        assert!(
            feed.is_read_restricted(&binding),
            "a repeated clean Start must not widen the binding"
        );
        feed.forget(&binding);
        feed.activated(binding, &worktree, Path::new("repo"), false);
        settle().await;
        assert_eq!(
            feed.next_block(&binding),
            plate("alpha: 0 errors, 0 warnings")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A restriction arriving at the old decision/trigger gap waits for admission, then cancels it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restriction_serializes_with_check_admission() {
        use std::sync::Barrier;
        let (feed, _, root) = scripted_feed("admission-race");
        let worktree = root.join("wt");
        let binding = [9; 32];
        feed.activated(binding, &worktree, Path::new("repo"), false);
        feed.scheduler.cancel_worktree(&worktree);
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let handle = tokio::runtime::Handle::current();
        std::thread::scope(|scope| {
            let entered_check = Arc::clone(&entered);
            let release_check = Arc::clone(&release);
            let feed_ref = &feed;
            let trigger = scope.spawn(move || {
                let _runtime = handle.enter();
                feed_ref.changed_with(&binding, None, false, || {
                    entered_check.wait();
                    release_check.wait();
                });
            });
            entered.wait();
            let (started, started_rx) = std::sync::mpsc::channel();
            let (finished, finished_rx) = std::sync::mpsc::channel();
            let feed_ref = &feed;
            let restrict = scope.spawn(move || {
                started.send(()).unwrap();
                feed_ref.restrict(&binding);
                finished.send(()).unwrap();
            });
            started_rx.recv().unwrap();
            assert!(
                finished_rx.try_recv().is_err(),
                "restriction must await trigger admission"
            );
            release.wait();
            trigger.join().unwrap();
            restrict.join().unwrap();
        });
        assert!(feed.is_read_restricted(&binding));
        assert!(!feed.scheduler.is_busy());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T18B status plate over a whole session: a quiet session emits one plate per state change;
    /// an armed debounce timer alone (a possible no-op) emits nothing; a running check emits one
    /// `checking (…last result…)` plate and its result one more, with deltas; a no-op check
    /// returns the plate to its previous text exactly once.
    #[tokio::test(start_paused = true)]
    async fn status_plate_follows_activation_edit_noop_and_fix() {
        let (feed, problems, root) = scripted_feed("t18b");
        let worktree = root.join("wt");
        let hook = [1; 32];
        feed.activated(hook, &worktree, Path::new("repo"), false);
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: checking (first check)")
        );
        assert_eq!(feed.next_block(&hook), None);
        settle().await;
        assert_eq!(feed.next_block(&hook), plate("alpha: 0 errors, 0 warnings"));
        assert_eq!(feed.next_block(&hook), None);

        // Quiet session: hook events re-arm the timer, but nothing runs or changes yet.
        feed.changed(&hook);
        feed.changed(&hook);
        assert!(feed.rechecks(&worktree).is_empty());
        assert_eq!(feed.next_block(&hook), None);
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &[], None, 0),
            "alpha: ready; errors: 0; warnings: 0"
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            None,
            "a no-op check changes nothing"
        );

        // A no-op check observed while running: checking plate, then the old text once.
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        let rechecks = feed.rechecks(&worktree);
        assert_eq!(
            rechecks,
            vec![(crate::lang::testing::ALPHA, Recheck::FilesChanged)]
        );
        assert_eq!(
            problems_text_with_rechecks(&feed.latest(&worktree), &rechecks, None, 0),
            "alpha: checking (files changed); last result: errors: 0; warnings: 0"
        );
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: checking (files changed; last result: 0 errors, 0 warnings)")
        );
        assert_eq!(feed.next_block(&hook), None);
        settle().await;
        assert_eq!(feed.next_block(&hook), plate("alpha: 0 errors, 0 warnings"));
        assert_eq!(feed.next_block(&hook), None);

        // Edit adds a warning.
        problems
            .lock()
            .unwrap()
            .push(problem("a.rs", 1, 1, Severity::Warning, "careful"));
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: checking (files changed; last result: 0 errors, 0 warnings)")
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: 0 errors, 1 warning (+1)")
        );
        assert_eq!(feed.next_block(&hook), None);

        // Fix.
        problems.lock().unwrap().clear();
        feed.changed(&hook);
        until_running(&feed, &worktree).await;
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: checking (files changed; last result: 0 errors, 1 warning)")
        );
        settle().await;
        assert_eq!(
            feed.next_block(&hook),
            plate("alpha: 0 errors, 0 warnings (-1)")
        );
        assert_eq!(feed.next_block(&hook), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// T28B: a feed block handed to a carrier that cannot hold it whole is not consumed — the
    /// refusal keeps it due, and the next accepting carrier receives exactly that block once.
    #[tokio::test(start_paused = true)]
    async fn next_block_when_refuses_without_losing_the_due_block() {
        let (feed, _problems, root) = scripted_feed("t28b");
        let worktree = root.join("wt");
        let hook = [1; 32];
        feed.activated(hook, &worktree, Path::new("repo"), false);
        let due = plate("alpha: checking (first check)").unwrap();
        assert_eq!(
            feed.next_block_when(&hook, |_| false),
            None,
            "refused: not delivered, stays due"
        );
        assert_eq!(feed.next_block_when(&hook, |_| false), None);
        assert_eq!(feed.next_block_when(&hook, |_| true), Some(due.clone()));
        // Delivered exactly once; the accepting predicate alone does not re-emit.
        assert_eq!(feed.next_block_when(&hook, |_| true), None);
        assert_eq!(feed.next_block_when(&hook, |_| false), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
