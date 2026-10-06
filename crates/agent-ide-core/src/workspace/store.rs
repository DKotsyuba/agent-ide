//! Workspace-owned schema admission and source-observation persistence.

use super::{
    authority::WorktreeRef,
    observation::{
        ObservationError, ObservationRef, ObservedState, SourceBytes, SourceChange, SourceCoverage,
        SourceObservation, SourceReadLimits, SourceRevision, read_authorized_source,
        valid_relative_path,
    },
};
use crate::app::store::{
    DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey, OperationId,
    Store as ApplicationStore, StoreError, TrustedUpSql, UntrackedOutcome,
};
use rusqlite::{OptionalExtension, params};
use std::{os::unix::ffi::OsStrExt, path::PathBuf};

/// Stable Application migration namespace owned by Workspace.
const DOMAIN: &str = "workspace";
/// Stable Workspace-owned schema identity.
const KEY: &str = "source_observations_v1";
/// Trusted, immutable v0.1 Workspace schema.
const SQL: &str = "CREATE TABLE workspace_source_observations (
 worktree_id TEXT NOT NULL, incarnation INTEGER NOT NULL, authority_epoch INTEGER NOT NULL,
 source_sequence INTEGER NOT NULL, operation_id TEXT NOT NULL UNIQUE, observation_reference TEXT NOT NULL,
 relative_path BLOB NOT NULL, byte_digest BLOB, byte_length INTEGER, source_revision TEXT NOT NULL,
 coverage TEXT NOT NULL, observed_state TEXT NOT NULL,
 PRIMARY KEY (worktree_id, incarnation, source_sequence),
 CHECK (incarnation > 0), CHECK (authority_epoch > 0), CHECK (source_sequence > 0),
 CHECK ((observed_state = 'present' AND byte_digest IS NOT NULL AND byte_length IS NOT NULL)
 OR (observed_state = 'missing' AND byte_digest IS NULL AND byte_length IS NULL)))";

/// Carries validated source facts before durable persistence assigns a source sequence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationDraft {
    /// Identity and incarnation that own the monotonic sequence.
    worktree: WorktreeRef,
    /// Authority generation that scoped collection.
    authority_epoch: u64,
    /// Stable receipt identifier reused for retries.
    operation: OperationId,
    /// Opaque source-observation correlation reference.
    reference: ObservationRef,
    /// Raw relative Unix source pathname.
    path: PathBuf,
    /// Digest and length only for present source bytes.
    bytes: Option<SourceBytes>,
    /// Opaque source revision, distinct from Git and task revisions.
    revision: SourceRevision,
    /// Explicit collection completeness.
    coverage: SourceCoverage,
    /// Present or missing registered-path state.
    state: ObservedState,
}

impl ObservationDraft {
    /// Builds a present observation draft from bounded source bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        worktree: WorktreeRef,
        authority_epoch: u64,
        operation: OperationId,
        reference: ObservationRef,
        path: PathBuf,
        bytes: SourceBytes,
        revision: SourceRevision,
        coverage: SourceCoverage,
    ) -> Result<Self, ObservationError> {
        Self::build(
            worktree,
            authority_epoch,
            operation,
            reference,
            path,
            Some(bytes),
            revision,
            coverage,
            ObservedState::Present,
        )
    }
    /// Builds a missing-path draft without claiming closure or cache retirement.
    pub fn missing(
        worktree: WorktreeRef,
        authority_epoch: u64,
        operation: OperationId,
        reference: ObservationRef,
        path: PathBuf,
        revision: SourceRevision,
        coverage: SourceCoverage,
    ) -> Result<Self, ObservationError> {
        Self::build(
            worktree,
            authority_epoch,
            operation,
            reference,
            path,
            None,
            revision,
            coverage,
            ObservedState::Missing,
        )
    }
    /// Enforces the input invariants independent of durable sequence allocation.
    #[allow(clippy::too_many_arguments)]
    fn build(
        worktree: WorktreeRef,
        authority_epoch: u64,
        operation: OperationId,
        reference: ObservationRef,
        path: PathBuf,
        bytes: Option<SourceBytes>,
        revision: SourceRevision,
        coverage: SourceCoverage,
        state: ObservedState,
    ) -> Result<Self, ObservationError> {
        if authority_epoch == 0
            || !valid_relative_path(&path)
            || (matches!(state, ObservedState::Present) != bytes.is_some())
        {
            return Err(ObservationError::InvalidObservation);
        }
        Ok(Self {
            worktree,
            authority_epoch,
            operation,
            reference,
            path,
            bytes,
            revision,
            coverage,
            state,
        })
    }
    /// Converts this committed draft to the externally typed observation.
    fn observed(self, sequence: u64) -> Result<SourceObservation, ObservationError> {
        SourceObservation::new(
            self.worktree,
            self.authority_epoch,
            sequence,
            self.reference,
            self.path,
            self.bytes,
            self.revision,
            self.coverage,
            self.state,
        )
    }
}

