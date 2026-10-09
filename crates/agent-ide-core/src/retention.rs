//! Automatic retention of the shared per-user caches below `~/.agent-ide`: worktree leases, the
//! sweep policy, the dry-run report and the daemon's background task.
//!
//! `docs/cache-retention.md` is the policy truth source. In short: check caches (`checks/`) and
//! telemetry stores (`telemetry/`) are removed when their worktree is gone, when idle, or least
//! recently used first while over budget — only under an exclusive claim of the worktree lease that
//! every activation, check and test run holds shared, and never while a live `agent-ide` process
//! that does not take leases exists. Installed releases (`standalone/releases/`) are removed when
//! not current, not among the newest, old enough and not executed by any live process.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

mod rustc;
mod usage;
use usage::{Sharing, Usage};

/// Last-use age after which a worktree's rustc `incremental` tier is removed, keeping the rest.
pub const INCREMENTAL_IDLE: Duration = Duration::from_secs(2 * 86_400);

/// Allocated bytes the check caches may keep before least-recently-used eviction.
pub const CHECKS_BUDGET_BYTES: u64 = 15 << 30;
/// Allocated bytes the provider caches may keep before least-recently-used eviction.
pub const PROVIDERS_BUDGET_BYTES: u64 = 8 << 30;

/// Last-use age after which a provider cache namespace is removed.
pub const PROVIDERS_IDLE: Duration = Duration::from_secs(14 * 86_400);

/// Last-use age after which a check cache is removed.
pub const CHECKS_IDLE: Duration = Duration::from_secs(7 * 86_400);
/// Allocated bytes the telemetry stores may keep before least-recently-used eviction.
pub const TELEMETRY_BUDGET_BYTES: u64 = 1 << 30;
/// Last-use age after which a telemetry store is removed.
pub const TELEMETRY_IDLE: Duration = Duration::from_secs(30 * 86_400);
/// Newest installed release versions always kept, besides `current`.
pub const RELEASES_KEPT: usize = 3;
/// Minimum age of an installed release (its `COMPLETE` mtime) before it may be removed.
pub const RELEASE_AGE_FLOOR: Duration = Duration::from_secs(14 * 86_400);
/// Delay before a daemon's first sweep, so a starting session's activation leases first.
const FIRST_SWEEP_DELAY: Duration = Duration::from_secs(60);
/// Interval between a daemon's sweeps.
const SWEEP_INTERVAL: Duration = Duration::from_secs(3_600);
/// A scheduled sweep is skipped when any daemon completed one more recently than this.
const SWEEP_SPACING: Duration = Duration::from_secs(55 * 60);
/// Newest version whose processes do not take worktree leases.
const LEGACY_BOUNDARY: (u64, u64, u64) = (0, 9, 1);
/// Present in every executable built from this source, which takes worktree leases: a running
/// `agent-ide` whose executable file contains it is a proven participating build. Never edit it
/// without keeping older builds recognizable only by their release version.
#[used]
static LEASE_BUILD_PROOF: &[u8] = b"agent-ide/worktree-lease-protocol/proof-1";
/// Present only in executables whose Claude hook key hint publishers take the directory lock a
/// collection relies on (`hook_hints::PublishGuard`), which was introduced after
/// [`LEASE_BUILD_PROOF`]: a lease-only build is no proof that hints are published under the lock.
#[used]
static HINT_LOCK_BUILD_PROOF: &[u8] = b"agent-ide/hint-publish-lock/proof-1";
/// Largest executable scanned for a build proof; a larger one is unproven.
const PROOF_SCAN_LIMIT: u64 = 1 << 30;
/// Marker naming the worktree (checks) or launch directory (telemetry) of one cache directory.
pub const MARKER_FILE_NAME: &str = "worktree.path";
/// Directory of the stable, never-removed lease files below the state root.
const LOCKS_DIR: &str = "locks";
/// Lease key held shared by every [`Lease`]; claiming a store with no marker needs it exclusive.
const ANY_LEASE: &str = "any";
/// Global lock serializing sweeps; its mtime is the last completed sweep.
const SWEEP_LOCK: &str = "retention.lock";
/// Per-cache directory a claimed entry is renamed into before deletion.
const TRASH_DIR: &str = ".trash";
/// The telemetry writer's own lifetime lock inside each store directory.
const TELEMETRY_LOCK: &str = "state.sqlite.lock";

/// Returns the real per-user state root `<home>/.agent-ide` (see [`crate::userhome`]).
pub fn state_root() -> Option<PathBuf> {
    crate::userhome::user_home().map(|home| home.join(".agent-ide"))
}

/// Derives the lease key of a worktree: the same 16-hex key as its check cache directory name.
pub fn worktree_key(worktree: &Path) -> String {
    crate::checks::scheduler::hash16(worktree.to_string_lossy().as_bytes())
}

/// Shared advisory locks on one worktree's lease file and on the machine-wide `ANY_LEASE`;
/// dropping it releases both.
///
/// While any lease of a worktree is held, no sweeper on the machine can claim that worktree's
/// caches; while any lease at all is held, no sweeper can claim a store whose worktree is unknown.
/// Taking and releasing a lease both touch the worktree lock file's mtime, its last use.
#[derive(Debug)]
pub struct Lease {
    /// The worktree's lease file, locked shared.
    worktree: File,
    /// `ANY_LEASE`, locked shared.
    _any: File,
}

impl Lease {
    /// Takes the shared lease of `worktree` (canonicalized when possible) below `state_root`.
    ///
    /// Blocks only while a sweeper holds its exclusive claim, which lasts one rename. Returns
    /// `None` when a lock file cannot be opened or locked; callers must then not use the caches.
    pub fn acquire(state_root: &Path, worktree: &Path) -> Option<Self> {
        let canonical = fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
        Self::acquire_key(state_root, &worktree_key(&canonical))
    }

    /// Takes the shared lease named by an already derived worktree key.
    pub fn acquire_key(state_root: &Path, key: &str) -> Option<Self> {
        let locks = state_root.join(LOCKS_DIR);
        let any = open_lock(&locks.join(format!("{ANY_LEASE}.lock")))?;
        let worktree = open_lock(&locks.join(format!("{key}.lock")))?;
        if !flock(&any, libc::LOCK_SH) || !flock(&worktree, libc::LOCK_SH) {
            return None;
        }
        // Only a stamped (nonempty) lease file's mtime is a use; a sweeper's probe creates empty.
        if worktree
            .metadata()
            .is_ok_and(|metadata| metadata.len() == 0)
        {
            use std::io::Write as _;
            let _ = (&worktree).write_all(b"leased\n");
        }
        let _ = worktree.set_modified(SystemTime::now());
        Some(Self {
            worktree,
            _any: any,
        })
    }

    /// Takes the shared lease of `worktree` below the real per-user [`state_root`].
    pub fn for_worktree(worktree: &Path) -> Option<Self> {
        Self::acquire(&state_root()?, worktree)
    }
}

impl Drop for Lease {
    /// Records the release as the worktree's last use; closing the descriptor unlocks it.
    fn drop(&mut self) {
        let _ = self.worktree.set_modified(SystemTime::now());
    }
}

/// Lease kept until the work it protects has provably settled.
///
/// [`SettledLease::settled`] releases it normally. Dropping it unsettled (a cancelled future whose
/// child process may still be running) leaks the descriptor instead, so the lease lives until the
/// process exits.
#[derive(Debug)]
pub struct SettledLease(Option<Lease>);

impl SettledLease {
    /// Arms `lease` (if any) for the duration of one unit of work.
    pub fn new(lease: Option<Lease>) -> Self {
        Self(lease)
    }

    /// Releases the lease after the protected work completed.
    pub fn settled(mut self) {
        self.0.take();
    }
}

impl Drop for SettledLease {
    /// Leaks an unsettled lease for the rest of the process lifetime.
    fn drop(&mut self) {
        // ponytail: a cancelled run keeps its worktree lease until the daemon exits (daemons
        // idle-exit); hand it to the child's reaper if leaked leases ever matter.
        if let Some(lease) = self.0.take() {
            std::mem::forget(lease);
        }
    }
}

/// Whether `path` is a real (non-symlink) directory owned by the effective user that no group
/// or other user can write.
fn safe_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        // SAFETY: `geteuid` has no preconditions.
        metadata.file_type().is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0
    })
}

