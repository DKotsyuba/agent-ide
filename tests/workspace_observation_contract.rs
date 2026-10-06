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
        store::{OperationId, Store, StoreError},
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
    let root = fs::canonicalize(root).unwrap();
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
        Err(ObservationError::TooLarge { size: 4 })
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
        RenameReconciliation::DeleteAndCreate { .. }
    ));
}

/// Full receipt capacity refuses new effects while exact observation retries remain recoverable.
#[tokio::test]
async fn receipt_exhaustion_preserves_exact_observation_recovery() {
    let root = temporary("receipt-root");
    fs::create_dir(&root).unwrap();
    let tree = worktree(&root);
    let database = temporary("receipt-database").with_extension("sqlite");
    let mut limited = config();
    limited.receipt_capacity = 8;
    let store = Store::open(&database, limited).unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let original = ObservationDraft::present(
        tree.clone(),
        1,
        operation("recoverable"),
        reference("recoverable"),
        "raw-path".into(),
        SourceBytes::from_bytes(b"one"),
        revision("revision-one"),
        SourceCoverage::Complete,
    )
    .unwrap();
    let ObservationAdmission::Recorded(recorded) =
        workspace.record(original.clone()).await.unwrap()
    else {
        panic!("initial observation must commit")
    };
    store
        .execute(operation("change-persisted-identity"), |tx| {
            tx.execute(
                "UPDATE workspace_source_observations SET source_revision='revision-two' WHERE operation_id='recoverable'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        workspace
            .freshness(operation("receipt-free-freshness"), &recorded)
            .await
            .unwrap(),
        ObservationFreshness::Stale,
        "same-sequence rows with different source identity are not current"
    );
    loop {
        let count = store
            .read_one(
                "SELECT count(*) FROM application_operation_receipts",
                vec![],
                |row| row.get::<_, usize>(0),
            )
            .await
            .unwrap()
            .unwrap();
        if count == limited.receipt_capacity {
            break;
        }
        store
            .execute(operation(&format!("receipt-fill-{count}")), |_| Ok(()))
            .await
            .unwrap();
    }
    assert_eq!(
        workspace.record(original).await.unwrap(),
        ObservationAdmission::Conflict
    );
    let exact = ObservationDraft::present(
        tree.clone(),
        1,
        operation("recoverable"),
        reference("recoverable"),
        "raw-path".into(),
        SourceBytes::from_bytes(b"one"),
        revision("revision-two"),
        SourceCoverage::Complete,
    )
    .unwrap();
    assert_eq!(
        workspace.record(exact).await.unwrap(),
        ObservationAdmission::AlreadyRecorded
    );
    let unrelated = ObservationDraft::present(
        tree,
        1,
        operation("new-effect"),
        reference("new-effect"),
        "other-path".into(),
        SourceBytes::from_bytes(b"two"),
        revision("revision-two"),
        SourceCoverage::Complete,
    )
    .unwrap();
    // A new observation needs no receipt, so it still records at an exhausted cap; tracked
    // operations stay refused there.
    assert!(matches!(
        workspace.record(unrelated).await.unwrap(),
        ObservationAdmission::Recorded(_)
    ));
    assert_eq!(
        store
            .execute(
                operation("tracked-at-cap"),
                |_| Ok::<_, rusqlite::Error>(())
            )
            .await
            .unwrap_err(),
        StoreError::ReceiptCapacityExhausted
    );
    drop(store);
    fs::remove_dir_all(root).unwrap();
    let _ = fs::remove_file(database);
}

/// Ignores stale/cross-worktree hints, never certifies a Boolean rename, and distinguishes root loss.
#[tokio::test]
async fn reconciliation_uses_latest_internal_state_and_rejects_root_absence() {
    use agent_ide::workspace::{observation::SourceChange, store::WorkspaceStoreError};
    let root = temporary("reconcile-root");
    let foreign_root = temporary("foreign-root");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&foreign_root).unwrap();
    let tree = worktree(&root);
    let foreign = worktree(&foreign_root);
    let database = temporary("reconcile-db").with_extension("sqlite");
    let store = Store::open(&database, config()).unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    fs::write(root.join("file"), b"one").unwrap();
    let ReconciliationAdmission::Fact(Some(SourceChange::Open(first))) = workspace
        .reconcile_registered_path(
            None,
            request(tree.clone(), "first", "first", "file".into(), "v1"),
        )
        .await
        .unwrap()
    else {
        panic!("first open")
    };
    fs::write(root.join("file"), b"two").unwrap();
    let ReconciliationAdmission::Fact(Some(SourceChange::Change {
        current: second, ..
    })) = workspace
        .reconcile_registered_path(
            None,
            request(tree.clone(), "second", "second", "file".into(), "v2"),
        )
        .await
        .unwrap()
    else {
        panic!("stored previous supplies change even without hint")
    };
    assert_eq!(
        workspace
            .reconcile_registered_path(
                Some(&first),
                request(tree.clone(), "third", "third", "file".into(), "v2")
            )
            .await
            .unwrap(),
        ReconciliationAdmission::Fact(None)
    );
    fs::write(foreign_root.join("file"), b"two").unwrap();
    assert!(matches!(
        workspace
            .reconcile_registered_path(
                Some(&second),
                request(foreign.clone(), "foreign", "foreign", "file".into(), "v2")
            )
            .await
            .unwrap(),
        ReconciliationAdmission::Fact(Some(SourceChange::Open(_)))
    ));
    fs::remove_file(root.join("file")).unwrap();
    let rename = workspace
        .reconcile_rename(
            Some(&first),
            request(tree.clone(), "old", "old", "file".into(), "v3"),
            Some(&first),
            request(foreign, "new", "new", "file".into(), "v2"),
            true,
        )
        .await
        .unwrap();
    assert!(
        matches!(rename, RenameReconciliation::DeleteAndCreate { old: ReconciliationAdmission::Fact(Some(SourceChange::Close { previous, .. })), new: ReconciliationAdmission::Fact(None) } if previous.sequence() == 3)
    );
    fs::write(root.join("still-present"), b"present").unwrap();
    workspace
        .reconcile_registered_path(
            None,
            request(
                tree.clone(),
                "present",
                "present",
                "still-present".into(),
                "v1",
            ),
        )
        .await
        .unwrap();
    let moved = temporary("moved-root");
    fs::rename(&root, &moved).unwrap();
    assert_eq!(
        workspace
            .reconcile_registered_path(
                None,
                request(
                    tree.clone(),
                    "root-gone",
                    "root-gone",
                    "still-present".into(),
                    "v2"
                )
            )
            .await,
        Err(WorkspaceStoreError::Observation(
            ObservationError::RootUnavailable
        ))
    );
    let latest = workspace
        .load_latest(
            operation("root-check"),
            tree.clone(),
            "still-present".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.source_revision().as_str(), "v1");
    fs::rename(&moved, &root).unwrap();
    assert_eq!(
        read_authorized_source(
            &tree,
            Path::new("missing/child"),
            SourceReadLimits::new(256, 64).unwrap()
        ),
        Err(ObservationError::Missing)
    );
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(foreign_root).unwrap();
}

/// Source reads reject every raw empty/dot/parent path component before inspecting descendants.
#[test]
fn raw_source_paths_never_normalize_aliases() {
    let root = temporary("raw-components");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join("nested")).unwrap();
    fs::write(root.join("nested/file"), b"data").unwrap();
    let tree = worktree(&fs::canonicalize(&root).unwrap());
    let limits = SourceReadLimits::new(256, 32).unwrap();
    for path in [
        "nested//file",
        "nested/./file",
        "nested/../nested/file",
        "nested/file/",
        "/nested/file",
        "nested/",
        "nested//",
    ] {
        assert_eq!(
            read_authorized_source(&tree, Path::new(path), limits),
            Err(ObservationError::InvalidPath),
            "raw path {path:?}"
        );
        assert!(
            RegisteredPathRequest::new(
                tree.clone(),
                1,
                operation("invalid"),
                reference("invalid"),
                path.into(),
                revision("invalid"),
                SourceCoverage::Complete,
                limits
            )
            .is_err()
        );
    }
    assert_eq!(
        read_authorized_source(&tree, Path::new("nested/file"), limits)
            .unwrap()
            .contents(),
        b"data"
    );
    fs::remove_dir_all(root).unwrap();
}

