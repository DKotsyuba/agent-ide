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
        cache::{CacheNamespaceId, CacheRoot, VerifiedCacheRetirement},
        config::StoreConfig,
        store::{OperationId, Store},
    },
    intelligence::freshness::{
        CacheIdentity, CacheLifecycle, DiagnosticReadiness, DiagnosticReference, Freshness,
        SourceBinding, ViewFreshness, ViewGeneration,
    },
    workspace::{
        authority::WorktreeRef,
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
        "other",
        "toolchain",
        "trusted",
        "tree-state",
    )
    .unwrap();
    let mut cache = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("freshness").unwrap(),
        identity.clone(),
    )
    .unwrap();
    assert!(!cache.handoff(&identity));
    cache.quiesce();
    assert!(cache.handoff(&identity));
    assert!(!cache.handoff(&incompatible));
    assert!(cache.retained());
    cache.retire(VerifiedCacheRetirement::Closed).unwrap();
    assert!(!cache.retained());
    let mut failed_cache = CacheLifecycle::retain(
        &cache_root,
        CacheNamespaceId::new("failed-retirement").unwrap(),
        identity.clone(),
    )
    .unwrap();
    let failed_path = cache_path.join("failed-retirement");
    fs::set_permissions(&failed_path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(failed_cache.retire(VerifiedCacheRetirement::Reset).is_err());
    assert!(failed_cache.retained());
    failed_cache.quiesce();
    assert!(!failed_cache.handoff(&identity));
    fs::set_permissions(&failed_path, fs::Permissions::from_mode(0o700)).unwrap();
    failed_cache.retire(VerifiedCacheRetirement::Reset).unwrap();
    assert!(!failed_cache.retained());
    fs::remove_dir_all(root).unwrap();
    fs::remove_file(database).unwrap();
    fs::remove_dir_all(cache_path).unwrap();
}
