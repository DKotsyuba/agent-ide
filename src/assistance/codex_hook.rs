//! Fail-open native host hook entrypoint with a deadline independent of MCP calls.

use super::{
    codex_rendezvous::{CodexRouteIdentity, discover, effective_root},
    facade::{
        HookIngressOutcome, TrustedTransport, render_hook_context, stall_rendezvous_for_test,
        submit_hook_event,
    },
    host_binding::{HookPhase, HostKind, parse_claude_hook_event, parse_hook_event},
};
use crate::telemetry::{Telemetry, adapters};
use std::{
    io::Read,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
    },
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
/// Private authenticated-datagram endpoint owned by the daemon inside its runtime directory.
const FALLBACK_SOCKET: &str = "telemetry-fallback.sock";
/// Closed event marker prefixed to one runtime-and-attachment authenticator.
const FALLBACK_MARKER: u8 = 1;
/// Exact authenticated datagram length: one marker byte plus one BLAKE3 authenticator.
const FALLBACK_MESSAGE_BYTES: usize = 33;
/// Bounds the final kernel-datagram drain after hook ingress is closed for shutdown.
const FALLBACK_DRAIN_DEADLINE: Duration = Duration::from_millis(10);

/// Owns the daemon-side fixed-message fallback socket until it has drained at shutdown.
pub(crate) struct NativeFallbackIngress {
    /// Runtime-local socket removed only after the receiver task has stopped.
    path: PathBuf,
    /// Device identity captured from the bound socket pathname.
    device: u64,
    /// Inode identity captured from the bound socket pathname.
    inode: u64,
    /// Single-use stop signal that asks the receiver to drain already delivered datagrams.
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Receiver task joined before the telemetry writer itself is drained.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl NativeFallbackIngress {
    /// Binds the daemon-private socket and accepts only exact authenticated fallback messages.
    ///
    /// `attachments` are the immutable configured launcher credentials for this runtime. They are
    /// reduced to one-way runtime-bound authenticators in memory and are never written by ingress.
    /// A stale path is retired only when it is already an owner-only Unix socket.
    pub(crate) fn bind<'a>(
        runtime_dir: &Path,
        telemetry: Telemetry,
        attachments: impl IntoIterator<Item = &'a str>,
    ) -> std::io::Result<Self> {
        let path = runtime_dir.join(FALLBACK_SOCKET);
        retire_fallback_socket(&path)?;
        let authenticators: Vec<_> = attachments
            .into_iter()
            .map(|attachment| fallback_message(runtime_dir, attachment))
            .collect();
        let socket = tokio::net::UnixDatagram::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_socket()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "unsafe telemetry fallback socket",
            ));
        }
        let device = metadata.dev();
        let inode = metadata.ino();
        let (shutdown, mut stopping) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let mut message = [0_u8; FALLBACK_MESSAGE_BYTES + 1];
            loop {
                tokio::select! {
                    received = socket.recv(&mut message) => match received {
                        Ok(FALLBACK_MESSAGE_BYTES)
                            if authenticators.iter().any(|expected| message[..FALLBACK_MESSAGE_BYTES] == *expected) =>
                        {
                            record_fallback(&telemetry)
                        }
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
                                socket.recv(&mut message),
                            ).await {
                                Ok(Ok(FALLBACK_MESSAGE_BYTES))
                                    if authenticators.iter().any(|expected| message[..FALLBACK_MESSAGE_BYTES] == *expected) =>
                                {
                                    record_fallback(&telemetry)
                                }
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
            device,
            inode,
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
        remove_owned_socket(&self.path, self.device, self.inode);
    }
}

impl Drop for NativeFallbackIngress {
    /// Cancels an ungracefully dropped receiver and removes only its runtime-local socket path.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        remove_owned_socket(&self.path, self.device, self.inode);
    }
}

/// Removes an old fallback path only when it is an owner-only Unix socket, never a link or file.
fn retire_fallback_socket(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0 =>
        {
            std::fs::remove_file(path)
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe telemetry fallback socket path",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Removes the fallback socket only while its captured device and inode still identify the path.
fn remove_owned_socket(path: &Path, device: u64, inode: u64) {
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.file_type().is_socket() && metadata.dev() == device && metadata.ino() == inode
    }) {
        let _ = std::fs::remove_file(path);
    }
}

