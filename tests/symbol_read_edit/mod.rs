//! Boundary tests of `symbol-read-edit-v1` (docs/contracts/symbol-read-edit-v1.md section 7)
//! against the real daemon and MCP front.
//!
//! Included from `product_mcp_contract.rs` so these tests share its fixture, actor and settle
//! helpers; each test owns its fixture worktree and daemon. Every case is an exact request and
//! an exact (or exactly-prefixed) reply, with the file on disk asserted after each edit.

use super::*;

/// The fixture file: a header comment, one import, a constant and two functions.
/// Lines: 1 header, 2 import, 4 constant, 6-10 `utility` (6 is its doc comment), 12-14 `second`.
const ORIGINAL: &str = "// util fixture\nuse std::fmt;\n\npub const LIMIT: u32 = 3;\n\n/// Doubles a value.\npub fn utility(x: u32) -> u32 {\n    let a = x;\n    a * 2\n}\n\npub fn second() -> u32 {\n    3\n}\n";

/// The fixture file path, relative to the worktree.
const FILE: &str = "src/batch.rs";

/// One settled call with its compact text, any due status plate stripped.
async fn call(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    tool: &str,
    arguments: Value,
) -> (Value, String) {
    let reply = batch_call_with_text(actor, fixture, tool, arguments).await;
    let (reply, text) = batch_settle_with_text(actor, fixture, reply).await;
    (reply, batch_without_status_plate(&text).to_owned())
}

/// Restores the fixture file to [`ORIGINAL`] before the next case.
fn reset(fixture: &ProductFixture) {
    std::fs::write(fixture.root.join(FILE), ORIGINAL).unwrap();
}

/// The fixture file as it is on disk.
fn on_disk(fixture: &ProductFixture) -> String {
    std::fs::read_to_string(fixture.root.join(FILE)).unwrap()
}

/// Starts the daemon and an actor warmed on [`FILE`].
async fn started(name: &'static str) -> (ProductFixture, OwnedDaemon, ProductActor) {
    let fixture = batch_edit_fixture(ORIGINAL);
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let daemon = fixture.daemon_with_home(Some(&home)).await;
    let actor = batch_started_actor(&fixture, name, FILE).await;
    (fixture, daemon, actor)
}

/// Stops the actor and the daemon.
async fn finished(mut actor: ProductActor, mut daemon: OwnedDaemon, fixture: &ProductFixture) {
    actor.call(fixture, "ide.stop", json!({})).await;
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// R1-R7: reads number lines inside the unit, accept windows, regions and `path:line`, clamp
/// plain ranges at the end of the file and keep per-item failures per item.
#[tokio::test]
async fn reads_by_address_number_lines_inside_the_unit() {
    let (fixture, daemon, mut actor) = started("sre-read").await;
    // R1: header with extent, lines numbered from 1, the doc comment is line 1, no source_ref.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    assert!(
        text.starts_with(
            "src/batch.rs#utility  (5 lines; file lines 6-10)\n1\t/// Doubles a value.\n2\tpub fn utility(x: u32) -> u32 {\n3\t    let a = x;\n4\t    a * 2\n5\t}\n"
        ),
        "{text}"
    );
    assert!(!text.contains("source_ref"), "{text}");
    // R2: a window keeps the unit's numbers.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility:3-4"}),
    )
    .await;
    assert!(
        text.starts_with(
            "src/batch.rs#utility:3-4  (lines 3-4 of 5; file lines 8-9)\n3\t    let a = x;\n4\t    a * 2\n"
        ),
        "{text}"
    );
    // R3: an absolute `path:line` reads the whole enclosing unit and names the line inside it.
    let absolute = format!("{}:9", fixture.root.join(FILE).display());
    let (_, text) = call(&mut actor, &fixture, "ide.read", json!({"symbol":absolute})).await;
    assert!(
        text.starts_with(
            "src/batch.rs#utility  (5 lines; file lines 6-10; your line 9 is line 4)\n1\t"
        ),
        "{text}"
    );
    // R4: regions and constants read like symbols.
    let (_, header) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#@header"}),
    )
    .await;
    assert!(
        header.starts_with("src/batch.rs#@header  (") && header.contains("1\t// util fixture"),
        "{header}"
    );
    let (_, imports) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#@imports"}),
    )
    .await;
    assert!(
        imports.starts_with("src/batch.rs#@imports  (") && imports.contains("use std::fmt;"),
        "{imports}"
    );
    let (_, constant) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#LIMIT"}),
    )
    .await;
    assert!(
        constant.starts_with("src/batch.rs#LIMIT  (") && constant.contains("pub const LIMIT"),
        "{constant}"
    );
    // R5: an end past the file is clamped.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"path":FILE,"lines":"10-99"}),
    )
    .await;
    assert!(text.contains("14\t}") && !text.contains("15\t"), "{text}");
    // R6: a start past the file is an answer, not an error.
    let (reply, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"path":FILE,"lines":"400-420"}),
    )
    .await;
    assert!(
        text.contains("file has 14 lines; nothing at 400-420"),
        "{text}"
    );
    assert!(reply["code"].is_null(), "{reply}");
    // R7: a batch mixing the forms keeps its order and fails only the unknown item.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbols":["src/batch.rs#utility:2","src/batch.rs:13","src/batch.rs#Missing/x"]}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility:2  (lines 2-2 of 5")
            && text.contains("src/batch.rs#second  (3 lines")
            && text.contains("no such symbol: src/batch.rs#Missing/x"),
        "{text}"
    );
    finished(actor, daemon, &fixture).await;
}

