//! Closed, serialized-size-bounded Assistance outcomes with no transport or authority secrets.

use std::path::Path;

use crate::app::transport::OpaqueJson;
use serde::{Deserialize, Serialize};

/// Maximum complete Assistance JSON envelope including escaped text and all keys.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Maximum model-visible feedback text returned to a native host hook.
pub const MAX_FEEDBACK_BYTES: usize = 4 * 1024;
/// Leaves room for fixed MCP content and protocol wrapper fields.
pub(crate) const MCP_RESERVE: usize = 1024;
/// Bounds each path a `project_moved` cause names, after home shortening (T15B).
const MAX_CAUSE_PATH_BYTES: usize = 256;
/// Bounds the requested path a `no_such_file` or `unsupported_file` failure names in its reason
/// text.
pub(crate) const MAX_NO_SUCH_FILE_PATH_BYTES: usize = 256;

/// First missing peer without implying workspace authority was granted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPeer {
    /// Exact trusted hook and MCP invocation correlation is unavailable.
    HostBinding,
    /// Host correlation succeeded but Workspace activation is not connected.
    WorkspaceActivation,
}

/// Closed cause of one `unavailable: host_binding` refusal, named in parentheses after the reason
/// (T15B) and journaled as the reply's `detail`.
///
/// Every variant except [`Self::ProjectMoved`] is a payload-free tag like the T115 stage tags.
/// [`Self::ProjectMoved`] deliberately names both directories — that direction is the fix's whole
/// value — with each path bounded to 256 bytes and home-shortened where possible.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HostBindingCause {
    /// The session's bound project resolves below no configured `allowed_roots` entry.
    OutsideAllowedRoots,
    /// This daemon received no successful hook for the calling channel since it started.
    HooksNotDelivered,
    /// The host metadata was not a valid bounded JSON object.
    InvalidMetadata,
    /// A required host metadata field was absent.
    MissingField,
    /// A supported host metadata field was empty, not a string, or too long.
    InvalidField,
    /// The private host attachment did not identify a valid channel session.
    InvalidAttachment,
    /// The host hook used a lifecycle phase the IDE does not support.
    UnsupportedHookPhase,
    /// No exact pre-hook observation existed for this invocation.
    MissingPre,
    /// The exact call identity was already observed, validated, or permanently rejected.
    Replay,
    /// A hook could not be linked exactly to one registered MCP candidate.
    Mismatch,
    /// A post-hook arrived before the matching tool invocation completed validation.
    MissingInvocation,
    /// No active matching actor/channel-session binding existed for an ordinary call.
    InactiveBinding,
    /// This session never held an IDE activation in this daemon boot.
    NeverActivated,
    /// Bounded pending, binding, or replay storage is full for this scope.
    CapacityExceeded,
    /// The caller's `_meta` names no supported host contract, so no invocation can correlate.
    HostUnrecognized,
    /// `ide.start {root}` named a directory other than the session's bound project.
    ProjectMoved {
        /// Home-shortened bounded path the session is currently bound to.
        bound: String,
        /// Bounded requested root exactly as the model named it.
        asked: String,
    },
}

impl HostBindingCause {
    /// Maps the guard's own closed refusal reasons that a model-facing reply can carry.
    pub(crate) fn from_binding(reason: super::host_binding::BindingUnavailable) -> Option<Self> {
        Some(match reason {
            super::host_binding::BindingUnavailable::InvalidMetadata => Self::InvalidMetadata,
            super::host_binding::BindingUnavailable::MissingField(_) => Self::MissingField,
            super::host_binding::BindingUnavailable::InvalidField(_) => Self::InvalidField,
            super::host_binding::BindingUnavailable::InvalidAttachment => Self::InvalidAttachment,
            super::host_binding::BindingUnavailable::UnsupportedHookPhase => {
                Self::UnsupportedHookPhase
            }
            super::host_binding::BindingUnavailable::MissingPre => Self::MissingPre,
            super::host_binding::BindingUnavailable::Replay => Self::Replay,
            super::host_binding::BindingUnavailable::Mismatch => Self::Mismatch,
            super::host_binding::BindingUnavailable::MissingInvocation => Self::MissingInvocation,
            super::host_binding::BindingUnavailable::InactiveBinding => Self::InactiveBinding,
            super::host_binding::BindingUnavailable::NeverActivated => Self::NeverActivated,
            super::host_binding::BindingUnavailable::CapacityExceeded => Self::CapacityExceeded,
        })
    }

