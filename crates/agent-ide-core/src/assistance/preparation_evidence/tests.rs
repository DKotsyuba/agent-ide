//! Offline proofs: operator-only configuration, failure-only extraction bound to bytes present
//! before the run, honest non-success outcomes, and real support admission/cancellation.

use super::*;
use crate::lang::testing::ALPHA;
use serde_json::json;

/// A canonical fixture worktree; `test_*` files are tests for the fixture language.
fn project() -> (crate::scratch::ScratchDir, PathBuf) {
    crate::lang::testing::install();
    let scratch = crate::scratch::ScratchDir::new("public-evidence");
    let root = scratch.join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    for (path, text) in [
        ("src/compose.alpha", "a\nb\nc\n"),
        ("src/request.alpha", "r\n"),
        ("src/hono_base.alpha", "h\n"),
        ("src/test_request.alpha", "t1\nt2\nt3\nt4\n"),
        ("src/test_broken.alpha", "b\n"),
        ("src/test_ok.alpha", "ok\n"),
    ] {
        std::fs::write(root.join(path), text).unwrap();
    }
    (scratch, root)
}

/// Clock strictly after every fixture write.
fn after_writes() -> SystemTime {
    std::thread::sleep(Duration::from_millis(10));
    SystemTime::now()
}

/// A settled result as the registry would retain it.
fn settled(
    output: &str,
    report: TestReport,
    exit: Option<i32>,
    stopped: bool,
    cancelled: bool,
) -> RunResult {
    RunResult {
        report,
        output: output.to_owned(),
        output_paged: false,
        elapsed: Duration::from_millis(1200),
        stopped,
        cancelled,
        exit,
        budget: Duration::from_secs(30),
        detail_ref: "fixture-detail".into(),
        command: vec!["runner".into()],
        rerun_dir: None,
        environment_label: None,
        targets: Vec::new(),
    }
}

fn hash(root: &Path, path: &str) -> String {
    blake3::hash(&std::fs::read(root.join(path)).unwrap())
        .to_hex()
        .to_string()
}

/// Runner output with a passing test that logs a location, a failed suite and a failed test.
fn mixed_output(root: &Path) -> String {
    format!(
        " ✓ src/test_ok.alpha (1 test) 2ms
stdout | src/test_ok.alpha > ok > logs
    at src/hono_base.alpha:1:1
 ❯ src/test_request.alpha (2 tests | 1 failed) 5ms
   × Request > parses 3ms

⎯⎯⎯⎯⎯⎯⎯ Failed Suites 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/test_broken.alpha [ src/test_broken.alpha ]
Error: Transform failed
 ❯ src/request.alpha:1:1

⎯⎯⎯⎯⎯⎯⎯ Failed Tests 1 ⎯⎯⎯⎯⎯⎯⎯

 FAIL  src/test_request.alpha > Request > parses
AssertionError: expected 1 to be 2
 ❯ src/compose.alpha:2:3
 ❯ src/test_request.alpha:4:5
 ❯ node_modules/dep/index.js:1:1
 ❯ {}/src/compose.alpha:2:9
 ❯ src/compose.alpha:99:1

⎯⎯⎯⎯⎯⎯⎯[1/1]⎯

 Test Files  2 failed | 1 passed (3)
      Tests  1 failed | 1 passed (2)
",
        root.display()
    )
}

fn failed_report() -> TestReport {
    TestReport {
        passed: 1,
        failed: 1,
        ignored: 0,
        failures: vec![TestFailure {
            name: "Request > parses".into(),
            location: Some((PathBuf::from("src/compose.alpha"), 2)),
            message: "AssertionError: expected 1 to be 2".into(),
        }],
        incomplete: false,
    }
}

