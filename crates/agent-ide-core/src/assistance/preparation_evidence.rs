//! Bounded public-check evidence for `ide.prepare`: one operator-configured test selection runs
//! through the existing support admission on the IDE's own behalf, and only the failure and
//! error blocks of that run become typed locations bound to the bytes the check observed.
//! Never a shell, a second runner, model arguments or hidden/evaluator data.

use super::super::outbox::JobEpoch;
use super::super::tests::{RunResult, SupportOrigin};
use super::*;
use crate::lang::{TestFailure, TestReport, TestTarget};
use std::os::unix::fs::MetadataExt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Longest public check a prepare may start; the configured budget never exceeds it.
pub(super) const MAX_CHECK_BUDGET: Duration = Duration::from_secs(30);
/// Largest source file hashed for one location.
const MAX_SOURCE_BYTES: u64 = 8 * 1024 * 1024;
/// Report bounds: typed failures, distinct frames, explicit errors, message bytes, block lines.
const MAX_FAILURES: usize = 64;
const MAX_FRAMES: usize = 64;
const MAX_ERRORS: usize = 32;
const MAX_MESSAGE: usize = 1024;
const MAX_BLOCK_LINES: usize = 80;
/// Retrieval handoff bounds, inside the context evidence limits (32 locations, 12 KiB).
const MAX_ANCHORS: usize = 32;
const ANCHOR_MESSAGE: usize = 300;
const ANCHOR_BUDGET: usize = 11 * 1024;

/// Trusted launcher setting (`task_context.evidence`); absent or `enabled: false` is off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EvidenceConfig {
    /// Public test selection planned by the normal language recipe.
    pub(super) target: EvidenceTarget,
    /// Process budget, 1 ms ..= [`MAX_CHECK_BUDGET`].
    pub(super) budget: Duration,
}

/// The configured public selection; worktree-relative files or a runner filter, never a path
/// outside the participant's worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EvidenceTarget {
    /// One runner invocation over these test files (at most 64).
    Files(Vec<PathBuf>),
    /// A runner filter (at most 512 bytes, no control characters).
    Pattern(String),
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    enabled: bool,
    target: RawTarget,
    budget_ms: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RawTarget {
    Files(Vec<PathBuf>),
    Pattern(String),
}

impl EvidenceConfig {
    /// Validates the operator setting: `Ok(None)` is off, `Err` names the rejected field.
    /// Reads no file, plans nothing and never accepts model-supplied values.
    pub(super) fn parse(raw: Option<&Value>) -> Result<Option<Self>, &'static str> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let raw: RawConfig =
            serde_json::from_value(raw.clone()).map_err(|_| "evidence_config_invalid")?;
        if !raw.enabled {
            return Ok(None);
        }
        if !(1..=MAX_CHECK_BUDGET.as_millis() as u64).contains(&raw.budget_ms) {
            return Err("evidence_budget_invalid");
        }
        let target = match raw.target {
            RawTarget::Files(paths) => {
                if paths.is_empty() || paths.len() > 64 || !paths.iter().all(|p| public_relative(p))
                {
                    return Err("evidence_files_invalid");
                }
                EvidenceTarget::Files(paths)
            }
            RawTarget::Pattern(pattern) => {
                if pattern.trim().is_empty()
                    || pattern.len() > 512
                    || pattern.chars().any(char::is_control)
                {
                    return Err("evidence_pattern_invalid");
                }
                EvidenceTarget::Pattern(pattern)
            }
        };
        Ok(Some(Self {
            target,
            budget: Duration::from_millis(raw.budget_ms),
        }))
    }

    /// The language-neutral selection handed to the normal test planner. One file is the
    /// ordinary single-file target, which every runner plans; several stay one `Files` run.
    fn test_target(&self) -> TestTarget {
        match &self.target {
            EvidenceTarget::Files(paths) if paths.len() == 1 => TestTarget::File(paths[0].clone()),
            EvidenceTarget::Files(paths) => TestTarget::Files(paths.clone()),
            EvidenceTarget::Pattern(pattern) => TestTarget::Pattern(pattern.clone()),
        }
    }
}

