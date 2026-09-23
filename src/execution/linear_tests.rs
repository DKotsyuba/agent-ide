//! Exploit regressions for linear provider admission and exact direct-child settlement.

use super::*;
use tokio::io::AsyncBufReadExt;

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
        Path::new("/unused"),
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

/// Managed launch supplies only its wrapper and explicitly configured search roots.
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
    let process =
        build_command(&command, &sandbox, Path::new("/opt/codex/bin/codex"), None).unwrap();
    let path = process
        .as_std()
        .get_envs()
        .find_map(|(name, value)| (name == "PATH").then_some(value.unwrap()))
        .unwrap();
    assert_eq!(
        std::env::split_paths(path).collect::<Vec<_>>(),
        ["/opt/codex/bin", "/toolchain/bin"]
            .map(PathBuf::from)
            .to_vec()
    );

    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        PathBuf::from("/usr/bin/true"),
        Vec::new(),
        PathBuf::from("/private/tmp"),
        BTreeMap::new(),
    )
    .unwrap();
    let process =
        build_command(&command, &sandbox, Path::new("/opt/codex/bin/codex"), None).unwrap();
    let path = process
        .as_std()
        .get_envs()
        .find_map(|(name, value)| (name == "PATH").then_some(value.unwrap()))
        .unwrap();
    assert_eq!(
        std::env::split_paths(path).collect::<Vec<_>>(),
        [PathBuf::from("/opt/codex/bin")],
        "an absolute provider does not expose its own directory implicitly"
    );
}

/// Two identical relative managed policies under different cwds must not share effective rights.
///
/// This is the sharing split GSC1 names: the same policy JSON resolved from `/a` and from `/b`
/// grants access to different absolute directories, so their identities must differ even though the
/// literal policy text is byte-identical. The positive half proves the identity is not merely
/// "hash everything": two states whose roots resolve to the *same* absolute directory share one
/// identity even though their `sandboxCwd` values differ, and a disabled profile — which applies no
/// cwd-derived restriction at all — shares across cwd under existing policy.
#[test]
fn effective_rights_identity_splits_relative_roots_and_shares_equal_absolute_rights() {
    use super::effective_rights_identity;
    let relative = |cwd: &str| {
        serde_json::json!({
            "permissionProfile":{"type":"managed","file_system":{"roots":["work"]},"network":false},
            "codexLinuxSandboxExe":null,"sandboxCwd":cwd,"useLegacyLandlock":false
        })
    };
    assert_ne!(
        effective_rights_identity(&relative("/private/tmp/a")),
        effective_rights_identity(&relative("/private/tmp/b")),
        "a relative root under a different cwd grants different rights and must not be shared"
    );
    assert_eq!(
        effective_rights_identity(&relative("/private/tmp/a")),
        effective_rights_identity(&serde_json::json!({
            "permissionProfile":{"type":"managed","file_system":{"roots":["/private/tmp/a/work"]},"network":false},
            "codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp/elsewhere","useLegacyLandlock":false
        })),
        "equal effective absolute rights share one identity even from a different cwd"
    );
    let disabled = |cwd: &str| {
        serde_json::json!({
            "permissionProfile":{"type":"disabled"},
            "codexLinuxSandboxExe":null,"sandboxCwd":cwd,"useLegacyLandlock":false
        })
    };
    assert_eq!(
        effective_rights_identity(&disabled("/private/tmp/a")),
        effective_rights_identity(&disabled("/private/tmp/b")),
        "a disabled profile applies no cwd-derived restriction and stays shareable"
    );
    // A managed profile that declares no root, an unwalkable root shape, and a `..` escape all stay
    // non-shareable across cwd instead of being widened into a match.
    for unproven in [
        serde_json::json!({"type":"managed","file_system":{},"network":false}),
        serde_json::json!({"type":"managed","file_system":{"roots":[{"opaque":1}]},"network":false}),
        serde_json::json!({"type":"managed","file_system":{"roots":["../escape"]},"network":false}),
        // An empty `*_roots` array proves nothing: no root was actually resolved against cwd, so
        // the policy must not be treated as cwd-independent just because the key is present.
        serde_json::json!({"type":"managed","file_system":{"roots":[]},"network":false}),
        // Mixing a proven-looking (but empty) `read_roots` with an unrecognized `entries` shape that
        // carries its own cwd-dependent relative path ("work") must refuse the whole profile instead
        // of letting the empty roots key falsely mark it cwd-independent while `entries` silently
        // keeps unresolved relative meaning.
        serde_json::json!({
            "type":"managed",
            "file_system":{"read_roots":[],"entries":[{"path":"work"}]},
            "network":false
        }),
        // A proven nonempty absolute root cannot make a sibling opaque relative policy safe to
        // share. The closed schema intentionally refuses `entries` until its semantics are known.
        serde_json::json!({
            "type":"managed",
            "file_system":{"roots":["/private/tmp/shared"],"entries":[{"path":"work"}]},
            "network":false
        }),
    ] {
        let state = |cwd: &str| {
            serde_json::json!({
                "permissionProfile":unproven,"codexLinuxSandboxExe":null,
                "sandboxCwd":cwd,"useLegacyLandlock":false
            })
        };
        assert_ne!(
            effective_rights_identity(&state("/private/tmp/a")),
            effective_rights_identity(&state("/private/tmp/b")),
            "an unproven managed policy must fail closed rather than share: {unproven}"
        );
    }
}

/// Returns the real captured default Codex managed state, with `sandboxCwd` replaced by `cwd`.
///
/// The entry shapes are the actual `permissionProfile.file_system.entries` a default Codex host
/// advertises, so the recognizer is exercised against real metadata rather than an invented schema.
fn real_managed_state(cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "codexLinuxSandboxExe": null,
        "permissionProfile": {
            "file_system": {
                "entries": [
                    {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
                    {"access":"write","path":{"path":"/private/tmp/host/work","type":"path"}},
                    {"access":"write","path":{"type":"special","value":{"kind":"slash_tmp"}}},
                    {"access":"write","path":{"type":"special","value":{"kind":"tmpdir"}}},
                    {"access":"read","missing_path_behavior":"skip",
                     "path":{"path":"/private/tmp/host/work/.git","type":"path"}}
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

/// Equivalent captured profiles match across workdirs, while an outside path remains significant.
#[test]
fn profile_templates_are_portable_only_for_worktree_paths() {
    let state = |cwd: &str, outside: &str| {
        HostSandboxState::parse(Some(serde_json::json!({
            "codexLinuxSandboxExe": null,
            "permissionProfile": {
                "file_system": {
                    "entries": [
                        {"access":"write","path":{"path":cwd,"type":"path"}},
                        {"access":"read","path":{"path":format!("{cwd}/.git"),"type":"path"}},
                        {"access":"read","path":{"path":format!("{cwd}/.agents"),"type":"path"}},
                        {"access":"read","path":{"path":format!("{cwd}/.codex"),"type":"path"}},
                        {"access":"read","path":{"path":outside,"type":"path"}},
                        {"access":"write","path":{"type":"special","value":{"kind":"tmpdir"}}}
                    ],
                    "type": "restricted"
                },
                "network": "restricted",
                "type": "managed",
                "unknown": {"preserved": true}
            },
            "sandboxCwd": cwd,
            "useLegacyLandlock": false
        })))
        .unwrap()
    };
    let first = state("/private/tmp/one/work", "/opt/shared/toolchain");
    let second = state("/private/tmp/two/work", "/opt/shared/toolchain");
    assert_eq!(first.profile_digest(), second.profile_digest());
    assert_eq!(
        semantic_state_identity(&first),
        semantic_state_identity(&second)
    );

    let outside_changed = state("/private/tmp/two/work", "/opt/other/toolchain");
    assert_ne!(first.profile_digest(), outside_changed.profile_digest());
    assert_ne!(
        semantic_state_identity(&first),
        semantic_state_identity(&outside_changed)
    );
    assert_eq!(first.raw["sandboxCwd"], "/private/tmp/one/work");
    assert_eq!(
        first.raw["permissionProfile"]["file_system"]["entries"][1]["path"]["path"],
        "/private/tmp/one/work/.git"
    );
}

/// The recognizer accepts the real default managed entries and refuses every unrecognized shape.
#[test]
fn read_all_recognition_is_closed_over_real_entry_shapes() {
    assert!(
        grants_read_of_all_roots(&real_managed_state("file:///private/tmp/host/work")),
        "the captured default Codex profile grants read of the whole root"
    );
    let entries = |extra: serde_json::Value| {
        serde_json::json!({
            "permissionProfile": {
                "file_system": {"entries": [extra], "type": "restricted"},
                "network": "enabled",
                "type": "managed"
            },
            "sandboxCwd": "file:///private/tmp/host/work"
        })
    };
    for refused in [
        // No root entry at all: nothing proves read of `/`.
        serde_json::json!({"access":"read","path":{"path":"/private/tmp","type":"path"}}),
        // Write authority alone is not accepted as proof of read authority.
        serde_json::json!({"access":"write","path":{"type":"special","value":{"kind":"root"}}}),
        // An explicit subtraction of access is never recognized.
        serde_json::json!({"access":"none","path":{"type":"special","value":{"kind":"root"}}}),
        // Unknown access, key, special kind, and path type each fail closed.
        serde_json::json!({"access":"append","path":{"type":"special","value":{"kind":"root"}}}),
        serde_json::json!({"access":"read","unknown":1,
                           "path":{"type":"special","value":{"kind":"root"}}}),
        serde_json::json!({"access":"read","path":{"type":"special","value":{"kind":"home"}}}),
        serde_json::json!({"access":"read","path":{"type":"glob","value":"/**"}}),
        // A relative or cwd-derived path is never resolved here.
        serde_json::json!({"access":"read","path":{"path":"work","type":"path"}}),
        serde_json::json!({"access":"read","path":{"path":"/private/tmp/../etc","type":"path"}}),
        // An unrecognized missing-path behaviour changes what the entries mean.
        serde_json::json!({"access":"read","missing_path_behavior":"error",
                           "path":{"type":"special","value":{"kind":"root"}}}),
    ] {
        assert!(
            !grants_read_of_all_roots(&entries(refused.clone())),
            "unrecognized entry must keep strict sandbox-cwd equality: {refused}"
        );
    }
    // A read-restricted or otherwise unrecognized file-system envelope is refused whole.
    for profile in [
        serde_json::json!({"file_system":{"type":"unrestricted"},"network":"enabled",
                           "type":"managed"}),
        serde_json::json!({"file_system":{"entries":[],"type":"restricted","extra":1},
                           "network":"enabled","type":"managed"}),
        serde_json::json!({"type":"disabled"}),
    ] {
        assert!(
            !grants_read_of_all_roots(&serde_json::json!({
                "permissionProfile": profile.clone(),
                "sandboxCwd": "file:///private/tmp/host/work"
            })),
            "unrecognized profile envelope must fail closed: {profile}"
        );
    }
}

/// A same-cwd managed launch keeps byte-identical argv; a differing cwd adds only `env -C <root>`.
#[test]
fn managed_argv_is_unchanged_without_a_trampoline_and_only_wrapped_with_one() {
    let sandbox = HostSandboxState::parse(Some(real_managed_state("/private/tmp"))).unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Provider,
        PathBuf::from("/usr/bin/true"),
        vec![OsString::from("--version")],
        PathBuf::from("/private/tmp"),
        BTreeMap::new(),
    )
    .unwrap();
    let argv = |trampoline: Option<&ControlledTrampoline>| {
        let process = build_command(
            &command,
            &sandbox,
            Path::new("/opt/codex/bin/codex"),
            trampoline,
        )
        .unwrap();
        let std_process = process.as_std();
        (
            std_process.get_program().to_owned(),
            std_process
                .get_args()
                .map(OsString::from)
                .collect::<Vec<_>>(),
        )
    };
    let (program, plain) = argv(None);
    assert_eq!(program, OsString::from("/opt/codex/bin/codex"));
    assert_eq!(
        plain,
        [
            OsString::from("sandbox"),
            OsString::from("--sandbox-state-json"),
            OsString::from(sandbox.sandbox_state_json()),
            OsString::from("--"),
            OsString::from("/usr/bin/true"),
            OsString::from("--version"),
        ]
    );
    let Some(trampoline) = accepted_env_trampoline() else {
        return;
    };
    let (wrapped_program, wrapped) = argv(Some(&trampoline));
    assert_eq!(wrapped_program, program, "the sandbox wrapper is unchanged");
    assert_eq!(
        wrapped,
        [
            OsString::from("sandbox"),
            OsString::from("--sandbox-state-json"),
            OsString::from(sandbox.sandbox_state_json()),
            OsString::from("--"),
            OsString::from("/usr/bin/env"),
            OsString::from("-C"),
            OsString::from("/private/tmp"),
            OsString::from("/usr/bin/true"),
            OsString::from("--version"),
        ],
        "only the fixed trampoline prefix is inserted, and the replayed state is untouched"
    );
}

/// Seals the platform `/usr/bin/env` against its own current bytes, as an operator would declare
/// them, or returns `None` where this contract is unavailable or the file cannot be read.
fn accepted_env_trampoline() -> Option<ControlledTrampoline> {
    let declared = blake3::hash(&std::fs::read("/usr/bin/env").ok()?)
        .to_hex()
        .to_string();
    ControlledTrampoline::accept(PathBuf::from("/usr/bin/env"), &declared).ok()
}

/// Only `/usr/bin/env` is accepted, and a program `env` would read as an assignment is refused.
#[test]
fn trampoline_acceptance_is_closed_and_rejects_assignment_programs() {
    assert_eq!(
        ControlledTrampoline::accept(PathBuf::from("/private/tmp/wrapper.sh"), &"0".repeat(64)),
        Err(RequestError::TrampolineUnavailable)
    );
    assert_eq!(
        ControlledTrampoline::accepts_program(Path::new("/private/tmp/NAME=value")),
        Err(RequestError::TrampolineProgramRejected)
    );
    assert_eq!(
        ControlledTrampoline::accepts_program(Path::new("/usr/bin/true")),
        Ok(())
    );
}

/// The seal is pinned to the operator's declaration and to the bytes measured at that moment.
///
/// An executable whose current bytes contradict the declared digest is never adopted as a new
/// baseline, and an identity that no longer describes the file on disk refuses to launch. Both
/// halves are exercised without touching the protected system `/usr/bin/env`.
#[cfg(target_os = "macos")]
#[test]
fn trampoline_seal_requires_the_declared_digest_and_refuses_a_changed_object() {
    assert_eq!(
        ControlledTrampoline::accept(PathBuf::from("/usr/bin/env"), &"0".repeat(64)),
        Err(RequestError::ExecutableUnavailable),
        "a wrong declared digest must not be re-baselined to whatever bytes are present"
    );
    let Some(mut trampoline) = accepted_env_trampoline() else {
        return;
    };
    trampoline.verified_path().unwrap();
    // Altering only the retained expected identity models an executable replaced after the
    // operator's declaration was accepted; the pre-spawn recheck must refuse it.
    trampoline.identity.digest = blake3::hash(b"replaced trampoline bytes");
    assert_eq!(
        trampoline.verified_path(),
        Err(RequestError::ExecutableUnavailable)
    );
}

/// An unknown key on the permission profile itself disqualifies read-all recognition.
///
/// The inner filesystem checks cannot see a profile-level permission this build has never parsed,
/// so the envelope is closed separately; opaque parsing and byte-exact replay stay unaffected.
#[test]
fn read_all_recognition_rejects_unknown_permission_profile_keys() {
    let mut state = real_managed_state("file:///private/tmp/host/work");
    assert!(grants_read_of_all_roots(&state));
    state["permissionProfile"]["future_permission"] = serde_json::json!({"deny": ["/"]});
    assert!(
        !grants_read_of_all_roots(&state),
        "an unrecognized profile-level permission must keep strict sandbox-cwd equality"
    );
    // The same state still parses and replays unchanged for ordinary same-cwd execution.
    let parsed = HostSandboxState::parse(Some(state.clone())).unwrap();
    assert_eq!(parsed.class(), ProfileClass::Managed);
    assert_eq!(parsed.sandbox_state_json(), state.to_string());
    // A profile missing one of the three known keys is equally unrecognized.
    let mut narrowed = real_managed_state("file:///private/tmp/host/work");
    narrowed["permissionProfile"]
        .as_object_mut()
        .unwrap()
        .remove("network");
    assert!(!grants_read_of_all_roots(&narrowed));
}

/// T24B: `permit` distinguishes a missing template from a stale profile digest, so the Assistance
/// error log can name exactly which execution-profile condition refused the observed class.
#[test]
fn permit_distinguishes_no_template_from_digest_mismatch() {
    let disabled = |landlock: bool| {
        HostSandboxState::parse(Some(serde_json::json!(
            {"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":landlock}
        )))
        .unwrap()
    };
    let observed = disabled(false);
    // No accepted template for the class at all.
    let empty = ExecutionProfileCatalog::from_execution_evidence(vec![]).unwrap();
    assert!(matches!(
        empty.permit(&observed, observed.cwd()),
        Err(RequestError::ExecutionProfileNoTemplate(
            ProfileClass::Disabled
        ))
    ));
    // A template for the class whose accepted digest differs from the observed state.
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("linear-contract", 1, &disabled(true))
            .unwrap(),
    ])
    .unwrap();
    assert!(matches!(
        catalog.permit(&observed, observed.cwd()),
        Err(RequestError::ExecutionProfileDigestMismatch(
            ProfileClass::Disabled
        ))
    ));
    // The exact accepted state itself still mints its permit.
    let accepted = disabled(true);
    assert!(catalog.permit(&accepted, accepted.cwd()).is_ok());
}

/// Builds one managed sandbox envelope whose digest follows its effective `network` policy.
fn managed_value(network: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "permissionProfile":{"type":"managed","file_system":{"roots":["/private/tmp/host/work"]},"network":network},
        "codexLinuxSandboxExe":null,"sandboxCwd":"file:///private/tmp/host/work","useLegacyLandlock":false
    })
}

