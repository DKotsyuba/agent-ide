//! Private Unix daemon lifecycle with health and finite Assistance wire transport.

/// Private cache directory mechanics with peer-supplied retirement facts.
pub mod cache;
/// Immutable restart-only limits and their provenance for Application infrastructure.
pub mod config;
/// Long-lived client lease admission and idle-timeout daemon shutdown.
pub mod lease;
/// Dedicated SQLite owner-thread mechanics and durable operation receipts for domain SQL.
pub mod store;
/// Finite opaque Assistance hook and current-method transport values.
pub mod transport;

use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

use self::transport::{
    AssistanceDispatch, AssistanceDispatchReply, AssistanceDispatcher, AssistanceMethod,
    HookSubmit, HookSubmitTransportResult, HookTransportLimits, MethodDispatch,
    MethodDispatchTransportResult, OpaqueJson,
};

const SOCKET_NAME: &str = "agent-ide.sock";
/// File name of the exclusive lock a daemon holds in its runtime directory for its whole life.
pub(crate) const LOCK_NAME: &str = "agent-ide.lock";
const WIRE_VERSION: u8 = 1;
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_V1_FRAME_BYTES: usize = 64 * 1024;
/// Cap for one complete Assistance IPC frame, shared by the daemon and the MCP facade's reader.
pub(crate) const MAX_V2_FRAME_BYTES: usize = 160 * 1024;
/// Cap for one Assistance JSON payload inside a frame.
pub(crate) const MAX_ASSISTANCE_JSON_BYTES: usize = 144 * 1024;
/// Maximum time to wait for an Assistance method reply after its request is written.
const METHOD_DISPATCH_BUDGET: Duration = Duration::from_secs(10);
/// Total connect, request, and acknowledgement budget when a Codex MCP opens its client lease.
const CLIENT_LEASE_OPEN_TIMEOUT: Duration = Duration::from_secs(3);
/// Concurrent hook submissions one daemon serves, a lane of its own apart from the tool-call
/// permits (`max_connections`): every native tool call of every session sends two hooks, and a
/// burst of slow tool calls must neither starve the hooks that authenticate them nor be starved
/// by them.
const HOOK_CONNECTIONS: usize = 4;
/// Window of the journal line that counts refused connections: one line per lane per minute.
const BUSY_JOURNAL_WINDOW_MS: u64 = 60_000;
/// How long a failed daemon keeps serving after its dispatcher reported the failure, answering
/// health `restarting` and every new call a typed `restarting` reply, so a reply that is already
/// being written (the panicked call's own) reaches its peer before the connections are dropped.
const FAULT_DRAIN: Duration = Duration::from_millis(500);
/// Pause before the accept loop tries again after a transient `accept` error (descriptor or
/// memory exhaustion, an aborted handshake): long enough for the pressure to ease, short enough
/// that no call waits noticeably.
const ACCEPT_RETRY_PAUSE: Duration = Duration::from_millis(50);
/// File name of the launcher record a managed daemon generation is started with, inside its
/// runtime directory; a crash-only exit removes it so a replacement can write its own at once.
pub const LAUNCHER_FILE: &str = "launcher.json";
/// File name of the marker a front creates in a runtime directory just before it force-signals the
/// daemon that holds it: a daemon that finds it at exit keeps the runtime store whichever way it
/// exits (even an orderly `SIGTERM`), and a daemon that starts removes a stale one.
pub const RETAIN_STORE_FILE: &str = "retain-store";
/// File name of the Claude attachment record of a managed daemon generation, inside its runtime
/// directory; removed together with [`LAUNCHER_FILE`] by a crash-only exit.
pub const CLAUDE_ATTACHMENT_FILE: &str = "attachment";

/// One bounded connection lane: its permits, and the rate window of its refusal journal line.
#[derive(Clone)]
struct Lane {
    /// Concurrent connections the lane serves.
    permits: Arc<Semaphore>,
    /// Rate window of the journal line that counts refusals.
    refused: Arc<std::sync::Mutex<crate::errorlog::RateWindow>>,
    /// Closed journal detail naming the lane.
    detail: &'static str,
}

impl Lane {
    /// Creates a lane that serves at most `capacity` connections at once.
    fn new(capacity: usize, detail: &'static str) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(capacity)),
            refused: Arc::default(),
            detail,
        }
    }

    /// Counts one refused connection in the journal: the first refusal of a window is written at
    /// once, the rest only counted and flushed with the next window's line, so overload cannot
    /// flood the journal.
    fn note_refused(&self) {
        let due = self.refused.lock().ok().and_then(|mut window| {
            window.record(crate::errorlog::now_ms(), BUSY_JOURNAL_WINDOW_MS)
        });
        if let Some(suppressed) = due {
            self.journal_refusals(suppressed);
        }
    }

    /// Writes the refusals counted since the last journal line, when there are any and either
    /// their window has elapsed or `force` is set (daemon shutdown), so a burst followed by
    /// silence is never lost.
    fn flush_refusals(&self, force: bool) {
        let due = self.refused.lock().ok().and_then(|mut window| {
            let (started, suppressed) = window.parts();
            let elapsed = started.is_some_and(|started| {
                crate::errorlog::now_ms().saturating_sub(started) >= BUSY_JOURNAL_WINDOW_MS
            });
            (suppressed > 0 && (force || elapsed)).then(|| {
                *window = crate::errorlog::RateWindow::resume(started, 0);
                suppressed
            })
        });
        if let Some(suppressed) = due {
            self.journal_refusals(suppressed);
        }
    }

    /// Writes one refusal journal line; `suppressed` further refusals are carried as its count.
    fn journal_refusals(&self, suppressed: u64) {
        crate::errorlog::record(
            crate::errorlog::Method::Daemon,
            crate::errorlog::Outcome::Refused,
            crate::errorlog::Fields {
                reason: Some(crate::errorlog::ReasonCode::Capacity),
                detail: Some(self.detail),
                count: (suppressed > 0).then_some(suppressed),
                ..crate::errorlog::Fields::default()
            },
        );
    }
}

/// The two connection lanes of one daemon: tool calls (`max_connections`) and hooks.
#[derive(Clone)]
struct Lanes {
    /// Tool-call connections.
    calls: Lane,
    /// Hook-submission connections.
    hooks: Lane,
}

/// Reports whether a daemon answered the side-effect-free health request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    /// The endpoint answered with its current, non-authorizing daemon generation.
    Healthy { daemon_generation: String },
    /// No daemon could be reached without creating files or treating a stale endpoint as healthy.
    Unavailable,
}

/// Classifies the runtime directory without changing its contents or following untrusted paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorRuntimeState {
    /// The runtime directory is absent.
    Missing,
    /// The runtime directory exists but is not a private directory owned by this user.
    Unsafe,
    /// The runtime directory is a valid private directory.
    Private,
}

/// Classifies the endpoint pathname without connecting to or changing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorEndpointState {
    /// The endpoint pathname does not exist.
    Missing,
    /// The endpoint pathname is a Unix socket.
    Socket,
    /// The endpoint pathname exists but is not a Unix socket.
    Unexpected,
    /// Metadata could not be inspected.
    Unavailable,
}

/// Classifies whether an existing daemon lock is currently retained without creating a lock file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorLockState {
    /// The lock pathname does not exist.
    Missing,
    /// A nonblocking exclusive probe found another process retaining the lock.
    Held,
    /// A nonblocking exclusive probe succeeded and released the lock immediately.
    Unheld,
    /// The lock could not be safely inspected.
    Unavailable,
}

/// Records doctor observations and fixed protocol support without starting optional services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    /// Health result from an existing daemon only.
    pub status: DoctorStatus,
    /// Runtime directory classification made without creating it.
    pub runtime: DoctorRuntimeState,
    /// Endpoint pathname classification made before the health exchange.
    pub endpoint: DoctorEndpointState,
    /// Existing lock classification made without creating a lock file.
    pub lock: DoctorLockState,
    /// Effective immutable Application configuration used by this binary.
    pub config: config::EffectiveConfig,
}

/// Describes a local infrastructure failure without exposing host proof or domain state.
#[derive(Debug)]
pub enum AppError {
    /// The requested runtime directory is missing, unsafe, or not owned by the effective user.
    UnsafeRuntimeDirectory,
    /// Another daemon currently retains the runtime lock or answers on its endpoint.
    AlreadyRunning,
    /// An existing socket cannot safely be classified as stale.
    SocketStateUnknown,
    /// A daemon response failed the fixed health protocol.
    InvalidResponse,
    /// An operating-system operation failed while creating, connecting, or serving the private endpoint.
    Io(io::Error),
}

