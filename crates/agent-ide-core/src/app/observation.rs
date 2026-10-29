//! Fixed read-only projection of retained checks over the existing same-user socket.

use super::{AssistanceDispatcher, DoctorEndpointState, DoctorRuntimeState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use tokio::net::UnixStream;

/// Maximum summaries per page; diagnostics and source bytes are never included.
pub const PAGE_ROWS: usize = 16;
/// Maximum encoded snapshot-array bytes, leaving room for the fixed envelope and telemetry.
pub const PAGE_BYTES: usize = 128 * 1024;
/// Maximum UTF-8 bytes of an accepted identity or source path.
pub const MAX_PATH_BYTES: usize = 4096;

/// Retained scheduler evidence with no actor, credential, diagnostic message, or process data.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CheckSummary {
    /// Digest identifying this retained result within its daemon instance.
    pub snapshot_id: String,
    /// Canonical worktree that produced the result, never inferred from a repository name.
    pub source_path: String,
    /// Registered language identifier.
    pub language: String,
    /// Existing closed check state, including partial and unavailable reasons.
    pub state: crate::checks::CheckState,
    /// Full snapshot error count; unavailable never implies passing checks.
    pub errors: u32,
    /// Full snapshot warning count.
    pub warnings: u32,
    /// Original scheduler input generation.
    pub input_generation: u64,
    /// Original publication time in Unix milliseconds; absent for legacy evidence.
    pub observed_at_ms: Option<u64>,
    /// `stale` for known newer inputs, otherwise `unknown`; observation never checks files.
    pub freshness: String,
    /// Whether a timer or check is replacing this result.
    pub rechecking: bool,
}

/// Bounded retained evidence; missing data is explicitly unavailable.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChecksPage {
    /// `available` only when a scheduler answered; empty snapshots remain unknown coverage.
    pub availability: String,
    /// Safe summaries ordered by canonical worktree and language.
    pub snapshots: Vec<CheckSummary>,
    /// `partial` for another page, `complete` for retained rows, `unknown` without evidence.
    pub coverage: String,
    /// Offset to continue; successive pages are not an atomic history.
    pub next_cursor: Option<u64>,
}

/// Privacy-safe counts of at most 100 retained telemetry rows from the existing owner.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelemetrySummary {
    /// `available` only when the existing owner returned a bounded page.
    pub availability: String,
    /// Counts keyed only by the six closed telemetry event tags.
    pub counts: std::collections::BTreeMap<String, u64>,
    /// `partial` when more events exist, `unknown` when evidence is absent.
    pub coverage: String,
    /// Original durable sequence to continue through the existing telemetry query command.
    pub next_cursor: Option<u64>,
    /// Writer's known drop count, or null when no writer observation is available.
    pub dropped: Option<u64>,
    /// Always null: existing telemetry rows contain no original observation timestamp.
    pub observed_at_ms: Option<u64>,
}

impl TelemetrySummary {
    /// Describes missing telemetry without inventing zero event or drop history.
    pub fn unavailable() -> Self {
        Self {
            availability: "unavailable".into(),
            counts: Default::default(),
            coverage: "unknown".into(),
            next_cursor: None,
            dropped: None,
            observed_at_ms: None,
        }
    }
}

/// Fixed read-only owner reply; fields cannot carry source, process arguments, or host credentials.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Evidence {
    /// Retained scheduler evidence.
    pub checks: ChecksPage,
    /// Bounded telemetry counts without event payloads.
    pub telemetry: TelemetrySummary,
}

impl Evidence {
    /// Returns unavailable evidence when an existing owner cannot answer.
    pub fn unavailable() -> Self {
        Self {
            checks: ChecksPage::unavailable(),
            telemetry: TelemetrySummary::unavailable(),
        }
    }
}

impl ChecksPage {
    /// Returns no fabricated success when an absent, old, failed, or unconfigured owner cannot answer.
    pub fn unavailable() -> Self {
        Self {
            availability: "unavailable".into(),
            snapshots: Vec::new(),
            coverage: "unknown".into(),
            next_cursor: None,
        }
    }
}

