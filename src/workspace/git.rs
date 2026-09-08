//! Raw Git evidence and NUL-safe status parsing owned by Workspace.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

use super::authority::{AuthorityStamp, WorktreeRef};
use crate::execution::{CommandKind, ControlledCommand, WorkspaceAuthority as ExecutionAuthority};

/// Names one v0.1 Git comparison that Workspace supplies to Changes for bounded composition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiffMode {
    /// Compares the current worktree against the selected HEAD identity.
    Head,
    /// Compares the index against the selected HEAD identity.
    Staged,
    /// Compares working bytes against the selected index identity.
    Unstaged,
}

/// Represents one exact Git identity without claiming it is a source revision or baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitIdentity(Vec<u8>);

impl GitIdentity {
    /// Validates one nonempty, bounded raw Git identity emitted by a controlled Git query.
    pub fn new(value: Vec<u8>) -> Result<Self, GitError> {
        if value.is_empty() || value.len() > 4096 || value.contains(&0) {
            return Err(GitError::InvalidIdentity);
        }
        Ok(Self(value))
    }

    /// Returns the opaque Git identity bytes without reinterpreting them as source content.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Distinguishes an exact complete baseline from a partial capture that cannot prove clean state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaselineCoverage {
    /// The capture includes every supported input required by its stated provenance.
    Complete,
    /// A filter, size ceiling, or unsupported source condition omitted part of the capture.
    Partial,
    /// Capture completion is unknown and must not be reported as empty or clean.
    Unknown,
}

/// States whether a baseline's Git/source capture window has actually been verified.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BaselineWindow {
    /// Caller supplied descriptive context only; no stored capture exists.
    NotCaptured,
    /// A bounded capture is stored, but no joint atomic Git/source window has been established.
    Unverified,
}

/// Carries session-baseline provenance only as context beside an exact Git comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BaselineContext {
    /// Stable local reference for the bounded baseline observation, not a Git mode input.
    reference: String,
    /// Explicit completeness state for that observation.
    coverage: BaselineCoverage,
    /// Scope of a durable capture, absent for descriptive caller context.
    scope: Option<GitScope>,
    /// Explicit capture-window limitation; callers cannot claim a closed window.
    window: BaselineWindow,
    /// Fingerprint of the bounded stored payload, absent for descriptive context.
    capture_digest: Option<[u8; 32]>,
}

impl BaselineContext {
    /// Creates descriptive Partial/Unknown context without claiming a stored capture.
    /// Complete coverage is rejected: only a future verified joint capture window may mint it.
    pub fn new(reference: impl Into<String>, coverage: BaselineCoverage) -> Result<Self, GitError> {
        if coverage == BaselineCoverage::Complete {
            return Err(GitError::UnverifiedBaseline);
        }
        let reference = reference.into();
        if reference.is_empty() || reference.len() > 128 {
            return Err(GitError::InvalidBaselineReference);
        }
        Ok(Self {
            reference,
            coverage,
            scope: None,
            window: BaselineWindow::NotCaptured,
            capture_digest: None,
        })
    }

    /// Mints partial context only after Workspace has committed and verified a bounded stored payload.
    pub(super) fn from_stored(
        reference: String,
        scope: GitScope,
        digest: [u8; 32],
    ) -> Result<Self, GitError> {
        let mut context = Self::new(reference, BaselineCoverage::Partial)?;
        context.scope = Some(scope);
        context.window = BaselineWindow::Unverified;
        context.capture_digest = Some(digest);
        Ok(context)
    }

    /// Returns the explicit capture-window status, never an inferred complete snapshot.
    pub const fn window(&self) -> BaselineWindow {
        self.window
    }

    /// Returns the stored payload digest; None means no capture was minted by Workspace.
    pub fn capture_digest(&self) -> Option<&[u8; 32]> {
        self.capture_digest.as_ref()
    }

