//! Command-line entrypoint for managed and legacy MCP, native hooks, the daemon, and diagnostics.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::time::Duration;

use agent_ide::app::{
    AppError, DoctorReport, DoctorStatus, RuntimeDir, config::EffectiveConfig, doctor_report,
    run_daemon_with_assistance,
};
use agent_ide::assistance::{
    assembly::ProductDispatcher,
    facade::StdioFacade,
    host_binding::HostKind,
    launcher::{AcceptedExecutable, LauncherConfig},
};
use agent_ide::execution::{D03ProfileEvidence, HostSandboxState, PersistedProfileRecord};
use rmcp::{serve_server, transport::io::stdio};

/// Selects an explicit mode; MCP writes only protocol messages to stdout and never autostarts.
#[tokio::main]
async fn main() -> ExitCode {
    // Claude's workspace identity is captured before argument parsing or asynchronous setup and is
    // never accepted from an MCP call, hook payload, or later environment read.
    let claude_project_dir = std::env::var_os("CLAUDE_PROJECT_DIR");
    match command(std::env::args_os().skip(1)) {
        Ok(Command::Daemon { runtime_dir }) => match RuntimeDir::prepare_for_daemon(runtime_dir) {
            Ok(runtime_dir) => match run_daemon_with_assistance(
                runtime_dir,
                Arc::new(match std::env::var("AGENT_IDE_LAUNCHER_CONFIG") {
                    Ok(path) => match LauncherConfig::read(std::path::Path::new(&path)) {
                        Ok(config)
                            if std::env::var("AGENT_IDE_MANAGED_CODEX_ATTACHMENT")
                                .ok()
                                .is_some_and(|attachment| config.target(&attachment).is_some()) =>
                        {
                            ProductDispatcher::with_managed_codex_launcher(config)
                        }
                        Ok(config) => ProductDispatcher::with_launcher(config),
                        Err(_) => return fail(AppError::InvalidResponse),
                    },
                    Err(std::env::VarError::NotPresent) => ProductDispatcher::default(),
                    Err(_) => return fail(AppError::InvalidResponse),
                }),
                EffectiveConfig::defaults(),
            )
            .await
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(error),
            },
            Err(error) => fail(error),
        },
        Ok(Command::CodexHook { runtime_dir }) => {
            agent_ide::assistance::codex_hook::run(
                &runtime_dir,
                std::env::var("AGENT_IDE_HOST_ATTACHMENT").ok(),
                HostKind::Codex,
            )
            .await;
            ExitCode::SUCCESS
        }
        Ok(Command::ClaudeHook { runtime_dir }) => {
            agent_ide::assistance::codex_hook::run(
                &runtime_dir,
                std::env::var("AGENT_IDE_HOST_ATTACHMENT").ok(),
                HostKind::Claude,
            )
            .await;
            ExitCode::SUCCESS
        }
        Ok(Command::Mcp { runtime_dir }) => {
            let facade = match std::env::var("AGENT_IDE_HOST_ATTACHMENT") {
                Ok(attachment) => {
                    match StdioFacade::with_host_attachment(runtime_dir, attachment) {
                        Some(facade) => facade,
                        None => return fail(AppError::InvalidResponse),
                    }
                }
                Err(std::env::VarError::NotPresent) => StdioFacade::new(runtime_dir),
                Err(std::env::VarError::NotUnicode(_)) => return fail(AppError::InvalidResponse),
            };
            match serve_server(facade, stdio()).await {
                Ok(service) => match service.waiting().await {
                    Ok(_) => ExitCode::SUCCESS,
                    Err(_) => fail(AppError::InvalidResponse),
                },
                Err(_) => fail(AppError::InvalidResponse),
            }
        }
        Ok(Command::ManagedMcp {
            launcher_template,
            host,
        }) => {
            let candidate = match host {
                // Codex retains its existing current-directory identity contract.
                ManagedHost::Codex => std::env::current_dir(),
                ManagedHost::Claude => canonical_claude_project(claude_project_dir),
            };
            run_managed_mcp(launcher_template, candidate, host).await
        }
        Ok(Command::AutoManagedMcp { launcher_template }) => {
            let (host, candidate) = auto_managed_candidate(claude_project_dir);
            run_managed_mcp(launcher_template, candidate, host).await
        }
        Ok(Command::ManagedClaudeHook) => {
            run_managed_claude_hook(claude_project_dir).await;
            ExitCode::SUCCESS
        }
        Ok(Command::ClaudeWorker {
            runtime_dir,
            attachment,
            detail_ref,
        }) => {
            agent_ide::assistance::claude_helper::run(
                &runtime_dir,
                Some(attachment),
                Some(detail_ref),
            )
            .await;
            ExitCode::SUCCESS
        }
        Ok(Command::Doctor { runtime_dir }) => match doctor_report(&runtime_dir).await {
            Ok(report) => {
                let healthy = matches!(report.status, DoctorStatus::Healthy { .. });
                print_doctor_report(&report);
                if healthy {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(error) => fail(error),
        },
        Ok(Command::EvidenceRecord {
            sandbox_state,
            profile_id,
            revision,
            provider_binary,
            toolchain,
            configuration,
            trust,
            transport,
            d03_evidence,
        }) => match evidence_record(
            &sandbox_state,
            &profile_id,
            revision,
            D03ProfileEvidence {
                provider_binary,
                toolchain,
                configuration,
                trust,
                transport,
                d03_evidence,
            },
        ) {
            Ok(fragment) => {
                println!("{fragment}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        },
        Ok(Command::EvidenceExecutable { identity, path }) => {
            match evidence_executable(&identity, path) {
                Ok(fragment) => {
                    println!("{fragment}");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            }
        }
        Ok(Command::LauncherCheck { path }) => match launcher_check(&path) {
            Ok(()) => {
                println!("ok");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        },
        Err(error) => fail(error),
    }
}

/// Reads bounded sandbox-state JSON from `path` without inferring or widening its authority.
///
/// `path` must contain the exact object a host would advertise on `codex/sandbox-state-meta`.
/// Returns [`AppError::InvalidResponse`] if the file cannot be read or the envelope is malformed
/// or names an unsupported profile.
fn read_sandbox_state(path: &std::path::Path) -> Result<HostSandboxState, AppError> {
    const MAX_SANDBOX_STATE_BYTES: u64 = 64 * 1024;
    let mut raw = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_SANDBOX_STATE_BYTES + 1)
                .read_to_string(&mut raw)
        })
        .map_err(|_| AppError::InvalidResponse)?;
    if raw.len() as u64 > MAX_SANDBOX_STATE_BYTES {
        return Err(AppError::InvalidResponse);
    }
    HostSandboxState::parse_json(&raw).map_err(|_| AppError::InvalidResponse)
}

/// Builds the exact `{record, sandbox_state}` launcher-profile fragment for one verified D03 run.
///
/// This is a pure offline helper for an operator preparing a launcher configuration file: it
/// reuses [`PersistedProfileRecord::from_execution_evidence`] and never starts a daemon, spawns a
/// provider, or writes any file. `sandbox_state` names a file holding the exact captured
/// `codex/sandbox-state-meta` envelope for the tested run; `evidence` carries the non-state D03
/// identities. The returned JSON text is the exact shape a `profiles` entry in a launcher
/// configuration expects. Returns
/// [`AppError::InvalidResponse`] for an unreadable/malformed sandbox-state file or evidence that
/// [`PersistedProfileRecord::from_execution_evidence`] rejects (an empty identity or zero revision).
fn evidence_record(
    sandbox_state: &std::path::Path,
    profile_id: &str,
    revision: u32,
    evidence: D03ProfileEvidence,
) -> Result<String, AppError> {
    let state = read_sandbox_state(sandbox_state)?;
    let record =
        PersistedProfileRecord::from_execution_evidence(profile_id, revision, evidence, &state)
            .map_err(|_| AppError::InvalidResponse)?;
    let record_value: serde_json::Value =
        serde_json::from_str(&record.to_json()).map_err(|_| AppError::InvalidResponse)?;
    let sandbox_value: serde_json::Value =
        serde_json::from_str(state.sandbox_state_json()).map_err(|_| AppError::InvalidResponse)?;
    Ok(serde_json::json!({ "record": record_value, "sandbox_state": sandbox_value }).to_string())
}

/// Builds one `{path, identity, blake3}` accepted-executable fragment by measuring `path`'s bytes.
///
/// This is a pure offline helper for an operator preparing a launcher configuration file: it
/// reuses [`AcceptedExecutable::from_path`] and never starts a daemon or launches `path`.
/// `identity` is the operator-chosen binary/version label paired with the measured digest; it is
/// never inferred from the file. Returns [`AppError::InvalidResponse`] for a malformed identity or
/// a `path` that is not a stable, readable, executable regular file.
fn evidence_executable(identity: &str, path: PathBuf) -> Result<String, AppError> {
    let executable =
        AcceptedExecutable::from_path(path, identity).map_err(|_| AppError::InvalidResponse)?;
    Ok(serde_json::json!({
        "path": executable.path.to_string_lossy(),
        "identity": executable.identity,
        "blake3": executable.blake3,
    })
    .to_string())
}

/// Loads and verifies one launcher configuration's accepted executables without a running daemon.
///
/// Reuses [`LauncherConfig::read`] and [`LauncherConfig::verify`], the exact checks the daemon
/// performs at startup, so an operator can validate a configuration file offline before pointing
/// `AGENT_IDE_LAUNCHER_CONFIG` at it. Returns [`AppError::InvalidResponse`] for an unreadable or
/// malformed configuration, or an executable whose current bytes no longer match its accepted
/// digest.
fn launcher_check(path: &std::path::Path) -> Result<(), AppError> {
    let config = LauncherConfig::read(path).map_err(|_| AppError::InvalidResponse)?;
    config.verify().map_err(|_| AppError::InvalidResponse)
}

/// Prints a fixed complete doctor report without creating runtime state or opening peer services.
fn print_doctor_report(report: &DoctorReport) {
    let status = match &report.status {
        DoctorStatus::Healthy { daemon_generation } => format!("healthy:{daemon_generation}"),
        DoctorStatus::Unavailable => "unavailable".to_owned(),
    };
    let ipc = report.config.ipc();
    let store = report.config.store();
    println!("status={status}");
    println!("runtime={:?}", report.runtime);
    println!("endpoint={:?}", report.endpoint);
    println!("lock={:?}", report.lock);
    println!("config.generation={}", report.config.generation());
    println!(
        "config.ipc.connection_deadline_ms={}",
        ipc.connection_deadline.as_millis()
    );
    println!("config.ipc.max_connections={}", ipc.max_connections);
    println!("config.store.queue_capacity={}", store.queue_capacity);
    println!(
        "config.store.busy_timeout_ms={}",
        store.busy_timeout.as_millis()
    );
    println!(
        "config.store.request_deadline_ms={}",
        store.request_deadline.as_millis()
    );
    println!("config.store.receipt_capacity={}", store.receipt_capacity);
    println!("protocol.health=v1");
    println!("protocol.assistance_transport=v2");
    println!("control.daemon_autostart=unsupported");
    println!("control.workspace_scan=unsupported");
    println!("control.lsp_open=unsupported");
    println!("control.cache_retirement=requires-peer-verified-closure-or-reset");
}

/// Prints a compact local error and maps it to a nonzero command exit code.
fn fail(error: AppError) -> ExitCode {
    eprintln!("agent-ide: {error}");
    ExitCode::FAILURE
}

/// Holds the explicit runtime directory required by each supported executable mode.
enum Command {
    /// Serves health and finite Assistance dispatch until interrupted or killed.
    Daemon { runtime_dir: PathBuf },
    /// Serves the static six-tool MCP surface on stdio without creating local runtime state.
    Mcp { runtime_dir: PathBuf },
    /// Owns one private daemon and serves the same static six-tool surface until stdio ends.
    ManagedMcp {
        /// Absolute one-target launcher template rebound to this process and captured candidate.
        launcher_template: PathBuf,
        /// Host contract selected by the explicit managed MCP flag.
        host: ManagedHost,
    },
    /// Selects one existing managed host contract from the startup environment.
    AutoManagedMcp {
        /// Absolute one-target launcher template used by either selected managed host.
        launcher_template: PathBuf,
    },
    /// Submits one managed Claude hook through the project-derived private rendezvous.
    ManagedClaudeHook,
    /// Submits one bounded native Codex hook and exits successfully on every ingress failure.
    CodexHook {
        /// Existing daemon endpoint directory; never created by the hook command.
        runtime_dir: PathBuf,
    },
    /// Submits one bounded native Claude Code hook and always fails open at host ingress.
    ClaudeHook {
        /// Existing daemon endpoint directory; never created by the hook command.
        runtime_dir: PathBuf,
    },
    /// Runs one bounded foreground Claude helper operation and exits.
    ///
    /// Launched by the model through its ordinary shell tool, so every child it starts inherits
    /// the host's own sandbox. It creates no runtime state and never autostarts a daemon.
    ClaudeWorker {
        /// Existing daemon endpoint directory; never created by the helper command.
        runtime_dir: PathBuf,
        /// Opaque private transport attachment the operation was minted on.
        attachment: String,
        /// Action-scoped single-use handle this helper was launched to claim.
        detail_ref: String,
    },
    /// Queries an existing daemon without creating a directory or daemon process.
    Doctor { runtime_dir: PathBuf },
    /// Emits one `{record, sandbox_state}` launcher-profile fragment from verified D03 evidence.
    ///
    /// Pure offline evidence-formatting; creates no runtime state and never starts a daemon.
    EvidenceRecord {
        /// File holding the exact captured `codex/sandbox-state-meta` envelope for the tested run.
        sandbox_state: PathBuf,
        /// Stable Execution-owned profile-template identity for this record.
        profile_id: String,
        /// Monotonic Execution-owned template revision; must be nonzero.
        revision: u32,
        /// Exact provider binary identity observed by the D03 run.
        provider_binary: String,
        /// Exact toolchain identity observed by the D03 run.
        toolchain: String,
        /// Effective provider configuration identity observed by the D03 run.
        configuration: String,
        /// Effective trust decision identity observed by the D03 run.
        trust: String,
        /// Sandbox transport/mechanism identity observed by the D03 run.
        transport: String,
        /// Immutable D03 evidence identity for this tested record.
        d03_evidence: String,
    },
    /// Emits one `{path, identity, blake3}` accepted-executable fragment for a measured file.
    ///
    /// Pure offline evidence-formatting; creates no runtime state and never launches `path`.
    EvidenceExecutable {
        /// Operator-chosen binary/version identity paired with the measured digest.
        identity: String,
        /// Absolute executable whose current bytes are measured.
        path: PathBuf,
    },
    /// Verifies one launcher configuration's accepted executables without starting a daemon.
    LauncherCheck {
        /// Launcher configuration file to load and verify.
        path: PathBuf,
    },
}

/// Selects the host-specific identity and binding behavior of a self-contained managed MCP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedHost {
    /// Existing Codex mode derives its candidate from the startup current directory and binds directly.
    Codex,
    /// Claude derives its candidate only from startup-captured `CLAUDE_PROJECT_DIR` and uses hooks.
    Claude,
}

/// Selects the existing managed host and captures its candidate without fallback between hosts.
///
/// A present `claude_project_dir` always selects Claude, including when canonical validation
/// fails; the returned error then drives the existing Claude fail-open MCP path. An absent value
/// selects Codex and returns the process current directory, preserving its existing error behavior.
fn auto_managed_candidate(
    claude_project_dir: Option<OsString>,
) -> (ManagedHost, std::io::Result<PathBuf>) {
    match claude_project_dir {
        Some(project) => (ManagedHost::Claude, canonical_claude_project(Some(project))),
        None => (ManagedHost::Codex, std::env::current_dir()),
    }
}

/// Matches `pairs` against the exact ordered `--flag value` sequence in `expected`.
///
/// Returns each value as `&str` in `expected`'s order, or `None` for a wrong element count, a
/// flag out of order or misspelled, or a value that is not valid UTF-8.
fn ordered_flags<'a>(pairs: &'a [OsString], expected: &[&str]) -> Option<Vec<&'a str>> {
    if pairs.len() != expected.len() * 2 {
        return None;
    }
    expected
        .iter()
        .enumerate()
        .map(|(index, flag)| {
            (pairs[index * 2] == *flag)
                .then(|| pairs[index * 2 + 1].to_str())
                .flatten()
        })
        .collect()
}

