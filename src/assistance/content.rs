//! Deterministic compact model content projected from validated Assistance replies.

use rmcp::model::{CallToolResult, ContentBlock};

use super::reply::{
    EditDiagnostics, FailureCode, MAX_REPLY_BYTES, MCP_RESERVE, MissingPeer, PeerReply, ResultKind,
};
use crate::changes::edit::{EditOutcome, EditResult};

/// Selects whether a projected [`CallToolResult`] also carries the duplicate typed
/// `structuredContent` copy alongside the compact `content` text block.
///
/// The Codex host reads only `structuredContent`, so its acceptance evidence needs
/// [`Self::WithStructured`]. The Claude host instead hands `structuredContent` straight to its
/// model in place of `content`, defeating the compact renderer (T14B); the managed Claude MCP
/// therefore projects [`Self::TextOnly`], so the model sees only the deterministic compact text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Envelope {
    /// Omits `structuredContent` entirely; the compact `content` text is the only carrier.
    TextOnly,
    /// Carries both the compact `content` text and the complete typed `structuredContent` copy.
    WithStructured,
}

/// Renders one validated reply and shrinks only owner text until the final MCP envelope fits.
///
/// The returned result contains exactly one text content block and, when `envelope` is
/// [`Envelope::WithStructured`], the complete serialized reply in `structured_content`. Only
/// [`PeerReply::Error`] sets `is_error`; invalid serialization or a non-shrinkable oversized result
/// returns `None` without partially emitting identifiers.
pub(crate) fn render(mut reply: PeerReply, envelope: Envelope) -> Option<CallToolResult> {
    loop {
        let rendered = project(&reply, envelope)?;
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
pub(crate) fn fits(reply: &PeerReply, envelope: Envelope) -> bool {
    project(reply, envelope).is_some_and(|rendered| call_tool_result_fits(&rendered))
}

/// Projects one unchanged reply into compact content plus, per `envelope`, the complete typed
/// structured value.
///
/// Serialization failure returns `None` regardless of `envelope`, so a value this renderer cannot
/// faithfully represent never silently drops its structured copy. The projection performs no I/O,
/// host inspection, diagnostics inference, or model call, and never places the serialized JSON in
/// `content`.
fn project(reply: &PeerReply, envelope: Envelope) -> Option<CallToolResult> {
    let structured = serde_json::to_value(reply).ok()?;
    let content = vec![ContentBlock::text(render_text(reply))];
    let mut rendered = if matches!(reply, PeerReply::Error { .. }) {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    if matches!(envelope, Envelope::WithStructured) {
        rendered.structured_content = Some(structured);
    }
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
             wrapping, or appended arguments:\n{helper}\nRun it as the only command of one Bash \
             call (no prefix such as \"date;\"). If ide.inspect answers pending again, the helper \
             has not been run yet, or it was run in a modified form and was refused; run the \
             command above exactly. After it completes, use ide.inspect with detail_ref \
             {detail_ref}; do not inspect before completion"
        ),
        PeerReply::Pending {
            detail_ref,
            helper: None,
        } => format!("pending: use ide.inspect with detail_ref {detail_ref}"),
        PeerReply::Error {
            code: FailureCode::ResolutionUnverified,
        } => "error: resolution_unverified; establish a supported configured project with exact document membership, then retry ide.context".to_owned(),
        PeerReply::Error {
            code: FailureCode::SourceTooLarge { size, ceiling },
        } => format!(
            "error: source_too_large; source is {size} bytes, exceeding the {ceiling} byte read \
             ceiling; continue with native tools"
        ),
        PeerReply::Error {
            code: FailureCode::InvalidDetail,
        } => "error: invalid_detail; this detail_ref is unknown or has expired (an un-run or \
              refused helper ticket expires unclaimed); repeat the original ide.* call to get a \
              fresh one, or continue with native tools"
            .to_owned(),
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
                FailureCode::ResolutionUnverified => unreachable!("handled above"),
                FailureCode::Cancelled => "cancelled",
                FailureCode::Deadline => "deadline",
                FailureCode::Capacity => "capacity",
                FailureCode::InvalidDetail => unreachable!("handled above"),
                FailureCode::SourceUnavailable => "source_unavailable",
                FailureCode::SourceTooLarge { .. } => unreachable!("handled above"),
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
            continuation: true,
        } => format!(
            "complete context: {text}\nOutput is truncated; use ide.inspect with detail_ref \
             {detail_ref} before editing"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            detail_ref: Some(detail_ref),
            truncated: true,
            ..
        } => format!(
            "complete context: {text}\nOutput is incomplete; use ide.edit with source_ref \
             {detail_ref} when available, otherwise use the native editor"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            truncated: true,
            ..
        } => format!(
            "complete context: {text}\nOutput is incomplete; use ide.edit when available, \
             otherwise use the native editor"
        ),
        PeerReply::Complete {
            kind: ResultKind::Context,
            text,
            detail_ref: Some(detail_ref),
            ..
        } => format!(
            "complete context: {text}\nDiagnostics are exactly as reported; use ide.edit with \
             source_ref {detail_ref} when available, otherwise use the native editor"
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
            continuation: true,
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
            "complete diff: {text}\nOutput is incomplete; stop or review the available hunks \
             safely with native tools"
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
        PeerReply::Edit {
            result,
            diagnostics,
        } => render_edit(result, diagnostics),
    }
}

/// Renders one validated edit outcome and its exact-generation diagnostic projection.
///
/// The compact text retains the public path, successful post-read source reference, reported
/// messages, and exactly one safe next action. Operation identifiers remain only in structured
/// content because they are not needed for that decision. This function performs no inference:
/// unknown diagnostics remain unknown, and only a validated pending detail recommends inspection.
fn render_edit(result: &EditResult, diagnostics: &EditDiagnostics) -> String {
    let outcome = result.outcome.as_str();
    match result.outcome {
        EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged => {
            let source_ref = result.source_ref.as_deref().unwrap_or("unavailable");
            match diagnostics {
                EditDiagnostics::CurrentReported {
                    messages,
                    delta,
                    truncated,
                } => format!(
                    "edit: {outcome}; path {}; source_ref {source_ref}; diagnostics: \
                     current_reported ({delta}){}\n{}\nNext: use ide.edit with source_ref \
                     {source_ref}",
                    result.path,
                    if *truncated { " [truncated]" } else { "" },
                    messages.join("\n")
                ),
                EditDiagnostics::CurrentClean {} => format!(
                    "edit: {outcome}; path {}; source_ref {source_ref}; diagnostics: \
                     current_clean. Next: use ide.diff",
                    result.path
                ),
                EditDiagnostics::Unknown {} => format!(
                    "edit: {outcome}; path {}; source_ref {source_ref}; diagnostics: unknown. \
                     Next: use ide.context",
                    result.path
                ),
                EditDiagnostics::Pending { detail_ref } => format!(
                    "edit: {outcome}; path {}; source_ref {source_ref}; diagnostics: pending. \
                     Next: use ide.inspect with detail_ref {detail_ref}",
                    result.path
                ),
            }
        }
        EditOutcome::StaleSource => format!(
            "edit: {outcome}; path {}. No write occurred; use ide.context before another edit",
            result.path
        ),
        EditOutcome::ConflictingDuplicate => format!(
            "edit: {outcome}; path {}. No write occurred; use ide.context to inspect the target \
             before a new operation_id",
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
            "edit: {outcome}; path {}. Inspect this target with native tools before any later \
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
        }
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
            let rendered = render(reply, Envelope::WithStructured).unwrap();
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
        let rendered = render(
            PeerReply::Pending {
                detail_ref: detail_ref.into(),
                helper: Some(helper.into()),
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains(helper) && text.contains(detail_ref));
        assert!(text.find(helper).unwrap() < text.find("ide.inspect").unwrap());
    }

    /// A pending helper instruction says the command must stand alone and how an un-run ticket
    /// looks, and an expired or unknown detail says to repeat the original call.
    #[test]
    fn pending_helper_and_expired_detail_explain_the_recovery() {
        let pending = render(
            PeerReply::Pending {
                detail_ref: "detail-1".into(),
                helper: Some("agent-ide claude-worker --detail-ref detail-1".into()),
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&pending);
        assert!(
            text.contains("agent-ide claude-worker --detail-ref detail-1"),
            "{text}"
        );
        assert!(text.contains("only command of one Bash call"), "{text}");
        assert!(text.contains("has not been run yet"), "{text}");
        assert!(text.contains("refused"), "{text}");

        let expired = render(
            PeerReply::Error {
                code: FailureCode::InvalidDetail,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&expired);
        assert!(text.starts_with("error: invalid_detail;"), "{text}");
        assert!(text.contains("repeat the original ide.* call"), "{text}");
        assert_eq!(expired.is_error, Some(true));
    }

    /// Keeps unresolved TypeScript configuration actionable without claiming a native substitute.
    #[test]
    fn resolution_unverified_is_closed_without_native_path_overclaim() {
        let reply = PeerReply::Error {
            code: FailureCode::ResolutionUnverified,
        };
        let expected = serde_json::to_value(&reply).unwrap();
        let rendered = render(reply, Envelope::WithStructured).unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("resolution_unverified") && text.contains("ide.context"));
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
            };
            let expected = serde_json::to_value(&reply).unwrap();
            let rendered = render(reply, Envelope::WithStructured).unwrap();
            let text = text_of(&rendered);
            assert!(text.starts_with(&format!("edit: {}", outcome.as_str())));
            assert!(text.contains("src/lib.rs"));
            assert!(!text.contains("private-operation-id"));
            if outcome != EditOutcome::ConflictingDuplicate {
                assert!(!text.contains("operation_id"));
            }
            assert_eq!(rendered.structured_content, Some(expected));
            assert_eq!(rendered.is_error, None);
            if outcome == EditOutcome::OutcomeUnknown {
                assert!(text.contains("do not replay") && text.contains("native tools"));
            }
        }
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
                "ide.edit",
            ),
            (
                EditDiagnostics::CurrentClean {},
                "current_clean",
                "ide.diff",
            ),
            (EditDiagnostics::Unknown {}, "unknown", "ide.context"),
            (
                EditDiagnostics::Pending {
                    detail_ref: "diagnostic-detail".into(),
                },
                "pending",
                "ide.inspect",
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

    /// Closes the T14B gap where a Claude reader, which never sees `structuredContent`, would have
    /// no way to learn the `source_ref` a later `ide.edit` needs: unlike Diff, Context's
    /// `detail_ref` names the exact same-binding source an `ide.edit` `source_ref` must match, so
    /// it belongs in the compact text whenever it is `Some`, truncated or not, continuation or not.
    #[test]
    fn context_text_always_carries_its_edit_source_ref() {
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
                text.contains("source_ref live-source-detail") && text.contains("ide.edit"),
                "{text}"
            );
        }
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
            text_of(&context).contains("incomplete") && !text_of(&context).contains("ide.inspect")
        );
        assert!(
            text_of(&diff).contains("stop or review") && !text_of(&diff).contains("ide.inspect")
        );
        assert!(text_of(&paged).contains("ide.inspect with detail_ref page-detail"));
    }

    /// Sends duplicate-edit recovery through the IDE context path with a fresh operation identity.
    #[test]
    fn conflicting_duplicate_requests_context_and_a_new_operation_id() {
        let rendered = render(
            PeerReply::Edit {
                result: edit_result(EditOutcome::ConflictingDuplicate).unwrap(),
                diagnostics: EditDiagnostics::Unknown {},
            },
            Envelope::WithStructured,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("use ide.context") && text.contains("new operation_id"));
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
}
