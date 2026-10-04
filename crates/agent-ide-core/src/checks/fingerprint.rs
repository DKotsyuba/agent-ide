//! Cheap worktree input fingerprint for the scheduler's skip-unchanged rule (T20B).
//!
//! `git_worktree_fingerprint` hashes everything `git ls-files` considers part of the worktree
//! (tracked plus untracked, non-ignored files): for every listed path it mixes the path bytes,
//! the file's size and its mtime in nanoseconds into a blake3 digest. A content-identical rewrite
//! still bumps the mtime and is therefore reported as a change — the fingerprint must be cheap,
//! not perfect, because its only consumer decides between skipping and re-running a check.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Total budget for the `git ls-files` child before it is killed and the fingerprint is reported
/// as unknown (which the scheduler treats as "changed" and runs the check).
const GIT_BUDGET: Duration = Duration::from_secs(5);

/// Poll interval while waiting for the `git` child to exit.
const POLL_STEP: Duration = Duration::from_millis(10);

/// Fixed value mixed into the digest for a listed path that cannot be stat'ed any more (removed
/// between listing and stat): distinct from every size/mtime pair.
const MISSING_FILE_MARKER: u64 = u64::MAX;

/// Computes a deterministic fingerprint of `worktree`'s check-relevant inputs, or `None` when the
/// inputs cannot be fingerprinted cheaply.
///
/// `None` is returned when `worktree` is not a git checkout (no `.git` entry — a synchronous
/// check, so the common non-git case never spawns a process), when `git ls-files` fails, or when
/// it exceeds `GIT_BUDGET`. The scheduler treats `None` as "unknown, assume changed" and runs
/// the check. Spawned with a plain `std::process::Command` because the value must be computable
/// from a synchronous `Fn` (the scheduler off-threads it); the stdout pipe is drained on a helper
/// thread so a path list larger than the pipe buffer cannot deadlock the child while this thread
/// waits for its exit.
///
/// Besides every listed file's path, size and mtime, the digest also mixes the path and mtime of
/// every untracked directory git reports collapsed — including ignored ones, which never appear
/// in the plain listing. A project environment directory (a virtual environment, a build cache)
/// is usually gitignored, so without this the scheduler's skip-unchanged rule would keep a
/// durable `env missing` result even after the environment appeared.
pub fn git_worktree_fingerprint(worktree: &Path) -> Option<u64> {
    let listed = git_listed_paths(worktree)?;
    let mut hasher = blake3::Hasher::new();
    for path in listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
    {
        hasher.update(path);
        hasher.update(&[0]);
        let full = worktree.join(OsStr::from_bytes(path));
        match std::fs::metadata(&full) {
            Ok(metadata) => {
                hasher.update(&metadata.len().to_le_bytes());
                let mtime_ns = i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec());
                hasher.update(&(mtime_ns as u64).to_le_bytes());
            }
            Err(_) => {
                hasher.update(&MISSING_FILE_MARKER.to_le_bytes());
            }
        }
    }
    // Untracked directories, ignored ones included, as git's `--directory` collapses them: one
    // entry per directory (`env/`), stat'ed as the directory itself. An environment directory
    // that appears, disappears or rebuilds its top level therefore moves the fingerprint even
    // though none of its files is ever listed.
    for path in git_output(
        worktree,
        &[
            "ls-files",
            "-z",
            "--others",
            "--directory",
            "--no-empty-directory",
        ],
    )?
    .split(|byte| *byte == 0)
    .filter(|path| path.last() == Some(&b'/'))
    {
        hasher.update(path);
        hasher.update(&[0]);
        match std::fs::metadata(worktree.join(OsStr::from_bytes(path))) {
            Ok(metadata) => {
                let mtime_ns = i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec());
                hasher.update(&(mtime_ns as u64).to_le_bytes());
            }
            Err(_) => {
                hasher.update(&MISSING_FILE_MARKER.to_le_bytes());
            }
        }
    }
    let digest = hasher.finalize();
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&digest.as_bytes()[..8]);
    Some(u64::from_le_bytes(prefix))
}

