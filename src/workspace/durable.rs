//! SQLite-owned physical worktree identities, boot-fenced authority, and bounded baseline capture.

use super::authority::{
    ActivationRequest, AuthorityError, AuthorityRevoked, AuthorityStamp, RevocationReason,
    StopBindingHandoff, WorktreeRef,
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
    fs::{self, File},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
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
    IdentityUnavailable,
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
}
impl StartReceipt {
    /// Returns the canonical worktree whose historical grant this receipt records.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }
    /// Returns the stable activation operation identifier.
    pub fn operation(&self) -> &str {
        &self.operation
    }
    /// Returns the committed epoch without asserting it is still active.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Sole product admission owner for one Application Store boot; clones share the same boot fence.
/// Opening another owner deliberately fences prior claims. Product wiring must hold one owner,
/// use canonical resolution before start, and call authority/authorize for every scoped admission.
#[derive(Clone, Copy)]
pub struct DurableWorkspace<'a> {
    /// Application-owned SQLite connection and bounded submission mechanics.
    store: &'a Store,
    /// Durable boot generation minted during this owner's initialization.
    boot: u64,
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
        let mut nonce = [0; 32];
        File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut nonce))
            .map_err(|error| StoreError::Infrastructure(error.to_string()))?;
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
        })
    }

    /// Resolves real directories without following symlinks and mints or reuses their stored incarnation.
    /// Caller-supplied incarnation values are never accepted. Moves require explicit later reconciliation;
    /// same-path recreation with changed native identity receives a new SQLite-allocated incarnation.
    pub async fn resolve_worktree(
        &self,
        root: PathBuf,
        repository: PathBuf,
        common_dir: PathBuf,
    ) -> Result<WorktreeRef, DurableError> {
        let native = NativeIdentity::read(&root, &repository, &common_dir)?;
        let physical = native.physical;
        let key = native.key;
        let root = native.root.as_os_str().as_bytes().to_vec();
        let repository = native.repository.as_os_str().as_bytes().to_vec();
        let common = native.common.as_os_str().as_bytes().to_vec();
        let boot = signed(self.boot)?;
        let op = operation("identity", &key)?;
        let result = self.store.execute(op.clone(), move |tx| {
            if current_boot(tx)? != boot { return Ok(None); }
            tx.execute("INSERT OR IGNORE INTO workspace_worktrees(physical_key,native_key,root,repository,common_dir) VALUES (?1,?2,?3,?4,?5)", params![physical.as_slice(),key.as_slice(),root,repository,common])?;
            tx.query_row("SELECT incarnation,native_key FROM workspace_worktrees WHERE physical_key=?1", [physical.as_slice()], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Vec<u8>>(1)?))).optional()
        }).await;
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                self.store.read_one("SELECT incarnation,native_key FROM workspace_worktrees WHERE physical_key=?1", vec![Value::Blob(physical.to_vec())], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,Vec<u8>>(1)?))).await?
            }
        }.ok_or(AuthorityError::StaleAuthority)?;
        if row.1 != key {
            return Err(DurableError::IdentityUnavailable);
        }
        let mut tree = WorktreeRef::from_discovery(
            native.root,
            native.repository,
            native.common,
            unsigned(row.0)?,
        )?;
        tree.native_key = Some(key);
        tree.native_root_identity = Some(native.root_identity);
        Ok(tree)
    }

    /// Commits one exact host-bound start or returns its original immutable receipt on an exact retry.
    /// The stable identity excludes per-call IDs so a fresh host call can retry the same actor/binding/worktree intent.
    /// Historical receipts survive restart but cannot mint current authority. A binding used by a prior
    /// boot or a stopped grant cannot start a new operation; a fresh host binding is required.
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
        let incarnation = signed(request.worktree.incarnation())?;
        let boot = signed(self.boot)?;
        let op = operation("start", id.as_bytes())?;
        let sql_actor = actor.clone();
        let sql_id = id.clone();
        let native_key = request
            .worktree
            .native_key
            .ok_or(DurableError::IdentityUnavailable)?;
        let result = self.store.execute(op.clone(), move |tx| {
            let outcome = if current_boot(tx)? != boot { "stale" }
                else if !tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_worktrees WHERE incarnation=?1 AND native_key=?2)", params![incarnation,native_key.as_slice()], |row| row.get::<_,bool>(0))? { "identity" }
                else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE binding=?1 AND outcome='granted' AND (boot<>?2 OR active=0))", params![binding.as_slice(),boot], |row| row.get::<_,bool>(0))? { "binding" }
                else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE incarnation=?1 AND active=1)", [incarnation], |row| row.get::<_,bool>(0))? { "worktree_owned" }
                else if tx.query_row("SELECT EXISTS(SELECT 1 FROM workspace_starts WHERE actor=?1 AND active=1)", [&sql_actor], |row| row.get::<_,bool>(0))? { "actor_owned" }
                else { "granted" };
            let epoch = if outcome == "granted" {
                let epoch: i64 = tx.query_row("SELECT epoch FROM workspace_authority_clock WHERE singleton=1", [], |row| row.get(0))?;
                let epoch = epoch.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
                tx.execute("UPDATE workspace_authority_clock SET epoch=?1 WHERE singleton=1", [epoch])?;
                epoch
            } else { 0 };
            tx.execute("INSERT INTO workspace_starts(operation,digest,incarnation,actor,binding,boot,epoch,outcome,active) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![sql_id,digest.as_slice(),incarnation,sql_actor,binding.as_slice(),boot,epoch,outcome,i64::from(outcome=="granted")])?;
            Ok((digest.to_vec(),boot,epoch,outcome.to_owned()))
        }).await;
        let row = match result {
            Ok(row) => row,
            Err(error) => {
                self.require_committed(&op, error).await?;
                self.store
                    .read_one(
                        "SELECT digest,boot,epoch,outcome FROM workspace_starts WHERE operation=?1",
                        vec![Value::Text(id.clone())],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, String>(3)?,
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
        if row.3 != "granted" {
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
        })
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
        if tree.native_key == Some(native.key) {
            Ok(())
        } else {
            Err(DurableError::IdentityUnavailable)
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

/// Native directory identity plus canonical raw paths used to reject aliases and lifecycle guesses.
struct NativeIdentity {
    /// Canonical worktree root with every symlink component refused.
    root: PathBuf,
    /// Creation-aware identity of the same opened root used for the durable native key.
    root_identity: [u8; 32],
    /// Canonical discovered repository root.
    repository: PathBuf,
    /// Canonical Git common directory.
    common: PathBuf,
    /// Stable root device/inode key independent of caller-supplied incarnation or Git path guesses.
    physical: [u8; 32],
    /// Fingerprint of root, repository, common-directory native identities and canonical paths.
    key: [u8; 32],
}
impl NativeIdentity {
    /// Reads exactly the three discovered directories; no repository scan or process is launched.
    fn read(root: &Path, repository: &Path, common: &Path) -> Result<Self, DurableError> {
        let (root, root_id, root_identity) = real_directory_with_root_identity(root)?;
        let (repository, repo_id) = real_directory(repository)?;
        let (common, common_id) = real_directory(&if common.is_absolute() {
            common.to_path_buf()
        } else {
            root.join(common)
        })?;
        let physical = fingerprint(&[b"physical-root", &root_id]);
        let key = fingerprint(&[
            b"native-worktree",
            &physical,
            &repo_id,
            &common_id,
            root.as_os_str().as_bytes(),
            repository.as_os_str().as_bytes(),
            common.as_os_str().as_bytes(),
        ]);
        Ok(Self {
            root,
            root_identity,
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
    let (path, native, _) = real_directory_with_root_identity(path)?;
    Ok((path, native))
}

/// Resolves a directory while retaining creation-aware identity from that exact opened descriptor.
fn real_directory_with_root_identity(
    path: &Path,
) -> Result<(PathBuf, [u8; 16], [u8; 32]), DurableError> {
    if !path.is_absolute() {
        return Err(DurableError::IdentityUnavailable);
    }
    let mut directory = File::open("/").map_err(|_| DurableError::IdentityUnavailable)?;
    for component in path.components() {
        if component == std::path::Component::RootDir {
            continue;
        }
        let next = super::observation::open_directory(directory.as_raw_fd(), component.as_os_str())
            .map_err(|_| DurableError::IdentityUnavailable)?;
        // SAFETY: open_directory returned a new owned descriptor; File closes the previous one on assignment.
        directory = unsafe { File::from_raw_fd(next) };
    }
    let root_identity = super::observation::native_directory_identity(&directory)
        .map_err(|_| DurableError::IdentityUnavailable)?;
    let metadata = directory
        .metadata()
        .map_err(|_| DurableError::IdentityUnavailable)?;
    let canonical = fs::canonicalize(path).map_err(|_| DurableError::IdentityUnavailable)?;
    let current =
        fs::symlink_metadata(&canonical).map_err(|_| DurableError::IdentityUnavailable)?;
    if !current.is_dir() || current.dev() != metadata.dev() || current.ino() != metadata.ino() {
        return Err(DurableError::IdentityUnavailable);
    }
    let mut native = [0; 16];
    native[..8].copy_from_slice(&metadata.dev().to_le_bytes());
    native[8..].copy_from_slice(&metadata.ino().to_le_bytes());
    Ok((canonical, native, root_identity))
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
        "identity" => DurableError::IdentityUnavailable,
        "worktree_owned" => AuthorityError::WorktreeOwned.into(),
        "actor_owned" => AuthorityError::ActorAlreadyOwnsWorktree.into(),
        _ => DurableError::CorruptState,
    }
}

/// Maximum number of explicitly registered source paths in one stored baseline capture.
pub const MAX_BASELINE_PATHS: usize = 128;
/// Maximum framed Git/source payload stored for one baseline, including raw metadata.
pub const MAX_BASELINE_BYTES: usize = 4 * 1024 * 1024;

impl DurableWorkspace<'_> {
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
        use super::{
            git::{BaselineContext, DiffMode, GitScope},
            observation::{SourceReadLimits, read_authorized_source, valid_relative_path},
        };
        self.authorize(expected, active).await?;
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
            &self.boot.to_le_bytes(),
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
            )
            .map_err(|_| DurableError::CorruptState);
        }
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
                        super::observation::ObservationError::TooLarge => b"too_large",
                        super::observation::ObservationError::SymlinkEscape => b"symlink",
                        super::observation::ObservationError::NotRegularFile => b"not_regular",
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
        let result = self.store.execute(op.clone(),move |tx| {
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
        )
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
