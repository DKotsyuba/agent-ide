//! The checked-out branch or detached commit of one worktree, read from Git's own administrative
//! files without running Git, so a branch another process switched is noticed between calls.
//!
//! Every read stays inside the worktree's already-validated Git directories: the live `.git`
//! backpointers are rechecked exactly as Execution does before each Git spawn
//! (`current_admin_dir`), and each file is opened by a no-follow walk from `/`, so a `.git`,
//! gitfile or ref redirected elsewhere is refused instead of read. Only a well-formed ref name
//! and full object ids are kept, so a notice never carries arbitrary file text.

use super::{GitObjectId, discovery::current_admin_dir};
use crate::workspace::{
    authority::WorktreeRef,
    observation::{read_authorized_resolution_input, valid_relative_path},
};
use std::path::Path;

/// Largest `HEAD` or loose ref read; real ones are a single short line.
const MAX_SMALL_FILE: usize = 4096;
/// Largest `packed-refs` read while resolving a branch; a larger one leaves the commit unknown.
const MAX_PACKED_REFS: usize = 8 << 20;

/// One worktree's `HEAD`: the symbolic ref or detached commit, plus the commit it resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadState {
    /// `refs/heads/<branch>` while a branch is checked out, else the detached commit id.
    target: String,
    /// Resolved commit id; absent for an unborn branch or an unreadable ref.
    commit: Option<String>,
}

impl HeadState {
    /// Reads `HEAD` of the worktree at `root` whose common Git directory is `common`, both the
    /// exact canonical paths stored at activation.
    ///
    /// `None` when the live Git metadata no longer validates (a symlinked or redirected `.git`,
    /// gitfile or backpointer), a file is missing, linked, oversized or not UTF-8, the symbolic ref
    /// is not a plain `refs/…` name, or a detached `HEAD` is not a full object id. Reads at most
    /// the validation files, `HEAD`, one loose ref per Git directory and `packed-refs`.
    pub fn read(root: &Path, common: &Path) -> Option<Self> {
        let git_dir = current_admin_dir(root, common).ok()?;
        let head = read_confined(&git_dir, "HEAD", MAX_SMALL_FILE)?;
        let head = head.trim();
        Some(match head.strip_prefix("ref: ") {
            Some(reference) if plain_ref(reference) => Self {
                target: reference.to_owned(),
                commit: resolve(&git_dir, common, reference),
            },
            Some(_) => return None,
            None => {
                let commit = object_id(head)?;
                Self {
                    target: commit.clone(),
                    commit: Some(commit),
                }
            }
        })
    }

    /// Returns the one-line status-plate notice when `now` checks out another branch or detached
    /// commit than `self`; a commit on the same branch keeps the target and says nothing.
    pub fn moved_notice(&self, now: &Self) -> Option<String> {
        (self.target != now.target).then(|| {
            format!(
                "git: HEAD moved {} → {} ({} → {}) outside Agent IDE; earlier indexed answers may \
                 be stale",
                short(self.commit.as_deref()),
                short(now.commit.as_deref()),
                self.branch(),
                now.branch()
            )
        })
    }

    /// Names the checked-out branch, the full ref of a non-branch symbolic target, or `detached`.
    fn branch(&self) -> &str {
        match self.target.strip_prefix("refs/heads/") {
            Some(branch) => branch,
            None if self.target.starts_with("refs/") => &self.target,
            None => "detached",
        }
    }
}

/// Abbreviates a commit id to seven characters, or `unborn` when there is none.
fn short(commit: Option<&str>) -> &str {
    commit.map_or("unborn", |commit| commit.get(..7).unwrap_or(commit))
}

