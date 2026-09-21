//! In-process checks that typed failure replies and check starts reach the error log without any
//! telemetry sink (the writer is process-global, so this lives in its own test binary).

use std::time::Duration;

use agent_ide::assistance::facade::AssistanceTool;
use agent_ide::assistance::reply::{FailureCode, PeerReply};
use agent_ide::checks::scheduler::Scheduler;
use agent_ide::checks::{CheckState, FakeChecker, Language, ProblemSnapshot};
use agent_ide::errorlog;
use agent_ide::telemetry::adapters;

/// Every typed `Error` reply is logged once with its own reason code, and a `source_too_large`
/// reply settled through `ide.inspect` is one of them; no telemetry is involved.
#[tokio::test(start_paused = true)]
async fn typed_failures_and_check_starts_are_logged_without_telemetry() {
    let key = format!(
        "{:016x}",
        std::process::id() as u64 * 7919 + 0xabc0_0000_0000
    );
    errorlog::init_repository(&key);
    let dir = errorlog::log_root().unwrap().join(&key);
    let _ = std::fs::remove_dir_all(&dir);

    let failures = [
        (
            FailureCode::SourceTooLarge {
                size: 9,
                ceiling: 4,
            },
            "source_too_large",
        ),
        (FailureCode::SourceUnavailable, "source_unavailable"),
        (FailureCode::InvalidDetail, "invalid_detail"),
        (FailureCode::Capacity, "capacity"),
        (FailureCode::Internal, "internal"),
    ];
    for (code, _) in failures {
        adapters::log_tool_reply(
            AssistanceTool::Inspect,
            &PeerReply::Error { code },
            Duration::from_millis(3),
        );
    }
    let logged = errorlog::read_events(&dir);
    for (index, (_, reason)) in failures.iter().enumerate() {
        assert_eq!(logged[index].method, "inspect");
        assert_eq!(logged[index].reason.as_deref(), Some(*reason));
    }
    assert_eq!(
        logged.len(),
        failures.len(),
        "each reply is logged exactly once"
    );
    assert_eq!(logged[0].outcome, "failed");

    // A scheduler dispatch logs `check started` before the check runs.
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("agent-ide-elog-check-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let worktree = root.join("wt");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join("Cargo.toml"), "[package]\n").unwrap();
    let snapshot =
        ProblemSnapshot::from_problems(Language::Rust, CheckState::Ready, Vec::new(), 1, 0);
    let scheduler = Scheduler::new(
        vec![std::sync::Arc::new(FakeChecker::new(
            Language::Rust,
            snapshot,
        ))],
        Duration::from_millis(10),
        1,
        root.join("cache"),
    );
    scheduler.trigger("repo", &worktree);
    for _ in 0..20 {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(10)).await;
    }
    let started = errorlog::read_events(&dir)
        .into_iter()
        .filter(|event| event.method == "check" && event.outcome == "started")
        .collect::<Vec<_>>();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].detail.as_deref(), Some("rust"));
    assert_eq!(started[0].worktree.as_deref(), worktree.to_str(),);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&dir);
}