    /// Requires a stored baseline to match the worktree incarnation and authority epoch being compared.
    /// Descriptive partial/unknown context has no authority and cannot claim Complete coverage.
    pub fn matches_scope(&self, scope: &GitScope) -> bool {
        self.scope
            .as_ref()
            .map_or(self.coverage != BaselineCoverage::Complete, |captured| {
                captured.worktree() == scope.worktree()
                    && captured.authority_epoch() == scope.authority_epoch()
            })
    }

    /// Returns the bounded baseline lookup reference.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns whether baseline coverage is complete, partial, or unknown.
    pub const fn coverage(&self) -> BaselineCoverage {
        self.coverage
    }
}

/// Identifies the exact left and right Git sides for one requested comparison mode.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitComparison {
    /// Exact worktree incarnation, authority epoch, and logical mode of both identities.
    scope: GitScope,
    /// Exact controlled-Git identity on the left side.
    left: GitIdentity,
    /// Exact controlled-Git identity on the right side.
    right: GitIdentity,
    /// Session-baseline completeness and provenance presented only alongside the comparison.
    baseline: BaselineContext,
}

impl GitComparison {
    /// Binds exact Git identities to their worktree incarnation, authority epoch, and mode.
    pub fn new(
        scope: GitScope,
        left: GitIdentity,
        right: GitIdentity,
        baseline: BaselineContext,
    ) -> Self {
        Self {
            scope,
            left,
            right,
            baseline,
        }
    }

    /// Returns the requested `head`, `staged`, or `unstaged` comparison mode.
    pub const fn mode(&self) -> DiffMode {
        self.scope.mode
    }

    /// Returns the worktree incarnation, epoch, and comparison mode of these identities.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }

    /// Returns the exact left-side Git identity.
    pub fn left(&self) -> &GitIdentity {
        &self.left
    }

    /// Returns the exact right-side Git identity.
    pub fn right(&self) -> &GitIdentity {
        &self.right
    }

    /// Returns baseline provenance without allowing it to replace the comparison sides.
    pub fn baseline(&self) -> &BaselineContext {
        &self.baseline
    }
}

/// Scopes one raw Git request to the exact current worktree incarnation and authority epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitScope {
    /// Current worktree identity, including incarnation.
    worktree: WorktreeRef,
    /// Current authority epoch that Changes must return with its result/expansion request.
    authority_epoch: u64,
    /// Exact logical comparison mode requested by the current actor.
    mode: DiffMode,
}

/// Selects one fixed read-only Git collection command without accepting model-provided argv or paths.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitReadQuery {
    /// Collects NUL-delimited porcelain-v2 status, including separately reported untracked paths.
    Status,
    /// Quietly verifies the current `HEAD`; its fixed missing-ref exit reports an unborn head.
    HeadIdentity,
    /// Collects the complete NUL-delimited index state without writing a tree object.
    IndexState,
    /// Collects the raw `HEAD` comparison bytes for the current worktree.
    HeadDiff,
    /// Collects the raw staged/index comparison bytes.
    StagedDiff,
    /// Collects the raw unstaged/worktree comparison bytes.
    UnstagedDiff,
}

impl GitReadQuery {
    /// Returns the fixed comparison mode assigned to this collection command.
    pub const fn mode(self) -> DiffMode {
        match self {
            Self::Status | Self::HeadIdentity | Self::HeadDiff => DiffMode::Head,
            Self::IndexState | Self::StagedDiff => DiffMode::Staged,
            Self::UnstagedDiff => DiffMode::Unstaged,
        }
    }

    /// Returns the only patch query that can supply a requested comparison mode.
    pub const fn diff_for(mode: DiffMode) -> Self {
        match mode {
            DiffMode::Head => Self::HeadDiff,
            DiffMode::Staged => Self::StagedDiff,
            DiffMode::Unstaged => Self::UnstagedDiff,
        }
    }
}

/// Couples a current Workspace scope to one immutable read-only Git argv/env construction.
#[derive(Clone, Debug)]
pub struct GitReadIntent {
    /// Scope whose worktree incarnation and epoch Execution must preserve.
    scope: GitScope,
    /// Absolute configured Git executable, never supplied by a model argument.
    program: PathBuf,
    /// Fixed read-only query selected by Workspace code.
    query: GitReadQuery,
}

