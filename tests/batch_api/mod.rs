//! Product tests of the batch forms (batch-api-v1) against the real daemon and MCP front.
//!
//! Included from `product_mcp_contract.rs` so these tests share its fixture, actor and settle
//! helpers without a second copy; each test owns its own fixture worktree and daemon.

use super::*;

/// Reads the `source_ref` line of a batch read reply text and the files it names.
fn source_ref_line(text: &str) -> &str {
    text.lines()
        .find(|line| line.starts_with("source_ref: "))
        .unwrap_or_else(|| panic!("no source_ref line: {text}"))
}

/// `ide.read {items}` answers a symbol, a range and a whole file across three files in ONE
/// reply: the summary line first, blocks in request order, failed items in place without failing
/// the rest, a duplicate once, and one `source_ref` that a following multi-change `ide.edit` of a
/// delivered file accepts without a re-read. A second read whose middle item does not fit the
/// reply envelope delivers the prefix, names the rest with an exact `{"items":[…]}` call, and its
/// `source_ref` does not cover the cut file.
#[tokio::test]
async fn read_items_answer_files_symbols_and_ranges_in_one_reply() {
    let fixture = symbol_test_fixture();
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub struct Service;\npub fn value() -> i32 { 1 }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("src/util.rs"),
        "// fixture\npub fn utility() -> i32 {\n    2\n}\npub fn second() -> i32 {\n    3\n}\n",
    )
    .unwrap();
    std::fs::write(fixture.root.join("notes.md"), "# title\nbody\n").unwrap();
    let wide = "x".repeat(100);
    std::fs::write(
        fixture.root.join("big.md"),
        (1..=700)
            .map(|line| format!("{line} {wide}\n"))
            .collect::<String>(),
    )
    .unwrap();
    std::fs::write(fixture.root.join("tail.md"), "tail\n").unwrap();
    fixture.git(&[
        "add",
        "--",
        "src/lib.rs",
        "src/util.rs",
        "notes.md",
        "big.md",
        "tail.md",
    ]);
    fixture.git(&["commit", "--quiet", "-m", "read items fixture"]);

    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "read-items").await;
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"read-items"}))
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");

    let (read, text) = settled_read(
        &mut actor,
        &fixture,
        json!({"items":[
            {"symbol":"src/lib.rs#value"},
            {"path":"src/util.rs","lines":"5-7"},
            {"path":"notes.md"},
            {"symbol":"src/lib.rs#missing"},
            {"path":"nope.rs"},
            {"path":"notes.md"}
        ]}),
    )
    .await;
    assert_eq!(read["kind"], "read", "{read} {text}");
    assert!(
        text.starts_with(
            "read: 5 items in 4 files — 3 delivered, 0 unchanged, 0 not included, 2 failed\n"
        ),
        "{text}"
    );
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing {needle:?}: {text}"))
    };
    assert!(
        at("pub fn value") < at("5\tpub fn second")
            && at("5\tpub fn second") < at("1\t# title")
            && at("1\t# title") < at("no such symbol: src/lib.rs#missing")
            && at("no such symbol: src/lib.rs#missing") < at("no such file: nope.rs"),
        "blocks and failures stay in request order: {text}"
    );
    assert!(text.contains("duplicates shown once: notes.md"), "{text}");
    assert!(
        !text.contains("not included (over the reply budget)"),
        "{text}"
    );
    let line = source_ref_line(&text);
    assert!(
        line.contains("src/lib.rs") && line.contains("src/util.rs") && line.contains("notes.md"),
        "{line}"
    );
    let source_ref = read["detail_ref"].as_str().unwrap().to_owned();
    let edit = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"read-items-edit",
                "path":"src/util.rs",
                "source_ref":source_ref,
                "changes":[{"lines":"6-6","content":"    42"}]
            }),
        )
        .await;
    let edit = actor.settle(&fixture, edit).await;
    assert_eq!(edit["result"]["outcome"], "replaced", "{edit}");

    let (cut, text) = settled_read(
        &mut actor,
        &fixture,
        json!({"items":[{"path":"tail.md"},{"path":"big.md"},{"path":"notes.md"}]}),
    )
    .await;
    assert_eq!(cut["kind"], "read", "{cut} {text}");
    assert!(
        text.starts_with(
            "read: 3 items in 3 files — 1 delivered, 0 unchanged, 2 not included, 0 failed\n"
        ),
        "{text}"
    );
    let footer = text
        .lines()
        .find(|line| line.starts_with("not included (over the reply budget): "))
        .unwrap_or_else(|| panic!("no footer: {text}"));
    let call: Value = serde_json::from_str(footer.split_once("call ide.read ").unwrap().1).unwrap();
    assert_eq!(
        call,
        json!({"items":[{"path":"big.md"},{"path":"notes.md"}]}),
        "{footer}"
    );
    assert!(!source_ref_line(&text).contains("notes.md"), "{text}");
    let cut_ref = cut["detail_ref"].as_str().unwrap().to_owned();
    let refused = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"read-items-cut-edit",
                "path":"notes.md",
                "source_ref":cut_ref,
                "changes":[{"lines":"2-2","content":"changed"}]
            }),
        )
        .await;
    let refused = actor.settle(&fixture, refused).await;
    assert_eq!(
        refused["result"]["outcome"], "stale_source",
        "a cut file is not an edit base: {refused}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("notes.md")).unwrap(),
        "# title\nbody\n"
    );

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// The legacy batch forms keep answering as before and only gain the summary line.
#[tokio::test]
async fn legacy_batch_reads_gain_only_the_summary_line() {
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(fixture.root.join("notes.md"), "# title\nbody\n").unwrap();
    fixture.git(&["add", "--", "notes.md"]);
    fixture.git(&["commit", "--quiet", "-m", "legacy read fixture"]);
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "legacy-reads").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"legacy-reads"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let (_, ranges) = settled_read(
        &mut actor,
        &fixture,
        json!({"path":"notes.md","ranges":["1-1","2-2"]}),
    )
    .await;
    assert!(
        ranges.starts_with(
            "read: 2 items in 1 files — 2 delivered, 0 unchanged, 0 not included, 0 failed\n"
        ) && ranges.contains("1\t# title")
            && ranges.contains("2\tbody"),
        "{ranges}"
    );
    let (_, single) = settled_read(&mut actor, &fixture, json!({"path":"notes.md"})).await;
    assert!(
        !single.starts_with("read:") && single.contains("1\t# title"),
        "single forms are unchanged: {single}"
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
