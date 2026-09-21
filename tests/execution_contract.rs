//! Contract checks for the unassembled Execution module.

use agent_ide::workspace::authority::{
    ActivationRequest, AuthorityRegistry, StopBindingHandoff, WorktreeRef,
};
use agent_ide::{assistance, execution};

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use assistance::host_binding::{
    BindingStatus, ChannelSessionRef, HostBindingGuard, parse_candidate, parse_channel_session,
    parse_hook_event, parse_observed_sandbox_state,
};
use execution::ControlledTrampoline;
use execution::{
    Admission, AdmissionClass, AdmissionController, AdmissionLimits, BorrowedEndpoint, CommandKind,
    ControlledCommand, D03ProfileEvidence, DiscoverWorktreeRequest, DiscoveryOperationRef,
    EndpointOwnership, ExecutionProfileCatalog, ExecutionProfileTemplate, GitDiscoveryPolicy,
    GitDiscoveryQuery, HostSandboxState, LocalExecutionPolicy, OwnerId, PersistedProfileRecord,
    ProfileClass, ProviderBackendKind, ProviderLeaseAdmission, ProviderLeaseLimits,
    ProviderLeaseRegistry, RequestError, SandboxStateError, ValidatedExecutionRequest,
    ValidatedHostInvocation, WorkspaceAuthority,
};
use serde_json::json;
use tokio::io::AsyncReadExt;

/// Allocates unique, disposable execution worktrees without reusing existing user content.
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

/// Builds the complete disabled host state needed for controlled unit-only child launches.
fn disabled_state(root: &Path) -> HostSandboxState {
    HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": root,
        "useLegacyLandlock": false
    })))
    .unwrap()
}

/// Supplies finite view ceilings for provider-lease contract scenarios.
fn lease_limits(total_views: usize, per_backend_views: usize) -> ProviderLeaseLimits {
    ProviderLeaseLimits {
        total_views,
        per_backend_views,
    }
}

/// Builds a request whose argv/profile data is fixed by this test rather than model text.
fn request(root: &Path, script: &str) -> ValidatedExecutionRequest {
    request_kind(root, script, CommandKind::Job)
}

/// Builds the declared direct or provider command category for a fixed native test script.
fn request_kind(root: &Path, script: &str, kind: CommandKind) -> ValidatedExecutionRequest {
    let sandbox = disabled_state(root);
    let profiles = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("disabled-contract-case", 1, &sandbox)
            .unwrap(),
    ])
    .unwrap();
    let invocation = ValidatedHostInvocation::from_verified_binding("bound-test", sandbox).unwrap();
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
        LocalExecutionPolicy::new(BTreeSet::from([PathBuf::from("/bin/sh")]), 4096, 4, true)
            .unwrap();
    ValidatedExecutionRequest::validate(invocation, authority, command, &policy, &profiles).unwrap()
}

/// Creates a trusted host candidate with the metadata fields Assistance validates.
fn candidate(actor: &str, call: &str) -> assistance::host_binding::CandidateInvocation {
    parse_candidate(
        json!({
            "threadId": actor,
            "callId": call,
            "x-codex-turn-metadata": {"turn": "bounded"}
        })
        .as_object()
        .unwrap(),
    )
    .unwrap()
}

