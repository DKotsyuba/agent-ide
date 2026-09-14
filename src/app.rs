//! Private Unix daemon lifecycle with health and finite Assistance wire transport.

/// Private cache directory mechanics with peer-supplied retirement facts.
pub mod cache;
/// Immutable restart-only limits and their provenance for Application infrastructure.
pub mod config;
/// Dedicated SQLite owner-thread mechanics and durable operation receipts for domain SQL.
pub mod store;
/// Finite opaque Assistance hook and current-method transport values.
pub mod transport;

use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
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
const LOCK_NAME: &str = "agent-ide.lock";
const WIRE_VERSION: u8 = 1;
const MAX_REQUEST_ID_BYTES: usize = 128;
const MAX_V1_FRAME_BYTES: usize = 64 * 1024;
const MAX_V2_FRAME_BYTES: usize = 128 * 1024;
const MAX_ASSISTANCE_JSON_BYTES: usize = 64 * 1024;

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
    run_daemon_inner(runtime_dir, None, ipc, None).await
}

/// Starts a private daemon that routes finite v2 hook/method and v3 method-only Assistance ingress.
///
/// `dispatcher` owns all attachment, host, rendering, and method semantics. Application only
/// frames, limits, correlates, and times out `assistance.hook_submit` and the closed current-method
/// dispatch set. Health remains available with its unchanged version-one contract. SIGINT/SIGTERM
/// stops ingress and awaits the dispatcher's bounded owned-resource cleanup before return.
pub async fn run_daemon_with_assistance(
    runtime_dir: RuntimeDir,
    dispatcher: Arc<dyn AssistanceDispatcher>,
    config: config::EffectiveConfig,
) -> Result<(), AppError> {
    let ipc = config.ipc();
    let limits = HookTransportLimits::new(
        MAX_V2_FRAME_BYTES,
        MAX_ASSISTANCE_JSON_BYTES,
        ipc.connection_deadline,
    )
    .expect("fixed Assistance transport limits are valid");
    run_daemon_inner(runtime_dir, Some(dispatcher), ipc, Some(limits)).await
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
) -> Result<(), AppError> {
    let _lock = DaemonLock::acquire(runtime_dir.lock_path())?;
    let termination = termination_signal()?;
    tokio::pin!(termination);
    if let Some(dispatcher) = &dispatcher {
        tokio::select! {
            initialized = initialize_dispatcher(dispatcher, runtime_dir.path()) => initialized?,
            _ = &mut termination => {
                shutdown_dispatcher(dispatcher).await?;
                return Ok(());
            }
        }
    }
    let mut connections = tokio::task::JoinSet::new();
    let mut owned_socket = None;
    let serving = async {
        let socket_path = runtime_dir.socket_path();
        retire_stale_socket(&socket_path, ipc.connection_deadline).await?;
        let listener = UnixListener::bind(&socket_path)?;
        owned_socket = Some(OwnedSocket::new(socket_path.clone())?);
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let generation = new_generation()?;
        let permits = Arc::new(Semaphore::new(ipc.max_connections));

        loop {
            let accepted = tokio::select! {
                accepted = listener.accept() => accepted,
                _ = &mut termination => break,
                _ = connections.join_next(), if !connections.is_empty() => continue,
            };
            let (stream, _) = accepted?;
            let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                drop(stream);
                continue;
            };
            let generation = generation.clone();
            let dispatcher = dispatcher.clone();
            connections.spawn(async move {
                let _permit = permit;
                let _ = tokio::time::timeout(
                    ipc.connection_deadline,
                    serve_connection(stream, generation, dispatcher, transport_limits),
                )
                .await;
            });
        }
        Ok(())
    }
    .await;
    let result = finish_daemon(serving, &mut connections, dispatcher.as_ref()).await;
    drop((owned_socket, _lock));
    result
}

