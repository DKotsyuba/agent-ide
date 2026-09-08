//! Real Apple Git and Execution regression checks for the raw snapshot pipeline.
#[path = "support/git_snapshot.rs"]
mod support;

use agent_ide::{
    changes::{DiffResultState, DiffSelectionBudget, compose_diff},
    workspace::git::{
        DiffMode, GitError,
        snapshot::{MAX_SNAPSHOT_BLOB_BYTES, SnapshotIntent},
    },
};
use std::{ffi::OsString, fs, os::unix::ffi::OsStrExt, path::PathBuf};
use support::{GIT, GitFixture, Runner, authority_for, collect};

/// Proves clean/smudge/process, textconv, external-diff and fsmonitor helpers never run in any mode.
#[tokio::test]
async fn content_queries_must_not_execute_repository_clean_filters() {
    let fixture = GitFixture::new();
    fixture.install_malicious_helpers();
    fixture.write(b".gitattributes", b"*.txt diff=evil\nstaged.txt filter=clean\nunstaged.txt filter=process\n\"special space\\n-leading.txt\" filter=smudge\n");
    for key in [
        "filter.clean.clean",
        "filter.smudge.smudge",
        "filter.process.process",
    ] {
        fixture.git_os([
            OsString::from("config"),
            OsString::from("--local"),
            key.into(),
            fixture.root.join(".git/sentinel.sh").into_os_string(),
        ]);
    }
    for mode in [DiffMode::Head, DiffMode::Staged, DiffMode::Unstaged] {
        let mut runner = Runner::default();
        let snapshot = collect(&fixture, mode, &mut runner).await.unwrap();
        assert!(
            runner.different > 0,
            "Apple Git no-index returns successful differences as exit 1"
        );
        assert!(runner.directories.iter().all(|dir| !dir.exists()));
        assert!(
            !fixture.sentinel.exists(),
            "a repository helper executed in {mode:?}"
        );
        let comparison = snapshot.comparison().clone();
        let scope = snapshot.scope().clone();
        let result = compose_diff(
            &scope,
            &comparison,
            snapshot,
            DiffSelectionBudget::default(),
        );
        assert_eq!(result.state(), DiffResultState::Ready);
        assert!(
            result
                .selected_hunks()
                .iter()
                .all(|hunk| !hunk.patch().starts_with(b"diff --git"))
        );
        assert!(
            result
                .untracked()
                .iter()
                .any(|path| path.path().as_os_str().as_bytes() == b"untracked space\n-leading.txt")
        );
        let paths: Vec<_> = result.tracked().iter().map(|entry| entry.path()).collect();
        match mode {
            DiffMode::Head => assert_eq!(paths.len(), 3),
            DiffMode::Staged => {
                assert_eq!(paths.len(), 2);
                assert!(!paths.contains(&std::path::Path::new("unstaged.txt")));
            }
            DiffMode::Unstaged => assert_eq!(paths, vec![std::path::Path::new("unstaged.txt")]),
        }
        if fixture.non_utf8_supported {
            assert!(
                result.untracked().iter().any(
                    |path| path.path().as_os_str().as_bytes() == b"untracked-non-utf8-\xff.txt"
                )
            );
        } else {
            eprintln!(
                "platform unsupported: Darwin filesystem rejected non-UTF-8 filenames; byte parser contract is separately checked"
            );
        }
    }
    let output = std::process::Command::new(GIT)
        .arg("--version")
        .output()
        .unwrap();
    eprintln!(
        "real platform evidence: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

/// Confirms raw additions/deletions, rename source, binary, mode-only and conflicts remain attributable.
#[tokio::test]
async fn raw_modes_paths_and_separate_conflicts_survive() {
    use std::{
        io::Write,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };
    let fixture = GitFixture::new();
    for (path, bytes) in [
        ("delete.txt", b"delete me\n".as_slice()),
        ("rename-old.txt", b"rename me\n"),
        ("binary.txt", b"binary\0old"),
        ("mode.txt", b"same bytes\n"),
        ("conflict.txt", b"conflict\n"),
    ] {
        fixture.write(path.as_bytes(), bytes);
    }
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "edge baseline"]);
    fixture.write(b"add.txt", b"added\n");
    fixture.git(["add", "add.txt"]);
    fixture.git(["rm", "--quiet", "delete.txt"]);
    fixture.git(["mv", "rename-old.txt", "rename-new.txt"]);
    fixture.write(b"binary.txt", b"binary\0new");
    fs::set_permissions(
        fixture.root.join("mode.txt"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let object = fixture.git(["rev-parse", "HEAD:conflict.txt"]);
    let oid = std::str::from_utf8(&object.stdout).unwrap().trim();
    let mut child = Command::new(GIT)
        .env_clear()
        .arg("-C")
        .arg(&fixture.root)
        .args(["update-index", "--index-info"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let conflict = format!(
        "0 {}\tconflict.txt\n100644 {oid} 1\tconflict.txt\n100644 {oid} 2\tconflict.txt\n100644 {oid} 3\tconflict.txt\n",
        "0".repeat(40)
    );
    child
        .stdin
        .take()
        .unwrap()
        .write_all(conflict.as_bytes())
        .unwrap();
    assert!(child.wait().unwrap().success());
    fixture.write(b"untracked.txt", b"untracked\n");
    fixture.install_malicious_helpers();
    let mut runner = Runner::default();
    let snapshot = collect(&fixture, DiffMode::Head, &mut runner)
        .await
        .unwrap();
    let renamed = snapshot
        .paths()
        .iter()
        .find(|path| path.status().path() == std::path::Path::new("rename-new.txt"))
        .unwrap();
    assert_eq!(renamed.status().original_path(), None);
    assert!(
        !renamed.patch().is_empty(),
        "raw collection represents rename as delete/add without guessing"
    );
    assert!(
        snapshot
            .paths()
            .iter()
            .any(|path| path.status().path() == std::path::Path::new("rename-old.txt"))
    );
    assert!(snapshot.paths().iter().all(|path| path.source().is_some()));
    let deleted = snapshot
        .paths()
        .iter()
        .find(|path| path.status().path() == std::path::Path::new("delete.txt"))
        .unwrap();
    assert!(deleted.source().unwrap().bytes().is_none());
    let scope = snapshot.scope().clone();
    let comparison = snapshot.comparison().clone();
    let result = compose_diff(
        &scope,
        &comparison,
        snapshot,
        DiffSelectionBudget::default(),
    );
    assert_eq!(
        result.state(),
        DiffResultState::Incomplete,
        "binary is explicit partial textual coverage"
    );
    assert_eq!(result.conflicts().len(), 1);
    assert!(
        result
            .selected_hunks()
            .iter()
            .all(|hunk| hunk.path() != &PathBuf::from("conflict.txt"))
    );
    let binary = result
        .selected_hunks()
        .iter()
        .find(|hunk| hunk.is_binary())
        .unwrap();
    assert_eq!(binary.path(), &PathBuf::from("binary.txt"));
    assert!(binary.patch().is_empty());
    let mode = result
        .tracked()
        .iter()
        .find(|entry| entry.path() == std::path::Path::new("mode.txt"))
        .unwrap();
    assert_eq!(mode.modes(), Some([0o100644, 0o100644, 0o100755]));
    assert!(runner.directories.iter().all(|dir| !dir.exists()));
    assert!(!fixture.sentinel.exists());
}

/// Retries one changing capture, rejects a second change, and removes every scratch directory.
#[tokio::test]
async fn instability_has_one_retry_and_cleanup() {
    let fixture = GitFixture::new();
    let mut once = Runner {
        mutate_path: Some(fixture.root.join("unstaged.txt")),
        mutate_once: true,
        ..Runner::default()
    };
    assert!(
        collect(&fixture, DiffMode::Unstaged, &mut once)
            .await
            .is_ok()
    );
    assert_eq!(once.directories.len(), 2);
    assert!(once.directories.iter().all(|dir| !dir.exists()));
    let mut always = Runner {
        mutate_path: Some(fixture.root.join("unstaged.txt")),
        ..Runner::default()
    };
    assert_eq!(
        collect(&fixture, DiffMode::Unstaged, &mut always).await,
        Err(GitError::UnstableSnapshot)
    );
    assert_eq!(always.directories.len(), 2);
    assert!(always.directories.iter().all(|dir| !dir.exists()));
}

/// Rejects oversized source/blob evidence, excessive paths, and unborn HEAD without producing clean results.
#[tokio::test]
async fn snapshot_bounds_and_unborn_head_are_explicit() {
    let fixture = GitFixture::new();
    fixture.write(b"unstaged.txt", &vec![b'x'; MAX_SNAPSHOT_BLOB_BYTES + 1]);
    assert_eq!(
        collect(&fixture, DiffMode::Unstaged, &mut Runner::default()).await,
        Err(GitError::EvidenceTooLarge)
    );
    fixture.write(b"unstaged.txt", b"working change\n");
    for n in 0..257 {
        fixture.write(format!("untracked-{n}").as_bytes(), b"");
    }
    assert_eq!(
        collect(&fixture, DiffMode::Head, &mut Runner::default()).await,
        Err(GitError::EvidenceTooLarge)
    );
    assert_eq!(
        collect(
            &GitFixture::unborn(),
            DiffMode::Head,
            &mut Runner::default()
        )
        .await,
        Err(GitError::UnbornHead)
    );
}

/// An abandoned or oversized comparison leaves no scratch files, including cloned pending intents.
#[test]
fn snapshot_intents_own_private_file_cleanup() {
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent =
        SnapshotIntent::compare(scope.clone(), std::path::Path::new(GIT), b"left", b"right")
            .unwrap();
    let dir = intent.snapshot_directory().unwrap().to_path_buf();
    let pending = intent.clone();
    drop(intent);
    assert!(dir.exists());
    drop(pending);
    assert!(!dir.exists());
    assert!(
        SnapshotIntent::compare(
            scope,
            std::path::Path::new(GIT),
            &vec![0; MAX_SNAPSHOT_BLOB_BYTES + 1],
            b""
        )
        .is_err()
    );
}

/// Failed, signalled, truncated and undrained processes cannot mint patch evidence; exit one can.
#[tokio::test]
async fn incomplete_execution_never_becomes_snapshot_evidence() {
    use agent_ide::workspace::git::snapshot::SnapshotRunner;
    use std::{os::unix::process::ExitStatusExt, process::ExitStatus};
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent =
        SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"old\n", b"new\n").unwrap();
    let dir = intent.snapshot_directory().unwrap().to_owned();
    let result = Runner::default().run(intent.clone()).await.unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(intent.accept(result.clone()).is_ok());
    let mut rejected = Vec::new();
    let mut output = result.clone();
    output.status = ExitStatus::from_raw(2 << 8);
    rejected.push(output);
    let mut output = result.clone();
    output.status = ExitStatus::from_raw(9);
    rejected.push(output);
    let mut output = result.clone();
    output.stdout.complete = false;
    rejected.push(output);
    let mut output = result.clone();
    output.stderr.complete = false;
    rejected.push(output);
    let mut output = result.clone();
    output.stdout.truncated = true;
    rejected.push(output);
    let mut output = result.clone();
    output.stderr.truncated = true;
    rejected.push(output);
    let mut output = result;
    output.stdout.bytes = vec![0; MAX_SNAPSHOT_BLOB_BYTES + 1];
    rejected.push(output);
    for output in rejected {
        assert_eq!(intent.accept(output), Err(GitError::IncompleteIdentity));
    }
    drop(intent);
    assert!(!dir.exists());
}

/// Changed committed identities and untracked metadata invalidate the entire capture generation.
#[tokio::test]
async fn metadata_brackets_detect_head_and_untracked_changes() {
    use std::process::Command;
    let fixture = GitFixture::new();
    let initial = fixture.git(["rev-parse", "HEAD"]);
    let alternate = fixture.git(["commit-tree", "-m", "alternate metadata", "HEAD^{tree}"]);
    let initial = String::from_utf8(initial.stdout).unwrap().trim().to_owned();
    let alternate = String::from_utf8(alternate.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let root = fixture.root.clone();
    let mut alternate_next = true;
    let mut runner = Runner {
        after_compare: Some(Box::new(move || {
            let oid = if alternate_next { &alternate } else { &initial };
            alternate_next = !alternate_next;
            assert!(
                Command::new(GIT)
                    .env_clear()
                    .arg("-C")
                    .arg(&root)
                    .args(["update-ref", "HEAD", oid])
                    .status()
                    .unwrap()
                    .success()
            );
        })),
        ..Runner::default()
    };
    assert!(matches!(
        collect(&fixture, DiffMode::Unstaged, &mut runner).await,
        Err(GitError::UnstableSnapshot)
    ));
    assert_eq!(runner.directories.len(), 2);
    assert!(runner.directories.iter().all(|path| !path.exists()));
    let path = fixture.root.join("metadata-created");
    let mut next = true;
    let mut runner = Runner {
        after_compare: Some(Box::new(move || {
            if next {
                fs::write(&path, b"new").unwrap();
            } else {
                fs::remove_file(&path).unwrap();
            }
            next = !next;
        })),
        ..Runner::default()
    };
    assert!(matches!(
        collect(&fixture, DiffMode::Unstaged, &mut runner).await,
        Err(GitError::UnstableSnapshot)
    ));
    assert_eq!(runner.directories.len(), 2);
}

/// Aggregate source and patch ceilings reject complete-looking output after bounded work.
#[tokio::test]
async fn aggregate_source_and_patch_limits_are_enforced() {
    let fixture = GitFixture::new();
    for n in 0..9 {
        fixture.write(format!("large-{n}").as_bytes(), b"base\n");
    }
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "aggregate baseline"]);
    for n in 0..9 {
        fixture.write(format!("large-{n}").as_bytes(), &vec![0; 950_000]);
    }
    let mut runner = Runner::default();
    assert!(matches!(
        collect(&fixture, DiffMode::Head, &mut runner).await,
        Err(GitError::EvidenceTooLarge)
    ));
    assert!(runner.directories.iter().all(|dir| !dir.exists()));
    let fixture = GitFixture::new();
    for path in ["patch-a", "patch-b"] {
        fixture.write(path.as_bytes(), b"");
    }
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "patch baseline"]);
    for path in ["patch-a", "patch-b"] {
        fixture.write(path.as_bytes(), &b"long added text line\n".repeat(29_000));
    }
    let mut runner = Runner::default();
    assert!(matches!(
        collect(&fixture, DiffMode::Head, &mut runner).await,
        Err(GitError::EvidenceTooLarge)
    ));
    assert!(runner.directories.iter().all(|dir| !dir.exists()));
}

