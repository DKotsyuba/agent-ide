//! Project card: the language-independent summary block `ide.start` prints.
//!
//! Per-language detection (manifests, environment, commands, entry points) is owned by the
//! [`crate::lang::LanguageSupport`] modules and arrives here as an already-built
//! `Vec<`[`LanguageProject`](crate::lang::LanguageProject)`>`. This module adds everything no language module owns: git
//! plumbing, a single walk of the tree for layout/line counts/docs, and the caller-supplied
//! language server states and problem summary, then renders the fixed-shape card.
//!
//! Rendering deviates from a byte-perfect ASCII mock-up in two documented ways: the `commands:`
//! block is one line per command kind rather than a two-column grid (a grid cannot host a
//! multi-language kind without breaking its own alignment), and `environment:` renders every
//! `(key, value)` pair as `"<language> <key> <value>"` rather than language-specific wording.
//! Both keep every literal rendering rule (merge order, `—` for absent, per-language lines for a
//! shared kind, the size ceiling) without inventing per-language prose this module cannot own.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::lang::{CommandSource, Language, LanguageProject, ProjectCommand, ProjectCommands};

/// Budget for one `git` plumbing call before it is killed and the whole [`GitState`] is dropped.
const GIT_TIMEOUT: Duration = Duration::from_secs(3);

/// Poll interval while waiting for a `git` child to exit inside [`GIT_TIMEOUT`].
const GIT_POLL: Duration = Duration::from_millis(10);

/// Files beyond this count stop the walk early; [`ProjectCard::truncated`] records it and the
/// counts already gathered are reported as a partial result rather than discarded.
const MAX_WALK_FILES: u32 = 50_000;

/// Files larger than this are counted in [`LanguageSummary::files`] but never opened for a line
/// count, so one huge generated file cannot make the walk read gigabytes.
const MAX_LINE_COUNT_BYTES: u64 = 2 * 1024 * 1024;

/// Top-level [`ProjectCard::layout`] entries kept after sorting by file count descending.
const MAX_LAYOUT_TOP: usize = 12;

/// [`ProjectCard::docs`] entries `render` lists before collapsing the rest into a trailing count.
const MAX_DOCS_SHOWN: usize = 6;

/// Byte ceiling for one rendered card; `render` collapses `layout` children first, then `docs`.
const MAX_CARD_BYTES: usize = 1500;

/// Git plumbing state for [`ProjectCard::git`].
///
/// `None` on any read failure or timeout — never a fallback `clean: true`, since a failed
/// `git status` call is not evidence that the tree is clean.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitState {
    /// Current branch name from `git rev-parse --abbrev-ref HEAD`, or `"HEAD"` when detached.
    pub branch: Option<String>,
    /// Whether `git status --porcelain` printed nothing.
    pub clean: bool,
    /// `(short sha, subject)` of `HEAD` from `git log -1`; `None` on a repository with no commits
    /// yet (a legitimate empty state, distinct from a failed or timed-out call).
    pub last_commit: Option<(String, String)>,
}

/// One already-detected language's file/line footprint under the project root.
#[derive(Clone, Debug, PartialEq)]
pub struct LanguageSummary {
    pub language: Language,
    /// Files under the root whose extension [`Language::for_path`] maps to this language.
    pub files: u32,
    /// Summed line count of those files that are at most `MAX_LINE_COUNT_BYTES`; larger files
    /// still count towards `files` but are never opened, so `lines` can undercount on repos with
    /// huge generated sources.
    pub lines: u64,
    /// The language module's own detection result, carried through unchanged.
    pub project: LanguageProject,
}

/// One directory's total file count and, for the top two levels, its own subdirectories.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirSummary {
    /// Path relative to the project root, e.g. `src` or `src/assistance`.
    pub path: PathBuf,
    /// Files anywhere under this directory at any depth; noise directories are excluded from the
    /// count but the count itself is not depth-limited.
    pub files: u32,
    /// Immediate subdirectories as their own summaries; populated at depth 1 (children of the
    /// root), empty at depth 2 so the structure never reports a depth-3 breakdown.
    pub children: Vec<DirSummary>,
}

/// A language server's readiness as the caller already knows it; this module never probes a
/// server itself, it only renders what `ide.start` was told.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerState {
    pub language: Language,
    /// Free text status: `"ready"`, `"loading (~10 s)"`, `"n/a"`.
    pub state: String,
}

