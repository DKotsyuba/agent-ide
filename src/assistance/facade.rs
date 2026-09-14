//! Bounded MCP discovery, finite Application routing, and fail-open Assistance feedback.
//!
//! This module has no peer-domain implementation of its own. It validates the six logical tool
//! inputs, carries trusted host transport context, and honestly reports an unavailable or
//! incomplete result until Workspace, Intelligence, and Changes return their typed facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, tool, tool_router};
use serde_json::{Map, Value, json};

use crate::{
    app::{
        dispatch_method_if_running, submit_hook_if_running,
        transport::{
            AssistanceMethod, HookSubmit, HookSubmitTransportResult, HookTransportLimits,
            MethodDispatch, MethodDispatchTransportResult, OpaqueJson,
        },
    },
    assistance::{
        content,
        host_binding::{
            HookEvent, HookLaunch, HookPhase, HostBindingGuard, HostKind, parse_candidate,
            parse_claude_call_id, parse_hook_event, parse_host_kind,
        },
        reply::{MAX_FEEDBACK_BYTES, MissingPeer, PeerReply, ResultKind},
    },
    workspace::authority::{
        AuthorityError, AuthorityRegistry, AuthorityRevoked, AuthorityStamp, StopBindingHandoff,
    },
};

const MAX_PARAMETER_BYTES: usize = 4 * 1024;
const MAX_TEXT_BYTES: usize = 512;
const MAX_DETAIL_REF_BYTES: usize = 128;
/// Maximum UTF-8 relative source path accepted from a model request.
const MAX_RELATIVE_PATH_BYTES: usize = 1024;
/// Byte offsets cannot exceed Workspace's bounded source payload.
const MAX_BYTE_OFFSET: u64 = crate::workspace::observation::MAX_SOURCE_BYTES as u64;
const MAX_ACTIVATION_ID_BYTES: usize = 128;
const MAX_HOOK_BYTES: usize = 64 * 1024;

/// Names the only logical MCP methods exposed by Assistance through v0.2.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AssistanceTool {
    /// Creates or retries one bounded Workspace activation operation.
    Start,
    /// Requests bounded context from Intelligence when that peer is available.
    Context,
    /// Requests a bounded Changes diff when that peer is available.
    Diff,
    /// Expands one owner-scoped detail reference when its owner peer is available.
    Inspect,
    /// Stops the host binding and the expected Workspace authority generation.
    Stop,
    /// Applies one full-content, stale-safe edit through Changes and Workspace.
    Edit,
}

impl AssistanceTool {
    /// Returns the stable MCP discovery name for this logical method.
    pub const fn mcp_name(self) -> &'static str {
        match self {
            Self::Start => "ide.start",
            Self::Context => "ide.context",
            Self::Diff => "ide.diff",
            Self::Inspect => "ide.inspect",
            Self::Stop => "ide.stop",
            Self::Edit => "ide.edit",
        }
    }

    /// Returns the corresponding closed Application transport tag.
    const fn transport_method(self) -> AssistanceMethod {
        match self {
            Self::Start => AssistanceMethod::Start,
            Self::Context => AssistanceMethod::Context,
            Self::Diff => AssistanceMethod::Diff,
            Self::Inspect => AssistanceMethod::Inspect,
            Self::Stop => AssistanceMethod::Stop,
            Self::Edit => AssistanceMethod::Edit,
        }
    }
}

/// Describes one statically available MCP tool without consulting daemon health.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSchema {
    /// Logical method selected by the schema.
    pub tool: AssistanceTool,
    /// MCP-visible stable tool name.
    pub name: &'static str,
    /// JSON Schema object enforcing the bounded model-facing arguments.
    pub input_schema: Value,
}

/// Returns exactly the six current Assistance schemas regardless of daemon availability.
pub fn tool_schemas() -> [ToolSchema; 6] {
    [
        schema(
            AssistanceTool::Start,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["activation_id"],
                "properties": {"activation_id": {"type": "string", "minLength": 1, "maxLength": MAX_ACTIVATION_ID_BYTES}}
            }),
        ),
        schema(
            AssistanceTool::Context,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES},
                    "byte_offset": {"type": "integer", "minimum": 0, "maximum": MAX_BYTE_OFFSET},
                    "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES}
                }
            }),
        ),
        schema(
            AssistanceTool::Diff,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {"mode": {"type":"string","enum":["head","staged","unstaged"],"default":"head"}, "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES}}
            }),
        ),
        schema(
            AssistanceTool::Inspect,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["detail_ref"],
                "properties": {"detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES}}
            }),
        ),
        schema(
            AssistanceTool::Stop,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {}
            }),
        ),
        schema(
            AssistanceTool::Edit,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["operation_id", "path", "source_ref", "content"],
                "properties": {
                    "operation_id": {"type": "string", "minLength": 1, "maxLength": 128},
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES},
                    "source_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES},
                    "content": {"type": "string", "maxLength": crate::workspace::edit::MAX_EDIT_CONTENT_BYTES}
                }
            }),
        ),
    ]
}