/// Only an explicit, bounded, public operator setting enables the collector.
#[test]
fn config_is_operator_only_and_bounded() {
    let parse = |value: Value| EvidenceConfig::parse(Some(&value));
    assert_eq!(EvidenceConfig::parse(None), Ok(None));
    assert_eq!(
        parse(json!({"enabled":false,"target":{"pattern":"src"},"budget_ms":1000})),
        Ok(None)
    );
    assert_eq!(
        parse(json!({"enabled":true,"target":{"pattern":"src"},"budget_ms":30000})),
        Ok(Some(EvidenceConfig {
            target: EvidenceTarget::Pattern("src".into()),
            budget: Duration::from_secs(30),
        }))
    );
    assert_eq!(
        parse(
            json!({"enabled":true,"target":{"files":["src/test_request.alpha"]},"budget_ms":500})
        ),
        Ok(Some(EvidenceConfig {
            target: EvidenceTarget::Files(vec![PathBuf::from("src/test_request.alpha")]),
            budget: Duration::from_millis(500),
        }))
    );
    for budget in [0, 30_001] {
        assert_eq!(
            parse(json!({"enabled":true,"target":{"pattern":"src"},"budget_ms":budget})),
            Err("evidence_budget_invalid")
        );
    }
    for files in [
        json!([]),
        json!(["/abs/test_a.alpha"]),
        json!(["../test_a.alpha"]),
        json!([".git/test_a.alpha"]),
        json!([".hidden/test_a.alpha"]),
        json!(["node_modules/test_a.alpha"]),
    ] {
        assert_eq!(
            parse(json!({"enabled":true,"target":{"files":files},"budget_ms":1000})),
            Err("evidence_files_invalid")
        );
    }
    for pattern in ["", "  ", "a\nb"] {
        assert_eq!(
            parse(json!({"enabled":true,"target":{"pattern":pattern},"budget_ms":1000})),
            Err("evidence_pattern_invalid")
        );
    }
    assert_eq!(
        parse(json!({"enabled":true,"target":{"pattern":"src"},"budget_ms":1000,"oracle":"x"})),
        Err("evidence_config_invalid")
    );
}

/// One configured file uses the ordinary single-file target; some runners refuse grouped files.
/// Several files stay one `Files` invocation; a pattern stays a pattern.
#[test]
fn single_file_selection_uses_the_file_target() {
    let config = |target| EvidenceConfig {
        target,
        budget: Duration::from_secs(1),
    };
    assert!(matches!(
        config(EvidenceTarget::Files(vec!["src/lib.rs".into()])).test_target(),
        TestTarget::File(path) if path == Path::new("src/lib.rs")
    ));
    assert!(matches!(
        config(EvidenceTarget::Files(vec!["a.rs".into(), "b.rs".into()])).test_target(),
        TestTarget::Files(paths) if paths == [PathBuf::from("a.rs"), PathBuf::from("b.rs")]
    ));
    assert!(matches!(
        config(EvidenceTarget::Pattern("src".into())).test_target(),
        TestTarget::Pattern(pattern) if pattern == "src"
    ));
}

/// Locations come only from failure blocks and typed failures; passing output, dependencies,
/// outside lines and duplicates never become frames; hashes are the observed bytes.
#[test]
fn extraction_is_failure_only_and_hash_bound() {
    let (_scratch, root) = project();
    let before = after_writes();
    let result = settled(&mixed_output(&root), failed_report(), Some(1), false, false);
    let report = extract(&root, 7, &result, before);
    assert_eq!(report.outcome, CheckOutcome::Failed);
    assert_eq!((report.passed, report.failed, report.exit), (1, 1, Some(1)));
    assert_eq!(report.failures.len(), 2);
    let suite = &report.failures[0];
    assert_eq!(suite.kind, FailureKind::Suite);
    assert_eq!(
        suite.test_file.as_deref(),
        Some(Path::new("src/test_broken.alpha"))
    );
    assert_eq!(suite.message, "Error: Transform failed");
    let test = &report.failures[1];
    assert_eq!(test.kind, FailureKind::Test);
    assert_eq!(test.name, "Request > parses");
    assert_eq!(
        test.test_file.as_deref(),
        Some(Path::new("src/test_request.alpha"))
    );
    let location = test.location.as_ref().unwrap();
    assert_eq!(
        (location.path.as_path(), location.line),
        (Path::new("src/compose.alpha"), 2)
    );
    assert_eq!(location.source_hash, hash(&root, "src/compose.alpha"));
    let frames: Vec<(&Path, u32, bool, usize, u32)> = report
        .frames
        .iter()
        .map(|f| (f.path.as_path(), f.line, f.test, f.failure, f.count))
        .collect();
    assert_eq!(
        frames,
        [
            (Path::new("src/request.alpha"), 1, false, 0, 1),
            (Path::new("src/compose.alpha"), 2, false, 1, 2),
            (Path::new("src/test_request.alpha"), 4, true, 1, 1),
        ]
    );
    assert!(
        report
            .frames
            .iter()
            .all(|f| f.path != Path::new("src/hono_base.alpha"))
    );
    assert_eq!(
        report.frames[2].source_hash,
        hash(&root, "src/test_request.alpha")
    );
    assert_eq!(report.unresolved_frames, 1);
    assert_eq!(
        report.errors,
        ["frame_line_outside_source: src/compose.alpha:99"]
    );
}

