//! Shared local fixtures for filter-free snapshot contract checks.
#![allow(dead_code)]

use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use agent_ide::{
    assistance::host_binding::{
        BindingStatus, CandidateInvocation, HostBindingGuard, parse_candidate,
        parse_channel_session, parse_hook_event,
    },
    workspace::authority::{ActivationRequest, AuthorityRegistry, WorktreeRef},
};
use serde_json::json;

/// Counter separating concurrent test-owned temporary worktrees.
static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
/// Exact Apple Git executable exercised by the integration fixtures.
pub const GIT: &str = "/usr/bin/git";

/// Owns one recoverable local Git worktree and removes only that directory on drop.
pub struct GitFixture {
    /// Exact worktree root created for this test instance.
    pub root: PathBuf,
    /// Marker written only if a configured malicious Git helper executes.
    pub sentinel: PathBuf,
    /// Whether this Darwin filesystem accepted the attempted non-UTF-8 fixture filename.
    pub non_utf8_supported: bool,
}

impl GitFixture {
    /// Creates a committed repository with separately staged, unstaged, and raw-path changes.
    pub fn new() -> Self {
        let root = unique_temp_path("changes-git");
        fs::create_dir_all(&root).expect("fixture root is creatable");
        let mut fixture = Self {
            sentinel: root.join("sentinel-ran"),
            root,
            non_utf8_supported: false,
        };
        fixture.git(["init", "--quiet"]);
        fixture.git([
            "config",
            "--local",
            "user.email",
            "changes-git@example.invalid",
        ]);
        fixture.git(["config", "--local", "user.name", "Changes Git Fixture"]);

        fixture.write(b"staged.txt", b"base\n");
        fixture.write(b"unstaged.txt", b"base\n");
        fixture.write(b"special space\n-leading.txt", b"base\n");
        fixture.git_os([
            OsString::from("add"),
            OsString::from("--"),
            OsString::from("."),
        ]);
        fixture.git(["commit", "--quiet", "-m", "fixture baseline"]);

        fixture.write(b"staged.txt", b"index change\n");
        fixture.git_os([
            OsString::from("add"),
            OsString::from("--"),
            OsString::from("staged.txt"),
        ]);
        fixture.write(b"unstaged.txt", b"working change\n");
        fixture.write(b"special space\n-leading.txt", b"special index change\n");
        fixture.git_os([
            OsString::from("add"),
            OsString::from("--"),
            OsString::from_vec(b"special space\n-leading.txt".to_vec()),
        ]);
        fixture.write(b"untracked space\n-leading.txt", b"untracked\n");
        fixture.non_utf8_supported = fs::write(
            fixture
                .root
                .join(OsString::from_vec(b"untracked-non-utf8-\xff.txt".to_vec())),
            b"untracked non-utf8\n",
        )
        .is_ok();

        fixture
    }

    /// Creates an otherwise empty repository for the explicit unborn-HEAD platform check.
    pub fn unborn() -> Self {
        let root = unique_temp_path("changes-git-unborn");
        fs::create_dir_all(&root).expect("fixture root is creatable");
        let fixture = Self {
            sentinel: root.join("sentinel-ran"),
            root,
            non_utf8_supported: false,
        };
        fixture.git(["init", "--quiet"]);
        fixture
    }

    /// Writes one raw Unix relative path without interpreting it as UTF-8.
    pub fn write(&self, relative: &[u8], bytes: &[u8]) {
        fs::write(self.root.join(OsString::from_vec(relative.to_vec())), bytes)
            .expect("fixture path is writable");
    }

    /// Runs a local-only setup command against this fixture with no inherited user configuration.
    pub fn git<const N: usize>(&self, args: [&str; N]) -> Output {
        self.git_os(args.map(OsString::from))
    }

    /// Runs one setup command whose arguments may contain raw Unix bytes.
    pub fn git_os<const N: usize>(&self, args: [OsString; N]) -> Output {
        let output = base_git(&self.root)
            .args(args)
            .output()
            .expect("Git starts");
        assert!(output.status.success(), "Git setup failed: {output:?}");
        output
    }

    /// Installs local-only fsmonitor, external-diff, and textconv sentinels that must never run.
    pub fn install_malicious_helpers(&self) {
        let helper = self.root.join(".git/sentinel.sh");
        fs::write(
            &helper,
            format!("#!/bin/sh\ntouch {}\n", self.sentinel.display()),
        )
        .expect("sentinel helper is writable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))
                .expect("sentinel helper is executable");
        }
        self.write(b".gitattributes", b"*.txt diff=evil\n");
        self.git_os([
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("core.fsmonitor"),
            helper.as_os_str().to_os_string(),
        ]);
        self.git_os([
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("diff.external"),
            helper.as_os_str().to_os_string(),
        ]);
        self.git_os([
            OsString::from("config"),
            OsString::from("--local"),
            OsString::from("diff.evil.textconv"),
            helper.as_os_str().to_os_string(),
        ]);
    }
}

