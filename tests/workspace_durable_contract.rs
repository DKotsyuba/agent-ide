//! SQLite reopen, canonical native identity, and bounded baseline admission contracts.

use agent_ide::{
    app::{
        config::StoreConfig,
        store::{OperationId, Store},
    },
    assistance::host_binding::{
        BindingStatus, HostBindingGuard, ValidatedInvocation, parse_candidate,
        parse_channel_session, parse_hook_event,
    },
    workspace::{
        authority::{
            ActivationRequest, AuthorityError, AuthorityRegistry, AuthorityStamp, StartRole,
            StopBindingHandoff, WorktreeRef,
        },
        durable::{DurableError, DurableWorkspace, StartReceipt},
        git::{
            BaselineContext, BaselineCoverage, BaselineWindow, DiffMode, GitError, GitReadQuery,
            GitScope, RawGitEvidence,
        },
    },
};
use serde_json::json;
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

/// The closed identity refusal with no holder, for legacy identity-failure assertions.
fn identity_unavailable() -> DurableError {
    DurableError::IdentityUnavailable {
        step: "identity_read",
        holder: None,
    }
}

/// Separates disposable fixture roots within the current process.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Owns the exact temporary directories and database used by one contract test.
struct Fixture {
    /// Private test root whose descendants may be removed on drop.
    base: PathBuf,
    /// Real mutable worktree root, with a real `.git` common directory.
    root: PathBuf,
}
impl Fixture {
    /// Creates real directories beneath `/private/tmp`, avoiding Darwin's `/tmp` symlink alias.
    fn new() -> Self {
        let base = PathBuf::from(format!(
            "/private/tmp/workspace-durable-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("worktree");
        fs::create_dir_all(root.join(".git")).unwrap();
        Self { base, root }
    }
    /// Opens the same SQLite database with bounded receipts and a private migration backup root.
    fn store(&self) -> Store {
        Store::open_with_backup_root(
            &self.base.join("state.sqlite"),
            &self.base.join("backups"),
            StoreConfig {
                queue_capacity: 64,
                busy_timeout: Duration::from_secs(1),
                request_deadline: Duration::from_secs(2),
                receipt_capacity: 256,
            },
        )
        .unwrap()
    }
    /// Resolves the exact physical worktree using a relative Git common-directory discovery value.
    async fn resolve(&self, owner: &DurableWorkspace<'_>) -> WorktreeRef {
        owner
            .resolve_worktree(self.root.clone(), self.root.clone(), PathBuf::from(".git"))
            .await
            .unwrap()
    }
}
impl Drop for Fixture {
    /// Removes only this fixture's uniquely owned directory tree.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Creates a validated local test binding from matching pre-hook and private-channel metadata.
fn binding(actor: &str, call: &str, channel: &str) -> (HostBindingGuard, ValidatedInvocation) {
    let mut guard = HostBindingGuard::default();
    let invocation = fresh_call(&mut guard, actor, call, channel);
    (guard, invocation)
}

/// Validates a fresh call under a live actor/channel binding instead of replaying a completed call ID.
fn fresh_call(
    guard: &mut HostBindingGuard,
    actor: &str,
    call: &str,
    channel: &str,
) -> ValidatedInvocation {
    let channel = parse_channel_session(channel.as_bytes()).unwrap();
    let hook = parse_hook_event(
        json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":call})
            .to_string()
            .as_bytes(),
    )
    .unwrap();
    assert!(matches!(
        guard.observe_hook(hook, channel.clone()),
        BindingStatus::PreObserved
    ));
    let candidate = parse_candidate(
        json!({"threadId":actor,"callId":call,"x-codex-turn-metadata":{"turn":"durable-contract"}})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
        panic!("validated fixture")
    };
    invocation
}

/// Requests one exact activation with a fresh consumed binding use and a supplied canonical reference.
fn request(
    id: &str,
    guard: &mut HostBindingGuard,
    invocation: &ValidatedInvocation,
    tree: &WorktreeRef,
) -> ActivationRequest {
    request_mode(id, false, guard, invocation, tree)
}

/// Requests a writer or explicit read-only activation with a fresh consumed binding use.
fn request_mode(
    id: &str,
    read_only: bool,
    guard: &mut HostBindingGuard,
    invocation: &ValidatedInvocation,
    tree: &WorktreeRef,
) -> ActivationRequest {
    ActivationRequest::new(
        id,
        read_only,
        invocation.clone(),
        guard.consume_active(invocation.binding_ref()).unwrap(),
        tree.clone(),
    )
    .unwrap()
}

/// Obtains a canonical durable authority for an isolated actor and channel.
async fn activate(
    owner: &DurableWorkspace<'_>,
    tree: &WorktreeRef,
    id: &str,
    actor: &str,
) -> (
    HostBindingGuard,
    ValidatedInvocation,
    AuthorityStamp,
    StartReceipt,
) {
    let (mut guard, invocation) = binding(actor, id, id);
    let receipt = owner
        .activate(request(id, &mut guard, &invocation, tree))
        .await
        .unwrap();
    let stamp = owner
        .authority(
            &receipt,
            &guard.consume_active(invocation.binding_ref()).unwrap(),
        )
        .await
        .unwrap();
    (guard, invocation, stamp, receipt)
}

/// Role changes with new activation IDs preserve authority and release the writer slot on stop.
#[tokio::test]
async fn role_switch_rebinds_active_start_to_new_activation_id() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let (mut guard, invocation) = binding("actor", "call-a", "channel-a");

    let reader = owner
        .activate(request_mode(
            "reader-a",
            true,
            &mut guard,
            &invocation,
            &tree,
        ))
        .await
        .unwrap();
    assert_eq!(reader.role(), StartRole::Reader);
    let writer = owner
        .activate(request("writer-b", &mut guard, &invocation, &tree))
        .await
        .unwrap();
    assert_eq!(writer.role(), StartRole::Writer);
    assert!(
        owner
            .authority(
                &writer,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await
            .is_ok()
    );
    let reader = owner
        .activate(request_mode(
            "reader-c",
            true,
            &mut guard,
            &invocation,
            &tree,
        ))
        .await
        .unwrap();
    assert_eq!(reader.role(), StartRole::Reader);
    assert!(
        owner
            .authority(
                &reader,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await
            .is_ok()
    );
    owner
        .revoke(
            OperationId::new("stop-reader-c").unwrap(),
            &reader,
            StopBindingHandoff::Confirmed,
        )
        .await
        .unwrap();

    let (mut next_guard, next_invocation) = binding("next", "call-next", "channel-next");
    let next_writer = owner
        .activate(request(
            "next-writer",
            &mut next_guard,
            &next_invocation,
            &tree,
        ))
        .await
        .unwrap();
    assert_eq!(next_writer.role(), StartRole::Writer);
}

/// Fences old boot stamps/bindings while preserving exact historical start and stop outcomes.
#[tokio::test]
async fn reopen_never_revives_authority_and_retries_preserve_committed_receipts() {
    let fixture = Fixture::new();
    let (mut guard, invocation) = binding("actor", "start-original", "channel-original");
    let (original, old_stamp) = {
        let store = fixture.store();
        let owner = DurableWorkspace::open(&store).await.unwrap();
        let tree = fixture.resolve(&owner).await;
        let receipt = owner
            .activate(request("start-original", &mut guard, &invocation, &tree))
            .await
            .unwrap();
        let retry_call = fresh_call(&mut guard, "actor", "start-retry-call", "channel-original");
        assert_eq!(
            receipt,
            owner
                .activate(request("start-original", &mut guard, &retry_call, &tree))
                .await
                .unwrap()
        );
        assert_eq!(
            receipt,
            owner
                .activate(request("start-original", &mut guard, &invocation, &tree))
                .await
                .unwrap()
        );
        let stamp = owner
            .authority(
                &receipt,
                &guard.consume_active(invocation.binding_ref()).unwrap(),
            )
            .await
            .unwrap();
        (receipt, stamp)
    };
    let (stopped_stamp, stop_receipt) = {
        let store = fixture.store();
        let owner = DurableWorkspace::open(&store).await.unwrap();
        let tree = fixture.resolve(&owner).await;
        assert_eq!(&tree, original.worktree());
        assert_eq!(
            owner
                .authorize(
                    &old_stamp,
                    &guard.consume_active(invocation.binding_ref()).unwrap()
                )
                .await,
            Err(DurableError::Authority(AuthorityError::StaleAuthority))
        );
        let retry_after_restart = fresh_call(
            &mut guard,
            "actor",
            "restart-retry-call",
            "channel-original",
        );
        assert_eq!(
            original,
            owner
                .activate(request(
                    "start-original",
                    &mut guard,
                    &retry_after_restart,
                    &tree
                ))
                .await
                .unwrap()
        );
        let (mut changed_binding, changed_invocation) =
            binding("actor", "changed-call", "changed-channel");
        assert_eq!(
            owner
                .activate(request(
                    "start-original",
                    &mut changed_binding,
                    &changed_invocation,
                    &tree
                ))
                .await,
            Err(DurableError::OperationConflict)
        );
        let changed_root = fixture.base.join("changed-worktree");
        fs::create_dir_all(changed_root.join(".git")).unwrap();
        let changed_tree = owner
            .resolve_worktree(
                changed_root.clone(),
                changed_root.clone(),
                changed_root.join(".git"),
            )
            .await
            .unwrap();
        assert_eq!(
            owner
                .activate(request(
                    "start-original",
                    &mut guard,
                    &retry_after_restart,
                    &changed_tree
                ))
                .await,
            Err(DurableError::OperationConflict)
        );
        let recovered = owner
            .activate(request("start-original", &mut guard, &invocation, &tree))
            .await
            .unwrap();
        assert_eq!(recovered, original);
        assert_eq!(
            owner
                .authority(
                    &recovered,
                    &guard.consume_active(invocation.binding_ref()).unwrap()
                )
                .await,
            Err(DurableError::Authority(AuthorityError::StaleAuthority))
        );
        assert_eq!(
            owner
                .activate(request(
                    "new-id-old-binding",
                    &mut guard,
                    &invocation,
                    &tree
                ))
                .await,
            Err(DurableError::Authority(AuthorityError::BindingNotCurrent))
        );
        let (mut fresh, invocation, new_stamp, new_receipt) =
            activate(&owner, &tree, "fresh-start", "actor").await;
        assert!(new_stamp.epoch() > old_stamp.epoch());
        let cached_use = fresh.consume_active(invocation.binding_ref()).unwrap();
        fresh.stop_binding(new_stamp.binding()).unwrap();
        let stopped = owner
            .revoke(
                OperationId::new("stop-fresh").unwrap(),
                &new_receipt,
                StopBindingHandoff::Confirmed,
            )
            .await
            .unwrap();
        assert_eq!(
            owner.authorize(&new_stamp, &cached_use).await,
            Err(DurableError::Authority(AuthorityError::StaleAuthority))
        );
        assert_eq!(
            owner
                .revoke(
                    OperationId::new("stop-fresh").unwrap(),
                    &new_receipt,
                    StopBindingHandoff::Confirmed
                )
                .await
                .unwrap(),
            stopped
        );
        assert_eq!(
            owner
                .revoke(
                    OperationId::new("stop-fresh").unwrap(),
                    &new_receipt,
                    StopBindingHandoff::Missing
                )
                .await,
            Err(DurableError::OperationConflict)
        );
        (new_stamp, stopped)
    };
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let (mut stop_guard, stop_invocation) = binding("actor", "stop-recovery-call", "fresh-start");
    let recovered_start = owner
        .activate(request(
            "fresh-start",
            &mut stop_guard,
            &stop_invocation,
            &tree,
        ))
        .await
        .unwrap();
    assert_eq!(
        owner
            .revoke(
                OperationId::new("stop-fresh").unwrap(),
                &recovered_start,
                StopBindingHandoff::Confirmed
            )
            .await
            .unwrap(),
        stop_receipt
    );
    let tree = fixture.resolve(&owner).await;
    let (_, _, next, _) = activate(&owner, &tree, "after-stop-reopen", "actor").await;
    assert!(next.epoch() > stopped_stamp.epoch());
}

/// Makes physical identity canonical, refuses fabricated incarnations/aliases, and detects moves/recreation.
#[tokio::test]
async fn canonical_identity_and_sqlite_ownership_do_not_trust_caller_incarnations() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    assert_eq!(
        tree,
        owner
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                fixture.root.join(".git")
            )
            .await
            .unwrap()
    );
    let (mut first, one) = binding("one", "one", "one");
    let forged = WorktreeRef::from_discovery(
        fixture.root.clone(),
        fixture.root.clone(),
        fixture.root.join(".git"),
        999,
    )
    .unwrap();
    assert_eq!(
        owner
            .activate(request("forged", &mut first, &one, &forged))
            .await,
        Err(identity_unavailable())
    );
    let alias = fixture.base.join("alias");
    std::os::unix::fs::symlink(&fixture.root, &alias).unwrap();
    assert_eq!(
        owner
            .resolve_worktree(alias.clone(), alias.clone(), alias.join(".git"))
            .await,
        Err(identity_unavailable())
    );
    let (mut second, two) = binding("two", "two", "two");
    let (left, right) = tokio::join!(
        owner.activate(request("one", &mut first, &one, &tree)),
        owner.activate(request_mode("two", true, &mut second, &two, &tree))
    );
    // An explicit reader coexists with the writer without taking its unique slot (E013 item 2).
    let (receipt, reader, guard, invocation, reader_guard, reader_invocation) =
        match (&left, &right) {
            (Ok(left), Ok(right)) if left.role() == StartRole::Writer => (
                left.clone(),
                right.clone(),
                &mut first,
                &one,
                &mut second,
                &two,
            ),
            (Ok(left), Ok(right)) => (
                right.clone(),
                left.clone(),
                &mut second,
                &two,
                &mut first,
                &one,
            ),
            _ => panic!("one writer and one reader must both be admitted: {left:?} {right:?}"),
        };
    assert_eq!(receipt.role(), StartRole::Writer);
    assert_eq!(reader.role(), StartRole::Reader);
    assert_eq!(reader.epoch(), receipt.epoch());
    let stamp = owner
        .authority(
            &receipt,
            &guard.consume_active(invocation.binding_ref()).unwrap(),
        )
        .await
        .unwrap();
    let reader_stamp = owner
        .authority(
            &reader,
            &reader_guard
                .consume_active(reader_invocation.binding_ref())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reader_stamp.role(), StartRole::Reader);
    let mut registry = AuthorityRegistry::default();
    let helper = registry
        .activate(request("helper", guard, invocation, &tree))
        .unwrap();
    assert_eq!(
        owner
            .authorize(
                &helper,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await,
        Err(DurableError::Authority(AuthorityError::StaleAuthority))
    );
    let moved = fixture.base.join("moved");
    fs::rename(&fixture.root, &moved).unwrap();
    assert_eq!(
        owner
            .authorize(
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await,
        Err(identity_unavailable())
    );
    // The moved directory matches the stored physical identity under another path: the closed
    // step names that alias (E013 item 7).
    assert!(matches!(
        owner
            .resolve_worktree(moved.clone(), moved.clone(), moved.join(".git"))
            .await,
        Err(DurableError::IdentityUnavailable {
            step: "identity_alias",
            holder: None,
        })
    ));
    fs::create_dir_all(fixture.root.join(".git")).unwrap();
    // A recreated directory at the same path cannot replace an identity an active start still
    // holds, and the refusal names that holder instead of one collapsed cause (E013 items 1/7).
    assert!(matches!(
        owner
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                fixture.root.join(".git")
            )
            .await,
        Err(DurableError::IdentityUnavailable {
            step: "identity_held",
            holder: Some(_),
        })
    ));
    owner
        .revoke(
            OperationId::new("retire-replaced-grant").unwrap(),
            &receipt,
            StopBindingHandoff::Confirmed,
        )
        .await
        .unwrap();
    // The reader's start alone still holds the replaced identity, so a replacement stays
    // refused until that reader stops too.
    assert!(matches!(
        owner
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                fixture.root.join(".git")
            )
            .await,
        Err(DurableError::IdentityUnavailable {
            step: "identity_held",
            holder: Some(_),
        })
    ));
    owner
        .revoke(
            OperationId::new("retire-reader-grant").unwrap(),
            &reader,
            StopBindingHandoff::Confirmed,
        )
        .await
        .unwrap();
    let recreated = fixture.resolve(&owner).await;
    assert!(recreated.incarnation() > tree.incarnation());
    assert_ne!(recreated.id(), tree.id());
}

/// Stores only bounded partial captures; caller coverage claims and foreign capture scopes cannot pass.
#[tokio::test]
async fn baseline_complete_cannot_be_forged_and_partial_capture_is_durable() {
    assert_eq!(
        BaselineContext::new("forged", BaselineCoverage::Complete),
        Err(GitError::UnverifiedBaseline)
    );
    let descriptive = BaselineContext::new("context", BaselineCoverage::Partial).unwrap();
    assert_eq!(descriptive.window(), BaselineWindow::NotCaptured);
    assert!(descriptive.capture_digest().is_none());
    let fixture = Fixture::new();
    let store = fixture.store();
    agent_ide::workspace::store::WorkspaceStore::new(&store)
        .install_schema()
        .await
        .unwrap();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let (mut guard, invocation, stamp, _) =
        activate(&owner, &tree, "baseline-start", "actor").await;
    fs::write(fixture.root.join("source"), [0, 255, 10]).unwrap();
    let scope = GitScope::from_authority(&stamp, DiffMode::Head);
    let git = RawGitEvidence::new(
        "head",
        scope.clone(),
        GitReadQuery::HeadIdentity,
        b"head-oid\n".to_vec(),
        vec![],
        Some(0),
        false,
        false,
    )
    .unwrap();
    let baseline = owner
        .capture_baseline(
            OperationId::new("baseline").unwrap(),
            &stamp,
            &guard.consume_active(invocation.binding_ref()).unwrap(),
            vec![git.clone()],
            vec!["source".into(), "missing".into()],
        )
        .await
        .unwrap();
    assert_eq!(baseline.coverage(), BaselineCoverage::Partial);
    assert_eq!(baseline.window(), BaselineWindow::Unverified);
    assert!(baseline.capture_digest().is_some());
    assert!(baseline.matches_scope(&scope));
    let comparison = agent_ide::workspace::git::GitComparison::new(
        scope.clone(),
        agent_ide::workspace::git::GitIdentity::new(b"left".to_vec()).unwrap(),
        agent_ide::workspace::git::GitIdentity::new(b"right".to_vec()).unwrap(),
        baseline.clone(),
    );
    assert_eq!(comparison.baseline().window(), BaselineWindow::Unverified);
    assert_eq!(
        owner
            .capture_baseline(
                OperationId::new("too-many-paths").unwrap(),
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap(),
                vec![git.clone()],
                vec![
                    PathBuf::from("source");
                    agent_ide::workspace::durable::MAX_BASELINE_PATHS + 1
                ]
            )
            .await,
        Err(DurableError::InvalidCapture)
    );
    fs::write(fixture.root.join("source"), b"changed since capture").unwrap();
    assert_eq!(
        owner
            .capture_baseline(
                OperationId::new("baseline").unwrap(),
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap(),
                vec![git.clone()],
                vec!["source".into(), "missing".into()]
            )
            .await
            .unwrap(),
        baseline
    );
    let length = store
        .read_one(
            "SELECT length(payload) FROM workspace_baselines WHERE operation='baseline'",
            vec![],
            |row| row.get::<_, i64>(0),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(length > 0 && length <= agent_ide::workspace::durable::MAX_BASELINE_BYTES as i64);
    let other = fixture.base.join("other");
    fs::create_dir_all(other.join(".git")).unwrap();
    let other_tree = owner
        .resolve_worktree(other.clone(), other.clone(), other.join(".git"))
        .await
        .unwrap();
    let (mut other_guard, other_invocation, other_stamp, _) =
        activate(&owner, &other_tree, "other-start", "other-actor").await;
    let other_scope = GitScope::from_authority(&other_stamp, DiffMode::Head);
    assert!(!baseline.matches_scope(&other_scope));
    assert_eq!(
        owner
            .capture_baseline(
                OperationId::new("foreign").unwrap(),
                &other_stamp,
                &other_guard
                    .consume_active(other_invocation.binding_ref())
                    .unwrap(),
                vec![git],
                vec![]
            )
            .await,
        Err(DurableError::InvalidCapture)
    );
}

/// Reconciliation/currentness lookups cannot mutate domain rows or consume finite operation receipts.
#[tokio::test]
async fn read_one_is_readonly_and_receipt_free() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let _owner = DurableWorkspace::open(&store).await.unwrap();
    let before = store
        .read_one(
            "SELECT count(*) FROM application_operation_receipts",
            vec![],
            |row| row.get::<_, i64>(0),
        )
        .await
        .unwrap();
    for _ in 0..10 {
        assert_eq!(
            store
                .read_one("SELECT 1", vec![], |row| row.get::<_, i64>(0))
                .await
                .unwrap(),
            Some(1)
        );
    }
    assert!(
        store
            .read_one("DELETE FROM workspace_authority_clock", vec![], |row| row
                .get::<_, i64>(
                0
            ))
            .await
            .is_err()
    );
    assert_eq!(
        store
            .read_one(
                "SELECT count(*) FROM workspace_authority_clock",
                vec![],
                |row| row.get::<_, i64>(0)
            )
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        store
            .read_one(
                "SELECT count(*) FROM application_operation_receipts",
                vec![],
                |row| row.get::<_, i64>(0)
            )
            .await
            .unwrap(),
        before
    );
}

/// Leaves a committed grant behind an OS process exit that bypasses Rust/SQLite destructor cleanup.
#[test]
fn abrupt_exit_writer() {
    let Ok(base) = std::env::var("AGENT_IDE_DURABLE_CRASH_FIXTURE") else {
        return;
    };
    let fixture = Fixture {
        root: PathBuf::from(&base).join("worktree"),
        base: PathBuf::from(base),
    };
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let store = fixture.store();
        let owner = DurableWorkspace::open(&store).await.unwrap();
        let tree = fixture.resolve(&owner).await;
        let (mut guard, invocation, stamp, _) =
            activate(&owner, &tree, "abrupt-start", "abrupt-actor").await;
        let git = RawGitEvidence::new(
            "abrupt-head",
            GitScope::from_authority(&stamp, DiffMode::Head),
            GitReadQuery::HeadIdentity,
            b"oid\n".to_vec(),
            vec![],
            Some(0),
            false,
            false,
        )
        .unwrap();
        owner
            .capture_baseline(
                OperationId::new("abrupt-baseline").unwrap(),
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap(),
                vec![git],
                vec![],
            )
            .await
            .unwrap();

        std::process::exit(0);
    });
}

