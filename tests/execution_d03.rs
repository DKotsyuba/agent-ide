//! Real managed-sandbox D03 check driven by a captured host state and disposable targets.

#[allow(dead_code)]
#[path = "../src/execution/mod.rs"]
mod execution;

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
    ControlledCommand, ExecutionProfileCatalog, ExecutionProfileTemplate, HostSandboxState,
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
    let authority = WorkspaceAuthority::from_workspace(state.cwd().to_path_buf(), 1).unwrap();
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
) -> execution::ReapedProcess {
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
    let result = OwnedChild::spawn_captured(request, lease, codex, 8192)
        .unwrap()
        .reap(Duration::from_secs(10))
        .await
        .unwrap();
    assert!(result.stdout.complete && result.stderr.complete);
    assert!(admission.release(result.lease).unwrap().is_empty());
    result
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
        allowed_result.status.success(),
        "allowed stderr: {:?}",
        allowed_result.stderr.bytes
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
            !result.status.success() && !unexpectedly_written,
            "{label} write escaped the captured profile; stderr: {:?}",
            result.stderr.bytes
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
        !network_result.status.success(),
        "network-restricted profile connected to the controlled listener"
    );
}