/// E1-E12 without a file-version reference or an operation id: whole symbol, window, scoped
/// old text, file-wide old text, region insert, imports, delete and whole-file content, each with
/// the file asserted on disk and the changed lines named by address.
#[tokio::test]
async fn edits_by_address_need_no_source_ref_and_name_the_changed_lines() {
    let (fixture, daemon, mut actor) = started("sre-edit").await;
    // E4: old/new on the whole file; the reply names the changed line by address.
    let (reply, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"old":"a * 2","new":"a * 3"}),
    )
    .await;
    assert_eq!(reply["result"]["outcome"], "replaced", "{reply}");
    assert!(
        text.starts_with("edit: replaced; path src/batch.rs; diagnostics: "),
        "{text}"
    );
    assert!(
        text.contains("  src/batch.rs#utility:4 (L9)  replaced (1 lines → 1)"),
        "{text}"
    );
    assert!(!text.contains("source_ref"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 3"));
    // E4 aliases.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"file_path":FILE,"old_string":"a * 2","new_string":"a * 4"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility:4 (L9)  replaced"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 4"));
    // E3: old/new scoped to a symbol; an ambiguous match outside the scope is not a problem.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#second","old":"    3","new":"    4"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#second:2 (L13)  replaced"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("    3\n}", "    4\n}"));
    // E2: a window with no remembered read applies and echoes the replaced old lines.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:3-3","content":"    let a = x + 1;"}),
    )
    .await;
    assert!(
        text.contains(
            "src/batch.rs#utility:3 (L8)  replaced (1 lines → 1); was:\n    3\t    let a = x;"
        ),
        "{text}"
    );
    assert_eq!(
        on_disk(&fixture),
        ORIGINAL.replace("let a = x;", "let a = x + 1;")
    );
    // E1: a whole symbol replaces its doc comment and body.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility","content":"pub fn utility(x: u32) -> u32 {\n    x\n}"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility (L6-8)  replaced (5 lines → 3)"),
        "{text}"
    );
    assert!(!on_disk(&fixture).contains("Doubles"));
    // E6: delete names the address the symbol had.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#second","op":"delete"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#second  deleted (3 lines)"),
        "{text}"
    );
    assert!(!on_disk(&fixture).contains("fn second"));
    // E8: add an import after the others, idempotently; remove it again.
    reset(&fixture);
    let statement = "use std::collections::HashMap;";
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"add_import":statement}),
    )
    .await;
    assert!(
        text.contains("#@imports") && text.contains("  added"),
        "{text}"
    );
    let added = on_disk(&fixture);
    // The project formatter runs on the result and may sort the imports, so only their presence
    // in the imports block is asserted.
    assert!(
        added.contains(&format!("{statement}\n")) && added.contains("use std::fmt;\n"),
        "{added}"
    );
    assert_eq!(added.matches("use std::").count(), 2, "{added}");
    let (reply, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"add_import":statement}),
    )
    .await;
    assert!(
        text.contains("  unchanged") || reply["result"]["outcome"] == "unchanged",
        "{text}"
    );
    assert_eq!(
        on_disk(&fixture),
        added,
        "an import already present is not added twice"
    );
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"remove_import":statement}),
    )
    .await;
    assert!(text.contains("  removed"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL);
    // E5: insert next to a region.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#@imports","op":"insert","where":"after","content":"use std::io;"}),
    )
    .await;
    assert!(text.contains("  inserted"), "{text}");
    assert!(
        on_disk(&fixture).contains("use std::fmt;\nuse std::io;\n"),
        "{}",
        on_disk(&fixture)
    );
    // E9: a whole-file content edit replaces an existing file without a reference.
    reset(&fixture);
    let (reply, _) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"content":"pub fn only() {}\n"}),
    )
    .await;
    assert_eq!(reply["result"]["outcome"], "replaced", "{reply}");
    assert_eq!(on_disk(&fixture), "pub fn only() {}\n");
    // E11: several entries of one file resolve against the same bytes.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"changes":[
            {"symbol":"src/batch.rs#utility:4-4","content":"    a * 5"},
            {"symbol":"src/batch.rs#second","old":"    3","new":"    6"}]}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility:4 (L9)")
            && text.contains("src/batch.rs#second:2 (L13)"),
        "{text}"
    );
    assert_eq!(
        on_disk(&fixture),
        ORIGINAL
            .replace("a * 2", "a * 5")
            .replace("    3\n}", "    6\n}")
    );
    // Refusals write nothing: an unknown symbol and a window past the unit.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#Missing","content":"x"}),
    )
    .await;
    assert!(text.contains("no symbol src/batch.rs#Missing"), "{text}");
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:9-9","content":"x"}),
    )
    .await;
    assert!(text.contains("has 5 lines, not 9"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL);
    finished(actor, daemon, &fixture).await;
}

