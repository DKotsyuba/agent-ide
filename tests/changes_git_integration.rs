//! Real local-Git coverage for the Workspace-to-Changes diff boundary.

use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::{OsStrExt, OsStringExt},
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
    changes::{DiffResultState, DiffSelectionBudget, compose_diff},
    workspace::{
        authority::{ActivationRequest, AuthorityRegistry, WorktreeRef},
        git::{
            BaselineContext, BaselineCoverage, DiffMode, GitError, GitReadIntent, GitReadQuery,
            GitStatus, RawGitEvidence, comparison_from_evidence,
        },
    },
};
use serde_json::json;

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);
const GIT: &str = "/usr/bin/git";

/// Owns one recoverable local Git worktree and removes only that directory on drop.
struct GitFixture {
    /// Exact worktree root created for this test instance.
    root: PathBuf,
    /// Marker written only if a configured malicious Git helper executes.
    sentinel: PathBuf,
    /// Whether this Darwin filesystem accepted the attempted non-UTF-8 fixture filename.
    non_utf8_supported: bool,
}

impl GitFixture {
    /// Creates a committed repository with separately staged, unstaged, and raw-path changes.
    fn new() -> Self {
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
        fixture.install_malicious_helpers();
        fixture
    }

    /// Creates an otherwise empty repository for the explicit unborn-HEAD platform check.
    fn unborn() -> Self {
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
    fn write(&self, relative: &[u8], bytes: &[u8]) {
        fs::write(self.root.join(OsString::from_vec(relative.to_vec())), bytes)
            .expect("fixture path is writable");
    }

    /// Runs a local-only setup command against this fixture with no inherited user configuration.
    fn git<const N: usize>(&self, args: [&str; N]) -> Output {
        self.git_os(args.map(OsString::from))
    }

    /// Runs one setup command whose arguments may contain raw Unix bytes.
    fn git_os<const N: usize>(&self, args: [OsString; N]) -> Output {
        let output = base_git(&self.root)
            .args(args)
            .output()
            .expect("Git starts");
        assert!(output.status.success(), "Git setup failed: {output:?}");
        output
    }

    /// Installs local-only fsmonitor, external-diff, and textconv sentinels that must never run.
    fn install_malicious_helpers(&self) {
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

/// Executes the exact fixed Workspace query selected by the supplied intent.
fn run_fixed_query(fixture: &GitFixture, intent: &GitReadIntent) -> Output {
    let controlled = format!(
        "{:?}",
        intent.controlled_command().expect("fixed command builds")
    );
    for required in ["core.fsmonitor=false", "--no-pager"] {
        assert!(
            controlled.contains(required),
            "intent omits {required}: {controlled}"
        );
    }
    if matches!(
        intent.query(),
        GitReadQuery::HeadDiff | GitReadQuery::StagedDiff | GitReadQuery::UnstagedDiff
    ) {
        for required in [
            "--no-ext-diff",
            "--no-textconv",
            "--full-index",
            "--patch",
            "--no-color",
        ] {
            assert!(
                controlled.contains(required),
                "diff intent omits {required}: {controlled}"
            );
        }
    }
    let mut command = base_git(&fixture.root);
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "color.ui=false",
        "--no-pager",
    ]);
    match intent.query() {
        GitReadQuery::Status => {
            command.args(["status", "--porcelain=v2", "-z", "--untracked-files=all"]);
        }
        GitReadQuery::HeadIdentity => {
            command.args(["rev-parse", "--verify", "--quiet", "HEAD", "--"]);
        }
        GitReadQuery::IndexState => {
            command.args(["ls-files", "--stage", "-z", "--"]);
        }
        GitReadQuery::HeadDiff => {
            command.args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                "--patch",
                "--no-color",
                "HEAD",
                "--",
            ]);
        }
        GitReadQuery::StagedDiff => {
            command.args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                "--patch",
                "--no-color",
                "--cached",
                "--",
            ]);
        }
        GitReadQuery::UnstagedDiff => {
            command.args([
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--full-index",
                "--patch",
                "--no-color",
                "--",
            ]);
        }
    }
    let output = command.output().expect("fixed Git query starts");
    assert!(
        output.status.success() || intent.query() == GitReadQuery::HeadIdentity,
        "fixed Git query failed unexpectedly: {output:?}"
    );
    assert!(
        !fixture.sentinel.exists(),
        "configured Git helper ran despite fixed read-only arguments"
    );
    output
}

