//! Bounded diff composition for Workspace raw Git evidence in Changes v0.1.

use std::path::PathBuf;

use crate::workspace::git::{
    BaselineCoverage, BaselineWindow, DiffMode, GitComparison, GitScope, GitStatus, PathStatus,
    snapshot::GitSnapshot,
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
    /// Evidence metadata could not be confirmed as current for this request. This covers two
    /// distinct causes: (1) structurally unavailable evidence, where scope/cursor/comparison
    /// mismatched and no epoch relationship could even be evaluated, and (2) a `Ready` result
    /// whose status still carries unmerged conflicts, since a conflicted comparison cannot itself
    /// claim to be a clean, current view of the requested scope even though its raw evidence was
    /// otherwise trusted. Callers must treat both causes as "do not assume current" and never
    /// infer `Stale` from `Unknown`.
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
    /// Builds a bounded budget. A zero request can never select or page past even one hunk, so it
    /// is clamped to the minimum budget that still guarantees progress; `usize` has no negative
    /// values to reject separately.
    pub const fn bounded(max_hunks: usize, max_bytes: usize) -> Self {
        Self {
            max_hunks: if max_hunks == 0 { 1 } else { max_hunks },
            max_bytes: if max_bytes == 0 { 1 } else { max_bytes },
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
    /// Position in the complete per-path hunk stream for bounded expansion.
    index: usize,
    /// Exact raw Workspace path, independent of temporary Git headers.
    path: PathBuf,
    /// Complete hunk bytes, or empty for binary summaries.
    patch: Vec<u8>,
    /// Whether this entry represents binary rather than textual content.
    is_binary: bool,
}

impl DiffHunk {
    /// Returns the 0-based position of this hunk in the unbounded raw hunk stream.
    pub const fn index(&self) -> usize {
        self.index
    }

    /// Returns the exact raw Workspace evidence path; temporary Git headers never supply identity.
    pub fn path(&self) -> &PathBuf {
        &self.path
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
    /// Exact scope that owns this cursor; a reference never authorizes a different scope.
    scope: GitScope,
    /// Capture generation that must still identify the retained snapshot.
    capture_generation: u64,
    /// Bounded owner operation reference, interpreted only together with scope and generation.
    operation_reference: String,
    /// Exact comparison identities and baseline context retained from the originating request.
    comparison: GitComparison,
    /// Global index of the first omitted hunk within that exact snapshot.
    next_hunk: usize,
}

impl DiffDetailCursor {
    /// Mints a cursor only from the exact snapshot whose bounded selection omitted this hunk.
    fn new(snapshot: &GitSnapshot, next_hunk: usize) -> Self {
        Self {
            scope: snapshot.scope().clone(),
            capture_generation: snapshot.generation(),
            operation_reference: snapshot.operation_reference().to_owned(),
            comparison: snapshot.comparison().clone(),
            next_hunk,
        }
    }

    /// Returns the full required worktree/incarnation/epoch/mode scope for detail expansion.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }
    /// Returns the exact required capture generation; operation-reference reuse cannot satisfy it.
    pub const fn capture_generation(&self) -> u64 {
        self.capture_generation
    }
    /// Checks the complete cursor identity against an already retained Workspace capture.
    fn matches(&self, snapshot: &GitSnapshot) -> bool {
        self.scope == *snapshot.scope()
            && self.capture_generation == snapshot.generation()
            && self.operation_reference == snapshot.operation_reference()
            && self.comparison == *snapshot.comparison()
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
    /// Exact scope bound to the operation reference, absent when input validation failed.
    scope: Option<GitScope>,
    /// Exact capture generation bound to that same reference.
    capture_generation: Option<u64>,
    /// Baseline context reference, never a Git comparison side.
    baseline_reference: Option<String>,
    /// Explicit baseline coverage, absent when scope checks fail.
    baseline_coverage: Option<BaselineCoverage>,
    /// Whether baseline context was captured and its joint window verified.
    baseline_window: Option<BaselineWindow>,
}

impl DiffProvenance {
    /// Builds rendering metadata; absent scope/generation means no validated capture.
    /// These fields grant no authority: expansion separately validates its bound cursor.
    pub fn new(
        operation_reference: Option<String>,
        scope: Option<GitScope>,
        capture_generation: Option<u64>,
        baseline_reference: Option<String>,
        baseline_coverage: Option<BaselineCoverage>,
        baseline_window: Option<BaselineWindow>,
    ) -> Self {
        Self {
            operation_reference,
            scope,
            capture_generation,
            baseline_reference,
            baseline_coverage,
            baseline_window,
        }
    }

    /// Returns the exact scope needed to interpret this operation reference.
    pub fn scope(&self) -> Option<&GitScope> {
        self.scope.as_ref()
    }
    /// Returns the exact generation needed to interpret this operation reference.
    pub const fn capture_generation(&self) -> Option<u64> {
        self.capture_generation
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
    /// Overall availability and completeness outcome.
    state: DiffResultState,
    /// Evidence currentness relative to requested scope.
    freshness: DiffFreshness,
    /// Whether bounded selection covers all supported material.
    coverage: DiffCoverage,
    /// Exact requested HEAD/index/worktree comparison mode.
    scope_mode: DiffMode,
    /// Authority generation that fences expansion.
    authority_epoch: u64,
    /// Opaque worktree identity, including its incarnation.
    worktree_id: String,
    /// Exact comparison side fingerprints; absent for unavailable results.
    identities: DiffComparisonIdentities,
    /// Counts of the original separately classified raw status.
    status_counts: DiffStatusCounts,
    /// Unsliced directly attributed hunks retained within request budgets.
    selected_hunks: Vec<DiffHunk>,
    /// Whether retained raw output was incomplete; minted snapshots require complete streams.
    /// Always `false` for a `GitSnapshot` that reached composition: Workspace's raw evidence
    /// constructor rejects any oversized stdout/stderr with `GitError::EvidenceTooLarge` before a
    /// snapshot can exist, so process-stream truncation can never survive admission. It stays an
    /// explicit typed field rather than a derived constant so a future evidence source that can
    /// legitimately truncate does not have to change this struct's shape; hunk-level omission is
    /// reported separately and exactly via `overflow_hunks`/`overflow_bytes`.
    truncated_output: bool,
    /// Number of complete hunks omitted by request budgets.
    overflow_hunks: usize,
    /// Sum of bytes in omitted complete hunks.
    overflow_bytes: usize,
    /// Selected tracked paths, including additions/deletions and mode-only changes with no hunks.
    tracked: Vec<PathStatus>,
    /// Separately listed raw untracked paths, never treated as baseline content.
    untracked: Vec<PathStatus>,
    /// Unmerged paths retained without guessed hunks.
    conflicts: Vec<PathStatus>,
    /// Optional standalone ignored context; live snapshots do not scan ignored files.
    ignored: Vec<PathStatus>,
    /// First omitted hunk in the owning operation, if selection overflowed.
    detail_cursor: Option<DiffDetailCursor>,
    /// Bounded operation and baseline context for rendering and expansion.
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

    /// Returns exact tracked paths selected for this mode, including changes without textual hunks.
    pub fn tracked(&self) -> &[PathStatus] {
        &self.tracked
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

/// Composes bounded hunks from a complete per-path Workspace snapshot; mismatched scope yields no payload.
pub fn compose_diff(
    expected_scope: &GitScope,
    comparison: &GitComparison,
    evidence: GitSnapshot,
    budget: DiffSelectionBudget,
) -> DiffResult {
    compose_diff_at(expected_scope, comparison, evidence, None, budget)
}

/// Expands only the exact scope/generation/operation retained by a prior bounded result.
/// A reused reference from a different capture returns Unavailable with no payload or references.
pub fn expand_diff(
    expected_scope: &GitScope,
    comparison: &GitComparison,
    evidence: GitSnapshot,
    cursor: &DiffDetailCursor,
    budget: DiffSelectionBudget,
) -> DiffResult {
    compose_diff_at(expected_scope, comparison, evidence, Some(cursor), budget)
}

/// Applies common scope/cursor validation before parsing or selecting any hunk payload.
fn compose_diff_at(
    expected_scope: &GitScope,
    comparison: &GitComparison,
    evidence: GitSnapshot,
    cursor: Option<&DiffDetailCursor>,
    budget: DiffSelectionBudget,
) -> DiffResult {
    let status = evidence.status();
    let same_worktree = evidence.scope().worktree() == expected_scope.worktree();
    let stale = same_worktree
        && evidence.scope().authority_epoch() != expected_scope.authority_epoch()
        && evidence.scope().mode() == expected_scope.mode()
        && comparison.mode() == expected_scope.mode();
    let status_matches = status.scope() == Some(expected_scope);
    if cursor.is_some_and(|cursor| !cursor.matches(&evidence))
        || evidence.scope() != expected_scope
        || comparison.scope() != expected_scope
        || evidence.comparison() != comparison
        || evidence.paths().iter().any(|path| {
            path.scope() != expected_scope || path.generation() != evidence.generation()
        })
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
            tracked: Vec::new(),
            untracked: Vec::new(),
            conflicts: Vec::new(),
            ignored: Vec::new(),
            detail_cursor: None,
            provenance: DiffProvenance::new(None, None, None, None, None, None),
        };
    }

    let mut state = DiffResultState::Ready;
    let mut coverage = DiffCoverage::Complete;
    let mut detail_cursor = None;

    let truncated_output = false;
    let parsed = parse_snapshot_hunks(&evidence);
    let malformed = parsed.malformed;
    let has_binary = parsed.has_binary;
    let raw_hunks = parsed
        .hunks
        .into_iter()
        .skip(cursor.map_or(0, DiffDetailCursor::next_hunk))
        .collect();

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
        detail_cursor = cursor_offset.map(|offset| DiffDetailCursor::new(&evidence, offset));
    }

    DiffResult {
        state,
        freshness: if status.conflicts().is_empty() {
            DiffFreshness::Current
        } else {
            DiffFreshness::Unknown
        },
        coverage,
        scope_mode: expected_scope.mode(),
        authority_epoch: expected_scope.authority_epoch(),
        worktree_id: expected_scope.worktree().id().to_owned(),
        identities: DiffComparisonIdentities::new(
            comparison.left().as_bytes(),
            comparison.right().as_bytes(),
        ),
        status_counts: DiffStatusCounts {
            tracked: evidence.paths().len(),
            ..DiffStatusCounts::from_status(status)
        },
        selected_hunks,
        truncated_output,
        overflow_hunks,
        overflow_bytes,
        tracked: evidence
            .paths()
            .iter()
            .map(|path| path.status().clone())
            .collect(),
        untracked: status.untracked().to_vec(),
        conflicts: status.conflicts().to_vec(),
        ignored: status.ignored().to_vec(),
        detail_cursor,
        provenance: DiffProvenance::new(
            Some(evidence.operation_reference().to_owned()),
            Some(evidence.scope().clone()),
            Some(evidence.generation()),
            Some(comparison.baseline().reference().to_owned()),
            Some(comparison.baseline().coverage()),
            Some(comparison.baseline().window()),
        ),
    }
}

/// Selects exact hunks without splitting payload under configured boundaries.
///
/// A hunk whose own byte length exceeds `budget.max_bytes` can never fit in any single page under
/// this budget, so it is permanently skipped (counted as omitted, never selected) rather than
/// parked behind a cursor that could never resolve it; this guarantees the returned cursor, if
/// any, always strictly advances past every hunk already visited by this call. A hunk that could
/// still fit a future page (only the count/byte budget of *this* call was exhausted) pauses
/// selection instead, so the caller can resume exactly there with a fresh budget.
fn select_hunks(
    hunks: Vec<RawHunk>,
    budget: DiffSelectionBudget,
) -> (Vec<DiffHunk>, usize, usize, Option<usize>) {
    let mut selected = Vec::new();
    let mut selected_bytes = 0usize;
    let mut omitted = 0usize;
    let mut omitted_bytes = 0usize;
    let mut cursor = None;

    let mut hunks = hunks.into_iter();
    while let Some(hunk) = hunks.next() {
        let fits_count = selected.len() < budget.max_hunks;
        let fits_bytes = selected_bytes + hunk.patch.len() <= budget.max_bytes;
        if fits_count && fits_bytes {
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

        if hunk.patch.len() > budget.max_bytes {
            // Never fits under this budget regardless of page; advance past it for good.
            continue;
        }

        cursor = Some(hunk.original_index);
        omitted += hunks.len();
        omitted_bytes += hunks.map(|remaining| remaining.patch.len()).sum::<usize>();
        break;
    }

    (selected, omitted, omitted_bytes, cursor)
}

/// Parsed, owner-scoped raw diff hunk before budget selection.
struct RawHunk {
    /// Zero-based position in the raw hunk stream.
    original_index: usize,
    /// Exact raw path bound directly to the Workspace per-path evidence.
    path: PathBuf,
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

/// Parses each bounded patch with its explicit path; headers are transport details, never identities.
fn parse_snapshot_hunks(snapshot: &GitSnapshot) -> ParsedDiff {
    let mut parsed = ParsedDiff {
        hunks: Vec::new(),
        malformed: false,
        has_binary: false,
    };
    for evidence in snapshot.paths() {
        let stdout = evidence.patch();
        let path = evidence.status().path().to_path_buf();
        let mut cursor = 0;
        while cursor < stdout.len() {
            let end = next_line_end(stdout, cursor);
            let line = &stdout[cursor..end];
            if line.starts_with(b"Binary files ") || line.starts_with(b"Binary file ") {
                parsed.hunks.push(RawHunk {
                    original_index: parsed.hunks.len(),
                    path: path.clone(),
                    patch: Vec::new(),
                    binary: true,
                });
                parsed.has_binary = true;
            } else if line.starts_with(b"@@ ") {
                let start = cursor;
                let mut next = end;
                while next < stdout.len() {
                    let next_end = next_line_end(stdout, next);
                    let candidate = &stdout[next..next_end];
                    if candidate.starts_with(b"@@ ") || candidate.starts_with(b"diff --git ") {
                        break;
                    }
                    if !matches!(candidate.first(), Some(b'+' | b'-' | b' ' | b'\\')) {
                        parsed.malformed = true;
                    }
                    next = next_end;
                }
                parsed.hunks.push(RawHunk {
                    original_index: parsed.hunks.len(),
                    path: path.clone(),
                    patch: stdout[start..next].to_vec(),
                    binary: false,
                });
                cursor = next;
                continue;
            } else if !line.starts_with(b"diff --git ")
                && !line.starts_with(b"index ")
                && !line.starts_with(b"--- ")
                && !line.starts_with(b"+++ ")
                && !line.starts_with(b"old mode ")
                && !line.starts_with(b"new mode ")
                && !line.starts_with(b"new file mode ")
                && !line.starts_with(b"deleted file mode ")
            {
                parsed.malformed = true;
            }
            cursor = end;
        }
    }
    parsed
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
