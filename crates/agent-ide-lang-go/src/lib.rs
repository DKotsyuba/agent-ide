//! The Go language for Agent IDE: symbol support over gopls document symbols, the shared-listener
//! gopls server integration, and the registration entry [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup. Go has
//! no confined project check.

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod backend;
pub mod profile;
pub mod support;

/// Registration descriptor of the Go language.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "go",
    display_name: "Go",
    extensions: &["go"],
    card_manifest: None,
    home_tool_dirs: &[],
    support: &support::GoSupport,
    checks: None,
    server: Some(&backend::GoplsServer),
    names: None,
};

/// The Go language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