/// Builds one schema record while keeping its MCP name coupled to its logical method.
fn schema(tool: AssistanceTool, input_schema: Value) -> ToolSchema {
    ToolSchema {
        tool,
        name: tool.mcp_name(),
        input_schema,
    }
}

/// Explains why a model-facing tool argument object was rejected before routing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParameterError {
    /// The argument value was not an object under the hard byte limit.
    InvalidObject,
    /// The object named a field outside the selected method's closed schema.
    UnknownField,
    /// A required field was absent, empty, non-string, or over its method-specific limit.
    InvalidField,
}

/// Holds one validated bounded method payload with no identity or authority fields.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedCall {
    /// Selected logical method.
    tool: AssistanceTool,
    /// Exact normalized JSON object to send in the finite Application envelope.
    parameters: Value,
}

impl ValidatedCall {
    /// Returns the selected logical tool after its model arguments passed boundary validation.
    pub const fn tool(&self) -> AssistanceTool {
        self.tool
    }

    /// Returns the bounded normalized object without adding host identity or authority fields.
    pub fn parameters(&self) -> &Value {
        &self.parameters
    }
}

/// Validates one closed logical tool payload and rejects identity or authority-shaped extra fields.
pub fn validate_call(
    tool: AssistanceTool,
    mut parameters: Value,
) -> Result<ValidatedCall, ParameterError> {
    let parameter_limit = if tool == AssistanceTool::Edit {
        crate::changes::edit::MAX_EDIT_ARGUMENT_BYTES
    } else {
        MAX_PARAMETER_BYTES
    };
    if serde_json::to_vec(&parameters)
        .ok()
        .is_none_or(|value| value.len() > parameter_limit)
    {
        return Err(ParameterError::InvalidObject);
    }
    let object = parameters
        .as_object()
        .ok_or(ParameterError::InvalidObject)?;
    let allowed = match tool {
        AssistanceTool::Start => &["activation_id"][..],
        AssistanceTool::Context => &["path", "byte_offset", "detail_ref"][..],
        AssistanceTool::Diff => &["mode", "detail_ref"][..],
        AssistanceTool::Inspect => &["detail_ref"][..],
        AssistanceTool::Stop => &[][..],
        AssistanceTool::Edit => &["operation_id", "path", "source_ref", "content"][..],
    };
    if object
        .keys()
        .any(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ParameterError::UnknownField);
    }
    match tool {
        AssistanceTool::Start => {
            required_string(object, "activation_id", MAX_ACTIVATION_ID_BYTES)?;
        }
        AssistanceTool::Context => {
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            if path.as_bytes().contains(&0)
                || path.split('/').any(|part| matches!(part, "" | "." | ".."))
            {
                return Err(ParameterError::InvalidField);
            }
            if object
                .get("byte_offset")
                .is_some_and(|value| value.as_u64().is_none_or(|offset| offset > MAX_BYTE_OFFSET))
            {
                return Err(ParameterError::InvalidField);
            }

            optional_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Diff => {
            optional_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
            if object.get("mode").is_some_and(|value| {
                !matches!(value.as_str(), Some("head" | "staged" | "unstaged"))
            }) {
                return Err(ParameterError::InvalidField);
            }
        }

        AssistanceTool::Inspect => {
            required_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Stop => {}
        AssistanceTool::Edit => {
            let request = crate::changes::edit::EditRequest::new(
                required_string(object, "operation_id", 128)?,
                required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?,
                required_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?,
                object
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or(ParameterError::InvalidField)?,
            )
            .map_err(|_| ParameterError::InvalidField)?;
            parameters = serde_json::to_value(request).map_err(|_| ParameterError::InvalidField)?;
        }
    }
    if tool == AssistanceTool::Diff {
        parameters
            .as_object_mut()
            .expect("validated object")
            .entry("mode")
            .or_insert(json!("head"));
    }
    Ok(ValidatedCall { tool, parameters })
}

/// Reads one required bounded nonempty string from a closed method object.
fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    max_bytes: usize,
) -> Result<&'a str, ParameterError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ParameterError::InvalidField)?;
    (!value.is_empty() && value.len() <= max_bytes)
        .then_some(value)
        .ok_or(ParameterError::InvalidField)
}

