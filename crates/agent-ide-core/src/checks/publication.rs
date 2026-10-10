//! Durable publication state for the authoritative accepted diagnostic view (Agent Bridge seam).
//!
//! The scheduler retains one current [`ProblemSnapshot`](crate::checks::ProblemSnapshot) per
//! `(worktree, language)` in memory; this module is the durable counterpart a future Bridge
//! publisher publishes from. Each accepted view is an immutable row keyed by stream and a
//! durable monotonic revision assigned inside the store transaction at acceptance — never
//! derived from `input_generation` or arrival order. Exactly one row per stream is marked
//! current; superseding it advances the pointer but never deletes or rewrites the superseded
//! row, so a pending publication whose Bridge acknowledgement was lost keeps its exact event
//! identity, hash and payload bytes and stays acknowledgeable until its custody is reconciled.
//! A suppressed transient or outdated completion changes nothing.
//!
//! Mechanics identity is deliberately not content-derived alone: a proven rolled-back attempt
//! (confirmed by the store's receipt) is retried under a fresh attempt discriminator, receipt
//! retirement never blocks unchanged-view recovery, and every ambiguous or duplicate settled
//! receipt is reconciled from the durable rows instead of being read as domain success. The
//! domain owns its SQL and transitions; [`Store`](crate::app::store::Store) owns the
//! connection, transaction boundary and mechanics receipts.

use std::num::NonZeroU64;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;
use rusqlite::params;
use rusqlite::types::Value;

use crate::app::store::{
    DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey, OperationId,
    Store, StoreError, StoreOutcome, TrustedUpSql,
};
use crate::checks::Language;
use crate::checks::scheduler::{CompletionDisposition, CompletionEvent};

/// Migration namespace for the view-publication domain.
const DOMAIN: &str = "bridge_views";
/// Stable identity of the single upward migration that creates the publication table.
const MIGRATION_KEY: &str = "view_publication_v1";
/// Prefix binding the frozen view serialization to a schema version, hashed with the view bytes.
const FROZEN_VIEW_PREFIX: &str = "agent-ide-view-v1:";
/// Bounded count of mechanics attempts one record call may consume; a proven no-effect
/// rollback or an untraceable settled receipt each consume one attempt.
const MAX_ATTEMPTS: usize = 3;

/// Creates the one table this domain owns: one immutable row per accepted revision of a
/// `(worktree, language)` stream, with the current row singled out by `disposition`.
const MIGRATION_SQL: &str = "CREATE TABLE IF NOT EXISTS bridge_view_publication(
    worktree TEXT NOT NULL,
    language TEXT NOT NULL,
    revision INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    view_hash BLOB NOT NULL,
    view_json TEXT NOT NULL,
    input_generation INTEGER NOT NULL,
    policy_generation INTEGER NOT NULL,
    policy_digest TEXT NOT NULL,
    accepted_ms INTEGER NOT NULL,
    custody TEXT NOT NULL,
    disposition TEXT NOT NULL,
    PRIMARY KEY(worktree,language,revision))";

/// Reads a stream's current publication row.
static CURRENT_SQL: &str = "SELECT revision,event_id,view_hash,view_json,input_generation,policy_generation,policy_digest,accepted_ms,custody FROM bridge_view_publication WHERE worktree=?1 AND language=?2 AND disposition='current'";

/// Reads one publication row by its stable retry identity, whatever its disposition.
static FIND_SQL: &str = "SELECT revision,event_id,view_hash,view_json,input_generation,policy_generation,policy_digest,accepted_ms,custody FROM bridge_view_publication WHERE worktree=?1 AND language=?2 AND event_id=?3";

/// Reads the newest publication row carrying one exact view hash, current or superseded.
static BY_HASH_SQL: &str = "SELECT revision,event_id,view_hash,view_json,input_generation,policy_generation,policy_digest,accepted_ms,custody FROM bridge_view_publication WHERE worktree=?1 AND language=?2 AND view_hash=?3 ORDER BY revision DESC LIMIT 1";

/// One durable authoritative-view publication row of a `(worktree, language)` stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewPublication {
    /// Stable retry identity of this publication; derived from stream and revision, so a retry
    /// after a lost reply reuses exactly this ID.
    pub event_id: String,
    /// Durable monotonic publication revision, assigned at acceptance; strictly increasing per
    /// stream across restarts.
    pub revision: NonZeroU64,
    /// BLAKE3 digest of the frozen view bytes (prefix included).
    pub view_hash: [u8; 32],
    /// The frozen view serialization retained for publication.
    pub view_json: String,
    /// Worktree input generation the view was produced against (context, not the revision).
    pub input_generation: u64,
    /// Policy generation that fenced the view's acceptance.
    pub policy_generation: u64,
    /// Policy digest in force at acceptance.
    pub policy_digest: String,
    /// Wall-clock acceptance time in milliseconds since the Unix epoch.
    pub accepted_ms: u64,
    /// Local record of Bridge custody for this view.
    pub custody: ViewCustody,
}