/// One started support run owned by exactly one prepare (binding, authority, task, epoch).
pub(super) struct PublicCheck {
    /// Daemon-unique run id.
    pub(super) run_id: u64,
    /// Authorized worktree the run uses.
    root: PathBuf,
    /// Immutable ownership; cancellation and collection require an exact match.
    origin: SupportOrigin,
    /// Bounded ledger slot reserved for this run, released once collected.
    reference: String,
    /// Wall clock taken immediately before admission; later-changed sources are not evidence.
    before: SystemTime,
}

/// Admission outcome; nothing but `Started` spawned a process.
pub(super) enum CheckStart {
    /// The configured check runs under support admission.
    Started(PublicCheck),
    /// Another run holds this worktree (the main agent's test has priority); never preempted.
    Busy { running: u64 },
    /// No check could start; the reason is a stable label for `needs_evidence`.
    Unavailable(&'static str),
}

/// What the settled run proves; only `Passed` is a clean run, and it carries no anchors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CheckOutcome {
    /// Exit 0, complete summary, no failure and no failure block.
    Passed,
    /// At least one failed test or failure block was observed.
    Failed,
    /// Killed by its budget before completing; whatever was parsed stays partial.
    BudgetExhausted,
    /// The prepare's own cancellation won.
    Cancelled,
    /// Nonzero exit or missing summary without a parsed failure: unknown, never success.
    Incomplete,
}

/// The kind of block a failure came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum FailureKind {
    /// A test the runner reported as failed.
    Test,
    /// A runner `FAIL` header without a test name (file/suite failed to load or run).
    Suite,
    /// A runner section of errors outside any test.
    Unhandled,
}

/// One source location observed in a failure block, bound to the bytes present during the run.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct SourceFrame {
    /// Worktree-relative regular file.
    pub(super) path: PathBuf,
    /// 1-based line, inside the file.
    pub(super) line: u32,
    /// blake3 hex of the file, unchanged since before the run started.
    pub(super) source_hash: String,
    /// The language names this path a test file.
    pub(super) test: bool,
    /// Index into [`PublicCheckReport::failures`] of the first block naming it.
    pub(super) failure: usize,
    /// How many failure blocks named this exact location.
    pub(super) count: u32,
}

/// One failed test, suite or error section.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct FailedTest {
    pub(super) kind: FailureKind,
    /// Runner test name (`suite > test`), or the bounded block header for suites/sections.
    pub(super) name: String,
    /// Test file of this failure when the block or location names one.
    pub(super) test_file: Option<PathBuf>,
    /// The runner's own typed location, when it resolves to a current-at-run source.
    pub(super) location: Option<SourceFrame>,
    /// First error line, bounded.
    pub(super) message: String,
}

/// Typed, failure-only report of one settled public check.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(super) struct PublicCheckReport {
    pub(super) run_id: u64,
    /// Actual replay argv of the run.
    pub(super) command: Vec<String>,
    pub(super) outcome: CheckOutcome,
    pub(super) exit: Option<i32>,
    pub(super) elapsed_ms: u64,
    pub(super) passed: u32,
    pub(super) failed: u32,
    pub(super) failures: Vec<FailedTest>,
    /// Distinct locations from failure blocks only, first-seen order.
    pub(super) frames: Vec<SourceFrame>,
    /// Block locations that named no current worktree source (dependencies, outside paths).
    pub(super) unresolved_frames: u32,
    /// Explicit gaps: incomplete summary, cut output, changed sources, missing blocks.
    pub(super) errors: Vec<String>,
}

/// One planned, not yet started check: the module-admitted recipe and the exact authority it
/// was planned under. Planning spawns nothing; [`admit_public_check`] rechecks before spawning.
pub(super) struct PlannedCheck {
    authority: AuthorityStamp,
    source_epoch: JobEpoch,
    task: [u8; 32],
    root: PathBuf,
    spec: crate::checks::runner::RunSpec,
    argv: Vec<String>,
    language: Language,
    budget: Duration,
}

