//! `ide.test {targets}` on the worker: several test targets as ONE run of the worktree
//! (batch-api-v1 §6).
//!
//! Each target is planned as the single form plans it. Symbol targets of one language merge
//! their referencing tests into one selection, so the language's runner gets every filter in one
//! invocation (libtest names, pytest node ids, vitest files). Path targets of one language merge
//! into one invocation when the language's runner takes several files at once (pytest paths,
//! vitest/jest/node files, cargo integration tests of one package); otherwise each path, and
//! every pattern target, is its own invocation. The invocations then run one after another as one run of the worktree
//! ([`super::tests::TestRuns::start_batch`]), whose single result has one line per target and
//! one rerun line for the whole run, and reaches the caller as one `test-finished` fact.

use super::*;
use crate::assistance::batch::{self, TestTargetSpec};
use crate::assistance::tests::{BatchCommand, BatchLaunch, BatchPlan, BatchTarget};
use crate::lang::{SymbolPath, TestId, TestTarget};

/// One planned invocation before it is ordered: the first target it runs, its launch, and the
/// `N tests selected` note its targets' started lines show.
type Planned = (usize, BatchLaunch, Option<String>);

/// What planning one target yields: a planned invocation, the target's whole outcome when it
/// runs nothing, or (symbol targets) its definition, referencing tests and language for merging.
enum TargetPlan {
    /// A test file of a known language, to merge with the language's other path targets.
    File(PathBuf, crate::lang::Language),
    /// Its own invocation: argv, language, selected count and how it starts.
    Run(
        Vec<String>,
        crate::lang::Language,
        Option<String>,
        Option<crate::modules::calls::PlannedStart>,
    ),
    /// Explicit argv needs no test discovery or semantic provider.
    Command(Vec<String>),
    /// It runs nothing; the line's outcome.
    Settled(String),
    /// A symbol's definition address, its referencing tests and their language.
    Tests(SymbolPath, Vec<TestId>, crate::lang::Language),
}

