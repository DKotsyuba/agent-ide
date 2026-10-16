//! The T-008 table: every refusal the reply template renders, with its E-002 (`eaeba10`) and
//! current byte size, asserted to have the harness-reply-v1 R5 shape.
//!
//! The rows are the closed set of `reply.jinja` refusal branches (`unavailable`, typed `error`
//! codes with their stage details, `invalid_parameters` and the durable `edit` outcomes). Each
//! row renders through the production projection ([`super::render_with_call`]) with the
//! text-only envelope; the assertion is the contract of R5: a head of at most 160 bytes, exactly
//! one `Repair:` line of at most 120 bytes that names one step (no `or`, no retry count), no
//! internal reason code, and an excerpt only after the Repair line.

use super::*;
use crate::assistance::reply::EditDiagnostics;
use crate::assistance::reply::{ExecutionProfileCause, HostBindingCause, MissingPeer};
use crate::changes::edit::{EditOutcome, EditResult};

/// One table row: a label, the reply that provokes the refusal, the tool the call named, an
/// optional `ide.test` run id for the inactive-binding wording.
struct Row {
    /// Stable row label printed in the inventory table.
    label: &'static str,
    /// Typed reply that selects exactly one template branch.
    reply: PeerReply,
    /// Public tool name the call carried, when the wording depends on it.
    tool: Option<&'static str>,
    /// `ide.test` run id for the inactive-binding recovery wording.
    test_status: Option<u64>,
}

/// Builds a typed error row.
fn error(label: &'static str, code: FailureCode, detail: Option<&str>) -> Row {
    Row {
        label,
        reply: PeerReply::Error {
            code,
            detail: detail.map(str::to_owned),
        },
        tool: None,
        test_status: None,
    }
}

/// Builds a host-binding refusal row.
fn binding(label: &'static str, cause: Option<HostBindingCause>) -> Row {
    Row {
        label,
        reply: PeerReply::Unavailable {
            reason: MissingPeer::HostBinding,
            cause,
        },
        tool: None,
        test_status: None,
    }
}

/// Builds a durable edit outcome row.
fn edit(label: &'static str, outcome: EditOutcome, note: Option<&str>) -> Row {
    Row {
        label,
        reply: PeerReply::Edit {
            result: EditResult::new("op-1".into(), "src/lib.rs".into(), outcome, None).unwrap(),
            diagnostics: EditDiagnostics::Unknown {},
            note: note.map(str::to_owned),
            operation: None,
        },
        tool: None,
        test_status: None,
    }
}

