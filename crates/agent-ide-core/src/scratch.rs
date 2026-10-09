//! Test-only scratch directory that owns and removes its whole state.

use std::fs;
use std::ops::Deref;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes scratch directories created by concurrent tests of one process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// A unique owner-only directory below the temporary directory, removed with everything in it
/// when the value drops, however the test ends (a failing assertion unwinds through `Drop`).
///
/// Declare it before the owners that write into it so they drop first. A write that still lands
/// after the removal fails inside the vanished directory and recreates nothing, so no file
/// outlives the test in `TMPDIR`. The path is canonical, so it compares equal to what the code
/// under test resolves.
pub(crate) struct ScratchDir(PathBuf);

impl ScratchDir {
    /// Creates `agent-ide-<label>-<pid>-<n>` with mode `0700`.
    pub(crate) fn new(label: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "agent-ide-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("the scratch directory is created");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("the scratch directory is private");
        Self(fs::canonicalize(directory).expect("the scratch directory resolves"))
    }
}

impl Deref for ScratchDir {
    type Target = Path;

    /// The directory path.
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for ScratchDir {
    /// The directory path, for APIs that take `impl AsRef<Path>`.
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    /// Removes the whole directory; a few retries cover a store still closing on its worker.
    fn drop(&mut self) {
        for _ in 0..5 {
            if fs::remove_dir_all(&self.0).is_ok() || !self.0.exists() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// One SQLite test database (`state.sqlite`) inside a [`ScratchDir`], which owns the database, its
/// `-wal`/`-shm` companions, any owner lock and any backup a store makes.
///
/// Bind it before the owners that open the database (`let (database, owner) = ..`) so they drop
/// first.
pub(crate) struct ScratchDatabase {
    /// The reserved `state.sqlite` inside the directory.
    path: PathBuf,
    /// Removes the whole directory on drop, however the test ends.
    _directory: ScratchDir,
}

impl ScratchDatabase {
    /// Creates the private directory and reserves its `state.sqlite`.
    pub(crate) fn new(label: &str) -> Self {
        let directory = ScratchDir::new(label);
        Self {
            path: directory.join("state.sqlite"),
            _directory: directory,
        }
    }
}

impl Deref for ScratchDatabase {
    type Target = Path;

    /// The reserved database path.
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for ScratchDatabase {
    /// The reserved database path, for APIs that take `impl AsRef<Path>`.
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directory and its contents are gone after a normal drop and after an unwind, and a
    /// late write after removal cannot bring it back.
    #[test]
    fn scratch_directory_is_removed_after_pass_and_after_failure() {
        let kept = {
            let scratch = ScratchDir::new("scratch-pass");
            fs::write(scratch.join("state.sqlite-wal"), b"x").unwrap();
            fs::create_dir(scratch.join("nested")).unwrap();
            scratch.to_path_buf()
        };
        assert!(!kept.exists(), "{kept:?} survived a normal drop");

        let seen = std::sync::Mutex::new(None);
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let scratch = ScratchDir::new("scratch-fail");
            fs::write(scratch.join("file"), b"x").unwrap();
            *seen.lock().unwrap() = Some(scratch.to_path_buf());
            panic!("failing test body");
        }));
        assert!(unwound.is_err());
        let kept = seen.lock().unwrap().take().unwrap();
        assert!(!kept.exists(), "{kept:?} survived an unwind");
        assert!(fs::write(kept.join("late"), b"x").is_err());
    }
}