/// Opens (creating `0600`, a missing parent `0700`) one lock file without following symlinks.
///
/// The parent and grandparent must be [`safe_dir`]s and the opened file a regular file owned by
/// the effective user; anything else is refused.
fn open_lock(path: &Path) -> Option<File> {
    let parent = path.parent()?;
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent);
    }
    if !safe_dir(parent) || !parent.parent().is_some_and(safe_dir) {
        return None;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    // SAFETY: `geteuid` has no preconditions.
    (metadata.file_type().is_file() && metadata.uid() == unsafe { libc::geteuid() }).then_some(file)
}

/// Applies one `flock` operation, retrying an interrupted wait; `false` when not acquired.
fn flock(file: &File, operation: libc::c_int) -> bool {
    loop {
        // SAFETY: `flock` needs only a valid open descriptor and stores nothing.
        if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != ErrorKind::Interrupted {
            return false;
        }
    }
}

/// Opens and exclusively locks `path` without waiting; `None` when busy or unavailable.
fn try_exclusive(path: &Path) -> Option<File> {
    let file = open_lock(path)?;
    flock(&file, libc::LOCK_EX | libc::LOCK_NB).then_some(file)
}

/// Names `launch` in the marker of its telemetry store directory `store`, atomically, so a store
/// the launch uses is never left marker-less (which keeps it behind the machine-wide `any` lease).
///
/// `store` must be exactly `<state_root>/telemetry/<full BLAKE3 hex of launch>`, with all three
/// being private real directories. The shared lease of `launch` is taken first (a sweeper's claim,
/// which locks that lease or `any`, then cannot run) and the chain and the store's identity are
/// validated again under it, so a store a sweeper moved to trash or a directory replaced while
/// waiting is refused. The marker is written to a fresh `0600` sibling, synced and renamed over
/// any existing one, then read back. An existing marker that is a private regular file already
/// naming `launch` is left untouched; one with other content (a torn write) or a non-private mode
/// is replaced (so a historical `0644` marker is migrated to `0600`); a symlink, any other non-file or a marker writable by others is refused, never followed. A live telemetry
/// writer is unaffected (the marker is no part of its SQLite state, and its lock is not taken).
/// Publishing counts as a use of the store: it touches the directory's mtime, which delays its
/// idle expiry by up to the idle age (the launch is a use anyway).
pub fn adopt_marker(state_root: &Path, store: &Path, launch: &Path) -> std::io::Result<()> {
    use std::io::{Error, Read as _, Write as _};
    let refused = |message: &'static str| Error::new(ErrorKind::InvalidInput, message);
    let digest = blake3::hash(launch.as_os_str().as_bytes()).to_hex();
    let telemetry = state_root.join("telemetry");
    if !launch.is_absolute()
        || store.file_name().map(OsStr::as_bytes) != Some(digest.as_bytes())
        || store.parent() != Some(telemetry.as_path())
    {
        return Err(refused("not the telemetry store of this launch directory"));
    }
    // The three directories are private real directories; returns the store's identity.
    let chain = || -> std::io::Result<(u64, u64)> {
        let private =
            |dir: &Path| safe_dir(dir) && fs::metadata(dir).is_ok_and(|m| m.mode() & 0o077 == 0);
        if ![state_root, telemetry.as_path(), store]
            .into_iter()
            .all(private)
        {
            return Err(refused("telemetry state is not private"));
        }
        let metadata = fs::symlink_metadata(store)?;
        Ok((metadata.dev(), metadata.ino()))
    };
    let before = chain()?;
    let _lease = Lease::acquire(state_root, launch)
        .ok_or_else(|| Error::new(ErrorKind::PermissionDenied, "worktree lease unavailable"))?;
    if chain()? != before {
        return Err(refused(
            "telemetry store changed while waiting for its lease",
        ));
    }
    let marker = store.join(MARKER_FILE_NAME);
    let wanted = launch.as_os_str().as_bytes();
    // `Some(true)` when the marker is a private regular file of this user naming exactly
    // `launch`, `Some(false)` when it is one with other content or a wider mode, `None` when
    // missing; anything else is an error.
    let current = || -> std::io::Result<Option<bool>> {
        let metadata = match fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // SAFETY: `geteuid` has no preconditions.
        if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "marker is not a file of this user",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&marker)?;
        let opened = file.metadata()?;
        if (opened.dev(), opened.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "marker was replaced while read",
            ));
        }
        // Version 0.10.6 wrote markers with the process umask (`0644`): safe inside the private
        // store, republished as `0600`. A marker others can write is refused.
        if opened.mode() & 0o022 != 0 {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "marker is writable by others",
            ));
        }
        let mut bytes = Vec::new();
        file.take(wanted.len() as u64 + 1).read_to_end(&mut bytes)?;
        Ok(Some(bytes == wanted && opened.mode() & 0o077 == 0))
    };
    if current()? == Some(true) {
        return Ok(());
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = store.join(format!(
        "{MARKER_FILE_NAME}.{}-{nanos}.tmp",
        std::process::id()
    ));
    let published = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(wanted)?;
            file.sync_all()
        })
        .and_then(|()| {
            if chain()? != before {
                return Err(refused(
                    "telemetry store changed before its marker was published",
                ));
            }
            fs::rename(&temporary, &marker)
        });
    if let Err(error) = published {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    if current()? == Some(true) {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::InvalidData,
            "marker changed while it was published",
        ))
    }
}

/// One cache family the sweep manages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `checks/<repo16>/<worktree16>`: one worktree's project-check build cache.
    Checks,
    /// `telemetry/<digest>`: one launch directory's telemetry store.
    Telemetry,
    /// `providers/<namespace>`: one language server's native cache namespace.
    Providers,
    /// `standalone/releases/<X.Y.Z>`: one installed release.
    Release,
}

impl Kind {
    /// Renders the closed lowercase tag used in reports and log details.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Checks => "checks",
            Self::Telemetry => "telemetry",
            Self::Providers => "providers",
            Self::Release => "release",
        }
    }
}

/// Why the policy selected an entry for removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The marker's worktree path no longer exists.
    Gone,
    /// Last use is older than the idle age.
    Idle,
    /// The cache family is over its budget and this is its least recently used entry.
    Budget,
    /// A release that is neither current, among the newest, recent nor executing.
    Superseded,
    /// Trash left by an interrupted sweep.
    Leftover,
    /// An idle worktree's rustc `incremental` tier, removed before the worktree itself.
    Incremental,
    /// Finalized rustc sessions older than the newest of their crate.
    Sessions,
}

impl Reason {
    /// Renders the closed lowercase tag used in reports and log details.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gone => "gone",
            Self::Idle => "idle",
            Self::Budget => "budget",
            Self::Superseded => "superseded",
            Self::Leftover => "leftover",
            Self::Incremental => "incremental",
            Self::Sessions => "sessions",
        }
    }
}

/// What happened to (or, in a dry run, would happen to) one selected entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// Removed (dry run: claimable now, so a sweep would remove it).
    Removed,
    /// Kept: a lease or writer lock is held, a lock could not be taken, or a rename failed.
    InUse,
    /// Kept: its tree could not be fully read, so its size and last use are unknown.
    Unreadable,
    /// Kept: a legacy or unknown `agent-ide` process is alive, so no check or telemetry
    /// entry may be removed.
    Paused,
}

/// One entry the policy selected, with its outcome.
#[derive(Clone, Debug)]
pub struct Verdict {
    /// Cache family.
    pub kind: Kind,
    /// Directory selected.
    pub path: PathBuf,
    /// Worktree or launch directory named by its marker, when it has one.
    pub worktree: Option<PathBuf>,
    /// Allocated bytes: actually freed when removed by a sweep, else the measured size.
    pub bytes: u64,
    /// Why it was selected.
    pub reason: Reason,
    /// Outcome.
    pub fate: Fate,
}

/// One scanned cache directory of the checks or telemetry family.
#[derive(Clone, Debug)]
struct Entry {
    /// Cache family.
    kind: Kind,
    /// Directory that is claimed and removed as a whole.
    path: PathBuf,
    /// Worktree or launch directory named by the marker, if one was readable.
    worktree: Option<PathBuf>,
    /// Lease keys whose exclusive locks claim the entry.
    lease_keys: Vec<String>,
    /// Allocated bytes below `path` counting each shared group once, as if no other entry existed.
    bytes: u64,
    /// What the tree shares with the rest of its family.
    usage: Usage,
    /// Newest mtime of the tree and its lease file.
    last_used: SystemTime,
    /// The marker was read and its path is definitely missing.
    gone: bool,
    /// Every directory of the tree was readable.
    complete: bool,
}

