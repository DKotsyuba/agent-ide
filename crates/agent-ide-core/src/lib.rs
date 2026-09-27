//! Language-independent core of the Agent IDE daemon.
//!
//! Everything here is keyed on registered languages (see [`lang::install`]); no module
//! names a language. Language crates implement [`lang::LanguageSupport`],
//! [`checks::LanguageChecks`] and [`intelligence::server::LanguageServer`] and the
//! application registers them at startup.

/// Owns the process-local daemon and private Unix IPC boundary.
pub mod app;

/// Validates and tracks trusted host invocations and their active bindings.
pub mod assistance;

/// Owns confined background project checks and the shared problem snapshot types.
pub mod checks;

/// Renders the bounded `<agent-ide>` problem block with per-actor dedup (EYES-r1 §6).
pub mod feed;

/// Admits and owns bounded local processes under supported host profiles.
pub mod execution;

/// Owns worktree identity, authority lifecycles, raw Git evidence, and source observations.
pub mod workspace;

/// Provides bounded internal LSP wire safety primitives for later semantic views.
pub mod intelligence;

/// Language identity, registration and the support contract for the symbol-addressed tools.
pub mod lang;

/// Builds and renders the language-independent project card `ide.start` shows.
pub mod project;

/// Composes bounded diff summaries and hunk payload for Changes v0.1.
pub mod changes;

/// Owns closed, bounded, local-only usage telemetry and its durable query/export surface.
pub mod telemetry;

/// Owns the append-only, bounded, closed-reason-code error log (T107).
pub mod errorlog;

/// Resolves the real per-user home from the password database instead of `$HOME`.
pub mod userhome;

/// Implements the `init` command: per-user home tree and launcher template creation.
pub mod init;

/// Implements the argument-less `doctor` command: read-only installation health findings.
pub mod doctor_install;
/// Installs one sealed release bundle into the immutable standalone layout (`self-install`).
pub mod selfinstall;
