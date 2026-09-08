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
        ("ide.context", json!({"query":"source"})),
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
