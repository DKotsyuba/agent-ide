//! Executable MCP roundtrips for static discovery, separated ingress, and finite daemon routing.

use std::{
    path::{Path, PathBuf},
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

/// Distinguishes temporary endpoints across concurrently running scenarios in this process.
static NEXT_RUNTIME: AtomicUsize = AtomicUsize::new(0);

/// Allows 200 ms for subprocess startup/scheduling beyond the 250 ms hook contract.
///
/// Child Tokio clocks cannot be paused by this test runtime. This wall-clock ceiling stays
/// below both a doubled (500 ms) and sixfold (1500 ms) timeout without changing product code.
const HOOK_EXIT_CEILING: Duration = Duration::from_millis(450);

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
    mcp.close().await;
    assert!(!runtime.exists());
}

/// Proves all five shipping handlers reach the real daemon only after separated launcher and request ingress.
#[tokio::test]
async fn binary_routes_five_methods_to_typed_missing_peer_and_survives_daemon_loss() {
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
        assert_eq!(response["result"]["isError"], true, "{response}");
        assert_eq!(
            response["result"]["content"][0]["text"],
            "Assistance unavailable: host_binding; continue with native tools"
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
    assert_eq!(stopped["result"]["isError"], false);
    boundary(&stopped, "host binding stopped");
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
        br#"{"hook_event_name":"PreToolUse","session_id":"a","agent_id":"b","tool_use_id":"c"}"#
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
                "session_id":null,"agent_type":null})
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
                "session_id":null,"agent_type":null}),
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
        "host binding stopped",
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
        let fixture = Self {
            runtime: base.join("ipc"),
            config: base.join("launcher.json"),
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
        fixture.write_config(providers);
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
    fn write_config(&self, providers: Value) {
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
        let config = json!({"version":1,"limits":{"queued":16,"details":64,"operation_ms":120000,"output_bytes":1048576},"targets":[{"attachment":"private-host-channel","candidate":self.root,"git":accepted_program("/usr/bin/git","fixture-git"),"codex":accepted_program("/usr/bin/true","unused-disabled-wrapper"),"providers":providers,"profiles":[{"record":serde_json::from_str::<Value>(&record.to_json()).unwrap(),"sandbox_state":self.state()}],"allow_disabled_host":true}]});
        std::fs::write(&self.config, config.to_string()).unwrap();
    }
    /// Returns the current fixture's complete measured-state-shaped payload outside model arguments.
    fn state(&self) -> Value {
        json!({"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":self.root,"useLegacyLandlock":false})
    }
    /// Starts one configured shipping daemon and waits only for its real private endpoint.
    async fn daemon(&self) -> Child {
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_agent-ide"))
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
        let output = tokio::time::timeout(Duration::from_secs(2), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    }
    /// Runs exact Pre→MCP→Post with current host state separated from bounded model arguments.
    async fn call(&mut self, fixture: &ProductFixture, name: &str, arguments: Value) -> Value {
        self.next += 1;
        let call = format!("call-{}", self.next);
        self.lifecycle(fixture, "PreToolUse", &call).await;
        let reply=self.mcp.exchange(json!({"jsonrpc":"2.0","id":self.next,"method":"tools/call","params":{"name":name,"arguments":arguments,"_meta":{"threadId":self.actor,"callId":call,"x-codex-turn-metadata":{},"codex/sandbox-state-meta":self.state}}})).await;
        self.lifecycle(fixture, "PostToolUse", &call).await;
        assert!(!reply["result"]["structuredContent"].is_null(), "{reply}");
        reply["result"]["structuredContent"].clone()
    }
    /// Retrieves a same-binding result without keeping the original IPC request open through warmup.
    async fn settle(&mut self, fixture: &ProductFixture, mut reply: Value) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(150);
        while reply["state"] == "pending" {
            assert!(
                tokio::time::Instant::now() < deadline,
                "product operation did not settle"
            );
            let reference = reply["detail_ref"].as_str().unwrap().to_owned();
            tokio::time::sleep(Duration::from_millis(25)).await;
            reply = self
                .call(fixture, "ide.inspect", json!({"detail_ref":reference}))
                .await;
        }
        reply
    }
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
                assert!(text.contains("session_baseline: Unverified"));
                diff_ref = diff["detail_ref"].as_str().unwrap().to_owned();
            }
            "staged" => {
                assert!(text.contains("-base") && text.contains("+index"));
                assert!(text.contains("session_baseline: NotCaptured"));
            }
            _ => {
                assert!(text.contains("-index") && text.contains("+worktree"));
                assert!(text.contains("session_baseline: NotCaptured"));
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
        for directory in ["cache", "cargo", "target"] {
            std::fs::create_dir(fixture.base.join(directory)).unwrap();
        }
        std::fs::write(&wrapper, format!("#!/bin/sh\nexport XDG_CACHE_HOME='{}'\nexport CARGO_HOME='{}'\nexport CARGO_TARGET_DIR='{}'\nexec '{}' \"$@\"\n",fixture.base.join("cache").display(),fixture.base.join("cargo").display(),fixture.base.join("target").display(),analyzer.replace('\'', "'\\''"))).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let providers = json!([{"executable":accepted_program(&gopls,"golang.org/x/tools/gopls v0.23.0"),"settings":"gopls_defaults","toolchain":go,"cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"fixture-go-cache"},{"executable":accepted_program(wrapper.to_str().unwrap(),"rust-analyzer 1.98.1 (48a229ce 2026-09-01)"),"settings":"rust_cache_priming_disabled_v1","toolchain":toolchain,"cargo_version":"cargo 1.98.1","rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"fixture-rust-cache"}]);
        fixture.write_config(providers);
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
            assert_eq!(response["kind"], "context", "{response}");
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

/// Cancels a configured owned provider during warmup: the direct child is still killed and reaped even
/// though its socket identity was never captured, and the stop honestly reports that uncertainty
/// instead of a false success.
#[tokio::test]
async fn configured_product_stop_reaps_a_provider_that_never_becomes_ready() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = ProductFixture::new(json!([]));
    let program = fixture.base.join("slow-provider");
    let marker = fixture.base.join("provider-pid");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s' $$ > '{}'\nexec /bin/sleep 30\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"slow-fixture-provider"),"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"slow-fixture-cache"}]));
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
    assert_eq!(cache_namespaces.len(), 1);
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
    let stopped = actor.call(&fixture, "ide.stop", json!({})).await;
    // The provider never created its socket, so `close_provider` cannot prove the on-disk socket's
    // fate; it still kills and reaps the direct child below but reports the cleanup honestly instead
    // of a false "stop" success.
    assert_eq!(stopped["code"], "internal", "{stopped}");
    assert_eq!(stopped["state"], "error", "{stopped}");
    assert!(cache_namespaces[0].is_dir());
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
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"signal-fixture-provider"),"settings":"gopls_defaults","toolchain":"/usr/bin/true","cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"signal-fixture-cache"}]));
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
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s' $$ > '{}'\n: > '{}'\nexec /bin/sleep 30\n",
            process.display(),
            ready.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.write_config(json!([{"executable":accepted_program(program.to_str().unwrap(),"rust-analyzer signal fixture"),"settings":"rust_cache_priming_disabled_v1","toolchain":"stable","cargo_version":"cargo 1.98.1","rustc_version":"rustc 1.98.1","trust":"fixture-disabled","cache_namespace":"signal-rust-cache"}]));
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

/// Two configured root/child channels keep one compatible listener while isolating current source and stop.
#[tokio::test]
#[ignore = "requires accepted AGENT_IDE_GOPLS and AGENT_IDE_GO environment"]
async fn configured_product_shares_go_across_two_exact_actors_without_crossing_views() {
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
    fixture.write_config(json!([{"executable":accepted_program(wrapper.to_str().unwrap(),"golang.org/x/tools/gopls v0.23.0"),"settings":"gopls_defaults","toolchain":go,"cargo_version":null,"rustc_version":null,"trust":"fixture-disabled","cache_namespace":"shared-fixture-cache"}]));
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
    let stop = root.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stop["kind"], "stop", "{stop}");
    let live = child
        .call(
            &fixture,
            "ide.context",
            json!({"path":"main.go","byte_offset":child_offset}),
        )
        .await;
    let live = child.settle(&fixture, live).await;
    assert!(live["text"].as_str().unwrap().contains("mode: semantic"));
    let stop = child.call(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stop["kind"], "stop", "{stop}");
    tokio::join!(root.mcp.close(), child.mcp.close());
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
