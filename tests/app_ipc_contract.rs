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

/// A dispatcher whose failure the test raises on demand, counting what it was asked to run.
struct FailableDispatcher {
    /// The failure flag; `true` once [`AssistanceDispatcher::is_failed`] must report it.
    failed: tokio::sync::watch::Sender<bool>,
    /// How many requests reached [`AssistanceDispatcher::dispatch`].
    dispatched: AtomicUsize,
}

impl FailableDispatcher {
    /// Builds a healthy dispatcher that has run nothing.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            failed: tokio::sync::watch::Sender::new(false),
            dispatched: AtomicUsize::new(0),
        })
    }
}

impl AssistanceDispatcher for FailableDispatcher {
    /// Answers like [`TestDispatcher`] and counts the request.
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
        self.dispatched.fetch_add(1, Ordering::SeqCst);
        TestDispatcher.dispatch(request)
    }

    /// Reports the flag the test raised.
    fn is_failed(&self) -> bool {
        *self.failed.borrow()
    }

    /// Resolves once the test raised the flag.
    fn failed(&self) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let mut flag = self.failed.subscribe();
        Box::pin(async move {
            let _ = flag.wait_for(|failed| *failed).await;
        })
    }
}

/// Starts the in-process daemon over `dispatcher` and returns its task, whose result the caller
/// inspects, once its private socket accepts connections.
async fn start_failable_daemon(
    runtime_dir: &Path,
    dispatcher: Arc<FailableDispatcher>,
) -> tokio::task::JoinHandle<Result<(), agent_ide::app::AppError>> {
    let daemon_runtime = RuntimeDir::prepare_for_daemon(runtime_dir).unwrap();
    let task = tokio::spawn(run_daemon_with_assistance(
        daemon_runtime,
        dispatcher,
        EffectiveConfig::defaults(),
        agent_ide::app::lease::DEFAULT_IDLE_TIMEOUT,
    ));
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("Assistance daemon did not bind its private socket");
}

/// Sends one method call and returns the transport outcome.
async fn one_context_call(runtime_dir: &Path, id: &str) -> MethodDispatchTransportResult {
    dispatch_method_if_running(
        runtime_dir,
        MethodDispatch::new(
            id,
            id,
            "attachment",
            agent_ide::app::transport::AssistanceMethod::Context,
            OpaqueJson::new("{}", 64).unwrap(),
        )
        .unwrap(),
        transport_limits(),
    )
    .await
}

