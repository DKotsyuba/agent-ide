//! Bounded MCP discovery, finite Application routing, and fail-open Assistance feedback.
//!
//! This module has no peer-domain implementation of its own. It validates the eleven logical tool
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
            HookEvent, HookPhase, HostBindingGuard, HostKind, parse_candidate,
            parse_claude_call_id, parse_hook_event, parse_host_kind,
        },
        reply::{
            FailureCode, HostBindingCause, MAX_FEEDBACK_BYTES, MissingPeer, PeerReply, ResultKind,
        },
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

/// Names the only logical MCP methods exposed by Assistance.
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
    /// Returns a file's skeleton: symbols with signatures and docs, no bodies.
    Outline,
    /// Returns one symbol's body or an explicit line range, numbered.
    Read,
    /// Returns a symbol card: definition, signature, docs, usages, callers.
    Symbol,
    /// Returns a bounded live call graph around one symbol.
    Graph,
    /// Runs or inspects one explicitly requested project test job.
    Test,
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
            Self::Outline => "ide.outline",
            Self::Read => "ide.read",
            Self::Symbol => "ide.symbol",
            Self::Graph => "ide.graph",
            Self::Test => "ide.test",
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
            Self::Outline => AssistanceMethod::Outline,
            Self::Read => AssistanceMethod::Read,
            Self::Symbol => AssistanceMethod::Symbol,
            Self::Graph => AssistanceMethod::Graph,
            Self::Test => AssistanceMethod::Test,
        }
    }
}

/// Checks that one completed result kind belongs to the called tool before the facade renders it.
fn tool_accepts_result_kind(tool: AssistanceTool, kind: ResultKind) -> bool {
    matches!(
        (tool, kind),
        (AssistanceTool::Start, ResultKind::Activation)
            | (AssistanceTool::Context, ResultKind::Context)
            | (AssistanceTool::Diff, ResultKind::Diff)
            | (AssistanceTool::Stop, ResultKind::Stop)
            | (AssistanceTool::Outline, ResultKind::Outline)
            | (AssistanceTool::Read, ResultKind::Read)
            | (AssistanceTool::Symbol, ResultKind::Symbol)
            | (AssistanceTool::Graph, ResultKind::Graph)
            | (AssistanceTool::Test, ResultKind::Test)
    )
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

/// Identifiers of the registered languages with project checks, in registration order: the
/// closed values of the `problems` language filter.
fn checked_language_ids() -> Vec<&'static str> {
    crate::lang::registered()
        .iter()
        .filter(|language| language.checks().is_some())
        .map(|language| language.name())
        .collect()
}

/// Joins `items` as an English list: `a`, `a or b`, `a, b<last> c` where `last` separates the
/// final item (`", or "` or `" or "`) when there are three or more.
fn alternatives(items: &[String], last: &str) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} or {second}"),
        [init @ .., tail] => format!("{}{last}{tail}", init.join(", ")),
    }
}

/// Fills the language-derived phrases of the tool descriptions from the registered languages.
///
/// `{project_checks}` becomes the check tools (`a, b or c`); `{manifests_head}` and
/// `{manifests_tail}` become the project manifests the card replaces reading, split before the
/// last one (`a, b` and `or c`) so the description keeps its line break there.
fn describe_languages(description: &str) -> String {
    let registered = crate::lang::registered();
    let checks = registered
        .iter()
        .filter_map(|language| language.checks())
        .map(|checks| checks.tool_name().to_owned())
        .collect::<Vec<_>>();
    let manifests = registered
        .iter()
        .filter_map(|language| language.descriptor().card_manifest)
        .collect::<Vec<_>>();
    let (head, tail) = match manifests.split_last() {
        None => (String::new(), String::new()),
        Some((last, [])) => (String::new(), (*last).to_owned()),
        Some((last, init)) => (init.join(", "), format!("or {last}")),
    };
    description
        .replace("{project_checks}", &alternatives(&checks, " or "))
        .replace("{manifests_head}", &head)
        .replace("{manifests_tail}", &tail)
}

/// Returns exactly the eleven current Assistance schemas regardless of daemon availability.
pub fn tool_schemas() -> [ToolSchema; 11] {
    [
        schema(
            AssistanceTool::Start,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["activation_id"],
                "properties": {
                    "activation_id": {"type": "string", "minLength": 1, "maxLength": MAX_ACTIVATION_ID_BYTES, "description": "Any stable id for this activation (e.g. the task name); repeating it returns the same activation."},
                    "root": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "Absolute working directory to activate; defaults to the host's project directory. Must lie below a configured allowed root."}
                }
            }),
        ),
        schema(
            AssistanceTool::Context,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "File relative to the project root; returns its source with diagnostics."},
                    "byte_offset": {"type": "integer", "minimum": 0, "maximum": MAX_BYTE_OFFSET, "description": "Continue a truncated source reply from this byte offset."},
                    "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "Reference from an earlier reply: continue that result."},
                    "kind": {"type": "string", "enum": ["problems"], "description": "`problems`: the project check results instead of a file."},
                    "language": {"type": "string", "enum": checked_language_ids(), "description": "With `problems`: limit to one language."},
                    "offset": {"type": "integer", "minimum": 0, "maximum": MAX_PROBLEM_OFFSET, "description": "With `problems`: continue from this problem index (see `next_offset`)."}
                }
            }),
        ),
        schema(
            AssistanceTool::Diff,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {"mode": {"type":"string","enum":["head","staged","unstaged"],"default":"head","description":"`head`: everything not yet committed; `staged` / `unstaged`: only that part."}, "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "Reference from an earlier reply: continue that result."}}
            }),
        ),
        schema(
            AssistanceTool::Inspect,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["detail_ref"],
                "properties": {"detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "Reference from an earlier reply: continue that result."}}
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
                "required": ["operation_id"],
                "properties": {
                    "operation_id": {"type": "string", "minLength": 1, "maxLength": 128, "description": "Any unique id for this edit; repeating it never applies the edit twice."},
                    "op": {"type": "string", "enum": ["replace", "insert", "delete", "rename"], "default": "replace", "description": "Symbol operation. replace: new body for `symbol` (or for `path`+`lines`); insert: new symbol placed `where` relative to `symbol`; delete: remove `symbol` with its header; rename: rename `symbol` project-wide to `new_name`."},
                    "symbol": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES, "description": "Symbol path `file#Owner/name`."},
                    "where": {"type": "string", "enum": ["before", "after", "first", "last"], "description": "For insert: before/after the anchor symbol, or first/last member of a container anchor."},
                    "new_name": {"type": "string", "minLength": 1, "maxLength": 128, "description": "For rename: the new identifier, applied project-wide."},
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES},
                    "lines": {"type": "string", "pattern": "^[0-9]+-[0-9]+$", "description": "With `path`: inclusive 1-based line range to replace."},
                    "source_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "Full-file form: the source_ref of the context/read this content is based on."},
                    "content": {"type": "string", "maxLength": MAX_EDIT_ARGUMENT_CONTENT_BYTES, "description": "Replacement or inserted code, including the symbol's doc comment and attributes."}
                }
            }),
        ),
        schema(
            AssistanceTool::Outline,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "File relative to the project root."}
                }
            }),
        ),
        schema(
            AssistanceTool::Read,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "symbol": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES, "description": "Symbol path `file#Owner/name`; returns its body with the doc header."},
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "File relative to the project root, with `lines`."},
                    "lines": {"type": "string", "pattern": "^[0-9]+-[0-9]+$", "description": "Inclusive 1-based line range such as `120-180`."}
                }
            }),
        ),
        schema(
            AssistanceTool::Symbol,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["symbol"],
                "properties": {
                    "symbol": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES, "description": "Symbol path `file#Owner/name`, or a bare name to search the project."},
                    "usages": {"type": "boolean", "default": true, "description": "Include usages (src/tests split, with source lines)."},
                    "callers": {"type": "integer", "minimum": 0, "maximum": 3, "default": 1, "description": "Callers to list (0 = none); use ide.graph for deeper trees."},
                    "callees": {"type": "integer", "minimum": 0, "maximum": 3, "default": 0, "description": "Callees to list (0 = none)."},
                    "history": {"type": "boolean", "default": false, "description": "Include the last commits touching the definition (opt-in)."}
                }
            }),
        ),
        schema(
            AssistanceTool::Graph,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["symbol"],
                "properties": {
                    "symbol": {"type":"string", "minLength":1, "maxLength":MAX_SYMBOL_PATH_BYTES, "description":"Symbol path `file#Owner/name`, or a bare name to search the project."},
                    "direction": {"type":"string", "enum":["callers", "callees", "both"], "default":"callers", "description":"`callers`: who calls it (blast radius); `callees`: what it calls; `both`."},
                    "depth": {"type":"integer", "minimum":1, "maximum":3, "default":2, "description":"Levels to expand; 2 is usually enough."},
                    "tests": {"type":"boolean", "default":false, "description":"Show test symbols as nodes (marked `[test]`) instead of collapsing them into one `+N tests` line per parent."}
                }
            }),
        ),
        schema(
            AssistanceTool::Test,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "symbol": {"type":"string", "minLength":1, "maxLength":MAX_SYMBOL_PATH_BYTES, "description":"Run the tests that reference this symbol (`file#Owner/name`)."},
                    "path": {"type":"string", "minLength":1, "maxLength":MAX_RELATIVE_PATH_BYTES, "description":"Run the tests in this file or directory."},
                    "pattern": {"type":"string", "minLength":1, "maxLength":MAX_TEXT_BYTES, "description":"Run tests whose name matches this substring (runner filter)."},
                    "command": {"type":"array", "minItems":1, "maxItems":64, "items":{"type":"string", "maxLength":MAX_TEXT_BYTES}, "description":"Explicit argv to run instead of the detected runner."},
                    "status": {"type":"integer", "minimum":1, "description":"Re-read run number N (from `tests #N`) instead of starting one."},
                    "budget_s": {"type":"integer", "minimum":1, "maximum":600, "default":120, "description":"Seconds before the run is stopped and reported as timed out."}
                }
            }),
        ),
    ]
}