/// Reads one optional bounded nonempty string and rejects a present non-string or empty value.
fn optional_string(
    object: &Map<String, Value>,
    field: &str,
    max_bytes: usize,
) -> Result<(), ParameterError> {
    object
        .get(field)
        .map(|_| required_string(object, field, max_bytes).map(|_| ()))
        .unwrap_or(Ok(()))
}

/// Holds opaque trusted host correlations that never come from model tool arguments.
#[derive(Clone, Eq, PartialEq)]
pub struct TrustedTransport {
    request_id: String,
    correlation_id: String,
    opaque_attachment: String,
    /// Selected MCP host metadata, absent for generic transport-only callers.
    host_meta: Option<Value>,
}

impl std::fmt::Debug for TrustedTransport {
    /// Hides every private correlation, launcher attachment and populated host metadata field.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TrustedTransport(..)")
    }
}

impl TrustedTransport {
    /// Accepts bounded correlations only from trusted host-adapter ingress for one finite dispatch.
    ///
    /// This constructor does not establish identity or authority. Callers must not populate it
    /// from model parameters; Application treats all three values as opaque transport data.
    pub fn from_host_ingress(
        request_id: impl Into<String>,
        correlation_id: impl Into<String>,
        opaque_attachment: impl Into<String>,
    ) -> Option<Self> {
        let request_id = request_id.into();
        let correlation_id = correlation_id.into();
        let opaque_attachment = opaque_attachment.into();
        (!request_id.is_empty()
            && request_id.len() <= MAX_DETAIL_REF_BYTES
            && !correlation_id.is_empty()
            && correlation_id.len() <= MAX_DETAIL_REF_BYTES
            && !opaque_attachment.is_empty()
            && opaque_attachment.len() <= MAX_DETAIL_REF_BYTES)
            .then_some(Self {
                request_id,
                correlation_id,
                opaque_attachment,
                host_meta: None,
            })
    }
}

/// Reports the honest bounded outcome of facade routing without manufacturing peer readiness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FacadeOutcome {
    /// Model arguments did not satisfy the selected tool schema; no IPC was attempted.
    InvalidParameters,
    /// The daemon, IPC, or typed peer result was unavailable; native host work remains unblocked.
    Unavailable,
    /// IPC accepted the envelope but no typed peer result was available for safe rendering.
    Incomplete,
    /// Assistance explicitly identified the first unavailable peer boundary.
    PeerUnavailable(MissingPeer),
    /// The exact host binding was revoked without claiming a Workspace operation.
    HostStopped,
    /// Pending, typed failure or current owner result accepted from the daemon.
    Reply(PeerReply),
}

/// Owns one local facade endpoint and the finite limits for every connect-only dispatch.
#[derive(Clone, Debug)]
pub struct AssistanceFacade {
    /// Connect-only endpoint root, absent for a deliberately disconnected facade.
    runtime_dir: Option<PathBuf>,
    limits: HookTransportLimits,
}