impl Display for AppError {
    /// Formats the stable local failure class without carrying secret actor or host-proof data.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafeRuntimeDirectory => formatter.write_str("unsafe runtime directory"),
            Self::AlreadyRunning => formatter.write_str("daemon already running"),
            Self::SocketStateUnknown => formatter.write_str("existing socket state is unknown"),
            Self::InvalidResponse => formatter.write_str("invalid daemon response"),
            Self::Io(error) => write!(formatter, "local application I/O failed: {error}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<io::Error> for AppError {
    /// Wraps an operating-system error produced by the private endpoint or runtime filesystem.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A runtime directory prepared exclusively for a daemon that this process is about to start.
#[derive(Debug)]
pub struct RuntimeDir {
    root: PathBuf,
}

impl RuntimeDir {
    /// Creates or validates `path` as a private real directory and returns it for daemon startup.
    ///
    /// This may create the final directory with mode `0700`; it never follows a symlink. Existing
    /// directories must already belong to the effective user and deny group/other access. Doctor
    /// deliberately accepts a plain `Path` instead, so querying never creates or repairs files.
    pub fn prepare_for_daemon(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = path.as_ref();
        if !root.is_absolute() {
            return Err(AppError::UnsafeRuntimeDirectory);
        }
        match fs::symlink_metadata(root) {
            Ok(metadata) => validate_private_directory(root, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = fs::DirBuilder::new();
                builder.mode(0o700).create(root)?;
                validate_private_directory(root, &fs::symlink_metadata(root)?)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Returns the private path used for this daemon's lock and socket.
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Returns the endpoint path reserved inside this verified private directory.
    fn socket_path(&self) -> PathBuf {
        self.root.join(SOCKET_NAME)
    }

    /// Returns the lock path reserved inside this verified private directory.
    fn lock_path(&self) -> PathBuf {
        self.root.join(LOCK_NAME)
    }
}

/// Starts the health-only private daemon and serves until its process is interrupted or killed.
///
/// The caller must obtain `runtime_dir` through [`RuntimeDir::prepare_for_daemon`]. Starting holds
/// an exclusive nonblocking lock, creates one fresh daemon generation, and never starts Workspace,
/// Execution, Intelligence, or Assistance work. It returns after bounded SIGINT/SIGTERM cleanup or
/// for a setup/listener failure; SIGKILL cannot run cleanup.
pub async fn run_daemon(runtime_dir: RuntimeDir) -> Result<(), AppError> {
    let ipc = config::EffectiveConfig::defaults().ipc();
    run_daemon_inner(runtime_dir, None, ipc, None, lease::DEFAULT_IDLE_TIMEOUT).await
}

/// Starts a private daemon that routes finite v2 hook/method and v3 method-only Assistance ingress.
///
/// `dispatcher` owns all attachment, host, rendering, and method semantics. Application only
/// frames, limits, correlates, and times out `assistance.hook_submit` and the closed current-method
/// dispatch set. Health remains available with its unchanged version-one contract. SIGINT/SIGTERM
/// stops ingress and awaits the dispatcher's bounded owned-resource cleanup before return.
///
/// `idle_timeout` bounds how long this daemon stays alive with zero open `ClientLease` connections
/// before it shuts down through the same orderly path as SIGINT/SIGTERM (EYES-r2 §2); callers that
/// do not yet source it from configuration should pass [`lease::DEFAULT_IDLE_TIMEOUT`].
pub async fn run_daemon_with_assistance(
    runtime_dir: RuntimeDir,
    dispatcher: Arc<dyn AssistanceDispatcher>,
    config: config::EffectiveConfig,
    idle_timeout: Duration,
) -> Result<(), AppError> {
    let ipc = config.ipc();
    let limits = HookTransportLimits::new(
        MAX_V2_FRAME_BYTES,
        MAX_ASSISTANCE_JSON_BYTES,
        ipc.connection_deadline,
    )
    .expect("fixed Assistance transport limits are valid");
    run_daemon_inner(
        runtime_dir,
        Some(dispatcher),
        ipc,
        Some(limits),
        idle_timeout,
    )
    .await
}

/// Binds one daemon endpoint until SIGINT/SIGTERM, then drains transport and bounded peer cleanup.
/// A dispatcher initialize timeout/error, and every setup or listener failure once serving starts,
/// take the same bounded shutdown path; the original serving failure wins if cleanup independently
/// fails. Draining accepted connections is a bounded cancel-and-join of the connection task set, not
/// a graceful wait for in-flight requests to finish.
async fn run_daemon_inner(
    runtime_dir: RuntimeDir,
    dispatcher: Option<Arc<dyn AssistanceDispatcher>>,
    ipc: config::IpcConfig,
    transport_limits: Option<HookTransportLimits>,
    idle_timeout: Duration,
) -> Result<(), AppError> {
    crate::errorlog::init(runtime_dir.path());
    crate::errorlog::install_panic_hook();
    let _lock = DaemonLock::acquire(runtime_dir.lock_path())?;
    // A marker left by an earlier generation's forced replacement has done its work.
    let _ = fs::remove_file(runtime_dir.path().join(RETAIN_STORE_FILE));
    let runtime_identity = fs::symlink_metadata(runtime_dir.path())
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()));
    let termination = termination_signal()?;
    tokio::pin!(termination);
    if let Some(dispatcher) = &dispatcher {
        tokio::select! {
            initialized = initialize_dispatcher(dispatcher, runtime_dir.path()) => {
                if let Err(error) = &initialized {
                    record_daemon_failure("initialize", error);
                }
                initialized?
            }
            _ = &mut termination => {
                shutdown_dispatcher(dispatcher).await?;
                return Ok(());
            }
        }
    }
    // Cache retention (`docs/cache-retention.md`) runs for the daemon's lifetime, first after a
    // delay and then hourly; it is aborted with the serving loop.
    let retention = dispatcher
        .is_some()
        .then(|| tokio::spawn(crate::retention::run_periodically()));
    let mut connections = tokio::task::JoinSet::new();
    let mut owned_socket = None;
    // Pending/running assistance jobs and project checks keep a lease-free daemon from idling
    // out (EYES-r2 §2); served calls restart the countdown at the connection layer (T26B).
    let busy = dispatcher.clone();
    let lease = lease::LeaseController::new(idle_timeout, move || {
        busy.as_ref().is_some_and(|dispatcher| dispatcher.is_busy())
    });
    let idle_expired = lease.idle_expired();
    tokio::pin!(idle_expired);
    let mut idle_exit = false;
    let mut fault_exit = false;
    let serving = async {
        let socket_path = runtime_dir.socket_path();
        retire_stale_socket(&socket_path, ipc.connection_deadline).await?;
        let listener = UnixListener::bind(&socket_path)?;
        owned_socket = Some(OwnedSocket::new(socket_path.clone())?);
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let generation = new_generation()?;
        let lanes = Lanes {
            calls: Lane::new(ipc.max_connections, "connection_busy:call"),
            hooks: Lane::new(HOOK_CONNECTIONS, "connection_busy:hook"),
        };
        crate::errorlog::record(
            crate::errorlog::Method::Daemon,
            crate::errorlog::Outcome::Started,
            crate::errorlog::Fields::default(),
        );

        let mut journal_tick = tokio::time::interval(Duration::from_millis(BUSY_JOURNAL_WINDOW_MS));
        // Crash-only containment: the dispatcher reports its own failure through `failed()`; the
        // daemon then keeps serving only `restarting` answers for `FAULT_DRAIN` and exits.
        let failure = async {
            match &dispatcher {
                Some(dispatcher) => dispatcher.failed().await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(failure);
        let mut drain_until: Option<tokio::time::Instant> = None;
        let mut accept_failing = false;
        loop {
            let draining = drain_until;
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = journal_tick.tick() => {
                    lanes.calls.flush_refusals(false);
                    lanes.hooks.flush_refusals(false);
                    continue;
                }
                _ = &mut termination => break,
                _ = &mut idle_expired => { idle_exit = !lease.stop_requested(); break; }
                joined = connections.join_next(), if !connections.is_empty() => {
                    journal_connection_panic(joined);
                    continue;
                }
                () = &mut failure, if draining.is_none() => {
                    drain_until = Some(tokio::time::Instant::now() + FAULT_DRAIN);
                    continue;
                }
                () = tokio::time::sleep_until(draining.unwrap_or_else(tokio::time::Instant::now)),
                    if draining.is_some() => {
                    fault_exit = true;
                    break;
                }
            };
            let (stream, _) = match accepted {
                Ok(accepted) => {
                    accept_failing = false;
                    accepted
                }
                Err(error) if transient_accept_error(&error) => {
                    // An exhausted descriptor table or a handshake that died in the queue must
                    // not end the daemon every session of the repository shares. One journal
                    // line per streak of failures keeps a long exhaustion from flooding it.
                    if !accept_failing {
                        accept_failing = true;
                        crate::errorlog::record(
                            crate::errorlog::Method::Daemon,
                            crate::errorlog::Outcome::Refused,
                            crate::errorlog::Fields {
                                reason: Some(crate::errorlog::ReasonCode::Capacity),
                                detail: Some("accept_retry"),
                                ..crate::errorlog::Fields::default()
                            },
                        );
                    }
                    tokio::time::sleep(ACCEPT_RETRY_PAUSE).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let generation = generation.clone();
            let dispatcher = dispatcher.clone();
            let lanes = lanes.clone();
            let lease = lease.clone();
            connections.spawn(async move {
                serve_accepted_connection(
                    stream,
                    generation,
                    dispatcher,
                    transport_limits,
                    ipc.connection_deadline,
                    lanes,
                    lease,
                )
                .await;
            });
        }
        // Refusals still counted in an open window would otherwise die with the daemon.
        lanes.calls.flush_refusals(true);
        lanes.hooks.flush_refusals(true);
        Ok(())
    }
    .await;
    if let Some(retention) = retention {
        retention.abort();
    }
    // The serving error (bind, accept, socket setup) wins over a later shutdown error, as in
    // `finish_daemon`; the journal names which of the two ended the daemon.
    let failed_serving = serving.is_err();
    let result = finish_daemon(serving, &mut connections, dispatcher.as_ref(), &lease).await;
    // The dispatcher's failure flag (or the retain marker a front leaves before force-signalling a
    // wedged daemon), not the exit branch that happened to win, decides whether this exit is
    // crash-only: a termination signal or idle expiry racing the fault drain, or the `SIGTERM` of a
    // forced replacement that the daemon handles orderly, must not delete the runtime store.
    let fault_exit = fault_exit
        || dispatcher.as_deref().is_some_and(|owner| owner.is_failed())
        || runtime_dir.path().join(RETAIN_STORE_FILE).exists();
    match &result {
        Err(error) => record_daemon_failure(
            if failed_serving {
                "serving"
            } else {
                "shutdown"
            },
            error,
        ),
        Ok(()) => crate::errorlog::record(
            crate::errorlog::Method::Daemon,
            if idle_exit {
                crate::errorlog::Outcome::IdleExit
            } else {
                crate::errorlog::Outcome::Stopped
            },
            crate::errorlog::Fields {
                detail: fault_exit.then_some("fault_exit"),
                ..crate::errorlog::Fields::default()
            },
        ),
    }
    if fault_exit {
        // Crash-only exit: the runtime directory keeps `state.sqlite` with its receipts, so the
        // replacement daemon finds every written edit as the unknown outcome it is and never
        // repeats one. Only this generation's launcher and attachment records go, so the
        // replacement can write its own at once instead of waiting out a stale-record race.
        retire_generation_records(runtime_dir.path());
    } else if result.is_ok()
        && owned_socket.is_some()
        && let Some(identity) = runtime_identity
    {
        remove_owned_runtime_directory(runtime_dir.path(), identity);
    }
    drop((owned_socket, _lock));
    result
}

/// Journals the end of a connection task when it ended by panicking: method `daemon`, outcome
/// `failed`, reason `internal`, detail `connection_task_panic` (closed cause, never the payload).
///
/// A panic in one connection task is contained there — the task is gone, the daemon and its other
/// connections keep serving — but `JoinSet` would otherwise hand the failure to nobody. Normal
/// completion and the cancellation of shutdown's `abort_all` are not faults and stay silent.
fn journal_connection_panic(joined: Option<Result<(), tokio::task::JoinError>>) {
    if matches!(joined, Some(Err(error)) if error.is_panic()) {
        crate::errorlog::record(
            crate::errorlog::Method::Daemon,
            crate::errorlog::Outcome::Failed,
            crate::errorlog::Fields {
                reason: Some(crate::errorlog::ReasonCode::Internal),
                detail: Some("connection_task_panic"),
                ..Default::default()
            },
        );
    }
}

/// Removes the launcher and Claude attachment records of the generation that is exiting after a
/// fault, leaving every other file of the runtime directory (the store, its backups, caches).
///
/// Best effort: a record that is already gone, or cannot be removed, changes nothing for the
/// replacement beyond the front's existing stale-record handling.
fn retire_generation_records(runtime_dir: &Path) {
    for name in [LAUNCHER_FILE, CLAUDE_ATTACHMENT_FILE] {
        let _ = fs::remove_file(runtime_dir.join(name));
    }
}

/// Reports whether an `accept` error is transient pressure rather than a broken listener.
fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::OutOfMemory
    ) || matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Journals that the daemon failed, with the stage that failed (`initialize`, `serving` or
/// `shutdown`) and the closed class of the error as `detail` (QW-4): `stage:class`, where class is
/// the [`AppError`] variant name or, for an operating-system failure, `io:` and the closed
/// [`io::ErrorKind`] name. Never the error's text, which can carry paths.
fn record_daemon_failure(stage: &str, error: &AppError) {
    let class = match error {
        AppError::UnsafeRuntimeDirectory => "unsafe_runtime_directory".to_owned(),
        AppError::AlreadyRunning => "already_running".to_owned(),
        AppError::SocketStateUnknown => "socket_state_unknown".to_owned(),
        AppError::InvalidResponse => "invalid_response".to_owned(),
        AppError::Io(error) => format!("io:{:?}", error.kind()),
    };
    crate::errorlog::record(
        crate::errorlog::Method::Daemon,
        crate::errorlog::Outcome::Failed,
        crate::errorlog::Fields {
            detail: Some(&format!("{stage}:{class}")),
            version: Some(env!("CARGO_PKG_VERSION")),
            ..Default::default()
        },
    );
}

/// Removes `path` only if it is still the exact directory identity captured when this daemon
/// acquired its lock; a directory a concurrent process already replaced is left untouched.
///
/// Per EYES-r2 §2, the shared daemon is never owned by any one MCP process, so its own orderly
/// shutdown (idle expiry or SIGINT/SIGTERM) is the only place left to remove its runtime directory,
/// matching what the existing managed-runtime cleanup does for a singly owned runtime.
fn remove_owned_runtime_directory(path: &Path, identity: (u64, u64)) {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && !metadata.file_type().is_symlink()
        && (metadata.dev(), metadata.ino()) == identity
    {
        let _ = fs::remove_dir_all(path);
    }
}

/// Bounds dispatcher initialization; a timeout or initialize error may still leave owned provider
/// children behind, so bounded shutdown always runs before the original failure is returned.
async fn initialize_dispatcher(
    dispatcher: &Arc<dyn AssistanceDispatcher>,
    runtime_dir: &Path,
) -> Result<(), AppError> {
    // Initialization measures every accepted executable (one language-server bundle alone is ~40 MB
    // of digests) and opens the store; on a loaded developer machine that takes over five
    // seconds, so the bound is generous while still finite.
    let initialized =
        tokio::time::timeout(Duration::from_secs(60), dispatcher.initialize(runtime_dir))
            .await
            .map_err(|_| AppError::InvalidResponse)
            .and_then(|result| result.map_err(|_| AppError::InvalidResponse));
    if initialized.is_err() {
        let shutdown = shutdown_dispatcher(dispatcher).await;
        return initialized.and(shutdown);
    }
    initialized
}

/// Aborts and joins every accepted connection task (cancellation, not a graceful drain of in-flight
/// requests), shuts down an initialized dispatcher, and runs every registered lease shutdown hook
/// before returning serving state. Cleanup is attempted in full; an earlier setup or accept error
/// remains the returned error.
async fn finish_daemon(
    serving: Result<(), AppError>,
    connections: &mut tokio::task::JoinSet<()>,
    dispatcher: Option<&Arc<dyn AssistanceDispatcher>>,
    lease: &lease::LeaseController,
) -> Result<(), AppError> {
    connections.abort_all();
    while let Some(joined) = connections.join_next().await {
        journal_connection_panic(Some(joined));
    }
    let shutdown = match dispatcher {
        Some(dispatcher) => shutdown_dispatcher(dispatcher).await,
        None => Ok(()),
    };
    lease.run_shutdown_hooks().await;
    serving.and(shutdown)
}

/// Creates native Tokio SIGINT/SIGTERM streams and resolves after the first delivered signal.
fn termination_signal() -> io::Result<impl std::future::Future<Output = ()>> {
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    })
}

/// Gives an Assistance peer at most forty seconds to cancel and reap its finite owned process set.
async fn shutdown_dispatcher(dispatcher: &Arc<dyn AssistanceDispatcher>) -> Result<(), AppError> {
    tokio::time::timeout(Duration::from_secs(40), dispatcher.shutdown())
        .await
        .map_err(|_| AppError::InvalidResponse)?
        .map_err(|_| AppError::InvalidResponse)
}

/// Connects to an already-running daemon for one hook submission without preparing or starting it.
///
/// Every connection, framing, deadline, or dispatcher fault becomes `Unavailable`. Callers must
/// exit their host hook permissively and must not retry inline or use this result as actor proof.
/// One absolute deadline covers connect, framing, exchange, and response parsing together.
pub async fn submit_hook_if_running(
    runtime_dir: &Path,
    request: HookSubmit,
    limits: HookTransportLimits,
) -> HookSubmitTransportResult {
    let Some(deadline) = tokio::time::Instant::now().checked_add(limits.deadline) else {
        return HookSubmitTransportResult::Unavailable;
    };
    let socket_path = runtime_dir.join(SOCKET_NAME);
    let mut stream = match tokio::time::timeout_at(deadline, UnixStream::connect(socket_path)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return HookSubmitTransportResult::Unavailable,
    };
    let observation =
        match serde_json::from_str::<Value>(request.sanitized_observation_json().as_str()) {
            Ok(observation) => observation,
            Err(_) => return HookSubmitTransportResult::Unavailable,
        };
    let wire = json!({
        "version": 2,
        "request_id": request.request_id(),
        "correlation_id": request.correlation_id(),
        "opaque_attachment": request.opaque_attachment(),
        "method": "assistance.hook_submit",
        "sanitized_observation_json": observation,
    });
    let result = async {
        write_frame(&mut stream, &wire, limits.max_frame_bytes).await?;
        let reply: Value = read_frame(&mut stream, limits.max_frame_bytes).await?;
        parse_hook_submit_reply(&reply, &request, limits.max_observation_bytes)
    };
    match tokio::time::timeout_at(deadline, result).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) | Err(_) => HookSubmitTransportResult::Unavailable,
    }
}

/// Connects to an already-running daemon for one closed v2-v5 method dispatch without starting it.
///
/// Connect faults return `Unavailable` and a connect deadline `TimedOut`: the request was never
/// delivered. Once writing the request began, any missing, malformed or explicitly unavailable
/// reply returns `OutcomeUnknown` and an elapsed deadline `WrittenTimedOut`, because the daemon may
/// have executed the call. Application does not retry, render, or reinterpret the opaque result.
/// Connect and request write share the hook transport deadline; after the write, reply waiting gets
/// the longer method budget. A short no-reply interval checks daemon health before keeping the
/// request open, so a paused daemon fails fast while a live worker retains the full method budget.
pub async fn dispatch_method_if_running(
    runtime_dir: &Path,
    request: MethodDispatch,
    limits: HookTransportLimits,
) -> MethodDispatchTransportResult {
    let Some(write_deadline) = tokio::time::Instant::now().checked_add(limits.deadline) else {
        return MethodDispatchTransportResult::Unavailable;
    };
    let socket_path = runtime_dir.join(SOCKET_NAME);
    let mut stream =
        match tokio::time::timeout_at(write_deadline, UnixStream::connect(socket_path)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) => return MethodDispatchTransportResult::Unavailable,
            Err(_) => return MethodDispatchTransportResult::TimedOut,
        };
    let params = match serde_json::from_str::<Value>(request.params_json().as_str()) {
        Ok(params) => params,
        Err(_) => return MethodDispatchTransportResult::Unavailable,
    };
    let method = match request.method() {
        AssistanceMethod::Start => "start",
        AssistanceMethod::Context => "context",
        AssistanceMethod::Diff => "diff",
        AssistanceMethod::Inspect => "inspect",
        AssistanceMethod::Stop => "stop",
        AssistanceMethod::Edit => "edit",
        AssistanceMethod::Outline => "outline",
        AssistanceMethod::Read => "read",
        AssistanceMethod::Symbol => "symbol",
        AssistanceMethod::Graph => "graph",
        AssistanceMethod::Test => "test",
        AssistanceMethod::HookSubmit => return MethodDispatchTransportResult::Unavailable,
    };
    let version = request.method().wire_version();
    let wire = json!({
        "version": version,
        "request_id": request.request_id(),
        "correlation_id": request.correlation_id(),
        "opaque_attachment": request.opaque_attachment(),
        "method": "assistance.method_dispatch",
        "dispatch_method": method,
        "params_json": params,
    });
    match tokio::time::timeout_at(
        write_deadline,
        write_frame(&mut stream, &wire, limits.max_frame_bytes),
    )
    .await
    {
        Ok(Ok(())) => {}
        // Writing began: a partial frame may already have reached the daemon.
        Ok(Err(_)) => return MethodDispatchTransportResult::OutcomeUnknown,
        Err(_) => return MethodDispatchTransportResult::WrittenTimedOut,
    }
    let Some(reply_deadline) = tokio::time::Instant::now().checked_add(METHOD_DISPATCH_BUDGET)
    else {
        return MethodDispatchTransportResult::OutcomeUnknown;
    };
    let Some(first_reply_deadline) =
        tokio::time::Instant::now().checked_add(limits.deadline.min(METHOD_DISPATCH_BUDGET))
    else {
        return MethodDispatchTransportResult::OutcomeUnknown;
    };
    let result = async {
        let reply: Value = read_frame(&mut stream, limits.max_frame_bytes).await?;
        parse_method_dispatch_reply(&reply, &request)
    };
    tokio::pin!(result);
    match tokio::time::timeout_at(first_reply_deadline, &mut result).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) => MethodDispatchTransportResult::OutcomeUnknown,
        Err(_) => {
            let responsive = tokio::time::timeout(limits.deadline, doctor(runtime_dir)).await;
            if !matches!(responsive, Ok(Ok(DoctorStatus::Healthy { .. }))) {
                return MethodDispatchTransportResult::WrittenTimedOut;
            }
            match tokio::time::timeout_at(reply_deadline, &mut result).await {
                Ok(Ok(reply)) => reply,
                Ok(Err(_)) => MethodDispatchTransportResult::OutcomeUnknown,
                Err(_) => MethodDispatchTransportResult::WrittenTimedOut,
            }
        }
    }
}

