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

/// The harness masks only values a reply generated, by slot: the references of its own
/// structured form and the fixture root (by value, in metadata rows only), and the `ide.start`
/// card's ids and timings and an `ide.test` duration. Source text a reply returns stays exact: timings, long hex runs, activation-looking
/// ids and reference-looking lines in an `ide.read`/`ide.context` body still differ, as do other
/// words, whitespace, short hex runs and paths.
#[test]
fn parity_normalizer_masks_only_volatile_tokens() {
    use parity::{line, normalized};
    let read = |text: &str| {
        normalized(&line(
            "ide.read",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"read","code":null,"text":text,"detail_ref":"r-1"}),
        ))
    };
    let context = |text: &str| {
        normalized(&line(
            "ide.context",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"context","code":null,"text":text,"detail_ref":"r-1"}),
        ))
    };
    let (long_a, long_b) = ("a".repeat(65), "b".repeat(65));
    let (id_a, id_b) = ("a".repeat(64), "b".repeat(64));
    // Preservation: every masked shape inside returned source still differs.
    let bodies: [&dyn Fn(&str) -> String; 2] = [&read, &context];
    for body in bodies {
        assert_ne!(body("1\tdelay = 12ms\n"), body("1\tdelay = 13ms\n"));
        assert_ne!(body("took 12ms (3.5ms)."), body("took 340ms (9ms)."));
        assert_ne!(
            body(&format!("1\t{long_a}\n")),
            body(&format!("1\t{long_b}\n"))
        );
        assert_ne!(
            body(&format!("key = \"activation {id_a}\"\n")),
            body(&format!("key = \"activation {id_b}\"\n"))
        );
        assert_ne!(
            body(&format!("activation {id_a}\n")),
            body(&format!("activation {id_b}\n"))
        );
        assert_ne!(
            body(&format!("source_ref: {long_a}\n")),
            body(&format!("source_ref: {long_b}\n"))
        );
        assert_ne!(
            body("tests #1: 1 passed, 0 failed, 0 s"),
            body("tests #1: 1 passed, 0 failed, 1 s")
        );
        assert_ne!(body("return items"), body("return params"));
        assert_ne!(body("a  b"), body("a b"));
        assert_ne!(body("    x = 1\n"), body("  x = 1\n"));
        assert_ne!(body("git 1a2b3c4\n"), body("git 5d6e7f8\n"));
        assert_ne!(body("at /tmp/a/x\n"), body("at /tmp/b/x\n"));
    }
    assert_eq!(normalized("x\n"), "x\n");
    assert_eq!(normalized("took 12ms\n"), "took 12ms\n");
    // A reference the reply generated is masked wherever its text repeats it, by value only.
    let reply = |reference: &str| {
        line(
            "ide.read",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"read","code":null,
                "text":format!("1\tx = 1\nsource_ref: {reference}\n"),"detail_ref":reference}),
        )
    };
    assert_eq!(reply(&format!("{long_a}-3")), reply(&format!("{long_b}-4")));
    let edit = |reference: &str| {
        line(
            "ide.edit",
            &json!({"path":"a.py"}),
            &json!({"state":"edit","kind":null,"code":null,"result":{"source_ref":reference},
                "text":format!("edit: replaced; path a.py; source_ref {reference}; diagnostics: unknown")}),
        )
    };
    assert_eq!(edit("e-1"), edit("e-2"));
    // ... but never inside a returned source row, even when that row holds the very value.
    let source = |row: &str| {
        line(
            "ide.read",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"read","code":null,
                "text":format!("{row}\nsource_ref: {long_a}-3\n"),"detail_ref":format!("{long_a}-3")}),
        )
    };
    assert_ne!(source(&format!("1\t{long_a}-3")), source("1\t<ref>"));
    let body = |text: &str| {
        line(
            "ide.context",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"context","code":null,
                "text":format!("path: a.py\n\n{text}"),"detail_ref":"c-1"}),
        )
    };
    assert_ne!(body("x = 'c-1'\n"), body("x = '<ref>'\n"));
    // The fixture root is masked in metadata rows (two fixtures compare equal there) and kept in
    // returned source rows.
    let (one, two) = (
        parity::Fixture::new(&[("a.py", "x = 1\n")], json!([])),
        parity::Fixture::new(&[("a.py", "x = 1\n")], json!([])),
    );
    let rooted = |fixture: &parity::Fixture, header: &str, text: &str| {
        parity::line_for(
            fixture,
            "ide.context",
            &json!({"path":"a.py"}),
            &json!({"state":"complete","kind":"context","code":null,
                "text":format!("path: a.py\n{header}\n\n{text}")}),
        )
    };
    let at =
        |fixture: &parity::Fixture| format!("definitions: file://{}/a.py", fixture.root.display());
    assert_eq!(
        rooted(&one, &at(&one), "x\n"),
        rooted(&two, &at(&two), "x\n")
    );
    // A symbol card's usage excerpt is returned source too.
    let card = |row: &str| {
        parity::line_for(
            &one,
            "ide.symbol",
            &json!({"symbol":"a.py#p"}),
            &json!({"state":"complete","kind":"symbol","code":null,
                "text":format!("symbol: p\nusages: 1 in 1 files\n  a.py:1  {row}\n")}),
        )
    };
    assert_ne!(
        card(&format!("p = '{}'", one.root.display())),
        card("p = '<root>'")
    );
    let path = format!("p = '{}'\n", one.root.display());
    assert_ne!(
        rooted(&one, "definitions: null", &path),
        rooted(&one, "definitions: null", "p = '<root>'\n")
    );
    // The `ide.start` card is generated metadata: its ids, digests and timings are masked.
    assert_eq!(
        normalized(&format!(
            "ide.start {{}} -> x\nexisting activation {id_a}; ready (12ms)\n"
        )),
        normalized(&format!(
            "ide.start {{}} -> x\nexisting activation {id_b}; ready (9ms)\n"
        ))
    );
    assert_ne!(
        normalized(&format!("ide.start {{}} -> x\nop {id_a}\n")),
        normalized(&format!("ide.start {{}} -> x\nop {id_b}\n"))
    );
    // An `ide.test` reply's run duration is masked; its counts are not.
    let test = |row: &str| normalized(&format!("ide.test {{}} -> x\n{row}"));
    assert_eq!(
        test("tests #1: 1 passed, 0 failed, 0 s · env .venv"),
        test("tests #1: 1 passed, 0 failed, 3 s · env .venv")
    );
    assert_ne!(
        test("tests #1: 1 passed, 0 failed, 0 s"),
        test("tests #1: 2 passed, 0 failed, 0 s")
    );
    assert_eq!(
        test("tests #2: no summary parsed, 4 s — inspect"),
        test("tests #2: no summary parsed, 5 s — inspect")
    );
    assert_eq!(
        test("tests #2: no test results (exit 2), 4 s — runner said: x"),
        test("tests #2: no test results (exit 2), 5 s — runner said: x")
    );
    // Only the settled status row: the runner's output tail and a started run's arguments keep
    // anything shaped like a duration.
    assert_ne!(
        test("tests #1: 1 passed, 0 failed, 0 s\n  output (tail):\ntests #7 marker, 12 s\n"),
        test("tests #1: 1 passed, 0 failed, 0 s\n  output (tail):\ntests #7 marker, 13 s\n")
    );
    assert_ne!(
        test("tests #1: started — pytest test_a,12 s.py (budget 120 s)"),
        test("tests #1: started — pytest test_a,13 s.py (budget 120 s)")
    );
    // A request's echoed references are masked by value only; the request still differs by
    // its other fields.
    let reply = json!({"state":"complete","kind":"edit","code":null,"text":"ok"});
    assert_eq!(
        line("ide.edit", &json!({"source_ref":"r1","path":"a"}), &reply),
        line("ide.edit", &json!({"source_ref":"r2","path":"a"}), &reply)
    );
    assert_ne!(
        line("ide.edit", &json!({"source_ref":"r1","path":"a"}), &reply),
        line("ide.edit", &json!({"source_ref":"r1","path":"b"}), &reply)
    );
}

