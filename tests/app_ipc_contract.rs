//! Contract checks for the Application layer's real private Unix IPC boundary.

use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_ide::app::config::EffectiveConfig;
use agent_ide::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, HookSubmit, HookSubmitTransportResult, HookTransportLimits,
    MethodDispatch, MethodDispatchTransportResult, OpaqueJson,
};
use agent_ide::app::{
    RuntimeDir, dispatch_method_if_running, run_daemon_with_assistance, submit_hook_if_running,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Returns the fixed finite limits used by all real v2 transport contract scenarios; these mirror
/// the production `MAX_V2_FRAME_BYTES`/`MAX_ASSISTANCE_JSON_BYTES` constants (v0.6.1 raised both
/// so a near-maximum `ide.edit` argument round-trips — see `edit_at_the_new_ceiling_round_trips`).
fn transport_limits() -> HookTransportLimits {
    HookTransportLimits::new(160 * 1024, 144 * 1024, Duration::from_secs(1)).unwrap()
}

/// Supplies opaque deterministic Assistance results without interpreting host identity or tool semantics.
struct TestDispatcher;

impl AssistanceDispatcher for TestDispatcher {
    /// Returns a distinct opaque reply for each of the two closed Application dispatch shapes.
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            match request {
                AssistanceDispatch::HookSubmit(_) => Ok(AssistanceDispatchReply::HookSubmit(
                    OpaqueJson::new("{\"hook\":true}", 64 * 1024).unwrap(),
                )),
                AssistanceDispatch::MethodDispatch(_) => {
                    Ok(AssistanceDispatchReply::MethodDispatch(
                        OpaqueJson::new("{\"method\":true}", 64 * 1024).unwrap(),
                    ))
                }
            }
        })
    }
}

/// Creates a unique private directory below the platform temporary directory for one daemon process.
fn runtime_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agent-ide-ipc-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// Starts the real daemon binary and waits only until its private socket accepts connections.
async fn start_daemon(runtime_dir: &Path) -> Child {
    let child = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(runtime_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon did not bind its private socket");
}

/// Starts the in-process finite Assistance daemon and waits until its Unix endpoint accepts a peer.
async fn start_assistance_daemon(runtime_dir: &Path) -> tokio::task::JoinHandle<()> {
    let daemon_runtime = RuntimeDir::prepare_for_daemon(runtime_dir).unwrap();
    let task = tokio::spawn(async move {
        run_daemon_with_assistance(
            daemon_runtime,
            Arc::new(TestDispatcher),
            EffectiveConfig::defaults(),
            agent_ide::app::lease::DEFAULT_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
    });
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("Assistance daemon did not bind its private socket");
}

/// Stops the test-only in-process daemon and removes its disposable private runtime directory.
async fn stop_assistance_daemon(task: tokio::task::JoinHandle<()>, runtime_dir: PathBuf) {
    task.abort();
    let _ = task.await;
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// Stops a disposable daemon process and removes its whole test-owned runtime directory.
async fn stop_daemon(mut child: Child, runtime_dir: PathBuf) {
    let _ = child.kill().await;
    let _ = child.wait().await;
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// Sends one complete JSON frame and returns the complete JSON response from the real daemon socket.
async fn exchange(runtime_dir: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
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

/// Confirms correlation, private modes, and the current effective user's accepted peer connection.
#[tokio::test]
async fn health_is_correlated_and_endpoint_is_private() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    let reply = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "caller-7", "method": "health"}),
    )
    .await;
    assert_eq!(reply["version"], 1);
    assert_eq!(reply["request_id"], "caller-7");
    assert_eq!(reply["status"], "ok");
    assert!(reply["daemon_generation"].as_str().unwrap().len() >= 32);
    assert_eq!(
        fs::metadata(&runtime_dir).unwrap().permissions().mode() & 0o077,
        0
    );
    assert_eq!(
        fs::metadata(runtime_dir.join("agent-ide.sock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o077,
        0
    );
    let doctor = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["doctor", "--runtime-dir"])
        .arg(&runtime_dir)
        .output()
        .await
        .unwrap();
    assert!(doctor.status.success());
    let report = String::from_utf8(doctor.stdout).unwrap();
    assert!(report.contains("status=healthy:"));
    assert!(report.contains("runtime=Private"));
    assert!(report.contains("endpoint=Socket"));
    assert!(report.contains("lock=Held"));
    assert!(report.contains("protocol.assistance_transport=v2"));
    stop_daemon(child, runtime_dir).await;
}

/// Proves malformed, unknown, oversized, and truncated frames cannot reach the health handler.
#[tokio::test]
async fn invalid_frames_close_without_a_health_reply() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    for request in [
        json!({"version": 1, "request_id": "id", "method": "health", "future": true}),
        json!({"version": 1, "request_id": "id", "method": "unknown"}),
    ] {
        let mut stream = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
            .await
            .unwrap();
        let body = serde_json::to_vec(&request).unwrap();
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .await
            .unwrap();
        stream.write_all(&body).await.unwrap();
        let mut reply = [0_u8; 1];
        assert_eq!(stream.read(&mut reply).await.unwrap(), 0);
    }
    let mut stream = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
    stream.write_all(&(65_537_u32).to_be_bytes()).await.unwrap();
    let mut reply = [0_u8; 1];
    assert_eq!(stream.read(&mut reply).await.unwrap(), 0);
    let mut stream = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
    stream.write_all(&[0, 0]).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(stream.read(&mut reply).await.unwrap(), 0);
    stop_daemon(child, runtime_dir).await;
}

/// Ensures a dead socket is recovered after the daemon takes the lock and a live lock blocks a peer.
#[tokio::test]
async fn stale_socket_recovers_but_live_daemon_lock_wins() {
    let runtime_dir = runtime_dir();
    let socket = runtime_dir.join("agent-ide.sock");
    drop(UnixListener::bind(&socket).unwrap());
    let child = start_daemon(&runtime_dir).await;
    let contender = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime_dir)
        .output()
        .await
        .unwrap();
    assert!(!contender.status.success());
    stop_daemon(child, runtime_dir).await;
}

