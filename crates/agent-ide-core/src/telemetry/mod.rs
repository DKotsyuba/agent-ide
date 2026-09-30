//! Closed, local-only usage telemetry with bounded durable retention and reads.
//!
//! Telemetry accepts only the typed vocabulary in this module. It deliberately has no string,
//! JSON, path, command, source, prompt, credential, output, or diagnostic-message field. Failed
//! ingress and persistence are counted locally and never affect the observed coding operation.

use std::{
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

#[cfg(test)]
use std::os::unix::fs::DirBuilderExt;

use rusqlite::{params, types::Value};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[cfg(test)]
use crate::app::config::StoreConfig;
#[cfg(test)]
use crate::app::store::UntrackedOutcome;
use crate::app::store::{
    DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey, Store,
    StoreError, TrustedUpSql,
};

/// Converts existing Assistance, provider, and Execution facts into closed telemetry events.
pub mod adapters;

/// Largest accepted canonical encoded event in bytes.
pub const MAX_EVENT_BYTES: usize = 2 * 1024;
/// Hard upper bound for retained durable telemetry rows.
pub const MAX_ROWS: usize = 100_000;
/// Hard upper bound for retained canonical event bytes.
pub const MAX_LOGICAL_BYTES: usize = 16 * 1024 * 1024;
/// Hard upper bound for one query page, excluding its continuation sentinel row.
pub const MAX_QUERY_ROWS: usize = 1_000;
/// Hard upper bound for canonical UTF-8 export bytes.
pub const MAX_EXPORT_BYTES: usize = 4 * 1024 * 1024;

const TELEMETRY_MIGRATION_SQL: &str = "
    CREATE TABLE IF NOT EXISTS telemetry_events (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
        tag TEXT NOT NULL,
        payload BLOB NOT NULL,
        logical_bytes INTEGER NOT NULL CHECK(logical_bytes > 0)
    );
    CREATE INDEX IF NOT EXISTS telemetry_events_tag_sequence
        ON telemetry_events(tag, sequence);";

/// Identifies one of the eleven public Assistance MCP methods without carrying its arguments.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolMethod {
    /// `ide.start` activation request.
    Start,
    /// `ide.context` bounded context request.
    Context,
    /// `ide.diff` bounded diff request.
    Diff,
    /// `ide.inspect` bounded detail request.
    Inspect,
    /// `ide.stop` binding-stop request.
    Stop,
    /// `ide.edit` bounded single-file edit request.
    Edit,
    /// `ide.outline` file skeleton request.
    Outline,
    /// `ide.read` symbol body request.
    Read,
    /// `ide.symbol` symbol card request.
    Symbol,
    /// `ide.graph` live call graph request.
    Graph,
    /// `ide.test` test run or status request.
    Test,
}

/// Classifies a closed completion result without retaining a peer message or error text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    /// The operation returned its current typed result.
    Completed,
    /// The operation rejected its bounded model-facing parameters.
    Invalid,
    /// A required local boundary was unavailable.
    Unavailable,
    /// The peer completed with a meaningful closed failure that was not invalid input or cancellation.
    Failed,
    /// The result was incomplete or timed out at the bounded observation point.
    Incomplete,
    /// The operation was explicitly stopped or cancelled.
    Cancelled,
    /// The peer answered `pending`: an `ide.inspect` round trip is required. Distinct from
    /// [`ToolOutcome::Incomplete`] because a normal pending/inspect round trip is not itself a
    /// failure (T107).
    Pending,
}

/// Classifies whether a provider cache was used without identifying a cache location or key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    /// The relevant provider path has no cache observation.
    NotApplicable,
    /// The provider used a retained cache entry.
    Hit,
    /// The provider performed work without a retained cache entry.
    Miss,
    /// The bounded provider observation was unavailable.
    Unavailable,
}

/// Classifies a provider diagnostic summary without retaining diagnostics or their messages.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticState {
    /// No diagnostic-state fact was available for this event.
    NotApplicable,
    /// The existing provider path reported no diagnostics.
    Clean,
    /// The existing provider path reported one or more diagnostics.
    Changed,
    /// The bounded provider observation was unavailable.
    Unavailable,
}

/// Classifies a bounded output-size observation without retaining output bytes or stream identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputSizeClass {
    /// No output was retained by the existing execution path.
    Empty,
    /// Retained output was at most 4 KiB.
    Small,
    /// Retained output was from 4 KiB through 64 KiB.
    Medium,
    /// Retained output exceeded 64 KiB without being truncated.
    Large,
    /// Existing output capture discarded trailing bytes at its cap.
    Truncated,
}

/// Classifies an existing Execution admission observation without creating a new measurement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    /// The existing path admitted its owned work.
    Admitted,
    /// The existing path rejected admission before work began.
    Rejected,
    /// The event has no Execution admission fact.
    NotApplicable,
}

/// Classifies an existing cancellation observation without inferring cancellation from exit status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationState {
    /// No cancellation request was observed.
    NotRequested,
    /// The existing path received an explicit cancellation request.
    Requested,
    /// The event has no cancellation fact.
    NotApplicable,
}

/// Classifies existing descendant settlement evidence without asserting process-tree completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DescendantSettlement {
    /// The direct child settled while descendants remain deliberately unverified.
    Unverified,
    /// The event has no descendant-settlement fact.
    NotApplicable,
}

/// Names a closed native fallback boundary without retaining a host payload or failure text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// Native hook submission could not reach an already-running daemon.
    HookUnavailable,
    /// MCP facade routing could not obtain a typed daemon result.
    FacadeUnavailable,
    /// A bounded provider observation was unavailable.
    ProviderUnavailable,
}

