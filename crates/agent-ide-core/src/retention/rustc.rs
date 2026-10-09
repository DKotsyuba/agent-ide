//! Locating rustc's incremental-compilation state inside one check cache, and the finalized
//! sessions of it that nothing will read again.
//!
//! rustc keeps `incremental/<crate>-<hash>/s-<timestamp>-<random>-<svh>` (a finalized session) and
//! `…-working` (one being written), each guarded by the sibling file `s-<timestamp>-<random>.lock`
//! it locks (a POSIX `fcntl` lock on non-Linux Unix; the `flock` taken here excludes it on macOS,
//! checked with a cross-process probe): exclusive while collecting or writing, shared while
//! reading. `<timestamp>` and `<random>` are base-36, so recency is the decoded timestamp, never the directory's mtime. Only
//! the newest finalized session of a crate is ever loaded; the rest are collected by rustc itself
//! only when that crate is compiled again, which a retired crate never is.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// Deepest level below a language cache's `target` directory searched for `incremental` (covers
/// `target/<profile>/incremental` and `target/<triple>/<profile>/incremental`).
const SEARCH_DEPTH: usize = 3;

/// A finalized session and the lock file that guards it.
#[derive(Debug)]
pub(super) struct Session {
    /// The session directory.
    pub(super) dir: PathBuf,
    /// Its sibling `s-<timestamp>-<random>.lock`.
    pub(super) lock: PathBuf,
}

/// What one directory name inside a crate's incremental directory is.
enum Name {
    /// `s-<timestamp>-<random>-<svh>`: the decoded timestamp.
    Finalized(u128),
    /// `s-<timestamp>-<random>-working`.
    Working,
    /// `s-<timestamp>-<random>.lock`.
    Lock,
    /// Not a rustc session file at all.
    Other,
    /// Looks like one (`s-…`) but does not parse: nothing in this crate is touched.
    Malformed,
}

/// Whether `text` is a nonempty lowercase base-36 number.
fn base36(text: &[u8]) -> Option<u128> {
    if text.is_empty() || text.len() > 25 {
        return None;
    }
    text.iter().try_fold(0u128, |value, byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'z' => byte - b'a' + 10,
            _ => return None,
        };
        value.checked_mul(36)?.checked_add(u128::from(digit))
    })
}

/// Classifies one name.
fn classify(name: &OsStr) -> Name {
    let bytes = name.as_bytes();
    let Some(rest) = bytes.strip_prefix(b"s-") else {
        return Name::Other;
    };
    if let Some(stem) = rest.strip_suffix(b".lock") {
        let mut parts = stem.split(|byte| *byte == b'-');
        return match (parts.next(), parts.next(), parts.next()) {
            (Some(timestamp), Some(random), None)
                if base36(timestamp).is_some() && base36(random).is_some() =>
            {
                Name::Lock
            }
            _ => Name::Malformed,
        };
    }
    let mut parts = rest.split(|byte| *byte == b'-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(timestamp), Some(random), Some(last), None) => {
            match (base36(timestamp), base36(random)) {
                (Some(_), Some(_)) if last == b"working" => Name::Working,
                (Some(timestamp), Some(_)) if base36(last).is_some() => Name::Finalized(timestamp),
                _ => Name::Malformed,
            }
        }
        _ => Name::Malformed,
    }
}

/// Finds every real `incremental` directory below `<entry>/<digest>/<language>/target`.
pub(super) fn incremental_dirs(entry: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for digest in real_subdirectories(entry) {
        for language in real_subdirectories(&digest) {
            // A symlinked `target` is a caller's directory, never ours to search or empty.
            let target = language.join("target");
            if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.is_dir()) {
                search(&target, SEARCH_DEPTH, &mut found);
            }
        }
    }
    found
}

/// Collects `incremental` directories below `dir`, not descending into one.
fn search(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    for child in real_subdirectories(dir) {
        if child.file_name() == Some(OsStr::new("incremental")) {
            found.push(child);
        } else if depth > 1 {
            search(&child, depth - 1, found);
        }
    }
}