/// Optional persisted source correlation retains exact revision/sequence and rejects stale byte identity.
#[tokio::test]
async fn source_digest_revision_and_sequence_are_correlated() {
    use agent_ide::{
        app::{
            config::StoreConfig,
            store::{OperationId, Store},
        },
        workspace::{
            observation::{ObservationRef, SourceBytes, SourceCoverage, SourceRevision},
            store::{ObservationAdmission, ObservationDraft, WorkspaceStore},
        },
    };
    let fixture = GitFixture::new();
    let authority = authority_for(&fixture);
    let store = Store::open(
        &fixture.root.join(".git/source.sqlite"),
        StoreConfig {
            queue_capacity: 8,
            busy_timeout: std::time::Duration::from_millis(100),
            request_deadline: std::time::Duration::from_secs(1),
            receipt_capacity: 16,
        },
    )
    .unwrap();
    let workspace = WorkspaceStore::new(&store);
    workspace.install_schema().await.unwrap();
    let ObservationAdmission::Recorded(observation) = workspace
        .record(
            ObservationDraft::present(
                authority.worktree().clone(),
                authority.epoch(),
                OperationId::new("source").unwrap(),
                ObservationRef::new("source").unwrap(),
                "unstaged.txt".into(),
                SourceBytes::from_bytes(b"working change\n"),
                SourceRevision::new("source-revision").unwrap(),
                SourceCoverage::Complete,
            )
            .unwrap(),
        )
        .await
        .unwrap()
    else {
        panic!("source persisted")
    };
    let mut runner = Runner {
        source_observation: Some(observation.clone()),
        ..Runner::default()
    };
    let snapshot = support::capture_with_authority(&authority, DiffMode::Unstaged, &mut runner)
        .await
        .unwrap();
    let source = snapshot.paths()[0].source().unwrap();
    assert_eq!(source.observation(), Some(&observation));
    assert_eq!(source.bytes(), observation.bytes());
    assert_eq!(
        source.observation().unwrap().source_revision().as_str(),
        "source-revision"
    );
    assert_eq!(
        source.observation().unwrap().sequence(),
        observation.sequence()
    );
    fixture.write(b"unstaged.txt", b"newer bytes\n");
    assert!(matches!(
        support::capture_with_authority(&authority, DiffMode::Unstaged, &mut runner).await,
        Err(GitError::UnstableSnapshot)
    ));
}