/// Rejects unknown, missing, and extra CLI arguments before any filesystem or daemon action.
fn command(arguments: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let arguments = arguments.collect::<Vec<_>>();
    if arguments.as_slice() == [OsString::from("claude-hook")] {
        return Ok(Command::ManagedClaudeHook);
    }
    // The helper command has its own fixed longer shape; every other mode keeps the exact
    // three-argument form it already had, so no existing invocation changes meaning.
    if let [
        mode,
        runtime_flag,
        runtime_dir,
        attachment_flag,
        attachment,
        detail_flag,
        detail_ref,
    ] = arguments.as_slice()
        && mode == "claude-worker"
    {
        if runtime_flag != "--runtime-dir"
            || attachment_flag != "--attachment"
            || detail_flag != "--detail-ref"
        {
            return Err(AppError::InvalidResponse);
        }
        let (Some(attachment), Some(detail_ref)) = (attachment.to_str(), detail_ref.to_str())
        else {
            return Err(AppError::InvalidResponse);
        };
        return Ok(Command::ClaudeWorker {
            runtime_dir: PathBuf::from(runtime_dir),
            attachment: attachment.to_owned(),
            detail_ref: detail_ref.to_owned(),
        });
    }
    // `evidence record` has its own fixed nine-flag shape, in the exact declared order; no flag
    // may be reordered, omitted, or repeated.
    if let [mode, sub, rest @ ..] = arguments.as_slice()
        && mode == "evidence"
        && sub == "record"
    {
        let values = ordered_flags(
            rest,
            &[
                "--sandbox-state",
                "--profile-id",
                "--revision",
                "--provider-binary",
                "--toolchain",
                "--configuration",
                "--trust",
                "--transport",
                "--d03-evidence",
            ],
        )
        .ok_or(AppError::InvalidResponse)?;
        let revision = values[2]
            .parse::<u32>()
            .map_err(|_| AppError::InvalidResponse)?;
        return Ok(Command::EvidenceRecord {
            sandbox_state: PathBuf::from(values[0]),
            profile_id: values[1].to_owned(),
            revision,
            provider_binary: values[3].to_owned(),
            toolchain: values[4].to_owned(),
            configuration: values[5].to_owned(),
            trust: values[6].to_owned(),
            transport: values[7].to_owned(),
            d03_evidence: values[8].to_owned(),
        });
    }
    // `evidence executable` measures one file; the identity flag always precedes the bare path.
    if let [mode, sub, identity_flag, identity, path] = arguments.as_slice()
        && mode == "evidence"
        && sub == "executable"
    {
        if identity_flag != "--identity" {
            return Err(AppError::InvalidResponse);
        }
        let Some(identity) = identity.to_str() else {
            return Err(AppError::InvalidResponse);
        };
        return Ok(Command::EvidenceExecutable {
            identity: identity.to_owned(),
            path: PathBuf::from(path),
        });
    }
    // `launcher check` takes a bare configuration path; no `--runtime-dir` is involved.
    if let [mode, sub, path] = arguments.as_slice()
        && mode == "launcher"
        && sub == "check"
    {
        return Ok(Command::LauncherCheck {
            path: PathBuf::from(path),
        });
    }
    let [mode, flag, value] = arguments.as_slice() else {
        return Err(AppError::InvalidResponse);
    };
    if mode == "mcp" && flag == "--auto-launcher-template" {
        return Ok(Command::AutoManagedMcp {
            launcher_template: PathBuf::from(value),
        });
    }
    if mode == "mcp" && flag == "--launcher-template" {
        return Ok(Command::ManagedMcp {
            launcher_template: PathBuf::from(value),
            host: ManagedHost::Codex,
        });
    }
    if mode == "mcp" && flag == "--claude-launcher-template" {
        return Ok(Command::ManagedMcp {
            launcher_template: PathBuf::from(value),
            host: ManagedHost::Claude,
        });
    }
    if flag != "--runtime-dir" {
        return Err(AppError::InvalidResponse);
    }
    let runtime_dir = PathBuf::from(value);
    match mode.to_str() {
        Some("daemon") => Ok(Command::Daemon { runtime_dir }),
        Some("doctor") => Ok(Command::Doctor { runtime_dir }),
        Some("mcp") => Ok(Command::Mcp { runtime_dir }),
        Some("codex-hook") => Ok(Command::CodexHook { runtime_dir }),
        Some("claude-hook") => Ok(Command::ClaudeHook { runtime_dir }),
        _ => Err(AppError::InvalidResponse),
    }
}

