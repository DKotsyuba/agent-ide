//! Bounded, NUL-safe operator Git metadata; does not provide session authority.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::{ChangeStatus, DiffCounts, FileDiff, GitCollectionState, GitSummary, StructureWarning};
use crate::workspace::git::{StatusKind, parse_porcelain_v2_z};

/// Maximum retained output of a metadata/patch query; excess is an unavailable fact.
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;

/// Executes a query with bounded output and a three-second deadline; source readers pass filter overrides.
fn execute(root: &Path, args: &[&str], overrides: &[String]) -> io::Result<Vec<u8>> {
    let mut command = Command::new("/usr/bin/git");
    command
        .args([
            "--no-pager",
            "--no-optional-locks",
            "--no-lazy-fetch",
            "-c",
            "core.fsmonitor=false",
            "-C",
        ])
        .arg(root);
    for setting in overrides {
        command.arg("-c").arg(setting);
    }
    let mut child = command
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Git output pipe unavailable"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(OUTPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                break Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Git query timed out",
                ));
            }
            Err(error) => break Err(error),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| io::Error::other("Git output reader failed"))??;
    if bytes.len() > OUTPUT_LIMIT as usize {
        return Err(io::Error::other("Git output limit reached"));
    }
    if !status?.success() {
        return Err(io::Error::other(
            "Git query failed or root is not a repository",
        ));
    }
    Ok(bytes)
}

/// One root's read-only Git settings, with every configured external content filter disabled.
struct Git<'a> {
    root: &'a Path,
    overrides: Vec<String>,
}

impl<'a> Git<'a> {
    /// Reads configuration names only; configuration lookup itself cannot invoke content filters.
    fn new(root: &'a Path) -> io::Result<Self> {
        let names = execute(root, &["config", "--null", "--name-only", "--list"], &[])?;
        let mut drivers = BTreeSet::new();
        for raw in names.split(|b| *b == 0).filter(|s| !s.is_empty()) {
            let key = std::str::from_utf8(raw)
                .map_err(|_| io::Error::other("invalid Git configuration key"))?;
            if let Some((driver, field)) = key.rsplit_once('.')
                && driver.to_ascii_lowercase().starts_with("filter.")
                && ["clean", "smudge", "process", "required"]
                    .iter()
                    .any(|s| field.eq_ignore_ascii_case(s))
            {
                drivers.insert(driver.to_owned());
            }
        }
        let mut overrides = Vec::new();
        for driver in drivers {
            for field in ["clean", "smudge", "process"] {
                overrides.push(format!("{driver}.{field}="));
            }
            overrides.push(format!("{driver}.required=false"));
        }
        Ok(Self { root, overrides })
    }

    /// Applies process-local overrides without modifying repository/global configuration files.
    fn run(&self, args: &[&str]) -> io::Result<Vec<u8>> {
        execute(self.root, args, &self.overrides)
    }
}

/// Retains exact Unix names from a complete NUL-delimited path list.
fn paths(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(io::Error::other("incomplete Git path list"));
    }
    Ok(bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsString::from_vec(p.to_vec())))
        .collect())
}

/// Lists ignored untracked paths/directories relative to the requested root.
pub(super) fn ignored(root: &Path) -> io::Result<Vec<PathBuf>> {
    let git = Git::new(root)?;
    let bytes = git.run(&[
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
        "-z",
        "--",
        ".",
    ])?;
    Ok(paths(&bytes)?
        .into_iter()
        .map(|p| p.components().collect())
        .collect())
}

