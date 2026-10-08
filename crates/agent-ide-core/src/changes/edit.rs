//! Durable one-file edit receipts and pure public effect semantics.

use std::path::{Component, Path};

use rusqlite::{params, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    app::store::{
        DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey,
        OperationId, Store, StoreError, StoreOutcome, TrustedUpSql,
    },
    workspace::edit::{EditOutcome as WorkspaceOutcome, EditPostRead, MAX_EDIT_CONTENT_BYTES},
};

/// Maximum canonical JSON size of the four edit arguments, independent of enclosing transports.
pub const MAX_EDIT_ARGUMENT_BYTES: usize = 136 * 1024;

/// Stable Changes migration namespace.
const DOMAIN: &str = "changes";
/// Immutable migration key for v0.2 one-file receipts.
const KEY: &str = "single_file_edit_receipts_v1";
/// Changes-owned receipt schema; content and source paths never enter Application mechanics rows.
const SQL: &str = "CREATE TABLE changes_edit_receipts (
 operation_id TEXT PRIMARY KEY, request_digest BLOB NOT NULL, relative_path TEXT NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('prepared','settled')),
 outcome TEXT, post_source_ref TEXT,
 CHECK((state='prepared' AND outcome IS NULL AND post_source_ref IS NULL)
 OR (state='settled' AND outcome IS NOT NULL)))";

/// The canonical and complete `ide.edit` request, with no optional or extension fields.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditRequest {
    /// Stable bounded identifier used for durable duplicate and uncertainty correlation.
    pub operation_id: String,
    /// Relative UTF-8 target beneath the active worktree.
    pub path: String,
    /// Opaque same-binding completed-context reference resolved only by Assistance.
    pub source_ref: String,
    /// Full UTF-8 replacement content, bounded to 128 KiB.
    pub content: String,
}

impl EditRequest {
    /// Validates the exact request and its independent content and canonical-argument limits.
    ///
    /// The relative path rejects empty, absolute, dot, parent and platform-prefix components.
    /// Identifiers are nonempty and at most 128 UTF-8 bytes. The canonical JSON encoding of all
    /// four fields must fit 136 KiB even when the content itself fits 128 KiB.
    pub fn new(
        operation_id: impl Into<String>,
        path: impl Into<String>,
        source_ref: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<Self, EditRequestError> {
        let request = Self {
            operation_id: operation_id.into(),
            path: path.into(),
            source_ref: source_ref.into(),
            content: content.into(),
        };
        request.validate()?;
        Ok(request)
    }

    /// Revalidates a decoded request before persistence or dispatch.
    pub fn validate(&self) -> Result<(), EditRequestError> {
        if self.operation_id.is_empty()
            || self.operation_id.len() > 128
            || self.source_ref.is_empty()
            || self.source_ref.len() > 128
            || !valid_path(&self.path)
        {
            return Err(EditRequestError::InvalidArgument);
        }
        if self.content.len() > MAX_EDIT_CONTENT_BYTES {
            return Err(EditRequestError::ContentTooLarge);
        }
        // The wire argument object is bounded separately by the facade (`MAX_EDIT_ARGUMENT_BYTES`);
        // an internally spliced symbol edit legitimately carries a whole file here.
        Ok(())
    }

    /// Returns the canonical framed request digest used for exact duplicate comparison.
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = blake3::Hasher::new();
        hash.update(b"changes-edit-request-v1");
        for part in [
            self.operation_id.as_bytes(),
            self.path.as_bytes(),
            self.source_ref.as_bytes(),
            self.content.as_bytes(),
        ] {
            hash.update(&(part.len() as u64).to_le_bytes());
            hash.update(part);
        }
        *hash.finalize().as_bytes()
    }
}

/// Explains why an edit request is rejected before a receipt or filesystem dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditRequestError {
    /// An identifier or relative path violates the closed request schema.
    InvalidArgument,
    /// Full UTF-8 content exceeds 48 KiB.
    ContentTooLarge,
}

