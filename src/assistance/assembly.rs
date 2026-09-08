//! Codex hook/MCP correlation at the finite product dispatch boundary.

use std::{future::Future, pin::Pin, sync::Mutex};

use serde_json::{Value, json};

use super::host_binding::{
    BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
    parse_observed_sandbox_state,
};
use crate::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, AssistanceMethod,
};

pub use super::reply::{MissingPeer, PeerReply};
use super::{launcher::LauncherConfig, reply::FailureCode};

/// Owns bounded daemon-lifetime Codex binding state behind one serialized admission boundary.
///
/// The private endpoint and launcher attachment scope observations; neither authenticates a
/// hostile local user. State contains selected identifiers only, never source or hook payloads.
#[derive(Debug, Default)]
pub struct ProductDispatcher {
    /// Serializes hook observations, validation and explicit stop; never held across I/O.
    bindings: Mutex<HostBindingGuard>,
    /// Immutable attachment mappings accepted at daemon startup, absent in discovery-only mode.
    launcher: Option<LauncherConfig>,
}

impl ProductDispatcher {
    /// Installs one restart-only trusted map; no invocation can replace it or infer a target.
    pub fn with_launcher(launcher: LauncherConfig) -> Self {
        Self {
            bindings: Mutex::new(HostBindingGuard::default()),
            launcher: Some(launcher),
        }
    }

    /// Validates one finite request using only separately supplied host correlations.
    ///
    /// Malformed input and poisoned synchronization fail closed. Only explicit start creates a
    /// binding; stop revokes before any future Workspace handoff. No peer is invoked here.
    fn handle(&self, request: &AssistanceDispatch) -> Option<PeerReply> {
        let mut bindings = self.bindings.lock().ok()?;
        match request {
            AssistanceDispatch::HookSubmit(hook) => {
                let observation: Value =
                    serde_json::from_str(hook.sanitized_observation_json().as_str()).ok()?;
                let object = observation.as_object()?;
                if object.len() != 3 {
                    return None;
                }
                let phase = match object.get("phase")?.as_str()? {
                    "pre" => "PreToolUse",
                    "post" => "PostToolUse",
                    _ => return None,
                };
                let event = parse_hook_event(json!({"hook_event_name":phase,"session_id":object.get("actor_id")?,"tool_use_id":object.get("call_id")?}).to_string().as_bytes()).ok()?;
                if event.call_id() != hook.correlation_id() {
                    return None;
                }
                let channel = parse_channel_session(hook.opaque_attachment().as_bytes()).ok()?;
                match bindings.observe_hook(event, channel) {
                    BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
                    BindingStatus::Settled(_) => Some(PeerReply::HookSettled {}),
                    BindingStatus::NativeObserved(_) => Some(PeerReply::NativeHookObserved {}),
                    _ => None,
                }
            }
            AssistanceDispatch::MethodDispatch(method) => {
                let envelope: Value = serde_json::from_str(method.params_json().as_str()).ok()?;
                let object = envelope.as_object()?;
                if object.len() != 2 {
                    return None;
                }
                let meta = object.get("host_meta")?.as_object()?;
                let candidate = parse_candidate(meta).ok()?;
                if candidate.call_id() != method.correlation_id() {
                    return None;
                }
                let tool = match method.method() {
                    AssistanceMethod::Start => super::facade::AssistanceTool::Start,
                    AssistanceMethod::Context => super::facade::AssistanceTool::Context,
                    AssistanceMethod::Diff => super::facade::AssistanceTool::Diff,
                    AssistanceMethod::Inspect => super::facade::AssistanceTool::Inspect,
                    AssistanceMethod::Stop => super::facade::AssistanceTool::Stop,
                    AssistanceMethod::HookSubmit => return None,
                };
                super::facade::validate_call(tool, object.get("parameters")?.clone()).ok()?;
                let channel = parse_channel_session(method.opaque_attachment().as_bytes()).ok()?;
                let status = if method.method() == AssistanceMethod::Start {
                    bindings.establish_start(candidate, channel)
                } else {
                    bindings.validate_active(candidate, channel)
                };
                let BindingStatus::Validated(invocation) = status else {
                    return None;
                };
                if method.method() == AssistanceMethod::Stop {
                    bindings.stop_binding(invocation.binding_ref()).ok()?;
                    Some(PeerReply::HostStopped {})
                } else {
                    let active = bindings.consume_active(invocation.binding_ref()).ok()?;
                    let observed =
                        match parse_observed_sandbox_state(meta, &invocation, &active, true) {
                            Ok(observed) => observed,
                            Err(_) => {
                                return Some(PeerReply::Error {
                                    code: FailureCode::SandboxState,
                                });
                            }
                        };
                    if crate::execution::HostSandboxState::parse(Some(
                        observed.state().as_json().clone(),
                    ))
                    .is_err()
                    {
                        return Some(PeerReply::Error {
                            code: FailureCode::SandboxState,
                        });
                    }
                    if self.launcher.as_ref().is_some_and(|launcher| {
                        launcher.target(method.opaque_attachment()).is_none()
                    }) {
                        return Some(PeerReply::Error {
                            code: FailureCode::LauncherConfiguration,
                        });
                    }

                    Some(PeerReply::Unavailable {
                        reason: MissingPeer::WorkspaceActivation,
                    })
                }
            }
        }
    }
}

impl AssistanceDispatcher for ProductDispatcher {
    /// Returns only a closed host outcome, with all binding transitions completed before return.
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let result = self.handle(&request).unwrap_or(PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
            });
            let reply = result.encode().ok_or(AssistanceDispatchUnavailable)?;
            Ok(match request {
                AssistanceDispatch::HookSubmit(_) => AssistanceDispatchReply::HookSubmit(reply),
                AssistanceDispatch::MethodDispatch(_) => {
                    AssistanceDispatchReply::MethodDispatch(reply)
                }
            })
        })
    }
}

/// Rejects unknown daemon result shapes instead of manufacturing peer readiness.
#[test]
fn peer_reply_accepts_only_the_closed_host_shapes() {
    for reply in [
        r#"{"state":"ready"}"#,
        r#"{"state":"unavailable","reason":"unknown"}"#,
        r#"{"state":"host_stopped","source":"forged"}"#,
    ] {
        assert!(serde_json::from_str::<PeerReply>(reply).is_err());
    }
}
