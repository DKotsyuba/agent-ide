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
    Connection, ErrorCode, MAIN_DB, OpenFlags, OptionalExtension, Transaction, TransactionBehavior,
    params,
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

/// Reports whether one accepted receipt-free transaction was observed to commit before its caller deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UntrackedOutcome<T> {
    /// The transaction committed and produced the enclosed domain value.
    Committed(T),
    /// The caller deadline elapsed after queue admission, so the transaction may still commit later.
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
    /// Retained for the stable error vocabulary; the store no longer caps receipts, so it never returns this.
    ReceiptCapacityExhausted,
    /// The operation ID's settled receipt was retired behind the replay horizon; its closure was not run.
    ReceiptExpired,
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
            Self::ReceiptExpired => formatter.write_str("store receipt expired"),
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
        Self::open_inner(database_path, None, config, false)
    }

    /// Opens an existing database on a dedicated read-only owner without creating ledgers or files.
    ///
    /// This variant is for fixed trusted export/query paths only. SQLite refuses every write,
    /// including WAL or migration setup, so a missing, locked, or non-readable database returns
    /// the existing infrastructure failure rather than being created or repaired.
    pub fn open_read_only(database_path: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        Self::open_inner(database_path, None, config, true)
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
        Self::open_inner(database_path, Some(backup_root), config, false)
    }

    /// Starts the owner thread after all public Store construction variants have validated inputs.
    fn open_inner(
        database_path: &Path,
        backup_root: Option<PathBuf>,
        config: StoreConfig,
        read_only: bool,
    ) -> Result<Self, StoreError> {
        validate_store_config(config)?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let database_path = database_path.to_path_buf();
        thread::Builder::new()
            .name("agent-ide-sqlite".to_owned())
            .spawn(move || {
                owner_thread(
                    database_path,
                    backup_root,
                    config,
                    read_only,
                    receiver,
                    ready_sender,
                )
            })
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

    /// Reconciles one migration key without executing trusted SQL or allocating another version.
    ///
    /// Workspace calls this only after `MigrationAdmission::OutcomeUnknown`. A present immutable
    /// ledger row returns `AlreadyApplied`; a missing or unreadable row remains unknown and never
    /// grants permission to replay the migration.
    pub async fn migration_admission(
        &self,
        domain: DomainName,
        key: MigrationKey,
    ) -> Result<MigrationAdmission, StoreError> {
        let unknown_key = key.clone();
        let (reply_sender, reply_receiver) = oneshot::channel();
        match self.sender.try_send(StoreMessage::MigrationLookup {
            domain,
            key,
            reply: reply_sender,
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, reply_receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Ok(MigrationAdmission::OutcomeUnknown { key: unknown_key }),
        }
    }

    /// Executes typed domain SQL inside an Application-owned transaction with a durable operation receipt.
    ///
    /// `sql` receives the live [`Transaction`] and must not manually begin, commit, or roll back a
    /// top-level transaction. Its value is delivered only after a commit that atomically includes the
    /// `Committed` receipt. A duplicate operation returns its prior state without calling `sql`, and an
    /// ID whose settled receipt was retired behind the replay horizon returns
    /// [`StoreError::ReceiptExpired`], also without calling `sql`. Unresolved receipts are kept
    /// indefinitely and keep answering as duplicates. After an accepted timeout, callers must use
    /// [`Self::outcome`] and never resubmit this operation ID.
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

    /// Reads at most one row of trusted static SELECT SQL on the existing owner thread.
    /// SQLite must classify the statement as read-only; no operation receipt is allocated.
    /// Empty results return None; queue, decoding, and timeout failures never imply a domain effect.
    pub async fn read_one<T, F>(
        &self,
        sql: &'static str,
        parameters: Vec<rusqlite::types::Value>,
        decode: F,
    ) -> Result<Option<T>, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        let (reply, receive) = oneshot::channel();
        let job = Box::new(TypedRead {
            sql,
            parameters,
            decode,
            reply,
        });
        match self.sender.try_send(StoreMessage::Read { job }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, receive).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Executes one trusted, receipt-free SQLite transaction on the owner thread.
    ///
    /// This is reserved for local append-only domains whose writes have no external effect and
    /// therefore need neither an operation receipt nor replay authority. `sql` receives the live
    /// transaction, must not manage a top-level transaction itself, and returns its typed value
    /// only after commit. Queue saturation, owner loss, SQLite failure, or a caller deadline
    /// return an error and do not alter the caller's domain behaviour. An accepted caller timeout
    /// returns [`UntrackedOutcome::OutcomeUnknown`] because queued work may still commit later.
    pub async fn execute_untracked<T, F>(&self, sql: F) -> Result<UntrackedOutcome<T>, StoreError>
    where
        T: Send + 'static,
        F: for<'transaction> FnOnce(&Transaction<'transaction>) -> rusqlite::Result<T>
            + Send
            + 'static,
    {
        let (reply_sender, reply_receiver) = oneshot::channel();
        let job = Box::new(TypedJob {
            sql,
            reply: reply_sender,
        });
        match self.sender.try_send(StoreMessage::ExecuteUntracked { job }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, reply_receiver).await {
            Ok(Ok(result)) => result.map(UntrackedOutcome::Committed),
            Ok(Err(_)) | Err(_) => Ok(UntrackedOutcome::OutcomeUnknown),
        }
    }

    /// Executes one receipt-free transaction and waits for its actual owner-thread settlement.
    ///
    /// This variant is reserved for background drains that have already accepted local work and
    /// must not finish shutdown while a commit remains outcome-unknown. Queue refusal is returned
    /// immediately; after admission the call waits until commit, rollback, or owner loss, without
    /// changing the bounded deadline of [`Self::execute_untracked`]. `sql` receives the live
    /// Application transaction, must not manage a top-level transaction, and yields its value only
    /// after commit.
    pub(crate) async fn execute_untracked_settled<T, F>(&self, sql: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: for<'transaction> FnOnce(&Transaction<'transaction>) -> rusqlite::Result<T>
            + Send
            + 'static,
    {
        let (reply_sender, reply_receiver) = oneshot::channel();
        let job = Box::new(TypedJob {
            sql,
            reply: reply_sender,
        });
        match self.sender.try_send(StoreMessage::ExecuteUntracked { job }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        reply_receiver.await.map_err(|_| StoreError::Unavailable)?
    }

    /// Reads at most `limit` rows of trusted static read-only SQL on the existing owner thread.
    ///
    /// `limit` must be in `1..=1001`; the extra row lets a domain prove page continuation while
    /// exposing no unbounded SQLite cursor. SQL is fixed by the trusted domain owner and must be a
    /// read-only `SELECT`; Application binds only the supplied values and never interprets rows.
    /// Queue, decode, timeout, and owner failures return an error without allocating a receipt.
    pub async fn read_many<T, F>(
        &self,
        sql: &'static str,
        parameters: Vec<rusqlite::types::Value>,
        limit: usize,
        decode: F,
    ) -> Result<Vec<T>, StoreError>
    where
        T: Send + 'static,
        F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        const MAX_TRUSTED_MULTI_ROWS: usize = 1_001;
        if limit == 0 || limit > MAX_TRUSTED_MULTI_ROWS {
            return Err(StoreError::InvalidConfig);
        }
        let (reply, receive) = oneshot::channel();
        let job = Box::new(TypedReadMany {
            sql,
            parameters,
            limit,
            decode,
            reply,
        });
        match self.sender.try_send(StoreMessage::ReadMany { job }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(StoreError::QueueFull),
            Err(TrySendError::Disconnected(_)) => return Err(StoreError::Unavailable),
        }
        match tokio::time::timeout(self.config.request_deadline, receive).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) | Err(_) => Err(StoreError::Unavailable),
        }
    }

    /// Looks up the durable mechanics receipt without replaying domain SQL.
    ///
    /// Missing, interrupted, corrupt, or retired (expired) receipts return `OutcomeUnknown`; that
    /// condition does not authorize a retry. Lookup is bounded by the same owner queue and request deadline as execute.
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