/// Queries `runtime_dir` for a health reply without creating directories, locks, sockets, or a daemon.
pub async fn doctor(runtime_dir: &Path) -> Result<DoctorStatus, AppError> {
    Ok(doctor_report(runtime_dir).await?.status)
}

/// Exchanges one health request only after a private real runtime directory and socket were observed.
async fn doctor_socket(socket_path: PathBuf, deadline: Duration) -> Result<DoctorStatus, AppError> {
    let stream = match tokio::time::timeout(deadline, UnixStream::connect(socket_path)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return Ok(DoctorStatus::Unavailable),
    };
    let request = HealthRequest::new("doctor");
    let response = match tokio::time::timeout(deadline, exchange(stream, &request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) | Err(_) => return Ok(DoctorStatus::Unavailable),
    };
    // A failed daemon that answers `restarting` is exiting to be replaced: reachable, not healthy.
    if response.version == WIRE_VERSION
        && response.request_id == request.request_id
        && response.status == "restarting"
    {
        return Ok(DoctorStatus::Unavailable);
    }
    if response.version != WIRE_VERSION
        || response.request_id != request.request_id
        || response.status != "ok"
    {
        return Err(AppError::InvalidResponse);
    }
    Ok(DoctorStatus::Healthy {
        daemon_generation: response.daemon_generation,
    })
}

/// Observes local startup compatibility without autostarting a daemon or opening peer services.
pub async fn doctor_report(runtime_dir: &Path) -> Result<DoctorReport, AppError> {
    let config = config::EffectiveConfig::defaults();
    let (runtime, endpoint, lock) = inspect_runtime(runtime_dir);
    let status = match (runtime, endpoint) {
        (DoctorRuntimeState::Private, DoctorEndpointState::Socket) => {
            doctor_socket(
                runtime_dir.join(SOCKET_NAME),
                config.ipc().connection_deadline,
            )
            .await?
        }
        _ => DoctorStatus::Unavailable,
    };
    Ok(DoctorReport {
        status,
        runtime,
        endpoint,
        lock,
        config,
    })
}

/// What one health probe of a daemon's control path found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthProbe {
    /// The daemon answered `ok` within the connection deadline.
    Healthy,
    /// The daemon answered `restarting`: it failed internally and is exiting to be replaced.
    Restarting,
    /// Nothing usable answered: no socket, a refused or stalled connection, or a bad reply.
    Silent,
}

