//! Contract checks for Workspace identity, activation, and expected-authority stop behavior.

use std::path::PathBuf;

use agent_ide::{
    assistance::host_binding::{
        BindingStatus, ChannelSessionRef, HostBindingGuard, parse_candidate, parse_channel_session,
        parse_hook_event,
    },
    workspace::authority::{
        ActivationRequest, AuthorityError, AuthorityRegistry, RevocationReason, StopBindingHandoff,
        WorktreeRef,
    },
};
use serde_json::json;

/// Produces a trusted candidate with one bounded actor/call pair for a contract scenario.
fn candidate(actor: &str, call: &str) -> agent_ide::assistance::host_binding::CandidateInvocation {
    parse_candidate(
        json!({
            "threadId": actor,
            "callId": call,
            "x-codex-turn-metadata": { "turn": "ignored" },
        })
        .as_object()
        .expect("test metadata is an object"),
    )
    .expect("test metadata is valid")
}

/// Produces the native pre-hook evidence that Assistance requires before validating a start.
fn pre(actor: &str, call: &str) -> agent_ide::assistance::host_binding::HookEvent {
    parse_hook_event(
        json!({
            "hook_event_name": "PreToolUse",
            "session_id": actor,
            "tool_use_id": call,
        })
        .to_string()
        .as_bytes(),
    )
    .expect("test pre hook is valid")
}

/// Builds a worktree incarnation whose paths exercise raw-path storage without filesystem access.
fn worktree(name: &str, incarnation: u64) -> WorktreeRef {
    WorktreeRef::from_discovery(
        PathBuf::from(format!("/private/tmp/{name}")),
        PathBuf::from(format!("/private/tmp/{name}")),
        PathBuf::from(format!("/private/tmp/{name}/.git")),
        incarnation,
    )
    .expect("test paths are canonical")
}

/// Starts an Assistance binding and consumes fresh liveness for one Workspace activation.
fn activation(
    guard: &mut HostBindingGuard,
    actor: &str,
    call: &str,
    operation: &str,
    worktree: WorktreeRef,
) -> ActivationRequest {
    let channel: ChannelSessionRef =
        parse_channel_session(b"workspace-contract").expect("test channel is valid");
    assert!(matches!(
        guard.observe_hook(pre(actor, call), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate(actor, call), channel)
    else {
        panic!("exact pre/start must validate");
    };
    let active_use = guard
        .consume_active(invocation.binding_ref())
        .expect("new start binding is active");
    ActivationRequest::new(operation, invocation, active_use, worktree)
        .expect("matching fresh binding builds an activation")
}

/// Verifies stable activation retries, binding-scoped authorization, and old-stop fencing.
#[test]
fn activation_and_expected_stop_preserve_the_newer_authority() {
    let mut guard = HostBindingGuard::default();
    let mut registry = AuthorityRegistry::default();
    let first = registry
        .activate(activation(
            &mut guard,
            "actor-a",
            "start-a",
            "activate-a",
            worktree("workspace-a", 1),
        ))
        .expect("first actor owns its worktree");
    let first_use = guard
        .consume_active(first.binding())
        .expect("live binding supplies a fresh use");
    registry
        .authorize(&first, &first_use)
        .expect("matching active use authorizes current stamp");

    guard
        .stop_binding(first.binding())
        .expect("Assistance stops the old generation first");
    let revoked = registry
        .revoke(&first, StopBindingHandoff::Confirmed)
        .expect("expected current stop revokes exactly one stamp");
    assert_eq!(revoked.old_epoch(), first.epoch());
    assert_eq!(revoked.reason(), RevocationReason::Requested);

    let replacement = registry
        .activate(activation(
            &mut guard,
            "actor-a",
            "start-b",
            "activate-b",
            worktree("workspace-a", 2),
        ))
        .expect("explicit restart receives a fresh authority");
    assert!(replacement.epoch() > first.epoch());
    assert_eq!(
        registry.revoke(&first, StopBindingHandoff::Confirmed),
        Err(AuthorityError::StaleAuthority)
    );
    let replacement_use = guard
        .consume_active(replacement.binding())
        .expect("old stop cannot revoke the newer Assistance generation");
    registry
        .authorize(&replacement, &replacement_use)
        .expect("old stop never invalidates newer authority");
}

/// Verifies a missing host stop handoff fails closed without trying to reactivate its binding.
#[test]
fn missing_stop_handoff_revokes_authority_without_reusing_the_binding() {
    let mut guard = HostBindingGuard::default();
    let mut registry = AuthorityRegistry::default();
    let stamp = registry
        .activate(activation(
            &mut guard,
            "actor-a",
            "start-a",
            "activate-a",
            worktree("workspace-a", 1),
        ))
        .expect("first activation succeeds");
    let revoked = registry
        .revoke(&stamp, StopBindingHandoff::Missing)
        .expect("missing handoff still removes Workspace authority");
    assert_eq!(revoked.reason(), RevocationReason::RevocationIncomplete);
    let use_after_revoke = guard
        .consume_active(stamp.binding())
        .expect("the test keeps Assistance live to prove Workspace refuses the old stamp");
    assert_eq!(
        registry.authorize(&stamp, &use_after_revoke),
        Err(AuthorityError::StaleAuthority)
    );
}