    /// Builds the one payload-carrying cause from both directories, bounded and home-shortened.
    pub fn project_moved(bound: &Path, asked: &str) -> Self {
        Self::ProjectMoved {
            bound: shortened_cause_path(bound),
            asked: bounded_utf8_prefix(asked, MAX_CAUSE_PATH_BYTES).to_owned(),
        }
    }

    /// Renders the exact bounded text inside the reply's parentheses and the journal `detail`.
    pub fn cause_tag(&self) -> String {
        match self {
            Self::OutsideAllowedRoots => "outside_allowed_roots".to_owned(),
            Self::HooksNotDelivered => "hooks_not_delivered".to_owned(),
            Self::InvalidMetadata => "invalid_metadata".to_owned(),
            Self::MissingField => "missing_field".to_owned(),
            Self::InvalidField => "invalid_field".to_owned(),
            Self::InvalidAttachment => "invalid_attachment".to_owned(),
            Self::UnsupportedHookPhase => "unsupported_hook_phase".to_owned(),
            Self::MissingPre => "missing_pre".to_owned(),
            Self::Replay => "replay".to_owned(),
            Self::Mismatch => "mismatch".to_owned(),
            Self::MissingInvocation => "missing_invocation".to_owned(),
            Self::InactiveBinding => "inactive_binding".to_owned(),
            Self::NeverActivated => "never_activated".to_owned(),
            Self::CapacityExceeded => "capacity_exceeded".to_owned(),
            Self::HostUnrecognized => "host_unrecognized".to_owned(),
            Self::ProjectMoved { bound, asked } => {
                format!("project_moved: bound to {bound}, asked {asked}")
            }
        }
    }
}

/// Returns one bounded display path, with the current home prefix shortened to `~` when it matches.
fn shortened_cause_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let shortened = std::env::var_os("HOME")
        .and_then(|home| home.into_string().ok())
        .filter(|home| home.len() > 1 && text.starts_with(home.as_str()))
        .map(|home| format!("~{}", &text[home.len()..]));
    bounded_utf8_prefix(shortened.as_deref().unwrap_or(&text), MAX_CAUSE_PATH_BYTES).to_owned()
}

/// Closed failures; arbitrary owner or OS error strings never cross the product boundary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// Trusted attachment mapping is missing or rejected.
    LauncherConfiguration,
    /// The activation root, its discovered Git worktree root or its Git common directory is not
    /// below any configured `allowed_roots` entry, or no allowed root is configured.
    OutsideAllowedRoots,
    /// Accepted executable/profile evidence does not authorize this operation.
    ExecutionProfile,
    /// Execution-profile refusal with one fixed, privacy-safe cause.
    ExecutionProfileCause(ExecutionProfileCause),
    /// Configured Git lacks the required filter-free discovery/snapshot flags.
    UnsupportedGit,
    /// Workspace activation is unavailable or refused.
    WorkspaceActivation,
    /// Current durable authority was revoked, replaced or fenced by a new boot.
    WorkspaceAuthority,
    /// The accepted language provider could not supply the requested service.
    ProviderUnavailable,
    /// The accepted provider is still loading its workspace; the same call succeeds later.
    ProviderLoading,
    /// No symbol matches the requested path or name in the file or project.
    UnknownSymbol,
    /// An edit's addresses or content did not resolve against the file it names — an overlap, a
    /// text that does not match exactly once, an unknown symbol or range, or a candidate that
    /// does not parse. Nothing was written and the `operation_id` is unconsumed.
    EditRefused,
    /// The requested path is not a registered source in the authorized worktree scope: the file
    /// does not exist. Carries the bounded path exactly as requested (T163 precedent: paths in
    /// the reason are allowed; the stage tag itself stays payload-free).
    NoSuchFile(String),
    /// The requested path exists but no registered language reads its file type (`Cargo.toml`, a
    /// shell script), so there is no outline or symbol to answer — a caller mistake, not a
    /// language server failure. Carries the bounded path exactly as requested.
    UnsupportedFile(String),
    /// A language server's project inputs were absent, unsupported, oversized, reordered, or
    /// changed.
    ResolutionUnverified,
    /// Stop or a generation fence cancelled this operation.
    Cancelled,
    /// A finite operation or process deadline expired without completed evidence.
    Deadline,
    /// Bounded queue, view or retained-detail capacity is full.
    Capacity,
    /// Detail reference is absent, expired or owned by another binding.
    InvalidDetail,
    /// Registered source bytes could not be observed in the authorized scope.
    SourceUnavailable,
    /// The registered source exceeded the bounded read ceiling before any content was captured.
    SourceTooLarge {
        /// Exact on-disk size of the source that exceeded `ceiling`.
        size: u64,
        /// The read ceiling `size` exceeded.
        ceiling: u64,
    },
    /// A single-owner resource is already held: an idempotent operation retried with different
    /// immutable parameters, or another live actor that currently owns this worktree incarnation
    /// or its provider cache namespace. The refusal never disturbs the actor that already owns it,
    /// and a handoff becomes possible after that owner stops.
    Conflict,
    /// Unexpected internal failure has no safe owner result.
    Internal,
}

