//! Fail-open native host hook entrypoint with a deadline independent of MCP calls.

use super::{
    facade::{HookIngressOutcome, TrustedTransport, render_hook_context, submit_hook_event},
    host_binding::{HostKind, parse_claude_hook_event, parse_hook_event},
};
use crate::telemetry::{Telemetry, adapters};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Caps raw host input before parsing; discarded fields are never sent to the daemon.
const MAX_INPUT_BYTES: u64 = 64 * 1024;
/// Bounds stdin, parsing, connect and reply together, including a silent or stuck host pipe.
const TOTAL_DEADLINE: Duration = Duration::from_millis(250);
/// Private fixed-message datagram endpoint owned by the daemon inside its runtime directory.
const FALLBACK_SOCKET: &str = "telemetry-fallback.sock";
/// Entire privacy-safe wire vocabulary for one native fallback observation.
const FALLBACK_MARKER: [u8; 1] = [1];
/// Bounds the final kernel-datagram drain after hook ingress is closed for shutdown.
const FALLBACK_DRAIN_DEADLINE: Duration = Duration::from_millis(10);

/// Owns the daemon-side fixed-message fallback socket until it has drained at shutdown.
pub(crate) struct NativeFallbackIngress {
    /// Runtime-local socket removed only after the receiver task has stopped.
    path: PathBuf,
    /// Single-use stop signal that asks the receiver to drain already delivered datagrams.
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Receiver task joined before the telemetry writer itself is drained.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl NativeFallbackIngress {
    /// Binds the daemon-private socket and starts accepting only the one-byte closed marker.
    pub(crate) fn bind(runtime_dir: &Path, telemetry: Telemetry) -> std::io::Result<Self> {
        let path = runtime_dir.join(FALLBACK_SOCKET);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let socket = tokio::net::UnixDatagram::bind(&path)?;
        let (shutdown, mut stopping) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut marker = [0_u8; 1];
            loop {
                tokio::select! {
                    received = socket.recv(&mut marker) => match received {
                        Ok(1) if marker == FALLBACK_MARKER => record_fallback(&telemetry),
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    _ = &mut stopping => {
                        let deadline = tokio::time::Instant::now() + FALLBACK_DRAIN_DEADLINE;
                        loop {
                            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                            if remaining.is_zero() {
                                break;
                            }
                            match tokio::time::timeout(
                                remaining,
                                socket.recv(&mut marker),
                            ).await {
                                Ok(Ok(1)) if marker == FALLBACK_MARKER => record_fallback(&telemetry),
                                Ok(Ok(_)) => {}
                                Ok(Err(_)) | Err(_) => break,
                            }
                        }
                        break;
                    }
                }
            }
        });
        Ok(Self {
            path,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    /// Stops ingress, drains delivered markers, joins the receiver, and removes its socket.
    pub(crate) async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for NativeFallbackIngress {
    /// Cancels an ungracefully dropped receiver and removes only its runtime-local socket path.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Records the sole fixed native fallback event without accepting any hook content.
fn record_fallback(telemetry: &Telemetry) {
    adapters::hook_result(telemetry, &HookIngressOutcome::Unavailable);
}

/// Sends one nonblocking fixed-byte marker without opening SQLite and reports kernel acceptance.
fn report_native_fallback(runtime_dir: &Path) -> bool {
    let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
        return false;
    };
    socket.set_nonblocking(true).is_ok()
        && socket
            .send_to(&FALLBACK_MARKER, runtime_dir.join(FALLBACK_SOCKET))
            .is_ok()
}

/// Reads one bounded native payload and submits only selected identity fields, without output.
///
/// Missing/invalid launcher attachment, malformed input, absent daemon and deadline expiry all
/// return normally. The detached reader cannot delay process exit if stdin remains open. This
/// function never creates hook-specific daemon state, retries, autostarts, or changes native tool
/// permission. After valid parsing, an unavailable/deadline result sends only a best-effort fixed
/// byte to the daemon's private socket. This ingress never opens SQLite, contends with its writer,
/// or creates a database.
pub async fn run(runtime_dir: &Path, attachment: Option<String>, host_kind: HostKind) {
    let valid_boundary = Arc::new(AtomicBool::new(false));
    let valid_for_hook = Arc::clone(&valid_boundary);
    let hook = tokio::time::timeout(TOTAL_DEADLINE, async {
        let attachment = attachment?;
        TrustedTransport::from_host_ingress("hook", "hook", attachment.clone())?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("codex-hook-input".into())
            .spawn(move || {
                let mut payload = Vec::new();
                let result = std::io::stdin()
                    .take(MAX_INPUT_BYTES + 1)
                    .read_to_end(&mut payload);
                let _ = sender.send(result.ok().map(|_| payload));
            })
            .ok()?;
        let payload = receiver.await.ok()??;
        let event = match host_kind {
            HostKind::Codex => parse_hook_event(&payload),
            HostKind::Claude => parse_claude_hook_event(&payload),
        }
        .ok()?;
        valid_for_hook.store(true, Ordering::Release);
        let correlation = event.optional_call_id().unwrap_or("post-tool-batch");
        let host = TrustedTransport::from_host_ingress(correlation, correlation, attachment)?;
        let outcome = submit_hook_event(runtime_dir, &host, &event).await;
        Some((event, outcome))
    })
    .await
    .ok()
    .flatten();
    if matches!(hook, Some((_, HookIngressOutcome::Unavailable)))
        || (hook.is_none() && valid_boundary.load(Ordering::Acquire))
    {
        let _ = report_native_fallback(runtime_dir);
    }
    if let Some((event, HookIngressOutcome::Feedback(text))) = hook
        && let Some(output) = render_hook_context(&event, &text)
    {
        println!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// Distinguishes short Unix-socket test paths within this process.
    static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(0);

    /// Returns one short private-runtime-shaped path beneath writable `/private/tmp`.
    fn runtime(prefix: &str) -> PathBuf {
        PathBuf::from(format!(
            "/private/tmp/{prefix}-{}-{}",
            std::process::id(),
            NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// Proves valid hook transport hits the real 250 ms boundary without consulting locked SQLite.
    #[tokio::test]
    async fn valid_hook_transport_respects_the_deadline_while_sqlite_is_locked() {
        let runtime = runtime("aiht");
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir(&runtime).unwrap();
        let listener = tokio::net::UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
        let database = runtime.join("state.sqlite");
        let lock = rusqlite::Connection::open(&database).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let event = parse_hook_event(
            br#"{"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call"}"#,
        )
        .unwrap();
        let transport =
            TrustedTransport::from_host_ingress("call", "call", "valid-attachment").unwrap();
        let started = std::time::Instant::now();
        let submitted = tokio::time::timeout(
            TOTAL_DEADLINE,
            submit_hook_event(&runtime, &transport, &event),
        )
        .await;
        assert!(submitted.is_err());
        assert!(started.elapsed() < TOTAL_DEADLINE + Duration::from_millis(100));
        drop(listener);
        drop(lock);
        let _ = std::fs::remove_file(database);
        let _ = std::fs::remove_dir(runtime);
    }

    /// Proves the fixed-byte ingress survives a SQLite lock and graceful shutdown drains its event.
    #[tokio::test]
    async fn native_fallback_marker_is_nonblocking_and_durable_at_shutdown() {
        let runtime = runtime("aihf");
        std::fs::create_dir(&runtime).unwrap();
        let database = runtime.join("telemetry.sqlite");
        let telemetry =
            Telemetry::open_database(&database, crate::telemetry::TelemetryConfig::default())
                .await
                .unwrap();
        let ingress = NativeFallbackIngress::bind(&runtime, telemetry.clone()).unwrap();
        let lock = rusqlite::Connection::open(&database).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let started = std::time::Instant::now();
        assert!(report_native_fallback(&runtime));
        assert!(started.elapsed() < TOTAL_DEADLINE);
        drop(lock);
        ingress.shutdown().await;
        telemetry.shutdown().await;
        assert_eq!(
            telemetry
                .query(crate::telemetry::Filter::All, None, 1)
                .await
                .unwrap()
                .rows
                .len(),
            1
        );
        drop(telemetry);
        let _ = std::fs::remove_dir_all(runtime);
    }
}