impl GitReadIntent {
    /// Creates one fixed read-only Git intent for a current authority without accepting paths or flags.
    pub fn new(
        authority: &AuthorityStamp,
        program: PathBuf,
        query: GitReadQuery,
    ) -> Result<Self, GitError> {
        if !is_normal_absolute(&program) {
            return Err(GitError::InvalidGitProgram);
        }
        let mode = query.mode();
        Ok(Self {
            scope: GitScope::from_authority(authority, mode),
            program,
            query,
        })
    }

    /// Returns the worktree incarnation, epoch, and logical mode bound to this collection request.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }

    /// Returns the fixed read-only query selected by Workspace.
    pub const fn query(&self) -> GitReadQuery {
        self.query
    }

    /// Builds the Execution-owned controlled command with no caller-provided argv or environment.
    ///
    /// The argv disables repository-configured fsmonitor, pager/color formatting, external diff,
    /// and textconv where a diff can invoke them. `GIT_OPTIONAL_LOCKS=0` prevents status reads from
    /// opportunistically refreshing the index. These flags do not disable clean/process filters;
    /// content queries still require a future raw-snapshot collector for helper isolation.
    /// Execution validates process policy and spawn.
    pub fn controlled_command(&self) -> Result<ControlledCommand, GitError> {
        ControlledCommand::from_validated_peer(
            CommandKind::Git,
            self.program.clone(),
            read_args(self.query),
            self.scope.worktree.worktree_path().to_path_buf(),
            BTreeMap::from([
                (OsString::from("GIT_OPTIONAL_LOCKS"), OsString::from("0")),
                (OsString::from("GIT_PAGER"), OsString::from("cat")),
                (OsString::from("GIT_TERMINAL_PROMPT"), OsString::from("0")),
            ]),
        )
        .map_err(|_| GitError::InvalidGitProgram)
    }

    /// Converts current Workspace identity to Execution's full worktree/incarnation/root/epoch token.
    pub fn execution_authority(&self) -> Result<ExecutionAuthority, GitError> {
        ExecutionAuthority::from_workspace(
            self.scope.worktree.id(),
            self.scope.worktree.incarnation().to_string(),
            self.scope.worktree.worktree_path().to_path_buf(),
            self.scope.authority_epoch,
        )
        .map_err(|_| GitError::InvalidGitProgram)
    }
}

impl GitScope {
    /// Copies only scope data from a current Workspace authority for peer-facing raw Git evidence.
    pub fn from_authority(authority: &AuthorityStamp, mode: DiffMode) -> Self {
        Self {
            worktree: authority.worktree().clone(),
            authority_epoch: authority.epoch(),
            mode,
        }
    }

    /// Returns the worktree incarnation whose evidence is safe to compose.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the exact authority epoch required for owner-scoped expansion.
    pub const fn authority_epoch(&self) -> u64 {
        self.authority_epoch
    }

    /// Returns the mode whose left/right identities must remain exact.
    pub const fn mode(&self) -> DiffMode {
        self.mode
    }
}

/// Hard stdout capture ceiling accepted by the Workspace evidence boundary.
pub const MAX_GIT_STDOUT_BYTES: usize = 1024 * 1024;
/// Hard stderr capture ceiling accepted by the Workspace evidence boundary.
pub const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;

/// Represents bounded raw execution output without assigning it diff or source semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawGitEvidence {
    /// Stable operation reference used for one owner-scoped expansion cursor.
    operation_reference: String,
    /// Scope that fences the evidence to one worktree incarnation and authority epoch.
    scope: GitScope,
    /// Fixed query whose output these bytes represent.
    query: GitReadQuery,
    /// Raw stdout bytes captured under the configured execution limit.
    stdout: Vec<u8>,
    /// Raw stderr bytes captured under the configured execution limit.
    stderr: Vec<u8>,
    /// Controlled Git process exit code when one exists.
    exit_code: Option<i32>,
    /// Whether stdout omitted bytes beyond the configured capture ceiling.
    stdout_truncated: bool,
    /// Whether stderr omitted bytes beyond the configured capture ceiling.
    stderr_truncated: bool,
}

