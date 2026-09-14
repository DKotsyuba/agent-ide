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
    let process =
        build_command(&command, &sandbox, Path::new("/opt/codex/bin/codex"), None).unwrap();
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
