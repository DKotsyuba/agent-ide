//! Command-line entrypoint for managed and legacy MCP, native hooks, the daemon, and diagnostics.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use agent_ide::app::{
    AppError, DoctorLockState, DoctorReport, DoctorStatus, RuntimeDir, config::EffectiveConfig,
    doctor_report, run_daemon_with_assistance,
};
use agent_ide::assistance::{
    assembly::ProductDispatcher,
    codex_rendezvous::ManagedCodexPublisher,
    facade::{ReestablishFn, SharedCodexPublisher, StdioFacade},
    host_binding::HostKind,
    launcher::{AcceptedExecutable, LauncherConfig},
};
use agent_ide::execution::{
    D03ProfileEvidence, HostSandboxState, PersistedProfileRecord, ProfileClass,
};
use agent_ide::{
    app::store::Store,
    telemetry::{Filter, Telemetry, TelemetryConfig},
};
use rmcp::{serve_server, transport::io::stdio};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// Selects an explicit mode; MCP writes only protocol messages to stdout and never autostarts.
#[tokio::main]
async fn main() -> ExitCode {
    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if is_version_request(&arguments) {
        println!("agent-ide {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    match usage_request(&arguments) {
        Some(UsageRequest::Help) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(UsageRequest::Unknown) => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
        None => {}
    }
    // Claude's workspace identity is captured before argument parsing or asynchronous setup and is
    // never accepted from an MCP call, hook payload, or later environment read.
    let claude_project_dir = std::env::var_os("CLAUDE_PROJECT_DIR");
    match command(arguments.into_iter()) {
        Ok(Command::Daemon { runtime_dir }) => match RuntimeDir::prepare_for_daemon(runtime_dir) {
            Ok(runtime_dir) => {
                let config = EffectiveConfig::defaults();
                let mut idle_timeout = agent_ide::app::lease::DEFAULT_IDLE_TIMEOUT;
                let dispatcher = match std::env::var("AGENT_IDE_LAUNCHER_CONFIG") {
                    Ok(path) => match LauncherConfig::read(std::path::Path::new(&path)) {
                        Ok(config) => {
                            if let Some(checks) = config.project_checks() {
                                idle_timeout = checks.idle_timeout();
                            }
                            if std::env::var("AGENT_IDE_MANAGED_CODEX_ATTACHMENT")
                                .ok()
                                .is_some_and(|attachment| config.target(&attachment).is_some())
                            {
                                ProductDispatcher::with_managed_codex_launcher(config)
                            } else if std::env::var("AGENT_IDE_MANAGED_CLAUDE_DAEMON").as_deref()
                                == Ok("1")
                            {
                                ProductDispatcher::with_managed_claude_launcher(config)
                            } else {
                                ProductDispatcher::with_launcher(config)
                            }
                        }
                        Err(_) => return fail(AppError::InvalidResponse),
                    },
                    Err(std::env::VarError::NotPresent) => ProductDispatcher::default(),
                    Err(_) => return fail(AppError::InvalidResponse),
                };
                match run_daemon_with_assistance(
                    runtime_dir,
                    Arc::new(dispatcher),
                    config,
                    idle_timeout,
                )
                .await
                {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => fail(error),
                }
            }
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
        Ok(Command::ManagedCodexHook) => {
            agent_ide::assistance::codex_hook::run_managed().await;
            ExitCode::SUCCESS
        }
        Ok(Command::CodexHooksPrint) => match codex_hooks_print() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(error),
        },
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
            run_managed_claude_hook().await;
            ExitCode::SUCCESS
        }
        Ok(Command::ClaudeRendezvous { project_dir }) => {
            match claude_rendezvous_paths(&project_dir).await {
                Ok((runtime_dir, helper_socket)) => {
                    println!("runtime_dir={}", runtime_dir.display());
                    println!("helper_socket={}", helper_socket.display());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("agent-ide: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Ok(Command::ClaudeWorker {
            runtime_dir,
            attachment,
            detail_ref,
        }) => {
            let claimed = agent_ide::assistance::claude_helper::run(
                &runtime_dir,
                Some(attachment),
                Some(detail_ref),
            )
            .await;
            if claimed {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
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
            shape_version,
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
            shape_version,
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
        Ok(Command::EvidenceRejected) => {
            evidence_rejected();
            ExitCode::SUCCESS
        }
        Ok(Command::TelemetryQuery {
            database,
            filter,
            cursor,
        }) => {
            let page = match telemetry_owner(&database).await {
                Ok(telemetry) => telemetry
                    .query(filter, cursor, 1_000)
                    .await
                    .map_err(|_| AppError::InvalidResponse),
                Err(error) => Err(error),
            };
            match page {
                Ok(page) => {
                    let rows = page
                        .rows
                        .iter()
                        .map(|row| {
                            serde_json::json!({
                                "sequence": row.sequence,
                                "event": row.event,
                            })
                        })
                        .collect::<Vec<_>>();
                    println!(
                        "{}",
                        serde_json::json!({
                            "rows": rows,
                            "next_cursor": page.next_cursor,
                            "truncated": page.truncated,
                            "dropped": page.dropped,
                        })
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            }
        }
        Ok(Command::TelemetryExport { database, filter }) => {
            let export = match telemetry_owner(&database).await {
                Ok(telemetry) => telemetry
                    .export(filter)
                    .await
                    .map_err(|_| AppError::InvalidResponse),
                Err(error) => Err(error),
            };
            match export {
                Ok(export) => {
                    if std::io::stdout().write_all(&export.bytes).is_err() {
                        return ExitCode::FAILURE;
                    }
                    eprintln!("truncated={}", export.truncated);
                    if let Some(sequence) = export.first_omitted_sequence {
                        eprintln!("first_omitted_sequence={sequence}");
                    }
                    match export.dropped {
                        Some(dropped) => eprintln!("dropped={dropped}"),
                        None => eprintln!("dropped=null"),
                    }
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            }
        }
        Ok(Command::Errors {
            repo,
            since_minutes,
            limit,
            summary,
            all,
        }) => match errors_command(repo, since_minutes, limit, summary, all).await {
            Ok(()) => ExitCode::SUCCESS,
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
/// reuses [`PersistedProfileRecord::from_execution_evidence_versioned`] and never starts a
/// daemon, spawns a provider, or writes any file. `sandbox_state` names a file holding the exact
/// captured `codex/sandbox-state-meta` envelope for the tested run; `evidence` carries the
/// non-state D03 identities. `shape_version` is `2` (the default: the conservative shape-based
/// record for supported managed captures) or `1` (the legacy exact-digest layout, requested
/// explicitly). The returned JSON text is the exact shape a `profiles` entry in a launcher
/// configuration expects. Returns
/// [`AppError::InvalidResponse`] for an unreadable/malformed sandbox-state file, a capture whose
/// state cannot support the requested generation (a `disabled` or unrecognized state never
/// silently becomes a v2 record), or evidence that the record constructor rejects (an empty
/// identity or zero revision).
fn evidence_record(
    sandbox_state: &std::path::Path,
    profile_id: &str,
    revision: u32,
    evidence: D03ProfileEvidence,
    shape_version: u32,
) -> Result<String, AppError> {
    let state = read_sandbox_state(sandbox_state)?;
    let record = PersistedProfileRecord::from_execution_evidence_versioned(
        profile_id,
        revision,
        evidence,
        &state,
        shape_version,
    )
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

/// Renders one state-derived string as a single bounded, control-free operator field (T25B).
///
/// Captured values come from the host, so control characters and non-printables are escaped
/// (`char::escape_default`) and the field is capped at [`MAX_RENDERED_FIELD_CHARS`] characters:
/// every listed entry stays exactly one line and cannot smuggle terminal escapes into operator
/// review.
fn escaped_field(value: &str) -> String {
    const MAX_RENDERED_FIELD_CHARS: usize = 256;
    value
        .chars()
        .take(MAX_RENDERED_FIELD_CHARS)
        .flat_map(char::escape_default)
        .collect()
}

/// Prints one line per captured rejected sandbox state below the real user home (T25B).
///
/// Each line is `name class sandbox-cwd mtime`, where `name` is the 16-hex capture stem an
/// operator joins with `~/.agent-ide/rejected-profiles/<name>.json` and `mtime` uses the error
/// log's RFC 3339 UTC form. The state-derived `sandbox-cwd` is rendered through
/// [`escaped_field`]. A missing or unreadable capture is skipped; the command still succeeds.
fn evidence_rejected() {
    for capture in agent_ide::execution::list_rejected_profiles() {
        let class = match capture.class {
            ProfileClass::Managed => "managed",
            ProfileClass::Disabled => "disabled",
        };
        let mtime = capture
            .modified
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_secs())
            .map(agent_ide::errorlog::format_rfc3339)
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{} {class} {} {mtime}",
            capture.name,
            escaped_field(&capture.sandbox_cwd)
        );
    }
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

/// Opens the bounded local telemetry owner used only by deterministic query and export commands.
///
/// `database` is an operator-provided existing local SQLite path; no direct SQLite handle escapes
/// this helper. Its read-only Application owner never creates ledgers, runs migrations, or changes
/// retention. Opening or reading failure reports the existing compact invalid-response class.
async fn telemetry_owner(database: &Path) -> Result<Telemetry, AppError> {
    if !database.is_file() {
        return Err(AppError::InvalidResponse);
    }
    let store = Arc::new(
        Store::open_read_only(database, EffectiveConfig::defaults().store())
            .map_err(|_| AppError::InvalidResponse)?,
    );
    Telemetry::open_read_only(store, TelemetryConfig::default())
        .await
        .map_err(|_| AppError::InvalidResponse)
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
    /// Submits one managed Codex hook discovered solely from private rendezvous records (T29B §3).
    ManagedCodexHook,
    /// Prints the managed Codex hooks.json fragment for the running executable (T29B §6).
    ///
    /// Pure offline reporting; stdout carries only the JSON and nothing is ever written anywhere.
    CodexHooksPrint,
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
        /// Record generation: `2` (default) mints the shape-based v2 layout for supported
        /// managed captures; `1` explicitly mints the legacy v1 exact-digest layout.
        shape_version: u32,
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
    /// Lists captured rejected sandbox states below the real home for operator review (T25B).
    ///
    /// Pure offline listing; creates no runtime state and never starts a daemon.
    EvidenceRejected,
    /// Prints one deterministic bounded telemetry page from an operator-selected local database.
    TelemetryQuery {
        /// Existing local SQLite database owned through Application's Store thread.
        database: PathBuf,
        /// Optional fixed event-tag restriction; no caller-supplied SQL or arbitrary tag is accepted.
        filter: Filter,
        /// Exclusive durable sequence returned as `next_cursor` by a prior query page.
        cursor: Option<u64>,
    },
    /// Writes deterministic bounded canonical telemetry rows from an operator-selected local database.
    TelemetryExport {
        /// Existing local SQLite database owned through Application's Store thread.
        database: PathBuf,
        /// Optional fixed event-tag restriction; no caller-supplied SQL or arbitrary tag is accepted.
        filter: Filter,
    },
    /// Prints one project's repository-wide Claude rendezvous runtime directory and helper socket.
    ///
    /// Pure shared-derivation reporting for operators; creates no runtime state and never starts a
    /// daemon. Any resolution failure, including a missing project directory, exits with code 2.
    ClaudeRendezvous {
        /// Existing project directory (relative paths are canonicalized) whose rendezvous is reported.
        project_dir: PathBuf,
    },
    /// Prints recent local error-log events for one repository, or their grouped summary (T107).
    ///
    /// Reads only `~/.agent-ide/logs/<repository-key>/events.jsonl[.1]`; works with no daemon running.
    Errors {
        /// Repository directory whose rendezvous key selects the log; defaults to the current directory.
        repo: Option<PathBuf>,
        /// Only events at most this many minutes old.
        since_minutes: Option<u64>,
        /// Bounds the number of printed or summarized events; defaults to 200.
        limit: usize,
        /// Prints grouped `(level, method, outcome, reason)` counts instead of individual lines.
        summary: bool,
        /// Includes `info`-level events (every completed call, lifecycle facts); default is
        /// `warn`/`error` only.
        all: bool,
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

/// Matches the exact one-argument version spellings, handled before any other mode.
fn is_version_request(arguments: &[OsString]) -> bool {
    matches!(
        arguments,
        [only] if matches!(only.to_str(), Some("-v" | "-V" | "--version" | "version"))
    )
}

/// Usage listing printed for `--help`/`-h`/`help` and for a missing or unknown subcommand.
const USAGE: &str = "\
usage: agent-ide <command> [args]

commands:
  mcp --runtime-dir <dir>                 MCP server over an existing daemon
  mcp --launcher-template <file>          managed Codex MCP server
  mcp --claude-launcher-template <file>   managed Claude MCP server
  mcp --auto-launcher-template <file>     managed MCP server, host auto-detected
  daemon --runtime-dir <dir>              run the repository daemon
  doctor --runtime-dir <dir>              report daemon health
  codex-hook --runtime-dir <dir>          Codex native hook
  codex-hook --managed                    Codex native hook, managed route discovery
  codex-hooks print                       print the managed Codex hooks.json fragment
  claude-hook [--runtime-dir <dir>]       Claude native hook
  claude-worker --runtime-dir <dir> --attachment <id> --detail-ref <ref>
                                          Claude foreground helper
  claude-rendezvous <project-dir>         print the Claude runtime and helper socket paths
  errors [--repo <path>] [--all] [--summary] [--since <minutes>] [--limit <n>]
                                          read the error log
  evidence record|executable ...          launcher evidence fragments
  evidence rejected                       list captured rejected sandbox states
  launcher check <file>                   validate a launcher configuration
  telemetry query|export --database <file> [--tag <tag>] [--cursor <n>]
  -v, --version, version                  print the version
  -h, --help, help                        print this listing
";

/// Subcommand names accepted as the first argument; everything else is an unknown subcommand.
const SUBCOMMANDS: &[&str] = &[
    "mcp",
    "daemon",
    "doctor",
    "codex-hook",
    "codex-hooks",
    "claude-hook",
    "claude-worker",
    "claude-rendezvous",
    "errors",
    "evidence",
    "launcher",
    "telemetry",
];

/// How a command line asks for the usage listing instead of a real command.
#[derive(Debug, Eq, PartialEq)]
enum UsageRequest {
    /// `--help`, `-h` or `help`: print the listing to stdout and succeed.
    Help,
    /// A missing or unknown subcommand: print the listing to stderr and exit 2.
    Unknown,
}

/// Classifies only the first argument; a known subcommand with bad arguments keeps its own error.
fn usage_request(arguments: &[OsString]) -> Option<UsageRequest> {
    match arguments.first().map(|first| first.to_str()) {
        Some(Some("--help" | "-h" | "help")) => Some(UsageRequest::Help),
        Some(Some(first)) if SUBCOMMANDS.contains(&first) => None,
        _ => Some(UsageRequest::Unknown),
    }
}

/// Rejects unknown, missing, and extra CLI arguments before any filesystem or daemon action.
fn command(arguments: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let arguments = arguments.collect::<Vec<_>>();
    if arguments.as_slice() == [OsString::from("claude-hook")] {
        return Ok(Command::ManagedClaudeHook);
    }
    // `claude-rendezvous` takes one bare project directory and only prints derived paths.
    if let [mode, project_dir] = arguments.as_slice()
        && mode == "claude-rendezvous"
    {
        return Ok(Command::ClaudeRendezvous {
            project_dir: PathBuf::from(project_dir),
        });
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
    // may be reordered, omitted, or repeated. One optional trailing `--shape-version <1|2>` flag
    // selects the record generation; the default is the v2 shape-based layout (T35B), and the
    // legacy v1 layout must be requested explicitly.
    if let [mode, sub, rest @ ..] = arguments.as_slice()
        && mode == "evidence"
        && sub == "record"
    {
        let (rest, shape_version) = match rest {
            [head @ .., flag, value] if flag == "--shape-version" => {
                let version = match value.to_str() {
                    Some("1") => 1,
                    Some("2") => 2,
                    _ => return Err(AppError::InvalidResponse),
                };
                (head, version)
            }
            _ => (rest, 2),
        };
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
            shape_version,
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
    // `evidence rejected` lists captured rejected sandbox states; it takes no flag at all.
    if let [mode, sub] = arguments.as_slice()
        && mode == "evidence"
        && sub == "rejected"
    {
        return Ok(Command::EvidenceRejected);
    }
    // `errors` accepts its flags in any order, unlike every other mode above.
    if let [mode, rest @ ..] = arguments.as_slice()
        && mode == "errors"
    {
        return parse_errors_command(rest);
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
    if let [mode, sub, database_flag, database] = arguments.as_slice()
        && mode == "telemetry"
        && matches!(sub.to_str(), Some("query" | "export"))
        && database_flag == "--database"
    {
        return telemetry_command(sub, PathBuf::from(database), Filter::All, None);
    }
    if let [mode, sub, database_flag, database, tag_flag, tag] = arguments.as_slice()
        && mode == "telemetry"
        && matches!(sub.to_str(), Some("query" | "export"))
        && database_flag == "--database"
        && tag_flag == "--tag"
    {
        let filter = telemetry_filter(tag.to_str().ok_or(AppError::InvalidResponse)?)?;
        return telemetry_command(sub, PathBuf::from(database), filter, None);
    }
    if let [mode, sub, database_flag, database, cursor_flag, cursor] = arguments.as_slice()
        && mode == "telemetry"
        && sub == "query"
        && database_flag == "--database"
        && cursor_flag == "--cursor"
    {
        let cursor = cursor
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(AppError::InvalidResponse)?;
        return telemetry_command(sub, PathBuf::from(database), Filter::All, Some(cursor));
    }
    if let [
        mode,
        sub,
        database_flag,
        database,
        tag_flag,
        tag,
        cursor_flag,
        cursor,
    ] = arguments.as_slice()
        && mode == "telemetry"
        && sub == "query"
        && database_flag == "--database"
        && tag_flag == "--tag"
        && cursor_flag == "--cursor"
    {
        let filter = telemetry_filter(tag.to_str().ok_or(AppError::InvalidResponse)?)?;
        let cursor = cursor
            .to_str()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(AppError::InvalidResponse)?;
        return telemetry_command(sub, PathBuf::from(database), filter, Some(cursor));
    }
    // Fixed two-argument managed Codex hook forms; every other mode keeps its existing shape.
    if let [mode, argument] = arguments.as_slice() {
        if mode == "codex-hook" && argument == "--managed" {
            return Ok(Command::ManagedCodexHook);
        }
        if mode == "codex-hooks" && argument == "print" {
            return Ok(Command::CodexHooksPrint);
        }
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

/// Builds a fixed telemetry CLI command from a trusted subcommand, database, closed filter, and cursor.
///
/// `cursor` is the exclusive durable sequence returned by a prior query and is rejected for export.
/// Unknown subcommands return [`AppError::InvalidResponse`] without filesystem access.
fn telemetry_command(
    subcommand: &OsString,
    database: PathBuf,
    filter: Filter,
    cursor: Option<u64>,
) -> Result<Command, AppError> {
    match subcommand.to_str() {
        Some("query") => Ok(Command::TelemetryQuery {
            database,
            filter,
            cursor,
        }),
        Some("export") if cursor.is_none() => Ok(Command::TelemetryExport { database, filter }),
        _ => Err(AppError::InvalidResponse),
    }
}

/// Reads and prints `~/.agent-ide/logs/<repository-key>/events.jsonl[.1]` for one repository.
///
/// Works without a running daemon: it only reads files, using the exact same repository-key
/// derivation the daemon used to name its log directory. `repo` defaults to the current directory.
async fn errors_command(
    repo: Option<PathBuf>,
    since_minutes: Option<u64>,
    limit: usize,
    summary: bool,
    all: bool,
) -> Result<(), AppError> {
    let repo = match repo {
        Some(repo) => repo,
        None => std::env::current_dir().map_err(|_| AppError::InvalidResponse)?,
    };
    let candidate = fs::canonicalize(&repo).map_err(|_| AppError::InvalidResponse)?;
    let root = agent_ide::errorlog::log_root().ok_or(AppError::InvalidResponse)?;
    let dir = error_log_dir(&root, &candidate).await;
    let mut events = agent_ide::errorlog::read_events(&dir);
    if let Some(since_minutes) = since_minutes {
        let cutoff = rfc3339_cutoff(since_minutes);
        events.retain(|event| event.timestamp >= cutoff);
    }
    if !all {
        agent_ide::errorlog::retain_warn_and_error(&mut events);
    }
    if summary {
        for (level, method, outcome, reason, count) in agent_ide::errorlog::summarize(&events) {
            println!("{count} {level} {method} {outcome} {reason}");
        }
        return Ok(());
    }
    let start = events.len().saturating_sub(limit);
    for event in &events[start..] {
        println!("{}", agent_ide::errorlog::format_line(event));
    }
    Ok(())
}

/// Finds the log directory the daemon of `candidate`'s repository writes, below `root`.
///
/// The first key whose directory exists wins, in the order the writer could have derived it: the
/// repository's git common directory as `git` reports it, the same directory found by reading
/// `.git` files (when the `git` probe failed or timed out here), then `candidate` itself (the
/// writer's own fallback outside a repository). With none present, the first key is returned so
/// the reader simply finds nothing.
async fn error_log_dir(root: &Path, candidate: &Path) -> PathBuf {
    let mut keys = vec![claude_rendezvous_key(candidate).await];
    keys.extend(common_dir_from_git_files(candidate));
    keys.push(candidate.to_owned());
    let dirs = keys
        .iter()
        .map(|key| root.join(&claude_rendezvous_identity(key)[..16]))
        .collect::<Vec<_>>();
    dirs.iter()
        .find(|dir| dir.is_dir())
        .unwrap_or(&dirs[0])
        .clone()
}

/// Resolves a repository's canonical git common directory by reading `.git` files, without `git`.
///
/// Walks up from `start` to the first `.git`. A directory is the common directory itself; a
/// worktree's `.git` file names its `gitdir:`, whose `commondir` file points at the common one.
fn common_dir_from_git_files(start: &Path) -> Option<PathBuf> {
    let dot_git = start
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|path| path.exists())?;
    if dot_git.is_dir() {
        return fs::canonicalize(dot_git).ok();
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let gitdir = dot_git
        .parent()?
        .join(text.trim().strip_prefix("gitdir:")?.trim());
    let common = fs::read_to_string(gitdir.join("commondir")).ok()?;
    fs::canonicalize(gitdir.join(common.trim())).ok()
}

/// Renders `now - since_minutes` as the same RFC 3339 UTC form error-log timestamps use, so a
/// plain string comparison against recorded events is exact without parsing them back.
fn rfc3339_cutoff(since_minutes: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now.saturating_sub(since_minutes.saturating_mul(60));
    agent_ide::errorlog::format_rfc3339(cutoff)
}

/// Parses `errors`' optional, any-order `--repo`/`--since`/`--limit`/`--summary`/`--all` flags.
fn parse_errors_command(rest: &[OsString]) -> Result<Command, AppError> {
    let mut repo = None;
    let mut since_minutes = None;
    let mut limit = 200usize;
    let mut summary = false;
    let mut all = false;
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].to_str().ok_or(AppError::InvalidResponse)?;
        match flag {
            "--summary" if !summary => {
                summary = true;
                index += 1;
            }
            "--all" if !all => {
                all = true;
                index += 1;
            }
            "--repo" | "--since" | "--limit" => {
                let value = rest.get(index + 1).ok_or(AppError::InvalidResponse)?;
                let value = value.to_str().ok_or(AppError::InvalidResponse)?;
                match flag {
                    "--repo" if repo.is_none() => repo = Some(PathBuf::from(value)),
                    "--since" if since_minutes.is_none() => {
                        since_minutes = Some(
                            value
                                .parse::<u64>()
                                .map_err(|_| AppError::InvalidResponse)?,
                        );
                    }
                    "--limit" => {
                        limit = value
                            .parse::<usize>()
                            .map_err(|_| AppError::InvalidResponse)?;
                    }
                    _ => return Err(AppError::InvalidResponse),
                }
                index += 2;
            }
            _ => return Err(AppError::InvalidResponse),
        }
    }
    Ok(Command::Errors {
        repo,
        since_minutes,
        limit,
        summary,
        all,
    })
}

/// Converts only canonical closed tags into query filters, rejecting arbitrary local SQLite selectors.
fn telemetry_filter(tag: &str) -> Result<Filter, AppError> {
    match tag {
        "tool_completed"
        | "execution_completed"
        | "provider_observed"
        | "native_fallback"
        | "project_check_completed" => Ok(Filter::Tag(match tag {
            "tool_completed" => "tool_completed",
            "execution_completed" => "execution_completed",
            "provider_observed" => "provider_observed",
            "native_fallback" => "native_fallback",
            "project_check_completed" => "project_check_completed",
            _ => unreachable!("closed tag match is exhaustive"),
        })),
        _ => Err(AppError::InvalidResponse),
    }
}

/// Owns the identity of one managed runtime tree this process created, adopted, or is ensuring.
///
/// The recorded device and inode fence cleanup against pathname replacement. For Codex, the
/// directory remains private to this one MCP process and is removed only after its exact daemon
/// child is reaped. For Claude, the directory is shared by every worktree of one repository and is
/// never removed by an MCP process; only its own generation-specific launcher/attachment files may
/// be cleared, and only before a confirmed-dead generation is replaced.
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

    /// Exclusively creates the one deterministic shared Claude runtime directory with mode `0700`.
    ///
    /// `path` must be the rendezvous-key-derived child of the canonical `/private/tmp` root. An
    /// existing path is never opened, repaired, or removed here, so a fresh generation can never
    /// silently reuse or overwrite a live daemon's directory identity; [`Self::ensure_deterministic`]
    /// is the idempotent entry point a second worktree's MCP uses to join an existing rendezvous. The
    /// captured device/inode identity fences the eventual recursive cleanup exactly as in managed
    /// Codex mode.
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

    /// Writes one rendezvous-key-bound random Claude attachment record exactly once with mode `0600`.
    ///
    /// The first field is the full rendezvous-key identity whose prefix selected this short runtime
    /// path; the second is the unguessable transport attachment. A hook validates both fields before
    /// it attempts IPC. Existing files are never followed or overwritten, and no value is rendered.
    fn write_claude_attachment(&self, key: &Path, attachment: &str) -> std::io::Result<()> {
        let record = format!("{} {attachment}\n", claude_rendezvous_identity(key));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(CLAUDE_ATTACHMENT_FILE))?;
        file.write_all(record.as_bytes())?;
        file.sync_all()
    }

    /// Idempotently ensures the one shared deterministic rendezvous directory at `path` exists.
    ///
    /// A fresh directory is created exclusively, exactly as [`Self::create_deterministic`]. When the
    /// directory already exists (another worktree's MCP created it first, or a prior generation left
    /// it behind), it is instead validated in place: it must be the expected nonsymlink, owner-only,
    /// mode `0700` real directory, and its current device/inode identity is captured for this
    /// process's own fencing. Any other existing-path state is rejected without repair.
    fn ensure_deterministic(path: PathBuf) -> std::io::Result<Self> {
        match Self::create_deterministic(path.clone()) {
            Ok(runtime) => Ok(runtime),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Self::open_existing(path)
            }
            Err(error) => Err(error),
        }
    }

    /// Explicitly validates and fences an already-existing shared runtime directory before this
    /// process ever trusts it for adoption.
    ///
    /// Per EYES-r2, adopting a directory this process did not itself just create requires its own
    /// explicit check, not a reuse of the freshly-created path's implicit trust: `path` must be a
    /// real, non-symlink, owner-only (mode `0700`) directory owned by the effective user. It is
    /// opened directly with `O_NOFOLLOW`/`O_DIRECTORY`, refusing a symlink at the final component,
    /// and its identity is captured from the *open file descriptor* (immune to a path swap between
    /// stat calls), then compared against an independent path-based stat taken first: any mismatch
    /// between the two means the path was replaced concurrently and is rejected rather than trusted.
    fn open_existing(path: PathBuf) -> std::io::Result<Self> {
        let initial = fs::symlink_metadata(&path)?;
        if initial.file_type().is_symlink() || !initial.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "shared managed Claude runtime is not a private directory",
            ));
        }
        let handle = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(&path)?;
        let opened = handle.metadata()?;
        if opened.file_type().is_symlink()
            || !opened.is_dir()
            || opened.uid() != unsafe { libc::geteuid() }
            || opened.permissions().mode() & 0o777 != 0o700
            || opened.dev() != initial.dev()
            || opened.ino() != initial.ino()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "shared managed Claude runtime identity is not stable and private",
            ));
        }
        Ok(Self {
            path,
            device: opened.dev(),
            inode: opened.ino(),
        })
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

/// Fixed short namespace for deterministic shared per-repository Claude runtimes below `/private/tmp`.
const CLAUDE_RUNTIME_PREFIX: &str = "ai-r-";
/// Bounded deadline for the local `git` rendezvous-key probe; a real repository answers instantly.
const GIT_COMMON_DIR_TIMEOUT: Duration = Duration::from_secs(2);
/// Fixed owner-only file carrying the full project identity and random transport attachment.
const CLAUDE_ATTACHMENT_FILE: &str = "attachment";
/// Per-candidate attachment cache written only after the daemon registers that candidate.
const CLAUDE_CANDIDATE_ATTACHMENT_FILE: &str = "candidate-attachment";
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

/// Returns the full BLAKE3 digest of one canonical rendezvous key's raw path bytes.
fn claude_rendezvous_identity(key: &Path) -> String {
    blake3::hash(key.as_os_str().as_bytes())
        .to_hex()
        .to_string()
}

/// Resolves the repository-wide rendezvous key shared by every worktree of one Claude candidate.
///
/// Shells out to the fixed `/usr/bin/git -C <candidate> rev-parse --path-format=absolute
/// --git-common-dir` under [`GIT_COMMON_DIR_TIMEOUT`] so distinct worktrees of one repository
/// resolve to the same canonical git common directory. A spawn failure, timeout, nonzero exit,
/// non-absolute output, or a value that fails to canonicalize is treated as "not a git repository"
/// and falls back to `candidate` itself; managed startup is never blocked or failed by this probe.
async fn claude_rendezvous_key(candidate: &Path) -> PathBuf {
    match git_common_dir(candidate).await {
        Some(common_dir) => fs::canonicalize(&common_dir).unwrap_or_else(|_| candidate.to_owned()),
        None => candidate.to_owned(),
    }
}

/// Runs one bounded `git rev-parse --git-common-dir` probe and returns its absolute output path.
///
/// Never removes, creates, or writes anything; a killed, failed, or malformed probe returns `None`.
async fn git_common_dir(candidate: &Path) -> Option<PathBuf> {
    let mut command = tokio::process::Command::new("/usr/bin/git");
    command
        .arg("-C")
        .arg(candidate)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let child = command.spawn().ok()?;
    let output = tokio::time::timeout(GIT_COMMON_DIR_TIMEOUT, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&output.stdout).ok()?;
    let path = PathBuf::from(text.trim_end_matches('\n'));
    absolute_local_path(&path).then_some(path)
}

/// Fixed short namespace for one candidate's non-authoritative cached rendezvous key.
const CLAUDE_KEY_CACHE_PREFIX: &str = "ai-k-";
/// Fixed owner-only file name holding one candidate's cached rendezvous key bytes.
const CLAUDE_KEY_CACHE_FILE: &str = "key";

/// Derives the deterministic per-candidate path of [`claude_rendezvous_key`]'s hot-path cache.
///
/// Keyed by the raw candidate path itself (never the resolved key), so it never requires `git` to
/// locate: every worktree has its own distinct, purely local cache slot.
fn claude_key_cache_path(candidate: &Path) -> std::io::Result<PathBuf> {
    let identity = blake3::hash(candidate.as_os_str().as_bytes())
        .to_hex()
        .to_string();
    Ok(fs::canonicalize(Path::new("/private/tmp"))?
        .join(format!("{CLAUDE_KEY_CACHE_PREFIX}{}", &identity[..16])))
}

/// Best-effort caches `key` for `candidate`'s hot path; per EYES-r2, only the MCP server calls this.
///
/// Per EYES-r2 §3, the hook must never spawn `git` on its bounded deadline, so the MCP server (which
/// can afford one bounded `git` probe at its own startup) leaves this hint for it. This is purely a
/// hint: it carries no secret and grants no authority by itself, since [`read_claude_attachment`]
/// independently re-validates any key read back from here against the runtime directory's own
/// identity before anything is trusted. Replaces stale hints atomically so a newly started daemon
/// and its first hook use the same key. Failure is silent and never blocks managed startup.
fn write_claude_key_cache(candidate: &Path, key: &Path) {
    let Ok(cache) = claude_key_cache_path(candidate) else {
        return;
    };
    if prepare_private_persistent_directory(&cache).is_err() {
        return;
    }
    let Ok(nonce) = random_hex(8) else {
        return;
    };
    let temporary = cache.join(format!("key-{nonce}"));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .and_then(|mut file| file.write_all(key.as_os_str().as_bytes()));
    if written.is_ok() {
        let _ = fs::rename(&temporary, cache.join(CLAUDE_KEY_CACHE_FILE));
    }
    let _ = fs::remove_file(temporary);
}

/// Caches the daemon-minted candidate attachment in the existing private key-cache directory.
fn write_claude_candidate_attachment(candidate: &Path, attachment: &str) {
    if !valid_random_attachment(attachment) {
        return;
    }
    if read_claude_key_cache(candidate).is_none() {
        return;
    }
    let Ok(cache) = claude_key_cache_path(candidate) else {
        return;
    };
    let path = cache.join(CLAUDE_CANDIDATE_ATTACHMENT_FILE);
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(path);
    if let Ok(mut file) = file
        && file.metadata().is_ok_and(|meta| {
            meta.is_file()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.permissions().mode() & 0o777 == 0o600
        })
    {
        let _ = file.set_len(0);
        let _ = file.write_all(attachment.as_bytes());
    }
}

/// Reads only a private bounded daemon-minted candidate attachment, when one is cached.
fn read_claude_candidate_attachment(candidate: &Path) -> Option<String> {
    let cache = claude_key_cache_path(candidate).ok()?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(cache.join(CLAUDE_CANDIDATE_ATTACHMENT_FILE))
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.permissions().mode() & 0o777 != 0o600
        || meta.len() != 64
    {
        return None;
    }
    let mut attachment = String::new();
    file.take(65).read_to_string(&mut attachment).ok()?;
    valid_random_attachment(&attachment).then_some(attachment)
}

/// Reads one candidate's cached rendezvous key without ever invoking `git`.
///
/// The cache directory and file must be the expected private (`0700`/`0600`), non-symlink,
/// owner-only state; anything else, including a missing cache, returns `None` rather than
/// repairing or removing it. This is only ever a hint for [`run_managed_claude_hook`]: the returned
/// key still passes through the exact same strict [`read_claude_attachment`] validation as one
/// resolved live, so a stale or hostile cache only ever fails safely.
fn read_claude_key_cache(candidate: &Path) -> Option<PathBuf> {
    let cache = claude_key_cache_path(candidate).ok()?;
    let directory_metadata = fs::symlink_metadata(&cache).ok()?;
    if directory_metadata.file_type().is_symlink()
        || !directory_metadata.is_dir()
        || directory_metadata.uid() != unsafe { libc::geteuid() }
        || directory_metadata.permissions().mode() & 0o777 != 0o700
    {
        return None;
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(cache.join(CLAUDE_KEY_CACHE_FILE))
        .ok()?;
    let file_metadata = file.metadata().ok()?;
    if !file_metadata.is_file()
        || file_metadata.uid() != unsafe { libc::geteuid() }
        || file_metadata.permissions().mode() & 0o777 != 0o600
    {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(4096).read_to_end(&mut bytes).ok()?;
    let key = PathBuf::from(std::ffi::OsStr::from_bytes(&bytes));
    absolute_local_path(&key).then_some(key)
}

/// Creates or validates one owner-only persistent directory without following a final symlink.
///
/// Missing paths are created with mode `0700`; an existing non-directory, symlink, foreign owner,
/// nonprivate mode, or I/O failure is returned without repairing or removing the path.
fn prepare_private_persistent_directory(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_dir()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o777 == 0o700 =>
        {
            Ok(())
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "telemetry state directory is not private",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700).create(path)?;
            prepare_private_persistent_directory(path)
        }
        Err(error) => Err(error),
    }
}

/// Derives one candidate database below an explicitly supplied private Application state root.
///
/// The application, telemetry, and digest directories are created or validated as `0700`. The
/// returned database path is not opened here; unsafe or unavailable directory state returns I/O.
fn managed_telemetry_database_in(
    application_state: &Path,
    candidate: &Path,
) -> std::io::Result<PathBuf> {
    prepare_private_persistent_directory(application_state)?;
    let telemetry = application_state.join("telemetry");
    prepare_private_persistent_directory(&telemetry)?;
    let candidate_state = telemetry.join(
        blake3::hash(candidate.as_os_str().as_bytes())
            .to_hex()
            .as_str(),
    );
    prepare_private_persistent_directory(&candidate_state)?;
    Ok(candidate_state.join("state.sqlite"))
}

/// Derives the persistent telemetry-only Store path for one canonical managed worktree.
///
/// State lives below the effective user's real home in private `0700` directories. The candidate
/// component is an opaque digest of the already-validated path, so fresh runtime generations reuse
/// prior events while runtime cleanup cannot delete them. Unsafe or symlinked state is rejected,
/// and Workspace/Changes authority remains in each daemon's private runtime.
fn managed_telemetry_database(candidate: &Path) -> std::io::Result<PathBuf> {
    let home = agent_ide::userhome::user_home()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "home is unavailable"))?;
    if !absolute_local_path(&home) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "home is not absolute and normalized",
        ));
    }
    // A relocated (`AGENT_IDE_HOME`) home may not exist yet; a real one always does.
    let _ = fs::create_dir_all(&home);
    let home = fs::canonicalize(home)?;
    let metadata = fs::symlink_metadata(&home)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "HOME is not owned by this user",
        ));
    }
    managed_telemetry_database_in(&home.join(".agent-ide"), candidate)
}

