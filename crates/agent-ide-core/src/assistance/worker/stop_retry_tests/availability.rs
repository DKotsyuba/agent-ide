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
