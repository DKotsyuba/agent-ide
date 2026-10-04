//! Contract checks for execution admission, leases, and owned process lifecycle.

use agent_ide::workspace::authority::{
    ActivationRequest, AuthorityRegistry, StopBindingHandoff, WorktreeRef,
};
use agent_ide::{assistance, execution};
use assistance::host_binding::{
    BindingStatus, ChannelSessionRef, HostBindingGuard, parse_candidate, parse_channel_session,
    parse_hook_event,
};
use execution::{
    Admission, AdmissionClass, AdmissionController, AdmissionLimits, BorrowedEndpoint, CommandKind,
    ControlledCommand, EndpointOwnership, LocalExecutionPolicy, OwnerId, ProviderBackendKind,
    ProviderLeaseAdmission, ProviderLeaseLimits, ProviderLeaseRegistry, ValidatedExecutionRequest,
    ValidatedHostInvocation, WorkspaceAuthority,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::io::AsyncReadExt;

/// Allocates a unique disposable worktree for process lifecycle tests.
fn worktree() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "agent-ide-execution-contract-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&root).unwrap();
    root
}

/// Supplies finite provider-view ceilings for lease contract scenarios.
fn lease_limits(total_views: usize, per_backend_views: usize) -> ProviderLeaseLimits {
    ProviderLeaseLimits {
        total_views,
        per_backend_views,
    }
}

/// Builds a direct job request for a fixed native shell script.
fn request(root: &Path, script: &str) -> ValidatedExecutionRequest {
    request_kind(root, script, CommandKind::Job)
}

/// Builds a direct or provider request for a fixed native shell script.
fn request_kind(root: &Path, script: &str, kind: CommandKind) -> ValidatedExecutionRequest {
    let invocation = ValidatedHostInvocation::from_verified_binding("bound-test").unwrap();
    let authority =
        WorkspaceAuthority::from_workspace("test-worktree", "1", root.to_path_buf(), 7).unwrap();
    let command = ControlledCommand::from_validated_peer(
        kind,
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from(script)],
        root.to_path_buf(),
        BTreeMap::new(),
    )
    .unwrap();
    let policy =
        LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/bin/sh")]), 4096, 4).unwrap();
    ValidatedExecutionRequest::validate(invocation, authority, command, &policy).unwrap()
}

/// Creates a trusted host candidate with the metadata fields Assistance validates.
fn candidate(actor: &str, call: &str) -> assistance::host_binding::CandidateInvocation {
    parse_candidate(
        json!({"threadId":actor,"callId":call,"x-codex-turn-metadata":{"turn":"bounded"}})
            .as_object()
            .unwrap(),
    )
    .unwrap()
}

/// Creates the matching native pre-hook event required to establish a binding generation.
fn pre_hook(actor: &str, call: &str) -> assistance::host_binding::HookEvent {
    parse_hook_event(
        json!({"hook_event_name":"PreToolUse","session_id":actor,"tool_use_id":call})
            .to_string()
            .as_bytes(),
    )
    .unwrap()
}

/// Creates a bounded trusted channel identifier for host-binding tests.
fn channel() -> ChannelSessionRef {
    parse_channel_session(b"execution-contract-channel").unwrap()
}