impl AssistanceFacade {
    /// Creates a facade that never prepares a runtime directory or starts a daemon.
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            runtime_dir: Some(runtime_dir),
            limits: HookTransportLimits::new(128 * 1024, MAX_HOOK_BYTES, Duration::from_secs(1))
                .expect("fixed Assistance transport limits are valid"),
        }
    }

    /// Creates a facade that exposes static schemas but can never attempt local IPC.
    ///
    /// This is the managed-startup failure state: calls still receive bounded validation and an
    /// honest unavailable result, while no predictable or caller-derived socket path can receive
    /// trusted host metadata.
    pub fn unavailable() -> Self {
        Self {
            runtime_dir: None,
            limits: HookTransportLimits::new(128 * 1024, MAX_HOOK_BYTES, Duration::from_secs(1))
                .expect("fixed Assistance transport limits are valid"),
        }
    }

    /// Validates and sends exactly one current method through Application's finite dispatch envelope.
    ///
    /// Only a closed typed missing-peer reply is rendered; arbitrary transport acceptance remains
    /// `Incomplete` and never becomes a model-read, source-read, or peer-ready claim.
    pub async fn dispatch(
        &self,
        host: &TrustedTransport,
        tool: AssistanceTool,
        parameters: Value,
    ) -> FacadeOutcome {
        let Ok(call) = validate_call(tool, parameters) else {
            return FacadeOutcome::InvalidParameters;
        };
        let Some(runtime_dir) = &self.runtime_dir else {
            return FacadeOutcome::Unavailable;
        };
        let Some(parameters) = OpaqueJson::from_value(
            &json!({"parameters":call.parameters(),"host_meta":host.host_meta}),
            MAX_HOOK_BYTES,
        ) else {
            return FacadeOutcome::InvalidParameters;
        };
        let Some(request) = MethodDispatch::new(
            host.request_id.clone(),
            host.correlation_id.clone(),
            host.opaque_attachment.clone(),
            call.tool().transport_method(),
            parameters,
        ) else {
            return FacadeOutcome::Unavailable;
        };
        match dispatch_method_if_running(runtime_dir, request, self.limits).await {
            MethodDispatchTransportResult::Unavailable => FacadeOutcome::Unavailable,
            MethodDispatchTransportResult::Dispatched { opaque_result_json } => {
                match PeerReply::decode(opaque_result_json.as_str()) {
                    Some(PeerReply::Unavailable { reason }) => {
                        FacadeOutcome::PeerUnavailable(reason)
                    }
                    Some(PeerReply::HostStopped {}) if tool == AssistanceTool::Stop => {
                        FacadeOutcome::HostStopped
                    }
                    Some(reply @ (PeerReply::Pending { .. } | PeerReply::Error { .. })) => {
                        FacadeOutcome::Reply(reply)
                    }
                    Some(reply @ PeerReply::Edit { .. })
                        if matches!(tool, AssistanceTool::Edit | AssistanceTool::Inspect) =>
                    {
                        FacadeOutcome::Reply(reply)
                    }
                    Some(reply @ PeerReply::Complete { kind, .. })
                        if tool == AssistanceTool::Inspect
                            || matches!(
                                (tool, kind),
                                (AssistanceTool::Start, ResultKind::Activation)
                                    | (AssistanceTool::Context, ResultKind::Context)
                                    | (AssistanceTool::Diff, ResultKind::Diff)
                                    | (AssistanceTool::Stop, ResultKind::Stop)
                            ) =>
                    {
                        FacadeOutcome::Reply(reply)
                    }
                    _ => FacadeOutcome::Incomplete,
                }
            }
        }
    }
}

/// Reports the result of one fail-open native hook submission without blocking the host tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HookIngressOutcome {
    /// The bounded observation reached an already-running daemon; this does not prove model delivery.
    Submitted,
    /// A current bounded delta is eligible for this exact post-hook model boundary.
    Feedback(String),
    /// Parsing, connection, framing, dispatch, or reply was unavailable and the hook continued.
    Unavailable,
}

/// Parses one native hook and performs one connect-only `assistance.hook_submit` attempt.
///
/// This function never starts a daemon, scans a workspace, waits for LSP, retries inline, or
/// changes the native host tool's outcome. It serializes only the selected parsed hook fields.
pub async fn submit_inactive_hook(
    runtime_dir: &Path,
    host: &TrustedTransport,
    payload: &[u8],
) -> HookIngressOutcome {
    let Ok(event) = parse_hook_event(payload) else {
        return HookIngressOutcome::Unavailable;
    };
    submit_hook_event(runtime_dir, host, &event).await
}