/// Owns the identity of one freshly created managed runtime tree.
///
/// The recorded device and inode fence cleanup against pathname replacement. The directory is
/// private to this MCP process and must be removed only after its exact daemon child is reaped.
struct ManagedRuntime {
    /// Short absolute directory used by the owned daemon's Unix socket and private state.
    path: PathBuf,
    /// Device identity captured immediately after exclusive creation.
    device: u64,
    /// Inode identity captured immediately after exclusive creation.
    inode: u64,
}

impl ManagedRuntime {
    /// Creates one unpredictable private directory below the canonical OS temp root with mode `0700`.
    ///
    /// At most sixteen exclusive attempts are made. No existing path is opened, repaired, or
    /// removed. Returns an I/O error when entropy, creation, or identity capture fails.
    fn create() -> std::io::Result<Self> {
        let temporary_root = fs::canonicalize(std::env::temp_dir())?;
        for _ in 0..16 {
            let name = format!("ai-{}", random_hex(8)?);
            let path = temporary_root.join(name);
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&path) {
                Ok(()) => {
                    let metadata = fs::symlink_metadata(&path)?;
                    return Ok(Self {
                        path,
                        device: metadata.dev(),
                        inode: metadata.ino(),
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "managed runtime collision limit reached",
        ))
    }

    /// Exclusively creates the one deterministic Claude runtime directory with exact mode `0700`.
    ///
    /// `path` must be the project-derived child of the canonical `/tmp` root. An existing path is
    /// never opened, repaired, removed, or adopted, so a concurrent second MCP stays disconnected
    /// and cannot overwrite the first owner's launcher or attachment. The captured device/inode
    /// identity fences the eventual recursive cleanup exactly as in managed Codex mode.
    fn create_deterministic(path: PathBuf) -> std::io::Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700).create(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed Claude runtime is not private",
            ));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    /// Writes the already validated bound launcher bytes once with mode `0600`.
    ///
    /// The returned absolute path is passed only to the exact daemon child. Existing files are
    /// never overwritten, and a short write leaves managed startup unavailable.
    fn write_launcher(&self, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.path.join("launcher.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(path)
    }

    /// Writes one project-bound random Claude attachment record exactly once with mode `0600`.
    ///
    /// The first field is the full project identity whose prefix selected this short runtime path;
    /// the second is the unguessable transport attachment. A hook validates both fields before it
    /// attempts IPC. Existing files are never followed or overwritten, and no value is rendered.
    fn write_claude_attachment(&self, project: &Path, attachment: &str) -> std::io::Result<()> {
        let record = format!("{} {attachment}\n", claude_project_identity(project));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(CLAUDE_ATTACHMENT_FILE))?;
        file.write_all(record.as_bytes())?;
        file.sync_all()
    }

