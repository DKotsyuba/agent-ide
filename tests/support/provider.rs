//! Host-bound execution fixtures shared by provider seam acceptance tests.

use agent_ide::{
    assistance::host_binding::{
        ActiveBindingUse, BindingRef, BindingStatus, HostBindingGuard, ValidatedInvocation,
        parse_candidate, parse_channel_session, parse_hook_event,
    },
    execution::{
        ControlledCommand, LocalExecutionPolicy, ValidatedExecutionRequest,
        ValidatedHostInvocation, WorkspaceAuthority,
    },
};
use serde_json::json;
use std::{collections::BTreeSet, path::Path};

/// Owns genuine Assistance-derived scope while requiring a fresh consume for each physical test spawn.
pub struct BoundRequest {
    /// Exact validated invocation used to correlate current metadata in read-admission tests.
    pub invocation: ValidatedInvocation,
    /// Execution request retaining the exact active binding generation.
    pub request: ValidatedExecutionRequest,
    /// Live test guard, never replaced by a cached ActiveBindingUse.
    pub guard: HostBindingGuard,
    /// Opaque binding identity used to consume/revoke liveness.
    pub binding: BindingRef,
}
impl BoundRequest {
    /// Validates exact authority and command under the test's matched host-binding pre-hook.
    pub fn new(
        label: &str,
        authority: WorkspaceAuthority,
        command: ControlledCommand,
        program: &Path,
    ) -> Self {
        let mut guard = HostBindingGuard::default();
        let channel = parse_channel_session(label.as_bytes()).unwrap();
        let actor = format!("actor-{label}");
        let hook = parse_hook_event(
            json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":"spawn"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert!(matches!(
            guard.observe_hook(hook, channel.clone()),
            BindingStatus::PreObserved
        ));
        let candidate=parse_candidate(json!({"threadId":actor,"callId":"spawn","x-codex-turn-metadata":{"turn":"provider-contract"}}).as_object().unwrap()).unwrap();
        let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
            panic!("valid fixture binding")
        };
        let binding = invocation.binding_ref().clone();
        let active = guard.consume_active(&binding).unwrap();
        let policy =
            LocalExecutionPolicy::new(BTreeSet::from([program.to_path_buf()]), 65536, 16).unwrap();
        let request = ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_active_use(active),
            authority,
            command,
            &policy,
        )
        .unwrap();
        Self {
            request,
            guard,
            binding,
            invocation,
        }
    }

    /// Consumes current liveness immediately before an individual delayed spawn.
    pub fn fresh(&mut self) -> ActiveBindingUse {
        self.guard.consume_active(&self.binding).unwrap()
    }
}
