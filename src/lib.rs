//! Agent IDE assembly: the language-independent core plus the bundled languages.
//!
//! The core crate ([`agent_ide_core`]) owns every language-independent component; this crate adds
//! the bundled language modules under the same paths the core modules use (`checks::rust`,
//! `intelligence::pyright`, `lang::python`, ...), registers the languages through [`languages`], and
//! builds the `agent-ide` binary. Every core module is re-exported unchanged.

pub use agent_ide_core::{
    app, assistance, changes, doctor_install, errorlog, execution, feed, hook_hints, init, project,
    retention, selfinstall, telemetry, test_seams, userhome, workspace,
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
    pub use agent_ide_lang_html::support as html;
    pub use agent_ide_lang_python::support as python;
    pub use agent_ide_lang_rust::support as rust;
    pub use agent_ide_lang_typescript::support as typescript;
}

/// The bundled languages and their one-time registration into [`lang::install`].
pub mod languages {
    use crate::assistance::launcher::RetiredSettings;
    use crate::lang::Language;

    /// The Rust language.
    pub const RUST: Language = agent_ide_lang_rust::LANGUAGE;
    /// The Python language.
    pub const PYTHON: Language = agent_ide_lang_python::LANGUAGE;
    /// The TypeScript language (JavaScript files included).
    pub const TYPESCRIPT: Language = agent_ide_lang_typescript::LANGUAGE;
    /// Style sheets: CSS, SCSS, Sass and LESS (no server, no project check).
    pub const CSS: Language = agent_ide_lang_css::LANGUAGE;
    /// HTML documents (no server, no project check).
    pub const HTML: Language = agent_ide_lang_html::LANGUAGE;

    /// Every bundled language in the order replies list them.
    pub const ALL: [Language; 5] = [RUST, PYTHON, TYPESCRIPT, CSS, HTML];

    /// Provider settings of removed servers: an old launcher configuration that still declares one
    /// keeps starting, the entry is ignored with a journal line and a doctor sentence.
    pub const RETIRED_SETTINGS: [RetiredSettings; 1] = [RetiredSettings {
        key: "gopls_defaults",
        notice: "Go support was removed in 0.10.8; the gopls provider entry is ignored",
    }];

    /// Languages whose bundled module ships default-on in this release; the others compute in
    /// process. `AGENT_IDE_LANGUAGE_MODE=<id>=in_process` sends a shipped one back.
    pub const SHIPPED_MODULES: [&str; 1] = ["python"];

    /// Each language's effect recipes: the only processes its module may ask the core to run.
    pub static RECIPES: [(&str, &[agent_ide_core::modules::payload::EffectRecipe]); 1] =
        [("python", agent_ide_lang_python::module::RECIPES)];

    /// Each language's module environment: the only variables its cleared module process (and
    /// `hello.config.env`) receives from the daemon.
    pub static MODULE_ENV: [(&str, &[&str]); 1] = [("rust", &["AGENT_IDE_RUST_TOOLCHAIN_DIR"])];

    /// Each language's static install roots its interactive recipes may name (Python's standard
    /// interpreter prefixes).
    pub static INSTALL_ROOTS: [(&str, &[&str]); 1] = [(
        "python",
        &agent_ide_lang_python::module::INTERPRETER_PREFIXES,
    )];

    /// Each language's host-side resolution of a worktree's accepted environments (Python's
    /// selected, pinned or discovered environments and their installation prefixes).
    pub static ENVIRONMENT_ROOTS: [(&str, agent_ide_core::modules::recipe::EnvironmentRoots); 1] =
        [("python", agent_ide_lang_python::environment::accepted_roots)];

    /// Registers every bundled language for this process. Idempotent; call it before parsing a
    /// launcher configuration or mapping any path to a language.
    pub fn install() {
        crate::lang::install(&ALL);
        crate::assistance::launcher::install_retired_settings(&RETIRED_SETTINGS);
        agent_ide_core::modules::router::ship(&SHIPPED_MODULES);
        agent_ide_core::modules::recipe::declare(&RECIPES);
        agent_ide_core::modules::launch::declare_env(&MODULE_ENV);
        agent_ide_core::modules::recipe::declare_roots(&INSTALL_ROOTS);
        agent_ide_core::modules::recipe::declare_environment_roots(&ENVIRONMENT_ROOTS);
    }
}

/// Launcher declaration tests that need the bundled languages registered.
#[cfg(test)]
mod launcher_tests;

/// Start-card tests that need the bundled languages registered.
#[cfg(test)]
mod start_card_tests {
    use agent_ide_core::project::{ServerState, collect, not_started_state, render};

    /// A mixed rust+python card's `servers:` line renders each language's own not-started
    /// sentence: only a language whose outline stays exact from source claims outline, read and
    /// edit answer now; the others just say when they start.
    #[test]
    fn not_started_server_states_are_per_language() {
        crate::languages::install();
        let root =
            std::env::temp_dir().join(format!("agent-ide-card-{}-rust-python", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(root.join("pyproject.toml"), "[project]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("main.py"), "print(1)\n").unwrap();
        let languages: Vec<_> = [crate::languages::RUST, crate::languages::PYTHON]
            .into_iter()
            .filter_map(|language| language.support().detect(&root))
            .collect();
        assert_eq!(languages.len(), 2, "rust and python both detect");
        let servers: Vec<_> = languages
            .iter()
            .map(|project| ServerState {
                language: project.language,
                state: not_started_state(project.language).to_owned(),
            })
            .collect();
        let card = collect(&root, languages, servers, None);
        let rendered = render(&card);
        let servers_line = rendered
            .lines()
            .find(|line| line.starts_with("servers: "))
            .expect("the card renders a servers line");
        assert_eq!(
            servers_line,
            "servers: rust not started; ide.outline, ide.read and ide.edit answer from source \
             now; ide.symbol and ide.graph wait for the server, which starts on their first \
             use · python not started; starts on first use"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }
}