    /// Removes this runtime tree only while its original private directory identity still matches.
    ///
    /// A missing tree is already clean. A symlink, owner/mode change, device change, or inode
    /// replacement is left untouched and reported as an error. Callers must reap the daemon first.
    fn remove(self) -> std::io::Result<()> {
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.dev() != self.device
            || metadata.ino() != self.inode
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "managed runtime identity changed",
            ));
        }
        fs::remove_dir_all(self.path)
    }
}

/// Returns `bytes` of operating-system randomness as lowercase hexadecimal.
///
/// The output is used only for unguessable private runtime and attachment names. Entropy failure
/// rejects managed startup rather than falling back to PID, time, or a reusable identifier.
fn random_hex(bytes: usize) -> std::io::Result<String> {
    let mut random = vec![0_u8; bytes];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Returns whether a path is absolute and lexically normalized without parent traversal.
fn absolute_local_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

/// Fixed short namespace for deterministic Claude runtimes below the canonical `/tmp` directory.
const CLAUDE_RUNTIME_PREFIX: &str = "ai-c-";
/// Fixed owner-only file carrying the full project identity and random transport attachment.
const CLAUDE_ATTACHMENT_FILE: &str = "attachment";
/// Exact record length: 64 digest bytes, one separator, 64 attachment bytes, and one newline.
const CLAUDE_ATTACHMENT_BYTES: u64 = 130;

/// Resolves one startup-captured Claude project value to an absolute canonical directory.
///
/// Relative, non-normalized, missing, nonexistent, and non-directory values are rejected without
/// falling back to the process current directory. The returned path is the sole managed Claude
/// candidate and the sole input to its deterministic runtime identity.
fn canonical_claude_project(value: Option<OsString>) -> std::io::Result<PathBuf> {
    let path = PathBuf::from(value.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "CLAUDE_PROJECT_DIR is unavailable",
        )
    })?);
    if !absolute_local_path(&path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CLAUDE_PROJECT_DIR is not absolute and normalized",
        ));
    }
    let project = fs::canonicalize(path)?;
    if !absolute_local_path(&project)
        || !fs::symlink_metadata(&project).is_ok_and(|metadata| metadata.is_dir())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "CLAUDE_PROJECT_DIR is not a canonical directory",
        ));
    }
    Ok(project)
}

