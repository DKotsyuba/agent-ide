//! Contract checks for T066's controlled freshness and cache-lifecycle substitute.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::{
    app::{
        cache::{CacheNamespaceId, CacheRoot},
        config::StoreConfig,
        store::{OperationId, Store},
    },
    intelligence::freshness::{
        CacheIdentity, CacheLifecycle, DiagnosticReadiness, DiagnosticReference, Freshness,
        SourceBinding, ViewFreshness, ViewGeneration,
    },
    workspace::{
        authority::WorktreeRef,
        durable::DurableWorkspace,
        observation::{ObservationRef, SourceBytes, SourceCoverage, SourceRevision},
        store::{ObservationAdmission, ObservationDraft, WorkspaceStore},
    },
};

/// Separates temporary source, database, and cache paths for this process.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Returns an uncreated unique temporary path for an isolated contract resource.
fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-freshness-{}-{}-{name}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Builds one canonical test worktree without discovering host Git state.
fn worktree(path: &Path) -> WorktreeRef {
    WorktreeRef::from_discovery(
        path.to_path_buf(),
        path.to_path_buf(),
        PathBuf::from(".git"),
        1,
    )
    .unwrap()
}

/// Builds bounded application storage settings for an isolated observation sequence.
fn config() -> StoreConfig {
    StoreConfig {
        queue_capacity: 8,
        busy_timeout: Duration::from_millis(100),
        request_deadline: Duration::from_secs(1),
        receipt_capacity: 8,
    }
}

