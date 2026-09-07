//! Bounded, durable SQLite execution mechanics without domain schemas or transition policy.

use std::fmt::{self, Display};
use std::fs::{self, File};
use std::io::Read;
use std::num::NonZeroU64;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;

use rusqlite::{
    Connection, ErrorCode, MAIN_DB, OptionalExtension, Transaction, TransactionBehavior, params,
};
use tokio::sync::oneshot;

use super::config::StoreConfig;

const MAX_DOMAIN_NAME_BYTES: usize = 64;
const MAX_MIGRATION_KEY_BYTES: usize = 128;
const MAX_MIGRATION_SQL_BYTES: usize = 1024 * 1024;

/// Names a validated domain whose migrations share one monotonic version sequence.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DomainName(String);

impl DomainName {
    /// Validates a lowercase ASCII domain name used only as an Application migration namespace.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= MAX_DOMAIN_NAME_BYTES
            && value.bytes().enumerate().all(|(index, byte)| match byte {
                b'a'..=b'z' => true,
                b'0'..=b'9' | b'_' => index > 0,
                _ => false,
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(StoreError::InvalidMigration)
        }
    }

    /// Returns the validated stable namespace string for ledger lookup.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identifies one immutable migration within a domain across retries and binary upgrades.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MigrationKey(String);

impl MigrationKey {
    /// Validates an opaque, nonempty migration identity up to 128 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_MIGRATION_KEY_BYTES {
            return Err(StoreError::InvalidMigration);
        }
        Ok(Self(value))
    }

    /// Returns the opaque stable identity used only as a ledger key, never as a file path.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Carries domain-reviewed, transaction-compatible SQL for one upward schema migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedUpSql(String);

impl TrustedUpSql {
    /// Validates a nonempty bounded SQL program without interpreting its domain semantics.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_MIGRATION_SQL_BYTES {
            return Err(StoreError::InvalidMigration);
        }
        Ok(Self(value))
    }

    /// Returns SQL supplied by the trusted domain owner for execution inside one SQLite transaction.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Represents the immutable BLAKE3 digest of one trusted upward SQL program or backup file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MigrationDigest([u8; 32]);

impl MigrationDigest {
    /// Computes the required digest from the exact UTF-8 bytes of trusted migration SQL.
    pub fn from_sql(sql: &TrustedUpSql) -> Self {
        Self(*blake3::hash(sql.as_str().as_bytes()).as_bytes())
    }

    /// Returns the digest bytes for immutable SQLite-ledger comparison.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Describes one exact, trusted migration admission request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainMigration {
    /// Namespace whose migration version sequence this request extends.
    pub domain: DomainName,
    /// Stable migration identity that prevents a changed body from being applied under the same key.
    pub key: MigrationKey,
    /// Domain-reviewed SQL to execute transactionally if admission succeeds.
    pub up_sql: TrustedUpSql,
    /// Digest the caller expects Application to recompute and verify before any backup or SQL.
    pub expected_digest: MigrationDigest,
}

/// References one fsynced Application-owned SQLite backup made before a nonfresh upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRef {
    /// Relative filename beneath the Store's validated backup root.
    pub relative_path: String,
    /// BLAKE3 digest of the completed backup file after file and directory sync.
    pub digest: MigrationDigest,
}

/// Reports an idempotent migration admission without inventing domain semantic success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationAdmission {
    /// Trusted SQL and its immutable ledger row committed; nonfresh upgrades include their backup.
    Applied {
        /// Application-allocated next version within this domain.
        version: NonZeroU64,
        /// Verified immutable trusted-SQL digest.
        digest: MigrationDigest,
        /// Fsynced backup reference for nonfresh databases, absent only for fresh initialization.
        backup: Option<BackupRef>,
    },
    /// The same immutable key and digest already committed without rerunning SQL.
    AlreadyApplied {
        /// Existing Application-allocated version.
        version: NonZeroU64,
        /// Existing matching trusted-SQL digest.
        digest: MigrationDigest,
    },
    /// This migration key already names a different immutable trusted-SQL body.
    Incompatible {
        /// Existing Application-allocated version for the conflicting key.
        version: NonZeroU64,
        /// Existing immutable digest that differs from the request.
        existing_digest: MigrationDigest,
    },
    /// Commit or caller delivery is ambiguous and must be reconciled by the same key.
    OutcomeUnknown {
        /// Stable migration key whose ledger state must be inspected before any new migration.
        key: MigrationKey,
    },
    /// No durable safe backup was available, so Application ran no trusted SQL.
    BackupUnavailable,
}

