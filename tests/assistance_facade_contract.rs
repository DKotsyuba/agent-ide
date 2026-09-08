//! Contract checks for the bounded five-tool Assistance facade and fail-open feedback core.

use std::{fs, path::PathBuf};

use agent_ide::{
    assistance::facade::{
        AssistanceFacade, AssistanceTool, FacadeOutcome, FeedbackDelta, FeedbackLedger,
        FeedbackRecord, FeedbackState, HookIngressOutcome, TrustedTransport, submit_inactive_hook,
        tool_schemas, validate_call,
    },
    assistance::host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
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
fn discovery_is_static_and_contains_exactly_five_current_methods() {
    let schemas = tool_schemas();
    assert_eq!(schemas.len(), 5);
    assert!(schemas.iter().map(|schema| schema.name).eq([
        "ide.start",
        "ide.context",
        "ide.diff",
        "ide.inspect",
        "ide.stop"
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
            .dispatch(&host(), AssistanceTool::Context, json!({"query":"status"}))
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
    feedback.stop_authority("authority-1");
    assert_eq!(
        feedback.record("authority-1", "source-2", &delta),
        FeedbackRecord::Suppressed
    );
}