/// Represents every accepted local telemetry event; no variant has an open or free-form field.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "tag", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    /// Records an Assistance tool completion using only closed result and summary facts.
    ToolCompleted {
        /// Public method whose arguments are intentionally omitted.
        method: ToolMethod,
        /// Closed result class at the facade boundary.
        outcome: ToolOutcome,
        /// Existing monotonic elapsed duration, saturated to whole milliseconds.
        duration_ms: u32,
        /// Closed language class when an existing provider supplied one.
        language: Option<Language>,
        /// Existing provider cache summary.
        cache: CacheState,
        /// Existing provider diagnostic summary.
        diagnostics: DiagnosticState,
        /// Closed reason code for a non-completed, non-pending outcome (T107). `#[serde(default)]`
        /// keeps durable rows recorded before this field existed readable as `None`.
        #[serde(default)]
        reason: Option<crate::errorlog::ReasonCode>,
    },
    /// Records an existing Execution completion using only already-measured bounded facts.
    ExecutionCompleted {
        /// Existing monotonic elapsed duration, saturated to whole milliseconds.
        duration_ms: u32,
        /// Existing bounded output-size summary.
        output: OutputSizeClass,
        /// Existing admission result.
        admission: AdmissionState,
        /// Existing explicit cancellation observation.
        cancellation: CancellationState,
        /// Existing conservative descendant settlement observation.
        descendants: DescendantSettlement,
    },
    /// Records an existing provider state summary without provider identity, cache key, or message.
    ProviderObserved {
        /// Closed provider language class.
        language: Language,
        /// Existing provider cache summary.
        cache: CacheState,
        /// Existing provider diagnostic summary.
        diagnostics: DiagnosticState,
    },
    /// Records a native fallback observation without changing native fallback behaviour.
    NativeFallback {
        /// Closed unavailable boundary that selected the native path.
        reason: FallbackReason,
    },
    /// Records one completed confined project check with bucketed counts only (EYES-r1 §8).
    ProjectCheckCompleted {
        /// Checked language, recorded as its identifier.
        language: Language,
        /// Closed state of the completed snapshot.
        state: ProjectCheckState,
        /// Checker-measured run duration, saturated to whole milliseconds.
        duration_ms: u32,
        /// Bucketed deduplicated error count.
        errors_bucket: CountBucket,
        /// Bucketed deduplicated warning count.
        warnings_bucket: CountBucket,
    },
    /// Records one name-index refresh that re-read files (a build or an update), language-free.
    NameIndexRefreshed {
        /// Index state after the refresh.
        state: NameIndexState,
        /// Bucketed indexed file count.
        files_bucket: CountBucket,
        /// Bucketed fact count.
        facts_bucket: CountBucket,
        /// Bucketed count of files whose facts came from the cache shared by the worktrees.
        reused_bucket: CountBucket,
        /// Refresh duration, saturated to whole milliseconds.
        duration_ms: u32,
    },
}

/// Classifies a name index after a refresh.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NameIndexState {
    /// A sweep is still running inside its build budget.
    Building,
    /// Every listed candidate was swept.
    Ready,
    /// A bound was hit; counts are lower bounds.
    Partial,
}

/// Classifies a completed project check's state without any path, message, or reason text.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectCheckState {
    /// Complete result for the configured scope.
    Ready,
    /// Result present with incomplete coverage.
    Partial,
    /// No result was available yet.
    Checking,
    /// Project checks were disabled.
    Disabled,
    /// The host could not prove whole-project read access, so the check was not run.
    ReadRestricted,
    /// The worktree was outside every allowed root.
    OutsideRoots,
    /// The configured tool was missing.
    ToolMissing,
    /// The project environment was missing.
    EnvMissing,
    /// The check tool ran but analyzed zero files.
    NoFiles,
    /// The check failed unrecoverably.
    Fatal,
    /// The check exceeded its timeout.
    Timeout,
}

/// Buckets one problem count so telemetry never retains an exact project-specific number.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CountBucket {
    /// Exactly zero.
    #[serde(rename = "0")]
    Zero,
    /// One through nine.
    #[serde(rename = "1-9")]
    Units,
    /// Ten through ninety-nine.
    #[serde(rename = "10-99")]
    Tens,
    /// One hundred or more.
    #[serde(rename = "100+")]
    Hundreds,
}

impl CountBucket {
    /// Returns the bucket containing `count`.
    pub const fn of(count: u32) -> Self {
        match count {
            0 => Self::Zero,
            1..=9 => Self::Units,
            10..=99 => Self::Tens,
            _ => Self::Hundreds,
        }
    }
}

/// Names a registered language without carrying a file path, source, or command; serialized as
/// the language identifier.
pub use crate::lang::Language;

impl Event {
    /// Returns this event's fixed schema tag for durable filtering and canonical export.
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::ToolCompleted { .. } => "tool_completed",
            Self::ExecutionCompleted { .. } => "execution_completed",
            Self::ProviderObserved { .. } => "provider_observed",
            Self::NativeFallback { .. } => "native_fallback",
            Self::ProjectCheckCompleted { .. } => "project_check_completed",
            Self::NameIndexRefreshed { .. } => "name_index_refreshed",
        }
    }

    /// Canonically encodes this closed event and rejects any encoding larger than 2 KiB.
    pub fn encode(&self) -> Result<Vec<u8>, TelemetryError> {
        let encoded = serde_json::to_vec(self).map_err(|_| TelemetryError::InvalidEvent)?;
        (encoded.len() <= MAX_EVENT_BYTES)
            .then_some(encoded)
            .ok_or(TelemetryError::OversizedEvent)
    }
}

/// Supplies restart-only local telemetry limits, each constrained by the contract hard maxima.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryConfig {
    /// Whether this owner accepts events; disabled owners drop every ingress immediately.
    pub enabled: bool,
    /// Maximum queued events awaiting durable insertion.
    pub queue_capacity: usize,
    /// Maximum durable event rows, from one through [`MAX_ROWS`].
    pub max_rows: usize,
    /// Maximum logical canonical event payload bytes, from one through [`MAX_LOGICAL_BYTES`].
    pub max_logical_bytes: usize,
    /// Maximum requested query rows, from one through [`MAX_QUERY_ROWS`].
    pub query_budget: usize,
    /// Maximum export bytes, from one through [`MAX_EXPORT_BYTES`].
    pub export_budget: usize,
}