/// Parses one [`managed_value`] envelope into a validated host sandbox state.
fn managed_state(network: serde_json::Value) -> HostSandboxState {
    HostSandboxState::parse(Some(managed_value(network))).unwrap()
}

/// T25B: several accepted profiles of one class each admit exactly their own state.
#[test]
fn catalog_permits_each_accepted_profile_of_one_class() {
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence(
            "managed-readonly",
            1,
            &managed_state(false.into()),
        )
        .unwrap(),
        ExecutionProfileTemplate::from_execution_evidence(
            "managed-write",
            1,
            &managed_state("restricted".into()),
        )
        .unwrap(),
    ])
    .unwrap();
    // Both accepted managed states mint their own permits; neither disturbs the other.
    let readonly = managed_state(false.into());
    let write = managed_state("restricted".into());
    assert!(catalog.permit(&readonly, readonly.cwd()).is_ok());
    assert!(catalog.permit(&write, write.cwd()).is_ok());
    // A third managed state no accepted template matches is still refused, naming the class.
    let unmatched = managed_state(true.into());
    assert!(matches!(
        catalog.permit(&unmatched, unmatched.cwd()),
        Err(RequestError::ExecutionProfileDigestMismatch(
            ProfileClass::Managed
        ))
    ));
}

/// T25B: digest remains a template's identity and eight stays the total accepted-profile cap.
#[test]
fn catalog_rejects_duplicate_digests_and_more_than_eight_profiles() {
    let template = |network| {
        ExecutionProfileTemplate::from_execution_evidence(
            "linear-contract",
            1,
            &managed_state(network),
        )
        .unwrap()
    };
    // An exact duplicate digest is denied even though both templates are well-formed.
    assert!(
        ExecutionProfileCatalog::from_execution_evidence(vec![
            template(false.into()),
            template(false.into())
        ])
        .is_err()
    );
    // Nine distinct profiles exceed the catalog's total cap and are denied outright.
    let networks: Vec<serde_json::Value> = (0..9)
        .map(|index| serde_json::json!(format!("net-{index}")))
        .collect();
    assert!(
        ExecutionProfileCatalog::from_execution_evidence(
            networks
                .iter()
                .map(|network| template(network.clone()))
                .collect()
        )
        .is_err()
    );
    // Eight distinct profiles, mixing classes, are still accepted.
    let eight: Vec<ExecutionProfileTemplate> = networks[..8]
        .iter()
        .map(|network| template(network.clone()))
        .collect();
    assert!(ExecutionProfileCatalog::from_execution_evidence(eight).is_ok());
}

/// Creates one unique empty per-test home directory below the system temporary directory.
fn rejected_capture_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "agent-ide-rejected-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&home).unwrap();
    home
}

