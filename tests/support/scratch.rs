//! Scratch directory ownership shared by the integration contracts that build their own roots.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// Owns one scratch tree and removes all of it when dropped — after a pass and after a failing
/// assertion alike. The tree is a private per-test `root` and the directory `dir` the test works
/// in (the root itself, or a child of it): a scheduler's lease state lives beside its cache root,
/// so owning the whole root removes that too. The system temporary directory is never touched.
pub struct Scratch {
    /// What is removed on drop.
    root: PathBuf,
    /// What the test sees.
    dir: PathBuf,
}

impl Scratch {
    /// Owns the existing directory `dir` alone.
    #[allow(dead_code, reason = "only the contracts without sibling state use it")]
    pub fn own(dir: PathBuf) -> Self {
        Self {
            root: dir.clone(),
            dir,
        }
    }

    /// Owns the existing `root` tree and presents its child `dir` to the test.
    #[allow(
        dead_code,
        reason = "only the contracts with sibling state beside a root use it"
    )]
    pub fn own_tree(root: PathBuf, dir: PathBuf) -> Self {
        Self { root, dir }
    }

    /// Returns the presented directory as an owned path.
    #[allow(
        dead_code,
        reason = "only the contracts that hand out owned roots use it"
    )]
    pub fn path(&self) -> PathBuf {
        self.dir.clone()
    }
}

impl Deref for Scratch {
    /// Derefs to the directory (or database) path.
    type Target = Path;

    /// The presented directory.
    fn deref(&self) -> &Path {
        &self.dir
    }
}

impl AsRef<Path> for Scratch {
    /// The presented directory, for APIs that take `impl AsRef<Path>`.
    fn as_ref(&self) -> &Path {
        &self.dir
    }
}

impl AsRef<std::ffi::OsStr> for Scratch {
    /// The presented directory, for process environment and argument APIs.
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.dir.as_os_str()
    }
}

impl Drop for Scratch {
    /// Removes the whole tree.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