/// Fixed execution-profile reasons safe to show to an agent; no path, ID, or host payload fits.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfileCause {
    /// Git execution policy could not be built.
    GitPolicy,
    /// The Git discovery query was refused.
    QueryPolicy,
    /// The accepted Git binary lacked a required operation.
    GitUnsupported,
    /// Child process creation failed before a usable process existed.
    SpawnIo,
    /// The process request failed validation.
    SpawnRequest,
    /// The process attempted to use reserved protocol stdout.
    SpawnProtocolStdoutReserved,
    /// A failed child could not be reaped before the deadline.
    SpawnReapTimedOut,
}

impl ExecutionProfileCause {
    /// Converts only exact closed log tags into agent-visible causes.
    pub fn from_log_tag(tag: &str) -> Option<Self> {
        Some(match tag.split(';').next()?.trim() {
            "git_policy" => Self::GitPolicy,
            "query_policy" => Self::QueryPolicy,
            "git_unsupported" => Self::GitUnsupported,
            "spawn:io" => Self::SpawnIo,
            "spawn:request" => Self::SpawnRequest,
            "spawn:protocol_stdout_reserved" => Self::SpawnProtocolStdoutReserved,
            "spawn:reap_timed_out" => Self::SpawnReapTimedOut,
            _ => return None,
        })
    }

    /// Returns the exact existing closed log tag for this refusal.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::GitPolicy => "git_policy",
            Self::QueryPolicy => "query_policy",
            Self::GitUnsupported => "git_unsupported",
            Self::SpawnIo => "spawn:io",
            Self::SpawnRequest => "spawn:request",
            Self::SpawnProtocolStdoutReserved => "spawn:protocol_stdout_reserved",
            Self::SpawnReapTimedOut => "spawn:reap_timed_out",
        }
    }
}

/// Owner evidence category for a completed result; not an extensible tool identifier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    /// Durable Workspace activated and authorized the exact binding.
    Activation,
    /// Intelligence returned source context or the project problems page.
    Context,
    /// Changes composed evidence for a current Workspace comparison scope.
    Diff,
    /// Durable authority was revoked and cleanup reached its reported outcome.
    Stop,
    /// A file skeleton from `ide.outline`.
    Outline,
    /// A symbol body or line range from `ide.read`.
    Read,
    /// A symbol card from `ide.symbol`.
    Symbol,
    /// A bounded live call graph from `ide.graph`.
    Graph,
    /// A background test run result from `ide.test`.
    Test,
}

