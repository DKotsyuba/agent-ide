#[path = "../src/assistance/mod.rs"]
mod assistance;

use assistance::host_binding::{
    BindingStatus, BindingUnavailable, HookPhase, HostBindingGuard, parse_candidate,
    parse_hook_event,
};
use serde_json::json;

fn candidate(actor: &str, call: &str) -> assistance::host_binding::CandidateInvocation {
    parse_candidate(
        json!({
            "threadId": actor,
            "callId": call,
            "x-codex-turn-metadata": { "turn": "not retained" },
            "arguments": { "pretend": "identity" },
        })
        .as_object()
        .unwrap(),
    )
    .unwrap()
}

fn hook(
    phase: &str,
    actor_field: &str,
    actor: &str,
    call: &str,
) -> assistance::host_binding::HookEvent {
    parse_hook_event(
        json!({
            "hook_event_name": phase,
            actor_field: actor,
            "tool_use_id": call,
            "tool_input": { "private": "never retained" },
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

#[test]
fn exact_pre_post_pair_is_validated_once_for_root_and_child() {
    for (actor_field, actor) in [("session_id", "root-session"), ("agent_id", "child-thread")] {
        let mut guard = HostBindingGuard::default();
        let claim = candidate(actor, "call-1");
        assert_eq!(claim.actor_id(), actor);
        assert_eq!(claim.call_id(), "call-1");
        assert!(matches!(guard.register(claim), BindingStatus::Candidate(_)));
        assert!(matches!(
            guard.observe_hook(hook("PreToolUse", actor_field, actor, "call-1")),
            BindingStatus::Candidate(_)
        ));
        let BindingStatus::Validated(validated) =
            guard.observe_hook(hook("PostToolUse", actor_field, actor, "call-1"))
        else {
            panic!("exact post hook must validate");
        };
        assert_eq!(validated.actor_id(), actor);
        assert_eq!(validated.call_id(), "call-1");
        assert!(matches!(
            guard.observe_hook(hook("PostToolUse", actor_field, actor, "call-1")),
            BindingStatus::Unavailable(BindingUnavailable::Replay)
        ));
    }
}

#[test]
fn missing_or_mismatched_or_ambiguous_host_fields_are_unavailable() {
    assert!(matches!(
        parse_candidate(
            json!({ "threadId": "actor", "callId": "call" })
                .as_object()
                .unwrap()
        ),
        Err(BindingUnavailable::MissingField("x-codex-turn-metadata"))
    ));
    assert!(matches!(
        parse_hook_event(br#"{"hook_event_name":"PreToolUse","session_id":"a","agent_id":"a","tool_use_id":"c"}"#),
        Err(BindingUnavailable::InvalidField("hook actor"))
    ));
    let mut guard = HostBindingGuard::default();
    let claim = candidate("actor", "call");
    assert!(matches!(guard.register(claim), BindingStatus::Candidate(_)));
    assert!(matches!(
        guard.observe_hook(hook("PreToolUse", "session_id", "other", "call")),
        BindingStatus::Unavailable(BindingUnavailable::Mismatch)
    ));
    assert!(matches!(
        guard.observe_hook(hook("PostToolUse", "session_id", "actor", "call")),
        BindingStatus::Unavailable(BindingUnavailable::MissingPre)
    ));
}

#[test]
fn stop_rejects_pending_and_later_lifecycle_events() {
    let mut guard = HostBindingGuard::default();
    let claim = candidate("actor", "call");
    assert!(matches!(guard.register(claim), BindingStatus::Candidate(_)));
    guard.stop();
    assert!(matches!(
        guard.observe_hook(hook("PreToolUse", "session_id", "actor", "call")),
        BindingStatus::Unavailable(BindingUnavailable::Stopped)
    ));
    assert!(matches!(
        guard.register(candidate("actor", "next")),
        BindingStatus::Unavailable(BindingUnavailable::Stopped)
    ));
}

#[test]
fn hook_parser_does_not_retain_raw_payload_fields() {
    let event = hook("PreToolUse", "session_id", "actor", "call");
    assert_eq!(event.phase(), HookPhase::Pre);
    assert_eq!(event.actor_id(), "actor");
    assert_eq!(event.call_id(), "call");
    assert!(!format!("{event:?}").contains("never retained"));
}
