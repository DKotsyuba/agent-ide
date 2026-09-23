//! Real managed-sandbox D03 check driven by a captured host state and disposable targets.
//!
//! T35B note: the fixed expectations below (cwd write succeeds, `.git` and one outside write
//! fail, loopback networking fails) describe the *legacy workspace-write* profile only. A
//! root-write profile cannot pass "outside all write roots is denied", and a network-enabled
//! profile cannot reuse restricted-network evidence, so a reviewed profile-specific expectation
//! manifest (allowed cwd/outside/temp writes, carve-outs, network behavior, rule-order and
//! duplicate behavior) remains a documented follow-up. The focused v3 visualization probe
//! below runs only with explicit native fixture inputs; ordinary runs leave it ignored.

use agent_ide::{
    assistance::host_binding::{
        BindingStatus, HostBindingGuard, parse_candidate, parse_channel_session, parse_hook_event,
        parse_observed_sandbox_state,
    },
    execution,
};

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use execution::{
    Admission, AdmissionClass, AdmissionController, AdmissionLimits, CommandKind,
    ControlledCommand, DiscoverWorktreeRequest, DiscoveryOperationRef, ExecutionProfileCatalog,
    ExecutionProfileTemplate, GitDiscoveryPolicy, GitDiscoveryQuery, HostSandboxState,
    LocalExecutionPolicy, OwnedChild, OwnerId, ValidatedExecutionRequest, ValidatedHostInvocation,
    WorkspaceAuthority,
};

/// Returns one required D03 environment value or stops before any sandboxed child starts.
fn required(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("missing required D03 environment variable {name}"))
}

/// Allocates a path that this test alone may create or remove inside a caller-approved directory.
fn test_path(directory: &Path, label: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    directory.join(format!(
        "agent-ide-d03-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Creates a matching trusted candidate for the local real-discovery probe binding guard.
fn candidate(actor: &str, call: &str) -> agent_ide::assistance::host_binding::CandidateInvocation {
    parse_candidate(
        serde_json::json!({
            "threadId": actor,
            "callId": call,
            "x-codex-turn-metadata": {"turn": "d03"}
        })
        .as_object()
        .unwrap(),
    )
    .unwrap()
}

/// Creates the matching native pre-hook required for the local real-discovery probe binding guard.
fn pre_hook(actor: &str, call: &str) -> agent_ide::assistance::host_binding::HookEvent {
    parse_hook_event(
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": actor,
            "tool_use_id": call
        })
        .to_string()
        .as_bytes(),
    )
    .unwrap()
}

/// Builds one request from the exact captured state, fixed executable, and one controlled path operand.
fn request(
    state: &HostSandboxState,
    catalog: &ExecutionProfileCatalog,
    touch: &Path,
    target: PathBuf,
) -> ValidatedExecutionRequest {
    request_args(state, catalog, touch, vec![OsString::from(target)])
}

/// Builds one request from the exact captured state, fixed executable, and fully controlled argv.
fn request_args(
    state: &HostSandboxState,
    catalog: &ExecutionProfileCatalog,
    program: &Path,
    args: Vec<OsString>,
) -> ValidatedExecutionRequest {
    let invocation =
        ValidatedHostInvocation::from_verified_binding("d03-fixture", state.clone()).unwrap();
    let authority = WorkspaceAuthority::from_workspace(
        "d03-worktree",
        "d03-incarnation",
        state.cwd().to_path_buf(),
        1,
    )
    .unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Job,
        program.to_path_buf(),
        args,
        state.cwd().to_path_buf(),
        BTreeMap::new(),
    )
    .unwrap();
    let policy =
        LocalExecutionPolicy::new(BTreeSet::from([program.to_path_buf()]), 4096, 0, false).unwrap();
    ValidatedExecutionRequest::validate(invocation, authority, command, &policy, catalog).unwrap()
}

