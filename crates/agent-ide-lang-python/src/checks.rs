//! Python project checker (EYES-r2 §4 "Python").
//!
//! [`PythonChecker`] runs the pinned `pyright` CLI under node, `--outputjson`, against the
//! project's own interpreter, and turns its JSON report into a [`ProblemSnapshot`]. It never
//! executes project source: pyright only parses and type-checks, though starting the resolved
//! interpreter to enumerate its search paths does run that interpreter's own start-up hooks
//! (`sitecustomize`, `.pth` files) inside the same [`ConfinedRunner`] confinement as the check
//! itself (EYES-r2 §4). Every process this checker starts goes through the injected
//! [`ConfinedRunner`]; this module never spawns a process directly.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use agent_ide_core::assistance::launcher::absolute;
use agent_ide_core::checks::runner::{ConfinedRunner, RunSpec};
use agent_ide_core::checks::{
    BoxFuture, CheckConfig, CheckRequest, CheckState, Checker, Language, LanguageChecks, Problem,
    ProblemSnapshot, Severity, UnavailableReason, run_failure_cause,
};

/// Name of the pyright project config file consulted at a worktree's root.
pub(crate) const PYRIGHT_CONFIG_FILE: &str = "pyrightconfig.json";

/// Name of the PEP 518 project file whose `[tool.pyright]` table is the second interpreter
/// resolution source.
pub(crate) const PYPROJECT_FILE: &str = "pyproject.toml";

/// Detail attached to an [`UnavailableReason::NoFiles`] snapshot (T12B), naming the project
/// configuration a fix should start from.
const NO_FILES_DETAIL: &str = "pyright analyzed 0 files; check \"include\"/\"exclude\" in pyrightconfig.json or [tool.pyright]";

/// Per-stream capture limit passed to the [`ConfinedRunner`] for every pyright run.
///
/// Pyright's JSON report grows with the number of reported diagnostics; 64 MiB comfortably
/// covers even a large monorepo without letting a runaway process buffer unbounded output.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Runs Python project-wide diagnostics with the pinned pyright CLI under a [`ConfinedRunner`].
///
/// One checker instance is built per configured `python` toolchain (EYES-r2 §1) and is reused
/// across every worktree and check run for that toolchain; it carries no per-run state of its
/// own beyond the fixed `node`/`pyright_cli`/`timeout` it was constructed with; `runner` is the
/// only process-execution seam this checker uses.
pub struct PythonChecker {
    /// Executes the confined `node <pyright_cli> …` process for every run.
    runner: Arc<dyn ConfinedRunner>,
    /// Absolute path to the pinned `node` binary; `pyright_cli` is run under it.
    node: PathBuf,
    /// Absolute path to the pinned pyright CLI entry point (a JavaScript file run by `node`, not
    /// a directly executable binary).
    pyright_cli: PathBuf,
    /// Wall-clock budget handed to the [`ConfinedRunner`] for one pyright run.
    timeout: Duration,
}

