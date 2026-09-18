//! One-way sanitizers from existing product facts into closed telemetry events.
//!
//! These adapters deliberately accept no model parameters, hook payloads, provider identities,
//! cache keys, diagnostics, process output, paths, commands, or error text. Their only side effect
//! is [`Telemetry::record`], which is synchronous, bounded, and fail-open.

use std::time::Duration;

use crate::{
    assistance::{
        facade::{AssistanceTool, HookIngressOutcome},
        reply::{EditDiagnostics, FailureCode, MissingPeer, PeerReply},
    },
    changes::edit::EditOutcome,
    checks::{self, CheckState, ProblemSnapshot, UnavailableReason},
    telemetry::{
        AdmissionState, CacheState, CancellationState, CountBucket, DescendantSettlement,
        DiagnosticState, Event, FallbackReason, Language, OutputSizeClass, ProjectCheckState,
        Telemetry, ToolMethod, ToolOutcome,
    },
};

/// Records one of the six Assistance tool completions from its existing typed daemon reply.
///
/// `elapsed` is the already measured local dispatch interval and is saturated to `u32` whole
/// milliseconds. `language`, `cache`, and `diagnostics` must be pre-sanitized closed summaries;
/// callers must use `None`/`NotApplicable` when no existing fact is available. The call never
/// changes the reply, waits for durable storage, or exposes a submission result. A settled edit
/// retrieved through `ide.inspect` remains attributed to `edit`, because the typed result owns the
/// completed operation while the earlier pending Edit call already records its incomplete poll.
pub fn tool_reply(
    telemetry: &Telemetry,
    tool: AssistanceTool,
    reply: &PeerReply,
    elapsed: Duration,
    language: Option<Language>,
    cache: CacheState,
    diagnostics: DiagnosticState,
) {
    let method = if matches!(reply, PeerReply::Edit { .. }) {
        ToolMethod::Edit
    } else {
        tool_method(tool)
    };
    let diagnostics = match reply {
        PeerReply::Edit {
            diagnostics: EditDiagnostics::CurrentClean {},
            ..
        } => DiagnosticState::Clean,
        PeerReply::Edit {
            diagnostics: EditDiagnostics::CurrentReported { .. },
            ..
        } => DiagnosticState::Changed,
        PeerReply::Edit {
            diagnostics: EditDiagnostics::Unknown {} | EditDiagnostics::Pending { .. },
            ..
        } => DiagnosticState::Unavailable,
        _ => diagnostics,
    };
    telemetry.record(Event::ToolCompleted {
        method,
        outcome: reply_outcome(reply),
        duration_ms: elapsed.as_millis().try_into().unwrap_or(u32::MAX),
        language,
        cache,
        diagnostics,
    });
}

/// Records an unavailable native hook boundary without retaining or interpreting the hook payload.
///
/// Successful hook submission does not need a duplicate event because its eventual daemon tool or
/// provider observation is recorded at that owner. Feedback text is intentionally ignored. This
/// function never prints, retries, or changes the hook's fail-open return behaviour.
pub fn hook_result(telemetry: &Telemetry, result: &HookIngressOutcome) {
    if matches!(result, HookIngressOutcome::Unavailable) {
        telemetry.record(Event::NativeFallback {
            reason: FallbackReason::HookUnavailable,
        });
    }
}

/// Records an existing provider/cache/diagnostic summary without accepting a provider name or key.
///
/// This is used where a provider path already knows its language and closed readiness/cache state.
/// It cannot cause provider startup, cache retention, diagnostic refresh, or a retry.
pub fn provider_summary(
    telemetry: &Telemetry,
    language: Language,
    cache: CacheState,
    diagnostics: DiagnosticState,
) {
    telemetry.record(Event::ProviderObserved {
        language,
        cache,
        diagnostics,
    });
}

