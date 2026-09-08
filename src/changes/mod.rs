//! Bounded diff composition for Workspace raw Git evidence in Changes v0.1.

use std::{collections::BTreeMap, os::unix::ffi::OsStrExt, path::PathBuf};

use crate::workspace::git::{
    BaselineCoverage, BaselineWindow, DiffMode, GitComparison, GitReadQuery, GitScope, GitStatus,
    PathStatus, RawGitEvidence,
};

/// Maximum number of hunks selected by default for one bounded composition.
pub const DEFAULT_MAX_HUNKS: usize = 32;
/// Maximum number of raw hunk bytes selected by default for one bounded composition.
pub const DEFAULT_MAX_HUNK_BYTES: usize = 32 * 1024;

/// Outcome state for one bounded diff composition request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffResultState {
    /// Requested evidence was available, scopes were coherent, and selected payload meets budget.
    Ready,
    /// Evidence was structurally unavailable for this request because scope or metadata did not match.
    Unavailable,
    /// Evidence existed but was incomplete due truncation, budget overflow, or unsupported evidence shape.
    Incomplete,
    /// Evidence exists but indicates command or execution failure.
    Failed,
}

/// Coverage of logical diff material returned by composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffCoverage {
    /// Every supported status and selected hunk was parsed and budget limits were never hit.
    Complete,
    /// Some status/hunk material was omitted due truncation or budget limit.
    Partial,
    /// Coverage cannot be claimed because evidence metadata was not trusted for this request.
    Unknown,
}

/// Currentness of the evidence relative to the requested owner scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffFreshness {
    /// Request metadata and evidence metadata matched exactly.
    Current,
    /// Evidence belongs to the same worktree incarnation but an older authority epoch.
    Stale,
    /// Evidence metadata could not be confirmed as current for this request.
    Unknown,
}

/// Bounded request budget for exact hunk selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffSelectionBudget {
    /// Maximum number of hunks that may be selected.
    pub max_hunks: usize,
    /// Maximum number of bytes across selected hunks.
    pub max_bytes: usize,
}

impl Default for DiffSelectionBudget {
    /// Returns the module defaults for one composition pass.
    fn default() -> Self {
        Self::bounded(DEFAULT_MAX_HUNKS, DEFAULT_MAX_HUNK_BYTES)
    }
}

impl DiffSelectionBudget {
    /// Builds a bounded budget; zero and negative-size requests intentionally return zero budgets.
    pub const fn bounded(max_hunks: usize, max_bytes: usize) -> Self {
        Self {
            max_hunks,
            max_bytes,
        }
    }
}

/// Tracks which exact compare identity pair was requested by Workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffComparisonIdentities {
    left: Vec<u8>,
    right: Vec<u8>,
}

impl DiffComparisonIdentities {
    /// Builds owned left/right identities for payload and audit comparison.
    pub fn new(left: &[u8], right: &[u8]) -> Self {
        Self {
            left: left.to_vec(),
            right: to_owned(right),
        }
    }

    /// Returns the exact left identity bytes.
    pub fn left(&self) -> &[u8] {
        &self.left
    }

    /// Returns the exact right identity bytes.
    pub fn right(&self) -> &[u8] {
        &self.right
    }
}

/// Bounded hunk payload selected from raw evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    index: usize,
    path: Option<PathBuf>,
    patch: Vec<u8>,
    is_binary: bool,
}

impl DiffHunk {
    /// Returns the 0-based position of this hunk in the unbounded raw hunk stream.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Returns a path only when Workspace supplied an exact, unambiguous mapping.
    ///
    /// Header bytes must exactly match one scoped status path pair, including Git quoting.
    /// Unsupported or ambiguous mappings are omitted and mark the result incomplete.
    pub fn path(&self) -> Option<&PathBuf> {
        self.path.as_ref()
    }

    /// Returns the exact raw hunk bytes, including header and context lines.
    pub fn patch(&self) -> &[u8] {
        &self.patch
    }

    /// Indicates whether this entry is a binary-change summary without a textual hunk body.
    pub const fn is_binary(&self) -> bool {
        self.is_binary
    }
}

/// Tracks cursor and scope data used for bounded detail inspection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffDetailCursor {
    operation_reference: String,
    next_hunk: usize,
}

impl DiffDetailCursor {
    /// Builds one owner-scoped hunk cursor for detail expansion.
    pub fn new(operation_reference: impl Into<String>, next_hunk: usize) -> Self {
        Self {
            operation_reference: operation_reference.into(),
            next_hunk,
        }
    }

