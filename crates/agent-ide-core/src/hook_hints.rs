//! Safe collection of the Claude hook key hints `/private/tmp/ai-k-<16 hex>`.
//!
//! A managed Claude MCP leaves one hint directory per candidate worktree (`key`, the rendezvous
//! key the hook must not run `git` to find, and `candidate-attachment`). Nothing ever removed
//! them, and the candidate cannot be recovered from the truncated digest in the name, so a hint is
//! judged only by what it names: the cached key path. A hint is collected only when all of these
//! hold, each checked again under its exclusive directory lock:
//!
//! * it is a private (`0700`) real directory of this user holding nothing but regular files of
//!   this user named `key`, `candidate-attachment` or a `key-*` publication temporary;
//! * the directory and every file in it are older than [`STALE_AFTER`](crate::hook_hints::STALE_AFTER);
//! * its `key` file is absent, or names an absolute path that definitely does not exist
//!   (`NotFound`) and whose runtime directory `ai-r-<16 hex>` does not exist either, so no daemon
//!   a hook could still reach is keyed by it.
//!
//! Anything else — a symlink, an unexpected entry, another owner or mode, an unreadable file, a
//! lookup failing for a reason other than `NotFound`, a busy lock — keeps the hint. A publisher
//! holds the shared lock of the directory through [`PublishGuard`](crate::hook_hints::PublishGuard) while it writes, so a
//! collection and a publication never overlap.

use std::fs::{self, File, OpenOptions};
use std::io::Read as _;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The temporary root the hint and runtime directories live in (its canonical path).
pub const TMP_ROOT: &str = "/private/tmp";
/// Name prefix of a key hint directory.
pub const KEY_CACHE_PREFIX: &str = "ai-k-";
/// File holding the cached rendezvous key path.
pub const KEY_FILE: &str = "key";
/// File holding the daemon-minted candidate attachment.
pub const ATTACHMENT_FILE: &str = "candidate-attachment";
/// Name prefix of a shared per-repository Claude runtime directory.
pub const RUNTIME_PREFIX: &str = "ai-r-";
/// Age beyond which an untouched hint may be collected.
pub const STALE_AFTER: Duration = Duration::from_secs(86_400);
/// Longest `key` file read, as written by the publisher.
const KEY_BYTES: u64 = 4096;

