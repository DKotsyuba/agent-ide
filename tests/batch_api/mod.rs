//! Product tests of the batch forms (batch-api-v1) against the real daemon and MCP front.
//!
//! Included from `product_mcp_contract.rs` so these tests share its fixture, actor and settle
//! helpers without a second copy; each test owns its own fixture worktree and daemon.

use super::*;

/// Asserts a read reply text names no file-version reference: the current text is the base of
/// every edit (symbol-read-edit-v1), and the structured `detail_ref` stays beside the text.
fn assert_no_source_ref(text: &str) {
    assert!(!text.contains("source_ref"), "{text}");
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

    let (read, text) = read_text(
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
    assert_no_source_ref(&text);
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

    let (cut, text) = read_text(
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
    assert_no_source_ref(&text);
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
    let (_, ranges) = read_text(
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
    let (_, single) = read_text(&mut actor, &fixture, json!({"path":"notes.md"})).await;
    assert!(
        !single.starts_with("read:") && single.contains("1\t# title"),
        "single forms are unchanged: {single}"
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// The summary counts of a batch read reply text, as `(delivered, unchanged)`; only the seam
/// tests read them.
#[cfg(feature = "test-seams")]
fn read_counts(text: &str) -> (usize, usize) {
    let summary = text.lines().next().unwrap_or_default();
    let count = |word: &str| -> usize {
        summary
            .split(", ")
            .chain(summary.split(" — "))
            .find_map(|part| part.trim().strip_suffix(word))
            .and_then(|number| number.trim().rsplit(' ').next())
            .and_then(|number| number.parse().ok())
            .unwrap_or_else(|| panic!("no {word} count: {summary}"))
    };
    (count(" delivered"), count(" unchanged"))
}

/// With a known context epoch (test seam standing in for the session outbox's compaction
/// signal), a repeated items read of identical bytes answers `unchanged since your read <ref>`
/// with a `source_ref` that still admits an edit; changed bytes, `force:true`, a new epoch and
/// `ide.stop` each bring the text back.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn read_items_answer_unchanged_for_identical_bytes_until_change_force_compaction_or_stop() {
    let fixture = edit_fixture();
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let mut daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = ProductActor::new(&fixture, "read-unchanged").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"read-unchanged"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let items = json!({"items":[{"path":"a.md"},{"path":"b.md"}]});

    let (first, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(read_counts(&text), (2, 0), "{text}");
    let first_ref = first["detail_ref"].as_str().unwrap().to_owned();
    let (second, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(read_counts(&text), (0, 2), "{text}");
    assert!(
        text.contains(&format!(
            "unchanged since your read {first_ref}: a.md (2 lines); \"force\":true returns the text"
        )),
        "{text}"
    );
    assert!(!text.contains("a.md line1"), "{text}");
    assert_no_source_ref(&text);
    let second_ref = second["detail_ref"].as_str().unwrap().to_owned();
    let edit = actor
        .call(
            &fixture,
            "ide.edit",
            json!({"operation_id":"after-unchanged","path":"a.md","source_ref":second_ref,
                   "changes":[{"lines":"2-2","content":"a.md changed"}]}),
        )
        .await;
    let edit = actor.settle(&fixture, edit).await;
    assert_eq!(edit["result"]["outcome"], "replaced", "{edit}");

    let (_, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(
        read_counts(&text),
        (1, 1),
        "changed bytes come back: {text}"
    );
    assert!(text.contains("a.md changed"), "{text}");
    let mut forced = items.clone();
    forced["force"] = json!(true);
    let (_, text) = read_text(&mut actor, &fixture, forced).await;
    assert_eq!(read_counts(&text), (2, 0), "force returns the text: {text}");
    std::fs::write(&epoch, "2").unwrap();
    let (_, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(read_counts(&text), (2, 0), "a compaction resets: {text}");
    let (_, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(read_counts(&text), (0, 2), "{text}");
    actor.call(&fixture, "ide.stop", json!({})).await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"read-unchanged-2"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let (_, text) = read_text(&mut actor, &fixture, items.clone()).await;
    assert_eq!(read_counts(&text), (2, 0), "ide.stop resets: {text}");

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Without a compaction signal (an unknown context epoch, as on a channel that never delivered a
/// native hook; the test seam stands in because every product actor binds through hooks) the
/// unchanged answer is never given: a repeated items read returns the text.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn read_items_never_answer_unchanged_without_a_compaction_signal() {
    let fixture = edit_fixture();
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "unknown").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let mut daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = ProductActor::new(&fixture, "read-no-signal").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"read-no-signal"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let items = json!({"items":[{"path":"a.md"},{"path":"b.md"}]});
    for _ in 0..2 {
        let (_, text) = read_text(&mut actor, &fixture, items.clone()).await;
        assert_eq!(read_counts(&text), (2, 0), "{text}");
    }
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// [`batch_call_with_text`] with a due status plate stripped from the text (T28B): the plate
/// rides the structured `status` field, never the reply text under test.
async fn call_text(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    name: &str,
    arguments: Value,
) -> (Value, String) {
    let (reply, text) = batch_call_with_text(actor, fixture, name, arguments).await;
    (reply, batch_without_status_plate(&text).to_owned())
}

/// [`batch_settle_with_text`] with a due status plate stripped from the settled text.
async fn settle_text(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    reply: (Value, String),
) -> (Value, String) {
    let (reply, text) = batch_settle_with_text(actor, fixture, reply).await;
    (reply, batch_without_status_plate(&text).to_owned())
}

/// One settled `ide.read` with its compact text, any due status plate stripped.
async fn read_text(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    arguments: Value,
) -> (Value, String) {
    let reply = call_text(actor, fixture, "ide.read", arguments).await;
    settle_text(actor, fixture, reply).await
}

/// Settles one call and returns its structured reply and compact text; an answer given while a
/// server loads (a single form's `approximate:` text, or a several-symbol summary counting
/// approximate or pending entries) is followed through its `detail_ref` to the precise one.
async fn settled_call(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    tool: &str,
    arguments: Value,
) -> (Value, String) {
    let reply = call_text(actor, fixture, tool, arguments).await;
    settle_early_answer(actor, fixture, reply).await
}

/// Whether `text` is an answer given while a server loads: a single form's `approximate:` text,
/// or a several-symbol summary line counting approximate or pending entries.
fn early_answer(text: &str) -> bool {
    let summary = text.lines().next().unwrap_or_default();
    text.starts_with("approximate: ")
        || ((summary.starts_with("symbols: ") || summary.starts_with("graph: "))
            && !(summary.contains(" 0 approximate, 0 pending,")))
}

/// Follows `reply` through `ide.inspect` of its `detail_ref` while it is `pending` or an
/// [`early_answer`], bounded like every product settle.
async fn settle_early_answer(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    reply: (Value, String),
) -> (Value, String) {
    let mut reply = settle_text(actor, fixture, reply).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    while early_answer(&reply.1) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no precise answer: {}",
            reply.1
        );
        let reference = reply.0["detail_ref"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let next = call_text(
            actor,
            fixture,
            "ide.inspect",
            json!({"detail_ref":reference}),
        )
        .await;
        reply = settle_text(actor, fixture, next).await;
    }
    reply
}

/// `ide.symbol {symbols}` and `ide.graph {symbols}` answer every entry in ONE reply, in request
/// order behind one summary line; a failed entry fails only itself, and each answered block is
/// exactly the single form's reply text. On the cold worktree the first several-symbol call
/// answers at once, never `pending`, with each loading entry approximate or pending, and its
/// `detail_ref` settles to the precise entries (batch-api-v1 §4).
#[tokio::test]
async fn symbol_and_graph_answer_several_symbols_in_one_reply() {
    let fixture = symbol_test_fixture();
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> i32 {\n    7\n}\npub fn caller() -> i32 {\n    value()\n}\npub fn top() -> i32 {\n    caller()\n}\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "src/lib.rs"]);
    fixture.git(&["commit", "--quiet", "-m", "symbols fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "batch-symbols").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"batch-symbols"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");

    let symbols = json!({"symbols":["src/lib.rs#value","src/lib.rs#missing","src/lib.rs#caller"],"callers":1});
    let asked = std::time::Instant::now();
    let cold = call_text(&mut actor, &fixture, "ide.symbol", symbols.clone()).await;
    assert_ne!(cold.0["state"], "pending", "{}", cold.1);
    assert!(
        asked.elapsed() < Duration::from_secs(8),
        "{:?}",
        asked.elapsed()
    );
    assert!(cold.1.starts_with("symbols: 3 — "), "{}", cold.1);
    let (_, settled) = settle_early_answer(&mut actor, &fixture, cold).await;
    assert!(
        settled.starts_with("symbols: 3 — 2 cards, 0 approximate, 0 pending, 1 failed\n"),
        "{settled}"
    );

    let (single, single_text) = settled_call(
        &mut actor,
        &fixture,
        "ide.symbol",
        json!({"symbol":"src/lib.rs#caller","callers":1}),
    )
    .await;
    assert_eq!(single["kind"], "symbol", "{single}");
    let (batch, text) = settled_call(&mut actor, &fixture, "ide.symbol", symbols).await;
    assert_eq!(batch["kind"], "symbol", "{batch} {text}");
    assert!(
        text.starts_with("symbols: 3 — 2 cards, 0 approximate, 0 pending, 1 failed\n"),
        "{text}"
    );
    let missing = text
        .find("No symbol matches the requested address.")
        .expect("failed entry in place");
    let caller = text.find(single_text.trim_end()).unwrap_or_else(|| {
        panic!("the caller block equals the single form's text:\n{single_text}\n---\n{text}")
    });
    assert!(
        text.find("value").unwrap() < missing && missing < caller,
        "{text}"
    );

    let (graph, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.graph",
        json!({"symbols":["src/lib.rs#value","src/lib.rs#caller"],"direction":"callers","depth":1}),
    )
    .await;
    assert_eq!(graph["kind"], "graph", "{graph} {text}");
    assert!(
        text.starts_with("graph: 2 roots (callers, depth 1) — "),
        "{text}"
    );
    assert!(text.contains("caller") && text.contains("top"), "{text}");

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// `ide.outline {paths}` outlines several files in request order with a failed file in place;
/// `ide.outline {glob}` outlines the matching regular files in path order, never leaving the
/// worktree, never through a symbolic link, skipping hidden and build directories, at most 24
/// with the rest counted; files past the reply budget are one line each.
#[tokio::test]
async fn outline_answers_several_paths_or_a_bounded_glob() {
    use std::os::unix::fs::symlink;
    let fixture = symbol_test_fixture();
    let function = |name: &str| format!("pub fn {name}() -> i32 {{\n    1\n}}\n");
    let files = [
        ("src/a.rs", function("alpha")),
        ("src/b.rs", function("beta")),
        ("src/nested/c.rs", function("gamma")),
        ("src/nested/d.txt", "text\n".to_owned()),
        ("src/target/skipped.rs", function("skipped")),
        ("src/.hidden/skipped.rs", function("hidden")),
        (
            "big/huge.rs",
            (0..2500)
                .map(|index| function(&format!("f{index}")))
                .collect(),
        ),
    ];
    for (path, content) in &files {
        let path = fixture.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    for index in 0..26 {
        let path = fixture.root.join(format!("many/m{index:02}.rs"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, function(&format!("m{index}"))).unwrap();
    }
    let outside = fixture.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("escape.rs"), function("escape")).unwrap();
    symlink(&outside, fixture.root.join("src/link")).unwrap();
    symlink(&outside, fixture.root.join("linked")).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "batch-outline").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"batch-outline"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");

    let (paths, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.outline",
        json!({"paths":["src/b.rs","src/missing.rs","src/a.rs"]}),
    )
    .await;
    assert_eq!(paths["kind"], "outline", "{paths} {text}");
    assert!(
        text.starts_with("outline: 3 files — 2 outlined, 0 listed only, 1 failed\n"),
        "{text}"
    );
    let at = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("{needle}: {text}"))
    };
    assert!(
        at("beta") < at("This path does not exist in the worktree.")
            && at("This path does not exist in the worktree.") < at("alpha"),
        "{text}"
    );

    let (_, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.outline",
        json!({"glob":"src/**/*.rs","kinds":"fn"}),
    )
    .await;
    assert!(
        text.starts_with("outline: 4 files — 4 outlined, 0 listed only, 0 failed\n"),
        "{text}"
    );
    assert!(
        at_order(
            &text,
            &["src/a.rs", "src/b.rs", "src/lib.rs", "src/nested/c.rs"]
        ),
        "{text}"
    );
    for absent in ["skipped", "hidden", "escape"] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }
    let (_, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.outline",
        json!({"glob":"linked/*.rs"}),
    )
    .await;
    assert!(text.starts_with("outline: 0 files — "), "{text}");
    // A glob with `..` or an absolute start is refused by the facade (unit-tested there).

    let (_, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.outline",
        json!({"glob":"many/*.rs"}),
    )
    .await;
    assert!(
        text.starts_with(
            "outline: 24 files — 24 outlined, 0 listed only, 0 failed; +2 more files matched\n"
        ),
        "{text}"
    );
    assert!(text.contains("m23") && !text.contains("m24"), "{text}");

    let (_, text) = settled_call(
        &mut actor,
        &fixture,
        "ide.outline",
        json!({"paths":["src/a.rs","big/huge.rs","src/b.rs"]}),
    )
    .await;
    assert!(
        text.starts_with("outline: 3 files — 2 outlined, 1 listed only, 0 failed\n"),
        "{text}"
    );
    assert!(
        text.contains("big/huge.rs (7500 lines) → ide.outline {\"path\":\"big/huge.rs\"}"),
        "{text}"
    );
    assert!(at_order(&text, &["alpha", "big/huge.rs", "beta"]), "{text}");

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Whether `needles` occur in `text` in this order.
fn at_order(text: &str, needles: &[&str]) -> bool {
    let positions: Vec<Option<usize>> = needles.iter().map(|needle| text.find(needle)).collect();
    positions.iter().all(Option::is_some) && positions.windows(2).all(|pair| pair[0] < pair[1])
}

/// The three text files every multi-file edit test edits, with their committed content.
const EDIT_FILES: [&str; 3] = ["a.md", "b.md", "c.md"];

/// The committed content of one [`EDIT_FILES`] file.
fn original(name: &str) -> String {
    format!("{name} line1\n{name} line2\n")
}

/// The content one [`edit_arguments`] change produces; applying it twice would add a second
/// `added` line, so equality proves single application.
fn edited(name: &str) -> String {
    format!("{name} line1\nadded\n{name} line2\n")
}

/// A fixture holding [`EDIT_FILES`] committed with their [`original`] content.
fn edit_fixture() -> ProductFixture {
    let fixture = ProductFixture::new(json!([]));
    for name in EDIT_FILES {
        std::fs::write(fixture.root.join(name), original(name)).unwrap();
    }
    fixture.git(&["add", "--", "a.md", "b.md", "c.md"]);
    fixture.git(&["commit", "--quiet", "-m", "multi-file edit fixture"]);
    fixture
}

/// One multi-file edit request over [`EDIT_FILES`], each replacing line 1 with two lines.
fn edit_arguments(operation_id: &str, source_ref: &str) -> Value {
    json!({
        "operation_id": operation_id,
        "source_ref": source_ref,
        "files": EDIT_FILES
            .iter()
            .map(|name| json!({"path": name, "changes": [{"lines": "1-1", "content": format!("{name} line1\nadded")}]}))
            .collect::<Vec<_>>()
    })
}

/// Reads [`EDIT_FILES`] in one items call and returns the reply's `source_ref`.
async fn read_edit_files(actor: &mut ProductActor, fixture: &ProductFixture) -> String {
    let (read, text) = read_text(
        actor,
        fixture,
        json!({"items": EDIT_FILES.iter().map(|name| json!({"path": name})).collect::<Vec<_>>()}),
    )
    .await;
    assert_no_source_ref(&text);
    read["detail_ref"].as_str().unwrap().to_owned()
}

/// Calls `ide.edit` and returns the settled structured reply and its compact text.
async fn settled_edit(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    arguments: Value,
) -> (Value, String) {
    let reply = call_text(actor, fixture, "ide.edit", arguments).await;
    settle_text(actor, fixture, reply).await
}

/// Asserts every [`EDIT_FILES`] file holds exactly `expected(name)`.
fn assert_files(fixture: &ProductFixture, expected: impl Fn(&str) -> String, context: &str) {
    for name in EDIT_FILES {
        assert_eq!(
            std::fs::read_to_string(fixture.root.join(name)).unwrap(),
            expected(name),
            "{context}: {name}"
        );
    }
}

/// `ide.edit {files}` validates every file before the first write: a file made stale at any
/// position, a change that cannot apply at any position, and a create of an existing path each
/// refuse the whole call as `edit_refused` naming that file, and no file is written.
#[tokio::test]
async fn multi_file_edit_writes_nothing_when_any_file_is_stale_or_any_change_fails() {
    let fixture = edit_fixture();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "edit-files-refusals").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"edit-files-refusals"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");

    for (position, stale) in EDIT_FILES.iter().enumerate() {
        let source_ref = read_edit_files(&mut actor, &fixture).await;
        let native = format!("{}native\n", original(stale));
        std::fs::write(fixture.root.join(stale), &native).unwrap();
        let (reply, text) = settled_edit(
            &mut actor,
            &fixture,
            edit_arguments(&format!("stale-{position}"), &source_ref),
        )
        .await;
        assert_eq!(reply["code"], "edit_refused", "{reply} {text}");
        assert!(
            text.contains(&format!("file {} {stale}: stale_source", position + 1)),
            "{text}"
        );
        assert_files(
            &fixture,
            |name| {
                if name == *stale {
                    native.clone()
                } else {
                    original(name)
                }
            },
            "a stale file writes nothing",
        );
        std::fs::write(fixture.root.join(stale), original(stale)).unwrap();
    }

    for (position, failing) in EDIT_FILES.iter().enumerate() {
        let source_ref = read_edit_files(&mut actor, &fixture).await;
        let mut arguments = edit_arguments(&format!("failing-{position}"), &source_ref);
        arguments["files"][position]["changes"] = json!([{"old": "absent text", "new": "x"}]);
        let (reply, text) = settled_edit(&mut actor, &fixture, arguments).await;
        assert_eq!(reply["code"], "edit_refused", "{reply} {text}");
        assert!(
            text.contains(&format!(
                "file {} {failing}: 1 of 1 changes refused",
                position + 1
            )),
            "{text}"
        );
        assert_files(&fixture, original, "a failing change writes nothing");
    }

    let source_ref = read_edit_files(&mut actor, &fixture).await;
    let mut arguments = edit_arguments("create-existing", &source_ref);
    arguments["files"][2] = json!({"path": "c.md", "create": true, "content": "new\n"});
    let (reply, text) = settled_edit(&mut actor, &fixture, arguments).await;
    assert_eq!(reply["code"], "edit_refused", "{reply} {text}");
    assert!(text.contains("file 3 c.md: already exists"), "{text}");
    assert_files(
        &fixture,
        original,
        "a create of an existing path writes nothing",
    );

    // A refused operation id stays unused: the same id now applies.
    let (reply, text) = settled_edit(
        &mut actor,
        &fixture,
        edit_arguments("create-existing", &source_ref),
    )
    .await;
    assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
    assert!(
        text.starts_with("edit: applied 3 of 3 files; diagnostics: "),
        "{text}"
    );
    assert_files(&fixture, edited, "the unused id applies");

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A daemon crash right after the k-th write (test seam) loses the reply, which reports the
/// unknown outcome; a replacement daemon answers the same request from the durable receipt:
/// files 1..k are written earlier, the rest are written now, and no change is applied twice.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn multi_file_edit_resumes_after_a_crash_after_each_write_without_double_application() {
    for written in 1..=EDIT_FILES.len() {
        let fixture = edit_fixture();
        let home = enable_fake_rust_checks(&fixture, &fixture.base);
        let (fault, _) = fault_flag(&fixture, &format!("batch_edit_after_write_{written}"), true);
        let env = [("AGENT_IDE_TEST_FAULT", fault.as_str())];
        let mut daemon = fixture
            .spawn_configured_daemon_with_env(
                Some(&home),
                false,
                Duration::from_secs(30),
                None,
                &env,
            )
            .await;
        let mut actor = ProductActor::new(&fixture, "edit-files-crash").await;
        let started = actor
            .call(
                &fixture,
                "ide.start",
                json!({"activation_id":"edit-files-crash"}),
            )
            .await;
        assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
        let source_ref = read_edit_files(&mut actor, &fixture).await;
        let arguments = edit_arguments("crash-op", &source_ref);
        let lost = actor.call(&fixture, "ide.edit", arguments.clone()).await;
        let lost = actor.settle(&fixture, lost).await;
        assert_eq!(lost["state"], "edit_batch", "{lost}");
        assert_eq!(
            lost["outcome"], "outcome_unknown",
            "a crash after a write never answers internal: {lost}"
        );
        assert_eq!(lost["operation_id"], "crash-op", "{lost}");
        assert_eq!(
            lost["files"],
            json!(
                EDIT_FILES
                    .iter()
                    .map(|path| json!({"path": path, "state": "unknown"}))
                    .collect::<Vec<_>>()
            ),
            "{lost}"
        );
        assert_crash_only_exit(&mut daemon, &fixture).await;
        actor.mcp.close().await;
        assert_files(
            &fixture,
            |name| {
                let index = EDIT_FILES.iter().position(|file| *file == name).unwrap();
                if index < written {
                    edited(name)
                } else {
                    original(name)
                }
            },
            "the crash stopped the commit after the seam's write",
        );

        let mut replacement = fixture
            .spawn_configured_daemon_with_env(
                Some(&home),
                false,
                Duration::from_secs(30),
                None,
                &env,
            )
            .await;
        let mut again = ProductActor::new(&fixture, "edit-files-crash-again").await;
        let started = again
            .call(
                &fixture,
                "ide.start",
                json!({"activation_id":"edit-files-crash-2"}),
            )
            .await;
        assert_eq!(again.settle(&fixture, started).await["kind"], "activation");
        let (reply, text) = settled_edit(&mut again, &fixture, arguments).await;
        assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
        assert!(text.starts_with("edit: applied 3 of 3 files"), "{text}");
        for (index, name) in EDIT_FILES.iter().enumerate() {
            let earlier = format!(
                "  {} {name}: written earlier by this operation_id",
                index + 1
            );
            assert_eq!(text.contains(&earlier), index < written, "{name}: {text}");
        }
        assert_files(
            &fixture,
            edited,
            "the retry finished without double application",
        );
        again.call(&fixture, "ide.stop", json!({})).await;
        again.mcp.close().await;
        replacement.kill().await.unwrap();
        replacement.wait().await.unwrap();
    }
}

/// A native write that lands between validation and the k-th write (test seam) stops the commit
/// there: files before k are written, file k keeps the native bytes and later files are not
/// reached. After file k is restored, the same request resumes and finishes; a different request
/// with the same operation id conflicts and writes nothing.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn multi_file_edit_stops_at_a_native_write_and_resumes_only_unchanged_bases() {
    for position in 1..=EDIT_FILES.len() {
        let fixture = edit_fixture();
        let home = enable_fake_rust_checks(&fixture, &fixture.base);
        let (fault, _) = fault_flag(
            &fixture,
            &format!("batch_edit_before_write_{position}"),
            true,
        );
        let env = [("AGENT_IDE_TEST_FAULT", fault.as_str())];
        let mut daemon = fixture
            .spawn_configured_daemon_with_env(
                Some(&home),
                false,
                Duration::from_secs(30),
                None,
                &env,
            )
            .await;
        let mut actor = ProductActor::new(&fixture, "edit-files-native").await;
        let started = actor
            .call(
                &fixture,
                "ide.start",
                json!({"activation_id":"edit-files-native"}),
            )
            .await;
        assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
        let source_ref = read_edit_files(&mut actor, &fixture).await;
        let arguments = edit_arguments("native-op", &source_ref);
        let (reply, text) = settled_edit(&mut actor, &fixture, arguments.clone()).await;
        assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
        let stale = EDIT_FILES[position - 1];
        assert_eq!(reply["outcome"], "partial", "{reply}");
        assert_eq!(
            reply["files"][position - 1],
            json!({"path": stale, "state": "not_written", "reason": "changed_after_validation"}),
            "{reply}"
        );
        if position < EDIT_FILES.len() {
            assert_eq!(reply["files"][position]["reason"], "not_reached", "{reply}");
        }
        assert_eq!(
            reply["source_ref"].is_string(),
            position > 1,
            "a source_ref only when a file is written: {reply}"
        );
        assert!(
            text.starts_with(&format!(
                "edit: partial — written {} of 3; not written: {position} {stale} (changed after validation)",
                position - 1
            )),
            "{text}"
        );
        let native = format!("{}native\n", original(stale));
        assert_files(
            &fixture,
            |name| {
                let index = EDIT_FILES.iter().position(|file| *file == name).unwrap() + 1;
                match index.cmp(&position) {
                    std::cmp::Ordering::Less => edited(name),
                    std::cmp::Ordering::Equal => native.clone(),
                    std::cmp::Ordering::Greater => original(name),
                }
            },
            "the native bytes are never overwritten",
        );

        // Still changed: the retry stops at the same file again.
        let (_, text) = settled_edit(&mut actor, &fixture, arguments.clone()).await;
        assert!(text.starts_with("edit: partial"), "{text}");
        std::fs::write(fixture.root.join(stale), original(stale)).unwrap();
        let (reply, text) = settled_edit(&mut actor, &fixture, arguments.clone()).await;
        assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
        assert!(text.starts_with("edit: applied 3 of 3 files"), "{text}");
        assert_files(&fixture, edited, "the resume finished the rest once");

        assert_eq!(reply["outcome"], "applied", "{reply}");
        let mut other = arguments.clone();
        other["files"][0]["changes"][0]["content"] = json!("different");
        let (duplicate, text) = settled_edit(&mut actor, &fixture, other).await;
        assert!(text.starts_with("edit: conflicting_duplicate"), "{text}");
        assert_eq!(duplicate["outcome"], "conflicting_duplicate", "{duplicate}");
        assert_eq!(duplicate["files"], json!([]), "{duplicate}");
        assert_files(&fixture, edited, "a conflicting duplicate writes nothing");

        actor.call(&fixture, "ide.stop", json!({})).await;
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }
}

/// One multi-file edit of three Rust files runs exactly one project check and reports each
/// file's diagnostics; the reply's `source_ref` is an edit base for every written file.
#[tokio::test]
async fn multi_file_edit_runs_one_project_check_and_reports_each_file() {
    let fixture = ProductFixture::new(json!([]));
    for name in ["src/a.rs", "src/b.rs"] {
        std::fs::write(fixture.root.join(name), "pub fn f() -> i32 {\n    1\n}\n").unwrap();
    }
    fixture.git(&["add", "--", "src/a.rs", "src/b.rs"]);
    fixture.git(&["commit", "--quiet", "-m", "rust edit fixture"]);
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let mut daemon = fixture
        .spawn_configured_daemon(Some(&home), true, Duration::from_secs(30), None)
        .await;
    let mut actor = ProductActor::new(&fixture, "edit-files-check").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"edit-files-check"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let settle = |expected: usize| {
        let database = fixture.telemetry.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
            while completed_check_count(&database).await < expected {
                assert!(tokio::time::Instant::now() < deadline, "no check completed");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            // Quiet period: nothing else may start.
            tokio::time::sleep(Duration::from_secs(2)).await;
            completed_check_count(&database).await
        }
    };
    let before = settle(1).await;
    let (read, _) = read_text(
        &mut actor,
        &fixture,
        json!({"items":[{"path":"src/lib.rs"},{"path":"src/a.rs"},{"path":"src/b.rs"}]}),
    )
    .await;
    let source_ref = read["detail_ref"].as_str().unwrap().to_owned();
    let three = json!({"operation_id":"rust-three","source_ref":source_ref,"files":[
        {"path":"src/lib.rs","changes":[{"lines":"1-1","content":"pub fn value() -> i32 { 8 }"}]},
        {"path":"src/a.rs","changes":[{"lines":"2-2","content":"    2"}]},
        {"path":"src/b.rs","changes":[{"lines":"2-2","content":"    3"}]}]});
    let (reply, text) = settled_edit(&mut actor, &fixture, three.clone()).await;
    assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
    assert!(text.starts_with("edit: applied 3 of 3 files"), "{text}");
    for (index, name) in ["src/lib.rs", "src/a.rs", "src/b.rs"].iter().enumerate() {
        let line = text
            .lines()
            .find(|line| line.starts_with(&format!("  {} {name}: replaced", index + 1)))
            .unwrap_or_else(|| panic!("no line for {name}: {text}"));
        assert!(line.contains("; diagnostics "), "{line}");
    }
    assert_eq!(
        settle(before + 1).await,
        before + 1,
        "exactly one project check ran for the three files"
    );
    // A retry of the settled operation writes nothing and answers the settled diagnostics.
    let (retry, retried) = settled_edit(&mut actor, &fixture, three).await;
    assert_eq!(retry["outcome"], "applied", "{retry}");
    let summary = |text: &str| {
        text.lines()
            .next()
            .and_then(|line| line.split_once("; diagnostics: "))
            .map(|(_, words)| words.to_owned())
    };
    // The first reply may be the immediate `pending` one (the check arrives as a fact); the
    // retry then answers the settled diagnostics, which are never pending.
    let first_pending = summary(&text).is_some_and(|words| words.starts_with("pending"));
    if first_pending {
        assert!(
            summary(&retried).is_some_and(|words| !words.starts_with("pending")),
            "{retried}"
        );
    } else {
        assert_eq!(summary(&retried), summary(&text), "{retried}");
    }
    assert!(!retried.contains("no checked language"), "{retried}");
    for (index, name) in ["src/lib.rs", "src/a.rs", "src/b.rs"].iter().enumerate() {
        let words = |text: &str| {
            text.lines()
                .find(|line| line.starts_with(&format!("  {} {name}: ", index + 1)))
                .and_then(|line| line.split_once("; diagnostics "))
                .map(|(_, words)| words.to_owned())
        };
        assert!(
            retried.contains(&format!(
                "  {} {name}: written earlier by this operation_id",
                index + 1
            )),
            "{retried}"
        );
        if !first_pending {
            assert_eq!(words(&retried), words(&text), "{name}: {retried}");
        }
    }
    let edit_ref = reply["source_ref"].as_str().unwrap().to_owned();
    let next = actor
        .call(
            &fixture,
            "ide.edit",
            json!({"operation_id":"rust-next","path":"src/b.rs","source_ref":edit_ref,"changes":[{"lines":"2-2","content":"    4"}]}),
        )
        .await;
    let next = actor.settle(&fixture, next).await;
    assert_eq!(next["result"]["outcome"], "replaced", "{next}");
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A multi-file edit whose one project check outlasts the inline window answers at once with
/// every file written and `diagnostics: pending`; the check then reaches the agent as exactly one
/// `edit-checked` fact for the whole operation (session-outbox-v1 §3.3, §8.6).
#[tokio::test]
async fn slow_multi_file_edit_check_arrives_as_one_edit_checked_fact() {
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(
        fixture.root.join("src/a.rs"),
        "pub fn f() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "src/a.rs"]);
    fixture.git(&["commit", "--quiet", "-m", "slow check fixture"]);
    let home = enable_fake_rust_checks_holding(&fixture, &fixture.base, "sleep 12");
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "slow-batch-check").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"slow-batch-check"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let (read, _) = read_text(
        &mut actor,
        &fixture,
        json!({"items":[{"path":"src/lib.rs"},{"path":"src/a.rs"}]}),
    )
    .await;
    let source_ref = read["detail_ref"].as_str().unwrap().to_owned();
    let (edit, text) = call_text(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"operation_id":"slow-batch","source_ref":source_ref,"files":[
            {"path":"src/lib.rs","changes":[{"lines":"1-1","content":"pub fn value() -> i32 { 8 }"}]},
            {"path":"src/a.rs","changes":[{"lines":"2-2","content":"    2"}]}]}),
    )
    .await;
    assert_eq!(
        edit["state"], "edit_batch",
        "the check never parks the reply: {edit} {text}"
    );
    assert!(
        text.starts_with("edit: applied 2 of 2 files; diagnostics: pending"),
        "{text}"
    );
    assert_no_source_ref(&text);
    assert!(
        text.contains(
            "diagnostics: pending; the project check arrives by itself as an edit-checked fact"
        ),
        "{text}"
    );
    let checked = actor
        .call(&fixture, "ide.inspect", json!({"wait_ms":60000}))
        .await;
    let checked_text = batch_without_status_plate(checked["text"].as_str().unwrap_or_default());
    assert!(
        checked_text.starts_with("edit-checked edit slow-batch: 2 files"),
        "{checked}"
    );
    assert!(
        checked_text.contains("src/lib.rs: ") && checked_text.contains("src/a.rs: "),
        "{checked}"
    );
    let again = actor.call(&fixture, "ide.inspect", json!({})).await;
    assert!(
        again["text"]
            .as_str()
            .is_some_and(|text| batch_without_status_plate(text).starts_with("nothing ready yet")),
        "the fact is handed over once: {again}"
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A rename across two files goes through the multi-file commit with the symbol's own file last:
/// a native write that lands before that file's write (test seam) stops the rename with the other
/// file written and the reply says so per file; after the file is restored, the same
/// `operation_id` finishes the rename with the usual `edit: renamed` reply; once settled the same
/// request answers from the receipt and a different rename under that id conflicts.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn rename_stopped_midway_reports_each_file_and_the_same_operation_id_finishes_it() {
    let fixture = symbol_test_fixture();
    let lib = "pub mod other;\npub fn helper() -> i32 {\n    1\n}\npub fn local() -> i32 {\n    helper()\n}\n";
    std::fs::write(fixture.root.join("src/lib.rs"), lib).unwrap();
    std::fs::write(
        fixture.root.join("src/other.rs"),
        "pub fn uses() -> i32 {\n    crate::helper()\n}\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "src/lib.rs", "src/other.rs"]);
    fixture.git(&["commit", "--quiet", "-m", "rename fixture"]);
    let (fault, _) = fault_flag(&fixture, "batch_edit_before_write_2", true);
    let env = [("AGENT_IDE_TEST_FAULT", fault.as_str())];
    let mut daemon = fixture
        .spawn_configured_daemon_with_env(None, false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = ProductActor::new(&fixture, "rename-resume").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"rename-resume"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let warmup = actor
        .call(&fixture, "ide.outline", json!({"path":"src/lib.rs"}))
        .await;
    assert_eq!(actor.settle(&fixture, warmup).await["kind"], "outline");
    let rename = json!({"operation_id":"rename-op","op":"rename","symbol":"src/lib.rs#helper","new_name":"assist"});

    let (reply, text) = settled_edit(&mut actor, &fixture, rename.clone()).await;
    assert_eq!(reply["state"], "edit_batch", "{reply} {text}");
    assert!(
        text.starts_with(
            "edit: partial — written 1 of 2; not written: 2 src/lib.rs (changed after validation)"
        ),
        "{text}"
    );
    assert!(text.contains("  1 src/other.rs: replaced"), "{text}");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/other.rs")).unwrap(),
        "pub fn uses() -> i32 {\n    crate::assist()\n}\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap(),
        format!("{lib}native\n"),
        "the native bytes are never overwritten"
    );

    std::fs::write(fixture.root.join("src/lib.rs"), lib).unwrap();
    let finished = actor.call(&fixture, "ide.edit", rename.clone()).await;
    let finished = actor.settle(&fixture, finished).await;
    assert_eq!(finished["state"], "edit", "{finished}");
    assert_eq!(finished["operation"], "renamed", "{finished}");
    assert!(
        finished["note"]
            .as_str()
            .unwrap_or_default()
            .starts_with("renamed helper → assist; "),
        "{finished}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap(),
        lib.replace("helper", "assist")
    );

    let (again, text) = settled_edit(&mut actor, &fixture, rename.clone()).await;
    assert_eq!(again["state"], "edit_batch", "{again} {text}");
    assert!(text.starts_with("edit: applied 2 of 2 files"), "{text}");
    assert!(
        text.contains("written earlier by this operation_id"),
        "{text}"
    );
    let mut other = rename.clone();
    other["new_name"] = json!("different");
    let (_, text) = settled_edit(&mut actor, &fixture, other).await;
    assert!(text.starts_with("edit: conflicting_duplicate"), "{text}");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap(),
        lib.replace("helper", "assist")
    );

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// `ide.test {targets}` runs every target as ONE run of the worktree: the symbol targets of one
/// language merge into one runner invocation (libtest exact names); the two integration test
/// files, which cargo's admitted recipes run one per invocation, run one after another in the
/// same run; and a target the runner cannot take is answered on its own line. While the run is live, another
/// several-target start in the worktree answers exactly `tests #1: still running`; the settled
/// result has one line per target and one rerun line, and that line reproduces the whole run.
#[tokio::test]
async fn test_targets_run_as_one_run_with_a_line_per_target_and_one_rerun() {
    let fixture = symbol_test_fixture();
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub struct FileFlag;\nimpl FileFlag { pub fn is_file(&self) -> bool { true } }\n\
         pub fn twice(x: i32) -> i32 {\n    x * 2\n}\npub fn half(x: i32) -> i32 {\n    x / 2\n}\n\
         #[cfg(test)]\nmod tests {\n    #[test]\n    fn doubles() {\n        assert_eq!(super::twice(2), 4);\n    }\n\
         \n    #[test]\n    fn fails() {\n        assert_eq!(super::twice(2), 5);\n    }\n\
         \n    #[test]\n    fn halves() {\n        assert_eq!(super::half(4), 2);\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("tests/more_tests.rs"),
        "#[test]\nfn more_is_file() {\n    assert!(product_fixture::FileFlag.is_file());\n}\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "src/lib.rs", "tests/more_tests.rs"]);
    fixture.git(&["commit", "--quiet", "-m", "test targets fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "test-targets").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"test-targets"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let warm = actor
        .call(&fixture, "ide.symbol", json!({"symbol":"src/lib.rs#twice"}))
        .await;
    assert_eq!(actor.settle(&fixture, warm).await["kind"], "symbol");

    let run = actor
        .call(
            &fixture,
            "ide.test",
            json!({"targets":[
                {"symbol":"src/lib.rs#twice"},
                {"symbol":"src/lib.rs#half"},
                {"path":"tests/path_tests.rs"},
                {"path":"tests/more_tests.rs"},
                {"path":"README.md"}
            ]}),
        )
        .await;
    let run = actor.settle(&fixture, run).await;
    let text = batch_without_status_plate(run["text"].as_str().unwrap_or_default()).to_owned();
    assert!(
        text.starts_with(
            "tests #1: started — 5 targets in 3 runs (budget 120 s); the result arrives by itself \
             as a test-finished fact\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n  target 1 symbol src/lib.rs#twice: run 1 (3 tests selected)")
            && text.contains("\n  target 2 symbol src/lib.rs#half: run 1 (3 tests selected)")
            && text.contains("\n  target 3 path tests/path_tests.rs: run 2")
            && text.contains("\n  target 4 path tests/more_tests.rs: run 3")
            && text.contains("\n  target 5 path README.md: not runnable: "),
        "{text}"
    );
    let busy = actor
        .call(
            &fixture,
            "ide.test",
            json!({"targets":[{"pattern":"doubles"}]}),
        )
        .await;
    assert_eq!(
        batch_without_status_plate(busy["text"].as_str().unwrap_or_default()),
        "tests #1: still running",
        "one run per worktree, no seconds on unfinished work: {busy}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
    let result = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run did not settle"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        let status = actor.call(&fixture, "ide.test", json!({"status":1})).await;
        let text = batch_without_status_plate(status["text"].as_str().unwrap_or_default());
        if !text.contains("running") {
            break text.to_owned();
        }
    };
    assert!(
        result.starts_with("tests #1: 4 passed, 1 failed, "),
        "{result}"
    );
    assert!(result.contains("FAIL tests::fails"), "{result}");
    assert!(
        result.contains("\n  target 1 symbol src/lib.rs#twice: run 1 — 2 passed, 1 failed")
            && result.contains("\n  target 2 symbol src/lib.rs#half: run 1 — 2 passed, 1 failed")
            && result.contains("\n  target 3 path tests/path_tests.rs: run 2 — 1 passed, 0 failed")
            && result.contains("\n  target 4 path tests/more_tests.rs: run 3 — 1 passed, 0 failed")
            && result.contains("\n  target 5 path README.md: not runnable: "),
        "{result}"
    );
    let rerun = result
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("rerun: "))
        .unwrap_or_else(|| panic!("one rerun line: {result}"));
    assert!(
        rerun.starts_with("sh -c 'cargo test --workspace -- --exact ")
            && rerun.ends_with(
                "; cargo test --manifest-path Cargo.toml --test path_tests; cargo test \
                 --manifest-path Cargo.toml --test more_tests'"
            ),
        "{rerun}"
    );
    let reproduced = std::process::Command::new("/bin/sh")
        .args(["-c", rerun])
        .current_dir(&fixture.root)
        .output()
        .unwrap();
    let reproduced = String::from_utf8_lossy(&reproduced.stdout);
    for line in [
        "test tests::doubles ... ok",
        "test tests::fails ... FAILED",
        "test tests::halves ... ok",
        "test checks_is_file ... ok",
        "test more_is_file ... ok",
    ] {
        assert!(reproduced.contains(line), "{line}: {reproduced}");
    }

    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Path targets of a runner that takes several files at once share ONE invocation per language:
/// two pytest test files run as one `pytest a b` run. Whether or not this machine has pytest, the
/// merge shows in the run numbers; with pytest the rerun line names both files.
#[tokio::test]
async fn path_targets_of_one_pytest_project_share_one_invocation() {
    let fixture = ProductFixture::new(json!([]));
    std::fs::create_dir_all(fixture.root.join("tools")).unwrap();
    std::fs::write(fixture.root.join("tools/requirements-ml.txt"), "pytest\n").unwrap();
    std::fs::write(fixture.root.join("tools/analyze.py"), "import pandas\n").unwrap();
    std::fs::create_dir_all(fixture.root.join("tests")).unwrap();
    for name in ["test_a.py", "test_b.py"] {
        std::fs::write(
            fixture.root.join("tests").join(name),
            "def test_it():\n    assert [] == []\n",
        )
        .unwrap();
    }
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "pytest targets fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "pytest-targets").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"pytest-targets"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let run = actor
        .call(
            &fixture,
            "ide.test",
            json!({"targets":[{"path":"tests/test_a.py"},{"path":"tests/test_b.py"}]}),
        )
        .await;
    let run = actor.settle(&fixture, run).await;
    let text = batch_without_status_plate(run["text"].as_str().unwrap_or_default()).to_owned();
    assert!(
        text.starts_with("tests #1: started — 2 targets in 1 runs ("),
        "{run}"
    );
    assert!(
        text.contains("\n  target 1 path tests/test_a.py: run 1")
            && text.contains("\n  target 2 path tests/test_b.py: run 1"),
        "{text}"
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let result = loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run did not settle"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        let status = actor.call(&fixture, "ide.test", json!({"status":1})).await;
        let text = batch_without_status_plate(status["text"].as_str().unwrap_or_default());
        if !text.contains("running") {
            break text.to_owned();
        }
    };
    let rerun = result
        .lines()
        .find_map(|line| line.trim_start().strip_prefix("rerun: "))
        .unwrap_or_else(|| panic!("one rerun line: {result}"));
    assert!(!rerun.starts_with("sh -c"), "one invocation: {result}");
    // Without pytest the planned run cannot start and its rerun names only the missing program.
    if !result.contains("could not start") {
        assert!(
            rerun.contains("tests/test_a.py tests/test_b.py"),
            "one invocation for both files: {result}"
        );
    }
    assert!(
        result.contains("\n  target 1 path tests/test_a.py: run 1")
            && result.contains("\n  target 2 path tests/test_b.py: run 1"),
        "{result}"
    );
    actor.call(&fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
