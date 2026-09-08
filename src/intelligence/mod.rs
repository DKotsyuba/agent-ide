//! Internal LSP safety boundaries used before semantic-view APIs are exposed.

/// Generation fencing, diagnostic readiness, and native-cache lifecycle facts for semantic views.
pub mod freshness;

/// Defines the exclusive rust-analyzer v0.1 profile and its view lifecycle.
pub mod rust;

/// Holds the non-product framing and callback primitives used by the later view adapter.
// The contract intentionally delays wiring this internal boundary into semantic views.
#[allow(dead_code)]
pub(crate) mod wire;

/// Provides the bounded shared `gopls` listener and isolated logical-view profile.
pub mod gopls;
