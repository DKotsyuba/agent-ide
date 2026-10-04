//! SQLite-owned physical worktree identities, boot-fenced authority, and bounded baseline capture.

use super::authority::{
    ActivationRequest, AuthorityError, AuthorityRevoked, AuthorityStamp, RevocationReason,
    StartRole, StopBindingHandoff, WorktreeRef,
};
use crate::{
    app::store::{
        DomainMigration, DomainName, MigrationAdmission, MigrationDigest, MigrationKey,
        OperationId, Store, StoreError, StoreOutcome, TrustedUpSql,
    },
    assistance::host_binding::ActiveBindingUse,
};
use rusqlite::{OptionalExtension, params, types::Value};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Immutable Workspace migration for canonical identity and durable admission receipts.
const SQL: &str = "
CREATE TABLE workspace_authority_clock (singleton INTEGER PRIMARY KEY CHECK(singleton=1), boot INTEGER NOT NULL, epoch INTEGER NOT NULL);
INSERT INTO workspace_authority_clock VALUES (1,0,0);
CREATE TABLE workspace_worktrees (incarnation INTEGER PRIMARY KEY AUTOINCREMENT, physical_key BLOB NOT NULL UNIQUE, native_key BLOB NOT NULL, root BLOB NOT NULL, repository BLOB NOT NULL, common_dir BLOB NOT NULL);
CREATE TABLE workspace_starts (operation TEXT PRIMARY KEY, digest BLOB NOT NULL, incarnation INTEGER NOT NULL REFERENCES workspace_worktrees(incarnation), actor TEXT NOT NULL, binding BLOB NOT NULL, boot INTEGER NOT NULL, epoch INTEGER NOT NULL, outcome TEXT NOT NULL, active INTEGER NOT NULL CHECK(active IN (0,1)));
CREATE UNIQUE INDEX workspace_one_worktree_owner ON workspace_starts(incarnation) WHERE active=1;
CREATE UNIQUE INDEX workspace_one_actor_worktree ON workspace_starts(actor) WHERE active=1;
CREATE TABLE workspace_stops (operation TEXT PRIMARY KEY, digest BLOB NOT NULL, outcome TEXT NOT NULL);
CREATE TABLE workspace_baselines (operation TEXT PRIMARY KEY, digest BLOB NOT NULL, incarnation INTEGER NOT NULL REFERENCES workspace_worktrees(incarnation), epoch INTEGER NOT NULL, boot INTEGER NOT NULL, payload BLOB NOT NULL, coverage TEXT NOT NULL, capture_window TEXT NOT NULL);
";

/// Adds opaque lifecycle nonces and explicit closure without changing the admitted v1 migration.
const IDENTITY_SQL: &str = "
ALTER TABLE workspace_worktrees ADD COLUMN nonce BLOB;
ALTER TABLE workspace_worktrees ADD COLUMN root_object BLOB;
ALTER TABLE workspace_worktrees ADD COLUMN root_identity BLOB;
ALTER TABLE workspace_worktrees ADD COLUMN closed INTEGER NOT NULL DEFAULT 0 CHECK(closed IN (0,1));
CREATE TABLE workspace_closures (operation TEXT PRIMARY KEY, digest BLOB NOT NULL, incarnation INTEGER NOT NULL, outcome TEXT NOT NULL);
UPDATE workspace_worktrees SET closed=1 WHERE nonce IS NULL;
CREATE UNIQUE INDEX workspace_open_root ON workspace_worktrees(root) WHERE closed=0;
CREATE UNIQUE INDEX workspace_open_object ON workspace_worktrees(root_object) WHERE closed=0;
";

/// Adds start roles and holder facts: one writer per worktree (the `workspace_one_worktree_owner`
/// invariant, now over writer rows only), coexisting readers, and the commit time a refusal names.
const READER_SQL: &str = "
ALTER TABLE workspace_starts ADD COLUMN role TEXT NOT NULL DEFAULT 'writer' CHECK(role IN ('writer','reader'));
ALTER TABLE workspace_starts ADD COLUMN started_ms INTEGER NOT NULL DEFAULT 0;
DROP INDEX workspace_one_worktree_owner;
CREATE UNIQUE INDEX workspace_one_worktree_owner ON workspace_starts(incarnation) WHERE active=1 AND role='writer';
";

/// Explains an unavailable native identity, failed durable admission, or conflicting stable operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurableError {
    /// Application mechanics refused or could not settle the operation.
    Application(StoreError),
    /// Schema admission was incompatible, ambiguous, or lacked the required durable backup.
    Migration(MigrationAdmission),
    /// The requested authority transition was rejected without granting access.
    Authority(AuthorityError),
    /// An existing stable operation names different immutable request facts.
    OperationConflict,
    /// Native identity is missing, symlinked, moved, or no longer matches its stored incarnation.
    /// Carries the closed failing resolve step and, when another start blocks the identity, its
    /// holder facts, so the refusal and the journal can name both instead of a bare conflict.
    IdentityUnavailable {
        /// Closed failing step of `resolve_worktree`, e.g. `identity_held`.
        step: &'static str,
        /// The active start that blocks a replaced identity, when one does.
        holder: Option<Box<StartHolder>>,
    },
    /// Durable data is missing or malformed, so it cannot authorize a caller.
    CorruptState,
    /// A capture request exceeded a hard input bound or contained invalid evidence.
    InvalidCapture,
}

impl From<StoreError> for DurableError {
    /// Preserves Application admission and ambiguity semantics.
    fn from(error: StoreError) -> Self {
        Self::Application(error)
    }
}
impl From<AuthorityError> for DurableError {
    /// Preserves Workspace authority refusal semantics.
    fn from(error: AuthorityError) -> Self {
        Self::Authority(error)
    }
}

/// Immutable committed start outcome; retaining it never grants current authority after stop or boot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartReceipt {
    /// Canonical stored worktree identity for this activation.
    worktree: WorktreeRef,
    /// Stable activation operation identifier.
    operation: String,
    /// Exact request fingerprint that makes retries idempotent.
    digest: [u8; 32],
    /// Host-validated actor whose exclusivity was admitted.
    actor: String,
    /// Opaque Assistance binding fingerprint; never reconstructs host proof.
    binding: [u8; 32],
    /// Durable owner boot that committed the grant.
    boot: u64,
    /// Monotonic durable authority generation issued by this grant.
    epoch: u64,
    /// Authority class committed for this start.
    role: StartRole,
}
/// Closed facts naming the active start a refused activation collided with (no authority).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartHolder {
    /// Whether the holder runs under the refused request's own actor.
    pub same_actor: bool,
    /// Host-validated actor of the holding start.
    pub actor: String,
    /// Binding fingerprint of the holding start, so in-memory liveness can be matched.
    pub binding: [u8; 32],
    /// The holder's activation operation identifier.
    pub activation: String,
    /// Authority epoch the holder committed.
    pub epoch: u64,
    /// Wall-clock milliseconds of the holder's start commit.
    pub started_ms: u64,
    /// Authority class of the holding start.
    pub role: StartRole,
}

impl StartReceipt {
    /// Returns the canonical worktree whose historical grant this receipt records.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }
    /// Returns the host-validated actor this receipt was committed for, for crate-internal
    /// reconciliation of a pending revoke against a fresh start by the same actor.
    pub(crate) fn actor(&self) -> &str {
        &self.actor
    }
    /// Returns the stable activation operation identifier.
    pub fn operation(&self) -> &str {
        &self.operation
    }
    /// Returns the committed epoch without asserting it is still active.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
    /// Returns the authority class committed for this start.
    pub const fn role(&self) -> StartRole {
        self.role
    }
}