/// Creates the capture directory below `home` as a real, private (0700) directory.
fn private_capture_dir(home: &Path) -> PathBuf {
    let dir = home.join(".agent-ide").join(super::REJECTED_PROFILES_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

/// T25B: a refused profile is captured once, privately, and round-trips to the same digest.
#[test]
fn rejected_capture_round_trips_once_and_never_rewrites() {
    let home = rejected_capture_home("capture");
    let state_value = managed_value(true.into());
    let expected = HostSandboxState::parse(Some(state_value.clone()))
        .unwrap()
        .profile_digest()
        .to_hex()
        .to_string();
    // The first rejection writes exactly one private file.
    let super::RejectedCapture::Captured(stem) =
        super::capture_rejected_profile_in(&home, &state_value)
    else {
        panic!("expected a capture");
    };
    assert_eq!(stem.len(), 16);
    assert_eq!(stem, expected[..16]);
    let path = home
        .join(".agent-ide")
        .join(super::REJECTED_PROFILES_DIR)
        .join(format!("{stem}.json"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    // The capture is the exact envelope `evidence record --sandbox-state` reads.
    let reparsed = HostSandboxState::parse_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(reparsed.profile_digest().to_hex().to_string(), expected);
    // A second identical rejection never rewrites: the directory still holds one file.
    assert_eq!(
        super::capture_rejected_profile_in(&home, &state_value),
        super::RejectedCapture::Unavailable
    );
    assert_eq!(
        std::fs::read_dir(home.join(".agent-ide").join(super::REJECTED_PROFILES_DIR))
            .unwrap()
            .count(),
        1
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// T25B: the capture directory retains at most sixteen files and skips beyond that silently.
#[test]
fn rejected_capture_stops_at_sixteen_files() {
    let home = rejected_capture_home("cap");
    let dir = private_capture_dir(&home);
    for index in 0..15 {
        std::fs::write(dir.join(format!("{index:016x}.json")), b"{}").unwrap();
    }
    // The sixteenth capture succeeds; the seventeenth distinct state is skipped silently.
    assert!(matches!(
        super::capture_rejected_profile_in(&home, &managed_value(true.into())),
        super::RejectedCapture::Captured(_)
    ));
    assert_eq!(
        super::capture_rejected_profile_in(&home, &managed_value("restricted".into())),
        super::RejectedCapture::Unavailable
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 16);
    let _ = std::fs::remove_dir_all(&home);
}

/// T25B: a symlinked capture directory is never written through; its target stays untouched.
#[cfg(unix)]
#[test]
fn rejected_capture_skips_a_symlinked_directory() {
    let home = rejected_capture_home("symlink");
    let real = home.join("real-target");
    std::fs::create_dir_all(&real).unwrap();
    let parent = home.join(".agent-ide");
    std::fs::create_dir_all(&parent).unwrap();
    std::os::unix::fs::symlink(&real, parent.join(super::REJECTED_PROFILES_DIR)).unwrap();
    assert_eq!(
        super::capture_rejected_profile_in(&home, &managed_value(true.into())),
        super::RejectedCapture::Unavailable
    );
    assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// T25B: a capture directory with group or other permission bits is never used.
#[cfg(unix)]
#[test]
fn rejected_capture_skips_a_shared_directory() {
    use std::os::unix::fs::PermissionsExt;
    let home = rejected_capture_home("shared");
    let dir = home.join(".agent-ide").join(super::REJECTED_PROFILES_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        super::capture_rejected_profile_in(&home, &managed_value(true.into())),
        super::RejectedCapture::Unavailable
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// T25B: a state with an unknown top-level field is never written, not even filtered.
#[test]
fn rejected_capture_skips_unknown_top_level_fields() {
    let home = rejected_capture_home("unknown");
    let dir = private_capture_dir(&home);
    let mut state = managed_value(true.into());
    state["session_id"] = serde_json::json!("leaky-host-session");
    assert_eq!(
        super::capture_rejected_profile_in(&home, &state),
        super::RejectedCapture::UnknownFields
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&home);
}

/// T25B: listing admits only bounded, correctly named regular captures, never symlinks.
#[cfg(unix)]
#[test]
fn rejected_listing_admits_only_bounded_regular_captures() {
    let home = rejected_capture_home("list");
    let dir = private_capture_dir(&home);
    // A symlinked entry whose target holds a valid state is ignored.
    let target = home.join("target.json");
    std::fs::write(&target, managed_value(true.into()).to_string()).unwrap();
    std::os::unix::fs::symlink(&target, dir.join("aaaaaaaaaaaaaaaa.json")).unwrap();
    // An oversized regular file and a wrongly named file are ignored, as is uppercase hex.
    std::fs::write(dir.join("bbbbbbbbbbbbbbbb.json"), vec![b'x'; 64 * 1024 + 1]).unwrap();
    std::fs::write(dir.join("not-a-capture.json"), b"{}").unwrap();
    std::fs::write(dir.join("CCCCCCCCCCCCCCCC.json"), b"{}").unwrap();
    // One genuine capture survives.
    std::fs::write(
        dir.join("0123456789abcdef.json"),
        managed_value(true.into()).to_string(),
    )
    .unwrap();
    let captures = super::list_rejected_profiles_in(&home);
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].name, "0123456789abcdef");
    assert_eq!(captures[0].class, super::ProfileClass::Managed);
    assert_eq!(captures[0].sandbox_cwd, "file:///private/tmp/host/work");
    let _ = std::fs::remove_dir_all(&home);
}

// ---------------------------------------------------------------------------
// T35B: profile-shape v2 derivation, conservative narrowing, and versioned
// records. The fixtures below are the nine captured sandbox states of the
// design's acceptance matrix; each test states which design row it seals.
// ---------------------------------------------------------------------------

/// Relocates a captured fixture into this process's temporary tree and creates its cwd.
///
/// Every original path under the developer's home or stability tree receives the same new
/// prefix, preserving relative paths and deny/glob relationships. The exact-capture assertions
/// below compare the relocated bytes because v2 capture identity intentionally binds the cwd.
pub(crate) fn sandbox_fixture(name: &str) -> String {
    let captured = std::fs::read_to_string(format!(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sandbox-states/{}.json"
        ),
        name.trim_end_matches(".json")
    ))
    .unwrap();
    let root = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("agent-ide-test-fixtures-{}", std::process::id()));
    let relocated = captured
        .replace("/Users/pluto", root.join("home").to_str().unwrap())
        .replace(
            "/private/tmp/agent-ide-stability",
            root.join("stability").to_str().unwrap(),
        );
    assert!(!relocated.contains("/Users/pluto"));
    assert!(!relocated.contains("/private/tmp/agent-ide-stability"));
    let value: serde_json::Value = serde_json::from_str(&relocated).unwrap();
    let cwd = value["sandboxCwd"].as_str().unwrap();
    std::fs::create_dir_all(cwd.strip_prefix("file://").unwrap_or(cwd)).unwrap();
    relocated
}

/// Parses one captured sandbox-state fixture into a validated host state.
fn fixture_state(name: &str) -> HostSandboxState {
    HostSandboxState::parse_json(&sandbox_fixture(name)).unwrap()
}

/// The short names the design matrix uses for the nine fixtures.
const FIXTURE_1ED: &str = "1ed43a00ce845709.json";
const FIXTURE_555: &str = "555ebcab7e884d62.json";
const FIXTURE_B8A: &str = "b8a736675faa7cba.json";
const FIXTURE_C9E: &str = "c9ea07ed289773b3.json";
const FIXTURE_FD3: &str = "fd37241d7322ebc3.json";
const FIXTURE_84F: &str = "84f07fac27b67d1d.json";
const FIXTURE_728: &str = "728b26d824d0d380.json";
const FIXTURE_READONLY_V1: &str = "accepted-codex-managed-read-only-v1.json";
const FIXTURE_WORKSPACE_WRITE_V1: &str = "accepted-codex-managed-workspace-write-v1.json";

/// Mints one v2 catalog carrying exactly the named fixture as its accepted template.
fn v2_catalog(name: &str) -> ExecutionProfileCatalog {
    ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2(
            "t35b-template",
            1,
            &fixture_state(name),
        )
        .unwrap(),
    ])
    .unwrap()
}

/// Runs the v2 permit decision of one accepted fixture template against one live fixture.
///
/// The trusted candidate is the live state's own cwd, exactly as a request validated in that
/// directory would bind it.
fn permit_fixture(template: &str, live: &str) -> Result<(), RequestError> {
    let live = fixture_state(live);
    v2_catalog(template).permit(&live, live.cwd()).map(|_| ())
}

/// Creates one synthetic capture cwd on disk so derivation's canonicalization sees it (T37B).
///
/// Real captures always name an existing directory; the synthetic fixtures name constants, so
/// the helpers materialize them. Failures are ignored: states whose cwd the grammar or the
/// binding must refuse keep refusing, only through the refusal they are testing.
fn ensure_real_dir(path: &str) -> &str {
    let _ = std::fs::create_dir_all(path);
    path
}

/// Creates the local directory of one sandbox-state JSON value's `sandboxCwd` spelling.
fn ensure_value_cwd(value: &serde_json::Value) {
    if let Some(cwd) = value.get("sandboxCwd").and_then(serde_json::Value::as_str) {
        let local = cwd.strip_prefix("file://").unwrap_or(cwd);
        let _ = std::fs::create_dir_all(local);
    }
}

/// Derives the v2 shape of one sandbox-state JSON value, or reports the unsupported reason.
///
/// The trusted candidate is the state's own cwd: the self-consistent binding.
fn shape_of(value: serde_json::Value) -> Result<ProfileShapeV2, UnsupportedShape> {
    ensure_value_cwd(&value);
    let state = HostSandboxState::parse(Some(value)).unwrap();
    state.shape_v2(state.cwd())
}

/// The exact fixture name every replay-preservation assertion re-checks.
#[test]
fn v2_fixture_matrix_admits_and_refuses_as_designed() {
    // One template minted from `1ed…` admits `1ed…`, `555…`, and `b8…` after portable
    // normalization of the cwd-derived selectors and glob bases.
    for admitted in [FIXTURE_1ED, FIXTURE_555, FIXTURE_B8A] {
        assert!(
            permit_fixture(FIXTURE_1ED, admitted).is_ok(),
            "the workspace-write template must admit {admitted}"
        );
    }
    // `c9…` and `fd…` carry additional, different absolute write grants and are refused.
    for refused in [FIXTURE_C9E, FIXTURE_FD3] {
        assert!(
            matches!(
                permit_fixture(FIXTURE_1ED, refused),
                Err(RequestError::ExecutionProfileShapeNotNarrower(
                    ProfileClass::Managed
                ))
            ),
            "the template must refuse {refused}"
        );
    }
    // `84f…` grants write of the whole root and is refused by every workspace-write template.
    for template in [FIXTURE_1ED, FIXTURE_WORKSPACE_WRITE_V1] {
        assert!(matches!(
            permit_fixture(template, FIXTURE_84F),
            Err(RequestError::ExecutionProfileShapeNotNarrower(_))
        ));
    }
    // `728…` matches the read-only template exactly.
    assert!(permit_fixture(FIXTURE_READONLY_V1, FIXTURE_728).is_ok());
    // The read-only template does not admit a workspace-write state.
    assert!(matches!(
        permit_fixture(FIXTURE_READONLY_V1, FIXTURE_1ED),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
}

/// Both accepted legacy files keep v1 behaviour byte-for-byte (T35B compatibility half).
#[test]
fn accepted_legacy_files_keep_v1_digest_admission() {
    for name in [FIXTURE_READONLY_V1, FIXTURE_WORKSPACE_WRITE_V1] {
        let state = fixture_state(name);
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence("legacy-v1", 1, &state).unwrap(),
        ])
        .unwrap();
        // The exact state admits through the legacy exact-digest path.
        assert!(catalog.permit(&state, state.cwd()).is_ok());
        // The v1 record layout is unchanged: eleven fields, no shape_version key.
        let record = PersistedProfileRecord::from_execution_evidence(
            "legacy-v1",
            1,
            D03ProfileEvidence {
                provider_binary: "p".into(),
                toolchain: "t".into(),
                configuration: "c".into(),
                trust: "u".into(),
                transport: "x".into(),
                d03_evidence: "d".into(),
            },
            &state,
        )
        .unwrap();
        assert!(!record.to_json().contains("shape_version"));
        assert_eq!(
            PersistedProfileRecord::from_json(&record.to_json()).unwrap(),
            record
        );
    }
    // The read-only v1 file and its compact reserialization of `728…` even share one legacy
    // digest, proving v1 portability is untouched.
    assert_eq!(
        fixture_state(FIXTURE_READONLY_V1).profile_digest(),
        fixture_state(FIXTURE_728).profile_digest()
    );
}

/// Reordering entries and duplicating identical rules never change the v2 shape (T35B).
#[test]
fn reordering_and_duplicates_do_not_change_the_v2_shape() {
    let base = sandbox_fixture(FIXTURE_1ED);
    let entries = serde_json::from_str::<serde_json::Value>(&base).unwrap()
        ["permissionProfile"]["file_system"]["entries"]
        .as_array()
        .unwrap()
        .clone();
    let mut reordered = entries.clone();
    reordered.reverse();
    let mut duplicated = entries.clone();
    duplicated.extend(entries.iter().cloned());
    let digest = |entries: &[serde_json::Value]| {
        let mut value = serde_json::from_str::<serde_json::Value>(&base).unwrap();
        value["permissionProfile"]["file_system"]["entries"] = entries.to_vec().into();
        shape_of(value).unwrap().digest()
    };
    assert_eq!(digest(&entries), digest(&reordered));
    assert_eq!(digest(&entries), digest(&duplicated));
    // And the reordered, duplicated live state still matches the pristine template exactly.
    let template = fixture_state(FIXTURE_1ED);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-template", 1, &template)
            .unwrap(),
    ])
    .unwrap();
    let mut live = serde_json::from_str::<serde_json::Value>(&base).unwrap();
    live["permissionProfile"]["file_system"]["entries"] = duplicated.into();
    let live = HostSandboxState::parse(Some(live)).unwrap();
    assert!(catalog.permit(&live, live.cwd()).is_ok());
}

/// Returns the permit outcome for one mutated copy of the T35B base state (attack harness).
///
/// The unmutated base state is the positive control: it is admitted by the template minted
/// from it. Every attack applies exactly one mutation to a fresh copy.
fn t35b_permit_outcome(mutation: impl FnOnce(&mut serde_json::Value)) -> Result<(), RequestError> {
    let mut live_value = t35b_base_state();
    mutation(&mut live_value);
    permit_values(t35b_base_state(), live_value)
}

/// Mints a v2 template from `template_value` and permits `live_value` against it.
fn permit_values(
    template_value: serde_json::Value,
    live_value: serde_json::Value,
) -> Result<(), RequestError> {
    ensure_value_cwd(&template_value);
    ensure_value_cwd(&live_value);
    let template = HostSandboxState::parse(Some(template_value)).unwrap();
    let live = HostSandboxState::parse(Some(live_value)).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-base", 1, &template).unwrap(),
    ])
    .unwrap();
    catalog.permit(&live, live.cwd()).map(|_| ())
}

