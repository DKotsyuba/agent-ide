//! Deterministic compact model content projected from validated Assistance replies.

use rmcp::model::{CallToolResult, ContentBlock};

use super::reply::{FailureCode, MAX_REPLY_BYTES, MCP_RESERVE, MissingPeer, PeerReply, ResultKind};
use crate::changes::edit::{EditOutcome, EditResult};

/// Renders one validated reply and shrinks only owner text until the final MCP envelope fits.
///
/// The returned result contains exactly one text content block and the complete serialized reply
/// in `structured_content`. Only [`PeerReply::Error`] sets `is_error`; invalid serialization or a
/// non-shrinkable oversized result returns `None` without partially emitting identifiers.
pub(crate) fn render(mut reply: PeerReply) -> Option<CallToolResult> {
    loop {
        let rendered = project(&reply)?;
        if call_tool_result_fits(&rendered) {
            return Some(rendered);
        }
        if !reply.shrink_text() {
            return None;
        }
    }
}

/// Reports whether one unchanged reply fits after projection into the exact final MCP carrier.
///
/// This predicate never shrinks text. Diff pagination uses it to accept only whole-hunk pages that
/// the facade can later render byte-for-byte without advancing a cursor past omitted content.
pub(crate) fn fits(reply: &PeerReply) -> bool {
    project(reply).is_some_and(|rendered| call_tool_result_fits(&rendered))
}

/// Projects one unchanged reply into compact content plus the complete typed structured value.
///
/// Serialization failure returns `None`. The projection performs no I/O, host inspection,
/// diagnostics inference, or model call, and never places the serialized JSON in `content`.
fn project(reply: &PeerReply) -> Option<CallToolResult> {
    let structured = serde_json::to_value(reply).ok()?;
    let content = vec![ContentBlock::text(render_text(reply))];
    let mut rendered = if matches!(reply, PeerReply::Error { .. }) {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    rendered.structured_content = Some(structured);
    rendered.is_error = matches!(reply, PeerReply::Error { .. }).then_some(true);
    Some(rendered)
}

/// Returns deterministic decision-facing text for one validated reply without serializing it.
///
/// The text begins with the closed reply state, preserves exact pending commands and live detail
/// references, and names no host metadata, telemetry, provider errors, or inferred diagnostics.
fn render_text(reply: &PeerReply) -> String {
    match reply {
        PeerReply::Unavailable { reason } => format!(
            "unavailable: {}; continue with native tools",
            match reason {
                MissingPeer::HostBinding => "host_binding",
                MissingPeer::WorkspaceActivation => "workspace_activation",
            }
        ),
        PeerReply::HookObserved {} => "hook_observed: native pre-hook retained".to_owned(),
        PeerReply::HookSettled {} => "hook_settled: validated invocation settled".to_owned(),
        PeerReply::NativeHookObserved {} => {
            "native_hook_observed: registered-path reconciliation requested".to_owned()
        }
        PeerReply::Feedback { text } => format!("feedback: {text}"),
        PeerReply::HostStopped {} => {
            "host_stopped: host binding released; no workspace authority was created".to_owned()
        }
        PeerReply::Pending {
            detail_ref,
            helper: Some(helper),
        } => format!(
            "pending: run exactly this command with Bash in the foreground, with no editing, \
             wrapping, or appended arguments:\n{helper}\nAfter it completes, use ide.inspect with \
             detail_ref {detail_ref}; do not inspect before completion"
        ),
        PeerReply::Pending {
            detail_ref,
            helper: None,
        } => format!("pending: use ide.inspect with detail_ref {detail_ref}"),
        PeerReply::Error { code } => format!(
            "error: {}; continue with native tools",
            match code {
                FailureCode::LauncherConfiguration => "launcher_configuration",
                FailureCode::SandboxState => "sandbox_state",
                FailureCode::ExecutionProfile => "execution_profile",
                FailureCode::UnsupportedGit => "unsupported_git",
                FailureCode::WorkspaceActivation => "workspace_activation",
                FailureCode::WorkspaceAuthority => "workspace_authority",
                FailureCode::ProviderUnavailable => "provider_unavailable",
                FailureCode::Cancelled => "cancelled",
                FailureCode::Deadline => "deadline",
                FailureCode::Capacity => "capacity",
                FailureCode::InvalidDetail => "invalid_detail",
                FailureCode::SourceUnavailable => "source_unavailable",
                FailureCode::Conflict => "conflict",
                FailureCode::Internal => "internal",
            }
        ),
        PeerReply::Complete {
            kind: ResultKind::Activation,
            text,
            ..
        } => format!("complete activation: {text}\nNext: use ide.context"),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            detail_ref: Some(detail_ref),
            truncated: true,
        } => format!(
            "complete context: {text}\nOutput is truncated; use ide.inspect with detail_ref \
             {detail_ref} before editing"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            truncated: true,
            ..
        } => format!(
            "complete context: {text}\nOutput is truncated without a detail reference; continue \
             with native tools"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            ..
        } => format!(
            "complete context: {text}\nDiagnostics are exactly as reported; use ide.edit when \
             available, otherwise use the native editor"
        ),
        PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            detail_ref: Some(detail_ref),
            truncated: true,
        } => format!(
            "complete diff: {text}\nOutput is truncated; use ide.inspect with detail_ref \
             {detail_ref}"
        ),
        PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            truncated: true,
            ..
        } => format!(
            "complete diff: {text}\nOutput is truncated without a detail reference; continue \
             with native tools"
        ),
        PeerReply::Complete {
            kind: ResultKind::Diff,
            text,
            ..
        } => format!("complete diff: {text}\nNext: use ide.stop"),
        PeerReply::Complete {
            kind: ResultKind::Stop,
            text,
            ..
        } => format!(
            "complete stop: {text}\nWorkspace authority is released; native edits remain on disk"
        ),
        PeerReply::Edit { result } => render_edit(result),
    }
}