/// Plans the configured check for a writer prepare through the normal recipe
/// (`test_selection`); only a module-admitted spec is accepted. Spawns and reserves nothing, so
/// it may run outside any admission gate. Reader authority or a moved source epoch plan nothing.
pub(super) async fn plan_public_check(
    shared: &Shared,
    authority: &AuthorityStamp,
    source_epoch: JobEpoch,
    task: &str,
    config: &EvidenceConfig,
    launch_target: &LaunchTarget,
) -> Result<PlannedCheck, &'static str> {
    if authority.role() != crate::workspace::authority::StartRole::Writer {
        return Err("evidence_requires_writer");
    }
    if task.is_empty() {
        return Err("evidence_task_missing");
    }
    if !current(shared, authority, source_epoch) {
        return Err("source_changed_before_check");
    }
    let root = authority.worktree().worktree_path().to_owned();
    let (argv, language, spec) = match test_selection(
        &root,
        config.test_target(),
        config.budget.min(MAX_CHECK_BUDGET),
        &shared.launcher,
        launch_target,
    )
    .await
    {
        Ok(Ok((argv, language, _, Some(crate::modules::calls::PlannedStart::Admitted(spec))))) => {
            (argv, language, spec)
        }
        Ok(Ok((_, _, _, Some(crate::modules::calls::PlannedStart::MissingTool(_))))) => {
            return Err("test_tool_missing");
        }
        Ok(Ok(_)) => return Err("in_process_runner_unsupported"),
        Ok(Err(_)) => return Err("test_selection_unavailable"),
        Err(_) => return Err("language_module_unavailable"),
    };
    Ok(PlannedCheck {
        authority: authority.clone(),
        source_epoch,
        task: *blake3::hash(task.as_bytes()).as_bytes(),
        root,
        spec,
        argv,
        language,
        budget: config.budget,
    })
}

/// Synchronously starts a planned check: rechecks the active binding and source epoch, reserves
/// a ledger slot and spawns through support admission. Never awaits, so a caller may hold a
/// plain mutex across it to make admission and registration one step.
pub(super) fn admit_public_check(shared: &Shared, planned: PlannedCheck) -> CheckStart {
    if !current(shared, &planned.authority, planned.source_epoch) {
        return CheckStart::Unavailable("source_changed_before_check");
    }
    let Some(reference) = reserve(shared, &planned.authority, planned.source_epoch) else {
        return CheckStart::Unavailable("details_full");
    };
    let origin = SupportOrigin {
        binding: planned.authority.binding().clone(),
        authority_epoch: planned.authority.epoch(),
        task: planned.task,
        source_epoch: planned.source_epoch,
    };
    let start = admit(
        &shared.test_runs,
        planned.root,
        planned.spec,
        planned.argv,
        planned.language,
        planned.budget,
        reference.clone(),
        origin,
    );
    if !matches!(start, CheckStart::Started(_)) {
        release(shared, &reference);
    }
    start
}

/// The binding is active and its source epoch is still the planned one.
fn current(shared: &Shared, authority: &AuthorityStamp, source_epoch: JobEpoch) -> bool {
    let binding = authority.binding();
    shared.active(binding).is_ok() && shared.outbox.current_epoch(binding) == source_epoch
}

/// `None` while the run is live. Once settled: the failure-only report, with every hash
/// withdrawn when the binding's source epoch moved during the run; the ledger slot is released.
pub(super) fn poll_public_check(shared: &Shared, check: &PublicCheck) -> Option<PublicCheckReport> {
    let mut report = collect(&shared.test_runs, check)?;
    if shared.outbox.current_epoch(&check.origin.binding) != check.origin.source_epoch {
        report.withdraw_sources("source_epoch_changed_during_check");
    }
    release(shared, &check.reference);
    Some(report)
}

/// Requests cancellation of this prepare's own run only; any other run, including the main
/// agent's, is never touched. The caller still polls to observe settlement and release.
pub(super) fn cancel_public_check(shared: &Shared, check: &PublicCheck) -> bool {
    cancel(&shared.test_runs, check)
}

/// Spawns through `start_support_admitted`; the clock is read before the spawn.
#[allow(
    clippy::too_many_arguments,
    reason = "the existing admitted recipe boundary plus typed support origin"
)]
fn admit(
    runs: &TestRuns,
    root: PathBuf,
    spec: crate::checks::runner::RunSpec,
    argv: Vec<String>,
    language: Language,
    budget: Duration,
    reference: String,
    origin: SupportOrigin,
) -> CheckStart {
    let before = SystemTime::now();
    let owner = origin.binding.clone();
    match runs.start_support_admitted(
        root.clone(),
        spec,
        argv,
        &owner,
        language,
        budget.min(MAX_CHECK_BUDGET),
        reference.clone(),
        origin.clone(),
    ) {
        StartResult::Started(run_id) => CheckStart::Started(PublicCheck {
            run_id,
            root,
            origin,
            reference,
            before,
        }),
        StartResult::Running(running, _) => CheckStart::Busy { running },
        StartResult::Failed { .. } => CheckStart::Unavailable("check_start_failed"),
    }
}

