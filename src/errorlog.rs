//! Append-only, best-effort error log: one bounded JSON line per operation completion or
//! lifecycle fact.
//!
//! This is deliberately separate from `telemetry`, which stays a bucketed, restart-only, durable
//! SQLite sink with no per-event detail. The error log is a plain rotated file so it can be read
//! without a running daemon, and it carries just enough closed context (worktree, host, actor,
//! correlation id, method, outcome, reason) to tell a normal pending/helper round trip apart from
//! a real failure, to say *why* a real failure happened, and to follow one agent operation across
//! several calls, without ever logging source text, file contents, diffs, command lines, prompts,
//! environment values, or an arbitrary OS error string. Every call and lifecycle fact is logged,
//! not just a failing one, with a closed `level` derived from its `outcome` (`Outcome::level`);
//! the `errors` CLI reader defaults to showing only `warn`/`error` and opts into `info` with
//! `--all`.
//!
//! Writing never blocks or fails the calling operation: every I/O error is swallowed and every
//! critical section is one open-check-write under a short-held lock.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use serde::{Deserialize, Serialize};

use crate::assistance::{
    host_binding::{BindingUnavailable, HostKind},
    reply::FailureCode,
};
use crate::checks::UnavailableReason;

/// Log file name inside `~/.agent-ide/logs/<repository-key>/`.
const LOG_FILE_NAME: &str = "events.jsonl";
/// Rotation threshold; the current file is renamed to `events.jsonl.1` once it exceeds this size.
///
/// Raised from the original 5 MiB once every tool call and lifecycle fact started being logged
/// (not just failures), which grows volume by roughly two orders of magnitude.
const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;
/// Hard cap on the optional bounded `detail` text, matching the checks contract's own cap.
const MAX_DETAIL_BYTES: usize = 160;
/// Prefix shared with `CLAUDE_RUNTIME_PREFIX` in `main.rs` for the `ai-r-<16 hex>` runtime dir name.
const RUNTIME_DIR_PREFIX: &str = "ai-r-";

/// One of the closed daemon/client operations an error-log event can describe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Method {
    /// `ide.start`.
    Start,
    /// `ide.context`.
    Context,
    /// `ide.diff`.
    Diff,
    /// `ide.edit`.
    Edit,
    /// `ide.inspect`.
    Inspect,
    /// `ide.stop`.
    Stop,
    /// A native pre/post hook observation.
    Hook,
    /// A confined background project check.
    Check,
    /// Daemon process lifecycle (start, stop, idle exit, client lease open/close).
    Daemon,
    /// MCP client process lifecycle (re-establishment, transport unavailable).
    Client,
    /// An `<agent-ide>` problems feed block was emitted into a reply.
    Feed,
}

impl Method {
    /// Renders the closed lowercase tag used in the log line and by the reader/summary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Context => "context",
            Self::Diff => "diff",
            Self::Edit => "edit",
            Self::Inspect => "inspect",
            Self::Stop => "stop",
            Self::Hook => "hook",
            Self::Check => "check",
            Self::Daemon => "daemon",
            Self::Client => "client",
            Self::Feed => "feed",
        }
    }
}

/// Closed severity class, always a pure function of [`Outcome`] so it can never disagree with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Level {
    /// A real failure: the operation did not do what it was asked.
    Error,
    /// A boundary was unavailable, refused, cancelled, or retried, without failing outright.
    Warn,
    /// A success, a legitimate pending round trip, or a lifecycle fact.
    Info,
}

impl Level {
    /// Renders the closed lowercase tag used in the log line and by the reader/summary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
        }
    }
}

/// Closed outcome class for one logged event; never a free-form status string.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The operation returned its typed result.
    Completed,
    /// The peer answered `pending`: a helper round trip is required and is not itself a failure.
    Pending,
    /// The result was incomplete or timed out at the bounded observation point.
    Incomplete,
    /// A required local boundary was unavailable.
    Unavailable,
    /// A meaningful closed failure that was not invalid input, capacity, or cancellation.
    Failed,
    /// The operation rejected its bounded parameters.
    Invalid,
    /// The operation was explicitly stopped or cancelled.
    Cancelled,
    /// A claim, launch, or recognition was refused before any physical work started.
    Refused,
    /// A background check failed unrecoverably.
    Fatal,
    /// A background check exceeded its timeout.
    Timeout,
    /// Lifecycle fact: the daemon started serving.
    Started,
    /// Lifecycle fact: the daemon stopped on signal or dispatcher shutdown.
    Stopped,
    /// Lifecycle fact: the daemon exited after its idle timeout.
    IdleExit,
    /// Lifecycle fact: an MCP client re-established its connection to the daemon.
    Reestablished,
    /// Lifecycle fact: a client lease (open `ClientLease` connection) was admitted.
    LeaseOpened,
    /// Lifecycle fact: a client lease was released.
    LeaseClosed,
}