/// E10: a Codex `apply_patch` body applies Add and Update atomically; a missed hunk and a delete
/// are refused with nothing written.
#[tokio::test]
async fn apply_patch_applies_atomically_and_refuses_misses() {
    let (fixture, daemon, mut actor) = started("sre-patch").await;
    let patch = "*** Begin Patch\n*** Update File: src/batch.rs\n@@\n-    a * 2\n+    a * 7\n*** Add File: src/added.rs\n+pub fn added() {}\n*** End Patch";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"patch":patch})).await;
    assert!(text.starts_with("edit: applied 2 of 2 files"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 7"));
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("src/added.rs")).unwrap(),
        "pub fn added() {}\n"
    );
    reset(&fixture);
    let miss = "*** Begin Patch\n*** Update File: src/batch.rs\n@@\n-    no such line\n+    x\n*** End Patch";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"patch":miss})).await;
    assert!(
        text.contains("patch refused, nothing written: hunk 1 of src/batch.rs does not match"),
        "{text}"
    );
    assert!(
        text.contains("Repair: resend the hunk with these lines as context."),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL);
    let delete = "*** Begin Patch\n*** Delete File: src/batch.rs\n*** End Patch";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"patch":delete})).await;
    assert!(
        text.contains("patch: delete and move are not supported; use a native tool"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL);
    finished(actor, daemon, &fixture).await;
}