impl PythonChecker {
    /// Builds a checker for the pinned `node`/`pyright_cli` pair.
    ///
    /// Neither path is validated here: a missing `node` or `pyright_cli` is detected lazily, on
    /// the first [`Checker::check`] call, and reported as [`UnavailableReason::ToolMissing`]
    /// rather than rejected at construction, so a checker can be built once at daemon start even
    /// before its configured toolchain is confirmed present.
    pub fn new(
        runner: Arc<dyn ConfinedRunner>,
        node: PathBuf,
        pyright_cli: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            node,
            pyright_cli,
            timeout,
        }
    }

    /// Builds the [`RunSpec`] for one pyright run against `request`, using the already-resolved
    /// `interpreter` (as the shared resolver returns it, not yet canonicalized).
    ///
    /// `interpreter` itself — not its canonical form — is what `--pythonpath` receives: a
    /// uv-managed (or otherwise symlinked) venv's `bin/python` is a symlink to a base
    /// installation's interpreter, and Python's own venv detection keys off `pyvenv.cfg` sitting
    /// next to the symlink it was *invoked as* (`sys._base_executable`/`sys.prefix` resolution),
    /// not next to whatever that symlink resolves to. Passing the canonical (resolved) path here
    /// would make pyright run the base interpreter as if it had no venv, so it would never see
    /// the venv's `site-packages`. A `venvPath`/`venv` pin in `pyrightconfig.json`/`pyproject.toml`
    /// outranks `--pythonpath` inside pyright (the pin supplies the site-packages; pythonPath adds
    /// only non-site-packages roots), so the resolver reads that pin first and hands exactly its
    /// interpreter here: the two sources cannot disagree.
    ///
    /// `interpreter` is canonicalized only to derive `read_roots` (falling back to the given path
    /// if canonicalization fails, which only happens if the file was removed between resolution
    /// and this call): the canonical path's installation prefix (its parent-of-parent) is added
    /// so the confinement profile covers the real interpreter binary and its standard library —
    /// pyright starts `interpreter` to enumerate its own search paths, and under Seatbelt that
    /// exec follows the symlink to a location outside the venv — including the directory
    /// `pyvenv.cfg`'s `home` key names (the base prefix's own `bin`, already inside that prefix).
    /// `interpreter`'s own parent-of-parent (the venv root, e.g. a `.venv` directory) is added to
    /// `read_roots` unresolved, because pyright reads the venv's own layout (for example
    /// `pyvenv.cfg` and `site-packages`) independently of where its `python` symlink ultimately
    /// points. This function performs no process execution and no writes; it is deterministic for
    /// a fixed filesystem state, which is what its unit tests rely on.
    pub fn pyright_spec(&self, request: &CheckRequest, interpreter: &Path) -> RunSpec {
        self.pyright_spec_for_root(request, &request.worktree.clone(), interpreter)
    }

    /// Builds the [`RunSpec`] for one pyright run against the Python root `project_root` — the
    /// worktree itself, or one of `python_roots`'s nested package roots — using
    /// the already-resolved `interpreter` (as the shared resolver returns it, not yet
    /// canonicalized).
    ///
    /// `interpreter` itself — not its canonical form — is what `--pythonpath` receives: a
    /// uv-managed (or otherwise symlinked) venv's `bin/python` is a symlink to a base
    /// installation's interpreter, and Python's own venv detection keys off `pyvenv.cfg` sitting
    /// next to the symlink it was *invoked as* (`sys._base_executable`/`sys.prefix` resolution),
    /// not next to whatever that symlink resolves to. Passing the canonical (resolved) path here
    /// would make pyright run the base interpreter as if it had no venv, so it would never see
    /// the venv's `site-packages`. A `venvPath`/`venv` pin in `pyrightconfig.json`/`pyproject.toml`
    /// outranks `--pythonpath` inside pyright (the pin supplies the site-packages; pythonPath adds
    /// only non-site-packages roots), so the resolver reads that pin first and hands exactly its
    /// interpreter here: the two sources cannot disagree.
    ///
    /// `--project` is `project_root`'s own `pyrightconfig.json` when one is readable there, else
    /// `project_root` itself, so a nested package's own configuration governs its run. A nested
    /// root is also added to `read_roots` (the worktree always is). `interpreter` is canonicalized
    /// only to derive `read_roots` (falling back to the given path if canonicalization fails,
    /// which only happens if the file was removed between resolution and this call): the canonical
    /// path's installation prefix (its parent-of-parent) is added so the confinement profile
    /// covers the real interpreter binary and its standard library — pyright starts `interpreter`
    /// to enumerate its own search paths, and under Seatbelt that exec follows the symlink to a
    /// location outside the venv — including the directory `pyvenv.cfg`'s `home` key names (the
    /// base prefix's own `bin`, already inside that prefix). `interpreter`'s own parent-of-parent
    /// (the venv root, e.g. a `.venv` directory) is added to `read_roots` unresolved, because
    /// pyright reads the venv's own layout (for example `pyvenv.cfg` and `site-packages`)
    /// independently of where its `python` symlink ultimately points. This function performs no
    /// process execution and no writes; it is deterministic for a fixed filesystem state, which
    /// is what its unit tests rely on.
    pub fn pyright_spec_for_root(
        &self,
        request: &CheckRequest,
        project_root: &Path,
        interpreter: &Path,
    ) -> RunSpec {
        let canonical_interpreter = if request.read_denies.is_empty() {
            fs::canonicalize(interpreter).unwrap_or_else(|_| interpreter.to_path_buf())
        } else {
            resolved_link_target(interpreter, &request.read_denies)
                .unwrap_or_else(|| interpreter.to_path_buf())
        };

        let project = {
            let config = project_root.join(PYRIGHT_CONFIG_FILE);
            if allowed_config(&config, &request.read_denies) {
                config
            } else {
                project_root.to_path_buf()
            }
        };

        let node_bin_dir = parent_or_self(&self.node);
        let node_root = parent_or_self(&node_bin_dir);
        let pyright_root = parent_or_self(&self.pyright_cli);
        let venv_root = grandparent_or_self(interpreter);
        let base_prefix = grandparent_or_self(&canonical_interpreter);

        let tmp_dir = request.cache_dir.join("tmp");
        let home = agent_ide_core::userhome::user_home()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path_env = format!("{}:/usr/bin:/bin", node_bin_dir.display());

        let mut read_roots = vec![request.worktree.clone()];
        if project_root != request.worktree {
            read_roots.push(project_root.to_path_buf());
        }
        read_roots.extend([node_root, pyright_root, venv_root, base_prefix]);
        read_roots.push(PathBuf::from("/private/etc"));

        RunSpec {
            program: self.node.clone(),
            args: vec![
                self.pyright_cli.clone().into_os_string(),
                OsString::from("--outputjson"),
                OsString::from("--project"),
                project.into_os_string(),
                OsString::from("--pythonpath"),
                interpreter.as_os_str().to_os_string(),
            ],
            cwd: request.worktree.clone(),
            env: vec![
                ("PATH".to_string(), path_env),
                ("HOME".to_string(), home),
                ("TMPDIR".to_string(), tmp_dir.display().to_string()),
            ],
            read_roots,
            write_roots: vec![request.cache_dir.clone()],
            read_denies: request.read_denies.clone(),
            timeout: self.timeout,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

impl Checker for PythonChecker {
    fn language(&self) -> Language {
        crate::LANGUAGE
    }

    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            let generation = request.input_generation;
            if [&self.node, &self.pyright_cli]
                .iter()
                .any(|path| !allowed_file(path, &request.read_denies))
            {
                return ProblemSnapshot::unavailable(
                    crate::LANGUAGE,
                    UnavailableReason::ToolMissing,
                    generation,
                );
            }
            // One pyright run per Python root, each with the shared resolver's environment. A
            // root with none is skipped — running pyright without an interpreter would only
            // flood unresolved-import errors — and only a worktree whose every root lacks one
            // reports `EnvMissing`, carrying each root's cause and next step.
            let environments =
                crate::environment::environments(&request.worktree, &request.read_denies);
            let tmp_dir = request.cache_dir.join("tmp");
            if let Err(error) = fs::create_dir_all(&tmp_dir) {
                return ProblemSnapshot::unavailable_with_detail(
                    crate::LANGUAGE,
                    UnavailableReason::Fatal,
                    generation,
                    0,
                    run_failure_cause(error.to_string().as_bytes(), None),
                );
            }
            let started = Instant::now();
            let mut snapshots = Vec::new();
            let mut missing = Vec::new();
            for env in &environments {
                let root = crate::environment::absolute_root(&request.worktree, env);
                let Some(interpreter) = crate::environment::interpreter(env) else {
                    missing.extend(env.missing_next_step.as_ref().map(|step| {
                        if env.root.as_os_str().is_empty() {
                            step.clone()
                        } else {
                            format!("{}: {step}", env.root.display())
                        }
                    }));
                    continue;
                };
                let spec = self.pyright_spec_for_root(&request, &root, &interpreter);
                let output = match self.runner.run(spec).await {
                    Ok(output) => output,
                    Err(error) => {
                        return ProblemSnapshot::unavailable_with_detail(
                            crate::LANGUAGE,
                            UnavailableReason::Fatal,
                            generation,
                            started.elapsed().as_millis() as u64,
                            run_failure_cause(error.to_string().as_bytes(), None),
                        );
                    }
                };
                if output.timed_out {
                    return ProblemSnapshot::unavailable(
                        crate::LANGUAGE,
                        UnavailableReason::Timeout,
                        generation,
                    );
                }
                let duration_ms = started.elapsed().as_millis() as u64;
                let mut snapshot = parse_pyright_output_with_denies(
                    output.status,
                    &output.stdout,
                    &output.stderr,
                    generation,
                    duration_ms,
                    &request.worktree,
                    &request.read_denies,
                );
                relativize_paths(&mut snapshot, &request.worktree);
                snapshots.push(snapshot);
            }
            if snapshots.is_empty() {
                // Every root lacked an environment (or there was no root at all): the durable
                // condition, not a failed run.
                return ProblemSnapshot::unavailable_with_detail(
                    crate::LANGUAGE,
                    UnavailableReason::EnvMissing,
                    generation,
                    started.elapsed().as_millis() as u64,
                    (!missing.is_empty()).then(|| missing.join("; ")),
                );
            }
            if snapshots.len() == 1 {
                return snapshots.pop().expect("one snapshot checked above");
            }
            merge_root_snapshots(snapshots, generation, started.elapsed().as_millis() as u64)
        })
    }
}

