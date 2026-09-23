//! Confined root-config TypeScript and JavaScript project checks.
//!
//! The pinned `tsc.js` CLI supplies diagnostics, a complete file list, and a diagnostics footer.
//! A snapshot is ready only when all three agree and at least one worktree file was analyzed.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::runner::{ConfinedRunner, RunOutput, RunSpec};
use super::{
    BoxFuture, CheckRequest, CheckState, Checker, Language, Problem, ProblemSnapshot, Severity,
    UnavailableReason,
};
use crate::execution::seatbelt::ReadDeny;

/// Maximum bytes captured from either CLI stream; a larger report fails closed.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// ponytail: Bound deny preflight work; larger trees stay unavailable until a cheaper source inventory exists.
const MAX_POLICY_ENTRIES: usize = 100_000;
/// Ordered footer labels emitted by pinned TypeScript 5.9.3 with `--diagnostics`.
const FOOTER: [&str; 13] = [
    "Files",
    "Lines",
    "Identifiers",
    "Symbols",
    "Types",
    "Instantiations",
    "Memory used",
    "I/O read",
    "I/O write",
    "Parse time",
    "Bind time",
    "Check time",
    "Emit time",
];
/// Final footer label proving the CLI completed its report.
const TOTAL_TIME: &str = "Total time";

/// Runs a root `tsconfig.json` or `jsconfig.json` under the shared confinement seam.
///
/// Instances own immutable tool paths and a timeout. Each run checks config/tool admission,
/// writes only to its private cache, and reports `Unavailable` when coverage cannot be proved.
pub struct TypeScriptChecker {
    /// Process execution seam, shared with the other project checkers.
    runner: Arc<dyn ConfinedRunner>,
    /// Configured absolute Node executable.
    node: PathBuf,
    /// Configured absolute pinned TypeScript CLI module.
    tsc_cli: PathBuf,
    /// Maximum wall time for one confined CLI run.
    timeout: Duration,
}

impl TypeScriptChecker {
    /// Builds one reusable checker; path availability is checked on each invocation.
    pub fn new(
        runner: Arc<dyn ConfinedRunner>,
        node: PathBuf,
        tsc_cli: PathBuf,
        timeout: Duration,
    ) -> Self {
        Self {
            runner,
            node,
            tsc_cli,
            timeout,
        }
    }

    /// Describes one CLI run with writes restricted to `request.cache_dir`.
    ///
    /// `config` is the already admitted regular root config; the caller must not pass a link.
    /// The Node executable and separate TypeScript package are readable, even when they live in
    /// different directory trees; system paths are already granted by Seatbelt. CLI flags override project options that would
    /// write build metadata or replace the bounded diagnostics/file-list output format.
    pub fn run_spec(&self, request: &CheckRequest, config: &Path) -> RunSpec {
        let typescript_root = self
            .tsc_cli
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.tsc_cli);
        let tmp = request.cache_dir.join("tmp");
        RunSpec {
            program: self.node.clone(),
            args: vec![
                self.tsc_cli.clone().into_os_string(),
                OsString::from("--project"),
                config.as_os_str().to_os_string(),
                OsString::from("--pretty"),
                OsString::from("false"),
                OsString::from("--diagnostics"),
                OsString::from("--listFiles"),
                OsString::from("--noEmit"),
                OsString::from("--incremental"),
                OsString::from("false"),
                OsString::from("--composite"),
                OsString::from("false"),
                OsString::from("--extendedDiagnostics"),
                OsString::from("false"),
                OsString::from("--explainFiles"),
                OsString::from("false"),
                OsString::from("--traceResolution"),
                OsString::from("false"),
            ],
            cwd: request.worktree.clone(),
            env: vec![
                (
                    "PATH".into(),
                    format!(
                        "{}:/usr/bin:/bin",
                        self.node
                            .parent()
                            .unwrap_or(Path::new("/usr/bin"))
                            .display()
                    ),
                ),
                (
                    "HOME".into(),
                    crate::userhome::user_home()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default(),
                ),
                ("TMPDIR".into(), tmp.display().to_string()),
            ],
            read_roots: vec![
                request.worktree.clone(),
                self.node.clone(),
                typescript_root.to_path_buf(),
            ],
            write_roots: vec![request.cache_dir.clone()],
            read_denies: request.read_denies.clone(),
            timeout: self.timeout,
            max_output_bytes: MAX_OUTPUT_BYTES,
        }
    }
}