/// Establishes one live Workspace authority whose worktree paths are the fixture's exact raw paths.
fn authority_for(fixture: &GitFixture) -> agent_ide::workspace::authority::AuthorityStamp {
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
        PathBuf::from(".git"),
        1,
    )
    .expect("fixture worktree is valid");
    let request = ActivationRequest::new("changes-real-git", invocation, active, worktree)
        .expect("activation request is valid");
    AuthorityRegistry::default()
        .activate(request)
        .expect("authority activates")
}

/// Converts a completed fixed query into Workspace raw evidence without assigning its contents a new meaning.
fn evidence(operation: &str, intent: &GitReadIntent, output: Output) -> RawGitEvidence {
    RawGitEvidence::new(
        operation,
        intent.scope().clone(),
        intent.query(),
        output.stdout,
        output.stderr,
        output.status.code(),
        false,
        false,
    )
    .expect("fixed output has bounded metadata")
}

/// Uses a complete baseline only as context, never as a comparison side.
fn baseline() -> BaselineContext {
    BaselineContext::new("real-git-session-baseline", BaselineCoverage::Complete)
        .expect("baseline context is bounded")
}

/// Confirms the real macOS Git boundary preserves comparison modes, raw paths, bounded hunks, and helper safety.
#[test]
fn real_git_workspace_evidence_composes_distinct_bounded_changes() {
    let fixture = GitFixture::new();
    let authority = authority_for(&fixture);
    let program = PathBuf::from(GIT);
    let status_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::Status).unwrap();
    let head_id_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::HeadIdentity).unwrap();
    let index_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::IndexState).unwrap();
    let head_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::HeadDiff).unwrap();
    let staged_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::StagedDiff).unwrap();
    let unstaged_intent =
        GitReadIntent::new(&authority, program, GitReadQuery::UnstagedDiff).unwrap();

    let status = GitStatus::from_evidence(&evidence(
        "status",
        &status_intent,
        run_fixed_query(&fixture, &status_intent),
    ))
    .expect("real porcelain-v2 -z parses");
    let raw_special = b"special space\n-leading.txt";
    assert!(
        status
            .tracked()
            .iter()
            .any(|entry| entry.path().as_os_str().as_bytes() == raw_special)
    );
    assert!(
        status.untracked().iter().any(|entry| {
            entry.path().as_os_str().as_bytes() == b"untracked space\n-leading.txt"
        })
    );
    if fixture.non_utf8_supported {
        assert!(status.untracked().iter().any(|entry| {
            entry.path().as_os_str().as_bytes() == b"untracked-non-utf8-\xff.txt"
        }));
    } else {
        eprintln!("platform unsupported: Darwin filesystem rejected a non-UTF-8 raw filename");
    }

    let head_identity = evidence(
        "head-id",
        &head_id_intent,
        run_fixed_query(&fixture, &head_id_intent),
    );
    let index = evidence(
        "index",
        &index_intent,
        run_fixed_query(&fixture, &index_intent),
    );
    let head_diff = evidence(
        "head-diff",
        &head_intent,
        run_fixed_query(&fixture, &head_intent),
    );
    let staged_diff = evidence(
        "staged-diff",
        &staged_intent,
        run_fixed_query(&fixture, &staged_intent),
    );
    let unstaged_diff = evidence(
        "unstaged-diff",
        &unstaged_intent,
        run_fixed_query(&fixture, &unstaged_intent),
    );

    let head = compose_diff(
        head_intent.scope(),
        &comparison_from_evidence(
            DiffMode::Head,
            &head_identity,
            &index,
            &head_diff,
            baseline(),
        )
        .unwrap(),
        status.clone(),
        head_diff,
        DiffSelectionBudget::default(),
    );
    let staged = compose_diff(
        staged_intent.scope(),
        &comparison_from_evidence(
            DiffMode::Staged,
            &head_identity,
            &index,
            &staged_diff,
            baseline(),
        )
        .unwrap(),
        status.clone(),
        staged_diff,
        DiffSelectionBudget::default(),
    );
    let unstaged = compose_diff(
        unstaged_intent.scope(),
        &comparison_from_evidence(
            DiffMode::Unstaged,
            &head_identity,
            &index,
            &unstaged_diff,
            baseline(),
        )
        .unwrap(),
        status.clone(),
        unstaged_diff,
        DiffSelectionBudget::default(),
    );

    for result in [&head, &staged, &unstaged] {
        assert_eq!(result.state(), DiffResultState::Ready);
        assert!(!result.selected_hunks().is_empty());
        assert!(
            result
                .selected_hunks()
                .iter()
                .all(|hunk| hunk.path().is_some())
        );
        assert!(
            result
                .selected_hunks()
                .iter()
                .all(|hunk| hunk.patch().starts_with(b"@@"))
        );
        assert_eq!(
            result.provenance().baseline_reference(),
            Some("real-git-session-baseline")
        );
    }
    assert_eq!(head.identities().left(), staged.identities().left());
    assert_eq!(staged.identities().right(), unstaged.identities().left());
    assert_ne!(head.identities().left(), head.identities().right());
    assert_ne!(staged.identities().left(), staged.identities().right());
    assert_ne!(unstaged.identities().left(), unstaged.identities().right());
    assert_ne!(head.identities().right(), unstaged.identities().right());

    let bounded_evidence = evidence(
        "head-overflow",
        &head_intent,
        run_fixed_query(&fixture, &head_intent),
    );
    let bounded = compose_diff(
        head_intent.scope(),
        &comparison_from_evidence(
            DiffMode::Head,
            &head_identity,
            &index,
            &bounded_evidence,
            baseline(),
        )
        .unwrap(),
        status,
        bounded_evidence,
        DiffSelectionBudget::bounded(0, 0),
    );
    assert_eq!(bounded.state(), DiffResultState::Incomplete);
    assert!(bounded.selected_hunks().is_empty());
    assert!(bounded.overflow_hunks() > 0);
    assert_eq!(
        bounded
            .detail_cursor()
            .expect("overflow has a cursor")
            .next_hunk(),
        0
    );
    assert!(
        !fixture.sentinel.exists(),
        "Changes must not execute Git helpers"
    );
}

