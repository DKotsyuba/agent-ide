//! Executable MCP roundtrips for static discovery, separated ingress, and finite daemon routing.

use std::{
    os::unix::{ffi::OsStrExt, fs::DirBuilderExt, fs::PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use agent_ide::assistance::codex_rendezvous::{
    CodexRouteIdentity, ManagedCodexPublisher, discover,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// Distinguishes temporary endpoints across concurrently running scenarios in this process.
static NEXT_RUNTIME: AtomicUsize = AtomicUsize::new(0);
/// Serializes tests that assert global managed-Codex runtime-directory counts.
static MANAGED_CODEX_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Allows 200 ms for subprocess startup/scheduling beyond the 250 ms hook contract.
///
/// Child Tokio clocks cannot be paused by this test runtime. This wall-clock ceiling stays
/// below both a doubled (500 ms) and sixfold (1500 ms) timeout without changing product code.
const HOOK_EXIT_CEILING: Duration = Duration::from_millis(450);
/// Maximum retained-result polls in one product fixture operation.
///
/// At the capped delay this spans approximately the configured 120-second operation lifetime while
/// consuming at most 61 of Assistance's 1,024 replay entries for the actor scope.
const PRODUCT_SETTLE_MAX_POLLS: usize = 61;
/// First retained-result poll delay, leaving the daemon's short hook budget free during startup.
const PRODUCT_SETTLE_INITIAL_DELAY: Duration = Duration::from_millis(750);
/// Largest retained-result poll delay; 61 stepped waits cover the product operation lifetime.
const PRODUCT_SETTLE_MAX_DELAY: Duration = Duration::from_secs(2);

/// Returns a unique missing runtime path; the tested command decides whether to create it.
fn runtime() -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-ide-product-{}-{}",
        std::process::id(),
        NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
    ))
}

/// Owns one real shipping MCP process and its newline-delimited protocol streams.
struct Mcp {
    /// Killed on drop if a scenario panics before its explicit shutdown.
    child: Child,
    /// Host-to-server stream; closing it ends the MCP session.
    input: ChildStdin,
    /// Server output, which must contain only valid MCP JSON messages.
    output: BufReader<ChildStdout>,
}

impl Mcp {
    /// Starts and initializes the shipping binary, optionally supplying a separate launcher attachment.
    async fn start(runtime: &Path, attachment: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
        command.env("TOKIO_WORKER_THREADS", "1");
        command
            .args(["mcp", "--runtime-dir"])
            .arg(runtime)
            .env_remove("AGENT_IDE_HOST_ATTACHMENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(attachment) = attachment {
            command.env("AGENT_IDE_HOST_ATTACHMENT", attachment);
        }
        let mut child = command.spawn().unwrap();
        let mut mcp = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let response = mcp.exchange(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"product-contract","version":"1"}
        }})).await;
        assert!(response.get("result").is_some(), "{response}");
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        mcp
    }

    /// Starts the shipping self-contained managed MCP in `candidate` from one launcher template.
    async fn start_managed(template: &Path, candidate: &Path) -> Self {
        Self::start_managed_with_home(template, candidate, None).await
    }

    /// Same, with the managed daemon's home (`AGENT_IDE_HOME`) redirected into the fixture so its
    /// project check caches never touch the real home directory.
    async fn start_managed_with_home(
        template: &Path,
        candidate: &Path,
        home: Option<&Path>,
    ) -> Self {
        Self::start_managed_custom(template, candidate, home, None).await
    }

    /// Same, with the managed Codex rendezvous root (`AGENT_IDE_CODEX_RENDEZVOUS_ROOT`, the product
    /// test seam for T29B) redirected into the fixture so route publication is fully observable.
    async fn start_managed_with_rendezvous(
        template: &Path,
        candidate: &Path,
        rendezvous_root: &Path,
    ) -> Self {
        Self::start_managed_custom(template, candidate, None, Some(rendezvous_root)).await
    }

    /// Starts one managed Codex MCP with optional home and rendezvous-root redirection.
    async fn start_managed_custom(
        template: &Path,
        candidate: &Path,
        home: Option<&Path>,
        rendezvous_root: Option<&Path>,
    ) -> Self {
        Self::start_managed_custom_with_env(template, candidate, home, rendezvous_root, None).await
    }

    /// Same, with one extra environment variable set for the MCP process (a test-only seam such
    /// as the rendezvous stall).
    async fn start_managed_custom_with_env(
        template: &Path,
        candidate: &Path,
        home: Option<&Path>,
        rendezvous_root: Option<&Path>,
        seam: Option<(&str, &str)>,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
        command
            .env("TOKIO_WORKER_THREADS", "1")
            .env_remove("CLAUDE_PROJECT_DIR")
            .args(["mcp", "--auto-launcher-template"])
            .arg(template)
            .current_dir(candidate)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(home) = home {
            command.env(agent_ide::userhome::HOME_OVERRIDE_ENV, home);
        }
        if let Some(rendezvous_root) = rendezvous_root {
            command.env("AGENT_IDE_CODEX_RENDEZVOUS_ROOT", rendezvous_root);
        }
        if let Some((key, value)) = seam {
            command.env(key, value);
        }
        let mut child = command.spawn().unwrap();
        let mut mcp = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let response = mcp.exchange(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"managed-product-contract","version":"1"}
        }})).await;
        assert!(response.get("result").is_some(), "{response}");
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        mcp
    }

    /// Starts the shipping self-contained Claude MCP using only its captured project environment.
    async fn start_managed_claude(template: &Path, project: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
        command
            .env("TOKIO_WORKER_THREADS", "1")
            .env("CLAUDE_PROJECT_DIR", project)
            .env_remove("AGENT_IDE_HOST_ATTACHMENT")
            .env_remove("AGENT_IDE_MANAGED_CODEX_ATTACHMENT")
            .args(["mcp", "--claude-launcher-template"])
            .arg(template)
            .current_dir(project.parent().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let mut mcp = Self {
            input: child.stdin.take().unwrap(),
            output: BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        let response = mcp.exchange(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"managed-claude-contract","version":"1"}
        }})).await;
        assert!(response.get("result").is_some(), "{response}");
        mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .await;
        mcp
    }

    /// Writes and flushes one JSON protocol message without awaiting a response.
    async fn send(&mut self, request: Value) {
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }

    /// Exchanges one request, ignoring notifications and enforcing a five-second test deadline.
    async fn exchange(&mut self, request: Value) -> Value {
        let id = request["id"].clone();
        self.send(request).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    self.output.read_line(&mut line).await.unwrap(),
                    0,
                    "MCP exited before response"
                );
                let response: Value = serde_json::from_str(&line).unwrap();
                if response["id"] == id {
                    return response;
                }
            }
        })
        .await
        .expect("MCP response exceeded bounded deadline")
    }

    /// Closes stdin and verifies the MCP process exits cleanly without requiring a daemon shutdown.
    async fn close(self) {
        let Self {
            mut child, input, ..
        } = self;
        drop(input);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
    }
}

/// Verifies one successful MCP tool carrier has a single non-JSON block matching its typed state.
///
/// `reply` must be a JSON-RPC tools/call response with `structuredContent`. The assertion performs
/// no I/O and deliberately compares only the closed state projection: source and diagnostic text
/// may legitimately occur in the compact block, while the complete typed value remains separate.
/// A reply-delivered host's terminal reply may lead with the due status plate (T28B); the plate is
/// stripped from the text before the state comparison and must lead the structured copy's text too.
fn assert_compact_envelope(reply: &Value) {
    let result = reply["result"].as_object().expect("tool result object");
    let content = result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1, "{reply}");
    let text = content[0]["text"].as_str().expect("sole text block");
    assert!(serde_json::from_str::<Value>(text).is_err(), "{reply}");
    let structured = result["structuredContent"]
        .as_object()
        .expect("typed structured result");
    let state = structured["state"].as_str().expect("closed reply state");
    let text = match text.strip_prefix("<agent-ide>\n") {
        Some(rest) => {
            let end = rest.find("\n</agent-ide>\n").expect("closed status plate");
            &rest[end + "\n</agent-ide>\n".len()..]
        }
        None => text,
    };
    assert!(text.starts_with(state), "{reply}");
    assert_eq!(
        result.get("isError") == Some(&json!(true)),
        state == "error"
    );
}

/// Returns the carried status plate of a structured reply, absent when none was attached (T28B).
fn carried_status(structured: &Value) -> Option<&str> {
    structured["status"].as_str()
}

/// Asserts the Claude host projection carries no `structuredContent` duplicate (T14B): Claude hands
/// that field straight to its model in place of `content`, defeating the compact renderer, so the
/// managed Claude MCP omits it entirely. Returns the sole compact text block for `claude_fields`.
fn assert_claude_envelope(reply: &Value) -> &str {
    let result = reply["result"].as_object().expect("tool result object");
    let content = result["content"].as_array().expect("content array");
    assert_eq!(content.len(), 1, "{reply}");
    let text = content[0]["text"].as_str().expect("sole text block");
    assert!(serde_json::from_str::<Value>(text).is_err(), "{reply}");
    assert!(
        result.get("structuredContent").is_none(),
        "Claude must never receive the structured JSON duplicate: {reply}"
    );
    assert_eq!(
        result.get("isError") == Some(&json!(true)),
        text.starts_with("error"),
        "{reply}"
    );
    text
}

/// Reconstructs the compact-text facts a Claude-path test needs, mirroring
/// [`agent_ide::assistance::content`]'s deterministic `render_text` formats (T14B): the server no
/// longer sends `structuredContent` to Claude, so tests read the same facts the model itself
/// receives instead of the typed JSON copy. Unset fields are simply absent from the returned
/// object, matching `serde_json::Value`'s null-on-missing-key indexing.
fn claude_fields(text: &str) -> Value {
    /// Returns the bounded token starting at `text`, ending at the first space, `;`, `\n`, `.` or
    /// the string end; every identifier this parser extracts (`reason`, `code`, `outcome`,
    /// `detail_ref`, `source_ref`) is itself a single space-free token, so this stops exactly where
    /// the surrounding sentence resumes.
    fn token(text: &str) -> &str {
        let end = text.find([' ', ';', '\n', '.']).unwrap_or(text.len());
        text[..end].trim()
    }
    /// Returns the bounded token immediately following the first occurrence of `marker`, if any.
    fn after<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
        text.find(marker)
            .map(|index| token(&text[index + marker.len()..]))
    }

    if let Some(rest) = text.strip_prefix("unavailable: ") {
        return json!({"state":"unavailable","reason":token(rest)});
    }
    if let Some(rest) = text.strip_prefix("error: ") {
        return json!({"state":"error","code":token(rest)});
    }
    if let Some(rest) = text.strip_prefix("pending: ") {
        let helper = rest
            .strip_prefix(
                "run exactly this command with Bash in the foreground, with no editing, \
                 wrapping, or appended arguments:\n",
            )
            .and_then(|rest| rest.split('\n').next());
        return json!({
            "state":"pending",
            "detail_ref":after(rest, "detail_ref "),
            "helper":helper,
        });
    }
    if let Some(rest) = text.strip_prefix("edit: ") {
        let outcome = token(rest);
        let source_ref = after(rest, "source_ref ");
        return json!({
            "state":"edit",
            "result":{"outcome":outcome,"source_ref":source_ref},
        });
    }
    for kind in ["activation", "context", "diff", "stop"] {
        let Some(rest) = text.strip_prefix(&format!("complete {kind}: ")) else {
            continue;
        };
        let continuation = rest.contains("\nOutput is truncated; use ide.inspect");
        let truncated = continuation || rest.contains("\nOutput is incomplete;");
        let detail_ref = after(rest, "detail_ref ").or_else(|| after(rest, "source_ref "));
        let body_end = [
            "\nNext: use ide.context",
            "\nOutput is truncated;",
            "\nOutput is incomplete;",
            "\nDiagnostics are exactly as reported;",
            "\nNext: use ide.stop",
            "\nWorkspace authority is released",
        ]
        .into_iter()
        .filter_map(|marker| rest.find(marker))
        .min()
        .unwrap_or(rest.len());
        return json!({
            "state":"complete","kind":kind,"text":&rest[..body_end],
            "truncated":truncated,"continuation":continuation,"detail_ref":detail_ref,
        });
    }
    json!({"text":text})
}

/// Calls one managed Codex tool using only trusted request metadata for actor and sandbox identity.
async fn managed_call(
    mcp: &mut Mcp,
    id: usize,
    actor: &str,
    name: &str,
    arguments: Value,
    state: &Value,
) -> Value {
    let reply=mcp.exchange(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"threadId":actor,"callId":format!("managed-{actor}-{id}"),"x-codex-turn-metadata":{},"codex/sandbox-state-meta":state}}})).await;
    assert_compact_envelope(&reply);
    reply["result"]["structuredContent"].clone()
}

/// Polls one managed pending operation through same-actor `ide.inspect` calls until it settles.
async fn settle_managed(
    mcp: &mut Mcp,
    next: &mut usize,
    actor: &str,
    state: &Value,
    mut reply: Value,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while reply["state"] == "pending" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "managed operation did not settle"
        );
        let reference = reply["detail_ref"].as_str().unwrap().to_owned();
        tokio::time::sleep(Duration::from_millis(20)).await;
        *next += 1;
        reply = managed_call(
            mcp,
            *next,
            actor,
            "ide.inspect",
            json!({"detail_ref":reference}),
            state,
        )
        .await;
    }
    reply
}

/// Supplies valid host-shaped request metadata, outside the model-owned arguments object.
fn metadata(call: usize) -> Value {
    json!({"threadId":"actor","callId":format!("call-{call}"),"x-codex-turn-metadata":{"turn":"test"}})
}

/// Rejects malformed launcher attachments before the binary opens stdin or creates runtime state.
#[tokio::test]
async fn binary_rejects_invalid_launcher_attachments_before_serving() {
    let runtime = runtime();
    for attachment in [String::new(), "x".repeat(129)] {
        let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["mcp", "--runtime-dir"])
            .arg(&runtime)
            .env("AGENT_IDE_HOST_ATTACHMENT", attachment)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    assert!(!runtime.exists());
}

/// Verifies static binary discovery and fail-open calls cannot create runtime state or accept identity arguments.
#[tokio::test]
async fn binary_discovery_is_static_and_inactive_calls_are_fail_open() {
    let runtime = runtime();
    let mut mcp = Mcp::start(&runtime, None).await;
    let discovery = mcp
        .exchange(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    let tools = discovery["result"]["tools"].as_array().unwrap();
    let mut names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "ide.context",
            "ide.diff",
            "ide.edit",
            "ide.inspect",
            "ide.start",
            "ide.stop"
        ]
    );
    assert!(
        tools
            .iter()
            .all(|tool| tool["inputSchema"]["additionalProperties"] == false)
    );
    let unavailable = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"ide.start","arguments":{"activation_id":"start"},"_meta":metadata(3)
            }}),
        )
        .await;
    assert_eq!(unavailable["result"]["isError"], true);
    assert!(
        unavailable["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("continue with native tools")
    );
    let invalid = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"ide.start","arguments":{"activation_id":"start","actor_id":"forged"}
            }}),
        )
        .await;
    assert_eq!(invalid["result"]["isError"], true);
    assert!(
        invalid["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid bounded parameters")
    );
    // The plain `ide.context` schema cannot express "path or problems", so the handler explains it.
    let no_target = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"ide.context","arguments":{}
            }}),
        )
        .await;
    assert_eq!(no_target["result"]["isError"], true);
    assert_eq!(
        no_target["result"]["content"][0]["text"],
        "invalid bounded parameters: ide.context needs either \"path\" or \"kind\":\"problems\""
    );
    mcp.close().await;
    assert!(!runtime.exists());
}

/// Managed startup failure remains a disconnected static six-tool MCP with bounded fallback calls.
#[tokio::test]
async fn managed_startup_failure_serves_exact_static_tools_without_ipc() {
    let candidate = std::env::current_dir().unwrap();
    let mut mcp = Mcp::start_managed(
        Path::new("/private/tmp/agent-ide-missing-launcher-template"),
        &candidate,
    )
    .await;
    let discovery = mcp
        .exchange(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    let mut names = discovery["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "ide.context",
            "ide.diff",
            "ide.edit",
            "ide.inspect",
            "ide.start",
            "ide.stop"
        ]
    );
    let unavailable = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"ide.start","arguments":{"activation_id":"fallback"},"_meta":metadata(3)
            }}),
        )
        .await;
    assert_eq!(unavailable["result"]["isError"], true, "{unavailable}");
    assert!(
        unavailable["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("continue with native tools")
    );
    mcp.close().await;
}

/// Proves all six shipping handlers reach the real daemon only after separated launcher and request ingress.
#[tokio::test]
async fn binary_routes_six_methods_to_typed_missing_peer_and_survives_daemon_loss() {
    let runtime = runtime();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(runtime.join("agent-ide.sock"))
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                daemon.try_wait().unwrap().is_none(),
                "daemon exited during startup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut mcp = Mcp::start(&runtime, Some("private-host-channel")).await;
    let no_metadata = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"ide.stop","arguments":{}
            }}),
        )
        .await;
    assert!(
        no_metadata["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("host attachment or daemon")
    );
    for (index, (name, arguments)) in [
        ("ide.start", json!({"activation_id":"activate"})),
        ("ide.context", json!({"path":"src/main.rs"})),
        ("ide.diff", json!({})),
        ("ide.inspect", json!({"detail_ref":"detail"})),
        ("ide.stop", json!({})),
        (
            "ide.edit",
            json!({"operation_id":"op","path":"src/main.rs","source_ref":"detail","content":"new"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let response = mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":index+3,"method":"tools/call","params":{
                    "name":name,"arguments":arguments,"_meta":metadata(index)
                }}),
            )
            .await;
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        assert_eq!(response["result"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "unavailable: host_binding; continue with native tools"
        );
        assert_eq!(
            response["result"]["structuredContent"],
            json!({"state":"unavailable","reason":"host_binding"})
        );
        assert!(!response.to_string().contains("private-host-channel"));
    }
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    let lost = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{
                "name":"ide.diff","arguments":{},"_meta":metadata(20)
            }}),
        )
        .await;
    assert_eq!(lost["result"]["isError"], true);
    assert!(
        lost["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("host attachment or daemon")
    );
    mcp.close().await;
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Starts a disposable daemon, waiting for its real Unix endpoint under a bounded test deadline.
async fn daemon(runtime: &Path) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(runtime.join("agent-ide.sock"))
                .await
                .is_ok()
            {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    child
}

/// Starts the real hook process; callers own stdin closure and bounded completion checks.
fn hook_process(runtime: &Path, attachment: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
    // Bound fixture thread creation so the wall-clock deadline measures ingress, not CPU-sized pools.
    command.env("TOKIO_WORKER_THREADS", "1");
    command
        .args(["codex-hook", "--runtime-dir"])
        .arg(runtime)
        .env_remove("AGENT_IDE_HOST_ATTACHMENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(attachment) = attachment {
        command.env("AGENT_IDE_HOST_ATTACHMENT", attachment);
    }
    command.spawn().unwrap()
}

/// Starts the real Claude hook mode with the same bounded environment as the Codex helper.
fn claude_hook_process(runtime: &Path, attachment: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
    command.env("TOKIO_WORKER_THREADS", "1");
    command
        .args(["claude-hook", "--runtime-dir"])
        .arg(runtime)
        .env_remove("AGENT_IDE_HOST_ATTACHMENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(attachment) = attachment {
        command.env("AGENT_IDE_HOST_ATTACHMENT", attachment);
    }
    command.spawn().unwrap()
}

/// Starts the argument-free managed Claude hook with no caller-selected runtime or attachment.
fn managed_claude_hook_process(project: Option<&Path>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
    command
        .env("TOKIO_WORKER_THREADS", "1")
        .env_remove("CLAUDE_PROJECT_DIR")
        .env_remove("AGENT_IDE_HOST_ATTACHMENT")
        .args(["claude-hook"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(project) = project {
        command.env("CLAUDE_PROJECT_DIR", project);
        command.current_dir(project);
    }
    command.spawn().unwrap()
}

/// Sends one native Claude payload through the managed hook and returns its bounded process output.
async fn managed_claude_hook(project: Option<&Path>, payload: Value) -> std::process::Output {
    let mut child = managed_claude_hook_process(project);
    let mut payload = payload;
    if let Some(project) = project {
        payload["cwd"] = json!(project);
    }
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(payload.to_string().as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

/// Submits native-shaped hook JSON through the executable and requires silent fail-open completion.
async fn hook(runtime: &Path, phase: &str, field: &str, actor: &str, call: &str) {
    let payload = json!({"hook_event_name":phase,field:actor,"tool_use_id":call,
        "tool_input":{"secret":"must-never-leave-hook"},"tool_response":"private-output",
        "cwd":"private-cwd","transcript_path":"private-transcript"});
    let mut child = hook_process(runtime, Some("private-host-channel"));
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(payload.to_string().as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
}

/// Creates one fresh owner-only rendezvous test area directly below `/private/tmp`.
///
/// Kept short and outside `std::env::temp_dir()`: fixture Unix-socket paths must stay under
/// `SUN_LEN`, and the area must never be a real production rendezvous root.
fn rendezvous_area(tag: &str) -> PathBuf {
    let base = PathBuf::from(format!(
        "/private/tmp/.airvb-{tag}-{}-{}",
        std::process::id(),
        NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir(&base).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
    base
}

/// Creates one valid fixture runtime: `0700` directory with an owner-only bound daemon socket.
///
/// The listener stays with the caller so a scenario decides whether the socket answers, stalls, or
/// is already dead (a dropped listener leaves the socket file in place with nothing behind it).
fn fixture_runtime(base: &Path) -> (PathBuf, tokio::net::UnixListener) {
    use tokio::net::UnixListener;
    let runtime_dir = base.join("runtime");
    std::fs::create_dir(&runtime_dir).unwrap();
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let listener = UnixListener::bind(runtime_dir.join("agent-ide.sock")).unwrap();
    std::fs::set_permissions(
        runtime_dir.join("agent-ide.sock"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    (runtime_dir, listener)
}

/// Starts the real managed Codex hook: no runtime dir, no credential; discovery via the test root.
fn managed_codex_hook_process(root: &Path, decoy_attachment: Option<&str>) -> Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
    command.env("TOKIO_WORKER_THREADS", "1");
    command
        .args(["codex-hook", "--managed"])
        .env("AGENT_IDE_CODEX_RENDEZVOUS_ROOT", root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(attachment) = decoy_attachment {
        command.env("AGENT_IDE_HOST_ATTACHMENT", attachment);
    } else {
        command.env_remove("AGENT_IDE_HOST_ATTACHMENT");
    }
    command.spawn().unwrap()
}

/// Sends one payload through the managed hook and returns its bounded process output.
async fn managed_codex_hook(root: &Path, payload: &Value) -> std::process::Output {
    let mut child = managed_codex_hook_process(root, None);
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(payload.to_string().as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

/// Asserts one managed hook run failed open exactly as the contract requires: silent success.
fn assert_managed_hook_silent(output: &std::process::Output) {
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "managed hook must stay silent: {output:?}"
    );
}

/// Calls one managed Codex tool whose trusted metadata carries an explicit root session id.
async fn managed_root_call(
    mcp: &mut Mcp,
    id: usize,
    actor: &str,
    root_session: &str,
    name: &str,
    arguments: Value,
    state: &Value,
) -> Value {
    let reply = mcp
        .exchange(json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,
            "arguments":arguments,
            "_meta":{"threadId":actor,"callId":format!("managed-{actor}-{id}"),
            "x-codex-turn-metadata":{"session_id":root_session},"codex/sandbox-state-meta":state}}}))
        .await;
    assert_compact_envelope(&reply);
    reply["result"]["structuredContent"].clone()
}

/// Builds identical model inputs and request/call IDs for actors whose identity differs only in host metadata.
fn host_call(actor: &str, call: &str, name: &str) -> Value {
    let arguments = if name == "ide.start" {
        json!({"activation_id":"same-activation"})
    } else if name == "ide.context" {
        json!({"path":"src/main.rs"})
    } else {
        json!({})
    };
    json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
        "name":name,"arguments":arguments,"_meta":{"threadId":actor,"callId":call,
        "x-codex-turn-metadata":{"private":"not-retained"},"codex/sandbox-state-meta":{"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":false}}}})
}

/// Checks a closed host boundary response and proves private launch/input fields were not rendered.
fn boundary(response: &Value, expected: &str) {
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains(expected), "{response}");
    for private in [
        "private-host-channel",
        "must-never-leave-hook",
        "private-output",
        "private-cwd",
        "private-transcript",
        "not-retained",
    ] {
        assert!(!response.to_string().contains(private));
    }
}

/// Proves parallel root/child calls with identical arguments and call IDs remain actor-scoped through stop.
#[tokio::test]
async fn binary_codex_hooks_bind_exact_parallel_actors_and_stop_before_workspace() {
    let runtime = runtime();
    let mut daemon = daemon(&runtime).await;
    let (mut root, mut child) = tokio::join!(
        Mcp::start(&runtime, Some("private-host-channel")),
        Mcp::start(&runtime, Some("private-host-channel"))
    );
    // One actor's pre-hook must never validate the other's identical pending call.
    hook(&runtime, "PreToolUse", "session_id", "root", "only-root").await;
    let (root_reply, child_reply) = tokio::join!(
        root.exchange(host_call("root", "only-root", "ide.start")),
        child.exchange(host_call("child", "only-root", "ide.start"))
    );
    boundary(&root_reply, "workspace_activation");
    boundary(&child_reply, "host_binding");
    hook(&runtime, "PostToolUse", "session_id", "root", "only-root").await;
    // Both actors supply their own exact lifecycle, with identical call/JSON-RPC correlations.
    tokio::join!(
        hook(&runtime, "PreToolUse", "session_id", "root", "parallel"),
        hook(&runtime, "PreToolUse", "agent_id", "child", "parallel")
    );
    let (root_reply, child_reply) = tokio::join!(
        root.exchange(host_call("root", "parallel", "ide.start")),
        child.exchange(host_call("child", "parallel", "ide.start"))
    );
    boundary(&root_reply, "workspace_activation");
    boundary(&child_reply, "workspace_activation");
    tokio::join!(
        hook(&runtime, "PostToolUse", "session_id", "root", "parallel"),
        hook(&runtime, "PostToolUse", "agent_id", "child", "parallel")
    );
    boundary(
        &root
            .exchange(host_call("root", "parallel", "ide.start"))
            .await,
        "host_binding",
    );
    // Host stop is honest about its scope, and cannot revoke the other actor.
    hook(&runtime, "PreToolUse", "session_id", "root", "stop").await;
    let stopped = root.exchange(host_call("root", "stop", "ide.stop")).await;
    assert_compact_envelope(&stopped);
    boundary(&stopped, "host_stopped");
    hook(&runtime, "PostToolUse", "session_id", "root", "stop").await;
    tokio::join!(
        hook(&runtime, "PreToolUse", "session_id", "root", "after-stop"),
        hook(&runtime, "PreToolUse", "agent_id", "child", "after-stop")
    );
    let (root_reply, child_reply) = tokio::join!(
        root.exchange(host_call("root", "after-stop", "ide.context")),
        child.exchange(host_call("child", "after-stop", "ide.context"))
    );
    boundary(&root_reply, "host_binding");
    boundary(&child_reply, "workspace_activation");
    hook(&runtime, "PreToolUse", "session_id", "root", "restart").await;
    boundary(
        &root
            .exchange(host_call("root", "restart", "ide.start"))
            .await,
        "workspace_activation",
    );
    // Duplicate and premature post observations cannot be repaired by a subsequent MCP call.
    for (call, second_phase) in [("duplicate", "PreToolUse"), ("early-post", "PostToolUse")] {
        hook(&runtime, "PreToolUse", "session_id", "root", call).await;
        hook(&runtime, second_phase, "session_id", "root", call).await;
        boundary(
            &root.exchange(host_call("root", call, "ide.start")).await,
            "host_binding",
        );
    }
    // MCP-before-pre is permanently rejected for that exact invocation.
    boundary(
        &root
            .exchange(host_call("root", "late-pre", "ide.start"))
            .await,
        "host_binding",
    );
    hook(&runtime, "PreToolUse", "session_id", "root", "late-pre").await;
    boundary(
        &root
            .exchange(host_call("root", "late-pre", "ide.start"))
            .await,
        "host_binding",
    );
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    hook(&runtime, "PreToolUse", "session_id", "root", "daemon-lost").await;
    boundary(
        &root
            .exchange(host_call("root", "daemon-lost", "ide.start"))
            .await,
        "host attachment or daemon",
    );
    tokio::join!(root.close(), child.close());
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Checks absent daemon, inactive launch, malformed/oversized JSON, and an open stdin all exit silently.
#[tokio::test]
async fn binary_codex_hook_fail_open_inactive_invalid_and_stdin_deadline() {
    let runtime = runtime();
    hook(&runtime, "PreToolUse", "session_id", "root", "absent").await;
    for payload in [
        b"{".to_vec(),
        vec![b'x'; 65537],
        br#"{"hook_event_name":"PreToolUse","session_id":"","agent_id":"b","tool_use_id":"c"}"#
            .to_vec(),
    ] {
        let mut child = hook_process(&runtime, Some("private-host-channel"));
        let _ = child.stdin.take().unwrap().write_all(&payload).await;
        let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    }
    for attachment in [None, Some("private-host-channel")] {
        let mut child = hook_process(&runtime, attachment);
        let input = child.stdin.take().unwrap();
        let before = std::time::Instant::now();
        let output = tokio::time::timeout(HOOK_EXIT_CEILING, child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            before.elapsed() < HOOK_EXIT_CEILING,
            "hook exceeded 250 ms deadline plus 200 ms scheduler allowance"
        );
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
        drop(input);
    }
    assert!(!runtime.exists());
}

/// Claude ingress is silent on malformed identity and daemon loss, without creating runtime state.
#[tokio::test]
async fn binary_claude_hook_is_silent_when_input_or_daemon_is_unavailable() {
    let runtime = runtime();
    for payload in [
        br#"{"hook_event_name":"PostToolUse","agent_id":"child","tool_use_id":"call"}"#.as_slice(),
        br#"{"hook_event_name":"PostToolUse","session_id":"session","agent_id":"child","agent_id":"other","tool_use_id":"call"}"#.as_slice(),
        br#"{"hook_event_name":"PostToolBatch","session_id":"session","agent_id":"child","permission_mode":"bypassPermissions"}"#.as_slice(),
    ] {
        let mut child = claude_hook_process(&runtime, Some("private-host-channel"));
        child.stdin.take().unwrap().write_all(payload).await.unwrap();
        let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    assert!(!runtime.exists());
}

/// Exercises the shipping Claude command and accepts only the bounded additional-context schema.
#[tokio::test]
async fn binary_claude_post_emits_closed_model_context_from_typed_feedback() {
    use tokio::net::UnixListener;
    let runtime = runtime();
    std::fs::create_dir(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
    let mut child = claude_hook_process(&runtime, Some("private-host-channel"));
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            br#"{"hook_event_name":"PostToolUse","session_id":"root","tool_use_id":"call","tool_input":{"secret":"hidden"}}"#,
        )
        .await
        .unwrap();
    let server = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let size = stream.read_u32().await.unwrap();
        let mut body = vec![0; size as usize];
        stream.read_exact(&mut body).await.unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["sanitized_observation_json"]["host"], "claude");
        assert_eq!(request["sanitized_observation_json"]["session_id"], "root");
        assert!(!String::from_utf8(body).unwrap().contains("hidden"));
        let reply = serde_json::to_vec(&json!({
            "version": 2,
            "request_id": request["request_id"],
            "correlation_id": request["correlation_id"],
            "opaque_reply_json": {"state":"feedback","text":"one bounded fact"}
        }))
        .unwrap();
        stream.write_u32(reply.len() as u32).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    };
    let (output, ()) = tokio::join!(child.wait_with_output(), server);
    let output = output.unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    let rendered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rendered.as_object().unwrap().len(), 1);
    assert_eq!(
        rendered["hookSpecificOutput"],
        json!({"hookEventName":"PostToolUse","additionalContext":"one bounded fact"})
    );
    drop(listener);
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Captures the real hook IPC frame while withholding a reply, proving sanitization and total deadline.
#[tokio::test]
async fn binary_codex_hook_hung_daemon_deadline_sends_only_selected_fields() {
    use tokio::net::UnixListener;
    let runtime = runtime();
    std::fs::create_dir(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
    // Cold executable startup can precede the hook timer by >800 ms under the test harness.
    // Bound that separately; the measured invocation still fails a doubled 500 ms hook deadline.
    let warm = tokio::time::timeout(
        Duration::from_secs(2),
        hook_process(&runtime, None).wait_with_output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(warm.status.success() && warm.stdout.is_empty() && warm.stderr.is_empty());
    let before = std::time::Instant::now();

    let hook = hook(&runtime, "PreToolUse", "agent_id", "child", "hung");
    let capture = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let size = stream.read_u32().await.unwrap();
        assert!(size < 4096);
        let mut bytes = vec![0; size as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            frame["sanitized_observation_json"],
            json!({"host":"codex","phase":"pre","actor_id":"child","call_id":"hung",
                "session_id":null,"agent_type":null,"launch_command":null,"launch_background":null,"failed":false,
                "tool_name":null})
        );
        let wire = String::from_utf8(bytes).unwrap();
        for private in [
            "must-never-leave-hook",
            "private-output",
            "private-cwd",
            "private-transcript",
        ] {
            assert!(!wire.contains(private));
        }
        // Hold the connection through client EOF; no reply can help it finish.
        assert_eq!(
            stream.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    };
    tokio::time::timeout(HOOK_EXIT_CEILING, async {
        tokio::join!(hook, capture);
    })
    .await
    .unwrap();
    assert!(
        before.elapsed() < HOOK_EXIT_CEILING,
        "hook exceeded 250 ms deadline plus 200 ms scheduler allowance"
    );
    drop(listener);
    std::fs::remove_dir_all(runtime).unwrap();
}

/// The managed hook discovers a published route, submits its published attachment (never the env
/// credential override), and renders the daemon's feedback — legacy forms are untouched elsewhere.
#[tokio::test]
async fn binary_managed_codex_hook_discovers_route_and_renders_feedback() {
    let base = rendezvous_area("hook-feedback");
    let root = base.join("rendezvous");
    let (runtime_dir, listener) = fixture_runtime(&base);
    let attachment = "a1b2c3d4".repeat(8);
    let identity = CodexRouteIdentity::new("root-session-1", "actor-1").unwrap();
    let mut publisher = ManagedCodexPublisher::new(root.clone(), runtime_dir, attachment.clone());
    publisher.publish(&identity).unwrap();
    let payload = json!({"hook_event_name":"PostToolUse","session_id":"root-session-1",
        "agent_id":"actor-1","tool_use_id":"call-1","tool_name":"Bash",
        "tool_input":{"secret":"must-never-leave-hook"},"cwd":"private-cwd"});
    // Warm the binary first: cold executable startup must not pollute the measured round trip.
    // The warm invocation carries no stdin, so it exits silently the moment the payload is absent.
    let mut warm = managed_codex_hook_process(&root, None);
    drop(warm.stdin.take());
    let warm_output = tokio::time::timeout(Duration::from_secs(2), warm.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&warm_output);
    // The decoy environment credential must be ignored entirely in managed mode.
    let started = std::time::Instant::now();
    let mut child = managed_codex_hook_process(&root, Some(&"f".repeat(64)));
    let server = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let size = stream.read_u32().await.unwrap();
        let mut body = vec![0; size as usize];
        stream.read_exact(&mut body).await.unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            request["opaque_attachment"], attachment,
            "the published attachment is submitted, never the environment override"
        );
        assert_eq!(request["sanitized_observation_json"]["host"], "codex");
        assert_eq!(request["sanitized_observation_json"]["phase"], "post");
        assert_eq!(
            request["sanitized_observation_json"]["session_id"],
            "root-session-1"
        );
        assert_eq!(request["sanitized_observation_json"]["actor_id"], "actor-1");
        assert_eq!(request["sanitized_observation_json"]["tool_name"], "Bash");
        assert!(
            !String::from_utf8_lossy(&body).contains("must-never-leave-hook"),
            "{}",
            String::from_utf8_lossy(&body)
        );
        let reply = serde_json::to_vec(&json!({
            "version": 2,
            "request_id": request["request_id"],
            "correlation_id": request["correlation_id"],
            "opaque_reply_json": {"state":"feedback","text":"one bounded fact"}
        }))
        .unwrap();
        stream.write_u32(reply.len() as u32).await.unwrap();
        stream.write_all(&reply).await.unwrap();
    };
    let output = async {
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    };
    let (output, ()) = tokio::join!(output, server);
    assert!(
        started.elapsed() < HOOK_EXIT_CEILING,
        "managed feedback round trip exceeded the hook budget, took {:?}",
        started.elapsed()
    );
    assert!(output.status.success() && output.stderr.is_empty());
    let rendered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        rendered,
        json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"one bounded fact"}})
    );
    drop(publisher);
    drop(listener);
    std::fs::remove_dir_all(base).unwrap();
}

