//! Language-independent core of the Agent IDE daemon.
//!
//! Everything here is keyed on registered languages (see [`lang::install`]); no module
//! names a language. Language crates implement [`lang::LanguageSupport`],
//! [`checks::LanguageChecks`], [`intelligence::server::LanguageServer`] and
//! [`lang::names::NameFacts`] and the application registers them at startup.

/// Owns the process-local daemon and private Unix IPC boundary.
pub mod app;

/// Validates and tracks trusted host invocations and their active bindings.
pub mod assistance;

/// Owns confined background project checks and the shared problem snapshot types.
pub mod checks;

/// Renders the bounded `<agent-ide>` problem block with per-actor dedup (EYES-r1 §6).
pub mod feed;

/// Admits and owns bounded local processes under supported host profiles.
pub mod execution;

/// Owns worktree identity, authority lifecycles, raw Git evidence, and source observations.
pub mod workspace;

/// Provides bounded internal LSP wire safety primitives for later semantic views.
pub mod intelligence;

/// Language identity, registration and the support contract for the symbol-addressed tools.
pub mod lang;

/// Builds and renders the language-independent project card `ide.start` shows.
pub mod project;

/// Composes bounded diff summaries and hunk payload for Changes v0.1.
pub mod changes;

/// Owns closed, bounded, local-only usage telemetry and its durable query/export surface.
pub mod telemetry;

/// Owns the append-only, bounded, closed-reason-code error log (T107).
pub mod errorlog;

/// Resolves the real per-user home from the password database instead of `$HOME`.
pub mod userhome;

/// Implements the `init` command: per-user home tree and launcher template creation.
pub mod init;

/// Implements the argument-less `doctor` command: read-only installation health findings.
pub mod doctor_install;
/// Installs one sealed release bundle into the immutable standalone layout (`self-install`).
pub mod selfinstall;

/// Enforces the crate boundary: the core names no language and depends on no language crate.
#[cfg(test)]
mod language_free {
    use std::path::{Path, PathBuf};

    /// Language and language-server names the core must never mention, assembled from pieces so
    /// this file does not trip its own scan. Matched case-insensitively as whole words, so
    /// hyphenated server names such as `<language>-analyzer` are caught by the language name.
    /// The web languages are listed ahead of their crates. `less` cannot be listed: it is an
    /// English word the core's prose uses.
    const NAMES: [&str; 14] = [
        concat!("ru", "st"),
        concat!("pyth", "on"),
        concat!("type", "script"),
        concat!("java", "script"),
        concat!("go", "lang"),
        concat!("py", "right"),
        concat!("ts", "server"),
        concat!("go", "pls"),
        concat!("ht", "ml"),
        concat!("c", "ss"),
        concat!("sc", "ss"),
        concat!("sa", "ss"),
        concat!("ts", "x"),
        concat!("js", "x"),
    ];

    /// The capitalized language name matched case-sensitively (the lowercase word is English).
    const CASED: &str = concat!("G", "o");

    /// Every `.rs` file below `dir`, recursively.
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable source directory") {
            let path = entry.expect("readable entry").path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }

    /// Whether `word` occurs in `text` delimited by non-identifier characters on both sides.
    fn has_word(text: &str, word: &str) -> bool {
        let identifier = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
        text.match_indices(word).any(|(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + word.len()..].chars().next();
            !before.is_some_and(identifier) && !after.is_some_and(identifier)
        })
    }

    /// No core source line names a language or a language's server or tool.
    #[test]
    fn core_sources_name_no_language() {
        let mut files = Vec::new();
        sources(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut offenders = Vec::new();
        for file in files {
            let text = std::fs::read_to_string(&file).expect("readable source");
            for (number, line) in text.lines().enumerate() {
                let lower = line.to_lowercase();
                if NAMES.iter().any(|name| has_word(&lower, name)) || has_word(line, CASED) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        file.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "the core names a language:\n{}",
            offenders.join("\n")
        );
    }

    /// The core manifest depends on no language crate, and no language crate depends on another.
    #[test]
    fn crate_dependencies_point_only_into_the_core() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the core lives in the workspace crates directory");
        let core = std::fs::read_to_string(crates.join("agent-ide-core/Cargo.toml")).unwrap();
        assert!(
            !core.contains("agent-ide-lang-"),
            "the core depends on a language crate"
        );
        for entry in std::fs::read_dir(crates).unwrap() {
            let dir = entry.unwrap().path();
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            if !name.starts_with("agent-ide-lang-") {
                continue;
            }
            let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
            for line in manifest.lines() {
                assert!(
                    !line.contains("agent-ide-lang-")
                        || line.trim() == format!("name = \"{name}\""),
                    "{name} depends on another language crate: {line}"
                );
            }
            assert!(
                manifest.contains("agent-ide-core"),
                "{name} must build on the core"
            );
        }
    }
}