/// Immutable explicit lifecycle-close outcome; it is neither a stop handoff nor new authority.
#[derive(Debug, Eq, PartialEq)]
pub struct VerifiedWorktreeClosure {
    /// Exact stable operation that committed closure.
    operation: OperationId,
    /// Canonical nonce-bound identity of the worktree this closure retired. It is carried beside
    /// the incarnation because the incarnation counter is Store-local and repeats across databases.
    worktree: String,
    /// Retired durable lifecycle generation.
    incarnation: u64,
}
impl VerifiedWorktreeClosure {
    /// Returns the historical close operation for reconciliation.
    pub fn operation(&self) -> &OperationId {
        &self.operation
    }
    /// Returns the canonical nonce-bound identity of the worktree this closure retired.
    pub fn worktree(&self) -> &str {
        &self.worktree
    }
    /// Returns the closed incarnation without granting access to a reopened directory.
    pub const fn incarnation(&self) -> u64 {
        self.incarnation
    }
}

/// Boot-owned native handles indexed by incarnation with the matching database nonce.
type HeldIdentities = BTreeMap<u64, ([u8; 32], NativeIdentity)>;

/// Sole product admission owner for one Application Store boot; clones share the same boot fence.
/// Opening another owner deliberately fences prior claims. Product wiring must hold one owner,
/// use canonical resolution before start, and call authority/authorize for every scoped admission.
#[derive(Clone)]
pub struct DurableWorkspace<'a> {
    /// Application-owned SQLite connection and bounded submission mechanics.
    store: &'a Store,
    /// Durable boot generation minted during this owner's initialization.
    boot: u64,
    /// Keeps admitted native objects open across calls so live inode reuse cannot alias a grant.
    identities: Arc<Mutex<HeldIdentities>>,
}

