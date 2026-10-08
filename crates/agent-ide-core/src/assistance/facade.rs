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
use rmcp::model::{CacheScope, CallToolResult, ContentBlock, ListToolsResult, ProtocolVersion};
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
/// Maximum combined UTF-8 bytes across explicit `ide.test` argv entries.
const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Larger envelope for `ide.test` argv and its optional command environment.
const MAX_TEST_PARAMETERS_BYTES: usize = 24 * 1024;
/// Envelope for eight selectors, including worst-case JSON escaping and project-root keys.
const MAX_START_PARAMETERS_BYTES: usize = 64 * 1024;
/// Maximum variables accepted by the explicit test-command environment.
const MAX_ENV_ENTRIES: usize = 32;
/// Maximum UTF-8 bytes in one environment variable name.
const MAX_ENV_NAME_BYTES: usize = 128;
/// Maximum UTF-8 bytes in one environment variable value.
const MAX_ENV_VALUE_BYTES: usize = 4 * 1024;
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

    /// Reports whether this call can change binding, worktree or run state, so a reply lost after
    /// delivery must be reported as an unknown outcome rather than a call to simply repeat.
    pub const fn mutates(self) -> bool {
        matches!(self, Self::Start | Self::Stop | Self::Edit | Self::Test)
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
    let mut environment_languages = checked_language_ids();
    environment_languages.sort_unstable();
    let example_language = environment_languages.first().copied().unwrap_or("language");
    let environment_pattern = format!(
        r"^({})(:[^\u0000]{{1,512}})?$",
        environment_languages.join("|")
    );
    let environment_description = format!(
        "Pick the current environment per language, optionally per project root (`{example_language}:packages/alpha`). Value: a candidate the start card lists, or a path to one (relative to that root, or absolute), or `auto` to return to the default. Kept for this worktree until changed. Only languages whose card shows an environment line accept a choice."
    );
    [
        schema(
            AssistanceTool::Start,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "activation_id": {"type": "string", "minLength": 1, "maxLength": MAX_ACTIVATION_ID_BYTES, "description": "Any stable id for this activation (e.g. the task name); repeating it returns the same activation. Optional: a start that names none derives a stable id from this session, so repeating it also returns the same activation."},
                    "root": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "Absolute working directory to activate; defaults to the host's project directory. Must lie below a configured allowed root."},
                    "read_only": {"type": "boolean", "default": false, "description": "Start as a reader; the default is the one writer allowed per worktree."},
                    "environment": {"type": "object", "maxProperties": 8, "propertyNames": {"pattern": environment_pattern}, "additionalProperties": {"type": "string", "minLength": 1, "maxLength": 1024}, "description": environment_description}
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
                "properties": {"paths": {"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":1024},"description":"Limit capture and hunk budgets to these worktree-relative files or directories; literal paths only, no absolute paths or .. segments."}, "mode": {"type":"string","enum":["head","staged","unstaged","task"],"default":"head","description":"`head`: everything not yet committed; `staged` / `unstaged`: only that part; `task`: everything changed since activation, including commits."}, "detail_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "Reference from an earlier reply: continue that result."}, "provenance": {"type": "boolean", "default": false, "description": "Return the exact worktree/comparison identity header instead of the compact default; no hashes appear otherwise."}}
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
                "properties": {
                    "activation_id": {"type": "string", "minLength": 1, "maxLength": MAX_ACTIVATION_ID_BYTES, "description": "Optional, accepted for symmetry with ide.start; stop always ends this session's activation."}
                }
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
                    "source_ref": {"type": "string", "minLength": 1, "maxLength": MAX_DETAIL_REF_BYTES, "description": "The source_ref of the ide.read/ide.context this content is based on; required with `path`+`lines` and to replace an existing file with `path`+`content`, optional (but validated) with `symbol`. Omit it with `path`+`content` to create a new file."},
                    "content": {"type": "string", "maxLength": MAX_EDIT_ARGUMENT_CONTENT_BYTES, "description": "Replacement or inserted code, including the symbol's doc comment and attributes."},
                    "changes": {"type": "array", "minItems": 1, "maxItems": 32, "items": {"type": "object", "additionalProperties": false, "properties": {"lines": {"type": "string", "pattern": "^[0-9]+-[0-9]+$"}, "symbol": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES}, "op": {"type": "string", "enum": ["replace", "insert", "delete"]}, "where": {"type": "string", "enum": ["before", "after", "first", "last"]}, "content": {"type": "string"}, "old": {"type": "string", "minLength": 1}, "new": {"type": "string"}, "within": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES}}, "description": "1–32 changes to ONE file (named by `path`), each addressed by exactly one of `lines`, `symbol` or `old`; every address resolves against the bytes `source_ref` names before anything is applied, and overlapping or unmatched addresses are refused together with nothing written. `lines` numbers are that version's, never shifted by other changes (empty `content` deletes the range); `old` must match exactly once (add `within` to scope it) and `new` replaces exactly the matched bytes, terminators included (`\"\"` deletes the match)."}}
                }
            }),
        ),
        schema(
            AssistanceTool::Outline,
            json!({
                "type": "object", "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "File relative to the project root."},
                    "kinds": {"type": "string", "minLength": 1, "maxLength": 256, "pattern": "^(module|namespace|struct|enum|class|interface|trait|impl|type|fn|method|constructor|field|variant|const|var|test|symbol)(,(module|namespace|struct|enum|class|interface|trait|impl|type|fn|method|constructor|field|variant|const|var|test|symbol))*$", "description": "Comma list over the closed symbol-kind vocabulary; keeps a symbol whose kind is selected or a descendant's is, so a container of a selected member still shows."}
                }
            }),
        ),
        schema(
            AssistanceTool::Read,
            json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "symbol": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES, "description": "Symbol path `file#Owner/name`; returns its body with the doc header."},
                    "path": {"type": "string", "minLength": 1, "maxLength": MAX_RELATIVE_PATH_BYTES, "description": "File relative to the project root; alone it reads the whole file, with `lines` or `ranges` those lines."},
                    "lines": {"type": "string", "pattern": "^[0-9]+-[0-9]+$", "description": "Inclusive 1-based line range such as `120-180`."},
                    "symbols": {"type": "array", "minItems": 1, "maxItems": 16, "items": {"type": "string", "minLength": 1, "maxLength": MAX_SYMBOL_PATH_BYTES}, "description": "Several bodies in one reply, request order, one `source_ref` valid for every file included: `[\"src/a.rs#Foo/bar\",\"src/b.rs#qux\"]`. Unknown symbols are reported per item without failing the rest; a bare file path reads the text of a file no IDE language reads."},
                    "ranges": {"type": "array", "minItems": 1, "maxItems": 16, "items": {"type": "string", "pattern": "^[0-9]+-[0-9]+$"}, "description": "Several line ranges of `path` in one reply, request order, with the same numbered gutter and one `source_ref`."}
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
                    "callers": {"type": "integer", "minimum": 0, "maximum": 3, "default": 1, "description": "Callers to list, 0–3 (0 = none); use ide.graph for deeper trees."},
                    "callees": {"type": "integer", "minimum": 0, "maximum": 3, "default": 0, "description": "Callees to list, 0–3 (0 = none)."},
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
                    "command": {"type":"array", "minItems":1, "maxItems":64, "items":{"type":"string", "maxLength":MAX_COMMAND_BYTES}, "description":"Explicit argv to run instead of the detected runner; all argument bytes together are limited to 16 KiB."},
                    "cwd": {"type":"string", "minLength":1, "maxLength":MAX_RELATIVE_PATH_BYTES, "description":"Working directory relative to the worktree root; must resolve inside the worktree. Only with `command`."},
                    "env": {"type":"object", "maxProperties":MAX_ENV_ENTRIES, "additionalProperties":{"type":"string", "maxLength":MAX_ENV_VALUE_BYTES}, "description":"Environment variables added to the inherited environment for this explicit command. Only with `command`; names are bounded environment identifiers."},
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
pub const MAX_EDIT_ARGUMENT_CONTENT_BYTES: usize = 128 * 1024;
/// Most entries one `ide.edit` `changes` array may hold.
pub const MAX_EDIT_CHANGES: usize = 32;
/// Most entries one `ide.read` `symbols` array or `ranges` array may hold.
pub const MAX_READ_ADDRESSES: usize = 16;

/// Reads one bounded optional array of nonempty strings from a closed method object: `None` when
/// the field is absent, the clipped list when present, with the field's own shape refusal
/// otherwise. `bound` bounds both the entry count and each entry's bytes.
fn string_list(
    object: &Map<String, Value>,
    field: &'static str,
    max: usize,
) -> Result<Option<Vec<String>>, ParameterError> {
    let Some(list) = object.get(field) else {
        return Ok(None);
    };
    let list = list
        .as_array()
        .filter(|list| !list.is_empty() && list.len() <= max)
        .ok_or_else(|| invalid_field(field, FieldRule::OneOf(LIST_OF_STRINGS)))?;
    list.iter()
        .map(|entry| {
            let text = entry
                .as_str()
                .filter(|text| !text.is_empty() && text.len() <= max * 64);
            text.map(str::to_owned)
                .ok_or_else(|| invalid_field(field, FieldRule::OneOf("non-empty string entries")))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Closed rule text for a bounded list argument of strings.
const LIST_OF_STRINGS: &str = "an array of 1 to 16 non-empty strings";

/// The `symbols` list: each entry a `path#Owner/name` (a nonempty file part before the `#`) or a
/// bare relative file path with no `#` (the whole file's text, for a file no IDE language reads),
/// never a sigil address — those stay on the single `symbol` form.
fn symbol_list(
    object: &Map<String, Value>,
    field: &'static str,
    max: usize,
) -> Result<Option<Vec<String>>, ParameterError> {
    let list = string_list(object, field, max)?;
    if let Some(list) = &list
        && list.iter().any(|entry| {
            crate::lang::SymbolPath::parse_item(entry).map_or(true, |symbol| {
                symbol.file().is_none() || (symbol.segments().is_empty() && entry.contains('#'))
            })
        })
    {
        return Err(invalid_field(
            field,
            FieldRule::OneOf(
                "symbol paths like src/x.rs#Owner/name, or a bare file path (its text, for a file no IDE language reads)",
            ),
        ));
    }
    Ok(list)
}

/// The `ranges` list: each entry an inclusive 1-based `start-end` range.
fn range_list(
    object: &Map<String, Value>,
    field: &'static str,
    max: usize,
) -> Result<Option<Vec<String>>, ParameterError> {
    let list = string_list(object, field, max)?;
    if let Some(list) = &list
        && list.iter().any(|range| parse_line_range(range).is_none())
    {
        return Err(invalid_field(field, FieldRule::LineRange));
    }
    Ok(list)
}

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
    /// `ide.outline` requires a relative file or directory path.
    OutlineTarget,
    /// `ide.read` needs exactly one of: `symbol`, `path` alone (the whole file), `path` with
    /// `lines`, `path` with `ranges`, or `symbols`.
    ReadTarget,
    /// `ide.edit` needs `symbol` (with `op`), or `path` with `lines` and `source_ref` for a range
    /// replace, or the full-file form `path` + `content` (with `source_ref` to replace an existing
    /// file, without it only to create a missing one); the forms are exclusive, except a redundant
    /// `path` naming the symbol's own file.
    EditTarget,
    /// `ide.edit` with `changes` needs `path` (one file) and no single-change form beside it.
    ChangesTarget,
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
    /// The value must fall within one closed inclusive integer range.
    IntegerRange {
        /// Lowest accepted integer.
        min: u64,
        /// Highest accepted integer.
        max: u64,
    },
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
    /// The closed method cannot proceed without the `source_ref` of the read its lines came from.
    RangeEditSourceRef,
    /// Exact-text changes require a source observation from the file that contains the text.
    OldTextSourceRef,
    /// A full-file edit without `source_ref` named a file that already exists: it may only create
    /// a missing one, so replacing needs the `source_ref` of a read of that file. The worker, not
    /// the facade, applies this rule, since only it observes whether the path exists.
    ReplaceSourceRef,
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
            Self::IntegerRange { min, max } => {
                format!("must be an integer from {min} to {max}")
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
            Self::RangeEditSourceRef => {
                "is required for a line-range edit; re-read the lines (ide.read) and retry with \
                 the new source_ref"
                    .to_string()
            }
            Self::OldTextSourceRef => {
                "is required for an old-text change; re-read this file with ide.read and retry with the new source_ref".to_string()
            }
            Self::ReplaceSourceRef => {
                "is required to replace an existing file: read it first (ide.read)".to_string()
            }
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
            Self::OutlineTarget => OUTLINE_TARGET_MESSAGE.to_string(),
            Self::ReadTarget => {
                "ide.read needs one form: `symbol`, or `path` alone (the whole file), or `path` \
                 with `lines`, or `path` with `ranges`, or `symbols` — exactly one"
                    .to_string()
            }
            Self::EditTarget => {
                "ide.edit takes one form: `symbol` with `op`, or `path` with `lines` and \
                 `source_ref` — not both"
                    .to_string()
            }
            Self::ChangesTarget => {
                "ide.edit takes one form: \"changes\" with \"path\", or one single-change form \
                 — not both"
                    .to_string()
            }
            Self::TestTargetUnsupported(message) => format!(
                "invalid bounded parameters: {message}; use ide.test with `pattern` or `command`"
            ),
        }
    }
}

/// Model-facing text for a context request that names neither a path nor the problems kind.
const CONTEXT_TARGET_MESSAGE: &str =
    "invalid bounded parameters: ide.context needs either \"path\" or \"kind\":\"problems\"";
/// Model-facing text for an outline request without its required path.
const OUTLINE_TARGET_MESSAGE: &str = "invalid bounded parameters: ide.outline needs \"path\" (a file or directory relative to the worktree root)";

/// Returns the closed allowed field list for one logical tool.
fn allowed_fields(tool: AssistanceTool) -> &'static [&'static str] {
    match tool {
        AssistanceTool::Start => &["activation_id", "root", "read_only", "environment"],
        AssistanceTool::Context => &[
            "path",
            "byte_offset",
            "detail_ref",
            "kind",
            "language",
            "offset",
        ],
        AssistanceTool::Diff => &["mode", "detail_ref", "provenance", "paths"],
        AssistanceTool::Inspect => &["detail_ref"],
        AssistanceTool::Stop => &["activation_id"],
        AssistanceTool::Edit => &[
            "operation_id",
            "path",
            "source_ref",
            "content",
            "changes",
            "op",
            "symbol",
            "where",
            "new_name",
            "lines",
        ],
        AssistanceTool::Outline => &["path", "kinds"],
        AssistanceTool::Read => &["symbol", "path", "lines", "symbols", "ranges"],
        AssistanceTool::Symbol => &["symbol", "usages", "callers", "callees", "history"],
        AssistanceTool::Graph => &["symbol", "direction", "depth", "tests"],
        AssistanceTool::Test => &[
            "symbol", "path", "pattern", "command", "cwd", "env", "status", "budget_s",
        ],
    }
}

