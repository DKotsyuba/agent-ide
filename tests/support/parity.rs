//! Shared parity-transcript harness for bundled language modules.
//!
//! A test builds one committed [`Fixture`], runs the same MCP calls on a fresh configured daemon
//! once with `AGENT_IDE_LANGUAGE_MODE=<language>=in_process` and once without it
//! ([`transcript`]), and compares the replies after masking per-daemon tokens ([`normalized`],
//! [`assert_parity`]). [`Transcript::tree`] records the daemon's process tree while the session
//! was live, so a test can assert that a module is a direct daemon child, that the provider's
//! parent is the module, and that no language server is a direct daemon child. Every process the
//! daemon started is terminated when the [`Daemon`] drops, on success and on failure alike.
#![allow(
    dead_code,
    reason = "each test binary uses a different part of the harness"
)]

use std::{
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// The fallback switch the harness flips.
pub const LANGUAGE_MODE: &str = "AGENT_IDE_LANGUAGE_MODE";
/// Trusted fixture channel attachment.
const ATTACHMENT: &str = "parity-host-channel";
/// Distinguishes fixture trees of concurrently running tests.
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Next tool call id, shared by every front of the test process: a daemon refuses a call id it
/// already processed as a replay, so two fronts on one daemon must never reuse one.
static CALL: AtomicUsize = AtomicUsize::new(100);

/// The shipping binary under test.
pub fn binary() -> PathBuf {
    std::env::var_os("AGENT_IDE_PRODUCT_BINARY").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_agent-ide")),
        PathBuf::from,
    )
}

/// One exact process: PID plus kernel start time, so a reused PID never matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    /// Process id.
    pub pid: libc::pid_t,
    /// Kernel start time (seconds, microseconds).
    started: (u64, u64),
}