impl<'a> DurableWorkspace<'a> {
    /// Admits the immutable Workspace schema, advances the boot fence, and retires old active claims.
    /// A configured backup root is required when this upgrades an existing database. Ambiguity
    /// yields no owner or authority; creating a new owner is a new boot, never replay of an old one.
    pub async fn open(store: &'a Store) -> Result<Self, DurableError> {
        let sql = TrustedUpSql::new(SQL)?;
        let domain = DomainName::new("workspace")?;
        let key = MigrationKey::new("identity_authority_baseline_v1")?;
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
            return Err(DurableError::Migration(admission));
        }
        let sql = TrustedUpSql::new(IDENTITY_SQL)?;
        let domain = DomainName::new("workspace")?;
        let key = MigrationKey::new("creation_identity_v2")?;
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
            return Err(DurableError::Migration(admission));
        }
        let sql = TrustedUpSql::new(READER_SQL)?;
        let domain = DomainName::new("workspace")?;
        let key = MigrationKey::new("reader_starts_v3")?;
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
            return Err(DurableError::Migration(admission));
        }
        let nonce = random_nonce()?;
        let operation = operation("boot", &nonce)?;
        let boot = store
            .execute(operation, |tx| {
                let (boot, epoch): (i64, i64) = tx.query_row(
                    "SELECT boot,epoch FROM workspace_authority_clock WHERE singleton=1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if boot < 0 || epoch < 0 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let boot = boot.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
                let epoch = epoch.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
                tx.execute(
                    "UPDATE workspace_authority_clock SET boot=?1,epoch=?2 WHERE singleton=1",
                    params![boot, epoch],
                )?;
                tx.execute("UPDATE workspace_starts SET active=0 WHERE active=1", [])?;
                Ok(boot)
            })
            .await?;
        Ok(Self {
            store,
            boot: unsigned(boot)?,
            identities: Arc::default(),
        })
    }

    /// Resolves creation-aware native objects, retaining owned descriptors for this owner's lifetime.
    /// Changed creation-aware objects replace inactive records; active grants must first be revoked.
    /// Legacy rows without creation identity/nonces fail closed and cannot revive an old grant.
    ///
    /// Every refusal names its closed failing step: `identity_read` (the discovered directories
    /// could not be opened as real directories), `identity_changed` (a transient re-read mismatch
    /// inside the commit), `identity_alias` (an open row matches the physical root under another
    /// path), `identity_held` (an active start still owns the previous incarnation of this
    /// replaced directory, with its holder facts attached), or `identity_commit` (the durable
    /// identity row could not be committed) — never one collapsed cause again (E013 item 7).
    pub async fn resolve_worktree(
        &self,
        root: PathBuf,
        repository: PathBuf,
        common_dir: PathBuf,
    ) -> Result<WorktreeRef, DurableError> {
        let read_failed = || DurableError::IdentityUnavailable {
            step: "identity_read",
            holder: None,
        };
        let native =
            NativeIdentity::read(&root, &repository, &common_dir).map_err(|_| read_failed())?;
        let root_object = native.physical;
        let root_identity = native.root_identity;
        let key = native.key;
        let root = native.root.as_os_str().as_bytes().to_vec();
        let repository = native.repository.as_os_str().as_bytes().to_vec();
        let common = native.common.as_os_str().as_bytes().to_vec();
        let nonce = random_nonce().map_err(|_| read_failed())?;
        let physical = fingerprint(&[b"incarnation", &root_object, &nonce]);
        let boot = signed(self.boot)?;
        let op = operation("identity", &nonce).map_err(|_| DurableError::CorruptState)?;
        let verify_paths = (
            native.root.clone(),
            native.repository.clone(),
            native.common.clone(),
        );
        // The commit returns the blocked incarnation's holder facts instead of a bare `None`, so
        // the refusal can name the live start that owns the replaced identity.
        let result = self.store.execute(op.clone(), move |tx| {
            if current_boot(tx)? != boot || NativeIdentity::read(&verify_paths.0, &verify_paths.1, &verify_paths.2).map(|current| current.key) != Ok(key) { return Ok(Err(BlockedIdentity::Step("identity_changed"))); }
            let existing = match tx.query_row("SELECT incarnation,native_key,nonce,root FROM workspace_worktrees WHERE closed=0 AND (root=?1 OR root_object=?2) ORDER BY (root=?1) DESC LIMIT 1", params![root,root_object.as_slice()], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,Option<Vec<u8>>>(2)?,row.get::<_,Vec<u8>>(3)?))).optional() {
                Ok(existing) => existing,
                Err(_) => return Ok(Err(BlockedIdentity::Step("identity_commit"))),
            };
            if let Some((incarnation, previous_key, previous_nonce, previous_root)) = existing {
                if previous_root != root { return Ok(Err(BlockedIdentity::Step("identity_alias"))); }
                if previous_key == key { return Ok(Ok((incarnation, previous_key, previous_nonce))); }
                let holder = match tx.query_row("SELECT actor,binding,operation,epoch,started_ms,role FROM workspace_starts WHERE incarnation=?1 AND active=1 ORDER BY role='writer' DESC LIMIT 1", [incarnation], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?,row.get::<_,String>(5)?))).optional() {
                    Ok(holder) => holder,
                    Err(_) => return Ok(Err(BlockedIdentity::Step("identity_commit"))),
                };
                if let Some((actor, binding, activation, epoch, started_ms, role)) = holder {
                    return Ok(Err(BlockedIdentity::Held(StartHolder {
                        same_actor: false,
                        actor,
                        binding: binding.as_slice().try_into().unwrap_or([0; 32]),
                        activation,
                        epoch: u64::try_from(epoch).unwrap_or(0),
                        started_ms: u64::try_from(started_ms).unwrap_or(0),
                        role: StartRole::parse(&role).unwrap_or(StartRole::Writer),
                    })));
                }
                tx.execute("UPDATE workspace_worktrees SET closed=1 WHERE incarnation=?1", [incarnation])?;
            }

            match tx.execute("INSERT INTO workspace_worktrees(physical_key,native_key,root,repository,common_dir,nonce,root_object,root_identity) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)", params![physical.as_slice(),key.as_slice(),root,repository,common,nonce.as_slice(),root_object.as_slice(),root_identity.as_slice()]) {
                Ok(_) => Ok(Ok((tx.last_insert_rowid(), key.to_vec(), Some(nonce.to_vec())))),
                Err(_) => Ok(Err(BlockedIdentity::Step("identity_commit"))),
            }
        }).await;
        let committed = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                // An outcome-unknown retry that committed the identity row resolves it here;
                // anything else is a commit refusal even though the operation settled.
                self.store
                    .read_one("SELECT incarnation,native_key,nonce FROM workspace_worktrees WHERE native_key=?1 AND closed=0", vec![Value::Blob(key.to_vec())], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Vec<u8>>(1)?,row.get::<_,Option<Vec<u8>>>(2)?)))
                    .await
                    .ok()
                    .flatten()
                    .ok_or(BlockedIdentity::Step("identity_commit"))
            }
        };
        let row = match committed {
            Ok(row) => row,
            Err(step) => {
                return Err(match step {
                    BlockedIdentity::Step(step) => {
                        DurableError::IdentityUnavailable { step, holder: None }
                    }
                    BlockedIdentity::Held(holder) => DurableError::IdentityUnavailable {
                        step: "identity_held",
                        holder: Some(Box::new(holder)),
                    },
                });
            }
        };
        let unavailable = || DurableError::IdentityUnavailable {
            step: "identity_commit",
            holder: None,
        };
        let nonce: [u8; 32] = row
            .2
            .ok_or_else(unavailable)?
            .try_into()
            .map_err(|_| DurableError::CorruptState)?;
        if row.1 != key {
            return Err(unavailable());
        }
        let incarnation = unsigned(row.0)?;
        let mut tree = WorktreeRef::from_discovery(
            native.root.clone(),
            native.repository.clone(),
            native.common.clone(),
            incarnation,
        )?;
        tree.native_key = Some(key);
        tree.native_root_identity = Some(native.root_identity);
        tree.set_durable_nonce(nonce);
        self.identities
            .lock()
            .map_err(|_| DurableError::CorruptState)?
            .insert(incarnation, (nonce, native));
        Ok(tree)
    }

    /// Commits explicit lifecycle closure only for an exact verified directory with no active grant.
    /// This is separate from actor stop; no product stop ingress invokes it. A later resolution of
    /// even the same native directory mints a fresh nonce/incarnation. Exact retries recover the receipt.
    pub async fn close_worktree(
        &self,
        id: OperationId,
        tree: &WorktreeRef,
    ) -> Result<VerifiedWorktreeClosure, DurableError> {
        let unavailable = || DurableError::IdentityUnavailable {
            step: "identity_read",
            holder: None,
        };
        let key = tree.native_key.ok_or_else(unavailable)?;
        let nonce = tree.durable_nonce.ok_or_else(unavailable)?;
        let digest = fingerprint(&[id.as_str().as_bytes(), tree.id().as_bytes(), &key, &nonce]);
        let op = operation("close", id.as_str().as_bytes())?;
        let incarnation = signed(tree.incarnation())?;
        let boot = signed(self.boot)?;
        let closure_tree = tree.clone();
        let sql_id = id.as_str().to_owned();
        let result = self.store.execute(op.clone(), move |tx| {
            let outcome = if current_boot(tx)? != boot { "stale" }
            else if NativeIdentity::read(closure_tree.worktree_path(),closure_tree.repository_root(),closure_tree.git_common_dir()).map(|native|native.key) != Ok(key) { "identity" }
            else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE incarnation=?1 AND active=1)", [incarnation], |row|row.get::<_,bool>(0))? { "worktree_owned" }
            else if tx.execute("UPDATE workspace_worktrees SET closed=1 WHERE incarnation=?1 AND native_key=?2 AND nonce=?3 AND closed=0",params![incarnation,key.as_slice(),nonce.as_slice()])? == 1 { "closed" }
            else { "identity" };
            tx.execute("INSERT INTO workspace_closures(operation,digest,incarnation,outcome) VALUES (?1,?2,?3,?4)",params![sql_id,digest.as_slice(),incarnation,outcome])?;
            Ok((digest.to_vec(),outcome.to_owned()))
        }).await;
        let (stored_digest, outcome) = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                self.store
                    .read_one(
                        "SELECT digest,outcome FROM workspace_closures WHERE operation=?1",
                        vec![Value::Text(id.as_str().to_owned())],
                        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
                    )
                    .await?
                    .ok_or(DurableError::CorruptState)?
            }
        };
        if stored_digest != digest {
            return Err(DurableError::OperationConflict);
        }
        if outcome != "closed" {
            return Err(rejection(&outcome));
        }
        Ok(VerifiedWorktreeClosure {
            operation: id,
            worktree: tree.id().to_owned(),
            incarnation: tree.incarnation(),
        })
    }

    /// Commits one exact host-bound start or returns its original immutable receipt on an exact retry.
    /// The stable identity excludes per-call IDs so a fresh host call can retry the same actor/binding/worktree intent.
    /// Historical receipts survive restart but cannot mint current authority. A binding used by a prior
    /// boot or a stopped grant cannot start a new operation; a fresh host binding is required.
    ///
    /// A start whose own actor and binding already hold this worktree in this boot is idempotent: no
    /// new authority is minted and the holder's original receipt (and its activation operation) is
    /// returned, so a repeated `ide.start` succeeds instead of refusing with a conflict.
    ///
    /// One writer is admitted per worktree. Only an explicit `read_only` request admits a reader;
    /// readers coexist with writers and each other without taking the writer index. The same
    /// activation upgrades when it starts without `read_only` and the slot is free, and a writer
    /// downgrades when it starts with `read_only: true`.
    pub async fn activate(&self, request: ActivationRequest) -> Result<StartReceipt, DurableError> {
        self.validate_native(&request.worktree)?;
        let binding = request.active_use.binding_ref().fingerprint();
        let actor = request.invocation.actor_id().to_owned();
        let digest = fingerprint(&[
            request.activation_id.as_bytes(),
            actor.as_bytes(),
            &binding,
            request.worktree.id().as_bytes(),
        ]);
        let id = request.activation_id.clone();
        let wants_reader = request.read_only;
        let incarnation = signed(request.worktree.incarnation())?;
        let boot = signed(self.boot)?;
        let started_ms = crate::errorlog::now_ms() as i64;
        // One store operation per call, not per activation id: a repeated start of the same
        // activation must re-run its admission (an idempotent retry answers the holder receipt, a
        // reader upgrades once no writer remains), so the row-level logic — not the store's
        // same-operation replay — decides the outcome.
        let call_nonce = random_nonce()?;
        let mut call_identity = id.as_bytes().to_vec();
        call_identity.extend_from_slice(&call_nonce);
        let op = operation("start", &call_identity)?;
        let sql_actor = actor.clone();
        let sql_id = id.clone();
        let sql_binding = binding.to_vec();
        let unavailable = || DurableError::IdentityUnavailable {
            step: "identity_read",
            holder: None,
        };
        let native_key = request.worktree.native_key.ok_or_else(unavailable)?;
        let nonce = request.worktree.durable_nonce.ok_or_else(unavailable)?;
        let result = self.store.execute(op.clone(), move |tx| {
            // Preserve the original receipt for an exact historical retry, but let an active
            // reader row flow through admission again so it can upgrade after its writer stops.
            if let Some((previous_digest, previous_boot, previous_epoch, active, previous_role)) = tx
                .query_row(
                    "SELECT digest,boot,epoch,active,role FROM workspace_starts WHERE operation=?1",
                    [&sql_id],
                    |row| {
                        Ok((
                            row.get::<_, Vec<u8>>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, bool>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .optional()?
                && (!active || previous_digest != digest)
            {
                return Ok((
                    previous_digest,
                    previous_boot,
                    previous_epoch,
                    "historical".to_owned(),
                    previous_role,
                ));
            }
            // The one active writer of this incarnation (readers may coexist with it) and this
            // actor's own active start (any role, any worktree), both read before the exclusivity
            // checks so the same owner is recognized instead of refused.
            let writer = tx
                .query_row(
                    "SELECT actor,binding,epoch FROM workspace_starts WHERE incarnation=?1 AND active=1 AND role='writer'",
                    [incarnation],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let own = tx
                .query_row(
                    "SELECT incarnation,role,epoch FROM workspace_starts WHERE actor=?1 AND binding=?2 AND active=1",
                    params![sql_actor, sql_binding.as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let outcome = if current_boot(tx)? != boot { "stale" }
                else if !tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_worktrees WHERE incarnation=?1 AND native_key=?2 AND nonce=?3 AND closed=0)", params![incarnation,native_key.as_slice(),nonce.as_slice()], |row| row.get::<_,bool>(0))? { "identity" }
                else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE binding=?1 AND outcome='granted' AND (boot<>?2 OR active=0))", params![binding.as_slice(),boot], |row| row.get::<_,bool>(0))? { "binding" }
                else if let Some((own_incarnation, own_role, _)) = &own {
                    if own_incarnation != &incarnation { "actor_owned" }
                    else if own_role == "writer" && wants_reader { "downgrade" }
                    else if own_role == "reader" && !wants_reader && writer.is_none() { "upgrade" }
                    else if own_role == "reader" && !wants_reader && writer.is_some() { "worktree_owned" }
                    else { "held" }
                }
                else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE actor=?1 AND active=1)", [&sql_actor], |row| row.get::<_,bool>(0))? { "actor_owned" }
                else if wants_reader { "reader" }
                else if writer.is_some() { "worktree_owned" }
                else { "granted" };
            let role = match outcome {
                "reader" | "downgrade" => "reader",
                _ => "writer",
            };
            // Reader receipts use the same granted marker as writers; the role column is the
            // authorization class and the partial unique index reserves only writer rows.
            let stored_outcome = if outcome == "reader" || outcome == "downgrade" {
                "granted"
            } else {
                outcome
            };
            let epoch = if matches!(outcome, "granted" | "upgrade" | "downgrade") {
                let epoch: i64 = tx.query_row("SELECT epoch FROM workspace_authority_clock WHERE singleton=1", [], |row| row.get(0))?;
                let epoch = epoch.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
                tx.execute("UPDATE workspace_authority_clock SET epoch=?1 WHERE singleton=1", [epoch])?;
                epoch
            } else if outcome == "reader" {
                writer.as_ref().map_or_else(
                    || tx.query_row("SELECT epoch FROM workspace_authority_clock WHERE singleton=1", [], |row| row.get(0)),
                    |(_, _, epoch)| Ok(*epoch),
                )?
            } else {
                writer.as_ref().map_or_else(|| own.as_ref().map_or(0, |(_, _, epoch)| *epoch), |(_, _, epoch)| *epoch)
            };
            if outcome == "upgrade" {
                tx.execute("UPDATE workspace_starts SET role='writer', epoch=?1 WHERE operation=?2 AND active=1", params![epoch, sql_id])?;
            } else if outcome == "downgrade" {
                tx.execute("UPDATE workspace_starts SET role='reader', epoch=?1 WHERE operation=?2 AND active=1", params![epoch, sql_id])?;
            } else if outcome == "granted" || outcome == "reader" {
                // Only authority-bearing outcomes need a start row. Refusals remain retryable,
                // including after a boot or stop, and must not collide with the activation's
                // stable primary key when a caller retries it.
                tx.execute("INSERT INTO workspace_starts(operation,digest,incarnation,actor,binding,boot,epoch,outcome,active,role,started_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)", params![sql_id,digest.as_slice(),incarnation,sql_actor,binding.as_slice(),boot,epoch,stored_outcome,i64::from(stored_outcome=="granted"),role,started_ms])?;
            }
            Ok((digest.to_vec(),boot,epoch,outcome.to_owned(),role.to_owned()))
        }).await;
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                self.store
                    .read_one(
                        "SELECT digest,boot,epoch,outcome,role FROM workspace_starts WHERE operation=?1",
                        vec![Value::Text(id.clone())],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .await?
                    .ok_or(DurableError::CorruptState)?
            }
        };
        if row.0 != digest {
            return Err(DurableError::OperationConflict);
        }
        if row.3 == "held" {
            // The same actor and binding already hold this worktree in this boot. The request
            // committed no new authority, so answer with the holder's original receipt — including
            // its activation operation, which is what a repeated `ide.start` must report.
            let (operation, epoch, role) = self
                .store
                .read_one(
                    "SELECT operation,epoch,role FROM workspace_starts WHERE incarnation=?1 AND active=1 AND outcome='granted' AND actor=?2 AND binding=?3",
                    vec![Value::Integer(incarnation), Value::Text(actor.clone()), Value::Blob(binding.to_vec())],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?)),
                )
                .await?
                .ok_or(DurableError::CorruptState)?;
            return Ok(StartReceipt {
                worktree: request.worktree,
                operation,
                digest,
                actor,
                binding,
                boot: unsigned(row.1)?,
                epoch: unsigned(epoch)?,
                role: StartRole::parse(&role).ok_or(DurableError::CorruptState)?,
            });
        }
        if row.3 != "granted"
            && row.3 != "reader"
            && row.3 != "upgrade"
            && row.3 != "downgrade"
            && row.3 != "historical"
        {
            return Err(rejection(&row.3));
        }
        Ok(StartReceipt {
            worktree: request.worktree,
            operation: id,
            digest,
            actor,
            binding,
            boot: unsigned(row.1)?,
            epoch: unsigned(row.2)?,
            role: if row.3 == "reader" {
                StartRole::Reader
            } else if row.3 == "granted" {
                // A fallback read of a committed row: the role column carries the class.
                StartRole::parse(&row.4).ok_or(DurableError::CorruptState)?
            } else if row.3 == "downgrade" || row.3 == "historical" {
                // Exact retries keep the old boot and cannot pass `authority`'s boot fence.
                StartRole::parse(&row.4).ok_or(DurableError::CorruptState)?
            } else {
                // A committed `upgrade` holds writer authority now.
                StartRole::Writer
            },
        })
    }

    /// Reads the closed facts of the active start a refused activation collided with, so the
    /// refusal can name who holds what instead of a bare conflict.
    ///
    /// The worktree's own active start is preferred (its writer before any reader); an actor that
    /// owns another worktree answers with that start instead. Nothing is granted and no authority
    /// is minted by this read.
    pub async fn start_holder(
        &self,
        worktree: &WorktreeRef,
        actor: &str,
    ) -> Result<Option<StartHolder>, DurableError> {
        let incarnation = signed(worktree.incarnation())?;
        let sql_actor = actor.to_owned();
        let sql = "SELECT actor,binding,operation,epoch,started_ms,role FROM workspace_starts WHERE active=1 AND (incarnation=?1 OR actor=?2) ORDER BY (incarnation=?1) DESC, (role='writer') DESC LIMIT 1";
        let holder = self
            .store
            .read_one(
                sql,
                vec![Value::Integer(incarnation), Value::Text(sql_actor.clone())],
                move |row| {
                    let binding: Vec<u8> = row.get(1)?;
                    let role: String = row.get(5)?;
                    let epoch = u64::try_from(row.get::<_, i64>(3)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    let started_ms = u64::try_from(row.get::<_, i64>(4)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                    Ok(StartHolder {
                        same_actor: row.get::<_, String>(0)? == sql_actor,
                        actor: row.get(0)?,
                        binding: binding.as_slice().try_into().unwrap_or([0; 32]),
                        activation: row.get(2)?,
                        epoch,
                        started_ms,
                        role: StartRole::parse(&role).ok_or(rusqlite::Error::InvalidQuery)?,
                    })
                },
            )
            .await?;
        Ok(holder)
    }

    /// Converts a committed receipt to authority only after fresh binding, native identity, and boot checks.
    /// An exact historical retry can return a receipt while this method correctly refuses its authority.
    pub async fn authority(
        &self,
        receipt: &StartReceipt,
        active: &ActiveBindingUse,
    ) -> Result<AuthorityStamp, DurableError> {
        if active.binding_ref().fingerprint() != receipt.binding {
            return Err(AuthorityError::BindingNotCurrent.into());
        }
        let stamp = AuthorityStamp {
            worktree: receipt.worktree.clone(),
            binding: active.binding_ref().clone(),
            actor_id: receipt.actor.clone(),
            epoch: receipt.epoch,
            activation_id: receipt.operation.clone(),
            owner_boot: Some(receipt.boot),
            role: receipt.role,
        };
        self.authorize(&stamp, active).await?;
        Ok(stamp)
    }

    /// Gates every product-scoped use with fresh binding, exact native identity, and a current durable grant.
    /// In-memory registry stamps, prior boot stamps, and stopped/replaced grants are never sufficient.
    pub async fn authorize(
        &self,
        stamp: &AuthorityStamp,
        active: &ActiveBindingUse,
    ) -> Result<(), DurableError> {
        if stamp.owner_boot != Some(self.boot) {
            return Err(AuthorityError::StaleAuthority.into());
        }
        if &stamp.binding != active.binding_ref() {
            return Err(AuthorityError::BindingNotCurrent.into());
        }
        self.validate_native(&stamp.worktree)?;
        let exists = self.store.read_one("SELECT EXISTS(SELECT 1 FROM workspace_starts s JOIN workspace_authority_clock c ON c.singleton=1 WHERE s.operation=?1 AND s.incarnation=?2 AND s.actor=?3 AND s.binding=?4 AND s.epoch=?5 AND s.boot=?6 AND c.boot=s.boot AND s.active=1 AND s.outcome='granted')", vec![Value::Text(stamp.activation_id.clone()),Value::Integer(signed(stamp.worktree.incarnation())?),Value::Text(stamp.actor_id.clone()),Value::Blob(stamp.binding.fingerprint().to_vec()),Value::Integer(signed(stamp.epoch)?),Value::Integer(signed(self.boot)?)], |row| row.get::<_,bool>(0)).await?.unwrap_or(false);
        if exists {
            Ok(())
        } else {
            Err(AuthorityError::StaleAuthority.into())
        }
    }

    /// Durably revokes an exact current grant after the supplied Assistance stop handoff.
    /// Accepts a recoverable immutable start receipt, not a live stamp, so an identical stop retry
    /// returns its committed outcome after process restart without minting authority; changed arguments conflict.
    pub async fn revoke(
        &self,
        operation_id: OperationId,
        expected: &StartReceipt,
        handoff: StopBindingHandoff,
    ) -> Result<AuthorityRevoked, DurableError> {
        let reason = match handoff {
            StopBindingHandoff::Confirmed => "requested",
            StopBindingHandoff::Missing => "incomplete",
        };
        let digest = fingerprint(&[
            operation_id.as_str().as_bytes(),
            expected.operation.as_bytes(),
            expected.worktree.id().as_bytes(),
            &expected.epoch.to_le_bytes(),
            &expected.boot.to_le_bytes(),
            &expected.binding,
            reason.as_bytes(),
        ]);
        let id = operation_id.as_str().to_owned();
        let sql_id = id.clone();
        let activation = expected.operation.clone();
        let epoch = signed(expected.epoch)?;
        let expected_boot = signed(expected.boot)?;
        let boot = signed(self.boot)?;
        let op = operation("stop", id.as_bytes())?;
        let result = self.store.execute(op.clone(), move |tx| {
            let changed = if current_boot(tx)? == boot && expected_boot == boot {
                tx.execute("UPDATE workspace_starts SET active=0 WHERE operation=?1 AND epoch=?2 AND boot=?3 AND active=1", params![activation,epoch,boot])?
            } else { 0 };
            let outcome = if changed == 1 { reason } else { "stale" };
            tx.execute("INSERT INTO workspace_stops(operation,digest,outcome) VALUES (?1,?2,?3)", params![sql_id,digest.as_slice(),outcome])?;
            Ok((digest.to_vec(),outcome.to_owned()))
        }).await;
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                self.store
                    .read_one(
                        "SELECT digest,outcome FROM workspace_stops WHERE operation=?1",
                        vec![Value::Text(id)],
                        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
                    )
                    .await?
                    .ok_or(DurableError::CorruptState)?
            }
        };
        if row.0 != digest {
            return Err(DurableError::OperationConflict);
        }
        if !matches!(row.1.as_str(), "requested" | "incomplete") {
            return Err(rejection(&row.1));
        }
        Ok(AuthorityRevoked {
            worktree: expected.worktree.clone(),
            old_epoch: expected.epoch,
            reason: if row.1 == "requested" {
                RevocationReason::Requested
            } else {
                RevocationReason::RevocationIncomplete
            },
        })
    }

    /// Rechecks all native directory evidence; unverified caller-built references fail before SQL use.
    fn validate_native(&self, tree: &WorktreeRef) -> Result<(), DurableError> {
        let native = NativeIdentity::read(
            tree.worktree_path(),
            tree.repository_root(),
            tree.git_common_dir(),
        )?;
        let held = self
            .identities
            .lock()
            .map_err(|_| DurableError::CorruptState)?;
        if tree.durable_nonce.is_some()
            && tree.native_key == Some(native.key)
            && held
                .get(&tree.incarnation())
                .is_some_and(|(nonce, identity)| {
                    Some(*nonce) == tree.durable_nonce && identity.key == native.key
                })
        {
            Ok(())
        } else {
            Err(identity_read())
        }
    }

    /// Allows domain-row lookup only after Application has established a committed receipt.
    async fn require_committed(
        &self,
        op: &OperationId,
        error: StoreError,
    ) -> Result<(), DurableError> {
        match error {
            StoreError::DuplicateOperation {
                existing: StoreOutcome::Committed,
            } => Ok(()),
            StoreError::OutcomeUnknown { .. }
                if self.store.outcome(op.clone()).await? == StoreOutcome::Committed =>
            {
                Ok(())
            }
            error => Err(error.into()),
        }
    }
}

