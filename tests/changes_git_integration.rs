//! Real Apple Git and Execution regression checks for the raw snapshot pipeline.
#[path = "support/git_snapshot.rs"]
mod support;

use agent_ide::{
    changes::{DiffFreshness, DiffResultState, DiffSelectionBudget, compose_diff},
    workspace::{
        git::{
            BaselineContext, BaselineCoverage, DiffMode, GitError,
            snapshot::{MAX_SNAPSHOT_BLOB_BYTES, SnapshotIntent, SnapshotRunner, collect_snapshot},
        },
        store::CurrentObservation,
    },
};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};
use support::{GIT, GitFixture, Runner, authority_for, collect};

/// Adapts the shared process runner with one Workspace-certified current source observation.
struct CurrentRunner {
    /// Existing exact-child Execution runner used unchanged for every Git command.
    inner: Runner,
    /// Sealed durable currentness proof returned only for its exact raw path.
    observation: CurrentObservation,
}

impl SnapshotRunner for CurrentRunner {
    /// Delegates command ownership, bounded collection, and exact reap to the shared runner.
    async fn run(
        &mut self,
        intent: SnapshotIntent,
    ) -> Result<agent_ide::execution::CapturedProcessEvidence, GitError> {
        self.inner.run(intent).await
    }

    /// Test harness: delegates to the shared runner, which admits every fixture path.
    async fn authorize_read_path(&mut self, _path: &Path) -> Result<(), GitError> {
        Ok(())
    }

    /// Returns the sealed observation only for its exact raw source path.
    async fn current_observation(
        &mut self,
        _authority: &agent_ide::workspace::authority::AuthorityStamp,
        path: &Path,
    ) -> Option<CurrentObservation> {
        (self.observation.observation().path() == path).then(|| self.observation.clone())
    }
}

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
    assert_eq!(once.comparisons, 2);
    assert!(once.directories.iter().all(|dir| !dir.exists()));
    let mut always = Runner {
        mutate_path: Some(fixture.root.join("unstaged.txt")),
        ..Runner::default()
    };
    assert_eq!(
        collect(&fixture, DiffMode::Unstaged, &mut always).await,
        Err(GitError::UnstableSnapshot)
    );
    assert_eq!(always.comparisons, 2);
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

/// Writer proving the private scratch root follows the process TMPDIR, not a hardcoded `/tmp`.
/// Run only via the exact-process fixture below so peer tests never race environment mutation.
#[test]
fn snapshot_directory_honors_host_temp_writer() {
    let Ok(custom_temp) = std::env::var("AGENT_IDE_SNAPSHOT_TEMP_FIXTURE") else {
        return;
    };
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent =
        SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"left", b"right").unwrap();
    let dir = intent.snapshot_directory().unwrap().to_path_buf();
    let expected_parent = fs::canonicalize(&custom_temp).expect("fixture temp dir exists");
    assert_eq!(
        dir.parent().expect("scratch dir has a parent"),
        expected_parent,
        "scratch directory must live under the process TMPDIR, not a hardcoded /tmp"
    );
}

/// Spawns the writer above with an isolated TMPDIR so no peer test races process-global environment.
#[test]
fn snapshot_directory_follows_process_tmpdir() {
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    let custom_temp = PathBuf::from(format!(
        "/private/tmp/agent-ide-alt-scratch-{}-{tick}",
        std::process::id()
    ));
    fs::create_dir_all(&custom_temp).expect("alternate temp root is creatable");
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "snapshot_directory_honors_host_temp_writer",
            "--nocapture",
        ])
        .env("TMPDIR", &custom_temp)
        .env("AGENT_IDE_SNAPSHOT_TEMP_FIXTURE", &custom_temp)
        .output()
        .expect("writer process starts");
    fs::remove_dir_all(&custom_temp).ok();
    assert!(
        child.status.success(),
        "writer failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
}