/// Validates the fixed Git discovery query and rejects a relative candidate before spawn.
#[test]
fn discovery_validates_fixed_query_and_candidate_path() {
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("discover", "query"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("discover", "query"), channel)
    else {
        panic!("expected validated binding");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let policy = execution::GitDiscoveryPolicy::new(PathBuf::from("/usr/bin/git"), 1024).unwrap();
    let query = execution::DiscoverWorktreeRequest::from_active_use(
        active,
        OsString::from("/private/tmp/candidate"),
        execution::DiscoveryOperationRef::new("discover-test").unwrap(),
    )
    .unwrap();
    assert!(
        query
            .validate_query(execution::GitDiscoveryQuery::ShowTopLevel, &policy)
            .is_ok()
    );

    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let relative = execution::DiscoverWorktreeRequest::from_active_use(
        active,
        OsString::from("relative"),
        execution::DiscoveryOperationRef::new("discover-relative").unwrap(),
    )
    .unwrap();
    assert!(matches!(
        relative.validate_query(execution::GitDiscoveryQuery::ShowTopLevel, &policy),
        Err(execution::RequestError::InvalidCommandPath)
    ));
}

/// Requires fresh Assistance liveness at spawn when validation retained an active binding.
#[tokio::test]
async fn active_request_without_fresh_spawn_use_never_starts() {
    let root = worktree();
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("spawn", "missing-use"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("spawn", "missing-use"), channel)
    else {
        panic!("expected validated binding");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Job,
        PathBuf::from("/bin/sh"),
        vec![OsString::from("-c"), OsString::from("true")],
        root.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    let authority = WorkspaceAuthority::from_workspace("active-use", "1", root.clone(), 1).unwrap();
    let policy =
        LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/bin/sh")]), 4096, 4).unwrap();
    let request = ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_active_use(active),
        authority,
        command,
        &policy,
    )
    .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let Admission::Granted(lease) = admission.submit(
        OwnerId::new("active-use").unwrap(),
        AdmissionClass::Interactive,
    ) else {
        panic!("fixture admission must succeed");
    };
    let execution::ProcessError::NeverStarted { cause, settlement } =
        execution::OwnedChild::spawn_captured(&request, lease, None, 64)
            .err()
            .unwrap()
    else {
        panic!("missing active use must fail before child spawn");
    };
    assert!(matches!(
        *cause,
        execution::ProcessError::Request(execution::RequestError::MissingActiveBindingUse)
    ));
    admission.settle_never_started(settlement).unwrap();
    fs::remove_dir_all(root).unwrap();
}

/// Enforces the execution layer's local program, cwd, argv, and environment ceilings.
#[test]
fn request_validation_enforces_local_ceilings() {
    let root = worktree();
    let invocation = || ValidatedHostInvocation::from_verified_binding("local-ceilings").unwrap();
    let authority =
        || WorkspaceAuthority::from_workspace("ceiling-test", "1", root.clone(), 1).unwrap();
    let command = |cwd: PathBuf, args: Vec<OsString>, env: BTreeMap<OsString, OsString>| {
        ControlledCommand::from_validated_peer(
            CommandKind::Job,
            PathBuf::from("/bin/sh"),
            args,
            cwd,
            env,
        )
        .unwrap()
    };
    let policy = |programs, argv, env| LocalExecutionPolicy::new(programs, argv, env).unwrap();

    assert!(matches!(
        ValidatedExecutionRequest::validate(
            invocation(),
            authority(),
            command(root.clone(), Vec::new(), BTreeMap::new()),
            &policy(BTreeSet::from([PathBuf::from("/bin/ls")]), 16, 1)
        ),
        Err(execution::RequestError::ProgramDenied)
    ));
    assert!(matches!(
        ValidatedExecutionRequest::validate(
            invocation(),
            authority(),
            command(root.join("other"), Vec::new(), BTreeMap::new()),
            &policy(BTreeSet::from([PathBuf::from("/bin/sh")]), 16, 1)
        ),
        Err(execution::RequestError::WorktreeDenied)
    ));
    assert!(matches!(
        ValidatedExecutionRequest::validate(
            invocation(),
            authority(),
            command(
                root.clone(),
                vec![OsString::from("long-argument")],
                BTreeMap::new()
            ),
            &policy(BTreeSet::from([PathBuf::from("/bin/sh")]), 2, 1)
        ),
        Err(execution::RequestError::ArgvTooLarge)
    ));
    assert!(matches!(
        ValidatedExecutionRequest::validate(
            invocation(),
            authority(),
            command(
                root.clone(),
                Vec::new(),
                BTreeMap::from([
                    (OsString::from("A"), OsString::from("1")),
                    (OsString::from("B"), OsString::from("2"))
                ])
            ),
            &policy(BTreeSet::from([PathBuf::from("/bin/sh")]), 16, 1)
        ),
        Err(execution::RequestError::EnvironmentTooLarge)
    ));
    fs::remove_dir_all(root).unwrap();
}

/// Rejects a Git request whose worktree `.git` backpointer no longer matches Workspace authority.
#[test]
fn git_request_rejects_changed_metadata_backpointer() {
    let root = worktree();
    let common = root.join("common");
    fs::create_dir(&common).unwrap();
    fs::write(root.join(".git"), "gitdir: /missing/admin\n").unwrap();
    let authority = WorkspaceAuthority::from_workspace_with_git_common_dir(
        "git-metadata",
        "1",
        root.clone(),
        common,
        1,
    )
    .unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Git,
        PathBuf::from("/usr/bin/git"),
        Vec::new(),
        root.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    let policy =
        LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/usr/bin/git")]), 16, 1).unwrap();
    assert!(matches!(
        ValidatedExecutionRequest::validate(
            ValidatedHostInvocation::from_verified_binding("git-metadata").unwrap(),
            authority,
            command,
            &policy,
        ),
        Err(execution::RequestError::GitMetadataInvalid)
    ));
    fs::remove_dir_all(root).unwrap();
}

/// Proves bounded class preference and owner rotation without granting queued work a hidden slot.
#[test]
fn admission_bounds_and_fairness_are_centralized() {
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let a = OwnerId::new("a").unwrap();
    let b = OwnerId::new("b").unwrap();
    let first = match admission.submit(a.clone(), AdmissionClass::Interactive) {
        Admission::Granted(lease) => lease,
        outcome => panic!("unexpected initial admission: {outcome:?}"),
    };
    let a_ticket = match admission.submit(a, AdmissionClass::Interactive) {
        Admission::Queued(ticket) => ticket,
        outcome => panic!("unexpected queued outcome: {outcome:?}"),
    };
    assert!(admission.contains_ticket(a_ticket));
    let b_ticket = match admission.submit(b, AdmissionClass::Background) {
        Admission::Queued(ticket) => ticket,
        outcome => panic!("unexpected queued outcome: {outcome:?}"),
    };
    let promoted = admission.release(first).unwrap();
    assert_eq!(promoted.len(), 1);
    assert!(admission.contains_ticket(a_ticket));
    assert!(!admission.contains_ticket(b_ticket));
    assert_eq!(admission.running_count(), 1);
}
/// Proves compatible views share one heavy reservation while exclusive and queued requests do not.
#[test]
fn provider_leases_share_only_compatible_owned_backends() {
    let authority =
        WorkspaceAuthority::from_workspace("worktree", "1", PathBuf::from("/private/tmp"), 7)
            .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 2,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(lease_limits(2, 2)).unwrap();
    let first = match registry.request(
        &mut admission,
        OwnerId::new("owner").unwrap(),
        AdmissionClass::Interactive,
        "shared",
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        result => panic!("unexpected first view: {result:?}"),
    };
    let second = match registry.request(
        &mut admission,
        OwnerId::new("other").unwrap(),
        AdmissionClass::Interactive,
        "shared",
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        result => panic!("unexpected shared view: {result:?}"),
    };
    let no_child = registry.take_spawn_lease(first).unwrap().cancel();
    assert!(matches!(
        registry.take_spawn_lease(first),
        Err(execution::ProviderLeaseError::SpawnUnavailable)
    ));
    assert!(matches!(
        registry.take_spawn_lease(second),
        Err(execution::ProviderLeaseError::SpawnUnavailable)
    ));
    assert_eq!(registry.counts(), (1, 2));
    assert!(matches!(
        registry.release(first).unwrap(),
        execution::BackendRelease::SharedPeerSurvives
    ));
    assert!(matches!(
        registry.release(second).unwrap(),
        execution::BackendRelease::ReapOwned(_)
    ));
    assert_eq!(registry.counts(), (1, 0));
    registry
        .settle_never_started(&mut admission, no_child)
        .unwrap();
    assert_eq!(registry.counts(), (0, 0));
    let exclusive = match registry.request(
        &mut admission,
        OwnerId::new("owner").unwrap(),
        AdmissionClass::Interactive,
        "exclusive",
        ProviderBackendKind::OwnedExclusive,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        result => panic!("unexpected exclusive view: {result:?}"),
    };
    let ticket = match registry.request(
        &mut admission,
        OwnerId::new("other").unwrap(),
        AdmissionClass::Background,
        "queued",
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Queued(ticket) => ticket,
        result => panic!("unexpected queued view: {result:?}"),
    };
    let promotions = registry
        .cancel_unstarted(&mut admission, exclusive)
        .unwrap();
    let promotion = promotions.into_iter().next().unwrap();
    assert_eq!(promotion.ticket(), ticket);
    assert!(
        registry
            .promote(&mut admission, promotion, &authority)
            .is_ok()
    );
    assert_eq!(registry.counts(), (1, 1));
}

/// Proves forwarder ceilings reject before mutation while the earlier shared view remains usable.
#[test]
fn provider_view_limits_bound_shared_forwarders() {
    let authority =
        WorkspaceAuthority::from_workspace("worktree", "1", PathBuf::from("/private/tmp"), 7)
            .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 2,
        per_owner_running: 2,
        per_owner_queued: 2,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let mut registry = ProviderLeaseRegistry::new(lease_limits(1, 1)).unwrap();
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("owner").unwrap(),
            AdmissionClass::Interactive,
            "shared",
            ProviderBackendKind::OwnedShared,
            &authority
        ),
        ProviderLeaseAdmission::Granted(_)
    ));
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("peer").unwrap(),
            AdmissionClass::Interactive,
            "shared",
            ProviderBackendKind::OwnedShared,
            &authority
        ),
        ProviderLeaseAdmission::Rejected(execution::ProviderLeaseError::ViewCapacity)
    ));
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("borrowed").unwrap(),
            AdmissionClass::Interactive,
            "other",
            ProviderBackendKind::Borrowed,
            &authority
        ),
        ProviderLeaseAdmission::Rejected(execution::ProviderLeaseError::ViewCapacity)
    ));
    assert_eq!(registry.counts(), (1, 1));
}

