//! Private actor-addressed rendezvous records for the managed Codex native hook (T29B §2).
//!
//! One managed Codex MCP process publishes a small, immutable, versioned record per actor route so
//! the native `codex-hook --managed` can find the private runtime serving its exact root session
//! and actor. The layout is `<root>/<full-route-digest>/<publication-nonce>.json` under a fixed
//! effective-UID rendezvous root, with the random runtime directory named only inside the record.
//! Discovery is deterministic and bounded: absence, ambiguity, corruption, contention and stale
//! state all return [`None`] silently; nothing here repairs, chmods, follows symlinks, launches a
//! daemon, reads host configuration, or grants workspace authority.
//!
//! A record is live only while its publisher holds an exclusive advisory lock on the record
//! descriptor, so crash leftovers are inert until a later publisher removes them under the route
//! directory lock. The published attachment is a bearer credential: it never appears in [`Debug`]
//! output and must never be logged. The trust boundary matches the managed Claude precedent:
//! cooperating same-UID processes are trusted, and no protection against hostile same-UID
//! processes is claimed.

use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Schema version of the published record; records carrying any other value are rejected.
const RECORD_VERSION: u32 = 1;

/// Versioned domain string hashed ahead of every framed digest component.
const DIGEST_DOMAIN: &str = "agent-ide/codex-rendezvous/v1";

/// Hard upper bound for one published record; larger records are never written or read.
const MAX_RECORD_BYTES: usize = 4096;

/// Maximum number of actor routes one publisher may keep published at once (T29B §2).
const MAX_ROUTES_PER_PUBLISHER: usize = 64;

/// Maximum number of record names discovery considers in one route directory (T29B §2).
const MAX_RECORDS_PER_ROUTE: usize = 16;

/// Maximum directory entries enumerated in one route directory before refusing it as unbounded.
const MAX_ROUTE_ENTRIES: usize = 512;

/// Maximum byte length of one validated root-session or actor identity component.
const MAX_IDENTITY_LEN: usize = 256;

/// Random bytes behind one publication nonce, rendered as lowercase hexadecimal (32 characters).
const NONCE_BYTES: usize = 16;

/// Fixed suffix of every published record file name.
const RECORD_SUFFIX: &str = ".json";

/// Prefix of in-flight temporary record files; such names are never discoverable records.
const TEMP_PREFIX: &str = ".tmp-";

/// Daemon IPC socket name inside the validated runtime directory (`src/app.rs::SOCKET_NAME`).
const SOCKET_NAME: &str = "agent-ide.sock";

/// Closed failure space of one publication attempt; every path is quiet and retryable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublishError {
    /// The route identity failed validation (empty, oversized, or containing NUL).
    InvalidIdentity,
    /// The fixed attachment is not one valid 64-character lowercase hexadecimal credential.
    InvalidAttachment,
    /// The rendezvous root exists in an unsafe state (wrong owner, mode, or a symlink).
    UnsafeRoot,
    /// The route directory exists in an unsafe state (wrong owner, mode, or a symlink).
    UnsafeRoute,
    /// The runtime directory is missing or unsafe, so no identity could be captured for it.
    InvalidRuntime,
    /// The publisher already keeps the bounded maximum of actor routes.
    TooManyRoutes,
    /// The route-directory lock stayed contended beyond the short bounded wait.
    Contended,
    /// A transient filesystem failure prevented publication; no partial record was left live.
    Unavailable,
}

/// Validated addressing of one managed Codex actor route (T29B §2).
///
/// The root session and the actor are exactly the already-validated host metadata fields; the raw
/// values never reach the filesystem, only the digest does.
#[derive(Clone, Eq, PartialEq)]
pub struct CodexRouteIdentity {
    root_session: String,
    actor: String,
}

impl CodexRouteIdentity {
    /// Validates and creates one route identity from a root session and an exact actor.
    ///
    /// Both components must be non-empty, at most `MAX_IDENTITY_LEN` bytes, and free of NUL so
    /// they can never smuggle separators or terminators into any derived name.
    pub fn new(
        root_session: impl Into<String>,
        actor: impl Into<String>,
    ) -> Result<Self, PublishError> {
        let root_session = root_session.into();
        let actor = actor.into();
        if !valid_identity_component(&root_session) || !valid_identity_component(&actor) {
            return Err(PublishError::InvalidIdentity);
        }
        Ok(Self {
            root_session,
            actor,
        })
    }

    /// Returns the validated root-session component.
    pub fn root_session(&self) -> &str {
        &self.root_session
    }

    /// Returns the validated actor component.
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// Returns the full route digest: BLAKE3 over a versioned domain string, the effective UID,
    /// the root session and the actor, each length-framed, as lowercase hexadecimal.
    ///
    /// The digest is never derived from CWD, repository, PID, tool arguments or timing, and the
    /// length framing keeps distinct component boundaries from colliding.
    pub fn digest(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        frame(&mut hasher, DIGEST_DOMAIN.as_bytes());
        // SAFETY: `geteuid` merely reports the calling process's effective uid.
        frame(&mut hasher, &unsafe { libc::geteuid() }.to_le_bytes());
        frame(&mut hasher, self.root_session.as_bytes());
        frame(&mut hasher, self.actor.as_bytes());
        hasher.finalize().to_hex().to_string()
    }
}

impl fmt::Debug for CodexRouteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Raw session and actor identifiers stay out of diagnostics, matching the record contract.
        formatter
            .debug_struct("CodexRouteIdentity")
            .field("digest", &self.digest())
            .finish()
    }
}

/// Length-frames one byte string into the BLAKE3 stream: a 64-bit little-endian length, then bytes.
fn frame(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

/// Reports whether one identity component is non-empty, bounded, and free of NUL.
fn valid_identity_component(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_IDENTITY_LEN && !value.bytes().any(|byte| byte == 0)
}

/// Reports whether a value is exactly one 64-byte lowercase hexadecimal attachment.
fn valid_attachment(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Reports whether a value is exactly the lowercase-hexadecimal rendering of [`NONCE_BYTES`] bytes.
fn is_nonce_hex(value: &str) -> bool {
    value.len() == NONCE_BYTES * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// The one live, validated hook destination discovered for an actor route.
#[derive(Clone, Eq, PartialEq)]
pub struct HookTarget {
    runtime_dir: PathBuf,
    attachment: String,
}

impl HookTarget {
    /// Returns the absolute canonical runtime directory holding the daemon socket.
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    /// Returns the private bearer attachment to present to the daemon.
    pub fn attachment(&self) -> &str {
        &self.attachment
    }
}

impl fmt::Debug for HookTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The attachment is a secret bearer credential and must never be logged (T29B §2).
        formatter
            .debug_struct("HookTarget")
            .field("runtime_dir", &self.runtime_dir)
            .field("attachment", &"<redacted>")
            .finish()
    }
}

/// The versioned on-disk publication record; at most [`MAX_RECORD_BYTES`] bytes as JSON.
///
/// No raw session identifiers, source paths, tool payloads or execution profiles appear here: the
/// route is addressed only by its digest, and the runtime location plus identity travel in the
/// record so discovery can re-verify the directory it is about to return.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationRecord {
    version: u32,
    route: String,
    nonce: String,
    runtime: String,
    device: u64,
    inode: u64,
    attachment: String,
}

