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
    let (read, text) = settled_read(
        actor,
        fixture,
        json!({"items": EDIT_FILES.iter().map(|name| json!({"path": name})).collect::<Vec<_>>()}),
    )
    .await;
    assert!(
        source_ref_line(&text).contains("valid for a.md, b.md, c.md"),
        "{text}"
    );
    read["detail_ref"].as_str().unwrap().to_owned()
}

/// Calls `ide.edit` and returns the settled structured reply and its compact text.
async fn settled_edit(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    arguments: Value,
) -> (Value, String) {
    let reply = batch_call_with_text(actor, fixture, "ide.edit", arguments).await;
    batch_settle_with_text(actor, fixture, reply).await
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
    assert_eq!(reply["kind"], "edit_batch", "{reply} {text}");
    assert!(
        text.starts_with("edit: applied 3 of 3 files; source_ref "),
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
        assert_eq!(
            lost["result"]["outcome"], "outcome_unknown",
            "a crash after a write never answers internal: {lost}"
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
        assert_eq!(reply["kind"], "edit_batch", "{reply} {text}");
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
        assert_eq!(reply["kind"], "edit_batch", "{reply} {text}");
        let stale = EDIT_FILES[position - 1];
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
        assert_eq!(reply["kind"], "edit_batch", "{reply} {text}");
        assert!(text.starts_with("edit: applied 3 of 3 files"), "{text}");
        assert_files(&fixture, edited, "the resume finished the rest once");

        let mut other = arguments.clone();
        other["files"][0]["changes"][0]["content"] = json!("different");
        let (_, text) = settled_edit(&mut actor, &fixture, other).await;
        assert!(text.starts_with("edit: conflicting_duplicate"), "{text}");
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
    let (read, _) = settled_read(
        &mut actor,
        &fixture,
        json!({"items":[{"path":"src/lib.rs"},{"path":"src/a.rs"},{"path":"src/b.rs"}]}),
    )
    .await;
    let source_ref = read["detail_ref"].as_str().unwrap().to_owned();
    let (reply, text) = settled_edit(
        &mut actor,
        &fixture,
        json!({"operation_id":"rust-three","source_ref":source_ref,"files":[
            {"path":"src/lib.rs","changes":[{"lines":"1-1","content":"pub fn value() -> i32 { 8 }"}]},
            {"path":"src/a.rs","changes":[{"lines":"2-2","content":"    2"}]},
            {"path":"src/b.rs","changes":[{"lines":"2-2","content":"    3"}]}]}),
    )
    .await;
    assert_eq!(reply["kind"], "edit_batch", "{reply} {text}");
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
    let edit_ref = reply["detail_ref"].as_str().unwrap().to_owned();
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
