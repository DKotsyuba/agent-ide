//! Linear provider settlement, fresh host binding, and cancellable discovery regressions.

#[path = "support/provider.rs"]
mod provider;
use agent_ide::{
    execution::*,
    intelligence::gopls::{GoplsProfile, SharedGopls},
    workspace::authority::WorktreeRef,
};
use provider::BoundRequest;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::io::AsyncReadExt;

/// Separates exact fixture directories within a process.
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Owns one temporary directory whose contents are all test-generated.
struct Fixture(PathBuf);
impl Fixture {
    /// Creates a private absolute cwd for controlled shell/true commands.
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/private/tmp/provider-seam-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    /// Returns an exact worktree reference and matching Execution scope.
    fn scope(&self) -> (WorktreeRef, WorkspaceAuthority) {
        let tree =
            WorktreeRef::from_discovery(self.0.clone(), self.0.clone(), ".git".into(), 1).unwrap();
        let authority =
            WorkspaceAuthority::from_workspace(tree.id(), "1", self.0.clone(), 1).unwrap();
        (tree, authority)
    }
    /// Creates a direct bounded job for non-provider capture/discovery correlation tests.
    fn job(&self, label: &str, program: &Path, args: Vec<std::ffi::OsString>) -> BoundRequest {
        let (_, authority) = self.scope();
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Job,
            program.to_path_buf(),
            args,
            self.0.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        BoundRequest::new(label, authority, command, program)
    }
    /// Creates one host-bound fixed provider command in the fixture cwd.
    fn command(&self, label: &str, program: &Path, args: Vec<std::ffi::OsString>) -> BoundRequest {
        let (_, authority) = self.scope();
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            program.to_path_buf(),
            args,
            self.0.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        BoundRequest::new(label, authority, command, program)
    }
}
impl Drop for Fixture {
    /// Removes only the test-owned cwd after owned process cleanup.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Creates one finite controller and registry for exact physical accounting assertions.
fn controllers(total: usize) -> (AdmissionController, ProviderLeaseRegistry) {
    (
        AdmissionController::new(AdmissionLimits {
            total_running: total,
            per_owner_running: total,
            per_owner_queued: 4,
            total_queued: 4,
            interactive_burst: 1,
        })
        .unwrap(),
        ProviderLeaseRegistry::new(ProviderLeaseLimits {
            total_views: 4,
            per_backend_views: 4,
        })
        .unwrap(),
    )
}
/// Reserves one isolated backend without bypassing central admission.
fn backend(
    registry: &mut ProviderLeaseRegistry,
    admission: &mut AdmissionController,
    name: &str,
    authority: &WorkspaceAuthority,
) -> ProviderViewLease {
    match registry.request(
        admission,
        OwnerId::new(name).unwrap(),
        AdmissionClass::Interactive,
        name,
        ProviderBackendKind::OwnedExclusive,
        authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        other => panic!("expected view: {other:?}"),
    }
}
/// Extracts the explicit pre-child failure; any other process error is not safe no-spawn evidence.
fn never_started(error: ProcessError) -> (ProcessError, SpawnNeverStarted) {
    match error {
        ProcessError::NeverStarted { cause, settlement } => (*cause, settlement),
        other => panic!("no definite pre-child proof: {other:?}"),
    }
}

/// Missing/mismatched uses and OS spawn errors cannot launch or promote until exact no-spawn settlement.
#[tokio::test]
async fn provider_pre_spawn_failures_settle_once_without_reap_fiction() {
    let fixture = Fixture::new();
    for failure in ["missing", "stale", "authority", "os"] {
        let os_program;
        let program = if failure == "os" {
            // Declared while present so construction still measures a real identity; removed
            // below, right before spawn, so the OS-level failure is a genuine vanished executable
            // rather than an identity Execution could reject at declaration time.
            use std::os::unix::fs::PermissionsExt;
            os_program = fixture.0.join("vanishing-provider");
            fs::write(&os_program, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&os_program, fs::Permissions::from_mode(0o700)).unwrap();
            os_program.as_path()
        } else {
            Path::new("/usr/bin/true")
        };
        let mut bound = fixture.command(failure, program, vec![]);
        let (mut admission, mut registry) = controllers(1);
        let authority = bound.request.authority().clone();
        let view = backend(&mut registry, &mut admission, "first", &authority);
        let mut other = fixture.command("other-binding", Path::new("/usr/bin/true"), vec![]);
        let use_now = if failure == "missing" {
            None
        } else if failure == "stale" {
            Some(other.fresh())
        } else {
            Some(bound.fresh())
        };
        let wrong_authority =
            WorkspaceAuthority::from_workspace(authority.worktree_id(), "1", fixture.0.clone(), 2)
                .unwrap();
        let wrong_command = ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            program.to_path_buf(),
            vec![],
            fixture.0.clone(),
            BTreeMap::new(),
        )
        .unwrap();
        let wrong = BoundRequest::new("wrong-authority", wrong_authority, wrong_command, program);
        let request = if failure == "authority" {
            &wrong.request
        } else {
            &bound.request
        };
        if failure == "os" {
            fs::remove_file(program).unwrap();
        }
        let error = OwnedProtocolChild::spawn_from_provider_lease(
            request,
            registry.take_spawn_lease(view).unwrap(),
            use_now,
            Path::new("/unused"),
            64,
        )
        .err()
        .unwrap();
        let (cause, proof) = never_started(error);
        if matches!(failure, "missing" | "stale") {
            assert!(matches!(
                cause,
                ProcessError::Request(RequestError::MissingActiveBindingUse)
            ));
        }
        assert!(matches!(
            registry.request(
                &mut admission,
                OwnerId::new("queued").unwrap(),
                AdmissionClass::Interactive,
                "queued",
                ProviderBackendKind::OwnedExclusive,
                &authority
            ),
            ProviderLeaseAdmission::Queued(_)
        ));
        assert_eq!(admission.running_count(), 1);

        let promotions = registry
            .settle_never_started(&mut admission, proof)
            .unwrap();
        assert_eq!(promotions.len(), 1);
        assert!(matches!(
            registry.cancel_unstarted(&mut admission, view),
            Err(ProviderLeaseError::UnknownView)
        ));
        let promoted = registry
            .promote(
                &mut admission,
                promotions.into_iter().next().unwrap(),
                &authority,
            )
            .unwrap();
        registry.cancel_unstarted(&mut admission, promoted).unwrap();
        assert_eq!(admission.running_count(), 0);
    }
}

