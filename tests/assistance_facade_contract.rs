//! Contract checks for the bounded six-tool Assistance facade and fail-open feedback core.

use std::{fs, path::PathBuf};

use agent_ide::{
    assistance::facade::{
        AssistanceFacade, AssistanceTool, FacadeOutcome, FeedbackDelta, FeedbackLedger,
        FeedbackRecord, FeedbackState, HookIngressOutcome, TrustedTransport, render_hook_context,
        submit_inactive_hook, tool_schemas, validate_call,
    },
    assistance::host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session,
        parse_claude_hook_event, parse_hook_event,
    },
    workspace::authority::{ActivationRequest, AuthorityRegistry, WorktreeRef},
};
use serde_json::json;

/// Creates a private missing runtime location without causing the facade to create it.
fn missing_runtime() -> PathBuf {
    std::env::temp_dir().join(format!("agent-ide-facade-missing-{}", std::process::id()))
}

/// Creates trusted-looking test-only transport correlations outside model parameters.
fn host() -> TrustedTransport {
    TrustedTransport::from_host_ingress("request", "correlation", "attachment").unwrap()
}

/// Creates a valid test candidate using the pre-existing trusted-ingress parser contract.
fn candidate() -> agent_ide::assistance::host_binding::CandidateInvocation {
    parse_candidate(
        json!({
            "threadId": "actor",
            "callId": "call",
            "x-codex-turn-metadata": {"turn": "bounded"}
        })
        .as_object()
        .unwrap(),
    )
    .unwrap()
}

/// Creates the exact pre-hook needed to establish one host binding in the stop ordering scenario.
fn pre_hook() -> agent_ide::assistance::host_binding::HookEvent {
    parse_hook_event(
        br#"{"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call"}"#,
    )
    .unwrap()
}

#[test]
fn discovery_is_static_and_contains_exactly_six_current_methods() {
    let schemas = tool_schemas();
    assert_eq!(schemas.len(), 6);
    assert!(schemas.iter().map(|schema| schema.name).eq([
        "ide.start",
        "ide.context",
        "ide.diff",
        "ide.inspect",
        "ide.stop",
        "ide.edit"
    ]));
    assert!(
        schemas
            .iter()
            .all(|schema| schema.input_schema["additionalProperties"] == false)
    );
}

#[test]
fn validation_rejects_unknown_identity_and_requires_stable_operation_and_detail_ids() {
    assert!(validate_call(AssistanceTool::Start, json!({"activation_id":"activate-1"})).is_ok());
    assert!(validate_call(AssistanceTool::Inspect, json!({"detail_ref":"detail-1"})).is_ok());
    assert!(validate_call(AssistanceTool::Start, json!({"actor_id":"forged"})).is_err());
    assert!(validate_call(AssistanceTool::Stop, json!({"authority":"forged"})).is_err());
    assert!(validate_call(AssistanceTool::Inspect, json!({})).is_err());
}

#[tokio::test]
async fn unavailable_facade_and_hook_are_connect_only_and_fail_open() {
    let runtime = missing_runtime();
    let _ = fs::remove_dir_all(&runtime);
    let facade = AssistanceFacade::new(runtime.clone());
    assert_eq!(
        facade
            .dispatch(
                &host(),
                AssistanceTool::Context,
                json!({"path":"src/main.rs"})
            )
            .await,
        FacadeOutcome::Unavailable
    );
    assert_eq!(
        submit_inactive_hook(
            &runtime,
            &host(),
            br#"{"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call"}"#,
        )
        .await,
        HookIngressOutcome::Unavailable
    );
    assert!(!runtime.exists());
}

