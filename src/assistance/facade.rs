//! Bounded MCP discovery, finite Application routing, and fail-open Assistance feedback.
//!
//! This module has no peer-domain implementation of its own. It validates the six logical tool
//! inputs, carries trusted host transport context, and honestly reports an unavailable or
//! incomplete result until Workspace, Intelligence, and Changes return their typed facts.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::RequestContext;
use rmcp::{RoleServer, tool, tool_router};
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::{
    app::{
        dispatch_method_if_running, submit_hook_if_running,
        transport::{
            AssistanceMethod, HookSubmit, HookSubmitTransportResult, HookTransportLimits,
            MethodDispatch, MethodDispatchTransportResult, OpaqueJson,
        },
    },
    assistance::{
        codex_rendezvous::{CodexRouteIdentity, ManagedCodexPublisher},
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
/// Maximum problems-page offset accepted from a model request (u32 range).
const MAX_PROBLEM_OFFSET: u64 = u32::MAX as u64;
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
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES},
                    "byte_offset": {"type": "integer", "minimum": 0, "maximum": MAX_BYTE_OFFSET},
                    "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES},
                    "kind": {"type": "string", "enum": ["problems"]},
                    "language": {"type": "string", "enum": ["rust", "python"]},
                    "offset": {"type": "integer", "minimum": 0, "maximum": MAX_PROBLEM_OFFSET}
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParameterError {
    /// The argument value was not an object under the hard byte limit.
    InvalidObject,
    /// The object named a field outside the selected method's closed schema.
    ///
    /// Carries the caller-supplied field name only when it passed the conservative
    /// `echoable_field` check; otherwise the refusal omits the name entirely.
    UnknownField(Option<String>),
    /// One named field violated one specific closed rule of the selected method.
    InvalidField {
        /// Facade-owned field name, always safe to echo back.
        field: &'static str,
        /// The exact closed rule the field value violated.
        rule: FieldRule,
    },
    /// `ide.context` named neither `path` nor `kind: "problems"`.
    ///
    /// The published schema stays a plain object (providers such as GLM drop a tool whose schema
    /// uses `allOf`/`if`/`else`), so this either-or rule lives here instead of in the schema.
    ContextTarget,
}

/// Names the specific closed rule one field value violated (T21B).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldRule {
    /// The closed method cannot proceed without this field.
    Required,
    /// The value must be a bounded nonempty string.
    NonEmptyString,
    /// The value must be a string; an empty one is allowed.
    String,
    /// The string exceeds its method-specific byte limit.
    TooLong(usize),
    /// The path must stay beneath the worktree root.
    RelativePath,
    /// A `..` path segment would leave the worktree root.
    NoDotDot,
    /// The string carries a NUL byte.
    NoNul,
    /// The value must be an integer between zero and the carried inclusive maximum.
    NonNegativeInteger(u64),
    /// The value must be one of the carried quoted alternatives.
    OneOf(&'static str),
    /// The field is meaningless without `kind: "problems"`.
    RequiresProblemsKind,
}

impl FieldRule {
    /// Renders the rule text appended after the quoted field name in one refusal line.
    fn text(self) -> String {
        match self {
            Self::Required => "is required".to_string(),
            Self::NonEmptyString => "must be a non-empty string".to_string(),
            Self::String => "must be a string".to_string(),
            Self::TooLong(limit) => format!("is longer than {limit} bytes"),
            Self::RelativePath => {
                "must be a path relative to the worktree root, not absolute".to_string()
            }
            Self::NoDotDot => "must not contain \"..\"".to_string(),
            Self::NoNul => "must not contain a NUL byte".to_string(),
            Self::NonNegativeInteger(limit) => {
                format!("must be a non-negative integer up to {limit}")
            }
            Self::OneOf(values) => format!("must be {values}"),
            Self::RequiresProblemsKind => "requires \"kind\":\"problems\"".to_string(),
        }
    }
}

impl ParameterError {
    /// Renders the single-line model-facing refusal naming exactly what to fix (T21B).
    ///
    /// Only caller field names that passed `echoable_field` are echoed back, and no field value
    /// is ever included, so the text stays safe and bounded under 256 bytes on one line.
    pub fn message(&self, tool: AssistanceTool) -> String {
        match self {
            Self::InvalidObject => format!(
                "invalid bounded parameters: arguments must be a JSON object under {} bytes",
                parameter_limit(tool)
            ),
            Self::UnknownField(name) => {
                let named = name
                    .as_ref()
                    .map(|name| format!(" \"{name}\""))
                    .unwrap_or_default();
                let allowed = allowed_fields(tool).join(", ");
                if allowed.is_empty() {
                    format!("invalid bounded parameters: unknown field{named}")
                } else {
                    format!("invalid bounded parameters: unknown field{named}; allowed: {allowed}")
                }
            }
            Self::InvalidField { field, rule } => {
                format!("invalid bounded parameters: \"{field}\" {}", rule.text())
            }
            Self::ContextTarget => CONTEXT_TARGET_MESSAGE.to_string(),
        }
    }
}

/// Model-facing text for a context request that names neither a path nor the problems kind.
const CONTEXT_TARGET_MESSAGE: &str =
    "invalid bounded parameters: ide.context needs either \"path\" or \"kind\":\"problems\"";

/// Returns the closed allowed field list for one logical tool.
fn allowed_fields(tool: AssistanceTool) -> &'static [&'static str] {
    match tool {
        AssistanceTool::Start => &["activation_id"],
        AssistanceTool::Context => &[
            "path",
            "byte_offset",
            "detail_ref",
            "kind",
            "language",
            "offset",
        ],
        AssistanceTool::Diff => &["mode", "detail_ref"],
        AssistanceTool::Inspect => &["detail_ref"],
        AssistanceTool::Stop => &[],
        AssistanceTool::Edit => &["operation_id", "path", "source_ref", "content"],
    }
}