/// Builds one synthetic workspace-write managed state shaped like a real default capture.
fn t35b_base_state() -> serde_json::Value {
    ensure_real_dir("/private/tmp/t35b/work");
    serde_json::json!({
        "codexLinuxSandboxExe": null,
        "permissionProfile": {
            "file_system": {
                "entries": [
                    {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
                    {"access":"write","path":{"path":"/private/tmp/t35b/work","type":"path"}},
                    {"access":"write","path":{"type":"special","value":{"kind":"slash_tmp"}}},
                    {"access":"write","path":{"type":"special","value":{"kind":"tmpdir"}}},
                    {"access":"read","missing_path_behavior":"skip",
                     "path":{"path":"/private/tmp/t35b/work/.git","type":"path"}},
                    {"access":"deny","path":{"path":"/Users/pluto/.aws","type":"path"}},
                    {"access":"deny","path":{"pattern":"/private/tmp/t35b/work/**/.env","type":"glob_pattern"}}
                ],
                "glob_scan_max_depth": 8,
                "type": "restricted"
            },
            "network": "enabled",
            "type": "managed"
        },
        "sandboxCwd": "/private/tmp/t35b/work",
        "useLegacyLandlock": false
    })
}

/// Every positive-selector set mutation is refused; the unmutated control is admitted (§5).
#[test]
fn attack_selector_set_mutations_refuse() {
    // Positive control first: the exact base state admits.
    assert!(t35b_permit_outcome(|_| {}).is_ok());

    // Add an outside write selector.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(
                    {"access":"write","path":{"path":"/opt/escape","type":"path"}}
                ));
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Add an outside read selector.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(
                    {"access":"read","path":{"path":"/opt/secret","type":"path"}}
                ));
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Replace the cwd write with a root write (`84f…` shape). The mutated state also carries
    // two conflicting accesses at the root selector, so the closed refusal is either the
    // unsupported-shape reason or the shape-proof failure — never an admission.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            let entries = state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap();
            entries[1] = serde_json::json!(
                {"access":"write","path":{"type":"special","value":{"kind":"root"}}}
            );
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_)
            | RequestError::ExecutionProfileShapeUnsupported(_))
    ));
    // Replace `slash_tmp` with `tmpdir`: special selectors are distinct, so the template over a
    // `slash_tmp` write never admits a state whose temporary-directory write moved to `tmpdir`.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"][2]["path"]["value"]["kind"] =
                serde_json::json!("tmpdir");
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Add a read below an accepted deny: a new positive selector can reopen a denied subtree.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!(
                    {"access":"read","path":{"path":"/Users/pluto/.aws/creds","type":"path"}}
                ));
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Remove the `.git` skip-read restriction: never treated as redundant root-read coverage.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .remove(4);
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Removing a write selector stays refused by this deliberately incomplete proof.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap()
                .remove(2);
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
}

/// Network and mechanism mutations refuse; the safe network direction may pass (§5).
#[test]
fn attack_network_and_mechanism_mutations_refuse() {
    // Restricted → enabled network refuses.
    let mut restricted_template = t35b_base_state();
    restricted_template["permissionProfile"]["network"] = serde_json::json!("restricted");
    let mut enabled_live = restricted_template.clone();
    enabled_live["permissionProfile"]["network"] = serde_json::json!("enabled");
    assert!(matches!(
        permit_values(restricted_template.clone(), enabled_live),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // The reverse direction may pass.
    assert!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["network"] = serde_json::json!("restricted");
        })
        .is_ok()
    );
    // Flipping the Landlock mode refuses: exact mechanism mismatch.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["useLegacyLandlock"] = serde_json::json!(true);
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
}

/// Accepted denies are required; added supported denies narrow without native-read authority.
#[test]
fn deny_churn_is_monotonic_and_never_creates_read_authority() {
    // Removing an accepted path deny refuses.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            let entries = state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap();
            entries.retain(|entry| entry["access"] != "deny" || entry["path"]["type"] != "path");
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Weakening an accepted glob deny refuses: a different tail is a different required rule.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            let entries = state["permissionProfile"]["file_system"]["entries"]
                .as_array_mut()
                .unwrap();
            for entry in entries.iter_mut() {
                if entry["access"] == "deny" && entry["path"]["type"] == "glob_pattern" {
                    entry["path"]["pattern"] =
                        serde_json::json!("/private/tmp/t35b/work/**/.env.bak");
                }
            }
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Adding supported deny rules narrows without re-minting, but it grants no native-read
    // authority: the whole-root read classifier still refuses deny-bearing states unchanged.
    let mut narrowed = t35b_base_state();
    narrowed["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(
            {"access":"deny","path":{"path":"/Users/pluto/.ssh","type":"path"}}
        ));
    let template = HostSandboxState::parse(Some(t35b_base_state())).unwrap();
    let live = HostSandboxState::parse(Some(narrowed.clone())).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-base", 1, &template).unwrap(),
    ])
    .unwrap();
    assert!(catalog.permit(&live, live.cwd()).is_ok());
    assert!(
        !live.grants_read_of_all_roots() && live.has_deny_entries(),
        "admission never creates native-read or cached-disclosure authority"
    );
}

/// Unknown fields and selectors refuse v2 derivation itself, even benign-looking ones (§5).
#[test]
fn unknown_fields_and_selectors_refuse_derivation() {
    let refuse = |mutation: &dyn Fn(&mut serde_json::Value)| {
        let mut value = t35b_base_state();
        mutation(&mut value);
        assert!(
            shape_of(value).is_err(),
            "an unknown field or selector must refuse v2 derivation"
        );
    };
    // Unknown top-level, permission-profile, filesystem, entry, and selector fields.
    refuse(&|state| {
        state["future_top_level"] = serde_json::json!(1);
    });
    refuse(&|state| {
        state["permissionProfile"]["future_permission"] = serde_json::json!({"deny": ["/"]});
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["future"] = serde_json::json!(1);
    });
    refuse(&|state| {
        let entries = state["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap();
        entries[0]
            .as_object_mut()
            .unwrap()
            .insert("unknown".into(), serde_json::json!(1));
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["path"]["unknown"] =
            serde_json::json!(1);
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["path"]["value"]["unknown"] =
            serde_json::json!(1);
    });
    // Unknown selector and access values: never guessed.
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["path"]["type"] =
            serde_json::json!("glob");
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["path"]["value"]["kind"] =
            serde_json::json!("home");
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["access"] =
            serde_json::json!("none");
    });
    // Unknown network and missing-path values, and an unknown filesystem type.
    refuse(&|state| {
        state["permissionProfile"]["network"] = serde_json::json!(true);
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["entries"][0]["missing_path_behavior"] =
            serde_json::json!("error");
    });
    refuse(&|state| {
        state["permissionProfile"]["file_system"]["type"] = serde_json::json!("unrestricted");
    });
    // A missing-path behavior on a deny has unreviewed semantics and refuses.
    refuse(&|state| {
        let entries = state["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap();
        entries[5]
            .as_object_mut()
            .unwrap()
            .insert("missing_path_behavior".into(), serde_json::json!("skip"));
    });
    // A `/` cwd refuses the portable workspace binding outright.
    refuse(&|state| {
        state["sandboxCwd"] = serde_json::json!("/");
    });
    // A disabled profile has no v2 shape; it stays a legacy v1 exact-digest concern.
    assert!(
        shape_of(serde_json::json!({
            "permissionProfile": {"type": "disabled"},
            "codexLinuxSandboxExe": null,
            "sandboxCwd": "/private/tmp/t35b/work",
            "useLegacyLandlock": false
        }))
        .is_err()
    );
}

/// Identical duplicates are harmless; conflicting accesses at one selector refuse (§5).
#[test]
fn duplicate_rules_deduplicate_and_conflicts_refuse() {
    // Two identical read entries and two identical denies deduplicate to one shape.
    let mut deduped = t35b_base_state();
    let root_read = serde_json::json!(
        {"access":"read","path":{"type":"special","value":{"kind":"root"}}}
    );
    let aws_deny =
        serde_json::json!({"access":"deny","path":{"path":"/Users/pluto/.aws","type":"path"}});
    deduped["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .extend([root_read, aws_deny]);
    assert_eq!(
        shape_of(t35b_base_state()).unwrap().digest(),
        shape_of(deduped).unwrap().digest()
    );
    // The same selector with conflicting accesses refuses outright.
    let mut conflicting = t35b_base_state();
    conflicting["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(
            {"access":"read","path":{"path":"/private/tmp/t35b/work","type":"path"}}
        ));
    assert!(shape_of(conflicting).is_err());
    // Order never decides: the conflict refuses in either array position.
    let mut conflicting = t35b_base_state();
    conflicting["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .insert(
            0,
            serde_json::json!(
                {"access":"read","path":{"path":"/private/tmp/t35b/work","type":"path"}}
            ),
        );
    assert!(shape_of(conflicting).is_err());
}

/// The strict local path grammar refuses traversal, relative, NUL, and URI variants (§5).
#[test]
fn path_grammar_refuses_before_normalization() {
    let entry = |path: serde_json::Value| {
        let mut value = t35b_base_state();
        value["permissionProfile"]["file_system"]["entries"][1]["path"] = path;
        shape_of(value)
    };
    // `..`, embedded `.`, relative paths, NUL, and ambiguous separators.
    for refused in [
        "/private/tmp/t35b/../t35b/work",
        "/private/tmp/t35b/./work",
        "private/tmp/t35b/work",
        "/private/tmp/t35b/work\0",
        "/private/tmp//t35b/work",
        "/private/tmp/t35b/work/",
    ] {
        assert!(
            entry(serde_json::json!({"path": refused, "type": "path"})).is_err(),
            "the grammar must refuse {refused:?}"
        );
    }
    // `file://host`, percent encoding, and query/fragment URI variants.
    for refused in [
        "file://localhost/private/tmp/t35b/work",
        "file:///private/tmp/t35b%32/work",
        "file:///private/tmp/t35b/work?x=1",
        "file:///private/tmp/t35b/work#frag",
        "file:private/tmp/t35b/work",
    ] {
        assert!(
            entry(serde_json::json!({"path": refused, "type": "path"})).is_err(),
            "the grammar must refuse {refused:?}"
        );
    }
    // The supported local spellings both derive.
    assert!(entry(serde_json::json!({"path": "/private/tmp/t35b/work", "type": "path"})).is_ok());
    assert!(
        entry(serde_json::json!({"path": "file:///private/tmp/t35b/work", "type": "path"})).is_ok()
    );
}

/// Case, Unicode, and prefix collisions never merge two distinct selectors (§5).
#[test]
fn selector_identities_are_byte_exact_and_component_wise() {
    // A sibling sharing a string prefix stays outside the trusted cwd, component-wise.
    let value = |cwd: &str, write: &str| {
        let mut state = t35b_base_state();
        state["sandboxCwd"] = serde_json::json!(cwd);
        state["permissionProfile"]["file_system"]["entries"][1]["path"] =
            serde_json::json!({"path": write, "type": "path"});
        state
    };
    let inside = shape_of(value(
        "/private/tmp/t35b/work",
        "/private/tmp/t35b/work/sub",
    ))
    .unwrap();
    let escape = shape_of(value(
        "/private/tmp/t35b/work",
        "/private/tmp/t35b/work-escape",
    ))
    .unwrap();
    let escape_components = vec![
        "private".to_owned(),
        "tmp".to_owned(),
        "t35b".to_owned(),
        "work-escape".to_owned(),
    ];
    // The sibling string-prefix path stays an absolute selector outside the workspace form.
    assert!(
        escape
            .rules
            .contains_key(&profile_shape::Selector::Absolute(
                escape_components.clone()
            ))
    );
    assert!(
        !escape
            .rules
            .contains_key(&profile_shape::Selector::WorkspaceRelative(Vec::new()))
    );
    let inside_components = vec!["sub".to_owned()];
    assert!(
        inside
            .rules
            .contains_key(&profile_shape::Selector::WorkspaceRelative(
                inside_components
            ))
    );
    assert!(
        !inside
            .rules
            .contains_key(&profile_shape::Selector::Absolute(escape_components))
    );
    // Case and Unicode spellings never fold: a differing spelling is a different selector, so
    // a template over one spelling never admits a state spelled differently. The composed and
    // decomposed spellings of the same directory name are likewise never merged.
    for spelled in [
        "/Private/tmp/t35b/work",
        "/private/tmp/t35b/w\u{f6}rk",
        "/private/tmp/t35b/wo\u{308}rk",
    ] {
        let mut live = t35b_base_state();
        live["permissionProfile"]["file_system"]["entries"][1]["path"] =
            serde_json::json!({"path": spelled, "type": "path"});
        assert!(matches!(
            permit_values(t35b_base_state(), live),
            Err(RequestError::ExecutionProfileShapeNotNarrower(_))
        ));
    }
}