/// Reopens the WAL after abrupt process exit and refuses the prior binding even on a new operation ID.
#[tokio::test]
async fn process_exit_keeps_receipts_without_reviving_stale_binding() {
    let fixture = Fixture::new();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "abrupt_exit_writer", "--nocapture"])
        .env("AGENT_IDE_DURABLE_CRASH_FIXTURE", &fixture.base)
        .output()
        .unwrap();
    assert!(child.status.success(), "abrupt fixture failed: {child:?}");
    let moved = fixture.base.join("missing-after-crash");
    fs::rename(&fixture.root, &moved).unwrap();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let baseline = owner
        .committed_baseline(&OperationId::new("abrupt-baseline").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(baseline.window(), BaselineWindow::Unverified);
    assert!(baseline.capture_digest().is_some());
    fs::rename(&moved, &fixture.root).unwrap();

    let tree = fixture.resolve(&owner).await;
    let (mut old, invocation) = binding("abrupt-actor", "abrupt-reconnect-call", "abrupt-start");
    let receipt = owner
        .activate(request("abrupt-start", &mut old, &invocation, &tree))
        .await
        .unwrap();
    assert_eq!(
        owner
            .authority(
                &receipt,
                &old.consume_active(invocation.binding_ref()).unwrap()
            )
            .await,
        Err(DurableError::Authority(AuthorityError::StaleAuthority))
    );
    assert_eq!(
        owner
            .activate(request(
                "abrupt-replay-new-id",
                &mut old,
                &invocation,
                &tree
            ))
            .await,
        Err(DurableError::Authority(AuthorityError::BindingNotCurrent))
    );
    let (_, _, new, _) = activate(&owner, &tree, "after-abrupt", "abrupt-actor").await;
    assert!(new.epoch() > receipt.epoch());
}