/// Derives the one short deterministic private runtime path shared by every worktree of one key.
///
/// `key` is the rendezvous key resolved by [`claude_rendezvous_key`] (a repository's canonical git
/// common directory, or the canonical candidate itself outside a git repository). The full digest
/// remains in the attachment record to reject a theoretical collision in the sixteen-hex-character
/// pathname prefix. The returned parent is canonical `/private/tmp`; no caller or model path can
/// redirect the rendezvous elsewhere.
fn claude_runtime_path(key: &Path) -> std::io::Result<PathBuf> {
    let identity = claude_rendezvous_identity(key);
    Ok(fs::canonicalize(Path::new("/private/tmp"))?
        .join(format!("{CLAUDE_RUNTIME_PREFIX}{}", &identity[..16])))
}

/// Resolves the two operator-reported Claude rendezvous paths for one existing project directory.
///
/// The derivation is exactly the shared one used by the managed Claude MCP server and hook: the
/// repository-wide rendezvous key of [`claude_rendezvous_key`] (EYES-r2 §2) fed through
/// [`claude_runtime_path`], with the helper socket name appended, so the reported paths match the
/// allowlist entry a Claude session in the same repository resolves. `project_dir` may be
/// relative and is canonicalized first; nothing is created, read, or removed under the reported
/// paths. Returns an error when the directory is missing or not a directory, or when
/// `/private/tmp` cannot be resolved; the caller reports any error with exit code 2.
async fn claude_rendezvous_paths(project_dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    if !fs::symlink_metadata(project_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("project directory {} does not exist", project_dir.display()),
        ));
    }
    let candidate = fs::canonicalize(project_dir)?;
    let key = claude_rendezvous_key(&candidate).await;
    let runtime = claude_runtime_path(&key)?;
    let socket = runtime.join(agent_ide::assistance::claude_helper::HELPER_SOCKET);
    Ok((runtime, socket))
}

