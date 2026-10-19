//! Deterministic compact model content projected from validated Assistance replies.

use std::sync::OnceLock;

use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use rmcp::model::{CallToolResult, ContentBlock};

use super::reply::{
    EditBatchOutcome, FailureCode, MAX_REPLY_BYTES, MCP_RESERVE, PeerReply, ResultKind,
};
use crate::changes::edit::EditOutcome;

/// Serialized source-response budget, including framing and an optional structured copy.
/// Eight KiB stays below the recorded single-call outer 10k-token truncation boundary even for
/// dense text; callers aggregating several replies must still budget their outer envelope.
/// Producers use this same bound before advancing source pages or recording delivered blocks.
const MAX_SOURCE_REPLY_BYTES: usize = 8 * 1024;

/// Build-embedded MiniJinja source projecting every closed [`PeerReply`] state into its compact
/// model-facing text; the template owns the presentation so daemon code never formats reply text.
const REPLY_TEMPLATE: &str = include_str!("../../../../assets/mcp/reply.jinja");

/// Selects whether a projected [`CallToolResult`] also carries the duplicate typed
/// `structuredContent` copy alongside the compact `content` text block.
///
/// Both hosts show the model `structuredContent` in place of `content` when it is present, which
/// defeats the compact renderer and doubles the reply bytes. Every production reply therefore
/// uses [`Self::TextOnly`] (harness-reply-v1 R1); [`Self::WithStructured`] is reachable only
/// through the `AGENT_IDE_TEST_STRUCTURED_REPLIES` test seam (see [`reply_envelope`]).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Envelope {
    /// Omits `structuredContent` entirely; the compact `content` text is the only carrier.
    TextOnly,
    /// Carries both the compact `content` text and the complete typed `structuredContent` copy.
    /// Selected only by the test seam, so the product tests keep the typed reply as their oracle.
    WithStructured,
}

/// Returns the envelope every non-Claude reply is sized and carried in.
///
/// Always [`Envelope::TextOnly`] (harness-reply-v1 R1), for recognized and unrecognized hosts
/// alike. Only a `test-seams` build with `AGENT_IDE_TEST_STRUCTURED_REPLIES=1` (set for the
/// workspace tests by `.cargo/config.toml`) selects [`Envelope::WithStructured`]; a release build
/// never reads the seam.
pub(crate) fn reply_envelope() -> Envelope {
    match crate::test_seams::var("AGENT_IDE_TEST_STRUCTURED_REPLIES").as_deref() {
        Some("1") => Envelope::WithStructured,
        _ => Envelope::TextOnly,
    }
}

/// Renders one validated reply and shrinks only owner text until the final MCP envelope fits.
///
/// The returned result contains exactly one text content block and, when `envelope` is
/// [`Envelope::WithStructured`], the complete serialized reply in `structured_content`. Only
/// the replies [`reports_failure`] names set `is_error`; invalid serialization or a non-shrinkable
/// oversized result returns `None` without partially emitting identifiers.
///
/// `status` is the optional due status plate (T28B): the complete `<agent-ide>` plate text, which
/// prefixes the compact content text, separated by one newline, and is the leading `status` string
/// field of the structured copy. The plate itself is never shrunk or cut — only owner text is — so
/// a fitted result always carries it whole.
pub(crate) fn render_with_status(
    reply: PeerReply,
    status: Option<&str>,
    envelope: Envelope,
) -> Option<CallToolResult> {
    render_with_call(reply, status, envelope, None, None)
}

/// Renders with the tool name and optional `ide.test` status run id so inactive-binding guidance
/// can name the correct recovery action; all other replies keep their existing compact wording.
pub(crate) fn render_with_call(
    mut reply: PeerReply,
    status: Option<&str>,
    envelope: Envelope,
    tool_name: Option<&str>,
    test_status_id: Option<u64>,
) -> Option<CallToolResult> {
    loop {
        let rendered = project(&reply, status, envelope, tool_name, test_status_id)?;
        if reply_fits(&reply, &rendered) {
            return Some(rendered);
        }
        if !reply.shrink_text() {
            return None;
        }
    }
}

/// Reports whether one unchanged reply fits after projection into the exact final MCP carrier.
///
/// This predicate never shrinks text. Diff pagination measures whole hunks, exact line parts,
/// oversized-line notices and the actual continuation trailer before advancing its cursor.
pub(crate) fn fits(reply: &PeerReply, envelope: Envelope) -> bool {
    project(reply, None, envelope, None, None).is_some_and(|rendered| reply_fits(reply, &rendered))
}

/// Reports whether one unchanged reply fits the final MCP carrier with `status` attached whole.
///
/// The daemon-side twin of [`render_with_status`]'s fitting: a host without hook delivery
/// attaches a plate only after this accepts it, so the facade's own render can never be the
/// call that cuts it.
pub(crate) fn fits_with_status(reply: &PeerReply, status: &str, envelope: Envelope) -> bool {
    project(reply, Some(status), envelope, None, None)
        .is_some_and(|rendered| reply_fits(reply, &rendered))
}

/// Projects one unchanged reply into compact content plus, per `envelope`, the complete typed
/// structured value.
///
/// Serialization or template failure returns `None` regardless of `envelope`, so a value this
/// renderer cannot faithfully represent never silently drops its structured copy. A tagged
/// execution-profile refusal keeps the public structured `code` string stable; the closed cause
/// tag selects natural compact wording without being printed; likewise an error's
/// `detail` reaches the template as `resolution_detail` and is removed from the structured copy.
/// Projection performs no I/O, host inspection, diagnostics inference, or model call.
fn project(
    reply: &PeerReply,
    status: Option<&str>,
    envelope: Envelope,
    tool_name: Option<&str>,
    test_status_id: Option<u64>,
) -> Option<CallToolResult> {
    let mut structured = serde_json::to_value(reply).ok()?;
    let cause_tag = match reply {
        PeerReply::Error {
            code: FailureCode::ExecutionProfileCause(cause),
            ..
        } => Some(cause.tag().to_owned()),
        PeerReply::Unavailable {
            cause: Some(cause), ..
        } => Some(cause.cause_tag()),
        _ => None,
    };
    let mut context = structured.clone();
    // Strict-undefined rendering expects these fields on every reply.
    context["cause_tag"] = serde_json::Value::String(String::new());
    context["resolution_message"] = serde_json::Value::String(String::new());
    context["environment_key"] = serde_json::Value::String(String::new());
    context["environment_selector"] = serde_json::Value::String(String::new());
    context["environment_example"] = serde_json::Value::String(String::new());
    context["refusal_excerpt"] = serde_json::Value::String(String::new());
    context["tool_name"] = serde_json::Value::String(tool_name.unwrap_or_default().to_owned());
    context["test_status_id"] =
        test_status_id.map_or(serde_json::Value::Null, serde_json::Value::from);
    if let PeerReply::Error { detail, .. } = reply {
        let detail = detail.as_deref().unwrap_or_default();
        context["resolution_detail"] = serde_json::Value::String(detail.to_owned());
        context["resolution_message"] =
            serde_json::Value::String(resolution_message(detail).to_owned());
        if matches!(
            reply,
            PeerReply::Error {
                code: FailureCode::EditRefused,
                ..
            }
        ) {
            // Authorized producer text stays exact inside the separately quoted current-text
            // excerpt; labels outside that excerpt still pass display_safe below.
            context["refusal_excerpt"] =
                serde_json::Value::String(resolution_message(detail).to_owned());
        }
        if let Some((key, selector)) = environment_refusal(detail) {
            context["environment_key"] = serde_json::Value::String(key.clone());
            context["environment_selector"] = serde_json::Value::String(selector.to_owned());
            context["environment_example"] = serde_json::Value::String(format!(
                "{{{}: \".venv\"}}",
                serde_json::Value::String(key.clone())
            ));
        }
        if let Some(selection) = detail.strip_prefix("environment:") {
            context["refusal_excerpt"] = serde_json::Value::String(display_safe(selection.trim()));
        }
        if let Some(ancestor) = detail
            .strip_prefix("start:root_absent; nearest existing ancestor below an allowed root: ")
        {
            context["refusal_excerpt"] = serde_json::Value::String(ancestor.to_owned());
        }
        if let Some(fields) = structured.as_object_mut() {
            fields.remove("detail");
        }
    }
    if let PeerReply::Unavailable {
        cause: Some(super::reply::HostBindingCause::TakenOver { reason }),
        ..
    } = reply
    {
        // The takeover's fixed reason phrase completes the `taken_over` sentence.
        context["resolution_message"] = serde_json::Value::String(reason.clone());
    }
    if let PeerReply::Unavailable {
        cause:
            Some(
                super::reply::HostBindingCause::ProjectMoved { bound, asked }
                | super::reply::HostBindingCause::OtherRepository { bound, asked },
            ),
        ..
    } = reply
    {
        context["resolution_message"] = serde_json::Value::String(format!(
            "Bound repository: {bound}; requested root: {asked}"
        ));
    }
    if let PeerReply::Unavailable { .. } = reply {
        // The public structured shape keeps its historical fields; the closed cause reaches the
        // compact text only, exactly like an error's `detail` (T15B).
        for value in [&mut structured, &mut context] {
            if let Some(fields) = value.as_object_mut() {
                fields.remove("cause");
            }
        }
    }
    if let Some(tag) = cause_tag {
        if let PeerReply::Error {
            code: FailureCode::ExecutionProfileCause(_),
            ..
        } = reply
        {
            let public = serde_json::Value::String("execution_profile".to_owned());
            structured["code"] = public.clone();
            context["code"] = public;
        }
        context["cause_tag"] = serde_json::Value::String(tag);
    }
    escape_untrusted_labels(&mut context);
    let reply_text = render_text(&context)?;
    let structured = match status {
        Some(status) => match structured {
            serde_json::Value::Object(mut fields) => {
                let mut carried = serde_json::Map::new();
                carried.insert(
                    "status".to_owned(),
                    serde_json::Value::String(status.to_owned()),
                );
                carried.append(&mut fields);
                serde_json::Value::Object(carried)
            }
            other => other,
        },
        None => structured,
    };
    let text = match status {
        Some(status) => format!("{status}\n{reply_text}"),
        None => reply_text,
    };
    let content = vec![ContentBlock::text(text)];
    let failed = reports_failure(reply);
    let mut rendered = if failed {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    if matches!(envelope, Envelope::WithStructured) {
        rendered.structured_content = Some(structured);
    }
    rendered.is_error = failed.then_some(true);
    Some(rendered)
}

/// Whether a one-file edit `outcome` is not a write the call asked for: everything but a created,
/// replaced or unchanged file.
fn edit_outcome_failed(outcome: EditOutcome) -> bool {
    !matches!(
        outcome,
        EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged
    )
}

/// Whether `reply` tells the model its call did not do what it asked, so the MCP result is an
/// error: a closed failure, rejected parameters, or an edit that was refused, cancelled, left
/// unknown or applied only in part (everything but a created, replaced or unchanged file, and a
/// fully applied multi-file edit). A call that ran and answered a question, an unavailable
/// session plate or a pending result is not a failure.
fn reports_failure(reply: &PeerReply) -> bool {
    match reply {
        PeerReply::Error { .. } | PeerReply::InvalidParameters { .. } => true,
        PeerReply::Edit { result, .. } => edit_outcome_failed(result.outcome),
        PeerReply::EditBatch { outcome, .. } => *outcome != EditBatchOutcome::Applied,
        _ => false,
    }
}

/// Escapes every untrusted label the template interpolates into reply text (SAFE-02): paths,
/// symbol names, provider messages, and stage details may not forge structural lines or hide
/// reordering behind bidi marks. Exact content (`Complete.text`, edit notes) and validated
/// actionable references (`detail_ref`, `source_ref`) stay byte-exact; ordinary paths are not
/// quoted.
fn escape_untrusted_labels(context: &mut serde_json::Value) {
    fn field(value: &mut serde_json::Value, key: &str) {
        let Some(escaped) = value.get(key).and_then(|label| label.as_str()) else {
            return;
        };
        let escaped = display_safe(escaped);
        if let Some(slot) = value.as_object_mut().and_then(|fields| fields.get_mut(key)) {
            *slot = serde_json::Value::String(escaped);
        }
    }
    if context.get("state") == Some(&serde_json::Value::from("invalid_parameters")) {
        field(context, "text");
    }
    if let Some(code) = context.get_mut("code") {
        field(code, "no_such_file");
        field(code, "unsupported_file");
    }
    for key in [
        "resolution_detail",
        "resolution_message",
        "cause_tag",
        "environment_key",
        "environment_selector",
        "environment_example",
    ] {
        field(context, key);
    }
    if let Some(result) = context.get_mut("result") {
        field(result, "path");
    }
    if let Some(diagnostics) = context.get_mut("diagnostics") {
        field(diagnostics, "reason");
        if let Some(messages) = diagnostics
            .get_mut("messages")
            .and_then(|m| m.as_array_mut())
        {
            for message in messages {
                if let Some(label) = message.as_str() {
                    *message = serde_json::Value::String(display_safe(label));
                }
            }
        }
    }
}

/// Renders one untrusted label as visible single-line text: newline, carriage return, tab and
/// the remaining control characters become their escapes, and bidirectional format marks become
/// explicit `\u` escapes instead of silently reordering the line.
pub(crate) fn display_safe(label: &str) -> String {
    let mut escaped = String::with_capacity(label.len());
    for character in label.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            '\u{8}' => escaped.push_str("\\b"),
            '\u{c}' => escaped.push_str("\\f"),
            control if control.is_control() => {
                escaped.push_str(&format!("\\u{:04x}", control as u32));
            }
            '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => escaped.push_str(&format!("\\u{:04x}", character as u32)),
            visible => escaped.push(visible),
        }
    }
    escaped
}

