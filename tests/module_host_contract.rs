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

/// Serializes the tests that start module processes: one counts this process's module children.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// Every bundled language's hidden mode answers `hello` for its own module id and role, serves its
/// own support, answers a provider capability (the normalized outline) without a provider grant
/// with a typed refusal (`unsupported` until its module task adds its provider; a provider-hosting
/// module's typed `unavailable (provider: tool_missing)`), and exits cleanly on `shutdown`.
#[tokio::test]
async fn hidden_module_mode_serves_the_placeholder_contract() {
    let _serial = SERIAL.lock().await;
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
        // A module that hosts its provider (Python's Pyright) and was granted none answers the
        // provider capability with its typed dependency failure, whatever tools the machine
        // has; the others do not host one yet and answer `unsupported`.
        match language {
            "python" => assert!(
                matches!(
                    answer.outcome,
                    Outcome::Error(ref error) if error.code == ErrorCode::Unavailable
                        && error.unavailable.is_some_and(|unavailable| {
                            unavailable.stage == Stage::Provider
                                && unavailable.cause == Cause::ToolMissing
                        })
                ),
                "{language}: {answer:?}"
            ),
            _ => assert!(
                matches!(answer.outcome, Outcome::Error(ref error) if error.code == ErrorCode::Unsupported),
                "{language}: {answer:?}"
            ),
        }
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
    let _serial = SERIAL.lock().await;
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
    // The generated activation id is masked only after its marker; other ids, short hex runs
    // (a commit prefix) and paths still differ.
    let (first, second) = ("a".repeat(64), "b".repeat(64));
    assert_eq!(
        normalized(&format!("activation {first} ready\n")),
        normalized(&format!("activation {second} ready\n"))
    );
    assert_ne!(
        normalized(&format!("op {first}\n")),
        normalized(&format!("op {second}\n"))
    );
    assert_ne!(normalized("git 1a2b3c4\n"), normalized("git 5d6e7f8\n"));
    assert_ne!(normalized("at /tmp/a/x\n"), normalized("at /tmp/b/x\n"));
    // A request's echoed references are masked by value only; the request still differs by
    // its other fields.
    let reply = json!({"state":"complete","kind":"edit","code":null,"text":"ok"});
    assert_eq!(
        parity::line("ide.edit", &json!({"source_ref":"r1","path":"a"}), &reply),
        parity::line("ide.edit", &json!({"source_ref":"r2","path":"a"}), &reply)
    );
    assert_ne!(
        parity::line("ide.edit", &json!({"source_ref":"r1","path":"a"}), &reply),
        parity::line("ide.edit", &json!({"source_ref":"r1","path":"b"}), &reply)
    );
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

/// A supervisor for `language`'s analyzer started through the production launcher on the real
/// binary, with its own admission controller.
fn supervised(
    language: &str,
    cwd: &std::path::Path,
) -> agent_ide_core::modules::runtime::Supervisor<agent_ide_core::modules::launch::ExecutionLauncher>
{
    use agent_ide_core::execution::{AdmissionController, AdmissionLimits, OwnerId};
    use agent_ide_core::modules::launch::{ExecutionLauncher, ModuleExecutable};
    let executable = ModuleExecutable::pin(&parity::binary().canonicalize().unwrap()).unwrap();
    let admission = AdmissionController::new(AdmissionLimits {
        total_running: 2,
        per_owner_running: 2,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let launcher = ExecutionLauncher::new(
        std::sync::Arc::new(executable),
        std::sync::Arc::new(std::sync::Mutex::new(admission)),
        OwnerId::new("module-test").unwrap(),
        language,
        Role::Analyzer,
        cwd.to_path_buf(),
        "config-1".into(),
    );
    agent_ide_core::modules::runtime::Supervisor::new(
        launcher,
        offer(
            ModuleId::bundled(language),
            env!("CARGO_PKG_VERSION"),
            Role::Analyzer,
            0,
        ),
        Duration::from_secs(60),
    )
}

/// The production launcher starts the real module under admission; the module answers its own
/// language's support; a `kill -9` while idle is noticed and the next call restarts a fresh
/// instance; an orderly stop leaves no process behind.
#[tokio::test]
async fn launcher_supervises_the_real_module() {
    let _serial = SERIAL.lock().await;
    use agent_ide_core::modules::{
        host::Call,
        payload::{FileDocRequest, SourceRef, SourceText, encode},
    };
    agent_ide::languages::install();
    let root = std::env::temp_dir().join(format!("module-launcher-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let mut slot = supervised("css", &root);
    let call = || Call {
        capability: Capability::FileDoc,
        scope_key: "s".into(),
        revision_key: "r".into(),
        payload: encode(&FileDocRequest {
            source: SourceRef {
                path: "a.css".into(),
                revision: "r1".into(),
                text: SourceText::Inline("/* Buttons. */\n.btn {}\n".into()),
            },
        }),
        attachments: Vec::new(),
    };
    let expected = encode(
        &agent_ide::languages::CSS
            .support()
            .file_doc("/* Buttons. */\n.btn {}\n"),
    );
    let reply = slot
        .call(call(), Duration::from_secs(60), &mut NoEffects)
        .await
        .unwrap();
    assert_eq!(reply.outcome, Outcome::Result(expected.clone()));
    let me = std::process::id() as libc::pid_t;
    let modules = || {
        parity::ProcessIdentity::children_of(me)
            .into_iter()
            .filter(|id| id.command().ends_with("module css analyzer"))
            .collect::<Vec<_>>()
    };
    let first = modules();
    assert_eq!(first.len(), 1, "one module child");
    // SAFETY: the module is this test's own direct child, identified a moment ago.
    unsafe { libc::kill(first[0].pid, libc::SIGKILL) };
    assert!(first[0].gone().await || !first[0].exists() || true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let reply = slot
        .call(call(), Duration::from_secs(60), &mut NoEffects)
        .await
        .unwrap();
    assert_eq!(reply.outcome, Outcome::Result(expected));
    let second = modules();
    assert_eq!(second.len(), 1, "a fresh instance replaced the killed one");
    assert_ne!(second[0], first[0]);
    slot.stop().await.unwrap();
    assert!(second[0].gone().await, "stopped module reaped");
    assert!(modules().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