/// Everything `ide.start` prints about the project, independent of any one language.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectCard {
    pub root: PathBuf,
    /// `root`'s last path component; the full path when `root` has none (`/`, `.`).
    pub name: String,
    pub git: Option<GitState>,
    /// Sorted by `lines` descending (ties broken by [`Language`]'s declaration order) so the
    /// largest language always leads `render`'s `languages:` line.
    pub languages: Vec<LanguageSummary>,
    /// Top-level directories, sorted by `files` descending, capped at `MAX_LAYOUT_TOP` entries.
    pub layout: Vec<DirSummary>,
    /// Matched doc paths (`README*`, `CLAUDE.md`, `AGENTS.md`, `CONTRIBUTING*` at the root;
    /// `docs/**/*.md` at any depth), in display priority order (that same list order, then
    /// alphabetical). `render` shows only the first `MAX_DOCS_SHOWN`.
    pub docs: Vec<PathBuf>,
    pub servers: Vec<ServerState>,
    pub problems: Option<String>,
    /// Set when the walk hit `MAX_WALK_FILES` before finishing; every count above is then a
    /// partial result, not a complete one.
    pub truncated: bool,
}

/// Mutable state threaded through the recursive walk in [`scan_dir`].
struct WalkState {
    files_seen: u32,
    truncated: bool,
    language_files: BTreeMap<Language, u32>,
    language_lines: BTreeMap<Language, u64>,
    docs: Vec<PathBuf>,
}

impl WalkState {
    fn new() -> Self {
        Self {
            files_seen: 0,
            truncated: false,
            language_files: BTreeMap::new(),
            language_lines: BTreeMap::new(),
            docs: Vec::new(),
        }
    }
}

/// Whether `name` is a directory the walk never descends into: hidden directories (`.git`,
/// `.venv`, `.agent-ide`, `.codegraph`, …) by the leading dot, plus the fixed noise names that
/// are not hidden on disk.
fn is_noise_dir(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.')
        || matches!(
            name.as_ref(),
            "node_modules" | "target" | "venv" | "dist" | "build" | "__pycache__"
        )
}

/// Whether `rel` (already relative to the root) is one of the fixed doc patterns: `README*`,
/// `CLAUDE.md`, `AGENTS.md` and `CONTRIBUTING*` only when they sit directly at the root (so a
/// vendored `some/vendor/README.md` deep in the tree is not counted), plus any `*.md` file
/// anywhere under a top-level `docs/` directory.
fn is_doc_path(rel: &Path) -> bool {
    let Some(file_name) = rel.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let mut components = rel.components();
    let first = components.next();
    let is_root_level = components.next().is_none();
    if is_root_level
        && (file_name.starts_with("README")
            || file_name == "CLAUDE.md"
            || file_name == "AGENTS.md"
            || file_name.starts_with("CONTRIBUTING"))
    {
        return true;
    }
    first == Some(Component::Normal(OsStr::new("docs")))
        && rel.extension().and_then(OsStr::to_str) == Some("md")
}

/// Priority key used to order matched docs before `render` truncates the list: named root files
/// first, in the order the contract lists them, then everything else (the `docs/**/*.md` files)
/// alphabetically.
fn doc_priority(path: &Path) -> u8 {
    let name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    if name.starts_with("README") {
        0
    } else if name == "CLAUDE.md" {
        1
    } else if name == "AGENTS.md" {
        2
    } else if name.starts_with("CONTRIBUTING") {
        3
    } else {
        4
    }
}

/// Counts physical lines in `bytes` without requiring valid UTF-8, matching
/// [`crate::lang::line_count`]'s "unterminated last line still counts" rule.
fn count_lines(bytes: &[u8]) -> u64 {
    let newlines = bytes.iter().filter(|byte| **byte == b'\n').count() as u64;
    if bytes.is_empty() {
        0
    } else if *bytes.last().unwrap() == b'\n' {
        newlines
    } else {
        newlines + 1
    }
}