/// Runs one owned child and releases its slot only after direct-child reap evidence exists.
async fn run_child(
    request: &ValidatedExecutionRequest,
    codex: &Path,
    label: &str,
) -> execution::CapturedProcessEvidence {
    let mut admission = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        per_owner_queued: 1,
        total_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let lease = match admission.submit(OwnerId::new(label).unwrap(), AdmissionClass::Interactive) {
        Admission::Granted(lease) => lease,
        outcome => panic!("unexpected D03 admission result: {outcome:?}"),
    };
    let result = OwnedChild::spawn_captured(request, lease, None, codex, 8192)
        .unwrap()
        .reap(Duration::from_secs(60), Duration::from_secs(10))
        .await
        .unwrap();
    assert!(result.evidence.stdout().complete && result.evidence.stderr().complete);
    assert!(
        admission
            .release_reaped(result.settlement)
            .unwrap()
            .is_empty()
    );
    result.evidence
}

/// Proves the captured profile permits its fixture, refuses an approved outside target and `.git`, and reaps children.
#[tokio::test]
#[ignore = "requires AGENT_IDE_D03_STATE, AGENT_IDE_D03_DENIED_DIR, and AGENT_IDE_D03_CODEX"]
async fn captured_managed_profile_enforces_fixture_boundaries() {
    let state_text = fs::read_to_string(required("AGENT_IDE_D03_STATE")).unwrap();
    let state = HostSandboxState::parse_json(&state_text).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("captured-managed-d03", 1, &state)
            .unwrap(),
    ])
    .unwrap();
    let codex = PathBuf::from(required("AGENT_IDE_D03_CODEX"));
    let touch = Path::new("/usr/bin/touch");
    assert!(touch.is_file(), "D03 helper is not available at {touch:?}");
    let allowed = test_path(state.cwd(), "allowed");
    let denied = test_path(
        &PathBuf::from(required("AGENT_IDE_D03_DENIED_DIR")),
        "outside",
    );
    let denied_git = test_path(&state.cwd().join(".git"), "git");
    assert!(
        state.cwd().join(".git").is_dir(),
        "captured fixture has no .git directory"
    );

    let allowed_result = run_child(
        &request(&state, &catalog, touch, allowed.clone()),
        &codex,
        "allowed",
    )
    .await;
    assert!(
        allowed_result.status().success(),
        "allowed stderr: {:?}",
        allowed_result.stderr().bytes
    );
    assert!(allowed.is_file(), "allowed fixture write did not occur");
    fs::remove_file(&allowed).unwrap();

    for (label, target) in [("outside", denied), ("git", denied_git)] {
        let result = run_child(
            &request(&state, &catalog, touch, target.clone()),
            &codex,
            label,
        )
        .await;
        let unexpectedly_written = target.is_file();
        if unexpectedly_written {
            fs::remove_file(&target).unwrap();
        }
        assert!(
            !result.status().success() && !unexpectedly_written,
            "{label} write escaped the captured profile; stderr: {:?}",
            result.stderr().bytes
        );
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let netcat = Path::new("/usr/bin/nc");
    assert!(
        netcat.is_file(),
        "network probe helper is unavailable at {netcat:?}"
    );
    let network_result = run_child(
        &request_args(
            &state,
            &catalog,
            netcat,
            vec![
                OsString::from("-z"),
                OsString::from("-w"),
                OsString::from("1"),
                OsString::from("127.0.0.1"),
                OsString::from(port),
            ],
        ),
        &codex,
        "network",
    )
    .await;
    assert!(
        !network_result.status().success(),
        "network-restricted profile connected to the controlled listener"
    );
}

/// Placeholder for the reviewed, profile-specific expectation-manifest experiment (T35B note).
///
/// The manifest run must consume an explicit reviewed expectation file for the *accepted*
/// authority shape — allowed cwd/outside/temp writes against disposable targets, deny-protected
/// disposable files, read-only carve-outs, expected network behavior, and rule-order, duplicate,
/// missing-path, special-path, and symlink behavior — and replay it through the exact Codex
/// sandbox argv, so a profile whose expectations differ (root-write, network-enabled) cannot
/// reuse legacy evidence. It is named here so the gap stays visible in test listings and so the
/// follow-up lands as this test's body rather than as new undocumented evidence.
#[tokio::test]
#[ignore = "the reviewed profile-specific expectation manifest (see module note) is not yet implemented"]
async fn profile_specific_expectation_manifest_enforces_the_accepted_authority() {
    panic!(
        "the profile-specific D03 expectation-manifest experiment is pending; \
         see the module note and docs/contracts/execution.md"
    );
}

