//! Reusable, read-only project trees with optional source-backed details.
//!
//! This operator utility is independent of sessions and does not mint source-delivery receipts.
//! Callers authorize the root before use. No provider, model, formatter or test is started.

mod git;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::lang::{Language, Symbol, line_count, regions, render::outline_text};

/// Informational comparison policy, presented once instead of as an operational failure.
const GIT_FILTER_NOTICE: &str =
    "Git content filters disabled; observations use unfiltered worktree bytes";

/// Independent collection switches; limits apply to collection, not merely rendering.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StructureOptions {
    /// Root is depth zero; deeper directories remain visible but collapsed.
    pub max_depth: usize,
    pub include_files: bool,
    pub include_hidden: bool,
    pub respect_gitignore: bool,
    /// Excludes common dependency/build/cache directory names, not arbitrary source directories.
    pub exclude_generated: bool,
    /// Normal relative paths; directories exclude all descendants.
    pub exclude_paths: Vec<PathBuf>,
    /// Extensions without a leading dot; empty selects all files.
    pub extensions: Vec<String>,
    /// Paths to expand, including their ancestors; empty expands every directory within depth.
    pub expand_paths: Vec<PathBuf>,
    /// Keeps Git-changed paths and ancestors; failure to collect status is an error.
    pub changed_only: bool,
    pub include_diff: bool,
    pub include_header: bool,
    pub include_skeleton: bool,
    /// Retains structured declarations from the same parse used by the skeleton.
    pub include_symbols: bool,
    /// Retains symbol documentation appended to skeleton declarations.
    pub include_skeleton_comments: bool,
    /// Renders one space per nesting level instead of tree connectors.
    pub compact: bool,
    /// Compact source declarations; no dependency resolution or inferred relationships.
    pub include_imports: bool,
    pub include_size: bool,
    pub include_line_count: bool,
    /// Includes the root and collapsed directories.
    pub max_nodes: usize,
    pub max_file_bytes: usize,
    pub max_total_read_bytes: usize,
    pub max_header_lines: usize,
    pub max_skeleton_lines: usize,
    pub max_imports: usize,
    /// Bounded file-detail parser threads (1..=16); reads/budget admission remain ordered.
    pub workers: usize,
}

impl Default for StructureOptions {
    /// Builds a shallow tree without reading file bodies or collecting diffs.
    fn default() -> Self {
        Self {
            max_depth: 3,
            include_files: true,
            include_hidden: true,
            respect_gitignore: true,
            exclude_generated: true,
            exclude_paths: Vec::new(),
            extensions: Vec::new(),
            expand_paths: Vec::new(),
            changed_only: false,
            include_diff: false,
            include_header: false,
            include_skeleton: false,
            include_symbols: false,
            include_skeleton_comments: true,
            compact: false,
            include_imports: false,
            include_size: false,
            include_line_count: false,
            max_nodes: 2_000,
            max_file_bytes: 2 * 1024 * 1024,
            max_total_read_bytes: 16 * 1024 * 1024,
            max_header_lines: 12,
            max_skeleton_lines: 80,
            max_imports: 32,
            workers: 4,
        }
    }
}

/// Distinguishes disabled collection, confirmed absence and an unavailable observation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum Detail<T> {
    Disabled,
    Available(T),
    Absent,
    Unavailable { reason: String },
}

/// Text associated with one-based inclusive source line bounds.
/// Headers omit leading doc markers and trailing whitespace; imports are compacted, not raw slices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceExcerpt {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub truncated: bool,
}

/// Existing declaration-only outline; truncated only between rendered lines.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CodeSkeleton {
    pub text: String,
    pub truncated: bool,
}

/// Compact unique import declarations with source ranges; never resolved dependency edges.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ImportSummary {
    pub statements: Vec<SourceExcerpt>,
    pub truncated: bool,
}

/// Separate index and working-copy observations; unknown counts are never zero-filled.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FileDiff {
    pub status: ChangeStatus,
    pub staged: DiffCounts,
    pub unstaged: DiffCounts,
    pub previous_path: Option<PathBuf>,
}

/// Status is based on the index/working-copy state, not solely the net diff against HEAD.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeStatus {
    Unchanged,
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
}

/// Line counts can be unknown for binary/untracked content; hunk counts can hit a patch limit.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DiffCounts {
    pub added_lines: Option<u64>,
    pub removed_lines: Option<u64>,
    pub hunks: Option<u64>,
}

impl DiffCounts {
    /// Confirmed absence of a diff in a successful comparison.
    fn zero() -> Self {
        Self {
            added_lines: Some(0),
            removed_lines: Some(0),
            hunks: Some(0),
        }
    }
}

/// File-system links and special objects are listed but never opened as source files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
    Other,
    Deleted,
}