/// Local custody state of one published view.
///
/// `Pending` is the state after local acceptance; `Acknowledged` is set only by an explicit
/// local record of a Bridge custody acknowledgement, never by publishing or rendering.
/// Superseding a row never changes its custody: a superseded view whose acknowledgement was
/// lost stays pending and acknowledgeable until reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewCustody {
    /// Accepted locally; no local record of Bridge custody acknowledgement exists yet.
    Pending,
    /// The source recorded the Bridge custody acknowledgement locally.
    Acknowledged,
}

impl ViewCustody {
    /// Decodes the persisted custody string; anything unexpected is unreadable state.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "custody_ack" => Some(Self::Acknowledged),
            _ => None,
        }
    }
}

/// Outcome of recording one completion event against a stream's durable publication state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordOutcome {
    /// A changed view was published under a newly assigned revision.
    Advanced {
        /// The newly assigned revision.
        revision: NonZeroU64,
        /// Stable retry identity of the new publication.
        event_id: String,
    },
    /// The submitted view is already retained at this revision: an unchanged retry (read
    /// directly, or reconciled from a settled mechanics receipt after its reply or horizon was
    /// lost), or a settled attempt whose content a later revision has since superseded.
    Current {
        /// The revision the content is retained at.
        revision: NonZeroU64,
        /// Stable retry identity of that retained publication.
        event_id: String,
    },
    /// A transient completion the scheduler suppressed; the retained rows are untouched.
    SuppressedTransient,
    /// A completion whose input generation predates the retained view; rows are untouched.
    Outdated,
}

/// Result of one custody-acknowledgement request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustodyAck {
    /// The named publication's custody was recorded as acknowledged.
    Acknowledged,
    /// No row of this stream carries that event identity; nothing changed. Repeating the call
    /// with the same wrong identity stays `NoMatch`: a settled mechanics receipt is reconciled
    /// from the durable row, never read as domain success.
    NoMatch,
}

/// Why one publication-domain operation refused or failed.
#[derive(Debug, Clone, PartialEq)]
pub enum PublicationError {
    /// The underlying store mechanics refused or failed; no domain effect is inferred.
    Store(StoreError),
    /// The stored current row's event identity does not match its stream and revision, so the
    /// row is not this domain's derivable state; it is never overwritten silently.
    Conflict {
        /// The stored event ID that failed derivation.
        stored_event_id: String,
    },
    /// The frozen view could not be serialized.
    Serialization(String),
}

impl From<StoreError> for PublicationError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

impl std::fmt::Display for PublicationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "view publication store failed: {error}"),
            Self::Conflict { stored_event_id } => write!(
                formatter,
                "stored view publication {stored_event_id} does not match its revision"
            ),
            Self::Serialization(message) => {
                write!(formatter, "view serialization failed: {message}")
            }
        }
    }
}

impl std::error::Error for PublicationError {}

/// Durable publication state for accepted diagnostic views, owned by one Application [`Store`].
///
/// Open once per daemon (after the store) and keep it for the daemon's lifetime; the store's
/// single owner thread serializes every method, so revision assignment is totally ordered per
/// stream.
#[derive(Clone, Copy)]
pub struct ViewPublicationStore<'a> {
    /// Application-owned SQLite connection and bounded submission mechanics.
    store: &'a Store,
}

impl<'a> ViewPublicationStore<'a> {
    /// Admits the domain migration and returns the ready store.
    ///
    /// An ambiguous migration outcome is reconciled by its key before this returns, so an
    /// `Err` never leaves the schema state unknown.
    pub async fn open(store: &'a Store) -> Result<Self, PublicationError> {
        let sql = TrustedUpSql::new(MIGRATION_SQL)?;
        let domain = DomainName::new(DOMAIN)?;
        let key = MigrationKey::new(MIGRATION_KEY)?;
        let migration = DomainMigration {
            domain: domain.clone(),
            key: key.clone(),
            expected_digest: MigrationDigest::from_sql(&sql),
            up_sql: sql,
        };
        let admission = match store.admit_migration(migration).await? {
            MigrationAdmission::OutcomeUnknown { .. } => {
                store.migration_admission(domain, key).await?
            }
            value => value,
        };
        if !matches!(
            admission,
            MigrationAdmission::Applied { .. } | MigrationAdmission::AlreadyApplied { .. }
        ) {
            return Err(PublicationError::Store(StoreError::MigrationRolledBack));
        }
        Ok(Self { store })
    }