/// Records existing Execution completion facts after output has already been bounded and settled.
///
/// `output`, `admission`, `cancellation`, and `descendants` are closed classifications produced by
/// the existing Execution path. No CPU/RSS sampling, output inspection, process lookup, or child
/// control occurs here; `elapsed` is saturated to whole milliseconds before recording.
pub fn execution_summary(
    telemetry: &Telemetry,
    elapsed: Duration,
    output: OutputSizeClass,
    admission: AdmissionState,
    cancellation: CancellationState,
    descendants: DescendantSettlement,
) {
    telemetry.record(Event::ExecutionCompleted {
        duration_ms: elapsed.as_millis().try_into().unwrap_or(u32::MAX),
        output,
        admission,
        cancellation,
        descendants,
    });
}

/// Records one completed project check from its snapshot's language, state, duration and counts.
///
/// Problem paths, messages and codes are never read; counts are bucketed and the duration is
/// saturated to whole `u32` milliseconds. Recording is synchronous, bounded and fail-open.
pub fn project_check(telemetry: &Telemetry, snapshot: &ProblemSnapshot) {
    telemetry.record(Event::ProjectCheckCompleted {
        language: match snapshot.language {
            checks::Language::Rust => Language::Rust,
            checks::Language::Python => Language::Python,
        },
        state: match snapshot.state {
            CheckState::Ready => ProjectCheckState::Ready,
            CheckState::Partial => ProjectCheckState::Partial,
            CheckState::Checking => ProjectCheckState::Checking,
            CheckState::Unavailable(UnavailableReason::Disabled) => ProjectCheckState::Disabled,
            CheckState::Unavailable(UnavailableReason::OutsideRoots) => {
                ProjectCheckState::OutsideRoots
            }
            CheckState::Unavailable(UnavailableReason::ToolMissing) => {
                ProjectCheckState::ToolMissing
            }
            CheckState::Unavailable(UnavailableReason::EnvMissing) => ProjectCheckState::EnvMissing,
            CheckState::Unavailable(UnavailableReason::NoFiles) => ProjectCheckState::NoFiles,
            CheckState::Unavailable(UnavailableReason::Fatal) => ProjectCheckState::Fatal,
            CheckState::Unavailable(UnavailableReason::Timeout) => ProjectCheckState::Timeout,
        },
        duration_ms: snapshot.duration_ms.try_into().unwrap_or(u32::MAX),
        errors_bucket: CountBucket::of(snapshot.errors),
        warnings_bucket: CountBucket::of(snapshot.warnings),
    });
}

/// Converts the existing public Assistance tool enum into the corresponding closed telemetry tag.
fn tool_method(tool: AssistanceTool) -> ToolMethod {
    match tool {
        AssistanceTool::Start => ToolMethod::Start,
        AssistanceTool::Context => ToolMethod::Context,
        AssistanceTool::Diff => ToolMethod::Diff,
        AssistanceTool::Inspect => ToolMethod::Inspect,
        AssistanceTool::Stop => ToolMethod::Stop,
        AssistanceTool::Edit => ToolMethod::Edit,
    }
}

