//! Bounded, durable SQLite execution mechanics without domain schemas or transition policy.

use std::fmt::{self, Display};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, Transaction, TransactionBehavior, params,
};
use tokio::sync::oneshot;

use super::config::StoreConfig;

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
    /// Opens `database_path` on one dedicated owner thread and initializes only mechanics receipts.
    ///
    /// The owner configures bundled SQLite with WAL and the configured busy timeout, then converts
    /// interrupted `Queued`/`Started` receipts to `OutcomeUnknown`. It creates no domain table and
    /// does not infer whether interrupted external work happened.
    pub fn open(database_path: &Path, config: StoreConfig) -> Result<Self, StoreError> {
        validate_store_config(config)?;
        let (sender, receiver) = mpsc::sync_channel(config.queue_capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let database_path = database_path.to_path_buf();
        thread::Builder::new()
            .name("agent-ide-sqlite".to_owned())
            .spawn(move || owner_thread(database_path, config, receiver, ready_sender))
            .map_err(|error| StoreError::Infrastructure(error.to_string()))?;
        ready_receiver
            .recv()
            .map_err(|_| StoreError::Unavailable)??;
        Ok(Self { sender, config })
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
}

/// Opens SQLite once, applies Application mechanics policy, and serially owns all messages.
fn owner_thread(
    database_path: std::path::PathBuf,
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
        }
    }
}

/// Opens bundled SQLite and initializes bounded, durable Application mechanics receipts.
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
             UPDATE application_operation_receipts
                 SET outcome = 'outcome_unknown'
                 WHERE outcome IN ('queued', 'started');",
        )
        .map_err(infrastructure)?;
    Ok(connection)
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