/// Verifies doctor neither creates a missing runtime directory nor treats absence as health.
#[tokio::test]
async fn doctor_does_not_autostart_or_create_runtime_directory() {
    let runtime_dir = std::env::temp_dir().join(format!(
        "agent-ide-missing-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["doctor", "--runtime-dir"])
        .arg(&runtime_dir)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("status=unavailable"));
    assert!(stdout.contains("runtime=Missing"));
    assert!(stdout.contains("endpoint=Missing"));
    assert!(stdout.contains("lock=Missing"));
    assert!(stdout.contains("config.generation=1"));
    assert!(stdout.contains("protocol.health=v1"));
    assert!(stdout.contains("control.daemon_autostart=unsupported"));
    assert!(stdout.contains("control.workspace_scan=unsupported"));
    assert!(stdout.contains("control.lsp_open=unsupported"));
    assert!(!runtime_dir.exists());
}

/// Proves doctor never follows an unsafe runtime symlink to connect to an otherwise observable socket.
#[tokio::test]
async fn doctor_does_not_connect_through_an_unsafe_runtime_path() {
    let target = runtime_dir();
    let socket = target.join("agent-ide.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let unsafe_path = std::env::temp_dir().join(format!(
        "agent-ide-unsafe-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    symlink(&target, &unsafe_path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["doctor", "--runtime-dir"])
        .arg(&unsafe_path)
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("status=unavailable"));
    assert!(stdout.contains("runtime=Unsafe"));
    assert!(stdout.contains("endpoint=Unavailable"));
    assert!(stdout.contains("lock=Unavailable"));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err()
    );
    assert!(unsafe_path.is_symlink());
    assert!(socket.exists());
    fs::remove_file(unsafe_path).unwrap();
    fs::remove_file(socket).unwrap();
    fs::remove_dir(target).unwrap();
}

/// Proves doctor does not follow a socket symlink even when its containing runtime directory is safe.
#[tokio::test]
async fn doctor_does_not_connect_through_a_non_socket_endpoint() {
    let runtime = runtime_dir();
    let target = runtime_dir();
    let target_socket = target.join("agent-ide.sock");
    let listener = UnixListener::bind(&target_socket).unwrap();
    let endpoint = runtime.join("agent-ide.sock");
    symlink(&target_socket, &endpoint).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["doctor", "--runtime-dir"])
        .arg(&runtime)
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("status=unavailable"));
    assert!(stdout.contains("runtime=Private"));
    assert!(stdout.contains("endpoint=Unexpected"));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err()
    );
    assert!(endpoint.is_symlink());
    assert!(target_socket.exists());
    fs::remove_file(endpoint).unwrap();
    fs::remove_file(target_socket).unwrap();
    fs::remove_dir(runtime).unwrap();
    fs::remove_dir(target).unwrap();
}

