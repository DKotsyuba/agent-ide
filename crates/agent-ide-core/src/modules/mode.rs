//! The one-release fallback switch `AGENT_IDE_LANGUAGE_MODE=<id>=in_process[,<id>=in_process…]`,
//! read once at daemon start. Unset means module mode for every language whose module shipped;
//! an unknown id or value is ignored and reported once as `language_mode_ignored:<entry>`.

use std::collections::BTreeSet;

/// The environment variable.
pub const ENV: &str = "AGENT_IDE_LANGUAGE_MODE";

/// Where a language's computations run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// In the language's bundled module process.
    Module,
    /// In the daemon, through the unchanged language crate (the fallback).
    InProcess,
}

/// The parsed switch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LanguageModes {
    /// Languages switched back in process.
    in_process: BTreeSet<String>,
    /// Entries that were ignored, as written.
    ignored: Vec<String>,
}

impl LanguageModes {
    /// Parses `value` against the registered language ids `known`.
    pub fn parse(value: Option<&str>, known: &[&str]) -> Self {
        let mut modes = Self::default();
        for entry in value.unwrap_or_default().split(',') {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            match entry.split_once('=') {
                Some((id, "in_process")) if known.contains(&id) => {
                    modes.in_process.insert(id.to_owned());
                }
                _ => modes.ignored.push(entry.to_owned()),
            }
        }
        modes
    }

    /// Reads [`ENV`] for the registered languages.
    pub fn from_env() -> Self {
        let known: Vec<&str> = crate::lang::registered()
            .iter()
            .map(|language| language.name())
            .collect();
        Self::parse(std::env::var(ENV).ok().as_deref(), &known)
    }

    /// The mode the switch asks for `language` (a language whose module has not shipped stays in
    /// process whatever this says; that decision belongs to the host runtime).
    pub fn mode(&self, language: &str) -> Mode {
        if self.in_process.contains(language) {
            Mode::InProcess
        } else {
            Mode::Module
        }
    }

    /// One `language_mode_ignored:<id>` journal line per ignored entry. Only the id part is kept,
    /// and only when it is a short lowercase identifier; anything else (control characters, a
    /// secret pasted by mistake) is reported as `?`. Values are never echoed.
    pub fn ignored_lines(&self) -> Vec<String> {
        self.ignored
            .iter()
            .map(|entry| {
                let id = entry.split_once('=').map_or(entry.as_str(), |(id, _)| id);
                let plain = !id.is_empty()
                    && id.len() <= 32
                    && id
                        .bytes()
                        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
                format!("language_mode_ignored:{}", if plain { id } else { "?" })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Known ids switch in process; unset, empty, unknown ids and unknown values do not, and each
    /// ignored entry is reported once.
    #[test]
    fn parses_the_switch() {
        let known = ["alpha", "beta"];
        let unset = LanguageModes::parse(None, &known);
        assert_eq!(unset.mode("alpha"), Mode::Module);
        assert!(unset.ignored_lines().is_empty());
        let modes = LanguageModes::parse(
            Some("alpha=in_process, ,gamma=in_process,beta=module,beta,beta=tok\nen,Se cret=x"),
            &known,
        );
        assert_eq!(modes.mode("alpha"), Mode::InProcess);
        assert_eq!(modes.mode("beta"), Mode::Module);
        let lines = modes.ignored_lines();
        assert_eq!(
            lines,
            [
                "language_mode_ignored:gamma",
                "language_mode_ignored:beta",
                "language_mode_ignored:beta",
                "language_mode_ignored:beta",
                "language_mode_ignored:?",
            ]
        );
        assert!(
            lines
                .iter()
                .all(|line| !line.contains("tok") && !line.contains('\n')),
            "values never reach the journal"
        );
    }
}
