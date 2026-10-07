//! F-04 contract: a daemon whose connection lanes are full answers a typed `busy` reply instead of
//! dropping the connection, hooks keep a lane of their own, and the refusals are counted in the
//! error journal.
//!
//! These scenarios live in their own test binary because the error journal's writer is a
//! process-wide `OnceLock`: the first daemon of a process fixes where its journal goes, so the
//! journal assertions need a process in which [`journal_home`] runs before any daemon starts.

use std::fs;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use agent_ide::app::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatchUnavailable,
    AssistanceDispatcher, HookSubmit, HookSubmitTransportResult, HookTransportLimits,
    MethodDispatch, MethodDispatchTransportResult, OpaqueJson,
};
use agent_ide::app::{
    RuntimeDir, dispatch_method_if_running, run_daemon_with_assistance, submit_hook_if_running,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

static TEST_ID: AtomicUsize = AtomicUsize::new(0);

/// Fixed finite limits for every v2 transport scenario here.
fn transport_limits() -> HookTransportLimits {
    HookTransportLimits::new(160 * 1024, 144 * 1024, Duration::from_secs(1)).unwrap()
}

/// Creates a unique private directory below the platform temporary directory.
fn runtime_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agent-ide-busy-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

/// Stops an in-process daemon task and removes its test-owned runtime directory.
async fn stop_assistance_daemon(task: tokio::task::JoinHandle<()>, runtime_dir: PathBuf) {
    task.abort();
    let _ = task.await;
    fs::remove_dir_all(runtime_dir).unwrap();
}

/// Sends one complete JSON frame and returns the complete JSON response from the daemon socket.
async fn exchange(runtime_dir: &Path, request: Value) -> Value {
    let mut stream = UnixStream::connect(runtime_dir.join("agent-ide.sock"))
        .await
        .unwrap();
    let body = serde_json::to_vec(&request).unwrap();
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&body).await.unwrap();
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut body = vec![0; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut body).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// Parks selected calls inside the dispatcher until the test releases them.
struct ParkingDispatcher {
    /// Method dispatches that reached the dispatcher.
    methods_entered: Arc<AtomicUsize>,
    /// Hook submissions that reached the dispatcher.
    hooks_entered: Arc<AtomicUsize>,
    /// Parked calls wait for a permit here; the test adds permits to release them.
    release: Arc<tokio::sync::Semaphore>,
    /// Whether hook submissions park as well; method dispatches always park.
    park_hooks: bool,
}

impl AssistanceDispatcher for ParkingDispatcher {
    /// Counts the arrival, parks a method (and a hook when asked), then answers like [`TestDispatcher`].
    fn dispatch(
        &self,
        request: AssistanceDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            match request {
                AssistanceDispatch::HookSubmit(_) => {
                    self.hooks_entered.fetch_add(1, Ordering::SeqCst);
                    if self.park_hooks {
                        let _ = self.release.acquire().await;
                    }
                    Ok(AssistanceDispatchReply::HookSubmit(
                        OpaqueJson::new("{\"hook\":true}", 64 * 1024).unwrap(),
                    ))
                }
                AssistanceDispatch::MethodDispatch(_) => {
                    self.methods_entered.fetch_add(1, Ordering::SeqCst);
                    let _ = self.release.acquire().await;
                    Ok(AssistanceDispatchReply::MethodDispatch(
                        OpaqueJson::new("{\"method\":true}", 64 * 1024).unwrap(),
                    ))
                }
            }
        })
    }
}