/// Reads the settled result as its owner (marking it observed) and reduces it.
fn collect(runs: &TestRuns, check: &PublicCheck) -> Option<PublicCheckReport> {
    let result = runs
        .get(&check.root, check.run_id, &check.origin.binding)?
        .result?;
    Some(extract(&check.root, check.run_id, &result, check.before))
}

/// Cancels only when the registry's immutable origin equals this check's origin.
fn cancel(runs: &TestRuns, check: &PublicCheck) -> bool {
    let own = runs.support_origin(check.run_id).is_some_and(|origin| {
        origin.binding == check.origin.binding
            && origin.authority_epoch == check.origin.authority_epoch
            && origin.task == check.origin.task
            && origin.source_epoch == check.origin.source_epoch
    });
    own && runs.cancel(&check.root, check.run_id, &check.origin.binding) == Some(true)
}

/// Reserves an ordinary bounded result slot without evicting main-agent work; full means none.
fn reserve(shared: &Shared, authority: &AuthorityStamp, epoch: JobEpoch) -> Option<String> {
    let mut ledger = shared.ledger.lock().ok()?;
    if ledger.details.len() >= shared.launcher.limits.details {
        return None;
    }
    ledger.next = ledger.next.checked_add(1)?;
    let reference = format!(
        "{}-{}",
        blake3::Hash::from_bytes(shared.nonce).to_hex(),
        ledger.next
    );
    ledger.details.insert(
        reference.clone(),
        Detail {
            progress: Arc::new(Mutex::new(progress::Progress::default())),
            binding: authority.binding().clone(),
            reply: PeerReply::Pending {
                detail_ref: reference.clone(),
            },
            selection: (AssistanceTool::Prepare, [0; 32]),
            authority: Some(authority.clone()),
            source: None,
            extra_sources: Vec::new(),
            native_epoch: epoch.native_epoch,
            line_movement: None,
            diff_page: None,
            diff_page_fresh: false,
            context_page: None,
            test_replay: None,
            context_page_fresh: false,
            diff_provenance: Some(BTreeSet::new()),
        },
    );
    Some(reference)
}

/// Drops this check's ledger slot; the run's result stays in the test registry.
fn release(shared: &Shared, reference: &str) {
    if let Ok(mut ledger) = shared.ledger.lock() {
        ledger.details.remove(reference);
    }
}

impl PublicCheckReport {
    /// Removes every source binding after a broken fence, keeping names and messages.
    fn withdraw_sources(&mut self, reason: &str) {
        for failure in &mut self.failures {
            failure.location = None;
        }
        self.frames.clear();
        self.push_error(reason.to_owned());
    }

    /// Appends one bounded explicit error; overflow is counted, never silent.
    fn push_error(&mut self, error: String) {
        push_error(&mut self.errors, error);
    }

    /// Converts to the context-retrieval evidence: failure locations then frames, each bound to
    /// its observed hash, at most 32 entries inside the 12 KiB budget. `None` without anchors.
    pub(super) fn retrieval_evidence(
        &self,
    ) -> Option<crate::project::context::evidence::PublicEvidence> {
        use crate::project::context::evidence::{FailureEvidence, PublicEvidence};
        let mut failures: Vec<FailureEvidence> = Vec::new();
        let mut used = 0;
        let mut seen = BTreeSet::new();
        let candidates = self
            .failures
            .iter()
            .filter_map(|failure| {
                failure
                    .location
                    .as_ref()
                    .map(|frame| (frame, failure.message.as_str()))
            })
            .chain(self.frames.iter().map(|frame| {
                let failure = &self.failures[frame.failure];
                (frame, failure.name.as_str())
            }));
        for (frame, message) in candidates {
            if failures.len() == MAX_ANCHORS {
                break;
            }
            if !seen.insert((frame.path.clone(), frame.line)) {
                continue;
            }
            let message = bounded(message, ANCHOR_MESSAGE);
            let cost = frame.path.as_os_str().len() + frame.source_hash.len() + message.len() + 96;
            if used + cost > ANCHOR_BUDGET {
                break;
            }
            used += cost;
            failures.push(FailureEvidence {
                path: frame.path.clone(),
                content_hash: frame.source_hash.clone(),
                line: Some(frame.line),
                message,
            });
        }
        (!failures.is_empty()).then(|| PublicEvidence {
            run_id: format!("tests-{}", self.run_id),
            failures,
        })
    }
}