/// Returns the hard argument-object byte limit for one logical tool.
fn parameter_limit(tool: AssistanceTool) -> usize {
    match tool {
        AssistanceTool::Edit => crate::changes::edit::MAX_EDIT_ARGUMENT_BYTES,
        AssistanceTool::Test => MAX_TEST_PARAMETERS_BYTES,
        AssistanceTool::Start => MAX_START_PARAMETERS_BYTES,
        _ => MAX_PARAMETER_BYTES,
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
            if !object.contains_key("path") {
                return Err(ParameterError::OutlineTarget);
            }
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            if let Some(rule) = path_shape_rule(path.strip_suffix('/').unwrap_or(path)) {
                return Err(invalid_field("path", rule));
            }
            optional_string(object, "kinds", 256)?;
            if object
                .get("kinds")
                .and_then(Value::as_str)
                .is_some_and(|kinds| {
                    kinds
                        .split(',')
                        .any(|kind| crate::lang::SymbolKind::from_name(kind).is_none())
                })
            {
                return Err(invalid_field(
                    "kinds",
                    FieldRule::OneOf("a comma list over the closed symbol-kind vocabulary"),
                ));
            }
        }
        AssistanceTool::Read => {
            optional_string(object, "symbol", MAX_SYMBOL_PATH_BYTES)?;
            optional_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            optional_string(object, "lines", 32)?;
            // `symbols`: several strict `path#Owner/name` addresses (no sigil addresses — those
            // stay on the single `symbol` form); `ranges`: several ranges of one `path`.
            let symbols = symbol_list(object, "symbols", MAX_READ_ADDRESSES)?;
            let ranges = range_list(object, "ranges", MAX_READ_ADDRESSES)?;
            match (
                object.contains_key("symbol"),
                object.contains_key("path"),
                object.contains_key("lines"),
                symbols.as_ref().is_some_and(|list| !list.is_empty()),
                ranges.as_ref().is_some_and(|list| !list.is_empty()),
            ) {
                (true, false, false, false, false) => {}
                (false, true, true, false, false) => {
                    let path = object["path"].as_str().unwrap_or_default();
                    if let Some(rule) = path_shape_rule(path) {
                        return Err(invalid_field("path", rule));
                    }
                    if parse_line_range(object["lines"].as_str().unwrap_or_default()).is_none() {
                        return Err(invalid_field("lines", FieldRule::LineRange));
                    }
                }
                // A batch of symbol addresses needs nothing else.
                (false, false, false, true, false) => {}
                // `path` with `ranges` reads those ranges; `path` alone (no empty list beside it)
                // reads the whole file.
                (false, true, false, false, with_ranges)
                    if with_ranges || (symbols.is_none() && ranges.is_none()) =>
                {
                    let path = object["path"].as_str().unwrap_or_default();
                    if let Some(rule) = path_shape_rule(path) {
                        return Err(invalid_field("path", rule));
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
                return Err(invalid_field(
                    "depth",
                    FieldRule::IntegerRange { min: 1, max: 3 },
                ));
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
            if object.contains_key("cwd") || object.contains_key("env") {
                if !object.contains_key("command") {
                    return Err(invalid_field("cwd", FieldRule::OneOf("only with command")));
                }
                if let Some(cwd) = object.get("cwd") {
                    let cwd = cwd
                        .as_str()
                        .ok_or_else(|| invalid_field("cwd", FieldRule::String))?;
                    if cwd.len() > MAX_RELATIVE_PATH_BYTES
                        || (cwd != "." && path_shape_rule(cwd).is_some())
                    {
                        return Err(invalid_field("cwd", FieldRule::RelativePath));
                    }
                }
                if let Some(env) = object.get("env") {
                    let values = env
                        .as_object()
                        .filter(|env| env.len() <= MAX_ENV_ENTRIES)
                        .ok_or_else(|| {
                            invalid_field(
                                "env",
                                FieldRule::OneOf("an object with at most 32 environment entries"),
                            )
                        })?;
                    for (name, value) in values {
                        let valid_name = !name.is_empty()
                            && name.len() <= MAX_ENV_NAME_BYTES
                            && (name.as_bytes()[0].is_ascii_alphabetic() || name.starts_with('_'))
                            && name
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
                        if !valid_name
                            || value.as_str().is_none_or(|value| {
                                value.len() > MAX_ENV_VALUE_BYTES || value.contains('\0')
                            })
                        {
                            return Err(invalid_field(
                                "env",
                                FieldRule::OneOf(
                                    "environment names and values must be bounded strings without NUL",
                                ),
                            ));
                        }
                    }
                }
            }
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
                let mut command_bytes = 0usize;
                if argv.iter().any(|arg| {
                    let Some(arg) = arg.as_str() else { return true };
                    command_bytes = command_bytes.saturating_add(arg.len());
                    arg.len() > MAX_COMMAND_BYTES
                        || arg.contains('\0')
                        || command_bytes > MAX_COMMAND_BYTES
                }) {
                    return Err(invalid_field(
                        "command",
                        FieldRule::OneOf("argv strings without NUL totaling at most 16384 bytes"),
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
                    FieldRule::IntegerRange { min: 1, max: 600 },
                ));
            }
        }
        AssistanceTool::Start => {
            // `activation_id` is optional: a start that names none keeps a stable default derived
            // from its binding server-side (E013 item 4); a present one stays bounded nonempty.
            optional_string(object, "activation_id", MAX_ACTIVATION_ID_BYTES)?;
            if let Some(environment) = object.get("environment") {
                let choices = environment.as_object().ok_or_else(|| {
                    invalid_field(
                        "environment",
                        FieldRule::OneOf("an object with at most 8 environment selections"),
                    )
                })?;
                if choices.len() > 8 {
                    return Err(invalid_field(
                        "environment",
                        FieldRule::OneOf("at most 8 environment selections"),
                    ));
                }
                for (key, value) in choices {
                    let (language, root) = key
                        .split_once(':')
                        .map_or((key.as_str(), None), |(language, root)| {
                            (language, Some(root))
                        });
                    if language.is_empty()
                        || language.len() > 128
                        || !language
                            .bytes()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_')
                        || root.is_some_and(|root| {
                            root.is_empty() || root.len() > 512 || path_shape_rule(root).is_some()
                        })
                        || value.as_str().is_none_or(|value| {
                            value.is_empty() || value.chars().count() > 1024 || value.contains('\0')
                        })
                    {
                        return Err(invalid_field(
                            "environment",
                            FieldRule::OneOf(
                                "language[:relative root] keys and nonempty selectors of at most 1024 characters",
                            ),
                        ));
                    }
                }
            }
            optional_string(object, "root", MAX_RELATIVE_PATH_BYTES)?;
            if object
                .get("read_only")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(invalid_field("read_only", FieldRule::Boolean));
            }
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
            if let Some(paths) = string_list(object, "paths", 16)? {
                for path in paths {
                    if let Some(rule) = path_shape_rule(&path) {
                        return Err(invalid_field("paths", rule));
                    }
                }
            }
            optional_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
            if object.get("mode").is_some_and(|value| {
                !matches!(
                    value.as_str(),
                    Some("head" | "staged" | "unstaged" | "task")
                )
            }) {
                return Err(invalid_field(
                    "mode",
                    FieldRule::OneOf("\"head\", \"staged\", \"unstaged\", or \"task\""),
                ));
            }
            if object
                .get("provenance")
                .is_some_and(|value| !value.is_boolean())
            {
                return Err(invalid_field("provenance", FieldRule::Boolean));
            }
        }

        AssistanceTool::Inspect => {
            required_string(object, "detail_ref", MAX_DETAIL_REF_BYTES)?;
        }
        AssistanceTool::Stop => {
            optional_string(object, "activation_id", MAX_ACTIVATION_ID_BYTES)?;
        }
        AssistanceTool::Edit if object.contains_key("changes") => {
            required_string(object, "operation_id", 128)?;
            for field in ["symbol", "lines", "op", "where", "new_name", "content"] {
                if object.contains_key(field) {
                    return Err(ParameterError::ChangesTarget);
                }
            }
            let path = required_string(object, "path", MAX_RELATIVE_PATH_BYTES)?;
            if let Some(rule) = path_shape_rule(path) {
                return Err(invalid_field("path", rule));
            }
            let changes = object["changes"]
                .as_array()
                .filter(|list| (1..=MAX_EDIT_CHANGES).contains(&list.len()))
                .ok_or_else(|| {
                    invalid_field(
                        "changes",
                        FieldRule::OneOf("an array of 1 to 32 change objects"),
                    )
                })?;
            // Line numbers and exact text are only meaningful for the bytes the caller read, so
            // any `lines` or `old` entry makes the call name the observation it came from.
            let mut needs_source = false;
            for entry in changes {
                let entry = entry.as_object().ok_or_else(|| {
                    invalid_field("changes", FieldRule::OneOf("an array of change objects"))
                })?;
                let allowed = [
                    "lines", "symbol", "op", "where", "content", "old", "new", "within",
                ];
                if let Some(field) = entry
                    .keys()
                    .find(|field| !allowed.contains(&field.as_str()))
                {
                    return Err(ParameterError::UnknownField(echoable_field(field)));
                }
                let addresses = ["lines", "symbol", "old"]
                    .into_iter()
                    .filter(|field| entry.contains_key(*field))
                    .count();
                if addresses != 1 {
                    return Err(invalid_field(
                        "changes",
                        FieldRule::OneOf(
                            "each entry addressed by exactly one of \"lines\", \"symbol\" or \
                             \"old\"",
                        ),
                    ));
                }
                if entry.contains_key("lines") {
                    if parse_line_range(entry_string(entry, "lines")?).is_none() {
                        return Err(invalid_field(
                            "changes",
                            FieldRule::OneOf("entry \"lines\" like 12-20"),
                        ));
                    }
                    needs_source = true;
                    // The content may be empty: empty replaces the range with nothing, which
                    // deletes those lines.
                    if !entry.get("content").is_some_and(Value::is_string) {
                        return Err(invalid_field("content", FieldRule::String));
                    }
                } else if entry.contains_key("symbol") {
                    let symbol = entry_string(entry, "symbol")?;
                    if symbol.len() > MAX_SYMBOL_PATH_BYTES {
                        return Err(invalid_field(
                            "changes",
                            FieldRule::OneOf("entry \"symbol\" at most 1024 bytes"),
                        ));
                    }
                    let op = match entry.get("op") {
                        Some(Value::String(op)) => op.as_str(),
                        Some(_) => {
                            return Err(invalid_field(
                                "changes",
                                FieldRule::OneOf(
                                    "entry \"op\" one of \"replace\", \"insert\", \"delete\"",
                                ),
                            ));
                        }
                        None => "replace",
                    };
                    if !matches!(op, "replace" | "insert" | "delete") {
                        return Err(invalid_field(
                            "changes",
                            FieldRule::OneOf(
                                "entry \"op\" one of \"replace\", \"insert\", \"delete\"",
                            ),
                        ));
                    }
                    match op {
                        "replace" => {
                            entry_string(entry, "content")?;
                        }
                        "insert" => {
                            entry_string(entry, "content")?;
                            if !entry
                                .get("where")
                                .and_then(Value::as_str)
                                .is_some_and(|value| {
                                    matches!(value, "before" | "after" | "first" | "last")
                                })
                            {
                                return Err(invalid_field(
                                    "changes",
                                    FieldRule::OneOf(
                                        "an insert entry needs \"where\" one of \"before\", \
                                         \"after\", \"first\", \"last\"",
                                    ),
                                ));
                            }
                        }
                        _ => {}
                    }
                } else {
                    entry_string(entry, "old")?;
                    needs_source = true;
                    if !entry
                        .get("new")
                        .is_some_and(|value| value.as_str().is_some())
                    {
                        return Err(invalid_field(
                            "changes",
                            FieldRule::OneOf("an \"old\" entry needs its \"new\" text"),
                        ));
                    }
                    if let Some(value) = entry.get("within") {
                        let Some(within) = value.as_str() else {
                            return Err(invalid_field(
                                "changes",
                                FieldRule::OneOf("entry \"within\" a symbol path string"),
                            ));
                        };
                        if within.is_empty() || within.len() > MAX_SYMBOL_PATH_BYTES {
                            return Err(invalid_field(
                                "changes",
                                FieldRule::OneOf("entry \"within\" at most 1024 bytes"),
                            ));
                        }
                    }
                }
            }
            if needs_source {
                // Line numbers and exact text name the read they came from; the refusal teaches
                // the re-read the way the single line-range form's does.
                if !object.contains_key("source_ref") {
                    let rule = if changes.iter().any(|entry| entry.get("old").is_some()) {
                        FieldRule::OldTextSourceRef
                    } else {
                        FieldRule::RangeEditSourceRef
                    };
                    return Err(invalid_field("source_ref", rule));
                }
                required_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?;
            } else {
                optional_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?;
            }
        }
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
                // A redundant `path` naming the symbol's own file is accepted and ignored; any
                // other mix of the two forms is refused as exclusive.
                let symbol = object["symbol"].as_str().unwrap_or_default();
                let names_own_file = symbol.split_once('#').is_some_and(|(file, _)| {
                    object.get("path").and_then(Value::as_str) == Some(file)
                });
                if (object.contains_key("path") && !names_own_file) || object.contains_key("lines")
                {
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
                // Line numbers are only meaningful for the exact content the caller read, so the
                // range form always names the observation it came from.
                if !object.contains_key("source_ref") {
                    return Err(invalid_field("source_ref", FieldRule::RangeEditSourceRef));
                }
                required_string(object, "source_ref", MAX_DETAIL_REF_BYTES)?;
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
            // Without `source_ref` the call may only create a file that does not exist yet: the
            // worker observes the path itself and refuses an existing file. The placeholder only
            // runs the request's own validation here (the worker supplies the real reference) and
            // is dropped from the forwarded parameters below.
            let source_ref = object
                .contains_key("source_ref")
                .then(|| required_string(object, "source_ref", MAX_DETAIL_REF_BYTES))
                .transpose()?;
            let request = crate::changes::edit::EditRequest::new(
                operation_id,
                path,
                source_ref.unwrap_or("create"),
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
            let creates = source_ref.is_none();
            parameters =
                serde_json::to_value(request).map_err(|_| ParameterError::InvalidObject)?;
            if creates {
                parameters
                    .as_object_mut()
                    .expect("serialized request object")
                    .remove("source_ref");
            }
        }
    }
    if tool == AssistanceTool::Diff {
        let object = parameters.as_object_mut().expect("validated object");
        object.entry("mode").or_insert(json!("head"));
        object.entry("provenance").or_insert(json!(false));
    }
    Ok(ValidatedCall { tool, parameters })
}

/// Reads one required bounded nonempty string from one `changes` entry, refusing on the entry
/// field's own name so the caller learns exactly which key was missing or empty.
fn entry_string<'a>(
    entry: &'a Map<String, Value>,
    field: &'static str,
) -> Result<&'a str, ParameterError> {
    entry
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_field(field, FieldRule::NonEmptyString))
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
    /// The daemon answered a typed `busy` reply: every connection of its lane was taken, the call
    /// never ran (nothing was applied), and repeating it is safe.
    Busy,
    /// The daemon answered a typed `restarting` reply: it failed internally and is exiting to be
    /// replaced, the call never ran (nothing was applied), and a managed client re-establishes a
    /// daemon and sends it once more.
    Restarting,
    /// Writing the request began but no usable reply arrived, or a mutating call's reply timed
    /// out: the call may have executed, so it is never resent and no reconnect is attempted.
    OutcomeUnknown,
    /// Transport failed and this managed client could not re-establish a daemon.
    ReestablishFailed,
    /// IPC accepted the envelope but no typed peer result was available for safe rendering.
    Incomplete,
    /// Typed peer result accepted from the daemon, with its optional carried status plate (T28B).
    /// Boxed so the rare large reply does not size every outcome.
    Reply(Box<PeerReply>, Option<String>),
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
            limits: HookTransportLimits::new(
                crate::app::MAX_V2_FRAME_BYTES,
                MAX_HOOK_BYTES,
                Duration::from_secs(1),
            )
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
            limits: HookTransportLimits::new(
                crate::app::MAX_V2_FRAME_BYTES,
                MAX_HOOK_BYTES,
                Duration::from_secs(1),
            )
            .expect("fixed Assistance transport limits are valid"),
        }
    }

    /// Validates and sends exactly one current method through Application's finite dispatch envelope.
    ///
    /// Only a closed typed missing-peer reply is rendered; arbitrary transport acceptance remains
    /// `Incomplete` (`OutcomeUnknown` for a mutating tool, which may already have applied) and
    /// never becomes a model-read, source-read, or peer-ready claim.
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
        self.dispatch_validated(runtime_dir, host, call).await.0
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
        self.dispatch_at_tagged(runtime_dir, host, tool, parameters)
            .await
            .0
    }

    /// [`Self::dispatch_at`], also returning the private actor tag a current managed Claude
    /// daemon attaches to start replies, identity answers and `recovery_needed` refusals;
    /// `Some(None)` is an identity answer that names no actor.
    pub async fn dispatch_at_tagged(
        &self,
        runtime_dir: &Path,
        host: &TrustedTransport,
        tool: AssistanceTool,
        parameters: Value,
    ) -> (FacadeOutcome, Option<Option<String>>) {
        let Ok(call) = validate_call(tool, parameters) else {
            return (FacadeOutcome::InvalidParameters, None);
        };
        self.dispatch_validated(runtime_dir, host, call).await
    }

    /// Shared tail of [`Self::dispatch`] and [`Self::dispatch_at`] once parameters are validated.
    async fn dispatch_validated(
        &self,
        runtime_dir: &Path,
        host: &TrustedTransport,
        call: ValidatedCall,
    ) -> (FacadeOutcome, Option<Option<String>>) {
        let tool = call.tool();
        // The method envelope has the daemon's own method-payload bound, so the front's private
        // host metadata never takes room from bounded model arguments.
        let Some(parameters) = OpaqueJson::from_value(
            &json!({"parameters":call.parameters(),"host_meta":host.host_meta}),
            crate::app::MAX_ASSISTANCE_JSON_BYTES,
        ) else {
            return (FacadeOutcome::InvalidParameters, None);
        };
        let Some(request) = MethodDispatch::new(
            host.request_id.clone(),
            host.correlation_id.clone(),
            host.opaque_attachment.clone(),
            call.tool().transport_method(),
            parameters,
        ) else {
            return (FacadeOutcome::Unavailable, None);
        };
        let mut tag = None;
        let outcome = match dispatch_method_if_running(runtime_dir, request, self.limits).await {
            MethodDispatchTransportResult::Unavailable => FacadeOutcome::Unavailable,
            MethodDispatchTransportResult::TimedOut => {
                journal_transport_timeout(tool, "connect");
                FacadeOutcome::TimedOut
            }
            MethodDispatchTransportResult::Busy => FacadeOutcome::Busy,
            MethodDispatchTransportResult::Restarting => FacadeOutcome::Restarting,
            MethodDispatchTransportResult::OutcomeUnknown => FacadeOutcome::OutcomeUnknown,
            // A read-only call that timed out after delivery changed nothing worth checking.
            MethodDispatchTransportResult::WrittenTimedOut if tool.mutates() => {
                journal_transport_timeout(tool, "reply");
                FacadeOutcome::OutcomeUnknown
            }
            MethodDispatchTransportResult::WrittenTimedOut => {
                journal_transport_timeout(tool, "reply");
                FacadeOutcome::TimedOut
            }
            MethodDispatchTransportResult::Dispatched { opaque_result_json } => {
                let (actor, delivered) = match untag_reply(opaque_result_json.as_str()) {
                    // Only an identity query may be answered with no actor; on any other call the
                    // wrapper stays undecodable.
                    (Some(None), _)
                        if host
                            .host_meta
                            .as_ref()
                            .and_then(|meta| meta.get("claudecode/whois"))
                            .is_none() =>
                    {
                        (None, opaque_result_json.as_str().to_owned())
                    }
                    split => split,
                };
                tag = actor;
                match PeerReply::decode_delivered(&delivered) {
                    Some((
                        reply @ (PeerReply::Unavailable { .. }
                        | PeerReply::HostStopped {}
                        | PeerReply::Pending { .. }
                        | PeerReply::Error { .. }
                        | PeerReply::InvalidParameters { .. }),
                        status,
                    )) => FacadeOutcome::Reply(Box::new(reply), status),
                    Some((reply @ PeerReply::Edit { .. }, status))
                        if matches!(tool, AssistanceTool::Edit | AssistanceTool::Inspect) =>
                    {
                        FacadeOutcome::Reply(Box::new(reply), status)
                    }
                    Some((reply @ PeerReply::Complete { kind, .. }, status))
                        if tool == AssistanceTool::Inspect
                            || tool_accepts_result_kind(tool, kind) =>
                    {
                        FacadeOutcome::Reply(Box::new(reply), status)
                    }
                    // An undecodable reply to a delivered mutation cannot prove it did not apply.
                    _ if tool.mutates() => FacadeOutcome::OutcomeUnknown,
                    _ => FacadeOutcome::Incomplete,
                }
            }
        };
        (outcome, tag)
    }
}