/// Identifies one caller-provided operation across submit, timeout reconciliation, and restart recovery.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationId(String);

impl OperationId {
    /// Validates and stores an opaque, nonempty operation identifier up to 128 UTF-8 bytes.
    ///
    /// The ID carries no authority. A caller retains it to reconcile an accepted timeout and uses a
    /// fresh one only for an intentionally new effect.
    pub fn new(value: impl Into<String>) -> Result<Self, StoreError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(StoreError::InvalidOperationId);
        }
        Ok(Self(value))
    }

    /// Returns the opaque identifier for a domain receipt correlation request.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Records the conservative durable state known for an accepted store operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// Receipt admission is durable, but the owner has not begun its SQL transaction.
    Queued,
    /// A transaction began; its final state is not yet durably known.
    Started,
    /// Domain SQL and its matching receipt state committed atomically.
    Committed,
    /// Domain SQL rolled back and the owner recorded confirmed rollback.
    RolledBack,
    /// SQLite's configured busy wait expired before the domain transaction began; returned per call.
    Busy,
    /// Crash, timeout, missing, or corrupt receipt evidence prevents a safe effect claim.
    OutcomeUnknown,
}

impl StoreOutcome {
    /// Decodes one stable mechanics receipt string, treating unexpected data as unknown.
    fn parse(value: &str) -> Self {
        match value {
            "queued" => Self::Queued,
            "started" => Self::Started,
            "committed" => Self::Committed,
            "rolled_back" => Self::RolledBack,
            "busy" => Self::Busy,
            "outcome_unknown" => Self::OutcomeUnknown,
            _ => Self::OutcomeUnknown,
        }
    }

    /// Returns the stable mechanics receipt string persisted by this module.
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Started => "started",
            Self::Committed => "committed",
            Self::RolledBack => "rolled_back",
            Self::Busy => "busy",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// Explains a safe refusal, indeterminate outcome, or SQLite mechanics failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The supplied migration name, key, SQL body, or expected digest is invalid.
    InvalidMigration,
    /// The caller's expected migration digest differs from the exact trusted SQL bytes.
    MigrationDigestMismatch,
    /// Trusted migration SQL failed and its enclosing transaction rolled back.
    MigrationRolledBack,
    /// The supplied direct store configuration contains a zero capacity or deadline.
    InvalidConfig,
    /// The caller supplied an empty or oversized operation identifier.
    InvalidOperationId,
    /// The bounded owner-thread submission channel is full before this operation was accepted.
    QueueFull,
    /// The owner thread stopped before it could accept or answer the requested operation.
    Unavailable,
    /// The caller's wait expired after accepted work; use `outcome` instead of resubmitting it.
    OutcomeUnknown {
        /// The stable operation ID that must be reconciled.
        operation: OperationId,
    },
    /// The supplied operation identifier already has a durable receipt and its closure was not run.
    DuplicateOperation {
        /// The existing durable outcome that must be reconciled instead of replayed.
        existing: StoreOutcome,
    },
    /// Durable mechanics receipts reached their hard bounded capacity, so no SQL was started.
    ReceiptCapacityExhausted,
    /// SQLite's configured busy wait expired without starting the domain transaction.
    Busy,
    /// The owner confirmed transaction rollback after the domain SQL closure returned an error.
    RolledBack,
    /// SQLite or owner-thread setup failed; this never asserts a domain effect outcome.
    Infrastructure(String),
}

