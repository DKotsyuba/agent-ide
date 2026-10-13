//! Tiny generic MCP frontend demonstrating SDK onboarding without a framework.
//! This probe synchronously registers then publishes; real producers persist
//! request/operation/route/event intent with their own admission/outcome journal.
use bridge_sdk::bridge_protocol::*;
use bridge_sdk::{Client, Config, extract_native_evidence};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Read, Write},
    time::Duration,
};

/// Runs a single configurable generic tool preserving original MCP _meta.
/// Args: registration socket, core socket, private credential file, native host
/// (`codex` or `claude_code`), opaque tool name. Stdout contains only MCP frames;
/// neither payload nor credential is logged. Errors are secret-safe class text.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 {
        return Err(
            "expected registration/core sockets, credential file, host and tool name".into(),
        );
    }
    let host: Host = serde_json::from_value(Value::String(args[4].clone()))?;
    let credential = std::fs::read_to_string(&args[3])?;
    let client = Client::new(Config::new(
        &args[1],
        &args[2],
        Secret::new(credential.trim_end()),
        Duration::from_secs(5),
    )?)?;
    let mut input = io::stdin().lock();
    loop {
        let mut line = String::new();
        let read = input
            .by_ref()
            .take((MAX_REQUEST_BYTES + 1) as u64)
            .read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if !line.ends_with("\n") {
            return Err("incomplete MCP frame".into());
        }
        if line.len() > MAX_REQUEST_BYTES {
            return Err("MCP frame too large".into());
        }
        let rpc: Value = serde_json::from_str(&line)?;
        let Some(id) = rpc.get("id") else {
            continue;
        };
        let result = match rpc.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"generic-sdk-producer","version":"0.2.0"}})
            }
            Some("ping") => json!({}),
            Some("tools/list") => {
                json!({"tools":[{"name":args[5],"description":"Register the actual native call and publish an opaque result via Bridge","inputSchema":{"type":"object","properties":{"operation_key":{"type":"string"},"registration_request_id":{"type":"string"},"publication_request_id":{"type":"string"},"event_id":{"type":"string"},"kind":{"type":"string"},"payload":{"type":"string"},"mode":{"type":"string","enum":["message","inject","auto"]}},"required":["operation_key","registration_request_id","publication_request_id","event_id","kind","payload"],"additionalProperties":false}}]})
            }
            Some("tools/call") => {
                let p = rpc.get("params").ok_or("missing MCP params")?;
                if p.get("name").and_then(Value::as_str) != Some(args[5].as_str()) {
                    return Err("unsupported tool".into());
                }
                let a = p.get("arguments").ok_or("missing arguments")?;
                let evidence = extract_native_evidence(
                    host,
                    p.get("_meta").ok_or("missing protected _meta")?,
                )?;
                let get = |key| {
                    a.get(key)
                        .and_then(Value::as_str)
                        .ok_or("missing bounded argument")
                };
                let mode: Mode =
                    serde_json::from_value(a.get("mode").cloned().unwrap_or(json!("message")))?;
                let registration = client.register_call(
                    get("registration_request_id")?,
                    RegisterCallParams {
                        operation_key: get("operation_key")?.into(),
                        evidence: Some(evidence),
                        reply_policy: ReplyPolicy::SelfActor,
                        allowed_modes: Some(vec![mode]),
                        allowed_kinds: Some(vec![get("kind")?.into()]),
                        max_payload_bytes: None,
                        route_expires_at_ms: None,
                        assertion_audience: None,
                        invocation_key: None,
                    },
                );
                match registration {
                    Ok(r) => match client.publish(
                        get("publication_request_id")?,
                        PublishParams {
                            destination: Destination::Route {
                                route_key: r.receipt.route_key.clone(),
                            },
                            event_id: get("event_id")?.into(),
                            kind: get("kind")?.into(),
                            payload: Payload::new(get("payload")?)?,
                            mode,
                            policy: Policy::KeepAll,
                            metadata: Metadata::new(),
                            expires_at_ms: None,
                            delivery_scope_key: None,
                        },
                    ) {
                        Ok(p) => {
                            json!({"content":[{"type":"text","text":serde_json::to_string(&json!({"registration":r.receipt,"custody":p}))?}]})
                        }
                        Err(e) => {
                            json!({"isError":true,"content":[{"type":"text","text":e.to_string()}]})
                        }
                    },
                    Err(e) => {
                        json!({"isError":true,"content":[{"type":"text","text":e.to_string()}]})
                    }
                }
            }
            _ => json!({}),
        };
        serde_json::to_writer(
            io::stdout().lock(),
            &json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?;
        println!();
        io::stdout().flush()?;
    }
    Ok(())
}
