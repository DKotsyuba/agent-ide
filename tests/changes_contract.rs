//! Contract checks for bounded Changes v0.1 diff composition.

use serde_json::json;

use agent_ide::assistance::host_binding::{
    BindingStatus, CandidateInvocation, ChannelSessionRef, HostBindingGuard, parse_candidate,
    parse_channel_session, parse_hook_event,
};
use agent_ide::changes::{DiffResultState, DiffSelectionBudget, compose_diff};
use agent_ide::workspace::authority::{
    ActivationRequest, AuthorityError, AuthorityRegistry, StopBindingHandoff, WorktreeRef,
};
use agent_ide::workspace::git::{
    BaselineContext, BaselineCoverage, DiffMode, GitComparison, GitIdentity, GitScope,
    RawGitEvidence, StatusKind, parse_porcelain_v2_z,
};

use std::path::PathBuf;

fn candidate(actor: &str, call: &str) -> CandidateInvocation {
    parse_candidate(
        json!({
            "threadId": actor,
            "callId": call,
            "x-codex-turn-metadata": { "turn": "contract" },
        })
        .as_object()
        .unwrap(),
    )
    .expect("candidate fixture is valid")
}

fn pre_hook(actor: &str, call: &str) -> agent_ide::assistance::host_binding::HookEvent {
    parse_hook_event(
        json!({
            "hook_event_name": "PreToolUse",
            "session_id": actor,
            "tool_use_id": call,
        })
        .to_string()
        .as_bytes(),
    )
    .expect("pre hook fixture is valid")
}