/// Totals of one cache family as found by the scan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    /// Entries found.
    pub entries: usize,
    /// Charged bytes found: allocation with every hard-linked inode and perfect-clone stream
    /// counted once. This is what the budget bounds.
    pub bytes: u64,
    /// Sum of file lengths found, every link and clone counted.
    pub logical: u64,
    /// Estimate of what the volume gets back at once if all entries went (APFS private bytes);
    /// shown for reclaim only, never charged.
    pub private: u64,
    /// Budget in bytes, `0` for releases (no budget).
    pub budget: u64,
    /// Why the family was not (fully) evaluated: unsafe root, incomplete listing (budget not
    /// applied), install in progress or unknown processes.
    pub note: Option<&'static str>,
}

/// Result of one sweep or dry run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Live `agent-ide` processes that do not take leases, or `None` when the process snapshot
    /// failed; either non-empty or `None` pauses all check and telemetry removal.
    pub legacy: Option<Vec<Process>>,
    /// Live development or test builds proven to take leases; they never pause removal.
    pub builds: Vec<Process>,
    /// Check cache totals before the sweep.
    pub checks: Totals,
    /// Telemetry store totals before the sweep.
    pub telemetry: Totals,
    /// Provider cache namespace totals before the sweep.
    pub providers: Totals,
    /// Installed release totals before the sweep.
    pub releases: Totals,
    /// Every entry the policy selected, in decision order.
    pub verdicts: Vec<Verdict>,
}

impl Report {
    /// Whether check and telemetry removal was paused by legacy or unknown processes.
    pub fn paused(&self) -> bool {
        self.legacy.as_ref().is_none_or(|legacy| !legacy.is_empty())
    }

    /// Renders the operator-facing report; `applied` selects "removed" over "would remove".
    pub fn render(&self, applied: bool) -> String {
        let mut text = String::new();
        for (name, totals) in [
            ("checks", self.checks),
            ("telemetry", self.telemetry),
            ("providers", self.providers),
            ("releases", self.releases),
        ] {
            let _ = write!(
                text,
                "{name}: {} entries, {}",
                totals.entries,
                human(totals.bytes)
            );
            if totals.budget > 0 {
                let _ = write!(
                    text,
                    " charged ({} logical, {} private reclaim estimate)",
                    human(totals.logical),
                    human(totals.private)
                );
                let _ = write!(text, " (budget {})", human(totals.budget));
            }
            if let Some(note) = totals.note {
                let _ = write!(text, " [{note}]");
            }
            text.push('\n');
        }
        match &self.legacy {
            None => text.push_str(
                "eviction of checks/telemetry paused: the process list could not be read\n",
            ),
            Some(legacy) if !legacy.is_empty() => {
                text.push_str(
                    "eviction of checks/telemetry paused until these agent-ide processes exit \
                     (releases up to 0.9.1, or executables not proven to take leases):\n",
                );
                for (pid, exe) in legacy {
                    let exe = exe.as_deref().map_or(
                        "(executable deleted or unreadable)".into(),
                        Path::to_string_lossy,
                    );
                    let _ = writeln!(text, "  pid {pid} {exe}");
                }
            }
            Some(_) => {}
        }
        if !self.builds.is_empty() {
            text.push_str(
                "these development or test builds take leases and do not pause eviction:\n",
            );
            for (pid, exe) in &self.builds {
                let exe = exe.as_deref().map_or("?".into(), Path::to_string_lossy);
                let _ = writeln!(text, "  pid {pid} {exe}");
            }
        }
        if self.verdicts.is_empty() {
            text.push_str("nothing to remove\n");
        }
        for verdict in &self.verdicts {
            let fate = match (verdict.fate, applied) {
                (Fate::Removed, true) => "removed",
                (Fate::Removed, false) => "would remove",
                (Fate::InUse, _) => "kept, in use",
                (Fate::Unreadable, _) => "kept, unreadable",
                (Fate::Paused, _) => "kept, paused",
            };
            let _ = write!(
                text,
                "{fate}: {} {} {} {}",
                verdict.kind.as_str(),
                verdict.reason.as_str(),
                human(verdict.bytes),
                verdict.path.display()
            );
            if let Some(worktree) = &verdict.worktree {
                let _ = write!(text, " ({})", worktree.display());
            }
            text.push('\n');
        }
        text
    }

    /// Records every removal (and a legacy pause) in the process error log.
    pub fn record(&self) {
        use crate::errorlog::{Fields, Method, Outcome, record};
        if self.paused() {
            let detail = self
                .legacy
                .as_ref()
                .map_or("legacy=unknown".to_owned(), |legacy| {
                    format!("legacy={}", legacy.len())
                });
            record(
                Method::Retention,
                Outcome::Skipped,
                Fields {
                    detail: Some(&detail),
                    ..Default::default()
                },
            );
        }
        for verdict in self.verdicts.iter().filter(|v| v.fate == Fate::Removed) {
            let detail = format!(
                "{} {} freed={}",
                verdict.kind.as_str(),
                verdict.reason.as_str(),
                verdict.bytes
            );
            record(
                Method::Retention,
                Outcome::Completed,
                Fields {
                    worktree: Some(verdict.worktree.as_deref().unwrap_or(&verdict.path)),
                    detail: Some(&detail),
                    ..Default::default()
                },
            );
        }
    }
}

/// Renders a byte count with a binary unit.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One live `agent-ide` process of the effective user: pid and executable path, `None` when the
/// path is unreadable (for example its executable was deleted).
pub type Process = (i32, Option<PathBuf>);

/// Lists every live process of the effective user whose name starts with `agent-ide`, with its
/// executable path; `None` when the list is unavailable, truncated, or a process could not be
/// inspected for a reason other than having exited.
pub fn process_snapshot() -> Option<Vec<Process>> {
    let mut capacity = 1024usize;
    let pids = loop {
        let mut pids = vec![0 as libc::c_int; capacity];
        let bytes = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::c_int>()).ok()?;
        // SAFETY: `pids` is a writable buffer of exactly `bytes` bytes.
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if filled <= 0 {
            return None;
        }
        if (filled as usize) < pids.len() {
            pids.truncate(filled as usize);
            break pids;
        }
        if capacity >= 1 << 20 {
            return None;
        }
        capacity *= 4;
    };
    // SAFETY: `geteuid` has no preconditions.
    let euid = unsafe { libc::geteuid() };
    let gone = || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    let mut processes = Vec::new();
    let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    for pid in pids {
        // SAFETY: `proc_bsdshortinfo` is plain data that `proc_pidinfo` fills up to its size.
        let mut info: libc::proc_bsdshortinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        // SAFETY: `info` is writable for `size` bytes.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&raw mut info).cast(),
                size,
            )
        };
        if filled != size {
            if gone() || pid == 0 {
                continue;
            }
            return None;
        }
        let name = info.pbsi_comm.map(|byte| byte as u8);
        if info.pbsi_uid != euid || !name.starts_with(b"agent-ide") {
            continue;
        }
        // SAFETY: `path` is writable for its full length, which is passed as its size.
        let length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if length > 0 {
            let exe = PathBuf::from(OsStr::from_bytes(&path[..length as usize]));
            processes.push((pid, Some(exe)));
        } else if !gone() {
            processes.push((pid, None));
        }
    }
    Some(processes)
}

