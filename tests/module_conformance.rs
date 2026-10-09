//! Module host conformance: real bundled module processes started through Execution admission by
//! the daemon's routing ([`ModuleHost`]), driven into every transport and process fault through
//! the module fault seam. Each fault settles its call with a typed `module_unavailable`, the next
//! call restarts within the budget, and no process survives. Needs a `test-seams` build.
#![cfg(feature = "test-seams")]

#[path = "support/parity.rs"]
mod parity;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_ide_core::{
    checks::{CheckRequest, Checker, runner::AdmittedRunner},
    execution::{AdmissionController, AdmissionLimits},
    modules::{
        checker::ModuleChecker,
        contract::{Capability, Cause, ModuleUnavailable, Stage},
        launch::ModuleExecutable,
        payload::{
            Arg, EffectRecipe, ExecutableSlot, FileDocRequest, RunClass, SlotSource, SourceRef,
            SourceText, Stdin, encode,
        },
        recipe,
        router::ModuleHost,
        serve::{FAULT_SEAM, FIXTURE_SEAM},
    },
};

/// Serializes the tests: each counts this process's module children.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A scratch worktree removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    /// Creates `module-conformance-<tag>` under the temporary directory.
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("module-conformance-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    /// Removes the tree.
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One admission controller for a test's modules and jobs.
fn admission() -> Arc<Mutex<AdmissionController>> {
    Arc::new(Mutex::new(
        AdmissionController::new(AdmissionLimits {
            total_running: 4,
            per_owner_running: 4,
            per_owner_queued: 1,
            total_queued: 1,
            interactive_burst: 1,
        })
        .unwrap(),
    ))
}

/// A host routing the style-sheet language to its real module under `admission`, with `env` in
/// the module environment and a request `budget`.
fn host_with(
    admission: Arc<Mutex<AdmissionController>>,
    env: Vec<(String, String)>,
    budget: Duration,
) -> ModuleHost {
    agent_ide::languages::install();
    let executable = ModuleExecutable::pin(&parity::binary().canonicalize().unwrap()).unwrap();
    ModuleHost::with_parts(executable, admission, &["css"], env, budget)
}

/// A host routing the style-sheet language to its real module, with the fault `seam` in the
/// module environment and a short request budget.
fn host(seam: Option<String>) -> ModuleHost {
    host_with(
        admission(),
        seam.map(|value| vec![(FAULT_SEAM.to_owned(), value)])
            .unwrap_or_default(),
        Duration::from_millis(1500),
    )
}

/// One file-doc request for a small style sheet.
async fn file_doc(host: &ModuleHost, worktree: &Path) -> Result<Option<String>, ModuleUnavailable> {
    host.request(
        agent_ide::languages::CSS,
        worktree,
        Capability::FileDoc,
        encode(&FileDocRequest {
            source: SourceRef {
                path: "a.css".into(),
                revision: "r1".into(),
                text: SourceText::Inline("/* Buttons. */\n.btn {}\n".into()),
            },
        }),
        Vec::new(),
    )
    .await
}

/// The style-sheet language's own in-process answer to [`file_doc`].
fn in_process() -> Option<String> {
    agent_ide::languages::CSS
        .support()
        .file_doc("/* Buttons. */\n.btn {}\n")
}

/// The live `module css analyzer` children of this test process.
fn modules() -> Vec<parity::ProcessIdentity> {
    parity::ProcessIdentity::children_of(std::process::id() as libc::pid_t)
        .into_iter()
        .filter(|id| id.command().ends_with("module css analyzer"))
        .collect()
}

/// The live module children of this test process, in any role.
fn any_modules() -> Vec<parity::ProcessIdentity> {
    parity::ProcessIdentity::children_of(std::process::id() as libc::pid_t)
        .into_iter()
        .filter(|id| id.command().contains("module css "))
        .collect()
}

/// Waits until no module child of this process remains.
async fn no_modules_left() -> bool {
    for _ in 0..250 {
        if any_modules().is_empty() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// Every transport fault on a live instance (exit, stall, malformed, oversized, wrong fence,
/// late reply, truncated attachment) and an exit before `hello` settles its call with the typed
/// cause; the next call restarts a fresh instance and answers; the slot stops with nothing left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transport_faults_are_typed_and_recover() {
    let _serial = SERIAL.lock().await;
    let expected = parity::ProcessIdentity::children_of(std::process::id() as libc::pid_t).len();
    for (kind, target, stage, cause) in [
        ("exit", "file_doc", Stage::Request, Cause::Exited),
        ("stall", "file_doc", Stage::Request, Cause::Timeout),
        ("malformed", "file_doc", Stage::Request, Cause::Malformed),
        ("oversize", "file_doc", Stage::Request, Cause::Oversized),
        ("wrong-fence", "file_doc", Stage::Request, Cause::WrongFence),
        ("late", "file_doc", Stage::Request, Cause::Timeout),
        ("truncate", "file_doc", Stage::Decode, Cause::Exited),
        ("exit", "hello", Stage::Hello, Cause::Exited),
    ] {
        let scratch = Scratch::new(&format!("{kind}-{target}"));
        let flag = scratch.0.join("flag");
        std::fs::write(&flag, "").unwrap();
        let host = host(Some(format!("{kind}:{target}:{}", flag.display())));
        let error = file_doc(&host, &scratch.0).await.unwrap_err();
        assert_eq!(
            (error.stage, error.cause),
            (stage, cause),
            "{kind}:{target}: {error}"
        );
        assert_eq!(error.module_id.as_str(), "bundled.css");
        let answer = file_doc(&host, &scratch.0).await;
        assert_eq!(
            answer,
            Ok(in_process()),
            "{kind}:{target} recovers with the in-process answer"
        );
        host.stop_all().await;
        assert!(no_modules_left().await, "{kind}:{target} left a module");
    }
    assert_eq!(
        parity::ProcessIdentity::children_of(std::process::id() as libc::pid_t).len(),
        expected
    );
}

/// A module killed while idle is noticed before the next call, which restarts and answers; a
/// stderr flood is drained without affecting the reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_kill_and_stderr_flood() {
    let _serial = SERIAL.lock().await;
    let scratch = Scratch::new("idle");
    let flag = scratch.0.join("flag");
    std::fs::write(&flag, "").unwrap();
    let host = host(Some(format!("stderr-flood:file_doc:{}", flag.display())));
    assert_eq!(
        file_doc(&host, &scratch.0).await,
        Ok(in_process()),
        "flooded call answers"
    );
    let first = modules();
    assert_eq!(first.len(), 1);
    // SAFETY: the module is this test's own direct child, identified a moment ago.
    unsafe { libc::kill(first[0].pid, libc::SIGKILL) };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        file_doc(&host, &scratch.0).await.is_ok(),
        "restarts after an idle kill"
    );
    let second = modules();
    assert_eq!(second.len(), 1);
    assert_ne!(second[0], first[0]);
    host.stop_all().await;
    assert!(no_modules_left().await);
}