/// Returns whether a string is exactly one generated 32-byte lowercase hexadecimal attachment.
fn valid_random_attachment(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

/// POSIX-shell single-quotes one absolute executable path, surviving spaces and apostrophes.
///
/// Every embedded apostrophe is closed, escaped as `\'`, and reopened, so the result is exactly
/// one shell word that expands to the original path.
fn shell_single_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// Builds the exact managed Codex `hooks.json` fragment from T29B §6 for one executable path.
///
/// Both handlers use one catch-all matcher, one second timeout, and no async execution, so the
/// pre-observation arrives before the post and the post feedback stays immediate.
fn managed_hooks_fragment(executable: &Path) -> serde_json::Value {
    let command = format!("{} codex-hook --managed", shell_single_quote(executable));
    let handler = serde_json::json!({
        "matcher": ".*",
        "hooks": [{"type": "command", "command": command, "timeout": 1}]
    });
    serde_json::json!({
        "hooks": {
            "PreToolUse": [handler],
            "PostToolUse": [handler],
        }
    })
}

/// Prints only the managed Codex `hooks.json` fragment for this running executable (T29B §6).
///
/// The path is the installed executable's absolute, canonical path, so a symlinked command name
/// still resolves to the real binary. Stdout carries JSON only; nothing is ever written anywhere.
fn codex_hooks_print() -> Result<(), AppError> {
    let executable = std::env::current_exe().map_err(|_| AppError::InvalidResponse)?;
    let executable = fs::canonicalize(&executable).unwrap_or(executable);
    println!(
        "{}",
        serde_json::to_string(&managed_hooks_fragment(&executable))
            .map_err(|_| AppError::InvalidResponse)?
    );
    Ok(())
}

/// Reads one key-derived managed Claude rendezvous after strict owner/mode/identity checks.
///
/// The directory must be the expected nonsymlink owned by the effective user with exact mode
/// `0700`. Its fixed attachment file is opened with `O_NOFOLLOW`, must be a regular owner-only
/// `0600` file of the exact bounded size, and must carry this rendezvous key's full digest plus one
/// valid random attachment. Any missing, stale, replaced, or corrupt state is rejected without repair.
fn read_claude_attachment(key: &Path) -> std::io::Result<(PathBuf, String)> {
    let runtime = claude_runtime_path(key)?;
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
    if identity != claude_rendezvous_identity(key) || !valid_random_attachment(attachment) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "managed Claude attachment does not match the rendezvous key",
        ));
    }
    Ok((runtime, attachment.to_owned()))
}