/// A TERM-ignoring protocol child keeps the last-view slot draining until KILL plus successful wait.
#[tokio::test]
async fn protocol_cancellation_fences_promotion_until_direct_child_reap() {
    let fixture = Fixture::new();
    let mut bound = fixture.command(
        "hung-protocol",
        Path::new("/bin/sh"),
        vec![
            "-c".into(),
            "trap '' TERM; printf ready; while :; do sleep 1; done".into(),
        ],
    );
    let (mut admission, mut registry) = controllers(1);
    let authority = bound.request.authority().clone();
    let view = backend(&mut registry, &mut admission, "hung", &authority);
    let active = bound.fresh();
    let mut child = OwnedProtocolChild::spawn_from_provider_lease(
        &bound.request,
        registry.take_spawn_lease(view).unwrap(),
        Some(active),
        Path::new("/unused"),
        64,
    )
    .unwrap();
    let mut ready = [0; 5];
    tokio::time::timeout(Duration::from_secs(1), child.stdout.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ready, b"ready");
    let BackendRelease::ReapOwned(reap) = registry.release(view).unwrap() else {
        panic!("draining capability")
    };

    assert_eq!(registry.counts(), (1, 0));
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("same").unwrap(),
            AdmissionClass::Interactive,
            "hung",
            ProviderBackendKind::OwnedExclusive,
            &authority
        ),
        ProviderLeaseAdmission::Rejected(ProviderLeaseError::BackendDraining)
    ));
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("next").unwrap(),
            AdmissionClass::Interactive,
            "next",
            ProviderBackendKind::OwnedExclusive,
            &authority
        ),
        ProviderLeaseAdmission::Queued(_)
    ));
    assert_eq!(admission.running_count(), 1);
    let result = child
        .cancel_and_reap(Duration::from_millis(20), Duration::from_secs(1))
        .await
        .unwrap();
    assert!(result.cancellation.unwrap().kill_requested);
    assert_eq!(result.descendants, DescendantEvidence::Unverified);
    assert_eq!(
        admission.running_count(),
        1,
        "reap alone never releases a provider slot"
    );
    let promotions = registry
        .complete_reap(&mut admission, reap, result.proof)
        .unwrap();
    assert_eq!(promotions.len(), 1);
    assert!(matches!(
        registry.release(view),
        Err(ProviderLeaseError::UnknownView)
    ));

    let next = registry
        .promote(
            &mut admission,
            promotions.into_iter().next().unwrap(),
            &authority,
        )
        .unwrap();
    registry.cancel_unstarted(&mut admission, next).unwrap();
    assert_eq!(admission.running_count(), 0);
    assert_eq!(registry.counts(), (0, 0));
}