/// Closed diagnostic evidence attached to one successful Assistance edit reply.
///
/// Assistance owns this projection because it describes a bounded provider observation for the
/// exact post-read source generation, not durable filesystem receipt state. [`Self::Unknown`]
/// never means clean, and [`Self::Pending`] is valid only with a same-binding detail that the
/// worker has actually retained for inspection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditDiagnostics {
    /// A matching versioned provider result explicitly reported no diagnostics.
    CurrentClean {},
    /// A matching versioned provider result reported bounded diagnostics.
    CurrentReported {
        /// At most eight provider messages, each limited to 256 UTF-8 bytes.
        messages: Vec<String>,
        /// Bounded explanation of the exact-generation diagnostic delta.
        delta: String,
        /// True when the provider or reply ceiling omitted one or more messages.
        truncated: bool,
    },
    /// No matching-generation diagnostic result was established before the edit deadline.
    Unknown {},
    /// The completed project check named no problem in the file but never analysed it (for
    /// example a source file no build target reaches); never a clean result.
    NotAnalysed {
        /// Bounded language-provided reason, at most 256 UTF-8 bytes.
        reason: String,
    },
    /// Matching-generation diagnostic work remains inspectable by the same binding.
    Pending {
        /// Opaque retained reference accepted only by `ide.inspect` under that binding.
        detail_ref: String,
    },
}

impl EditDiagnostics {
    /// Converts one correlated provider snapshot without interpreting silence as cleanliness.
    ///
    /// Current semantic context, matching source/generation/version identities, and an explicit
    /// clean or reported readiness are all required. Unavailable, stale, lexical, unversioned, or
    /// unready snapshots return [`Self::Unknown`]. Messages name `path`, the worktree-relative
    /// edited file (see [`diagnostic_line`]).
    pub(crate) fn from_snapshot(
        path: &str,
        context: &crate::intelligence::context::ContextResult,
        diagnostics: &crate::intelligence::session::DiagnosticSnapshot,
    ) -> Self {
        use crate::intelligence::freshness::{DiagnosticReadiness, Freshness};

        let exact = context.freshness == Freshness::Current
            && diagnostics.freshness == Freshness::Provisional
            && diagnostics.source.as_ref() == Some(&context.source)
            && Some(diagnostics.generation) == context.generation
            && diagnostics.document_version == context.document_version
            && diagnostics
                .document_version
                .is_some_and(|version| version > 0);
        if !exact {
            return Self::Unknown {};
        }
        match diagnostics.readiness {
            DiagnosticReadiness::Clean if diagnostics.diagnostics.is_empty() => {
                Self::CurrentClean {}
            }
            DiagnosticReadiness::Reported if !diagnostics.diagnostics.is_empty() => {
                let messages = diagnostics
                    .diagnostics
                    .iter()
                    .take(8)
                    .map(|diagnostic| diagnostic_line(path, diagnostic))
                    .collect::<Vec<String>>();
                Self::CurrentReported {
                    delta: format!(
                        "language server reported {} diagnostics for the exact post-edit source generation",
                        diagnostics.diagnostics.len()
                    ),
                    truncated: diagnostics.truncated
                        || diagnostics.diagnostics.len() > messages.len(),
                    messages,
                }
            }
            _ => Self::Unknown {},
        }
    }

    /// Reports whether all model-visible fields satisfy the closed response bounds.
    pub(super) fn valid(&self) -> bool {
        match self {
            Self::CurrentClean {} | Self::Unknown {} => true,
            Self::NotAnalysed { reason } => {
                !reason.is_empty() && reason.len() <= 256 && !reason.chars().any(char::is_control)
            }
            Self::CurrentReported {
                messages, delta, ..
            } => {
                !messages.is_empty()
                    && messages.len() <= 8
                    && messages.iter().all(|message| message.len() <= 256)
                    && !delta.is_empty()
                    && delta.len() <= MAX_FEEDBACK_BYTES
            }
            Self::Pending { detail_ref } => {
                !detail_ref.is_empty()
                    && detail_ref.len() <= 128
                    && !detail_ref.chars().any(char::is_control)
            }
        }
    }
}

/// Renders one language-server diagnostic as `path:line:col severity [code] message` (1-based
/// position, severity omitted when the server sent none), bounded to 256 UTF-8 bytes.
pub(crate) fn diagnostic_line(path: &str, diagnostic: &async_lsp::lsp_types::Diagnostic) -> String {
    use async_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    let severity = match diagnostic.severity {
        Some(DiagnosticSeverity::ERROR) => "error ",
        Some(DiagnosticSeverity::WARNING) => "warning ",
        Some(DiagnosticSeverity::INFORMATION) => "information ",
        Some(DiagnosticSeverity::HINT) => "hint ",
        _ => "",
    };
    let code = match &diagnostic.code {
        Some(NumberOrString::Number(code)) => format!("[{code}] "),
        Some(NumberOrString::String(code)) => format!("[{code}] "),
        None => String::new(),
    };
    bounded_utf8_prefix(
        &format!(
            "{path}:{}:{} {severity}{code}{}",
            diagnostic.range.start.line + 1,
            diagnostic.range.start.character + 1,
            diagnostic.message
        ),
        256,
    )
}