/// E6a-E6c: the light window guard. With a known context epoch, a window edit whose lines changed
/// since the session's read is refused with the unit's current text; edits elsewhere in the unit
/// never refuse it; after a re-read the edit applies.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn window_edits_are_guarded_by_the_last_read() {
    let fixture = batch_edit_fixture(ORIGINAL);
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = batch_started_actor(&fixture, "sre-guard", FILE).await;
    let window = json!({"symbol":"src/batch.rs#utility:3-3","content":"    let a = x + 1;"});
    // The session reads the unit, then another writer changes the window's line.
    call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    std::fs::write(
        fixture.root.join(FILE),
        ORIGINAL.replace("let a = x;", "let a = x * 9;"),
    )
    .unwrap();
    // E6a: refused with the unit's current numbered text and the repair step.
    let (_, text) = call(&mut actor, &fixture, "ide.edit", window.clone()).await;
    assert!(
        text.contains("lines 3-3 of src/batch.rs#utility changed since your read")
            && text.contains("Current text:")
            // A refusal is one line: its line breaks and tabs are escaped.
            && text.contains("let a = x * 9;")
            && text.contains("Repair:"),
        "{text}"
    );
    assert!(on_disk(&fixture).contains("let a = x * 9;"));
    // E6b: a change outside the window never refuses the edit.
    reset(&fixture);
    call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    std::fs::write(fixture.root.join(FILE), ORIGINAL.replace("a * 2", "a * 9")).unwrap();
    let (_, text) = call(&mut actor, &fixture, "ide.edit", window.clone()).await;
    assert!(
        text.contains("src/batch.rs#utility:3 (L8)  replaced"),
        "{text}"
    );
    assert!(on_disk(&fixture).contains("let a = x + 1;"));
    // E6c: after ide.stop nothing is remembered; the edit applies and echoes the old line.
    reset(&fixture);
    call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    actor.call(&fixture, "ide.stop", json!({})).await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"sre-guard-2"}),
        )
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    std::fs::write(
        fixture.root.join(FILE),
        ORIGINAL.replace("let a = x;", "let a = x * 9;"),
    )
    .unwrap();
    let (_, text) = call(&mut actor, &fixture, "ide.edit", window).await;
    assert!(text.contains("; was:\n    3\t    let a = x * 9;"), "{text}");
    finished(actor, daemon, &fixture).await;
}

/// Every plain and batch file read protects the unit lines it actually delivered, exactly as
/// a symbol read does (symbol-read-edit-v1 section 3.2 rule 3). Each form owns fresh memory;
/// reading only another line must still leave the target window unguarded and echo its old text.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn file_range_reads_guard_later_symbol_window_edits() {
    for (form, arguments) in [
        ("range", json!({"path":FILE,"lines":"6-10"})),
        ("whole", json!({"path":FILE})),
        (
            "item-range",
            json!({"items":[{"path":FILE,"lines":"6-10"}]}),
        ),
        ("item-whole", json!({"items":[{"path":FILE}]})),
        ("ranges", json!({"path":FILE,"ranges":["6-10"]})),
    ] {
        let fixture = batch_edit_fixture(ORIGINAL);
        let epoch = fixture.base.join("context-epoch");
        std::fs::write(&epoch, "1").unwrap();
        let home = enable_fake_rust_checks(&fixture, &fixture.base);
        let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
        let daemon = fixture
            .spawn_configured_daemon_with_env(
                Some(&home),
                false,
                Duration::from_secs(30),
                None,
                &env,
            )
            .await;
        let mut actor = batch_started_actor(&fixture, "sre-file-range-guard", FILE).await;
        let (_, read) = call(&mut actor, &fixture, "ide.read", arguments).await;
        assert!(read.contains("let a = x;"), "{form}: {read}");
        let external = ORIGINAL.replace("let a = x;", "let a = x * 9;");
        std::fs::write(fixture.root.join(FILE), &external).unwrap();
        let window = json!({"symbol":"src/batch.rs#utility:3-3","content":"    let a = x + 1;"});
        let (_, text) = call(&mut actor, &fixture, "ide.edit", window.clone()).await;
        assert!(
            text.contains("lines 3-3 of src/batch.rs#utility changed since your read")
                && text.contains("Current text:")
                && text.contains("let a = x * 9;")
                && text.contains("Repair:"),
            "{form}: {text}"
        );
        assert_eq!(on_disk(&fixture), external, "{form}");
        if form == "range" {
            reset(&fixture);
            let (_, read) = call(
                &mut actor,
                &fixture,
                "ide.read",
                json!({"path":FILE,"lines":"9"}),
            )
            .await;
            assert!(
                read.contains("a * 2") && !read.contains("let a = x;"),
                "{read}"
            );
            std::fs::write(fixture.root.join(FILE), &external).unwrap();
            let (_, text) = call(&mut actor, &fixture, "ide.edit", window).await;
            assert!(
                text.contains("src/batch.rs#utility:3 (L8)  replaced")
                    && text.contains("; was:")
                    && text.contains("let a = x * 9;")
                    && !text.contains("changed since your read"),
                "{text}"
            );
            assert!(on_disk(&fixture).contains("let a = x + 1;"));
        }
        finished(actor, daemon, &fixture).await;
    }
}