/// Missing, ambiguous, stale, malformed, and unsupported-phase managed routes all stay silent.
///
/// The daemon fixture behind the route ANSWERS every submission with feedback, so the silence of
/// the ambiguous and stale cases is attributable to discovery's refusal, never to a dead socket:
/// a discovery that wrongly resolved either route would produce output and fail the test.
#[tokio::test]
async fn binary_managed_codex_hook_missing_ambiguous_and_stale_routes_are_silent() {
    let base = rendezvous_area("hook-silent");
    let root = base.join("rendezvous");
    let payload = json!({"hook_event_name":"PostToolUse","session_id":"root-session-1",
        "agent_id":"actor-1","tool_use_id":"call-1","tool_name":"Bash"});

    // Missing route: nothing was ever published under this root.
    assert_managed_hook_silent(&managed_codex_hook(&root, &payload).await);

    // Malformed identity: no root session at all in the payload.
    let identityless =
        json!({"hook_event_name":"PostToolUse","agent_id":"actor-1","tool_use_id":"call-1"});
    assert_managed_hook_silent(&managed_codex_hook(&root, &identityless).await);

    // Unsupported phase: managed mode accepts only PreToolUse and PostToolUse.
    let batch = json!({"hook_event_name":"PostToolBatch","session_id":"root-session-1",
        "agent_id":"actor-1"});
    assert_managed_hook_silent(&managed_codex_hook(&root, &batch).await);

    // One live answering daemon serves the fixture runtime for the remaining cases: any
    // submission that got past discovery's checks would be answered, rendered, and printed.
    let (runtime_dir, listener) = fixture_runtime(&base);
    let attachment = "a1b2c3d4".repeat(8);
    let identity = CodexRouteIdentity::new("root-session-1", "actor-1").unwrap();
    let answerer = |listener: tokio::net::UnixListener| {
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let size = stream.read_u32().await.unwrap();
                let mut body = vec![0; size as usize];
                stream.read_exact(&mut body).await.unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let reply = serde_json::to_vec(&json!({
                    "version": 2,
                    "request_id": request["request_id"],
                    "correlation_id": request["correlation_id"],
                    "opaque_reply_json": {"state":"feedback","text":"wrong discovery would speak"}
                }))
                .unwrap();
                stream.write_u32(reply.len() as u32).await.unwrap();
                stream.write_all(&reply).await.unwrap();
            }
        })
    };
    let answering = answerer(listener);

    // Ambiguous route: two live publishers for one identity, with the socket answering. Only
    // the ambiguity refusal (never an unreachable socket) keeps the hook silent here.
    let mut first =
        ManagedCodexPublisher::new(root.clone(), runtime_dir.clone(), attachment.clone());
    let mut second = ManagedCodexPublisher::new(root.clone(), runtime_dir.clone(), attachment);
    first.publish(&identity).unwrap();
    second.publish(&identity).unwrap();
    assert_managed_hook_silent(&managed_codex_hook(&root, &payload).await);

    // Stale route: the single remaining record still points at the original runtime directory,
    // which is replaced at the same path with a fresh answering runtime — a fresh inode. A
    // discovery that adopted the old record without re-verifying its device/inode would submit
    // to the answering socket and produce output; correct discovery refuses the route instead.
    drop(second);
    answering.abort();
    std::fs::remove_dir_all(&runtime_dir).unwrap();
    let (replaced_dir, replaced_listener) = fixture_runtime(&base);
    assert_eq!(replaced_dir, runtime_dir, "replacement at the same path");
    let replaced_answering = answerer(replaced_listener);
    assert_managed_hook_silent(&managed_codex_hook(&root, &payload).await);
    // The answerer is genuine: with the stale publisher gone, a record created against the
    // replacement runtime delivers through it.
    drop(first);
    let mut successor =
        ManagedCodexPublisher::new(root.clone(), replaced_dir.clone(), "a1b2c3d4".repeat(8));
    successor.publish(&identity).unwrap();
    let answered = managed_codex_hook(&root, &payload).await;
    assert!(
        answered.status.success() && answered.stderr.is_empty(),
        "{answered:?}"
    );
    assert_eq!(
        managed_hook_context(&String::from_utf8(answered.stdout).unwrap()),
        "wrong discovery would speak",
        "precondition: the answering daemon really delivers"
    );

    successor.retire();
    replaced_answering.abort();
    std::fs::remove_dir_all(base).unwrap();
}

/// An oversized managed payload is discarded whole and silently, never parsed or submitted.
#[tokio::test]
async fn binary_managed_codex_hook_oversized_stdin_is_silent() {
    let base = rendezvous_area("hook-oversize");
    let root = base.join("rendezvous");
    let mut child = managed_codex_hook_process(&root, None);
    let mut input = child.stdin.take().unwrap();
    // Exactly one byte over the 64 KiB ingress bound: the whole payload must be discarded.
    let oversized = vec![b'x'; 64 * 1024 + 1];
    input.write_all(&oversized).await.unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&output);
    std::fs::remove_dir_all(base).unwrap();
}

/// A live publication whose daemon socket accepts but never answers still exits within the deadline.
#[tokio::test]
async fn binary_managed_codex_hook_stalled_daemon_returns_within_deadline() {
    let base = rendezvous_area("hook-stalled");
    let root = base.join("rendezvous");
    let payload = json!({"hook_event_name":"PostToolUse","session_id":"root-session-1",
        "agent_id":"actor-1","tool_use_id":"call-1","tool_name":"Bash"});
    // Warm the binary first: cold executable startup must not pollute the measured invocation.
    // The warm invocation carries no stdin, so it exits silently the moment the payload is absent.
    let mut warm = managed_codex_hook_process(&root, None);
    drop(warm.stdin.take());
    let warm_output = tokio::time::timeout(Duration::from_secs(2), warm.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&warm_output);

    let (runtime_dir, listener) = fixture_runtime(&base);
    let identity = CodexRouteIdentity::new("root-session-1", "actor-1").unwrap();
    let mut publisher = ManagedCodexPublisher::new(root.clone(), runtime_dir, "a1b2c3d4".repeat(8));
    publisher.publish(&identity).unwrap();
    let started = std::time::Instant::now();
    let output = managed_codex_hook(&root, &payload).await;
    assert_managed_hook_silent(&output);
    assert!(
        started.elapsed() < HOOK_EXIT_CEILING,
        "stalled daemon must return within the hook deadline, took {:?}",
        started.elapsed()
    );
    drop(publisher);
    drop(listener);
    std::fs::remove_dir_all(base).unwrap();
}

/// A stalled filesystem lookup still exits within the deadline (T29B final review 1).
///
/// The test seam stalls root resolution and discovery for far longer than the 250 ms budget.
/// Discovery runs on a detached thread whose result is abandoned — never joined — at the
/// deadline, so the process itself must exit 0 with no output; a discovery left on the blocking
/// pool would hold runtime shutdown past the ceiling and fail this test.
#[tokio::test]
async fn binary_managed_codex_hook_stalled_discovery_exits_within_the_deadline() {
    let base = rendezvous_area("hook-stall-discovery");
    let root = base.join("rendezvous");
    // Warm the binary first: cold executable startup must not pollute the measured invocation.
    // The warm invocation carries no stdin, so it exits silently the moment the payload is absent.
    let mut warm = managed_codex_hook_process(&root, None);
    drop(warm.stdin.take());
    let warm_output = tokio::time::timeout(Duration::from_secs(2), warm.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&warm_output);

    let payload = json!({"hook_event_name":"PostToolUse","session_id":"root-session-1",
        "agent_id":"actor-1","tool_use_id":"call-1","tool_name":"Bash"});
    let started = std::time::Instant::now();
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
    command
        .env("TOKIO_WORKER_THREADS", "1")
        .args(["codex-hook", "--managed"])
        .env("AGENT_IDE_CODEX_RENDEZVOUS_ROOT", &root)
        .env("AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS", "5000")
        .env_remove("AGENT_IDE_HOST_ATTACHMENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(payload.to_string().as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        started.elapsed() < HOOK_EXIT_CEILING,
        "a stalled discovery must not extend the exit past the hook deadline, took {:?}",
        started.elapsed()
    );
    assert_managed_hook_silent(&output);
    std::fs::remove_dir_all(base).unwrap();
}

/// `codex-hooks print` emits only the §6 JSON fragment for the running executable.
#[tokio::test]
async fn binary_codex_hooks_print_emits_only_the_managed_fragment() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["codex-hooks", "print"])
        .env_remove("AGENT_IDE_CODEX_RENDEZVOUS_ROOT")
        .output()
        .await
        .unwrap();
    assert!(output.status.success() && output.stderr.is_empty());
    let rendered: Value = serde_json::from_slice(&output.stdout).unwrap();
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_agent-ide")).unwrap();
    let command = format!("{} codex-hook --managed", shell_quote_for_test(&executable));
    assert_eq!(
        rendered,
        json!({
            "hooks": {
                "PreToolUse": [{"matcher": ".*", "hooks": [{
                    "type": "command", "command": command, "timeout": 1}]}],
                "PostToolUse": [{"matcher": ".*", "hooks": [{
                    "type": "command", "command": command, "timeout": 1}]}],
            }
        })
    );
    // Exactly one line of JSON on stdout and nothing else anywhere.
    assert_eq!(String::from_utf8_lossy(&output.stdout).lines().count(), 1);
}

/// Mirrors the product's POSIX-shell single-quoting for the print snapshot assertion.
fn shell_quote_for_test(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// An overridden rendezvous root that is group/other-readable is refused outright by the managed
/// hook (silent exit 0): the environment override exists only for tests, and every safety check
/// — effective-UID owner, exact `0700` directories, `0600` records, no symlinks — still applies
/// to an overridden root (T29B §8). The same root at `0700` answers through the same live daemon,
/// proving only the mode kept the first invocation silent.
#[tokio::test]
async fn binary_managed_codex_hook_refuses_a_loose_rendezvous_root() {
    let base = rendezvous_area("hook-loose-root");
    let root = base.join("rendezvous");
    let (runtime_dir, listener) = fixture_runtime(&base);
    let attachment = "a1b2c3d4".repeat(8);
    let identity = CodexRouteIdentity::new("root-session-1", "actor-1").unwrap();
    let mut publisher = ManagedCodexPublisher::new(root.clone(), runtime_dir, attachment.clone());
    publisher.publish(&identity).unwrap();
    // Warm the binary first: cold executable startup must not eat into the hook deadline.
    let mut warm = managed_codex_hook_process(&root, None);
    drop(warm.stdin.take());
    let warm_output = tokio::time::timeout(Duration::from_secs(2), warm.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&warm_output);
    // One live answerer: any discovery that gets past the checks renders this feedback.
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let size = stream.read_u32().await.unwrap();
            let mut body = vec![0; size as usize];
            stream.read_exact(&mut body).await.unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            let reply = serde_json::to_vec(&json!({
                "version": 2,
                "request_id": request["request_id"],
                "correlation_id": request["correlation_id"],
                "opaque_reply_json": {"state":"feedback","text":"loose-root probe"}
            }))
            .unwrap();
            stream.write_u32(reply.len() as u32).await.unwrap();
            stream.write_all(&reply).await.unwrap();
        }
    });
    let payload = json!({"hook_event_name":"PostToolUse","session_id":"root-session-1",
        "agent_id":"actor-1","tool_use_id":"call-1","tool_name":"Bash"});

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_managed_hook_silent(&managed_codex_hook(&root, &payload).await);

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = managed_codex_hook(&root, &payload).await;
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{output:?}"
    );
    let rendered: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        rendered["hookSpecificOutput"]["additionalContext"],
        "loose-root probe"
    );
    drop(publisher);
    server.abort();
    std::fs::remove_dir_all(base).unwrap();
}

/// Proves CLI query follows its returned cursor and read-only query/export report unknown drops.
#[tokio::test]
async fn telemetry_cli_continues_after_first_page_and_never_invents_drops() {
    let database = runtime().with_extension("sqlite");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE telemetry_events (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                tag TEXT NOT NULL,
                payload BLOB NOT NULL,
                logical_bytes INTEGER NOT NULL CHECK(logical_bytes > 0)
            );",
        )
        .unwrap();
    let payload = br#"{"tag":"native_fallback","reason":"hook_unavailable"}"#;
    let transaction = connection.unchecked_transaction().unwrap();
    for _ in 0..1_001 {
        transaction
            .execute(
                "INSERT INTO telemetry_events(tag,payload,logical_bytes) VALUES(?1,?2,?3)",
                rusqlite::params!["native_fallback", payload, payload.len()],
            )
            .unwrap();
    }
    transaction.commit().unwrap();
    drop(connection);

    let first = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "query", "--database"])
        .arg(&database)
        .output()
        .await
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first["rows"].as_array().unwrap().len(), 1_000);
    assert_eq!(first["next_cursor"], 1_000);
    assert_eq!(first["dropped"], Value::Null);

    let second = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "query", "--database"])
        .arg(&database)
        .args(["--cursor", "1000"])
        .output()
        .await
        .unwrap();
    assert!(second.status.success());
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["rows"].as_array().unwrap().len(), 1);
    assert_eq!(second["rows"][0]["sequence"], 1_001);

    let export = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "export", "--database"])
        .arg(&database)
        .output()
        .await
        .unwrap();
    assert!(export.status.success());
    assert_eq!(export.stdout.split(|byte| *byte == b'\n').count(), 1_002);
    assert!(
        String::from_utf8(export.stderr)
            .unwrap()
            .contains("dropped=null")
    );
    std::fs::remove_file(database).unwrap();
}

/// Reads the closed post-hook acknowledgement through real IPC without exposing it in the hook process.
async fn post_ack(runtime: &Path, actor: &str, call: &str) -> Value {
    use agent_ide::app::{
        submit_hook_if_running,
        transport::{HookSubmit, HookSubmitTransportResult, HookTransportLimits, OpaqueJson},
    };
    let request = HookSubmit::new(
        call,
        call,
        "private-host-channel",
        OpaqueJson::from_value(
            &json!({"host":"codex","phase":"post","actor_id":actor,"call_id":call,
                "session_id":null,"agent_type":null,"launch_command":null,"launch_background":null,"failed":false}),
            1024,
        )
        .unwrap(),
    )
    .unwrap();
    let reply = submit_hook_if_running(
        runtime,
        request,
        HookTransportLimits::new(128 * 1024, 64 * 1024, Duration::from_secs(1)).unwrap(),
    )
    .await;
    let HookSubmitTransportResult::Dispatched {
        opaque_reply_json, ..
    } = reply
    else {
        panic!("daemon must reply");
    };
    serde_json::from_str(opaque_reply_json.as_str()).unwrap()
}

/// Active ordinary-tool hook lifecycles produce bounded recheck hints, including failed commands.
#[tokio::test]
async fn binary_active_native_hooks_accept_edits_deletes_renames_and_failed_commands() {
    let runtime = runtime();
    let mut daemon = daemon(&runtime).await;
    let mut mcp = Mcp::start(&runtime, Some("private-host-channel")).await;
    hook(&runtime, "PreToolUse", "session_id", "actor", "inactive").await;
    assert_eq!(
        post_ack(&runtime, "actor", "inactive").await["reason"],
        "host_binding"
    );
    hook(&runtime, "PreToolUse", "session_id", "actor", "start").await;
    boundary(
        &mcp.exchange(host_call("actor", "start", "ide.start")).await,
        "workspace_activation",
    );
    assert_eq!(
        post_ack(&runtime, "actor", "start").await["state"],
        "hook_settled"
    );
    for (call, command) in [
        ("edit", "edit file"),
        ("delete", "delete file"),
        ("rename", "rename file"),
        ("failed", "exit 7"),
    ] {
        let payload = json!({"hook_event_name":"PreToolUse","session_id":"actor","tool_use_id":call,
            "tool_name":"exec_command","tool_input":{"cmd":command},"tool_response":{"exit_code":7}});
        let mut native = hook_process(&runtime, Some("private-host-channel"));
        native
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(2), native.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
        assert_eq!(
            post_ack(&runtime, "actor", call).await,
            json!({"state":"native_hook_observed"})
        );
        // A native hint must never turn a post-before-MCP call into validated activation.
        boundary(
            &mcp.exchange(host_call("actor", call, "ide.start")).await,
            "host_binding",
        );
    }
    hook(&runtime, "PreToolUse", "session_id", "actor", "stop").await;
    boundary(
        &mcp.exchange(host_call("actor", "stop", "ide.stop")).await,
        "host_stopped",
    );
    hook(
        &runtime,
        "PreToolUse",
        "session_id",
        "actor",
        "after-stop-native",
    )
    .await;
    assert_eq!(
        post_ack(&runtime, "actor", "after-stop-native").await["reason"],
        "host_binding"
    );
    mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Preserves measured nested sandbox fields through real MCP ingress and renders a bounded pending envelope.
#[tokio::test]
async fn binary_preserves_sandbox_metadata_and_renders_closed_pending() {
    use tokio::net::UnixListener;
    let runtime = runtime();
    std::fs::create_dir(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
    let mut mcp = Mcp::start(&runtime, Some("private-host-channel")).await;
    let state = json!({"permissionProfile":{"type":"managed","file_system":{"opaque":[1,2]},"network":{"enabled":false}},"codexLinuxSandboxExe":"/trusted/wrapper","sandboxCwd":"file:///private/tmp/worktree","useLegacyLandlock":false,"preserved":{"nested":"private-state-marker"}});
    let mut call = host_call("actor", "state-call", "ide.start");
    call["params"]["_meta"]["codex/sandbox-state-meta"] = state.clone();
    let peer = async {
        let (mut stream, _) = listener.accept().await.unwrap();
        let size = stream.read_u32().await.unwrap();
        assert!(size < 128 * 1024);
        let mut bytes = vec![0; size as usize];
        stream.read_exact(&mut bytes).await.unwrap();
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            frame["params_json"]["host_meta"]["codex/sandbox-state-meta"],
            state
        );
        assert_eq!(
            frame["params_json"]["parameters"],
            json!({"activation_id":"same-activation"})
        );
        let reply = json!({"version":2,"request_id":frame["request_id"],"opaque_result_json":{"state":"pending","detail_ref":"detail-1"}}).to_string();
        stream.write_u32(reply.len() as u32).await.unwrap();
        stream.write_all(reply.as_bytes()).await.unwrap();
    };
    let (reply, ()) = tokio::join!(mcp.exchange(call), peer);
    assert_eq!(
        reply["result"]["structuredContent"],
        json!({"state":"pending","detail_ref":"detail-1"})
    );
    assert!(!reply.to_string().contains("private-state-marker"));
    assert!(reply.to_string().len() < 64 * 1024);
    mcp.close().await;
    drop(listener);
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Routes typed unavailable and host-stopped replies through the compact structured MCP envelope.
#[tokio::test]
async fn binary_renders_typed_lifecycle_replies_without_transport_errors() {
    use tokio::net::UnixListener;

    let runtime = runtime();
    std::fs::create_dir(&runtime).unwrap();
    let listener = UnixListener::bind(runtime.join("agent-ide.sock")).unwrap();
    let mut mcp = Mcp::start(&runtime, Some("private-host-channel")).await;
    let replies = [
        (
            host_call("actor", "unavailable", "ide.start"),
            json!({"state":"unavailable","reason":"host_binding"}),
            "unavailable: host_binding; continue with native tools",
        ),
        (
            host_call("actor", "stopped", "ide.stop"),
            json!({"state":"host_stopped"}),
            "host_stopped: host binding released; no workspace authority was created",
        ),
    ];
    for (call, expected, text) in replies {
        let peer = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let size = stream.read_u32().await.unwrap();
            let mut bytes = vec![0; size as usize];
            stream.read_exact(&mut bytes).await.unwrap();
            let frame: Value = serde_json::from_slice(&bytes).unwrap();
            let reply = json!({
                "version": 2,
                "request_id": frame["request_id"],
                "opaque_result_json": expected,
            })
            .to_string();
            stream.write_u32(reply.len() as u32).await.unwrap();
            stream.write_all(reply.as_bytes()).await.unwrap();
        };
        let (response, ()) = tokio::join!(mcp.exchange(call), peer);
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        assert_eq!(response["result"]["content"].as_array().unwrap().len(), 1);
        assert_eq!(response["result"]["content"][0]["text"], text);
        assert_eq!(response["result"]["structuredContent"], expected);
    }
    mcp.close().await;
    drop(listener);
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Opens one durable owner after the daemon lock; a rejected second daemon cannot advance its boot.
#[tokio::test]
async fn configured_daemon_opens_workspace_once_after_exclusive_lock() {
    let runtime = runtime();
    let config = runtime.with_extension("json");
    std::fs::write(&config,json!({"version":1,"limits":{"queued":4,"details":8,"operation_ms":1000,"output_bytes":4096},"targets":[]}).to_string()).unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime)
        .env("AGENT_IDE_LAUNCHER_CONFIG", &config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(runtime.join("agent-ide.sock"))
                .await
                .is_ok()
            {
                break;
            }
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let database = rusqlite::Connection::open(runtime.join("state.sqlite")).unwrap();
    let boot: i64 = database
        .query_row("SELECT boot FROM workspace_authority_clock", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(boot, 1);
    let rejected = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime)
        .env("AGENT_IDE_LAUNCHER_CONFIG", &config)
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    let boot: i64 = database
        .query_row("SELECT boot FROM workspace_authority_clock", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(boot, 1);
    drop(database);
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    std::fs::remove_dir_all(runtime).unwrap();
    std::fs::remove_file(config).unwrap();
}

/// Owns a configured product daemon's private Git worktree and accepted test-only disabled profile.
struct ProductFixture {
    /// Unique private parent removed only after this fixture's daemon has exited.
    base: PathBuf,
    /// Explicit canonical worktree target supplied by the trusted launcher fixture.
    root: PathBuf,
    /// Private daemon endpoint directory.
    runtime: PathBuf,
    /// Restart-only launcher JSON, never sent as model arguments.
    config: PathBuf,
    /// Durable telemetry database outside the transient runtime directory, in a `0700` parent.
    ///
    /// An orderly daemon shutdown removes its whole runtime directory, so telemetry that must
    /// survive a restart is selected through the absolute `AGENT_IDE_TELEMETRY_DATABASE` override,
    /// exactly as the managed launcher does.
    telemetry: PathBuf,
}
impl ProductFixture {
    /// Creates committed source plus staged/unstaged changes, without user Git configuration or hooks.
    fn new(providers: Value) -> Self {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "w-{}-{}",
                std::process::id(),
                NEXT_RUNTIME.fetch_add(1, Ordering::Relaxed)
            ));
        let root = base.join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let telemetry_dir = base.join("telemetry");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&telemetry_dir)
            .unwrap();
        let fixture = Self {
            runtime: base.join("ipc"),
            config: base.join("launcher.json"),
            telemetry: telemetry_dir.join("telemetry.sqlite"),
            base,
            root,
        };
        std::fs::write(
            fixture.root.join("Cargo.toml"),
            "[package]\nname=\"product_fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            fixture.root.join("src/lib.rs"),
            "pub fn value() -> i32 { 7 }\npub fn caller() -> i32 { value() }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.root.join("go.mod"),
            "module contract.local/product\n\ngo 1.25.0\n",
        )
        .unwrap();
        std::fs::write(
            fixture.root.join("main.go"),
            "package main\nfunc Value() int { return 7 }\nfunc main() { _ = Value() }\n",
        )
        .unwrap();
        std::fs::write(fixture.root.join("tracked.txt"), "base\n").unwrap();
        fixture.git(&["init", "--quiet"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fixture.git(&["config", "user.name", "Product Fixture"]);
        fixture.git(&["add", "--", "."]);
        fixture.git(&["commit", "--quiet", "-m", "fixture"]);
        std::fs::write(fixture.root.join("tracked.txt"), "index\n").unwrap();
        fixture.git(&["add", "--", "tracked.txt"]);
        std::fs::write(fixture.root.join("tracked.txt"), "worktree\n").unwrap();
        fixture.write_config(providers, None);
        fixture
    }
    /// Creates the same fixture, additionally accepting a strict test-only Claude operator profile.
    ///
    /// This is wiring proof only: it asserts the daemon-side
    /// [`agent_ide::assistance::claude_worker::ClaudeOperatorProfile::validate`] contract, never a
    /// real Claude host's actual sandbox enforcement.
    fn new_claude(providers: Value) -> Self {
        let fixture = Self::new(providers.clone());
        fixture.write_config(
            providers,
            Some(json!({
                "enabled": true,
                "fail_if_unavailable": true,
                "allow_unsandboxed_commands": false,
                "no_matching_excluded_commands": true,
                "scope_declared": true,
                "platform": "mac_os"
            })),
        );
        fixture
    }
    /// Runs fixed local fixture setup with all user/system Git configuration excluded.
    fn git(&self, args: &[&str]) {
        let output = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["-C"])
            .arg(&self.root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "fixture Git failed");
    }
    /// Writes only synthetic fixture acceptance records; this is not live Codex/D03 certification.
    ///
    /// `claude_profile` is `None` for every existing Codex-shaped fixture, keeping their emitted
    /// config byte-for-byte free of the new field; a Claude fixture supplies the strict test-only
    /// operator profile asserted by
    /// [`agent_ide::assistance::claude_worker::ClaudeOperatorProfile::validate`].
    fn write_config(&self, providers: Value, claude_profile: Option<Value>) {
        use agent_ide::execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord};
        let state = HostSandboxState::parse(Some(self.state())).unwrap();
        let record = PersistedProfileRecord::from_execution_evidence(
            "product-fixture-disabled",
            1,
            D03ProfileEvidence {
                provider_binary: "fixture-git".into(),
                toolchain: "fixture-toolchain".into(),
                configuration: "fixture-v1".into(),
                trust: "explicit-test-only-disabled".into(),
                transport: "direct-fixture".into(),
                d03_evidence: "fixture-only-not-host-certification".into(),
            },
            &state,
        )
        .unwrap();
        let mut config = json!({"version":1,"limits":{"queued":16,"details":64,"operation_ms":120000,"output_bytes":1048576},"targets":[{"attachment":"private-host-channel","candidate":self.root,"git":accepted_program("/usr/bin/git","fixture-git"),"codex":accepted_program("/usr/bin/true","unused-disabled-wrapper"),"providers":providers,"profiles":[{"record":serde_json::from_str::<Value>(&record.to_json()).unwrap(),"sandbox_state":self.state()}],"allow_disabled_host":true}]});
        if let Some(claude_profile) = claude_profile {
            config["targets"][0]["claude_profile"] = claude_profile;
        }
        std::fs::write(&self.config, config.to_string()).unwrap();
    }
    /// Returns the current fixture's complete measured-state-shaped payload outside model arguments.
    fn state(&self) -> Value {
        json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":self.root,"useLegacyLandlock":false})
    }
    /// Starts one configured shipping daemon and waits only for its real private endpoint.
    async fn daemon(&self) -> Child {
        self.daemon_with_home(None).await
    }
    /// Starts the configured daemon with durable telemetry captured at [`Self::telemetry`].
    ///
    /// An orderly daemon shutdown removes its whole runtime directory, so only the absolute
    /// `AGENT_IDE_TELEMETRY_DATABASE` override — exactly what the managed launcher selects — makes
    /// sanitized telemetry queryable and exportable after a restart.
    async fn daemon_with_durable_telemetry(&self) -> Child {
        self.spawn_configured_daemon(None, true).await
    }
    /// Starts the configured daemon, optionally with its home (`AGENT_IDE_HOME`, which the product
    /// resolves instead of `$HOME`) redirected into the fixture so its project check caches never
    /// touch the real home directory. Without one it inherits the test-wide `AGENT_IDE_HOME`.
    async fn daemon_with_home(&self, home: Option<&Path>) -> Child {
        self.spawn_configured_daemon(home, false).await
    }
    /// Starts one configured shipping daemon and waits only for its real private endpoint.
    ///
    /// `durable_telemetry` selects the absolute `AGENT_IDE_TELEMETRY_DATABASE` override exactly as
    /// the managed launcher does, keeping capture alive when shutdown removes the runtime directory.
    async fn spawn_configured_daemon(&self, home: Option<&Path>, durable_telemetry: bool) -> Child {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-ide"));
        if let Some(home) = home {
            command.env(agent_ide::userhome::HOME_OVERRIDE_ENV, home);
        }
        if durable_telemetry {
            command.env("AGENT_IDE_TELEMETRY_DATABASE", &self.telemetry);
        }
        let mut daemon = command
            .args(["daemon", "--runtime-dir"])
            .arg(&self.runtime)
            .env("AGENT_IDE_LAUNCHER_CONFIG", &self.config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if UnixStream::connect(self.runtime.join("agent-ide.sock"))
                    .await
                    .is_ok()
                {
                    break;
                }
                if let Some(status) = daemon.try_wait().unwrap() {
                    let mut stderr = String::new();
                    daemon
                        .stderr
                        .take()
                        .unwrap()
                        .read_to_string(&mut stderr)
                        .await
                        .unwrap();
                    panic!("configured daemon exited with {status}: {stderr}");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        daemon
    }
}
impl Drop for ProductFixture {
    /// Removes only the test-owned private tree; callers must stop/reap their daemon first.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Supplies an exact content fingerprint for a fixture's explicitly accepted executable.
fn accepted_program(path: &str, identity: &str) -> Value {
    json!({"path":path,"identity":identity,"blake3":blake3::hash(&std::fs::read(path).unwrap()).to_hex().to_string()})
}

/// Supplies an exact path, length, and content fingerprint for a non-executable bundle member.
fn accepted_typescript_file(path: &Path) -> Value {
    let bytes = std::fs::read(path).unwrap();
    json!({"path":path,"blake3":blake3::hash(&bytes).to_hex().to_string(),"bytes":bytes.len()})
}

/// Builds the accepted Pyright provider from exact environment paths and a fixture cache label.
///
/// `cache_namespace` is a nonempty synthetic label used only inside the disposable fixture.
/// Missing or unreadable `AGENT_IDE_PYRIGHT`/`AGENT_IDE_NODE` paths panic because callers are
/// ignored release tests. The returned value fingerprints both files and stores no source text.
fn accepted_pyright_provider(cache_namespace: &str) -> Value {
    assert!(!cache_namespace.is_empty());
    let pyright = std::env::var("AGENT_IDE_PYRIGHT").unwrap();
    let node = std::env::var("AGENT_IDE_NODE").unwrap();
    json!({
        "executable":accepted_program(&pyright,"pyright 1.1.413"),
        "settings":"pyright_defaults_v1",
        "toolchain":"node-fixture",
        "node":accepted_program(&node,"node-fixture"),
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":cache_namespace
    })
}

/// Builds the release-pinned TypeScript provider from the three exact acceptance environment paths.
///
/// The Node executable, bridge module, and `tsserver.js` must be absolute readable files from the
/// accepted 24.4.0/6.0.0/5.9.3 bundle. Missing files or environment values panic because callers
/// are ignored release tests, and the returned JSON contains both bundle-bound host evidence hashes.
fn accepted_typescript_provider() -> Value {
    let node = PathBuf::from(std::env::var("AGENT_IDE_NODE").unwrap());
    let bridge = PathBuf::from(std::env::var("AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER").unwrap());
    let tsserver = PathBuf::from(std::env::var("AGENT_IDE_TSSERVER").unwrap());
    let typescript_root = tsserver.parent().unwrap().parent().unwrap();
    let bridge_root = bridge.parent().unwrap().parent().unwrap();
    let mut closure = [
        bridge_root.join("package.json"),
        typescript_root.join("lib/_tsserver.js"),
        typescript_root.join("lib/typescript.js"),
        typescript_root.join("package.json"),
    ];
    closure.sort();
    let mut provider = json!({
        "executable":accepted_program(bridge.to_str().unwrap(),"6.0.0"),
        "settings":"typescript_defaults_v1",
        "toolchain":"24.4.0",
        "node":accepted_program(node.to_str().unwrap(),"24.4.0"),
        "typescript":{
            "bridge_bytes":std::fs::metadata(&bridge).unwrap().len(),
            "bridge_version":"6.0.0",
            "tsserver":accepted_typescript_file(&tsserver),
            "typescript_version":"5.9.3",
            "closure":closure.iter().map(|path| accepted_typescript_file(path)).collect::<Vec<_>>(),
            "codex_macos_evidence":"macos-26.6.2-node-24.4.0-tls-6.0.0-ts-5.9.3-codex-r3-2026-09-14",
            "claude_macos_evidence":null
        },
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":"fixture-typescript-cache"
    });
    let unbound: agent_ide::assistance::launcher::ProviderLaunch =
        serde_json::from_value(provider.clone()).unwrap();
    provider["typescript"]["codex_macos_evidence"] =
        json!(unbound.expected_typescript_codex_macos_evidence().unwrap());
    provider["typescript"]["claude_macos_evidence"] =
        json!(unbound.expected_typescript_claude_macos_evidence().unwrap());
    provider
}

/// Returns the operator-verified absolute path of one binary inside the accepted rustup toolchain,
/// honoring `AGENT_IDE_RUST_TOOLCHAIN_DIR` when the harness points at a non-default rustup home.
fn toolchain_bin(tool: &str) -> String {
    let root = std::env::var("AGENT_IDE_RUST_TOOLCHAIN_DIR")
        .unwrap_or_else(|_| "/Users/pluto/.rustup/toolchains/1.98.1-aarch64-apple-darwin".into());
    format!("{root}/bin/{tool}")
}

/// A real MCP client plus host actor/call correlation owned by one configured test target.
struct ProductActor {
    /// Actual shipping MCP process; its stdin is closed explicitly at test completion.
    mcp: Mcp,
    /// Host actor identity, never inserted into the model argument object.
    actor: &'static str,
    /// Trusted per-channel fixture attachment, independent of model arguments.
    attachment: &'static str,
    /// Explicit native actor field for root or child hook fixtures.
    actor_field: &'static str,
    /// Current host metadata fixture, separate from tool arguments and restart configuration.
    state: Value,
    /// Non-reused native call IDs for this host actor.
    next: usize,
}
impl ProductActor {
    /// Initializes one static tool surface on the configured private launcher channel.
    async fn new(fixture: &ProductFixture, actor: &'static str) -> Self {
        Self::new_at(
            fixture,
            actor,
            "private-host-channel",
            "session_id",
            fixture.state(),
        )
        .await
    }
    /// Initializes an explicitly configured channel/actor/state combination for parallel target tests.
    async fn new_at(
        fixture: &ProductFixture,
        actor: &'static str,
        attachment: &'static str,
        actor_field: &'static str,
        state: Value,
    ) -> Self {
        Self {
            mcp: Mcp::start(&fixture.runtime, Some(attachment)).await,
            actor,
            attachment,
            actor_field,
            state,
            next: 100,
        }
    }
    /// Submits one exact root/child hook through the shipping command with the configured attachment.
    async fn lifecycle(&self, fixture: &ProductFixture, phase: &str, call: &str) {
        let output = self.lifecycle_output(fixture, phase, call).await;
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    }
    /// Returns the shipping Codex hook output so a feedback assertion can inspect additionalContext.
    async fn lifecycle_output(
        &self,
        fixture: &ProductFixture,
        phase: &str,
        call: &str,
    ) -> std::process::Output {
        let mut child = hook_process(&fixture.runtime, Some(self.attachment));
        let payload =
            json!({"hook_event_name":phase,self.actor_field:self.actor,"tool_use_id":call});
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }
    /// Submits the same root/child correlation hook through the real shipping Claude command.
    async fn claude_lifecycle(&self, fixture: &ProductFixture, phase: &str, call: &str) {
        let output = self.claude_lifecycle_output(fixture, phase, call).await;
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    }
    /// Returns the shipping Claude hook output so a feedback assertion can inspect additionalContext.
    async fn claude_lifecycle_output(
        &self,
        fixture: &ProductFixture,
        phase: &str,
        call: &str,
    ) -> std::process::Output {
        let mut child = claude_hook_process(&fixture.runtime, Some(self.attachment));
        let payload =
            json!({"hook_event_name":phase,self.actor_field:self.actor,"tool_use_id":call});
        let mut input = child.stdin.take().unwrap();
        input
            .write_all(payload.to_string().as_bytes())
            .await
            .unwrap();
        input.shutdown().await.unwrap();
        drop(input);
        tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap()
    }
    /// Runs exact Pre→MCP→Post through the real Claude hook and Claude tool-use metadata shape.
    async fn call_claude(
        &mut self,
        fixture: &ProductFixture,
        name: &str,
        arguments: Value,
    ) -> Value {
        self.next += 1;
        let call = format!("call-{}", self.next);
        self.claude_lifecycle(fixture, "PreToolUse", &call).await;
        let reply = self
            .mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{
                "name":name,"arguments":arguments,"_meta":{"claudecode/toolUseId":call}}}),
            )
            .await;
        self.claude_lifecycle(fixture, "PostToolUse", &call).await;
        claude_fields(assert_claude_envelope(&reply))
    }
    /// Runs one native Claude tool's Pre/Post hooks and returns the post hook's stdout.
    ///
    /// `tool` is the native tool name relayed on both phases; the pre-hook must stay silent.
    async fn claude_native_post(&mut self, fixture: &ProductFixture, tool: &str) -> String {
        self.next += 1;
        let call = format!("native-{}", self.next);
        let mut post = String::new();
        for phase in ["PreToolUse", "PostToolUse"] {
            let mut child = claude_hook_process(&fixture.runtime, Some(self.attachment));
            let payload = json!({"hook_event_name":phase,self.actor_field:self.actor,
                "tool_use_id":call,"tool_name":tool,"tool_input":{"file_path":"src/lib.rs"}});
            let mut input = child.stdin.take().unwrap();
            input
                .write_all(payload.to_string().as_bytes())
                .await
                .unwrap();
            input.shutdown().await.unwrap();
            drop(input);
            let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
            assert!(output.status.success() && output.stderr.is_empty());
            post = String::from_utf8(output.stdout).unwrap();
            if phase == "PreToolUse" {
                assert!(post.is_empty(), "{post}");
            }
        }
        post
    }
    /// Arms and runs the exact foreground helper named by a pending reply.
    ///
    /// Sends the `Bash` pre-hook that makes the launch recognizable, runs the helper to delivery,
    /// and returns `(detail_ref, launch_call)` with the helper's own `Bash` post hook left for the
    /// caller, which may want to inspect or withhold it (T22B).
    async fn run_claude_pending(
        &self,
        fixture: &ProductFixture,
        pending: &Value,
    ) -> (String, String) {
        assert_eq!(pending["state"], "pending", "{pending}");
        let detail_ref = pending["detail_ref"].as_str().unwrap().to_owned();
        let helper = pending["helper"].as_str().unwrap().to_owned();
        let launch_call = format!("bash-launch-{detail_ref}");
        let mut arm = claude_hook_process(&fixture.runtime, Some(self.attachment));
        arm.stdin
            .take()
            .unwrap()
            .write_all(
                json!({"hook_event_name":"PreToolUse","session_id":self.actor,
                    "tool_use_id":launch_call,"tool_name":"Bash",
                    "tool_input":{"command":helper}})
                .to_string()
                .as_bytes(),
            )
            .await
            .unwrap();
        let armed = arm.wait_with_output().await.unwrap();
        assert!(armed.status.success() && armed.stdout.is_empty() && armed.stderr.is_empty());
        let helper_output = tokio::time::timeout(
            Duration::from_secs(90),
            Command::new("/bin/sh").arg("-c").arg(&helper).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            helper_output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&helper_output.stderr)
        );
        (detail_ref, launch_call)
    }
    /// Runs the exact foreground helper named by a pending reply and returns its owned handle.
    ///
    /// The helper's own `Bash` post hook is sent and asserted silent; a caller that expects model
    /// context there must use [`Self::run_claude_pending`] and send the post itself (T22B).
    async fn launch_claude_pending(&self, fixture: &ProductFixture, pending: &Value) -> String {
        let (detail_ref, launch_call) = self.run_claude_pending(fixture, pending).await;
        let mut post = claude_hook_process(&fixture.runtime, Some(self.attachment));
        post.stdin
            .take()
            .unwrap()
            .write_all(
                json!({"hook_event_name":"PostToolUse","session_id":self.actor,
                    "tool_use_id":launch_call,"tool_response":{"success":true}})
                .to_string()
                .as_bytes(),
            )
            .await
            .unwrap();
        let post = post.wait_with_output().await.unwrap();
        assert!(post.status.success() && post.stdout.is_empty() && post.stderr.is_empty());
        detail_ref
    }
    /// Runs the exact foreground helper named by a pending reply, then inspects its settled result.
    ///
    /// Returns the structured result and the inspect call's post-hook stdout. Start/Diff produce an
    /// empty hook output unless a status plate is due there (T22B); Context may produce the actual
    /// bounded `additionalContext` delta.
    async fn complete_claude_pending(
        &mut self,
        fixture: &ProductFixture,
        pending: &Value,
    ) -> (Value, Vec<u8>) {
        let detail_ref = self.launch_claude_pending(fixture, pending).await;
        self.next += 1;
        let inspect_call = format!("call-{}", self.next);
        self.claude_lifecycle(fixture, "PreToolUse", &inspect_call)
            .await;
        let reply = self
            .mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{
                "name":"ide.inspect","arguments":{"detail_ref":detail_ref},
                "_meta":{"claudecode/toolUseId":inspect_call}}}),
            )
            .await;
        let feedback = self
            .claude_lifecycle_output(fixture, "PostToolUse", &inspect_call)
            .await;
        assert!(feedback.status.success() && feedback.stderr.is_empty());
        (
            claude_fields(assert_claude_envelope(&reply)),
            feedback.stdout,
        )
    }
    /// Runs exact Pre→MCP→Post with current host state separated from bounded model arguments.
    async fn call(&mut self, fixture: &ProductFixture, name: &str, arguments: Value) -> Value {
        self.next += 1;
        let call = format!("call-{}", self.next);
        self.lifecycle(fixture, "PreToolUse", &call).await;
        let reply=self.mcp.exchange(json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"threadId":self.actor,"callId":call,"x-codex-turn-metadata":{},"codex/sandbox-state-meta":self.state}}})).await;
        self.lifecycle(fixture, "PostToolUse", &call).await;
        assert_compact_envelope(&reply);
        reply["result"]["structuredContent"].clone()
    }
    /// Retrieves a same-binding result with fresh call IDs and bounded replay-safe backoff.
    ///
    /// Each inspection uses [`Self::call`], which advances the host correlation before both its
    /// native hook and MCP request. The capped backoff and 61-attempt ceiling cover the fixture's
    /// 120-second operation lifetime without approaching the product replay budget.
    async fn settle(&mut self, fixture: &ProductFixture, mut reply: Value) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
        let mut polls = 0;
        let mut delay = PRODUCT_SETTLE_INITIAL_DELAY;
        while reply["state"] == "pending" {
            assert!(
                tokio::time::Instant::now() < deadline && polls < PRODUCT_SETTLE_MAX_POLLS,
                "product operation did not settle"
            );
            let reference = reply["detail_ref"].as_str().unwrap().to_owned();
            tokio::time::sleep(delay).await;
            polls += 1;
            reply = self
                .call(fixture, "ide.inspect", json!({"detail_ref":reference}))
                .await;
            delay = delay.saturating_mul(2).min(PRODUCT_SETTLE_MAX_DELAY);
        }
        reply
    }
}