/// Reduces one settled run to failure-only evidence. Locations come only from failure/error
/// blocks and the runner's typed failures; a source counts only when it is a regular worktree
/// file whose inode times precede `before` and stay unchanged while it is hashed.
fn extract(root: &Path, run_id: u64, result: &RunResult, before: SystemTime) -> PublicCheckReport {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_owned());
    let report = &result.report;
    let text = strip_ansi(&result.output);
    let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
    let mut out = PublicCheckReport {
        run_id,
        command: result.command.clone(),
        outcome: CheckOutcome::Incomplete,
        exit: result.exit,
        elapsed_ms: u64::try_from(result.elapsed.as_millis()).unwrap_or(u64::MAX),
        passed: report.passed,
        failed: report.failed,
        failures: Vec::new(),
        frames: Vec::new(),
        unresolved_frames: 0,
        errors: Vec::new(),
    };
    if result.output_paged {
        out.push_error("output_paged; failure blocks unavailable".into());
    } else if result.output.starts_with("[cut]") {
        out.push_error("output_head_cut; earlier failure blocks unavailable".into());
    }
    if report.incomplete {
        out.push_error("runner_summary_missing".into());
    }
    let mut sources = Sources::new(&root, before);
    let blocks = failure_blocks(&lines, report, &root, &mut out.errors);
    for block in blocks.iter().take(MAX_FAILURES) {
        let index = out.failures.len();
        let typed = block.failure.map(|i| &report.failures[i]);
        let location = typed
            .and_then(|failure| failure.location.as_ref())
            .and_then(|(path, line)| {
                let raw = path.to_string_lossy();
                sources.frame(&raw, *line, index, &mut out)
            });
        let mut test_file = location.as_ref().filter(|f| f.test).map(|f| f.path.clone());
        for (raw, line) in block_tokens(&lines[block.start..block.end]) {
            match line {
                Some(line) => {
                    let Some(frame) = sources.frame(raw, line, index, &mut out) else {
                        continue;
                    };
                    if frame.test && test_file.is_none() {
                        test_file = Some(frame.path.clone());
                    }
                    if let Some(kept) = out
                        .frames
                        .iter_mut()
                        .find(|kept| kept.path == frame.path && kept.line == frame.line)
                    {
                        kept.count += 1;
                    } else if out.frames.len() < MAX_FRAMES {
                        out.frames.push(frame);
                    }
                }
                None if test_file.is_none() => {
                    test_file = worktree_relative(&root, raw)
                        .filter(|p| is_test_file(p) && root.join(p).is_file());
                }
                None => {}
            }
        }
        out.failures.push(FailedTest {
            kind: block.kind,
            name: typed.map_or_else(
                || bounded(header_title(lines[block.start]), 256),
                |failure| bounded(&failure.name, 256),
            ),
            test_file,
            location,
            message: typed.map_or_else(
                || block_message(&lines[block.start + 1..block.end]),
                |failure| bounded(&failure.message, MAX_MESSAGE),
            ),
        });
    }
    if blocks.len() > MAX_FAILURES {
        out.push_error(format!(
            "failures_truncated: {} more",
            blocks.len() - MAX_FAILURES
        ));
    }
    out.outcome = if result.cancelled {
        CheckOutcome::Cancelled
    } else if result.stopped {
        CheckOutcome::BudgetExhausted
    } else if report.failed > 0 || !out.failures.is_empty() {
        CheckOutcome::Failed
    } else if result.exit == Some(0) && !report.incomplete {
        CheckOutcome::Passed
    } else {
        CheckOutcome::Incomplete
    };
    match out.outcome {
        CheckOutcome::BudgetExhausted => out.push_error("budget_exhausted; report partial".into()),
        CheckOutcome::Cancelled => out.push_error("cancelled; report partial".into()),
        CheckOutcome::Incomplete => {
            out.push_error("unclean_run_without_parsed_failures".into());
        }
        CheckOutcome::Failed if out.failures.is_empty() => {
            out.push_error("failure_blocks_not_found".into());
        }
        _ => {}
    }
    out
}

