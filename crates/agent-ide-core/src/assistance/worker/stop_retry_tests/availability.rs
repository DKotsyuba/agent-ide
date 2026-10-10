//! Availability and navigation tests: a faulted call hierarchy, bare-name resolution and
//! construction usages, stale writer takeover, the start card's servers line and approximate
//! answers while a server loads. They reuse the stop-retry fixtures: a real Git worktree, a real
//! store and the scripted epsilon language server of `lang::testing`.

use super::*;

/// The text of a complete reply; panics with the whole reply when it is anything else.
fn complete_text(reply: &PeerReply) -> &str {
    match reply {
        PeerReply::Complete { text, .. } => text,
        other => panic!("expected a complete reply, got {other:?}"),
    }
}

/// Runs one symbol job of `actor` for `a.epsilon#a` through the production tool path and returns
/// its outcome; unlike `scripted_tool` it allows the job to park (a loading session).
async fn scripted_symbol_attempt(
    worker: &mut Worker<'_>,
    actor: &str,
    call: &str,
) -> Result<PeerReply, FailureCode> {
    let invocation = production_call(worker, actor, call);
    let (cancel_sender, cancel) = watch::channel(false);
    let _keep = cancel_sender;
    let mut job = Job {
        reference: format!("attempt-{call}"),
        invocation,
        tool: AssistanceTool::Symbol,
        parameters: serde_json::json!({"symbol":"a.epsilon#a","usages":false,"callers":0}),
        target: provider_target(&worker.runtime),
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        cancel,
        stop_reply: None,
        native_epoch: 0,
        failure_detail: None,
        format_note: None,
        check_scheduled: false,
        park_until: None,
        stage: None,
        session_binding: None,
    };
    worker.providers.begin_job();
    let outcome = worker.symbol(&mut job).await.map(|(reply, _, _)| reply);
    worker.settle_session_health(&job).await;
    outcome
}

/// Runs a production `ide.start` of `actor` with the epsilon fixture provider configured and
/// returns the reply text (`read_only` selects the mode; `id` is the activation id).
async fn provider_start_text(worker: &mut Worker<'_>, actor: &str, id: &str) -> String {
    crate::lang::testing::install();
    let invocation = production_call(worker, actor, id);
    let (_cancel_sender, cancel) = watch::channel(false);
    let mut job = Job {
        reference: format!("provider-text-{id}"),
        invocation,
        tool: AssistanceTool::Start,
        parameters: serde_json::json!({"activation_id":actor,"read_only":true}),
        target: provider_target(&worker.runtime),
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        cancel,
        stop_reply: None,
        native_epoch: 0,
        failure_detail: None,
        format_note: None,
        check_scheduled: false,
        park_until: None,
        stage: None,
        session_binding: None,
    };
    let (reply, _, _) = worker.activate(&mut job).await.unwrap();
    complete_text(&reply).to_owned()
}

/// The start card's servers line reports the worktree's session as it is: `not started` (with
/// the lazy-start wording) before the first semantic call, `warming` while a session loads and
/// `ready` once one answers. The start itself starts nothing.
#[tokio::test]
async fn the_servers_line_reports_the_session_as_it_is() {
    use crate::lang::testing::fixture_serve_session;
    let fixture = Fixture::new();
    epsilon_source(&fixture);
    std::fs::write(fixture.root.join("a.epsilon"), "sym a\nend\n").unwrap();
    std::fs::write(fixture.root.join("epsilon.toml"), "").unwrap();
    git_commit(&fixture.root, "scripted source");
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let first = provider_start_text(&mut worker, "card-reader", "card-1").await;
    let servers = |text: &str| {
        text.lines()
            .find(|line| line.starts_with("servers: "))
            .unwrap_or_else(|| panic!("no servers line: {text}"))
            .to_owned()
    };
    assert_eq!(
        servers(&first),
        format!(
            "servers: epsilon {}",
            crate::project::not_started_state(crate::lang::testing::EPSILON)
        )
    );
    assert!(
        servers(&first).contains("answers approximately at once"),
        "{first}"
    );
    let reader = production_call(&worker, "card-reader", "card-bind")
        .binding_ref()
        .clone();
    // A session that loads: the first symbol call starts it, the card then reads `warming`.
    fixture_serve_session(&reader, &["loading"]);
    let _ = scripted_symbol_attempt(&mut worker, "card-reader", "card-load").await;
    let warming = provider_start_text(&mut worker, "card-reader", "card-warm").await;
    assert_eq!(
        servers(&warming),
        format!(
            "servers: epsilon {}",
            crate::project::warming_state(crate::lang::testing::EPSILON)
        ),
        "{warming}"
    );
    worker.release_live(&reader).await;
    fixture_serve_session(&reader, &[]);
    let (opened, _) = scripted_tool(
        &mut worker,
        "card-reader",
        "card-symbol",
        AssistanceTool::Symbol,
        serde_json::json!({"symbol":"a.epsilon#a","usages":false,"callers":0}),
    )
    .await;
    opened.expect("the scripted session answers");
    let again = provider_start_text(&mut worker, "card-reader", "card-2").await;
    assert_eq!(servers(&again), "servers: epsilon ready", "{again}");
}

