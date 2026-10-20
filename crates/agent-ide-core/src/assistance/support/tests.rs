//! Offline pilot proofs through actual authorized worker edits and the existing test executor.

use super::*;
use crate::assistance::host_binding::{
    BindingStatus, parse_candidate, parse_channel_session, parse_hook_event,
};
use std::os::unix::fs::PermissionsExt;

/// Owns one isolated worker/runtime and its private task trace; no credential file exists.
struct Harness {
    /// Removes only this fixture's temporary subtree after normal paths have awaited shutdown.
    scratch: crate::scratch::ScratchDir,
    /// Actual daemon worker with normal workspace/admission/output machinery.
    handle: WorkerHandle,
    /// Canonical project directory beneath the configured allowed root.
    root: PathBuf,
}

impl Harness {
    /// Starts the real worker with scripted decisions and a core-admitted local shell fixture.
    async fn new(
        mode: &str,
        scope: Vec<&str>,
        decisions: Vec<Result<client::Selection, client::Failure>>,
        script: &str,
    ) -> Self {
        crate::lang::testing::install();
        let scratch = crate::scratch::ScratchDir::new("jev-wiring");
        let root = scratch.join("project");
        let runtime = scratch.join("runtime");
        let trace = scratch.join("trace");
        for directory in [&root, &runtime, &trace] {
            std::fs::create_dir(directory).unwrap();
        }
        std::fs::set_permissions(&trace, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(["init", "--quiet"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("main.gamma"), "sym old\nend\n").unwrap();
        std::fs::write(root.join("cache.gamma"), "sym unrelated\nend\n").unwrap();
        let launcher = LauncherConfig::parse(serde_json::to_vec(&serde_json::json!({
            "version":1,"limits":{"queued":8,"details":16,"operation_ms":5000,"output_bytes":32768},"allowed_roots":[root],
            "targets":[{"attachment":"pilot","candidate":root,"git":{"path":"/usr/bin/git","identity":"fixture-git","blake3":blake3::hash(&std::fs::read("/usr/bin/git").unwrap()).to_hex().to_string()},"providers":[]}],
            "jev_pilot":{"mode":mode,"task":"Review the changed behavior using the observed project evidence.","credential_file":scratch.join("credential-never-created"),"egress_allowed":true,"trace_dir":trace,"initial_scope":scope}
        })).unwrap().as_slice()).unwrap();
        let handle = WorkerHandle::new(
            Arc::new(Mutex::new(HostBindingGuard::default())),
            launcher,
            [19; 32],
            Arc::new(Mutex::new(admission_controller())),
        );
        handle.shared.support.state.lock().unwrap().fixture = Some(Fixture {
            decisions: decisions.into(),
            pause: None,
            plan: Some((
                crate::checks::runner::RunSpec {
                    program: "/bin/sh".into(),
                    args: vec!["-c".into(), script.into()],
                    cwd: root.clone(),
                    env: vec![("PATH".into(), "/usr/bin:/bin".into())],
                    read_roots: vec![root.clone()],
                    write_roots: vec![root.clone()],
                    read_denies: Vec::new(),
                    timeout: Duration::from_secs(60),
                    max_output_bytes: 8192,
                },
                crate::lang::testing::ALPHA,
            )),
        });
        handle.start(&runtime).await.unwrap();
        let harness = Self {
            scratch,
            handle,
            root,
        };
        let start = harness
            .call(
                AssistanceTool::Start,
                serde_json::json!({"activation_id":"pilot-start"}),
                "start",
            )
            .await;
        assert!(
            matches!(
                start,
                PeerReply::Complete {
                    kind: ResultKind::Activation,
                    ..
                }
            ),
            "{start:?}"
        );
        harness
    }

    /// Establishes an exact current host binding through the existing hook/MCP authentication seam.
    fn invocation(&self, id: &str) -> ValidatedInvocation {
        let mut guard = self.handle.shared.bindings.lock().unwrap();
        let channel = parse_channel_session(b"pilot-channel").unwrap();
        let hook = parse_hook_event(serde_json::json!({"hook_event_name":"PreToolUse","session_id":"pilot-actor","tool_use_id":id}).to_string().as_bytes()).unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate = parse_candidate(serde_json::json!({"threadId":"pilot-actor","callId":id,"x-codex-turn-metadata":{"turn":"pilot"}}).as_object().unwrap()).unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("valid host binding required");
        };
        invocation
    }

    /// Sends one actual worker operation with its host-authorized binding and fixed trusted target.
    async fn call(&self, tool: AssistanceTool, parameters: Value, id: &str) -> PeerReply {
        let invocation = self.invocation(id);
        self.handle.note_hook(invocation.binding_ref());
        self.handle
            .submit(invocation, tool, parameters, "pilot", None)
            .await
    }

    /// Performs a real authorized read/edit and returns the exact owning binding generation.
    async fn edit(&self) -> BindingRef {
        let before = std::fs::read_to_string(self.root.join("main.gamma")).unwrap();
        let after = before.replace("sym old", "sym new");
        let read = self
            .call(
                AssistanceTool::Read,
                serde_json::json!({"items":[{"path":"main.gamma"}]}),
                "read",
            )
            .await;
        let PeerReply::Complete {
            detail_ref: Some(reference),
            ..
        } = read
        else {
            panic!("read source required: {read:?}");
        };
        let binding = self.invocation("edit-binding").binding_ref().clone();
        // A full-content source_ref requires every actual read page to reach its caller.
        for _ in 0..16 {
            if self.handle.shared.ledger.lock().unwrap().details[&reference]
                .context_page
                .is_none()
            {
                break;
            }
            let page = self
                .handle
                .inspect(binding.clone(), reference.clone(), "pilot", None)
                .await;
            assert!(
                matches!(page, PeerReply::Complete { .. }),
                "{}",
                page.encode().unwrap().as_str()
            );
        }
        assert!(
            self.handle.shared.ledger.lock().unwrap().details[&reference]
                .context_page
                .is_none()
        );
        let reply = self.call(AssistanceTool::Edit,serde_json::json!({"path":"main.gamma","content":after,"source_ref":reference,"operation_id":"pilot-edit"}),"edit").await;
        assert!(
            matches!(
                reply,
                PeerReply::Edit {
                    result: EditResult {
                        outcome: ChangesEditOutcome::Replaced,
                        ..
                    },
                    ..
                }
            ),
            "{}",
            reply.encode().unwrap().as_str()
        );
        assert_eq!(
            std::fs::read_to_string(self.root.join("main.gamma")).unwrap(),
            after
        );
        binding
    }

    /// Joins the exact owned episode without cancelling it; timeout is solely a test hang guard.
    async fn join(&self) {
        let task = self.handle.shared.support.state.lock().unwrap().task.take();
        if let Some(task) = task {
            tokio::time::timeout(Duration::from_secs(8), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    /// Reads this fixture's sole exact private trace, preserving JSON records for assertions.
    fn records(&self) -> Vec<Value> {
        let files = std::fs::read_dir(self.scratch.join("trace"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect::<Vec<_>>();
        assert_eq!(files.len(), 1);
        std::fs::read_to_string(&files[0])
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

/// Scripted valid choice; its absence of transport usage represents an offline decision fixture.
fn choice(
    id: &str,
    confidence: f64,
    noul: Option<f64>,
) -> Result<client::Selection, client::Failure> {
    Ok(client::Selection {
        selected_id: id.into(),
        confidence,
        auto_test_probability: noul,
        request: serde_json::json!({"offline_fixture":true}),
        response: serde_json::json!({"choice":id}),
        usage: None,
        attempt: client::Attempt::default(),
    })
}

/// A physical transport failure with unknown usage, exercised without constructing HTTP.
fn failure(category: &'static str) -> Result<client::Selection, client::Failure> {
    Err(client::Failure {
        category,
        request: None,
        response: None,
        usage_unknown: true,
        attempt: client::Attempt::default(),
    })
}

/// OFF performs the actual core edit without credential I/O, a claim, HTTP or optional execution.
#[tokio::test]
async fn off_real_edit_is_independent() {
    let harness = Harness::new(
        "off",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass",
    )
    .await;
    harness.edit().await;
    assert!(!harness.handle.shared.support.is_busy());
    assert_eq!(
        std::fs::read_dir(harness.scratch.join("trace"))
            .unwrap()
            .count(),
        0
    );
    assert!(!harness.handle.shared.test_runs.running(&harness.root));
    harness.handle.shutdown().await.unwrap();
}

/// Correct selection launches exactly one real admitted child, retaining actual exit/count/output
/// and replay through the existing canonical test channel; stop/restart cannot replay the task.
#[tokio::test]
async fn real_edit_runs_once_with_canonical_output_and_restart_claim() {
    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 0.9, Some(0.9))],
        "printf 'pass\n'; exit 0",
    )
    .await;
    let binding = harness.edit().await;
    harness.join().await;
    let result = harness
        .handle
        .shared
        .test_runs
        .settled_result(1)
        .expect("one real admitted test");
    assert_eq!(result.exit, Some(0));
    assert_eq!(result.report.passed, 1);
    assert!(harness.handle.shared.test_runs.settled_result(2).is_none());
    let origin = harness.handle.shared.test_runs.support_origin(1).unwrap();
    assert_eq!(origin.binding, binding);
    {
        let ledger = harness.handle.shared.ledger.lock().unwrap();
        let detail = &ledger.details[&result.detail_ref];
        assert!(detail.source.is_none() && detail.extra_sources.is_empty());
        assert!(
            detail
                .test_replay
                .as_ref()
                .unwrap()
                .text
                .contains("/bin/sh")
        );
        assert_eq!(detail.context_page.as_ref().unwrap().text, "pass\n");
    }
    let records = harness.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.get("state").is_some())
            .count(),
        1
    );
    let config = harness.handle.shared.support.config.as_ref().unwrap();
    let source = harness
        .handle
        .shared
        .support
        .state
        .lock()
        .unwrap()
        .last_change
        .clone()
        .unwrap();
    let authority = harness
        .handle
        .shared
        .ledger
        .lock()
        .unwrap()
        .details
        .values()
        .find_map(|detail| detail.authority.clone())
        .unwrap();
    let input = Input::capture(
        &harness.handle.shared,
        &authority,
        &source,
        &harness
            .handle
            .shared
            .launcher
            .target("pilot")
            .unwrap()
            .clone(),
    );
    assert!(matches!(
        Trace::claim(config, &input),
        Err("already_claimed_or_trace_unavailable")
    ));
    harness.handle.shutdown().await.unwrap();
}

/// A never-main-read initial scope is privately authorized; expanding preserves the changed-file
/// frontier, reveals new facts, and chooses one test in the second and final decision.
#[tokio::test]
async fn wrong_scope_expands_private_facts_without_main_source_receipt() {
    let harness = Harness::new(
        "jev",
        vec!["cache.gamma"],
        vec![choice("a3", 0.8, Some(0.9)), choice("a4", 0.9, Some(0.9))],
        "printf pass",
    )
    .await;
    let binding = harness.edit().await;
    harness.join().await;
    assert!(harness.handle.shared.test_runs.settled_result(1).is_some());
    let records = harness.records();
    let states = records
        .iter()
        .filter_map(|record| record.get("state").and_then(Value::as_str))
        .map(|state| serde_json::from_str::<Value>(state).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0]["scope"], "cache.gamma");
    assert_eq!(states[1]["scope"], "main.gamma");
    assert!(
        states[0]["file_version"]
            .as_str()
            .unwrap()
            .starts_with("private:")
    );
    assert_ne!(states[0]["source"], states[1]["source"]);
    assert!(
        harness
            .handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .details
            .values()
            .filter(|detail| detail.binding == binding)
            .flat_map(|detail| detail.source.iter().chain(&detail.extra_sources))
            .all(|source| source.path() != Path::new("cache.gamma"))
    );
    let summary = records.last().unwrap();
    assert_eq!(summary["decisions"], 2);
    assert!(summary["read_bytes"].as_u64().unwrap() <= 2 * 1024 * 1024);
    harness.handle.shutdown().await.unwrap();
}

/// Every closed refusal remains fail-open after the real edit, consumes one choice and never
/// retries or launches a test. Two inspect decisions exhaust the same fixed episode budget.
#[tokio::test]
async fn failures_low_confidence_unknown_id_and_budget_remain_inactive() {
    for decisions in [
        vec![choice("a3", 0.49, Some(1.0))],
        vec![choice("a3", 1.0, Some(0.79))],
        vec![choice("a0", 1.0, None)],
        vec![choice("unknown", 1.0, None)],
        vec![failure("http_status_429")],
        vec![failure("response_json_invalid")],
        vec![failure("http_timeout")],
        vec![
            choice("a2", 1.0, None),
            choice("a2", 1.0, None),
            choice("a3", 1.0, Some(1.0)),
        ],
    ] {
        let harness = Harness::new("jev", Vec::new(), decisions, "printf pass").await;
        harness.edit().await;
        harness.join().await;
        assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
        assert!(
            harness.records().last().unwrap()["decisions"]
                .as_u64()
                .unwrap()
                <= 2
        );
        harness.handle.shutdown().await.unwrap();
    }
}

/// A source change at a deterministic decision boundary invalidates the response; a same-root
/// foreign test is exclusion only and is never reused, cancelled or claimed as support output.
#[tokio::test]
async fn stale_response_and_foreign_busy_never_admit_or_reuse() {
    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass",
    )
    .await;
    let ready = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    harness
        .handle
        .shared
        .support
        .state
        .lock()
        .unwrap()
        .fixture
        .as_mut()
        .unwrap()
        .pause = Some((ready.clone(), resume.clone()));
    harness.edit().await;
    tokio::time::timeout(Duration::from_secs(3), ready.notified())
        .await
        .unwrap();
    std::fs::write(
        harness.root.join("main.gamma"),
        "sym externally_changed\nend\n",
    )
    .unwrap();
    resume.notify_one();
    harness.join().await;
    assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
    harness.handle.shutdown().await.unwrap();

    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass",
    )
    .await;
    let foreign = BindingRef::fixture("foreign", "foreign", 1);
    assert!(matches!(
        harness.handle.shared.test_runs.start(
            harness.root.clone(),
            vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
            crate::lang::testing::ALPHA,
            Duration::from_secs(60),
            "foreign-result".into(),
            &foreign
        ),
        StartResult::Started(1)
    ));
    let binding = harness.edit().await;
    harness.join().await;
    assert!(harness.handle.shared.test_runs.support_origin(1).is_none());
    interrupt(&harness.handle.shared, Some(&binding), "stop").await;
    assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
    assert!(
        harness
            .handle
            .shared
            .test_runs
            .cancel(&harness.root, 1, &foreign)
            .unwrap()
    );
    let notified = harness.handle.shared.support.settled.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    if harness.handle.shared.test_runs.settled_result(1).is_none() {
        tokio::time::timeout(Duration::from_secs(3), notified)
            .await
            .unwrap();
    }
    assert!(
        harness
            .handle
            .shared
            .test_runs
            .settled_result(1)
            .unwrap()
            .cancelled
    );
    harness.handle.shutdown().await.unwrap();
}

