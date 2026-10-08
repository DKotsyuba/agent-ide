//! Collection and publication-exclusion tests for [`super`], on private temporary roots.

use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

/// Two days: past [`STALE_AFTER`].
const OLD: Duration = Duration::from_secs(2 * 86_400);

/// Creates a fresh canonical scratch directory unique to this process and `name`.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-ide-hints-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::canonicalize(dir).unwrap()
}

/// Builds the hint directory of `slot` (16 hex digits) below `root`, holding a `key` file naming
/// `key` when given, then backdates every entry by `age`.
fn hint(root: &Path, slot: &str, key: Option<&Path>, age: Duration) -> PathBuf {
    let dir = root.join(format!("{KEY_CACHE_PREFIX}{slot}"));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    if let Some(key) = key {
        let file = dir.join(KEY_FILE);
        fs::write(&file, key.as_os_str().as_bytes()).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(dir.join(ATTACHMENT_FILE), [b'a'; 64]).unwrap();
    }
    backdate(&dir, age);
    dir
}

/// Sets the mtime of `dir` and its files `age` into the past (children first).
fn backdate(dir: &Path, age: Duration) {
    let when = SystemTime::now() - age;
    for entry in fs::read_dir(dir).unwrap() {
        File::open(entry.unwrap().path())
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    File::open(dir).unwrap().set_modified(when).unwrap();
}

/// Only an old hint whose cached key and runtime are definitely gone is collected; one a live
/// candidate may still use, a young one and every unusual shape are kept.
#[test]
fn only_hints_nothing_can_use_are_collected() {
    let tmp = scratch("collect");
    let live_key = tmp.join("repo/.git");
    fs::create_dir_all(&live_key).unwrap();
    let gone_key = tmp.join("deleted/.git");
    let served_key = tmp.join("served/.git");
    fs::create_dir_all(runtime_dir(&tmp, &served_key)).unwrap();

    let stale = hint(&tmp, "0000000000000001", Some(&gone_key), OLD);
    let empty = hint(&tmp, "0000000000000002", None, OLD);
    let unusable = hint(
        &tmp,
        "0000000000000003",
        Some(Path::new("relative/key")),
        OLD,
    );
    let live = hint(&tmp, "0000000000000004", Some(&live_key), OLD);
    let runtime = hint(&tmp, "0000000000000005", Some(&served_key), OLD);
    let young = hint(
        &tmp,
        "0000000000000006",
        Some(&gone_key),
        Duration::from_secs(60),
    );
    let loose = hint(&tmp, "0000000000000007", Some(&gone_key), OLD);
    fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).unwrap();
    let foreign = hint(&tmp, "0000000000000008", Some(&gone_key), OLD);
    fs::create_dir(foreign.join("extra")).unwrap();
    backdate(&foreign, OLD);
    let linked = hint(&tmp, "0000000000000009", Some(&gone_key), OLD);
    symlink(tmp.join("repo"), linked.join("key-link")).unwrap();
    let outside = tmp.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"keep").unwrap();
    symlink(&outside, tmp.join("ai-k-000000000000000a")).unwrap();
    // Names that are not hint directories are never looked at.
    let other = tmp.join("ai-k-short");
    fs::create_dir(&other).unwrap();
    backdate(&other, OLD);
    let upper = tmp.join("ai-k-ZZZZZZZZZZZZZZZZ");
    fs::create_dir(&upper).unwrap();

    let dry = collect(&tmp, false, SystemTime::now(), &|| true);
    assert_eq!(
        dry,
        Collection {
            hints: 10,
            stale: 3
        }
    );
    assert!(
        stale.exists() && empty.exists() && unusable.exists(),
        "a dry run removes nothing"
    );

    let done = collect(&tmp, true, SystemTime::now(), &|| true);
    assert_eq!(
        done,
        Collection {
            hints: 10,
            stale: 3
        }
    );
    for gone in [&stale, &empty, &unusable] {
        assert!(!gone.exists(), "{} should be collected", gone.display());
    }
    for kept in [
        &live, &runtime, &young, &loose, &foreign, &linked, &other, &upper,
    ] {
        assert!(kept.exists(), "{} should be kept", kept.display());
    }
    assert!(
        outside.join("keep").exists(),
        "a symlinked hint is never followed"
    );
    assert!(live.join(KEY_FILE).exists() && live.join(ATTACHMENT_FILE).exists());
}