/// Reproduces the public deterministic Claude rendezvous contract for product-edge assertions.
///
/// The key is `project`'s canonical git common directory (every fixture is its own real Git
/// repository), independently rediscovered through the real `git` binary rather than assumed, so
/// this stays a black-box reproduction of the product formula instead of a peek at its internals.
fn managed_claude_runtime_path(project: &Path) -> PathBuf {
    let project = std::fs::canonicalize(project).unwrap();
    let output = std::process::Command::new("/usr/bin/git")
        .arg("-C")
        .arg(&project)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .unwrap();
    let key = if output.status.success() {
        std::fs::canonicalize(String::from_utf8(output.stdout).unwrap().trim_end())
            .unwrap_or_else(|_| project.clone())
    } else {
        project
    };
    let hash = blake3::hash(key.as_os_str().as_bytes());
    std::fs::canonicalize("/private/tmp")
        .unwrap()
        .join(format!("ai-r-{}", &hash.to_hex().as_str()[..16]))
}

/// Sends SIGTERM to the exact process holding a shared Claude daemon's runtime lock, if any.
///
/// A shared daemon deliberately outlives every MCP process's own EOF (EYES-r1 §2), so a test that
/// causes one to be spawned must reap it explicitly instead of leaving it running past the test
/// binary's own exit. `lsof` is asked for the specific lock file's current holder only; this never
/// pattern-kills by process name or command line.
/// Guarantees a shared daemon a test caused to be spawned is reaped even if an assertion later in
/// the same test panics, so a failing test cannot leak a long-lived orphan process.
struct SharedClaudeDaemonGuard(PathBuf);

impl Drop for SharedClaudeDaemonGuard {
    fn drop(&mut self) {
        terminate_shared_claude_daemon(&self.0);
    }
}

fn terminate_shared_claude_daemon(runtime: &Path) {
    let Ok(output) = std::process::Command::new("/usr/sbin/lsof")
        .arg("-t")
        .arg(runtime.join("agent-ide.lock"))
        .output()
    else {
        return;
    };
    for pid in String::from_utf8_lossy(&output.stdout).split_whitespace() {
        if let Ok(pid) = pid.parse::<libc::pid_t>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// Builds one exact Claude root or child lifecycle event without any transport attachment fields.
fn managed_claude_event(phase: &str, session: &str, agent: Option<&str>, call: &str) -> Value {
    let mut event = json!({
        "hook_event_name": phase,
        "session_id": session,
        "tool_use_id": call,
    });
    if let Some(agent) = agent {
        event["agent_id"] = Value::String(agent.to_owned());
        event["agent_type"] = Value::String("fixture-child".into());
    }
    event
}

/// Runs exact managed Claude Pre→MCP→terminal-hook correlation for one root or child actor.
async fn managed_claude_call(
    mcp: &mut Mcp,
    project: &Path,
    id: usize,
    session: &str,
    agent: Option<&str>,
    name: &str,
    arguments: Value,
) -> Value {
    let call = format!("managed-claude-{id}");
    let pre = managed_claude_hook(
        Some(project),
        managed_claude_event("PreToolUse", session, agent, &call),
    )
    .await;
    assert!(
        pre.status.success() && pre.stdout.is_empty() && pre.stderr.is_empty(),
        "managed pre-hook failed: {}",
        String::from_utf8_lossy(&pre.stderr)
    );
    let reply = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
                "name":name,"arguments":arguments,"_meta":{"claudecode/toolUseId":call}
            }}),
        )
        .await;
    let post = managed_claude_hook(
        Some(project),
        managed_claude_event("PostToolUse", session, agent, &call),
    )
    .await;
    assert!(post.status.success() && post.stderr.is_empty());
    claude_fields(assert_claude_envelope(&reply))
}

/// Executes and settles one pending managed Claude start through the ordinary foreground helper.
async fn settle_managed_claude_start(
    mcp: &mut Mcp,
    project: &Path,
    next: &mut usize,
    session: &str,
    agent: Option<&str>,
    pending: &Value,
) -> Value {
    assert_eq!(pending["state"], "pending", "{pending}");
    let helper = pending["helper"].as_str().unwrap();
    let detail_ref = pending["detail_ref"].as_str().unwrap();
    let launch_call = format!("managed-claude-bash-{next}");
    let mut pre = managed_claude_event("PreToolUse", session, agent, &launch_call);
    pre["tool_name"] = Value::String("Bash".into());
    pre["tool_input"] = json!({"command":helper});
    let armed = managed_claude_hook(Some(project), pre).await;
    assert!(armed.status.success() && armed.stdout.is_empty() && armed.stderr.is_empty());

    let output = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new("/bin/sh").arg("-c").arg(helper).output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "managed helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut post = managed_claude_event("PostToolUse", session, agent, &launch_call);
    post["tool_response"] = json!({"success":true});
    let settled = managed_claude_hook(Some(project), post).await;
    assert!(settled.status.success() && settled.stdout.is_empty() && settled.stderr.is_empty());

    *next += 1;
    managed_claude_call(
        mcp,
        project,
        *next,
        session,
        agent,
        "ide.inspect",
        json!({"detail_ref":detail_ref}),
    )
    .await
}

/// Lists live short managed runtime directories so EOF cleanup can be observed at the product edge.
fn managed_runtime_paths() -> std::collections::BTreeSet<PathBuf> {
    std::fs::read_dir(std::fs::canonicalize(std::env::temp_dir()).unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ai-") && name.len() == 19)
        })
        .collect()
}

/// Proves managed Codex needs no hooks, observes native edits, isolates actors, and cleans on EOF.
#[tokio::test]
async fn managed_codex_smoke_and_eof_cleanup() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let before = managed_runtime_paths();
    let mut mcp = Mcp::start_managed(&fixture.config, &fixture.root).await;
    let during = managed_runtime_paths();
    assert_eq!(
        during.difference(&before).count(),
        1,
        "{before:?} -> {during:?}"
    );
    let state = fixture.state();
    let actor = "managed-root";
    let mut next = 10;

    let started = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.start",
        json!({"activation_id":"managed-start"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    next += 1;
    let original = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    let original = settle_managed(&mut mcp, &mut next, actor, &state, original).await;
    assert!(original["text"].as_str().unwrap().contains("worktree"));
    let original_ref = original["detail_ref"].as_str().unwrap().to_owned();
    next += 1;
    let retrieved = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.context",
        json!({"path":"tracked.txt","detail_ref":original_ref}),
        &state,
    )
    .await;
    assert_eq!(retrieved, original, "{retrieved}");

    next += 1;
    let edited = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.edit",
        json!({
            "operation_id":"managed-edit-1",
            "path":"tracked.txt",
            "source_ref":original["detail_ref"],
            "content":"managed-ide-edit\n"
        }),
        &state,
    )
    .await;
    let edited = settle_managed(&mut mcp, &mut next, actor, &state, edited).await;
    assert_eq!(edited["state"], "edit", "{edited}");
    assert_eq!(edited["result"]["outcome"], "replaced", "{edited}");
    assert_eq!(
        std::fs::read(fixture.root.join("tracked.txt")).unwrap(),
        b"managed-ide-edit\n"
    );

    // Preferred edit never disables the native fallback; a native writer remains independently usable.
    std::fs::write(fixture.root.join("tracked.txt"), "managed-native-edit\n").unwrap();
    next += 1;
    let refreshed = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    let refreshed = settle_managed(&mut mcp, &mut next, actor, &state, refreshed).await;
    assert!(
        refreshed["text"]
            .as_str()
            .unwrap()
            .contains("managed-native-edit")
    );

    next += 1;
    let diff = managed_call(&mut mcp, next, actor, "ide.diff", json!({}), &state).await;
    let diff = settle_managed(&mut mcp, &mut next, actor, &state, diff).await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    assert!(
        diff["text"]
            .as_str()
            .unwrap()
            .contains("managed-native-edit")
    );
    let diff_ref = diff["detail_ref"].as_str().unwrap().to_owned();
    next += 1;
    let retrieved = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.diff",
        json!({"mode":"head","detail_ref":diff_ref}),
        &state,
    )
    .await;
    assert_eq!(retrieved, diff, "{retrieved}");

    next += 1;
    let forged = mcp.exchange(json!({"jsonrpc":"2.0","id":next,"method":"tools/call","params":{"name":"ide.context","arguments":{"path":"tracked.txt","actor_id":"forged","sandbox":state},"_meta":{"threadId":actor,"callId":format!("managed-{actor}-{next}"),"x-codex-turn-metadata":{},"codex/sandbox-state-meta":fixture.state()}}})).await;
    assert_eq!(forged["result"]["isError"], true, "{forged}");

    next += 1;
    let isolated = mcp.exchange(json!({"jsonrpc":"2.0","id":next,"method":"tools/call","params":{"name":"ide.context","arguments":{"path":"tracked.txt"},"_meta":{"threadId":"managed-stranger","callId":format!("managed-stranger-{next}"),"x-codex-turn-metadata":{},"codex/sandbox-state-meta":fixture.state()}}})).await;
    assert_compact_envelope(&isolated);
    assert_eq!(
        isolated["result"]["structuredContent"],
        json!({"state":"unavailable","reason":"host_binding"}),
        "{isolated}"
    );

    next += 1;
    let stopped = managed_call(&mut mcp, next, actor, "ide.stop", json!({}), &state).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    mcp.close().await;
    assert_eq!(managed_runtime_paths(), before);
}

/// Managed Codex publishes distinct actor routes on first valid calls and retires them on clean
/// shutdown, with the record pointing at exactly this MCP process's private runtime (T29B §2).
#[tokio::test]
async fn managed_codex_publishes_distinct_actor_routes_and_retires_them_on_shutdown() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let base = rendezvous_area("publish");
    let root = base.join("rendezvous");
    let before = managed_runtime_paths();
    let mut mcp = Mcp::start_managed_with_rendezvous(&fixture.config, &fixture.root, &root).await;
    let during = managed_runtime_paths();
    let mut runtimes = during.difference(&before).cloned().collect::<Vec<_>>();
    assert_eq!(runtimes.len(), 1, "{before:?} -> {during:?}");
    let runtime_dir = runtimes.remove(0);
    let state = fixture.state();
    let actor = "managed-root";
    let mut next = 10;

    // The first valid call with a root session publishes this process's route for that actor.
    let start = managed_root_call(
        &mut mcp,
        next,
        actor,
        "root-session-a",
        "ide.start",
        json!({"activation_id":"publish-start"}),
        &state,
    )
    .await;
    let route_a = CodexRouteIdentity::new("root-session-a", actor).unwrap();
    let target = discover(&root, &route_a).expect("first valid call publishes the actor route");
    assert_eq!(
        target.runtime_dir(),
        std::fs::canonicalize(&runtime_dir).unwrap(),
        "the record points at this MCP process's own runtime"
    );
    let started = settle_managed(&mut mcp, &mut next, actor, &state, start).await;
    assert_eq!(started["kind"], "activation", "{started}");

    // A second root session over the same repository gets its own distinct route; both stay live.
    next += 1;
    let refreshed = managed_root_call(
        &mut mcp,
        next,
        actor,
        "root-session-b",
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    assert_eq!(refreshed["state"], "pending", "{refreshed}");
    let route_b = CodexRouteIdentity::new("root-session-b", actor).unwrap();
    assert_ne!(route_a.digest(), route_b.digest());
    assert!(
        discover(&root, &route_b).is_some(),
        "second session publishes its own route"
    );
    assert!(discover(&root, &route_a).is_some(), "routes coexist");

    // Clean EOF shutdown retires every route before removing the runtime.
    mcp.close().await;
    assert!(discover(&root, &route_a).is_none(), "route a retired");
    assert!(discover(&root, &route_b).is_none(), "route b retired");
    assert_eq!(managed_runtime_paths(), before);
    std::fs::remove_dir_all(base).unwrap();
}

/// Retirement is permanent when the owned daemon dies while the MCP process keeps serving
/// (T29B final review 3): the daemon-exit observer retires the publication, and a further MCP
/// call — whose dispatch would otherwise re-publish idempotently — must not re-create a
/// discoverable record for the dead daemon.
#[tokio::test]
async fn managed_codex_daemon_exit_keeps_the_route_retired() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let base = rendezvous_area("daemon-exit-retire");
    let root = base.join("rendezvous");
    let before = managed_runtime_paths();
    let mut mcp = Mcp::start_managed_with_rendezvous(&fixture.config, &fixture.root, &root).await;
    let during = managed_runtime_paths();
    let mut runtimes = during.difference(&before).cloned().collect::<Vec<_>>();
    assert_eq!(runtimes.len(), 1, "{before:?} -> {during:?}");
    let runtime_dir = runtimes.remove(0);
    let state = fixture.state();
    let actor = "managed-root";
    let session = "root-session-a";
    let mut next = 10;
    let identity = CodexRouteIdentity::new(session, actor).unwrap();

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"daemon-exit-start"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    assert!(discover(&root, &identity).is_some(), "route published");

    // Kill the owned daemon outright; its runtime lock names the exact process to signal.
    let holder = Command::new("/usr/sbin/lsof")
        .args(["-t"])
        .arg(runtime_dir.join("agent-ide.lock"))
        .output()
        .await
        .unwrap();
    let mut killed = false;
    for pid in String::from_utf8_lossy(&holder.stdout).split_whitespace() {
        if let Ok(pid) = pid.parse::<libc::pid_t>() {
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
            killed = true;
        }
    }
    assert!(killed, "no daemon holds the owned runtime lock");

    // The exit observer retires the publication; the record stops being discoverable.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while discover(&root, &identity).is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the daemon-exit observer never retired the publication"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A further MCP call still gets an honest reply and must not re-create a discoverable
    // record for the dead daemon: publication is permanently retired.
    next += 1;
    let reply = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":next,"method":"tools/call","params":{"name":"ide.context",
            "arguments":{"kind":"problems"},
            "_meta":{"threadId":actor,"callId":format!("managed-{actor}-{next}"),
            "x-codex-turn-metadata":{"session_id":session},"codex/sandbox-state-meta":state}}}),
        )
        .await;
    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert!(
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("continue with native tools"),
        "{reply}"
    );
    assert!(
        discover(&root, &identity).is_none(),
        "a further MCP call re-published a dead daemon's route"
    );
    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// A publication failure (unusable rendezvous root) never touches managed MCP replies.
#[tokio::test]
async fn managed_codex_publication_failure_leaves_replies_working() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let base = rendezvous_area("publish-failure");
    // A regular file occupies the rendezvous root path: every publication fails quietly.
    let blocked = base.join("rendezvous");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let mut mcp =
        Mcp::start_managed_with_rendezvous(&fixture.config, &fixture.root, &blocked).await;
    let state = fixture.state();
    let actor = "managed-root";
    let mut next = 10;

    let start = managed_root_call(
        &mut mcp,
        next,
        actor,
        "root-session-a",
        "ide.start",
        json!({"activation_id":"publish-failure-start"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, start).await;
    assert_eq!(started["kind"], "activation", "{started}");
    next += 1;
    let context = managed_root_call(
        &mut mcp,
        next,
        actor,
        "root-session-a",
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    let settled = settle_managed(&mut mcp, &mut next, actor, &state, context).await;
    assert!(
        settled["text"].as_str().unwrap().contains("worktree"),
        "{settled}"
    );
    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// A stalled publication never delays MCP dispatch (T29B final review 2).
///
/// The test seam holds this MCP process's route publication far longer than a normal call, so
/// the publication wait must be bounded: the MCP reply still arrives within the normal deadline,
/// the detached publish task finishes in the background afterwards, and teardown's retirement
/// then leaves nothing discoverable.
#[tokio::test]
async fn managed_codex_stalled_publication_keeps_replies_bounded() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let base = rendezvous_area("publish-stall");
    let root = base.join("rendezvous");
    let mut mcp = Mcp::start_managed_custom_with_env(
        &fixture.config,
        &fixture.root,
        None,
        Some(&root),
        Some(("AGENT_IDE_CODEX_RENDEZVOUS_STALL_MS", "6000")),
    )
    .await;
    let state = fixture.state();
    let actor = "managed-root";
    let session = "root-session-a";
    let identity = CodexRouteIdentity::new(session, actor).unwrap();
    let next = 10;

    // The very first valid call would publish before dispatch: with an unbounded publication
    // wait this exchange could not finish inside its normal deadline.
    let started = std::time::Instant::now();
    let start = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"publish-stall-start"}),
        &state,
    )
    .await;
    assert_eq!(start["state"], "pending", "{start}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a stalled publication delayed the MCP reply, took {:?}",
        started.elapsed()
    );
    assert!(
        discover(&root, &identity).is_none(),
        "the stalled publication has not completed yet"
    );

    // The detached publish task still finishes in the background.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while discover(&root, &identity).is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the background publication never completed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Teardown retires the publication permanently.
    mcp.close().await;
    assert!(discover(&root, &identity).is_none(), "route retired");
    std::fs::remove_dir_all(base).unwrap();
}

/// Proves two managed sessions share only telemetry ownership, not Workspace boot authority.
#[tokio::test]
async fn parallel_managed_daemons_do_not_fence_each_others_workspace() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let mut first = Mcp::start_managed(&fixture.config, &fixture.root).await;
    let mut second = Mcp::start_managed(&fixture.config, &fixture.root).await;
    let state = fixture.state();
    let mut first_id = 2;
    let mut second_id = 2;

    let first_started = managed_call(
        &mut first,
        first_id,
        "parallel-first",
        "ide.start",
        json!({"activation_id":"first-session"}),
        &state,
    )
    .await;
    let first_started = settle_managed(
        &mut first,
        &mut first_id,
        "parallel-first",
        &state,
        first_started,
    )
    .await;
    assert_eq!(first_started["kind"], "activation", "{first_started}");

    let second_started = managed_call(
        &mut second,
        second_id,
        "parallel-second",
        "ide.start",
        json!({"activation_id":"second-session"}),
        &state,
    )
    .await;
    let second_started = settle_managed(
        &mut second,
        &mut second_id,
        "parallel-second",
        &state,
        second_started,
    )
    .await;
    assert_eq!(second_started["kind"], "activation", "{second_started}");

    first_id += 1;
    let still_live = managed_call(
        &mut first,
        first_id,
        "parallel-first",
        "ide.context",
        json!({"path":"src/lib.rs"}),
        &state,
    )
    .await;
    let still_live = settle_managed(
        &mut first,
        &mut first_id,
        "parallel-first",
        &state,
        still_live,
    )
    .await;
    assert_eq!(still_live["kind"], "context", "{still_live}");

    first.close().await;
    second.close().await;
}

/// Managed Claude hooks silently ignore absent and corrupt project-derived attachment state.
#[tokio::test]
async fn managed_claude_hook_missing_or_corrupt_attachment_is_silent() {
    let fixture = ProductFixture::new_claude(json!([]));
    let runtime = managed_claude_runtime_path(&fixture.root);
    assert!(!runtime.exists());
    let payload = managed_claude_event("PreToolUse", "root", None, "missing");

    for project in [None, Some(fixture.root.as_path())] {
        let output = managed_claude_hook(project, payload.clone()).await;
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }

    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let missing = managed_claude_hook(Some(&fixture.root), payload.clone()).await;
    assert!(missing.status.success() && missing.stdout.is_empty() && missing.stderr.is_empty());

    std::fs::write(runtime.join("attachment"), b"corrupt").unwrap();
    std::fs::set_permissions(
        runtime.join("attachment"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let corrupt = managed_claude_hook(Some(&fixture.root), payload).await;
    assert!(corrupt.status.success() && corrupt.stdout.is_empty() && corrupt.stderr.is_empty());
    std::fs::remove_dir_all(runtime).unwrap();
}

/// Missing templates and templates without strict Claude evidence stay bounded and disconnected.
#[tokio::test]
async fn managed_claude_startup_requires_template_and_strict_profile() {
    let fixture = ProductFixture::new(json!([]));
    let runtime = managed_claude_runtime_path(&fixture.root);
    for template in [
        fixture.base.join("missing-launcher.json"),
        fixture.config.clone(),
    ] {
        let mut mcp = Mcp::start_managed_claude(&template, &fixture.root).await;
        let response = mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                    "name":"ide.start","arguments":{"activation_id":"unavailable"},
                    "_meta":{"claudecode/toolUseId":"unavailable"}
                }}),
            )
            .await;
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("continue with native tools")
        );
        mcp.close().await;
        // The shared rendezvous directory this MCP created is never removed on exit, even when no
        // daemon ever started inside it; a later MCP simply reuses it through the same idempotent
        // ensure-or-adopt path.
        assert!(runtime.is_dir());
    }
}