/// `git ls-files -z --cached --others --exclude-standard` of `worktree`: every tracked and
/// untracked, non-ignored path as NUL-separated bytes, or `None` when `worktree` has no `.git`
/// entry, the child fails, or it exceeds `GIT_BUDGET`. Shared by the fingerprint and the
/// cross-language name index so both see the same candidate files.
pub(crate) fn git_listed_paths(worktree: &Path) -> Option<Vec<u8>> {
    git_output(
        worktree,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
}

/// The stdout of one read-only `git` query in `worktree`, or `None` when `worktree` has no
/// `.git` entry, the child fails, or it exceeds `GIT_BUDGET`.
///
/// Absolute program path, like every other git call of the daemon; `core.fsmonitor` is forced
/// off because a repository-configured fsmonitor hook would otherwise run unconfined here. The
/// stdout pipe is drained on a helper thread so a large answer cannot deadlock the child.
pub(crate) fn git_output(worktree: &Path, args: &[&str]) -> Option<Vec<u8>> {
    if !worktree.join(".git").exists() {
        return None;
    }
    let mut child = Command::new("/usr/bin/git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(worktree)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut listed = Vec::new();
        stdout.read_to_end(&mut listed).ok()?;
        Some(listed)
    });
    let deadline = Instant::now() + GIT_BUDGET;
    let exited_cleanly = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() >= deadline => break false,
            Ok(None) => thread::sleep(POLL_STEP),
            Err(_) => break false,
        }
    };
    if !exited_cleanly {
        let _ = child.kill();
        let _ = child.wait();
    }
    let listed = reader.join().ok()??;
    exited_cleanly.then_some(listed)
}

#[cfg(test)]
mod tests {
    use super::super::scheduler::FingerprintFn;
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    /// Creates a fresh empty scratch directory for fingerprint tests.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-fingerprint-{}-{name}-{}",
            std::process::id(),
            name.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir created");
        dir
    }

    /// Runs `git` with `args` in `dir`, panicking on failure.
    fn git(args: &[&str], dir: &Path) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// (T20B) The git-based fingerprint is stable across two calls, changes after a tracked edit
    /// and after adding an untracked non-ignored file, and is unchanged after writing an ignored
    /// file. If temp directories are not writable in the sandbox this test cannot run.
    #[test]
    fn git_fingerprint_is_stable_and_tracks_tracked_untracked_and_ignored_changes() {
        let dir = scratch_dir("git-repo");
        git(&["init", "-q"], &dir);
        std::fs::write(dir.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git(&["add", "."], &dir);

        let first = git_worktree_fingerprint(&dir).expect("a git repo has a fingerprint");
        let second = git_worktree_fingerprint(&dir).expect("a git repo has a fingerprint");
        assert_eq!(first, second, "stable across two calls with no changes");

        std::fs::write(dir.join("tracked.txt"), "two\n").unwrap();
        let after_tracked_edit = git_worktree_fingerprint(&dir).expect("fingerprint");
        assert_ne!(after_tracked_edit, first, "a tracked edit changes it");

        std::fs::write(dir.join("untracked.txt"), "new\n").unwrap();
        let after_untracked = git_worktree_fingerprint(&dir).expect("fingerprint");
        assert_ne!(
            after_untracked, after_tracked_edit,
            "a new non-ignored file changes it"
        );

        std::fs::write(dir.join("ignored.txt"), "noise\n").unwrap();
        assert_eq!(
            git_worktree_fingerprint(&dir),
            Some(after_untracked),
            "an ignored file does not change it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Outside a git checkout the fingerprint is unknown (`None`), so the scheduler runs.
    #[test]
    fn git_fingerprint_is_none_outside_a_git_checkout() {
        let dir = scratch_dir("no-repo");
        assert_eq!(git_worktree_fingerprint(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gitignored environment directory still moves the fingerprint: it is listed collapsed
    /// (`--directory`) with its own mtime, so a skip-unchanged baseline held against a durable
    /// `env missing` result ends the moment the environment directory appears or disappears.
    #[test]
    fn git_fingerprint_tracks_ignored_environment_directories() {
        let dir = scratch_dir("git-env-dir");
        git(&["init", "-q"], &dir);
        std::fs::write(dir.join(".gitignore"), ".venv/\ntracked.txt\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        git(&["add", "."], &dir);

        let before = git_worktree_fingerprint(&dir).expect("a git repo has a fingerprint");
        let executable = dir.join(".venv/bin/interpreter");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "stub\n").unwrap();
        let with_env = git_worktree_fingerprint(&dir).expect("fingerprint");
        assert_ne!(
            with_env, before,
            "an ignored environment directory appearing must move the fingerprint"
        );

        std::fs::remove_dir_all(dir.join(".venv")).unwrap();
        let without_env = git_worktree_fingerprint(&dir).expect("fingerprint");
        assert_ne!(
            without_env, with_env,
            "removing the environment directory must move the fingerprint"
        );
        assert_eq!(
            without_env, before,
            "an empty worktree returns to its earlier fingerprint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `FingerprintFn` alias is the scheduler-facing signature of this function.
    #[test]
    fn fingerprint_matches_the_scheduler_seam_type() {
        let seam: FingerprintFn = Arc::new(git_worktree_fingerprint);
        let dir = scratch_dir("seam");
        assert_eq!(seam(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
