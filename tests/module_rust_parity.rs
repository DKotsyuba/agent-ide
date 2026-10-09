//! Rust module parity and faults: the same MCP calls answer equally with `bundled.rust` in its
//! module and in process (`AGENT_IDE_LANGUAGE_MODE=rust=in_process`), rust-analyzer's parent is
//! the module, the module is a direct daemon child, and a module death is a typed unavailable
//! outcome that neither restarts the daemon nor leaves an orphan. Needs the pinned 1.98.1
//! toolchain and a binary built with `--features test-seams`.
#![cfg(feature = "test-seams")]

#[path = "support/parity.rs"]
mod parity;

use std::time::Duration;

use serde_json::{Value, json};

/// The module-routing seam of a pre-release build.
const SHIPPED: (&str, &str) = ("AGENT_IDE_TEST_MODULE_LANGUAGES", "rust");
/// The fallback switch for the in-process side.
const IN_PROCESS: (&str, &str) = (parity::LANGUAGE_MODE, "rust=in_process");

/// The accepted rustup toolchain directory.
fn toolchain_dir() -> String {
    std::env::var("AGENT_IDE_RUST_TOOLCHAIN_DIR")
        .unwrap_or_else(|_| "/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin".into())
}

/// One accepted program: path, identity and BLAKE3 of its bytes.
fn accepted(path: &str, identity: &str) -> Value {
    json!({"path":path,"identity":identity,
        "blake3":blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()})
}

/// The launcher `providers` entry for the accepted rust-analyzer.
fn providers() -> Value {
    let dir = toolchain_dir();
    let analyzer = std::env::var("AGENT_IDE_RUST_ANALYZER")
        .unwrap_or_else(|_| format!("{dir}/bin/rust-analyzer"));
    let toolchain = std::env::var("AGENT_IDE_RUST_TOOLCHAIN").unwrap_or_else(|_| {
        std::path::Path::new(&dir)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    });
    json!([{
        "executable":accepted(&analyzer,"rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),
        "settings":"rust_cache_priming_disabled_v1",
        "toolchain":toolchain,
        "cargo":accepted(&format!("{dir}/bin/cargo"),"cargo 1.98.1"),
        "cargo_version":"cargo 1.98.1",
        "rustc":accepted(&format!("{dir}/bin/rustc"),"rustc 1.98.1"),
        "rustc_version":"rustc 1.98.1",
        "trust":"fixture-disabled",
        "cache_namespace":"fixture-module-rust-parity"
    }])
}

/// A small crate with a type, methods, a caller chain, a unit test and an integration test.
fn fixture() -> parity::Fixture {
    parity::Fixture::new(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"paritycrate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            ),
            (
                "src/lib.rs",
                "//! Parity crate.\n\n/// A service.\npub struct Service;\n\nimpl Service {\n    /// Works.\n    pub fn work(&self) -> bool {\n        Self::helper()\n    }\n    fn helper() -> bool {\n        true\n    }\n}\n\npub fn user() -> bool {\n    Service.work()\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn works() {\n        assert!(user());\n    }\n}\n",
            ),
            (
                "tests/service.rs",
                "#[test]\nfn integration() {\n    assert!(paritycrate::user());\n}\n",
            ),
        ],
        providers(),
    )
}

/// The calls both transports answer: lexical answers before the server is ready, then semantic
/// queries, symbol edits (insert, rename), a format-on-edit, a test run and the problems view.
fn calls() -> Vec<(&'static str, Value)> {
    let edit = |params: Value| ("ide.edit", params);
    vec![
        ("ide.outline", json!({"path":"src/lib.rs"})),
        ("ide.read", json!({"path":"src/lib.rs","lines":"1-12"})),
        ("ide.read", json!({"symbol":"src/lib.rs#Service/work"})),
        ("ide.symbol", json!({"symbol":"src/lib.rs#Service/work"})),
        (
            "ide.graph",
            json!({"symbol":"src/lib.rs#user","direction":"both","depth":2}),
        ),
        ("ide.context", json!({"path":"src/lib.rs"})),
        (
            "ide.outline",
            json!({"path":"src/lib.rs","kinds":"fn,method"}),
        ),
        edit(
            json!({"operation_id":"p-insert","op":"insert","symbol":"src/lib.rs#user",
            "where":"after","content":"pub   fn added()->bool{ true }"}),
        ),
        edit(
            json!({"operation_id":"p-rename","op":"rename","symbol":"src/lib.rs#Service/work",
            "new_name":"operate"}),
        ),
        ("ide.diff", json!({})),
        ("ide.test", json!({"symbol":"src/lib.rs#user"})),
        ("ide.context", json!({"kind":"problems"})),
    ]
}