/// Folds the per-root pyright snapshots of one check run into the single snapshot the scheduler
/// stores for `(worktree, python)`.
///
/// Problems are concatenated and rebuilt through [`ProblemSnapshot::from_problems`], so the
/// merged result deduplicates, counts and caps exactly like a single run's. The state is the
/// strongest any run proved: a transient `Fatal`/`Timeout` run alongside a usable `Ready`/
/// `Partial` result downgrades that result to `Partial` (that root's files are uncovered), a
/// usable result with no such run keeps its own state, and a worktree where no run was usable
/// returns the first transient failure outright. `duration_ms` is the whole run's wall clock.
fn merge_root_snapshots(
    snapshots: Vec<ProblemSnapshot>,
    generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    let mut fatal: Option<ProblemSnapshot> = None;
    let mut usable: Option<&ProblemSnapshot> = None;
    for snapshot in &snapshots {
        match &snapshot.state {
            CheckState::Ready | CheckState::Partial => {
                usable = Some(match usable {
                    Some(existing) if existing.state == CheckState::Ready => existing,
                    _ => snapshot,
                });
            }
            CheckState::Unavailable(UnavailableReason::Fatal | UnavailableReason::Timeout) => {
                fatal.get_or_insert(snapshot.clone());
            }
            CheckState::Unavailable(_) | CheckState::Checking => {}
        }
    }
    let Some(usable) = usable else {
        // No run produced a usable result: the first transient failure explains the run; with
        // none, the shared durable state (every run e.g. `NoFiles`) stands.
        return fatal.unwrap_or_else(|| snapshots[0].clone());
    };
    let state = if fatal.is_some() {
        CheckState::Partial
    } else {
        usable.state.clone()
    };
    let problems = snapshots
        .iter()
        .filter(|snapshot| matches!(snapshot.state, CheckState::Ready | CheckState::Partial))
        .flat_map(|snapshot| snapshot.problems.iter().cloned())
        .collect();
    ProblemSnapshot::from_problems(crate::LANGUAGE, state, problems, generation, duration_ms)
}

/// Returns `path`'s parent directory, or `path` itself when it has none (for example a bare
/// filename with no directory component).
fn parent_or_self(path: &Path) -> PathBuf {
    path.parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// Returns `path`'s parent's parent directory (its grandparent), or `path` itself when fewer
/// than two ancestors exist. Used to strip a `bin/<interpreter>` suffix down to its installation
/// root.
fn grandparent_or_self(path: &Path) -> PathBuf {
    path.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| path.to_path_buf())
}

/// Rewrites every [`Problem::path`] in `snapshot` relative to `worktree` in place.
///
/// A path pyright reported outside `worktree` (which should not happen, since pyright is only
/// ever pointed at the worktree's own project) is left exactly as reported rather than forced
/// into a relative form that would misrepresent it.
fn relativize_paths(snapshot: &mut ProblemSnapshot, worktree: &Path) {
    for problem in &mut snapshot.problems {
        if let Ok(relative) = Path::new(&problem.path).strip_prefix(worktree)
            && let Some(relative) = relative.to_str()
        {
            problem.path = relative.to_string();
        }
    }
}