/// Closed public edit outcome tags owned by Changes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EditOutcome {
    /// A missing eligible final component was created.
    Created,
    /// A current regular file was replaced.
    Replaced,
    /// Requested bytes already matched and no write occurred.
    Unchanged,
    /// Source binding or current bytes did not match; no write occurred.
    StaleSource,
    /// The same operation identifier names different immutable request bytes.
    ConflictingDuplicate,
    /// Target type, containment or metadata policy refused before write.
    UnsafeTarget,
    /// Cancellation was observed before dispatch/effect.
    CancelledNoEffect,
    /// Deadline expiry was observed before dispatch/effect.
    DeadlineNoEffect,
    /// Capacity refusal occurred before dispatch/effect.
    CapacityNoEffect,
    /// A write may have occurred; callers must inspect the named path and never blind-retry.
    OutcomeUnknown,
    /// No Workspace effect was dispatched because the operation route was unavailable.
    UnavailableBeforeDispatch,
}

impl EditOutcome {
    /// Returns the stable SQLite and JSON tag for this closed result.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Replaced => "replaced",
            Self::Unchanged => "unchanged",
            Self::StaleSource => "stale_source",
            Self::ConflictingDuplicate => "conflicting_duplicate",
            Self::UnsafeTarget => "unsafe_target",
            Self::CancelledNoEffect => "cancelled_no_effect",
            Self::DeadlineNoEffect => "deadline_no_effect",
            Self::CapacityNoEffect => "capacity_no_effect",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::UnavailableBeforeDispatch => "unavailable_before_dispatch",
        }
    }

    /// Decodes only the closed durable vocabulary; unknown storage is never treated as success.
    fn from_str(value: &str) -> Option<Self> {
        Some(match value {
            "created" => Self::Created,
            "replaced" => Self::Replaced,
            "unchanged" => Self::Unchanged,
            "stale_source" => Self::StaleSource,
            "conflicting_duplicate" => Self::ConflictingDuplicate,
            "unsafe_target" => Self::UnsafeTarget,
            "cancelled_no_effect" => Self::CancelledNoEffect,
            "deadline_no_effect" => Self::DeadlineNoEffect,
            "capacity_no_effect" => Self::CapacityNoEffect,
            "outcome_unknown" => Self::OutcomeUnknown,
            "unavailable_before_dispatch" => Self::UnavailableBeforeDispatch,
            _ => return None,
        })
    }

    /// Returns whether this outcome requires an exact post-read source reference.
    pub const fn has_post_source(self) -> bool {
        matches!(self, Self::Created | Self::Replaced | Self::Unchanged)
    }
}

/// A bounded public result containing no source content or diagnostic text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EditResult {
    /// Stable operation identifier from the exact request.
    pub operation_id: String,
    /// Exact relative UTF-8 path from the exact request.
    pub path: String,
    /// Closed Changes outcome.
    pub outcome: EditOutcome,
    /// Post-read source reference for success, absent for every refusal or unknown result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
}

impl EditResult {
    /// Builds a bounded result while enforcing success/post-reference consistency.
    pub fn new(
        operation_id: String,
        path: String,
        outcome: EditOutcome,
        source_ref: Option<String>,
    ) -> Result<Self, EditReceiptError> {
        if operation_id.is_empty()
            || operation_id.len() > 128
            || !valid_path(&path)
            || outcome.has_post_source() != source_ref.is_some()
            || source_ref
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 128)
        {
            return Err(EditReceiptError::CorruptReceipt);
        }
        Ok(Self {
            operation_id,
            path,
            outcome,
            source_ref,
        })
    }

    /// Maps one typed Workspace result to the closed public vocabulary without filesystem inference.
    ///
    /// `post_source_ref` is called only for a known post-read success. If observation refresh cannot
    /// supply a bounded reference after a possible write, the result conservatively becomes unknown.
    pub fn from_workspace(
        request: &EditRequest,
        workspace: WorkspaceOutcome,
        post_source_ref: impl FnOnce(&EditPostRead) -> Option<String>,
    ) -> Self {
        let (outcome, source_ref) = match workspace {
            WorkspaceOutcome::Created(read) => {
                success(EditOutcome::Created, &read, post_source_ref)
            }
            WorkspaceOutcome::Replaced(read) => {
                success(EditOutcome::Replaced, &read, post_source_ref)
            }
            WorkspaceOutcome::Unchanged(read) => {
                success(EditOutcome::Unchanged, &read, post_source_ref)
            }
            WorkspaceOutcome::StaleSource => (EditOutcome::StaleSource, None),
            WorkspaceOutcome::UnsafeTarget => (EditOutcome::UnsafeTarget, None),
            WorkspaceOutcome::CancelledNoEffect => (EditOutcome::CancelledNoEffect, None),
            WorkspaceOutcome::DeadlineNoEffect => (EditOutcome::DeadlineNoEffect, None),
            WorkspaceOutcome::CapacityNoEffect => (EditOutcome::CapacityNoEffect, None),
            WorkspaceOutcome::OutcomeUnknown { .. } => (EditOutcome::OutcomeUnknown, None),
        };
        Self {
            operation_id: request.operation_id.clone(),
            path: request.path.clone(),
            outcome,
            source_ref,
        }
    }

    /// Returns exact known effects, or `None` when any target write remains possible but unproven.
    pub fn effects(&self) -> Option<EditEffects> {
        match self.outcome {
            EditOutcome::Created => Some(EditEffects::Created(self.path.clone())),
            EditOutcome::Replaced => Some(EditEffects::Replaced(self.path.clone())),
            EditOutcome::OutcomeUnknown => None,
            _ => Some(EditEffects::NoWrite),
        }
    }
}

