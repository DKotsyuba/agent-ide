//! Idle wake behind one narrow interface (OUTBOX-r3 §8.4).
//!
//! When an idle agent's own requested result arrives, the daemon sends one explicit user message
//! into the agent's host session. [`SessionWaker`](crate::assistance::wake::SessionWaker) is the
//! only seam between the session outbox and any host route, so the owner can later replace the
//! route with a shared cross-harness system.
//!
//! The one route in revision 3 follows agent-run's Claude inbox delivery: the Claude session
//! registry (`<config>/sessions/<pid>.json`) names the session's process, state and messaging
//! socket; a token from `<pid>.*.key` authenticates one `user` frame; the inbox confirms the
//! enqueue with a `peer_message_status` receipt on a private reply socket. Only an interactive,
//! idle main session is woken. Subagents, headless sessions and Codex answer
//! [`WakeOutcome::Unavailable`](crate::assistance::wake::WakeOutcome::Unavailable). No text,
//! token or path ever enters a log.

use std::future::Future;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::host_binding::HostKind;

/// Longest wake message, in bytes (the inbox's message bound and the hook budget).
pub const MAX_WAKE_BYTES: usize = 4096;
/// Bounded wait for the inbox receipt after the frames are written.
const RECEIPT_WAIT: Duration = Duration::from_millis(1500);
/// Receipt frames are one short JSON line.
const RECEIPT_FRAME_LIMIT: usize = 4096;
/// Stray connections accepted while waiting for one receipt.
const RECEIPT_CONNECTION_LIMIT: usize = 4;
/// Most registry key files inspected for one token.
const MAX_KEY_FILES: usize = 64;
/// Most registry descriptors read while looking for one Claude session.
const MAX_REGISTRY_FILES: usize = 512;
/// Largest registry descriptor read.
const MAX_DESCRIPTOR_BYTES: u64 = 64 * 1024;

/// Returns the Claude configuration directory: `CLAUDE_CONFIG_DIR`, else `$HOME/.claude`.
pub(crate) fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))
}

/// Reads the Claude session-registry descriptor (`<registry>/<pid>.json`) whose `sessionId` is
/// `session_id`, as a JSON object; `None` when there is none, or it is unreadable or oversized.
///
/// The registry is read-only here and bounded to [`MAX_REGISTRY_FILES`] descriptors of at most
/// [`MAX_DESCRIPTOR_BYTES`]; malformed entries are skipped.
pub(crate) fn registry_entry(
    registry: &Path,
    session_id: &str,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut paths = std::fs::read_dir(registry)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().take(MAX_REGISTRY_FILES).find_map(|path| {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_DESCRIPTOR_BYTES {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
        let object = value.as_object()?;
        (object.get("sessionId").and_then(serde_json::Value::as_str) == Some(session_id))
            .then(|| object.clone())
    })
}

/// Whether `session_id` names an interactive Claude session in the registry under `registry`:
/// only such a session may hold a `SubagentStop` (OUTBOX §8.3 rule 6), because a headless
/// session's background subagent outliving its parent's result breaks agent runners.
pub(crate) fn interactive_claude_session(registry: &Path, session_id: &str) -> bool {
    registry_entry(registry, session_id).is_some_and(|entry| driven_interactively(&entry))
}

/// Whether a registry descriptor names a session a person drives: kind `interactive` and an
/// entry point other than the headless SDK ones. Claude 2.1.294 registers a headless
/// `claude --print` session (agent-run's) as kind `interactive` with entry point `sdk-cli`
/// (probe Q3), so the kind alone does not prove a person.
// ponytail: a deny-list of `sdk*` entry points; switch to an allow-list if Claude adds other
// headless entry points.
fn driven_interactively(entry: &serde_json::Map<String, serde_json::Value>) -> bool {
    let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
    field("kind") == Some("interactive")
        && !field("entrypoint").is_some_and(|entrypoint| entrypoint.starts_with("sdk"))
}

/// The host session one wake addresses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeTarget {
    /// The host contract of the session.
    pub host: HostKind,
    /// The host's own session identifier (Claude `session_id`, Codex thread id).
    pub session_id: String,
    /// True when the outbox's session is the host session's main actor, not a subagent.
    pub actor_is_main: bool,
}

