//! QW-4: a dispatch journal line carries the closed context fields (version, host, role,
//! language, request form, request id, eligibility) — an explicit closed value when the call could
//! not name one — marks a degraded success only from the daemon's own mark, records a pending job's
//! completion through the same classifier, and never carries source text, paths, model-chosen
//! parameter names or other request values (the writer is process-global, so this lives in its own
//! test binary).

use std::time::Duration;

use agent_ide::assistance::facade::AssistanceTool;
use agent_ide::assistance::host_binding::HostKind;
use agent_ide::assistance::reply::{EditDiagnostics, PeerReply, ResultKind};
use agent_ide::changes::edit::{EditOutcome, EditResult};
use agent_ide::errorlog::{self, Role};
use agent_ide::telemetry::adapters::{self, DispatchContext};
use serde_json::json;

/// A completed reply carrying `text`.
fn complete(text: &str) -> PeerReply {
    PeerReply::Complete {
        kind: ResultKind::Outline,
        text: text.to_owned(),
        detail_ref: None,
        truncated: false,
        continuation: false,
    }
}

/// An edit reply with the given outcome and diagnostics.
fn edit(outcome: EditOutcome, diagnostics: EditDiagnostics) -> PeerReply {
    let source_ref = outcome.has_post_source().then(|| "after".to_owned());
    PeerReply::Edit {
        result: EditResult::new("op-1".into(), "src/lib.rs".into(), outcome, source_ref).unwrap(),
        diagnostics,
        note: None,
        operation: None,
    }
}