/// Pure, exact filesystem effects derivable from a settled public result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditEffects {
    /// Exactly one missing final component was created at this path.
    Created(String),
    /// Exactly one existing regular target was replaced at this path.
    Replaced(String),
    /// The operation is known to have made zero target writes.
    NoWrite,
}

/// A newly committed prepared receipt that alone authorizes one Workspace dispatch.
#[derive(Debug)]
pub struct PreparedEdit {
    request: EditRequest,
    digest: [u8; 32],
}

impl PreparedEdit {
    /// Returns the exact validated request whose new durable prepare this token represents.
    pub fn request(&self) -> &EditRequest {
        &self.request
    }
}

/// Result of durable prepare and exact duplicate reconciliation.
#[derive(Debug)]
pub enum PrepareAdmission {
    /// This call durably prepared a new request and may dispatch it exactly once.
    Prepared(PreparedEdit),
    /// An exact duplicate already settled and returns its immutable result.
    Settled(EditResult),
    /// The operation identifier already names different immutable request bytes.
    ConflictingDuplicate(EditResult),
    /// An exact prepared receipt exists but may have dispatched; it must never be replayed.
    OutcomeUnknown(EditResult),
}

/// Explains unavailable persistence or corrupt receipt data without inventing filesystem effects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditReceiptError {
    /// Application mechanics refused or could not safely reconcile the transaction.
    Application(StoreError),
    /// Schema admission did not establish this immutable Changes migration.
    Migration(MigrationAdmission),
    /// A durable row violated the closed Changes receipt schema.
    CorruptReceipt,
}

impl From<StoreError> for EditReceiptError {
    /// Preserves Application mechanics while Changes retains semantic ownership.
    fn from(value: StoreError) -> Self {
        Self::Application(value)
    }
}

/// Provides Changes semantics over Application's bounded transaction and receipt mechanics.
pub struct EditReceiptStore<'a> {
    application: &'a Store,
}

impl<'a> EditReceiptStore<'a> {
    /// Binds edit receipts to one open Application Store without installing schema implicitly.
    pub const fn new(application: &'a Store) -> Self {
        Self { application }
    }

