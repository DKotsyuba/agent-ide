//! The `agent-ide init` command: one-shot creation of the per-user home tree and launcher
//! template.
//!
//! Creates `<home>` (the effective home's `.agent-ide`; see [`crate::userhome`]) with mode
//! `0700` when missing, and the launcher configuration (`~/.config/agent-ide/launcher.json` by
//! default) with mode `0600` when absent. Existing files are never modified and symlinked paths
//! are refused. The written template is the documented minimal version-one shape with the
//! measured platform `git`, so `agent-ide launcher check` accepts it immediately and a managed
//! MCP can later rebind its single target via `LauncherConfig::bind_one_candidate`.

use crate::assistance::launcher::AcceptedExecutable;
use crate::userhome;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

/// What one init run created or found.
#[derive(Debug)]
pub struct Outcome {
    /// The effective home directory, created when it was missing.
    pub home: PathBuf,
    /// The launcher configuration path, written when it was missing.
    pub config: PathBuf,
    /// Absolute paths newly created by this run, home first.
    pub created: Vec<PathBuf>,
}

impl Outcome {
    /// Renders the one-line init JSON report.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "home": self.home.to_string_lossy(),
            "config": self.config.to_string_lossy(),
            "created": self.created.iter().map(|path| path.to_string_lossy()).collect::<Vec<_>>(),
        })
        .to_string()
    }
}

/// Runs init: validates inputs, creates what is missing, and reports what it created.
///
/// `home` defaults to the effective home's `.agent-ide` and `config` to the effective home's
/// `.config/agent-ide/launcher.json`, so `AGENT_IDE_HOME` relocates both together. Refusal is a
/// one-line reason and leaves no partial file behind; directories already created stay.
pub fn run(
    home: Option<PathBuf>,
    config: Option<PathBuf>,
    allowed_roots: Vec<PathBuf>,
) -> Result<Outcome, String> {
    let effective =
        userhome::user_home().ok_or_else(|| "no home directory is available".to_owned())?;
    let home = home.unwrap_or_else(|| effective.join(".agent-ide"));
    let config =
        config.unwrap_or_else(|| effective.join(".config").join("agent-ide/launcher.json"));
    let roots = match allowed_roots {
        roots if roots.is_empty() => default_roots(&effective),
        roots => roots,
    };
    if let Some(root) = roots.iter().find(|root| !valid_root(root)) {
        return Err(format!(
            "allowed root {} is not an absolute normalized directory",
            root.display()
        ));
    }
    if fs::symlink_metadata(&home).is_ok_and(|metadata| !metadata.is_dir()) {
        return Err(format!("{} exists and is not a directory", home.display()));
    }
    if fs::symlink_metadata(&config).is_ok_and(|metadata| metadata.is_symlink()) {
        return Err(format!("{} is a symlink", config.display()));
    }
    let mut created = Vec::new();
    if fs::symlink_metadata(&home).is_err() {
        fs::DirBuilder::new()
            .mode(0o700)
            .recursive(true)
            .create(&home)
            .map_err(|error| format!("cannot create {}: {error}", home.display()))?;
        created.push(home.clone());
    }
    if fs::symlink_metadata(&config).is_err() {
        write_template(&config, &roots)?;
        created.push(config.clone());
    }
    Ok(Outcome {
        home,
        config,
        created,
    })
}

/// Writes the launcher template once with mode `0600`, creating its parent directories `0700`.
fn write_template(config: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let parent = config
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", config.display()))?;
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(config)
        .map_err(|error| format!("cannot create {}: {error}", config.display()))?;
    file.write_all(&template_bytes(roots))
        .map_err(|error| format!("cannot write {}: {error}", config.display()))
}

/// Chooses the default allowed roots: `~/projects` when it exists, else the home itself.
fn default_roots(effective: &Path) -> Vec<PathBuf> {
    let projects = effective.join("projects");
    if projects.is_dir() {
        vec![projects]
    } else {
        vec![effective.to_owned()]
    }
}

/// Applies the launcher's own allowed-root rule: absolute, normalized, no trailing separator.
fn valid_root(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().ends_with(b"/")
}

/// Builds the minimal valid version-one launcher template for `roots`.
///
/// The single target uses the documented placeholder attachment and the platform `git` measured
/// now, so the file passes `launcher check` immediately and stays rebindable to a real
/// candidate; without a measurable `/usr/bin/git` the template keeps zero targets and remains
/// valid.
fn template_bytes(roots: &[PathBuf]) -> Vec<u8> {
    let target = roots
        .first()
        .filter(|candidate| valid_root(candidate))
        .cloned()
        .zip(AcceptedExecutable::from_path(PathBuf::from("/usr/bin/git"), "git").ok())
        .map(|(candidate, git)| {
            serde_json::json!({
                "attachment": "opaque-launcher-channel",
                "candidate": candidate.to_string_lossy(),
                "git": {
                    "path": git.path.to_string_lossy(),
                    "identity": git.identity,
                    "blake3": git.blake3,
                },
                "providers": [],
            })
        });
    serde_json::json!({
        "version": 1,
        "limits": {"queued": 16, "details": 64, "operation_ms": 120_000, "output_bytes": 65_536},
        "targets": target.map(|target| vec![target]).unwrap_or_default(),
        "allowed_roots": roots.iter().map(|root| root.to_string_lossy()).collect::<Vec<_>>(),
    })
    .to_string()
    .into_bytes()
}