impl Outcome {
    /// Renders the closed lowercase tag used in the log line and by the reader/summary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Pending => "pending",
            Self::Incomplete => "incomplete",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
            Self::Invalid => "invalid",
            Self::Cancelled => "cancelled",
            Self::Refused => "refused",
            Self::Fatal => "fatal",
            Self::Timeout => "timeout",
            Self::Started => "started",
            Self::Stopped => "stopped",
            Self::IdleExit => "idle_exit",
            Self::Reestablished => "reestablished",
            Self::LeaseOpened => "lease_opened",
            Self::LeaseClosed => "lease_closed",
        }
    }

    /// Maps this outcome to its closed severity (T107 full-logging extension): `error` for a real
    /// failure, `warn` for an unavailable/refused/cancelled/transient-check boundary, `info` for a
    /// success, a legitimate pending round trip, or a lifecycle fact.
    pub const fn level(self) -> Level {
        match self {
            Self::Failed | Self::Invalid | Self::Fatal => Level::Error,
            Self::Incomplete
            | Self::Unavailable
            | Self::Cancelled
            | Self::Refused
            | Self::Timeout => Level::Warn,
            Self::Completed
            | Self::Pending
            | Self::Started
            | Self::Stopped
            | Self::IdleExit
            | Self::Reestablished
            | Self::LeaseOpened
            | Self::LeaseClosed => Level::Info,
        }
    }
}

/// Closed reason code: always the most specific existing enum value at the point of failure.
///
/// Never a free-form or partially redacted message. Each variant below names the exact source
/// enum and variant it mirrors, so extending the vocabulary is a compile-time, reviewable change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    // `assistance::host_binding::BindingUnavailable`.
    /// [`BindingUnavailable::InvalidMetadata`].
    InvalidMetadata,
    /// [`BindingUnavailable::MissingField`].
    MissingField,
    /// [`BindingUnavailable::InvalidField`].
    InvalidField,
    /// [`BindingUnavailable::InvalidAttachment`].
    InvalidAttachment,
    /// [`BindingUnavailable::UnsupportedHookPhase`].
    UnsupportedHookPhase,
    /// [`BindingUnavailable::Mismatch`].
    Mismatch,
    /// [`BindingUnavailable::MissingPre`].
    MissingPre,
    /// [`BindingUnavailable::MissingInvocation`].
    MissingInvocation,
    /// [`BindingUnavailable::InactiveBinding`].
    InactiveBinding,
    /// [`BindingUnavailable::Replay`].
    Replay,
    /// [`BindingUnavailable::CapacityExceeded`].
    CapacityExceeded,

    // `assistance::reply::FailureCode`.
    /// [`FailureCode::LauncherConfiguration`].
    LauncherConfiguration,
    /// [`FailureCode::OutsideAllowedRoots`].
    OutsideAllowedRoots,
    /// [`FailureCode::ExecutionProfile`].
    ExecutionProfile,
    /// [`FailureCode::UnsupportedGit`].
    UnsupportedGit,
    /// [`FailureCode::WorkspaceActivation`].
    WorkspaceActivation,
    /// [`FailureCode::WorkspaceAuthority`].
    WorkspaceAuthority,
    /// [`FailureCode::ProviderUnavailable`].
    ProviderUnavailable,
    /// [`FailureCode::ProviderLoading`].
    ProviderLoading,
    /// [`FailureCode::ResolutionUnverified`].
    ResolutionUnverified,
    /// [`FailureCode::Cancelled`].
    Cancelled,
    /// [`FailureCode::Deadline`].
    Deadline,
    /// [`FailureCode::Capacity`].
    Capacity,
    /// [`FailureCode::InvalidDetail`].
    InvalidDetail,
    /// [`FailureCode::SourceUnavailable`].
    SourceUnavailable,
    /// [`FailureCode::SourceTooLarge`]: the source exceeds the read ceiling (T13B).
    SourceTooLarge,
    /// [`FailureCode::Conflict`].
    Conflict,
    /// [`FailureCode::Internal`].
    Internal,

    // `checks::UnavailableReason`.
    /// [`UnavailableReason::Disabled`].
    ChecksDisabled,
    /// [`UnavailableReason::OutsideRoots`].
    OutsideRoots,
    /// [`UnavailableReason::ToolMissing`].
    ToolMissing,
    /// [`UnavailableReason::EnvMissing`].
    EnvMissing,
    /// [`UnavailableReason::NoFiles`].
    NoFiles,
    /// [`UnavailableReason::Fatal`].
    CheckFatal,
    /// [`UnavailableReason::Timeout`].
    CheckTimeout,

    // `assistance::claude_worker::LaunchLedger::diagnose_ignored` classification.
    /// No minted ticket matched the observed command at all.
    NoTicket,
    /// A matching ticket existed but its actor did not match the native pre-hook's actor.
    ActorMismatch,
    /// A matching same-actor ticket existed but was no longer in its unlaunched `Minted` state.
    NotMinted,
    /// A matching same-actor `Minted` ticket existed but its deadline had already passed.
    Expired,

    // `changes::edit::EditOutcome` variants with no `FailureCode` counterpart.
    /// [`crate::changes::edit::EditOutcome::StaleSource`].
    StaleSource,
    /// [`crate::changes::edit::EditOutcome::ConflictingDuplicate`].
    ConflictingDuplicate,
    /// [`crate::changes::edit::EditOutcome::UnsafeTarget`].
    UnsafeTarget,
    /// [`crate::changes::edit::EditOutcome::OutcomeUnknown`].
    EditOutcomeUnknown,

    // Local closed boundaries with no existing upstream enum.
    /// A scheduler `target/` clonefile attempt failed and the check proceeded cold.
    SchedulerCacheCloneFailed,
    /// A reply envelope exceeded its bounded transport size.
    OversizeEnvelope,
}