/// Submits one already host-validated event using only its selected identity and lifecycle fields.
///
/// The serialized observation explicitly names its host contract. Claude session and optional
/// agent type are retained for isolation evidence; raw hook fields never enter the transport.
pub async fn submit_hook_event(
    runtime_dir: &Path,
    host: &TrustedTransport,
    event: &HookEvent,
) -> HookIngressOutcome {
    let observation = json!({
        "host": match event.host() {
            HostKind::Codex => "codex",
            HostKind::Claude => "claude",
        },
        "phase": match event.phase() {
            HookPhase::Pre => "pre",
            HookPhase::Post => "post",
            HookPhase::PostFailure => "post_failure",
            HookPhase::PermissionDenied => "permission_denied",
            HookPhase::PostBatch => "post_batch",
        },
        "actor_id": event.actor_id(),
        "call_id": event.optional_call_id(),
        "session_id": event.session_id(),
        "agent_type": event.agent_type(),
        // Present only for a Claude shell pre-hook. The daemon compares these bytes against a
        // command it generated itself and discards them otherwise; no other tool's arguments,
        // and no other field of this one, ever reach the transport.
        "launch_command": event.launch().map(HookLaunch::command),
        "launch_background": event.launch().map(HookLaunch::run_in_background),
        // Explicit post failure only; a post that carried no marker relays false, which means
        // "no failure was reported", not "success was proven".
        "failed": event.failed(),
    });
    let Some(observation) = OpaqueJson::from_value(&observation, MAX_HOOK_BYTES) else {
        return HookIngressOutcome::Unavailable;
    };
    let Some(request) = HookSubmit::new(
        host.request_id.clone(),
        host.correlation_id.clone(),
        host.opaque_attachment.clone(),
        observation,
    ) else {
        return HookIngressOutcome::Unavailable;
    };
    match submit_hook_if_running(
        runtime_dir,
        request,
        HookTransportLimits::new(128 * 1024, MAX_HOOK_BYTES, Duration::from_secs(1))
            .expect("fixed Assistance hook limits are valid"),
    )
    .await
    {
        HookSubmitTransportResult::Dispatched {
            opaque_reply_json, ..
        } => match PeerReply::decode(opaque_reply_json.as_str()) {
            Some(PeerReply::Feedback { text }) => HookIngressOutcome::Feedback(text),
            _ => HookIngressOutcome::Submitted,
        },
        HookSubmitTransportResult::Unavailable => HookIngressOutcome::Unavailable,
    }
}

/// Encodes one bounded post-hook delta in the model-context schema required by its explicit host.
///
/// Pre-hooks, empty/oversized text, invalid JSON serialization, and a `PermissionDenied` outcome
/// all produce no output. Both current host contracts use `hookSpecificOutput`, but the event name
/// is selected from the validated host event rather than copied from arbitrary input.
pub fn render_hook_context(event: &HookEvent, text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_FEEDBACK_BYTES {
        return None;
    }
    let hook_event_name = match event.phase() {
        HookPhase::Pre => return None,
        HookPhase::Post => "PostToolUse",
        HookPhase::PostFailure => "PostToolUseFailure",
        HookPhase::PermissionDenied => return None,
        HookPhase::PostBatch => "PostToolBatch",
    };
    serde_json::to_string(&json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": text,
        }
    }))
    .ok()
}

/// Stops the exact Assistance binding before asking Workspace to revoke that same authority stamp.
///
/// An old expected stamp cannot revoke a newer authority because Workspace compares the complete
/// current stamp. If binding revocation is unavailable, Workspace still fences the expected active
/// authority with `Missing` rather than pretending that Assistance completed the handoff.
pub fn stop_binding_then_revoke(
    bindings: &mut HostBindingGuard,
    authorities: &mut AuthorityRegistry,
    expected: &AuthorityStamp,
) -> Result<AuthorityRevoked, AuthorityError> {
    let handoff = match bindings.stop_binding(expected.binding()) {
        Ok(()) => StopBindingHandoff::Confirmed,
        Err(_) => StopBindingHandoff::Missing,
    };
    authorities.revoke(expected, handoff)
}