    /// Returns the owning operation reference for owner-scoped detail requests.
    pub fn operation_reference(&self) -> &str {
        &self.operation_reference
    }

    /// Returns the first omitted hunk index that a bounded inspector may request.
    pub const fn next_hunk(&self) -> usize {
        self.next_hunk
    }
}

/// Counts affected paths for one bounded compare mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffStatusCounts {
    /// Files represented by ordinary tracked status records.
    tracked: usize,
    /// Files represented by conflict records.
    conflicted: usize,
    /// Untracked files listed separately from tracked/conflict records.
    untracked: usize,
    /// Ignored files retained as evidence context but not part of requested diff groups.
    ignored: usize,
}

impl DiffStatusCounts {
    /// Builds exact counts from one parsed status evidence object.
    pub fn from_status(status: &GitStatus) -> Self {
        Self {
            tracked: status.tracked().len(),
            conflicted: status.conflicts().len(),
            untracked: status.untracked().len(),
            ignored: status.ignored().len(),
        }
    }

    /// Returns tracked and ordinary status count.
    pub const fn tracked(&self) -> usize {
        self.tracked
    }

    /// Returns conflict status count.
    pub const fn conflicted(&self) -> usize {
        self.conflicted
    }

    /// Returns untracked status count.
    pub const fn untracked(&self) -> usize {
        self.untracked
    }

    /// Returns ignored status count.
    pub const fn ignored(&self) -> usize {
        self.ignored
    }
}

/// Describes owner-scoped freshness, coverage, and provenance metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffProvenance {
    /// Owner-scoped raw execution reference, absent for unavailable evidence.
    operation_reference: Option<String>,
    /// Baseline context reference, never a Git comparison side.
    baseline_reference: Option<String>,
    /// Explicit baseline coverage, absent when scope checks fail.
    baseline_coverage: Option<BaselineCoverage>,
    /// Whether baseline context was captured and its joint window verified.
    baseline_window: Option<BaselineWindow>,
}

impl DiffProvenance {
    /// Builds provenance for one compose result.
    pub fn new(
        operation_reference: Option<String>,
        baseline_reference: Option<String>,
        baseline_coverage: Option<BaselineCoverage>,
        baseline_window: Option<BaselineWindow>,
    ) -> Self {
        Self {
            operation_reference,
            baseline_reference,
            baseline_coverage,
            baseline_window,
        }
    }

    /// Returns the owner-scoped operation reference returned by Workspace.
    pub fn operation_reference(&self) -> Option<&str> {
        self.operation_reference.as_deref()
    }

    /// Returns baseline provenance when Workspace context was present for this request.
    pub fn baseline_reference(&self) -> Option<&str> {
        self.baseline_reference.as_deref()
    }

    /// Returns the explicit baseline capture-window limitation when scoped context is available.
    pub const fn baseline_window(&self) -> Option<BaselineWindow> {
        self.baseline_window
    }

    /// Returns baseline completeness when Workspace context was present for this request.
    pub fn baseline_coverage(&self) -> Option<BaselineCoverage> {
        self.baseline_coverage
    }
}

/// Bounded diff composition result exposed to Assistance formatting/rendering layers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffResult {
    state: DiffResultState,
    freshness: DiffFreshness,
    coverage: DiffCoverage,
    scope_mode: DiffMode,
    authority_epoch: u64,
    worktree_id: String,
    identities: DiffComparisonIdentities,
    status_counts: DiffStatusCounts,
    selected_hunks: Vec<DiffHunk>,
    truncated_output: bool,
    overflow_hunks: usize,
    overflow_bytes: usize,
    untracked: Vec<PathStatus>,
    conflicts: Vec<PathStatus>,
    ignored: Vec<PathStatus>,
    detail_cursor: Option<DiffDetailCursor>,
    provenance: DiffProvenance,
}

impl DiffResult {
    /// Returns bounded outcome for caller routing and downstream rendering.
    pub const fn state(&self) -> DiffResultState {
        self.state
    }

    /// Returns evidence freshness relative to the requested owner scope.
    pub const fn freshness(&self) -> DiffFreshness {
        self.freshness
    }

    /// Returns material coverage for selected hunks and status detail.
    pub const fn coverage(&self) -> DiffCoverage {
        self.coverage
    }

    /// Returns compare mode used by this request.
    pub const fn mode(&self) -> DiffMode {
        self.scope_mode
    }

    /// Returns the owner epoch required for deterministic reauthorization checks.
    pub const fn authority_epoch(&self) -> u64 {
        self.authority_epoch
    }

    /// Returns the canonical worktree ID tied to this compose result.
    pub fn worktree_id(&self) -> &str {
        &self.worktree_id
    }