/// Splits an environment-selector refusal detail, `environment "<root key>" <selector>`, into the
/// root key (a language id, or `<language id>:<relative root>`) and the selector exactly as the
/// agent sent it.
///
/// The worker JSON-quotes the key, so a root containing spaces, quotes or colons is read back
/// unchanged; the selector is everything after the single space that follows the quoted key and
/// may itself contain spaces. Returns `None` for every other detail, including an unquoted key.
fn environment_refusal(detail: &str) -> Option<(String, &str)> {
    let rest = detail.strip_prefix("environment ")?;
    let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
    let key = keys.next()?.ok()?;
    let selector = rest[keys.byte_offset()..].strip_prefix(' ')?;
    (!key.is_empty() && !selector.is_empty()).then_some((key, selector))
}

/// Extracts a producer's trailing detail payload without depending on prefix byte lengths.
///
/// For the no-server shape `<tool>:provider_unavailable ext=<ext>[ (provider: …)]` the payload is
/// the bare extension: it ends at the first space, so the optional trailing parenthesised stage
/// the journal carries never reaches the rendered sentence.
fn resolution_message(detail: &str) -> &str {
    let Some((_, stage_detail)) = detail.split_once(':') else {
        return detail;
    };
    if let Some(extension) = stage_detail.strip_prefix("provider_unavailable ext=") {
        // The extension ends at the space before the trailing parenthesised stage, when present.
        return extension.split(' ').next().unwrap_or(extension);
    }
    stage_detail
        .split_once(':')
        .map_or(stage_detail, |(_, message)| {
            message.strip_prefix(' ').unwrap_or(message)
        })
}

/// Returns the shared compile-time template environment for every MCP text projection.
///
/// Undefined behavior is strict, so a template reading a field the closed reply shape does not
/// carry for that state fails the render instead of silently omitting a fact, and auto-escaping
/// stays off because the carrier is plain text rather than a markup document. The environment is
/// built exactly once; [`REPLY_TEMPLATE`] is embedded at build time and its every branch is
/// exercised by this module's tests, so construction failure is a programmatic bug.
fn environment() -> &'static Environment<'static> {
    static ENVIRONMENT: OnceLock<Environment<'static>> = OnceLock::new();
    ENVIRONMENT.get_or_init(|| reply_environment(Some(RENDER_FUEL)))
}

/// Formats the typed template mapper's refusal as one <=160-byte head, exactly one
/// <=120-byte Repair line and an optional <=60-line/4096-byte producer excerpt. Head and
/// repair are escaped single-line labels; excerpt rows are quoted as data without parsing
/// away current source, effect or stale-safety information. Performs no I/O or inference.
fn refusal_text(head: String, repair: String, excerpt: Option<String>) -> String {
    let head = super::reply::bounded_utf8_prefix(&display_safe(&head), 160);
    let repair = super::reply::bounded_utf8_prefix(&display_safe(&repair), 112);
    let mut text = format!("{head}\nRepair: {repair}");
    if let Some(excerpt) = excerpt.filter(|text| !text.is_empty()) {
        let quoted = excerpt
            .lines()
            .take(60)
            .map(|line| format!("  {line}\n"))
            .collect::<String>();
        text.push('\n');
        text.push_str(&super::reply::bounded_utf8_prefix(&quoted, 4096));
    }
    text
}

/// Like [`refusal_text`] for a stale-edit refusal whose producer may supply its own note.
///
/// With no note (or an empty one) `head` and `repair` are used as given. A note of the form
/// `"<sentence>. Repair: <step>"` becomes the head (first letter capitalised, closed with
/// `; nothing was written.`) and the Repair line; a note without a `Repair:` part becomes the
/// head and keeps the default `repair`. Performs no I/O.
fn refusal_note(head: String, repair: String, note: Option<String>) -> String {
    let Some(note) = note.filter(|note| !note.trim().is_empty()) else {
        return refusal_text(head, repair, None);
    };
    let (sentence, step) = match note.split_once("Repair:") {
        Some((sentence, step)) => (sentence, step.trim().trim_end_matches('.').to_owned()),
        None => (note.as_str(), repair),
    };
    let sentence = sentence.trim().trim_end_matches('.');
    let mut letters = sentence.chars();
    let sentence = letters
        .next()
        .map(|first| first.to_uppercase().chain(letters).collect::<String>())
        .unwrap_or_default();
    refusal_text(format!("{sentence}; nothing was written."), step, None)
}

/// Longest path label, in bytes, that a refusal head carries after its effect sentence.
const PATH_LABEL_BYTES: usize = 60;

/// Shortens a worktree-relative path for a refusal head, keeping its tail.
///
/// A path of at most [`PATH_LABEL_BYTES`] bytes is returned unchanged; a longer one becomes `…`
/// plus the longest suffix that fits, cut at a UTF-8 boundary, because the file name and its
/// nearest directories identify the target best. The head formatter places the label after the
/// reason and effect sentence, so a long label can never push those facts past the 160-byte cut.
/// Performs no I/O.
fn path_label(path: String) -> String {
    if path.len() <= PATH_LABEL_BYTES {
        return path;
    }
    let mut start = path.len() - (PATH_LABEL_BYTES - '…'.len_utf8());
    while !path.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &path[start..])
}

/// VM-instruction budget for one reply render; a render that exceeds it fails into the
/// presentation-degraded fallback instead of looping. Keeps tenfold headroom over the costliest
/// real render (the final error fallbacks walk every `elif` of `reply.jinja`, so each added error
/// branch raises it).
const RENDER_FUEL: u64 = 6_000;
/// Deepest template nesting one render may reach; `reply.jinja` has no recursion at all.
const RENDER_RECURSION_LIMIT: usize = 16;

/// Builds the closed reply environment: no implicit filters, tests, functions or globals — only
/// the helpers `reply.jinja` actually uses are registered, so an added template construct fails
/// loudly here instead of silently widening the engine (JINJA-02).
///
/// Registered set: filter `join`; bounded refusal formatters (`refusal`, `refusal_note`) and the
/// `path_label` shortener; tests `defined`, `mapping`,
/// `startingwith`, `endingwith`. The formatter receives data, never template source.
fn reply_environment(fuel: Option<u64>) -> Environment<'static> {
    let mut environment = Environment::empty();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment.set_auto_escape_callback(|_| AutoEscape::None);
    environment.add_filter("join", minijinja::filters::join);
    environment.add_function("refusal", refusal_text);
    environment.add_function("refusal_note", refusal_note);
    environment.add_function("path_label", path_label);
    environment.add_test("defined", minijinja::tests::is_defined);
    environment.add_test("mapping", minijinja::tests::is_mapping);
    environment.add_test("startingwith", minijinja::tests::is_startingwith);
    environment.add_test("endingwith", minijinja::tests::is_endingwith);
    environment.set_fuel(fuel);
    environment.set_recursion_limit(RENDER_RECURSION_LIMIT);
    environment
        .add_template("reply.jinja", REPLY_TEMPLATE)
        .expect("embedded reply template parses");
    environment
}

/// Returns deterministic decision-facing text for one serialized validated reply.
///
/// `context` is the complete serialized [`PeerReply`] value, plus `cause_tag` for a tagged
/// execution-profile refusal. The static template begins with the closed reply state, preserves
/// exact pending commands and live detail references, and names no host metadata, telemetry,
/// provider errors, or inferred diagnostics. Untrusted reply text is bound strictly as data;
/// template sources are static build assets only. A strict-undefined, serialization, or empty
/// projection returns `None`, failing closed without partial text for future reply variants.
fn render_text(context: &serde_json::Value) -> Option<String> {
    environment()
        .get_template("reply.jinja")
        .ok()?
        .render(minijinja::Value::from_serialize(context))
        .ok()
        .filter(|text| !text.is_empty())
}

/// Applies the source budget to the complete rendered carrier before the ordinary transport
/// ceiling. Other result kinds retain their existing budget. No source is cut by this predicate.
fn reply_fits(reply: &PeerReply, rendered: &CallToolResult) -> bool {
    call_tool_result_fits(rendered)
        && (!matches!(
            reply,
            PeerReply::Complete {
                kind: ResultKind::Read,
                ..
            }
        ) || serde_json::to_vec(rendered)
            .is_ok_and(|bytes| bytes.len() <= MAX_SOURCE_REPLY_BYTES))
}

/// Returns whether the serialized final MCP carrier stays below the Assistance reply ceiling.
pub(crate) fn call_tool_result_fits(rendered: &CallToolResult) -> bool {
    serde_json::to_vec(rendered).is_ok_and(|bytes| bytes.len() <= MAX_REPLY_BYTES - MCP_RESERVE)
}

/// Immutable execution receipt kept outside the template context (FAIL-01): when a mutating reply
/// cannot be projected — the render exhausts its budget or the carrier cannot fit after shrinking
/// only owner text — the effect already happened, so the reply still reports the outcome, the exact
/// identifiers a retry must reuse, and whether the mutation may be repeated.
pub(crate) enum PresentationReceipt {
    /// One executed edit: durable outcome, display word, requested path, durable operation id,
    /// and the retained diagnostics reference when matching-generation work is inspectable.
    Edit {
        /// Durable closed outcome; alone decides the no-replay recovery sentence.
        outcome: EditOutcome,
        /// Outcome word exactly as the compact first line would name it.
        word: String,
        /// Requested path, display-escaped but never quoted.
        path: String,
        /// Durable operation id a retry or reconciliation must reuse.
        operation_id: String,
        /// `ide.inspect` reference for pending post-edit diagnostics, when one was retained.
        detail_ref: Option<String>,
    },
    /// Workspace authority was released; edited files stay on disk.
    Stop,
    /// A durable workspace activation now exists for this binding.
    Activation,
}