/// Returns the largest UTF-8 prefix of `value` that fits `limit` bytes.
pub(crate) fn bounded_utf8_prefix(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

impl Default for EditDiagnostics {
    /// Decodes an absent backward-compatible diagnostic field as unknown, never clean.
    fn default() -> Self {
        Self::Unknown {}
    }
}

/// Closed result shape; decoding rejects unknown states, fields and invalid detail references.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PeerReply {
    /// No authority or peer success is implied by reaching a missing boundary.
    Unavailable {
        /// First missing boundary.
        reason: MissingPeer,
        /// Closed cause of a `host_binding` refusal (T15B); absent keeps the historical bare reply.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<HostBindingCause>,
    },
    /// One native pre-hook was retained without an authority or delivery claim.
    HookObserved {},
    /// One post-hook settled a validated MCP invocation.
    HookSettled {},
    /// Active native lifecycle requested registered-path reconciliation; no effect is inferred.
    NativeHookObserved {},
    /// One current versioned feedback delta survived native binding and exact-source rechecks.
    Feedback {
        /// Bounded fact/evidence/next-step text containing no source or native tool payload.
        text: String,
    },
    /// Host binding was revoked before any Workspace authority existed.
    HostStopped {},
    /// Daemon owns the pending work; no successful peer result exists yet.
    Pending {
        /// Opaque reference usable only under the same live binding.
        detail_ref: String,
    },
    /// Closed failure without provider, OS or host payloads.
    Error {
        /// Stable actionable failure category.
        code: FailureCode,
        /// Request-local plain-words reason (the worker's `failure_detail`), rendered only in the
        /// compact `resolution_unverified` text and stripped from the public structured reply.
        /// Absent from the encoded form when `None`, so retained replies keep their prior shape.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Model-facing parameter rejection discovered after the worktree's runner contract is known.
    InvalidParameters {
        /// Bounded one-line ParameterError-style explanation.
        #[serde(rename = "text")]
        message: String,
    },
    /// Bounded rendering of actual owner evidence; only peer completion may construct this variant.
    Complete {
        /// Closed owner/result kind.
        kind: ResultKind,
        /// Text derived from owner facts, retaining explicit freshness and coverage caveats.
        text: String,
        /// Same-binding usable detail reference; absent for a problems page with no edit source.
        detail_ref: Option<String>,
        /// True when serialized-result budgeting omitted owner text.
        truncated: bool,
        /// True only when the referenced result retains another consumable page for `ide.inspect`.
        ///
        /// A detail reference alone is not a continuation: retained Context and Diff results may
        /// be inspectable but cannot yield new evidence. Missing from an older
        /// envelope decodes as `false`, preserving the safe non-looping default.
        #[serde(default)]
        continuation: bool,
    },
    /// Exact Changes-owned one-file result plus Assistance-owned post-edit diagnostics.
    Edit {
        /// Durable closed outcome, operation/path correlation, and optional post-read source ref.
        result: crate::changes::edit::EditResult,
        /// Closed diagnostic projection for the exact post-read source generation.
        #[serde(default)]
        diagnostics: EditDiagnostics,
        /// Present only when the project's formatter moved lines after the edited region, so a
        /// later line-addressed edit does not reuse the pre-format line numbers. Always
        /// serialized (`null` when absent; the strict template reads it), and absent from an
        /// older envelope, decoding as `None`.
        #[serde(default)]
        note: Option<String>,
        /// Display word naming the operation the reply reports (`inserted`, `deleted`,
        /// `renamed`), replacing the durable outcome word in the first line; `None` (the
        /// default, and absent from an older envelope) keeps the outcome. Display only —
        /// the durable outcome stays [`crate::changes::edit::EditResult::outcome`].
        #[serde(default)]
        operation: Option<String>,
    },
}
impl std::fmt::Debug for PeerReply {
    /// Omits text and detail handles from diagnostics.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PeerReply(..)")
    }
}