/// A graph whose call-hierarchy request fails answers `unavailable (<cause>)`, never a normal
/// graph with 0 edges (0.10.8 answered `1 nodes, 0 edges`). The scripted epsilon server fails
/// every call-hierarchy request.
#[tokio::test]
async fn faulted_call_hierarchy_renders_unavailable_not_an_empty_graph() {
    use crate::lang::testing::fixture_serve_session;
    let fixture = Fixture::new();
    epsilon_source(&fixture);
    std::fs::write(fixture.root.join("a.epsilon"), "sym a\nend\n").unwrap();
    git_commit(&fixture.root, "scripted source");
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let reader = provider_start(&mut worker, "gf-reader", "gf-start", true, None)
        .await
        .unwrap();
    fixture_serve_session(&reader, &[]);
    let (reply, _) = scripted_tool(
        &mut worker,
        "gf-reader",
        "gf-1",
        AssistanceTool::Graph,
        serde_json::json!({"symbol":"a.epsilon#a","direction":"callers","depth":2}),
    )
    .await;
    let reply = reply.expect("the graph answers");
    let text = complete_text(&reply);
    assert!(
        text.contains("unavailable (incoming calls request failed)"),
        "{text}"
    );
    assert!(!text.contains("0 edges"), "{text}");
}

/// A bare name whose matches are a struct and its impl block (one address) resolves in one
/// `ide.symbol` call and one `ide.graph` call; 0.10.8 answered `ambiguous_symbol: a matches 1
/// symbols` for the card. The scripted `structImpl` session answers exactly that pair.
#[tokio::test]
async fn bare_name_of_a_struct_and_its_impl_resolves_in_one_call() {
    use crate::lang::testing::fixture_serve_session;
    let fixture = Fixture::new();
    epsilon_source(&fixture);
    std::fs::write(fixture.root.join("a.epsilon"), "sym a\nend\n").unwrap();
    git_commit(&fixture.root, "scripted source");
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let reader = provider_start(&mut worker, "bn-reader", "bn-start", true, None)
        .await
        .unwrap();
    fixture_serve_session(&reader, &["structImpl"]);
    let cases = [
        (
            AssistanceTool::Symbol,
            serde_json::json!({"symbol":"a","usages":false,"callers":0}),
            "symbol: a — struct",
        ),
        (
            AssistanceTool::Graph,
            serde_json::json!({"symbol":"a","direction":"callers","depth":1}),
            "graph: callers of a.epsilon#a",
        ),
    ];
    for (index, (tool, parameters, start)) in cases.into_iter().enumerate() {
        let (reply, _) = scripted_tool(
            &mut worker,
            "bn-reader",
            &format!("bn-{index}"),
            tool,
            parameters,
        )
        .await;
        let reply = reply.expect("the bare name resolves");
        let text = complete_text(&reply);
        assert!(!text.contains("ambiguous_symbol"), "{text}");
        assert!(text.starts_with(start), "{text}");
    }
}