/// Validation of a refused multi-file edit must not refresh read memory with an unwritten
/// candidate. An external writer choosing those candidate bytes still differs from the last
/// actual read, so a later window edit refuses and preserves the external write.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn refused_batch_candidates_do_not_refresh_window_read_memory() {
    let fixture = batch_edit_fixture(ORIGINAL);
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = batch_started_actor(&fixture, "sre-refused-batch-memory", FILE).await;
    let (_, read) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    assert!(read.contains("let a = x;"), "{read}");
    let other_before = std::fs::read(fixture.root.join("src/lib.rs")).unwrap();
    let (refused, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"files":[
            {"path":FILE,"changes":[{"symbol":"src/batch.rs#utility:3","content":"    let a = x * 9;"}]},
            {"path":"src/lib.rs","changes":[{"old":"absent-memory-regression-marker","new":"replacement"}]}
        ]}),
    )
    .await;
    assert_eq!(refused["code"], "edit_refused", "{refused}");
    assert!(
        text.contains("nothing written") && text.contains("src/lib.rs"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL);
    assert_eq!(
        std::fs::read(fixture.root.join("src/lib.rs")).unwrap(),
        other_before
    );
    let external = ORIGINAL.replace("let a = x;", "let a = x * 9;");
    std::fs::write(fixture.root.join(FILE), &external).unwrap();
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:3","content":"    let a = x + 1;"}),
    )
    .await;
    assert!(
        text.contains("lines 3-3 of src/batch.rs#utility changed since your read")
            && text.contains("Current text:")
            && text.contains("let a = x * 9;")
            && text.contains("Repair:"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), external);
    finished(actor, daemon, &fixture).await;
}

/// A complete file read containing more than 128 units retains the first unit's guard, both
/// immediately and after this session edits another unit. All source fits one reply so eviction
/// cannot be justified by undelivered rows (symbol-read-edit-v1 section 3.2 rule 3).
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn full_file_reads_guard_all_units_after_own_edits() {
    let source: String = (0..129)
        .map(|index| format!("pub fn function_{index}() -> u32 {{\n    {index}\n}}\n"))
        .collect();
    let fixture = batch_edit_fixture(&source);
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = batch_started_actor(&fixture, "sre-many-units-guard", FILE).await;
    for own_edit in [false, true] {
        std::fs::write(fixture.root.join(FILE), &source).unwrap();
        let (reply, read) = call(
            &mut actor,
            &fixture,
            "ide.read",
            json!({"path":FILE,"force":true}),
        )
        .await;
        assert_eq!(reply["kind"], "read", "{reply}");
        assert_eq!(
            reply["truncated"], false,
            "the whole fixture must be delivered: {reply}"
        );
        assert_eq!(reply["continuation"], false, "{reply}");
        assert!(
            read.contains("pub fn function_0()") && read.contains("pub fn function_128()"),
            "{read}"
        );
        let current = if own_edit {
            let (_, text) = call(
                &mut actor,
                &fixture,
                "ide.edit",
                json!({"symbol":"src/batch.rs#function_128:2","content":"    999"}),
            )
            .await;
            assert!(
                text.contains("#function_128:2") && text.contains("replaced"),
                "{text}"
            );
            let expected = source.replace("    128\n", "    999\n");
            assert_eq!(on_disk(&fixture), expected);
            expected
        } else {
            source.clone()
        };
        let external = current.replacen("    0\n", "    777\n", 1);
        std::fs::write(fixture.root.join(FILE), &external).unwrap();
        let (_, text) = call(
            &mut actor,
            &fixture,
            "ide.edit",
            json!({"symbol":"src/batch.rs#function_0:2","content":"    888"}),
        )
        .await;
        assert!(
            text.contains("lines 2-2 of src/batch.rs#function_0 changed since your read")
                && text.contains("Current text:")
                && text.contains("777")
                && text.contains("Repair:"),
            "own_edit={own_edit}: {text}"
        );
        assert_eq!(on_disk(&fixture), external, "own_edit={own_edit}");
    }
    finished(actor, daemon, &fixture).await;
}