impl RawGitEvidence {
    /// Creates raw evidence retaining its fixed query and scope; their modes must agree.
    /// Rejects stdout over 1 MiB or stderr over 64 KiB regardless of caller truncation flags.
    /// Callers must retain truthful Execution truncation flags; this constructor cannot recover omitted bytes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        operation_reference: impl Into<String>,
        scope: GitScope,
        query: GitReadQuery,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        exit_code: Option<i32>,
        stdout_truncated: bool,
        stderr_truncated: bool,
    ) -> Result<Self, GitError> {
        let operation_reference = operation_reference.into();
        if operation_reference.is_empty() || operation_reference.len() > 128 {
            return Err(GitError::InvalidOperationReference);
        }
        if stdout.len() > MAX_GIT_STDOUT_BYTES || stderr.len() > MAX_GIT_STDERR_BYTES {
            return Err(GitError::EvidenceTooLarge);
        }
        if scope.mode() != query.mode() {
            return Err(GitError::IncompleteIdentity);
        }
        Ok(Self {
            operation_reference,
            scope,
            query,
            stdout,
            stderr,
            exit_code,
            stdout_truncated,
            stderr_truncated,
        })
    }

    /// Returns the owner-scoped operation reference for bounded hunk expansion.
    pub fn operation_reference(&self) -> &str {
        &self.operation_reference
    }

    /// Returns the authority/worktree/mode scope that invalidates stale evidence.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }

    /// Returns the fixed collection query, retained across parsing and composition.
    pub const fn query(&self) -> GitReadQuery {
        self.query
    }

    /// Returns the raw bounded stdout captured by Execution.
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns the raw bounded stderr captured by Execution.
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Returns the observed process exit code, or `None` when Execution has no exit evidence.
    pub const fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    /// Returns whether either captured stream omitted bytes.
    pub const fn is_truncated(&self) -> bool {
        self.stdout_truncated || self.stderr_truncated
    }
}

/// Classifies a porcelain-v2 path record without converting its path bytes to UTF-8.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatusKind {
    /// A tracked ordinary record supplies its two-byte index/worktree status.
    Ordinary,
    /// A rename or copy record supplies its two-byte status and original path.
    RenamedOrCopied,
    /// An unmerged record reports a merge conflict.
    Unmerged,
    /// An untracked path is listed separately from tracked status records.
    Untracked,
    /// An ignored path is reported only as raw status evidence.
    Ignored,
}

/// Preserves one status path, optional original path, and bounded raw status bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathStatus {
    /// Porcelain record category that determines whether this entry is tracked, conflicted, or untracked.
    kind: StatusKind,
    /// Raw Unix path from the first NUL-delimited porcelain record.
    path: PathBuf,
    /// Original raw Unix path supplied only by rename/copy records.
    original_path: Option<PathBuf>,
    /// Two-byte XY status for tracked records; untracked and ignored records have no XY value.
    status: Option<[u8; 2]>,
}

impl PathStatus {
    /// Returns the porcelain record category.
    pub const fn kind(&self) -> StatusKind {
        self.kind
    }

    /// Returns the raw Unix path without assuming UTF-8 or treating leading dashes specially.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the original raw path for a rename/copy, when porcelain supplied one.
    pub fn original_path(&self) -> Option<&Path> {
        self.original_path.as_deref()
    }

    /// Returns the tracked XY status bytes, when the record class carries them.
    pub const fn status(&self) -> Option<[u8; 2]> {
        self.status
    }
}

/// Splits porcelain status into tracked entries, conflicts, and separately listed untracked paths.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GitStatus {
    /// Collection scope, absent for untrusted standalone byte parsing.
    scope: Option<GitScope>,
    /// Non-conflicted tracked records.
    tracked: Vec<PathStatus>,
    /// Unmerged records whose status must be visible as conflicts.
    conflicts: Vec<PathStatus>,
    /// Untracked records that never disappear into a baseline or tracked summary.
    untracked: Vec<PathStatus>,
    /// Ignored records retained as raw evidence but excluded from the three primary groups.
    ignored: Vec<PathStatus>,
}