/// Fixture output cannot mint actual-wait identity or release files, even with an apparent success exit.
#[tokio::test]
async fn incomplete_execution_never_becomes_snapshot_evidence() {
    use agent_ide::{execution::CapturedProcessEvidence, workspace::git::snapshot::SnapshotRunner};
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent =
        SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"old\n", b"new\n").unwrap();
    let dir = intent.snapshot_directory().unwrap().to_owned();
    let result = Runner::default().run(intent.clone()).await.unwrap();
    assert_eq!(result.status().code(), Some(1));
    let fixture_only = CapturedProcessEvidence::new(
        result.status(),
        result.cancellation(),
        result.stdout().clone(),
        result.stderr().clone(),
        result.descendants(),
    )
    .unwrap();
    assert!(fixture_only.reap_identity().is_none());
    assert_eq!(
        intent.accept(fixture_only),
        Err(GitError::IncompleteIdentity)
    );
    assert!(intent.accept(result).is_ok());
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
    assert_eq!(runner.comparisons, 2);
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
    assert_eq!(runner.comparisons, 2);
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
    let mut runner = CurrentRunner {
        observation: workspace
            .confirm_current(
                OperationId::new("confirm-source").unwrap(),
                observation.clone(),
            )
            .await
            .unwrap()
            .unwrap(),
        inner: Runner::default(),
    };
    let snapshot = collect_snapshot(
        &authority,
        Path::new(GIT),
        DiffMode::Unstaged,
        1,
        "snapshot-operation",
        BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
        &mut runner,
    )
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
        collect_snapshot(
            &authority,
            Path::new(GIT),
            DiffMode::Unstaged,
            1,
            "snapshot-operation",
            BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
            &mut runner,
        )
        .await,
        Err(GitError::UnstableSnapshot)
    ));
}