/// Returns the full BLAKE3 digest of one canonical Claude project root's raw path bytes.
fn claude_project_identity(project: &Path) -> String {
    blake3::hash(project.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

/// Derives the one short deterministic private runtime path for a canonical Claude project root.
///
/// The full digest remains in the attachment record to reject a theoretical collision in the
/// sixteen-hex-character pathname prefix. The returned parent is canonical `/tmp`; no caller or
/// model path can redirect the rendezvous elsewhere.
fn claude_runtime_path(project: &Path) -> std::io::Result<PathBuf> {
    let identity = claude_project_identity(project);
    Ok(fs::canonicalize(Path::new("/tmp"))?
        .join(format!("{CLAUDE_RUNTIME_PREFIX}{}", &identity[..16])))
}

/// Returns whether a string is exactly one generated 32-byte lowercase hexadecimal attachment.
fn valid_random_attachment(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// Reads one project-derived managed Claude rendezvous after strict owner/mode/identity checks.
///
/// The directory must be the expected nonsymlink owned by the effective user with exact mode
/// `0700`. Its fixed attachment file is opened with `O_NOFOLLOW`, must be a regular owner-only
/// `0600` file of the exact bounded size, and must carry this project's full digest plus one valid
/// random attachment. Any missing, stale, replaced, or corrupt state is rejected without repair.
fn read_claude_attachment(project: &Path) -> std::io::Result<(PathBuf, String)> {
    let runtime = claude_runtime_path(project)?;
    let metadata = fs::symlink_metadata(&runtime)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed Claude runtime identity is invalid",
        ));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(runtime.join(CLAUDE_ATTACHMENT_FILE))?;
    let attachment_metadata = file.metadata()?;
    if !attachment_metadata.is_file()
        || attachment_metadata.uid() != unsafe { libc::geteuid() }
        || attachment_metadata.permissions().mode() & 0o777 != 0o600
        || attachment_metadata.len() != CLAUDE_ATTACHMENT_BYTES
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "managed Claude attachment identity is invalid",
        ));
    }
    let mut record = String::new();
    file.take(CLAUDE_ATTACHMENT_BYTES + 1)
        .read_to_string(&mut record)?;
    let Some((identity, attachment)) = record
        .strip_suffix('\n')
        .and_then(|value| value.split_once(' '))
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Claude attachment is malformed",
        ));
    };
    if identity != claude_project_identity(project) || !valid_random_attachment(attachment) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Claude attachment does not match the project",
        ));
    }
    Ok((runtime, attachment.to_owned()))
}

