//! Contract checks for Application's private cache namespace mechanics.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_ide::app::cache::{CacheNamespaceId, CacheRoot};

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Creates a unique cache root owned solely by this contract test.
fn cache_root() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-cache-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Proves namespaces are private bounded path components that reopen instead of being recreated.
///
/// Retirement itself is deliberately unreachable from here: `CacheNamespace::retire` consumes an
/// `app::cache::VerifiedCacheRetirement`, whose only constructor is crate-internal, so no external
/// caller — including this contract test — can name a closure or reset reason and delete a private
/// namespace. That path is covered where the authority actually exists, against a real Workspace
/// closure receipt, in `tests/intelligence_freshness_contract.rs`.
#[test]
fn cache_namespaces_are_private_bounded_and_reopened_not_recreated() {
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
    fs::write(namespace.path().join("opaque"), b"provider bytes").unwrap();
    let reopened = root
        .retain(CacheNamespaceId::new("worktree_42").unwrap())
        .unwrap();
    assert_eq!(reopened.path(), namespace.path());
    assert!(
        reopened.path().join("opaque").exists(),
        "reopening must never discard retained cache contents"
    );
    fs::set_permissions(namespace.path(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        root.retain(CacheNamespaceId::new("worktree_42").unwrap())
            .is_err(),
        "a namespace that stopped being private must not be handed out"
    );
    fs::remove_dir_all(root_path).unwrap();
}
