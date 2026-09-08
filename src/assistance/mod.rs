//! Bounded host-attachment validation for the Assistance facade.
//!
//! This module transports and validates host-origin metadata. It deliberately grants no
//! workspace authority and never interprets model-provided tool arguments as host identity.

/// Exposes the five bounded MCP tools, finite Application routing, and fail-open feedback state.
pub mod facade;

pub mod host_binding;