    /// Returns left/right exact comparison identities.
    pub fn identities(&self) -> &DiffComparisonIdentities {
        &self.identities
    }

    /// Returns exact status counts used for summary rendering.
    pub fn counts(&self) -> &DiffStatusCounts {
        &self.status_counts
    }

    /// Returns selected hunks that satisfied exact byte/hunk budgets.
    pub fn selected_hunks(&self) -> &[DiffHunk] {
        &self.selected_hunks
    }

    /// Returns explicit untracked paths preserved by Workspace parsing.
    pub fn untracked(&self) -> &[PathStatus] {
        &self.untracked
    }

    /// Returns explicit conflict paths preserved by Workspace parsing.
    pub fn conflicts(&self) -> &[PathStatus] {
        &self.conflicts
    }

    /// Returns ignored-path context when Workspace supplied it.
    pub fn ignored(&self) -> &[PathStatus] {
        &self.ignored
    }

    /// Returns whether raw stdout/stderr had bounded process truncation.
    pub const fn truncated_output(&self) -> bool {
        self.truncated_output
    }

    /// Returns the count of omitted hunks due budget policy.
    pub const fn overflow_hunks(&self) -> usize {
        self.overflow_hunks
    }

    /// Returns the count of omitted bytes due budget policy.
    pub const fn overflow_bytes(&self) -> usize {
        self.overflow_bytes
    }

    /// Returns owner-scoped cursor and operation reference for bounded detail expansion.
    pub fn detail_cursor(&self) -> Option<&DiffDetailCursor> {
        self.detail_cursor.as_ref()
    }

    /// Returns operation/reference-level provenance for rendering and follow-up expansion.
    pub fn provenance(&self) -> &DiffProvenance {
        &self.provenance
    }
}

/// Composes bounded hunks only from matching comparison/patch scopes and scoped status.
/// Mismatches return unavailable with no payload; unassociated paths are omitted as incomplete.
pub fn compose_diff(
    expected_scope: &GitScope,
    comparison: &GitComparison,
    status: GitStatus,
    evidence: RawGitEvidence,
    budget: DiffSelectionBudget,
) -> DiffResult {
    let same_worktree = evidence.scope().worktree() == expected_scope.worktree();
    let stale = same_worktree
        && evidence.scope().authority_epoch() != expected_scope.authority_epoch()
        && evidence.scope().mode() == expected_scope.mode()
        && comparison.mode() == expected_scope.mode();
    let status_matches = status.scope().is_some_and(|scope| {
        scope.worktree() == expected_scope.worktree()
            && scope.authority_epoch() == expected_scope.authority_epoch()
    });
    if evidence.scope() != expected_scope
        || comparison.scope() != expected_scope
        || evidence.query() != GitReadQuery::diff_for(expected_scope.mode())
        || !status_matches
        || !comparison.baseline().matches_scope(expected_scope)
    {
        return DiffResult {
            state: DiffResultState::Unavailable,
            freshness: if stale {
                DiffFreshness::Stale
            } else {
                DiffFreshness::Unknown
            },
            coverage: DiffCoverage::Unknown,
            scope_mode: expected_scope.mode(),
            authority_epoch: expected_scope.authority_epoch(),
            worktree_id: expected_scope.worktree().id().to_owned(),
            identities: DiffComparisonIdentities::new(&[], &[]),
            status_counts: DiffStatusCounts {
                tracked: 0,
                conflicted: 0,
                untracked: 0,
                ignored: 0,
            },
            selected_hunks: Vec::new(),
            truncated_output: false,
            overflow_hunks: 0,
            overflow_bytes: 0,
            untracked: Vec::new(),
            conflicts: Vec::new(),
            ignored: Vec::new(),
            detail_cursor: None,
            provenance: DiffProvenance::new(None, None, None, None),
        };
    }

    let mut state = DiffResultState::Ready;
    let mut coverage = DiffCoverage::Complete;
    let mut detail_cursor = None;

    if evidence.exit_code() != Some(0) {
        state = DiffResultState::Failed;
        coverage = DiffCoverage::Unknown;
    } else if evidence.exit_code().is_none() {
        state = DiffResultState::Incomplete;
        coverage = DiffCoverage::Partial;
    }

    let truncated_output = evidence.is_truncated();
    if truncated_output && state != DiffResultState::Failed {
        state = DiffResultState::Incomplete;
        coverage = DiffCoverage::Partial;
    }

    let parsed = parse_diff_hunks(evidence.stdout(), &status);
    let malformed = parsed.malformed;
    let has_binary = parsed.has_binary;
    let raw_hunks = parsed.hunks;

    if malformed && state != DiffResultState::Failed {
        state = DiffResultState::Incomplete;
        coverage = DiffCoverage::Unknown;
    }

    if has_binary {
        state = match state {
            DiffResultState::Ready => DiffResultState::Incomplete,
            previous => previous,
        };
        coverage = match coverage {
            DiffCoverage::Complete => DiffCoverage::Partial,
            current => current,
        };
    }

    let (selected_hunks, overflow_hunks, overflow_bytes, cursor_offset) =
        select_hunks(raw_hunks, budget);
    if overflow_hunks > 0 {
        state = match state {
            DiffResultState::Ready => DiffResultState::Incomplete,
            previous => previous,
        };
        coverage = match coverage {
            DiffCoverage::Complete => DiffCoverage::Partial,
            current => current,
        };
        detail_cursor = cursor_offset
            .map(|offset| DiffDetailCursor::new(evidence.operation_reference(), offset));
    }

    DiffResult {
        state,
        freshness: DiffFreshness::Current,
        coverage,
        scope_mode: expected_scope.mode(),
        authority_epoch: expected_scope.authority_epoch(),
        worktree_id: expected_scope.worktree().id().to_owned(),
        identities: DiffComparisonIdentities::new(
            comparison.left().as_bytes(),
            comparison.right().as_bytes(),
        ),
        status_counts: DiffStatusCounts::from_status(&status),
        selected_hunks,
        truncated_output,
        overflow_hunks,
        overflow_bytes,
        untracked: status.untracked().to_vec(),
        conflicts: status.conflicts().to_vec(),
        ignored: status.ignored().to_vec(),
        detail_cursor,
        provenance: DiffProvenance::new(
            Some(evidence.operation_reference().to_owned()),
            Some(comparison.baseline().reference().to_owned()),
            Some(comparison.baseline().coverage()),
            Some(comparison.baseline().window()),
        ),
    }
}