/// Why one `resolve_worktree` commit admitted no identity: a closed step tag, or the active
/// start that still owns the replaced incarnation of the discovered directory.
enum BlockedIdentity {
    /// Closed failing step, e.g. `identity_changed` for a transient in-commit re-read mismatch.
    Step(&'static str),
    /// An active start blocks closing the previous incarnation of this replaced directory.
    Held(StartHolder),
}

/// Native directory identity plus canonical raw paths used to reject aliases and lifecycle guesses.
struct NativeIdentity {
    /// Canonical worktree root with every symlink component refused.
    root: PathBuf,
    /// Owned root, repository, and common-directory handles prevent live inode reuse.
    _directories: [File; 3],
    /// Creation-aware identity of the same opened root used for the durable native key.
    root_identity: [u8; 32],
    /// Canonical discovered repository root.
    repository: PathBuf,
    /// Canonical Git common directory.
    common: PathBuf,
    /// Device/inode index used only to reject live-root path aliases; never sufficient for lifecycle reuse.
    physical: [u8; 32],
    /// Fingerprint of root, repository, common-directory native identities and canonical paths.
    key: [u8; 32],
}
impl NativeIdentity {
    /// Reads exactly the three discovered directories; no repository scan or process is launched.
    fn read(root: &Path, repository: &Path, common: &Path) -> Result<Self, DurableError> {
        let (root, root_id, root_identity, root_handle) = real_directory_with_root_identity(root)?;
        let (repository, _, repo_id, repo_handle) = real_directory_with_root_identity(repository)?;
        let (common, _, common_id, common_handle) =
            real_directory_with_root_identity(&if common.is_absolute() {
                common.to_path_buf()
            } else {
                root.join(common)
            })?;
        let physical = fingerprint(&[b"physical-root", &root_id]);
        let key = fingerprint(&[
            b"native-worktree",
            &root_identity,
            &repo_id,
            &common_id,
            root.as_os_str().as_bytes(),
            repository.as_os_str().as_bytes(),
            common.as_os_str().as_bytes(),
        ]);
        Ok(Self {
            root,
            root_identity,
            _directories: [root_handle, repo_handle, common_handle],
            repository,
            common,
            physical,
            key,
        })
    }
}

/// Walks native directories through owned descriptors, refusing symlinks without a check/open race.
/// Returns a canonical path only when it still names the opened final device/inode.
pub(super) fn real_directory(path: &Path) -> Result<(PathBuf, [u8; 16]), DurableError> {
    let (path, native, _, _) = real_directory_with_root_identity(path)?;
    Ok((path, native))
}

/// Resolves a directory while retaining creation-aware identity from that exact opened descriptor.
fn real_directory_with_root_identity(
    path: &Path,
) -> Result<(PathBuf, [u8; 16], [u8; 32], File), DurableError> {
    if !path.is_absolute() {
        return Err(identity_read());
    }
    let mut directory = File::open("/").map_err(|_| identity_read())?;
    for component in path.components() {
        if component == std::path::Component::RootDir {
            continue;
        }
        let next = super::observation::open_directory(directory.as_raw_fd(), component.as_os_str())
            .map_err(|_| identity_read())?;
        // SAFETY: open_directory returned a new owned descriptor; File closes the previous one on assignment.
        directory = unsafe { File::from_raw_fd(next) };
    }
    let root_identity =
        super::observation::native_directory_identity(&directory).map_err(|_| identity_read())?;
    let metadata = directory.metadata().map_err(|_| identity_read())?;
    let canonical = fs::canonicalize(path).map_err(|_| identity_read())?;
    let current = fs::symlink_metadata(&canonical).map_err(|_| identity_read())?;
    if !current.is_dir() || current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(identity_read());
    }
    let mut native = [0; 16];
    native[..8].copy_from_slice(&metadata.dev().to_le_bytes());
    native[8..].copy_from_slice(&metadata.ino().to_le_bytes());
    Ok((canonical, native, root_identity, directory))
}