fn scope_from_mode(mode: DiffMode) -> GitScope {
    let mut registry = AuthorityRegistry::default();
    let mut guard = HostBindingGuard::default();
    let channel: ChannelSessionRef = parse_channel_session(b"changes-contract").unwrap();
    let pre = pre_hook("actor", "call");
    assert!(matches!(
        guard.observe_hook(pre, channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("start must validate")
    };
    let active_use = guard
        .consume_active(invocation.binding_ref())
        .expect("active start should provide a fresh use");
    let request = ActivationRequest::new(
        "changes-contract-operation",
        invocation,
        active_use,
        WorktreeRef::from_discovery(
            PathBuf::from("/private/tmp/changes-contract"),
            PathBuf::from("/private/tmp/changes-contract"),
            PathBuf::from("/private/tmp/changes-contract/.git"),
            1,
        )
        .unwrap(),
    )
    .expect("activation request is coherent");
    let stamp = match registry.activate(request) {
        Ok(stamp) => stamp,
        Err(AuthorityError::WorktreeOwned) => {
            panic!("fixture worktree must be unique in this test process")
        }
        Err(error) => panic!("expected activation success: {error:?}"),
    };

    GitScope::from_authority(&stamp, mode)
}

fn comparison(mode: DiffMode) -> GitComparison {
    GitComparison::new(
        mode,
        GitIdentity::new(b"left-id".to_vec()).expect("left identity is valid"),
        GitIdentity::new(b"right-id".to_vec()).expect("right identity is valid"),
        BaselineContext::new("baseline", BaselineCoverage::Complete)
            .expect("baseline context is bounded"),
    )
}

fn make_evidence(
    scope: &GitScope,
    stdout: &[u8],
    exit_code: Option<i32>,
    truncated: bool,
) -> RawGitEvidence {
    RawGitEvidence::new(
        "operation-1",
        scope.clone(),
        stdout.to_vec(),
        Vec::new(),
        exit_code,
        truncated,
        false,
    )
    .expect("evidence has bounded metadata")
}

fn status_fixture() -> agent_ide::workspace::git::GitStatus {
    parse_porcelain_v2_z(
        b"1 M. N... 100644 100644 100644 a b tracked\0u UU N... 100644 100644 100644 100644 a b c conflict\0? -leading\npath\0",
    )
    .expect("fixture status parses as bounded tracked/conflict/untracked")
}

fn assert_mode_counts(result: &agent_ide::changes::DiffResult, tracked: usize, conflicted: usize) {
    let counts = result.counts();
    assert_eq!(counts.tracked(), tracked);
    assert_eq!(counts.conflicted(), conflicted);
}

#[test]
fn compose_ready_keeps_mode_exact_and_separates_untracked_and_conflicts() {
    let scope = scope_from_mode(DiffMode::Head);
    let comparison = comparison(DiffMode::Head);
    let status = status_fixture();
    let diff = b"diff --git a/tracked b/tracked\n@@ -1,1 +1,1 @@\n-old\n+new\n";
    let evidence = make_evidence(&scope, diff, Some(0), false);
    let result = compose_diff(
        &scope,
        &comparison,
        status,
        evidence,
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Ready);
    assert_eq!(result.mode(), DiffMode::Head);
    assert!(result.detail_cursor().is_none());
    assert_eq!(result.untracked().len(), 1);
    assert_eq!(result.conflicts().len(), 1);
    assert_eq!(result.selected_hunks().len(), 1);
    assert!(
        result
            .ignored()
            .iter()
            .all(|p| p.kind() == StatusKind::Ignored)
    );
    assert_mode_counts(&result, 1, 1);
}

#[test]
fn compose_rejects_scope_mismatch_as_unavailable() {
    let scope = scope_from_mode(DiffMode::Head);
    let mismatched = comparison(DiffMode::Staged);
    let result = compose_diff(
        &scope,
        &mismatched,
        status_fixture(),
        make_evidence(&scope, b"diff --git a/a b/a\n", Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Unavailable);
    assert_eq!(
        result.freshness(),
        agent_ide::changes::DiffFreshness::Unknown
    );
    assert_eq!(result.identities().left(), b"");
    assert_eq!(result.identities().right(), b"");
    assert_eq!(result.counts().tracked(), 0);
    assert!(result.selected_hunks().is_empty());
    assert!(result.untracked().is_empty());
    assert!(result.conflicts().is_empty());
    assert!(result.ignored().is_empty());
    assert!(result.detail_cursor().is_none());
    assert_eq!(result.provenance().operation_reference(), None);
    assert_eq!(result.provenance().baseline_reference(), None);
}

#[test]
fn compose_marks_truncated_stdout_as_incomplete() {
    let scope = scope_from_mode(DiffMode::Unstaged);
    let comparison = comparison(DiffMode::Unstaged);
    let status = status_fixture();
    let result = compose_diff(
        &scope,
        &comparison,
        status.clone(),
        make_evidence(
            &scope,
            b"diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\n",
            Some(0),
            true,
        ),
        DiffSelectionBudget::bounded(1, 4),
    );
    assert_eq!(result.state(), DiffResultState::Incomplete);
    assert!(result.truncated_output());
    assert_eq!(result.overflow_hunks(), 1);
}

#[test]
fn compose_marks_malformed_output_as_incomplete() {
    let scope = scope_from_mode(DiffMode::Head);
    let comparison = comparison(DiffMode::Head);
    let result = compose_diff(
        &scope,
        &comparison,
        status_fixture(),
        make_evidence(
            &scope,
            b"diff --git a/a b/a\n@@ -1 +1 @@\n-old\n+new\nmalformed-tail\n",
            Some(0),
            false,
        ),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Incomplete);
    assert_eq!(result.overflow_hunks(), 0);
    assert_eq!(result.selected_hunks().len(), 1);
}

#[test]
fn compose_marks_binary_evidence_incomplete_and_preserves_binary_flag() {
    let scope = scope_from_mode(DiffMode::Unstaged);
    let comparison = comparison(DiffMode::Unstaged);
    let result = compose_diff(
        &scope,
        &comparison,
        status_fixture(),
        make_evidence(
            &scope,
            b"diff --git a/image.bin b/image.bin\nBinary files a/image.bin and b/image.bin differ\n",
            Some(0),
            false,
        ),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Incomplete);
    assert_eq!(result.selected_hunks().len(), 1);
    assert!(result.selected_hunks()[0].is_binary());
}

#[test]
fn compose_preserves_comparison_identities_and_provenance_owner_ref() {
    let scope = scope_from_mode(DiffMode::Head);
    let comparison = comparison(DiffMode::Head);
    let result = compose_diff(
        &scope,
        &comparison,
        status_fixture(),
        make_evidence(&scope, b"", Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.identities().left(), b"left-id");
    assert_eq!(result.identities().right(), b"right-id");
    assert_eq!(
        result.provenance().operation_reference(),
        Some("operation-1")
    );
}

#[test]
fn compose_applies_byte_and_hunk_overflow_without_partial_hunk_selection() {
    let scope = scope_from_mode(DiffMode::Head);
    let comparison = comparison(DiffMode::Head);
    let status = status_fixture();
    let diff = b"diff --git a/large b/large\n@@ -1,1 +1,1 @@\nline\nline\nline\nline\nline\n";
    let result = compose_diff(
        &scope,
        &comparison,
        status,
        make_evidence(&scope, diff, Some(0), false),
        DiffSelectionBudget::bounded(1, 8),
    );
    assert_eq!(result.state(), DiffResultState::Incomplete);
    assert_eq!(result.selected_hunks().len(), 0);
    assert_eq!(result.overflow_hunks(), 1);
    let cursor = result
        .detail_cursor()
        .expect("byte-limited payload must include cursor");
    assert_eq!(cursor.operation_reference(), "operation-1");
    assert_eq!(cursor.next_hunk(), 0);
}

#[test]
fn failed_exit_code_remains_failed_and_not_ready() {
    let scope = scope_from_mode(DiffMode::Head);
    let comparison = comparison(DiffMode::Head);
    let result = compose_diff(
        &scope,
        &comparison,
        status_fixture(),
        make_evidence(&scope, b"command failed", Some(1), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Failed);
}

#[test]
fn failed_exit_with_truncation_malformed_binary_keeps_failed_and_budget() {
    let scope = scope_from_mode(DiffMode::Head);
    let result = compose_diff(
        &scope,
        &comparison(DiffMode::Head),
        status_fixture(),
        make_evidence(
            &scope,
            b"diff --git a/file b/file\n@@ -1 +1 @@\n-old\n+new\nBinary files a/x and b/x differ\nbad-tail\n",
            Some(7),
            true,
        ),
        DiffSelectionBudget::bounded(0, 0),
    );
    assert_eq!(result.state(), DiffResultState::Failed);
    assert!(result.selected_hunks().is_empty());
    assert_eq!(result.overflow_hunks(), 1);
}

#[test]
fn path_with_spaces_keeps_raw_hunk_and_no_guessed_path() {
    let scope = scope_from_mode(DiffMode::Head);
    let patch = b"diff --git a/with space.txt b/with space.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let result = compose_diff(
        &scope,
        &comparison(DiffMode::Head),
        status_fixture(),
        make_evidence(&scope, patch, Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(
        result.selected_hunks()[0].patch(),
        b"@@ -1 +1 @@\n-old\n+new\n"
    );
    assert!(result.selected_hunks()[0].path().is_none());
}

#[test]
fn old_epoch_for_same_worktree_is_stale_and_returns_no_data() {
    let mut registry = AuthorityRegistry::default();
    let mut guard = HostBindingGuard::default();
    let worktree = WorktreeRef::from_discovery(
        PathBuf::from("/private/tmp/stale-contract"),
        PathBuf::from("/private/tmp/stale-contract"),
        PathBuf::from("/private/tmp/stale-contract/.git"),
        1,
    )
    .unwrap();
    let channel = parse_channel_session(b"stale-one").unwrap();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call-1"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call-1"), channel)
    else {
        panic!()
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let request =
        ActivationRequest::new("stale-op-1", invocation, active, worktree.clone()).unwrap();
    let old = registry.activate(request).unwrap();
    let old_scope = GitScope::from_authority(&old, DiffMode::Head);
    guard.stop_binding(old.binding()).unwrap();
    registry
        .revoke(&old, StopBindingHandoff::Confirmed)
        .unwrap();

    let channel = parse_channel_session(b"stale-two").unwrap();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call-2"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call-2"), channel)
    else {
        panic!()
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let request = ActivationRequest::new("stale-op-2", invocation, active, worktree).unwrap();
    let current = registry.activate(request).unwrap();
    let expected = GitScope::from_authority(&current, DiffMode::Head);
    let result = compose_diff(
        &expected,
        &comparison(DiffMode::Head),
        status_fixture(),
        make_evidence(&old_scope, b"foreign", Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Unavailable);
    assert_eq!(result.freshness(), agent_ide::changes::DiffFreshness::Stale);
    assert!(
        result.selected_hunks().is_empty()
            && result.untracked().is_empty()
            && result.conflicts().is_empty()
            && result.ignored().is_empty()
    );
    assert_eq!(result.identities().left(), b"");
    assert_eq!(result.counts().tracked(), 0);
    assert!(result.detail_cursor().is_none());
    assert_eq!(result.provenance().operation_reference(), None);
    assert_eq!(result.provenance().baseline_reference(), None);
}