/// O_NOFOLLOW protects every ancestor of a root, not merely the root's final component.
#[test]
fn source_root_parent_symlink_is_not_followed() {
    let outer = temporary("root-parent-link");
    fs::create_dir(&outer).unwrap();
    fs::create_dir_all(outer.join("physical/root")).unwrap();
    fs::write(outer.join("physical/root/secret"), b"must not escape").unwrap();
    std::os::unix::fs::symlink("physical", outer.join("alias")).unwrap();
    let canonical = fs::canonicalize(&outer).unwrap();
    let tree = WorktreeRef::from_discovery(
        canonical.join("alias/root"),
        canonical.join("alias/root"),
        ".git".into(),
        1,
    )
    .unwrap();
    assert_eq!(
        read_authorized_source(
            &tree,
            Path::new("secret"),
            SourceReadLimits::new(256, 64).unwrap()
        ),
        Err(ObservationError::SymlinkEscape)
    );
    fs::remove_dir_all(outer).unwrap();
}

/// A replaced durable root is rejected before reading bytes or treating absent descendants as missing.
#[tokio::test]
async fn durable_root_replacement_cannot_reuse_source_authority() {
    use agent_ide::workspace::durable::DurableWorkspace;
    let outer = temporary("durable-source-root");
    fs::create_dir(&outer).unwrap();
    let outer = fs::canonicalize(outer).unwrap();
    let root = outer.join("root");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("file"), b"original").unwrap();
    let store = Store::open_with_backup_root(
        &outer.join("state.sqlite"),
        &outer.join("backups"),
        config(),
    )
    .unwrap();
    let durable = DurableWorkspace::open(&store).await.unwrap();
    let tree = durable
        .resolve_worktree(root.clone(), root.clone(), ".git".into())
        .await
        .unwrap();
    let limits = SourceReadLimits::new(256, 64).unwrap();
    assert_eq!(
        read_authorized_source(&tree, Path::new("file"), limits)
            .unwrap()
            .contents(),
        b"original"
    );
    let observations = WorkspaceStore::new(&store);
    assert!(matches!(
        observations.install_schema().await.unwrap(),
        agent_ide::app::store::MigrationAdmission::Applied { .. }
    ));
    let ObservationAdmission::Recorded(recorded) = observations
        .record(
            ObservationDraft::present(
                tree.clone(),
                1,
                operation("durable-proof"),
                reference("durable-proof"),
                "file".into(),
                SourceBytes::from_bytes(b"original"),
                revision("durable-proof"),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("observation recorded")
    };
    let reloaded = observations
        .load_latest(
            operation("reload-durable-proof"),
            tree.clone(),
            "file".into(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reloaded, recorded,
        "reload retains the supplied durable WorktreeRef proof; it does not reconstruct it from path strings"
    );
    fs::rename(&root, outer.join("retained-old-root")).unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::write(root.join("file"), b"replacement").unwrap();
    for path in ["file", "absent"] {
        assert_eq!(
            read_authorized_source(&tree, Path::new(path), limits),
            Err(ObservationError::RootIdentityChanged)
        );
    }
    drop(store);
    fs::remove_dir_all(outer).unwrap();
}

/// Source observations take no Application receipt: many more reads than the hard receipt cap
/// all record, a repeated operation still answers `AlreadyRecorded`, and tracked operations keep
/// their whole budget (a field daemon refused every durable write once reads filled the cap).
#[tokio::test]
async fn source_observations_never_consume_the_receipt_cap() {
    let root = temporary("receipt-free-root");
    fs::create_dir(&root).unwrap();
    let tree = worktree(&root);
    let database = temporary("receipt-free-database").with_extension("sqlite");
    let store = Store::open(
        &database,
        StoreConfig {
            receipt_capacity: 8,
            ..config()
        },
    )
    .unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let draft = |n: usize| {
        ObservationDraft::present(
            tree.clone(),
            1,
            operation(&format!("source-{n}")),
            reference(&format!("source-{n}")),
            PathBuf::from(format!("file-{}.txt", n % 3)),
            SourceBytes::from_bytes(format!("bytes {n}").as_bytes()),
            revision(&format!("rev-{n}")),
            SourceCoverage::Complete,
        )
        .unwrap()
    };
    for n in 0..64 {
        assert!(
            matches!(
                workspace.record(draft(n)).await.unwrap(),
                ObservationAdmission::Recorded(_)
            ),
            "observation {n}"
        );
    }
    assert_eq!(
        workspace.record(draft(7)).await.unwrap(),
        ObservationAdmission::AlreadyRecorded
    );
    store
        .execute(operation("tracked-after-reads"), |_| {
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    drop(store);
    fs::remove_dir_all(root).unwrap();
    let _ = fs::remove_file(database);
}