/// One failure or error block: header line `start`, body up to `end` (exclusive).
struct Block {
    kind: FailureKind,
    start: usize,
    end: usize,
    /// Index into the runner's typed failures.
    failure: Option<usize>,
}

/// Finds blocks: each typed failure at the occurrence of its name opening the block with the
/// most locations (progress and summary lines open empty blocks), header-only `FAIL` lines,
/// and each unhandled-error rule. Passing tests never open a block; blocks stop at passes.
fn failure_blocks(
    lines: &[&str],
    report: &TestReport,
    root: &Path,
    errors: &mut Vec<String>,
) -> Vec<Block> {
    let mut starts: BTreeMap<usize, (FailureKind, Option<usize>)> = BTreeMap::new();
    for (index, failure) in report.failures.iter().enumerate() {
        match typed_header(lines, failure, root) {
            Some(line) => {
                starts.insert(line, (FailureKind::Test, Some(index)));
            }
            None => push_error(
                errors,
                format!("failure_block_not_found: {}", bounded(&failure.name, 128)),
            ),
        }
    }
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("FAIL ") {
            starts.entry(index).or_insert((FailureKind::Suite, None));
        } else if is_rule(trimmed)
            && (trimmed.contains("Unhandled ") || trimmed.contains("Uncaught "))
        {
            // The section rule and each per-error rule (`Unhandled Rejection`, `Uncaught
            // Exception`, `Unhandled Error`) open blocks; only the plural section summary
            // without any location is dropped below.
            starts
                .entry(index)
                .or_insert((FailureKind::Unhandled, None));
        }
    }
    let heads: Vec<usize> = starts.keys().copied().collect();
    starts
        .into_iter()
        .filter_map(|(start, (kind, failure))| {
            let end = block_end(lines, start, &heads);
            // A bare `FAIL file` header followed directly by named blocks is a grouping line.
            let empty = lines[start + 1..end]
                .iter()
                .all(|line| line.trim().is_empty());
            let summary = kind == FailureKind::Unhandled
                && header_title(lines[start]).eq_ignore_ascii_case("Unhandled Errors")
                && !block_tokens(&lines[start + 1..end]).any(|(_, line)| line.is_some());
            (failure.is_some() || (!empty && !summary)).then_some(Block {
                kind,
                start,
                end,
                failure,
            })
        })
        .collect()
}

/// The header line of one typed failure, preferring the occurrence whose block has locations.
fn typed_header(lines: &[&str], failure: &TestFailure, root: &Path) -> Option<usize> {
    if failure.name.trim().is_empty() {
        return None;
    }
    let alternate = failure.name.replace(" > ", " › ");
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(&failure.name) || line.contains(&alternate))
        .map(|(index, _)| {
            let end = block_end(lines, index, &[]);
            let located = block_tokens(&lines[index + 1..end])
                .filter(|(raw, line)| line.is_some() && worktree_relative(root, raw).is_some())
                .count();
            (located, index)
        })
        .max()
        .map(|(_, index)| index)
}

/// First line after `start` that opens another block, is a rule, a pass or a summary line.
fn block_end(lines: &[&str], start: usize, heads: &[usize]) -> usize {
    let limit = lines.len().min(start + 1 + MAX_BLOCK_LINES);
    (start + 1..limit)
        .find(|&index| {
            let trimmed = lines[index].trim();
            heads.binary_search(&index).is_ok()
                || is_rule(trimmed)
                || ["✓", "✔", "√", "PASS ", "ok "]
                    .iter()
                    .any(|marker| trimmed.starts_with(marker))
                || trimmed.ends_with(" ... ok")
                || [
                    "Test Files",
                    "Tests ",
                    "Tests:",
                    "Test Suites:",
                    "test result:",
                    "failures:",
                    "Duration ",
                    "Ran ",
                ]
                .iter()
                .any(|marker| trimmed.starts_with(marker))
        })
        .unwrap_or(limit)
}

/// A block header without surrounding rule characters (`⎯⎯ Unhandled Rejection ⎯⎯`).
fn header_title(line: &str) -> &str {
    line.trim()
        .trim_matches(|c| matches!(c, '⎯' | '═' | '━' | '─' | '-' | '=' | '_' | '~'))
        .trim()
}