/// A Git binary lacking mandatory no-lazy-fetch is explicit unsupported, never a weaker fallback.
#[tokio::test]
async fn unsupported_git_and_missing_promisor_blob_fail_without_helpers() {
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let wrapper = support::git_wrapper(
        &fixture,
        "unsupported-git",
        "printf '%s\\n' 'unknown option: --no-lazy-fetch' >&2\nexit 129",
    );
    let intent = SnapshotIntent::compare(scope, &wrapper, b"old\n", b"new\n").unwrap();
    let (child, mut admissions) =
        support::launch_intent(&intent, &wrapper, MAX_SNAPSHOT_BLOB_BYTES).unwrap();
    let completed = child
        .reap(
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
    admissions.release_reaped(completed.settlement).unwrap();
    assert_eq!(
        intent.accept(completed.evidence),
        Err(GitError::UnsupportedSnapshotGit)
    );
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
    assert!(
        fixture
            .git([
                "-c",
                "core.fsmonitor=false",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=no"
            ])
            .stdout
            .is_empty(),
        "Apple Git also treats group-only execute as 100644"
    );

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

/// Corrupted loose object content cannot be admitted merely because cat-file exits successfully.
#[tokio::test]
async fn blob_bytes_are_verified_against_the_requested_object_identity() {
    let fixture = GitFixture::new();
    let requested = fixture.git(["rev-parse", "HEAD:unstaged.txt"]);
    let requested = std::str::from_utf8(&requested.stdout).unwrap().trim();
    fixture.write(b".git/tampered-input", b"tampered\n");
    let replacement = fixture.git([
        "hash-object",
        "-w",
        "--no-filters",
        "--",
        ".git/tampered-input",
    ]);
    let replacement = std::str::from_utf8(&replacement.stdout).unwrap().trim();
    let object_path = fixture
        .root
        .join(".git/objects")
        .join(&requested[..2])
        .join(&requested[2..]);
    let replacement_path = fixture
        .root
        .join(".git/objects")
        .join(&replacement[..2])
        .join(&replacement[2..]);
    let corrupted = fs::read(replacement_path).unwrap();
    fs::remove_file(&object_path).unwrap();
    fs::write(object_path, corrupted).unwrap();
    assert_eq!(
        fixture.git(["cat-file", "blob", requested]).stdout,
        b"tampered\n",
        "Apple Git cat-file does not check the requested blob hash"
    );
    fixture.install_malicious_helpers();
    let mut runner = Runner::default();
    assert_eq!(
        collect(&fixture, DiffMode::Head, &mut runner).await,
        Err(GitError::ObjectHashMismatch)
    );
    assert!(!fixture.sentinel.exists());
    assert!(runner.directories.iter().all(|path| !path.exists()));
}

/// The fixed no-filter verifier uses the repository object format for a real SHA-256 repository.
#[tokio::test]
async fn sha256_blob_verification_uses_the_repository_format() {
    let fixture = GitFixture::unborn();
    fs::remove_dir_all(fixture.root.join(".git")).unwrap();
    fixture.git(["init", "--quiet", "--object-format=sha256"]);
    fixture.git(["config", "user.email", "snapshot@example.invalid"]);
    fixture.git(["config", "user.name", "Snapshot Fixture"]);
    fixture.write(b"tracked.txt", b"base\n");
    fixture.git(["add", "tracked.txt"]);
    fixture.git(["commit", "--quiet", "-m", "sha256 baseline"]);
    fixture.write(b"tracked.txt", b"working\n");
    fixture.install_malicious_helpers();
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert_eq!(
        snapshot.paths()[0].status().objects().unwrap()[0]
            .as_ref()
            .unwrap()
            .as_str()
            .len(),
        64
    );
    assert!(!fixture.sentinel.exists());
}

/// Partial conflict stages remain exact and do not become a fabricated UU status.
#[tokio::test]
async fn conflicts_retain_actual_stage_modes_and_objects() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let fixture = GitFixture::new();
    let base = fixture.git(["rev-parse", "HEAD:unstaged.txt"]);
    let base = std::str::from_utf8(&base.stdout).unwrap().trim();
    let theirs = fixture.git(["rev-parse", ":staged.txt"]);
    let theirs = std::str::from_utf8(&theirs.stdout).unwrap().trim();
    let mut child = Command::new(GIT)
        .env_clear()
        .arg("-C")
        .arg(&fixture.root)
        .args(["update-index", "--index-info"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(format!("0 {}\tunstaged.txt\n100644 {base} 1\tunstaged.txt\n100755 {theirs} 3\tunstaged.txt\n", "0".repeat(40)).as_bytes()).unwrap();
    assert!(child.wait().unwrap().success());
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    let conflict = &snapshot.status().conflicts()[0];
    assert_eq!(conflict.status(), None);
    let comparison = snapshot.comparison().clone();
    let result = compose_diff(
        snapshot.scope(),
        &comparison,
        snapshot.clone(),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.freshness(), DiffFreshness::Unknown);
    let stages = conflict.conflict_stages();
    assert_eq!(stages.len(), 2);
    assert_eq!(
        (
            stages[0].stage(),
            stages[0].mode(),
            stages[0].object().as_str()
        ),
        (1, 0o100644, base)
    );
    assert_eq!(
        (
            stages[1].stage(),
            stages[1].mode(),
            stages[1].object().as_str()
        ),
        (3, 0o100755, theirs)
    );
    assert!(
        snapshot
            .paths()
            .iter()
            .all(|path| path.status().path().as_os_str().as_bytes() != b"unstaged.txt")
    );
}

/// Git-listed links are rejected; Apple Git omits FIFOs, while direct source reads reject them without blocking.
#[tokio::test]
async fn untracked_symlink_and_special_entries_are_explicitly_unsupported() {
    use std::{
        ffi::CString,
        os::unix::{ffi::OsStrExt, fs::symlink},
    };
    let fixture = GitFixture::new();
    let entry = fixture.root.join("untracked-special");
    symlink("staged.txt", &entry).unwrap();
    assert_eq!(
        collect(&fixture, DiffMode::Head, &mut Runner::default()).await,
        Err(GitError::UnsupportedSnapshot)
    );
    fs::remove_file(&entry).unwrap();
    let path = CString::new(entry.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a live NUL-terminated test-owned pathname and mode is a bounded Unix permission value.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert!(
        snapshot
            .status()
            .untracked()
            .iter()
            .all(|entry| entry.path().as_os_str().as_bytes() != b"untracked-special"),
        "Apple Git does not list untracked FIFOs; this is not a complete filesystem inventory"
    );
    let authority = authority_for(&fixture);
    assert_eq!(
        agent_ide::workspace::observation::read_authorized_source(
            authority.worktree(),
            std::path::Path::new("untracked-special"),
            agent_ide::workspace::observation::SourceReadLimits::new(4096, 1024).unwrap()
        ),
        Err(agent_ide::workspace::observation::ObservationError::NotRegularFile)
    );
}

/// Dropping a borrowed wait preserves ownership; a reaper handoff retains scratch until actual wait.
#[tokio::test]
async fn cancellation_handoff_keeps_private_files_until_matching_reap() {
    use std::time::Duration;
    let fixture = GitFixture::new();
    let wrapper = support::git_wrapper(&fixture, "slow-git", "exec /bin/sleep 5");
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent = SnapshotIntent::compare(scope, &wrapper, b"old\n", b"new\n").unwrap();
    let directory = intent.snapshot_directory().unwrap().to_owned();
    let (mut child, mut admissions) = support::launch_intent(&intent, &wrapper, 4096).unwrap();
    assert!(
        child.take_process_identity().is_none(),
        "launch token was transferred exactly once"
    );
    assert!(
        intent.command().is_err(),
        "private argv cannot be exported twice"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            child.wait(Duration::from_secs(2))
        )
        .await
        .is_err()
    );
    assert!(directory.exists());
    let (release, transferred) = tokio::sync::oneshot::channel();
    let reaper = tokio::spawn(async move {
        transferred.await.unwrap();
        child
            .cancel_bounded(Duration::from_millis(10), Duration::from_secs(2))
            .await
            .unwrap();
        let completed = child
            .reap(Duration::from_secs(2), Duration::from_secs(2))
            .await
            .unwrap();
        intent.acknowledge_reap(&completed.evidence).unwrap();
        admissions.release_reaped(completed.settlement).unwrap();
        drop(intent);
    });
    assert!(
        directory.exists(),
        "handoff owns the guard before cancellation completes"
    );
    release.send(()).unwrap();
    reaper.await.unwrap();
    assert!(!directory.exists());
}

/// Actual wait evidence for another launch cannot release this still-running child's private files.
#[tokio::test]
async fn foreign_reap_identity_cannot_release_scratch() {
    use agent_ide::workspace::git::snapshot::SnapshotRunner;
    use std::time::Duration;
    let fixture = GitFixture::new();
    let wrapper = support::git_wrapper(&fixture, "other-slow-git", "exec /bin/sleep 5");
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent = SnapshotIntent::compare(scope.clone(), &wrapper, b"a\n", b"b\n").unwrap();
    let directory = intent.snapshot_directory().unwrap().to_owned();
    let (mut child, mut admissions) = support::launch_intent(&intent, &wrapper, 4096).unwrap();
    let other = SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"a\n", b"b\n").unwrap();
    let foreign = Runner::default().run(other).await.unwrap();
    assert_eq!(
        intent.acknowledge_reap(&foreign),
        Err(GitError::IncompleteIdentity)
    );
    assert!(directory.exists());
    child
        .cancel_bounded(Duration::from_millis(10), Duration::from_secs(2))
        .await
        .unwrap();
    let completed = child
        .reap(Duration::from_secs(2), Duration::from_secs(2))
        .await
        .unwrap();
    intent.acknowledge_reap(&completed.evidence).unwrap();
    admissions.release_reaped(completed.settlement).unwrap();
    drop(intent);
    assert!(!directory.exists());
}

/// Issued argv without actual-child evidence remains quarantined on drop instead of guessing no effect.
#[test]
fn uncertain_issued_snapshot_drop_quarantines_files() {
    let fixture = GitFixture::new();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent = SnapshotIntent::compare(scope, std::path::Path::new(GIT), b"a", b"b").unwrap();
    let directory = intent.snapshot_directory().unwrap().to_owned();
    let _unused_command = intent.command().unwrap();
    drop(intent);
    assert!(directory.exists());
    // This fixture never spawns a child; remove its deliberately unstarted quarantine explicitly.
    fs::remove_dir_all(directory).unwrap();
}

/// Unchanged paths must not cost per-path sandboxed blob commands: worktree sides are classified
/// by one batched no-filter hash command, and blobs are fetched only for changed comparison sides
/// (T27B: every managed sandbox replay costs a fixed multi-second startup per child).
#[tokio::test]
async fn unchanged_paths_cost_no_per_path_blob_commands() {
    let fixture = GitFixture::new();
    // The fixture's only worktree change is unstaged.txt; make it clean, add bulk clean paths,
    // then leave exactly one tracked path changed.
    fixture.write(b"unstaged.txt", b"base\n");
    for n in 0..14 {
        fixture.write(
            format!("bulk-{n:02}.txt").as_bytes(),
            format!("bulk {n}\n").as_bytes(),
        );
    }
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "bulk baseline"]);
    fixture.write(b"bulk-07.txt", b"changed\n");
    let mut runner = Runner::default();
    let snapshot = collect(&fixture, DiffMode::Unstaged, &mut runner)
        .await
        .unwrap();
    // 12 metadata + 1 worktree hash + 1 cat-file + 1 blob verification + 1 comparison.
    assert!(
        runner.operations <= 16,
        "sandboxed spawns must stay bounded, got {}",
        runner.operations
    );
    assert_eq!(runner.blobs, 1);
    assert_eq!(runner.comparisons, 1);
    assert_eq!(snapshot.paths().len(), 1);
    assert_eq!(
        snapshot.paths()[0].status().path(),
        std::path::Path::new("bulk-07.txt")
    );
    assert_eq!(snapshot.paths()[0].status().status(), Some(*b".M"));
    assert!(!snapshot.paths()[0].patch().is_empty());
    assert!(runner.directories.iter().all(|dir| !dir.exists()));
}