impl Display for StoreError {
    /// Formats a compact mechanics failure without inventing domain effect status.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMigration => formatter.write_str("invalid migration admission"),
            Self::MigrationDigestMismatch => formatter.write_str("migration digest mismatch"),
            Self::MigrationRolledBack => formatter.write_str("migration transaction rolled back"),
            Self::InvalidConfig => formatter.write_str("invalid store configuration"),
            Self::InvalidOperationId => formatter.write_str("invalid operation identifier"),
            Self::QueueFull => formatter.write_str("store queue is full"),
            Self::Unavailable => formatter.write_str("store owner is unavailable"),
            Self::OutcomeUnknown { operation } => {
                write!(
                    formatter,
                    "store outcome unknown for {}",
                    operation.as_str()
                )
            }
            Self::DuplicateOperation { existing } => {
                write!(formatter, "operation already exists with {existing:?}")
            }
            Self::ReceiptCapacityExhausted => {
                formatter.write_str("store receipt capacity is exhausted")
            }
            Self::Busy => formatter.write_str("SQLite is busy"),
            Self::RolledBack => formatter.write_str("store transaction rolled back"),
            Self::Infrastructure(message) => {
                write!(formatter, "store infrastructure failed: {message}")
            }
        }
    }
}

impl std::error::Error for StoreError {}

/// Owns a bounded submission channel and one dedicated SQLite connection thread.
///
/// Domain modules own their SQL, tables, and semantic transitions. This type owns only the
/// connection, transaction boundary, durable mechanics receipt, busy policy, and conservative
/// recovery status. It is not a generic domain repository or query framework.
pub struct Store {
    sender: SyncSender<StoreMessage>,
    config: StoreConfig,
}

impl Store {
    /// Opens `database_path` on one dedicated owner thread and initializes Application receipt ledgers.
    ///
    /// The owner configures bundled SQLite with WAL and the configured busy timeout, then converts
    /// interrupted `Queued`/`Started` receipts to `OutcomeUnknown`. It creates only Application
    /// operation and migration ledgers, never a domain table or external-work inference.
    pub fn open(database_path: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        Self::open_inner(database_path, None, config)
    }

    /// Opens a Store that may create durable SQLite backups before nonfresh migration upgrades.
    ///
    /// `backup_root` is created or validated as one private real directory owned by this user. Its
    /// path is never derived from a migration key or domain name. The ordinary [`Self::open`] API
    /// remains valid for non-migration work, but such a Store returns `BackupUnavailable` before a
    /// nonfresh migration instead of attempting any SQL.
    pub fn open_with_backup_root(
        database_path: &Path,
        backup_root: &Path,
        config: StoreConfig,
    ) -> Result<Self, StoreError> {
        let backup_root = prepare_backup_root(backup_root)?;
        Self::open_inner(database_path, Some(backup_root), config)
    }

    /// Starts the owner thread after all public Store construction variants have validated inputs.
    fn open_inner(
        database_path: &Path,
        backup_root: Option<PathBuf>,
        config: StoreConfig,
    ) -> Result<Self, StoreError> {
        validate_store_config(config)?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let database_path = database_path.to_path_buf();
        thread::Builder::new()
            .name("agent-ide-sqlite".to_owned())
            .spawn(move || owner_thread(database_path, backup_root, config, receiver, ready_sender))
            .map_err(|error| StoreError::Infrastructure(error.to_string()))?;
        ready_receiver
            .recv()
            .map_err(|_| StoreError::Unavailable)??;
        Ok(Self { sender, config })
    }

    /// Admits one immutable trusted migration, allocating its next domain version only on success.
    ///
    /// Application recomputes the SQL digest before accepting the request. Matching committed
    /// migrations return `AlreadyApplied`; a changed body under the same key returns
    /// `Incompatible`; a caller timeout returns `OutcomeUnknown` and never authorizes a replay.
    pub async fn admit_migration(
        &self,
        migration: DomainMigration,
    ) -> Result<MigrationAdmission, StoreError> {
        if MigrationDigest::from_sql(&migration.up_sql) != migration.expected_digest {
            return Err(StoreError::MigrationDigestMismatch);
        }
        let key = migration.key.clone();
        let (reply_sender, reply_receiver) = oneshot::channel();
        match self.sender.try_send(StoreMessage::Migrate {
            migration,
            reply: reply_sender,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, reply_receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Ok(MigrationAdmission::OutcomeUnknown { key }),
        }
    }

    /// Executes typed domain SQL inside an Application-owned transaction with a durable operation receipt.
    ///
    /// `sql` receives the live [`Transaction`] and must not manually begin, commit, or roll back a
    /// top-level transaction. Its value is delivered only after a commit that atomically includes the
    /// `Committed` receipt. A duplicate operation returns its prior state without calling `sql`. After
    /// an accepted timeout, callers must use [`Self::outcome`] and never resubmit this operation ID.
    pub async fn execute<T, F>(&self, operation: OperationId, sql: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: for<'transaction> FnOnce(&Transaction<'transaction>) -> rusqlite::Result<T>
            + Send
            + 'static,
    {
        let (reply_sender, reply_receiver) = oneshot::channel();
        let message = StoreMessage::Execute {
            operation: operation.clone(),
            job: Box::new(TypedJob {
                sql,
                reply: reply_sender,
            }),
        };
        match self.sender.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, reply_receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(StoreError::OutcomeUnknown { operation }),
        }
    }