/// Collects status first; index/worktree line and hunk counts can independently be unavailable.
pub(super) fn collect(
    root: &Path,
    include_counts: bool,
    warnings: &mut Vec<StructureWarning>,
) -> io::Result<(BTreeMap<PathBuf, FileDiff>, GitSummary)> {
    let git = Git::new(root)?;
    if !git.overrides.is_empty() {
        warnings.push(StructureWarning {
            path: None,
            reason: super::GIT_FILTER_NOTICE.into(),
        });
    }
    let prefix_bytes = git.run(&["rev-parse", "--show-prefix"])?;
    let prefix = PathBuf::from(OsString::from_vec(
        prefix_bytes
            .strip_suffix(b"\n")
            .unwrap_or(&prefix_bytes)
            .to_vec(),
    ));
    let bytes = git.run(&[
        "status",
        "--porcelain=v2",
        "-z",
        "--untracked-files=all",
        "--",
        ".",
    ])?;
    let status =
        parse_porcelain_v2_z(&bytes).map_err(|_| io::Error::other("invalid Git status"))?;
    let mut changes = BTreeMap::new();
    for item in status
        .tracked()
        .iter()
        .chain(status.conflicts())
        .chain(status.untracked())
    {
        let Ok(path) = item.path().strip_prefix(&prefix) else {
            continue;
        };
        let xy = item.status().unwrap_or(*b"??");
        let kind = if item.kind() == StatusKind::Untracked {
            ChangeStatus::Untracked
        } else if item.kind() == StatusKind::Unmerged {
            ChangeStatus::Conflicted
        } else if item.original_path().is_some() {
            ChangeStatus::Renamed
        } else if xy.contains(&b'D') {
            ChangeStatus::Deleted
        } else if xy.contains(&b'A') {
            ChangeStatus::Added
        } else {
            ChangeStatus::Modified
        };
        changes.insert(
            path.into(),
            FileDiff {
                status: kind,
                staged: if xy[0] == b'.' {
                    DiffCounts::zero()
                } else {
                    DiffCounts::default()
                },
                unstaged: if xy[1] == b'.' {
                    DiffCounts::zero()
                } else {
                    DiffCounts::default()
                },
                previous_path: item
                    .original_path()
                    .and_then(|p| p.strip_prefix(&prefix).ok())
                    .map(Path::to_path_buf),
            },
        );
    }
    if include_counts {
        for staged in [true, false] {
            match diff(&git, staged, warnings) {
                Ok(counts) => {
                    for (path, counts) in counts {
                        let entry = changes.entry(path).or_insert(FileDiff {
                            status: ChangeStatus::Deleted,
                            staged: DiffCounts::zero(),
                            unstaged: DiffCounts::zero(),
                            previous_path: None,
                        });
                        if staged {
                            entry.staged = counts;
                        } else {
                            entry.unstaged = counts;
                        }
                    }
                }
                Err(error) => warnings.push(StructureWarning {
                    path: None,
                    reason: format!(
                        "{} diff counts unavailable: {error}",
                        if staged { "staged" } else { "unstaged" }
                    ),
                }),
            }
        }
    }
    let head = git
        .run(&["rev-parse", "--verify", "HEAD"])
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim().into());
    Ok((
        changes,
        GitSummary {
            state: GitCollectionState::Available,
            head,
        },
    ))
}

/// Reads NUL numstat paths and counts patch headers in the identical no-renames comparison order.
fn diff(
    git: &Git<'_>,
    staged: bool,
    warnings: &mut Vec<StructureWarning>,
) -> io::Result<Vec<(PathBuf, DiffCounts)>> {
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--relative",
    ];
    if staged {
        args.push("--cached");
    }
    args.extend(["--numstat", "-z", "--", "."]);
    let bytes = git.run(&args)?;
    let mut result = Vec::new();
    for record in bytes.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        let mut fields = record.splitn(3, |b| *b == b'\t');
        let added = fields
            .next()
            .ok_or_else(|| io::Error::other("invalid numstat"))?;
        let removed = fields
            .next()
            .ok_or_else(|| io::Error::other("invalid numstat"))?;
        let path = fields
            .next()
            .ok_or_else(|| io::Error::other("invalid numstat path"))?;
        let number = |v: &[u8]| {
            if v == b"-" {
                Ok(None)
            } else {
                std::str::from_utf8(v)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .map(Some)
                    .ok_or_else(|| io::Error::other("invalid numstat count"))
            }
        };
        result.push((
            PathBuf::from(OsString::from_vec(path.to_vec())),
            DiffCounts {
                added_lines: number(added)?,
                removed_lines: number(removed)?,
                hunks: None,
            },
        ));
    }
    if result.is_empty() {
        return Ok(result);
    }
    let tail = args.len() - 4;
    args.truncate(tail);
    args.extend(["--patch", "--unified=0", "--", "."]);
    match git.run(&args) {
        Ok(patch) => {
            let mut hunks = Vec::<u64>::new();
            for line in patch.split(|b| *b == b'\n') {
                if line.starts_with(b"diff --git ") {
                    hunks.push(0);
                } else if line.starts_with(b"@@ ")
                    && let Some(count) = hunks.last_mut()
                {
                    *count += 1;
                }
            }
            if hunks.len() == result.len() {
                for ((_, count), hunks) in result.iter_mut().zip(hunks) {
                    count.hunks = Some(hunks);
                }
            } else {
                warnings.push(StructureWarning {
                    path: None,
                    reason: "hunk counts unavailable: patch and numstat paths disagree".into(),
                });
            }
        }
        Err(error) => warnings.push(StructureWarning {
            path: None,
            reason: format!("hunk counts unavailable: {error}"),
        }),
    }
    Ok(result)
}