/// Explicit closure requires an inactive exact lifecycle and permits reopening the same native directory.
#[tokio::test]
async fn explicit_closure_reopens_same_directory_with_a_fresh_incarnation() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let (mut guard, invocation, stamp, start) =
        activate(&owner, &tree, "close-start", "close-actor").await;
    assert_eq!(
        owner
            .close_worktree(OperationId::new("close-active").unwrap(), &tree)
            .await,
        Err(DurableError::Authority(AuthorityError::WorktreeOwned))
    );
    owner
        .revoke(
            OperationId::new("stop-before-close").unwrap(),
            &start,
            StopBindingHandoff::Confirmed,
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.resolve(&owner).await,
        tree,
        "actor stop does not close a worktree lifecycle"
    );
    let id = OperationId::new("explicit-close").unwrap();
    let closed = owner.close_worktree(id.clone(), &tree).await.unwrap();
    assert_eq!(closed.incarnation(), tree.incarnation());
    assert_eq!(closed.operation(), &id);
    assert_eq!(
        owner.close_worktree(id.clone(), &tree).await.unwrap(),
        closed
    );
    let reopened = fixture.resolve(&owner).await;
    assert_ne!(tree.id(), reopened.id());
    assert!(reopened.incarnation() > tree.incarnation());
    assert_eq!(
        owner.close_worktree(id, &reopened).await,
        Err(DurableError::OperationConflict)
    );
    assert!(
        owner
            .authorize(
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await
            .is_err()
    );
}