/// Reports whether a delivered call's outcome is the kind a wedged daemon produces: a transport
/// timeout, a lost reply, or the daemon's own `internal` refusal.
fn suspects_wedged_daemon(outcome: &FacadeOutcome) -> bool {
    match outcome {
        FacadeOutcome::TimedOut | FacadeOutcome::OutcomeUnknown => true,
        FacadeOutcome::Reply(reply, _) => matches!(
            reply.as_ref(),
            PeerReply::Error {
                code: FailureCode::Internal,
                ..
            }
        ),
        _ => false,
    }
}

/// Writes the client journal line of one transport timeout, which leaves no other trace: the
/// failed phase (`connect` or `reply`) and the tool, never any parameter or reply content.
fn journal_transport_timeout(tool: AssistanceTool, phase: &str) {
    crate::errorlog::record(
        crate::errorlog::Method::Client,
        crate::errorlog::Outcome::Timeout,
        crate::errorlog::Fields {
            reason: Some(crate::errorlog::ReasonCode::Deadline),
            detail: Some(&format!("transport:{phase}_timed_out:{}", tool.mcp_name())),
            ..Default::default()
        },
    );
}

/// The answer to an `ide.stop` that was not sent because its actor could not be identified.
const STOP_UNIDENTIFIED: &str = "error: stop_unidentified: the IDE could not identify this ide.stop's session, so nothing was stopped; repeat ide.stop, or continue with native tools";

/// What one stop's identity query established (see `StdioFacade::identify_actor`).
enum StopIdentity {
    /// The private tag of the actor whose pending pre owns the stop.
    Actor(String),
    /// The serving daemon does not answer identity queries at all.
    Unsupported,
    /// The daemon's own refusal, or the transport outcome: the stop is not sent.
    Refused(FacadeOutcome),
}

/// Builds one bounded synthetic call id for a front-issued call that stands in for `real`; the
/// real id (itself up to the identifier bound) travels only in private host metadata.
fn synthetic_call_id(prefix: &str, real: &str) -> String {
    format!("{prefix}-{}", &blake3::hash(real.as_bytes()).to_hex()[..32])
}

