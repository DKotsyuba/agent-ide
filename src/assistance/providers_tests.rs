//! Transactional-retention checks for the worker's multi-provider cache lifecycle map.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{CacheRequest, MAX_CACHE_NAMESPACES, retain_cache_plan};
use crate::app::cache::{CacheNamespaceId, CacheRoot};
use crate::assistance::reply::FailureCode;
use crate::intelligence::freshness::{CacheIdentity, CacheLifecycle};
use crate::workspace::authority::WorktreeRef;

/// Separates the disposable cache roots created by tests in this process.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Returns an uncreated unique temporary directory owned solely by one check.
///
/// Uses `/private/tmp` directly so Darwin's `/tmp` symlink alias cannot make the private-directory
/// validation reject a path this test just created.
fn temporary() -> PathBuf {
    PathBuf::from(format!(
        "/private/tmp/agent-ide-provider-caches-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Builds one canonical worktree reference without discovering host Git state.
///
/// `path` only has to be a plausible absolute root; the reference's incarnation is what the
/// retained lifecycles record as their owning durable generation.
fn worktree(path: &std::path::Path) -> WorktreeRef {
    WorktreeRef::from_discovery(
        path.to_path_buf(),
        path.to_path_buf(),
        PathBuf::from(".git"),
        1,
    )
    .unwrap()
}

/// Builds a compatibility identity that differs only by the caller-supplied provider name.
fn identity(provider: &str) -> CacheIdentity {
    CacheIdentity::new(
        provider,
        "settings",
        "configuration",
        "toolchain",
        "trusted",
        "tree:1",
    )
    .unwrap()
}

/// Builds the two-provider plan whose second entry is the one a check makes fail late.
fn plan() -> Vec<CacheRequest> {
    vec![
        CacheRequest {
            key: "first-provider".to_owned(),
            identity: identity("first"),
            required: &["gopls"],
        },
        CacheRequest {
            key: "second-provider".to_owned(),
            identity: identity("second"),
            required: &["target"],
        },
    ]
}

/// A late failure preserves every earlier lifecycle, and the corrected retry retains all of them.
///
/// This is the multi-provider regression: the first provider's namespace must stay exactly as it
/// was — retained, still quiescent, still reusable by an identical identity — when the second
/// provider's required subdirectory cannot be validated, and no lifecycle may be quiesced or
/// deleted by the failure itself.
#[test]
fn a_late_provider_failure_leaves_earlier_lifecycles_reusable_and_retries_cleanly() {
    let root_path = temporary();
    let root = CacheRoot::prepare(&root_path).unwrap();
    let tree = worktree(&temporary());
    let mut caches: BTreeMap<String, CacheLifecycle> = BTreeMap::new();
    let plan = plan();

    let keys = retain_cache_plan(&mut caches, &root, &tree, &plan).unwrap();
    assert_eq!(keys, vec!["first-provider", "second-provider"]);
    for cache in caches.values_mut() {
        cache.quiesce();
    }

    // A stale regular file where the second provider needs a private directory fails validation
    // only after the first provider's namespace has already been prepared.
    let blocked = root_path.join("second-provider").join("target");
    fs::remove_dir_all(&blocked).unwrap();
    fs::write(&blocked, b"not a directory").unwrap();
    assert_eq!(
        retain_cache_plan(&mut caches, &root, &tree, &plan),
        Err(FailureCode::ProviderUnavailable)
    );
    assert_eq!(caches.len(), 2);
    for (key, cache) in &mut caches {
        assert!(cache.retained(), "{key} must remain retained");
        assert!(
            cache.quiescent(),
            "{key} must keep the quiescent state the failure never touched"
        );
        assert!(
            root_path.join(key).is_dir(),
            "{key} must keep its on-disk namespace"
        );
    }
    assert!(
        caches
            .get_mut("first-provider")
            .unwrap()
            .handoff(&identity("first")),
        "the earlier lifecycle must still be reusable after the later failure"
    );

    fs::remove_file(&blocked).unwrap();
    assert_eq!(
        retain_cache_plan(&mut caches, &root, &tree, &plan).unwrap(),
        vec!["first-provider", "second-provider"]
    );
    assert!(caches.values().all(|cache| !cache.quiescent()));
    fs::remove_dir_all(root_path).unwrap();
}

/// A second live owner of the same namespace is refused as a finite conflict and may hand off later.
#[test]
fn a_live_namespace_owner_is_reported_as_a_conflict_until_it_quiesces() {
    let root_path = temporary();
    let root = CacheRoot::prepare(&root_path).unwrap();
    let tree = worktree(&temporary());
    let mut caches: BTreeMap<String, CacheLifecycle> = BTreeMap::new();
    let plan = plan();

    retain_cache_plan(&mut caches, &root, &tree, &plan).unwrap();
    assert_eq!(
        retain_cache_plan(&mut caches, &root, &tree, &plan),
        Err(FailureCode::Conflict),
        "a concurrent actor must see a finite ownership conflict, not silent unknown reuse"
    );
    assert!(caches.values().all(|cache| cache.retained()));
    assert!(
        caches.values().all(|cache| !cache.quiescent()),
        "the refused activation must not quiesce the actor that already owns the namespace"
    );

    for cache in caches.values_mut() {
        cache.quiesce();
    }
    retain_cache_plan(&mut caches, &root, &tree, &plan).expect("handoff after stop");

    // An identical key whose effective configuration changed is an incompatibility, not a conflict.
    for cache in caches.values_mut() {
        cache.quiesce();
    }
    let incompatible = vec![CacheRequest {
        key: "first-provider".to_owned(),
        identity: identity("relaunched-with-other-configuration"),
        required: &["gopls"],
    }];
    assert_eq!(
        retain_cache_plan(&mut caches, &root, &tree, &incompatible),
        Err(FailureCode::ProviderUnavailable)
    );
    fs::remove_dir_all(root_path).unwrap();
}

/// A full lifecycle map fails closed with a typed capacity error and evicts no retained namespace.
#[test]
fn bounded_lifecycle_ownership_fails_closed_instead_of_evicting_retained_state() {
    let root_path = temporary();
    let root = CacheRoot::prepare(&root_path).unwrap();
    let tree = worktree(&temporary());
    let mut caches: BTreeMap<String, CacheLifecycle> = BTreeMap::new();
    for index in 0..MAX_CACHE_NAMESPACES {
        let key = format!("held-{index}");
        let mut cache = CacheLifecycle::retain(
            &root,
            CacheNamespaceId::new(key.clone()).unwrap(),
            identity("held"),
            &tree,
        )
        .unwrap();
        cache.quiesce();
        caches.insert(key, cache);
    }
    assert_eq!(
        retain_cache_plan(&mut caches, &root, &tree, &plan()),
        Err(FailureCode::Capacity)
    );
    assert_eq!(caches.len(), MAX_CACHE_NAMESPACES);
    assert!(caches.values().all(CacheLifecycle::retained));

    // A key already held is not new ownership, so it still succeeds under a full map.
    let held = vec![CacheRequest {
        key: "held-0".to_owned(),
        identity: identity("held"),
        required: &["gopls"],
    }];
    retain_cache_plan(&mut caches, &root, &tree, &held).expect("reopening a held namespace");
    assert_eq!(caches.len(), MAX_CACHE_NAMESPACES);
    fs::remove_dir_all(root_path).unwrap();
}
