use agent_ide::assistance::host_binding::{
    BindingStatus, BindingUnavailable, ChannelSessionRef, HookPhase, HostBindingGuard, HostKind,
    SandboxStateProvenance, parse_candidate, parse_channel_session, parse_claude_hook_event,
    parse_hook_event, parse_observed_sandbox_state,
};
use serde_json::json;

fn candidate(actor: &str, call: &str) -> agent_ide::assistance::host_binding::CandidateInvocation {
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
) -> agent_ide::assistance::host_binding::HookEvent {
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

fn channel(name: &str) -> ChannelSessionRef {
    parse_channel_session(name.as_bytes()).unwrap()
}

#[test]
fn pre_then_start_or_active_mcp_then_post_is_exact_for_root_and_child() {
    for (actor_field, actor) in [("session_id", "root-session"), ("agent_id", "child-thread")] {
        let mut guard = HostBindingGuard::default();
        let channel = channel("channel-a");
        assert!(matches!(
            guard.observe_hook(
                hook("PreToolUse", actor_field, actor, "call-1"),
                channel.clone()
            ),
            BindingStatus::PreObserved
        ));
        let claim = candidate(actor, "call-1");
        assert_eq!(claim.actor_id(), actor);
        assert_eq!(claim.call_id(), "call-1");
        let BindingStatus::Validated(started) = guard.establish_start(claim, channel.clone())
        else {
            panic!("exact pre hook must validate explicit start");
        };
        assert_eq!(started.actor_id(), actor);
        assert_eq!(started.call_id(), "call-1");
        let consumed = guard.consume_active(started.binding_ref()).unwrap();
        assert_eq!(consumed.binding_ref(), started.binding_ref());
        let BindingStatus::Settled(settled) = guard.observe_hook(
            hook("PostToolUse", actor_field, actor, "call-1"),
            channel.clone(),
        ) else {
            panic!("post hook must settle an already validated invocation");
        };
        assert_eq!(settled.call_id(), "call-1");
        assert!(matches!(
            guard.observe_hook(
                hook("PostToolUse", actor_field, actor, "call-1"),
                channel.clone(),
            ),
            BindingStatus::Unavailable(BindingUnavailable::Replay)
        ));
        assert!(matches!(
            guard.observe_hook(
                hook("PreToolUse", actor_field, actor, "call-2"),
                channel.clone()
            ),
            BindingStatus::PreObserved
        ));
        let BindingStatus::Validated(ordinary) =
            guard.validate_active(candidate(actor, "call-2"), channel)
        else {
            panic!("ordinary call must use the active start binding");
        };
        assert_eq!(ordinary.binding_ref(), started.binding_ref());
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
    assert!(matches!(
        parse_channel_session(&[0xff]),
        Err(BindingUnavailable::InvalidAttachment)
    ));
    let mut guard = HostBindingGuard::default();
    let claim = candidate("actor", "call");
    assert!(matches!(
        guard.observe_hook(
            hook("PreToolUse", "session_id", "other", "call"),
            channel("channel-a"),
        ),
        BindingStatus::PreObserved
    ));
    assert!(matches!(
        guard.establish_start(claim, channel("channel-a")),
        BindingStatus::Unavailable(BindingUnavailable::MissingPre)
    ));
    assert!(matches!(
        guard.observe_hook(
            hook("PostToolUse", "session_id", "actor", "call"),
            channel("channel-a"),
        ),
        BindingStatus::Unavailable(BindingUnavailable::Mismatch)
    ));
}

#[test]
fn stop_revokes_old_refs_and_later_explicit_start_gets_a_fresh_generation() {
    let mut guard = HostBindingGuard::default();
    let channel = channel("channel-a");
    assert!(matches!(
        guard.observe_hook(
            hook("PreToolUse", "session_id", "actor", "call"),
            channel.clone(),
        ),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(first) =
        guard.establish_start(candidate("actor", "call"), channel.clone())
    else {
        panic!("first explicit start must validate");
    };
    guard.stop_binding(first.binding_ref()).unwrap();
    assert!(matches!(
        guard.consume_active(first.binding_ref()),
        Err(BindingUnavailable::InactiveBinding)
    ));
    assert!(matches!(
        guard.observe_hook(
            hook("PreToolUse", "session_id", "actor", "ordinary"),
            channel.clone(),
        ),
        BindingStatus::PreObserved
    ));
    assert!(matches!(
        guard.validate_active(candidate("actor", "ordinary"), channel.clone()),
        BindingStatus::Unavailable(BindingUnavailable::InactiveBinding)
    ));
    assert!(matches!(
        guard.observe_hook(
            hook("PreToolUse", "session_id", "actor", "restart"),
            channel.clone(),
        ),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(restarted) =
        guard.establish_start(candidate("actor", "restart"), channel)
    else {
        panic!("later explicit start must create a fresh binding");
    };
    assert_ne!(restarted.binding_ref(), first.binding_ref());
    guard.stop();
    assert!(matches!(
        guard.check_active(restarted.binding_ref()),
        Err(BindingUnavailable::InactiveBinding)
    ));
}

#[test]
fn observed_sandbox_state_requires_active_use_and_preserves_nested_object() {
    let mut guard = HostBindingGuard::default();
    let channel = channel("channel-a");
    assert!(matches!(
        guard.observe_hook(
            hook("PreToolUse", "session_id", "actor", "call"),
            channel.clone(),
        ),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("explicit start must validate");
    };
    let active_use = guard.consume_active(invocation.binding_ref()).unwrap();
    let observed = parse_observed_sandbox_state(
        json!({
            "codex/sandbox-state-meta": {
                "permissionProfile": "managed",
                "codexLinuxSandboxExe": { "opaque": true },
                "sandboxCwd": "/private/tmp/fixture",
                "useLegacyLandlock": false,
                "nested": { "preserved": [1, 2, 3] },
            }
        })
        .as_object()
        .unwrap(),
        &invocation,
        &active_use,
        true,
    )
    .unwrap();
    assert_eq!(observed.actor_id(), "actor");
    assert_eq!(observed.call_id(), "call");
    assert_eq!(observed.binding_ref(), invocation.binding_ref());
    assert_eq!(
        observed.provenance(),
        SandboxStateProvenance::AdvertisedAndReturned
    );
    assert_eq!(observed.state().as_json()["nested"]["preserved"][2], 3);
    assert!(!format!("{observed:?}").contains("preserved"));
    assert!(matches!(
        parse_observed_sandbox_state(
            json!({}).as_object().unwrap(),
            &invocation,
            &active_use,
            false,
        ),
        Err(BindingUnavailable::CapabilityNotAdvertised)
    ));
}

/// Drops raw host payload fields and refuses ambiguous duplicate correlation keys.
#[test]
fn hook_parser_does_not_retain_raw_payload_fields() {
    let event = hook("PreToolUse", "session_id", "actor", "call");
    assert_eq!(event.phase(), HookPhase::Pre);
    assert_eq!(event.actor_id(), "actor");
    assert_eq!(event.call_id(), "call");
    assert!(!format!("{event:?}").contains("never retained"));
    assert!(parse_hook_event(br#"{"hook_event_name":"PreToolUse","session_id":"a","session_id":"b","tool_use_id":"c"}"#).is_err());
    assert!(parse_hook_event(br#"{"hook_event_name":"PreToolUse","session_id":"a","tool_use_id":"c","tool_use_id":"d"}"#).is_err());
}

/// Preserves Claude's exact session and two distinct subagent identities without permission inference.
#[test]
fn claude_parent_and_two_subagents_remain_explicit_and_isolated() {
    let parent = parse_claude_hook_event(
        br#"{"hook_event_name":"PostToolUse","session_id":"session","tool_use_id":"parent-call","permission_mode":"bypassPermissions","tool_response":"secret"}"#,
    )
    .unwrap();
    assert_eq!(parent.host(), HostKind::Claude);
    assert_eq!(parent.actor_id(), "session");
    assert_eq!(parent.session_id(), Some("session"));
    assert_eq!(parent.agent_type(), None);
    for (agent_id, agent_type) in [("child-1", "Explore"), ("child-2", "general-purpose")] {
        let event = parse_claude_hook_event(
            json!({"hook_event_name":"PostToolUse","session_id":"session","agent_id":agent_id,
                "agent_type":agent_type,"tool_use_id":"same-call","permission_mode":"default",
                "tool_input":{"secret":"discarded"}})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(event.actor_id(), agent_id);
        assert_eq!(event.session_id(), Some("session"));
        assert_eq!(event.agent_type(), Some(agent_type));
        assert!(!format!("{event:?}").contains("discarded"));
    }
}

/// Accepts a host-declared batch boundary without fabricating a tool or session-derived call ID.
#[test]
fn batch_hook_has_no_synthetic_call_identity() {
    let event = parse_claude_hook_event(
        br#"{"hook_event_name":"PostToolBatch","session_id":"session","agent_id":"child"}"#,
    )
    .unwrap();
    assert_eq!(event.phase(), HookPhase::PostBatch);
    assert_eq!(event.call_id(), "");
    assert_eq!(event.optional_call_id(), None);
    assert!(
        parse_claude_hook_event(
            br#"{"hook_event_name":"PostToolUse","agent_id":"child","tool_use_id":"call"}"#
        )
        .is_err()
    );
}

/// Coalesces active native lifecycles without authorizing effects or repairing a premature MCP post.
#[test]
fn active_native_lifecycles_only_request_a_revocable_registered_path_recheck() {
    let mut guard = HostBindingGuard::default();
    let channel = channel("native-channel");
    guard.observe_hook(
        hook("PreToolUse", "session_id", "actor", "start"),
        channel.clone(),
    );
    let BindingStatus::Validated(started) =
        guard.establish_start(candidate("actor", "start"), channel.clone())
    else {
        panic!("start must validate");
    };
    for call in ["edit", "delete", "rename", "failed-command"] {
        guard.observe_hook(
            hook("PreToolUse", "session_id", "actor", call),
            channel.clone(),
        );
        let post = parse_hook_event(json!({"hook_event_name":"PostToolUse","session_id":"actor","tool_use_id":call,
            "tool_name":"exec_command","tool_input":{"cmd":"opaque command"},"tool_response":{"exit_code":7}}).to_string().as_bytes()).unwrap();
        assert!(matches!(
            guard.observe_hook(post, channel.clone()),
            BindingStatus::NativeObserved(_)
        ));
        assert!(matches!(
            guard.establish_start(candidate("actor", call), channel.clone()),
            BindingStatus::Unavailable(_)
        ));
    }
    assert!(
        guard
            .take_native_change_hint(started.binding_ref())
            .unwrap()
    );
    assert!(
        !guard
            .take_native_change_hint(started.binding_ref())
            .unwrap()
    );
    guard.observe_hook(
        hook("PreToolUse", "session_id", "actor", "pending"),
        channel.clone(),
    );
    guard.observe_hook(
        hook("PostToolUse", "session_id", "actor", "pending"),
        channel.clone(),
    );
    guard.stop_binding(started.binding_ref()).unwrap();
    assert!(
        guard
            .take_native_change_hint(started.binding_ref())
            .is_err()
    );
    guard.observe_hook(
        hook("PreToolUse", "session_id", "actor", "inactive"),
        channel.clone(),
    );
    assert!(matches!(
        guard.observe_hook(
            hook("PostToolUse", "session_id", "actor", "inactive"),
            channel
        ),
        BindingStatus::Unavailable(_)
    ));
}
