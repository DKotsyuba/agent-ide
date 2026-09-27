//! Confined root-config TypeScript and JavaScript project checks.
//!
//! The pinned TypeScript compiler supplies diagnostics, a complete file list, and a diagnostics footer.
//! A snapshot is ready only when all three agree and at least one worktree file was analyzed.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::runner::{ConfinedRunner, RunOutput, RunSpec};
use super::{
    BoxFuture, CheckConfig, CheckRequest, CheckState, Checker, Language, LanguageChecks, Problem,
    ProblemSnapshot, Severity, UnavailableReason,
};
use crate::assistance::launcher::absolute;
use crate::execution::seatbelt::ReadDeny;

/// Maximum bytes captured from either CLI stream; a larger report fails closed.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
/// Adapter exit status reserved for a refused compiler read or filesystem probe.
const READ_RESTRICTED_STATUS: i32 = 77;
/// First-party adapter embedded into the private cache for each compiler run.
const ADAPTER: &str = include_str!("typescript_adapter.js");
/// Unique temporary adapter names within one daemon process.
static ADAPTER_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);
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
    /// different directory trees. The adapter guards compiler filesystem calls; CLI flags direct
    /// build metadata to the private cache without changing incremental or composite semantics.
    pub fn run_spec(&self, request: &CheckRequest, config: &Path) -> RunSpec {
        let typescript_root = self
            .tsc_cli
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&self.tsc_cli);
        let tmp = request.cache_dir.join("tmp");
        let mut read_roots = vec![
            request.worktree.clone(),
            self.node.clone(),
            typescript_root.to_path_buf(),
            request.cache_dir.clone(),
        ];
        // TypeScript probes ancestor package boundaries and node_modules even without imports.
        for parent in request.worktree.ancestors().skip(1) {
            read_roots.push(parent.join("node_modules"));
            read_roots.push(parent.join("package.json"));
        }
        RunSpec {
            program: self.node.clone(),
            args: vec![
                request
                    .cache_dir
                    .join("typescript-check.js")
                    .into_os_string(),
                OsString::from("--project"),
                config.as_os_str().to_os_string(),
                OsString::from("--pretty"),
                OsString::from("false"),
                OsString::from("--diagnostics"),
                OsString::from("--listFiles"),
                OsString::from("--noEmit"),
                OsString::from("--tsBuildInfoFile"),
                request.cache_dir.join("check.tsbuildinfo").into_os_string(),
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
                ("CHECK_TSC_CLI".into(), self.tsc_cli.display().to_string()),
                (
                    "CHECK_CACHE".into(),
                    request.cache_dir.display().to_string(),
                ),
                (
                    "CHECK_READ_ROOTS".into(),
                    serde_json::to_string(&read_roots).unwrap_or_default(),
                ),
                (
                    "CHECK_READ_DENIES".into(),
                    serde_json::to_string(&request.read_denies).unwrap_or_default(),
                ),
            ],
            read_roots,
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
        crate::lang::typescript::LANGUAGE
    }

    /// Runs the selected root config and admits only a fully parsed, nontruncated CLI result.
    /// A compiler read refused by the adapter yields `ReadRestricted`, including probes that
    /// TypeScript normally converts into absent files. All other malformed output fails closed.
    fn check(&self, request: CheckRequest) -> BoxFuture<'_, ProblemSnapshot> {
        Box::pin(async move {
            let generation = request.input_generation;
            let Some(config) = select_config(&request.worktree, &request.read_denies) else {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::ReadRestricted,
                    generation,
                );
            };
            if fs::create_dir_all(request.cache_dir.join("tmp")).is_err() {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::Fatal,
                    generation,
                );
            }
            if stage_adapter(&request.cache_dir).is_err() {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::Fatal,
                    generation,
                );
            }
            let spec = self.run_spec(&request, &config);
            if [&self.node, &self.tsc_cli].iter().any(|tool| {
                tool.ancestors()
                    .any(|part| request.read_denies.iter().any(|deny| deny.matches(part)))
            }) {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::ReadRestricted,
                    generation,
                );
            }
            if !regular_allowed(&self.node, &request.read_denies)
                || !regular_allowed(&self.tsc_cli, &request.read_denies)
            {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::ToolMissing,
                    generation,
                );
            }
            let started = Instant::now();
            let output = match self.runner.run(spec.clone()).await {
                Ok(output) => output,
                Err(_) => {
                    return ProblemSnapshot::unavailable(
                        crate::lang::typescript::LANGUAGE,
                        UnavailableReason::Fatal,
                        generation,
                    );
                }
            };
            if output.timed_out {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::Timeout,
                    generation,
                );
            }
            if output.truncated {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
                    UnavailableReason::Fatal,
                    generation,
                );
            }
            if output.status == Some(READ_RESTRICTED_STATUS) {
                return ProblemSnapshot::unavailable(
                    crate::lang::typescript::LANGUAGE,
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

/// Atomically replace the private adapter without following a pre-existing final symlink.
/// Each concurrent invocation writes identical embedded bytes through a unique new file.
fn stage_adapter(cache_dir: &Path) -> io::Result<()> {
    let temporary = cache_dir.join(format!(
        ".typescript-check-{}-{}.tmp",
        std::process::id(),
        ADAPTER_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(ADAPTER.as_bytes())?;
        drop(file);
        fs::rename(&temporary, cache_dir.join("typescript-check.js"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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

/// Admits a compiler-reported worktree path, including aliases whose targets avoid host denies.
/// The adapter already guarded the compiler read; this second check protects published paths.
fn report_path_allowed(worktree: &Path, reported: &str, denies: &[ReadDeny]) -> bool {
    if denies.is_empty() {
        return true;
    }
    let path = Path::new(reported);
    let relative = if path.is_absolute() {
        let Ok(relative) = path.strip_prefix(worktree) else {
            return false;
        };
        relative
    } else {
        path
    };
    if relative.as_os_str().is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return false;
    }
    let mut current = worktree.join(relative);
    for _ in 0..40 {
        if denies.iter().any(|deny| deny.matches(&current)) {
            return false;
        }
        let mut prefix = PathBuf::from("/");
        let mut alias = None;
        for component in current.components() {
            prefix.push(component.as_os_str());
            if denies.iter().any(|deny| deny.matches(&prefix)) {
                return false;
            }
            match fs::symlink_metadata(&prefix) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    alias = Some(prefix.clone());
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
                Err(_) => return false,
            }
        }
        let Some(alias) = alias else {
            return true;
        };
        let Ok(target) = fs::read_link(&alias) else {
            return false;
        };
        let Ok(suffix) = current.strip_prefix(&alias) else {
            return false;
        };
        let target = if target.is_absolute() {
            target.join(suffix)
        } else {
            alias
                .parent()
                .unwrap_or(Path::new("/"))
                .join(target)
                .join(suffix)
        };
        let mut normalized = PathBuf::from("/");
        for component in target.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                std::path::Component::Normal(name) => normalized.push(name),
                std::path::Component::Prefix(_) => return false,
            }
        }
        current = normalized;
    }
    false
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
    let fatal = || {
        ProblemSnapshot::unavailable(
            crate::lang::typescript::LANGUAGE,
            UnavailableReason::Fatal,
            generation,
        )
    };
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
                if !report_path_allowed(worktree, &problem.path, denies) {
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
                    if !report_path_allowed(worktree, line, denies) {
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
            crate::lang::typescript::LANGUAGE,
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
        crate::lang::typescript::LANGUAGE,
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
/// TypeScript's (and JavaScript's) project-check integration.
pub struct TypeScriptChecks;

impl LanguageChecks for TypeScriptChecks {
    /// TypeScript is present only when a root `tsconfig.json` or `jsconfig.json` entry exists,
    /// including a link that the checker will reject as unprovable; `package.json` alone does
    /// not count.
    fn is_present(&self, worktree: &Path) -> bool {
        fs::symlink_metadata(worktree.join("tsconfig.json")).is_ok()
            || fs::symlink_metadata(worktree.join("jsconfig.json")).is_ok()
    }

    /// `tsc`.
    fn tool_name(&self) -> &'static str {
        "tsc"
    }

    /// Decodes [`ProjectTypeScriptChecksConfig`].
    fn parse_config(
        &self,
        section: serde_json::Value,
    ) -> Result<Arc<dyn CheckConfig>, serde_json::Error> {
        let config: ProjectTypeScriptChecksConfig = serde_json::from_value(section)?;
        Ok(Arc::new(config))
    }
}

/// Accepted TypeScript CLI declaration for confined background project checks.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTypeScriptChecksConfig {
    /// Absolute normalized pinned Node executable.
    node: PathBuf,
    /// Absolute normalized pinned TypeScript `tsc.js` module.
    tsc_cli: PathBuf,
}

impl ProjectTypeScriptChecksConfig {
    /// Returns the declared Node executable path.
    pub fn node(&self) -> &Path {
        &self.node
    }
    /// Returns the declared TypeScript CLI module path.
    pub fn tsc_cli(&self) -> &Path {
        &self.tsc_cli
    }
}

impl CheckConfig for ProjectTypeScriptChecksConfig {
    /// Rejects either tool path unless absolute and lexically normalized.
    fn validate(&self) -> bool {
        absolute(&self.node) && absolute(&self.tsc_cli)
    }

    /// Builds the confined `tsc` runner for these tools.
    fn checker(&self, runner: Arc<dyn ConfinedRunner>, timeout: Duration) -> Arc<dyn Checker> {
        Arc::new(TypeScriptChecker::new(
            runner,
            self.node.clone(),
            self.tsc_cli.clone(),
            timeout,
        ))
    }

    /// Node, then the `tsc` module run by Node.
    fn programs(&self) -> Vec<(PathBuf, Option<PathBuf>)> {
        vec![
            (self.node.clone(), None),
            (self.tsc_cli.clone(), Some(self.node.clone())),
        ]
    }
}