impl Default for TelemetryConfig {
    /// Returns the immutable production defaults, which use all contract hard maxima.
    fn default() -> Self {
        Self {
            enabled: true,
            queue_capacity: 64,
            max_rows: MAX_ROWS,
            max_logical_bytes: MAX_LOGICAL_BYTES,
            query_budget: MAX_QUERY_ROWS,
            export_budget: MAX_EXPORT_BYTES,
        }
    }
}

impl TelemetryConfig {
    /// Validates restart-only limits before an owner and its worker are created.
    pub fn validate(self) -> Result<Self, TelemetryError> {
        (self.queue_capacity > 0
            && self.max_rows > 0
            && self.max_rows <= MAX_ROWS
            && self.max_logical_bytes > 0
            && self.max_logical_bytes <= MAX_LOGICAL_BYTES
            && self.query_budget > 0
            && self.query_budget <= MAX_QUERY_ROWS
            && self.export_budget > 0
            && self.export_budget <= MAX_EXPORT_BYTES)
            .then_some(self)
            .ok_or(TelemetryError::InvalidConfig)
    }
}

/// Explains an internal telemetry refusal without carrying an operation failure or private content.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TelemetryError {
    /// A restart-only telemetry setting was zero or exceeded its contract hard maximum.
    InvalidConfig,
    /// An event could not be encoded as the closed canonical schema.
    InvalidEvent,
    /// A closed event's canonical encoding exceeded [`MAX_EVENT_BYTES`].
    OversizedEvent,
    /// Application durable storage was unavailable or rejected the bounded request.
    Store,
    /// Another process already owns the stable telemetry database.
    Busy,
    /// The telemetry migration key already names a different immutable schema.
    MigrationIncompatible,
    /// A nonfresh telemetry migration could not obtain its required durable backup.
    MigrationBackupUnavailable,
    /// Telemetry migration commit or caller delivery could not be determined safely.
    MigrationOutcomeUnknown,
}

/// Describes a durable telemetry row in increasing allocated sequence order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryRow {
    /// Durable monotonically increasing row sequence; zero is never valid.
    pub sequence: u64,
    /// Decoded closed event that supplied this row's logical bytes.
    pub event: Event,
}

/// Restricts a query or export to one fixed event tag, or accepts all closed tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Filter {
    /// Selects all closed event variants.
    All,
    /// Selects only one fixed event tag.
    Tag(&'static str),
}

/// Returns one contiguous durable page and honest continuation/drop state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryPage {
    /// Sequence-ordered matching rows, never longer than the requested bounded limit.
    pub rows: Vec<TelemetryRow>,
    /// Exclusive cursor for the next page: the final sequence returned here when another matching row exists.
    pub next_cursor: Option<u64>,
    /// True when a matching row remained beyond this page's limit.
    pub truncated: bool,
    /// Events this writer observed as dropped, or `None` when a read-only owner cannot know.
    pub dropped: Option<u64>,
}

/// Returns canonical newline-delimited UTF-8 export bytes and an explicit first omitted sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Export {
    /// Canonical sequence-ordered event rows, one JSON object plus newline per retained row.
    pub bytes: Vec<u8>,
    /// True when export stopped before its byte budget.
    pub truncated: bool,
    /// Durable sequence of the first matching row omitted by the byte cap.
    pub first_omitted_sequence: Option<u64>,
    /// Events this writer observed as dropped, or `None` when a read-only owner cannot know.
    pub dropped: Option<u64>,
}

/// Retains the telemetry database's exclusive advisory lock for one writer lifetime.
struct TelemetryOwnership {
    /// Open lock-file descriptor whose process lock excludes another stable writer.
    _file: File,
}

impl TelemetryOwnership {
    /// Acquires the validated sibling lock file without following links or waiting.
    ///
    /// The database parent must already be an owner-only real directory. A pre-existing lock must
    /// be a regular owner-only file; contention returns the fail-open busy class.
    fn acquire(database: &Path) -> Result<Self, TelemetryError> {
        let mut lock_path = database.as_os_str().to_os_string();
        lock_path.push(".lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&lock_path)
            .map_err(|_| TelemetryError::Store)?;
        validate_private_file(&file.metadata().map_err(|_| TelemetryError::Store)?)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(Self { _file: file })
        } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            Err(TelemetryError::Busy)
        } else {
            Err(TelemetryError::Store)
        }
    }
}

/// Rejects a path that is not absolute and lexically normalized without parent traversal.
fn absolute_local_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

/// Validates one persistent telemetry directory as a real owner-only directory.
fn validate_private_directory(path: &Path) -> Result<(), TelemetryError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| TelemetryError::Store)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        Err(TelemetryError::Store)
    } else {
        Ok(())
    }
}

/// Validates owner-only regular-file metadata for a database, journal, WAL, shared memory, or lock.
fn validate_private_file(metadata: &fs::Metadata) -> Result<(), TelemetryError> {
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        Err(TelemetryError::Store)
    } else {
        Ok(())
    }
}

/// Returns a SQLite companion path by appending its fixed suffix to the database pathname.
fn sqlite_companion(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// Rejects an existing SQLite state path unless it is a nonsymlink owner-only regular file.
fn validate_private_file_if_present(path: &Path) -> Result<(), TelemetryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TelemetryError::Store),
        Ok(metadata) => validate_private_file(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(TelemetryError::Store),
    }
}

/// Creates or validates the database and every SQLite companion path inside its private directory.
fn prepare_private_database(database: &Path) -> Result<(), TelemetryError> {
    if !absolute_local_path(database) {
        return Err(TelemetryError::Store);
    }
    validate_private_directory(database.parent().ok_or(TelemetryError::Store)?)?;
    for suffix in ["-journal", "-wal", "-shm"] {
        validate_private_file_if_present(&sqlite_companion(database, suffix))?;
    }
    match fs::symlink_metadata(database) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TelemetryError::Store),
        Ok(metadata) => validate_private_file(&metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let file = OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(database)
                .map_err(|_| TelemetryError::Store)?;
            validate_private_file(&file.metadata().map_err(|_| TelemetryError::Store)?)
        }
        Err(_) => Err(TelemetryError::Store),
    }
}