impl ReasonCode {
    /// Renders the closed lowercase tag used in the log line and by the reader/summary.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidMetadata => "invalid_metadata",
            Self::MissingField => "missing_field",
            Self::InvalidField => "invalid_field",
            Self::InvalidAttachment => "invalid_attachment",
            Self::UnsupportedHookPhase => "unsupported_hook_phase",
            Self::Mismatch => "mismatch",
            Self::MissingPre => "missing_pre",
            Self::MissingInvocation => "missing_invocation",
            Self::InactiveBinding => "inactive_binding",
            Self::Replay => "replay",
            Self::CapacityExceeded => "capacity_exceeded",
            Self::LauncherConfiguration => "launcher_configuration",
            Self::OutsideAllowedRoots => "outside_allowed_roots",
            Self::ExecutionProfile => "execution_profile",
            Self::UnsupportedGit => "unsupported_git",
            Self::WorkspaceActivation => "workspace_activation",
            Self::WorkspaceAuthority => "workspace_authority",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderLoading => "provider_loading",
            Self::ResolutionUnverified => "resolution_unverified",
            Self::Cancelled => "cancelled",
            Self::Deadline => "deadline",
            Self::Capacity => "capacity",
            Self::InvalidDetail => "invalid_detail",
            Self::SourceUnavailable => "source_unavailable",
            Self::SourceTooLarge => "source_too_large",
            Self::Conflict => "conflict",
            Self::Internal => "internal",
            Self::ChecksDisabled => "checks_disabled",
            Self::OutsideRoots => "outside_roots",
            Self::ToolMissing => "tool_missing",
            Self::EnvMissing => "env_missing",
            Self::NoFiles => "no_files",
            Self::CheckFatal => "check_fatal",
            Self::CheckTimeout => "check_timeout",
            Self::NoTicket => "no_ticket",
            Self::ActorMismatch => "actor_mismatch",
            Self::NotMinted => "not_minted",
            Self::Expired => "expired",
            Self::StaleSource => "stale_source",
            Self::ConflictingDuplicate => "conflicting_duplicate",
            Self::UnsafeTarget => "unsafe_target",
            Self::EditOutcomeUnknown => "edit_outcome_unknown",
            Self::SchedulerCacheCloneFailed => "scheduler_cache_clone_failed",
            Self::OversizeEnvelope => "oversize_envelope",
        }
    }
}

impl From<BindingUnavailable> for ReasonCode {
    /// Drops only the closed static field-name payload of `MissingField`/`InvalidField`; every
    /// other variant maps one-to-one.
    fn from(value: BindingUnavailable) -> Self {
        match value {
            BindingUnavailable::InvalidMetadata => Self::InvalidMetadata,
            BindingUnavailable::MissingField(_) => Self::MissingField,
            BindingUnavailable::InvalidField(_) => Self::InvalidField,
            BindingUnavailable::InvalidAttachment => Self::InvalidAttachment,
            BindingUnavailable::UnsupportedHookPhase => Self::UnsupportedHookPhase,
            BindingUnavailable::Mismatch => Self::Mismatch,
            BindingUnavailable::MissingPre => Self::MissingPre,
            BindingUnavailable::MissingInvocation => Self::MissingInvocation,
            BindingUnavailable::InactiveBinding => Self::InactiveBinding,
            BindingUnavailable::Replay => Self::Replay,
            BindingUnavailable::CapacityExceeded => Self::CapacityExceeded,
        }
    }
}

impl From<FailureCode> for ReasonCode {
    fn from(value: FailureCode) -> Self {
        match value {
            FailureCode::LauncherConfiguration => Self::LauncherConfiguration,
            FailureCode::OutsideAllowedRoots => Self::OutsideAllowedRoots,
            FailureCode::ExecutionProfile | FailureCode::ExecutionProfileCause(_) => {
                Self::ExecutionProfile
            }
            FailureCode::UnsupportedGit => Self::UnsupportedGit,
            FailureCode::WorkspaceActivation => Self::WorkspaceActivation,
            FailureCode::WorkspaceAuthority => Self::WorkspaceAuthority,
            FailureCode::ProviderUnavailable => Self::ProviderUnavailable,
            FailureCode::ProviderLoading => Self::ProviderLoading,
            FailureCode::ResolutionUnverified => Self::ResolutionUnverified,
            FailureCode::Cancelled => Self::Cancelled,
            FailureCode::Deadline => Self::Deadline,
            FailureCode::Capacity => Self::Capacity,
            FailureCode::InvalidDetail => Self::InvalidDetail,
            FailureCode::SourceUnavailable => Self::SourceUnavailable,
            FailureCode::SourceTooLarge { .. } => Self::SourceTooLarge,
            FailureCode::Conflict => Self::Conflict,
            FailureCode::Internal => Self::Internal,
        }
    }
}