/// Selects exact hunks without splitting payload under configured boundaries.
fn select_hunks(
    hunks: Vec<RawHunk>,
    budget: DiffSelectionBudget,
) -> (Vec<DiffHunk>, usize, usize, Option<usize>) {
    let mut selected = Vec::new();
    let mut selected_bytes = 0usize;
    let mut omitted = 0usize;
    let mut omitted_bytes = 0usize;
    let mut cursor = None;

    for hunk in hunks {
        let next = selected.len();
        if next < budget.max_hunks && selected_bytes + hunk.patch.len() <= budget.max_bytes {
            selected_bytes += hunk.patch.len();
            selected.push(DiffHunk {
                index: hunk.original_index,
                path: hunk.path,
                patch: hunk.patch,
                is_binary: hunk.binary,
            });
            continue;
        }

        omitted += 1;
        omitted_bytes += hunk.patch.len();
        if cursor.is_none() {
            cursor = Some(hunk.original_index);
        }
    }

    (selected, omitted, omitted_bytes, cursor)
}

/// Parsed, owner-scoped raw diff hunk before budget selection.
struct RawHunk {
    /// Zero-based position in the raw hunk stream.
    original_index: usize,
    /// Path supplied only when an exact Workspace status mapping exists.
    path: Option<PathBuf>,
    /// Complete raw patch bytes.
    patch: Vec<u8>,
    /// Whether this entry is a binary summary.
    binary: bool,
}

/// Splits stdout into exact hunks, preserving complete patch units.
struct ParsedDiff {
    /// All full hunks successfully parsed from stdout.
    hunks: Vec<RawHunk>,
    /// Whether parser saw unsupported or malformed hunk structure.
    malformed: bool,
    /// Whether parser saw one or more binary summary lines.
    has_binary: bool,
}