impl ProcessIdentity {
    /// The identity, parent and run state of `pid`, or `None` once it is gone.
    pub fn of(pid: libc::pid_t) -> Option<(Self, libc::pid_t, u32)> {
        // SAFETY: an all-zero `proc_bsdinfo` is a valid plain-data value for the kernel to fill.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is writable for `size` bytes; the call only reads kernel process data.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        (written == size).then_some((
            Self {
                pid,
                started: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            },
            info.pbi_ppid as libc::pid_t,
            info.pbi_status,
        ))
    }

    /// Whether the kernel still lists this exact process (a zombie included).
    pub fn exists(self) -> bool {
        Self::of(self.pid).is_some_and(|(now, _, _)| now == self)
    }

    /// Whether this exact process still runs (not a zombie).
    fn running(self) -> bool {
        Self::of(self.pid).is_some_and(|(now, _, state)| now == self && state != libc::SZOMB)
    }

    /// The live direct children of `parent`; call it only while `parent` is provably ours.
    pub fn children_of(parent: libc::pid_t) -> Vec<Self> {
        let mut pids = vec![0 as libc::pid_t; 1024];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: `pids` is writable for `bytes` bytes and the call only lists kernel PIDs.
        let listed = unsafe { libc::proc_listchildpids(parent, pids.as_mut_ptr().cast(), bytes) };
        pids.into_iter()
            .take(usize::try_from(listed).unwrap_or(0))
            .filter(|pid| *pid > 0)
            .filter_map(Self::of)
            .filter(|(_, ppid, _)| *ppid == parent)
            .map(|(identity, _, _)| identity)
            .collect()
    }

    /// The command line, empty once the process is gone.
    pub fn command(self) -> String {
        let listing = std::process::Command::new("/bin/ps")
            .args(["-o", "command=", "-p", &self.pid.to_string()])
            .output()
            .map(|output| output.stdout)
            .unwrap_or_default();
        String::from_utf8_lossy(&listing).trim().to_owned()
    }

    /// Waits up to five seconds for the process to be gone; returns whether it is.
    pub async fn gone(self) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while self.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !self.exists()
    }

    /// `SIGTERM`, then `SIGKILL` after five seconds, to every still-running process of `owned`;
    /// a group leader is signalled as its group.
    pub fn terminate(owned: &[Self]) {
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            if !owned.iter().any(|id| id.exists()) {
                return;
            }
            for id in owned.iter().copied().filter(|id| id.running()) {
                // SAFETY: `id` was verified a moment ago to be the exact captured process; a
                // group is addressed only when that process leads it.
                unsafe {
                    let leads = libc::getpgid(id.pid) == id.pid;
                    libc::kill(if leads { -id.pid } else { id.pid }, signal);
                }
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while std::time::Instant::now() < deadline && owned.iter().any(|id| id.exists()) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// A private committed worktree plus the daemon's runtime directory and launcher config.
pub struct Fixture {
    /// Private parent, removed on drop.
    pub base: PathBuf,
    /// The worktree.
    pub root: PathBuf,
    /// Daemon runtime directory.
    pub runtime: PathBuf,
    /// Launcher configuration.
    pub config: PathBuf,
}

impl Fixture {
    /// Writes `files`, commits them without user Git configuration, and configures `providers`.
    pub fn new(files: &[(&str, &str)], providers: Value) -> Self {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "parity-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let root = base.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Self {
            runtime: base.join("ipc"),
            config: base.join("launcher.json"),
            base,
            root,
        };
        for (path, text) in files {
            let path = fixture.root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        fixture.git(&["init", "--quiet"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fixture.git(&["config", "user.name", "Parity Fixture"]);
        fixture.git(&["add", "--", "."]);
        fixture.git(&["commit", "--quiet", "-m", "fixture"]);
        let git = std::fs::read("/usr/bin/git").unwrap();
        let config = json!({"version":1,"limits":{"queued":16,"details":64,"operation_ms":120000,"output_bytes":1_048_576},
            "allowed_roots":[fixture.base],
            "targets":[{"attachment":ATTACHMENT,"candidate":fixture.root,
                "git":{"path":"/usr/bin/git","identity":"fixture-git","blake3":blake3::hash(&git).to_hex().to_string()},
                "providers":providers}]});
        std::fs::write(&fixture.config, config.to_string()).unwrap();
        fixture
    }

    /// Runs fixture Git with user and system configuration excluded.
    pub fn git(&self, args: &[&str]) {
        let output = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            // Identical fixtures commit to identical SHAs.
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture Git failed");
    }

    /// Host metadata the fixture's actor reports.
    fn state(&self) -> Value {
        json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":self.root,"useLegacyLandlock":false})
    }
}

impl Drop for Fixture {
    /// Removes the private tree; the [`Daemon`] has already stopped.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// A configured daemon whose descendants die with it.
pub struct Daemon {
    /// The daemon; its unreaped handle proves the PID is ours.
    child: Child,
    /// Direct children seen while it was alive.
    owned: Vec<ProcessIdentity>,
}

impl Daemon {
    /// Starts a daemon on `fixture` with `env` and waits for its endpoint.
    pub async fn start(fixture: &Fixture, env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(binary());
        command
            .env_remove(LANGUAGE_MODE)
            .envs(env.iter().copied())
            .env("CARGO_TARGET_DIR", fixture.base.join("cargo-target"))
            .args(["daemon", "--runtime-dir"])
            .arg(&fixture.runtime)
            .env("AGENT_IDE_LAUNCHER_CONFIG", &fixture.config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut daemon = Self {
            child: command.spawn().unwrap(),
            owned: Vec::new(),
        };
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if UnixStream::connect(fixture.runtime.join("agent-ide.sock"))
                    .await
                    .is_ok()
                {
                    return;
                }
                if let Some(status) = daemon.child.try_wait().unwrap() {
                    let mut stderr = String::new();
                    let _ = daemon
                        .child
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut stderr)
                        .await;
                    panic!("daemon exited with {status}: {stderr}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the daemon endpoint within 30 s");
        daemon
    }

    /// The daemon's PID.
    pub fn pid(&self) -> libc::pid_t {
        self.child.id().expect("a live daemon") as libc::pid_t
    }

    /// Records the daemon's current direct children for cleanup.
    fn adopt_children(&mut self) {
        if let Some(pid) = self.child.id() {
            for child in ProcessIdentity::children_of(pid as libc::pid_t) {
                if !self.owned.contains(&child) {
                    self.owned.push(child);
                }
            }
        }
    }

    /// The daemon's direct children and each one's children, with command lines.
    pub fn tree(&mut self) -> ProcessTree {
        self.adopt_children();
        let children = ProcessIdentity::children_of(self.pid())
            .into_iter()
            .map(|child| {
                let grandchildren = if child.exists() {
                    ProcessIdentity::children_of(child.pid)
                } else {
                    Vec::new()
                };
                self.owned.extend(grandchildren.iter().copied());
                Node {
                    id: child,
                    command: child.command(),
                    children: grandchildren
                        .into_iter()
                        .map(|id| (id, id.command()))
                        .collect(),
                }
            })
            .collect();
        ProcessTree { children }
    }
}

impl Drop for Daemon {
    /// Stops the daemon orderly (`SIGTERM`, ten seconds), then terminates every process it owned.
    fn drop(&mut self) {
        self.adopt_children();
        if let Some(pid) = self.child.id() {
            // SAFETY: the unreaped `Child` handle proves `pid` is this test's own daemon.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !matches!(self.child.try_wait(), Ok(Some(_)))
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.start_kill();
        }
        ProcessIdentity::terminate(&self.owned);
    }
}

/// The daemon's own `health` answer on `fixture`'s endpoint (`"ok"` when healthy), read over its
/// framed IPC socket independently of any MCP front; a daemon that does not answer within 10 s
/// fails the caller (whose fixture and daemon still clean up).
pub async fn health(fixture: &Fixture) -> String {
    tokio::time::timeout(Duration::from_secs(10), exchange_health(fixture))
        .await
        .expect("the daemon answers its health check within 10 s")
}

/// One framed `health` exchange on `fixture`'s endpoint.
async fn exchange_health(fixture: &Fixture) -> String {
    let mut stream = UnixStream::connect(fixture.runtime.join("agent-ide.sock"))
        .await
        .unwrap();
    let body =
        serde_json::to_vec(&json!({"version":1,"request_id":"parity-health","method":"health"}))
            .unwrap();
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut body = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).await.unwrap();
    let reply: Value = serde_json::from_slice(&body).unwrap();
    reply["status"].as_str().unwrap_or_default().to_owned()
}

/// One daemon child with its own children.
#[derive(Clone, Debug)]
pub struct Node {
    /// The child.
    pub id: ProcessIdentity,
    /// Its command line.
    pub command: String,
    /// Its children with their command lines.
    pub children: Vec<(ProcessIdentity, String)>,
}

/// The daemon's process tree two levels deep.
#[derive(Clone, Debug, Default)]
pub struct ProcessTree {
    /// Direct daemon children.
    pub children: Vec<Node>,
}

impl ProcessTree {
    /// The `agent-ide module <language> <role>` child, if one runs.
    pub fn module(&self, language: &str, role: &str) -> Option<&Node> {
        self.children
            .iter()
            .find(|node| node.command.ends_with(&format!("module {language} {role}")))
    }

    /// Whether a direct daemon child's command line contains `needle` (a language server).
    pub fn direct_child_runs(&self, needle: &str) -> bool {
        self.children
            .iter()
            .any(|node| node.command.contains(needle))
    }

    /// Every process the tree names.
    pub fn all(&self) -> Vec<ProcessIdentity> {
        self.children
            .iter()
            .flat_map(|node| {
                std::iter::once(node.id).chain(node.children.iter().map(|(id, _)| *id))
            })
            .collect()
    }
}

/// One MCP front on the fixture's channel, with Codex hook correlation.
pub struct Session {
    /// The MCP process.
    child: Child,
    /// Its stdin.
    input: ChildStdin,
    /// Its stdout.
    output: BufReader<ChildStdout>,
    /// The binary serving the front and its hooks.
    program: PathBuf,
}

impl Session {
    /// Starts and initializes an MCP front and activates the fixture.
    pub async fn start(fixture: &Fixture) -> Self {
        Self::start_with(fixture, binary()).await
    }

    /// [`Session::start`] with `program` serving the front and its hooks, within 30 s.
    pub async fn start_with(fixture: &Fixture, program: PathBuf) -> Self {
        Self::start_within(fixture, program, Duration::from_secs(30)).await
    }

    /// [`Session::start_with`] within `deadline`: the start is repeated (the same activation id,
    /// so it is idempotent) until it settles to an activation, a pending start settled through
    /// `ide.inspect`; one overall deadline covers every repeat and every nested poll.
    pub async fn start_within(fixture: &Fixture, program: PathBuf, deadline: Duration) -> Self {
        let mut child = Command::new(&program)
            .env("TOKIO_WORKER_THREADS", "1")
            .env("AGENT_IDE_HOST_ATTACHMENT", ATTACHMENT)
            .args(["mcp", "--runtime-dir"])
            .arg(&fixture.runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut session = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
            program,
        };
        let initialized = session
            .exchange(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"parity","version":"1"}}}))
            .await;
        assert!(initialized.get("result").is_some(), "{initialized}");
        session
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        let arguments = json!({"activation_id":"parity-start"});
        let mut last = Value::Null;
        let settled = tokio::time::timeout(deadline, async {
            loop {
                last = session.call(fixture, "ide.start", arguments.clone()).await;
                if last["kind"] == "activation" {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        })
        .await;
        assert!(
            settled.is_ok(),
            "ide.start did not settle to an activation within {deadline:?}: {last}"
        );
        session
    }

    /// Writes one protocol message.
    async fn send(&mut self, message: Value) {
        self.input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    /// Exchanges one request within 30 s, skipping notifications.
    async fn exchange(&mut self, request: Value) -> Value {
        let id = request["id"].clone();
        self.send(request).await;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.output.read_line(&mut line).await.unwrap(),
                    0,
                    "MCP exited"
                );
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
            }
        })
        .await
        .expect("an MCP response within 30 s")
    }

    /// Submits one Codex hook for `call`.
    async fn hook(&self, fixture: &Fixture, phase: &str, call: &str) {
        let mut child = Command::new(&self.program)
            .env("TOKIO_WORKER_THREADS", "1")
            .env("AGENT_IDE_HOST_ATTACHMENT", ATTACHMENT)
            .args(["codex-hook", "--runtime-dir"])
            .arg(&fixture.runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let payload = json!({"hook_event_name":phase,"session_id":"parity","tool_use_id":call});
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "hook {phase} failed");
    }

    /// One settled tool call: its structured reply after polling `ide.inspect` while pending.
    pub async fn call(&mut self, fixture: &Fixture, tool: &str, arguments: Value) -> Value {
        let mut reply = self.call_once(fixture, tool, arguments).await;
        let mut delay = Duration::from_millis(750);
        for _ in 0..61 {
            if reply["state"] != "pending" {
                return reply;
            }
            let reference = reply["detail_ref"].as_str().unwrap().to_owned();
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            reply = self
                .call_once(fixture, "ide.inspect", json!({"detail_ref":reference}))
                .await;
        }
        panic!("{tool} did not settle: {reply}");
    }

    /// One correlated tool call without settling.
    async fn call_once(&mut self, fixture: &Fixture, tool: &str, arguments: Value) -> Value {
        let id = CALL.fetch_add(1, Ordering::Relaxed) + 1;
        let call = format!("call-{id}");
        self.hook(fixture, "PreToolUse", &call).await;
        let reply = self
            .exchange(
                json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                "name":tool,"arguments":arguments,"_meta":{"threadId":"parity","callId":call,
                "x-codex-turn-metadata":{},"codex/sandbox-state-meta":fixture.state()}}}),
            )
            .await;
        self.hook(fixture, "PostToolUse", &call).await;
        let mut structured = reply["result"]["structuredContent"].clone();
        // An error reply carries its message as content text only; keep it comparable.
        if let Some(object) = structured.as_object_mut()
            && !object.contains_key("text")
            && let Some(text) = reply["result"]["content"][0]["text"].as_str()
        {
            object.insert("text".to_owned(), Value::String(text.to_owned()));
        }
        structured
    }

    /// Stops the activation and closes the front.
    pub async fn close(mut self, fixture: &Fixture) {
        let _ = self.call(fixture, "ide.stop", json!({})).await;
        drop(self.input);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

/// One settled reply rendered as `tool args -> state kind code` plus its text. The per-daemon
/// references the reply itself generated (every `source_ref`/`detail_ref` value of its structured
/// form) are written as `<ref>` in the reply's metadata rows ([`metadata_rows`]), the ones a
/// request echoes are masked in its arguments, and a leading `<agent-ide>` status plate (the
/// feed of what changed since the last call) is dropped; no other byte changes, and source rows
/// the reply returns stay exact.
pub fn line(tool: &str, arguments: &Value, reply: &Value) -> String {
    masked_line(tool, arguments, reply, None)
}

/// [`line`] with the fixture's own root (a private temporary path) also written as `<root>` in
/// the reply's metadata rows, keeping every relative suffix.
pub fn line_for(fixture: &Fixture, tool: &str, arguments: &Value, reply: &Value) -> String {
    let root = fixture.root.display().to_string();
    masked_line(tool, arguments, reply, Some(&root))
}

/// [`line`], masking `root` too when given.
fn masked_line(tool: &str, arguments: &Value, reply: &Value, root: Option<&str>) -> String {
    /// Collects every string under a `source_ref`/`detail_ref` key of `value`.
    fn references(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    match value {
                        Value::String(reference)
                            if matches!(key.as_str(), "source_ref" | "detail_ref")
                                && !reference.is_empty() =>
                        {
                            found.push(reference.clone());
                        }
                        _ => references(value, found),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| references(item, found)),
            _ => {}
        }
    }
    let mut arguments = arguments.clone();
    if let Some(object) = arguments.as_object_mut() {
        for key in ["source_ref", "detail_ref"] {
            if let Some(value) = object.get_mut(key) {
                *value = Value::String("<ref>".to_owned());
            }
        }
    }
    let mut masks: Vec<(String, &str)> = Vec::new();
    let mut generated = Vec::new();
    references(reply, &mut generated);
    masks.extend(generated.into_iter().map(|reference| (reference, "<ref>")));
    masks.extend(root.map(|root| (root.to_owned(), "<root>")));
    // Longest first, so a value that contains another is masked whole.
    masks.sort_by_key(|(value, _)| std::cmp::Reverse(value.len()));
    let text = reply["text"].as_str().unwrap_or_default();
    // A leading `<agent-ide>` status plate is the daemon's feed of what changed since the last
    // call (a check still running or landed), not this reply's answer: it is dropped whole.
    let text = text
        .strip_prefix("<agent-ide>\n")
        .and_then(|plate| plate.split_once("</agent-ide>\n"))
        .map_or(text, |(_, rest)| rest);
    let context = reply["kind"] == "context";
    let text: String = metadata_rows(text, context)
        .map(|(row, metadata)| {
            if metadata {
                masks.iter().fold(row.to_owned(), |row, (value, mask)| {
                    row.replace(value.as_str(), mask)
                })
            } else {
                row.to_owned()
            }
        })
        .collect();
    format!(
        "{tool} {arguments} -> {} {} {}\n{text}",
        reply["state"], reply["kind"], reply["code"],
    )
}