/// Reports durable insertion, idempotent prior insertion, or unresolved commit ambiguity.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ObservationAdmission {
    /// A new row committed atomically with its application receipt.
    Recorded(SourceObservation),
    /// A stable operation already committed, so it was never replayed.
    AlreadyRecorded,
    /// The stable operation identifies a different durable observation and cannot be replayed.
    Conflict,
    /// Receipt lookup cannot safely establish the operation outcome.
    OutcomeUnknown,
}

/// Classifies whether an observation is stale for its exact registered raw path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationFreshness {
    /// Complete coverage and the newest sequence for this exact path.
    Current,
    /// A later sequence exists for this exact path.
    Stale,
    /// Partial or unknown coverage is never current or clean.
    Incomplete,
}

/// Carries a complete observation only after Workspace matched it to the exact latest durable row.
/// Callers can retain and return this token but cannot construct one from raw or stale hints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentObservation(SourceObservation);

impl CurrentObservation {
    /// Returns the exact durable observation certified current when this token was minted.
    /// Later persistence can supersede it, so each new collection request must obtain a fresh token.
    pub fn observation(&self) -> &SourceObservation {
        &self.0
    }

    /// Transfers the certified observation into the bounded snapshot collector.
    pub(crate) fn into_observation(self) -> SourceObservation {
        self.0
    }
}

/// Supplies one explicit registered path for bounded polling reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredPathRequest {
    /// Worktree identity authorized for the one path read.
    worktree: WorktreeRef,
    /// Authority generation that scoped this read.
    authority_epoch: u64,
    /// Stable operation identifier for this observation.
    operation: OperationId,
    /// Opaque observation reference.
    reference: ObservationRef,
    /// Exact registered path; it is never expanded to a scan.
    path: PathBuf,
    /// Distinct opaque source revision.
    revision: SourceRevision,
    /// Explicit completeness state.
    coverage: SourceCoverage,
    /// Native reader bounds.
    limits: SourceReadLimits,
}

impl RegisteredPathRequest {
    /// Validates one exact registered path request.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        worktree: WorktreeRef,
        authority_epoch: u64,
        operation: OperationId,
        reference: ObservationRef,
        path: PathBuf,
        revision: SourceRevision,
        coverage: SourceCoverage,
        limits: SourceReadLimits,
    ) -> Result<Self, ObservationError> {
        if authority_epoch == 0 || !valid_relative_path(&path) {
            return Err(ObservationError::InvalidObservation);
        }
        Ok(Self {
            worktree,
            authority_epoch,
            operation,
            reference,
            path,
            revision,
            coverage,
            limits,
        })
    }
    /// Reads exactly the registered path and turns missing state into an observation rather than closure.
    fn collect(self) -> Result<ObservationDraft, ObservationError> {
        match read_authorized_source(&self.worktree, &self.path, self.limits) {
            Ok(read) => ObservationDraft::present(
                self.worktree,
                self.authority_epoch,
                self.operation,
                self.reference,
                self.path,
                read.bytes().clone(),
                self.revision,
                self.coverage,
            ),
            Err(ObservationError::Missing) => ObservationDraft::missing(
                self.worktree,
                self.authority_epoch,
                self.operation,
                self.reference,
                self.path,
                self.revision,
                self.coverage,
            ),
            Err(error) => Err(error),
        }
    }
}

/// Reports bounded reconciliation without a filesystem watcher or worktree-lifecycle claim.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ReconciliationAdmission {
    /// A committed observation produced an optional later didOpen/didChange/didClose fact.
    Fact(Option<SourceChange>),
    /// The stable collection operation was already committed.
    AlreadyRecorded,
    /// The stable collection operation identifies different durable source facts.
    Conflict,
    /// The stable collection operation remains ambiguous.
    OutcomeUnknown,
}