/// A publisher holding the shared lock keeps its hint from being collected; once it lets go the
/// hint goes, and a publisher whose directory was collected while it waited gets `None`.
#[test]
fn a_publisher_and_a_collection_exclude_each_other() {
    let tmp = scratch("exclusion");
    let gone_key = tmp.join("deleted/.git");
    let dir = hint(&tmp, "00000000000000b1", Some(&gone_key), OLD);

    let guard = PublishGuard::acquire(&dir).expect("shared lock");
    assert_eq!(collect(&tmp, true, SystemTime::now(), &|| true).stale, 0);
    assert!(dir.exists(), "a hint being published is not collected");
    drop(guard);
    assert_eq!(collect(&tmp, true, SystemTime::now(), &|| true).stale, 1);
    assert!(!dir.exists());
    assert!(
        PublishGuard::acquire(&dir).is_none(),
        "the collected directory is gone"
    );

    // A directory replaced while the publisher waited is not the one it locked.
    let dir = hint(&tmp, "00000000000000b2", Some(&gone_key), OLD);
    let exclusive = {
        let file = open_private(&dir).unwrap();
        assert!(lock(&file, libc::LOCK_EX | libc::LOCK_NB));
        file
    };
    let waiting = {
        let dir = dir.clone();
        std::thread::spawn(move || PublishGuard::acquire(&dir).is_some())
    };
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !waiting.is_finished(),
        "the publisher waits behind a collection"
    );
    fs::rename(&dir, tmp.join("moved")).unwrap();
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    drop(exclusive);
    assert!(
        !waiting.join().unwrap(),
        "the replaced directory is refused"
    );
}

/// A directory replaced between a collection opening it and locking it is not the one judged or
/// removed: the stranger that took the name, stale by every rule but with a publisher holding its
/// shared lock, survives, because the lock the collection holds is on the original inode.
#[test]
fn a_directory_replaced_before_the_lock_is_never_removed() {
    let tmp = scratch("replaced");
    let gone_key = tmp.join("deleted/.git");
    let dir = hint(&tmp, "00000000000000c1", Some(&gone_key), OLD);
    let publisher = std::cell::RefCell::new(None);
    let replace = |path: &Path| {
        fs::rename(path, tmp.join("moved-away")).unwrap();
        fs::create_dir(path).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join(KEY_FILE), gone_key.as_os_str().as_bytes()).unwrap();
        backdate(path, OLD);
        *publisher.borrow_mut() = Some(PublishGuard::acquire(path).expect("publisher lock"));
    };
    let found = collect_with(&tmp, true, SystemTime::now(), &|| true, &replace);
    assert_eq!(found.stale, 0);
    assert!(
        dir.join(KEY_FILE).exists(),
        "the directory a publisher holds is not removed"
    );
    assert!(tmp.join("moved-away").exists());
    drop(publisher);
}

/// Safety is asked again right before each removal: when an unsafe publisher appears mid-pass,
/// the hints not yet removed stay, though they were judged stale.
#[test]
fn an_unsafe_publisher_appearing_mid_pass_keeps_the_remaining_hints() {
    let tmp = scratch("mid-pass");
    let gone_key = tmp.join("deleted/.git");
    for slot in ["00000000000000d1", "00000000000000d2", "00000000000000d3"] {
        hint(&tmp, slot, Some(&gone_key), OLD);
    }
    let asked = std::cell::Cell::new(0);
    // Safe for the first removal only: an old front starts after that.
    let safe = || {
        asked.set(asked.get() + 1);
        asked.get() == 1
    };
    let found = collect(&tmp, true, SystemTime::now(), &safe);
    assert_eq!(found, Collection { hints: 3, stale: 1 });
    assert_eq!(asked.get(), 3, "asked before every removal, never skipped");
    let left = fs::read_dir(&tmp)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("ai-k-")
        })
        .count();
    assert_eq!(left, 2);
}
