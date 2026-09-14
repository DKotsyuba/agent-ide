//! Local infrastructure for the Agent IDE daemon.

/// Owns the process-local daemon and private Unix IPC boundary.
pub mod app;

/// Validates and tracks trusted host invocations and their active bindings.
pub mod assistance;

/// Admits and owns bounded local processes under supported host profiles.
pub mod execution;

/// Owns worktree identity, authority lifecycles, raw Git evidence, and source observations.
pub mod workspace;

/// Provides bounded internal LSP wire safety primitives for later semantic views.
pub mod intelligence;

/// Composes bounded diff summaries and hunk payload for Changes v0.1.
pub mod changes;

/// Owns closed, bounded, local-only usage telemetry and its durable query/export surface.
pub mod telemetry;
