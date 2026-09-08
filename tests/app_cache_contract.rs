//! Contract checks for Application's private cache namespace mechanics.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_ide::app::cache::{CacheNamespaceId, CacheRoot, VerifiedCacheRetirement};

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Creates a unique cache root owned solely by this contract test.
fn cache_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-cache-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Proves namespaces are private, bounded path components and retain a retryable handle after a failed verified retirement.
#[test]
fn cache_retirement_requires_an_explicit_verified_fact() {
    let root_path = cache_root();
    let root = CacheRoot::prepare(&root_path).unwrap();
    let namespace = root
        .retain(CacheNamespaceId::new("worktree_42").unwrap())
        .unwrap();
    assert_eq!(
        fs::metadata(namespace.path()).unwrap().permissions().mode() & 0o077,
        0
    );
    assert!(CacheNamespaceId::new("../worktree").is_none());
    assert!(CacheNamespaceId::new("worktree/name").is_none());
    assert!(namespace.path().exists());
    namespace.retire(VerifiedCacheRetirement::Reset).unwrap();
    assert!(!root_path.join("worktree_42").exists());
    let retry = root
        .retain(CacheNamespaceId::new("retry").unwrap())
        .unwrap();
    fs::set_permissions(retry.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(retry.retire(VerifiedCacheRetirement::Closed).is_err());
    assert!(retry.path().exists());
    fs::set_permissions(retry.path(), fs::Permissions::from_mode(0o700)).unwrap();
    retry.retire(VerifiedCacheRetirement::Closed).unwrap();
    assert!(!retry.path().exists());
    fs::remove_dir(root_path).unwrap();
}
