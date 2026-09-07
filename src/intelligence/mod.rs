//! Internal LSP safety boundaries used before semantic-view APIs are exposed.

/// Holds the non-product framing and callback primitives used by the later view adapter.
// The contract intentionally delays wiring this internal boundary into semantic views.
#[allow(dead_code)]
pub(crate) mod wire;