/// Tracks retained aggregates in memory so ordinary inserts never scan the full event table.
#[derive(Clone, Copy)]
struct RetentionState {
    /// Number of rows committed by this exclusive owner.
    rows: i64,
    /// Sum of committed canonical payload bytes.
    logical_bytes: i64,
}

/// Provides fail-open nonblocking ingress plus durable query and export for one restart-only owner.
#[derive(Clone)]
pub struct Telemetry {
    /// Bounded sender to the sole background durable writer; absent when telemetry is disabled.
    sender: Option<mpsc::Sender<Event>>,
    /// Durable Store owner shared only with the background writer and bounded readers.
    store: Arc<Store>,
    /// Immutable limits established at construction and never live-reloaded.
    config: TelemetryConfig,
    /// Counts locally dropped events, or is absent for a read-only owner with unknown history.
    dropped: Option<Arc<AtomicU64>>,
    /// Prevents new ingress once graceful writer drain begins.
    closing: Arc<AtomicBool>,
    /// Signals the background writer to close ingress and drain every already queued event.
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    /// Shared single-use join handle for the background writer.
    writer: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
    /// Exclusive stable ownership shared with the writer until every admitted transaction settles.
    /// Absent for caller-supplied and read-only stores.
    _ownership: Option<Arc<TelemetryOwnership>>,
}

impl Telemetry {
    /// Migrates the closed schema and starts this restart-only owner's nonblocking writer task.
    ///
    /// Opening is an administrative startup operation and may return [`TelemetryError::Store`].
    /// Once returned, [`Self::record`] never waits for SQLite, retries an event, changes a caller's
    /// result, or exposes an ingestion error.
    pub async fn open(store: Arc<Store>, config: TelemetryConfig) -> Result<Self, TelemetryError> {
        Self::open_inner(store, config, None).await
    }

    /// Acquires one stable database writer without waiting, then migrates and starts telemetry.
    ///
    /// `database` must be an absolute normalized child of an existing owner-only `0700` directory.
    /// The database, SQLite companions, backup directory, and sibling advisory lock are created or
    /// validated as nonsymlink owner-only state. Unsafe state returns [`TelemetryError::Store`].
    /// The lock is retained through writer settlement; a competing process gets
    /// [`TelemetryError::Busy`] before SQLite is opened and can disable telemetry without
    /// disturbing the writer or any session-local Workspace authority.
    pub async fn open_database(
        database: &Path,
        config: TelemetryConfig,
    ) -> Result<Self, TelemetryError> {
        prepare_private_database(database)?;
        let ownership = Arc::new(TelemetryOwnership::acquire(database)?);
        let backup_root = database
            .parent()
            .ok_or(TelemetryError::Store)?
            .join("backups");
        let store = Store::open_with_backup_root(
            database,
            &backup_root,
            crate::app::config::EffectiveConfig::defaults().store(),
        )
        .map_err(map_store)?;
        prepare_private_database(database)?;
        Self::open_inner(Arc::new(store), config, Some(ownership)).await
    }