/// Records one source observation so the tested binding uses the real Workspace type.
async fn observation(
    store: &WorkspaceStore<'_>,
    tree: WorktreeRef,
    operation: &str,
    reference: &str,
    sequence_value: &str,
    coverage: SourceCoverage,
) -> agent_ide::workspace::observation::SourceObservation {
    match store
        .record(
            ObservationDraft::present(
                tree,
                1,
                OperationId::new(operation).unwrap(),
                ObservationRef::new(reference).unwrap(),
                PathBuf::from("tracked.rs"),
                SourceBytes::from_bytes(sequence_value.as_bytes()),
                SourceRevision::new(format!("revision-{sequence_value}")).unwrap(),
                coverage,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    {
        ObservationAdmission::Recorded(value) => value,
        other => panic!("expected recorded observation, got {other:?}"),
    }
}

/// Proves every source and generation fence rejects late diagnostics, and absence or incomplete coverage never becomes clean.
#[tokio::test]
async fn freshness_fences_late_results_and_never_cleans_unknown_diagnostics() {
    let root = temporary("source");
    fs::create_dir(&root).unwrap();
    let database = temporary("database").with_extension("sqlite");
    let store = Store::open(&database, config()).unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let tree = worktree(&root);
    let first = observation(
        &workspace,
        tree.clone(),
        "first",
        "first",
        "one",
        SourceCoverage::Complete,
    )
    .await;
    let binding = SourceBinding::from_observation(&first);
    let generation = ViewGeneration {
        backend: 1,
        configuration: 1,
        toolchain: 1,
        view: 1,
    };
    let mut view = ViewFreshness::new(binding.clone(), generation, 1, 2).unwrap();
    assert_eq!(
        view.evaluate(&binding, generation, 1, false),
        Freshness::Current
    );
    for late in [
        ViewGeneration {
            backend: 2,
            ..generation
        },
        ViewGeneration {
            configuration: 2,
            ..generation
        },
        ViewGeneration {
            toolchain: 2,
            ..generation
        },
        ViewGeneration {
            view: 2,
            ..generation
        },
    ] {
        assert_eq!(view.evaluate(&binding, late, 1, false), Freshness::Stale);
    }
    assert_eq!(
        view.evaluate(&binding, generation, 2, false),
        Freshness::Stale
    );
    assert_eq!(
        view.record_diagnostics(
            &binding,
            generation,
            1,
            true,
            DiagnosticReference::new("push").unwrap(),
            false
        ),
        Freshness::Provisional
    );
    assert_eq!(view.diagnostic_readiness(), DiagnosticReadiness::Unknown);
    let partial = observation(
        &workspace,
        tree,
        "partial",
        "partial",
        "two",
        SourceCoverage::Partial,
    )
    .await;
    let partial_binding = SourceBinding::from_observation(&partial);
    assert!(view.update_source(partial_binding.clone(), 2));
    assert_eq!(
        view.record_diagnostics(
            &partial_binding,
            generation,
            2,
            false,
            DiagnosticReference::new("partial").unwrap(),
            false
        ),
        Freshness::Unknown
    );
    assert_eq!(view.diagnostic_readiness(), DiagnosticReadiness::Unknown);
    view.quiesce();
    assert_eq!(
        view.evaluate(&partial_binding, generation, 2, false),
        Freshness::Stale
    );
    fs::remove_dir_all(root).unwrap();
    fs::remove_file(database).unwrap();
}

/// Proves the delta ceiling and cache handoff/retirement rules without a real provider process.
#[tokio::test]
async fn diagnostics_are_bounded_and_cache_reuse_requires_quiescent_compatibility() {
    let root = temporary("source");
    fs::create_dir(&root).unwrap();
    let database = temporary("database").with_extension("sqlite");
    let store = Store::open(&database, config()).unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let first = observation(
        &workspace,
        worktree(&root),
        "first",
        "first",
        "one",
        SourceCoverage::Complete,
    )
    .await;
    let binding = SourceBinding::from_observation(&first);
    let generation = ViewGeneration {
        backend: 1,
        configuration: 1,
        toolchain: 1,
        view: 1,
    };
    let mut view = ViewFreshness::new(binding.clone(), generation, 1, 2).unwrap();
    for reference in ["one", "two", "three"] {
        assert_eq!(
            view.record_diagnostics(
                &binding,
                generation,
                1,
                false,
                DiagnosticReference::new(reference).unwrap(),
                true
            ),
            Freshness::Current
        );
    }
    assert_eq!(view.diagnostic_deltas().references().len(), 2);
    assert!(view.diagnostic_deltas().overflowed());
    // Retirement authority must be Workspace's own private-field closure receipt, so this section
    // commits two real durable closures instead of naming a publicly constructible reason.
    // Native identity refuses symlinked components, so this real worktree lives under
    // `/private/tmp` rather than behind Darwin's `/tmp` and `/var` aliases.
    let tree_root = PathBuf::from(format!(
        "/private/tmp/agent-ide-freshness-{}-{}-closable-worktree",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(tree_root.join(".git")).unwrap();
    let durable_database = temporary("durable").with_extension("sqlite");
    let durable_backups = temporary("durable-backups");
    let durable_store =
        Store::open_with_backup_root(&durable_database, &durable_backups, config()).unwrap();
    let owner = DurableWorkspace::open(&durable_store).await.unwrap();
    let owned_tree = owner
        .resolve_worktree(tree_root.clone(), tree_root.clone(), PathBuf::from(".git"))
        .await
        .unwrap();
    let closure = owner
        .close_worktree(OperationId::new("close-cache-owner").unwrap(), &owned_tree)
        .await
        .unwrap();
    let reopened = owner
        .resolve_worktree(tree_root.clone(), tree_root.clone(), PathBuf::from(".git"))
        .await
        .unwrap();
    let other_closure = owner
        .close_worktree(
            OperationId::new("close-other-incarnation").unwrap(),
            &reopened,
        )
        .await
        .unwrap();
    assert_ne!(other_closure.incarnation(), closure.incarnation());
    let cache_path = temporary("cache");
    let cache_root = CacheRoot::prepare(&cache_path).unwrap();
    let identity = CacheIdentity::new(
        "gopls",
        "shared",
        "config",
        "toolchain",
        "trusted",
        "tree-state",
    )
    .unwrap();
    let incompatible = CacheIdentity::new(
        "gopls",
        "shared",
        "config",
        "toolchain",
        "trusted",
        "other-tree-state",
    )
    .unwrap();
    let mut cache = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("freshness").unwrap(),
        identity.clone(),
        &owned_tree,
    )
    .unwrap();
    assert_eq!(
        cache.namespace_path(),
        Some(cache_path.join("freshness").as_path()),
        "the retained lifecycle is the single source of the provider's cache path"
    );
    assert!(!cache.handoff(&identity));
    assert!(!cache.quiescent());
    cache.quiesce();
    assert!(cache.quiescent());
    assert!(cache.handoff(&identity));
    assert!(!cache.handoff(&incompatible));
    assert!(cache.retained());
    assert!(
        cache.retire(&other_closure).is_err(),
        "a closure of another worktree incarnation must retire nothing"
    );
    assert!(cache.retained());
    cache.retire(&closure).unwrap();
    assert!(!cache.retained());
    assert_eq!(cache.namespace_path(), None);
    let mut failed_cache = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("failed-retirement").unwrap(),
        identity.clone(),
        &owned_tree,
    )
    .unwrap();
    let failed_path = cache_path.join("failed-retirement");
    assert!(
        failed_cache.retire(&closure).is_err(),
        "retire must refuse a still-active (non-quiescent) lifecycle"
    );
    assert!(failed_cache.retained());
    failed_cache.quiesce();
    fs::set_permissions(&failed_path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed_cache.retire(&closure).is_err());
    assert!(failed_cache.retained());
    assert!(!failed_cache.handoff(&identity));
    fs::set_permissions(&failed_path, fs::Permissions::from_mode(0o700)).unwrap();
    failed_cache.retire(&closure).unwrap();
    assert!(!failed_cache.retained());

    // A closure whose incarnation happens to match but whose canonical worktree does not — the
    // exact same incarnation counter in a second Store, or a caller-built unverified reference —
    // must delete nothing: the incarnation alone is Store-local, so ownership is the nonce-bound
    // `WorktreeRef::id()` plus that incarnation.
    let foreign_database = temporary("foreign").with_extension("sqlite");
    let foreign_backups = temporary("foreign-backups");
    let foreign_store =
        Store::open_with_backup_root(&foreign_database, &foreign_backups, config()).unwrap();
    let foreign_owner = DurableWorkspace::open(&foreign_store).await.unwrap();
    let foreign_root = PathBuf::from(format!(
        "/private/tmp/agent-ide-freshness-{}-{}-foreign-worktree",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(foreign_root.join(".git")).unwrap();
    let foreign_tree = foreign_owner
        .resolve_worktree(
            foreign_root.clone(),
            foreign_root.clone(),
            PathBuf::from(".git"),
        )
        .await
        .unwrap();
    let foreign_closure = foreign_owner
        .close_worktree(
            OperationId::new("close-foreign-owner").unwrap(),
            &foreign_tree,
        )
        .await
        .unwrap();
    let mut cross_store = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("cross-store").unwrap(),
        identity.clone(),
        &owned_tree,
    )
    .unwrap();
    cross_store.quiesce();
    assert_eq!(foreign_closure.incarnation(), closure.incarnation());
    assert_ne!(foreign_closure.worktree(), closure.worktree());
    assert!(
        cross_store.retire(&foreign_closure).is_err(),
        "a same-incarnation closure from another Store must retire nothing"
    );
    assert!(cross_store.retained());
    assert!(cache_path.join("cross-store").is_dir());

    // A caller-built (unverified) reference names the same path and incarnation but cannot produce
    // the nonce-bound identity the durable owner minted, so it owns no namespace either.
    let unverified = agent_ide::workspace::authority::WorktreeRef::from_discovery(
        tree_root.clone(),
        tree_root.clone(),
        PathBuf::from(".git"),
        owned_tree.incarnation(),
    )
    .unwrap();
    assert_ne!(unverified.id(), owned_tree.id());
    let mut unverified_cache = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("unverified-owner").unwrap(),
        identity.clone(),
        &unverified,
    )
    .unwrap();
    unverified_cache.quiesce();
    assert!(
        unverified_cache.retire(&closure).is_err(),
        "the durable closure must not retire a namespace owned by an unverified reference"
    );
    assert!(unverified_cache.retained());

    fs::remove_dir_all(root).unwrap();
    fs::remove_file(database).unwrap();
    fs::remove_dir_all(cache_path).unwrap();
    fs::remove_dir_all(tree_root).unwrap();
    // Retain the durable database and its anonymous backup root until every Workspace and Store
    // borrowing them has been dropped, then remove both instead of leaking them into the host.
    drop(foreign_owner);
    drop(foreign_store);
    drop(owner);
    drop(durable_store);
    fs::remove_dir_all(foreign_root).unwrap();
    for database in [&durable_database, &foreign_database] {
        let _ = fs::remove_file(database);
    }
    for backups in [&durable_backups, &foreign_backups] {
        let _ = fs::remove_dir_all(backups);
    }
}
