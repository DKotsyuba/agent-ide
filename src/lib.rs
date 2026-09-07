//! Local infrastructure for the Agent IDE daemon.

/// Owns the process-local daemon and private Unix IPC boundary.
pub mod app;

/// Validates and tracks trusted host invocations and their active bindings.
pub mod assistance;

/// Admits and owns bounded local processes under supported host profiles.
pub mod execution;