    /// Admits the immutable edit receipt schema and reconciles only the same migration key.
    pub async fn install_schema(&self) -> Result<MigrationAdmission, EditReceiptError> {
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

    /// Durably prepares a new exact request or reconciles an existing receipt without replay.
    pub async fn prepare(
        &self,
        request: EditRequest,
    ) -> Result<PrepareAdmission, EditReceiptError> {
        request
            .validate()
            .map_err(|_| EditReceiptError::CorruptReceipt)?;
        if let Some(row) = self.load(&request.operation_id).await? {
            return reconcile(request, row);
        }
        let digest = request.digest();
        let operation_id = request.operation_id.clone();
        let path = request.path.clone();
        let mechanics = mechanics_operation("prepare", &operation_id)?;
        let result = self
            .application
            .execute(mechanics.clone(), move |transaction| {
                transaction.execute(
                    "INSERT INTO changes_edit_receipts (operation_id,request_digest,relative_path,state,outcome,post_source_ref) VALUES (?1,?2,?3,'prepared',NULL,NULL)",
                    params![operation_id, digest.to_vec(), path],
                )?;
                Ok(())
            })
            .await;
        match result {
            Ok(()) => Ok(PrepareAdmission::Prepared(PreparedEdit { request, digest })),
            Err(StoreError::DuplicateOperation {
                existing: StoreOutcome::Committed,
            }) => reconcile_loaded(self, request).await,
            Err(StoreError::OutcomeUnknown { operation })
                if self.application.outcome(operation.clone()).await?
                    == StoreOutcome::Committed =>
            {
                reconcile_loaded(self, request).await
            }
            Err(
                StoreError::DuplicateOperation { .. }
                | StoreError::OutcomeUnknown { .. }
                | StoreError::ReceiptExpired,
            ) => Ok(PrepareAdmission::OutcomeUnknown(unknown(&request))),
            Err(error) => Err(error.into()),
        }
    }

    /// Settles one newly prepared token exactly once from a typed Workspace-derived result.
    ///
    /// The token is consumed by value. Ambiguous settlement is reconciled by read only; this method
    /// never dispatches Workspace and never reconstructs a new prepared token.
    pub async fn settle(
        &self,
        prepared: PreparedEdit,
        result: EditResult,
    ) -> Result<EditResult, EditReceiptError> {
        if prepared.request.operation_id != result.operation_id
            || prepared.request.path != result.path
            || result.outcome == EditOutcome::ConflictingDuplicate
        {
            return Err(EditReceiptError::CorruptReceipt);
        }
        let operation_id = result.operation_id.clone();
        let digest = prepared.digest;
        let outcome = result.outcome.as_str().to_owned();
        let source_ref = result.source_ref.clone();
        let mechanics = mechanics_operation("settle", &operation_id)?;
        let write = self.application.execute(mechanics, move |transaction| {
            let changed = transaction.execute(
                "UPDATE changes_edit_receipts SET state='settled',outcome=?1,post_source_ref=?2 WHERE operation_id=?3 AND request_digest=?4 AND state='prepared'",
                params![outcome, source_ref, operation_id, digest.to_vec()],
            )?;
            if changed != 1 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(())
        }).await;
        match write {
            Ok(()) => Ok(result),
            Err(StoreError::DuplicateOperation {
                existing: StoreOutcome::Committed,
            }) => self.settled_duplicate(&prepared.request, &result).await,
            Err(StoreError::OutcomeUnknown { operation })
                if self.application.outcome(operation.clone()).await?
                    == StoreOutcome::Committed =>
            {
                self.settled_duplicate(&prepared.request, &result).await
            }
            Err(
                StoreError::DuplicateOperation { .. }
                | StoreError::OutcomeUnknown { .. }
                | StoreError::ReceiptExpired,
            ) => Ok(unknown(&prepared.request)),
            Err(error) => Err(error.into()),
        }
    }

    /// Returns an exact settled duplicate or unknown without creating any dispatch token.
    async fn settled_duplicate(
        &self,
        request: &EditRequest,
        expected: &EditResult,
    ) -> Result<EditResult, EditReceiptError> {
        let admission = reconcile_loaded(self, request.clone()).await?;
        match admission {
            PrepareAdmission::Settled(actual) if &actual == expected => Ok(actual),
            PrepareAdmission::OutcomeUnknown(result) => Ok(result),
            _ => Err(EditReceiptError::CorruptReceipt),
        }
    }

    /// Reads one receipt without consuming an Application operation or interpreting target state.
    async fn load(&self, operation_id: &str) -> Result<Option<ReceiptRow>, EditReceiptError> {
        let operation_id = operation_id.to_owned();
        self.application
            .read_one(
                "SELECT request_digest,relative_path,state,outcome,post_source_ref FROM changes_edit_receipts WHERE operation_id=?1",
                vec![Value::Text(operation_id)],
                |row| {
                    Ok(ReceiptRow {
                        digest: row.get(0)?,
                        path: row.get(1)?,
                        state: row.get(2)?,
                        outcome: row.get(3)?,
                        post_source_ref: row.get(4)?,
                    })
                },
            )
            .await
            .map_err(Into::into)
    }
}

/// Decoded storage row awaiting closed-schema validation.
struct ReceiptRow {
    /// Exact canonical request digest.
    digest: Vec<u8>,
    /// Exact relative path stored for bounded recovery reporting.
    path: String,
    /// `prepared` or `settled` durable state.
    state: String,
    /// Closed settled outcome, absent only while prepared.
    outcome: Option<String>,
    /// Exact post-read source reference for a successful settlement.
    post_source_ref: Option<String>,
}

/// Loads and reconciles a receipt after Application reports committed or duplicate mechanics.
async fn reconcile_loaded(
    store: &EditReceiptStore<'_>,
    request: EditRequest,
) -> Result<PrepareAdmission, EditReceiptError> {
    let row = store
        .load(&request.operation_id)
        .await?
        .ok_or(EditReceiptError::CorruptReceipt)?;
    reconcile(request, row)
}

/// Applies exact duplicate, conflict and no-blind-replay rules to one durable row.
fn reconcile(request: EditRequest, row: ReceiptRow) -> Result<PrepareAdmission, EditReceiptError> {
    if row.path != request.path || row.digest.as_slice() != request.digest() {
        return Ok(PrepareAdmission::ConflictingDuplicate(EditResult {
            operation_id: request.operation_id,
            path: request.path,
            outcome: EditOutcome::ConflictingDuplicate,
            source_ref: None,
        }));
    }
    match (row.state.as_str(), row.outcome) {
        ("prepared", None) if row.post_source_ref.is_none() => {
            Ok(PrepareAdmission::OutcomeUnknown(unknown(&request)))
        }
        ("settled", Some(outcome)) => {
            let outcome =
                EditOutcome::from_str(&outcome).ok_or(EditReceiptError::CorruptReceipt)?;
            let result = EditResult::new(
                request.operation_id,
                request.path,
                outcome,
                row.post_source_ref,
            )?;
            Ok(PrepareAdmission::Settled(result))
        }
        _ => Err(EditReceiptError::CorruptReceipt),
    }
}

/// Converts a known post-read success or observation-refresh failure without losing write ambiguity.
fn success(
    outcome: EditOutcome,
    read: &EditPostRead,
    source_ref: impl FnOnce(&EditPostRead) -> Option<String>,
) -> (EditOutcome, Option<String>) {
    match source_ref(read) {
        Some(reference) if !reference.is_empty() && reference.len() <= 128 => {
            (outcome, Some(reference))
        }
        _ => (EditOutcome::OutcomeUnknown, None),
    }
}

/// Builds the only recovery result permitted for an existing prepared receipt.
fn unknown(request: &EditRequest) -> EditResult {
    EditResult {
        operation_id: request.operation_id.clone(),
        path: request.path.clone(),
        outcome: EditOutcome::OutcomeUnknown,
        source_ref: None,
    }
}

/// Namespaces Application mechanics operations while retaining only a fixed-size request hash.
fn mechanics_operation(kind: &str, operation_id: &str) -> Result<OperationId, StoreError> {
    OperationId::new(format!(
        "changes-edit-{kind}-{}",
        blake3::hash(operation_id.as_bytes()).to_hex()
    ))
}

/// Validates the contract's portable relative UTF-8 path without normalizing it.
fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= crate::workspace::observation::MAX_SOURCE_PATH_BYTES
        && !path.as_bytes().contains(&0)
        && path
            .as_bytes()
            .split(|byte| *byte == b'/')
            .all(|part| !part.is_empty() && part != b"." && part != b"..")
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{config::StoreConfig, store::MigrationAdmission};
    use std::{fs, time::Duration};