impl Checker for TypeScriptChecker {
    /// Identifies the single TypeScript/JavaScript project snapshot produced here.
    fn language(&self) -> Language {
        Language::TypeScript
    }

    /// Runs the selected root config and admits only a fully parsed, nontruncated CLI result.
    /// A read exclusion touching any runner read root, or a credential glob with an unprovable
    /// match-free inventory, yields `ReadRestricted` before execution or publication.
    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            let generation = request.input_generation;
            let Some(config) = select_config(&request.worktree, &request.read_denies) else {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::ReadRestricted,
                    generation,
                );
            };
            if fs::create_dir_all(request.cache_dir.join("tmp")).is_err() {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::Fatal,
                    generation,
                );
            }
            let spec = self.run_spec(&request, &config);
            if !read_policy_supported(&spec) {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::ReadRestricted,
                    generation,
                );
            }
            if !regular_allowed(&self.node, &request.read_denies)
                || !regular_allowed(&self.tsc_cli, &request.read_denies)
            {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::ToolMissing,
                    generation,
                );
            }
            let started = Instant::now();
            let output = match self.runner.run(spec.clone()).await {
                Ok(output) => output,
                Err(_) => {
                    return ProblemSnapshot::unavailable(
                        Language::TypeScript,
                        UnavailableReason::Fatal,
                        generation,
                    );
                }
            };
            if output.timed_out {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::Timeout,
                    generation,
                );
            }
            if !read_policy_supported(&spec) {
                return ProblemSnapshot::unavailable(
                    Language::TypeScript,
                    UnavailableReason::ReadRestricted,
                    generation,
                );
            }
            parse_tsc_output(
                &output,
                &request.worktree,
                &config,
                &request.read_denies,
                generation,
                started.elapsed().as_millis() as u64,
            )
        })
    }
}

/// Proves that a host deny cannot hide an existing input in any readable runner root.
///
/// Path denies intersecting a read or write root always refuse the check. With any deny, every
/// explicit grant is scanned twice: unreadable entries, links, a matching entry, or the bounded
/// scan ceiling refuse publication, including aliases into denies outside the lexical roots.
/// The filesystem can still change between the
/// two scans; the scheduler's input-generation fence handles observed edits, not a transient edit
/// that appears and disappears entirely during one run.
fn read_policy_supported(spec: &RunSpec) -> bool {
    if spec.read_denies.is_empty() {
        return true;
    }
    let mut roots = spec.read_roots.iter().chain(&spec.write_roots);
    if spec.read_denies.iter().any(|deny| match deny {
        ReadDeny::Path(path) => roots.clone().any(|root| paths_overlap(path, root)),
        ReadDeny::Glob { .. } => false,
    }) {
        return false;
    }
    roots.all(|root| no_denied_entries(root, &spec.read_denies))
}

/// Reports whether two absolute paths can share a descendant, ignoring ASCII case like Seatbelt.
/// Non-ASCII or non-absolute paths are unprovable and conservatively overlap.
fn paths_overlap(left: &Path, right: &Path) -> bool {
    let (Some(left), Some(right)) = (left.to_str(), right.to_str()) else {
        return true;
    };
    if !left.is_ascii()
        || !right.is_ascii()
        || !Path::new(left).is_absolute()
        || !Path::new(right).is_absolute()
    {
        return true;
    }
    let left = left.to_ascii_lowercase();
    let right = right.to_ascii_lowercase();
    Path::new(&left).starts_with(&right) || Path::new(&right).starts_with(&left)
}

