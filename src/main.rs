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
    AppError, DaemonStop, DoctorLockState, DoctorReport, DoctorStatus, RuntimeDir,
    config::EffectiveConfig, daemon_needs_replacement, doctor_report, reported_daemon_version,
    request_daemon_stop, run_daemon_with_assistance,
};
use agent_ide::assistance::{
    assembly::ProductDispatcher,
    codex_rendezvous::ManagedCodexPublisher,
    facade::{
        DaemonCurrencyNote, EvictFn, ReestablishFn, RerootFn, RerootOutcome, SharedCodexPublisher,
        SharedDaemonNote, StdioFacade,
    },
    host_binding::HostKind,
    launcher::{AcceptedExecutable, LauncherConfig},
};
use agent_ide::{
    app::store::Store,
    selfinstall,
    telemetry::{Filter, Telemetry, TelemetryConfig},
};
use rmcp::{serve_server, transport::io::stdio};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

/// Selects an explicit mode; MCP writes only protocol messages to stdout and never autostarts.
#[tokio::main]
async fn main() -> ExitCode {
    agent_ide::languages::install();
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
    let parsed = command(arguments.into_iter());
    // Product test seam: only a daemon may simulate a slow (cold, loaded) start, before any
    // rendezvous-visible work runs; production never sets the variable.
    if matches!(parsed, Ok(Command::Daemon { .. })) {
        stall_daemon_startup_for_test().await;
    }
    match parsed {
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
                ManagedHost::Claude | ManagedHost::ClaudeCompatible => {
                    canonical_claude_project(claude_project_dir)
                }
            };
            run_managed_mcp(launcher_template, candidate, host).await
        }
        Ok(Command::AutoManagedMcp { launcher_template }) => {
            let (host, candidate) =
                auto_managed_candidate(claude_project_dir, AutoHostEvidence::from_env());
            run_managed_mcp(launcher_template, candidate, host).await
        }
        Ok(Command::ManagedClaudeHook) => {
            run_managed_claude_hook().await;
            ExitCode::SUCCESS
        }
        Ok(Command::ClaudeRendezvous { project_dir }) => {
            match claude_rendezvous_paths(&project_dir).await {
                Ok(runtime_dir) => {
                    println!("runtime_dir={}", runtime_dir.display());
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("agent-ide: {error}");
                    ExitCode::from(2)
                }
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
        Ok(Command::Init {
            home,
            config,
            allowed_roots,
        }) => match agent_ide::init::run(home, config, allowed_roots) {
            Ok(outcome) => {
                println!("{}", outcome.to_json());
                ExitCode::SUCCESS
            }
            Err(reason) => {
                eprintln!("agent-ide: {reason}");
                ExitCode::from(2)
            }
        },
        Ok(Command::Cache { prune }) => run_cache(prune).await,
        Ok(Command::DoctorInstall) => {
            let report = agent_ide::doctor_install::report().await;
            match serde_json::to_string(&report) {
                Ok(line) => {
                    println!("{line}");
                    if report
                        .findings
                        .iter()
                        .any(|finding| finding.severity == "error")
                    {
                        ExitCode::from(2)
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(_) => fail(AppError::InvalidResponse),
            }
        }
        Ok(Command::SelfInstall { args }) => match selfinstall::run(args) {
            Ok(summary) => {
                println!("{}", summary.to_json());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("agent-ide: {error}");
                ExitCode::FAILURE
            }
        },
        Err(error) => fail(error),
    }
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
    /// Queries an existing daemon without creating a directory or daemon process.
    Doctor { runtime_dir: PathBuf },
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
    /// Creates the per-user home tree and a minimal launcher template, never modifying existing files.
    Init {
        /// Explicit home root; defaults to the effective home's `.agent-ide` (`AGENT_IDE_HOME`-aware).
        home: Option<PathBuf>,
        /// Explicit launcher configuration path; defaults to the effective home's config path.
        config: Option<PathBuf>,
        /// Absolute allowed roots for the template; defaults to `~/projects` or the home itself.
        allowed_roots: Vec<PathBuf>,
    },
    /// Reports installation health read-only as bounded JSON findings (no daemon is contacted).
    DoctorInstall,
    /// Reports (`status`, a dry run) or applies now (`prune`) the cache retention policy.
    Cache {
        /// `true` for `prune`: remove what the policy selects; `false` for `status`.
        prune: bool,
    },
    /// Verifies and installs one sealed release bundle into the standalone layout, offline.
    SelfInstall {
        /// Explicit flags; the documented home, prefix, bin, and share defaults resolve at run time.
        args: selfinstall::Args,
    },
}

/// Selects the host-specific identity and binding behavior of a self-contained managed MCP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedHost {
    /// Existing Codex mode derives its candidate from the startup current directory and binds directly.
    Codex,
    /// Claude derives its candidate only from startup-captured `CLAUDE_PROJECT_DIR` and uses hooks.
    Claude,
    /// A recognized non-Codex host (no `CLAUDE_PROJECT_DIR`) whose hooks are the Claude-compatible
    /// `claude-hook` contract (ZCode): the Claude flow with the process directory as candidate,
    /// so `ide.start {root}` re-rooting binds it exactly like a moved Claude session.
    ClaudeCompatible,
}

/// Positive startup-environment evidence auto mode may choose a host from, read once.
///
/// The Codex markers are the optional startup variables the host probe already knows
/// (`examples/host_probe.rs` `STARTUP_ENV`). Live Codex children — including agent-run's — set
/// none of them, so absence is never Codex evidence. The ZCode markers were verified on a live
/// ZCode MCP child (`ZCODE_APP_VERSION`, `ZCODE_ENV`, `ZCODE_PROCESS_LABEL`, …, with no
/// `CLAUDE_PROJECT_DIR` and no `CODEX_*`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct AutoHostEvidence {
    /// Any `CODEX_THREAD_ID`/`CODEX_TURN_ID`/`CODEX_SESSION_ID` startup variable is present.
    codex: bool,
    /// Any `ZCODE_*` startup variable is present.
    zcode: bool,
}

impl AutoHostEvidence {
    /// Reads the markers from the process environment once, before any host is chosen.
    fn from_env() -> Self {
        let mut evidence = Self::default();
        for (name, _) in std::env::vars_os() {
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("ZCODE_") && name.len() > "ZCODE_".len() {
                evidence.zcode = true;
            }
            if matches!(
                name,
                "CODEX_THREAD_ID" | "CODEX_TURN_ID" | "CODEX_SESSION_ID"
            ) {
                evidence.codex = true;
            }
        }
        evidence
    }
}

/// Selects the existing managed host and captures its candidate without fallback between hosts.
///
/// A present `claude_project_dir` always selects Claude, including when canonical validation
/// fails; the returned error then drives the existing Claude fail-open MCP path. An absent value
/// needs positive non-Codex evidence before it may leave the Codex contract: a `ZCODE_*`
/// startup variable selects the Claude-compatible flow with the canonicalized process directory
/// (its project arrives through `ide.start {root}` re-rooting). Everything else — recognized
/// Codex evidence, and no evidence at all — keeps the previous Codex default, so agent-run's
/// Codex children and every existing auto-mode Codex user are unchanged.
fn auto_managed_candidate(
    claude_project_dir: Option<OsString>,
    evidence: AutoHostEvidence,
) -> (ManagedHost, std::io::Result<PathBuf>) {
    match claude_project_dir {
        Some(project) => (ManagedHost::Claude, canonical_claude_project(Some(project))),
        None if evidence.zcode && !evidence.codex => match std::env::current_dir() {
            Ok(directory) => (
                ManagedHost::ClaudeCompatible,
                canonical_claude_project(Some(directory.into_os_string())),
            ),
            Err(error) => (ManagedHost::ClaudeCompatible, Err(error)),
        },
        None => (ManagedHost::Codex, std::env::current_dir()),
    }
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
  doctor                                  report installation health as JSON findings
  doctor --runtime-dir <dir>              report daemon health
  codex-hook --runtime-dir <dir>          Codex native hook
  codex-hook --managed                    Codex native hook, managed route discovery
  codex-hooks print                       print the managed Codex hooks.json fragment
  claude-hook [--runtime-dir <dir>]       Claude native hook
  claude-rendezvous <project-dir>         print the Claude shared daemon runtime path
  errors [--repo <path>] [--all] [--summary] [--since <minutes>] [--limit <n>]
                                          read the error log
  init [--home <dir>] [--config <file>] [--allowed-root <dir>]...
                                          create the home tree and a launcher template
  self-install --release <dir> --version <v>
                                          install a sealed release bundle
  evidence executable --identity <id> <path>
                                          accepted-executable launcher fragment
  launcher check <file>                   validate a launcher configuration
  telemetry query|export --database <file> [--tag <tag>] [--cursor <n>]
  cache status|prune                      show or apply the cache retention policy
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
    "claude-rendezvous",
    "errors",
    "init",
    "evidence",
    "launcher",
    "telemetry",
    "self-install",
    "cache",
];

