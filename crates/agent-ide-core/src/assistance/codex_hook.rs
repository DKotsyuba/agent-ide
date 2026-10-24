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

/// Bounds the projected identity payload handed to the parser and the daemon transport. A larger
/// raw payload (megabytes of `tool_input`/`tool_response` the daemon never consumes) is streamed
/// through [`read_hook_payload`], which keeps only the projected fields and buffers nothing else.
const MAX_INPUT_BYTES: u64 = 64 * 1024;
/// Top-level hook fields the daemon's observation consumes, copied by every projection.
const PROJECTED_FIELDS: [&str; 6] = [
    "hook_event_name",
    "session_id",
    "agent_id",
    "tool_use_id",
    "tool_name",
    "source",
];
/// Claude writer-tool path fields kept from `tool_input` (`NotebookEdit` carries `notebook_path`).
const TOOL_INPUT_PATHS: [&str; 2] = ["file_path", "notebook_path"];
/// Error text a kept value over the transport bound raises inside the streaming visitor.
const OVERSIZE_FIELD: &str = "projected hook field over the transport bound";
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

/// Reads one raw host payload from stdin through [`read_hook_payload`]; a projection that still
/// exceeds the transport bound names the closed oversize fact.
async fn read_projected_stdin(host_kind: HostKind) -> Option<Vec<u8>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("codex-hook-input".into())
        .spawn(move || {
            let _ = sender.send(read_hook_payload(std::io::stdin().lock(), host_kind, false));
        })
        .ok()?;
    match receiver.await.ok()? {
        Ok(payload) => Some(payload),
        Err(HookInputError::Oversize) => {
            log_oversize();
            None
        }
        Err(_) => None,
    }
}

/// Why one host hook payload could not be read or projected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookInputError {
    /// Reading the host pipe failed.
    Read,
    /// The payload is not one JSON object, or repeats a projected field.
    Malformed,
    /// Even the projected fields exceed the 64 KiB transport bound.
    Oversize,
}

/// Reads one host hook payload of any size without buffering the bodies the daemon never uses.
///
/// A payload within the 64 KiB transport bound keeps its exact bytes (so the parser still rejects
/// duplicate keys). A larger one is streamed: only the projected top-level fields (plus `cwd` when
/// `keep_cwd`, which the Claude router needs to route the event, and Claude's `agent_type` and
/// writer-tool path) are kept, a repeated projected field is malformed, and every other value —
/// multi-megabyte `tool_input`/`tool_response` bodies included — is skipped as it is read.
///
/// Memory: skipped values are never buffered, objects and arrays in kept fields are drained, but
/// `serde_json` decodes one whole string token (a kept value or any object key) before the
/// visitor can bound it, so peak memory is the size of the longest such single token, not of
/// the payload. The input comes from the local host's hook pipe within the hook deadline.
pub fn read_hook_payload(
    mut reader: impl Read,
    host_kind: HostKind,
    keep_cwd: bool,
) -> Result<Vec<u8>, HookInputError> {
    let mut head = Vec::new();
    (&mut reader)
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut head)
        .map_err(|_| HookInputError::Read)?;
    if head.len() <= MAX_INPUT_BYTES as usize {
        return Ok(head);
    }
    project_reader(
        std::io::BufReader::with_capacity(256 * 1024, std::io::Cursor::new(head).chain(reader)),
        host_kind,
        keep_cwd,
    )
}

/// Streams one hook object from `reader` into its bounded projection.
fn project_reader(
    reader: impl Read,
    host_kind: HostKind,
    keep_cwd: bool,
) -> Result<Vec<u8>, HookInputError> {
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let error = |error: serde_json::Error| {
        if error.is_io() {
            HookInputError::Read
        } else if error.to_string().starts_with(OVERSIZE_FIELD) {
            HookInputError::Oversize
        } else {
            HookInputError::Malformed
        }
    };
    let projected = serde::Deserializer::deserialize_map(
        &mut deserializer,
        Projection {
            host_kind,
            keep_cwd,
        },
    )
    .map_err(error)?;
    deserializer.end().map_err(error)?;
    let bytes = serde_json::to_vec(&serde_json::Value::Object(projected))
        .map_err(|_| HookInputError::Malformed)?;
    if bytes.len() > MAX_INPUT_BYTES as usize {
        return Err(HookInputError::Oversize);
    }
    Ok(bytes)
}

