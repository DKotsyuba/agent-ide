//! Worktree-owned identity, authority, Git, and observation boundaries.

/// Worktree identity, activation, authority, and revocation state.
pub mod authority;

/// Raw Git comparison scope, baseline context, and NUL-safe porcelain parsing.
pub mod git;

/// Bounded source-byte observations and authorized native path reads.
pub mod observation;

/// Workspace schema admission and durable source-observation persistence.
pub mod store;
