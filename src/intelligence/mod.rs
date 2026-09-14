//! Read-only provider sessions, exact-source context, and semantic freshness boundaries.

/// Generation fencing, diagnostic readiness, and native-cache lifecycle facts for semantic views.
pub mod freshness;

/// Defines the exclusive rust-analyzer v0.1 profile and its view lifecycle.
pub mod rust;

/// Defines the exclusive Pyright v0.1.1 profile and its owned stdio lifecycle.
pub mod pyright;

/// Defines the exclusive release-pinned TypeScript profile and normal/abnormal stdio lifecycle.
pub mod typescript;

/// Validates bounded framing before production sessions decode provider messages.
#[allow(dead_code)]
pub(crate) mod wire;

/// Provides the bounded shared `gopls` listener and isolated logical-view profile.
pub mod gopls;

/// Exact-file and exact-symbol context with bounded lexical fallback.
pub mod context;
/// Production async-lsp sessions over borrowed Execution-owned protocol pipes.
pub mod session;
