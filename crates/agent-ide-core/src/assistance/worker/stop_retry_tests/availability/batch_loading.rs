//! Several-symbol calls while a language server loads (batch-api-v1 §4, symbol-graph-item r2 §1,
//! session-outbox-v1 §13.3), on the availability fixtures: the scripted epsilon server stays
//! loading until a test arms a session for it.

use super::*;
use crate::assistance::outbox::{Carrier, HandoffRequest};

/// While the server loads, `ide.symbol {symbols}` answers its waiter at once: the entry whose
/// definition the source outline finds is approximate, the bare name the lexical scan cannot
/// resolve reads `pending: <entry> - will be delivered`. The job re-runs only those entries; once
/// the server answers it settles with the precise blocks, and its final reply becomes ONE
/// `job-ready` fact naming the batch `detail_ref` and both indexes, holding the turn because an
/// entry got no answer early.
#[tokio::test]
async fn a_loading_batch_answers_per_entry_at_once_then_one_fact() {
    use crate::lang::testing::fixture_serve_session;
    let (fixture, store) = loading_fixture("bl").await;
    std::fs::write(fixture.root.join("c.epsilon"), "sym a\nend\n").unwrap();
    git_commit(&fixture.root, "a second a");
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    let reader = provider_start(&mut worker, "bl-reader", "bl-start", true, None)
        .await
        .unwrap();
    let (mut job, mut wait, _cancel) = waiting_job(
        &worker,
        "bl-reader",
        "bl-1",
        AssistanceTool::Symbol,
        serde_json::json!({"symbols":["a.epsilon#a","a"],"callers":0}),
    );
    worker.perform(&mut job).await;
    let early = wait.try_recv().expect("the waiter is answered at once");
    let text = complete_text(&early);
    assert!(
        text.starts_with("symbols: 2 — 0 cards, 1 approximate, 1 pending, 0 failed\napproximate: "),
        "{text}"
    );
    assert!(text.contains("b.epsilon:2  ~ a(1)"), "{text}");
    assert!(text.ends_with("pending: a - will be delivered\n"), "{text}");
    assert!(job.park_until.is_some(), "the job keeps running");
    assert!(
        job.deadline > tokio::time::Instant::now() + Duration::from_secs(9 * 60),
        "the precise budget is ten minutes"
    );
    assert_eq!(
        deferrals_of(&worker, &job.reference),
        vec![(false, vec!["a.epsilon".to_owned()])],
        "one deferral, fenced by the approximate entry's definition, holding the turn"
    );

    // A retry while still loading keeps the early blocks and waits.
    job.park_until = None;
    worker.perform(&mut job).await;
    assert!(job.park_until.is_some());
    assert!(worker.symbol_batches.contains_key(&job.reference));

    // The server answers: the same job settles with the precise blocks.
    fixture_serve_session(&reader, &[]);
    job.park_until = None;
    worker.perform(&mut job).await;
    assert!(job.park_until.is_none(), "settled");
    assert!(!worker.symbol_batches.contains_key(&job.reference));
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
    assert!(
        precise.starts_with("symbols: 2 — ") && precise.contains(" 0 approximate, 0 pending,"),
        "{precise}"
    );
    assert!(precise.contains("\nsymbol: a"), "{precise}");
    assert!(!precise.contains("approximate:"), "{precise}");
    assert!(deferrals_of(&worker, &job.reference).is_empty(), "consumed");
    let facts = worker
        .shared
        .outbox
        .reserve_facts(
            &reader,
            HandoffRequest {
                carrier: Carrier::Inspect,
                budget: 8_000,
                holding_only: false,
            },
        )
        .expect("one fact")
        .facts;
    assert_eq!(facts.len(), 1, "{facts:?}");
    assert_eq!(
        facts[0].subject,
        format!("ide.symbol batch {}: items 1,2", job.reference)
    );
    assert!(facts[0].holds_turn);
    assert_eq!(facts[0].detail_ref.as_deref(), Some(job.reference.as_str()));
}

/// A several-symbol call whose caller already left (the inline wait answered it `pending`) is
/// not answered early: the whole batch waits as before and keeps no per-entry state.
#[tokio::test]
async fn a_loading_batch_without_a_waiter_waits_whole() {
    let (fixture, store) = loading_fixture("bw").await;
    let workspace = DurableWorkspace::open(&store).await.unwrap();
    let mut worker = worker(&store, workspace, fixture.root.clone());
    worker.observations.install_schema().await.unwrap();
    provider_start(&mut worker, "bw-reader", "bw-start", true, None)
        .await
        .unwrap();
    let (mut job, wait, _cancel) = waiting_job(
        &worker,
        "bw-reader",
        "bw-1",
        AssistanceTool::Graph,
        serde_json::json!({"symbols":["a.epsilon#a"],"direction":"callers","depth":1}),
    );
    drop(wait);
    job.stop_reply = None;
    worker.perform(&mut job).await;
    assert!(job.park_until.is_some(), "parked");
    assert!(job.deadline < tokio::time::Instant::now() + Duration::from_secs(60));
    assert!(deferrals_of(&worker, &job.reference).is_empty());
    assert!(!worker.symbol_batches.contains_key(&job.reference));
}