/// Submits one argument-free managed Claude hook on the payload cwd's lease channel.
///
/// The hook's inherited project environment can name another worktree of the same repository.
/// Missing or malformed cwd, cached key, runtime, candidate attachment, or daemon returns silently
/// to Claude while recording a closed, path-free reason in the repository error log.
/// Per EYES-r2 §3, this never spawns `git` itself and so never risks the existing bounded 250 ms
/// total deadline on that account: the rendezvous key is only ever read from
/// [`read_claude_key_cache`], a hint the owning MCP server left behind at its own startup. Once
/// validated, the existing bounded Claude parser, sanitized transport, exact lifecycle correlation,
/// feedback rendering, and foreground-helper recognition remain unchanged.
async fn run_managed_claude_hook() {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    let log_candidate = std::env::var_os("CLAUDE_PROJECT_DIR")
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok())
        .or_else(|| std::env::current_dir().ok());
    if let Some(candidate) = log_candidate.as_deref() {
        let key = common_dir_from_git_files(candidate)
            .or_else(|| read_claude_key_cache(candidate))
            .unwrap_or_else(|| candidate.to_owned());
        agent_ide::errorlog::init_repository(&claude_rendezvous_identity(&key)[..16]);
    }
    let log = |detail| {
        agent_ide::errorlog::record(
            agent_ide::errorlog::Method::Hook,
            agent_ide::errorlog::Outcome::Unavailable,
            agent_ide::errorlog::Fields {
                host: Some(HostKind::Claude),
                detail: Some(detail),
                ..Default::default()
            },
        );
    };
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if std::thread::Builder::new()
        .name("claude-hook-input".into())
        .spawn(move || {
            let mut payload = Vec::new();
            let result = std::io::stdin()
                .take(64 * 1024 + 1)
                .read_to_end(&mut payload);
            let _ = sender.send(result.ok().map(|_| payload));
        })
        .is_err()
    {
        log("hook_no_cwd");
        return;
    }
    let Ok(Ok(Some(payload))) = tokio::time::timeout_at(deadline, receiver).await else {
        log("hook_no_cwd");
        return;
    };
    if payload.len() > 64 * 1024 {
        log("hook_no_cwd");
        return;
    }
    let Some(cwd) = serde_json::from_slice::<serde_json::Value>(&payload)
        .ok()
        .and_then(|value| value.get("cwd")?.as_str().map(PathBuf::from))
        .filter(|path| absolute_local_path(path))
        .and_then(|path| fs::canonicalize(path).ok())
    else {
        log("hook_no_cwd");
        return;
    };
    let Some(project) = cwd
        .ancestors()
        .find(|path| read_claude_key_cache(path).is_some())
    else {
        log("hook_no_key_cache");
        return;
    };
    let Some(key) = read_claude_key_cache(project) else {
        log("hook_no_key_cache");
        return;
    };
    let Ok((runtime, _)) = read_claude_attachment(&key) else {
        log("hook_no_rendezvous");
        return;
    };
    let Some(attachment) = read_claude_candidate_attachment(project) else {
        log("hook_no_candidate_attachment");
        return;
    };
    if !agent_ide::assistance::codex_hook::run_with_payload(
        &runtime,
        Some(attachment),
        HostKind::Claude,
        Some(payload),
        deadline,
    )
    .await
    {
        log("hook_submit_refused:unavailable");
    }
}

