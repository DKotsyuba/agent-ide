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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::runner::{ConfinedRunner, RunSpec};
use super::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, Problem, ProblemSnapshot, Severity,
    UnavailableReason,
};

/// Name of the pyright project config file consulted at a worktree's root.
const PYRIGHT_CONFIG_FILE: &str = "pyrightconfig.json";

/// Name of the PEP 518 project file whose `[tool.pyright]` table is the second interpreter
/// resolution source.
const PYPROJECT_FILE: &str = "pyproject.toml";

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
    /// `interpreter` (as returned by [`resolve_interpreter`], not yet canonicalized).
    ///
    /// `interpreter` itself — not its canonical form — is what `--pythonpath` receives: a
    /// uv-managed (or otherwise symlinked) venv's `bin/python` is a symlink to a base
    /// installation's interpreter, and Python's own venv detection keys off `pyvenv.cfg` sitting
    /// next to the symlink it was *invoked as* (`sys._base_executable`/`sys.prefix` resolution),
    /// not next to whatever that symlink resolves to. Passing the canonical (resolved) path here
    /// would make pyright run the base interpreter as if it had no venv, so it would never see
    /// the venv's `site-packages`. `interpreter` still takes precedence over any `venvPath`/`venv`
    /// pyright would otherwise read from `pyrightconfig.json`/`pyproject.toml` itself: pyright
    /// gives an explicit `--pythonpath` priority over its own config-driven venv resolution, so
    /// the two sources cannot disagree here.
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
        let canonical_interpreter =
            fs::canonicalize(interpreter).unwrap_or_else(|_| interpreter.to_path_buf());

        let project = {
            let config = request.worktree.join(PYRIGHT_CONFIG_FILE);
            if !request.read_denies.iter().any(|deny| deny.matches(&config)) && config.is_file() {
                config
            } else {
                request.worktree.clone()
            }
        };

        let node_bin_dir = parent_or_self(&self.node);
        let node_root = parent_or_self(&node_bin_dir);
        let pyright_root = parent_or_self(&self.pyright_cli);
        let venv_root = grandparent_or_self(interpreter);
        let base_prefix = grandparent_or_self(&canonical_interpreter);

        let tmp_dir = request.cache_dir.join("tmp");
        let home = crate::userhome::user_home()
            .map(|home| home.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path_env = format!("{}:/usr/bin:/bin", node_bin_dir.display());

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
            read_roots: vec![
                request.worktree.clone(),
                node_root,
                pyright_root,
                venv_root,
                base_prefix,
                PathBuf::from("/private/etc"),
            ],
            write_roots: vec![request.cache_dir.clone()],
            read_denies: request.read_denies.clone(),
            timeout: self.timeout,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

impl Checker for PythonChecker {
    fn language(&self) -> Language {
        Language::Python
    }

    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            let generation = request.input_generation;
            if [&self.node, &self.pyright_cli].iter().any(|path| {
                request.read_denies.iter().any(|deny| deny.matches(path)) || !path.is_file()
            }) {
                return ProblemSnapshot::unavailable(
                    Language::Python,
                    UnavailableReason::ToolMissing,
                    generation,
                );
            }
            let Some(interpreter) =
                resolve_interpreter_with_denies(&request.worktree, &request.read_denies)
            else {
                return ProblemSnapshot::unavailable(
                    Language::Python,
                    UnavailableReason::EnvMissing,
                    generation,
                );
            };
            let tmp_dir = request.cache_dir.join("tmp");
            if fs::create_dir_all(&tmp_dir).is_err() {
                return ProblemSnapshot::unavailable(
                    Language::Python,
                    UnavailableReason::Fatal,
                    generation,
                );
            }
            let spec = self.pyright_spec(&request, &interpreter);
            let started = Instant::now();
            let output = match self.runner.run(spec).await {
                Ok(output) => output,
                Err(_) => {
                    return ProblemSnapshot::unavailable(
                        Language::Python,
                        UnavailableReason::Fatal,
                        generation,
                    );
                }
            };
            if output.timed_out {
                return ProblemSnapshot::unavailable(
                    Language::Python,
                    UnavailableReason::Timeout,
                    generation,
                );
            }
            let duration_ms = started.elapsed().as_millis() as u64;
            let mut snapshot = parse_pyright_output_with_denies(
                output.status,
                &output.stdout,
                generation,
                duration_ms,
                &request.worktree,
                &request.read_denies,
            );
            relativize_paths(&mut snapshot, &request.worktree);
            snapshot
        })
    }
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

