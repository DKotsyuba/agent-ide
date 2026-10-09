//! The single Python environment resolver (environment-selection design §5.1).
//!
//! Every consumer — the project card, the project check, the Pyright session, test and format
//! commands and the syntax probe — asks [`resolve`] (directly or through `environments`), so
//! they can never disagree about which interpreter a project root uses. Precedence per root:
//!
//! 1. a Pyright config pin (`pyrightconfig.json` when it exists, else `[tool.pyright]`
//!    `venvPath`+`venv`); a pinned environment that is missing or broken resolves to nothing with
//!    a detail, never to a fallback;
//! 2. the agent's stored selection ([`agent_ide_core::lang::environment::selections`]), with the
//!    same no-fallback rule;
//! 3. discovery: the first runnable of `.venv`, `venv`, then suffixed `.venv*`/`venv*` sorted by
//!    name, beside the root, else (for a nested root) beside the worktree.
//!
//! `.python-version` is a version request only: a mismatch with the chosen environment becomes a
//! warning and never switches it. Only files are read, under the host's read denies; no
//! environment manager ever runs.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};

use agent_ide_core::execution::seatbelt::ReadDeny;
use agent_ide_core::lang::environment::{CommandEnv, EnvCandidate, EnvSource, ResolvedEnv};

use crate::checks::{
    PYPROJECT_FILE, PYRIGHT_CONFIG_FILE, allowed_file, read_config, read_pyproject_venv_keys,
    read_pyrightconfig_venv_keys,
};
use crate::support::{ROOT_MARKER_FILES, is_python_project, python_roots, venv_directories};

/// A Pyright config pin in force for one project root.
struct Pin {
    /// The file that sets it: `pyrightconfig.json` or `pyproject.toml`.
    file: &'static str,
    /// The `venv` key as written.
    venv: String,
    /// The environment directory `venvPath`/`venv` name.
    dir: PathBuf,
}

/// Reads the Pyright pin of `root`. Pyright reads `[tool.pyright]` only when no
/// `pyrightconfig.json` exists, so an existing JSON config without the two keys pins nothing.
fn pin(root: &Path, denies: &[ReadDeny]) -> Option<Pin> {
    let (file, (venv_path, venv)) = if fs::symlink_metadata(root.join(PYRIGHT_CONFIG_FILE)).is_ok()
    {
        (
            PYRIGHT_CONFIG_FILE,
            read_pyrightconfig_venv_keys(root, denies)?,
        )
    } else {
        (PYPROJECT_FILE, read_pyproject_venv_keys(root, denies)?)
    };
    // A relative `venvPath` is relative to the config file's directory, as Pyright reads it.
    let dir = root.join(venv_path).join(&venv);
    Some(Pin { file, venv, dir })
}

/// The `bin/python` interpreter of an environment directory.
fn python_of(dir: &Path) -> PathBuf {
    dir.join("bin").join("python")
}

/// The interpreter of a resolved environment, when one is chosen.
pub fn interpreter(env: &ResolvedEnv) -> Option<PathBuf> {
    env.chosen.as_ref().map(|chosen| python_of(&chosen.path))
}

/// Whether the environment `dir` has an interpreter: `Some(true)` when `bin/python` is a readable
/// file, `Some(false)` when it is a dangling link (its base interpreter is gone), `None` when it
/// is absent or hidden by host read denies.
fn interpreter_state(dir: &Path, denies: &[ReadDeny]) -> Option<bool> {
    let python = python_of(dir);
    if allowed_file(&python, denies) {
        Some(true)
    } else if denies.is_empty()
        && fs::symlink_metadata(&python).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        Some(false)
    } else {
        None
    }
}

/// `X.Y.Z` from a `pyvenv.cfg` version value: CPython writes `3.12.7`, uv `3.14.0`, virtualenv
/// `3.12.7.final.0`; anything that does not start with a number is no version.
fn release(value: &str) -> Option<String> {
    let parts: Vec<&str> = value
        .split('.')
        .take(3)
        .take_while(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("."))
}

/// What the card shows for an environment directory: relative to the root, else relative to the
/// worktree, else the absolute path.
fn label(worktree: &Path, root: &Path, dir: &Path) -> String {
    dir.strip_prefix(root)
        .or_else(|_| dir.strip_prefix(worktree))
        .unwrap_or(dir)
        .display()
        .to_string()
}

/// Describes one environment directory from its `pyvenv.cfg` (PEP 405), read under the host's
/// denies: the version from `version`, else `version_info`, else none; broken when its
/// interpreter is a dangling link (`runnable` false) or the `home` directory is gone.
fn candidate(
    worktree: &Path,
    root: &Path,
    dir: &Path,
    runnable: bool,
    denies: &[ReadDeny],
) -> EnvCandidate {
    let config = read_config(&dir.join("pyvenv.cfg"), denies).unwrap_or_default();
    let value = |key: &str| {
        config.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == key).then(|| value.trim().to_owned())
        })
    };
    EnvCandidate {
        label: label(worktree, root, dir),
        path: std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()),
        version: value("version")
            .or_else(|| value("version_info"))
            .and_then(|version| release(&version)),
        broken: !runnable || value("home").is_some_and(|home| !Path::new(&home).is_dir()),
    }
}

