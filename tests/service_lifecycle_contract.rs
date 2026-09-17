//! Executable EYES-r1 §2 rendezvous/spawn/adopt lifecycle contract for the shared managed Claude
//! daemon: repository-keyed sharing across worktrees, start-race convergence to one lock holder,
//! and daemon persistence across an owning MCP process's own stdio EOF.

use std::{
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::app::{DoctorLockState, DoctorStatus, doctor_report};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Distinguishes temporary fixture paths across concurrently running scenarios in this process.
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// Returns a fresh, unique absolute temp path for one fixture element; nothing is created here.
fn unique_path(label: &str) -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!(
            "agent-ide-service-lifecycle-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
}

/// Runs one fixed local Git subcommand against `root` with all user/system configuration excluded.
fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates one committed real Git repository at a fresh temp root and returns its path.
fn init_repo() -> PathBuf {
    let root = unique_path("repo");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("tracked.txt"), "base\n").unwrap();
    git(&root, &["init", "--quiet"]);
    git(&root, &["config", "user.email", "fixture@example.invalid"]);
    git(&root, &["config", "user.name", "Service Lifecycle Fixture"]);
    git(&root, &["add", "--", "."]);
    git(&root, &["commit", "--quiet", "-m", "fixture"]);
    root
}

/// Adds one linked worktree of `repo` on a fresh branch at a fresh temp path, and returns it.
fn add_worktree(repo: &Path, branch: &str) -> PathBuf {
    let worktree = unique_path(&format!("worktree-{branch}"));
    git(
        repo,
        &["worktree", "add", "-b", branch, worktree.to_str().unwrap()],
    );
    worktree
}

/// Reproduces the public deterministic rendezvous formula (EYES-r1 §2) for black-box assertions.
///
/// The key is `candidate`'s canonical git common directory, independently rediscovered through the
/// real `git` binary; a `candidate` outside any repository falls back to itself, exactly like the
/// product code. This never inspects product internals, only its documented external contract.
fn expected_runtime_path(candidate: &Path) -> PathBuf {
    let candidate = std::fs::canonicalize(candidate).unwrap();
    let output = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(&candidate)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .unwrap();
    let key = if output.status.success() {
        std::fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim_end())
            .unwrap_or_else(|_| candidate.clone())
    } else {
        candidate
    };
    let hash = blake3::hash(key.as_os_str().as_bytes());
    std::fs::canonicalize("/private/tmp")
        .unwrap()
        .join(format!("ai-r-{}", &hash.to_hex().as_str()[..16]))
}

/// Sends SIGTERM to the exact process holding a shared daemon's runtime lock, if any.
///
/// A shared daemon deliberately outlives every MCP process's own EOF, so a test that causes one to
/// be spawned must reap it explicitly. `lsof` is asked for the specific lock file's current holder
/// only; this never pattern-kills by process name or command line.
fn terminate_shared_daemon(runtime: &Path) {
    let Ok(output) = std::process::Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg(runtime.join("agent-ide.lock"))
        .output()
    else {
        return;
    };
    for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if let Ok(pid) = pid.parse::<libc::pid_t>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// Guarantees a shared daemon this test caused to be spawned is reaped, even if an assertion
/// later in the same test panics, so a failing test cannot leak a long-lived orphan process.
struct DaemonGuard(PathBuf);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        terminate_shared_daemon(&self.0);
    }
}

/// Counts the exact processes currently holding a shared daemon's runtime lock open.
///
/// Used only to prove a start race converges to a single lock holder; reads `lsof`, never any
/// process listing filtered by name or command line.
fn lock_holder_count(runtime: &Path) -> usize {
    let Ok(output) = std::process::Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg(runtime.join("agent-ide.lock"))
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .count()
}