/// The v2 shape digest is domain-separated and independent of JSON spelling (T35B).
#[test]
fn shape_digest_is_domain_separated_and_spelling_independent() {
    let compact = fixture_state(FIXTURE_1ED);
    // The same state reserialized with pretty spacing derives the identical shape.
    let pretty = {
        let value: serde_json::Value = serde_json::from_str(&sandbox_fixture(FIXTURE_1ED)).unwrap();
        HostSandboxState::parse_json(&serde_json::to_string_pretty(&value).unwrap()).unwrap()
    };
    let compact_shape = compact.shape_v2(compact.cwd()).unwrap();
    let pretty_shape = pretty.shape_v2(pretty.cwd()).unwrap();
    assert_eq!(compact_shape, pretty_shape);
    assert_eq!(compact_shape.digest(), pretty_shape.digest());
    // The shape digest is not the raw-state digest and differs across states.
    assert_ne!(
        compact_shape.digest().to_hex().to_string(),
        blake3::hash(compact.sandbox_state_json().as_bytes())
            .to_hex()
            .to_string()
    );
    let write_fixture = fixture_state(FIXTURE_WORKSPACE_WRITE_V1);
    assert_ne!(
        compact_shape.digest(),
        write_fixture
            .shape_v2(write_fixture.cwd())
            .unwrap()
            .digest()
    );
    // A v2 record's captured-state identity is separately domain-separated and pins the cwd.
    let captured = super::captured_state_identity_v2(&compact);
    assert_ne!(captured, super::semantic_state_identity(&compact));
    let moved = sandbox_fixture(FIXTURE_1ED).replace("agent-pipline-compressor", "elsewhere");
    assert_ne!(
        captured,
        super::captured_state_identity_v2(&HostSandboxState::parse_json(&moved).unwrap()),
        "the v2 captured-state identity includes the actual cwd"
    );
}

/// A v2 record round-trips, restores only against its own capture, and replays byte-for-byte.
#[test]
fn v2_records_round_trip_and_pin_the_exact_capture() {
    let captured_text = sandbox_fixture(FIXTURE_1ED);
    let captured = HostSandboxState::parse_json(&captured_text).unwrap();
    let record = PersistedProfileRecord::from_execution_evidence_v2(
        "t35b-managed-write",
        3,
        D03ProfileEvidence {
            provider_binary: "codex".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "managed".into(),
            d03_evidence: "d03-run".into(),
        },
        &captured,
    )
    .unwrap();
    // Twelve fields, explicit shape_version, exact capture identity.
    let parsed = PersistedProfileRecord::from_json(&record.to_json()).unwrap();
    assert_eq!(parsed, record);
    assert!(parsed.matches_state(&captured));
    // Restoration is exact evidence matching, never subtyping: a different managed capture
    // (even one the minted template would admit live) does not match the record.
    assert!(!parsed.matches_state(&fixture_state(FIXTURE_555)));
    // A v2 catalog rebuilt from the record admits the live narrower states through `permit`.
    let expected = vec![record.clone()];
    let catalog = ExecutionProfileCatalog::from_persisted_records(
        vec![(parsed, captured.clone())],
        &expected,
    )
    .unwrap();
    assert!(catalog.permit(&captured, captured.cwd()).is_ok());
    let live = fixture_state(FIXTURE_555);
    assert!(catalog.permit(&live, live.cwd()).is_ok());
    // Derivation, digests, and permits never rewrite the replay JSON.
    assert_eq!(captured.sandbox_state_json(), captured_text);
}

/// Record-version confusion, tampered identities, and unknown keys all fail closed (T35B).
#[test]
fn record_version_parsing_is_closed() {
    let captured = fixture_state(FIXTURE_1ED);
    let record = PersistedProfileRecord::from_execution_evidence_v2(
        "t35b-managed-write",
        1,
        D03ProfileEvidence {
            provider_binary: "codex".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "managed".into(),
            d03_evidence: "d03-run".into(),
        },
        &captured,
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&record.to_json()).unwrap();
    // Unknown versions, an explicit v1 key (a mixed layout), string/null versions, and unknown
    // or missing keys refuse.
    for refused in [
        {
            let mut refused = value.clone();
            refused["shape_version"] = serde_json::json!(4);
            refused
        },
        {
            let mut refused = value.clone();
            refused["shape_version"] = serde_json::json!(1);
            refused
        },
        {
            let mut refused = value.clone();
            refused["shape_version"] = serde_json::json!("2");
            refused
        },
        {
            let mut refused = value.clone();
            refused["shape_version"] = serde_json::Value::Null;
            refused
        },
        {
            let mut refused = value.clone();
            refused["unknown_field"] = serde_json::json!(1);
            refused
        },
        {
            let mut refused = value.clone();
            refused.as_object_mut().unwrap().remove("trust");
            refused
        },
    ] {
        assert!(
            PersistedProfileRecord::from_json(&refused.to_string()).is_err(),
            "record layout must fail closed: {refused}"
        );
    }
    // Tampered identities parse but never match their capture, so restoration is refused.
    for tampered in ["permission_value", "semantic_state"] {
        let mut forged = value.clone();
        forged[tampered] = serde_json::json!("0".repeat(64));
        let forged = PersistedProfileRecord::from_json(&forged.to_string()).unwrap();
        assert!(!forged.matches_state(&captured));
        assert!(
            ExecutionProfileCatalog::from_persisted_records(
                vec![(forged, captured.clone())],
                std::slice::from_ref(&record),
            )
            .is_err()
        );
    }
}

/// A mixed v1/v2 catalog admits each generation by its own rules and never mixes proofs (T35B).
#[test]
fn mixed_version_catalog_selects_one_complete_template() {
    let readonly = fixture_state(FIXTURE_READONLY_V1);
    let write = fixture_state(FIXTURE_WORKSPACE_WRITE_V1);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        // A v2 template over the read-only capture.
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-readonly", 1, &readonly)
            .unwrap(),
        // A v1 template over the workspace-write capture.
        ExecutionProfileTemplate::from_execution_evidence("legacy-write", 1, &write).unwrap(),
    ])
    .unwrap();
    // Each generation admits its own state; the v2 side also admits the equivalent live
    // read-only state.
    let live_readonly = fixture_state(FIXTURE_READONLY_V1);
    let live_728 = fixture_state(FIXTURE_728);
    assert!(catalog.permit(&live_readonly, live_readonly.cwd()).is_ok());
    assert!(catalog.permit(&live_728, live_728.cwd()).is_ok());
    assert!(catalog.permit(&write, write.cwd()).is_ok());
    // A drifted workspace-write variant matches neither the v1 digest nor any v2 proof and
    // refuses through the closed shape reason, never a looser comparison.
    let mut drifted =
        serde_json::from_str::<serde_json::Value>(&sandbox_fixture(FIXTURE_WORKSPACE_WRITE_V1))
            .unwrap();
    drifted["permissionProfile"]["network"] = serde_json::json!("enabled");
    let drifted = HostSandboxState::parse(Some(drifted)).unwrap();
    assert!(matches!(
        catalog.permit(&drifted, drifted.cwd()),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Duplicate identities are refused within one generation.
    assert!(
        ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence_v2("t35b-readonly", 1, &readonly)
                .unwrap(),
            ExecutionProfileTemplate::from_execution_evidence_v2(
                "t35b-readonly-again",
                2,
                &readonly
            )
            .unwrap(),
        ])
        .is_err()
    );
}

/// Permit selection prefers an exact v2 shape, then a v1 exact digest, then a deterministic
/// narrower proof (T35B §4 decision order).
#[test]
fn permit_selection_is_exact_first_and_deterministic() {
    // `1ed…` and `555…` normalize to the same v2 shape, so a v2 template over either is an
    // exact match for both; an exact v2 shape beats a v1 digest match.
    let v2_template = fixture_state(FIXTURE_1ED);
    let v1_template = fixture_state(FIXTURE_555);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("v2-template", 1, &v2_template)
            .unwrap(),
        ExecutionProfileTemplate::from_execution_evidence("v1-template", 1, &v1_template).unwrap(),
    ])
    .unwrap();
    let live_555 = fixture_state(FIXTURE_555);
    assert_eq!(
        catalog
            .permit(&live_555, live_555.cwd())
            .unwrap()
            .template
            .id,
        "v2-template",
        "an exact v2 shape match is preferred over a v1 digest match"
    );
    // A v1 exact digest beats a merely-narrower v2 proof.
    let write = fixture_state(FIXTURE_WORKSPACE_WRITE_V1);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("v2-template", 1, &v2_template)
            .unwrap(),
        ExecutionProfileTemplate::from_execution_evidence("v1-write", 1, &write).unwrap(),
    ])
    .unwrap();
    assert_eq!(
        catalog.permit(&write, write.cwd()).unwrap().template.id,
        "v1-write",
        "a v1 exact digest beats a v2 narrowing proof"
    );
    // With only narrower proofs left, selection is deterministic by template identity.
    let mut extra_deny =
        serde_json::from_str::<serde_json::Value>(&sandbox_fixture(FIXTURE_1ED)).unwrap();
    extra_deny["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(
            {"access":"deny","path":{"path":"/Users/pluto/.t35b-extra-secret","type":"path"}}
        ));
    let mut fewer_denies =
        serde_json::from_str::<serde_json::Value>(&sandbox_fixture(FIXTURE_1ED)).unwrap();
    fewer_denies["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap()
        .retain(|entry| entry["access"] != "deny" || entry["path"]["type"] != "path");
    let live = HostSandboxState::parse(Some(extra_deny)).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("b-template", 1, &v2_template)
            .unwrap(),
        ExecutionProfileTemplate::from_execution_evidence_v2(
            "a-template",
            1,
            &HostSandboxState::parse(Some(fewer_denies)).unwrap(),
        )
        .unwrap(),
    ])
    .unwrap();
    assert_eq!(
        catalog.permit(&live, live.cwd()).unwrap().template.id,
        "a-template",
        "equal narrower proofs select the lowest template identity"
    );
}
/// A path deny and a same-text glob deny are never one identity (T35B-r finding 1).
#[test]
fn glob_and_path_denies_are_distinct_identities() {
    // The same deny text under both spellings produces different shape digests: the glob keeps
    // its tag, and a wildcard-free glob is never collapsed into a path deny.
    let shape = |deny: serde_json::Value| {
        let mut value = t35b_base_state();
        value["permissionProfile"]["file_system"]["entries"][5]["path"] = deny;
        shape_of(value).unwrap().digest()
    };
    assert_ne!(
        shape(serde_json::json!({"type":"path","path":"/Users/pluto/.aws"})),
        shape(serde_json::json!({"type":"glob_pattern","pattern":"/Users/pluto/.aws"})),
        "a path deny and a same-text glob deny must digest differently"
    );
    // A backslash is a literal path byte but a glob escape (T35B-r2): any path, cwd, or glob
    // base carrying one refuses derivation outright instead of being normalized, so a relocated
    // cwd can never turn an escaped glob into a different matcher under the same digest.
    for raw in ["/Users/pluto/work\\dir", "/Users/pluto/work\\*"] {
        let mut value = t35b_base_state();
        value["permissionProfile"]["file_system"]["entries"][5]["path"] =
            serde_json::json!({"type":"path","path":raw});
        assert!(shape_of(value).is_err(), "path {raw:?} must refuse");
        let mut value = t35b_base_state();
        value["permissionProfile"]["file_system"]["entries"][5]["path"] =
            serde_json::json!({"type":"glob_pattern","pattern":raw});
        assert!(shape_of(value).is_err(), "glob {raw:?} must refuse");
    }
    let mut relocated = t35b_base_state();
    relocated["sandboxCwd"] = serde_json::json!("file:///work\\dir");
    let state = HostSandboxState::parse(Some(relocated)).unwrap();
    assert!(
        state.shape_v2(Path::new("/work\\dir")).is_err(),
        "a backslash cwd must refuse even when it equals the trusted candidate"
    );
    // The derived glob rule keeps the glob tag and the byte-exact pattern remainder.
    let mut globbed = t35b_base_state();
    globbed["permissionProfile"]["file_system"]["entries"][5]["path"] =
        serde_json::json!({"type":"glob_pattern","pattern":"/Users/pluto/.aws"});
    let state = HostSandboxState::parse(Some(globbed)).unwrap();
    let shape = state.shape_v2(state.cwd()).unwrap();
    assert!(shape.denies.iter().any(|deny| matches!(
        deny,
        profile_shape::DenyRule::Glob { base, pattern }
            if *base == profile_shape::Selector::Absolute(vec!["Users".into(), "pluto".into(), ".aws".into()])
                && pattern.is_empty()
    )));
    // Swapping the accepted path deny for the same-text glob deny is not admission: the
    // accepted required rule is missing, whatever else the live state carries.
    assert!(matches!(
        t35b_permit_outcome(|state| {
            state["permissionProfile"]["file_system"]["entries"][5]["path"] =
                serde_json::json!({"type":"glob_pattern","pattern":"/Users/pluto/.aws"});
        }),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
}

/// A non-null `codexLinuxSandboxExe` refuses v2 derivation entirely; v1 is unchanged (T35B-r
/// finding 4). The accepted helper string is evidence of a path, never a pin on the executable's
/// identity, so a replaced helper can never slip through a v2 admission.
#[test]
fn non_null_helper_refuses_v2_derivation_but_keeps_v1() {
    for helper in ["/trusted/helper", "/replaced/helper"] {
        let mut state = t35b_base_state();
        state["codexLinuxSandboxExe"] = serde_json::json!(helper);
        assert_eq!(
            shape_of(state.clone()).unwrap_err(),
            UnsupportedShape("codexLinuxSandboxExe helper"),
            "a non-null helper must refuse v2 derivation"
        );
        // The permit decision names the typed unsupported reason; it never falls back.
        let parsed = HostSandboxState::parse(Some(state.clone())).unwrap();
        let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence_v2(
                "t35b-base",
                1,
                &HostSandboxState::parse(Some(t35b_base_state())).unwrap(),
            )
            .unwrap(),
        ])
        .unwrap();
        assert!(matches!(
            catalog.permit(&parsed, parsed.cwd()),
            Err(RequestError::ExecutionProfileShapeUnsupported(_))
        ));
        // Legacy v1 behaviour is unchanged: the exact helper state admits by its digest.
        let v1 = ExecutionProfileCatalog::from_execution_evidence(vec![
            ExecutionProfileTemplate::from_execution_evidence("legacy-v1", 1, &parsed).unwrap(),
        ])
        .unwrap();
        assert!(v1.permit(&parsed, parsed.cwd()).is_ok());
    }
}

