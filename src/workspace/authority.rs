//! Worktree identity and revocable actor authority.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    path::{Component, Path, PathBuf},
};

use crate::assistance::host_binding::{ActiveBindingUse, BindingRef, ValidatedInvocation};

const MAX_OPERATION_ID_BYTES: usize = 128;

/// Carries a raw worktree incarnation; only durable-resolved values qualify for product authority.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WorktreeRef {
    /// Opaque digest of raw identity paths and this lifecycle incarnation.
    id: String,
    /// Lifecycle discriminator minted by the durable resolver for product identities.
    incarnation: u64,
    /// Raw Unix path of the mutable worktree root.
    worktree_path: PathBuf,
    /// Raw Unix path of the repository root observed by Git discovery.
    repository_root: PathBuf,
    /// Raw Git common-directory value, which may be relative to the discovered worktree root.
    git_common_dir: PathBuf,
    /// Native identity evidence minted only by the durable Workspace resolver.
    pub(super) native_key: Option<[u8; 32]>,
    /// Descriptor-derived root identity minted by durable resolution, including creation time.
    pub(super) native_root_identity: Option<[u8; 32]>,
}

impl WorktreeRef {
    /// Builds an immutable worktree reference from separately discovered Git identity paths.
    ///
    /// Worktree and repository-root paths must be lexically absolute without `.` or `..`
    /// components. Git may report a relative common directory such as `.git`, so that raw value
    /// need only be nonempty and is retained unchanged beside the absolute worktree root. The
    /// caller-supplied incarnation creates an unverified fixture/reference only. Durable activation
    /// rejects it; product identities are minted by `DurableWorkspace::resolve_worktree`.
    pub fn from_discovery(
        worktree_path: PathBuf,
        repository_root: PathBuf,
        git_common_dir: PathBuf,
        incarnation: u64,
    ) -> Result<Self, AuthorityError> {
        if incarnation == 0
            || !is_normal_absolute(&worktree_path)
            || !is_normal_absolute(&repository_root)
            || git_common_dir.as_os_str().is_empty()
        {
            return Err(AuthorityError::InvalidWorktreeIdentity);
        }
        let id = identity_id(
            &worktree_path,
            &repository_root,
            &git_common_dir,
            incarnation,
        );
        Ok(Self {
            id,
            incarnation,
            worktree_path,
            repository_root,
            git_common_dir,
            native_key: None,
            native_root_identity: None,
        })
    }

    /// Returns this opaque identity, which is stable only for these exact paths and incarnation.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the lifecycle incarnation that distinguishes recreated worktrees at one path.
    pub const fn incarnation(&self) -> u64 {
        self.incarnation
    }

    /// Returns the exact raw Unix worktree path supplied by Git discovery.
    pub fn worktree_path(&self) -> &Path {
        &self.worktree_path
    }

    /// Returns the exact raw Unix repository root supplied by Git discovery.
    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    /// Returns the exact raw Git common-directory value supplied by Git discovery.
    pub fn git_common_dir(&self) -> &Path {
        &self.git_common_dir
    }
}

/// Carries one actor/binding/worktree authority claim.
/// Registry-only claims are not product authority; every use requires DurableWorkspace validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityStamp {
    /// Canonical worktree incarnation owned by this actor.
    pub(super) worktree: WorktreeRef,
    /// Assistance binding generation that must remain live for scoped use.
    pub(super) binding: BindingRef,
    /// Host-validated actor identity retained only for exclusivity checks.
    pub(super) actor_id: String,
    /// Monotonic authority generation for stale-peer fencing.
    pub(super) epoch: u64,
    /// Stable activation operation ID for idempotent retry correlation.
    pub(super) activation_id: String,
    /// Durable owner boot generation; absent for the non-authoritative in-process helper.
    pub(super) owner_boot: Option<u64>,
}

impl AuthorityStamp {
    /// Returns the worktree and incarnation to which this authority is permanently bound.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the active Assistance binding generation required for every scoped use.
    pub fn binding(&self) -> &BindingRef {
        &self.binding
    }

    /// Returns the host-validated actor that owns this authority.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Returns the monotonic Workspace authority epoch used to reject stale peer requests.
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Returns the stable activation operation ID that makes an in-flight start retry idempotent.
    pub fn activation_id(&self) -> &str {
        &self.activation_id
    }
}