/// Maximum bytes of a symbol path argument.
pub const MAX_SYMBOL_PATH_BYTES: usize = 1024;
/// Maximum bytes of the `content` argument on the wire; spliced whole files may be larger
/// internally (`crate::workspace::edit::MAX_EDIT_CONTENT_BYTES`).
pub const MAX_EDIT_ARGUMENT_CONTENT_BYTES: usize = 48 * 1024;

/// Parses an inclusive 1-based `start-end` line range; `None` for any other shape.
pub(crate) fn parse_line_range(text: &str) -> Option<crate::lang::LineRange> {
    let (start, end) = text.split_once('-')?;
    let start: u32 = start.parse().ok()?;
    let end: u32 = end.parse().ok()?;
    (start >= 1 && end >= start).then(|| crate::lang::LineRange::new(start, end))
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
    /// `ide.read` needs either `symbol` or both `path` and `lines`.
    ReadTarget,
    /// `ide.edit` needs `symbol` (with `op`), or `path` with `lines` for a range replace, or the
    /// full-file form `path` + `source_ref` + `content`.
    EditTarget,
    /// The language runner rejected a semantically unsupported test target.
    TestTargetUnsupported(String),
}

/// Names the specific closed rule one field value violated (T21B).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldRule {
    /// The closed method cannot proceed without this field.
    Required,
    /// The value must be a bounded nonempty string.
    NonEmptyString,
    /// The value must be an absolute path without empty, `.` or `..` segments.
    AbsolutePath,
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
    /// The value must be the identifier of a registered language with project checks.
    CheckedLanguage,
    /// The field is meaningless without `kind: "problems"`.
    RequiresProblemsKind,
    /// The value must be `true` or `false`.
    Boolean,
    /// The value must be an inclusive 1-based range `start-end` with `start <= end`.
    LineRange,
}

impl FieldRule {
    /// Renders the rule text appended after the quoted field name in one refusal line.
    fn text(self) -> String {
        match self {
            Self::Required => "is required".to_string(),
            Self::NonEmptyString => "must be a non-empty string".to_string(),
            Self::AbsolutePath => {
                "must be an absolute path without empty, \".\" or \"..\" segments".to_string()
            }
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
            Self::CheckedLanguage => format!(
                "must be {}",
                alternatives(
                    &checked_language_ids()
                        .iter()
                        .map(|id| format!("\"{id}\""))
                        .collect::<Vec<_>>(),
                    ", or ",
                )
            ),
            Self::RequiresProblemsKind => "requires \"kind\":\"problems\"".to_string(),
            Self::Boolean => "must be true or false".to_string(),
            Self::LineRange => "must be an inclusive 1-based range like 120-180".to_string(),
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
            Self::ReadTarget => "ide.read needs `symbol`, or `path` with `lines`".to_string(),
            Self::EditTarget => "ide.edit needs `symbol` with `op`, or `path` with `lines`, or `path` with `source_ref` and `content`".to_string(),
            Self::TestTargetUnsupported(message) => format!("invalid bounded parameters: {message}; use ide.test with `pattern` or `command`"),
        }
    }
}

/// Model-facing text for a context request that names neither a path nor the problems kind.
const CONTEXT_TARGET_MESSAGE: &str =
    "invalid bounded parameters: ide.context needs either \"path\" or \"kind\":\"problems\"";