/// The interpreter the shared resolver chooses for a worktree whose root is `root` (honouring
/// a stored selection), or `None` when nothing resolves — a pinned environment that is missing
/// included, which the checker reports as [`UnavailableReason::EnvMissing`] without running
/// pyright.
pub fn resolve_interpreter(root: &Path) -> Option<PathBuf> {
    crate::environment::interpreter(&crate::environment::resolve_root(root, root, &[]))
}

/// The interpreter of the worktree's shared Pyright session: the first project root, the
/// worktree itself first, whose environment resolves.
pub fn session_interpreter(worktree: &Path) -> Option<PathBuf> {
    crate::environment::session(worktree).0
}

/// Follows at most 32 interpreter or tool links with no-follow metadata, proving each normalized
/// hop first. Loops, denied paths, and unsafe relative targets return `None`.
fn resolved_link_target(
    path: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> Option<PathBuf> {
    let mut current = path.to_path_buf();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..32 {
        if denied_interpreter_path(&current, denies) || !seen.insert(current.clone()) {
            return None;
        }
        let metadata = fs::symlink_metadata(&current).ok()?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&current).ok()?;
            current = lexical_link_target(&current, &target, denies)?;
        } else {
            return metadata.is_file().then_some(current);
        }
    }
    None
}

/// Resolves a link target lexically without following links; each `..` may remove only a real,
/// allowed directory (never a symlink, denied path, or missing name), so normalization cannot hide
/// a symlink or denied path from the next hop check. Returns the normalized absolute target, or
/// `None` for a relative result, a Windows prefix, a link without a parent, or any component that
/// cannot be proved — resolution fails closed.
fn lexical_link_target(
    link: &Path,
    target: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> Option<PathBuf> {
    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent()?.join(target)
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::RootDir => normalized.push("/"),
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => normalized.push(name),
            std::path::Component::ParentDir => {
                if denied_interpreter_path(&normalized, denies)
                    || !fs::symlink_metadata(&normalized).ok()?.is_dir()
                    || !normalized.pop()
                {
                    return None;
                }
            }
            std::path::Component::Prefix(_) => return None,
        }
    }
    normalized.is_absolute().then_some(normalized)
}