/// Submits one argument-free managed Claude hook through its exact project rendezvous.
///
/// Missing project identity, runtime, attachment, or daemon state returns silently. Once validated,
/// the existing bounded Claude parser, 250 ms total deadline, sanitized transport, exact lifecycle
/// correlation, feedback rendering, and foreground-helper recognition remain unchanged.
async fn run_managed_claude_hook(project: Option<OsString>) {
    let Ok(project) = canonical_claude_project(project) else {
        return;
    };
    let Ok((runtime, attachment)) = read_claude_attachment(&project) else {
        return;
    };
    agent_ide::assistance::codex_hook::run(&runtime, Some(attachment), HostKind::Claude).await;
}

/// Starts, health-checks, serves, and tears down one self-contained managed MCP generation.
///
/// Setup failure still serves the static six tools through a connect-only unavailable facade.
/// Successful setup binds one captured local candidate to one random attachment, starts exactly
/// one daemon child, and tears it down on stdio EOF, MCP cancellation, SIGINT, or SIGTERM.
async fn run_managed_mcp(
    launcher_template: PathBuf,
    candidate: std::io::Result<PathBuf>,
    host: ManagedHost,
) -> ExitCode {
    let Ok(candidate) = candidate else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None).await;
    };
    let runtime = match host {
        ManagedHost::Codex => ManagedRuntime::create(),
        ManagedHost::Claude => {
            claude_runtime_path(&candidate).and_then(ManagedRuntime::create_deterministic)
        }
    };
    let Ok(runtime) = runtime else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None).await;
    };
    let runtime_path = runtime.path.clone();
    let started = start_managed_daemon(&runtime, &launcher_template, candidate, host).await;
    match started {
        Ok((attachment, child)) => {
            let Some(facade) = StdioFacade::with_host_attachment(runtime_path, attachment) else {
                terminate_owned_daemon(child).await;
                let _ = runtime.remove();
                return serve_managed_stdio(StdioFacade::unavailable(), None, None).await;
            };
            serve_managed_stdio(facade, Some(child), Some(runtime)).await
        }
        Err(()) => {
            let _ = runtime.remove();
            serve_managed_stdio(StdioFacade::unavailable(), None, None).await
        }
    }
}

/// Validates the managed inputs and returns the exact healthy daemon child and private attachment.
///
/// The candidate must be the one captured by the parent and remain an absolute local directory.
/// Git identity is deliberately discovered later by the existing worker activation path. Launcher
/// executables and profiles are validated through [`LauncherConfig`] before the child starts.
async fn start_managed_daemon(
    runtime: &ManagedRuntime,
    launcher_template: &Path,
    candidate: PathBuf,
    host: ManagedHost,
) -> Result<(String, tokio::process::Child), ()> {
    if !absolute_local_path(&candidate)
        || !fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.is_dir())
        || !absolute_local_path(launcher_template)
    {
        return Err(());
    }
    let attachment = random_hex(32).map_err(|_| ())?;
    let (launcher, bytes) =
        LauncherConfig::bind_one_candidate(launcher_template, &attachment, &candidate)
            .map_err(|_| ())?;
    if host == ManagedHost::Claude
        && launcher
            .target(&attachment)
            .is_none_or(|target| target.claude_profile.is_none())
    {
        return Err(());
    }
    launcher.verify().map_err(|_| ())?;
    let launcher_path = runtime.write_launcher(&bytes).map_err(|_| ())?;
    if host == ManagedHost::Claude {
        runtime
            .write_claude_attachment(&candidate, &attachment)
            .map_err(|_| ())?;
    }
    let mut command = tokio::process::Command::new(std::env::current_exe().map_err(|_| ())?);
    command
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime.path)
        .env("AGENT_IDE_LAUNCHER_CONFIG", launcher_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match host {
        ManagedHost::Codex => {
            command.env("AGENT_IDE_MANAGED_CODEX_ATTACHMENT", &attachment);
        }
        ManagedHost::Claude => {
            // Claude deliberately uses the existing hook-correlated daemon path.
            command.env_remove("AGENT_IDE_MANAGED_CODEX_ATTACHMENT");
        }
    }
    let mut child = command.spawn().map_err(|_| ())?;
    if !health_check_owned_daemon(&mut child, &runtime.path).await {
        terminate_owned_daemon(child).await;
        return Err(());
    }
    Ok((attachment, child))
}