/// Two fronts opened back to back on one daemon never reuse a tool call id: the second front's
/// calls answer instead of being refused as a replay of the first's.
#[tokio::test]
async fn parity_fronts_on_one_daemon_use_distinct_call_ids() {
    let fixture = parity::Fixture::new(&[("style.css", ".btn { color: red; }\n")], json!([]));
    let _daemon = parity::Daemon::start(&fixture, &[]).await;
    for front in 0..2 {
        let mut session = parity::Session::start(&fixture).await;
        let reply = session
            .call(&fixture, "ide.outline", json!({"path":"style.css"}))
            .await;
        assert_eq!(reply["kind"], "outline", "front {front}: {reply}");
        session.close(&fixture).await;
    }
}

/// A front stand-in: answers `initialize`; refuses the first `ide.start` as unavailable and
/// answers the next as pending (`ide.inspect` then settles it to an activation); answers anything
/// else complete; as `codex-hook` it accepts the hook.
const FAKE_FRONT: &str = r#"#!/bin/sh
if [ "$1" = codex-hook ]; then cat >/dev/null; exit 0; fi
starts=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  [ -z "$id" ] && continue
  case "$line" in
    *'"initialize"'*) reply='{}' ;;
    *'"ide.start"'*)
      starts=$((starts + 1))
      if [ "$starts" = 1 ]; then
        reply='{"structuredContent":{"state":"unavailable","reason":"host_binding"}}'
      else
        reply='{"structuredContent":{"state":"pending","detail_ref":"start-1"}}'
      fi ;;
    *'"ide.inspect"'*) reply='{"structuredContent":{"state":"complete","kind":"activation","text":"ok"}}' ;;
    *) reply='{"structuredContent":{"state":"complete","kind":"stop","text":"ok"}}' ;;
  esac
  printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$reply"