/// Renders a validated Changes result without inventing diagnostics absent from its typed schema.
///
/// The public path and a successful post-read source reference are retained. Operation identifiers
/// remain in structured content because they are not needed for the model's next safe decision.
fn render_edit(result: &EditResult) -> String {
    let outcome = result.outcome.as_str();
    match result.outcome {
        EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged => format!(
            "edit: {outcome}; path {}; source_ref {}. Diagnostics were not reported; use \
             ide.context",
            result.path,
            result.source_ref.as_deref().unwrap_or("unavailable")
        ),
        EditOutcome::StaleSource => format!(
            "edit: {outcome}; path {}. No write occurred; use ide.context before another edit",
            result.path
        ),
        EditOutcome::ConflictingDuplicate => format!(
            "edit: {outcome}; path {}. No write occurred; use native tools to inspect the target \
             before a new operation",
            result.path
        ),
        EditOutcome::UnsafeTarget => format!(
            "edit: {outcome}; path {}. No write occurred; continue with native tools",
            result.path
        ),
        EditOutcome::CancelledNoEffect
        | EditOutcome::DeadlineNoEffect
        | EditOutcome::CapacityNoEffect => {
            format!("edit: {outcome}; path {}. No write occurred", result.path)
        }
        EditOutcome::OutcomeUnknown => format!(
            "edit: {outcome}; path {}. Use ide.context to inspect this target before any later \
             mutation; do not replay this operation",
            result.path
        ),
        EditOutcome::UnavailableBeforeDispatch => format!(
            "edit: {outcome}; path {}. No write was dispatched; continue with native tools",
            result.path
        ),
    }
}

/// Returns whether the serialized final MCP carrier stays below the Assistance reply ceiling.
pub(crate) fn call_tool_result_fits(rendered: &CallToolResult) -> bool {
    serde_json::to_vec(rendered).is_ok_and(|bytes| bytes.len() <= MAX_REPLY_BYTES - MCP_RESERVE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::edit::EditReceiptError;

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

    /// Covers every non-edit state and result kind with one compact structured projection.
    #[test]
    fn every_reply_state_has_one_compact_text_block_and_equal_structured_content() {
        let replies = vec![
            PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
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
                helper: None,
            },
            PeerReply::Error {
                code: FailureCode::Capacity,
            },
            PeerReply::Complete {
                kind: ResultKind::Activation,
                text: "active".into(),
                detail_ref: None,
                truncated: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "context evidence".into(),
                detail_ref: None,
                truncated: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Diff,
                text: "diff evidence".into(),
                detail_ref: None,
                truncated: false,
            },
            PeerReply::Complete {
                kind: ResultKind::Stop,
                text: "stopped".into(),
                detail_ref: None,
                truncated: false,
            },
        ];
        for reply in replies {
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply).unwrap();
            let text = text_of(&rendered);
            assert!(!text.starts_with('{') && !text.contains("\"state\""));
            for field in ["reason", "code"] {
                if let Some(value) = expected[field].as_str() {
                    assert!(text.contains(value));
                }
            }
            assert_eq!(rendered.structured_content, Some(expected.clone()));
            assert_eq!(
                rendered.is_error,
                (expected["state"] == "error").then_some(true)
            );
        }
    }

    /// Preserves the helper and detail reference exactly while retaining foreground ordering.
    #[test]
    fn pending_helper_and_reference_are_exact() {
        let helper = "helper --argument='🦀 value'";
        let detail_ref = "exact-detail-reference";
        let rendered = render(PeerReply::Pending {
            detail_ref: detail_ref.into(),
            helper: Some(helper.into()),
        })
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains(helper) && text.contains(detail_ref));
        assert!(text.find(helper).unwrap() < text.find("ide.inspect").unwrap());
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
            };
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply).unwrap();
            let text = text_of(&rendered);
            assert!(text.starts_with(&format!("edit: {}", outcome.as_str())));
            assert!(text.contains("src/lib.rs"));
            assert!(!text.contains("private-operation-id") && !text.contains("operation_id"));
            assert_eq!(rendered.structured_content, Some(expected));
            assert_eq!(rendered.is_error, None);
            if outcome == EditOutcome::OutcomeUnknown {
                assert!(text.contains("do not replay") && text.contains("ide.context"));
            }
        }
    }

    /// Keeps exact truncated references in model text and omits unrelated structured field names.
    #[test]
    fn compact_text_excludes_json_duplication_and_unneeded_fields() {
        let rendered = render(PeerReply::Complete {
            kind: ResultKind::Context,
            text: "bounded owner evidence".into(),
            detail_ref: Some("private-live-detail".into()),
            truncated: true,
        })
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("private-live-detail") && text.contains("bounded owner evidence"));
        for excluded in ["\"state\"", "\"kind\"", "structured_content", "is_error"] {
            assert!(!text.contains(excluded));
        }
    }

    /// Shrinks only complete owner text on UTF-8 boundaries and measures the final MCP bytes.
    #[test]
    fn utf8_shrinking_fits_the_final_call_tool_result() {
        let rendered = render(PeerReply::Complete {
            kind: ResultKind::Context,
            text: "\0🦀\"\\".repeat(16_000),
            detail_ref: Some("same-binding-detail".into()),
            truncated: false,
        })
        .unwrap();
        assert!(call_tool_result_fits(&rendered));
        let structured = rendered.structured_content.as_ref().unwrap();
        assert_eq!(structured["truncated"], true);
        assert!(structured["text"].as_str().unwrap().contains('🦀'));
        assert!(text_of(&rendered).contains('🦀'));
    }
}