/// Main-test priority cancels only the owned support process and waits for real cleanup before
/// admitting the main child. Stop during an outstanding decision joins it without any test.
#[tokio::test]
async fn main_priority_and_stop_join_actual_owned_work() {
    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass; sleep 30",
    )
    .await;
    harness.edit().await;
    tokio::time::timeout(
        Duration::from_secs(3),
        harness.handle.shared.support.running.notified(),
    )
    .await
    .unwrap();
    let began = std::time::Instant::now();
    let reply = harness
        .call(
            AssistanceTool::Test,
            serde_json::json!({"command":["/bin/sh","-c","printf pass"],"budget_ms":1000}),
            "main-test",
        )
        .await;
    assert!(
        began.elapsed() < Duration::from_secs(2),
        "main priority cleanup delay: {:?}",
        began.elapsed()
    );
    assert!(
        harness
            .handle
            .shared
            .test_runs
            .settled_result(1)
            .unwrap()
            .cancelled
    );
    assert!(harness.handle.shared.test_runs.support_origin(2).is_none());
    assert!(!matches!(reply, PeerReply::Error { .. }), "{reply:?}");
    harness.handle.shutdown().await.unwrap();

    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass",
    )
    .await;
    let ready = Arc::new(Notify::new());
    let resume = Arc::new(Notify::new());
    harness
        .handle
        .shared
        .support
        .state
        .lock()
        .unwrap()
        .fixture
        .as_mut()
        .unwrap()
        .pause = Some((ready.clone(), resume));
    harness.edit().await;
    tokio::time::timeout(Duration::from_secs(3), ready.notified())
        .await
        .unwrap();
    let stop = harness
        .handle
        .stop(harness.invocation("stop"), "pilot")
        .await;
    assert!(!matches!(stop, PeerReply::Error { .. }), "{stop:?}");
    assert!(!harness.handle.shared.support.is_busy());
    assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
    harness.handle.shutdown().await.unwrap();
}