    /// Records one completion event against its stream's durable publication state.
    ///
    /// Suppressed-transient and outdated events change nothing and report that. A stored event
    /// whose frozen view equals the stream's current one reports [`RecordOutcome::Current`]
    /// straight from the durable row, without consuming any mechanics receipt, so receipt
    /// retirement can never block unchanged recovery. A changed view inserts a new immutable
    /// row under `current + 1` with a fresh derived event ID and marks the previous current row
    /// superseded — its bytes, identity and custody are left intact. Mechanics attempts carry a
    /// bounded attempt discriminator: a proven rolled-back receipt is retried under the next
    /// attempt, while a duplicate, ambiguous-but-committed or retired receipt is reconciled from
    /// the durable rows (current match, retained-anywhere match, else treated as no-effect and
    /// retried). A stored current row whose event ID does not match its revision is a conflict
    /// and is left untouched.
    pub async fn record(&self, event: &CompletionEvent) -> Result<RecordOutcome, PublicationError> {
        match event.disposition {
            CompletionDisposition::TransientSuppressed { .. } => {
                return Ok(RecordOutcome::SuppressedTransient);
            }
            CompletionDisposition::Outdated => return Ok(RecordOutcome::Outdated),
            CompletionDisposition::Stored => {}
        }
        let view_json = serde_json::to_string(&event.snapshot)
            .map_err(|error| PublicationError::Serialization(error.to_string()))?;
        let view_hash = frozen_view_hash(&view_json);
        let worktree = event.worktree.to_string_lossy().into_owned();
        let language = event.snapshot.language.as_str().to_owned();
        let stream = stream_id(&worktree, &language);
        // Fast path: an unchanged current view needs no tracked mechanics at all, so neither a
        // duplicate receipt nor a retired one can stand between recovery and the durable row.
        if let Some(row) = self
            .current(&event.worktree, event.snapshot.language)
            .await?
            .filter(|row| row.view_hash == view_hash)
        {
            return Ok(RecordOutcome::Current {
                revision: row.revision,
                event_id: row.event_id,
            });
        }
        let input_generation = event.snapshot.input_generation;
        let policy_generation = event.policy_generation;
        let mut attempt = 0usize;
        let mut executions_left = MAX_ATTEMPTS;
        loop {
            if executions_left == 0 {
                return Err(PublicationError::Store(StoreError::RolledBack));
            }
            let worktree = worktree.clone();
            let language = language.clone();
            let view_json = view_json.clone();
            let stream = stream.clone();
            let policy_digest = event.policy_digest.clone();
            let operation = OperationId::new(format!(
                "bridge-view|{}|{}|{attempt}",
                &stream[..32],
                hex_prefix(&view_hash, 32)
            ))?;
            let result = self.store.execute(operation, move |tx| {
                let row = tx
                    .query_row(
                        "SELECT revision,event_id,view_hash FROM bridge_view_publication WHERE worktree=?1 AND language=?2 AND disposition='current'",
                        params![worktree, language],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Vec<u8>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let pending = match row {
                    None => Pending::Insert { revision: 1 },
                    Some((stored_revision, stored_event_id, stored_hash)) => {
                        let revision = u64::try_from(stored_revision)
                            .ok()
                            .filter(|value| *value > 0)
                            .ok_or(rusqlite::Error::InvalidQuery)?;
                        if stored_event_id != derive_event_id(&stream, revision) {
                            return Ok(Err(stored_event_id));
                        }
                        if stored_hash.as_slice() == view_hash {
                            Pending::Unchanged {
                                revision,
                                event_id: stored_event_id,
                            }
                        } else {
                            Pending::Advance {
                                revision: revision
                                    .checked_add(1)
                                    .ok_or(rusqlite::Error::InvalidQuery)?,
                                previous: revision,
                            }
                        }
                    }
                };
                let outcome = match pending {
                    Pending::Insert { revision } => {
                        let event_id = derive_event_id(&stream, revision);
                        insert_current(
                            tx,
                            &worktree,
                            &language,
                            revision,
                            &event_id,
                            &view_hash,
                            &view_json,
                            input_generation,
                            policy_generation,
                            &policy_digest,
                        )?;
                        (revision, event_id, true)
                    }
                    Pending::Unchanged { revision, event_id } => (revision, event_id, false),
                    Pending::Advance { revision, previous } => {
                        // The superseded row keeps its bytes, identity and custody untouched:
                        // only its disposition changes, so a possibly-handed pending
                        // publication stays exactly reconcilable after the view advances.
                        tx.execute(
                            "UPDATE bridge_view_publication SET disposition='superseded' WHERE worktree=?1 AND language=?2 AND revision=?3 AND disposition='current'",
                            params![worktree, language, previous as i64],
                        )?;
                        let event_id = derive_event_id(&stream, revision);
                        insert_current(
                            tx,
                            &worktree,
                            &language,
                            revision,
                            &event_id,
                            &view_hash,
                            &view_json,
                            input_generation,
                            policy_generation,
                            &policy_digest,
                        )?;
                        (revision, event_id, true)
                    }
                };
                Ok(Ok(outcome))
            }).await;
            let (revision, event_id, advanced) = match result {
                Ok(value) => value
                    .map_err(|stored_event_id| PublicationError::Conflict { stored_event_id })?,
                Err(error) => match self.settled(error).await? {
                    Settlement::ProvenNoEffect { charged } => {
                        if charged {
                            executions_left -= 1;
                        }
                        attempt += 1;
                        continue;
                    }
                    Settlement::Reconcile => {
                        if let Some(outcome) = self
                            .reconcile(&event.worktree, event.snapshot.language, &view_hash)
                            .await?
                        {
                            return Ok(outcome);
                        }
                        // The settled attempt left no trace of this content, so it took no
                        // effect; the next attempt discriminator retries the same view.
                        executions_left -= 1;
                        attempt += 1;
                        continue;
                    }
                    Settlement::Failed(error) => return Err(error.into()),
                },
            };
            let revision = NonZeroU64::new(revision).ok_or(StoreError::InvalidOperationId)?;
            return Ok(if advanced {
                RecordOutcome::Advanced { revision, event_id }
            } else {
                RecordOutcome::Current { revision, event_id }
            });
        }
    }

    /// Reads a stream's current publication, or `None` when the stream has none.
    pub async fn current(
        &self,
        worktree: &Path,
        language: Language,
    ) -> Result<Option<ViewPublication>, PublicationError> {
        let row = self
            .store
            .read_one(
                CURRENT_SQL,
                vec![
                    Value::Text(worktree.to_string_lossy().into_owned()),
                    Value::Text(language.as_str().to_owned()),
                ],
                decode_row,
            )
            .await?;
        Ok(row.map(|decoded| decoded.0))
    }

    /// Reads one publication of a stream by its stable retry identity, current or superseded.
    ///
    /// A superseded publication keeps its exact event identity, hash and frozen view bytes, so
    /// a late custody acknowledgement or reconciliation still addresses it.
    pub async fn find(
        &self,
        worktree: &Path,
        language: Language,
        event_id: &str,
    ) -> Result<Option<ViewPublication>, PublicationError> {
        let row = self
            .store
            .read_one(
                FIND_SQL,
                vec![
                    Value::Text(worktree.to_string_lossy().into_owned()),
                    Value::Text(language.as_str().to_owned()),
                    Value::Text(event_id.to_owned()),
                ],
                decode_row,
            )
            .await?;
        Ok(row.map(|decoded| decoded.0))
    }

    /// Records the local acknowledgement of Bridge custody for one named publication.
    ///
    /// Only the exact `event_id` transitions, whatever its disposition; a stream with no such
    /// event reports [`CustodyAck::NoMatch`] and changes nothing. Repeating the call is
    /// idempotent for the right identity and stays `NoMatch` for a wrong one: a settled
    /// mechanics receipt is reconciled from the durable row, because a committed transaction is
    /// mechanics success, not the domain result.
    pub async fn acknowledge_custody(
        &self,
        worktree: &Path,
        language: Language,
        event_id: &str,
    ) -> Result<CustodyAck, PublicationError> {
        let stream = stream_id(&worktree.to_string_lossy(), language.as_str());
        let mut attempt = 0usize;
        let mut executions_left = MAX_ATTEMPTS;
        loop {
            if executions_left == 0 {
                return Err(PublicationError::Store(StoreError::RolledBack));
            }
            let operation = OperationId::new(format!(
                "bridge-view-ack|{}|{event_id}|{attempt}",
                &stream[..32]
            ))?;
            let stored_worktree = worktree.to_string_lossy().into_owned();
            let stored_language = language.as_str().to_owned();
            let stored_event = event_id.to_owned();
            let result = self.store.execute(operation, move |tx| {
                let changed = tx.execute(
                    "UPDATE bridge_view_publication SET custody='custody_ack' WHERE worktree=?1 AND language=?2 AND event_id=?3",
                    params![stored_worktree, stored_language, stored_event],
                )?;
                Ok(changed == 1)
            }).await;
            match result {
                Ok(acknowledged) => {
                    return Ok(if acknowledged {
                        CustodyAck::Acknowledged
                    } else {
                        CustodyAck::NoMatch
                    });
                }
                Err(error) => match self.settled(error).await? {
                    Settlement::ProvenNoEffect { charged } => {
                        if charged {
                            executions_left -= 1;
                        }
                        attempt += 1;
                    }
                    Settlement::Reconcile => {
                        // The committed transaction's domain result is re-derived from the
                        // row: only a row that actually carries this event and an acknowledged
                        // custody counts, so a repeated wrong identity stays NoMatch.
                        return Ok(match self.find(worktree, language, event_id).await? {
                            Some(row) if row.custody == ViewCustody::Acknowledged => {
                                CustodyAck::Acknowledged
                            }
                            _ => CustodyAck::NoMatch,
                        });
                    }
                    Settlement::Failed(error) => return Err(error.into()),
                },
            }
        }
    }

    /// Classifies one refused mechanics attempt for retry, reconciliation or failure.
    ///
    /// [`Settlement::ProvenNoEffect`] covers every receipt the store confirms rolled back;
    /// `charged` distinguishes a fresh execution (which consumed one bounded attempt) from a
    /// duplicate rolled-back receipt answered without executing. [`Settlement::Reconcile`]
    /// covers a duplicate committed receipt, an ambiguous attempt whose receipt resolves to
    /// committed, and a retired receipt: all settled, none proving the domain result.
    /// Everything else is a hard failure.
    async fn settled(&self, error: StoreError) -> Result<Settlement, PublicationError> {
        Ok(match error {
            StoreError::RolledBack => Settlement::ProvenNoEffect { charged: true },
            StoreError::DuplicateOperation {
                existing: StoreOutcome::RolledBack,
            } => Settlement::ProvenNoEffect { charged: false },
            StoreError::DuplicateOperation {
                existing: StoreOutcome::Committed,
            } => Settlement::Reconcile,
            StoreError::ReceiptExpired => Settlement::Reconcile,
            StoreError::OutcomeUnknown { operation } => {
                match self.store.outcome(operation.clone()).await? {
                    StoreOutcome::Committed => Settlement::Reconcile,
                    StoreOutcome::RolledBack => Settlement::ProvenNoEffect { charged: true },
                    _ => return Err(StoreError::OutcomeUnknown { operation }.into()),
                }
            }
            error => Settlement::Failed(error),
        })
    }

    /// Reconciles a settled record attempt from the durable rows by submitted content.
    ///
    /// The current row matching the submitted hash means the content is the retained current
    /// view; any superseded row matching it means it was already published at that revision and
    /// a later revision has since replaced it, so the settled attempt needs no new write; no
    /// row at all means the settled attempt took no effect, reported as `None` so the caller
    /// may retry under a fresh attempt discriminator.
    async fn reconcile(
        &self,
        worktree: &Path,
        language: Language,
        view_hash: &[u8; 32],
    ) -> Result<Option<RecordOutcome>, PublicationError> {
        if let Some(row) = self.by_hash(worktree, language, view_hash).await? {
            return Ok(Some(RecordOutcome::Current {
                revision: row.revision,
                event_id: row.event_id,
            }));
        }
        Ok(None)
    }

    /// Reads the newest row of a stream carrying one exact view hash, if any.
    async fn by_hash(
        &self,
        worktree: &Path,
        language: Language,
        view_hash: &[u8; 32],
    ) -> Result<Option<ViewPublication>, PublicationError> {
        let row = self
            .store
            .read_one(
                BY_HASH_SQL,
                vec![
                    Value::Text(worktree.to_string_lossy().into_owned()),
                    Value::Text(language.as_str().to_owned()),
                    Value::Blob(view_hash.to_vec()),
                ],
                decode_row,
            )
            .await?;
        Ok(row.map(|decoded| decoded.0))
    }
}

/// What one refused mechanics attempt proves about its domain effect.
enum Settlement {
    /// The store confirms the transaction rolled back; the same view may be retried safely.
    /// `charged` names whether this refusal consumed one of the bounded real executions.
    ProvenNoEffect {
        /// Whether a fresh execution (not just a duplicate receipt) consumed an attempt.
        charged: bool,
    },
    /// The attempt settled without proving the domain result; reconcile from durable rows.
    Reconcile,
    /// A hard mechanics failure; surfaced unchanged.
    Failed(StoreError),
}

/// One transaction's planned write against a stream's publication rows.
enum Pending {
    /// The stream has no rows at all; the first revision becomes current.
    Insert {
        /// Revision to insert, always 1.
        revision: u64,
    },
    /// The current row already holds this exact view; no write.
    Unchanged {
        /// The retained revision.
        revision: u64,
        /// The retained event identity.
        event_id: String,
    },
    /// A changed view: mark the previous current row superseded and insert the next revision.
    Advance {
        /// The new revision to insert.
        revision: u64,
        /// The revision being superseded.
        previous: u64,
    },
}

/// Inserts one revision as the stream's current row with pending custody.
#[allow(clippy::too_many_arguments)]
fn insert_current(
    tx: &rusqlite::Transaction<'_>,
    worktree: &str,
    language: &str,
    revision: u64,
    event_id: &str,
    view_hash: &[u8; 32],
    view_json: &str,
    input_generation: u64,
    policy_generation: u64,
    policy_digest: &str,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO bridge_view_publication(worktree,language,revision,event_id,view_hash,view_json,input_generation,policy_generation,policy_digest,accepted_ms,custody,disposition) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'pending','current')",
        params![
            worktree,
            language,
            revision as i64,
            event_id,
            view_hash,
            view_json,
            input_generation as i64,
            policy_generation as i64,
            policy_digest,
            now_ms()
        ],
    )?;
    Ok(())
}

