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
    let (_, text) = call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#utility"})).await;
    assert!(
        text.starts_with(
            "src/batch.rs#utility  (5 lines; file lines 6-10)\n1\t/// Doubles a value.\n2\tpub fn utility(x: u32) -> u32 {\n3\t    let a = x;\n4\t    a * 2\n5\t}\n"
        ),
        "{text}"
    );
    assert!(!text.contains("source_ref"), "{text}");
    // R2: a window keeps the unit's numbers.
    let (_, text) = call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#utility:3-4"})).await;
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
        text.starts_with("src/batch.rs#utility  (5 lines; file lines 6-10; your line 9 is line 4)\n1\t"),
        "{text}"
    );
    // R4: regions and constants read like symbols.
    let (_, header) = call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#@header"})).await;
    assert!(header.starts_with("src/batch.rs#@header  (") && header.contains("1\t// util fixture"), "{header}");
    let (_, imports) = call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#@imports"})).await;
    assert!(imports.starts_with("src/batch.rs#@imports  (") && imports.contains("use std::fmt;"), "{imports}");
    let (_, constant) = call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#LIMIT"})).await;
    assert!(constant.starts_with("src/batch.rs#LIMIT  (") && constant.contains("pub const LIMIT"), "{constant}");
    // R5: an end past the file is clamped.
    let (_, text) = call(&mut actor, &fixture, "ide.read", json!({"path":FILE,"lines":"10-99"})).await;
    assert!(text.contains("14\t}") && !text.contains("15\t"), "{text}");
    // R6: a start past the file is an answer, not an error.
    let (reply, text) = call(&mut actor, &fixture, "ide.read", json!({"path":FILE,"lines":"400-420"})).await;
    assert!(text.contains("file has 14 lines; nothing at 400-420"), "{text}");
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
        text.contains("src/batch.rs#utility:2  (lines 2-2 of 5") && text.contains("src/batch.rs#second  (3 lines")
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
    let (reply, text) = call(&mut actor, &fixture, "ide.edit", json!({"path":FILE,"old":"a * 2","new":"a * 3"})).await;
    assert_eq!(reply["result"]["outcome"], "replaced", "{reply}");
    assert!(text.starts_with("edit: replaced; path src/batch.rs; diagnostics: "), "{text}");
    assert!(text.contains("  src/batch.rs#utility:4 (L9)  replaced (1 lines → 1)"), "{text}");
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
    assert!(text.contains("src/batch.rs#utility:4 (L9)  replaced"), "{text}");
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
    assert!(text.contains("src/batch.rs#second:2 (L13)  replaced"), "{text}");
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
        text.contains("src/batch.rs#utility:3 (L8)  replaced (1 lines → 1); was:\n    3\t    let a = x;"),
        "{text}"
    );
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("let a = x;", "let a = x + 1;"));
    // E1: a whole symbol replaces its doc comment and body.
    reset(&fixture);
    let (_, text) = call(
        &mut actor,
        &fixture,
        "ide.edit",
        json!({"symbol":"src/batch.rs#utility","content":"pub fn utility(x: u32) -> u32 {\n    x\n}"}),
    )
    .await;
    assert!(text.contains("src/batch.rs#utility (L6-8)  replaced (5 lines → 3)"), "{text}");
    assert!(!on_disk(&fixture).contains("Doubles"));
    // E6: delete names the address the symbol had.
    reset(&fixture);
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"symbol":"src/batch.rs#second","op":"delete"})).await;
    assert!(text.contains("src/batch.rs#second  deleted (3 lines)"), "{text}");
    assert!(!on_disk(&fixture).contains("fn second"));
    // E8: add an import after the others, idempotently; remove it again.
    reset(&fixture);
    let statement = "use std::collections::HashMap;";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"path":FILE,"add_import":statement})).await;
    assert!(text.contains("#@imports") && text.contains("  added"), "{text}");
    let added = on_disk(&fixture);
    assert!(added.contains(&format!("use std::fmt;\n{statement}\n")), "{added}");
    let (reply, text) = call(&mut actor, &fixture, "ide.edit", json!({"path":FILE,"add_import":statement})).await;
    assert!(text.contains("  unchanged") || reply["result"]["outcome"] == "unchanged", "{text}");
    assert_eq!(on_disk(&fixture), added, "an import already present is not added twice");
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"path":FILE,"remove_import":statement})).await;
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
    assert!(on_disk(&fixture).contains("use std::fmt;\nuse std::io;\n"), "{}", on_disk(&fixture));
    // E9: a whole-file content edit replaces an existing file without a reference.
    reset(&fixture);
    let (reply, _) = call(&mut actor, &fixture, "ide.edit", json!({"path":FILE,"content":"pub fn only() {}\n"})).await;
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
    assert!(text.contains("src/batch.rs#utility:4 (L9)") && text.contains("src/batch.rs#second:2 (L13)"), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL.replace("a * 2", "a * 5").replace("    3\n}", "    6\n}"));
    // Refusals write nothing: an unknown symbol and a window past the unit.
    reset(&fixture);
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"symbol":"src/batch.rs#Missing","content":"x"})).await;
    assert!(text.contains("no symbol src/batch.rs#Missing"), "{text}");
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"symbol":"src/batch.rs#utility:9-9","content":"x"})).await;
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
    assert_eq!(std::fs::read_to_string(fixture.root.join("src/added.rs")).unwrap(), "pub fn added() {}\n");
    reset(&fixture);
    let miss = "*** Begin Patch\n*** Update File: src/batch.rs\n@@\n-    no such line\n+    x\n*** End Patch";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"patch":miss})).await;
    assert!(text.contains("patch refused, nothing written: hunk 1 of src/batch.rs does not match"), "{text}");
    assert!(text.contains("Repair: resend the hunk with these lines as context."), "{text}");
    assert_eq!(on_disk(&fixture), ORIGINAL);
    let delete = "*** Begin Patch\n*** Delete File: src/batch.rs\n*** End Patch";
    let (_, text) = call(&mut actor, &fixture, "ide.edit", json!({"patch":delete})).await;
    assert!(text.contains("patch: delete and move are not supported; use a native tool"), "{text}");
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
    call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#utility"})).await;
    std::fs::write(fixture.root.join(FILE), ORIGINAL.replace("let a = x;", "let a = x * 9;")).unwrap();
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
    call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#utility"})).await;
    std::fs::write(fixture.root.join(FILE), ORIGINAL.replace("a * 2", "a * 9")).unwrap();
    let (_, text) = call(&mut actor, &fixture, "ide.edit", window.clone()).await;
    assert!(text.contains("src/batch.rs#utility:3 (L8)  replaced"), "{text}");
    assert!(on_disk(&fixture).contains("let a = x + 1;"));
    // E6c: after ide.stop nothing is remembered; the edit applies and echoes the old line.
    reset(&fixture);
    call(&mut actor, &fixture, "ide.read", json!({"symbol":"src/batch.rs#utility"})).await;
    actor.call(&fixture, "ide.stop", json!({})).await;
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"sre-guard-2"}))
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    std::fs::write(fixture.root.join(FILE), ORIGINAL.replace("let a = x;", "let a = x * 9;")).unwrap();
    let (_, text) = call(&mut actor, &fixture, "ide.edit", window).await;
    assert!(text.contains("; was:\n    3\t    let a = x * 9;"), "{text}");
    finished(actor, daemon, &fixture).await;
}
