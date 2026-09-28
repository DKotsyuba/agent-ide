//! HTML for Agent IDE: element outlines computed from the text, cross-language name facts (class
//! uses, element ids), and the registration entry [`LANGUAGE`].
//!
//! Depends only on `agent-ide-core`; the application registers [`LANGUAGE`] at startup. HTML has
//! no language server and no confined project check: the symbol tools outline documents from
//! source ([`support::HtmlSupport`]) and the language bridge indexes their facts
//! ([`names::HtmlFacts`]).

use agent_ide_core::lang::{Language, LanguageDescriptor};

pub mod names;
mod scan;
pub mod support;

/// Registration descriptor of HTML.
pub static DESCRIPTOR: LanguageDescriptor = LanguageDescriptor {
    id: "html",
    display_name: "HTML",
    extensions: &["html", "htm"],
    card_manifest: None,
    home_tool_dirs: &[],
    support: &support::HtmlSupport,
    checks: None,
    server: None,
    names: Some(&names::HtmlFacts),
};

/// The HTML language handle.
pub const LANGUAGE: Language = Language::of(&DESCRIPTOR);