impl PresentationReceipt {
    /// Extracts the receipt only mutating replies carry; reads keep the historical generic
    /// bounded-envelope error, because their requested answer, not an effect, is what was lost.
    pub(crate) fn of(reply: &PeerReply) -> Option<Self> {
        match reply {
            PeerReply::Edit {
                result,
                diagnostics,
                operation,
                ..
            } => Some(Self::Edit {
                outcome: result.outcome,
                word: operation
                    .clone()
                    .unwrap_or_else(|| result.outcome.as_str().to_owned()),
                path: display_safe(&result.path),
                operation_id: display_safe(&result.operation_id),
                detail_ref: match diagnostics {
                    super::reply::EditDiagnostics::Pending { detail_ref } => {
                        Some(detail_ref.clone())
                    }
                    _ => None,
                },
            }),
            PeerReply::Complete {
                kind: ResultKind::Stop,
                ..
            } => Some(Self::Stop),
            PeerReply::Complete {
                kind: ResultKind::Activation,
                ..
            } => Some(Self::Activation),
            _ => None,
        }
    }

    /// Renders the small fixed-wording fallback from the receipt; `is_error` keeps the value the normal
    /// projection of the same reply would set ([`reports_failure`]), so a confirmed mutation stays
    /// success-shaped and a refused or unknown one stays an error.
    pub(crate) fn degraded(self) -> CallToolResult {
        let failed = match &self {
            Self::Edit { outcome, .. } => edit_outcome_failed(*outcome),
            Self::Stop | Self::Activation => false,
        };
        let text = match self {
            Self::Edit {
                outcome,
                word,
                path,
                operation_id,
                detail_ref,
            } => {
                let recovery = match outcome {
                    EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged => {
                        "Do not repeat the mutation to repair this response."
                    }
                    EditOutcome::OutcomeUnknown => {
                        "Inspect this target with native tools before any later mutation; do \
                         not replay this operation."
                    }
                    _ => "No write occurred.",
                };
                let diagnostics = detail_ref.as_ref().map_or_else(String::new, |reference| {
                    format!(" Diagnostics pending; result retained under {reference}; Bridge delivers it automatically.")
                });
                format!(
                    "edit: {word}; path {path}; operation_id {operation_id}\n\
                     Presentation degraded (presentation_failed). {recovery}{diagnostics}"
                )
            }
            Self::Stop => {
                "presentation degraded (presentation_failed); the workspace is released".to_owned()
            }
            Self::Activation => {
                "presentation degraded (presentation_failed); the workspace is active, repeat \
                 ide.start with the same activation_id for the project card"
                    .to_owned()
            }
        };
        let content = vec![ContentBlock::text(text)];
        if failed {
            CallToolResult::error(content)
        } else {
            CallToolResult::success(content)
        }
    }
}

#[cfg(test)]
mod refusal_table;

#[cfg(test)]
mod tests {

    /// Source fitting includes the complete serialized MCP carrier, before any delivery ledger
    /// can claim the body was served; a batch larger than the observed outer budget must page.
    #[test]
    fn read_reply_budget_includes_the_complete_carrier() {
        let reply = PeerReply::Complete {
            kind: ResultKind::Read,
            text: "0123456789".repeat(1200),
            detail_ref: Some("source-budget-ref".into()),
            truncated: false,
            continuation: false,
        };
        assert!(!fits(&reply, Envelope::TextOnly));
        assert!(!fits(&reply, Envelope::WithStructured));
        let small = PeerReply::Complete {
            kind: ResultKind::Read,
            text: "1\tconst answer = 42;\n".into(),
            detail_ref: Some("source-budget-ref".into()),
            truncated: false,
            continuation: false,
        };
        assert!(fits(&small, Envelope::TextOnly));
        assert!(fits_with_status(
            &small,
            "<agent-ide>checking</agent-ide>",
            Envelope::TextOnly
        ));
    }
    use super::*;
    use crate::assistance::reply::{EditDiagnostics, MissingPeer, ResultKind};
    use crate::changes::edit::{EditOutcome, EditReceiptError, EditResult};