/// Classifies one file the walk discovered: records it as a doc when it matches a doc pattern,
/// and when its extension maps to a [`Language`], counts it and (when it is at most
/// `MAX_LINE_COUNT_BYTES`) adds its line count.
fn visit_file(root: &Path, path: &Path, state: &mut WalkState) {
    let rel = path.strip_prefix(root).unwrap_or(path).to_path_buf();
    if is_doc_path(&rel) {
        state.docs.push(rel);
    }
    let Some(language) = Language::for_path(path) else {
        return;
    };
    *state.language_files.entry(language).or_insert(0) += 1;
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() > MAX_LINE_COUNT_BYTES {
        return;
    }
    let Ok(bytes) = fs::read(path) else {
        return;
    };
    *state.language_lines.entry(language).or_insert(0) += count_lines(&bytes);
}

/// Recursively scans `dir`, `depth` levels below `root`, stopping early once
/// `MAX_WALK_FILES` files have been seen. Returns the total file count anywhere under `dir`
/// (noise directories excluded, no depth limit) and, only for `depth < 2`, a [`DirSummary`] per
/// immediate subdirectory — so the root call (`depth == 0`) returns the depth-1 entries with
/// their own depth-2 children attached, and depth-2 directories report a file count but no
/// children of their own.
fn scan_dir(root: &Path, dir: &Path, depth: u32, state: &mut WalkState) -> (u32, Vec<DirSummary>) {
    let mut total = 0u32;
    let mut children = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return (0, children);
    };
    for entry in entries.flatten() {
        if state.truncated {
            break;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if is_noise_dir(&entry.file_name()) {
                continue;
            }
            let (sub_total, sub_children) = scan_dir(root, &path, depth + 1, state);
            total += sub_total;
            if depth < 2 {
                let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
                children.push(DirSummary {
                    path: rel,
                    files: sub_total,
                    children: sub_children,
                });
            }
        } else if file_type.is_file() {
            state.files_seen += 1;
            if state.files_seen > MAX_WALK_FILES {
                state.truncated = true;
                break;
            }
            total += 1;
            visit_file(root, &path, state);
        }
    }
    children.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.path.cmp(&b.path)));
    if depth == 0 {
        children.truncate(MAX_LAYOUT_TOP);
    }
    (total, children)
}

/// Runs one `git` plumbing command in `root` with a bounded lifetime, mirroring the spawn/poll
/// shape in [`crate::checks::fingerprint::git_worktree_fingerprint`]: stdout is drained on a
/// helper thread while this thread polls `try_wait` against [`GIT_TIMEOUT`], so a child that
/// blocks writing to a full pipe cannot deadlock the wait. `None` on a spawn failure, a non-zero
/// exit, invalid UTF-8, or a run that has to be killed past the deadline.
fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("/usr/bin/git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .args(["-c", "core.fsmonitor=false"])
        .arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        stdout.read_to_end(&mut buffer).ok()?;
        Some(buffer)
    });
    let deadline = Instant::now() + GIT_TIMEOUT;
    let exited_cleanly = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() >= deadline => break false,
            Ok(None) => thread::sleep(GIT_POLL),
            Err(_) => break false,
        }
    };
    if !exited_cleanly {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let bytes = reader.join().ok().flatten()?;
    String::from_utf8(bytes)
        .ok()
        .map(|text| text.trim().to_string())
}

