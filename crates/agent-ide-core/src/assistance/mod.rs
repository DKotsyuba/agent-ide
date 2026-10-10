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

/// Correlates exact Codex and Claude host identity from native hooks and MCP call metadata.
pub mod host_binding;

/// Carrier liveness evidence and the stale-writer takeover rule.
pub(crate) mod carriers;

/// Exposes the bounded fail-open native Codex and Claude hook command modes.
pub mod codex_hook;

/// Publishes, discovers and retires bounded private actor-addressed Codex rendezvous records.
#[cfg(unix)]
pub mod codex_rendezvous;

/// Defines the confined project-problem source seam and the compact problems page text.
pub mod problems;

/// Loads bounded restart-only trusted target and executable/profile configuration.
pub mod launcher;

/// Defines closed byte-budgeted pending/detail/error and owner-result envelopes.
pub mod reply;

/// Runs one bounded daemon-owned job/detail worker with durable authority gates.
pub mod worker;

/// Defines the per-session outbox of deferred facts and its producer/carrier seam (OUTBOX-r3).
pub mod outbox;

/// Stores each session's undelivered facts, change counters and agent state (OUTBOX-r3 §5-§9).
pub mod session_outbox;

/// Tracks environment resolution changes across worktree bindings.
mod environment;

/// Replaceable nonblocking sink for privacy-safe edit and native-fallback facts.
pub mod telemetry;

/// Owns explicitly requested background test runs for the daemon lifetime.
pub mod tests;

/// Renders the deterministic, byte-bounded terminal text of one settled test run.
pub(crate) mod test_fact;

/// Holds the call-signature block that ends every `ide.start` reply.
pub(crate) mod signature_card;

/// Guards that no reply, plate, fact, description or instruction invites polling.
#[cfg(test)]
mod hygiene;