/// Proves hook ingress and the closed five-method dispatch stay finite, correlated, and unavailable when inactive.
#[tokio::test]
async fn assistance_transport_is_finite_and_hook_submission_never_autostarts() {
    let missing = std::env::temp_dir().join(format!(
        "agent-ide-hook-missing-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let inactive = submit_hook_if_running(
        &missing,
        HookSubmit::new(
            "request",
            "correlation",
            "attachment",
            OpaqueJson::new("{\"phase\":\"post\"}", 64 * 1024).unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await;
    assert_eq!(inactive, HookSubmitTransportResult::Unavailable);
    assert!(!missing.exists());

    let runtime_dir = runtime_dir();
    let task = start_assistance_daemon(&runtime_dir).await;
    let hook = submit_hook_if_running(
        &runtime_dir,
        HookSubmit::new(
            "request",
            "correlation",
            "attachment",
            OpaqueJson::new("{\"phase\":\"post\",\"actor_id\":\"a\"}", 64 * 1024).unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await;
    assert!(matches!(hook, HookSubmitTransportResult::Dispatched { .. }));

    let method = dispatch_method_if_running(
        &runtime_dir,
        MethodDispatch::new(
            "method-request",
            "method-correlation",
            "attachment",
            agent_ide::app::transport::AssistanceMethod::Context,
            OpaqueJson::new("{\"path\":\"main.rs\"}", 64 * 1024).unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await;
    assert!(matches!(
        method,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    let edit = dispatch_method_if_running(
        &runtime_dir,
        MethodDispatch::new(
            "edit-request",
            "edit-correlation",
            "attachment",
            agent_ide::app::transport::AssistanceMethod::Edit,
            OpaqueJson::new(
                r#"{"operation_id":"op","path":"a.rs","source_ref":"source","content":"new"}"#,
                64 * 1024,
            )
            .unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await;
    assert!(matches!(
        edit,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    let v3 = exchange(
        &runtime_dir,
        json!({
            "version":3,
            "request_id":"v3-request",
            "correlation_id":"v3-correlation",
            "opaque_attachment":"attachment",
            "method":"assistance.method_dispatch",
            "dispatch_method":"edit",
            "params_json":{}
        }),
    )
    .await;
    assert_eq!(v3["version"], 3);
    assert_eq!(v3["request_id"], "v3-request");
    let mut unknown = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
    let unknown_body = serde_json::to_vec(&json!({
        "version": 2,
        "request_id": "unknown-request",
        "correlation_id": "unknown-correlation",
        "opaque_attachment": "attachment",
        "method": "assistance.method_dispatch",
        "dispatch_method": "finish",
        "params_json": {},
    }))
    .unwrap();
    unknown
        .write_all(&(unknown_body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    unknown.write_all(&unknown_body).await.unwrap();
    let mut reply = [0_u8; 1];
    assert_eq!(unknown.read(&mut reply).await.unwrap(), 0);
    let mut v2_edit = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
    let body = serde_json::to_vec(&json!({
        "version":2,
        "request_id":"v2-edit",
        "correlation_id":"v2-edit-correlation",
        "opaque_attachment":"attachment",
        "method":"assistance.method_dispatch",
        "dispatch_method":"edit",
        "params_json":{}
    }))
    .unwrap();
    v2_edit
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    v2_edit.write_all(&body).await.unwrap();
    assert_eq!(v2_edit.read(&mut reply).await.unwrap(), 0);
    stop_assistance_daemon(task, runtime_dir).await;
}

/// v0.6.1 raised the wire ceilings (content 128 KiB, argument object 136 KiB, assistance JSON 144
/// KiB, V2 frame 160 KiB) so a near-maximum `ide.edit` argument round-trips through the real
/// socket, where the old 64 KiB assistance-JSON/128 KiB frame ceilings would have refused it.
#[tokio::test]
async fn edit_at_the_new_ceiling_round_trips() {
    let runtime_dir = runtime_dir();
    let task = start_assistance_daemon(&runtime_dir).await;
    let params = json!({
        "operation_id": "edit-large",
        "path": "a.rs",
        "source_ref": "source",
        "content": "x".repeat(100 * 1024),
    });
    let edit = dispatch_method_if_running(
        &runtime_dir,
        MethodDispatch::new(
            "edit-large-request",
            "edit-large-correlation",
            "attachment",
            agent_ide::app::transport::AssistanceMethod::Edit,
            OpaqueJson::new(params.to_string(), 144 * 1024).unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await;
    assert!(matches!(
        edit,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    stop_assistance_daemon(task, runtime_dir).await;
}

/// Proves connect and write keep the configured deadline for both hook and method requests.
#[tokio::test(start_paused = true)]
async fn hook_and_method_keep_connect_write_within_the_configured_deadline() {
    // A runnable task prevents paused time from auto-advancing while the socket reactor catches up.
    let clock_guard = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    for hook in [true, false] {
        let runtime = runtime_dir();
        let listener = UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
        let limits =
            HookTransportLimits::new(128 * 1024, 64 * 1024, Duration::from_millis(100)).unwrap();
        let mut request = Box::pin(async {
            if hook {
                submit_hook_if_running(
                    &runtime,
                    HookSubmit::new(
                        "request",
                        "correlation",
                        "attachment",
                        OpaqueJson::new("{}", 64).unwrap(),
                    )
                    .unwrap(),
                    limits,
                )
                .await
                    == HookSubmitTransportResult::Unavailable
            } else {
                dispatch_method_if_running(
                    &runtime,
                    MethodDispatch::new(
                        "request",
                        "correlation",
                        "attachment",
                        agent_ide::app::transport::AssistanceMethod::Context,
                        OpaqueJson::new("{}", 64).unwrap(),
                    )
                    .unwrap(),
                    limits,
                )
                .await
                    == MethodDispatchTransportResult::Unavailable
            }
        });
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(request.as_mut().poll(&mut context).is_pending());
        tokio::time::advance(Duration::from_millis(60)).await;
        let (mut socket, _) = listener.accept().await.unwrap();
        // Drive the suspended connect and write to completion, then keep the peer silent.
        let frame = tokio::select! {
            _ = &mut request => panic!("transport completed before the fake peer read its request"),
            frame = async {
                let length = socket.read_u32().await.unwrap() as usize;
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                serde_json::from_slice::<Value>(&body).unwrap()
            } => {
                assert_eq!(frame["request_id"], "request");
                frame
            },
        };
        if hook {
            tokio::time::advance(Duration::from_millis(50)).await;
            assert_eq!(
                request.as_mut().poll(&mut context),
                std::task::Poll::Ready(true)
            );
        } else {
            let response = json!({"version":frame["version"],"request_id":"request","opaque_result_json":{"state":"pending","detail_ref":"detail"}});
            let body = serde_json::to_vec(&response).unwrap();
            socket
                .write_all(&(body.len() as u32).to_be_bytes())
                .await
                .unwrap();
            socket.write_all(&body).await.unwrap();
            tokio::task::yield_now().await;
            assert_eq!(
                request.as_mut().poll(&mut context),
                std::task::Poll::Ready(false)
            );
        }
        drop(request);
        drop(socket);
        drop(listener);
        fs::remove_dir_all(runtime).unwrap();
    }
    clock_guard.abort();
}

/// Parks selected calls inside the dispatcher until the test releases them.
struct ParkingDispatcher {
    /// Method dispatches that reached the dispatcher.
    methods_entered: Arc<AtomicUsize>,
    /// Hook submissions that reached the dispatcher.
    hooks_entered: Arc<AtomicUsize>,
    /// Parked calls wait for a permit here; the test adds permits to release them.
    release: Arc<tokio::sync::Semaphore>,
    /// Whether hook submissions park as well; method dispatches always park.
    park_hooks: bool,
}

impl AssistanceDispatcher for ParkingDispatcher {
    /// Counts the arrival, parks a method (and a hook when asked), then answers like [`TestDispatcher`].
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            match request {
                AssistanceDispatch::HookSubmit(_) => {
                    self.hooks_entered.fetch_add(1, Ordering::SeqCst);
                    if self.park_hooks {
                        let _ = self.release.acquire().await;
                    }
                    Ok(AssistanceDispatchReply::HookSubmit(
                        OpaqueJson::new("{\"hook\":true}", 64 * 1024).unwrap(),
                    ))
                }
                AssistanceDispatch::MethodDispatch(_) => {
                    self.methods_entered.fetch_add(1, Ordering::SeqCst);
                    let _ = self.release.acquire().await;
                    Ok(AssistanceDispatchReply::MethodDispatch(
                        OpaqueJson::new("{\"method\":true}", 64 * 1024).unwrap(),
                    ))
                }
            }
        })
    }
}

/// Starts an in-process daemon with `max_connections` call permits around `dispatcher`.
async fn start_limited_daemon(
    runtime_dir: &Path,
    dispatcher: Arc<ParkingDispatcher>,
    max_connections: usize,
) -> tokio::task::JoinHandle<()> {
    use agent_ide::app::config::{AppConfigPatch, ConfigLayer, ConfigOrigin, effective_config};
    let config = effective_config(
        std::num::NonZeroU64::new(1).unwrap(),
        &[ConfigLayer {
            origin: ConfigOrigin::Host,
            values: AppConfigPatch {
                ipc_max_connections: Some(max_connections),
                ..Default::default()
            },
        }],
    )
    .unwrap();
    let daemon_runtime = RuntimeDir::prepare_for_daemon(runtime_dir).unwrap();
    let task = tokio::spawn(async move {
        run_daemon_with_assistance(
            daemon_runtime,
            dispatcher,
            config,
            agent_ide::app::lease::DEFAULT_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
    });
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("limited daemon did not bind its private socket");
}

/// The one journal home of this test process, created and wired on first use.
///
/// The journal writer is initialised once per process, so every test that reads the journal
/// shares this directory and tells its own lines apart by content.
fn journal_home() -> &'static Path {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = runtime_dir();
        // SAFETY: runs once, before the journal writer exists; no test reads this variable
        // concurrently except through the same initialisation.
        unsafe { std::env::set_var(agent_ide::userhome::HOME_OVERRIDE_ENV, &home) };
        agent_ide::errorlog::init_repository("0123456789abcdef");
        home
    })
}

/// Everything the journal of [`journal_home`] holds.
fn journal_text() -> String {
    fs::read_dir(journal_home().join(".agent-ide/logs/0123456789abcdef"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

/// One context dispatch with a caller-chosen request id.
fn context_dispatch(request_id: &str) -> MethodDispatch {
    MethodDispatch::new(
        request_id,
        format!("{request_id}-correlation"),
        "attachment",
        agent_ide::app::transport::AssistanceMethod::Context,
        OpaqueJson::new("{\"path\":\"main.rs\"}", 64 * 1024).unwrap(),
    )
    .unwrap()
}

/// One hook submission with a caller-chosen request id.
fn hook_submission(request_id: &str) -> HookSubmit {
    HookSubmit::new(
        request_id,
        format!("{request_id}-correlation"),
        "attachment",
        OpaqueJson::new("{\"phase\":\"post\"}", 64 * 1024).unwrap(),
    )
    .unwrap()
}

/// F-04: a call that finds every call permit taken is answered `busy` (never dropped, so the
/// front can say nothing ran), hooks keep their own lane while calls are saturated, and the
/// refusal is counted in the journal.
#[tokio::test]
async fn saturated_call_lane_answers_busy_and_leaves_the_hook_lane_free() {
    journal_home();
    let runtime_dir = runtime_dir();
    let methods = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::clone(&methods),
        hooks_entered: Arc::new(AtomicUsize::new(0)),
        release: Arc::clone(&release),
        park_hooks: false,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 1).await;

    let parked_runtime = runtime_dir.clone();
    let parked = tokio::spawn(async move {
        dispatch_method_if_running(
            &parked_runtime,
            context_dispatch("parked"),
            transport_limits(),
        )
        .await
    });
    while methods.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let refused = dispatch_method_if_running(
        &runtime_dir,
        context_dispatch("refused"),
        transport_limits(),
    )
    .await;
    assert_eq!(refused, MethodDispatchTransportResult::Busy);
    let hook =
        submit_hook_if_running(&runtime_dir, hook_submission("hook"), transport_limits()).await;
    assert!(
        matches!(hook, HookSubmitTransportResult::Dispatched { .. }),
        "hooks keep their own lane while calls are saturated: {hook:?}"
    );
    release.add_permits(8);
    assert!(matches!(
        parked.await.unwrap(),
        MethodDispatchTransportResult::Dispatched { .. }
    ));

    let journal = journal_text();
    assert!(
        journal.contains("\"outcome\":\"refused\"") && journal.contains("connection_busy:call"),
        "the refused connection is journaled: {journal}"
    );
    stop_assistance_daemon(task, runtime_dir).await;
}

/// F-04: the hook lane is small and bounded too: a fifth concurrent hook is answered `busy`
/// while method calls, on their own lane, still dispatch.
#[tokio::test]
async fn saturated_hook_lane_answers_busy_and_leaves_the_call_lane_free() {
    let runtime_dir = runtime_dir();
    let hooks = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::new(AtomicUsize::new(0)),
        hooks_entered: Arc::clone(&hooks),
        release: Arc::clone(&release),
        park_hooks: true,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 16).await;
    let mut parked = Vec::new();
    for index in 0..4 {
        let runtime = runtime_dir.clone();
        parked.push(tokio::spawn(async move {
            submit_hook_if_running(
                &runtime,
                hook_submission(&format!("parked-{index}")),
                transport_limits(),
            )
            .await
        }));
    }
    while hooks.load(Ordering::SeqCst) < 4 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let reply = exchange(
        &runtime_dir,
        json!({
            "version": 2,
            "request_id": "fifth",
            "correlation_id": "fifth-correlation",
            "opaque_attachment": "attachment",
            "method": "assistance.hook_submit",
            "sanitized_observation_json": {"phase": "post"},
        }),
    )
    .await;
    assert_eq!(reply["status"], "busy", "{reply}");
    assert_eq!(reply["request_id"], "fifth");
    release.add_permits(8);
    let method = tokio::time::timeout(
        Duration::from_secs(2),
        dispatch_method_if_running(&runtime_dir, context_dispatch("call"), transport_limits()),
    )
    .await
    .expect("a call dispatches while the hook lane is saturated");
    assert!(matches!(
        method,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    for task in parked {
        let _ = task.await;
    }
    stop_assistance_daemon(task, runtime_dir).await;
}

/// F-04: refusals still counted in an open journal window are written when the daemon stops, so a
/// burst followed by silence loses none of them.
///
/// Three refused calls inside one window: the first line is written at once, the other two are
/// only counted, and the orderly daemon exit must flush them as one `count: 2` line.
#[tokio::test]
async fn refusal_counts_are_flushed_when_the_daemon_stops() {
    journal_home();
    let runtime_dir = runtime_dir();
    let methods = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::clone(&methods),
        hooks_entered: Arc::new(AtomicUsize::new(0)),
        release: Arc::clone(&release),
        park_hooks: false,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 1).await;
    let parked_runtime = runtime_dir.clone();
    let parked = tokio::spawn(async move {
        dispatch_method_if_running(
            &parked_runtime,
            context_dispatch("parked"),
            transport_limits(),
        )
        .await
    });
    while methods.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for index in 0..3 {
        let refused = dispatch_method_if_running(
            &runtime_dir,
            context_dispatch(&format!("refused-{index}")),
            transport_limits(),
        )
        .await;
        assert_eq!(refused, MethodDispatchTransportResult::Busy);
    }
    release.add_permits(8);
    assert!(matches!(
        parked.await.unwrap(),
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    // An idle daemon acknowledges `daemon.stop` and exits through its orderly path.
    let stop = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "stop", "method": "daemon.stop"}),
    )
    .await;
    assert_eq!(stop["status"], "ok", "{stop}");
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the daemon stops")
        .unwrap();

    let journal = journal_text();
    assert!(
        journal.contains("\"count\":2"),
        "the two refusals counted inside the window are flushed at shutdown: {journal}"
    );
    let _ = fs::remove_dir_all(&runtime_dir);
}