    /// Looks up the durable mechanics receipt without replaying domain SQL.
    ///
    /// Missing, interrupted, or corrupt receipts return `OutcomeUnknown`; that condition does not
    /// authorize a retry. Lookup is bounded by the same owner queue and request deadline as execute.
    pub async fn outcome(&self, operation: OperationId) -> Result<StoreOutcome, StoreError> {
        let (reply_sender, reply_receiver) = oneshot::channel();
        match self.sender.try_send(StoreMessage::Lookup {
            operation: operation.clone(),
            reply: reply_sender,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, reply_receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(StoreError::OutcomeUnknown { operation }),
        }
    }
}

/// Rejects direct store startup settings that would make a bounded owner-thread contract impossible.
fn validate_store_config(config: StoreConfig) -> Result<(), StoreError> {
    if config.queue_capacity == 0
        || config.receipt_capacity == 0
        || config.busy_timeout.is_zero()
        || config.request_deadline.is_zero()
    {
        Err(StoreError::InvalidConfig)
    } else {
        Ok(())
    }
}

/// Creates or validates the private final directory used only for Store-created backup files.
fn prepare_backup_root(path: &Path) -> Result<PathBuf, StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_backup_root(&metadata)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder
                .mode(0o700)
                .create(path)
                .map_err(|error| StoreError::Infrastructure(error.to_string()))?;
            validate_private_backup_root(
                &fs::symlink_metadata(path)
                    .map_err(|error| StoreError::Infrastructure(error.to_string()))?,
            )?;
        }
        Err(error) => return Err(StoreError::Infrastructure(error.to_string())),
    }
    Ok(path.to_path_buf())
}

/// Rejects a backup root that is not a private real directory owned by this effective user.
fn validate_private_backup_root(metadata: &fs::Metadata) -> Result<(), StoreError> {
    let uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
    {
        Err(StoreError::Infrastructure(
            "unsafe migration backup root".to_owned(),
        ))
    } else {
        Ok(())
    }
}

/// Erases a typed transaction closure while preserving its lifetime and deferred caller reply.
trait StoreJob: Send {
    /// Runs domain SQL inside `transaction` and retains its value until commit is settled.
    fn run(self: Box<Self>, transaction: &Transaction<'_>) -> JobRun;

    /// Delivers an admission refusal without running the domain closure.
    fn fail(self: Box<Self>, error: StoreError);
}

/// Holds a typed domain value until Application knows whether its transaction committed.
trait JobCompletion: Send {
    /// Delivers a committed value or a conservative error exactly once.
    fn finish(self: Box<Self>, result: Result<StoreOutcome, StoreError>);
}

/// Distinguishes a closure that produced a value from one that requires rollback.
enum JobRun {
    /// The SQL closure produced a value that can be sent only after commit.
    Success(Box<dyn JobCompletion>),
    /// The SQL closure failed, so rollback is required before its caller receives an error.
    Failure(Box<dyn JobCompletion>),
}

/// Couples one generic transaction closure to its typed one-shot caller response.
struct TypedJob<T, F> {
    sql: F,
    reply: oneshot::Sender<Result<T, StoreError>>,
}

impl<T, F> StoreJob for TypedJob<T, F>
where
    T: Send + 'static,
    F: for<'transaction> FnOnce(&Transaction<'transaction>) -> rusqlite::Result<T> + Send + 'static,
{
    /// Runs the typed closure and defers delivery of its value until Application commits it.
    fn run(self: Box<Self>, transaction: &Transaction<'_>) -> JobRun {
        let Self { sql, reply } = *self;
        match sql(transaction) {
            Ok(value) => JobRun::Success(Box::new(TypedCompletion {
                value: Some(value),
                reply,
            })),
            Err(_) => JobRun::Failure(Box::new(TypedCompletion { value: None, reply })),
        }
    }

    /// Returns an admission failure without invoking the typed SQL closure.
    fn fail(self: Box<Self>, error: StoreError) {
        let _ = self.reply.send(Err(error));
    }
}