/// A separator: at least three leading rule characters.
fn is_rule(trimmed: &str) -> bool {
    let mut chars = trimmed.chars();
    (0..3).all(|_| {
        chars
            .next()
            .is_some_and(|c| matches!(c, '⎯' | '═' | '━' | '─' | '-' | '=' | '_' | '~'))
    })
}

/// `path:line[:col]` tokens (with `Some(line)`) and bare path-looking tokens (`None`).
fn block_tokens<'a>(lines: &'a [&'a str]) -> impl Iterator<Item = (&'a str, Option<u32>)> + 'a {
    lines.iter().flat_map(|line| {
        line.split(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '(' | ')' | '[' | ']' | '"' | '\'' | '`' | ',' | '<' | '>'
                )
        })
        .filter_map(|token| {
            let token = token.strip_prefix("file://").unwrap_or(token);
            let token = token.trim_end_matches([':', '.', ';', ',']);
            if token.is_empty() {
                return None;
            }
            match location_token(token) {
                Some((path, line)) => Some((path, Some(line))),
                None => {
                    let name = token.rsplit('/').next().unwrap_or(token);
                    (name.contains('.') && !token.contains(':')).then_some((token, None))
                }
            }
        })
    })
}

/// Splits `path:line` or `path:line:col`; the file name must have an extension.
fn location_token(token: &str) -> Option<(&str, u32)> {
    let (rest, last) = token.rsplit_once(':')?;
    let last: u32 = last.parse().ok()?;
    let (path, line) = match rest.rsplit_once(':') {
        Some((path, line)) => match line.parse::<u32>() {
            Ok(line) => (path, line),
            Err(_) => (rest, last),
        },
        None => (rest, last),
    };
    let name = path.rsplit('/').next()?;
    (line > 0 && name.contains('.') && !name.starts_with('.')).then_some((path, line))
}

/// Worktree-relative public path of an output token; absolute paths must lie under `root`.
fn worktree_relative(root: &Path, raw: &str) -> Option<PathBuf> {
    let path = Path::new(raw);
    let relative = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };
    public_relative(&relative).then_some(relative)
}

/// Plain relative components only; no hidden, VCS, IDE-state or dependency directory.
fn public_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path.components().all(|part| match part {
            std::path::Component::Normal(name) => name
                .to_str()
                .is_some_and(|name| !name.starts_with('.') && name != "node_modules"),
            _ => false,
        })
}

/// Whether the path's language calls it a test file.
fn is_test_file(path: &Path) -> bool {
    Language::for_path(path).is_some_and(|language| language.support().is_test_file(path))
}

/// Per-path source observations, each read once per report.
struct Sources<'a> {
    root: &'a Path,
    before: SystemTime,
    seen: BTreeMap<PathBuf, Result<(String, usize), &'static str>>,
}

impl<'a> Sources<'a> {
    fn new(root: &'a Path, before: SystemTime) -> Self {
        Self {
            root,
            before,
            seen: BTreeMap::new(),
        }
    }

    /// A frame for `raw:line` bound to its observed hash; unresolved or changed sources are
    /// counted or recorded in `out` and yield `None`.
    fn frame(
        &mut self,
        raw: &str,
        line: u32,
        failure: usize,
        out: &mut PublicCheckReport,
    ) -> Option<SourceFrame> {
        let Some(path) = worktree_relative(self.root, raw) else {
            out.unresolved_frames += 1;
            return None;
        };
        let (root, before) = (self.root, self.before);
        let observed = self
            .seen
            .entry(path.clone())
            .or_insert_with(|| observe(root, &path, before));
        match observed {
            Ok((hash, lines)) if line as usize <= *lines => Some(SourceFrame {
                test: is_test_file(&path),
                path,
                line,
                source_hash: hash.clone(),
                failure,
                count: 1,
            }),
            Ok(_) => {
                out.push_error(format!(
                    "frame_line_outside_source: {}:{line}",
                    path.display()
                ));
                None
            }
            Err("source_unavailable") => {
                out.unresolved_frames += 1;
                None
            }
            Err(reason) => {
                let error = format!("{reason}: {}", path.display());
                if !out.errors.contains(&error) {
                    out.push_error(error);
                }
                None
            }
        }
    }
}