impl ReasonCode {
    /// Maps a non-completed [`crate::changes::edit::EditOutcome`] to its closed reason code;
    /// `None` for the three completed variants, which never reach a failure boundary.
    pub fn from_edit_outcome(value: crate::changes::edit::EditOutcome) -> Option<Self> {
        use crate::changes::edit::EditOutcome;
        Some(match value {
            EditOutcome::Created | EditOutcome::Replaced | EditOutcome::Unchanged => return None,
            EditOutcome::StaleSource => Self::StaleSource,
            EditOutcome::ConflictingDuplicate => Self::ConflictingDuplicate,
            EditOutcome::UnsafeTarget => Self::UnsafeTarget,
            EditOutcome::CancelledNoEffect => Self::Cancelled,
            EditOutcome::DeadlineNoEffect => Self::Deadline,
            EditOutcome::CapacityNoEffect => Self::Capacity,
            EditOutcome::OutcomeUnknown => Self::EditOutcomeUnknown,
            EditOutcome::UnavailableBeforeDispatch => Self::SourceUnavailable,
        })
    }
}

impl From<UnavailableReason> for ReasonCode {
    fn from(value: UnavailableReason) -> Self {
        match value {
            UnavailableReason::Disabled => Self::ChecksDisabled,
            UnavailableReason::ReadRestricted => Self::ExecutionProfile,
            UnavailableReason::OutsideRoots => Self::OutsideRoots,
            UnavailableReason::ToolMissing => Self::ToolMissing,
            UnavailableReason::EnvMissing => Self::EnvMissing,
            UnavailableReason::NoFiles => Self::NoFiles,
            UnavailableReason::Fatal => Self::CheckFatal,
            UnavailableReason::Timeout => Self::CheckTimeout,
        }
    }
}

/// Bounds `text` to at most [`MAX_DETAIL_BYTES`] bytes on a `char` boundary.
fn bounded_detail(text: &str) -> &str {
    if text.len() <= MAX_DETAIL_BYTES {
        return text;
    }
    let mut end = MAX_DETAIL_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Renders `unix_seconds` as a bounded `YYYY-MM-DDTHH:MM:SSZ` UTC timestamp with no time crate.
///
/// Public so the `errors` CLI reader can render an identical `--since` cutoff for a plain string
/// comparison against recorded events, without parsing them back into a numeric timestamp.
pub fn format_rfc3339(unix_seconds: u64) -> String {
    let days = (unix_seconds / 86_400) as i64;
    let secs_of_day = unix_seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: converts a day count since the Unix epoch (1970-01-01) to
/// a proleptic-Gregorian `(year, month, day)` triple. Valid across the whole `i64` day range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year_of_era = yoe as i64;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Derives the 16 lowercase hex character repository key shared with `~/.agent-ide/checks/<id>`
/// and the `ai-r-<id>` runtime directory name (EYES-r2 §2).
///
/// The fast path strips the shared `ai-r-` runtime-directory prefix, since that name is already
/// exactly this id. A `runtime_dir` that was not produced by the shared rendezvous derivation
/// (for example a manually chosen path in a test or a non-managed invocation) falls back to
/// hashing its own canonicalized bytes, which is still stable and collision-resistant even though
/// it will not line up with `~/.agent-ide/checks/<id>` in that uncommon case.
pub fn repository_key(runtime_dir: &Path) -> String {
    if let Some(name) = runtime_dir.file_name().and_then(|name| name.to_str())
        && let Some(candidate) = name.strip_prefix(RUNTIME_DIR_PREFIX)
        && candidate.len() == 16
        && candidate.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return candidate.to_ascii_lowercase();
    }
    let canonical = fs::canonicalize(runtime_dir).unwrap_or_else(|_| runtime_dir.to_path_buf());
    hash16(canonical.as_os_str().as_encoded_bytes())
}

/// Derives a 16 hex character key from `bytes`: the first 8 bytes of its BLAKE3 digest, hex
/// encoded. Mirrors `checks::scheduler::hash16` and the `ai-r-<id>` derivation exactly.
fn hash16(bytes: &[u8]) -> String {
    let digest = blake3::hash(bytes);
    digest.as_bytes()[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Appends bounded, rotated JSON Lines events to one file, swallowing every I/O failure.
pub struct Writer {
    dir: PathBuf,
    lock: Mutex<()>,
}

impl Writer {
    /// Creates a writer over `dir` (typically `~/.agent-ide/logs/<repository-key>`). Nothing is
    /// created or validated until the first [`Writer::append`] call.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            lock: Mutex::new(()),
        }
    }

    /// Appends one already-encoded line (without its trailing newline) under a short lock.
    ///
    /// Never blocks the caller beyond one open/write/close, never panics, and never surfaces an
    /// error: a missing or unwritable directory, a rotation failure, or a write failure all just
    /// drop this one line.
    pub fn append(&self, line: &[u8]) {
        let Ok(_guard) = self.lock.lock() else {
            return;
        };
        let _ = self.append_inner(line);
    }

    fn append_inner(&self, line: &[u8]) -> std::io::Result<()> {
        ensure_private_dir(&self.dir)?;
        let path = self.dir.join(LOG_FILE_NAME);
        if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.len() > MAX_LOG_BYTES) {
            let rotated = self.dir.join(format!("{LOG_FILE_NAME}.1"));
            let _ = fs::rename(&path, &rotated);
        }
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&path)?;
        // One write per record: daemon and client processes append to the same file, and a
        // separate newline write lets another process land between a record and its newline.
        let mut record = Vec::with_capacity(line.len() + 1);
        record.extend_from_slice(line);
        record.push(b'\n');
        file.write_all(&record)
    }
}