/// Dispatches one self-contained managed MCP generation to its host-specific lifecycle contract.
///
/// Setup failure always still serves the static six tools through a connect-only unavailable
/// facade, for either host.
async fn run_managed_mcp(
    launcher_template: PathBuf,
    candidate: std::io::Result<PathBuf>,
    host: ManagedHost,
) -> ExitCode {
    match host {
        ManagedHost::Codex => run_managed_codex_mcp(launcher_template, candidate).await,
        ManagedHost::Claude => run_managed_claude_mcp(launcher_template, candidate).await,
    }
}

/// Starts, health-checks, serves, and tears down a managed Codex MCP's owned daemon generations.
///
/// Codex owns one daemon and private runtime at a time. It holds a lease while serving and replaces
/// a failed daemon on the next call; EOF or a termination signal tears down the current generation.
async fn run_managed_codex_mcp(
    launcher_template: PathBuf,
    candidate: std::io::Result<PathBuf>,
) -> ExitCode {
    let Ok(candidate) = candidate else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    };
    let Ok(runtime) = ManagedRuntime::create() else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    };
    agent_ide::errorlog::init_repository(
        &claude_rendezvous_identity(&claude_rendezvous_key(&candidate).await)[..16],
    );
    let runtime_path = runtime.path.clone();
    let started = start_managed_daemon(
        &runtime,
        &launcher_template,
        &candidate,
        &candidate,
        ManagedHost::Codex,
    )
    .await;
    match started {
        Ok((attachment, child)) => {
            let child = Arc::new(Mutex::new(child));
            let publisher = managed_codex_publisher(&runtime.path, &attachment);
            let lease = agent_ide::app::open_client_lease(&runtime_path, "managed-codex-mcp").await;
            let Some(lease) = lease else {
                terminate_owned_daemon(child).await;
                let _ = runtime.remove();
                return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None)
                    .await;
            };
            let lease = Arc::new(Mutex::new(Some(lease)));
            let runtime = Arc::new(Mutex::new(Some(runtime)));
            let generation = Arc::new(AtomicU64::new(0));
            let reconnect = codex_reestablish_hook(
                launcher_template,
                candidate,
                Arc::clone(&child),
                Arc::clone(&runtime),
                Arc::clone(&lease),
                publisher.clone(),
                Arc::clone(&generation),
            );
            let facade = match &publisher {
                Some(publisher) => StdioFacade::with_reestablishing_managed_codex(
                    runtime_path,
                    attachment,
                    Arc::clone(publisher),
                    reconnect,
                ),
                None => {
                    StdioFacade::with_reestablishing_attachment(runtime_path, attachment, reconnect)
                }
            };
            let Some(facade) = facade else {
                terminate_owned_daemon(child).await;
                if let Some(runtime) = runtime.lock().await.take() {
                    let _ = runtime.remove();
                }
                return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None)
                    .await;
            };
            if let Some(publisher) = &publisher {
                tokio::spawn(unpublish_when_daemon_exits(
                    Arc::clone(&child),
                    Arc::clone(publisher),
                    generation,
                    0,
                ));
            }
            serve_managed_stdio(facade, Some(child), Some(runtime), Some(lease), publisher).await
        }
        Err(_) => {
            let _ = runtime.remove();
            serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await
        }
    }
}

/// Restarts this MCP's owned Codex daemon once after transport loss, restoring its lease and hooks.
///
/// The guard serializes concurrent calls; a healthy replacement is reused. The old generation is
/// reaped before its fenced runtime is removed, and the publisher is rebound only after a fresh
/// daemon and lease both exist. Failure leaves later calls free to try again.
fn codex_reestablish_hook(
    launcher_template: PathBuf,
    candidate: PathBuf,
    child: SharedChild,
    runtime: Arc<Mutex<Option<ManagedRuntime>>>,
    lease: Arc<Mutex<Option<UnixStream>>>,
    publisher: Option<SharedCodexPublisher>,
    generation: Arc<AtomicU64>,
) -> ReestablishFn {
    let restart = Arc::new(Mutex::new(()));
    let current = Arc::new(Mutex::new(None::<(PathBuf, String)>));
    Arc::new(move || {
        let launcher_template = launcher_template.clone();
        let candidate = candidate.clone();
        let child = Arc::clone(&child);
        let runtime = Arc::clone(&runtime);
        let lease = Arc::clone(&lease);
        let publisher = publisher.clone();
        let generation = Arc::clone(&generation);
        let restart = Arc::clone(&restart);
        let current = Arc::clone(&current);
        Box::pin(async move {
            let _guard = restart.lock().await;
            if let Some(connection) = current.lock().await.clone()
                && doctor_report(&connection.0)
                    .await
                    .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. }))
            {
                return Some(connection);
            }
            terminate_owned_daemon(Arc::clone(&child)).await;
            *lease.lock().await = None;
            if let Some(old) = runtime.lock().await.take() {
                let _ = old.remove();
            }
            let fresh = ManagedRuntime::create().ok()?;
            let path = fresh.path.clone();
            let started = start_managed_daemon(
                &fresh,
                &launcher_template,
                &candidate,
                &candidate,
                ManagedHost::Codex,
            )
            .await;
            let Ok((attachment, new_child)) = started else {
                let _ = fresh.remove();
                return None;
            };
            let Some(connection) =
                agent_ide::app::open_client_lease(&path, "managed-codex-mcp").await
            else {
                terminate_owned_daemon(Arc::new(Mutex::new(new_child))).await;
                let _ = fresh.remove();
                return None;
            };
            *child.lock().await = new_child;
            *runtime.lock().await = Some(fresh);
            *lease.lock().await = Some(connection);
            if let Some(publisher) = publisher {
                let mut publisher_lock = publisher.lock().expect("managed codex publisher mutex");
                let next = generation.fetch_add(1, Ordering::AcqRel) + 1;
                publisher_lock.rebind(path.clone(), attachment.clone());
                drop(publisher_lock);
                tokio::spawn(unpublish_when_daemon_exits(
                    Arc::clone(&child),
                    publisher,
                    Arc::clone(&generation),
                    next,
                ));
            }
            let connection = (path, attachment);
            *current.lock().await = Some(connection.clone());
            Some(connection)
        })
    })
}

/// Creates this MCP process's managed Codex rendezvous publisher, if a rendezvous root resolves.
///
/// Nothing is published here: the actor route identity is only known from the first valid call's
/// trusted metadata (T29B §2). The root is the fixed effective-UID rendezvous root, redirected only
/// by the product-test seam `AGENT_IDE_CODEX_RENDEZVOUS_ROOT`; an unresolvable root disables
/// publication while every MCP reply keeps working exactly as before.
fn managed_codex_publisher(runtime_dir: &Path, attachment: &str) -> Option<SharedCodexPublisher> {
    let root = agent_ide::assistance::codex_rendezvous::effective_root()?;
    Some(Arc::new(std::sync::Mutex::new(ManagedCodexPublisher::new(
        root,
        runtime_dir.to_owned(),
        attachment,
    ))))
}

/// Finds or starts the one daemon shared by every worktree of `candidate`'s repository, and serves.
///
/// Per EYES-r1 §2, the runtime directory is keyed by the repository (its canonical git common
/// directory, or `candidate` itself outside a git repository), not by this one MCP process. This
/// generation never owns the resulting daemon's lifetime: on stdio EOF, MCP cancellation, SIGINT,
/// or SIGTERM it exits without terminating or removing an adopted or spawned daemon.
async fn run_managed_claude_mcp(
    launcher_template: PathBuf,
    candidate: std::io::Result<PathBuf>,
) -> ExitCode {
    let Ok(candidate) = candidate else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    };
    if !absolute_local_path(&candidate)
        || !fs::symlink_metadata(&candidate).is_ok_and(|metadata| metadata.is_dir())
    {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    }
    let key = claude_rendezvous_key(&candidate).await;
    // The MCP server can afford this one bounded `git` probe at its own startup; the hook cannot,
    // so it is left this cache instead of ever resolving the key itself (EYES-r2 §3).
    write_claude_key_cache(&candidate, &key);
    let Ok(path) = claude_runtime_path(&key) else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    };
    // Lets this client log its own lifecycle facts into the daemon's per-repository log.
    agent_ide::errorlog::init(&path);
    match rendezvous_with_claude_daemon(&path, &key, &launcher_template, &candidate).await {
        Some((runtime_path, _)) => {
            // Per EYES-r2 §2, this generation never owns the shared daemon's lifetime, so it holds
            // one `ClientLease` connection open for its own entire lifetime instead: the daemon's
            // idle-shutdown countdown only ever runs while zero managed Claude MCPs are attached.
            // `claude_reestablish_hook` replaces this handle with a fresh lease against the
            // re-established daemon, so the same guarantee holds across a reconnect.
            let Some((connection, attachment)) = open_client_lease(&runtime_path, &candidate).await
            else {
                return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None)
                    .await;
            };
            write_claude_candidate_attachment(&candidate, &attachment);
            let lease = Arc::new(Mutex::new(Some(connection)));
            let reestablish = claude_reestablish_hook(
                path,
                key,
                launcher_template,
                candidate,
                Arc::clone(&lease),
            );
            match StdioFacade::with_reestablishing_attachment(runtime_path, attachment, reestablish)
            {
                Some(facade) => serve_managed_stdio(facade, None, None, Some(lease), None).await,
                None => {
                    serve_managed_stdio(StdioFacade::unavailable(), None, None, Some(lease), None)
                        .await
                }
            }
        }
        None => serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await,
    }
}