impl Worker<'_> {
    /// `ide.test {targets, budget_s?}`: plans every target, merges the symbol targets of one
    /// language into one selection, and starts all invocations as ONE run of the worktree.
    ///
    /// A target that runs nothing (no tests reference it, a file without tests, a target the
    /// language cannot run, an ambiguous or unknown symbol) is answered on its own line and never
    /// fails the others. When nothing runs at all the reply is `tests: nothing to run` with the
    /// target lines. A started run answers `tests #N: started — <n> targets in <r> runs (budget
    /// <b> s); the result is retained under its reference; Bridge delivers it automatically when
    /// configured` and a line per target; a busy worktree answers the live run and retains its
    /// result for this subscriber too.
    pub(super) async fn batch_tests(
        &mut self,
        job: &mut Job,
    ) -> Result<(PeerReply, Option<AuthorityStamp>, Option<SourceObservation>), FailureCode> {
        let binding = job.invocation.binding_ref().clone();
        let authority = self.authority(&binding).await?;
        self.shared.active(&binding)?;
        let root = authority.worktree().worktree_path().to_path_buf();
        let budget = Duration::from_secs(
            job.parameters
                .get("budget_s")
                .and_then(Value::as_u64)
                .unwrap_or(120)
                .clamp(1, 600),
        );
        let specs = batch::test_targets(&job.parameters);
        let mut targets: Vec<BatchTarget> = specs
            .iter()
            .map(|spec| BatchTarget {
                label: spec.label(),
                settled: None,
            })
            .collect();
        let mut planned: Vec<Planned> = Vec::new();
        // Symbol targets of one language: the first one's address, every referencing test, and
        // the targets they came from.
        let mut merged: Vec<(crate::lang::Language, SymbolPath, Vec<TestId>, Vec<usize>)> =
            Vec::new();
        // Path targets of one language, each with its target index.
        let mut paths: Vec<(crate::lang::Language, Vec<(usize, PathBuf)>)> = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            match self.plan_target(job, &binding, &root, spec, budget).await? {
                TargetPlan::Run(argv, language, count, start) => {
                    let launch = self
                        .batch_launch(job, &root, vec![index], argv, language, start)
                        .await?;
                    planned.push((index, launch, count));
                }
                TargetPlan::Command(argv) => {
                    let language = crate::lang::registered()
                        .first()
                        .copied()
                        .ok_or(FailureCode::ProviderUnavailable)?;
                    let resolution = crate::assistance::tests::resolve_command(
                        &root,
                        &root,
                        &argv,
                        None,
                        &self.shared.launcher,
                        &job.target,
                    )
                    .await
                    .map_err(|failure| symbols::module_failure(job, &failure))?
                    .map_err(|error| symbols::environment_failure(job, &error))?;
                    planned.push((
                        index,
                        BatchLaunch {
                            targets: vec![index],
                            language,
                            argv,
                            start: BatchCommand::Resolved(resolution),
                        },
                        None,
                    ));
                }
                TargetPlan::Settled(outcome) => targets[index].settled = Some(outcome),
                TargetPlan::File(path, language) => {
                    match paths.iter_mut().find(|group| group.0 == language) {
                        Some(group) => group.1.push((index, path)),
                        None => paths.push((language, vec![(index, path)])),
                    }
                }
                TargetPlan::Tests(path, tests, language) => {
                    match merged.iter_mut().find(|group| group.0 == language) {
                        Some(group) => {
                            group.2.extend(tests);
                            group.3.push(index);
                        }
                        None => merged.push((language, path, tests, vec![index])),
                    }
                }
            }
        }
        for (_, files) in paths {
            // One invocation for every file when the runner takes them together.
            if files.len() > 1 {
                let all = TestTarget::Files(files.iter().map(|file| file.1.clone()).collect());
                if let Ok((argv, language, count, start)) =
                    test_selection(&root, all, budget, &self.shared.launcher, &job.target)
                        .await
                        .map_err(|failure| symbols::module_failure(job, &failure))?
                {
                    let indexes: Vec<usize> = files.iter().map(|file| file.0).collect();
                    let first = indexes[0];
                    let launch = self
                        .batch_launch(job, &root, indexes, argv, language, start)
                        .await?;
                    planned.push((first, launch, count));
                    continue;
                }
            }
            for (index, path) in files {
                match test_selection(
                    &root,
                    TestTarget::File(path),
                    budget,
                    &self.shared.launcher,
                    &job.target,
                )
                .await
                .map_err(|failure| symbols::module_failure(job, &failure))?
                {
                    Ok((argv, language, count, start)) => {
                        let launch = self
                            .batch_launch(job, &root, vec![index], argv, language, start)
                            .await?;
                        planned.push((index, launch, count));
                    }
                    Err(error) => targets[index].settled = Some(unrunnable(&error)),
                }
            }
        }
        for (language, path, tests, indexes) in merged {
            let target = TestTarget::Symbol {
                path,
                referencing_tests: tests,
            };
            let config = self
                .shared
                .launcher
                .module_config(language, &root, &job.target);
            let project = crate::modules::calls::detect(language, &root, &config)
                .await
                .map_err(|failure| symbols::module_failure(job, &failure))?;
            let selection = match project {
                Some(project) => crate::modules::calls::test_run(
                    language, &root, &project, &target, budget, &config,
                )
                .await
                .map_err(|failure| symbols::module_failure(job, &failure))?,
                None => Err(crate::lang::LangError::Unsupported(
                    "no supported test runner was detected".to_owned(),
                )),
            };
            match selection {
                Ok(selection) => {
                    let count = Some(format!("{} tests selected", selection.tests.len()));
                    let launch = self
                        .batch_launch(
                            job,
                            &root,
                            indexes.clone(),
                            selection.command,
                            language,
                            Some(selection.start),
                        )
                        .await?;
                    planned.push((indexes[0], launch, count));
                }
                Err(error) => {
                    let outcome = unrunnable(&error);
                    for index in indexes {
                        targets[index].settled = Some(outcome.clone());
                    }
                }
            }
        }
        planned.sort_by_key(|planned| planned.0);
        let started_lines = targets
            .iter()
            .enumerate()
            .map(|(index, target)| {
                let outcome = target.settled.clone().unwrap_or_else(|| {
                    planned
                        .iter()
                        .position(|run| run.1.targets.contains(&index))
                        .map_or_else(String::new, |run| match &planned[run].2 {
                            Some(count) => format!("run {} ({count})", run + 1),
                            None => format!("run {}", run + 1),
                        })
                });
                format!("\n  target {} {}: {outcome}", index + 1, target.label)
            })
            .collect::<String>();
        let complete = |text: String, detail_ref: Option<String>| {
            Ok((
                PeerReply::Complete {
                    kind: ResultKind::Test,
                    text,
                    detail_ref,
                    truncated: false,
                    continuation: false,
                },
                Some(authority.clone()),
                None,
            ))
        };
        if planned.is_empty() {
            return complete(
                format!(
                    "tests: nothing to run — none of the {} targets selects a test{started_lines}",
                    targets.len()
                ),
                None,
            );
        }
        let runs = planned.len();
        let launches: Vec<BatchLaunch> = planned.into_iter().map(|planned| planned.1).collect();
        let rerun = whole_rerun(&root, &launches);
        let count = targets.len();
        let plan = BatchPlan {
            targets,
            launches,
            rerun,
            budget,
            detail_ref: job.reference.clone(),
        };
        match self
            .shared
            .test_runs
            .start_batch(root.clone(), &binding, plan)
        {
            StartResult::Started(id) => {
                // The caller leaves with a started line: the outcome is pushed once as a
                // `test-finished` fact (OUTBOX §2).
                self.shared.test_started(id, &binding);
                complete(
                    format!("{}{started_lines}", started_line(id, count, runs, budget)),
                    Some(job.reference.clone()),
                )
            }
            StartResult::Running(id, _) => {
                // One run per worktree: this session receives the live run's own result.
                self.shared.test_subscribed(id, &binding);
                complete(running_line(id), None)
            }
            StartResult::Failed { error, .. } => complete(
                format!(
                    "tests: could not start the run: {}",
                    test_text_line(&error, 240)
                ),
                None,
            ),
        }
    }

    /// Plans one target as the single form would: a path that is not a test file of a language
    /// whose tests live only in test files, a symbol no test references, an ambiguous bare name
    /// and a selection the language cannot run settle the target on its own line; a test file of
    /// a known language and a symbol's referencing tests are returned for merging. Failures of the target alone (an unknown
    /// symbol) settle it too; a loading server parks the whole call as in the single form.
    async fn plan_target(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        root: &Path,
        spec: &TestTargetSpec,
        budget: Duration,
    ) -> Result<TargetPlan, FailureCode> {
        let selection = match spec {
            TestTargetSpec::Path(path) => {
                let target = PathBuf::from(path);
                let not_a_test_file = match crate::lang::Language::for_path(&target) {
                    Some(language)
                        if !root.join(&target).is_dir()
                            && language.support().tests_only_in_test_files() =>
                    {
                        !crate::modules::calls::test_facts(language, root, &target)
                            .await
                            .map_err(|failure| symbols::module_failure(job, &failure))?
                            .is_test_file
                    }
                    _ => false,
                };
                if not_a_test_file {
                    return Ok(TargetPlan::Settled(format!(
                        "no tests in {path}; the file has no tests"
                    )));
                }
                // A test file of a known language may share its language's invocation.
                if let Some(language) = crate::lang::Language::for_path(&target)
                    && !root.join(&target).is_dir()
                {
                    return Ok(TargetPlan::File(target, language));
                }
                test_selection(
                    root,
                    TestTarget::File(target),
                    budget,
                    &self.shared.launcher,
                    &job.target,
                )
                .await
            }
            TestTargetSpec::Command(argv) => return Ok(TargetPlan::Command(argv.clone())),
            TestTargetSpec::Pattern(pattern) => {
                test_selection(
                    root,
                    TestTarget::Pattern(pattern.clone()),
                    budget,
                    &self.shared.launcher,
                    &job.target,
                )
                .await
            }
            TestTargetSpec::Symbol(symbol) => {
                return match self.referencing_tests(job, binding, symbol).await {
                    Ok(plan) => Ok(plan),
                    Err(FailureCode::ProviderLoading) => Err(FailureCode::ProviderLoading),
                    Err(code) => {
                        let detail = job.failure_detail.take();
                        let text = batch_targets::rendered_text(
                            PeerReply::Error { code, detail },
                            AssistanceTool::Test,
                        );
                        Ok(TargetPlan::Settled(text.trim_end().to_owned()))
                    }
                };
            }
        };
        Ok(
            match selection.map_err(|failure| symbols::module_failure(job, &failure))? {
                Ok((argv, language, count, start)) => TargetPlan::Run(argv, language, count, start),
                Err(error) => TargetPlan::Settled(unrunnable(&error)),
            },
        )
    }

    /// The tests that reference `symbol`, resolved as the single form resolves it (a bare name
    /// through the workspace symbols), with its definition address and language; no
    /// referencing test settles the target with the single form's hint.
    async fn referencing_tests(
        &mut self,
        job: &mut Job,
        binding: &BindingRef,
        symbol: &str,
    ) -> Result<TargetPlan, FailureCode> {
        let path = SymbolPath::parse(symbol).map_err(|_| FailureCode::UnknownSymbol)?;
        let resolved = match path.file() {
            Some(_) => None,
            None => match self
                .locate_by_name(job, binding, path.name().unwrap_or_default())
                .await?
            {
                Located::One(file) => Some(file),
                Located::Many(candidates) => {
                    return Ok(TargetPlan::Settled(format!(
                        "{symbol} matches {} symbols; name one as file#Owner/name: {}",
                        candidates.len(),
                        candidates
                            .iter()
                            .take(4)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
            },
        };
        let path = match (path.file(), &resolved) {
            (Some(_), _) => path,
            (None, Some(file)) => SymbolPath::parse(&format!(
                "{}#{}",
                file.display(),
                path.name().unwrap_or_default()
            ))
            .map_err(|_| FailureCode::UnknownSymbol)?,
            (None, None) => return Err(FailureCode::UnknownSymbol),
        };
        let (tests, language, file_test_count) = self
            .tests_referencing_symbol(job, symbol, resolved.as_ref())
            .await?;
        if tests.is_empty() {
            let file = path
                .file()
                .map(|file| file.display().to_string())
                .unwrap_or_default();
            return Ok(TargetPlan::Settled(if file_test_count == 0 {
                format!("no tests reference {symbol}; the file has no tests")
            } else {
                format!(
                    "no tests reference {symbol}; the file has {file_test_count} tests — ide.test {}",
                    serde_json::json!({"path": file})
                )
            }));
        }
        Ok(TargetPlan::Tests(path, tests, language))
    }

    /// One invocation of a several-target run running `targets`: an admitted recipe as is, a
    /// missing home tool as a start error, else `argv` in the language's resolved command
    /// environment from the worktree root.
    async fn batch_launch(
        &mut self,
        job: &mut Job,
        root: &Path,
        targets: Vec<usize>,
        argv: Vec<String>,
        language: crate::lang::Language,
        start: Option<crate::modules::calls::PlannedStart>,
    ) -> Result<BatchLaunch, FailureCode> {
        let start = match start {
            Some(crate::modules::calls::PlannedStart::Admitted(spec)) => {
                BatchCommand::Admitted(spec)
            }
            Some(crate::modules::calls::PlannedStart::MissingTool(program)) => {
                BatchCommand::Missing(format!("{program} not found"))
            }
            Some(crate::modules::calls::PlannedStart::InProcess) | None => BatchCommand::Resolved(
                crate::assistance::tests::resolve_command(
                    root,
                    root,
                    &argv,
                    Some(language),
                    &self.shared.launcher,
                    &job.target,
                )
                .await
                .map_err(|failure| symbols::module_failure(job, &failure))?
                .map_err(|error| symbols::environment_failure(job, &error))?,
            ),
        };
        Ok(BatchLaunch {
            targets,
            language,
            argv,
            start,
        })
    }
}

/// The first line of a started several-target run: `tests #N: started — <n> targets in <r> runs
/// (budget <b> s); the result is retained under its reference; Bridge delivers it automatically`.
fn started_line(id: u64, targets: usize, runs: usize, budget: Duration) -> String {
    format!(
        "tests #{id}: started — {targets} targets in {runs} runs (budget {} s); the result \
         is retained under its reference; Bridge delivers it automatically",
        budget.as_secs()
    )
}

/// The reply of a several-target start while the worktree's run `id` is live, exactly the
/// hygiene text `tests #N: still running` (no seconds on unfinished work, M024-M027-REPLY-HYGIENE
/// r2 §2); nothing new starts and the live run's result stays retained for this session too.
fn running_line(id: u64) -> String {
    format!("tests #{id}: still running")
}

/// The outcome line of a target its language cannot run: the single form's refusal text, or
/// that no selection was available.
fn unrunnable(error: &crate::lang::LangError) -> String {
    match error {
        crate::lang::LangError::Environment(message) => format!(
            "not run: environment unavailable: {}",
            test_text_line(message, 200)
        ),
        crate::lang::LangError::Unsupported(message) => {
            format!("not runnable: {}", test_text_line(message, 200))
        }
        _ => "not runnable: the test selection is unavailable".to_owned(),
    }
}

/// The rerun line of a whole several-target run: one invocation's own command (and directory),
/// or every invocation in run order as one `sh -c` line, each admitted recipe that runs in a
/// subdirectory behind its own `cd`.
fn whole_rerun(root: &Path, launches: &[BatchLaunch]) -> (Vec<String>, Option<String>) {
    let directory = |launch: &BatchLaunch| match &launch.start {
        BatchCommand::Admitted(spec) => spec
            .cwd
            .strip_prefix(root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map(|relative| relative.display().to_string()),
        _ => None,
    };
    if let [launch] = launches {
        return (launch.argv.clone(), directory(launch));
    }
    let line = launches
        .iter()
        .map(|launch| match directory(launch) {
            Some(directory) => format!(
                "(cd {} && {})",
                display_argv(std::slice::from_ref(&directory)),
                display_argv(&launch.argv)
            ),
            None => display_argv(&launch.argv),
        })
        .collect::<Vec<_>>()
        .join("; ");
    (vec!["sh".to_owned(), "-c".to_owned(), line], None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolved launch of `argv` for `targets`.
    fn launch(targets: Vec<usize>, argv: &[&str]) -> BatchLaunch {
        BatchLaunch {
            targets,
            language: crate::lang::testing::ALPHA,
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
            start: BatchCommand::Resolved(crate::assistance::tests::CommandResolution {
                resolved: None,
                environment_label: None,
            }),
        }
    }

    #[test]
    fn the_rerun_line_reproduces_the_whole_run() {
        let root = Path::new("/work");
        let one = [launch(
            vec![0, 1],
            &["cargo", "test", "--", "--exact", "a::b"],
        )];
        assert_eq!(
            whole_rerun(root, &one),
            (
                vec![
                    "cargo".to_owned(),
                    "test".to_owned(),
                    "--".to_owned(),
                    "--exact".to_owned(),
                    "a::b".to_owned()
                ],
                None
            )
        );
        let two = [
            launch(vec![0], &["cargo", "test", "--workspace", "roles_"]),
            launch(vec![1], &["pytest", "tests/test a.py"]),
        ];
        assert_eq!(
            whole_rerun(root, &two).0,
            [
                "sh",
                "-c",
                "cargo test --workspace roles_; pytest 'tests/test a.py'"
            ]
        );
        assert_eq!(
            unrunnable(&crate::lang::LangError::Unsupported("README.md".to_owned())),
            "not runnable: README.md"
        );
    }

    /// The started and still-running lines say what happened and never invite polling, and the
    /// still-running line carries no seconds count (M024-M027-REPLY-HYGIENE r2 §2).
    #[test]
    fn run_lines_never_invite_polling() {
        let started = started_line(4, 3, 2, Duration::from_secs(300));
        assert_eq!(
            started,
            "tests #4: started — 3 targets in 2 runs (budget 300 s); the result is retained under \
             its reference; Bridge delivers it automatically"
        );
        crate::assistance::batch::assert_no_polling(&started);
        let running = running_line(4);
        assert_eq!(running, "tests #4: still running");
        crate::assistance::batch::assert_no_polling(&running);
        crate::assistance::batch::assert_no_unfinished_seconds(&running);
    }
}
