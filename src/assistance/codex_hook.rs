//! Fail-open native host hook entrypoint with a deadline independent of MCP calls.

use super::{
    facade::{HookIngressOutcome, TrustedTransport, render_hook_context, submit_hook_event},
    host_binding::{HostKind, parse_claude_hook_event, parse_hook_event},
};
use crate::{
    app::{config::EffectiveConfig, store::Store},
    telemetry::{Telemetry, TelemetryConfig, adapters},
};
use std::{io::Read, path::Path, sync::Arc, time::Duration};

/// Caps raw host input before parsing; discarded fields are never sent to the daemon.
const MAX_INPUT_BYTES: u64 = 64 * 1024;
/// Bounds stdin, parsing, connect and reply together, including a silent or stuck host pipe.
const TOTAL_DEADLINE: Duration = Duration::from_millis(250);

/// Reads one bounded native payload and submits only selected identity fields, without output.
///
/// Missing/invalid launcher attachment, malformed input, absent daemon and deadline expiry all
/// return normally. The detached reader cannot delay process exit if stdin remains open. This
/// function never creates hook-specific daemon state, retries, autostarts, or changes native tool
/// permission. It may asynchronously record an unavailable outcome in an existing local telemetry
/// database, which never changes the hook result or creates a database.
pub async fn run(runtime_dir: &Path, attachment: Option<String>, host_kind: HostKind) {
    let telemetry = open_hook_telemetry(runtime_dir);
    let hook = tokio::time::timeout(TOTAL_DEADLINE, async {
        let attachment = attachment?;
        TrustedTransport::from_host_ingress("hook", "hook", attachment.clone())?;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("codex-hook-input".into())
            .spawn(move || {
                let mut payload = Vec::new();
                let result = std::io::stdin()
                    .take(MAX_INPUT_BYTES + 1)
                    .read_to_end(&mut payload);
                let _ = sender.send(result.ok().map(|_| payload));
            })
            .ok()?;
        let payload = receiver.await.ok()??;
        let event = match host_kind {
            HostKind::Codex => parse_hook_event(&payload),
            HostKind::Claude => parse_claude_hook_event(&payload),
        }
        .ok()?;
        let correlation = event.optional_call_id().unwrap_or("post-tool-batch");
        let host = TrustedTransport::from_host_ingress(correlation, correlation, attachment)?;
        let outcome = submit_hook_event(runtime_dir, &host, &event).await;
        Some((event, outcome))
    })
    .await
    .ok()
    .flatten();
    let outcome = hook
        .as_ref()
        .map(|(_, outcome)| outcome)
        .unwrap_or(&HookIngressOutcome::Unavailable);
    if telemetry.is_finished() {
        if let Ok(Some(telemetry)) = telemetry.await {
            adapters::hook_result(&telemetry, outcome);
        }
    } else {
        telemetry.abort();
    }
    if let Some((event, HookIngressOutcome::Feedback(text))) = hook
        && let Some(output) = render_hook_context(&event, &text)
    {
        println!("{output}");
    }
}

/// Opens an existing optional hook-local telemetry owner concurrently with native ingress.
///
/// The task is never awaited unless it already completed inside the hook's existing deadline, so
/// SQLite opening, migration, or telemetry failure cannot delay stdin handling, transport, native
/// fallback, or feedback output. An absent database is never created by a hook. The runtime
/// directory comes from the trusted launcher rather than a hook payload; no hook field reaches the
/// database path or the telemetry event.
fn open_hook_telemetry(runtime_dir: &Path) -> tokio::task::JoinHandle<Option<Telemetry>> {
    let database = runtime_dir.join("telemetry.sqlite");
    if !database.is_file() {
        return tokio::spawn(async { None });
    }
    tokio::spawn(async move {
        let store = tokio::task::spawn_blocking(move || {
            Store::open(&database, EffectiveConfig::defaults().store()).ok()
        })
        .await
        .ok()??;
        Telemetry::open(Arc::new(store), TelemetryConfig::default())
            .await
            .ok()
    })
}