/// Accepts a symbolic target only as a plain relative `refs/…` name: no empty, `.` or `..`
/// component, no whitespace or control character, so it cannot climb out of a Git directory.
fn plain_ref(reference: &str) -> bool {
    reference.starts_with("refs/")
        && valid_relative_path(Path::new(reference))
        && !reference
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Returns `value` as a full lowercase object id, or `None` for anything else (including zeros).
fn object_id(value: &str) -> Option<String> {
    GitObjectId::parse(value.as_bytes())
        .ok()
        .flatten()
        .map(|id| id.as_str().to_owned())
}

/// Resolves `reference` through a loose ref (worktree-private first, then shared) or the shared
/// `packed-refs`; only a full object id is returned.
fn resolve(git_dir: &Path, common: &Path, reference: &str) -> Option<String> {
    [git_dir, common]
        .iter()
        .find_map(|dir| object_id(read_confined(dir, reference, MAX_SMALL_FILE)?.trim()))
        .or_else(|| {
            read_confined(common, "packed-refs", MAX_PACKED_REFS)?
                .lines()
                .find_map(|line| {
                    let (commit, name) = line.split_once(' ')?;
                    (name == reference).then_some(commit).and_then(object_id)
                })
        })
}

/// Reads the UTF-8 file `relative` (at most `limit` bytes) beneath the canonical directory `dir`
/// through the Workspace no-follow reader: every component from `/` is opened without following
/// a symlink, and only a regular file is read.
fn read_confined(dir: &Path, relative: &str, limit: usize) -> Option<String> {
    let tree =
        WorktreeRef::from_discovery(dir.to_path_buf(), dir.to_path_buf(), dir.to_path_buf(), 1)
            .ok()?;
    let read = read_authorized_resolution_input(&tree, Path::new(relative), limit).ok()?;
    String::from_utf8(read.contents().to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A branch switch names both commits and branches; a same-branch commit and an unchanged
    /// head say nothing; a linked worktree resolves through its gitfile and `packed-refs`.
    #[test]
    fn branch_switch_is_noticed_and_same_branch_commit_is_not() {
        let base = scratch("switch");
        let (root, common, admin) = linked(&base);
        std::fs::write(common.join("refs/heads/claude/a"), oid("4e2e2e2") + "\n").unwrap();
        std::fs::write(
            common.join("packed-refs"),
            format!(
                "# pack-refs with: peeled\n{} refs/heads/claude/b\n",
                oid("9daac64")
            ),
        )
        .unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/claude/a\n").unwrap();
        let before = HeadState::read(&root, &common).unwrap();
        assert_eq!(before.moved_notice(&before), None);

        std::fs::write(common.join("refs/heads/claude/a"), oid("5f5f5f5") + "\n").unwrap();
        let committed = HeadState::read(&root, &common).unwrap();
        assert_eq!(before.moved_notice(&committed), None);

        std::fs::write(admin.join("HEAD"), "ref: refs/heads/claude/b\n").unwrap();
        let switched = HeadState::read(&root, &common).unwrap();
        assert_eq!(
            committed.moved_notice(&switched).as_deref(),
            Some(
                "git: HEAD moved 5f5f5f5 → 9daac64 (claude/a → claude/b) outside Agent IDE; \
                 earlier indexed answers may be stale"
            )
        );

        std::fs::write(admin.join("HEAD"), oid("0123456789abcdef") + "\n").unwrap();
        let detached = HeadState::read(&root, &common).unwrap();
        assert!(
            switched
                .moved_notice(&detached)
                .is_some_and(|notice| notice.contains("9daac64 → 0123456 (claude/b → detached)"))
        );

        std::fs::write(admin.join("HEAD"), "not-an-object-id\n").unwrap();
        assert_eq!(
            HeadState::read(&root, &common),
            None,
            "a detached HEAD that is not an object id is not echoed"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    /// Creates an empty canonical scratch directory, so no-follow walks from `/` can open it.
    fn scratch(name: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("agent-ide-head-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    /// Lays out the linked worktree `base/wt` of `base/repo` with the gitfile, `commondir` and
    /// `gitdir` backpointers Git writes; returns `(root, common, admin)`.
    fn linked(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let common = base.join("repo/.git");
        let admin = common.join("worktrees/wt");
        let root = base.join("wt");
        std::fs::create_dir_all(common.join("refs/heads/claude")).unwrap();
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("commondir"), "../..\n").unwrap();
        let backpointer = format!("{}\n", root.join(".git").display());
        std::fs::write(admin.join("gitdir"), backpointer).unwrap();
        (root, common, admin)
    }

    /// Pads an abbreviated commit id to a full 40-digit object id.
    fn oid(prefix: &str) -> String {
        format!("{prefix:0<40}")
    }

    /// Git files redirected after activation are refused, never read: a `.git` symlink, a gitfile
    /// naming a directory outside the common one, and a `HEAD` whose ref climbs out with `..`.
    #[test]
    fn redirected_git_files_are_not_read() {
        let base = scratch("redirected");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("refs/heads")).unwrap();
        std::fs::write(outside.join("HEAD"), "ref: refs/heads/outside-secret\n").unwrap();
        let outside_ref = format!("{}\n", oid("0badbad"));
        std::fs::write(outside.join("refs/heads/outside-secret"), outside_ref).unwrap();
        std::fs::write(outside.join("secret"), "SECRET-TEXT\n").unwrap();

        let standalone = base.join("standalone");
        std::fs::create_dir_all(&standalone).unwrap();
        std::os::unix::fs::symlink(&outside, standalone.join(".git")).unwrap();
        assert_eq!(
            HeadState::read(&standalone, &standalone.join(".git")),
            None,
            "a `.git` symlink is not followed"
        );

        let (root, common, admin) = linked(&base);
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/claude/a\n").unwrap();
        std::fs::write(common.join("refs/heads/claude/a"), oid("4e2e2e2")).unwrap();
        assert!(HeadState::read(&root, &common).is_some(), "valid layout");
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", outside.display()),
        )
        .unwrap();
        assert_eq!(
            HeadState::read(&root, &common),
            None,
            "a gitfile outside the common directory is not read"
        );

        std::fs::write(root.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(admin.join("HEAD"), "ref: ../../../../outside/secret\n").unwrap();
        assert_eq!(
            HeadState::read(&root, &common),
            None,
            "a ref name climbing out is refused"
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