/// Delivers a deferred typed SQL result after Application settles transaction durability.
struct TypedCompletion<T> {
    value: Option<T>,
    reply: oneshot::Sender<Result<T, StoreError>>,
}

impl<T> JobCompletion for TypedCompletion<T>
where
    T: Send + 'static,
{
    /// Returns the stored value only after commit and maps every other settlement to the given error.
    fn finish(self: Box<Self>, result: Result<StoreOutcome, StoreError>) {
        let Self { value, reply } = *self;
        let reply_value = match (value, result) {
            (Some(value), Ok(StoreOutcome::Committed)) => Ok(value),
            (_, Ok(_)) => Err(StoreError::Infrastructure(
                "invalid Application transaction settlement".to_owned(),
            )),
            (_, Err(error)) => Err(error),
        };
        let _ = reply.send(reply_value);
    }
}

/// Carries one bounded owner-thread execution or read-only mechanics lookup.
enum StoreMessage {
    /// Attempts exactly one operation receipt admission and transaction execution.
    Execute {
        /// Caller-supplied stable ID used to suppress duplicate execution.
        operation: OperationId,
        /// Domain SQL run only after receipt admission inside the Application-owned transaction.
        job: Box<dyn StoreJob>,
    },
    /// Returns the durable mechanics state for an already named operation.
    Lookup {
        /// Caller-supplied stable ID to reconcile without executing SQL.
        operation: OperationId,
        /// Returns the durable outcome or an honest unknown result.
        reply: oneshot::Sender<Result<StoreOutcome, StoreError>>,
    },
    /// Allocates and applies one trusted domain migration with idempotent ledger admission.
    Migrate {
        /// Trusted immutable migration request supplied by the owning domain.
        migration: DomainMigration,
        /// Returns only the bounded migration admission result.
        reply: oneshot::Sender<Result<MigrationAdmission, StoreError>>,
    },
}

/// Opens SQLite once, applies Application mechanics policy, and serially owns all messages.
fn owner_thread(
    database_path: std::path::PathBuf,
    backup_root: Option<PathBuf>,
    config: StoreConfig,
    receiver: mpsc::Receiver<StoreMessage>,
    ready_sender: SyncSender<Result<(), StoreError>>,
) {
    let mut connection = match open_connection(&database_path, config) {
        Ok(connection) => {
            let _ = ready_sender.send(Ok(()));
            connection
        }
        Err(error) => {
            let _ = ready_sender.send(Err(error));
            return;
        }
    };
    while let Ok(message) = receiver.recv() {
        match message {
            StoreMessage::Execute { operation, job } => {
                execute_one(&mut connection, config, operation, job);
            }
            StoreMessage::Lookup { operation, reply } => {
                let _ = reply.send(read_outcome(&connection, &operation));
            }
            StoreMessage::Migrate { migration, reply } => {
                let _ = reply.send(admit_one_migration(
                    &mut connection,
                    backup_root.as_deref(),
                    migration,
                ));
            }
        }
    }
}

/// Opens bundled SQLite and initializes bounded, durable Application operation and migration ledgers.
fn open_connection(database_path: &Path, config: StoreConfig) -> Result<Connection, StoreError> {
    let connection = Connection::open(database_path).map_err(infrastructure)?;
    connection
        .busy_timeout(config.busy_timeout)
        .map_err(infrastructure)?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS application_operation_receipts (
                 operation_id TEXT PRIMARY KEY NOT NULL,
                 outcome TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS application_domain_migrations (
                 domain TEXT NOT NULL,
                 migration_key TEXT NOT NULL,
                 version INTEGER NOT NULL,
                 digest BLOB NOT NULL,
                 backup_relative_path TEXT,
                 backup_digest BLOB,
                 PRIMARY KEY (domain, migration_key),
                 UNIQUE (domain, version),
                 UNIQUE (domain, digest)
             );
             UPDATE application_operation_receipts
                 SET outcome = 'outcome_unknown'
                 WHERE outcome IN ('queued', 'started');",
        )
        .map_err(infrastructure)?;
    Ok(connection)
}