/// A module that dies leaving a TERM-resistant descendant in its group has the whole group torn
/// down when its instance is retired: the descendant does not outlive it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_leader_takes_its_resistant_descendant_along() {
    let _serial = SERIAL.lock().await;
    let scratch = Scratch::new("orphan");
    let flag = scratch.0.join("flag");
    std::fs::write(&flag, "").unwrap();
    let host = host(Some(format!("orphan:file_doc:{}", flag.display())));
    let error = file_doc(&host, &scratch.0).await.unwrap_err();
    assert_eq!(error.cause, Cause::Exited);
    let pid: libc::pid_t = std::fs::read_to_string(scratch.0.join("flag.pid"))
        .expect("the descendant recorded its pid")
        .trim()
        .parse()
        .unwrap();
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        // SAFETY: signal 0 only checks existence of the recorded descendant.
        while unsafe { libc::kill(pid, 0) } == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "the TERM-resistant descendant was killed with its group"
    );
    host.stop_all().await;
    assert!(no_modules_left().await);
}

/// A crash loop spends the restart budget: after the initial start and three restarts the slot
/// answers `restart_exhausted` with a retry time and starts nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_loop_exhausts_the_restart_budget() {
    let _serial = SERIAL.lock().await;
    let scratch = Scratch::new("loop");
    let flag = scratch.0.join("flag");
    let host = host(Some(format!("exit:file_doc:{}", flag.display())));
    for attempt in 0..4 {
        std::fs::write(&flag, "").unwrap();
        let error = file_doc(&host, &scratch.0).await.unwrap_err();
        assert_eq!(error.cause, Cause::Exited, "attempt {attempt}");
        // Let each backoff (250 ms, 1 s, 4 s) pass so the next attempt really starts.
        tokio::time::sleep(Duration::from_millis([300, 1100, 4100, 0][attempt])).await;
    }
    let error = file_doc(&host, &scratch.0).await.unwrap_err();
    assert_eq!(
        (error.stage, error.cause),
        (Stage::Spawn, Cause::RestartExhausted),
        "{error}"
    );
    assert!(error.retry_after_ms.is_some_and(|ms| ms > 0));
    assert!(no_modules_left().await, "nothing started while exhausted");
    host.stop_all().await;
}