/// Returns the closed allowed field list for one logical tool.
fn allowed_fields(tool: AssistanceTool) -> &'static [&'static str] {
    match tool {
        AssistanceTool::Start => &["activation_id", "root"],
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
        AssistanceTool::Edit => &[
            "operation_id",
            "path",
            "source_ref",
            "content",
            "op",
            "symbol",
            "where",
            "new_name",
            "lines",
        ],
        AssistanceTool::Outline => &["path"],
        AssistanceTool::Read => &["symbol", "path", "lines"],
        AssistanceTool::Symbol => &["symbol", "usages", "callers", "callees", "history"],
        AssistanceTool::Graph => &["symbol", "direction", "depth", "tests"],
        AssistanceTool::Test => &["symbol", "path", "pattern", "command", "status", "budget_s"],
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
        AssistanceTool::Outline => {
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            if let Some(rule) = path_shape_rule(path.strip_suffix('/').unwrap_or(path)) {
                return Err(invalid_field("path", rule));
            }
        }
        AssistanceTool::Read => {
            optional_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
            optional_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            optional_string(object, "lines", 32)?;
            match (
                object.contains_key("symbol"),
                object.contains_key("path"),
                object.contains_key("lines"),
            ) {
                (true, false, false) => {}
                (false, true, true) => {
                    let path = object["path"].as_str().unwrap_or_default();
                    if let Some(rule) = path_shape_rule(path) {
                        return Err(invalid_field("path", rule));
                    }
                    if parse_line_range(object["lines"].as_str().unwrap_or_default()).is_none() {
                        return Err(invalid_field("lines", FieldRule::LineRange));
                    }
                }
                _ => return Err(ParameterError::ReadTarget),
            }
        }
        AssistanceTool::Symbol => {
            required_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
            for field in ["usages", "history"] {
                if object.get(field).is_some_and(|value| !value.is_boolean()) {
                    return Err(invalid_field(field, FieldRule::Boolean));
                }
            }
            for field in ["callers", "callees"] {
                if object
                    .get(field)
                    .is_some_and(|value| value.as_u64().is_none_or(|depth| depth > 3))
                {
                    return Err(invalid_field(field, FieldRule::NonNegativeInteger(3)));
                }
            }
        }
        AssistanceTool::Graph => {
            required_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
            if object.get("direction").is_some_and(|value| {
                !matches!(value.as_str(), Some("callers" | "callees" | "both"))
            }) {
                return Err(invalid_field(
                    "direction",
                    FieldRule::OneOf("callers, callees, or both"),
                ));
            }
            if object
                .get("depth")
                .is_some_and(|value| value.as_u64().is_none_or(|depth| !(1..=3).contains(&depth)))
            {
                return Err(invalid_field("depth", FieldRule::NonNegativeInteger(3)));
            }
            if object.get("tests").is_some_and(|value| !value.is_boolean()) {
                return Err(invalid_field("tests", FieldRule::Boolean));
            }
        }
        AssistanceTool::Test => {
            let targets = ["symbol", "path", "pattern", "command", "status"]
                .into_iter()
                .filter(|field| object.contains_key(*field))
                .count();
            if targets != 1 {
                return Err(invalid_field(
                    "target",
                    FieldRule::OneOf("exactly one of symbol, path, pattern, command, or status"),
                ));
            }
            optional_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
            if let Some(path) = object.get("path") {
                let path = path
                    .as_str()
                    .ok_or_else(|| invalid_field("path", FieldRule::String))?;
                if path.len() > MAX_RELATIVE_PATH_BYTES {
                    return Err(invalid_field(
                        "path",
                        FieldRule::TooLong(MAX_RELATIVE_PATH_BYTES),
                    ));
                }
                if let Some(rule) = path_shape_rule(path) {
                    return Err(invalid_field("path", rule));
                }
            }
            optional_string(object, "pattern", MAX_TEXT_BYTES)?;
            if let Some(command) = object.get("command") {
                let argv = command
                    .as_array()
                    .filter(|argv| !argv.is_empty() && argv.len() <= 64)
                    .ok_or_else(|| {
                        invalid_field(
                            "command",
                            FieldRule::OneOf("a non-empty argv array with at most 64 entries"),
                        )
                    })?;
                if argv.iter().any(|arg| {
                    arg.as_str()
                        .is_none_or(|arg| arg.len() > MAX_TEXT_BYTES || arg.contains('\0'))
                }) {
                    return Err(invalid_field(
                        "command",
                        FieldRule::OneOf("strings up to 512 bytes without NUL"),
                    ));
                }
            }
            if object
                .get("status")
                .is_some_and(|value| value.as_u64().is_none_or(|id| id == 0))
            {
                return Err(invalid_field(
                    "status",
                    FieldRule::NonNegativeInteger(u64::MAX),
                ));
            }
            if object.get("budget_s").is_some_and(|value| {
                value
                    .as_u64()
                    .is_none_or(|budget| !(1..=600).contains(&budget))
            }) {
                return Err(invalid_field(
                    "budget_s",
                    FieldRule::NonNegativeInteger(600),
                ));
            }
        }
        AssistanceTool::Start => {
            required_string(object, "activation_id", MAX_ACTIVATION_ID_BYTES)?;
            optional_string(object, "root", MAX_RELATIVE_PATH_BYTES)?;
            if let Some(root) = object.get("root").and_then(Value::as_str)
                && (!root.starts_with('/')
                    || root
                        .split('/')
                        .skip(1)
                        .any(|segment| matches!(segment, "" | "." | "..")))
            {
                return Err(invalid_field("root", FieldRule::AbsolutePath));
            }
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
                if object.get("language").is_some_and(|value| {
                    value
                        .as_str()
                        .and_then(crate::assistance::problems::parse_language)
                        .is_none()
                }) {
                    return Err(invalid_field("language", FieldRule::CheckedLanguage));
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
        AssistanceTool::Edit if object.contains_key("symbol") || object.contains_key("lines") => {
            required_string(object, "operation_id", 128)?;
            let op = object
                .get("op")
                .map(|value| {
                    value
                        .as_str()
                        .ok_or_else(|| invalid_field("op", FieldRule::String))
                })
                .transpose()?
                .unwrap_or("replace");
            if !matches!(op, "replace" | "insert" | "delete" | "rename") {
                return Err(invalid_field(
                    "op",
                    FieldRule::OneOf("\"replace\", \"insert\", \"delete\", or \"rename\""),
                ));
            }
            if object.contains_key("symbol") {
                required_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
                if object.contains_key("path") || object.contains_key("lines") {
                    return Err(ParameterError::EditTarget);
                }
            } else {
                if op != "replace" {
                    return Err(ParameterError::EditTarget);
                }
                let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
                if let Some(rule) = path_shape_rule(path) {
                    return Err(invalid_field("path", rule));
                }
                if parse_line_range(required_string(object, "lines", 32)?).is_none() {
                    return Err(invalid_field("lines", FieldRule::LineRange));
                }
            }
            match op {
                "replace" | "insert" => {
                    let Some(content) = object.get("content").and_then(Value::as_str) else {
                        return Err(invalid_field("content", FieldRule::String));
                    };
                    if content.len() > MAX_EDIT_ARGUMENT_CONTENT_BYTES {
                        return Err(invalid_field(
                            "content",
                            FieldRule::TooLong(MAX_EDIT_ARGUMENT_CONTENT_BYTES),
                        ));
                    }
                }
                "delete" => {}
                _ => {
                    required_string(object, "new_name", 128)?;
                }
            }
            if op == "insert"
                && !object
                    .get("where")
                    .and_then(Value::as_str)
                    .is_some_and(|value| matches!(value, "before" | "after" | "first" | "last"))
            {
                return Err(invalid_field(
                    "where",
                    FieldRule::OneOf("\"before\", \"after\", \"first\", or \"last\""),
                ));
            }
            optional_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Edit => {
            let operation_id = required_string(object, "operation_id", 128)?;
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            if object
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.len() > MAX_EDIT_ARGUMENT_CONTENT_BYTES)
            {
                return Err(invalid_field(
                    "content",
                    FieldRule::TooLong(MAX_EDIT_ARGUMENT_CONTENT_BYTES),
                ));
            }
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
    /// Trusted host attachment or per-call metadata was absent; no IPC was attempted.
    MissingHostMetadata,
    /// The daemon, IPC, or typed peer result was unavailable; native host work remains unblocked.
    Unavailable,
    /// The daemon did not answer an admitted call in time; managed routes must not reconnect.
    TimedOut,
    /// Transport failed and this managed client could not re-establish a daemon.
    ReestablishFailed,
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
            MethodDispatchTransportResult::TimedOut => FacadeOutcome::TimedOut,
            MethodDispatchTransportResult::Dispatched { opaque_result_json } => {
                match PeerReply::decode_delivered(opaque_result_json.as_str()) {
                    Some((
                        reply @ (PeerReply::Unavailable { .. }
                        | PeerReply::HostStopped {}
                        | PeerReply::Pending { .. }
                        | PeerReply::Error { .. }
                        | PeerReply::InvalidParameters { .. }),
                        status,
                    )) => FacadeOutcome::Reply(reply, status),
                    Some((reply @ PeerReply::Edit { .. }, status))
                        if matches!(tool, AssistanceTool::Edit | AssistanceTool::Inspect) =>
                    {
                        FacadeOutcome::Reply(reply, status)
                    }
                    Some((reply @ PeerReply::Complete { kind, .. }, status))
                        if tool == AssistanceTool::Inspect
                            || tool_accepts_result_kind(tool, kind) =>
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

/// Re-establishes a managed daemon through its host-specific startup path.
///
/// Returns a reachable `(runtime_dir, attachment)` pair, or `None` if restart or rendezvous fails;
/// the caller then reports re-establishment failure without another retry.
pub type ReestablishFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<(PathBuf, String)>> + Send>> + Send + Sync>;

/// Re-roots one managed Claude session to the root directory an `ide.start {root}` named (T15B).
///
/// The closure receives the model's exact `root` argument, canonicalizes and admits it itself, and
/// attaches through the same path a fresh session in that directory would take.
pub type RerootFn =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = RerootOutcome> + Send>> + Send + Sync>;

/// Closed outcome of one managed Claude re-root attempt (T15B).
#[derive(Debug, Eq, PartialEq)]
pub enum RerootOutcome {
    /// The session is already bound to this exact root; dispatch proceeds unchanged.
    Unchanged,
    /// Attached to the requested root's daemon: the fresh pair and the bound candidate.
    Attached(PathBuf, String, PathBuf),
    /// The requested root resolves below no allowed root; the call proceeds against the current
    /// pair and the daemon's own admission answers.
    OutsideAllowedRoots,
    /// The allowed root's daemon could not be attached.
    Failed,
}

/// Shares one managed Codex publisher between the facade call path and MCP teardown (T29B §2).
///
/// The facade publishes actor routes before dispatching valid managed Codex calls; the MCP process
/// retires them at teardown and when its owned daemon exit is observed, then rebinds known routes
/// if the daemon restarts. The plain lock is only ever held across bounded local filesystem work.
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

/// One remembered successful activation, the facts a transparent re-activation replays.
#[derive(Clone, Debug)]
struct RememberedActivation {
    /// The model-supplied activation id of the successful `ide.start`.
    activation_id: String,
    /// The `root` argument that start carried, when it carried one.
    root: Option<String>,
}

/// Shares one live `(runtime_dir, attachment)` pair across every clone of a [`StdioFacade`].
///
/// A managed daemon can exit while its MCP process keeps running. Every call reads the current
/// pair and, on transport loss, re-runs `reestablish` once and stores its result for later calls.
/// A managed Claude session additionally tracks the canonical project it is bound to, may carry a
/// `reroot` hook that moves the whole pair (and the host's hook rendezvous) to another root an
/// `ide.start {root}` named (T15B), and remembers its last successful activation so a daemon
/// replacement can be recovered transparently (T15B restart recovery).
#[derive(Clone)]
struct ManagedConnection {
    current: Arc<Mutex<(PathBuf, String)>>,
    candidate: Arc<Mutex<Option<PathBuf>>>,
    /// The last successful activation of this session, when it ever activated.
    last_activation: Arc<Mutex<Option<RememberedActivation>>>,
    /// Set while a daemon replacement still needs its binding transparently re-activated.
    recovery_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Set from a daemon replacement until the next successful activation; references issued
    /// before it are the ones a replacement invalidated.
    replaced: Arc<std::sync::atomic::AtomicBool>,
    reroot: Option<RerootFn>,
    reestablish: ReestablishFn,
}

impl ManagedConnection {
    fn new(runtime_dir: PathBuf, attachment: String, reestablish: ReestablishFn) -> Self {
        Self {
            current: Arc::new(Mutex::new((runtime_dir, attachment))),
            candidate: Arc::new(Mutex::new(None)),
            last_activation: Arc::new(Mutex::new(None)),
            recovery_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            replaced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reroot: None,
            reestablish,
        }
    }

    /// Like [`Self::new`], for a managed Claude session that knows its bound project and can
    /// re-root to another admitted root (T15B).
    fn rerootable(
        runtime_dir: PathBuf,
        attachment: String,
        candidate: PathBuf,
        reestablish: ReestablishFn,
        reroot: RerootFn,
    ) -> Self {
        Self {
            candidate: Arc::new(Mutex::new(Some(candidate))),
            ..Self::new(runtime_dir, attachment, reestablish)
        }
        .with_reroot(reroot)
    }

    /// Returns this connection with one re-root hook installed.
    fn with_reroot(mut self, reroot: RerootFn) -> Self {
        self.reroot = Some(reroot);
        self
    }

    async fn current(&self) -> (PathBuf, String) {
        self.current.lock().await.clone()
    }

    async fn store(&self, runtime_dir: PathBuf, attachment: String) {
        *self.current.lock().await = (runtime_dir, attachment);
    }

    /// Returns the canonical project this session is currently bound to, when it knows one.
    async fn bound_candidate(&self) -> Option<PathBuf> {
        self.candidate.lock().await.clone()
    }

    /// Records the project a successful re-root bound the session to.
    async fn store_candidate(&self, candidate: PathBuf) {
        *self.candidate.lock().await = Some(candidate);
    }

    /// Marks this session's daemon as replaced, with its binding awaiting re-activation.
    fn mark_replaced(&self) {
        self.recovery_pending
            .store(true, std::sync::atomic::Ordering::Release);
        self.replaced
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Marks a successful activation: the binding is current again.
    ///
    /// `replaced` stays sticky: references issued before a replacement remain invalid forever,
    /// and only the session's explicit end forgets the remembered activation.
    fn mark_activated(&self) {
        self.recovery_pending
            .store(false, std::sync::atomic::Ordering::Release);
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

/// Hosts the static eleven-tool rmcp surface even when no trusted host attachment exists.
#[derive(Clone)]
pub struct StdioFacade {
    /// Connect-only Application endpoint and finite deadline.
    facade: AssistanceFacade,
    /// Host-launcher attachment, never populated from model tool arguments or request metadata.
    ///
    /// Unused (always `None`) once `reconnect` is set, which tracks its own current attachment.
    attachment: Option<String>,
    /// Live target and re-establish hook for a managed daemon that may be replaced.
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
            router: Self::described_tool_router(),
        }
    }

    /// Creates a disconnected eleven-tool facade for managed startup failure.
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
            router: Self::described_tool_router(),
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
            router: Self::described_tool_router(),
        })
    }

    /// Configures managed Codex route publication and one retry after its owned daemon exits.
    ///
    /// The reconnect hook replaces the daemon, its lease, and the publisher's route target before
    /// this facade retries the interrupted call. Invalid attachments still refuse construction.
    pub fn with_reestablishing_managed_codex(
        runtime_dir: PathBuf,
        attachment: String,
        publisher: SharedCodexPublisher,
        reestablish: ReestablishFn,
    ) -> Option<Self> {
        TrustedTransport::from_host_ingress("validate", "validate", attachment.clone())?;
        Some(Self {
            facade: AssistanceFacade::new(runtime_dir.clone()),
            attachment: None,
            reconnect: Some(ManagedConnection::new(runtime_dir, attachment, reestablish)),
            publisher: Some(publisher),
            router: Self::described_tool_router(),
        })
    }

    /// Configures a bounded opaque attachment for a managed daemon this facade can re-establish.
    ///
    /// Identical bounds to [`Self::with_host_attachment`], plus `reestablish` is stored for later
    /// calls to restore a lost connection through the host's startup path.
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
            router: Self::described_tool_router(),
        })
    }

    /// Configures one managed Claude session bound to `candidate` that can re-establish its shared
    /// daemon and re-root to another admitted root an `ide.start {root}` names (T15B).
    ///
    /// A host that moves a project never restarts this MCP process; the re-root hook re-attaches
    /// through the fresh-session path of the requested root before the call dispatches.
    pub fn with_reestablishing_claude_attachment(
        runtime_dir: PathBuf,
        attachment: String,
        candidate: PathBuf,
        reestablish: ReestablishFn,
        reroot: RerootFn,
    ) -> Option<Self> {
        TrustedTransport::from_host_ingress("validate", "validate", attachment.clone())?;
        Some(Self {
            facade: AssistanceFacade::new(runtime_dir.clone()),
            attachment: None,
            reconnect: Some(ManagedConnection::rerootable(
                runtime_dir,
                attachment,
                candidate,
                reestablish,
                reroot,
            )),
            publisher: None,
            router: Self::described_tool_router(),
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
                let selected = json!({"threadId":candidate.actor_id(),"callId":candidate.call_id(),"x-codex-turn-metadata":{}});
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

    /// Dispatches one already-validated call, re-establishing a lost managed daemon exactly once.
    ///
    /// A facade without `reconnect` (plain `--runtime-dir` or startup failure) dispatches
    /// once, matching prior behaviour. A facade with `reconnect` additionally treats a transport
    /// `Unavailable` outcome as "the daemon may be gone": it calls the host's restart or rendezvous
    /// hook, stores the refreshed pair for itself and every later call, and retries this call
    /// exactly once. A failed hook reports re-establishment failure; a still-unavailable retry
    /// reports transport unavailability. There is no retry loop.
    ///
    /// A managed Claude session first re-roots when this `ide.start` names another admitted root
    /// (T15B): the reroot hook attaches through the requested root's fresh-session path, stores the
    /// new pair for every later call, and this call then dispatches there. Its own pre-hook may
    /// have been refused before the re-root, so the first reply carries the re-root retry hint
    /// instead of a hard refusal; a re-root that cannot attach answers `project_moved` honestly.
    ///
    /// The returned [`Resume`] value names a target change: a transient timeout against the same
    /// live daemon must not claim it restarted. A replacement's or re-root's first dispatch may
    /// need the binding-recovery hint because its earlier pre-hook observation is gone.
    async fn dispatch_with_reconnect(
        &self,
        tool: AssistanceTool,
        parameters: Value,
        context: &RequestContext<RoleServer>,
    ) -> (FacadeOutcome, Resume) {
        let Some((runtime_dir, attachment)) = self.current_connection().await else {
            return (FacadeOutcome::MissingHostMetadata, Resume::Fresh);
        };
        let mut resume = Resume::Fresh;
        if let Some(reconnect) = &self.reconnect
            && let Some(reroot) = &reconnect.reroot
            && tool == AssistanceTool::Start
            && let Some(root) = parameters.get("root").and_then(Value::as_str)
        {
            match reroot(root.to_owned()).await {
                RerootOutcome::Unchanged | RerootOutcome::OutsideAllowedRoots => {}
                RerootOutcome::Attached(new_runtime, new_attachment, candidate) => {
                    if new_runtime != runtime_dir || new_attachment != attachment {
                        resume = Resume::Rerooted;
                    }
                    reconnect.store(new_runtime, new_attachment).await;
                    reconnect.store_candidate(candidate).await;
                }
                RerootOutcome::Failed => {
                    // Name both directories honestly; only an admitted root can be re-rooted.
                    let bound = reconnect.bound_candidate().await;
                    let cause = bound.map(|bound| HostBindingCause::project_moved(&bound, root));
                    return (
                        FacadeOutcome::Reply(
                            PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause,
                            },
                            None,
                        ),
                        resume,
                    );
                }
            }
        }
        let (runtime_dir, attachment) = if resume == Resume::Rerooted {
            let Some(current) = self.current_connection().await else {
                return (FacadeOutcome::MissingHostMetadata, resume);
            };
            current
        } else {
            (runtime_dir, attachment)
        };
        // T15B restart recovery: after a daemon replacement, the first call whose pre-hook
        // already reached the healed daemon transparently re-runs the remembered activation
        // before dispatching, so the session continues without the agent re-activating.
        if let Some(reconnect) = &self.reconnect
            && reconnect
                .recovery_pending
                .load(std::sync::atomic::Ordering::Acquire)
            && reconnect.last_activation.lock().await.is_some()
        {
            self.reactivate_remembered_binding(&runtime_dir, &attachment)
                .await;
        }
        let Some(host) = self.build_host(&attachment, context) else {
            return (FacadeOutcome::MissingHostMetadata, resume);
        };
        // Managed Codex only: publish this process's route before the first dispatch of every
        // valid call. Idempotent, so retried calls after publication failure still publish.
        self.publish_codex_route(&context.meta).await;
        let outcome = self
            .facade
            .dispatch_at(&runtime_dir, &host, tool, parameters.clone())
            .await;
        let Some(reconnect) = &self.reconnect else {
            return (outcome, resume);
        };
        if !matches!(outcome, FacadeOutcome::Unavailable) {
            return (outcome, resume);
        }
        let Some((new_runtime, new_attachment)) = (reconnect.reestablish)().await else {
            return (FacadeOutcome::ReestablishFailed, resume);
        };
        if new_runtime != runtime_dir || new_attachment != attachment {
            resume = Resume::Restarted;
        }
        reconnect
            .store(new_runtime.clone(), new_attachment.clone())
            .await;
        if resume == Resume::Restarted {
            // The replacement discarded this session's binding; the next call re-activates it.
            reconnect.mark_replaced();
        }
        let Some(host) = self.build_host(&new_attachment, context) else {
            return (outcome, resume);
        };
        self.publish_codex_route(&context.meta).await;
        let retried = self
            .facade
            .dispatch_at(&new_runtime, &host, tool, parameters)
            .await;
        (retried, resume)
    }

    /// Re-attaches after the shared daemon generation ended and marks the session for transparent
    /// re-activation (T15B restart recovery); called from the lease watcher the moment the held
    /// lease stream observes the daemon's end, and safe to repeat.
    ///
    /// Returns whether a live daemon is attached again. On success the remembered activation is
    /// replayed immediately; while no pre-hook has reached the healed daemon yet, that attempt
    /// waits inside its bounded arrival window and the session stays marked for the lazy
    /// re-activation the next dispatched call performs.
    pub async fn recover_lost_daemon(&self) -> bool {
        let Some(reconnect) = &self.reconnect else {
            return false;
        };
        reconnect.mark_replaced();
        let Some((runtime, attachment)) = (reconnect.reestablish)().await else {
            return false;
        };
        reconnect.store(runtime.clone(), attachment.clone()).await;
        self.reactivate_remembered_binding(&runtime, &attachment)
            .await;
        true
    }

    /// Re-runs the remembered activation through the ordinary `ide.start` path, with the trusted
    /// `claudecode/reactivation` host marker (T15B restart recovery).
    ///
    /// The daemon admits the root by the same `allowed_roots` rule as any start and binds from
    /// the actor of a genuine pre-hook the channel already delivered, so a session whose next
    /// pre arrived on the healed daemon re-activates without the agent doing anything.
    async fn reactivate_remembered_binding(&self, runtime_dir: &Path, attachment: &str) {
        let Some(reconnect) = &self.reconnect else {
            return;
        };
        let Some(remembered) = reconnect.last_activation.lock().await.clone() else {
            return;
        };
        let mut parameters = json!({"activation_id": remembered.activation_id});
        if let Some(root) = &remembered.root {
            parameters["root"] = json!(root);
        }
        // Each attempt carries a fresh call identity (a synthetic call never receives its own
        // post-hook, so its settling entry could not be reused), and an activation retry reuses
        // the committed facts of the one in flight, so a bounded second attempt settles a first
        // `pending` quickly.
        for attempt in 0..3 {
            let call = format!("reactivate-{}-{}", remembered.activation_id, attempt);
            let Some(mut host) =
                TrustedTransport::from_host_ingress(&call, &call, attachment.to_owned())
            else {
                return;
            };
            host.host_meta = Some(json!({
                "claudecode/toolUseId": call,
                "claudecode/reactivation": true,
            }));
            if let FacadeOutcome::Reply(
                PeerReply::Complete {
                    kind: ResultKind::Activation,
                    ..
                },
                _,
            ) = self
                .facade
                .dispatch_at(
                    runtime_dir,
                    &host,
                    AssistanceTool::Start,
                    parameters.clone(),
                )
                .await
            {
                reconnect.mark_activated();
                return;
            }
        }
    }

    /// Reports whether this session activated before and its daemon was replaced since.
    async fn binding_was_replaced(&self) -> bool {
        let Some(reconnect) = &self.reconnect else {
            return false;
        };
        reconnect
            .replaced
            .load(std::sync::atomic::Ordering::Acquire)
            && reconnect.last_activation.lock().await.is_some()
    }

    /// Reports whether one reply refuses a reference only a daemon replacement invalidated.
    ///
    /// Only a replaced daemon's `detail_ref`/`source_ref` reach this state on a healed session:
    /// references are boot-unique, and the flag clears at the next successful activation, so a
    /// merely mistyped reference keeps its ordinary error.
    async fn references_predate_replacement(&self, reply: &PeerReply) -> bool {
        self.binding_was_replaced().await
            && matches!(
                reply,
                PeerReply::Error {
                    code: FailureCode::InvalidDetail,
                    ..
                }
            )
    }

    /// Forgets the remembered activation after the session explicitly ended.
    async fn forget_remembered_activation(&self) {
        if let Some(reconnect) = &self.reconnect {
            *reconnect.last_activation.lock().await = None;
            reconnect.mark_activated();
        }
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
        let stage_parameters = parameters.clone();
        let (outcome, resume) = match validate_call(tool, parameters.clone()) {
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
        // Every failed reply names its stage: the failing path's own tag when it set one, else
        // the derived `<tool>:<reason>` default — the same tag the daemon journal records.
        let outcome = match outcome {
            FacadeOutcome::Reply(mut reply, status) => {
                if let PeerReply::Error { code, detail } = &mut reply
                    && detail.is_none()
                {
                    *detail = Some(staged_detail(tool, code, &stage_parameters));
                }
                FacadeOutcome::Reply(reply, status)
            }
            other => other,
        };
        // Remember every successful activation: its id and root are what a transparent
        // re-activation replays after a daemon replacement (T15B restart recovery).
        if let FacadeOutcome::Reply(
            PeerReply::Complete {
                kind: ResultKind::Activation,
                ..
            },
            _,
        ) = &outcome
            && let Some(reconnect) = &self.reconnect
            && let Some(activation_id) = stage_parameters
                .get("activation_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            *reconnect.last_activation.lock().await = Some(RememberedActivation {
                activation_id: activation_id.to_owned(),
                root: stage_parameters
                    .get("root")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            });
            reconnect.mark_activated();
        }
        // A stop that finds no binding because the daemon was replaced already achieved its
        // goal: the replacement revoked everything the stop would have revoked.
        let outcome = match outcome {
            FacadeOutcome::Reply(reply, status)
                if tool == AssistanceTool::Stop
                    && matches!(
                        reply,
                        PeerReply::Unavailable {
                            reason: MissingPeer::HostBinding,
                            ..
                        }
                    )
                    && self.binding_was_replaced().await =>
            {
                self.forget_remembered_activation().await;
                FacadeOutcome::Reply(
                    PeerReply::Complete {
                        kind: ResultKind::Stop,
                        text: "stopped (the IDE had already restarted)".into(),
                        detail_ref: None,
                        truncated: false,
                        continuation: false,
                    },
                    status,
                )
            }
            other => other,
        };
        let envelope = match parse_host_kind(&context.meta) {
            Ok(HostKind::Claude) => content::Envelope::TextOnly,
            _ => content::Envelope::WithStructured,
        };
        let message = match outcome {
            FacadeOutcome::Reply(reply, status) if resume != Resume::Fresh => {
                let note = self.references_predate_replacement(&reply).await;
                return note_replaced_references(
                    render_reply_after_reconnect(
                        tool,
                        reply,
                        status.as_deref(),
                        envelope,
                        resume == Resume::Rerooted,
                    ),
                    note,
                );
            }
            FacadeOutcome::Reply(reply, status) => {
                let note = self.references_predate_replacement(&reply).await;
                return note_replaced_references(
                    render_reply_with_status(reply, status.as_deref(), envelope),
                    note,
                );
            }
            FacadeOutcome::InvalidParameters => {
                "invalid bounded parameters; inspect the tool schema"
            }
            FacadeOutcome::MissingHostMetadata => {
                // A caller whose `_meta` names no supported host contract can never correlate an
                // invocation (the ZCode auto-mode case): name that closed cause rather than the
                // generic attachment text, which stays for a missing connection or attachment.
                if parse_host_kind(&context.meta).is_err() {
                    let reply = PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: Some(HostBindingCause::HostUnrecognized),
                    };
                    return render_reply_with_status(reply, None, envelope);
                }
                "Assistance host metadata or attachment is unavailable; continue with native tools"
            }
            FacadeOutcome::Unavailable => {
                "Assistance daemon transport is unavailable; continue with native tools"
            }
            FacadeOutcome::TimedOut => {
                "Assistance daemon transport timed out; continue with native tools"
            }
            FacadeOutcome::ReestablishFailed => {
                "Assistance daemon exited; re-establish failed; continue with native tools"
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
/// [`content::render_with_status`] shrinks only owner Complete text at UTF-8 boundaries. Diff pages have already
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
/// Recovery when a non-Start call reaches a replacement with no actor binding.
const RECONNECT_START_HINT: &str =
    "daemon restarted; call ide.start first, then repeat this call with fresh references";
/// Recovery after this session re-rooted to the requested root (T15B): the re-rooted call's own
/// pre-hook ran before the new rendezvous existed, so the next call pairs normally.
const REROOT_RETRY_HINT: &str = "session re-rooted to the requested root; repeat this call once";

/// A provider refusal's derived stage names the requested file's extension only, never its path,
/// and every other failure keeps the plain `<tool>:<reason>` default.
#[test]
fn provider_refusal_names_the_requested_file_extension() {
    assert_eq!(
        staged_detail(
            AssistanceTool::Outline,
            &FailureCode::ProviderUnavailable,
            &json!({"path": "docs/notes.md"}),
        ),
        "outline:provider_unavailable ext=md"
    );
    assert_eq!(
        staged_detail(
            AssistanceTool::Read,
            &FailureCode::ProviderUnavailable,
            &json!({"symbol": "src/app.gamma#Button/onClick"}),
        ),
        "read:provider_unavailable ext=gamma"
    );
    assert_eq!(
        staged_detail(
            AssistanceTool::Symbol,
            &FailureCode::ProviderUnavailable,
            &json!({"symbol": "#main"}),
        ),
        "symbol:provider_unavailable"
    );
    assert_eq!(
        staged_detail(
            AssistanceTool::Context,
            &FailureCode::ProviderLoading,
            &json!({"path": "src/main.rs"}),
        ),
        "context:provider_loading"
    );
    assert_eq!(
        staged_detail(
            AssistanceTool::Outline,
            &FailureCode::ProviderUnavailable,
            &json!({"path": "notes.über-big-extension"}),
        ),
        "outline:provider_unavailable"
    );
}

/// Renders like [`render_reply`], with a binding-recovery hint after a daemon replacement or
/// re-root.
///
/// After [`StdioFacade::dispatch_with_reconnect`] re-establishes a lost shared daemon mid call,
/// that first retried dispatch has no pre-hook observation for the call whose hook fired before
/// the new daemon existed ([`crate::assistance::host_binding::BindingUnavailable::MissingPre`]),
/// so it reports the same host-binding-unavailable outcome an agent would otherwise see with no
/// daemon at all. A Start call can be repeated after its new pre-hook; every other tool first needs
/// a fresh `ide.start` binding, and old detail/source references must be refreshed. A re-rooted
/// Start (T15B) says so instead of claiming a restart. This keeps the existing machine fields and
/// adds a short stable `retry` hint. Every other reply following a reconnect (including a
/// still-unavailable transport outcome, which never reaches this function) renders unchanged.
fn render_reply_after_reconnect(
    tool: AssistanceTool,
    reply: PeerReply,
    status: Option<&str>,
    envelope: content::Envelope,
    rerooted: bool,
) -> CallToolResult {
    let is_host_binding_unavailable = matches!(
        reply,
        PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            ..
        }
    );
    let mut rendered = render_reply_with_status(reply, status, envelope);
    if !is_host_binding_unavailable {
        return rendered;
    }
    let hint = if rerooted {
        REROOT_RETRY_HINT
    } else if tool == AssistanceTool::Start {
        RECONNECT_RETRY_HINT
    } else {
        RECONNECT_START_HINT
    };
    if let Some(Value::Object(fields)) = rendered.structured_content.as_mut() {
        fields.insert("retry".to_owned(), Value::String(hint.to_owned()));
    }
    if let Some(ContentBlock::Text(text)) = rendered.content.first_mut() {
        text.text = format!("{}; retry: {hint}", text.text);
    }
    rendered
}

/// Derives the default `<tool>:<reason>` stage, appending the requested file's extension for a
/// provider refusal so the journal names which file type found no server (T15B log follow-up).
///
/// The extension is the only path fragment — the journal's privacy rule keeps paths out — and
/// comes from the request's `path`, or a symbol path's file segment before its `#`.
pub(crate) fn staged_detail(
    tool: AssistanceTool,
    code: &FailureCode,
    parameters: &Value,
) -> String {
    let stage = crate::telemetry::adapters::default_stage(tool, code);
    if !matches!(code, FailureCode::ProviderUnavailable) {
        return stage;
    }
    let requested = parameters
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| {
            parameters
                .get("symbol")
                .and_then(Value::as_str)
                .map(|symbol| symbol.split('#').next().unwrap_or_default())
        })
        .map(Path::new);
    let Some(extension) = requested
        .and_then(Path::extension)
        .and_then(|ext| ext.to_str())
    else {
        return stage;
    };
    if !extension.is_empty()
        && extension.len() <= 16
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return format!("{stage} ext={extension}");
    }
    stage
}

/// Appends the explicit replaced-reference note to one rendered result, when due.
fn note_replaced_references(mut rendered: CallToolResult, note: bool) -> CallToolResult {
    if note && let Some(ContentBlock::Text(text)) = rendered.content.first_mut() {
        text.text
            .push_str(" (issued before the IDE restarted; re-read)");
    }
    rendered
}

/// Why one dispatch's target changed under the call, selecting its recovery hint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resume {
    /// The target pair did not change; the reply renders unchanged.
    Fresh,
    /// A lost daemon was re-established mid-call (T08B).
    Restarted,
    /// The session re-rooted to the requested root before this call (T15B).
    Rerooted,
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

/// Keeps every tool's inline completion kind in the same typed rendered envelope used by Inspect.
#[test]
fn inline_complete_kinds_keep_structured_content() {
    for (tool, kind) in [
        (AssistanceTool::Start, ResultKind::Activation),
        (AssistanceTool::Context, ResultKind::Context),
        (AssistanceTool::Diff, ResultKind::Diff),
        (AssistanceTool::Stop, ResultKind::Stop),
        (AssistanceTool::Outline, ResultKind::Outline),
        (AssistanceTool::Read, ResultKind::Read),
        (AssistanceTool::Symbol, ResultKind::Symbol),
        (AssistanceTool::Graph, ResultKind::Graph),
        (AssistanceTool::Test, ResultKind::Test),
    ] {
        let reply = PeerReply::Complete {
            kind,
            text: "complete".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        assert!(tool_accepts_result_kind(tool, kind));
        let encoded = reply.encode().unwrap();
        let (decoded, _) = PeerReply::decode_delivered(encoded.as_str()).unwrap();
        let rendered = render_reply(decoded, content::Envelope::WithStructured);
        assert_eq!(
            rendered.structured_content.unwrap()["kind"],
            format!("{kind:?}").to_lowercase()
        );
    }
    assert!(!tool_accepts_result_kind(
        AssistanceTool::Start,
        ResultKind::Outline
    ));
}

/// Projects typed unavailable and stop lifecycle replies through the shared compact envelope.
#[test]
fn typed_lifecycle_replies_preserve_structured_content_without_transport_errors() {
    for reply in [
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
            cause: None,
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
            cause: None,
        },
        PeerReply::HostStopped {},
        PeerReply::Pending {
            detail_ref: "detail-queued".into(),
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

/// Both reconnect recovery hints reach Claude through content alone; its envelope never populates
/// `structuredContent` for either hint to be inserted into. A re-rooted Start names the re-root,
/// not a restart (T15B).
#[test]
fn claude_envelope_reconnect_retry_hint_survives_in_content_text() {
    let rendered = render_reply_after_reconnect(
        AssistanceTool::Start,
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
            cause: None,
        },
        None,
        content::Envelope::TextOnly,
        false,
    );
    assert_eq!(rendered.structured_content, None);
    let ContentBlock::Text(text) = &rendered.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(RECONNECT_RETRY_HINT), "{}", text.text);
    let context = render_reply_after_reconnect(
        AssistanceTool::Context,
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
            cause: None,
        },
        None,
        content::Envelope::TextOnly,
        false,
    );
    assert_eq!(context.structured_content, None);
    let ContentBlock::Text(text) = &context.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(RECONNECT_START_HINT), "{}", text.text);
    let rerooted = render_reply_after_reconnect(
        AssistanceTool::Start,
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
            cause: None,
        },
        None,
        content::Envelope::TextOnly,
        true,
    );
    let ContentBlock::Text(text) = &rerooted.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(REROOT_RETRY_HINT), "{}", text.text);
    assert!(!text.text.contains(RECONNECT_RETRY_HINT), "{}", text.text);
}