/// Replays the exact supplied JSON through pinned Codex and probes the v3 leaf boundary.
///
/// The caller supplies an existing writable outside directory and pinned Codex executable;
/// this ignored experiment creates only disposable children, reaps each within the D03 bound,
/// and checks both positive and negative permissions before its own fixture cleanup.
#[tokio::test]
#[ignore = "requires AGENT_IDE_D03_STATE, AGENT_IDE_D03_CODEX, and AGENT_IDE_D03_DENIED_DIR"]
async fn visualization_family_native_d03() {
    let state_text = fs::read_to_string(required("AGENT_IDE_D03_STATE")).unwrap();
    let state = HostSandboxState::parse_json(&state_text).unwrap();
    assert_eq!(state.sandbox_state_json(), state_text);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence_v3("visualization-d03", 1, &state)
            .unwrap(),
    ])
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(&state_text).unwrap();
    let leaf = value["permissionProfile"]["file_system"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| {
            (entry["access"] == "write")
                .then(|| entry["path"]["path"].as_str())
                .flatten()
        })
        .find(|path| path.contains("/.codex/visualizations/"))
        .map(PathBuf::from)
        .unwrap();
    let codex = PathBuf::from(required("AGENT_IDE_D03_CODEX"));
    let outside_dir = PathBuf::from(required("AGENT_IDE_D03_DENIED_DIR"));
    let sibling = test_path(leaf.parent().unwrap(), "sibling-dir");
    fs::create_dir(&sibling).unwrap();
    let touch = Path::new("/usr/bin/touch");
    let cat = Path::new("/bin/cat");
    let nc = Path::new("/usr/bin/nc");
    for program in [touch, cat, nc] {
        assert!(program.is_file(), "missing D03 helper {program:?}");
    }
    let worktree_write = test_path(Path::new(env!("CARGO_MANIFEST_DIR")), "worktree-write");
    let leaf_write = test_path(&leaf, "leaf-write");
    let sibling_write = test_path(&sibling, "sibling-write");
    let outside_write = test_path(&outside_dir, "outside-write");
    // Prove the negative target is writable by the unsandboxed fixture process first.
    fs::write(&outside_write, b"baseline").unwrap();
    fs::remove_file(&outside_write).unwrap();
    for (label, target, expected) in [
        ("worktree-write", &worktree_write, true),
        ("leaf-write", &leaf_write, true),
        ("sibling-write", &sibling_write, false),
        ("outside-write", &outside_write, false),
    ] {
        let result = run_child(
            &request(&state, &catalog, touch, target.clone()),
            &codex,
            label,
        )
        .await;
        let written = target.is_file();
        if written {
            fs::remove_file(target).unwrap();
        }
        assert_eq!(
            result.status().success(),
            expected,
            "{label} stderr: {:?}",
            result.stderr().bytes
        );
        assert_eq!(written, expected, "{label} write result");
    }
    let credential = test_path(&leaf, "credential").with_extension("key");
    let ordinary = test_path(&leaf, "ordinary").with_extension("txt");
    fs::write(&credential, b"D03 sentinel").unwrap();
    fs::write(&ordinary, b"D03 ordinary").unwrap();
    for (label, target, expected) in [
        ("credential-read", &credential, false),
        ("ordinary-read", &ordinary, true),
    ] {
        let result = run_child(
            &request(&state, &catalog, cat, target.clone()),
            &codex,
            label,
        )
        .await;
        assert_eq!(
            result.status().success(),
            expected,
            "{label} stderr: {:?}",
            result.stderr().bytes
        );
    }
    fs::remove_file(credential).unwrap();
    fs::remove_file(ordinary).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let result = run_child(
        &request_args(
            &state,
            &catalog,
            nc,
            vec![
                "-z".into(),
                "-w".into(),
                "1".into(),
                "127.0.0.1".into(),
                port.into(),
            ],
        ),
        &codex,
        "network-enabled",
    )
    .await;
    assert!(
        result.status().success(),
        "enabled network stderr: {:?}",
        result.stderr().bytes
    );
    fs::remove_dir(sibling).unwrap();
}