/// The standard Claude MCP/hook pair activates root then child; a second MCP for the same
/// repository adopts the same shared daemon instead of starting its own, and the daemon outlives
/// every MCP process's own EOF (EYES-r1 §2).
#[tokio::test]
async fn managed_claude_root_child_rendezvous_shared_daemon_survives_eof() {
    let fixture = ProductFixture::new_claude(json!([]));
    let runtime = managed_claude_runtime_path(&fixture.root);
    let _guard = SharedClaudeDaemonGuard(runtime.clone());
    assert!(!runtime.exists());
    // Claude validates the helper binary before every minted operation. Warm the test artifact so
    // this contract measures rendezvous behavior rather than cold debug-binary filesystem I/O.
    let _helper_bytes = std::fs::read(env!("CARGO_BIN_EXE_agent-ide")).unwrap();
    let mut mcp = Mcp::start_managed_claude(&fixture.config, &fixture.root).await;
    assert!(runtime.is_dir());
    assert_eq!(
        std::fs::symlink_metadata(&runtime)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let attachment_path = runtime.join("attachment");
    assert_eq!(
        std::fs::symlink_metadata(&attachment_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let first_attachment = std::fs::read(&attachment_path).unwrap();

    // The second MCP adopts the exact daemon the first one spawned, so its attachment file is
    // read, never rewritten; the daemon itself still processes the call (unlike the old rejected
    // second-owner contract), but this call carries no prior hook binding for its tool-use id, so
    // it gets the same unavailable "host_binding" outcome as any other unbound call.
    let mut second = Mcp::start_managed_claude(&fixture.config, &fixture.root).await;
    let unbound = second
        .exchange(
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"ide.start","arguments":{"activation_id":"second-owner"},
                "_meta":{"claudecode/toolUseId":"second-owner"}
            }}),
        )
        .await;
    assert_eq!(
        claude_fields(assert_claude_envelope(&unbound)),
        json!({"state":"unavailable","reason":"host_binding"}),
        "{unbound}"
    );
    assert_eq!(std::fs::read(&attachment_path).unwrap(), first_attachment);
    second.close().await;
    assert!(
        runtime.is_dir(),
        "an adopting second owner must not remove the shared runtime"
    );

    let denied_call = "managed-claude-denied";
    let denied_pre = managed_claude_hook(
        Some(&fixture.root),
        managed_claude_event("PreToolUse", "root-session", None, denied_call),
    )
    .await;
    assert!(
        denied_pre.status.success() && denied_pre.stdout.is_empty() && denied_pre.stderr.is_empty()
    );
    let denied_terminal = managed_claude_hook(
        Some(&fixture.root),
        managed_claude_event("PermissionDenied", "root-session", None, denied_call),
    )
    .await;
    assert!(
        denied_terminal.status.success()
            && denied_terminal.stdout.is_empty()
            && denied_terminal.stderr.is_empty()
    );
    let denied = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"ide.start","arguments":{"activation_id":"denied"},
                "_meta":{"claudecode/toolUseId":denied_call}
            }}),
        )
        .await;
    assert_eq!(
        claude_fields(assert_claude_envelope(&denied)),
        json!({"state":"unavailable","reason":"host_binding"}),
        "{denied}"
    );

    let mut next = 10;
    let root_pending = managed_claude_call(
        &mut mcp,
        &fixture.root,
        next,
        "root-session",
        None,
        "ide.start",
        json!({"activation_id":"root-start"}),
    )
    .await;
    let root_started = settle_managed_claude_start(
        &mut mcp,
        &fixture.root,
        &mut next,
        "root-session",
        None,
        &root_pending,
    )
    .await;
    assert_eq!(root_started["kind"], "activation", "{root_started}");

    next += 1;
    let failed_pre = managed_claude_hook(
        Some(&fixture.root),
        managed_claude_event("PreToolUse", "root-session", None, "native-failure"),
    )
    .await;
    let failed_post = managed_claude_hook(
        Some(&fixture.root),
        managed_claude_event("PostToolUseFailure", "root-session", None, "native-failure"),
    )
    .await;
    assert!(failed_pre.status.success() && failed_pre.stderr.is_empty());
    assert!(failed_post.status.success() && failed_post.stderr.is_empty());

    next += 1;
    let root_stopped = managed_claude_call(
        &mut mcp,
        &fixture.root,
        next,
        "root-session",
        None,
        "ide.stop",
        json!({}),
    )
    .await;
    assert_eq!(root_stopped["kind"], "stop", "{root_stopped}");

    next += 1;
    let child_pending = managed_claude_call(
        &mut mcp,
        &fixture.root,
        next,
        "root-session",
        Some("child-agent"),
        "ide.start",
        json!({"activation_id":"child-start"}),
    )
    .await;
    let child_started = settle_managed_claude_start(
        &mut mcp,
        &fixture.root,
        &mut next,
        "root-session",
        Some("child-agent"),
        &child_pending,
    )
    .await;
    assert_eq!(child_started["kind"], "activation", "{child_started}");

    next += 1;
    let child_stopped = managed_claude_call(
        &mut mcp,
        &fixture.root,
        next,
        "root-session",
        Some("child-agent"),
        "ide.stop",
        json!({}),
    )
    .await;
    assert_eq!(child_stopped["kind"], "stop", "{child_stopped}");

    mcp.close().await;
    assert!(
        runtime.is_dir(),
        "the shared daemon must outlive its spawning MCP's own EOF"
    );
}

/// A fresh daemon started by its first non-root worktree pairs the real hook before the first MCP call.
/// A stale cached key must be replaced before either worktree's full helper flow begins.
#[tokio::test]
async fn managed_claude_first_worktree_start_pairs_before_mcp_call() {
    let fixture = ProductFixture::new_claude(json!([]));
    let left = fixture.base.join("first-worktree");
    let right = fixture.base.join("second-worktree");
    fixture.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "first",
        left.to_str().unwrap(),
    ]);
    fixture.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "second",
        right.to_str().unwrap(),
    ]);
    let runtime = managed_claude_runtime_path(&left);
    let _guard = SharedClaudeDaemonGuard(runtime.clone());
    assert!(!runtime.exists());
    let cache = std::fs::canonicalize("/private/tmp").unwrap().join(format!(
        "ai-k-{}",
        &blake3::hash(left.as_os_str().as_bytes()).to_hex().as_str()[..16]
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&cache)
        .unwrap();
    std::fs::write(cache.join("key"), b"/private/tmp/stale-rendezvous-key").unwrap();
    std::fs::set_permissions(cache.join("key"), std::fs::Permissions::from_mode(0o600)).unwrap();

    for (project, session) in [(&left, "first-session"), (&right, "second-session")] {
        let mut mcp = Mcp::start_managed_claude(&fixture.config, project).await;
        let mut next = 1;
        let pending = managed_claude_call(
            &mut mcp,
            project,
            next,
            session,
            None,
            "ide.start",
            json!({"activation_id":session}),
        )
        .await;
        let started =
            settle_managed_claude_start(&mut mcp, project, &mut next, session, None, &pending)
                .await;
        assert_eq!(started["kind"], "activation", "{started}");
        mcp.close().await;
    }
}

/// A removed worktree must not poison later activation in the same repository daemon.
#[tokio::test]
async fn managed_claude_activates_after_removing_an_earlier_worktree() {
    let fixture = ProductFixture::new_claude(json!([]));
    let runtime = managed_claude_runtime_path(&fixture.root);
    let _guard = SharedClaudeDaemonGuard(runtime.clone());
    let left = fixture.base.join("left");
    let right = fixture.base.join("right");
    fixture.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "left",
        left.to_str().unwrap(),
    ]);
    let mut first = Mcp::start_managed_claude(&fixture.config, &left).await;
    let original_attachment = std::fs::read(runtime.join("attachment")).unwrap();
    let mut next = 1;
    let pending = managed_claude_call(
        &mut first,
        &left,
        next,
        "left-session",
        None,
        "ide.start",
        json!({"activation_id":"left-start"}),
    )
    .await;
    let started =
        settle_managed_claude_start(&mut first, &left, &mut next, "left-session", None, &pending)
            .await;
    assert_eq!(started["kind"], "activation", "{started}");
    next += 1;
    let stopped = managed_claude_call(
        &mut first,
        &left,
        next,
        "left-session",
        None,
        "ide.stop",
        json!({}),
    )
    .await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    first.close().await;
    assert!(runtime.is_dir());

    fixture.git(&["worktree", "remove", left.to_str().unwrap()]);
    fixture.git(&[
        "worktree",
        "add",
        "--quiet",
        "-b",
        "right",
        right.to_str().unwrap(),
    ]);
    let mut second = Mcp::start_managed_claude(&fixture.config, &right).await;
    assert_eq!(
        std::fs::read(runtime.join("attachment")).unwrap(),
        original_attachment
    );
    next += 1;
    let pending = managed_claude_call(
        &mut second,
        &right,
        next,
        "right-session",
        None,
        "ide.start",
        json!({"activation_id":"right-start"}),
    )
    .await;
    let started = settle_managed_claude_start(
        &mut second,
        &right,
        &mut next,
        "right-session",
        None,
        &pending,
    )
    .await;
    assert_eq!(started["kind"], "activation", "{started}");
    second.close().await;
}

/// The hook resolves its rendezvous key from its own cache, never by probing `git` itself
/// (EYES-r2 §3): once that cache is warm, a hook call still correlates correctly even after the
/// candidate's `.git` directory is moved away, which a live re-probe would instead treat as a
/// non-git candidate and resolve to a completely different (and unreachable) rendezvous. Reaching
/// the daemon's own unrelated `unsupported_git` outcome (rather than the "host_binding" outcome an
/// uncorrelated call gets) proves the Pre/PostToolUse hooks still bound to the exact right daemon.
#[tokio::test]
async fn managed_claude_hook_relies_on_its_cached_key_not_a_live_git_probe() {
    let fixture = ProductFixture::new_claude(json!([]));
    let runtime = managed_claude_runtime_path(&fixture.root);
    let _guard = SharedClaudeDaemonGuard(runtime.clone());
    let mut mcp = Mcp::start_managed_claude(&fixture.config, &fixture.root).await;
    assert!(runtime.is_dir());

    std::fs::rename(
        fixture.root.join(".git"),
        fixture.base.join("git-moved-away-after-cache-warm"),
    )
    .unwrap();

    let mut next = 1;
    let pending = managed_claude_call(
        &mut mcp,
        &fixture.root,
        next,
        "cache-session",
        None,
        "ide.start",
        json!({"activation_id":"cache-start"}),
    )
    .await;
    let started = settle_managed_claude_start(
        &mut mcp,
        &fixture.root,
        &mut next,
        "cache-session",
        None,
        &pending,
    )
    .await;
    assert_eq!(
        started,
        json!({"state":"error","code":"unsupported_git"}),
        "{started}"
    );

    mcp.close().await;
}

/// A reused Start with unusable sandbox metadata is refused without disturbing the live binding.
///
/// The dispatcher stops a binding a failing Start *created*, which is right for a first call: no
/// generation may survive metadata it could not validate. The regression is the second case. Once
/// an actor is already active, the same actor's later Start reuses that existing generation, so
/// tearing it down on a metadata failure would revoke live authority the model never gave up. Here
/// the refused reuse must leave the original binding and its Workspace generation fully usable.
#[tokio::test]
async fn configured_product_refuses_a_reused_start_with_unusable_sandbox_metadata() {
    let fixture = ProductFixture::new(json!([]));
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "sandbox-reuse-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"first-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    // The same live actor reuses its binding and this time carries unusable measured state.
    let valid = std::mem::replace(&mut actor.state, json!({"permissionProfile":null}));
    let refused = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"invalid-metadata"}),
        )
        .await;
    assert_eq!(
        refused["code"], "sandbox_state",
        "a Start whose measured host state cannot be validated must be refused: {refused}"
    );

    // The original generation is untouched: it still reads source and still stops cleanly.
    actor.state = valid;
    let live = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"src/lib.rs","byte_offset":0}),
        )
        .await;
    let live = actor.settle(&fixture, live).await;
    assert_eq!(
        live["kind"], "context",
        "the refused reuse must not revoke the live binding: {live}"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Bounded host binding generations one daemon retains, mirroring `host_binding::MAX_BINDINGS`.
const MAX_HOST_BINDINGS: usize = 64;

/// An unknown attachment allocates no channel or scope state on either ingress path.
///
/// Both the hook and the method route reject an attachment this daemon's launcher never configured
/// before a channel is resolved, so an unrecognized host cannot mint a channel session, a binding
/// generation or a pending scope.
///
/// What this pins is the observable end state on both ingress paths: hook submission fails open
/// silently, method dispatch returns the unavailable envelope rather than any owner result, and
/// enough distinct stranger actors to fill the bounded binding table leave the configured actor
/// able to start, read and stop normally. Removing `ProductDispatcher`'s own attachment checks
/// alone does not make this fail, because the transport refuses an unconfigured attachment before
/// dispatch as well; the dispatcher check is the second of two layers, and this test proves the
/// combined ingress contract rather than isolating that inner layer.
#[tokio::test]
async fn configured_product_unknown_attachments_allocate_no_channel_or_scope_state() {
    let fixture = ProductFixture::new(json!([]));
    let mut daemon = fixture.daemon().await;

    // Hook ingress: an unknown attachment is accepted silently and fails open with no output.
    let mut child = hook_process(&fixture.runtime, Some("unconfigured-channel"));
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(
            json!({"hook_event_name":"PreToolUse","session_id":"stranger","tool_use_id":"call-1"})
                .to_string()
                .as_bytes(),
        )
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success() && output.stdout.is_empty() && output.stderr.is_empty(),
        "an unknown attachment must fail open with no output"
    );

    // Method ingress: the same unknown attachment gets the unavailable envelope, never a typed
    // owner result, so no scope was created to carry one.
    let mut stranger = ProductActor::new_at(
        &fixture,
        "stranger-root",
        "unconfigured-channel",
        "session_id",
        fixture.state(),
    )
    .await;
    stranger.next += 1;
    let reply=stranger.mcp.exchange(json!({"jsonrpc":"2.0","id":stranger.next,"method":"tools/call","params":{"name":"ide.start","arguments":{"activation_id":"stranger-start"},"_meta":{"threadId":stranger.actor,"callId":"call-1","x-codex-turn-metadata":{},"codex/sandbox-state-meta":stranger.state}}})).await;
    assert_compact_envelope(&reply);
    assert_eq!(
        reply["result"]["structuredContent"],
        json!({"state":"unavailable","reason":"host_binding"}),
        "an unknown attachment must not produce an owner result: {reply}"
    );
    // Allocation is checked by capacity: enough distinct stranger actors to fill the bounded
    // binding table are sent, so an ingress that minted a channel and generation for each of them
    // would leave no room for the configured actor below.
    for index in 0..MAX_HOST_BINDINGS {
        stranger.next += 1;
        let actor = format!("stranger-{index}");
        let reply=stranger.mcp.exchange(json!({"jsonrpc":"2.0","id":stranger.next,"method":"tools/call","params":{"name":"ide.start","arguments":{"activation_id":"stranger-start"},"_meta":{"threadId":actor,"callId":format!("call-{index}"),"x-codex-turn-metadata":{},"codex/sandbox-state-meta":stranger.state}}})).await;
        assert_compact_envelope(&reply);
        assert_eq!(
            reply["result"]["structuredContent"],
            json!({"state":"unavailable","reason":"host_binding"}),
            "an unknown attachment must not produce an owner result: {reply}"
        );
    }
    stranger.mcp.close().await;

    // The configured attachment is unaffected and still starts, reads and stops normally.
    let mut actor = ProductActor::new(&fixture, "configured-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"configured-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Verifies actual durable activation, exact-file context, safe diff modes, detail scope and native invalidation.
#[tokio::test]
async fn configured_product_activates_reads_diffs_invalidates_and_stops() {
    let fixture = ProductFixture::new(json!([]));
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "product-root").await;
    let first = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let started = actor.settle(&fixture, first).await;
    assert_eq!(started["kind"], "activation", "{started}");
    assert!(
        started["text"]
            .as_str()
            .unwrap()
            .contains("baseline: partial (Unverified; durable capture true)"),
        "{started}"
    );
    let retried = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let retried = actor.settle(&fixture, retried).await;
    assert_eq!(retried, started);
    let context = actor
        .call(&fixture, "ide.context", json!({"path":"src/lib.rs"}))
        .await;
    let context = actor.settle(&fixture, context).await;
    assert_eq!(context["kind"], "context", "{context}");
    assert!(context["text"].as_str().unwrap().contains("mode: lexical"));
    assert!(
        context["text"]
            .as_str()
            .unwrap()
            .contains("diagnostics_freshness: unknown")
    );
    assert!(context["text"].as_str().unwrap().contains("pub fn value"));
    let old_context = context["detail_ref"].as_str().unwrap().to_owned();
    actor.state["useLegacyLandlock"] = json!(true);
    let denied = actor
        .call(&fixture, "ide.inspect", json!({"detail_ref":old_context}))
        .await;
    assert_eq!(denied["code"], "execution_profile");
    actor.state["useLegacyLandlock"] = json!(false);

    // Registers a real durable observation for the exact path the diff snapshot below also
    // covers, so the product worker's `ProductSnapshotRunner::current_observation` must load and
    // confirm it from the durable store during the diff capture that follows, not merely accept
    // the trait's default `None`.
    let tracked_context = actor
        .call(&fixture, "ide.context", json!({"path":"tracked.txt"}))
        .await;
    let tracked_context = actor.settle(&fixture, tracked_context).await;
    assert_eq!(tracked_context["kind"], "context", "{tracked_context}");

    let mut diff_ref = String::new();
    for mode in ["head", "staged", "unstaged"] {
        let diff = actor.call(&fixture, "ide.diff", json!({"mode":mode})).await;
        let diff = actor.settle(&fixture, diff).await;
        assert_eq!(diff["kind"], "diff", "{diff}");
        let text = diff["text"].as_str().unwrap();
        assert!(text.contains("tracked.txt"));
        match mode {
            "head" => {
                assert!(text.contains("-base") && text.contains("+worktree"));
                assert!(text.contains("baseline_window: Some(Unverified)"), "{text}");
                diff_ref = diff["detail_ref"].as_str().unwrap().to_owned();
            }
            "staged" => {
                assert!(text.contains("-base") && text.contains("+index"));
                assert!(
                    text.contains("baseline_window: Some(NotCaptured)"),
                    "{text}"
                );
            }
            _ => {
                assert!(text.contains("-index") && text.contains("+worktree"));
                assert!(text.contains("baseline_window: Some(NotCaptured)"));
            }
        }
    }
    let wrong_mode = actor
        .call(
            &fixture,
            "ide.diff",
            json!({"mode":"staged","detail_ref":diff_ref}),
        )
        .await;
    assert_eq!(wrong_mode["code"], "invalid_detail");
    std::fs::write(fixture.root.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
    hook(
        &fixture.runtime,
        "PreToolUse",
        "session_id",
        actor.actor,
        "native-edit",
    )
    .await;
    hook(
        &fixture.runtime,
        "PostToolUse",
        "session_id",
        actor.actor,
        "native-edit",
    )
    .await;
    let stale = actor
        .call(&fixture, "ide.inspect", json!({"detail_ref":old_context}))
        .await;
    assert_eq!(stale["code"], "source_unavailable");
    let latest = actor
        .call(&fixture, "ide.context", json!({"path":"src/lib.rs"}))
        .await;
    let latest = actor.settle(&fixture, latest).await;
    assert!(latest["text"].as_str().unwrap().contains("changed"));
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "product-root").await;
    let fresh = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let fresh = actor.settle(&fixture, fresh).await;
    assert_eq!(fresh["kind"], "activation", "{fresh}");
    assert_ne!(fresh["detail_ref"], started["detail_ref"]);
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Holds SQLite's real write lock while a shipping Start needs durable activation, proving the
/// frontend fails closed without delaying the independent host hook or ordinary native command.
#[tokio::test]
async fn configured_product_db_write_contention_fails_open_without_blocking_native_turn() {
    let fixture = ProductFixture::new(json!([]));
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "db-contention-root").await;
    let lock = rusqlite::Connection::open(fixture.runtime.join("state.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE;").unwrap();

    actor.next += 1;
    let start_call = format!("call-{}", actor.next);
    let hook_started = std::time::Instant::now();
    actor.lifecycle(&fixture, "PreToolUse", &start_call).await;
    assert!(
        hook_started.elapsed() < Duration::from_secs(2),
        "the shipping hook must retain its independent bounded completion"
    );
    let pending = actor
        .mcp
        .exchange(json!({"jsonrpc":"2.0","id":actor.next,"method":"tools/call","params":{"name":"ide.start","arguments":{"activation_id":"blocked-start"},"_meta":{"threadId":actor.actor,"callId":start_call,"x-codex-turn-metadata":{},"codex/sandbox-state-meta":actor.state}}}))
        .await;
    actor.lifecycle(&fixture, "PostToolUse", &start_call).await;
    assert!(
        !pending["result"]["structuredContent"].is_null(),
        "{pending}"
    );
    let blocked = actor
        .settle(&fixture, pending["result"]["structuredContent"].clone())
        .await;
    assert_eq!(blocked["state"], "error", "{blocked}");
    assert_eq!(blocked["code"], "workspace_activation", "{blocked}");
    assert_ne!(blocked["kind"], "activation", "{blocked}");

    let native_call = format!("call-{}", actor.next + 1);
    let native_hook_started = std::time::Instant::now();
    actor.lifecycle(&fixture, "PreToolUse", &native_call).await;
    let native = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("/bin/sh")
            .args(["-c", "test -f tracked.txt"])
            .current_dir(&fixture.root)
            .status(),
    )
    .await
    .expect("ordinary native command must not wait for SQLite")
    .unwrap();
    actor.lifecycle(&fixture, "PostToolUse", &native_call).await;
    assert!(native.success());
    assert!(
        native_hook_started.elapsed() < Duration::from_secs(2),
        "the hook around the ordinary native command must remain bounded"
    );

    lock.execute_batch("ROLLBACK;").unwrap();
    drop(lock);
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Reclaims only the stopped actor's bounded result capacity while preserving its live peer's handles.
#[tokio::test]
async fn configured_product_stop_reclaims_only_its_binding_details() {
    let fixture = ProductFixture::new(json!([]));
    let peer = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["limits"]["details"] = json!(3);
    let mut peer_target = config["targets"][0].clone();
    peer_target["attachment"] = json!("private-child-channel");
    peer_target["candidate"] = json!(peer.root);
    config["targets"].as_array_mut().unwrap().push(peer_target);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut first = ProductActor::new(&fixture, "capacity-first").await;
    let mut second = ProductActor::new_at(
        &fixture,
        "capacity-second",
        "private-child-channel",
        "agent_id",
        peer.state(),
    )
    .await;
    let start = first
        .call(&fixture, "ide.start", json!({"activation_id":"first"}))
        .await;
    assert_eq!(first.settle(&fixture, start).await["kind"], "activation");
    let start = second
        .call(&fixture, "ide.start", json!({"activation_id":"second"}))
        .await;
    let start = second.settle(&fixture, start).await;
    assert_eq!(start["kind"], "activation", "{start}");
    let context = first
        .call(&fixture, "ide.context", json!({"path":"src/lib.rs"}))
        .await;
    assert_eq!(first.settle(&fixture, context).await["kind"], "context");
    let full = second
        .call(&fixture, "ide.context", json!({"path":"src/lib.rs"}))
        .await;
    assert_eq!(full["code"], "capacity", "{full}");
    let stopped = first.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    let retained = second
        .call(
            &fixture,
            "ide.inspect",
            json!({"detail_ref":start["detail_ref"]}),
        )
        .await;
    assert_eq!(retained["kind"], "activation", "{retained}");
    let context = second
        .call(&fixture, "ide.context", json!({"path":"src/lib.rs"}))
        .await;
    let context = second.settle(&fixture, context).await;
    assert_eq!(context["kind"], "context", "{context}");
    let stopped = second.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    first.mcp.close().await;
    second.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Exercises two fresh source/cache roots through actual gopls and accepted Rust product sessions.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS, AGENT_IDE_GO, AGENT_IDE_RUST_ANALYZER and AGENT_IDE_RUST_TOOLCHAIN environment"]
async fn configured_product_returns_real_go_and_rust_semantic_context() {
    use std::os::unix::fs::PermissionsExt;
    let gopls = std::env::var("AGENT_IDE_GOPLS").unwrap();
    let go = std::env::var("AGENT_IDE_GO").unwrap();
    let toolchain = std::env::var("AGENT_IDE_RUST_TOOLCHAIN").unwrap();
    let analyzer = std::env::var("AGENT_IDE_RUST_ANALYZER").unwrap();
    for _ in 0..2 {
        let fixture = ProductFixture::new(json!([]));
        let wrapper = fixture.base.join("rust-provider");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec '{}' \"$@\"\n",
                analyzer.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let providers = json!([{"executable":accepted_program(&gopls,"golang.org/x/tools/gopls v0.23.0"),"settings":"gopls_defaults","toolchain":go,"cargo":null,"cargo_version":null,"rustc":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"fixture-go-cache"},{"executable":accepted_program(wrapper.to_str().unwrap(),"rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),"settings":"rust_cache_priming_disabled_v1","toolchain":toolchain,"cargo":accepted_program(&toolchain_bin("cargo"),"cargo 1.98.1"),"cargo_version":"cargo 1.98.1","rustc":accepted_program(&toolchain_bin("rustc"),"rustc 1.98.1"),"rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"fixture-rust-cache"}]);
        fixture.write_config(providers, None);
        let mut daemon = fixture.daemon().await;
        let mut actor = ProductActor::new(&fixture, "provider-root").await;
        let start = actor
            .call(
                &fixture,
                "ide.start",
                json!({"activation_id":"semantic-start"}),
            )
            .await;
        let start = actor.settle(&fixture, start).await;
        assert_eq!(start["kind"], "activation", "{start}");
        for (path, symbol) in [("main.go", "Value()"), ("src/lib.rs", "value()")] {
            let bytes = std::fs::read_to_string(fixture.root.join(path)).unwrap();
            let offset = bytes.rfind(symbol).unwrap();
            let response = actor
                .call(
                    &fixture,
                    "ide.context",
                    json!({"path":path,"byte_offset":offset}),
                )
                .await;
            let response = actor.settle(&fixture, response).await;
            if response["kind"] != "context" {
                actor.mcp.close().await;
                daemon.kill().await.unwrap();
                let output = daemon.wait_with_output().await.unwrap();
                panic!(
                    "{path}: calls={}: {response}; daemon stderr={}",
                    actor.next,
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            if !response["text"]
                .as_str()
                .unwrap()
                .contains("mode: semantic")
            {
                let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
                let stopped = actor.settle(&fixture, stopped).await;
                actor.mcp.close().await;
                daemon.kill().await.unwrap();
                let output = daemon.wait_with_output().await.unwrap();
                panic!(
                    "{response}; cleanup={stopped}; daemon stderr={}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(
                response["text"]
                    .as_str()
                    .unwrap()
                    .contains("mode: semantic"),
                "{response}"
            );
            assert!(
                response["text"]
                    .as_str()
                    .unwrap()
                    .contains("definitions: [{"),
                "{response}"
            );
        }
        let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
        assert_eq!(stopped["kind"], "stop", "{stopped}");
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        daemon.wait().await.unwrap();
    }
}

/// Exercises the accepted exclusive Pyright process through the five-tool Codex product loop.
///
/// The test requires launcher-owned executable and Node identities. It proves a `.py` session
/// reaches semantic definition/reference and the currently rendered diagnostic state, then proves
/// a corrected retry is fresh before returning a diff and reaping the owned child.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT and AGENT_IDE_NODE environment"]
async fn configured_product_returns_real_pyright_semantic_context_and_reaps() {
    let providers = json!([accepted_pyright_provider("fixture-pyright-cache")]);
    let fixture = ProductFixture::new(providers);
    let path = fixture.root.join("main.py");
    std::fs::write(
        &path,
        "def value() -> int:\n    return \"bad\"\n\ndef caller() -> int:\n    return value()\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "main.py"]);
    fixture.git(&["commit", "--quiet", "-m", "python fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "pyright-root").await;
    let start = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"pyright-start"}),
        )
        .await;
    let start = actor.settle(&fixture, start).await;
    assert_eq!(start["kind"], "activation", "{start}");
    let source = std::fs::read_to_string(&path).unwrap();
    let response = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.py","byte_offset":source.rfind("value()").unwrap()}),
        )
        .await;
    let response = actor.settle(&fixture, response).await;
    assert_eq!(response["kind"], "context", "{response}");
    assert!(
        response["text"]
            .as_str()
            .unwrap()
            .contains("mode: semantic"),
        "{response}"
    );
    let text = response["text"].as_str().unwrap();
    assert!(text.contains("definitions: [{"), "{response}");
    assert!(text.contains("references: [{"), "{response}");
    assert!(text.contains("diagnostic_count: 1"), "{response}");
    assert!(
        text.contains("Type \\\"Literal['bad']\\\" is not assignable to return type \\\"int\\\""),
        "{response}"
    );
    assert!(text.contains("return \"bad\""), "{response}");
    std::fs::write(
        &path,
        "def value() -> int:\n    return 8\n\ndef caller() -> int:\n    return value()\n",
    )
    .unwrap();
    let fixed = std::fs::read_to_string(&path).unwrap();
    let fixed_response = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.py","byte_offset":fixed.rfind("value()").unwrap()}),
        )
        .await;
    let fixed_response = actor.settle(&fixture, fixed_response).await;
    assert_eq!(fixed_response["kind"], "context", "{fixed_response}");
    let fixed_text = fixed_response["text"].as_str().unwrap();
    assert!(fixed_text.contains("mode: semantic"), "{fixed_response}");
    assert!(fixed_text.contains("return 8"), "{fixed_response}");
    assert!(
        fixed_text.contains("source_sequence: 2"),
        "{fixed_response}"
    );
    assert!(
        !fixed_text.contains("return \"bad\"") && !fixed_text.contains("Literal['bad']"),
        "stale Pyright diagnostic survived corrected retry: {fixed_response}"
    );
    let diff = actor.call(&fixture, "ide.diff", json!({})).await;
    let diff = actor.settle(&fixture, diff).await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    let stopped = actor.settle(&fixture, stopped).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Exercises the integrated macOS product loop, stale-write fence, restart telemetry, and fallback.
///
/// This ignored release gate uses the exact configured Pyright and Node files. It proves a known
/// diagnostic can be edited to another same-response reported diagnostic and then to current clean,
/// with every MCP carrier checked by [`assert_compact_envelope`]. It also proves a stale source
/// reference writes nothing, a later native edit remains usable, and sanitized edit telemetry
/// selected by the durable `AGENT_IDE_TELEMETRY_DATABASE` override is queryable and exportable
/// after a graceful daemon restart removed the transient runtime directory. Host CLI
/// identity/containment remains outside this product-contract test and is recorded by the external
/// acceptance driver.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT and AGENT_IDE_NODE environment"]
async fn configured_product_acceptance_edit_diagnostics_telemetry_and_fallback() {
    let providers = json!([accepted_pyright_provider(
        "fixture-acceptance-pyright-cache"
    )]);
    let fixture = ProductFixture::new(providers);
    let path = fixture.root.join("main.py");
    let initial = "def value() -> int:\n    return \"initial-private-bad\"\n\ndef caller() -> int:\n    return value()\n";
    std::fs::write(&path, initial).unwrap();
    fixture.git(&["add", "--", "main.py"]);
    fixture.git(&["commit", "--quiet", "-m", "acceptance fixture"]);

    let mut daemon = fixture.daemon_with_durable_telemetry().await;
    let mut actor = ProductActor::new(&fixture, "acceptance-product").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"acceptance-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let context = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.py","byte_offset":initial.rfind("value()").unwrap()}),
        )
        .await;
    let context = actor.settle(&fixture, context).await;
    assert_eq!(context["kind"], "context", "{context}");
    assert!(
        context["text"]
            .as_str()
            .unwrap()
            .contains("diagnostic_count: 1"),
        "{context}"
    );
    let stale_ref = context["detail_ref"].as_str().unwrap().to_owned();

    let intervening = "def value() -> int:\n    return \"intervening-private-bad\"\n\ndef caller() -> int:\n    return value()\n";
    std::fs::write(&path, intervening).unwrap();
    let stale = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"acceptance-stale",
                "path":"main.py",
                "source_ref":stale_ref,
                "content":"def value() -> int:\n    return 0\n"
            }),
        )
        .await;
    let stale = actor.settle(&fixture, stale).await;
    assert_eq!(stale["result"]["outcome"], "stale_source", "{stale}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), intervening);

    let current = actor
        .call(&fixture, "ide.context", json!({"path":"main.py"}))
        .await;
    let current = actor.settle(&fixture, current).await;
    let reported_source = "def value() -> int:\n    return \"reported-private-bad\"\n\ndef caller() -> int:\n    return value()\n";
    let reported = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"acceptance-reported",
                "path":"main.py",
                "source_ref":current["detail_ref"],
                "content":reported_source
            }),
        )
        .await;
    let reported = actor.settle(&fixture, reported).await;
    assert_eq!(reported["result"]["outcome"], "replaced", "{reported}");
    assert_eq!(
        reported["diagnostics"]["state"], "current_reported",
        "{reported}"
    );
    assert!(
        !reported["diagnostics"]["messages"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{reported}"
    );

    let fixed = "def value() -> int:\n    return 8\n\ndef caller() -> int:\n    return value()\n";
    let clean = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"acceptance-clean",
                "path":"main.py",
                "source_ref":reported["result"]["source_ref"],
                "content":fixed
            }),
        )
        .await;
    let clean = actor.settle(&fixture, clean).await;
    assert_eq!(clean["result"]["outcome"], "replaced", "{clean}");
    assert_eq!(clean["diagnostics"]["state"], "current_clean", "{clean}");

    let diff = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let diff = actor.settle(&fixture, diff).await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    assert!(
        diff["text"].as_str().unwrap().contains("return 8"),
        "{diff}"
    );

    let native = "def value() -> int:\n    return 9\n\ndef caller() -> int:\n    return value()\n";
    std::fs::write(&path, native).unwrap();
    actor
        .lifecycle(&fixture, "PreToolUse", "acceptance-native-edit")
        .await;
    actor
        .lifecycle(&fixture, "PostToolUse", "acceptance-native-edit")
        .await;
    let refreshed = actor
        .call(&fixture, "ide.context", json!({"path":"main.py"}))
        .await;
    let refreshed = actor.settle(&fixture, refreshed).await;
    assert!(
        refreshed["text"].as_str().unwrap().contains("return 9"),
        "{refreshed}"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;

    let daemon_pid = daemon.id().unwrap() as libc::pid_t;
    // SAFETY: this test owns the live daemon identified by the Tokio Child handle.
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGTERM) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), daemon.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    let mut restarted = fixture.daemon_with_durable_telemetry().await;
    let mut restarted_actor = ProductActor::new(&fixture, "acceptance-restarted").await;
    let fresh = restarted_actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"acceptance-restart"}),
        )
        .await;
    let fresh = restarted_actor.settle(&fixture, fresh).await;
    assert_eq!(fresh["kind"], "activation", "{fresh}");
    let stopped = restarted_actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    restarted_actor.mcp.close().await;
    let restarted_pid = restarted.id().unwrap() as libc::pid_t;
    // SAFETY: this test owns the restarted live daemon identified by its Tokio Child handle.
    assert_eq!(unsafe { libc::kill(restarted_pid, libc::SIGTERM) }, 0);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), restarted.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    let telemetry = fixture.telemetry.clone();
    let query = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "query", "--database"])
        .arg(&telemetry)
        .args(["--tag", "tool_completed"])
        .output()
        .await
        .unwrap();
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    let rows = query["rows"].as_array().unwrap();
    assert!(rows.iter().any(|row| {
        row["event"]["method"] == "edit" && row["event"]["diagnostics"] == "changed"
    }));
    assert!(
        rows.iter().any(|row| {
            row["event"]["method"] == "edit" && row["event"]["diagnostics"] == "clean"
        })
    );

    let export = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "export", "--database"])
        .arg(&telemetry)
        .args(["--tag", "tool_completed"])
        .output()
        .await
        .unwrap();
    assert!(export.status.success());
    let export = String::from_utf8(export.stdout).unwrap();
    for forbidden in [
        "main.py",
        "initial-private-bad",
        "intervening-private-bad",
        "reported-private-bad",
        "operation_id",
        "source_ref",
        "prompt",
        "credential",
        "command",
    ] {
        assert!(
            !export.contains(forbidden),
            "telemetry leaked {forbidden}: {export}"
        );
    }
}

