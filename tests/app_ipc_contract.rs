//! Contract checks for the Application layer's real private Unix IPC boundary.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

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
    for _ in 0..40 {
        if UnixStream::connect(&socket).await.is_ok() {
            return child;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("daemon did not bind its private socket");
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
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "unavailable"
    );
    assert!(!runtime_dir.exists());
}