/// Reads [`GitState`] for `root`. `None` when `root` has no `.git` entry (the common non-git
/// case never spawns a process) or when the branch or status call fails or times out. `HEAD`'s
/// last commit is best-effort: a repository with no commits yet leaves `last_commit: None`
/// without invalidating the rest of the state.
fn collect_git(root: &Path) -> Option<GitState> {
    if !root.join(".git").exists() {
        return None;
    }
    let branch = run_git(root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let status = run_git(root, &["status", "--porcelain"])?;
    let last_commit = run_git(root, &["log", "-1", "--format=%h%x09%s"]).and_then(|line| {
        line.split_once('\t')
            .map(|(sha, subject)| (sha.to_string(), subject.to_string()))
    });
    Some(GitState {
        branch: Some(branch),
        clean: status.is_empty(),
        last_commit,
    })
}

/// Builds the [`ProjectCard`] for `root`.
///
/// `languages` is the already-detected `Vec<LanguageProject>` from the per-language
/// [`crate::lang::LanguageSupport`] modules; `servers` and `problems` are the caller's own
/// up-to-date readiness/problem summary. Everything else — git state, layout, per-language file
/// and line counts, and doc paths — comes from one walk of the tree done here.
pub fn collect(
    root: &Path,
    languages: Vec<LanguageProject>,
    servers: Vec<ServerState>,
    problems: Option<String>,
) -> ProjectCard {
    let name = root
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    let git = collect_git(root);
    let mut state = WalkState::new();
    let (_, layout) = scan_dir(root, root, 0, &mut state);
    let mut summaries: Vec<LanguageSummary> = languages
        .into_iter()
        .map(|project| {
            let language = project.language;
            LanguageSummary {
                language,
                files: state.language_files.get(&language).copied().unwrap_or(0),
                lines: state.language_lines.get(&language).copied().unwrap_or(0),
                project,
            }
        })
        .collect();
    summaries.sort_by(|a, b| {
        b.lines
            .cmp(&a.lines)
            .then_with(|| a.language.cmp(&b.language))
    });
    let mut docs = state.docs;
    docs.sort_by(|a, b| doc_priority(a).cmp(&doc_priority(b)).then_with(|| a.cmp(b)));
    ProjectCard {
        root: root.to_path_buf(),
        name,
        git,
        languages: summaries,
        layout,
        docs,
        servers,
        problems,
        truncated: state.truncated,
    }
}

/// A command kind's field accessor on [`ProjectCommands`].
type CommandField = fn(&ProjectCommands) -> &Option<ProjectCommand>;

/// Fixed kind order the `commands:` block always follows, and the field each kind reads off
/// [`ProjectCommands`].
const COMMAND_KINDS: [(&str, CommandField); 6] = [
    ("build", |commands| &commands.build),
    ("check", |commands| &commands.check),
    ("test", |commands| &commands.test),
    ("lint", |commands| &commands.lint),
    ("fmt", |commands| &commands.format),
    ("typecheck", |commands| &commands.typecheck),
];

/// Fixed [`CommandSource`] order the `commands (…):` provenance note follows.
const COMMAND_SOURCE_ORDER: [CommandSource; 5] = [
    CommandSource::Ci,
    CommandSource::Makefile,
    CommandSource::Manifest,
    CommandSource::Readme,
    CommandSource::Default,
];

/// Renders `card` as the fixed-shape project card block `ide.start` prints.
///
/// `project:`/`root:` and, when any language was detected, `languages:` always appear;
/// `git:`/`problems:` vanish with a `None` value, and `commands:`/`environment:`/`layout:`/
/// `entry points:`/`docs:`/`servers:` vanish when there is nothing to say. When the full render
/// exceeds `MAX_CARD_BYTES` bytes, `layout` drops its depth-2 children first, then `docs`
/// collapses to 3 entries, then to none, in that order, until the render fits (or the smallest
/// attempt is returned as a best effort).
pub fn render(card: &ProjectCard) -> String {
    let attempts: [(bool, usize); 4] = [
        (true, MAX_DOCS_SHOWN),
        (false, MAX_DOCS_SHOWN),
        (false, 3),
        (false, 0),
    ];
    let mut last = String::new();
    for (show_layout_children, docs_shown) in attempts {
        last = render_at(card, show_layout_children, docs_shown);
        if last.len() <= MAX_CARD_BYTES {
            return last;
        }
    }
    last
}

fn render_at(card: &ProjectCard, show_layout_children: bool, docs_shown: usize) -> String {
    let mut lines = Vec::new();
    lines.push(render_header(card));
    if !card.languages.is_empty() {
        lines.push(render_languages(card));
    }
    if let Some(commands) = render_commands(card) {
        lines.push(commands);
    }
    if let Some(environment) = render_environment(card) {
        lines.push(environment);
    }
    if !card.layout.is_empty() {
        lines.push(render_layout(card, show_layout_children));
    }
    if let Some(entry_points) = render_entry_points(card) {
        lines.push(entry_points);
    }
    if docs_shown > 0 && !card.docs.is_empty() {
        lines.push(render_docs(card, docs_shown));
    }
    if !card.servers.is_empty() {
        lines.push(render_servers(card));
    }
    if let Some(problems) = &card.problems {
        lines.push(format!("problems: {problems}"));
    }
    lines.join("\n")
}

fn render_header(card: &ProjectCard) -> String {
    let mut header = format!("project: {}  root: {}", card.name, card.root.display());
    if let Some(git) = &card.git {
        let branch = git.branch.as_deref().unwrap_or("HEAD");
        let state = if git.clean { "clean" } else { "dirty" };
        let _ = write!(header, "  (git: {branch}, {state}");
        if let Some((sha, subject)) = &git.last_commit {
            let _ = write!(header, ", last {sha} {subject}");
        }
        header.push(')');
    }
    header
}

/// Rounds a line/file count for display: plain integers below 1000, one decimal place of `k`
/// between 1.0k and 9.9k, and a whole `k` from 10k up (`47k`, `1.2k`, `312`).
fn format_count(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let scaled = n as f64 / 1000.0;
    if scaled >= 10.0 {
        format!("{}k", scaled.round() as u64)
    } else {
        let rounded = (scaled * 10.0).round() / 10.0;
        if (rounded - rounded.trunc()).abs() < f64::EPSILON {
            format!("{}k", rounded as u64)
        } else {
            format!("{rounded:.1}k")
        }
    }
}

fn render_languages(card: &ProjectCard) -> String {
    let parts: Vec<String> = card
        .languages
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let lines = format_count(summary.lines);
            if index == 0 {
                format!(
                    "{} {lines} lines in {} files",
                    summary.language, summary.files
                )
            } else {
                format!("{} {lines} in {}", summary.language, summary.files)
            }
        })
        .collect();
    format!("languages: {}", parts.join(" · "))
}