/// The rows a reply generates around what it returns, by their leading label: the `source_ref:`
/// row of a read, the `edit:`/`Next:` rows of an edit, a test run's `tests #`/`rerun:`/`full
/// output:` rows, a pending or truncated reply's `ide.inspect` rows, the start card's `project:`
/// row and a context header's `Detail:`/`definitions:`/`references:`/`lexical_matches:` rows.
const METADATA_LABELS: &[&str] = &[
    "source_ref:",
    "edit:",
    "Next:",
    "pending:",
    "tests #",
    "rerun:",
    "full output:",
    "Output is",
    "Diagnostics are",
    "project:",
    "Detail:",
    "definitions:",
    "references:",
    "lexical_matches:",
];

/// Each row of a reply `text` (with its line end) and whether it is generated metadata: a row
/// that starts (after its indent) with one of [`METADATA_LABELS`], outside an `ide.context`
/// reply's body (every row after its header's first empty row). Every other row (numbered
/// source, usage excerpts, signatures, docs, diagnostics, a context body) is returned content.
pub fn metadata_rows(text: &str, context: bool) -> impl Iterator<Item = (&str, bool)> {
    let mut body = false;
    text.split_inclusive('\n').map(move |row| {
        let labelled = METADATA_LABELS
            .iter()
            .any(|label| row.trim_start_matches(' ').starts_with(label));
        let metadata = !body && labelled;
        if context && row.trim_end_matches(['\r', '\n']).is_empty() {
            body = true;
        }
        (row, metadata)
    })
}