/// Returns the full BLAKE3 digest, as hex, of one canonical rendezvous key's raw path bytes.
pub fn rendezvous_identity(key: &Path) -> String {
    blake3::hash(key.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

/// Returns the runtime directory a daemon keyed by `key` uses below `tmp_root`.
pub fn runtime_dir(tmp_root: &Path, key: &Path) -> PathBuf {
    tmp_root.join(format!(
        "{RUNTIME_PREFIX}{}",
        &rendezvous_identity(key)[..16]
    ))
}

/// Applies one `flock` operation to the directory descriptor; `false` when not acquired.
fn lock(dir: &File, operation: libc::c_int) -> bool {
    loop {
        // SAFETY: `flock` needs only a valid open descriptor and stores nothing.
        if unsafe { libc::flock(dir.as_raw_fd(), operation) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Opens `dir` without following a link and checks that the descriptor is still the directory at
/// the path: a real directory of this user with mode `0700`.
fn open_private(dir: &Path) -> Option<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
        .ok()?;
    let opened = file.metadata().ok()?;
    let current = fs::symlink_metadata(dir).ok()?;
    // SAFETY: `geteuid` has no preconditions.
    (current.is_dir()
        && (current.dev(), current.ino()) == (opened.dev(), opened.ino())
        && current.uid() == unsafe { libc::geteuid() }
        && current.mode() & 0o777 == 0o700)
        .then_some(file)
}

/// Shared lock of one hint directory, held while a publisher writes into it.
#[derive(Debug)]
pub struct PublishGuard {
    /// The locked directory descriptor; closing it releases the lock.
    _directory: File,
}

impl PublishGuard {
    /// Waits for the shared lock of `dir`, which must be a private real directory.
    ///
    /// `None` when the directory is not one, or was removed or replaced while waiting (a
    /// collection ran); the caller prepares the directory again and retries.
    pub fn acquire(dir: &Path) -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir)
            .ok()?;
        if !lock(&file, libc::LOCK_SH) {
            return None;
        }
        // The path must still name the directory that was locked.
        still_the_directory(&file, dir).then_some(Self { _directory: file })
    }
}

/// What one pass over the temporary directory found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Collection {
    /// Hint directories recognized by name.
    pub hints: usize,
    /// Stale hints removed (or, in a dry run, removable).
    pub stale: usize,
}

/// Whether `name` is `ai-k-` followed by exactly sixteen lowercase hexadecimal digits.
fn is_hint_name(name: &str) -> bool {
    name.strip_prefix(KEY_CACHE_PREFIX).is_some_and(|hex| {
        hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Whether `path` is absolute and lexically normalized, as the hook requires of a cached key.
fn usable_key(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

/// Whether the hint directory `dir`, locked and verified by the caller, is stale at `now`.
fn is_stale(dir: &Path, tmp_root: &Path, now: SystemTime) -> bool {
    let old = |metadata: &fs::Metadata| {
        metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > STALE_AFTER)
    };
    if !fs::symlink_metadata(dir).is_ok_and(|metadata| old(&metadata)) {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut key_file = None;
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let name = entry.file_name();
        let known =
            name == KEY_FILE || name == ATTACHMENT_FILE || name.as_bytes().starts_with(b"key-");
        let regular = fs::symlink_metadata(entry.path()).is_ok_and(|metadata| {
            // SAFETY: `geteuid` has no preconditions.
            metadata.file_type().is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && old(&metadata)
        });
        if !known || !regular {
            return false;
        }
        if name == KEY_FILE {
            key_file = Some(entry.path());
        }
    }
    let Some(key_file) = key_file else {
        // Never published (or lost): nothing a hook could read.
        return true;
    };
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&key_file)
    else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(KEY_BYTES).read_to_end(&mut bytes).is_err() {
        return false;
    }
    let key = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
    if !usable_key(&key) {
        // The hook rejects it too: no candidate can use this hint.
        return true;
    }
    let missing = |path: &Path| matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound);
    missing(&key) && missing(&runtime_dir(tmp_root, &key))
}

/// Whether `path` still names the directory `held` has open: same device and inode.
fn still_the_directory(held: &File, path: &Path) -> bool {
    open_private(path)
        .and_then(|current| current.metadata().ok().zip(held.metadata().ok()))
        .is_some_and(|(a, b)| (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Collects (or, without `apply`, counts) the stale hint directories directly below `tmp_root`.
///
/// Each candidate is opened without following links, its exclusive lock is taken without waiting
/// (a publisher holds the shared one), and the directory is checked to still be the one locked
/// before it is judged and again before it is removed, so a hint refreshed or replaced meanwhile
/// is never lost. A dry run takes no lock.
///
/// `publishers_safe` is asked immediately before every removal, after the directory was judged
/// stale and locked, and must answer `true` only while no process that publishes hints without
/// the lock can exist; a `false` keeps the hint.
pub fn collect(
    tmp_root: &Path,
    apply: bool,
    now: SystemTime,
    publishers_safe: &dyn Fn() -> bool,
) -> Collection {
    collect_with(tmp_root, apply, now, publishers_safe, &|_| {})
}

/// [`collect`] calling `opened` with each candidate path between opening and locking it.
fn collect_with(
    tmp_root: &Path,
    apply: bool,
    now: SystemTime,
    publishers_safe: &dyn Fn() -> bool,
    opened: &dyn Fn(&Path),
) -> Collection {
    let mut found = Collection::default();
    let Ok(entries) = fs::read_dir(tmp_root) else {
        return found;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(is_hint_name) {
            continue;
        }
        found.hints += 1;
        let path = entry.path();
        let Some(dir) = open_private(&path) else {
            continue;
        };
        if !apply {
            found.stale += usize::from(is_stale(&path, tmp_root, now));
            continue;
        }
        opened(&path);
        if !lock(&dir, libc::LOCK_EX | libc::LOCK_NB) {
            continue;
        }
        // The lock is held to the end of the iteration.
        if still_the_directory(&dir, &path)
            && is_stale(&path, tmp_root, now)
            && still_the_directory(&dir, &path)
            && publishers_safe()
        {
            found.stale += usize::from(fs::remove_dir_all(&path).is_ok());
        }
    }
    found
}

#[cfg(test)]
#[path = "hook_hints_tests.rs"]
mod tests;
