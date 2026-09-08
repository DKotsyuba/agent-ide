//! Validates fixed Execution discovery outputs before Workspace mints durable worktree identity.

use super::{
    GitError, GitObjectId, MAX_GIT_STDERR_BYTES, MAX_GIT_STDOUT_BYTES, parse_terminal_path,
    raw_path,
};
use crate::{
    execution::{DiscoveryOperationRef, GitDiscoveryQuery, RawGitDiscovery},
    workspace::{
        authority::WorktreeRef,
        durable::real_directory,
        observation::{SourceReadLimits, read_authorized_source},
    },
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Maximum worktree-list entries inspected without recursively following other worktrees.
pub const MAX_DISCOVERY_WORKTREES: usize = 256;

/// Canonical candidate paths validated against a correlated listing and Git administrative identity.
/// This is a discovery result only; DurableWorkspace still allocates incarnation and authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredWorktree {
    /// Exact canonical --show-toplevel root; nested invocation cwd never becomes an identity root.
    root: PathBuf,
    /// Canonical shared Git administrative directory validated against the candidate's backpointer.
    common_dir: PathBuf,
}

impl DiscoveredWorktree {
    /// Returns the candidate worktree's canonical top-level root.
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Returns the same verified repository top-level root, including for linked worktrees.
    pub fn repository_root(&self) -> &Path {
        &self.root
    }
    /// Returns the verified absolute common administrative directory.
    pub fn common_dir(&self) -> &Path {
        &self.common_dir
    }
}

/// Checks exactly three correlated bounded successful discovery outputs and one candidate identity.
/// Common-dir must be absolute from the fixed --path-format=absolute query; raw Unix bytes
/// survive LF/NUL parsing. Other listed worktrees are parsed uniquely but never opened or scanned.
/// Truncation, incomplete drain, unknown/failed exits, malformed/duplicate/foreign candidate output,
/// symlinks, unsupported bare candidates and inconsistent Git administrative backpointers fail closed.
/// Callers pass the returned paths to DurableWorkspace::resolve_worktree for final native revalidation.
pub fn validate_discovery(
    candidate_cwd: &Path,
    operation: &DiscoveryOperationRef,
    outputs: &[RawGitDiscovery],
) -> Result<DiscoveredWorktree, GitError> {
    if outputs.len() != 3 {
        return Err(GitError::InvalidDiscovery);
    }
    let mut values: [Option<&[u8]>; 3] = [None, None, None];
    for output in outputs {
        if output.query == GitDiscoveryQuery::GitCommonDir
            && output.exit_status.code() != Some(0)
            && output
                .stderr
                .bytes
                .windows(b"path-format".len())
                .any(|part| part == b"path-format")
            && output
                .stderr
                .bytes
                .windows(b"unknown option".len())
                .any(|part| part == b"unknown option")
        {
            return Err(GitError::UnsupportedDiscoveryGit);
        }
        if &output.operation != operation
            || output.exit_status.code() != Some(0)
            || output.stdout.truncated
            || output.stderr.truncated
            || !output.stdout.complete
            || !output.stderr.complete
            || output.stdout.bytes.len() > MAX_GIT_STDOUT_BYTES
            || output.stderr.bytes.len() > MAX_GIT_STDERR_BYTES
        {
            return Err(GitError::InvalidDiscovery);
        }
        if output.query == GitDiscoveryQuery::GitCommonDir
            && output.stdout.bytes.starts_with(b"--path-format=absolute\n")
        {
            return Err(GitError::UnsupportedDiscoveryGit);
        }
        let slot = match output.query {
            GitDiscoveryQuery::ShowTopLevel => 0,
            GitDiscoveryQuery::GitCommonDir => 1,
            GitDiscoveryQuery::WorktreeListPorcelainZ => 2,
        };
        if values[slot].replace(&output.stdout.bytes).is_some() {
            return Err(GitError::InvalidDiscovery);
        }
    }
    let top = parse_terminal_path(values[0].ok_or(GitError::InvalidDiscovery)?)?;
    let (root, _) = real_directory(&top).map_err(|_| GitError::InvalidDiscovery)?;
    let (cwd, _) = real_directory(candidate_cwd).map_err(|_| GitError::InvalidDiscovery)?;
    if !cwd.starts_with(&root) {
        return Err(GitError::InvalidDiscovery);
    }
    let raw_common = parse_terminal_path(values[1].ok_or(GitError::InvalidDiscovery)?)?;
    if !raw_common.is_absolute() {
        return Err(GitError::InvalidDiscovery);
    }
    let common = raw_common;
    let (common_dir, common_identity) =
        real_directory(&common).map_err(|_| GitError::InvalidDiscovery)?;
    validate_listing(values[2].ok_or(GitError::InvalidDiscovery)?, &root)?;
    let dot_git = root.join(".git");
    if let Ok((directory, identity)) = real_directory(&dot_git) {
        if directory != common_dir || identity != common_identity {
            return Err(GitError::InvalidDiscovery);
        }
    } else {
        let gitfile = administrative_file(&root, &common_dir, ".git")?;
        let raw_admin = gitfile
            .strip_prefix(b"gitdir: ")
            .ok_or(GitError::InvalidDiscovery)?;
        let admin_path = parse_terminal_path(raw_admin)?;
        let admin_path = if admin_path.is_absolute() {
            admin_path
        } else {
            root.join(admin_path)
        };
        let (admin, _) = real_directory(&admin_path).map_err(|_| GitError::InvalidDiscovery)?;
        if admin.parent() != Some(common_dir.join("worktrees").as_path()) {
            return Err(GitError::InvalidDiscovery);
        }
        let shared = parse_terminal_path(&administrative_file(&admin, &common_dir, "commondir")?)?;
        let shared = if shared.is_absolute() {
            shared
        } else {
            admin.join(shared)
        };
        let (resolved, identity) =
            real_directory(&shared).map_err(|_| GitError::InvalidDiscovery)?;
        if resolved != common_dir || identity != common_identity {
            return Err(GitError::InvalidDiscovery);
        }
        let backpointer =
            parse_terminal_path(&administrative_file(&admin, &common_dir, "gitdir")?)?;
        let backpointer = if backpointer.is_absolute() {
            backpointer
        } else {
            admin.join(backpointer)
        };
        if backpointer.file_name() != Some(std::ffi::OsStr::new(".git")) {
            return Err(GitError::InvalidDiscovery);
        }
        let (back_root, _) =
            real_directory(backpointer.parent().ok_or(GitError::InvalidDiscovery)?)
                .map_err(|_| GitError::InvalidDiscovery)?;
        if back_root != root || administrative_file(&root, &common_dir, ".git")? != gitfile {
            return Err(GitError::InvalidDiscovery);
        }
    }
    Ok(DiscoveredWorktree { root, common_dir })
}