/// The replies of one daemon run and the process tree seen before `ide.stop`.
pub struct Transcript {
    /// One [`line`] per call, in order.
    pub replies: Vec<String>,
    /// The daemon's tree while the session was live.
    pub tree: ProcessTree,
    /// The daemon, kept alive so the caller can inspect it further; dropping it cleans up.
    pub daemon: Daemon,
}

/// Runs `calls` on a fresh daemon with `env`, then waits `settle` and records the tree.
pub async fn transcript(
    fixture: &Fixture,
    env: &[(&str, &str)],
    calls: &[(&str, Value)],
) -> Transcript {
    let mut daemon = Daemon::start(fixture, env).await;
    let mut session = Session::start(fixture).await;
    let mut replies = Vec::new();
    for (tool, arguments) in calls {
        let reply = session.call(fixture, tool, arguments.clone()).await;
        replies.push(line_for(fixture, tool, arguments, &reply));
    }
    let tree = daemon.tree();
    session.close(fixture).await;
    Transcript {
        replies,
        tree,
        daemon,
    }
}

/// Masks the per-run values of the two generated reply kinds that carry them, by slot, and keeps
/// every other byte (whitespace included) of every other reply, so source text an `ide.read`,
/// `ide.context` or `ide.symbol` returns is compared exactly:
/// - an `ide.start` reply is generated metadata only: a hex run over 64 characters (with `-`/`,`
///   separators), a numeric timing (`12ms`, `(3.5ms)`) and the 64-hex id after a word naming the
///   activation are masked; short hex runs and paths stay;
/// - an `ide.test` reply's settled `tests #N: …, S s` line has its whole-second duration `S` masked.
///
/// References a reply generated are already masked by value in [`line`].
pub fn normalized(text: &str) -> String {
    if text.starts_with("ide.start ") {
        start_card(text)
    } else if text.starts_with("ide.test ") {
        // Only the settled status row (the reply's first row after the call header); started
        // arguments and the runner's output tail stay exact.
        let mut rows = text.split_inclusive('\n');
        let header = rows.next().unwrap_or_default();
        let status = rows.next().map(test_duration).unwrap_or_default();
        std::iter::once(header.to_owned())
            .chain(std::iter::once(status))
            .chain(rows.map(str::to_owned))
            .collect()
    } else {
        text.to_owned()
    }
}