/// Lists the real (non-symlink) subdirectories of `dir`; an unreadable directory has none.
fn real_subdirectories(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect()
}

/// Lists the finalized sessions of every crate below `incremental` that are older than the
/// crate's newest finalized session.
///
/// A crate directory is left entirely alone when any `s-…` entry in it is malformed, when its
/// newest timestamp is shared by two finalized sessions (both are kept), when a finalized entry is
/// not a real directory, or when it cannot be read. `-working` sessions and their locks are never
/// listed.
pub(super) fn older_sessions(incremental: &Path) -> Vec<Session> {
    let mut older = Vec::new();
    for krate in real_subdirectories(incremental) {
        let Ok(children) = fs::read_dir(&krate) else {
            continue;
        };
        let mut finalized = Vec::new();
        let mut certain = true;
        for child in children {
            let Ok(child) = child else {
                certain = false;
                continue;
            };
            match classify(&child.file_name()) {
                Name::Finalized(timestamp) => {
                    if child.file_type().is_ok_and(|kind| kind.is_dir()) {
                        finalized.push((timestamp, child.file_name(), child.path()));
                    } else {
                        certain = false;
                    }
                }
                Name::Malformed => certain = false,
                Name::Working | Name::Lock | Name::Other => {}
            }
        }
        let Some(newest) = finalized.iter().map(|(timestamp, ..)| *timestamp).max() else {
            continue;
        };
        if !certain || finalized.iter().filter(|(t, ..)| *t == newest).count() > 1 {
            continue;
        }
        for (timestamp, name, dir) in finalized {
            if timestamp == newest {
                continue;
            }
            let name = name.as_bytes();
            // `s-<timestamp>-<random>-<svh>` → `s-<timestamp>-<random>.lock`.
            let Some(stem) = name
                .iter()
                .rposition(|byte| *byte == b'-')
                .map(|at| &name[..at])
            else {
                continue;
            };
            let mut lock = stem.to_vec();
            lock.extend_from_slice(b".lock");
            older.push(Session {
                lock: krate.join(OsStr::from_bytes(&lock)),
                dir,
            });
        }
    }
    older
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classification of every name shape rustc writes, and of look-alikes.
    #[test]
    fn names_are_classified_exactly() {
        assert!(matches!(
            classify(OsStr::new("s-hq1ab2-0xyz-3k9")),
            Name::Finalized(_)
        ));
        assert!(matches!(
            classify(OsStr::new("s-hq1ab2-0xyz-working")),
            Name::Working
        ));
        assert!(matches!(
            classify(OsStr::new("s-hq1ab2-0xyz.lock")),
            Name::Lock
        ));
        assert!(matches!(
            classify(OsStr::new("query-cache.bin")),
            Name::Other
        ));
        for bad in ["s-", "s-A-b-c", "s-a-b", "s-a-b-c-d", "s-a-b-C", "s--b-c"] {
            assert!(
                matches!(classify(OsStr::new(bad)), Name::Malformed),
                "{bad}"
            );
        }
        assert!(matches!(
            classify(OsStr::new("s-a-b-c.lock")),
            Name::Malformed
        ));
    }

    /// Recency is the decoded base-36 value, not the text order (`z` < `10` as numbers).
    #[test]
    fn the_newest_session_is_chosen_by_numeric_timestamp() {
        let root = std::env::temp_dir().join(format!("agent-ide-rustc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let krate = root.join("incremental/krate-abc");
        for name in ["s-z-a1-h1", "s-10-b2-h2", "s-9-c3-h3", "s-10-d4-working"] {
            fs::create_dir_all(krate.join(name)).unwrap();
        }
        let older = older_sessions(&root.join("incremental"));
        let mut names: Vec<_> = older
            .iter()
            .map(|session| {
                session
                    .dir
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["s-9-c3-h3", "s-z-a1-h1"]);
        assert_eq!(older[0].lock.parent(), Some(krate.as_path()));
        assert!(older[0].lock.to_string_lossy().ends_with(".lock"));
        fs::remove_dir_all(root).unwrap();
    }
}