/// A source changed after the run began is never bound to a hash, and says so.
#[test]
fn sources_changed_during_check_are_refused() {
    let (_scratch, root) = project();
    let before = after_writes();
    std::fs::write(root.join("src/compose.alpha"), "a\nb\nc\nd\n").unwrap();
    let result = settled(&mixed_output(&root), failed_report(), Some(1), false, false);
    let report = extract(&root, 7, &result, before);
    assert!(report.failures[1].location.is_none());
    assert!(
        report
            .frames
            .iter()
            .all(|f| f.path != Path::new("src/compose.alpha"))
    );
    assert!(
        report
            .errors
            .contains(&"source_changed_during_check: src/compose.alpha".to_owned())
    );
    let mut fenced = extract(&root, 7, &result, before);
    fenced.withdraw_sources("source_epoch_changed_during_check");
    assert!(fenced.frames.is_empty() && fenced.failures.iter().all(|f| f.location.is_none()));
    assert!(fenced.retrieval_evidence().is_none());
}

/// A symlink that leaves the worktree is not a source.
#[test]
fn symlinked_sources_are_not_evidence() {
    let (scratch, root) = project();
    std::fs::write(scratch.join("outside.alpha"), "secret\n").unwrap();
    std::os::unix::fs::symlink(scratch.join("outside.alpha"), root.join("src/link.alpha")).unwrap();
    let before = after_writes();
    let output = " FAIL  src/test_broken.alpha [ src/test_broken.alpha ]\nError: boom\n ❯ src/link.alpha:1:1\n";
    let result = settled(
        output,
        TestReport {
            failed: 1,
            ..TestReport::default()
        },
        Some(1),
        false,
        false,
    );
    let report = extract(&root, 3, &result, before);
    assert!(report.frames.is_empty());
    assert_eq!(report.errors, ["source_not_regular_file: src/link.alpha"]);
}

/// Budget kills, cancellations and unexplained nonzero exits are never success.
#[test]
fn unclean_runs_are_never_success() {
    let (_scratch, root) = project();
    let before = after_writes();
    let incomplete = TestReport {
        incomplete: true,
        ..TestReport::default()
    };
    let budget = extract(
        &root,
        1,
        &settled("", incomplete.clone(), None, true, false),
        before,
    );
    assert_eq!(budget.outcome, CheckOutcome::BudgetExhausted);
    assert_eq!(
        budget.errors,
        ["runner_summary_missing", "budget_exhausted; report partial"]
    );
    let cancelled = extract(
        &root,
        2,
        &settled("", incomplete.clone(), None, true, true),
        before,
    );
    assert_eq!(cancelled.outcome, CheckOutcome::Cancelled);
    let crashed = extract(
        &root,
        3,
        &settled("boom\n", TestReport::default(), Some(1), false, false),
        before,
    );
    assert_eq!(crashed.outcome, CheckOutcome::Incomplete);
    assert_eq!(crashed.errors, ["unclean_run_without_parsed_failures"]);
    let silent = extract(
        &root,
        4,
        &settled("", incomplete, Some(0), false, false),
        before,
    );
    assert_eq!(silent.outcome, CheckOutcome::Incomplete);
    let clean = extract(
        &root,
        5,
        &settled(
            " ✓ src/test_ok.alpha\n",
            TestReport {
                passed: 1,
                ..TestReport::default()
            },
            Some(0),
            false,
            false,
        ),
        before,
    );
    assert_eq!(clean.outcome, CheckOutcome::Passed);
    assert!(clean.errors.is_empty() && clean.retrieval_evidence().is_none());
    let mut cut = settled(
        "[cut]\n",
        TestReport {
            failed: 1,
            ..TestReport::default()
        },
        Some(1),
        false,
        false,
    );
    assert!(
        extract(&root, 6, &cut, before)
            .errors
            .contains(&"output_head_cut; earlier failure blocks unavailable".to_owned())
    );
    cut.output_paged = true;
    let paged = extract(&root, 6, &cut, before);
    assert_eq!(paged.outcome, CheckOutcome::Failed);
    assert_eq!(
        paged.errors,
        [
            "output_paged; failure blocks unavailable",
            "failure_blocks_not_found"
        ]
    );
}