/// Content fingerprint of an ordered set of raw diagnostic messages.
///
/// Built from the exact typed messages a provider reported, never from any rendered fact text, so
/// it can be compared across two independent renderings of the same underlying issue. Any change
/// to the message set — added, removed or reworded diagnostics, even at an unchanged count —
/// yields a different fingerprint; an unchanged, re-observed set yields the same one regardless of
/// which job or detail reference produced it.
pub(super) fn diagnostic_fingerprint(messages: &[&str]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(messages.len() as u64).to_le_bytes());
    for message in messages {
        hasher.update(&(message.len() as u64).to_le_bytes());
        hasher.update(message.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Carries one bounded feedback fact and its rendering envelope before an external channel sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeedbackDelta {
    /// One new relevant fact, never a diagnostic dump.
    fact: String,
    /// Bounded evidence provenance for that fact.
    provenance: String,
    /// One practical next action or explicit absence of a safe action.
    next_action: String,
    /// Freshness or coverage qualifier that limits the fact's applicability.
    freshness: String,
    /// Optional owner-scoped detail reference; it is not expanded by this renderer.
    detail_ref: Option<String>,
}

impl FeedbackDelta {
    /// Validates the bounded factual envelope used for deduplication and later delivery.
    pub fn new(
        fact: impl Into<String>,
        provenance: impl Into<String>,
        next_action: impl Into<String>,
        freshness: impl Into<String>,
        detail_ref: Option<String>,
    ) -> Option<Self> {
        let fact = fact.into();
        let provenance = provenance.into();
        let next_action = next_action.into();
        let freshness = freshness.into();
        let fields = [&fact, &provenance, &next_action, &freshness];
        (!fields
            .iter()
            .any(|value| value.is_empty() || value.len() > MAX_TEXT_BYTES)
            && detail_ref
                .as_deref()
                .is_none_or(|value| !value.is_empty() && value.len() <= MAX_DETAIL_REF_BYTES))
        .then_some(Self {
            fact,
            provenance,
            next_action,
            freshness,
            detail_ref,
        })
    }

    /// Renders the one-fact envelope without claiming that a model read or acted on it.
    pub fn render(&self) -> String {
        let mut rendered = format!(
            "Fact: {}\nEvidence: {}\nNext: {}\nFreshness: {}",
            self.fact, self.provenance, self.next_action, self.freshness
        );
        if let Some(detail_ref) = &self.detail_ref {
            rendered.push_str("\nDetail: ");
            rendered.push_str(detail_ref);
        }
        rendered
    }
}

/// Tracks the explicit lifecycle of one feedback attempt without inferring model visibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackState {
    /// The fact is eligible for a later authority/source recheck.
    Pending,
    /// A delivery attempt was submitted to the selected host channel.
    Submitted,
    /// The host accepted or lost the attempt without evidence of model-context delivery.
    DeliveryUnknown,
    /// Authority stopped, source changed, or a newer fact replaced this entry before delivery.
    Superseded,
}

/// Identifies the observable result of recording one feedback delta.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedbackRecord {
    /// A new pending fact was retained for its authority scope.
    Pending,
    /// An equivalent relevant fact was already retained and was not duplicated.
    Deduplicated,
    /// The authority was already stopped, so no post-stop feedback was retained.
    Suppressed,
}

/// Keeps bounded per-authority feedback state and explicit deduplication outcomes.
#[derive(Debug, Default)]
pub struct FeedbackLedger {
    entries: BTreeMap<(String, String), FeedbackState>,
    stopped_authorities: BTreeSet<String>,
}

impl FeedbackLedger {
    /// Records one new fact when its authority is live and no equivalent source revision exists.
    pub fn record(
        &mut self,
        authority: impl Into<String>,
        source_revision: impl Into<String>,
        delta: &FeedbackDelta,
    ) -> FeedbackRecord {
        let authority = authority.into();
        let source_revision = source_revision.into();
        if authority.is_empty()
            || source_revision.is_empty()
            || self.stopped_authorities.contains(&authority)
        {
            return FeedbackRecord::Suppressed;
        }
        let key = (authority, format!("{source_revision}\u{0}{}", delta.fact));
        if self.entries.contains_key(&key) {
            return FeedbackRecord::Deduplicated;
        }
        self.entries.insert(key, FeedbackState::Pending);
        FeedbackRecord::Pending
    }

    /// Rechecks authority and source currency before marking one pending fact submitted.
    pub fn prepare_delivery(
        &mut self,
        authority: &str,
        source_revision: &str,
        fact: &str,
        authority_current: bool,
        source_current: bool,
    ) -> Option<FeedbackState> {
        let key = (
            authority.to_owned(),
            format!("{source_revision}\u{0}{fact}"),
        );
        let state = self.entries.get_mut(&key)?;
        if !authority_current || !source_current || self.stopped_authorities.contains(authority) {
            *state = FeedbackState::Superseded;
            return Some(*state);
        }
        if *state == FeedbackState::Pending {
            *state = FeedbackState::Submitted;
        }
        Some(*state)
    }

    /// Marks one submitted attempt as delivery-unknown without asserting the model read it.
    pub fn mark_delivery_unknown(
        &mut self,
        authority: &str,
        source_revision: &str,
        fact: &str,
    ) -> Option<FeedbackState> {
        let key = (
            authority.to_owned(),
            format!("{source_revision}\u{0}{fact}"),
        );
        let state = self.entries.get_mut(&key)?;
        if *state == FeedbackState::Submitted {
            *state = FeedbackState::DeliveryUnknown;
        }
        Some(*state)
    }

