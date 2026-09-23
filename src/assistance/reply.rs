//! Closed, serialized-size-bounded Assistance outcomes with no transport or authority secrets.

use crate::app::transport::OpaqueJson;
use serde::{Deserialize, Serialize};

/// Maximum complete Assistance JSON envelope including escaped text and all keys.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Maximum model-visible feedback text returned to a native host hook.
pub const MAX_FEEDBACK_BYTES: usize = 4 * 1024;
/// Maximum complete foreground-helper instruction carried on a pending reply.
///
/// The instruction is never trimmed, so this bound is a hard admission gate: an operation whose
/// exact command would not fit is refused rather than answered with an unusable partial command.
pub const MAX_HELPER_INSTRUCTION_BYTES: usize = 8 * 1024;
/// Leaves room for fixed MCP content and protocol wrapper fields.
pub(crate) const MCP_RESERVE: usize = 1024;

/// First missing peer without implying workspace authority was granted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPeer {
    /// Exact trusted hook and MCP invocation correlation is unavailable.
    HostBinding,
    /// Host correlation succeeded but Workspace activation is not connected.
    WorkspaceActivation,
}

/// Closed failures; arbitrary owner or OS error strings never cross the product boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCode {
    /// Trusted attachment mapping is missing or rejected.
    LauncherConfiguration,
    /// Required measured Codex sandbox metadata is absent or invalid.
    SandboxState,
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
    /// TypeScript project inputs were absent, unsupported, oversized, reordered, or changed.
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
    /// The disabled host was not admitted by launcher policy.
    HostDisabled,
    /// No managed profile matches the observed state.
    NoProfileManaged,
    /// No disabled profile matches the observed state.
    NoProfileDisabled,
    /// The managed profile digest differs.
    DigestMismatchManaged,
    /// The disabled profile digest differs.
    DigestMismatchDisabled,
    /// The managed shape is unsupported.
    ShapeUnsupportedManaged,
    /// The disabled shape is unsupported.
    ShapeUnsupportedDisabled,
    /// The managed shape is not narrower than its template.
    ShapeNotNarrowerManaged,
    /// The disabled shape is not narrower than its template.
    ShapeNotNarrowerDisabled,
    /// Git execution policy could not be built.
    GitPolicy,
    /// The Git discovery query was refused.
    QueryPolicy,
    /// The accepted Git binary lacked a required operation.
    GitUnsupported,
    /// The read binding differs from the active one.
    ReadBindingMismatch,
    /// The observed read profile was unavailable.
    ReadObservedStateUnavailable,
    /// The read sandbox cwd differs from its workspace.
    ReadSandboxCwdMismatch,
    /// The requested read path was not proven.
    ReadPathUnproven,
    /// Another fixed read-scope condition refused the operation.
    ReadRefused,
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
    /// Converts only exact closed log tags into agent-visible causes; capture suffixes are excluded.
    pub fn from_log_tag(tag: &str) -> Option<Self> {
        Some(match tag.split(';').next()?.trim() {
            "host_disabled" => Self::HostDisabled,
            "no_profile_for_class:managed" => Self::NoProfileManaged,
            "no_profile_for_class:disabled" => Self::NoProfileDisabled,
            "profile_digest_mismatch:managed" => Self::DigestMismatchManaged,
            "profile_digest_mismatch:disabled" => Self::DigestMismatchDisabled,
            "shape_unsupported:managed" => Self::ShapeUnsupportedManaged,
            "shape_unsupported:disabled" => Self::ShapeUnsupportedDisabled,
            "shape_not_narrower:managed" => Self::ShapeNotNarrowerManaged,
            "shape_not_narrower:disabled" => Self::ShapeNotNarrowerDisabled,
            "git_policy" => Self::GitPolicy,
            "query_policy" => Self::QueryPolicy,
            "git_unsupported" => Self::GitUnsupported,
            "read_scope:binding_mismatch" => Self::ReadBindingMismatch,
            "read_scope:observed_state_unavailable" => Self::ReadObservedStateUnavailable,
            "read_scope:sandbox_cwd_mismatch" => Self::ReadSandboxCwdMismatch,
            "read_scope:path_unproven" => Self::ReadPathUnproven,
            "read_scope:refused" => Self::ReadRefused,
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
            Self::HostDisabled => "host_disabled",
            Self::NoProfileManaged => "no_profile_for_class:managed",
            Self::NoProfileDisabled => "no_profile_for_class:disabled",
            Self::DigestMismatchManaged => "profile_digest_mismatch:managed",
            Self::DigestMismatchDisabled => "profile_digest_mismatch:disabled",
            Self::ShapeUnsupportedManaged => "shape_unsupported:managed",
            Self::ShapeUnsupportedDisabled => "shape_unsupported:disabled",
            Self::ShapeNotNarrowerManaged => "shape_not_narrower:managed",
            Self::ShapeNotNarrowerDisabled => "shape_not_narrower:disabled",
            Self::GitPolicy => "git_policy",
            Self::QueryPolicy => "query_policy",
            Self::GitUnsupported => "git_unsupported",
            Self::ReadBindingMismatch => "read_scope:binding_mismatch",
            Self::ReadObservedStateUnavailable => "read_scope:observed_state_unavailable",
            Self::ReadSandboxCwdMismatch => "read_scope:sandbox_cwd_mismatch",
            Self::ReadPathUnproven => "read_scope:path_unproven",
            Self::ReadRefused => "read_scope:refused",
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
    /// unready snapshots return [`Self::Unknown`].
    pub(crate) fn from_snapshot(
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
                    .map(|diagnostic| bounded_utf8_prefix(&diagnostic.message, 256))
                    .collect::<Vec<String>>();
                Self::CurrentReported {
                    delta: format!(
                        "Provider reported {} diagnostics for the exact post-edit source generation.",
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

/// Returns the largest UTF-8 prefix of `value` that fits `limit` bytes.
fn bounded_utf8_prefix(value: &str, limit: usize) -> String {
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
        /// Complete exact command the model must run before inspecting, for hosts that execute
        /// their own operation in a foreground helper.
        ///
        /// Absent for every daemon-executed operation, so the Codex envelope is byte-identical to
        /// its previous form. When present it is never trimmed: `PeerReply::shrink_text` refuses
        /// to shrink a pending reply, so an over-budget instruction fails closed instead of being
        /// silently cut into a command the launch recognizer could never match.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        helper: Option<String>,
    },
    /// Closed failure without provider, OS or host payloads.
    Error {
        /// Stable actionable failure category.
        code: FailureCode,
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
        /// A detail reference alone is not a continuation: retained Context and helper-composed
        /// Diff results may be inspectable but cannot yield new evidence. Missing from an older
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
    /// must be a nonempty block within [`crate::feed::MAX_BLOCK_BYTES`] and its reply must pass
    /// the same closed-reference validation as a bare one.
    pub(crate) fn decode_delivered(value: &str) -> Option<(Self, Option<String>)> {
        if let Ok(carried) = serde_json::from_str::<StatusCarriedReply>(value) {
            if value.len() > MAX_REPLY_BYTES {
                return None;
            }
            let valid = carried.reply.valid_reference()
                && !carried.status.is_empty()
                && carried.status.len() <= crate::feed::MAX_BLOCK_BYTES;
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
        if matches!(self, Self::Pending { helper: Some(helper), .. } if helper.is_empty() || helper.len() > MAX_HELPER_INSTRUCTION_BYTES || helper.chars().any(char::is_control))
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

    // A plate over the feed's block cap never decodes.
    let overlong = format!(
        "<agent-ide>\n{}\n</agent-ide>",
        "x".repeat(crate::feed::MAX_BLOCK_BYTES)
    );
    let encoded = PeerReply::encode_with_status(&reply, &overlong).unwrap();
    assert!(PeerReply::decode_delivered(encoded.as_str()).is_none());
}