/// One root-relative entry; source-derived fields refer to the bytes identified by source_hash.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructureNode {
    pub path: PathBuf,
    pub kind: NodeKind,
    pub children: Vec<StructureNode>,
    pub collapsed: bool,
    pub truncated: bool,
    pub size_bytes: Option<u64>,
    pub line_count: Option<u32>,
    pub language: Option<String>,
    pub source_hash: Option<String>,
    pub header: Detail<SourceExcerpt>,
    pub imports: Detail<ImportSummary>,
    pub skeleton: Detail<CodeSkeleton>,
    /// Source-backed declarations; omitted from serialized legacy views unless requested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<Detail<Vec<Symbol>>>,
    pub diff: Detail<FileDiff>,
}

/// Successful status collection does not claim an atomic source/Git snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum GitCollectionState {
    NotRequested,
    Available,
    Unavailable { reason: String },
}

/// Identifies the Git status observation; HEAD can be absent in a new repository.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GitSummary {
    pub state: GitCollectionState,
    pub head: Option<String>,
}

/// Counts returned nodes and source bytes consumed for enrichment, excluding Git subprocess I/O.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StructureSummary {
    pub files_shown: usize,
    pub directories_shown: usize,
    pub source_bytes_read: usize,
    /// File detail sets reused only after reading and hashing the current source bytes.
    #[serde(default)]
    pub source_cache_hits: usize,
    pub truncated: bool,
}

/// Per-path limitations remain visible to both machine and text consumers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StructureWarning {
    pub path: Option<PathBuf>,
    pub reason: String,
}

/// Owned result reusable by renderers, classifiers and agent-context builders.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectStructure {
    pub root: PathBuf,
    pub options: StructureOptions,
    pub tree: StructureNode,
    pub git: GitSummary,
    pub summary: StructureSummary,
    pub warnings: Vec<StructureWarning>,
}

/// Invalid options, root access and mandatory Git failures use ordinary I/O errors.
pub type StructureError = io::Error;

/// Builds a bounded directory tree; source outlines use the process's registered languages.
///
/// Paths in options must be normal relative paths. Root canonicalization is explicit; child
/// symlinks are not followed. This is a local operator utility, not a session authority boundary.
/// Child read failures are warnings. Invalid root/options and failed changed-only status are errors.
pub fn build_structure(
    root: &Path,
    options: &StructureOptions,
) -> Result<ProjectStructure, StructureError> {
    build_structure_cached(root, options, &BTreeMap::new())
}

/// Reuses caller-validated detail records. The caller namespaces cache entries by extraction
/// settings and executable identity; current bytes are still read under normal limits.
pub(crate) fn build_structure_cached(
    root: &Path,
    options: &StructureOptions,
    cache: &BTreeMap<PathBuf, StructureNode>,
) -> Result<ProjectStructure, StructureError> {
    build_structure_cancellable(root, options, cache, &|| false)
}

/// Builds cached facts cooperatively, stopping directory and source work between bounded units.
pub(crate) fn build_structure_cancellable(
    root: &Path,
    options: &StructureOptions,
    cache: &BTreeMap<PathBuf, StructureNode>,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<ProjectStructure, StructureError> {
    check_cancelled(cancelled)?;
    validate(options)?;
    let root = root.canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "root must be a directory",
        ));
    }
    let mut warnings = Vec::new();
    let ignored = if options.respect_gitignore {
        match git::ignored(&root) {
            Ok(paths) => paths,
            Err(error) => {
                warnings.push(StructureWarning {
                    path: None,
                    reason: format!("gitignore unavailable: {error}"),
                });
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let (changes, git) = if options.include_diff || options.changed_only {
        match git::collect(&root, options.include_diff, &mut warnings) {
            Ok(value) => value,
            Err(error) if options.changed_only => {
                return Err(io::Error::other(format!(
                    "changed-only requires Git status: {error}"
                )));
            }
            Err(error) => (
                BTreeMap::new(),
                GitSummary {
                    state: GitCollectionState::Unavailable {
                        reason: error.to_string(),
                    },
                    head: None,
                },
            ),
        }
    } else {
        (
            BTreeMap::new(),
            GitSummary {
                state: GitCollectionState::NotRequested,
                head: None,
            },
        )
    };
    let mut walk = Walk {
        root: &root,
        options,
        ignored,
        changes,
        warnings,
        summary: StructureSummary::default(),
        nodes: 0,
        git_available: matches!(git.state, GitCollectionState::Available),
        cache,
        cancelled,
    };
    let mut tree = walk.node(Path::new(""), NodeKind::Directory, 0);
    check_cancelled(cancelled)?;
    walk.enrich_tree(&mut tree)?;
    check_cancelled(cancelled)?;
    walk.summary.files_shown = 0;
    walk.summary.directories_shown = 0;
    count_shown(&tree, &mut walk.summary);
    Ok(ProjectStructure {
        root: root.clone(),
        options: options.clone(),
        tree,
        git,
        summary: walk.summary,
        warnings: walk.warnings,
    })
}

/// Rejects ambiguous paths and invalid budgets before traversal or Git access.
fn validate(options: &StructureOptions) -> io::Result<()> {
    if !(1..=16).contains(&options.workers)
        || options.max_depth > 64
        || options.max_nodes == 0
        || options.max_nodes > 100_000
        || options.max_file_bytes == 0
        || options.max_total_read_bytes == 0
        || options.max_header_lines == 0
        || options.max_skeleton_lines == 0
        || options.max_imports == 0
        || options.max_file_bytes > 16 * 1024 * 1024
        || options.max_total_read_bytes > 128 * 1024 * 1024
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid structure limits",
        ));
    }
    for path in options.exclude_paths.iter().chain(&options.expand_paths) {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filters must be normal relative paths",
            ));
        }
    }
    if options
        .extensions
        .iter()
        .any(|s| s.is_empty() || s.contains(['/', '\\', '.']))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "extensions must omit dots and separators",
        ));
    }
    Ok(())
}