    /// Suppresses all current and later feedback for an authority after its stop boundary.
    pub fn stop_authority(&mut self, authority: impl Into<String>) {
        let authority = authority.into();
        self.stopped_authorities.insert(authority.clone());
        for ((entry_authority, _), state) in &mut self.entries {
            if entry_authority == &authority && *state == FeedbackState::Pending {
                *state = FeedbackState::Superseded;
            }
        }
    }
}

/// Hosts the static six-tool rmcp surface even when no trusted host attachment exists.
#[derive(Clone)]
pub struct StdioFacade {
    /// Connect-only Application endpoint and finite deadline.
    facade: AssistanceFacade,
    /// Host-launcher attachment, never populated from model tool arguments or request metadata.
    attachment: Option<String>,
    /// Generated static tool router; independent of daemon availability.
    router: rmcp::handler::server::tool::ToolRouter<Self>,
}

impl StdioFacade {
    /// Creates a stdio facade whose calls remain unavailable until a real host adapter supplies context.
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            facade: AssistanceFacade::new(runtime_dir),
            attachment: None,
            router: Self::tool_router(),
        }
    }

    /// Creates a disconnected six-tool facade for managed startup failure.
    ///
    /// Discovery remains static and calls validate normally before returning unavailable. The
    /// facade contains neither a host attachment nor an IPC path, so it cannot disclose request
    /// metadata to an unrelated local socket.
    pub fn unavailable() -> Self {
        Self {
            facade: AssistanceFacade::unavailable(),
            attachment: None,
            router: Self::tool_router(),
        }
    }

    /// Configures a bounded opaque attachment supplied separately by a trusted host launcher.
    ///
    /// Empty or oversized attachments are rejected. Each invocation additionally needs supported
    /// host request metadata; neither this attachment nor parsed metadata grants binding authority.
    pub fn with_host_attachment(runtime_dir: PathBuf, attachment: String) -> Option<Self> {
        TrustedTransport::from_host_ingress("validate", "validate", attachment.clone())?;
        Some(Self {
            facade: AssistanceFacade::new(runtime_dir),
            attachment: Some(attachment),
            router: Self::tool_router(),
        })
    }

    /// Validates model parameters before using separately supplied host metadata for finite IPC.
    ///
    /// Missing or invalid ingress performs no IPC. Valid ingress sends selected actor/call metadata;
    /// the daemon must establish its own host binding before returning any successful peer result.
    async fn call(
        &self,
        tool: AssistanceTool,
        parameters: Value,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        let outcome = match validate_call(tool, parameters.clone()) {
            Ok(_) => {
                let host = self.attachment.as_ref().and_then(|attachment| {
                    let (call_id, selected) = match parse_host_kind(&context.meta).ok()? {
                        HostKind::Codex => {
                            let candidate = parse_candidate(&context.meta).ok()?;
                            let mut selected = json!({"threadId":candidate.actor_id(),"callId":candidate.call_id(),"x-codex-turn-metadata":{}});
                            if let Some(state) = context.meta.get("codex/sandbox-state-meta") {
                                selected["codex/sandbox-state-meta"] = state.clone();
                            }
                            (candidate.call_id().to_owned(), selected)
                        }
                        HostKind::Claude => {
                            // Real Claude Code 2.1.267 MCP `_meta` carries only this call identity
                            // plus unrelated progress metadata; actor and sandbox values never do.
                            let call_id = parse_claude_call_id(&context.meta).ok()?;
                            let selected = json!({"claudecode/toolUseId":call_id});
                            (call_id, selected)
                        }
                    };
                    let mut host = TrustedTransport::from_host_ingress(
                        context.id.to_string(),
                        call_id,
                        attachment.clone(),
                    )?;
                    if serde_json::to_vec(&selected).ok()?.len() > MAX_HOOK_BYTES {
                        return None;
                    }
                    host.host_meta = Some(selected);

                    Some(host)
                });
                match host {
                    Some(host) => self.facade.dispatch(&host, tool, parameters).await,
                    None => FacadeOutcome::Unavailable,
                }
            }
            Err(_) => FacadeOutcome::InvalidParameters,
        };
        let message = match outcome {
            FacadeOutcome::Reply(reply) => return render_reply(reply),
            FacadeOutcome::InvalidParameters => {
                "invalid bounded parameters; inspect the tool schema"
            }
            FacadeOutcome::Unavailable => {
                "Assistance host attachment or daemon is unavailable; continue with native tools"
            }
            FacadeOutcome::Incomplete => {
                "typed Assistance peer result is unavailable; continue with native tools"
            }
            FacadeOutcome::HostStopped => {
                return CallToolResult::success(vec![ContentBlock::text(
                    "Assistance host binding stopped; no Workspace authority was created",
                )]);
            }
            FacadeOutcome::PeerUnavailable(MissingPeer::WorkspaceActivation) => {
                "Assistance unavailable: workspace_activation; continue with native tools"
            }
            FacadeOutcome::PeerUnavailable(MissingPeer::HostBinding) => {
                "Assistance unavailable: host_binding; continue with native tools"
            }
        };
        CallToolResult::error(vec![ContentBlock::text(message)])
    }
}