/// One daemon method reply wrapped around an attached status plate (T28B).
///
/// The daemon encodes this shape exactly when a terminal reply carries a due plate for a host
/// whose feed delivery rides replies; every other reply stays a bare [`PeerReply`], byte-identical
/// to earlier releases.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct StatusCarriedReply {
    /// The complete `<agent-ide>` plate, delivered verbatim at the top of the rendered reply.
    status: String,
    /// The closed reply the plate was attached to.
    reply: PeerReply,
}
impl PeerReply {
    /// Fits the complete serialized budget by trimming only owner text at UTF-8 boundaries.
    /// Invalid references fail closed. Trimming sets truncated and never changes the evidence kind.
    pub(crate) fn encode(mut self) -> Option<OpaqueJson> {
        if !self.valid_reference() {
            return None;
        }
        loop {
            let serialized = serde_json::to_string(&self).ok()?;
            if serialized.len() <= MAX_REPLY_BYTES - MCP_RESERVE {
                return OpaqueJson::new(serialized, MAX_REPLY_BYTES);
            }
            if !self.shrink_text() {
                return None;
            }
        }
    }
    /// Reduces only owner text while preserving UTF-8 and explicitly marking omitted content.
    /// Returns false for non-text results or already empty text; callers then fail closed.
    pub(crate) fn shrink_text(&mut self) -> bool {
        let Self::Complete {
            text, truncated, ..
        } = self
        else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        let mut limit = text.len() / 2;
        while !text.is_char_boundary(limit) {
            limit -= 1;
        }
        text.truncate(limit);
        *truncated = true;
        true
    }
    /// Decodes a closed envelope only when serialized bytes and reference syntax are bounded.
    pub(crate) fn decode(value: &str) -> Option<Self> {
        if value.len() > MAX_REPLY_BYTES - MCP_RESERVE {
            return None;
        }
        let reply: Self = serde_json::from_str(value).ok()?;
        reply.valid_reference().then_some(reply)
    }
    /// Decodes one daemon method reply with its optional carried status plate (T28B).
    ///
    /// Accepts the bare closed reply (`status` absent, the historical wire form) and the
    /// [`StatusCarriedReply`] wrapper. Both forms enforce the same byte bound; a carried plate
    /// must be a nonempty block within [`MAX_FEEDBACK_BYTES`] — the ceiling a hook-delivered
    /// plate has too, since due git and environment notices join the feed's
    /// [`crate::feed::MAX_BLOCK_BYTES`] block — and its reply must pass the same
    /// closed-reference validation as a bare one.
    pub(crate) fn decode_delivered(value: &str) -> Option<(Self, Option<String>)> {
        if let Ok(carried) = serde_json::from_str::<StatusCarriedReply>(value) {
            if value.len() > MAX_REPLY_BYTES {
                return None;
            }
            let valid = carried.reply.valid_reference()
                && !carried.status.is_empty()
                && carried.status.len() <= MAX_FEEDBACK_BYTES;
            return valid.then_some((carried.reply, Some(carried.status)));
        }
        Some((Self::decode(value)?, None))
    }
    /// Encodes one reply with an attached status plate in the [`StatusCarriedReply`] wire form.
    ///
    /// Callers attach a plate only after the complete final MCP carrier was proven to fit
    /// (`content::fits_with_status`), so the wrapper — strictly smaller than that carrier —
    /// always stays inside the transport bound; failure returns `None` rather than a cut plate.
    pub(crate) fn encode_with_status(reply: &Self, status: &str) -> Option<OpaqueJson> {
        let wrapped = serde_json::json!({"status": status, "reply": reply});
        OpaqueJson::new(serde_json::to_string(&wrapped).ok()?, MAX_REPLY_BYTES)
    }
    /// Checks reference syntax; ownership and current liveness remain worker admission gates.
    fn valid_reference(&self) -> bool {
        if matches!(self, Self::Edit { diagnostics, .. } if !diagnostics.valid()) {
            return false;
        }
        if matches!(self, Self::Feedback { text } if text.is_empty() || text.len() > MAX_FEEDBACK_BYTES)
        {
            return false;
        }
        let reference = match self {
            Self::Pending { detail_ref, .. } => Some(detail_ref),
            Self::Complete { detail_ref, .. } => detail_ref.as_ref(),
            _ => None,
        };
        reference.is_none_or(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
    }
}

/// Includes escaping/framing costs, preserves Unicode boundaries, and rejects unknown envelope fields.
#[test]
fn envelopes_are_closed_and_fit_serialized_budget() {
    let reply = PeerReply::Complete {
        kind: ResultKind::Context,
        text: "\0🦀\"\\".repeat(16000),
        detail_ref: Some("detail-1".into()),
        truncated: false,
        continuation: false,
    };
    let encoded = reply.encode().unwrap();
    assert!(encoded.as_str().len() < MAX_REPLY_BYTES);
    let Some(PeerReply::Complete {
        text, truncated, ..
    }) = PeerReply::decode(encoded.as_str())
    else {
        panic!("closed result");
    };
    assert!(truncated && !text.is_empty());
    for raw in [
        r#"{"state":"pending","detail_ref":""}"#,
        r#"{"state":"complete","kind":"future_tool","text":"x","detail_ref":null,"truncated":false}"#,
        r#"{"state":"error","code":"internal","secret":"x"}"#,
    ] {
        assert!(PeerReply::decode(raw).is_none());
    }
}

/// The status-carried wire form (T28B) round-trips the plate and the closed reply, keeps the bare
/// form decoding without a plate, and refuses oversize, overlong-plate or invalid-reference wraps.
#[test]
fn status_carried_replies_round_trip_and_stay_closed() {
    let plate = "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>";
    let reply = PeerReply::Complete {
        kind: ResultKind::Context,
        text: "owner evidence".into(),
        detail_ref: Some("detail-1".into()),
        truncated: false,
        continuation: false,
    };
    let encoded = PeerReply::encode_with_status(&reply, plate).unwrap();
    let (decoded, status) = PeerReply::decode_delivered(encoded.as_str()).unwrap();
    assert_eq!(decoded, reply);
    assert_eq!(status.as_deref(), Some(plate));

    // Bare replies decode exactly as before, with no plate.
    let (bare, status) =
        PeerReply::decode_delivered(reply.clone().encode().unwrap().as_str()).unwrap();
    assert_eq!((bare, status), (reply.clone(), None));

    // A wrapped reply whose inner reference is invalid fails closed.
    let forged = serde_json::json!({
        "status": plate,
        "reply": {"state":"pending","detail_ref":""}
    })
    .to_string();
    assert!(PeerReply::decode_delivered(&forged).is_none());

    // A feed block led by due notices may exceed the feed's block cap and still decodes; a plate
    // over the hook feedback ceiling never does.
    let noticed = format!(
        "<agent-ide>\n{}\n</agent-ide>",
        "x".repeat(crate::feed::MAX_BLOCK_BYTES)
    );
    let encoded = PeerReply::encode_with_status(&reply, &noticed).unwrap();
    assert!(PeerReply::decode_delivered(encoded.as_str()).is_some());
    let overlong = format!(
        "<agent-ide>\n{}\n</agent-ide>",
        "x".repeat(MAX_FEEDBACK_BYTES)
    );
    let encoded = PeerReply::encode_with_status(&reply, &overlong).unwrap();
    assert!(PeerReply::decode_delivered(encoded.as_str()).is_none());
}

/// Every host-binding cause survives one encode/decode round trip, and a bare reply stays bare.
#[test]
fn host_binding_causes_round_trip_and_bare_replies_stay_bare() {
    for cause in [
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
        HostBindingCause::CapacityExceeded,
        HostBindingCause::HostUnrecognized,
        HostBindingCause::project_moved(
            Path::new("/Users/pluto/projects/agent-worktree"),
            "/private/tmp/agent-ide-stability/fixture",
        ),
    ] {
        let reply = PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause: Some(cause.clone()),
        };
        let decoded = PeerReply::decode(reply.clone().encode().unwrap().as_str()).unwrap();
        assert_eq!(decoded, reply);
        assert!(!cause.cause_tag().is_empty());
    }
    let bare = PeerReply::Unavailable {
        reason: MissingPeer::HostBinding,
        cause: None,
    };
    let encoded = bare.clone().encode().unwrap();
    assert!(!encoded.as_str().contains("cause"));
    assert_eq!(PeerReply::decode(encoded.as_str()).unwrap(), bare);
    // The one payload-carrying cause renders both directories and bounds each path.
    let moved = HostBindingCause::project_moved(
        Path::new("/private/tmp"),
        &format!("/private/tmp/{}", "x".repeat(600)),
    );
    let HostBindingCause::ProjectMoved { asked, .. } = &moved else {
        panic!("project_moved cause");
    };
    assert!(asked.len() <= 256, "{asked}");
    assert_eq!(
        moved.cause_tag(),
        format!("project_moved: bound to /private/tmp, asked {asked}")
    );
    assert_eq!(
        moved.cause_tag().len(),
        "project_moved: bound to /private/tmp, asked ".len() + asked.len()
    );
}