/// Verifies Apple Git's fixed quiet HEAD query reports an unborn repository explicitly.
#[test]
fn real_git_unborn_head_is_explicit_not_an_empty_identity() {
    let fixture = GitFixture::unborn();
    let authority = authority_for(&fixture);
    let program = PathBuf::from(GIT);
    let head_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::HeadIdentity).unwrap();
    let index_intent =
        GitReadIntent::new(&authority, program.clone(), GitReadQuery::IndexState).unwrap();
    let working_intent =
        GitReadIntent::new(&authority, program, GitReadQuery::UnstagedDiff).unwrap();
    let head = evidence(
        "unborn-head",
        &head_intent,
        run_fixed_query(&fixture, &head_intent),
    );
    assert_eq!(
        head.exit_code(),
        Some(1),
        "Apple Git quiet missing HEAD exit changed"
    );
    let index = evidence(
        "unborn-index",
        &index_intent,
        run_fixed_query(&fixture, &index_intent),
    );
    let working = evidence(
        "unborn-working",
        &working_intent,
        run_fixed_query(&fixture, &working_intent),
    );
    assert_eq!(
        comparison_from_evidence(DiffMode::Unstaged, &head, &index, &working, baseline()),
        Err(GitError::UnbornHead)
    );
}

/// Reproduces the outstanding clean-filter execution gap in content-reading Git commands.
#[test]
#[ignore = "known unsafe clean-filter path; requires raw snapshot collector before enabling"]
fn content_queries_must_not_execute_repository_clean_filters() {
    let fixture = GitFixture::new();
    fixture.write(b".gitattributes", b"*.txt filter=evil\n");
    fixture.git_os([
        OsString::from("config"),
        OsString::from("--local"),
        OsString::from("filter.evil.clean"),
        fixture.root.join(".git/sentinel.sh").into_os_string(),
    ]);
    let intent = GitReadIntent::new(
        &authority_for(&fixture),
        PathBuf::from(GIT),
        GitReadQuery::HeadDiff,
    )
    .unwrap();
    run_fixed_query(&fixture, &intent);
    assert!(
        !fixture.sentinel.exists(),
        "repository clean filter executed"
    );
}