/// Merges every language's [`ProjectCommands`] into the fixed six-kind order; a kind more than
/// one language provides gets one `"<kind> (<language>): <argv>"` line per language instead of a
/// shared unprefixed line, and a kind nobody provides prints as `"<kind>: —"`.
fn render_commands(card: &ProjectCard) -> Option<String> {
    if card.languages.is_empty() {
        return None;
    }
    let mut sources = Vec::new();
    let mut body = Vec::new();
    for (label, select) in COMMAND_KINDS {
        let providers: Vec<(Language, &ProjectCommand)> = card
            .languages
            .iter()
            .filter_map(|summary| {
                select(&summary.project.commands)
                    .as_ref()
                    .map(|command| (summary.language, command))
            })
            .collect();
        if providers.is_empty() {
            body.push(format!("  {label}: —"));
            continue;
        }
        for (_, command) in &providers {
            if !sources.contains(&command.source) {
                sources.push(command.source);
            }
        }
        if providers.len() == 1 {
            let (_, command) = providers[0];
            body.push(format!("  {label}: {}", command.argv.join(" ")));
        } else {
            for (language, command) in providers {
                body.push(format!(
                    "  {label} ({language}): {}",
                    command.argv.join(" ")
                ));
            }
        }
    }
    if sources.is_empty() {
        return Some(format!("commands:\n{}", body.join("\n")));
    }
    let source_names: Vec<&str> = COMMAND_SOURCE_ORDER
        .iter()
        .filter(|source| sources.contains(source))
        .map(|source| source.name())
        .collect();
    Some(format!(
        "commands ({}):\n{}",
        source_names.join(", "),
        body.join("\n")
    ))
}