/// Proves a queued provider request has no registry reservation until its exact promotion is consumed.
#[test]
fn queued_provider_ticket_promotes_once_with_its_matching_lease() {
    let authority =
        WorkspaceAuthority::from_workspace("worktree", "1", PathBuf::from("/private/tmp"), 7)
            .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 2,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let held = match admission.submit(OwnerId::new("holder").unwrap(), AdmissionClass::Interactive)
    {
        Admission::Granted(lease) => lease,
        result => panic!("unexpected holder: {result:?}"),
    };
    let mut registry = ProviderLeaseRegistry::new(lease_limits(2, 1)).unwrap();
    let ticket = match registry.request(
        &mut admission,
        OwnerId::new("queued").unwrap(),
        AdmissionClass::Background,
        "queued-backend",
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Queued(ticket) => ticket,
        result => panic!("unexpected queue: {result:?}"),
    };
    assert_eq!(registry.counts(), (0, 0));
    let promotion = admission
        .release_with_promotions(held)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(promotion.ticket(), ticket);
    let view = registry
        .promote(&mut admission, promotion, &authority)
        .unwrap();
    assert_eq!(registry.counts(), (1, 1));
    assert!(registry.take_spawn_lease(view).is_ok());
    assert!(matches!(
        registry.take_spawn_lease(view),
        Err(execution::ProviderLeaseError::SpawnUnavailable)
    ));
    // The consumed promotion cannot be reused; the compile-fail contract covers that ownership boundary.
    assert!(matches!(
        registry.release(view),
        Ok(execution::BackendRelease::ReapOwned(_))
    ));
}

