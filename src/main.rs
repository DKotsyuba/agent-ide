//! Command-line entrypoint for the local Application daemon and doctor.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use agent_ide::app::{AppError, DoctorStatus, RuntimeDir, doctor, run_daemon};

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
        Ok(Command::Doctor { runtime_dir }) => match doctor(&runtime_dir).await {
            Ok(DoctorStatus::Healthy { daemon_generation }) => {
                println!("healthy {daemon_generation}");
                ExitCode::SUCCESS
            }
            Ok(DoctorStatus::Unavailable) => {
                println!("unavailable");
                ExitCode::FAILURE
            }
            Err(error) => fail(error),
        },
        Err(error) => fail(error),
    }
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