/// Exercises real JS, JSX, TS, and TSX through the pinned exclusive TypeScript product profile.
///
/// The ignored release check requires exact launcher-owned Node, bridge, `tsserver.js`, and loaded
/// closure paths. Each extension must reach semantic definition/reference results through a fresh
/// one-shot bridge; exact configured membership proves project selection, while graceful shutdown,
/// EOF, zero exit, and direct-child reap are enforced by the production path before the next
/// fixture may run.
#[tokio::test]
#[ignore = "requires exact AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environment"]
async fn configured_product_returns_real_typescript_family_context_and_reaps() {
    let providers = json!([accepted_typescript_provider()]);
    let fixture = ProductFixture::new(providers);
    std::fs::write(
        fixture.root.join("tsconfig.json"),
        "{\"compilerOptions\":{\"types\":[],\"moduleResolution\":\"node10\",\"allowJs\":true},\"files\":[\"fixture.js\",\"fixture.jsx\",\"fixture.ts\",\"fixture.tsx\"]}\n",
    )
    .unwrap();
    let cases = [
        (
            "fixture.js",
            "export function identity(input) { return input; }\nexport const value = 42;\nexport const use = value;\n",
            "value;",
        ),
        (
            "fixture.jsx",
            "export function identity(input) { return input; }\nexport function Component() { return <div />; }\nexport const view = <Component />;\n",
            "Component />",
        ),
        (
            "fixture.ts",
            "export function identity(input) { return input; }\nexport const value: number = 42;\nexport const use: number = value;\n",
            "value;",
        ),
        (
            "fixture.tsx",
            "export function identity(input) { return input; }\nexport function Component(): JSX.Element { return <div />; }\nexport const view = <Component />;\n",
            "Component />",
        ),
    ];
    for (path, source, _) in cases {
        std::fs::write(fixture.root.join(path), source).unwrap();
    }
    fixture.git(&[
        "add",
        "--",
        "fixture.js",
        "fixture.jsx",
        "fixture.ts",
        "fixture.tsx",
        "tsconfig.json",
    ]);
    fixture.git(&["commit", "--quiet", "-m", "TypeScript fixtures"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "typescript-root").await;
    let start = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"typescript-start"}),
        )
        .await;
    let start = actor.settle(&fixture, start).await;
    assert_eq!(start["kind"], "activation", "{start}");
    for (path, source, occurrence) in cases {
        let response = actor
            .call(
                &fixture,
                "ide.context",
                json!({"path":path,"byte_offset":source.rfind(occurrence).unwrap()}),
            )
            .await;
        let response = actor.settle(&fixture, response).await;
        assert_eq!(response["kind"], "context", "{path}: {response}");
        let text = response["text"].as_str().unwrap();
        assert!(text.contains("mode: semantic"), "{path}: {response}");
        assert!(text.contains("definitions: [{"), "{path}: {response}");
        assert!(text.contains("references: [{"), "{path}: {response}");
    }
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Exercises the release-pinned TypeScript provider through Claude's foreground helper path.
///
/// The ignored release check requires the exact accepted Node, bridge, `tsserver.js`, and closure
/// environment paths. A helper-private `.ts` session must return semantic definition/reference
/// evidence and complete its graceful shutdown before the helper reports all children reaped.
#[tokio::test]
#[ignore = "requires exact AGENT_IDE_NODE, AGENT_IDE_TYPESCRIPT_LANGUAGE_SERVER and AGENT_IDE_TSSERVER environment"]
async fn configured_product_claude_helper_returns_real_typescript_semantic_context_and_reaps() {
    let fixture = ProductFixture::new_claude(json!([accepted_typescript_provider()]));
    let source = "export const value: number = 42;\nexport const use: number = value;\n";
    std::fs::write(fixture.root.join("fixture.ts"), source).unwrap();
    std::fs::write(
        fixture.root.join("tsconfig.json"),
        "{\"compilerOptions\":{\"types\":[],\"moduleResolution\":\"node10\"},\"files\":[\"fixture.ts\"]}\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "fixture.ts", "tsconfig.json"]);
    fixture.git(&["commit", "--quiet", "-m", "TypeScript Claude fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "claude-typescript").await;
    let started = actor
        .call_claude(
            &fixture,
            "ide.start",
            json!({"activation_id":"typescript-start"}),
        )
        .await;
    let (started, _) = actor.complete_claude_pending(&fixture, &started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let context = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"fixture.ts","byte_offset":source.rfind("value;").unwrap()}),
        )
        .await;
    let (context, _) = actor.complete_claude_pending(&fixture, &context).await;
    assert_eq!(context["kind"], "context", "{context}");
    let text = context["text"].as_str().unwrap();
    assert!(text.contains("mode: semantic"), "{context}");
    assert!(text.contains("definitions: [{"), "{context}");
    assert!(text.contains("references: [{"), "{context}");

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Exercises the accepted exclusive Pyright process through Claude's foreground-helper route.
///
/// The test proves that the helper reconstructs the same launcher-bound Pyright profile as Codex:
/// semantic definitions and references, the exact current diagnostic, a native-edit refresh, the
/// tracked diff, and Stop all complete without daemon-side provider execution.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_PYRIGHT and AGENT_IDE_NODE environment"]
async fn configured_product_claude_helper_returns_real_pyright_semantic_context_diff_and_stop() {
    let pyright = std::env::var("AGENT_IDE_PYRIGHT").unwrap();
    let node = std::env::var("AGENT_IDE_NODE").unwrap();
    let node_identity = "node-fixture";
    let providers = json!([{
        "executable":accepted_program(&pyright,"pyright 1.1.413"),
        "settings":"pyright_defaults_v1",
        "toolchain":node_identity,
        "node":accepted_program(&node,node_identity),
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":"fixture-claude-pyright-cache"
    }]);
    let fixture = ProductFixture::new_claude(providers);
    let path = fixture.root.join("main.py");
    std::fs::write(
        &path,
        "def value() -> int:\n    return \"bad\"\n\ndef caller() -> int:\n    return value()\n",
    )
    .unwrap();
    fixture.git(&["add", "--", "main.py"]);
    fixture.git(&["commit", "--quiet", "-m", "claude python fixture"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "claude-pyright").await;

    let pending = actor
        .call_claude(
            &fixture,
            "ide.start",
            json!({"activation_id":"pyright-start"}),
        )
        .await;
    let (started, feedback) = actor.complete_claude_pending(&fixture, &pending).await;
    assert_eq!(started["kind"], "activation", "{started}");
    assert!(feedback.is_empty());

    let source = std::fs::read_to_string(&path).unwrap();
    let pending = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"main.py","byte_offset":source.rfind("value()").unwrap()}),
        )
        .await;
    let (context, _) = actor.complete_claude_pending(&fixture, &pending).await;
    let text = context["text"].as_str().unwrap();
    assert_eq!(context["kind"], "context", "{context}");
    assert!(text.contains("mode: semantic"), "{context}");
    assert!(text.contains("definitions: [{"), "{context}");
    assert!(text.contains("references: [{"), "{context}");
    assert!(text.contains("diagnostic_count: 1"), "{context}");
    assert!(
        text.contains("Type \\\"Literal['bad']\\\" is not assignable to return type \\\"int\\\""),
        "{context}"
    );

    std::fs::write(
        &path,
        "def value() -> int:\n    return 8\n\ndef caller() -> int:\n    return value()\n",
    )
    .unwrap();
    actor
        .claude_lifecycle(&fixture, "PreToolUse", "native-python-edit")
        .await;
    actor
        .claude_lifecycle(&fixture, "PostToolUse", "native-python-edit")
        .await;
    let fixed = std::fs::read_to_string(&path).unwrap();
    let pending = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"main.py","byte_offset":fixed.rfind("value()").unwrap()}),
        )
        .await;
    let (refreshed, _) = actor.complete_claude_pending(&fixture, &pending).await;
    let refreshed_text = refreshed["text"].as_str().unwrap();
    assert!(refreshed_text.contains("return 8"), "{refreshed}");
    assert!(
        !refreshed_text.contains("return \\\"bad\\\"")
            && !refreshed_text.contains("Literal['bad']"),
        "stale Pyright diagnostic survived native edit: {refreshed}"
    );

    let pending = actor
        .call_claude(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let (diff, feedback) = actor.complete_claude_pending(&fixture, &pending).await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    assert!(
        diff["text"].as_str().unwrap().contains("return 8"),
        "{diff}"
    );
    assert!(feedback.is_empty());
    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Proves CARGO/RUSTC threading actually lets rust-analyzer load the Cargo workspace under
/// `env_clear`: a detached single file cannot resolve a symbol defined only in a path-dependency
/// crate, so a passing cross-crate definition is real evidence of loaded workspace semantics, not
/// same-file lexical fallback. The managed-sandbox `procMacro`-disabled route is proven separately
/// (`session_tests::managed_rust_settings_disable_proc_macro_expansion`); this exercises the same
/// production Rust profile construction through the disabled-profile fixture route.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_RUST_ANALYZER and AGENT_IDE_RUST_TOOLCHAIN environment"]
async fn configured_product_rust_resolves_definition_across_a_crate_boundary() {
    use std::os::unix::fs::PermissionsExt;
    let toolchain = std::env::var("AGENT_IDE_RUST_TOOLCHAIN").unwrap();
    let analyzer = std::env::var("AGENT_IDE_RUST_ANALYZER").unwrap();
    let fixture = ProductFixture::new(json!([]));
    std::fs::create_dir_all(fixture.root.join("dep/src")).unwrap();
    std::fs::write(
        fixture.root.join("dep/Cargo.toml"),
        "[package]\nname=\"dep\"\nversion=\"0.1.0\"\nedition=\"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("dep/src/lib.rs"),
        "pub fn shared_value() -> i32 { 42 }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("Cargo.toml"),
        "[package]\nname=\"product_fixture\"\nversion=\"0.1.0\"\nedition=\"2024\"\n\n[dependencies]\ndep = { path = \"dep\" }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.root.join("src/lib.rs"),
        "pub fn value() -> i32 { dep::shared_value() }\npub fn caller() -> i32 { value() }\n",
    )
    .unwrap();
    fixture.git(&["add", "-A"]);
    fixture.git(&["commit", "--quiet", "-m", "cross-crate fixture"]);
    let wrapper = fixture.base.join("rust-provider");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexec '{}' \"$@\"\n",
            analyzer.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let providers = json!([{"executable":accepted_program(wrapper.to_str().unwrap(),"rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),"settings":"rust_cache_priming_disabled_v1","toolchain":toolchain,"cargo":accepted_program(&toolchain_bin("cargo"),"cargo 1.98.1"),"cargo_version":"cargo 1.98.1","rustc":accepted_program(&toolchain_bin("rustc"),"rustc 1.98.1"),"rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"fixture-rust-cross-crate-cache"}]);
    fixture.write_config(providers, None);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "cross-crate-root").await;
    let start = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"cross-crate-start"}),
        )
        .await;
    let start = actor.settle(&fixture, start).await;
    if start["kind"] != "activation" {
        actor.mcp.close().await;
        daemon.kill().await.unwrap();
        let output = daemon.wait_with_output().await.unwrap();
        panic!(
            "{start}; daemon stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let bytes = std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap();
    let offset = bytes.rfind("shared_value()").unwrap();
    let response = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"src/lib.rs","byte_offset":offset}),
        )
        .await;
    let response = actor.settle(&fixture, response).await;
    assert_eq!(response["kind"], "context", "{response}");
    let text = response["text"].as_str().unwrap();
    assert!(text.contains("mode: semantic"), "{response}");
    assert!(
        text.contains("dep/src/lib.rs"),
        "cross-crate definition did not resolve into the dependency crate: {response}"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A real second actor on the same worktree is refused as a finite conflict, and hands off on stop.
///
/// The refusal comes from durable activation itself (`AuthorityError::WorktreeOwned`), not from the
/// cache map, so the important part is that the actor which already owns the worktree keeps working
/// while the second one is told exactly why it cannot start, and that the same second actor starts
/// successfully once the first has stopped.
#[tokio::test]
async fn configured_product_reports_a_second_actor_on_one_worktree_as_a_conflict() {
    let fixture = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    let mut second_target = config["targets"][0].clone();
    second_target["attachment"] = json!("private-second-channel");
    config["targets"]
        .as_array_mut()
        .unwrap()
        .push(second_target);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut first = ProductActor::new(&fixture, "owning-view").await;
    let mut second = ProductActor::new_at(
        &fixture,
        "waiting-view",
        "private-second-channel",
        "agent_id",
        fixture.state(),
    )
    .await;

    let started = first
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"first-start"}),
        )
        .await;
    let started = first.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let refused = second
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"second-start"}),
        )
        .await;
    let refused = second.settle(&fixture, refused).await;
    assert_eq!(
        refused["code"], "conflict",
        "a second live actor on one worktree must get the finite ownership conflict: {refused}"
    );

    // The refusal must not have disturbed the owner: its context still resolves.
    let live = first
        .call(
            &fixture,
            "ide.context",
            json!({"path":"src/lib.rs","byte_offset":0}),
        )
        .await;
    let live = first.settle(&fixture, live).await;
    assert_eq!(live["kind"], "context", "{live}");

    let stopped = first.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");

    // Handoff after a successful stop: the same second actor now activates.
    let handed = second
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"second-handoff"}),
        )
        .await;
    let handed = second.settle(&fixture, handed).await;
    assert_eq!(handed["kind"], "activation", "{handed}");
    let stopped = second.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    tokio::join!(first.mcp.close(), second.mcp.close());
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Cancels a configured owned provider during warmup: the direct child is still killed and reaped even
/// though its socket identity was never captured, and the stop honestly reports that uncertainty
/// instead of a false success.
#[tokio::test]
async fn configured_product_stop_reaps_a_provider_that_never_becomes_ready() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductFixture::new(json!([]));
    let program = fixture.base.join("slow-provider");
    let marker = fixture.base.join("provider-pid");
    let environment = fixture.base.join("provider-environment");
    let environment_keys = fixture.base.join("provider-environment-keys");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n%s\\n%s\\n%s' \"$GOPLSCACHE\" \"$GOCACHE\" \"$GOMODCACHE\" \"$GOTMPDIR\" \"$TMPDIR\" \"$PATH\" > '{}'\nenv | sed 's/=.*//' | sort > '{}'\nprintf '%s' $$ > '{}'\nexec /bin/sleep 30\n",
            environment.display(),
            environment_keys.display(),
            marker.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"slow-fixture-provider"),"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"slow-fixture-cache"}]), None);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "cancel-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"cancel-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let cache_namespaces = std::fs::read_dir(fixture.runtime.join("cache"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    // One shared native namespace (holding `gopls/`/`tmp`, keyed by executable/settings/toolchain/
    // trust) plus one private per-worktree namespace (also holding `gopls/`, with
    // `go-build`/`go-mod` distinguishing it from the shared namespace).
    assert_eq!(cache_namespaces.len(), 2);
    let pending = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":59}),
        )
        .await;
    assert_eq!(pending["state"], "pending", "{pending}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid: libc::pid_t = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    let provider_environment = std::fs::read_to_string(environment).unwrap();
    let shared_namespace = cache_namespaces
        .iter()
        .find(|namespace| namespace.join("gopls").is_dir() && !namespace.join("go-build").exists())
        .expect("shared native namespace");
    let worktree_namespace = cache_namespaces
        .iter()
        .find(|namespace| namespace.join("go-build").is_dir())
        .expect("per-worktree namespace");
    assert!(worktree_namespace.join("gopls").is_dir());
    // The shared listener process env carries only the shared, process-global `GOPLSCACHE` and a
    // backend-scoped native `TMPDIR` inside that same shared namespace; the per-worktree
    // `GOCACHE`/`GOMODCACHE`/`GOTMPDIR` are never process env (they are delivered per view through
    // the LSP session instead), so those three are unset here.
    assert_eq!(
        provider_environment.lines().collect::<Vec<_>>(),
        vec![
            shared_namespace.join("gopls").to_str().unwrap(),
            "",
            "",
            "",
            shared_namespace.join("tmp").to_str().unwrap(),
            "/usr/bin",
        ]
    );
    // `/bin/sh` sets PWD, SHLVL and _ in the fixture script itself; every other name in the
    // dumped set was delivered by the daemon, so an extra inherited or leaked variable fails here.
    assert_eq!(
        std::fs::read_to_string(&environment_keys)
            .unwrap()
            .lines()
            .filter(|key| !matches!(*key, "PWD" | "SHLVL" | "_"))
            .collect::<Vec<_>>(),
        vec![
            "AGENT_IDE_GOPLS_PROFILE",
            "GOPLSCACHE",
            "GOTOOLCHAIN",
            "PATH",
            "TMPDIR",
        ],
        "the provider environment must be exactly this finite cleared set"
    );
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    // The provider never created its socket, so `close_provider` cannot prove the on-disk socket's
    // fate; it still kills and reaps the direct child below but reports the cleanup honestly instead
    // of a false "stop" success.
    assert_eq!(stopped["code"], "internal", "{stopped}");
    assert_eq!(stopped["state"], "error", "{stopped}");
    assert!(shared_namespace.is_dir());
    assert!(worktree_namespace.is_dir());
    // SAFETY: zero only probes the fixture's previously recorded direct-child PID; it sends no signal.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A provider that exits before ever attempting to bind its listener (a sandboxed `gopls` refused the
/// `bind()` syscall is the real-world case) must fall back to a lexical `ide.context` result almost
/// immediately, never by exhausting the 120s operation budget polling a socket that has not appeared
/// yet. `context` treats `FailureCode::ProviderUnavailable` as a deliberate lexical fallback rather
/// than a fatal error, so the settled reply is `state: "complete"` carrying the fallback reason, not
/// `state: "error"`. This fixture's provider never touches its socket path at all, so its cleanup
/// stays exactly as unproved/uncertain as the still-alive never-ready case (preceding test): Stop
/// still honestly reports `internal` here, since nothing ever proved that path clean; what changed is
/// only that `ide.context` no longer waits out the full operation deadline to learn that. A fresh
/// restart afterward must still work end to end, proving no leaked capacity or process from the first
/// failure.
#[tokio::test]
async fn configured_product_context_settles_promptly_when_provider_exits_before_bind() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductFixture::new(json!([]));
    let program = fixture.base.join("exit-before-bind-provider");
    let marker = fixture.base.join("exit-before-bind-marker");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf 'contract-fixture: exit before bind\\n' >&2\n: > '{}'\nexit 2\n",
            marker.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(
        json!([{"executable":accepted_program(program.to_str().unwrap(),"exit-before-bind-fixture-provider"),"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"exit-before-bind-fixture-cache"}]),
        None,
    );
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "exit-before-bind-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"exit-before-bind-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let pending = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":59}),
        )
        .await;
    assert_eq!(pending["state"], "pending", "{pending}");
    let began = tokio::time::Instant::now();
    let settled = actor.settle(&fixture, pending).await;
    let elapsed = began.elapsed();
    assert!(
        elapsed < Duration::from_secs(20),
        "an already-dead listener must settle well before the 120s operation budget, took {elapsed:?}"
    );
    assert_eq!(settled["state"], "complete", "{settled}");
    assert!(
        settled["text"]
            .as_str()
            .unwrap()
            .contains("accepted semantic provider is unavailable"),
        "{settled}"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // This provider never touches its socket path, so nothing ever proves that path clean; Stop
    // honestly reports the same unproved `internal` disposition as the still-alive never-ready case,
    // exactly like `configured_product_stop_reaps_a_provider_that_never_becomes_ready` above.
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["state"], "error", "{stopped}");
    assert_eq!(stopped["code"], "internal", "{stopped}");

    // A fresh Start/Context cycle against the same always-failing configured provider must still
    // work end to end, proving no quarantined capacity or leaked process from the first failure.
    let restarted = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"exit-before-bind-restart"}),
        )
        .await;
    let restarted = actor.settle(&fixture, restarted).await;
    assert_eq!(restarted["kind"], "activation", "{restarted}");
    let refreshed = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":59}),
        )
        .await;
    assert_eq!(refreshed["state"], "pending", "{refreshed}");
    let refreshed = actor.settle(&fixture, refreshed).await;
    assert_eq!(refreshed["state"], "complete", "{refreshed}");
    assert!(
        refreshed["text"]
            .as_str()
            .unwrap()
            .contains("accepted semantic provider is unavailable"),
        "{refreshed}"
    );

    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// SIGTERM stops admission, reaps an active owned provider, and removes both owned socket paths.
