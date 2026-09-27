//! Read-only provider sessions, exact-source context, and semantic freshness boundaries.

/// Generation fencing, diagnostic readiness, and native-cache lifecycle facts for semantic views.
pub mod freshness;

/// Validates bounded framing before production sessions decode provider messages.
#[allow(dead_code)]
pub(crate) mod wire;

/// Exact-file and exact-symbol context with bounded lexical fallback.
pub mod context;
/// The language-server seam: static server descriptions, per-worker backends and their host.
pub mod server;
/// Production async-lsp sessions over borrowed Execution-owned protocol pipes.
pub mod session;