/// Splits a current managed Claude daemon's `{"actor":tag,"reply":...}` wrapper into the private
/// actor tag and the unchanged reply wire. `Some(None)` is a wrapper whose actor is `null` (an
/// identity query no actor owns, answered by a daemon that supports the query). Any other reply,
/// including a wrapper whose tag is not exactly 64 lowercase hex digits, passes through untouched
/// and so stays undecodable. No closed reply form has exactly these two fields, so the split
/// cannot misread a reply.
fn untag_reply(delivered: &str) -> (Option<Option<String>>, String) {
    if let Ok(Value::Object(wrapper)) = serde_json::from_str::<Value>(delivered)
        && wrapper.len() == 2
        && let (Some(actor), Some(reply)) = (wrapper.get("actor"), wrapper.get("reply"))
    {
        match actor {
            Value::Null => return (Some(None), reply.to_string()),
            Value::String(tag) if super::host_binding::valid_actor_tag(tag) => {
                return (Some(Some(tag.clone())), reply.to_string());
            }
            _ => {}
        }
    }
    (None, delivered.to_owned())
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
/// both hosts) and Claude's optional agent type are kept for isolation evidence, the post phase
/// tool name selects check triggers, and a writer tool's bounded file path names the changed
/// file's language for that trigger; no other raw hook field ever enters the transport.
/// Only an observed, settled, or feedback reply counts as submission; a daemon refusal is
/// unavailable.
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
        // Post phases of either host: the bare native tool name that selects project-check
        // triggers, and the writer tool's bounded file path that names the changed file's
        // language for the trigger (`None` for a tool that names no file, e.g. `Bash`).
        "tool_name": event.tool_name(),
        "tool_file": event.tool_file(),
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

    /// Renders the model-facing fact, next action and freshness without exposing internal evidence provenance.
    pub fn render(&self) -> String {
        let mut rendered = format!(
            "Fact: {}\nNext: {}\nFreshness: {}",
            self.fact, self.next_action, self.freshness
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

/// Forcibly ends the daemon a managed front owns or shares when it stayed wedged: it holds its
/// runtime but its control path answered nothing across repeated probes.
///
/// Called by the facade's wedge watch with the runtime directory it probed, the pid of the daemon
/// whose silence it observed (when known), the number of failed probes and the time they span (evidence for the journal), only after at least [`crate::app::WEDGE_MIN_PROBES`] probes
/// spanning [`crate::app::WEDGE_MIN_SPAN`]. The implementation owns every safety check and the journal line; it must
/// never signal a daemon whose control path answers. The facade re-establishes afterwards.
pub type EvictFn = Arc<
    dyn Fn(PathBuf, Option<i32>, u32, Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

/// Pause between the wedge watch's probes.
const WEDGE_PROBE_INTERVAL: Duration = Duration::from_secs(15);

/// Re-roots one managed Claude session to the root directory a refused `ide.start` named (T15B).
///
/// The closure receives the model's exact `root` argument when the call carried one, or `None`
/// when a root-less start returns to the host's own project directory; it canonicalizes and admits
/// the target itself and attaches through the same path a fresh session in that directory would
/// take.
///
/// The second argument says whether the session has other actors than this call's own — every
/// remembered actor the refusal did not identify as the caller, or, for a caller no tag
/// identified, the single remembered activation of an older daemon. A target in another
/// repository is then refused with [`RerootOutcome::OtherRepository`] instead of moving the whole
/// session away from those actors.
pub type RerootFn = Arc<
    dyn Fn(Option<String>, bool) -> Pin<Box<dyn Future<Output = RerootOutcome> + Send>>
        + Send
        + Sync,
>;

/// Closed outcome of one managed Claude re-root attempt (T15B).
#[derive(Debug, Eq, PartialEq)]
pub enum RerootOutcome {
    /// The session is already bound to this exact target; the refused reply stands.
    Unchanged,
    /// Attached to the target root's daemon: the fresh pair and the bound candidate.
    Attached(PathBuf, String, PathBuf),
    /// The target is another worktree of the same repository on a current shared daemon: it was
    /// registered so its own actors' hooks pair there, and the session's pair stays unchanged.
    Registered,
    /// The target is another repository while the session has other actors: moving the session
    /// would strand them, so nothing moved and the start is refused with an explicit cause.
    OtherRepository,
    /// The target root resolves below no allowed root; the refused reply stands and the daemon's
    /// own admission answers.
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
/// Only a `test-seams` build reads the variable; the hook's own 250 ms total
/// deadline and this module's [`PUBLICATION_WAIT`] bound the delay's observable effect either way.
pub(crate) fn stall_rendezvous_for_test() {
    let Some(milliseconds) = crate::test_seams::var("AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    std::thread::sleep(Duration::from_millis(milliseconds.min(60_000)));
}

/// Most actors one managed Claude front remembers starts for (one daemon's recover announcement).
const MAX_REMEMBERED_ACTORS: usize = 32;

/// One remembered successful activation, the facts a transparent re-activation replays.
#[derive(Clone, Debug)]
struct RememberedActivation {
    /// The model-supplied activation id of the successful `ide.start`.
    activation_id: String,
    /// The `root` argument that start carried, when it carried one.
    root: Option<String>,
}

/// What a managed front knows about the currency of the daemon it serves (0.6.7), shared between
/// the attach path that learns it and the facade that renders it.
///
/// The front compares the daemon's health-reported version with its own at every rendezvous: an
/// idle older daemon is replaced before serving begins, and one still bound by another session is
/// kept serving with one honest line on the start card naming both versions.
#[derive(Default)]
pub struct DaemonCurrencyNote {
    /// One honest start-card line while an outdated daemon is still serving this session.
    line: Option<String>,
    /// One-shot: the most recent attach replaced an outdated daemon before serving began.
    replaced: bool,
}

impl DaemonCurrencyNote {
    /// Records that this session serves a current daemon; no note is due.
    pub fn note_current(&mut self) {
        self.line = None;
        self.replaced = false;
    }

    /// Records that this session's attach spawned a current daemon: no honest line is due, and a
    /// replacement marker set earlier in the same attach (it stopped an outdated daemon first)
    /// survives, because that first reply's pre-hook observations still died with the old daemon.
    pub fn note_spawned_current(&mut self) {
        self.line = None;
    }

    /// Records that this session keeps serving an outdated daemon, with the start-card line
    /// explaining why it was not replaced.
    pub fn note_outdated(&mut self, line: String) {
        self.line = Some(line);
        self.replaced = false;
    }

    /// Records that this session's attach replaced an outdated daemon.
    pub fn note_replaced(&mut self) {
        self.line = None;
        self.replaced = true;
    }

    /// Returns the honest start-card line, when one is due.
    pub fn line(&self) -> Option<&str> {
        self.line.as_deref()
    }

    /// Takes the one-shot replaced marker: the first reply after the replacement is the only one
    /// that can still predate the new daemon's own observations.
    pub fn take_replaced(&mut self) -> bool {
        std::mem::take(&mut self.replaced)
    }
}

/// One shared [`DaemonCurrencyNote`], held by the managed attach path and the facade together.
pub type SharedDaemonNote = Arc<std::sync::Mutex<DaemonCurrencyNote>>;

/// Shares one live `(runtime_dir, attachment)` pair across every clone of a [`StdioFacade`].
///
/// A managed daemon can exit while its MCP process keeps running. Every call reads the current
/// pair and, on transport loss, re-runs `reestablish` once and stores its result for later calls.
/// A managed Claude session additionally tracks the canonical project it is bound to, may carry a
/// `reroot` hook that moves the whole pair (and the host's hook rendezvous) to another root after
/// the daemon proved this session's hooks no longer pair where it dispatched (T15B), and remembers
/// each actor's admitted start (or, for an untagged daemon, its last successful activation) so a
/// daemon replacement can be recovered transparently (T15B restart recovery).
#[derive(Clone)]
struct ManagedConnection {
    current: Arc<Mutex<(PathBuf, String)>>,
    candidate: Arc<Mutex<Option<PathBuf>>>,
    /// The last successful activation answered without an actor tag (an older daemon, or a
    /// Codex host): the 0.10.2 single remembered activation.
    last_activation: Arc<Mutex<Option<RememberedActivation>>>,
    /// Each actor's last admitted start parameters on a current managed Claude daemon, keyed by
    /// the daemon's private actor tag, oldest first, at most [`MAX_REMEMBERED_ACTORS`].
    remembered: Arc<std::sync::Mutex<Vec<(String, Value)>>>,
    /// Set once the bounded memory above evicted an actor that may still be active: from then on
    /// the session may hold actors it no longer remembers, so no actor is assumed gone.
    // ponytail: sticky for the rest of the session once more than 32 actors have been remembered
    // (a refused cross-repository start then says other agents *may* still work in the bound
    // repository); start a new session in the other repository. Upgrade by tracking active
    // actors separately from recovery parameters if sessions with that many actors become common.
    memory_overflowed: Arc<std::sync::atomic::AtomicBool>,
    /// Orders every start and stop (identity query, dispatch, memory update), each actor
    /// recovery and lease-driven recovery, so remembered starts follow the daemon's own order.
    lifecycle: Arc<Mutex<()>>,
    /// Set while a daemon replacement still needs its binding transparently re-activated.
    recovery_pending: Arc<std::sync::atomic::AtomicBool>,
    /// Set from a daemon replacement until the next successful activation; references issued
    /// before it are the ones a replacement invalidated.
    replaced: Arc<std::sync::atomic::AtomicBool>,
    /// What the attach path learned about this daemon's version currency (0.6.7); a plain or
    /// Codex-owned connection never notes anything.
    note: Option<SharedDaemonNote>,
    reroot: Option<RerootFn>,
    reestablish: ReestablishFn,
    /// Force-replaces a daemon that stayed wedged; absent for a connection with no such authority.
    evict: Option<EvictFn>,
    /// Set while a background wedge watch runs, so one watch serves every concurrent failure.
    watching: Arc<std::sync::atomic::AtomicBool>,
}

impl ManagedConnection {
    fn new(runtime_dir: PathBuf, attachment: String, reestablish: ReestablishFn) -> Self {
        Self {
            current: Arc::new(Mutex::new((runtime_dir, attachment))),
            candidate: Arc::new(Mutex::new(None)),
            last_activation: Arc::new(Mutex::new(None)),
            remembered: Arc::new(std::sync::Mutex::new(Vec::new())),
            memory_overflowed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
            recovery_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            replaced: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            note: None,
            reroot: None,
            reestablish,
            evict: None,
            watching: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Like [`Self::new`], for a managed Claude session that knows its bound project and can
    /// re-root to another admitted root (T15B), sharing its daemon-currency note with the attach
    /// path that keeps it current (0.6.7).
    fn rerootable(
        runtime_dir: PathBuf,
        attachment: String,
        candidate: PathBuf,
        reestablish: ReestablishFn,
        reroot: RerootFn,
        note: SharedDaemonNote,
    ) -> Self {
        Self {
            candidate: Arc::new(Mutex::new(Some(candidate))),
            note: Some(note),
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

    /// Remembers one actor's admitted start parameters under its tag, replacing its older ones.
    fn remember(&self, tag: String, parameters: Value) {
        let mut remembered = self.remembered.lock().expect("remembered starts mutex");
        remembered.retain(|(known, _)| known != &tag);
        if remembered.len() >= MAX_REMEMBERED_ACTORS {
            remembered.remove(0);
            self.memory_overflowed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        remembered.push((tag, parameters));
    }

    /// Forgets one actor's remembered start.
    fn forget(&self, tag: &str) {
        self.remembered
            .lock()
            .expect("remembered starts mutex")
            .retain(|(known, _)| known != tag);
    }

    /// Returns one actor's remembered start parameters.
    fn remembered(&self, tag: &str) -> Option<Value> {
        self.remembered
            .lock()
            .expect("remembered starts mutex")
            .iter()
            .find(|(known, _)| known == tag)
            .map(|(_, parameters)| parameters.clone())
    }

    /// Reports whether the session has actors other than this call's own, so a start naming
    /// another repository must not move the session away from them (F-02).
    ///
    /// `caller` is the actor tag the refusal identified, when it did. Every remembered actor
    /// except the caller counts; for an unidentified caller the older daemon's single remembered
    /// activation counts too; and once the bounded memory evicted an actor that may still be
    /// active, the answer stays `true` — an evicted actor is never assumed gone.
    async fn has_other_actors(&self, caller: Option<&str>) -> bool {
        !self.remembered_tags(caller).is_empty()
            || (caller.is_none() && self.last_activation.lock().await.is_some())
            || self
                .memory_overflowed
                .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Returns every remembered actor tag except `exclude`.
    fn remembered_tags(&self, exclude: Option<&str>) -> Vec<String> {
        self.remembered
            .lock()
            .expect("remembered starts mutex")
            .iter()
            .map(|(tag, _)| tag.clone())
            .filter(|tag| Some(tag.as_str()) != exclude)
            .collect()
    }

    /// Reports whether this session holds any remembered activation, tagged or not.
    async fn activated_before(&self) -> bool {
        self.last_activation.lock().await.is_some()
            || !self
                .remembered
                .lock()
                .expect("remembered starts mutex")
                .is_empty()
    }

    /// Marks this session's daemon as replaced, with its binding awaiting re-activation.
    fn mark_replaced(&self) {
        self.recovery_pending
            .store(true, std::sync::atomic::Ordering::Release);
        self.replaced
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Re-establishes the daemon and publishes the new pair for later calls, marking the session
    /// replaced when the pair changed. A failed re-establishment changes nothing.
    async fn heal(&self, runtime_dir: &Path, attachment: &str) {
        if let Some((new_runtime, new_attachment)) = (self.reestablish)().await {
            if new_runtime != runtime_dir || new_attachment != attachment {
                self.mark_replaced();
            }
            self.store(new_runtime, new_attachment).await;
        }
    }

    /// Starts the background liveness check of a daemon a delivered call just suspected of being
    /// wedged (it timed out, lost its reply or answered `internal`), unless one already runs.
    ///
    /// The call in hand has its outcome and is never resent; nothing here delays it. The check
    /// probes the daemon's control path once: a healthy daemon is left alone however busy; one that
    /// says `restarting`, or is gone, is re-established at once; one that is silent but still holds
    /// its runtime is watched every [`WEDGE_PROBE_INTERVAL`]. Once
    /// [`crate::app::WEDGE_MIN_PROBES`] probes failed over at least [`crate::app::WEDGE_MIN_SPAN`]
    /// the watch calls `evict` (when this connection has that authority) and re-establishes the
    /// daemon, so the next call lands on a live one without any agent action. A daemon that
    /// answers again ends the watch with nothing signalled; it gives up after ten probes.
    fn check_suspect_daemon(&self, runtime_dir: PathBuf, attachment: String) {
        if self
            .watching
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
        let connection = self.clone();
        tokio::spawn(async move {
            let began = tokio::time::Instant::now();
            let mut probes = 0_u32;
            let mut pinned: Option<Option<i32>> = None;
            for round in 0..10 {
                if round > 0 {
                    tokio::time::sleep(WEDGE_PROBE_INTERVAL).await;
                }
                match crate::app::probe_health(&runtime_dir).await {
                    crate::app::HealthProbe::Healthy => break,
                    crate::app::HealthProbe::Restarting => {
                        connection.heal(&runtime_dir, &attachment).await;
                        break;
                    }
                    crate::app::HealthProbe::Silent => {}
                }
                if !crate::app::lock_is_held(&runtime_dir) {
                    // The daemon left its runtime: nothing to wait for or signal.
                    connection.heal(&runtime_dir, &attachment).await;
                    break;
                }
                // The evidence belongs to one daemon generation: when the connection moved to
                // another pair, or another process took the lock, this watch is over.
                let holder = crate::app::lock_holder_pid(&runtime_dir);
                let pinned_holder = *pinned.get_or_insert(holder);
                if connection.current().await != (runtime_dir.clone(), attachment.clone())
                    || holder != pinned_holder
                {
                    break;
                }
                probes += 1;
                let span = began.elapsed();
                if let Some(evict) = &connection.evict
                    && probes >= crate::app::WEDGE_MIN_PROBES
                    && span >= crate::app::WEDGE_MIN_SPAN
                {
                    evict(runtime_dir.clone(), pinned_holder, probes, span).await;
                    connection.heal(&runtime_dir, &attachment).await;
                    break;
                }
            }
            connection
                .watching
                .store(false, std::sync::atomic::Ordering::Release);
        });
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
    /// daemon and re-root after a daemon-refused `ide.start {root}` (T15B).
    ///
    /// A host that moves a project never restarts this MCP process; the re-root hook re-attaches
    /// through the fresh-session path of the refused start's root and the call retries there.
    pub fn with_reestablishing_claude_attachment(
        runtime_dir: PathBuf,
        attachment: String,
        candidate: PathBuf,
        reestablish: ReestablishFn,
        reroot: RerootFn,
        note: SharedDaemonNote,
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
                note,
            )),
            publisher: None,
            router: Self::described_tool_router(),
        })
    }

    /// Gives this managed facade the authority to force-replace a wedged daemon through `evict`
    /// (see [`EvictFn`]); without it a silent daemon is only probed and re-established when it
    /// leaves by itself.
    pub fn with_wedge_eviction(mut self, evict: EvictFn) -> Self {
        if let Some(reconnect) = &mut self.reconnect {
            reconnect.evict = Some(evict);
        }
        self
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
    /// reports transport unavailability. There is no retry loop. Only a call that was never
    /// delivered is retried: a written call whose reply was lost or late is never resent.
    ///
    /// A same-repository target on a current daemon is only registered (`RerootOutcome::Registered`):
    /// the session's pair stays, and the refused call gets the retry hint.
    ///
    /// A managed Claude session re-roots only after the daemon it dispatched a `Start` to refused
    /// that call with a closed cause proving this session's hooks no longer pair there (T15B,
    /// [`reroot_target`]): the reroot hook attaches through the target root's fresh-session path,
    /// stores the new pair for every later call, and retries this call there. A Start whose hooks
    /// still pair — including one naming another repository's admitted root — never re-roots: the
    /// daemon itself activates that root, so a cross-repository start can no longer strand the
    /// session between two daemons. The retried call's own pre-hook ran before the new rendezvous
    /// existed, so its first reply carries the re-root retry hint instead of a hard refusal; a
    /// re-root that cannot attach answers `project_moved` honestly.
    ///
    /// The returned [`Resume`] value names a target change: a transient timeout against the same
    /// live daemon must not claim it restarted. A replacement's or re-root's first dispatch may
    /// need the binding-recovery hint because its earlier pre-hook observation is gone.
    async fn dispatch_with_reconnect(
        &self,
        tool: AssistanceTool,
        parameters: Value,
        context: &RequestContext<RoleServer>,
        expected: Option<&str>,
    ) -> (FacadeOutcome, Resume, Option<String>) {
        let Some((runtime_dir, attachment)) = self.current_connection().await else {
            return (FacadeOutcome::MissingHostMetadata, Resume::Fresh, None);
        };
        let mut resume = Resume::Fresh;
        // T15B restart recovery (0.10.2 single slot, kept for activations an older daemon or a
        // Codex host answered without an actor tag): after a daemon replacement, the first call
        // whose pre-hook already reached the healed daemon transparently re-runs the remembered
        // activation before dispatching, naming that call. An explicit start or stop decides the
        // binding itself and is never preceded by one.
        if let Some(reconnect) = &self.reconnect
            && !matches!(tool, AssistanceTool::Start | AssistanceTool::Stop)
            && reconnect
                .recovery_pending
                .load(std::sync::atomic::Ordering::Acquire)
            && reconnect.last_activation.lock().await.is_some()
        {
            // The call's own id: a current daemon binds exactly that call's actor, and only when
            // its pre came through this front's attachment; an older daemon only sees the key.
            let marker =
                parse_claude_call_id(&context.meta).map_or(json!(true), |call| json!(call));
            self.reactivate_remembered_binding(&runtime_dir, &attachment, marker)
                .await;
        }
        let (outcome, tag) = self
            .dispatch_once(
                &runtime_dir,
                &attachment,
                tool,
                &parameters,
                context,
                expected,
            )
            .await;
        // T15B re-root: retry one refused Start on the root the refusal's own evidence names.
        if let Some(reconnect) = &self.reconnect
            && let Some(reroot) = &reconnect.reroot
            && tool == AssistanceTool::Start
            && let Some((target, rerooted)) = reroot_target(&parameters, &outcome, reconnect).await
        {
            // Other actors of this session (every remembered actor except this call's own, when
            // the refusal identified it) work in the bound repository: a move to another
            // repository would strand them.
            let others = reconnect.has_other_actors(tag.as_deref()).await;
            match reroot(target.clone(), others).await {
                RerootOutcome::Attached(new_runtime, new_attachment, candidate) => {
                    reconnect
                        .store(new_runtime.clone(), new_attachment.clone())
                        .await;
                    reconnect.store_candidate(candidate).await;
                    // Only a genuinely different daemon can answer the retry: the refused call
                    // identity is already replay evidence on the daemon that refused it.
                    if new_runtime == runtime_dir {
                        return (outcome, resume, tag);
                    }
                    resume = rerooted;
                    let (retried, tag) = self
                        .dispatch_once(
                            &new_runtime,
                            &new_attachment,
                            tool,
                            &parameters,
                            context,
                            expected,
                        )
                        .await;
                    return (retried, resume, tag);
                }
                // The refused call's pre-hook never reached the daemon; the next one now does.
                RerootOutcome::Registered => return (outcome, rerooted, tag),
                RerootOutcome::Unchanged | RerootOutcome::OutsideAllowedRoots => {
                    return (outcome, resume, tag);
                }
                // Nothing moved: name both directories and the reason, never re-root the session.
                RerootOutcome::OtherRepository => {
                    let asked = target.filter(|asked| !asked.is_empty());
                    let bound = reconnect.bound_candidate().await;
                    let cause = bound
                        .zip(asked)
                        .map(|(bound, asked)| HostBindingCause::other_repository(&bound, &asked));
                    return (
                        FacadeOutcome::Reply(
                            Box::new(PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause,
                            }),
                            None,
                        ),
                        resume,
                        tag,
                    );
                }
                RerootOutcome::Failed if target.is_some() => {
                    // Name both directories honestly; only an admitted root can be re-rooted.
                    let asked = target.filter(|asked| !asked.is_empty());
                    let bound = reconnect.bound_candidate().await;
                    let cause = bound
                        .zip(asked)
                        .map(|(bound, asked)| HostBindingCause::project_moved(&bound, &asked));
                    return (
                        FacadeOutcome::Reply(
                            Box::new(PeerReply::Unavailable {
                                reason: MissingPeer::HostBinding,
                                cause,
                            }),
                            None,
                        ),
                        resume,
                        None,
                    );
                }
                // A root-less return to the host's project directory that cannot attach keeps the
                // daemon's own refusal: no root was asked for, so none is named.
                RerootOutcome::Failed => return (outcome, resume, tag),
            }
        }
        let Some(reconnect) = &self.reconnect else {
            return (outcome, resume, tag);
        };
        // Only a call that was never delivered may be sent again: not reached (`Unavailable`), or
        // refused by a failed daemon before it ran (`Restarting`). A written call whose reply was
        // lost or late is never resent (OutcomeUnknown/TimedOut).
        if !matches!(
            outcome,
            FacadeOutcome::Unavailable | FacadeOutcome::Restarting
        ) {
            // Self-heal (stability QW-7): a delivered call that timed out, lost its reply or came
            // back `internal` may have met a wedged daemon. A background liveness check decides
            // (see `check_suspect_daemon`), so the next call lands on a live one. The call in hand
            // keeps its outcome at once and is never resent.
            if suspects_wedged_daemon(&outcome) {
                reconnect.check_suspect_daemon(runtime_dir.clone(), attachment.clone());
            }
            return (outcome, resume, tag);
        }
        let Some((new_runtime, new_attachment)) = (reconnect.reestablish)().await else {
            return (FacadeOutcome::ReestablishFailed, resume, None);
        };
        if new_runtime != runtime_dir || new_attachment != attachment {
            resume = Resume::Restarted;
            // The replacement discarded this session's bindings; marked before the new pair is
            // published, so no concurrent call reaches it without announcing its actors.
            reconnect.mark_replaced();
        }
        reconnect
            .store(new_runtime.clone(), new_attachment.clone())
            .await;
        let (retried, tag) = self
            .dispatch_once(
                &new_runtime,
                &new_attachment,
                tool,
                &parameters,
                context,
                expected,
            )
            .await;
        (retried, resume, tag)
    }

    /// Dispatches one call once against `runtime_dir`/`attachment`, restoring its actor first when
    /// a current managed Claude daemon answers `recovery_needed`.
    ///
    /// That refusal kept the call's pre-hook and named its actor: the actor's remembered start is
    /// re-run (under the lifecycle lock, so it cannot race that actor's stop) and the call is then
    /// sent once more without that actor's tag, so it can never loop. A call without valid host
    /// metadata performs no IPC.
    async fn dispatch_once(
        &self,
        runtime_dir: &Path,
        attachment: &str,
        tool: AssistanceTool,
        parameters: &Value,
        context: &RequestContext<RoleServer>,
        expected: Option<&str>,
    ) -> (FacadeOutcome, Option<String>) {
        let Some(mut host) = self.build_host(attachment, context) else {
            return (FacadeOutcome::MissingHostMetadata, None);
        };
        self.announce(&mut host, None, expected);
        // Managed Codex only: publish this process's route before the first dispatch of every
        // valid call. Idempotent, so retried calls after publication failure still publish.
        self.publish_codex_route(&context.meta).await;
        let (outcome, tag) = self
            .facade
            .dispatch_at_tagged(runtime_dir, &host, tool, parameters.clone())
            .await;
        let tag = tag.flatten();
        let recovery_needed = matches!(
            &outcome,
            FacadeOutcome::Reply(reply, _) if matches!(
                reply.as_ref(),
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(HostBindingCause::RecoveryNeeded),
                }
            )
        );
        let (Some(reconnect), Some(actor), true) = (&self.reconnect, tag.clone(), recovery_needed)
        else {
            return (outcome, tag);
        };
        {
            let _lifecycle = reconnect.lifecycle.lock().await;
            if let Some(remembered) = reconnect.remembered(&actor) {
                self.reactivate(
                    runtime_dir,
                    attachment,
                    remembered,
                    json!(host.correlation_id),
                    Some(&actor),
                )
                .await;
            }
        }
        let Some(mut host) = self.build_host(attachment, context) else {
            return (FacadeOutcome::MissingHostMetadata, None);
        };
        // The repeat belongs to the actor just restored: a pre now owned by anyone else refuses.
        self.announce(&mut host, Some(&actor), Some(&actor));
        let (outcome, tag) = self
            .facade
            .dispatch_at_tagged(runtime_dir, &host, tool, parameters.clone())
            .await;
        (outcome, tag.flatten())
    }

    /// Adds this managed Claude front's private announcements to one call's host metadata:
    /// `claudecode/recover` (empty until a daemon replacement, then the remembered actor tags
    /// except `exclude`) and, when the actor was already identified, `claudecode/actor`. Other
    /// hosts and fronts send nothing extra; older daemons read neither key.
    fn announce(&self, host: &mut TrustedTransport, exclude: Option<&str>, expected: Option<&str>) {
        let Some(reconnect) = self
            .reconnect
            .as_ref()
            .filter(|reconnect| reconnect.note.is_some())
        else {
            return;
        };
        let Some(Value::Object(meta)) = host.host_meta.as_mut() else {
            return;
        };
        if !meta.contains_key("claudecode/toolUseId") {
            return;
        }
        let tags = if reconnect
            .replaced
            .load(std::sync::atomic::Ordering::Acquire)
        {
            reconnect.remembered_tags(exclude)
        } else {
            Vec::new()
        };
        meta.insert("claudecode/recover".into(), json!(tags));
        if let Some(expected) = expected {
            meta.insert("claudecode/actor".into(), json!(expected));
        }
    }

    /// Re-attaches after the shared daemon generation ended and marks the session for transparent
    /// re-activation (T15B restart recovery); called from the lease watcher the moment the held
    /// lease stream observes the daemon's end, and safe to repeat.
    ///
    /// Returns whether a live daemon is attached again. Re-activation itself waits for the next
    /// dispatched call, whose own pre-hook names the actor to restore.
    pub async fn recover_lost_daemon(&self) -> bool {
        let Some(reconnect) = &self.reconnect else {
            return false;
        };
        reconnect.mark_replaced();
        let Some((runtime, attachment)) = (reconnect.reestablish)().await else {
            return false;
        };
        reconnect.store(runtime.clone(), attachment.clone()).await;
        // Nothing is re-activated here: with no call in hand, any pre on this attachment could
        // lend its actor to another's start. The next ordinary call recovers (see
        // `dispatch_with_reconnect`).
        true
    }

    /// Re-runs the 0.10.2 single remembered activation (see [`Self::reactivate`]); `marker` names
    /// the call about to be dispatched, or is `true` when none is.
    async fn reactivate_remembered_binding(
        &self,
        runtime_dir: &Path,
        attachment: &str,
        marker: Value,
    ) {
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
        if self
            .reactivate(runtime_dir, attachment, parameters, marker, None)
            .await
        {
            reconnect.mark_activated();
        }
    }

    /// Re-runs one remembered start through the ordinary `ide.start` path with the trusted
    /// `claudecode/reactivation` host marker (T15B restart recovery); reports success.
    ///
    /// The daemon admits the root by the same `allowed_roots` rule as any start. `marker` names
    /// the real call whose genuine pending pre-hook lends its actor (`true`: any pre delivered
    /// through this attachment, the 0.10.2 form an older daemon also understands), and
    /// `expected`, when known, is that actor's tag, so no other actor can be bound in its place.
    async fn reactivate(
        &self,
        runtime_dir: &Path,
        attachment: &str,
        parameters: Value,
        marker: Value,
        expected: Option<&str>,
    ) -> bool {
        let key = marker.as_str().map_or_else(
            || {
                parameters["activation_id"]
                    .as_str()
                    .unwrap_or("start")
                    .to_owned()
            },
            str::to_owned,
        );
        // Each attempt carries a fresh call identity (a synthetic call never receives its own
        // post-hook, so its settling entry could not be reused), and an activation retry reuses
        // the committed facts of the one in flight, so a bounded second attempt settles a first
        // `pending` quickly.
        for attempt in 0..3 {
            let call = synthetic_call_id(&format!("reactivate-{attempt}"), &key);
            let Some(mut host) =
                TrustedTransport::from_host_ingress(&call, &call, attachment.to_owned())
            else {
                return false;
            };
            let mut meta = json!({
                "claudecode/toolUseId": call,
                "claudecode/reactivation": marker,
            });
            if let Some(expected) = expected {
                meta["claudecode/actor"] = json!(expected);
            }
            host.host_meta = Some(meta);
            let FacadeOutcome::Reply(reply, _) = self
                .facade
                .dispatch_at(
                    runtime_dir,
                    &host,
                    AssistanceTool::Start,
                    parameters.clone(),
                )
                .await
            else {
                // A lost or late reply may already have applied: never send another.
                return false;
            };
            match reply.as_ref() {
                PeerReply::Complete {
                    kind: ResultKind::Activation,
                    ..
                } => return true,
                // Still activating, or its pre-hook not there yet: one more bounded attempt.
                PeerReply::Pending { .. }
                | PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(HostBindingCause::MissingPre),
                } => {}
                _ => return false,
            }
        }
        false
    }

    /// Asks the daemon for the private tag of the actor whose pending pre belongs to this call,
    /// without sending the call or touching that pre.
    ///
    /// The query carries its own synthetic call id and names the real one only in
    /// `claudecode/whois`. A current daemon always answers inside the tag wrapper, naming the
    /// actor or, with a `null` actor, refusing for the guard's exact reason. A daemon without the
    /// query sees an ordinary call without any pre and refuses it unwrapped with `missing_pre`,
    /// leaving the real pre untouched: that alone proves the query unsupported. Any other
    /// unwrapped refusal (an unknown attachment, for one) is a refusal, never that proof.
    async fn identify_actor(&self, context: &RequestContext<RoleServer>) -> StopIdentity {
        let Ok(call) = parse_claude_call_id(&context.meta) else {
            return StopIdentity::Refused(FacadeOutcome::MissingHostMetadata);
        };
        let Some((runtime_dir, attachment)) = self.current_connection().await else {
            return StopIdentity::Refused(FacadeOutcome::MissingHostMetadata);
        };
        let query = synthetic_call_id("whois", &call);
        let Some(mut host) = TrustedTransport::from_host_ingress(&query, &query, attachment) else {
            return StopIdentity::Refused(FacadeOutcome::MissingHostMetadata);
        };
        host.host_meta = Some(json!({
            "claudecode/toolUseId": query,
            "claudecode/whois": call,
            "claudecode/recover": [],
        }));
        match self
            .facade
            .dispatch_at_tagged(
                &runtime_dir,
                &host,
                AssistanceTool::Context,
                json!({"kind": "problems"}),
            )
            .await
        {
            // Only the exact identity answer names an actor; anything else proves nothing.
            (FacadeOutcome::Reply(reply, _), Some(Some(tag)))
                if matches!(
                    reply.as_ref(),
                    PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: None,
                    }
                ) =>
            {
                StopIdentity::Actor(tag)
            }
            (FacadeOutcome::Reply(reply, _), None)
                if matches!(
                    reply.as_ref(),
                    PeerReply::Unavailable {
                        reason: MissingPeer::HostBinding,
                        cause: Some(HostBindingCause::MissingPre),
                    }
                ) =>
            {
                StopIdentity::Unsupported
            }
            (outcome, _) => StopIdentity::Refused(outcome),
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
            && reconnect.activated_before().await
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

    /// Reads this session's daemon-currency knowledge for one about-to-render reply (0.6.7).
    ///
    /// Two honest additions, both learned at the rendezvous: the start card carries one line
    /// naming the older daemon still serving this session and why it was not replaced, and the
    /// first reply after an attach *replaced* an outdated daemon carries the same restart guidance
    /// a mid-call replacement gives — that reply's own pre-hook observations died with the old
    /// daemon, so a repeat against the new one is exactly what pairs the session again.
    async fn daemon_currency_due(
        &self,
        tool: AssistanceTool,
        reply: &PeerReply,
    ) -> (Option<String>, Option<&'static str>) {
        let Some(reconnect) = &self.reconnect else {
            return (None, None);
        };
        let Some(shared) = &reconnect.note else {
            return (None, None);
        };
        let mut note = shared.lock().expect("daemon currency note mutex");
        let activation = matches!(
            reply,
            PeerReply::Complete {
                kind: ResultKind::Activation,
                ..
            }
        );
        let host_binding = matches!(
            reply,
            PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                ..
            }
        );
        let line = if tool == AssistanceTool::Start && (activation || host_binding) {
            note.line().map(str::to_owned)
        } else {
            None
        };
        let hint = if (activation || host_binding) && note.take_replaced() {
            Some(if tool == AssistanceTool::Start {
                RECONNECT_RETRY_HINT
            } else {
                RECONNECT_START_HINT
            })
        } else {
            None
        };
        (line, hint)
    }

    /// The recovery hint for a root-less `ide.start` the daemon refused on its own missing
    /// pre-hook (F2, 0.6.5): such a start never re-roots, so it would otherwise end with no
    /// guidance at all.
    ///
    /// Only a session that activated before gets one, and it names the current directory as the
    /// root — the exact argument that lets the next start re-root deliberately, because a start
    /// naming a root does re-pair on this same refusal.
    async fn missing_pre_start_hint(
        &self,
        tool: AssistanceTool,
        parameters: &Value,
        reply: &PeerReply,
    ) -> Option<String> {
        if tool != AssistanceTool::Start
            || parameters.get("root").and_then(Value::as_str).is_some()
            || !matches!(
                reply,
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(HostBindingCause::MissingPre),
                }
            )
        {
            return None;
        }
        let reconnect = self.reconnect.as_ref()?;
        if !reconnect.activated_before().await {
            return None;
        }
        let root = std::env::current_dir().ok()?;
        Some(format!(
            "call ide.start again naming the current directory: ide.start {{root: {}}}",
            root.display()
        ))
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
        if let Err(error) = validate_call(tool, parameters.clone()) {
            return CallToolResult::error(vec![ContentBlock::text(error.message(tool))]);
        }
        let envelope = match parse_host_kind(&context.meta) {
            Ok(HostKind::Claude) => content::Envelope::TextOnly,
            _ => content::Envelope::WithStructured,
        };
        // Starts and stops are ordered with actor recovery and lease recovery, so a delayed
        // start reply can never re-remember an actor after its own stop.
        let _lifecycle = match (&self.reconnect, tool) {
            (Some(reconnect), AssistanceTool::Start | AssistanceTool::Stop) => {
                Some(reconnect.lifecycle.lock().await)
            }
            _ => None,
        };
        let replaced_before = self.binding_was_replaced().await;
        // What this front knew it had activated before the stop: the single 0.10.2 slot, or the
        // identified actor's own remembered start.
        let mut stop_intent = match &self.reconnect {
            Some(reconnect) => reconnect.last_activation.lock().await.is_some(),
            None => false,
        };
        // A stop names its actor before anything is sent, so its remembered start is forgotten
        // even when the stop's own reply is lost; an unidentified stop is not sent at all.
        let mut expected = None;
        // Tagged memory exists only after a daemon that supports the query answered with a tag;
        // the query itself is harmless on any other daemon.
        if tool == AssistanceTool::Stop
            && let Some(reconnect) = &self.reconnect
            && !reconnect.remembered_tags(None).is_empty()
        {
            match self.identify_actor(&context).await {
                StopIdentity::Actor(tag) => {
                    stop_intent = reconnect.remembered(&tag).is_some();
                    reconnect.forget(&tag);
                    expected = Some(tag);
                }
                // The daemon serving now cannot name actors (an older generation won a restart).
                // With one remembered actor the stop is that actor's and goes out as in 0.10.2;
                // with several, guessing could forget the wrong one, so nothing is sent (several
                // actors were never supported by such a daemon).
                StopIdentity::Unsupported if reconnect.remembered_tags(None).len() == 1 => {
                    let sole = reconnect.remembered_tags(None).remove(0);
                    stop_intent = true;
                    reconnect.forget(&sole);
                }
                StopIdentity::Unsupported => {
                    return CallToolResult::error(vec![ContentBlock::text(STOP_UNIDENTIFIED)]);
                }
                // Only a refusal can stand as the answer to a stop that was never sent.
                StopIdentity::Refused(FacadeOutcome::Reply(reply, status))
                    if matches!(reply.as_ref(), PeerReply::Unavailable { .. }) =>
                {
                    return render_reply_with_status(*reply, status.as_deref(), envelope);
                }
                StopIdentity::Refused(_) => {
                    return CallToolResult::error(vec![ContentBlock::text(STOP_UNIDENTIFIED)]);
                }
            }
        }
        let (outcome, resume, tag) = self
            .dispatch_with_reconnect(tool, parameters, &context, expected.as_deref())
            .await;
        // The Claude host hands `structuredContent` straight to its model in place of `content`,
        // defeating the compact renderer (T14B); its calls therefore never receive that duplicate
        // JSON copy. `parse_host_kind` reads the same trusted per-call `_meta` shape `build_host`
        // already establishes host identity from, so this holds for every managed Claude MCP mode
        // regardless of how the process itself was launched.
        // Every failed reply names its stage: the failing path's own tag when it set one, else
        // the derived `<tool>:<reason>` default — the same tag the daemon journal records.
        let outcome = match outcome {
            FacadeOutcome::Reply(mut reply, status) => {
                if let PeerReply::Error { code, detail } = reply.as_mut()
                    && detail.is_none()
                {
                    *detail = Some(staged_detail(tool, code, &stage_parameters));
                }
                FacadeOutcome::Reply(reply, status)
            }
            other => other,
        };
        // A current managed Claude daemon tags every start whose actor it resolved: an admitted
        // start (complete or still pending) is remembered under that actor, with its exact
        // parameters, for a transparent re-activation after a daemon replacement.
        if tool == AssistanceTool::Start
            && let Some(reconnect) = &self.reconnect
            && let Some(tag) = &tag
            && let FacadeOutcome::Reply(reply, _) = &outcome
            && matches!(
                reply.as_ref(),
                PeerReply::Pending { .. }
                    | PeerReply::Complete {
                        kind: ResultKind::Activation,
                        ..
                    }
            )
        {
            reconnect.remember(tag.clone(), stage_parameters.clone());
            // A daemon that names actors makes the anonymous 0.10.2 slot obsolete: it could only
            // ever bind some actor's pre to another actor's start.
            *reconnect.last_activation.lock().await = None;
            reconnect.mark_activated();
        }
        // Without a tag (an older daemon, a Codex host) the 0.10.2 single slot remembers every
        // successful activation's id and root (T15B restart recovery). Such a start proves the
        // daemon serving now names no actors, so tagged memory from a replaced daemon that did is
        // stale and is discarded: it could only ever re-bind an actor this daemon stopped.
        if tool == AssistanceTool::Start
            && tag.is_none()
            && let FacadeOutcome::Reply(reply, _) = &outcome
            && matches!(
                reply.as_ref(),
                PeerReply::Complete {
                    kind: ResultKind::Activation,
                    ..
                }
            )
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
            for stale in reconnect.remembered_tags(None) {
                reconnect.forget(&stale);
            }
            reconnect.mark_activated();
        }
        // A stop of a known activation that finds no binding because the daemon was replaced
        // (before or during this call) already achieved its goal: the replacement revoked
        // everything the stop would have revoked. Any other refusal stays honest.
        let binding_absent = |reply: &PeerReply| {
            matches!(
                reply,
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(
                        HostBindingCause::NeverActivated | HostBindingCause::InactiveBinding
                    ),
                }
            )
        };
        let outcome = match outcome {
            FacadeOutcome::Reply(reply, status)
                if tool == AssistanceTool::Stop
                    && stop_intent
                    && (replaced_before || resume == Resume::Restarted)
                    && binding_absent(reply.as_ref()) =>
            {
                FacadeOutcome::Reply(
                    Box::new(PeerReply::Complete {
                        kind: ResultKind::Stop,
                        text: "stopped (the IDE had already restarted)".into(),
                        detail_ref: None,
                        truncated: false,
                        continuation: false,
                    }),
                    status,
                )
            }
            other => other,
        };
        // An unidentified stop that may have applied ends the single remembered activation as
        // well, so a later replacement never re-activates a stopped session; an identified stop
        // already forgot exactly its own actor.
        let may_have_applied = match &outcome {
            FacadeOutcome::OutcomeUnknown | FacadeOutcome::TimedOut => true,
            FacadeOutcome::Reply(reply, _) => {
                matches!(
                    reply.as_ref(),
                    PeerReply::HostStopped {} | PeerReply::Complete { .. }
                )
            }
            _ => false,
        };
        if tool == AssistanceTool::Stop && expected.is_none() && may_have_applied {
            self.forget_remembered_activation().await;
        }
        let message = match outcome {
            FacadeOutcome::Reply(reply, status) if resume != Resume::Fresh => {
                let note = self.references_predate_replacement(&reply).await;
                let (line, hint) = self.daemon_currency_due(tool, &reply).await;
                let rendered =
                    render_reply_after_reconnect(tool, *reply, status.as_deref(), envelope, resume);
                return note_replaced_references(apply_daemon_currency(rendered, line, hint), note);
            }
            FacadeOutcome::Reply(reply, status) => {
                let note = self.references_predate_replacement(&reply).await;
                // F2 (0.6.5): a root-less start refused on its own missing pre-hook gets no
                // re-root and no guidance, even though this session activated before and naming
                // the current directory as the root is exactly the call that re-pairs it.
                let hint = self
                    .missing_pre_start_hint(tool, &stage_parameters, reply.as_ref())
                    .await;
                let (line, currency) = self.daemon_currency_due(tool, reply.as_ref()).await;
                let rendered = content::render_with_call(
                    (*reply).clone(),
                    status.as_deref(),
                    envelope,
                    Some(tool.mcp_name()),
                    (tool == AssistanceTool::Test)
                        .then(|| stage_parameters.get("status").and_then(Value::as_u64))
                        .flatten(),
                )
                .unwrap_or_else(|| render_reply_with_status(*reply, status.as_deref(), envelope));
                let rendered = match hint {
                    Some(hint) => with_retry_hint(rendered, &hint),
                    None => rendered,
                };
                return note_replaced_references(
                    apply_daemon_currency(rendered, line, currency),
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
            // The daemon refused the call before running it, so even a mutation applied nothing.
            FacadeOutcome::Busy => {
                "error: busy: the IDE is serving too many calls at once and did not run this one, so nothing was applied; repeat this call in a moment"
            }
            // The daemon refused the call before running it (and a managed client already tried
            // the replacement once), so even a mutation applied nothing.
            FacadeOutcome::Restarting => {
                "error: restarting: the IDE restarted after an internal fault and did not run this call, so nothing was applied; repeat this call"
            }
            // Never resent: the daemon may have executed the call, and a resend could only repeat
            // a change or be refused for its already consumed pre-hook.
            FacadeOutcome::OutcomeUnknown => match tool {
                AssistanceTool::Edit => {
                    "error: outcome_unknown: ide.edit may have reached the IDE and applied; verify with ide.diff before another edit"
                }
                AssistanceTool::Test => {
                    "error: outcome_unknown: ide.test may have reached the IDE and started; check ide.inspect or ide.test status before another run"
                }
                AssistanceTool::Start | AssistanceTool::Stop => {
                    "error: outcome_unknown: this call may have reached the IDE and applied; call ide.start to see the current binding before continuing"
                }
                _ => {
                    "error: outcome_unknown: this call may have reached the IDE but its reply was lost; repeat this call"
                }
            },
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
    // FAIL-01: the receipt is taken before the reply is consumed, so a render that cannot fit or
    // complete still reports an executed edit's outcome, identifiers and no-replay rule.
    let receipt = content::PresentationReceipt::of(&reply);
    content::render_with_status(reply, status, envelope).unwrap_or_else(|| {
        crate::errorlog::record(
            crate::errorlog::Method::Client,
            crate::errorlog::Outcome::Failed,
            crate::errorlog::Fields {
                reason: Some(crate::errorlog::ReasonCode::OversizeEnvelope),
                ..Default::default()
            },
        );
        receipt.map_or_else(
            || {
                CallToolResult::error(vec![ContentBlock::text(
                    "Assistance result exceeds the bounded envelope; continue with native tools",
                )])
            },
            content::PresentationReceipt::degraded,
        )
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
/// Recovery after this session re-rooted on a missing pre-hook alone (T15B): a repeat pairs when
/// the session truly moved, and a root-less start returns it to the host's project directory when
/// the one lost pre-hook left it there.
const REROOT_LATE_PRE_HINT: &str = "session re-rooted to the requested root; repeat this call once, or call ide.start without root";

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
    resume: Resume,
) -> CallToolResult {
    let is_host_binding_unavailable = matches!(
        reply,
        PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            ..
        }
    );
    let rendered = render_reply_with_status(reply, status, envelope);
    if !is_host_binding_unavailable {
        return rendered;
    }
    let hint = match resume {
        Resume::Fresh => return rendered,
        Resume::Restarted if tool == AssistanceTool::Start => RECONNECT_RETRY_HINT,
        Resume::Restarted => RECONNECT_START_HINT,
        Resume::Rerooted => REROOT_RETRY_HINT,
        Resume::RerootedWithoutPre => REROOT_LATE_PRE_HINT,
    };
    with_retry_hint(rendered, hint)
}

/// Appends one stable recovery hint to a rendered result: a `retry` field beside the structured
/// copy and the same sentence at the end of the compact text.
fn with_retry_hint(mut rendered: CallToolResult, hint: &str) -> CallToolResult {
    if let Some(Value::Object(fields)) = rendered.structured_content.as_mut() {
        fields.insert("retry".to_owned(), Value::String(hint.to_owned()));
    }
    if let Some(ContentBlock::Text(text)) = rendered.content.first_mut() {
        text.text = format!("{}; retry: {hint}", text.text);
    }
    rendered
}

/// Appends one plain sentence to a rendered result's compact text, changing no machine field.
fn append_text_note(mut rendered: CallToolResult, note: &str) -> CallToolResult {
    if let Some(ContentBlock::Text(text)) = rendered.content.first_mut() {
        text.text = format!("{}; {note}", text.text);
    }
    rendered
}

/// Applies the daemon-currency additions [`StdioFacade::daemon_currency_due`] selected: the honest
/// start-card line, then the one-shot restart hint.
fn apply_daemon_currency(
    mut rendered: CallToolResult,
    line: Option<String>,
    hint: Option<&'static str>,
) -> CallToolResult {
    if let Some(line) = line.as_deref() {
        rendered = append_text_note(rendered, line);
    }
    match hint {
        Some(hint) => with_retry_hint(rendered, hint),
        None => rendered,
    }
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
    /// The session re-rooted after the daemon proved this channel never delivered a hook there
    /// (T15B); the retried call pairs on its next pre-hook.
    Rerooted,
    /// The session re-rooted although only this call's pre-hook was missing (T15B): a repeat
    /// pairs when the session truly moved, and a root-less start returns it when it did not.
    RerootedWithoutPre,
}

/// Decides whether one daemon-refused `ide.start` should re-root, and where (T15B).
///
/// The refusal's own closed cause is the evidence, never the model's say-so alone:
/// `hooks_not_delivered` and `outside_allowed_roots` prove the channel never delivered one hook to
/// the daemon this facade dispatched against, and `missing_pre` proves only this call's pre-hook
/// never arrived. A start naming another root re-roots on any of the three — the session's hooks
/// demonstrably no longer pair where they must — and every other cause keeps the daemon's refusal,
/// because its hooks are arriving and moving the session would only strand it between two daemons.
/// A root-less start re-roots back to the host's own project directory on the two never-delivered
/// causes alone. No start re-roots while a daemon replacement still awaits its re-activation: the
/// replacement's fresh channel has observed no hook yet, so that replacement, not a moved session,
/// is then the explanation for the same refusal.
async fn reroot_target(
    parameters: &Value,
    outcome: &FacadeOutcome,
    reconnect: &ManagedConnection,
) -> Option<(Option<String>, Resume)> {
    let FacadeOutcome::Reply(reply, _) = outcome else {
        return None;
    };
    let PeerReply::Unavailable {
        reason: MissingPeer::HostBinding,
        cause: Some(cause),
    } = reply.as_ref()
    else {
        return None;
    };
    // Only the 0.10.2 single remembered activation still awaits an eager re-activation; tagged
    // actors recover per call and never hold up another actor's start.
    if reconnect
        .recovery_pending
        .load(std::sync::atomic::Ordering::Acquire)
        && reconnect.last_activation.lock().await.is_some()
    {
        return None;
    }
    let never_delivered = matches!(
        cause,
        HostBindingCause::HooksNotDelivered | HostBindingCause::OutsideAllowedRoots
    );
    match parameters.get("root").and_then(Value::as_str) {
        Some(root) => {
            if never_delivered {
                Some((Some(root.to_owned()), Resume::Rerooted))
            } else if matches!(cause, HostBindingCause::MissingPre) {
                Some((Some(root.to_owned()), Resume::RerootedWithoutPre))
            } else {
                None
            }
        }
        None => never_delivered.then_some((None, Resume::Rerooted)),
    }
}

/// A start naming another root does not re-root on a silent channel while a daemon replacement
/// awaits its re-activation (the replacement explains the refusal); once the binding is current
/// again the same refusal re-roots.
#[tokio::test]
async fn no_start_reroots_while_a_replacement_awaits_reactivation() {
    let reestablish: ReestablishFn = Arc::new(|| Box::pin(async { None }));
    let connection = ManagedConnection::new(PathBuf::from("/runtime"), "a".to_owned(), reestablish);
    let refused = FacadeOutcome::Reply(
        Box::new(PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause: Some(HostBindingCause::HooksNotDelivered),
        }),
        None,
    );
    let named = serde_json::json!({"activation_id": "x", "root": "/elsewhere"});
    let rootless = serde_json::json!({"activation_id": "x"});
    connection.mark_replaced();
    // Without a single remembered activation awaiting re-activation, nothing blocks the re-root.
    assert!(reroot_target(&named, &refused, &connection).await.is_some());
    *connection.last_activation.lock().await = Some(RememberedActivation {
        activation_id: "x".to_owned(),
        root: None,
    });
    assert!(reroot_target(&named, &refused, &connection).await.is_none());
    assert!(
        reroot_target(&rootless, &refused, &connection)
            .await
            .is_none()
    );
    connection.mark_activated();
    assert_eq!(
        reroot_target(&named, &refused, &connection).await,
        Some((Some("/elsewhere".to_owned()), Resume::Rerooted))
    );
    assert_eq!(
        reroot_target(&rootless, &refused, &connection).await,
        Some((None, Resume::Rerooted))
    );
}

/// The private actor wrapper is split only in its exact shape: a valid tag, or `null` for an
/// identity query no actor owns; every other reply passes through untouched.
#[test]
fn actor_wrapper_is_split_only_in_its_exact_shape() {
    let tag = "a".repeat(64);
    let inner = r#"{"state":"host_stopped"}"#;
    let wrapped = format!(r#"{{"actor":"{tag}","reply":{inner}}}"#);
    assert_eq!(
        untag_reply(&wrapped),
        (Some(Some(tag.clone())), inner.to_owned())
    );
    let unowned = format!(r#"{{"actor":null,"reply":{inner}}}"#);
    assert_eq!(untag_reply(&unowned), (Some(None), inner.to_owned()));
    for passthrough in [
        inner.to_owned(),
        format!(r#"{{"actor":"x","reply":{inner}}}"#),
        format!(r#"{{"actor":"{}","reply":{inner}}}"#, "A".repeat(64)),
        format!(r#"{{"actor":"{tag}","reply":{inner},"extra":1}}"#),
    ] {
        assert_eq!(untag_reply(&passthrough), (None, passthrough.clone()));
    }
}

/// Synthetic identity and re-activation call ids stay inside the transport bound even for a real
/// call id at that bound.
#[test]
fn synthetic_call_ids_stay_bounded_for_maximal_real_ids() {
    let real = "x".repeat(MAX_ACTIVATION_ID_BYTES);
    for prefix in ["whois", "reactivate-2"] {
        let call = synthetic_call_id(prefix, &real);
        assert!(
            TrustedTransport::from_host_ingress(&call, &call, "attachment".to_owned()).is_some()
        );
        assert_ne!(call, synthetic_call_id(prefix, "other"));
    }
}

/// A root-less missing-pre refusal tells a previously activated session how to pair again.
#[tokio::test]
async fn rootless_missing_pre_start_names_current_directory_hint() {
    let reestablish: ReestablishFn = Arc::new(|| Box::pin(async { None }));
    let reconnect = ManagedConnection::new(PathBuf::from("/runtime"), "a".to_owned(), reestablish);
    *reconnect.last_activation.lock().await = Some(RememberedActivation {
        activation_id: "prior-start".to_owned(),
        root: None,
    });
    let mut facade = StdioFacade::new(PathBuf::from("/runtime"));
    facade.reconnect = Some(reconnect);
    let reply = PeerReply::Unavailable {
        reason: MissingPeer::HostBinding,
        cause: Some(HostBindingCause::MissingPre),
    };
    let hint = facade
        .missing_pre_start_hint(
            AssistanceTool::Start,
            &json!({"activation_id":"retry"}),
            &reply,
        )
        .await
        .unwrap();
    assert!(hint.contains("ide.start {root:"));
    assert!(hint.contains(&std::env::current_dir().unwrap().display().to_string()));
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

/// FAIL-01: an edit reply too large for the bounded envelope still reports its outcome, path and
/// operation_id from the presentation receipt, keeps the no-replay rule for `outcome_unknown`, and keeps
/// the `is_error` value the normal projection would have set. An oversized note is the one
/// non-shrinkable field an Edit reply carries, so it is the shape that cannot fit.
#[test]
fn oversized_edit_replies_keep_outcome_operation_id_and_no_replay() {
    let oversized_note = |outcome| PeerReply::Edit {
        result: crate::changes::edit::EditResult::new(
            "operation-9".into(),
            "src/lib.rs".into(),
            outcome,
            outcome
                .has_post_source()
                .then(|| "source-after-edit".to_owned()),
        )
        .unwrap(),
        diagnostics: crate::assistance::reply::EditDiagnostics::Unknown {},
        note: Some("n".repeat(crate::assistance::reply::MAX_REPLY_BYTES)),
        operation: None,
    };
    let committed = render_reply_with_status(
        oversized_note(crate::changes::edit::EditOutcome::Replaced),
        None,
        content::Envelope::WithStructured,
    );
    let ContentBlock::Text(text) = &committed.content[0] else {
        panic!("sole content block must be text");
    };
    assert_eq!(
        text.text,
        "edit: replaced; path src/lib.rs; operation_id operation-9\n\
         Presentation degraded (presentation_failed). Do not repeat the mutation to repair \
         this response."
    );
    assert!(committed.structured_content.is_none());
    assert_ne!(committed.is_error, Some(true));

    // `outcome_unknown` renders no note, so the oversized carrier comes from a status plate the
    // daemon-side fitter would never attach; the facade loop and receipt are the same path.
    let unknown = render_reply_with_status(
        PeerReply::Edit {
            result: crate::changes::edit::EditResult::new(
                "operation-9".into(),
                "src/lib.rs".into(),
                crate::changes::edit::EditOutcome::OutcomeUnknown,
                None,
            )
            .unwrap(),
            diagnostics: crate::assistance::reply::EditDiagnostics::Unknown {},
            note: None,
            operation: None,
        },
        Some(&"x".repeat(crate::assistance::reply::MAX_REPLY_BYTES)),
        content::Envelope::TextOnly,
    );
    let ContentBlock::Text(text) = &unknown.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(
        text.text
            .starts_with("edit: outcome_unknown; path src/lib.rs; operation_id operation-9")
    );
    assert!(text.text.contains("do not replay this operation"));
    assert_ne!(unknown.is_error, Some(true));

    // A read that cannot be presented keeps the historical generic bounded-envelope error: it
    // carries no effect whose outcome must survive, and shrinking owner text cannot rescue it.
    let read = render_reply_with_status(
        PeerReply::Complete {
            kind: ResultKind::Context,
            text: "bounded owner evidence".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        },
        Some(&"x".repeat(crate::assistance::reply::MAX_REPLY_BYTES)),
        content::Envelope::TextOnly,
    );
    let ContentBlock::Text(text) = &read.content[0] else {
        panic!("sole content block must be text");
    };
    assert_eq!(
        text.text,
        "Assistance result exceeds the bounded envelope; continue with native tools"
    );
    assert_eq!(read.is_error, Some(true));
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
        Resume::Restarted,
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
        Resume::Restarted,
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
        Resume::Rerooted,
    );
    let ContentBlock::Text(text) = &rerooted.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(REROOT_RETRY_HINT), "{}", text.text);
    assert!(!text.text.contains(RECONNECT_RETRY_HINT), "{}", text.text);
    let late_pre = render_reply_after_reconnect(
        AssistanceTool::Start,
        PeerReply::Unavailable {
            reason: crate::assistance::reply::MissingPeer::HostBinding,
            cause: None,
        },
        None,
        content::Envelope::TextOnly,
        Resume::RerootedWithoutPre,
    );
    let ContentBlock::Text(text) = &late_pre.content[0] else {
        panic!("sole content block must be text");
    };
    assert!(text.text.contains(REROOT_LATE_PRE_HINT), "{}", text.text);
    assert!(
        text.text.contains("or call ide.start without root"),
        "{}",
        text.text
    );
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
    #[tool(name = "ide.start", input_schema = tool_schemas()[0].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.context", input_schema = tool_schemas()[1].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
    async fn context(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Context, parameters, context)
            .await
    }

    /// Diff of what this task changed in the working tree (`head`, `staged`, `unstaged` or `task`),
    /// paged. Review it before finishing or handing off, instead of running `git diff` in a
    /// shell.
    #[tool(name = "ide.diff", input_schema = tool_schemas()[2].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.outline", input_schema = tool_schemas()[6].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.read", input_schema = tool_schemas()[7].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.symbol", input_schema = tool_schemas()[8].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.graph", input_schema = tool_schemas()[9].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
    async fn graph(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Graph, parameters, context).await
    }

    /// Run tests selected by `symbol` (the tests that reference it), by `path`, by name
    /// `pattern`, or an explicit `command`; one background run per worktree under `budget_s`.
    /// Explicit commands may set a worktree-relative `cwd` and bounded inherited-environment
    /// overrides through `env`; their argv is limited to 16 KiB in total.
    /// Returns the pass/fail line with an exact rerun command; full output is paged through
    /// ide.inspect (`status` re-reads a run). Known runner summaries are parsed regardless of how
    /// the runner was launched; otherwise the result includes the exit code and bounded output
    /// tail. Use it instead of running the test command in a shell: bounded selection and output.
    #[tool(name = "ide.test", input_schema = tool_schemas()[10].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = false))]
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
    #[tool(name = "ide.inspect", input_schema = tool_schemas()[3].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = true, open_world_hint = false))]
    async fn inspect(
        &self,
        Parameters(parameters): Parameters<Value>,
        context: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.call(AssistanceTool::Inspect, parameters, context)
            .await
    }

    /// End this task's IDE session; edited files stay on disk. Lists up to eight test runs started
    /// in this binding whose results were never collected. Call once when done or before handing off.
    #[tool(name = "ide.stop", input_schema = tool_schemas()[4].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false))]
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
    #[tool(name = "ide.edit", input_schema = tool_schemas()[5].input_schema.as_object().expect("tool schema is an object").clone(),
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false))]
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

/// Private catalog cache lifetime in milliseconds for modern MCP requests (SEP-2549).
const TOOLS_LIST_TTL_MS: u64 = 60_000;

/// Reports whether one request speaks MCP 2026-07-28 or newer.
///
/// Modern requests carry their protocol version per request in `_meta` instead of the legacy
/// `initialize` handshake; `RequestContext::protocol_version` reads that first and falls back to
/// the legacy negotiated version. Protocol versions are ISO dates, so string order equals
/// version order. rmcp strips only `resultType` for legacy peers and never the cache hints, so
/// this gate alone decides which era's wire shape `tools/list` answers with.
fn modern_request(context: &RequestContext<RoleServer>) -> bool {
    context
        .protocol_version()
        .is_some_and(|version| version.as_str() >= ProtocolVersion::V_2026_07_28.as_str())
}

#[rmcp::tool_handler(router = self.router)]
impl rmcp::ServerHandler for StdioFacade {
    /// Advertises only the tool surface; no host sandbox metadata is requested. The identity is
    /// the product, not the SDK: rmcp's `ServerConfig::new` would expand its own crate name and
    /// version, so hosts would see `rmcp` `3.4.0` instead of the shipping binary.
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(
            rmcp::model::Implementation::new("agent-ide", env!("CARGO_PKG_VERSION"))
                .with_title("Agent IDE"),
        )
        .with_instructions(SERVER_INSTRUCTIONS)
    }

    /// Serves the static eleven-tool catalog, with private cache hints for modern requests only.
    ///
    /// The catalog is static per binary, but a short private TTL keeps a client from holding
    /// stale schemas across an upgrade; `ttlMs` and `cacheScope` stay absent for legacy sessions
    /// so their wire bytes never change. Overrides the `#[tool_handler]` default (which would
    /// answer modern requests with `ttlMs: 0`, `cacheScope: "public"`).
    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let mut result = ListToolsResult::with_all_items(self.router.list_all());
        if modern_request(&context) {
            result = result
                .with_ttl_ms(TOOLS_LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private);
        }
        Ok(result)
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
            "invalid bounded parameters: unknown field \"actor_id\"; allowed: activation_id, root, read_only, environment"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Stop, json!({"authority":1}))
                .unwrap_err(),
            AssistanceTool::Stop,
            "invalid bounded parameters: unknown field \"authority\"; allowed: activation_id"
                .to_string(),
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
            "invalid bounded parameters: \"mode\" must be \"head\", \"staged\", \"unstaged\", or \"task\""
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
                json!({"operation_id":"o","path":"a.rs","source_ref":"s","content":"x".repeat(MAX_EDIT_ARGUMENT_CONTENT_BYTES + 1)}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            format!(
                "invalid bounded parameters: \"content\" is longer than {MAX_EDIT_ARGUMENT_CONTENT_BYTES} bytes"
            ),
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
        (
            validate_call(AssistanceTool::Outline, json!({})).unwrap_err(),
            AssistanceTool::Outline,
            "invalid bounded parameters: ide.outline needs \"path\" (a file or directory relative to the worktree root)".to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"a.rs","lines":"1-2","content":"x"}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "invalid bounded parameters: \"source_ref\" is required for a line-range edit; \
             re-read the lines (ide.read) and retry with the new source_ref"
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"a.rs","changes":[{"old":"x","new":"y"}]}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "invalid bounded parameters: \"source_ref\" is required for an old-text change; re-read this file with ide.read and retry with the new source_ref".to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Read,
                json!({"symbol":"a.rs#run","path":"a.rs","lines":"1-2"}),
            )
            .unwrap_err(),
            AssistanceTool::Read,
            "ide.read needs one form: `symbol`, or `path` alone (the whole file), or `path` \
             with `lines`, or `path` with `ranges`, or `symbols` — exactly one"
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","path":"a.rs","changes":[{"lines":"1-2","content":"x"}],"content":"y"}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "ide.edit takes one form: \"changes\" with \"path\", or one single-change form \
             — not both"
                .to_string(),
        ),
        (
            validate_call(
                AssistanceTool::Edit,
                json!({"operation_id":"o","op":"replace","symbol":"a.rs#run","path":"b.rs","content":"x"}),
            )
            .unwrap_err(),
            AssistanceTool::Edit,
            "ide.edit takes one form: `symbol` with `op`, or `path` with `lines` and \
             `source_ref` — not both"
                .to_string(),
        ),
        (
            validate_call(AssistanceTool::Graph, json!({"symbol":"f","depth":0})).unwrap_err(),
            AssistanceTool::Graph,
            "invalid bounded parameters: \"depth\" must be an integer from 1 to 3".to_owned(),
        ),
        (
            validate_call(AssistanceTool::Test, json!({"symbol":"a.rs#f","budget_s":0}))
                .unwrap_err(),
            AssistanceTool::Test,
            "invalid bounded parameters: \"budget_s\" must be an integer from 1 to 600"
                .to_owned(),
        ),
    ]
}

/// Keeps explicit test commands useful for package work while enforcing their aggregate bounds.
#[cfg(test)]
#[test]
fn explicit_test_command_accepts_cwd_env_and_16k_argv() {
    let command = "x".repeat(MAX_COMMAND_BYTES);
    assert!(
        validate_call(
            AssistanceTool::Test,
            json!({"command":[command],"cwd":"packages/pkg","env":{"PYTHONPATH":"src:vendor"}}),
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Test,
            json!({"command":["x".repeat(MAX_COMMAND_BYTES + 1)]}),
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Test,
            json!({"command":["echo"],"cwd":"../../outside"}),
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Test,
            json!({"command":["echo"],"env":{"bad-name":"value"}}),
        )
        .is_err()
    );
}

/// A line-range edit carrying the read it came from validates; the symbol form's `source_ref`
/// stays optional and tolerates a redundant `path` naming the symbol's own file.
#[test]
fn range_edit_accepts_the_read_it_came_from() {
    assert!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"o","path":"a.rs","lines":"1-2","source_ref":"s","content":"x"})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"o","op":"replace","symbol":"a.rs#run","content":"x"})
        )
        .is_ok()
    );
    assert!(validate_call(
        AssistanceTool::Edit,
        json!({"operation_id":"o","op":"replace","symbol":"a.rs#run","source_ref":"s","content":"x"})
    )
    .is_ok());
    // A redundant `path` naming the symbol's own file is accepted and ignored.
    assert!(validate_call(
        AssistanceTool::Edit,
        json!({"operation_id":"o","op":"replace","symbol":"a.rs#run","path":"a.rs","content":"x"})
    )
    .is_ok());
    // `lines` belongs to the path form alone, even beside a symbol whose file is named.
    assert!(validate_call(
        AssistanceTool::Edit,
        json!({"operation_id":"o","op":"replace","symbol":"a.rs#run","path":"a.rs","lines":"1-2","source_ref":"s","content":"x"})
    )
    .is_err());
}

