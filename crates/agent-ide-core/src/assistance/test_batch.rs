//! Several test targets as ONE run of a worktree (batch-api-v1 §6).
//!
//! The worker plans the targets into runner invocations (symbol targets of one language share
//! one invocation, every other target gets its own) and hands them here. The registry holds the
//! worktree's single live run slot for the whole sequence: the invocations run one after another
//! under one run id, one budget and one retained result, whose report sums every invocation's
//! counts and whose per-target lines say what each target ran and how it ended.

use super::*;

/// How one invocation of a several-target run starts.
pub enum BatchCommand {
    /// Exactly a language's admitted test recipe (see [`TestRuns::start_admitted`]).
    Admitted(crate::checks::runner::RunSpec),
    /// A selected runner command started from the worktree root in its resolved command
    /// environment (see [`TestRuns::start_with_options`]).
    Resolved(CommandResolution),
    /// The language's runner is missing; the invocation reports this error and starts nothing.
    Missing(String),
}

/// One runner invocation of a several-target run.
pub struct BatchLaunch {
    /// Zero-based indexes of the targets this invocation runs.
    pub targets: Vec<usize>,
    /// Language whose parser reads the invocation's output.
    pub language: Language,
    /// The invocation's argv (the admitted recipe's shown command, or the argv to start).
    pub argv: Vec<String>,
    /// How it starts.
    pub start: BatchCommand,
}

/// One target of a several-target run, as its line names it.
pub struct BatchTarget {
    /// `symbol <address>`, `path <path>` or `pattern <filter>`.
    pub label: String,
    /// The target's whole outcome when it runs nothing (`no tests reference …`, a refusal), else
    /// `None`: the invocation that runs it supplies the outcome.
    pub settled: Option<String>,
}

/// The facts of a several-target run that are known before it starts.
pub struct BatchPlan {
    /// Every target in request order.
    pub targets: Vec<BatchTarget>,
    /// The invocations in run order; at least one.
    pub launches: Vec<BatchLaunch>,
    /// The rerun line's argv reproducing the whole run, and its worktree-relative directory.
    pub rerun: (Vec<String>, Option<String>),
    /// The whole run's budget; each invocation gets what the earlier ones left.
    pub budget: Duration,
    /// Detail reference that retains the combined output.
    pub detail_ref: String,
}

/// How one invocation of a several-target run ended.
enum Ran {
    /// It ran; its parsed result.
    Result(Box<RunResult>),
    /// It could not start.
    NotStarted(String),
    /// The budget was spent before its turn.
    Skipped,
}

impl TestRuns {
    /// Starts the several-target run `plan` in `root` unless that worktree already has a live
    /// run, holding the worktree's single run slot until its last invocation settles.
    ///
    /// Returns [`StartResult::Running`] with the live run when the worktree is busy, and
    /// [`StartResult::Failed`] when the worktree's cache lease cannot be taken; a single
    /// invocation that cannot start is reported on its targets' lines instead.
    pub fn start_batch(&self, root: PathBuf, owner: &BindingRef, plan: BatchPlan) -> StartResult {
        let mut state = match self.0.lock() {
            Ok(state) => state,
            Err(error) => {
                return StartResult::Failed {
                    error: error.to_string(),
                    not_found: false,
                };
            }
        };
        if let Some((id, job)) = state
            .jobs
            .iter()
            .find(|(_, job)| job.root == root && job.result.is_none())
        {
            return StartResult::Running(*id, job.started.elapsed());
        }
        let Some(lease) = crate::retention::Lease::for_worktree(&root) else {
            return StartResult::Failed {
                error: "the worktree's cache retention lease could not be taken".to_owned(),
                not_found: false,
            };
        };
        let lease = crate::retention::SettledLease::new(Some(lease));
        state.next_id = state.next_id.saturating_add(1);
        let id = state.next_id;
        state.jobs.insert(
            id,
            Job {
                root: root.clone(),
                owner: owner.fingerprint(),
                channel: owner.channel_identity().fingerprint(),
                detail_ref: plan.detail_ref.clone(),
                command: plan.rerun.0.clone(),
                explicit_command: false,
                started: tokio::time::Instant::now(),
                result: None,
                completed_at: None,
                observed: false,
            },
        );
        drop(state);
        let registry = self.0.clone();
        let admission = self.1.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            let mut ran = Vec::with_capacity(plan.launches.len());
            for launch in &plan.launches {
                let remaining = plan.budget.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    ran.push(Ran::Skipped);
                    continue;
                }
                let (spawned, budget, cap) = match &launch.start {
                    BatchCommand::Admitted(spec) => (
                        spawn_spec(&admission, &root, spec),
                        remaining.min(spec.timeout),
                        spec.max_output_bytes.min(MAX_OUTPUT),
                    ),
                    BatchCommand::Resolved(resolution) => (
                        spawn_resolved(
                            &admission,
                            &root,
                            &root,
                            &launch.argv,
                            &[],
                            resolution.resolved.as_ref(),
                        ),
                        remaining,
                        MAX_OUTPUT,
                    ),
                    BatchCommand::Missing(error) => (
                        Err(io::Error::new(io::ErrorKind::NotFound, error.clone())),
                        remaining,
                        MAX_OUTPUT,
                    ),
                };
                ran.push(match spawned {
                    Ok(child) => Ran::Result(Box::new(
                        run_child_capped(launch.language, budget, child, &root, cap).await,
                    )),
                    Err(error) => Ran::NotStarted(error.to_string()),
                });
            }
            lease.settled();
            let result = combine(&plan, &ran, started.elapsed());
            let mut on_settle = None;
            if let Ok(mut state) = registry.lock()
                && let Some(job) = state.jobs.get_mut(&id)
            {
                job.result = Some(result);
                job.completed_at = Some(tokio::time::Instant::now());
                on_settle = state.on_settle.clone();
            }
            if let Some(on_settle) = on_settle {
                on_settle(id);
            }
            prune_completed(&registry, &root);
        });
        StartResult::Started(id)
    }
}

