//! Explicit Codex/Claude ingress and finite dispatch into one daemon-owned product worker.

/// Stable closed result types shared by existing callers of the assembly boundary.
pub use super::reply::{MissingPeer, PeerReply};
use super::{
    host_binding::{
        BindingStatus, HostBindingGuard, HostKind, parse_candidate, parse_channel_session,
        parse_claude_call_id, parse_claude_hook_event, parse_hook_event, parse_host_kind,
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
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(hook.opaque_attachment()))
                {
                    return None;
                }
                let observation: Value =
                    serde_json::from_str(hook.sanitized_observation_json().as_str()).ok()?;
                let object = observation.as_object()?;
                if object.len() != 6 {
                    return None;
                }
                let phase = match object.get("phase")?.as_str()? {
                    "pre" => "PreToolUse",
                    "post" => "PostToolUse",
                    "post_failure" => "PostToolUseFailure",
                    "permission_denied" => "PermissionDenied",
                    "post_batch" => "PostToolBatch",
                    _ => return None,
                };
                let event = match object.get("host")?.as_str()? {
                    "codex" => parse_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("actor_id")?,"tool_use_id":object.get("call_id")?})
                            .to_string().as_bytes(),
                    ),
                    "claude" => parse_claude_hook_event(
                        json!({"hook_event_name":phase,"session_id":object.get("session_id")?,"agent_id":(object.get("actor_id")? != object.get("session_id")?).then_some(object.get("actor_id")?),"agent_type":object.get("agent_type")?,"tool_use_id":object.get("call_id")?})
                            .to_string().as_bytes(),
                    ),
                    _ => return None,
                }
                .ok()?;
                if event.optional_call_id().unwrap_or("post-tool-batch") != hook.correlation_id() {
                    return None;
                }
                let channel = self.channel(hook.opaque_attachment())?;
                let status = self.bindings.lock().ok()?.observe_hook(event, channel);
                match status {
                    BindingStatus::PreObserved => Some(PeerReply::HookObserved {}),
                    BindingStatus::Settled(_) => Some(PeerReply::HookSettled {}),
                    BindingStatus::NativeObserved(binding) => {
                        if let Some(worker) = &self.worker {
                            worker.native_hint(binding.clone());
                            if let Some(text) = worker.take_current_feedback(binding).await {
                                return Some(PeerReply::Feedback { text });
                            }
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
                let host = parse_host_kind(meta).ok()?;
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
                if self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.accepts_attachment(method.opaque_attachment()))
                {
                    return None;
                }
                let channel = self.channel(method.opaque_attachment())?;
                let (invocation, observed) = {
                    let mut bindings = self.bindings.lock().ok()?;
                    let status = match host {
                        HostKind::Codex => {
                            let candidate = parse_candidate(meta).ok()?;
                            if candidate.call_id() != method.correlation_id() {
                                return None;
                            }
                            if method.method() == AssistanceMethod::Start {
                                bindings.establish_start(candidate, channel)
                            } else {
                                bindings.validate_active(candidate, channel)
                            }
                        }
                        HostKind::Claude => {
                            let call_id = parse_claude_call_id(meta).ok()?;
                            if call_id != method.correlation_id() {
                                return None;
                            }
                            if method.method() == AssistanceMethod::Start {
                                bindings.establish_start_claude(&call_id, channel)
                            } else {
                                bindings.validate_active_claude(&call_id, channel)
                            }
                        }
                    };
                    let BindingStatus::Validated(invocation) = status else {
                        return None;
                    };
                    if method.method() == AssistanceMethod::Stop {
                        bindings.stop_binding(invocation.binding_ref()).ok()?;
                        (invocation, None)
                    } else {
                        let active = bindings.consume_active(invocation.binding_ref()).ok()?;
                        // Claude never advertises or returns `codex/sandbox-state-meta`; establishing
                        // its correlation must never invent sandbox authority it was never given.
                        let observed = match parse_observed_sandbox_state(
                            meta,
                            &invocation,
                            &active,
                            host == HostKind::Codex,
                        ) {
                            Ok(observed) => observed,
                            Err(_) => {
                                if method.method() == AssistanceMethod::Start
                                    && invocation.created_binding()
                                {
                                    let _ = bindings.stop_binding(invocation.binding_ref());
                                }
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
                            if method.method() == AssistanceMethod::Start
                                && invocation.created_binding()
                            {
                                let _ = bindings.stop_binding(invocation.binding_ref());
                            }
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
                                observed?,
                                method.opaque_attachment(),
                                None,
                            )
                            .await
                    }
                    _ => {
                        worker
                            .submit(
                                invocation,
                                observed,
                                tool,
                                call.parameters().clone(),
                                method.opaque_attachment(),
                            )
                            .await
                    }
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
    /// Cancels all queued and active work and waits for the worker to reap owned providers.
    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + '_>> {
        Box::pin(async move {
            match &self.worker {
                Some(worker) => worker
                    .shutdown()
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

/// Uses host-shaped daemon frames to reject mixed Codex and Claude metadata before correlation.
#[tokio::test]
async fn host_shaped_mixed_metadata_is_unavailable_at_daemon_ingress() {
    use crate::app::transport::{MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let parameters = OpaqueJson::from_value(
        &json!({
            "parameters": {"activation_id":"activate"},
            "host_meta": {
                "threadId":"actor",
                "callId":"call",
                "x-codex-turn-metadata":{},
                "claudecode/toolUseId":"call"
            }
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let request = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "call",
            "attachment",
            AssistanceMethod::Start,
            parameters,
        )
        .expect("test dispatch is valid"),
    );
    assert_eq!(dispatcher.handle(&request).await, None);
}

/// Uses host-shaped frames to prove Claude correlation stops at sandbox-state failure.
#[tokio::test]
async fn host_shaped_claude_start_never_reaches_workspace_without_sandbox_authority() {
    use crate::app::transport::{HookSubmit, MethodDispatch, OpaqueJson};

    let dispatcher = ProductDispatcher::default();
    let observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"call",
            "session_id":"session",
            "agent_type":null
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "call", "attachment", observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&hook).await,
        Some(PeerReply::HookObserved {})
    );

    let parameters = OpaqueJson::from_value(
        &json!({
            "parameters":{"activation_id":"activate"},
            "host_meta":{"claudecode/toolUseId":"call"}
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let method = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "call",
            "attachment",
            AssistanceMethod::Start,
            parameters,
        )
        .expect("test method dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&method).await,
        Some(PeerReply::Error {
            code: FailureCode::SandboxState
        })
    );

    let next_observation = OpaqueJson::from_value(
        &json!({
            "host":"claude",
            "phase":"pre",
            "actor_id":"session",
            "call_id":"next",
            "session_id":"session",
            "agent_type":null
        }),
        64 * 1024,
    )
    .expect("test observation is bounded");
    let next_hook = AssistanceDispatch::HookSubmit(
        HookSubmit::new("request", "next", "attachment", next_observation)
            .expect("test hook dispatch is valid"),
    );
    assert_eq!(
        dispatcher.handle(&next_hook).await,
        Some(PeerReply::HookObserved {})
    );
    let next_parameters = OpaqueJson::from_value(
        &json!({
            "parameters":{"path":"tracked.rs"},
            "host_meta":{"claudecode/toolUseId":"next"}
        }),
        64 * 1024,
    )
    .expect("test frame is bounded");
    let next_method = AssistanceDispatch::MethodDispatch(
        MethodDispatch::new(
            "request",
            "next",
            "attachment",
            AssistanceMethod::Context,
            next_parameters,
        )
        .expect("test method dispatch is valid"),
    );
    assert_eq!(dispatcher.handle(&next_method).await, None);
}
