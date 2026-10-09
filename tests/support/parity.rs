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
    fn of(pid: libc::pid_t) -> Option<(Self, libc::pid_t, u32)> {
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
    /// Next call id.
    next: usize,
}

impl Session {
    /// Starts and initializes an MCP front and activates the fixture.
    pub async fn start(fixture: &Fixture) -> Self {
        let mut child = Command::new(binary())
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
            next: 100,
        };
        let initialized = session
            .exchange(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"parity","version":"1"}}}))
            .await;
        assert!(initialized.get("result").is_some(), "{initialized}");
        session
            .send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        let start = session
            .call(
                fixture,
                "ide.start",
                json!({"activation_id":"parity-start"}),
            )
            .await;
        assert_eq!(start["kind"], "activation", "{start}");
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
        let mut child = Command::new(binary())
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
        self.next += 1;
        let call = format!("call-{}", self.next);
        self.hook(fixture, "PreToolUse", &call).await;
        let reply = self
            .exchange(
                json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{
                "name":tool,"arguments":arguments,"_meta":{"threadId":"parity","callId":call,
                "x-codex-turn-metadata":{},"codex/sandbox-state-meta":fixture.state()}}}),
            )
            .await;
        self.hook(fixture, "PostToolUse", &call).await;
        reply["result"]["structuredContent"].clone()
    }

    /// Stops the activation and closes the front.
    pub async fn close(mut self, fixture: &Fixture) {
        let _ = self.call(fixture, "ide.stop", json!({})).await;
        drop(self.input);
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

/// One settled reply rendered as `tool args -> state kind code` plus its text.
pub fn line(tool: &str, arguments: &Value, reply: &Value) -> String {
    format!(
        "{tool} {arguments} -> {} {} {}\n{}",
        reply["state"],
        reply["kind"],
        reply["code"],
        reply["text"].as_str().unwrap_or_default()
    )
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
        replies.push(line(tool, arguments, &reply));
    }
    let tree = daemon.tree();
    session.close(fixture).await;
    Transcript {
        replies,
        tree,
        daemon,
    }
}

/// Masks only per-daemon tokens and keeps every other byte, whitespace included: a source-ref
/// digest (a hex run over 64 characters, with `-`/`,` separators) and a numeric timing (`12ms`,
/// `3.5ms`, optionally wrapped in punctuation such as `(12ms)` or `12ms,`).
pub fn normalized(text: &str) -> String {
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
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars().chain(std::iter::once('\n')) {
        if c.is_whitespace() {
            out.push_str(if volatile(&word) { "<volatile>" } else { &word });
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.pop();
    out
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