/// One synchronous collection's budgets and already-collected Git observations.
struct Walk<'a> {
    root: &'a Path,
    options: &'a StructureOptions,
    ignored: Vec<PathBuf>,
    changes: BTreeMap<PathBuf, FileDiff>,
    warnings: Vec<StructureWarning>,
    summary: StructureSummary,
    nodes: usize,
    git_available: bool,
    cache: &'a BTreeMap<PathBuf, StructureNode>,
    cancelled: &'a (dyn Fn() -> bool + Sync),
}

impl Walk<'_> {
    /// Records an English limitation without turning incomplete evidence into an empty fact.
    fn warn(&mut self, path: &Path, reason: impl Into<String>) {
        self.warnings.push(StructureWarning {
            path: Some(path.into()),
            reason: reason.into(),
        });
    }

    /// Applies name/path filters before descending or enriching file contents.
    fn selected(&self, path: &Path, kind: &NodeKind) -> bool {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if matches!(name.as_ref(), ".git" | ".agent-ide")
            || (!self.options.include_hidden && name.starts_with('.'))
        {
            return false;
        }
        if self.options.exclude_generated
            && matches!(kind, NodeKind::Directory)
            && matches!(
                name.as_ref(),
                "target"
                    | "dist"
                    | "build"
                    | "node_modules"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".cache"
                    | ".agent-ide"
                    | ".codegraph"
            )
        {
            return false;
        }
        if self
            .options
            .exclude_paths
            .iter()
            .chain(&self.ignored)
            .any(|p| path.starts_with(p))
        {
            return false;
        }
        if self.options.changed_only
            && !self
                .changes
                .range(path.to_path_buf()..)
                .next()
                .is_some_and(|(p, _)| {
                    p == path || matches!(kind, NodeKind::Directory) && p.starts_with(path)
                })
        {
            return false;
        }
        if !matches!(kind, NodeKind::Directory) {
            if !self.options.include_files {
                return false;
            }
            if !self.options.extensions.is_empty()
                && !path
                    .extension()
                    .is_some_and(|ext| self.options.extensions.iter().any(|s| ext == s.as_str()))
            {
                return false;
            }
        }
        true
    }

    /// Lists directories first and names lexically; virtual deleted paths keep their ancestors.
    fn entries(&mut self, relative: &Path) -> BTreeMap<OsString, (PathBuf, NodeKind)> {
        let mut found = BTreeMap::new();
        if !self.options.changed_only {
            match fs::read_dir(self.root.join(relative)) {
                Ok(entries) => {
                    for (i, entry) in entries.enumerate() {
                        if (self.cancelled)() {
                            break;
                        }
                        if i >= self.options.max_nodes {
                            self.summary.truncated = true;
                            self.warn(
                                relative,
                                "directory entry limit reached; filesystem enumeration omitted",
                            );
                            found.clear();
                            break;
                        }
                        match entry.and_then(|e| Ok((e.file_name(), e.file_type()?))) {
                            Ok((name, kind)) => {
                                found.insert(name.clone(), (relative.join(name), node_kind(kind)));
                            }
                            Err(error) => {
                                self.warn(relative, format!("directory entry unavailable: {error}"))
                            }
                        }
                    }
                }
                Err(error)
                    if self.changes.keys().any(|p| p.starts_with(relative))
                        && error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => self.warn(relative, format!("directory unavailable: {error}")),
            }
        }
        // A component prefix is contiguous in Path ordering. Retain only immediate child names
        // and whether they have virtual descendants, without cloning the complete changes map.
        let mut children = BTreeMap::<OsString, bool>::new();
        for (path, _) in self
            .changes
            .range(relative.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(relative))
        {
            let Ok(tail) = path.strip_prefix(relative) else {
                continue;
            };
            let mut parts = tail.components();
            let Some(Component::Normal(name)) = parts.next() else {
                continue;
            };
            let descendants = parts.next().is_some();
            *children.entry(name.to_owned()).or_default() |= descendants;
        }
        for (name, descendants) in children {
            if found.contains_key(&name) {
                continue;
            }
            let child = relative.join(&name);
            let kind = match fs::symlink_metadata(self.root.join(&child)) {
                Ok(meta) => node_kind(meta.file_type()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if descendants {
                        NodeKind::Directory
                    } else {
                        NodeKind::Deleted
                    }
                }
                Err(error) => {
                    self.warn(&child, format!("entry metadata unavailable: {error}"));
                    NodeKind::Other
                }
            };
            found.insert(name, (child, kind));
        }
        found
    }

    /// Collects one entry, preserving depth/budget truncation rather than pretending completeness.
    fn node(&mut self, path: &Path, kind: NodeKind, depth: usize) -> StructureNode {
        self.nodes += 1;
        let mut node = StructureNode {
            path: path.into(),
            kind,
            children: Vec::new(),
            collapsed: false,
            truncated: false,
            size_bytes: None,
            line_count: None,
            language: None,
            source_hash: None,
            header: Detail::Disabled,
            skeleton: Detail::Disabled,
            symbols: None,
            imports: Detail::Disabled,
            diff: Detail::Disabled,
        };
        if (self.cancelled)() {
            node.truncated = true;
            return node;
        }
        if node.kind == NodeKind::Directory {
            if self.options.include_diff
                && let Some(diff) = self.changes.get(path)
            {
                node.diff = Detail::Available(diff.clone());
            }
            self.summary.directories_shown += 1;
            if depth >= self.options.max_depth
                || (!path.as_os_str().is_empty()
                    && !self.options.expand_paths.is_empty()
                    && !self
                        .options
                        .expand_paths
                        .iter()
                        .any(|p| p.starts_with(path) || path.starts_with(p)))
            {
                node.collapsed = true;
                return node;
            }
            let warnings_before = self.warnings.len();
            let mut entries: Vec<_> = self
                .entries(path)
                .into_values()
                .filter(|(p, k)| self.selected(p, k))
                .collect();
            if self.warnings.len() > warnings_before {
                node.truncated = true;
                self.summary.truncated = true;
            }
            entries.sort_by(|(a, ak), (b, bk)| {
                (ak != &NodeKind::Directory)
                    .cmp(&(bk != &NodeKind::Directory))
                    .then(a.cmp(b))
            });
            for (child, kind) in entries {
                if self.nodes >= self.options.max_nodes {
                    node.truncated = true;
                    self.summary.truncated = true;
                    break;
                }
                let child = self.node(&child, kind, depth + 1);
                if child.kind != NodeKind::Directory
                    || child.collapsed
                    || child.truncated
                    || !child.children.is_empty()
                    || self.changes.contains_key(&child.path)
                    || (!self.options.changed_only && self.options.extensions.is_empty())
                    || !self.options.include_files
                {
                    node.children.push(child);
                }
            }
        } else {
            self.summary.files_shown += 1;
            if self.options.include_diff {
                node.diff = if self.git_available {
                    Detail::Available(self.changes.get(path).cloned().unwrap_or(FileDiff {
                        status: ChangeStatus::Unchanged,
                        staged: DiffCounts::zero(),
                        unstaged: DiffCounts::zero(),
                        previous_path: None,
                    }))
                } else {
                    Detail::Unavailable {
                        reason: "Git status unavailable".into(),
                    }
                };
            }
            if node.kind != NodeKind::File {
                if self.options.include_header {
                    node.header = Detail::Unavailable {
                        reason: "not a regular source file".into(),
                    };
                }
                if self.options.include_skeleton {
                    node.skeleton = Detail::Unavailable {
                        reason: "not a regular source file".into(),
                    };
                }
                if self.options.include_imports {
                    node.imports = Detail::Unavailable {
                        reason: "not a regular source file".into(),
                    };
                }
            }
        }
        node
    }

    /// Reads files in stable tree order, overlapping bounded parser work without changing
    /// byte-budget winners. Scoped workers own disjoint nodes and are joined before return.
    fn enrich_tree(&mut self, tree: &mut StructureNode) -> io::Result<()> {
        let mut nodes = Vec::new();
        source_nodes(tree, &mut nodes);
        if self.options.workers == 1
            || nodes.len() < 2
            || !(self.options.include_header
                || self.options.include_imports
                || self.options.include_skeleton
                || self.options.include_symbols
                || self.options.include_line_count)
        {
            for node in nodes {
                check_cancelled(self.cancelled)?;
                if let Some((language, source)) = self.prepare_source(node) {
                    self.warnings
                        .extend(enrich_source(node, language, &source, self.options));
                }
                self.summary.truncated |= node.truncated;
            }
            return Ok(());
        }
        let workers = self.options.workers.min(nodes.len());
        let options = self.options;
        // Rendezvous bounds retained source buffers to active workers plus this reader.
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<(usize, &mut StructureNode, Language, String)>(0);
        let receiver = std::sync::Arc::new(std::sync::Mutex::new(receiver));
        let outcomes = std::thread::scope(|scope| -> io::Result<_> {
            let mut handles = Vec::new();
            for index in 0..workers {
                let receiver = std::sync::Arc::clone(&receiver);
                handles.push(
                    std::thread::Builder::new()
                        .name(format!("structure-{index}"))
                        .spawn_scoped(scope, move || {
                            let mut outcomes = Vec::new();
                            loop {
                                let job = receiver.lock().expect("structure queue poisoned").recv();
                                let Ok((index, node, language, source)) = job else {
                                    break;
                                };
                                let warnings = enrich_source(node, language, &source, options);
                                outcomes.push((index, warnings, node.truncated));
                            }
                            outcomes
                        })?,
                );
            }
            // Only workers retain the receiving end: their collective failure disconnects
            // the reader instead of leaving it blocked on a receiver owned by the scope.
            drop(receiver);
            let mut outcomes = Vec::new();
            let mut failed = false;
            for (index, node) in nodes.into_iter().enumerate() {
                if (self.cancelled)() {
                    break;
                }
                let start = self.warnings.len();
                let prepared = self.prepare_source(node);
                let warnings = self.warnings.split_off(start);
                outcomes.push((index, warnings, node.truncated));
                if let Some((language, source)) = prepared
                    && sender.send((index, node, language, source)).is_err()
                {
                    failed = true;
                    break;
                }
            }
            drop(sender);
            for handle in handles {
                match handle.join() {
                    Ok(results) => outcomes.extend(results),
                    Err(_) => failed = true,
                }
            }
            if failed {
                Err(io::Error::other("structure parser failed"))
            } else {
                Ok(outcomes)
            }
        })?;
        let mut outcomes = outcomes;
        outcomes.sort_by_key(|(index, _, _)| *index);
        for (_, warnings, truncated) in outcomes {
            self.warnings.extend(warnings);
            self.summary.truncated |= truncated;
        }
        Ok(())
    }

    /// Opens only regular source files once, charging the ordered aggregate byte budget.
    fn prepare_source(&mut self, node: &mut StructureNode) -> Option<(Language, String)> {
        let path = self.root.join(&node.path);
        if self.options.include_size {
            match fs::symlink_metadata(&path) {
                Ok(m) => node.size_bytes = Some(m.len()),
                Err(e) => self.warn(&node.path, format!("size unavailable: {e}")),
            }
        }
        let language = Language::for_path(&node.path);
        node.language = language.map(|l| l.descriptor().id.into());
        if !(self.options.include_header
            || self.options.include_skeleton
            || self.options.include_symbols
            || self.options.include_imports
            || self.options.include_line_count)
        {
            return None;
        }
        // Only registered source files are opened; metadata-only manifests and secret files stay closed.
        let Some(language) = language else {
            if self.options.include_header {
                node.header = Detail::Unavailable {
                    reason: "source language not registered".into(),
                };
            }
            if self.options.include_symbols {
                node.symbols = Some(Detail::Unavailable {
                    reason: "source language not registered".into(),
                });
            }
            if self.options.include_skeleton {
                node.skeleton = Detail::Unavailable {
                    reason: "source language not registered".into(),
                };
            }
            if self.options.include_imports {
                node.imports = Detail::Unavailable {
                    reason: "source language not registered".into(),
                };
            }
            return None;
        };
        let remaining = self
            .options
            .max_total_read_bytes
            .saturating_sub(self.summary.source_bytes_read);
        let limit = remaining.min(self.options.max_file_bytes);
        let result = (|| -> io::Result<String> {
            if limit == 0 {
                return Err(io::Error::other("aggregate source byte limit reached"));
            }
            // Revalidate static ancestors before opening; this local walk is not a concurrent sandbox.
            if path.parent().and_then(|p| p.canonicalize().ok()).as_deref() != path.parent() {
                return Err(io::Error::other("source ancestor changed or is a symlink"));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(io::Error::other("not a regular file"));
            }
            if metadata.len() > limit as u64 {
                return Err(io::Error::other("source byte limit reached"));
            }
            let mut bytes = Vec::new();
            file.by_ref()
                .take(limit.saturating_add(1) as u64)
                .read_to_end(&mut bytes)?;
            self.summary.source_bytes_read += bytes.len();
            if bytes.len() > limit {
                return Err(io::Error::other("source grew beyond byte limit"));
            }
            String::from_utf8(bytes).map_err(|_| io::Error::other("source is not UTF-8 text"))
        })();
        let source = match result {
            Ok(source) => source,
            Err(error) => {
                let reason = error.to_string();
                if reason.contains("limit") {
                    node.truncated = true;
                    self.summary.truncated = true;
                }
                self.warn(&node.path, format!("source details unavailable: {reason}"));
                if self.options.include_header {
                    node.header = Detail::Unavailable {
                        reason: reason.clone(),
                    };
                }
                if self.options.include_symbols {
                    node.symbols = Some(Detail::Unavailable {
                        reason: reason.clone(),
                    });
                }
                if self.options.include_skeleton {
                    node.skeleton = Detail::Unavailable {
                        reason: reason.clone(),
                    };
                }
                if self.options.include_imports {
                    node.imports = Detail::Unavailable { reason };
                }
                return None;
            }
        };
        let hash = blake3::hash(source.as_bytes()).to_hex().to_string();
        if let Some(cached) = self.cache.get(&node.path)
            && cached.source_hash.as_deref() == Some(hash.as_str())
            && !cached.truncated
        {
            node.source_hash = Some(hash);
            node.header = cached.header.clone();
            node.imports = cached.imports.clone();
            node.symbols = cached.symbols.clone();
            node.skeleton = cached.skeleton.clone();
            node.line_count = cached.line_count;
            self.summary.source_cache_hits += 1;
            return None;
        }
        Some((language, source))
    }
}