/// An own edit or no-op in another unit never adopts an external writer's unread changes.
/// Both file and explicit-unit reads must preserve the original guard until a fresh read.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn own_edits_do_not_adopt_external_changes_in_other_units() {
    for read in [
        json!({"path":FILE}),
        json!({"symbol":"src/batch.rs#utility"}),
    ] {
        for own_value in [5, 3] {
            let fixture = batch_edit_fixture(ORIGINAL);
            let epoch = fixture.base.join("context-epoch");
            std::fs::write(&epoch, "1").unwrap();
            let home = enable_fake_rust_checks(&fixture, &fixture.base);
            let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
            let daemon = fixture
                .spawn_configured_daemon_with_env(
                    Some(&home),
                    false,
                    Duration::from_secs(30),
                    None,
                    &env,
                )
                .await;
            let mut actor = batch_started_actor(&fixture, "sre-other-writer", FILE).await;
            call(&mut actor, &fixture, "ide.read", read.clone()).await;
            let external = ORIGINAL.replace("let a = x;", "let a = x * 9;");
            std::fs::write(fixture.root.join(FILE), &external).unwrap();
            let (_, own) = call(
                &mut actor,
                &fixture,
                "ide.edit",
                json!({
                    "symbol":"src/batch.rs#second:2", "content":format!("    {own_value}"),
                }),
            )
            .await;
            let expected = external.replace("    3\n", &format!("    {own_value}\n"));
            assert_eq!(on_disk(&fixture), expected, "{own}");
            let (_, attempted) = call(
                &mut actor,
                &fixture,
                "ide.edit",
                json!({
                    "symbol":"src/batch.rs#utility:3", "content":"    let a = x + 1;",
                }),
            )
            .await;
            assert!(
                attempted.contains("changed since your read"),
                "read={read}, own={own_value}: {attempted}"
            );
            assert_eq!(
                on_disk(&fixture),
                expected,
                "external bytes were overwritten"
            );
            finished(actor, daemon, &fixture).await;
        }
    }
}

/// The guard covers only the lines the session read, and follows the session's own edits: a
/// window outside a partial read is unguarded and echoes what it replaces (so lines the agent
/// never saw are not overwritten silently), and a window copied from the session's own previous
/// edit reply is not "changed since your read".
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn the_guard_covers_the_lines_read_and_follows_own_edits() {
    let fixture = batch_edit_fixture(ORIGINAL);
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let mut actor = batch_started_actor(&fixture, "sre-guard-span", FILE).await;
    // F4: only unit lines 1-3 were read; line 4 changes on disk; an edit of line 4 is unguarded
    // and shows the old line it replaces.
    call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility:1-3"}),
    )
    .await;
    std::fs::write(fixture.root.join(FILE), ORIGINAL.replace("a * 2", "a * 9")).unwrap();
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:4-4","content":"    a * 5"}),
    )
    .await;
    assert!(
        text.contains(
            "src/batch.rs#utility:4 (L9)  replaced (1 lines → 1); was:\n    4\t    a * 9"
        ),
        "{text}"
    );
    assert!(on_disk(&fixture).contains("    a * 5"));
    // F3: read the whole unit, edit a window, then edit again from the edit's own address: the
    // remembered unit follows the session's write, so nothing is "changed since your read".
    reset(&fixture);
    call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility"}),
    )
    .await;
    let (_, first) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:3-3","content":"    let a = x + 1;"}),
    )
    .await;
    assert!(
        first.contains("src/batch.rs#utility:3 (L8)  replaced"),
        "{first}"
    );
    let (_, second) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:3-3","content":"    let a = x + 2;"}),
    )
    .await;
    assert!(
        second.contains("src/batch.rs#utility:3 (L8)  replaced")
            && !second.contains("changed since your read"),
        "{second}"
    );
    assert!(on_disk(&fixture).contains("let a = x + 2;"));
    finished(actor, daemon, &fixture).await;
}

