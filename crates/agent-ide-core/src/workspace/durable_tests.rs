//! Deterministic native lifecycle and baseline capture boundary regressions.

use super::*;
use crate::{
    app::config::StoreConfig,
    assistance::host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
    },
    workspace::git::{DiffMode, GitReadQuery, GitScope, RawGitEvidence},
};
use serde_json::json;
use std::time::Duration;

/// Selections survive a daemon reboot, auto deletes only its key, and a replacement root
/// receives a fresh incarnation with no inherited selection.
#[tokio::test]
async fn environment_roundtrip_auto_and_new_incarnation() {
    use crate::lang::{
        environment::{EnvSelection, replace_selections, selections},
        testing::ALPHA,
    };
    crate::lang::testing::install();
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let root = fixture.root();
    let tree = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    owner
        .set_environment(
            &tree,
            vec![
                (
                    ALPHA,
                    EnvSelection {
                        root: PathBuf::new(),
                        selector: "two".into(),
                    },
                ),
                (
                    ALPHA,
                    EnvSelection {
                        root: "nested".into(),
                        selector: "one".into(),
                    },
                ),
            ],
        )
        .await
        .unwrap();
    assert_eq!(selections(&root, ALPHA).len(), 2);
    replace_selections(&root, ALPHA, Vec::new());
    drop(owner);
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let rebooted = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    assert_eq!(rebooted.incarnation(), tree.incarnation());
    assert_eq!(selections(&root, ALPHA).len(), 2);
    owner
        .set_environment(
            &rebooted,
            vec![(
                ALPHA,
                EnvSelection {
                    root: PathBuf::new(),
                    selector: "auto".into(),
                },
            )],
        )
        .await
        .unwrap();
    assert_eq!(
        selections(&root, ALPHA),
        vec![EnvSelection {
            root: "nested".into(),
            selector: "one".into()
        }]
    );
    fixture.replace_root();
    let recreated = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    assert_ne!(recreated.incarnation(), tree.incarnation());
    assert!(selections(&root, ALPHA).is_empty());
}

