//! Style sheets for Agent IDE: CSS, SCSS, Sass (indented) and LESS outlines computed from the
//! text, cross-language name facts (class names, element ids, custom properties), and the
//! registration entry [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup. Style
//! sheets have no language server and no confined project check: the symbol tools outline them
//! from source ([`support::CssSupport`]) and the language bridge indexes their facts
//! ([`names::CssFacts`]).

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod names;
mod scan;
pub mod support;

/// Registration descriptor of the style-sheet language.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "css",
    display_name: "CSS",
    extensions: &["css", "scss", "sass", "less"],
    card_manifest: None,
    home_tool_dirs: &[],
    support: &support::CssSupport,
    checks: None,
    server: None,
    names: Some(&names::CssFacts),
};

/// The style-sheet language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