    /// Creates one isolated SQLite path beneath the process temporary directory.
    fn database(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "agent-ide-changes-edit-{tag}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = fs::remove_file(&path);
        path
    }

    /// Returns finite Store settings suitable for receipt and restart tests.
    fn config() -> StoreConfig {
        StoreConfig {
            queue_capacity: 16,
            busy_timeout: Duration::from_millis(100),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 64,
        }
    }

    /// Builds one small canonical request for duplicate tests.
    fn request(operation: &str, content: &str) -> EditRequest {
        EditRequest::new(operation, "src/a.rs", "context-ref", content).expect("request")
    }

    /// Proves exact duplicates settle identically while changed duplicates conflict durably.
    #[tokio::test]
    async fn prepared_and_settled_receipts_survive_restart_without_replay() {
        let path = database("restart");
        let store = Store::open(&path, config()).expect("store");
        let receipts = EditReceiptStore::new(&store);
        assert!(matches!(
            receipts.install_schema().await.expect("schema"),
            MigrationAdmission::Applied { .. }
        ));
        let prepared = match receipts
            .prepare(request("one", "new"))
            .await
            .expect("prepare")
        {
            PrepareAdmission::Prepared(prepared) => prepared,
            other => panic!("unexpected {other:?}"),
        };
        let settled = EditResult::new(
            "one".into(),
            "src/a.rs".into(),
            EditOutcome::Replaced,
            Some("post-ref".into()),
        )
        .expect("result");
        assert_eq!(
            receipts
                .settle(prepared, settled.clone())
                .await
                .expect("settle"),
            settled
        );
        drop(store);

        let reopened = Store::open(&path, config()).expect("reopen");
        let receipts = EditReceiptStore::new(&reopened);
        assert!(matches!(
            receipts.install_schema().await.expect("schema"),
            MigrationAdmission::AlreadyApplied { .. }
        ));
        assert!(matches!(
            receipts.prepare(request("one", "new")).await.expect("duplicate"),
            PrepareAdmission::Settled(result) if result == settled
        ));
        assert!(matches!(
            receipts.prepare(request("one", "different")).await.expect("conflict"),
            PrepareAdmission::ConflictingDuplicate(result)
                if result.outcome == EditOutcome::ConflictingDuplicate
        ));
        drop(reopened);
        let _ = fs::remove_file(path);
    }