/// Returns every rewritten refusal, in template order.
fn rows() -> Vec<Row> {
    use FailureCode as F;
    let mut rows = vec![
        binding(
            "host: outside_allowed_roots",
            Some(HostBindingCause::OutsideAllowedRoots),
        ),
        binding(
            "host: hooks_not_delivered",
            Some(HostBindingCause::HooksNotDelivered),
        ),
        binding(
            "host: invalid_metadata",
            Some(HostBindingCause::InvalidMetadata),
        ),
        binding("host: missing_field", Some(HostBindingCause::MissingField)),
        binding("host: invalid_field", Some(HostBindingCause::InvalidField)),
        binding(
            "host: invalid_attachment",
            Some(HostBindingCause::InvalidAttachment),
        ),
        binding(
            "host: unsupported_hook_phase",
            Some(HostBindingCause::UnsupportedHookPhase),
        ),
        binding("host: missing_pre", Some(HostBindingCause::MissingPre)),
        binding("host: replay", Some(HostBindingCause::Replay)),
        binding("host: mismatch", Some(HostBindingCause::Mismatch)),
        binding(
            "host: missing_invocation",
            Some(HostBindingCause::MissingInvocation),
        ),
        binding(
            "host: inactive_binding",
            Some(HostBindingCause::InactiveBinding),
        ),
        binding(
            "host: never_activated",
            Some(HostBindingCause::NeverActivated),
        ),
        binding(
            "host: recovery_needed",
            Some(HostBindingCause::RecoveryNeeded),
        ),
        binding(
            "host: capacity_exceeded",
            Some(HostBindingCause::CapacityExceeded),
        ),
        binding(
            "host: host_unrecognized",
            Some(HostBindingCause::HostUnrecognized),
        ),
        binding(
            "host: invalid_parameters (fallback)",
            Some(HostBindingCause::InvalidParameters),
        ),
        binding(
            "host: internal_lock (fallback)",
            Some(HostBindingCause::InternalLock),
        ),
        binding(
            "host: worker_unavailable (fallback)",
            Some(HostBindingCause::WorkerUnavailable),
        ),
        binding(
            "host: taken_over",
            Some(HostBindingCause::TakenOver {
                reason: "after 30 min without activity".into(),
            }),
        ),
        binding(
            "host: project_moved",
            Some(HostBindingCause::project_moved(
                std::path::Path::new("/work/bound"),
                "/work/other",
            )),
        ),
        binding(
            "host: other_repository",
            Some(HostBindingCause::other_repository(
                std::path::Path::new("/work/bound"),
                "/work/other",
            )),
        ),
        binding("host: bare", None),
        Row {
            label: "host: inactive_binding for ide.test",
            reply: PeerReply::Unavailable {
                reason: MissingPeer::HostBinding,
                cause: Some(HostBindingCause::InactiveBinding),
            },
            tool: Some("ide.test"),
            test_status: Some(3),
        },
        Row {
            label: "workspace_activation (bare unavailable)",
            reply: PeerReply::Unavailable {
                reason: MissingPeer::WorkspaceActivation,
                cause: None,
            },
            tool: None,
            test_status: None,
        },
        Row {
            label: "invalid_parameters",
            reply: PeerReply::InvalidParameters {
                message: "`symbol` must be a string; got a number".into(),
            },
            tool: None,
            test_status: None,
        },
        error("no_such_file", F::NoSuchFile("src/x.rs".into()), None),
        error("unsupported_file", F::UnsupportedFile("a.xyz".into()), None),
        error(
            "source_too_large",
            F::SourceTooLarge {
                size: 5000,
                ceiling: 4096,
            },
            None,
        ),
        error(
            "edit_refused",
            F::EditRefused,
            Some(
                "edit:refused: 1 of 1 changes refused, nothing written — change 1: old text not found",
            ),
        ),
        error("unknown_symbol", F::UnknownSymbol, None),
        error(
            "unknown_symbol: region",
            F::UnknownSymbol,
            Some("symbol:region: src/a.rs:3 is file-level code at src/a.rs#@imports, not a symbol"),
        ),
        error(
            "unknown_symbol: line_past_end",
            F::UnknownSymbol,
            Some("symbol:line_past_end: src/a.rs:99 is past the end of the file"),
        ),
        error(
            "unknown_symbol: edit range_past_end",
            F::UnknownSymbol,
            Some("edit:range_past_end: line 17 is past the end of src/a.rs (3 lines)"),
        ),
        error("provider_loading", F::ProviderLoading, None),
        error(
            "invalid_detail: activation_conflict",
            F::InvalidDetail,
            Some("start:activation_conflict"),
        ),
        error(
            "invalid_detail: detail_mismatch",
            F::InvalidDetail,
            Some("inspect:detail_mismatch"),
        ),
        error(
            "invalid_detail: unknown test run",
            F::InvalidDetail,
            Some("test:unknown_run:7"),
        ),
        error(
            "invalid_detail: environment",
            F::InvalidDetail,
            Some("environment: alpha: candidate absent; candidates one, two"),
        ),
        error(
            "invalid_detail: detail_expired",
            F::InvalidDetail,
            Some("inspect:detail_expired"),
        ),
        error(
            "invalid_detail: never issued",
            F::InvalidDetail,
            Some("inspect:detail_unknown"),
        ),
        error(
            "source_unavailable: not a git repository",
            F::SourceUnavailable,
            Some("diff:not_a_git_repository"),
        ),
        error(
            "source_unavailable: invalid byte offset",
            F::SourceUnavailable,
            Some("symbol:invalid_byte_offset"),
        ),
        error(
            "source_unavailable: source changed",
            F::SourceUnavailable,
            Some("inspect:source_changed:src/a.rs changed since capture"),
        ),
        error(
            "source_unavailable: source stale",
            F::SourceUnavailable,
            Some("inspect:source_stale"),
        ),
        error(
            "source_unavailable: activation commit unknown",
            F::SourceUnavailable,
            Some("diff:activation_commit_unknown"),
        ),
        error(
            "source_unavailable: diff unsupported entry",
            F::SourceUnavailable,
            Some("diff:unsupported_entry"),
        ),
        error(
            "source_unavailable: line range",
            F::SourceUnavailable,
            Some("read:line_range:src/a.rs has 3 lines"),
        ),
        error(
            "source_unavailable: not text",
            F::SourceUnavailable,
            Some("read:not_text:src/a.png is binary"),
        ),
        error(
            "source_unavailable: other",
            F::SourceUnavailable,
            Some("read:source_unavailable:src/a.rs vanished"),
        ),
        error(
            "resolution_unverified",
            F::ResolutionUnverified,
            Some("outline:resolution_unverified"),
        ),
        error(
            "outside_allowed_roots: environment",
            F::OutsideAllowedRoots,
            Some("environment \"rust\" ../venv"),
        ),
        error(
            "outside_allowed_roots: root absent",
            F::OutsideAllowedRoots,
            Some("start:root_absent; nearest existing ancestor below an allowed root: /work"),
        ),
        error("outside_allowed_roots", F::OutsideAllowedRoots, None),
        error(
            "conflict: read_only",
            F::Conflict,
            Some("read_only:ide.edit:activation t7, since 2026-10-04T10:00:00Z"),
        ),
        error(
            "conflict: held by another actor",
            F::Conflict,
            Some("start:worktree_held_by_another_actor activation t7"),
        ),
        error(
            "conflict: held by this actor",
            F::Conflict,
            Some("start:worktree_held_by_this_actor channel two"),
        ),
        error(
            "conflict: actor owns another worktree",
            F::Conflict,
            Some("start:actor_owns_another_worktree /work/other"),
        ),
        error(
            "conflict: cache namespace",
            F::Conflict,
            Some("start:provider_cache_namespace_conflict"),
        ),
        error(
            "conflict: activation_conflict",
            F::Conflict,
            Some("start:activation_conflict"),
        ),
        error("conflict", F::Conflict, None),
        error(
            "workspace_activation: git discovery",
            F::WorkspaceActivation,
            Some("start:git_discovery_failed:not a repository"),
        ),
        error(
            "workspace_activation: durable state",
            F::WorkspaceActivation,
            Some("start:durable_state:locked"),
        ),
        error(
            "workspace_activation: worktree unresolved",
            F::WorkspaceActivation,
            Some("start:worktree_unresolved:identity_read"),
        ),
        error("workspace_activation", F::WorkspaceActivation, None),
        error(
            "stop: store_deadline",
            F::Internal,
            Some("stop:store_deadline"),
        ),
        error("stop: store_full", F::Internal, Some("stop:store_full")),
        error("stop: busy", F::Internal, Some("stop:busy")),
        error("stop: deadline", F::Internal, Some("stop:deadline")),
        error("stop: authority", F::Internal, Some("stop:authority")),
        error("workspace_authority", F::WorkspaceAuthority, None),
        error(
            "provider: rename_no_edits",
            F::ProviderUnavailable,
            Some("edit:rename_no_edits:foo"),
        ),
        error(
            "provider: rename_request_failed",
            F::ProviderUnavailable,
            Some("edit:rename_request_failed:timeout"),
        ),
        error(
            "provider: rename_unsupported_edits",
            F::ProviderUnavailable,
            Some("edit:rename_unsupported_edits:outside root"),
        ),
        error(
            "provider: test selection",
            F::ProviderUnavailable,
            Some("test:selection_unavailable"),
        ),
        error(
            "provider: answer from source",
            F::ProviderUnavailable,
            Some(
                "symbol:provider_unavailable (fixtureserver: load failed; outline and read answer from source)",
            ),
        ),
        error(
            "provider: native reads",
            F::ProviderUnavailable,
            Some("symbol:provider_unavailable (fixtureserver: load failed; use native reads)"),
        ),
        error(
            "provider: no server for type",
            F::ProviderUnavailable,
            Some("outline:provider_unavailable ext=md"),
        ),
        error("provider_unavailable", F::ProviderUnavailable, None),
        error("unsupported_git", F::UnsupportedGit, None),
        error("launcher_configuration", F::LauncherConfiguration, None),
        error("capacity: diff", F::Capacity, Some("diff:too_large")),
        error(
            "capacity: queue_full",
            F::Capacity,
            Some("worker:queue_full"),
        ),
        error(
            "capacity: result_store_full",
            F::Capacity,
            Some("worker:result_store_full"),
        ),
        error(
            "capacity: actor_share_full",
            F::Capacity,
            Some("worker:actor_share_full"),
        ),
        error("capacity", F::Capacity, None),
        error("cancelled", F::Cancelled, None),
        error("deadline", F::Deadline, None),
        error(
            "execution_profile: symlink preflight",
            F::ExecutionProfile,
            Some("inspect:symlink_preflight"),
        ),
        error("execution_profile", F::ExecutionProfile, None),
        error("internal", F::Internal, None),
    ];
    for (label, cause) in [
        (
            "execution_profile: git_policy",
            ExecutionProfileCause::GitPolicy,
        ),
        (
            "execution_profile: query_policy",
            ExecutionProfileCause::QueryPolicy,
        ),
        (
            "execution_profile: git_unsupported",
            ExecutionProfileCause::GitUnsupported,
        ),
        (
            "execution_profile: spawn_io",
            ExecutionProfileCause::SpawnIo,
        ),
        (
            "execution_profile: spawn_request",
            ExecutionProfileCause::SpawnRequest,
        ),
        (
            "execution_profile: stdout_reserved",
            ExecutionProfileCause::SpawnProtocolStdoutReserved,
        ),
        (
            "execution_profile: reap_timed_out",
            ExecutionProfileCause::SpawnReapTimedOut,
        ),
    ] {
        rows.push(error(label, F::ExecutionProfileCause(cause), None));
    }
    rows.extend([
        edit("edit: stale_source", EditOutcome::StaleSource, None),
        edit(
            "edit: stale_source (supplied source_ref)",
            EditOutcome::StaleSource,
            Some(
                "the supplied source_ref does not match this file's current text. Repair: Send the edit without source_ref",
            )),
        edit(
            "edit: conflicting_duplicate",
            EditOutcome::ConflictingDuplicate,
            None),
        edit("edit: unsafe_target", EditOutcome::UnsafeTarget, None),
        edit(
            "edit: cancelled_no_effect",
            EditOutcome::CancelledNoEffect,
            None),
        edit(
            "edit: deadline_no_effect",
            EditOutcome::DeadlineNoEffect,
            None),
        edit(
            "edit: capacity_no_effect",
            EditOutcome::CapacityNoEffect,
            None),
        edit(
            "edit: outcome_unknown",
            EditOutcome::OutcomeUnknown,
            None),
        edit(
            "edit: unavailable_before_dispatch",
            EditOutcome::UnavailableBeforeDispatch,
            None),
    ]);
    rows
}

