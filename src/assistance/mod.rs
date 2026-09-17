//! Bounded explicit-host attachment validation for the Assistance facade.
//!
//! This module transports and validates host-origin metadata. It deliberately grants no
//! workspace authority and never interprets model-provided tool arguments as host identity.

/// Exposes the five bounded MCP tools, finite Application routing, and fail-open feedback state.
pub mod facade;

/// Projects validated peer replies into compact model text and unchanged structured results.
pub(crate) mod content;

/// Connects bounded daemon host correlation to closed peer-boundary outcomes.
pub mod assembly;

pub mod host_binding;

/// Serves the private helper claim/finish endpoint and runs the foreground helper itself.
pub mod claude_helper;

/// Correlates Claude foreground-helper tickets, exact launch recognition and one-use claims.
pub mod claude_worker;

/// Exposes the bounded fail-open native Codex and Claude hook command modes.
pub mod codex_hook;

/// Defines the confined project-problem source seam and the compact problems page text.
pub mod problems;

/// Loads bounded restart-only trusted target and executable/profile configuration.
pub mod launcher;

/// Defines closed byte-budgeted pending/detail/error and owner-result envelopes.
pub mod reply;

/// Runs one bounded daemon-owned job/detail worker with durable authority gates.
pub mod worker;

/// Replaceable nonblocking sink for privacy-safe edit and native-fallback facts.
pub mod telemetry;
