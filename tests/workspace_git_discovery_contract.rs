//! Canonical discovery validation for raw Execution outputs and Git worktree administrative identity.
#[path = "support/git_snapshot.rs"]
mod support;

use agent_ide::{
    execution::{
        CapturedOutput, DescendantEvidence, DiscoveryOperationRef, GitDiscoveryEvidence,
        GitDiscoveryQuery,
    },
    workspace::git::{
        GitError,
        discovery::{DiscoveredWorktree, validate_discovery as validate_captured_discovery},
    },
};
use std::{
    ffi::OsString,
    fs,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        process::ExitStatusExt,
    },
    path::Path,
    process::{Command, ExitStatus},
    time::Duration,
};
use support::{GIT, GitFixture};

/// Mutable test data for malformed-input scenarios; conversion still passes Execution's checked constructor.
#[derive(Clone)]
struct DiscoveryFixture {
    /// Exact test operation reference.
    operation: DiscoveryOperationRef,
    /// Fixed query tag.
    query: GitDiscoveryQuery,
    /// Raw captured test stdout.
    stdout: CapturedOutput,
    /// Raw captured test stderr.
    stderr: CapturedOutput,
    /// Synthetic or actually observed child status; no settlement capability is present.
    exit_status: ExitStatus,
    /// Test capture interval.
    elapsed: Duration,
}