/// Expired protocol waits never return settlement proof or free their uncertain reservation.
#[tokio::test]
async fn hung_protocol_wait_is_bounded_and_keeps_accounting_uncertain() {
    let fixture = Fixture::new();
    let mut bound = fixture.command(
        "timeout",
        Path::new("/bin/sh"),
        vec!["-c".into(), "sleep 10".into()],
    );
    let (mut admission, mut registry) = controllers(1);
    let view = backend(
        &mut registry,
        &mut admission,
        "timeout",
        bound.request.authority(),
    );
    let active = bound.fresh();
    let child = OwnedProtocolChild::spawn_from_provider_lease(
        &bound.request,
        registry.take_spawn_lease(view).unwrap(),
        Some(active),
        Path::new("/unused"),
        64,
    )
    .unwrap();
    let BackendRelease::ReapOwned(_pending) = registry.release(view).unwrap() else {
        panic!("draining capability")
    };

    assert!(matches!(
        child.reap(Duration::from_millis(20)).await,
        Err(ProcessError::ReapTimedOut)
    ));
    assert_eq!(admission.running_count(), 1);
    assert_eq!(registry.counts(), (1, 0));
    assert_eq!(
        BorrowedEndpoint::observe("borrowed").unwrap().cancel(),
        EndpointOwnership::Borrowed
    );
}

/// A hung fixed-query discovery retains its handle during cancellable wait and preserves raw provenance.
#[tokio::test]
async fn discovery_can_be_cancelled_without_losing_query_or_release_evidence() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let script = fixture.0.join("git-fixture");
    fs::write(
        &script,
        "#!/bin/sh\ntrap '' TERM\nprintf 'raw-discovery'\nprintf ready > discovery-ready\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bound = fixture.command("discovery", &script, vec![]);
    let operation = DiscoveryOperationRef::new("discovery-operation").unwrap();
    let request = DiscoverWorktreeRequest::from_active_observation(
        bound.fresh(),
        bound.observed.clone(),
        fixture.0.clone().into_os_string(),
        operation.clone(),
    )
    .unwrap()
    .validate_query(
        GitDiscoveryQuery::ShowTopLevel,
        &GitDiscoveryPolicy::new(script, 128, true).unwrap(),
        &bound.catalog,
    )
    .unwrap();
    let (mut admission, _) = controllers(1);
    let lease = match admission.submit(
        OwnerId::new("discovery").unwrap(),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        _ => panic!("discovery slot"),
    };
    let mut child = request
        .spawn(lease, bound.fresh(), Path::new("/unused"))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.0.join("discovery-ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        child.wait(Duration::from_millis(30)).await,
        Err(ProcessError::ReapTimedOut)
    ));
    assert_eq!(admission.running_count(), 1);
    let result = child
        .cancel_and_reap(Duration::from_millis(20), Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(result.evidence.operation(), &operation);
    assert_eq!(result.evidence.query(), GitDiscoveryQuery::ShowTopLevel);
    assert!(
        result.evidence.stdout().bytes.starts_with(b"raw-discovery"),
        "capture: {:?}",
        result.evidence
    );
    assert!(result.evidence.cancellation().is_some());
    admission.release_reaped(result.settlement).unwrap();
    assert_eq!(admission.running_count(), 0);
}