/// Creates the matching native pre-hook event required to establish a binding generation.
fn pre_hook(actor: &str, call: &str) -> assistance::host_binding::HookEvent {
    parse_hook_event(
        json!({
            "hook_event_name": "PreToolUse",
            "session_id": actor,
            "tool_use_id": call
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

/// Creates a bounded trusted channel identifier for the host-binding guard.
fn channel() -> ChannelSessionRef {
    parse_channel_session(b"execution-contract-channel").unwrap()
}

/// Checks that opaque profile classification never treats missing or external state as executable.
#[test]
fn opaque_state_rejects_missing_and_external_profiles() {
    assert_eq!(
        HostSandboxState::parse(None),
        Err(SandboxStateError::Missing)
    );
    assert_eq!(
        HostSandboxState::parse(Some(json!({
            "permissionProfile": {"type": "external"},
            "sandboxCwd": "/private/tmp"
        }))),
        Err(SandboxStateError::ExternalUnsupported)
    );
    assert_eq!(
        HostSandboxState::parse(Some(json!({
            "permissionProfile": {
                "type": "managed",
                "file_system": {"type": "read"},
                "network": false,
                "workspace_roots": ["/one", "/two"]
            },
            "sandboxCwd": "/one"
        }))),
        Err(SandboxStateError::UnsupportedProfile)
    );
    let managed = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "managed", "file_system": {"type": "read"}, "network": false},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": "/private/tmp",
        "useLegacyLandlock": false
    })))
    .unwrap();
    assert_eq!(managed.class(), ProfileClass::Managed);
    let uri = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "managed", "file_system": {"type": "read"}, "network": false},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": "file:///private/tmp",
        "useLegacyLandlock": false
    })))
    .unwrap();
    assert_eq!(uri.sandbox_cwd(), "file:///private/tmp");
    assert_eq!(uri.cwd(), Path::new("/private/tmp"));
    let raw_json = "{\n  \"permissionProfile\": {\"type\": \"disabled\"},\n  \"sandboxCwd\": \"/private/tmp\"\n}";
    assert_eq!(
        HostSandboxState::parse_json(raw_json)
            .unwrap()
            .sandbox_state_json(),
        raw_json
    );
}

/// Proves an Execution admission accepts only Assistance-consumed liveness paired with its state.
#[test]
fn active_observation_becomes_a_validated_execution_invocation() {
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("explicit start must establish the binding");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let observed = parse_observed_sandbox_state(
        json!({
            "codex/sandbox-state-meta": {
                "permissionProfile": {"type": "managed", "file_system": {}, "network": "restricted"},
                "codexLinuxSandboxExe": null,
                "sandboxCwd": "file:///private/tmp",
                "useLegacyLandlock": false,
                "unknown_nested_field": {"preserved": true}
            }
        })
        .as_object()
        .unwrap(),
        &invocation,
        &active,
        true,
    )
    .unwrap();
    let execution =
        execution::ValidatedHostInvocation::from_active_observation(active, observed).unwrap();
    assert_eq!(execution.sandbox().class(), ProfileClass::Managed);
    assert_eq!(execution.sandbox().cwd(), Path::new("/private/tmp"));
}

/// Proves a request originating from Assistance cannot launch after a queue delay without a fresh use.
#[tokio::test]
async fn observed_request_rejects_missing_fresh_use_at_spawn() {
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("explicit start must validate");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let observed = parse_observed_sandbox_state(
        json!({
            "codex/sandbox-state-meta": {
                "permissionProfile": {"type": "managed", "file_system": {}, "network": "restricted"},
                "codexLinuxSandboxExe": null,
                "sandboxCwd": "file:///private/tmp",
                "useLegacyLandlock": false
            }
        })
        .as_object()
        .unwrap(),
        &invocation,
        &active,
        true,
    )
    .unwrap();
    let state = HostSandboxState::parse(Some(observed.state().as_json().clone())).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("managed", 1, &state).unwrap(),
    ])
    .unwrap();
    let execution =
        execution::ValidatedHostInvocation::from_active_observation(active, observed).unwrap();
    let authority = WorkspaceAuthority::from_workspace(
        "worktree",
        "incarnation",
        PathBuf::from("/private/tmp"),
        1,
    )
    .unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Job,
        PathBuf::from("/usr/bin/true"),
        Vec::new(),
        PathBuf::from("/private/tmp"),
        BTreeMap::new(),
    )
    .unwrap();
    let policy = LocalExecutionPolicy::new(
        BTreeSet::from([PathBuf::from("/usr/bin/true")]),
        1,
        0,
        false,
    )
    .unwrap();
    let request =
        ValidatedExecutionRequest::validate(execution, authority, command, &policy, &catalog)
            .unwrap();
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
    assert!(matches!(
        execution::OwnedChild::spawn_captured(
            &request,
            lease,
            None,
            Path::new("/usr/bin/codex"),
            1
        ),
        Err(execution::ProcessError::NeverStarted {cause,..}) if matches!(*cause,execution::ProcessError::Request(execution::RequestError::MissingActiveBindingUse))
    ));
}

