//! Validates fixed Execution discovery outputs before Workspace mints durable worktree identity.

use super::{
    GitError, GitObjectId, MAX_GIT_STDERR_BYTES, MAX_GIT_STDOUT_BYTES, parse_terminal_path,
    raw_path,
};
use crate::{
    execution::{DiscoveryOperationRef, GitDiscoveryEvidence, GitDiscoveryQuery},
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
/// Cancellation, truncation, incomplete drain, unknown/failed exits, malformed/duplicate/foreign output,
/// symlinks, unsupported bare candidates and inconsistent Git administrative backpointers fail closed.
/// Callers pass the returned paths to DurableWorkspace::resolve_worktree for final native revalidation.
pub fn validate_discovery(
    candidate_cwd: &Path,
    operation: &DiscoveryOperationRef,
    outputs: &[GitDiscoveryEvidence],
) -> Result<DiscoveredWorktree, GitError> {
    validate_native_identity(
        candidate_cwd,
        &parse_discovery_evidence(operation, outputs)?,
    )
}

/// The paths and listing bytes one correlated discovery triple asserts, before anything is opened.
///
/// This is *evidence*, not identity. Its paths are exactly what Git printed: they have not been
/// canonicalized, their descriptors have not been inspected, and no Git administrative file has
/// been read. Only [`validate_native_identity`] or `DurableWorkspace::resolve_worktree` turns it
/// into an identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedDiscovery {
    /// Raw `--show-toplevel` path, terminal-parsed but not canonicalized.
    top: PathBuf,
    /// Raw absolute `--git-common-dir` path, terminal-parsed but not canonicalized.
    common: PathBuf,
    /// Raw NUL-delimited `worktree list --porcelain -z` bytes, structurally unvalidated.
    listing: Vec<u8>,
}

impl ParsedDiscovery {
    /// Returns the asserted top-level path exactly as Git printed it.
    pub fn top(&self) -> &Path {
        &self.top
    }
    /// Returns the asserted absolute common administrative directory as Git printed it.
    pub fn common(&self) -> &Path {
        &self.common
    }
    /// Returns the raw worktree listing bytes for later validation against a canonical root.
    pub fn listing(&self) -> &[u8] {
        &self.listing
    }
}

/// Parses one correlated discovery triple without touching the filesystem.
///
/// Checks exactly three correlated outputs, their operation identity, cancellation, exit status,
/// truncation, drain completeness and byte bounds; rejects a duplicated or missing query slot;
/// parses the two terminal paths; and requires the common directory to be absolute, as the fixed
/// `--path-format=absolute` query guarantees. A Git that does not support that flag is reported as
/// [`GitError::UnsupportedDiscoveryGit`] rather than as malformed output.
///
/// Performs no I/O whatsoever: no `stat`, no `open`, no directory traversal, no process. This is
/// the half the daemon is permitted to run for helper-supplied Claude evidence, where reading
/// repository source or Git administrative files daemon-side is forbidden.
pub fn parse_discovery_evidence(
    operation: &DiscoveryOperationRef,
    outputs: &[GitDiscoveryEvidence],
) -> Result<ParsedDiscovery, GitError> {
    if outputs.len() != 3 {
        return Err(GitError::InvalidDiscovery);
    }
    let mut values: [Option<&[u8]>; 3] = [None, None, None];
    for output in outputs {
        if output.query() == GitDiscoveryQuery::GitCommonDir
            && output.exit_status().code() != Some(0)
            && output
                .stderr()
                .bytes
                .windows(b"path-format".len())
                .any(|part| part == b"path-format")
            && output
                .stderr()
                .bytes
                .windows(b"unknown option".len())
                .any(|part| part == b"unknown option")
        {
            return Err(GitError::UnsupportedDiscoveryGit);
        }
        if output.operation() != operation
            || output.cancellation().is_some()
            || output.exit_status().code() != Some(0)
            || output.stdout().truncated
            || output.stderr().truncated
            || !output.stdout().complete
            || !output.stderr().complete
            || output.stdout().bytes.len() > MAX_GIT_STDOUT_BYTES
            || output.stderr().bytes.len() > MAX_GIT_STDERR_BYTES
        {
            return Err(GitError::InvalidDiscovery);
        }
        if output.query() == GitDiscoveryQuery::GitCommonDir
            && output
                .stdout()
                .bytes
                .starts_with(b"--path-format=absolute\n")
        {
            return Err(GitError::UnsupportedDiscoveryGit);
        }
        let slot = match output.query() {
            GitDiscoveryQuery::ShowTopLevel => 0,
            GitDiscoveryQuery::GitCommonDir => 1,
            GitDiscoveryQuery::WorktreeListPorcelainZ => 2,
        };
        if values[slot].replace(&output.stdout().bytes).is_some() {
            return Err(GitError::InvalidDiscovery);
        }
    }
    let top = parse_terminal_path(values[0].ok_or(GitError::InvalidDiscovery)?)?;
    let common = parse_terminal_path(values[1].ok_or(GitError::InvalidDiscovery)?)?;
    if !common.is_absolute() {
        return Err(GitError::InvalidDiscovery);
    }
    Ok(ParsedDiscovery {
        top,
        common,
        listing: values[2].ok_or(GitError::InvalidDiscovery)?.to_vec(),
    })
}

/// Resolves parsed discovery evidence into a native worktree identity.
///
/// This is the half that touches the filesystem: it canonicalizes the asserted paths through
/// descriptor-checked `real_directory`, requires the invocation directory to lie inside the
/// resolved root, validates the worktree listing against that canonical root, and cross-checks the
/// candidate's Git administrative backpointers. Symlinked paths, unsupported bare candidates,
/// foreign or duplicated listing entries and inconsistent backpointers all fail closed.
///
/// Callers still pass the returned paths to `DurableWorkspace::resolve_worktree`, which alone mints
/// durable nonce, native key and incarnation.
pub fn validate_native_identity(
    candidate_cwd: &Path,
    parsed: &ParsedDiscovery,
) -> Result<DiscoveredWorktree, GitError> {
    let (root, _) = real_directory(&parsed.top).map_err(|_| GitError::InvalidDiscovery)?;
    let (cwd, _) = real_directory(candidate_cwd).map_err(|_| GitError::InvalidDiscovery)?;
    if !cwd.starts_with(&root) {
        return Err(GitError::InvalidDiscovery);
    }
    let (common_dir, common_identity) =
        real_directory(&parsed.common).map_err(|_| GitError::InvalidDiscovery)?;
    validate_listing(&parsed.listing, &root)?;
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
///
/// I/O-free: `candidate` is compared as a path value and no listed worktree is ever opened.
/// Requires the candidate to appear exactly once and to be neither bare nor prunable.
pub fn validate_listing(bytes: &[u8], candidate: &Path) -> Result<(), GitError> {
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
