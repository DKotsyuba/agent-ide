//! Executable EYES-r2 §2 rendezvous/spawn/adopt/lease/idle lifecycle contract for the shared managed
//! Claude daemon: repository-keyed sharing across worktrees, start-race convergence to one lock
//! holder, daemon persistence across an owning MCP process's own stdio EOF, and client-lease-driven
//! idle shutdown.

use std::{
    future::Future,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::app::{
    self, DoctorLockState, DoctorStatus, RuntimeDir,
    config::EffectiveConfig,
    doctor_report,
    transport::{
        AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
        AssistanceDispatcher,
    },
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
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
    write_launcher_template_with_idle_timeout_s(candidate, None)
}

/// Identical to [`write_launcher_template`], plus an explicit `idle_timeout_s` override.
///
/// The real managed-MCP startup path only ever reads this from the launcher's `project_checks`
/// (EYES-r2 §1); `None` here omits that section entirely, keeping the contract default (300s).
fn write_launcher_template_with_idle_timeout_s(
    candidate: &Path,
    idle_timeout_s: Option<u64>,
) -> PathBuf {
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
    let mut config = json!({
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
    if let Some(idle_timeout_s) = idle_timeout_s {
        config["project_checks"] = json!({"idle_timeout_s": idle_timeout_s});
    }
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
    // The production idle timeout is 300s (EYES-r2 §1 default), far longer than this assertion
    // window, so the daemon must still be exactly the one already running here, unaffected by its
    // spawning MCP's own process exit; T093's lease/idle mechanism is covered under a short test
    // timeout by the dedicated lease/idle scenarios below.
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

/// Sends one `ide.start` call over `mcp` with a fresh Claude tool-use id and returns the raw reply.
async fn call_ide_start(mcp: &mut Mcp, id: u64, activation_id: &str) -> Value {
    mcp.exchange(
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"ide.start","arguments":{"activation_id":activation_id},
            "_meta":{"claudecode/toolUseId":format!("call-{id}")}
        }}),
    )
    .await
}

/// Asserts one `ide.start` reply reached a live daemon and got its typed no-prior-hook outcome,
/// rather than the transport-level "daemon is unavailable" fallback text this contract targets.
///
/// The Claude host this fixture drives never receives `structuredContent` (T14B): it hands that
/// field straight to its model in place of `content`, defeating the compact renderer, so the
/// managed Claude MCP omits it entirely and this asserts against the compact `content` text instead.
fn assert_reached_live_daemon(response: &Value) {
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        response["result"].get("structuredContent").is_none(),
        "{response}"
    );
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("unavailable: host_binding")),
        "{response}"
    );
}

/// Asserts the same typed no-prior-hook outcome as [`assert_reached_live_daemon`], plus the T08B
/// retry hint: this call's own dispatch is the one that re-established the shared daemon, so the
/// freshly re-established daemon has no pre-hook observation for it either, and an agent reading
/// only the plain host-binding-unavailable outcome would have no way to know a repeat could
/// succeed.
fn assert_reached_live_daemon_after_reconnect(response: &Value) {
    assert_ne!(response["result"]["isError"], json!(true), "{response}");
    assert!(
        response["result"].get("structuredContent").is_none(),
        "{response}"
    );
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.starts_with("unavailable: host_binding")
                && text.contains("retry: daemon restarted; repeat this call once")),
        "{response}"
    );
}

/// After the shared daemon an MCP client is using exits (here, the test's own `SIGTERM`, standing
/// in for idle shutdown, a crash, or a binary upgrade), the client re-establishes it on the next
/// call instead of failing every later call, and the repository still ends up with exactly one
/// live daemon.
#[tokio::test]
async fn mcp_client_re_establishes_a_lost_shared_daemon_and_serves_the_next_call() {
    let candidate = init_repo();
    let runtime = expected_runtime_path(&candidate);
    let _guard = DaemonGuard(runtime.clone());
    let template = write_launcher_template(&candidate);

    let mut mcp = Mcp::start(&template, &candidate).await;
    wait_for_healthy_locked_daemon(&runtime).await;
    let generation_before = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("daemon must be healthy before the reconnect scenario"),
    };

    assert_reached_live_daemon(&call_ide_start(&mut mcp, 2, "before").await);

    // The test owns this daemon process and terminates it directly, standing in for a graceful
    // idle shutdown, a crash, or a binary upgrade while the MCP process itself keeps running.
    terminate_shared_daemon(&runtime);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !runtime.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the terminated daemon must remove its own runtime directory");

    // The very next call must reach a live daemon again, not repeat the stale unavailable error,
    // and its own outcome must carry the T08B retry hint because it is the retried dispatch that
    // re-established the daemon.
    assert_reached_live_daemon_after_reconnect(&call_ide_start(&mut mcp, 3, "after").await);

    wait_for_healthy_locked_daemon(&runtime).await;
    let generation_after = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("the re-established daemon must be healthy"),
    };
    assert_ne!(
        generation_after, generation_before,
        "the re-established daemon must be a fresh generation, not the terminated one"
    );
    assert_eq!(
        lock_holder_count(&runtime),
        1,
        "the repository must still end up with exactly one live daemon"
    );

    mcp.close().await;
    assert!(runtime.is_dir());

    let _ = std::fs::remove_dir_all(candidate);
}