/// The RAW `sandboxCwd` string is validated with the full grammar before any `file://` stripping
/// (T35B-r finding 5): a fragment, query, host part, or percent-encoding in the original
/// spelling is refused — a host part or percent-encoding already refuses at the opaque parse
/// layer, and a fragment or query, which that layer used to strip into an ordinary path
/// character, now refuses v2 derivation itself.
#[test]
fn raw_cwd_grammar_refuses_uri_variants() {
    // Refused before derivation: the opaque cwd parser rejects these spellings outright.
    for parse_refused in ["file://host/work", "file:///work%2Fx"] {
        let mut state = t35b_base_state();
        state["sandboxCwd"] = serde_json::json!(parse_refused);
        assert_eq!(
            HostSandboxState::parse(Some(state)).unwrap_err(),
            SandboxStateError::UnsupportedCwd,
            "the opaque parser must refuse {parse_refused:?}"
        );
    }
    // Refused by the v2 derivation: the raw spelling carries a query or fragment the legacy
    // parser used to strip away, and derivation validates the raw string instead.
    for derive_refused in ["file:///work#fragment", "file:///work?x"] {
        let mut state = t35b_base_state();
        state["sandboxCwd"] = serde_json::json!(derive_refused);
        let state = HostSandboxState::parse(Some(state)).unwrap();
        assert!(
            state.shape_v2(state.cwd()).is_err(),
            "derivation must refuse the raw cwd spelling {derive_refused:?}"
        );
    }
    // The supported local spellings still derive.
    for accepted in ["/private/tmp/t35b/work", "file:///private/tmp/t35b/work"] {
        let mut state = t35b_base_state();
        state["sandboxCwd"] = serde_json::json!(accepted);
        assert!(shape_of(state).is_ok(), "{accepted:?} must derive");
    }
}

/// The portable cwd is bound to the trusted candidate/Workspace worktree (T35B-r finding 3).
///
/// Relocating the cwd grant beneath an absolute path deny refuses derivation outright — exactly
/// the reviewed attack where the shape used to stay identical while the grant moved beneath a
/// retained deny — and a permit whose trusted candidate is not the state's own `sandboxCwd`
/// never derives a v2 shape at all. The relocation target, the deny's subtree, and the foreign
/// candidate are real directories: the binding compares them through the filesystem (T37B).
#[cfg(unix)]
#[test]
fn cwd_binding_refuses_relocation_under_an_absolute_deny_and_a_foreign_candidate() {
    // The reviewed attack: relocate cwd, its write, its nested read, and its glob denies onto
    // one real directory while retaining an absolute deny of that directory's parent. Every
    // cwd-derived selector normalizes identically, so the shape digests used to match;
    // derivation must refuse because the cwd now sits inside an absolute deny's subtree.
    let base = std::env::temp_dir().join(format!(
        "t35b-relocate-{}-{}",
        std::process::id(),
        blake3::hash(b"t35b-relocate").to_hex()
    ));
    let denied = base.join("denied");
    let relocated_dir = denied.join("project");
    std::fs::create_dir_all(&relocated_dir).unwrap();
    let foreign_dir = base.join("other");
    std::fs::create_dir_all(&foreign_dir).unwrap();
    // The entries are spelled the way the host emits them: canonically (T37B probe evidence).
    let denied = std::fs::canonicalize(&denied).unwrap();
    let relocated_dir = std::fs::canonicalize(&relocated_dir).unwrap();
    let relocated_under_deny = |mutation: fn(&mut serde_json::Value)| {
        let mut value = t35b_base_state();
        value["sandboxCwd"] = serde_json::json!(relocated_dir.to_string_lossy().as_ref());
        let entries = value["permissionProfile"]["file_system"]["entries"]
            .as_array_mut()
            .unwrap();
        entries[1]["path"]["path"] = serde_json::json!(relocated_dir.to_string_lossy().as_ref());
        entries[4]["path"]["path"] =
            serde_json::json!(relocated_dir.join(".git").to_string_lossy().as_ref());
        entries[6]["path"]["pattern"] = serde_json::json!(format!(
            "{}/.env",
            relocated_dir.join("**").to_string_lossy()
        ));
        entries.push(serde_json::json!(
            {"access":"deny","path":{"path":denied.to_string_lossy().as_ref(),"type":"path"}}
        ));
        mutation(&mut value);
        HostSandboxState::parse(Some(value)).unwrap()
    };
    let relocated = relocated_under_deny(|_| {});
    // Positive control over the untrusted parse path is the binding check itself: the same
    // relocated state derived against its own cwd refuses on the deny overlap...
    assert_eq!(
        relocated.shape_v2(relocated.cwd()).unwrap_err(),
        UnsupportedShape("cwd overlap")
    );
    // ...and the pristine base state derived against a foreign real candidate refuses on
    // binding.
    let base_state = HostSandboxState::parse(Some(t35b_base_state())).unwrap();
    assert_eq!(
        base_state.shape_v2(&foreign_dir).unwrap_err(),
        UnsupportedShape("cwd binding")
    );
    // Through the public permit decision both arrive as the typed unsupported reason, never a
    // silent admission.
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-base", 1, &base_state).unwrap(),
    ])
    .unwrap();
    assert!(matches!(
        catalog.permit(&relocated, relocated.cwd()),
        Err(RequestError::ExecutionProfileShapeUnsupported(_))
    ));
    assert!(matches!(
        catalog.permit(&base_state, &foreign_dir),
        Err(RequestError::ExecutionProfileShapeUnsupported(_))
    ));
    // Relocation to a different trusted candidate with no deny overlap stays admitted: that is
    // the portability the fixtures require, now with the candidate checked.
    let moved_dir = base.join("work2");
    std::fs::create_dir_all(&moved_dir).unwrap();
    let moved_dir = std::fs::canonicalize(&moved_dir).unwrap();
    let mut moved = t35b_base_state();
    moved["sandboxCwd"] = serde_json::json!(moved_dir.to_string_lossy().as_ref());
    let entries = moved["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap();
    entries[1]["path"]["path"] = serde_json::json!(moved_dir.to_string_lossy().as_ref());
    entries[4]["path"]["path"] =
        serde_json::json!(moved_dir.join(".git").to_string_lossy().as_ref());
    entries[6]["path"]["pattern"] =
        serde_json::json!(format!("{}/.env", moved_dir.join("**").to_string_lossy()));
    let moved = HostSandboxState::parse(Some(moved)).unwrap();
    let template = HostSandboxState::parse(Some(t35b_base_state())).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2("t35b-base", 1, &template).unwrap(),
    ])
    .unwrap();
    assert!(catalog.permit(&moved, moved.cwd()).is_ok());
    let _ = std::fs::remove_dir_all(&base);
}

/// Two spellings of one real directory bind to one directory (T35B-r findings 3 and 6, T37B):
/// the binding canonicalizes both sides through the filesystem, so the symlinked `/var/folders`
/// style spelling and its canonical `/private/var/folders` form derive against each other,
/// while a different real directory and an unresolvable candidate still refuse.
#[cfg(unix)]
#[test]
fn symlinked_cwd_alias_and_canonical_spelling_bind_to_one_real_directory() {
    let base = std::env::temp_dir().join(format!(
        "t35b-symlink-{}-{}",
        std::process::id(),
        blake3::hash(b"t35b-symlink").to_hex()
    ));
    let real = base.join("real");
    std::fs::create_dir_all(&real).unwrap();
    let alias = base.join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    let escape = base.join("escape");
    std::fs::create_dir_all(&escape).unwrap();
    let mut state = t35b_base_state();
    state["sandboxCwd"] = serde_json::json!(real.to_string_lossy().as_ref());
    let entries = state["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap();
    entries[1]["path"]["path"] = serde_json::json!(real.to_string_lossy().as_ref());
    entries[4]["path"]["path"] = serde_json::json!(real.join(".git").to_string_lossy().as_ref());
    entries[6]["path"]["pattern"] =
        serde_json::json!(format!("{}/.env", real.join("**").to_string_lossy()));
    let state = HostSandboxState::parse(Some(state)).unwrap();
    // The real directory and its symlink alias are one directory: both derive.
    assert!(state.shape_v2(&real).is_ok());
    assert!(state.shape_v2(&alias).is_ok());
    // A different real directory — a sibling outside the granted worktree — still refuses.
    assert_eq!(
        state.shape_v2(&escape).unwrap_err(),
        UnsupportedShape("cwd binding")
    );
    // A candidate that cannot be canonicalized refuses fail-closed.
    assert_eq!(
        state.shape_v2(&base.join("missing")).unwrap_err(),
        UnsupportedShape("trusted cwd canonicalization")
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// `glob_scan_max_depth` presence and value are essential: changing the expansion depth can
/// change which restrictions take effect, so any change refuses (T35B-r finding 6).
#[test]
fn glob_scan_max_depth_changes_refuse() {
    let depth = |value: Option<u64>| {
        let mut template = t35b_base_state();
        match value {
            None => {
                template["permissionProfile"]["file_system"]
                    .as_object_mut()
                    .unwrap()
                    .remove("glob_scan_max_depth");
            }
            Some(depth) => {
                template["permissionProfile"]["file_system"]["glob_scan_max_depth"] =
                    serde_json::json!(depth);
            }
        }
        template
    };
    // A different depth or a dropped depth refuses against the accepted depth-8 capture.
    for live in [depth(Some(9)), depth(None)] {
        assert!(matches!(
            permit_values(depth(Some(8)), live),
            Err(RequestError::ExecutionProfileShapeNotNarrower(_))
        ));
    }
    // An added depth refuses against an accepted depth-free capture.
    assert!(matches!(
        permit_values(depth(None), depth(Some(8))),
        Err(RequestError::ExecutionProfileShapeNotNarrower(_))
    ));
    // Presence and value are part of the shape identity itself.
    assert_ne!(
        shape_of(depth(Some(8))).unwrap().digest(),
        shape_of(depth(Some(9))).unwrap().digest()
    );
    assert_ne!(
        shape_of(depth(Some(8))).unwrap().digest(),
        shape_of(depth(None)).unwrap().digest()
    );
}

/// A genuine competing-template selection: a v1-exact and a v2-narrower template over
/// compatible selector sets both admit the live state, and the documented order — exact v2
/// shape, then v1 digest, then deterministic v2 narrowing — is what selects (T35B-r finding 6).
#[test]
fn v1_exact_and_v2_narrower_templates_compete_over_compatible_selectors() {
    // Live = the capture plus one more deny. The v1 template carries exactly this state; the
    // v2 template carries the deny-free ceiling, so the live state is a genuine *narrower*
    // match for it — same non-deny selectors, every accepted deny present, one extra deny.
    let mut live_value = sandbox_fixture(FIXTURE_1ED);
    live_value = live_value.replace(
        "\"entries\":[",
        "\"entries\":[{\"access\":\"deny\",\"path\":{\"path\":\"/Users/pluto/.t35b-extra-secret\",\"type\":\"path\"}},",
    );
    let live = HostSandboxState::parse_json(&live_value).unwrap();
    let v1 = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("v1-template", 1, &live).unwrap(),
        ExecutionProfileTemplate::from_execution_evidence_v2(
            "v2-template",
            1,
            &fixture_state(FIXTURE_1ED),
        )
        .unwrap(),
    ])
    .unwrap();
    // Each candidate alone admits: the v1 by exact digest, the v2 by a genuine narrowing proof
    // (never an exact shape match — the live state carries the extra deny).
    assert!(v1.permit(&live, live.cwd()).is_ok());
    let v2_only = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v2(
            "v2-template",
            1,
            &fixture_state(FIXTURE_1ED),
        )
        .unwrap(),
    ])
    .unwrap();
    assert!(v2_only.permit(&live, live.cwd()).is_ok());
    // Competing together over these compatible selector sets, the v1 exact digest wins.
    assert_eq!(
        v1.permit(&live, live.cwd()).unwrap().template.id,
        "v1-template",
        "a v1 exact digest beats a genuine v2 narrowing proof"
    );
}