/// Creates `dir` as an owner-only `0700` directory tree if it does not already exist.
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.is_dir()) {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)
}

/// Process-wide writer, set at most once per process by [`init`]. Absent until initialized, and
/// permanently absent when initialization failed or was never attempted; [`record`] is then a
/// no-op, matching every other fail-open boundary in this daemon.
static WRITER: OnceLock<Option<Writer>> = OnceLock::new();

/// Initializes the process-wide error log writer from a daemon or MCP client's runtime directory.
///
/// Idempotent: only the first call in a process takes effect. Creates no file; the directory and
/// file are created lazily by the first [`record`] call that actually has something to log.
///
/// The log directory is keyed by [`LOG_KEY_ENV`] when the spawning MCP process supplied it (a
/// managed Codex daemon runs in a random `ai-<random>` runtime that says nothing about its
/// repository), else by [`repository_key`] of `runtime_dir`.
pub fn init(runtime_dir: &Path) {
    let key = std::env::var(LOG_KEY_ENV)
        .ok()
        .filter(|key| valid_key(key))
        .unwrap_or_else(|| repository_key(runtime_dir));
    init_repository(&key);
}

/// Initializes the process-wide writer for an already derived repository `key` (first call wins).
///
/// For an MCP client whose own runtime directory says nothing about its repository.
pub fn init_repository(key: &str) {
    let _ = WRITER.set(
        log_root()
            .filter(|_| valid_key(key))
            .map(|root| Writer::new(root.join(key))),
    );
}

/// Environment variable carrying the 16 lowercase hex repository key the reader derives for the
/// daemon's repository, set by the MCP process that spawns the daemon.
pub const LOG_KEY_ENV: &str = "AGENT_IDE_LOG_KEY";

/// Whether `key` has the shape of a repository key: 16 lowercase hex characters.
fn valid_key(key: &str) -> bool {
    key.len() == 16 && key.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Returns `<real home>/.agent-ide/logs` (see [`crate::userhome`]), or `None` without a home.
pub fn log_root() -> Option<PathBuf> {
    Some(
        crate::userhome::user_home()?
            .join(".agent-ide")
            .join("logs"),
    )
}

/// Optional closed context for one [`record`] call; every field is logged only when the caller
/// has it cheaply at hand.
///
/// `correlation` is the call id / `detail_ref` / activation id already known to the daemon for
/// this one operation, so one agent action (hook -> mint -> helper claim -> settle -> inspect) can
/// be followed across its several log lines; it is an opaque id, never source text. `detail` is
/// bounded to 160 bytes and must already be privacy-safe (an opaque id, or existing sanitized
/// checker text) before this call; it typically carries a fact `correlation` cannot (a checker's
/// sanitized error line, a byte length).
#[derive(Clone, Copy, Debug, Default)]
pub struct Fields<'a> {
    /// Closed reason code for a non-success outcome.
    pub reason: Option<ReasonCode>,
    /// Worktree path, when a specific worktree is already resolved.
    pub worktree: Option<&'a Path>,
    /// Host contract, when already known.
    pub host: Option<HostKind>,
    /// Actor id, when already known.
    pub actor: Option<&'a str>,
    /// Opaque call id / `detail_ref` / activation id already known to the daemon.
    pub correlation: Option<&'a str>,
    /// Bounded, already privacy-safe free text (at most 160 bytes; longer text is truncated).
    pub detail: Option<&'a str>,
    /// Elapsed wall-clock duration of the logged operation, saturated to whole milliseconds.
    pub duration_ms: Option<u32>,
}

/// Records one event, best-effort: never blocks, never panics, never surfaces an error.
///
/// A no-op before [`init`], when initialization failed, or when the process-wide writer is
/// otherwise unavailable. `level` is derived from `outcome` (see [`Outcome::level`]) so it can
/// never disagree with it.
pub fn record(method: Method, outcome: Outcome, fields: Fields<'_>) {
    let Some(Some(writer)) = WRITER.get() else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    writer.append(&build_line(method, outcome, fields, timestamp));
}