/// A failed dispatcher turns the daemon crash-only (stability QW-3): health answers `restarting`,
/// a new call is refused as `restarting` without reaching the dispatcher, and the daemon then
/// exits keeping its runtime directory with the store (`state.sqlite`) and receipts, retiring only
/// the generation's launcher and attachment records.
///
/// Before the fix health answered `ok` for ever, the call was dispatched to the dead dispatcher, and
/// an orderly exit deleted the whole runtime directory.
#[tokio::test]
async fn a_failed_dispatcher_answers_restarting_and_exits_keeping_the_runtime_store() {
    let runtime_dir = runtime_dir();
    let dispatcher = FailableDispatcher::new();
    let task = start_failable_daemon(&runtime_dir, dispatcher.clone()).await;
    fs::write(runtime_dir.join("state.sqlite"), b"receipts").unwrap();
    fs::write(runtime_dir.join("launcher.json"), b"{}").unwrap();
    fs::write(runtime_dir.join("attachment"), b"record").unwrap();
    let healthy = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "before", "method": "health"}),
    )
    .await;
    assert_eq!(healthy["status"], "ok");
    assert!(matches!(
        one_context_call(&runtime_dir, "served").await,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    let before = dispatcher.dispatched.load(Ordering::SeqCst);

    dispatcher.failed.send_replace(true);
    // The daemon keeps answering for a short drain, then exits; both answers must be seen in it.
    let (mut health, mut call) = (false, false);
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(health && call) && tokio::time::Instant::now() < until && !task.is_finished() {
        if let Ok(stream) = UnixStream::connect(runtime_dir.join("agent-ide.sock")).await {
            drop(stream);
            let reply = exchange(
                &runtime_dir,
                json!({"version": 1, "request_id": "during", "method": "health"}),
            )
            .await;
            health |= reply["status"] == "restarting";
        }
        call |= one_context_call(&runtime_dir, "refused").await
            == MethodDispatchTransportResult::Restarting;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        health,
        "health must answer restarting once the dispatcher failed"
    );
    assert!(
        call,
        "a call must be refused as restarting once the dispatcher failed"
    );
    assert_eq!(
        dispatcher.dispatched.load(Ordering::SeqCst),
        before,
        "a refused call never reaches the failed dispatcher"
    );

    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("a failed daemon must exit")
        .unwrap()
        .unwrap();
    assert_eq!(
        fs::read(runtime_dir.join("state.sqlite")).unwrap(),
        b"receipts"
    );
    assert!(
        !runtime_dir.join("launcher.json").exists() && !runtime_dir.join("attachment").exists(),
        "the generation's own records are retired so a replacement writes its own at once"
    );
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// The failure flag, not the exit branch that wins, decides whether the runtime store survives: an
/// exit that ends the daemon before the fault drain does (here the idle expiry; a termination
/// signal is the same branch) must not take the orderly path that deletes the runtime directory.
///
/// The dispatcher is failed before the daemon starts and the idle timeout (50 ms) is far shorter
/// than the 500 ms fault drain, so the idle branch wins; only the flag read at exit keeps the store.
#[tokio::test]
async fn a_failure_racing_an_idle_exit_still_keeps_the_runtime_store() {
    let runtime_dir = runtime_dir();
    fs::write(runtime_dir.join("state.sqlite"), b"receipts").unwrap();
    let dispatcher = FailableDispatcher::new();
    dispatcher.failed.send_replace(true);
    let daemon_runtime = RuntimeDir::prepare_for_daemon(&runtime_dir).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        run_daemon_with_assistance(
            daemon_runtime,
            dispatcher,
            EffectiveConfig::defaults(),
            Duration::from_millis(50),
        ),
    )
    .await
    .expect("the daemon must exit")
    .unwrap();
    assert_eq!(
        fs::read(runtime_dir.join("state.sqlite")).unwrap(),
        b"receipts",
        "a failed generation keeps the store whichever exit branch ends it"
    );
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// Without a failure the same idle exit is orderly and removes the runtime directory, so the two
/// tests above pin the difference to the failure flag alone.
#[tokio::test]
async fn an_unfailed_idle_exit_removes_the_runtime_directory() {
    let runtime_dir = runtime_dir();
    let daemon_runtime = RuntimeDir::prepare_for_daemon(&runtime_dir).unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        run_daemon_with_assistance(
            daemon_runtime,
            FailableDispatcher::new(),
            EffectiveConfig::defaults(),
            Duration::from_millis(50),
        ),
    )
    .await
    .expect("the daemon must exit")
    .unwrap();
    assert!(
        !runtime_dir.exists(),
        "an orderly exit removes the runtime directory"
    );
}

/// Returns the pid of a spawned child, which the test owns and kills at the end.
fn pid_of(child: &Child) -> i32 {
    child.id().expect("a live child has a pid") as i32
}

/// Reports whether the process still exists (signal 0 only checks).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 delivers nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Evidence large enough for a forced replacement: three silent probes over 31 seconds.
const ENOUGH: (u32, Duration) = (3, Duration::from_secs(31));

/// A forced replacement needs repeated silence: fewer probes or a shorter span refuse without
/// signalling anything, even for a daemon that really is stopped (stability QW-7: a stall of a few
/// seconds keeps its daemon).
#[tokio::test]
async fn eviction_without_enough_evidence_signals_nothing() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    // SAFETY: `pid` is this test's own daemon child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    for (probes, span) in [
        (1, Duration::from_secs(60)),
        (3, Duration::from_secs(29)),
        (0, Duration::ZERO),
    ] {
        assert_eq!(
            agent_ide::app::evict_wedged_daemon(&runtime_dir, None, probes, span).await,
            agent_ide::app::EvictOutcome::Refused("insufficient_evidence"),
            "{probes} probes over {span:?}"
        );
    }
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(alive(pid), "the stalled daemon was never signalled");
    let reply = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "after-stall", "method": "health"}),
    )
    .await;
    assert_eq!(reply["status"], "ok", "it answers again once it resumes");
    stop_daemon(child, runtime_dir).await;
}

/// A daemon whose control path answers is never signalled, however much evidence the caller
/// presents: a busy daemon is not a wedged one.
#[tokio::test]
async fn eviction_never_signals_a_daemon_that_answers_health() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, None, ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("answering")
    );
    assert!(alive(pid));
    let reply = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "still-served", "method": "health"}),
    )
    .await;
    assert_eq!(reply["status"], "ok");
    stop_daemon(child, runtime_dir).await;
}

/// Holds an exclusive lock on `runtime_dir/agent-ide.lock` that records `pid`, standing in for a
/// daemon, and returns the file whose lifetime is the lock's.
fn hold_lock_recording(runtime_dir: &Path, pid: i32) -> fs::File {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(runtime_dir.join("agent-ide.lock"))
        .unwrap();
    // SAFETY: the descriptor is valid for the file's lifetime.
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    writeln!(file, "{pid}").unwrap();
    file
}