/// The one result of a several-target run: every invocation's counts and failures summed,
/// stopped when any invocation was stopped or skipped, the first non-zero exit (else 0, or none
/// when an invocation has none), every invocation's output behind its `== run <k>` header (the
/// last [`MAX_OUTPUT`] bytes kept), the plan's rerun line, and one line per target.
fn combine(plan: &BatchPlan, ran: &[Ran], elapsed: Duration) -> RunResult {
    let mut report = TestReport::default();
    let mut output = String::new();
    let mut stopped = false;
    let mut exit = Some(0);
    for (index, (launch, outcome)) in plan.launches.iter().zip(ran).enumerate() {
        output.push_str(&format!(
            "== run {}: {}\n",
            index + 1,
            launch.argv.join(" ")
        ));
        match outcome {
            Ran::Result(result) => {
                report.passed = report.passed.saturating_add(result.report.passed);
                report.failed = report.failed.saturating_add(result.report.failed);
                report.ignored = report.ignored.saturating_add(result.report.ignored);
                report
                    .failures
                    .extend(result.report.failures.iter().cloned());
                report.incomplete |= result.report.incomplete;
                stopped |= result.stopped;
                if exit == Some(0) {
                    exit = result.exit;
                }
                output.push_str(&result.output);
                if !output.ends_with('\n') {
                    output.push('\n');
                }
            }
            Ran::NotStarted(error) => {
                report.incomplete = true;
                if exit == Some(0) {
                    exit = None;
                }
                output.push_str(&format!("could not start: {error}\n"));
            }
            Ran::Skipped => {
                report.incomplete = true;
                stopped = true;
                output.push_str("not run: the budget was spent\n");
            }
        }
    }
    let mut start = output.len().saturating_sub(MAX_OUTPUT);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    let output = output[start..].to_owned();
    let targets = plan
        .targets
        .iter()
        .enumerate()
        .map(|(index, target)| {
            let outcome = target.settled.clone().unwrap_or_else(|| {
                plan.launches
                    .iter()
                    .position(|launch| launch.targets.contains(&index))
                    .map_or_else(
                        || "not run".to_owned(),
                        |run| run_outcome(run, ran.get(run)),
                    )
            });
            format!("target {} {}: {outcome}", index + 1, target.label)
        })
        .collect();
    RunResult {
        report,
        output,
        output_paged: false,
        elapsed,
        stopped,
        exit,
        budget: plan.budget,
        detail_ref: plan.detail_ref.clone(),
        command: plan.rerun.0.clone(),
        rerun_dir: plan.rerun.1.clone(),
        environment_label: None,
        targets,
    }
}

/// The outcome a target line shows for invocation `run` (zero-based): its counts (`run 2 — 3
/// passed, 0 failed`, plus `, stopped at budget`), `no test results (exit N)` when it counted
/// nothing, why it could not start, or that the budget was spent before it.
fn run_outcome(run: usize, ran: Option<&Ran>) -> String {
    let number = run + 1;
    match ran {
        Some(Ran::Result(result)) => {
            let report = &result.report;
            if report.passed == 0 && report.failed == 0 && result.exit.is_some_and(|code| code != 0)
            {
                format!(
                    "run {number} — no test results (exit {})",
                    result.exit.unwrap_or_default()
                )
            } else {
                let mut line = format!(
                    "run {number} — {} passed, {} failed",
                    report.passed, report.failed
                );
                if result.stopped {
                    line.push_str(", stopped at budget");
                }
                line
            }
        }
        Some(Ran::NotStarted(error)) => format!("run {number} — could not start: {error}"),
        Some(Ran::Skipped) | None => format!("run {number} — not run: the budget was spent"),
    }
}

#[cfg(test)]
mod batch_tests {
    use super::*;