/// Logs calls and checks the fields, the explicit unknown values, the degraded outcome, the
/// completion records and the privacy invariant.
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
    let elapsed = Duration::from_millis(5);

    // 0: a well-formed read the daemon marked degraded (lexical fallback): eligible, degraded.
    let parameters = json!({"path":"src/secret_dir/lib.rs","lines":"1-3"});
    adapters::log_tool_reply(
        AssistanceTool::Read,
        &complete("ordinary answer"),
        elapsed,
        None,
        &DispatchContext {
            host: Some(HostKind::Claude),
            role: Some(Role::Reader),
            request: Some("call-7"),
            degraded: true,
            parameters: Some(&parameters),
            ..Default::default()
        },
    );
    // 1: ordinary file text that happens to contain the lexical markers is NOT degraded: only the
    // daemon's own mark decides.
    let parameters = json!({"path":"app.py"});
    adapters::log_tool_reply(
        AssistanceTool::Outline,
        &complete("outline: from source, exact (x)\nmode: lexical (y)"),
        elapsed,
        None,
        &DispatchContext {
            host: Some(HostKind::Codex),
            role: Some(Role::Writer),
            request: Some("call-8"),
            parameters: Some(&parameters),
            ..Default::default()
        },
    );
    // 2: a request the validator refuses, with no host, role or file named: not eligible, and
    // every field the call could not name is an explicit closed value.
    let parameters = json!({"caller_chosen_key":"SECRET_VALUE"});
    adapters::log_tool_reply(
        AssistanceTool::Read,
        &PeerReply::InvalidParameters {
            message: "SECRET_MESSAGE".to_owned(),
        },
        elapsed,
        None,
        &DispatchContext {
            request: Some("call-9"),
            parameters: Some(&parameters),
            ..Default::default()
        },
    );
    // 3: a file no registered language owns.
    let parameters = json!({"path":"notes.unknownext"});
    adapters::log_tool_reply(
        AssistanceTool::Outline,
        &complete("nothing"),
        elapsed,
        None,
        &DispatchContext {
            request: Some("call-10"),
            probe: Some("whois"),
            parameters: Some(&parameters),
            ..Default::default()
        },
    );
    // 4: an inspection names the call that queued the result it asks for, beside its own id.
    let parameters = json!({"detail_ref":"ref-1"});
    adapters::log_tool_reply(
        AssistanceTool::Inspect,
        &complete("later"),
        elapsed,
        Some("ref-1"),
        &DispatchContext {
            request: Some("call-11"),
            origin: Some("call-7"),
            delivered: Some(true),
            parameters: Some(&parameters),
            ..Default::default()
        },
    );

    // Completion records of jobs finished after their caller was told `pending`.
    adapters::log_pending_completion(
        AssistanceTool::Edit,
        &edit(EditOutcome::StaleSource, EditDiagnostics::Unknown {}),
        "job-refused",
        Some("call-12"),
        false,
    );
    adapters::log_pending_completion(
        AssistanceTool::Edit,
        &edit(EditOutcome::OutcomeUnknown, EditDiagnostics::Unknown {}),
        "job-unknown",
        Some("call-13"),
        false,
    );
    adapters::log_pending_completion(
        AssistanceTool::Edit,
        &edit(EditOutcome::Replaced, EditDiagnostics::Unknown {}),
        "job-weak",
        Some("call-14"),
        false,
    );
    adapters::log_pending_completion(
        AssistanceTool::Outline,
        &complete("fine"),
        "job-degraded",
        Some("call-15"),
        true,
    );
    adapters::log_pending_completion(
        AssistanceTool::Outline,
        &complete("fine"),
        "job-ok",
        Some("call-16"),
        false,
    );

    let events = errorlog::read_events(&dir);
    assert_eq!(events.len(), 10);
    let version = env!("CARGO_PKG_VERSION");

    assert_eq!(events[0].outcome, "degraded");
    assert_eq!(events[0].level, "warn");
    assert_eq!(events[0].version.as_deref(), Some(version));
    assert_eq!(events[0].host.as_deref(), Some("claude"));
    assert_eq!(events[0].role.as_deref(), Some("reader"));
    assert_eq!(events[0].language.as_deref(), Some("rust"));
    assert_eq!(events[0].form.as_deref(), Some("path+lines"));
    assert_eq!(events[0].request.as_deref(), Some("call-7"));
    assert_eq!(events[0].eligible, Some(true));

    assert_eq!(
        events[1].outcome, "completed",
        "ordinary text is never degraded"
    );
    assert_eq!(events[1].host.as_deref(), Some("codex"));
    assert_eq!(events[1].role.as_deref(), Some("writer"));
    assert_eq!(events[1].language.as_deref(), Some("python"));
    assert_eq!(events[1].form.as_deref(), Some("path"));

    assert_eq!(events[2].outcome, "invalid");
    assert_eq!(events[2].eligible, Some(false));
    assert_eq!(
        (
            events[2].host.as_deref(),
            events[2].role.as_deref(),
            events[2].language.as_deref(),
            events[2].form.as_deref(),
            events[2].version.as_deref(),
        ),
        (
            Some("unknown"),
            Some("none"),
            Some("none"),
            Some("none"),
            Some(version)
        )
    );

    assert_eq!(events[3].language.as_deref(), Some("unknown"));
    assert_eq!(events[3].host.as_deref(), Some("unknown"));
    assert_eq!(events[3].probe.as_deref(), Some("whois"));

    assert_eq!(events[4].method, "inspect");
    assert_eq!(events[4].correlation.as_deref(), Some("ref-1"));
    assert_eq!(events[4].request.as_deref(), Some("call-11"));
    assert_eq!(events[4].origin.as_deref(), Some("call-7"));
    assert_eq!(events[4].delivered, Some(true));
    assert_eq!(
        events[0].delivered, None,
        "a call that retrieves nothing carries no delivery flag"
    );

    // The completion records: a refusal and an unknown outcome are not successes; a success with
    // unknown diagnostics or a degraded answer is degraded; a plain answer is completed.
    let record = |index: usize| {
        let event = &events[5 + index];
        assert_eq!(event.detail.as_deref(), Some("pending_completion"));
        assert!(
            event.eligible.is_none(),
            "a completion record is not a request line"
        );
        event
    };
    assert_eq!(record(0).outcome, "invalid");
    assert_eq!(record(0).reason.as_deref(), Some("stale_source"));
    assert_eq!(record(0).correlation.as_deref(), Some("job-refused"));
    assert_eq!(record(0).request.as_deref(), Some("call-12"));
    assert_eq!(record(1).outcome, "incomplete");
    assert_eq!(record(1).reason.as_deref(), Some("edit_outcome_unknown"));
    assert_eq!(record(2).outcome, "degraded");
    assert_eq!(record(3).outcome, "degraded");
    assert_eq!(record(4).outcome, "completed");

    let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
    for forbidden in [
        "ordinary answer",
        "mode: lexical",
        "secret_dir",
        "caller_chosen_key",
        "SECRET_VALUE",
        "SECRET_MESSAGE",
        "lib.rs",
        "notes.unknownext",
    ] {
        assert!(
            !raw.contains(forbidden),
            "the journal must not carry {forbidden:?}: {raw}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
