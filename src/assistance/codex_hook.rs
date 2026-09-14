//! Fail-open native host hook entrypoint with a deadline independent of MCP calls.

use super::{
    facade::{HookIngressOutcome, TrustedTransport, render_hook_context, submit_hook_event},
    host_binding::{HostKind, parse_claude_hook_event, parse_hook_event},
};
use std::{io::Read, path::Path, time::Duration};

/// Caps raw host input before parsing; discarded fields are never sent to the daemon.
const MAX_INPUT_BYTES: u64 = 64 * 1024;
/// Bounds stdin, parsing, connect and reply together, including a silent or stuck host pipe.
const TOTAL_DEADLINE: Duration = Duration::from_millis(250);

/// Reads one bounded native payload and submits only selected identity fields, without output.
///
/// Missing/invalid launcher attachment, malformed input, absent daemon and deadline expiry all
/// return normally. The detached reader cannot delay process exit if stdin remains open. This
/// function never creates hook-specific daemon state, retries, autostarts, or changes native tool
/// permission. Telemetry is owned by the already-running daemon, so this ingress never opens
/// SQLite, contends with its writer, or creates a database.
pub async fn run(runtime_dir: &Path, attachment: Option<String>, host_kind: HostKind) {
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
    if let Some((event, HookIngressOutcome::Feedback(text))) = hook
        && let Some(output) = render_hook_context(&event, &text)
    {
        println!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proves hook ingress does not open or wait on SQLite, even while a local database is locked.
    #[tokio::test]
    async fn locked_sqlite_cannot_extend_the_hook_deadline() {
        let runtime =
            std::env::temp_dir().join(format!("agent-ide-hook-telemetry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir(&runtime).unwrap();
        let database = runtime.join("state.sqlite");
        let lock = rusqlite::Connection::open(&database).unwrap();
        lock.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let started = std::time::Instant::now();
        run(&runtime, None, HostKind::Codex).await;
        assert!(started.elapsed() < TOTAL_DEADLINE);
        drop(lock);
        let _ = std::fs::remove_file(database);
        let _ = std::fs::remove_dir(runtime);
    }
}
