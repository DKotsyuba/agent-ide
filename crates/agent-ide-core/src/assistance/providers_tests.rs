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
            required: &["listener"],
            shared: false,
        },
        CacheRequest {
            key: "second-provider".to_owned(),
            identity: identity("second"),
            required: &["target"],
            shared: false,
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
    let shared_refs: BTreeMap<String, usize> = BTreeMap::new();
    let plan = plan();

    let keys = retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan).unwrap();
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
        retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan),
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
        retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan).unwrap(),
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
    let shared_refs: BTreeMap<String, usize> = BTreeMap::new();
    let plan = plan();

    retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan).unwrap();
    assert_eq!(
        retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan),
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
    retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan).expect("handoff after stop");

    // An identical key whose effective configuration changed is an incompatibility, not a conflict.
    for cache in caches.values_mut() {
        cache.quiesce();
    }
    let incompatible = vec![CacheRequest {
        key: "first-provider".to_owned(),
        identity: identity("relaunched-with-other-configuration"),
        required: &["listener"],
        shared: false,
    }];
    assert_eq!(
        retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &incompatible),
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
    let shared_refs: BTreeMap<String, usize> = BTreeMap::new();
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
        retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan()),
        Err(FailureCode::Capacity)
    );
    assert_eq!(caches.len(), MAX_CACHE_NAMESPACES);
    assert!(caches.values().all(CacheLifecycle::retained));

    // A key already held is not new ownership, so it still succeeds under a full map.
    let held = vec![CacheRequest {
        key: "held-0".to_owned(),
        identity: identity("held"),
        required: &["listener"],
        shared: false,
    }];
    retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &held)
        .expect("reopening a held namespace");
    assert_eq!(caches.len(), MAX_CACHE_NAMESPACES);
    fs::remove_dir_all(root_path).unwrap();
}

/// Repeated failed unique activations must not grow the lifecycle map, refcounts, or the disk.
///
/// This is the GSC4 regression: every attempt uses a fresh unique first key, so before the rollback
/// each failure left a newly created namespace behind that no `binding_caches` entry, no lifecycle
/// map entry and no capacity bound ever accounted for. Only directories this failed operation is
/// proven to have created are removed, and the pre-existing retained namespace it did not create
/// keeps both its directory and its contents.
#[test]
fn a_failed_later_provider_leaves_no_unaccounted_namespace_or_directory_growth() {
    let root_path = temporary();
    let root = CacheRoot::prepare(&root_path).unwrap();
    let tree = worktree(&temporary());
    let mut caches: BTreeMap<String, CacheLifecycle> = BTreeMap::new();
    let shared_refs: BTreeMap<String, usize> = BTreeMap::new();

    // One pre-existing retained namespace with real content the rollback must never touch.
    let retained = vec![CacheRequest {
        key: "retained-provider".to_owned(),
        identity: identity("retained"),
        required: &["listener"],
        shared: false,
    }];
    retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &retained).unwrap();
    let retained_content = root_path
        .join("retained-provider")
        .join("listener")
        .join("db");
    fs::write(&retained_content, b"native cache content").unwrap();

    // A stale regular file blocks the second provider's required directory on every attempt.
    let blocked_root = root_path.join("blocked-provider");
    fs::create_dir(&blocked_root).unwrap();
    fs::set_permissions(
        &blocked_root,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    fs::write(blocked_root.join("target"), b"not a directory").unwrap();

    for attempt in 0..8 {
        let plan = vec![
            CacheRequest {
                key: format!("unique-provider-{attempt}"),
                identity: identity("unique"),
                required: &["listener", "tmp"],
                shared: false,
            },
            CacheRequest {
                key: "blocked-provider".to_owned(),
                identity: identity("blocked"),
                required: &["target"],
                shared: false,
            },
        ];
        assert_eq!(
            retain_cache_plan(&mut caches, &shared_refs, &root, &tree, &plan),
            Err(FailureCode::ProviderUnavailable)
        );
        assert_eq!(
            caches.len(),
            1,
            "attempt {attempt} must leave only the pre-existing retained lifecycle accounted"
        );
        assert!(
            !root_path
                .join(format!("unique-provider-{attempt}"))
                .exists(),
            "attempt {attempt} must roll back the namespace it alone created"
        );
    }
    // Only the retained namespace and the pre-existing blocked directory remain on disk.
    let mut remaining = fs::read_dir(&root_path)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    remaining.sort();
    assert_eq!(remaining, vec!["blocked-provider", "retained-provider"]);
    assert_eq!(
        fs::read_to_string(&retained_content).unwrap(),
        "native cache content",
        "rollback must never remove pre-existing retained contents"
    );
    fs::remove_dir_all(root_path).unwrap();
}