/// Parses supported patch units against scoped raw path pairs; unmapped hunks are omitted as malformed.
fn parse_diff_hunks(stdout: &[u8], status: &GitStatus) -> ParsedDiff {
    if stdout.is_empty() {
        return ParsedDiff {
            hunks: Vec::new(),
            malformed: false,
            has_binary: false,
        };
    }

    let mut hunks = Vec::new();
    let mut index = 0usize;
    let mut cursor = 0usize;
    let mut malformed = false;
    let mut has_binary = false;
    let mut path = None;
    let paths = exact_diff_paths(status);

    while cursor < stdout.len() {
        let end = next_line_end(stdout, cursor);
        let line = &stdout[cursor..end];

        if line.starts_with(b"diff --git ") {
            path = paths.get(line).cloned().flatten();
            malformed |= path.is_none();
            cursor = end;
            continue;
        }

        if line.starts_with(b"Binary files ") || line.starts_with(b"Binary file ") {
            hunks.push(RawHunk {
                original_index: index,
                path: path.clone(),
                patch: line.to_vec(),
                binary: true,
            });
            has_binary = true;
            index += 1;
            cursor = end;
            continue;
        }

        if line.starts_with(b"@@") {
            let start = cursor;
            let mut next = end;
            let mut previous_end = end;
            loop {
                if next >= stdout.len() {
                    previous_end = next;
                    break;
                }
                next = next_line_end(stdout, next);
                if next == previous_end {
                    break;
                }
                let candidate = &stdout[previous_end..next];
                if !candidate.starts_with(b"+")
                    && !candidate.starts_with(b"-")
                    && !candidate.starts_with(b" ")
                    && !candidate.starts_with(b"\\")
                    && !candidate.starts_with(b"@@")
                    && !candidate.starts_with(b"diff --git ")
                {
                    malformed = true;
                }
                if candidate.starts_with(b"@@") || candidate.starts_with(b"diff --git ") {
                    break;
                }
                previous_end = next;
            }
            if previous_end > start {
                hunks.push(RawHunk {
                    original_index: index,
                    path: path.clone(),
                    patch: stdout[start..previous_end].to_vec(),
                    binary: false,
                });
                index += 1;
            }
            cursor = previous_end;
            continue;
        }

        if line != b"\n"
            && line != b""
            && !line.starts_with(b"diff --git ")
            && !line.starts_with(b"index ")
            && !line.starts_with(b"--- ")
            && !line.starts_with(b"+++ ")
            && !line.starts_with(b"\\")
        {
            malformed = true;
        }

        cursor = end;
    }

    malformed |= hunks.iter().any(|hunk| hunk.path.is_none());
    hunks.retain(|hunk| hunk.path.is_some());
    ParsedDiff {
        hunks,
        malformed,
        has_binary,
    }
}

/// Indexes entire default Git headers by exact raw status path pairs without splitting on spaces.
/// Duplicate headers map to None; unknown quoting/prefixes/rename attribution cannot select hunks.
fn exact_diff_paths(status: &GitStatus) -> BTreeMap<Vec<u8>, Option<PathBuf>> {
    let mut paths = BTreeMap::new();
    for entry in status.tracked().iter().chain(status.conflicts()) {
        let mut header = b"diff --git ".to_vec();
        header.extend(quoted_git_path(
            b"a/",
            entry
                .original_path()
                .unwrap_or(entry.path())
                .as_os_str()
                .as_bytes(),
        ));
        header.push(b' ');
        header.extend(quoted_git_path(b"b/", entry.path().as_os_str().as_bytes()));
        header.push(b'\n');
        paths
            .entry(header)
            .and_modify(|path| *path = None)
            .or_insert_with(|| Some(entry.path().to_path_buf()));
    }
    paths
}

/// Encodes a raw Git path with default quotePath C quoting, retaining every non-UTF-8 byte.
/// Unknown nondefault encodings are rejected by exact matching instead of guessed.
fn quoted_git_path(prefix: &[u8], path: &[u8]) -> Vec<u8> {
    if path
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte) && !matches!(byte, b'"' | b'\\'))
    {
        return [prefix, path].concat();
    }
    let mut result = vec![b'"'];
    result.extend_from_slice(prefix);
    for &byte in path {
        match byte {
            b'"' | b'\\' => result.extend_from_slice(&[b'\\', byte]),
            b'\n' => result.extend_from_slice(b"\\n"),
            b'\r' => result.extend_from_slice(b"\\r"),
            b'\t' => result.extend_from_slice(b"\\t"),
            7 => result.extend_from_slice(b"\\a"),
            8 => result.extend_from_slice(b"\\b"),
            11 => result.extend_from_slice(b"\\v"),
            12 => result.extend_from_slice(b"\\f"),
            b' '..=b'~' => result.push(byte),
            _ => result.extend_from_slice(&[
                b'\\',
                b'0' + (byte >> 6),
                b'0' + ((byte >> 3) & 7),
                b'0' + (byte & 7),
            ]),
        }
    }
    result.push(b'"');
    result
}

/// Returns the byte index after the next line terminator.
fn next_line_end(bytes: &[u8], start: usize) -> usize {
    let mut end = start;
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end + 1
    } else {
        bytes.len()
    }
}

/// Clones bytes into a Vec without forcing a UTF-8 interpretation.
fn to_owned(value: &[u8]) -> Vec<u8> {
    value.to_vec()
}
