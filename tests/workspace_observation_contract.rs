//! Contract checks for Workspace durable source observations and bounded native reads.

use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::{
    app::{
        config::StoreConfig,
        store::{OperationId, Store},
    },
    workspace::{
        authority::WorktreeRef,
        observation::{
            ObservationError, ObservationRef, SourceBytes, SourceCoverage, SourceReadLimits,
            SourceRevision, read_authorized_source,
        },
        store::{
            ObservationAdmission, ObservationDraft, ObservationFreshness, ReconciliationAdmission,
            RegisteredPathRequest, RenameReconciliation, WorkspaceStore,
        },
    },
};

/// Separates temporary database and worktree paths within this process.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Returns one unique absolute temporary path without creating it.
fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-observation-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Returns bounded Application mechanics settings for this isolated contract test.
fn config() -> StoreConfig {
    StoreConfig {
        queue_capacity: 32,
        busy_timeout: Duration::from_millis(100),
        request_deadline: Duration::from_secs(1),
        receipt_capacity: 128,
    }
}

/// Builds a canonical WorktreeRef rooted at the supplied test directory.
fn worktree(root: &Path) -> WorktreeRef {
    WorktreeRef::from_discovery(
        root.to_path_buf(),
        root.to_path_buf(),
        PathBuf::from(".git"),
        1,
    )
    .unwrap()
}

/// Builds one short stable Application operation identifier.
fn operation(value: &str) -> OperationId {
    OperationId::new(value).unwrap()
}

/// Builds one opaque source revision that cannot be a Git identity by type.
fn revision(value: &str) -> SourceRevision {
    SourceRevision::new(value).unwrap()
}

/// Builds one opaque observation correlation reference.
fn reference(value: &str) -> ObservationRef {
    ObservationRef::new(value).unwrap()
}

/// Builds an exact registered-path polling request with complete coverage.
fn request(
    worktree: WorktreeRef,
    operation_id: &str,
    reference_id: &str,
    path: PathBuf,
    revision_id: &str,
) -> RegisteredPathRequest {
    RegisteredPathRequest::new(
        worktree,
        1,
        operation(operation_id),
        reference(reference_id),
        path,
        revision(revision_id),
        SourceCoverage::Complete,
        SourceReadLimits::new(256, 64).unwrap(),
    )
    .unwrap()
}