/// Device and inode of a file, following symlinks.
fn file_id(path: &Path) -> Option<(u64, u64)> {
    fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Parses an exact `X.Y.Z` version.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// An executable's identity (device, inode, length, mtime) and which proof was scanned for
/// (`true`: the hint lock, `false`: the lease protocol).
type ProofKey = (u64, u64, u64, SystemTime, bool);

/// What a sweeping process knows about itself to classify other `agent-ide` processes.
#[derive(Debug)]
struct Identity {
    /// Device and inode of the sweeping executable.
    own: Option<(u64, u64)>,
    /// Canonical `standalone/releases` directory.
    releases: PathBuf,
    /// Releases strictly newer than this take leases.
    floor: (u64, u64, u64),
    /// When a process started, so a rebuilt file is not mistaken for the code it runs.
    started: fn(i32) -> Option<SystemTime>,
    /// Scan verdicts by executable identity (device, inode, length, mtime): one scan per build.
    proofs: RefCell<BTreeMap<ProofKey, bool>>,
}

/// How the live `agent-ide` processes divide for one sweep.
#[derive(Clone, Debug, Default)]
pub struct Classified {
    /// Processes not known to take leases; any of them pauses eviction.
    pub legacy: Vec<Process>,
    /// Participating development or test builds outside `standalone/releases`, proven by their
    /// executable; they never pause eviction and are listed so the report can say so.
    pub builds: Vec<Process>,
}

impl Identity {
    /// Describes the current process under `state_root`.
    fn current(state_root: &Path) -> Self {
        let releases = state_root.join("standalone").join("releases");
        Self {
            own: std::env::current_exe().ok().and_then(|exe| file_id(&exe)),
            releases: fs::canonicalize(&releases).unwrap_or(releases),
            // Every release after the boundary takes leases, older than this build or not: a
            // session started before an upgrade must not pause the upgraded sweeper.
            floor: LEGACY_BOUNDARY,
            started: process_start,
            proofs: RefCell::default(),
        }
    }

    /// Splits the live `agent-ide` processes into those that pause eviction and the proven
    /// development builds that do not.
    ///
    /// A process participates when its executable is this process's own file, or
    /// `releases/X.Y.Z/agent-ide` with `X.Y.Z` strictly newer than [`Identity::floor`], or when it
    /// is a build proven to take leases ([`Identity::proven_build`]). One whose executable path is
    /// unreadable never does.
    fn classify(&self, processes: &[Process]) -> Classified {
        let mut classified = Classified::default();
        for process in processes.iter().filter(|(_, exe)| {
            exe.as_ref().is_none_or(|exe| {
                exe.file_name()
                    .is_some_and(|name| name.as_bytes().starts_with(b"agent-ide"))
            })
        }) {
            match &process.1 {
                Some(exe) if self.participates(exe) => {}
                Some(exe) if self.proven_build(process.0, exe) => {
                    classified.builds.push(process.clone());
                }
                _ => classified.legacy.push(process.clone()),
            }
        }
        classified
    }

    /// Returns the live `agent-ide` processes that may still publish hook key hints without the
    /// directory lock a collection relies on: every one that is not this very executable or a
    /// build proven to lock ([`HINT_LOCK_BUILD_PROOF`], embedded only together with that lock; a
    /// lease-only build proves nothing here). An installed release is proven by its contents like
    /// any other file, never by its version number, so every release before the lock keeps the
    /// collection paused.
    fn hint_unsafe(&self, processes: &[Process]) -> Vec<Process> {
        processes
            .iter()
            .filter(|(pid, exe)| {
                !exe.as_deref().is_some_and(|exe| {
                    (self.own.is_some() && file_id(exe) == self.own)
                        || self.build_contains(*pid, exe, HINT_LOCK_BUILD_PROOF)
                })
            })
            .cloned()
            .collect()
    }

    /// Returns the live `agent-ide` processes that are not known to take leases.
    fn legacy(&self, processes: &[Process]) -> Vec<Process> {
        self.classify(processes).legacy
    }

    /// Whether one `agent-ide` executable is known to take leases.
    fn participates(&self, exe: &Path) -> bool {
        if self.own.is_some() && file_id(exe) == self.own {
            return true;
        }
        exe.file_name() == Some(OsStr::new("agent-ide"))
            && exe
                .parent()
                .filter(|dir| dir.parent() == Some(self.releases.as_path()))
                .and_then(|dir| parse_version(dir.file_name()?.to_str()?))
                .is_some_and(|version| version > self.floor)
    }

    /// Whether process `pid` runs a build of this source: its executable is a regular file not
    /// modified since the process started (so it is the code that runs, not a later rebuild) and
    /// contains [`LEASE_BUILD_PROOF`]. A path, name or location proves nothing.
    ///
    /// ponytail: an executable replaced by a file with an older mtime than the process start is
    /// not noticed; the process list offers no inode to compare.
    fn proven_build(&self, pid: i32, exe: &Path) -> bool {
        self.build_contains(pid, exe, LEASE_BUILD_PROOF)
    }

    /// [`Self::proven_build`] for the build proof `needle`, which must be [`LEASE_BUILD_PROOF`] or
    /// [`HINT_LOCK_BUILD_PROOF`]: the proof cache tells exactly those two kinds apart.
    fn build_contains(&self, pid: i32, exe: &Path, needle: &'static [u8]) -> bool {
        let Ok(file) = File::open(exe) else {
            return false;
        };
        let Ok(metadata) = file.metadata() else {
            return false;
        };
        let (Some(started), Ok(modified)) = ((self.started)(pid), metadata.modified()) else {
            return false;
        };
        if !metadata.is_file() || metadata.len() > PROOF_SCAN_LIMIT || modified > started {
            return false;
        }
        // The cache tells the two proofs apart: a lease-only build never answers for the lock.
        let lock = needle == HINT_LOCK_BUILD_PROOF;
        let key = (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            modified,
            lock,
        );
        if let Some(proven) = self.proofs.borrow().get(&key) {
            return *proven;
        }
        let proven = contains_proof(&file, needle);
        // A rebuild during the scan changed the file or replaced the path: no verdict at all.
        let unchanged = |now: fs::Metadata| {
            (now.dev(), now.ino(), now.len(), now.modified().ok())
                == (key.0, key.1, key.2, Some(key.3))
        };
        if !file.metadata().is_ok_and(unchanged) || !fs::metadata(exe).is_ok_and(unchanged) {
            return false;
        }
        self.proofs.borrow_mut().insert(key, proven);
        proven
    }
}

/// Lists the live `agent-ide` processes that may publish Claude hook key hints without the
/// directory lock, or `None` when the process list is unavailable; hint collection
/// ([`crate::hook_hints::collect`]) must not apply while either is non-empty or `None`.
pub fn hint_publishers_unsafe(snapshot: &dyn Fn() -> Option<Vec<Process>>) -> Option<Vec<Process>> {
    hint_publishers_unsafe_as(&Identity::current(&state_root()?), snapshot)
}

/// Returns the question hook key hint collection asks right before every removal: `true` only while
/// a fresh process snapshot shows every live `agent-ide` to be this executable or a proven build.
/// One proof cache serves all questions of a pass.
pub fn hint_publishers_safe() -> impl Fn() -> bool {
    let identity = state_root().map(|root| Identity::current(&root));
    move || {
        identity.as_ref().is_some_and(|identity| {
            hint_publishers_unsafe_as(identity, &process_snapshot)
                .is_some_and(|unsafe_publishers| unsafe_publishers.is_empty())
        })
    }
}

/// [`hint_publishers_unsafe`] classifying processes as `identity` does.
fn hint_publishers_unsafe_as(
    identity: &Identity,
    snapshot: &dyn Fn() -> Option<Vec<Process>>,
) -> Option<Vec<Process>> {
    Some(identity.hint_unsafe(&snapshot()?))
}

/// When process `pid` started, or `None` when it cannot be inspected.
fn process_start(pid: i32) -> Option<SystemTime> {
    // SAFETY: `proc_bsdinfo` is plain data that `proc_pidinfo` fills up to its size.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: `info` is writable for `size` bytes.
    let filled =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (filled == size).then(|| {
        SystemTime::UNIX_EPOCH
            + Duration::new(info.pbi_start_tvsec, (info.pbi_start_tvusec as u32) * 1_000)
    })
}

/// Whether the stream contains the build proof `needle`; any read error means no.
fn contains_proof(mut file: &File, needle: &[u8]) -> bool {
    use std::io::Read as _;
    let mut buffer = vec![0u8; (1 << 20) + needle.len()];
    let mut kept = 0;
    loop {
        let Ok(read) = file.read(&mut buffer[kept..]) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        let end = kept + read;
        let haystack = &buffer[..end];
        if haystack
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
        // Keep a tail so a proof split across two reads is still found.
        kept = (needle.len() - 1).min(end);
        buffer.copy_within(end - kept..end, 0);
    }
}

/// Whether `name` is exactly `length` lowercase hex characters.
fn is_hex(name: &OsStr, length: usize) -> bool {
    name.len() == length
        && name
            .as_bytes()
            .iter()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Lists the real subdirectories of `dir` whose names satisfy `accept`; the flag is `false` when
/// the listing failed or skipped an unreadable entry.
fn subdirectories(dir: &Path, accept: impl Fn(&OsStr) -> bool) -> (Vec<PathBuf>, bool) {
    let Ok(entries) = fs::read_dir(dir) else {
        return (Vec::new(), false);
    };
    let mut complete = true;
    let mut found = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        if !accept(&entry.file_name()) {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => found.push(entry.path()),
            Ok(_) => {}
            Err(_) => complete = false,
        }
    }
    (found, complete)
}

/// Visits every entry of a tree without following symlinks, reporting the newest mtime; the flag
/// is `false` when any directory or entry could not be read.
fn walk(
    root: &Path,
    skip: &[PathBuf],
    mut visit: impl FnMut(&Path, &fs::Metadata),
) -> (SystemTime, bool) {
    let mut newest = SystemTime::UNIX_EPOCH;
    let mut complete = true;
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        if skip.contains(&path) {
            continue;
        }
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            complete = false;
            continue;
        };
        visit(&path, &metadata);
        if let Ok(modified) = metadata.modified() {
            newest = newest.max(modified);
        }
        if metadata.file_type().is_dir() {
            match fs::read_dir(&path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => stack.push(entry.path()),
                            Err(_) => complete = false,
                        }
                    }
                }
                Err(_) => complete = false,
            }
        }
    }
    (newest, complete)
}