/// Renders the complete compact MCP result within the same exact envelope that retained Diff page
/// fitting uses.
///
/// [`content::render`] shrinks only owner Complete text at UTF-8 boundaries. Diff pages have already
/// passed [`content::fits`] without shrinking, so the facade never re-cuts an accepted whole hunk.
///
/// `pub(super)` so `worker::Shared::mark_feedback_inline_delivered` can trace the exact same
/// final carrier a live caller would receive, instead of re-approximating the fitting boundary.
pub(super) fn render_reply(reply: PeerReply) -> CallToolResult {
    content::render(reply).unwrap_or_else(|| {
        CallToolResult::error(vec![ContentBlock::text(
            "Assistance result exceeds the bounded envelope; continue with native tools",
        )])
    })
}

/// Ensures escaped compact text cannot defeat the actual serialized response budget.
#[test]
fn rendered_reply_bounds_the_complete_mcp_result() {
    let rendered = render_reply(PeerReply::Complete {
        kind: ResultKind::Context,
        text: "\0🦀\"\\".repeat(16000),
        detail_ref: Some("same-binding-detail".into()),
        truncated: false,
    });
    assert!(content::call_tool_result_fits(&rendered));
    assert_eq!(rendered.content.len(), 1);
    let result = rendered.structured_content.unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["detail_ref"], "same-binding-detail");
    assert!(result["text"].as_str().unwrap().contains('🦀'));
}

#[tool_router]
impl StdioFacade {
    /// Activates this actor/worktree once; call `ide.context` next before a native source edit.
    #[tool(name = "ide.start", input_schema = tool_schemas()[0].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn start(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Start, parameters, context).await
    }

    /// Reads bounded source and diagnostics before or after editing with the native host writer.
    #[tool(name = "ide.context", input_schema = tool_schemas()[1].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn context(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Context, parameters, context)
            .await
    }

    /// Reviews the accumulated native edits before the task finishes and `ide.stop` releases them.
    #[tool(name = "ide.diff", input_schema = tool_schemas()[2].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn diff(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Diff, parameters, context).await
    }

    /// Expands only a `detail_ref` returned by a pending or truncated IDE reply.
    #[tool(name = "ide.inspect", input_schema = tool_schemas()[3].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn inspect(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Inspect, parameters, context)
            .await
    }

    /// Releases this actor's IDE binding at task end or handoff; edited files remain on disk.
    #[tool(name = "ide.stop", input_schema = tool_schemas()[4].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn stop(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Stop, parameters, context).await
    }

    /// Applies one bounded full-content edit only through the active host-bound product route.
    #[tool(name = "ide.edit", input_schema = tool_schemas()[5].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn edit(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Edit, parameters, context).await
    }
}

/// Ensures diagnostics hide both launch attachments and populated host metadata in either format.
#[test]
fn debug_redacts_trusted_transport_and_host_metadata() {
    let secret = "private-debug-sentinel";
    let mut host = TrustedTransport::from_host_ingress(secret, secret, secret).unwrap();
    host.host_meta = Some(json!({"threadId":secret,"callId":secret,"private_json":secret}));
    for rendered in [format!("{host:?}"), format!("{host:#?}")] {
        assert!(
            !rendered.contains(secret) && !rendered.contains("private_json"),
            "private host Debug leaked"
        );
    }
}

#[rmcp::tool_handler(router = self.router)]
impl rmcp::ServerHandler for StdioFacade {
    /// Requests the measured Codex sandbox-state envelope without claiming its authority.
    fn get_info(&self) -> rmcp::model::ServerInfo {
        let mut experimental = rmcp::model::ExperimentalCapabilities::new();
        experimental.insert("codex/sandbox-state-meta".into(), Default::default());
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .enable_experimental_with(experimental)
                .build(),
        )
    }
}
