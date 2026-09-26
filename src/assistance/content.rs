//! Deterministic compact model content projected from validated Assistance replies.

use std::sync::OnceLock;

use minijinja::{AutoEscape, Environment, UndefinedBehavior};
use rmcp::model::{CallToolResult, ContentBlock};

use super::reply::{FailureCode, MAX_REPLY_BYTES, MCP_RESERVE, PeerReply};

/// Build-embedded MiniJinja source projecting every closed [`PeerReply`] state into its compact
/// model-facing text; the template owns the presentation so Rust code never formats reply text.
const REPLY_TEMPLATE: &str = include_str!("../../assets/mcp/reply.jinja");

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
/// [`PeerReply::Error`] and [`PeerReply::InvalidParameters`] set `is_error`; invalid serialization or a non-shrinkable oversized result
/// returns `None` without partially emitting identifiers.
///
/// `status` is the optional due status plate (T28B): the complete `<agent-ide>` plate text, which
/// prefixes the compact content text, separated by one newline, and is the leading `status` string
/// field of the structured copy. The plate itself is never shrunk or cut — only owner text is — so
/// a fitted result always carries it whole.
pub(crate) fn render_with_status(
    mut reply: PeerReply,
    status: Option<&str>,
    envelope: Envelope,
) -> Option<CallToolResult> {
    loop {
        let rendered = project(&reply, status, envelope)?;
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
    project(reply, None, envelope).is_some_and(|rendered| call_tool_result_fits(&rendered))
}

/// Reports whether one unchanged reply fits the final MCP carrier with `status` attached whole.
///
/// The daemon-side twin of [`render_with_status`]'s fitting: a host without hook delivery
/// attaches a plate only after this accepts it, so the facade's own render can never be the
/// call that cuts it.
pub(crate) fn fits_with_status(reply: &PeerReply, status: &str, envelope: Envelope) -> bool {
    project(reply, Some(status), envelope).is_some_and(|rendered| call_tool_result_fits(&rendered))
}

/// Projects one unchanged reply into compact content plus, per `envelope`, the complete typed
/// structured value.
///
/// Serialization or template failure returns `None` regardless of `envelope`, so a value this
/// renderer cannot faithfully represent never silently drops its structured copy. A tagged
/// execution-profile refusal keeps the public structured `code` string stable; the closed cause
/// tag is passed to the template only, so it appears solely in compact text; likewise an error's
/// `detail` reaches the template as `resolution_detail` and is removed from the structured copy.
/// Projection performs no I/O, host inspection, diagnostics inference, or model call.
fn project(reply: &PeerReply, status: Option<&str>, envelope: Envelope) -> Option<CallToolResult> {
    let mut structured = serde_json::to_value(reply).ok()?;
    let cause_tag = match reply {
        PeerReply::Error {
            code: FailureCode::ExecutionProfileCause(cause),
            ..
        } => Some(cause.tag()),
        _ => None,
    };
    let mut context = structured.clone();
    if let PeerReply::Error { detail, .. } = reply {
        context["resolution_detail"] =
            serde_json::Value::String(detail.clone().unwrap_or_default());
        if let Some(fields) = structured.as_object_mut() {
            fields.remove("detail");
        }
    }
    if let Some(tag) = cause_tag {
        let public = serde_json::Value::String("execution_profile".to_owned());
        structured["code"] = public.clone();
        context["code"] = public;
        context["cause_tag"] = serde_json::Value::String(tag.to_owned());
    }
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
    let mut rendered = if matches!(
        reply,
        PeerReply::Error { .. } | PeerReply::InvalidParameters { .. }
    ) {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    if matches!(envelope, Envelope::WithStructured) {
        rendered.structured_content = Some(structured);
    }
    rendered.is_error = matches!(
        reply,
        PeerReply::Error { .. } | PeerReply::InvalidParameters { .. }
    )
    .then_some(true);
    Some(rendered)
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
    ENVIRONMENT.get_or_init(|| {
        let mut environment = Environment::new();
        environment.set_undefined_behavior(UndefinedBehavior::Strict);
        environment.set_auto_escape_callback(|_| AutoEscape::None);
        environment
            .add_template("reply.jinja", REPLY_TEMPLATE)
            .expect("embedded reply template parses");
        environment
    })
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

/// Returns whether the serialized final MCP carrier stays below the Assistance reply ceiling.
pub(crate) fn call_tool_result_fits(rendered: &CallToolResult) -> bool {
    serde_json::to_vec(rendered).is_ok_and(|bytes| bytes.len() <= MAX_REPLY_BYTES - MCP_RESERVE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistance::reply::{EditDiagnostics, MissingPeer, ResultKind};
    use crate::changes::edit::{EditOutcome, EditReceiptError, EditResult};

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
        }
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
                matches!(
                    expected["state"].as_str(),
                    Some("error" | "invalid_parameters")
                )
                .then_some(true)
            );
        }
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
        assert!(text.starts_with("error: invalid_detail;"), "{text}");
        assert!(text.contains("repeat the original ide.* call"), "{text}");
        assert_eq!(expired.is_error, Some(true));
    }

    /// A closed profile cause appears after the stable code; capture suffixes and unknown tags
    /// cannot be admitted into agent-facing text.
    #[test]
    fn execution_profile_cause_is_closed_and_actionable() {
        use crate::assistance::reply::ExecutionProfileCause;
        let cause = ExecutionProfileCause::from_log_tag("spawn:io; captured:deadbeef")
            .expect("known cause");
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
            "error: execution_profile (spawn:io); continue with native tools"
        );
        assert_eq!(
            rendered.structured_content.unwrap()["code"],
            "execution_profile"
        );
        assert!(ExecutionProfileCause::from_log_tag("spawn:io:/private/path").is_none());
    }

    /// Keeps unresolved TypeScript configuration actionable without claiming a native substitute.
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
        assert!(text.contains("resolution_unverified") && text.contains("ide.context"));
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

    /// Names both content changes and unusable references without claiming which one occurred.
    #[test]
    fn stale_edit_text_explains_why_no_write_occurred() {
        let reply = PeerReply::Edit {
            result: edit_result(EditOutcome::StaleSource).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
        };
        let rendered = render(reply, Envelope::TextOnly).unwrap();
        let text = text_of(&rendered);
        assert!(text.contains("No write occurred"));
        assert!(text.contains("content/presence changed"));
        assert!(text.contains("newer observation alone does not invalidate"));
        assert!(!text.contains("source_ref "));
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

    /// A problems Context keeps its v0.2 text without advertising an edit reference.
    #[test]
    fn context_without_source_keeps_bare_content() {
        let rendered = render(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "python: unavailable:env_missing".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Envelope::TextOnly,
        )
        .unwrap();
        let text = text_of(&rendered);
        assert_eq!(text, "complete context: python: unavailable:env_missing");
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
            "complete context: page 1; bytes 0-2 of 4\n\nx\n\nOutput is truncated; use ide.inspect with detail_ref next-page before editing"
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
            "complete context: x\nOutput is truncated; use ide.inspect with detail_ref next-page before editing"
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

    /// A carried status plate (T28B) leads both carriers: one newline after the plate, the
    /// compact reply text follows, and the structured copy names the plate as its leading
    /// `status` string field.
    #[test]
    fn carried_status_leads_both_carriers() {
        let plate = "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>";
        let rendered = render_with_status(
            PeerReply::Complete {
                kind: ResultKind::Context,
                text: "rust: ready; errors: 2; warnings: 0".into(),
                detail_ref: None,
                truncated: false,
                continuation: false,
            },
            Some(plate),
            Envelope::WithStructured,
        )
        .unwrap();
        assert!(
            text_of(&rendered).starts_with(&format!("{plate}\ncomplete context: rust: ready")),
            "{}",
            text_of(&rendered)
        );
        let structured = rendered.structured_content.unwrap();
        let fields = structured.as_object().unwrap();
        // `status` is exposed on the structured copy; serde_json's map is order-free, so only
        // presence and verbatim content are asserted, not key position.
        assert_eq!(fields["status"], plate, "{fields:?}");
        assert_eq!(structured["state"], "complete");
        assert_eq!(structured["text"], "rust: ready; errors: 2; warnings: 0");
    }

    /// The plate is never a shrink candidate: a reply that only fits with the plate after owner
    /// text was cut still carries the plate whole, and `fits_with_status` accepts exactly the
    /// forms the facade's own render loop accepts.
    #[test]
    fn fitting_cuts_owner_text_never_the_plate() {
        let plate = "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>";
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
            text_of(&rendered).starts_with(&format!("{plate}\ncomplete context:")),
            "{}",
            text_of(&rendered)
        );
        let structured = rendered.structured_content.unwrap();
        assert_eq!(structured["status"], plate);
        assert_eq!(structured["truncated"], true);
    }
}
