//! Closed, serialized-size-bounded Assistance outcomes with no transport or authority secrets.

use crate::app::transport::OpaqueJson;
use rmcp::model::{CallToolResult, ContentBlock};
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
const MCP_RESERVE: usize = 1024;

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
    /// Configured Git lacks the required filter-free discovery/snapshot flags.
    UnsupportedGit,
    /// Workspace activation is unavailable or refused.
    WorkspaceActivation,
    /// Current durable authority was revoked, replaced or fenced by a new boot.
    WorkspaceAuthority,
    /// The accepted language provider could not supply the requested service.
    ProviderUnavailable,
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
    /// A single-owner resource is already held: an idempotent operation retried with different
    /// immutable parameters, or another live actor that currently owns this worktree incarnation
    /// or its provider cache namespace. The refusal never disturbs the actor that already owns it,
    /// and a handoff becomes possible after that owner stops.
    Conflict,
    /// Unexpected internal failure has no safe owner result.
    Internal,
}

/// Owner evidence category for a completed result; not an extensible tool identifier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultKind {
    /// Durable Workspace activated and authorized the exact binding.
    Activation,
    /// Intelligence returned context over current registered source bytes.
    Context,
    /// Changes composed evidence for a current Workspace comparison scope.
    Diff,
    /// Durable authority was revoked and cleanup reached its reported outcome.
    Stop,
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
        /// Same-binding retained detail reference, absent if no retained result exists.
        detail_ref: Option<String>,
        /// True when serialized-result budgeting omitted owner text.
        truncated: bool,
    },
    /// Exact Changes-owned one-file edit result with no source content or diagnostics.
    Edit {
        /// Durable closed outcome, operation/path correlation, and optional post-read source ref.
        result: crate::changes::edit::EditResult,
    },
}
impl std::fmt::Debug for PeerReply {
    /// Omits text and detail handles from diagnostics.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PeerReply(..)")
    }
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
    /// Checks reference syntax; ownership and current liveness remain worker admission gates.
    fn valid_reference(&self) -> bool {
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

/// Builds the exact complete MCP tool result for one reply without ever shrinking its text.
///
/// This is the sole envelope constructor shared by transport rendering
/// (`facade::render_reply`) and whole-hunk page fitting
/// (`worker::snapshots::fit_diff_page`), so both measure the same bytes that are
/// actually sent to the MCP host: [`CallToolResult::structured`]/[`CallToolResult::structured_error`]
/// duplicate `reply`'s JSON into both `content[0].text` and `structured_content`, and this prepends
/// the summary line exactly as the real response does. A page proven to fit by
/// [`call_tool_result_fits`] on the result of this function is therefore never cut mid-hunk by a
/// later, independently computed reserve.
///
/// The summary is deterministic, drawn only from `reply`'s own closed shape (no LLM, state or
/// telemetry), and gives the model its next bounded step: a foreground-helper [`PeerReply::Pending`]
/// carries the exact command verbatim followed by the required `ide.inspect` order; a queued
/// [`PeerReply::Pending`] names the exact `detail_ref` to inspect; a [`PeerReply::Complete`] names
/// the next tool by [`ResultKind`] (`ide.context` before editing after Activation or a fresh
/// Context, `ide.inspect` for a truncated Context/Diff, `ide.stop` after a reviewed Diff, and that
/// native edits remain on disk after Stop). A foreground-helper command is never trimmed: that
/// command is counted here so an instruction too large for the envelope fails closed rather than
/// reaching the host truncated.
pub(crate) fn render_call_tool_result(reply: &PeerReply) -> Option<CallToolResult> {
    let value = serde_json::to_value(reply).ok()?;
    let summary = match reply {
        // The complete command is rendered verbatim in the summary the model actually reads.
        // Nothing below can trim it: `shrink_text` refuses pending replies, so an envelope that
        // cannot hold the exact command becomes an explicit error instead of a cut command.
        PeerReply::Pending {
            detail_ref,
            helper: Some(helper),
        } => format!(
            "Assistance work is pending and this host runs it in a foreground helper. \
             Run exactly this command with Bash, in the foreground (run_in_background must be \
             false), without editing, wrapping or appending to it:\n{helper}\n\
             Then use ide.inspect with detail_ref {detail_ref}. \
             Do not call ide.inspect before that command has completed.",
        ),
        PeerReply::Pending {
            detail_ref,
            helper: None,
        } => format!("Assistance work is pending; use ide.inspect with detail_ref {detail_ref}."),
        PeerReply::Error { .. } => {
            "Assistance could not complete this operation; inspect the typed error and continue with native tools".to_owned()
        }
        PeerReply::Complete {
            kind: ResultKind::Activation,
            ..
        } => "Workspace is active; call ide.context before editing source with native host tools"
            .to_owned(),
        PeerReply::Complete {
            kind: ResultKind::Context,
            detail_ref: Some(detail_ref),
            truncated: true,
            ..
        } => format!(
            "Context is truncated; use ide.inspect with detail_ref {detail_ref} for the rest before editing"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            ..
        } => {
            "Edit with native host tools, then call ide.context again to refresh".to_owned()
        }
        PeerReply::Complete {
            kind: ResultKind::Diff,
            detail_ref: Some(detail_ref),
            truncated: true,
            ..
        } => format!(
            "Diff is truncated; use ide.inspect with detail_ref {detail_ref} for the remaining hunks, then call ide.stop when finished"
        ),
        PeerReply::Complete {
            kind: ResultKind::Diff,
            ..
        } => "Review this diff, then call ide.stop when finished".to_owned(),
        PeerReply::Complete {
            kind: ResultKind::Stop,
            ..
        } => "Workspace authority is stopped; files already edited by native host tools remain on disk"
            .to_owned(),
        PeerReply::Edit { result }
            if result.outcome == crate::changes::edit::EditOutcome::OutcomeUnknown =>
        {
            format!(
                "Edit outcome is unknown for {}; inspect that target before any later mutation and do not replay this operation",
                result.path
            )
        }
        PeerReply::Edit { .. } => {
            "Changes returned a closed single-file edit outcome; native editing remains available"
                .to_owned()
        }
        _ => "Assistance returned the current owner result".to_owned(),
    };
    let mut rendered = if matches!(reply, PeerReply::Error { .. }) {
        CallToolResult::structured_error(value)
    } else {
        CallToolResult::structured(value)
    };
    rendered.content.insert(0, ContentBlock::text(summary));
    Some(rendered)
}

/// True when `rendered`'s exact serialized bytes fit the bounded MCP reply budget.
///
/// This is the exact predicate every accepted page and every final rendered reply must satisfy;
/// callers must not substitute an approximate reserve computed over a narrower value (such as the
/// unduplicated [`PeerReply`] alone), because that undercounts the real envelope and can accept a
/// page that `facade::render_reply` then has to shrink.
pub(crate) fn call_tool_result_fits(rendered: &CallToolResult) -> bool {
    serde_json::to_vec(rendered).is_ok_and(|bytes| bytes.len() <= MAX_REPLY_BYTES - MCP_RESERVE)
}

/// Includes escaping/framing costs, preserves Unicode boundaries, and rejects unknown envelope fields.
#[test]
fn envelopes_are_closed_and_fit_serialized_budget() {
    let reply = PeerReply::Complete {
        kind: ResultKind::Context,
        text: "\0🦀\"\\".repeat(16000),
        detail_ref: Some("detail-1".into()),
        truncated: false,
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

/// Each summary names the exact next-step tool for its `ResultKind`/`Pending` shape, without
/// pinning the full sentence, so the guidance can be reworded freely as long as the named tool
/// stays correct.
#[test]
fn summary_names_the_next_step_tool_for_each_reply_shape() {
    fn summary(reply: PeerReply) -> String {
        let rendered = render_call_tool_result(&reply).unwrap();
        let ContentBlock::Text(text) = &rendered.content[0] else {
            panic!("summary is a text block");
        };
        text.text.clone()
    }
    let queued = summary(PeerReply::Pending {
        detail_ref: "detail-1".into(),
        helper: None,
    });
    assert!(queued.contains("ide.inspect") && queued.contains("detail-1"));
    let helper = summary(PeerReply::Pending {
        detail_ref: "detail-2".into(),
        helper: Some("run-me".into()),
    });
    assert!(helper.contains("run-me") && helper.contains("ide.inspect"));
    let activation = summary(PeerReply::Complete {
        kind: ResultKind::Activation,
        text: String::new(),
        detail_ref: Some("detail-3".into()),
        truncated: false,
    });
    assert!(activation.contains("ide.context"));
    let truncated_context = summary(PeerReply::Complete {
        kind: ResultKind::Context,
        text: String::new(),
        detail_ref: Some("detail-4".into()),
        truncated: true,
    });
    assert!(truncated_context.contains("ide.inspect") && truncated_context.contains("detail-4"));
    let fresh_context = summary(PeerReply::Complete {
        kind: ResultKind::Context,
        text: String::new(),
        detail_ref: Some("detail-5".into()),
        truncated: false,
    });
    assert!(!fresh_context.contains("ide.inspect"));
    let diff = summary(PeerReply::Complete {
        kind: ResultKind::Diff,
        text: String::new(),
        detail_ref: Some("detail-6".into()),
        truncated: false,
    });
    assert!(diff.contains("ide.stop"));
    let stop = summary(PeerReply::Complete {
        kind: ResultKind::Stop,
        text: String::new(),
        detail_ref: None,
        truncated: false,
    });
    assert!(stop.contains("remain on disk"));
}
