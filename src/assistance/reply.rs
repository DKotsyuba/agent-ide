//! Closed, serialized-size-bounded Assistance outcomes with no transport or authority secrets.

use crate::app::transport::OpaqueJson;
use serde::{Deserialize, Serialize};

/// Maximum complete Assistance JSON envelope including escaped text and all keys.
pub const MAX_REPLY_BYTES: usize = 64 * 1024;
/// Maximum model-visible feedback text returned to a native host hook.
pub const MAX_FEEDBACK_BYTES: usize = 4 * 1024;
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
    /// Idempotent operation was retried with different immutable parameters.
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
        let reference = match self {
            Self::Pending { detail_ref } => Some(detail_ref),
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