impl Drop for GitFixture {
    /// Removes only the uniquely created fixture directory after its test completes.
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Returns a unique `/private/tmp` location without relying on test order or a global fixture path.
fn unique_temp_path(prefix: &str) -> PathBuf {
    let tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after epoch")
        .as_nanos();
    PathBuf::from(format!(
        "/private/tmp/{prefix}-{}-{tick}-{}",
        std::process::id(),
        NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Starts Git with the same cleared environment and literal worktree selection as the fixture needs.
fn base_git(root: &Path) -> Command {
    let mut command = Command::new(GIT);
    command
        .env_clear()
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(root);
    command
}

/// Establishes one live Workspace authority whose worktree paths are the fixture's exact raw paths.
pub fn authority_for(fixture: &GitFixture) -> agent_ide::workspace::authority::AuthorityStamp {
    let mut guard = HostBindingGuard::default();
    let channel = parse_channel_session(b"changes-real-git").expect("channel is valid");
    let hook = parse_hook_event(
        json!({"hook_event_name": "PreToolUse", "session_id": "real-git", "tool_use_id": "collect"})
            .to_string()
            .as_bytes(),
    )
    .expect("hook is valid");
    assert!(matches!(
        guard.observe_hook(hook, channel.clone()),
        BindingStatus::PreObserved
    ));
    let candidate: CandidateInvocation = parse_candidate(
        json!({"threadId": "real-git", "callId": "collect", "x-codex-turn-metadata": {"turn": "real"}})
            .as_object()
            .expect("candidate object"),
    )
    .expect("candidate is valid");
    let BindingStatus::Validated(invocation) = guard.establish_start(candidate, channel) else {
        panic!("binding must validate");
    };
    let active = guard
        .consume_active(invocation.binding_ref())
        .expect("binding is active");
    let worktree = WorktreeRef::from_discovery(
        fixture.root.clone(),
        fixture.root.clone(),
        fixture.root.join(".git"),
        1,
    )
    .expect("fixture worktree is valid");
    let request = ActivationRequest::new("changes-real-git", invocation, active, worktree)
        .expect("activation request is valid");
    AuthorityRegistry::default()
        .activate(request)
        .expect("authority activates")
}

use agent_ide::{
    execution::{
        Admission, AdmissionClass, AdmissionController, AdmissionLimits, CapturedProcessEvidence,
        LocalExecutionPolicy, OwnedChild, OwnerId, ValidatedExecutionRequest,
        ValidatedHostInvocation,
    },
    workspace::{
        authority::AuthorityStamp,
        git::{
            BaselineContext, BaselineCoverage, DiffMode, GitError,
            snapshot::{
                GitSnapshot, MAX_SNAPSHOT_BLOB_BYTES, SnapshotIntent, SnapshotRunner,
                collect_snapshot,
            },
        },
    },
};
use std::{collections::BTreeSet, time::Duration};

/// Actual Execution-owned local runner with controlled test-only disabled host evidence.
#[derive(Default)]
pub struct Runner {
    /// Optional fixture Git wrapper; absent uses the installed Git binary.
    pub program: Option<PathBuf>,
    /// Optional per-stream cap for attribute queries, used to force bounded truncation.
    pub attribute_output_cap: Option<usize>,
    /// All private directories observed while live, checked for cleanup after capture.
    pub directories: Vec<PathBuf>,
    /// Completed operations counted for deterministic before/after mutation injection.
    pub operations: usize,
    /// Optional mutation after every no-index child finishes, to exercise bounded retries.
    pub mutate_path: Option<PathBuf>,
    /// Whether one mutation rather than both attempts should happen.
    pub mutate_once: bool,
    /// Number of observed blob commands, used to prove OID deduplication.
    pub blobs: usize,
    /// Number of private no-filter hash batches, including worktree classification.
    pub hashes: usize,
    /// Number of successful differences exits; these must not be classified as command failures.
    pub different: usize,
    /// Number of completed no-index comparisons, excluding private blob-hash verification.
    pub comparisons: usize,
    /// Optional deterministic metadata mutation after a completed private comparison.
    pub after_compare: Option<Box<dyn FnMut() + Send>>,
    /// Optional durable source observation supplied to the exact-byte correlation boundary.
    pub source_observation: Option<agent_ide::workspace::observation::SourceObservation>,
}

impl SnapshotRunner for Runner {
    /// Returns a matching test-provided durable observation without inventing source revision/sequence.
    fn observation(
        &mut self,
        _authority: &AuthorityStamp,
        path: &Path,
    ) -> Option<agent_ide::workspace::observation::SourceObservation> {
        self.source_observation
            .as_ref()
            .filter(|obs| obs.path() == path)
            .cloned()
    }

    /// Test harness: these fixtures run without a host sandbox, so every path is provable.
    /// Production authorization lives in the Assistance product runner through Execution.
    async fn authorize_read_path(&mut self, _path: &Path) -> Result<(), GitError> {
        Ok(())
    }

    /// Admits and reaps the exact peer command; scratch remains owned through process completion.
    async fn run(&mut self, intent: SnapshotIntent) -> Result<CapturedProcessEvidence, GitError> {
        use std::os::unix::fs::PermissionsExt;
        self.hashes += usize::from(intent.label() == "hash-object");
        if let Some(dir) = intent.snapshot_directory() {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            for file in fs::read_dir(dir).unwrap() {
                assert_eq!(
                    file.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
            self.directories.push(dir.to_path_buf());
        } else if format!("{:?}", intent.command()).contains("cat-file") {
            self.blobs += 1;
        }
        let (child, mut admissions) = launch_intent(
            &intent,
            self.program.as_deref().unwrap_or(Path::new(GIT)),
            if intent.label() == "check-attr" {
                self.attribute_output_cap.unwrap_or(MAX_SNAPSHOT_BLOB_BYTES)
            } else {
                MAX_SNAPSHOT_BLOB_BYTES
            },
        )?;
        let completed = child
            .reap(Duration::from_secs(5), Duration::from_secs(5))
            .await
            .map_err(|_| GitError::IncompleteIdentity)?;
        intent.acknowledge_reap(&completed.evidence)?;
        admissions.release_reaped(completed.settlement).unwrap();
        let result = completed.evidence;
        self.operations += 1;
        if intent.is_comparison() {
            self.comparisons += 1;
            if let Some(hook) = &mut self.after_compare {
                hook();
            }
            self.different += usize::from(result.status().code() == Some(1));
            if let Some(path) = &self.mutate_path {
                fs::write(
                    path,
                    format!(
                        "mutated after operation {} at {:?}\n",
                        self.operations,
                        SystemTime::now()
                    ),
                )
                .unwrap();
                if self.mutate_once {
                    self.mutate_path = None;
                }
            }
        }
        Ok(result)
    }
}

/// Runs the complete typed collector through real Execution process admission and drain/reap.
pub async fn collect(
    fixture: &GitFixture,
    mode: DiffMode,
    runner: &mut Runner,
) -> Result<GitSnapshot, GitError> {
    capture_with_authority(&authority_for(fixture), mode, runner).await
}

/// Captures at one exact authority for scope invalidation and repeated collection tests.
pub async fn capture_with_authority(
    authority: &AuthorityStamp,
    mode: DiffMode,
    runner: &mut Runner,
) -> Result<GitSnapshot, GitError> {
    collect_snapshot(
        authority,
        Path::new(GIT),
        mode,
        1,
        "snapshot-operation",
        BaselineContext::new("baseline", BaselineCoverage::Partial).unwrap(),
        runner,
    )
    .await
}

/// Starts one exact private/metadata intent through real Execution and binds its transferred launch token.
/// The returned controller remains owned by the caller until it consumes the child's settlement proof.
pub fn launch_intent(
    intent: &SnapshotIntent,
    allowed_program: &Path,
    output_cap: usize,
) -> Result<(OwnedChild, AdmissionController), GitError> {
    let root = intent.scope().worktree().worktree_path();
    let invocation = ValidatedHostInvocation::from_verified_binding("snapshot-test").unwrap();
    let policy =
        LocalExecutionPolicy::new(BTreeSet::from([allowed_program.to_path_buf()]), 8192, 16)
            .unwrap();
    let request = ValidatedExecutionRequest::validate(
        invocation,
        intent.execution_authority()?,
        intent.command()?,
        &policy,
    )
    .unwrap();
    let mut admissions = AdmissionController::new(AdmissionLimits {
        total_running: 1,
        per_owner_running: 1,
        total_queued: 1,
        per_owner_queued: 1,
        interactive_burst: 1,
    })
    .unwrap();
    let Admission::Granted(lease) = admissions.submit(
        OwnerId::new("snapshot").unwrap(),
        AdmissionClass::Interactive,
    ) else {
        panic!("bounded runner is admitted")
    };
    let mut child = OwnedChild::spawn_captured(
        &request,
        lease,
        None,
        Path::new("/usr/bin/false"),
        output_cap,
    )
    .map_err(|_| GitError::IncompleteIdentity)?;
    if intent.snapshot_directory().is_some() {
        intent.bind_process(
            child
                .take_process_identity()
                .ok_or(GitError::IncompleteIdentity)?,
        )?;
    }
    Ok((child, admissions))
}

/// Writes a trusted test-only executable wrapper under this disposable fixture's Git admin directory.
pub fn git_wrapper(fixture: &GitFixture, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture.root.join(".git").join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}
