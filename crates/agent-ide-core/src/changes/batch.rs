//! Durable receipts of multi-file edits (`ide.edit {files}`, batch-api-v1 §7).
//!
//! A multi-file edit cannot be one atomic filesystem transaction, so its safety rests on this
//! receipt: before the first write the operation records the digest of the caller's own arguments
//! and, per file in request order, the path, the digest of the bytes it was validated against
//! (absent for a file to create) and the digest of the candidate it will write. Each file is
//! marked written once its write is known; the operation is settled once every file is written.
//! A retry with the same operation id and request compares current bytes against these digests,
//! so it resumes exactly the unwritten files, never applies a change twice and never overwrites
//! bytes it did not validate. The receipt holds digests and paths only, never content.

use rusqlite::{params, types::Value};

use crate::app::store::{
    DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey, OperationId,
    Store, StoreError, TrustedUpSql,
};

/// Stable Changes migration namespace, shared with the one-file receipts.
const DOMAIN: &str = "changes";
/// Immutable migration key of the multi-file receipt schema.
const KEY: &str = "batch_edit_receipts_v1";
/// Changes-owned schema: one row per operation and one row per file of it, digests only.
const SQL: &str = "CREATE TABLE changes_batch_receipts (
 operation_id TEXT PRIMARY KEY, request_digest BLOB NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('prepared','settled')));
CREATE TABLE changes_batch_files (
 operation_id TEXT NOT NULL REFERENCES changes_batch_receipts(operation_id),
 idx INTEGER NOT NULL CHECK(idx >= 0), relative_path TEXT NOT NULL,
 base_digest BLOB, candidate_digest BLOB NOT NULL,
 written INTEGER NOT NULL DEFAULT 0 CHECK(written IN (0,1)),
 PRIMARY KEY(operation_id, idx))";

/// Most files one receipt may hold, matching the `ide.edit {files}` cap.
pub const MAX_BATCH_FILES: usize = 16;

/// One file of a multi-file edit as recorded before the first write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchFile {
    /// Exact worktree-relative path as requested.
    pub path: String,
    /// BLAKE3 digest of the bytes the file was validated against; `None` for a file to create.
    pub base: Option<[u8; 32]>,
    /// BLAKE3 digest of the exact candidate bytes the commit writes.
    pub candidate: [u8; 32],
    /// True once the file's write is known to have happened.
    pub written: bool,
}

/// The durable state of one multi-file edit operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchReceipt {
    /// Digest of the caller's canonical arguments; a different digest is a conflicting duplicate.
    pub request_digest: [u8; 32],
    /// True once every file was written and the operation settled.
    pub settled: bool,
    /// Every file in request order.
    pub files: Vec<BatchFile>,
}

/// Why a receipt operation could not establish durable state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchReceiptError {
    /// Application mechanics refused or could not settle the transaction.
    Application(StoreError),
    /// A stored row violates the closed schema (wrong digest width, index gap, …).
    CorruptReceipt,
    /// The request named no file or more than [`MAX_BATCH_FILES`].
    InvalidRequest,
}

impl From<StoreError> for BatchReceiptError {
    /// Keeps the Application failure for the caller's refusal.
    fn from(value: StoreError) -> Self {
        Self::Application(value)
    }
}

/// Result of [`BatchReceiptStore::prepare`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatchAdmission {
    /// This call recorded a new receipt; nothing was written before it.
    Prepared,
    /// A receipt for the operation id already exists; the caller compares its digest and states.
    Existing(BatchReceipt),
}

/// Changes semantics for multi-file receipts over Application's transaction mechanics.
pub struct BatchReceiptStore<'a> {
    /// The daemon's single Application store.
    application: &'a Store,
}

impl<'a> BatchReceiptStore<'a> {
    /// Binds multi-file receipts to one open Application store without installing the schema.
    pub const fn new(application: &'a Store) -> Self {
        Self { application }
    }

    /// Admits the immutable multi-file receipt schema; repeated calls report it already applied.
    pub async fn install_schema(&self) -> Result<MigrationAdmission, BatchReceiptError> {
        let sql = TrustedUpSql::new(SQL)?;
        let migration = DomainMigration {
            domain: DomainName::new(DOMAIN)?,
            key: MigrationKey::new(KEY)?,
            expected_digest: MigrationDigest::from_sql(&sql),
            up_sql: sql,
        };
        match self.application.admit_migration(migration.clone()).await? {
            MigrationAdmission::OutcomeUnknown { key } => Ok(self
                .application
                .migration_admission(migration.domain, key)
                .await?),
            admission => Ok(admission),
        }
    }