/// Visits one top-level hook object, keeping only the projected fields.
struct Projection {
    host_kind: HostKind,
    keep_cwd: bool,
}

impl<'de> serde::de::Visitor<'de> for Projection {
    type Value = serde_json::Map<String, serde_json::Value>;

    /// Names the only accepted shape: one JSON object.
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("one hook payload object")
    }

    /// Copies projected fields, refuses a repeated one, and skips every other value unbuffered.
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let claude = self.host_kind == HostKind::Claude;
        let mut out = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let kept = PROJECTED_FIELDS.contains(&key.as_str())
                || (claude && key == "agent_type")
                || (self.keep_cwd && key == "cwd");
            let tool_input = claude && key == "tool_input";
            if (kept || tool_input) && out.contains_key(&key) {
                return Err(serde::de::Error::custom("repeated hook field"));
            }
            if kept {
                let value = map.next_value_seed(KeptValue {
                    drop_oversize: false,
                })?;
                out.insert(key, value);
            } else if tool_input {
                let paths = map.next_value_seed(ToolInputPaths)?;
                out.insert(key, serde_json::Value::Object(paths));
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        // A `tool_input` without a kept path projects to nothing, as an absent one does.
        if out
            .get("tool_input")
            .and_then(serde_json::Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            out.remove("tool_input");
        }
        Ok(out)
    }
}

/// Reads one kept value without materializing structure: a string over the transport bound is
/// oversize, a scalar is kept, and an object or array (never a valid identity or path) is drained
/// unbuffered and kept as an empty one of the same type, so the parser refuses it exactly as it
/// would refuse the original. With `drop_oversize` (a `tool_input` path, which the parser drops
/// rather than refusing the event when invalid) an over-bound string is kept as `null` instead.
struct KeptValue {
    drop_oversize: bool,
}

impl<'de> serde::de::DeserializeSeed<'de> for KeptValue {
    type Value = serde_json::Value;

    /// Accepts any JSON value.
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for KeptValue {
    type Value = serde_json::Value;

    /// Names the accepted shape: any JSON value.
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a hook field value")
    }

    /// Keeps a string within the transport bound; a longer one is oversize (or `null` when
    /// `drop_oversize`).
    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        if value.len() <= MAX_INPUT_BYTES as usize {
            Ok(serde_json::Value::String(value.to_owned()))
        } else if self.drop_oversize {
            Ok(serde_json::Value::Null)
        } else {
            Err(E::custom(OVERSIZE_FIELD))
        }
    }

    /// Keeps a boolean.
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }

    /// Keeps a signed number.
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    /// Keeps an unsigned number.
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    /// Keeps a finite float; any other float projects to `null`, as `serde_json` would.
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(serde_json::Number::from_f64(value).map_or(serde_json::Value::Null, Into::into))
    }

    /// Keeps `null`.
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    /// Drains an array unbuffered; an empty array stands in for it.
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(serde_json::Value::Array(Vec::new()))
    }

    /// Drains an object unbuffered; an empty object stands in for it.
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        while map
            .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
            .is_some()
        {}
        Ok(serde_json::Value::Object(serde_json::Map::new()))
    }
}

/// Reads Claude's `tool_input`, keeping only its writer-tool path fields exactly as the parser
/// sees them: each present key with its last value (a non-string value is kept as `null`, which
/// the parser treats like the original: present, but no path), every other field skipped.
struct ToolInputPaths;

impl<'de> serde::de::DeserializeSeed<'de> for ToolInputPaths {
    type Value = serde_json::Map<String, serde_json::Value>;

    /// Accepts any JSON value: only an object can carry a kept path.
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for ToolInputPaths {
    type Value = serde_json::Map<String, serde_json::Value>;

    /// Names the accepted shape: any JSON value.
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a tool_input value")
    }

    /// Keeps the last value of each path key (a string, or `null` for anything else); skips the rest.
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut paths = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if TOOL_INPUT_PATHS.contains(&key.as_str()) {
                let value = match map.next_value_seed(KeptValue {
                    drop_oversize: true,
                })? {
                    path @ serde_json::Value::String(_) => path,
                    _ => serde_json::Value::Null,
                };
                paths.insert(key, value);
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(paths)
    }

    /// Drains an array value without keeping it.
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(serde_json::Map::new())
    }

    /// A scalar carries no path.
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }

    /// A scalar carries no path.
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }

    /// A scalar carries no path.
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }

    /// A scalar carries no path.
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }

    /// A scalar carries no path.
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }

    /// `null` carries no path.
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Map::new())
    }
}

