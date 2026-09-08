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
    BaselineContext, BaselineCoverage, DiffMode, GitComparison, GitIdentity, GitReadQuery,
    GitScope, GitStatus, RawGitEvidence, StatusKind,
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

/// Builds a current scope at the default isolated contract root.
fn scope_from_mode(mode: DiffMode) -> GitScope {
    scope_for_root(mode, "/private/tmp/changes-contract")
}

/// Activates an isolated registry for one exact fixture root and comparison mode.
fn scope_for_root(mode: DiffMode, root: &str) -> GitScope {
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
            PathBuf::from(root),
            PathBuf::from(root),
            PathBuf::from(root).join(".git"),
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

/// Binds fixed test identities to the requested fixture mode and full scope.
fn comparison(mode: DiffMode) -> GitComparison {
    GitComparison::new(
        scope_from_mode(mode),
        GitIdentity::new(b"left-id".to_vec()).expect("left identity is valid"),
        GitIdentity::new(b"right-id".to_vec()).expect("right identity is valid"),
        BaselineContext::new("baseline", BaselineCoverage::Complete)
            .expect("baseline context is bounded"),
    )
}

/// Captures bounded patch evidence retaining its mode-specific query provenance.
fn make_evidence(
    scope: &GitScope,
    stdout: &[u8],
    exit_code: Option<i32>,
    truncated: bool,
) -> RawGitEvidence {
    RawGitEvidence::new(
        "operation-1",
        scope.clone(),
        GitReadQuery::diff_for(scope.mode()),
        stdout.to_vec(),
        Vec::new(),
        exit_code,
        truncated,
        false,
    )
    .expect("evidence has bounded metadata")
}

/// Parses complete scoped status evidence for one tracked, conflicted, and untracked path.
fn status_fixture() -> agent_ide::workspace::git::GitStatus {
    GitStatus::from_evidence(&RawGitEvidence::new(
        "status", scope_from_mode(DiffMode::Head), GitReadQuery::Status,
        b"1 M. N... 100644 100644 100644 a b tracked\0u UU N... 100644 100644 100644 100644 a b c conflict\0? -leading\npath\0".to_vec(),
        Vec::new(), Some(0), false, false,
    ).unwrap())
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
        make_evidence(&scope, b"diff --git a/tracked b/tracked\n", Some(0), false),
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
            b"diff --git a/tracked b/tracked\n@@ -1 +1 @@\n-old\n+new\n",
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
            b"diff --git a/tracked b/tracked\n@@ -1 +1 @@\n-old\n+new\nmalformed-tail\n",
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
            b"diff --git a/tracked b/tracked\nBinary files a/image.bin and b/image.bin differ\n",
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
    let comparison = GitComparison::new(
        scope.clone(),
        GitIdentity::new(b"left-id".to_vec()).unwrap(),
        GitIdentity::new(b"right-id".to_vec()).unwrap(),
        BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
    );

    let result = compose_diff(
        &scope,
        &comparison,
        status_fixture(),
        make_evidence(&scope, b"", Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(
        result.provenance().baseline_coverage(),
        Some(BaselineCoverage::Partial)
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
    let diff = b"diff --git a/tracked b/tracked\n@@ -1,1 +1,1 @@\nline\nline\nline\nline\nline\n";
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
            b"diff --git a/tracked b/tracked\n@@ -1 +1 @@\n-old\n+new\nBinary files a/x and b/x differ\nbad-tail\n",
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
fn path_absent_from_status_is_incomplete_without_guessed_hunks() {
    let scope = scope_from_mode(DiffMode::Head);
    let patch = b"diff --git a/with space.txt b/with space.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let result = compose_diff(
        &scope,
        &comparison(DiffMode::Head),
        status_fixture(),
        make_evidence(&scope, patch, Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Incomplete);
    assert!(result.selected_hunks().is_empty());
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
    let baseline = BaselineContext::new("baseline", BaselineCoverage::Complete).unwrap();
    let identities = |scope| {
        GitComparison::new(
            scope,
            GitIdentity::new(b"left".to_vec()).unwrap(),
            GitIdentity::new(b"right".to_vec()).unwrap(),
            baseline.clone(),
        )
    };
    let status = |scope| {
        GitStatus::from_evidence(
            &RawGitEvidence::new(
                "status",
                scope,
                GitReadQuery::Status,
                vec![],
                vec![],
                Some(0),
                false,
                false,
            )
            .unwrap(),
        )
        .unwrap()
    };
    for (comparison, status) in [
        (identities(old_scope.clone()), status(expected.clone())),
        (identities(expected.clone()), status(old_scope.clone())),
    ] {
        assert_eq!(
            compose_diff(
                &expected,
                &comparison,
                status,
                make_evidence(&expected, b"", Some(0), false),
                DiffSelectionBudget::default()
            )
            .state(),
            DiffResultState::Unavailable
        );
    }
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

/// Rejects scopes on every component and preserves fixed-query roles and hard stream ceilings.
#[test]
fn comparison_status_query_and_capture_boundaries_are_enforced() {
    use agent_ide::workspace::git::{
        GitError, MAX_GIT_STDERR_BYTES, MAX_GIT_STDOUT_BYTES, comparison_from_evidence,
    };
    let scope = scope_from_mode(DiffMode::Head);
    let foreign = scope_for_root(DiffMode::Head, "/private/tmp/foreign-changes");
    let identities = |scope: GitScope| {
        GitComparison::new(
            scope,
            GitIdentity::new(b"left".to_vec()).unwrap(),
            GitIdentity::new(b"right".to_vec()).unwrap(),
            BaselineContext::new("baseline", BaselineCoverage::Complete).unwrap(),
        )
    };
    let capture = |scope: GitScope, query, stdout: Vec<u8>, stderr: Vec<u8>| {
        RawGitEvidence::new(
            "capture",
            scope,
            query,
            stdout,
            stderr,
            Some(0),
            false,
            false,
        )
    };
    let foreign_status = GitStatus::from_evidence(
        &capture(foreign.clone(), GitReadQuery::Status, vec![], vec![]).unwrap(),
    )
    .unwrap();
    for (comparison, status) in [
        (identities(foreign), status_fixture()),
        (identities(scope.clone()), foreign_status),
    ] {
        let result = compose_diff(
            &scope,
            &comparison,
            status,
            make_evidence(&scope, b"", Some(0), false),
            DiffSelectionBudget::default(),
        );
        assert_eq!(result.state(), DiffResultState::Unavailable);
        assert!(result.selected_hunks().is_empty());
        assert!(result.identities().left().is_empty());
    }
    let wrong_query = capture(
        scope.clone(),
        GitReadQuery::HeadIdentity,
        b"not a patch".to_vec(),
        vec![],
    )
    .unwrap();
    assert_eq!(
        compose_diff(
            &scope,
            &identities(scope.clone()),
            status_fixture(),
            wrong_query.clone(),
            DiffSelectionBudget::default()
        )
        .state(),
        DiffResultState::Unavailable
    );
    let index = capture(
        scope_from_mode(DiffMode::Staged),
        GitReadQuery::IndexState,
        vec![],
        vec![],
    )
    .unwrap();
    let patch = make_evidence(&scope, b"", Some(0), false);
    assert_eq!(
        comparison_from_evidence(
            DiffMode::Head,
            &patch,
            &index,
            &wrong_query,
            BaselineContext::new("baseline", BaselineCoverage::Complete).unwrap()
        ),
        Err(GitError::IncompleteIdentity)
    );
    assert_eq!(
        capture(
            scope.clone(),
            GitReadQuery::HeadDiff,
            vec![0; MAX_GIT_STDOUT_BYTES + 1],
            vec![]
        ),
        Err(GitError::EvidenceTooLarge)
    );
    assert_eq!(
        capture(
            scope,
            GitReadQuery::HeadDiff,
            vec![],
            vec![0; MAX_GIT_STDERR_BYTES + 1]
        ),
        Err(GitError::EvidenceTooLarge)
    );
}

/// Attributes multiple hunks to exact space, newline, and non-UTF-8 status paths without guessing.
#[test]
fn multiple_files_keep_exact_raw_path_association() {
    use std::os::unix::ffi::OsStrExt;
    let scope = scope_from_mode(DiffMode::Head);
    let status = GitStatus::from_evidence(&RawGitEvidence::new("status", scope.clone(), GitReadQuery::Status,
        b"1 M. N... 100644 100644 100644 a b with space.txt\x001 M. N... 100644 100644 100644 a b line\nraw-\xff\0".to_vec(), vec![], Some(0), false, false).unwrap()).unwrap();
    let patch = b"diff --git a/with space.txt b/with space.txt\n@@ -1 +1 @@\n-old\n+space\ndiff --git \"a/line\\nraw-\\377\" \"b/line\\nraw-\\377\"\n@@ -1 +1 @@\n-old\n+raw\n";
    let result = compose_diff(
        &scope,
        &comparison(DiffMode::Head),
        status,
        make_evidence(&scope, patch, Some(0), false),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Ready);
    assert_eq!(result.selected_hunks().len(), 2);
    assert_eq!(
        result.selected_hunks()[0]
            .path()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        b"with space.txt"
    );
    assert_eq!(
        result.selected_hunks()[1]
            .path()
            .unwrap()
            .as_os_str()
            .as_bytes(),
        b"line\nraw-\xff"
    );
    assert!(result.selected_hunks()[0].patch().ends_with(b"+space\n"));
    assert!(result.selected_hunks()[1].patch().ends_with(b"+raw\n"));
}