/// The `changes` shape matrix: one address per entry, payloads per operation, `source_ref`
/// required exactly when an entry addresses by lines or text, and no single-change form beside.
#[test]
fn edit_changes_shape_matrix() {
    let batch = |source_ref: bool, changes: Value| {
        let mut call = json!({"operation_id":"o","path":"src/x.rs","changes":changes});
        if source_ref {
            call["source_ref"] = json!("read-1");
        }
        validate_call(AssistanceTool::Edit, call)
    };
    // Every address form is admitted with its payload; lines/old force the source_ref.
    assert!(batch(true, json!([{"lines":"3-4","content":"x"}])).is_ok());
    assert!(
        batch(true, json!([{"old":"a","new":"b","within":"src/x.rs#F"}])).is_ok(),
        "old/within entries resolve"
    );
    assert!(batch(false, json!([{"symbol":"src/x.rs#F","op":"delete"}])).is_ok());
    assert!(
        batch(
            false,
            json!([{"symbol":"src/x.rs#F","op":"replace","content":"x"}])
        )
        .is_ok()
    );
    assert!(
        batch(
            false,
            json!([{"symbol":"src/x.rs#F","op":"insert","where":"after","content":"x"}])
        )
        .is_ok()
    );
    // A lines entry without source_ref is refused on the source_ref rule.
    assert_eq!(
        batch(false, json!([{"lines":"3-4","content":"x"}])).unwrap_err(),
        invalid_field("source_ref", FieldRule::RangeEditSourceRef)
    );
    assert_eq!(
        batch(false, json!([{"old":"a","new":"b"}])).unwrap_err(),
        invalid_field("source_ref", FieldRule::OldTextSourceRef)
    );
    // No address, two addresses, a bad op, a bad where, a missing payload.
    assert!(batch(true, json!([{"content":"x"}])).is_err());
    assert!(batch(true, json!([{"lines":"1-2","old":"a","new":"b"}])).is_err());
    assert!(
        batch(
            true,
            json!([{"symbol":"src/x.rs#F","op":"rename","new_name":"g"}])
        )
        .is_err()
    );
    assert!(
        batch(
            true,
            json!([{"symbol":"src/x.rs#F","op":"insert","where":"under","content":"x"}])
        )
        .is_err()
    );
    assert!(batch(true, json!([{"symbol":"src/x.rs#F","op":7,"content":"x"}])).is_err());
    assert!(batch(true, json!([{"old":"a","new":"b","within":7}])).is_err());
    assert!(batch(true, json!([{"symbol":"src/x.rs#F","op":"replace"}])).is_err());
    // Bounds: at most 32 entries.
    let many: Vec<Value> = (0..33)
        .map(|_| json!({"symbol":"src/x.rs#F","op":"delete"}))
        .collect();
    assert!(batch(false, json!(many)).is_err());
    // A single-change field beside `changes` is refused as a form mix.
    assert_eq!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"o","path":"src/x.rs","changes":[{"lines":"1-2","content":"x"}],"lines":"1-2","source_ref":"s"})
        )
        .unwrap_err(),
        ParameterError::ChangesTarget
    );
}