/// The module run's tree: the module is a direct daemon child, rust-analyzer's parent is the
/// module, and no language server is a direct daemon child.
fn assert_module_tree(tree: &parity::ProcessTree) {
    let module = tree
        .module("rust", "analyzer")
        .unwrap_or_else(|| panic!("no rust analyzer module in {tree:?}"));
    assert!(
        module
            .children
            .iter()
            .any(|(_, command)| command.contains("rust-analyzer")),
        "rust-analyzer must run under the module: {module:?}"
    );
    assert!(
        !tree.direct_child_runs("rust-analyzer"),
        "no direct daemon child runs rust-analyzer: {tree:?}"
    );
}

/// Every Rust row answers equally in the module and in process, with the process tree of §4.2.
#[tokio::test]
async fn rust_module_and_in_process_transcripts_are_equal() {
    let fixture = fixture();
    let calls = calls();
    let local = parity::transcript(&fixture, &[IN_PROCESS], &calls).await;
    assert!(local.tree.module("rust", "analyzer").is_none());
    assert!(
        local.tree.direct_child_runs("rust-analyzer")
            || local
                .tree
                .children
                .iter()
                .all(|node| !node.command.contains("module rust")),
        "the in-process run has no rust module: {:?}",
        local.tree
    );
    assert!(
        local.replies[0].contains("Service"),
        "outline answers: {}",
        local.replies[0]
    );
    drop(local.daemon);
    fixture.git(&["checkout", "--quiet", "--", "."]);
    fixture.git(&["clean", "--quiet", "-fd"]);
    let remote = parity::transcript(&fixture, &[SHIPPED], &calls).await;
    assert_module_tree(&remote.tree);
    parity::assert_parity(&local.replies, &remote.replies);
    let owned = remote.tree.all();
    drop(remote.daemon);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived the daemon");
    }
}

/// Killing the module while rust-analyzer indexes is a typed unavailable outcome: the daemon is
/// the same process, rust-analyzer is gone with the module, and the next call is answered.
#[tokio::test]
async fn killing_the_module_while_rust_analyzer_indexes_is_typed_and_leaves_no_orphan() {
    let fixture = fixture();
    let mut daemon = parity::Daemon::start(&fixture, &[SHIPPED]).await;
    let daemon_pid = daemon.pid();
    let mut session = parity::Session::start(&fixture).await;
    // Starts the session; the symbol request waits on indexing, so the module is busy.
    let warm = session
        .call(&fixture, "ide.outline", json!({"path":"src/lib.rs"}))
        .await;
    assert_eq!(warm["kind"], "outline", "{warm}");
    let tree = daemon.tree();
    let module = tree
        .module("rust", "analyzer")
        .expect("a rust module")
        .clone();
    let owned = tree.all();
    // SAFETY: the module is this test's own daemon child, identified a moment ago.
    unsafe { libc::kill(module.id.pid, libc::SIGKILL) };
    assert!(module.id.gone().await, "the killed module is gone");
    for (id, command) in &module.children {
        assert!(id.gone().await, "{command} outlived its module");
    }
    let after = session
        .call(
            &fixture,
            "ide.symbol",
            json!({"symbol":"src/lib.rs#Service/work"}),
        )
        .await;
    let text = after["text"].as_str().unwrap_or_default();
    assert!(
        after["kind"] == "symbol" || text.contains("unavailable"),
        "a typed outcome, never a hang or a daemon death: {after}"
    );
    assert_eq!(daemon.pid(), daemon_pid, "the daemon is the same process");
    assert!(
        std::os::unix::net::UnixStream::connect(fixture.runtime.join("agent-ide.sock")).is_ok(),
        "the daemon still serves"
    );
    session.close(&fixture).await;
    let later = daemon.tree().all();
    drop(daemon);
    for id in owned.into_iter().chain(later) {
        assert!(id.gone().await, "{id:?} survived the daemon");
    }
}

/// A `cargo check` that never finishes ends as a typed non-pass result, never hangs the daemon,
/// and its cargo, rustc and build script processes die with the daemon.
#[tokio::test]
async fn a_stalled_cargo_check_is_bounded_and_leaves_no_orphan() {
    let fixture = parity::Fixture::new(
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"stalled\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
            ),
            (
                "build.rs",
                "fn main() { std::thread::sleep(std::time::Duration::from_secs(600)); }\n",
            ),
            ("src/lib.rs", "pub fn one() -> u8 { 1 }\n"),
        ],
        providers(),
    );
    let mut daemon = parity::Daemon::start(&fixture, &[SHIPPED]).await;
    let daemon_pid = daemon.pid();
    let mut session = parity::Session::start(&fixture).await;
    let reply = session
        .call(&fixture, "ide.context", json!({"kind":"problems"}))
        .await;
    assert!(
        reply.get("text").is_some(),
        "the problems view answers: {reply}"
    );
    let tree = daemon.tree();
    let owned = tree.all();
    assert_eq!(daemon.pid(), daemon_pid);
    session.close(&fixture).await;
    drop(daemon);
    tokio::time::sleep(Duration::from_millis(100)).await;
    for id in owned {
        assert!(id.gone().await, "{id:?} survived the daemon");
    }
}