/// gopls listener/forwarder wrappers accept genuine host-bound fresh uses instead of substituting None.
#[tokio::test]
async fn gopls_wrappers_forward_fresh_binding_uses() {
    let fixture = Fixture::new();
    let (tree, authority) = fixture.scope();
    let profile = GoplsProfile::new(
        "/usr/bin/true".into(),
        "fixture".into(),
        "v1".into(),
        "default".into(),
        "/usr/bin/true".into(),
        "test".into(),
        fixture.0.join("gopls-cache").display().to_string(),
    )
    .unwrap();
    let socket = fixture.0.join("unused.sock");
    let (mut admission, mut registry) = controllers(2);
    let view = match registry.request(
        &mut admission,
        OwnerId::new("gopls").unwrap(),
        AdmissionClass::Interactive,
        profile.compatibility_key(),
        ProviderBackendKind::OwnedShared,
        &authority,
    ) {
        ProviderLeaseAdmission::Granted(view) => view,
        _ => panic!("gopls view"),
    };
    let mut listener = BoundRequest::new(
        "gopls-listener",
        authority.clone(),
        profile.listener_command(&authority, &socket).unwrap(),
        Path::new("/usr/bin/true"),
    );
    let active = listener.fresh();
    let mut shared = SharedGopls::start(
        &profile,
        &listener.request,
        registry.take_spawn_lease(view).unwrap(),
        Some(active),
        Path::new("/unused"),
        64,
    )
    .unwrap();
    let mut forwarder = BoundRequest::new(
        "gopls-forwarder",
        authority.clone(),
        profile.forwarder_command(&authority, &socket).unwrap(),
        Path::new("/usr/bin/true"),
    );
    let slot = match admission.submit(
        OwnerId::new("forwarder").unwrap(),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        _ => panic!("forwarder slot"),
    };
    let capability = registry
        .take_forwarder_spawn_lease(&mut admission, view, &forwarder.request, slot)
        .unwrap();
    let active = forwarder.fresh();
    let child = shared
        .open_view(
            tree.clone(),
            1,
            &forwarder.request,
            capability,
            Some(active),
            Path::new("/unused"),
            64,
        )
        .unwrap();
    let reaped = child.child.reap(Duration::from_secs(1)).await.unwrap();
    registry
        .complete_forwarder_reap(&mut admission, reaped.proof)
        .unwrap();
    shared.release_view(&tree, view).unwrap();
    let BackendRelease::ReapOwned(capability) = registry.release(view).unwrap() else {
        panic!("draining capability")
    };
    let listener = shared
        .stop(Duration::from_millis(10), Duration::from_secs(1))
        .await
        .unwrap();
    registry
        .complete_reap(&mut admission, capability, listener.settlement)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
}

/// Settles each ordinary captured slot immediately, returning only bounded immutable snapshot evidence.
async fn settled_snapshot(
    admission: &mut AdmissionController,
    bound: &mut BoundRequest,
) -> CapturedProcessEvidence {
    let lease = match admission.submit(
        OwnerId::new("snapshot").unwrap(),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        _ => panic!("previous snapshot retained a reservation"),
    };
    let active = bound.fresh();
    let completed = OwnedChild::spawn_captured(
        &bound.request,
        lease,
        Some(active),
        Path::new("/unused"),
        128,
    )
    .unwrap()
    .reap(Duration::from_secs(1), Duration::from_secs(1))
    .await
    .unwrap();
    admission.release_reaped(completed.settlement).unwrap();
    completed.evidence
}