/// Batch reads validate their list shapes and stay exclusive with the single forms.
#[test]
fn read_symbols_and_ranges_validate() {
    assert!(
        validate_call(
            AssistanceTool::Read,
            json!({"symbols":["src/a.rs#F/g","src/b.rs#q"]})
        )
        .is_ok()
    );
    assert!(
        validate_call(
            AssistanceTool::Read,
            json!({"path":"src/x.rs","ranges":["10-20","44-60"]})
        )
        .is_ok()
    );
    // `path` alone reads the whole file, but not beside an empty batch list.
    assert!(validate_call(AssistanceTool::Read, json!({"path":"notes.md"})).is_ok());
    assert!(validate_call(AssistanceTool::Read, json!({"path":"/abs.md"})).is_err());
    assert!(validate_call(AssistanceTool::Read, json!({"path":"a.md","ranges":[]})).is_err());
    assert!(validate_call(AssistanceTool::Read, json!({"path":"a.md","symbols":[]})).is_err());
    // A sigil address is not a strict symbol path; ranges need `path`.
    assert!(
        validate_call(AssistanceTool::Read, json!({"symbols":["#main"]})).is_err(),
        "a sigil stays on the single form"
    );
    assert!(validate_call(AssistanceTool::Read, json!({"symbols":["src/a.rs#"]})).is_err());
    assert!(validate_call(AssistanceTool::Read, json!({"ranges":["10-20"]})).is_err());
    // The two batch forms are exclusive with every single form.
    assert!(
        validate_call(
            AssistanceTool::Read,
            json!({"symbols":["src/a.rs#F"],"path":"src/a.rs"})
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Read,
            json!({"path":"src/x.rs","ranges":["10-20"],"lines":"1-2"})
        )
        .is_err()
    );
    // At most 16 entries, each a range.
    let many: Vec<String> = (0..17).map(|index| format!("{index}-20")).collect();
    let mut call = json!({"path":"src/x.rs"});
    call["ranges"] = json!(many);
    assert!(validate_call(AssistanceTool::Read, call).is_err());
    let mut call = json!({});
    call["symbols"] = json!((0..17).map(|_| "a.rs#F").collect::<Vec<_>>());
    assert!(validate_call(AssistanceTool::Read, call).is_err());
}