/// How a command line asks for the usage listing instead of a real command.
#[derive(Debug, Eq, PartialEq)]
enum UsageRequest {
    /// `--help`, `-h` or `help`: print the listing to stdout and succeed.
    Help,
    /// A missing or unknown subcommand: print the listing to stderr and exit 2.
    Unknown,
}

/// Classifies the first argument, plus a lone `--help`/`-h` after a known subcommand; any other
/// known subcommand with bad arguments keeps its own error.
fn usage_request(arguments: &[OsString]) -> Option<UsageRequest> {
    match arguments.first().map(|first| first.to_str()) {
        Some(Some("--help" | "-h" | "help")) => Some(UsageRequest::Help),
        Some(Some(first)) if SUBCOMMANDS.contains(&first) => match arguments {
            [_, flag] if flag == "--help" || flag == "-h" => Some(UsageRequest::Help),
            _ => None,
        },
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
    // `errors` accepts its flags in any order, unlike every other mode above.
    if let [mode, rest @ ..] = arguments.as_slice()
        && mode == "errors"
    {
        return parse_errors_command(rest);
    }
    // `doctor` with no arguments is the installation doctor; the daemon form keeps its flag.
    if let [mode] = arguments.as_slice()
        && mode == "doctor"
    {
        return Ok(Command::DoctorInstall);
    }
    if let [mode, action] = arguments.as_slice()
        && mode == "cache"
        && (action == "status" || action == "prune")
    {
        return Ok(Command::Cache {
            prune: action == "prune",
        });
    }
    // `init` accepts its flags in any order, like `errors`.
    if let [mode, rest @ ..] = arguments.as_slice()
        && mode == "init"
    {
        return parse_init_command(rest);
    }
    // `self-install` accepts its flags in any order; the installer validates them itself.
    if let [mode, rest @ ..] = arguments.as_slice()
        && mode == "self-install"
    {
        return selfinstall::parse_args(rest)
            .map_err(|_| AppError::InvalidResponse)
            .map(|args| Command::SelfInstall { args });
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

/// Parses `init`'s optional, any-order `--home`/`--config`/`--allowed-root` flags.
fn parse_init_command(rest: &[OsString]) -> Result<Command, AppError> {
    let mut home = None;
    let mut config = None;
    let mut allowed_roots = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let flag = rest[index].to_str().ok_or(AppError::InvalidResponse)?;
        match flag {
            "--home" if home.is_none() => {
                home = Some(PathBuf::from(flag_value(rest, index + 1)?));
                index += 2;
            }
            "--config" if config.is_none() => {
                config = Some(PathBuf::from(flag_value(rest, index + 1)?));
                index += 2;
            }
            "--allowed-root" => {
                allowed_roots.push(PathBuf::from(flag_value(rest, index + 1)?));
                index += 2;
            }
            _ => return Err(AppError::InvalidResponse),
        }
    }
    Ok(Command::Init {
        home,
        config,
        allowed_roots,
    })
}

/// Returns the flag value at `index`, rejecting a missing one.
fn flag_value(rest: &[OsString], index: usize) -> Result<&OsString, AppError> {
    rest.get(index).ok_or(AppError::InvalidResponse)
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
/// be cleared, and only before a confirmed-dead generation is replaced. Clones carry the same
/// fenced identity; the managed Codex state owns the copy responsible for final removal.
#[derive(Clone)]
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

    /// Reports whether this path still names the private directory captured at creation.
    ///
    /// A missing or replaced path returns `false`; an indeterminate filesystem error is returned
    /// to the caller, which must not treat it as proof that its owned daemon has died.
    fn identity_matches(&self) -> std::io::Result<bool> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => Ok(!metadata.file_type().is_symlink()
                && metadata.is_dir()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
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
        let path = self.path.join(agent_ide::app::LAUNCHER_FILE);
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
const CLAUDE_ATTACHMENT_FILE: &str = agent_ide::app::CLAUDE_ATTACHMENT_FILE;
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

/// The rendezvous key of a re-root target, or `None` while its repository identity is uncertain.
///
/// Unlike [`claude_rendezvous_key`], a failed probe never falls back to the directory itself: that
/// fallback would classify a sibling worktree as another repository and move the whole session to a
/// daemon keyed by the worktree. The `git` probe answers first, then the on-disk `.git` evidence;
/// only a directory with provably no `.git` above it is its own key.
async fn reroot_rendezvous_key(candidate: &Path) -> Option<PathBuf> {
    if let Some(common_dir) = git_common_dir(candidate).await
        && let Ok(key) = fs::canonicalize(common_dir)
    {
        return Some(key);
    }
    if let Some(key) = common_dir_from_git_files(candidate) {
        return Some(key);
    }
    candidate
        .ancestors()
        .all(|dir| {
            fs::symlink_metadata(dir.join(".git"))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        })
        .then(|| candidate.to_owned())
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

/// Prints the cache retention report for the real per-user state root (`docs/cache-retention.md`).
///
/// `status` only reports; `prune` sweeps now under the machine-wide sweep lock (exit 2 when
/// another sweep holds it), ignoring the hourly spacing but every lease and rule, and records its
/// removals in the error log of the current directory's repository (the one `errors` reads).
async fn run_cache(prune: bool) -> ExitCode {
    let Some(root) = agent_ide::retention::state_root() else {
        eprintln!("agent-ide: cannot resolve the user home");
        return ExitCode::from(2);
    };
    if !prune {
        print!(
            "{}",
            agent_ide::retention::sweep(&root, false).render(false)
        );
        return ExitCode::SUCCESS;
    }
    let Some(_lock) = agent_ide::retention::SweepLock::try_acquire(&root, None) else {
        eprintln!("agent-ide: another cache sweep is running");
        return ExitCode::from(2);
    };
    if let (Ok(directory), Some(logs)) = (
        std::env::current_dir().and_then(fs::canonicalize),
        agent_ide::errorlog::log_root(),
    ) && let Some(key) = error_log_dir(&logs, &directory).await.file_name()
    {
        agent_ide::errorlog::init_repository(&key.to_string_lossy());
    }
    let report = agent_ide::retention::sweep(&root, true);
    report.record();
    print!("{}", report.render(true));
    ExitCode::SUCCESS
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
    // Names the launch directory so cache retention can tell when it is gone.
    let marker = candidate_state.join(agent_ide::retention::MARKER_FILE_NAME);
    if fs::symlink_metadata(&marker).is_err() {
        let _ = fs::write(&marker, candidate.as_os_str().as_bytes());
    }
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
async fn claude_rendezvous_paths(project_dir: &Path) -> std::io::Result<PathBuf> {
    if !fs::symlink_metadata(project_dir).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("project directory {} does not exist", project_dir.display()),
        ));
    }
    let candidate = fs::canonicalize(project_dir)?;
    let key = claude_rendezvous_key(&candidate).await;
    claude_runtime_path(&key)
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

/// The repository identity a hook compares routes by: the canonical git common directory read from
/// `.git` files (never `git` itself), else the key the owning MCP cached for `dir`, else `dir`.
fn hook_repository_key(dir: &Path) -> PathBuf {
    common_dir_from_git_files(dir)
        .or_else(|| read_claude_key_cache(dir))
        .unwrap_or_else(|| dir.to_owned())
}

/// Resolves where a managed Claude hook submits: its runtime directory and candidate attachment.
///
/// The payload `cwd` route comes first: the nearest ancestor holding a cached rendezvous key (a
/// registered worktree) names the project. It is accepted only when that project belongs to the
/// same repository as `project_dir` — the canonical `CLAUDE_PROJECT_DIR` this session's own MCP
/// registered under — so a shell that wandered into another registered repository never delivers
/// this session's pre there (`hook_cwd_other_repository`); a sibling worktree of the session's
/// repository still routes by its own cwd. When the cwd route misses (no cached key, runtime or
/// candidate attachment, or another repository), `project_dir` gets the same lookup. That fallback
/// names the session's own registration and nothing else, so it can never reach a different
/// registered repository, and it only ever replaces a path that would drop the pre; the daemon's
/// attachment, session and exact call-id checks still decide the pairing. Without a `project_dir`
/// (the variable is absent; a present but unresolvable one never reaches this function) the cwd
/// route is the only route, unchanged.
///
/// Returns the route plus, when the fallback carried it, the closed reason the cwd route missed
/// (for the caller's journal warn); on failure the closed reason of the cwd route.
fn claude_hook_route(
    cwd: &Path,
    project_dir: Option<&Path>,
) -> Result<((PathBuf, String), Option<&'static str>), &'static str> {
    let resolve = |project: &Path| -> Result<(PathBuf, String), &'static str> {
        let key = read_claude_key_cache(project).ok_or("hook_no_key_cache")?;
        let (runtime, _) = read_claude_attachment(&key).map_err(|_| "hook_no_rendezvous")?;
        let attachment =
            read_claude_candidate_attachment(project).ok_or("hook_no_candidate_attachment")?;
        Ok((runtime, attachment))
    };
    let by_cwd = cwd
        .ancestors()
        .find(|path| read_claude_key_cache(path).is_some())
        .ok_or("hook_no_key_cache")
        .and_then(|project| match project_dir {
            Some(session) if hook_repository_key(project) != hook_repository_key(session) => {
                Err("hook_cwd_other_repository")
            }
            _ => resolve(project),
        });
    match (by_cwd, project_dir) {
        (Ok(route), _) => Ok((route, None)),
        (Err(miss), Some(project)) => resolve(project)
            .map(|route| (route, Some(miss)))
            .map_err(|_| miss),
        (Err(miss), None) => Err(miss),
    }
}

/// The closed journal detail of a managed Claude hook whose submission did not reach the daemon.
///
/// A submission that outlived the hook's own 250 ms budget (`now` at or past `deadline`) is a lost
/// pre: the call it belongs to will be refused `missing_pre`, so it gets its own detail,
/// `hook_submit_timeout`, which is journaled as a per-call warn (QW-4). Any other refusal keeps the
/// rate-limited bookkeeping detail `hook_submit_refused:unavailable`; the daemon journals a refusal
/// of a channel that already holds a binding itself, per call.
fn submit_refusal_detail(
    now: tokio::time::Instant,
    deadline: tokio::time::Instant,
) -> &'static str {
    if now >= deadline {
        "hook_submit_timeout"
    } else {
        "hook_submit_refused:unavailable"
    }
}

/// Window for one client-side hook skip: at most one journal line per detail per ten minutes.
const HOOK_SKIP_WINDOW: Duration = Duration::from_secs(600);

/// Records one rate-limited hook skip against a stamp file next to the journal.
///
/// The hook process is stateless, so the window state lives in one tiny file beside the journal,
/// holding `<window start ms> <suppressed>`; the content carries the window start because
/// rewriting the counter would reset a mtime-based window. Every failure is silent — the rate
/// limiter must never block or fail the hook. Returns the suppressed count when a line is due.
fn hook_skip_window(dir: &Path, detail: &str) -> Option<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0);
    let stamp = dir.join(format!(
        "{}.skip",
        &blake3::hash(detail.as_bytes()).to_hex().as_str()[..16]
    ));
    let mut parts = fs::read_to_string(&stamp)
        .map(|content| {
            content
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter();
    let mut window = agent_ide::errorlog::RateWindow::resume(
        parts.next().and_then(|value| value.parse::<u64>().ok()),
        parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0),
    );
    let due = window.record(now, HOOK_SKIP_WINDOW.as_millis() as u64);
    let (started, suppressed) = window.parts();
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(&stamp, format!("{} {}", started.unwrap_or(0), suppressed));
    due
}

/// Submits one argument-free managed Claude hook on the lease channel of the session's project.
///
/// The project is found from the payload cwd first, then from the session's own
/// `CLAUDE_PROJECT_DIR` when the cwd finds no rendezvous (see [`claude_hook_route`]). Missing or
/// malformed cwd, a present but unresolvable `CLAUDE_PROJECT_DIR` (`hook_project_dir_invalid`:
/// the session's repository identity is unknown, so no route may be trusted), cached key, runtime,
/// candidate attachment, or daemon returns silently to Claude
/// while recording a closed, path-free reason in the repository error log; a pre that paired only
/// through the fallback also leaves a per-call `warn` naming why the cwd route missed.
/// Per EYES-r2 §3, this never spawns `git` itself and so never risks the existing bounded 250 ms
/// total deadline on that account: the rendezvous key is only ever read from
/// [`read_claude_key_cache`], a hint the owning MCP server left behind at its own startup. Once
/// validated, the existing bounded Claude parser, sanitized transport, exact lifecycle correlation,
/// feedback rendering, and foreground-helper recognition remain unchanged. Closed input failure
/// details distinguish thread startup, timeout, read, size, and cwd failures without logging input.
async fn run_managed_claude_hook() {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(250);
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
    // A hook for a session that never activated the IDE is bookkeeping, not a failure (the T15B
    // noise): it is skipped at info, at most one line per detail per window, counting the rest.
    let journal_dir = log_candidate.as_deref().and_then(|candidate| {
        let key = common_dir_from_git_files(candidate)
            .or_else(|| read_claude_key_cache(candidate))
            .unwrap_or_else(|| candidate.to_owned());
        agent_ide::errorlog::log_root()
            .map(|root| root.join(&claude_rendezvous_identity(&key)[..16]))
    });
    let log = |detail: &str| {
        let inactive = matches!(
            detail,
            "hook_no_rendezvous"
                | "hook_no_key_cache"
                | "hook_no_candidate_attachment"
                | "hook_submit_refused:unavailable"
        );
        let count = inactive
            .then(|| {
                journal_dir
                    .as_ref()
                    .and_then(|dir| hook_skip_window(dir, detail))
            })
            .flatten();
        if inactive && count.is_none() {
            return; // Inside the rate window: suppressed, only counted.
        }
        agent_ide::errorlog::record(
            agent_ide::errorlog::Method::Hook,
            if inactive {
                agent_ide::errorlog::Outcome::Skipped
            } else {
                agent_ide::errorlog::Outcome::Unavailable
            },
            agent_ide::errorlog::Fields {
                host: Some(HostKind::Claude),
                detail: Some(detail),
                count,
                duration_ms: Some(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX)),
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
                .take(agent_ide::assistance::codex_hook::MAX_HOOK_INPUT_BYTES + 1)
                .read_to_end(&mut payload);
            let _ = sender.send(result.ok().map(|_| payload));
        })
        .is_err()
    {
        log("hook_input_spawn");
        return;
    }
    let payload = match tokio::time::timeout_at(deadline, receiver).await {
        Ok(Ok(Some(payload))) => payload,
        Err(_) => {
            log("hook_input_timeout");
            return;
        }
        _ => {
            log("hook_input_read");
            return;
        }
    };
    if payload.len() > agent_ide::assistance::codex_hook::MAX_HOOK_INPUT_BYTES as usize {
        log("hook_input_oversize");
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
    // The payload cwd follows the session's shell, which can leave the project; the project the
    // session's own MCP registered under (`CLAUDE_PROJECT_DIR`) is the fallback for that miss.
    // A present but unusable project directory (a removed session worktree) is not an absent one:
    // without its repository identity the cwd route cannot be told apart from another registered
    // repository's, so the pre is dropped locally rather than routed unrestricted.
    let project_dir = match std::env::var_os("CLAUDE_PROJECT_DIR") {
        None => None,
        Some(raw) => match canonical_claude_project(Some(raw)) {
            Ok(path) => Some(path),
            Err(_) => {
                log("hook_project_dir_invalid");
                return;
            }
        },
    };
    let (runtime, attachment) = match claude_hook_route(&cwd, project_dir.as_deref()) {
        Ok((route, rerouted)) => {
            if let Some(miss) = rerouted {
                // A per-call warn (not rate limited): this session's pre only paired because of
                // the fallback, and the cwd route's own closed miss reason names why.
                agent_ide::errorlog::record(
                    agent_ide::errorlog::Method::Hook,
                    agent_ide::errorlog::Outcome::Unavailable,
                    agent_ide::errorlog::Fields {
                        host: Some(HostKind::Claude),
                        detail: Some(&format!("hook_cwd_rerouted:{miss}")),
                        duration_ms: Some(
                            u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX),
                        ),
                        ..Default::default()
                    },
                );
            }
            route
        }
        Err(miss) => {
            log(miss);
            return;
        }
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
        log(submit_refusal_detail(tokio::time::Instant::now(), deadline));
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
        // An unrecognized auto host runs the Claude contract: its hooks are claude-hook, and its
        // project arrives through ide.start {root} re-rooting exactly like a moved Claude session.
        ManagedHost::Claude | ManagedHost::ClaudeCompatible => {
            run_managed_claude_mcp(launcher_template, candidate).await
        }
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
            let runtime = Arc::new(OwnedCodexRuntime {
                restart: Mutex::new(()),
                current: std::sync::Mutex::new(Some(runtime)),
                generation: Arc::new(AtomicU64::new(0)),
            });
            let reconnect = codex_reestablish_hook(
                launcher_template,
                candidate,
                attachment.clone(),
                Arc::clone(&child),
                Arc::clone(&runtime),
                Arc::clone(&lease),
                publisher.clone(),
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
            let evict: EvictFn = {
                let child = Arc::clone(&child);
                Arc::new(
                    move |runtime_dir: PathBuf, pid: Option<i32>, probes: u32, span: Duration| {
                        let child = Arc::clone(&child);
                        Box::pin(async move {
                            terminate_wedged_owned_daemon(child, &runtime_dir, pid, probes, span)
                                .await;
                        })
                    },
                )
            };
            let facade = facade.map(|facade| facade.with_wedge_eviction(evict));
            let Some(facade) = facade else {
                terminate_owned_daemon(child).await;
                if let Some(runtime) = runtime.current.lock().expect("owned runtime mutex").take() {
                    let _ = runtime.remove();
                }
                return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None)
                    .await;
            };
            if let Some(publisher) = &publisher {
                tokio::spawn(unpublish_when_daemon_exits(
                    Arc::clone(&child),
                    Arc::clone(publisher),
                    Arc::clone(&runtime.generation),
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
/// The guard serializes calls with shutdown. A transport fault preserves the exact live child and
/// runtime; only a reaped child or replaced runtime is restarted. A reaped child's own directory is
/// reused in place (its store and receipts survive a crash-only exit); a missing or replaced
/// directory is replaced by a fresh one, which enters teardown state before startup awaits, so
/// cancellation cannot leave it behind.
fn codex_reestablish_hook(
    launcher_template: PathBuf,
    candidate: PathBuf,
    attachment: String,
    child: SharedChild,
    runtime: Arc<OwnedCodexRuntime>,
    lease: Arc<Mutex<Option<UnixStream>>>,
    publisher: Option<SharedCodexPublisher>,
) -> ReestablishFn {
    let current = Arc::new(Mutex::new(Some((
        runtime
            .current
            .lock()
            .expect("owned runtime mutex")
            .as_ref()
            .expect("managed Codex runtime exists at startup")
            .path
            .clone(),
        attachment,
    ))));
    Arc::new(move || {
        let launcher_template = launcher_template.clone();
        let candidate = candidate.clone();
        let child = Arc::clone(&child);
        let runtime = Arc::clone(&runtime);
        let lease = Arc::clone(&lease);
        let publisher = publisher.clone();
        let current = Arc::clone(&current);
        Box::pin(async move {
            let _guard = runtime.restart.lock().await;
            if let Some(connection) = current.lock().await.clone() {
                let exited = matches!(child.lock().await.try_wait(), Ok(Some(_)));
                let foreign = runtime
                    .current
                    .lock()
                    .expect("owned runtime mutex")
                    .as_ref()
                    .is_some_and(|owned| {
                        owned.path != connection.0 || matches!(owned.identity_matches(), Ok(false))
                    });
                if !exited
                    && !foreign
                    && agent_ide::app::probe_health(&connection.0).await
                        != agent_ide::app::HealthProbe::Restarting
                {
                    return Some(connection);
                }
                if !exited && !foreign {
                    // The daemon answered `restarting`: it failed internally and exits by itself
                    // within moments. Waiting a bounded time for that exit lets this very call
                    // find the replacement instead of the daemon that is leaving.
                    let mut child = child.lock().await;
                    let _ = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
                }
            }
            terminate_owned_daemon(Arc::clone(&child)).await;
            *lease.lock().await = None;
            // The reaped daemon's directory still holds its store and receipts when it exited
            // crash-only (an internal fault) or was killed: the replacement runs in that same
            // directory, so a written edit stays the unknown outcome it is and is never repeated.
            // Only a directory that is gone or no longer this MCP's own is replaced by a new one.
            let kept = runtime
                .current
                .lock()
                .expect("owned runtime mutex")
                .clone()
                .filter(|old| matches!(old.identity_matches(), Ok(true)));
            let in_place = kept.is_some();
            let fresh = match kept {
                Some(old) => {
                    clear_claude_generation(&old);
                    old
                }
                None => {
                    if let Some(old) = runtime.current.lock().expect("owned runtime mutex").take() {
                        let _ = old.remove();
                    }
                    let fresh = ManagedRuntime::create().ok()?;
                    *runtime.current.lock().expect("owned runtime mutex") = Some(fresh.clone());
                    fresh
                }
            };
            let path = fresh.path.clone();
            let started = start_managed_daemon(
                &fresh,
                &launcher_template,
                &candidate,
                &candidate,
                ManagedHost::Codex,
            )
            .await;
            let Ok((attachment, mut new_child)) = started else {
                if !in_place
                    && let Some(fresh) = runtime.current.lock().expect("owned runtime mutex").take()
                {
                    let _ = fresh.remove();
                }
                return None;
            };
            // Product test seam: hold a live replacement before its lease is acquired so SIGTERM
            // can prove that cancellation still reaps the child and removes this registered runtime.
            if let Some(milliseconds) =
                agent_ide::test_seams::var("AGENT_IDE_MANAGED_CODEX_RESTART_STALL_MS")
                    .and_then(|value| value.parse::<u64>().ok())
            {
                // The marker makes this narrow test window observable after startup health has
                // succeeded, before the lease request can reach the replacement daemon.
                let _ = fs::write(path.join("restart-lease-pending"), []);
                tokio::time::sleep(Duration::from_millis(milliseconds.min(60_000))).await;
            }
            let Some(lease_connection) =
                agent_ide::app::open_client_lease(&path, "managed-codex-mcp").await
            else {
                // No lease or actor work exists on this replacement; a stalled acknowledgement
                // must be force-reaped before its fenced runtime is removed.
                let _ = new_child.start_kill();
                let _ = new_child.wait().await;
                if !in_place
                    && let Some(fresh) = runtime.current.lock().expect("owned runtime mutex").take()
                {
                    let _ = fresh.remove();
                }
                return None;
            };
            let target = (path, attachment);
            *current.lock().await = Some(target.clone());
            *lease.lock().await = Some(lease_connection);
            *child.lock().await = new_child;
            if let Some(publisher) = publisher {
                let mut publisher_lock = publisher.lock().expect("managed codex publisher mutex");
                let next = runtime.generation.fetch_add(1, Ordering::AcqRel) + 1;
                publisher_lock.rebind(target.0.clone(), target.1.clone());
                drop(publisher_lock);
                tokio::spawn(unpublish_when_daemon_exits(
                    Arc::clone(&child),
                    publisher,
                    Arc::clone(&runtime.generation),
                    next,
                ));
            }
            Some(target)
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
/// One managed Claude session's current repository binding (T15B).
#[derive(Clone)]
struct ClaudeBinding {
    /// Canonical bound project directory: the startup `CLAUDE_PROJECT_DIR`, or a later re-root.
    candidate: PathBuf,
    /// Repository-wide rendezvous key of `candidate`.
    key: PathBuf,
    /// Canonical allowed roots copied once from the validated launcher template; a re-root admits
    /// only directories below one of these, exactly as activation itself does.
    allowed_roots: Vec<PathBuf>,
    /// Other worktrees of the same repository registered for this session's actors (a subagent's
    /// own worktree): every attach re-registers the ones that still exist, so their hooks reach a
    /// restarted daemon too. Bounded by [`MAX_CLAUDE_SIBLINGS`], oldest dropped first.
    siblings: Vec<PathBuf>,
}

/// Reports whether `path` definitely no longer is a directory (not found, or replaced by a
/// non-directory); any other metadata failure is uncertain and keeps the path.
fn definitely_gone(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => !metadata.is_dir(),
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Most sibling worktrees one managed Claude session keeps registered.
const MAX_CLAUDE_SIBLINGS: usize = 32;

/// Registers `candidate` as a sibling of the session `binding` (see [`register_claude_sibling`])
/// and remembers it for later re-registration, answering the facade's re-root outcome.
async fn register_claude_sibling_into(
    binding: &std::sync::Mutex<ClaudeBinding>,
    key: &Path,
    candidate: PathBuf,
) -> RerootOutcome {
    let Ok(runtime) = claude_runtime_path(key) else {
        return RerootOutcome::Failed;
    };
    if !register_claude_sibling(&runtime, key, &candidate).await {
        return RerootOutcome::Failed;
    }
    let mut shared = binding.lock().expect("claude binding mutex");
    shared
        .siblings
        .retain(|sibling| sibling != &candidate && !definitely_gone(sibling));
    if shared.siblings.len() >= MAX_CLAUDE_SIBLINGS {
        shared.siblings.remove(0);
    }
    shared.siblings.push(candidate);
    RerootOutcome::Registered
}

/// Registers one more worktree of the bound repository with its shared daemon and makes its hooks
/// deliverable (key cache and candidate attachment), without moving the session: the daemon
/// pairs every managed Claude attachment on one channel, so the session keeps dispatching through
/// its own attachment while that worktree's actors are paired by their own pre-hooks.
async fn register_claude_sibling(runtime: &Path, key: &Path, candidate: &Path) -> bool {
    // The lease only performs the registration; dropping it leaves the target registered.
    let Some((_lease, attachment)) = open_client_lease(runtime, candidate).await else {
        return false;
    };
    write_claude_key_cache(candidate, key);
    write_claude_candidate_attachment(candidate, &attachment);
    true
}

/// Attaches one binding to its repository's shared daemon exactly as a fresh session there would.
///
/// The one shared tail of managed startup, a reconnect, and a re-root (T15B): refresh the
/// project's key cache so its hooks can resolve the daemon (EYES-r2 §3 — the hook cannot resolve
/// the key itself), adopt or spawn the repository's shared daemon — replacing an outdated idle one
/// on the way (0.6.7) — open one fresh `ClientLease`, publish the candidate attachment, and swap
/// the held lease so the daemon's idle countdown never runs under a live MCP. Returns the live
/// `(runtime_dir, attachment)` pair; `note` receives what the attach learned about the daemon's
/// version currency.
async fn attach_claude_binding(
    binding: &ClaudeBinding,
    launcher_template: &Path,
    lease: &Arc<Mutex<Option<UnixStream>>>,
    note: &SharedDaemonNote,
) -> Option<(PathBuf, String)> {
    write_claude_key_cache(&binding.candidate, &binding.key);
    let path = claude_runtime_path(&binding.key).ok()?;
    let (runtime_path, _) = rendezvous_with_claude_daemon(
        &path,
        &binding.key,
        launcher_template,
        &binding.candidate,
        note,
    )
    .await?;
    let (connection, attachment) = open_client_lease(&runtime_path, &binding.candidate).await?;
    write_claude_candidate_attachment(&binding.candidate, &attachment);
    *lease.lock().await = Some(connection);
    for sibling in binding.siblings.iter().filter(|sibling| sibling.is_dir()) {
        register_claude_sibling(&runtime_path, &binding.key, sibling).await;
    }
    Some((runtime_path, attachment))
}

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
    let allowed_roots = LauncherConfig::read(&launcher_template)
        .map(|config| config.allowed_roots().to_vec())
        .unwrap_or_default();
    let binding = Arc::new(std::sync::Mutex::new(ClaudeBinding {
        candidate: candidate.clone(),
        key,
        allowed_roots,
        siblings: Vec::new(),
    }));
    let lease: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
    // What every attach learns about the shared daemon's version currency (0.6.7): the initial
    // rendezvous, each re-establish, and each re-root all write it, and the facade renders it.
    let note: SharedDaemonNote = Arc::new(std::sync::Mutex::new(DaemonCurrencyNote::default()));
    // Per EYES-r2 §2, this generation never owns the shared daemon's lifetime, so it holds one
    // `ClientLease` connection open for its own entire lifetime instead: the daemon's
    // idle-shutdown countdown only ever runs while zero managed Claude MCPs are attached.
    // `claude_reestablish_hook` replaces this handle with a fresh lease against the
    // re-established daemon, so the same guarantee holds across a reconnect or a re-root.
    let initial = binding.lock().expect("claude binding mutex").clone();
    let Some((runtime_path, attachment)) =
        attach_claude_binding(&initial, &launcher_template, &lease, &note).await
    else {
        return serve_managed_stdio(StdioFacade::unavailable(), None, None, None, None).await;
    };
    let reestablish = claude_reestablish_hook(
        Arc::clone(&binding),
        launcher_template.clone(),
        Arc::clone(&lease),
        Arc::clone(&note),
    );
    let reroot = claude_reroot_hook(
        Arc::clone(&binding),
        launcher_template.clone(),
        Arc::clone(&lease),
        Arc::clone(&note),
        candidate.clone(),
    );
    // A shared daemon that holds its runtime but stays silent across repeated probes is replaced
    // by the facade's wedge watch; every safety check lives in `evict_wedged_daemon`.
    let evict: EvictFn = Arc::new(
        |runtime_dir: PathBuf, pid: Option<i32>, probes: u32, span: Duration| {
            Box::pin(async move {
                let _ = agent_ide::app::evict_wedged_daemon(&runtime_dir, pid, probes, span).await;
            })
        },
    );
    match StdioFacade::with_reestablishing_claude_attachment(
        runtime_path,
        attachment,
        candidate,
        reestablish,
        reroot,
        note,
    )
    .map(|facade| facade.with_wedge_eviction(evict))
    {
        Some(facade) => {
            // The held lease stream is a live death notice for the shared daemon: watching it
            // heals the session the moment a generation ends, instead of at the next failed tool
            // call, so the host's next pre-hook already finds a healthy rendezvous (T15B).
            tokio::spawn(watch_claude_lease(Arc::clone(&lease), facade.clone()));
            serve_managed_stdio(facade, None, None, Some(lease), None).await
        }
        None => {
            serve_managed_stdio(StdioFacade::unavailable(), None, None, Some(lease), None).await
        }
    }
}

/// Heals a managed Claude session the moment its shared daemon generation ends (T15B).
///
/// EOF or error on the held lease stream means the daemon exited — idle expiry, a signal, or a
/// crash. The watcher re-attaches through the startup path at once; the facade marks the session
/// for transparent re-activation, so the host's next pre-hook lands on a healthy daemon and the
/// next tool call re-runs the remembered activation itself. A failed heal retries at a bounded
/// cadence; nothing here can block or fail the MCP's serving loop.
async fn watch_claude_lease(lease: Arc<Mutex<Option<UnixStream>>>, facade: StdioFacade) {
    follow_lease(lease, || facade.recover_lost_daemon()).await;
}

/// How often the lease watcher checks whether a newer lease replaced the one it is reading.
const LEASE_SWAP_POLL: Duration = Duration::from_millis(250);

/// Follows whichever lease `lease` currently holds, forever (F-03).
///
/// The watcher takes the stored stream and reads it until the daemon ends it, then calls `heal`
/// until it reports a fresh daemon attached (which stores a new lease). A re-root or reconnect
/// that stores a newer lease while the watcher is still reading the old one makes the watcher drop
/// the old stream — so the old daemon can idle out — and follow the new one, whose daemon's death
/// it must notice. Never returns.
async fn follow_lease<Heal, Healed>(lease: Arc<Mutex<Option<UnixStream>>>, mut heal: Heal)
where
    Heal: FnMut() -> Healed,
    Healed: std::future::Future<Output = bool>,
{
    use tokio::io::AsyncReadExt as _;
    loop {
        let stream = lease.lock().await.take();
        let Some(mut stream) = stream else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let ended = async {
            let mut discard = [0_u8; 256];
            while matches!(stream.read(&mut discard).await, Ok(read) if read > 0) {}
        };
        let replaced = async {
            loop {
                tokio::time::sleep(LEASE_SWAP_POLL).await;
                if lease.lock().await.is_some() {
                    break;
                }
            }
        };
        tokio::select! {
            () = ended => {
                // This generation ended. Heal until a fresh one is attached, then watch it — but
                // only while no newer lease is stored: a replaced daemon closing its old stream
                // must not make the current, healthy daemon look replaced.
                while lease.lock().await.is_none() && !heal().await {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
            () = replaced => {}
        }
        // `stream` is dropped here: on a replacement that closes the old daemon's lease.
    }
}

/// Builds the closure a Claude [`StdioFacade`] calls to re-establish a lost shared daemon.
///
/// Repeats the exact attach path used at startup for the currently bound repository, so a daemon
/// that exited (idle timeout, `SIGTERM`, a crash, or a binary upgrade) is relaunched or re-adopted
/// under the same runtime-dir lock, and several MCP clients racing to relaunch it still end up with
/// one daemon (EYES-r2 §2). On success, also opens a fresh `ClientLease` against the re-established
/// daemon and stores it in `lease`, replacing (and thereby dropping) the dead one: otherwise the new
/// daemon generation would see zero leases from this still-live MCP and idle out from under it.
/// After a re-root (T15B) the shared binding already names the moved repository, so this same hook
/// re-establishes that root's daemon. Sibling worktrees whose directory is definitely gone are
/// dropped first, and a definitely removed home attaches through the first live sibling.
fn claude_reestablish_hook(
    binding: Arc<std::sync::Mutex<ClaudeBinding>>,
    launcher_template: PathBuf,
    lease: Arc<Mutex<Option<UnixStream>>>,
    note: SharedDaemonNote,
) -> ReestablishFn {
    Arc::new(move || {
        let binding = {
            let mut shared = binding.lock().expect("claude binding mutex");
            // A removed sibling is retired for good; any other failure keeps it for the next try.
            shared.siblings.retain(|sibling| !definitely_gone(sibling));
            // A removed home cannot be attached again: the session survives through a live
            // sibling of the same repository instead.
            if definitely_gone(&shared.candidate)
                && let Some(index) = shared.siblings.iter().position(|sibling| sibling.is_dir())
            {
                shared.candidate = shared.siblings.remove(index);
            }
            shared.clone()
        };
        let launcher_template = launcher_template.clone();
        let lease = Arc::clone(&lease);
        let note = Arc::clone(&note);
        Box::pin(async move {
            let started = std::time::Instant::now();
            let result = attach_claude_binding(&binding, &launcher_template, &lease, &note).await;
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
                    worktree: Some(&binding.candidate),
                    host: Some(HostKind::Claude),
                    duration_ms: started.elapsed().as_millis().try_into().ok(),
                    ..Default::default()
                },
            );
            result
        })
    })
}

/// Builds the closure a Claude [`StdioFacade`] calls to re-root a moved session (T15B).
///
/// A host that moves a project never restarts this MCP process, so a session started in one
/// directory stays bound there while its hooks already run with the new project directory and
/// find no rendezvous. Only after the daemon refused a Start with a cause proving this session's
/// hooks no longer pair there does the facade call this hook: `Some(root)` naming another worktree
/// of the same repository on a current daemon only registers it ([`register_claude_sibling_into`];
/// the daemon pairs every worktree on one channel, so nobody moves), any other admitted root
/// re-attaches through the exact fresh-session path of [`attach_claude_binding`] — key cache,
/// shared daemon, lease, candidate attachment — and `None` returns to `startup`, the host's project directory this MCP
/// process began in, so a root-less start can never stay stranded on a root its session left. Only
/// targets below the template's `allowed_roots` are accepted; that single rule is the whole
/// security boundary and is unchanged.
fn claude_reroot_hook(
    binding: Arc<std::sync::Mutex<ClaudeBinding>>,
    launcher_template: PathBuf,
    lease: Arc<Mutex<Option<UnixStream>>>,
    note: SharedDaemonNote,
    startup: PathBuf,
) -> RerootFn {
    Arc::new(move |requested, other_actors| {
        let binding = Arc::clone(&binding);
        let launcher_template = launcher_template.clone();
        let lease = Arc::clone(&lease);
        let note = Arc::clone(&note);
        let startup = startup.clone();
        Box::pin(async move {
            let started = std::time::Instant::now();
            let current = binding.lock().expect("claude binding mutex").clone();
            // An unresolvable explicit root is not this hook's decision: the daemon's own admission
            // answer names it, exactly as for a Start that never carried a usable root. An
            // unresolvable host project directory cannot be returned to; the pair stays.
            let (candidate, outside) = match requested {
                Some(requested) => match fs::canonicalize(&requested) {
                    Ok(candidate) if absolute_local_path(&candidate) => (Some(candidate), false),
                    _ => (None, true),
                },
                None => (
                    fs::canonicalize(&startup)
                        .ok()
                        .filter(|path| absolute_local_path(path)),
                    false,
                ),
            };
            let outcome = if outside {
                RerootOutcome::OutsideAllowedRoots
            } else {
                match candidate {
                    None => RerootOutcome::Unchanged,
                    Some(candidate) => {
                        if agent_ide::assistance::launcher::admit_worktree(
                            &current.allowed_roots,
                            &candidate,
                        )
                        .is_err()
                        {
                            RerootOutcome::OutsideAllowedRoots
                        } else if candidate == current.candidate {
                            RerootOutcome::Unchanged
                        } else if let Some(key) = reroot_rendezvous_key(&candidate).await {
                            let same_repository = key == current.key;
                            let current_daemon = note
                                .lock()
                                .expect("daemon currency note mutex")
                                .line()
                                .is_none();
                            if !same_repository && other_actors {
                                // F-02: other actors of this session still work in the bound
                                // repository, and their hooks stay there; moving the session
                                // would strand them. Refuse before any route changes.
                                RerootOutcome::OtherRepository
                            } else if same_repository
                                && current_daemon
                                && !definitely_gone(&current.candidate)
                            {
                                // Same repository on a current daemon: register the worktree for
                                // its actors' hooks and keep the session where it is. An outdated
                                // daemon still pairs per attachment, so it keeps the 0.10.2 move
                                // below, and a removed home moves the session to this worktree.
                                register_claude_sibling_into(&binding, &current.key, candidate)
                                    .await
                            } else {
                                let moved = ClaudeBinding {
                                    candidate: candidate.clone(),
                                    key,
                                    allowed_roots: current.allowed_roots.clone(),
                                    // Siblings belong to their own repository's daemon.
                                    siblings: if same_repository {
                                        current.siblings.clone()
                                    } else {
                                        Vec::new()
                                    },
                                };
                                match attach_claude_binding(
                                    &moved,
                                    &launcher_template,
                                    &lease,
                                    &note,
                                )
                                .await
                                {
                                    Some((runtime, attachment)) => {
                                        *binding.lock().expect("claude binding mutex") = moved;
                                        RerootOutcome::Attached(runtime, attachment, candidate)
                                    }
                                    None => RerootOutcome::Failed,
                                }
                            }
                        } else {
                            // Uncertain repository identity never moves or registers anything.
                            RerootOutcome::Failed
                        }
                    }
                }
            };
            agent_ide::errorlog::record(
                agent_ide::errorlog::Method::Client,
                if matches!(
                    outcome,
                    RerootOutcome::Attached(..)
                        | RerootOutcome::Registered
                        | RerootOutcome::Unchanged
                ) {
                    agent_ide::errorlog::Outcome::Completed
                } else {
                    agent_ide::errorlog::Outcome::Unavailable
                },
                agent_ide::errorlog::Fields {
                    reason: matches!(outcome, RerootOutcome::Failed)
                        .then_some(agent_ide::errorlog::ReasonCode::ProviderUnavailable),
                    worktree: Some(&current.candidate),
                    host: Some(HostKind::Claude),
                    detail: Some(match &outcome {
                        RerootOutcome::Unchanged => "reroot:unchanged",
                        RerootOutcome::Attached(..) => "reroot:attached",
                        RerootOutcome::Registered => "reroot:registered",
                        RerootOutcome::OutsideAllowedRoots => "reroot:outside_allowed_roots",
                        RerootOutcome::OtherRepository => "reroot:other_repository",
                        RerootOutcome::Failed => "reroot:failed",
                    }),
                    duration_ms: started.elapsed().as_millis().try_into().ok(),
                    ..Default::default()
                },
            );
            outcome
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
/// Every adoption checks the daemon's reported version against this binary (0.6.7): an outdated
/// idle daemon is stopped and replaced by a current spawn, an outdated busy one keeps serving with
/// the honest line `note` records, and a current or newer one is adopted unchanged. Two bounded
/// rounds cover a replacement racing another front's spawn; after them, whatever answers is
/// served, because no daemon at all is worse than an outdated one.
async fn rendezvous_with_claude_daemon(
    path: &Path,
    key: &Path,
    launcher_template: &Path,
    candidate: &Path,
    note: &SharedDaemonNote,
) -> Option<(PathBuf, String)> {
    for _ in 0..2 {
        if let Some(attachment) = adopt_current_claude_daemon(path, key, note).await {
            return Some((path.to_owned(), attachment));
        }
        wait_for_exiting_daemon(path).await;
        if let Ok(runtime) = ManagedRuntime::ensure_deterministic(path.to_owned()) {
            if let Some(attachment) =
                spawn_claude_daemon(&runtime, key, launcher_template, candidate, note).await
            {
                return Some((path.to_owned(), attachment));
            }
        } else {
            break;
        }
    }
    // Lost the start race to a concurrent MCP, or startup failed for another reason; make one more
    // adoption attempt before reporting this generation unavailable.
    let adopted = adopt_claude_daemon(path, key)
        .await
        .map(|attachment| (path.to_owned(), attachment));
    if adopted.is_some() {
        note_last_resort_daemon(path, note).await;
    }
    adopted
}

/// Waits a bounded interval while a daemon still holds the runtime lock without answering healthy.
///
/// Such a daemon is either starting (another front's spawn) or exiting after an internal fault
/// (it answers `restarting`, then releases the lock); spawning over it would only start a child
/// that loses the lock and exits. The wait ends as soon as the lock is released or the daemon
/// answers healthy, and at the latest after eight seconds, so a hung daemon never blocks the
/// attach.
async fn wait_for_exiting_daemon(path: &Path) {
    let _ = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match doctor_report(path).await {
                Ok(report)
                    if report.lock == DoctorLockState::Held
                        && !matches!(report.status, DoctorStatus::Healthy { .. }) =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                _ => return,
            }
        }
    })
    .await;
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

/// Adopts the repository's live daemon only when its reported version may serve this front
/// (0.6.7), asking an outdated idle one to stop so the caller spawns the current binary.
///
/// Returns the attachment to keep serving, or `None` when nothing is adoptable or the outdated
/// daemon acknowledged `daemon.stop` (the caller's spawn path then rebuilds the rendezvous). A
/// daemon still bound by another session is never stopped under it: the note records the honest
/// start-card line instead. A pre-0.6.7 daemon reports no version and cannot be asked to stop at
/// all, so it too keeps serving under that line until it idles out after its last session.
async fn adopt_current_claude_daemon(
    path: &Path,
    key: &Path,
    note: &SharedDaemonNote,
) -> Option<String> {
    let attachment = adopt_claude_daemon(path, key).await?;
    let DoctorStatus::Healthy { daemon_generation } = doctor_report(path).await.ok()?.status else {
        return None;
    };
    let daemon_version = reported_daemon_version(&daemon_generation);
    let front_version = front_version();
    let outdated = daemon_needs_replacement(&daemon_generation, &front_version);
    if !outdated {
        note.lock()
            .expect("daemon currency note mutex")
            .note_current();
        return Some(attachment);
    }
    // Only a version-reporting daemon can be asked to stop; a pre-0.6.7 one stays silent.
    if daemon_version.is_some() && request_daemon_stop(path).await == DaemonStop::Stopped {
        wait_for_daemon_exit(path).await;
        note.lock()
            .expect("daemon currency note mutex")
            .note_replaced();
        return None;
    }
    note.lock()
        .expect("daemon currency note mutex")
        .note_outdated(outdated_daemon_line(daemon_version, &front_version));
    Some(attachment)
}

/// The product version of this running front, the reference every adopted daemon is compared to.
/// Returns the installed front version, with a validated test seam for release-version scenarios.
fn front_version() -> String {
    agent_ide::test_seams::var("AGENT_IDE_TEST_FRONT_VERSION")
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value
                    .split('.')
                    .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

/// One honest start-card line for a session still served by a daemon older than this front.
fn outdated_daemon_line(daemon_version: Option<&str>, front_version: &str) -> String {
    match daemon_version {
        Some(version) => format!(
            "daemon: {version} still serving (another session is active); restart that session or wait for it to stop for {front_version}"
        ),
        None => format!(
            "daemon: older than 0.6.7 still serving (it reports no version and cannot be asked to stop); stop it manually or wait for it to idle out after its last session, then start a fresh session for {front_version}"
        ),
    }
}

/// Waits a bounded interval for a daemon that acknowledged `daemon.stop` to finish its orderly
/// exit, so the caller's spawn does not immediately collide with the dying generation's lock,
/// launcher, or runtime directory. A stuck daemon simply exhausts the wait; the spawn path's
/// existing write-race recovery then clears whatever it left behind.
async fn wait_for_daemon_exit(path: &Path) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let healthy = doctor_report(path)
                .await
                .is_ok_and(|report| matches!(report.status, DoctorStatus::Healthy { .. }));
            if !healthy && !path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .ok();
}

/// Records the currency of a last-resort adopted daemon without ever stopping it (0.6.7): the
/// rendezvous already spent its replacement rounds, so the note alone says what is being served.
async fn note_last_resort_daemon(path: &Path, note: &SharedDaemonNote) {
    let daemon_generation = match doctor_report(path).await {
        Ok(report) => match report.status {
            DoctorStatus::Healthy {
                ref daemon_generation,
            } => daemon_generation.clone(),
            DoctorStatus::Unavailable => return,
        },
        Err(_) => return,
    };
    let daemon_version = reported_daemon_version(&daemon_generation);
    let front_version = front_version();
    let mut note = note.lock().expect("daemon currency note mutex");
    if daemon_needs_replacement(&daemon_generation, &front_version) {
        note.note_outdated(outdated_daemon_line(daemon_version, &front_version));
    } else {
        note.note_current();
    }
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
    note: &SharedDaemonNote,
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
            // A daemon this binary itself started is current by construction; the note keeps only
            // a replacement marker this same attach set by stopping an outdated daemon (0.6.7).
            note.lock()
                .expect("daemon currency note mutex")
                .note_spawned_current();
            drop(child);
            Some(attachment)
        }
        Err(StartDaemonError::WriteRace) => {
            if wait_for_external_health(&runtime.path).await {
                // The winner is a foreign daemon: adopt it under the same version check, so a
                // concurrent older front's spawn is itself replaced when nothing binds it (0.6.7).
                return adopt_current_claude_daemon(&runtime.path, key, note).await;
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
                    note.lock()
                        .expect("daemon currency note mutex")
                        .note_spawned_current();
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

/// Best-effort removal of one managed runtime's generation-specific launcher/attachment files, for
/// a shared Claude runtime and for a Codex runtime restarted in place.
///
/// Never removes the shared rendezvous directory itself, and never fails the caller: a missing file
/// is already clean, and any other removal error is silently accepted, since a live daemon's own
/// files (if this race was lost) are recreated identically by nothing else touching this directory.
fn clear_claude_generation(runtime: &ManagedRuntime) {
    let _ = fs::remove_file(runtime.path.join(agent_ide::app::LAUNCHER_FILE));
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
        ManagedHost::Claude | ManagedHost::ClaudeCompatible => identity.to_owned(),
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
        ManagedHost::Claude | ManagedHost::ClaudeCompatible => {
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

/// Bounds how long a daemon this process spawned may take to answer its first health request.
///
/// The daemon binds its socket only after dispatcher initialization has measured every accepted
/// executable (over five seconds on a loaded machine by the product's own measurement note), and a
/// cold CI runner can take several times that; a shorter window kills slow-but-successful starts,
/// so every managed startup, reconnect, and restart-heal attempt fails and its leftover generation
/// files then cost each later attempt a further write-race wait. Thirty seconds is the established
/// startup budget (the product fixture's daemon startup wait and the harness's exchange ceiling),
/// stays under the dispatcher's own sixty-second initialize bound, and a child that exits first
/// still fails fast. A tool call that re-establishes inside this window may therefore wait most of
/// it; a host whose own call timeout is shorter cancels that call, the lease watcher still
/// completes the heal on its own, and the session's next call lands on the healthy daemon.
const OWNED_DAEMON_READINESS_BUDGET: Duration = Duration::from_secs(30);

/// Test-only startup stall, honored only when the product-test seam
/// `AGENT_IDE_DAEMON_STARTUP_STALL_MS` is set to a millisecond value (capped at one minute).
///
/// The daemon sleeps before any rendezvous-visible work, so a cold loaded machine's slow start —
/// the exact condition that killed replacements inside shorter health windows — can be reproduced
/// deterministically. Only a `test-seams` build reads the variable.
async fn stall_daemon_startup_for_test() {
    let Some(milliseconds) = agent_ide::test_seams::var("AGENT_IDE_DAEMON_STARTUP_STALL_MS")
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return;
    };
    tokio::time::sleep(Duration::from_millis(milliseconds.min(60_000))).await;
}

/// Waits a bounded interval for an external daemon (not owned by this process) to answer healthy.
///
/// Used only to decide whether a concurrent MCP's in-flight spawn for the same shared rendezvous
/// completed, before ever treating its files as a stale, crashed generation's leftovers. This is a
/// race between writers, not a slow-start budget: the previous generation's orderly shutdown
/// closes client leases before it removes its own files, so the first replacement attempt usually
/// meets its leftover launcher here, and the wait must stay short enough that clearing those files
/// and retrying still heals well inside the startup budget.
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

/// Waits within [`OWNED_DAEMON_READINESS_BUDGET`] for the exact child to answer the existing
/// side-effect-free health RPC.
async fn health_check_owned_daemon(child: &mut tokio::process::Child, runtime: &Path) -> bool {
    tokio::time::timeout(OWNED_DAEMON_READINESS_BUDGET, async {
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
/// A signal exits the process after this cleanup because Tokio's stdin worker may remain blocked
/// on an open host pipe even after the MCP service has been canceled.
async fn serve_managed_stdio(
    facade: StdioFacade,
    child: Option<SharedChild>,
    runtime: Option<Arc<OwnedCodexRuntime>>,
    lease: Option<Arc<Mutex<Option<UnixStream>>>>,
    publisher: Option<SharedCodexPublisher>,
) -> ExitCode {
    // Install signal handlers before `serve_server` can send its initialize reply. The host may
    // signal us as soon as it receives that reply, before `serve_server` has returned here.
    let termination = managed_termination_signal();
    tokio::pin!(termination);
    let service = tokio::select! {
        result = serve_server(facade, stdio()) => Some(result),
        () = &mut termination => None,
    };
    let (served, signalled) = match service {
        Some(Ok(service)) => {
            tokio::select! {
                result = service.waiting() => (result.is_ok(), false),
                () = &mut termination => (true, true),
            }
        }
        Some(Err(_)) => (false, false),
        None => (true, true),
    };
    // A restart may be between runtime creation and child/lease installation. Wait for it (or
    // for cancellation to drop it) before retiring routes and tearing down the owned generation.
    let _restart = if let Some(runtime) = &runtime {
        Some(runtime.restart.lock().await)
    } else {
        None
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
    if let Some(runtime) = runtime.as_ref()
        && let Some(runtime) = runtime.current.lock().expect("owned runtime mutex").take()
    {
        let _ = runtime.remove();
    }
    if signalled {
        // Tokio's stdio reader may still be blocked in a non-cancellable stdin read after an
        // in-flight request. All owned resources are settled above; exit without waiting for that
        // runtime worker, since the host may leave the MCP pipe open after SIGTERM.
        std::process::exit(0);
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

/// Coordinates this MCP's owned runtime registration with restart and final teardown.
///
/// `restart` excludes teardown while a generation is being replaced. `current` uses a short
/// synchronous lock so a newly created private directory is registered before any async await;
/// its fenced removal remains the MCP's responsibility even if restart is canceled.
struct OwnedCodexRuntime {
    /// Serializes replacement with teardown without holding a filesystem lock across awaits.
    restart: Mutex<()>,
    /// Current fenced directory, including a pending replacement not yet serving calls.
    current: std::sync::Mutex<Option<ManagedRuntime>>,
    /// Fences each child-exit route observer against a later replacement generation.
    generation: Arc<AtomicU64>,
}

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

/// Registers SIGINT/SIGTERM immediately, then resolves after the first signal.
///
/// Registration failure leaves the returned future pending so stdio EOF remains the exit path.
fn managed_termination_signal() -> impl std::future::Future<Output = ()> {
    let signals = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
    );
    async move {
        let (Ok(mut interrupt), Ok(mut terminate)) = signals else {
            std::future::pending::<()>().await;
            return;
        };
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
}

/// Force-replaces the exact daemon child this MCP owns after its wedge watch found it silent.
///
/// Signals only that owned child, and only if the evidence reaches the same minimum the shared
/// path enforces (`WEDGE_MIN_PROBES` probes over `WEDGE_MIN_SPAN`), the child is still the daemon
/// whose silence was observed (`expected_pid`), and a probe of its control path at `runtime_dir`,
/// made while this function holds the child, still finds it silent (a daemon that answers is never
/// signalled, however busy). Holding the child slot serializes this with a restart, so a
/// replacement that took the slot meanwhile is never signalled. The runtime store is marked for
/// retention first ([`agent_ide::app::retain_runtime_store`]), so even an orderly exit on `SIGTERM`
/// keeps the receipts. `SIGTERM`, up to [`agent_ide::app::WEDGE_TERM_GRACE`] for it to exit, a
/// second probe (a daemon that resumed and answers is not killed), then `SIGKILL`; the caller then
/// restarts the daemon in the same directory. Journals the replacement with the probe count and
/// span (see `agent_ide::app::record_forced_replacement`) only when the child is really gone.
async fn terminate_wedged_owned_daemon(
    child: SharedChild,
    runtime_dir: &Path,
    expected_pid: Option<i32>,
    probes: u32,
    span: Duration,
) {
    if probes < agent_ide::app::WEDGE_MIN_PROBES || span < agent_ide::app::WEDGE_MIN_SPAN {
        return;
    }
    let mut child = child.lock().await;
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let Some(pid) = child.id() else {
        return;
    };
    if expected_pid.is_some_and(|expected| expected != pid as i32)
        || agent_ide::app::probe_health(runtime_dir).await != agent_ide::app::HealthProbe::Silent
    {
        return;
    }
    if agent_ide::app::retain_runtime_store(runtime_dir).is_err() {
        return;
    }
    let _ = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    let mut killed = false;
    if tokio::time::timeout(agent_ide::app::WEDGE_TERM_GRACE, child.wait())
        .await
        .is_err()
    {
        // A child that resumed and answers is leaving by itself and is left to finish.
        if agent_ide::app::probe_health(runtime_dir).await != agent_ide::app::HealthProbe::Silent {
            return;
        }
        killed = true;
        let _ = child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    // Only a child that is really gone was replaced; one that survived `SIGKILL` is not claimed.
    if child.try_wait().ok().flatten().is_some() {
        agent_ide::app::record_forced_replacement(pid as i32, probes, span, killed);
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
    /// `launcher check` parses a bare path and reports malformed configuration consistently.
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

    /// Auto host selection uses positive evidence in a fixed order: Claude's project variable,
    /// Codex markers, then a ZCode marker; no evidence at all keeps the previous Codex default.
    #[test]
    fn auto_managed_candidate_preserves_invalid_claude_selection() {
        let no_evidence = AutoHostEvidence::default();
        let (codex, candidate) = auto_managed_candidate(None, no_evidence);
        assert_eq!(
            codex,
            ManagedHost::Codex,
            "no evidence keeps the Codex default"
        );
        assert!(candidate.is_ok());

        let (codex_evidence, candidate) = auto_managed_candidate(
            None,
            AutoHostEvidence {
                codex: true,
                zcode: true,
            },
        );
        assert_eq!(
            codex_evidence,
            ManagedHost::Codex,
            "Codex evidence wins over a ZCode marker"
        );
        assert!(candidate.is_ok());

        let (zcode, candidate) = auto_managed_candidate(
            None,
            AutoHostEvidence {
                codex: false,
                zcode: true,
            },
        );
        assert_eq!(zcode, ManagedHost::ClaudeCompatible);
        assert_eq!(
            candidate.unwrap(),
            fs::canonicalize(std::env::current_dir().unwrap()).unwrap()
        );

        let project = fs::canonicalize(std::env::temp_dir()).unwrap();
        let (claude, candidate) =
            auto_managed_candidate(Some(project.clone().into_os_string()), no_evidence);
        assert_eq!(claude, ManagedHost::Claude);
        assert_eq!(candidate.unwrap(), project);

        let (invalid_claude, candidate) =
            auto_managed_candidate(Some(OsString::from("relative")), no_evidence);
        assert_eq!(invalid_claude, ManagedHost::Claude);
        assert!(candidate.is_err());
    }

    /// The client hook's stamp file emits the first skip at once, counts repeats silently, and
    /// flushes the total once the window has expired; no failure of the limiter can surface.
    #[test]
    fn hook_skip_window_emits_counts_and_flushes_per_window() {
        let dir = std::env::temp_dir().join(format!("t15b-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let detail = "hook_no_rendezvous";
        assert_eq!(hook_skip_window(&dir, detail), Some(0), "first line emits");
        for _ in 0..2 {
            assert_eq!(hook_skip_window(&dir, detail), None, "repeats suppress");
        }
        // Age the stamp past the window by rewriting its start far in the past.
        let identity = blake3::hash(detail.as_bytes()).to_hex().to_string();
        let stamp = dir.join(format!("{}.skip", &identity[..16]));
        let suppressed = fs::read_to_string(&stamp).unwrap();
        let count = suppressed.split_whitespace().nth(1).unwrap().to_owned();
        fs::write(&stamp, format!("0 {count}")).unwrap();
        assert_eq!(
            hook_skip_window(&dir, detail),
            Some(2),
            "next window flushes the count"
        );
        // A corrupt or unwritable stamp never panics and still emits.
        fs::write(&stamp, "not a window").unwrap();
        assert_eq!(hook_skip_window(&dir, detail), Some(0));
        let _ = fs::remove_dir_all(&dir);
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

        let repo_runtime = claude_rendezvous_paths(&repo).await.unwrap();
        let worktree_runtime = claude_rendezvous_paths(&worktree).await.unwrap();
        assert_eq!(repo_runtime, worktree_runtime);
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
            for help in ["--help", "-h"] {
                assert_eq!(request(&[known, help]), Some(UsageRequest::Help), "{known}");
            }
            assert_eq!(request(&[known, "--help", "x"]), None, "{known}");
            assert!(USAGE.contains(known), "usage lacks {known}");
        }
    }

    /// QW-4: a hook submission that outlived its 250 ms budget is a lost pre and is journaled
    /// per call; any other refusal stays rate-limited bookkeeping.
    #[test]
    fn a_submission_past_the_hook_budget_is_a_lost_pre() {
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_millis(250);
        assert_eq!(
            submit_refusal_detail(start + Duration::from_millis(10), deadline),
            "hook_submit_refused:unavailable"
        );
        assert_eq!(
            submit_refusal_detail(deadline, deadline),
            "hook_submit_timeout"
        );
        assert_eq!(
            submit_refusal_detail(deadline + Duration::from_millis(5), deadline),
            "hook_submit_timeout"
        );
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

    /// F-03: when a newer lease replaces the one the watcher is reading, the watcher closes the old
    /// stream (so the old daemon can idle out) and then notices the new daemon's death.
    #[tokio::test]
    async fn lease_watcher_follows_a_replaced_lease() {
        use std::sync::atomic::AtomicUsize;
        use tokio::io::AsyncReadExt as _;
        let (old_lease, mut old_daemon) = UnixStream::pair().unwrap();
        let (new_lease, new_daemon) = UnixStream::pair().unwrap();
        let lease = Arc::new(Mutex::new(Some(old_lease)));
        let healed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&healed);
        let watcher = tokio::spawn(follow_lease(Arc::clone(&lease), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::future::ready(true)
        }));
        // Wait until the watcher has taken the old stream, then swap in a newer lease exactly as
        // a re-root's attach does.
        while lease.lock().await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        *lease.lock().await = Some(new_lease);
        let mut byte = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(3), old_daemon.read(&mut byte))
            .await
            .expect("the watcher must drop the replaced lease stream");
        assert_eq!(closed.unwrap(), 0, "the old daemon sees its lease closed");
        assert_eq!(
            healed.load(Ordering::SeqCst),
            0,
            "a swap is not a lost daemon"
        );
        // The new daemon dies: the watcher must notice and heal exactly once.
        drop(new_daemon);
        tokio::time::timeout(Duration::from_secs(3), async {
            while healed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the death of the new daemon must be noticed");
        watcher.abort();
    }

    /// F-03: the death of a daemon whose lease was already replaced is not a lost daemon: the
    /// current lease is healthy, so the watcher heals only when the current daemon dies.
    #[tokio::test]
    async fn lease_watcher_ignores_the_death_of_a_replaced_daemon() {
        use std::sync::atomic::AtomicUsize;
        let (old_lease, old_daemon) = UnixStream::pair().unwrap();
        let (new_lease, new_daemon) = UnixStream::pair().unwrap();
        let lease = Arc::new(Mutex::new(Some(old_lease)));
        let healed = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&healed);
        let watcher = tokio::spawn(follow_lease(Arc::clone(&lease), move || {
            counter.fetch_add(1, Ordering::SeqCst);
            std::future::ready(true)
        }));
        while lease.lock().await.is_some() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The replacement is stored and the old daemon dies at once, before the swap poll runs.
        *lease.lock().await = Some(new_lease);
        drop(old_daemon);
        tokio::time::sleep(LEASE_SWAP_POLL * 3).await;
        assert_eq!(
            healed.load(Ordering::SeqCst),
            0,
            "the current daemon is alive"
        );
        drop(new_daemon);
        tokio::time::timeout(Duration::from_secs(3), async {
            while healed.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the death of the current daemon must be noticed");
        watcher.abort();
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