/// Converts a typed daemon reply into a closed telemetry result without retaining reply text.
fn reply_outcome(reply: &PeerReply) -> ToolOutcome {
    match reply {
        PeerReply::Complete { .. } | PeerReply::HostStopped {} | PeerReply::HookSettled {} => {
            ToolOutcome::Completed
        }
        PeerReply::Pending { .. }
        | PeerReply::HookObserved {}
        | PeerReply::NativeHookObserved {} => ToolOutcome::Incomplete,
        PeerReply::Unavailable { reason } => match reason {
            MissingPeer::WorkspaceActivation | MissingPeer::HostBinding => ToolOutcome::Unavailable,
        },
        PeerReply::Error {
            code: FailureCode::Cancelled,
        } => ToolOutcome::Cancelled,
        PeerReply::Error {
            code: FailureCode::InvalidDetail,
        } => ToolOutcome::Invalid,
        PeerReply::Error {
            code: FailureCode::Deadline,
        } => ToolOutcome::Incomplete,
        PeerReply::Error { .. } => ToolOutcome::Failed,
        PeerReply::Feedback { .. } => ToolOutcome::Completed,
        PeerReply::Edit { result, .. } => match result.outcome {
            EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged => {
                ToolOutcome::Completed
            }
            EditOutcome::StaleSource
            | EditOutcome::ConflictingDuplicate
            | EditOutcome::UnsafeTarget => ToolOutcome::Invalid,
            EditOutcome::CancelledNoEffect => ToolOutcome::Cancelled,
            EditOutcome::DeadlineNoEffect
            | EditOutcome::CapacityNoEffect
            | EditOutcome::OutcomeUnknown => ToolOutcome::Incomplete,
            EditOutcome::UnavailableBeforeDispatch => ToolOutcome::Unavailable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{Filter, TelemetryConfig, ToolMethod};
    use std::time::Duration;

    /// Proves adapters mention only the fixed closed event vocabulary in their source module.
    #[test]
    fn adapters_keep_their_public_input_closed() {
        assert_eq!(tool_method(AssistanceTool::Diff), ToolMethod::Diff);
        assert_eq!(
            reply_outcome(&PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
            }),
            ToolOutcome::Unavailable
        );
        assert_eq!(
            reply_outcome(&PeerReply::Error {
                code: FailureCode::ProviderUnavailable,
            }),
            ToolOutcome::Failed
        );
        assert_eq!(
            reply_outcome(&PeerReply::Error {
                code: FailureCode::Deadline,
            }),
            ToolOutcome::Incomplete
        );
    }

    /// Proves a tool adapter persists only its closed result facts and never the reply's text.
    #[tokio::test]
    async fn tool_adapter_omits_completed_reply_text() {
        let (telemetry, path) = super::super::open_test_telemetry(TelemetryConfig::default()).await;
        tool_reply(
            &telemetry,
            AssistanceTool::Context,
            &PeerReply::Complete {
                kind: crate::assistance::reply::ResultKind::Context,
                text: "private source-shaped result".to_owned(),
                detail_ref: Some("private-detail".to_owned()),
                truncated: false,
                continuation: false,
            },
            Duration::from_millis(12),
            None,
            CacheState::NotApplicable,
            DiagnosticState::NotApplicable,
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        let row = telemetry
            .query(Filter::All, None, 1)
            .await
            .unwrap()
            .rows
            .remove(0);
        assert_eq!(
            row.event,
            Event::ToolCompleted {
                method: ToolMethod::Context,
                outcome: ToolOutcome::Completed,
                duration_ms: 12,
                language: None,
                cache: CacheState::NotApplicable,
                diagnostics: DiagnosticState::NotApplicable,
            }
        );
        let _ = std::fs::remove_file(path);
    }

    /// Persists an edit's closed method/outcome/diagnostic classes without its private fields.
    #[tokio::test]
    async fn edit_adapter_omits_path_reference_and_diagnostic_message() {
        let (telemetry, path) = super::super::open_test_telemetry(TelemetryConfig::default()).await;
        let reply = PeerReply::Edit {
            result: crate::changes::edit::EditResult::new(
                "private-operation".into(),
                "private/source.ts".into(),
                EditOutcome::Replaced,
                Some("private-source-reference".into()),
            )
            .unwrap(),
            diagnostics: EditDiagnostics::CurrentReported {
                messages: vec!["private diagnostic message".into()],
                delta: "private diagnostic delta".into(),
                truncated: false,
            },
        };
        tool_reply(
            &telemetry,
            AssistanceTool::Edit,
            &reply,
            Duration::from_millis(18),
            Some(crate::telemetry::Language::Typescript),
            CacheState::Miss,
            DiagnosticState::NotApplicable,
        );
        telemetry.shutdown().await;
        let row = telemetry
            .query(Filter::All, None, 1)
            .await
            .unwrap()
            .rows
            .remove(0);
        assert_eq!(
            row.event,
            Event::ToolCompleted {
                method: ToolMethod::Edit,
                outcome: ToolOutcome::Completed,
                duration_ms: 18,
                language: Some(crate::telemetry::Language::Typescript),
                cache: CacheState::Miss,
                diagnostics: DiagnosticState::Changed,
            }
        );
        let export = String::from_utf8(telemetry.export(Filter::All).await.unwrap().bytes).unwrap();
        for private in [
            "private-operation",
            "private/source.ts",
            "private-source-reference",
            "private diagnostic message",
            "private diagnostic delta",
        ] {
            assert!(!export.contains(private));
        }
        let _ = std::fs::remove_file(path);
    }
}