/// What one wake attempt achieved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WakeOutcome {
    /// The host accepted the message (the Claude inbox held or delivered it).
    Accepted,
    /// The message may have been sent but no receipt confirmed it; never retried.
    Ambiguous,
    /// The host definitely refused the message.
    Refused,
    /// No route reaches this session (subagent, headless, Codex, busy, gone).
    Unavailable,
}

/// The only seam between the session outbox and any host wake route (OUTBOX §8.4).
///
/// Implementations must be bounded in time, never retry on their own, never log message text,
/// tokens or paths, and report [`WakeOutcome::Ambiguous`] whenever a message may have left
/// without confirmation.
pub trait SessionWaker: Send + Sync {
    /// Sends `text` (at most [`MAX_WAKE_BYTES`]) as one user message into `target`'s session.
    fn wake<'a>(
        &'a self,
        target: &'a WakeTarget,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = WakeOutcome> + Send + 'a>>;
}

/// The production waker: the Claude session-registry route, everything else unavailable.
#[derive(Clone, Debug, Default)]
pub struct HostWaker {
    /// Registry directory override for tests; `None` uses `<claude config>/sessions`.
    registry: Option<PathBuf>,
}

impl HostWaker {
    /// A waker reading the registry under `registry` instead of the Claude configuration.
    pub fn with_registry(registry: PathBuf) -> Self {
        Self {
            registry: Some(registry),
        }
    }

    /// The registry directory in use, if any can be named.
    fn registry(&self) -> Option<PathBuf> {
        self.registry
            .clone()
            .or_else(|| claude_config_dir().map(|config| config.join("sessions")))
    }
}

impl SessionWaker for HostWaker {
    /// Wakes an interactive, idle Claude main session through its inbox; every other target is
    /// unavailable.
    fn wake<'a>(
        &'a self,
        target: &'a WakeTarget,
        text: &'a str,
    ) -> Pin<Box<dyn Future<Output = WakeOutcome> + Send + 'a>> {
        Box::pin(async move {
            if target.host != HostKind::Claude || !target.actor_is_main {
                return WakeOutcome::Unavailable;
            }
            let Some(registry) = self.registry() else {
                return WakeOutcome::Unavailable;
            };
            claude_inbox_wake(&registry, &target.session_id, text).await
        })
    }
}

/// One resolved inbox: the receiving process and its messaging socket.
struct Inbox {
    /// Process id the registry names; the socket's peer must be exactly this process.
    pid: i64,
    /// Messaging socket path.
    socket: PathBuf,
}

/// Resolves an interactive, idle session's inbox from its registry descriptor.
fn resolve_inbox(registry: &Path, session_id: &str) -> Option<Inbox> {
    let entry = registry_entry(registry, session_id)?;
    let field = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
    if !driven_interactively(&entry) || field("status") != Some("idle") {
        return None;
    }
    let pid = entry.get("pid")?.as_i64().filter(|pid| *pid > 0)?;
    let socket = field("messagingSocketPath").filter(|path| !path.is_empty())?;
    Some(Inbox {
        pid,
        socket: PathBuf::from(socket),
    })
}

/// Reads the nonempty `peerToken` of process `pid` from its `<pid>.*.key` registry file.
fn inbox_token(registry: &Path, pid: i64) -> Option<String> {
    let prefix = format!("{pid}.");
    let mut paths = std::fs::read_dir(registry)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
                && path.extension().is_some_and(|extension| extension == "key")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.into_iter().take(MAX_KEY_FILES).find_map(|path| {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        value
            .get("peerToken")?
            .as_str()
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    })
}

/// Returns this process's effective user id.
fn effective_uid() -> u32 {
    // SAFETY: geteuid only reads kernel state and cannot fail.
    unsafe { libc::geteuid() }
}