#[tokio::test]
async fn configured_product_sigterm_reaps_active_provider_and_owned_sockets() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductFixture::new(json!([]));
    let program = fixture.base.join("signal-provider");
    let listener_ready = fixture.base.join("listener-ready");
    let forwarder_ready = fixture.base.join("forwarder-ready");
    let process = fixture.base.join("provider-process");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\ncase \"$*\" in *-listen=unix*) ready='{}';; *) ready='{}';; esac\nprintf '%s\\t%s\\n' $$ \"$*\" >> '{}'\n: > \"$ready\"\nexec /bin/sleep 30\n",
            listener_ready.display(),
            forwarder_ready.display(),
            process.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"signal-fixture-provider"),"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"signal-fixture-cache"}]), None);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "signal-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"signal-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let pending = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":59}),
        )
        .await;
    assert_eq!(pending["state"], "pending", "{pending}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !listener_ready.exists() {
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let listener = std::fs::read_to_string(&process).unwrap();
    let (listener_pid, listener_arguments) =
        listener.lines().next().unwrap().split_once('\t').unwrap();
    let listener_pid: libc::pid_t = listener_pid.parse().unwrap();
    let listener_socket = listener_arguments
        .split_whitespace()
        .find_map(|argument| argument.strip_prefix("-listen=unix;"))
        .map(PathBuf::from)
        .unwrap();
    let _provider_socket = std::os::unix::net::UnixListener::bind(&listener_socket).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !forwarder_ready.exists() {
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let processes = std::fs::read_to_string(&process).unwrap();
    let provider_pids = processes
        .lines()
        .map(|line| {
            line.split_once('\t')
                .unwrap()
                .0
                .parse::<libc::pid_t>()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(provider_pids.len(), 2, "{processes}");
    // SAFETY: signal zero only observes the readiness-marked direct child and changes no state.
    assert_eq!(unsafe { libc::kill(listener_pid, 0) }, 0);
    let daemon_pid = daemon.id().unwrap() as libc::pid_t;
    // SAFETY: this test owns the live daemon subprocess identified by its Tokio Child handle.
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "daemon exited with {status}");
    for provider_pid in provider_pids {
        // SAFETY: signal zero only verifies a readiness-marked provider PID after daemon completion.
        assert_eq!(unsafe { libc::kill(provider_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    assert!(!listener_socket.exists());
    assert!(!fixture.runtime.join("agent-ide.sock").exists());
    actor.mcp.close().await;
}

/// SIGTERM cooperatively cancels and reaps an in-flight Rust-only provider before daemon exit.
#[tokio::test]
async fn configured_product_sigterm_reaps_in_flight_rust_only_provider() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductFixture::new(json!([]));
    let program = fixture.base.join("signal-rust-provider");
    let ready = fixture.base.join("rust-provider-ready");
    let process = fixture.base.join("rust-provider-process");
    let environment = fixture.base.join("rust-provider-environment");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n%s\\n%s\\n%s' \"$CARGO_HOME\" \"$CARGO_TARGET_DIR\" \"$TMPDIR\" \"$RUSTUP_TOOLCHAIN\" > '{}'\nprintf '%s' $$ > '{}'\n: > '{}'\nexec /bin/sleep 30\n",
            environment.display(),
            process.display(),
            ready.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"rust-analyzer signal fixture"),"settings":"rust_cache_priming_disabled_v1","toolchain":"stable","cargo":accepted_program("/usr/bin/true","cargo 1.98.1"),"cargo_version":"cargo 1.98.1","rustc":accepted_program("/usr/bin/true","rustc 1.98.1"),"rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"signal-rust-cache"}]), None);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "signal-rust-root").await;
    let started = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"signal-rust-start"}),
        )
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let pending = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"src/lib.rs","byte_offset":48}),
        )
        .await;
    assert_eq!(pending["state"], "pending", "{pending}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            assert!(daemon.try_wait().unwrap().is_none());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let provider_pid: libc::pid_t = std::fs::read_to_string(&process).unwrap().parse().unwrap();
    let namespaces = std::fs::read_dir(fixture.runtime.join("cache"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    assert_eq!(namespaces.len(), 1);
    assert_eq!(
        std::fs::read_to_string(environment)
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        vec![
            namespaces[0].join("cargo").to_str().unwrap(),
            namespaces[0].join("target").to_str().unwrap(),
            namespaces[0].join("tmp").to_str().unwrap(),
            "stable",
        ]
    );
    // SAFETY: signal zero only observes the readiness-marked direct child and changes no state.
    assert_eq!(unsafe { libc::kill(provider_pid, 0) }, 0);
    let daemon_pid = daemon.id().unwrap() as libc::pid_t;
    // SAFETY: this test owns the live daemon subprocess identified by its Tokio Child handle.
    assert_eq!(unsafe { libc::kill(daemon_pid, libc::SIGTERM) }, 0);
    let status = tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "daemon exited with {status}");
    // SAFETY: signal zero verifies only the readiness-marked Rust provider after daemon completion.
    assert_eq!(unsafe { libc::kill(provider_pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert!(!fixture.runtime.join("agent-ide.sock").exists());
    actor.mcp.close().await;
}

/// Two configured root/child channels on divergent worktrees share the one compatible heavy
/// listener and its shared native namespace, each through its own forwarder view and its own
/// private Go build/module/temp namespace, while current source and stop stay isolated.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS and AGENT_IDE_GO environment"]
async fn configured_product_isolates_go_across_two_divergent_worktree_actors() {
    use std::os::unix::fs::PermissionsExt;
    let gopls = std::env::var("AGENT_IDE_GOPLS").unwrap();
    let go = std::env::var("AGENT_IDE_GO").unwrap();
    let fixture = ProductFixture::new(json!([]));
    let invocation_log = fixture.base.join("gopls-invocations");
    let wrapper = fixture.base.join("gopls-provider");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$$\" \"$PWD\" \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
            invocation_log.display(),
            gopls.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(wrapper.to_str().unwrap(),"golang.org/x/tools/gopls v0.23.0"),"settings":"gopls_defaults","toolchain":go,"cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"shared-fixture-cache"}]), None);
    let child_root = fixture.base.join("child");
    std::fs::create_dir(&child_root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(&child_root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    std::fs::write(
        child_root.join("go.mod"),
        "module contract.local/product\n\ngo 1.25.0\n",
    )
    .unwrap();
    std::fs::write(child_root.join("main.go"),"package main\nfunc Value() string { return \"child-value\" }\nfunc main() { _ = Value() }\n").unwrap();
    git(&["init", "--quiet"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["add", "--", "."]);
    git(&["commit", "--quiet", "-m", "fixture"]);
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    let mut child_target = config["targets"][0].clone();
    child_target["attachment"] = json!("private-child-channel");
    child_target["candidate"] = json!(child_root);
    config["targets"].as_array_mut().unwrap().push(child_target);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut root = ProductActor::new(&fixture, "root-view").await;
    let mut child_state = fixture.state();
    child_state["sandboxCwd"] = json!(child_root);
    let mut child = ProductActor::new_at(
        &fixture,
        "child-view",
        "private-child-channel",
        "agent_id",
        child_state,
    )
    .await;
    let (root_start, child_start) = tokio::join!(
        root.call(&fixture, "ide.start", json!({"activation_id":"same-start"})),
        child.call(&fixture, "ide.start", json!({"activation_id":"same-start"}))
    );
    let (root_start, child_start) = tokio::join!(
        root.settle(&fixture, root_start),
        child.settle(&fixture, child_start)
    );
    assert_eq!(root_start["kind"], "activation", "{root_start}");
    assert_eq!(child_start["kind"], "activation", "{child_start}");
    let root_offset = std::fs::read_to_string(fixture.root.join("main.go"))
        .unwrap()
        .rfind("Value()")
        .unwrap();
    let child_offset = std::fs::read_to_string(child_root.join("main.go"))
        .unwrap()
        .rfind("Value()")
        .unwrap();
    let (a, b) = tokio::join!(
        root.call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":root_offset})
        ),
        child.call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":child_offset})
        )
    );
    let (a, b) = tokio::join!(root.settle(&fixture, a), child.settle(&fixture, b));
    assert!(
        a["text"].as_str().unwrap().contains("mode: semantic"),
        "{a}"
    );
    assert!(
        b["text"].as_str().unwrap().contains("mode: semantic"),
        "{b}"
    );
    assert!(a["text"].as_str().unwrap().contains("return 7"));
    assert!(b["text"].as_str().unwrap().contains("child-value"));
    assert!(!a["text"].as_str().unwrap().contains("child-value"));
    let invocations = std::fs::read_to_string(&invocation_log).unwrap();
    let listeners = invocations
        .lines()
        .filter(|line| line.contains("-listen=unix;"))
        .collect::<Vec<_>>();
    let forwarders = invocations
        .lines()
        .filter(|line| line.contains("-remote=unix;"))
        .collect::<Vec<_>>();
    // Root and child are divergent worktrees with a compatible executable/settings/toolchain/trust
    // identity, so they share the one heavy gopls listener (its process-global on-disk filecache is
    // bound to a single shared native namespace) while each worktree still gets its own forwarder
    // view and its own private Go build/module/temp namespace.
    assert_eq!(listeners.len(), 1, "{invocations}");
    assert_eq!(forwarders.len(), 2, "{invocations}");
    assert!(
        forwarders
            .iter()
            .any(|line| line.contains(fixture.root.to_str().unwrap())),
        "{invocations}"
    );
    assert!(
        forwarders
            .iter()
            .any(|line| line.contains(child_root.to_str().unwrap())),
        "{invocations}"
    );
    let listener_pid: libc::pid_t = listeners[0].split('\t').next().unwrap().parse().unwrap();
    // SAFETY: signal zero only observes the wrapper-recorded listener PID and changes no process state.
    assert_eq!(unsafe { libc::kill(listener_pid, 0) }, 0);
    let sockets = std::fs::read_dir(&fixture.runtime)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("g-"))
        .count();
    assert_eq!(sockets, 1);
    let cache_root = fixture.runtime.join("cache");
    let worktree_namespaces_before = std::fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("go-build").is_dir())
        .count();
    // One shared native namespace (holding `gopls/`) plus one private namespace per worktree
    // (holding `go-build`/`go-mod`/`tmp`).
    assert_eq!(worktree_namespaces_before, 2, "{cache_root:?}");
    assert!(
        std::fs::read_dir(&cache_root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("gopls").is_dir()),
        "{cache_root:?}"
    );
    let stop = root.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stop["kind"], "stop", "{stop}");
    // Stopping root's actor must not retire child's still-live worktree namespace, and the shared
    // native namespace and root's own worktree namespace must also survive this handoff.
    let worktree_namespaces_after = std::fs::read_dir(&cache_root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("go-build").is_dir())
        .count();
    assert_eq!(worktree_namespaces_after, 2, "{cache_root:?}");
    assert!(
        std::fs::read_dir(&cache_root)
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("gopls").is_dir()),
        "{cache_root:?}"
    );
    let live = child
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":child_offset}),
        )
        .await;
    let live = child.settle(&fixture, live).await;
    assert!(live["text"].as_str().unwrap().contains("mode: semantic"));
    assert!(live["text"].as_str().unwrap().contains("child-value"));
    let stop = child.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stop["kind"], "stop", "{stop}");
    tokio::join!(root.mcp.close(), child.mcp.close());
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Holds the first real shared gopls listener before startup, then proves a bounded product burst
/// queues valid work, refuses overflow, starts one listener/two forwarders, and keeps the peer view.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS and AGENT_IDE_GO environment"]
async fn configured_product_cold_go_burst_preserves_admission_and_peer_view() {
    use std::os::unix::fs::PermissionsExt;
    let gopls = std::env::var("AGENT_IDE_GOPLS").unwrap();
    let go = std::env::var("AGENT_IDE_GO").unwrap();
    let fixture = ProductFixture::new(json!([]));
    let gate = fixture.base.join("release-gopls-listener");
    let invocation_log = fixture.base.join("gopls-cold-invocations");
    let wrapper = fixture.base.join("gopls-cold-provider");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s\\t%s\\t%s\\n' \"$$\" \"$PWD\" \"$*\" >> '{}'\ncase \"$*\" in *'-listen=unix;'*) while [ ! -f '{}' ]; do sleep 0.01; done;; esac\nexec '{}' \"$@\"\n",
            invocation_log.display(),
            gate.display(),
            gopls.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(wrapper.to_str().unwrap(),"golang.org/x/tools/gopls v0.23.0"),"settings":"gopls_defaults","toolchain":go,"cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"cold-shared-fixture-cache"}]), None);
    let child_root = fixture.base.join("cold-child");
    std::fs::create_dir(&child_root).unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .env_clear()
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .arg("-C")
            .arg(&child_root)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success());
    };
    std::fs::write(
        child_root.join("go.mod"),
        "module contract.local/cold-child\n\ngo 1.25.0\n",
    )
    .unwrap();
    std::fs::write(child_root.join("main.go"), "package main\nfunc Value() string { return \"cold-child\" }\nfunc main() { _ = Value() }\n").unwrap();
    git(&["init", "--quiet"]);
    git(&["config", "user.email", "fixture@example.invalid"]);
    git(&["config", "user.name", "Fixture"]);
    git(&["add", "--", "."]);
    git(&["commit", "--quiet", "-m", "fixture"]);
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    let mut child_target = config["targets"][0].clone();
    child_target["attachment"] = json!("private-child-channel");
    child_target["candidate"] = json!(child_root);
    config["targets"].as_array_mut().unwrap().push(child_target);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    for index in 0..15 {
        std::fs::write(
            fixture.root.join(format!("burst-{index}.go")),
            format!(
                "package main\nfunc Burst{index}() string {{ return \"burst-{index}\" }}\nfunc callBurst{index}() {{ _ = Burst{index}() }}\n"
            ),
        )
        .unwrap();
    }
    std::fs::write(
        fixture.root.join("burst-overflow.go"),
        "package main\nfunc BurstOverflow() string { return \"burst-overflow\" }\nfunc callBurstOverflow() { _ = BurstOverflow() }\n",
    )
    .unwrap();
    let mut daemon = fixture.daemon().await;
    let mut root = ProductActor::new(&fixture, "cold-root").await;
    let mut child_state = fixture.state();
    child_state["sandboxCwd"] = json!(child_root);
    let mut child = ProductActor::new_at(
        &fixture,
        "cold-child",
        "private-child-channel",
        "agent_id",
        child_state,
    )
    .await;
    let (root_start, child_start) = tokio::join!(
        root.call(&fixture, "ide.start", json!({"activation_id":"cold-start"})),
        child.call(&fixture, "ide.start", json!({"activation_id":"cold-start"}))
    );
    assert_eq!(
        root.settle(&fixture, root_start).await["kind"],
        "activation"
    );
    assert_eq!(
        child.settle(&fixture, child_start).await["kind"],
        "activation"
    );
    let root_offset = std::fs::read_to_string(fixture.root.join("main.go"))
        .unwrap()
        .rfind("Value()")
        .unwrap();
    let child_offset = std::fs::read_to_string(child_root.join("main.go"))
        .unwrap()
        .rfind("Value()")
        .unwrap();
    let (root_pending, child_pending) = tokio::join!(
        root.call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":root_offset})
        ),
        child.call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":child_offset})
        )
    );
    assert_eq!(root_pending["state"], "pending", "{root_pending}");
    assert_eq!(child_pending["state"], "pending", "{child_pending}");
    let mut burst_pending = Vec::with_capacity(15);
    for index in 0..15 {
        let name = format!("burst-{index}.go");
        let offset = std::fs::read_to_string(fixture.root.join(&name))
            .unwrap()
            .rfind(&format!("Burst{index}()"))
            .unwrap();
        let queued = root
            .call(
                &fixture,
                "ide.context",
                json!({"path":name,"byte_offset":offset}),
            )
            .await;
        assert_eq!(queued["state"], "pending", "{queued}");
        burst_pending.push((index, queued));
    }
    let overflow_offset = std::fs::read_to_string(fixture.root.join("burst-overflow.go"))
        .unwrap()
        .rfind("BurstOverflow()")
        .unwrap();
    let refused = root
        .call(
            &fixture,
            "ide.context",
            json!({"path":"burst-overflow.go","byte_offset":overflow_offset}),
        )
        .await;
    assert_eq!(refused["state"], "error", "{refused}");
    assert_eq!(refused["code"], "capacity", "{refused}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !std::fs::read_to_string(&invocation_log)
            .ok()
            .is_some_and(|log| log.contains("-listen=unix;"))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("listener wrapper must record the gated cold start");
    std::fs::write(&gate, "release\n").unwrap();
    let root_context = root.settle(&fixture, root_pending).await;
    let child_context = child.settle(&fixture, child_pending).await;
    assert!(
        root_context["text"]
            .as_str()
            .unwrap()
            .contains("mode: semantic"),
        "{root_context}"
    );
    assert!(
        child_context["text"]
            .as_str()
            .unwrap()
            .contains("mode: semantic"),
        "{child_context}"
    );
    assert!(
        child_context["text"]
            .as_str()
            .unwrap()
            .contains("cold-child"),
        "{child_context}"
    );
    // Settle every accepted burst reply too: this is the full set of 17 admitted operations
    // (root + child + 15 burst), not an arbitrary early prefix of the invocation log.
    for (index, pending) in burst_pending {
        let settled = root.settle(&fixture, pending).await;
        assert!(
            settled["text"].as_str().unwrap().contains("mode: semantic"),
            "burst-{index}: {settled}"
        );
        assert!(
            settled["text"]
                .as_str()
                .unwrap()
                .contains(&format!("burst-{index}")),
            "burst-{index}: {settled}"
        );
    }
    let invocations = std::fs::read_to_string(&invocation_log).unwrap();
    let listeners = invocations
        .lines()
        .filter(|line| line.contains("-listen=unix;"))
        .collect::<Vec<_>>();
    let forwarders = invocations
        .lines()
        .filter(|line| line.contains("-remote=unix;"))
        .collect::<Vec<_>>();
    assert_eq!(listeners.len(), 1, "{invocations}");
    assert_eq!(forwarders.len(), 17, "{invocations}");
    let root_forwarders = forwarders
        .iter()
        .filter(|line| line.contains(fixture.root.to_str().unwrap()))
        .count();
    let child_forwarders = forwarders
        .iter()
        .filter(|line| line.contains(child_root.to_str().unwrap()))
        .count();
    assert_eq!(root_forwarders, 16, "{invocations}");
    assert_eq!(child_forwarders, 1, "{invocations}");
    let listener_pid: libc::pid_t = listeners[0].split('\t').next().unwrap().parse().unwrap();
    let forwarder_pids: Vec<libc::pid_t> = forwarders
        .iter()
        .map(|line| line.split('\t').next().unwrap().parse().unwrap())
        .collect();
    let mut unique_pids: std::collections::BTreeSet<libc::pid_t> =
        forwarder_pids.iter().copied().collect();
    assert_eq!(unique_pids.len(), forwarder_pids.len(), "{invocations}");
    unique_pids.insert(listener_pid);
    assert_eq!(unique_pids.len(), forwarder_pids.len() + 1, "{invocations}");
    // SAFETY: signal zero only observes the wrapper-recorded listener PID and changes no process
    // state.
    assert_eq!(unsafe { libc::kill(listener_pid, 0) }, 0);
    // Every forwarder above is a one-shot process the product reaps immediately after its own
    // exchange settles (see `SharedGopls::open_view`/`cancel_and_reap`), so now that every reply
    // has settled none of the recorded PIDs should still be running.
    for pid in &forwarder_pids {
        // SAFETY: signal zero only observes a recorded PID and changes no process state.
        assert_ne!(
            unsafe { libc::kill(*pid, 0) },
            0,
            "settled forwarder pid {pid} is still alive: {invocations}"
        );
    }
    let stopped = root.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    // SAFETY: signal zero only observes the wrapper-recorded listener PID and changes no process
    // state.
    assert_eq!(unsafe { libc::kill(listener_pid, 0) }, 0);
    // Change the child's own source after root's actor has fully torn down, so a stale/cached
    // answer or a dead peer backend cannot coincidentally still satisfy this assertion.
    std::fs::write(
        child_root.join("main.go"),
        "package main\nfunc Value() string { return \"cold-child-poststop\" }\nfunc main() { _ = Value() }\n",
    )
    .unwrap();
    let child_offset_after_edit = std::fs::read_to_string(child_root.join("main.go"))
        .unwrap()
        .rfind("Value()")
        .unwrap();
    let live = child
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":child_offset_after_edit}),
        )
        .await;
    let live = child.settle(&fixture, live).await;
    assert!(
        live["text"].as_str().unwrap().contains("mode: semantic"),
        "{live}"
    );
    assert!(
        live["text"]
            .as_str()
            .unwrap()
            .contains("cold-child-poststop"),
        "{live}"
    );
    let invocations_after_stop = std::fs::read_to_string(&invocation_log).unwrap();
    let listeners_after_stop = invocations_after_stop
        .lines()
        .filter(|line| line.contains("-listen=unix;"))
        .collect::<Vec<_>>();
    let forwarders_after_stop = invocations_after_stop
        .lines()
        .filter(|line| line.contains("-remote=unix;"))
        .collect::<Vec<_>>();
    assert_eq!(listeners_after_stop.len(), 1, "{invocations_after_stop}");
    assert_eq!(forwarders_after_stop.len(), 18, "{invocations_after_stop}");
    let new_forwarders: Vec<&&str> = forwarders_after_stop
        .iter()
        .filter(|line| {
            let pid: libc::pid_t = line.split('\t').next().unwrap().parse().unwrap();
            !forwarder_pids.contains(&pid)
        })
        .collect();
    assert_eq!(new_forwarders.len(), 1, "{invocations_after_stop}");
    assert!(
        new_forwarders[0].contains(child_root.to_str().unwrap()),
        "{invocations_after_stop}"
    );
    assert_eq!(
        child.call(&fixture, "ide.stop", json!({})).await["kind"],
        "stop"
    );
    tokio::join!(root.mcp.close(), child.mcp.close());
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A real background Context job that finishes while its own caller only ever saw `Pending`
/// leaves a genuinely undelivered new fact: the first eligible native-edit hook must deliver it
/// once, and a second must not resurrect it. This drives the actual product Worker/dispatcher
/// entry path end to end (real daemon, real gopls, real hook binary) — `ide.inspect` is never
/// called for this detail, so nothing but the job's own completion and the hook can be the source
/// of delivery.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS and AGENT_IDE_GO environment"]
async fn configured_product_pending_context_job_completes_and_native_hook_delivers_its_feedback_once()
 {
    use std::os::unix::fs::PermissionsExt;
    let gopls = std::env::var("AGENT_IDE_GOPLS").unwrap();
    let go = std::env::var("AGENT_IDE_GO").unwrap();
    let fixture = ProductFixture::new(json!([]));
    // gopls cannot start until this test releases the gate, so the very first `ide.context` must
    // observe the job still queued (`Pending`) rather than racing a fast real provider.
    let gate = fixture.base.join("release-gopls-listener");
    let wrapper = fixture.base.join("gopls-gated-provider");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nwhile [ ! -f '{}' ]; do sleep 0.02; done\nexec '{}' \"$@\"\n",
            gate.display(),
            gopls.replace('\'', "'\\''")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let providers = json!([{
        "executable":accepted_program(wrapper.to_str().unwrap(),"golang.org/x/tools/gopls v0.23.0"),
        "settings":"gopls_defaults",
        "toolchain":go,
        "cargo":null,
        "cargo_version":null,
        "rustc":null,
        "rustc_version":null,
        "trust":"fixture-disabled",
        "cache_namespace":"fixture-pending-feedback-cache"
    }]);
    fixture.write_config(providers, None);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "pending-feedback-root").await;
    let start = actor
        .call(
            &fixture,
            "ide.start",
            json!({"activation_id":"pending-start"}),
        )
        .await;
    let start = actor.settle(&fixture, start).await;
    assert_eq!(start["kind"], "activation", "{start}");

    std::fs::write(
        fixture.root.join("main.go"),
        "package main\nfunc Value() int { return \"bad\" }\nfunc main() { _ = Value() }\n",
    )
    .unwrap();
    let offset = std::fs::read_to_string(fixture.root.join("main.go"))
        .unwrap()
        .find("Value")
        .unwrap();
    let response = actor
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":offset}),
        )
        .await;
    assert_eq!(
        response["state"], "pending",
        "gopls is gated and must not have answered synchronously: {response}"
    );

    // Release the gate: the job now finishes for real inside the daemon's own worker loop. This
    // caller never calls `ide.inspect` for it — the completed reply is retained but unretrieved.
    std::fs::write(&gate, b"go").unwrap();
    tokio::time::sleep(Duration::from_secs(20)).await;

    actor
        .lifecycle(&fixture, "PreToolUse", "native-edit-1")
        .await;
    let first = actor
        .lifecycle_output(&fixture, "PostToolUse", "native-edit-1")
        .await;
    assert!(first.status.success() && first.stderr.is_empty());
    assert!(
        !first.stdout.is_empty(),
        "a completed-but-unretrieved fact must deliver on the first eligible hook: got empty stdout"
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    let additional = first["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(additional.contains("Provider reported"), "{additional}");

    actor
        .lifecycle(&fixture, "PreToolUse", "native-edit-2")
        .await;
    let second = actor
        .lifecycle_output(&fixture, "PostToolUse", "native-edit-2")
        .await;
    assert!(second.status.success() && second.stderr.is_empty());
    assert!(
        second.stdout.is_empty(),
        "the already-delivered fact must not resurrect on a second hook: {:?}",
        String::from_utf8_lossy(&second.stdout)
    );

    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Wrong executable bytes prevent worker readiness and durable boot side effects.
#[tokio::test]
async fn configured_product_rejects_changed_executable_before_opening_workspace() {
    let fixture = ProductFixture::new(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["targets"][0]["git"]["blake3"] = json!("0".repeat(64));
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["daemon", "--runtime-dir"])
            .arg(&fixture.runtime)
            .env("AGENT_IDE_LAUNCHER_CONFIG", &fixture.config)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(!fixture.runtime.join("state.sqlite").exists());
    assert!(!fixture.runtime.join("agent-ide.sock").exists());
}

/// Returns one newline- or semicolon-delimited `key: value` field from a Diff page's bounded text.
fn page_field(text: &str, key: &str) -> String {
    text.split(['\n', ';'])
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("missing {key} in page text:\n{text}"))
        .to_owned()
}

/// Builds escape-heavy content whose JSON-serialized form is far larger than its raw byte length,
/// so hunk selection that only counts raw bytes cannot predict whether a page fits the envelope.
fn escape_heavy(marker: &str, lines: usize) -> String {
    (0..lines)
        .map(|line| format!("{marker} {} {line:04}\n", "\\\"".repeat(12)))
        .collect()
}

/// Builds plain-ASCII content with no JSON escaping overhead, so its serialized size stays close
/// to its raw byte length even when the raw size alone exceeds a prior, since-removed half-budget.
fn plain_ascii(marker: &str, lines: usize) -> String {
    (0..lines)
        .map(|line| format!("{marker} plain unescaped content line {line:04}\n"))
        .collect()
}

/// Verifies whole-hunk pagination, serialized bounds, deferred delivery, and snapshot freshness.
///
/// Gates the fixed Git snapshot behind a fixture marker so the first `ide.diff` is provably
/// `pending` before any evidence exists, then proves that every whole hunk is delivered exactly
/// once across pages (never cut, duplicated or permanently skipped by a shrinking budget), that
/// each page carries valid typed provenance and truthful `Unknown` delivery freshness, and that an
/// out-of-band edit with no native hook can never yield a retained page presented as current.
#[tokio::test]
async fn diff_pagination_delivers_every_whole_hunk_once_with_truthful_freshness() {
    let fixture = ProductFixture::new(json!([]));
    // Twelve escape-heavy hunks exceed the serialized envelope together but not the captured raw
    // byte budget; one much larger hunk exceeds every halved budget the previous implementation
    // would have tried, and one invalid-UTF-8 file forces the non-text rendering path.
    for index in 0..12 {
        std::fs::write(
            fixture.root.join(format!("many-{index:02}.txt")),
            format!("base-{index:02}\n"),
        )
        .unwrap();
    }
    std::fs::write(fixture.root.join("many-big.txt"), "base-big\n").unwrap();
    std::fs::write(fixture.root.join("many-raw.bin"), "base-raw\n").unwrap();
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "many"]);
    for index in 0..12 {
        std::fs::write(
            fixture.root.join(format!("many-{index:02}.txt")),
            escape_heavy(&format!("hunkmark-{index:02}"), 128),
        )
        .unwrap();
    }
    // Plain ASCII, not escape-heavy: its raw byte length alone exceeds the prior 24 KiB
    // half-budget a since-removed shrinking-byte-ceiling approach used to test against, but with
    // negligible JSON escaping overhead it still fits the final duplicated MCP envelope as its own
    // page, unlike an escape-heavy hunk of the same raw size would.
    std::fs::write(
        fixture.root.join("many-big.txt"),
        plain_ascii("hunkmark-big", 600),
    )
    .unwrap();
    std::fs::write(fixture.root.join("many-raw.bin"), [0xff_u8; 64]).unwrap();

    // A gate the fixed snapshot Git must pass through, so the first diff is deterministically
    // pending until this test releases it.
    let gate = fixture.base.join("snapshot-gate");
    let proxy = fixture.base.join("gated-git.sh");
    std::fs::write(
        &proxy,
        format!(
            "#!/bin/sh\nwhile [ -e {} ]; do sleep 0.05; done\nexec /usr/bin/git \"$@\"\n",
            gate.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&proxy, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["targets"][0]["git"] = accepted_program(proxy.to_str().unwrap(), "fixture-git");
    std::fs::write(&fixture.config, config.to_string()).unwrap();

    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "product-root").await;
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    // Close the gate only after activation, so the snapshot Git of the diff below blocks and its
    // first reply is forced to be pending rather than an already composed page.
    std::fs::write(&gate, b"closed").unwrap();
    let first_call = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    assert_eq!(first_call["state"], "pending", "{first_call}");
    let reference = first_call["detail_ref"].as_str().unwrap().to_owned();
    let still_pending = actor
        .call(&fixture, "ide.inspect", json!({"detail_ref":&reference}))
        .await;
    assert_eq!(still_pending["state"], "pending", "{still_pending}");
    std::fs::remove_file(&gate).unwrap();

    let page1 = actor.settle(&fixture, first_call).await;
    assert_eq!(page1["kind"], "diff", "{page1}");
    let page1_text = page1["text"].as_str().unwrap().to_owned();
    assert!(page1_text.contains("hunkmark-00"), "{page1_text}");
    assert!(!page1_text.contains("hunkmark-big"), "{page1_text}");
    assert_eq!(page_field(&page1_text, "more_available"), "true");
    assert_eq!(page1["detail_ref"].as_str().unwrap(), reference);

    // Delivery never claims currentness; the capture-time freshness is preserved separately.
    assert_eq!(page_field(&page1_text, "freshness"), "Unknown");
    assert!(
        !page_field(&page1_text, "captured_freshness").is_empty(),
        "{page1_text}"
    );
    // Typed provenance is present and non-placeholder on the delivered page.
    assert!(
        !page_field(&page1_text, "worktree_id").is_empty(),
        "{page1_text}"
    );
    assert_ne!(page_field(&page1_text, "worktree_incarnation"), "0");
    assert_ne!(page_field(&page1_text, "operation_reference"), "none");
    assert_ne!(page_field(&page1_text, "capture_generation"), "none");
    assert_eq!(page_field(&page1_text, "comparison_left").len() % 2, 0);
    assert!(
        !page_field(&page1_text, "comparison_right").is_empty(),
        "{page1_text}"
    );

    let mut pages = vec![page1_text.clone()];
    while page_field(pages.last().unwrap(), "more_available") == "true" {
        assert!(pages.len() < 12, "pagination did not terminate");
        let next = actor
            .call(&fixture, "ide.inspect", json!({"detail_ref":&reference}))
            .await;
        assert_eq!(next["kind"], "diff", "{next}");
        let text = next["text"].as_str().unwrap().to_owned();
        assert_eq!(page_field(&text, "freshness"), "Unknown", "{text}");
        assert_ne!(page_field(&text, "capture_generation"), "none", "{text}");
        assert!(!pages.contains(&text), "page repeated verbatim:\n{text}");
        pages.push(text);
    }
    // Every whole hunk is delivered exactly once: no page cut one short, repeated one, or advanced
    // past one that fit the originally captured byte ceiling.
    for marker in (0..12)
        .map(|index| format!("hunkmark-{index:02}"))
        .chain(["hunkmark-big".to_owned()])
    {
        let carrying = pages.iter().filter(|text| text.contains(&marker)).count();
        assert_eq!(carrying, 1, "{marker} appeared on {carrying} pages");
    }

    // A retained page is an immutable capture: an out-of-band edit with no native hook must make a
    // later retrieval fail closed instead of delivering evidence presented as current.
    let reopened = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let reopened = actor.settle(&fixture, reopened).await;
    assert_eq!(reopened["kind"], "diff", "{reopened}");
    let reopened_ref = reopened["detail_ref"].as_str().unwrap().to_owned();
    assert_eq!(
        page_field(reopened["text"].as_str().unwrap(), "freshness"),
        "Unknown"
    );
    std::fs::write(
        fixture.root.join("many-00.txt"),
        escape_heavy("hunkmark-00-edited", 8),
    )
    .unwrap();
    let after_edit = actor
        .call(&fixture, "ide.inspect", json!({"detail_ref":&reopened_ref}))
        .await;
    assert_eq!(after_edit["state"], "error", "{after_edit}");
    assert_eq!(after_edit["code"], "source_unavailable", "{after_edit}");
    let after_edit_retry = actor
        .call(&fixture, "ide.inspect", json!({"detail_ref":&reopened_ref}))
        .await;
    assert_eq!(after_edit_retry["kind"], "diff", "{after_edit_retry}");
    assert_eq!(
        after_edit_retry["continuation"], false,
        "{after_edit_retry}"
    );

    // The production snapshot runner correlates each captured path with its durable observation:
    // a registered path edited without any reconciliation must fail the capture rather than being
    // silently captured as if the recorded revision still described it.
    let observed = actor
        .call(&fixture, "ide.context", json!({"path":"tracked.txt"}))
        .await;
    let observed = actor.settle(&fixture, observed).await;
    assert_eq!(observed["kind"], "context", "{observed}");
    std::fs::write(fixture.root.join("tracked.txt"), "unreconciled\n").unwrap();
    let mismatched = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let mismatched = actor.settle(&fixture, mismatched).await;
    assert_eq!(mismatched["state"], "error", "{mismatched}");
    assert_eq!(mismatched["code"], "source_unavailable", "{mismatched}");

    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A single hunk can be small enough in raw bytes to be selected by `select_hunks` (well under the
/// captured byte ceiling) yet still too large, once escaping and the duplicated MCP envelope are
/// accounted for, to ever fit a page by itself. Proves this reports an explicit `capacity` failure
/// — never a truncated hunk delivered as complete, and never state corrupted so a retry regresses
/// to something other than the same explicit failure.
#[tokio::test]
async fn diff_oversized_single_hunk_reports_capacity_without_false_continuation() {
    let fixture = ProductFixture::new(json!([]));
    std::fs::write(fixture.root.join("huge.txt"), "base\n").unwrap();
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "huge"]);
    // Raw patch bytes stay well under the 48 KiB captured byte ceiling, so this hunk is selected
    // rather than permanently skipped by `select_hunks`; its escape-heavy JSON form is what makes
    // the actual duplicated MCP envelope impossible to fit.
    std::fs::write(
        fixture.root.join("huge.txt"),
        escape_heavy("hunkmark-huge", 800),
    )
    .unwrap();

    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "product-root").await;
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let first = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let first = actor.settle(&fixture, first).await;
    assert_eq!(first["state"], "error", "{first}");
    assert_eq!(first["code"], "capacity", "{first}");

    // No continuation was ever retained for this failed capture, so a retry must reach the exact
    // same explicit failure rather than a stale or corrupted detail reference.
    let retry = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let retry = actor.settle(&fixture, retry).await;
    assert_eq!(retry["state"], "error", "{retry}");
    assert_eq!(retry["code"], "capacity", "{retry}");

    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// The shipping Codex route handles a large clean tree and charges only changed paths to the cap.