/// Three snapshot query results can be retained for parsing while every completed process slot is already free.
#[tokio::test]
async fn captured_snapshot_evidence_retains_no_process_reservations() {
    let fixture = Fixture::new();
    let mut bound = fixture.job(
        "snapshot",
        Path::new("/bin/sh"),
        vec!["-c".into(), "printf snapshot".into()],
    );
    let (mut admission, _) = controllers(1);
    let mut evidence = Vec::new();
    for _ in 0..3 {
        evidence.push(settled_snapshot(&mut admission, &mut bound).await);
        assert_eq!(admission.running_count(), 0);
    }
    assert!(
        evidence
            .iter()
            .all(|value| value.status().success() && value.stdout().bytes == b"snapshot")
    );
    let invalid = CapturedOutput {
        bytes: vec![1, 2],
        truncated: false,
        drained_bytes: 1,
        complete: true,
    };
    assert!(
        CapturedProcessEvidence::new(
            evidence[0].status(),
            None,
            invalid,
            evidence[0].stderr().clone(),
            DescendantEvidence::Unverified
        )
        .is_err()
    );
}

/// Definite ordinary spawn failures settle exactly once; duplicate proof attempts cannot free/promote again.
#[tokio::test]
async fn definite_no_child_settlement_is_not_repeatable() {
    let fixture = Fixture::new();
    // Declared while present so construction still measures a real identity; removed below,
    // right before spawn, so the no-spawn evidence comes from a genuinely vanished executable
    // rather than an identity Execution could reject at declaration time.
    use std::os::unix::fs::PermissionsExt;
    let program_path = fixture.0.join("vanishing-git-fixture");
    fs::write(&program_path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program_path, fs::Permissions::from_mode(0o700)).unwrap();
    let program = program_path.as_path();
    let mut bound = fixture.command("no-discovery", program, vec![]);
    let request = DiscoverWorktreeRequest::from_active_observation(
        bound.fresh(),
        bound.observed.clone(),
        fixture.0.clone().into_os_string(),
        DiscoveryOperationRef::new("no-child").unwrap(),
    )
    .unwrap()
    .validate_query(
        GitDiscoveryQuery::GitCommonDir,
        &GitDiscoveryPolicy::new(program.to_path_buf(), 128, true).unwrap(),
        &bound.catalog,
    )
    .unwrap();
    let (mut admission, _) = controllers(1);
    let lease = match admission.submit(
        OwnerId::new("no-child").unwrap(),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        _ => panic!("slot"),
    };
    fs::remove_file(program).unwrap();
    let (_, first) = never_started(
        request
            .spawn(lease, bound.fresh(), Path::new("/unused"))
            .err()
            .unwrap(),
    );
    assert_eq!(admission.running_count(), 1);
    admission.settle_never_started(first).unwrap();
    assert_eq!(admission.running_count(), 0);
    // Both the discovery request and its admission are consumed; duplicate issuance is a compile error.
}

/// Equal local numeric IDs cannot mix reservations from two distinct central controllers.
#[test]
fn opaque_reservations_include_their_controller_identity() {
    let (mut left, _) = controllers(1);
    let (mut right, _) = controllers(1);
    let Admission::Granted(a) =
        left.submit(OwnerId::new("left").unwrap(), AdmissionClass::Interactive)
    else {
        panic!()
    };
    let Admission::Granted(b) =
        right.submit(OwnerId::new("right").unwrap(), AdmissionClass::Interactive)
    else {
        panic!()
    };
    assert_ne!(a, b);
    assert_eq!(left.release(b), Err(AdmissionError::UnknownLease));
    assert_eq!(left.running_count(), 1);
    left.release(a).unwrap();
    assert_eq!(
        right.running_count(),
        1,
        "a foreign failed release cannot free its rightful controller"
    );
}