impl GitStatus {
    /// Parses only complete successful Status evidence and retains its worktree and epoch.
    pub fn from_evidence(evidence: &RawGitEvidence) -> Result<Self, GitError> {
        if evidence.query() != GitReadQuery::Status
            || evidence.exit_code() != Some(0)
            || evidence.is_truncated()
        {
            return Err(GitError::IncompleteIdentity);
        }
        let mut status = parse_porcelain_v2_z(evidence.stdout())?;
        status.scope = Some(evidence.scope().clone());
        Ok(status)
    }

    /// Returns collection provenance; standalone parsed bytes cannot authorize composition.
    pub fn scope(&self) -> Option<&GitScope> {
        self.scope.as_ref()
    }

    /// Returns ordinary and rename/copy tracked records.
    pub fn tracked(&self) -> &[PathStatus] {
        &self.tracked
    }

    /// Returns unmerged conflict records.
    pub fn conflicts(&self) -> &[PathStatus] {
        &self.conflicts
    }

    /// Returns untracked records separately from tracked state.
    pub fn untracked(&self) -> &[PathStatus] {
        &self.untracked
    }

    /// Returns ignored records retained only as raw status evidence.
    pub fn ignored(&self) -> &[PathStatus] {
        &self.ignored
    }
}

/// Reports malformed raw Git output or an invalid bounded Workspace input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitError {
    /// A raw Git identity is empty, contains NUL, or exceeds the bounded evidence limit.
    InvalidIdentity,
    /// A baseline lookup reference is empty or exceeds the bounded local identifier limit.
    InvalidBaselineReference,
    /// A caller attempted to claim Complete without a verified stored capture window.
    UnverifiedBaseline,
    /// An operation reference is empty or exceeds the bounded local identifier limit.
    InvalidOperationReference,
    /// A captured stream exceeds Workspace's hard byte ceiling, regardless of truncation flags.
    EvidenceTooLarge,
    /// The configured Git executable is not a lexically normal absolute Unix path.
    InvalidGitProgram,
    /// A terminal-LF discovery value omitted its terminal delimiter or had no path bytes.
    InvalidTerminalPath,
    /// A NUL-delimited porcelain record was incomplete or had an unsupported fixed header shape.
    InvalidPorcelain,
    /// Required identity evidence was truncated, failed, missing, or scoped to another worktree.
    IncompleteIdentity,
    /// `HEAD` could not be verified because the repository has no first commit yet.
    UnbornHead,
}

/// Builds an exact comparison from fixed, complete read-only Git evidence.
pub fn comparison_from_evidence(
    mode: DiffMode,
    head: &RawGitEvidence,
    index: &RawGitEvidence,
    working: &RawGitEvidence,
    baseline: BaselineContext,
) -> Result<GitComparison, GitError> {
    if !baseline.matches_scope(head.scope())
        || head.query() != GitReadQuery::HeadIdentity
        || index.query() != GitReadQuery::IndexState
        || working.query() != GitReadQuery::diff_for(mode)
        || !same_scope(head, index)
        || !same_scope(head, working)
        || head.is_truncated()
        || index.is_truncated()
        || working.is_truncated()
    {
        return Err(GitError::IncompleteIdentity);
    }
    if head.exit_code() != Some(0) {
        return Err(if head.exit_code() == Some(1) {
            GitError::UnbornHead
        } else {
            GitError::IncompleteIdentity
        });
    }
    if index.exit_code() != Some(0) || working.exit_code() != Some(0) || head.stdout().is_empty() {
        return Err(GitError::IncompleteIdentity);
    }
    let head_identity = evidence_identity(b"workspace-git-head-v1", head.stdout());
    let index_identity = evidence_identity(b"workspace-git-index-v1", index.stdout());
    let working_identity = evidence_identity(b"workspace-git-working-v1", working.stdout());
    let (left, right) = match mode {
        DiffMode::Head => (head_identity, working_identity),
        DiffMode::Staged => (head_identity, index_identity),
        DiffMode::Unstaged => (index_identity, working_identity),
    };
    let mut scope = head.scope().clone();
    scope.mode = mode;
    Ok(GitComparison::new(scope, left, right, baseline))
}