/// The fixture's check recipe: the launcher program `tool` (a shell) writes `bytes` zero bytes,
/// captured up to the job class ceiling of 64 MiB per stream.
const RECIPES: &[EffectRecipe] = &[EffectRecipe {
    id: "check",
    program: "tool",
    args: &[
        Arg::Literal("-c"),
        Arg::Literal("head -c \"$0\" /dev/zero"),
        Arg::Param("bytes"),
    ],
    env: &[],
    paths: &[],
    executables: &[ExecutableSlot {
        name: "tool",
        source: SlotSource::Launcher("tool"),
    }],
    stdin: Stdin::Null,
    class: RunClass::Background,
    timeout_ceiling_ms: 900_000,
    capture_bytes: 64 << 20,
}];

/// Check output of 3 MiB, 9 MiB and exactly the 64 MiB ceiling lands through a real checker
/// module: the module plans the run, the core expands its recipe and runs it as an
/// Execution-owned job, and the raw bytes travel back as attachments the module interprets. Past
/// the ceiling the bytes are a prefix marked truncated. Nothing outlives the host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn large_check_output_lands_through_the_module() {
    let _serial = SERIAL.lock().await;
    recipe::declare(&[("css", RECIPES)]);
    let scratch = Scratch::new("large");
    let admission = admission();
    let host = Arc::new(host_with(
        admission.clone(),
        vec![(FIXTURE_SEAM.to_owned(), "1".to_owned())],
        Duration::from_secs(30),
    ));
    let runner = Arc::new(AdmittedRunner::new(admission.clone(), false));
    for (bytes, truncated) in [
        (3u64 << 20, false),
        (9 << 20, false),
        (64 << 20, false),
        ((64 << 20) + 1, true),
    ] {
        let checker = ModuleChecker::new(
            agent_ide::languages::CSS,
            host.clone(),
            serde_json::json!({
                "programs": [{"name": "tool", "path": "/bin/sh", "interpreter": null}],
                "params": {"tool": {"executable": "tool"}, "bytes": {"scalar": bytes}},
            }),
            runner.clone(),
            Duration::from_secs(120),
        );
        let snapshot = checker
            .check(CheckRequest {
                worktree: scratch.0.clone(),
                cache_dir: scratch.0.join("cache"),
                input_generation: 1,
                read_denies: Vec::new(),
            })
            .await;
        let landed = bytes.min(64 << 20);
        assert_eq!(
            snapshot.detail.as_deref(),
            Some(
                format!(
                    "{landed} output bytes{}",
                    if truncated { ", truncated" } else { "" }
                )
                .as_str()
            ),
            "{bytes} bytes: {snapshot:?}"
        );
    }
    host.stop_all().await;
    assert!(no_modules_left().await);
    assert_eq!(admission.lock().unwrap().running_count(), 0);
}

/// One call in flight and at most eight waiting per instance: a further call is refused at once
/// as `resource_limit`, a waiting call's deadline covers its wait, and a stop cancels the
/// in-flight call and every waiter at once instead of queueing behind them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiters_are_bounded_and_a_stop_cancels_them() {
    let _serial = SERIAL.lock().await;
    let scratch = Scratch::new("queue");
    let flag = scratch.0.join("flag");
    std::fs::write(&flag, "").unwrap();
    let host = Arc::new(host_with(
        admission(),
        vec![(
            FAULT_SEAM.to_owned(),
            format!("stall:file_doc:{}", flag.display()),
        )],
        Duration::from_secs(30),
    ));
    let call = |host: Arc<ModuleHost>, root: PathBuf| {
        tokio::spawn(async move { file_doc(&host, &root).await })
    };
    let stalled = call(host.clone(), scratch.0.clone());
    for _ in 0..250 {
        if !flag.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!flag.exists(), "the first call is in flight and stalls");
    let waiters: Vec<_> = (0..8)
        .map(|_| call(host.clone(), scratch.0.clone()))
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = file_doc(&host, &scratch.0).await.unwrap_err();
    assert_eq!(
        (busy.stage, busy.cause),
        (Stage::Admission, Cause::ResourceLimit),
        "{busy}"
    );
    let started = std::time::Instant::now();
    host.stop_all().await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the stop did not queue behind the calls"
    );
    for task in std::iter::once(stalled).chain(waiters) {
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.cause, Cause::Exited, "{error}");
    }
    assert!(no_modules_left().await);
}