    /// Starts a writer over one already-open store and optional stable ownership guard.
    async fn open_inner(
        store: Arc<Store>,
        config: TelemetryConfig,
        ownership: Option<Arc<TelemetryOwnership>>,
    ) -> Result<Self, TelemetryError> {
        let config = config.validate()?;
        migrate(&store).await?;
        let dropped = Arc::new(AtomicU64::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        if !config.enabled {
            return Ok(Self {
                sender: None,
                store,
                config,
                dropped: Some(dropped),
                closing,
                shutdown: None,
                writer: Arc::new(tokio::sync::Mutex::new(None)),
                _ownership: ownership,
            });
        }
        let mut retention = load_retention(&store).await?;
        let (sender, mut receiver) = mpsc::channel(config.queue_capacity);
        let (shutdown, mut shutdown_receiver) = tokio::sync::watch::channel(false);
        let writer_store = Arc::clone(&store);
        let writer_config = config;
        let writer_dropped = Arc::clone(&dropped);
        let writer_ownership = ownership.clone();
        let writer = tokio::spawn(async move {
            let _ownership = writer_ownership;
            loop {
                tokio::select! {
                    biased;
                    changed = shutdown_receiver.changed() => {
                        if changed.is_err() || *shutdown_receiver.borrow() {
                            receiver.close();
                            while let Some(event) = receiver.recv().await {
                                write_event(
                                    &writer_store,
                                    writer_config,
                                    event,
                                    &mut retention,
                                    &writer_dropped,
                                ).await;
                            }
                            break;
                        }
                    }
                    event = receiver.recv() => match event {
                        Some(event) => write_event(
                            &writer_store,
                            writer_config,
                            event,
                            &mut retention,
                            &writer_dropped,
                        ).await,
                        None => break,
                    }
                }
            }
        });
        Ok(Self {
            sender: Some(sender),
            store,
            config,
            dropped: Some(dropped),
            closing,
            shutdown: Some(shutdown),
            writer: Arc::new(tokio::sync::Mutex::new(Some(writer))),
            _ownership: ownership,
        })
    }

    /// Opens an existing telemetry schema for query/export without migration, a writer, or mutation.
    ///
    /// `store` must be Application's read-only owner. The returned disabled ingress discards any
    /// attempted record and reports drop history as unknown, while reads use `config` budgets.
    pub async fn open_read_only(
        store: Arc<Store>,
        config: TelemetryConfig,
    ) -> Result<Self, TelemetryError> {
        Ok(Self {
            sender: None,
            store,
            config: config.validate()?,
            dropped: None,
            closing: Arc::new(AtomicBool::new(false)),
            shutdown: None,
            writer: Arc::new(tokio::sync::Mutex::new(None)),
            _ownership: None,
        })
    }

    /// Attempts to queue one closed event and drops it immediately on every unavailable condition.
    ///
    /// This method is synchronous and never waits, allocates a retry, alters an originating tool or
    /// native fallback outcome, or returns an error. Invalid and oversized events are dropped just
    /// like a full queue or unavailable durable sink.
    pub fn record(&self, event: Event) {
        let Some(dropped) = &self.dropped else {
            return;
        };
        if self.closing.load(Ordering::Acquire) {
            dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if event.encode().is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(sender) = &self.sender else {
            dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if sender.try_send(event).is_err() {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Stops new ingress, drains every accepted event, and waits for its final durable settlement.
    ///
    /// Clones share one join handle, so repeated shutdown calls are harmless. Read-only and
    /// disabled owners return immediately.
    pub async fn shutdown(&self) {
        self.closing.store(true, Ordering::Release);
        if let Some(shutdown) = &self.shutdown {
            let _ = shutdown.send(true);
        }
        if let Some(writer) = self.writer.lock().await.take() {
            let _ = writer.await;
        }
    }

    /// Reads a contiguous matching sequence page after `cursor`, limited to at most 1,000 rows.
    ///
    /// `cursor` is exclusive and `None` begins at durable sequence zero. The call never mutates
    /// retention or allocates a receipt. A failed store read is reported as [`TelemetryError::Store`]
    /// rather than fabricating a page; callers can distinguish a page boundary via `truncated` and
    /// `next_cursor`.
    pub async fn query(
        &self,
        filter: Filter,
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<QueryPage, TelemetryError> {
        let limit = limit.min(self.config.query_budget).min(MAX_QUERY_ROWS);
        if limit == 0 {
            return Err(TelemetryError::InvalidConfig);
        }
        let rows = read_rows(&self.store, filter, cursor.unwrap_or(0), limit + 1).await?;
        let truncated = rows.len() > limit;
        let mut rows = rows;
        let next_cursor = truncated.then(|| rows[limit - 1].sequence);
        rows.truncate(limit);
        Ok(QueryPage {
            rows,
            next_cursor,
            truncated,
            dropped: self
                .dropped
                .as_ref()
                .map(|dropped| dropped.load(Ordering::Relaxed)),
        })
    }

    /// Renders canonical newline-delimited UTF-8 rows in sequence order up to this owner's export cap.
    ///
    /// Export reads only through Application's bounded trusted multi-row API and has no SQLite
    /// bypass. It stops before the next complete row would exceed the cap, reports that row's
    /// sequence, and never samples, reorders, or changes retention.
    pub async fn export(&self, filter: Filter) -> Result<Export, TelemetryError> {
        let mut cursor = 0;
        let mut bytes = Vec::new();
        loop {
            let rows = read_rows(&self.store, filter, cursor, MAX_QUERY_ROWS).await?;
            if rows.is_empty() {
                return Ok(Export {
                    bytes,
                    truncated: false,
                    first_omitted_sequence: None,
                    dropped: self
                        .dropped
                        .as_ref()
                        .map(|dropped| dropped.load(Ordering::Relaxed)),
                });
            }
            for row in &rows {
                let mut line = row.event.encode()?;
                line.push(b'\n');
                if bytes.len().saturating_add(line.len()) > self.config.export_budget {
                    return Ok(Export {
                        bytes,
                        truncated: true,
                        first_omitted_sequence: Some(row.sequence),
                        dropped: self
                            .dropped
                            .as_ref()
                            .map(|dropped| dropped.load(Ordering::Relaxed)),
                    });
                }
                bytes.extend_from_slice(&line);
                cursor = row.sequence;
            }
            if rows.len() < MAX_QUERY_ROWS {
                return Ok(Export {
                    bytes,
                    truncated: false,
                    first_omitted_sequence: None,
                    dropped: self
                        .dropped
                        .as_ref()
                        .map(|dropped| dropped.load(Ordering::Relaxed)),
                });
            }
        }
    }
}

/// Applies the immutable telemetry schema migration through Application's migration ledger.
async fn migrate(store: &Store) -> Result<(), TelemetryError> {
    let sql = TrustedUpSql::new(TELEMETRY_MIGRATION_SQL).map_err(|_| TelemetryError::Store)?;
    let migration = DomainMigration {
        domain: DomainName::new("telemetry").map_err(|_| TelemetryError::Store)?,
        key: MigrationKey::new("telemetry-v0-2-r1").map_err(|_| TelemetryError::Store)?,
        expected_digest: MigrationDigest::from_sql(&sql),
        up_sql: sql,
    };
    match store.admit_migration(migration).await.map_err(map_store)? {
        MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. } => Ok(()),
        MigrationAdmission::Incompatible { .. } => Err(TelemetryError::MigrationIncompatible),
        MigrationAdmission::BackupUnavailable => Err(TelemetryError::MigrationBackupUnavailable),
        MigrationAdmission::OutcomeUnknown { .. } => Err(TelemetryError::MigrationOutcomeUnknown),
    }
}

/// Reads retained aggregates once when an exclusive telemetry writer starts.
async fn load_retention(store: &Store) -> Result<RetentionState, TelemetryError> {
    store
        .read_one(
            "SELECT COUNT(*), COALESCE(SUM(logical_bytes), 0) FROM telemetry_events",
            Vec::new(),
            |row| {
                Ok(RetentionState {
                    rows: row.get(0)?,
                    logical_bytes: row.get(1)?,
                })
            },
        )
        .await
        .map_err(map_store)?
        .ok_or(TelemetryError::Store)
}

/// Persists one queued event and returns only after its Store transaction actually settles.
async fn write_event(
    store: &Store,
    config: TelemetryConfig,
    event: Event,
    retention: &mut RetentionState,
    dropped: &AtomicU64,
) {
    match persist(store, config, event, *retention).await {
        Ok(next) => *retention = next,
        Err(_) => {
            dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Persists one event and evicts only the oldest rows needed for both ceilings to hold.
async fn persist(
    store: &Store,
    config: TelemetryConfig,
    event: Event,
    retained: RetentionState,
) -> Result<RetentionState, TelemetryError> {
    let payload = event.encode()?;
    let tag = event.tag();
    let logical_bytes = i64::try_from(payload.len()).map_err(|_| TelemetryError::InvalidEvent)?;
    let max_rows = i64::try_from(config.max_rows).map_err(|_| TelemetryError::InvalidConfig)?;
    let max_bytes =
        i64::try_from(config.max_logical_bytes).map_err(|_| TelemetryError::InvalidConfig)?;
    store
        .execute_untracked_settled(move |transaction| {
            transaction.execute(
                "INSERT INTO telemetry_events (tag, payload, logical_bytes) VALUES (?1, ?2, ?3)",
                params![tag, payload, logical_bytes],
            )?;
            let mut next = RetentionState {
                rows: retained
                    .rows
                    .checked_add(1)
                    .ok_or(rusqlite::Error::InvalidQuery)?,
                logical_bytes: retained
                    .logical_bytes
                    .checked_add(logical_bytes)
                    .ok_or(rusqlite::Error::InvalidQuery)?,
            };
            let mut cutoff = None;
            if next.rows > max_rows || next.logical_bytes > max_bytes {
                let mut statement = transaction.prepare(
                    "SELECT sequence, logical_bytes FROM telemetry_events ORDER BY sequence ASC",
                )?;
                let mut rows = statement.query([])?;
                while next.rows > max_rows || next.logical_bytes > max_bytes {
                    let row = rows.next()?.ok_or(rusqlite::Error::InvalidQuery)?;
                    cutoff = Some(row.get::<_, i64>(0)?);
                    next.rows -= 1;
                    next.logical_bytes = next
                        .logical_bytes
                        .checked_sub(row.get::<_, i64>(1)?)
                        .ok_or(rusqlite::Error::InvalidQuery)?;
                }
            }
            if let Some(cutoff) = cutoff {
                transaction.execute(
                    "DELETE FROM telemetry_events WHERE sequence <= ?1",
                    params![cutoff],
                )?;
            }
            Ok(next)
        })
        .await
        .map_err(map_store)
}

/// Reads and validates one bounded durable page through Application's trusted multi-row API.
async fn read_rows(
    store: &Store,
    filter: Filter,
    cursor: u64,
    limit: usize,
) -> Result<Vec<TelemetryRow>, TelemetryError> {
    let cursor = i64::try_from(cursor).map_err(|_| TelemetryError::Store)?;
    let (sql, parameters) = match filter {
        Filter::All => (
            "SELECT sequence, payload FROM telemetry_events WHERE sequence > ?1 ORDER BY sequence ASC",
            vec![Value::Integer(cursor)],
        ),
        Filter::Tag(tag) => (
            "SELECT sequence, payload FROM telemetry_events WHERE sequence > ?1 AND tag = ?2 ORDER BY sequence ASC",
            vec![Value::Integer(cursor), Value::Text(tag.to_owned())],
        ),
    };
    store
        .read_many(sql, parameters, limit, |row| {
            let sequence = row.get::<_, i64>(0)?;
            let payload = row.get::<_, Vec<u8>>(1)?;
            let event =
                serde_json::from_slice(&payload).map_err(|_| rusqlite::Error::InvalidQuery)?;
            let sequence = u64::try_from(sequence).map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok(TelemetryRow { sequence, event })
        })
        .await
        .map_err(map_store)
}

/// Converts every Application failure into the closed telemetry-store unavailable class.
fn map_store(_: StoreError) -> TelemetryError {
    TelemetryError::Store
}

/// Distinguishes temporary telemetry databases created by concurrent focused tests.
#[cfg(test)]
static NEXT_TEST_DATABASE: AtomicU64 = AtomicU64::new(0);

/// Returns Application defaults so focused tests can open a standalone local telemetry store.
#[cfg(test)]
fn test_store_config() -> StoreConfig {
    StoreConfig {
        queue_capacity: 16,
        busy_timeout: std::time::Duration::from_millis(100),
        request_deadline: std::time::Duration::from_secs(1),
        receipt_capacity: 16,
    }
}

/// Opens a fresh temporary telemetry owner for module-local contract tests only.
#[cfg(test)]
pub(crate) async fn open_test_telemetry(
    config: TelemetryConfig,
) -> (Telemetry, std::path::PathBuf) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch in tests")
        .as_nanos();
    let ordinal = NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("agent-ide-telemetry-{nonce}-{ordinal}.sqlite"));
    let store = Arc::new(Store::open(&path, test_store_config()).expect("test store opens"));
    (
        Telemetry::open(store, config)
            .await
            .expect("test telemetry opens"),
        path,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Creates a unique owner-only directory and returns its reserved telemetry database path.
    fn private_database(prefix: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&directory).unwrap();
        directory.join("state.sqlite")
    }

    /// Removes one test database's complete private state directory after all owners are dropped.
    fn remove_private_database(database: &Path) {
        let _ = fs::remove_dir_all(database.parent().unwrap());
    }

    /// Supplies a representative schema-closed event without any user-controlled content field.
    fn event() -> Event {
        Event::ToolCompleted {
            method: ToolMethod::Context,
            outcome: ToolOutcome::Completed,
            duration_ms: 84,
            language: Some(crate::lang::testing::ALPHA),
            cache: CacheState::Hit,
            diagnostics: DiagnosticState::Clean,
            reason: None,
        }
    }

    /// Proves canonical events have no model-facing content fields and unknown JSON fields are refused.
    #[test]
    fn closed_event_schema_rejects_unknown_fields() {
        assert!(serde_json::from_str::<Event>(r#"{"tag":"tool_completed","method":"context","outcome":"completed","duration_ms":1,"language":null,"cache":"hit","diagnostics":"clean","path":"secret"}"#).is_err());
        let encoded = event().encode().unwrap();
        assert!(!String::from_utf8(encoded).unwrap().contains("path"));
    }

    /// A name-index refresh records its state, bucketed counts and duration, and nothing that
    /// names a language, path or name.
    #[test]
    fn name_index_event_encodes_closed_buckets() {
        let event = Event::NameIndexRefreshed {
            state: NameIndexState::Partial,
            files_bucket: CountBucket::of(12),
            facts_bucket: CountBucket::of(5_000),
            reused_bucket: CountBucket::of(3),
            duration_ms: 840,
        };
        assert_eq!(event.tag(), "name_index_refreshed");
        assert_eq!(
            String::from_utf8(event.encode().unwrap()).unwrap(),
            r#"{"tag":"name_index_refreshed","state":"partial","files_bucket":"10-99","facts_bucket":"100+","reused_bucket":"1-9","duration_ms":840}"#
        );
    }

    /// Proves the project check event encodes the EYES-r1 §8 bucket labels under its closed tag.
    #[test]
    fn project_check_event_encodes_closed_buckets() {
        let event = Event::ProjectCheckCompleted {
            language: crate::lang::testing::BETA,
            state: ProjectCheckState::OutsideRoots,
            duration_ms: 7,
            errors_bucket: CountBucket::of(100),
            warnings_bucket: CountBucket::of(9),
        };
        assert_eq!(event.tag(), "project_check_completed");
        assert_eq!(
            String::from_utf8(event.encode().unwrap()).unwrap(),
            r#"{"tag":"project_check_completed","language":"beta","state":"outside_roots","duration_ms":7,"errors_bucket":"100+","warnings_bucket":"1-9"}"#
        );
        for (count, bucket) in [
            (0, CountBucket::Zero),
            (1, CountBucket::Units),
            (10, CountBucket::Tens),
            (99, CountBucket::Tens),
            (u32::MAX, CountBucket::Hundreds),
        ] {
            assert_eq!(CountBucket::of(count), bucket);
        }
    }

    /// Proves smallest configured row ceiling evicts the oldest durable rows in sequence order.
    #[tokio::test]
    async fn retention_evicts_oldest_at_the_first_row_ceiling() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig {
            max_rows: 2,
            ..TelemetryConfig::default()
        })
        .await;
        telemetry.record(event());
        telemetry.record(event());
        telemetry.record(event());
        tokio::time::sleep(Duration::from_millis(30)).await;
        let page = telemetry.query(Filter::All, None, 2).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert!(page.rows[0].sequence > 1);
        let _ = std::fs::remove_file(path);
    }

    /// Proves logical canonical payload bytes independently evict the oldest rows before row capacity.
    #[tokio::test]
    async fn retention_evicts_oldest_at_the_logical_byte_ceiling() {
        let one_event_bytes = event().encode().unwrap().len();
        let (telemetry, path) = open_test_telemetry(TelemetryConfig {
            max_logical_bytes: one_event_bytes * 2,
            ..TelemetryConfig::default()
        })
        .await;
        telemetry.record(event());
        telemetry.record(event());
        telemetry.record(event());
        tokio::time::sleep(Duration::from_millis(30)).await;
        let page = telemetry.query(Filter::All, None, 3).await.unwrap();
        assert_eq!(page.rows.len(), 2);
        assert!(page.rows[0].sequence > 1);
        let _ = std::fs::remove_file(path);
    }

    /// Proves page continuation is sequence ordered and tells callers where the next page begins.
    #[tokio::test]
    async fn query_exposes_contiguous_sequence_cursor() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig::default()).await;
        telemetry.record(event());
        telemetry.record(Event::NativeFallback {
            reason: FallbackReason::HookUnavailable,
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let first = telemetry.query(Filter::All, None, 1).await.unwrap();
        assert!(first.truncated);
        let second = telemetry
            .query(Filter::All, first.next_cursor, 1)
            .await
            .unwrap();
        assert_eq!(second.rows.len(), 1);
        assert!(second.rows[0].sequence > first.rows[0].sequence);
        let _ = std::fs::remove_file(path);
    }

    /// Proves a deliberately small export ceiling reports its first omitted sequence rather than sampling.
    #[tokio::test]
    async fn export_reports_explicit_byte_truncation() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig {
            export_budget: 1,
            ..TelemetryConfig::default()
        })
        .await;
        telemetry.record(event());
        tokio::time::sleep(Duration::from_millis(30)).await;
        let export = telemetry.export(Filter::All).await.unwrap();
        assert!(export.truncated);
        assert_eq!(export.first_omitted_sequence, Some(1));
        assert!(export.bytes.is_empty());
        let _ = std::fs::remove_file(path);
    }

    /// Proves a reopened owner reads prior durable rows in sequence order after its writer stops.
    #[tokio::test]
    async fn reopen_preserves_durable_sequence_order() {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-reopen-{}.sqlite",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = Arc::new(Store::open(&path, test_store_config()).unwrap());
        let telemetry = Telemetry::open(Arc::clone(&store), TelemetryConfig::default())
            .await
            .unwrap();
        telemetry.record(event());
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(telemetry);
        drop(store);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let reopened_store = Arc::new(Store::open(&path, test_store_config()).unwrap());
        let reopened = Telemetry::open(reopened_store, TelemetryConfig::default())
            .await
            .unwrap();
        let rows = reopened.query(Filter::All, None, 1).await.unwrap().rows;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sequence, 1);
        let _ = std::fs::remove_file(path);
    }

    /// Proves an immediate graceful stop commits the final accepted event before returning.
    #[tokio::test]
    async fn shutdown_drains_the_final_queued_event() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig::default()).await;
        telemetry.record(event());
        telemetry.shutdown().await;
        let store = Arc::new(Store::open_read_only(&path, test_store_config()).unwrap());
        let reader = Telemetry::open_read_only(store, TelemetryConfig::default())
            .await
            .unwrap();
        let page = reader.query(Filter::All, None, 1).await.unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.dropped, None);
        let _ = std::fs::remove_file(path);
    }

