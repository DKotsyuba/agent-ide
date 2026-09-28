//! Shared operator-home resolution for Rust tool subprocesses.
//!
//! The rust-analyzer session profile and the confined `cargo check` runner must hand their Cargo
//! subprocesses the same operator-owned registry: the real user home from the password database
//! (never `$HOME`, which a host such as `agent-run` substitutes), with the operator-declared
//! cargo home override winning when one is configured.

use std::{
    env,
    path::{Path, PathBuf},
};

/// Returns the real user home directory from the password database (never `$HOME`, which a host
/// may substitute), honoring only the `AGENT_IDE_HOME` override; when neither resolves the system
/// temp dir is substituted so path construction stays absolute.
pub(crate) fn real_home() -> PathBuf {
    agent_ide_core::userhome::user_home().unwrap_or_else(env::temp_dir)
}

/// Returns the effective cargo home: the explicitly configured directory, else the home's
/// `.cargo`.
///
/// `configured` is the operator-declared override (`project_checks.rust.cargo_home` for checks);
/// an inherited `CARGO_HOME` is deliberately ignored because a host may substitute the
/// environment, leaving it pointing at an empty registry.
pub(crate) fn effective_cargo_home(configured: Option<&Path>, home: &Path) -> PathBuf {
    configured
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".cargo"))
}