/// Reports independent path reconciliation; v0.1 does not certify rename identity from caller hints.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum RenameReconciliation {
    /// Reserved for future internally proven native identity continuity; v0.1 never emits this variant.
    Rename(SourceChange),
    /// Identity was not proven, so Workspace retains honest independent delete/create observations.
    DeleteAndCreate {
        /// Reconciliation result for the explicitly supplied old path.
        old: ReconciliationAdmission,
        /// Reconciliation result for the explicitly supplied new path.
        new: ReconciliationAdmission,
    },
}

/// Explains Application mechanics, source validation, and sequence-range failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceStoreError {
    /// Application mechanics failed or remain indeterminate.
    Application(StoreError),
    /// Source observation or native reading failed its bounded contract.
    Observation(ObservationError),
    /// SQLite signed integer storage cannot represent the supplied monotonic value.
    SequenceExhausted,
}
impl From<StoreError> for WorkspaceStoreError {
    /// Preserves Application error semantics.
    fn from(value: StoreError) -> Self {
        Self::Application(value)
    }
}
impl From<ObservationError> for WorkspaceStoreError {
    /// Preserves source observation error semantics.
    fn from(value: ObservationError) -> Self {
        Self::Observation(value)
    }
}

/// Provides Workspace interpretation atop Application-owned migration and transaction mechanics.
pub struct WorkspaceStore<'a> {
    /// The Application store that owns threads, receipt records, and SQLite transactions.
    application: &'a ApplicationStore,
}
impl<'a> WorkspaceStore<'a> {
    /// Binds Workspace persistence to one open Application Store.
    pub const fn new(application: &'a ApplicationStore) -> Self {
        Self { application }
    }
    /// Admits the immutable schema and, after ambiguity, performs only same-domain/key lookup.
    pub async fn install_schema(&self) -> Result<MigrationAdmission, WorkspaceStoreError> {
        let migration = migration()?;
        match self.application.admit_migration(migration.clone()).await? {
            MigrationAdmission::OutcomeUnknown { key } => Ok(self
                .application
                .migration_admission(migration.domain, key)
                .await?),
            admission => Ok(admission),
        }
    }
    /// Inserts exactly once, allocating a sequence in the same transaction as the observation row.
    pub async fn record(
        &self,
        draft: ObservationDraft,
    ) -> Result<ObservationAdmission, WorkspaceStoreError> {
        self.record_with_previous(draft)
            .await
            .map(|(admission, _)| admission)
    }