/// After re-establishing a lost shared daemon, the MCP process's own `ClientLease` connection is
/// also reopened against the new generation (T08B follow-up): with the MCP still alive and
/// completely silent for longer than the configured idle timeout, the re-established daemon must
/// still be running, not idled out from under it because this live MCP held a dead lease.
#[tokio::test]
async fn mcp_client_reopens_its_lease_after_re_establishing_a_lost_shared_daemon() {
    const IDLE_TIMEOUT_S: u64 = 30;
    let candidate = init_repo();
    let runtime = expected_runtime_path(&candidate);
    let _guard = DaemonGuard(runtime.clone());
    let template = write_launcher_template_with_idle_timeout_s(&candidate, Some(IDLE_TIMEOUT_S));

    let mut mcp = Mcp::start(&template, &candidate).await;
    wait_for_healthy_locked_daemon(&runtime).await;
    let generation_before = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("daemon must be healthy before the reconnect scenario"),
    };

    // Stands in for idle shutdown, a crash, or a binary upgrade while the MCP process keeps running.
    terminate_shared_daemon(&runtime);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !runtime.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the terminated daemon must remove its own runtime directory");

    // This is the first call made on this connection, and the daemon it was minted against is
    // already gone, so this dispatch is itself the one that re-establishes it and must carry the
    // T08B retry hint.
    assert_reached_live_daemon_after_reconnect(&call_ide_start(&mut mcp, 2, "reconnect").await);

    wait_for_healthy_locked_daemon(&runtime).await;
    let generation_after = match doctor_report(&runtime).await.unwrap().status {
        DoctorStatus::Healthy { daemon_generation } => daemon_generation,
        DoctorStatus::Unavailable => panic!("the re-established daemon must be healthy"),
    };
    assert_ne!(
        generation_after, generation_before,
        "this scenario requires a fresh generation, not the terminated one"
    );

    // The MCP process makes no further calls from here on; only a reopened lease can suppress the
    // idle countdown a re-established daemon otherwise starts with zero leases.
    tokio::time::sleep(Duration::from_secs(IDLE_TIMEOUT_S + 5)).await;
    assert!(
        doctor_report(&runtime)
            .await
            .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. })),
        "the re-established daemon must still be running: its lease must have been reopened"
    );

    mcp.close().await;
    assert!(runtime.is_dir());

    let _ = std::fs::remove_dir_all(candidate);
}

/// Accepts every dispatch as unavailable; the lease/idle scenarios below never exercise Assistance
/// semantics, only the Application-layer lease count and idle-timeout shutdown path.
struct NoopDispatcher;

impl AssistanceDispatcher for NoopDispatcher {
    fn dispatch(
        &self,
        _request: AssistanceDispatch,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async { Err(AssistanceDispatchUnavailable) })
    }
}

/// Starts the real in-process Assistance daemon with an explicit short `idle_timeout`, and returns
/// its runtime directory and the task that resolves once the daemon's own run loop returns.
///
/// The real managed-MCP startup path always uses the EYES-r2 §1 production default (300s), so the
/// short deadlines these scenarios need can only come from this daemon-runner parameter directly.
async fn start_lease_test_daemon(idle_timeout: Duration) -> (PathBuf, tokio::task::JoinHandle<()>) {
    // A short, uncanonicalized prefix, unlike `unique_path`: a Unix socket path must fit
    // `sockaddr_un.sun_path` (104 bytes on macOS), which `unique_path`'s longer fixture prefix
    // combined with a canonicalized `TMPDIR` can already exceed before the socket name is appended.
    let runtime_dir = std::env::temp_dir().join(format!(
        "ai-lt-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let prepared = RuntimeDir::prepare_for_daemon(&runtime_dir).unwrap();
    let task = tokio::spawn(async move {
        app::run_daemon_with_assistance(
            prepared,
            Arc::new(NoopDispatcher),
            EffectiveConfig::defaults(),
            idle_timeout,
        )
        .await
        .unwrap();
    });
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return (runtime_dir, task);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("lease test daemon did not bind its private socket");
}

/// A daemon that starts with zero open leases shuts itself down after its configured idle timeout,
/// through the same orderly path as SIGTERM, and removes its own runtime directory (EYES-r2 §2).
#[tokio::test]
async fn daemon_shuts_down_after_idle_timeout_with_zero_leases() {
    let (runtime, task) = start_lease_test_daemon(Duration::from_millis(300)).await;
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("a daemon with no lease must shut itself down after its idle timeout")
        .unwrap();
    assert!(
        !runtime.exists(),
        "orderly idle shutdown must remove its own runtime directory"
    );
}

/// An open `ClientLease` connection suppresses idle shutdown for as long as it stays open; its own
/// EOF releases the lease and lets the idle countdown run to completion.
#[tokio::test]
async fn open_lease_suppresses_idle_shutdown_until_released() {
    let (runtime, task) = start_lease_test_daemon(Duration::from_millis(400)).await;
    let lease = app::open_client_lease(&runtime, "lease-a")
        .await
        .expect("daemon must admit a well-formed lease request");

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        doctor_report(&runtime)
            .await
            .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. })),
        "an open lease must keep the daemon alive past its idle timeout"
    );

    drop(lease);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("idle expiry must stop the daemon within a bounded deadline")
        .unwrap();
    assert!(!runtime.exists());
}

