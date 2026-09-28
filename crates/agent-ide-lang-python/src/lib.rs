//! The Python language for Agent IDE: symbol support over Pyright document symbols, confined
//! Pyright project checks, the exclusive Pyright server integration, and the registration entry
//! [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup.

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod backend;
pub mod checks;
pub mod profile;
pub mod support;

/// Registration descriptor of the Python language.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "python",
    display_name: "Python",
    extensions: &["py", "pyi"],
    card_manifest: None,
    home_tool_dirs: &[".local/bin"],
    support: &support::Python,
    checks: Some(&checks::PythonChecks),
    server: Some(&backend::PyrightServer),
    names: None,
};

/// The Python language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
