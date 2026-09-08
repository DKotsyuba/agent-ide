//! Command-line entrypoint for the local Application daemon and doctor.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use agent_ide::app::{AppError, DoctorReport, DoctorStatus, RuntimeDir, doctor_report, run_daemon};

/// Parses the two initial executable modes and prints only bounded operational status.
#[tokio::main]
async fn main() -> ExitCode {
    match command(std::env::args_os().skip(1)) {
        Ok(Command::Daemon { runtime_dir }) => match RuntimeDir::prepare_for_daemon(runtime_dir) {
            Ok(runtime_dir) => match run_daemon(runtime_dir).await {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(error),
            },
            Err(error) => fail(error),
        },
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
        Err(error) => fail(error),
    }
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
    println!("protocol.assistance_transport=v2-unavailable-without-peer-dispatcher");
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
    /// Serves the health-only daemon until the process is interrupted or killed.
    Daemon { runtime_dir: PathBuf },
    /// Queries an existing daemon without creating a directory or daemon process.
    Doctor { runtime_dir: PathBuf },
}

/// Rejects unknown, missing, and extra CLI arguments before any filesystem or daemon action.
fn command(arguments: impl Iterator<Item = OsString>) -> Result<Command, AppError> {
    let arguments = arguments.collect::<Vec<_>>();
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
        _ => Err(AppError::InvalidResponse),
    }
}