/// Accepts an existing regular file without following an unproved link under host read denies.
pub(crate) fn allowed_file(
    path: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> bool {
    if denies.is_empty() {
        path.is_file()
    } else {
        resolved_link_target(path, denies).is_some()
    }
}

/// Accepts a project config only when its final component is a regular file, not a link.
fn allowed_config(path: &Path, denies: &[agent_ide_core::execution::seatbelt::ReadDeny]) -> bool {
    if denies.is_empty() {
        return path.is_file();
    }
    !denies.iter().any(|deny| deny.matches(path))
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Reads a project config through an `O_NOFOLLOW` descriptor under host read exclusions.
pub(crate) fn read_config(
    path: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> Option<String> {
    if denies.is_empty() {
        return fs::read_to_string(path).ok();
    }
    if !allowed_config(path, denies) {
        return None;
    }
    let mut text = String::new();
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?
        .read_to_string(&mut text)
        .ok()?;
    Some(text)
}

/// Rejects a denied interpreter path or a symlinked parent before any following `is_file` probe.
fn denied_interpreter_path(
    path: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> bool {
    if denies.is_empty() {
        return false;
    }
    if path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return true;
    }
    if denies.iter().any(|deny| deny.matches(path)) {
        return true;
    }
    path.parent().is_none_or(|parent| {
        parent.ancestors().any(|prefix| {
            denies.iter().any(|deny| deny.matches(prefix))
                || match std::fs::symlink_metadata(prefix) {
                    Ok(metadata) => metadata.file_type().is_symlink(),
                    Err(error) => error.kind() != std::io::ErrorKind::NotFound,
                }
        })
    })
}

/// The two keys of a pyright JSON config this checker reads; every other key is ignored.
#[derive(Debug, Deserialize)]
struct PyrightConfigVenvKeys {
    /// Directory containing named virtual environments, relative to the config file's directory
    /// unless absolute.
    #[serde(rename = "venvPath")]
    venv_path: Option<String>,
    /// Name of the virtual environment directory under `venv_path`.
    venv: Option<String>,
}

/// Reads `venvPath`/`venv` from `<worktree>/pyrightconfig.json`.
///
/// Under host denies the read uses `O_NOFOLLOW`; denied or linked configs, absent or invalid JSON,
/// and configs without both keys return `None`.
pub(crate) fn read_pyrightconfig_venv_keys(
    worktree: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> Option<(String, String)> {
    let text = read_config(&worktree.join(PYRIGHT_CONFIG_FILE), denies)?;
    let config: PyrightConfigVenvKeys = serde_json::from_str(&text).ok()?;
    match (config.venv_path, config.venv) {
        (Some(venv_path), Some(venv)) => Some((venv_path, venv)),
        _ => None,
    }
}

/// Reads `venvPath`/`venv` from the `[tool.pyright]` table of `<worktree>/pyproject.toml`.
///
/// No TOML crate is vendored in this workspace's `Cargo.lock`, so this is a minimal, strict,
/// line-oriented parser scoped to exactly the two keys this checker needs: it tracks the current
/// `[section]` header (trimmed, matched byte-exact against `tool.pyright`) and, once inside that
/// section, matches lines of the exact form `key = "value"` (a single line, a double-quoted value
/// with no escape sequences, an optional trailing `# …` comment). Any other TOML construct in that
/// position — multi-line strings, arrays, inline tables, single-quoted strings, dotted keys,
/// escaped characters — is not recognized as a value for that key, so a project that uses one of
/// those forms for `venvPath`/`venv` is treated as not specifying them (falls through to the next
/// resolution source) rather than being mis-parsed into a wrong path. Under host denies the read
/// uses `O_NOFOLLOW` and refuses denied or linked config files.
pub(crate) fn read_pyproject_venv_keys(
    worktree: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> Option<(String, String)> {
    let text = read_config(&worktree.join(PYPROJECT_FILE), denies)?;
    let mut in_target_section = false;
    let mut venv_path: Option<String> = None;
    let mut venv: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_target_section = trimmed[1..trimmed.len() - 1].trim() == "tool.pyright";
            continue;
        }
        if !in_target_section {
            continue;
        }
        if let Some((key, value)) = parse_strict_string_assignment(trimmed) {
            match key {
                "venvPath" => venv_path = Some(value),
                "venv" => venv = Some(value),
                _ => {}
            }
        }
    }
    match (venv_path, venv) {
        (Some(venv_path), Some(venv)) => Some((venv_path, venv)),
        _ => None,
    }
}

/// Parses one strict `key = "value"` line as described on [`read_pyproject_venv_keys`].
///
/// Returns `None` for a blank line, a comment-only line, a key containing whitespace (so a
/// dotted-key or table-header fragment is rejected rather than guessed at), a non-double-quoted
/// value, or a quoted value containing an embedded `"` or `\`.
fn parse_strict_string_assignment(line: &str) -> Option<(&str, String)> {
    let (key, rest) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let value = rest.trim();
    let value = match value.split_once('#') {
        Some((before, _)) => before.trim(),
        None => value,
    };
    let inner = value.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains('"') || inner.contains('\\') {
        return None;
    }
    Some((key, inner.to_string()))
}

/// One `pyright --outputjson` report, restricted to the fields this checker reads.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PyrightReport {
    /// Every reported diagnostic, in pyright's own order.
    general_diagnostics: Vec<PyrightDiagnostic>,
    /// Aggregate counts used to cross-check `general_diagnostics` and to detect a report that
    /// analyzed no files.
    summary: PyrightSummary,
}

/// One entry of `PyrightReport::general_diagnostics`.
#[derive(Debug, Deserialize)]
struct PyrightDiagnostic {
    /// Absolute path of the analyzed file, as pyright reports it.
    file: String,
    /// `"error"`, `"warning"`, `"information"`, or `"hint"`; only the first two become a
    /// [`Problem`], matching EYES-r2 §4.
    severity: String,
    /// Human-readable diagnostic text; untrusted, carried into [`Problem::message`] as-is (the
    /// [`Problem::new`] constructor applies the shared length cap).
    message: String,
    /// Zero-based start position of the diagnostic span.
    range: PyrightRange,
    /// Pyright rule identifier (for example `reportMissingImports`), when the diagnostic has one.
    rule: Option<String>,
}

/// The `range` object of one pyright diagnostic; only its start position is used.
#[derive(Debug, Deserialize)]
struct PyrightRange {
    /// Zero-based start line/character of the diagnostic span.
    start: PyrightPosition,
}

/// A zero-based line/character position in a pyright diagnostic range.
#[derive(Debug, Deserialize)]
struct PyrightPosition {
    /// Zero-based line number; [`parse_pyright_output`] reports it 1-based in [`Problem::line`].
    line: u32,
    /// Zero-based character offset; [`parse_pyright_output`] reports it 1-based in
    /// [`Problem::column`].
    character: u32,
}

/// The `summary` object of a pyright report.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PyrightSummary {
    /// Total error-severity diagnostics pyright counted; must equal the tally this checker takes
    /// from `general_diagnostics` or the report is treated as untrustworthy.
    error_count: u32,
    /// Total warning-severity diagnostics pyright counted; same cross-check as `error_count`.
    warning_count: u32,
    /// Number of files pyright actually analyzed; `0` signals a report produced without a usable
    /// project (for example an interpreter with no resolvable standard library).
    files_analyzed: u32,
}

/// Turns one completed pyright run into a [`ProblemSnapshot`], per EYES-r2 §4 ("Python").
///
/// `exit` is the process exit code (`None` when the process was killed by a signal; the caller
/// is expected to have already handled a timeout kill separately and never call this function for
/// one). `stdout` is the captured `--outputjson` report; `stderr` is only read for a failure
/// cause. Only exit `0` or `1` with a parseable report are accepted; any other exit code, or a
/// report `serde_json` cannot parse, produces [`UnavailableReason::Fatal`] carrying the run's
/// cause ([`run_failure_cause`]: first `error:` line of `stderr`, else its first non-empty line,
/// else `exit <status>`) so a refused sandbox apply or a crashed node says why. A parsed report
/// whose `summary.filesAnalyzed` is `0` produces [`UnavailableReason::NoFiles`] with a `detail`
/// pointing at the project's `include`/`exclude` configuration: pyright ran against a resolved
/// environment but had nothing to analyze, which is a project-configuration problem, not a
/// missing interpreter ([`UnavailableReason::EnvMissing`] stays reserved for that). A parsed
/// report whose raw error/warning tally does not equal `summary.errorCount`/`warningCount`
/// produces [`UnavailableReason::Fatal`], since a report that disagrees with its own summary is
/// not safe to trust. Otherwise returns a `Ready` snapshot built by [`ProblemSnapshot::from_problems`]
/// (which deduplicates, sorts, and caps the retained problem list; `errors`/`warnings` still count
/// every raw error/warning diagnostic). Reported paths are pyright's own absolute paths; the
/// caller relativizes them against the worktree afterward.
pub fn parse_pyright_output(
    exit: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    input_generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    parse_pyright_output_with_denies(
        exit,
        stdout,
        stderr,
        input_generation,
        duration_ms,
        Path::new(""),
        &[],
    )
}

/// Parses a check report while discarding host-denied diagnostic paths before counting and cap.
fn parse_pyright_output_with_denies(
    exit: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    input_generation: u64,
    duration_ms: u64,
    worktree: &Path,
    denies: &[agent_ide_core::execution::seatbelt::ReadDeny],
) -> ProblemSnapshot {
    if !matches!(exit, Some(0) | Some(1)) {
        return ProblemSnapshot::unavailable_with_detail(
            crate::LANGUAGE,
            UnavailableReason::Fatal,
            input_generation,
            duration_ms,
            run_failure_cause(stderr, exit),
        );
    }
    let Ok(report) = serde_json::from_slice::<PyrightReport>(stdout) else {
        return ProblemSnapshot::unavailable_with_detail(
            crate::LANGUAGE,
            UnavailableReason::Fatal,
            input_generation,
            duration_ms,
            run_failure_cause(stderr, exit),
        );
    };
    if report.summary.files_analyzed == 0 {
        return ProblemSnapshot::unavailable_with_detail(
            crate::LANGUAGE,
            UnavailableReason::NoFiles,
            input_generation,
            duration_ms,
            Some(NO_FILES_DETAIL.to_owned()),
        );
    }

    let mut problems = Vec::with_capacity(report.general_diagnostics.len());
    let mut errors: u32 = 0;
    let mut warnings: u32 = 0;
    for diagnostic in report.general_diagnostics {
        let severity = match diagnostic.severity.as_str() {
            "error" => {
                errors += 1;
                Severity::Error
            }
            "warning" => {
                warnings += 1;
                Severity::Warning
            }
            _ => continue,
        };
        if !agent_ide_core::checks::check_problem_path_allowed(worktree, &diagnostic.file, denies) {
            continue;
        }
        problems.push(Problem::new(
            diagnostic.file,
            diagnostic.range.start.line + 1,
            diagnostic.range.start.character + 1,
            severity,
            diagnostic.rule,
            diagnostic.message,
        ));
    }

    if errors != report.summary.error_count || warnings != report.summary.warning_count {
        return ProblemSnapshot::unavailable_with_detail(
            crate::LANGUAGE,
            UnavailableReason::Fatal,
            input_generation,
            duration_ms,
            run_failure_cause(stderr, exit),
        );
    }

    ProblemSnapshot::from_problems(
        crate::LANGUAGE,
        CheckState::Ready,
        problems,
        input_generation,
        duration_ms,
    )
}

/// Python's project-check integration.
pub struct PythonChecks;

impl LanguageChecks for PythonChecks {
    /// Python is present iff the worktree matches the shared marker rule
    /// (`is_python_project`, the same list the project card uses): root
    /// `pyproject.toml`/`setup.py`/`setup.cfg`/`Pipfile`/`pyrightconfig.json`/`requirements*.txt`,
    /// a `.venv`/`venv` directory, or any of the nested package roots
    /// `python_roots` discovers from manifests two levels down. This
    /// deliberately never walks the tree for source files.
    fn is_present(&self, worktree: &Path) -> bool {
        crate::support::is_python_project(worktree)
    }

    /// A Python file below no discovered root is never analyzed, and so is a file whose only
    /// covering root declares no environment (its pyright run is skipped, not run half-blind).
    /// `path` may arrive worktree-relative (the edit reply names files that way); it is joined
    /// against `worktree` before the roots are compared.
    fn not_analysed(&self, worktree: &Path, path: &Path) -> Option<&'static str> {
        let roots = crate::support::python_roots(worktree);
        if roots.is_empty() {
            return None;
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            worktree.join(path)
        };
        let covering: Vec<&PathBuf> = roots
            .iter()
            .filter(|root| absolute.starts_with(root.as_path()))
            .collect();
        if covering.is_empty() {
            return Some("no Python project root covers this file");
        }
        covering
            .iter()
            .all(|root| {
                crate::environment::resolve_root(worktree, root, &[])
                    .chosen
                    .is_none()
            })
            .then_some("the Python project root beside this file has no environment")
    }

    /// `pyright`.
    fn tool_name(&self) -> &'static str {
        "pyright"
    }

    /// Decodes [`ProjectPythonChecksConfig`].
    fn parse_config(
        &self,
        section: serde_json::Value,
    ) -> Result<Arc<dyn CheckConfig>, serde_json::Error> {
        let config: ProjectPythonChecksConfig = serde_json::from_value(section)?;
        Ok(Arc::new(config))
    }
}