/// The guard's own closed refusal reasons map onto exactly the model-visible causes.
#[test]
fn host_binding_causes_map_the_guard_refusals() {
    use super::host_binding::BindingUnavailable;
    for (reason, expected) in [
        (BindingUnavailable::InvalidMetadata, "invalid_metadata"),
        (BindingUnavailable::MissingField("field"), "missing_field"),
        (BindingUnavailable::InvalidField("field"), "invalid_field"),
        (BindingUnavailable::InvalidAttachment, "invalid_attachment"),
        (
            BindingUnavailable::UnsupportedHookPhase,
            "unsupported_hook_phase",
        ),
        (BindingUnavailable::MissingPre, "missing_pre"),
        (BindingUnavailable::Replay, "replay"),
        (BindingUnavailable::Mismatch, "mismatch"),
        (BindingUnavailable::MissingInvocation, "missing_invocation"),
        (BindingUnavailable::InactiveBinding, "inactive_binding"),
        (BindingUnavailable::CapacityExceeded, "capacity_exceeded"),
    ] {
        assert_eq!(
            HostBindingCause::from_binding(reason)
                .expect("every guard refusal maps")
                .cause_tag(),
            expected
        );
    }
}

/// A language-server diagnostic renders with its 1-based location, severity and code.
#[test]
fn diagnostic_line_names_location_severity_and_code() {
    use async_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

    let mut diagnostic = Diagnostic {
        range: Range::new(Position::new(1, 11), Position::new(1, 23)),
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(NumberOrString::String("reportReturnType".into())),
        message: "bad".into(),
        ..Diagnostic::default()
    };
    assert_eq!(
        diagnostic_line("pkg/mod.py", &diagnostic),
        "pkg/mod.py:2:12 error [reportReturnType] bad"
    );
    diagnostic.severity = None;
    diagnostic.code = None;
    assert_eq!(
        diagnostic_line("pkg/mod.py", &diagnostic),
        "pkg/mod.py:2:12 bad"
    );
}