/// Reads an opaque OS random incarnation/operation nonce; randomness never substitutes for native identity.
fn random_nonce() -> Result<[u8; 32], DurableError> {
    let mut nonce = [0; 32];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut nonce))
        .map_err(|_| identity_read())?;
    Ok(nonce)
}

/// Returns the durable singleton boot inside the same admission transaction.
fn current_boot(tx: &rusqlite::Transaction<'_>) -> rusqlite::Result<i64> {
    tx.query_row(
        "SELECT boot FROM workspace_authority_clock WHERE singleton=1",
        [],
        |row| row.get(0),
    )
}

/// Frames an ordered tuple under one Workspace domain, preserving raw byte distinctions.
fn fingerprint(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(b"workspace-durable-v1");
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    *hash.finalize().as_bytes()
}

/// Namespaces a stable domain operation without exhausting the bounded Application identifier size.
fn operation(kind: &str, identity: &[u8]) -> Result<OperationId, StoreError> {
    OperationId::new(format!(
        "workspace-{kind}-{}",
        blake3::Hash::from(fingerprint(&[kind.as_bytes(), identity])).to_hex()
    ))
}

/// The closed identity-read refusal: a discovered directory could not be opened as a real
/// directory (missing, symlinked, moved, or its filesystem identity could not be read).
fn identity_read() -> DurableError {
    DurableError::IdentityUnavailable {
        step: "identity_read",
        holder: None,
    }
}