impl StdioFacade {
    /// Builds the eleven-tool router with the language-derived description phrases filled in
    /// from the registered languages (see [`describe_languages`]).
    fn described_tool_router() -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            if let Some(description) = route.attr.description.as_mut() {
                *description = std::borrow::Cow::Owned(describe_languages(description));
            }
        }
        router
    }
}

#[tool_router]
impl StdioFacade {
    /// Activate Agent IDE for this project — once per task, before any other ide.* call. Returns
    /// a project card: languages with sizes, the build/check/test/lint commands, layout by
    /// directory, entry points and docs. Use it to orient instead of reading README, {manifests_head}
    /// {manifests_tail}. Then use ide.outline / ide.symbol instead of native file reads.
    #[tool(name = "ide.start", input_schema = tool_schemas()[0].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn start(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Start, parameters, context).await
    }

    /// Bounded source with current diagnostics for one `path`, or with `kind: "problems"` the
    /// project's latest check results (errors and warnings by file, paged). Use `problems` to
    /// see what is broken right now instead of running the build yourself; use `path` before or
    /// after editing a file natively.
    #[tool(name = "ide.context", input_schema = tool_schemas()[1].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn context(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Context, parameters, context)
            .await
    }

    /// Diff of what this task changed in the working tree (`head`, `staged` or `unstaged`),
    /// paged. Review it before finishing or handing off, instead of running `git diff` in a
    /// shell.
    #[tool(name = "ide.diff", input_schema = tool_schemas()[2].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn diff(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Diff, parameters, context).await
    }

    /// Skeleton of a file (every symbol with signature, doc line and line numbers, members
    /// indented, tests collapsed) or of a directory (files with line counts and first doc line).
    /// A fraction of the cost of reading the file — use it before any native read of a source
    /// file longer than a screen. Answers inline on a warm language server; a cold one answers
    /// `pending` — poll ide.inspect.
    #[tool(name = "ide.outline", input_schema = tool_schemas()[6].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn outline(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Outline, parameters, context)
            .await
    }

    /// Body of one symbol (`file#Owner/name`) or an explicit line range, numbered, with its doc
    /// header. The precise replacement for reading a whole file when you already know what you
    /// need; its `source_ref` is what a full-file ide.edit is based on.
    #[tool(name = "ide.read", input_schema = tool_schemas()[7].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn read(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Read, parameters, context).await
    }

    /// Symbol card for `file#Owner/name` or a bare name: resolved signature, doc, definition
    /// location, usages split src/tests with the source line, callers and callees as full symbol
    /// paths, and with `history: true` the last commits touching the definition. Language-server
    /// accurate — replaces grep for usages and reading files to find callers. A bare name with
    /// several matches returns the candidate paths.
    #[tool(name = "ide.symbol", input_schema = tool_schemas()[8].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn symbol(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Symbol, parameters, context).await
    }

    /// Bounded call graph around one symbol: callers, callees or both, depth 1–3, at most 60
    /// nodes, every node a full symbol path, cycles marked `(seen)`, tests collapsed into `+N tests` per parent (set `tests: true` to list them). Use
    /// it to see the blast radius before changing a function or to trace how a call reaches a
    /// symbol, instead of chained greps.
    #[tool(name = "ide.graph", input_schema = tool_schemas()[9].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn graph(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Graph, parameters, context).await
    }

    /// Run tests selected by `symbol` (the tests that reference it), by `path`, by name
    /// `pattern`, or an explicit `command`; one background run per worktree under `budget_s`.
    /// Returns the pass/fail line with an exact rerun command; full output is paged through
    /// ide.inspect (`status` re-reads a run). Use it instead of running the test command in a
    /// shell: exact selection, bounded output.
    #[tool(name = "ide.test", input_schema = tool_schemas()[10].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn test(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Test, parameters, context).await
    }

    /// Fetch the result behind a `detail_ref`: a `pending` reply that has since completed, or
    /// the next page of a long result (outline, symbol, graph, diff, test output). Poll every
    /// few seconds while it stays pending.
    #[tool(name = "ide.inspect", input_schema = tool_schemas()[3].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn inspect(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Inspect, parameters, context)
            .await
    }

    /// End this task's IDE session; edited files stay on disk. Call it once when the task is
    /// done or before handing off.
    #[tool(name = "ide.stop", input_schema = tool_schemas()[4].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn stop(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Stop, parameters, context).await
    }

    /// Edit by symbol: `op` replace / insert / delete / rename on `file#Owner/name` (rename is
    /// project-wide), or replace a `path` + `lines` range, or a full-file rewrite based on a
    /// `source_ref`. Formats the result with the project formatter, runs the project check
    /// ({project_checks}) and returns this file's errors and warnings in the reply.
    /// Prefer it over native edit/write for source: no line matching, no separate check step.
    #[tool(name = "ide.edit", input_schema = tool_schemas()[5].input_schema.as_object().expect("tool schema is an object").clone())]
    async fn edit(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Edit, parameters, context).await
    }
}