/// Reads a fixed reply from an existing private endpoint within two seconds total.
/// Never acquires a lease, marks activity, starts work, creates files, or opens databases.
/// Missing, old, or invalid peers return unavailable without falling back to mutation.
pub async fn query(runtime: &Path, common_dir: &str, cursor: u64) -> Evidence {
    if common_dir.len() > MAX_PATH_BYTES {
        return Evidence::unavailable();
    }
    let (directory, endpoint, _) = super::inspect_runtime(runtime);
    if (directory, endpoint) != (DoctorRuntimeState::Private, DoctorEndpointState::Socket) {
        return Evidence::unavailable();
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut stream = UnixStream::connect(runtime.join(super::SOCKET_NAME)).await?;
        if super::peer_uid(std::os::fd::AsRawFd::as_raw_fd(&stream))? != super::effective_uid() {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        super::write_frame(
            &mut stream,
            &json!({
                "version":1,"method":"observation","request_id":"observe",
                "common_dir":common_dir,"cursor":cursor
            }),
            super::MAX_V2_FRAME_BYTES,
        )
        .await?;
        let reply: Value = super::read_frame(&mut stream, super::MAX_V2_FRAME_BYTES).await?;
        if reply["version"] != 1 || reply["request_id"] != "observe" || reply["status"] != "ok" {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        let evidence: Evidence = serde_json::from_value(reply["evidence"].clone())?;
        let page = &evidence.checks;
        if page.snapshots.len() > PAGE_ROWS
            || page.snapshots.iter().any(|row| {
                row.source_path.len() > MAX_PATH_BYTES
                    || row.language.len() > 128
                    || row.snapshot_id.len() > 128
                    || !matches!(row.freshness.as_str(), "stale" | "unknown")
            })
            || !matches!(page.availability.as_str(), "available" | "unavailable")
            || !matches!(page.coverage.as_str(), "complete" | "partial" | "unknown")
        {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        if evidence.telemetry.counts.len() > 6
            || evidence.telemetry.counts.iter().any(|(tag, count)| {
                !matches!(
                    tag.as_str(),
                    "tool_completed"
                        | "execution_completed"
                        | "provider_observed"
                        | "native_fallback"
                        | "project_check_completed"
                        | "name_index_refreshed"
                ) || *count > 100
            })
            || !matches!(
                evidence.telemetry.availability.as_str(),
                "available" | "unavailable"
            )
            || !matches!(
                evidence.telemetry.coverage.as_str(),
                "complete" | "partial" | "unknown"
            )
        {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
        }
        Ok::<_, std::io::Error>(evidence)
    })
    .await;
    match result {
        Ok(Ok(evidence)) => evidence,
        _ => Evidence::unavailable(),
    }
}

/// Answers a same-user v1 observation without dispatching work or touching lease activity.
/// Malformed identity/cursor inputs are refused silently like malformed health requests.
pub(super) async fn serve(
    stream: &mut UnixStream,
    request: &Value,
    dispatcher: Option<&dyn AssistanceDispatcher>,
) -> std::io::Result<()> {
    let Some(common_dir) = request["common_dir"]
        .as_str()
        .filter(|path| path.len() <= MAX_PATH_BYTES && Path::new(path).is_absolute())
    else {
        return Ok(());
    };
    let Some(cursor) = request["cursor"].as_u64() else {
        return Ok(());
    };
    let Some(request_id) = request["request_id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= super::MAX_REQUEST_ID_BYTES)
    else {
        return Ok(());
    };
    let checks = dispatcher
        .filter(|owner| !owner.is_failed())
        .map_or_else(ChecksPage::unavailable, |owner| {
            owner.observe_checks(common_dir, cursor)
        });
    let telemetry = match dispatcher.filter(|owner| !owner.is_failed()) {
        Some(owner) => owner.observe_telemetry().await,
        None => TelemetrySummary::unavailable(),
    };
    super::write_frame(stream, &json!({
        "version":1,"request_id":request_id,"status":"ok","evidence":Evidence { checks, telemetry }
    }), super::MAX_V2_FRAME_BYTES).await
}

#[cfg(test)]
/// Socket fixtures and missing-path checks confined to the caller's task temp directory.
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    /// Missing, unsafe, and corrupt endpoints stay unavailable without creating/repairing paths,
    /// locks, leases, actors, stores, cache roots, or checks on repeated opens.
    #[tokio::test]
    async fn observation_missing_unsafe_and_corrupt_are_read_only() {
        let root = crate::scratch::ScratchDir::new("observe-endpoint");
        let missing = root.join("missing");
        for _ in 0..2 {
            assert_eq!(
                query(&missing, "/fixture/common", 0).await.checks.coverage,
                "unknown"
            );
        }
        assert!(!missing.exists());
        let denied = root.join("denied");
        fs::create_dir(&denied).unwrap();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            query(&denied, "/fixture/common", 0)
                .await
                .checks
                .availability,
            "unavailable"
        );
        assert_eq!(fs::read_dir(&denied).unwrap().count(), 0);
        let runtime = root.join("runtime");
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(runtime.join("sentinel"), b"retained").unwrap();
        let listener =
            tokio::net::UnixListener::bind(runtime.join(super::super::SOCKET_NAME)).unwrap();
        let before = fs::metadata(&runtime).unwrap().modified().unwrap();
        let fake = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: Value = super::super::read_frame(&mut stream, super::super::MAX_V2_FRAME_BYTES)
                .await
                .unwrap();
            super::super::write_frame(
                &mut stream,
                &json!({"version":1,"status":"ok",
                "request_id":"wrong","evidence":{}}),
                super::super::MAX_V2_FRAME_BYTES,
            )
            .await
            .unwrap();
        };
        let (evidence, ()) = tokio::join!(query(&runtime, "/fixture/common", 0), fake);
        assert_eq!(evidence.checks.availability, "unavailable");
        assert_eq!(evidence.telemetry.availability, "unavailable");
        assert_eq!(fs::read(runtime.join("sentinel")).unwrap(), b"retained");
        assert_eq!(fs::read_dir(&runtime).unwrap().count(), 2);
        assert_eq!(fs::metadata(&runtime).unwrap().modified().unwrap(), before);
    }
}