/// Collects disjoint file references in rendered order without copying nodes or source text.
fn source_nodes<'a>(node: &'a mut StructureNode, out: &mut Vec<&'a mut StructureNode>) {
    if node.kind == NodeKind::File {
        out.push(node);
    } else {
        for child in &mut node.children {
            source_nodes(child, out);
        }
    }
}

/// Enriches one exclusively owned node from immutable bytes; never reads files or shared state.
fn enrich_source(
    node: &mut StructureNode,
    language: Language,
    source: &str,
    options: &StructureOptions,
) -> Vec<StructureWarning> {
    let mut warnings = Vec::new();
    node.source_hash = Some(blake3::hash(source.as_bytes()).to_hex().to_string());
    if options.include_line_count {
        node.line_count = Some(line_count(source));
    }
    let lines: Vec<_> = source.lines().collect();
    if options.include_header {
        let head = regions::scan_gap(
            &lines,
            1,
            lines.len().min(options.max_header_lines.saturating_add(1)) as u32,
            true,
            language.support().region_rules(),
        )
        .into_iter()
        .find(|r| r.kind == regions::RawKind::Header);
        node.header = match head {
            Some(region) => {
                let end = (region.span.end as usize).min(options.max_header_lines);
                let text = lines[..end]
                    .iter()
                    .map(|line| header_text(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    Detail::Absent
                } else {
                    Detail::Available(SourceExcerpt {
                        text: text.trim_end().into(),
                        start_line: 1,
                        end_line: end as u32,
                        truncated: region.span.end as usize > options.max_header_lines,
                    })
                }
            }
            None => Detail::Absent,
        };
    }
    let (import_spans, outline) = language.support().source_details(
        &node.path,
        source,
        options.include_imports,
        options.include_skeleton || options.include_symbols,
    );
    if options.include_imports {
        if let Some(spans) = import_spans {
            let spans = coalesce_import_spans(spans);
            let mut statements = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut truncated = false;
            for span in spans {
                let text = compact_import(
                    &lines[span.start as usize - 1..span.end as usize].join("\n"),
                    language.support().region_rules(),
                );
                if !seen.insert(text.clone()) {
                    continue;
                }
                if statements.len() >= options.max_imports {
                    truncated = true;
                    break;
                }
                statements.push(SourceExcerpt {
                    text,
                    start_line: span.start,
                    end_line: span.end,
                    truncated: false,
                });
            }
            node.imports = if statements.is_empty() {
                Detail::Absent
            } else {
                Detail::Available(ImportSummary {
                    statements,
                    truncated,
                })
            };
        } else {
            node.imports = Detail::Unavailable {
                reason: "source import scan unavailable".into(),
            };
            warnings.push(StructureWarning {
                path: Some(node.path.clone()),
                reason: "source import scan unavailable".into(),
            });
        }
    }
    if options.include_symbols {
        node.symbols = Some(match &outline {
            Some(outline) => Detail::Available(outline.symbols.clone()),
            None => Detail::Unavailable {
                reason: "source-only outline unavailable".into(),
            },
        });
    }
    if options.include_skeleton {
        node.skeleton = match outline {
            Some(mut outline) => {
                if !options.include_skeleton_comments {
                    clear_symbol_docs(&mut outline.symbols);
                }
                let text = outline_text(&outline);
                let lines: Vec<_> = text.lines().skip(1).map(str::trim_start).collect();
                Detail::Available(CodeSkeleton {
                    text: lines
                        .iter()
                        .take(options.max_skeleton_lines)
                        .copied()
                        .collect::<Vec<_>>()
                        .join("\n"),
                    truncated: lines.len() > options.max_skeleton_lines,
                })
            }
            None => {
                warnings.push(StructureWarning { path: Some(node.path.clone()), reason: "source-only outline unavailable; syntax or parser limits may prevent collection".into() });
                Detail::Unavailable {
                    reason: "source-only outline unavailable".into(),
                }
            }
        };
    }
    if matches!(&node.header, Detail::Available(value) if value.truncated)
        || matches!(&node.skeleton, Detail::Available(value) if value.truncated)
        || matches!(&node.imports, Detail::Available(value) if value.truncated)
    {
        node.truncated = true;
    }
    warnings
}

/// Coalesces duplicate/overlapping physical import ranges before any source joins or copies.
/// Adjacent lines remain separate records; ordering is deterministic for every language producer.
fn coalesce_import_spans(mut spans: Vec<crate::lang::LineRange>) -> Vec<crate::lang::LineRange> {
    spans.sort_unstable_by_key(|span| (span.start, span.end));
    let mut out: Vec<crate::lang::LineRange> = Vec::with_capacity(spans.len());
    for span in spans {
        if let Some(last) = out.last_mut()
            && span.start <= last.end
        {
            last.end = last.end.max(span.end);
        } else {
            out.push(span);
        }
    }
    out
}

/// Removes comment trivia and condenses whitespace outside quoted import targets only.
fn compact_import(source: &str, rules: &regions::RegionRules) -> String {
    let mut out = String::new();
    let mut at = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut space = false;
    while at < source.len() {
        let rest = &source[at..];
        let ch = rest.chars().next().expect("remaining character");
        if let Some(end) = quote {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == end {
                quote = None;
            }
            at += ch.len_utf8();
            continue;
        }
        if rules
            .line_comments
            .iter()
            .any(|marker| rest.starts_with(marker))
        {
            at += rest.find('\n').unwrap_or(rest.len());
            space = true;
            continue;
        }
        if let Some((open, close)) = rules.block_comment
            && rest.starts_with(open)
        {
            let mut depth = 1;
            at += open.len();
            while at < source.len() && depth > 0 {
                if source[at..].starts_with(open) {
                    depth += 1;
                    at += open.len();
                } else if source[at..].starts_with(close) {
                    depth -= 1;
                    at += close.len();
                } else {
                    at += source[at..]
                        .chars()
                        .next()
                        .expect("remaining comment character")
                        .len_utf8();
                }
            }
            space = true;
            continue;
        }
        if rest.starts_with("\\\n") || rest.starts_with("\\\r\n") {
            at += if rest.starts_with("\\\r\n") { 3 } else { 2 };
            space = true;
            continue;
        }
        if ch.is_whitespace() {
            space = true;
        } else {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            if matches!(ch, '\'' | '"' | '`') {
                quote = Some(ch);
            }
            out.push(ch);
        }
        at += ch.len_utf8();
    }
    out
}

/// Counts only retained nodes; pruned empty filter ancestors do not contribute.
fn count_shown(node: &StructureNode, summary: &mut StructureSummary) {
    if node.kind == NodeKind::Directory {
        summary.directories_shown += 1;
    } else {
        summary.files_shown += 1;
    }
    for child in &node.children {
        count_shown(child, summary);
    }
}

/// Maps native file types without following links or opening special files.
fn node_kind(kind: fs::FileType) -> NodeKind {
    if kind.is_dir() {
        NodeKind::Directory
    } else if kind.is_file() {
        NodeKind::File
    } else if kind.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

/// Removes the repeated leading module-doc marker while preserving paragraph indentation.
pub(crate) fn header_text(line: &str) -> &str {
    line.trim_start()
        .strip_prefix("//!")
        .map(|text| text.strip_prefix(' ').unwrap_or(text))
        .unwrap_or(line)
}

/// Escapes control characters so filenames and excerpts cannot forge extra tree lines.
fn label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// Removes attached documentation without touching declaration text or quoted comment markers.
fn clear_symbol_docs(symbols: &mut [Symbol]) {
    for symbol in symbols {
        symbol.doc = None;
        clear_symbol_docs(&mut symbol.children);
    }
}

/// Renders an owned snapshot with one capability note, available details and genuine warnings.
/// Unsupported/absent detail blocks are omitted; typed JSON states remain explicit.
pub fn render_tree(structure: &ProjectStructure) -> String {
    let mut out = String::new();
    if structure.options.include_header
        || structure.options.include_imports
        || structure.options.include_skeleton
        || structure.options.include_line_count
    {
        out.push_str("Details: header/imports/skeleton are shown only when supported and present; other files list metadata only.\n");
    }
    if structure.options.include_diff {
        match &structure.git.state {
            GitCollectionState::Unavailable { reason } => {
                let _ = writeln!(out, "Git: unavailable ({}); omitted observations do not imply zero changes.", label(reason));
            }
            GitCollectionState::Available => out.push_str("Git: omitted changes/counters are zero; unknown or unavailable observations remain explicit.\n"),
            GitCollectionState::NotRequested => {}
        }
    }
    if structure.warnings.iter().any(git_policy_note) {
        out.push_str("Git: unfiltered comparisons; external content filters disabled.\n");
    }
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "{}/",
        label(
            &structure
                .root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )
    );
    render_children(&structure.tree, "", structure.options.compact, &mut out);
    if structure.tree.collapsed {
        out.push_str("[collapsed]\n");
    }
    if structure.summary.truncated {
        out.push_str("[truncated: tree or source detail limit reached]\n");
    }
    for warning in structure
        .warnings
        .iter()
        .filter(|warning| !git_policy_note(warning))
    {
        let path = warning
            .path
            .as_ref()
            .map(|p| format!("{}: ", label(&p.to_string_lossy())))
            .unwrap_or_default();
        let _ = writeln!(out, "warning: {path}{}", label(&warning.reason));
    }
    out
}

/// Recognizes only the known global Git policy; actual Git and per-file failures remain warnings.
fn git_policy_note(warning: &StructureWarning) -> bool {
    warning.path.is_none() && warning.reason == GIT_FILTER_NOTICE
}

/// Renders header, compact imports and skeleton in that order, without duplicate file headings.
fn render_children(node: &StructureNode, prefix: &str, compact: bool, out: &mut String) {
    for (i, child) in node.children.iter().enumerate() {
        let last = i + 1 == node.children.len();
        let branch = if compact {
            " "
        } else if last {
            "└ "
        } else {
            "├ "
        };
        let next = format!(
            "{prefix}{}",
            if compact {
                " "
            } else if last {
                "  "
            } else {
                "│ "
            }
        );
        let detail_prefix = format!("{next}{}", if compact { " " } else { "" });
        let detail_indent = if compact { " " } else { "  " };
        let name = label(&child.path.file_name().unwrap_or_default().to_string_lossy());
        let suffix = match child.kind {
            NodeKind::Directory => "/",
            NodeKind::Symlink => " [symlink; not followed]",
            NodeKind::Deleted => " [deleted]",
            NodeKind::Other => " [special; not opened]",
            NodeKind::File => "",
        };
        let _ = write!(out, "{prefix}{branch}{name}{suffix}");
        let mut facts = Vec::new();
        if let Some(n) = child.line_count {
            facts.push(format!("lines: {n}"));
        }
        if let Some(n) = child.size_bytes {
            facts.push(format!("bytes: {n}"));
        }
        if let Detail::Available(diff) = &child.diff {
            if diff.status != ChangeStatus::Unchanged {
                facts.push(format!("changes: {:?}", diff.status));
            }
            for (name, value) in [("staged", &diff.staged), ("unstaged", &diff.unstaged)] {
                if value != &DiffCounts::zero() {
                    facts.push(format!("{name} {}", counts(value)));
                }
            }
        } else if let Detail::Unavailable { .. } = &child.diff {
            facts.push("changes: unavailable".into());
        }
        if !facts.is_empty() {
            let _ = write!(out, " [{}]", facts.join("; "));
        }
        if child.collapsed {
            out.push_str(" …");
        }
        if child.truncated {
            out.push_str(" [truncated]");
        }
        out.push('\n');
        if let Detail::Available(header) = &child.header {
            let _ = writeln!(
                out,
                "{detail_prefix}header L{}-{}{}:",
                header.start_line,
                header.end_line,
                if header.truncated { " [truncated]" } else { "" }
            );
            for line in header.text.lines() {
                let _ = writeln!(out, "{detail_prefix}{detail_indent}{}", label(line));
            }
        }
        if let Detail::Available(imports) = &child.imports {
            let _ = writeln!(
                out,
                "{detail_prefix}imports{}:",
                if imports.truncated {
                    " [truncated]"
                } else {
                    ""
                }
            );
            for import in &imports.statements {
                let _ = writeln!(
                    out,
                    "{detail_prefix}{detail_indent}L{}-{}: {}",
                    import.start_line,
                    import.end_line,
                    label(&import.text)
                );
            }
        }
        if let Detail::Available(skeleton) = &child.skeleton {
            let _ = writeln!(
                out,
                "{detail_prefix}skeleton{}:",
                if skeleton.truncated {
                    " [truncated]"
                } else {
                    ""
                }
            );
            for line in skeleton.text.lines() {
                let _ = writeln!(out, "{detail_prefix}{detail_indent}{}", label(line));
            }
        }
        render_children(child, &next, compact, out);
    }
}

/// Formats unknown counts explicitly, including untracked/binary lines and bounded-out hunks.
fn counts(value: &DiffCounts) -> String {
    let n = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
    format!(
        "+{}/-{} (hunks: {})",
        n(value.added_lines),
        n(value.removed_lines),
        n(value.hunks)
    )
}

/// Converts a cooperative stop request into a non-successful, unpublished build.
fn check_cancelled(cancelled: &(dyn Fn() -> bool + Sync)) -> io::Result<()> {
    if cancelled() {
        Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "structure cancelled",
        ))
    } else {
        Ok(())
    }
}
