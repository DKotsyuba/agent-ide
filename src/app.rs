//! Private Unix daemon lifecycle and its health-only wire protocol.

/// Immutable restart-only limits and their provenance for Application infrastructure.
pub mod config;
/// Dedicated SQLite owner-thread mechanics and durable operation receipts for domain SQL.
pub mod store;

use std::fmt::{self, Display};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Semaphore;

const SOCKET_NAME: &str = "agent-ide.sock";
const LOCK_NAME: &str = "agent-ide.lock";
const WIRE_VERSION: u8 = 1;
const MAX_FRAME_BYTES: usize = 64 * 1024;
const MAX_REQUEST_ID_BYTES: usize = 128;

/// Reports whether a daemon answered the side-effect-free health request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorStatus {
    /// The endpoint answered with its current, non-authorizing daemon generation.
    Healthy { daemon_generation: String },
    /// No daemon could be reached without creating files or treating a stale endpoint as healthy.
    Unavailable,
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
/// Execution, Intelligence, or Assistance work. It returns only for a setup or listener failure.
pub async fn run_daemon(runtime_dir: RuntimeDir) -> Result<(), AppError> {
    let ipc = config::EffectiveConfig::defaults().ipc();
    let _lock = DaemonLock::acquire(runtime_dir.lock_path())?;
    let socket_path = runtime_dir.socket_path();
    retire_stale_socket(&socket_path, ipc.connection_deadline).await?;
    let listener = UnixListener::bind(&socket_path)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
    let _socket = OwnedSocket::new(socket_path)?;
    let generation = new_generation()?;
    let permits = Arc::new(Semaphore::new(ipc.max_connections));

    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let generation = generation.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = tokio::time::timeout(
                ipc.connection_deadline,
                serve_connection(stream, generation),
            )
            .await;
        });
    }

    #[allow(unreachable_code)]
    {
        drop((_socket, _lock));
        Ok(())
    }
}

/// Queries `runtime_dir` for a health reply without creating directories, locks, sockets, or a daemon.
pub async fn doctor(runtime_dir: &Path) -> Result<DoctorStatus, AppError> {
    let deadline = config::EffectiveConfig::defaults()
        .ipc()
        .connection_deadline;
    let socket_path = runtime_dir.join(SOCKET_NAME);
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

/// Validates one accepted peer, one bounded health request, and its correlated response.
async fn serve_connection(mut stream: UnixStream, generation: String) -> io::Result<()> {
    if peer_uid(stream.as_raw_fd())? != effective_uid() {
        return Ok(());
    }
    let request: HealthRequest = read_frame(&mut stream).await?;
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
    write_frame(&mut stream, &response).await
}

/// Sends a health request over one connection and decodes its one response.
async fn exchange(mut stream: UnixStream, request: &HealthRequest) -> io::Result<HealthResponse> {
    write_frame(&mut stream, request).await?;
    read_frame(&mut stream).await
}

/// Reads one length-prefixed JSON value without allocating for an unchecked frame length.
async fn read_frame<T>(stream: &mut UnixStream) -> io::Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
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
async fn write_frame<T>(stream: &mut UnixStream, value: &T) -> io::Result<()>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.len() > MAX_FRAME_BYTES {
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