/// Invalid helper settings disable only the helper; OFF ignores nonexistent credential files.
#[test]
fn malformed_helper_is_safe_and_closed() {
    assert_eq!(
        Pilot::new(Some(&serde_json::json!({"mode":"jev","surprise":true}))).reason(),
        "config_invalid"
    );
    assert_eq!(
        Pilot::new(Some(&serde_json::json!({"mode":"jev"}))).reason(),
        "config_incomplete"
    );
    assert_eq!(
        Pilot::new(Some(
            &serde_json::json!({"mode":"off","credential_file":"/nonexistent"})
        ))
        .reason(),
        "off"
    );
}

/// Both full-content and batched line edits show the actual changed region past the first 4 KiB,
/// using the existing verified before/after hook rather than an invented source-diff authority.
#[tokio::test]
async fn actual_change_window_covers_single_and_batch_edit_beyond_prefix() {
    for batch in [false, true] {
        let harness = Harness::new(
            "jev",
            Vec::new(),
            vec![choice("a0", 1.0, None)],
            "printf pass",
        )
        .await;
        let before = format!(
            "{}sym old\nend\n",
            "// unchanged padding for a long source line that is not the edited region\n"
                .repeat(100)
        );
        std::fs::write(harness.root.join("main.gamma"), &before).unwrap();
        if batch {
            let read = harness
                .call(
                    AssistanceTool::Read,
                    serde_json::json!({"items":[{"path":"main.gamma"}]}),
                    "batch-read",
                )
                .await;
            assert!(matches!(read, PeerReply::Complete { .. }), "{read:?}");
            let reply=harness.call(AssistanceTool::Edit,serde_json::json!({"path":"main.gamma","operation_id":"batch-span","changes":[{"lines":"101-101","content":"sym new"}]}),"batch-edit").await;
            assert!(
                matches!(reply, PeerReply::EditBatch { .. } | PeerReply::Edit { .. }),
                "{reply:?}"
            );
        } else {
            harness.edit().await;
        }
        harness.join().await;
        let records = harness.records();
        let state: Value = serde_json::from_str(
            records
                .iter()
                .find_map(|record| record["state"].as_str())
                .unwrap(),
        )
        .unwrap();
        assert!(
            state["confirmed_change"]["first_differing_byte"]
                .as_u64()
                .unwrap()
                > 4096,
            "{state}"
        );
        assert!(
            state["confirmed_change"]["current"]["text"]
                .as_str()
                .unwrap()
                .contains("sym new")
        );
        assert_eq!(
            state["confirmed_change"]["current"]["coverage"],
            "partial current source window"
        );
        harness.handle.shutdown().await.unwrap();
    }
}

