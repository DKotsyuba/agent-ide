//! Agent IDE assembly: the language-independent core plus the bundled languages.
//!
//! The core crate ([`agent_ide_core`]) owns every language-independent component; this crate adds
//! the bundled language modules under the same paths the core modules use (`checks::rust`,
//! `intelligence::pyright`, `lang::go`, ...), registers the languages through [`languages`], and
//! builds the `agent-ide` binary. Every core module is re-exported unchanged.

pub use agent_ide_core::{
    app, assistance, changes, doctor_install, errorlog, execution, feed, init, project,
    selfinstall, telemetry, userhome, workspace,
};

/// Confined project checks: the core scheduling and snapshot types plus each bundled language's
/// checker.
pub mod checks {
    pub use agent_ide_core::checks::*;

    pub use agent_ide_lang_python::checks as python;
    pub use agent_ide_lang_rust::checks as rust;
    pub use agent_ide_lang_typescript::checks as typescript;
}

/// Provider sessions and semantic context: the core session machinery plus each bundled
/// language's server profile and worker backend.
pub mod intelligence {
    pub use agent_ide_core::intelligence::*;

    pub use agent_ide_lang_go::backend as gopls_backend;
    pub use agent_ide_lang_go::profile as gopls;
    pub use agent_ide_lang_python::backend as pyright_backend;
    pub use agent_ide_lang_python::profile as pyright;
    pub use agent_ide_lang_rust::backend as rust_backend;
    pub use agent_ide_lang_rust::profile as rust;
    pub use agent_ide_lang_typescript::backend as typescript_backend;
    pub use agent_ide_lang_typescript::profile as typescript;
}

/// Language identity and the symbol-tool contract from the core plus each bundled language's
/// support module.
pub mod lang {
    pub use agent_ide_core::lang::*;

    pub use agent_ide_lang_css::support as css;
    pub use agent_ide_lang_go::support as go;
    pub use agent_ide_lang_python::support as python;
    pub use agent_ide_lang_rust::support as rust;
    pub use agent_ide_lang_typescript::support as typescript;
}

/// The bundled languages and their one-time registration into [`lang::install`].
pub mod languages {
    use crate::lang::Language;

    /// The Rust language.
    pub const RUST: Language = agent_ide_lang_rust::LANGUAGE;
    /// The Python language.
    pub const PYTHON: Language = agent_ide_lang_python::LANGUAGE;
    /// The TypeScript language (JavaScript files included).
    pub const TYPESCRIPT: Language = agent_ide_lang_typescript::LANGUAGE;
    /// The Go language.
    pub const GO: Language = agent_ide_lang_go::LANGUAGE;
    /// Style sheets: CSS, SCSS, Sass and LESS (no server, no project check).
    pub const CSS: Language = agent_ide_lang_css::LANGUAGE;

    /// Every bundled language in the order replies list them.
    pub const ALL: [Language; 5] = [RUST, PYTHON, TYPESCRIPT, GO, CSS];

    /// Registers every bundled language for this process. Idempotent; call it before parsing a
    /// launcher configuration or mapping any path to a language.
    pub fn install() {
        crate::lang::install(&ALL);
    }
}

/// Launcher declaration tests that need the bundled languages registered.
#[cfg(test)]
mod launcher_tests;