/// Bytes of the same reply rendered by the E-002 template (`eaeba10:assets/mcp/reply.jinja`),
/// measured by rendering that template with the same engine and context; one entry per row.
const E002_BYTES: [(&str, usize); 107] = [
    ("host: outside_allowed_roots", 182),
    ("host: hooks_not_delivered", 169),
    ("host: invalid_metadata", 136),
    ("host: missing_field", 134),
    ("host: invalid_field", 140),
    ("host: invalid_attachment", 143),
    ("host: unsupported_hook_phase", 148),
    ("host: missing_pre", 161),
    ("host: replay", 132),
    ("host: mismatch", 124),
    ("host: missing_invocation", 171),
    ("host: inactive_binding", 157),
    ("host: never_activated", 152),
    ("host: recovery_needed", 182),
    ("host: capacity_exceeded", 143),
    ("host: host_unrecognized", 132),
    ("host: invalid_parameters (fallback)", 74),
    ("host: internal_lock (fallback)", 69),
    ("host: worker_unavailable (fallback)", 74),
    ("host: taken_over", 222),
    ("host: project_moved", 208),
    ("host: other_repository", 323),
    ("host: bare", 53),
    ("host: inactive_binding for ide.test", 180),
    ("workspace_activation (bare unavailable)", 61),
    ("invalid_parameters", 39),
    ("no_such_file", 152),
    ("unsupported_file", 207),
    ("source_too_large", 137),
    ("edit_refused", 168),
    ("unknown_symbol", 153),
    ("unknown_symbol: region", 211),
    ("unknown_symbol: line_past_end", 193),
    ("unknown_symbol: edit range_past_end", 157),
    ("provider_loading", 249),
    ("invalid_detail: activation_conflict", 121),
    ("invalid_detail: detail_mismatch", 178),
    ("invalid_detail: unknown test run", 101),
    ("invalid_detail: environment", 155),
    ("invalid_detail: detail_expired", 157),
    ("invalid_detail: never issued", 162),
    ("source_unavailable: not a git repository", 88),
    ("source_unavailable: invalid byte offset", 144),
    ("source_unavailable: source changed", 176),
    ("source_unavailable: source stale", 151),
    ("source_unavailable: activation commit unknown", 129),
    ("source_unavailable: diff unsupported entry", 121),
    ("source_unavailable: line range", 65),
    ("source_unavailable: not text", 207),
    ("source_unavailable: other", 148),
    ("resolution_unverified", 135),
    ("outside_allowed_roots: environment", 399),
    ("outside_allowed_roots: root absent", 272),
    ("outside_allowed_roots", 198),
    ("conflict: read_only", 231),
    ("conflict: held by another actor", 251),
    ("conflict: held by this actor", 216),
    ("conflict: actor owns another worktree", 335),
    ("conflict: cache namespace", 154),
    ("conflict: activation_conflict", 136),
    ("conflict", 130),
    ("workspace_activation: git discovery", 148),
    ("workspace_activation: durable state", 160),
    ("workspace_activation: worktree unresolved", 200),
    ("workspace_activation", 161),
    ("stop: store_deadline", 176),
    ("stop: store_full", 287),
    ("stop: busy", 182),
    ("stop: deadline", 115),
    ("stop: authority", 147),
    ("workspace_authority", 151),
    ("provider: rename_no_edits", 142),
    ("provider: rename_request_failed", 147),
    ("provider: rename_unsupported_edits", 175),
    ("provider: test selection", 143),
    ("provider: answer from source", 387),
    ("provider: native reads", 342),
    ("provider: no server for type", 139),
    ("provider_unavailable", 135),
    ("unsupported_git", 144),
    ("launcher_configuration", 179),
    ("capacity: diff", 139),
    ("capacity: queue_full", 97),
    ("capacity: result_store_full", 163),
    ("capacity: actor_share_full", 149),
    ("capacity", 137),
    ("cancelled", 128),
    ("deadline", 124),
    ("execution_profile: symlink preflight", 152),
    ("execution_profile", 152),
    ("internal", 117),
    ("execution_profile: git_policy", 133),
    ("execution_profile: query_policy", 118),
    ("execution_profile: git_unsupported", 128),
    ("execution_profile: spawn_io", 109),
    ("execution_profile: spawn_request", 101),
    ("execution_profile: stdout_reserved", 147),
    ("execution_profile: reap_timed_out", 128),
    ("edit: stale_source", 180),
    ("edit: stale_source (supplied source_ref)", 286),
    ("edit: conflicting_duplicate", 128),
    ("edit: unsafe_target", 149),
    ("edit: cancelled_no_effect", 126),
    ("edit: deadline_no_effect", 125),
    ("edit: capacity_no_effect", 125),
    ("edit: outcome_unknown", 133),
    ("edit: unavailable_before_dispatch", 171),
];