/// Accepted Python toolchain declaration for confined background project checks.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPythonChecksConfig {
    /// Absolute normalized Node executable; the only program allowed to start confined Pyright.
    node: PathBuf,
    /// Absolute normalized Pyright CLI entry module executed by `node`.
    pyright_cli: PathBuf,
}

impl ProjectPythonChecksConfig {
    /// Returns the declared absolute Node executable path.
    pub fn node(&self) -> &Path {
        &self.node
    }
    /// Returns the declared absolute Pyright CLI entry module path.
    pub fn pyright_cli(&self) -> &Path {
        &self.pyright_cli
    }
}

impl CheckConfig for ProjectPythonChecksConfig {
    /// Rejects a relative or lexically non-normal tool path.
    fn validate(&self) -> bool {
        absolute(&self.node) && absolute(&self.pyright_cli)
    }

    /// Builds the confined Pyright runner for these tools.
    fn checker(&self, runner: Arc<dyn ConfinedRunner>, timeout: Duration) -> Arc<dyn Checker> {
        Arc::new(PythonChecker::new(
            runner,
            self.node.clone(),
            self.pyright_cli.clone(),
            timeout,
        ))
    }

    /// Node, then the Pyright CLI run by Node.
    fn programs(&self) -> Vec<(PathBuf, Option<PathBuf>)> {
        vec![
            (self.node.clone(), None),
            (self.pyright_cli.clone(), Some(self.node.clone())),
        ]
    }
}