/// Returns a domain-separated bounded identity for exact complete controlled-Git bytes.
fn evidence_identity(domain: &[u8], bytes: &[u8]) -> GitIdentity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    GitIdentity(hasher.finalize().as_bytes().to_vec())
}

/// Requires all evidence to refer to the same live worktree incarnation and authority generation.
fn same_scope(left: &RawGitEvidence, right: &RawGitEvidence) -> bool {
    left.scope.worktree == right.scope.worktree
        && left.scope.authority_epoch == right.scope.authority_epoch
}

/// Parses one terminal-LF Git discovery result without splitting newline bytes inside the path.
pub fn parse_terminal_path(value: &[u8]) -> Result<PathBuf, GitError> {
    let Some(path) = value.strip_suffix(b"\n") else {
        return Err(GitError::InvalidTerminalPath);
    };
    if path.is_empty() {
        return Err(GitError::InvalidTerminalPath);
    }
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

/// Parses at most 1 MiB of terminal-NUL porcelain-v2 records, retaining raw Unix paths.
/// Returns unscoped status for standalone inspection; composition requires `GitStatus::from_evidence`.
pub fn parse_porcelain_v2_z(value: &[u8]) -> Result<GitStatus, GitError> {
    if value.len() > MAX_GIT_STDOUT_BYTES || (!value.is_empty() && !value.ends_with(&[0])) {
        return Err(GitError::InvalidPorcelain);
    }
    let mut records = value.split(|byte| *byte == 0).peekable();
    let mut status = GitStatus::default();
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        let kind = *record.first().ok_or(GitError::InvalidPorcelain)?;
        let entry = match kind {
            b'1' => parse_tracked(record, StatusKind::Ordinary, 8, None)?,
            b'2' => {
                let mut entry = parse_tracked(record, StatusKind::RenamedOrCopied, 9, None)?;
                let original = records.next().ok_or(GitError::InvalidPorcelain)?;
                if original.is_empty() {
                    return Err(GitError::InvalidPorcelain);
                }
                entry.original_path = Some(raw_path(original));
                entry
            }
            b'u' => parse_tracked(record, StatusKind::Unmerged, 10, None)?,
            b'?' => parse_simple(record, StatusKind::Untracked)?,
            b'!' => parse_simple(record, StatusKind::Ignored)?,
            _ => return Err(GitError::InvalidPorcelain),
        };
        match entry.kind {
            StatusKind::Unmerged => status.conflicts.push(entry),
            StatusKind::Untracked => status.untracked.push(entry),
            StatusKind::Ignored => status.ignored.push(entry),
            StatusKind::Ordinary | StatusKind::RenamedOrCopied => status.tracked.push(entry),
        }
    }
    Ok(status)
}

/// Extracts one simple `? <path>` or `! <path>` record without interpreting its path bytes.
fn parse_simple(record: &[u8], kind: StatusKind) -> Result<PathStatus, GitError> {
    if record.len() < 3 || record[1] != b' ' {
        return Err(GitError::InvalidPorcelain);
    }
    let path = &record[2..];
    if path.is_empty() {
        return Err(GitError::InvalidPorcelain);
    }
    Ok(PathStatus {
        kind,
        path: raw_path(path),
        original_path: None,
        status: None,
    })
}

