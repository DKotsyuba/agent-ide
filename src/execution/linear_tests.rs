//! Exploit regressions for linear provider admission and exact direct-child settlement.

use super::*;
use tokio::io::AsyncBufReadExt;

/// Creates one synthetic request with a bounded local execution policy.
fn request(kind: CommandKind, program: &str, args: &[&str]) -> ValidatedExecutionRequest {
    let root = PathBuf::from("/private/tmp");
    ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("linear-contract").unwrap(),
        WorkspaceAuthority::from_workspace("tree", "1", root.clone(), 1).unwrap(),
        ControlledCommand::from_validated_peer(
            kind,
            program.into(),
            args.iter().map(OsString::from).collect(),
            root,
            BTreeMap::new(),
        )
        .unwrap(),
        &LocalExecutionPolicy::new(BTreeSet::from([program.into()]), 4096, 16).unwrap(),
    )
    .unwrap()
}

/// Returns a finite central controller and logical-view registry for one isolated exploit.
fn controllers() -> (AdmissionController, ProviderLeaseRegistry) {
    (
        AdmissionController::new(AdmissionLimits {
            total_running: 4,
            per_owner_running: 4,
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

/// Reserves one owned shared backend and returns the logical view owning its pending listener.
fn view(
    registry: &mut ProviderLeaseRegistry,
    admission: &mut AdmissionController,
    request: &ValidatedExecutionRequest,
) -> ProviderViewLease {
    let ProviderLeaseAdmission::Granted(view) = registry.request(
        admission,
        OwnerId::new("provider").unwrap(),
        AdmissionClass::Interactive,
        "shared",
        ProviderBackendKind::OwnedShared,
        request.authority(),
    ) else {
        panic!("provider grant");
    };
    view
}

/// Reserves one distinct direct process slot before it is moved into a typed forwarder.
fn lease(admission: &mut AdmissionController) -> AdmissionLease {
    let Admission::Granted(lease) = admission.submit(
        OwnerId::new("forwarder").unwrap(),
        AdmissionClass::Interactive,
    ) else {
        panic!("direct grant");
    };
    lease
}

/// Reproduces the historical copied-lease bypass internally; public copying is now a compile error.
/// Neither an ordinary second child's reap nor forged matching target metadata can release the live forwarder.
#[tokio::test]
async fn second_arbitrary_child_cannot_settle_a_live_forwarder() {
    let (mut admission, mut registry) = controllers();
    let provider = request(CommandKind::Provider, "/bin/sleep", &["30"]);
    let view = view(&mut registry, &mut admission, &provider);
    let lease = lease(&mut admission);
    let key = lease.key();
    let capability = registry
        .take_forwarder_spawn_lease(&mut admission, view, &provider, lease)
        .unwrap();
    let child =
        OwnedProtocolChild::spawn_from_forwarder_lease(&provider, capability, None, 64).unwrap();
    let job = request(CommandKind::Job, "/usr/bin/true", &[]);
    for forge_target in [false, true] {
        // This private construction models the formerly public Copy implementation only in the regression.
        let copied = AdmissionLease(key.0, key.1);
        let completed = OwnedChild::spawn_captured(&job, copied, None, 64)
            .unwrap()
            .reap(Duration::from_secs(2), Duration::from_secs(2))
            .await
            .unwrap();
        let mut proof = completed.settlement;
        if forge_target {
            proof.target = SpawnTarget::Forwarder {
                backend: "shared".into(),
                view,
            };
        }
        assert_eq!(
            registry.complete_forwarder_reap(&mut admission, proof),
            Err(ProviderLeaseError::InvalidReap)
        );
        assert_eq!(admission.running_count(), 2);
        assert_eq!(registry.forwarder_count(), 1);
    }
    let completed = child
        .cancel_and_reap(Duration::from_millis(10), Duration::from_secs(2))
        .await
        .unwrap();
    let duplicate = DirectChildReap {
        lease: AdmissionLease(key.0, key.1),
        target: completed.proof.target.clone(),
        identity: completed.proof.identity,
    };
    registry
        .complete_forwarder_reap(&mut admission, completed.proof)
        .unwrap();
    assert_eq!(admission.running_count(), 1);
    assert_eq!(
        registry.complete_forwarder_reap(&mut admission, duplicate),
        Err(ProviderLeaseError::InvalidReap)
    );
    registry.cancel_unstarted(&mut admission, view).unwrap();
    assert_eq!(admission.running_count(), 0);
}

/// Rejects both raw spawn paths for Provider commands while returning their sole direct reservation.
#[tokio::test]
async fn raw_provider_spawns_require_typed_registry_capabilities() {
    let (mut admission, _) = controllers();
    let request = request(CommandKind::Provider, "/usr/bin/true", &[]);
    let capture = OwnedChild::spawn_captured(&request, lease(&mut admission), None, 64)
        .err()
        .unwrap();
    let protocol = OwnedProtocolChild::spawn(&request, lease(&mut admission), None, 64)
        .err()
        .unwrap();
    for error in [capture, protocol] {
        let ProcessError::NeverStarted { cause, settlement } = error else {
            panic!("definite no-child proof");
        };
        assert!(
            matches!(*cause,ProcessError::Io(ref error) if error.kind()==io::ErrorKind::PermissionDenied)
        );
        admission.settle_never_started(settlement).unwrap();
    }
    assert_eq!(admission.running_count(), 0);
}

/// Normal TypeScript-style protocol settlement reaps successfully without requesting any signal.
#[tokio::test]
async fn normal_protocol_reap_sends_no_signal_and_keeps_descendants_unverified() {
    let (mut admission, mut registry) = controllers();
    let provider = request(CommandKind::Provider, "/usr/bin/true", &[]);
    let view = view(&mut registry, &mut admission, &provider);
    let child = OwnedProtocolChild::spawn_from_provider_lease(
        &provider,
        registry.take_spawn_lease(view).unwrap(),
        None,
        64,
    )
    .unwrap();
    let BackendRelease::ReapOwned(capability) = registry.release(view).unwrap() else {
        panic!("exclusive provider must require direct-child reap")
    };

    let completed = child.reap(Duration::from_secs(2)).await.unwrap();

    assert!(completed.status.success());
    assert_eq!(completed.cancellation, None);
    assert_eq!(completed.descendants, DescendantEvidence::Unverified);
    registry
        .complete_reap(&mut admission, capability, completed.proof)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
}

/// Abnormal TypeScript cleanup preserves the TERM grace before group/direct KILL and direct reap.
#[cfg(unix)]
#[tokio::test]
async fn abnormal_typescript_cleanup_waits_full_grace_before_kill_and_reap() {
    let (mut admission, mut registry) = controllers();
    let provider = request(
        CommandKind::Provider,
        "/bin/sh",
        &[
            "-c",
            "trap 'exit 0' TERM; /bin/sh -c 'trap \"\" TERM; while :; do sleep 1; done' & printf '%s\\n' \"$!\"; while :; do sleep 1; done",
        ],
    );
    let view = view(&mut registry, &mut admission, &provider);
    let mut child = OwnedProtocolChild::spawn_from_provider_lease(
        &provider,
        registry.take_spawn_lease(view).unwrap(),
        None,
        Path::new("/unused"),
        64,
    )
    .unwrap();
    let mut descendant = String::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        tokio::io::BufReader::new(&mut child.stdout).read_line(&mut descendant),
    )
    .await
    .unwrap()
    .unwrap();
    let descendant: libc::pid_t = descendant.trim().parse().unwrap();
    let BackendRelease::ReapOwned(capability) = registry.release(view).unwrap() else {
        panic!("exclusive provider must require direct-child reap")
    };
    let grace = Duration::from_millis(100);
    let started = tokio::time::Instant::now();

    let completed = child
        .terminate_typescript_abnormally(grace, Duration::from_secs(2))
        .await
        .unwrap();

    assert!(started.elapsed() >= grace);
    assert_eq!(
        completed.cancellation,
        Some(CancellationEvidence {
            term_requested: true,
            kill_requested: true,
        })
    );
    assert_eq!(completed.descendants, DescendantEvidence::Unverified);
    registry
        .complete_reap(&mut admission, capability, completed.proof)
        .unwrap();
    assert_eq!(admission.running_count(), 0);
    tokio::time::timeout(Duration::from_secs(2), async {
        while unsafe { libc::kill(descendant, 0) } == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture descendant should be absent after group KILL");
}

/// Released logical views invalidate already-issued listener and forwarder launch capabilities.
#[tokio::test]
async fn issued_capabilities_cannot_spawn_after_logical_release() {
    for forwarder in [false, true] {
        let (mut admission, mut registry) = controllers();
        let request = request(CommandKind::Provider, "/usr/bin/true", &[]);
        let view = view(&mut registry, &mut admission, &request);
        let mut listener = Some(registry.take_spawn_lease(view).unwrap());
        let capability = if forwarder {
            let process_lease = lease(&mut admission);
            Some(
                registry
                    .take_forwarder_spawn_lease(&mut admission, view, &request, process_lease)
                    .unwrap(),
            )
        } else {
            None
        };
        let _draining = registry.release(view).unwrap();
        let error = if let Some(capability) = capability {
            OwnedProtocolChild::spawn_from_forwarder_lease(&request, capability, None, 64)
                .err()
                .unwrap()
        } else {
            OwnedProtocolChild::spawn_from_provider_lease(
                &request,
                listener.take().unwrap(),
                None,
                64,
            )
            .err()
            .unwrap()
        };
        let ProcessError::NeverStarted { settlement, .. } = error else {
            panic!("revoked before child exists");
        };
        registry
            .settle_never_started(&mut admission, settlement)
            .unwrap();
        assert_eq!(registry.forwarder_count(), 0);
        assert_eq!(admission.running_count(), usize::from(forwarder));
        if let Some(listener) = listener {
            registry
                .settle_never_started(&mut admission, listener.cancel())
                .unwrap();
        }
        assert_eq!(admission.running_count(), 0);
    }
}

/// Dropping the registry fences an outstanding capability; no lost registry can authorize a late spawn.
#[tokio::test]
async fn dropped_registry_cannot_authorize_an_outstanding_capability() {
    let (mut admission, mut registry) = controllers();
    let request = request(CommandKind::Provider, "/usr/bin/true", &[]);
    let view = view(&mut registry, &mut admission, &request);
    let capability = registry.take_spawn_lease(view).unwrap();
    drop(registry);
    let error = OwnedChild::spawn_from_provider_lease(&request, capability, None, 64)
        .err()
        .unwrap();
    assert!(matches!(error, ProcessError::NeverStarted { .. }));
    assert_eq!(
        admission.running_count(),
        1,
        "lost registry does not fabricate settlement"
    );
}