/// Writes one minimal valid Claude launcher template accepting `candidate`, and returns its path.
///
/// This is wiring proof only: it satisfies [`agent_ide::assistance::launcher::LauncherConfig`]
/// verification and the strict test-only
/// [`agent_ide::assistance::claude_worker::ClaudeOperatorProfile::validate`] contract so the daemon
/// reaches its health-checkable serving state; it never exercises real Codex/D03 certification.
fn write_launcher_template(candidate: &Path) -> PathBuf {
    use agent_ide::execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord};
    let sandbox_state = json!({
        "permissionProfile": {"type": "disabled"},
        "codexLinuxSandboxExe": null,
        "sandboxCwd": candidate,
        "useLegacyLandlock": false
    });
    let state = HostSandboxState::parse(Some(sandbox_state.clone())).unwrap();
    let record = PersistedProfileRecord::from_execution_evidence(
        "service-lifecycle-fixture-disabled",
        1,
        D03ProfileEvidence {
            provider_binary: "fixture-git".into(),
            toolchain: "fixture-toolchain".into(),
            configuration: "fixture-v1".into(),
            trust: "explicit-test-only-disabled".into(),
            transport: "direct-fixture".into(),
            d03_evidence: "fixture-only-not-host-certification".into(),
        },
        &state,
    )
    .unwrap();
    let config = json!({
        "version": 1,
        "limits": {"queued": 16, "details": 64, "operation_ms": 120000, "output_bytes": 1048576},
        "targets": [{
            "attachment": "private-host-channel",
            "candidate": candidate,
            "git": accepted_program("/usr/bin/git", "fixture-git"),
            "codex": accepted_program("/usr/bin/true", "unused-disabled-wrapper"),
            "providers": [],
            "profiles": [{
                "record": serde_json::from_str::<Value>(&record.to_json()).unwrap(),
                "sandbox_state": sandbox_state
            }],
            "allow_disabled_host": true,
            "claude_profile": {
                "enabled": true,
                "fail_if_unavailable": true,
                "allow_unsandboxed_commands": false,
                "no_matching_excluded_commands": true,
                "scope_declared": true,
                "platform": "mac_os"
            }
        }]
    });
    let path = unique_path("launcher-config").with_extension("json");
    std::fs::write(&path, config.to_string()).unwrap();
    path
}

/// Supplies an exact content fingerprint for a fixture's explicitly accepted executable.
fn accepted_program(path: &str, identity: &str) -> Value {
    json!({
        "path": path,
        "identity": identity,
        "blake3": blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()
    })
}

/// Owns one real shipping managed-Claude MCP process and its newline-delimited protocol streams.
struct Mcp {
    /// Killed on drop if a scenario panics before its explicit shutdown.
    child: Child,
    /// Host-to-server stream; closing it ends the MCP session.
    input: ChildStdin,
    /// Server output, which must contain only valid MCP JSON messages.
    output: BufReader<ChildStdout>,
}