/// Missing native paths mint nothing, while verified replacement of inactive root/common objects retires old identities.
#[tokio::test]
async fn inactive_recreation_and_common_directory_replacement_change_identity() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    fs::rename(&fixture.root, fixture.base.join("old-root")).unwrap();
    assert_eq!(
        owner
            .resolve_worktree(
                fixture.root.clone(),
                fixture.root.clone(),
                fixture.root.join(".git")
            )
            .await,
        Err(identity_unavailable())
    );
    fs::create_dir_all(fixture.root.join(".git")).unwrap();
    let recreated = fixture.resolve(&owner).await;
    assert_ne!(tree.id(), recreated.id());
    fs::rename(fixture.root.join(".git"), fixture.root.join(".git-old")).unwrap();
    fs::create_dir(fixture.root.join(".git")).unwrap();
    let changed_common = fixture.resolve(&owner).await;
    assert_ne!(recreated.id(), changed_common.id());
}

/// Stable operation lookup recovers committed bytes after restart even when no live native path or authority remains.
#[tokio::test]
async fn baseline_lookup_after_restart_never_recaptures_or_revives_authority() {
    let fixture = Fixture::new();
    let id = OperationId::new("restart-baseline").unwrap();
    let (baseline, start, stamp, mut guard, invocation) = {
        let store = fixture.store();
        let owner = DurableWorkspace::open(&store).await.unwrap();
        let tree = fixture.resolve(&owner).await;
        let (mut guard, invocation, stamp, start) =
            activate(&owner, &tree, "restart-capture", "actor").await;
        fs::write(fixture.root.join("source"), b"original capture").unwrap();
        let git = RawGitEvidence::new(
            "restart-head",
            GitScope::from_authority(&stamp, DiffMode::Head),
            GitReadQuery::HeadIdentity,
            b"oid\n".to_vec(),
            vec![],
            Some(0),
            false,
            false,
        )
        .unwrap();
        let baseline = owner
            .capture_baseline(
                id.clone(),
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap(),
                vec![git],
                vec!["source".into()],
            )
            .await
            .unwrap();
        (baseline, start, stamp, guard, invocation)
    };
    let store = fixture.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let recovered = owner
        .activate(request("restart-capture", &mut guard, &invocation, &tree))
        .await
        .unwrap();
    assert_eq!(recovered, start);
    fs::rename(&fixture.root, fixture.base.join("temporarily-missing")).unwrap();
    assert_eq!(owner.committed_baseline(&id).await.unwrap(), Some(baseline));
    assert_eq!(
        owner
            .committed_baseline(&OperationId::new("missing-baseline").unwrap())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        owner
            .authorize(
                &stamp,
                &guard.consume_active(invocation.binding_ref()).unwrap()
            )
            .await,
        Err(DurableError::Authority(AuthorityError::StaleAuthority))
    );
    fs::create_dir_all(fixture.root.join(".git")).unwrap();
    let other_tree = fixture.resolve(&owner).await;
    let (_, _, other_stamp, _) = activate(&owner, &other_tree, "replacement-start", "other").await;
    let restored = owner.committed_baseline(&id).await.unwrap().unwrap();
    assert!(!restored.matches_scope(&GitScope::from_authority(&other_stamp, DiffMode::Head)));
}

/// Identical paths and integer incarnations from another database cannot alias canonical authority.
#[tokio::test]
async fn independent_databases_have_distinct_nonces_and_reject_foreign_references() {
    let fixture = Fixture::new();
    let other = Fixture::new();
    let store = fixture.store();
    let other_store = other.store();
    let owner = DurableWorkspace::open(&store).await.unwrap();
    let other_owner = DurableWorkspace::open(&other_store).await.unwrap();
    let tree = fixture.resolve(&owner).await;
    let foreign = fixture.resolve(&other_owner).await;
    assert_eq!(tree.incarnation(), foreign.incarnation());
    assert_ne!(tree.id(), foreign.id());
    let (mut guard, invocation) = binding("foreign", "foreign-call", "foreign-channel");
    assert_eq!(
        owner
            .activate(request("foreign-start", &mut guard, &invocation, &foreign))
            .await,
        Err(identity_unavailable())
    );
}
