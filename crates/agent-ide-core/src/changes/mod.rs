//! Bounded diff composition for Workspace raw Git evidence in Changes v0.1.

use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

use crate::workspace::git::{
    BaselineCoverage, BaselineWindow, DiffMode, GitComparison, GitScope, GitStatus, PathStatus,
    snapshot::GitSnapshot,
};

/// Durable request, settlement, recovery and exact-effect semantics for v0.2 single-file edits.
pub mod edit;

/// Maximum number of hunks selected by default for one bounded composition.
pub const DEFAULT_MAX_HUNKS: usize = 32;
/// Default target bytes across whole hunks; a lone oversized hunk is retained for line paging.
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
    /// Target bytes across selected whole hunks; one oversized first hunk remains pageable
    /// through Assistance's line-part fitter rather than being silently skipped.
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
    /// One-based line of this part in the original hunk, including its header.
    line_start: usize,
    /// Number of lines in the complete original hunk.
    total_lines: usize,
    /// One-based current-source line at this part, for a single-line read recovery.
    source_line: usize,
    /// Bounded recovery notice replacing one line that cannot fit in the reply envelope.
    line_notice: Option<String>,
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

    /// Returns recovery text for a delivered oversized line, or None for exact patch bytes.
    pub fn line_notice(&self) -> Option<&str> {
        self.line_notice.as_deref()
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
    /// Exact comparison identities when captured from a snapshot; absent for a plain Git diff.
    comparison: Option<GitComparison>,
    /// Global index of the first omitted hunk within that exact snapshot.
    next_hunk: usize,
    /// Already delivered bytes within next_hunk; always ends at a line boundary.
    hunk_offset: usize,
}

impl DiffDetailCursor {
    /// Mints a cursor only from the exact snapshot whose bounded selection omitted this hunk.
    fn new(snapshot: &GitSnapshot, next_hunk: usize) -> Self {
        Self {
            scope: snapshot.scope().clone(),
            capture_generation: snapshot.generation(),
            operation_reference: snapshot.operation_reference().to_owned(),
            comparison: Some(snapshot.comparison().clone()),
            next_hunk,
            hunk_offset: 0,
        }
    }