/// Returns the hard argument-object byte limit for one logical tool.
fn parameter_limit(tool: AssistanceTool) -> usize {
    if tool == AssistanceTool::Edit {
        crate::changes::edit::MAX_EDIT_ARGUMENT_BYTES
    } else {
        MAX_PARAMETER_BYTES
    }
}

/// Echoes one caller-supplied field name only when it passes a conservative shape check.
fn echoable_field(field: &str) -> Option<String> {
    (!field.is_empty()
        && field.len() <= 32
        && field
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'))
    .then(|| field.to_string())
}

/// Builds one field-level rejection naming the violated closed rule.
fn invalid_field(field: &'static str, rule: FieldRule) -> ParameterError {
    ParameterError::InvalidField { field, rule }
}

/// Names the closed relative-path rule one already-bounded path string violated, if any.
fn path_shape_rule(path: &str) -> Option<FieldRule> {
    if path.as_bytes().contains(&0) {
        Some(FieldRule::NoNul)
    } else if path.split('/').any(|part| part == "..") {
        Some(FieldRule::NoDotDot)
    } else if path.split('/').any(|part| matches!(part, "" | ".")) {
        Some(FieldRule::RelativePath)
    } else {
        None
    }
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
    let parameter_limit = parameter_limit(tool);
    if serde_json::to_vec(&parameters)
        .ok()
        .is_none_or(|value| value.len() > parameter_limit)
    {
        return Err(ParameterError::InvalidObject);
    }
    let object = parameters
        .as_object()
        .ok_or(ParameterError::InvalidObject)?;
    let allowed = allowed_fields(tool);
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ParameterError::UnknownField(echoable_field(field)));
    }
    match tool {
        AssistanceTool::Start => {
            required_string(object, "activation_id", MAX_ACTIVATION_ID_BYTES)?;
        }
        AssistanceTool::Context => {
            let problems = match object.get("kind") {
                None => false,
                Some(kind) => match kind.as_str() {
                    Some("problems") => true,
                    // Any other kind value keeps the exact v0.2 context behaviour (EYES-r1 §7).
                    Some(_) => false,
                    None => return Err(invalid_field("kind", FieldRule::String)),
                },
            };
            if !problems && !object.contains_key("path") {
                return Err(ParameterError::ContextTarget);
            }
            if problems {
                if object
                    .get("language")
                    .is_some_and(|value| !matches!(value.as_str(), Some("rust" | "python")))
                {
                    return Err(invalid_field(
                        "language",
                        FieldRule::OneOf("\"rust\" or \"python\""),
                    ));
                }
                if object.get("offset").is_some_and(|value| {
                    value
                        .as_u64()
                        .is_none_or(|offset| offset > MAX_PROBLEM_OFFSET)
                }) {
                    return Err(invalid_field(
                        "offset",
                        FieldRule::NonNegativeInteger(MAX_PROBLEM_OFFSET),
                    ));
                }
            } else {
                // The problem-feed fields are meaningless without `kind: "problems"`; v0.2 keeps
                // its exact closed field set, so a request naming them there is rejected.
                if object.contains_key("language") || object.contains_key("offset") {
                    let field = if object.contains_key("language") {
                        "language"
                    } else {
                        "offset"
                    };
                    return Err(invalid_field(field, FieldRule::RequiresProblemsKind));
                }
            }
            if !problems || object.contains_key("path") {
                let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
                if let Some(rule) = path_shape_rule(path) {
                    return Err(invalid_field("path", rule));
                }
            }
            if object
                .get("byte_offset")
                .is_some_and(|value| value.as_u64().is_none_or(|offset| offset > MAX_BYTE_OFFSET))
            {
                return Err(invalid_field(
                    "byte_offset",
                    FieldRule::NonNegativeInteger(MAX_BYTE_OFFSET),
                ));
            }

            optional_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Diff => {
            optional_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
            if object.get("mode").is_some_and(|value| {
                !matches!(value.as_str(), Some("head" | "staged" | "unstaged"))
            }) {
                return Err(invalid_field(
                    "mode",
                    FieldRule::OneOf("\"head\", \"staged\", or \"unstaged\""),
                ));
            }
        }

        AssistanceTool::Inspect => {
            required_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Stop => {}
        AssistanceTool::Edit => {
            let operation_id = required_string(object, "operation_id", 128)?;
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            let request = crate::changes::edit::EditRequest::new(
                operation_id,
                path,
                required_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?,
                object
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_field("content", FieldRule::String))?,
            )
            .map_err(|error| match error {
                crate::changes::edit::EditRequestError::ContentTooLarge => invalid_field(
                    "content",
                    FieldRule::TooLong(crate::workspace::edit::MAX_EDIT_CONTENT_BYTES),
                ),
                // `operation_id` and `source_ref` bounds already held above and the path length
                // already held at 1024 bytes, so only the path shape can remain invalid here.
                crate::changes::edit::EditRequestError::InvalidArgument => invalid_field(
                    "path",
                    path_shape_rule(path).unwrap_or(FieldRule::RelativePath),
                ),
                crate::changes::edit::EditRequestError::ArgumentsTooLarge => {
                    ParameterError::InvalidObject
                }
            })?;
            parameters =
                serde_json::to_value(request).map_err(|_| ParameterError::InvalidObject)?;
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
    field: &'static str,
    max_bytes: usize,
) -> Result<&'a str, ParameterError> {
    let Some(value) = object.get(field).and_then(Value::as_str) else {
        return Err(invalid_field(
            field,
            if object.contains_key(field) {
                FieldRule::NonEmptyString
            } else {
                FieldRule::Required
            },
        ));
    };
    if value.is_empty() {
        return Err(invalid_field(field, FieldRule::NonEmptyString));
    }
    if value.len() > max_bytes {
        return Err(invalid_field(field, FieldRule::TooLong(max_bytes)));
    }
    Ok(value)
}

/// Reads one optional bounded nonempty string and rejects a present non-string or empty value.
fn optional_string(
    object: &Map<String, Value>,
    field: &'static str,
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
    /// Typed peer result accepted from the daemon, with its optional carried status plate (T28B).
    Reply(PeerReply, Option<String>),
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
        self.dispatch_validated(runtime_dir, host, call).await
    }

    /// Validates and sends exactly one current method against an explicit `runtime_dir`.
    ///
    /// Used by a caller that tracks its own live rendezvous target (e.g. after re-establishing a
    /// lost shared daemon) instead of the fixed directory this facade was constructed with.
    pub async fn dispatch_at(
        &self,
        runtime_dir: &Path,
        host: &TrustedTransport,
        tool: AssistanceTool,
        parameters: Value,
    ) -> FacadeOutcome {
        let Ok(call) = validate_call(tool, parameters) else {
            return FacadeOutcome::InvalidParameters;
        };
        self.dispatch_validated(runtime_dir, host, call).await
    }

    /// Shared tail of [`Self::dispatch`] and [`Self::dispatch_at`] once parameters are validated.
    async fn dispatch_validated(
        &self,
        runtime_dir: &Path,
        host: &TrustedTransport,
        call: ValidatedCall,
    ) -> FacadeOutcome {
        let tool = call.tool();
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
                match PeerReply::decode_delivered(opaque_result_json.as_str()) {
                    Some((
                        reply @ (PeerReply::Unavailable { .. }
                        | PeerReply::HostStopped {}
                        | PeerReply::Pending { .. }
                        | PeerReply::Error { .. }),
                        status,
                    )) => FacadeOutcome::Reply(reply, status),
                    Some((reply @ PeerReply::Edit { .. }, status))
                        if matches!(tool, AssistanceTool::Edit | AssistanceTool::Inspect) =>
                    {
                        FacadeOutcome::Reply(reply, status)
                    }
                    Some((reply @ PeerReply::Complete { kind, .. }, status))
                        if tool == AssistanceTool::Inspect
                            || matches!(
                                (tool, kind),
                                (AssistanceTool::Start, ResultKind::Activation)
                                    | (AssistanceTool::Context, ResultKind::Context)
                                    | (AssistanceTool::Diff, ResultKind::Diff)
                                    | (AssistanceTool::Stop, ResultKind::Stop)
                            ) =>
                    {
                        FacadeOutcome::Reply(reply, status)
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
/// The serialized observation explicitly names its host contract. The root session (retained for
/// both hosts) and Claude's optional agent type are kept for isolation evidence, and the post
/// phase tool name selects check triggers; raw hook fields never enter the transport. Only an
/// observed, settled, or feedback reply counts as submission; a daemon refusal is unavailable.
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
        // Post phases of either host: the bare native tool name that selects project-check triggers.
        "tool_name": event.tool_name(),
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
            Some(
                PeerReply::HookObserved {}
                | PeerReply::HookSettled {}
                | PeerReply::NativeHookObserved {},
            ) => HookIngressOutcome::Submitted,
            _ => HookIngressOutcome::Unavailable,
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

/// Redoes the shared-daemon launch-or-adopt rendezvous the exact way MCP startup performs it.
///
/// Returns the freshly reachable `(runtime_dir, attachment)` pair, or `None` if that rendezvous
/// itself failed; the caller then reports the call unavailable exactly as it would have without
/// ever attempting a reconnect.
pub type ReestablishFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<(PathBuf, String)>> + Send>> + Send + Sync>;

/// Shares one managed Codex publisher between the facade call path and MCP teardown (T29B §2).
///
/// The facade publishes actor routes before dispatching valid managed Codex calls; the MCP process
/// retires them at teardown and when its owned daemon exit is observed. The plain lock is only ever
/// held across bounded local filesystem work.
pub type SharedCodexPublisher = Arc<std::sync::Mutex<ManagedCodexPublisher>>;

/// Bounds how long dispatch waits for one route publication before continuing without it.
const PUBLICATION_WAIT: Duration = Duration::from_millis(25);

/// Test-only stall for the rendezvous filesystem paths, honored only when the product-test seam
/// `AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS` is set to a millisecond value (capped at one minute).
///
/// Both blocking rendezvous sites — the managed hook's route discovery and the MCP process's route
/// publication — sleep for this long before touching the filesystem, so deadline regressions can
/// inject a slow publisher or a stalled discovery without an artificially hostile filesystem.
/// Only product tests set the variable; production never does, and the hook's own 250 ms total
/// deadline and this module's [`PUBLICATION_WAIT`] bound the delay's observable effect either way.
pub(crate) fn stall_rendezvous_for_test() {
    let Some(milliseconds) = std::env::var_os("AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS")
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    std::thread::sleep(Duration::from_millis(milliseconds.min(60_000)));
}

/// Shares one live `(runtime_dir, attachment)` pair across every clone of a [`StdioFacade`].
///
/// A daemon this generation never owns (EYES-r2 §2) can exit while the MCP process keeps running
/// (idle timeout, `SIGTERM`, a crash, or a binary upgrade). Rather than caching a connection that
/// silently goes stale forever, every call reads the current pair and, on failure, re-runs
/// `reestablish` once and stores its result here for itself and every later call.
#[derive(Clone)]
struct ManagedConnection {
    current: Arc<Mutex<(PathBuf, String)>>,
    reestablish: ReestablishFn,
}

impl ManagedConnection {
    fn new(runtime_dir: PathBuf, attachment: String, reestablish: ReestablishFn) -> Self {
        Self {
            current: Arc::new(Mutex::new((runtime_dir, attachment))),
            reestablish,
        }
    }

    async fn current(&self) -> (PathBuf, String) {
        self.current.lock().await.clone()
    }

    async fn store(&self, runtime_dir: PathBuf, attachment: String) {
        *self.current.lock().await = (runtime_dir, attachment);
    }
}

/// Extracts the managed Codex route identity from the original trusted request `_meta`.
///
/// The root session is `_meta["x-codex-turn-metadata"].session_id` and the actor is
/// `_meta.threadId`, the same measured fields `parse_candidate` validates for the call lifecycle
/// (docs/host-probe.md, observed Codex field contract). `CodexRouteIdentity::new` re-validates both
/// components; anything missing, non-string, or out of bounds disables only publication.
fn codex_route_identity(meta: &Map<String, Value>) -> Option<CodexRouteIdentity> {
    let root_session = meta
        .get("x-codex-turn-metadata")?
        .get("session_id")?
        .as_str()?;
    let actor = meta.get("threadId")?.as_str()?;
    CodexRouteIdentity::new(root_session, actor).ok()
}

/// Hosts the static six-tool rmcp surface even when no trusted host attachment exists.
#[derive(Clone)]
pub struct StdioFacade {
    /// Connect-only Application endpoint and finite deadline.
    facade: AssistanceFacade,
    /// Host-launcher attachment, never populated from model tool arguments or request metadata.
    ///
    /// Unused (always `None`) once `reconnect` is set, which tracks its own current attachment.
    attachment: Option<String>,
    /// Live rendezvous target and re-establish hook for a shared daemon this facade does not own.
    reconnect: Option<ManagedConnection>,
    /// Managed Codex publisher owned by this MCP process; publication stays best-effort and quiet.
    ///
    /// Only the managed Codex constructor sets this. Publication never activates anything and is
    /// skipped entirely when the trusted request metadata lacks a root session or actor.
    publisher: Option<SharedCodexPublisher>,
    /// Generated static tool router; independent of daemon availability.
    router: rmcp::handler::server::tool::ToolRouter<Self>,
}

impl StdioFacade {
    /// Creates a stdio facade whose calls remain unavailable until a real host adapter supplies context.
    pub fn new(runtime_dir: PathBuf) -> Self {
        Self {
            facade: AssistanceFacade::new(runtime_dir),
            attachment: None,
            reconnect: None,
            publisher: None,
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
            reconnect: None,
            publisher: None,
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
            reconnect: None,
            publisher: None,
            router: Self::tool_router(),
        })
    }

    /// Configures the managed Codex facade: fixed attachment plus this process's route publisher.
    ///
    /// Identical bounds to [`Self::with_host_attachment`]. The publisher publishes actor routes
    /// before dispatching valid calls (T29B §2) but never activates anything: every publication
    /// failure, like every missing route identity, only skips publication.
    pub fn with_managed_codex(
        runtime_dir: PathBuf,
        attachment: String,
        publisher: SharedCodexPublisher,
    ) -> Option<Self> {
        TrustedTransport::from_host_ingress("validate", "validate", attachment.clone())?;
        Some(Self {
            facade: AssistanceFacade::new(runtime_dir),
            attachment: Some(attachment),
            reconnect: None,
            publisher: Some(publisher),
            router: Self::tool_router(),
        })
    }

    /// Configures a bounded opaque attachment for a shared daemon this facade can re-establish.
    ///
    /// Identical bounds to [`Self::with_host_attachment`], plus `reestablish` is stored for later
    /// calls to redo the launch-or-adopt rendezvous once a live connection is lost (EYES-r2 §2).
    pub fn with_reestablishing_attachment(
        runtime_dir: PathBuf,
        attachment: String,
        reestablish: ReestablishFn,
    ) -> Option<Self> {
        TrustedTransport::from_host_ingress("validate", "validate", attachment.clone())?;
        Some(Self {
            facade: AssistanceFacade::new(runtime_dir.clone()),
            attachment: None,
            reconnect: Some(ManagedConnection::new(runtime_dir, attachment, reestablish)),
            publisher: None,
            router: Self::tool_router(),
        })
    }

    /// Publishes this process's actor route for the managed Codex native hook, best-effort.
    ///
    /// The identity comes from the original trusted request `_meta` before projection: the root
    /// session is `_meta["x-codex-turn-metadata"].session_id` and the actor is `_meta.threadId`
    /// (docs/host-probe.md, observed Codex field contract). Missing identity, an invalid identity,
    /// or any publication error skips publication silently and leaves the call path unchanged.
    /// The wait on the blocking filesystem work is bounded: a stalled root or a contended
    /// publisher mutex abandons the wait after [`PUBLICATION_WAIT`] and dispatch continues, while
    /// the detached blocking task finishes on its own — it holds the publisher mutex only for its
    /// own duration — and a later call re-attempts the idempotent publication.
    async fn publish_codex_route(&self, meta: &Map<String, Value>) {
        let Some(publisher) = &self.publisher else {
            return;
        };
        let Some(identity) = codex_route_identity(meta) else {
            return;
        };
        let publisher = Arc::clone(publisher);
        let _ = tokio::time::timeout(PUBLICATION_WAIT, async move {
            let _ = tokio::task::spawn_blocking(move || {
                stall_rendezvous_for_test();
                publisher
                    .lock()
                    .expect("managed codex publisher mutex")
                    .publish(&identity)
            })
            .await;
        })
        .await;
    }

    /// Builds one trusted transport envelope from `attachment` and this call's host request metadata.
    ///
    /// `None` for missing/unsupported host metadata or an oversized selected fragment; this never
    /// grants binding authority, it only carries the identity the daemon must establish itself.
    fn build_host(
        &self,
        attachment: &str,
        context: &RequestContext<RoleServer>,
    ) -> Option<TrustedTransport> {
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
                // Real Claude Code 2.1.267 MCP `_meta` carries only this call identity plus
                // unrelated progress metadata; actor and sandbox values never do.
                let call_id = parse_claude_call_id(&context.meta).ok()?;
                let selected = json!({"claudecode/toolUseId":call_id});
                (call_id, selected)
            }
        };
        let mut host = TrustedTransport::from_host_ingress(
            context.id.to_string(),
            call_id,
            attachment.to_owned(),
        )?;
        if serde_json::to_vec(&selected).ok()?.len() > MAX_HOOK_BYTES {
            return None;
        }
        host.host_meta = Some(selected);
        Some(host)
    }

    /// Returns the current `(runtime_dir, attachment)` this facade should dispatch against.
    async fn current_connection(&self) -> Option<(PathBuf, String)> {
        match &self.reconnect {
            Some(reconnect) => Some(reconnect.current().await),
            None => Some((self.facade.runtime_dir.clone()?, self.attachment.clone()?)),
        }
    }

    /// Dispatches one already-validated call, re-establishing a lost shared daemon exactly once.
    ///
    /// A facade without `reconnect` (Codex, plain `--runtime-dir`, or startup failure) dispatches
    /// once, matching prior behaviour. A facade with `reconnect` additionally treats a transport
    /// `Unavailable` outcome as "the shared daemon may be gone": it re-runs the same launch-or-adopt
    /// rendezvous this facade started with, stores the refreshed pair for itself and every later
    /// call, and retries this one call exactly once more. There is no retry loop: a still-unavailable
    /// retry, or a failed rendezvous, returns the original outcome.
    ///
    /// The returned flag is true only when that retried dispatch actually ran (T08B): the new
    /// daemon has no pre-hook observation for the call whose hook fired before it existed, so this
    /// retried call's own outcome needs a retry hint even though the daemon itself is back.
    async fn dispatch_with_reconnect(
        &self,
        tool: AssistanceTool,
        parameters: Value,
        context: &RequestContext<RoleServer>,
    ) -> (FacadeOutcome, bool) {
        let Some((runtime_dir, attachment)) = self.current_connection().await else {
            return (FacadeOutcome::Unavailable, false);
        };
        let Some(host) = self.build_host(&attachment, context) else {
            return (FacadeOutcome::Unavailable, false);
        };
        // Managed Codex only: publish this process's route before the first dispatch of every
        // valid call. Idempotent, so retried calls after publication failure still publish.
        self.publish_codex_route(&context.meta).await;
        let outcome = self
            .facade
            .dispatch_at(&runtime_dir, &host, tool, parameters.clone())
            .await;
        let Some(reconnect) = &self.reconnect else {
            return (outcome, false);
        };
        if !matches!(outcome, FacadeOutcome::Unavailable) {
            return (outcome, false);
        }
        let Some((runtime_dir, attachment)) = (reconnect.reestablish)().await else {
            return (outcome, false);
        };
        reconnect
            .store(runtime_dir.clone(), attachment.clone())
            .await;
        let Some(host) = self.build_host(&attachment, context) else {
            return (outcome, false);
        };
        let retried = self
            .facade
            .dispatch_at(&runtime_dir, &host, tool, parameters)
            .await;
        (retried, true)
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
        let (outcome, reconnected) = match validate_call(tool, parameters.clone()) {
            Ok(_) => {
                self.dispatch_with_reconnect(tool, parameters, &context)
                    .await
            }
            Err(error) => {
                return CallToolResult::error(vec![ContentBlock::text(error.message(tool))]);
            }
        };
        // The Claude host hands `structuredContent` straight to its model in place of `content`,
        // defeating the compact renderer (T14B); its calls therefore never receive that duplicate
        // JSON copy. `parse_host_kind` reads the same trusted per-call `_meta` shape `build_host`
        // already establishes host identity from, so this holds for every managed Claude MCP mode
        // regardless of how the process itself was launched.
        let envelope = match parse_host_kind(&context.meta) {
            Ok(HostKind::Claude) => content::Envelope::TextOnly,
            _ => content::Envelope::WithStructured,
        };
        let message = match outcome {
            FacadeOutcome::Reply(reply, status) if reconnected => {
                return render_reply_after_reconnect(reply, status.as_deref(), envelope);
            }
            FacadeOutcome::Reply(reply, status) => {
                return render_reply_with_status(reply, status.as_deref(), envelope);
            }
            FacadeOutcome::InvalidParameters => {
                "invalid bounded parameters; inspect the tool schema"
            }
            FacadeOutcome::Unavailable => {
                "Assistance host attachment or daemon is unavailable; continue with native tools"
            }
            FacadeOutcome::Incomplete => {
                "typed Assistance peer result is unavailable; continue with native tools"
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
/// `envelope` selects whether the final carrier also duplicates the typed reply as
/// `structuredContent` (T14B): the Claude host hands that field straight to its model in place of
/// `content`, defeating the compact renderer, so its calls render with [`content::Envelope::TextOnly`].
/// Every other caller — Codex, and `worker::Shared::mark_feedback_inline_delivered` tracing the
/// exact final carrier a live caller would receive — keeps [`content::Envelope::WithStructured`].
///
/// `pub(super)` so `worker::Shared::mark_feedback_inline_delivered` can call it directly instead of
/// re-approximating the fitting boundary.
pub(super) fn render_reply(reply: PeerReply, envelope: content::Envelope) -> CallToolResult {
    render_reply_with_status(reply, None, envelope)
}

/// Like [`render_reply`], but a due status plate (T28B) leads the rendered reply.
///
/// `status` is the complete plate text a host without hook delivery attached at the daemon
/// boundary; [`content::render_with_status`] leads both the compact text and the structured
/// copy with it and never cuts it.
pub(super) fn render_reply_with_status(
    reply: PeerReply,
    status: Option<&str>,
    envelope: content::Envelope,
) -> CallToolResult {
    content::render_with_status(reply, status, envelope).unwrap_or_else(|| {
        crate::errorlog::record(
            crate::errorlog::Method::Client,
            crate::errorlog::Outcome::Failed,
            crate::errorlog::Fields {
                reason: Some(crate::errorlog::ReasonCode::OversizeEnvelope),
                ..Default::default()
            },
        );
        CallToolResult::error(vec![ContentBlock::text(
            "Assistance result exceeds the bounded envelope; continue with native tools",
        )])
    })
}

/// Stable text for [`render_reply_after_reconnect`]'s added `retry` field (T08B).
const RECONNECT_RETRY_HINT: &str = "daemon restarted; repeat this call once";

/// Renders exactly like [`render_reply`], except a host-binding-unavailable reply also tells the
/// agent to repeat the call once (T08B).
///
/// After [`StdioFacade::dispatch_with_reconnect`] re-establishes a lost shared daemon mid call,
/// that first retried dispatch has no pre-hook observation for the call whose hook fired before
/// the new daemon existed ([`crate::assistance::host_binding::BindingUnavailable::MissingPre`]),
/// so it reports the same host-binding-unavailable outcome an agent would otherwise see with no
/// daemon at all. An agent may not retry on its own and would stay without the IDE, so this keeps
/// the existing machine fields and adds a short stable `retry` hint instead. Every other reply
/// following a reconnect (including a still-unavailable transport outcome, which never reaches
/// this function) renders unchanged.
fn render_reply_after_reconnect(
    reply: PeerReply,
    status: Option<&str>,
    envelope: content::Envelope,
) -> CallToolResult {
    let is_host_binding_unavailable = matches!(
        reply,
        PeerReply::Unavailable {
            reason: MissingPeer::HostBinding
        }
    );
    let mut rendered = render_reply_with_status(reply, status, envelope);
    if !is_host_binding_unavailable {
        return rendered;
    }
    if let Some(Value::Object(fields)) = rendered.structured_content.as_mut() {
        fields.insert(
            "retry".to_owned(),
            Value::String(RECONNECT_RETRY_HINT.to_owned()),
        );
    }
    if let Some(ContentBlock::Text(text)) = rendered.content.first_mut() {
        text.text = format!("{}; retry: {RECONNECT_RETRY_HINT}", text.text);
    }
    rendered
}

/// Ensures escaped compact text cannot defeat the actual serialized response budget.
#[test]
fn rendered_reply_bounds_the_complete_mcp_result() {
    let rendered = render_reply(
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: "\0🦀\"\\".repeat(16000),
            detail_ref: Some("same-binding-detail".into()),
            truncated: false,
            continuation: false,
        },
        content::Envelope::WithStructured,
    );
    assert!(content::call_tool_result_fits(&rendered));
    assert_eq!(rendered.content.len(), 1);
    let result = rendered.structured_content.unwrap();
    assert_eq!(result["truncated"], true);
    assert_eq!(result["detail_ref"], "same-binding-detail");
    assert!(result["text"].as_str().unwrap().contains('🦀'));
}

/// Projects typed unavailable and stop lifecycle replies through the shared compact envelope.
#[test]
fn typed_lifecycle_replies_preserve_structured_content_without_transport_errors() {
    for reply in [
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
        },
        PeerReply::HostStopped {},
    ] {
        let expected = serde_json::to_value(&reply).unwrap();
        let rendered = render_reply(reply, content::Envelope::WithStructured);
        assert_eq!(rendered.content.len(), 1);
        assert_eq!(rendered.structured_content, Some(expected));
        assert_ne!(rendered.is_error, Some(true));
    }
}

/// The Claude host reads `structuredContent` straight into its model in place of `content`,
/// defeating the compact renderer; `render_reply` with [`content::Envelope::TextOnly`] must
/// therefore never populate it, for every reply state Claude can receive (T14B).
#[test]
fn claude_envelope_never_carries_structured_content() {
    for reply in [
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
        },
        PeerReply::HostStopped {},
        PeerReply::Pending {
            detail_ref: "detail-queued".into(),
            helper: Some("agent-ide claude-helper --claim detail-queued".into()),
        },
        PeerReply::Complete {
            kind: ResultKind::Activation,
            text: "durable capture true".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        },
    ] {
        let rendered = render_reply(reply, content::Envelope::TextOnly);
        assert_eq!(rendered.content.len(), 1);
        assert_eq!(rendered.structured_content, None);
    }
}

/// The T08B reconnect retry hint reaches the Claude host through `content` alone, since
/// [`content::Envelope::TextOnly`] never populates `structuredContent` for it to be inserted into.
#[test]
fn claude_envelope_reconnect_retry_hint_survives_in_content_text() {
    let rendered = render_reply_after_reconnect(
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
        },
        None,
        content::Envelope::TextOnly,
    );
    assert_eq!(rendered.structured_content, None);
    let ContentBlock::Text(text) = &rendered.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(RECONNECT_RETRY_HINT), "{}", text.text);
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

    /// Reads bounded source and diagnostics before or after editing with the native host writer;
    /// needs `path`, or `kind` set to `problems`.
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

/// Collects one T21B refusal per validation rule so every message names the field to fix.
#[cfg(test)]
fn t21b_refusals() -> Vec<(ParameterError, AssistanceTool, String)> {
    let oversized = json!({"path": "a".repeat(5000)});
    vec![
        (
            validate_call(
                AssistanceTool::Start,
                json!({"activation_id":"a","actor_id":"x"}),
            )
            .unwrap_err(),
            AssistanceTool::Start,
            "invalid bounded parameters: unknown field \"actor_id\"; allowed: activation_id"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Stop, json!({"authority":1}))
                .unwrap_err(),
            AssistanceTool::Stop,
            "invalid bounded parameters: unknown field \"authority\"".to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({"../escape":1})).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: unknown field; allowed: path, byte_offset, detail_ref, kind, language, offset"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Inspect, json!({})).unwrap_err(),
            AssistanceTool::Inspect,
            "invalid bounded parameters: \"detail_ref\" is required".to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Context,
                json!({"path": "/private/tmp/agent-ide-stability/agent-tasks/pyproject.toml"}),
            )
            .unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"path\" must be a path relative to the worktree root, not absolute"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({"path": "../secrets"})).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"path\" must not contain \"..\"".to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, oversized).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: arguments must be a JSON object under 4096 bytes"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({"path": "a".repeat(1025)})).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"path\" is longer than 1024 bytes".to_string(),
        ),
        (
            validate_call(AssistanceTool::Start, json!({"activation_id": ""})).unwrap_err(),
            AssistanceTool::Start,
            "invalid bounded parameters: \"activation_id\" must be a non-empty string".to_string(),
        ),
        (
            validate_call(AssistanceTool::Start, json!({"activation_id": 7})).unwrap_err(),
            AssistanceTool::Start,
            "invalid bounded parameters: \"activation_id\" must be a non-empty string".to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({"kind":"problems","offset":-1}))
                .unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"offset\" must be a non-negative integer up to 4294967295"
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Context,
                json!({"path":"src/main.rs","byte_offset":"soonest"}),
            )
            .unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"byte_offset\" must be a non-negative integer up to 1048576"
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Context,
                json!({"kind":"problems","language":"go"}),
            )
            .unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"language\" must be \"rust\" or \"python\"".to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({"path":"a.rs","offset":5})).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: \"offset\" requires \"kind\":\"problems\"".to_string(),
        ),
        (
            validate_call(AssistanceTool::Diff, json!({"mode":"all"})).unwrap_err(),
            AssistanceTool::Diff,
            "invalid bounded parameters: \"mode\" must be \"head\", \"staged\", or \"unstaged\""
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"a.rs","source_ref":"s","content":5}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "invalid bounded parameters: \"content\" must be a string".to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"a.rs","source_ref":"s","content":"x".repeat(49 * 1024)}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "invalid bounded parameters: \"content\" is longer than 49152 bytes".to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"/abs/a.rs","source_ref":"s","content":""}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "invalid bounded parameters: \"path\" must be a path relative to the worktree root, not absolute"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Context, json!({})).unwrap_err(),
            AssistanceTool::Context,
            "invalid bounded parameters: ide.context needs either \"path\" or \"kind\":\"problems\""
                .to_string(),
        ),
    ]
}