/// E15 and the open-ended window: a stale deprecated `source_ref` is still refused, with its own
/// wording and no write; `:N-` replaces from line N to the unit's last line.
#[tokio::test]
async fn a_stale_source_ref_is_refused_and_an_open_window_reaches_the_unit_end() {
    let (fixture, daemon, mut actor) = started("sre-stale").await;
    let (reply, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"old":"a * 2","new":"a * 3","source_ref":"no-such-ref-1"}),
    )
    .await;
    assert_eq!(reply["result"]["outcome"], "stale_source", "{reply}");
    assert!(
        text.contains("supplied source_ref does not match")
            && !text.contains("changed while the edit was applied"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility:4-","content":"    a * 7\n}"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility:4-5 (L9-10)  replaced (2 lines → 2)"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 7"));
    finished(actor, daemon, &fixture).await;
}

/// A file-numbered `lines` edit echoes the old lines it replaces, and an edit by a relative
/// `path:line` replaces the whole unit or region that holds the line (symbol-read-edit-v1 §1).
#[tokio::test]
async fn lines_edits_echo_and_locations_address_units_and_regions() {
    let (fixture, daemon, mut actor) = started("sre-lines").await;
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"path":FILE,"lines":"9-9","content":"    a * 4"}),
    )
    .await;
    assert!(
        text.contains("(L9)  replaced (1 lines → 1); was:\n    9\t    a * 2"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 4"));
    // A location inside a function replaces that whole function.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs:9","content":"pub fn utility(x: u32) -> u32 {\n    x\n}"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility (L6-8)  replaced (5 lines → 3)"),
        "{text}"
    );
    assert!(!on_disk(&fixture).contains("Doubles"));
    // A location inside a region replaces the region.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs:2","content":"use std::io;"}),
    )
    .await;
    assert!(text.contains("#@imports"), "{text}");
    let after = on_disk(&fixture);
    assert!(
        after.contains("use std::io;") && !after.contains("use std::fmt;"),
        "{after}"
    );
    finished(actor, daemon, &fixture).await;
}

/// A file with a small function and a `big` one whose numbered text is larger than one reply
/// page. Unit lines of `big`: 1 is its header, 2-6001 the repeated `let`, 6002-6003 its tail.
#[cfg(feature = "test-seams")]
/// A window that ends past its unit is refused on edits (symbol-read-edit-v1 §3.2) although the
/// same window is clamped on reads; the refusal names the unit's length and writes nothing.
#[tokio::test]
async fn an_edit_window_past_the_unit_is_refused_not_clamped() {
    let (fixture, daemon, mut actor) = started("sre-clamp").await;
    for request in [
        json!({"symbol":"src/batch.rs#utility:4-999","content":"    a * 9\n}"}),
        json!({"operation_id":"clamp-changes","path":FILE,"changes":[{"symbol":"src/batch.rs#utility:4-999","content":"    a * 9\n}"}]}),
        json!({"symbol":"src/batch.rs#utility:4-999","old":"a * 2","new":"a * 9"}),
    ] {
        let (_, text) = call(&mut actor, &fixture, "ide.edit", request.clone()).await;
        assert!(
            text.contains("src/batch.rs#utility:4-999 has 5 lines, not 999"),
            "{request}: {text}"
        );
        assert_eq!(on_disk(&fixture), ORIGINAL, "{request}");
    }
    // An end inside the unit and an open end still work, and a read of the same window clamps.
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#utility:4-999"}),
    )
    .await;
    assert!(text.contains("lines 4-5 of 5"), "{text}");
    finished(actor, daemon, &fixture).await;
}