impl Mcp {
    /// Starts the shipping self-contained Claude MCP using only its captured project environment.
    async fn start(template: &Path, project: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
        command
            .env("TOKIO_WORKER_THREADS", "1")
            .env("CLAUDE_PROJECT_DIR", project)
            .env_remove("AGENT_IDE_HOST_ATTACHMENT")
            .env_remove("AGENT_IDE_MANAGED_CODEX_ATTACHMENT")
            .args(["mcp", "--auto-launcher-template"])
            .arg(template)
            .current_dir(project.parent().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut mcp = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let response = mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-03-26","capabilities":{},
                    "clientInfo":{"name":"service-lifecycle-contract","version":"1"}
                }}),
            )
            .await;
        assert!(response.get("result").is_some(), "{response}");
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        mcp
    }

    /// Writes and flushes one JSON protocol message without awaiting a response.
    async fn send(&mut self, request: Value) {
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    /// Exchanges one request, ignoring notifications and enforcing a five-second test deadline.
    async fn exchange(&mut self, request: Value) -> Value {
        let id = request["id"].clone();
        self.send(request).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.output.read_line(&mut line).await.unwrap(),
                    0,
                    "MCP exited before response"
                );
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
            }
        })
        .await
        .expect("MCP response exceeded bounded deadline")
    }

    /// Closes stdin and verifies the MCP process exits cleanly without requiring a daemon shutdown.
    async fn close(self) {
        let Self {
            mut child, input, ..
        } = self;
        drop(input);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

/// Polls until `runtime` reports a healthy daemon with its lock held, or the deadline elapses.
async fn wait_for_healthy_locked_daemon(runtime: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(report) = doctor_report(runtime).await
                && report.lock == DoctorLockState::Held
                && matches!(report.status, DoctorStatus::Healthy { .. })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("daemon did not become healthy within the bounded deadline");
}

/// Two worktrees of one repository share the exact same rendezvous path; a second, unrelated
/// repository and a non-git directory each resolve to their own distinct paths.
#[tokio::test]
async fn shared_runtime_is_keyed_by_repository_not_by_worktree_or_process() {
    let repo = init_repo();
    let worktree_a = add_worktree(&repo, "worktree-a");
    let worktree_b = add_worktree(&repo, "worktree-b");
    let other_repo = init_repo();
    let non_git = unique_path("plain-dir");
    std::fs::create_dir_all(&non_git).unwrap();

    let path_a = expected_runtime_path(&worktree_a);
    let path_b = expected_runtime_path(&worktree_b);
    let path_other = expected_runtime_path(&other_repo);
    let path_plain = expected_runtime_path(&non_git);
    assert_eq!(
        path_a, path_b,
        "two worktrees of one repository must share one rendezvous path"
    );
    assert_ne!(
        path_a, path_other,
        "two different repositories must not share a rendezvous path"
    );
    assert_ne!(
        path_a, path_plain,
        "a non-git candidate must not collide with a git-keyed rendezvous path"
    );
    assert!(
        !path_a.exists() && !path_other.exists() && !path_plain.exists(),
        "no rendezvous may exist before any MCP starts"
    );

    let _guard_a = DaemonGuard(path_a.clone());
    let _guard_plain = DaemonGuard(path_plain.clone());

    let template = write_launcher_template(&worktree_a);
    let mcp_a = Mcp::start(&template, &worktree_a).await;
    wait_for_healthy_locked_daemon(&path_a).await;
    assert!(!path_other.exists());

    // A second worktree of the same repository adopts the exact same daemon: no second rendezvous
    // directory ever appears for it.
    let template_b = write_launcher_template(&worktree_b);
    let mcp_b = Mcp::start(&template_b, &worktree_b).await;
    wait_for_healthy_locked_daemon(&path_a).await;
    assert_eq!(
        lock_holder_count(&path_a),
        1,
        "one repository must never run two daemons"
    );

    // A non-git candidate resolves and starts its own independent rendezvous.
    let template_plain = write_launcher_template(&non_git);
    let mcp_plain = Mcp::start(&template_plain, &non_git).await;
    wait_for_healthy_locked_daemon(&path_plain).await;
    assert_ne!(
        std::fs::canonicalize(&path_plain).unwrap(),
        std::fs::canonicalize(&path_a).unwrap()
    );

    mcp_a.close().await;
    mcp_b.close().await;
    mcp_plain.close().await;
    assert!(
        path_a.is_dir(),
        "the shared daemon must outlive every MCP's own EOF"
    );
    assert!(
        path_plain.is_dir(),
        "the independent daemon must also outlive its MCP's own EOF"
    );

    let _ = std::fs::remove_dir_all(repo);
    let _ = std::fs::remove_dir_all(worktree_a);
    let _ = std::fs::remove_dir_all(worktree_b);
    let _ = std::fs::remove_dir_all(other_repo);
    let _ = std::fs::remove_dir_all(non_git);
}

/// Two MCP processes starting at once for the same candidate converge on exactly one live daemon;
/// the process that loses the daemon's exclusive lock adopts the winner instead of failing open.
#[tokio::test]
async fn concurrent_mcp_starts_converge_on_one_lock_holder() {
    let candidate = init_repo();
    let runtime = expected_runtime_path(&candidate);
    let _guard = DaemonGuard(runtime.clone());
    assert!(!runtime.exists());
    let template = write_launcher_template(&candidate);

    let (first, second) = tokio::join!(
        Mcp::start(&template, &candidate),
        Mcp::start(&template, &candidate)
    );

    wait_for_healthy_locked_daemon(&runtime).await;
    assert_eq!(
        lock_holder_count(&runtime),
        1,
        "a concurrent start race must still end with exactly one daemon"
    );

    first.close().await;
    second.close().await;
    assert!(
        runtime.is_dir(),
        "the winning daemon must outlive both MCP processes' own EOF"
    );

    let _ = std::fs::remove_dir_all(candidate);
}

/// The daemon started by one MCP survives that MCP's own stdio EOF, and a later MCP for the same
/// repository adopts the exact same still-live daemon rather than starting a new generation.
#[tokio::test]
async fn daemon_survives_mcp_eof_and_is_adopted_by_a_later_mcp() {
    let candidate = init_repo();
    let runtime = expected_runtime_path(&candidate);
    let _guard = DaemonGuard(runtime.clone());
    let template = write_launcher_template(&candidate);

    let first = Mcp::start(&template, &candidate).await;
    wait_for_healthy_locked_daemon(&runtime).await;
    let generation_before = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("daemon must be healthy before EOF"),
    };

    first.close().await;
    // No lease/idle timeout exists yet (T093); the daemon must still be exactly the one already
    // running, unaffected by its spawning MCP's own process exit.
    let after_eof = doctor_report(&runtime).await.unwrap();
    assert_eq!(after_eof.lock, DoctorLockState::Held);
    match after_eof.status {
        DoctorStatus::Healthy { daemon_generation } => {
            assert_eq!(daemon_generation, generation_before)
        }
        DoctorStatus::Unavailable => panic!("daemon must remain healthy after its MCP's own EOF"),
    }

    let second = Mcp::start(&template, &candidate).await;
    wait_for_healthy_locked_daemon(&runtime).await;
    assert_eq!(
        lock_holder_count(&runtime),
        1,
        "adopting a live daemon must never start a second one"
    );
    let generation_after = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("adopted daemon must be healthy"),
    };
    assert_eq!(
        generation_after, generation_before,
        "the second MCP must adopt the exact same daemon generation, not start a new one"
    );

    second.close().await;
    assert!(runtime.is_dir());

    let _ = std::fs::remove_dir_all(candidate);
}