/// Builds the closure a Claude [`StdioFacade`] calls to re-establish a lost shared daemon.
///
/// Repeats the exact [`rendezvous_with_claude_daemon`] path used at startup, so a daemon that
/// exited (idle timeout, `SIGTERM`, a crash, or a binary upgrade) is relaunched or re-adopted under
/// the same runtime-dir lock, and several MCP clients racing to relaunch it still end up with one
/// daemon (EYES-r2 §2). On success, also opens a fresh `ClientLease` against the re-established
/// daemon and stores it in `lease`, replacing (and thereby dropping) the dead one: otherwise the new
/// daemon generation would see zero leases from this still-live MCP and idle out from under it.
fn claude_reestablish_hook(
    path: PathBuf,
    key: PathBuf,
    launcher_template: PathBuf,
    candidate: PathBuf,
    lease: Arc<Mutex<Option<UnixStream>>>,
) -> ReestablishFn {
    Arc::new(move || {
        let path = path.clone();
        let key = key.clone();
        let launcher_template = launcher_template.clone();
        let candidate = candidate.clone();
        let lease = Arc::clone(&lease);
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result =
                rendezvous_with_claude_daemon(&path, &key, &launcher_template, &candidate).await;
            let result = if let Some((runtime_path, _)) = result {
                if let Some((connection, attachment)) =
                    open_client_lease(&runtime_path, &candidate).await
                {
                    write_claude_candidate_attachment(&candidate, &attachment);
                    *lease.lock().await = Some(connection);
                    Some((runtime_path, attachment))
                } else {
                    None
                }
            } else {
                None
            };
            agent_ide::errorlog::record(
                agent_ide::errorlog::Method::Client,
                if result.is_some() {
                    agent_ide::errorlog::Outcome::Reestablished
                } else {
                    agent_ide::errorlog::Outcome::Unavailable
                },
                agent_ide::errorlog::Fields {
                    reason: result
                        .is_none()
                        .then_some(agent_ide::errorlog::ReasonCode::ProviderUnavailable),
                    worktree: Some(&candidate),
                    host: Some(HostKind::Claude),
                    duration_ms: started.elapsed().as_millis().try_into().ok(),
                    ..Default::default()
                },
            );
            result
        })
    })
}

/// Opens and acknowledges one long-lived `ClientLease` connection to the daemon at `runtime`.
///
/// Fails open: any connect, framing, or correlation fault yields `None`, and the managed MCP still
/// serves normally without ever holding up an idle daemon's shutdown (EYES-r2 §2). The caller must
/// hold the returned stream for its own entire process lifetime.
async fn open_client_lease(runtime: &Path, candidate: &Path) -> Option<(UnixStream, String)> {
    agent_ide::app::open_claude_client_lease(runtime, candidate).await
}

/// Adopts a currently live daemon, or spawns one and adopts the eventual winner of a start race.
///
/// Never returns ownership of a child process or the runtime directory to the caller: whichever
/// generation actually serves the daemon manages its own lifetime independently of this MCP.
async fn rendezvous_with_claude_daemon(
    path: &Path,
    key: &Path,
    launcher_template: &Path,
    candidate: &Path,
) -> Option<(PathBuf, String)> {
    if let Some(attachment) = adopt_claude_daemon(path, key).await {
        return Some((path.to_owned(), attachment));
    }
    let runtime = ManagedRuntime::ensure_deterministic(path.to_owned()).ok()?;
    if let Some(attachment) = spawn_claude_daemon(&runtime, key, launcher_template, candidate).await
    {
        return Some((path.to_owned(), attachment));
    }
    // Lost the start race to a concurrent MCP, or startup failed for another reason; make one more
    // adoption attempt before reporting this generation unavailable.
    adopt_claude_daemon(path, key)
        .await
        .map(|attachment| (path.to_owned(), attachment))
}

/// Adopts an existing daemon only after its directory identity, lock, and health all check out.
///
/// Per EYES-r2, a directory this process did not itself just create is never adopted on doctor
/// output alone: [`ManagedRuntime::open_existing`] first requires it to be a real, non-symlink,
/// owner-only (mode `0700`) directory owned by the effective user, with its device/inode identity
/// re-validated from an open file descriptor. Only then is the exact "lock held and socket answers"
/// test from EYES-r1 §2 applied; a merely present but unhealthy runtime directory (a crashed
/// daemon's leftovers) is never adopted.
async fn adopt_claude_daemon(path: &Path, key: &Path) -> Option<String> {
    ManagedRuntime::open_existing(path.to_owned()).ok()?;
    let report = doctor_report(path).await.ok()?;
    if report.lock != DoctorLockState::Held
        || !matches!(report.status, DoctorStatus::Healthy { .. })
    {
        return None;
    }
    read_claude_attachment(key)
        .ok()
        .map(|(_, attachment)| attachment)
}

/// Spawns one detached daemon generation, or safely joins a concurrent MCP's in-flight spawn.
///
/// Only ever called after [`adopt_claude_daemon`] found nothing live. A [`StartDaemonError::WriteRace`]
/// means a concurrent MCP already owns this generation's launcher/attachment write slot, so its
/// files are never touched on a guess: this waits for it to answer healthy and adopts it. Only once
/// that wait times out (a crashed generation nobody is completing) are the leftover files cleared
/// for exactly one clean retry. A `WriteRace` on that retry, or any other failure, clears this
/// attempt's own files again so a later caller is not blocked by it.
async fn spawn_claude_daemon(
    runtime: &ManagedRuntime,
    key: &Path,
    launcher_template: &Path,
    candidate: &Path,
) -> Option<String> {
    match start_managed_daemon(
        runtime,
        launcher_template,
        key,
        candidate,
        ManagedHost::Claude,
    )
    .await
    {
        Ok((attachment, child)) => {
            // Detached: the daemon now owns its own lifetime independently of this MCP process, so
            // its handle is dropped without killing it (kill-on-drop was disabled for this spawn).
            drop(child);
            Some(attachment)
        }
        Err(StartDaemonError::WriteRace) => {
            if wait_for_external_health(&runtime.path).await {
                return adopt_claude_daemon(&runtime.path, key).await;
            }
            clear_claude_generation(runtime);
            match start_managed_daemon(
                runtime,
                launcher_template,
                key,
                candidate,
                ManagedHost::Claude,
            )
            .await
            {
                Ok((attachment, child)) => {
                    drop(child);
                    Some(attachment)
                }
                Err(_) => {
                    clear_claude_generation(runtime);
                    None
                }
            }
        }
        Err(StartDaemonError::Other) => {
            clear_claude_generation(runtime);
            None
        }
    }
}

/// Best-effort removal of one shared Claude runtime's generation-specific launcher/attachment files.
///
/// Never removes the shared rendezvous directory itself, and never fails the caller: a missing file
/// is already clean, and any other removal error is silently accepted, since a live daemon's own
/// files (if this race was lost) are recreated identically by nothing else touching this directory.
fn clear_claude_generation(runtime: &ManagedRuntime) {
    let _ = fs::remove_file(runtime.path.join("launcher.json"));
    let _ = fs::remove_file(runtime.path.join(CLAUDE_ATTACHMENT_FILE));
}

/// Distinguishes a benign concurrent write race from every other managed-daemon startup failure.
enum StartDaemonError {
    /// Another process already holds this rendezvous's launcher or attachment write slot; its files
    /// must never be assumed stale without first waiting to see whether it becomes healthy.
    WriteRace,
    /// Any other validation, spawn, or health-check failure.
    Other,
}

/// Validates the managed inputs and returns the exact healthy daemon child and private attachment.
///
/// The candidate must be the one captured by the parent and remain an absolute local directory.
/// Git identity is deliberately discovered later by the existing worker activation path. Launcher
/// executables and profiles are validated through [`LauncherConfig`] before the child starts. The
/// child receives only the candidate-stable telemetry path; its authority Store stays runtime-local.
/// `identity` is the value bound into the Claude attachment record (the resolved rendezvous key);
/// it is ignored for Codex, which keeps its pre-existing candidate-only identity. A Claude daemon is
/// additionally spawned detached in its own process group with kill-on-drop disabled, so it outlives
/// this MCP process; a Codex daemon keeps its pre-existing MCP-owned lifetime.
async fn start_managed_daemon(
    runtime: &ManagedRuntime,
    launcher_template: &Path,
    identity: &Path,
    candidate: &Path,
    host: ManagedHost,
) -> Result<(String, tokio::process::Child), StartDaemonError> {
    if !absolute_local_path(candidate)
        || !fs::symlink_metadata(candidate).is_ok_and(|metadata| metadata.is_dir())
        || !absolute_local_path(launcher_template)
    {
        return Err(StartDaemonError::Other);
    }
    let attachment = random_hex(32).map_err(|_| StartDaemonError::Other)?;
    let telemetry_database =
        managed_telemetry_database(candidate).map_err(|_| StartDaemonError::Other)?;
    let (launcher, bytes) =
        LauncherConfig::bind_one_candidate(launcher_template, &attachment, candidate)
            .map_err(|_| StartDaemonError::Other)?;
    if host == ManagedHost::Claude
        && launcher
            .target(&attachment)
            .is_none_or(|target| target.claude_profile.is_none())
    {
        return Err(StartDaemonError::Other);
    }
    launcher.verify().map_err(|_| StartDaemonError::Other)?;
    let launcher_path = match runtime.write_launcher(&bytes) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(StartDaemonError::WriteRace);
        }
        Err(_) => return Err(StartDaemonError::Other),
    };
    if host == ManagedHost::Claude {
        match runtime.write_claude_attachment(identity, &attachment) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StartDaemonError::WriteRace);
            }
            Err(_) => return Err(StartDaemonError::Other),
        }
    }
    // The error log is keyed by the repository, which a random Codex runtime directory cannot say.
    let log_key_source = match host {
        ManagedHost::Codex => claude_rendezvous_key(candidate).await,
        ManagedHost::Claude => identity.to_owned(),
    };
    let mut command =
        tokio::process::Command::new(std::env::current_exe().map_err(|_| StartDaemonError::Other)?);
    command
        .args(["daemon", "--runtime-dir"])
        .arg(&runtime.path)
        .env("AGENT_IDE_LAUNCHER_CONFIG", launcher_path)
        .env("AGENT_IDE_TELEMETRY_DATABASE", telemetry_database)
        .env(
            agent_ide::errorlog::LOG_KEY_ENV,
            &claude_rendezvous_identity(&log_key_source)[..16],
        )
        .env_remove("AGENT_IDE_STATE_DATABASE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match host {
        ManagedHost::Codex => {
            command.env("AGENT_IDE_MANAGED_CODEX_ATTACHMENT", &attachment);
            command.env_remove("AGENT_IDE_MANAGED_CLAUDE_DAEMON");
        }
        ManagedHost::Claude => {
            // Claude deliberately uses the existing hook-correlated daemon path. The daemon is
            // shared by every worktree of this repository, so it must outlive this one MCP process:
            // it runs detached, in its own process group, and is never killed by dropping the handle.
            command.env_remove("AGENT_IDE_MANAGED_CODEX_ATTACHMENT");
            command.env("AGENT_IDE_MANAGED_CLAUDE_DAEMON", "1");
            command.kill_on_drop(false);
            command.process_group(0);
        }
    }
    let mut child = command.spawn().map_err(|_| StartDaemonError::Other)?;
    if !health_check_owned_daemon(&mut child, &runtime.path).await {
        terminate_owned_daemon(Arc::new(Mutex::new(child))).await;
        return Err(StartDaemonError::Other);
    }
    Ok((attachment, child))
}

