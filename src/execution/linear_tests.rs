//! Exploit regressions for linear provider admission and exact direct-child settlement.

use super::*;

/// Creates one exact disabled-host command accepted by a test-only local execution profile.
fn request(kind: CommandKind, program: &str, args: &[&str]) -> ValidatedExecutionRequest {
    let root = PathBuf::from("/private/tmp");
    let sandbox=HostSandboxState::parse(Some(serde_json::json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":root,"useLegacyLandlock":false}))).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("linear-contract", 1, &sandbox).unwrap(),
    ])
    .unwrap();
    ValidatedExecutionRequest::validate(
        ValidatedHostInvocation::from_verified_binding("linear-contract", sandbox).unwrap(),
        WorkspaceAuthority::from_workspace("tree", "1", root.clone(), 1).unwrap(),
        ControlledCommand::from_validated_peer(
            kind,
            program.into(),
            args.iter().map(OsString::from).collect(),
            root,
            BTreeMap::new(),
        )
        .unwrap(),
        &LocalExecutionPolicy::new(BTreeSet::from([program.into()]), 4096, 16, true).unwrap(),
        &catalog,
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
    let child = OwnedProtocolChild::spawn_from_forwarder_lease(
        &provider,
        capability,
        None,
        Path::new("/unused"),
        64,
    )
    .unwrap();
    let job = request(CommandKind::Job, "/usr/bin/true", &[]);
    for forge_target in [false, true] {
        // This private construction models the formerly public Copy implementation only in the regression.
        let copied = AdmissionLease(key.0, key.1);
        let completed = OwnedChild::spawn_captured(&job, copied, None, Path::new("/unused"), 64)
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
    let capture = OwnedChild::spawn_captured(
        &request,
        lease(&mut admission),
        None,
        Path::new("/unused"),
        64,
    )
    .err()
    .unwrap();
    let protocol = OwnedProtocolChild::spawn(
        &request,
        lease(&mut admission),
        None,
        Path::new("/unused"),
        64,
    )
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
            OwnedProtocolChild::spawn_from_forwarder_lease(
                &request,
                capability,
                None,
                Path::new("/unused"),
                64,
            )
            .err()
            .unwrap()
        } else {
            OwnedProtocolChild::spawn_from_provider_lease(
                &request,
                listener.take().unwrap(),
                None,
                Path::new("/unused"),
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
    let error =
        OwnedChild::spawn_from_provider_lease(&request, capability, None, Path::new("/unused"), 64)
            .err()
            .unwrap();
    assert!(matches!(error, ProcessError::NeverStarted { .. }));
    assert_eq!(
        admission.running_count(),
        1,
        "lost registry does not fabricate settlement"
    );
}

/// Managed launch supplies wrapper, provider, and configured tool search roots without ambient PATH.
#[test]
fn managed_provider_search_path_is_explicit_and_complete() {
    let sandbox = HostSandboxState::parse(Some(serde_json::json!({
        "permissionProfile": {"type": "managed", "file_system": {}, "network": "restricted"},
        "sandboxCwd": "/private/tmp"
    })))
    .unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        PathBuf::from("/usr/bin/true"),
        Vec::new(),
        PathBuf::from("/private/tmp"),
        BTreeMap::from([(OsString::from("PATH"), OsString::from("/toolchain/bin"))]),
    )
    .unwrap();
    let process = build_command(&command, &sandbox, Path::new("/opt/codex/bin/codex")).unwrap();
    let path = process
        .as_std()
        .get_envs()
        .find_map(|(name, value)| (name == "PATH").then_some(value.unwrap()))
        .unwrap();
    assert_eq!(
        std::env::split_paths(path).collect::<Vec<_>>(),
        ["/opt/codex/bin", "/usr/bin", "/toolchain/bin"]
            .map(PathBuf::from)
            .to_vec()
    );
}