    /// Keeps ownership through a contended background settlement so no second writer overlaps it.
    #[tokio::test]
    async fn stable_database_ownership_outlives_the_last_ingress_handle() {
        let path = private_database("agent-ide-telemetry-owner");
        let first = Telemetry::open_database(&path, TelemetryConfig::default())
            .await
            .unwrap();
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
        first.record(event());
        tokio::task::yield_now().await;
        drop(first);
        assert!(matches!(
            Telemetry::open_database(&path, TelemetryConfig::default()).await,
            Err(TelemetryError::Busy)
        ));
        drop(blocker);
        let reopened = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match Telemetry::open_database(&path, TelemetryConfig::default()).await {
                    Ok(owner) => break owner,
                    Err(TelemetryError::Busy) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected telemetry reopen failure: {error:?}"),
                }
            }
        })
        .await
        .expect("settled writer releases ownership");
        assert_eq!(
            reopened
                .query(Filter::All, None, 1)
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        reopened.shutdown().await;
        drop(reopened);
        remove_private_database(&path);
    }

    /// Rejects preplaced database symlinks and world-readable state, then creates private files.
    #[tokio::test]
    async fn stable_database_requires_private_nonsymlink_state() {
        let path = private_database("agent-ide-telemetry-private");
        let target = path.parent().unwrap().join("target.sqlite");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(matches!(
            Telemetry::open_database(&path, TelemetryConfig::default()).await,
            Err(TelemetryError::Store)
        ));
        assert!(!target.exists());
        fs::remove_file(&path).unwrap();
        fs::write(&path, []).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            Telemetry::open_database(&path, TelemetryConfig::default()).await,
            Err(TelemetryError::Store)
        ));
        fs::remove_file(&path).unwrap();
        let telemetry = Telemetry::open_database(&path, TelemetryConfig::default())
            .await
            .unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let lock = sqlite_companion(&path, ".lock");
        assert_eq!(
            fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(path.parent().unwrap().join("backups"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        telemetry.shutdown().await;
        drop(telemetry);
        remove_private_database(&path);
    }

    /// Proves incompatible migration admission is not mistaken for an initialized telemetry schema.
    #[tokio::test]
    async fn incompatible_migration_is_propagated() {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-migration-{}-{}.sqlite",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Arc::new(Store::open(&path, test_store_config()).unwrap());
        let sql = TrustedUpSql::new("CREATE TABLE incompatible(value INTEGER);").unwrap();
        assert!(matches!(
            store
                .admit_migration(DomainMigration {
                    domain: DomainName::new("telemetry").unwrap(),
                    key: MigrationKey::new("telemetry-v0-2-r1").unwrap(),
                    expected_digest: MigrationDigest::from_sql(&sql),
                    up_sql: sql,
                })
                .await
                .unwrap(),
            MigrationAdmission::Applied { .. }
        ));
        assert!(matches!(
            Telemetry::open(store, TelemetryConfig::default()).await,
            Err(TelemetryError::MigrationIncompatible)
        ));
        let _ = std::fs::remove_file(path);
    }

    /// Proves a nonfresh database without an approved backup root cannot masquerade as migrated.
    #[tokio::test]
    async fn backup_unavailable_migration_is_propagated() {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-backup-{}-{}.sqlite",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Arc::new(Store::open(&path, test_store_config()).unwrap());
        assert!(matches!(
            store
                .execute_untracked(|transaction| {
                    transaction.execute_batch("CREATE TABLE prior_domain(value INTEGER);")
                })
                .await
                .unwrap(),
            UntrackedOutcome::Committed(())
        ));
        assert!(matches!(
            Telemetry::open(store, TelemetryConfig::default()).await,
            Err(TelemetryError::MigrationBackupUnavailable)
        ));
        let _ = std::fs::remove_file(path);
    }

    /// Proves accepted migration timeout remains unknown instead of authorizing a telemetry owner.
    #[tokio::test]
    async fn outcome_unknown_migration_is_propagated() {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-unknown-{}-{}.sqlite",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let config = StoreConfig {
            request_deadline: Duration::from_millis(10),
            ..test_store_config()
        };
        let store = Arc::new(Store::open(&path, config).unwrap());
        let blocker_store = Arc::clone(&store);
        let blocker = tokio::spawn(async move {
            blocker_store
                .execute_untracked(|_| {
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(())
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(matches!(
            Telemetry::open(store, TelemetryConfig::default()).await,
            Err(TelemetryError::MigrationOutcomeUnknown)
        ));
        assert!(matches!(
            blocker.await.unwrap().unwrap(),
            UntrackedOutcome::OutcomeUnknown
        ));
        let _ = std::fs::remove_file(path);
    }

    /// Proves immediate shutdown waits through contention for the final accepted Store settlement.
    #[tokio::test]
    async fn immediate_shutdown_settles_the_final_accepted_event_under_contention() {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-timeout-{}-{}.sqlite",
            std::process::id(),
            NEXT_TEST_DATABASE.fetch_add(1, Ordering::Relaxed)
        ));
        let config = StoreConfig {
            request_deadline: Duration::from_millis(10),
            ..test_store_config()
        };
        let store = Arc::new(Store::open(&path, config).unwrap());
        let telemetry = Telemetry::open(Arc::clone(&store), TelemetryConfig::default())
            .await
            .unwrap();
        let blocker = tokio::spawn(async move {
            store
                .execute_untracked(|_| {
                    std::thread::sleep(Duration::from_millis(100));
                    Ok(())
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        telemetry.record(event());
        telemetry.shutdown().await;
        assert!(matches!(
            blocker.await.unwrap().unwrap(),
            UntrackedOutcome::OutcomeUnknown
        ));
        let page = telemetry.query(Filter::All, None, 1).await.unwrap();
        assert_eq!(page.rows.len(), 1);
        assert_eq!(page.dropped, Some(0));
        let _ = std::fs::remove_file(path);
    }

    /// Proves a disabled or unavailable telemetry owner drops ingress without returning an error.
    #[tokio::test]
    async fn disabled_sink_is_fail_open() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig {
            enabled: false,
            ..TelemetryConfig::default()
        })
        .await;
        telemetry.record(event());
        let page = telemetry.query(Filter::All, None, 1).await.unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(page.dropped, Some(1));
        let _ = std::fs::remove_file(path);
    }

    /// Proves a full ingress queue drops observations synchronously without delaying the producer.
    #[tokio::test]
    async fn full_sink_is_fail_open() {
        let (telemetry, path) = open_test_telemetry(TelemetryConfig {
            queue_capacity: 1,
            ..TelemetryConfig::default()
        })
        .await;
        for _ in 0..8 {
            telemetry.record(event());
        }
        assert!(telemetry.dropped.as_ref().unwrap().load(Ordering::Relaxed) >= 7);
        let _ = std::fs::remove_file(path);
    }
}
