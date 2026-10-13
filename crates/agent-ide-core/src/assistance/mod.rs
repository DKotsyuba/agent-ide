//! Bounded explicit-host attachment validation for the Assistance facade.
//!
//! This module transports and validates host-origin metadata. It deliberately grants no
//! workspace authority and never interprets model-provided tool arguments as host identity.

/// Exposes the five bounded MCP tools, finite Application routing, and fail-open feedback state.
pub mod facade;

/// Projects validated peer replies into compact model text and unchanged structured results.
pub(crate) mod content;

/// Batch request forms (batch-api-v1): caps, item parsing, summary lines and follow-up calls.
pub(crate) mod batch;

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

/// Connects optional protected Bridge routing without changing local authority.
pub mod bridge;

/// Defines closed byte-budgeted pending/detail/error and owner-result envelopes.
pub mod reply;

/// Runs one bounded daemon-owned job/detail worker with durable authority gates.
pub mod worker;

/// Defines the per-session outbox of deferred facts and its producer/carrier seam (OUTBOX-r3).
pub mod outbox;

/// Wakes an idle agent for its own requested results behind one narrow interface (OUTBOX-r3 §8.4).
pub mod wake;

/// Stores each session's undelivered facts, change counters and agent state (OUTBOX-r3 §5-§9).
pub mod session_outbox;

/// Remembers, in the MCP front, requested work it relayed unresolved, for restarts (OUTBOX-r3 §10).
pub mod unresolved;

/// Tracks environment resolution changes across worktree bindings.
mod environment;

/// Replaceable nonblocking sink for privacy-safe edit and native-fallback facts.
pub mod telemetry;

/// Owns explicitly requested background test runs for the daemon lifetime.
pub mod tests;

/// Renders the deterministic, byte-bounded terminal text of one settled test run.
pub(crate) mod test_fact;

/// Parses Codex `apply_patch` envelopes and applies their hunks to source text, purely.
pub(crate) mod patch;

/// Rewrites the address-shaped single `ide.edit` forms into the one-file `changes` form.
mod edit_forms;

/// Holds the call-signature block that ends every `ide.start` reply.
pub(crate) mod signature_card;

/// Guards that no reply, plate, fact, description or instruction invites polling.
#[cfg(test)]
mod hygiene;