/// The full-file form without `source_ref` validates as a creation request whose forwarded
/// parameters carry no reference (the worker observes the path itself); with one it stays the
/// canonical replace request, and path/content rules still hold for both.
#[test]
fn full_file_edit_without_source_ref_is_a_creation_request() {
    let created = validate_call(
        AssistanceTool::Edit,
        json!({"operation_id":"o","path":"src/new.rs","content":"x"}),
    )
    .unwrap();
    assert_eq!(
        created.parameters(),
        &json!({"operation_id":"o","path":"src/new.rs","content":"x"})
    );
    let replaced = validate_call(
        AssistanceTool::Edit,
        json!({"operation_id":"o","path":"a.rs","source_ref":"s","content":"x"}),
    )
    .unwrap();
    assert_eq!(replaced.parameters()["source_ref"], "s");
    assert!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"o","path":"/abs.rs","content":"x"})
        )
        .is_err()
    );
    assert!(
        validate_call(
            AssistanceTool::Edit,
            json!({"operation_id":"o","path":"a.rs","source_ref":"","content":"x"})
        )
        .is_err()
    );
    assert_eq!(
        ParameterError::InvalidField {
            field: "source_ref",
            rule: FieldRule::ReplaceSourceRef,
        }
        .message(AssistanceTool::Edit),
        "invalid bounded parameters: \"source_ref\" is required to replace an existing file: \
         read it first (ide.read)"
    );
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
            json!({"activation_id":"a","root":"/private/tmp/work","read_only":true})
        )
        .is_ok()
    );
    assert!(validate_call(AssistanceTool::Start, json!({"read_only":false})).is_ok());
    assert_eq!(
        validate_call(AssistanceTool::Start, json!({"read_only":"yes"})).unwrap_err(),
        ParameterError::InvalidField {
            field: "read_only",
            rule: FieldRule::Boolean,
        }
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

/// Environment maps accept bounded selectors and relative roots while refusing malformed payloads.
#[test]
fn start_environment_parameter_validation() {
    let accepted = [
        json!({"alpha":"two"}),
        json!({"alpha:packages/one":"auto"}),
        json!({"unknown":"two"}),
    ];
    for environment in accepted {
        assert!(validate_call(AssistanceTool::Start, json!({"environment":environment})).is_ok());
    }
    for environment in [
        json!([]),
        json!({"alpha":42}),
        json!({"alpha":""}),
        json!({"alpha:../escape":"two"}),
        json!({"alpha:/absolute":"two"}),
        json!({"alpha:":"two"}),
        json!({"alpha":"x".repeat(1025)}),
        json!({"alpha":"nul\u{0}"}),
    ] {
        assert!(validate_call(AssistanceTool::Start, json!({"environment":environment})).is_err());
    }
    let choices: serde_json::Map<String, Value> = (0..9)
        .map(|i| (format!("alpha:root{i}"), json!("two")))
        .collect();
    assert!(validate_call(AssistanceTool::Start, json!({"environment":choices})).is_err());
}

/// In-process MCP regressions for the managed Claude front against a scripted daemon socket.
#[cfg(test)]
mod managed_claude_front_tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

    /// A private runtime directory whose scripted daemon listens at `bound` (not necessarily the
    /// daemon socket name, so a test can make the first connect fail).
    struct ScriptedDaemon {
        runtime: PathBuf,
        listener: tokio::net::UnixListener,
    }

    impl ScriptedDaemon {
        /// Binds the scripted daemon at `runtime/<bound>`.
        fn new(bound: &str) -> Self {
            use std::os::unix::fs::DirBuilderExt;
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let runtime = std::env::temp_dir().join(format!(
                "ide-front-{}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&runtime)
                .unwrap();
            let listener = tokio::net::UnixListener::bind(runtime.join(bound)).unwrap();
            Self { runtime, listener }
        }

        /// Builds a managed Claude front on this runtime with attachment `old`.
        fn front(&self, reestablish: ReestablishFn) -> StdioFacade {
            StdioFacade::with_reestablishing_claude_attachment(
                self.runtime.clone(),
                "old".into(),
                PathBuf::from("/repo"),
                reestablish,
                Arc::new(|_, _| Box::pin(async { RerootOutcome::Unchanged })),
                Arc::new(std::sync::Mutex::new(DaemonCurrencyNote::default())),
            )
            .unwrap()
        }

        /// Accepts one request, or `None` when none arrives within a short window.
        async fn request(&self) -> Option<(tokio::net::UnixStream, Value)> {
            let (mut stream, _) =
                tokio::time::timeout(Duration::from_millis(1500), self.listener.accept())
                    .await
                    .ok()?
                    .unwrap();
            let size = stream.read_u32().await.unwrap();
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            Some((stream, serde_json::from_slice(&bytes).unwrap()))
        }
    }

    impl Drop for ScriptedDaemon {
        /// Removes only this fixture's private runtime directory.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.runtime);
        }
    }

    /// Answers one request with `result` as its opaque method result.
    async fn answer(mut stream: tokio::net::UnixStream, request: &Value, result: Value) {
        let reply = json!({"version":request["version"],"request_id":request["request_id"],"opaque_result_json":result})
            .to_string();
        stream.write_u32(reply.len() as u32).await.unwrap();
        stream.write_all(reply.as_bytes()).await.unwrap();
    }

    /// Runs one MCP handshake and one Claude `tools/call` against `front`, returning its text.
    async fn claude_call(front: StdioFacade, name: &str, arguments: Value) -> String {
        use rmcp::ServiceExt;
        let (server, client) = tokio::io::duplex(1 << 16);
        let serving = tokio::spawn(async move {
            front.serve(server).await.unwrap().waiting().await.unwrap();
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut reader = tokio::io::BufReader::new(reader);
        writer.write_all(concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\",\"params\":",
            "{\"protocolVersion\":\"2025-03-26\",\"capabilities\":{},\"clientInfo\":{\"name\":\"front\",\"version\":\"1\"}}}\n"
        ).as_bytes()).await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        let call = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":name,"arguments":arguments,"_meta":{"claudecode/toolUseId":"real-call"}
        }});
        writer
            .write_all(format!("{call}\n").as_bytes())
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                line.clear();
                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                let reply: Value = serde_json::from_str(&line).unwrap();
                if reply["id"] == json!(1) {
                    break reply;
                }
            }
        })
        .await
        .unwrap();
        drop(reader);
        drop(writer);
        serving.await.unwrap();
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// A transient connect failure on a live daemon re-establishes the SAME pair: the call is
    /// retried once, and nothing is marked replaced or forgotten (F1).
    #[tokio::test]
    async fn live_daemon_connect_failure_retries_without_replacement() {
        let daemon = ScriptedDaemon::new("parked.sock");
        let runtime = daemon.runtime.clone();
        let reestablish: ReestablishFn = Arc::new(move || {
            let runtime = runtime.clone();
            Box::pin(async move {
                std::fs::rename(runtime.join("parked.sock"), runtime.join("agent-ide.sock"))
                    .unwrap();
                Some((runtime, "old".to_owned()))
            })
        });
        let front = daemon.front(reestablish);
        let reconnect = front.reconnect.clone().unwrap();
        reconnect.remember("a".repeat(64), json!({"activation_id":"kept"}));
        let peer = async {
            let (stream, request) = daemon.request().await.unwrap();
            assert_eq!(request["dispatch_method"], "read");
            assert_eq!(
                request["params_json"]["host_meta"]["claudecode/recover"],
                json!([])
            );
            let read = PeerReply::Complete {
                kind: ResultKind::Read,
                text: "pub fn value() {}".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(stream, &request, serde_json::to_value(read).unwrap()).await;
        };
        let (text, ()) = tokio::join!(
            claude_call(
                front,
                "ide.read",
                json!({"path":"src/lib.rs","lines":"1-2"})
            ),
            peer
        );
        assert!(text.contains("pub fn value()"), "{text}");
        assert!(
            !reconnect
                .replaced
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(reconnect.remembered_tags(None), vec!["a".repeat(64)]);
    }

    /// A stop is identified first with a synthetic call id; a daemon without the query (an
    /// unwrapped refusal) gets the real stop only when exactly one actor is remembered.
    #[tokio::test]
    async fn unsupported_identity_query_stops_only_a_sole_remembered_actor() {
        for (remembered, cause) in [
            (1, HostBindingCause::MissingPre),
            (2, HostBindingCause::MissingPre),
            (1, HostBindingCause::HooksNotDelivered),
        ] {
            let daemon = ScriptedDaemon::new("agent-ide.sock");
            let front = daemon.front(Arc::new(|| Box::pin(async { None })));
            let reconnect = front.reconnect.clone().unwrap();
            for index in 0..remembered {
                reconnect.remember(format!("{index}").repeat(64), json!({"activation_id":"x"}));
            }
            let unsupported = cause == HostBindingCause::MissingPre;
            let peer = async {
                let (stream, query) = daemon.request().await.unwrap();
                let meta = &query["params_json"]["host_meta"];
                assert_eq!(meta["claudecode/whois"], "real-call");
                assert_ne!(meta["claudecode/toolUseId"], "real-call");
                let refusal = serde_json::to_value(PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(cause.clone()),
                })
                .unwrap();
                answer(stream, &query, refusal).await;
                let stop = daemon.request().await;
                if let Some((stream, stop)) = stop {
                    assert_eq!(stop["dispatch_method"], "stop");
                    assert_eq!(
                        stop["params_json"]["host_meta"]["claudecode/toolUseId"],
                        "real-call"
                    );
                    answer(stream, &stop, json!({"state":"host_stopped"})).await;
                    true
                } else {
                    false
                }
            };
            let (text, sent) = tokio::join!(claude_call(front, "ide.stop", json!({})), peer);
            match (remembered, unsupported) {
                // An older daemon and one remembered actor: that actor's stop goes out.
                (1, true) => {
                    assert!(sent && !text.starts_with("error"), "{text}");
                    assert!(reconnect.remembered_tags(None).is_empty());
                }
                // An older daemon and several actors: nothing is guessed or sent.
                (_, true) => {
                    assert!(
                        !sent && text.starts_with("error: stop_unidentified"),
                        "{text}"
                    );
                    assert_eq!(reconnect.remembered_tags(None).len(), 2);
                }
                // Any other refusal is not proof of an older daemon: nothing sent, memory kept.
                _ => {
                    assert!(!sent && text.contains("(hooks_not_delivered)"), "{text}");
                    assert_eq!(reconnect.remembered_tags(None).len(), 1);
                }
            }
        }
    }

    /// A tagged identity answer that is not the exact identity reply names nobody: the stop is
    /// not sent, nothing is forgotten, and no success-shaped reply stands in for it.
    #[tokio::test]
    async fn malformed_identity_answer_forgets_nothing_and_sends_no_stop() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        let tag = "e".repeat(64);
        reconnect.remember(tag.clone(), json!({"activation_id":"x"}));
        let peer = async {
            let (stream, query) = daemon.request().await.unwrap();
            assert_eq!(
                query["params_json"]["host_meta"]["claudecode/whois"],
                "real-call"
            );
            answer(
                stream,
                &query,
                json!({"actor":tag,"reply":{"state":"host_stopped"}}),
            )
            .await;
            daemon.request().await.is_some()
        };
        let (text, sent) = tokio::join!(claude_call(front, "ide.stop", json!({})), peer);
        assert!(
            !sent && text.starts_with("error: stop_unidentified"),
            "{text}"
        );
        assert_eq!(reconnect.remembered_tags(None), vec![tag]);
    }

    /// A model-sized edit still reaches the daemon after a replacement, beside the largest
    /// announcement of remembered actors.
    #[tokio::test]
    async fn large_edit_fits_beside_the_largest_recovery_announcement() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        for index in 0..MAX_REMEMBERED_ACTORS {
            reconnect.remember(format!("{index:064x}"), json!({"activation_id":"x"}));
        }
        reconnect.mark_replaced();
        let content = "x".repeat(63 * 1024);
        let peer = async {
            let (stream, request) = daemon.request().await.unwrap();
            assert_eq!(request["dispatch_method"], "edit");
            assert_eq!(
                request["params_json"]["host_meta"]["claudecode/recover"]
                    .as_array()
                    .unwrap()
                    .len(),
                MAX_REMEMBERED_ACTORS
            );
            let refusal = serde_json::to_value(PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: Some(HostBindingCause::NeverActivated),
            })
            .unwrap();
            answer(stream, &request, refusal).await;
        };
        let (text, ()) = tokio::join!(
            claude_call(
                front,
                "ide.edit",
                json!({"operation_id":"large","path":"src/large.rs","content":content})
            ),
            peer
        );
        assert!(text.contains("(never_activated)"), "{text}");
    }

    /// After a replacement a call announces the remembered tags; on `recovery_needed` the front
    /// re-runs exactly that actor's remembered start for the real call, then repeats the call once
    /// without that tag and fenced to that actor.
    #[tokio::test]
    async fn recovery_needed_restores_the_named_actor_then_repeats_the_call_once() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        let (busy, idle) = ("b".repeat(64), "c".repeat(64));
        let remembered = json!({"activation_id":"busy","read_only":true});
        reconnect.remember(idle.clone(), json!({"activation_id":"idle"}));
        reconnect.remember(busy.clone(), remembered.clone());
        reconnect.mark_replaced();
        let peer = async {
            let (stream, first) = daemon.request().await.unwrap();
            let meta = &first["params_json"]["host_meta"];
            assert_eq!(
                meta["claudecode/recover"],
                json!([idle.clone(), busy.clone()])
            );
            assert!(meta["claudecode/actor"].is_null());
            let refusal = serde_json::to_value(PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: Some(HostBindingCause::RecoveryNeeded),
            })
            .unwrap();
            answer(stream, &first, json!({"actor":busy,"reply":refusal})).await;
            let (stream, start) = daemon.request().await.unwrap();
            assert_eq!(start["dispatch_method"], "start");
            assert_eq!(start["params_json"]["parameters"], remembered);
            let meta = &start["params_json"]["host_meta"];
            assert_eq!(meta["claudecode/reactivation"], "real-call");
            assert_eq!(meta["claudecode/actor"], json!(busy));
            let activation = PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "activated".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(stream, &start, serde_json::to_value(activation).unwrap()).await;
            let (stream, repeat) = daemon.request().await.unwrap();
            let meta = &repeat["params_json"]["host_meta"];
            assert_eq!(meta["claudecode/toolUseId"], "real-call");
            assert_eq!(meta["claudecode/recover"], json!([idle.clone()]));
            assert_eq!(meta["claudecode/actor"], json!(busy));
            let read = PeerReply::Complete {
                kind: ResultKind::Read,
                text: "pub fn value() {}".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(stream, &repeat, serde_json::to_value(read).unwrap()).await;
        };
        let (text, ()) = tokio::join!(
            claude_call(
                front,
                "ide.read",
                json!({"path":"src/lib.rs","lines":"1-2"})
            ),
            peer
        );
        assert!(text.contains("pub fn value()"), "{text}");
    }

    /// The anonymous 0.10.2 slot is re-activated before an ordinary call by naming that very
    /// call, never with an expected actor, and the call follows once.
    #[tokio::test]
    async fn anonymous_slot_recovery_names_the_ordinary_call() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        *reconnect.last_activation.lock().await = Some(RememberedActivation {
            activation_id: "anonymous".to_owned(),
            root: None,
        });
        reconnect.mark_replaced();
        let peer = async {
            let (stream, start) = daemon.request().await.unwrap();
            assert_eq!(start["dispatch_method"], "start");
            let meta = &start["params_json"]["host_meta"];
            assert_eq!(meta["claudecode/reactivation"], "real-call");
            assert!(meta["claudecode/actor"].is_null());
            let activation = PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "activated".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(stream, &start, serde_json::to_value(activation).unwrap()).await;
            let (stream, read) = daemon.request().await.unwrap();
            assert_eq!(
                read["params_json"]["host_meta"]["claudecode/toolUseId"],
                "real-call"
            );
            let refusal = serde_json::to_value(PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: Some(HostBindingCause::NeverActivated),
            })
            .unwrap();
            answer(stream, &read, refusal).await;
        };
        let (text, ()) = tokio::join!(
            claude_call(
                front,
                "ide.read",
                json!({"path":"src/lib.rs","lines":"1-2"})
            ),
            peer
        );
        assert!(text.contains("host_binding"), "{text}");
    }

    /// F-02: against an older daemon that identifies no actor, the single remembered activation
    /// counts as another actor for an unidentified cross-repository start, which is refused by
    /// name; a front with no remembered activation still lets the start re-root.
    #[tokio::test]
    async fn legacy_remembered_activation_blocks_an_unidentified_cross_repository_start() {
        for remembered in [true, false] {
            let daemon = ScriptedDaemon::new("agent-ide.sock");
            let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = Arc::clone(&asked);
            let front = StdioFacade::with_reestablishing_claude_attachment(
                daemon.runtime.clone(),
                "old".into(),
                PathBuf::from("/repo"),
                Arc::new(|| Box::pin(async { None })),
                Arc::new(move |_, others| {
                    seen.lock().unwrap().push(others);
                    Box::pin(async move {
                        if others {
                            RerootOutcome::OtherRepository
                        } else {
                            RerootOutcome::Unchanged
                        }
                    })
                }),
                Arc::new(std::sync::Mutex::new(DaemonCurrencyNote::default())),
            )
            .unwrap();
            if remembered {
                let reconnect = front.reconnect.clone().unwrap();
                *reconnect.last_activation.lock().await = Some(RememberedActivation {
                    activation_id: "legacy".to_owned(),
                    root: None,
                });
            }
            let peer = async {
                let (stream, start) = daemon.request().await.unwrap();
                assert_eq!(start["dispatch_method"], "start");
                let refusal = serde_json::to_value(PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(HostBindingCause::HooksNotDelivered),
                })
                .unwrap();
                answer(stream, &start, refusal).await;
            };
            let (text, ()) = tokio::join!(
                claude_call(
                    front,
                    "ide.start",
                    json!({"activation_id":"stranger","root":"/other"})
                ),
                peer
            );
            assert_eq!(*asked.lock().unwrap(), [remembered], "{text}");
            assert_eq!(
                text.contains("other_repository: bound to /repo, asked /other"),
                remembered,
                "{text}"
            );
        }
    }

    /// F-02: an actor the bounded recovery memory evicted may still be active, so the reroot
    /// guard keeps counting other actors after the overflow even when every remembered one has
    /// stopped.
    #[tokio::test]
    async fn evicted_actor_still_counts_as_another_actor_for_the_reroot_guard() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        assert!(!reconnect.has_other_actors(None).await);
        for index in 0..=MAX_REMEMBERED_ACTORS {
            reconnect.remember(format!("actor-{index}"), json!({}));
        }
        // Actor 0 was evicted from the memory; every remembered actor then stops.
        assert!(
            !reconnect
                .remembered_tags(None)
                .contains(&"actor-0".to_owned())
        );
        for index in 1..=MAX_REMEMBERED_ACTORS {
            reconnect.forget(&format!("actor-{index}"));
        }
        assert!(reconnect.remembered_tags(None).is_empty());
        assert!(
            reconnect.has_other_actors(Some("a-new-actor")).await,
            "the evicted actor may still be active"
        );
    }

    /// A tagged start discards the anonymous 0.10.2 slot and its pending eager recovery.
    #[tokio::test]
    async fn tagged_start_discards_the_anonymous_slot() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        *reconnect.last_activation.lock().await = Some(RememberedActivation {
            activation_id: "anonymous".to_owned(),
            root: None,
        });
        reconnect.mark_replaced();
        reconnect.mark_activated();
        let tag = "f".repeat(64);
        let peer = async {
            let (stream, request) = daemon.request().await.unwrap();
            assert_eq!(request["dispatch_method"], "start");
            let activation = PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "activated".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(
                stream,
                &request,
                json!({"actor":tag,"reply":serde_json::to_value(activation).unwrap()}),
            )
            .await;
        };
        let (text, ()) = tokio::join!(
            claude_call(front, "ide.start", json!({"activation_id":"tagged"})),
            peer
        );
        assert!(!text.starts_with("error"), "{text}");
        assert!(reconnect.last_activation.lock().await.is_none());
        assert_eq!(reconnect.remembered_tags(None), vec![tag]);
    }

    /// An untagged start (a daemon that names no actors now serves) discards tagged memory left
    /// by a replaced daemon that did, and fills the anonymous slot.
    #[tokio::test]
    async fn untagged_start_discards_stale_tagged_memory() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        reconnect.remember("4".repeat(64), json!({"activation_id":"a"}));
        reconnect.remember("5".repeat(64), json!({"activation_id":"b"}));
        let peer = async {
            let (stream, request) = daemon.request().await.unwrap();
            let activation = PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "activated".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            };
            answer(stream, &request, serde_json::to_value(activation).unwrap()).await;
        };
        let (text, ()) = tokio::join!(
            claude_call(front, "ide.start", json!({"activation_id":"old-mode"})),
            peer
        );
        assert!(!text.starts_with("error"), "{text}");
        assert!(reconnect.remembered_tags(None).is_empty());
        assert_eq!(
            reconnect
                .last_activation
                .lock()
                .await
                .as_ref()
                .unwrap()
                .activation_id,
            "old-mode"
        );
    }

    /// A stop clears only its own actor: an identified stop keeps the other actors and any
    /// anonymous slot, and a stop for an actor nobody remembered leaves nothing to rebind.
    #[tokio::test]
    async fn stop_clears_only_its_own_actor() {
        let (own, other, absent) = ("1".repeat(64), "2".repeat(64), "3".repeat(64));
        for (stopping, sibling) in [
            (own.clone(), other.clone()),
            (absent.clone(), other.clone()),
        ] {
            let daemon = ScriptedDaemon::new("agent-ide.sock");
            let front = daemon.front(Arc::new(|| Box::pin(async { None })));
            let reconnect = front.reconnect.clone().unwrap();
            reconnect.remember(own.clone(), json!({"activation_id":"own"}));
            reconnect.remember(sibling.clone(), json!({"activation_id":"sibling"}));
            *reconnect.last_activation.lock().await = Some(RememberedActivation {
                activation_id: "anonymous".to_owned(),
                root: None,
            });
            let peer = async {
                let (stream, query) = daemon.request().await.unwrap();
                let identity = serde_json::to_value(PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: None,
                })
                .unwrap();
                answer(stream, &query, json!({"actor":stopping,"reply":identity})).await;
                let (stream, stop) = daemon.request().await.unwrap();
                assert_eq!(
                    stop["params_json"]["host_meta"]["claudecode/actor"],
                    json!(stopping)
                );
                answer(stream, &stop, json!({"state":"host_stopped"})).await;
            };
            let ((), _text) = tokio::join!(peer, claude_call(front, "ide.stop", json!({})));
            let mut expected = vec![own.clone(), other.clone()];
            expected.retain(|tag| tag != &stopping);
            assert_eq!(reconnect.remembered_tags(None), expected);
            assert!(reconnect.last_activation.lock().await.is_some());
            assert!(reconnect.remembered(&absent).is_none());
        }
    }

    /// Lease-driven recovery only re-attaches: with no call in hand it re-activates nothing, so
    /// no pre on the attachment can lend its actor to the anonymous slot's start.
    #[tokio::test]
    async fn lease_recovery_reactivates_nothing() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let runtime = daemon.runtime.clone();
        let front = daemon.front(Arc::new(move || {
            let runtime = runtime.clone();
            Box::pin(async move { Some((runtime, "new".to_owned())) })
        }));
        let reconnect = front.reconnect.clone().unwrap();
        *reconnect.last_activation.lock().await = Some(RememberedActivation {
            activation_id: "anonymous".to_owned(),
            root: None,
        });
        let (healed, request) = tokio::join!(front.recover_lost_daemon(), daemon.request());
        assert!(healed);
        assert!(request.is_none(), "lease recovery must send nothing");
        assert!(
            reconnect
                .recovery_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(reconnect.current().await.1, "new");
    }

    /// A start answered inside the actor wrapper with no actor is malformed: it is never
    /// remembered as an untagged activation and reports an unknown outcome.
    #[tokio::test]
    async fn null_actor_start_reply_is_not_an_activation() {
        let daemon = ScriptedDaemon::new("agent-ide.sock");
        let front = daemon.front(Arc::new(|| Box::pin(async { None })));
        let reconnect = front.reconnect.clone().unwrap();
        let peer = async {
            let (stream, request) = daemon.request().await.unwrap();
            answer(
                stream,
                &request,
                json!({"actor":null,"reply":{"state":"complete","kind":"activation","text":"activated"}}),
            )
            .await;
        };
        let (text, ()) = tokio::join!(
            claude_call(front, "ide.start", json!({"activation_id":"x"})),
            peer
        );
        assert!(text.starts_with("error: outcome_unknown"), "{text}");
        assert!(!reconnect.activated_before().await);
    }
}