/// Proves one Workspace revocation drains only its views and leaves a shared peer backend alive.
#[test]
fn authority_revocation_returns_logical_drain_not_reap_claim() {
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("expected validated binding");
    };
    let worktree = WorktreeRef::from_discovery(
        PathBuf::from("/private/tmp"),
        PathBuf::from("/private/tmp"),
        PathBuf::from(".git"),
        1,
    )
    .unwrap();
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let mut workspace = AuthorityRegistry::default();
    let stamp = workspace
        .activate(ActivationRequest::new("activate", false, invocation, active, worktree).unwrap())
        .unwrap();
    let authority = WorkspaceAuthority::from_workspace(
        stamp.worktree().id(),
        stamp.worktree().incarnation().to_string(),
        stamp.worktree().worktree_path().to_path_buf(),
        stamp.epoch(),
    )
    .unwrap();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 2,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let mut leases = ProviderLeaseRegistry::new(lease_limits(2, 2)).unwrap();
    let first = match leases.request(
        &mut admission,
        OwnerId::new("owner").unwrap(),
        AdmissionClass::Interactive,
        "backend",
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        result => panic!("unexpected first lease: {result:?}"),
    };
    let peer_authority = WorkspaceAuthority::from_workspace(
        stamp.worktree().id(),
        stamp.worktree().incarnation().to_string(),
        stamp.worktree().worktree_path().to_path_buf(),
        stamp.epoch() + 1,
    )
    .unwrap();
    assert!(matches!(
        leases.request(
            &mut admission,
            OwnerId::new("peer").unwrap(),
            AdmissionClass::Interactive,
            "backend",
            ProviderBackendKind::OwnedShared,
            &peer_authority,
        ),
        ProviderLeaseAdmission::Granted(_)
    ));
    guard.stop_binding(stamp.binding()).unwrap();
    let revoked = workspace
        .revoke(&stamp, StopBindingHandoff::Confirmed)
        .unwrap();
    let receipt = leases.revoke_authority(&revoked);
    assert_eq!(receipt.drained_views, 1);
    assert_eq!(
        receipt.backend_releases,
        vec![execution::BackendRelease::SharedPeerSurvives]
    );
    assert!(!receipt.reap_uncertain);
    assert!(matches!(
        leases.release(first),
        Err(execution::ProviderLeaseError::UnknownView)
    ));
    assert_eq!(leases.counts(), (1, 1));
}

