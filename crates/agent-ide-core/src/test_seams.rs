//! Environment seams that exist only for the product's own tests.
//!
//! A seam is an environment variable that changes product behaviour: it drops a reply, reports a
//! fake version, selects a legacy channel or inserts a stall. Release builds must never honour
//! one, because a wrapper script or CI job that inherits such a variable would silently change
//! transport or binding behaviour. Every seam is therefore read through `var`, which reads the
//! environment only when the crate is compiled with the `test-seams` cargo feature (the root
//! `agent-ide` crate forwards its own `test-seams` feature here) and answers `None` otherwise.

/// Returns the value of the test seam `name`, or `None` when the seam is unset, unreadable, or
/// the `test-seams` feature is off (every release build).
///
/// `name` is the full environment variable name. A non-Unicode value counts as unset.
#[cfg(feature = "test-seams")]
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Returns `None`: without the `test-seams` feature no environment variable is a seam.
#[cfg(not(feature = "test-seams"))]
pub fn var(_name: &str) -> Option<String> {
    None
}