/// Carries all Assistance-derived values that are required to request one authority grant.
#[derive(Debug)]
pub struct ActivationRequest {
    /// Stable bounded activation operation ID supplied by Assistance's start request.
    pub(super) activation_id: String,
    /// Exact host-validated invocation that established or reactivated this binding generation.
    pub(super) invocation: ValidatedInvocation,
    /// Fresh transient Assistance liveness evidence for the invocation binding.
    pub(super) active_use: ActiveBindingUse,
    /// Candidate reference; durable activation requires native identity minted by Workspace.
    pub(super) worktree: WorktreeRef,
}

impl ActivationRequest {
    /// Validates a bounded stable activation ID and pairs a fresh consumed binding with its invocation.
    ///
    /// The caller must obtain `active_use` from Assistance immediately before this call. This type
    /// verifies only binding equality; it cannot turn model input or an actor string into proof.
    pub fn new(
        activation_id: impl Into<String>,
        invocation: ValidatedInvocation,
        active_use: ActiveBindingUse,
        worktree: WorktreeRef,
    ) -> Result<Self, AuthorityError> {
        let activation_id = activation_id.into();
        if activation_id.is_empty() || activation_id.len() > MAX_OPERATION_ID_BYTES {
            return Err(AuthorityError::InvalidActivationId);
        }
        if invocation.binding_ref() != active_use.binding_ref() {
            return Err(AuthorityError::BindingMismatch);
        }
        Ok(Self {
            activation_id,
            invocation,
            active_use,
            worktree,
        })
    }
}

/// Reports whether Assistance confirmed its live binding was stopped before Workspace finalization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopBindingHandoff {
    /// Assistance stopped the exact binding generation before or atomically with Workspace revocation.
    Confirmed,
    /// Workspace received no trustworthy stop handoff and must fail closed.
    Missing,
}

/// Explains why authority was revoked so direct consumers can fence only the affected view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevocationReason {
    /// The expected current authority was explicitly stopped after Assistance revocation.
    Requested,
    /// Assistance stop handoff was unavailable, so Workspace revoked authority without claiming it settled.
    RevocationIncomplete,
}

/// Is the finite direct Workspace-to-Intelligence revocation value for one worktree incarnation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityRevoked {
    /// Exact worktree incarnation whose logical peer view must be fenced.
    pub(super) worktree: WorktreeRef,
    /// Authority epoch invalidated by this finite direct event.
    pub(super) old_epoch: u64,
    /// Whether the binding stop handoff was confirmed or had to fail closed.
    pub(super) reason: RevocationReason,
}

impl AuthorityRevoked {
    /// Returns the exact worktree incarnation whose logical view must be fenced.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the authority epoch that is no longer usable.
    pub const fn old_epoch(&self) -> u64 {
        self.old_epoch
    }

    /// Returns whether stop handoff was confirmed or had to fail closed.
    pub const fn reason(&self) -> RevocationReason {
        self.reason
    }
}

/// Reports a rejected activation, authority use, or expected-authority stop request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityError {
    /// Git discovery omitted normal absolute worktree/repository paths, a nonempty common-directory value, or a nonzero incarnation.
    InvalidWorktreeIdentity,
    /// The activation operation ID is empty or exceeds the bounded local identifier limit.
    InvalidActivationId,
    /// Assistance invocation and consumed liveness evidence name different binding generations.
    BindingMismatch,
    /// A retried activation ID changed its actor, binding, or worktree identity.
    ActivationConflict,
    /// The same activation was already stopped and must not silently reactivate authority.
    ActivationStopped,
    /// Another actor currently owns the requested mutable worktree incarnation.
    WorktreeOwned,
    /// The requested actor already owns a different active worktree.
    ActorAlreadyOwnsWorktree,
    /// The stamp is no longer the exact active authority expected by this operation.
    StaleAuthority,
    /// Fresh Assistance liveness evidence names a different binding generation than the stamp.
    BindingNotCurrent,
    /// The authority epoch space cannot advance without wrapping.
    EpochExhausted,
}

/// Maintains bounded in-process fixture/cache state; it is not a product authority owner.
/// Its stamps cannot satisfy `DurableWorkspace` admission; product callers require durable receipts.
#[derive(Debug, Default)]
pub struct AuthorityRegistry {
    /// Every submitted activation operation and whether its authority remains active.
    activations: BTreeMap<String, ActivationState>,
    /// Current exclusive authority keyed by full worktree incarnation.
    active_worktrees: BTreeMap<WorktreeRef, AuthorityStamp>,
    /// Current worktree keyed by host-validated actor identity.
    active_actors: BTreeMap<String, WorktreeRef>,
    /// Last issued authority epoch, advanced without wrapping.
    next_epoch: u64,
}

/// Retains the idempotent result of one stable activation operation after revocation.
#[derive(Debug)]
struct ActivationState {
    /// Authority granted for this activation operation.
    stamp: AuthorityStamp,
    /// Whether this activation still owns its worktree and can authorize scoped work.
    active: bool,
}