    /// Durably records a new operation before its first write, or returns the existing receipt.
    ///
    /// `files` must hold 1 to [`MAX_BATCH_FILES`] entries in request order; their `written` flags
    /// are ignored and stored as unwritten. An existing receipt is returned whatever its digest, so
    /// the caller decides between resume and conflicting duplicate. A prepare whose commit is
    /// uncertain is reconciled by reading the row; an unreadable one is an error and the caller
    /// must not write.
    pub async fn prepare(
        &self,
        operation_id: &str,
        request_digest: [u8; 32],
        files: &[BatchFile],
    ) -> Result<BatchAdmission, BatchReceiptError> {
        if files.is_empty() || files.len() > MAX_BATCH_FILES {
            return Err(BatchReceiptError::InvalidRequest);
        }
        if let Some(existing) = self.load(operation_id).await? {
            return Ok(BatchAdmission::Existing(existing));
        }
        let operation = operation_id.to_owned();
        let rows: Vec<(String, Option<Vec<u8>>, Vec<u8>)> = files
            .iter()
            .map(|file| {
                (
                    file.path.clone(),
                    file.base.map(|digest| digest.to_vec()),
                    file.candidate.to_vec(),
                )
            })
            .collect();
        let result = self
            .application
            .execute(mechanics("prepare", operation_id, None)?, move |tx| {
                tx.execute(
                    "INSERT INTO changes_batch_receipts (operation_id,request_digest,state) \
                     VALUES (?1,?2,'prepared')",
                    params![operation, request_digest.to_vec()],
                )?;
                for (index, (path, base, candidate)) in rows.into_iter().enumerate() {
                    tx.execute(
                        "INSERT INTO changes_batch_files \
                         (operation_id,idx,relative_path,base_digest,candidate_digest,written) \
                         VALUES (?1,?2,?3,?4,?5,0)",
                        params![operation, index as i64, path, base, candidate],
                    )?;
                }
                Ok(())
            })
            .await;
        match result {
            Ok(()) => Ok(BatchAdmission::Prepared),
            Err(StoreError::DuplicateOperation { .. } | StoreError::OutcomeUnknown { .. }) => {
                match self.load(operation_id).await? {
                    Some(existing) if existing.request_digest == request_digest => {
                        // Our own insert committed (or an identical one did): nothing is written yet.
                        if existing.files.iter().all(|file| !file.written) && !existing.settled {
                            Ok(BatchAdmission::Prepared)
                        } else {
                            Ok(BatchAdmission::Existing(existing))
                        }
                    }
                    Some(existing) => Ok(BatchAdmission::Existing(existing)),
                    None => Err(BatchReceiptError::CorruptReceipt),
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Marks file `index` of the operation written. Idempotent: a repeat changes nothing.
    pub async fn mark_written(
        &self,
        operation_id: &str,
        index: usize,
    ) -> Result<(), BatchReceiptError> {
        let operation = operation_id.to_owned();
        let result = self
            .application
            .execute(
                mechanics("written", operation_id, Some(index))?,
                move |tx| {
                    tx.execute(
                        "UPDATE changes_batch_files SET written=1 WHERE operation_id=?1 AND idx=?2",
                        params![operation, index as i64],
                    )?;
                    Ok(())
                },
            )
            .await;
        match result {
            Ok(()) | Err(StoreError::DuplicateOperation { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Settles the operation once every file is written. Idempotent.
    pub async fn settle(&self, operation_id: &str) -> Result<(), BatchReceiptError> {
        let operation = operation_id.to_owned();
        let result = self
            .application
            .execute(mechanics("settle", operation_id, None)?, move |tx| {
                tx.execute(
                    "UPDATE changes_batch_receipts SET state='settled' WHERE operation_id=?1",
                    params![operation],
                )?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) | Err(StoreError::DuplicateOperation { .. }) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Reads one operation's receipt and files without consuming an Application operation.
    pub async fn load(
        &self,
        operation_id: &str,
    ) -> Result<Option<BatchReceipt>, BatchReceiptError> {
        let head = self
            .application
            .read_one(
                "SELECT request_digest,state FROM changes_batch_receipts WHERE operation_id=?1",
                vec![Value::Text(operation_id.to_owned())],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .await?;
        let Some((request_digest, state)) = head else {
            return Ok(None);
        };
        let rows = self
            .application
            .read_many(
                "SELECT idx,relative_path,base_digest,candidate_digest,written \
                 FROM changes_batch_files WHERE operation_id=?1 ORDER BY idx",
                vec![Value::Text(operation_id.to_owned())],
                MAX_BATCH_FILES + 1,
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .await?;
        let digest = |bytes: Vec<u8>| -> Result<[u8; 32], BatchReceiptError> {
            bytes
                .try_into()
                .map_err(|_| BatchReceiptError::CorruptReceipt)
        };
        if rows.is_empty() || rows.len() > MAX_BATCH_FILES {
            return Err(BatchReceiptError::CorruptReceipt);
        }
        let mut files = Vec::with_capacity(rows.len());
        for (position, (index, path, base, candidate, written)) in rows.into_iter().enumerate() {
            if index != position as i64 {
                return Err(BatchReceiptError::CorruptReceipt);
            }
            files.push(BatchFile {
                path,
                base: base.map(digest).transpose()?,
                candidate: digest(candidate)?,
                written: written == 1,
            });
        }
        Ok(Some(BatchReceipt {
            request_digest: digest(request_digest)?,
            settled: state == "settled",
            files,
        }))
    }
}

/// Namespaces Application mechanics operations by kind and a fixed-size hash of the operation id.
fn mechanics(
    kind: &str,
    operation_id: &str,
    index: Option<usize>,
) -> Result<OperationId, StoreError> {
    let hash = blake3::hash(operation_id.as_bytes()).to_hex();
    OperationId::new(match index {
        Some(index) => format!("changes-batch-{kind}-{hash}-{index}"),
        None => format!("changes-batch-{kind}-{hash}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::StoreConfig;
    use std::time::Duration;

    /// Finite Store settings suitable for receipt and restart tests.
    fn config() -> StoreConfig {
        StoreConfig {
            queue_capacity: 16,
            busy_timeout: Duration::from_millis(100),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 64,
        }
    }

    /// Two files: an existing one and one to create.
    fn files() -> Vec<BatchFile> {
        vec![
            BatchFile {
                path: "src/a.rs".into(),
                base: Some([1; 32]),
                candidate: [2; 32],
                written: false,
            },
            BatchFile {
                path: "src/new.rs".into(),
                base: None,
                candidate: [3; 32],
                written: false,
            },
        ]
    }

    #[tokio::test]
    async fn receipts_record_files_marks_and_settlement_across_restart() {
        let path = crate::scratch::ScratchDatabase::new("changes-batch-restart");
        {
            let store = Store::open(&path, config()).expect("store");
            let receipts = BatchReceiptStore::new(&store);
            assert!(matches!(
                receipts.install_schema().await.expect("schema"),
                MigrationAdmission::Applied { .. }
            ));
            assert_eq!(receipts.load("op").await.expect("load"), None);
            assert_eq!(
                receipts
                    .prepare("op", [9; 32], &files())
                    .await
                    .expect("prepare"),
                BatchAdmission::Prepared
            );
            receipts.mark_written("op", 0).await.expect("mark");
            receipts.mark_written("op", 0).await.expect("mark twice");
        }
        let store = Store::open(&path, config()).expect("reopened");
        let receipts = BatchReceiptStore::new(&store);
        assert!(matches!(
            receipts.install_schema().await.expect("schema"),
            MigrationAdmission::AlreadyApplied { .. }
        ));
        let mut expected = files();
        expected[0].written = true;
        let existing = BatchReceipt {
            request_digest: [9; 32],
            settled: false,
            files: expected.clone(),
        };
        assert_eq!(
            receipts.load("op").await.expect("load"),
            Some(existing.clone())
        );
        assert_eq!(
            receipts
                .prepare("op", [9; 32], &files())
                .await
                .expect("again"),
            BatchAdmission::Existing(existing.clone()),
            "a retry finds the receipt and its written file"
        );
        assert_eq!(
            receipts
                .prepare("op", [7; 32], &files())
                .await
                .expect("other"),
            BatchAdmission::Existing(existing),
            "a different request sees the stored digest and conflicts"
        );
        receipts.mark_written("op", 1).await.expect("mark");
        receipts.settle("op").await.expect("settle");
        let settled = receipts.load("op").await.expect("load").expect("row");
        assert!(settled.settled && settled.files.iter().all(|file| file.written));
    }

    #[tokio::test]
    async fn prepare_refuses_empty_and_oversized_requests() {
        let path = crate::scratch::ScratchDatabase::new("changes-batch-bounds");
        let store = Store::open(&path, config()).expect("store");
        let receipts = BatchReceiptStore::new(&store);
        receipts.install_schema().await.expect("schema");
        assert_eq!(
            receipts.prepare("op", [0; 32], &[]).await,
            Err(BatchReceiptError::InvalidRequest)
        );
        let many = vec![files()[0].clone(); MAX_BATCH_FILES + 1];
        assert_eq!(
            receipts.prepare("op", [0; 32], &many).await,
            Err(BatchReceiptError::InvalidRequest)
        );
        assert_eq!(receipts.load("op").await.expect("load"), None);
    }
}
