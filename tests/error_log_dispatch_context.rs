//! QW-4: a dispatch journal line carries the closed context fields (version, host, role,
//! language, request form, request id, eligibility), marks a degraded success, and never carries
//! source text, paths, model-chosen parameter names or other request values (the writer is
//! process-global, so this lives in its own test binary).

use std::time::Duration;

use agent_ide::assistance::facade::AssistanceTool;
use agent_ide::assistance::host_binding::HostKind;
use agent_ide::assistance::reply::{PeerReply, ResultKind};
use agent_ide::errorlog::{self, Role};
use agent_ide::telemetry::adapters::{self, DispatchContext};
use serde_json::json;

/// Logs three calls and checks the fields, the degraded outcome and the privacy invariant.
#[test]
fn dispatch_lines_carry_closed_context_and_never_request_values() {
    agent_ide::languages::install();
    let key = format!(
        "{:016x}",
        std::process::id() as u64 * 6007 + 0xdef0_0000_0000
    );
    errorlog::init_repository(&key);
    let dir = errorlog::log_root().unwrap().join(&key);
    let _ = std::fs::remove_dir_all(&dir);

    let complete = |text: &str| PeerReply::Complete {
        kind: ResultKind::Outline,
        text: text.to_owned(),
        detail_ref: None,
        truncated: false,
        continuation: false,
    };
    let elapsed = Duration::from_millis(5);

    // A well-formed read answered lexically: eligible, a degraded success.
    let parameters = json!({"path":"src/secret_dir/lib.rs","lines":"1-3"});
    adapters::log_tool_reply(
        AssistanceTool::Read,
        &complete(
            "SOURCE_TEXT_SENTINEL\noutline: from source, exact (rust-analyzer unavailable; no need to repeat)",
        ),
        elapsed,
        None,
        &DispatchContext {
            host: Some(HostKind::Claude),
            role: Some(Role::Reader),
            request: Some("req-7"),
            parameters: Some(&parameters),
        },
    );
    // A well-formed outline answered by the server: a plain success.
    let parameters = json!({"path":"app.py"});
    adapters::log_tool_reply(
        AssistanceTool::Outline,
        &complete("outline of app.py"),
        elapsed,
        None,
        &DispatchContext {
            host: Some(HostKind::Codex),
            role: Some(Role::Writer),
            request: Some("req-8"),
            parameters: Some(&parameters),
        },
    );
    // A request the validator refuses: not eligible, and neither the model-chosen name nor any
    // value appears.
    let parameters = json!({"path":"src/secret_dir/lib.rs","caller_chosen_key":"SECRET_VALUE"});
    adapters::log_tool_reply(
        AssistanceTool::Read,
        &PeerReply::InvalidParameters {
            message: "SECRET_MESSAGE".to_owned(),
        },
        elapsed,
        None,
        &DispatchContext {
            parameters: Some(&parameters),
            ..Default::default()
        },
    );

    let events = errorlog::read_events(&dir);
    assert_eq!(events.len(), 3);
    let version = env!("CARGO_PKG_VERSION");

    assert_eq!(events[0].outcome, "degraded");
    assert_eq!(events[0].level, "warn");
    assert_eq!(events[0].version.as_deref(), Some(version));
    assert_eq!(events[0].host.as_deref(), Some("claude"));
    assert_eq!(events[0].role.as_deref(), Some("reader"));
    assert_eq!(events[0].language.as_deref(), Some("rust"));
    assert_eq!(events[0].form.as_deref(), Some("path+lines"));
    assert_eq!(events[0].request.as_deref(), Some("req-7"));
    assert_eq!(events[0].eligible, Some(true));

    assert_eq!(events[1].outcome, "completed");
    assert_eq!(events[1].host.as_deref(), Some("codex"));
    assert_eq!(events[1].role.as_deref(), Some("writer"));
    assert_eq!(events[1].language.as_deref(), Some("python"));
    assert_eq!(events[1].form.as_deref(), Some("path"));

    assert_eq!(events[2].outcome, "invalid");
    assert_eq!(events[2].eligible, Some(false));
    assert_eq!(events[2].form.as_deref(), Some("path"));

    let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    for forbidden in [
        "SOURCE_TEXT_SENTINEL",
        "secret_dir",
        "caller_chosen_key",
        "SECRET_VALUE",
        "SECRET_MESSAGE",
        "lib.rs",
    ] {
        assert!(
            !raw.contains(forbidden),
            "the journal must not carry {forbidden:?}: {raw}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