/// Verifies that bounded retained output still drains both streams to EOF.
#[tokio::test]
async fn captured_streams_are_bounded_but_drained() {
    let root = worktree();
    let request = request(&root, "printf 'abcdef'; printf '123456' >&2");
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let lease = match admission.submit(OwnerId::new("owner").unwrap(), AdmissionClass::Interactive)
    {
        Admission::Granted(lease) => lease,
        outcome => panic!("unexpected admission: {outcome:?}"),
    };
    let result = execution::OwnedChild::spawn_captured(&request, lease, None, 3)
        .unwrap()
        .reap(Duration::from_secs(1), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(result.evidence.status().success());
    assert_eq!(result.evidence.stdout().bytes, b"abc");
    assert_eq!(result.evidence.stderr().bytes, b"123");
    assert_eq!(result.evidence.stdout().drained_bytes, 6);
    assert_eq!(result.evidence.stderr().drained_bytes, 6);
    assert!(result.evidence.stdout().truncated && result.evidence.stderr().truncated);
    assert!(result.evidence.stdout().complete && result.evidence.stderr().complete);
    assert!(
        admission
            .release_reaped(result.settlement)
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

/// Separates cancellation request acknowledgement from direct-child reaping evidence.
#[tokio::test]
async fn cancellation_reports_reap_without_claiming_descendants() {
    let root = worktree();
    let request = request(&root, "sleep 5");
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let lease = match admission.submit(OwnerId::new("owner").unwrap(), AdmissionClass::Interactive)
    {
        Admission::Granted(lease) => lease,
        outcome => panic!("unexpected admission: {outcome:?}"),
    };
    let result = execution::OwnedChild::spawn_captured(&request, lease, None, 64)
        .unwrap()
        .cancel_and_reap(Duration::from_millis(100), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(result.evidence.cancellation().unwrap().term_requested);
    assert_eq!(
        result.evidence.descendants(),
        execution::DescendantEvidence::Unverified
    );
    assert!(
        admission
            .release_reaped(result.settlement)
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

/// Ensures protocol stdout stays with Intelligence rather than becoming a second Execution reader.
#[tokio::test]
async fn protocol_stdout_has_one_owner_and_borrowed_endpoints_cannot_be_killed() {
    let root = worktree();
    let request = request_kind(
        &root,
        "printf protocol; printf diagnostic >&2",
        CommandKind::Provider,
    );
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let authority = request.authority().clone();
    let mut registry = ProviderLeaseRegistry::new(lease_limits(1, 1)).unwrap();
    let view = match registry.request(
        &mut admission,
        OwnerId::new("owner").unwrap(),
        AdmissionClass::Interactive,
        "protocol-backend",
        ProviderBackendKind::OwnedExclusive,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        outcome => panic!("unexpected admission: {outcome:?}"),
    };
    let capability = registry.take_spawn_lease(view).unwrap();
    let mut child =
        execution::OwnedProtocolChild::spawn_from_provider_lease(&request, capability, None, 64)
            .unwrap();
    let mut protocol = String::new();
    child
        .stdout
        .as_mut()
        .unwrap()
        .read_to_string(&mut protocol)
        .await
        .unwrap();
    let reaped = child.reap(Duration::from_secs(1)).await.unwrap();
    let status = reaped.status;
    let stderr = &reaped.stderr;
    assert!(status.success());
    assert_eq!(protocol, "protocol");
    assert_eq!(stderr.bytes, b"diagnostic");
    let execution::BackendRelease::ReapOwned(capability) = registry.release(view).unwrap() else {
        panic!("owned reap capability")
    };

    assert_eq!(admission.running_count(), 1);
    registry
        .complete_reap(&mut admission, capability, reaped.proof)
        .unwrap();
    assert_eq!(
        BorrowedEndpoint::observe("peer:42").unwrap().cancel(),
        EndpointOwnership::Borrowed
    );
    fs::remove_dir_all(root).unwrap();
}

/// Kills only the test-created process group if an assertion fails before ownership cleanup succeeds.
struct ChildCleanup(libc::pid_t);

impl Drop for ChildCleanup {
    /// Best-effort cleanup of the still-owned fixture group; no borrowed or unrelated PID is accepted.
    fn drop(&mut self) {
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

/// Waits for the fixture's PID handshake before cancellation, so the regression cannot pass by racing spawn.
async fn child_cleanup(root: &Path) -> ChildCleanup {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(pid) = fs::read_to_string(root.join("child.pid"))
                .ok()
                .and_then(|pid| pid.parse::<libc::pid_t>().ok())
            {
                return ChildCleanup(pid);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

/// Proves dropped captured/protocol handles and cancelled reap futures kill their real direct child.
#[tokio::test]
async fn dropping_owned_children_and_reap_futures_kills_without_freeing_uncertain_slots() {
    let mut borrowed = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let endpoint = BorrowedEndpoint::observe(format!("peer:{}", borrowed.id().unwrap())).unwrap();
    assert_eq!(endpoint.cancel(), EndpointOwnership::Borrowed);
    drop(endpoint);
    for mode in 0..5 {
        let root = worktree();
        let request = request(
            &root,
            "trap '' TERM; printf '%s' \"$$\" > child.pid; exec /bin/sleep 30",
        );
        let mut admission = AdmissionController::new(AdmissionLimits {
            total_running: 1,
            per_owner_running: 1,
            per_owner_queued: 1,
            total_queued: 1,
            interactive_burst: 1,
        })
        .unwrap();
        let Admission::Granted(lease) =
            admission.submit(OwnerId::new("owner").unwrap(), AdmissionClass::Interactive)
        else {
            panic!("fixture admission must succeed")
        };
        let cleanup;
        let mut group_member = None;
        if mode < 3 {
            let child = execution::OwnedChild::spawn_captured(&request, lease, None, 64).unwrap();
            cleanup = child_cleanup(&root).await;
            if mode == 0 {
                // A test-owned direct child joins the group, so its exit can be reaped without
                // assuming waitpid rights over arbitrary grandchildren or relying on init.
                group_member = Some(
                    tokio::process::Command::new("/bin/sleep")
                        .arg("30")
                        .process_group(cleanup.0)
                        .kill_on_drop(true)
                        .spawn()
                        .unwrap(),
                );
            }
            match mode {
                0 => drop(child),
                1 => {
                    let mut future =
                        Box::pin(child.reap(Duration::from_secs(1), Duration::from_secs(1)));
                    assert!(
                        future
                            .as_mut()
                            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                            .is_pending()
                    );
                    drop(future);
                }
                _ => {
                    let mut future = Box::pin(
                        child.cancel_and_reap(Duration::from_secs(10), Duration::from_secs(1)),
                    );
                    assert!(
                        future
                            .as_mut()
                            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                            .is_pending()
                    );
                    drop(future);
                }
            }
        } else {
            let child = execution::OwnedProtocolChild::spawn(&request, lease, None, 64).unwrap();
            cleanup = child_cleanup(&root).await;
            if mode == 3 {
                drop(child);
            } else {
                let mut future = Box::pin(child.reap(Duration::from_secs(1)));
                assert!(
                    future
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                        .is_pending()
                );
                drop(future);
            }
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let result = unsafe { libc::kill(cleanup.0, 0) };
                if result == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("owned child survived dropped mode {mode}"));
        if let Some(mut member) = group_member {
            assert!(
                !tokio::time::timeout(Duration::from_secs(3), member.wait())
                    .await
                    .unwrap()
                    .unwrap()
                    .success(),
                "owned group member survived drop"
            );
        }
        assert!(
            borrowed.try_wait().unwrap().is_none(),
            "borrowed endpoint was killed"
        );
        assert_eq!(
            admission.running_count(),
            1,
            "drop must not manufacture reap evidence"
        );
        assert!(matches!(
            admission.submit(OwnerId::new("peer").unwrap(), AdmissionClass::Interactive),
            Admission::Queued(_)
        ));
        std::mem::forget(cleanup); // The reaped PID must never be signalled again after possible reuse.
        fs::remove_dir_all(root).unwrap();
    }
    borrowed.kill().await.unwrap();
    borrowed.wait().await.unwrap();
}