/// Connect-time instructions the host shows the model next to the tool list: the one-paragraph
/// case for the IDE over native file tools, and the two rules every reply relies on.
const SERVER_INSTRUCTIONS: &str = "Agent IDE: language-server-backed tools for source work. \
Call ide.start once per task, then prefer the ide.* tools over native file tools for source: \
ide.outline instead of reading a file, ide.symbol / ide.graph instead of grep for usages and \
callers, ide.read for one symbol's body, ide.edit for changes (formatted and project-checked in \
the same reply), ide.test to run exactly the tests that matter, ide.diff to review before \
ide.stop. Symbols are addressed as `file#Owner/name`. A reply that says `pending` is not an \
error: call ide.inspect with its detail_ref every few seconds until it completes. Native \
read/edit tools remain available for non-source files and as a fallback.";

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
    /// Advertises only the tool surface; no host sandbox metadata is requested.
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_instructions(SERVER_INSTRUCTIONS)
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
            "invalid bounded parameters: unknown field \"actor_id\"; allowed: activation_id, root"
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
                json!({"kind":"problems","language":"delta"}),
            )
            .unwrap_err(),
            AssistanceTool::Context,
            format!(
                "invalid bounded parameters: \"language\" must be {}",
                checked_language_alternatives()
            ),
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

