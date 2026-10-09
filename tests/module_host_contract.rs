//! Bundled language modules: the hidden `agent-ide module <language> <role>` mode speaks
//! `bundled-module/0`, and the shared parity-transcript harness drives a real daemon.

#[path = "support/parity.rs"]
mod parity;

use std::{process::Stdio, time::Duration};

use agent_ide_core::modules::{
    contract::{Capability, Cause, ErrorCode, ModuleId, Outcome, Role, Stage},
    fake::{offer, sample_call},
    host::{HostChannel, NoEffects},
};
use serde_json::json;
use tokio::process::Command;

/// Starts `agent-ide module <language> <role>` with piped stdio.
fn module_process(language: &str, role: &str) -> tokio::process::Child {
    Command::new(parity::binary())
        .args(["module", language, role])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// Every bundled language's hidden mode answers `hello` for its own module id and role, declares
/// every capability unsupported until its module task fills it in, answers a call with a typed
/// `unsupported` error, and exits cleanly on `shutdown`.
#[tokio::test]
async fn hidden_module_mode_serves_the_placeholder_contract() {
    agent_ide::languages::install();
    for language in ["python", "rust", "typescript", "html", "css"] {
        let mut child = module_process(language, "analyzer");
        let (input, output) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let id = ModuleId::bundled(language);
        let (mut channel, reply) = HostChannel::open(
            input,
            output,
            offer(id, env!("CARGO_PKG_VERSION"), Role::Analyzer, 1),
            Duration::from_secs(10),
        )
        .await
        .unwrap_or_else(|error| panic!("{language}: {error}"));
        assert_eq!(
            reply.capabilities.len(),
            Capability::ALL.len(),
            "{language}"
        );
        let answer = channel
            .call(
                sample_call(Capability::Outline),
                Duration::from_secs(10),
                &mut NoEffects,
            )
            .await
            .unwrap();
        assert!(
            matches!(answer.outcome, Outcome::Error(ref error) if error.code == ErrorCode::Unsupported),
            "{language}: {answer:?}"
        );
        channel.shutdown().await;
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{language} exits cleanly on shutdown");
    }
}

/// A role or version mismatch is refused at `hello` (`incompatible`); an unknown language or
/// role exits with usage status 2 before reading anything.
#[tokio::test]
async fn hidden_module_mode_refuses_mismatches() {
    agent_ide::languages::install();
    for (role, version) in [
        (Role::Checker, env!("CARGO_PKG_VERSION")),
        (Role::Analyzer, "0.0.1"),
    ] {
        let mut child = module_process("css", "analyzer");
        let (input, output) = (child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let error = HostChannel::open(
            input,
            output,
            offer(ModuleId::bundled("css"), version, role, 1),
            Duration::from_secs(10),
        )
        .await
        .err()
        .expect("a refused hello");
        assert_eq!(
            (error.stage, error.cause),
            (Stage::Hello, Cause::Incompatible)
        );
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
    for (language, role) in [("cobol", "analyzer"), ("css", "printer")] {
        let status = module_process(language, role).wait().await.unwrap();
        assert_eq!(status.code(), Some(2), "{language} {role}");
    }
}

/// The normalizer masks only digests and numeric timings: real words ending in `ms`, other
/// words and whitespace differences still compare unequal.
#[test]
fn parity_normalizer_masks_only_volatile_tokens() {
    use parity::normalized;
    assert_ne!(normalized("return items"), normalized("return params"));
    assert_ne!(normalized("a  b"), normalized("a b"));
    assert_ne!(normalized("    x = 1\n"), normalized("  x = 1\n"));
    assert_eq!(
        normalized("took 12ms (3.5ms)."),
        normalized("took 340ms (9ms).")
    );
    let (one, two) = ("a".repeat(65), "b".repeat(65));
    assert_eq!(
        normalized(&format!("ref {one}\n")),
        normalized(&format!("ref {two}\n"))
    );
    assert_eq!(normalized("x\n"), "x\n");
}

/// The parity harness runs the same calls on two fresh daemons, with and without the fallback
/// switch, and finds them equal; no module process runs yet (every language is in process).
#[tokio::test]
async fn parity_harness_compares_two_daemon_runs() {
    let fixture = parity::Fixture::new(
        &[
            ("style.css", ".btn { color: red; }\n#main { margin: 0; }\n"),
            (
                "index.html",
                "<main id=\"main\"><button class=\"btn\">go</button></main>\n",
            ),
        ],
        json!([]),
    );
    let calls = [
        ("ide.outline", json!({"path":"style.css"})),
        ("ide.read", json!({"path":"index.html"})),
        ("ide.symbol", json!({"symbol":".btn"})),
    ];
    let in_process = parity::transcript(
        &fixture,
        &[(parity::LANGUAGE_MODE, "css=in_process,html=in_process")],
        &calls,
    )
    .await;
    assert!(in_process.tree.module("css", "analyzer").is_none());
    drop(in_process.daemon);
    let default = parity::transcript(&fixture, &[], &calls).await;
    assert!(
        default.tree.module("css", "analyzer").is_none(),
        "no module runs before its language ships"
    );
    assert!(
        in_process.replies[0].contains(".btn"),
        "{}",
        in_process.replies[0]
    );
    parity::assert_parity(&in_process.replies, &default.replies);
    let owned = default.tree.all();
    drop(default.daemon);
    for id in owned {
        assert!(id.gone().await, "{id:?} survived the daemon");
    }
}