/// Proves durable reopen, idempotent stable operations, source freshness, byte/path bounds, and explicit reconciliation facts.
#[tokio::test]
async fn workspace_source_observations_are_durable_bounded_and_honest() {
    let root = temporary("root");
    fs::create_dir(&root).unwrap();
    let tree = worktree(&root);
    let database = temporary("database").with_extension("sqlite");
    let store = Store::open(&database, config()).unwrap();
    let workspace = WorkspaceStore::new(&store);
    assert!(matches!(
        workspace.install_schema().await.unwrap(),
        agent_ide::app::store::MigrationAdmission::Applied { .. }
    ));

    let path = PathBuf::from("tracked.txt");
    fs::write(root.join(&path), b"one").unwrap();
    let first = match workspace
        .record(
            ObservationDraft::present(
                tree.clone(),
                1,
                operation("first"),
                reference("first"),
                path.clone(),
                SourceBytes::from_bytes(b"one"),
                revision("source-r1"),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(value) => value,
        other => panic!("expected durable observation, got {other:?}"),
    };
    assert_eq!(first.sequence(), 1);
    assert_eq!(
        workspace
            .record(
                ObservationDraft::present(
                    tree.clone(),
                    1,
                    operation("first"),
                    reference("first"),
                    path.clone(),
                    SourceBytes::from_bytes(b"one"),
                    revision("source-r1"),
                    SourceCoverage::Complete
                )
                .unwrap()
            )
            .await
            .unwrap(),
        ObservationAdmission::AlreadyRecorded
    );
    assert_eq!(
        workspace
            .record(
                ObservationDraft::present(
                    tree.clone(),
                    1,
                    operation("first"),
                    reference("first"),
                    PathBuf::from("different.txt"),
                    SourceBytes::from_bytes(b"different"),
                    revision("source-r1"),
                    SourceCoverage::Complete,
                )
                .unwrap(),
            )
            .await
            .unwrap(),
        ObservationAdmission::Conflict
    );
    assert_eq!(
        workspace
            .freshness(operation("fresh-first"), &first)
            .await
            .unwrap(),
        ObservationFreshness::Current
    );

    let second = match workspace
        .record(
            ObservationDraft::present(
                tree.clone(),
                1,
                operation("second"),
                reference("second"),
                path.clone(),
                SourceBytes::from_bytes(b"two"),
                revision("source-r2"),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(value) => value,
        other => panic!("expected later observation, got {other:?}"),
    };
    assert!(second.sequence() > first.sequence());
    assert_eq!(
        workspace
            .freshness(operation("fresh-stale"), &first)
            .await
            .unwrap(),
        ObservationFreshness::Stale
    );
    let partial = match workspace
        .record(
            ObservationDraft::present(
                tree.clone(),
                1,
                operation("partial"),
                reference("partial"),
                path.clone(),
                SourceBytes::from_bytes(b"three"),
                revision("source-r3"),
                SourceCoverage::Partial,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(value) => value,
        other => panic!("expected partial observation, got {other:?}"),
    };
    assert_eq!(
        workspace
            .freshness(operation("fresh-partial"), &partial)
            .await
            .unwrap(),
        ObservationFreshness::Incomplete
    );
    drop(store);

    let reopened = Store::open(&database, config()).unwrap();
    let workspace = WorkspaceStore::new(&reopened);
    assert!(matches!(
        workspace.install_schema().await.unwrap(),
        agent_ide::app::store::MigrationAdmission::AlreadyApplied { .. }
    ));
    let restored = workspace
        .load_latest(operation("load-reopened"), tree.clone(), path.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.sequence(), partial.sequence());
    assert_eq!(restored.source_revision().as_str(), "source-r3");
    assert_ne!(restored.source_revision().as_str().as_bytes(), b"HEAD");

    let limits = SourceReadLimits::new(256, 3).unwrap();
    assert_eq!(
        read_authorized_source(&tree, Path::new("tracked.txt"), limits)
            .unwrap()
            .contents(),
        b"one"
    );
    assert_eq!(
        read_authorized_source(&tree, Path::new("/absolute"), limits),
        Err(ObservationError::InvalidPath)
    );
    assert_eq!(
        read_authorized_source(&tree, Path::new(""), limits),
        Err(ObservationError::InvalidPath)
    );
    assert_eq!(
        read_authorized_source(&tree, Path::new("./tracked.txt"), limits),
        Err(ObservationError::InvalidPath)
    );
    assert_eq!(
        read_authorized_source(&tree, Path::new("../tracked.txt"), limits),
        Err(ObservationError::InvalidPath)
    );
    fs::write(root.join("large"), b"four").unwrap();
    assert_eq!(
        read_authorized_source(&tree, Path::new("large"), limits),
        Err(ObservationError::TooLarge)
    );
    let raw = PathBuf::from(OsString::from_vec(b"non-utf8-\xFF".to_vec()));
    // APFS rejects this spelling, while common Unix filesystems preserve the raw byte.
    if fs::write(root.join(&raw), b"x").is_ok() {
        assert_eq!(
            read_authorized_source(&tree, &raw, limits)
                .unwrap()
                .contents(),
            b"x"
        );
    }
    let outside = temporary("outside");
    fs::write(&outside, b"x").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    assert_eq!(
        read_authorized_source(&tree, Path::new("escape"), limits),
        Err(ObservationError::SymlinkEscape)
    );

    let edited = PathBuf::from("edited");
    fs::write(root.join(&edited), b"a").unwrap();
    let opened = match workspace
        .reconcile_registered_path(
            None,
            request(
                tree.clone(),
                "edit-open",
                "edit-open",
                edited.clone(),
                "source-edit-1",
            ),
        )
        .await
        .unwrap()
    {
        ReconciliationAdmission::Fact(Some(
            agent_ide::workspace::observation::SourceChange::Open(value),
        )) => value,
        other => panic!("expected open fact, got {other:?}"),
    };
    fs::write(root.join(&edited), b"b").unwrap();
    let changed = match workspace
        .reconcile_registered_path(
            Some(&opened),
            request(
                tree.clone(),
                "edit-change",
                "edit-change",
                edited.clone(),
                "source-edit-2",
            ),
        )
        .await
        .unwrap()
    {
        ReconciliationAdmission::Fact(Some(
            agent_ide::workspace::observation::SourceChange::Change { current, .. },
        )) => current,
        other => panic!("expected change fact, got {other:?}"),
    };
    fs::remove_file(root.join(&edited)).unwrap();
    assert!(matches!(
        workspace
            .reconcile_registered_path(
                Some(&changed),
                request(
                    tree.clone(),
                    "edit-delete",
                    "edit-delete",
                    edited.clone(),
                    "source-edit-3"
                )
            )
            .await
            .unwrap(),
        ReconciliationAdmission::Fact(Some(
            agent_ide::workspace::observation::SourceChange::Close { .. }
        ))
    ));

    let old = PathBuf::from("old-name");
    let new = PathBuf::from("new-name");
    fs::write(root.join(&old), b"rename").unwrap();
    let old_observation = match workspace
        .reconcile_registered_path(
            None,
            request(
                tree.clone(),
                "rename-open",
                "rename-open",
                old.clone(),
                "source-rename-1",
            ),
        )
        .await
        .unwrap()
    {
        ReconciliationAdmission::Fact(Some(
            agent_ide::workspace::observation::SourceChange::Open(value),
        )) => value,
        other => panic!("expected old open, got {other:?}"),
    };
    fs::rename(root.join(&old), root.join(&new)).unwrap();
    assert!(matches!(
        workspace
            .reconcile_rename(
                Some(&old_observation),
                request(
                    tree.clone(),
                    "rename-old",
                    "rename-old",
                    old,
                    "source-rename-2"
                ),
                None,
                request(tree, "rename-new", "rename-new", new, "source-rename-2"),
                true
            )
            .await
            .unwrap(),
        RenameReconciliation::Rename(
            agent_ide::workspace::observation::SourceChange::Rename { .. }
        )
    ));
}
