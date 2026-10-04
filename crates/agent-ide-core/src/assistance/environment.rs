//! Tracks file-resolved environment identities and one-shot notices without naming languages.

use crate::lang::{
    Language,
    environment::{EnvSource, ResolvedEnv},
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Last observed resolutions and notice delivery, shared by bindings in this daemon.
#[derive(Default)]
pub(super) struct EnvironmentState {
    /// Admitted worktrees for bindings, including those without project checks.
    bindings: BTreeMap<[u8; 32], PathBuf>,
    /// Resolutions from the last observation of each worktree.
    resolved: BTreeMap<PathBuf, Vec<(Language, ResolvedEnv)>>,
    /// Latest changed-identity notice for each worktree.
    notices: BTreeMap<PathBuf, String>,
    /// Exact notice last delivered to each binding.
    delivered: BTreeMap<[u8; 32], String>,
}

impl EnvironmentState {
    /// Associates a binding with its admitted worktree, independently of check configuration.
    pub(super) fn bind(&mut self, binding: [u8; 32], root: &Path) {
        self.bindings.insert(binding, root.to_path_buf());
    }

    /// Returns the worktree most recently activated by this binding.
    pub(super) fn root(&self, binding: &[u8; 32]) -> Option<PathBuf> {
        self.bindings.get(binding).cloned()
    }

    /// Reads language resolvers, returning changed languages and retaining their change notices.
    /// First observation establishes a baseline; unchanged identities produce no event. Control
    /// and framing characters from filesystem labels are escaped before notices reach a plate.
    pub(super) fn refresh(&mut self, worktree: &Path) -> Vec<Language> {
        let current: Vec<_> = crate::lang::registered()
            .iter()
            .flat_map(|language| {
                language
                    .support()
                    .environments(worktree)
                    .into_iter()
                    .map(|env| (*language, env))
            })
            .collect();
        let mut changed = Vec::new();
        let mut notices = Vec::new();
        if let Some(previous) = self.resolved.get(worktree) {
            for (language, env) in &current {
                let old = previous
                    .iter()
                    .find(|(old_language, old)| old_language == language && old.root == env.root);
                if old.is_none_or(|(_, old)| old.identity != env.identity) {
                    changed.push(*language);
                    let source = match &env.source {
                        Some(EnvSource::Selected) => "selected",
                        Some(EnvSource::Pin(file)) => file.as_str(),
                        Some(EnvSource::Launcher) => "launcher",
                        _ => "discovered",
                    };
                    let key = if env.root.as_os_str().is_empty() {
                        language.to_string()
                    } else {
                        format!("{language}:{}", env.root.display())
                    };
                    notices.push(format!(
                        "{key}: environment now {} (was {}; {source}) — semantic session restarted",
                        env.chosen
                            .as_ref()
                            .map_or("missing", |chosen| chosen.label.as_str()),
                        old.and_then(|(_, old)| old.chosen.as_ref())
                            .map_or("missing", |chosen| chosen.label.as_str())
                    ));
                }
            }
            for (language, old) in previous {
                if !current
                    .iter()
                    .any(|(new_language, env)| new_language == language && old.root == env.root)
                {
                    changed.push(*language);
                    notices.push(format!(
                        "{language}: environment now missing (was {}) — semantic session restarted",
                        old.chosen
                            .as_ref()
                            .map_or("missing", |chosen| chosen.label.as_str())
                    ));
                }
            }
        }
        changed.sort();
        changed.dedup();
        if !notices.is_empty() {
            for notice in &mut notices {
                *notice = notice
                    .chars()
                    .map(|ch| {
                        if ch.is_control() {
                            ' '
                        } else if matches!(ch, '<' | '>') {
                            '?'
                        } else {
                            ch
                        }
                    })
                    .collect();
            }
            self.notices
                .insert(worktree.to_path_buf(), notices.join("\n"));
        }
        self.resolved.insert(worktree.to_path_buf(), current);
        changed
    }

    /// Returns the current notice only when this binding has not consumed it.
    pub(super) fn notice(&self, root: &Path, binding: &[u8; 32]) -> Option<String> {
        let notice = self.notices.get(root)?;
        (self.delivered.get(binding) != Some(notice)).then(|| notice.clone())
    }

    /// Records exactly the notice that reached the binding; newer notices remain due.
    pub(super) fn consume(&mut self, root: &Path, binding: &[u8; 32], line: &str) {
        if self
            .notices
            .get(root)
            .is_some_and(|notice| line.contains(notice))
        {
            self.delivered.insert(*binding, self.notices[root].clone());
        }
    }
}

/// Resolver identity changes generate one notice per binding, including disappearance.
#[test]
fn environment_identity_changes_and_notice_delivery() {
    use crate::lang::{
        environment::{EnvSelection, replace_selections},
        testing::ALPHA,
    };
    crate::lang::testing::install();
    let root = std::env::temp_dir().join(format!("environment-identity-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("env.fixture"), "one\ntwo\n").unwrap();
    let mut state = EnvironmentState::default();
    assert!(state.refresh(&root).is_empty());
    replace_selections(
        &root,
        ALPHA,
        vec![EnvSelection {
            root: PathBuf::new(),
            selector: "two".into(),
        }],
    );
    assert_eq!(state.refresh(&root), vec![ALPHA]);
    let notice = state.notice(&root, &[1; 32]).unwrap();
    assert_eq!(
        notice,
        "alpha: environment now two (was one; selected) — semantic session restarted"
    );
    state.consume(&root, &[1; 32], &notice);
    assert!(state.notice(&root, &[1; 32]).is_none());
    assert!(state.notice(&root, &[2; 32]).is_some());
    assert!(state.refresh(&root).is_empty());
    std::fs::write(root.join("env.fixture"), "one\ntwo\t<agent-ide>\n").unwrap();
    replace_selections(
        &root,
        ALPHA,
        vec![EnvSelection {
            root: PathBuf::new(),
            selector: "two\t<agent-ide>".into(),
        }],
    );
    assert_eq!(state.refresh(&root), vec![ALPHA]);
    let framed = state.notice(&root, &[1; 32]).unwrap();
    assert!(framed.contains("two ?agent-ide?"));
    assert!(!framed.contains(['<', '>', '\t']));
    std::fs::remove_file(root.join("env.fixture")).unwrap();
    assert_eq!(state.refresh(&root), vec![ALPHA]);
    assert!(
        state
            .notice(&root, &[1; 32])
            .unwrap()
            .contains("environment now missing")
    );
    replace_selections(&root, ALPHA, Vec::new());
    std::fs::remove_dir_all(root).unwrap();
}