// ---------------------------------------------------------------------------
// T36B: the conservative per-path native-read proof. Synthetic states below
// carry the minimal entries each rule needs; the six captured write fixtures
// seal the same decisions on real host captures.
// ---------------------------------------------------------------------------

/// Builds one minimal managed restricted state with the given cwd and filesystem entries.
fn proof_state(cwd: &str, entries: serde_json::Value) -> HostSandboxState {
    ensure_real_dir(cwd);
    HostSandboxState::parse(Some(serde_json::json!({
        "permissionProfile":{"type":"managed","file_system":{"entries":entries,"type":"restricted"},"network":"restricted"},
        "codexLinuxSandboxExe":null,"sandboxCwd":cwd,"useLegacyLandlock":false
    })))
    .unwrap()
}

/// Derives the shape of a synthetic state bound to its own cwd and runs the read proof.
fn proof_of(
    cwd: &str,
    entries: serde_json::Value,
    relative: &str,
) -> super::profile_shape::ReadProof {
    let state = proof_state(cwd, entries);
    let shape = state.shape_v2(state.cwd()).unwrap();
    super::profile_shape::read_proof(&shape, state.cwd(), Path::new(relative))
}

/// The `read`-of-root grant that stands in for whole-tree coverage in synthetic states.
fn read_root() -> serde_json::Value {
    serde_json::json!({"access":"read","path":{"type":"special","value":{"kind":"root"}}})
}

const PROOF_CWD: &str = "/private/tmp/t36b-work";

#[test]
fn t36b_path_grammar_refuses_every_non_normal_relative_path() {
    for relative in [
        "",            // empty
        "/etc/passwd", // absolute
        "..",          // parent
        "a/../b",      // embedded parent
        "a/./b",       // embedded dot
        "a//b",        // empty component
        "a\\b",        // backslash
        "a/\0b",       // NUL byte — matches any byte after the slash, so it stays refused
    ] {
        assert_eq!(
            proof_of(PROOF_CWD, serde_json::json!([read_root()]), relative),
            super::profile_shape::ReadProof::Unproven,
            "{relative:?} must be unprovable"
        );
    }
}

