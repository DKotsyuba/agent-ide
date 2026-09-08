//! Executable MCP roundtrips for static discovery, separated ingress, and finite daemon routing.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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

/// Captures the real hook IPC frame while withholding a reply, proving sanitization and total deadline.
#[tokio::test]
async fn binary_codex_hook_hung_daemon_deadline_sends_only_selected_fields() {
    use tokio::{io::AsyncReadExt, net::UnixListener};
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
            json!({"phase":"pre","actor_id":"child","call_id":"hung"})
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
            &json!({"phase":"post","actor_id":actor,"call_id":call}),
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
    use tokio::{io::AsyncReadExt, net::UnixListener};
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