    /// Mints an owner-bound cursor for a retained plain diff payload.
    fn new_plain(scope: &GitScope, operation: &str, generation: u64, next_hunk: usize) -> Self {
        Self {
            scope: scope.clone(),
            capture_generation: generation,
            operation_reference: operation.to_owned(),
            comparison: None,
            next_hunk,
            hunk_offset: 0,
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
            && self.comparison.as_ref() == Some(snapshot.comparison())
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
    /// Directly attributed whole hunks, exact line parts, or a bounded oversized-line notice.
    selected_hunks: Vec<DiffHunk>,
    /// Whether retained raw output was incomplete; minted snapshots require complete streams.
    /// Always `false` for a `GitSnapshot` that reached composition: Workspace's raw evidence
    /// constructor rejects any oversized stdout/stderr with `GitError::EvidenceTooLarge` before a
    /// snapshot can exist, so process-stream truncation can never survive admission. It stays an
    /// explicit typed field rather than a derived constant so a future evidence source that can
    /// legitimately truncate does not have to change this struct's shape; hunk-level omission is
    /// reported separately and exactly via `overflow_hunks`/`overflow_bytes`.
    truncated_output: bool,
    /// Number of remaining hunks or hunk remainders awaiting another page.
    overflow_hunks: usize,
    /// Patch bytes in remaining hunks and line-part remainders.
    overflow_bytes: usize,
    /// Total added lines across every hunk in the whole diff, independent of pagination.
    additions: usize,
    /// Total removed lines across every hunk in the whole diff, independent of pagination.
    deletions: usize,
    /// Selected tracked paths, including additions/deletions and mode-only changes with no hunks.
    tracked: Vec<PathStatus>,
    /// Separately listed untracked paths, optionally reviewed as additions against empty.
    untracked: Vec<PathStatus>,
    /// Unmerged paths retained without guessed hunks.
    conflicts: Vec<PathStatus>,
    /// Optional standalone ignored context; live snapshots do not scan ignored files.
    ignored: Vec<PathStatus>,
    /// First omitted hunk in the owning operation, if selection overflowed.
    detail_cursor: Option<DiffDetailCursor>,
    /// Bounded operation and baseline context for rendering and expansion.
    provenance: DiffProvenance,
    /// Cursor for the first selected hunk, used only when the envelope requires line parts.
    split_cursor: Option<DiffDetailCursor>,
    /// Full hunk count before page selection, for stable part labels.
    total_hunks: usize,
    /// Untracked entries whose content cannot be reviewed under the capture limits.
    name_only: Vec<(PathBuf, String)>,
    /// Every captured changed path independent of which hunks are delivered on this page.
    inventory: Vec<PathBuf>,
    /// Fixed producer label for a plain fallback that cannot claim an exact capture window.
    degraded: Option<&'static str>,
    /// Captured untracked text files represented by additions, independent of the current page.
    untracked_text_files: usize,
}

impl DiffResult {
    /// Attaches separately captured untracked names to a plain diff result and updates its count.
    ///
    /// The caller must already have authorized and confined these paths; this method only stores
    /// the records for rendering and later-page reuse, and merges their names into the sorted,
    /// deduplicated inventory without changing the delivered hunks.
    pub fn with_untracked(mut self, untracked: Vec<PathStatus>) -> Self {
        self.status_counts.untracked = untracked.len();
        self.inventory
            .extend(untracked.iter().map(|path| path.path().to_path_buf()));
        self.inventory.sort();
        self.inventory.dedup();
        self.untracked = untracked;
        self
    }

    /// Attaches bounded name-only reasons and removes untracked patch paths from tracked counts.
    /// The names remain the independent inventory even when an entry has no delivered hunk.
    pub fn with_untracked_notes(
        mut self,
        notes: Vec<(PathBuf, String)>,
        patch_paths: usize,
    ) -> Self {
        self.status_counts.tracked = self.status_counts.tracked.saturating_sub(patch_paths);
        self.untracked_text_files = patch_paths;
        if !notes.is_empty() {
            self.coverage = DiffCoverage::Partial;
            if self.state == DiffResultState::Ready {
                self.state = DiffResultState::Incomplete;
            }
        }
        self.name_only = notes;
        self
    }

    /// Attaches a fixed capture limitation carried on every page; None keeps ordinary formatting.
    /// Only producer-owned static text is accepted, with no host or process output in the label.
    pub(crate) fn with_degraded(mut self, note: Option<&'static str>) -> Self {
        self.degraded = note;
        self
    }

    /// Returns the producer's fixed capture limitation, or None when no limitation was recorded.
    pub(crate) fn degraded(&self) -> Option<&'static str> {
        self.degraded
    }

    /// Returns the bounded captured path inventory, independently of this page's selected hunks.
    pub fn inventory(&self) -> &[PathBuf] {
        &self.inventory
    }

    /// Returns capture-limited untracked names and the reason their content was omitted.
    pub fn name_only(&self) -> &[(PathBuf, String)] {
        &self.name_only
    }

    /// Returns stable labels for a split hunk, empty for a complete whole hunk.
    /// Line numbers count the original patch header and body; the bytes following the label are exact.
    pub fn hunk_part_label(&self, hunk: &DiffHunk, continuation: bool) -> String {
        if hunk.line_notice.is_some() {
            return String::new();
        }
        let count = hunk.patch.split_inclusive(|byte| *byte == b'\n').count();
        if hunk.line_start == 1 && count == hunk.total_lines {
            return String::new();
        }
        let end = hunk.line_start + count.saturating_sub(1);
        format!(
            "hunk {} of {}, lines {}-{} of {}{}\n",
            hunk.index + 1,
            self.total_hunks,
            hunk.line_start,
            end,
            hunk.total_lines,
            if end < hunk.total_lines && continuation {
                "; continues on the next page"
            } else if end < hunk.total_lines {
                "; remaining lines require recapture"
            } else {
                "; last part"
            }
        )
    }

    /// Cuts the first selected hunk at a complete line boundary and points its cursor at the remainder.
    /// The envelope fitter uses a one-hunk result with a nonempty proper prefix.
    ///
    /// # Panics
    /// Panics when there is no selected hunk or capture cursor, or end is zero, reaches/passes
    /// the patch end, or does not follow a newline. Callers must provide a valid line boundary.
    pub fn split_first_hunk(&self, end: usize) -> Self {
        let mut result = self.clone();
        let hunk = result
            .selected_hunks
            .first_mut()
            .expect("one selected hunk");
        assert!(end > 0 && end < hunk.patch.len() && hunk.patch[end - 1] == b'\n');
        hunk.patch.truncate(end);
        let mut cursor = result.split_cursor.clone().expect("captured hunk cursor");
        cursor.hunk_offset += end;
        result.detail_cursor = Some(cursor);
        result.overflow_hunks += 1;
        result.overflow_bytes += self.selected_hunks[0].patch.len() - end;
        result.coverage = DiffCoverage::Partial;
        result
    }

    /// Counts tracked changes and captured untracked text additions for the summary.
    pub fn files(&self) -> usize {
        self.status_counts.tracked + self.untracked_text_files
    }

    /// Replaces the first undeliverable line with a bounded read recovery and advances its cursor.
    /// Returns None without mutation if there is no captured one-hunk selection or nonempty line.
    /// The notice reports raw bytes including the line terminator; later hunks remain reachable.
    pub(crate) fn skip_first_line(&self) -> Option<Self> {
        if self.selected_hunks.len() != 1 {
            return None;
        }
        let original = self.selected_hunks.first()?;
        let mut cursor = self.split_cursor.clone()?;
        let end = original
            .patch
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(original.patch.len(), |index| index + 1);
        if end == 0 {
            return None;
        }
        let mut result = self.clone();
        let hunk = result.selected_hunks.first_mut()?;
        hunk.line_notice = Some(format!(
            "hunk {} of {}, line {} of {}: one line of {} bytes is too long for one reply; read it with ide.read {{\"path\":{:?},\"lines\":\"{}-{}\"}}\n",
            hunk.index + 1,
            self.total_hunks,
            hunk.line_start,
            hunk.total_lines,
            end,
            hunk.path.to_string_lossy(),
            hunk.source_line,
            hunk.source_line
        ));
        hunk.patch.clear();
        if end < original.patch.len() {
            cursor.hunk_offset += end;
            result.overflow_hunks += 1;
            result.overflow_bytes += original.patch.len() - end;
        } else {
            cursor.next_hunk += 1;
            cursor.hunk_offset = 0;
        }
        result.detail_cursor = (cursor.next_hunk < self.total_hunks).then_some(cursor);
        result.coverage = DiffCoverage::Partial;
        Some(result)
    }

    /// Removes hunk bytes from a clone to distinguish an oversized inventory from a long line.
    /// Used only for reply-budget diagnosis; the retained result and cursor are not mutated.
    pub(crate) fn inventory_only(&self) -> Self {
        let mut result = self.clone();
        result.selected_hunks.clear();
        result.detail_cursor = None;
        result
    }

    /// Names the line that cannot fit a reply and a bounded source-read recovery.
    /// The hunk line is one-based within the exact patch, so native Git can also locate deleted lines.
    pub fn line_capacity_detail(&self) -> String {
        let Some(hunk) = self.selected_hunks.first() else {
            return "diff:reply_inventory".to_owned();
        };
        format!(
            "diff:single_line:{:?} source line {} (hunk {} line {}); read with ide.read {{\"path\":{:?},\"lines\":\"{}-{}\"}} or native git; paths can exclude this file",
            hunk.path,
            hunk.source_line,
            hunk.index + 1,
            hunk.line_start,
            hunk.path.to_string_lossy(),
            hunk.source_line,
            hunk.source_line
        )
    }

    /// Returns a payload-free refusal for retained plain-diff state that no longer validates.
    pub(crate) fn unavailable(scope: &GitScope) -> Self {
        Self {
            state: DiffResultState::Unavailable,
            freshness: DiffFreshness::Unknown,
            coverage: DiffCoverage::Unknown,
            scope_mode: scope.mode(),
            authority_epoch: scope.authority_epoch(),
            worktree_id: scope.worktree().id().to_owned(),
            identities: DiffComparisonIdentities::new(&[], &[]),
            status_counts: DiffStatusCounts {
                tracked: 0,
                conflicted: 0,
                untracked: 0,
                ignored: 0,
            },
            selected_hunks: Vec::new(),
            split_cursor: None,
            total_hunks: 0,
            name_only: Vec::new(),
            inventory: Vec::new(),
            degraded: None,
            untracked_text_files: 0,
            truncated_output: false,
            overflow_hunks: 0,
            overflow_bytes: 0,
            additions: 0,
            deletions: 0,
            tracked: Vec::new(),
            untracked: Vec::new(),
            conflicts: Vec::new(),
            ignored: Vec::new(),
            detail_cursor: None,
            provenance: DiffProvenance::new(None, None, None, None, None, None),
        }
    }

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

    /// Returns total added lines across the whole diff, independent of pagination.
    pub const fn additions(&self) -> usize {
        self.additions
    }

    /// Returns total removed lines across the whole diff, independent of pagination.
    pub const fn deletions(&self) -> usize {
        self.deletions
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

/// Composes a degraded, single-pass `DiffResult` directly from plain `git diff` stdout, used only
/// when the exact two-pass snapshot capture proved unstable. No comparison identities and no
/// built-in untracked or conflict inventory, so freshness remains Unknown. Callers attach
/// separately captured untracked additions and use compose_plain_diff_page with an operation
/// and generation to retain continuation, including line parts and oversized-line notices.
///
/// `stdout` must come from `SnapshotIntent::plain_diff` (no renames, `a/`/`b/` prefixes). Returns
/// `None` when any of it cannot be attributed exactly — a `diff --git` header of another shape, a
/// hunk before any header, or a hunk line with an unexpected first byte — so file and line counts
/// are never silently understated; the caller then refuses instead of rendering. `confine`
/// receives every named path (selected or not) once, before any hunk is selected, and must return
/// `false` for any path the exact capture would refuse (Workspace's
/// `snapshot::confine_plain_diff_paths`); `false` also yields `None`, so no page ever answers
/// for such a path. A `Some` result is `Ready`, with `Partial` coverage only when `budget` left
/// hunks unselected.
pub fn compose_plain_diff(
    expected_scope: &GitScope,
    stdout: &[u8],
    budget: DiffSelectionBudget,
    confine: impl FnOnce(&[PathBuf]) -> bool,
) -> Option<DiffResult> {
    compose_plain_diff_page(expected_scope, stdout, None, "", 0, budget, confine)
}

/// Returns paths from a well-formed bounded plain patch, preserving raw path bytes and header order.
/// Returns `None` if the patch has a malformed header or hunk line.
pub fn plain_diff_paths(stdout: &[u8]) -> Option<Vec<PathBuf>> {
    let parsed = parse_plain_diff(stdout);
    (!parsed.malformed).then_some(parsed.paths)
}

/// Selects one bounded page from retained plain Git output using the snapshot hunk cursor.
///
/// `cursor` must belong to this exact scope, operation, generation, and plain-output capture;
/// `None` selects page one. `confine` checks every named path before selection. `None` means the
/// patch is malformed, a path is refused, or the supplied cursor is stale. A successful result
/// carries the next cursor only while complete hunks remain under the original byte ceiling.
/// `operation` and `generation` are required for resumable task pages; empty/zero values are used
/// only by `compose_plain_diff`, which deliberately does not mint a continuation.
pub fn compose_plain_diff_page(
    expected_scope: &GitScope,
    stdout: &[u8],
    cursor: Option<&DiffDetailCursor>,
    operation: &str,
    generation: u64,
    budget: DiffSelectionBudget,
    confine: impl FnOnce(&[PathBuf]) -> bool,
) -> Option<DiffResult> {
    let parsed = parse_plain_diff(stdout);
    if parsed.malformed
        || !confine(&parsed.paths)
        || cursor.is_some_and(|cursor| {
            cursor.scope != *expected_scope
                || cursor.capture_generation != generation
                || cursor.operation_reference != operation
                || cursor.comparison.is_some()
        })
    {
        return None;
    }
    let total_hunks = parsed.hunks.len();
    let start = cursor.map_or(0, DiffDetailCursor::next_hunk);
    let (selected_hunks, overflow_hunks, overflow_bytes, next_hunk) = select_hunks(
        parsed.hunks.into_iter().skip(start).collect(),
        budget,
        cursor.map_or(0, |cursor| cursor.hunk_offset),
    );
    let coverage = if overflow_hunks > 0 {
        DiffCoverage::Partial
    } else {
        DiffCoverage::Complete
    };
    Some(DiffResult {
        state: DiffResultState::Ready,
        freshness: DiffFreshness::Unknown,
        coverage,
        scope_mode: expected_scope.mode(),
        authority_epoch: expected_scope.authority_epoch(),
        worktree_id: expected_scope.worktree().id().to_owned(),
        identities: DiffComparisonIdentities::new(&[], &[]),
        status_counts: DiffStatusCounts {
            tracked: parsed.paths.len(),
            conflicted: 0,
            untracked: 0,
            ignored: 0,
        },
        selected_hunks: selected_hunks.clone(),
        truncated_output: false,
        overflow_hunks,
        overflow_bytes,
        additions: parsed.additions,
        deletions: parsed.deletions,
        tracked: Vec::new(),
        untracked: Vec::new(),
        conflicts: Vec::new(),
        ignored: Vec::new(),
        split_cursor: selected_hunks.first().map(|hunk| {
            let mut split =
                DiffDetailCursor::new_plain(expected_scope, operation, generation, hunk.index);
            split.hunk_offset = cursor.map_or(0, |cursor| cursor.hunk_offset);
            split
        }),
        total_hunks,
        name_only: Vec::new(),
        inventory: parsed.paths,
        degraded: None,
        untracked_text_files: 0,
        detail_cursor: next_hunk
            .filter(|_| !operation.is_empty() && generation > 0)
            .map(|next| DiffDetailCursor::new_plain(expected_scope, operation, generation, next)),
        provenance: if operation.is_empty() || generation == 0 {
            DiffProvenance::new(None, None, None, None, None, None)
        } else {
            DiffProvenance::new(
                Some(operation.to_owned()),
                Some(expected_scope.clone()),
                Some(generation),
                None,
                None,
                None,
            )
        },
    })
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
        || evidence
            .paths()
            .iter()
            .chain(evidence.untracked_paths())
            .any(|path| {
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
            split_cursor: None,
            total_hunks: 0,
            name_only: Vec::new(),
            inventory: Vec::new(),
            degraded: None,
            untracked_text_files: 0,
            truncated_output: false,
            overflow_hunks: 0,
            overflow_bytes: 0,
            additions: 0,
            deletions: 0,
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
    let has_binary = parsed.has_binary
        || evidence
            .untracked_paths()
            .iter()
            .any(|path| path.name_only().is_some());
    let total_hunks = parsed.hunks.len();
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

    let (selected_hunks, overflow_hunks, overflow_bytes, cursor_offset) = select_hunks(
        raw_hunks,
        budget,
        cursor.map_or(0, |cursor| cursor.hunk_offset),
    );
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
        split_cursor: selected_hunks.first().map(|hunk| {
            let mut split = DiffDetailCursor::new(&evidence, hunk.index);
            split.hunk_offset = cursor.map_or(0, |cursor| cursor.hunk_offset);
            split
        }),
        total_hunks,
        inventory: evidence
            .paths()
            .iter()
            .map(|path| path.status().path().to_path_buf())
            .chain(
                status
                    .untracked()
                    .iter()
                    .map(|path| path.path().to_path_buf()),
            )
            .chain(
                status
                    .conflicts()
                    .iter()
                    .map(|path| path.path().to_path_buf()),
            )
            .collect(),
        degraded: None,
        untracked_text_files: evidence
            .untracked_paths()
            .iter()
            .filter(|path| !path.patch().is_empty())
            .count(),
        name_only: evidence
            .untracked_paths()
            .iter()
            .filter_map(|path| {
                path.name_only()
                    .map(|note| (path.status().path().to_path_buf(), note.to_owned()))
            })
            .collect(),
        selected_hunks,
        truncated_output,
        overflow_hunks,
        overflow_bytes,
        additions: parsed.additions,
        deletions: parsed.deletions,
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

/// Selects whole hunks under count/byte budgets, resuming the first at a proven line offset.
/// A lone oversized hunk remains selected for Assistance's bounded line-part fitter; no hunk is
/// permanently skipped. Returns the first undelivered hunk index, with exact omitted counts.
fn select_hunks(
    hunks: Vec<RawHunk>,
    budget: DiffSelectionBudget,
    offset: usize,
) -> (Vec<DiffHunk>, usize, usize, Option<usize>) {
    let mut selected = Vec::new();
    let mut selected_bytes = 0usize;
    let mut omitted = 0usize;
    let mut omitted_bytes = 0usize;
    let mut cursor = None;
    let mut hunks = hunks.into_iter();
    let mut offset = offset;
    while let Some(mut hunk) = hunks.next() {
        let total_lines = hunk.patch.split_inclusive(|byte| *byte == b'\n').count();
        let start_line = std::str::from_utf8(
            hunk.patch
                .split(|byte| *byte == b'\n')
                .next()
                .unwrap_or(&[]),
        )
        .ok()
        .and_then(|header| header.split_whitespace().nth(2))
        .and_then(|range| range.strip_prefix('+'))
        .and_then(|range| range.split(',').next())
        .and_then(|line| line.parse::<usize>().ok())
        .unwrap_or(1)
        .max(1);
        let source_line = start_line
            + hunk.patch[..offset.min(hunk.patch.len())]
                .split_inclusive(|byte| *byte == b'\n')
                .filter(|line| matches!(line.first(), Some(b'+' | b' ')))
                .count();
        let line_start = 1 + hunk.patch[..offset.min(hunk.patch.len())]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count();
        hunk.patch.drain(..offset.min(hunk.patch.len()));
        offset = 0;
        let fits_count = selected.len() < budget.max_hunks;
        let fits_bytes = selected_bytes + hunk.patch.len() <= budget.max_bytes;
        if fits_count && (fits_bytes || selected.is_empty()) {
            selected_bytes += hunk.patch.len();
            selected.push(DiffHunk {
                index: hunk.original_index,
                path: hunk.path,
                patch: hunk.patch,
                is_binary: hunk.binary,
                line_start,
                total_lines,
                source_line,
                line_notice: None,
            });
            continue;
        }
        omitted += 1;
        omitted_bytes += hunk.patch.len();
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
    /// Total added lines across every parsed hunk, independent of later budget selection.
    additions: usize,
    /// Total removed lines across every parsed hunk, independent of later budget selection.
    deletions: usize,
}

/// Counts `+`/`-` content lines in one hunk body (every line after its `@@ … @@` header).
fn count_hunk_delta(body: &[u8]) -> (usize, usize) {
    let mut additions = 0;
    let mut deletions = 0;
    let mut cursor = 0;
    while cursor < body.len() {
        let end = next_line_end(body, cursor);
        match body[cursor..end].first() {
            Some(b'+') => additions += 1,
            Some(b'-') => deletions += 1,
            _ => {}
        }
        cursor = end;
    }
    (additions, deletions)
}

/// Parses each bounded patch with its explicit path; headers are transport details, never identities.
fn parse_snapshot_hunks(snapshot: &GitSnapshot) -> ParsedDiff {
    let mut parsed = ParsedDiff {
        hunks: Vec::new(),
        malformed: false,
        has_binary: false,
        additions: 0,
        deletions: 0,
    };
    for evidence in snapshot.paths().iter().chain(snapshot.untracked_paths()) {
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
                let (additions, deletions) = count_hunk_delta(&stdout[end..next]);
                parsed.additions += additions;
                parsed.deletions += deletions;
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

/// Parsed raw multi-file plain `git diff` stdout: unlike [`parse_snapshot_hunks`], no per-path
/// Workspace evidence exists yet, so the path for every hunk comes from its own `diff --git`
/// header line.
struct ParsedPlainDiff {
    /// All full hunks successfully parsed from stdout.
    hunks: Vec<RawHunk>,
    /// Every file named by a `diff --git` header, in output order, independent of hunk selection;
    /// its length is the reported file count and each entry is subject to the caller's
    /// confinement check.
    paths: Vec<PathBuf>,
    /// Total added lines across every hunk.
    additions: usize,
    /// Total removed lines across every hunk.
    deletions: usize,
    /// Whether any output could not be attributed exactly: an unparsed `diff --git` header, a
    /// hunk before any header, or a hunk body line with an unexpected first byte.
    malformed: bool,
}

/// Splits raw single-pass `git diff` stdout into hunks attributed by their own `diff --git`
/// header, the only per-file identity this degraded capture has. Extended header lines (`index`,
/// modes, `---`/`+++`) are skipped; a header [`parse_diff_git_header`] cannot decode flags the
/// output malformed and detaches the hunks after it, so no hunk is ever credited to the previous
/// file and the caller can refuse instead of understating counts.
fn parse_plain_diff(stdout: &[u8]) -> ParsedPlainDiff {
    let mut parsed = ParsedPlainDiff {
        hunks: Vec::new(),
        paths: Vec::new(),
        additions: 0,
        deletions: 0,
        malformed: false,
    };
    let mut current: Option<PathBuf> = None;
    let mut cursor = 0;
    while cursor < stdout.len() {
        let end = next_line_end(stdout, cursor);
        let line = &stdout[cursor..end];
        if line.starts_with(b"diff --git ") {
            current = parse_diff_git_header(line);
            match &current {
                Some(path) => parsed.paths.push(path.clone()),
                None => parsed.malformed = true,
            }
            cursor = end;
        } else if line.starts_with(b"@@ ") {
            let Some(path) = current.clone() else {
                parsed.malformed = true;
                cursor = end;
                continue;
            };
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
            let (additions, deletions) = count_hunk_delta(&stdout[end..next]);
            parsed.additions += additions;
            parsed.deletions += deletions;
            parsed.hunks.push(RawHunk {
                original_index: parsed.hunks.len(),
                path,
                patch: stdout[start..next].to_vec(),
                binary: false,
            });
            cursor = next;
        } else if line.starts_with(b"Binary files ") || line.starts_with(b"Binary file ") {
            if let Some(path) = current.clone() {
                parsed.hunks.push(RawHunk {
                    original_index: parsed.hunks.len(),
                    path,
                    patch: Vec::new(),
                    binary: true,
                });
            }
            cursor = end;
        } else {
            cursor = end;
        }
    }
    parsed
}

/// Extracts the raw path from one `diff --git a/<path> b/<path>` header line (with or without its
/// terminating LF). The fallback runs with `--no-renames` and explicit `a/`/`b/` prefixes, so both
/// sides always name the same path: a quoted header is decoded from Git's C-quoted form, and an
/// unquoted one is split exactly at its midpoint, so a name that itself contains ` b/` is still
/// attributed correctly. `None` for any other shape, including two sides naming different paths.
fn parse_diff_git_header(line: &[u8]) -> Option<PathBuf> {
    let rest = line.strip_prefix(b"diff --git ")?;
    let rest = rest.strip_suffix(b"\n").unwrap_or(rest);
    let (old, new) = if rest.first() == Some(&b'"') {
        let (old, tail) = unquote_c_path(rest)?;
        let (new, tail) = unquote_c_path(tail.strip_prefix(b" ")?)?;
        if !tail.is_empty() {
            return None;
        }
        (old, new)
    } else {
        let half = rest.len().checked_sub(1)? / 2;
        if rest.len() % 2 == 0 || rest[half] != b' ' {
            return None;
        }
        (rest[..half].to_vec(), rest[half + 1..].to_vec())
    };
    let old = old.strip_prefix(b"a/")?;
    let path = new.strip_prefix(b"b/")?;
    (old == path && !path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(path)))
}

/// Decodes one leading Git C-quoted name — `"…"`, as Git writes a path holding a control
/// character, `"`, `\` or (under the default `core.quotePath`) a non-ASCII byte — with the
/// `\a \b \t \n \v \f \r \" \\` and three-digit octal escapes Git emits. Returns the raw name
/// bytes and the input after the closing quote; `None` when `bytes` does not start with one
/// complete quoted name or carries an escape Git never writes.
fn unquote_c_path(bytes: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut rest = bytes.strip_prefix(b"\"")?;
    let mut name = Vec::new();
    loop {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        if byte == b'"' {
            return Some((name, rest));
        }
        if byte != b'\\' {
            name.push(byte);
            continue;
        }
        let (&escape, tail) = rest.split_first()?;
        rest = tail;
        name.push(match escape {
            b'a' => 0x07,
            b'b' => 0x08,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 0x0b,
            b'f' => 0x0c,
            b'r' => b'\r',
            b'"' | b'\\' => escape,
            b'0'..=b'3' => {
                let (digits, tail) = rest.split_at_checked(2)?;
                if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                    return None;
                }
                rest = tail;
                ((escape - b'0') << 6) | ((digits[0] - b'0') << 3) | (digits[1] - b'0')
            }
            _ => return None,
        });
    }
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