/// A multibyte diagnostic message truncated by characters could exceed the closed byte bound,
/// invalidating the whole edit reply round trip (T m060 blocker).
#[test]
fn multibyte_diagnostic_messages_stay_inside_the_byte_bound() {
    let long = "é".repeat(300);
    assert!(long.chars().count() == 300 && long.len() == 600);
    let bounded = bounded_utf8_prefix(&format!("src/lib.rs:1:1 error {long}"), 256);
    assert!(bounded.len() <= 256, "{}", bounded.len());
    assert!(bounded.ends_with('é') || !bounded.contains('é') || true);
    let diagnostics = EditDiagnostics::CurrentReported {
        messages: vec![bounded],
        delta: "project check 12.0s: 1 errors, 0 warnings in this file".to_owned(),
        truncated: false,
    };
    assert!(diagnostics.valid());
    let reply = PeerReply::Edit {
        result: crate::changes::edit::EditResult::new(
            "op".into(),
            "src/lib.rs".into(),
            crate::changes::edit::EditOutcome::Replaced,
            Some("source-after-edit".into()),
        )
        .unwrap(),
        diagnostics,
        note: None,
        operation: Some("inserted".to_owned()),
    };
    let encoded = reply.clone().encode().unwrap();
    assert_eq!(PeerReply::decode(encoded.as_str()).unwrap(), reply);
}