/// Renders every language's environment facts as `"<language> <key> <value>"`, in language then
/// declaration order, joined with `" · "`.
fn render_environment(card: &ProjectCard) -> Option<String> {
    let parts: Vec<String> = card
        .languages
        .iter()
        .flat_map(|summary| {
            summary
                .project
                .environment
                .iter()
                .map(move |(key, value)| format!("{} {key} {value}", summary.language))
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(format!("environment: {}", parts.join(" · ")))
    }
}

fn render_layout(card: &ProjectCard, show_children: bool) -> String {
    let parts: Vec<String> = card
        .layout
        .iter()
        .map(|dir| {
            let mut entry = format!("{}/ {}", dir.path.display(), dir.files);
            if show_children && !dir.children.is_empty() {
                let children: Vec<String> = dir
                    .children
                    .iter()
                    .map(|child| {
                        let name = child
                            .path
                            .file_name()
                            .map(|value| value.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        format!("{name} {}", child.files)
                    })
                    .collect();
                let _ = write!(entry, " ({})", children.join(", "));
            }
            entry
        })
        .collect();
    format!("layout: {}", parts.join(" · "))
}

/// Merges every language's entry points, deduplicated, in language then declaration order.
fn render_entry_points(card: &ProjectCard) -> Option<String> {
    let mut seen: Vec<&PathBuf> = Vec::new();
    for summary in &card.languages {
        for entry in &summary.project.entry_points {
            if !seen.contains(&entry) {
                seen.push(entry);
            }
        }
    }
    if seen.is_empty() {
        return None;
    }
    let rendered: Vec<String> = seen.iter().map(|path| path.display().to_string()).collect();
    Some(format!("entry points: {}", rendered.join(" · ")))
}

fn render_docs(card: &ProjectCard, shown: usize) -> String {
    let shown = shown.min(card.docs.len());
    let listed: Vec<String> = card.docs[..shown]
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let mut line = format!("docs: {}", listed.join(", "));
    if card.docs.len() > shown {
        let _ = write!(line, " … ({})", card.docs.len());
    }
    line
}

fn render_servers(card: &ProjectCard) -> String {
    let parts: Vec<String> = card
        .servers
        .iter()
        .map(|server| format!("{} {}", server.language, server.state))
        .collect();
    format!("servers: {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory under the system temp dir, removed on drop, unique per test run so parallel
    /// test runs of this module never collide.
    struct TempTree {
        root: PathBuf,
    }

    impl TempTree {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "agent-ide-project-{tag}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("create temp tree root");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn write(&self, relative: &str, contents: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).expect("create parent dirs");
            fs::write(path, contents).expect("write fixture file");
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn git_available() -> bool {
        Path::new("/usr/bin/git").exists()
    }

    fn alpha_project() -> LanguageProject {
        LanguageProject {
            language: crate::lang::testing::ALPHA,
            manifests: vec![PathBuf::from("alpha.toml")],
            environment: vec![("toolchain".to_string(), "1.98.1".to_string())],
            interpreter: None,
            commands: ProjectCommands {
                build: Some(ProjectCommand {
                    argv: vec!["alphac".into(), "build".into(), "--release".into()],
                    source: CommandSource::Manifest,
                }),
                check: Some(ProjectCommand {
                    argv: vec!["alphac".into(), "check".into()],
                    source: CommandSource::Manifest,
                }),
                test: Some(ProjectCommand {
                    argv: vec!["alphac".into(), "test".into()],
                    source: CommandSource::Ci,
                }),
                lint: None,
                format: Some(ProjectCommand {
                    argv: vec!["alphac".into(), "fmt".into(), "--all".into()],
                    source: CommandSource::Default,
                }),
                typecheck: None,
            },
            entry_points: vec![
                PathBuf::from("src/main.alpha"),
                PathBuf::from("src/lib.alpha"),
            ],
        }
    }

    fn beta_project() -> LanguageProject {
        LanguageProject {
            language: crate::lang::testing::BETA,
            manifests: vec![PathBuf::from("beta.toml")],
            environment: vec![("venv".to_string(), "none".to_string())],
            interpreter: None,
            commands: ProjectCommands {
                build: None,
                check: None,
                test: Some(ProjectCommand {
                    argv: vec!["betatest".into()],
                    source: CommandSource::Manifest,
                }),
                lint: None,
                format: None,
                typecheck: None,
            },
            entry_points: vec![],
        }
    }

    #[test]
    fn renders_a_fixed_fixture_exactly() {
        crate::lang::testing::install();
        let tree = TempTree::new("fixture");
        tree.write("src/main.alpha", "fn main() {}\n");
        tree.write("src/lib.alpha", "pub fn lib() {}\npub fn two() {}\n");
        tree.write("src/assistance/mod.alpha", "pub struct A;\n");
        tree.write("tests/it.alpha", "#[test]\nfn ok() {}\n");
        tree.write("app.beta", "print('hi')\n");
        tree.write("README.md", "# demo\n");
        tree.write("CLAUDE.md", "notes\n");

        let card = collect(
            tree.path(),
            vec![alpha_project(), beta_project()],
            vec![ServerState {
                language: crate::lang::testing::ALPHA,
                state: "ready".to_string(),
            }],
            Some("alpha checking (first check)".to_string()),
        );

        assert_eq!(card.git, None);
        assert!(!card.truncated);

        let rendered = render(&card);
        let expected = format!(
            "project: {name}  root: {root}\n\
             languages: alpha 6 lines in 4 files · beta 1 in 1\n\
             commands (ci, manifest, default):\n\
             \x20 build: alphac build --release\n\
             \x20 check: alphac check\n\
             \x20 test (alpha): alphac test\n\
             \x20 test (beta): betatest\n\
             \x20 lint: —\n\
             \x20 fmt: alphac fmt --all\n\
             \x20 typecheck: —\n\
             environment: alpha toolchain 1.98.1 · beta venv none\n\
             layout: src/ 3 (assistance 1) · tests/ 1\n\
             entry points: src/main.alpha · src/lib.alpha\n\
             docs: README.md, CLAUDE.md\n\
             servers: alpha ready\n\
             problems: alpha checking (first check)",
            name = tree.path().file_name().unwrap().to_string_lossy(),
            root = tree.path().display(),
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn noise_directories_are_never_counted() {
        crate::lang::testing::install();
        let tree = TempTree::new("noise");
        tree.write("src/main.alpha", "fn main() {}\n");
        tree.write("target/debug/build.alpha", "junk\n");
        tree.write("node_modules/pkg/index.js", "junk\n");
        tree.write(".git/HEAD", "ref: refs/heads/main\n");
        tree.write(".venv/lib/site.beta", "junk\n");
        tree.write("__pycache__/mod.cpython.pyc", "junk\n");

        let card = collect(tree.path(), vec![alpha_project()], vec![], None);
        assert_eq!(card.layout.len(), 1);
        assert_eq!(card.layout[0].path, PathBuf::from("src"));
        assert_eq!(card.layout[0].files, 1);
        let alpha = card
            .languages
            .iter()
            .find(|s| s.language == crate::lang::testing::ALPHA)
            .unwrap();
        assert_eq!(alpha.files, 1);
    }

    #[test]
    fn render_collapses_detail_to_fit_the_byte_ceiling() {
        crate::lang::testing::install();
        let tree = TempTree::new("big");
        for top in 0..20 {
            for child in 0..5 {
                tree.write(
                    &format!("src/dir{top}/child{child}/f.alpha",),
                    "fn f() {}\n",
                );
            }
        }
        for doc in 0..30 {
            tree.write(&format!("docs/topic{doc}.md"), "# doc\n");
        }

        let card = collect(tree.path(), vec![alpha_project()], vec![], None);
        let rendered = render(&card);
        assert!(
            rendered.len() <= MAX_CARD_BYTES,
            "rendered card is {} bytes",
            rendered.len()
        );
        assert!(rendered.contains("… ("));
    }

    #[test]
    fn git_state_reads_branch_clean_flag_and_last_commit() {
        if !git_available() {
            return;
        }
        let tree = TempTree::new("git");
        tree.write("README.md", "# repo\n");
        let run = |args: &[&str]| {
            let status = StdCommand::new("/usr/bin/git")
                .arg("-C")
                .arg(tree.path())
                .args(args)
                .status()
                .expect("run git");
            assert!(status.success(), "git {args:?} failed");
        };
        run(&["init", "--quiet", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["add", "README.md"]);
        run(&["commit", "--quiet", "-m", "initial commit"]);

        let git = collect_git(tree.path()).expect("git state for a real repo");
        assert_eq!(git.branch.as_deref(), Some("main"));
        assert!(git.clean);
        let (_, subject) = git.last_commit.expect("last commit");
        assert_eq!(subject, "initial commit");

        tree.write("dirty.txt", "uncommitted\n");
        let dirty = collect_git(tree.path()).expect("git state after a new untracked file");
        assert!(!dirty.clean);
    }

    #[test]
    fn non_git_directory_reports_no_git_state() {
        let tree = TempTree::new("nogit");
        tree.write("file.txt", "content\n");
        assert_eq!(collect_git(tree.path()), None);
    }

    #[test]
    fn format_count_rounds_as_documented() {
        assert_eq!(format_count(312), "312");
        assert_eq!(format_count(1_000), "1k");
        assert_eq!(format_count(1_235), "1.2k");
        assert_eq!(format_count(47_000), "47k");
    }
}