/// Converts a durable positive generation without wrapping or treating zero/corrupt rows as current.
fn unsigned(value: i64) -> Result<u64, DurableError> {
    if value <= 0 {
        return Err(DurableError::CorruptState);
    }
    u64::try_from(value).map_err(|_| DurableError::CorruptState)
}
/// Converts a bounded generation to SQLite's signed integer range.
fn signed(value: u64) -> Result<i64, DurableError> {
    i64::try_from(value).map_err(|_| AuthorityError::EpochExhausted.into())
}

/// Decodes a persisted refusal; unknown or wrong-operation success tags never imply a grant/revocation.
fn rejection(outcome: &str) -> DurableError {
    match outcome {
        "stale" => AuthorityError::StaleAuthority.into(),
        "binding" => AuthorityError::BindingNotCurrent.into(),
        "identity" => DurableError::IdentityUnavailable {
            step: "identity_changed",
            holder: None,
        },
        "worktree_owned" => AuthorityError::WorktreeOwned.into(),
        "actor_owned" => AuthorityError::ActorAlreadyOwnsWorktree.into(),
        _ => DurableError::CorruptState,
    }
}

/// Maximum number of explicitly registered source paths in one stored baseline capture.
pub const MAX_BASELINE_PATHS: usize = 128;
/// Maximum framed Git/source payload stored for one baseline, including raw metadata.
pub const MAX_BASELINE_BYTES: usize = 4 * 1024 * 1024;