/// Applies one immutable migration or reports its existing admission without rerunning trusted SQL.
fn admit_one_migration(
    connection: &mut Connection,
    backup_root: Option<&Path>,
    migration: DomainMigration,
) -> Result<MigrationAdmission, StoreError> {
    let digest = MigrationDigest::from_sql(&migration.up_sql);
    if digest != migration.expected_digest {
        return Err(StoreError::MigrationDigestMismatch);
    }
    if let Some((version, existing_digest)) = migration_by_key(connection, &migration)? {
        return Ok(if existing_digest == digest {
            MigrationAdmission::AlreadyApplied { version, digest }
        } else {
            MigrationAdmission::Incompatible {
                version,
                existing_digest,
            }
        });
    }
    if let Some(version) = migration_by_digest(connection, &migration.domain, digest)? {
        return Ok(MigrationAdmission::AlreadyApplied { version, digest });
    }
    let version = next_migration_version(connection, &migration.domain)?;
    let fresh = database_is_fresh_for_domain(connection, &migration.domain)?;
    let backup = if fresh {
        None
    } else {
        let Some(backup_root) = backup_root else {
            return Ok(MigrationAdmission::BackupUnavailable);
        };
        match create_backup(connection, backup_root, &migration, version) {
            Ok(backup) => Some(backup),
            Err(()) => return Ok(MigrationAdmission::BackupUnavailable),
        }
    };
    let transaction = match begin_transaction(connection) {
        Ok(transaction) => transaction,
        Err(_) => {
            return Ok(MigrationAdmission::OutcomeUnknown { key: migration.key });
        }
    };
    if transaction
        .execute_batch(migration.up_sql.as_str())
        .is_err()
    {
        if transaction.rollback().is_ok() {
            return Err(StoreError::MigrationRolledBack);
        }
        return Ok(MigrationAdmission::OutcomeUnknown { key: migration.key });
    }
    let backup_path = backup.as_ref().map(|backup| backup.relative_path.as_str());
    let backup_digest = backup
        .as_ref()
        .map(|backup| backup.digest.as_bytes().as_slice());
    if transaction
        .execute(
            "INSERT INTO application_domain_migrations
                 (domain, migration_key, version, digest, backup_relative_path, backup_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                migration.domain.as_str(),
                migration.key.as_str(),
                version.get(),
                digest.as_bytes().as_slice(),
                backup_path,
                backup_digest,
            ],
        )
        .is_err()
    {
        return Ok(MigrationAdmission::OutcomeUnknown { key: migration.key });
    }
    match transaction.commit() {
        Ok(()) => Ok(MigrationAdmission::Applied {
            version,
            digest,
            backup,
        }),
        Err(_) => Ok(MigrationAdmission::OutcomeUnknown { key: migration.key }),
    }
}