/// Resolves the Python interpreter pyright should use for `worktree`, per EYES-r2 §4.
///
/// Tries, in order: (1) `venvPath`/`venv` from `<worktree>/pyrightconfig.json`; (2) the same two
/// keys from the `[tool.pyright]` table of `<worktree>/pyproject.toml`; (3)
/// `<worktree>/.venv/bin/python`. Each source is authoritative once it defines both keys: if the
/// resulting `<venvPath>/<venv>/bin/python` does not exist as a file, resolution stops there and
/// returns `None` rather than silently falling through to a later source that might resolve to a
/// different, unintended interpreter. Returns `None` when no source names an existing interpreter
/// file, which the checker maps to [`UnavailableReason::EnvMissing`] without ever invoking
/// pyright (a missing environment must never produce the flood of unresolved-import errors that
/// running pyright without a venv would report).
pub fn resolve_interpreter(worktree: &Path) -> Option<PathBuf> {
    resolve_interpreter_with_denies(worktree, &[])
}

/// Resolves the interpreter without probing any host-denied config or executable path.
fn resolve_interpreter_with_denies(
    worktree: &Path,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> Option<PathBuf> {
    if [PYRIGHT_CONFIG_FILE, PYPROJECT_FILE]
        .iter()
        .any(|name| denies.iter().any(|deny| deny.matches(&worktree.join(name))))
    {
        return None;
    }
    if let Some((venv_path, venv)) = read_pyrightconfig_venv_keys(worktree) {
        return existing_python(venv_interpreter_path(worktree, &venv_path, &venv), denies);
    }
    if let Some((venv_path, venv)) = read_pyproject_venv_keys(worktree) {
        return existing_python(venv_interpreter_path(worktree, &venv_path, &venv), denies);
    }
    existing_python(worktree.join(".venv").join("bin").join("python"), denies)
}

/// Joins `venvPath`/`venv` into the `bin/python` interpreter path they name.
///
/// `venv_path` is resolved relative to `worktree` when it is not already absolute, matching
/// pyright's own resolution of a config-relative `venvPath`.
fn venv_interpreter_path(worktree: &Path, venv_path: &str, venv: &str) -> PathBuf {
    let base = Path::new(venv_path);
    let base = if base.is_absolute() {
        base.to_path_buf()
    } else {
        worktree.join(base)
    };
    base.join(venv).join("bin").join("python")
}

/// Returns an existing interpreter outside host denies, including a direct venv symlink when
/// its target is allowed; denied paths and symlinked parents return `None`.
fn existing_python(
    candidate: PathBuf,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> Option<PathBuf> {
    if denied_interpreter_path(&candidate, denies) {
        return None;
    }
    if let Ok(target) = fs::read_link(&candidate) {
        let target = if target.is_absolute() {
            target
        } else {
            candidate.parent()?.join(target)
        };
        if denied_interpreter_path(&target, denies) {
            return None;
        }
    }
    if candidate.is_file() {
        Some(candidate)
    } else {
        None
    }
}

/// Rejects a denied interpreter path or a symlinked parent before any following `is_file` probe.
fn denied_interpreter_path(path: &Path, denies: &[crate::execution::seatbelt::ReadDeny]) -> bool {
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
/// Returns `None` when the file is absent, is not valid JSON, or does not define both keys.
fn read_pyrightconfig_venv_keys(worktree: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(worktree.join(PYRIGHT_CONFIG_FILE)).ok()?;
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
/// resolution source) rather than being mis-parsed into a wrong path.
fn read_pyproject_venv_keys(worktree: &Path) -> Option<(String, String)> {
    let text = fs::read_to_string(worktree.join(PYPROJECT_FILE)).ok()?;
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
/// one). `stdout` is the captured `--outputjson` report. Only exit `0` or `1` with a parseable
/// report are accepted; any other exit code, or a report `serde_json` cannot parse, produces
/// [`UnavailableReason::Fatal`] without inspecting `stdout` further. A parsed report whose
/// `summary.filesAnalyzed` is `0` produces [`UnavailableReason::NoFiles`] with a `detail`
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
    input_generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    parse_pyright_output_with_denies(
        exit,
        stdout,
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
    input_generation: u64,
    duration_ms: u64,
    worktree: &Path,
    denies: &[crate::execution::seatbelt::ReadDeny],
) -> ProblemSnapshot {
    if !matches!(exit, Some(0) | Some(1)) {
        return ProblemSnapshot::unavailable(
            Language::Python,
            UnavailableReason::Fatal,
            input_generation,
        );
    }
    let Ok(report) = serde_json::from_slice::<PyrightReport>(stdout) else {
        return ProblemSnapshot::unavailable(
            Language::Python,
            UnavailableReason::Fatal,
            input_generation,
        );
    };
    if report.summary.files_analyzed == 0 {
        return ProblemSnapshot::unavailable_with_detail(
            Language::Python,
            UnavailableReason::NoFiles,
            input_generation,
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
        if !super::check_problem_path_allowed(worktree, &diagnostic.file, denies) {
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
        return ProblemSnapshot::unavailable(
            Language::Python,
            UnavailableReason::Fatal,
            input_generation,
        );
    }

    ProblemSnapshot::from_problems(
        Language::Python,
        CheckState::Ready,
        problems,
        input_generation,
        duration_ms,
    )
}