/// Builds the exact fixed-shape marker authenticated to one runtime and launcher attachment.
fn fallback_message(runtime_dir: &Path, attachment: &str) -> [u8; FALLBACK_MESSAGE_BYTES] {
    let key = blake3::derive_key(
        "agent-ide telemetry fallback attachment authentication v1",
        attachment.as_bytes(),
    );
    let mut hash = blake3::Hasher::new_keyed(&key);
    hash.update(runtime_dir.as_os_str().as_bytes());
    hash.update(&[FALLBACK_MARKER]);
    let mut message = [0_u8; FALLBACK_MESSAGE_BYTES];
    message[0] = FALLBACK_MARKER;
    message[1..].copy_from_slice(hash.finalize().as_bytes());
    message
}

/// Records the sole fixed native fallback event without accepting any hook content.
fn record_fallback(telemetry: &Telemetry) {
    adapters::hook_result(telemetry, &HookIngressOutcome::Unavailable);
}

/// Sends one nonblocking authenticated marker without opening SQLite and reports kernel acceptance.
fn report_native_fallback(runtime_dir: &Path, attachment: &str) -> bool {
    let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
        return false;
    };
    socket.set_nonblocking(true).is_ok()
        && socket
            .send_to(
                &fallback_message(runtime_dir, attachment),
                runtime_dir.join(FALLBACK_SOCKET),
            )
            .is_ok()
}

/// Reads one bounded native payload and submits only selected identity fields, without output.
///
/// Missing/invalid launcher attachment, malformed input, absent daemon and deadline expiry all
/// return normally. The detached reader cannot delay process exit if stdin remains open. This
/// function never creates hook-specific daemon state, retries, autostarts, or changes native tool
/// permission. After valid parsing, an unavailable/deadline result sends only a best-effort
/// fixed-shape marker authenticated to this runtime and attachment. This ingress never opens
/// SQLite, contends with its writer, persists the credential, or creates a database.
pub async fn run(runtime_dir: &Path, attachment: Option<String>, host_kind: HostKind) {
    run_with_payload(
        runtime_dir,
        attachment,
        host_kind,
        None,
        tokio::time::Instant::now() + TOTAL_DEADLINE,
    )
    .await;
}