/// Hashes one regular worktree file whose change and modification times precede `before` and
/// whose identity, size and times are equal before and after the read.
fn observe(
    root: &Path,
    relative: &Path,
    before: SystemTime,
) -> Result<(String, usize), &'static str> {
    let path = root.join(relative);
    let first = std::fs::symlink_metadata(&path).map_err(|_| "source_unavailable")?;
    if !first.file_type().is_file() {
        return Err("source_not_regular_file");
    }
    if first.len() > MAX_SOURCE_BYTES {
        return Err("source_too_large");
    }
    let canonical = path.canonicalize().map_err(|_| "source_unavailable")?;
    if !canonical.starts_with(root) {
        return Err("source_outside_worktree");
    }
    if !predates(&first, before) {
        return Err("source_changed_during_check");
    }
    let bytes = std::fs::read(&path).map_err(|_| "source_unavailable")?;
    let second = std::fs::symlink_metadata(&path).map_err(|_| "source_unavailable")?;
    if bytes.len() as u64 != first.len() || !same_inode_state(&first, &second) {
        return Err("source_changed_during_check");
    }
    let lines = String::from_utf8_lossy(&bytes).lines().count();
    Ok((blake3::hash(&bytes).to_hex().to_string(), lines))
}

/// Both inode change time and content modification time are strictly before `before`.
fn predates(meta: &std::fs::Metadata, before: SystemTime) -> bool {
    let changed = u64::try_from(meta.ctime())
        .ok()
        .zip(u32::try_from(meta.ctime_nsec()).ok())
        .map(|(secs, nanos)| UNIX_EPOCH + Duration::new(secs, nanos));
    changed.is_some_and(|changed| changed < before)
        && meta.modified().is_ok_and(|modified| modified < before)
}

/// Same file identity, size and times.
fn same_inode_state(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    (
        a.dev(),
        a.ino(),
        a.len(),
        a.mtime(),
        a.mtime_nsec(),
        a.ctime(),
        a.ctime_nsec(),
    ) == (
        b.dev(),
        b.ino(),
        b.len(),
        b.mtime(),
        b.mtime_nsec(),
        b.ctime(),
        b.ctime_nsec(),
    )
}

/// First error-looking line of a block body, else its first non-empty line.
fn block_message(body: &[&str]) -> String {
    let error = |line: &&&str| {
        let line = line.to_ascii_lowercase();
        ["error", "assert", "panicked", "expected", "exception"]
            .iter()
            .any(|word| line.contains(word))
    };
    body.iter()
        .find(error)
        .or_else(|| body.iter().find(|line| !line.trim().is_empty()))
        .map_or_else(String::new, |line| bounded(line.trim(), MAX_MESSAGE))
}

/// Removes ANSI CSI sequences.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
        }
    }
    out
}

/// At most `limit` bytes on a character boundary.
fn bounded(text: &str, limit: usize) -> String {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Appends one error up to [`MAX_ERRORS`]; the last slot counts the overflow.
fn push_error(errors: &mut Vec<String>, error: String) {
    if errors.len() + 1 < MAX_ERRORS {
        errors.push(error);
    } else if errors.len() + 1 == MAX_ERRORS {
        errors.push("errors_truncated".into());
    }
}

/// Offline access for sibling admission tests: a real support start of `/bin/sh -c script`.
#[cfg(test)]
pub(super) fn fixture_admit(
    runs: &TestRuns,
    root: &Path,
    script: &str,
    owner: &BindingRef,
    task: &str,
) -> CheckStart {
    admit(
        runs,
        root.to_owned(),
        tests::shell(root, script),
        vec!["public-check".into()],
        crate::lang::testing::ALPHA,
        Duration::from_secs(10),
        format!("evidence-{task}"),
        tests::origin(owner, task),
    )
}

/// Offline cancellation of an own fixture check through the same origin match.
#[cfg(test)]
pub(super) fn fixture_cancel(runs: &TestRuns, check: &PublicCheck) -> bool {
    cancel(runs, check)
}

/// Offline settled report of a fixture check, `None` while it runs.
#[cfg(test)]
pub(super) fn fixture_settled(runs: &TestRuns, check: &PublicCheck) -> Option<PublicCheckReport> {
    collect(runs, check)
}

#[cfg(test)]
#[path = "preparation_evidence/tests.rs"]
mod tests;
