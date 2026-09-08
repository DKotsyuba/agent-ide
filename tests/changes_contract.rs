//! Typed snapshot composition contract checks using the real Workspace/Execution boundary.
#[path = "support/git_snapshot.rs"]
mod support;

use agent_ide::{
    changes::{DiffCoverage, DiffFreshness, DiffResultState, DiffSelectionBudget, compose_diff},
    workspace::git::{
        DiffMode, GitComparison, GitError, GitIdentity, GitObjectId, GitReadIntent, GitReadQuery,
        GitScope, MAX_GIT_STDERR_BYTES, MAX_GIT_STDOUT_BYTES, RawGitEvidence,
    },
};
use support::{GIT, GitFixture, Runner, authority_for, capture_with_authority, collect};

/// A different mode/root/epoch/identity yields no payload or references, and old epochs are stale.
#[tokio::test]
async fn composition_fences_scope_and_exact_comparison() {
    let fixture = GitFixture::new();
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    let comparison = snapshot.comparison().clone();
    let other = GitFixture::new();
    for scope in [
        GitScope::from_authority(&authority_for(&other), DiffMode::Head),
        GitScope::from_authority(&authority_for(&fixture), DiffMode::Staged),
    ] {
        let result = compose_diff(
            &scope,
            &comparison,
            snapshot.clone(),
            DiffSelectionBudget::default(),
        );
        assert_eq!(result.state(), DiffResultState::Unavailable);
        assert_eq!(result.coverage(), DiffCoverage::Unknown);
        assert!(result.selected_hunks().is_empty());
        assert!(result.tracked().is_empty());
        assert!(result.provenance().operation_reference().is_none());
    }
    let mismatched = GitComparison::new(
        snapshot.scope().clone(),
        GitIdentity::new(b"wrong".to_vec()).unwrap(),
        comparison.right().clone(),
        comparison.baseline().clone(),
    );
    let result = compose_diff(
        snapshot.scope(),
        &mismatched,
        snapshot.clone(),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Unavailable);
}

/// Byte and count budgets preserve raw paths and exact unsplit hunks plus owner-scoped cursors.
#[tokio::test]
async fn bounded_selection_keeps_explicit_raw_paths() {
    let fixture = GitFixture::new();
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    let scope = snapshot.scope().clone();
    let comparison = snapshot.comparison().clone();
    let full = compose_diff(
        &scope,
        &comparison,
        snapshot.clone(),
        DiffSelectionBudget::default(),
    );
    assert_eq!(full.state(), DiffResultState::Ready);
    assert_eq!(full.freshness(), DiffFreshness::Current);
    assert_eq!(full.selected_hunks().len(), 3);
    assert!(
        full.selected_hunks()
            .iter()
            .any(|hunk| hunk.path() == &std::path::PathBuf::from("special space\n-leading.txt"))
    );
    for budget in [
        DiffSelectionBudget::bounded(1, 65536),
        DiffSelectionBudget::bounded(32, 1),
        DiffSelectionBudget::bounded(0, 0),
    ] {
        let limited = compose_diff(&scope, &comparison, snapshot.clone(), budget);
        assert_eq!(limited.state(), DiffResultState::Incomplete);
        assert_eq!(limited.coverage(), DiffCoverage::Partial);
        assert!(limited.overflow_hunks() > 0);
        assert!(limited.overflow_bytes() > 0);
        assert_eq!(
            limited.detail_cursor().unwrap().operation_reference(),
            snapshot.operation_reference()
        );
        for hunk in limited.selected_hunks() {
            assert_eq!(hunk, &full.selected_hunks()[hunk.index()]);
        }
    }
}

/// Empty tracked comparisons remain distinct from untracked paths and descriptive baseline coverage.
#[tokio::test]
async fn empty_snapshot_never_hides_untracked_or_invents_complete_baseline() {
    let fixture = GitFixture::new();
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "clean baseline"]);
    fixture.write(b"only-untracked", b"untracked");
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    let comparison = snapshot.comparison().clone();
    let scope = snapshot.scope().clone();
    let result = compose_diff(
        &scope,
        &comparison,
        snapshot,
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Ready);
    assert!(result.selected_hunks().is_empty());
    assert_eq!(result.untracked().len(), 1);
    assert_eq!(
        result.provenance().baseline_coverage(),
        Some(agent_ide::workspace::git::BaselineCoverage::Partial)
    );
}

/// Immutable OIDs are full 40/64 hex only; old repository-diff intents cannot be constructed.
#[test]
fn object_and_raw_evidence_boundaries_reject_ambiguous_inputs() {
    for value in [
        b"HEAD".as_slice(),
        b"--filters",
        b"abcd",
        &[b'g'; 40],
        &[b'a'; 63],
    ] {
        assert_eq!(GitObjectId::parse(value), Err(GitError::InvalidIdentity));
    }
    assert!(GitObjectId::parse(&[b'a'; 40]).unwrap().is_some());
    assert!(GitObjectId::parse(&[b'F'; 64]).unwrap().is_some());
    assert!(GitObjectId::parse(&[b'0'; 40]).unwrap().is_none());
    let fixture = GitFixture::new();
    let authority = authority_for(&fixture);
    for query in [
        GitReadQuery::Status,
        GitReadQuery::HeadDiff,
        GitReadQuery::StagedDiff,
        GitReadQuery::UnstagedDiff,
    ] {
        assert!(matches!(
            GitReadIntent::new(&authority, GIT.into(), query),
            Err(GitError::SnapshotRequired)
        ));
    }
    let scope = GitScope::from_authority(&authority, DiffMode::Head);
    for (stdout, stderr) in [
        (vec![0; MAX_GIT_STDOUT_BYTES + 1], vec![]),
        (vec![], vec![0; MAX_GIT_STDERR_BYTES + 1]),
    ] {
        assert_eq!(
            RawGitEvidence::new(
                "over",
                scope.clone(),
                GitReadQuery::Status,
                stdout,
                stderr,
                Some(0),
                false,
                false
            ),
            Err(GitError::EvidenceTooLarge)
        );
    }
}

/// Staged reads deduplicate immutable left OIDs and never claim a working source revision.
#[tokio::test]
async fn object_deduplication_and_mode_side_identity_are_exact() {
    let fixture = GitFixture::new();
    let authority = authority_for(&fixture);
    let mut runner = Runner::default();
    let staged = capture_with_authority(&authority, DiffMode::Staged, &mut runner)
        .await
        .unwrap();
    assert_eq!(
        runner.blobs, 3,
        "both paths share one base blob and have distinct staged blobs"
    );
    assert!(staged.paths().iter().all(|path| path.source().is_none()));
    let unstaged = capture_with_authority(&authority, DiffMode::Unstaged, &mut Runner::default())
        .await
        .unwrap();
    let head = capture_with_authority(&authority, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert_eq!(staged.comparison().left(), head.comparison().left());
    assert_eq!(staged.comparison().right(), unstaged.comparison().left());
    for path in head.paths() {
        assert_eq!(path.scope(), head.scope());
        assert_eq!(path.generation(), head.generation());
        assert!(path.source().unwrap().bytes().is_some());
    }
}