/// Discovered environments of `root` in discovery order, broken ones included; a nested root
/// without its own falls back to the worktree's, which covers every package beneath it.
fn discovered(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Vec<EnvCandidate> {
    let mut dirs = venv_directories(root);
    if dirs.is_empty() && root != worktree {
        dirs = venv_directories(worktree);
    }
    dirs.iter()
        .filter_map(|dir| {
            interpreter_state(dir, denies)
                .map(|runnable| candidate(worktree, root, dir, runnable, denies))
        })
        .collect()
}

/// The directory `selector` names for `root`: a candidate label, else a path (relative to the
/// root, or absolute).
fn selected_dir(root: &Path, selector: &str, candidates: &[EnvCandidate]) -> PathBuf {
    candidates
        .iter()
        .find(|candidate| candidate.label == selector)
        .map_or_else(|| root.join(selector), |candidate| candidate.path.clone())
}

/// The `ide.start environment` key that addresses `root`: `python`, or `python:<root>` for a
/// nested root.
fn selector_key(worktree: &Path, root: &Path) -> String {
    match root.strip_prefix(worktree) {
        Ok(relative) if !relative.as_os_str().is_empty() => {
            format!("python:{}", relative.display())
        }
        _ => "python".to_owned(),
    }
}

/// The candidates as the card lists them: `.venv, .venv-py314`, or `none`.
fn labels(candidates: &[EnvCandidate]) -> String {
    if candidates.is_empty() {
        return "none".to_owned();
    }
    candidates
        .iter()
        .map(|candidate| candidate.label.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Cause and next step when discovery finds nothing runnable for `root`.
fn nothing_found(worktree: &Path, root: &Path, key: &str, candidates: &[EnvCandidate]) -> String {
    let manifest = ROOT_MARKER_FILES
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
        .unwrap_or_else(|| root.to_path_buf());
    let beside = label(worktree, worktree, &manifest);
    let beside = if beside.is_empty() {
        "the worktree root".to_owned()
    } else {
        beside
    };
    let cause = if candidates.is_empty() {
        format!("no .venv* beside {beside}")
    } else {
        format!(
            "every environment beside {beside} is broken (base interpreter gone): {}",
            labels(candidates)
        )
    };
    format!("{cause}; create one (uv venv) or ide.start environment {{\"{key}\": \"<path>\"}}")
}

/// The version `.python-version` requests for `root` (its own, else the worktree's), read under
/// the host's denies: a denied file is unreadable and says nothing.
fn version_request(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Option<String> {
    [root, worktree].iter().find_map(|dir| {
        read_config(&dir.join(".python-version"), denies)?
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    })
}

/// Lists `candidate` first among `candidates` unless discovery already found it.
fn take(candidate: &EnvCandidate, candidates: &mut Vec<EnvCandidate>) {
    if !candidates.iter().any(|known| known.path == candidate.path) {
        candidates.insert(0, candidate.clone());
    }
}

/// What one root resolves to, as `(chosen, source, missing)`.
type Outcome = (Option<EnvCandidate>, Option<EnvSource>, Option<String>);

/// Settles an environment a selection or pin demands: chosen when it runs, else missing with
/// `absent` (no interpreter at all) or a broken-interpreter cause followed by `next`. An existing
/// one is listed among the candidates either way.
fn settle(
    (worktree, root, dir): (&Path, &Path, &Path),
    source: EnvSource,
    absent: String,
    next: &str,
    candidates: &mut Vec<EnvCandidate>,
    denies: &[ReadDeny],
) -> Outcome {
    let Some(runnable) = interpreter_state(dir, denies) else {
        return (None, None, Some(absent));
    };
    let found = candidate(worktree, root, dir, runnable, denies);
    take(&found, candidates);
    if found.broken {
        let cause = format!(
            "environment {} is broken (base interpreter gone) — {next}",
            found.label
        );
        (None, None, Some(cause))
    } else {
        (Some(found), Some(source), None)
    }
}

/// `ino.mtime` of `path` without following a final link, empty when it cannot be read.
fn stamp(path: &Path) -> String {
    fs::symlink_metadata(path)
        .map(|metadata| {
            format!(
                "{}.{}.{}",
                metadata.ino(),
                metadata.mtime(),
                metadata.mtime_nsec()
            )
        })
        .unwrap_or_default()
}

/// Opaque identity of a resolution: the chosen environment's path and version plus everything
/// that changes when it is recreated in place (directory and interpreter inodes, the interpreter
/// link's target, `pyvenv.cfg` stamp and content), or the missing cause. Why it won (`source`)
/// is left out: selecting the current winner changes nothing a consumer depends on.
fn identity(chosen: Option<&EnvCandidate>, missing: Option<&str>, denies: &[ReadDeny]) -> String {
    let Some(chosen) = chosen else {
        return format!("missing|{}", missing.unwrap_or_default());
    };
    let python = python_of(&chosen.path);
    let config = chosen.path.join("pyvenv.cfg");
    format!(
        "{}|{:?}|{}|{}|{:?}|{}|{}",
        chosen.path.display(),
        chosen.version,
        stamp(&chosen.path),
        stamp(&python),
        fs::read_link(&python).ok(),
        stamp(&config),
        blake3::hash(read_config(&config, denies).unwrap_or_default().as_bytes())
    )
}

/// One root's resolution, with whether a selection or pin governs it: an authoritative
/// environment that is missing must never be replaced by another interpreter.
#[derive(Clone, Debug)]
pub(crate) struct Resolution {
    /// The resolver's answer for the root.
    pub(crate) env: ResolvedEnv,
    /// A selection, a pin, or an unreadable (denied) Pyright config decides this root.
    pub(crate) authoritative: bool,
    /// The environment directory the selection or pin names, whether or not it exists.
    pub(crate) demanded: Option<PathBuf>,
}

/// Resolves the environment of the project `root` (absolute, inside `worktree`) with the agent's
/// `selector`, per the module's precedence, reading every file under the host's `denies`.
pub(crate) fn resolve_with_denies(
    worktree: &Path,
    root: &Path,
    selector: Option<&str>,
    denies: &[ReadDeny],
) -> Resolution {
    let key = selector_key(worktree, root);
    let mut candidates = discovered(worktree, root, denies);
    let mut warnings = Vec::new();
    let mut authoritative = true;
    let mut demanded = None;
    let paths = (worktree, root);
    let (chosen, source, missing): Outcome = if [PYRIGHT_CONFIG_FILE, PYPROJECT_FILE]
        .iter()
        .any(|name| denies.iter().any(|deny| deny.matches(&root.join(name))))
    {
        let cause = format!(
            "{PYRIGHT_CONFIG_FILE} or {PYPROJECT_FILE} is excluded from reads by the host, so the environment cannot be resolved"
        );
        (None, None, Some(cause))
    } else if let Some(pin) = pin(root, denies) {
        if let Some(selector) = selector {
            warnings.push(format!(
                "selection {selector} ignored: {} pins venv \"{}\"",
                pin.file, pin.venv
            ));
        }
        let absent = format!(
            "{} pins venv \"{}\" ({}), which has no bin/python; candidates {}; create it, or edit venv there or remove the pin",
            pin.file,
            pin.venv,
            label(worktree, root, &pin.dir),
            labels(&candidates)
        );
        demanded = Some(pin.dir.clone());
        settle(
            (paths.0, paths.1, &pin.dir),
            EnvSource::Pin(pin.file.to_owned()),
            absent,
            "recreate it, or edit venv there or remove the pin",
            &mut candidates,
            denies,
        )
    } else if let Some(selector) = selector {
        let dir = selected_dir(root, selector, &candidates);
        let next = format!("recreate it or ide.start environment {{\"{key}\": \"auto\"}}");
        let absent = format!("environment {selector} missing (selected) — {next}");
        demanded = Some(dir.clone());
        settle(
            (paths.0, paths.1, &dir),
            EnvSource::Selected,
            absent,
            &next,
            &mut candidates,
            denies,
        )
    } else {
        authoritative = false;
        match candidates.iter().find(|candidate| !candidate.broken) {
            Some(first) => (Some(first.clone()), Some(EnvSource::Discovered), None),
            None => (
                None,
                None,
                Some(nothing_found(worktree, root, &key, &candidates)),
            ),
        }
    };
    if let (Some(chosen), Some(request)) = (&chosen, version_request(worktree, root, denies))
        && let Some(version) = &chosen.version
        && request.starts_with(|first: char| first.is_ascii_digit())
        && *version != request
        && !version.starts_with(&format!("{request}."))
    {
        warnings.push(format!("≠ .python-version {request}"));
    }
    let identity = identity(chosen.as_ref(), missing.as_deref(), denies);
    Resolution {
        env: ResolvedEnv {
            root: root
                .strip_prefix(worktree)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
            chosen,
            source,
            candidates,
            warnings,
            missing_next_step: missing,
            identity,
        },
        authoritative,
        demanded,
    }
}

/// `resolve_with_denies` without host read denies.
pub fn resolve(worktree: &Path, root: &Path, selector: Option<&str>) -> ResolvedEnv {
    resolve_with_denies(worktree, root, selector, &[]).env
}

/// The project roots environments are resolved for: [`python_roots`], or the worktree alone
/// when it is a Python project only by an environment directory. Empty for a non-Python tree.
fn roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = python_roots(worktree);
    if roots.is_empty() && is_python_project(worktree) {
        roots.push(worktree.to_path_buf());
    }
    roots
}

/// The agent's stored selector for `root`, if any.
fn stored_selection(worktree: &Path, root: &Path) -> Option<String> {
    agent_ide_core::lang::environment::selections(worktree, crate::LANGUAGE)
        .into_iter()
        .find(|selection| worktree.join(&selection.root) == root)
        .map(|selection| selection.selector)
}

/// Resolves `root` with whatever the agent stored for it.
pub(crate) fn resolve_root(worktree: &Path, root: &Path, denies: &[ReadDeny]) -> Resolution {
    resolve_with_denies(
        worktree,
        root,
        stored_selection(worktree, root).as_deref(),
        denies,
    )
}

/// One [`Resolution`] per project root of `worktree`, honouring stored selections.
pub(crate) fn resolutions(worktree: &Path, denies: &[ReadDeny]) -> Vec<Resolution> {
    roots(worktree)
        .iter()
        .map(|root| resolve_root(worktree, root, denies))
        .collect()
}

/// One [`ResolvedEnv`] per project root of `worktree`, honouring stored selections.
pub(crate) fn environments(worktree: &Path, denies: &[ReadDeny]) -> Vec<ResolvedEnv> {
    resolutions(worktree, denies)
        .into_iter()
        .map(|resolution| resolution.env)
        .collect()
}

/// The resolution that speaks for the whole worktree: the first root (the worktree itself sorts
/// first) that chose an environment or whose selection or pin governs it, so a missing
/// authoritative environment is never papered over by another root's. `ponytail:` one
/// interpreter per session, because Pyright has no per-root interpreter; one session per
/// interpreter if monorepos diverge.
pub(crate) fn governing(worktree: &Path) -> Option<Resolution> {
    resolutions(worktree, &[])
        .into_iter()
        .find(|resolution| resolution.env.chosen.is_some() || resolution.authoritative)
}

/// The resolution of the deepest project root containing `path` (absolute, or relative to
/// `worktree`), else the [`governing`] one.
pub(crate) fn for_path(worktree: &Path, path: &Path) -> Option<Resolution> {
    let path = worktree.join(path);
    resolutions(worktree, &[])
        .into_iter()
        .filter(|resolution| path.starts_with(absolute_root(worktree, &resolution.env)))
        .max_by_key(|resolution| resolution.env.root.components().count())
        .or_else(|| governing(worktree))
}

/// The roots a run of `worktree`'s accepted environments reads: each chosen environment directory
/// and its interpreter's canonical installation prefix (a base interpreter outside every static
/// prefix, such as a user-home installation, included), exactly as the in-process check grants.
pub fn accepted_roots(worktree: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for env in environments(worktree, &[]) {
        let Some(interpreter) = interpreter(&env) else {
            continue;
        };
        let canonical = fs::canonicalize(&interpreter).unwrap_or_else(|_| interpreter.clone());
        for path in [&interpreter, &canonical] {
            let prefix = path.parent().and_then(Path::parent).unwrap_or(path);
            if !roots.iter().any(|root| root == prefix) {
                roots.push(prefix.to_path_buf());
            }
        }
    }
    roots
}

/// The session's interpreter and the identity a live session is compared against.
pub(crate) fn session(worktree: &Path) -> (Option<PathBuf>, String) {
    governing(worktree).map_or_else(
        || (None, "none".to_owned()),
        |resolution| (interpreter(&resolution.env), resolution.env.identity),
    )
}

/// Accepts `selector` for the project root `root` (relative to `worktree`): `auto`, a candidate
/// label, or a path to an environment directory; refuses an unknown root, a Pyright pin in force
/// and an unknown selector, each with the way out.
pub(crate) fn check_selection(worktree: &Path, root: &Path, selector: &str) -> Result<(), String> {
    let absolute = worktree.join(root);
    let known = roots(worktree);
    if !known.contains(&absolute) {
        let names: Vec<String> = known
            .iter()
            .map(|known| match label(worktree, worktree, known) {
                name if name.is_empty() => ".".to_owned(),
                name => name,
            })
            .collect();
        return Err(format!(
            "{} is not a Python project root; roots {}",
            root.display(),
            names.join(", ")
        ));
    }
    if selector == "auto" {
        return Ok(());
    }
    if let Some(pin) = pin(&absolute, &[]) {
        return Err(format!(
            "{} pins venv \"{}\" — selection ignored; edit venv there or remove the pin",
            pin.file, pin.venv
        ));
    }
    let candidates = discovered(worktree, &absolute, &[]);
    match interpreter_state(&selected_dir(&absolute, selector, &candidates), &[]) {
        Some(true) => Ok(()),
        Some(false) => Err(format!(
            "\"{selector}\" is broken (base interpreter gone); candidates {}",
            labels(&candidates)
        )),
        None => Err(format!(
            "\"{selector}\" not found; candidates {}",
            labels(&candidates)
        )),
    }
}

/// A root's missing-environment cause and next step, prefixed with the root when it is nested.
pub(crate) fn root_detail(env: &ResolvedEnv) -> Option<String> {
    env.missing_next_step.as_ref().map(|step| {
        if env.root.as_os_str().is_empty() {
            step.clone()
        } else {
            format!("{}: {step}", env.root.display())
        }
    })
}

/// Absent demanded interpreter → the resolver's cause and next step for it.
type MissingSteps = HashMap<PathBuf, String>;

/// The resolver's way out for each demanded interpreter `detect` found absent. A
/// `LanguageProject` carries that interpreter path but no worktree, so test and format commands
/// built from it look the text up here instead of guessing whether a pin or a selection (and
/// which `python:<root>` key) is in force. Entries are a few short strings per worktree.
static MISSING_STEPS: LazyLock<Mutex<MissingSteps>> = LazyLock::new(Mutex::default);

/// Records `step` as the way out for the absent demanded interpreter `python`.
pub(crate) fn remember_missing(python: &Path, step: String) {
    MISSING_STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(python.to_path_buf(), step);
}

/// The way out recorded for the absent demanded interpreter `python`, if `detect` saw it.
pub(crate) fn missing_step(python: &Path) -> Option<String> {
    MISSING_STEPS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(python)
        .cloned()
}

/// The absolute project root of `env`: the worktree itself (without a trailing separator) for
/// the worktree's own root.
pub(crate) fn absolute_root(worktree: &Path, env: &ResolvedEnv) -> PathBuf {
    if env.root.as_os_str().is_empty() {
        worktree.to_path_buf()
    } else {
        worktree.join(&env.root)
    }
}

/// Whether the environment's `bin` holds `program` as an executable file: a bare name
/// (`pytest`), or an absolute path into that `bin` (the language's own commands).
fn owns(bin: &Path, program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let tool = Path::new(program);
    let path = match tool.parent() {
        Some(parent) if parent == bin => tool.to_path_buf(),
        Some(parent) if parent.as_os_str().is_empty() => bin.join(tool),
        _ => return false,
    };
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Activation of the resolved environment of the deepest project root containing `cwd`, for
/// programs that environment owns only: a bare `python`/`python3` is replaced by the
/// environment's interpreter, and a tool installed in its `bin` (`pytest`, `ruff`) runs with that
/// `bin` first on `PATH` and `VIRTUAL_ENV` set. `None` for every other program (`cargo`, `make`),
/// so another language's toolchain is never taken over, and when that root resolves nothing.
pub(crate) fn command_env(worktree: &Path, cwd: &Path, program: &str) -> Option<CommandEnv> {
    let venv = for_path(worktree, cwd)?.env.chosen?.path;
    let bin = venv.join("bin");
    let argv_prefix = if matches!(program, "python" | "python3") {
        vec![python_of(&venv).into_os_string()]
    } else if owns(&bin, program) {
        Vec::new()
    } else {
        return None;
    };
    Some(CommandEnv {
        argv_prefix,
        path_prefix: Some(bin),
        vars: vec![(OsString::from("VIRTUAL_ENV"), venv.into_os_string())],
    })
}

#[cfg(test)]
mod tests {
    //! The resolver's precedence table and its agreement with every consumer (environment
    //! selection design §5.6).

    use std::sync::Arc;
    use std::time::Duration;

    use agent_ide_core::checks::runner::{ConfinedRunner, FakeRunner, RunOutput};
    use agent_ide_core::checks::{
        CheckRequest, CheckState, Checker, LanguageChecks, UnavailableReason,
    };
    use agent_ide_core::lang::environment::{EnvSelection, replace_selections};
    use agent_ide_core::lang::{LangError, LanguageSupport, TestTarget};

    use super::*;
    use crate::checks::{PythonChecker, PythonChecks};
    use crate::support::Python;

    /// Fresh scratch directory for one fixture.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-python-env-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Writes `text` to `root/relative`, creating parents.
    fn put(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Creates the environment `name` under `root` with `bin/python` and, unless empty, the
    /// given `pyvenv.cfg` text.
    fn venv(root: &Path, name: &str, config: &str) {
        put(root, &format!("{name}/bin/python"), "");
        if !config.is_empty() {
            put(root, &format!("{name}/pyvenv.cfg"), config);
        }
    }

    /// Stores the agent's selection `selector` for the project root `root` of `worktree`.
    fn select(worktree: &Path, root: &str, selector: &str) {
        replace_selections(
            worktree,
            crate::LANGUAGE,
            vec![EnvSelection {
                root: PathBuf::from(root),
                selector: selector.to_owned(),
            }],
        );
    }

    /// The chosen environment's label, if any.
    fn chosen(env: &ResolvedEnv) -> Option<&str> {
        env.chosen.as_ref().map(|chosen| chosen.label.as_str())
    }

    /// A checker over placeholder tools that exist (so it gets past its tool check) and a fake
    /// runner answering one clean report per run.
    fn checker(root: &Path, runs: usize) -> (PythonChecker, Arc<FakeRunner>) {
        put(root, "tools/node", "");
        put(root, "tools/pyright.js", "");
        let clean = br#"{"generalDiagnostics": [], "summary": {"errorCount": 0, "warningCount": 0, "filesAnalyzed": 1}}"#;
        let fake = Arc::new(FakeRunner::new(
            (0..runs)
                .map(|_| {
                    Ok(RunOutput {
                        status: Some(0),
                        stdout: clean.to_vec(),
                        ..RunOutput::default()
                    })
                })
                .collect(),
        ));
        let runner: Arc<dyn ConfinedRunner> = fake.clone();
        (
            PythonChecker::new(
                runner,
                root.join("tools/node"),
                root.join("tools/pyright.js"),
                Duration::from_secs(60),
            ),
            fake,
        )
    }

    /// One check request for `worktree` with a scratch cache directory.
    fn request(worktree: &Path, cache: &Path) -> CheckRequest {
        CheckRequest {
            worktree: worktree.to_path_buf(),
            cache_dir: cache.to_path_buf(),
            input_generation: 1,
            read_denies: Vec::new(),
        }
    }

    /// One case per precedence step: suffixed discovery sorted by name, `venv` over suffixed,
    /// `.venv` over `venv`, a selection over discovery, `[tool.pyright]` over a selection,
    /// `pyrightconfig.json` over `[tool.pyright]`, and nothing at all.
    #[test]
    fn precedence_runs_pin_then_selection_then_discovery() {
        let root = scratch("precedence");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        venv(&root, ".venv-b", "");
        venv(&root, "venv-a", "");
        venv(&root, ".venv-a", "");
        let env = resolve(&root, &root, None);
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-a"), Some(EnvSource::Discovered)),
            "suffixed candidates sort by name"
        );
        venv(&root, "venv", "");
        assert_eq!(chosen(&resolve(&root, &root, None)), Some("venv"));
        venv(&root, ".venv", "");
        let env = resolve(&root, &root, None);
        assert_eq!(chosen(&env), Some(".venv"));
        let order: Vec<&str> = env
            .candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect();
        assert_eq!(order, [".venv", "venv", ".venv-a", ".venv-b", "venv-a"]);

        let env = resolve(&root, &root, Some(".venv-b"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-b"), Some(EnvSource::Selected))
        );

        put(
            &root,
            "pyproject.toml",
            "[project]\nname = \"svc\"\n\n[tool.pyright]\nvenvPath = \".\"\nvenv = \"venv\"\n",
        );
        let env = resolve(&root, &root, Some(".venv-b"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (
                Some("venv"),
                Some(EnvSource::Pin("pyproject.toml".to_owned()))
            )
        );
        assert_eq!(
            env.warnings,
            ["selection .venv-b ignored: pyproject.toml pins venv \"venv\""]
        );

        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-a"}"#,
        );
        let env = resolve(&root, &root, None);
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (
                Some(".venv-a"),
                Some(EnvSource::Pin("pyrightconfig.json".to_owned()))
            )
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("precedence-none");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        let env = resolve(&root, &root, None);
        assert_eq!((env.chosen.clone(), env.source.clone()), (None, None));
        assert_eq!(
            env.missing_next_step.as_deref(),
            Some(
                "no .venv* beside pyproject.toml; create one (uv venv) or ide.start environment {\"python\": \"<path>\"}"
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Versions come from `pyvenv.cfg` `version`, else `version_info` (trimmed to `X.Y.Z`),
    /// else none, which is not broken; a missing `home` directory is.
    #[test]
    fn pyvenv_cfg_supplies_version_and_broken() {
        let root = scratch("pyvenv");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "home = /usr/bin\nversion = 3.12.7\n");
        venv(
            &root,
            ".venv-uv",
            "home = /nonexistent-agent-ide-home/bin\nversion_info = 3.14.0\n",
        );
        venv(&root, ".venv-virtualenv", "version_info = 3.12.7.final.0\n");
        venv(&root, ".venv-bare", "");
        let env = resolve(&root, &root, None);
        let described: Vec<(&str, Option<&str>, bool)> = env
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.label.as_str(),
                    candidate.version.as_deref(),
                    candidate.broken,
                )
            })
            .collect();
        assert_eq!(
            described,
            [
                (".venv", Some("3.12.7"), false),
                (".venv-bare", None, false),
                (".venv-uv", Some("3.14.0"), true),
                (".venv-virtualenv", Some("3.12.7"), false),
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Failure (a): a pin naming an absent environment resolves to nothing — never a fallback
    /// to `.venv-py314` — and the card, the resolver and the check report the same cause.
    #[tokio::test]
    async fn pinned_missing_environment_is_env_missing_everywhere() {
        let root = scratch("pin-missing");
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
        );
        venv(&root, ".venv-py314", "");
        let envs = environments(&root, &[]);
        assert_eq!(envs.len(), 1);
        assert_eq!(chosen(&envs[0]), None);
        let step = envs[0].missing_next_step.clone().unwrap();
        assert_eq!(
            step,
            "pyrightconfig.json pins venv \".venv-gone\" (.venv-gone), which has no bin/python; candidates .venv-py314; create it, or edit venv there or remove the pin"
        );
        assert_eq!(Python.detect(&root).unwrap().commands.test, None);
        assert_eq!(crate::checks::session_interpreter(&root), None);

        let (checker, fake) = checker(&root, 0);
        let snapshot = checker.check(request(&root, &root.join("cache"))).await;
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::EnvMissing)
        );
        assert_eq!(snapshot.detail, Some(step));
        assert!(fake.specs().is_empty(), "pyright never runs without an env");
        fs::remove_dir_all(&root).unwrap();
    }

    /// Failure (b): a worktree whose only Python marker is `.venv-py314` is a Python project for
    /// the card and the checks alike; a suffixed directory without `bin/python` is not.
    #[test]
    fn suffixed_environment_alone_marks_a_python_project() {
        let root = scratch("suffixed-only");
        venv(&root, ".venv-py314", "");
        assert!(PythonChecks.is_present(&root));
        assert_eq!(
            Python.detect(&root).unwrap().interpreter,
            Some(root.join(".venv-py314/bin/python"))
        );
        let envs = environments(&root, &[]);
        assert_eq!(envs.len(), 1);
        assert_eq!(chosen(&envs[0]), Some(".venv-py314"));
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("suffixed-empty");
        put(&root, ".venv-py314/lib/readme", "");
        assert!(!PythonChecks.is_present(&root));
        assert!(environments(&root, &[]).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// One resolver: the card's interpreter, the probe's program, the session interpreter and
    /// the check's `--pythonpath` are the same path on discovery, selection, pin and nested
    /// fallback fixtures.
    #[tokio::test]
    async fn card_check_session_and_probe_agree() {
        type Setup = fn(&Path);
        let cases: [(&str, Setup, &str); 4] = [
            (
                "agree-discovered",
                |root| {
                    put(root, "pyproject.toml", "");
                    venv(root, ".venv-py314", "");
                    venv(root, ".venv", "");
                },
                ".venv",
            ),
            (
                "agree-selected",
                |root| {
                    put(root, "pyproject.toml", "");
                    venv(root, ".venv-py314", "");
                    venv(root, ".venv", "");
                    select(root, "", ".venv-py314");
                },
                ".venv-py314",
            ),
            (
                "agree-pinned",
                |root| {
                    put(
                        root,
                        "pyproject.toml",
                        "[tool.pyright]\nvenvPath = \"envs\"\nvenv = \"main\"\n",
                    );
                    venv(root, ".venv", "");
                    venv(root, "envs/main", "");
                },
                "envs/main",
            ),
            (
                "agree-nested",
                |root| {
                    venv(root, ".venv", "");
                    put(root, "svc/pyproject.toml", "");
                    put(root, "svc/app.py", "");
                },
                ".venv",
            ),
        ];
        for (name, setup, expected) in cases {
            let root = scratch(name);
            setup(&root);
            let python = root.join(expected).join("bin/python");
            let project = Python.detect(&root).unwrap();
            assert_eq!(project.interpreter.as_ref(), Some(&python), "{name}: card");
            assert_eq!(
                Python
                    .syntax_probe_command(&project, &root, Path::new("a.py"), None)
                    .unwrap()[0],
                python.display().to_string(),
                "{name}: probe"
            );
            assert_eq!(
                crate::checks::session_interpreter(&root).as_ref(),
                Some(&python),
                "{name}: session"
            );
            let (checker, fake) = checker(&root, 1);
            checker.check(request(&root, &root.join("cache"))).await;
            let specs = fake.specs();
            assert_eq!(specs.len(), 1, "{name}: one root checked");
            let at = specs[0]
                .args
                .iter()
                .position(|arg| arg == "--pythonpath")
                .unwrap();
            assert_eq!(specs[0].args[at + 1], python.as_os_str(), "{name}: check");
            replace_selections(&root, crate::LANGUAGE, Vec::new());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// `.python-version` is a version request: a mismatch warns and never switches the env.
    #[test]
    fn python_version_mismatch_warns_without_switching() {
        let root = scratch("python-version");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "version = 3.12.7\n");
        venv(&root, ".venv-py314", "version_info = 3.14.0\n");
        put(&root, ".python-version", "3.14\n");
        let env = resolve(&root, &root, None);
        assert_eq!(chosen(&env), Some(".venv"));
        assert_eq!(env.warnings, ["≠ .python-version 3.14"]);
        put(&root, ".python-version", "3.12\n");
        assert!(resolve(&root, &root, None).warnings.is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    /// A stored selection is honoured by every consumer, cleared by an empty list, and a
    /// selected environment that disappears is missing with its way back — never a fallback.
    #[test]
    fn stored_selection_is_honoured_and_its_disappearance_reported() {
        let root = scratch("stored-selection");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        select(&root, "", ".venv-py314");
        let envs = Python.environments(&root);
        assert_eq!(
            (chosen(&envs[0]), envs[0].source.clone()),
            (Some(".venv-py314"), Some(EnvSource::Selected))
        );
        assert_eq!(
            Python.detect(&root).unwrap().interpreter,
            Some(root.join(".venv-py314/bin/python"))
        );
        fs::remove_dir_all(root.join(".venv-py314")).unwrap();
        let env = &Python.environments(&root)[0];
        assert_eq!(chosen(env), None);
        assert_eq!(
            env.missing_next_step.as_deref(),
            Some(
                "environment .venv-py314 missing (selected) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        assert_eq!(chosen(&Python.environments(&root)[0]), Some(".venv"));
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("stored-selection-nested");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        select(&root, "packages/alpha", "gone");
        assert_eq!(
            Python.environments(&root)[0].missing_next_step.as_deref(),
            Some(
                "environment gone missing (selected) — recreate it or ide.start environment {\"python:packages/alpha\": \"auto\"}"
            )
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Accepts a label, `auto`, a relative or absolute path to an environment; refuses an
    /// unknown selector with the candidates, an unknown root, and any selection under a pin.
    #[test]
    fn check_selection_accepts_and_refuses() {
        let root = scratch("check-selection");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        let outside = scratch("check-selection-outside");
        venv(&outside, "env", "");
        let check = |selector: &str| Python.check_selection(&root, Path::new(""), selector);
        assert_eq!(check(".venv-py314"), Ok(()));
        assert_eq!(check("auto"), Ok(()));
        assert_eq!(check("./.venv-py314"), Ok(()));
        assert_eq!(check(&outside.join("env").display().to_string()), Ok(()));
        assert_eq!(
            check(".venv-py9"),
            Err("\".venv-py9\" not found; candidates .venv, .venv-py314".to_owned())
        );
        assert!(
            Python
                .check_selection(&root, Path::new("nope"), ".venv")
                .unwrap_err()
                .starts_with("nope is not a Python project root; roots .")
        );
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv"}"#,
        );
        assert_eq!(
            check(".venv-py314"),
            Err(
                "pyrightconfig.json pins venv \".venv\" — selection ignored; edit venv there or remove the pin"
                    .to_owned()
            )
        );
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    /// The accepted roots are the chosen environment and its interpreter's canonical installation
    /// prefix, reached through the `bin/python` link even when it lies outside every static
    /// prefix; a project without an environment accepts nothing.
    #[test]
    fn accepted_roots_follow_the_interpreter_link() {
        let root = scratch("accepted-roots");
        let base = scratch("accepted-roots-base");
        put(&base, "bin/python3.14", "");
        put(&root, "pyproject.toml", "");
        fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::os::unix::fs::symlink(base.join("bin/python3.14"), root.join(".venv/bin/python"))
            .unwrap();
        assert_eq!(
            accepted_roots(&root),
            vec![root.join(".venv"), fs::canonicalize(&base).unwrap()]
        );
        fs::remove_dir_all(root.join(".venv")).unwrap();
        assert!(accepted_roots(&root).is_empty());
        fs::remove_dir_all(&root).unwrap();
        fs::remove_dir_all(&base).unwrap();
    }

    /// Explicit commands run activated in the environment of the deepest root holding `cwd`,
    /// for the programs that environment owns only: `python` is replaced by the interpreter, an
    /// installed tool gets the environment's `bin` and `VIRTUAL_ENV`, and an uninstalled tool or
    /// another language's program (`cargo`) is left alone.
    #[test]
    fn command_env_activates_only_programs_the_environment_owns() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("command-env");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        let alpha = root.join("packages/alpha/.venv-a");
        let pytest = alpha.join("bin/pytest");
        put(&root, "packages/alpha/.venv-a/bin/pytest", "");
        fs::set_permissions(&pytest, fs::Permissions::from_mode(0o755)).unwrap();
        let activated = CommandEnv {
            argv_prefix: Vec::new(),
            path_prefix: Some(alpha.join("bin")),
            vars: vec![(
                OsString::from("VIRTUAL_ENV"),
                alpha.clone().into_os_string(),
            )],
        };

        let env = Python
            .command_env(&root, &root.join("packages/alpha/tests"), "python3")
            .unwrap();
        assert_eq!(
            env,
            CommandEnv {
                argv_prefix: vec![alpha.join("bin/python").into_os_string()],
                ..activated.clone()
            }
        );
        assert_eq!(
            Python.command_env(&root, Path::new("packages/alpha"), "pytest"),
            Some(activated.clone()),
            "an installed tool runs activated"
        );
        assert_eq!(
            Python.command_env(
                &root,
                Path::new("packages/alpha"),
                &pytest.display().to_string()
            ),
            Some(activated),
            "the language's own absolute command runs activated"
        );
        assert_eq!(
            Python.command_env(&root, Path::new("packages/alpha"), "ruff"),
            None,
            "a tool the environment lacks is not taken over"
        );
        assert_eq!(
            Python.command_env(&root, &root, "cargo"),
            None,
            "another language's program is never taken over"
        );
        assert_eq!(Python.command_env(&root, &root, "/usr/bin/make"), None);
        assert_eq!(
            Python
                .command_env(&root, &root, "python")
                .unwrap()
                .path_prefix,
            Some(root.join(".venv/bin"))
        );
        fs::remove_dir_all(root.join(".venv")).unwrap();
        assert_eq!(Python.command_env(&root, &root, "python"), None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Real Pyright: `.venv` lacks `stub_package`, `.venv-py314` provides it. Before the
    /// selection the project check reports the import unresolved; after selecting
    /// `.venv-py314` the same check resolves it. Needs `AGENT_IDE_PYRIGHT`, `AGENT_IDE_NODE` and
    /// `AGENT_IDE_PYTHON` like the product tests.
    #[tokio::test]
    #[ignore = "requires accepted AGENT_IDE_PYRIGHT, AGENT_IDE_NODE and AGENT_IDE_PYTHON environments"]
    async fn real_pyright_check_follows_the_selected_environment() {
        use agent_ide_core::checks::runner::{NestedSandboxFallbackRunner, SeatbeltRunner};

        let python = PathBuf::from(std::env::var_os("AGENT_IDE_PYTHON").unwrap());
        let node = PathBuf::from(std::env::var_os("AGENT_IDE_NODE").unwrap());
        let pyright = PathBuf::from(std::env::var_os("AGENT_IDE_PYRIGHT").unwrap());
        let cli = pyright
            .parent()
            .and_then(Path::parent)
            .map(|prefix| prefix.join("lib/node_modules/pyright/dist/pyright.js"))
            .map(|cli| fs::canonicalize(&cli).unwrap_or(cli))
            .unwrap();
        let root = fs::canonicalize(scratch("real-selection")).unwrap();
        put(&root, "pyproject.toml", "[project]\nname = \"svc\"\n");
        put(&root, "src/app.py", "from stub_package import VALUE\n");
        // Pyright 1.1.413 flags parts of the 3.14 standard library itself; ignore them so only the
        // import under test counts. No `venv` key: this config pins nothing.
        let prefix = fs::canonicalize(&python)
            .unwrap()
            .parent()
            .and_then(Path::parent)
            .unwrap()
            .to_path_buf();
        put(
            &root,
            "pyrightconfig.json",
            &serde_json::json!({ "ignore": [prefix] }).to_string(),
        );
        for name in [".venv", ".venv-py314"] {
            let interpreter = root.join(name).join("bin/python");
            fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&python, &interpreter).unwrap();
            put(
                &root,
                &format!("{name}/pyvenv.cfg"),
                &format!(
                    "home = {}\ninclude-system-site-packages = false\nversion = 3.14.0\n",
                    python.parent().unwrap().display()
                ),
            );
        }
        put(
            &root,
            ".venv-py314/lib/python3.14/site-packages/stub_package/__init__.py",
            "VALUE: int = 42\n",
        );
        let runner: Arc<dyn ConfinedRunner> =
            Arc::new(NestedSandboxFallbackRunner::new(Arc::new(SeatbeltRunner)));
        let checker = PythonChecker::new(runner, node, cli, Duration::from_secs(120));
        let unresolved = |snapshot: &agent_ide_core::checks::ProblemSnapshot| {
            assert!(
                matches!(snapshot.state, CheckState::Ready | CheckState::Partial),
                "{snapshot:?}"
            );
            snapshot.problems.iter().any(|problem| {
                problem.path == "src/app.py"
                    && problem.code.as_deref() == Some("reportMissingImports")
            })
        };

        let before = checker.check(request(&root, &root.join("cache"))).await;
        assert!(unresolved(&before), "{before:?}");
        select(&root, "", ".venv-py314");
        let after = checker.check(request(&root, &root.join("cache"))).await;
        assert!(!unresolved(&after), "{after:?}");
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix A: a selected or pinned environment that is missing never lets another interpreter
    /// run: no card command, a test error with the way out, no formatter and no probe.
    #[test]
    fn demanded_missing_environment_never_runs_another_interpreter() {
        for (name, pinned) in [("demanded-selected", false), ("demanded-pinned", true)] {
            let root = scratch(name);
            put(&root, "pyproject.toml", "[tool.ruff]\nline-length = 100\n");
            put(&root, "uv.lock", "");
            venv(&root, ".venv", "");
            if pinned {
                put(
                    &root,
                    "pyrightconfig.json",
                    r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
                );
            } else {
                select(&root, "", ".venv-gone");
            }
            let project = Python.detect(&root).unwrap();
            assert_eq!(project.commands.test, None, "{name}");
            assert_eq!(project.commands.format, None, "{name}");
            let error = Python
                .test_selection(&project, &TestTarget::Pattern("x".to_owned()))
                .unwrap_err();
            let step = environments(&root, &[])[0]
                .missing_next_step
                .clone()
                .unwrap();
            assert_eq!(error, LangError::Unsupported(step), "{name}");
            assert_eq!(Python.format_command(&project, Path::new("a.py")), None);
            assert_eq!(
                Python.format_stdin_command(&project, Path::new("a.py")),
                None
            );
            assert_eq!(
                Python.syntax_probe_command(&project, &root, Path::new("a.py"), None),
                None,
                "{name}"
            );
            replace_selections(&root, crate::LANGUAGE, Vec::new());
            fs::remove_dir_all(&root).unwrap();
        }
    }

    /// Fix B: the worktree root's missing selection is reported by the check and leaves the
    /// session without an interpreter, although a nested root's environment works.
    #[tokio::test]
    async fn missing_worktree_selection_is_not_hidden_by_a_nested_root() {
        let root = scratch("hidden-by-nested");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        select(&root, "", ".venv-gone");
        assert_eq!(crate::checks::session_interpreter(&root), None);
        let (checker, fake) = checker(&root, 2);
        let snapshot = checker.check(request(&root, &root.join("cache"))).await;
        assert_eq!(
            snapshot.state,
            CheckState::Unavailable(UnavailableReason::EnvMissing)
        );
        assert_eq!(
            snapshot.detail.as_deref(),
            Some(
                "environment .venv-gone missing (selected) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        assert!(fake.specs().is_empty());
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix C (probe): the syntax probe runs the interpreter of the root holding the file, a
    /// nested root's selection included.
    #[test]
    fn probe_uses_the_environment_of_the_files_root() {
        let root = scratch("probe-per-root");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        venv(&root, "packages/alpha/.venv-a", "");
        venv(&root, "packages/alpha/.venv-b", "");
        select(&root, "packages/alpha", ".venv-b");
        let project = Python.detect(&root).unwrap();
        let probe = |file: &str| {
            Python
                .syntax_probe_command(&project, &root, Path::new(file), None)
                .unwrap()
                .remove(0)
        };
        assert_eq!(
            probe("packages/alpha/a.py"),
            root.join("packages/alpha/.venv-b/bin/python")
                .display()
                .to_string()
        );
        assert_eq!(
            probe("src/x.py"),
            root.join(".venv/bin/python").display().to_string()
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix D: an existing `pyrightconfig.json` without `venv` keys pins nothing, even when
    /// `[tool.pyright]` names a venv (Pyright never reads it then), so a selection is allowed.
    #[test]
    fn pyrightconfig_without_venv_keys_overrides_the_pyproject_pin() {
        let root = scratch("json-wins");
        put(
            &root,
            "pyproject.toml",
            "[tool.pyright]\nvenvPath = \".\"\nvenv = \".venv\"\n",
        );
        put(&root, "pyrightconfig.json", r#"{"strict": ["src"]}"#);
        venv(&root, ".venv", "");
        venv(&root, ".venv-py314", "");
        assert_eq!(
            Python.check_selection(&root, Path::new(""), ".venv-py314"),
            Ok(())
        );
        let env = resolve(&root, &root, Some(".venv-py314"));
        assert_eq!(
            (chosen(&env), env.source.clone()),
            (Some(".venv-py314"), Some(EnvSource::Selected))
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix E: `.python-version` and `pyvenv.cfg` follow the host's read denies; denied files
    /// are unreadable and their contents never reach a warning or a version.
    #[test]
    fn read_denies_hide_python_version_and_pyvenv_cfg() {
        let root = fs::canonicalize(scratch("denied-files")).unwrap();
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "version = 3.12.7\n");
        put(&root, ".python-version", "3.99\n");
        assert_eq!(
            resolve(&root, &root, None).warnings,
            ["≠ .python-version 3.99"]
        );
        let env = resolve_with_denies(
            &root,
            &root,
            None,
            &[ReadDeny::Path(root.join(".python-version"))],
        )
        .env;
        assert_eq!(chosen(&env), Some(".venv"));
        assert!(env.warnings.is_empty(), "{:?}", env.warnings);
        assert_eq!(env.chosen.unwrap().version.as_deref(), Some("3.12.7"));
        let env = resolve_with_denies(
            &root,
            &root,
            None,
            &[ReadDeny::Path(root.join(".venv/pyvenv.cfg"))],
        )
        .env;
        assert_eq!(env.chosen.unwrap().version, None);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix F: selecting the current winner keeps the identity; recreating the environment at
    /// the same path changes it.
    #[test]
    fn identity_ignores_source_and_tracks_recreation() {
        let root = scratch("identity");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        let discovered = resolve(&root, &root, None).identity;
        assert_eq!(resolve(&root, &root, Some(".venv")).identity, discovered);
        fs::remove_dir_all(root.join(".venv")).unwrap();
        venv(&root, ".venv", "");
        assert_ne!(resolve(&root, &root, None).identity, discovered);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fix G: an environment whose `bin/python` is a dangling link stays a candidate marked
    /// broken; discovery passes over it, and selecting it reports it broken.
    #[test]
    fn dangling_interpreter_stays_a_broken_candidate() {
        let root = scratch("dangling");
        put(&root, "pyproject.toml", "");
        fs::create_dir_all(root.join(".venv/bin")).unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent-agent-ide/python3",
            root.join(".venv/bin/python"),
        )
        .unwrap();
        venv(&root, ".venv-ok", "");
        let env = resolve(&root, &root, None);
        let broken: Vec<(&str, bool)> = env
            .candidates
            .iter()
            .map(|candidate| (candidate.label.as_str(), candidate.broken))
            .collect();
        assert_eq!(broken, [(".venv", true), (".venv-ok", false)]);
        assert_eq!(chosen(&env), Some(".venv-ok"));
        let selected = resolve(&root, &root, Some(".venv"));
        assert_eq!(chosen(&selected), None);
        assert_eq!(
            selected.missing_next_step.as_deref(),
            Some(
                "environment .venv is broken (base interpreter gone) — recreate it or ide.start environment {\"python\": \"auto\"}"
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// A broken environment (dangling `bin/python`) is refused with its cause, since selecting
    /// it could only end in a missing environment.
    #[test]
    fn check_selection_refuses_a_broken_environment() {
        let root = scratch("select-broken");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        fs::create_dir_all(root.join(".venv-old/bin")).unwrap();
        std::os::unix::fs::symlink(
            "/nonexistent-agent-ide/python3",
            root.join(".venv-old/bin/python"),
        )
        .unwrap();
        assert_eq!(
            Python.check_selection(&root, Path::new(""), ".venv-old"),
            Err(
                "\".venv-old\" is broken (base interpreter gone); candidates .venv, .venv-old"
                    .to_owned()
            )
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// The test command's error is the resolver's own way out: for a pin, editing or removing
    /// the pin (a selection cannot help); for a nested root's selection, its `python:<root>` key.
    #[test]
    fn missing_environment_error_is_the_resolvers_next_step() {
        let root = scratch("next-step-pin");
        put(&root, "pyproject.toml", "");
        venv(&root, ".venv", "");
        put(
            &root,
            "pyrightconfig.json",
            r#"{"venvPath": ".", "venv": ".venv-gone"}"#,
        );
        let project = Python.detect(&root).unwrap();
        let Err(LangError::Unsupported(message)) =
            Python.test_selection(&project, &TestTarget::Pattern("x".to_owned()))
        else {
            panic!("a missing pinned environment refuses the test run");
        };
        assert!(
            message.contains("edit venv there or remove the pin") && !message.contains("auto"),
            "{message}"
        );
        fs::remove_dir_all(&root).unwrap();

        let root = scratch("next-step-nested");
        put(&root, "packages/alpha/pyproject.toml", "");
        put(&root, "packages/alpha/a.py", "");
        select(&root, "packages/alpha", ".venv-gone");
        let project = Python.detect(&root).unwrap();
        assert_eq!(
            Python.test_selection(&project, &TestTarget::Pattern("x".to_owned())),
            Err(LangError::Unsupported(
                "packages/alpha: environment .venv-gone missing (selected) — recreate it or ide.start environment {\"python:packages/alpha\": \"auto\"}"
                    .to_owned()
            ))
        );
        replace_selections(&root, crate::LANGUAGE, Vec::new());
        fs::remove_dir_all(&root).unwrap();
    }
}