/// Six hundred clean tracked paths consume metadata only; changed paths alone enter byte caps.
#[tokio::test]
async fn large_repository_captures_changed_and_proven_empty_diffs() {
    let fixture = GitFixture::unborn();
    fixture.git(["config", "user.email", "large@example.invalid"]);
    fixture.git(["config", "user.name", "Large Fixture"]);
    for n in 0..600 {
        fixture.write(format!("tracked-{n:03}.txt").as_bytes(), b"base\n");
    }
    fixture.git(["add", "."]);
    fixture.git(["commit", "--quiet", "-m", "large baseline"]);
    let clean = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    let clean_result = compose_diff(
        clean.scope(),
        clean.comparison(),
        clean.clone(),
        DiffSelectionBudget::default(),
    );
    assert_eq!(clean_result.state(), DiffResultState::Ready);
    assert!(clean_result.tracked().is_empty());
    for n in 0..300 {
        fixture.write(format!("tracked-{n:03}.txt").as_bytes(), b"base\n");
    }
    let touched = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert!(
        touched.paths().is_empty(),
        "stat-only touches are not changed paths"
    );
    fixture.write(b"tracked-123.txt", b"changed\n");
    fixture.write(b"new.txt", b"untracked\n");
    let mut runner = Runner::default();
    let changed = collect(&fixture, DiffMode::Head, &mut runner)
        .await
        .unwrap();
    let result = compose_diff(
        changed.scope(),
        changed.comparison(),
        changed.clone(),
        DiffSelectionBudget::default(),
    );
    assert_eq!(result.state(), DiffResultState::Ready);
    assert_eq!(result.tracked().len(), 1);
    assert_eq!(result.tracked()[0].path(), Path::new("tracked-123.txt"));
    assert_eq!(result.untracked().len(), 1);
    assert_eq!(runner.comparisons, 1);
    for n in 0..257 {
        fixture.write(format!("tracked-{n:03}.txt").as_bytes(), b"over cap\n");
    }
    assert_eq!(
        collect(&fixture, DiffMode::Head, &mut Runner::default()).await,
        Err(GitError::EvidenceTooLarge)
    );
}