/// Extracts a fixed porcelain header and its raw path without splitting spaces inside that path.
fn parse_tracked(
    record: &[u8],
    kind: StatusKind,
    header_spaces: usize,
    original_path: Option<PathBuf>,
) -> Result<PathStatus, GitError> {
    let path_start = nth_space(record, header_spaces).ok_or(GitError::InvalidPorcelain)? + 1;
    let status_start = record.get(2..4).ok_or(GitError::InvalidPorcelain)?;
    if record.get(1) != Some(&b' ') || record.get(4) != Some(&b' ') {
        return Err(GitError::InvalidPorcelain);
    }
    let path = record.get(path_start..).ok_or(GitError::InvalidPorcelain)?;
    if path.is_empty() {
        return Err(GitError::InvalidPorcelain);
    }
    Ok(PathStatus {
        kind,
        path: raw_path(path),
        original_path,
        status: Some([status_start[0], status_start[1]]),
    })
}

/// Finds the byte index of one fixed header delimiter without inspecting path bytes.
fn nth_space(record: &[u8], nth: usize) -> Option<usize> {
    record
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b' ').then_some(index))
        .nth(nth.saturating_sub(1))
}

/// Converts raw Unix path bytes into an `OsString` without lossy UTF-8 decoding.
fn raw_path(path: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(path.to_vec()))
}

/// Builds one fixed Git argv that cannot enable repository helpers or accept arbitrary pathspecs.
fn read_args(query: GitReadQuery) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("core.fsmonitor=false"),
        OsString::from("-c"),
        OsString::from("color.ui=false"),
        OsString::from("--no-pager"),
    ];
    match query {
        GitReadQuery::Status => args.extend([
            OsString::from("status"),
            OsString::from("--porcelain=v2"),
            OsString::from("-z"),
            OsString::from("--untracked-files=all"),
        ]),
        GitReadQuery::HeadIdentity => args.extend([
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from("HEAD"),
            OsString::from("--"),
        ]),
        GitReadQuery::IndexState => args.extend([
            OsString::from("ls-files"),
            OsString::from("--stage"),
            OsString::from("-z"),
            OsString::from("--"),
        ]),
        GitReadQuery::HeadDiff => args.extend(diff_args([OsString::from("HEAD")])),
        GitReadQuery::StagedDiff => {
            args.extend(diff_args([OsString::from("--cached")]));
        }
        GitReadQuery::UnstagedDiff => args.extend(diff_args([])),
    }
    args
}

/// Returns common raw-diff flags that prevent external diff and textconv helper execution.
fn diff_args<const N: usize>(mode: [OsString; N]) -> Vec<OsString> {
    let mut args = vec![
        OsString::from("diff"),
        OsString::from("--no-ext-diff"),
        OsString::from("--no-textconv"),
        OsString::from("--full-index"),
        OsString::from("--patch"),
        OsString::from("--no-color"),
    ];
    args.extend(mode);
    args.push(OsString::from("--"));
    args
}

/// Checks an absolute Unix executable path without normalizing or resolving its raw bytes.
fn is_normal_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path.components().all(|component| {
            matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
}

#[cfg(test)]
mod tests {
    use super::{GitReadQuery, read_args};
    use std::ffi::OsString;

    /// Keeps read-only collection options fixed rather than allowing repository helpers or caller argv.
    #[test]
    fn fixed_read_only_args_disable_fsmonitor_and_diff_helpers() {
        let status = read_args(GitReadQuery::Status);
        assert!(status.contains(&OsString::from("core.fsmonitor=false")));
        assert!(status.contains(&OsString::from("--porcelain=v2")));
        let diff = read_args(GitReadQuery::UnstagedDiff);
        assert!(diff.contains(&OsString::from("--no-ext-diff")));
        assert!(diff.contains(&OsString::from("--no-textconv")));
        assert!(diff.contains(&OsString::from("--patch")));
        assert!(diff.contains(&OsString::from("--full-index")));
        assert_eq!(diff.last(), Some(&OsString::from("--")));
        assert!(!diff.contains(&OsString::from("--raw")));
        let head = read_args(GitReadQuery::HeadIdentity);
        assert!(head.contains(&OsString::from("--verify")));
        assert!(head.contains(&OsString::from("--quiet")));
        assert_eq!(head.last(), Some(&OsString::from("--")));
    }
}