/// The retrieval handoff keeps exact hashes, drops duplicates and stays inside its budget.
#[test]
fn retrieval_handoff_is_bounded_and_hash_bound() {
    let (_scratch, root) = project();
    let before = after_writes();
    let report = extract(
        &root,
        7,
        &settled(&mixed_output(&root), failed_report(), Some(1), false, false),
        before,
    );
    let evidence = report.retrieval_evidence().unwrap();
    assert_eq!(evidence.run_id, "tests-7");
    let anchors: Vec<(&Path, Option<u32>, &str)> = evidence
        .failures
        .iter()
        .map(|f| (f.path.as_path(), f.line, f.message.as_str()))
        .collect();
    assert_eq!(
        anchors,
        [
            (
                Path::new("src/compose.alpha"),
                Some(2),
                "AssertionError: expected 1 to be 2"
            ),
            (
                Path::new("src/request.alpha"),
                Some(1),
                "FAIL  src/test_broken.alpha [ src/test_broken.alpha ]"
            ),
            (
                Path::new("src/test_request.alpha"),
                Some(4),
                "Request > parses"
            ),
        ]
    );
    for failure in &evidence.failures {
        assert_eq!(
            failure.content_hash,
            hash(&root, failure.path.to_str().unwrap())
        );
    }
    assert!(serde_json::to_string(&evidence).unwrap().len() <= 12 * 1024);
}

pub(super) fn origin(owner: &BindingRef, task: &str) -> SupportOrigin {
    SupportOrigin {
        binding: owner.clone(),
        authority_epoch: 1,
        task: *blake3::hash(task.as_bytes()).as_bytes(),
        source_epoch: JobEpoch::default(),
    }
}

pub(super) fn shell(root: &Path, script: &str) -> crate::checks::runner::RunSpec {
    crate::checks::runner::RunSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: root.to_owned(),
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        read_roots: Vec::new(),
        write_roots: Vec::new(),
        read_denies: Vec::new(),
        timeout: Duration::from_secs(10),
        max_output_bytes: 65_536,
    }
}

async fn settle(runs: &TestRuns, check: &PublicCheck) -> PublicCheckReport {
    for _ in 0..400 {
        if let Some(report) = collect(runs, check) {
            return report;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("check {} did not settle", check.run_id);
}

/// A main run keeps the worktree (Busy, nothing spawned); the support run's failure block is
/// collected; cancellation touches only the exact own origin.
#[tokio::test]
async fn support_admission_busy_collect_and_own_cancel() {
    let (_scratch, root) = project();
    let runs = TestRuns::default();
    let owner = BindingRef::fixture("evidence-actor", "evidence-channel", 1);
    let admit_script = |script: &str, task: &str| {
        admit(
            &runs,
            root.clone(),
            shell(&root, script),
            vec!["public-check".into()],
            ALPHA,
            Duration::from_secs(10),
            format!("evidence-{task}"),
            origin(&owner, task),
        )
    };
    assert!(matches!(
        runs.start_admitted(
            root.clone(),
            shell(&root, "sleep 5"),
            vec!["main".into()],
            &owner,
            ALPHA,
            Duration::from_secs(10),
            "main-detail".into(),
            None,
        ),
        StartResult::Started(1)
    ));
    assert!(matches!(
        admit_script("echo never", "task"),
        CheckStart::Busy { running: 1 }
    ));
    assert_eq!(runs.cancel(&root, 1, &owner), Some(true));
    for _ in 0..400 {
        if runs.settled_result(1).is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let CheckStart::Started(check) = admit_script(
        "printf ' FAIL  src/test_broken.alpha [ src/test_broken.alpha ]\\nError: boom\\n \\342\\235\\257 src/compose.alpha:1:1\\nfail\\n'; exit 1",
        "task",
    ) else {
        panic!("public check did not start");
    };
    let report = settle(&runs, &check).await;
    assert_eq!(report.outcome, CheckOutcome::Failed);
    assert_eq!(report.exit, Some(1));
    assert_eq!(report.failures.len(), 1);
    assert_eq!(report.failures[0].kind, FailureKind::Suite);
    assert_eq!(
        report.failures[0].test_file.as_deref(),
        Some(Path::new("src/test_broken.alpha"))
    );
    assert_eq!(
        report
            .frames
            .iter()
            .map(|f| (f.path.as_path(), f.line))
            .collect::<Vec<_>>(),
        [(Path::new("src/compose.alpha"), 1)]
    );
    let CheckStart::Started(own) = admit_script("sleep 10", "task") else {
        panic!("long check did not start");
    };
    let foreign = PublicCheck {
        run_id: own.run_id,
        root: own.root.clone(),
        origin: origin(&owner, "another task"),
        reference: own.reference.clone(),
        before: own.before,
    };
    assert!(!cancel(&runs, &foreign));
    assert!(collect(&runs, &own).is_none());
    assert!(cancel(&runs, &own));
    let cancelled = settle(&runs, &own).await;
    assert_eq!(cancelled.outcome, CheckOutcome::Cancelled);
    assert!(cancelled.frames.is_empty());
}