/// [`normalized`] for an `ide.start` reply.
fn start_card(text: &str) -> String {
    /// Whether `word` is one volatile token.
    fn volatile(word: &str) -> bool {
        let digest = word.len() > 64
            && word
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-' || c == ',');
        let core = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | ':' | '.'));
        let timing = core.strip_suffix("ms").is_some_and(|number| {
            !number.is_empty()
                && number.chars().any(|c| c.is_ascii_digit())
                && number.chars().all(|c| c.is_ascii_digit() || c == '.')
        });
        digest || timing
    }
    /// Whether `word` is exactly a generated 64-hex identifier.
    fn generated_id(word: &str) -> bool {
        let core = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ';' | ':' | '.'));
        core.len() == 64 && core.chars().all(|c| c.is_ascii_hexdigit())
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let mut previous = String::new();
    for c in text.chars().chain(std::iter::once('\n')) {
        if c.is_whitespace() {
            if word.is_empty() {
                out.push(c);
                continue;
            }
            // The activation id a daemon generates follows its marker word.
            let activation = previous.contains("activation") && generated_id(&word);
            out.push_str(if activation {
                "<activation>"
            } else if volatile(&word) {
                "<volatile>"
            } else {
                &word
            });
            previous = std::mem::take(&mut word);
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.pop();
    out
}

/// A settled `tests #N: …` status row (`P passed, F failed, S s`, `no summary parsed, S s` or
/// `no test results (exit C), S s`) with its whole-second duration `S` masked; any other row,
/// a started one included, is kept exactly.
fn test_duration(row: &str) -> String {
    /// The length of the leading ASCII digits of `text`.
    fn digits(text: &str) -> usize {
        text.len() - text.trim_start_matches(|c: char| c.is_ascii_digit()).len()
    }
    /// `text` after one leading run of digits, if there is one.
    fn after_number(text: &str) -> Option<&str> {
        let count = digits(text);
        (count > 0).then(|| &text[count..])
    }
    let settled = || -> Option<usize> {
        let rest = after_number(row.strip_prefix("tests #")?)?.strip_prefix(": ")?;
        let rest = if let Some(rest) = rest.strip_prefix("no summary parsed") {
            rest
        } else if let Some(rest) = rest.strip_prefix("no test results (exit ") {
            let rest = rest.strip_prefix('-').unwrap_or(rest);
            after_number(rest)?.strip_prefix(')')?
        } else {
            let rest = after_number(rest)?.strip_prefix(" passed, ")?;
            after_number(rest)?.strip_prefix(" failed")?
        };
        let seconds = rest.strip_prefix(", ")?;
        let after = after_number(seconds)?;
        let end = after.strip_prefix(" s")?;
        matches!(end.chars().next(), None | Some(' ' | '\n' | '\r' | ';'))
            .then_some(row.len() - seconds.len())
    };
    match settled() {
        Some(at) => {
            let count = digits(&row[at..]);
            format!("{}<secs>{}", &row[..at], &row[at + count..])
        }
        None => row.to_owned(),
    }
}

/// Asserts both transcripts have the same calls and equal normalized replies.
pub fn assert_parity(in_process: &[String], module: &[String]) {
    assert_eq!(in_process.len(), module.len(), "same number of replies");
    for (local, remote) in in_process.iter().zip(module) {
        assert_eq!(
            normalized(local),
            normalized(remote),
            "\n--- in process ---\n{local}\n--- module ---\n{remote}"
        );
    }
}