/// Starts an in-process daemon with `max_connections` call permits around `dispatcher`.
async fn start_limited_daemon(
    runtime_dir: &Path,
    dispatcher: Arc<ParkingDispatcher>,
    max_connections: usize,
) -> tokio::task::JoinHandle<()> {
    use agent_ide::app::config::{AppConfigPatch, ConfigLayer, ConfigOrigin, effective_config};
    let config = effective_config(
        std::num::NonZeroU64::new(1).unwrap(),
        &[ConfigLayer {
            origin: ConfigOrigin::Host,
            values: AppConfigPatch {
                ipc_max_connections: Some(max_connections),
                ..Default::default()
            },
        }],
    )
    .unwrap();
    let daemon_runtime = RuntimeDir::prepare_for_daemon(runtime_dir).unwrap();
    let task = tokio::spawn(async move {
        run_daemon_with_assistance(
            daemon_runtime,
            dispatcher,
            config,
            agent_ide::app::lease::DEFAULT_IDLE_TIMEOUT,
        )
        .await
        .unwrap();
    });
    let socket = runtime_dir.join("agent-ide.sock");
    for _ in 0..200 {
        if UnixStream::connect(&socket).await.is_ok() {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("limited daemon did not bind its private socket");
}

/// The one journal home of this test process, created and wired on first use.
///
/// The journal writer is initialised once per process, so every test that reads the journal
/// shares this directory and tells its own lines apart by content.
fn journal_home() -> &'static Path {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        let home = runtime_dir();
        // SAFETY: runs once, before the journal writer exists; no test reads this variable
        // concurrently except through the same initialisation.
        unsafe { std::env::set_var(agent_ide::userhome::HOME_OVERRIDE_ENV, &home) };
        agent_ide::errorlog::init_repository("0123456789abcdef");
        home
    })
}

/// Everything the journal of [`journal_home`] holds.
fn journal_text() -> String {
    fs::read_dir(journal_home().join(".agent-ide/logs/0123456789abcdef"))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| fs::read_to_string(entry.path()).unwrap_or_default())
                .collect()
        })
        .unwrap_or_default()
}

/// One context dispatch with a caller-chosen request id.
fn context_dispatch(request_id: &str) -> MethodDispatch {
    MethodDispatch::new(
        request_id,
        format!("{request_id}-correlation"),
        "attachment",
        agent_ide::app::transport::AssistanceMethod::Context,
        OpaqueJson::new("{\"path\":\"main.rs\"}", 64 * 1024).unwrap(),
    )
    .unwrap()
}

/// One hook submission with a caller-chosen request id.
fn hook_submission(request_id: &str) -> HookSubmit {
    HookSubmit::new(
        request_id,
        format!("{request_id}-correlation"),
        "attachment",
        OpaqueJson::new("{\"phase\":\"post\"}", 64 * 1024).unwrap(),
    )
    .unwrap()
}

/// F-04: a call that finds every call permit taken is answered `busy` (never dropped, so the
/// front can say nothing ran), hooks keep their own lane while calls are saturated, and the
/// refusal is counted in the journal.
#[tokio::test]
async fn saturated_call_lane_answers_busy_and_leaves_the_hook_lane_free() {
    journal_home();
    let runtime_dir = runtime_dir();
    let methods = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::clone(&methods),
        hooks_entered: Arc::new(AtomicUsize::new(0)),
        release: Arc::clone(&release),
        park_hooks: false,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 1).await;

    let parked_runtime = runtime_dir.clone();
    let parked = tokio::spawn(async move {
        dispatch_method_if_running(
            &parked_runtime,
            context_dispatch("parked"),
            transport_limits(),
        )
        .await
    });
    while methods.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let refused = dispatch_method_if_running(
        &runtime_dir,
        context_dispatch("refused"),
        transport_limits(),
    )
    .await;
    assert_eq!(refused, MethodDispatchTransportResult::Busy);
    let hook =
        submit_hook_if_running(&runtime_dir, hook_submission("hook"), transport_limits()).await;
    assert!(
        matches!(hook, HookSubmitTransportResult::Dispatched { .. }),
        "hooks keep their own lane while calls are saturated: {hook:?}"
    );
    release.add_permits(8);
    assert!(matches!(
        parked.await.unwrap(),
        MethodDispatchTransportResult::Dispatched { .. }
    ));

    let journal = journal_text();
    assert!(
        journal.contains("\"outcome\":\"refused\"") && journal.contains("connection_busy:call"),
        "the refused connection is journaled: {journal}"
    );
    stop_assistance_daemon(task, runtime_dir).await;
}