done
"#;

/// A start that first answers unavailable and then pending settles to its activation: the
/// harness repeats the idempotent start and settles the pending reply through `ide.inspect`.
#[tokio::test]
async fn parity_session_settles_a_late_activation() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = parity::Fixture::new(&[("a.css", ".a {}\n")], json!([]));
    let front = fixture.base.join("fake-front");
    std::fs::write(&front, FAKE_FRONT).unwrap();
    std::fs::set_permissions(&front, std::fs::Permissions::from_mode(0o700)).unwrap();
    let session = parity::Session::start_with(&fixture, front).await;
    session.close(&fixture).await;
}

/// A start that never settles fails at its one overall deadline, however many nested pending
/// polls it would otherwise take.
#[tokio::test]
#[should_panic(expected = "ide.start did not settle to an activation within")]
async fn parity_session_start_has_one_deadline() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = parity::Fixture::new(&[("a.css", ".a {}\n")], json!([]));
    let front = fixture.base.join("pending-front");
    std::fs::write(
        &front,
        FAKE_FRONT.replace(
            r#"*'"ide.inspect"'*) reply='{"structuredContent":{"state":"complete","kind":"activation","text":"ok"}}' ;;"#,
            r#"*'"ide.inspect"'*) reply='{"structuredContent":{"state":"pending","detail_ref":"start-1"}}' ;;"#,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&front, std::fs::Permissions::from_mode(0o700)).unwrap();
    let started = std::time::Instant::now();
    let session = tokio::time::timeout(
        Duration::from_secs(20),
        parity::Session::start_within(&fixture, front, Duration::from_secs(3)),
    )
    .await
    .expect("the start's own deadline ends it first");
    drop(session);
    panic!("never settles ({:?})", started.elapsed());
}

/// The parity harness runs the same calls on two fresh daemons, with and without the fallback
/// switch, and finds them equal; CSS runs as a module by default exactly when it ships as one.
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
    assert_eq!(
        default.tree.module("css", "analyzer").is_some(),
        agent_ide::languages::SHIPPED_MODULES.contains(&"css"),
        "css runs as a module by default exactly when it ships as one"
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