/// Typed row plus the columns the by-identity lookup needs; both readers decode identically.
type DecodedRow = (ViewPublication,);

/// Maps one stored row onto its typed value; unreadable values surface as decode failures.
fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DecodedRow> {
    let revision: i64 = row.get(0)?;
    let event_id: String = row.get(1)?;
    let hash: Vec<u8> = row.get(2)?;
    let view_json: String = row.get(3)?;
    let input_generation: i64 = row.get(4)?;
    let policy_generation: i64 = row.get(5)?;
    let policy_digest: String = row.get(6)?;
    let accepted_ms: i64 = row.get(7)?;
    let custody: String = row.get(8)?;
    let hash: [u8; 32] = hash
        .as_slice()
        .try_into()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    let revision =
        NonZeroU64::new(u64::try_from(revision).map_err(|_| rusqlite::Error::InvalidQuery)?)
            .ok_or(rusqlite::Error::InvalidQuery)?;
    let unsigned = |value: i64| u64::try_from(value).map_err(|_| rusqlite::Error::InvalidQuery);
    Ok((ViewPublication {
        event_id,
        revision,
        view_hash: hash,
        view_json,
        input_generation: unsigned(input_generation)?,
        policy_generation: unsigned(policy_generation)?,
        policy_digest,
        accepted_ms: unsigned(accepted_ms)?,
        custody: ViewCustody::parse(&custody).ok_or(rusqlite::Error::InvalidQuery)?,
    },))
}