/// Every T21B refusal names the exact parameter to fix, stays single-line, and stays bounded.
#[test]
fn invalid_parameter_refusals_name_the_field_and_rule() {
    for (error, tool, expected) in t21b_refusals() {
        let message = error.message(tool);
        assert_eq!(message, expected);
        assert!(!message.contains('\n'), "refusal must stay single-line");
        assert!(message.len() < 256, "refusal must stay under 256 bytes");
    }
}

/// The observed live failure (an absolute `ide.context` path) now explains the relative-path rule.
#[test]
fn absolute_context_path_refusal_names_the_relative_path_rule() {
    let message = validate_call(
        AssistanceTool::Context,
        json!({"path": "/private/tmp/agent-ide-stability/agent-tasks/pyproject.toml"}),
    )
    .unwrap_err()
    .message(AssistanceTool::Context);
    assert_eq!(
        message,
        "invalid bounded parameters: \"path\" must be a path relative to the worktree root, not absolute"
    );
}

/// Caller field names are echoed only when they pass the conservative echo check (T21B).
#[test]
fn unknown_field_names_are_echoed_only_when_safe() {
    for (name, echoed) in [
        ("actor_id".to_string(), true),
        ("Actor_9".to_string(), true),
        ("a".repeat(32), true),
        ("a".repeat(33), false),
        ("actor-id".to_string(), false),
        ("../escape".to_string(), false),
        (String::new(), false),
    ] {
        let mut object = Map::new();
        object.insert("activation_id".to_string(), json!("a"));
        object.insert(name.clone(), json!("x"));
        let error = validate_call(AssistanceTool::Start, Value::Object(object)).unwrap_err();
        let ParameterError::UnknownField(carried) = error else {
            panic!("unknown field must be carried");
        };
        assert_eq!(carried.as_deref(), echoed.then_some(name.as_str()));
    }
}