/// Waits a bounded interval for the exact child to answer the existing side-effect-free health RPC.
async fn health_check_owned_daemon(child: &mut tokio::process::Child, runtime: &Path) -> bool {
    tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if child.try_wait().ok().flatten().is_some() {
                return false;
            }
            if doctor_report(runtime)
                .await
                .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. }))
            {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or(false)
}

/// Serves one static MCP facade and always cleans up an optional owned daemon/runtime generation.
async fn serve_managed_stdio(
    facade: StdioFacade,
    child: Option<tokio::process::Child>,
    runtime: Option<ManagedRuntime>,
) -> ExitCode {
    let served = match serve_server(facade, stdio()).await {
        Ok(service) => {
            tokio::select! {
                result = service.waiting() => result.is_ok(),
                () = managed_termination_signal() => true,
            }
        }
        Err(_) => false,
    };
    if let Some(child) = child {
        terminate_owned_daemon(child).await;
    }
    if let Some(runtime) = runtime {
        let _ = runtime.remove();
    }
    if served {
        ExitCode::SUCCESS
    } else {
        fail(AppError::InvalidResponse)
    }
}

/// Resolves after the first process SIGINT or SIGTERM; registration failure waits for stdio EOF.
async fn managed_termination_signal() {
    let (Ok(mut interrupt), Ok(mut terminate)) = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
    ) else {
        std::future::pending::<()>().await;
        return;
    };
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
    }
}