/// One retained publication of a single actor route.
struct Publication {
    route: String,
    nonce: String,
    /// Record descriptor carrying the publisher's exclusive lock for the publisher's lifetime.
    record: File,
    /// Pinned, validated route directory the record was renamed into.
    route_dir: File,
    /// Pinned, validated rendezvous root holding the route directory.
    root_dir: File,
}

impl Publication {
    /// Removes this publication's own record, never a successor that replaced the same name.
    ///
    /// Under the short exclusive route-directory lock, the captured nonce name is unlinked only
    /// while it still refers to the captured record inode; a replaced or missing record is left
    /// untouched. Removal is never recursive, and the route directory is removed only when empty,
    /// with any failure ignored.
    fn unpublish(&self) {
        if lock_directory_exclusive(&self.route_dir) {
            let name = format!("{}{RECORD_SUFFIX}", self.nonce);
            if let (Some(current), Some(own)) = (
                stat_at(&self.route_dir, &name),
                stat_descriptor(&self.record),
            ) && current.st_dev == own.st_dev
                && current.st_ino == own.st_ino
            {
                let _ = unlink_at(&self.route_dir, &name, 0);
            }
        }
        let _ = unlink_at(&self.root_dir, &self.route, libc::AT_REMOVEDIR);
    }
}

/// Publishes and retires the actor routes of one managed Codex MCP process (T29B §2).
///
/// The rendezvous root and runtime directory are fixed at construction (the root is injectable for
/// tests; production callers pass [`default_root`]). Every successful [`publish`](Self::publish)
/// retains the locked record descriptor, so dropping the publisher — or an explicit
/// [`unpublish_all`](Self::unpublish_all) — retires discovery immediately, while an MCP crash
/// leaves only inert unlocked leftovers that a later publisher may prune.
pub struct ManagedCodexPublisher {
    root: PathBuf,
    runtime_dir: PathBuf,
    attachment: String,
    publications: Vec<Publication>,
}

impl ManagedCodexPublisher {
    /// Creates one publisher for a fixed rendezvous root, runtime directory, and attachment.
    ///
    /// Nothing is touched on the filesystem here; validation happens per publication so a transient
    /// failure never disables the publisher. The attachment must be the credential already bound
    /// into the launcher for this MCP process, never a fresh one (T29B §2).
    pub fn new(root: PathBuf, runtime_dir: PathBuf, attachment: impl Into<String>) -> Self {
        Self {
            root,
            runtime_dir,
            attachment: attachment.into(),
            publications: Vec::new(),
        }
    }

    /// Idempotently publishes one actor route, bounded to `MAX_ROUTES_PER_PUBLISHER` routes.
    ///
    /// The record is written completely to a private temporary file inside the route directory,
    /// locked exclusively for the publisher's lifetime, then renamed atomically into place under
    /// the short exclusive route-directory lock — so no partially written record is ever
    /// discoverable. While holding that lock the publisher also removes validated but unlocked
    /// crash leftovers in its own route directory. Another publisher's live record is never
    /// touched, so overlapping publishers on one route deliberately make discovery ambiguous
    /// rather than silently rerouting either session.
    pub fn publish(&mut self, identity: &CodexRouteIdentity) -> Result<(), PublishError> {
        if !valid_attachment(&self.attachment) {
            return Err(PublishError::InvalidAttachment);
        }
        let route = identity.digest();
        if self
            .publications
            .iter()
            .any(|publication| publication.route == route)
        {
            return Ok(());
        }
        if self.publications.len() >= MAX_ROUTES_PER_PUBLISHER {
            return Err(PublishError::TooManyRoutes);
        }

        let root_dir = ensure_private_directory(&self.root)?;
        let runtime =
            fs::canonicalize(&self.runtime_dir).map_err(|_| PublishError::InvalidRuntime)?;
        let runtime_dir = open_directory_at_path(&runtime).ok_or(PublishError::InvalidRuntime)?;
        let runtime_status = stat_descriptor(&runtime_dir).ok_or(PublishError::InvalidRuntime)?;
        if !is_owned_private_directory(&runtime_status) {
            return Err(PublishError::InvalidRuntime);
        }

        match create_directory_at(&root_dir, &route) {
            Ok(()) | Err(libc::EEXIST) => {}
            Err(_) => return Err(PublishError::Unavailable),
        }
        let route_dir = open_directory_at(&root_dir, &route).ok_or(PublishError::UnsafeRoute)?;
        let route_status = stat_descriptor(&route_dir).ok_or(PublishError::UnsafeRoute)?;
        if !is_owned_private_directory(&route_status) {
            return Err(PublishError::UnsafeRoute);
        }

        if !lock_directory_exclusive(&route_dir) {
            return Err(PublishError::Contended);
        }

        // Under the directory lock: remove unlocked crash leftovers, then publish atomically.
        sweep_route_directory(&route_dir, &route);
        let nonce = random_hex(NONCE_BYTES).ok_or(PublishError::Unavailable)?;
        let temp_name = format!("{TEMP_PREFIX}{nonce}");
        let final_name = format!("{nonce}{RECORD_SUFFIX}");
        let body = serde_json::to_vec(&PublicationRecord {
            version: RECORD_VERSION,
            route: route.clone(),
            nonce: nonce.clone(),
            runtime: runtime.to_string_lossy().into_owned(),
            device: runtime_status.st_dev as u64,
            inode: runtime_status.st_ino as u64,
            attachment: self.attachment.clone(),
        })
        .map_err(|_| PublishError::Unavailable)?;
        if body.len() > MAX_RECORD_BYTES {
            return Err(PublishError::Unavailable);
        }

        let record = match write_locked_record(&route_dir, &temp_name, &final_name, &body) {
            Ok(record) => record,
            Err(error) => {
                let _ = unlink_at(&route_dir, &temp_name, 0);
                return Err(error);
            }
        };

        let _ = unlock(&route_dir);
        self.publications.push(Publication {
            route,
            nonce,
            record,
            route_dir,
            root_dir,
        });
        Ok(())
    }