    /// The render fuel budget keeps at least tenfold headroom above the largest real render this
    /// module exercises, and one instruction below that render already fails the render — so a
    /// template growth that outgrows the budget fails loudly here, and a starved render fails
    /// into the presentation-degraded fallback rather than looping.
    #[test]
    fn fuel_bounds_every_real_render_with_tenfold_headroom() {
        /// Returns the smallest fuel budget that renders `context` with the production engine.
        fn consumed(context: &serde_json::Value) -> u64 {
            let (mut low, mut high) = (1u64, RENDER_FUEL);
            while low < high {
                let middle = low + (high - low) / 2;
                let rendered = reply_environment(Some(middle))
                    .get_template("reply.jinja")
                    .expect("embedded template")
                    .render(minijinja::Value::from_serialize(context))
                    .is_ok();
                if rendered {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            low
        }
        // The deepest branch chains reply.jinja offers: the final error fallbacks, a tagged
        // execution-profile refusal, an edit carrying eight joined messages, and a page-sized
        // Complete text.
        let contexts = [
            serde_json::json!({
                "state": "error", "code": "internal",
                "resolution_detail": "", "resolution_message": "", "cause_tag": ""
            }),
            serde_json::json!({
                "state": "error", "code": "conflict",
                "resolution_detail": "", "resolution_message": "", "cause_tag": ""
            }),
            serde_json::json!({
                "state": "error", "code": "execution_profile", "cause_tag": "spawn:reap_timed_out",
                "resolution_detail": "", "resolution_message": ""
            }),
            serde_json::json!({
                "state": "error", "code": "provider_unavailable",
                "resolution_detail": "symbol:provider_unavailable (fixtureserver: workspace \
                 load failed; outline and read answer from source)",
                "resolution_message": "message", "cause_tag": ""
            }),
            serde_json::json!({
                "state": "edit",
                "result": {"outcome": "replaced", "path": "src/lib.rs",
                           "operation_id": "op", "source_ref": "ref"},
                "diagnostics": {"state": "current_reported",
                    "messages": ["m1\nx", "m2", "m3", "m4", "m5", "m6", "m7", "m8"],
                    "delta": "Provider reported 8 diagnostics for the exact source generation.",
                    "truncated": true},
                "note": "3 changes applied: change 1: lines 12-20 replaced",
                "operation": "renamed"
            }),
            serde_json::json!({
                "state": "complete", "kind": "context",
                "text": "\u{0}\u{1f980}\"\\".repeat(16_000),
                "detail_ref": "next-page", "truncated": true, "continuation": true
            }),
            serde_json::json!({
                "state": "unavailable", "reason": "host_binding",
                "cause_tag": "project_moved: bound to /a, asked /b",
                "resolution_message": "Bound repository: /a; requested root: /b",
                "tool_name": "ide.test", "test_status_id": 7
            }),
        ];
        let largest = contexts
            .iter()
            .map(consumed)
            .enumerate()
            .max_by_key(|&(_, cost)| cost)
            .expect("representative contexts");
        assert!(
            RENDER_FUEL >= 10 * largest.1,
            "largest real render costs {} fuel; budget is {RENDER_FUEL}",
            largest.1
        );
        let starved = reply_environment(Some(largest.1 - 1));
        assert!(
            starved
                .get_template("reply.jinja")
                .expect("embedded template")
                .render(minijinja::Value::from_serialize(&contexts[largest.0]))
                .is_err()
        );
    }

    /// Renders with no carried status plate, the projection every reply had before T28B.
    fn render(reply: PeerReply, envelope: Envelope) -> Option<CallToolResult> {
        render_with_status(reply, None, envelope)
    }

    /// Extracts the sole model-facing text block from a rendered test result.
    fn text_of(rendered: &CallToolResult) -> &str {
        assert_eq!(rendered.content.len(), 1);
        rendered.content[0].as_text().unwrap().text.as_str()
    }

    /// Builds one constructor-validated edit result for the requested closed outcome.
    fn edit_result(outcome: EditOutcome) -> Result<EditResult, EditReceiptError> {
        EditResult::new(
            "private-operation-id".into(),
            "src/lib.rs".into(),
            outcome,
            outcome
                .has_post_source()
                .then(|| "source-after-edit".into()),
        )
    }

    /// Builds a successful edit reply with a usable post-read reference and supplied diagnostics.
    fn successful_edit_reply(diagnostics: EditDiagnostics) -> PeerReply {
        PeerReply::Edit {
            result: EditResult::new(
                "private-operation-id".into(),
                "src/lib.rs".into(),
                EditOutcome::Replaced,
                Some("source-after-edit".into()),
            )
            .expect("fixed successful edit result"),
            diagnostics,
            note: None,
            operation: None,
        }
    }

    /// The operation word (`inserted`, `deleted`, `renamed`) replaces the durable outcome word,
    /// so the reply names what happened while the structured result keeps the closed outcome.
    #[test]
    fn edit_operation_word_replaces_the_outcome_word() {
        let reply = PeerReply::Edit {
            result: edit_result(EditOutcome::Replaced).unwrap(),
            diagnostics: EditDiagnostics::CurrentClean { project_errors: 0 },
            note: Some(
                "renamed c → cee; 3 sites in 2 files: src/lib.rs (2), tests/x.rs (1)".into(),
            ),
            operation: Some("renamed".into()),
        };
        let expected = serde_json::to_value(&reply).unwrap();
        let rendered = render(reply, Envelope::WithStructured).unwrap();
        let text = text_of(&rendered);
        assert!(
            text.starts_with("edit: renamed; path src/lib.rs; diagnostics: current_clean"),
            "{text}"
        );
        assert!(
            text.contains(
                "diagnostics: current_clean\n\
                 renamed c → cee; 3 sites in 2 files: src/lib.rs (2), tests/x.rs (1)"
            ),
            "{text}"
        );
        assert_eq!(rendered.structured_content, Some(expected));
    }

    /// Generic refusals obey r5 budgets through the production renderer, preserve isError and
    /// typed outcomes, and expose no internal inspect/start/diff reason labels in model text.
    #[test]
    fn generic_refusals_have_one_bounded_repair_and_preserve_outcomes() {
        for (code, detail) in [
            (FailureCode::InvalidDetail, "inspect:detail_expired"),
            (
                FailureCode::WorkspaceActivation,
                "start:worktree_unresolved:identity_read",
            ),
            (FailureCode::Capacity, "diff:too_large"),
            (FailureCode::Internal, "daemon:internal"),
            (
                FailureCode::SourceTooLarge {
                    size: u64::MAX,
                    ceiling: 4096,
                },
                "read:source_too_large",
            ),
        ] {
            let reply = PeerReply::Error {
                code,
                detail: Some(detail.to_owned()),
            };
            let mut expected = serde_json::to_value(&reply).unwrap();
            expected.as_object_mut().unwrap().remove("detail");
            let result = render(reply, Envelope::WithStructured).unwrap();
            let text = text_of(&result);
            let lines = text.lines().collect::<Vec<_>>();
            assert!(lines[0].len() <= 160, "{text}");
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.starts_with("Repair:"))
                    .count(),
                1,
                "{text}"
            );
            assert!(
                lines[1].starts_with("Repair:") && lines[1].len() <= 120,
                "{text}"
            );
            assert!(
                !text.contains(detail)
                    && !lines[1].contains(" or ")
                    && !lines[1].contains("once")
                    && !lines[0].starts_with("error:")
                    && !lines[0].starts_with("unavailable:"),
                "{text}"
            );
            assert_eq!(result.is_error, Some(true));
            assert_eq!(result.structured_content, Some(expected));
        }
        let rendered = render(
            PeerReply::Error {
                code: FailureCode::EditRefused,
                detail: Some(format!(
                    "edit:refused: Current unit:\n{}",
                    "1\tλ\n".repeat(100)
                )),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&rendered);
        let excerpt = text.splitn(3, '\n').nth(2).unwrap();
        assert!(
            excerpt.len() <= 4096 && excerpt.lines().count() <= 60,
            "{text}"
        );
        assert!(excerpt.contains("1\tλ"), "current unit was lost: {text}");
        let bounded = refusal_text("λ".repeat(300), "λ".repeat(300), None);
        let lines = bounded.lines().collect::<Vec<_>>();
        assert_eq!(lines[0].len(), 160);
        assert_eq!(lines[1].len(), 120);
    }

    /// An edit refusal retains its per-change/current-text data under one repair line.
    #[test]
    fn edit_refused_names_its_changes_and_the_same_operation_id() {
        let reply = PeerReply::Error {
            code: FailureCode::EditRefused,
            detail: Some(
                "edit:refused: 2 of 3 changes refused, nothing written — change 2: old text \
                 not found; closest line 214: \"        let total = lang::line_count(&source);\"; \
                 change 3: old text matches 2 places (lines 40, 88); add \
                 \"within\":\"src/x.rs#Owner/name\" or more surrounding lines"
                    .to_owned(),
            ),
        };
        let rendered = render(reply, Envelope::WithStructured).unwrap();
        let text = text_of(&rendered);
        assert_eq!(
            text,
            "Edit refused; nothing was written.\nRepair: Correct the named changes and repeat with the same operation_id.\n  2 of 3 changes refused, nothing written — \
             change 2: old text not found; closest line 214: \"        let total = \
             lang::line_count(&source);\"; change 3: old text matches 2 places (lines 40, 88); \
             add \"within\":\"src/x.rs#Owner/name\" or more surrounding lines\n"
        );
        // The structured copy keeps the closed code and drops the detail payload.
        let structured = rendered.structured_content.unwrap();
        assert_eq!(structured["code"], "edit_refused");
        assert!(structured.get("detail").is_none());
        assert!(fits(
            &PeerReply::Error {
                code: FailureCode::EditRefused,
                detail: Some("edit:refused: 32 of 32 changes refused, nothing written".to_owned()),
            },
            Envelope::WithStructured
        ));
    }

    /// A batch landing note rides the ordinary edit note field after the first line.
    #[test]
    fn batch_landing_note_lines_render_after_the_edit_line() {
        let reply = PeerReply::Edit {
            result: edit_result(EditOutcome::Replaced).unwrap(),
            diagnostics: EditDiagnostics::CurrentClean { project_errors: 0 },
            note: Some(
                "3 changes applied: change 1: lines 12–20 replaced (now 12–24); change 2: \
                 src/x.rs#Foo/bar inserted after #Foo (now 26–33); change 3: old text at line \
                 88 replaced (now 90–91)\nnote: src/x.rs already had a syntax error (line 4) \
                 before this edit; edit applied"
                    .into(),
            ),
            operation: None,
        };
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(
            text.contains("\n3 changes applied: change 1: lines 12–20 replaced (now 12–24)"),
            "{text}"
        );
        assert!(
            text.ends_with(
                "note: src/x.rs already had a syntax error (line 4) before this \
                            edit; edit applied"
            ),
            "{text}"
        );
    }

    /// Refuses future reply variants instead of emitting an empty success-shaped MCP page.
    #[test]
    fn unknown_template_branches_fail_closed() {
        for context in [
            serde_json::json!({"state": "future"}),
            serde_json::json!({
                "state": "edit",
                "result": {"outcome": "future", "path": "src/lib.rs"},
                "diagnostics": {"state": "unknown"}
            }),
            serde_json::json!({
                "state": "edit",
                "result": {"outcome": "replaced", "path": "src/lib.rs", "source_ref": "ref"},
                "diagnostics": {"state": "future"}
            }),
        ] {
            assert!(render_text(&context).is_none());
        }
    }

    /// Covers every non-edit state and result kind with one compact projection; refusals have
    /// a bounded natural head and repair while the structured oracle retains its closed tags.
    /// Feedback and complete bodies arrive byte-exactly rather than being replaced by an ack.
    #[test]
    fn every_reply_state_has_one_compact_text_block_and_equal_structured_content() {
        let replies = vec![
            PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: None,
            },
            PeerReply::HookObserved {},
            PeerReply::HookSettled {},
            PeerReply::NativeHookObserved {},
            PeerReply::Feedback {
                text: "bounded fact".into(),
            },
            PeerReply::HostStopped {},
            PeerReply::Pending {
                detail_ref: "detail-queued".into(),
            },
            PeerReply::Error {
                code: FailureCode::Capacity,
                detail: None,
            },
            PeerReply::InvalidParameters {
                message: "invalid bounded parameters: unsupported path".into(),
            },
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "active".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "context evidence".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: "diff evidence".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text: "stopped".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
        ];
        for reply in replies {
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply, Envelope::WithStructured)
                .unwrap_or_else(|| panic!("missing projection for {expected:?}"));
            let text = text_of(&rendered);
            assert!(!text.starts_with('{') && !text.contains("\"state\""));
            if matches!(expected["state"].as_str(), Some("feedback" | "complete")) {
                assert_eq!(Some(text), expected["text"].as_str(), "{expected:?}");
            }
            if matches!(
                expected["state"].as_str(),
                Some("unavailable" | "error" | "invalid_parameters")
            ) {
                let lines = text.lines().collect::<Vec<_>>();
                assert!(lines[0].len() <= 160, "{text}");
                assert!(
                    lines[1].starts_with("Repair:") && lines[1].len() <= 120,
                    "{text}"
                );
                assert_eq!(
                    lines
                        .iter()
                        .filter(|line| line.starts_with("Repair:"))
                        .count(),
                    1
                );
                assert!(
                    !text.starts_with("error:") && !text.starts_with("unavailable:"),
                    "{text}"
                );
            }
            assert_eq!(rendered.structured_content, Some(expected.clone()));
            assert_eq!(
                rendered.is_error,
                matches!(
                    expected["state"].as_str(),
                    Some("error" | "invalid_parameters")
                )
                .then_some(true)
            );
        }
    }

    /// An inactive test status names the preserved run and inspect route, while other tools retain
    /// the generic activation recovery wording.
    #[test]
    fn inactive_test_status_names_inspect_run() {
        let reply = PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause: Some(crate::assistance::reply::HostBindingCause::InactiveBinding),
        };
        let rendered = render_with_call(
            reply,
            None,
            Envelope::WithStructured,
            Some("ide.test"),
            Some(7),
        )
        .unwrap();
        assert_eq!(
            text_of(&rendered),
            "This IDE activation has stopped.\nRepair: Call ide.start {\"read_only\":true}, then ide.test {\"result\":7}."
        );
        let other = render_with_call(
            PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: Some(crate::assistance::reply::HostBindingCause::InactiveBinding),
            },
            None,
            Envelope::WithStructured,
            Some("ide.context"),
            Some(7),
        )
        .unwrap();
        assert_eq!(
            text_of(&other),
            "This IDE session is not active.\nRepair: Call ide.start in this project."
        );
    }

    /// A session displaced by a stale-writer takeover is told why and how to start again, in the
    /// exact start-index-state r2 §4c words.
    #[test]
    fn taken_over_names_the_reason_and_the_read_only_restart() {
        use crate::assistance::host_binding::TakeoverReason;
        for (reason, why) in [
            (
                TakeoverReason::SessionEnded,
                "after this session's IDE connection closed",
            ),
            (
                TakeoverReason::Inactive { minutes: 31 },
                "after 31 min without activity",
            ),
        ] {
            let cause = crate::assistance::reply::HostBindingCause::from_binding(
                crate::assistance::host_binding::BindingUnavailable::TakenOver(reason),
            );
            let rendered = render(
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause,
                },
                Envelope::TextOnly,
            )
            .unwrap();
            assert_eq!(
                text_of(&rendered),
                format!(
                    "Another session took over this activation.\nRepair: Call ide.start with {{\"read_only\":true}} while that session writes.\n  {why}\n"
                )
            );
        }
    }

    /// Every host-binding cause names its closed tag in compact text without changing the public
    /// structured copy; a cause-less refusal keeps its historical text (T15B).
    #[test]
    fn host_binding_causes_name_their_tag_in_compact_text_only() {
        use crate::assistance::reply::HostBindingCause;
        let causes = vec![
            HostBindingCause::OutsideAllowedRoots,
            HostBindingCause::HooksNotDelivered,
            HostBindingCause::InvalidMetadata,
            HostBindingCause::MissingField,
            HostBindingCause::InvalidField,
            HostBindingCause::InvalidAttachment,
            HostBindingCause::UnsupportedHookPhase,
            HostBindingCause::MissingPre,
            HostBindingCause::Replay,
            HostBindingCause::Mismatch,
            HostBindingCause::MissingInvocation,
            HostBindingCause::InactiveBinding,
            HostBindingCause::NeverActivated,
            HostBindingCause::RecoveryNeeded,
            HostBindingCause::CapacityExceeded,
            HostBindingCause::HostUnrecognized,
            HostBindingCause::project_moved(
                std::path::Path::new("/private/tmp/ai-r-move"),
                "/Users/pluto/projects/agent-worktree",
            ),
            HostBindingCause::other_repository(
                std::path::Path::new("/private/tmp/ai-r-move"),
                "/Users/pluto/projects/other-repository",
            ),
        ];
        for cause in causes {
            let tag = cause.cause_tag();
            let expected = match tag.as_str() {
                "outside_allowed_roots" => {
                    "This project is outside the directories the IDE may open.\nRepair: Call ide.start with an allowed root."
                }
                "hooks_not_delivered" => {
                    "The IDE received no host event for this session.\nRepair: Call ide.start in this project."
                }
                "invalid_metadata" => {
                    "The host sent malformed call metadata.\nRepair: Send a new tool call."
                }
                "missing_field" => {
                    "Required host call metadata is missing.\nRepair: Send a new tool call."
                }
                "invalid_field" => {
                    "Host call metadata contains an invalid field.\nRepair: Send a new tool call."
                }
                "invalid_attachment" => {
                    "The host attachment does not identify this call.\nRepair: Repeat the call as a new tool invocation."
                }
                "unsupported_hook_phase" => {
                    "The host reported an unsupported tool-call phase.\nRepair: Repeat the call as a new tool invocation."
                }
                "missing_pre" => {
                    "The host's before-tool event did not reach the IDE.\nRepair: Repeat the call."
                }
                "replay" => "This exact call was already processed.\nRepair: Send a new tool call.",
                "mismatch" => {
                    "The host event did not match this call.\nRepair: Repeat the call as a new tool invocation."
                }
                "missing_invocation" => {
                    "The host's completion event arrived before call validation.\nRepair: Send a new tool call."
                }
                "inactive_binding" => {
                    "This IDE session is not active.\nRepair: Call ide.start in this project."
                }
                "never_activated" => {
                    "This session has no IDE activation yet.\nRepair: Call ide.start in this project."
                }
                "recovery_needed" => {
                    "The IDE restarted; this activation has not been restored.\nRepair: Call ide.start in this project."
                }
                "capacity_exceeded" => {
                    "The IDE's bounded session table is full.\nRepair: Continue this work with native tools."
                }
                "host_unrecognized" => {
                    "This host did not identify the call in a supported format.\nRepair: Continue this work with native tools."
                }
                other if other.starts_with("other_repository:") => {
                    "Other agents still use the bound repository; nothing moved.\nRepair: Call ide.start under the bound repository.\n  Bound repository: /private/tmp/ai-r-move; requested root: /Users/pluto/projects/other-repository\n"
                }
                moved if moved.starts_with("project_moved:") => {
                    "The IDE could not move to the requested root.\nRepair: Call ide.start under this session's current root.\n  Bound repository: /private/tmp/ai-r-move; requested root: /Users/pluto/projects/agent-worktree\n"
                }
                _ => panic!("unrecognized host-binding cause: {tag}"),
            };
            let rendered = render(
                PeerReply::Unavailable {
                    reason: MissingPeer::HostBinding,
                    cause: Some(cause),
                },
                Envelope::WithStructured,
            )
            .unwrap();
            assert_eq!(text_of(&rendered), expected);
            assert_eq!(
                rendered.structured_content,
                Some(serde_json::json!({"state":"unavailable","reason":"host_binding"}))
            );
            assert_ne!(rendered.is_error, Some(true));
        }
        let bare = render(
            PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: None,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&bare),
            "The host could not bind this IDE call.\nRepair: Repeat the call as a new tool invocation."
        );
    }

    /// Changed-source stages ask the caller to refresh; diff stages ask for new Git evidence.
    #[test]
    fn source_unavailable_names_the_recovery_for_its_stage() {
        for (detail, expected) in [
            (
                "diff:unsupported_entry",
                "Git evidence for this comparison became unusable.\nRepair: Call ide.diff again.",
            ),
            (
                "inspect:source_changed",
                "The source changed since this result was captured.\nRepair: Read the requested unit again with ide.read.",
            ),
            (
                "inspect:source_changed:\"src/main.rs\"",
                "The source changed since this result was captured.\nRepair: Read the requested unit again with ide.read.\n  \"src/main.rs\"\n",
            ),
            (
                "inspect:source_changed:\"src/main.rs\"",
                "The source changed since this result was captured.\nRepair: Read the requested unit again with ide.read.\n  \"src/main.rs\"\n",
            ),
            (
                "context:source_changed",
                "The source changed since this result was captured.\nRepair: Read the requested unit again with ide.read.",
            ),
            (
                "context:source_changed:\"src/main.rs\"",
                "The source changed since this result was captured.\nRepair: Read the requested unit again with ide.read.\n  \"src/main.rs\"\n",
            ),
            (
                "context:observation_failed:\"src/main.rs\"",
                "The requested source could not be read safely.\nRepair: Read it with a native tool.\n  \"src/main.rs\"\n",
            ),
            (
                "read:line_range:file has 249 lines; requested 286-334",
                "The requested range is unavailable.\nRepair: Read a valid range with ide.read.\n  file has 249 lines; requested 286-334\n",
            ),
            (
                "read:not_text:assets/logo.png",
                "This file is not readable text.\nRepair: Inspect it with a native file tool.\n  assets/logo.png\n",
            ),
            (
                "read:source_unavailable:src/main.rs",
                "The requested source could not be read safely.\nRepair: Read it with a native tool.\n  src/main.rs\n",
            ),
            (
                "diff:activation_commit_unknown; use mode: head",
                "The activation's start commit is unknown; the task diff is unavailable.\nRepair: Call ide.diff with {\"mode\":\"head\"}.",
            ),
        ] {
            let rendered = render(
                PeerReply::Error {
                    code: FailureCode::SourceUnavailable,
                    detail: Some(detail.to_owned()),
                },
                Envelope::WithStructured,
            )
            .unwrap();
            assert_eq!(text_of(&rendered), expected);
        }
        let bare = render(
            PeerReply::Error {
                code: FailureCode::SourceUnavailable,
                detail: None,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&bare),
            "The requested source could not be read safely.\nRepair: Read it with a native tool."
        );
    }

    /// A missing `ide.outline`/`ide.read` path names the exact bounded requested path in the
    /// reason itself, ahead of the payload-free stage tag (T163, W6).
    #[test]
    fn no_such_file_names_the_requested_path() {
        let rendered = render(
            PeerReply::Error {
                code: FailureCode::NoSuchFile("src/assistance/host_bindng.rs".to_owned()),
                detail: Some("outline:no_such_file".to_owned()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&rendered),
            "This path does not exist in the worktree.\nRepair: Correct the path and repeat this call.\n  src/assistance/host_bindng.rs\n"
        );
        let for_read = render(
            PeerReply::Error {
                code: FailureCode::NoSuchFile("src/missing.rs".to_owned()),
                detail: Some("read:no_such_file".to_owned()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&for_read),
            "This path does not exist in the worktree.\nRepair: Correct the path and repeat this call.\n  src/missing.rs\n"
        );
    }

    /// A file no language reads is named as the caller's wrong tool, not a server outage.
    #[test]
    fn unsupported_file_names_the_path_and_the_read_that_works() {
        let rendered = render(
            PeerReply::Error {
                code: FailureCode::UnsupportedFile("Cargo.toml".to_owned()),
                detail: Some("outline:unsupported_file".to_owned()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&rendered),
            "No registered language outlines this file type.\nRepair: Read the file with a native tool.\n  Cargo.toml\n"
        );
        assert_eq!(
            rendered.structured_content.unwrap()["code"]["unsupported_file"],
            "Cargo.toml"
        );
    }

    /// An expired or unknown detail says to repeat the original call.
    #[test]
    fn expired_detail_explains_the_recovery() {
        let expired = render(
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
                detail: None,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&expired);
        assert!(
            text.starts_with("This result reference was never issued.\nRepair: Repeat the original IDE call to get a fresh result."),
            "{text}"
        );
        assert!(text.contains("Repeat the original IDE call"), "{text}");
        assert_eq!(expired.is_error, Some(true));
    }

    /// A never-issued, expired, or parameter-mismatched detail states the correct recovery.
    #[test]
    fn invalid_detail_names_unknown_expired_and_mismatched_separately() {
        for (detail, expected) in [
            (
                "inspect:detail_unknown",
                "This result reference was never issued.\nRepair: Repeat the original IDE call to get a fresh result.",
            ),
            (
                "inspect:detail_expired",
                "This result reference has expired.\nRepair: Repeat the original IDE call to get a fresh result.",
            ),
            (
                "inspect:detail_mismatch",
                "This result reference belongs to different arguments.\nRepair: Repeat the original call with its exact arguments.",
            ),
        ] {
            let rendered = render(
                PeerReply::Error {
                    code: FailureCode::InvalidDetail,
                    detail: Some(detail.to_owned()),
                },
                Envelope::TextOnly,
            )
            .unwrap();
            assert_eq!(text_of(&rendered), expected);
        }
    }

    /// A repeated activation under one id but another root names its own fix.
    #[test]
    fn activation_conflict_names_its_own_recovery() {
        let conflict = render(
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
                detail: Some("start:activation_conflict".to_owned()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert_eq!(
            text_of(&conflict),
            "This activation_id was already used with another root.\nRepair: Call ide.start with a new activation_id."
        );
    }

    /// A kept same-actor activation names the held worktree and what keeps it in the reply.
    #[test]
    fn kept_activation_refusal_names_worktree_and_reason() {
        let rendered = render(
            PeerReply::Error {
                code: FailureCode::Conflict,
                detail: Some(
                    "start:actor_owns_another_worktree: actor a (activation x, writer, since t, last activity u); worktree /w/old; kept: a pending edit"
                        .to_owned(),
                ),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(
            text.contains("worktree /w/old; kept: a pending edit"),
            "{text}"
        );
        assert!(text.contains("not handed over automatically"), "{text}");
        assert!(text.contains("Call ide.stop there"), "{text}");
    }

    /// Start stages keep distinct causes and recovery instructions in the compact reply.
    #[test]
    fn start_refusals_name_holder_and_failure_stage() {
        for (detail, expected) in [
            (
                "start:worktree_held_by_another_actor",
                "Another agent session owns this worktree's IDE activation",
            ),
            (
                "start:worktree_held_by_this_actor",
                "Another session for this actor owns this worktree",
            ),
            (
                "start:actor_owns_another_worktree",
                "This actor's IDE activation is attached to another worktree",
            ),
            (
                "start:provider_cache_namespace_conflict",
                "Another active session owns the language-server cache",
            ),
            (
                "start:git_discovery_failed: not a Git worktree",
                "Git worktree discovery failed",
            ),
            (
                "start:worktree_unresolved: could not be resolved",
                "The worktree could not be resolved",
            ),
            (
                "start:durable_state: activation state failed",
                "Saved workspace state could not be read",
            ),
        ] {
            let rendered = render(
                PeerReply::Error {
                    code: if detail.starts_with("start:provider_cache")
                        || detail.starts_with("start:worktree_held")
                        || detail.starts_with("start:actor_owns")
                    {
                        FailureCode::Conflict
                    } else {
                        FailureCode::WorkspaceActivation
                    },
                    detail: Some(detail.to_owned()),
                },
                Envelope::TextOnly,
            )
            .unwrap();
            assert!(
                text_of(&rendered).contains(expected),
                "{detail}: {rendered:?}"
            );
        }
        let absent = render(
            PeerReply::Error {
                code: FailureCode::OutsideAllowedRoots,
                detail: Some(
                    "start:root_absent; nearest existing ancestor below an allowed root: /repo"
                        .to_owned(),
                ),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert!(text_of(&absent).contains("requested root does not exist yet"));
        assert!(text_of(&absent).contains("/repo"));

        let held = render(
            PeerReply::Error {
                code: FailureCode::Conflict,
                detail: Some("start:worktree_held_by_another_actor: actor agent-a (activation task-7, writer, since 2026-10-04T10:00:00Z, last activity 2026-10-04T10:01:00Z)".to_owned()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let held = text_of(&held);
        for fact in [
            "agent-a",
            "task-7",
            "since 2026-10-04T10:00:00Z",
            "last activity 2026-10-04T10:01:00Z",
            "Call ide.start with {\"read_only\":true} to read alongside",
            "The slot frees when that session ends or after 30 min without activity",
        ] {
            assert!(held.contains(fact), "missing {fact:?}: {held}");
        }

        let unresolved = render(
            PeerReply::Error {
                code: FailureCode::WorkspaceActivation,
                detail: Some("start:worktree_unresolved:identity_read".to_owned()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert!(text_of(&unresolved).contains("Check the path and retry ide.start from its root"));
    }

    /// An environment selector refused outside `allowed_roots` names the selector and the project
    /// venv form instead of telling the agent to move the project root (F-28), while an ordinary
    /// root refusal keeps its own text.
    #[test]
    fn environment_selector_refusal_names_the_selector_and_the_venv_form() {
        // The worker JSON-quotes the root key, so roots and selectors with spaces stay exact.
        for (key, selector) in [
            ("alpha", "/usr/local/bin/interpreter-1"),
            ("alpha:packages/my project", "/opt/my envs/interpreter 1"),
            ("alpha:odd\"root", "/usr/bin/interpreter"),
        ] {
            let refused = render(
                PeerReply::Error {
                    code: FailureCode::OutsideAllowedRoots,
                    detail: Some(format!(
                        "environment {} {selector}",
                        serde_json::Value::String(key.to_owned())
                    )),
                },
                Envelope::TextOnly,
            )
            .unwrap();
            let line = text_of(&refused);
            let example = format!(
                "{{{}: \".venv\"}}",
                serde_json::Value::String(key.to_owned())
            );
            for fact in [
                &format!("Selector: {selector}; project key: {key}"),
                "inside the worktree",
                "not a base interpreter",
                &format!("environment {example}"),
                "allowed roots",
            ] {
                assert!(line.contains(fact), "missing {fact:?}: {line}");
            }
            assert!(!line.contains("requested project root"), "{line}");
        }

        let ordinary = render(
            PeerReply::Error {
                code: FailureCode::OutsideAllowedRoots,
                detail: None,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert!(text_of(&ordinary).contains("The requested root is outside"));
    }

    /// Edit and test refusals share the exact read-only response shape and current-writer field.
    #[test]
    fn read_only_refusals_are_uniform_for_mutating_tools() {
        for tool in ["ide.edit", "ide.test"] {
            let rendered = render(
                PeerReply::Error {
                    code: FailureCode::Conflict,
                    detail: Some(format!(
                        "read_only:{tool}:activation task-7, since 2026-10-04T10:00:00Z, last activity 2026-10-04T10:01:00Z"
                    )),
                },
                Envelope::TextOnly,
            )
            .unwrap();
            let line = text_of(&rendered);
            assert!(
                line.starts_with("This activation started with read_only: true;"),
                "{line}"
            );
            assert!(line.contains("started with read_only: true"), "{line}");
            assert!(line.contains("Call ide.start without read_only"), "{line}");
            assert!(line.contains("one writer per worktree"), "{line}");
            assert!(
                line.contains("Current writer: activation task-7, since"),
                "{line}"
            );
        }
        let none = render(
            PeerReply::Error {
                code: FailureCode::Conflict,
                detail: Some("read_only:ide.edit:none".to_owned()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert!(text_of(&none).ends_with("Current writer: none\n"));
    }

    /// Capacity failures identify the capture, queue, result-store or protected actor resource
    /// and give the corresponding recovery, including a path-specific read for a lone long line.
    #[test]
    fn capacity_error_names_its_stage_and_recovery() {
        let large_diff = render(
            PeerReply::Error {
                code: FailureCode::Capacity,
                detail: Some("diff:too_large".to_owned()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert_eq!(
            text_of(&large_diff),
            "This diff exceeds a bounded capture or reply envelope limit.\nRepair: Call ide.diff with narrower paths."
        );
        let staged = render(
            PeerReply::Error {
                code: FailureCode::Capacity,
                detail: Some("inspect:detail_unknown".to_owned()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert_eq!(
            text_of(&staged),
            "The IDE's bounded queue or result store is full.\nRepair: Continue this operation with native tools."
        );
        for (stage, recovery) in [
            ("worker:queue_full", "request queue"),
            ("inspect:queue_full", "request queue"),
            ("worker:result_store_full", "result store"),
            ("worker:actor_share_full", "other active actors"),
            (
                "diff:single_line:src/long.rs source line 1; ide.read",
                "reply envelope",
            ),
        ] {
            let reply = render(
                PeerReply::Error {
                    code: FailureCode::Capacity,
                    detail: Some(stage.to_owned()),
                },
                Envelope::WithStructured,
            )
            .unwrap();
            let text = text_of(&reply);
            assert!(text.contains(recovery), "{text}");
            // One concrete Repair action: exactly one line, within budget, never a two-step.
            let lines = text.lines().collect::<Vec<_>>();
            assert!(lines[0].len() <= 160 && lines[1].len() <= 120, "{text}");
            assert_eq!(
                lines.iter().filter(|l| l.starts_with("Repair: ")).count(),
                1,
                "{text}"
            );
            assert!(!text.contains("; otherwise"), "{text}");
        }
        let bare = render(
            PeerReply::Error {
                code: FailureCode::Capacity,
                detail: None,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert_eq!(
            text_of(&bare),
            "The IDE's bounded queue or result store is full.\nRepair: Continue this operation with native tools."
        );
    }

    /// Split error stages preserve the cause and give the matching recovery for each refusal.
    #[test]
    fn split_error_causes_render_distinct_next_steps() {
        for (code, detail, expected) in [
            (
                FailureCode::UnknownSymbol,
                "symbol:region: src/lib.rs:3 is file-level code at src/lib.rs#@imports, not a symbol; read it with ide.read",
                "No symbol matches the requested address.\nRepair: List the file's addresses with ide.outline.\n  src/lib.rs:3 is file-level code at src/lib.rs#@imports, not a symbol; read it with ide.read\n",
            ),
            (
                FailureCode::UnknownSymbol,
                "symbol:line_past_end: src/lib.rs:99 is past the end of the file",
                "No symbol matches the requested address.\nRepair: List the file's addresses with ide.outline.\n  src/lib.rs:99 is past the end of the file\n",
            ),
            (
                FailureCode::UnknownSymbol,
                "edit:range_past_end: line 17 is past the end of src/a.rs (3 lines)",
                "No symbol matches the requested address.\nRepair: List the file's addresses with ide.outline.\n  line 17 is past the end of src/a.rs (3 lines)\n",
            ),
            (
                FailureCode::ProviderUnavailable,
                "outline:provider_unavailable ext=md",
                "No language server is configured for .md files in this project.\nRepair: Continue this operation with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "symbol:provider_unavailable ext=md (provider: no server for this file type)",
                "No language server is configured for .md files in this project.\nRepair: Continue this operation with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "edit:rename_no_edits:foo",
                "The language server returned no edits for this rename.\nRepair: Check the requested symbol with ide.symbol.\n  foo\n",
            ),
            (
                FailureCode::ProviderUnavailable,
                "edit:rename_request_failed:foo",
                "The language server failed to process this rename.\nRepair: Check the requested symbol with ide.symbol.\n  foo\n",
            ),
            (
                FailureCode::ProviderUnavailable,
                "edit:rename_unsupported_edits:foo",
                "The language server returned rename edits the IDE cannot safely apply.\nRepair: Use native tools for this rename.\n  foo\n",
            ),
            (
                FailureCode::ProviderUnavailable,
                "test:selection_unavailable",
                "Tests could not be selected from these arguments.\nRepair: Call ide.test with a pattern.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "read:provider_unavailable ext=gamma",
                "No language server is configured for .gamma files in this project.\nRepair: Continue this operation with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "symbol:provider_unavailable ext=delta",
                "No language server is configured for .delta files in this project.\nRepair: Continue this operation with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "context:provider_unavailable ext=epsilon",
                "No language server is configured for .epsilon files in this project.\nRepair: Continue this operation with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "symbol:provider_unavailable (fixtureserver: workspace load failed; outline and read answer from source)",
                "The server cannot answer semantic requests; source outline and read remain available.\nRepair: Read the definition with ide.read.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "read:provider_unavailable (fixtureserver: transport gone; outline and read answer from source)",
                "The server cannot answer semantic requests; source outline and read remain available.\nRepair: Read the definition with ide.read.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "symbol:provider_unavailable (fixtureserver: references request failed; outline and read answer from source)",
                "The server cannot answer semantic requests; source outline and read remain available.\nRepair: Read the definition with ide.read.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "outline:provider_unavailable (fixtureserver: documentSymbols request failed; use native reads)",
                "The server cannot answer this request and no source outline is available.\nRepair: Read the source with native tools.",
            ),
            (
                FailureCode::ProviderUnavailable,
                "symbol:provider_unavailable (fixtureserver-plain: workspace load failed; use native reads)",
                "The server cannot answer this request and no source outline is available.\nRepair: Read the source with native tools.",
            ),
            (
                FailureCode::InvalidDetail,
                "test:unknown_run:42",
                "This test run is unknown or expired.\nRepair: Start a new run with ide.test.",
            ),
            (
                FailureCode::Capacity,
                "stop:busy",
                "This stop is not recorded; its grant remains held while the IDE retries.\nRepair: Continue this work with native tools.",
            ),
            (
                FailureCode::Capacity,
                "stop:store_full",
                "This stop is not recorded; its grant remains held while the IDE retries.\nRepair: Continue this work with native tools.\n  The full state store clears only when every IDE session of this repository ends and the IDE restarts.\n",
            ),
            (
                FailureCode::Deadline,
                "stop:store_deadline",
                "Recording this stop took too long and may still complete; the IDE retries with the same operation id.\nRepair: Continue this work with native tools.",
            ),
            (
                FailureCode::WorkspaceAuthority,
                "stop:store_unavailable",
                "This stop is not recorded; its grant remains held while the IDE retries.\nRepair: Continue this work with native tools.",
            ),
            (
                FailureCode::WorkspaceAuthority,
                "stop:authority",
                "This stop conflicts with the recorded activation.\nRepair: Call ide.start to see the current binding.",
            ),
            (
                FailureCode::Deadline,
                "stop:deadline",
                "Stopping exceeded its time limit; cleanup may still be running.\nRepair: Call ide.start to check the session before editing.",
            ),
        ] {
            let rendered = render(
                PeerReply::Error {
                    code,
                    detail: Some(detail.to_owned()),
                },
                Envelope::TextOnly,
            )
            .unwrap();
            assert_eq!(text_of(&rendered), expected);
        }
    }

    /// A closed profile cause appears after the stable code; capture suffixes and unknown tags
    /// cannot be admitted into agent-facing text.
    #[test]
    fn execution_profile_cause_is_closed_and_actionable() {
        use crate::assistance::reply::ExecutionProfileCause;
        for (tag, message) in [
            (
                "git_policy",
                "the IDE could not establish the allowed Git commands for this call",
            ),
            (
                "query_policy",
                "the Git query was refused by the execution policy",
            ),
            (
                "git_unsupported",
                "the configured Git does not support a required operation",
            ),
            ("spawn:io", "the IDE could not start the required process"),
            ("spawn:request", "the process request was invalid"),
            (
                "spawn:protocol_stdout_reserved",
                "the process tried to write data reserved for the IDE's reply",
            ),
            (
                "spawn:reap_timed_out",
                "the failed process did not stop before its deadline",
            ),
        ] {
            let cause = ExecutionProfileCause::from_log_tag(tag).expect("known cause");
            let rendered = render(
                PeerReply::Error {
                    code: FailureCode::ExecutionProfileCause(cause),
                    detail: None,
                },
                Envelope::WithStructured,
            )
            .unwrap();
            assert_eq!(
                text_of(&rendered),
                format!(
                    "{}.\nRepair: Continue with native tools.",
                    message[..1].to_uppercase() + &message[1..]
                )
            );
            assert_eq!(
                rendered.structured_content.unwrap()["code"],
                "execution_profile"
            );
        }
        assert!(ExecutionProfileCause::from_log_tag("spawn:io:/private/path").is_none());
    }

    /// Keeps unverified project configuration actionable without claiming a native substitute.
    #[test]
    fn resolution_unverified_is_closed_without_native_path_overclaim() {
        let reply = PeerReply::Error {
            code: FailureCode::ResolutionUnverified,
            detail: Some(
                "tsconfig.json could not prove membership for src/outside.ts; add it to files or include".into(),
            ),
        };
        let mut expected = serde_json::to_value(&reply).unwrap();
        expected.as_object_mut().unwrap().remove("detail");
        let rendered = render(reply, Envelope::WithStructured).unwrap();
        let text = text_of(&rendered);
        assert!(call_tool_result_fits(&rendered), "{text}");
        assert!(
            text.contains("could not verify project membership")
                && text.contains("Repair: Read the project problems")
        );
        assert!(text.contains("tsconfig.json") && text.contains("src/outside.ts"));
        assert!(!text.contains("native"));
        assert_eq!(rendered.structured_content, Some(expected));
        assert_eq!(rendered.is_error, Some(true));
    }

    /// Covers every closed edit outcome without exposing irrelevant operation correlation.
    #[test]
    fn every_edit_outcome_is_compact_private_and_safe() {
        for outcome in [
            EditOutcome::Created,
            EditOutcome::Replaced,
            EditOutcome::Unchanged,
            EditOutcome::StaleSource,
            EditOutcome::ConflictingDuplicate,
            EditOutcome::UnsafeTarget,
            EditOutcome::CancelledNoEffect,
            EditOutcome::DeadlineNoEffect,
            EditOutcome::CapacityNoEffect,
            EditOutcome::OutcomeUnknown,
            EditOutcome::UnavailableBeforeDispatch,
        ] {
            let reply = PeerReply::Edit {
                result: edit_result(outcome).unwrap(),
                diagnostics: EditDiagnostics::Unknown {},
                note: None,

                operation: None,
            };
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply, Envelope::WithStructured).unwrap();
            let text = text_of(&rendered);
            let refused = !matches!(
                outcome,
                EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged
            );
            if refused {
                // harness-reply-v1 R5: a head, one Repair line, no internal outcome label.
                let lines = text.lines().collect::<Vec<_>>();
                assert!(lines.len() == 2 && lines[0].len() <= 160, "{text}");
                assert!(
                    lines[1].starts_with("Repair: ") && lines[1].len() <= 120,
                    "{text}"
                );
                assert!(!text.contains(outcome.as_str()), "{text}");
                assert!(
                    !lines[1].contains(" or ") && !text.contains("otherwise"),
                    "{text}"
                );
            } else {
                assert!(text.starts_with(&format!("edit: {}", outcome.as_str())));
            }
            assert!(text.contains("src/lib.rs"));
            assert!(!text.contains("private-operation-id"));
            if !matches!(
                outcome,
                EditOutcome::ConflictingDuplicate
                    | EditOutcome::CancelledNoEffect
                    | EditOutcome::DeadlineNoEffect
                    | EditOutcome::CapacityNoEffect
            ) {
                assert!(!text.contains("operation_id"));
            }
            assert_eq!(rendered.structured_content, Some(expected));
            // Only a file that was created, replaced or found unchanged is a success; every
            // refusal, cancellation and unknown outcome is an MCP error.
            let written = matches!(
                outcome,
                EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged
            );
            assert_eq!(rendered.is_error, (!written).then_some(true), "{outcome:?}");
            if outcome == EditOutcome::OutcomeUnknown {
                assert!(text.contains("do not repeat this edit") && text.contains("native tools"));
            }
            if refused && outcome != EditOutcome::OutcomeUnknown {
                assert!(text.starts_with("Nothing was written; "), "{text}");
            }
        }
    }

    /// A valid 203-byte path never costs a durable edit refusal its effect sentence: the effect
    /// leads the head, the path label is shortened to its tail and the head stays within 160 B.
    #[test]
    fn long_paths_keep_the_effect_of_every_durable_edit_refusal() {
        let path = format!("{}/{}.rs", "a".repeat(100), "b".repeat(99));
        assert_eq!(path.len(), 203);
        for outcome in [
            EditOutcome::StaleSource,
            EditOutcome::ConflictingDuplicate,
            EditOutcome::UnsafeTarget,
            EditOutcome::CancelledNoEffect,
            EditOutcome::DeadlineNoEffect,
            EditOutcome::CapacityNoEffect,
            EditOutcome::OutcomeUnknown,
            EditOutcome::UnavailableBeforeDispatch,
        ] {
            let reply = PeerReply::Edit {
                result: EditResult::new("op".into(), path.clone(), outcome, None).unwrap(),
                diagnostics: EditDiagnostics::Unknown {},
                note: None,
                operation: None,
            };
            let rendered = render(reply, Envelope::TextOnly).unwrap();
            let text = text_of(&rendered);
            let lines = text.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 2, "{text}");
            assert!(lines[0].len() <= 160, "{text}");
            assert!(
                lines[1].starts_with("Repair: ") && lines[1].len() <= 120,
                "{text}"
            );
            let effect = if outcome == EditOutcome::OutcomeUnknown {
                "may or may not have been written"
            } else {
                "Nothing was written; "
            };
            assert!(text.contains(effect), "{text}");
            assert!(lines[0].ends_with(&path[path.len() - 50..]), "{text}");
            assert!(!lines[0].contains(&path), "{text}");
        }
        assert_eq!(path_label("src/lib.rs".into()), "src/lib.rs");
        assert_eq!(path_label(path).len(), PATH_LABEL_BYTES);
        assert_eq!(path_label("é".repeat(40)).chars().count(), 29);
    }

    /// Renders same-response diagnostics into one non-JSON block with the matching typed result.
    #[test]
    fn edit_diagnostics_render_the_exact_safe_next_action() {
        let cases = [
            (
                EditDiagnostics::CurrentReported {
                    messages: vec!["cannot find value `missing`".into()],
                    delta: "Provider reported 1 diagnostic for the exact source generation.".into(),
                    truncated: false,
                },
                "current_reported",
                // The reported problems follow the first line; no file-version reference.
                "cannot find value `missing`",
            ),
            (
                EditDiagnostics::CurrentClean { project_errors: 0 },
                "current_clean",
                "current_clean",
            ),
            (EditDiagnostics::Unknown {}, "unknown", "ide.inspect"),
            (
                EditDiagnostics::Pending {
                    detail_ref: "diagnostic-detail".into(),
                },
                "pending",
                // OUTBOX-r4 §8.7: the check stays retained and Bridge delivery is conditional.
                "result retained under diagnostic-detail; Bridge delivers it automatically",
            ),
        ];
        for (diagnostics, state, next_tool) in cases {
            let reply = successful_edit_reply(diagnostics);
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply, Envelope::WithStructured).unwrap();
            let text = text_of(&rendered);
            assert_eq!(rendered.content.len(), 1);
            assert!(text.contains(state) && text.contains(next_tool));
            assert!(!text.starts_with('{') && !text.contains("\"state\""));
            assert_eq!(rendered.structured_content, Some(expected));
        }
    }

    /// A file the project check never analysed says so with the language's reason, which carries
    /// its own next step (a gated file must not be told to declare itself), never
    /// `current_clean`.
    #[test]
    fn not_analysed_diagnostics_render_the_reason_and_next_step() {
        let reply = successful_edit_reply(EditDiagnostics::NotAnalysed {
            reason: "the check did not compile this file; enable its feature".into(),
        });
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(
            text.ends_with(
                "diagnostics: not_analysed (the check did not compile this file; enable its feature)"
            ) && !text.contains("declare it")
                && !text.contains("current_clean"),
            "{text}"
        );
    }

    /// A clean file in a project whose check reports errors elsewhere says so and points at the
    /// problems instead of the diff, so it never reads as a passing build; without such errors
    /// the reply is unchanged.
    #[test]
    fn clean_file_names_project_errors_elsewhere() {
        let failing = successful_edit_reply(EditDiagnostics::CurrentClean { project_errors: 7 });
        let rendered = render(failing, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(
            text.ends_with(
                "diagnostics: current_clean for this file; the project check reports 7 errors in \
                 other files (ide.inspect {\"kind\":\"problems\"})"
            ),
            "{text}"
        );
        let passing = successful_edit_reply(EditDiagnostics::CurrentClean { project_errors: 0 });
        let rendered = render(passing, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(text.ends_with("diagnostics: current_clean"), "{text}");
    }

    /// A stale write names the file changed during application, confirms no write occurred,
    /// and directs the caller to repeat the call without teaching file-version references.
    #[test]
    fn stale_edit_text_explains_why_no_write_occurred() {
        let reply = PeerReply::Edit {
            result: edit_result(EditOutcome::StaleSource).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
            note: None,
            operation: None,
        };
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        // symbol-read-edit-v1 §3.2 rule 6: a write that found the file changed answers with the
        // contract sentence and a repeat-the-call repair, and mentions no file-version reference.
        assert_eq!(
            text,
            "Nothing was written; the file changed while the edit was applied: src/lib.rs\nRepair: Repeat the call."
        );
        assert!(!text.contains("source_ref"), "{text}");
    }

    /// A stale deprecated `source_ref` names itself and says to send the edit without it.
    #[test]
    fn stale_source_ref_note_replaces_the_changed_while_applied_sentence() {
        let reply = PeerReply::Edit {
            result: edit_result(EditOutcome::StaleSource).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
            note: Some("the supplied source_ref does not match this file's current text. Repair: Send the edit without source_ref".into()),
            operation: None,
        };
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert_eq!(
            text,
            "The supplied source_ref does not match this file's current text; nothing was written.\nRepair: Send the edit without source_ref"
        );
    }

    /// A formatter that moved lines is stated as the reply's last line, with the reference the
    /// next edit must use.
    #[test]
    fn edit_reply_states_formatter_line_movement() {
        let mut reply = successful_edit_reply(EditDiagnostics::CurrentClean { project_errors: 0 });
        if let PeerReply::Edit { note, .. } = &mut reply {
            *note = Some(
                "formatted: +3 lines after line 24; use source_ref sym-9 for the next edit"
                    .to_owned(),
            );
        }
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(
            text.ends_with(
                "\nformatted: +3 lines after line 24; use source_ref sym-9 for the next edit"
            ),
            "{text}"
        );
        // A reply whose formatter moved nothing carries no movement line at all.
        let unchanged = successful_edit_reply(EditDiagnostics::CurrentClean { project_errors: 0 });
        assert!(!text_of(&render(unchanged, Envelope::TextOnly).unwrap()).contains("formatted:"));
    }

    /// Keeps exact truncated references in model text and omits unrelated structured field names.
    ///
    /// Diff's `detail_ref` is only ever useful for a retained continuation (never for a later
    /// `ide.edit`, which requires a Context source), so a non-continuation truncated Diff must not
    /// name it; Context's own `detail_ref` doubling as an `ide.edit` `source_ref` is covered by
    /// [`context_text_always_carries_its_edit_source_ref`].
    #[test]
    fn compact_text_excludes_json_duplication_and_unneeded_fields() {
        let rendered = render(
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: "bounded owner evidence".into(),
                detail_ref: Some("private-live-detail".into()),
                truncated: true,
                continuation: false,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("bounded owner evidence"));
        assert!(!text.contains("private-live-detail"));
        for excluded in ["\"state\"", "\"kind\"", "structured_content", "is_error"] {
            assert!(!text.contains(excluded));
        }
    }

    /// Edits need no file-version reference, so a Context reply never teaches one: it is the
    /// bare text, plus a single cut trailer when the text is incomplete.
    #[test]
    fn context_text_never_carries_a_source_ref() {
        for (truncated, continuation) in [(false, false), (true, false)] {
            let rendered = render(
                PeerReply::Complete {
                    kind: ResultKind::Context,
                    text: "owner evidence".into(),
                    detail_ref: Some("live-source-detail".into()),
                    truncated,
                    continuation,
                },
                Envelope::WithStructured,
            )
            .unwrap();
            let text = text_of(&rendered);
            assert!(
                !text.contains("source_ref") && !text.contains("live-source-detail"),
                "{text}"
            );
            assert_eq!(text.contains("Cut;"), truncated, "{text}");
        }
    }

    /// A problems Context keeps its v0.2 text without advertising an edit reference.
    #[test]
    fn context_without_source_keeps_bare_content() {
        let rendered = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "beta: unavailable:env_missing".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert_eq!(text, "beta: unavailable:env_missing");
    }

    /// Page hints select the actual implemented originating read/graph/test tool; compatibility
    /// context pages retain inspect instead of advertising the removed public context call.
    #[test]
    fn continuation_hints_use_the_originating_domain() {
        for (kind, tool) in [
            (ResultKind::Read, "ide.read"),
            (ResultKind::Graph, "ide.graph"),
            (ResultKind::Test, "ide.test"),
            (ResultKind::Context, "ide.inspect"),
        ] {
            let result = render(
                PeerReply::Complete {
                    kind,
                    text: "body".into(),
                    detail_ref: Some("next".into()),
                    truncated: true,
                    continuation: true,
                },
                Envelope::TextOnly,
            )
            .unwrap();
            assert!(
                text_of(&result).ends_with(&format!(
                    "Cut; {tool} {{\"detail_ref\":\"next\"}} continues"
                )),
                "{}",
                text_of(&result)
            );
        }
    }

    /// Keeps a page's final source newline separate from the continuation instruction.
    #[test]
    fn paged_context_preserves_source_trailing_newline() {
        let rendered = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "page 1; bytes 0-2 of 4\n\nx\n".into(),
                detail_ref: Some("next-page".into()),
                truncated: true,
                continuation: true,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert_eq!(
            text_of(&rendered),
            "page 1; bytes 0-2 of 4\n\nx\n\nCut; ide.inspect {\"detail_ref\":\"next-page\"} continues"
        );
        let without_trailing_newline = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "x".into(),
                detail_ref: Some("next-page".into()),
                truncated: true,
                continuation: true,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert_eq!(
            text_of(&without_trailing_newline),
            "x\nCut; ide.inspect {\"detail_ref\":\"next-page\"} continues"
        );
    }

    /// Recommends inspection only for a typed retained continuation, never merely for a handle.
    #[test]
    fn incomplete_results_do_not_infer_continuation_from_detail_reference() {
        let context = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "partial context".into(),
                detail_ref: Some("context-detail".into()),
                truncated: true,
                continuation: false,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let diff = render(
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: "partial diff".into(),
                detail_ref: Some("diff-detail".into()),
                truncated: true,
                continuation: false,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let paged = render(
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: "paged diff".into(),
                detail_ref: Some("page-detail".into()),
                truncated: true,
                continuation: true,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert!(
            text_of(&context).contains("nothing more to fetch")
                && !text_of(&context).contains("ide.inspect")
        );
        assert!(
            text_of(&diff).contains("nothing more to fetch")
                && !text_of(&diff).contains("ide.inspect")
        );
        assert!(text_of(&paged).contains("ide.inspect {\"detail_ref\":\"page-detail\"}"));
    }

    /// Sends duplicate-edit recovery through the IDE context path with a fresh operation identity.
    #[test]
    fn conflicting_duplicate_requests_context_and_a_new_operation_id() {
        let rendered = render(
            PeerReply::Edit {
                result: edit_result(EditOutcome::ConflictingDuplicate).unwrap(),
                diagnostics: EditDiagnostics::Unknown {},
                note: None,

                operation: None,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("with ide.read") && text.contains("new operation_id"));
        assert!(!text.contains("native tools"));
    }

    /// Shrinks only complete owner text on UTF-8 boundaries and measures the final MCP bytes.
    #[test]
    fn utf8_shrinking_fits_the_final_call_tool_result() {
        let rendered = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "\0🦀\"\\".repeat(16_000),
                detail_ref: Some("same-binding-detail".into()),
                truncated: false,
                continuation: false,
            },
            Envelope::WithStructured,
        )
        .unwrap();
        assert!(call_tool_result_fits(&rendered));
        let structured = rendered.structured_content.as_ref().unwrap();
        assert_eq!(structured["truncated"], true);
        assert!(structured["text"].as_str().unwrap().contains('🦀'));
        assert!(text_of(&rendered).contains('🦀'));
    }

    /// The Claude host reads `structuredContent` straight into its model in place of `content`
    /// (T14B); `Envelope::TextOnly` must therefore never populate it, while the compact text stays
    /// exactly as informative as the `WithStructured` projection of the same reply.
    #[test]
    fn text_only_envelope_omits_structured_content() {
        let reply = PeerReply::Complete {
            kind: ResultKind::Activation,
            text: "durable capture true".into(),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        assert_eq!(rendered.structured_content, None);
        assert_eq!(rendered.content.len(), 1);
        assert!(text_of(&rendered).contains("durable capture true"));
    }

    /// A carried status plate (T28B) leads both carriers: one newline after the plate, the
    /// compact reply text follows, and the structured copy names the plate as its leading
    /// `status` string field.
    #[test]
    fn carried_status_leads_both_carriers() {
        let plate = "<agent-ide>\nalpha: 2 errors, 0 warnings\n</agent-ide>";
        let rendered = render_with_status(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "alpha: ready; errors: 2; warnings: 0".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Some(plate),
            Envelope::WithStructured,
        )
        .unwrap();
        assert!(
            text_of(&rendered).starts_with(&format!("{plate}\nalpha: ready")),
            "{}",
            text_of(&rendered)
        );
        let structured = rendered.structured_content.unwrap();
        let fields = structured.as_object().unwrap();
        // `status` is exposed on the structured copy; serde_json's map is order-free, so only
        // presence and verbatim content are asserted, not key position.
        assert_eq!(fields["status"], plate, "{fields:?}");
        assert_eq!(structured["state"], "complete");
        assert_eq!(structured["text"], "alpha: ready; errors: 2; warnings: 0");
    }

    /// The plate is never a shrink candidate: a reply that only fits with the plate after owner
    /// text was cut still carries the plate whole, and `fits_with_status` accepts exactly the
    /// forms the facade's own render loop accepts.
    #[test]
    fn fitting_cuts_owner_text_never_the_plate() {
        let plate = "<agent-ide>\nalpha: 2 errors, 0 warnings\n</agent-ide>";
        let mut reply = PeerReply::Complete {
            kind: ResultKind::Context,
            text: "evidence ".repeat(6000),
            detail_ref: None,
            truncated: false,
            continuation: false,
        };
        assert!(!fits_with_status(&reply, plate, Envelope::WithStructured));
        let rendered = loop {
            if fits_with_status(&reply, plate, Envelope::WithStructured) {
                break render_with_status(reply.clone(), Some(plate), Envelope::WithStructured)
                    .unwrap();
            }
            assert!(
                reply.shrink_text(),
                "owner text must make room, never the plate"
            );
        };
        assert!(call_tool_result_fits(&rendered));
        assert!(
            text_of(&rendered).starts_with(&format!("{plate}\n")),
            "{}",
            text_of(&rendered)
        );
        let structured = rendered.structured_content.unwrap();
        assert_eq!(structured["status"], plate);
        assert_eq!(structured["truncated"], true);
    }

    /// A path or provider message containing a newline or a bidi override cannot forge a
    /// structural line of the reply: the label renders as one visible line with an explicit
    /// escape, while the structured copy keeps the exact bytes (SAFE-02).
    #[test]
    fn untrusted_labels_cannot_forge_reply_lines() {
        let newline_path = render(
            PeerReply::Error {
                code: FailureCode::NoSuchFile("src/li\ng.rs".to_owned()),
                detail: Some("outline:no_such_file".to_owned()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let text = text_of(&newline_path);
        assert_eq!(text.lines().nth(2), Some("  src/li\\ng.rs"), "{text}");
        assert_eq!(text.lines().count(), 3, "{text}");
        assert_eq!(
            newline_path.structured_content.unwrap()["code"]["no_such_file"],
            "src/li\ng.rs"
        );

        let bidi_path = render(
            PeerReply::Edit {
                result: EditResult::new(
                    "op-9".into(),
                    "src/\u{202e}evil.rs".into(),
                    EditOutcome::Replaced,
                    Some("source-after-edit".into()),
                )
                .unwrap(),
                diagnostics: EditDiagnostics::CurrentClean { project_errors: 0 },
                note: None,
                operation: None,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&bidi_path);
        assert!(text.contains("path src/\\u202eevil.rs"), "{text}");
        assert!(!text.contains('\u{202e}'), "{text}");
    }

    /// Literal template syntax arriving inside data is bound as data: neither a provider
    /// diagnostic nor source text evaluates `{{ x }}` or `{% if %}`.
    #[test]
    fn literal_template_syntax_in_data_stays_data() {
        let hostile = "{{ scope }} {% if state == \"unavailable\" %} {{ 7 * 7 }}";
        let rendered = render(
            PeerReply::Edit {
                result: EditResult::new(
                    "op-9".into(),
                    "src/lib.rs".into(),
                    EditOutcome::Replaced,
                    Some("source-after-edit".into()),
                )
                .unwrap(),
                diagnostics: EditDiagnostics::CurrentReported {
                    messages: vec![hostile.to_owned()],
                    delta: "Provider reported 1 diagnostic for the exact source generation.".into(),
                    truncated: false,
                },
                note: None,
                operation: None,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains(hostile), "{text}");
        // `{{ 7 * 7 }}` stayed data: no evaluated arithmetic reached the reply.
        assert!(!text.contains("49"), "{text}");

        let source = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: hostile.to_owned(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        assert!(text_of(&source).contains(hostile));
    }

    /// The FAIL-01 receipt keeps the outcome word, exact path, operation id, pending
    /// diagnostics reference and no-replay rule for every edit outcome class, with `is_error`
    /// exactly as the normal projection would set it.
    #[test]
    fn degraded_edit_receipts_keep_outcome_identity_and_recovery() {
        let mut reply = successful_edit_reply(EditDiagnostics::Pending {
            detail_ref: "diag-detail".into(),
        });
        if let PeerReply::Edit { operation, .. } = &mut reply {
            *operation = Some("renamed".to_owned());
        }
        let rendered = PresentationReceipt::of(&reply).unwrap().degraded();
        assert_eq!(
            text_of(&rendered),
            "edit: renamed; path src/lib.rs; operation_id private-operation-id\n\
             Presentation degraded (presentation_failed). Do not repeat the mutation to repair \
             this response. Diagnostics pending; result retained under diag-detail; Bridge \
             delivers it automatically."
        );
        assert_ne!(rendered.is_error, Some(true));

        let unknown = PeerReply::Edit {
            result: edit_result(EditOutcome::OutcomeUnknown).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
            note: None,
            operation: None,
        };
        let rendered = PresentationReceipt::of(&unknown).unwrap().degraded();
        let text = text_of(&rendered);
        assert!(text.starts_with(
            "edit: outcome_unknown; path src/lib.rs; operation_id private-operation-id"
        ));
        assert!(text.contains("do not replay this operation"), "{text}");
        assert_eq!(rendered.is_error, Some(true));

        let refused = PeerReply::Edit {
            result: edit_result(EditOutcome::StaleSource).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
            note: None,
            operation: None,
        };
        let refused_degraded = PresentationReceipt::of(&refused).unwrap().degraded();
        let text = text_of(&refused_degraded);
        assert!(text.contains("No write occurred."), "{text}");
        assert_eq!(refused_degraded.is_error, Some(true));

        // Reads carry no receipt: their loss is the requested answer, not an unreported effect.
        assert!(
            PresentationReceipt::of(&PeerReply::Complete {
                kind: ResultKind::Context,
                text: "evidence".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            })
            .is_none()
        );
    }
    /// Invalid environment candidates teach selection recovery rather than detail-handle recovery.
    #[test]
    fn environment_refusal_names_selection_recovery() {
        let result = render(
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
                detail: Some("environment: alpha: candidate absent; candidates one, two".into()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&result);
        assert!(
            text.contains("Repair: Select one listed project environment with ide.start."),
            "{text}"
        );
        assert!(
            text.contains("alpha: candidate absent; candidates one, two"),
            "{text}"
        );
        assert!(!text.contains("detail_ref"), "{text}");
        assert_eq!(result.is_error, Some(true));
    }
}