/// Probes the control path of the daemon at `runtime_dir` once, without starting or stopping
/// anything.
///
/// The health request is served by the daemon's accept loop, never by its job queue or worker, so a
/// daemon that is busy with long jobs still answers [`HealthProbe::Healthy`]. The probe is bounded
/// by the configured connection deadline for the connect and for the reply.
pub async fn probe_health(runtime_dir: &Path) -> HealthProbe {
    let (runtime, endpoint, _) = inspect_runtime(runtime_dir);
    if (runtime, endpoint) != (DoctorRuntimeState::Private, DoctorEndpointState::Socket) {
        return HealthProbe::Silent;
    }
    let deadline = config::EffectiveConfig::defaults()
        .ipc()
        .connection_deadline;
    let Ok(Ok(stream)) =
        tokio::time::timeout(deadline, UnixStream::connect(runtime_dir.join(SOCKET_NAME))).await
    else {
        return HealthProbe::Silent;
    };
    let request = HealthRequest::new("probe");
    let Ok(Ok(response)) = tokio::time::timeout(deadline, exchange(stream, &request)).await else {
        return HealthProbe::Silent;
    };
    if response.version != WIRE_VERSION || response.request_id != request.request_id {
        return HealthProbe::Silent;
    }
    match response.status.as_str() {
        "ok" => HealthProbe::Healthy,
        "restarting" => HealthProbe::Restarting,
        _ => HealthProbe::Silent,
    }
}

/// Reports whether a daemon currently holds the runtime lock at `runtime_dir` (a nonblocking
/// probe that releases at once any lock it takes).
pub fn lock_is_held(runtime_dir: &Path) -> bool {
    inspect_lock(&runtime_dir.join(LOCK_NAME)) == DoctorLockState::Held
}

/// Returns the pid the current lock holder recorded, when the lock is held and the record parses.
///
/// A hint for pinning a wedge watch to one daemon generation; [`evict_wedged_daemon`] validates the
/// pid itself before it signals anything.
pub fn lock_holder_pid(runtime_dir: &Path) -> Option<i32> {
    let lock_path = runtime_dir.join(LOCK_NAME);
    if inspect_lock(&lock_path) != DoctorLockState::Held {
        return None;
    }
    fs::read_to_string(lock_path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|pid| *pid > 1)
}

/// Asks the daemon that holds `runtime_dir` to keep its runtime store when it exits, however it
/// exits, by creating [`RETAIN_STORE_FILE`] (owner-only; never followed through a symlink).
///
/// A front calls it right before force-signalling a wedged daemon, so even an orderly `SIGTERM`
/// exit keeps the receipts, and must not signal when it fails: without the marker an orderly exit
/// deletes the runtime directory.
pub fn retain_runtime_store(runtime_dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(runtime_dir.join(RETAIN_STORE_FILE))
        .map(drop)
}

/// How long a forced replacement waits for the daemon to leave after `SIGTERM` before `SIGKILL`.
pub const WEDGE_TERM_GRACE: Duration = Duration::from_secs(8);
/// Failed liveness probes required before a wedged daemon may be force-replaced.
pub const WEDGE_MIN_PROBES: u32 = 2;
/// Time the failed probes must span before a wedged daemon may be force-replaced: a stall of a few
/// seconds (a load spike, a stopped process that resumes) keeps its daemon and its binding.
pub const WEDGE_MIN_SPAN: Duration = Duration::from_secs(30);
/// Executable file name a lock holder must have to be signalled; nothing else is ever signalled.
const DAEMON_EXECUTABLE_NAME: &[u8] = b"agent-ide";

/// The closed result of one attempt to replace a wedged shared daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictOutcome {
    /// The holder was signalled and no longer holds the lock; `killed` says `SIGKILL` was needed.
    Terminated {
        /// Process id of the signalled daemon.
        pid: i32,
        /// Whether `SIGTERM` did not suffice within [`WEDGE_TERM_GRACE`].
        killed: bool,
    },
    /// The holder was signalled but still holds the lock after `SIGKILL` and a further wait: no
    /// replacement happened, and nothing is journaled as one.
    StillHeld {
        /// Process id of the daemon that did not leave.
        pid: i32,
    },
    /// Eviction was refused; the reason is a closed tag (`insufficient_evidence`, `not_held`,
    /// `no_pid`, `dead`, `not_agent_ide`, `answering`, `changed`, `retain_failed`). Before
    /// `SIGTERM` nothing was signalled; `answering` and `changed` can also come back after the
    /// `SIGTERM` (the pre-`SIGKILL` check found the daemon answering again or the holder replaced),
    /// in which case only `SIGTERM` was sent and the daemon is left to leave by itself.
    Refused(&'static str),
}

/// The lock holder a forced replacement may signal: its pid as recorded in the lock file and the
/// identity (device and inode) of the lock file that record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LockHolder {
    /// Pid the holder wrote into the lock file at acquire.
    pid: i32,
    /// Device of the lock file.
    device: u64,
    /// Inode of the lock file.
    inode: u64,
}

/// Re-reads and fully validates the current lock holder, or names why nothing may be signalled.
///
/// Every check runs at the moment of the call: the lock file is a private regular file, its lock
/// is still held (the holder is alive), the pid it records is a live process other than this one
/// whose executable file name is exactly `agent-ide`. Callers call it again immediately before
/// each signal and compare the result with the holder they first saw, so a lock handed to a
/// replacement daemon, a released lock or a reused pid never receives a signal meant for another.
fn current_lock_holder(lock_path: &Path) -> Result<LockHolder, &'static str> {
    if inspect_lock(lock_path) != DoctorLockState::Held {
        return Err("not_held");
    }
    let metadata = fs::symlink_metadata(lock_path).map_err(|_| "not_held")?;
    let pid = fs::read_to_string(lock_path)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 1)
        .ok_or("no_pid")?;
    // SAFETY: signal 0 only checks that the process exists.
    if pid as u32 == std::process::id() || unsafe { libc::kill(pid, 0) } != 0 {
        return Err("dead");
    }
    if !pid_is_agent_ide(pid) {
        return Err("not_agent_ide");
    }
    Ok(LockHolder {
        pid,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

/// Replaces a daemon that holds the runtime lock but does not answer its control path.
///
/// Eligibility is enforced here, not by the caller: `probes` failed probes spanning `span` must
/// reach [`WEDGE_MIN_PROBES`] and [`WEDGE_MIN_SPAN`] (they are the caller's evidence and also reach
/// the journal), and a given `expected_pid` (the holder the evidence was collected against) must
/// still be the holder. The holder must then validate (`current_lock_holder`) and one more probe
/// must still find the control path silent, again before `SIGKILL` — a daemon whose control path answers is never signalled,
/// however long its jobs run. The holder is revalidated and compared with the first one
/// immediately before `SIGTERM` and again before `SIGKILL`; any change refuses without signalling.
/// `SIGTERM`, up to [`WEDGE_TERM_GRACE`] for the lock to be released, then `SIGKILL` and a further
/// five seconds. Only a released lock is reported as [`EvictOutcome::Terminated`] and journaled
/// ([`record_forced_replacement`]); a holder that stays is [`EvictOutcome::StillHeld`]. The runtime
/// directory is left as it is: the next daemon starts in it and finds the store and its receipts.
pub async fn evict_wedged_daemon(
    runtime_dir: &Path,
    expected_pid: Option<i32>,
    probes: u32,
    span: Duration,
) -> EvictOutcome {
    if probes < WEDGE_MIN_PROBES || span < WEDGE_MIN_SPAN {
        return EvictOutcome::Refused("insufficient_evidence");
    }
    let lock_path = runtime_dir.join(LOCK_NAME);
    let holder = match current_lock_holder(&lock_path) {
        Ok(holder) => holder,
        Err(reason) => return EvictOutcome::Refused(reason),
    };
    // The evidence belongs to one daemon generation: a replacement that took the lock since is a
    // different daemon and is never signalled on the old one's silence.
    if expected_pid.is_some_and(|pid| pid != holder.pid) {
        return EvictOutcome::Refused("changed");
    }
    if probe_health(runtime_dir).await != HealthProbe::Silent {
        return EvictOutcome::Refused("answering");
    }
    if current_lock_holder(&lock_path) != Ok(holder) {
        return EvictOutcome::Refused("changed");
    }
    // The signal may be handled as an orderly shutdown by a daemon that resumed; the marker keeps
    // its runtime store (and so the receipts) in that case too.
    if retain_runtime_store(runtime_dir).is_err() {
        return EvictOutcome::Refused("retain_failed");
    }
    // SAFETY: the pid was validated as the live lock holder running the agent-ide executable an
    // instant ago, and the lock file identity and pid record are unchanged.
    unsafe { libc::kill(holder.pid, libc::SIGTERM) };
    let mut killed = false;
    // Monotonic deadlines, not counted sleeps: a loaded host delays a sleep, never the grace.
    let mut deadline = tokio::time::Instant::now() + WEDGE_TERM_GRACE;
    while inspect_lock(&lock_path) == DoctorLockState::Held {
        if tokio::time::Instant::now() >= deadline {
            if killed {
                return EvictOutcome::StillHeld { pid: holder.pid };
            }
            // A daemon that resumed and answers (even `restarting`) is leaving by itself: never
            // killed on the evidence of the silence that is over. The holder is revalidated after
            // that awaited probe, immediately before the signal, as before `SIGTERM`.
            if probe_health(runtime_dir).await != HealthProbe::Silent {
                return EvictOutcome::Refused("answering");
            }
            if current_lock_holder(&lock_path) != Ok(holder) {
                return EvictOutcome::Refused("changed");
            }
            killed = true;
            // SAFETY: as above, revalidated just now; SIGTERM did not release the lock.
            unsafe { libc::kill(holder.pid, libc::SIGKILL) };
            deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    record_forced_replacement(holder.pid, probes, span, killed);
    EvictOutcome::Terminated {
        pid: holder.pid,
        killed,
    }
}

/// Reports whether `pid` runs an executable whose file name is exactly `agent-ide`.
fn pid_is_agent_ide(pid: i32) -> bool {
    let mut path = vec![0_u8; 4096];
    // SAFETY: `path` is writable for its full length, which is passed as its size.
    let length = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if length <= 0 {
        return false;
    }
    Path::new(std::ffi::OsStr::from_bytes(&path[..length as usize]))
        .file_name()
        .is_some_and(|name| name.as_bytes() == DAEMON_EXECUTABLE_NAME)
}

/// Writes the one typed journal line of a forced daemon replacement: the pid, the failed probes
/// that justified it, the time they spanned and whether `SIGKILL` was needed.
///
/// The daily fault report counts these lines (client journal, reason `deadline`, detail starting
/// `wedged_daemon_replaced`). Best effort like every journal write.
pub fn record_forced_replacement(pid: i32, probes: u32, span: Duration, killed: bool) {
    crate::errorlog::record(
        crate::errorlog::Method::Client,
        crate::errorlog::Outcome::Failed,
        crate::errorlog::Fields {
            reason: Some(crate::errorlog::ReasonCode::Deadline),
            detail: Some(&format!(
                "wedged_daemon_replaced pid={pid} probes={probes} span_s={} kill={killed}",
                span.as_secs()
            )),
            ..crate::errorlog::Fields::default()
        },
    );
}

/// Classifies only existing runtime paths; every missing or unsafe path remains non-mutating.
fn inspect_runtime(
    runtime_dir: &Path,
) -> (DoctorRuntimeState, DoctorEndpointState, DoctorLockState) {
    let metadata = match fs::symlink_metadata(runtime_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (
                DoctorRuntimeState::Missing,
                DoctorEndpointState::Missing,
                DoctorLockState::Missing,
            );
        }
        Err(_) => {
            return (
                DoctorRuntimeState::Unsafe,
                DoctorEndpointState::Unavailable,
                DoctorLockState::Unavailable,
            );
        }
    };
    if validate_private_directory(runtime_dir, &metadata).is_err() {
        return (
            DoctorRuntimeState::Unsafe,
            DoctorEndpointState::Unavailable,
            DoctorLockState::Unavailable,
        );
    }
    (
        DoctorRuntimeState::Private,
        inspect_endpoint(&runtime_dir.join(SOCKET_NAME)),
        inspect_lock(&runtime_dir.join(LOCK_NAME)),
    )
}

/// Classifies one endpoint pathname without opening a connection or altering filesystem state.
fn inspect_endpoint(path: &Path) -> DoctorEndpointState {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => DoctorEndpointState::Socket,
        Ok(_) => DoctorEndpointState::Unexpected,
        Err(error) if error.kind() == io::ErrorKind::NotFound => DoctorEndpointState::Missing,
        Err(_) => DoctorEndpointState::Unavailable,
    }
}

/// Probes an existing lock without creating it and immediately releases any successful probe.
fn inspect_lock(path: &Path) -> DoctorLockState {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return DoctorLockState::Missing,
        Err(_) => return DoctorLockState::Unavailable,
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return DoctorLockState::Unavailable;
    }
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(_) => return DoctorLockState::Unavailable,
    };
    match unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } {
        0 => DoctorLockState::Unheld,
        _ if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock => {
            DoctorLockState::Held
        }
        _ => DoctorLockState::Unavailable,
    }
}

