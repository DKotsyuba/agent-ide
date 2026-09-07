//! Contract checks for the unassembled Execution module.

#[allow(dead_code)]
#[path = "../src/execution/mod.rs"]
mod execution;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use execution::{
    Admission, AdmissionClass, AdmissionController, AdmissionLimits, BorrowedEndpoint, CommandKind,
    ControlledCommand, EndpointOwnership, ExecutionProfileCatalog, ExecutionProfileTemplate,
    HostSandboxState, LocalExecutionPolicy, OwnerId, ProfileClass, SandboxStateError,
    ValidatedExecutionRequest, ValidatedHostInvocation, WorkspaceAuthority,
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

/// Builds a request whose argv/profile data is fixed by this test rather than model text.
fn request(root: &Path, script: &str) -> ValidatedExecutionRequest {
    let sandbox = disabled_state(root);
    let profiles = ExecutionProfileCatalog::from_execution_evidence(vec![
        ExecutionProfileTemplate::from_execution_evidence("disabled-contract-case", 1, &sandbox)
            .unwrap(),
    ])
    .unwrap();
    let invocation = ValidatedHostInvocation::from_verified_binding("bound-test", sandbox).unwrap();
    let authority = WorkspaceAuthority::from_workspace(root.to_path_buf(), 7).unwrap();
    let command = ControlledCommand::from_validated_peer(
        CommandKind::Job,
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
    let result =
        execution::OwnedChild::spawn_captured(&request, lease, Path::new("/usr/bin/codex"), 3)
            .unwrap()
            .reap(Duration::from_secs(1))
            .await
            .unwrap();
    assert!(result.status.success());
    assert_eq!(result.stdout.bytes, b"abc");
    assert_eq!(result.stderr.bytes, b"123");
    assert_eq!(result.stdout.drained_bytes, 6);
    assert_eq!(result.stderr.drained_bytes, 6);
    assert!(result.stdout.truncated && result.stderr.truncated);
    assert!(result.stdout.complete && result.stderr.complete);
    assert!(admission.release(result.lease).unwrap().is_empty());
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
    let result =
        execution::OwnedChild::spawn_captured(&request, lease, Path::new("/usr/bin/codex"), 64)
            .unwrap()
            .cancel_and_reap(Duration::from_millis(100), Duration::from_secs(1))
            .await
            .unwrap();
    assert!(result.cancellation.unwrap().term_requested);
    assert_eq!(
        result.descendants,
        execution::DescendantEvidence::Unverified
    );
    assert!(admission.release(result.lease).unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}

/// Ensures protocol stdout stays with Intelligence rather than becoming a second Execution reader.
#[tokio::test]
async fn protocol_stdout_has_one_owner_and_borrowed_endpoints_cannot_be_killed() {
    let root = worktree();
    let request = request(&root, "printf protocol; printf diagnostic >&2");
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
    let mut child =
        execution::OwnedProtocolChild::spawn(&request, lease, Path::new("/usr/bin/codex"), 64)
            .unwrap();
    let mut protocol = String::new();
    child.stdout.read_to_string(&mut protocol).await.unwrap();
    let (status, stderr, released) = child.reap(Duration::from_secs(1)).await.unwrap();
    assert!(status.success());
    assert_eq!(protocol, "protocol");
    assert_eq!(stderr.bytes, b"diagnostic");
    assert!(admission.release(released).unwrap().is_empty());
    assert_eq!(
        BorrowedEndpoint::observe("peer:42").unwrap().cancel(),
        EndpointOwnership::Borrowed
    );
    fs::remove_dir_all(root).unwrap();
}