/// Proves pre-authority discovery is accepted only from a consumed observed binding and fixed Git policy.
#[test]
fn discovery_request_is_catalog_gated_before_authority_exists() {
    let mut guard = HostBindingGuard::default();
    let channel = channel();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("explicit start must validate");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let observed = parse_observed_sandbox_state(
        json!({
            "codex/sandbox-state-meta": {
                "permissionProfile": {"type": "managed", "file_system": {}, "network": "restricted"},
                "codexLinuxSandboxExe": null,
                "sandboxCwd": "file:///private/tmp",
                "useLegacyLandlock": false
            }
        })
        .as_object()
        .unwrap(),
        &invocation,
        &active,
        true,
    )
    .unwrap();
    let state = HostSandboxState::parse(Some(observed.state().as_json().clone())).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("managed", 1, &state).unwrap(),
    ])
    .unwrap();
    let request = DiscoverWorktreeRequest::from_active_observation(
        active,
        observed,
        OsString::from("/private/tmp/candidate\n"),
        DiscoveryOperationRef::new("discover-1").unwrap(),
    )
    .unwrap();
    let policy = GitDiscoveryPolicy::new(PathBuf::from("/usr/bin/git"), 1024, false).unwrap();
    assert!(
        request
            .validate_query(GitDiscoveryQuery::ShowTopLevel, &policy, &catalog)
            .is_ok()
    );
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

/// Proves corrupt durable profile records cannot restore a permit and value changes refuse replay.
#[test]
fn persisted_catalog_requires_complete_matching_d03_evidence() {
    let state = disabled_state(Path::new("/private/tmp"));
    let record = PersistedProfileRecord::from_execution_evidence(
        "disabled-d03",
        1,
        D03ProfileEvidence {
            provider_binary: "codex-sha".into(),
            toolchain: "toolchain-sha".into(),
            configuration: "config-sha".into(),
            trust: "trusted".into(),
            transport: "direct".into(),
            d03_evidence: "d03-run".into(),
        },
        &state,
    )
    .unwrap();
    let loaded = PersistedProfileRecord::from_json(&record.to_json()).unwrap();
    assert!(
        ExecutionProfileCatalog::from_persisted_records(
            vec![(loaded, state.clone())],
            std::slice::from_ref(&record),
        )
        .is_ok()
    );
    assert!(PersistedProfileRecord::from_json("{\"profile_id\":\"only\"}").is_err());
    let changed = HostSandboxState::parse(Some(json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": "/private/tmp",
        "useLegacyLandlock": false,
        "changed_semantic_value": true
    })))
    .unwrap();
    assert!(
        ExecutionProfileCatalog::from_persisted_records(
            vec![(record.clone(), changed)],
            std::slice::from_ref(&record)
        )
        .is_err()
    );
    let fabricated = PersistedProfileRecord::from_execution_evidence(
        "fabricated",
        1,
        D03ProfileEvidence {
            provider_binary: "fake".into(),
            toolchain: "fake".into(),
            configuration: "fake".into(),
            trust: "fake".into(),
            transport: "fake".into(),
            d03_evidence: "fake".into(),
        },
        &state,
    )
    .unwrap();
    assert!(
        ExecutionProfileCatalog::from_persisted_records(vec![(fabricated, state)], &[record])
            .is_err()
    );
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
        .activate(ActivationRequest::new("activate", invocation, active, worktree).unwrap())
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
    let result = execution::OwnedChild::spawn_captured(
        &request,
        lease,
        None,
        Path::new("/usr/bin/codex"),
        3,
    )
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
    let result = execution::OwnedChild::spawn_captured(
        &request,
        lease,
        None,
        Path::new("/usr/bin/codex"),
        64,
    )
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
    let mut child = execution::OwnedProtocolChild::spawn_from_provider_lease(
        &request,
        capability,
        None,
        Path::new("/usr/bin/codex"),
        64,
    )
    .unwrap();
    let mut protocol = String::new();
    child.stdout.read_to_string(&mut protocol).await.unwrap();
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
            let child = execution::OwnedChild::spawn_captured(
                &request,
                lease,
                None,
                Path::new("/usr/bin/codex"),
                64,
            )
            .unwrap();
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
            let child = execution::OwnedProtocolChild::spawn(
                &request,
                lease,
                None,
                Path::new("/usr/bin/codex"),
                64,
            )
            .unwrap();
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

