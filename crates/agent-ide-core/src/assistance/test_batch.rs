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
    /// `symbol <address>`, `path <path>`, `pattern <filter>` or displayed explicit command argv.
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
    /// Planned display while no launch has started; settled replay is built from actual starts.
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
    /// Owner cancellation prevented this invocation from starting.
    Cancelled,
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
        let cancel = tokio_util::sync::CancellationToken::new();
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
                cancel: cancel.clone(),
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
                if cancel.is_cancelled() {
                    ran.push(Ran::Cancelled);
                    continue;
                }
                if remaining.is_zero() {
                    ran.push(Ran::Skipped);
                    continue;
                }
                let (spawned, budget, cap) = match &launch.start {
                    BatchCommand::Admitted(spec) => (
                        spawn_spec(&admission, &root, spec).map(|child| {
                            (
                                child,
                                spec_replay(spec),
                                spec.cwd
                                    .strip_prefix(&root)
                                    .ok()
                                    .filter(|path| !path.as_os_str().is_empty())
                                    .map(|path| path.display().to_string()),
                            )
                        }),
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
                        )
                        .map(|(child, replay)| (child, replay, None)),
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
                    Ok((child, replay, cwd)) => {
                        let mut result = run_child_cancellable(
                            launch.language,
                            budget,
                            child,
                            &root,
                            cap,
                            cancel.clone(),
                        )
                        .await;
                        result.command = replay;
                        result.rerun_dir = cwd;
                        Ran::Result(Box::new(result))
                    }
                    Err(error) => Ran::NotStarted(error.to_string()),
                });
            }
            lease.settled();
            let mut result = combine(&plan, &ran, started.elapsed());
            result.cancelled |= ran.iter().any(|run| matches!(run, Ran::Cancelled));
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
/// last [`MAX_OUTPUT`] bytes kept), actual-start replay and one line per target. Planning labels
/// never become executable receipts; missing, skipped and cancelled-before-start steps have none.
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
            Ran::Cancelled => {
                report.incomplete = true;
                stopped = true;
                if exit == Some(0) {
                    exit = None;
                }
                output.push_str("not run: cancelled by owner\n");
            }
            Ran::Skipped => {
                report.incomplete = true;
                stopped = true;
                if exit == Some(0) {
                    exit = None;
                }
                output.push_str("not run: the budget was spent\n");
            }
        }
    }
    let output = retained_output(&output, MAX_OUTPUT, output.len() > MAX_OUTPUT);
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
            crate::assistance::test_fact::target_outcome(index + 1, &target.label, &outcome)
        })
        .collect();
    let (command, rerun_dir) = batch_replay(ran);
    RunResult {
        report,
        output,
        output_paged: false,
        elapsed,
        stopped,
        cancelled: ran.iter().any(|run| {
            matches!(run, Ran::Cancelled) || matches!(run, Ran::Result(result) if result.cancelled)
        }),
        exit,
        budget: plan.budget,
        detail_ref: plan.detail_ref.clone(),
        command,
        rerun_dir,
        environment_label: None,
        targets,
    }
}

/// Replays only invocations that actually started, in order, with their exact captured argv and
/// per-step working directory. Several commands use ordinary shell subshells to isolate cwd and
/// environment and preserve the first nonzero exit while continuing later steps. No starts yields
/// empty argv, not a fabricated successful command. The full vector is retained separately from
/// its compact preview; control characters stay exact inside the quoted shell script.
fn batch_replay(ran: &[Ran]) -> (Vec<String>, Option<String>) {
    let started: Vec<_> = ran
        .iter()
        .filter_map(|run| match run {
            Ran::Result(result) if !result.command.is_empty() => Some(result),
            _ => None,
        })
        .collect();
    if let [only] = started.as_slice() {
        return (only.command.clone(), only.rerun_dir.clone());
    }
    if started.is_empty() {
        return (Vec::new(), None);
    }
    let mut script = "status=0\n".to_owned();
    for result in started {
        let command = crate::assistance::worker::shell_argv(&result.command);
        let step = match &result.rerun_dir {
            Some(cwd) => format!(
                "cd {} && {command}",
                crate::assistance::worker::shell_argv(std::slice::from_ref(cwd))
            ),
            None => command,
        };
        script.push_str(&format!(
            "({step})\ncode=$?\nif [ \"$status\" -eq 0 ]; then status=$code; fi\n"
        ));
    }
    script.push_str("exit \"$status\"");
    (vec!["/bin/sh".into(), "-c".into(), script], None)
}