/// Returns the stable stream identity binding one worktree and language.
///
/// The full 32-byte BLAKE3 digest in lowercase hex; truncated prefixes of it are used where a
/// bounded identifier (operation IDs) is required.
fn stream_id(worktree: &str, language: &str) -> String {
    hex_full(&blake3_hash_pair(worktree, language))
}

/// Derives the stable retry event identity for one stream at one revision.
fn derive_event_id(stream: &str, revision: u64) -> String {
    format!("view-{stream}-r{revision}")
}

/// Hashes the stream identity input.
fn blake3_hash_pair(worktree: &str, language: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(worktree.len() + language.len() + 1);
    input.extend_from_slice(worktree.as_bytes());
    input.push(0);
    input.extend_from_slice(language.as_bytes());
    *blake3::hash(&input).as_bytes()
}

/// Hashes the frozen view: the schema prefix and the exact view bytes together, so a change in
/// either is a different publication.
fn frozen_view_hash(view_json: &str) -> [u8; 32] {
    let mut input = Vec::with_capacity(FROZEN_VIEW_PREFIX.len() + view_json.len());
    input.extend_from_slice(FROZEN_VIEW_PREFIX.as_bytes());
    input.extend_from_slice(view_json.as_bytes());
    *blake3::hash(&input).as_bytes()
}