#[tokio::test]
async fn codex_diff_large_repository_is_empty_then_changed_then_explicitly_capped() {
    let fixture = ProductFixture::new(json!([]));
    for index in 0..600 {
        std::fs::write(fixture.root.join(format!("bulk-{index:03}.txt")), "base\n").unwrap();
    }
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "large baseline"]);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "product-root").await;
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    assert_eq!(actor.settle(&fixture, started).await["kind"], "activation");
    let clean = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let clean = actor.settle(&fixture, clean).await;
    assert_eq!(clean["kind"], "diff", "{clean}");
    assert!(
        clean["text"]
            .as_str()
            .unwrap()
            .contains("tracked: 0; untracked: 0")
    );
    std::fs::write(fixture.root.join("bulk-123.txt"), "changed\n").unwrap();
    std::fs::write(fixture.root.join("new.txt"), "untracked\n").unwrap();
    let changed = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let changed = actor.settle(&fixture, changed).await;
    assert_eq!(changed["kind"], "diff", "{changed}");
    let text = changed["text"].as_str().unwrap();
    assert!(
        text.contains("bulk-123.txt") && text.contains("new.txt"),
        "{text}"
    );
    for index in 0..257 {
        std::fs::write(
            fixture.root.join(format!("bulk-{index:03}.txt")),
            "over cap\n",
        )
        .unwrap();
    }
    let capped = actor
        .call(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let capped = actor.settle(&fixture, capped).await;
    assert_eq!(capped["code"], "capacity", "{capped}");
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Parses one page's `page N[ (last)]; bytes A-B of TOTAL[; status]` position marker (T16B).
fn page_marker(text: &str) -> (usize, bool, usize, usize, usize) {
    let line = text.lines().next().unwrap_or_default();
    let rest = line.strip_prefix("page ").unwrap_or_else(|| {
        panic!(
            "page must start with a position marker:\n{}",
            &text[..text.len().min(200)]
        )
    });
    let (head, tail) = rest.split_once("; bytes ").expect("marker bytes");
    let last = head.ends_with(" (last)");
    let number = head
        .trim_end_matches(" (last)")
        .parse()
        .expect("page number");
    let (range, total) = tail.split_once(" of ").expect("marker total");
    let total = total.split(';').next().unwrap();
    let (from, to) = range.split_once('-').expect("marker range");
    (
        number,
        last,
        from.parse().unwrap(),
        to.parse().unwrap(),
        total.parse().unwrap(),
    )
}

/// A real Claude foreground helper reads a source file far over the former 64 KiB render cap in
/// one capture, and the daemon pages the whole composed text across repeated `ide.inspect` calls:
/// the first continuation call serves page two (not page one again), every page starts with a
/// contiguous position marker, the pages join to the exact file bytes, the last page states
/// completion, re-inspecting after it re-serves that last page, and `ide.edit` is refused on the
/// source reference until every page was delivered (T13B, T16B).
#[tokio::test]
async fn claude_context_pagination_delivers_the_whole_source_across_repeated_inspect() {
    let fixture = ProductFixture::new_claude(json!([]));
    // ~150 KB of short lines with a multibyte character: over the former 64 KiB cap, over the
    // 114000-byte live report, and well under the 1 MiB read ceiling.
    let content: String = (0..3300)
        .map(|line| format!("value_{line:05} = \"caf\u{e9} {line:05} padding padding padding\"\n"))
        .collect();
    assert!(content.len() > 140_000, "{}", content.len());
    std::fs::write(fixture.root.join("claude-large.py"), &content).unwrap();
    fixture.git(&["add", "--", "claude-large.py"]);
    fixture.git(&["commit", "--quiet", "-m", "claude large source"]);

    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "claude-large-context").await;
    let started = actor
        .call_claude(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let (started, _) = actor.complete_claude_pending(&fixture, &started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let first_call = actor
        .call_claude(&fixture, "ide.context", json!({"path":"claude-large.py"}))
        .await;
    let (page1, _) = actor.complete_claude_pending(&fixture, &first_call).await;
    assert_eq!(page1["kind"], "context", "{page1}");
    assert_eq!(page1["truncated"], true, "{page1}");
    assert_eq!(page1["continuation"], true, "{page1}");
    let reference = page1["detail_ref"]
        .as_str()
        .expect("a multi-page Context result must carry a detail_ref")
        .to_owned();
    let page1_text = page1["text"].as_str().unwrap().to_owned();
    assert!(
        page1_text.contains("coverage: complete helper-observed path"),
        "{page1_text}"
    );
    let (number, last, from, mut end, total) = page_marker(&page1_text);
    assert_eq!((number, last, from, total), (1, false, 0, content.len()));
    let mut collected = page1_text
        .split_once("\n\n")
        .expect("header ends at the first blank line")
        .1
        .to_owned();

    let mut continuation = true;
    let mut expected_page = 1;
    let mut last_page = String::new();
    while continuation {
        expected_page += 1;
        assert!(expected_page < 20, "pagination did not terminate");
        let next = actor
            .call_claude(&fixture, "ide.inspect", json!({"detail_ref":&reference}))
            .await;
        assert_eq!(next["kind"], "context", "{next}");
        let text = next["text"].as_str().unwrap();
        let (number, last, from, to, total) = page_marker(text);
        assert_eq!(
            number, expected_page,
            "the first continuation call must serve page two, never page one again: {text}"
        );
        assert_eq!((from, total), (end, content.len()), "{text}");
        end = to;
        continuation = next["continuation"].as_bool().unwrap();
        assert_eq!(last, !continuation, "{text}");
        assert_eq!(next["truncated"], json!(continuation), "{next}");
        collected.push_str(text.split_once('\n').unwrap().1);
        if expected_page == 2 {
            // Two pages of a longer file are a partial view: an edit built on its source
            // reference could truncate the file, so it is refused before the last page.
            let edit = actor
                .call_claude(
                    &fixture,
                    "ide.edit",
                    json!({"operation_id":"partial-view","path":"claude-large.py",
                           "source_ref":&reference,"content":"x = 1\n"}),
                )
                .await;
            assert_eq!(edit["result"]["outcome"], "stale_source", "{edit}");
            assert_eq!(edit["result"]["source_ref"], Value::Null, "{edit}");
        }
        if !continuation {
            assert!(
                text.lines().next().unwrap().ends_with("; complete"),
                "{text}"
            );
            last_page = text.to_owned();
        }
    }
    assert!(expected_page >= 3, "the fixture must force several pages");
    assert_eq!(end, content.len(), "the last page must reach the end");
    assert_eq!(
        collected, content,
        "pages must join to the exact file bytes"
    );

    // Terminal behaviour: a further inspect re-serves the same last page, byte-identical.
    let again = actor
        .call_claude(&fixture, "ide.inspect", json!({"detail_ref":&reference}))
        .await;
    assert_eq!(again["text"].as_str().unwrap(), last_page, "{again}");
    assert_eq!(again["continuation"], false, "{again}");

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Asserts every hunk of one Diff page has a `file:` line before it on the same page, so a hunk
/// on a continuation page is attributable without the previous page (T16B).
fn assert_hunks_attributed(page: &str) {
    let mut file = None;
    for line in page.lines() {
        if line.starts_with("file: ") {
            file = Some(line);
        } else if line.starts_with("@@") {
            assert!(
                file.is_some(),
                "hunk without a file line on its page:\n{page}"
            );
        }
    }
}

/// A real Claude foreground helper captures a diff too large for one MCP reply in the single
/// helper round trip, and the daemon pages the composed text across repeated `ide.inspect` calls
/// the same way it already pages a large Context result, until every hunk has been delivered
/// (T13B).
#[tokio::test]
async fn claude_diff_pagination_delivers_every_hunk_across_repeated_inspect() {
    let fixture = ProductFixture::new_claude(json!([]));
    for index in 0..12 {
        std::fs::write(
            fixture.root.join(format!("claude-many-{index:02}.txt")),
            format!("base-{index:02}\n"),
        )
        .unwrap();
    }
    std::fs::write(fixture.root.join("claude-many-big.txt"), "base-big\n").unwrap();
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "claude many"]);
    for index in 0..12 {
        std::fs::write(
            fixture.root.join(format!("claude-many-{index:02}.txt")),
            escape_heavy(&format!("claude-hunkmark-{index:02}"), 128),
        )
        .unwrap();
    }
    std::fs::write(
        fixture.root.join("claude-many-big.txt"),
        plain_ascii("claude-hunkmark-big", 600),
    )
    .unwrap();

    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "claude-pagination").await;
    let started = actor
        .call_claude(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let (started, _) = actor.complete_claude_pending(&fixture, &started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let first_call = actor
        .call_claude(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let (page1, _) = actor.complete_claude_pending(&fixture, &first_call).await;
    assert_eq!(page1["kind"], "diff", "{page1}");
    let page1_text = page1["text"].as_str().unwrap().to_owned();
    let reference = page1["detail_ref"]
        .as_str()
        .expect("a multi-page Diff must carry a detail_ref")
        .to_owned();
    assert_eq!(page1["truncated"], true, "{page1}");
    assert_eq!(page1["continuation"], true, "{page1}");
    // No hunk ever overflowed this single capture: every marker is somewhere in the composed
    // evidence, so nothing here is a hard ceiling failure, only a reply too large for one page.
    assert_eq!(
        page_field(&page1_text, "omitted_hunks"),
        "0",
        "{page1_text}"
    );
    assert_eq!(
        page_field(&page1_text, "omitted_bytes"),
        "0",
        "{page1_text}"
    );

    // The header must agree with the paging the reply itself announces (T16B).
    assert_eq!(
        page_field(&page1_text, "more_available"),
        "true",
        "{page1_text}"
    );
    assert_eq!(page_marker(&page1_text).0, 1, "{page1_text}");
    assert_hunks_attributed(&page1_text);

    let mut collected = page1_text;
    let mut continuation = page1["continuation"].as_bool().unwrap();
    let mut pages = 1;
    while continuation {
        pages += 1;
        assert!(pages < 12, "pagination did not terminate");
        let next = actor
            .call_claude(&fixture, "ide.inspect", json!({"detail_ref":&reference}))
            .await;
        assert_eq!(next["kind"], "diff", "{next}");
        let next_text = next["text"].as_str().unwrap();
        assert_eq!(
            page_marker(next_text).0,
            pages,
            "the first continuation call must serve page two: {next_text}"
        );
        assert_hunks_attributed(next_text);
        continuation = next["continuation"].as_bool().unwrap();
        // A further page names the exact detail_ref to inspect next; the terminal page carries
        // nothing left to fetch, so the compact Claude text omits it by design (T14B).
        if continuation {
            assert_eq!(
                next["detail_ref"].as_str().unwrap(),
                reference,
                "a page with more to fetch must echo the same detail_ref"
            );
        }
        assert_eq!(
            next["truncated"],
            json!(continuation),
            "truncated must agree with continuation on every page: {next}"
        );
        collected.push_str(next["text"].as_str().unwrap());
    }
    assert!(pages >= 2, "the fixture diff must force multiple pages");
    for marker in (0..12)
        .map(|index| format!("claude-hunkmark-{index:02}"))
        .chain(["claude-hunkmark-big".to_owned()])
    {
        assert!(
            collected.contains(&marker),
            "{marker} missing from the concatenated pages"
        );
    }

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Drives real foreground Claude helper Start, Diff, Context and Edit launches end to end: each
/// helper instruction is armed by a native Bash pre-hook, settles through its matching post-hook,
/// and publishes only through `ide.inspect`, including an escape-heavy Diff that must fit whole;
/// Stop also consumes an already-ready Edit result without losing its known replacement outcome.
///
/// The accepted `claude_profile` here is the fixture's test-only disabled launcher wiring proof,
/// not a real host sandbox measurement: it only proves the daemon→hook→helper→daemon correlation
/// and admission plumbing settle correctly, never that a live Claude host actually contained the
/// helper.
#[tokio::test]
async fn configured_product_claude_helper_activates_and_conflicts_a_second_actor_then_stops() {
    /// Runs one complete Claude helper round trip: mint, arm, real process, post, and inspect.
    async fn claude_operation(
        actor: &mut ProductActor,
        fixture: &ProductFixture,
        name: &str,
        arguments: Value,
    ) -> Value {
        let pending = actor.call_claude(fixture, name, arguments).await;
        assert_eq!(pending["state"], "pending", "{pending}");
        let detail_ref = pending["detail_ref"].as_str().unwrap().to_owned();
        let helper = pending["helper"].as_str().unwrap().to_owned();
        assert!(helper.contains("claude-worker"), "{helper}");

        // Inspecting a ticket whose helper never ran repeats the exact command instead of a bare
        // pending line the model could poll until the ticket expires.
        let early = actor
            .call_claude(fixture, "ide.inspect", json!({"detail_ref": &detail_ref}))
            .await;
        assert_eq!(early["state"], "pending", "{early}");
        assert_eq!(early["helper"], helper.as_str(), "{early}");

        // The ordinary Bash pre-hook recognizes the exact expected command; this is silent by
        // construction and performs no admission decision itself. Each invocation needs its own
        // unique tool-call id: a repeated helper launch under the same id would let the post-hook
        // settle a still-open earlier ticket instead of this one.
        let launch_call = format!("bash-launch-{detail_ref}");
        let launch_call = launch_call.as_str();
        let mut arm = claude_hook_process(&fixture.runtime, Some(actor.attachment));
        arm.stdin
            .take()
            .unwrap()
            .write_all(
                json!({"hook_event_name":"PreToolUse","session_id":actor.actor,
                    "tool_use_id":launch_call,"tool_name":"Bash",
                    "tool_input":{"command":helper}})
                .to_string()
                .as_bytes(),
            )
            .await
            .unwrap();
        let armed = tokio::time::timeout(Duration::from_secs(2), arm.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(armed.status.success() && armed.stdout.is_empty() && armed.stderr.is_empty());

        // Runs the fixed helper command as a real foreground process, exactly as a native Bash
        // tool call would; it claims the ticket once over the private socket and performs real
        // Git discovery against the fixture's worktree.
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("/bin/sh").arg("-c").arg(&helper).output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("claude-worker"),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );

        // The matching Bash post-hook settles the claimed ticket.
        let mut post = claude_hook_process(&fixture.runtime, Some(actor.attachment));
        post.stdin
            .take()
            .unwrap()
            .write_all(
                json!({"hook_event_name":"PostToolUse","session_id":actor.actor,
                    "tool_use_id":launch_call,"tool_response":{"success":true}})
                .to_string()
                .as_bytes(),
            )
            .await
            .unwrap();
        let settled = tokio::time::timeout(Duration::from_secs(2), post.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(settled.status.success() && settled.stdout.is_empty() && settled.stderr.is_empty());

        actor
            .call_claude(fixture, "ide.inspect", json!({"detail_ref":detail_ref}))
            .await
    }

    let fixture = ProductFixture::new_claude(json!([]));
    fixture.git(&["add", "--", "."]);
    fixture.git(&["commit", "--quiet", "-m", "clean claude fixture"]);
    std::fs::write(fixture.root.join("claude-heavy.txt"), "base\n").unwrap();
    fixture.git(&["add", "--", "claude-heavy.txt"]);
    fixture.git(&["commit", "--quiet", "-m", "claude heavy base"]);
    std::fs::write(
        fixture.root.join("claude-heavy.txt"),
        escape_heavy("claude-escape", 400),
    )
    .unwrap();
    let mut daemon = fixture.daemon().await;
    let mut first = ProductActor::new(&fixture, "claude-first").await;

    let started = claude_operation(
        &mut first,
        &fixture,
        "ide.start",
        json!({"activation_id":"start"}),
    )
    .await;
    assert_eq!(started["kind"], "activation", "{started}");
    let database = rusqlite::Connection::open(fixture.runtime.join("state.sqlite")).unwrap();
    let (operation, actor, outcome, active): (String, String, String, bool) = database
        .query_row(
            "SELECT operation, actor, outcome, active FROM workspace_starts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(actor, "claude-first");
    assert_eq!(outcome, "granted");
    assert!(active);

    // Re-running the exact same activation end to end is idempotent: the already-durable
    // Workspace publishes the identical retained evidence rather than a second row.
    let reread = claude_operation(
        &mut first,
        &fixture,
        "ide.start",
        json!({"activation_id":"start"}),
    )
    .await;
    assert_eq!(reread, started, "{reread}");
    assert_eq!(
        database
            .query_row("SELECT COUNT(*) FROM workspace_starts", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1,
        "replaying the activation must not persist a second receipt"
    );

    // A second actor targeting the same worktree is refused with Conflict while the first
    // actor's own activation stays usable.
    let mut second = ProductActor::new_at(
        &fixture,
        "claude-second",
        "private-host-channel",
        "session_id",
        fixture.state(),
    )
    .await;
    let conflicted = claude_operation(
        &mut second,
        &fixture,
        "ide.start",
        json!({"activation_id":"start"}),
    )
    .await;
    assert_eq!(conflicted["state"], "error", "{conflicted}");
    assert_eq!(conflicted["code"], "conflict", "{conflicted}");
    assert!(
        database
            .query_row(
                "SELECT active FROM workspace_starts WHERE operation=?1",
                [&operation],
                |row| row.get::<_, bool>(0),
            )
            .unwrap(),
        "the refused actor must not replace the first durable owner"
    );

    let still_usable = claude_operation(
        &mut first,
        &fixture,
        "ide.start",
        json!({"activation_id":"start"}),
    )
    .await;
    assert_eq!(still_usable, started, "{still_usable}");

    // This hunk is below Claude's raw selection cap but near the duplicated escaped MCP envelope.
    // Shared fitting may omit later whole hunks, but it must neither slice this hunk nor advertise
    // a cursor the helper cannot retain after its ticket is consumed.
    let diff = claude_operation(&mut first, &fixture, "ide.diff", json!({"mode":"head"})).await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    assert_eq!(diff["continuation"], false, "{diff}");
    let diff_text = diff["text"].as_str().unwrap();
    assert_eq!(diff_text.matches("claude-escape").count(), 400, "{diff}");

    let context = claude_operation(
        &mut first,
        &fixture,
        "ide.context",
        json!({"path":"tracked.txt"}),
    )
    .await;
    assert_eq!(context["kind"], "context", "{context}");
    let source_ref = context["detail_ref"].as_str().unwrap().to_owned();
    let edited = claude_operation(
        &mut first,
        &fixture,
        "ide.edit",
        json!({
            "operation_id":"claude-edit-1",
            "path":"tracked.txt",
            "source_ref":source_ref,
            "content":"claude-helper-edit\n"
        }),
    )
    .await;
    assert_eq!(edited["state"], "edit", "{edited}");
    assert_eq!(edited["result"]["outcome"], "replaced", "{edited}");
    let edited_source_ref = edited["result"]["source_ref"]
        .as_str()
        .expect("a successful Claude Edit must retain its post-read detail")
        .to_owned();
    assert_eq!(
        std::fs::read(fixture.root.join("tracked.txt")).unwrap(),
        b"claude-helper-edit\n"
    );
    // The returned Edit source reference is the already-reserved prepare detail, even while
    // ordinary detail capacity is saturated; a second real Claude helper can consume it directly.
    let chained = claude_operation(
        &mut first,
        &fixture,
        "ide.edit",
        json!({
            "operation_id":"claude-edit-2",
            "path":"tracked.txt",
            "source_ref":edited_source_ref,
            "content":"claude-helper-chain\n"
        }),
    )
    .await;
    assert_eq!(chained["result"]["outcome"], "replaced", "{chained}");
    assert_eq!(
        std::fs::read(fixture.root.join("tracked.txt")).unwrap(),
        b"claude-helper-chain\n"
    );
    std::fs::write(fixture.root.join("tracked.txt"), "claude-native-fallback\n").unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("tracked.txt")).unwrap(),
        b"claude-native-fallback\n"
    );

    let stop_context = claude_operation(
        &mut first,
        &fixture,
        "ide.context",
        json!({"path":"tracked.txt"}),
    )
    .await;
    let stop_source_ref = stop_context["detail_ref"].as_str().unwrap().to_owned();
    let ready = first
        .call_claude(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"claude-stop-ready",
                "path":"tracked.txt",
                "source_ref":stop_source_ref,
                "content":"claude-stop-ready\n"
            }),
        )
        .await;
    first.launch_claude_pending(&fixture, &ready).await;
    assert_eq!(
        std::fs::read(fixture.root.join("tracked.txt")).unwrap(),
        b"claude-stop-ready\n"
    );

    let stopped = first.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    let (edit_state, edit_outcome): (String, String) = database
        .query_row(
            "SELECT state,outcome FROM changes_edit_receipts WHERE operation_id='claude-stop-ready'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (edit_state.as_str(), edit_outcome.as_str()),
        ("settled", "replaced")
    );
    assert!(
        !database
            .query_row(
                "SELECT active FROM workspace_starts WHERE operation=?1",
                [&operation],
                |row| row.get::<_, bool>(0),
            )
            .unwrap(),
        "Stop must durably revoke the original activation"
    );
    first.next += 1;
    let stale_call = format!("call-{}", first.next);
    first
        .claude_lifecycle(&fixture, "PreToolUse", &stale_call)
        .await;
    // Stop already released the host binding itself, so any detail reference fails the same way
    // here, before a stale detail lookup is even reached.
    let stale_detail = first
        .mcp
        .exchange(json!({"jsonrpc":"2.0","id":first.next,"method":"tools/call","params":{
            "name":"ide.inspect","arguments":{"detail_ref":"post-stop-detail"},"_meta":{"claudecode/toolUseId":stale_call}}}))
        .await;
    first
        .claude_lifecycle(&fixture, "PostToolUse", &stale_call)
        .await;
    assert_ne!(
        stale_detail["result"]["isError"],
        json!(true),
        "{stale_detail}"
    );
    assert!(
        stale_detail["result"].get("structuredContent").is_none(),
        "{stale_detail}"
    );
    assert!(
        stale_detail["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("host_binding"),
        "{stale_detail}"
    );

    first.mcp.close().await;
    second.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Keeps a bounded Claude worker usable across repeated helper Diff finalization and repeated
/// failures.
///
/// One retained activation occupies the first slot. A second actor's settled-but-conflicting Start
/// is inspected repeatedly; each identical failure must retire its unusable worker detail. A
/// helper-composed Diff now retains a detail exactly like Context (T13B), so its capacity is sized
/// for the activation, each of the three Diffs, and the trailing source-producing Context: if a
/// completed Diff's detail were wrongly dropped or, conversely, never released, this sequence would
/// either under- or over-count against the bound and the fourth-through-sixth operation would fail.
/// A completed, non-continuation Diff's retained detail is never named in the Claude host's compact
/// text (T14B): unlike Context's `source_ref`, it has no later `ide.edit` use, so the real client
/// has no way to name it for an explicit re-inspection, and this test does not attempt one.
#[tokio::test]
async fn claude_diff_and_failed_reinspection_do_not_exhaust_detail_capacity() {
    let fixture = ProductFixture::new_claude(json!([]));
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["limits"]["details"] = json!(5);
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    let mut daemon = fixture.daemon().await;
    let mut first = ProductActor::new(&fixture, "claude-capacity-first").await;
    let started = first
        .call_claude(&fixture, "ide.start", json!({"activation_id":"first"}))
        .await;
    let (started, _) = first.complete_claude_pending(&fixture, &started).await;
    assert_eq!(started["kind"], "activation", "{started}");

    let mut second = ProductActor::new_at(
        &fixture,
        "claude-capacity-second",
        "private-host-channel",
        "session_id",
        fixture.state(),
    )
    .await;
    let conflicting = second
        .call_claude(&fixture, "ide.start", json!({"activation_id":"second"}))
        .await;
    let detail_ref = second.launch_claude_pending(&fixture, &conflicting).await;
    for _ in 0..3 {
        let conflict = second
            .call_claude(&fixture, "ide.inspect", json!({"detail_ref":detail_ref}))
            .await;
        assert_eq!(conflict["code"], "conflict", "{conflict}");
    }

    for _ in 0..3 {
        let pending = first
            .call_claude(&fixture, "ide.diff", json!({"mode":"head"}))
            .await;
        let (diff, _) = first.complete_claude_pending(&fixture, &pending).await;
        assert_eq!(diff["kind"], "diff", "{diff}");
        assert_eq!(diff["continuation"], false, "{diff}");
    }

    let context = first
        .call_claude(&fixture, "ide.context", json!({"path":"tracked.txt"}))
        .await;
    let (context, _) = first.complete_claude_pending(&fixture, &context).await;
    assert_eq!(context["kind"], "context", "{context}");
    assert!(context["detail_ref"].is_string(), "{context}");
    let stopped = first.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    first.mcp.close().await;
    second.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Proves Claude Context and Diff execute in the real foreground helper, that a diagnostic
/// already delivered inline inside a retrieved Context reply is never echoed a second time on the
/// next ordinary native-edit hook. Cross-production identity replacement is covered by the
/// worker's bounded ledger regression; this test owns the real Claude helper and host surfaces.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS and AGENT_IDE_GO environment"]
async fn configured_product_claude_helper_returns_context_diff_and_feedback() {
    let gopls = std::env::var("AGENT_IDE_GOPLS").unwrap();
    let go = std::env::var("AGENT_IDE_GO").unwrap();
    let rust_analyzer = std::env::var("AGENT_IDE_RUST_ANALYZER").unwrap();
    let toolchain = std::env::var("AGENT_IDE_RUST_TOOLCHAIN").unwrap();
    let providers = json!([
        {
            "executable":accepted_program(&gopls,"golang.org/x/tools/gopls v0.23.0"),
            "settings":"gopls_defaults",
            "toolchain":go,
            "cargo":null,
            "cargo_version":null,
            "rustc":null,
            "rustc_version":null,
            "trust":"fixture-disabled",
            "cache_namespace":"fixture-claude-go-cache"
        },
        {
            "executable":accepted_program(&rust_analyzer,"1.98.1 (48a229ce 2026-09-01)"),
            "settings":"rust_cache_priming_disabled_v1",
            "toolchain":toolchain,
            "cargo":accepted_program(&toolchain_bin("cargo"),"cargo 1.98.1"),
            "cargo_version":"cargo 1.98.1",
            "rustc":accepted_program(&toolchain_bin("rustc"),"rustc 1.98.1"),
            "rustc_version":"rustc 1.98.1",
            "trust":"fixture-disabled",
            "cache_namespace":"fixture-claude-rust-cache"
        }
    ]);
    let fixture = ProductFixture::new_claude(providers);
    let mut daemon = fixture.daemon().await;
    let mut actor = ProductActor::new(&fixture, "claude-context").await;

    let pending = actor
        .call_claude(&fixture, "ide.start", json!({"activation_id":"start"}))
        .await;
    let (started, feedback) = actor.complete_claude_pending(&fixture, &pending).await;
    assert_eq!(started["kind"], "activation", "{started}");
    assert!(
        started["text"]
            .as_str()
            .unwrap()
            .contains("durable capture true"),
        "{started}"
    );
    assert!(feedback.is_empty());

    std::fs::write(
        fixture.root.join("main.go"),
        "package main\nfunc Value() int { return \"bad\" }\nfunc main() { _ = Value() }\n",
    )
    .unwrap();
    let offset = std::fs::read_to_string(fixture.root.join("main.go"))
        .unwrap()
        .find("Value")
        .unwrap();
    let pending = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":offset}),
        )
        .await;
    let detail_ref = actor.launch_claude_pending(&fixture, &pending).await;
    let context = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":offset,"detail_ref":detail_ref}),
        )
        .await;
    assert_eq!(context["kind"], "context", "{context}");
    assert!(
        context["helper"].is_null(),
        "retrieval must not mint another helper: {context}"
    );
    let context_text = context["text"].as_str().unwrap();
    assert!(context_text.contains("mode: semantic"), "{context_text}");
    assert!(context_text.contains("return \"bad\""), "{context_text}");
    assert!(
        context_text.contains("diagnostic_count: 1"),
        "{context_text}"
    );
    // The diagnostic delta above was already handed to this same caller inline, inside the
    // retrieved `ide.context` reply. An ordinary native-edit post hook that follows must not echo
    // that already-submitted fact on the second channel: `claude_lifecycle` asserts empty stdout,
    // the same bar every other no-feedback hook in this file is held to.
    actor
        .claude_lifecycle(&fixture, "PreToolUse", "native-edit-after-context")
        .await;
    actor
        .claude_lifecycle(&fixture, "PostToolUse", "native-edit-after-context")
        .await;

    let rust_source = std::fs::read_to_string(fixture.root.join("src/lib.rs")).unwrap();
    let pending = actor
        .call_claude(
            &fixture,
            "ide.context",
            json!({"path":"src/lib.rs","byte_offset":rust_source.find("value").unwrap()}),
        )
        .await;
    let (rust_context, feedback) = actor.complete_claude_pending(&fixture, &pending).await;
    assert_eq!(rust_context["kind"], "context", "{rust_context}");
    let rust_text = rust_context["text"].as_str().unwrap();
    assert!(rust_text.contains("mode: semantic"), "{rust_text}");
    assert!(rust_text.contains("pub fn value()"), "{rust_text}");
    assert!(
        rust_text.contains("provider_generation: Some"),
        "{rust_text}"
    );
    assert!(feedback.is_empty());

    let pending = actor
        .call_claude(&fixture, "ide.diff", json!({"mode":"head"}))
        .await;
    let detail_ref = actor.launch_claude_pending(&fixture, &pending).await;
    let diff = actor
        .call_claude(
            &fixture,
            "ide.diff",
            json!({"mode":"head","detail_ref":detail_ref}),
        )
        .await;
    assert_eq!(diff["kind"], "diff", "{diff}");
    assert!(
        diff["helper"].is_null(),
        "retrieval must not mint another helper: {diff}"
    );
    let diff_text = diff["text"].as_str().unwrap();
    assert!(
        diff_text.contains("baseline_coverage: Some(Partial)"),
        "{diff_text}"
    );
    assert!(
        diff_text.contains("baseline_window: Some(Unverified)"),
        "{diff_text}"
    );
    assert!(
        diff_text.contains("tracked_path: \"main.go\""),
        "{diff_text}"
    );
    assert!(diff_text.contains("return \"bad\""), "{diff_text}");

    let mut retained = std::fs::read_dir(fixture.runtime.join("cache"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    retained.sort();
    assert_eq!(
        retained.len(),
        2,
        "one worktree namespace per one-shot provider"
    );
    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    let mut after_stop = std::fs::read_dir(fixture.runtime.join("cache"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    after_stop.sort();
    assert_eq!(
        after_stop, retained,
        "Stop retains compatible cache directories"
    );
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Enables project checks with a fake Rust toolchain whose `cargo` replays a fixed JSON stream.
///
/// The fake `cargo` runs under the real Seatbelt runner and reports one error per line of the
/// worktree's untracked `problems.count` value, so tests change counts without a real compiler.
/// `allowed_root` is the configured admission root. Returns the redirected daemon home (`AGENT_IDE_HOME`).
fn enable_fake_rust_checks(fixture: &ProductFixture, allowed_root: &Path) -> PathBuf {
    enable_fake_rust_checks_holding(fixture, allowed_root, "")
}

/// The gate file a gate-held fake `cargo` waits on, outside the repository worktree so its
/// creation never perturbs the scheduler's worktree-input fingerprint (T20B).
const CHECKS_GATE: &str = "checks-gate";

/// Hold command that blocks every fake check run at a barrier instead of assuming a wall-clock
/// window: the test releases [`release_checks_gate`] only after the activation replies have
/// settled, so the first result provably cannot be consumed by an earlier reply carrier.
fn hold_checks_at_gate(fixture: &ProductFixture) -> String {
    format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done",
        fixture.base.join(CHECKS_GATE).display()
    )
}

/// Releases the check barrier: every held fake `cargo` run completes and its result becomes due.
fn release_checks_gate(fixture: &ProductFixture) {
    std::fs::write(fixture.base.join(CHECKS_GATE), "go").unwrap();
}

/// Counts completed project-check telemetry rows in one daemon's own database; 0 when absent.
async fn completed_check_count(database: &Path) -> usize {
    let Ok(output) = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
        .args(["telemetry", "query", "--database"])
        .arg(database)
        .args(["--tag", "project_check_completed"])
        .output()
        .await
    else {
        return 0;
    };
    if !output.status.success() {
        return 0;
    }
    String::from_utf8_lossy(&output.stdout)
        .matches(r#""errors_bucket""#)
        .count()
}

/// Same, with a shell command prepended to the fake `cargo` so a test can hold the first check
/// in its running state long enough to observe a `checking (…)` plate (T22B).
fn enable_fake_rust_checks_holding(
    fixture: &ProductFixture,
    allowed_root: &Path,
    hold: &str,
) -> PathBuf {
    let home = fixture.base.join("home");
    let toolchain = home.join(".rustup/toolchains/fake");
    std::fs::create_dir_all(toolchain.join("bin")).unwrap();
    std::fs::create_dir_all(home.join(".cargo")).unwrap();
    let cargo = toolchain.join("bin/cargo");
    let mut script = String::new();
    if !hold.is_empty() {
        script.push_str(hold);
        script.push('\n');
    }
    script.push_str(
        r#"#!/bin/sh
n=$(/bin/cat problems.count 2>/dev/null || echo 0)
i=0
while [ "$i" -lt "$n" ]; do
  i=$((i+1))
  printf '{"reason":"compiler-message","package_id":"fixture","message":{"level":"error","message":"fake %s","code":null,"spans":[{"file_name":"src/lib.rs","is_primary":true,"line_start":%s,"column_start":1}]}}\n' "$i" "$i"
done
printf '{"reason":"compiler-artifact","package_id":"fixture"}\n'
printf '{"reason":"build-finished","success":true}\n'
"#,
    );
    std::fs::write(&cargo, script).unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&fixture.config).unwrap()).unwrap();
    config["allowed_roots"] = json!([allowed_root]);
    config["project_checks"] = json!({"debounce_ms":100,"rust":{"toolchain_dir":toolchain}});
    std::fs::write(&fixture.config, config.to_string()).unwrap();
    home
}

/// Activates a Claude actor through the real foreground helper.
///
/// Returns the actor and the `additionalContext` the activation inspect call's own post hook
/// delivered: the first due status plate arrives there now (T22B), empty when checks are off.
async fn eyes_claude_actor(
    fixture: &ProductFixture,
    actor: &'static str,
) -> (ProductActor, String) {
    let mut actor = ProductActor::new(fixture, actor).await;
    let started = actor
        .call_claude(fixture, "ide.start", json!({"activation_id":"eyes"}))
        .await;
    let (started, feedback) = actor.complete_claude_pending(fixture, &started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let context = if feedback.is_empty() {
        String::new()
    } else {
        let rendered: Value = serde_json::from_str(&String::from_utf8(feedback).unwrap()).unwrap();
        assert_eq!(
            rendered["hookSpecificOutput"]["hookEventName"], "PostToolUse",
            "{rendered}"
        );
        rendered["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    (actor, context)
}

/// Polls native `Read` post-hooks until one carries a model context, returning its text.
async fn await_eyes_block(actor: &mut ProductActor, fixture: &ProductFixture) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let output = actor.claude_native_post(fixture, "Read").await;
        if !output.is_empty() {
            let rendered: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(
                rendered["hookSpecificOutput"]["hookEventName"], "PostToolUse",
                "{rendered}"
            );
            return rendered["hookSpecificOutput"]["additionalContext"]
                .as_str()
                .unwrap()
                .to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no problem block was delivered"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits for the next delivered block that is a result plate, skipping `checking (…)` plates (T18B).
async fn await_eyes_result(actor: &mut ProductActor, fixture: &ProductFixture) -> String {
    loop {
        let block = await_eyes_block(actor, fixture).await;
        if !block.contains("checking") {
            return block;
        }
    }
}

/// Reads the `ide.context` problems page for an active Claude actor.
async fn eyes_problems(actor: &mut ProductActor, fixture: &ProductFixture) -> String {
    let reply = actor
        .call_claude(fixture, "ide.context", json!({"kind":"problems"}))
        .await;
    assert_eq!(reply["kind"], "context", "{reply}");
    reply["text"].as_str().unwrap().to_owned()
}

/// Start schedules a confined check whose first result reaches the next native post-hook once;
/// unchanged state stays silent, a native edit reruns the check and the block carries the delta,
/// and `ide.context kind=problems` reports the same counts (EYES-r1 §5–§7).
#[tokio::test]
async fn eyes_claude_post_hook_delivers_problem_block_and_delta() {
    let fixture = ProductFixture::new_claude(json!([]));
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let (mut actor, checking) = eyes_claude_actor(&fixture, "claude-eyes").await;
    // The first due plate reaches the activation inspect call's own post hook (T22B); the 100 ms
    // debounce means the check cannot have completed before that hook fires.
    assert!(checking.contains("checking (first check)"), "{checking}");

    let first = await_eyes_result(&mut actor, &fixture).await;
    assert_eq!(
        first,
        "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>"
    );
    assert!(
        actor.claude_native_post(&fixture, "Read").await.is_empty(),
        "unchanged problem state must not be re-emitted"
    );
    assert!(
        eyes_problems(&mut actor, &fixture)
            .await
            .starts_with("rust: ready; errors: 2; warnings: 0")
    );

    std::fs::write(fixture.root.join("problems.count"), "5").unwrap();
    let _ = actor.claude_native_post(&fixture, "Edit").await;
    let changed = await_eyes_result(&mut actor, &fixture).await;
    assert_eq!(
        changed,
        "<agent-ide>\nrust: 5 errors (+3), 0 warnings\n</agent-ide>"
    );
    assert!(
        eyes_problems(&mut actor, &fixture)
            .await
            .starts_with("rust: ready; errors: 5; warnings: 0")
    );
    // Each completed check records one bucketed telemetry event without paths or messages.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let query = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
            .args(["telemetry", "query", "--database"])
            .arg(fixture.runtime.join("telemetry.sqlite"))
            .args(["--tag", "project_check_completed"])
            .output()
            .await
            .unwrap();
        let rows = String::from_utf8_lossy(&query.stdout).into_owned();
        if query.status.success()
            && rows.contains(r#""errors_bucket":"1-9""#)
            && rows.contains(r#""state":"ready""#)
        {
            assert!(
                !rows.contains("src/lib.rs") && !rows.contains("fake"),
                "{rows}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "missing project check telemetry: {rows}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    assert!(home.join(".agent-ide/checks").is_dir());
}

/// Reads the `ide.context` problems page for an active Codex actor, with its carried status plate.
async fn eyes_codex_problems(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
) -> (String, Option<String>) {
    let reply = actor
        .call(fixture, "ide.context", json!({"kind":"problems"}))
        .await;
    assert_eq!(reply["kind"], "context", "{reply}");
    (
        reply["text"].as_str().unwrap().to_owned(),
        carried_status(&reply).map(str::to_owned),
    )
}

/// Polls Codex `ide.context` problems replies until one carries exactly `expected` (T28B).
///
/// Intermediate `checking (…)` plates are legitimately delivered once each; only the exact
/// expected plate satisfies the wait.
async fn await_eyes_codex_plate(
    actor: &mut ProductActor,
    fixture: &ProductFixture,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (_, plate) = eyes_codex_problems(actor, fixture).await;
        if plate.as_deref() == Some(expected) {
            return plate.expect("plate present");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no problem block was delivered on a reply: {plate:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A Codex host has no hook delivery: every terminal `ide.*` reply carries the due status plate
/// at its top, unchanged status is never repeated, an `ide.edit` that changes the check inputs is
/// followed by a reply with the delta, and `ide.stop` carries none (T28B).
#[tokio::test]
async fn eyes_codex_reply_carries_due_plate_and_delta() {
    let fixture = ProductFixture::new(json!([]));
    // The fake cargo holds its first run briefly so the activation reply cannot carry the result.
    let home = enable_fake_rust_checks_holding(&fixture, &fixture.base, "sleep 1");
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "codex-eyes").await;

    // Start completion carries the due first-check plate. Pending placeholders carry no plate;
    // the resolving terminal reply does.
    let started = actor
        .call(&fixture, "ide.start", json!({"activation_id":"eyes"}))
        .await;
    let started = actor.settle(&fixture, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let first = carried_status(&started).expect("start completion carries the due plate");
    assert!(
        first.contains("checking (first check)") || first.contains("rust:"),
        "{first}"
    );

    // A following terminal reply carries the first result (already carried at activation only
    // when that reply itself observed the completed check).
    if first.contains("2 errors") {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let (text, plate) = eyes_codex_problems(&mut actor, &fixture).await;
            assert_eq!(plate, None, "delivered results must not be re-emitted");
            if text.starts_with("rust: ready; errors: 2") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "problems page never reached readiness: {text}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    } else {
        let result = await_eyes_codex_plate(
            &mut actor,
            &fixture,
            "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>",
        )
        .await;
        assert_eq!(
            result,
            "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>"
        );
    }
    // Unchanged status is not repeated.
    for _ in 0..3 {
        let (_, plate) = eyes_codex_problems(&mut actor, &fixture).await;
        assert_eq!(plate, None, "unchanged status must not be re-emitted");
    }

    // An `ide.edit` that changes the check inputs is followed by a reply carrying the delta.
    let original = actor
        .call(&fixture, "ide.context", json!({"path":"problems.count"}))
        .await;
    let original = actor.settle(&fixture, original).await;
    let edited = actor
        .call(
            &fixture,
            "ide.edit",
            json!({
                "operation_id":"codex-eyes-edit-1",
                "path":"problems.count",
                "source_ref":original["detail_ref"],
                "content":"5\n"
            }),
        )
        .await;
    let edited = actor.settle(&fixture, edited).await;
    assert_eq!(edited["state"], "edit", "{edited}");
    let delta = await_eyes_codex_plate(
        &mut actor,
        &fixture,
        "<agent-ide>\nrust: 5 errors (+3), 0 warnings\n</agent-ide>",
    )
    .await;
    assert_eq!(
        delta,
        "<agent-ide>\nrust: 5 errors (+3), 0 warnings\n</agent-ide>"
    );

    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    assert!(
        carried_status(&stopped).is_none(),
        "ide.stop replies carry no plate"
    );
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
    assert!(home.join(".agent-ide/checks").is_dir());
}

/// Managed Codex (no hook stream at all) gets the plate on its managed replies too: activation
/// completion carries the first-check plate, a native edit between two calls is noticed and a
/// following managed reply carries the delta, and stop stays plate-free (T28B).
#[tokio::test]
async fn eyes_codex_managed_reply_carries_due_plate_after_native_edit() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    // The fake cargo holds its first run briefly so the activation reply cannot carry the result.
    let home = enable_fake_rust_checks_holding(&fixture, &fixture.base, "sleep 1");
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let mut mcp = Mcp::start_managed_with_home(&fixture.config, &fixture.root, Some(&home)).await;
    let state = fixture.state();
    let actor = "managed-eyes";
    let mut next = 10;

    let started = managed_call(
        &mut mcp,
        next,
        actor,
        "ide.start",
        json!({"activation_id":"eyes"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let first = carried_status(&started).expect("activation completion carries the due plate");
    assert!(
        first.contains("checking (first check)") || first.contains("rust:"),
        "{first}"
    );

    // A following managed reply carries the first result (already carried at activation only
    // when that reply itself observed the completed check).
    if !first.contains("1 error") {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let result = loop {
            next += 1;
            let reply = managed_call(
                &mut mcp,
                next,
                actor,
                "ide.context",
                json!({"kind":"problems"}),
                &state,
            )
            .await;
            if carried_status(&reply)
                == Some("<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>")
            {
                break carried_status(&reply).unwrap().to_owned();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no problem block was delivered on a managed reply: {reply}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        assert_eq!(
            result,
            "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
        );
    }

    // A native edit between two tool calls is noticed without any hook stream.
    std::fs::write(fixture.root.join("problems.count"), "3").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let delta = loop {
        next += 1;
        let reply = managed_call(
            &mut mcp,
            next,
            actor,
            "ide.context",
            json!({"kind":"problems"}),
            &state,
        )
        .await;
        if carried_status(&reply)
            == Some("<agent-ide>\nrust: 3 errors (+2), 0 warnings\n</agent-ide>")
        {
            break carried_status(&reply).unwrap().to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no delta was delivered on a managed reply: {reply}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert_eq!(
        delta,
        "<agent-ide>\nrust: 3 errors (+2), 0 warnings\n</agent-ide>"
    );

    next += 1;
    let stopped = managed_call(&mut mcp, next, actor, "ide.stop", json!({}), &state).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    assert!(carried_status(&stopped).is_none(), "{stopped}");
    mcp.close().await;
}

/// Runs one managed Codex native hook phase through the real `codex-hook --managed` command with
/// the exact identity a paired native call carries: root session, actor, tool-use id, tool name.
async fn managed_native_phase(
    root: &Path,
    session: &str,
    actor: &str,
    phase: &str,
    call: &str,
    tool: Option<&str>,
) -> std::process::Output {
    let mut payload = json!({
        "hook_event_name": phase,
        "session_id": session,
        "agent_id": actor,
        "tool_use_id": call,
    });
    if let Some(tool) = tool {
        payload["tool_name"] = json!(tool);
    }
    managed_codex_hook(root, &payload).await
}

/// Runs one paired native Pre/Post for an actor and returns the post hook's raw stdout.
///
/// The pre hook must stay silent and succeed; the post may carry model context.
async fn managed_native_post(
    root: &Path,
    session: &str,
    actor: &str,
    call: &str,
    tool: &str,
) -> String {
    let pre = managed_native_phase(root, session, actor, "PreToolUse", call, None).await;
    assert!(
        pre.status.success() && pre.stdout.is_empty() && pre.stderr.is_empty(),
        "managed pre hook must stay silent: {pre:?}"
    );
    let post = managed_native_phase(root, session, actor, "PostToolUse", call, Some(tool)).await;
    assert!(
        post.status.success() && post.stderr.is_empty(),
        "managed post hook failed: {post:?}"
    );
    String::from_utf8(post.stdout).unwrap()
}

/// Extracts `hookSpecificOutput.additionalContext` from a managed post hook's stdout; empty when
/// the hook was silent.
fn managed_hook_context(stdout: &str) -> String {
    if stdout.is_empty() {
        return String::new();
    }
    let rendered: Value = serde_json::from_str(stdout).expect("hook stdout is the JSON contract");
    rendered["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("post context carries additionalContext")
        .to_owned()
}

/// Polls one actor's paired managed native post hooks until one delivers exactly `expected`
/// (T29B §7). No `ide.*` call participates in this loop: the hook carrier is the only channel.
/// Intermediate `checking (…)` plates are legitimately delivered once each and simply consumed.
async fn await_eyes_codex_hook_plate(
    root: &Path,
    session: &str,
    actor: &str,
    expected: &str,
    poll: &mut usize,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        *poll += 1;
        let stdout =
            managed_native_post(root, session, actor, &format!("native-{poll}"), "Bash").await;
        let context = managed_hook_context(&stdout);
        if context == expected {
            return context;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no problem block was delivered on a managed hook: {context:?}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// Polls managed `ide.context` problems replies (explicit root session) until one carries exactly
/// `expected` on its reply carrier (T28B), returning the plate.
async fn await_eyes_codex_managed_root_plate(
    mcp: &mut Mcp,
    next: &mut usize,
    actor: &str,
    session: &str,
    state: &Value,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        *next += 1;
        let reply = managed_root_call(
            mcp,
            *next,
            actor,
            session,
            "ide.context",
            json!({"kind":"problems"}),
            state,
        )
        .await;
        if carried_status(&reply) == Some(expected) {
            return expected.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no problem block was delivered on a managed reply: {reply}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Managed Codex native hooks deliver the plate without any later `ide.*` call: after the
/// activation, paired native post hooks carry the first result and then the `(+1)` delta of a
/// native edit, and the reply carrier never repeats the hook's plate (T29B §5, §7).
#[tokio::test]
async fn eyes_codex_managed_native_post_delivers_plate_and_delta_without_ide_calls() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    // The fake cargo waits at the gate: the first result cannot become due until the test
    // releases it, so no activation reply can ever consume it before a hook polls.
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-hooks");
    let root = base.join("rendezvous");
    let mut mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    let state = fixture.state();
    let session = "eyes-session";
    let actor = "eyes-actor";
    let mut next = 100;

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"eyes-hooks"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    // Activation has settled: only now does the first result become due for a hook.
    release_checks_gate(&fixture);

    // From here on every delivery happens through managed native hooks only.
    let first = await_eyes_codex_hook_plate(
        &root,
        session,
        actor,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>",
        &mut next,
    )
    .await;
    assert_eq!(
        first,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
    );

    // A native edit that no `ide.*` call ever observes still produces the (+1) delta, on a hook.
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let delta = await_eyes_codex_hook_plate(
        &root,
        session,
        actor,
        "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>",
        &mut next,
    )
    .await;
    assert_eq!(
        delta,
        "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>"
    );

    // Hook→reply dedup: the terminal reply must not repeat the hook's plate. A `checking (…)`
    // plate for the reconciliation the reply itself scheduled is a new state, not a repeat.
    next += 1;
    let reply = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.context",
        json!({"kind":"problems"}),
        &state,
    )
    .await;
    let plate = carried_status(&reply);
    assert_ne!(
        plate,
        Some(delta.as_str()),
        "the reply carrier repeated the hook's plate: {reply}"
    );
    assert!(
        plate.is_none() || plate.unwrap().contains("checking"),
        "{plate:?}"
    );

    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// Reply→hook dedup: when the reply carrier delivers a due plate first, the paired managed native
/// post hook that follows stays silent, and the next reply carries nothing either (T29B §5).
#[tokio::test]
async fn eyes_codex_reply_carrier_consumes_the_plate_before_the_native_hook() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    // Gate-held: the first result cannot be consumed by the activation reply; the reply poll
    // below is the first carrier to run after the release.
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-reply-first");
    let root = base.join("rendezvous");
    let mut mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    let state = fixture.state();
    let session = "eyes-session";
    let actor = "eyes-actor";
    let mut next = 100;

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"eyes-reply-first"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    release_checks_gate(&fixture);

    let result = await_eyes_codex_managed_root_plate(
        &mut mcp,
        &mut next,
        actor,
        session,
        &state,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>",
    )
    .await;
    assert_eq!(
        result,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
    );

    // The reply consumed it: the paired native post that follows this call is silent, and so is
    // the next terminal reply.
    let stdout = managed_native_post(&root, session, actor, "native-reply-first", "Bash").await;
    assert!(
        managed_hook_context(&stdout).is_empty(),
        "the hook repeated the reply's plate: {stdout}"
    );
    next += 1;
    let reply = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.context",
        json!({"kind":"problems"}),
        &state,
    )
    .await;
    assert!(
        carried_status(&reply).is_none() || carried_status(&reply).unwrap().contains("checking"),
        "the reply repeated its own plate: {reply}"
    );

    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// Concurrent delivery: a managed native post hook and a terminal reply fired at the same moment
/// yield exactly one plate between them, and neither carrier repeats it afterwards (T29B §5).
#[tokio::test]
async fn eyes_codex_concurrent_hook_and_reply_deliver_exactly_one_plate() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-concurrent");
    let root = base.join("rendezvous");
    let mut mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    let state = fixture.state();
    let session = "eyes-session";
    let actor = "eyes-actor";
    let mut next = 100;

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"eyes-concurrent"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    // Activation has settled: releasing the gate is the synchronization point that makes the
    // result due, so the concurrent race below starts from a deterministically undelivered plate.
    release_checks_gate(&fixture);

    let expected = "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>";
    // Fire both carriers concurrently against the same binding fingerprint. While the held first
    // check is still running neither carrier has the plate due, so an all-silent round simply
    // retries; the moment the result is due, this race must yield exactly one delivery.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        next += 1;
        let next_for_reply = next;
        let race_call = format!("native-race-{next}");
        let hook_run = managed_native_post(&root, session, actor, &race_call, "Bash");
        let state_for_race = state.clone();
        let reply_run = mcp.exchange(
            json!({"jsonrpc":"2.0","id":next_for_reply,"method":"tools/call","params":{"name":"ide.context",
                "arguments":{"kind":"problems"},
                "_meta":{"threadId":actor,"callId":format!("managed-{actor}-{next_for_reply}"),
                "x-codex-turn-metadata":{"session_id":session},"codex/sandbox-state-meta":state_for_race}}}),
        );
        let (hook_stdout, reply) = tokio::join!(hook_run, reply_run);
        assert_compact_envelope(&reply);
        let hook_plate = managed_hook_context(&hook_stdout);
        let reply_plate = carried_status(&reply["result"]["structuredContent"]).map(str::to_owned);
        let carried = [
            hook_plate == expected,
            reply_plate.as_deref() == Some(expected),
        ];
        let deliveries = carried.iter().filter(|delivered| **delivered).count();
        if deliveries == 1 {
            break;
        }
        assert_eq!(
            deliveries, 0,
            "both carriers delivered the same plate: hook={hook_plate:?} reply={reply_plate:?}"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "the result never became due for the concurrent race"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    // And the losing carrier (and any later one) never repeats it.
    let stdout = managed_native_post(&root, session, actor, "native-race-2", "Bash").await;
    assert_ne!(
        managed_hook_context(&stdout),
        expected,
        "the hook repeated the delivered plate"
    );
    next += 1;
    let later = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.context",
        json!({"kind":"problems"}),
        &state,
    )
    .await;
    assert_ne!(
        carried_status(&later),
        Some(expected),
        "the reply repeated the delivered plate: {later}"
    );

    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// A parent and two child actors on one root session are fully isolated. Each actor owns its own
/// managed MCP process (a daemon's workspace authority admits one worktree owner, so a child
/// session runs its own), and all three share one rendezvous root: a child's hook never falls
/// back to the parent's route before the child published its own, each route is distinct and
/// live, and each actor's delta is delivered exactly once, by its own binding (T29B §7).
#[tokio::test]
async fn eyes_codex_parent_and_two_children_are_isolated_without_route_fallback() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-family");
    let root = base.join("rendezvous");
    let state = fixture.state();
    let session = "eyes-family";
    let parent = "eyes-parent";
    let children = ["eyes-child-1", "eyes-child-2"];

    // Before any child MCP process exists there is no child route: the hook must stay silent
    // instead of falling back to the parent's published route.
    let mut parent_mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    let mut next = 100;
    let started = managed_root_call(
        &mut parent_mcp,
        next,
        parent,
        session,
        "ide.start",
        json!({"activation_id":"eyes-family"}),
        &state,
    )
    .await;
    let settled = settle_managed(&mut parent_mcp, &mut next, parent, &state, started).await;
    assert_eq!(settled["kind"], "activation", "{settled}");
    let orphan = managed_native_phase(
        &root,
        session,
        children[0],
        "PostToolUse",
        "orphan-child",
        Some("Bash"),
    )
    .await;
    assert_managed_hook_silent(&orphan);

    // Each child is its own managed MCP: it publishes its own distinct route for the shared root
    // session and activates its own binding, so each actor gets only its own daemon's plates.
    let mut child_mcps = Vec::new();
    for child in children {
        let mut mcp =
            Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root))
                .await;
        let mut child_next = 100;
        let started = managed_root_call(
            &mut mcp,
            child_next,
            child,
            session,
            "ide.start",
            json!({"activation_id":format!("eyes-family-{child}")}),
            &state,
        )
        .await;
        let settled = settle_managed(&mut mcp, &mut child_next, child, &state, started).await;
        assert_eq!(settled["kind"], "activation", "{settled}");
        child_mcps.push(mcp);
    }
    // All three activations have settled while their checks stayed held: the release is the
    // synchronization point that makes each first result due for its own actor's hook poll.
    release_checks_gate(&fixture);
    let parent_identity = CodexRouteIdentity::new(session, parent).unwrap();
    assert!(discover(&root, &parent_identity).is_some(), "parent route");
    for child in children {
        let identity = CodexRouteIdentity::new(session, child).unwrap();
        assert_ne!(identity.digest(), parent_identity.digest());
        assert!(discover(&root, &identity).is_some(), "{child} route");
    }

    // The `(+1)` delta needs the first result delivered as each actor's delta baseline first.
    // Every activation completion happened while its check was held, so no reply carried the
    // result: each actor's own hooks deliver it, each exactly once.
    let mut poll = 200;
    for actor in [parent, children[0], children[1]] {
        let baseline = await_eyes_codex_hook_plate(
            &root,
            session,
            actor,
            "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>",
            &mut poll,
        )
        .await;
        assert_eq!(
            baseline,
            "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
        );
    }

    // One shared native edit: every actor's own hooks deliver the same delta exactly once,
    // each through its own binding — an entangled route would consume a sibling's due plate.
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    for actor in [parent, children[0], children[1]] {
        let delta = await_eyes_codex_hook_plate(
            &root,
            session,
            actor,
            "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>",
            &mut poll,
        )
        .await;
        assert_eq!(
            delta,
            "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>"
        );
        // Immediately delivered again to the same actor? Never. (A fresh `checking (…)` plate
        // for a scheduled recheck would be a new state, not a repeat.)
        let stdout = managed_native_post(
            &root,
            session,
            actor,
            &format!("native-again-{actor}"),
            "Bash",
        )
        .await;
        let context = managed_hook_context(&stdout);
        assert!(
            context.is_empty() || context.contains("checking"),
            "{actor}'s delta was delivered twice: {context}"
        );
    }

    parent_mcp.close().await;
    for mcp in child_mcps {
        mcp.close().await;
    }
    std::fs::remove_dir_all(base).unwrap();
}

/// Two root sessions on the same repository converge on one actor binding: both routes are
/// published and live, but a plate delivered through session A's route is never delivered again
/// through session B's route, because both resolve the same binding fingerprint (T29B §5).
#[tokio::test]
async fn eyes_codex_two_sessions_on_one_repository_deliver_once_across_routes() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-sessions");
    let root = base.join("rendezvous");
    let mut mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    let state = fixture.state();
    let actor = "eyes-resumed";
    let session_a = "eyes-session-a";
    let session_b = "eyes-session-b";
    let mut next = 100;

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session_a,
        "ide.start",
        json!({"activation_id":"eyes-session-a"}),
        &state,
    )
    .await;
    let settled = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(settled["kind"], "activation", "{settled}");
    // The second session publishes its own route through an ordinary call on the same actor; a
    // second explicit start would not be an idempotent replay of the first activation.
    next += 1;
    let resumed = managed_root_call(
        &mut mcp,
        next,
        actor,
        session_b,
        "ide.context",
        json!({"kind":"problems"}),
        &state,
    )
    .await;
    assert_eq!(resumed["state"], "complete", "{resumed}");
    let route_a = CodexRouteIdentity::new(session_a, actor).unwrap();
    let route_b = CodexRouteIdentity::new(session_b, actor).unwrap();
    assert_ne!(route_a.digest(), route_b.digest(), "two distinct routes");
    assert!(discover(&root, &route_a).is_some());
    assert!(discover(&root, &route_b).is_some());
    // Both lifecycle calls have settled while the check stayed held; the release makes the first
    // result due for the hook carrier below, and no reply could have consumed it beforehand.
    release_checks_gate(&fixture);

    // The delta needs the first result as its baseline first; the replies could not have carried
    // it (the held check was still running), so the hook carrier delivers it now.
    let baseline = await_eyes_codex_hook_plate(
        &root,
        session_a,
        actor,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>",
        &mut next,
    )
    .await;
    assert_eq!(
        baseline,
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>"
    );

    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let expected = "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>";
    let delta = await_eyes_codex_hook_plate(&root, session_a, actor, expected, &mut next).await;
    assert_eq!(delta, expected);

    // The same actor through the OTHER session's route: same binding fingerprint, same feed key,
    // so the delta is not delivered again.
    let stdout = managed_native_post(&root, session_b, actor, "native-session-b", "Bash").await;
    let context = managed_hook_context(&stdout);
    assert_ne!(
        context, expected,
        "session B's route repeated session A's plate: {context}"
    );
    assert!(
        context.is_empty() || context.contains("checking"),
        "{context}"
    );

    mcp.close().await;
    std::fs::remove_dir_all(base).unwrap();
}

/// Managed Codex hook lifecycles that must never speak: a post without its pre, the MCP call's
/// own paired native hooks in the real host order Pre → admission → Post, a call rejected before
/// admission whose self-MCP post must stay silent, stop → restart → the old Post, malformed and
/// oversized payloads, and a stale publication after the MCP exited — all silent exit 0 (T29B §7,
/// final review 4/5). A due-plate positive control first proves the same fixture WOULD deliver on
/// a valid native post, and the completed-check counter proves none of the silent lifecycles
/// scheduled a check.
#[tokio::test]
async fn eyes_codex_managed_hook_lifecycles_stay_silent_without_delivery() {
    let _managed_runtime_guard = MANAGED_CODEX_TEST_LOCK.lock().await;
    let fixture = ProductFixture::new(json!([]));
    // Gate-held checks: the first result cannot be consumed by any activation reply.
    let home =
        enable_fake_rust_checks_holding(&fixture, &fixture.base, &hold_checks_at_gate(&fixture));
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let base = rendezvous_area("eyes-lifecycle");
    let root = base.join("rendezvous");
    let mut mcp =
        Mcp::start_managed_custom(&fixture.config, &fixture.root, Some(&home), Some(&root)).await;
    // The managed daemon records check telemetry in its persistent per-candidate store below the
    // redirected home; this fixture's fresh home holds exactly one.
    let mut stores = std::fs::read_dir(home.join(".agent-ide/telemetry"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(stores.len(), 1, "{stores:?}");
    let database = stores.remove(0).path().join("state.sqlite");
    let state = fixture.state();
    let session = "edge-session";
    let actor = "edge-actor";
    let mut next = 100;

    let started = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"edge-start"}),
        &state,
    )
    .await;
    let started = settle_managed(&mut mcp, &mut next, actor, &state, started).await;
    assert_eq!(started["kind"], "activation", "{started}");
    // Activation has settled; the release is the synchronization point that makes the first
    // result due while nothing has consumed it yet.
    release_checks_gate(&fixture);

    // Positive control, part one: the activation check completed and its result is due.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while completed_check_count(&database).await < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the activation check never completed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A call rejected BEFORE admission (invalid parameters never reach the daemon) leaves its
    // pre buffered: its self-MCP post must be a silent rejection — no native observation, no
    // check trigger, and no plate, even though one is due right now (T29B final review 4).
    let rejected_call = "rejected-self-1";
    let rejected_pre =
        managed_native_phase(&root, session, actor, "PreToolUse", rejected_call, None).await;
    assert_managed_hook_silent(&rejected_pre);
    next += 1;
    let invalid = mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":next,"method":"tools/call","params":{"name":"ide.context",
            "arguments":{},
            "_meta":{"threadId":actor,"callId":rejected_call,
            "x-codex-turn-metadata":{"session_id":session},"codex/sandbox-state-meta":state}}}),
        )
        .await;
    assert_eq!(invalid["result"]["isError"], true, "{invalid}");
    assert!(
        invalid["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("invalid bounded parameters"),
        "{invalid}"
    );
    let rejected_post = managed_native_phase(
        &root,
        session,
        actor,
        "PostToolUse",
        rejected_call,
        Some("mcp__agent-ide__context"),
    )
    .await;
    assert_managed_hook_silent(&rejected_post);

    // Positive control, part two: the same due plate IS delivered to a valid paired native post
    // — the fixture would speak, so the rejected self post's silence above is meaningful.
    let control = managed_native_post(&root, session, actor, "control-1", "Bash").await;
    assert_eq!(
        managed_hook_context(&control),
        "<agent-ide>\nrust: 1 error, 0 warnings\n</agent-ide>",
        "positive control: the fixture must deliver on a valid native post"
    );

    // A post whose pre was never observed correlates to nothing and schedules nothing.
    let orphan = managed_native_phase(
        &root,
        session,
        actor,
        "PostToolUse",
        "orphan-1",
        Some("Bash"),
    )
    .await;
    assert_managed_hook_silent(&orphan);

    // The MCP call's own paired native hooks stay silent in the REAL host order: the pre hook
    // fires BEFORE the call, whose admission consumes it and records the call completed, so the
    // post is a completed replay — and neither phase may trigger a check that would treat the
    // call's own result as a foreign change.
    next += 1;
    let own_call = format!("managed-{actor}-{next}");
    let own_pre = managed_native_phase(&root, session, actor, "PreToolUse", &own_call, None).await;
    assert_managed_hook_silent(&own_pre);
    let reply = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    assert_eq!(reply["state"], "pending", "{reply}");
    let own_post = managed_native_phase(
        &root,
        session,
        actor,
        "PostToolUse",
        &own_call,
        Some("mcp__agent_ide__context"),
    )
    .await;
    assert_managed_hook_silent(&own_post);
    // Poisoned nothing: the same actor's next call still validates and answers.
    next += 1;
    let followup = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.context",
        json!({"path":"tracked.txt"}),
        &state,
    )
    .await;
    assert_eq!(followup["state"], "pending", "{followup}");

    // The check counter proves none of the silent lifecycles above (and not the control post
    // itself) scheduled a project check: the activation's one completed check is still all.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            completed_check_count(&database).await,
            1,
            "a silent lifecycle scheduled a project check"
        );
    }

    // A real native Pre→Post still works after the rejected self call: the edited check inputs
    // rerun and the paired native post delivers the (+1) delta (T29B final review 4).
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let delta = await_eyes_codex_hook_plate(
        &root,
        session,
        actor,
        "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>",
        &mut next,
    )
    .await;
    assert_eq!(
        delta,
        "<agent-ide>\nrust: 2 errors (+1), 0 warnings\n</agent-ide>"
    );

    // The post of a call whose pre predates ide.stop stays silent across a restart: the stop
    // rejects the buffered pre, so the old post can neither settle nor attach to the fresh
    // generation, and the fresh generation observes nothing (T29B final review 5).
    let late_pre = managed_native_phase(&root, session, actor, "PreToolUse", "late-1", None).await;
    assert_managed_hook_silent(&late_pre);
    next += 1;
    let stopped = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.stop",
        json!({}),
        &state,
    )
    .await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    let late_post =
        managed_native_phase(&root, session, actor, "PostToolUse", "late-1", Some("Bash")).await;
    assert_managed_hook_silent(&late_post);
    next += 1;
    let restarted = managed_root_call(
        &mut mcp,
        next,
        actor,
        session,
        "ide.start",
        json!({"activation_id":"edge-restart"}),
        &state,
    )
    .await;
    let restarted = settle_managed(&mut mcp, &mut next, actor, &state, restarted).await;
    assert_eq!(restarted["kind"], "activation", "{restarted}");
    let old_post =
        managed_native_phase(&root, session, actor, "PostToolUse", "late-1", Some("Bash")).await;
    assert_managed_hook_silent(&old_post);

    // Malformed (identity-less) and oversized payloads are discarded silently.
    let malformed = managed_codex_hook(
        &root,
        &json!({"hook_event_name":"PostToolUse","tool_use_id":"x"}),
    )
    .await;
    assert_managed_hook_silent(&malformed);
    let mut child = managed_codex_hook_process(&root, None);
    let mut input = child.stdin.take().unwrap();
    input.write_all(&vec![b'x'; 64 * 1024 + 1]).await.unwrap();
    input.shutdown().await.unwrap();
    drop(input);
    let oversized = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_managed_hook_silent(&oversized);

    // After the MCP exits, its routes are retired and its daemon is gone: a stale hook stays
    // silent and exits 0.
    mcp.close().await;
    let stale = managed_native_phase(
        &root,
        session,
        actor,
        "PostToolUse",
        "stale-1",
        Some("Bash"),
    )
    .await;
    assert_managed_hook_silent(&stale);
    std::fs::remove_dir_all(base).unwrap();
}

/// Claude keeps hook-only plate delivery: `ide.*` replies never carry the plate even while one is
/// due, the withheld posts stay silent, and the still-due plate reaches the next native post (T28B).
#[tokio::test]
async fn eyes_claude_method_replies_never_carry_the_plate() {
    let fixture = ProductFixture::new_claude(json!([]));
    let home = enable_fake_rust_checks(&fixture, &fixture.base);
    std::fs::write(fixture.root.join("problems.count"), "2").unwrap();
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let (mut actor, checking) = eyes_claude_actor(&fixture, "claude-eyes-replies").await;
    assert!(checking.contains("checking (first check)"), "{checking}");

    // Method calls with their posts withheld read the problems page while the first result is
    // becoming due; no reply ever leads with the plate.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let problems = loop {
        actor.next += 1;
        let call = format!("call-{}", actor.next);
        actor.claude_lifecycle(&fixture, "PreToolUse", &call).await;
        let reply = actor
            .mcp
            .exchange(
                json!({"jsonrpc":"2.0","id":actor.next,"method":"tools/call","params":{
                "name":"ide.context","arguments":{"kind":"problems"},
                "_meta":{"claudecode/toolUseId":call}}}),
            )
            .await;
        let text = assert_claude_envelope(&reply);
        assert!(!text.starts_with("<agent-ide>"), "{text}");
        if text.starts_with("complete context: rust: ready; errors: 2") {
            break text.to_owned();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "problems page never reached readiness: {text}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(
        problems.starts_with("complete context: rust: ready; errors: 2"),
        "{problems}"
    );

    // The plate was never consumed by a method reply: the next native post hook delivers it.
    let first = await_eyes_result(&mut actor, &fixture).await;
    assert_eq!(
        first,
        "<agent-ide>\nrust: 2 errors, 0 warnings\n</agent-ide>"
    );

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A plate that becomes due while a helper runs is delivered on the helper's own `Bash` post (T22B).
///
/// The activation inspect call's post hook and a second helper's minting post hook are both
/// withheld, so no hook fires between the activation and the helper's own `Bash` post; that post
/// delivers the still-running first check's plate exactly once, and the withheld posts plus the
/// following native post stay silent.
#[tokio::test]
async fn eyes_claude_helper_post_delivers_due_first_check_plate_once() {
    let fixture = ProductFixture::new_claude(json!([]));
    let home = enable_fake_rust_checks_holding(&fixture, &fixture.base, "sleep 20");
    std::fs::write(fixture.root.join("problems.count"), "1").unwrap();
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let mut actor = ProductActor::new(&fixture, "claude-eyes-helper-post").await;

    let started = actor
        .call_claude(&fixture, "ide.start", json!({"activation_id":"eyes"}))
        .await;
    // The start helper's own Bash post stays silent: activation is not complete yet.
    let start_ref = actor.launch_claude_pending(&fixture, &started).await;

    // Activation completes inside this inspect exchange; its post hook is withheld so the
    // first-check plate stays due.
    actor.next += 1;
    let inspect_call = format!("call-{}", actor.next);
    actor
        .claude_lifecycle(&fixture, "PreToolUse", &inspect_call)
        .await;
    let reply = actor
        .mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":actor.next,"method":"tools/call","params":{
            "name":"ide.inspect","arguments":{"detail_ref":start_ref},
            "_meta":{"claudecode/toolUseId":inspect_call}}}),
        )
        .await;
    let started = claude_fields(assert_claude_envelope(&reply));
    assert_eq!(started["kind"], "activation", "{started}");

    // A second helper is minted (its own post hook withheld too), then armed and run while the
    // first check is still held running by the sleeping fake cargo.
    actor.next += 1;
    let diff_call = format!("call-{}", actor.next);
    actor
        .claude_lifecycle(&fixture, "PreToolUse", &diff_call)
        .await;
    let diff_reply = actor
        .mcp
        .exchange(
            json!({"jsonrpc":"2.0","id":actor.next,"method":"tools/call","params":{
            "name":"ide.diff","arguments":{"mode":"head"},
            "_meta":{"claudecode/toolUseId":diff_call}}}),
        )
        .await;
    let diff = claude_fields(assert_claude_envelope(&diff_reply));
    assert_eq!(diff["state"], "pending", "{diff}");
    let (_detail_ref, launch_call) = actor.run_claude_pending(&fixture, &diff).await;

    // The helper's own Bash post hook carries the due plate.
    let post = actor
        .claude_lifecycle_output(&fixture, "PostToolUse", &launch_call)
        .await;
    assert!(post.status.success() && post.stderr.is_empty());
    let rendered: Value = serde_json::from_str(&String::from_utf8(post.stdout).unwrap()).unwrap();
    assert_eq!(
        rendered["hookSpecificOutput"]["hookEventName"], "PostToolUse",
        "{rendered}"
    );
    let plate = rendered["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(plate.contains("checking (first check)"), "{plate}");

    // The identical plate is never repeated: the two withheld MCP posts and the next native post
    // stay silent.
    for call in [diff_call, inspect_call] {
        let withheld = actor
            .claude_lifecycle_output(&fixture, "PostToolUse", &call)
            .await;
        assert!(
            withheld.status.success() && withheld.stdout.is_empty() && withheld.stderr.is_empty()
        );
    }
    assert!(actor.claude_native_post(&fixture, "Read").await.is_empty());

    let stopped = actor.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// A worktree outside every allowed root schedules no check and reports `outside allowed roots`.
#[tokio::test]
async fn eyes_outside_roots_reports_unavailable_without_checking() {
    let fixture = ProductFixture::new_claude(json!([]));
    let elsewhere = fixture.base.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let home = enable_fake_rust_checks(&fixture, &elsewhere);
    let mut daemon = fixture.daemon_with_home(Some(&home)).await;
    let (mut actor, block) = eyes_claude_actor(&fixture, "claude-eyes-outside").await;

    // No check can ever complete here, so the one due plate already arrived on the activation
    // inspect call's own post hook (T22B) and the next native post must not repeat it.
    assert_eq!(
        block,
        "<agent-ide>\nrust: outside allowed roots\n</agent-ide>"
    );
    assert!(actor.claude_native_post(&fixture, "Read").await.is_empty());
    assert_eq!(
        eyes_problems(&mut actor, &fixture).await,
        "rust: unavailable:outside_roots"
    );
    assert!(
        !home
            .join(".agent-ide/checks")
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some()),
        "no check cache may be created for an outside worktree"
    );

    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}

/// Without `project_checks`, native post-hooks stay silent and the problems kind is disabled.
#[tokio::test]
async fn eyes_absent_configuration_keeps_v02_hook_replies() {
    let fixture = ProductFixture::new_claude(json!([]));
    let mut daemon = fixture.daemon().await;
    let (mut actor, delivered) = eyes_claude_actor(&fixture, "claude-eyes-absent").await;
    assert!(delivered.is_empty(), "{delivered}");

    for tool in ["Edit", "Read"] {
        assert!(actor.claude_native_post(&fixture, tool).await.is_empty());
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(actor.claude_native_post(&fixture, "Read").await.is_empty());
    assert_eq!(eyes_problems(&mut actor, &fixture).await, "checks disabled");

    actor.mcp.close().await;
    daemon.kill().await.unwrap();
    daemon.wait().await.unwrap();
}
