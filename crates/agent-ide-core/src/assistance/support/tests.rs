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
    /// Whether the host has supplied a protected context hook; independent of write authority.
    context_known: bool,
    /// Original activation response, used to prove early handoff without any edit.
    activation: PeerReply,
}

impl Harness {
    /// Starts the real worker with scripted decisions and a core-admitted local shell fixture.
    async fn new(
        mode: &str,
        scope: Vec<&str>,
        decisions: Vec<Result<client::Selection, client::Failure>>,
        script: &str,
    ) -> Self {
        Self::new_with_prepared(mode, scope, decisions, script, None).await
    }

    /// Same actual worker, with an operator packet created before activation, never a model call.
    async fn new_with_prepared(
        mode: &str,
        scope: Vec<&str>,
        decisions: Vec<Result<client::Selection, client::Failure>>,
        script: &str,
        prepared_case: Option<&str>,
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
        let prepared_path = trace.join("prepared.json");
        let task = "Review the changed behavior using the observed project evidence.";
        if let Some(case) = prepared_case {
            let mut packet = serde_json::json!({
                "schema":"jev-prepared-context-v1", "task":task, "model":"jev-1.13-free",
                "selected_id":"bundle", "confidence":0.73,
                "evidence":[{"path":"main.gamma","version":blake3::hash(b"sym old\nend\n").to_hex().to_string(),
                    "start_byte":0,"end_byte":8,"text":"sym old\n",
                    "relation":"observed_only; not proof of test coverage","source_bytes":12}],
                "run_facts":{"current_actor_runs":"unknown"},"gaps":["contract unresolved"]
            });
            packet["evidence"].as_array_mut().unwrap().push(serde_json::json!({
                "path":"cache.gamma","version":blake3::hash(b"sym unrelated\nend\n").to_hex().to_string(),
                "start_byte":0,"end_byte":14,"text":"sym unrelated\n",
                "relation":"observed_only; not proof of test coverage","source_bytes":18
            }));
            match case {
                "stale" => packet["evidence"][0]["version"] = serde_json::json!("0".repeat(64)),
                "wrong_text" => packet["evidence"][0]["text"] = serde_json::json!("sym lie\n"),
                "wrong_task" => packet["task"] = serde_json::json!("another task"),
                "outside" => packet["evidence"][0]["path"] = serde_json::json!("../outside.gamma"),
                "low_confidence" => packet["confidence"] = serde_json::json!(0.49),
                _ => {}
            }
            std::fs::write(&prepared_path, serde_json::to_vec(&packet).unwrap()).unwrap();
            std::fs::set_permissions(
                &prepared_path,
                std::fs::Permissions::from_mode(if case == "public" { 0o644 } else { 0o600 }),
            )
            .unwrap();
        }
        if prepared_case == Some("source_link") {
            let outside = scratch.join("outside.gamma");
            std::fs::write(&outside, "PRIVATE_OUTSIDE_PROJECT\n").unwrap();
            std::fs::remove_file(root.join("main.gamma")).unwrap();
            std::os::unix::fs::symlink(outside, root.join("main.gamma")).unwrap();
        }
        if prepared_case == Some("symlink") {
            let real = trace.join("real-prepared.json");
            std::fs::rename(&prepared_path, &real).unwrap();
            std::os::unix::fs::symlink(real, &prepared_path).unwrap();
        }
        let launcher = LauncherConfig::parse(serde_json::to_vec(&serde_json::json!({
            "version":1,"limits":{"queued":8,"details":16,"operation_ms":5000,"output_bytes":32768},"allowed_roots":[root],
            "targets":[{"attachment":"pilot","candidate":root,"git":{"path":"/usr/bin/git","identity":"fixture-git","blake3":blake3::hash(&std::fs::read("/usr/bin/git").unwrap()).to_hex().to_string()},"providers":[]}],
            "jev_pilot":{"mode":mode,"task":"Review the changed behavior using the observed project evidence.","base_url":"https://fixture.invalid", "api_path":"v1/systemone", "model":"jev-1.13-free",
            "credential_file":scratch.join("credential-never-created"),"egress_allowed":true,"trace_dir":trace,"initial_scope":scope,"prepared_context_file":prepared_case.map(|_|prepared_path)}
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
        let mut harness = Self {
            scratch,
            handle,
            root,
            context_known: true,
            activation: PeerReply::Pending {
                detail_ref: "not-started".into(),
            },
        };
        let start = harness
            .call(
                AssistanceTool::Start,
                serde_json::json!({"activation_id":"pilot-start","read_only":prepared_case==Some("reader")}),
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
        harness.activation = start;
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
        if self.context_known {
            self.handle.note_hook(invocation.binding_ref());
        }
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
    let menus = records
        .iter()
        .filter_map(|record| record["options"].as_array())
        .collect::<Vec<_>>();
    assert!(
        menus[0].iter().all(|option| option["id"] != "a2"),
        "current scope has no new facts"
    );
    assert!(
        menus[1]
            .iter()
            .all(|option| !matches!(option["id"].as_str(), Some("a2" | "a3"))),
        "final private reads cannot benefit a later decision"
    );
    assert!(
        menus[1]
            .iter()
            .any(|option| option["id"] == "a4" && option["auto_test"] == true),
        "test capability keeps its stable ID"
    );

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
/// retries or launches a test. Unoffered private reads cannot consume another useful action.
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

/// Full-content, batched line and multi-file edits retain the changed region past the first 4 KiB,
/// using the existing verified before/after hook rather than an invented source-diff authority.
#[tokio::test]
async fn actual_change_window_covers_single_and_batch_edit_beyond_prefix() {
    for batch in [0, 1, 2] {
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
        if batch != 0 {
            let read = harness
                .call(
                    AssistanceTool::Read,
                    serde_json::json!({"items":[{"path":"main.gamma"},{"path":"cache.gamma"}]}),
                    "batch-read",
                )
                .await;
            assert!(matches!(read, PeerReply::Complete { .. }), "{read:?}");
            let parameters = if batch == 2 {
                serde_json::json!({"operation_id":"multi-span","files":[
                    {"path":"main.gamma","changes":[{"old":"sym old","new":"sym new"}]},
                    {"path":"cache.gamma","changes":[{"old":"sym unrelated","new":"sym changed"}]}
                ]})
            } else {
                serde_json::json!({"path":"main.gamma","operation_id":"batch-span","changes":[{"lines":"101-101","content":"sym new"}]})
            };
            let reply = harness
                .call(AssistanceTool::Edit, parameters, "batch-edit")
                .await;
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

/// A real patch with an unknown main context still supplies its verified geometry to the helper,
/// while the missing main read-memory epoch remains unknown and no receipt is fabricated.
#[tokio::test]
async fn unknown_main_context_keeps_verified_helper_change_geometry() {
    let mut harness = Harness::new(
        "jev",
        Vec::new(),
        vec![choice("a0", 1.0, None)],
        "printf pass",
    )
    .await;
    harness.context_known = false;
    harness
        .handle
        .shared
        .ledger
        .lock()
        .unwrap()
        .context_epochs
        .clear();
    let before = format!(
        "{}sym old\nend\n",
        "// unchanged source padding before the actual changed region\n".repeat(100)
    );
    std::fs::write(harness.root.join("main.gamma"), before).unwrap();
    let read = harness
        .call(
            AssistanceTool::Read,
            serde_json::json!({"items":[{"path":"main.gamma","lines":"101-101"}]}),
            "partial-read",
        )
        .await;
    assert!(matches!(read, PeerReply::Complete { .. }), "{read:?}");
    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@\n-sym old\n+sym new\n*** End Patch",
        harness.root.join("main.gamma").display()
    );
    let reply = harness
        .call(
            AssistanceTool::Edit,
            serde_json::json!({"operation_id":"unknown-context-patch","patch":patch}),
            "patch",
        )
        .await;
    assert!(
        matches!(reply, PeerReply::EditBatch { .. } | PeerReply::Edit { .. }),
        "{reply:?}"
    );
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
            .is_some_and(|offset| offset > 4096),
        "{state}"
    );
    assert!(
        state["confirmed_change"]["current"]["text"]
            .as_str()
            .unwrap()
            .contains("sym new")
    );
    assert!(
        harness
            .handle
            .shared
            .ledger
            .lock()
            .unwrap()
            .context_epochs
            .is_empty()
    );
    harness.handle.shutdown().await.unwrap();
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

/// Reads the actual early detail handle from the completed activation, never from private state.
fn early_reference(harness: &Harness) -> Option<String> {
    let PeerReply::Complete { text, .. } = &harness.activation else {
        return None;
    };
    text.lines()
        .find_map(|line| line.strip_prefix("Jev context ready: "))
        .and_then(|line| line.split(';').next())
        .map(str::to_owned)
}

/// The real activation retains a source-backed result before any code edit or model inference.
#[tokio::test]
async fn prepared_context_is_early_private_and_inspectable() {
    let harness =
        Harness::new_with_prepared("jev", vec![], vec![], "printf pass", Some("valid")).await;
    let reference = early_reference(&harness).expect("activation must expose the ready handle");
    let binding = harness.invocation("prepared-context").binding_ref().clone();
    {
        let ledger = harness.handle.shared.ledger.lock().unwrap();
        assert!(
            ledger.read_memory.is_empty(),
            "private reads cannot become main read receipts"
        );
        let detail = &ledger.details[&reference];
        assert_eq!(detail.selection.0, AssistanceTool::Inspect);
        assert!(
            admitted_edit_source(detail, &binding, &reference, "main.gamma").is_none(),
            "a partial context must not authorize a full-file replacement"
        );
    }
    let reply = harness
        .handle
        .inspect(binding.clone(), reference, "pilot", None)
        .await;
    let text = reply.encode().unwrap();
    assert!(text.as_str().contains("sym old"), "{text:?}");
    assert!(text.as_str().contains("unresolved"));
    assert!(
        harness
            .handle
            .shared
            .support
            .state
            .lock()
            .unwrap()
            .task
            .is_none()
    );
    assert!(harness.handle.shared.test_runs.settled_result(1).is_none());
    harness.handle.shutdown().await.unwrap();
}

/// Task mismatch, stale sources, low confidence and unsafe artifacts fail open with no code body.
#[tokio::test]
async fn prepared_context_refusals_preserve_normal_activation() {
    for case in [
        "stale",
        "wrong_task",
        "outside",
        "low_confidence",
        "public",
        "symlink",
        "source_link",
        "wrong_text",
    ] {
        let harness =
            Harness::new_with_prepared("jev", vec![], vec![], "printf pass", Some(case)).await;
        assert!(early_reference(&harness).is_none(), "{case}");
        let text = harness.activation.encode().unwrap();
        assert!(
            text.as_str().contains("Jev context unavailable"),
            "{case}: {text:?}"
        );
        assert!(!text.as_str().contains("source_windows"));
        assert!(
            harness
                .handle
                .shared
                .ledger
                .lock()
                .unwrap()
                .read_memory
                .is_empty()
        );
        assert!(
            harness
                .handle
                .shared
                .support
                .state
                .lock()
                .unwrap()
                .task
                .is_none()
        );
        harness.handle.shutdown().await.unwrap();
    }
}

/// An outside/native source change after preparation suppresses the retained stale body.
#[tokio::test]
async fn prepared_context_native_write_is_reproved_on_inspection() {
    let harness =
        Harness::new_with_prepared("jev", vec![], vec![], "printf pass", Some("valid")).await;
    let reference = early_reference(&harness).unwrap();
    std::fs::write(harness.root.join("main.gamma"), "sym foreign\nend\n").unwrap();
    let binding = harness
        .invocation("native-after-context")
        .binding_ref()
        .clone();
    let reply = harness
        .handle
        .inspect(binding, reference, "pilot", None)
        .await;
    assert!(matches!(reply, PeerReply::Error { .. }), "{reply:?}");
    assert!(!reply.encode().unwrap().as_str().contains("sym old"));
    harness.handle.shutdown().await.unwrap();
}

/// OFF and read-only activations do not load or publish a prepared source packet.
#[tokio::test]
async fn prepared_context_off_and_reader_remain_inactive() {
    for (mode, case) in [("off", "public"), ("jev", "reader")] {
        let harness =
            Harness::new_with_prepared(mode, vec![], vec![], "printf pass", Some(case)).await;
        assert!(early_reference(&harness).is_none());
        assert!(
            !harness
                .activation
                .encode()
                .unwrap()
                .as_str()
                .contains("Jev context")
        );
        {
            let ledger = harness.handle.shared.ledger.lock().unwrap();
            assert!(ledger.read_memory.is_empty());
            assert!(
                ledger
                    .details
                    .values()
                    .all(|detail| detail.source.is_none())
            );
        }
        harness.handle.shutdown().await.unwrap();
    }
}

/// Configuration alone toggles assistance and selects unrelated compatible transports; no I/O.
#[test]
fn mode_and_provider_are_configuration_only() {
    let off = Pilot::new(Some(&serde_json::json!({"mode":"off"})));
    assert!(off.config.is_none());
    let invalid_off = Pilot::new(Some(
        &serde_json::json!({"mode":"off", "base_url":"not a URL",
        "credential_file":"/nonexistent/key"}),
    ));
    assert!(invalid_off.config.is_none());
    for label in ["provider-a", "provider-b"] {
        let enabled = Pilot::new(Some(&serde_json::json!({"mode":"jev", "route":label,
            "base_url":format!("https://{label}.invalid/api"), "api_path":"decide",
            "model":format!("{label}-classifier"), "credential_file":"/nonexistent/key",
            "task":"Review behavior", "trace_dir":"/nonexistent/trace", "egress_allowed":true})));
        let config = enabled.config.unwrap();
        assert_eq!(config.selected_route().name(), label);
        assert_eq!(
            config.selected_route().model(),
            format!("{label}-classifier")
        );
        assert_eq!(
            config.selected_route().endpoint(),
            format!("https://{label}.invalid/api/decide")
        );
    }
}