/// A same-length rewrite at the index mtime is captured even when Git's stat cache is racy.
#[tokio::test]
async fn racy_index_timestamp_still_reports_a_same_length_rewrite() {
    let fixture = GitFixture::unborn();
    fixture.git(["config", "user.email", "racy@example.invalid"]);
    fixture.git(["config", "user.name", "Racy Fixture"]);
    fixture.write(b"racy.txt", b"base\n");
    fixture.git(["add", "racy.txt"]);
    fixture.git(["commit", "--quiet", "-m", "baseline"]);
    let index = fixture.git(["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let index = PathBuf::from(String::from_utf8(index.stdout).unwrap().trim());
    let mtime = fs::metadata(index).unwrap().modified().unwrap();
    fixture.write(b"racy.txt", b"next\n");
    fs::File::options()
        .write(true)
        .open(fixture.root.join("racy.txt"))
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(mtime))
        .unwrap();
    let snapshot = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert_eq!(snapshot.paths().len(), 1);
    assert_eq!(snapshot.paths()[0].status().path(), Path::new("racy.txt"));
}

/// Git prints assume-unchanged plus skip-worktree flags in hex; both still receive byte capture.
#[tokio::test]
async fn combined_index_flags_do_not_hide_worktree_changes() {
    let fixture = GitFixture::unborn();
    fixture.git(["config", "user.email", "flags@example.invalid"]);
    fixture.git(["config", "user.name", "Flags Fixture"]);
    fixture.write(b"flagged.txt", b"base\n");
    fixture.git(["add", "flagged.txt"]);
    fixture.git(["commit", "--quiet", "-m", "baseline"]);
    fixture.git(["update-index", "--assume-unchanged", "flagged.txt"]);
    fixture.git(["update-index", "--skip-worktree", "flagged.txt"]);
    let clean = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert!(clean.paths().is_empty());
    fixture.write(b"flagged.txt", b"next\n");
    let changed = collect(&fixture, DiffMode::Head, &mut Runner::default())
        .await
        .unwrap();
    assert_eq!(changed.paths().len(), 1);
    assert_eq!(changed.paths()[0].status().path(), Path::new("flagged.txt"));
}

/// Real truncated cat-file output is rejected despite successful exit and actual wait identity.
#[tokio::test]
async fn real_truncated_blob_evidence_is_not_complete() {
    use std::time::Duration;
    let fixture = GitFixture::new();
    fixture.write(b".git/large-blob", &vec![b'x'; 8192]);
    let output = fixture.git(["hash-object", "-w", "--no-filters", "--", ".git/large-blob"]);
    let oid =
        agent_ide::workspace::git::GitObjectId::parse(output.stdout.strip_suffix(b"\n").unwrap())
            .unwrap()
            .unwrap();
    let scope = agent_ide::workspace::git::GitScope::from_authority(
        &authority_for(&fixture),
        DiffMode::Head,
    );
    let intent = SnapshotIntent::blob(scope, std::path::Path::new(GIT), &oid).unwrap();
    let (child, mut admissions) =
        support::launch_intent(&intent, std::path::Path::new(GIT), 128).unwrap();
    let completed = child
        .reap(Duration::from_secs(2), Duration::from_secs(2))
        .await
        .unwrap();
    assert!(completed.evidence.stdout().truncated);
    assert_eq!(completed.evidence.status().code(), Some(0));
    admissions.release_reaped(completed.settlement).unwrap();
    assert_eq!(
        intent.accept(completed.evidence),
        Err(GitError::EvidenceTooLarge)
    );
}

/// One runner that refuses every path authorization, as an Execution owner must be able to
/// when the live host profile denies a path (T36B). It forwards commands to the shared
/// harness — capture legitimately learns the changed paths through sandboxed Git metadata
/// commands first — while recording whether any scratch-writing intent ever ran.
struct RefusingRunner {
    /// Shared process harness running only the sandboxed Git metadata commands.
    inner: Runner,
    /// Count of intents that would have written captured bytes into a scratch file.
    scratch_intents: usize,
    /// Number of initial path proofs permitted before the fixture refuses one.
    allowed_proofs: usize,
    /// Path proofs requested by the collector so far.
    proofs: usize,
}

/// A clean tracked path still needs a live read proof before either native metadata probe.
#[tokio::test]
async fn clean_tree_refuses_unproven_metadata_path() {
    let fixture = GitFixture::unborn();
    fixture.git(["config", "user.email", "proof@example.invalid"]);
    fixture.git(["config", "user.name", "Proof Fixture"]);
    fixture.write(b"clean.txt", b"base\n");
    fixture.git(["add", "clean.txt"]);
    fixture.git(["commit", "--quiet", "-m", "baseline"]);
    for allowed_proofs in [0, 1] {
        let mut runner = RefusingRunner {
            inner: Runner::default(),
            scratch_intents: 0,
            allowed_proofs,
            proofs: 0,
        };
        assert_eq!(
            collect_snapshot(
                &authority_for(&fixture),
                Path::new(GIT),
                DiffMode::Head,
                1,
                "clean-proof",
                BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
                &mut runner,
            )
            .await,
            Err(GitError::UnsupportedSnapshot)
        );
        assert_eq!(runner.proofs, allowed_proofs + 1);
        assert_eq!(runner.scratch_intents, 0);
    }
}

impl SnapshotRunner for RefusingRunner {
    /// Forwards the intent, counting any scratch-writing command that must never run.
    async fn run(
        &mut self,
        intent: SnapshotIntent,
    ) -> Result<agent_ide::execution::CapturedProcessEvidence, GitError> {
        if intent.snapshot_directory().is_some() {
            self.scratch_intents += 1;
        }
        self.inner.run(intent).await
    }

    /// Refuses after the configured count, like an unprovable path under a narrowed profile.
    async fn authorize_read_path(&mut self, _path: &Path) -> Result<(), GitError> {
        self.proofs += 1;
        if self.proofs > self.allowed_proofs {
            Err(GitError::UnsupportedSnapshot)
        } else {
            Ok(())
        }
    }
}

/// A refused path authorization fails the whole capture attempt before any native byte is
/// read, so denied content can never reach a scratch file, a blob hash, or a cached page.
#[tokio::test]
async fn refused_path_authorization_fails_the_capture_before_any_native_read() {
    let fixture = GitFixture::new();
    fixture.write(b"main.txt", b"proven bytes\n");
    fixture.git(["add", "."]);
    fixture.git([
        "-c",
        "user.email=t36b@example",
        "-c",
        "user.name=t36b",
        "commit",
        "-m",
        "base",
    ]);
    fixture.write(b"main.txt", b"changed proven bytes\n");
    fixture.write(b"untracked.txt", b"untracked bytes\n");
    for mode in [DiffMode::Head, DiffMode::Staged, DiffMode::Unstaged] {
        let mut runner = RefusingRunner {
            inner: Runner::default(),
            scratch_intents: 0,
            allowed_proofs: 0,
            proofs: 0,
        };
        let result = collect_snapshot(
            &authority_for(&fixture),
            Path::new(GIT),
            mode,
            1,
            "snapshot-operation",
            BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
            &mut runner,
        )
        .await;
        assert_eq!(result, Err(GitError::UnsupportedSnapshot));
        assert_eq!(
            runner.scratch_intents, 0,
            "no scratch-writing command may run once a path authorization is refused"
        );
        assert!(
            runner.inner.directories.is_empty(),
            "no scratch directory may even be created"
        );
        assert_eq!(runner.inner.comparisons, 0, "no private comparison may run");
    }
}

/// One runner that authorizes exactly the paths in its allow-set and refuses every other,
/// as an Execution owner must when the live host profile proves some paths and denies others
/// (T36B-r, review finding 5). Scratch-writing intents are counted, so the test can prove a
/// refused path's bytes never even reach a scratch file.
struct MixedRunner {
    /// Shared process harness running only the sandboxed Git metadata commands.
    inner: Runner,
    /// Worktree-relative paths whose authorization the live profile could prove.
    allowed: BTreeSet<String>,
    /// Count of intents that would have written captured bytes into a scratch file.
    scratch_intents: usize,
}

impl SnapshotRunner for MixedRunner {
    /// Forwards the intent, counting any scratch-writing command.
    async fn run(
        &mut self,
        intent: SnapshotIntent,
    ) -> Result<agent_ide::execution::CapturedProcessEvidence, GitError> {
        if intent.snapshot_directory().is_some() {
            self.scratch_intents += 1;
        }
        self.inner.run(intent).await
    }

    /// Proves only the allow-set; every other path is unprovable under the live profile.
    async fn authorize_read_path(&mut self, path: &Path) -> Result<(), GitError> {
        if self.allowed.contains(&path.to_string_lossy().into_owned()) {
            Ok(())
        } else {
            Err(GitError::UnsupportedSnapshot)
        }
    }
}

/// A mixed runner — some paths proven, others denied — discloses only proven bytes: a
/// capture whose union still contains one refused path fails before any scratch-writing
/// intent or private comparison runs, so the refused path's bytes reach neither scratch
/// files nor output, while the same mixed runner lets the allowed path's real bytes flow
/// through the whole pipeline once the refused path has left the capture's union entirely.
#[tokio::test]
async fn mixed_path_authorization_discloses_only_proven_bytes() {
    let fixture = GitFixture::new();
    fixture.write(b"allowed.txt", b"base\n");
    fixture.write(b"secret.txt", b"base\n");
    fixture.git(["add", "."]);
    fixture.git([
        "-c",
        "user.email=t36b@example",
        "-c",
        "user.name=t36b",
        "commit",
        "--quiet",
        "-m",
        "base",
    ]);
    fixture.write(b"allowed.txt", b"allowed proven bytes\n");
    fixture.write(b"secret.txt", b"secret denied bytes\n");
    // Keep both changes unstaged so the working tree holds the refused bytes.
    // The proven set covers every fixture path except the refused one, including the
    // fixture's own untracked entries (their names are authorized before any capture).
    let allowed = [
        "allowed.txt",
        "staged.txt",
        "unstaged.txt",
        "special space\n-leading.txt",
        "untracked space\n-leading.txt",
        // The fixture's non-UTF-8 untracked name, spelled exactly as the collector's
        // lossy rendering of it spells.
        "untracked-non-utf8-\u{FFFD}.txt",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<BTreeSet<_>>();

    // The union still contains the refused path: the capture fails before any native byte
    // is read — neither path's bytes reach scratch files or output.
    let mut runner = MixedRunner {
        inner: Runner::default(),
        allowed: allowed.clone(),
        scratch_intents: 0,
    };
    let result = collect_snapshot(
        &authority_for(&fixture),
        Path::new(GIT),
        DiffMode::Unstaged,
        1,
        "snapshot-operation",
        BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
        &mut runner,
    )
    .await;
    assert_eq!(
        result,
        Err(GitError::UnsupportedSnapshot),
        "one refused union path must fail the whole capture attempt"
    );
    assert_eq!(
        runner.scratch_intents, 0,
        "no scratch-writing command may run, so the refused bytes never reach disk"
    );
    assert!(
        runner.inner.directories.is_empty(),
        "no scratch directory may even be created"
    );
    assert_eq!(runner.inner.comparisons, 0, "no private comparison may run");

    // Removing the refused path from the repository entirely (deletion committed) leaves a
    // union of proven paths only: the same mixed runner captures, the allowed path's real
    // working-tree bytes reach the rendered evidence, and the refused bytes are nowhere in it.
    fixture.git(["rm", "--quiet", "--force", "secret.txt"]);
    fixture.git(["commit", "--quiet", "-m", "drop secret"]);
    let mut runner = MixedRunner {
        inner: Runner::default(),
        allowed,
        scratch_intents: 0,
    };
    let snapshot = collect_snapshot(
        &authority_for(&fixture),
        Path::new(GIT),
        DiffMode::Unstaged,
        1,
        "snapshot-operation",
        BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
        &mut runner,
    )
    .await
    .unwrap_or_else(|error| panic!("a union of proven paths must capture: {error:?}"));
    let allowed = snapshot
        .paths()
        .iter()
        .find(|path| path.status().path() == Path::new("allowed.txt"))
        .expect("the allowed changed path must be captured");
    let patch = std::str::from_utf8(allowed.patch()).unwrap();
    assert!(patch.contains("allowed proven bytes"), "{patch}");
    for path in snapshot.paths() {
        assert!(
            !path
                .patch()
                .windows(b"secret".len())
                .any(|part| part == b"secret"),
            "refused content must not appear anywhere in the rendered evidence"
        );
    }
}