/// Reads an immutable migration-key mapping, treating corrupt stored digest data as unsafe.
fn migration_by_key(
    connection: &Connection,
    migration: &DomainMigration,
) -> Result<Option<(NonZeroU64, MigrationDigest)>, StoreError> {
    let row = connection
        .query_row(
            "SELECT version, digest FROM application_domain_migrations
             WHERE domain = ?1 AND migration_key = ?2",
            params![migration.domain.as_str(), migration.key.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(infrastructure)?;
    row.map(|(version, digest)| {
        let version = u64::try_from(version)
            .ok()
            .and_then(NonZeroU64::new)
            .ok_or_else(|| StoreError::Infrastructure("corrupt migration version".to_owned()))?;
        let digest: [u8; 32] = digest
            .try_into()
            .map_err(|_| StoreError::Infrastructure("corrupt migration digest".to_owned()))?;
        Ok((version, MigrationDigest(digest)))
    })
    .transpose()
}

/// Returns an existing equal-digest version so a renamed migration cannot execute twice.
fn migration_by_digest(
    connection: &Connection,
    domain: &DomainName,
    digest: MigrationDigest,
) -> Result<Option<NonZeroU64>, StoreError> {
    let version = connection
        .query_row(
            "SELECT version FROM application_domain_migrations WHERE domain = ?1 AND digest = ?2",
            params![domain.as_str(), digest.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(infrastructure)?;
    version
        .map(|version| {
            u64::try_from(version)
                .ok()
                .and_then(NonZeroU64::new)
                .ok_or_else(|| StoreError::Infrastructure("corrupt migration version".to_owned()))
        })
        .transpose()
}

/// Allocates the next domain-local positive version while this owner thread excludes concurrent admission.
fn next_migration_version(
    connection: &Connection,
    domain: &DomainName,
) -> Result<NonZeroU64, StoreError> {
    let latest = connection
        .query_row(
            "SELECT MAX(version) FROM application_domain_migrations WHERE domain = ?1",
            params![domain.as_str()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(infrastructure)?
        .unwrap_or(0);
    let next = u64::try_from(latest)
        .ok()
        .and_then(|value| value.checked_add(1))
        .and_then(NonZeroU64::new)
        .ok_or_else(|| StoreError::Infrastructure("migration version overflow".to_owned()))?;
    Ok(next)
}

/// Detects whether only Application-owned schema objects exist and this domain has no ledger entry.
fn database_is_fresh_for_domain(
    connection: &Connection,
    domain: &DomainName,
) -> Result<bool, StoreError> {
    let foreign_object_count = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%'
               AND name NOT IN ('application_operation_receipts', 'application_domain_migrations')",
            [],
            |row| row.get::<_, usize>(0),
        )
        .map_err(infrastructure)?;
    let domain_has_ledger = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM application_domain_migrations WHERE domain = ?1)",
            params![domain.as_str()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(infrastructure)?;
    Ok(foreign_object_count == 0 && !domain_has_ledger)
}

/// Creates and fsyncs a safe owned SQLite backup before a nonfresh migration can run.
fn create_backup(
    connection: &Connection,
    backup_root: &Path,
    migration: &DomainMigration,
    version: NonZeroU64,
) -> Result<BackupRef, ()> {
    let backup_name = migration_backup_name(migration, version);
    let backup_path = backup_root.join(&backup_name);
    if fs::symlink_metadata(&backup_path).is_ok() {
        return Err(());
    }
    connection
        .backup(MAIN_DB, &backup_path, None)
        .map_err(|_| ())?;
    let mut file = File::open(&backup_path).map_err(|_| ())?;
    file.sync_all().map_err(|_| ())?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(|_| ())?;
    File::open(backup_root)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ())?;
    Ok(BackupRef {
        relative_path: backup_name,
        digest: MigrationDigest(*blake3::hash(&bytes).as_bytes()),
    })
}

/// Derives a filesystem-safe backup filename without embedding an untrusted domain or migration key.
fn migration_backup_name(migration: &DomainMigration, version: NonZeroU64) -> String {
    let identity = format!(
        "{}\0{}\0{}",
        migration.domain.as_str(),
        migration.key.as_str(),
        hex_digest(migration.expected_digest),
    );
    format!(
        "migration-v{}-{}.sqlite",
        version.get(),
        blake3::hash(identity.as_bytes()).to_hex()
    )
}

/// Encodes a fixed digest into hexadecimal only for the backup-name hash input.
fn hex_digest(digest: MigrationDigest) -> String {
    digest
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Executes one unique operation only after durable queued-receipt and hard-capacity admission.
fn execute_one(
    connection: &mut Connection,
    config: StoreConfig,
    operation: OperationId,
    job: Box<dyn StoreJob>,
) {
    match read_existing_outcome(connection, &operation) {
        Ok(Some(existing)) => {
            job.fail(StoreError::DuplicateOperation { existing });
            return;
        }
        Ok(None) => {}
        Err(error) => {
            job.fail(error);
            return;
        }
    }
    if let Err(error) = ensure_receipt_capacity(connection, config.receipt_capacity) {
        job.fail(error);
        return;
    }
    if let Err(error) = write_outcome(connection, &operation, StoreOutcome::Queued) {
        job.fail(error);
        return;
    }
    let begin = begin_transaction(connection);
    if let Err(error) = &begin {
        let busy = matches!(error, StoreError::Busy);
        drop(begin);
        if busy {
            if write_outcome(connection, &operation, StoreOutcome::Busy).is_ok() {
                job.fail(StoreError::Busy);
            } else {
                job.fail(StoreError::OutcomeUnknown { operation });
            }
        } else {
            job.fail(StoreError::OutcomeUnknown { operation });
        }
        return;
    }
    let transaction = begin.expect("the error branch returned");
    if transaction
        .execute(
            "UPDATE application_operation_receipts SET outcome = ?1 WHERE operation_id = ?2",
            params![StoreOutcome::Started.as_str(), operation.as_str()],
        )
        .is_err()
    {
        job.fail(StoreError::OutcomeUnknown { operation });
        return;
    }
    let completion = match job.run(&transaction) {
        JobRun::Failure(completion) => {
            let result = if transaction.rollback().is_ok() {
                if write_outcome(connection, &operation, StoreOutcome::RolledBack).is_ok() {
                    Err(StoreError::RolledBack)
                } else {
                    Err(StoreError::OutcomeUnknown { operation })
                }
            } else {
                Err(StoreError::OutcomeUnknown { operation })
            };
            completion.finish(result);
            return;
        }
        JobRun::Success(completion) => completion,
    };
    if transaction
        .execute(
            "UPDATE application_operation_receipts SET outcome = ?1 WHERE operation_id = ?2",
            params![StoreOutcome::Committed.as_str(), operation.as_str()],
        )
        .is_err()
    {
        completion.finish(Err(StoreError::OutcomeUnknown { operation }));
        return;
    }
    match transaction.commit() {
        Ok(()) => completion.finish(Ok(StoreOutcome::Committed)),
        Err(_) => completion.finish(Err(StoreError::OutcomeUnknown { operation })),
    }
}

/// Begins the owner-controlled immediate transaction without leaking a mutable borrow on failure.
fn begin_transaction(connection: &mut Connection) -> Result<Transaction<'_>, StoreError> {
    connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| {
            if is_busy(&error) {
                StoreError::Busy
            } else {
                StoreError::Infrastructure(error.to_string())
            }
        })
}

/// Returns the receipt or conservative unknown state when no trustworthy durable evidence exists.
fn read_outcome(
    connection: &Connection,
    operation: &OperationId,
) -> Result<StoreOutcome, StoreError> {
    Ok(read_existing_outcome(connection, operation)?.unwrap_or(StoreOutcome::OutcomeUnknown))
}

/// Reads one receipt without treating a corrupt value as absent or safe to replay.
fn read_existing_outcome(
    connection: &Connection,
    operation: &OperationId,
) -> Result<Option<StoreOutcome>, StoreError> {
    let value = connection
        .query_row(
            "SELECT outcome FROM application_operation_receipts WHERE operation_id = ?1",
            params![operation.as_str()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(infrastructure)?;
    Ok(value.map(|value| StoreOutcome::parse(&value)))
}

/// Inserts or updates one durable Application mechanics receipt outside a domain transaction.
fn write_outcome(
    connection: &Connection,
    operation: &OperationId,
    outcome: StoreOutcome,
) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO application_operation_receipts (operation_id, outcome)
             VALUES (?1, ?2)
             ON CONFLICT(operation_id) DO UPDATE SET outcome = excluded.outcome",
            params![operation.as_str(), outcome.as_str()],
        )
        .map_err(infrastructure)?;
    Ok(())
}

/// Refuses new admission after durable mechanics receipts reach their hard bounded capacity.
fn ensure_receipt_capacity(connection: &Connection, capacity: usize) -> Result<(), StoreError> {
    let count = connection
        .query_row(
            "SELECT COUNT(*) FROM application_operation_receipts",
            [],
            |row| row.get::<_, usize>(0),
        )
        .map_err(infrastructure)?;
    if count >= capacity {
        Err(StoreError::ReceiptCapacityExhausted)
    } else {
        Ok(())
    }
}

/// Maps a SQLite error to local mechanics diagnostics while retaining conservative effect semantics.
fn infrastructure(error: rusqlite::Error) -> StoreError {
    StoreError::Infrastructure(error.to_string())
}

/// Identifies SQLite lock errors that have no domain transaction effect to report.
fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(code, _)
            if matches!(code.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}