/// Identifies one accepted peer and routes it to health, Assistance dispatch, or a long-lived lease.
///
/// Peer identity and frame reading use `connection_deadline`; routed health, hook, lease, and
/// method replies keep their respective budgets. Method dispatch may wait up to
/// [`METHOD_DISPATCH_BUDGET`] after frame receipt, while hooks keep their configured deadline.
/// Only an admitted lease's ensuing hold-open phase is unbounded until peer EOF, run by
/// [`hold_lease_until_eof`] after this function returns (EYES-r2 §2). A lease request is admitted
/// from its own bounded pool ([`lease::LeaseController::try_admit`]) and never acquires `permits`,
/// the separate hook/assistance connection lanes.
///
/// A call or hook that finds its lane full is answered with a typed `busy` reply instead of being
/// dropped, and the refusal is counted in the journal (F-04).
async fn serve_accepted_connection(
    mut stream: UnixStream,
    generation: String,
    dispatcher: Option<Arc<dyn AssistanceDispatcher>>,
    transport_limits: Option<HookTransportLimits>,
    connection_deadline: Duration,
    lanes: Lanes,
    lease: lease::LeaseController,
) {
    let connection_deadline = tokio::time::Instant::now() + connection_deadline;
    let request = tokio::time::timeout_at(connection_deadline, async {
        if peer_uid(stream.as_raw_fd())? != effective_uid() {
            return Ok(None);
        }
        read_frame::<Value>(&mut stream, MAX_V2_FRAME_BYTES)
            .await
            .map(Some)
    })
    .await;
    let Ok(Ok(Some(request))) = request else {
        return;
    };
    let version = request.get("version").and_then(Value::as_u64);
    let identified = match version {
        Some(1) => {
            let failed = dispatcher.as_deref().is_some_and(|owner| owner.is_failed());
            let _ = tokio::time::timeout_at(
                connection_deadline,
                serve_v1_request(&mut stream, request, generation, &lease, failed),
            )
            .await;
            None
        }
        Some(version)
            if version == u64::from(transport::CLIENT_LEASE_WIRE_VERSION)
                && request.get("method").and_then(Value::as_str)
                    == Some("assistance.client_lease") =>
        {
            tokio::time::timeout_at(
                connection_deadline,
                serve_client_lease_handshake(&mut stream, request, &lease, dispatcher.as_deref()),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
        }
        Some(2..=5) => {
            // Served Assistance calls prove a live client session and restart the idle countdown.
            lease.mark_activity();
            if let (Some(dispatcher), Some(limits)) = (dispatcher, transport_limits) {
                let hook =
                    request.get("method").and_then(Value::as_str) == Some("assistance.hook_submit");
                let lane = if hook { &lanes.hooks } else { &lanes.calls };
                if dispatcher.is_failed() {
                    // Failed and exiting: nothing is dispatched, so nothing ran and the front may
                    // send the call to the replacement.
                    let _ = tokio::time::timeout_at(
                        connection_deadline,
                        write_status_reply(&mut stream, &request, version, "restarting"),
                    )
                    .await;
                } else if let Ok(_permit) = Arc::clone(&lane.permits).try_acquire_owned() {
                    let method = request.get("method").and_then(Value::as_str)
                        == Some("assistance.method_dispatch");
                    let budget = if method {
                        METHOD_DISPATCH_BUDGET
                    } else {
                        connection_deadline.duration_since(tokio::time::Instant::now())
                    };
                    let deadline = tokio::time::Instant::now() + budget;
                    let _ = tokio::time::timeout_at(
                        deadline,
                        serve_assistance_request(&mut stream, request, dispatcher, limits),
                    )
                    .await;
                } else {
                    lane.note_refused();
                    let _ = tokio::time::timeout_at(
                        connection_deadline,
                        write_status_reply(&mut stream, &request, version, "busy"),
                    )
                    .await;
                }
            }
            None
        }
        _ => None,
    };
    if let Some(guard) = identified {
        hold_lease_until_eof(stream).await;
        drop(guard);
    }
}

/// Answers a call or hook the daemon refused before dispatching it with a typed status reply:
/// `busy` (its lane was full) or `restarting` (the daemon failed and is exiting).
///
/// The reply carries the request's own version, `request_id` (and `correlation_id` for a hook)
/// and `"status"`, and no result: the daemon refused the request before dispatching it, so
/// nothing ran and the front may report that and let the caller repeat it (for `restarting`, on
/// the replacement daemon). A request without a usable `request_id` is dropped as before. The
/// write is best-effort; the peer may be gone.
async fn write_status_reply(
    stream: &mut UnixStream,
    request: &Value,
    version: Option<u64>,
    status: &str,
) -> io::Result<()> {
    let Some(request_id) = request.get("request_id").and_then(Value::as_str) else {
        return Ok(());
    };
    if request_id.is_empty() || request_id.len() > MAX_REQUEST_ID_BYTES {
        return Ok(());
    }
    let mut reply = json!({
        "version": version.unwrap_or_default(),
        "request_id": request_id,
        "status": status,
    });
    if let Some(correlation) = request.get("correlation_id").and_then(Value::as_str) {
        reply["correlation_id"] = Value::String(correlation.to_owned());
    }
    write_frame(stream, &reply, MAX_V1_FRAME_BYTES).await
}

/// Reads and discards bytes until EOF or error on one admitted lease connection.
///
/// Deliberately unbounded (no timeout): per EYES-r2 §2, an admitted lease is exempt from
/// `connection_deadline` for exactly this hold-open phase, since the peer keeps it open for its own
/// entire lifetime and only its EOF should release the lease.
async fn hold_lease_until_eof(mut stream: UnixStream) {
    let mut discard = [0_u8; 256];
    while matches!(stream.read(&mut discard).await, Ok(read) if read > 0) {}
}

/// Validates one `ClientLease` request, admits it from the bounded lease pool, and acks it.
///
/// Returns `Ok(None)` for a malformed request or a refused admission alike, so the connection is
/// simply dropped exactly like every other rejected frame; the peer never learns which occurred.
async fn serve_client_lease_handshake(
    stream: &mut UnixStream,
    request: Value,
    lease: &lease::LeaseController,
    dispatcher: Option<&dyn AssistanceDispatcher>,
) -> io::Result<Option<lease::LeaseGuard>> {
    let Ok(request) = serde_json::from_value::<transport::ClientLeaseRequest>(request) else {
        return Ok(None);
    };
    if request.version != transport::CLIENT_LEASE_WIRE_VERSION
        || request.request_id.is_empty()
        || request.request_id.len() > MAX_REQUEST_ID_BYTES
        || request.method != "assistance.client_lease"
    {
        return Ok(None);
    }
    // A failed daemon is exiting: a lease on it would only be dropped again.
    if dispatcher.is_some_and(|owner| owner.is_failed()) {
        return Ok(None);
    }
    let Some(guard) = lease.try_admit() else {
        return Ok(None);
    };
    let attachment = match request.candidate.as_deref() {
        Some(candidate) => {
            match dispatcher.and_then(|owner| owner.register_claude_candidate(candidate)) {
                Some(attachment) => Some(attachment),
                None => return Ok(None),
            }
        }
        None => None,
    };
    let ack = transport::ClientLeaseAck {
        version: transport::CLIENT_LEASE_WIRE_VERSION,
        request_id: request.request_id,
        status: "ok".to_owned(),
        attachment,
    };
    write_frame(stream, &ack, MAX_V1_FRAME_BYTES).await?;
    Ok(Some(guard))
}

/// Opens and acknowledges one long-lived `ClientLease` connection to a live daemon at `runtime_dir`.
///
/// Returns `None` for any connect, framing, correlation, or three-second handshake timeout; the
/// caller must fail open exactly like [`submit_hook_if_running`] and must not retry inline or use
/// this as actor proof. The caller must hold the returned stream for its entire lifetime; dropping
/// it sends EOF and releases the daemon's lease count (EYES-r2 §2).
pub async fn open_client_lease(
    runtime_dir: &Path,
    request_id: impl Into<String>,
) -> Option<UnixStream> {
    tokio::time::timeout(CLIENT_LEASE_OPEN_TIMEOUT, async move {
        let mut stream = UnixStream::connect(runtime_dir.join(SOCKET_NAME))
            .await
            .ok()?;
        let request = transport::ClientLeaseRequest::new(request_id);
        write_frame(&mut stream, &request, MAX_V1_FRAME_BYTES)
            .await
            .ok()?;
        let ack: transport::ClientLeaseAck =
            read_frame(&mut stream, MAX_V1_FRAME_BYTES).await.ok()?;
        (ack.version == transport::CLIENT_LEASE_WIRE_VERSION
            && ack.status == "ok"
            && ack.request_id == request.request_id)
            .then_some(stream)
    })
    .await
    .ok()
    .flatten()
}

/// Opens a lease and registers one host-selected Claude worktree on the shared daemon.
/// A missing registration acknowledgement leaves the caller unavailable.
pub async fn open_claude_client_lease(
    runtime_dir: &Path,
    candidate: &Path,
) -> Option<(UnixStream, String)> {
    let mut stream = UnixStream::connect(runtime_dir.join(SOCKET_NAME))
        .await
        .ok()?;
    let mut request = transport::ClientLeaseRequest::new("managed-claude-mcp");
    request.candidate = Some(candidate.to_path_buf());
    write_frame(&mut stream, &request, MAX_V1_FRAME_BYTES)
        .await
        .ok()?;
    let ack: transport::ClientLeaseAck = read_frame(&mut stream, MAX_V1_FRAME_BYTES).await.ok()?;
    (ack.version == transport::CLIENT_LEASE_WIRE_VERSION
        && ack.status == "ok"
        && ack.request_id == request.request_id)
        .then_some((stream, ack.attachment?))
}

/// Routes one version-one frame to its fixed method: health, or `daemon.stop` (0.6.7).
/// `failed` is the dispatcher's failure flag, which turns the health answer into `restarting`.
///
/// Every other method name is dropped without a reply, exactly as before; a pre-0.6.7 front
/// therefore never learns `daemon.stop` existed, and a pre-0.6.7 daemon stays silent for it.
async fn serve_v1_request(
    stream: &mut UnixStream,
    request: Value,
    generation: String,
    lease: &lease::LeaseController,
    failed: bool,
) -> io::Result<()> {
    match request.get("method").and_then(Value::as_str) {
        Some("daemon.stop") => serve_daemon_stop(stream, request, lease).await,
        _ => serve_health(stream, request, generation, failed).await,
    }
}

/// Answers one `daemon.stop` request: an idle daemon acknowledges and exits through its orderly
/// shutdown path; a daemon still holding live client bindings or daemon-owned work refuses.
///
/// This is the graceful replacement path a version-checking front (0.6.7) uses after learning from
/// the health exchange that this daemon is older than itself. The reply is sent before the exit
/// begins, because the orderly path closes this connection with every other.
async fn serve_daemon_stop(
    stream: &mut UnixStream,
    request: Value,
    lease: &lease::LeaseController,
) -> io::Result<()> {
    let Ok(request) = serde_json::from_value::<HealthRequest>(request) else {
        return Ok(());
    };
    if request.version != WIRE_VERSION
        || request.request_id.is_empty()
        || request.request_id.len() > MAX_REQUEST_ID_BYTES
        || request.method != "daemon.stop"
    {
        return Ok(());
    }
    let status = if lease.is_idle() {
        lease.request_stop();
        "ok"
    } else {
        "busy"
    };
    let response = DaemonStopResponse {
        version: WIRE_VERSION,
        request_id: request.request_id,
        status: status.to_owned(),
    };
    write_frame(stream, &response, MAX_V1_FRAME_BYTES).await
}

/// Validates the unchanged v1 health request and emits only its existing correlated health reply.
///
/// The reply keeps its shape; only its `status` changes: `ok`, or `restarting` once the
/// dispatcher `failed`, so a front never adopts a daemon that is exiting to be replaced.
async fn serve_health(
    stream: &mut UnixStream,
    request: Value,
    generation: String,
    failed: bool,
) -> io::Result<()> {
    let request: HealthRequest = serde_json::from_value(request)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if request.version != WIRE_VERSION
        || request.request_id.is_empty()
        || request.request_id.len() > MAX_REQUEST_ID_BYTES
        || request.method != "health"
    {
        return Ok(());
    }
    let response = HealthResponse {
        version: WIRE_VERSION,
        request_id: request.request_id,
        status: if failed { "restarting" } else { "ok" }.to_owned(),
        daemon_generation: generation,
    };
    write_frame(stream, &response, MAX_V1_FRAME_BYTES).await
}

/// Sends a health request over one connection and decodes its one response.
async fn exchange(mut stream: UnixStream, request: &HealthRequest) -> io::Result<HealthResponse> {
    write_frame(&mut stream, request, MAX_V1_FRAME_BYTES).await?;
    read_frame(&mut stream, MAX_V1_FRAME_BYTES).await
}

/// Decodes and forwards exactly one finite versioned Assistance request without inspecting semantics.
async fn serve_assistance_request(
    stream: &mut UnixStream,
    request: Value,
    dispatcher: Arc<dyn AssistanceDispatcher>,
    limits: HookTransportLimits,
) -> io::Result<()> {
    let object = request
        .as_object()
        .ok_or_else(|| invalid_transport("v2 request is not an object"))?;
    let version = object
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let method = required_string(object, "method")?;
    match method {
        "assistance.hook_submit" => {
            if version != 2 {
                return Err(invalid_transport("hook submit requires wire version 2"));
            }
            require_exact_keys(
                object,
                &[
                    "version",
                    "request_id",
                    "correlation_id",
                    "opaque_attachment",
                    "method",
                    "sanitized_observation_json",
                ],
            )?;
            let hook = HookSubmit::new(
                required_string(object, "request_id")?,
                required_string(object, "correlation_id")?,
                required_string(object, "opaque_attachment")?,
                OpaqueJson::from_value(
                    required_value(object, "sanitized_observation_json")?,
                    limits.max_observation_bytes,
                )
                .ok_or_else(|| invalid_transport("invalid hook observation"))?,
            )
            .ok_or_else(|| invalid_transport("invalid hook correlation"))?;
            let reply = tokio::time::timeout(
                limits.deadline,
                dispatcher.dispatch(AssistanceDispatch::HookSubmit(hook.clone())),
            )
            .await;
            let value = match reply {
                Ok(Ok(AssistanceDispatchReply::HookSubmit(payload)))
                    if payload.as_str().len() <= MAX_ASSISTANCE_JSON_BYTES =>
                {
                    json!({
                        "version": 2,
                        "request_id": hook.request_id(),
                        "correlation_id": hook.correlation_id(),
                        "opaque_reply_json": serde_json::from_str::<Value>(payload.as_str())
                            .map_err(|error| invalid_transport(error.to_string()))?,
                    })
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => json!({
                    "version": 2,
                    "request_id": hook.request_id(),
                    "correlation_id": hook.correlation_id(),
                    "status": "unavailable",
                }),
            };
            write_frame(stream, &value, limits.max_frame_bytes).await
        }
        "assistance.method_dispatch" => {
            require_exact_keys(
                object,
                &[
                    "version",
                    "request_id",
                    "correlation_id",
                    "opaque_attachment",
                    "method",
                    "dispatch_method",
                    "params_json",
                ],
            )?;
            let dispatch_method = AssistanceMethod::from_dispatch_tag(
                required_string(object, "dispatch_method")?,
                version,
            )
            .ok_or_else(|| invalid_transport("unsupported assistance method"))?;
            let dispatch = MethodDispatch::new(
                required_string(object, "request_id")?,
                required_string(object, "correlation_id")?,
                required_string(object, "opaque_attachment")?,
                dispatch_method,
                OpaqueJson::from_value(
                    required_value(object, "params_json")?,
                    MAX_ASSISTANCE_JSON_BYTES,
                )
                .ok_or_else(|| invalid_transport("invalid method parameters"))?,
            )
            .ok_or_else(|| invalid_transport("invalid method correlation"))?;
            let reply = tokio::time::timeout(
                METHOD_DISPATCH_BUDGET,
                dispatcher.dispatch(AssistanceDispatch::MethodDispatch(dispatch.clone())),
            )
            .await;
            if drop_reply_for_test(dispatch.method()) {
                return Ok(());
            }
            let value = match reply {
                Ok(Ok(AssistanceDispatchReply::MethodDispatch(payload)))
                    if payload.as_str().len() <= MAX_ASSISTANCE_JSON_BYTES =>
                {
                    json!({
                        "version": version,
                        "request_id": dispatch.request_id(),
                        "opaque_result_json": serde_json::from_str::<Value>(payload.as_str())
                            .map_err(|error| invalid_transport(error.to_string()))?,
                    })
                }
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {
                    json!({
                        "version": version,
                        "request_id": dispatch.request_id(),
                        "status": "unavailable",
                    })
                }
            };
            write_frame(stream, &value, limits.max_frame_bytes).await
        }
        _ => Err(invalid_transport("unsupported v2 transport method")),
    }
}

/// Parses one hook reply and maps all bounded unavailable or overflow states to fail-open transport.
fn parse_hook_submit_reply(
    reply: &Value,
    request: &HookSubmit,
    max_json_bytes: usize,
) -> io::Result<HookSubmitTransportResult> {
    let object = reply
        .as_object()
        .ok_or_else(|| invalid_transport("hook reply is not an object"))?;
    if object.get("version").and_then(Value::as_u64) != Some(2)
        || object.get("request_id").and_then(Value::as_str) != Some(request.request_id())
        || object.get("correlation_id").and_then(Value::as_str) != Some(request.correlation_id())
    {
        return Err(invalid_transport("hook reply correlation mismatch"));
    }
    let Some(payload) = object.get("opaque_reply_json") else {
        return Ok(HookSubmitTransportResult::Unavailable);
    };
    let payload = OpaqueJson::from_value(payload, max_json_bytes)
        .ok_or_else(|| invalid_transport("invalid hook reply payload"))?;
    Ok(HookSubmitTransportResult::Dispatched {
        correlation_id: request.correlation_id().to_owned(),
        opaque_reply_json: payload,
    })
}

/// Parses one closed method reply and maps transport unavailable or overflow states without semantics.
fn parse_method_dispatch_reply(
    reply: &Value,
    request: &MethodDispatch,
) -> io::Result<MethodDispatchTransportResult> {
    let object = reply
        .as_object()
        .ok_or_else(|| invalid_transport("method reply is not an object"))?;
    let expected_version = request.method().wire_version();
    if object.get("version").and_then(Value::as_u64) != Some(expected_version)
        || object.get("request_id").and_then(Value::as_str) != Some(request.request_id())
    {
        return Err(invalid_transport("method reply correlation mismatch"));
    }
    // The daemon refused the request before dispatching it: it never ran and may be repeated.
    if object.get("status").and_then(Value::as_str) == Some("busy") {
        return Ok(MethodDispatchTransportResult::Busy);
    }
    // Same guarantee from a failed daemon that is exiting: it never ran, but this daemon must not
    // be asked again.
    if object.get("status").and_then(Value::as_str) == Some("restarting") {
        return Ok(MethodDispatchTransportResult::Restarting);
    }
    // An explicit unavailable reply (for example a result over the payload bound) can follow an
    // executed call: the request was delivered, so its outcome is unknown, never "not sent".
    let Some(payload) = object.get("opaque_result_json") else {
        return Ok(MethodDispatchTransportResult::OutcomeUnknown);
    };
    let payload = OpaqueJson::from_value(payload, MAX_ASSISTANCE_JSON_BYTES)
        .ok_or_else(|| invalid_transport("invalid method reply payload"))?;
    Ok(MethodDispatchTransportResult::Dispatched {
        opaque_result_json: payload,
    })
}

/// Requires an exact string field from one finite v2 transport object.
fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
) -> io::Result<&'a str> {
    required_value(object, name)?
        .as_str()
        .ok_or_else(|| invalid_transport("required transport field is not a string"))
}

