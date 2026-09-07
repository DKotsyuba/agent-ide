//! Contract checks for the Application-owned SQLite thread and conservative durable receipts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_ide::app::config::StoreConfig;
use agent_ide::app::store::{OperationId, Store, StoreError, StoreOutcome};
use rusqlite::Connection;

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Creates a unique database pathname whose parent is already owned by this test process.
fn database_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-store-{}-{}.sqlite",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Returns deliberately small bounded mechanics limits suitable for isolated owner-thread tests.
fn test_config(receipt_capacity: usize) -> StoreConfig {
    StoreConfig {
        queue_capacity: 4,
        busy_timeout: Duration::from_millis(100),
        request_deadline: Duration::from_secs(1),
        receipt_capacity,
    }
}

/// Rejects direct Store construction with a zero limit instead of relying on callers to use config merging.
#[test]
fn direct_store_open_validates_bounded_limits() {
    let path = database_path();
    let error = Store::open(
        &path,
        StoreConfig {
            queue_capacity: 0,
            ..test_config(1)
        },
    )
    .err()
    .unwrap();
    assert_eq!(error, StoreError::InvalidConfig);
    assert!(!path.exists());
}

/// Removes the database and WAL sidecars created solely by this test.
fn remove_database(path: &Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{}", path.display(), suffix));
        let _ = fs::remove_file(path);
    }
}

/// Proves typed values arrive only after atomic commit and duplicate IDs never rerun their closure.
#[tokio::test]
async fn typed_commit_and_duplicate_suppression_are_durable() {
    let path = database_path();
    let store = Store::open(&path, test_config(4)).unwrap();
    let operation = OperationId::new("create-counter").unwrap();
    let value = store
        .execute(operation.clone(), |transaction| {
            transaction.execute("CREATE TABLE counter (value INTEGER NOT NULL)", [])?;
            transaction.execute("INSERT INTO counter VALUES (7)", [])?;
            Ok(7_u64)
        })
        .await
        .unwrap();
    assert_eq!(value, 7);
    assert_eq!(
        store.outcome(operation.clone()).await.unwrap(),
        StoreOutcome::Committed
    );
    let duplicate = store
        .execute(operation, |_| -> rusqlite::Result<()> {
            panic!("duplicate SQL closure must not run")
        })
        .await
        .unwrap_err();
    assert_eq!(
        duplicate,
        StoreError::DuplicateOperation {
            existing: StoreOutcome::Committed
        }
    );
    drop(store);
    remove_database(&path);
}

/// Proves a failing domain closure rolls back its SQL and records rollback for reconciliation.
#[tokio::test]
async fn failing_domain_sql_rolls_back_and_is_not_replayed() {
    let path = database_path();
    let store = Store::open(&path, test_config(4)).unwrap();
    let operation = OperationId::new("rollback").unwrap();
    let failure = store
        .execute(operation.clone(), |transaction| {
            transaction.execute("CREATE TABLE should_rollback (value INTEGER)", [])?;
            Err::<(), _>(rusqlite::Error::InvalidQuery)
        })
        .await
        .unwrap_err();
    assert_eq!(failure, StoreError::RolledBack);
    assert_eq!(
        store.outcome(operation).await.unwrap(),
        StoreOutcome::RolledBack
    );
    drop(store);
    let connection = Connection::open(&path).unwrap();
    assert!(connection.prepare("SELECT * FROM should_rollback").is_err());
    drop(connection);
    remove_database(&path);
}

/// Converts receipts interrupted before a known terminal state to conservative unknown on store restart.
#[tokio::test]
async fn restart_marks_incomplete_receipts_unknown() {
    let path = database_path();
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE application_operation_receipts (
                 operation_id TEXT PRIMARY KEY NOT NULL,
                 outcome TEXT NOT NULL
             );
             INSERT INTO application_operation_receipts VALUES ('interrupted', 'started');",
        )
        .unwrap();
    drop(connection);
    let store = Store::open(&path, test_config(4)).unwrap();
    assert_eq!(
        store
            .outcome(OperationId::new("interrupted").unwrap())
            .await
            .unwrap(),
        StoreOutcome::OutcomeUnknown
    );
    drop(store);
    remove_database(&path);
}

/// Refuses fresh operation admission at the hard receipt cap instead of forgetting an old ID.
#[tokio::test]
async fn receipt_capacity_never_evicts_operation_ids() {
    let path = database_path();
    let store = Store::open(&path, test_config(1)).unwrap();
    store
        .execute(OperationId::new("first").unwrap(), |_| {
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    let error = store
        .execute(OperationId::new("second").unwrap(), |_| {
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap_err();
    assert_eq!(error, StoreError::ReceiptCapacityExhausted);
    drop(store);
    remove_database(&path);
}