/// Proves Codex accepts the captured sandbox JSON after semantic Value serialization, not only original spelling.
#[tokio::test]
#[ignore = "requires AGENT_IDE_D03_STATE and AGENT_IDE_D03_CODEX"]
async fn managed_profile_accepts_semantic_json_reserialization() {
    let captured = fs::read_to_string(required("AGENT_IDE_D03_STATE")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&captured).unwrap();
    let semantic_variant = format!(
        "{{\n  \"useLegacyLandlock\": {},\n  \"sandboxCwd\": {},\n  \"permissionProfile\": {},\n  \"codexLinuxSandboxExe\": {}\n}}\n",
        serde_json::to_string_pretty(&value["useLegacyLandlock"]).unwrap(),
        serde_json::to_string_pretty(&value["sandboxCwd"]).unwrap(),
        serde_json::to_string_pretty(&value["permissionProfile"]).unwrap(),
        serde_json::to_string_pretty(&value["codexLinuxSandboxExe"]).unwrap(),
    );
    let state = HostSandboxState::parse_json(&semantic_variant).unwrap();
    assert_ne!(state.sandbox_state_json(), captured);
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("captured-managed-d03", 1, &state)
            .unwrap(),
    ])
    .unwrap();
    let codex = PathBuf::from(required("AGENT_IDE_D03_CODEX"));
    let touch = Path::new("/usr/bin/touch");
    let allowed = test_path(state.cwd(), "reserialized");
    let result = run_child(
        &request(&state, &catalog, touch, allowed.clone()),
        &codex,
        "reserialized",
    )
    .await;
    assert!(
        result.status().success(),
        "reserialized stderr: {:?}",
        result.stderr().bytes
    );
    assert!(
        allowed.is_file(),
        "reserialized state did not permit fixture write"
    );
    fs::remove_file(allowed).unwrap();
}

/// Proves the fixed pre-authority Git query runs with a fresh consumed binding use under the captured profile.
#[tokio::test]
#[ignore = "requires AGENT_IDE_D03_STATE and AGENT_IDE_D03_CODEX"]
async fn managed_profile_runs_fixed_git_discovery() {
    let captured = fs::read_to_string(required("AGENT_IDE_D03_STATE")).unwrap();
    let state = HostSandboxState::parse_json(&captured).unwrap();
    let observed_value: serde_json::Value = serde_json::from_str(&captured).unwrap();
    let catalog = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("captured-managed-d03", 1, &state)
            .unwrap(),
    ])
    .unwrap();
    let mut guard = HostBindingGuard::default();
    let channel = parse_channel_session(b"d03-discovery-channel").unwrap();
    assert!(matches!(
        guard.observe_hook(pre_hook("actor", "call"), channel.clone()),
        BindingStatus::PreObserved
    ));
    let BindingStatus::Validated(invocation) =
        guard.establish_start(candidate("actor", "call"), channel)
    else {
        panic!("explicit start must establish the real discovery probe binding");
    };
    let active = guard.consume_active(invocation.binding_ref()).unwrap();
    let observed = parse_observed_sandbox_state(
        serde_json::json!({"codex/sandbox-state-meta": observed_value})
            .as_object()
            .unwrap(),
        &invocation,
        &active,
        true,
    )
    .unwrap();
    let discovery = DiscoverWorktreeRequest::from_active_observation(
        active,
        observed,
        state.cwd().as_os_str().to_owned(),
        DiscoveryOperationRef::new("d03-discovery").unwrap(),
    )
    .unwrap()
    .validate_query(
        GitDiscoveryQuery::ShowTopLevel,
        &GitDiscoveryPolicy::new(PathBuf::from("/usr/bin/git"), 4096, false).unwrap(),
        &catalog,
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
    let lease = match admission.submit(
        OwnerId::new("d03-discovery").unwrap(),
        AdmissionClass::Interactive,
    ) {
        Admission::Granted(lease) => lease,
        outcome => panic!("unexpected discovery admission: {outcome:?}"),
    };
    let result = discovery
        .spawn(
            lease,
            guard.consume_active(invocation.binding_ref()).unwrap(),
            Path::new(&required("AGENT_IDE_D03_CODEX")),
        )
        .unwrap()
        .reap(Duration::from_secs(10), Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        result.evidence.exit_status().success(),
        "git stderr: {:?}",
        result.evidence.stderr().bytes
    );
    assert!(!result.evidence.stdout().bytes.is_empty());
    assert!(result.evidence.stdout().complete && result.evidence.stderr().complete);
    assert!(
        admission
            .release_reaped(result.settlement)
            .unwrap()
            .is_empty()
    );
}