/// Requires one present field from one finite v2 transport object.
fn required_value<'a>(
    object: &'a serde_json::Map<String, Value>,
    name: &str,
) -> io::Result<&'a Value> {
    object
        .get(name)
        .ok_or_else(|| invalid_transport("required transport field is missing"))
}

/// Rejects unknown or omitted fields before Application forwards a v2 request to Assistance.
fn require_exact_keys(
    object: &serde_json::Map<String, Value>,
    expected: &[&str],
) -> io::Result<()> {
    if object.len() == expected.len() && expected.iter().all(|key| object.contains_key(*key)) {
        Ok(())
    } else {
        Err(invalid_transport("unknown or missing transport field"))
    }
}

/// Creates one bounded invalid-data error for a rejected private transport frame.
fn invalid_transport(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// Reads one length-prefixed JSON value without allocating for an unchecked frame length.
async fn read_frame<T>(stream: &mut UnixStream, max_frame_bytes: usize) -> io::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > max_frame_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized IPC frame",
        ));
    }
    let mut bytes = vec![0_u8; length];
    stream.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Serializes one JSON value as the fixed bounded length-prefixed IPC frame.
async fn write_frame<T>(
    stream: &mut UnixStream,
    value: &T,
    max_frame_bytes: usize,
) -> io::Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > max_frame_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized IPC frame",
        ));
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&bytes).await?;
    stream.flush().await
}

/// Rejects a runtime directory unless it is a real, private directory owned by this local user.
fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), AppError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(AppError::UnsafeRuntimeDirectory);
    }
    if path.parent().is_none() {
        return Err(AppError::UnsafeRuntimeDirectory);
    }
    Ok(())
}

/// Takes the exclusive nonblocking daemon lock and keeps its file descriptor open for daemon life.
///
/// The holder writes its pid into the lock file once it owns the lock; the file's content is only
/// a hint for [`evict_wedged_daemon`], which trusts it only while the lock is still held.
struct DaemonLock {
    _file: File,
}

impl DaemonLock {
    /// Creates the private lock file and rejects concurrent daemon ownership.
    fn acquire(path: PathBuf) -> Result<Self, AppError> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            // Best effort: the holder's pid lets a front find a daemon that holds the lock but
            // answers nothing (see `evict_wedged_daemon`). The descriptor is close-on-exec (the
            // std default), so no language server or check the daemon spawns ever inherits the lock.
            let _ = file.set_len(0);
            let _ = (&file).write_all(format!("{}\n", std::process::id()).as_bytes());
            Ok(Self { _file: file })
        } else if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            Err(AppError::AlreadyRunning)
        } else {
            Err(io::Error::last_os_error().into())
        }
    }
}