impl AuthorityRegistry {
    /// Grants one new authority or returns the same still-active result for an identical activation retry.
    ///
    /// The registry enforces one mutable worktree per actor and one actor per worktree. It records
    /// only non-authoritative in-memory state. Product callers must use `DurableWorkspace`
    /// for canonical identity, durable start receipts, and every current authority admission.
    pub fn activate(
        &mut self,
        request: ActivationRequest,
    ) -> Result<AuthorityStamp, AuthorityError> {
        if let Some(previous) = self.activations.get(&request.activation_id) {
            if previous.stamp.actor_id != request.invocation.actor_id()
                || previous.stamp.binding != *request.active_use.binding_ref()
                || previous.stamp.worktree != request.worktree
            {
                return Err(AuthorityError::ActivationConflict);
            }
            return previous
                .active
                .then_some(previous.stamp.clone())
                .ok_or(AuthorityError::ActivationStopped);
        }
        if self.active_worktrees.contains_key(&request.worktree) {
            return Err(AuthorityError::WorktreeOwned);
        }
        if self
            .active_actors
            .contains_key(request.invocation.actor_id())
        {
            return Err(AuthorityError::ActorAlreadyOwnsWorktree);
        }
        self.next_epoch = self
            .next_epoch
            .checked_add(1)
            .ok_or(AuthorityError::EpochExhausted)?;
        let stamp = AuthorityStamp {
            worktree: request.worktree,
            binding: request.active_use.binding_ref().clone(),
            actor_id: request.invocation.actor_id().to_owned(),
            epoch: self.next_epoch,
            activation_id: request.activation_id,
            owner_boot: None,
        };
        self.active_actors
            .insert(stamp.actor_id.clone(), stamp.worktree.clone());
        self.active_worktrees
            .insert(stamp.worktree.clone(), stamp.clone());
        self.activations.insert(
            stamp.activation_id.clone(),
            ActivationState {
                stamp: stamp.clone(),
                active: true,
            },
        );
        Ok(stamp)
    }

    /// Verifies that a still-active stamp is accompanied by freshly consumed matching binding liveness.
    ///
    /// Assistance remains responsible for proving that `active_use` is fresh. Workspace requires
    /// it on every scoped request so a cached actor ID or stale binding cannot authorize access.
    pub fn authorize(
        &self,
        stamp: &AuthorityStamp,
        active_use: &ActiveBindingUse,
    ) -> Result<(), AuthorityError> {
        if active_use.binding_ref() != &stamp.binding {
            return Err(AuthorityError::BindingNotCurrent);
        }
        (self.active_worktrees.get(&stamp.worktree) == Some(stamp))
            .then_some(())
            .ok_or(AuthorityError::StaleAuthority)
    }

    /// Revokes exactly the expected authority and produces the direct logical-view fencing value.
    ///
    /// The caller must arrange Assistance `stop_binding` before or atomically with this call. A
    /// missing handoff still removes authority, reports `RevocationIncomplete`, and cannot be used
    /// to reactivate or consume a binding during finalization.
    pub fn revoke(
        &mut self,
        expected: &AuthorityStamp,
        handoff: StopBindingHandoff,
    ) -> Result<AuthorityRevoked, AuthorityError> {
        if self.active_worktrees.get(&expected.worktree) != Some(expected) {
            return Err(AuthorityError::StaleAuthority);
        }
        self.active_worktrees.remove(&expected.worktree);
        self.active_actors.remove(&expected.actor_id);
        if let Some(state) = self.activations.get_mut(&expected.activation_id) {
            state.active = false;
        }
        Ok(AuthorityRevoked {
            worktree: expected.worktree.clone(),
            old_epoch: expected.epoch,
            reason: match handoff {
                StopBindingHandoff::Confirmed => RevocationReason::Requested,
                StopBindingHandoff::Missing => RevocationReason::RevocationIncomplete,
            },
        })
    }
}

/// Checks an absolute Unix path without normalizing or decoding its raw bytes.
fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

/// Hashes raw identity path bytes and incarnation into one opaque local worktree identifier.
fn identity_id(
    worktree_path: &Path,
    repository_root: &Path,
    git_common_dir: &Path,
    incarnation: u64,
) -> String {
    let mut hasher = blake3::Hasher::new();
    for path in [worktree_path, repository_root, git_common_dir] {
        hasher.update(OsStr::as_bytes(path.as_os_str()));
        hasher.update(&[0]);
    }
    hasher.update(&incarnation.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}