/// Start accepts only absolute, normalized optional working-directory roots.
#[test]
fn start_root_must_be_absolute_and_normalized() {
    for root in ["relative", "/tmp/../outside", "/tmp//work"] {
        assert!(
            validate_call(
                AssistanceTool::Start,
                json!({"activation_id":"a","root":root})
            )
            .is_err()
        );
    }
    assert!(
        validate_call(
            AssistanceTool::Start,
            json!({"activation_id":"a","root":"/private/tmp/work"})
        )
        .is_ok()
    );
}

/// The quoted alternatives the `problems` language refusal lists: every registered checked
/// language in registration order, the last one after `, or `.
#[cfg(test)]
fn checked_language_alternatives() -> String {
    let quoted = checked_language_ids()
        .into_iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>();
    let (last, init) = quoted
        .split_last()
        .expect("checked test languages are registered");
    format!("{}, or {last}", init.join(", "))
}

/// Every T21B refusal names the exact parameter to fix, stays single-line, and stays bounded.
#[test]
fn invalid_parameter_refusals_name_the_field_and_rule() {
    crate::lang::testing::install();
    assert!(checked_language_alternatives().ends_with(", or \"gamma\""));
    for (error, tool, expected) in t21b_refusals() {
        let message = error.message(tool);
        assert_eq!(message, expected);
        assert!(!message.contains('\n'), "refusal must stay single-line");
        assert!(message.len() < 256, "refusal must stay under 256 bytes");
    }
}

/// The context validator accepts every checked language's problems filter and refuses a
/// registered language without checks.
#[test]
fn checked_language_problems_filter_is_admitted() {
    crate::lang::testing::install();
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"gamma"})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Context,
            json!({"kind":"problems","language":"delta"})
        )
        .is_err()
    );
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