/// Sums allocated bytes and finds the newest mtime of a tree without following symlinks;
/// the flag is `false` when any directory or entry could not be read.
fn measure(root: &Path) -> (u64, SystemTime, bool) {
    let mut bytes = 0u64;
    let (newest, complete) = walk(root, &[], |_, metadata| {
        bytes = bytes.saturating_add(metadata.blocks().saturating_mul(512));
    });
    (bytes, newest, complete)
}

/// [`measure`] for a cache entry: what the tree shares with the rest of its family, the newest
/// mtime, and whether every part was readable.
fn measure_usage(root: &Path) -> (Usage, SystemTime, bool) {
    measure_usage_excluding(root, &[])
}

/// [`measure_usage`] leaving out the subtrees at `skip`.
fn measure_usage_excluding(root: &Path, skip: &[PathBuf]) -> (Usage, SystemTime, bool) {
    let mut usage = Usage::default();
    let (newest, complete) = walk(root, skip, |path, metadata| {
        if metadata.file_type().is_file() {
            usage.add_file(path, metadata);
        } else {
            usage.add_other(metadata);
        }
    });
    (usage, newest, complete)
}

/// Deletes one claimed tree and returns the allocated bytes actually freed: its measured size
/// minus a completely measured remainder, or `0` when the remainder cannot be measured.
fn delete(path: &Path, bytes: u64) -> u64 {
    if fs::remove_dir_all(path).is_ok() {
        return bytes;
    }
    match measure(path) {
        (remaining, _, true) => bytes.saturating_sub(remaining),
        _ => 0,
    }
}

/// What one cache directory's marker says.
struct Marker {
    /// The absolute path it names, when readable.
    path: Option<PathBuf>,
    /// The named path definitely does not exist (its lookup failed with `NotFound`).
    gone: bool,
    /// The marker is absent (a legacy directory) or was read and its path checked; `false` for
    /// an unreadable or invalid marker or a path lookup failing otherwise, which keeps the entry.
    known: bool,
}

/// Reads one cache directory's marker and decides whether its path is definitely gone.
fn read_marker(dir: &Path) -> Marker {
    let unknown = Marker {
        path: None,
        gone: false,
        known: false,
    };
    let bytes = match fs::read(dir.join(MARKER_FILE_NAME)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Marker {
                known: true,
                ..unknown
            };
        }
        Err(_) => return unknown,
    };
    let path = PathBuf::from(OsStr::from_bytes(&bytes));
    if !path.is_absolute() {
        return unknown;
    }
    match fs::metadata(&path) {
        Ok(_) => Marker {
            path: Some(path),
            gone: false,
            known: true,
        },
        Err(error) if error.kind() == ErrorKind::NotFound => Marker {
            path: Some(path),
            gone: true,
            known: true,
        },
        Err(_) => Marker {
            path: Some(path),
            ..unknown
        },
    }
}

/// Builds one entry: size, last use (tree and lease file), marker.
fn entry(
    kind: Kind,
    path: PathBuf,
    lease_keys: Vec<String>,
    locks: &Path,
    marker: Marker,
) -> Entry {
    let (usage, newest, complete) = measure_usage(&path);
    let leased = lease_keys
        .iter()
        .filter_map(|key| fs::metadata(locks.join(format!("{key}.lock"))).ok())
        .filter(|metadata| metadata.len() > 0)
        .filter_map(|metadata| metadata.modified().ok())
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Entry {
        kind,
        path,
        worktree: marker.path,
        lease_keys,
        bytes: usage.standalone(),
        usage,
        last_used: newest.max(leased),
        gone: marker.gone,
        complete: complete && marker.known,
    }
}

/// Scans `checks/<repo16>/<worktree16>` entries; the lease key is the worktree directory name.
/// The flag is `false` when any listing was incomplete.
fn scan_checks(root: &Path, locks: &Path) -> (Vec<Entry>, bool) {
    let (repositories, mut complete) = subdirectories(root, |name| is_hex(name, 16));
    let mut entries = Vec::new();
    for repository in repositories {
        let (worktrees, listed) = subdirectories(&repository, |name| is_hex(name, 16));
        complete &= listed;
        for worktree in worktrees {
            let key = worktree
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .into_iter()
                .collect();
            let marker = read_marker(&worktree);
            entries.push(entry(Kind::Checks, worktree, key, locks, marker));
        }
    }
    (entries, complete)
}

/// Scans `telemetry/<digest>` entries. A store is keyed by its launch directory, which may lie
/// below the worktree an activation leases, so its lease keys are those of the marker path and
/// every ancestor; a claim locks them all (creating missing lease files), so an activation that
/// starts after the scan is still excluded.
fn scan_telemetry(root: &Path, locks: &Path) -> (Vec<Entry>, bool) {
    let (stores, complete) = subdirectories(root, |name| is_hex(name, 64));
    let entries = stores
        .into_iter()
        .map(|store| {
            let marker = read_marker(&store);
            // A store without a marker could belong to any worktree.
            let keys = match &marker.path {
                Some(launch) => launch.ancestors().map(worktree_key).collect(),
                None => vec![ANY_LEASE.to_owned()],
            };
            entry(Kind::Telemetry, store, keys, locks, marker)
        })
        .collect();
    (entries, complete)
}

/// Scans `providers/<namespace>` entries (64-hex keys). A namespace names its worktree in its
/// marker and is claimed through that worktree's lease; one without a marker (the shared native
/// namespace, or one whose marker is not yet published) could be in use by any activation, so its
/// claim needs the machine-wide `any` lease exclusively.
fn scan_providers(root: &Path, locks: &Path) -> (Vec<Entry>, bool) {
    let (namespaces, complete) = subdirectories(root, |name| is_hex(name, 64));
    let entries = namespaces
        .into_iter()
        .map(|namespace| {
            let marker = read_marker(&namespace);
            let keys = match &marker.path {
                Some(worktree) => vec![worktree_key(worktree)],
                None => vec![ANY_LEASE.to_owned()],
            };
            entry(Kind::Providers, namespace, keys, locks, marker)
        })
        .collect();
    (entries, complete)
}

/// Returns `<state_root>/providers`, creating it `0700` when missing; `None` when the state root
/// or the directory is not a private real directory of this user (the caller then keeps its
/// namespaces runtime-local).
pub fn providers_root(state_root: &Path) -> Option<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    builder.create(state_root).ok()?;
    let root = state_root.join("providers");
    let _ = builder.create(&root);
    (safe_dir(state_root) && safe_dir(&root)).then_some(root)
}

