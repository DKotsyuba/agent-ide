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