/// Removes an unreachable socket only after this daemon holds the exclusive lock.
async fn retire_stale_socket(socket_path: &Path, deadline: Duration) -> Result<(), AppError> {
    match fs::symlink_metadata(socket_path) {
        Ok(metadata) if metadata.file_type().is_socket() => {}
        Ok(_) => return Err(AppError::SocketStateUnknown),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    match tokio::time::timeout(deadline, UnixStream::connect(socket_path)).await {
        Ok(Err(error)) if error.raw_os_error() == Some(libc::ECONNREFUSED) => {
            fs::remove_file(socket_path)?;
            Ok(())
        }
        Ok(Ok(_)) => Err(AppError::AlreadyRunning),
        Ok(Err(_)) | Err(_) => Err(AppError::SocketStateUnknown),
    }
}

/// Retains the socket identity so cleanup cannot unlink a replacement created after this daemon exits.
struct OwnedSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocket {
    /// Captures the newly bound socket's filesystem identity for safe best-effort cleanup.
    fn new(path: PathBuf) -> Result<Self, AppError> {
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for OwnedSocket {
    /// Removes only the socket inode created by this daemon; replacements are left intact.
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path)
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Produces a fresh daemon-generation identifier carrying this daemon's version and executable.
///
/// Since 0.6.7 the identifier is `<version>-<executable-hash>-<random>`, so a front learns the
/// daemon's version and whether it runs from the same installed executable. A pre-0.6.7 daemon's
/// bare random hex names neither and is therefore older than every version-reporting front. The
/// identifier is opaque to older consumers, which compared generations only for equality.
fn new_generation() -> Result<String, AppError> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let random: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    match reported_version() {
        Some(version) => {
            let executable = std::env::current_exe()?;
            let hash = blake3::hash(executable.as_os_str().as_bytes()).to_hex();
            Ok(format!("{version}-{hash}-{random}"))
        }
        None => Ok(random),
    }
}

/// Reports whether this daemon drops the reply of one executed method call, once.
///
/// The `AGENT_IDE_TEST_DROP_REPLY` seam names one method (`edit`, `read`, `stop`, ...): the first
/// such call executes normally and its connection then closes without a reply, exactly the lost
/// reply a front must report as an unknown outcome. Only a `test-seams` build reads it.
fn drop_reply_for_test(method: AssistanceMethod) -> bool {
    static DROPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(seamed) = crate::test_seams::var("AGENT_IDE_TEST_DROP_REPLY") else {
        return false;
    };
    let named = match method {
        AssistanceMethod::Start => "start",
        AssistanceMethod::Context => "context",
        AssistanceMethod::Diff => "diff",
        AssistanceMethod::Inspect => "inspect",
        AssistanceMethod::Stop => "stop",
        AssistanceMethod::Edit => "edit",
        AssistanceMethod::Outline => "outline",
        AssistanceMethod::Read => "read",
        AssistanceMethod::Symbol => "symbol",
        AssistanceMethod::Graph => "graph",
        AssistanceMethod::Test => "test",
        AssistanceMethod::HookSubmit => return false,
    };
    seamed == named && !DROPPED.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// The product version this daemon reports in its generation identifier.
///
/// The `AGENT_IDE_TEST_DAEMON_VERSION` seam lets a product test start a daemon that reports an
/// older or newer version than the binary actually running — or, as `legacy`, no version at all,
/// exactly the pre-0.6.7 shape — so upgrade decisions can be exercised without a second binary;
/// a release build (no `test-seams` feature) ignores it, and an unusable value is ignored.
fn reported_version() -> Option<String> {
    let seamed = crate::test_seams::var("AGENT_IDE_TEST_DAEMON_VERSION")
        .filter(|value| !value.is_empty() && value.len() <= 32 && value.is_ascii());
    match seamed.as_deref() {
        Some("legacy") => None,
        Some(version) => Some(version.to_owned()),
        None => Some(env!("CARGO_PKG_VERSION").to_owned()),
    }
}

/// The daemon product version carried inside one health generation identifier, or `None` for a
/// pre-0.6.7 daemon whose generation is bare random hex or carries no parseable version.
pub fn reported_daemon_version(generation: &str) -> Option<&str> {
    let (version, _) = generation.split_once('-')?;
    version_segments(version).is_some().then_some(version)
}

/// Reports whether a daemon's version or executable requires replacement by `front`.
///
/// Older and unreported versions need an upgrade; equal-version binaries copied from another
/// executable path also need replacement. A newer daemon always remains in service for a
/// downgrade, regardless of its executable path.
pub fn daemon_needs_replacement(generation: &str, front: &str) -> bool {
    let Some(version) = reported_daemon_version(generation) else {
        return true;
    };
    if version_older_than(version, front) {
        return true;
    }
    !version_older_than(front, version) && !daemon_executable_matches_front(generation)
}

/// Reports whether `generation` came from the executable that is running this front.
///
/// Generations from pre-0.6.7 binaries and unrecognized identifier shapes return `false`, causing
/// a current front to attempt the same safe replacement path as for a version mismatch.
pub fn daemon_executable_matches_front(generation: &str) -> bool {
    let mut parts = generation.splitn(3, '-');
    let (Some(version), Some(hash), Some(_random)) = (parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if version_segments(version).is_none() || hash.len() != 64 {
        return false;
    }
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    let expected = blake3::hash(executable.as_os_str().as_bytes()).to_hex();
    hash == expected.as_str()
}

/// Orders two product versions by numeric dot segments; missing segments count as zero and any
/// `-suffix` is ignored, so `0.6` equals `0.6.0` and `0.6.10` is newer than `0.6.9`.
pub fn version_older_than(daemon: &str, front: &str) -> bool {
    let (Some(daemon), Some(front)) = (version_segments(daemon), version_segments(front)) else {
        // A version that does not parse names no release this binary knows; treat it as older so a
        // replacement is attempted rather than trusting an unparseable peer.
        return true;
    };
    for index in 0..daemon.len().max(front.len()) {
        let left = daemon.get(index).copied().unwrap_or(0);
        let right = front.get(index).copied().unwrap_or(0);
        if left != right {
            return left < right;
        }
    }
    false
}

/// Parses `version` into comparable numeric segments, ignoring any `-suffix`.
fn version_segments(version: &str) -> Option<Vec<u64>> {
    version
        .split('-')
        .next()?
        .split('.')
        .map(|segment| segment.parse::<u64>().ok())
        .collect()
}

/// Returns this process's effective UID for local endpoint and directory checks.
fn effective_uid() -> libc::uid_t {
    unsafe { libc::geteuid() }
}

/// Reads a connected Unix peer's UID using the operating system's credential primitive.
#[cfg(target_os = "linux")]
fn peer_uid(fd: RawFd) -> io::Result<libc::uid_t> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::zeroed();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { credentials.assume_init().uid })
}

/// Reads a connected Unix peer's UID using macOS's `getpeereid` primitive.
#[cfg(target_os = "macos")]
fn peer_uid(fd: RawFd) -> io::Result<libc::uid_t> {
    let mut uid = 0;
    let mut gid = 0;
    let result = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Represents the complete v1 health request and rejects all undeclared wire fields.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthRequest {
    version: u8,
    request_id: String,
    method: String,
}

impl HealthRequest {
    /// Creates the fixed health request for a nonempty local request identifier.
    fn new(request_id: impl Into<String>) -> Self {
        Self {
            version: WIRE_VERSION,
            request_id: request_id.into(),
            method: "health".to_owned(),
        }
    }
}

/// Represents the complete v1 health response correlated to one accepted request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HealthResponse {
    version: u8,
    request_id: String,
    status: String,
    daemon_generation: String,
}

/// Represents the complete v1 `daemon.stop` response correlated to one accepted request.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonStopResponse {
    version: u8,
    request_id: String,
    status: String,
}

/// The outcome of asking a live daemon to shut down through `daemon.stop`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonStop {
    /// Acknowledged: the daemon began its orderly shutdown and removes its own runtime directory.
    Stopped,
    /// Refused: the daemon still holds live client bindings or daemon-owned work.
    Busy,
    /// No correlated acknowledgement: the daemon predates the request, or never answered in time.
    Unanswered,
}