/// A lock that is not held, a holder that is not an `agent-ide` executable and this very process
/// are never signalled: the recorded pid is only a hint, validated at the moment of the call.
#[tokio::test]
async fn eviction_refuses_an_unheld_lock_a_foreign_executable_and_itself() {
    let runtime_dir = runtime_dir();
    let mut other = Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let other_pid = pid_of(&other);

    // A pid recorded in a lock file nobody holds.
    fs::write(runtime_dir.join("agent-ide.lock"), format!("{other_pid}\n")).unwrap();
    fs::set_permissions(
        runtime_dir.join("agent-ide.lock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, None, ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("not_held")
    );
    assert!(alive(other_pid));

    // A held lock whose recorded pid runs another program (a reused pid, or a stale record).
    let held = hold_lock_recording(&runtime_dir, other_pid);
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, None, ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("not_agent_ide")
    );
    assert!(alive(other_pid), "a foreign executable is never signalled");

    // A held lock that records this process.
    drop(held);
    let held = hold_lock_recording(&runtime_dir, std::process::id() as i32);
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, None, ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("dead")
    );
    drop(held);
    other.kill().await.unwrap();
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// A genuinely wedged daemon (stopped, holding the lock, answering nothing) is terminated after the
/// evidence minimum: `SIGTERM` stays pending on a stopped process, so `SIGKILL` follows the grace.
/// The lock is released, the runtime directory with the store stays, and a replacement daemon
/// starts in the same directory and answers.
#[tokio::test]
async fn a_wedged_daemon_is_terminated_and_its_replacement_starts_in_the_same_directory() {
    let runtime_dir = runtime_dir();
    let mut child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    fs::write(runtime_dir.join("state.sqlite"), b"receipts").unwrap();
    // SAFETY: `pid` is this test's own daemon child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, None, ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Terminated { pid, killed: true }
    );
    let status = child.wait().await.unwrap();
    assert!(!status.success(), "the wedged daemon was killed: {status}");
    assert_eq!(
        agent_ide::app::doctor_report(&runtime_dir)
            .await
            .unwrap()
            .lock,
        agent_ide::app::DoctorLockState::Unheld,
        "the lock is released"
    );
    assert_eq!(
        fs::read(runtime_dir.join("state.sqlite")).unwrap(),
        b"receipts"
    );

    let replacement = start_daemon(&runtime_dir).await;
    let reply = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "replacement", "method": "health"}),
    )
    .await;
    assert_eq!(reply["status"], "ok");
    assert_eq!(
        fs::read(runtime_dir.join("state.sqlite")).unwrap(),
        b"receipts"
    );
    stop_daemon(replacement, runtime_dir).await;
}

/// A forced `SIGTERM` that the resumed daemon handles as an orderly shutdown still keeps the
/// runtime store: the eviction leaves the retain marker first, so the orderly path that deletes the
/// directory is not taken. The daemon is stopped and evicted; it resumes one second after the
/// `SIGTERM`, so the pending signal is delivered and handled orderly instead of the `SIGKILL`.
#[tokio::test]
async fn a_forced_sigterm_handled_orderly_still_keeps_the_runtime_store() {
    let runtime_dir = runtime_dir();
    let mut child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    fs::write(runtime_dir.join("state.sqlite"), b"receipts").unwrap();
    // SAFETY: `pid` is this test's own daemon child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    let resume = tokio::spawn(async move {
        // After the first probe (2 s of silence) and the `SIGTERM`, which stays pending.
        tokio::time::sleep(Duration::from_secs(3)).await;
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGCONT) };
    });
    let outcome =
        agent_ide::app::evict_wedged_daemon(&runtime_dir, Some(pid), ENOUGH.0, ENOUGH.1).await;
    resume.await.unwrap();
    assert_eq!(
        outcome,
        agent_ide::app::EvictOutcome::Terminated { pid, killed: false }
    );
    let status = child.wait().await.unwrap();
    assert!(
        status.success(),
        "the daemon exited orderly on SIGTERM: {status}"
    );
    assert_eq!(
        fs::read(runtime_dir.join("state.sqlite")).unwrap(),
        b"receipts",
        "an orderly exit forced by a front keeps the store"
    );
    // The next daemon in the directory removes the spent marker and serves.
    let replacement = start_daemon(&runtime_dir).await;
    assert!(!runtime_dir.join(agent_ide::app::RETAIN_STORE_FILE).exists());
    stop_daemon(replacement, runtime_dir).await;
}