    /// Unpublishes every retained record: only the captured nonce files, never successors.
    pub fn unpublish_all(&mut self) {
        for publication in self.publications.drain(..) {
            publication.unpublish();
        }
    }
}

impl Drop for ManagedCodexPublisher {
    fn drop(&mut self) {
        // Best-effort and quiet: an MCP exit retires its routes without ever blocking or logging.
        self.unpublish_all();
    }
}

impl fmt::Debug for ManagedCodexPublisher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedCodexPublisher")
            .field("root", &self.root)
            .field("runtime_dir", &self.runtime_dir)
            .field("attachment", &"<redacted>")
            .field("published_routes", &self.publications.len())
            .finish()
    }
}

/// Writes one complete record body to a fresh private temporary file, locks it exclusively, and
/// atomically renames it into its final name, returning the retained locked descriptor.
///
/// Any failure removes the temporary file and leaves no record behind; an existing entry at the
/// final name is never replaced, so a nonce collision or a predecessor at the same name makes the
/// publication refuse instead of clobbering state.
fn write_locked_record(
    route_dir: &File,
    temp_name: &str,
    final_name: &str,
    body: &[u8],
) -> Result<File, PublishError> {
    let record = create_file_at(route_dir, temp_name).map_err(|_| PublishError::Unavailable)?;
    if write_all(&record, body).is_err() {
        return Err(PublishError::Unavailable);
    }
    // Hold the exclusive record lock before publication, so the record is live from the first
    // moment a discoverer can see its name.
    if !try_lock(&record, libc::LOCK_EX | libc::LOCK_NB) {
        return Err(PublishError::Unavailable);
    }
    if stat_at(route_dir, final_name).is_some()
        || rename_at(route_dir, temp_name, route_dir, final_name).is_err()
    {
        return Err(PublishError::Unavailable);
    }
    Ok(record)
}

/// Returns the production rendezvous root: `canonical("/private/tmp")/ai-c-<euid>`.
///
/// The fixed root never follows hook or MCP `TMPDIR` overrides. Returns [`None`] when the
/// canonical temporary directory cannot be resolved; callers then simply do not publish.
pub fn default_root() -> Option<PathBuf> {
    let temporary = fs::canonicalize("/private/tmp").ok()?;
    // SAFETY: `geteuid` merely reports the calling process's effective uid.
    Some(temporary.join(format!("ai-c-{}", unsafe { libc::geteuid() })))
}

/// Discovers the single live, validated hook target for one actor route, or [`None`].
///
/// The root and route directories are opened descriptor-relative with `O_NOFOLLOW|O_DIRECTORY`
/// and fstat-validated (effective-UID owner, exact mode `0700`, real directories). Under a bounded
/// shared route-directory lock, at most `MAX_RECORDS_PER_ROUTE` record names are considered;
/// more than that refuses the route as unbounded rather than risking a silent pick past the cap.
/// Each candidate is opened `O_NOFOLLOW`, fstat-validated (regular file, owner, exact mode `0600`,
/// `nlink == 1`, bounded size), probed for liveness — a record that can still be shared-locked has
/// no publisher and is skipped — then fully parsed, identity-checked, and its runtime directory
/// and owner-only daemon socket are re-verified through pinned descriptors before being returned.
/// Zero or several live valid records, and any unsafe state, return [`None`] without ever
/// repairing, deleting, or picking the newest record.
///
/// Every wait is bounded and non-blocking, so the whole lookup stays far inside the hook's total
/// deadline and never blocks unboundedly.
pub fn discover(root: &Path, identity: &CodexRouteIdentity) -> Option<HookTarget> {
    let route = identity.digest();
    let root_dir = open_directory_at_path(root)?;
    if !is_owned_private_directory(&stat_descriptor(&root_dir)?) {
        return None;
    }
    let route_dir = open_directory_at(&root_dir, &route)?;
    if !is_owned_private_directory(&stat_descriptor(&route_dir)?) {
        return None;
    }
    if !lock_directory_shared(&route_dir) {
        return None;
    }

    let names = list_directory(&route_dir, MAX_ROUTE_ENTRIES)?;
    let mut candidates: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| is_record_name(name))
        .collect();
    if candidates.len() > MAX_RECORDS_PER_ROUTE {
        return None;
    }
    candidates.sort_unstable();

    let mut target: Option<HookTarget> = None;
    for name in candidates {
        // One corrupt or unsafe candidate is skipped, never allowed to poison the route's valid
        // siblings and never repaired.
        let Some(record) = (|| {
            let record = open_file_at(&route_dir, name)?;
            let status = stat_descriptor(&record)?;
            if !is_owned_private_record(&status) || status.st_size as usize > MAX_RECORD_BYTES {
                return None;
            }
            // Liveness: if this shared probe succeeds, no publisher holds the record any more.
            if try_lock(&record, libc::LOCK_SH | libc::LOCK_NB) {
                return None;
            }
            Some(record)
        })() else {
            continue;
        };
        let Some(parsed) = validate_record(&record, name, &route) else {
            continue;
        };
        if !validate_runtime(&parsed.runtime, parsed.device, parsed.inode).unwrap_or(false) {
            continue;
        }
        if target.is_some() {
            // Several live records on one route: silent no-match, never "newest wins".
            return None;
        }
        target = Some(HookTarget {
            runtime_dir: PathBuf::from(parsed.runtime),
            attachment: parsed.attachment,
        });
    }
    target
}