/// A committed write whose acknowledgement was lost is read back without replaying the write.
#[tokio::test]
async fn review_environment_unknown_commit_rehydrates_selections() {
    crate::lang::testing::install();
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let root = fixture.root();
    let tree = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    let incarnation = signed(tree.incarnation()).unwrap();
    let op = OperationId::new("review-environment-unknown").unwrap();
    store.execute(op.clone(), move |tx| {
        tx.execute("INSERT INTO workspace_environment(incarnation,language,root,selector) VALUES (?1,'alpha','','two')", [incarnation])?;
        Ok(())
    }).await.unwrap();
    assert!(crate::lang::environment::selections(&root, crate::lang::testing::ALPHA).is_empty());
    owner
        .finish_environment_write(
            &tree,
            &op,
            Err(StoreError::OutcomeUnknown {
                operation: op.clone(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        crate::lang::environment::selections(&root, crate::lang::testing::ALPHA)[0].selector,
        "two"
    );
    let count = store
        .read_one(
            "SELECT count(*) FROM workspace_environment",
            Vec::new(),
            |row| row.get::<_, i64>(0),
        )
        .await
        .unwrap();
    assert_eq!(count, Some(1));
    crate::lang::environment::replace_selections(&root, crate::lang::testing::ALPHA, Vec::new());
}

/// Adding the environment migration to an existing v3 database preserves worktree/start rows.
#[tokio::test]
async fn review_environment_upgrade_preserves_v3_rows() {
    let fixture = Fixture::new();
    let store = fixture.store();
    for (key, source) in [
        ("identity_authority_baseline_v1", SQL),
        ("creation_identity_v2", IDENTITY_SQL),
        ("reader_starts_v3", READER_SQL),
    ] {
        let sql = TrustedUpSql::new(source).unwrap();
        let admission = store
            .admit_migration(DomainMigration {
                domain: DomainName::new("workspace").unwrap(),
                key: MigrationKey::new(key).unwrap(),
                expected_digest: MigrationDigest::from_sql(&sql),
                up_sql: sql,
            })
            .await
            .unwrap();
        assert!(matches!(admission, MigrationAdmission::Applied { .. }));
    }
    let root = fixture.root().as_os_str().as_bytes().to_vec();
    store.execute(OperationId::new("review-v3-seed").unwrap(), move |tx| {
        tx.execute("INSERT INTO workspace_worktrees(incarnation,physical_key,native_key,root,repository,common_dir,nonce,root_object,root_identity) VALUES (7,?1,?1,?2,?2,?2,?1,?1,?1)", params![vec![7u8;32], root])?;
        tx.execute("INSERT INTO workspace_starts(operation,digest,incarnation,actor,binding,boot,epoch,outcome,active,role,started_ms) VALUES ('v3-start',?1,7,'v3-actor',?1,0,1,'granted',0,'reader',123)", [vec![7u8;32]])?;
        Ok(())
    }).await.unwrap();
    let before = store
        .read_one(
            "SELECT count(*) FROM sqlite_master WHERE name='workspace_environment'",
            Vec::new(),
            |row| row.get::<_, i64>(0),
        )
        .await
        .unwrap();
    assert_eq!(before, Some(0));
    let _owner = DurableWorkspace::open(&store).await.unwrap();
    let preserved = store.read_one("SELECT incarnation,actor,role,started_ms FROM workspace_starts WHERE operation='v3-start'", Vec::new(), |row| Ok((row.get::<_,i64>(0)?, row.get::<_,String>(1)?, row.get::<_,String>(2)?, row.get::<_,i64>(3)?))).await.unwrap();
    assert_eq!(
        preserved,
        Some((7, "v3-actor".into(), "reader".into(), 123))
    );
    assert_eq!(
        store
            .read_one(
                "SELECT count(*) FROM workspace_worktrees WHERE incarnation=7",
                Vec::new(),
                |row| row.get::<_, i64>(0)
            )
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        store
            .read_one(
                "SELECT count(*) FROM workspace_environment",
                Vec::new(),
                |row| row.get::<_, i64>(0)
            )
            .await
            .unwrap(),
        Some(0)
    );
}

/// Keeps one collision-resistant native test tree and its database under a nonsymlinked temporary root.
struct Fixture(PathBuf);
impl Fixture {
    /// Creates isolated native directories and one registered source file.
    fn new() -> Self {
        let path = PathBuf::from("/private/tmp").join(format!(
            "durable-native-{}",
            blake3::Hash::from(random_nonce().unwrap()).to_hex()
        ));
        fs::create_dir_all(path.join("tree/.git")).unwrap();
        fs::write(path.join("tree/source"), b"authorized bytes").unwrap();
        Self(path)
    }
    /// Returns this fixture's real root.
    fn root(&self) -> PathBuf {
        self.0.join("tree")
    }
    /// Opens an isolated bounded store with migration backup admission.
    fn store(&self) -> Store {
        Store::open_with_backup_root(
            &self.0.join("state.sqlite"),
            &self.0.join("backups"),
            StoreConfig {
                queue_capacity: 16,
                busy_timeout: Duration::from_secs(1),
                request_deadline: Duration::from_secs(2),
                receipt_capacity: 128,
            },
        )
        .unwrap()
    }
    /// Replaces the root pathname with a different native directory carrying disallowed content.
    fn replace_root(&self) {
        fs::rename(self.root(), self.0.join("old-tree")).unwrap();
        fs::create_dir_all(self.root().join(".git")).unwrap();
        fs::write(
            self.root().join("source"),
            b"replacement must never be captured",
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    /// Removes only this fixture's collision-resistant owned directory.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Reproduces a root swap after live admission or after source reads, before capture commit.
async fn root_swap_at(checkpoint: CaptureCheckpoint) {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let root = fixture.root();
    let tree = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    let mut guard = HostBindingGuard::default();
    let channel = parse_channel_session(b"capture-race").unwrap();
    let hook = parse_hook_event(
        br#"{"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"capture"}"#,
    )
    .unwrap();
    assert!(matches!(
        guard.observe_hook(hook, channel.clone()),
        BindingStatus::PreObserved
    ));
    let candidate = parse_candidate(
        json!({"threadId":"actor","callId":"capture","x-codex-turn-metadata":{"turn":"race"}})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
        panic!("valid binding");
    };
    let receipt = owner
        .activate(
            ActivationRequest::new(
                "capture-race",
                false,
                invocation.clone(),
                guard.consume_active(invocation.binding_ref()).unwrap(),
                tree,
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let stamp = owner.authority(&receipt, &active).await.unwrap();
    let git = RawGitEvidence::new(
        "head",
        GitScope::from_authority(&stamp, DiffMode::Head),
        GitReadQuery::HeadIdentity,
        b"oid\n".to_vec(),
        vec![],
        Some(0),
        false,
        false,
    )
    .unwrap();
    let id = OperationId::new("raced-baseline").unwrap();
    assert_eq!(
        owner
            .capture_baseline_inner(
                id.clone(),
                &stamp,
                &active,
                vec![git],
                vec!["source".into()],
                |stage| {
                    if stage == checkpoint {
                        fixture.replace_root();
                    }
                }
            )
            .await,
        Err(DurableError::IdentityUnavailable {
            step: "identity_read",
            holder: None,
        })
    );
    assert_eq!(owner.committed_baseline(&id).await.unwrap(), None);
    assert_eq!(
        store
            .read_one("SELECT count(*) FROM workspace_baselines", vec![], |row| {
                row.get::<_, i64>(0)
            })
            .await
            .unwrap(),
        Some(0)
    );
}

/// Refuses replacement bytes when the root pathname changes after authorization but before opening source.
#[tokio::test]
async fn baseline_root_swap_before_source_open_is_not_captured() {
    root_swap_at(CaptureCheckpoint::BeforeSourceRead).await;
}

/// Refuses baseline commit when the native root changes after authorized source bytes were read.
#[tokio::test]
async fn baseline_root_swap_after_source_read_is_not_committed() {
    root_swap_at(CaptureCheckpoint::BeforeCommit).await;
}

/// Keeps all three native directories open, including across owner clones and path replacement.
#[tokio::test]
async fn admitted_directory_descriptors_remain_held_by_the_owner() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let root = fixture.root();
    let tree = owner
        .resolve_worktree(root.clone(), root.clone(), root.join(".git"))
        .await
        .unwrap();
    let clone = owner.clone();
    drop(owner);
    fixture.replace_root();
    let held = clone.identities.lock().unwrap();
    let (_, native) = held.get(&tree.incarnation()).unwrap();
    assert_eq!(
        super::super::observation::native_directory_identity(&native._directories[0]).unwrap(),
        tree.native_root_identity.unwrap()
    );
    assert_eq!(native._directories.len(), 3);
    for directory in &native._directories {
        assert!(directory.metadata().unwrap().is_dir());
    }
    assert_ne!(
        NativeIdentity::read(&root, &root, &root.join(".git"))
            .unwrap()
            .key,
        native.key
    );
}

/// A plain directory resolves with root, repository and common dir as one path: the native key
/// stays stable across resolutions and only recreating the directory mints a new incarnation.
#[tokio::test]
async fn plain_directory_identity_is_stable_until_recreation() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let plain = fixture.0.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let first = owner
        .resolve_worktree(plain.clone(), plain.clone(), plain.clone())
        .await
        .unwrap();
    assert!(first.is_plain_directory());
    let second = owner
        .resolve_worktree(plain.clone(), plain.clone(), plain.clone())
        .await
        .unwrap();
    assert_eq!(first.id(), second.id());
    assert_eq!(first.incarnation(), second.incarnation());
    fs::remove_dir_all(&plain).unwrap();
    fs::create_dir_all(&plain).unwrap();
    let third = owner
        .resolve_worktree(plain.clone(), plain.clone(), plain.clone())
        .await
        .unwrap();
    assert!(third.is_plain_directory());
    assert!(third.incarnation() > second.incarnation());
    assert_ne!(third.id(), second.id());
}

/// Simulates reusable device/inode numbers and verifies that creation time participates at nanosecond precision.
#[test]
fn creation_identity_rejects_reused_inodes_and_unavailable_birthtime() {
    use super::super::observation::{ObservationError, directory_identity};
    let first = directory_identity(7, 42, Duration::new(100, 1)).unwrap();
    assert_eq!(
        directory_identity(7, 42, Duration::new(100, 1)).unwrap(),
        first
    );
    assert_ne!(
        directory_identity(7, 42, Duration::new(100, 2)).unwrap(),
        first
    );
    assert_ne!(
        directory_identity(7, 42, Duration::new(101, 1)).unwrap(),
        first
    );
    assert_ne!(
        directory_identity(8, 42, Duration::new(100, 1)).unwrap(),
        first
    );
    assert_eq!(
        directory_identity(7, 42, Duration::ZERO),
        Err(ObservationError::RootUnavailable)
    );
}