/// A Git binary lacking mandatory no-lazy-fetch is explicit unsupported, never a weaker fallback.
#[tokio::test]
async fn unsupported_git_and_missing_promisor_blob_fail_without_helpers() {
    use agent_ide::{execution::CapturedOutput, workspace::git::snapshot::SnapshotRunner};
    use std::{os::unix::process::ExitStatusExt, process::ExitStatus};
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent =
        SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"old\n", b"new\n").unwrap();
    let mut output = Runner::default().run(intent.clone()).await.unwrap();
    output.status = ExitStatus::from_raw(129 << 8);
    output.stderr = CapturedOutput {
        bytes: b"unknown option: --no-lazy-fetch\n".to_vec(),
        truncated: false,
        complete: true,
        drained_bytes: 30,
    };
    assert_eq!(intent.accept(output), Err(GitError::UnsupportedSnapshotGit));
    fixture.install_malicious_helpers();
    fixture.git(["config", "remote.origin.promisor", "true"]);
    fixture.git(["config", "protocol.ext.allow", "always"]);
    fixture.git_os([
        "config".into(),
        "remote.origin.url".into(),
        format!("ext::{}", fixture.root.join(".git/sentinel.sh").display()).into(),
    ]);
    let object = fixture.git(["rev-parse", "HEAD:unstaged.txt"]);
    let oid = std::str::from_utf8(&object.stdout).unwrap().trim();
    fs::remove_file(
        fixture
            .root
            .join(".git/objects")
            .join(&oid[..2])
            .join(&oid[2..]),
    )
    .unwrap();
    let mut runner = Runner::default();
    assert!(
        collect(&fixture, DiffMode::Unstaged, &mut runner)
            .await
            .is_err()
    );
    assert!(
        !fixture.sentinel.exists(),
        "missing promisor object launched a remote helper"
    );
    assert!(runner.directories.iter().all(|dir| !dir.exists()));
}