#[test]
fn stop_revokes_binding_before_workspace_and_old_expected_stamp_cannot_revoke_newer() {
    let mut bindings = HostBindingGuard::default();
    let channel = parse_channel_session(b"attachment").unwrap();
    assert!(matches!(
        bindings.observe_hook(pre_hook(), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) = bindings.establish_start(candidate(), channel)
    else {
        panic!("matching pre-hook must establish the test binding");
    };
    let active = bindings.consume_active(invocation.binding_ref()).unwrap();
    let worktree = WorktreeRef::from_discovery(
        PathBuf::from("/private/tmp/worktree"),
        PathBuf::from("/private/tmp/worktree"),
        PathBuf::from(".git"),
        1,
    )
    .unwrap();
    let mut authorities = AuthorityRegistry::default();
    let stamp = authorities
        .activate(ActivationRequest::new("activate-1", invocation, active, worktree).unwrap())
        .unwrap();
    let revoked = agent_ide::assistance::facade::stop_binding_then_revoke(
        &mut bindings,
        &mut authorities,
        &stamp,
    )
    .unwrap();
    assert_eq!(revoked.old_epoch(), stamp.epoch());
    assert!(bindings.check_active(stamp.binding()).is_err());
    assert!(
        agent_ide::assistance::facade::stop_binding_then_revoke(
            &mut bindings,
            &mut authorities,
            &stamp,
        )
        .is_err()
    );
}

#[test]
fn feedback_deduplicates_rechecks_and_suppresses_after_stop() {
    let delta = FeedbackDelta::new(
        "one new fact",
        "workspace observation",
        "inspect detail",
        "current",
        Some("detail-1".into()),
    )
    .unwrap();
    let mut feedback = FeedbackLedger::default();
    assert_eq!(
        feedback.record("authority-1", "source-1", &delta),
        FeedbackRecord::Pending
    );
    assert_eq!(
        feedback.record("authority-1", "source-1", &delta),
        FeedbackRecord::Deduplicated
    );
    assert_eq!(
        feedback.prepare_delivery("authority-1", "source-1", "one new fact", true, true),
        Some(FeedbackState::Submitted)
    );
    assert_eq!(
        feedback.mark_delivery_unknown("authority-1", "source-1", "one new fact"),
        Some(FeedbackState::DeliveryUnknown)
    );
    let stale =
        FeedbackDelta::new("stale fact", "old version", "none", "superseded", None).unwrap();
    assert_eq!(
        feedback.record("authority-1", "source-old", &stale),
        FeedbackRecord::Pending
    );
    assert_eq!(
        feedback.prepare_delivery("authority-1", "source-old", "stale fact", true, false),
        Some(FeedbackState::Superseded)
    );
    feedback.stop_authority("authority-1");
    assert_eq!(
        feedback.record("authority-1", "source-2", &delta),
        FeedbackRecord::Suppressed
    );
}

/// Emits only bounded post-hook JSON context and never emits context from a pre-hook.
#[test]
fn host_feedback_output_is_closed_bounded_and_post_only() {
    let codex = parse_hook_event(
        br#"{"hook_event_name":"PostToolUse","session_id":"root","tool_use_id":"call"}"#,
    )
    .unwrap();
    let claude =
        parse_claude_hook_event(br#"{"hook_event_name":"PostToolBatch","session_id":"root"}"#)
            .unwrap();
    for (event, name) in [(&codex, "PostToolUse"), (&claude, "PostToolBatch")] {
        let output: serde_json::Value =
            serde_json::from_str(&render_hook_context(event, "one bounded fact").unwrap()).unwrap();
        assert_eq!(output.as_object().unwrap().len(), 1);
        assert_eq!(output["hookSpecificOutput"].as_object().unwrap().len(), 2);
        assert_eq!(output["hookSpecificOutput"]["hookEventName"], name);
        assert_eq!(
            output["hookSpecificOutput"]["additionalContext"],
            "one bounded fact"
        );
    }
    let pre = parse_hook_event(
        br#"{"hook_event_name":"PreToolUse","session_id":"root","tool_use_id":"call"}"#,
    )
    .unwrap();
    assert!(render_hook_context(&pre, "fact").is_none());
    assert!(render_hook_context(&codex, "").is_none());
    assert!(render_hook_context(&codex, &"x".repeat(4097)).is_none());
}

/// Keeps context source scope and diff modes bounded while rejecting authority-like model inputs.
#[test]
fn context_paths_offsets_and_diff_modes_are_closed() {
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../escape",
        "a//b",
        ".",
        "a/./b",
        "nul\0path",
    ] {
        assert!(validate_call(AssistanceTool::Context, json!({"path":path})).is_err());
    }
    for offset in [json!(-1), json!(1.5), json!(1_048_577), json!(null)] {
        assert!(
            validate_call(
                AssistanceTool::Context,
                json!({"path":"src/main.rs","byte_offset":offset})
            )
            .is_err()
        );
    }
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"path":"src/🦀.rs","byte_offset":0})
        )
        .is_ok()
    );
    assert!(validate_call(AssistanceTool::Context, json!({"query":"old schema"})).is_err());
    assert_eq!(
        validate_call(AssistanceTool::Diff, json!({}))
            .unwrap()
            .parameters()["mode"],
        "head"
    );
    for mode in ["head", "staged", "unstaged"] {
        assert!(validate_call(AssistanceTool::Diff, json!({"mode":mode})).is_ok());
    }
    for mode in [json!("HEAD~1"), json!("arbitrary"), json!(null)] {
        assert!(validate_call(AssistanceTool::Diff, json!({"mode":mode})).is_err());
    }
}