/// Waits a bounded interval for an external daemon (not owned by this process) to answer healthy.
///
/// Used only to decide whether a concurrent MCP's in-flight spawn for the same shared rendezvous
/// completed, before ever treating its files as a stale, crashed generation's leftovers.
async fn wait_for_external_health(runtime: &Path) -> bool {
    tokio::time::timeout(Duration::from_secs(7), async {
        loop {
            if doctor_report(runtime)
                .await
                .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. }))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
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
///
/// `lease` is an optional held-open `ClientLease` connection, refreshed in place by the managed
/// host's re-establish hook; it is dropped once serving ends, whatever the
/// reason, which is the client-side EOF that releases the daemon's lease count (EYES-r2 §2).
/// `publisher` is the managed Codex route publisher (Codex only): its records are retired before
/// the owned daemon is terminated and its runtime removed, on every exit path including signals.
async fn serve_managed_stdio(
    facade: StdioFacade,
    child: Option<SharedChild>,
    runtime: Option<Arc<Mutex<Option<ManagedRuntime>>>>,
    lease: Option<Arc<Mutex<Option<UnixStream>>>>,
    publisher: Option<SharedCodexPublisher>,
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
    drop(lease);
    if let Some(publisher) = publisher {
        // Permanent retirement: a queued publication racing this teardown publishes nothing.
        publisher
            .lock()
            .expect("managed codex publisher mutex")
            .retire();
    }
    if let Some(child) = child {
        terminate_owned_daemon(child).await;
    }
    if let Some(runtime) = runtime
        && let Some(runtime) = runtime.lock().await.take()
    {
        let _ = runtime.remove();
    }
    if served {
        ExitCode::SUCCESS
    } else {
        fail(AppError::InvalidResponse)
    }
}

/// The managed Codex daemon-child handle shared between exit observation and owned teardown.
///
/// The lock is only ever held across bounded synchronous probes or the final terminate/reap, so
/// the exit observer can always retry its non-blocking attempt.
type SharedChild = Arc<Mutex<tokio::process::Child>>;

/// Retires one managed Codex daemon generation's routes when its owned child exits.
///
/// A replacement generation increments `generation` while holding the publisher lock, so this
/// observer cannot retire the replacement's routes after the reconnect hook republishes them.
/// The child lock is taken only for a non-blocking probe; MCP teardown can still reap it.
async fn unpublish_when_daemon_exits(
    child: SharedChild,
    publisher: SharedCodexPublisher,
    generation: Arc<AtomicU64>,
    expected: u64,
) {
    loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if generation.load(Ordering::Acquire) != expected {
            return;
        }
        let Ok(mut child) = child.try_lock() else {
            continue;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            let mut publisher = publisher.lock().expect("managed codex publisher mutex");
            if generation.load(Ordering::Acquire) == expected {
                publisher.retire();
            }
            return;
        }
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
async fn terminate_owned_daemon(child: SharedChild) {
    let mut child = child.lock().await;
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

    /// `evidence record` parses its nine ordered flags plus the optional trailing
    /// `--shape-version` flag and emits the `{record, sandbox_state}` fragment a launcher
    /// configuration's `profiles` entry expects. A legacy v1 record is minted only when
    /// explicitly requested; the default is the v2 layout for supported managed captures.
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
            "--shape-version",
            "1",
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
            shape_version,
        } = parsed
        else {
            panic!("expected EvidenceRecord");
        };
        assert_eq!(shape_version, 1);
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
            shape_version,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();
        assert_eq!(value["record"]["profile_id"], "accepted-disabled");
        assert_eq!(value["sandbox_state"]["sandboxCwd"], "/private/tmp");
        assert!(
            value["record"].get("shape_version").is_none(),
            "a v1 record never grows the shape_version field"
        );
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

    /// T35B: a supported managed capture defaults to a v2 record without the flag, a `disabled`
    /// capture never silently becomes one, and unknown shape versions refuse.
    #[test]
    fn evidence_record_defaults_to_shape_v2_for_supported_managed_captures() {
        // The v2 shape binds the capture's cwd by realpath, so it must exist on any machine.
        std::fs::create_dir_all("/private/tmp/t35b-capture").unwrap();
        let sandbox_state = std::env::temp_dir().join(format!(
            "agent-ide-evidence-record-v2-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &sandbox_state,
            r#"{"codexLinuxSandboxExe":null,"permissionProfile":{"file_system":{"entries":[{"access":"read","path":{"type":"special","value":{"kind":"root"}}}],"type":"restricted"},"network":"restricted","type":"managed"},"sandboxCwd":"file:///private/tmp/t35b-capture","useLegacyLandlock":false}"#,
        )
        .unwrap();
        let evidence = D03ProfileEvidence {
            provider_binary: "accepted-codex".into(),
            toolchain: "toolchain".into(),
            configuration: "default".into(),
            trust: "accepted-local".into(),
            transport: "direct".into(),
            d03_evidence: "accepted-d03".into(),
        };
        let fragment = evidence_record(
            &sandbox_state,
            "accepted-managed-readonly",
            1,
            evidence.clone(),
            2,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&fragment).unwrap();
        assert_eq!(value["record"]["shape_version"], 2);
        // The stored capture is replayed byte-for-byte inside the fragment.
        let stored = value["sandbox_state"].to_string();
        let expected: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sandbox_state).unwrap()).unwrap();
        assert_eq!(stored, expected.to_string());

        // The default for a managed capture is v2; the flag only makes it explicit.
        let parsed = command(args(&[
            "evidence",
            "record",
            "--sandbox-state",
            sandbox_state.to_str().unwrap(),
            "--profile-id",
            "accepted-managed-readonly",
            "--revision",
            "1",
            "--provider-binary",
            "accepted-codex",
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
        let Command::EvidenceRecord { shape_version, .. } = parsed else {
            panic!("expected EvidenceRecord");
        };
        assert_eq!(shape_version, 2);

        // A disabled capture cannot support v2 and is refused instead of downgraded.
        std::fs::write(
            &sandbox_state,
            r#"{"permissionProfile":{"type":"disabled"},"codexLinuxSandboxExe":null,"sandboxCwd":"/private/tmp","useLegacyLandlock":false}"#,
        )
        .unwrap();
        assert!(matches!(
            evidence_record(&sandbox_state, "accepted-disabled", 1, evidence.clone(), 2),
            Err(AppError::InvalidResponse)
        ));
        assert!(
            evidence_record(&sandbox_state, "accepted-disabled", 1, evidence.clone(), 1).is_ok()
        );
        // Unknown generations refuse outright.
        assert!(matches!(
            evidence_record(&sandbox_state, "accepted-disabled", 1, evidence.clone(), 3),
            Err(AppError::InvalidResponse)
        ));
        assert!(matches!(
            command(args(&[
                "evidence",
                "record",
                "--sandbox-state",
                sandbox_state.to_str().unwrap(),
                "--shape-version",
                "9",
            ])),
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

    /// `evidence rejected` parses with no flag and refuses any extra argument (T25B).
    #[test]
    fn evidence_rejected_parses_only_bare() {
        assert!(matches!(
            command(args(&["evidence", "rejected"])),
            Ok(Command::EvidenceRejected)
        ));
        assert!(matches!(
            command(args(&["evidence", "rejected", "--extra"])),
            Err(AppError::InvalidResponse)
        ));
    }

    /// `evidence rejected` renders state-derived fields as one bounded control-free line (T25B).
    #[test]
    fn evidence_rejected_escapes_control_characters_in_fields() {
        assert_eq!(
            escaped_field("/tmp/\u{1b}[2J\nFORGED"),
            "/tmp/\\u{1b}[2J\\nFORGED"
        );
        assert_eq!(escaped_field(&"x".repeat(300)).chars().count(), 256);
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

    /// Telemetry query accepts its returned exclusive cursor with or without a closed tag filter.
    #[test]
    fn telemetry_query_cli_accepts_continuation_cursor() {
        assert!(matches!(
            command(args(&[
                "telemetry",
                "query",
                "--database",
                "/private/tmp/telemetry.sqlite",
                "--cursor",
                "42"
            ])),
            Ok(Command::TelemetryQuery {
                cursor: Some(42),
                filter: Filter::All,
                ..
            })
        ));
        assert!(matches!(
            command(args(&[
                "telemetry",
                "query",
                "--database",
                "/private/tmp/telemetry.sqlite",
                "--tag",
                "native_fallback",
                "--cursor",
                "42"
            ])),
            Ok(Command::TelemetryQuery {
                cursor: Some(42),
                filter: Filter::Tag("native_fallback"),
                ..
            })
        ));
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

    /// Proves a managed runtime cleanup cannot remove its candidate-stable local Store path.
    #[test]
    fn managed_telemetry_database_is_stable_outside_a_runtime_generation() {
        let candidate = fs::canonicalize(std::env::temp_dir()).unwrap();
        let state_parent = std::env::temp_dir().join(format!(
            "agent-ide-managed-state-{}-{}",
            std::process::id(),
            random_hex(4).unwrap()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&state_parent)
            .unwrap();
        let state = state_parent.join("app");
        let database = managed_telemetry_database_in(&state, &candidate).unwrap();
        let runtime = ManagedRuntime::create().unwrap();
        assert_ne!(database.parent(), Some(runtime.path.as_path()));
        assert_eq!(
            database,
            managed_telemetry_database_in(&state, &candidate).unwrap()
        );
        for directory in [
            state.clone(),
            state.join("telemetry"),
            database.parent().unwrap().to_path_buf(),
        ] {
            let metadata = fs::symlink_metadata(directory).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        runtime.remove().unwrap();
        fs::remove_dir_all(state_parent).unwrap();
    }

    /// Rejects preplaced symlink and nonprivate persistent telemetry directories without repair.
    #[test]
    fn managed_telemetry_database_rejects_unsafe_state_directories() {
        let parent = std::env::temp_dir().join(format!(
            "agent-ide-unsafe-state-{}-{}",
            std::process::id(),
            random_hex(4).unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        let state = parent.join("state");
        fs::DirBuilder::new().mode(0o755).create(&state).unwrap();
        assert!(managed_telemetry_database_in(&state, &parent).is_err());
        fs::remove_dir(&state).unwrap();
        std::os::unix::fs::symlink(&parent, &state).unwrap();
        assert!(managed_telemetry_database_in(&state, &parent).is_err());
        fs::remove_file(&state).unwrap();
        fs::remove_dir(parent).unwrap();
    }

    /// Proves a telemetry query refuses a missing database without creating a SQLite file.
    #[tokio::test]
    async fn telemetry_query_does_not_create_a_missing_database() {
        let database = std::env::temp_dir().join(format!(
            "agent-ide-telemetry-read-only-{}-{}.sqlite",
            std::process::id(),
            random_hex(4).unwrap()
        ));
        assert!(telemetry_owner(&database).await.is_err());
        assert!(!database.exists());
    }

    /// A repository and its linked worktree resolve one shared rendezvous and helper socket path.
    #[tokio::test]
    async fn claude_rendezvous_paths_share_one_runtime_between_worktrees() {
        let parent = std::env::temp_dir().join(format!(
            "agent-ide-rendezvous-cli-{}-{}",
            std::process::id(),
            random_hex(4).unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&parent).unwrap();
        let repo = parent.join("repo");
        let worktree = parent.join("linked");
        fs::create_dir(&repo).unwrap();
        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("/usr/bin/git")
                .args(arguments)
                .status()
                .unwrap();
            assert!(status.success(), "git {arguments:?} failed");
        };
        git(&["init", "-q", &repo.to_string_lossy()]);
        git(&[
            "-C",
            &repo.to_string_lossy(),
            "-c",
            "user.name=Rendezvous Fixture",
            "-c",
            "user.email=rendezvous@fixture.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "rendezvous fixture",
        ]);
        git(&[
            "-C",
            &repo.to_string_lossy(),
            "worktree",
            "add",
            "-q",
            &worktree.to_string_lossy(),
        ]);

        let (repo_runtime, repo_socket) = claude_rendezvous_paths(&repo).await.unwrap();
        let (worktree_runtime, worktree_socket) = claude_rendezvous_paths(&worktree).await.unwrap();
        assert_eq!(repo_runtime, worktree_runtime);
        assert_eq!(worktree_socket, repo_socket);
        assert_eq!(
            repo_socket,
            repo_runtime.join(agent_ide::assistance::claude_helper::HELPER_SOCKET)
        );
        // The shortened suffix is the shared key digest's sixteen-hex-character prefix, not any
        // candidate-specific path digest.
        let key = claude_rendezvous_key(&fs::canonicalize(&repo).unwrap()).await;
        let expected = fs::canonicalize(Path::new("/private/tmp"))
            .unwrap()
            .join(format!(
                "{CLAUDE_RUNTIME_PREFIX}{}",
                &claude_rendezvous_identity(&key)[..16]
            ));
        assert_eq!(repo_runtime, expected);
        assert!(
            claude_rendezvous_paths(&parent.join("absent"))
                .await
                .is_err()
        );

        fs::remove_dir_all(parent).unwrap();
    }

    /// Only a missing or unknown first argument (or an explicit help spelling) asks for usage; a
    /// known subcommand with bad arguments keeps its own error path.
    #[test]
    fn usage_is_requested_only_for_help_and_unknown_subcommands() {
        let request = |values: &[&str]| usage_request(&args(values).collect::<Vec<_>>());
        for help in ["--help", "-h", "help"] {
            assert_eq!(request(&[help]), Some(UsageRequest::Help));
        }
        assert_eq!(request(&[]), Some(UsageRequest::Unknown));
        assert_eq!(request(&["frobnicate"]), Some(UsageRequest::Unknown));
        assert_eq!(
            request(&["--runtime-dir", "x"]),
            Some(UsageRequest::Unknown)
        );
        for known in SUBCOMMANDS {
            assert_eq!(request(&[known]), None, "{known}");
            assert!(USAGE.contains(known), "usage lacks {known}");
        }
    }

    /// The reader's `git`-free fallback resolves a repository, a linked worktree and a path inside
    /// either (also through a symlink) to the same canonical git common directory.
    #[test]
    fn common_dir_from_git_files_resolves_worktrees_without_git() {
        let base = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("agent-ide-commondir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let common = base.join("main/.git");
        fs::create_dir_all(common.join("worktrees/linked")).unwrap();
        fs::write(common.join("worktrees/linked/commondir"), "../..\n").unwrap();
        let linked = base.join("linked/deep");
        fs::create_dir_all(&linked).unwrap();
        fs::write(
            base.join("linked/.git"),
            format!("gitdir: {}\n", common.join("worktrees/linked").display()),
        )
        .unwrap();
        fs::create_dir_all(base.join("main/sub")).unwrap();
        std::os::unix::fs::symlink(base.join("linked"), base.join("alias")).unwrap();
        for start in [
            base.join("main"),
            base.join("main/sub"),
            base.join("linked"),
            linked.clone(),
            base.join("alias/deep"),
        ] {
            assert_eq!(
                common_dir_from_git_files(&start),
                Some(common.clone()),
                "{start:?}"
            );
        }
        assert_eq!(common_dir_from_git_files(&base.join("nowhere")), None);
        fs::remove_dir_all(&base).unwrap();
    }

    /// Parses the two managed Codex hook CLI forms and rejects every wrong shape.
    #[test]
    fn managed_codex_hook_cli_forms_parse() {
        let parsed = |values: &[&str]| command(args(values));
        assert!(matches!(
            parsed(&["codex-hook", "--managed"]),
            Ok(Command::ManagedCodexHook)
        ));
        assert!(matches!(
            parsed(&["codex-hooks", "print"]),
            Ok(Command::CodexHooksPrint)
        ));
        assert!(matches!(
            parsed(&["codex-hook", "--runtime-dir", "/x"]),
            Ok(Command::CodexHook { .. })
        ));
        for wrong in [
            &["codex-hook"][..],
            &["codex-hook", "--managed", "extra"],
            &["codex-hook", "--runtime-dir"],
            &["codex-hooks"],
            &["codex-hooks", "write"],
            &["codex-hooks", "print", "extra"],
        ] {
            assert!(parsed(wrong).is_err(), "{wrong:?}");
        }
    }

    /// Shell-quoting survives spaces and apostrophes, and the printed fragment carries both.
    #[test]
    fn hooks_fragment_quotes_the_executable_and_snapshots() {
        let path = Path::new("/operator tools/a'b/agent-ide");
        let quoted = shell_single_quote(path);
        assert_eq!(quoted, "'/operator tools/a'\\''b/agent-ide'");
        // The real shell must expand the quoting back to exactly the original path.
        let output = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("printf '%s' {quoted}"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            path.to_str().unwrap()
        );
        let fragment = managed_hooks_fragment(path);
        assert_eq!(
            fragment,
            serde_json::json!({
                "hooks": {
                    "PreToolUse": [{"matcher": ".*", "hooks": [{
                        "type": "command",
                        "command": "'/operator tools/a'\\''b/agent-ide' codex-hook --managed",
                        "timeout": 1
                    }]}],
                    "PostToolUse": [{"matcher": ".*", "hooks": [{
                        "type": "command",
                        "command": "'/operator tools/a'\\''b/agent-ide' codex-hook --managed",
                        "timeout": 1
                    }]}],
                }
            })
        );
    }

    /// Retires the managed Codex publication exactly when the owned daemon child exit is observed.
    #[tokio::test]
    async fn publisher_unpublishes_when_the_owned_daemon_child_exits() {
        use agent_ide::assistance::codex_rendezvous::{CodexRouteIdentity, discover};
        use std::os::unix::{fs::PermissionsExt as _, net::UnixListener};

        // Kept directly below /private/tmp: the fixture socket path must stay under SUN_LEN.
        let base = PathBuf::from(format!(
            "/private/tmp/.aipw-{}-{}",
            std::process::id() % 100_000,
            random_hex(4).unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&base).unwrap();
        let root = base.join("rendezvous");
        let runtime_dir = base.join("runtime");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&runtime_dir)
            .unwrap();
        let listener = UnixListener::bind(runtime_dir.join("agent-ide.sock")).unwrap();
        fs::set_permissions(
            runtime_dir.join("agent-ide.sock"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let identity = CodexRouteIdentity::new("watched-session", "watched-actor").unwrap();
        let mut publisher =
            ManagedCodexPublisher::new(root.clone(), runtime_dir.clone(), "a1b2c3d4".repeat(8));
        publisher.publish(&identity).unwrap();
        drop(publisher);
        drop(listener);
        let child: SharedChild = Arc::new(Mutex::new(
            tokio::process::Command::new("/bin/sleep")
                .arg("0.3")
                .spawn()
                .unwrap(),
        ));
        let shared_publisher: SharedCodexPublisher = Arc::new(std::sync::Mutex::new(
            ManagedCodexPublisher::new(root.clone(), runtime_dir.clone(), "a1b2c3d4".repeat(8)),
        ));
        shared_publisher
            .lock()
            .expect("managed codex publisher mutex")
            .publish(&identity)
            .unwrap();
        assert!(discover(&root, &identity).is_some(), "route is published");
        tokio::spawn(unpublish_when_daemon_exits(
            Arc::clone(&child),
            Arc::clone(&shared_publisher),
            Arc::new(AtomicU64::new(0)),
            0,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut child = child.lock().await;
            child.wait().await.unwrap();
        })
        .await
        .unwrap();
        for _ in 0..100 {
            if discover(&root, &identity).is_none() {
                // The route directory stays for reuse (T29B-3r); only the record is retired.
                let leftovers = fs::read_dir(root.join(identity.digest()))
                    .map(|entries| entries.count())
                    .unwrap_or(0);
                assert_eq!(leftovers, 0, "the published record is retired");
                terminate_owned_daemon(Arc::clone(&child)).await;
                shared_publisher
                    .lock()
                    .expect("managed codex publisher mutex")
                    .unpublish_all();
                fs::remove_dir_all(base).unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("daemon exit was observed but the publication stayed discoverable");
    }
}