/// Fires the post-hook of `actor`'s call `id` on the fixture channel, settling that call so the
/// actor has no tool call in flight.
fn settle_call(worker: &Worker<'_>, actor: &str, id: &str) {
    let channel = parse_channel_session(b"stop-retry").unwrap();
    let hook = parse_hook_event(
        serde_json::json!({"hook_event_name":"PostToolUse","session_id":actor,"tool_use_id":id})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    let status = worker
        .shared
        .bindings
        .lock()
        .unwrap()
        .observe_hook(hook, channel);
    assert!(matches!(status, BindingStatus::Settled(_)), "{status:?}");
}

/// What the guard answers a new ordinary call of `actor` (its pre-hook, then validation).
fn next_call_of(worker: &Worker<'_>, actor: &str, id: &str) -> BindingStatus {
    let channel = parse_channel_session(b"stop-retry").unwrap();
    let mut guard = worker.shared.bindings.lock().unwrap();
    let hook = parse_hook_event(
        serde_json::json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":id})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    guard.observe_hook(hook, channel.clone());
    let candidate = parse_candidate(
        serde_json::json!({"threadId":actor,"callId":id,"x-codex-turn-metadata":{"turn":"stop-retry"}})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    guard.validate_active(candidate, channel)
}

/// Runs a writer `ide.start` for `actor` and returns its reply (or failure) and failure detail.
async fn writer_start(
    worker: &mut Worker<'_>,
    actor: &str,
    call: &str,
) -> (Result<PeerReply, FailureCode>, Option<String>) {
    let (mut job, _cancel) = start_job(
        worker,
        actor,
        call,
        serde_json::json!({"activation_id":call}),
    );
    let outcome = worker.activate(&mut job).await.map(|(reply, _, _)| reply);
    (outcome, job.failure_detail)
}

/// A writer whose carrier (MCP front) ended with nothing heard from it afterwards is taken over by
/// the next actor's start, which names the holder and the evidence; the displaced actor's next
/// call is told `taken_over`.
#[tokio::test]
async fn a_writer_whose_session_ended_is_taken_over() {
    use crate::assistance::host_binding::{BindingUnavailable, TakeoverReason};
    let fixture = Fixture::new();
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let (holder, _) = production_start(&mut worker, "gone-actor", "gone-start").await;
    {
        let mut guard = worker.shared.bindings.lock().unwrap();
        guard.carrier_opened(4242);
        guard.note_carrier(&holder, 4242);
        guard.carrier_closed(4242);
    }
    let (outcome, detail) = writer_start(&mut worker, "next-actor", "next-start").await;
    let reply = outcome.unwrap_or_else(|code| panic!("{code:?}: {detail:?}"));
    let text = complete_text(&reply);
    assert!(
        text.contains("mode: writer; took over the writer from actor gone-actor (activation ")
            && text.contains(", last activity "),
        "{text}"
    );
    assert!(text.contains("; its session ended); "), "{text}");
    assert!(
        !worker.grants.contains_key(&holder),
        "the holder is revoked"
    );
    assert_eq!(
        next_call_of(&worker, "gone-actor", "gone-late"),
        BindingStatus::Unavailable(BindingUnavailable::TakenOver(TakeoverReason::SessionEnded))
    );
}

/// A holder silent for 30 minutes with nothing in flight is taken over as inactive; the same
/// holder with a tool call still in flight is not.
#[tokio::test]
async fn a_long_inactive_writer_is_taken_over_only_when_idle() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let (holder, _) = production_start(&mut worker, "idle-actor", "idle-start").await;
    let silence = Duration::from_secs(31 * 60);
    worker
        .shared
        .bindings
        .lock()
        .unwrap()
        .backdate_seen(&holder, silence);
    // Its start call never settled: a tool call is in flight, so the writer is kept.
    let (outcome, detail) = writer_start(&mut worker, "eager-actor", "eager-start").await;
    assert_eq!(outcome.err(), Some(FailureCode::Conflict));
    assert!(
        detail
            .as_deref()
            .is_some_and(|detail| detail.starts_with("start:worktree_held_by_another_actor:")),
        "{detail:?}"
    );
    assert!(worker.grants.contains_key(&holder));
    settle_call(&worker, "idle-actor", "idle-start");
    worker
        .shared
        .bindings
        .lock()
        .unwrap()
        .backdate_seen(&holder, silence);
    let (outcome, detail) = writer_start(&mut worker, "eager-actor", "eager-again").await;
    let reply = outcome.unwrap_or_else(|code| panic!("{code:?}: {detail:?}"));
    let text = complete_text(&reply);
    assert!(
        text.contains("took over the writer from actor idle-actor")
            && text.contains("; inactive for 31 min); "),
        "{text}"
    );
    assert!(!worker.grants.contains_key(&holder));
}

/// A live holder, recently heard from and with an attached carrier, is never displaced: the start
/// is refused with the holder facts and the holder keeps its grant and its calls.
#[tokio::test]
async fn a_live_writer_is_never_taken_over() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let (holder, _) = production_start(&mut worker, "live-actor", "live-start").await;
    settle_call(&worker, "live-actor", "live-start");
    {
        let mut guard = worker.shared.bindings.lock().unwrap();
        guard.carrier_opened(4343);
        guard.note_carrier(&holder, 4343);
    }
    let (outcome, detail) = writer_start(&mut worker, "rival-actor", "rival-start").await;
    assert_eq!(outcome.err(), Some(FailureCode::Conflict));
    assert!(
        detail
            .as_deref()
            .is_some_and(|detail| detail.contains("actor live-actor")),
        "{detail:?}"
    );
    assert!(worker.grants.contains_key(&holder));
    assert!(matches!(
        next_call_of(&worker, "live-actor", "live-next"),
        BindingStatus::Validated(_)
    ));
}

/// A symbol or graph job of `actor` with a live waiter, as `submit` queues it: the job, the
/// waiter's receiver, and the job's retained `pending` result planted in the ledger.
fn waiting_job(
    worker: &Worker<'_>,
    actor: &str,
    call: &str,
    tool: AssistanceTool,
    parameters: Value,
) -> (Job, oneshot::Receiver<PeerReply>, watch::Sender<bool>) {
    let invocation = production_call(worker, actor, call);
    let reference = format!("waiting-{call}");
    worker.shared.ledger.lock().unwrap().details.insert(
        reference.clone(),
        Detail {
            binding: invocation.binding_ref().clone(),
            reply: PeerReply::Pending {
                detail_ref: reference.clone(),
            },
            selection: (tool, selection(&parameters)),
            authority: None,
            source: None,
            native_epoch: 0,
            line_movement: None,
            diff_page: None,
            diff_page_fresh: false,
            context_page: None,
            context_page_fresh: false,
            diff_provenance: None,
            extra_sources: Vec::new(),
        },
    );
    let (send, wait) = oneshot::channel();
    let (cancel_sender, cancel) = watch::channel(false);
    let job = Job {
        reference,
        invocation,
        tool,
        parameters,
        target: provider_target(&worker.runtime),
        deadline: tokio::time::Instant::now() + Duration::from_secs(5),
        cancel,
        stop_reply: Some(send),
        native_epoch: 0,
        failure_detail: None,
        format_note: None,
        check_scheduled: false,
        park_until: None,
        stage: None,
        session_binding: None,
    };
    (job, wait, cancel_sender)
}

/// The deferrals the test recorder holds for `reference`, as `(approximate_given, fence paths)`.
fn deferrals_of(reference: &str) -> Vec<(bool, Vec<String>)> {
    super::super::approximate::test_recorder()
        .calls()
        .into_iter()
        .filter_map(|call| match call {
            crate::assistance::outbox::OutboxCall::Defer {
                reference: deferred,
                spec,
                accepted: true,
            } if deferred == reference => Some((
                spec.approximate_given,
                spec.fence_paths
                    .into_iter()
                    .map(|fence| fence.path)
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// An epsilon worktree with `a` defined in `a.epsilon` and used in `b.epsilon`, and a worker
/// with the fixture provider; the returned reader binding's server is loading until a test arms
/// a scripted session for it.
async fn loading_fixture(actor: &str) -> (Fixture, Store) {
    let fixture = Fixture::new();
    epsilon_source(&fixture);
    std::fs::write(fixture.root.join("a.epsilon"), "sym a\nend\n").unwrap();
    std::fs::write(fixture.root.join("b.epsilon"), "sym user\na(1)\nend\n").unwrap();
    git_commit(&fixture.root, &format!("loading source for {actor}"));
    let store = fixture.store();
    (fixture, store)
}

/// While the server loads, `ide.symbol` answers its waiter at once with a labelled approximate
/// card (lexical usages marked `~`), registers the precise follow-up (`approximate_given`,
/// fenced by the definition file) and keeps running; once the server answers, the same job
/// settles with the precise card. No text tells the agent to poll, wait or repeat.
#[tokio::test]
async fn a_loading_symbol_answers_approximately_then_precisely() {
    use crate::lang::testing::fixture_serve_session;
    let (fixture, store) = loading_fixture("ap").await;
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let reader = provider_start(&mut worker, "ap-reader", "ap-start", true, None)
        .await
        .unwrap();
    let (mut job, mut wait, _cancel) = waiting_job(
        &worker,
        "ap-reader",
        "ap-1",
        AssistanceTool::Symbol,
        serde_json::json!({"symbol":"a.epsilon#a","callers":0}),
    );
    worker.perform(&mut job).await;
    let early = wait.try_recv().expect("the waiter is answered at once");
    let text = complete_text(&early);
    assert!(text.starts_with("approximate: "), "{text}");
    assert!(
        text.contains("the precise answer follows as a fact"),
        "{text}"
    );
    assert!(text.contains("b.epsilon:2  ~ a(1)"), "{text}");
    for banned in [
        "poll",
        "every few seconds",
        "in a few seconds",
        "until it completes",
        "again every",
        "repeat",
    ] {
        assert!(!text.contains(banned), "{banned}: {text}");
    }
    assert!(job.park_until.is_some(), "the job keeps running");
    assert!(
        job.deadline > tokio::time::Instant::now() + Duration::from_secs(9 * 60),
        "the precise budget is ten minutes"
    );
    assert_eq!(
        deferrals_of(&job.reference),
        vec![(true, vec!["a.epsilon".to_owned()])]
    );

    // A retry while still loading only keeps waiting.
    job.park_until = None;
    worker.perform(&mut job).await;
    assert!(job.park_until.is_some());

    // The server answers: the same job settles with the precise card.
    fixture_serve_session(&reader, &[]);
    job.park_until = None;
    worker.perform(&mut job).await;
    assert!(job.park_until.is_none(), "settled");
    let settled = worker
        .shared
        .ledger
        .lock()
        .unwrap()
        .details
        .get(&job.reference)
        .map(|detail| detail.reply.clone())
        .unwrap();
    let precise = complete_text(&settled);
    assert!(precise.starts_with("symbol: a"), "{precise}");
    assert!(!precise.contains("approximate"), "{precise}");
    assert_eq!(deferrals_of(&job.reference).len(), 1, "registered once");
}

/// While the server loads, `ide.graph` answers at once with an approximate depth-1 graph.
#[tokio::test]
async fn a_loading_graph_answers_approximately() {
    let (fixture, store) = loading_fixture("ag").await;
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    provider_start(&mut worker, "ag-reader", "ag-start", true, None)
        .await
        .unwrap();
    let (mut job, mut wait, _cancel) = waiting_job(
        &worker,
        "ag-reader",
        "ag-1",
        AssistanceTool::Graph,
        serde_json::json!({"symbol":"a.epsilon#a","direction":"both","depth":2}),
    );
    worker.perform(&mut job).await;
    let early = wait.try_recv().expect("the waiter is answered at once");
    let text = complete_text(&early);
    assert!(
        text.contains(
            "\ngraph: callers of a.epsilon#a (approximate, depth 1 of 2, 1 nodes, 0 edges)\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("callees: follows with the precise answer"),
        "{text}"
    );
    assert_eq!(deferrals_of(&job.reference).len(), 1);
}

/// A bare name the lexical scan cannot resolve to one definition answers `pending` at once and
/// registers a non-approximate deferral; once the session has four jobs waiting for their
/// precise answer, a fifth approximate answer says the precise one is not scheduled.
#[tokio::test]
async fn unresolvable_names_answer_pending_and_the_session_cap_holds() {
    let (fixture, store) = loading_fixture("cap").await;
    std::fs::write(fixture.root.join("c.epsilon"), "sym a\nend\n").unwrap();
    git_commit(&fixture.root, "a second a");
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    provider_start(&mut worker, "cap-reader", "cap-start", true, None)
        .await
        .unwrap();
    let (mut ambiguous, mut wait, _cancel) = waiting_job(
        &worker,
        "cap-reader",
        "cap-0",
        AssistanceTool::Symbol,
        serde_json::json!({"symbol":"a"}),
    );
    worker.perform(&mut ambiguous).await;
    assert!(
        matches!(wait.try_recv(), Ok(PeerReply::Pending { .. })),
        "an unresolvable bare name answers pending at once"
    );
    assert_eq!(
        deferrals_of(&ambiguous.reference),
        vec![(false, Vec::new())]
    );
    let mut keep = Vec::new();
    for index in 1..=3 {
        let (mut job, mut wait, cancel) = waiting_job(
            &worker,
            "cap-reader",
            &format!("cap-{index}"),
            AssistanceTool::Symbol,
            serde_json::json!({"symbol":"b.epsilon#user"}),
        );
        worker.perform(&mut job).await;
        assert!(matches!(wait.try_recv(), Ok(PeerReply::Complete { .. })));
        keep.push((job, cancel));
    }
    let (mut fifth, mut wait, _cancel) = waiting_job(
        &worker,
        "cap-reader",
        "cap-5",
        AssistanceTool::Symbol,
        serde_json::json!({"symbol":"b.epsilon#user"}),
    );
    worker.perform(&mut fifth).await;
    let early = wait.try_recv().expect("answered at once");
    assert!(
        complete_text(&early)
            .contains("precise answer not scheduled: 4 already pending for this session"),
        "{}",
        complete_text(&early)
    );
    assert!(deferrals_of(&fifth.reference).is_empty());
}