#[test]
fn t36b_positive_coverage_requires_root_or_prefixing_workspace_relative_absent_grant() {
    let write_cwd = serde_json::json!([
        {"access":"write","path":{"path":PROOF_CWD,"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, write_cwd, "src/main.py"),
        super::profile_shape::ReadProof::Proven,
        "a cwd write selector prefixes the target and covers it"
    );
    let write_subdir = serde_json::json!([
        {"access":"write","path":{"path":format!("{PROOF_CWD}/src"),"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, write_subdir.clone(), "src/main.py"),
        super::profile_shape::ReadProof::Proven,
        "a nested prefixing selector covers the target"
    );
    assert_eq!(
        proof_of(PROOF_CWD, write_subdir, "tests/main.py"),
        super::profile_shape::ReadProof::Unproven,
        "a selector that does not prefix the target covers nothing"
    );
    // A skipped grant is not discarded as noise: an absent skipped grant cannot establish
    // authority, so only the root grant below carries the coverage decision.
    let skipped_only = serde_json::json!([
        {"access":"read","path":{"path":format!("{PROOF_CWD}/src"),"type":"path"},"missing_path_behavior":"skip"}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, skipped_only, "src/main.py"),
        super::profile_shape::ReadProof::Unproven,
        "a MissingPath::Skip grant does not establish authority"
    );
    let skipped_plus_root = serde_json::json!([
        {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
        {"access":"read","path":{"path":format!("{PROOF_CWD}/src"),"type":"path"},"missing_path_behavior":"skip"}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, skipped_plus_root, "src/main.py"),
        super::profile_shape::ReadProof::Proven,
        "coverage comes from the present root grant, not the skipped one"
    );
}

#[test]
fn t36b_binding_refuses_a_relocated_cwd() {
    // The cwd never enters the serialized shape, so binding safety comes from derivation:
    // deriving against a trusted cwd that is not the state's own `sandboxCwd` refuses, which
    // is exactly how `validate_workspace_read` binds the live shape to the authoritative root.
    let state = proof_state(
        PROOF_CWD,
        serde_json::json!([{"access":"write","path":{"path":PROOF_CWD,"type":"path"}}]),
    );
    assert!(state.shape_v2(state.cwd()).is_ok());
    assert!(state.shape_v2(Path::new("/private/tmp/elsewhere")).is_err());
    let shape = state.shape_v2(state.cwd()).unwrap();
    assert_eq!(
        super::profile_shape::read_proof(&shape, state.cwd(), Path::new("src/main.py")),
        super::profile_shape::ReadProof::Proven
    );
}

#[test]
fn t36b_path_denies_refuse_ancestors_and_equality() {
    let entries = serde_json::json!([
        {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
        {"access":"deny","path":{"path":format!("{PROOF_CWD}/certs/x.key"),"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, entries.clone(), "certs/x.key"),
        super::profile_shape::ReadProof::Unproven,
        "an exact denied file is unproven"
    );
    let dir_deny = serde_json::json!([
        read_root(),
        {"access":"deny","path":{"path":format!("{PROOF_CWD}/certs"),"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, dir_deny, "certs/x.key"),
        super::profile_shape::ReadProof::Unproven,
        "an ancestor deny refuses its descendants"
    );
    // A deny below the target is not an ancestor deny: reading the parent directory itself
    // is decided by grants and other denies only (no precedence solving in either direction).
    assert_eq!(
        proof_of(PROOF_CWD, entries.clone(), "certs"),
        super::profile_shape::ReadProof::Proven
    );
    // ASCII case-folded path denies count as possible matches: a casing variant stays denied.
    assert_eq!(
        proof_of(PROOF_CWD, entries.clone(), "CERTS/X.KEY"),
        super::profile_shape::ReadProof::Unproven
    );
    // Grants are never folded: coverage was decided on exact components before this point.
    assert_eq!(
        proof_of(PROOF_CWD, entries, "certs/other.key"),
        super::profile_shape::ReadProof::Proven
    );
}

#[test]
fn t36b_glob_denies_stay_conservative_per_the_design_matrix() {
    let with_glob = |pattern: &str| {
        serde_json::json!([
            read_root(),
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}{pattern}"),"type":"glob_pattern"}}
        ])
    };
    // `**/.env` against `.env.local` is a nonmatch — but other denies still apply, and here
    // none do, so the file proves.
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/.env"), ".env.local"),
        super::profile_shape::ReadProof::Proven
    );
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/**/.env"), ".env"),
        super::profile_shape::ReadProof::Unproven,
        "the exact denied file matches `**/.env`"
    );
    // `*.key` against `x.key/child`: the ancestor directory matches, so the child is unproven.
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/*.key"), "x.key/child"),
        super::profile_shape::ReadProof::Unproven
    );
    // A case variant of a denied name stays denied under conservative ASCII folding.
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/**/.env"), ".ENV"),
        super::profile_shape::ReadProof::Unproven
    );
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/*.KEY"), "x.key"),
        super::profile_shape::ReadProof::Unproven
    );
    // A relevant glob containing `[` (or any unsupported syntax) is never declared irrelevant.
    assert_eq!(
        proof_of(PROOF_CWD, with_glob("/**/x[0]"), "src/main.py"),
        super::profile_shape::ReadProof::Unproven
    );
    // A provably disjoint base is irrelevant and never refuses on its own.
    let disjoint = serde_json::json!([
        read_root(),
        {"access":"deny","path":{"pattern":"/private/tmp/other-work/**/*.key","type":"glob_pattern"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, disjoint, "src/main.py"),
        super::profile_shape::ReadProof::Proven
    );
}

#[test]
fn t36b_relevant_glob_depth_beyond_a_present_cap_is_unproven() {
    let deep = "a/b/c/d/e/f/g/h/i.txt";
    let shallow = "a/b.txt";
    assert_eq!(deep.split('/').count(), 9, "nine components below the base");
    assert_eq!(shallow.split('/').count(), 2);
    // The glob `/**/*.key` is relevant to both targets, yet neither matches it.
    let with_cap = |depth: serde_json::Value| {
        let mut raw = serde_json::json!({
            "permissionProfile":{"type":"managed","file_system":{"entries":[
                {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
                {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/**/*.key"),"type":"glob_pattern"}}
            ],"type":"restricted"},"network":"restricted"},
            "codexLinuxSandboxExe":null,"sandboxCwd":PROOF_CWD,"useLegacyLandlock":false
        });
        if !depth.is_null() {
            raw["permissionProfile"]["file_system"]["glob_scan_max_depth"] = depth;
        }
        HostSandboxState::parse(Some(raw)).unwrap()
    };
    let prove = |state: &HostSandboxState, relative: &str| {
        let shape = super::profile_shape::ProfileShapeV2::derive(state, state.cwd()).unwrap();
        super::profile_shape::read_proof(&shape, state.cwd(), Path::new(relative))
    };
    // Depth nine exceeds the captured cap of eight: the glob may expand differently, so the
    // read refuses. The limit never declares the glob irrelevant.
    assert_eq!(
        prove(&with_cap(serde_json::json!(8)), deep),
        super::profile_shape::ReadProof::Unproven
    );
    // Two components sit below the cap, and the glob provably cannot match anyway.
    assert_eq!(
        prove(&with_cap(serde_json::json!(8)), shallow),
        super::profile_shape::ReadProof::Proven
    );
    assert_eq!(
        prove(&with_cap(serde_json::json!(16)), deep),
        super::profile_shape::ReadProof::Proven,
        "a higher cap admits the deep path once the glob cannot match"
    );
    // An absent cap never depth-refuses.
    assert_eq!(
        prove(&with_cap(serde_json::Value::Null), deep),
        super::profile_shape::ReadProof::Proven
    );
}

/// Proves the per-path read proof never declares a non-ASCII deny path, deny-glob base or
/// target disjoint from another name, because Unicode normalization can make two different
/// byte spellings name the same file (T36B-r, review finding 2).
#[test]
fn t36b_non_ascii_denies_and_targets_are_never_provably_disjoint() {
    // NFC folds the Kelvin sign `K` (U+212A) onto ASCII `K`, and a filesystem may store one
    // spelling under the other's normalization form, so a byte-wise disjoint answer is never
    // sound: every non-ASCII deny path, glob base, or target leaves the proof Unproven
    // (T36B-r, review finding 2). `composed` and `decomposed` are the two normalization
    // forms of the same name "Kéy"; the decomposed form even starts with an ASCII byte.
    let composed = "K\u{e9}y";
    let decomposed = "Ke\u{301}y";
    let kelvin = "K\u{212a}ey";
    assert_ne!(composed.as_bytes(), decomposed.as_bytes());
    // A non-ASCII path deny (composed, decomposed, or Kelvin-signed) is ambiguous against
    // every ASCII target under this cwd — exactly the reviewed counterexample.
    for denied in [composed, decomposed, kelvin] {
        let entries = serde_json::json!([
            read_root(),
            {"access":"deny","path":{"path":format!("{PROOF_CWD}/{denied}"),"type":"path"}}
        ]);
        assert_eq!(
            proof_of(PROOF_CWD, entries, "Key"),
            super::profile_shape::ReadProof::Unproven,
            "non-ASCII path deny {denied:?} must not be declared disjoint from \"Key\""
        );
    }
    // The mirror direction: an ASCII deny stays ambiguous against a non-ASCII target, so the
    // target refuses even though its first byte matches no deny byte.
    let ascii_deny = serde_json::json!([
        read_root(),
        {"access":"deny","path":{"path":format!("{PROOF_CWD}/Key"),"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, ascii_deny, decomposed),
        super::profile_shape::ReadProof::Unproven,
        "a non-ASCII target is never proven under a cwd with any deny"
    );
    // The same rule for glob bases: a wildcard-free glob whose base carries non-ASCII bytes
    // refuses every target under the cwd instead of being cleared as disjoint.
    for base in [composed, decomposed, kelvin] {
        let glob = serde_json::json!([
            read_root(),
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/{base}"),"type":"glob_pattern"}}
        ]);
        for target in ["Key", "other.txt", "src/main.py"] {
            assert_eq!(
                proof_of(PROOF_CWD, glob.clone(), target),
                super::profile_shape::ReadProof::Unproven,
                "non-ASCII glob base {base:?} must stay ambiguous for {target:?}"
            );
        }
    }
    // A relevant glob deny against a non-ASCII target cannot prove a nonmatch either: the
    // matcher reports Unknown, and Unknown never clears a deny.
    let ascii_glob = serde_json::json!([
        read_root(),
        {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/*.key"),"type":"glob_pattern"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, ascii_glob, decomposed),
        super::profile_shape::ReadProof::Unproven
    );
    // Positive control: an all-ASCII deny set against an all-ASCII target still proves.
    let ascii_pair = serde_json::json!([
        read_root(),
        {"access":"deny","path":{"path":format!("{PROOF_CWD}/Key"),"type":"path"}}
    ]);
    assert_eq!(
        proof_of(PROOF_CWD, ascii_pair, "other.txt"),
        super::profile_shape::ReadProof::Proven
    );
}

#[test]
fn t36b_write_fixtures_prove_main_refuse_env_and_key() {
    for name in [
        FIXTURE_1ED,
        FIXTURE_555,
        FIXTURE_B8A,
        FIXTURE_C9E,
        FIXTURE_FD3,
        FIXTURE_84F,
    ] {
        let state = fixture_state(name);
        let shape = state.shape_v2(state.cwd()).unwrap();
        let prove = |relative: &str| {
            super::profile_shape::read_proof(&shape, state.cwd(), Path::new(relative))
        };
        assert_eq!(
            prove("src/main.py"),
            super::profile_shape::ReadProof::Proven,
            "{name}: src/main.py must be proven"
        );
        assert_eq!(
            prove(".env"),
            super::profile_shape::ReadProof::Unproven,
            "{name}: .env must be unproven"
        );
        assert_eq!(
            prove("certs/x.key"),
            super::profile_shape::ReadProof::Unproven,
            "{name}: certs/x.key must be unproven"
        );
    }
}

// ---------------------------------------------------------------------------
// T37B: the live Codex 0.155.1 workspace-write state, captured by T25B from a
// real `codex exec -s workspace-write` session at a temporary-directory
// worktree. macOS `$TMPDIR` is the symlinked `/var/folders/...` spelling while
// every trusted product path is the canonical `/private/var/folders/...`
// spelling, so these tests drive the exact cwd spellings the acceptance route
// sees.
// ---------------------------------------------------------------------------

/// The 43 filesystem entries Codex 0.155.1 advertises for workspace-write at a
/// temporary worktree, exactly as T25B captured them; `<<CWD>>` marks the five
/// cwd-bound entries (the worktree write and the four credential globs).
fn t37b_live_entries() -> serde_json::Value {
    serde_json::json!([
        {"access":"read","path":{"path":"/Users/pluto/.agent-run","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.agent-run/accounts","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.aws","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.azure","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/.cache/uv","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.cargo/credentials","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.cargo/credentials.toml","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/.cargo/registry","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.codex/auth.json","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.config/gcloud","type":"path"}},
        {"access":"read","path":{"path":"/Users/pluto/.config/gh/hosts.yml","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.config/opencode/auth.json","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.docker/config.json","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.git-credentials","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.kube","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.local/share/opencode/auth.json","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.netrc","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/.npm","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.npmrc","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.pypirc","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/.ssh","type":"path"}},
        {"access":"read","path":{"path":"/Users/pluto/.ssh/known_hosts","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/Library/Caches/go-build","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/Library/Caches/pip","type":"path"}},
        {"access":"deny","path":{"path":"/Users/pluto/Library/Keychains","type":"path"}},
        {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
        {"access":"write","path":{"type":"special","value":{"kind":"slash_tmp"}}},
        {"access":"write","path":{"type":"special","value":{"kind":"tmpdir"}}},
        {"access":"deny","path":{"pattern":"<<CWD>>/**/*.key","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"<<CWD>>/**/*.pem","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"<<CWD>>/**/.env","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"<<CWD>>/**/.env.*","type":"glob_pattern"}},
        {"access":"write","path":{"path":"<<CWD>>","type":"path"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/.codex/worktrees/**/*.key","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/projects/**/*.key","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/.codex/worktrees/**/*.pem","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/projects/**/*.pem","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/.codex/worktrees/**/.env","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/projects/**/.env","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/.codex/worktrees/**/.env.*","type":"glob_pattern"}},
        {"access":"deny","path":{"pattern":"/Users/pluto/projects/**/.env.*","type":"glob_pattern"}},
        {"access":"write","path":{"path":"/Users/pluto/.codex/worktrees","type":"path"}},
        {"access":"write","path":{"path":"/Users/pluto/projects","type":"path"}}
    ])
}

/// Builds the live 0.155.1 state with every cwd-bound value spelled through `cwd`.
fn t37b_live_state(cwd: &str) -> serde_json::Value {
    let mut value = serde_json::json!({
        "permissionProfile":{"type":"managed","file_system":{
            "entries":t37b_live_entries(),"type":"restricted","glob_scan_max_depth":8},
            "network":"enabled"},
        "codexLinuxSandboxExe":null,
        "sandboxCwd":serde_json::Value::Null,
        "useLegacyLandlock":false
    });
    value["sandboxCwd"] = serde_json::json!(format!("file://{cwd}"));
    let entries = value["permissionProfile"]["file_system"]["entries"]
        .as_array_mut()
        .unwrap();
    for entry in entries {
        let spelled = serde_json::to_string(entry)
            .unwrap()
            .replace("<<CWD>>", cwd);
        *entry = serde_json::from_str(&spelled).unwrap();
    }
    value
}

/// Creates one real directory below the symlinked per-user temporary directory
/// plus its canonical spelling, mirroring the acceptance `$TMPDIR` worktrees.
#[cfg(unix)]
fn t37b_symlinked_worktree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use blake3::Hasher;
    let unique = {
        let mut hasher = Hasher::new();
        hasher.update(tag.as_bytes());
        hasher.update(&std::process::id().to_le_bytes());
        hasher.finalize().to_hex().to_string()[..12].to_owned()
    };
    let logical = std::env::temp_dir().join(format!("t37b-{tag}-{unique}"));
    std::fs::create_dir_all(&logical).unwrap();
    let canonical = std::fs::canonicalize(&logical).unwrap();
    (canonical, logical)
}

/// Pins the acceptance route's cwd comparison against the real 0.155.1 state (T37B).
///
/// Codex 0.155.1 emits `sandboxCwd` as `file://` plus the canonical physical
/// worktree path, while a path spelled through the symlinked `$TMPDIR`
/// (`/var/folders/...`) is the same real directory as its canonical
/// `/private/var/folders/...` spelling. The trusted candidate is canonical
/// (`current_dir`, git discovery), so:
///
/// 1. the state Codex really sends derives against the canonical candidate and
///    proves the acceptance diff path `acceptance-fixture/fixture.py` under
///    the live deny set — the whole l1b scenario must work;
/// 2. the same real directory spelled through the symlink alias binds to the
///    canonical trusted candidate too: realpath, not the raw spelling, is the
///    directory's identity, so a host that emits the logical temporary
///    directory form can never false-refuse every path as `path_unproven`.
#[cfg(unix)]
#[test]
fn t37b_live_codex_state_at_a_symlinked_tmp_worktree() {
    let (canonical, alias) = t37b_symlinked_worktree("state");
    // 1. The state exactly as the live probe captured it: canonical spelling.
    let live = HostSandboxState::parse(Some(t37b_live_state(canonical.to_str().unwrap()))).unwrap();
    let shape = live.shape_v2(&canonical).expect("live state must derive");
    assert_eq!(
        super::profile_shape::read_proof(
            &shape,
            &canonical,
            Path::new("acceptance-fixture/fixture.py")
        ),
        super::profile_shape::ReadProof::Proven,
        "the diff fixture path must prove under the real deny set"
    );
    // 2. The same real directory reached through the symlink alias spelling binds.
    let aliased = HostSandboxState::parse(Some(t37b_live_state(alias.to_str().unwrap()))).unwrap();
    let aliased_shape = aliased
        .shape_v2(&canonical)
        .expect("the alias spelling must bind to the same real directory");
    assert_eq!(
        super::profile_shape::read_proof(
            &aliased_shape,
            &canonical,
            Path::new("acceptance-fixture/fixture.py")
        ),
        super::profile_shape::ReadProof::Proven
    );
    let _ = std::fs::remove_dir_all(&canonical);
}

/// Every tracked path the acceptance fixture worktree reads natively stays within the live
/// profile's conservative glob reach (T37B).
///
/// The captured 0.155.1 workspace-write profile carries `glob_scan_max_depth: 8` and `**`
/// credential globs, so a tracked file more than eight components below the worktree root is
/// conservatively `Unproven` and the diff capture refuses — which is exactly how the
/// nine-and-ten-deep checker fixtures used to fail the l1b scenario three of three times. The
/// fixture tree keeps its deepest paths at eight components, and this pins the decision on the
/// real captured entries: the relocated acceptance paths prove, a ninth component refuses.
#[test]
fn t37b_deep_repo_fixture_paths_stay_within_the_live_glob_cap() {
    ensure_real_dir(PROOF_CWD);
    let state = HostSandboxState::parse(Some(serde_json::json!({
        "codexLinuxSandboxExe":null,
        "permissionProfile":{"type":"managed","file_system":{"entries":[
            {"access":"read","path":{"type":"special","value":{"kind":"root"}}},
            {"access":"write","path":{"path":PROOF_CWD,"type":"path"}},
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/**/*.key"),"type":"glob_pattern"}},
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/**/*.pem"),"type":"glob_pattern"}},
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/**/.env"),"type":"glob_pattern"}},
            {"access":"deny","path":{"pattern":format!("{PROOF_CWD}/**/.env.*"),"type":"glob_pattern"}}
        ],"type":"restricted","glob_scan_max_depth":8},"network":"restricted"},
        "sandboxCwd":PROOF_CWD,"useLegacyLandlock":false
    })))
    .unwrap();
    let shape = state.shape_v2(state.cwd()).unwrap();
    let prove =
        |relative: &str| super::profile_shape::read_proof(&shape, state.cwd(), Path::new(relative));
    // The acceptance fixture paths at their relocated (eight-component) depth.
    for relative in [
        "acceptance-fixture/fixture.py",
        "tests/fixtures/checks/errors/src/pkg/excluded/broken.py",
        "tests/fixtures/checks/interpreter_pyproject/env/myenv/bin/python",
        "tests/fixtures/checks/toolchain/pyright/node_modules/pyright/index.js",
    ] {
        assert_eq!(
            prove(relative),
            super::profile_shape::ReadProof::Proven,
            "{relative}"
        );
    }
    // A ninth component below the root exceeds the captured cap and stays refused.
    assert_eq!(
        prove("tests/fixtures/checks/interpreter_pyproject/env/myenv/deep/bin/python"),
        super::profile_shape::ReadProof::Unproven
    );
}