/// Returns the real captured default Codex managed state under `cwd`, optionally made unrecognized.
///
/// `recognized == false` replaces the root entry's access with `none`, which is exactly the shape
/// that may subtract read authority and must therefore keep strict sandbox-cwd equality.
fn inherited_managed_state(cwd: &Path, recognized: bool) -> serde_json::Value {
    json!({
        "codexLinuxSandboxExe": null,
        "permissionProfile": {
            "file_system": {
                "entries": [
                    {"access": if recognized {"read"} else {"none"},
                     "path":{"type":"special","value":{"kind":"root"}}},
                    {"access":"write","path":{"path": cwd, "type":"path"}},
                    {"access":"write","path":{"type":"special","value":{"kind":"slash_tmp"}}},
                    {"access":"read","missing_path_behavior":"skip",
                     "path":{"path": cwd.join(".git"), "type":"path"}}
                ],
                "type": "restricted"
            },
            "network": "enabled",
            "type": "managed"
        },
        "sandboxCwd": cwd,
        "useLegacyLandlock": false
    })
}

/// Seals the platform `/usr/bin/env` against its own current bytes, as an operator would declare
/// them, or returns `None` where this contract is unavailable or the file cannot be read.
fn accepted_env_trampoline() -> Option<ControlledTrampoline> {
    let declared = blake3::hash(&fs::read("/usr/bin/env").ok()?)
        .to_hex()
        .to_string();
    ControlledTrampoline::accept(PathBuf::from("/usr/bin/env"), &declared).ok()
}

/// Builds a managed invocation for `sandbox_cwd` plus its matching Execution-owned catalog.
fn inherited_invocation(
    sandbox_cwd: &Path,
    recognized: bool,
) -> (ValidatedHostInvocation, ExecutionProfileCatalog) {
    let state =
        HostSandboxState::parse(Some(inherited_managed_state(sandbox_cwd, recognized))).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("inherited-cwd-case", 1, &state).unwrap(),
    ])
    .unwrap();
    (
        ValidatedHostInvocation::from_verified_binding("inherited-bound", state).unwrap(),
        catalog,
    )
}