/// Lowercase hex of a full digest.
fn hex_full(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// First `len` lowercase hex characters of a digest.
fn hex_prefix(bytes: &[u8], len: usize) -> String {
    hex_full(bytes)[..len].to_owned()
}

/// Current wall-clock time in milliseconds since the Unix epoch, saturating at the range bound.
fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::StoreConfig;
    use crate::checks::scheduler::CompletionDisposition;
    use crate::checks::{CheckState, Problem, ProblemSnapshot};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Allocates a unique database pathname under the process temporary directory.
    fn database_path(id: &str) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "agent-ide-view-publication-{}-{}.sqlite",
            id,
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Small bounded owner-thread limits for isolated publication tests; the receipt capacity
    /// is deliberately tiny so retirement paths are exercised by unrelated traffic.
    fn config() -> StoreConfig {
        StoreConfig {
            queue_capacity: 4,
            busy_timeout: Duration::from_millis(200),
            request_deadline: Duration::from_secs(2),
            receipt_capacity: 8,
        }
    }

    /// Builds one stored completion event for `worktree` and `snapshot`.
    fn stored_event(worktree: &Path, snapshot: ProblemSnapshot) -> CompletionEvent {
        CompletionEvent {
            worktree: worktree.to_path_buf(),
            repository_key: "repository".to_owned(),
            policy_generation: 1,
            policy_digest: "policy-digest".to_owned(),
            snapshot,
            disposition: CompletionDisposition::Stored,
        }
    }

    /// One Ready snapshot carrying `errors` synthetic problems at input `generation`.
    fn ready(language: Language, problems: usize, generation: u64) -> ProblemSnapshot {
        ProblemSnapshot::from_problems(
            language,
            CheckState::Ready,
            (0..problems)
                .map(|index| {
                    Problem::new(
                        format!("src/file{index}.rs"),
                        1 + index as u32,
                        1,
                        crate::checks::Severity::Error,
                        None,
                        format!("synthetic problem {index}"),
                    )
                })
                .collect(),
            generation,
            12,
        )
    }

    /// Removes the database and WAL sidecars created solely by one test.
    fn remove_database(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    /// Revisions stay durable and monotonic across a restart, and an unchanged view retry
    /// returns the retained revision instead of a new one (A11/A18).
    #[tokio::test]
    async fn revision_monotonic_across_restart_and_unchanged_retry() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-a");
        let path = database_path("restart");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        let first = stored_event(worktree, ready(language, 0, 1));
        let second = stored_event(worktree, ready(language, 2, 2));
        let third = stored_event(worktree, ready(language, 1, 3));
        for (expected, event) in [(1, &first), (2, &second)] {
            match publications.record(event).await.unwrap() {
                RecordOutcome::Advanced { revision, .. } => {
                    assert_eq!(revision, NonZeroU64::new(expected).unwrap());
                }
                other => panic!("expected a new revision, got {other:?}"),
            }
        }
        drop(store);
        // Restart: the durable row, not memory, decides the next revision.
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        match publications.record(&second).await.unwrap() {
            RecordOutcome::Current { revision, .. } => {
                assert_eq!(revision, NonZeroU64::new(2).unwrap());
            }
            other => panic!("expected the unchanged view to stay current, got {other:?}"),
        }
        match publications.record(&third).await.unwrap() {
            RecordOutcome::Advanced { revision, .. } => {
                assert_eq!(revision, NonZeroU64::new(3).unwrap());
            }
            other => panic!("expected a new revision after a changed view, got {other:?}"),
        }
        let row = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.revision, NonZeroU64::new(3).unwrap());
        assert_eq!(row.input_generation, 3);
        assert_eq!(row.custody, ViewCustody::Pending);
        assert_eq!(
            row.view_json,
            serde_json::to_string(&third.snapshot).unwrap()
        );
        remove_database(&path);
    }

    /// A suppressed transient and an outdated completion never replace the retained view (A18).
    #[tokio::test]
    async fn transient_and_outdated_completions_leave_retained_view() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-b");
        let path = database_path("transient");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        let retained = ready(language, 1, 1);
        publications
            .record(&stored_event(worktree, retained.clone()))
            .await
            .unwrap();
        let fatal = CompletionEvent {
            worktree: worktree.to_path_buf(),
            repository_key: "repository".to_owned(),
            policy_generation: 1,
            policy_digest: "policy-digest".to_owned(),
            snapshot: ProblemSnapshot::unavailable(
                language,
                crate::checks::UnavailableReason::Fatal,
                2,
            ),
            disposition: CompletionDisposition::TransientSuppressed {
                retained: retained.clone(),
            },
        };
        assert_eq!(
            publications.record(&fatal).await.unwrap(),
            RecordOutcome::SuppressedTransient
        );
        let outdated = CompletionEvent {
            disposition: CompletionDisposition::Outdated,
            ..stored_event(worktree, ready(language, 9, 0))
        };
        assert_eq!(
            publications.record(&outdated).await.unwrap(),
            RecordOutcome::Outdated
        );
        let row = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.revision, NonZeroU64::new(1).unwrap());
        assert_eq!(row.view_json, serde_json::to_string(&retained).unwrap());
        remove_database(&path);
    }

    /// A replay of the same completion reconciles to the same identity, never a second
    /// publication of the same view under a new revision.
    #[tokio::test]
    async fn stable_retry_identity_replay_returns_current() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-c");
        let path = database_path("replay");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        let event = stored_event(worktree, ready(language, 1, 1));
        let first = publications.record(&event).await.unwrap();
        let second = publications.record(&event).await.unwrap();
        let (first_revision, first_event) = match first {
            RecordOutcome::Advanced { revision, event_id } => (revision, event_id),
            other => panic!("expected the first record to advance, got {other:?}"),
        };
        match second {
            RecordOutcome::Current { revision, event_id } => {
                assert_eq!(revision, first_revision);
                assert_eq!(event_id, first_event);
            }
            other => panic!("expected the replay to reconcile as current, got {other:?}"),
        }
        assert_eq!(
            publications
                .current(worktree, language)
                .await
                .unwrap()
                .unwrap()
                .revision,
            first_revision
        );
        remove_database(&path);
    }

    /// A stored current row whose event identity does not match its revision is a conflict,
    /// never a silent overwrite (A11 equal-revision conflict).
    #[tokio::test]
    async fn conflicting_event_identity_refuses_overwrite() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-d");
        let path = database_path("conflict");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        let stored_worktree = worktree.to_string_lossy().into_owned();
        let stored_language = language.as_str().to_owned();
        store
            .execute_untracked(move |tx| {
                tx.execute(
                    "INSERT INTO bridge_view_publication(worktree,language,revision,event_id,view_hash,view_json,input_generation,policy_generation,policy_digest,accepted_ms,custody,disposition) VALUES (?1,?2,1,'foreign-identity',?3,'{}',0,0,'',0,'pending','current')",
                    params![stored_worktree, stored_language, vec![0u8; 32]],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let error = publications
            .record(&stored_event(worktree, ready(language, 1, 1)))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            PublicationError::Conflict {
                stored_event_id: "foreign-identity".to_owned()
            }
        );
        let row = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.event_id, "foreign-identity");
        assert_eq!(row.view_hash, [0u8; 32]);
        remove_database(&path);
    }

    /// A superseded pending intent keeps its exact identity, hash and bytes and stays
    /// acknowledgeable after the current view advances (F05).
    #[tokio::test]
    async fn pending_intent_remains_with_exact_bytes_after_advance() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-e");
        let path = database_path("pending");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        publications
            .record(&stored_event(worktree, ready(language, 1, 1)))
            .await
            .unwrap();
        let earlier = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap();
        publications
            .record(&stored_event(worktree, ready(language, 2, 2)))
            .await
            .unwrap();
        let current = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, NonZeroU64::new(2).unwrap());
        assert_ne!(current.event_id, earlier.event_id);
        let retained = publications
            .find(worktree, language, &earlier.event_id)
            .await
            .unwrap()
            .expect("the superseded pending intent must remain addressable");
        assert_eq!(retained.revision, earlier.revision);
        assert_eq!(retained.view_hash, earlier.view_hash);
        assert_eq!(retained.view_json, earlier.view_json);
        assert_eq!(retained.custody, ViewCustody::Pending);
        assert_eq!(
            publications
                .acknowledge_custody(worktree, language, &earlier.event_id)
                .await
                .unwrap(),
            CustodyAck::Acknowledged
        );
        remove_database(&path);
    }

    /// A proven rolled-back mechanics attempt never poisons the identical publication retry;
    /// the retry lands at the same first revision (F06).
    #[tokio::test]
    async fn confirmed_rollback_allows_identical_publication() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-f");
        let path = database_path("rollback");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        store
            .execute_untracked(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER reject_publication BEFORE INSERT ON bridge_view_publication BEGIN SELECT RAISE(ABORT, 'synthetic rollback'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            publications
                .record(&stored_event(worktree, ready(language, 1, 1)))
                .await
                .is_err()
        );
        store
            .execute_untracked(|tx| {
                tx.execute_batch("DROP TRIGGER reject_publication")?;
                Ok(())
            })
            .await
            .unwrap();
        match publications
            .record(&stored_event(worktree, ready(language, 1, 1)))
            .await
            .unwrap()
        {
            RecordOutcome::Advanced { revision, .. } => {
                assert_eq!(revision, NonZeroU64::new(1).unwrap());
            }
            other => panic!("confirmed rollback poisoned the retry: {other:?}"),
        }
        remove_database(&path);
    }

    /// Retiring the mechanics receipt behind an unchanged view must not block durable-domain
    /// recovery: the same content still reports the retained revision (F06).
    #[tokio::test]
    async fn current_view_survives_mechanics_receipt_retirement() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-g");
        let path = database_path("retired-receipt");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        let event = stored_event(worktree, ready(language, 1, 1));
        publications.record(&event).await.unwrap();
        for index in 0..128 {
            store
                .execute(
                    OperationId::new(format!("synthetic-retirement-{index}")).unwrap(),
                    |_| Ok(()),
                )
                .await
                .unwrap();
        }
        match publications.record(&event).await.unwrap() {
            RecordOutcome::Current { revision, .. } => {
                assert_eq!(revision, NonZeroU64::new(1).unwrap());
            }
            other => panic!("retired receipt blocked unchanged recovery: {other:?}"),
        }
        remove_database(&path);
    }

    /// A repeated wrong-identity acknowledgement stays NoMatch; a committed mechanics receipt
    /// is never read as the domain result (F09).
    #[tokio::test]
    async fn wrong_identity_acknowledgement_remains_no_match() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-h");
        let path = database_path("wrong-ack");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        publications
            .record(&stored_event(worktree, ready(language, 0, 1)))
            .await
            .unwrap();
        for _ in 0..2 {
            assert_eq!(
                publications
                    .acknowledge_custody(worktree, language, "not-the-event")
                    .await
                    .unwrap(),
                CustodyAck::NoMatch
            );
        }
        remove_database(&path);
    }

    /// Custody acknowledgement matches the exact event identity only.
    #[tokio::test]
    async fn custody_acknowledgement_matches_exact_event() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-i");
        let path = database_path("custody");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        publications
            .record(&stored_event(worktree, ready(language, 0, 1)))
            .await
            .unwrap();
        let event_id = publications
            .current(worktree, language)
            .await
            .unwrap()
            .unwrap()
            .event_id;
        assert_eq!(
            publications
                .acknowledge_custody(worktree, language, "not-the-event")
                .await
                .unwrap(),
            CustodyAck::NoMatch
        );
        assert_eq!(
            publications
                .current(worktree, language)
                .await
                .unwrap()
                .unwrap()
                .custody,
            ViewCustody::Pending
        );
        assert_eq!(
            publications
                .acknowledge_custody(worktree, language, &event_id)
                .await
                .unwrap(),
            CustodyAck::Acknowledged
        );
        assert_eq!(
            publications
                .acknowledge_custody(worktree, language, &event_id)
                .await
                .unwrap(),
            CustodyAck::Acknowledged
        );
        assert_eq!(
            publications
                .current(worktree, language)
                .await
                .unwrap()
                .unwrap()
                .custody,
            ViewCustody::Acknowledged
        );
        remove_database(&path);
    }

    /// Equal input generations with different content each advance the revision: content, not
    /// arrival or generation, orders a stream (A11/A18 multi-view).
    #[tokio::test]
    async fn equal_input_generation_views_each_advance() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let worktree = Path::new("/tmp/agent-ide-view-j");
        let path = database_path("equal-generation");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        for (expected, problems) in [(1, 1), (2, 2)] {
            match publications
                .record(&stored_event(worktree, ready(language, problems, 7)))
                .await
                .unwrap()
            {
                RecordOutcome::Advanced { revision, .. } => {
                    assert_eq!(revision, NonZeroU64::new(expected).unwrap());
                }
                other => panic!("expected revision {expected}, got {other:?}"),
            }
        }
        remove_database(&path);
    }

    /// Two worktrees publish independent streams with independent revisions and identities.
    #[tokio::test]
    async fn two_worktrees_publish_independent_streams() {
        crate::lang::testing::install();
        let language = crate::lang::testing::ALPHA;
        let first = Path::new("/tmp/agent-ide-view-k1");
        let second = Path::new("/tmp/agent-ide-view-k2");
        let path = database_path("two-worktrees");
        let store = Store::open(&path, config()).unwrap();
        let publications = ViewPublicationStore::open(&store).await.unwrap();
        for worktree in [first, second] {
            match publications
                .record(&stored_event(worktree, ready(language, 1, 1)))
                .await
                .unwrap()
            {
                RecordOutcome::Advanced { revision, .. } => {
                    assert_eq!(revision, NonZeroU64::new(1).unwrap());
                }
                other => panic!("expected an independent first revision, got {other:?}"),
            }
        }
        let first_row = publications
            .current(first, language)
            .await
            .unwrap()
            .unwrap();
        let second_row = publications
            .current(second, language)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first_row.event_id, second_row.event_id);
        assert_eq!(first_row.revision, second_row.revision);
        remove_database(&path);
    }
}
