//! Worktree-owned identity, authority, Git, and observation boundaries.

/// Worktree identity, activation, authority, and revocation state.
pub mod authority;

/// Raw Git comparison scope, baseline context, and NUL-safe porcelain parsing.
pub mod git;