/// Renders one canonical JSON Lines record (without its trailing newline); pure and side-effect
/// free so [`record`]'s exact wire shape is unit-testable without touching the global writer.
pub(crate) fn build_line(
    method: Method,
    outcome: Outcome,
    fields: Fields<'_>,
    timestamp: u64,
) -> Vec<u8> {
    let mut object = serde_json::Map::new();
    object.insert(
        "ts".to_owned(),
        serde_json::Value::String(format_rfc3339(timestamp)),
    );
    object.insert(
        "level".to_owned(),
        serde_json::Value::String(outcome.level().as_str().to_owned()),
    );
    object.insert(
        "method".to_owned(),
        serde_json::Value::String(method.as_str().to_owned()),
    );
    object.insert(
        "outcome".to_owned(),
        serde_json::Value::String(outcome.as_str().to_owned()),
    );
    if let Some(reason) = fields.reason {
        object.insert(
            "reason".to_owned(),
            serde_json::Value::String(reason.as_str().to_owned()),
        );
    }
    if let Some(worktree) = fields.worktree {
        object.insert(
            "worktree".to_owned(),
            serde_json::Value::String(worktree.to_string_lossy().into_owned()),
        );
    }
    if let Some(host) = fields.host {
        let host = match host {
            HostKind::Claude => "claude",
            HostKind::Codex => "codex",
        };
        object.insert(
            "host".to_owned(),
            serde_json::Value::String(host.to_owned()),
        );
    }
    if let Some(actor) = fields.actor {
        object.insert(
            "actor".to_owned(),
            serde_json::Value::String(actor.to_owned()),
        );
    }
    if let Some(correlation) = fields.correlation {
        object.insert(
            "correlation".to_owned(),
            serde_json::Value::String(correlation.to_owned()),
        );
    }
    if let Some(detail) = fields.detail {
        object.insert(
            "detail".to_owned(),
            serde_json::Value::String(bounded_detail(detail).to_owned()),
        );
    }
    if let Some(duration_ms) = fields.duration_ms {
        object.insert(
            "duration_ms".to_owned(),
            serde_json::Value::Number(duration_ms.into()),
        );
    }
    serde_json::to_vec(&serde_json::Value::Object(object)).unwrap_or_default()
}

/// One decoded log line, as read back by [`read_events`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LoggedEvent {
    /// RFC 3339 UTC timestamp.
    #[serde(rename = "ts")]
    pub timestamp: String,
    /// Closed severity tag. Defaults to `warn` for a line written before this field existed, since
    /// every event logged back then was already a non-success outcome.
    #[serde(default = "default_level")]
    pub level: String,
    /// Closed method tag.
    pub method: String,
    /// Closed outcome tag.
    pub outcome: String,
    /// Closed reason tag, when the event carried one.
    #[serde(default)]
    pub reason: Option<String>,
    /// Worktree path, when the event carried one.
    #[serde(default)]
    pub worktree: Option<String>,
    /// Host kind, when the event carried one.
    #[serde(default)]
    pub host: Option<String>,
    /// Actor id, when the event carried one.
    #[serde(default)]
    pub actor: Option<String>,
    /// Opaque call id / `detail_ref` / activation id, when the event carried one.
    #[serde(default)]
    pub correlation: Option<String>,
    /// Bounded sanitized detail text, when the event carried one.
    #[serde(default)]
    pub detail: Option<String>,
    /// Elapsed duration of the logged operation in milliseconds, when the event carried one.
    #[serde(default)]
    pub duration_ms: Option<u32>,
}

impl Default for LoggedEvent {
    fn default() -> Self {
        Self {
            timestamp: String::new(),
            level: default_level(),
            method: String::new(),
            outcome: String::new(),
            reason: None,
            worktree: None,
            host: None,
            actor: None,
            correlation: None,
            detail: None,
            duration_ms: None,
        }
    }
}

/// Fallback [`LoggedEvent::level`] for a line written before that field existed.
fn default_level() -> String {
    Level::Warn.as_str().to_owned()
}

/// Parses one JSON Lines record; a malformed or empty line yields `None` and is skipped rather
/// than aborting the whole read.
pub fn parse_line(line: &str) -> Option<LoggedEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    serde_json::from_str(line).ok()
}

/// Reads every well-formed event from `dir`'s rotated log files, oldest generation first.
///
/// `dir` not existing, or either file being unreadable, yields whatever could still be read
/// rather than an error: this must work identically whether or not a daemon is running.
pub fn read_events(dir: &Path) -> Vec<LoggedEvent> {
    let mut events = Vec::new();
    for name in [format!("{LOG_FILE_NAME}.1"), LOG_FILE_NAME.to_owned()] {
        let Ok(contents) = fs::read_to_string(dir.join(&name)) else {
            continue;
        };
        events.extend(contents.lines().filter_map(parse_line));
    }
    events
}

/// Keeps only events at `Level::Warn` or `Level::Error`, the default `errors` CLI view.
///
/// A caller wanting every level (`--all`) simply skips calling this at all.
pub fn retain_warn_and_error(events: &mut Vec<LoggedEvent>) {
    events.retain(|event| event.level != Level::Info.as_str());
}