/// Names the worktree a provider cache namespace belongs to, so retention can tell when the
/// worktree is gone and claim the namespace through its lease. The marker is a fresh `0600` file
/// renamed into place; one already naming the worktree is left alone.
pub fn publish_namespace_marker(namespace: &Path, worktree: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let canonical = fs::canonicalize(worktree).unwrap_or_else(|_| worktree.to_path_buf());
    let marker = namespace.join(MARKER_FILE_NAME);
    if fs::read(&marker).is_ok_and(|bytes| bytes == canonical.as_os_str().as_bytes()) {
        return Ok(());
    }
    let temporary = namespace.join(format!("{MARKER_FILE_NAME}.{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(canonical.as_os_str().as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, &marker)
}

/// Which part of a check cache a partial claim removes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Every rustc `incremental` directory (the whole tier of an idle worktree).
    Incremental,
    /// Finalized rustc sessions older than the newest of their crate.
    Sessions,
}

/// The outcome of a partial claim: what happened and the entry's usage afterwards (`None` when
/// nothing changed). What it freed is the drop of the family charge, decided by [`select`].
struct Trimmed {
    /// Outcome.
    fate: Fate,
    /// The entry's measurement after the removal (dry run: with the removable parts left out).
    usage: Option<Usage>,
}

/// Claims one entry for removal: `(Fate, charged bytes freed)`; the second argument is the charge
/// that disappears with the entry (see [`Sharing::freed`]).
type Claim<'a> = dyn FnMut(&Entry, u64) -> (Fate, u64) + 'a;

/// Removes part of one entry, or `None` when it has nothing to remove at that tier.
type Trim<'a> = dyn FnMut(&Entry, Tier) -> Option<Trimmed> + 'a;

/// Applies the gone/idle/budget policy to one family; `claim` removes (or probes) one entry and
/// returns its fate and the bytes it freed. Gone and idle entries are decided first. The rest of
/// a check cache then loses its parts before any whole entry does (`trim`, when given): older
/// finalized rustc sessions, and the whole `incremental` tier of a worktree idle for
/// [`INCREMENTAL_IDLE`]. Last, the least recently used of what is left go while the remaining
/// charge exceeds `budget` (`None` skips the budget rule). The charge is recomputed from the
/// surviving entries after every removal, so a hard link or clone still held elsewhere frees
/// nothing. Unreadable entries are never claimed and stay counted.
fn select(
    mut entries: Vec<Entry>,
    idle: Duration,
    budget: Option<u64>,
    now: SystemTime,
    claim: &mut Claim<'_>,
    mut trim: Option<&mut Trim<'_>>,
) -> Vec<Verdict> {
    entries.sort_by_key(|entry| entry.last_used);
    let mut sharing = Sharing::new(entries.iter().map(|entry| &entry.usage));
    let mut verdicts = Vec::new();
    let mut decide =
        |entry: &Entry, reason: Reason, sharing: &mut Sharing, verdicts: &mut Vec<Verdict>| {
            let (fate, bytes) = if entry.complete {
                claim(entry, sharing.freed(&entry.usage))
            } else {
                (Fate::Unreadable, entry.bytes)
            };
            // A paused entry would be removed but for legacy processes; counting it keeps the
            // report to what the budget actually selects.
            if matches!(fate, Fate::Removed | Fate::Paused) {
                sharing.release(&entry.usage);
            }
            verdicts.push(Verdict {
                kind: entry.kind,
                path: entry.path.clone(),
                worktree: entry.worktree.clone(),
                bytes,
                reason,
                fate,
            });
        };
    let mut rest = Vec::new();
    for entry in entries {
        let idle_for = now.duration_since(entry.last_used).unwrap_or_default();
        if entry.gone {
            decide(&entry, Reason::Gone, &mut sharing, &mut verdicts);
        } else if idle_for > idle {
            decide(&entry, Reason::Idle, &mut sharing, &mut verdicts);
        } else {
            rest.push(entry);
        }
    }
    if let Some(trim) = trim.as_mut() {
        for entry in rest.iter_mut().filter(|entry| entry.complete) {
            let idle_for = now.duration_since(entry.last_used).unwrap_or_default();
            let (tier, reason) = if idle_for > INCREMENTAL_IDLE {
                (Tier::Incremental, Reason::Incremental)
            } else {
                (Tier::Sessions, Reason::Sessions)
            };
            let Some(trimmed) = trim(entry, tier) else {
                continue;
            };
            // What the trim frees is what the family charge drops by: bytes another entry still
            // holds through a hard link or clone stay charged and are not freed.
            let before = sharing.charged();
            if let (Fate::Removed | Fate::Paused, Some(usage)) = (trimmed.fate, trimmed.usage) {
                sharing.release(&entry.usage);
                sharing.add(&usage);
                entry.bytes = usage.standalone();
                entry.usage = usage;
            }
            verdicts.push(Verdict {
                kind: entry.kind,
                path: entry.path.clone(),
                worktree: entry.worktree.clone(),
                bytes: before.saturating_sub(sharing.charged()),
                reason,
                fate: trimmed.fate,
            });
        }
    }
    if let Some(budget) = budget {
        for entry in rest {
            if sharing.charged() <= budget {
                break;
            }
            decide(&entry, Reason::Budget, &mut sharing, &mut verdicts);
        }
    }
    verdicts
}

/// Takes the exclusive non-blocking locks that claim `entry` (its leases and, for a telemetry
/// store, its writer lock), refusing a lease used since `scan_started`. `None` means in use.
fn lock_entry(entry: &Entry, locks: &Path, scan_started: SystemTime) -> Option<Vec<File>> {
    let mut held = Vec::new();
    for key in &entry.lease_keys {
        let lease = locks.join(format!("{key}.lock"));
        let file = try_exclusive(&lease)?;
        // Only a lease taken since the scan stamps the file and moves its mtime past it; a file
        // this or another probe just created is empty.
        let used_since_scan = file.metadata().map_or(true, |metadata| {
            metadata.len() > 0
                && metadata
                    .modified()
                    .map_or(true, |modified| modified > scan_started)
        });
        if used_since_scan {
            return None;
        }
        held.push(file);
    }
    if entry.kind == Kind::Telemetry {
        held.push(try_exclusive(&entry.path.join(TELEMETRY_LOCK))?);
    }
    Some(held)
}

/// Claims one checks/telemetry entry: exclusive non-blocking locks on its lease (and telemetry
/// writer lock), a lease untouched since the scan began, no legacy process, then a rename into the
/// family's trash; the locks are released before the trash is deleted. A dry run only probes the
/// locks. `charge` is the charged bytes that disappear with the entry. Returns the fate and the
/// bytes freed (`charge` in a dry run).
fn claim_entry(
    entry: &Entry,
    charge: u64,
    locks: &Path,
    scan_started: SystemTime,
    apply: bool,
    legacy_free: &dyn Fn() -> bool,
) -> (Fate, u64) {
    let kept = (Fate::InUse, charge);
    let Some(held) = lock_entry(entry, locks, scan_started) else {
        return kept;
    };
    if !apply {
        return (Fate::Removed, charge);
    }
    if !legacy_free() {
        return (Fate::Paused, charge);
    }
    let Some(trash) = trash_path(entry) else {
        return kept;
    };
    if fs::rename(&entry.path, &trash).is_err() {
        return kept;
    }
    drop(held);
    let freed = delete(&trash, charge);
    if entry.kind == Kind::Checks
        && let Some(repository) = entry.path.parent()
    {
        // Only succeeds when empty; a concurrent `create_dir_all` recreates it.
        let _ = fs::remove_dir(repository);
    }
    (Fate::Removed, freed)
}

/// Removes one [`Tier`] of a check cache under the same claim as [`claim_entry`].
///
/// The entry's lease is held exclusively throughout, so no check of this worktree runs. The
/// `incremental` tier is renamed into the trash and deleted after the locks are released. Older
/// finalized sessions are removed one by one while the sibling rustc lock of each is held
/// exclusively and non-blockingly (a busy lock keeps that session) and are never `-working`
/// directories; the lock file goes last, still under the lock. A dry run probes the same locks
/// and removes nothing.
fn trim_entry(
    entry: &Entry,
    tier: Tier,
    locks: &Path,
    scan_started: SystemTime,
    apply: bool,
    legacy_free: &dyn Fn() -> bool,
) -> Option<Trimmed> {
    let incremental = rustc::incremental_dirs(&entry.path);
    let sessions: Vec<_> = match tier {
        Tier::Incremental => Vec::new(),
        Tier::Sessions => incremental
            .iter()
            .flat_map(|dir| rustc::older_sessions(dir))
            .collect(),
    };
    if incremental.is_empty() || (tier == Tier::Sessions && sessions.is_empty()) {
        return None;
    }
    let untouched = |fate| Some(Trimmed { fate, usage: None });
    let Some(held) = lock_entry(entry, locks, scan_started) else {
        return untouched(Fate::InUse);
    };
    if apply && !legacy_free() {
        return untouched(Fate::Paused);
    }
    let mut gone: Vec<PathBuf> = Vec::new();
    let mut trash = None;
    match tier {
        Tier::Incremental if !apply => gone.extend(incremental.iter().cloned()),
        Tier::Incremental => {
            // One unique trash directory per trim; each `incremental` directory moves in by rename.
            let base = trash_path(entry).filter(|base| fs::create_dir(base).is_ok());
            for (index, dir) in incremental.iter().enumerate() {
                if let Some(base) = &base
                    && fs::rename(dir, base.join(index.to_string())).is_ok()
                {
                    gone.push(dir.clone());
                }
            }
            trash = base;
        }
        Tier::Sessions => {
            for session in &sessions {
                // rustc's own protocol: the session is only collected while its lock is ours.
                let Some(lock) = lock_session(&session.lock, apply) else {
                    continue;
                };
                if !apply {
                    gone.push(session.dir.clone());
                } else if fs::remove_dir_all(&session.dir).is_ok() {
                    let _ = fs::remove_file(&session.lock);
                    gone.push(session.dir.clone());
                }
                drop(lock);
            }
        }
    }
    drop(held);
    if let Some(base) = trash {
        let _ = fs::remove_dir_all(base);
    }
    if gone.is_empty() {
        return untouched(Fate::InUse);
    }
    let (usage, _, complete) = measure_usage_excluding(&entry.path, &gone);
    if !complete {
        return untouched(Fate::Unreadable);
    }
    Some(Trimmed {
        fate: Fate::Removed,
        usage: Some(usage),
    })
}

/// Exclusively, non-blockingly locks one rustc session lock file: `None` when another process
/// holds it or it is not a regular file, `Some(None)` for a dry run's lock that does not exist
/// (nothing can hold it, and a probe creates nothing), else the held file. With `create` a missing
/// file is created, as rustc does.
fn lock_session(path: &Path, create: bool) -> Option<Option<File>> {
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if !create && error.kind() == ErrorKind::NotFound => return Some(None),
        Err(_) => return None,
    };
    (file.metadata().ok()?.is_file() && flock(&file, libc::LOCK_EX | libc::LOCK_NB))
        .then_some(Some(file))
}