/// Reads only a named bounded administrative file through the existing no-follow Workspace reader.
fn administrative_file(root: &Path, common: &Path, name: &str) -> Result<Vec<u8>, GitError> {
    let tree = WorktreeRef::from_discovery(
        root.to_path_buf(),
        root.to_path_buf(),
        common.to_path_buf(),
        1,
    )
    .map_err(|_| GitError::InvalidDiscovery)?;
    let read = read_authorized_source(
        &tree,
        Path::new(name),
        SourceReadLimits::new(4096, 4096).map_err(|_| GitError::InvalidDiscovery)?,
    )
    .map_err(|_| GitError::InvalidDiscovery)?;
    Ok(read.contents().to_vec())
}

/// Validates complete NUL-delimited porcelain records without normalizing or visiting peer paths.
fn validate_listing(bytes: &[u8], candidate: &Path) -> Result<(), GitError> {
    if bytes.is_empty() || !bytes.ends_with(&[0, 0]) {
        return Err(GitError::InvalidDiscovery);
    }
    let mut fields = bytes.split(|byte| *byte == 0).peekable();
    let mut paths = BTreeSet::new();
    let mut candidate_matches = 0usize;
    while let Some(first) = fields.next() {
        if first.is_empty() {
            if fields.peek().is_none() {
                break;
            }
            return Err(GitError::InvalidDiscovery);
        }
        let path = raw_path(
            first
                .strip_prefix(b"worktree ")
                .ok_or(GitError::InvalidDiscovery)?,
        );
        if !super::is_normal_absolute(&path)
            || !paths.insert(path.clone())
            || paths.len() > MAX_DISCOVERY_WORKTREES
        {
            return Err(GitError::InvalidDiscovery);
        }
        let mut head = false;
        let mut branch = false;
        let mut detached = false;
        let mut bare = false;
        let mut locked = false;
        let mut prunable = false;
        loop {
            let field = fields.next().ok_or(GitError::InvalidDiscovery)?;
            if field.is_empty() {
                break;
            }
            if let Some(oid) = field.strip_prefix(b"HEAD ") {
                if head {
                    return Err(GitError::InvalidDiscovery);
                }
                GitObjectId::parse(oid)?;
                head = true;
            } else if let Some(name) = field.strip_prefix(b"branch ") {
                if branch || !name.starts_with(b"refs/heads/") || name.len() == 11 {
                    return Err(GitError::InvalidDiscovery);
                }
                branch = true;
            } else if field == b"detached" {
                if detached {
                    return Err(GitError::InvalidDiscovery);
                }
                detached = true;
            } else if field == b"bare" {
                if bare {
                    return Err(GitError::InvalidDiscovery);
                }
                bare = true;
            } else if field == b"locked" || field.starts_with(b"locked ") {
                if locked {
                    return Err(GitError::InvalidDiscovery);
                }
                locked = true;
            } else if field == b"prunable" || field.starts_with(b"prunable ") {
                if prunable {
                    return Err(GitError::InvalidDiscovery);
                }
                prunable = true;
            } else {
                return Err(GitError::InvalidDiscovery);
            }
        }
        if bare {
            if head || branch || detached {
                return Err(GitError::InvalidDiscovery);
            }
        } else if !head || branch == detached {
            return Err(GitError::InvalidDiscovery);
        }
        if path == candidate {
            if bare || prunable {
                return Err(GitError::InvalidDiscovery);
            }
            candidate_matches += 1;
        }
    }
    if candidate_matches != 1 {
        return Err(GitError::InvalidDiscovery);
    }
    Ok(())
}