/// Scans one explicit grant without following links and rejects any deny match or alias.
/// Missing, unreadable, link-bearing, or over-limit trees cannot prove the grant safe.
fn no_denied_entries(root: &Path, denies: &[ReadDeny]) -> bool {
    let mut directories = vec![root.to_path_buf()];
    let mut seen = 0usize;
    while let Some(directory) = directories.pop() {
        if denies.iter().any(|deny| deny.matches(&directory))
            || directory.ancestors().any(|part| {
                !fs::symlink_metadata(part).is_ok_and(|metadata| !metadata.file_type().is_symlink())
            })
        {
            return false;
        }
        if fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return false;
            };
            seen += 1;
            if seen > MAX_POLICY_ENTRIES {
                return false;
            }
            let path = entry.path();
            if denies.iter().any(|deny| deny.matches(&path)) {
                return false;
            }
            let Ok(kind) = entry.file_type() else {
                return false;
            };
            if kind.is_symlink() {
                return false;
            }
            if kind.is_dir() {
                directories.push(path);
            }
        }
    }
    true
}

/// Selects a regular root `tsconfig.json`, then a regular root `jsconfig.json`.
///
/// A denied candidate, unreadable metadata, or a symlinked config cannot establish a safe
/// project root. A symlinked TypeScript config may defer to a regular JavaScript config.
fn select_config(worktree: &Path, denies: &[ReadDeny]) -> Option<PathBuf> {
    let ts = worktree.join("tsconfig.json");
    let js = worktree.join("jsconfig.json");
    for candidate in [&ts, &js] {
        if denies.iter().any(|deny| deny.matches(candidate)) {
            return None;
        }
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.is_file() => return Some(candidate.clone()),
            Ok(metadata) if metadata.file_type().is_symlink() => continue,
            Ok(_) => return None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Accepts only a regular tool file with link-free ancestors outside host read exclusions.
fn regular_allowed(path: &Path, denies: &[ReadDeny]) -> bool {
    path.ancestors().all(|prefix| {
        !denies.iter().any(|deny| deny.matches(prefix))
            && fs::symlink_metadata(prefix).is_ok_and(|metadata| !metadata.file_type().is_symlink())
    }) && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Parses pinned `tsc --pretty false --diagnostics --listFiles --noEmit` output.
///
/// Unknown lines, footer mismatch, denied diagnostic paths, inconsistent exit status, and
/// truncation become `Fatal`. With any host read deny, arbitrary diagnostic message text is
/// redacted after deduplication while admitted path, severity, and TS code remain; counts retain
/// distinct diagnostics. Zero analyzed worktree files become `NoFiles` even with a config
/// diagnostic, so no numeric zero is presented as a clean project result.
pub fn parse_tsc_output(
    output: &RunOutput,
    worktree: &Path,
    config: &Path,
    denies: &[ReadDeny],
    generation: u64,
    duration_ms: u64,
) -> ProblemSnapshot {
    let fatal =
        || ProblemSnapshot::unavailable(Language::TypeScript, UnavailableReason::Fatal, generation);
    if output.truncated
        || output.timed_out
        || !output.stderr.is_empty()
        || !matches!(output.status, Some(0 | 2))
    {
        return fatal();
    }
    let Ok(text) = std::str::from_utf8(&output.stdout) else {
        return fatal();
    };
    if !text.ends_with('\n') {
        return fatal();
    }
    let mut problems = Vec::new();
    let mut listed = 0usize;
    let mut project_file = false;
    let mut phase = 0u8;
    let mut footer = 0usize;
    let mut declared_files = None;
    for line in text.lines() {
        if phase == 0 {
            if let Some(problem) = diagnostic(line, worktree, config) {
                if !super::check_problem_path_allowed(worktree, &problem.path, denies) {
                    return fatal();
                }
                problems.push(problem);
                continue;
            }
            if line.starts_with(' ') && !line.trim().is_empty() {
                let Some(last) = problems.last_mut() else {
                    return fatal();
                };
                last.message.push(' ');
                last.message.push_str(line.trim());
                continue;
            }
            phase = 1;
        }
        if phase == 1 {
            if line.starts_with("Files:") {
                phase = 2;
            } else {
                let path = Path::new(line);
                if !path.is_absolute() {
                    return fatal();
                }
                if denies.iter().any(|deny| deny.matches(path)) {
                    return fatal();
                }
                listed += 1;
                if path.strip_prefix(worktree).is_ok() {
                    if !super::check_problem_path_allowed(worktree, line, denies) {
                        return fatal();
                    }
                    project_file |= matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs")
                    );
                }
                continue;
            }
        }
        if phase == 2 {
            let expected = if footer < FOOTER.len() {
                FOOTER[footer]
            } else if footer == FOOTER.len() {
                TOTAL_TIME
            } else {
                return fatal();
            };
            let Some((label, value)) = line.split_once(':') else {
                return fatal();
            };
            if label != expected || !valid_footer_value(footer, value.trim()) {
                return fatal();
            }
            if footer == 0 {
                declared_files = value.trim().parse::<usize>().ok();
            }
            footer += 1;
        }
    }
    if footer != FOOTER.len() + 1 || declared_files != Some(listed) {
        return fatal();
    }
    if !project_file {
        return ProblemSnapshot::unavailable(
            Language::TypeScript,
            UnavailableReason::NoFiles,
            generation,
        );
    }
    let errors = problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .count();
    if (output.status == Some(0)) != (errors == 0) {
        return fatal();
    }
    let mut snapshot = ProblemSnapshot::from_problems(
        Language::TypeScript,
        CheckState::Ready,
        problems,
        generation,
        duration_ms,
    );
    if !denies.is_empty() {
        for problem in &mut snapshot.problems {
            problem.message = "[redacted by host read policy]".to_owned();
        }
    }
    snapshot
}

/// Validates the numeric form of each pinned diagnostics footer value.
fn valid_footer_value(index: usize, value: &str) -> bool {
    match index {
        0..=5 => value.parse::<usize>().is_ok(),
        6 => value
            .strip_suffix('K')
            .is_some_and(|number| number.parse::<usize>().is_ok()),
        _ => value
            .strip_suffix('s')
            .and_then(|number| number.parse::<f64>().ok())
            .is_some_and(|seconds| seconds.is_finite() && seconds >= 0.0),
    }
}

/// Parses one file-scoped or project-scoped CLI diagnostic; returns `None` for non-diagnostics.
///
/// File-less diagnostics attach to the chosen config at line and column zero. File paths under
/// the worktree become relative so the public problems page uses stable project-local paths.
fn diagnostic(line: &str, worktree: &Path, config: &Path) -> Option<Problem> {
    let (path, line_number, column, rest) =
        if line.starts_with("error TS") || line.starts_with("warning TS") {
            (
                config
                    .strip_prefix(worktree)
                    .ok()?
                    .to_string_lossy()
                    .into_owned(),
                0,
                0,
                line,
            )
        } else {
            let (prefix, rest) = line.split_once("): ")?;
            let (file, position) = prefix.rsplit_once('(')?;
            let (row, col) = position.split_once(',')?;
            let row = row.parse::<u32>().ok()?;
            let col = col.parse::<u32>().ok()?;
            if row == 0 || col == 0 || file.is_empty() {
                return None;
            }
            let path = Path::new(file);
            let path = path
                .strip_prefix(worktree)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            (path, row, col, rest)
        };
    let (kind, rest) = rest.split_once(' ')?;
    let severity = match kind {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        _ => return None,
    };
    let (code, message) = rest.split_once(": ")?;
    if code.len() < 3
        || !code.starts_with("TS")
        || !code[2..].bytes().all(|byte| byte.is_ascii_digit())
        || message.is_empty()
    {
        return None;
    }
    Some(Problem::new(
        path,
        line_number,
        column,
        severity,
        Some(code.to_owned()),
        message.to_owned(),
    ))
}
