//! Transactional-retention checks for the worker's multi-provider cache lifecycle map.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{CacheRequest, MAX_CACHE_NAMESPACES, project_inputs_stamp, retain_cache_plan};
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

/// The project input stamp follows exactly the named files inside its search ceiling: an edit of a
/// named file changes it, an unrelated file or a skipped tree does not, a manifest deeper than the
/// ceiling is not seen, and a directory far larger than the entry budget is still stamped.
#[test]
fn project_inputs_stamp_follows_named_files_within_its_ceiling() {
    let root = temporary();
    fs::create_dir_all(root.join("member/src")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::create_dir_all(root.join("a/b/c/d/e")).unwrap();
    fs::write(root.join("Cargo.toml"), "one").unwrap();
    fs::write(root.join("member/Cargo.toml"), "one").unwrap();
    fs::write(root.join("target/debug/Cargo.toml"), "one").unwrap();
    fs::write(root.join("a/b/c/d/e/Cargo.toml"), "one").unwrap();
    let stamp = || project_inputs_stamp(&root, &["Cargo.toml"]);
    let before = stamp();
    assert_eq!(before, stamp(), "the stamp is deterministic");
    fs::write(root.join("member/src/lib.rs"), "unrelated").unwrap();
    assert_eq!(before, stamp(), "an unnamed file is not an input");
    fs::write(
        root.join("target/debug/Cargo.toml"),
        "changed in a skipped tree",
    )
    .unwrap();
    assert_eq!(before, stamp(), "a skipped tree is not searched");
    fs::write(
        root.join("a/b/c/d/e/Cargo.toml"),
        "changed past the depth ceiling",
    )
    .unwrap();
    assert_eq!(
        before,
        stamp(),
        "a manifest past the depth ceiling is not seen"
    );
    fs::write(root.join("member/Cargo.toml"), "changed member manifest").unwrap();
    assert_ne!(
        before,
        stamp(),
        "a named file inside the ceiling is an input"
    );
    // A same-length edit that restores the modification time still changes the stamp.
    let manifest = root.join("Cargo.toml");
    let modified = fs::metadata(&manifest).unwrap().modified().unwrap();
    let before_fix = stamp();
    fs::write(&manifest, "two").unwrap();
    fs::File::options()
        .write(true)
        .open(&manifest)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(fs::read(&manifest).unwrap().len(), 3);
    assert_ne!(
        before_fix,
        stamp(),
        "a small file is digested whole, whatever its timestamp"
    );
    // A file past the prefix: an ordinary tail edit moves the modification time and is seen; a
    // same-length tail edit that also restores it is the stated ceiling and is not.
    let lock = root.join("member/Cargo.lock");
    fs::write(
        &lock,
        vec![b'a'; super::INPUT_SCAN_FILE_BYTES as usize + 64],
    )
    .unwrap();
    let stamp = || project_inputs_stamp(&root, &["Cargo.toml", "Cargo.lock"]);
    let before_tail = stamp();
    let mut edited = vec![b'a'; super::INPUT_SCAN_FILE_BYTES as usize + 64];
    *edited.last_mut().unwrap() = b'b';
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&lock, &edited).unwrap();
    assert_ne!(
        before_tail,
        stamp(),
        "an ordinary tail edit moves the mtime"
    );
    let after_tail = stamp();
    let modified = fs::metadata(&lock).unwrap().modified().unwrap();
    *edited.last_mut().unwrap() = b'c';
    fs::write(&lock, &edited).unwrap();
    fs::File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    assert_eq!(
        after_tail,
        stamp(),
        "a same-length tail edit past the prefix with a restored mtime is the stated ceiling"
    );
    assert_eq!(
        project_inputs_stamp(&root, &[]),
        [0; 32],
        "no named inputs stamp to a constant"
    );
    // A directory far larger than the entry budget is read only up to the budget.
    let many = root.join("many");
    fs::create_dir_all(&many).unwrap();
    for index in 0..(super::INPUT_SCAN_ENTRIES + 500) {
        fs::write(many.join(format!("f{index}")), "").unwrap();
    }
    let _ = stamp();
    fs::remove_dir_all(root).unwrap();
}