/// Records that even a projected payload could not fit the transport bound; warn, not skipped.
fn log_oversize() {
    crate::errorlog::record(
        crate::errorlog::Method::Hook,
        crate::errorlog::Outcome::Unavailable,
        crate::errorlog::Fields {
            detail: Some("hook_input_oversize"),
            ..Default::default()
        },
    );
}

/// Projects one raw host payload down to the bounded identity fields the daemon consumes.
///
/// The daemon's observation uses only the host, phase, actor, session, and optional call/tool-name
/// fields, so multi-megabyte `tool_input`/`tool_response` bodies are dropped rather than the whole
/// event — except Claude's bounded `tool_input.file_path`/`notebook_path`, which a projected
/// writer post keeps so its check trigger can still name the changed file's language. A repeated
/// projected field is refused. Returns `None` when the payload is not a JSON object or even the
/// projection exceeds the transport bound (oversized identifiers).
pub fn project_hook_payload(host_kind: HostKind, payload: &[u8]) -> Option<Vec<u8>> {
    project_reader(payload, host_kind, false).ok()
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
/// input, transport failures, and expiry fail open with `false`; reading, parsing and submission all
/// stay inside `deadline`.
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
    let parsed = tokio::time::timeout_at(deadline, async {
        let attachment = attachment?;
        TrustedTransport::from_host_ingress("hook", "hook", attachment.clone())?;
        let payload = if let Some(payload) = payload {
            // A caller-supplied payload is either already small or already projected.
            if payload.len() > MAX_INPUT_BYTES as usize {
                project_hook_payload(host_kind, &payload)?
            } else {
                payload
            }
        } else {
            read_projected_stdin(host_kind).await?
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
        Some((event, attachment))
    })
    .await
    .ok()
    .flatten();
    let hook = match parsed {
        Some((event, attachment)) => tokio::time::timeout_at(deadline, async {
            let correlation = event.optional_call_id().unwrap_or("post-tool-batch");
            let host = TrustedTransport::from_host_ingress(correlation, correlation, attachment)?;
            let outcome = submit_hook_event(runtime_dir, &host, &event).await;
            Some((event, outcome))
        })
        .await
        .ok()
        .flatten(),
        None => None,
    };
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
/// malformed, oversized, or late payload does. `PreToolUse`, `PostToolUse` and
/// `PreCompact` are accepted; every other phase exits silently. The single
/// 250 ms deadline starts before stdin is read and covers reading, parsing, discovery, connect and
/// reply. Root resolution and the blocking filesystem discovery run on a detached thread reporting through a
/// oneshot, so the deadline abandons the lookup without joining it and the process still exits at
/// the boundary even when the filesystem itself stalls. Never retries, starts a daemon, runs Git,
/// or sends the legacy fallback marker.
pub async fn run_managed() {
    let deadline = tokio::time::Instant::now() + TOTAL_DEADLINE;
    let parsed = tokio::time::timeout_at(deadline, async {
        let payload = read_projected_stdin(HostKind::Codex).await?;
        if payload.len() > MAX_INPUT_BYTES as usize {
            return None;
        }
        parse_hook_event(&payload).ok()
    })
    .await
    .ok()
    .flatten();
    let Some(event) = parsed else {
        return;
    };
    let hook = tokio::time::timeout_at(deadline, async {
        if !matches!(
            event.phase(),
            HookPhase::Pre | HookPhase::Post | HookPhase::Compaction
        ) {
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

    /// A one-megabyte Write tool body no longer drops the event: the projection keeps exactly the
    /// identity fields, and a Pre/Post pair still parses and correlates as before.
    #[test]
    fn large_tool_bodies_project_and_still_pair_pre_and_post() {
        let body = "x".repeat(1024 * 1024);
        let lifecycle = |phase: &str| {
            serde_json::json!({
                "hook_event_name": phase,
                "session_id": "session",
                "tool_use_id": "call",
                "tool_name": "Write",
                "cwd": "/private/tmp/project",
                "tool_input": {"file_path": "/private/tmp/project/a.txt", "content": body},
            })
            .to_string()
        };
        for phase in ["PreToolUse", "PostToolUse"] {
            let raw = lifecycle(phase);
            assert!(raw.len() > MAX_INPUT_BYTES as usize);
            let projected =
                project_hook_payload(HostKind::Claude, raw.as_bytes()).expect("projection fits");
            assert!(projected.len() < 512, "{}", projected.len());
            let event = parse_claude_hook_event(&projected).expect("projected payload parses");
            assert_eq!(event.session_id(), Some("session"));
            assert_eq!(event.optional_call_id(), Some("call"));
            assert_eq!(
                event.tool_name(),
                phase.eq("PostToolUse").then_some("Write")
            );
            // The megabyte body is gone (`xx` never occurs; only the single `x` of `.txt`
            // survives), while the bounded `file_path` a post's check trigger needs is kept —
            // parsed out for a post, discarded for a pre.
            let text = String::from_utf8(projected).unwrap();
            assert!(!text.contains("xx"), "{text}");
            assert_eq!(
                event.tool_file(),
                phase
                    .eq("PostToolUse")
                    .then_some("/private/tmp/project/a.txt")
            );
        }
        // A Codex payload keeps its own field set, and non-object input never projects.
        let codex = project_hook_payload(
            HostKind::Codex,
            br#"{"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":"call","anything":"else"}"#,
        )
        .unwrap();
        assert!(parse_hook_event(&codex).is_ok());
        assert!(project_hook_payload(HostKind::Claude, b"[1,2,3]").is_none());
        // A projected identity itself over the transport bound cannot fit and is refused.
        let oversized = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s".repeat(MAX_INPUT_BYTES as usize),
        })
        .to_string();
        assert!(project_hook_payload(HostKind::Claude, oversized.as_bytes()).is_none());
    }

    /// A Post of any size (here about 20 MiB of `tool_response` and `tool_input` body) streams
    /// through: its projection equals that of the same event with small bodies, keeps the routing
    /// `cwd` only when asked, and parses to the same event; a repeated identity field is
    /// malformed and an identity that alone exceeds the transport bound is oversize.
    #[test]
    fn huge_hook_payloads_stream_to_the_same_projection() {
        let event = |body: &str| {
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "session_id": "session",
                "tool_use_id": "call",
                "tool_name": "NotebookEdit",
                "agent_type": "general-purpose",
                "cwd": "/private/tmp/project",
                "transcript_path": "/private/tmp/t.jsonl",
                "tool_input": {"notebook_path": "/private/tmp/project/n.ipynb", "new_source": body, "cells": [1, {"x": body}]},
                "tool_response": {"content": [{"type": "text", "text": body}], "isError": false},
            })
            .to_string()
        };
        let huge = event(&"é\"x".repeat(3 * 1024 * 1024));
        assert!(huge.len() > 16 * 1024 * 1024, "{}", huge.len());
        let started = std::time::Instant::now();
        let projected = read_hook_payload(huge.as_bytes(), HostKind::Claude, true).unwrap();
        eprintln!("streamed {} bytes in {:?}", huge.len(), started.elapsed());
        let small = event("ab");
        let mut small_projection: serde_json::Value = serde_json::from_slice(
            &project_reader(small.as_bytes(), HostKind::Claude, true).unwrap(),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&projected).unwrap(),
            small_projection
        );
        assert_eq!(small_projection["cwd"], "/private/tmp/project");
        assert_eq!(
            small_projection["tool_input"],
            serde_json::json!({"notebook_path": "/private/tmp/project/n.ipynb"})
        );
        assert!(small_projection.get("tool_response").is_none());
        // The parsed event equals that of the exact small payload.
        let parsed = parse_claude_hook_event(&projected).unwrap();
        let exact = parse_claude_hook_event(small.as_bytes()).unwrap();
        assert_eq!(parsed.session_id(), exact.session_id());
        assert_eq!(parsed.optional_call_id(), exact.optional_call_id());
        assert_eq!(parsed.tool_name(), exact.tool_name());
        assert_eq!(parsed.tool_file(), exact.tool_file());
        assert_eq!(parsed.agent_type(), exact.agent_type());
        // Without `keep_cwd` the routing field is not forwarded.
        small_projection.as_object_mut().unwrap().remove("cwd");
        let no_cwd = read_hook_payload(huge.as_bytes(), HostKind::Claude, false).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&no_cwd).unwrap(),
            small_projection
        );
        // A small payload keeps its exact bytes.
        assert_eq!(
            read_hook_payload(small.as_bytes(), HostKind::Claude, true).unwrap(),
            small.as_bytes()
        );
        let padding = "x".repeat(MAX_INPUT_BYTES as usize);
        let repeated =
            format!(r#"{{"session_id":"a","tool_response":"{padding}","session_id":"b"}}"#);
        assert_eq!(
            read_hook_payload(repeated.as_bytes(), HostKind::Claude, false),
            Err(HookInputError::Malformed)
        );
        let oversized = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "session_id": "s".repeat(MAX_INPUT_BYTES as usize),
        })
        .to_string();
        assert_eq!(
            read_hook_payload(oversized.as_bytes(), HostKind::Claude, false),
            Err(HookInputError::Oversize)
        );
        let not_object = "x".repeat(MAX_INPUT_BYTES as usize + 1);
        assert_eq!(
            read_hook_payload(not_object.as_bytes(), HostKind::Claude, false),
            Err(HookInputError::Malformed)
        );
    }

    /// A projected large payload parses to exactly what the parser makes of the same payload with
    /// small bodies: path precedence (`file_path` present but not a string blocks
    /// `notebook_path`), last-value duplicates, non-string and over-long paths, and a non-object
    /// `tool_input` all agree; an object-valued identity is refused either way.
    #[test]
    fn projection_matches_the_parser_for_every_tool_input_shape() {
        let long = "p".repeat(2000);
        let cases = [
            r#"{"file_path":"/w/a.rs"}"#.to_owned(),
            r#"{"notebook_path":"/w/n.ipynb"}"#.to_owned(),
            r#"{"file_path":null,"notebook_path":"/w/a.py"}"#.to_owned(),
            r#"{"file_path":"/w/a.rs","file_path":5}"#.to_owned(),
            r#"{"file_path":5,"file_path":"/w/b.rs"}"#.to_owned(),
            r#"{"file_path":{"x":1},"notebook_path":"/w/b.ipynb"}"#.to_owned(),
            r#""just text""#.to_owned(),
            format!(r#"{{"file_path":"{long}"}}"#),
        ];
        let event = |tool_input: &str, padding: &str| {
            format!(
                r#"{{"hook_event_name":"PostToolUse","session_id":"s","tool_use_id":"c","tool_name":"Write","agent_type":"t","tool_input":{tool_input},"tool_response":"{padding}"}}"#
            )
        };
        let padding = "x".repeat(MAX_INPUT_BYTES as usize + 1);
        for case in &cases {
            let small = event(case, "");
            let large = event(case, &padding);
            let exact = parse_claude_hook_event(small.as_bytes()).unwrap();
            let projected = read_hook_payload(large.as_bytes(), HostKind::Claude, false).unwrap();
            let parsed = parse_claude_hook_event(&projected).unwrap();
            assert_eq!(parsed.tool_file(), exact.tool_file(), "{case}");
            assert_eq!(parsed.session_id(), exact.session_id(), "{case}");
            assert_eq!(
                parsed.optional_call_id(),
                exact.optional_call_id(),
                "{case}"
            );
            assert_eq!(parsed.tool_name(), exact.tool_name(), "{case}");
            assert_eq!(parsed.agent_type(), exact.agent_type(), "{case}");
        }
        // A path over the transport bound is dropped like an invalid one, not the event.
        let huge_path = event(
            &format!(
                r#"{{"file_path":"{}"}}"#,
                "p".repeat(MAX_INPUT_BYTES as usize + 1)
            ),
            "",
        );
        let projected = read_hook_payload(huge_path.as_bytes(), HostKind::Claude, false).unwrap();
        assert_eq!(
            parse_claude_hook_event(&projected).unwrap().tool_file(),
            None
        );
        // An object-valued identity is refused by the parser for both shapes.
        let object_session = |padding: &str| {
            format!(
                r#"{{"hook_event_name":"PreToolUse","session_id":{{"a":"{}"}},"tool_use_id":"c","tool_response":"{padding}"}}"#,
                "b".repeat(16)
            )
        };
        assert!(parse_claude_hook_event(object_session("").as_bytes()).is_err());
        let projected =
            read_hook_payload(object_session(&padding).as_bytes(), HostKind::Claude, false)
                .unwrap();
        assert!(parse_claude_hook_event(&projected).is_err());
    }

    /// Distinguishes short Unix-socket test paths within this process.
    static NEXT_RUNTIME: AtomicU64 = AtomicU64::new(0);
    /// Returns one short private-runtime-shaped path beneath the canonical environment-selected temporary parent.
    fn runtime(prefix: &str) -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "{prefix}-{}-{}",
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
