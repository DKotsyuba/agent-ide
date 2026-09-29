//! The Rust language for Agent IDE: symbol support over rust-analyzer document symbols, confined
//! `cargo check` project checks, the exclusive rust-analyzer server integration, and the
//! registration entry [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup.

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod backend;
pub mod checks;
mod home;
mod module_graph;
pub mod profile;
pub mod support;

/// Registration descriptor of the Rust language.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "rust",
    display_name: "Rust",
    extensions: &["rs"],
    card_manifest: Some("Cargo.toml"),
    home_tool_dirs: &[".cargo/bin"],
    support: &support::RustSupport,
    checks: Some(&checks::RustChecks),
    server: Some(&backend::RustServer),
    names: None,
};

/// The Rust language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