/// Evidence collected against one daemon never justifies signalling another: when the pid the watch
/// pinned is not the lock holder any more, nothing is signalled.
#[tokio::test]
async fn eviction_pinned_to_another_generation_signals_nothing() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    // SAFETY: `pid` is this test's own daemon child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, Some(pid + 1), ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("changed")
    );
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(alive(pid));
    stop_daemon(child, runtime_dir).await;
}

/// Answers every health request on `listener` with `ok` while `answering` is set and otherwise
/// accepts and holds the connection without replying, like a daemon whose control path is stuck.
fn serve_health_when(listener: UnixListener, answering: Arc<std::sync::atomic::AtomicBool>) {
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            if !answering.load(Ordering::SeqCst) {
                held.push(stream);
                continue;
            }
            let mut length = [0_u8; 4];
            if stream.read_exact(&mut length).await.is_err() {
                continue;
            }
            let mut body = vec![0; u32::from_be_bytes(length) as usize];
            if stream.read_exact(&mut body).await.is_err() {
                continue;
            }
            let request: Value = serde_json::from_slice(&body).unwrap();
            let reply = serde_json::to_vec(&json!({
                "version": 1,
                "request_id": request["request_id"],
                "status": "ok",
                "daemon_generation": "0.0.0-fake-generation-for-the-test",
            }))
            .unwrap();
            let _ = stream.write_all(&(reply.len() as u32).to_be_bytes()).await;
            let _ = stream.write_all(&reply).await;
        }
    });
}

/// A daemon that resumed and answers its control path between the `SIGTERM` and the `SIGKILL` is
/// never killed: the second probe before `SIGKILL` sees it answer.
///
/// The stand-in is a copy of the product binary named `agent-ide`, running its plain stdio `mcp`
/// mode (which installs no `SIGTERM` handler) with `SIGTERM` ignored (an ignored disposition
/// survives `exec`), so only the `SIGKILL` could end it; the test holds the lock recording its pid
/// and serves a control path that stays silent for the first probes and answers during the
/// `SIGTERM` grace.
#[tokio::test]
async fn a_daemon_that_answers_before_sigkill_is_not_killed() {
    use std::os::unix::process::CommandExt;
    let runtime_dir = runtime_dir();
    let program = runtime_dir.join("agent-ide");
    fs::copy(env!("CARGO_BIN_EXE_agent-ide"), &program).unwrap();
    let mut command = std::process::Command::new(&program);
    command
        .args(["mcp", "--runtime-dir"])
        .arg(runtime_dir.join("unused"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe `signal` runs between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut standin = tokio::process::Command::from(command);
    let mut standin = standin.kill_on_drop(true).spawn().unwrap();
    let pid = pid_of(&standin);
    let held = hold_lock_recording(&runtime_dir, pid);
    let answering = Arc::new(std::sync::atomic::AtomicBool::new(false));
    serve_health_when(
        UnixListener::bind(runtime_dir.join("agent-ide.sock")).unwrap(),
        answering.clone(),
    );
    let flip = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(4)).await;
        answering.store(true, Ordering::SeqCst);
    });
    let outcome =
        agent_ide::app::evict_wedged_daemon(&runtime_dir, Some(pid), ENOUGH.0, ENOUGH.1).await;
    flip.await.unwrap();
    assert_eq!(outcome, agent_ide::app::EvictOutcome::Refused("answering"));
    assert!(alive(pid), "an answering daemon is never killed");
    drop(held);
    standin.kill().await.unwrap();
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// Without the retain marker an orderly `SIGTERM` exit would delete the receipts, so an eviction
/// that cannot create it (here the name is a symlink, which is never followed) signals nothing.
#[tokio::test]
async fn eviction_that_cannot_create_the_retain_marker_signals_nothing() {
    let runtime_dir = runtime_dir();
    let child = start_daemon(&runtime_dir).await;
    let pid = pid_of(&child);
    std::os::unix::fs::symlink(
        "/dev/null",
        runtime_dir.join(agent_ide::app::RETAIN_STORE_FILE),
    )
    .unwrap();
    // SAFETY: `pid` is this test's own daemon child.
    unsafe { libc::kill(pid, libc::SIGSTOP) };
    assert_eq!(
        agent_ide::app::evict_wedged_daemon(&runtime_dir, Some(pid), ENOUGH.0, ENOUGH.1).await,
        agent_ide::app::EvictOutcome::Refused("retain_failed")
    );
    // SAFETY: as above.
    unsafe { libc::kill(pid, libc::SIGCONT) };
    assert!(alive(pid), "nothing was signalled");
    stop_daemon(child, runtime_dir).await;
}