    /// Reads the exact latest same-path/epoch row and inserts its successor in one transaction.
    /// Returns previous facts only for a newly committed observation, never for replay or ambiguity.
    ///
    /// The write is receipt-free: an observation is local append-only bookkeeping with no external
    /// effect, its operation id is unique per daemon and source sequence and is never replayed, and
    /// its own row carries that id, so an ambiguous commit is reconciled from the row itself. A
    /// tracked receipt per read would exhaust the Application store's hard receipt cap (one per
    /// observed file version), after which every durable write of the daemon is refused.
    async fn record_with_previous(
        &self,
        draft: ObservationDraft,
    ) -> Result<(ObservationAdmission, Option<SourceObservation>), WorkspaceStoreError> {
        let operation = draft.operation.clone();

        let worktree_id = draft.worktree.id().to_owned();
        let incarnation = sqlite(draft.worktree.incarnation())?;
        let epoch = sqlite(draft.authority_epoch)?;
        let reference = draft.reference.as_str().to_owned();
        let path = draft.path.as_os_str().as_bytes().to_vec();
        let revision = draft.revision.as_str().to_owned();
        let coverage = draft.coverage.as_str().to_owned();
        let state = draft.state.as_str().to_owned();
        let (digest, length) = draft.bytes.as_ref().map_or((None, None), |bytes| {
            (Some(bytes.digest().to_vec()), Some(sqlite(bytes.length())))
        });
        let length = length.transpose()?;
        let result = self.application.execute_untracked(move |transaction| {
            // A row already carrying this operation id is the replay evidence the receipt used to
            // be: nothing is inserted and the caller classifies it from the row's facts.
            let recorded = transaction.query_row("SELECT 1 FROM workspace_source_observations WHERE operation_id = ?1", params![operation.as_str()], |_| Ok(())).optional()?;
            if recorded.is_some() {
                return Ok(None);
            }
            let previous = transaction.query_row("SELECT authority_epoch, source_sequence, observation_reference, byte_digest, byte_length, source_revision, coverage, observed_state FROM workspace_source_observations WHERE worktree_id = ?1 AND incarnation = ?2 AND relative_path = ?3 ORDER BY source_sequence DESC LIMIT 1", params![worktree_id, incarnation, path], |row| Ok(Row { epoch: row.get(0)?, sequence: row.get(1)?, reference: row.get(2)?, digest: row.get(3)?, length: row.get(4)?, revision: row.get(5)?, coverage: row.get(6)?, state: row.get(7)? })).optional()?;
            let latest: i64 = transaction.query_row("SELECT COALESCE(MAX(source_sequence), 0) FROM workspace_source_observations WHERE worktree_id = ?1 AND incarnation = ?2", params![worktree_id, incarnation], |row| row.get(0))?;
            let sequence = latest.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
            transaction.execute("INSERT INTO workspace_source_observations (worktree_id, incarnation, authority_epoch, source_sequence, operation_id, observation_reference, relative_path, byte_digest, byte_length, source_revision, coverage, observed_state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)", params![worktree_id, incarnation, epoch, sequence, operation.as_str(), reference, path, digest, length, revision, coverage, state])?;
            Ok(Some((sequence, previous)))
        }).await;
        match result {
            Ok(UntrackedOutcome::Committed(Some((sequence, previous)))) => {
                let previous = previous
                    .filter(|row| row.epoch == epoch)
                    .map(|row| row.observed(draft.worktree.clone(), draft.path.clone()))
                    .transpose()?;
                Ok((
                    ObservationAdmission::Recorded(
                        draft.observed(
                            u64::try_from(sequence)
                                .map_err(|_| WorkspaceStoreError::SequenceExhausted)?,
                        )?,
                    ),
                    previous,
                ))
            }
            Ok(UntrackedOutcome::Committed(None)) => self
                .duplicate_admission(&draft)
                .await
                .map(|admission| (admission, None)),
            // The deadline passed after admission: the row may still commit. Its presence is the
            // only evidence; an absent row stays unknown rather than a conflict.
            Ok(UntrackedOutcome::OutcomeUnknown) => match self.duplicate_admission(&draft).await? {
                ObservationAdmission::AlreadyRecorded => {
                    Ok((ObservationAdmission::AlreadyRecorded, None))
                }
                _ => Ok((ObservationAdmission::OutcomeUnknown, None)),
            },
            Err(error) => Err(error.into()),
        }
    }
    /// Confirms without allocating another finite receipt that a committed stable operation names
    /// these exact immutable source facts. Missing rows are conflicts; read ambiguity stays an error.
    async fn duplicate_admission(
        &self,
        draft: &ObservationDraft,
    ) -> Result<ObservationAdmission, WorkspaceStoreError> {
        let operation = draft.operation.as_str().to_owned();
        let worktree = draft.worktree.id().to_owned();
        let incarnation = sqlite(draft.worktree.incarnation())?;
        let epoch = sqlite(draft.authority_epoch)?;
        let reference = draft.reference.as_str().to_owned();
        let path = draft.path.as_os_str().as_bytes().to_vec();
        let revision = draft.revision.as_str().to_owned();
        let coverage = draft.coverage.as_str().to_owned();
        let state = draft.state.as_str().to_owned();
        let (digest, length) = draft.bytes.as_ref().map_or((None, None), |bytes| {
            (Some(bytes.digest().to_vec()), Some(sqlite(bytes.length())))
        });
        let length = length.transpose()?;
        let matches = self.application.read_one(
                "SELECT 1 FROM workspace_source_observations WHERE operation_id = ?1 AND worktree_id = ?2 AND incarnation = ?3 AND authority_epoch = ?4 AND observation_reference = ?5 AND relative_path = ?6 AND byte_digest IS ?7 AND byte_length IS ?8 AND source_revision = ?9 AND coverage = ?10 AND observed_state = ?11",
                vec![
                    operation.into(), worktree.into(), incarnation.into(), epoch.into(),
                    reference.into(), path.into(), digest.into(), length.into(), revision.into(),
                    coverage.into(), state.into(),
                ],
                |_| Ok(()),
            ).await?;
        Ok(if matches.is_some() {
            ObservationAdmission::AlreadyRecorded
        } else {
            ObservationAdmission::Conflict
        })
    }
    /// Reopens the latest row for one exact path without consuming a finite mechanics receipt.
    /// The legacy lookup identifier is ignored because this operation is read-only and non-replayed.
    pub async fn load_latest(
        &self,
        _lookup: OperationId,
        worktree: WorktreeRef,
        path: PathBuf,
    ) -> Result<Option<SourceObservation>, WorkspaceStoreError> {
        if !valid_relative_path(&path) {
            return Err(ObservationError::InvalidObservation.into());
        }
        let id = worktree.id().to_owned();
        let incarnation = sqlite(worktree.incarnation())?;
        let raw_path = path.as_os_str().as_bytes().to_vec();
        let row = self.application.read_one("SELECT authority_epoch, source_sequence, observation_reference, byte_digest, byte_length, source_revision, coverage, observed_state FROM workspace_source_observations WHERE worktree_id = ?1 AND incarnation = ?2 AND relative_path = ?3 ORDER BY source_sequence DESC LIMIT 1", vec![id.into(), incarnation.into(), raw_path.into()], |row| Ok(Row { epoch: row.get(0)?, sequence: row.get(1)?, reference: row.get(2)?, digest: row.get(3)?, length: row.get(4)?, revision: row.get(5)?, coverage: row.get(6)?, state: row.get(7)? })).await?;
        row.map(|row| row.observed(worktree, path)).transpose()
    }
    /// Determines freshness by exact equality with the latest persisted row for this raw path.
    /// Partial/unknown coverage and same-sequence observations with different identity are not current;
    /// the legacy lookup identifier is ignored because the comparison allocates no receipt.
    pub async fn freshness(
        &self,
        _lookup: OperationId,
        observation: &SourceObservation,
    ) -> Result<ObservationFreshness, WorkspaceStoreError> {
        if !observation.coverage().is_complete() {
            return Ok(ObservationFreshness::Incomplete);
        }
        let id = observation.worktree().id().to_owned();
        let incarnation = sqlite(observation.worktree().incarnation())?;
        let path = observation.path().as_os_str().as_bytes().to_vec();
        let newest = self.application.read_one("SELECT authority_epoch, source_sequence, observation_reference, byte_digest, byte_length, source_revision, coverage, observed_state FROM workspace_source_observations WHERE worktree_id = ?1 AND incarnation = ?2 AND relative_path = ?3 ORDER BY source_sequence DESC LIMIT 1", vec![id.into(), incarnation.into(), path.into()], |row| Ok(Row { epoch: row.get(0)?, sequence: row.get(1)?, reference: row.get(2)?, digest: row.get(3)?, length: row.get(4)?, revision: row.get(5)?, coverage: row.get(6)?, state: row.get(7)? })).await?
            .ok_or(ObservationError::CorruptPersistence)?
            .observed(observation.worktree().clone(), observation.path().to_path_buf())?;
        Ok(if newest == *observation {
            ObservationFreshness::Current
        } else {
            ObservationFreshness::Stale
        })
    }
    /// Mints a non-authorizing snapshot hint only for an exact current durable observation.
    /// Stale or incomplete observations return `None`; the read-only comparison consumes no receipt.
    pub async fn confirm_current(
        &self,
        lookup: OperationId,
        observation: SourceObservation,
    ) -> Result<Option<CurrentObservation>, WorkspaceStoreError> {
        Ok(matches!(
            self.freshness(lookup, &observation).await?,
            ObservationFreshness::Current
        )
        .then_some(CurrentObservation(observation)))
    }
    /// Reads and records one registered path, deriving facts from its atomically loaded latest row.
    /// The legacy `previous` hint is ignored; cross-path, stale, and cross-epoch hints confer no authority.
    // ponytail: polling covers only caller-registered paths; replace with an event-backed collector when missed edits require stronger delivery.
    pub async fn reconcile_registered_path(
        &self,
        _previous: Option<&SourceObservation>,
        request: RegisteredPathRequest,
    ) -> Result<ReconciliationAdmission, WorkspaceStoreError> {
        let (admission, previous) = self.record_with_previous(request.collect()?).await?;
        match admission {
            ObservationAdmission::Recorded(current) => Ok(ReconciliationAdmission::Fact(
                change_fact(previous.as_ref(), current),
            )),
            ObservationAdmission::AlreadyRecorded => Ok(ReconciliationAdmission::AlreadyRecorded),
            ObservationAdmission::Conflict => Ok(ReconciliationAdmission::Conflict),
            ObservationAdmission::OutcomeUnknown => Ok(ReconciliationAdmission::OutcomeUnknown),
        }
    }