/// Converts fixture data through the immutable evidence boundary before exercising Workspace validation.
fn validate_discovery(
    candidate: &Path,
    operation: &DiscoveryOperationRef,
    fixtures: &[DiscoveryFixture],
) -> Result<DiscoveredWorktree, GitError> {
    let evidence = fixtures
        .iter()
        .map(|fixture| {
            let mut stdout = fixture.stdout.clone();
            stdout.drained_bytes = stdout.drained_bytes.max(stdout.bytes.len() as u64);
            GitDiscoveryEvidence::new(
                fixture.operation.clone(),
                fixture.query,
                stdout,
                fixture.stderr.clone(),
                fixture.exit_status,
                fixture.elapsed,
                None,
                DescendantEvidence::Unverified,
            )
            .map_err(|_| GitError::InvalidDiscovery)
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_captured_discovery(candidate, operation, &evidence)
}

/// Wraps truthful bounded real output with the typed query/operation correlation consumed by Workspace.
fn discovered(root: &Path) -> (DiscoveryOperationRef, Vec<DiscoveryFixture>) {
    let operation = DiscoveryOperationRef::new("discovery").unwrap();
    let mut outputs = Vec::new();
    for (query, args) in [
        (
            GitDiscoveryQuery::ShowTopLevel,
            vec!["rev-parse", "--show-toplevel"],
        ),
        (
            GitDiscoveryQuery::GitCommonDir,
            vec!["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ),
        (
            GitDiscoveryQuery::WorktreeListPorcelainZ,
            vec!["worktree", "list", "--porcelain", "-z"],
        ),
    ] {
        let output = Command::new(GIT)
            .env_clear()
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
        outputs.push(DiscoveryFixture {
            operation: operation.clone(),
            query,
            stdout: captured(output.stdout),
            stderr: captured(output.stderr),
            exit_status: output.status,
            elapsed: Duration::ZERO,
        });
    }
    (operation, outputs)
}

/// Retains exact bytes and truthful complete stream metadata for synthetic mutation checks.
fn captured(bytes: Vec<u8>) -> CapturedOutput {
    CapturedOutput {
        drained_bytes: bytes.len() as u64,
        bytes,
        truncated: false,
        complete: true,
    }
}

/// Main, nested and linked worktree candidates resolve raw top-level/common identity without guessing.
#[test]
fn real_main_nested_and_linked_discovery_keeps_raw_paths() {
    let fixture = GitFixture::new();
    let nested = fixture.root.join("nested space\npath");
    fs::create_dir(&nested).unwrap();
    for candidate in [&fixture.root, &nested] {
        let (operation, outputs) = discovered(candidate);
        let result = validate_discovery(candidate, &operation, &outputs).unwrap_or_else(|error| {
            panic!(
                "candidate {candidate:?}: {error:?}; top {:?}, common {:?}, list {:?}",
                outputs[0].stdout.bytes, outputs[1].stdout.bytes, outputs[2].stdout.bytes
            )
        });
        assert_eq!(result.root(), fixture.root);
        assert_eq!(result.repository_root(), fixture.root);
        assert_eq!(result.common_dir(), fixture.root.join(".git"));
    }
    let linked = fixture.root.join("linked space\npath");
    fixture.git_os([
        "worktree".into(),
        "add".into(),
        "--quiet".into(),
        "--detach".into(),
        linked.as_os_str().to_owned(),
        "HEAD".into(),
    ]);
    let (operation, outputs) = discovered(&linked);
    let result = validate_discovery(&linked, &operation, &outputs).unwrap();
    assert_eq!(result.root(), linked);
    assert_eq!(result.repository_root(), linked);
    assert_eq!(result.common_dir(), fixture.root.join(".git"));
    let target = fs::read(linked.join(".git")).unwrap();
    let administrative = Path::new(std::ffi::OsStr::from_bytes(
        target
            .strip_prefix(b"gitdir: ")
            .unwrap()
            .strip_suffix(b"\n")
            .unwrap(),
    ));
    fs::write(
        administrative.join("gitdir"),
        format!("{}/wrong/.git\n", fixture.root.display()),
    )
    .unwrap();
    assert_eq!(
        validate_discovery(&linked, &operation, &outputs),
        Err(GitError::InvalidDiscovery)
    );
}

/// Missing/failed/partial/mixed/duplicate/foreign evidence and malformed listings never authorize a candidate.
#[test]
fn invalid_discovery_is_rejected_before_durable_identity() {
    let fixture = GitFixture::new();
    let foreign = GitFixture::new();
    let (operation, outputs) = discovered(&fixture.root);
    assert!(validate_discovery(&fixture.root, &operation, &outputs[..2]).is_err());
    let mut unsupported = outputs.clone();
    unsupported[1].exit_status = ExitStatus::from_raw(129 << 8);
    unsupported[1].stderr = captured(b"error: unknown option `path-format=absolute'\n".to_vec());
    assert_eq!(
        validate_discovery(&fixture.root, &operation, &unsupported),
        Err(GitError::UnsupportedDiscoveryGit)
    );
    let mut unsupported = outputs.clone();
    unsupported[1].stdout = captured(b"--path-format=absolute\n.git\n".to_vec());
    assert_eq!(
        validate_discovery(&fixture.root, &operation, &unsupported),
        Err(GitError::UnsupportedDiscoveryGit)
    );
    let mut relative = outputs.clone();
    relative[1].stdout = captured(b".git\n".to_vec());
    assert!(validate_discovery(&fixture.root, &operation, &relative).is_err());
    let mut bad = outputs.clone();
    bad[0].exit_status = ExitStatus::from_raw(1 << 8);
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[1].stdout.truncated = true;
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[2].stderr.complete = false;
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[1].operation = DiscoveryOperationRef::new("foreign").unwrap();
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[1].query = GitDiscoveryQuery::ShowTopLevel;
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[2].stdout.bytes.pop();
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[2].stdout.bytes.extend(outputs[2].stdout.bytes.clone());
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[1].stdout = captured([foreign.root.join(".git").as_os_str().as_bytes(), b"\n"].concat());
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
    let mut bad = outputs.clone();
    bad[2].stdout = discovered(&foreign.root).1[2].stdout.clone();
    assert!(validate_discovery(&fixture.root, &operation, &bad).is_err());
}

/// Unrelated stale/prunable and non-UTF-8 list paths are parsed boundedly but never followed.
#[test]
fn raw_peer_paths_are_not_opened_or_lossily_decoded() {
    let fixture = GitFixture::new();
    let (operation, mut outputs) = discovered(&fixture.root);
    let path = OsString::from_vec(b"/missing/non-utf8-\xff\n space".to_vec());
    outputs[2].stdout.bytes.extend(
        [
            b"worktree ".as_slice(),
            path.as_bytes(),
            b"\0HEAD ",
            &[b'a'; 40],
            b"\0detached\0prunable missing administrative file\0\0",
        ]
        .concat(),
    );
    assert!(validate_discovery(&fixture.root, &operation, &outputs).is_ok());
    let duplicate = outputs[2].stdout.bytes.clone();
    outputs[2].stdout.bytes.extend(duplicate);
    assert!(validate_discovery(&fixture.root, &operation, &outputs).is_err());
}