    /// Proves a crash-like prepared receipt never recreates a dispatch token after restart.
    #[tokio::test]
    async fn prepared_recovery_is_unknown_and_never_blind_replays() {
        let path = database("prepared");
        let store = Store::open(&path, config()).expect("store");
        let receipts = EditReceiptStore::new(&store);
        receipts.install_schema().await.expect("schema");
        assert!(matches!(
            receipts
                .prepare(request("two", "new"))
                .await
                .expect("prepare"),
            PrepareAdmission::Prepared(_)
        ));
        drop(store);
        let reopened = Store::open(&path, config()).expect("reopen");
        let receipts = EditReceiptStore::new(&reopened);
        receipts.install_schema().await.expect("schema");
        assert!(matches!(
            receipts.prepare(request("two", "new")).await.expect("recover"),
            PrepareAdmission::OutcomeUnknown(result)
                if result.outcome == EditOutcome::OutcomeUnknown
                    && result.effects().is_none()
        ));
        drop(reopened);
        let _ = fs::remove_file(path);
    }

    /// Proves all public outcomes map to exact zero/one writes or explicit unknown effects.
    #[test]
    fn effects_are_pure_and_request_limits_are_independent() {
        let result = |outcome| EditResult {
            operation_id: "op".into(),
            path: "a.rs".into(),
            outcome,
            source_ref: outcome.has_post_source().then(|| "post".into()),
        };
        assert_eq!(
            result(EditOutcome::Created).effects(),
            Some(EditEffects::Created("a.rs".into()))
        );
        assert_eq!(
            result(EditOutcome::Replaced).effects(),
            Some(EditEffects::Replaced("a.rs".into()))
        );
        assert_eq!(
            result(EditOutcome::StaleSource).effects(),
            Some(EditEffects::NoWrite)
        );
        assert_eq!(result(EditOutcome::OutcomeUnknown).effects(), None);
        assert!(matches!(
            EditRequest::new(
                "op",
                "a.rs",
                "source",
                "x".repeat(MAX_EDIT_CONTENT_BYTES + 1)
            ),
            Err(EditRequestError::ContentTooLarge)
        ));
        assert!(matches!(
            EditRequest::new("op", "../a.rs", "source", "x"),
            Err(EditRequestError::InvalidArgument)
        ));
        assert!(matches!(
            EditRequest::new("op", "src//a.rs", "source", "x"),
            Err(EditRequestError::InvalidArgument)
        ));
    }
}