/// Returns the E-002 size of the row `label`.
fn e002_bytes(label: &str) -> usize {
    E002_BYTES
        .iter()
        .find_map(|(name, bytes)| (*name == label).then_some(*bytes))
        .unwrap_or_else(|| panic!("no E-002 size for {label}"))
}

/// Words that name an internal reason code or a retry count and may not appear in a refusal.
const INTERNAL_WORDS: [&str; 14] = [
    "stale_source",
    "unsafe_target",
    "conflicting_duplicate",
    "cancelled_no_effect",
    "deadline_no_effect",
    "capacity_no_effect",
    "outcome_unknown",
    "unavailable_before_dispatch",
    "host_binding",
    "resolution_unverified",
    "edit:refused",
    "workspace_activation",
    "error:",
    "unavailable:",
];

/// Renders every row through the production projection and asserts the R5 shape; with
/// `AGENT_IDE_PRINT_REFUSAL_TABLE` set it prints `label | old | new` for the inventory table.
#[test]
fn every_rewritten_refusal_has_a_bounded_head_and_one_repair() {
    let print = std::env::var_os("AGENT_IDE_PRINT_REFUSAL_TABLE").is_some();
    let rows = rows();
    let mut labels = std::collections::BTreeSet::new();
    for row in &rows {
        assert!(labels.insert(row.label), "duplicate row {}", row.label);
        assert!(e002_bytes(row.label) > 0);
        let rendered = render_with_call(
            row.reply.clone(),
            None,
            Envelope::TextOnly,
            row.tool,
            row.test_status,
        )
        .unwrap_or_else(|| panic!("{} did not render", row.label));
        let text = rendered.content[0].as_text().unwrap().text.clone();
        let lines = text.lines().collect::<Vec<_>>();
        let repairs = lines
            .iter()
            .filter(|line| line.starts_with("Repair: "))
            .count();
        assert_eq!(repairs, 1, "{}: {text}", row.label);
        assert!(lines[0].len() <= 160, "{} head: {text}", row.label);
        assert!(
            lines[1].starts_with("Repair: ") && lines[1].len() <= 120,
            "{} repair: {text}",
            row.label
        );
        let repair = &lines[1]["Repair: ".len()..];
        assert!(
            !repair.contains(" or ") && !repair.contains("once") && !repair.contains("otherwise"),
            "{} names more than one step: {text}",
            row.label
        );
        for word in INTERNAL_WORDS {
            assert!(
                !lines[0].contains(word) && !repair.contains(word),
                "{} shows {word}: {text}",
                row.label
            );
        }
        assert!(
            lines[2..].iter().all(|line| line.starts_with("  ")),
            "{} excerpt must be quoted data after the Repair line: {text}",
            row.label
        );
        if print {
            println!(
                "ROW | {} | {} | {}",
                row.label,
                e002_bytes(row.label),
                text.len()
            );
        }
    }
    assert_eq!(
        rows.len(),
        E002_BYTES.len(),
        "the table and its E-002 sizes drifted apart"
    );
}