/// Native reads and cached delivery recheck changed host permissions even when the binding stays live.
#[test]
fn current_read_admission_rejects_changed_profile_and_cwd_without_a_command() {
    use agent_ide::assistance::host_binding::parse_observed_sandbox_state;
    use serde_json::json;
    let fixture = Fixture::new();
    let mut bound = fixture.command("native-read", Path::new("/usr/bin/true"), vec![]);
    let authority = bound.request.authority().clone();
    validate_workspace_read(
        bound.fresh(),
        bound.observed.clone(),
        &authority,
        &bound.catalog,
        true,
        agent_ide::execution::ReadScope::WholeTree,
    )
    .unwrap();
    assert!(matches!(
        validate_workspace_read(
            bound.fresh(),
            bound.observed.clone(),
            &authority,
            &bound.catalog,
            false,
            agent_ide::execution::ReadScope::WholeTree,
        ),
        Err(RequestError::DisabledHostDenied)
    ));
    for changed_cwd in [false, true] {
        let mut raw = bound.observed.state().as_json().clone();
        if changed_cwd {
            raw["sandboxCwd"] = json!(fixture.0.join("other"));
        } else {
            raw["permissionProfile"]["changed-permission"] = json!(true);
        }
        let active = bound.fresh();
        let observed = parse_observed_sandbox_state(
            json!({"codex/sandbox-state-meta":raw}).as_object().unwrap(),
            &bound.invocation,
            &active,
            true,
        )
        .unwrap();
        assert!(
            validate_workspace_read(
                active,
                observed,
                &authority,
                &bound.catalog,
                true,
                agent_ide::execution::ReadScope::WholeTree,
            )
            .is_err()
        );
    }
    bound.guard.stop_binding(&bound.binding).unwrap();
    assert!(bound.guard.consume_active(&bound.binding).is_err());
}

/// Actual wait identity can release only the exact armed child, while checked fixture data cannot forge it.
#[tokio::test]
async fn captured_wait_identity_is_bound_to_the_exact_child() {
    let fixture = Fixture::new();
    let mut bound = fixture.job("identity", Path::new("/usr/bin/true"), vec![]);
    let (mut admission, _) = controllers(2);
    let mut identities = Vec::new();
    let mut completed = Vec::new();
    for _ in 0..2 {
        let Admission::Granted(lease) = admission.submit(
            OwnerId::new("identity").unwrap(),
            AdmissionClass::Interactive,
        ) else {
            panic!()
        };
        let active = bound.fresh();
        let mut child = OwnedChild::spawn_captured(
            &bound.request,
            lease,
            Some(active),
            Path::new("/unused"),
            64,
        )
        .unwrap();
        identities.push(child.take_process_identity().unwrap());
        assert!(child.take_process_identity().is_none());
        completed.push(
            child
                .reap(Duration::from_secs(1), Duration::from_secs(1))
                .await
                .unwrap(),
        );
    }
    assert!(
        completed[0]
            .evidence
            .reap_identity()
            .unwrap()
            .matches(&identities[0])
    );
    assert!(
        !completed[0]
            .evidence
            .reap_identity()
            .unwrap()
            .matches(&identities[1])
    );
    let copied = completed[0].evidence.reap_identity().unwrap().clone();
    assert!(copied.matches(&identities[0]));
    let fixture = CapturedProcessEvidence::new(
        completed[0].evidence.status(),
        None,
        completed[0].evidence.stdout().clone(),
        completed[0].evidence.stderr().clone(),
        DescendantEvidence::Unverified,
    )
    .unwrap();
    assert!(fixture.reap_identity().is_none());
    for completed in completed {
        admission.release_reaped(completed.settlement).unwrap();
    }
    assert_eq!(admission.running_count(), 0);
}

/// Replacing a validated provider executable cannot launch different bytes under the old request.
#[tokio::test]
async fn provider_spawn_rejects_executable_replacement_before_child_creation() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let program = fixture.0.join("provider");
    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let mut bound = fixture.command("replace", &program, vec![]);
    let (mut admission, mut registry) = controllers(1);
    let authority = bound.request.authority().clone();
    let view = backend(&mut registry, &mut admission, "replace", &authority);

    fs::write(&program, "#!/bin/sh\nexit 7\n").unwrap();
    let active = bound.fresh();
    let error = OwnedProtocolChild::spawn_from_provider_lease(
        &bound.request,
        registry.take_spawn_lease(view).unwrap(),
        Some(active),
        Path::new("/unused"),
        64,
    )
    .err()
    .unwrap();
    let (cause, settlement) = never_started(error);
    assert!(matches!(
        cause,
        ProcessError::Request(RequestError::ExecutableUnavailable)
    ));
    registry
        .settle_never_started(&mut admission, settlement)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
}