/// Accepts the bounded problems arguments without a path and keeps every v0.2 context rejection
/// exact: absent kind still requires `path`, problem-feed fields stay rejected there, and any
/// other kind value falls back to v0.2 behaviour (EYES-r1 §7).
#[test]
fn context_problems_arguments_are_bounded_and_v02_behaviour_is_unchanged() {
    assert!(validate_call(AssistanceTool::Context, json!({"kind":"problems"})).is_ok());
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"rust"})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"python","offset":20,"detail_ref":"d"})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","path":"src/main.rs","offset":0})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","offset":u32::MAX})
        )
        .is_ok()
    );
    for invalid in [
        json!({"kind":"problems","language":"go"}),
        json!({"kind":"problems","language":null}),
        json!({"kind":"problems","offset":-1}),
        json!({"kind":"problems","offset":1.5}),
        json!({"kind":"problems","offset":u64::from(u32::MAX) + 1}),
        json!({"kind":null}),
        json!({"kind":"problems","path":"../escape"}),
        json!({"kind":"problems","path":"a//b"}),
    ] {
        assert!(
            validate_call(AssistanceTool::Context, invalid.clone()).is_err(),
            "{invalid}"
        );
    }
    // Any other kind value keeps v0.2 behaviour: path required, problem fields rejected.
    assert!(validate_call(AssistanceTool::Context, json!({"kind":"everything"})).is_err());
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"everything","path":"src/main.rs"})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"everything","path":"src/main.rs","offset":1})
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"path":"src/main.rs","language":"rust"})
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"path":"src/main.rs","offset":0})
        )
        .is_err()
    );
    // The v0.2 shape itself is unchanged.
    assert!(validate_call(AssistanceTool::Context, json!({"path":"src/main.rs"})).is_ok());
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"path":"src/main.rs","byte_offset":0})
        )
        .is_ok()
    );
    assert!(validate_call(AssistanceTool::Context, json!({"query":"old schema"})).is_err());
}

/// Keeps the edit schema exact and enforces its independent full-content and argument limits.
#[test]
fn edit_arguments_are_closed_and_bounded() {
    let valid = json!({
        "operation_id":"edit-1",
        "path":"src/main.rs",
        "source_ref":"context-1",
        "content":"fn main() {}\n"
    });
    assert!(validate_call(AssistanceTool::Edit, valid.clone()).is_ok());
    let mut extra = valid.clone();
    extra["patch"] = json!("@@");
    assert!(validate_call(AssistanceTool::Edit, extra).is_err());
    for field in ["operation_id", "path", "source_ref", "content"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(validate_call(AssistanceTool::Edit, missing).is_err());
    }
    assert!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"edit-2","path":"src/main.rs","source_ref":"context-1","content":"x".repeat(48 * 1024 + 1)})
        )
        .is_err()
    );
}