/// F-04: the hook lane is small and bounded too: a fifth concurrent hook is answered `busy`
/// while method calls, on their own lane, still dispatch.
#[tokio::test]
async fn saturated_hook_lane_answers_busy_and_leaves_the_call_lane_free() {
    // Every scenario of this binary fixes the journal home before any daemon starts.
    journal_home();
    let runtime_dir = runtime_dir();
    let hooks = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::new(AtomicUsize::new(0)),
        hooks_entered: Arc::clone(&hooks),
        release: Arc::clone(&release),
        park_hooks: true,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 16).await;
    let mut parked = Vec::new();
    for index in 0..4 {
        let runtime = runtime_dir.clone();
        parked.push(tokio::spawn(async move {
            submit_hook_if_running(
                &runtime,
                hook_submission(&format!("parked-{index}")),
                transport_limits(),
            )
            .await
        }));
    }
    while hooks.load(Ordering::SeqCst) < 4 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let reply = exchange(
        &runtime_dir,
        json!({
            "version": 2,
            "request_id": "fifth",
            "correlation_id": "fifth-correlation",
            "opaque_attachment": "attachment",
            "method": "assistance.hook_submit",
            "sanitized_observation_json": {"phase": "post"},
        }),
    )
    .await;
    assert_eq!(reply["status"], "busy", "{reply}");
    assert_eq!(reply["request_id"], "fifth");
    release.add_permits(8);
    let method = tokio::time::timeout(
        Duration::from_secs(2),
        dispatch_method_if_running(&runtime_dir, context_dispatch("call"), transport_limits()),
    )
    .await
    .expect("a call dispatches while the hook lane is saturated");
    assert!(matches!(
        method,
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    for task in parked {
        let _ = task.await;
    }
    stop_assistance_daemon(task, runtime_dir).await;
}

/// F-04: refusals still counted in an open journal window are written when the daemon stops, so a
/// burst followed by silence loses none of them.
///
/// Three refused calls inside one window: the first line is written at once, the other two are
/// only counted, and the orderly daemon exit must flush them as one `count: 2` line.
#[tokio::test]
async fn refusal_counts_are_flushed_when_the_daemon_stops() {
    journal_home();
    let runtime_dir = runtime_dir();
    let methods = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dispatcher = Arc::new(ParkingDispatcher {
        methods_entered: Arc::clone(&methods),
        hooks_entered: Arc::new(AtomicUsize::new(0)),
        release: Arc::clone(&release),
        park_hooks: false,
    });
    let task = start_limited_daemon(&runtime_dir, dispatcher, 1).await;
    let parked_runtime = runtime_dir.clone();
    let parked = tokio::spawn(async move {
        dispatch_method_if_running(
            &parked_runtime,
            context_dispatch("parked"),
            transport_limits(),
        )
        .await
    });
    while methods.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for index in 0..3 {
        let refused = dispatch_method_if_running(
            &runtime_dir,
            context_dispatch(&format!("refused-{index}")),
            transport_limits(),
        )
        .await;
        assert_eq!(refused, MethodDispatchTransportResult::Busy);
    }
    release.add_permits(8);
    assert!(matches!(
        parked.await.unwrap(),
        MethodDispatchTransportResult::Dispatched { .. }
    ));
    // An idle daemon acknowledges `daemon.stop` and exits through its orderly path.
    let stop = exchange(
        &runtime_dir,
        json!({"version": 1, "request_id": "stop", "method": "daemon.stop"}),
    )
    .await;
    assert_eq!(stop["status"], "ok", "{stop}");
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the daemon stops")
        .unwrap();

    let journal = journal_text();
    assert!(
        journal.contains("\"count\":2"),
        "the two refusals counted inside the window are flushed at shutdown: {journal}"
    );
    let _ = fs::remove_dir_all(&runtime_dir);
}