/// Token totals distinguish reported counters from absent cache usage and interrupted attempts.
#[test]
fn token_totals_preserve_unknown_coverage_and_exact_sums() {
    let budget = Budget {
        attempts: 2,
        usages: vec![
            Some(
                serde_json::json!({"input_tokens":40,"output_tokens":9,"cache_read_input_tokens":12}),
            ),
            None,
        ],
        ..Default::default()
    };
    let totals = token_totals(&budget);
    assert_eq!(totals["input_tokens"]["known_sum_decimal"], "40");
    assert_eq!(totals["input_tokens"]["unknown_attempts"], 1);
    assert_eq!(totals["output_tokens"]["known_sum_decimal"], "9");
    assert_eq!(totals["cache_read_input_tokens"]["known_sum_decimal"], "12");
    assert!(totals["cache_tokens"]["known_sum_decimal"].is_null());
    assert_eq!(totals["cache_tokens"]["unknown_attempts"], 2);
}

/// A new real worker boot and binding can edit normally but the durable same-task claim blocks
/// any second helper decision or test, even though its in-memory episode state starts empty.
#[tokio::test]
async fn actual_worker_restart_does_not_replay_helper_episode() {
    let mut harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a0", 1.0, None)],
        "printf pass",
    )
    .await;
    harness.edit().await;
    harness.join().await;
    let launcher = harness.handle.shared.launcher.clone();
    let plan = harness
        .handle
        .shared
        .support
        .state
        .lock()
        .unwrap()
        .fixture
        .as_ref()
        .unwrap()
        .plan
        .clone();
    harness.handle.shutdown().await.unwrap();
    let runtime = harness.scratch.join("restarted-runtime");
    std::fs::create_dir(&runtime).unwrap();
    let handle = WorkerHandle::new(
        Arc::new(Mutex::new(HostBindingGuard::default())),
        launcher,
        [29; 32],
        Arc::new(Mutex::new(admission_controller())),
    );
    handle.shared.support.state.lock().unwrap().fixture = Some(Fixture {
        decisions: vec![choice("a3", 1.0, Some(1.0))].into(),
        plan,
        pause: None,
    });
    handle.start(&runtime).await.unwrap();
    harness.handle = handle;
    let start = harness
        .call(
            AssistanceTool::Start,
            serde_json::json!({"activation_id":"restarted"}),
            "restart-start",
        )
        .await;
    assert!(
        matches!(
            start,
            PeerReply::Complete {
                kind: ResultKind::Activation,
                ..
            }
        ),
        "{start:?}"
    );
    let read = harness
        .call(
            AssistanceTool::Read,
            serde_json::json!({"items":[{"path":"main.gamma"}]}),
            "restart-read",
        )
        .await;
    let PeerReply::Complete {
        detail_ref: Some(reference),
        ..
    } = read
    else {
        panic!("{read:?}");
    };
    let edit=harness.call(AssistanceTool::Edit,serde_json::json!({"path":"main.gamma","source_ref":reference,"operation_id":"after-restart","content":"sym after_restart\nend\n"}),"restart-edit").await;
    assert!(
        matches!(
            edit,
            PeerReply::Edit {
                result: EditResult {
                    outcome: ChangesEditOutcome::Replaced,
                    ..
                },
                ..
            }
        ),
        "{edit:?}"
    );
    harness.join().await;
    assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
    assert_eq!(
        harness
            .handle
            .shared
            .support
            .state
            .lock()
            .unwrap()
            .fixture
            .as_ref()
            .unwrap()
            .decisions
            .len(),
        1
    );
    assert_eq!(
        harness
            .records()
            .iter()
            .filter(|record| record.get("state").is_some())
            .count(),
        1
    );
    harness.handle.shutdown().await.unwrap();
}