/// Native boundaries exposed only to the private deterministic capture harness.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CaptureCheckpoint {
    /// The original grant is authorized but the source root has not yet been opened.
    BeforeSourceRead,
    /// All source bytes are read but no baseline transaction has been submitted.
    BeforeCommit,
}

impl DurableWorkspace<'_> {
    /// Resolves one descriptor-held edit target only after a fresh durable authority check.
    ///
    /// `operation_id` and `path` are bounded by the Workspace edit contract. The returned permit
    /// can be consumed exactly once, and the target retains its parent/original descriptors across
    /// queueing. Any authority failure is reported as stale source with no filesystem write.
    pub async fn prepare_edit(
        &self,
        authority: &AuthorityStamp,
        active: &ActiveBindingUse,
        operation_id: impl Into<String>,
        path: PathBuf,
        source_ref: super::edit::EditSourceRef,
    ) -> Result<(super::edit::EditPermit, super::edit::CurrentEditTarget), super::edit::EditOutcome>
    {
        self.authorize(authority, active)
            .await
            .map_err(|_| super::edit::EditOutcome::StaleSource)?;
        let permit = super::edit::EditPermit::new(operation_id, path.clone())?;
        let target = super::edit::CurrentEditTarget::resolve(authority, &path, source_ref)?;
        Ok((permit, target))
    }

    /// Rechecks durable authority immediately before consuming one prepared edit permit.
    ///
    /// `continue_before_effect` is called after authority validation and descriptor resolution but
    /// before any target write. Returning false yields `cancelled_no_effect`. Once a rename may have
    /// happened, errors remain `outcome_unknown` and this method never retries the write.
    #[allow(clippy::too_many_arguments)]
    pub async fn replace_edit(
        &self,
        authority: &AuthorityStamp,
        active: &ActiveBindingUse,
        permit: super::edit::EditPermit,
        target: super::edit::CurrentEditTarget,
        source_ref: &super::edit::EditSourceRef,
        content: &[u8],
        continue_before_effect: impl FnOnce() -> bool,
    ) -> super::edit::EditOutcome {
        if self.authorize(authority, active).await.is_err() {
            return super::edit::EditOutcome::StaleSource;
        }
        super::edit::replace_if_current(
            permit,
            target,
            source_ref,
            content,
            || true,
            continue_before_effect,
        )
    }

    /// Captures bounded Git evidence and explicitly registered native source bytes under current authority.
    /// The stored window is always Unverified/Partial: separate Git evidence and native reads are not
    /// one atomic snapshot. Symlinks, unavailable files, truncation, and file ceilings remain explicit
    /// in the payload. Exact operation retries return the original committed capture, never recapture.
    pub async fn capture_baseline(
        &self,
        id: OperationId,
        expected: &AuthorityStamp,
        active: &ActiveBindingUse,
        git: Vec<super::git::RawGitEvidence>,
        paths: Vec<PathBuf>,
    ) -> Result<super::git::BaselineContext, DurableError> {
        self.capture_baseline_inner(id, expected, active, git, paths, |_| {})
            .await
    }

    /// Captures with deterministic boundary callbacks used by native root replacement regression tests.
    #[allow(clippy::too_many_arguments)]
    async fn capture_baseline_inner(
        &self,
        id: OperationId,
        expected: &AuthorityStamp,
        active: &ActiveBindingUse,
        git: Vec<super::git::RawGitEvidence>,
        paths: Vec<PathBuf>,
        mut checkpoint: impl FnMut(CaptureCheckpoint),
    ) -> Result<super::git::BaselineContext, DurableError> {
        use super::{
            git::{BaselineContext, DiffMode, GitScope},
            observation::{SourceReadLimits, read_authorized_source, valid_relative_path},
        };

        if git.is_empty()
            || git.len() > 6
            || paths.len() > MAX_BASELINE_PATHS
            || paths.iter().any(|path| !valid_relative_path(path))
        {
            return Err(DurableError::InvalidCapture);
        }
        let scope = GitScope::from_authority(expected, DiffMode::Head);
        let mut payload = Vec::new();
        frame(&mut payload, b"workspace-baseline-partial-unverified-v1")?;
        frame(&mut payload, &(git.len() as u64).to_le_bytes())?;
        for evidence in &git {
            if evidence.scope().worktree() != scope.worktree()
                || evidence.scope().authority_epoch() != scope.authority_epoch()
            {
                return Err(DurableError::InvalidCapture);
            }
            frame(
                &mut payload,
                &[evidence.query() as u8, u8::from(evidence.is_truncated())],
            )?;
            frame(&mut payload, evidence.operation_reference().as_bytes())?;
            frame(
                &mut payload,
                &evidence.exit_code().unwrap_or(i32::MIN).to_le_bytes(),
            )?;
            frame(&mut payload, evidence.stdout())?;
            frame(&mut payload, evidence.stderr())?;
        }
        frame(&mut payload, &(paths.len() as u64).to_le_bytes())?;
        for path in &paths {
            frame(&mut payload, path.as_os_str().as_bytes())?;
        }
        let digest = fingerprint(&[
            expected.worktree.id().as_bytes(),
            &expected.epoch.to_le_bytes(),
            &expected.owner_boot.ok_or(identity_read())?.to_le_bytes(),
            &payload,
        ]);
        let old = self.baseline_row(id.as_str()).await?;
        if let Some((stored_digest, stored_payload)) = old {
            if stored_digest != digest {
                return Err(DurableError::OperationConflict);
            }
            return BaselineContext::from_stored(
                id.as_str().to_owned(),
                scope,
                stored_capture_digest(&stored_payload)?,
                task_head_from_payload(&stored_payload)?,
            )
            .map_err(|_| DurableError::CorruptState);
        }
        self.authorize(expected, active).await?;
        checkpoint(CaptureCheckpoint::BeforeSourceRead);
        for path in paths {
            let available = MAX_BASELINE_BYTES
                .saturating_sub(payload.len())
                .saturating_sub(128);
            if available == 0 {
                return Err(DurableError::InvalidCapture);
            }
            let limits = SourceReadLimits::new(
                super::observation::MAX_SOURCE_PATH_BYTES,
                available.min(super::observation::MAX_SOURCE_BYTES),
            )
            .map_err(|_| DurableError::InvalidCapture)?;
            match read_authorized_source(&expected.worktree, &path, limits) {
                Ok(source) => {
                    frame(&mut payload, b"present")?;
                    frame(&mut payload, source.contents())?;
                }
                Err(error) => {
                    let tag: &[u8] = match error {
                        super::observation::ObservationError::Missing => b"missing",
                        super::observation::ObservationError::TooLarge { .. } => b"too_large",
                        super::observation::ObservationError::SymlinkEscape => b"symlink",
                        super::observation::ObservationError::NotRegularFile => b"not_regular",
                        super::observation::ObservationError::RootIdentityChanged
                        | super::observation::ObservationError::RootUnavailable => {
                            return Err(identity_read());
                        }
                        _ => b"unavailable",
                    };
                    frame(&mut payload, tag)?;
                }
            }
        }
        let boot = signed(self.boot)?;
        let incarnation = signed(expected.worktree.incarnation())?;
        let epoch = signed(expected.epoch)?;
        let activation = expected.activation_id.clone();
        let sql_id = id.as_str().to_owned();
        let op = operation("baseline", id.as_str().as_bytes())?;
        checkpoint(CaptureCheckpoint::BeforeCommit);
        self.authorize(expected, active).await?;
        let capture_tree = expected.worktree.clone();
        let result = self.store.execute(op.clone(),move |tx| {
            if NativeIdentity::read(capture_tree.worktree_path(), capture_tree.repository_root(), capture_tree.git_common_dir()).map(|native| Some(native.key)) != Ok(capture_tree.native_key) { return Ok(false); }
            let current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts s JOIN workspace_authority_clock c ON c.singleton=1 WHERE s.operation=?1 AND s.incarnation=?2 AND s.epoch=?3 AND s.boot=?4 AND c.boot=?4 AND s.active=1)",params![activation,incarnation,epoch,boot],|row|row.get(0))?;
            if !current { return Ok(false); }
            tx.execute("INSERT INTO workspace_baselines(operation,digest,incarnation,epoch,boot,payload,coverage,capture_window) VALUES (?1,?2,?3,?4,?5,?6,'partial','unverified')",params![sql_id,digest.as_slice(),incarnation,epoch,boot,payload])?;
            Ok(true)
        }).await;
        match result {
            Ok(true) => {}
            Ok(false) => return Err(AuthorityError::StaleAuthority.into()),
            Err(error) => self.require_committed(&op, error).await?,
        }
        let (stored_digest, payload) = self
            .baseline_row(id.as_str())
            .await?
            .ok_or(DurableError::CorruptState)?;
        if stored_digest != digest {
            return Err(DurableError::OperationConflict);
        }
        BaselineContext::from_stored(
            id.as_str().to_owned(),
            scope,
            stored_capture_digest(&payload)?,
            task_head_from_payload(&payload)?,
        )
        .map_err(|_| DurableError::CorruptState)
    }

    /// Recovers an immutable committed capture by its exact stable operation, even after root loss/restart.
    /// Scope comes entirely from stored evidence; callers cannot relabel it, and no authority is minted.
    /// Missing operations return None without reading source or consuming a mechanics receipt.
    pub async fn committed_baseline(
        &self,
        id: &OperationId,
    ) -> Result<Option<super::git::BaselineContext>, DurableError> {
        let row = self.store.read_one("SELECT b.incarnation,b.epoch,b.boot,w.nonce,w.native_key,w.root_identity,w.root,w.repository,w.common_dir,b.payload FROM workspace_baselines b JOIN workspace_worktrees w ON w.incarnation=b.incarnation WHERE b.operation=?1 AND b.coverage='partial' AND b.capture_window='unverified'", vec![Value::Text(id.as_str().to_owned())], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,Vec<u8>>(3)?,row.get::<_,Vec<u8>>(4)?,row.get::<_,Vec<u8>>(5)?,row.get::<_,Vec<u8>>(6)?,row.get::<_,Vec<u8>>(7)?,row.get::<_,Vec<u8>>(8)?,row.get::<_,Vec<u8>>(9)?))).await?;
        let Some((
            incarnation,
            epoch,
            boot,
            nonce,
            key,
            root_identity,
            root,
            repository,
            common,
            payload,
        )) = row
        else {
            return Ok(None);
        };
        unsigned(boot)?;
        let mut tree = WorktreeRef::from_discovery(
            PathBuf::from(std::ffi::OsStr::from_bytes(&root)),
            PathBuf::from(std::ffi::OsStr::from_bytes(&repository)),
            PathBuf::from(std::ffi::OsStr::from_bytes(&common)),
            unsigned(incarnation)?,
        )
        .map_err(|_| DurableError::CorruptState)?;
        tree.native_key = Some(key.try_into().map_err(|_| DurableError::CorruptState)?);
        tree.native_root_identity = Some(
            root_identity
                .try_into()
                .map_err(|_| DurableError::CorruptState)?,
        );
        tree.set_durable_nonce(nonce.try_into().map_err(|_| DurableError::CorruptState)?);
        super::git::BaselineContext::from_stored(
            id.as_str().to_owned(),
            super::git::GitScope::from_historical(tree, unsigned(epoch)?),
            stored_capture_digest(&payload)?,
            task_head_from_payload(&payload)?,
        )
        .map(Some)
        .map_err(|_| DurableError::CorruptState)
    }

    /// Reads an exact partial/unverified stored capture without allocating a mechanics receipt.
    async fn baseline_row(&self, id: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>, DurableError> {
        Ok(self.store.read_one("SELECT digest,payload FROM workspace_baselines WHERE operation=?1 AND coverage='partial' AND capture_window='unverified'",vec![Value::Text(id.to_owned())],|row|Ok((row.get(0)?,row.get(1)?))).await?)
    }
}