/// Git's regular-file mode reflects the owner execute bit, not unrelated group/other permissions.
#[tokio::test]
async fn source_modes_use_git_owner_execute_semantics() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = GitFixture::new();
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "mode baseline"]);
    fs::set_permissions(
        fixture.root.join("unstaged.txt"),
        fs::Permissions::from_mode(0o654),
    )
    .unwrap();
    let snapshot = collect(&fixture, DiffMode::Unstaged, &mut Runner::default())
        .await
        .unwrap();
    assert!(snapshot.paths().is_empty());
    fs::set_permissions(
        fixture.root.join("unstaged.txt"),
        fs::Permissions::from_mode(0o744),
    )
    .unwrap();
    let snapshot = collect(&fixture, DiffMode::Unstaged, &mut Runner::default())
        .await
        .unwrap();
    assert_eq!(snapshot.paths().len(), 1);
    assert_eq!(
        snapshot.paths()[0].status().modes(),
        Some([0o100644, 0o100644, 0o100755])
    );
}

/// The real Execution runner yields a Send collection future suitable for the product's tokio task.
#[test]
fn execution_snapshot_future_is_send() {
    /// Proves the future's Send bound at compile time without polling or spawning a new operation.
    fn assert_send<T: Send>(_future: T) {}
    let fixture = GitFixture::new();
    let authority = authority_for(&fixture);
    let mut runner = Runner::default();
    assert_send(support::capture_with_authority(
        &authority,
        DiffMode::Head,
        &mut runner,
    ));
}