/// Returns whether a connected peer is exactly process `pid` running as this user.
fn peer_is(stream: &UnixStream, pid: i64) -> bool {
    stream.peer_cred().is_ok_and(|credentials| {
        credentials.uid() == effective_uid() && credentials.pid().map(i64::from) == Some(pid)
    })
}

/// A private reply socket bound beside the inbox, removed on drop only while it is still the
/// exact inode this process bound.
struct ReplySocket {
    /// Bound path.
    path: PathBuf,
    /// Device of the bound inode.
    device: u64,
    /// Inode number of the bound inode.
    inode: u64,
}

impl Drop for ReplySocket {
    /// Removes the reply socket when it is still the inode bound here.
    fn drop(&mut self) {
        let owned = std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        });
        if owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Returns twelve hex characters unique enough for one reply socket or message id.
fn fresh_suffix() -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    hasher.update(&now.as_nanos().to_le_bytes());
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    hasher.update(
        &NEXT
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .to_le_bytes(),
    );
    hasher.finalize().to_hex()[..12].to_owned()
}

/// Binds one private reply socket in the inbox socket's directory, or `None`.
fn bind_reply(inbox: &Path) -> Option<(ReplySocket, UnixListener)> {
    let path = inbox.parent()?.join(format!("{}.sock", fresh_suffix()));
    let listener = UnixListener::bind(&path).ok()?;
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    metadata.file_type().is_socket().then(|| {
        (
            ReplySocket {
                path,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            listener,
        )
    })
}

/// Waits for this message's receipt; returns its status word.
async fn await_receipt(listener: UnixListener, msg_id: &str, pid: i64) -> Option<String> {
    let deadline = tokio::time::Instant::now() + RECEIPT_WAIT;
    for _ in 0..RECEIPT_CONNECTION_LIMIT {
        let Ok(Ok((stream, _))) = tokio::time::timeout_at(deadline, listener.accept()).await else {
            return None;
        };
        if !peer_is(&stream, pid) {
            continue;
        }
        let mut frame = Vec::new();
        let mut bounded = stream.take(RECEIPT_FRAME_LIMIT as u64 + 1);
        if !matches!(
            tokio::time::timeout_at(deadline, bounded.read_to_end(&mut frame)).await,
            Ok(Ok(_))
        ) || frame.len() > RECEIPT_FRAME_LIMIT
        {
            continue;
        }
        for line in String::from_utf8_lossy(&frame).lines() {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let field = |name: &str| value.get(name).and_then(serde_json::Value::as_str);
            if field("type") == Some("control")
                && field("action") == Some("peer_message_status")
                && field("orig_msg_id") == Some(msg_id)
                && let Some(status) = field("status")
            {
                return Some(status.to_owned());
            }
        }
    }
    None
}

/// Sends one user message into a Claude session's inbox and classifies the outcome.
async fn claude_inbox_wake(registry: &Path, session_id: &str, text: &str) -> WakeOutcome {
    if text.is_empty() || text.len() > MAX_WAKE_BYTES {
        return WakeOutcome::Refused;
    }
    let Some(inbox) = resolve_inbox(registry, session_id) else {
        return WakeOutcome::Unavailable;
    };
    let Some(token) = inbox_token(registry, inbox.pid) else {
        return WakeOutcome::Unavailable;
    };
    let reply = bind_reply(&inbox.socket);
    let mut stream = match tokio::time::timeout(
        Duration::from_secs(1),
        UnixStream::connect(&inbox.socket),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return WakeOutcome::Unavailable,
    };
    if !peer_is(&stream, inbox.pid) {
        return WakeOutcome::Unavailable;
    }
    let msg_id = format!("agent-ide-wake-{}", fresh_suffix());
    let from = reply
        .as_ref()
        .map(|(socket, _)| format!("uds:{}", socket.path.display()));
    let frames = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"auth","token":token}),
        serde_json::json!({"type":"user","session_id":session_id,"msg_id":msg_id,"from":from,
            "message":{"role":"user","content":text}}),
    );
    // Once a byte may have left, a failure is a possibly sent message, never retried.
    match tokio::time::timeout(Duration::from_secs(5), stream.write_all(frames.as_bytes())).await {
        Ok(Ok(())) => {}
        _ => return WakeOutcome::Ambiguous,
    }
    let _ = stream.shutdown().await;
    drop(stream);
    let Some((_socket, listener)) = reply else {
        return WakeOutcome::Ambiguous;
    };
    match await_receipt(listener, &msg_id, inbox.pid).await.as_deref() {
        Some("held" | "delivered") => WakeOutcome::Accepted,
        Some(_) => WakeOutcome::Refused,
        None => WakeOutcome::Ambiguous,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake Claude inbox in a private directory: one registry descriptor for this process, its
    /// key file, and a socket that answers each message with `status` (or nothing).
    struct FakeInbox {
        /// Private directory holding the registry and the socket.
        directory: PathBuf,
    }

    impl FakeInbox {
        /// Builds the registry for session `session` with `kind`, `entrypoint` and `state`, and
        /// returns the inbox with the socket path [`Self::serve`] listens on.
        fn new(tag: &str, kind: &str, entrypoint: &str, state: &str) -> (Self, PathBuf) {
            let directory = std::fs::canonicalize(std::env::temp_dir())
                .unwrap()
                .join(format!("m026-wake-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&directory);
            std::fs::create_dir_all(&directory).unwrap();
            let socket = directory.join("inbox.sock");
            let pid = std::process::id();
            std::fs::write(
                directory.join(format!("{pid}.json")),
                serde_json::json!({"pid":pid,"sessionId":"session","kind":kind,"entrypoint":entrypoint,"status":state,
                    "messagingSocketPath":socket})
                .to_string(),
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{pid}.abc.key")),
                r#"{"peerToken":"secret-token"}"#,
            )
            .unwrap();
            (Self { directory }, socket)
        }

        /// Serves one message: checks the token, then answers `status` on the reply socket.
        fn serve(socket: PathBuf, status: Option<&'static str>) -> tokio::task::JoinHandle<String> {
            let listener = UnixListener::bind(&socket).unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = String::new();
                stream.read_to_string(&mut raw).await.unwrap();
                let mut lines = raw.lines();
                let auth: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
                assert_eq!(auth["token"], "secret-token");
                let user: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
                if let Some(status) = status {
                    let from = user["from"].as_str().unwrap().strip_prefix("uds:").unwrap();
                    let mut reply = UnixStream::connect(from).await.unwrap();
                    reply
                        .write_all(
                            serde_json::json!({"type":"control","action":"peer_message_status",
                                "orig_msg_id":user["msg_id"],"status":status})
                            .to_string()
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
                user["message"]["content"].as_str().unwrap().to_owned()
            })
        }
    }

    impl Drop for FakeInbox {
        /// Removes the private directory.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    /// The main Claude target of the fake session.
    fn target() -> WakeTarget {
        WakeTarget {
            host: HostKind::Claude,
            session_id: "session".to_owned(),
            actor_is_main: true,
        }
    }

    /// A held receipt is accepted, a denial refused and silence ambiguous; the message arrives
    /// whole, and the reply socket is removed afterwards.
    #[tokio::test]
    async fn receipts_classify_the_wake() {
        for (tag, status, expected) in [
            ("held", Some("held"), WakeOutcome::Accepted),
            ("denied", Some("denied"), WakeOutcome::Refused),
            ("silent", None, WakeOutcome::Ambiguous),
        ] {
            let (inbox, socket) = FakeInbox::new(tag, "interactive", "cli", "idle");
            let served = FakeInbox::serve(socket, status);
            let waker = HostWaker::with_registry(inbox.directory.clone());
            assert_eq!(waker.wake(&target(), "facts").await, expected, "{tag}");
            assert_eq!(served.await.unwrap(), "facts");
            let leftovers = std::fs::read_dir(&inbox.directory)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    name.ends_with(".sock") && name != "inbox.sock"
                })
                .count();
            assert_eq!(leftovers, 0, "{tag}: the reply socket is removed");
        }
    }

    /// Only a descriptor naming the session with kind `interactive` and a non-SDK entry point proves
    /// an interactive session.
    #[test]
    fn registry_proves_interactive_sessions_only() {
        let registry = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("m026-registry-{}", std::process::id()));
        std::fs::create_dir_all(&registry).unwrap();
        std::fs::write(
            registry.join("100.json"),
            r#"{"pid":100,"sessionId":"live","kind":"interactive","entrypoint":"cli","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(
            registry.join("200.json"),
            r#"{"pid":200,"sessionId":"headless","kind":"sdk","status":"busy"}"#,
        )
        .unwrap();
        std::fs::write(
            registry.join("400.json"),
            r#"{"pid":400,"sessionId":"print","kind":"interactive","entrypoint":"sdk-cli","status":"idle"}"#,
        )
        .unwrap();
        std::fs::write(registry.join("300.json"), "not json").unwrap();
        assert!(interactive_claude_session(&registry, "live"));
        assert!(!interactive_claude_session(&registry, "headless"));
        assert!(!interactive_claude_session(&registry, "print"));
        assert!(!interactive_claude_session(&registry, "absent"));
        assert_eq!(
            registry_entry(&registry, "live").and_then(|entry| entry.get("status").cloned()),
            Some(serde_json::json!("idle"))
        );
        let _ = std::fs::remove_dir_all(&registry);
    }

    /// Busy, headless, subagent and Codex targets are never written to.
    #[tokio::test]
    async fn only_idle_interactive_main_sessions_are_woken() {
        let (busy, _) = FakeInbox::new("busy", "interactive", "cli", "busy");
        let waker = HostWaker::with_registry(busy.directory.clone());
        assert_eq!(waker.wake(&target(), "x").await, WakeOutcome::Unavailable);
        let (headless, _) = FakeInbox::new("headless", "sdk", "cli", "idle");
        let waker = HostWaker::with_registry(headless.directory.clone());
        assert_eq!(waker.wake(&target(), "x").await, WakeOutcome::Unavailable);
        // A headless `claude --print` session registers as interactive with an `sdk-cli` entry.
        let (print, _) = FakeInbox::new("print", "interactive", "sdk-cli", "idle");
        let waker = HostWaker::with_registry(print.directory.clone());
        assert_eq!(waker.wake(&target(), "x").await, WakeOutcome::Unavailable);
        let subagent = WakeTarget {
            actor_is_main: false,
            ..target()
        };
        assert_eq!(waker.wake(&subagent, "x").await, WakeOutcome::Unavailable);
        let codex = WakeTarget {
            host: HostKind::Codex,
            ..target()
        };
        assert_eq!(waker.wake(&codex, "x").await, WakeOutcome::Unavailable);
    }

    /// Live check, run by hand against an interactive Claude session the operator started:
    /// wakes the session named by `AGENT_IDE_LIVE_WAKE_SESSION` through the real registry; the
    /// message must then appear in that session's transcript as one new turn.
    #[tokio::test]
    #[ignore = "needs a live, idle, interactive Claude session"]
    async fn live_wake_reaches_an_idle_interactive_session() {
        let session_id = std::env::var("AGENT_IDE_LIVE_WAKE_SESSION").expect("session id");
        let target = WakeTarget {
            host: HostKind::Claude,
            session_id,
            actor_is_main: true,
        };
        let text = "<agent-ide>\ntest-finished tests #1: 3 passed, 2 s\n</agent-ide>\nResults \
                    you requested from Agent IDE arrived while you were idle.";
        // An idle session accepts the frame at once and sends no receipt (probe Q7).
        assert!(matches!(
            HostWaker::default().wake(&target, text).await,
            WakeOutcome::Accepted | WakeOutcome::Ambiguous
        ));
    }
}