/// Appends a length-framed raw field only when the entire durable payload remains under its hard cap.
fn frame(payload: &mut Vec<u8>, bytes: &[u8]) -> Result<(), DurableError> {
    if payload.len().saturating_add(8).saturating_add(bytes.len()) > MAX_BASELINE_BYTES {
        return Err(DurableError::InvalidCapture);
    }
    payload.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    payload.extend_from_slice(bytes);
    Ok(())
}

/// Rejects corrupt over-limit/empty persisted payloads before minting baseline provenance.
fn stored_capture_digest(payload: &[u8]) -> Result<[u8; 32], DurableError> {
    if payload.is_empty() || payload.len() > MAX_BASELINE_BYTES {
        return Err(DurableError::CorruptState);
    }
    Ok(*blake3::hash(payload).as_bytes())
}

/// Recovers the bounded activation commit identity from a previously validated baseline payload.
fn task_head_from_payload(payload: &[u8]) -> Result<Option<super::git::GitIdentity>, DurableError> {
    let mut offset: usize = 0;
    let mut take = || -> Result<&[u8], DurableError> {
        let length_end = offset.checked_add(8).ok_or(DurableError::CorruptState)?;
        let length = u64::from_le_bytes(
            payload
                .get(offset..length_end)
                .ok_or(DurableError::CorruptState)?
                .try_into()
                .map_err(|_| DurableError::CorruptState)?,
        );
        offset = length_end;
        let end = offset
            .checked_add(usize::try_from(length).map_err(|_| DurableError::CorruptState)?)
            .ok_or(DurableError::CorruptState)?;
        let value = payload.get(offset..end).ok_or(DurableError::CorruptState)?;
        offset = end;
        Ok(value)
    };
    if take()? != b"workspace-baseline-partial-unverified-v1" {
        return Err(DurableError::CorruptState);
    }
    let count = u64::from_le_bytes(take()?.try_into().map_err(|_| DurableError::CorruptState)?);
    if count > 6 {
        return Err(DurableError::CorruptState);
    }
    let mut head = None;
    for _ in 0..count {
        let tag = take()?;
        let _operation = take()?;
        let exit = i32::from_le_bytes(take()?.try_into().map_err(|_| DurableError::CorruptState)?);
        let stdout = take()?;
        let _stderr = take()?;
        if tag.first() == Some(&(super::git::GitReadQuery::HeadIdentity as u8))
            && tag.get(1) == Some(&0)
            && exit == 0
        {
            head = stdout
                .strip_suffix(b"\n")
                .and_then(|bytes| super::git::GitIdentity::new(bytes.to_vec()).ok());
        }
    }
    Ok(head)
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