/// A lease opened while the idle countdown is already running cancels that countdown; the daemon
/// only idles out once this later lease also releases.
#[tokio::test]
async fn a_lease_opened_during_the_countdown_cancels_shutdown() {
    let (runtime, task) = start_lease_test_daemon(Duration::from_millis(500)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let lease = app::open_client_lease(&runtime, "late-lease")
        .await
        .expect("daemon must still be alive to admit a lease mid-countdown");

    // Past the ORIGINAL 500ms deadline (300ms elapsed + 400ms more); still alive proves the lease
    // cancelled that countdown rather than merely outliving a shorter one.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        doctor_report(&runtime)
            .await
            .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. })),
        "a lease opened mid-countdown must cancel the pending idle shutdown"
    );

    drop(lease);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("idle expiry must fire once the later lease also releases")
        .unwrap();
    assert!(!runtime.exists());
}

/// Simulates two managed Claude MCP processes sharing one daemon, each holding its own `ClientLease`
/// open for its process lifetime (EYES-r2 §2): the daemon must survive the first exiting alone, and
/// only starts (and completes) its idle countdown once the second's lease also releases.
#[tokio::test]
async fn daemon_survives_first_of_two_mcp_leases_exiting_and_idles_after_the_second() {
    let (runtime, task) = start_lease_test_daemon(Duration::from_millis(400)).await;
    let lease_a = app::open_client_lease(&runtime, "mcp-a")
        .await
        .expect("first simulated MCP's lease must be admitted");
    let lease_b = app::open_client_lease(&runtime, "mcp-b")
        .await
        .expect("second simulated MCP's lease must be admitted");

    drop(lease_a);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        doctor_report(&runtime)
            .await
            .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. })),
        "the daemon must survive the first of two leases releasing while the second stays open"
    );

    drop(lease_b);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("idle expiry must fire once the second, and last, lease also releases")
        .unwrap();
    assert!(!runtime.exists());
}

/// Sends one raw v3 `assistance.method_dispatch` frame to the in-process daemon and returns its
/// correlated reply; the real v2/v3 Assistance framing (one big-endian u32 length prefix).
async fn exchange_v3(runtime: &Path, request_id: &str) -> Value {
    let mut stream = UnixStream::connect(runtime.join("agent-ide.sock"))
        .await
        .unwrap();
    let request = json!({
        "version": 3,
        "request_id": request_id,
        "correlation_id": format!("corr-{request_id}"),
        "opaque_attachment": "private-host-channel",
        "method": "assistance.method_dispatch",
        "dispatch_method": "start",
        "params_json": {"parameters": {"activation_id": request_id}}
    });
    let body = serde_json::to_vec(&request).unwrap();
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut body = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// A lease-free client session is visible to the daemon only through the calls it serves (T26B):
/// the managed Codex MCP owns its per-session daemon outright and holds no `ClientLease` at all.
/// A served v3 Assistance call within the idle window must restart the countdown — the daemon
/// outlives the original deadline — and one full silent window after the last served call must
/// still end it, so the idle-shutdown contract keeps exactly its configured meaning.
#[tokio::test]
async fn a_served_assistance_call_restarts_the_idle_countdown() {
    const IDLE_MS: u64 = 600;
    let (runtime, task) = start_lease_test_daemon(Duration::from_millis(IDLE_MS)).await;

    // One served call at half the window; without the restart the daemon would exit at 1x window.
    tokio::time::sleep(Duration::from_millis(IDLE_MS / 2)).await;
    let reply = exchange_v3(&runtime, "idle-probe").await;
    assert_eq!(reply["status"], json!("unavailable"), "{reply}");

    // Past the ORIGINAL deadline; still alive proves the served call restarted the countdown.
    tokio::time::sleep(Duration::from_millis(IDLE_MS * 3 / 4)).await;
    assert!(
        !task.is_finished(),
        "a served call within the idle window must restart the idle countdown"
    );

    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("one full silent window after the last served call must still end the daemon")
        .unwrap();
    assert!(
        !runtime.exists(),
        "orderly idle shutdown must remove its own runtime directory"
    );
}
