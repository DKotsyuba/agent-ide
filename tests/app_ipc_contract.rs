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

/// Returns the fixed finite limits used by all real v2 transport contract scenarios.
fn transport_limits() -> HookTransportLimits {
    HookTransportLimits::new(128 * 1024, 64 * 1024, Duration::from_secs(1)).unwrap()
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

/// Proves connect and exchange share one hook or method budget using a real socket and virtual time.
#[tokio::test(start_paused = true)]
async fn hook_and_method_keep_one_total_connect_and_exchange_deadline() {
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
        tokio::select! {
            _ = &mut request => panic!("transport completed before the fake peer read its request"),
            frame = async {
                let length = socket.read_u32().await.unwrap() as usize;
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                serde_json::from_slice::<Value>(&body).unwrap()
            } => assert_eq!(frame["request_id"], "request"),
        }
        tokio::time::advance(if hook {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(10)
        })
        .await;
        assert_eq!(
            request.as_mut().poll(&mut context),
            std::task::Poll::Ready(true),
            "hook={hook}: exchange reset the original transport deadline"
        );
        drop(request);
        drop(socket);
        drop(listener);
        fs::remove_dir_all(runtime).unwrap();
    }
    clock_guard.abort();
}