/// Asks the daemon at `runtime_dir` to shut down through its orderly path (0.6.7).
///
/// Any connect, framing, correlation, or timeout fault reports [`DaemonStop::Unanswered`], failing
/// open exactly like [`open_client_lease`]; a pre-0.6.7 daemon never replies to this method, which
/// is how a front distinguishes "cannot be asked" from "asked and refused".
pub async fn request_daemon_stop(runtime_dir: &Path) -> DaemonStop {
    let Ok(mut stream) = UnixStream::connect(runtime_dir.join(SOCKET_NAME)).await else {
        return DaemonStop::Unanswered;
    };
    let request = HealthRequest {
        version: WIRE_VERSION,
        request_id: "daemon-stop".to_owned(),
        method: "daemon.stop".to_owned(),
    };
    if write_frame(&mut stream, &request, MAX_V1_FRAME_BYTES)
        .await
        .is_err()
    {
        return DaemonStop::Unanswered;
    }
    let reply = async {
        let response: DaemonStopResponse = read_frame(&mut stream, MAX_V1_FRAME_BYTES).await?;
        io::Result::Ok(response)
    };
    let Ok(Ok(response)) = tokio::time::timeout(CLIENT_LEASE_OPEN_TIMEOUT, reply).await else {
        return DaemonStop::Unanswered;
    };
    if response.version != WIRE_VERSION || response.request_id != request.request_id {
        return DaemonStop::Unanswered;
    }
    match response.status.as_str() {
        "ok" => DaemonStop::Stopped,
        "busy" => DaemonStop::Busy,
        _ => DaemonStop::Unanswered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::transport::AssistanceDispatchUnavailable;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Ensures the bridge can wait longer than the worker's inline reply window.
    #[test]
    fn method_dispatch_budget_exceeds_worker_inline_reply_wait() {
        assert!(
            METHOD_DISPATCH_BUDGET > crate::assistance::worker::INLINE_REPLY_WAIT,
            "the bridge must preserve the original invocation through the inline wait"
        );
    }

    /// A 0.6.7 generation carries version and executable identity, while a pre-0.6.7 bare random
    /// generation names neither so every current front can identify it as old.
    #[test]
    fn generation_identifiers_carry_the_daemon_version() {
        let generation = new_generation().expect("OS randomness is available");
        let mut parts = generation.splitn(3, '-');
        let version = parts.next().expect("version is first");
        let executable_hash = parts.next().expect("executable hash follows version");
        let random = parts.next().expect("random generation is last");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        assert_eq!(executable_hash.len(), 64);
        assert!(executable_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(random.len(), 32, "{generation}");
        assert!(random.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(reported_daemon_version(&generation), Some(version));
        assert!(daemon_executable_matches_front(&generation));
        assert!(!daemon_needs_replacement(&generation, version));
        assert!(daemon_needs_replacement(
            &format!("{version}-{}-0123456789abcdef", "0".repeat(64)),
            version
        ));
        assert!(!daemon_needs_replacement(
            &format!("9.9.9-{}-0123456789abcdef", "0".repeat(64)),
            version
        ));
        // The pre-0.6.7 shape: bare random hex, no version to learn.
        assert_eq!(
            reported_daemon_version("0123456789abcdef0123456789abcdef"),
            None
        );
        assert_eq!(
            reported_daemon_version("not-a-version-0123456789abcdef"),
            None
        );
        assert!(daemon_needs_replacement(
            "0.6.6-0123456789abcdef0123456789abcdef-0123456789abcdef",
            "0.6.7"
        ));
    }

    /// Versions order numerically per dot segment, with missing segments as zero, `-suffixes`
    /// ignored, and anything unparseable older than every release this binary knows.
    #[test]
    fn versions_order_numerically_per_segment() {
        assert!(version_older_than("0.6.6", "0.6.7"));
        assert!(!version_older_than("0.6.7", "0.6.7"));
        assert!(
            !version_older_than("0.6.8", "0.6.7"),
            "a newer daemon is used as is"
        );
        assert!(
            version_older_than("0.6.9", "0.6.10"),
            "numeric, not lexical"
        );
        assert!(
            !version_older_than("0.6", "0.6.0"),
            "missing segments are zero"
        );
        assert!(!version_older_than("0.7.0-rc1", "0.7.0"));
        assert!(version_older_than("garbage", "0.6.7"));
        assert!(version_older_than("", "0.6.7"));
    }

    /// Records whether daemon initialization and shutdown reached the owned dispatcher.
    struct ShutdownProbe {
        /// Becomes true when the dispatcher owns initialized provider state.
        initialized: AtomicBool,
        /// Becomes true only after shutdown observes initialized ownership.
        shutdown: AtomicBool,
    }

    impl ShutdownProbe {
        /// Creates a probe with no initialized or reaped provider state.
        const fn new() -> Self {
            Self {
                initialized: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
            }
        }
    }

    impl AssistanceDispatcher for ShutdownProbe {
        /// Marks provider ownership as established; the path is irrelevant to this lifecycle probe.
        fn initialize<'a>(
            &'a self,
            _runtime_dir: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>>
        {
            Box::pin(async move {
                self.initialized.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        /// Proves shutdown runs after initialization and records the simulated provider reap.
        fn shutdown(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + '_>>
        {
            Box::pin(async move {
                assert!(self.initialized.load(Ordering::SeqCst));
                self.shutdown.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        /// Rejects dispatch because this probe exercises only initialized-resource cleanup.
        fn dispatch(
            &self,
            _request: AssistanceDispatch,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err(AssistanceDispatchUnavailable) })
        }
    }

    /// Marks an accepted connection task as reaped when cancellation drops its owned state.
    struct ReapProbe(Arc<AtomicBool>);

    impl Drop for ReapProbe {
        /// Records that abort-and-join dropped the task before daemon cleanup returned.
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Preserves an accept failure while still draining connections and reaping initialized providers.
    #[tokio::test]
    async fn accept_error_still_drains_connections_and_shuts_down_dispatcher() {
        let probe = Arc::new(ShutdownProbe::new());
        probe.initialize(Path::new("unused")).await.unwrap();
        let dispatcher: Arc<dyn AssistanceDispatcher> = probe.clone();
        let reaped = Arc::new(AtomicBool::new(false));
        let mut connections = tokio::task::JoinSet::new();
        let task_reaped = reaped.clone();
        connections.spawn(async move {
            let _probe = ReapProbe(task_reaped);
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;

        let lease = lease::LeaseController::new(Duration::from_secs(300), || false);
        let error = finish_daemon(
            Err(AppError::Io(io::Error::from_raw_os_error(libc::EMFILE))),
            &mut connections,
            Some(&dispatcher),
            &lease,
        )
        .await
        .unwrap_err();

        assert_eq!(
            match error {
                AppError::Io(error) => error.raw_os_error(),
                _ => None,
            },
            Some(libc::EMFILE)
        );
        assert!(reaped.load(Ordering::SeqCst));
        assert!(probe.shutdown.load(Ordering::SeqCst));
    }

    /// Rejects initialization and records whether shutdown still ran on this exact dispatcher.
    struct FailingInitProbe {
        /// Becomes true only if shutdown is invoked despite the failed initialize.
        shutdown_called: AtomicBool,
    }

    impl AssistanceDispatcher for FailingInitProbe {
        /// Simulates a dispatcher that already owns partial state before reporting failure.
        fn initialize<'a>(
            &'a self,
            _runtime_dir: &'a Path,
        ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + 'a>>
        {
            Box::pin(async { Err(AssistanceDispatchUnavailable) })
        }

        /// Records that bounded shutdown ran to reap whatever initialize may have started.
        fn shutdown(
            &self,
        ) -> Pin<Box<dyn Future<Output = Result<(), AssistanceDispatchUnavailable>> + Send + '_>>
        {
            Box::pin(async move {
                self.shutdown_called.store(true, Ordering::SeqCst);
                Ok(())
            })
        }

        /// Unreachable in this initialize-failure probe.
        fn dispatch(
            &self,
            _request: AssistanceDispatch,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err(AssistanceDispatchUnavailable) })
        }
    }

    /// QW-4: a failed daemon names the failing stage and the closed error class, never the
    /// error's text.
    #[test]
    fn daemon_failure_is_journaled_with_its_stage_and_closed_class() {
        crate::errorlog::capture_start();
        for (stage, error) in [
            ("initialize", AppError::InvalidResponse),
            ("serving", AppError::AlreadyRunning),
            ("serving", AppError::UnsafeRuntimeDirectory),
            ("shutdown", AppError::SocketStateUnknown),
            (
                "serving",
                AppError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "/secret/path/in/the/message",
                )),
            ),
        ] {
            record_daemon_failure(stage, &error);
        }
        let events = crate::errorlog::capture_take();
        let details = events
            .iter()
            .map(|event| {
                assert_eq!(
                    (event.method.as_str(), event.outcome.as_str()),
                    ("daemon", "failed")
                );
                assert_eq!(event.level, "error");
                event.detail.clone().unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            details,
            [
                "initialize:invalid_response",
                "serving:already_running",
                "serving:unsafe_runtime_directory",
                "shutdown:socket_state_unknown",
                "serving:io:PermissionDenied",
            ]
        );
    }

    /// Proves a rejected initialize still runs bounded shutdown and preserves the original failure.
    #[tokio::test]
    async fn failed_initialize_still_shuts_down_and_preserves_the_original_error() {
        let probe = Arc::new(FailingInitProbe {
            shutdown_called: AtomicBool::new(false),
        });
        let dispatcher: Arc<dyn AssistanceDispatcher> = probe.clone();

        let error = initialize_dispatcher(&dispatcher, Path::new("unused"))
            .await
            .unwrap_err();

        assert!(matches!(error, AppError::InvalidResponse));
        assert!(probe.shutdown_called.load(Ordering::SeqCst));
    }

    /// Counts the journal lines that name a panicked connection task.
    fn connection_panic_lines(events: &[crate::errorlog::LoggedEvent]) -> usize {
        events
            .iter()
            .filter(|event| {
                (
                    event.method.as_str(),
                    event.outcome.as_str(),
                    event.reason.as_deref(),
                ) == ("daemon", "failed", Some("internal"))
                    && event.detail.as_deref() == Some("connection_task_panic")
            })
            .count()
    }

    /// Dispatcher whose every call panics, so the connection task serving it panics.
    struct PanickingDispatcher;

    impl AssistanceDispatcher for PanickingDispatcher {
        /// Panics inside the connection task that awaits it.
        fn dispatch(
            &self,
            _request: AssistanceDispatch,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<AssistanceDispatchReply, AssistanceDispatchUnavailable>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { panic!("connection task fault") })
        }
    }

    /// Removes a scratch runtime directory on every exit path, including a failing assertion.
    struct ScratchRuntime(PathBuf);

    impl Drop for ScratchRuntime {
        /// Best-effort removal: the daemon already removes it after an orderly exit.
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A connection task that panics while the daemon serves is journaled with a closed cause (the
    /// live `join_next` arm), the daemon keeps serving, and the cause carries no payload text.
    ///
    /// Before the fix the `JoinSet` result was discarded: the task died with nothing naming it.
    #[tokio::test]
    async fn a_panicking_connection_task_is_journaled_while_the_daemon_serves() {
        crate::errorlog::capture_start();
        let scratch = ScratchRuntime(
            std::env::temp_dir().join(format!("agent-ide-panic-live-{}", std::process::id())),
        );
        let runtime = RuntimeDir::prepare_for_daemon(&scratch.0).unwrap();
        let daemon = tokio::spawn(run_daemon_with_assistance(
            runtime,
            Arc::new(PanickingDispatcher),
            config::EffectiveConfig::defaults(),
            Duration::from_millis(1500),
        ));
        for _ in 0..200 {
            if UnixStream::connect(scratch.0.join(SOCKET_NAME))
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let limits = HookTransportLimits::new(160 * 1024, 144 * 1024, Duration::from_secs(1))
            .expect("fixed limits are valid");
        let method = MethodDispatch::new(
            "panic",
            "panic",
            "attachment",
            AssistanceMethod::Context,
            OpaqueJson::new("{}", 64).unwrap(),
        )
        .unwrap();
        let outcome = dispatch_method_if_running(&scratch.0, method, limits).await;
        assert!(!matches!(
            outcome,
            MethodDispatchTransportResult::Dispatched { .. }
        ));
        // The daemon outlives the panicked task: health still answers.
        assert_eq!(probe_health(&scratch.0).await, HealthProbe::Healthy);
        tokio::time::timeout(Duration::from_secs(10), daemon)
            .await
            .expect("the idle daemon exits")
            .unwrap()
            .unwrap();
        let events = crate::errorlog::capture_take();
        assert_eq!(connection_panic_lines(&events), 1, "{events:?}");
    }

    /// A connection task that panicked before shutdown's abort-and-join is journaled by the drain
    /// too, while a task that is merely cancelled by the drain is not.
    #[tokio::test]
    async fn shutdown_drain_journals_a_panicked_task_but_not_a_cancelled_one() {
        crate::errorlog::capture_start();
        let mut connections = tokio::task::JoinSet::new();
        connections.spawn(async { panic!("connection task fault") });
        connections.spawn(std::future::pending::<()>());
        tokio::task::yield_now().await;
        let lease = lease::LeaseController::new(Duration::from_secs(300), || false);
        finish_daemon(Ok(()), &mut connections, None, &lease)
            .await
            .unwrap();
        let events = crate::errorlog::capture_take();
        assert_eq!(connection_panic_lines(&events), 1, "{events:?}");
    }

    /// Descriptor or memory exhaustion and a handshake that died in the queue are transient accept
    /// errors the daemon retries; a closed or invalid listener is not, and ends the daemon.
    #[test]
    fn transient_accept_errors_are_retried_and_listener_failures_are_not() {
        for code in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(
                transient_accept_error(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
        for kind in [
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::Interrupted,
        ] {
            assert!(transient_accept_error(&io::Error::from(kind)), "{kind:?}");
        }
        for code in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK] {
            assert!(
                !transient_accept_error(&io::Error::from_raw_os_error(code)),
                "{code}"
            );
        }
    }

    /// The daemon lock records its holder's pid for a front to find a wedged daemon, and its
    /// descriptor is close-on-exec, so no language server or check the daemon spawns inherits it.
    #[test]
    fn the_daemon_lock_records_its_pid_and_is_close_on_exec() {
        let dir = std::env::temp_dir().join(format!("agent-ide-lock-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LOCK_NAME);
        let lock = DaemonLock::acquire(path.clone()).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
        // SAFETY: the descriptor is valid while `lock` lives.
        let flags = unsafe { libc::fcntl(lock._file.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "{flags}");
        assert!(matches!(
            DaemonLock::acquire(path),
            Err(AppError::AlreadyRunning)
        ));
        drop(lock);
        fs::remove_dir_all(dir).unwrap();
    }
}