/// An absolute `path:line` inside the worktree edits like the relative one in both scopes: the
/// whole unit that holds the line (E1) and exact old text scoped to it (E3); a file outside the
/// worktree is refused before anything is resolved (symbol-read-edit-v1 §1).
#[tokio::test]
async fn absolute_locations_edit_inside_the_worktree_only() {
    let (fixture, daemon, mut actor) = started("sre-abs").await;
    let absolute = format!("{}:9", fixture.root.join(FILE).display());
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":absolute,"content":"pub fn utility(x: u32) -> u32 {\n    x\n}"}),
    )
    .await;
    assert!(
        text.contains("src/batch.rs#utility (L6-8)  replaced (5 lines → 3)"),
        "{text}"
    );
    assert!(!on_disk(&fixture).contains("Doubles"));
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":absolute,"old":"a * 2","new":"a * 3"}),
    )
    .await;
    assert!(text.contains("replaced"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 3"));
    reset(&fixture);
    let outside = std::env::temp_dir().join("outside-the-worktree.rs");
    let (reply, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":format!("{}:1", outside.display()),"content":"x"}),
    )
    .await;
    // The envelope oracle makes this refusal an MCP error; no file appears outside the worktree.
    assert_eq!(reply["state"], "error", "{text}");
    assert!(!outside.exists());
    assert_eq!(on_disk(&fixture), ORIGINAL);
    finished(actor, daemon, &fixture).await;
}

/// A fixture whose `impl` closes after its last method: line 7 is the closing brace.
const WITH_IMPL: &str =
    "pub struct S;\n\nimpl S {\n    pub fn a(&self) -> u32 {\n        1\n    }\n}\n";

/// A `path:line` on the closing line of a container that follows its last member reads that
/// container; only a line past the file's last line answers "nothing at" (§2.1).
#[tokio::test]
async fn a_location_on_a_closing_container_line_reads_the_container() {
    let fixture = batch_edit_fixture(WITH_IMPL);
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = batch_started_actor(&fixture, "sre-close", FILE).await;
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs:7"}),
    )
    .await;
    assert!(
        text.contains("your line 7 is line 5") && !text.contains("nothing at"),
        "{text}"
    );
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs:8"}),
    )
    .await;
    assert!(text.contains("file has 7 lines; nothing at 8"), "{text}");
    finished(actor, daemon, &fixture).await;
}

fn paged_source() -> String {
    format!(
        "pub fn small() -> u32 {{\n    1\n}}\n\npub fn big() -> u32 {{\n{}    7\n}}\n",
        "    let a = 1;\n".repeat(6000)
    )
}

/// Starts a daemon with a known context epoch (the window guard remembers nothing without one)
/// and an actor warmed on [`paged_source`].
#[cfg(feature = "test-seams")]
async fn paged_started(name: &'static str) -> (ProductFixture, OwnedDaemon, ProductActor) {
    let fixture = batch_edit_fixture(&paged_source());
    let epoch = fixture.base.join("context-epoch");
    std::fs::write(&epoch, "1").unwrap();
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    let env = [("AGENT_IDE_TEST_CONTEXT_EPOCH_FILE", epoch.to_str().unwrap())];
    let daemon = fixture
        .spawn_configured_daemon_with_env(Some(&home), false, Duration::from_secs(30), None, &env)
        .await;
    let actor = batch_started_actor(&fixture, name, FILE).await;
    (fixture, daemon, actor)
}

/// A unit read that is cut into pages counts as read only up to the last row its first page
/// delivered: a window edit of a row on a later page is unguarded and echoes what it replaces,
/// although the unit is unchanged since the read (symbol-read-edit-v1 §3.2 rule 3).
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn a_paged_read_guards_only_the_rows_it_delivered() {
    let (fixture, daemon, mut actor) = paged_started("sre-paged").await;
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbol":"src/batch.rs#big"}),
    )
    .await;
    assert!(
        text.contains("page 1;") && !text.contains("\n5990\t"),
        "the read must be cut before row 5990: {}",
        &text[..text.len().min(300)]
    );
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#big:5990-5990","content":"    let a = 2;"}),
    )
    .await;
    assert!(text.contains("; was:\n    5990\t    let a = 1;"), "{text}");
    finished(actor, daemon, &fixture).await;
}

/// A batch read remembers a unit only when its block is delivered in the reply: a block cut from
/// the reply leaves its unit unguarded, so a window edit there echoes what it replaces.
#[cfg(feature = "test-seams")]
#[tokio::test]
async fn a_batch_read_guards_only_the_blocks_it_delivered() {
    let (fixture, daemon, mut actor) = paged_started("sre-batch-cut").await;
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.read",
        json!({"symbols":["src/batch.rs#small","src/batch.rs#big"]}),
    )
    .await;
    assert!(
        text.contains("1\tpub fn small() -> u32 {") && !text.contains("src/batch.rs#big  ("),
        "the big block must be cut: {}",
        &text[..text.len().min(400)]
    );
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#big:100-100","content":"    let a = 2;"}),
    )
    .await;
    assert!(text.contains("; was:\n    100\t    let a = 1;"), "{text}");
    finished(actor, daemon, &fixture).await;
}
