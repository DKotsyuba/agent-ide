//! The TypeScript language (JavaScript files included) for Agent IDE: symbol support over
//! typescript-language-server document symbols, confined `tsc` project checks, the release-pinned
//! TypeScript server integration, cross-language name facts (class and element-id uses from JSX
//! and DOM queries), and the registration entry [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup.

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod backend;
pub mod checks;
pub mod module;
pub mod names;
pub mod profile;
pub mod support;

/// Registration descriptor of the TypeScript language.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "typescript",
    display_name: "TypeScript",
    extensions: &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"],
    card_manifest: Some("package.json"),
    home_tool_dirs: &[],
    support: &support::TypeScript,
    checks: Some(&checks::TypeScriptChecks),
    server: Some(&backend::TypeScriptServer),
    names: Some(&names::TsFacts),
};

/// The TypeScript language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