/// An executable absent at declaration cannot gain identity by appearing before spawn is attempted.
#[test]
fn provider_command_construction_fails_closed_when_executable_is_missing_at_declaration() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let program = fixture.0.join("late-provider");

    let missing = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        program.clone(),
        vec![],
        fixture.0.clone(),
        BTreeMap::new(),
    );
    assert!(matches!(missing, Err(RequestError::ExecutableUnavailable)));

    fs::write(&program, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();

    // The earlier, rejected declaration never observed the file: it stays unusable and there is
    // no path from a failed construction to a later spawn. Only a fresh declaration made after
    // the executable exists can measure and admit it.
    assert!(
        ControlledCommand::from_validated_peer(
            CommandKind::Provider,
            program,
            vec![],
            fixture.0.clone(),
            BTreeMap::new(),
        )
        .is_ok()
    );
}

/// Equal caller metadata cannot make different provider executables share one backend identity.
#[test]
fn provider_compatibility_uses_measured_executable_bytes() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = Fixture::new();
    let left = fixture.0.join("left-provider");
    let right = fixture.0.join("right-provider");
    fs::write(&left, "#!/bin/sh\nexit 0\n").unwrap();
    fs::write(&right, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&left, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&right, fs::Permissions::from_mode(0o700)).unwrap();
    let cache_namespace = fixture.0.join("gopls-cache").display().to_string();
    let profile = |binary| {
        GoplsProfile::new(
            binary,
            "self-attested-version".into(),
            "v1".into(),
            "default".into(),
            "/usr/bin/true".into(),
            "test".into(),
            cache_namespace.clone(),
        )
        .unwrap()
    };
    assert_ne!(
        profile(left).compatibility_key(),
        profile(right).compatibility_key()
    );
}

/// Pending views count against configured ceilings and inspection distinguishes queue from reservation.
#[test]
fn pending_provider_capacity_is_bounded_and_truthfully_inspected() {
    let fixture = Fixture::new();
    let (_, authority) = fixture.scope();
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 2,
        total_queued: 2,
        interactive_burst: 1,
    })
    .unwrap();
    let Admission::Granted(blocker) = admission.submit(
        OwnerId::new("blocker").unwrap(),
        AdmissionClass::Interactive,
    ) else {
        panic!("blocker slot")
    };
    let mut registry = ProviderLeaseRegistry::new(ProviderLeaseLimits {
        total_views: 1,
        per_backend_views: 1,
    })
    .unwrap();
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("first").unwrap(),
            AdmissionClass::Interactive,
            "first",
            ProviderBackendKind::OwnedExclusive,
            &authority,
        ),
        ProviderLeaseAdmission::Queued(_)
    ));
    assert!(matches!(
        registry.request(
            &mut admission,
            OwnerId::new("second").unwrap(),
            AdmissionClass::Interactive,
            "second",
            ProviderBackendKind::OwnedExclusive,
            &authority,
        ),
        ProviderLeaseAdmission::Rejected(ProviderLeaseError::ViewCapacity)
    ));
    let process = admission.inspect();
    assert_eq!(
        (process.reserved, process.queued, process.globally_available),
        (1, 1, 0)
    );
    let providers = registry.inspect();
    assert_eq!((providers.active_views, providers.pending_views), (0, 1));
    let promotions = admission.release_with_promotions(blocker).unwrap();
    assert_eq!(promotions.len(), 1);
    let view = registry
        .promote(
            &mut admission,
            promotions.into_iter().next().unwrap(),
            &authority,
        )
        .unwrap();
    let providers = registry.inspect();
    assert_eq!(
        (
            providers.active_views,
            providers.pending_views,
            providers.spawnable_backends
        ),
        (1, 0, 1)
    );
    registry.cancel_unstarted(&mut admission, view).unwrap();
}