/// Creates the family trash directory and names a unique destination inside it.
fn trash_path(entry: &Entry) -> Option<PathBuf> {
    let family = match entry.kind {
        Kind::Checks => entry.path.parent()?.parent()?,
        Kind::Telemetry | Kind::Providers | Kind::Release => entry.path.parent()?,
    };
    let trash = family.join(TRASH_DIR);
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = fs::DirBuilder::new().mode(0o700).create(&trash);
    }
    if !safe_dir(&trash) {
        return None;
    }
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Some(trash.join(format!(
        "{}-{}-{nanos}",
        entry.path.file_name()?.to_string_lossy(),
        std::process::id()
    )))
}

/// Deletes leftovers of an interrupted sweep in one (already validated) family root's trash,
/// returning one verdict per leftover; a dry run only reports them.
fn empty_trash(kind: Kind, root: &Path, apply: bool) -> Vec<Verdict> {
    let trash = root.join(TRASH_DIR);
    if !safe_dir(&trash) {
        return Vec::new();
    }
    subdirectories(&trash, |_| true)
        .0
        .into_iter()
        .map(|leftover| {
            let bytes = measure(&leftover).0;
            Verdict {
                kind,
                bytes: if apply {
                    delete(&leftover, bytes)
                } else {
                    bytes
                },
                path: leftover,
                worktree: None,
                reason: Reason::Leftover,
                fate: Fate::Removed,
            }
        })
        .collect()
}

/// Selects and (when `apply`) removes superseded releases under `standalone`, holding the install
/// lock for the whole evaluation (a dry run too) so `current` cannot move meanwhile.
///
/// Kept: the `current` target, the [`RELEASES_KEPT`] newest completed versions, any installed less
/// than [`RELEASE_AGE_FLOOR`] ago, any unreadable, and any containing a live executable. Removal
/// renames into `releases/.trash-<X.Y.Z>-<pid>`, re-snapshots processes and renames back when the
/// snapshot fails or shows a live executable under either path. Any unknown executable path or
/// failed snapshot keeps every release.
fn sweep_releases(
    standalone: &Path,
    apply: bool,
    now: SystemTime,
    snapshot: &dyn Fn() -> Option<Vec<Process>>,
    totals: &mut Totals,
) -> Vec<Verdict> {
    let releases = standalone.join("releases");
    if !safe_dir(standalone) || !safe_dir(&releases) {
        return Vec::new();
    }
    let Some(_install) = try_exclusive(&standalone.join(".install.lock")) else {
        totals.note = Some("install in progress");
        return Vec::new();
    };
    let canonical = fs::canonicalize(&releases).unwrap_or_else(|_| releases.clone());
    // `None` when liveness is unknown (snapshot failed or an executable path is unreadable).
    let executables = || {
        snapshot()?
            .into_iter()
            .map(|(_, exe)| exe)
            .collect::<Option<Vec<_>>>()
    };
    let live = |executables: &[PathBuf], name: &OsStr| {
        let dir = canonical.join(name);
        executables.iter().any(|exe| exe.starts_with(&dir))
    };
    let current = match fs::read_link(standalone.join("current")) {
        Ok(target) => target.file_name().map(OsStr::to_os_string),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(_) => {
            totals.note = Some("current is unreadable, every release kept");
            return Vec::new();
        }
    };
    let (dirs, listed) = subdirectories(&releases, |name| {
        name.to_str().and_then(parse_version).is_some()
    });
    let mut installed = dirs
        .into_iter()
        .filter_map(|path| {
            let completed = fs::symlink_metadata(path.join("COMPLETE"))
                .ok()
                .filter(|metadata| metadata.file_type().is_file())?
                .modified()
                .ok()?;
            let version = parse_version(path.file_name()?.to_str()?)?;
            let (bytes, _, complete) = measure(&path);
            Some((version, completed, path, bytes, complete))
        })
        .collect::<Vec<_>>();
    installed.sort_by_key(|release| std::cmp::Reverse(release.0));
    totals.entries = installed.len();
    totals.bytes = installed.iter().map(|release| release.3).sum();
    let Some(first) = executables() else {
        totals.note = Some("process list unknown, every release kept");
        return Vec::new();
    };
    if !listed {
        totals.note = Some("listing incomplete, every release kept");
        return Vec::new();
    }
    let mut verdicts = Vec::new();
    // A crashed sweep's quarantine: restored when live under either name, else deleted.
    for leftover in subdirectories(&releases, |name| name.as_bytes().starts_with(b".trash-")).0 {
        let Some(trash_name) = leftover.file_name().map(OsStr::to_os_string) else {
            continue;
        };
        let original = trash_name
            .to_string_lossy()
            .trim_start_matches(".trash-")
            .rsplit_once('-')
            .map(|(version, _)| version.to_owned())
            .filter(|version| parse_version(version).is_some());
        let restore = original
            .as_deref()
            .is_some_and(|version| live(&first, OsStr::new(version)))
            || live(&first, &trash_name);
        if !apply {
            continue;
        }
        if restore {
            if let Some(original) = original.map(|version| releases.join(version))
                && !original.exists()
            {
                let _ = fs::rename(&leftover, original);
            }
            continue;
        }
        let bytes = measure(&leftover).0;
        verdicts.push(Verdict {
            kind: Kind::Release,
            bytes: delete(&leftover, bytes),
            path: leftover,
            worktree: None,
            reason: Reason::Leftover,
            fate: Fate::Removed,
        });
    }
    for (_, completed, path, bytes, complete) in installed.iter().skip(RELEASES_KEPT) {
        let name = path.file_name().unwrap_or_default();
        if current.as_deref() == Some(name)
            || now.duration_since(*completed).unwrap_or_default() <= RELEASE_AGE_FLOOR
            || live(&first, name)
        {
            continue;
        }
        let (fate, freed) = if !complete {
            (Fate::Unreadable, *bytes)
        } else if !apply {
            (Fate::Removed, *bytes)
        } else {
            let trash_name = format!(".trash-{}-{}", name.to_string_lossy(), std::process::id());
            let trash = releases.join(&trash_name);
            if fs::rename(path, &trash).is_err() {
                (Fate::InUse, *bytes)
            } else {
                match executables() {
                    Some(second)
                        if !live(&second, name) && !live(&second, OsStr::new(&trash_name)) =>
                    {
                        (Fate::Removed, delete(&trash, *bytes))
                    }
                    _ => {
                        let _ = fs::rename(&trash, path);
                        (Fate::InUse, *bytes)
                    }
                }
            }
        };
        verdicts.push(Verdict {
            kind: Kind::Release,
            path: path.clone(),
            worktree: None,
            bytes: freed,
            reason: Reason::Superseded,
            fate,
        });
    }
    verdicts
}