    /// Reconciles old/new paths independently; a caller Boolean never proves native identity continuity.
    /// `identity_proven` is a legacy hint and is ignored; v0.1 returns conservative delete/create facts.
    pub async fn reconcile_rename(
        &self,
        old_previous: Option<&SourceObservation>,
        old_request: RegisteredPathRequest,
        new_previous: Option<&SourceObservation>,
        new_request: RegisteredPathRequest,
        _identity_proven: bool,
    ) -> Result<RenameReconciliation, WorkspaceStoreError> {
        let old = self
            .reconcile_registered_path(old_previous, old_request)
            .await?;
        let new = self
            .reconcile_registered_path(new_previous, new_request)
            .await?;
        Ok(RenameReconciliation::DeleteAndCreate { old, new })
    }
}

/// Carries the raw persisted fields until a caller supplies the authoritative worktree/path context.
struct Row {
    /// SQLite authority epoch.
    epoch: i64,
    /// SQLite source sequence.
    sequence: i64,
    /// Opaque persisted reference.
    reference: String,
    /// Optional present-file digest.
    digest: Option<Vec<u8>>,
    /// Optional present-file length.
    length: Option<i64>,
    /// Opaque source revision.
    revision: String,
    /// Coverage tag.
    coverage: String,
    /// State tag.
    state: String,
}
impl Row {
    /// Decodes one row conservatively, rejecting corrupt state/byte combinations.
    fn observed(
        self,
        worktree: WorktreeRef,
        path: PathBuf,
    ) -> Result<SourceObservation, WorkspaceStoreError> {
        let state = ObservedState::from_str(&self.state)?;
        let bytes = match (self.digest, self.length) {
            (Some(digest), Some(length)) if matches!(state, ObservedState::Present) => {
                Some(SourceBytes::from_persisted(digest, length)?)
            }
            (None, None) if matches!(state, ObservedState::Missing) => None,
            _ => return Err(ObservationError::CorruptPersistence.into()),
        };
        Ok(SourceObservation::new(
            worktree,
            u64::try_from(self.epoch).map_err(|_| ObservationError::CorruptPersistence)?,
            u64::try_from(self.sequence).map_err(|_| ObservationError::CorruptPersistence)?,
            ObservationRef::new(self.reference)?,
            path,
            bytes,
            SourceRevision::new(self.revision)?,
            SourceCoverage::from_str(&self.coverage),
            state,
        )?)
    }
}
/// Builds Workspace's one immutable trusted migration.
fn migration() -> Result<DomainMigration, WorkspaceStoreError> {
    let up_sql = TrustedUpSql::new(SQL)?;
    Ok(DomainMigration {
        domain: DomainName::new(DOMAIN)?,
        key: MigrationKey::new(KEY)?,
        expected_digest: MigrationDigest::from_sql(&up_sql),
        up_sql,
    })
}
/// Converts Workspace unsigned identifiers to SQLite's signed integer range.
fn sqlite(value: u64) -> Result<i64, WorkspaceStoreError> {
    i64::try_from(value).map_err(|_| WorkspaceStoreError::SequenceExhausted)
}
/// Emits an Intelligence-ready change fact without claiming a missing file closes a worktree.
fn change_fact(
    previous: Option<&SourceObservation>,
    current: SourceObservation,
) -> Option<SourceChange> {
    match (previous, current.state()) {
        (None, ObservedState::Present) => Some(SourceChange::Open(current)),
        (None, ObservedState::Missing) => None,
        (Some(previous), ObservedState::Missing)
            if matches!(previous.state(), ObservedState::Present) =>
        {
            Some(SourceChange::Close {
                previous: previous.clone(),
                missing: current,
            })
        }
        (Some(previous), ObservedState::Present)
            if matches!(previous.state(), ObservedState::Missing) =>
        {
            Some(SourceChange::Open(current))
        }
        (Some(previous), ObservedState::Present)
            if previous.bytes() != current.bytes()
                || previous.source_revision() != current.source_revision()
                || previous.coverage() != current.coverage() =>
        {
            Some(SourceChange::Change {
                previous: previous.clone(),
                current,
            })
        }
        _ => None,
    }
}