/// A native child inherits its parent's `sandboxCwd`, so a separate operator worktree must still
/// validate — but only under a profile that already grants read of the whole filesystem root, and
/// only with an accepted `/usr/bin/env` trampoline.
#[test]
fn inherited_sandbox_cwd_validates_only_for_a_recognized_read_all_profile_with_a_trampoline() {
    let root = worktree();
    let inherited = worktree();
    let authority =
        WorkspaceAuthority::from_workspace("inherited-worktree", "1", root.clone(), 7).unwrap();
    let command = || {
        ControlledCommand::from_validated_peer(
            CommandKind::Job,
            PathBuf::from("/usr/bin/true"),
            vec![OsString::from("--version")],
            root.clone(),
            BTreeMap::new(),
        )
        .unwrap()
    };
    let programs = || BTreeSet::from([PathBuf::from("/usr/bin/true")]);
    let plain = LocalExecutionPolicy::new(programs(), 4096, 4, false).unwrap();

    // An unrecognized profile keeps the original strict equality even with a trampoline available.
    let (invocation, catalog) = inherited_invocation(&inherited, false);
    assert_eq!(
        ValidatedExecutionRequest::validate(
            invocation,
            authority.clone(),
            command(),
            &plain,
            &catalog
        )
        .unwrap_err(),
        RequestError::SandboxCwdMismatch
    );

    // A recognized profile without an accepted trampoline is unavailable, never silently allowed.
    let (invocation, catalog) = inherited_invocation(&inherited, true);
    assert_eq!(
        ValidatedExecutionRequest::validate(
            invocation,
            authority.clone(),
            command(),
            &plain,
            &catalog
        )
        .unwrap_err(),
        RequestError::TrampolineUnavailable
    );

    // A command outside the authoritative worktree still fails first, trampoline or not.
    let (invocation, catalog) = inherited_invocation(&inherited, true);
    let elsewhere = ControlledCommand::from_validated_peer(
        CommandKind::Job,
        PathBuf::from("/usr/bin/true"),
        Vec::new(),
        inherited.clone(),
        BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        ValidatedExecutionRequest::validate(
            invocation,
            authority.clone(),
            elsewhere,
            &plain,
            &catalog
        )
        .unwrap_err(),
        RequestError::WorktreeDenied
    );

    let Some(trampoline) = accepted_env_trampoline() else {
        return;
    };
    let accepted =
        LocalExecutionPolicy::with_env_trampoline(programs(), 4096, 4, false, trampoline.clone())
            .unwrap();

    // The recognized profile plus an accepted trampoline is the one admitted combination, and the
    // replayed state keeps its exact original bytes.
    let (invocation, catalog) = inherited_invocation(&inherited, true);
    let raw = invocation.sandbox().sandbox_state_json().to_owned();
    let request = ValidatedExecutionRequest::validate(
        invocation,
        authority.clone(),
        command(),
        &accepted,
        &catalog,
    )
    .unwrap();
    assert_eq!(request.kind(), CommandKind::Job);
    assert_eq!(request.authority().root(), root.as_path());
    assert_eq!(
        raw,
        inherited_managed_state(&inherited, true).to_string(),
        "sandboxCwd is never rewritten toward the target worktree"
    );

    // A catalog whose access mode differs still refuses the otherwise portable profile. The
    // managed class has a template, so the refusal names the digest mismatch (T24B), not a
    // missing class template.
    let (invocation, _) = inherited_invocation(&inherited, true);
    let (_, foreign) = inherited_invocation(&worktree(), false);
    assert_eq!(
        ValidatedExecutionRequest::validate(
            invocation,
            authority.clone(),
            command(),
            &accepted,
            &foreign
        )
        .unwrap_err(),
        RequestError::ExecutionProfileDigestMismatch(ProfileClass::Managed)
    );

    // The added `-C <root> <program>` bytes count against the local argv ceiling.
    let tight = LocalExecutionPolicy::with_env_trampoline(
        programs(),
        "--version".len() + 1,
        4,
        false,
        trampoline,
    )
    .unwrap();
    let (invocation, catalog) = inherited_invocation(&inherited, true);
    assert_eq!(
        ValidatedExecutionRequest::validate(invocation, authority, command(), &tight, &catalog)
            .unwrap_err(),
        RequestError::ArgvTooLarge
    );
}

/// A durable-authorized native read accepts the inherited parent cwd only under the same
/// recognized read-all profile, and never accepts a changed authority or an unrecognized one.
#[test]
fn native_read_accepts_an_inherited_cwd_only_under_a_recognized_read_all_profile() {
    let root = worktree();
    let inherited = worktree();
    let authority =
        WorkspaceAuthority::from_workspace("inherited-read", "1", root.clone(), 3).unwrap();
    let read = |recognized: bool, authority: &WorkspaceAuthority| {
        let mut guard = HostBindingGuard::default();
        let channel = channel();
        assert!(matches!(
            guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
            BindingStatus::PreObserved
        ));
        let BindingStatus::Validated(invocation) =
            guard.establish_start(candidate("actor", "call"), channel)
        else {
            panic!("explicit start must establish the binding");
        };
        let active = guard.consume_active(invocation.binding_ref()).unwrap();
        let state = inherited_managed_state(&inherited, recognized);
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence(
                "inherited-read-case",
                1,
                &HostSandboxState::parse(Some(state.clone())).unwrap(),
            )
            .unwrap(),
        ])
        .unwrap();
        let observed = parse_observed_sandbox_state(
            json!({"codex/sandbox-state-meta": state})
                .as_object()
                .unwrap(),
            &invocation,
            &active,
            true,
        )
        .unwrap();
        execution::validate_workspace_read(active, observed, authority, &catalog, false)
    };
    read(true, &authority).unwrap();
    assert_eq!(
        read(false, &authority).unwrap_err(),
        RequestError::SandboxCwdMismatch
    );
    // A read-all profile is cwd-independent by construction, so a second Workspace-granted root is
    // served by the same state; the Workspace authority, not the cwd string, is what bounds it.
    let other = WorkspaceAuthority::from_workspace("other-read", "1", worktree(), 3).unwrap();
    read(true, &other).unwrap();
}

