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

    pub mod python;
    pub mod rust;
    pub mod typescript;
}

/// Provider sessions and semantic context: the core session machinery plus each bundled
/// language's server profile and worker backend.
pub mod intelligence {
    pub use agent_ide_core::intelligence::*;

    pub mod gopls;
    pub mod gopls_backend;
    pub mod pyright;
    pub mod pyright_backend;
    pub mod rust;
    pub mod rust_backend;
    pub mod typescript;
    pub mod typescript_backend;
}

/// Language identity and the symbol-tool contract from the core plus each bundled language's
/// support module.
pub mod lang {
    pub use agent_ide_core::lang::*;

    pub mod go;
    pub mod python;
    pub mod rust;
    pub mod typescript;
}

/// The bundled languages and their one-time registration into [`lang::install`].
pub mod languages {
    use crate::lang::Language;

    /// The Rust language.
    pub const RUST: Language = crate::lang::rust::LANGUAGE;
    /// The Python language.
    pub const PYTHON: Language = crate::lang::python::LANGUAGE;
    /// The TypeScript language (JavaScript files included).
    pub const TYPESCRIPT: Language = crate::lang::typescript::LANGUAGE;
    /// The Go language.
    pub const GO: Language = crate::lang::go::LANGUAGE;

    /// Every bundled language in the order replies list them.
    pub const ALL: [Language; 4] = [RUST, PYTHON, TYPESCRIPT, GO];

    /// Registers every bundled language for this process. Idempotent; call it before parsing a
    /// launcher configuration or mapping any path to a language.
    pub fn install() {
        crate::lang::install(&ALL);
    }
}

/// Launcher declaration tests that need the bundled languages registered.
#[cfg(test)]
mod launcher_tests;