#[cfg(test)]
mod deny_tests {
    use super::*;
    use agent_ide_core::execution::seatbelt::{CredentialGlob, ReadDeny};
    use std::os::unix::fs::symlink;

    /// A second interpreter link and symlinked project configs never follow into a denied file.
    #[test]
    fn deny_policy_checks_every_interpreter_hop_and_config_open() {
        let root =
            std::env::temp_dir().join(format!("agent-ide-python-deny-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let secret = root.join("secret.key");
        std::fs::write(&secret, "secret").unwrap();
        symlink("python3", root.join(".venv/bin/python")).unwrap();
        symlink(&secret, root.join(".venv/bin/python3")).unwrap();
        symlink(&secret, root.join(PYRIGHT_CONFIG_FILE)).unwrap();
        symlink(&secret, root.join(PYPROJECT_FILE)).unwrap();
        let denies = [ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Key,
        }];
        let resolved = |denies: &[ReadDeny]| {
            crate::environment::interpreter(&crate::environment::resolve_with_denies(
                &root, &root, None, denies,
            ))
        };
        assert_eq!(resolved(&denies), None);
        assert_eq!(read_config(&root.join(PYRIGHT_CONFIG_FILE), &denies), None);
        assert_eq!(read_config(&root.join(PYPROJECT_FILE), &denies), None);
        std::fs::remove_file(root.join(".venv/bin/python3")).unwrap();
        let allowed = root.join("python-real");
        std::fs::write(&allowed, "allowed").unwrap();
        symlink(&allowed, root.join(".venv/bin/python3")).unwrap();
        assert_eq!(resolved(&denies), Some(root.join(".venv/bin/python")));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Creates one fresh canonicalized fixture root for a relative-link test.
    ///
    /// Canonicalization moves the root onto the `/private` spelling of the system temp tree, so
    /// the deny machinery's symlinked-ancestor rule never rejects these fixtures for the host's
    /// `/var` -> `private/var` link instead of the property under test.
    fn link_fixture_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "agent-ide-python-links-{}-{}",
            std::process::id(),
            label
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    /// A Homebrew-style relative link (`bin/node -> ../Cellar/node/<v>/bin/node`) resolves through
    /// its `..` hop when every popped component is a real, allowed directory.
    #[test]
    fn deny_resolves_relative_homebrew_style_link_chain() {
        let root = link_fixture_root("homebrew");
        std::fs::create_dir_all(root.join("Cellar/node/1.2.3/bin")).unwrap();
        std::fs::write(root.join("Cellar/node/1.2.3/bin/node"), "node").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        symlink("../Cellar/node/1.2.3/bin/node", root.join("bin/node")).unwrap();
        let denies = [ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Key,
        }];
        assert_eq!(
            resolved_link_target(&root.join("bin/node"), &denies),
            Some(root.join("Cellar/node/1.2.3/bin/node"))
        );
        assert!(allowed_file(&root.join("bin/node"), &denies));
        let _ = std::fs::remove_dir_all(root);
    }

    /// A relative link whose normalized target sits inside a denied directory, or whose `..` pops
    /// a denied directory itself, never resolves: the deny cannot be normalized away.
    #[test]
    fn deny_rejects_relative_link_over_denied_hop() {
        let root = link_fixture_root("denied-hop");
        std::fs::create_dir_all(root.join("Cellar/node/1.2.3/bin")).unwrap();
        std::fs::write(root.join("Cellar/node/1.2.3/bin/node"), "node").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        symlink("../Cellar/node/1.2.3/bin/node", root.join("bin/node")).unwrap();
        std::fs::create_dir_all(root.join("keys")).unwrap();
        symlink("../keys/../node", root.join("bin/node2")).unwrap();
        let denies = [
            ReadDeny::Path(root.join("Cellar")),
            ReadDeny::Path(root.join("keys")),
        ];
        assert_eq!(resolved_link_target(&root.join("bin/node"), &denies), None);
        assert!(!allowed_file(&root.join("bin/node"), &denies));
        assert_eq!(resolved_link_target(&root.join("bin/node2"), &denies), None);
        let _ = std::fs::remove_dir_all(root);
    }

    /// A relative two-link loop (`python -> ../bin/python2 -> ../bin/python`) is rejected by the
    /// seen-set instead of iterating until the hop budget runs out.
    #[test]
    fn deny_rejects_relative_link_loop() {
        let root = link_fixture_root("loop");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        symlink("../bin/python2", root.join("bin/python")).unwrap();
        symlink("../bin/python", root.join("bin/python2")).unwrap();
        let denies = [ReadDeny::Glob {
            base: root.clone(),
            suffix: CredentialGlob::Key,
        }];
        assert_eq!(
            resolved_link_target(&root.join("bin/python"), &denies),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A `..` that would pop a symlinked directory is refused: physically the kernel resolves the
    /// symlink first, so lexical normalization (`bin/sub/../node` -> `bin/node`) would hide the
    /// symlink's real target — here a file under the denied `secret` tree — behind an allowed
    /// lexical path. The symlinked `sub` is invisible to every later hop check, so only the pop
    /// check can refuse it.
    #[test]
    fn deny_rejects_dotdot_popping_a_symlinked_directory() {
        let root = link_fixture_root("symlink-dotdot");
        std::fs::create_dir_all(root.join("secret/inner")).unwrap();
        std::fs::write(root.join("secret/node"), "secret").unwrap();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        symlink(root.join("secret/inner"), root.join("bin/sub")).unwrap();
        symlink("sub/../node", root.join("bin/node")).unwrap();
        let denies = [ReadDeny::Path(root.join("secret"))];
        assert_eq!(resolved_link_target(&root.join("bin/node"), &denies), None);
        assert!(!allowed_file(&root.join("bin/node"), &denies));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Builds a fresh empty scratch directory for presence-detection tests.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "agent-ide-checks-presence-{}-{name}-{}",
            std::process::id(),
            name.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir created");
        dir
    }

    #[test]
    fn is_present_python_accepts_any_shared_marker() {
        for marker in [
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "Pipfile",
            "pyrightconfig.json",
        ] {
            let dir = scratch_dir(&format!("python-presence-{marker}"));
            assert!(!PythonChecks.is_present(&dir), "{marker}");
            std::fs::write(dir.join(marker), "").unwrap();
            assert!(PythonChecks.is_present(&dir), "{marker}");
        }
        // The glob, not an exact name: a root `requirements-dev.txt` used to register the card
        // while checks stayed silent because the old presence list matched `requirements.txt`
        // only.
        for name in [
            "requirements.txt",
            "requirements-dev.txt",
            "requirements-ml.txt",
        ] {
            let dir = scratch_dir(&format!("python-presence-{name}"));
            assert!(!PythonChecks.is_present(&dir), "{name}");
            std::fs::write(dir.join(name), "").unwrap();
            assert!(PythonChecks.is_present(&dir), "{name}");
        }
        for venv_name in [".venv", "venv"] {
            let dir = scratch_dir(&format!("python-presence-{venv_name}"));
            assert!(!PythonChecks.is_present(&dir), "{venv_name}");
            std::fs::create_dir(dir.join(venv_name)).unwrap();
            assert!(PythonChecks.is_present(&dir), "{venv_name}");
        }
    }

    /// The bounded nested-root probe: a script directory such as `tools/` carrying
    /// `requirements-ml.txt` or a nested `pyproject.toml` registers Python when the same
    /// subdirectory holds at least one `.py` file; a marker alone — a Sphinx
    /// `docs/requirements.txt` in a non-Python repository — does not. The probe never recurses
    /// (`nested/deep/requirements.txt` stays invisible) and skips hidden and vendor directories.
    #[test]
    fn is_present_python_accepts_nested_markers_only() {
        let dir = scratch_dir("python-presence-nested-tools");
        assert!(!PythonChecks.is_present(&dir));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools/requirements-ml.txt"), "").unwrap();
        assert!(
            !PythonChecks.is_present(&dir),
            "tools/requirements-ml.txt alone does not register"
        );
        std::fs::write(dir.join("tools/analyze.py"), "import pandas\n").unwrap();
        assert!(
            PythonChecks.is_present(&dir),
            "tools/requirements-ml.txt beside tools/analyze.py registers"
        );

        let dir = scratch_dir("python-presence-nested-pyproject");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/pyproject.toml"), "").unwrap();
        std::fs::write(dir.join("sub/lib.py"), "").unwrap();
        assert!(
            PythonChecks.is_present(&dir),
            "sub/pyproject.toml beside sub/lib.py registers"
        );

        let dir = scratch_dir("python-presence-docs-requirements");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/requirements.txt"), "sphinx\n").unwrap();
        assert!(
            !PythonChecks.is_present(&dir),
            "a requirements.txt with no .py file beside it does not register"
        );

        let dir = scratch_dir("python-presence-nested-deep");
        std::fs::create_dir_all(dir.join("nested/deep")).unwrap();
        std::fs::write(dir.join("nested/deep/requirements.txt"), "").unwrap();
        std::fs::write(dir.join("nested/deep/lib.py"), "").unwrap();
        assert!(
            PythonChecks.is_present(&dir),
            "a manifest two levels down beside a .py file registers as a nested root"
        );
    }

    #[test]
    fn is_present_python_is_false_on_an_empty_worktree() {
        let dir = scratch_dir("empty-python-presence");
        assert!(!PythonChecks.is_present(&dir));
    }

    /// Resolves a root `.venv` without requiring Python project manifests.
    #[test]
    fn resolve_interpreter_finds_root_venv_without_manifest() {
        let dir = scratch_dir("venv-without-manifest");
        let interpreter = dir.join(".venv/bin/python");
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, "").unwrap();

        assert_eq!(resolve_interpreter(&dir), Some(interpreter));

        let _ = std::fs::remove_dir_all(dir);
    }
}