/// Submits a caller-read bounded payload before the same absolute hook deadline expires.
///
/// `None` reads stdin itself for legacy hooks; managed Claude supplies already-read bytes so its
/// worktree can be resolved first. Returns whether a valid event reached the daemon. Invalid
/// input, transport failures, and expiry fail open with `false`.
pub async fn run_with_payload(
    runtime_dir: &Path,
    attachment: Option<String>,
    host_kind: HostKind,
    payload: Option<Vec<u8>>,
    deadline: tokio::time::Instant,
) -> bool {
    let fallback_attachment = attachment.clone();
    let valid_boundary = Arc::new(AtomicBool::new(false));
    let valid_for_hook = Arc::clone(&valid_boundary);
    let hook = tokio::time::timeout_at(deadline, async {
        let attachment = attachment?;
        TrustedTransport::from_host_ingress("hook", "hook", attachment.clone())?;
        let payload = if let Some(payload) = payload {
            payload
        } else {
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
            receiver.await.ok()??
        };
        if payload.len() > MAX_INPUT_BYTES as usize {
            return None;
        }
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
    let unavailable = matches!(hook, Some((_, HookIngressOutcome::Unavailable)))
        || (hook.is_none() && valid_boundary.load(Ordering::Acquire));
    let submitted = matches!(
        hook,
        Some((
            _,
            HookIngressOutcome::Submitted | HookIngressOutcome::Feedback(_)
        ))
    );
    if unavailable && let Some(attachment) = fallback_attachment.as_deref() {
        let _ = report_native_fallback(runtime_dir, attachment);
    }
    if let Some((event, HookIngressOutcome::Feedback(text))) = hook
        && let Some(output) = render_hook_context(&event, &text)
    {
        println!("{output}");
    }
    submitted
}

/// Runs one managed Codex hook: bounded stdin, private-route discovery, one submit, quiet failures.
///
/// Managed mode ignores every credential and runtime environment override: the daemon destination
/// is discovered solely from the payload identity (root session `session_id`, actor
/// `agent_id.unwrap_or(session_id)`) against the fixed private rendezvous records, so a missing,
/// ambiguous, stale, contended, or unusable route exits successfully with no output exactly like a
/// malformed, oversized, or late payload does. Only `PreToolUse` and `PostToolUse` are accepted;
/// every other phase exits silently. The single 250 ms deadline starts before stdin is read and
/// covers reading, parsing, discovery, connect and reply; root resolution and the blocking
/// filesystem discovery run on a detached thread reporting through a oneshot, so the deadline
/// abandons the lookup without joining it and the process still exits at the boundary even when
/// the filesystem itself stalls. Never retries, starts a daemon, runs Git, or sends the legacy
/// fallback marker.
pub async fn run_managed() {
    let deadline = tokio::time::Instant::now() + TOTAL_DEADLINE;
    let hook = tokio::time::timeout_at(deadline, async {
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
        if payload.len() > MAX_INPUT_BYTES as usize {
            return None;
        }
        let event = parse_hook_event(&payload).ok()?;
        if !matches!(event.phase(), HookPhase::Pre | HookPhase::Post) {
            return None;
        }
        let identity = CodexRouteIdentity::new(event.session_id()?, event.actor_id()).ok()?;
        // Root resolution and discovery are blocking syscalls that cannot be interrupted. Like
        // the stdin reader above, they run on a detached thread reporting through a oneshot: at
        // the deadline the wait is abandoned and the thread is never joined, so a stalled
        // filesystem cannot extend the process's exit past the total boundary.
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("codex-hook-discover".into())
            .spawn(move || {
                stall_rendezvous_for_test();
                let _ = sender.send(effective_root().and_then(|root| discover(&root, &identity)));
            })
            .ok()?;
        let target = receiver.await.ok()??;
        let correlation = event.optional_call_id().unwrap_or("post-tool-batch");
        let host = TrustedTransport::from_host_ingress(
            correlation,
            correlation,
            target.attachment().to_owned(),
        )?;
        let outcome = submit_hook_event(target.runtime_dir(), &host, &event).await;
        Some((event, outcome))
    })
    .await
    .ok()
    .flatten();
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
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&runtime).unwrap();
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

    /// Accepts only exact configured-attachment datagrams and drains the valid one at shutdown.
    #[tokio::test]
    async fn native_fallback_rejects_invalid_attachment_and_oversized_datagrams() {
        let runtime = runtime("aihf");
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&runtime).unwrap();
        let database = runtime.join("telemetry.sqlite");
        let telemetry =
            Telemetry::open_database(&database, crate::telemetry::TelemetryConfig::default())
                .await
                .unwrap();
        let socket_path = runtime.join(FALLBACK_SOCKET);
        let decoy = runtime.join("decoy");
        std::os::unix::fs::symlink(&decoy, &socket_path).unwrap();
        assert!(NativeFallbackIngress::bind(&runtime, telemetry.clone(), ["configured"]).is_err());
        assert!(!decoy.exists());
        std::fs::remove_file(&socket_path).unwrap();
        let ingress =
            NativeFallbackIngress::bind(&runtime, telemetry.clone(), ["configured"]).unwrap();
        let lock = rusqlite::Connection::open(&database).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let started = std::time::Instant::now();
        assert!(report_native_fallback(&runtime, "unconfigured"));
        let sender = std::os::unix::net::UnixDatagram::unbound().unwrap();
        sender.set_nonblocking(true).unwrap();
        let mut oversized = fallback_message(&runtime, "configured").to_vec();
        oversized.push(0);
        assert_eq!(
            sender.send_to(&oversized, &socket_path).unwrap(),
            oversized.len()
        );
        assert!(report_native_fallback(&runtime, "configured"));
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