/// The outcome a target line shows for invocation `run` (zero-based): its counts (`run 2 — 3
/// passed, 0 failed`, plus `, stopped at budget`), `no test results (exit N)` when it counted
/// nothing, why it could not start, or that the budget was spent before it. A command that
/// never started has no observed exit; unparsed output never supplies test counts.
fn run_outcome(run: usize, ran: Option<&Ran>) -> String {
    let number = run + 1;
    match ran {
        Some(Ran::Result(result)) => {
            let report = &result.report;
            if summary_absent(result) {
                let exit = result
                    .exit
                    .map_or_else(|| "unavailable".to_owned(), |code| code.to_string());
                let state = if result.cancelled {
                    "; cancelled"
                } else if result.stopped {
                    "; stopped at budget"
                } else {
                    ""
                };
                format!("run {number} — exit {exit}; test counts unavailable{state}")
            } else if report.passed == 0
                && report.failed == 0
                && result.exit.is_some_and(|code| code != 0)
            {
                format!(
                    "run {number} — no test results (exit {})",
                    result.exit.unwrap_or_default()
                )
            } else {
                let mut line = format!(
                    "run {number} — {} passed, {} failed (exit {})",
                    report.passed,
                    report.failed,
                    result
                        .exit
                        .map_or_else(|| "unknown".to_owned(), |code| code.to_string())
                );
                if report.incomplete && !result.stopped {
                    line.push_str(", partial counts");
                }
                if result.cancelled {
                    line.push_str(", cancelled");
                } else if result.stopped {
                    line.push_str(", stopped at budget");
                }
                line
            }
        }
        Some(Ran::NotStarted(error)) => {
            format!("run {number} — exit unavailable; could not start: {error}")
        }
        Some(Ran::Cancelled) => {
            format!("run {number} — not run: cancelled by owner (exit unavailable)")
        }
        Some(Ran::Skipped) | None => {
            format!("run {number} — not run: the budget was spent (exit unavailable)")
        }
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

    /// An explicit check with no parsed tests reports its real exit and unavailable counts,
    /// rather than inventing a zero-count passing test report for that command.
    #[test]
    fn command_step_without_test_summary_reports_unavailable_counts() {
        let mut result = failed_run(Duration::from_secs(1), "check finished".into());
        result.exit = Some(0);
        let line = run_outcome(0, Some(&Ran::Result(Box::new(result))));
        assert_eq!(line, "run 1 — exit 0; test counts unavailable");
        assert!(!line.contains("passed"));
        let refused = run_outcome(0, Some(&Ran::NotStarted("long reason ".repeat(100))));
        assert!(
            crate::assistance::test_fact::target_outcome(1, "command", &refused)
                .contains("exit unavailable")
        );
    }

    /// Cancellation reaps the active invocation, skips later commands and preserves a terminal
    /// per-target result, rather than labelling the unused budget as a timeout.
    #[tokio::test]
    async fn cancelling_batch_reaps_current_and_skips_remaining_commands() {
        let scratch = crate::scratch::ScratchDir::new("batch-cancel");
        let root = scratch.to_path_buf();
        let runs = TestRuns::default();
        let owner = BindingRef::fixture("batch-cancel", "channel", 1);
        let settled = Arc::new(tokio::sync::Notify::new());
        let note = settled.clone();
        runs.on_settle(Arc::new(move |_| note.notify_one()));
        let first = shell(vec![0], "printf ready > ready; sleep 30");
        let second = shell(vec![1], "printf forbidden > second");
        assert!(matches!(
            runs.start_batch(
                root.clone(),
                &owner,
                plan(
                    &[("command one", None), ("command two", None)],
                    vec![first, second]
                )
            ),
            StartResult::Started(1)
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            while !root.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(runs.cancel(&root, 1, &owner), Some(true));
        tokio::time::timeout(Duration::from_secs(3), settled.notified())
            .await
            .unwrap();
        let result = runs.settled_result(1).unwrap();
        assert!(result.cancelled && result.stopped);
        assert!(!root.join("second").exists());
        assert!(
            result.targets[0].contains("cancelled"),
            "{:?}",
            result.targets
        );
        assert!(
            result.targets[1].contains("not run: cancelled by owner"),
            "{:?}",
            result.targets
        );
        assert!(
            crate::assistance::test_fact::terminal_fact(1, &result, true)
                .summary
                .starts_with("cancelled")
        );
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
    /// run slot throughout; the result sums their counts, keeps actual-start replay and gives
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
        let replay = tokio::process::Command::new(&result.command[0])
            .args(&result.command[1..])
            .current_dir(&root)
            .output()
            .await
            .unwrap();
        assert_eq!(replay.status.code(), Some(0));
        assert_eq!(replay.stdout, b"pass\npass\npass\nfail\n");
        assert!(
            !result.command.contains(&"a; b".into()),
            "planning labels are not replay receipts"
        );
        assert_eq!(
            result.targets,
            [
                "target 1 symbol a.rs#f: run 1 — 2 passed, 0 failed (exit 0)",
                "target 2 symbol a.rs#g: run 1 — 2 passed, 0 failed (exit 0)",
                "target 3 path b.rs: no tests in b.rs; the file has no tests",
                "target 4 pattern slow: run 2 — 1 passed, 1 failed (exit 0)",
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
            "target 1 pattern a: run 1 — exit unavailable; could not start: program not found"
        );
        assert!(
            result.targets[1].starts_with("target 2 pattern b: run 2 — "),
            "{:?}",
            result.targets
        );
        assert_eq!(
            result.targets[2],
            "target 3 pattern c: run 3 — not run: the budget was spent (exit unavailable)"
        );
    }
}