/// Runs one sweep (or, without `apply`, a dry run) over the state root `state_root`.
///
/// Callers that apply should hold [`SweepLock`]. `snapshot` lists live `agent-ide` processes;
/// the real one is [`process_snapshot`].
pub fn sweep_with(
    state_root: &Path,
    apply: bool,
    now: SystemTime,
    snapshot: &dyn Fn() -> Option<Vec<Process>>,
) -> Report {
    sweep_as(
        &Identity::current(state_root),
        state_root,
        apply,
        now,
        snapshot,
    )
}

/// [`sweep_with`] classifying processes as `identity` does.
fn sweep_as(
    identity: &Identity,
    state_root: &Path,
    apply: bool,
    now: SystemTime,
    snapshot: &dyn Fn() -> Option<Vec<Process>>,
) -> Report {
    let mut report = Report {
        checks: Totals {
            budget: CHECKS_BUDGET_BYTES,
            ..Totals::default()
        },
        telemetry: Totals {
            budget: TELEMETRY_BUDGET_BYTES,
            ..Totals::default()
        },
        providers: Totals {
            budget: PROVIDERS_BUDGET_BYTES,
            ..Totals::default()
        },
        ..Report::default()
    };
    if !safe_dir(state_root) {
        return report;
    }
    let classified = snapshot().map(|processes| identity.classify(&processes));
    report.builds = classified
        .as_ref()
        .map(|classified| classified.builds.clone())
        .unwrap_or_default();
    report.legacy = classified.map(|classified| classified.legacy);
    let paused = report.paused();
    let locks = state_root.join(LOCKS_DIR);
    let legacy_free = || snapshot().is_some_and(|processes| identity.legacy(&processes).is_empty());
    let scan_started = SystemTime::now();
    for (kind, idle) in [
        (Kind::Checks, CHECKS_IDLE),
        (Kind::Telemetry, TELEMETRY_IDLE),
        (Kind::Providers, PROVIDERS_IDLE),
    ] {
        let root = state_root.join(kind.as_str());
        let totals = match kind {
            Kind::Checks => &mut report.checks,
            Kind::Telemetry => &mut report.telemetry,
            _ => &mut report.providers,
        };
        if !safe_dir(&root) {
            if root.exists() {
                totals.note = Some("not a private directory, skipped");
            }
            continue;
        }
        if !paused {
            report.verdicts.extend(empty_trash(kind, &root, apply));
        }
        let (entries, complete) = match kind {
            Kind::Checks => scan_checks(&root, &locks),
            Kind::Telemetry => scan_telemetry(&root, &locks),
            _ => scan_providers(&root, &locks),
        };
        let sharing = Sharing::new(entries.iter().map(|entry| &entry.usage));
        totals.entries = entries.len();
        totals.bytes = sharing.charged();
        totals.logical = sharing.logical();
        totals.private = sharing.private();
        if !complete {
            totals.note = Some("listing incomplete, budget not applied");
        } else if entries.iter().any(|entry| !entry.complete) {
            totals.note = Some("some entries unreadable and kept, total is a lower bound");
        }
        let budget = complete.then_some(totals.budget);
        let mut claim = |entry: &Entry, charge: u64| {
            if paused {
                return (Fate::Paused, charge);
            }
            claim_entry(entry, charge, &locks, scan_started, apply, &legacy_free)
        };
        let mut trim = |entry: &Entry, tier: Tier| {
            if paused {
                return None;
            }
            trim_entry(entry, tier, &locks, scan_started, apply, &legacy_free)
        };
        let trimming: Option<&mut Trim<'_>> = (kind == Kind::Checks).then_some(&mut trim);
        report
            .verdicts
            .extend(select(entries, idle, budget, now, &mut claim, trimming));
    }
    let verdicts = sweep_releases(
        &state_root.join("standalone"),
        apply,
        now,
        snapshot,
        &mut report.releases,
    );
    report.verdicts.extend(verdicts);
    report
}

/// Runs one sweep or dry run with the real clock and process snapshot.
pub fn sweep(state_root: &Path, apply: bool) -> Report {
    sweep_with(state_root, apply, SystemTime::now(), &process_snapshot)
}

/// Exclusive hold of the machine-wide sweep lock; dropping it records the sweep as completed.
#[derive(Debug)]
pub struct SweepLock(File);

impl SweepLock {
    /// Takes the sweep lock without waiting; with `spacing`, also refuses when a sweep completed
    /// more recently than that. `None` means another sweep runs or ran recently.
    pub fn try_acquire(state_root: &Path, spacing: Option<Duration>) -> Option<Self> {
        if !safe_dir(state_root) {
            return None;
        }
        let file = try_exclusive(&state_root.join(LOCKS_DIR).join(SWEEP_LOCK))?;
        // An empty lock file was never stamped by a completed sweep.
        let metadata = file.metadata().ok()?;
        let recent = metadata.len() > 0
            && metadata
                .modified()
                .ok()
                .and_then(|last| SystemTime::now().duration_since(last).ok())
                .is_some_and(|age| spacing.is_some_and(|spacing| age < spacing));
        (!recent).then_some(Self(file))
    }
}

impl Drop for SweepLock {
    /// Stamps the completed sweep (content and mtime) for `SWEEP_SPACING`.
    fn drop(&mut self) {
        use std::io::Write;
        let _ = self.0.set_len(0);
        let _ = self.0.write_all(b"swept\n");
        let _ = self.0.set_modified(SystemTime::now());
    }
}

/// Applies the policy in a long-lived daemon: first after `FIRST_SWEEP_DELAY`, then every
/// `SWEEP_INTERVAL`, each on a blocking thread and only when no daemon swept within
/// `SWEEP_SPACING`. Removals are recorded in this process's error log. Runs until dropped.
pub async fn run_periodically() {
    let mut delay = FIRST_SWEEP_DELAY;
    loop {
        tokio::time::sleep(delay).await;
        delay = SWEEP_INTERVAL;
        let _ = tokio::task::spawn_blocking(|| {
            let Some(root) = state_root() else {
                return;
            };
            if let Some(_lock) = SweepLock::try_acquire(&root, Some(SWEEP_SPACING)) {
                sweep(&root, true).record();
                // The Claude hook key hints below the temporary root go with the same round, but
                // only while no process that may publish one without the lock is alive.
                use crate::errorlog::{Fields, Method, Outcome, record};
                let (outcome, detail) = match hint_publishers_unsafe(&process_snapshot) {
                    Some(unsafe_publishers) if unsafe_publishers.is_empty() => {
                        let hints = crate::hook_hints::collect(
                            Path::new(crate::hook_hints::TMP_ROOT),
                            true,
                            SystemTime::now(),
                            &hint_publishers_safe(),
                        );
                        (
                            Outcome::Completed,
                            format!("hook_key_hints removed={}", hints.stale),
                        )
                    }
                    Some(unsafe_publishers) => (
                        Outcome::Skipped,
                        format!("hook_key_hints paused={}", unsafe_publishers.len()),
                    ),
                    None => (Outcome::Skipped, "hook_key_hints paused=unknown".to_owned()),
                };
                if outcome == Outcome::Skipped || detail != "hook_key_hints removed=0" {
                    record(
                        Method::Retention,
                        outcome,
                        Fields {
                            detail: Some(&detail),
                            ..Default::default()
                        },
                    );
                }
            }
        })
        .await;
    }
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