/// Erases a single-row SELECT while keeping its typed result on the existing owner thread.
trait StoreRead: Send {
    /// Executes read-only SQL and sends one bounded row or the classified failure.
    fn run(self: Box<Self>, connection: &Connection);
}

/// Erases a bounded trusted multi-row SELECT while retaining its typed decoder on the owner thread.
trait StoreReadMany: Send {
    /// Executes the static read-only query and returns no more than its prevalidated row ceiling.
    fn run(self: Box<Self>, connection: &Connection);
}

/// Owns one static query, bound values, row decoder, and typed reply channel.
struct TypedRead<T, F> {
    /// Trusted SELECT statement, never model-supplied SQL.
    sql: &'static str,
    /// Owned SQLite bind values.
    parameters: Vec<rusqlite::types::Value>,
    /// Converts the first returned row to the caller's domain value.
    decode: F,
    /// Returns an optional decoded row without a durable mechanics receipt.
    reply: oneshot::Sender<Result<Option<T>, StoreError>>,
}

impl<T, F> StoreRead for TypedRead<T, F>
where
    T: Send + 'static,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T> + Send + 'static,
{
    /// Refuses non-SELECT or SQLite-write statements before binding or stepping them.
    fn run(self: Box<Self>, connection: &Connection) {
        let Self {
            sql,
            parameters,
            decode,
            reply,
        } = *self;
        let result = (|| {
            if !sql.trim_start().starts_with("SELECT ") {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let mut statement = connection.prepare(sql)?;
            if !statement.readonly() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            statement
                .query_row(rusqlite::params_from_iter(parameters), decode)
                .optional()
        })()
        .map_err(infrastructure);
        let _ = reply.send(result);
    }
}

/// Owns one bounded static query and its stateful row decoder for a trusted domain page.
struct TypedReadMany<T, F> {
    /// Trusted static SELECT statement, never model-supplied SQL.
    sql: &'static str,
    /// Owned SQLite bind values.
    parameters: Vec<rusqlite::types::Value>,
    /// Hard maximum decoded rows, validated before owner-thread submission.
    limit: usize,
    /// Converts each returned row into a domain value in durable sequence order.
    decode: F,
    /// Delivers the bounded page without a durable mechanics receipt.
    reply: oneshot::Sender<Result<Vec<T>, StoreError>>,
}

impl<T, F> StoreReadMany for TypedReadMany<T, F>
where
    T: Send + 'static,
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T> + Send + 'static,
{
    /// Refuses non-SELECT or SQLite-write statements and limits decoding before returning a page.
    fn run(self: Box<Self>, connection: &Connection) {
        let Self {
            sql,
            parameters,
            limit,
            mut decode,
            reply,
        } = *self;
        let result = (|| {
            if !sql.trim_start().starts_with("SELECT ") {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let mut statement = connection.prepare(sql)?;
            if !statement.readonly() {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let mut rows = statement.query(rusqlite::params_from_iter(parameters))?;
            let mut values = Vec::with_capacity(limit);
            while values.len() < limit {
                let Some(row) = rows.next()? else { break };
                values.push(decode(row)?);
            }
            Ok(values)
        })()
        .map_err(infrastructure);
        let _ = reply.send(result);
    }
}

/// Carries one bounded owner-thread execution or read-only mechanics lookup.
enum StoreMessage {
    /// Reads a domain row without admitting a mutating operation or allocating a receipt.
    Read {
        /// Single-row static SELECT and typed response.
        job: Box<dyn StoreRead>,
    },
    /// Reads a fixed bounded domain page without interpreting its row semantics.
    ReadMany {
        /// Static SELECT, capped decoder, and typed reply channel.
        job: Box<dyn StoreReadMany>,
    },
    /// Runs a receipt-free local-only transaction for a trusted append-only domain.
    ExecuteUntracked {
        /// Typed closure whose result is delivered only after transaction commit.
        job: Box<dyn StoreJob>,
    },
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
    /// Reads one immutable migration ledger row without executing or admitting any migration SQL.
    MigrationLookup {
        /// Domain namespace that scopes the opaque migration key.
        domain: DomainName,
        /// Stable key to reconcile after an unknown admission result.
        key: MigrationKey,
        /// Returns only a known applied row or an explicit unknown state.
        reply: oneshot::Sender<Result<MigrationAdmission, StoreError>>,
    },
}

/// Opens SQLite once, applies Application mechanics policy, and serially owns all messages.
fn owner_thread(
    database_path: std::path::PathBuf,
    backup_root: Option<PathBuf>,
    config: StoreConfig,
    read_only: bool,
    receiver: mpsc::Receiver<StoreMessage>,
    ready_sender: SyncSender<Result<(), StoreError>>,
) {
    let mut connection = match open_connection(&database_path, config, read_only) {
        Ok(connection) => {
            let _ = ready_sender.send(Ok(()));
            connection
        }
        Err(error) => {
            let _ = ready_sender.send(Err(error));
            return;
        }
    };
    let mut window = ReceiptWindow::default();
    if !read_only {
        window.sweep(&mut connection, config.receipt_capacity);
    }
    while let Ok(message) = receiver.recv() {
        match message {
            StoreMessage::Read { job } => job.run(&connection),
            StoreMessage::ReadMany { job } => job.run(&connection),
            StoreMessage::ExecuteUntracked { job } => {
                execute_untracked_one(&mut connection, job);
            }
            StoreMessage::Execute { operation, job } => {
                execute_one(&mut connection, config, &mut window, operation, job);
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
            StoreMessage::MigrationLookup { domain, key, reply } => {
                let _ = reply.send(read_migration_admission(&connection, domain, key));
            }
        }
    }
}

/// Opens bundled SQLite and initializes bounded, durable Application operation and migration ledgers.
fn open_connection(
    database_path: &Path,
    config: StoreConfig,
    read_only: bool,
) -> Result<Connection, StoreError> {
    let connection = if read_only {
        Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    } else {
        Connection::open(database_path)
    }
    .map_err(infrastructure)?;
    connection
        .busy_timeout(config.busy_timeout)
        .map_err(infrastructure)?;
    if read_only {
        return Ok(connection);
    }
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS application_operation_receipts (
                 operation_id TEXT PRIMARY KEY NOT NULL,
                 outcome TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS application_expired_receipts (
                 fingerprint BLOB PRIMARY KEY NOT NULL
             ) WITHOUT ROWID;
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
    if let Some((version, existing_digest)) =
        migration_by_key(connection, &migration.domain, &migration.key)?
    {
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
    domain: &DomainName,
    key: &MigrationKey,
) -> Result<Option<(NonZeroU64, MigrationDigest)>, StoreError> {
    let row = connection
        .query_row(
            "SELECT version, digest FROM application_domain_migrations
             WHERE domain = ?1 AND migration_key = ?2",
            params![domain.as_str(), key.as_str()],
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

/// Reads one completed migration row without interpreting a missing or corrupt row as safe to replay.
fn read_migration_admission(
    connection: &Connection,
    domain: DomainName,
    key: MigrationKey,
) -> Result<MigrationAdmission, StoreError> {
    match migration_by_key(connection, &domain, &key)? {
        Some((version, digest)) => Ok(MigrationAdmission::AlreadyApplied { version, digest }),
        None => Ok(MigrationAdmission::OutcomeUnknown { key }),
    }
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
               AND name NOT IN (
                   'application_operation_receipts',
                   'application_expired_receipts',
                   'application_domain_migrations'
               )",
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
    window: &mut ReceiptWindow,
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
    match receipt_expired(connection, &operation) {
        Ok(false) => {}
        Ok(true) => {
            job.fail(StoreError::ReceiptExpired);
            return;
        }
        Err(error) => {
            job.fail(error);
            return;
        }
    }
    window.admit(connection, config.receipt_capacity);
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

/// Commits one trusted local-only transaction without creating an operation receipt or retry right.
fn execute_untracked_one(connection: &mut Connection, job: Box<dyn StoreJob>) {
    let transaction = match begin_transaction(connection) {
        Ok(transaction) => transaction,
        Err(error) => {
            job.fail(error);
            return;
        }
    };
    let completion = match job.run(&transaction) {
        JobRun::Failure(completion) => {
            let result = transaction.rollback().map_err(infrastructure);
            completion.finish(result.and(Err(StoreError::RolledBack)));
            return;
        }
        JobRun::Success(completion) => completion,
    };
    completion.finish(
        transaction
            .commit()
            .map_err(infrastructure)
            .map(|_| StoreOutcome::Committed),
    );
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

/// Receipts admitted between two retirement sweeps; keeps the per-write cost O(1).
const SWEEP_INTERVAL: u32 = 64;

/// Settled receipts retired per transaction, bounding how long one sweep holds the write lock.
const SWEEP_BATCH: usize = 512;

/// Outcomes that are definite and carry no effect still in doubt; the only retirable receipts.
const SETTLED_SQL: &str = "('committed', 'rolled_back', 'busy')";

/// Owner-thread bookkeeping for the receipt replay horizon.
///
/// Settled receipts older than the newest `receipt_capacity` admissions are retired into a
/// 16-byte tombstone so a replay of the ID is refused as expired and never re-executed. A
/// queued, started, unknown, or unrecognized receipt is never retired and never blocks admission.
#[derive(Default)]
struct ReceiptWindow {
    /// Admissions since the last sweep.
    since_sweep: u32,
}

impl ReceiptWindow {
    /// Counts one admission and sweeps on schedule.
    fn admit(&mut self, connection: &mut Connection, horizon: usize) {
        self.since_sweep += 1;
        if self.since_sweep >= SWEEP_INTERVAL {
            self.sweep(connection, horizon);
        }
    }

    /// Retires settled receipts beyond the horizon.
    ///
    /// Housekeeping is best effort: a failure keeps every receipt and is retried at the next sweep.
    fn sweep(&mut self, connection: &mut Connection, horizon: usize) {
        self.since_sweep = 0;
        let _ = retire_settled(connection, horizon);
    }
}

/// Fixed-size identity of a retired operation ID; a collision can only refuse, never replay.
fn receipt_fingerprint(operation_id: &str) -> [u8; 16] {
    let mut fingerprint = [0u8; 16];
    fingerprint.copy_from_slice(&blake3::hash(operation_id.as_bytes()).as_bytes()[..16]);
    fingerprint
}

/// Reports whether the ID's settled receipt was retired behind the replay horizon.
fn receipt_expired(connection: &Connection, operation: &OperationId) -> Result<bool, StoreError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM application_expired_receipts WHERE fingerprint = ?1)",
            params![receipt_fingerprint(operation.as_str()).as_slice()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(infrastructure)
}

/// Moves every settled receipt older than the newest `horizon` rowids into a tombstone.
///
/// The receipt row and its tombstone change in one transaction, so a crash leaves the ID either
/// fully recorded or fully expired. Rowids grow with admission order, which makes the horizon an
/// age without a clock or a per-write count.
fn retire_settled(connection: &mut Connection, horizon: usize) -> Result<(), StoreError> {
    let newest = connection
        .query_row(
            "SELECT MAX(rowid) FROM application_operation_receipts",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )
        .map_err(infrastructure)?;
    let Some(cutoff) =
        newest.and_then(|newest| newest.checked_sub(i64::try_from(horizon).unwrap_or(i64::MAX)))
    else {
        return Ok(());
    };
    loop {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(infrastructure)?;
        let victims = {
            let mut statement = transaction
                .prepare(&format!(
                    "SELECT rowid, operation_id FROM application_operation_receipts
                     WHERE rowid <= ?1 AND outcome IN {SETTLED_SQL} ORDER BY rowid LIMIT ?2"
                ))
                .map_err(infrastructure)?;
            statement
                .query_map(params![cutoff, SWEEP_BATCH as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(infrastructure)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(infrastructure)?
        };
        for (rowid, operation_id) in &victims {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO application_expired_receipts (fingerprint) VALUES (?1)",
                    params![receipt_fingerprint(operation_id).as_slice()],
                )
                .map_err(infrastructure)?;
            transaction
                .execute(
                    &format!(
                        "DELETE FROM application_operation_receipts
                         WHERE rowid = ?1 AND outcome IN {SETTLED_SQL}"
                    ),
                    params![rowid],
                )
                .map_err(infrastructure)?;
        }
        transaction.commit().map_err(infrastructure)?;
        if victims.len() < SWEEP_BATCH {
            return Ok(());
        }
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
