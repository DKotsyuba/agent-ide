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
                "session_id":null,"agent_type":null,"launch_command":null,"launch_background":null,"failed":false})
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
        assert!(!reply["result"]["structuredContent"].is_null(), "{reply}");
        reply["result"]["structuredContent"].clone()
    }
    /// Runs the exact foreground helper named by a pending reply and returns its owned handle.
    async fn launch_claude_pending(&self, fixture: &ProductFixture, pending: &Value) -> String {
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
    /// empty hook output; Context may produce the actual bounded `additionalContext` delta.
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
        assert!(!reply["result"]["structuredContent"].is_null(), "{reply}");
        (
            reply["result"]["structuredContent"].clone(),
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
    assert!(
        reply["result"]["structuredContent"].is_null(),
        "an unknown attachment must not produce an owner result: {reply}"
    );
    assert_eq!(reply["result"]["isError"], json!(true), "{reply}");
    // Allocation is checked by capacity: enough distinct stranger actors to fill the bounded
    // binding table are sent, so an ingress that minted a channel and generation for each of them
    // would leave no room for the configured actor below.
    for index in 0..MAX_HOST_BINDINGS {
        stranger.next += 1;
        let actor = format!("stranger-{index}");
        let reply=stranger.mcp.exchange(json!({"jsonrpc":"2.0","id":stranger.next,"method":"tools/call","params":{"name":"ide.start","arguments":{"activation_id":"stranger-start"},"_meta":{"threadId":actor,"callId":format!("call-{index}"),"x-codex-turn-metadata":{},"codex/sandbox-state-meta":stranger.state}}})).await;
        assert!(
            reply["result"]["structuredContent"].is_null(),
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
        .call(&fixture, "ide.inspect", json!({"detail_ref":reopened_ref}))
        .await;
    assert_eq!(after_edit["state"], "error", "{after_edit}");
    assert_eq!(after_edit["code"], "source_unavailable", "{after_edit}");

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

/// Drives one real foreground Claude helper launch end to end: `ide.start` returns the exact
/// helper instruction, a native Bash pre-hook arms it, the fixed helper subcommand runs as a real
/// process and claims it over the private socket, a matching Bash post-hook settles it, and
/// `ide.inspect` is the only route that ever publishes a durable Workspace row from that evidence.
///
/// The accepted `claude_profile` here is the fixture's test-only disabled launcher wiring proof,
/// not a real host sandbox measurement: it only proves the daemon→hook→helper→daemon correlation
/// and admission plumbing settle correctly, never that a live Claude host actually contained the
/// helper.
#[tokio::test]
async fn configured_product_claude_helper_activates_and_conflicts_a_second_actor_then_stops() {
    /// Runs one complete Claude `ide.start` round trip: mint, arm, real helper process, settle.
    async fn claude_start(actor: &mut ProductActor, fixture: &ProductFixture) -> Value {
        let pending = actor
            .call_claude(fixture, "ide.start", json!({"activation_id":"start"}))
            .await;
        assert_eq!(pending["state"], "pending", "{pending}");
        let detail_ref = pending["detail_ref"].as_str().unwrap().to_owned();
        let helper = pending["helper"].as_str().unwrap().to_owned();
        assert!(helper.contains("claude-worker"), "{helper}");

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
    let mut daemon = fixture.daemon().await;
    let mut first = ProductActor::new(&fixture, "claude-first").await;

    let started = claude_start(&mut first, &fixture).await;
    assert_eq!(started["kind"], "activation", "{started}");
    let detail_ref = started["detail_ref"].as_str().unwrap().to_owned();
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
    let reread = claude_start(&mut first, &fixture).await;
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
    let conflicted = claude_start(&mut second, &fixture).await;
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

    let still_usable = claude_start(&mut first, &fixture).await;
    assert_eq!(still_usable, started, "{still_usable}");

    let stopped = first.call_claude(&fixture, "ide.stop", json!({})).await;
    assert_eq!(stopped["kind"], "stop", "{stopped}");
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
    let stale_detail = first
        .mcp
        .exchange(json!({"jsonrpc":"2.0","id":first.next,"method":"tools/call","params":{
            "name":"ide.inspect","arguments":{"detail_ref":detail_ref},"_meta":{"claudecode/toolUseId":stale_call}}}))
        .await;
    first
        .claude_lifecycle(&fixture, "PostToolUse", &stale_call)
        .await;
    assert_eq!(stale_detail["result"]["isError"], true, "{stale_detail}");
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

/// Proves Claude Context and Diff execute in the real foreground helper and publish one hook delta.
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
    actor
        .claude_lifecycle(&fixture, "PreToolUse", "native-edit-after-context")
        .await;
    let feedback = actor
        .claude_lifecycle_output(&fixture, "PostToolUse", "native-edit-after-context")
        .await;
    assert!(feedback.status.success() && feedback.stderr.is_empty());
    let feedback: Value = serde_json::from_slice(&feedback.stdout).unwrap();
    let additional = feedback["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        additional.contains("Provider reported 1 diagnostics"),
        "{additional}"
    );
    assert!(
        additional.contains("provisional helper snapshot"),
        "{additional}"
    );

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