/// Bounds dispatcher initialization; a timeout or initialize error may still leave owned provider
/// children behind, so bounded shutdown always runs before the original failure is returned.
async fn initialize_dispatcher(
    dispatcher: &Arc<dyn AssistanceDispatcher>,
    runtime_dir: &Path,
) -> Result<(), AppError> {
    let initialized =
        tokio::time::timeout(Duration::from_secs(5), dispatcher.initialize(runtime_dir))
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
/// requests) and shuts down an initialized dispatcher before returning serving state. Cleanup is
/// attempted in full; an earlier setup or accept error remains the returned error.
async fn finish_daemon(
    serving: Result<(), AppError>,
    connections: &mut tokio::task::JoinSet<()>,
    dispatcher: Option<&Arc<dyn AssistanceDispatcher>>,
) -> Result<(), AppError> {
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    let shutdown = match dispatcher {
        Some(dispatcher) => shutdown_dispatcher(dispatcher).await,
        None => Ok(()),
    };
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

/// Connects to an already-running daemon for one closed v2/v3 method dispatch without starting it.
///
/// Transport faults return `Unavailable`; Application does not retry, render, or reinterpret the
/// opaque result. Assistance decides whether that unavailable result must be shown to its caller.
/// Connect and exchange consume the same absolute deadline; a completed connect never resets it.
pub async fn dispatch_method_if_running(
    runtime_dir: &Path,
    request: MethodDispatch,
    limits: HookTransportLimits,
) -> MethodDispatchTransportResult {
    let Some(deadline) = tokio::time::Instant::now().checked_add(limits.deadline) else {
        return MethodDispatchTransportResult::Unavailable;
    };
    let socket_path = runtime_dir.join(SOCKET_NAME);
    let mut stream = match tokio::time::timeout_at(deadline, UnixStream::connect(socket_path)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => return MethodDispatchTransportResult::Unavailable,
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
        AssistanceMethod::HookSubmit => return MethodDispatchTransportResult::Unavailable,
    };
    let version = if request.method() == AssistanceMethod::Edit {
        3
    } else {
        2
    };
    let wire = json!({
        "version": version,
        "request_id": request.request_id(),
        "correlation_id": request.correlation_id(),
        "opaque_attachment": request.opaque_attachment(),
        "method": "assistance.method_dispatch",
        "dispatch_method": method,
        "params_json": params,
    });
    let result = async {
        write_frame(&mut stream, &wire, limits.max_frame_bytes).await?;
        let reply: Value = read_frame(&mut stream, limits.max_frame_bytes).await?;
        parse_method_dispatch_reply(&reply, &request)
    };
    match tokio::time::timeout_at(deadline, result).await {
        Ok(Ok(reply)) => reply,
        Ok(Err(_)) | Err(_) => MethodDispatchTransportResult::Unavailable,
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

/// Validates one accepted peer and routes either unchanged health or one finite Assistance request.
async fn serve_connection(
    mut stream: UnixStream,
    generation: String,
    dispatcher: Option<Arc<dyn AssistanceDispatcher>>,
    transport_limits: Option<HookTransportLimits>,
) -> io::Result<()> {
    if peer_uid(stream.as_raw_fd())? != effective_uid() {
        return Ok(());
    }
    let request: Value = read_frame(&mut stream, MAX_V2_FRAME_BYTES).await?;
    match request.get("version").and_then(Value::as_u64) {
        Some(1) => serve_health(&mut stream, request, generation).await,
        Some(2 | 3) => match (dispatcher, transport_limits) {
            (Some(dispatcher), Some(limits)) => {
                serve_assistance_request(&mut stream, request, dispatcher, limits).await
            }
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Validates the unchanged v1 health request and emits only its existing correlated health reply.
async fn serve_health(
    stream: &mut UnixStream,
    request: Value,
    generation: String,
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
        status: "ok".to_owned(),
        daemon_generation: generation,
    };
    write_frame(stream, &response, MAX_V1_FRAME_BYTES).await
}

/// Sends a health request over one connection and decodes its one response.
async fn exchange(mut stream: UnixStream, request: &HealthRequest) -> io::Result<HealthResponse> {
    write_frame(&mut stream, request, MAX_V1_FRAME_BYTES).await?;
    read_frame(&mut stream, MAX_V1_FRAME_BYTES).await
}

/// Decodes and forwards exactly one finite v2/v3 Assistance request without inspecting semantics.
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
                limits.deadline,
                dispatcher.dispatch(AssistanceDispatch::MethodDispatch(dispatch.clone())),
            )
            .await;
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
                Ok(Ok(_)) | Ok(Err(_)) | Err(_) => json!({
                    "version": version,
                    "request_id": dispatch.request_id(),
                    "status": "unavailable",
                }),
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
    let expected_version = if request.method() == AssistanceMethod::Edit {
        3
    } else {
        2
    };
    if object.get("version").and_then(Value::as_u64) != Some(expected_version)
        || object.get("request_id").and_then(Value::as_str) != Some(request.request_id())
    {
        return Err(invalid_transport("method reply correlation mismatch"));
    }
    let Some(payload) = object.get("opaque_result_json") else {
        return Ok(MethodDispatchTransportResult::Unavailable);
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

/// Produces a fresh opaque daemon-generation identifier from the OS random source.
fn new_generation() -> Result<String, AppError> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::transport::AssistanceDispatchUnavailable;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};

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

        let error = finish_daemon(
            Err(AppError::Io(io::Error::from_raw_os_error(libc::EMFILE))),
            &mut connections,
            Some(&dispatcher),
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
}
