//! Command-line entrypoint for the MCP facade, Codex hook, local daemon, and doctor.

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

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
    match command(std::env::args_os().skip(1)) {
        Ok(Command::Daemon { runtime_dir }) => match RuntimeDir::prepare_for_daemon(runtime_dir) {
            Ok(runtime_dir) => match run_daemon_with_assistance(
                runtime_dir,
                Arc::new(match std::env::var("AGENT_IDE_LAUNCHER_CONFIG") {
                    Ok(path) => match LauncherConfig::read(std::path::Path::new(&path)) {
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
    /// Serves the static five-tool MCP surface on stdio without creating local runtime state.
    Mcp { runtime_dir: PathBuf },
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
    let [mode, flag, runtime_dir] = arguments.as_slice() else {
        return Err(AppError::InvalidResponse);
    };
    if flag != "--runtime-dir" {
        return Err(AppError::InvalidResponse);
    }
    let runtime_dir = PathBuf::from(runtime_dir);
    match mode.to_str() {
        Some("daemon") => Ok(Command::Daemon { runtime_dir }),
        Some("doctor") => Ok(Command::Doctor { runtime_dir }),
        Some("mcp") => Ok(Command::Mcp { runtime_dir }),
        Some("codex-hook") => Ok(Command::CodexHook { runtime_dir }),
        Some("claude-hook") => Ok(Command::ClaudeHook { runtime_dir }),
        _ => Err(AppError::InvalidResponse),
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
}