/// Sends SIGTERM to the exact owned daemon PID, waits for its provider cleanup, then force-reaps.
async fn terminate_owned_daemon(mut child: tokio::process::Child) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    if let Some(pid) = child.id() {
        let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }
    if tokio::time::timeout(Duration::from_secs(42), child.wait())
        .await
        .is_err()
    {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the exact `OsString` argv `command` expects from plain string arguments.
    fn args(values: &[&str]) -> impl Iterator<Item = OsString> {
        values
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The shared flag helper matches an exact ordered sequence and rejects a wrong flag or arity.
    #[test]
    fn ordered_flags_matches_order_and_rejects_wrong_flag_or_arity() {
        let pairs = [
            OsString::from("--a"),
            OsString::from("1"),
            OsString::from("--b"),
            OsString::from("2"),
        ];
        assert_eq!(ordered_flags(&pairs, &["--a", "--b"]), Some(vec!["1", "2"]));
        assert_eq!(ordered_flags(&pairs, &["--b", "--a"]), None);
        assert_eq!(ordered_flags(&pairs[..2], &["--a", "--b"]), None);
    }

    /// `evidence record` parses its nine ordered flags and emits the `{record, sandbox_state}`
    /// fragment a launcher configuration's `profiles` entry expects.
    #[test]
    fn evidence_record_round_trips_through_the_cli() {
        let sandbox_state = std::env::temp_dir().join(format!(
            "agent-ide-evidence-record-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &sandbox_state,
            r#"{"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":false}"#,
        )
        .unwrap();
        let parsed = command(args(&[
            "evidence",
            "record",
            "--sandbox-state",
            sandbox_state.to_str().unwrap(),
            "--profile-id",
            "accepted-disabled",
            "--revision",
            "1",
            "--provider-binary",
            "accepted-git",
            "--toolchain",
            "toolchain",
            "--configuration",
            "default",
            "--trust",
            "accepted-local",
            "--transport",
            "direct",
            "--d03-evidence",
            "accepted-d03",
        ]))
        .unwrap();
        let Command::EvidenceRecord {
            sandbox_state: parsed_state,
            profile_id,
            revision,
            provider_binary,
            toolchain,
            configuration,
            trust,
            transport,
            d03_evidence,
        } = parsed
        else {
            panic!("expected EvidenceRecord");
        };
        let fragment = evidence_record(
            &parsed_state,
            &profile_id,
            revision,
            D03ProfileEvidence {
                provider_binary,
                toolchain,
                configuration,
                trust,
                transport,
                d03_evidence,
            },
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();
        assert_eq!(value["record"]["profile_id"], "accepted-disabled");
        assert_eq!(value["sandbox_state"]["sandboxCwd"], "/private/tmp");
        assert!(matches!(
            command(args(&["evidence", "record", "--profile-id", "x"])),
            Err(AppError::InvalidResponse)
        ));
        std::fs::write(&sandbox_state, vec![b' '; 64 * 1024 + 1]).unwrap();
        assert!(matches!(
            read_sandbox_state(&sandbox_state),
            Err(AppError::InvalidResponse)
        ));
        std::fs::remove_file(sandbox_state).unwrap();
    }

    /// `evidence executable` parses its identity flag and emits a measured `{path, identity,
    /// blake3}` fragment.
    #[test]
    fn evidence_executable_round_trips_through_the_cli() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "agent-ide-evidence-executable-{}",
            std::process::id()
        ));
        std::fs::write(&path, b"#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let parsed = command(args(&[
            "evidence",
            "executable",
            "--identity",
            "accepted-git",
            path.to_str().unwrap(),
        ]))
        .unwrap();
        let Command::EvidenceExecutable {
            identity,
            path: parsed_path,
        } = parsed
        else {
            panic!("expected EvidenceExecutable");
        };
        let fragment = evidence_executable(&identity, parsed_path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();
        assert_eq!(value["identity"], "accepted-git");
        assert_eq!(value["blake3"].as_str().unwrap().len(), 64);
        std::fs::remove_file(path).unwrap();
    }

    /// `launcher check` parses a bare path and reports the same failure as a direct call for a
    /// configuration that fails to load.
    #[test]
    fn launcher_check_round_trips_through_the_cli() {
        let path =
            std::env::temp_dir().join(format!("agent-ide-launcher-check-{}", std::process::id()));
        std::fs::write(&path, b"not json").unwrap();
        let parsed = command(args(&["launcher", "check", path.to_str().unwrap()])).unwrap();
        let Command::LauncherCheck { path: parsed_path } = parsed else {
            panic!("expected LauncherCheck");
        };
        assert!(matches!(
            launcher_check(&parsed_path),
            Err(AppError::InvalidResponse)
        ));
        std::fs::remove_file(path).unwrap();
    }

    /// Managed host flags and the argument-free Claude hook are distinct from both legacy forms.
    #[test]
    fn managed_and_legacy_mcp_cli_forms_are_distinct() {
        assert!(matches!(
            command(args(&[
                "mcp",
                "--auto-launcher-template",
                "/private/tmp/template.json"
            ])),
            Ok(Command::AutoManagedMcp { launcher_template })
                if launcher_template == Path::new("/private/tmp/template.json")
        ));
        assert!(matches!(
            command(args(&["mcp", "--launcher-template", "/private/tmp/template.json"])),
            Ok(Command::ManagedMcp {
                launcher_template,
                host: ManagedHost::Codex,
            })
                if launcher_template == Path::new("/private/tmp/template.json")
        ));
        assert!(matches!(
            command(args(&[
                "mcp",
                "--claude-launcher-template",
                "/private/tmp/template.json"
            ])),
            Ok(Command::ManagedMcp {
                launcher_template,
                host: ManagedHost::Claude,
            }) if launcher_template == Path::new("/private/tmp/template.json")
        ));
        assert!(matches!(
            command(args(&["mcp", "--runtime-dir", "/private/tmp/runtime"])),
            Ok(Command::Mcp { runtime_dir }) if runtime_dir == Path::new("/private/tmp/runtime")
        ));
        assert!(matches!(
            command(args(&["claude-hook"])),
            Ok(Command::ManagedClaudeHook)
        ));
        assert!(matches!(
            command(args(&[
                "claude-hook",
                "--runtime-dir",
                "/private/tmp/runtime"
            ])),
            Ok(Command::ClaudeHook { runtime_dir })
                if runtime_dir == Path::new("/private/tmp/runtime")
        ));
    }

    /// Auto host selection uses only Claude project-variable presence and never cross-falls back.
    #[test]
    fn auto_managed_candidate_preserves_invalid_claude_selection() {
        let (codex, candidate) = auto_managed_candidate(None);
        assert_eq!(codex, ManagedHost::Codex);
        assert!(candidate.is_ok());

        let project = fs::canonicalize(std::env::temp_dir()).unwrap();
        let (claude, candidate) = auto_managed_candidate(Some(project.clone().into_os_string()));
        assert_eq!(claude, ManagedHost::Claude);
        assert_eq!(candidate.unwrap(), project);

        let (invalid_claude, candidate) = auto_managed_candidate(Some(OsString::from("relative")));
        assert_eq!(invalid_claude, ManagedHost::Claude);
        assert!(candidate.is_err());
    }

    /// Claude's shortened rendezvous is stable for one root and different for another root.
    #[test]
    fn claude_runtime_path_is_stable_and_candidate_specific() {
        let first = Path::new("/private/tmp/agent-ide-claude-project-a");
        let second = Path::new("/private/tmp/agent-ide-claude-project-b");
        let first_path = claude_runtime_path(first).unwrap();
        assert_eq!(first_path, claude_runtime_path(first).unwrap());
        assert_ne!(first_path, claude_runtime_path(second).unwrap());
        assert_eq!(
            first_path.parent(),
            Some(fs::canonicalize(Path::new("/tmp")).unwrap().as_path())
        );
        assert!(canonical_claude_project(Some(OsString::from("relative"))).is_err());
        assert!(canonical_claude_project(Some(OsString::from("/private/tmp/../tmp"))).is_err());
    }

    /// Exclusive Claude ownership preserves the first attachment and rejects corrupt rendezvous.
    #[test]
    fn claude_runtime_second_owner_cannot_overwrite_attachment() {
        let project = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "agent-ide-claude-owner-{}-{}",
                std::process::id(),
                random_hex(4).unwrap()
            ));
        fs::create_dir(&project).unwrap();
        let project = fs::canonicalize(&project).unwrap();
        let runtime_path = claude_runtime_path(&project).unwrap();
        let runtime = ManagedRuntime::create_deterministic(runtime_path.clone()).unwrap();
        let attachment = "a".repeat(64);
        runtime
            .write_claude_attachment(&project, &attachment)
            .unwrap();
        let original = fs::read(runtime_path.join(CLAUDE_ATTACHMENT_FILE)).unwrap();

        assert!(matches!(
            ManagedRuntime::create_deterministic(runtime_path.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists
        ));
        assert_eq!(
            fs::read(runtime_path.join(CLAUDE_ATTACHMENT_FILE)).unwrap(),
            original
        );
        assert_eq!(read_claude_attachment(&project).unwrap().1, attachment);

        fs::write(runtime_path.join(CLAUDE_ATTACHMENT_FILE), b"corrupt").unwrap();
        assert!(read_claude_attachment(&project).is_err());
        runtime.remove().unwrap();
        fs::remove_dir(project).unwrap();
    }
}