/// Proves physically that a real accepted Codex sandbox plus `/usr/bin/env` runs the utility in a
/// separate operator worktree, and that an inaccessible target never executes the marker.
///
/// Ignored by default: it spawns the operator's real `codex` binary and must run from an outer,
/// already-approved unsandboxed runner, because macOS refuses a nested `sandbox_apply`. Supply
/// `AGENT_IDE_PHYSICAL_CODEX` (absolute accepted `codex` path) and `AGENT_IDE_PHYSICAL_STATE`
/// (file holding the captured default `codex/sandbox-state-meta` JSON, whose `sandboxCwd` is the
/// inherited parent directory and whose profile grants read of `/`). Both absent, the test skips.
#[tokio::test]
#[ignore = "spawns the operator's real Codex binary; needs an outer unsandboxed runner"]
async fn physical_inherited_cwd_runs_the_marker_only_in_an_accessible_target_worktree() {
    let (Ok(codex), Ok(state_path)) = (
        std::env::var("AGENT_IDE_PHYSICAL_CODEX"),
        std::env::var("AGENT_IDE_PHYSICAL_STATE"),
    ) else {
        eprintln!("skipped: AGENT_IDE_PHYSICAL_CODEX/AGENT_IDE_PHYSICAL_STATE are unset");
        return;
    };
    let raw = fs::read_to_string(&state_path).unwrap();
    let state = HostSandboxState::parse_json(raw.trim()).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("physical-inherited", 1, &state).unwrap(),
    ])
    .unwrap();
    let trampoline = accepted_env_trampoline().expect("an accepted /usr/bin/env declaration");
    let policy = LocalExecutionPolicy::with_env_trampoline(
        BTreeSet::from([PathBuf::from("/bin/pwd")]),
        4096,
        0,
        false,
        trampoline,
    )
    .unwrap();

    // `target` is a plain temporary directory standing in for a separate operator worktree — this
    // check is about the launch boundary, not Git discovery — and `missing` is removed before the
    // spawn so the marker cannot be produced from an inaccessible directory. Both are canonicalized
    // first: Workspace only ever holds a canonical root, and on a default macOS `TMPDIR` the
    // symlinked `/var/folders/...` form would otherwise disagree with the real path the child
    // prints.
    let target = fs::canonicalize(worktree()).unwrap();
    let missing = fs::canonicalize(worktree()).unwrap();
    fs::remove_dir(&missing).unwrap();
    for (root, expected) in [(target.clone(), true), (missing.clone(), false)] {
        let authority =
            WorkspaceAuthority::from_workspace("physical-worktree", "1", root.clone(), 1).unwrap();
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Job,
            PathBuf::from("/bin/pwd"),
            Vec::new(),
            root.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let invocation =
            ValidatedHostInvocation::from_verified_binding("physical-bound", state.clone())
                .unwrap();
        assert_ne!(
            state.cwd(),
            root.as_path(),
            "the captured state must describe the inherited parent cwd, not the target worktree"
        );
        let request =
            ValidatedExecutionRequest::validate(invocation, authority, command, &policy, &catalog)
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
            OwnerId::new("physical").unwrap(),
            AdmissionClass::Interactive,
        ) else {
            panic!("physical spawn must be admitted");
        };
        let child = execution::OwnedChild::spawn_captured(
            &request,
            lease,
            None,
            Path::new(&codex),
            64 * 1024,
        );
        let marker = root.to_string_lossy().to_string();
        match child {
            Ok(child) => {
                let captured = child
                    .reap(Duration::from_secs(30), Duration::from_secs(30))
                    .await
                    .unwrap();
                let stdout = String::from_utf8_lossy(&captured.evidence.stdout().bytes).to_string();
                assert_eq!(
                    stdout.trim() == marker,
                    expected,
                    "target worktree marker presence must match accessibility: {stdout:?}"
                );
            }
            Err(error) => assert!(
                !expected,
                "an accessible target must physically launch, got {error:?}"
            ),
        }
    }
    fs::remove_dir_all(target).unwrap();
}