/// Session health is tracked per owner and server slot: a failure of one session never marks or
/// clears another's, whatever order a job visits them in, and a failure is sticky for its session.
#[test]
fn session_health_is_kept_per_owner_and_slot() {
    use super::{Providers, SessionHealth};
    use crate::assistance::host_binding::BindingRef;
    let (a, b) = (
        (BindingRef::fixture("health-a", "health-channel", 1), 0),
        (BindingRef::fixture("health-b", "health-channel", 1), 1),
    );
    let mut providers = Providers::new(temporary());
    providers.begin_job();
    providers.current = Some(a.clone());
    providers.note_session_fault();
    providers.current = Some(b.clone());
    providers.note_session_healthy();
    providers.current = Some(a.clone());
    providers.note_session_healthy();
    assert_eq!(providers.health.get(&a), Some(&SessionHealth::Failed));
    assert_eq!(providers.health.get(&b), Some(&SessionHealth::Healthy));
    providers.begin_job();
    assert!(providers.health.is_empty());
}

/// A named pipe, or a symlink to one, carrying an input's name never blocks the stamp: only
/// regular files are opened, so the scan returns at once (a blocking open would hang the worker).
#[test]
fn project_inputs_stamp_never_opens_special_files() {
    let root = temporary();
    fs::create_dir_all(&root).unwrap();
    let pipe = root.join("Cargo.toml");
    let made = std::process::Command::new("mkfifo")
        .arg(&pipe)
        .status()
        .unwrap();
    assert!(made.success(), "mkfifo");
    std::os::unix::fs::symlink(&pipe, root.join("pyproject.toml")).unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let scanned = root.clone();
    std::thread::spawn(move || {
        let _ = sender.send(project_inputs_stamp(
            &scanned,
            &["Cargo.toml", "pyproject.toml"],
        ));
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stamp must not block on a named pipe");
    fs::remove_dir_all(root).unwrap();
}

/// A fixture provider declaration; every argument is one input of the namespace key.
fn launch(
    identity: &str,
    toolchain: &str,
    trust: &str,
    cache_namespace: &str,
) -> super::ProviderLaunch {
    crate::lang::testing::install();
    serde_json::from_value(serde_json::json!({
        "executable": {
            "path": "/usr/bin/git",
            "identity": identity,
            "blake3": "0".repeat(64),
        },
        "settings": "fixture_epsilon",
        "toolchain": toolchain,
        "trust": trust,
        "cache_namespace": cache_namespace,
    }))
    .unwrap()
}

/// The key a daemon derives for `launch` in `tree`.
fn key_of(tree: &WorktreeRef, launch: &super::ProviderLaunch, configuration: &str) -> String {
    super::provider_cache_key(
        &super::cache_state(tree),
        launch,
        launch.server().cache_settings(),
        configuration,
        &launch.trust,
    )
}

/// The same directory names the same state in every boot, whatever the boot-local identity says;
/// a recreated directory or another path never reaches it.
#[test]
fn restart_state_follows_the_directory_not_the_boot() {
    let path = temporary();
    fs::create_dir_all(&path).unwrap();
    let first_boot = worktree(&path);
    let second_boot =
        WorktreeRef::from_discovery(path.clone(), path.clone(), PathBuf::from(".git"), 9).unwrap();
    assert_ne!(first_boot.id(), second_boot.id());
    assert_eq!(
        super::cache_state(&first_boot),
        super::cache_state(&second_boot)
    );

    let elsewhere = temporary();
    fs::create_dir_all(&elsewhere).unwrap();
    assert_ne!(
        super::cache_state(&first_boot),
        super::cache_state(&worktree(&elsewhere))
    );

    // Deleted and recreated at the same path: a different directory, a different state.
    let before = super::cache_state(&first_boot);
    fs::remove_dir_all(&path).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::create_dir_all(&path).unwrap();
    assert_ne!(before, super::cache_state(&first_boot));

    // A tree that cannot be inspected keeps its boot-local identity, never shared with another boot.
    let missing = temporary();
    assert_ne!(
        super::cache_state(&worktree(&missing)),
        super::cache_state(
            &WorktreeRef::from_discovery(
                missing.clone(),
                missing.clone(),
                PathBuf::from(".git"),
                9
            )
            .unwrap()
        )
    );
}

/// Every input that makes a native cache unsafe to reuse changes the namespace key.
#[test]
fn the_namespace_key_fences_executable_settings_configuration_toolchain_and_trust() {
    let path = temporary();
    fs::create_dir_all(&path).unwrap();
    let tree = worktree(&path);
    let base = launch("exe-1", "toolchain-1", "trust-1", "ns");
    let key = key_of(&tree, &base, "configuration-1");
    assert_eq!(
        key,
        key_of(
            &tree,
            &launch("exe-1", "toolchain-1", "trust-1", "ns"),
            "configuration-1"
        )
    );
    for (what, other) in [
        (
            "executable",
            key_of(
                &tree,
                &launch("exe-2", "toolchain-1", "trust-1", "ns"),
                "configuration-1",
            ),
        ),
        (
            "toolchain",
            key_of(
                &tree,
                &launch("exe-1", "toolchain-2", "trust-1", "ns"),
                "configuration-1",
            ),
        ),
        (
            "trust",
            key_of(
                &tree,
                &launch("exe-1", "toolchain-1", "trust-2", "ns"),
                "configuration-1",
            ),
        ),
        (
            "namespace",
            key_of(
                &tree,
                &launch("exe-1", "toolchain-1", "trust-1", "ns2"),
                "configuration-1",
            ),
        ),
        ("configuration", key_of(&tree, &base, "configuration-2")),
    ] {
        assert_ne!(
            key, other,
            "a changed {what} must not adopt the old namespace"
        );
    }
    let settings = super::provider_cache_key(
        &super::cache_state(&tree),
        &base,
        "other-settings",
        "configuration-1",
        &base.trust,
    );
    assert_ne!(key, settings, "changed settings");
}

/// Namespaces live in the providers root: a second boot (new Store, new incarnation, empty map)
/// adopts the first boot's directory and its content, a changed trust starts cold in another
/// directory, `cache status` lists them, and a deleted worktree retires its namespace.
#[test]
fn a_restarted_daemon_adopts_the_namespace_and_retention_retires_it() {
    let home = temporary();
    let providers = crate::retention::providers_root(&home).expect("private providers root");
    let root = CacheRoot::prepare(&providers).unwrap();
    let path = temporary();
    fs::create_dir_all(&path).unwrap();
    let request = |tree: &WorktreeRef, trust: &str| {
        let launch = launch("exe-1", "toolchain-1", trust, "ns");
        vec![CacheRequest {
            key: key_of(tree, &launch, "configuration-1"),
            identity: CacheIdentity::new(
                "exe-1",
                "settings",
                "configuration-1",
                "toolchain-1",
                trust,
                super::cache_state(tree),
            )
            .unwrap(),
            required: &["target"],
            shared: false,
        }]
    };

    let boot_one = worktree(&path);
    let mut caches = BTreeMap::new();
    let keys = retain_cache_plan(
        &mut caches,
        &BTreeMap::new(),
        &root,
        &boot_one,
        &request(&boot_one, "trust-1"),
    )
    .unwrap();
    let namespace = providers.join(&keys[0]);
    fs::write(namespace.join("target/built"), b"native cache").unwrap();
    assert_eq!(
        fs::read(namespace.join(crate::retention::MARKER_FILE_NAME)).unwrap(),
        fs::canonicalize(&path)
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
    );

    let boot_two =
        WorktreeRef::from_discovery(path.clone(), path.clone(), PathBuf::from(".git"), 7).unwrap();
    let mut caches = BTreeMap::new();
    let adopted = retain_cache_plan(
        &mut caches,
        &BTreeMap::new(),
        &root,
        &boot_two,
        &request(&boot_two, "trust-1"),
    )
    .unwrap();
    assert_eq!(
        adopted, keys,
        "the restarted daemon finds the same namespace"
    );
    assert_eq!(
        fs::read(providers.join(&adopted[0]).join("target/built")).unwrap(),
        b"native cache"
    );

    let mut caches = BTreeMap::new();
    let changed = retain_cache_plan(
        &mut caches,
        &BTreeMap::new(),
        &root,
        &boot_two,
        &request(&boot_two, "trust-2"),
    )
    .unwrap();
    assert_ne!(changed, keys, "a changed trust never adopts it");
    assert!(!providers.join(&changed[0]).join("target/built").exists());

    let status = crate::retention::sweep_with(&home, false, std::time::SystemTime::now(), &|| {
        Some(Vec::new())
    })
    .render(false);
    assert!(status.contains("providers: 2 entries"), "{status}");

    fs::remove_dir_all(&path).unwrap();
    let report = crate::retention::sweep_with(&home, true, std::time::SystemTime::now(), &|| {
        Some(Vec::new())
    });
    assert!(
        report
            .verdicts
            .iter()
            .all(|verdict| verdict.reason == crate::retention::Reason::Gone)
    );
    assert_eq!(report.verdicts.len(), 2, "{report:?}");
    assert!(!namespace.exists());
    fs::remove_dir_all(home).unwrap();
}