/// Renders one compact `time level method outcome reason worktree detail` line; absent fields are
/// `-`.
pub fn format_line(event: &LoggedEvent) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        event.timestamp,
        event.level,
        event.method,
        event.outcome,
        event.reason.as_deref().unwrap_or("-"),
        event.worktree.as_deref().unwrap_or("-"),
        event.detail.as_deref().unwrap_or("-"),
    )
}

/// Groups `events` by `(level, method, outcome, reason)`, sorted by descending count then by key.
pub fn summarize(events: &[LoggedEvent]) -> Vec<(String, String, String, String, usize)> {
    let mut counts: std::collections::BTreeMap<(String, String, String, String), usize> =
        std::collections::BTreeMap::new();
    for event in events {
        let key = (
            event.level.clone(),
            event.method.clone(),
            event.outcome.clone(),
            event.reason.clone().unwrap_or_else(|| "-".to_owned()),
        );
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut rows: Vec<(String, String, String, String, usize)> = counts
        .into_iter()
        .map(|((level, method, outcome, reason), count)| (level, method, outcome, reason, count))
        .collect();
    rows.sort_by(|left, right| {
        right.4.cmp(&left.4).then_with(|| {
            (&left.0, &left.1, &left.2, &left.3).cmp(&(&right.0, &right.1, &right.2, &right.3))
        })
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent-ide-errorlog-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn rfc3339_formats_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2026-09-18T00:00:00Z, cross-checked against `date -u -j -f '%Y-%m-%d' 2026-09-18 +%s`.
        assert_eq!(format_rfc3339(1_789_689_600), "2026-09-18T00:00:00Z");
        assert_eq!(format_rfc3339(86_461), "1970-01-02T00:01:01Z");
    }

    #[test]
    fn repository_key_strips_the_runtime_prefix() {
        let key = repository_key(Path::new("/private/tmp/ai-r-0123456789abcdef"));
        assert_eq!(key, "0123456789abcdef");
    }

    #[test]
    fn repository_key_falls_back_to_a_stable_hash_for_other_paths() {
        let first = repository_key(Path::new("/some/nonexistent/path/one"));
        let second = repository_key(Path::new("/some/nonexistent/path/one"));
        let other = repository_key(Path::new("/some/nonexistent/path/two"));
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(first.len(), 16);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn writer_creates_a_private_directory_and_file() {
        let dir = temp_dir("basic");
        let writer = Writer::new(dir.clone());
        writer.append(br#"{"ts":"1970-01-01T00:00:00Z","method":"start","outcome":"failed"}"#);
        let path = dir.join(LOG_FILE_NAME);
        let contents = fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    /// Independent writers stand in for the daemon and client processes sharing one file: no
    /// record may be split from its newline by another writer's record.
    #[test]
    fn concurrent_writers_never_join_two_records_on_one_line() {
        let dir = temp_dir("concurrent");
        let line = br#"{"ts":"1970-01-01T00:00:00Z","method":"hook","outcome":"unavailable"}"#;
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let writer = Writer::new(dir.clone());
                scope.spawn(move || {
                    for _ in 0..500 {
                        writer.append(line);
                    }
                });
            }
        });
        let contents = fs::read_to_string(dir.join(LOG_FILE_NAME)).unwrap();
        assert_eq!(contents.lines().count(), 2000);
        assert!(contents.lines().all(|record| record.as_bytes() == line));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_rotates_past_the_size_ceiling_and_keeps_one_generation() {
        let dir = temp_dir("rotate");
        let writer = Writer::new(dir.clone());
        let big_line = vec![b'a'; 1024];
        // 5 KiB threshold stand-in via direct file manipulation keeps this test fast: write past
        // MAX_LOG_BYTES by pre-seeding the file, then prove the very next append rotates it.
        writer.append(&big_line);
        let path = dir.join(LOG_FILE_NAME);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        // Pad the file past the rotation ceiling without going through the writer's own lock.
        let filler =
            vec![b'x'; (MAX_LOG_BYTES as usize) - fs::metadata(&path).unwrap().len() as usize + 1];
        file.write_all(&filler).unwrap();
        drop(file);
        assert!(fs::metadata(&path).unwrap().len() > MAX_LOG_BYTES);
        writer.append(b"after-rotation");
        let rotated = dir.join(format!("{LOG_FILE_NAME}.1"));
        assert!(fs::metadata(&rotated).unwrap().len() > MAX_LOG_BYTES);
        let current = fs::read_to_string(&path).unwrap();
        assert_eq!(current.trim_end(), "after-rotation");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writer_never_panics_on_an_unwritable_directory() {
        let dir = temp_dir("unwritable-parent").join("blocked");
        // The parent does not exist and DirBuilder::create with a nonexistent grandparent under a
        // regular file target must fail cleanly rather than panic.
        let blocking_file = dir.parent().unwrap().to_path_buf();
        fs::write(&blocking_file, b"not a directory").unwrap();
        let writer = Writer::new(dir.join("child"));
        writer.append(b"dropped-silently");
        assert!(!dir.join("child").join(LOG_FILE_NAME).exists());
        let _ = fs::remove_file(&blocking_file);
    }

    #[test]
    fn record_is_a_silent_no_op_before_init() {
        // `WRITER` is process-global and `init` is exercised elsewhere; this only proves `record`
        // never panics when called (as it would be from an uninitialized MCP client helper run).
        record(
            Method::Start,
            Outcome::Failed,
            Fields {
                reason: Some(ReasonCode::Internal),
                ..Default::default()
            },
        );
    }

    #[test]
    fn parse_line_skips_blank_and_malformed_lines() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("not json").is_none());
        let event = parse_line(
            r#"{"ts":"2026-09-18T00:00:00Z","method":"inspect","outcome":"failed","reason":"source_unavailable"}"#,
        )
        .unwrap();
        assert_eq!(event.method, "inspect");
        assert_eq!(event.reason.as_deref(), Some("source_unavailable"));
        assert!(event.worktree.is_none());
    }

    #[test]
    fn read_events_reads_the_rotated_generation_before_the_current_one() {
        let dir = temp_dir("read-order");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{LOG_FILE_NAME}.1")),
            "{\"ts\":\"2026-09-18T00:00:00Z\",\"method\":\"start\",\"outcome\":\"failed\"}\n",
        )
        .unwrap();
        fs::write(
            dir.join(LOG_FILE_NAME),
            "{\"ts\":\"2026-09-18T00:01:00Z\",\"method\":\"stop\",\"outcome\":\"completed\"}\n",
        )
        .unwrap();
        let events = read_events(&dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].method, "start");
        assert_eq!(events[1].method, "stop");
        // A line written before `level` existed defaults to `warn`, matching every event that
        // reached the log back when only non-success outcomes were recorded.
        assert_eq!(events[0].level, "warn");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn outcome_level_is_a_pure_function_of_outcome() {
        assert_eq!(Outcome::Failed.level(), Level::Error);
        assert_eq!(Outcome::Invalid.level(), Level::Error);
        assert_eq!(Outcome::Fatal.level(), Level::Error);
        assert_eq!(Outcome::Unavailable.level(), Level::Warn);
        assert_eq!(Outcome::Refused.level(), Level::Warn);
        assert_eq!(Outcome::Timeout.level(), Level::Warn);
        assert_eq!(Outcome::Completed.level(), Level::Info);
        assert_eq!(Outcome::Pending.level(), Level::Info);
        assert_eq!(Outcome::Started.level(), Level::Info);
        assert_eq!(Outcome::LeaseOpened.level(), Level::Info);
    }

    #[test]
    fn record_writes_level_and_correlation_for_every_call_not_just_failures() {
        let dir = temp_dir("full-logging");
        let writer = Writer::new(dir.clone());
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let line = build_line(
            Method::Inspect,
            Outcome::Completed,
            Fields {
                correlation: Some("detail-ref-42"),
                duration_ms: Some(7),
                ..Default::default()
            },
            timestamp,
        );
        writer.append(&line);
        let events = read_events(&dir);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].level, "info");
        assert_eq!(events[0].outcome, "completed");
        assert_eq!(events[0].correlation.as_deref(), Some("detail-ref-42"));
        assert_eq!(events[0].duration_ms, Some(7));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retain_warn_and_error_drops_only_info_events() {
        let mk = |level: &str| LoggedEvent {
            level: level.to_owned(),
            ..Default::default()
        };
        let mut events = vec![mk("info"), mk("warn"), mk("error"), mk("info")];
        retain_warn_and_error(&mut events);
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.level != "info"));
    }

    #[test]
    fn format_line_uses_a_dash_for_every_absent_field() {
        let event = LoggedEvent {
            timestamp: "2026-09-18T00:00:00Z".to_owned(),
            level: "warn".to_owned(),
            method: "start".to_owned(),
            outcome: "unavailable".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            format_line(&event),
            "2026-09-18T00:00:00Z warn start unavailable - - -"
        );
    }

    #[test]
    fn summarize_groups_and_orders_by_descending_count() {
        let mk = |level: &str, method: &str, outcome: &str, reason: Option<&str>| LoggedEvent {
            timestamp: "2026-09-18T00:00:00Z".to_owned(),
            level: level.to_owned(),
            method: method.to_owned(),
            outcome: outcome.to_owned(),
            reason: reason.map(str::to_owned),
            ..Default::default()
        };
        let events = vec![
            mk("warn", "inspect", "incomplete", None),
            mk("warn", "inspect", "incomplete", None),
            mk("error", "inspect", "failed", Some("source_unavailable")),
            mk("warn", "start", "unavailable", Some("missing_pre")),
        ];
        let summary = summarize(&events);
        assert_eq!(
            summary[0],
            (
                "warn".to_owned(),
                "inspect".to_owned(),
                "incomplete".to_owned(),
                "-".to_owned(),
                2
            )
        );
        assert_eq!(summary.len(), 3);
    }

    #[test]
    fn bounded_detail_caps_to_the_byte_limit_on_a_char_boundary() {
        let text = "é".repeat(100); // 2 bytes per char, 200 bytes total
        let bounded = bounded_detail(&text);
        assert!(bounded.len() <= MAX_DETAIL_BYTES);
        assert!(text.starts_with(bounded));
    }
}
