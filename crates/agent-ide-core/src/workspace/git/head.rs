//! The checked-out branch or detached commit of one worktree, read from Git's own administrative
//! files without running Git, so a branch another process switched is noticed between calls.

use std::path::{Path, PathBuf};

/// Largest `HEAD`, gitfile or loose ref read; real ones are a single short line.
const MAX_SMALL_FILE: u64 = 4096;
/// Largest `packed-refs` read while resolving a branch; a larger one leaves the commit unknown.
const MAX_PACKED_REFS: u64 = 8 << 20;

/// One worktree's `HEAD`: the symbolic ref or detached commit, plus the commit it resolves to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadState {
    /// `refs/heads/<branch>` while a branch is checked out, else the detached commit id.
    target: String,
    /// Resolved commit id; absent for an unborn branch or an unreadable ref.
    commit: Option<String>,
}

impl HeadState {
    /// Reads `HEAD` of the worktree at `root` whose common Git directory is `common`; `None` when
    /// the worktree's Git files cannot be read.
    pub fn read(root: &Path, common: &Path) -> Option<Self> {
        let git_dir = admin_dir(root)?;
        let head = read_bounded(&git_dir.join("HEAD"), MAX_SMALL_FILE)?;
        let head = head.trim();
        Some(match head.strip_prefix("ref: ") {
            Some(reference) => Self {
                target: reference.to_owned(),
                commit: resolve(&git_dir, common, reference),
            },
            None => Self {
                target: head.to_owned(),
                commit: Some(head.to_owned()),
            },
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

/// Returns the worktree's own Git directory: `.git` itself, or the target of a linked gitfile.
fn admin_dir(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }
    let gitfile = read_bounded(&dot_git, MAX_SMALL_FILE)?;
    let admin = PathBuf::from(gitfile.trim().strip_prefix("gitdir: ")?);
    Some(if admin.is_absolute() {
        admin
    } else {
        root.join(admin)
    })
}

/// Resolves `reference` through a loose ref (worktree-private first, then shared) or the shared
/// `packed-refs`.
fn resolve(git_dir: &Path, common: &Path, reference: &str) -> Option<String> {
    [git_dir, common]
        .iter()
        .find_map(|dir| read_bounded(&dir.join(reference), MAX_SMALL_FILE))
        .map(|commit| commit.trim().to_owned())
        .or_else(|| {
            read_bounded(&common.join("packed-refs"), MAX_PACKED_REFS)?
                .lines()
                .find_map(|line| {
                    let (commit, name) = line.split_once(' ')?;
                    (name == reference).then(|| commit.to_owned())
                })
        })
}

/// Reads one UTF-8 file of at most `limit` bytes.
fn read_bounded(path: &Path, limit: u64) -> Option<String> {
    (std::fs::metadata(path).ok()?.len() <= limit)
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A branch switch names both commits and branches; a same-branch commit and an unchanged
    /// head say nothing; a linked worktree resolves through its gitfile and `packed-refs`.
    #[test]
    fn branch_switch_is_noticed_and_same_branch_commit_is_not() {
        let base = std::env::temp_dir().join(format!("agent-ide-head-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let common = base.join("repo/.git");
        let admin = common.join("worktrees/wt");
        let root = base.join("wt");
        std::fs::create_dir_all(common.join("refs/heads/claude")).unwrap();
        std::fs::create_dir_all(&admin).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".git"), format!("gitdir: {}\n", admin.display())).unwrap();
        std::fs::write(common.join("refs/heads/claude/a"), "4e2e2e2aaaa\n").unwrap();
        std::fs::write(
            common.join("packed-refs"),
            "# pack-refs with: peeled\n9daac64bbbb refs/heads/claude/b\n",
        )
        .unwrap();
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/claude/a\n").unwrap();
        let before = HeadState::read(&root, &common).unwrap();
        assert_eq!(before.moved_notice(&before), None);

        std::fs::write(common.join("refs/heads/claude/a"), "5f5f5f5cccc\n").unwrap();
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

        std::fs::write(admin.join("HEAD"), "0123456789abcdef\n").unwrap();
        let detached = HeadState::read(&root, &common).unwrap();
        assert!(
            switched
                .moved_notice(&detached)
                .is_some_and(|notice| notice.contains("9daac64 → 0123456 (claude/b → detached)"))
        );
    }
}
