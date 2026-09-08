//! Codex ingress and finite dispatch into one daemon-owned, durable-authorized product worker.

/// Stable closed result types shared by existing callers of the assembly boundary.
pub use super::reply::{MissingPeer, PeerReply};
use super::{
    host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
        parse_observed_sandbox_state,
    },
    launcher::LauncherConfig,
    reply::FailureCode,
    worker::WorkerHandle,
};
use crate::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, AssistanceMethod,
};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::Read,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
};

/// Owns exact host binding and at most one configured worker for one daemon boot.
/// Private launcher values never come from method arguments; no binding lock crosses an I/O await.
pub struct ProductDispatcher {
    /// Serializes exact hook/MCP correlation, liveness consumes and stop linearization.
    bindings: Arc<Mutex<HostBindingGuard>>,
    /// Absent in discovery-only mode, where peer operations remain explicitly unavailable.
    worker: Option<WorkerHandle>,
    /// Private nonce distinguishes effective channel/binding generations across daemon restarts.
    scope: Option<[u8; 32]>,
}
impl std::fmt::Debug for ProductDispatcher {
    /// Omits private channel nonces, host identities and all worker state.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProductDispatcher(..)")
    }
}

impl Default for ProductDispatcher {
    /// Creates an unconfigured host boundary with a fresh process-independent channel nonce.
    /// Entropy failure leaves binding unavailable rather than reusing a prior daemon scope.
    fn default() -> Self {
        let mut scope = [0; 32];
        let scope = std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut scope))
            .ok()
            .map(|_| scope);
        Self {
            bindings: Arc::new(Mutex::new(HostBindingGuard::default())),
            worker: None,
            scope,
        }
    }
}
impl ProductDispatcher {
    /// Installs one immutable trusted map; peer startup waits for Application's exclusive daemon lock.
    pub fn with_launcher(launcher: LauncherConfig) -> Self {
        let mut dispatcher = Self::default();
        if let Some(scope) = dispatcher.scope {
            dispatcher.worker = Some(WorkerHandle::new(
                dispatcher.bindings.clone(),
                launcher,
                scope,
            ));
        }
        dispatcher
    }
    /// Derives the same opaque channel for hook/MCP input under this exact daemon nonce.
    fn channel(&self, attachment: &str) -> Option<super::host_binding::ChannelSessionRef> {
        let mut hash = blake3::Hasher::new();
        hash.update(&self.scope?);
        hash.update(attachment.as_bytes());
        parse_channel_session(hash.finalize().to_hex().as_bytes()).ok()
    }
    /// Parses separated ingress and commits binding transitions before queue, inspection or stop I/O.
    async fn handle(&self, request: &AssistanceDispatch) -> Option<PeerReply> {
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
                let event=parse_hook_event(json!({"hook_event_name":phase,"session_id":object.get("actor_id")?,"tool_use_id":object.get("call_id")?}).to_string().as_bytes()).ok()?;
                if event.call_id() != hook.correlation_id() {
                    return None;
                }
                let channel = self.channel(hook.opaque_attachment())?;
                let status = self.bindings.lock().ok()?.observe_hook(event, channel);
                match status {
                    BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
                    BindingStatus::Settled(_) => Some(PeerReply::HookSettled {}),
                    BindingStatus::NativeObserved(binding) => {
                        if let Some(worker) = &self.worker {
                            worker.native_hint(binding);
                        }
                        Some(PeerReply::NativeHookObserved {})
                    }
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
                let call =
                    super::facade::validate_call(tool, object.get("parameters")?.clone()).ok()?;
                let channel = self.channel(method.opaque_attachment())?;
                let (invocation, observed) = {
                    let mut bindings = self.bindings.lock().ok()?;
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
                        (invocation, None)
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
                        (invocation, Some(observed))
                    }
                };
                let Some(worker) = &self.worker else {
                    return Some(if method.method() == AssistanceMethod::Stop {
                        PeerReply::HostStopped {}
                    } else {
                        PeerReply::Unavailable {
                            reason: MissingPeer::WorkspaceActivation,
                        }
                    });
                };
                Some(match method.method() {
                    AssistanceMethod::Stop => {
                        worker.stop(invocation, method.opaque_attachment()).await
                    }
                    AssistanceMethod::Inspect => {
                        worker
                            .inspect(
                                invocation.binding_ref().clone(),
                                call.parameters()["detail_ref"].as_str()?.to_owned(),
                            )
                            .await
                    }
                    _ => worker.submit(
                        invocation,
                        observed,
                        tool,
                        call.parameters().clone(),
                        method.opaque_attachment(),
                    ),
                })
            }
        }
    }
}
impl AssistanceDispatcher for ProductDispatcher {
    /// Opens configured peers once, only after Application owns the daemon endpoint lock.
    fn initialize<'a>(
        &'a self,
        runtime_dir: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>> {
        Box::pin(async move {
            match &self.worker {
                Some(worker) => worker
                    .start(runtime_dir)
                    .await
                    .map_err(|_| AssistanceDispatchUnavailable),
                None => Ok(()),
            }
        })
    }
    /// Returns bounded closed outcomes; slow jobs become pending while short inspections stay finite.
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
            let result = self
                .handle(&request)
                .await
                .unwrap_or(PeerReply::Unavailable {
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

/// Rejects arbitrary daemon state or extra result fields instead of manufacturing peer readiness.
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

/// Same attachment is stable within a daemon but cannot recreate a prior boot binding fingerprint.
#[test]
fn daemon_scope_is_fresh_without_actor_or_timing_inference() {
    let first = ProductDispatcher::default();
    let second = ProductDispatcher::default();
    assert_eq!(
        first.channel("same").unwrap(),
        first.channel("same").unwrap()
    );
    assert_ne!(
        first.channel("same").unwrap(),
        second.channel("same").unwrap()
    );
    assert!(!format!("{first:?}").contains("scope"));
}