/// Validates one live candidate record completely against this route and its own file name.
fn validate_record(record: &File, name: &str, route: &str) -> Option<PublicationRecord> {
    let mut bytes = Vec::new();
    record
        .take(MAX_RECORD_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_RECORD_BYTES {
        return None;
    }
    let parsed: PublicationRecord = serde_json::from_slice(&bytes).ok()?;
    if parsed.version != RECORD_VERSION
        || parsed.route != route
        // The nonce must equal the file-name stem, tying the body to the entry it was found at.
        || parsed.nonce.as_str() != name.strip_suffix(RECORD_SUFFIX)?
        || !valid_attachment(&parsed.attachment)
    {
        return None;
    }
    Some(parsed)
}

/// Re-verifies the recorded runtime directory and its owner-only daemon socket.
///
/// The path must be absolute and already canonical, must open `O_NOFOLLOW|O_DIRECTORY` as a real
/// `0700` effective-UID directory whose device/inode still match the record, and must contain the
/// daemon socket as an owner-only Unix socket (exactly what the daemon sets right after bind). Any
/// mismatch — including a runtime removed and recreated after publication — returns `false`, so a
/// replacement runtime is never adopted through an old record.
fn validate_runtime(runtime: &str, device: u64, inode: u64) -> Option<bool> {
    let path = PathBuf::from(runtime);
    if !path.is_absolute() || fs::canonicalize(&path).ok().as_deref() != Some(path.as_path()) {
        return Some(false);
    }
    let directory = open_directory_at_path(&path)?;
    let status = stat_descriptor(&directory)?;
    if !is_owned_private_directory(&status)
        || status.st_dev as u64 != device
        || status.st_ino != inode
    {
        return Some(false);
    }
    let socket = stat_at(&directory, SOCKET_NAME)?;
    Some(
        socket.st_mode & libc::S_IFMT == libc::S_IFSOCK
            && socket.st_uid == unsafe { libc::geteuid() }
            // Owner-only: no group or other permission bits on the socket inode.
            && socket.st_mode & 0o077 == 0,
    )
}

/// Removes validated but unlocked crash leftovers from this route directory, then orphaned
/// temporary files; best-effort and quiet.
///
/// Called only while the publisher holds the exclusive route-directory lock, so no concurrent
/// publication is in flight in this directory. A leftover is removed only after it validates as a
/// regular owner-only bounded record whose body still names this route, and only while it can be
/// locked exclusively — a live record of another publisher is never touched. Any invalid or locked
/// entry is left in place; publication never repairs foreign state.
fn sweep_route_directory(route_dir: &File, route: &str) {
    let Some(names) = list_directory(route_dir, MAX_ROUTE_ENTRIES) else {
        return;
    };
    for name in names {
        if is_record_name(&name) {
            let Some(record) = open_file_at(route_dir, &name) else {
                continue;
            };
            let removable = stat_descriptor(&record).is_some_and(|status| {
                is_owned_private_record(&status) && status.st_size as usize <= MAX_RECORD_BYTES
            }) && try_lock(&record, libc::LOCK_EX | libc::LOCK_NB)
                && validate_record(&record, &name, route).is_some();
            if removable {
                let _ = unlink_at(route_dir, &name, 0);
            }
        } else if let Some(nonce) = name.strip_prefix(TEMP_PREFIX) {
            if !is_nonce_hex(nonce) {
                continue;
            }
            let Some(status) = stat_at(route_dir, &name) else {
                continue;
            };
            if is_owned_private_record(&status) {
                let _ = unlink_at(route_dir, &name, 0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Descriptor-relative filesystem primitives (macOS-certified path, T29B §2).
// ---------------------------------------------------------------------------

/// Reports whether a status describes a real directory owned by the effective uid with exact `0700`.
fn is_owned_private_directory(status: &libc::stat) -> bool {
    status.st_mode & libc::S_IFMT == libc::S_IFDIR
        && status.st_uid == unsafe { libc::geteuid() }
        && status.st_mode & 0o777 == 0o700
}

/// Reports whether a status describes a regular file owned by the effective uid with exact `0600`
/// and exactly one link — hard-linked records are rejected.
fn is_owned_private_record(status: &libc::stat) -> bool {
    status.st_mode & libc::S_IFMT == libc::S_IFREG
        && status.st_uid == unsafe { libc::geteuid() }
        && status.st_mode & 0o777 == 0o600
        && status.st_nlink == 1
}

/// Creates the rendezvous root if missing and returns it pinned and validated, never repaired.
///
/// A missing directory is created with mode `0700` at birth; an existing one is only ever checked
/// through an `O_NOFOLLOW|O_DIRECTORY` descriptor, so a wrong-mode, foreign-owned, or symlinked
/// root is refused instead of fixed.
fn ensure_private_directory(path: &Path) -> Result<File, PublishError> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(PublishError::Unavailable),
    }
    let directory = open_directory_at_path(path).ok_or(PublishError::UnsafeRoot)?;
    if !is_owned_private_directory(&stat_descriptor(&directory).ok_or(PublishError::UnsafeRoot)?) {
        return Err(PublishError::UnsafeRoot);
    }
    Ok(directory)
}

/// Opens one path as a real directory, refusing a symlinked final component.
fn open_directory_at_path(path: &Path) -> Option<File> {
    let raw = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `raw` is a valid NUL-terminated path; the descriptor is owned from here on.
    let descriptor = unsafe {
        libc::openat(
            libc::AT_FDCWD,
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    nonnegative_file(descriptor)
}

/// Opens one directory component relative to a pinned parent descriptor.
fn open_directory_at(parent: &File, name: &str) -> Option<File> {
    let raw = CString::new(name).ok()?;
    // SAFETY: `raw` is valid for the call; the returned descriptor is owned from here on.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    nonnegative_file(descriptor)
}

/// Opens one regular-file component relative to a pinned parent descriptor, refusing symlinks.
fn open_file_at(parent: &File, name: &str) -> Option<File> {
    let raw = CString::new(name).ok()?;
    // SAFETY: `raw` is valid for the call; the returned descriptor is owned from here on.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            raw.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    nonnegative_file(descriptor)
}

/// Creates one exclusive mode-`0600` file relative to a pinned parent descriptor, refusing symlinks.
fn create_file_at(parent: &File, name: &str) -> std::io::Result<File> {
    let raw = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: `raw` is valid for the call; the returned descriptor is owned from here on.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            raw.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    match nonnegative_file(descriptor) {
        Some(record) => Ok(record),
        None => Err(std::io::Error::last_os_error()),
    }
}

/// Creates one mode-`0700` directory relative to a pinned parent descriptor.
///
/// Returns the raw errno value so the one special case (`EEXIST`) stays cheap to match.
fn create_directory_at(parent: &File, name: &str) -> Result<(), libc::c_int> {
    let raw = CString::new(name).map_err(|_| libc::EINVAL)?;
    // SAFETY: `raw` is valid for the call.
    let outcome = unsafe { libc::mkdirat(parent.as_raw_fd(), raw.as_ptr(), 0o700) };
    if outcome == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO))
    }
}

/// Renames one entry between pinned parent descriptors, atomically publishing the record.
fn rename_at(
    old_parent: &File,
    old_name: &str,
    new_parent: &File,
    new_name: &str,
) -> std::io::Result<()> {
    let old = CString::new(old_name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    let new = CString::new(new_name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: both name strings are valid for the call.
    let outcome = unsafe {
        libc::renameat(
            old_parent.as_raw_fd(),
            old.as_ptr(),
            new_parent.as_raw_fd(),
            new.as_ptr(),
        )
    };
    if outcome == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Unlinks or removes one entry relative to a pinned descriptor; flags may add `AT_REMOVEDIR`.
fn unlink_at(parent: &File, name: &str, flags: libc::c_int) -> std::io::Result<()> {
    let raw = CString::new(name).map_err(|_| std::io::ErrorKind::InvalidInput)?;
    // SAFETY: `raw` is valid for the call.
    let outcome = unsafe { libc::unlinkat(parent.as_raw_fd(), raw.as_ptr(), flags) };
    if outcome == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// fstats one open descriptor; identity captured from the descriptor cannot be path-swapped.
fn stat_descriptor(file: &File) -> Option<libc::stat> {
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `status` is a writable `stat` buffer for the valid descriptor.
    let outcome = unsafe { libc::fstat(file.as_raw_fd(), &mut status) };
    if outcome == 0 { Some(status) } else { None }
}

/// lstats one entry relative to a pinned descriptor without following a final symlink.
fn stat_at(parent: &File, name: &str) -> Option<libc::stat> {
    let raw = CString::new(name).ok()?;
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `raw` and `status` are valid for the call.
    let outcome = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            raw.as_ptr(),
            &mut status,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if outcome == 0 { Some(status) } else { None }
}

/// Applies one non-blocking `flock` operation to an owned descriptor.
fn try_lock(file: &File, operation: libc::c_int) -> bool {
    // SAFETY: the descriptor is valid and owned.
    unsafe { libc::flock(file.as_raw_fd(), operation) == 0 }
}

/// Releases any `flock` on an owned descriptor.
fn unlock(file: &File) -> bool {
    try_lock(file, libc::LOCK_UN)
}

/// Takes the short exclusive route-directory lock within a bounded wait.
fn lock_directory_exclusive(directory: &File) -> bool {
    lock_directory_bounded(directory, libc::LOCK_EX)
}

/// Takes the shared route-directory lock within a bounded wait; contention returns `false`.
fn lock_directory_shared(directory: &File) -> bool {
    lock_directory_bounded(directory, libc::LOCK_SH)
}

/// Retries one non-blocking lock a bounded number of times before giving up quietly.
///
/// The total wait stays around sixteen milliseconds, so a discovery under contention returns
/// [`None`] long before the hook's deadline and a publisher simply fails its publication.
fn lock_directory_bounded(directory: &File, operation: libc::c_int) -> bool {
    for attempt in 0..8 {
        if try_lock(directory, operation | libc::LOCK_NB) {
            return true;
        }
        if attempt + 1 < 8 {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    false
}

/// Wraps one owned descriptor from a syscall result; a negative value carries no descriptor.
fn nonnegative_file(descriptor: libc::c_int) -> Option<File> {
    if descriptor >= 0 {
        // SAFETY: the descriptor is freshly owned by this call and never duplicated elsewhere.
        Some(unsafe { File::from_raw_fd(descriptor) })
    } else {
        None
    }
}

/// Reads at most `cap` entry names through a pinned directory descriptor, or [`None`] if larger.
fn list_directory(directory: &File, cap: usize) -> Option<Vec<String>> {
    // SAFETY: duplicating a valid descriptor yields another owned descriptor.
    let duplicated = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicated < 0 {
        return None;
    }
    // SAFETY: `duplicated` is a valid directory descriptor; `fdopendir` takes ownership.
    let stream = unsafe { libc::fdopendir(duplicated) };
    if stream.is_null() {
        // SAFETY: the duplicated descriptor was not consumed by `fdopendir`.
        unsafe { libc::close(duplicated) };
        return None;
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is a valid directory stream until `closedir`.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: the entry's name is a NUL-terminated string valid until the next `readdir`.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        if name == "." || name == ".." {
            continue;
        }
        if names.len() >= cap {
            // SAFETY: `stream` is still open here.
            unsafe { libc::closedir(stream) };
            return None;
        }
        names.push(name);
    }
    // SAFETY: `stream` is open exactly once here.
    unsafe { libc::closedir(stream) };
    Some(names)
}

/// Returns `bytes` of operating-system randomness as lowercase hexadecimal.
fn random_hex(bytes: usize) -> Option<String> {
    let mut random = vec![0_u8; bytes];
    File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut random)
        .ok()?;
    Some(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Reports whether a name is exactly `<lowercase-hex nonce>.json`, the only discoverable shape.
fn is_record_name(name: &str) -> bool {
    name.len() == NONCE_BYTES * 2 + RECORD_SUFFIX.len()
        && name.ends_with(RECORD_SUFFIX)
        && is_nonce_hex(&name[..NONCE_BYTES * 2])
}

/// Writes a complete buffer through one descriptor.
fn write_all(file: &File, bytes: &[u8]) -> std::io::Result<()> {
    std::io::Write::write_all(&mut &*file, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// One isolated test area below the OS temporary directory, never a production rendezvous root.
    struct TestArea {
        base: PathBuf,
    }

    static AREA_ORDINAL: AtomicU64 = AtomicU64::new(0);

    impl TestArea {
        /// Creates a fresh owner-only base directory unique to one test.
        ///
        /// The name is kept short on purpose: the fixture runtime holds a Unix socket, whose
        /// bound path must stay under `SUN_LEN`, so the tag is deliberately not part of it.
        fn new(tag: &str) -> Self {
            let _ = tag;
            let ordinal = AREA_ORDINAL.fetch_add(1, Ordering::Relaxed);
            let base = std::env::temp_dir()
                .join(format!(".airdv{}-{ordinal}", std::process::id() % 100_000));
            let _ = fs::remove_dir_all(&base);
            fs::create_dir(&base).unwrap();
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700)).unwrap();
            Self { base }
        }

        /// The injectable rendezvous root path; created only by the code under test.
        fn root(&self) -> PathBuf {
            self.base.join("rendezvous")
        }

        /// Creates one valid runtime directory: `0700`, with an owner-only `agent-ide.sock`.
        fn runtime(&self) -> PathBuf {
            let directory = self.base.join("runtime");
            fs::create_dir(&directory).unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let listener = UnixListener::bind(directory.join(SOCKET_NAME)).unwrap();
            fs::set_permissions(
                directory.join(SOCKET_NAME),
                fs::Permissions::from_mode(0o600),
            )
            .unwrap();
            drop(listener);
            directory
        }
    }

    /// The fixture attachment; exactly one 64-byte lowercase hexadecimal credential.
    fn fixture_attachment() -> String {
        "a1b2c3d4".repeat(8)
    }

    /// One identity with distinct, valid components.
    fn fixture_identity(tag: &str) -> CodexRouteIdentity {
        CodexRouteIdentity::new(format!("root-session-{tag}"), format!("actor-{tag}")).unwrap()
    }

    /// The record file name for one hexadecimal nonce stem.
    fn record_name(nonce: &str) -> String {
        format!("{nonce}{RECORD_SUFFIX}")
    }

    /// A complete valid record body for `identity`, addressed at `runtime`.
    fn record_body(identity: &CodexRouteIdentity, nonce: &str, runtime: &Path) -> Vec<u8> {
        let status = fs::symlink_metadata(runtime).unwrap();
        serde_json::to_vec(&PublicationRecord {
            version: RECORD_VERSION,
            route: identity.digest(),
            nonce: nonce.to_owned(),
            runtime: runtime.to_string_lossy().into_owned(),
            device: status.dev(),
            inode: status.ino(),
            attachment: fixture_attachment(),
        })
        .unwrap()
    }

    /// Creates the rendezvous root and one route directory by hand, both `0700`.
    fn seed_route_area(root: &Path, identity: &CodexRouteIdentity) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        let route = root.join(identity.digest());
        fs::create_dir_all(&route).unwrap();
        fs::set_permissions(&route, fs::Permissions::from_mode(0o700)).unwrap();
        route
    }

    /// Seeds a live-locked record with `body` under `name`, returning its held locked descriptor.
    fn seed_locked_record(
        root: &Path,
        identity: &CodexRouteIdentity,
        name: &str,
        body: &[u8],
    ) -> File {
        let path = seed_route_area(root, identity).join(name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(body).unwrap();
        assert!(try_lock(&file, libc::LOCK_EX | libc::LOCK_NB));
        file
    }

    /// Publishes one live record and returns the publisher, runtime path, and identity.
    fn publish_fixture(area: &TestArea) -> (ManagedCodexPublisher, PathBuf, CodexRouteIdentity) {
        let runtime = area.runtime();
        let identity = fixture_identity("main");
        let mut publisher =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        publisher.publish(&identity).unwrap();
        (publisher, runtime, identity)
    }

    /// The single record file the fixture publisher left in the route directory.
    ///
    /// Publication nonces are random, so tests resolve the actual name instead of guessing it.
    fn published_record_path(root: &Path, identity: &CodexRouteIdentity) -> PathBuf {
        let mut entries = fs::read_dir(root.join(identity.digest())).unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert!(entries.next().is_none(), "exactly one publication expected");
        entry.path()
    }

    /// Simulates an MCP crash: releases every retained descriptor without unpublishing, so the
    /// records stay on disk but lose their locks exactly as the OS would leave them.
    fn crash_publisher(mut publisher: ManagedCodexPublisher) {
        drop(std::mem::take(&mut publisher.publications));
        std::mem::forget(publisher);
    }

    #[test]
    fn publish_is_idempotent_and_discovery_round_trips() {
        let area = TestArea::new("roundtrip");
        let (mut publisher, runtime, identity) = publish_fixture(&area);
        publisher.publish(&identity).unwrap();
        let root = area.root();
        assert_eq!(
            fs::read_dir(root.join(identity.digest())).unwrap().count(),
            1
        );
        let target = discover(&root, &identity).expect("one live record");
        assert_eq!(target.runtime_dir(), fs::canonicalize(&runtime).unwrap());
        assert_eq!(target.attachment(), fixture_attachment());
        assert!(discover(&root, &fixture_identity("other")).is_none());
    }

    #[test]
    fn dropped_publisher_unpublishes_its_record() {
        let area = TestArea::new("drop");
        let (publisher, _, identity) = publish_fixture(&area);
        let root = area.root();
        let route_dir = root.join(identity.digest());
        assert!(route_dir.is_dir());
        drop(publisher);
        assert!(
            !route_dir.exists(),
            "drop unpublishes: record and empty route removed"
        );
        assert!(discover(&root, &identity).is_none());
    }

    #[test]
    fn crashed_publisher_leaves_an_unlocked_record_that_discovery_ignores() {
        let area = TestArea::new("crash");
        let (publisher, runtime, identity) = publish_fixture(&area);
        let root = area.root();
        let record = published_record_path(&root, &identity);
        crash_publisher(publisher);
        assert!(record.exists(), "crash leftovers stay on disk");
        assert!(
            discover(&root, &identity).is_none(),
            "unlocked leftover is not live"
        );

        // A later publisher of the same route prunes the validated unlocked leftover.
        let mut successor = ManagedCodexPublisher::new(root.clone(), runtime, fixture_attachment());
        successor.publish(&identity).unwrap();
        assert!(!record.exists(), "validated unlocked crash leftover swept");
        assert!(discover(&root, &identity).is_some());
    }

    #[test]
    fn unpublish_all_removes_own_record_and_empty_route_directory() {
        let area = TestArea::new("unpublish");
        let (mut publisher, _, identity) = publish_fixture(&area);
        let route_dir = area.root().join(identity.digest());
        publisher.unpublish_all();
        assert!(!route_dir.exists(), "empty route directory is removed");
        assert!(area.root().is_dir(), "the root itself is left in place");
        assert!(discover(&area.root(), &identity).is_none());
    }

    #[test]
    fn two_live_publishers_are_ambiguous_until_one_exits() {
        let area = TestArea::new("ambiguity");
        let runtime = area.runtime();
        let identity = fixture_identity("main");
        let mut first =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        let mut second =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        first.publish(&identity).unwrap();
        second.publish(&identity).unwrap();
        assert!(
            discover(&area.root(), &identity).is_none(),
            "several live records: None"
        );
        drop(second);
        assert!(
            discover(&area.root(), &identity).is_some(),
            "exactly one live record"
        );
    }

    #[test]
    fn replaced_runtime_rejects_the_old_record() {
        let area = TestArea::new("replacement");
        let (publisher, runtime, identity) = publish_fixture(&area);
        let root = area.root();
        // Remove the runtime and recreate it at the same path: a fresh inode, same name.
        fs::remove_dir_all(&runtime).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        let listener = UnixListener::bind(runtime.join(SOCKET_NAME)).unwrap();
        fs::set_permissions(runtime.join(SOCKET_NAME), fs::Permissions::from_mode(0o600)).unwrap();
        drop(listener);
        assert!(
            discover(&root, &identity).is_none(),
            "device/inode mismatch rejects the replacement"
        );
        drop(publisher);
    }

    #[test]
    fn wrong_directory_modes_are_refused_without_repair() {
        let area = TestArea::new("dir-mode");
        // Root with the wrong mode from birth.
        fs::create_dir(area.root()).unwrap();
        fs::set_permissions(area.root(), fs::Permissions::from_mode(0o755)).unwrap();
        let identity = fixture_identity("main");
        let runtime = area.runtime();
        let mut publisher =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        assert_eq!(publisher.publish(&identity), Err(PublishError::UnsafeRoot));
        assert_eq!(
            fs::metadata(area.root()).unwrap().permissions().mode() & 0o777,
            0o755,
            "existing directories are never chmod'ed"
        );
        assert!(discover(&area.root(), &identity).is_none());

        // Route directory with the wrong mode from birth.
        fs::set_permissions(area.root(), fs::Permissions::from_mode(0o700)).unwrap();
        let route_dir = area.root().join(identity.digest());
        fs::create_dir(&route_dir).unwrap();
        fs::set_permissions(&route_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(publisher.publish(&identity), Err(PublishError::UnsafeRoute));
        assert!(discover(&area.root(), &identity).is_none());
    }

    #[test]
    fn wrong_record_mode_is_not_discoverable() {
        let area = TestArea::new("record-mode");
        let (publisher, _, identity) = publish_fixture(&area);
        let root = area.root();
        let record = published_record_path(&root, &identity);
        fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(discover(&root, &identity).is_none());
        drop(publisher);
    }

    #[test]
    fn symlinked_root_is_refused() {
        let area = TestArea::new("symlink-root");
        let real = area.base.join("real-root");
        fs::create_dir(&real).unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real, area.root()).unwrap();
        let identity = fixture_identity("main");
        let runtime = area.runtime();
        let mut publisher =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        assert_eq!(publisher.publish(&identity), Err(PublishError::UnsafeRoot));
        assert!(discover(&area.root(), &identity).is_none());
    }

    #[test]
    fn symlinked_route_directory_and_record_are_skipped() {
        let area = TestArea::new("symlink-route");
        let (_keeper, runtime, identity) = publish_fixture(&area);
        let root = area.root();
        let route_dir = root.join(identity.digest());
        // A symlinked record name is never followed; the live record is still found.
        std::os::unix::fs::symlink(
            published_record_path(&root, &identity),
            route_dir.join(record_name(&"f".repeat(NONCE_BYTES * 2))),
        )
        .unwrap();
        assert!(
            discover(&root, &identity).is_some(),
            "symlinked record skipped, live kept"
        );

        // A symlinked route directory is refused outright for discovery and publication.
        let other = fixture_identity("symlink");
        let real_route = area.base.join("elsewhere");
        fs::create_dir(&real_route).unwrap();
        fs::set_permissions(&real_route, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&real_route, root.join(other.digest())).unwrap();
        assert!(discover(&root, &other).is_none());
        let mut publisher = ManagedCodexPublisher::new(root, runtime, fixture_attachment());
        assert_eq!(publisher.publish(&other), Err(PublishError::UnsafeRoute));
    }

    #[test]
    fn hard_linked_records_are_rejected() {
        let area = TestArea::new("hardlink");
        let (publisher, _, identity) = publish_fixture(&area);
        let root = area.root();
        let route_dir = root.join(identity.digest());
        fs::hard_link(
            published_record_path(&root, &identity),
            route_dir.join(record_name(&"e".repeat(NONCE_BYTES * 2))),
        )
        .unwrap();
        assert!(
            discover(&root, &identity).is_none(),
            "nlink == 2 rejects the record"
        );
        drop(publisher);
    }

    #[test]
    fn corrupt_live_records_are_skipped_and_never_poison_valid_ones() {
        let area = TestArea::new("corrupt");
        let runtime = area.runtime();
        let identity = fixture_identity("main");
        let nonce = "1".repeat(NONCE_BYTES * 2);
        // One locked malformed record alone: live-looking, still no target.
        let held = seed_locked_record(&area.root(), &identity, &record_name(&nonce), b"not json");
        assert!(discover(&area.root(), &identity).is_none());

        // A live publisher plus locked junk records: only the valid publication counts.
        let mut publisher =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        publisher.publish(&identity).unwrap();

        let mut oversized = record_body(&identity, &nonce, &runtime);
        oversized.resize(MAX_RECORD_BYTES + 1, b' ');
        let mismatched = {
            let mut body = record_body(&identity, &nonce, &runtime);
            body[8] = b'x'; // Corrupt the JSON body after the opening brace.
            body
        };
        let wrong_nonce = serde_json::to_vec(&PublicationRecord {
            version: RECORD_VERSION,
            route: identity.digest(),
            nonce: "deadbeef".repeat(4),
            runtime: runtime.to_string_lossy().into_owned(),
            device: 0,
            inode: 0,
            attachment: fixture_attachment(),
        })
        .unwrap();
        let wrong_version = serde_json::to_vec(&PublicationRecord {
            version: RECORD_VERSION + 1,
            route: identity.digest(),
            nonce: nonce.clone(),
            runtime: runtime.to_string_lossy().into_owned(),
            device: 0,
            inode: 0,
            attachment: fixture_attachment(),
        })
        .unwrap();
        let bad_attachment = seed_locked_record(
            &area.root(),
            &identity,
            &record_name(&"2".repeat(NONCE_BYTES * 2)),
            br#"{"version":1,"route":"","nonce":"","runtime":"","device":0,"inode":0,"attachment":"nope"}"#,
        );
        let held_oversized = seed_locked_record(
            &area.root(),
            &identity,
            &record_name(&"3".repeat(NONCE_BYTES * 2)),
            &oversized,
        );
        let held_mismatched = seed_locked_record(
            &area.root(),
            &identity,
            &record_name(&"4".repeat(NONCE_BYTES * 2)),
            &mismatched,
        );
        let held_wrong_nonce = seed_locked_record(
            &area.root(),
            &identity,
            &record_name(&"5".repeat(NONCE_BYTES * 2)),
            &wrong_nonce,
        );
        let held_wrong_version = seed_locked_record(
            &area.root(),
            &identity,
            &record_name(&"6".repeat(NONCE_BYTES * 2)),
            &wrong_version,
        );
        assert!(
            discover(&area.root(), &identity).is_some(),
            "invalid records are skipped without poisoning the valid one"
        );
        drop(bad_attachment);
        drop(held_oversized);
        drop(held_mismatched);
        drop(held_wrong_nonce);
        drop(held_wrong_version);
        drop(held);
        drop(publisher);
    }

    #[test]
    fn temporary_records_are_never_discoverable_and_are_swept() {
        let area = TestArea::new("atomic");
        let runtime = area.runtime();
        let identity = fixture_identity("main");
        let nonce = "2".repeat(NONCE_BYTES * 2);
        let route_dir = seed_route_area(&area.root(), &identity);
        // A complete record body under a temporary name is invisible to discovery.
        let temp = route_dir.join(format!("{TEMP_PREFIX}{nonce}"));
        fs::write(&temp, record_body(&identity, &nonce, &runtime)).unwrap();
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(
            discover(&area.root(), &identity).is_none(),
            "no partial record is discoverable"
        );

        // Publishing the same route sweeps the orphaned temporary file and publishes cleanly.
        let mut publisher =
            ManagedCodexPublisher::new(area.root(), runtime.clone(), fixture_attachment());
        publisher.publish(&identity).unwrap();
        assert!(
            !temp.exists(),
            "orphaned temporary file removed under the directory lock"
        );
        assert!(discover(&area.root(), &identity).is_some());
    }

    #[test]
    fn unpublish_cannot_delete_a_successor_at_the_same_name() {
        let area = TestArea::new("successor");
        let (mut publisher, _, identity) = publish_fixture(&area);
        let root = area.root();
        let record_path = published_record_path(&root, &identity);
        // Simulate a successor that replaced the record under the same name with a fresh inode.
        let body = fs::read(&record_path).unwrap();
        fs::remove_file(&record_path).unwrap();
        fs::write(&record_path, body).unwrap();
        fs::set_permissions(&record_path, fs::Permissions::from_mode(0o600)).unwrap();
        publisher.unpublish_all();
        assert!(
            record_path.exists(),
            "the successor's file survives unpublish"
        );
    }

    #[test]
    fn publisher_caps_routes_at_sixty_four() {
        let area = TestArea::new("cap-routes");
        let runtime = area.runtime();
        let mut publisher = ManagedCodexPublisher::new(area.root(), runtime, fixture_attachment());
        for index in 0..MAX_ROUTES_PER_PUBLISHER {
            publisher
                .publish(&fixture_identity(&format!("route-{index}")))
                .unwrap_or_else(|error| panic!("route {index}: {error:?}"));
        }
        assert_eq!(
            publisher.publish(&fixture_identity("overflow")),
            Err(PublishError::TooManyRoutes)
        );
        publisher.unpublish_all();
        assert_eq!(
            fs::read_dir(area.root()).unwrap().count(),
            0,
            "every route directory is removed"
        );
    }

    #[test]
    fn discovery_considers_at_most_sixteen_records_per_route() {
        let area = TestArea::new("cap-records");
        let runtime = area.runtime();
        let identity = fixture_identity("main");
        let root = area.root();
        // Fifteen locked junk records plus the live one: exactly at the bound, discovery succeeds.
        let mut held = Vec::new();
        for index in 0..(MAX_RECORDS_PER_ROUTE - 1) {
            held.push(seed_locked_record(
                &root,
                &identity,
                &record_name(&format!("{index:0>32}")),
                format!("junk-{index}").as_bytes(),
            ));
        }
        let mut publisher =
            ManagedCodexPublisher::new(root.clone(), runtime.clone(), fixture_attachment());
        publisher.publish(&identity).unwrap();
        assert!(discover(&root, &identity).is_some());
        // One more record exceeds the bound and refuses the route outright.
        held.push(seed_locked_record(
            &root,
            &identity,
            &record_name(&"f".repeat(32)),
            b"junk-extra",
        ));
        assert!(
            discover(&root, &identity).is_none(),
            "over the record cap: None"
        );
    }

    #[test]
    fn debug_output_redacts_the_attachment() {
        let area = TestArea::new("redaction");
        let (publisher, runtime, identity) = publish_fixture(&area);
        let root = area.root();
        let target = discover(&root, &identity).unwrap();
        let rendered = format!("{target:?}");
        assert!(!rendered.contains(&fixture_attachment()));
        assert!(rendered.contains("<redacted>"));
        let second = ManagedCodexPublisher::new(root, runtime, fixture_attachment());
        assert!(!format!("{second:?}").contains(&fixture_attachment()));
        assert!(!format!("{publisher:?}").contains(&fixture_attachment()));
        assert!(!format!("{identity:?}").contains("root-session-main"));
    }

    #[test]
    fn digest_is_length_framed_and_identity_validation_holds() {
        let combined = CodexRouteIdentity::new("ab", "c").unwrap();
        let split = CodexRouteIdentity::new("a", "bc").unwrap();
        assert_ne!(
            combined.digest(),
            split.digest(),
            "length framing separates boundaries"
        );
        assert_eq!(
            combined.digest(),
            combined.digest(),
            "digest is deterministic"
        );
        assert_eq!(combined.digest().len(), 64);
        assert!(CodexRouteIdentity::new("", "actor").is_err());
        assert!(CodexRouteIdentity::new("root", "").is_err());
        assert!(CodexRouteIdentity::new("ro\0ot", "actor").is_err());
        assert!(
            CodexRouteIdentity::new("r".repeat(MAX_IDENTITY_LEN + 1), "actor").is_err(),
            "oversized components are rejected"
        );
        assert!(!valid_attachment(&"A".repeat(64)), "uppercase rejected");
        assert!(!valid_attachment(&"g".repeat(64)), "non-hex rejected");
        assert!(valid_attachment(&fixture_attachment()));
    }

    #[test]
    fn discovery_without_any_state_is_quietly_none() {
        let area = TestArea::new("absent");
        let identity = fixture_identity("absent");
        assert!(discover(&area.root(), &identity).is_none());
        assert!(
            default_root()
                .is_some_and(|root| root.to_string_lossy().starts_with("/private/tmp/ai-c-")),
            "the production root is the fixed /private/tmp/ai-c-<euid> path"
        );
    }
}