    /// A launch of `script` under `/bin/sh` for the test language, running `targets`.
    fn shell(targets: Vec<usize>, script: &str) -> BatchLaunch {
        BatchLaunch {
            targets,
            language: crate::lang::testing::ALPHA,
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            start: BatchCommand::Resolved(CommandResolution {
                resolved: None,
                environment_label: None,
            }),
        }
    }

    /// One plan over `targets` (labels; `Some` settles a target without running it).
    fn plan(targets: &[(&str, Option<&str>)], launches: Vec<BatchLaunch>) -> BatchPlan {
        BatchPlan {
            targets: targets
                .iter()
                .map(|(label, settled)| BatchTarget {
                    label: (*label).to_owned(),
                    settled: settled.map(str::to_owned),
                })
                .collect(),
            launches,
            rerun: (vec!["sh".into(), "-c".into(), "a; b".into()], None),
            budget: Duration::from_secs(5),
            detail_ref: "batch-ref".into(),
        }
    }

    /// Waits for run `id` of `root` to settle and returns its result.
    async fn settled(runs: &TestRuns, root: &PathBuf, id: u64, owner: &BindingRef) -> RunResult {
        for _ in 0..200 {
            if let Some(result) = runs.get(root, id, owner).and_then(|status| status.result) {
                return result;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("run {id} did not settle");
    }

    /// The invocations of one run go one after another under one id that holds the worktree's
    /// run slot throughout; the result sums their counts, keeps the plan's rerun line and gives
    /// every target a line, a target that runs nothing included.
    #[tokio::test]
    async fn targets_run_as_one_run_with_a_line_per_target() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-batch-run-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let owner = BindingRef::fixture("batch-actor", "batch-channel", 1);
        let started = runs.start_batch(
            root.clone(),
            &owner,
            plan(
                &[
                    ("symbol a.rs#f", None),
                    ("symbol a.rs#g", None),
                    ("path b.rs", Some("no tests in b.rs; the file has no tests")),
                    ("pattern slow", None),
                ],
                vec![
                    shell(vec![0, 1], "sleep 0.3; printf 'pass\\npass\\n'"),
                    shell(vec![3], "printf 'pass\\nfail\\n'"),
                ],
            ),
        );
        let StartResult::Started(id) = started else {
            panic!("the run starts");
        };
        assert!(
            matches!(
                runs.start(
                    root.clone(),
                    vec!["true".into()],
                    crate::lang::testing::ALPHA,
                    Duration::from_secs(1),
                    "other".into(),
                    &owner,
                ),
                StartResult::Running(running, _) if running == id
            ),
            "one run per worktree holds while the targets run"
        );
        let result = settled(&runs, &root, id, &owner).await;
        assert_eq!((result.report.passed, result.report.failed), (3, 1));
        assert_eq!(result.command, ["sh", "-c", "a; b"]);
        assert_eq!(
            result.targets,
            [
                "target 1 symbol a.rs#f: run 1 — 2 passed, 0 failed",
                "target 2 symbol a.rs#g: run 1 — 2 passed, 0 failed",
                "target 3 path b.rs: no tests in b.rs; the file has no tests",
                "target 4 pattern slow: run 2 — 1 passed, 1 failed",
            ]
        );
        assert!(
            result.output.starts_with("== run 1: /bin/sh -c"),
            "{}",
            result.output
        );
        assert!(
            result.output.contains("\n== run 2: /bin/sh -c"),
            "{}",
            result.output
        );
    }

    /// An invocation that cannot start reports on its targets' lines; the budget covers the whole
    /// run, so an invocation after a spent budget does not run and the run reads stopped.
    #[tokio::test]
    async fn missing_runners_and_a_spent_budget_are_reported_per_target() {
        let runs = TestRuns::default();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("agent-ide-batch-budget-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let owner = BindingRef::fixture("budget-actor", "budget-channel", 1);
        let mut budgeted = plan(
            &[
                ("pattern a", None),
                ("pattern b", None),
                ("pattern c", None),
            ],
            vec![
                BatchLaunch {
                    start: BatchCommand::Missing("program not found".into()),
                    ..shell(vec![0], "true")
                },
                shell(vec![1], "sleep 3"),
                shell(vec![2], "printf 'pass\\n'"),
            ],
        );
        budgeted.budget = Duration::from_secs(1);
        let StartResult::Started(id) = runs.start_batch(root.clone(), &owner, budgeted) else {
            panic!("the run starts");
        };
        let result = settled(&runs, &root, id, &owner).await;
        assert!(result.stopped);
        assert_eq!(
            result.targets[0],
            "target 1 pattern a: run 1 — could not start: program not found"
        );
        assert!(
            result.targets[1].starts_with("target 2 pattern b: run 2 — "),
            "{:?}",
            result.targets
        );
        assert_eq!(
            result.targets[2],
            "target 3 pattern c: run 3 — not run: the budget was spent"
        );
    }
}