/// A real admitted support child whose source changes retains honest cancellation/exit/counts
/// and marks only that optional result historical through the ordinary canonical channel.
#[tokio::test]
async fn started_optional_result_is_historical_after_real_source_change() {
    let harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a3", 1.0, Some(1.0))],
        "printf pass; sleep 30",
    )
    .await;
    let binding = harness.edit().await;
    tokio::time::timeout(
        Duration::from_secs(3),
        harness.handle.shared.support.running.notified(),
    )
    .await
    .unwrap();
    std::fs::write(harness.root.join("main.gamma"), "sym native_change\nend\n").unwrap();
    harness.handle.native_hint(binding.clone());
    interrupt(&harness.handle.shared, Some(&binding), "stop").await;
    let result = harness.handle.shared.test_runs.settled_result(1).unwrap();
    assert!(result.cancelled && result.stopped);
    assert!(harness.handle.shared.support_test_historical(1));
    {
        let ledger = harness.handle.shared.ledger.lock().unwrap();
        let PeerReply::Complete { text, .. } = &ledger.details[&result.detail_ref].reply else {
            panic!("canonical test result required");
        };
        assert!(text.contains("historical: source changed after optional test admission"));
        assert!(text.contains("cancelled"));
    }
    harness.handle.shutdown().await.unwrap();
}
