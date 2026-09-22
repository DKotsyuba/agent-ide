//! Filter-free, bounded Git comparisons assembled from immutable blobs and exact Workspace reads.

use super::{
    AuthorityStamp, BaselineContext, DiffMode, GitComparison, GitError, GitObjectId, GitReadIntent,
    GitReadQuery, GitScope, GitStatus, PathStatus, evidence_identity, safe_git_environment,
};
use crate::{
    execution::{
        CapturedProcessEvidence, CommandKind, ControlledCommand, ProcessIdentity,
        WorkspaceAuthority,
    },
    workspace::store::CurrentObservation,
    workspace::{
        authority::WorktreeRef,
        observation::{
            ObservationError, SourceBytes, SourceCoverage, SourceObservation, SourceRead,
            SourceReadLimits, read_authorized_source,
        },
    },
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Maximum changed paths processed in one capture, including conflicts and untracked entries.
pub const MAX_SNAPSHOT_PATHS: usize = 256;
/// Maximum aggregate raw pathname bytes in a capture, counting rename sources.
pub const MAX_SNAPSHOT_PATH_BYTES: usize = 64 * 1024;
/// Maximum bytes for one immutable blob or exact worktree source read.
pub const MAX_SNAPSHOT_BLOB_BYTES: usize = 1024 * 1024;
/// Maximum aggregate bytes of distinct blobs and worktree sources per attempt.
pub const MAX_SNAPSHOT_TOTAL_BYTES: usize = 8 * 1024 * 1024;
/// Maximum aggregate retained patch bytes across every path in one attempt.
pub const MAX_SNAPSHOT_PATCH_BYTES: usize = 1024 * 1024;
/// Target scratch-argv bytes of one batched `hash-object` command. Every managed sandbox replay
/// pays a fixed multi-second startup per child, so batches pack toward this goal, far below the
/// execution-policy ceiling regardless of the host temp-root depth; more entries simply mean one
/// more bounded child. This is the packing goal only — the hard ceiling is
/// [`crate::execution::MAX_PRODUCT_ARGV_BYTES`].
const MAX_HASH_BATCH_ARGV_BYTES: usize = 4096;
/// Fixed non-path arguments every batched `hash-object` command starts with; the argv budget
/// counts these exact strings plus one canonical scratch path per entry.
const HASH_BATCH_FLAGS: [&str; 7] = [
    "--no-pager",
    "--no-lazy-fetch",
    "-c",
    "core.fsmonitor=false",
    "hash-object",
    "--no-filters",
    "--",
];
/// Scratch-directory name stem shared by creation and argv budgeting so neither drifts.
const SNAPSHOT_SCRATCH_STEM: &str = "agent-ide-snapshot-";
/// Process-local suffix that prevents accidental reuse of a scratch directory.
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);

/// Owns private files until all corresponding intents have been reaped or dropped.
#[derive(Debug)]
struct SnapshotDirectory {
    /// ASCII absolute directory created exclusively with mode 0700 outside the worktree.
    path: PathBuf,
    /// Shared one-command lifecycle; unknown/cancelled owners quarantine instead of deleting files.
    lifecycle: Mutex<ScratchLifecycle>,
}

/// Private command ownership and exact-child correlation; no process or settlement proof is cloned.
#[derive(Debug)]
enum ScratchLifecycle {
    /// No controlled command has escaped; dropping this directory is safe.
    Unused,
    /// Command exported once, so a process may have started even if no binding was returned.
    Issued,
    /// Exact transferred launch token; only matching actual-wait evidence permits deletion.
    Bound {
        identity: ProcessIdentity,
        reaped: bool,
    },
    /// An invalid second binding made physical ownership uncertain; retain files for recovery.
    Quarantined,
}

impl SnapshotDirectory {
    /// Creates a unique exclusive scratch directory; rejects repositories containing the temp root.
    fn new(scope: &GitScope) -> Result<Arc<Self>, GitError> {
        let temp = fs::canonicalize(std::env::temp_dir()).map_err(|_| GitError::SnapshotIo)?;
        let root =
            fs::canonicalize(scope.worktree().worktree_path()).map_err(|_| GitError::SnapshotIo)?;
        if temp.starts_with(&root) {
            return Err(GitError::SnapshotIo);
        }
        for _ in 0..16 {
            let path = temp.join(format!(
                "{SNAPSHOT_SCRATCH_STEM}{}-{}",
                std::process::id(),
                NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
            ));
            match DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    return Ok(Arc::new(Self {
                        path,
                        lifecycle: Mutex::new(ScratchLifecycle::Unused),
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(GitError::SnapshotIo),
            }
        }
        Err(GitError::SnapshotIo)
    }

    /// Writes exactly one bounded side to an exclusive 0600 file; no repository paths enter names.
    fn write(&self, name: &str, bytes: &[u8]) -> Result<(), GitError> {
        if bytes.len() > MAX_SNAPSHOT_BLOB_BYTES {
            return Err(GitError::EvidenceTooLarge);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.path.join(name))
            .map_err(|_| GitError::SnapshotIo)?;
        file.write_all(bytes).map_err(|_| GitError::SnapshotIo)
    }
}

impl Drop for SnapshotDirectory {
    /// Removes unused or actually reaped scratch only; uncertain drops quarantine the private tree.
    fn drop(&mut self) {
        if self.lifecycle.get_mut().is_ok_and(|state| {
            matches!(
                state,
                ScratchLifecycle::Unused | ScratchLifecycle::Bound { reaped: true, .. }
            )
        }) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Fixed Git operation whose arguments cannot contain model-supplied flags or repository pathspecs.
#[derive(Clone, Debug)]
pub struct SnapshotIntent {
    /// Complete original worktree scope retained even when Git operates in private scratch space.
    scope: GitScope,
    /// Fixed peer-built command consumed by Execution's existing policy and process ownership APIs.
    command: ControlledCommand,
    /// Keeps snapshot files alive through owned runner completion and error handling.
    directory: Option<Arc<SnapshotDirectory>>,
    /// Whether exit one is the successful no-index differences result.
    differences_allowed: bool,
    /// Closed stage name used only for the bounded terminal diff failure detail.
    label: &'static str,
}

impl SnapshotIntent {
    /// Exports a private command once and marks its files potentially in use before returning argv.
    /// Repeated private exports fail; dropping an issued command without matching reap quarantines it.
    pub fn command(&self) -> Result<ControlledCommand, GitError> {
        if let Some(directory) = &self.directory {
            let mut state = directory
                .lifecycle
                .lock()
                .map_err(|_| GitError::IncompleteIdentity)?;
            if !matches!(*state, ScratchLifecycle::Unused) {
                return Err(GitError::IncompleteIdentity);
            }
            *state = ScratchLifecycle::Issued;
        }
        Ok(self.command.clone())
    }

    /// Consumes the exact child's one-time launch identity after a successful private-command spawn.
    /// Duplicate/unexpected binding quarantines files rather than accepting a potentially foreign reap.
    pub fn bind_process(&self, identity: ProcessIdentity) -> Result<(), GitError> {
        let directory = self
            .directory
            .as_ref()
            .ok_or(GitError::IncompleteIdentity)?;
        let mut state = directory
            .lifecycle
            .lock()
            .map_err(|_| GitError::IncompleteIdentity)?;
        if !matches!(*state, ScratchLifecycle::Issued) {
            *state = ScratchLifecycle::Quarantined;
            return Err(GitError::IncompleteIdentity);
        }
        *state = ScratchLifecycle::Bound {
            identity,
            reaped: false,
        };
        Ok(())
    }

    /// Allows cleanup only from actual Execution wait evidence for the exact bound child.
    /// This consumes no admission/settlement capability and is idempotent for matching evidence.
    /// A cancellation reaper must retain this same intent and call this method before dropping it.
    pub fn acknowledge_reap(&self, evidence: &CapturedProcessEvidence) -> Result<(), GitError> {
        let reaped_identity = evidence
            .reap_identity()
            .ok_or(GitError::IncompleteIdentity)?;
        if let Some(directory) = &self.directory {
            let mut state = directory
                .lifecycle
                .lock()
                .map_err(|_| GitError::IncompleteIdentity)?;
            let ScratchLifecycle::Bound { identity, reaped } = &mut *state else {
                return Err(GitError::IncompleteIdentity);
            };
            if !reaped_identity.matches(identity) {
                return Err(GitError::IncompleteIdentity);
            }
            *reaped = true;
        }
        Ok(())
    }

    /// Returns the exact original scope for admission, currentness checks, and output correlation.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }
    /// Returns scratch location only for lifecycle inspection; repository source paths never live here.
    pub fn snapshot_directory(&self) -> Option<&Path> {
        self.directory.as_ref().map(|dir| dir.path.as_path())
    }
    /// Converts this operation's original scope to the existing Execution authority token.
    pub fn execution_authority(&self) -> Result<WorkspaceAuthority, GitError> {
        WorkspaceAuthority::from_workspace(
            self.scope.worktree().id(),
            self.scope.worktree().incarnation().to_string(),
            self.scope.worktree().worktree_path().to_path_buf(),
            self.scope.authority_epoch(),
        )
        .map_err(|_| GitError::IncompleteIdentity)
    }
    /// Creates a metadata command using only the existing safe fixed-query constructor.
    fn metadata(scope: &GitScope, program: &Path, query: GitReadQuery) -> Result<Self, GitError> {
        let query_scope = GitScope::from_inherited(
            scope.worktree().clone(),
            scope.authority_epoch(),
            query.mode(),
        )?;
        let intent = GitReadIntent::from_scope(query_scope, program.to_path_buf(), query)?;
        let label = match query {
            GitReadQuery::HeadIdentity => "rev-parse",
            GitReadQuery::IndexState => "ls-files-stage",
            GitReadQuery::HeadTree => "ls-tree",
            GitReadQuery::UntrackedPaths => "ls-files-others",
            GitReadQuery::Status
            | GitReadQuery::HeadDiff
            | GitReadQuery::StagedDiff
            | GitReadQuery::UnstagedDiff => "git-metadata",
        };
        Ok(Self {
            scope: intent.scope().clone(),
            command: intent.controlled_command()?,
            directory: None,
            differences_allowed: false,
            label,
        })
    }
    /// Builds a no-filter immutable-object read; full OIDs prevent revision or option injection.
    pub fn blob(scope: GitScope, program: &Path, oid: &GitObjectId) -> Result<Self, GitError> {
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            program.to_path_buf(),
            vec![
                "--no-pager".into(),
                "--no-lazy-fetch".into(),
                "cat-file".into(),
                "blob".into(),
                oid.as_str().into(),
            ],
            scope.worktree().worktree_path().to_path_buf(),
            safe_git_environment(),
        )
        .map_err(|_| GitError::InvalidGitProgram)?;
        Ok(Self {
            scope,
            command,
            directory: None,
            differences_allowed: false,
            label: "cat-file",
        })
    }
    /// Returns whether this intent is a no-index comparison rather than metadata/blob verification.
    pub const fn is_comparison(&self) -> bool {
        self.differences_allowed
    }

    /// Returns the closed stage name of this operation for bounded failure detail.
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// Hashes exact private snapshot files in one bounded command using the repository's
    /// SHA-1/SHA-256 format and no filters. No object is written; output order matches `names`
    /// order. One child replaces one hash command per blob, which dominates wall time when every
    /// spawn replays a managed sandbox at a fixed multi-second startup cost.
    fn hash_files(
        scope: &GitScope,
        program: &Path,
        directory: Arc<SnapshotDirectory>,
        names: &[String],
    ) -> Result<Self, GitError> {
        let mut arguments: Vec<OsString> = HASH_BATCH_FLAGS.iter().map(Into::into).collect();
        arguments.extend(
            names
                .iter()
                .map(|name| directory.path.join(name).into_os_string()),
        );
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            program.to_path_buf(),
            arguments,
            scope.worktree().worktree_path().to_path_buf(),
            safe_git_environment(),
        )
        .map_err(|_| GitError::InvalidGitProgram)?;
        Ok(Self {
            scope: scope.clone(),
            command,
            directory: Some(directory),
            differences_allowed: false,
            label: "hash-object",
        })
    }

    /// Copies two exact sides into private files and compares them outside repository configuration.
    /// Modes stay in path evidence, so private files always remain 0600 even for executable sources.
    pub fn compare(
        scope: GitScope,
        program: &Path,
        left: &[u8],
        right: &[u8],
    ) -> Result<Self, GitError> {
        let directory = SnapshotDirectory::new(&scope)?;
        directory.write("left", left)?;
        directory.write("right", right)?;
        let mut environment = safe_git_environment();
        environment.insert(
            "GIT_CEILING_DIRECTORIES".into(),
            directory
                .path
                .parent()
                .ok_or(GitError::SnapshotIo)?
                .as_os_str()
                .to_owned(),
        );
        let command = ControlledCommand::from_validated_peer(
            CommandKind::Git,
            program.to_path_buf(),
            vec![
                "-C".into(),
                directory.path.as_os_str().to_owned(),
                "--no-pager".into(),
                "--no-lazy-fetch".into(),
                "-c".into(),
                "core.fsmonitor=false".into(),
                "-c".into(),
                "core.attributesFile=/dev/null".into(),
                "diff".into(),
                "--no-index".into(),
                "--no-ext-diff".into(),
                "--no-textconv".into(),
                "--no-color".into(),
                "--full-index".into(),
                "--patch".into(),
                "--".into(),
                "left".into(),
                "right".into(),
            ],
            scope.worktree().worktree_path().to_path_buf(),
            environment,
        )
        .map_err(|_| GitError::InvalidGitProgram)?;
        Ok(Self {
            scope,
            command,
            directory: Some(directory),
            differences_allowed: true,
            label: "diff-no-index",
        })
    }
    /// Accepts only reaped, fully drained bounded output with the command's exact success exit set.
    pub fn accept(&self, result: CapturedProcessEvidence) -> Result<Vec<u8>, GitError> {
        self.acknowledge_reap(&result)?;
        if result.status().code() != Some(0)
            && result
                .stderr()
                .bytes
                .windows(b"no-lazy-fetch".len())
                .any(|bytes| bytes == b"no-lazy-fetch")
            && result
                .stderr()
                .bytes
                .windows(b"unknown option".len())
                .any(|bytes| bytes == b"unknown option")
        {
            return Err(GitError::UnsupportedSnapshotGit);
        }
        if result.cancellation().is_some()
            || result.stdout().truncated
            || result.stderr().truncated
            || !result.stdout().complete
            || !result.stderr().complete
            || result.stdout().bytes.len() > MAX_SNAPSHOT_BLOB_BYTES
            || result.stderr().bytes.len() > super::MAX_GIT_STDERR_BYTES
            || !matches!(result.status().code(), Some(0))
                && !(self.differences_allowed && result.status().code() == Some(1))
        {
            return Err(GitError::IncompleteIdentity);
        }
        Ok(result.stdout().bytes.clone())
    }
}

/// Parses exactly one terminal-LF object name per line, in command argument order.
fn parse_batch_hashes(output: &[u8], count: usize) -> Result<Vec<GitObjectId>, GitError> {
    let lines: Vec<&[u8]> = output.split(|byte| *byte == b'\n').collect();
    if lines.last() != Some(&&b""[..]) || lines.len() != count + 1 {
        return Err(GitError::InvalidIdentity);
    }
    lines[..count]
        .iter()
        .map(|line| GitObjectId::parse(line)?.ok_or(GitError::InvalidIdentity))
        .collect()
}

/// Packs `count` ordered entries into consecutive `[start, end)` batches whose estimated argv
/// bytes stay below [`MAX_HASH_BATCH_ARGV_BYTES`], whatever the host temp-root depth. A single
/// entry too large even alone for `limit` fails closed with [`GitError::EvidenceTooLarge`]
/// before any scratch file is written; smaller boundaries never reorder output.
fn hash_batch_ranges(
    count: usize,
    prefix_cost: usize,
    name_cost: impl Fn(usize) -> usize,
    limit: usize,
) -> Result<Vec<(usize, usize)>, GitError> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut used = prefix_cost;
    for position in 0..count {
        let cost = prefix_cost + name_cost(position) + 1;
        if cost > limit {
            return Err(GitError::EvidenceTooLarge);
        }
        if position > start && used + cost > MAX_HASH_BATCH_ARGV_BYTES {
            ranges.push((start, position));
            start = position;
            used = prefix_cost;
        }
        used += cost;
    }
    ranges.push((start, count));
    Ok(ranges)
}

/// Returns the fixed argv bytes every batched hash command pays beyond its per-entry names: the
/// fixed flags plus one canonical scratch-directory path, each counted with a separating byte.
/// The estimate mirrors what the execution policy actually measures, so it uses the canonical
/// temp root and the real directory template — an unresolved `TMPDIR` alias can be arbitrarily
/// deeper than the paths that are really passed — and reserves full counter digits so it never
/// undershoots.
fn hash_batch_prefix_bytes() -> Result<usize, GitError> {
    let temp = fs::canonicalize(std::env::temp_dir()).map_err(|_| GitError::SnapshotIo)?;
    Ok(
        HASH_BATCH_FLAGS.iter().map(|flag| flag.len() + 1).sum::<usize>()
            + temp.as_os_str().len()
            + 1 // separator before the scratch directory name
            + SNAPSHOT_SCRATCH_STEM.len()
            + std::process::id().to_string().len()
            + 1 // separator before the counter suffix
            + 20 // counter digits reserved at full u64 width
            + 1, // separator before each path argument
    )
}

/// Hashes indexed private snapshots through as few bounded no-filter commands as possible.
///
/// One managed sandbox replay costs a fixed multi-second startup per child, and packed batches
/// keep that cost per ~4 KiB of scratch argv instead of per blob. Indexes may be sparse; the
/// returned hashes align with `entries` order and each proves the exact written bytes.
async fn batch_hashes<R: SnapshotRunner>(
    scope: &GitScope,
    program: &Path,
    marker: char,
    entries: Vec<(usize, Vec<u8>)>,
    runner: &mut R,
) -> Result<Vec<GitObjectId>, GitError> {
    // The budget uses the real argument strings — canonical scratch paths and the fixed flags —
    // and a singleton past the execution ceiling fails closed here, before any scratch file is
    // written for it.
    let prefix_cost = hash_batch_prefix_bytes()?;
    let ranges = hash_batch_ranges(
        entries.len(),
        prefix_cost,
        |index| format!("{marker}{index}").len(),
        crate::execution::MAX_PRODUCT_ARGV_BYTES,
    )?;
    let mut hashes = Vec::with_capacity(entries.len());
    for (start, end) in ranges {
        let directory = SnapshotDirectory::new(scope)?;
        let mut names = Vec::with_capacity(end - start);
        for (index, bytes) in &entries[start..end] {
            let name = format!("{marker}{index}");
            directory.write(&name, bytes)?;
            names.push(name);
        }
        let intent = SnapshotIntent::hash_files(scope, program, directory, &names)?;
        let output = intent.accept(runner.run(intent.clone()).await?)?;
        hashes.extend(parse_batch_hashes(&output, names.len())?);
    }
    Ok(hashes)
}

/// Integration boundary: Execution owns admission, live-use checks, timeouts, cancellation and reap.
/// Export a private command once, bind its transferred launch identity, and acknowledge actual wait.
/// Cancellation handoff retains child plus intent; unknown drops quarantine rather than deleting files.
/// No implementation may run repository diff commands or transform SourceRead content with Git.
pub trait SnapshotRunner: Send {
    /// Runs this immutable operation with bounded streams and a Send future returning immutable actual-wait evidence after the Execution owner settles its one-time capability.
    fn run(
        &mut self,
        intent: SnapshotIntent,
    ) -> impl std::future::Future<Output = Result<CapturedProcessEvidence, GitError>> + Send;
    /// Proves one worktree-relative path may be read natively under the live host profile (T36B).
    ///
    /// Required and fallible with no permissive default: every collector integration routes
    /// this through its Execution owner, so a path the live sandbox policy denies (or that
    /// cannot be proven) refuses the whole capture attempt before any byte is read. The
    /// collector calls it before each tracked-path capture (staged mode included), before
    /// every consistency reread, and before untracked inspection.
    fn authorize_read_path(
        &mut self,
        path: &Path,
    ) -> impl std::future::Future<Output = Result<(), GitError>> + Send;
    /// Supplies a legacy unverified source hint, which the collector intentionally ignores.
    /// Implementations should use [`Self::current_observation`] after Workspace reconciliation.
    fn observation(
        &mut self,
        _authority: &AuthorityStamp,
        _path: &Path,
    ) -> Option<SourceObservation> {
        None
    }
    /// Supplies an optional Workspace-certified current observation for this exact path.
    /// Raw/native hints and merely loaded observations cannot construct the required token; only
    /// a fresh durable re-check (as `WorkspaceStore::confirm_current` performs) may mint one, so
    /// this is async by design rather than accepting a synchronous or cached guess. Implementations
    /// that have no durable store, or that cannot confirm this exact path is still current, return
    /// `None`; a missing or stale row is never converted into a fabricated `Current` token.
    fn current_observation(
        &mut self,
        _authority: &AuthorityStamp,
        _path: &Path,
    ) -> impl std::future::Future<Output = Option<CurrentObservation>> + Send {
        std::future::ready(None)
    }
}

/// One path's exact read, optionally correlated with a previously persisted Workspace observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotSource {
    /// Exact safe bounded read, absent only after Workspace observed the path missing.
    read: Option<SourceRead>,
    /// Optional durable revision/sequence correlation, never invented by the Git collector.
    observation: Option<SourceObservation>,
}

impl SnapshotSource {
    /// Returns digest/length for present bytes; absence is retained separately from empty content.
    pub fn bytes(&self) -> Option<&SourceBytes> {
        self.read.as_ref().map(SourceRead::bytes)
    }
    /// Returns exact persisted source revision and sequence when an observation was applicable.
    pub fn observation(&self) -> Option<&SourceObservation> {
        self.observation.as_ref()
    }
    /// Reads bytes only through Workspace's no-follow reader and checks supplied observation identity.
    fn capture(
        worktree: &WorktreeRef,
        authority_epoch: u64,
        path: &Path,
        observation: Option<CurrentObservation>,
    ) -> Result<Self, GitError> {
        let read = match read_authorized_source(
            worktree,
            path,
            SourceReadLimits::new(4096, MAX_SNAPSHOT_BLOB_BYTES)
                .map_err(|_| GitError::EvidenceTooLarge)?,
        ) {
            Ok(read) => Some(read),
            Err(ObservationError::Missing) => None,
            Err(ObservationError::TooLarge { .. }) => return Err(GitError::EvidenceTooLarge),
            Err(ObservationError::RootIdentityChanged) => return Err(GitError::UnstableSnapshot),
            Err(ObservationError::NotRegularFile | ObservationError::SymlinkEscape) => {
                return Err(GitError::UnsupportedSnapshot);
            }
            Err(_) => return Err(GitError::SnapshotIo),
        };
        let observation = observation.map(CurrentObservation::into_observation);
        if let Some(obs) = &observation
            && (obs.worktree() != worktree
                || obs.authority_epoch() != authority_epoch
                || obs.path().as_os_str().as_bytes() != path.as_os_str().as_bytes()
                || obs.coverage() != SourceCoverage::Complete
                || matches!(
                    obs.state(),
                    crate::workspace::observation::ObservedState::Present
                ) != read.is_some()
                || obs.bytes() != read.as_ref().map(SourceRead::bytes))
        {
            return Err(GitError::UnstableSnapshot);
        }
        Ok(Self { read, observation })
    }
    /// Returns exact present bytes, using an empty comparison file only for explicit absence.
    fn contents(&self) -> &[u8] {
        self.read.as_ref().map_or(&[], SourceRead::contents)
    }
}

/// Immutable per-path patch evidence; original paths never come from temporary diff headers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSnapshot {
    /// Scope matching the collection and comparison.
    scope: GitScope,
    /// One caller-supplied nonzero operation capture generation.
    generation: u64,
    /// Raw porcelain path, original rename source, XY, object names and modes.
    status: PathStatus,
    /// Complete bounded raw no-index output; Changes ignores temporary file headers.
    patch: Vec<u8>,
    /// Raw worktree read provenance, absent for staged comparisons.
    source: Option<SnapshotSource>,
}

impl PathSnapshot {
    /// Returns exact worktree/mode/epoch provenance.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }
    /// Returns the single capture generation shared by every path.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    /// Returns raw status identity including rename source and mode-only changes.
    pub fn status(&self) -> &PathStatus {
        &self.status
    }
    /// Returns bounded raw patch bytes, never a repository-filtered representation.
    pub fn patch(&self) -> &[u8] {
        &self.patch
    }
    /// Returns source digest and optional persisted revision/sequence for worktree comparisons.
    pub fn source(&self) -> Option<&SnapshotSource> {
        self.source.as_ref()
    }
}

/// One bounded, twice-checked collection with exact comparison and separate conflict/untracked data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSnapshot {
    /// Full operation scope, shared by every path and comparison.
    scope: GitScope,
    /// Nonzero operation generation; a retry replaces the whole attempt under this generation.
    generation: u64,
    /// Owner-scoped detail expansion reference.
    operation: String,
    /// Jointly checked comparison identities; baseline remains descriptive context.
    comparison: GitComparison,
    /// Original complete scoped porcelain metadata.
    status: GitStatus,
    /// Exactly one evidence record for each supported selected tracked path.
    paths: Vec<PathSnapshot>,
}

impl GitSnapshot {
    /// Returns the collection's exact scope.
    pub fn scope(&self) -> &GitScope {
        &self.scope
    }
    /// Returns the operation capture generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    /// Returns the owner-scoped expansion reference.
    pub fn operation_reference(&self) -> &str {
        &self.operation
    }
    /// Returns exact side identities, derived from complete metadata and exact raw worktree reads.
    pub fn comparison(&self) -> &GitComparison {
        &self.comparison
    }
    /// Returns scoped metadata with separate untracked and conflict lists.
    pub fn status(&self) -> &GitStatus {
        &self.status
    }
    /// Returns raw per-path evidence; paths with conflicts never receive guessed hunks.
    pub fn paths(&self) -> &[PathSnapshot] {
        &self.paths
    }
}

/// Captures each required side without Git filters, retries one unstable window, then returns stale.
/// `generation` and `operation` are allocated by the caller; no partial result claims complete state.
/// Symlinks, submodules, unborn HEAD, exceeded bounds and failed Execution evidence stay explicit errors.
/// Equality brackets detect observed changes, not an atomic filesystem transaction or ABA mutations.
#[allow(clippy::too_many_arguments)]
pub async fn collect_snapshot<R: SnapshotRunner>(
    authority: &AuthorityStamp,
    program: &Path,
    mode: DiffMode,
    generation: u64,
    operation: &str,
    baseline: BaselineContext,
    runner: &mut R,
) -> Result<GitSnapshot, GitError> {
    collect_snapshot_scoped(
        GitScope::from_authority(authority, mode),
        Some(authority),
        program,
        generation,
        operation,
        baseline,
        runner,
    )
    .await
}

/// Collects a complete snapshot inside a verified inherited helper without minting authority.
///
/// The caller supplies a daemon-derived scope whose worktree carries descriptor root identity.
/// Durable current-observation correlation is unavailable in the helper, so path snapshots retain
/// exact bytes but no fabricated [`CurrentObservation`] token.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn collect_snapshot_scoped<R: SnapshotRunner>(
    scope: GitScope,
    authority: Option<&AuthorityStamp>,
    program: &Path,
    generation: u64,
    operation: &str,
    baseline: BaselineContext,
    runner: &mut R,
) -> Result<GitSnapshot, GitError> {
    if generation == 0 || operation.is_empty() || operation.len() > 128 {
        return Err(GitError::InvalidOperationReference);
    }
    if !baseline.matches_scope(&scope) {
        return Err(GitError::IncompleteIdentity);
    }
    for attempt in 0..2 {
        match capture_attempt(
            authority,
            program,
            scope.clone(),
            generation,
            operation,
            baseline.clone(),
            runner,
        )
        .await
        {
            Err(GitError::UnstableSnapshot) if attempt == 0 => continue,
            result => return result,
        }
    }
    Err(GitError::UnstableSnapshot)
}

/// Collects one complete metadata bracket without ever invoking porcelain status or filters.
async fn metadata<R: SnapshotRunner>(
    scope: &GitScope,
    program: &Path,
    runner: &mut R,
) -> Result<[Vec<u8>; 4], GitError> {
    let mut result = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (slot, query) in [
        GitReadQuery::HeadIdentity,
        GitReadQuery::IndexState,
        GitReadQuery::HeadTree,
        GitReadQuery::UntrackedPaths,
    ]
    .into_iter()
    .enumerate()
    {
        let intent = SnapshotIntent::metadata(scope, program, query)?;
        let output = runner.run(intent.clone()).await?;
        if query == GitReadQuery::HeadIdentity
            && output.status().code() == Some(1)
            && output.stdout().complete
            && output.stderr().complete
            && !output.stdout().truncated
            && !output.stderr().truncated
        {
            return Err(GitError::UnbornHead);
        }
        result[slot] = intent.accept(output)?;
    }
    let head = result[0]
        .strip_suffix(b"\n")
        .ok_or(GitError::InvalidIdentity)?;
    if GitObjectId::parse(head)?.is_none() {
        return Err(GitError::InvalidIdentity);
    }
    Ok(result)
}

/// One immutable regular-file entry from the committed tree or one index stage.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TreeEntry {
    /// Exact octal regular-file mode, independent of content identity.
    mode: u32,
    /// Full strict object name; absent entries are represented by Option outside this type.
    oid: GitObjectId,
}

/// Parses terminal-NUL tree/index records without splitting raw paths on spaces or newlines.
/// Index stages 1..3 remain explicit conflicts; duplicate or incompatible records fail closed.
fn parse_entries(
    bytes: &[u8],
    index: bool,
) -> Result<BTreeMap<PathBuf, BTreeMap<u8, TreeEntry>>, GitError> {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return Err(GitError::InvalidPorcelain);
    }
    let mut entries: BTreeMap<PathBuf, BTreeMap<u8, TreeEntry>> = BTreeMap::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(GitError::InvalidPorcelain)?;
        let fields: Vec<_> = record[..tab].split(|byte| *byte == b' ').collect();
        if fields.len() != 3 {
            return Err(GitError::InvalidPorcelain);
        }
        let mode = super::parse_mode(fields[0])?;
        if !matches!(mode, 0o100644 | 0o100755) {
            return Err(GitError::UnsupportedSnapshot);
        }
        let (oid, stage) = if index {
            if fields[2].len() != 1 || !(b'0'..=b'3').contains(&fields[2][0]) {
                return Err(GitError::InvalidPorcelain);
            }
            (fields[1], fields[2][0] - b'0')
        } else {
            if fields[1] != b"blob" {
                return Err(GitError::UnsupportedSnapshot);
            }
            (fields[2], 0)
        };
        let oid = GitObjectId::parse(oid)?.ok_or(GitError::InvalidIdentity)?;
        let path = super::raw_path(&record[tab + 1..]);
        if !crate::workspace::observation::valid_relative_path(&path) {
            return Err(GitError::InvalidPorcelain);
        }
        let stages = entries.entry(path).or_default();
        if stages.insert(stage, TreeEntry { mode, oid }).is_some()
            || (stages.contains_key(&0) && stages.len() > 1)
        {
            return Err(GitError::InvalidPorcelain);
        }
    }
    Ok(entries)
}

/// Assembles one generation from safe plumbing and exact raw file reads under aggregate budgets.
/// Rename inference is deliberately absent: old/new raw identities are separate delete/add records.
/// One selected changed path awaiting its blob fetch and private no-index comparison.
/// `right` is a committed/index object side for `Staged` (an absent side compares as empty
/// bytes) and the already-captured worktree content for `Head`/`Unstaged`.
struct PendingCompare {
    /// Exact status identity pushed into the snapshot regardless of comparison success.
    entry: PathStatus,
    /// Left comparison object; an absent left side compares as empty bytes.
    left: Option<GitObjectId>,
    /// Right comparison object for `Staged`; `None` selects the worktree content.
    right: Option<Option<GitObjectId>>,
    /// Exact captured worktree source carried into the snapshot evidence.
    source: SnapshotSource,
}

#[allow(clippy::too_many_arguments)]
async fn capture_attempt<R: SnapshotRunner>(
    authority: Option<&AuthorityStamp>,
    program: &Path,
    scope: GitScope,
    generation: u64,
    operation: &str,
    baseline: BaselineContext,
    runner: &mut R,
) -> Result<GitSnapshot, GitError> {
    let before = metadata(&scope, program, runner).await?;
    let head_entries = parse_entries(&before[2], false)?;
    let index_entries = parse_entries(&before[1], true)?;
    let union: std::collections::BTreeSet<_> = head_entries
        .keys()
        .chain(index_entries.keys())
        .cloned()
        .collect();
    if !before[3].is_empty() && !before[3].ends_with(&[0]) {
        return Err(GitError::InvalidPorcelain);
    }
    let mut status = GitStatus {
        scope: Some(scope.clone()),
        ..GitStatus::default()
    };
    for bytes in before[3]
        .split(|byte| *byte == 0)
        .filter(|bytes| !bytes.is_empty())
    {
        let path = super::raw_path(bytes);
        if !crate::workspace::observation::valid_relative_path(&path) {
            return Err(GitError::InvalidPorcelain);
        }
        status.untracked.push(PathStatus {
            kind: super::StatusKind::Untracked,
            path,
            original_path: None,
            status: None,
            modes: None,
            objects: None,
            conflict_stages: Vec::new(),
        });
    }
    if union.len() + status.untracked.len() > MAX_SNAPSHOT_PATHS
        || union
            .iter()
            .map(|path| path.as_os_str().as_bytes().len())
            .chain(
                status
                    .untracked
                    .iter()
                    .map(|entry| entry.path().as_os_str().as_bytes().len()),
            )
            .sum::<usize>()
            > MAX_SNAPSHOT_PATH_BYTES
    {
        return Err(GitError::EvidenceTooLarge);
    }
    for entry in status.untracked() {
        runner.authorize_read_path(entry.path()).await?;
        inspect_untracked(scope.worktree(), entry.path())?;
    }
    // Exact safe in-process reads cover every non-conflict union path, not only paths Git's stat
    // cache happened to mark dirty. Blob bytes are fetched later, only for sides a selected
    // comparison actually needs.
    let mut paths = Vec::new();
    let mut sources = BTreeMap::new();
    let mut total_bytes = 0usize;
    let mut patch_bytes = 0usize;
    let mut working = blake3::Hasher::new();
    for path in &union {
        let stages = index_entries.get(path);
        if stages.is_some_and(|entries| !entries.contains_key(&0)) {
            status.conflicts.push(PathStatus {
                kind: super::StatusKind::Unmerged,
                path: path.clone(),
                original_path: None,
                status: None,
                modes: None,
                objects: None,
                conflict_stages: stages
                    .expect("unmerged stages exist")
                    .iter()
                    .map(|(stage, entry)| super::ConflictStage {
                        stage: *stage,
                        mode: entry.mode,
                        object: entry.oid.clone(),
                    })
                    .collect(),
            });
            continue;
        }
        let current = match authority {
            Some(authority) => runner.current_observation(authority, path).await,
            None => None,
        };
        // T36B: the live per-path proof precedes every native capture, staged mode included.
        runner.authorize_read_path(path).await?;
        let source =
            SnapshotSource::capture(scope.worktree(), scope.authority_epoch(), path, current)?;
        total_bytes += source.contents().len();
        if total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
            return Err(GitError::EvidenceTooLarge);
        }
        let mode_w = source.read.as_ref().map_or(0, SourceRead::git_mode);
        let raw_path = path.as_os_str().as_bytes();
        working.update(&(raw_path.len() as u64).to_le_bytes());
        working.update(raw_path);
        working.update(&mode_w.to_le_bytes());
        working.update(&(source.contents().len() as u64).to_le_bytes());
        working.update(source.contents());
        sources.insert(path.clone(), source);
    }
    // One batched no-filter hash command classifies every unchanged worktree side by object name,
    // replacing the per-path cat-file plus per-blob verification commands that made a sandboxed
    // capture cost two multi-second sandbox replays for every clean tracked path.
    let present: Vec<&PathBuf> = sources
        .iter()
        .filter(|(_, source)| source.read.is_some())
        .map(|(path, _)| path)
        .collect();
    let mut work_hashes: BTreeMap<PathBuf, GitObjectId> = BTreeMap::new();
    if !present.is_empty() {
        let entries: Vec<(usize, Vec<u8>)> = present
            .iter()
            .enumerate()
            .map(|(index, path)| (index, sources[*path].contents().to_vec()))
            .collect();
        let hashes = batch_hashes(&scope, program, 'w', entries, runner).await?;
        for (path, oid) in present.into_iter().zip(hashes) {
            work_hashes.insert(path.clone(), oid);
        }
    }
    // Pure classification: X from committed/index identities, Y from the proven worktree hash.
    let mut compares: Vec<PendingCompare> = Vec::new();
    for path in union {
        let head = head_entries.get(&path).and_then(|entries| entries.get(&0));
        let stages = index_entries.get(&path);
        if stages.is_some_and(|entries| !entries.contains_key(&0)) {
            continue;
        }
        let index = stages.and_then(|entries| entries.get(&0));
        let source = &sources[&path];
        let mode_w = source.read.as_ref().map_or(0, SourceRead::git_mode);
        let x = match (head, index) {
            (None, Some(_)) => b'A',
            (Some(_), None) => b'D',
            (left, right) if left == right => b'.',
            _ => b'M',
        };
        let y = match (index, &source.read) {
            (Some(_), None) => b'D',
            (Some(index), Some(_)) => {
                if index.mode == mode_w
                    && work_hashes.get(&path).is_some_and(|oid| oid == &index.oid)
                {
                    b'.'
                } else {
                    b'M'
                }
            }
            _ => b'.',
        };
        if [x, y] == *b".." {
            continue;
        }
        let modes = [
            head.map_or(0, |entry| entry.mode),
            index.map_or(0, |entry| entry.mode),
            mode_w,
        ];
        let objects = [
            head.map(|entry| entry.oid.clone()),
            index.map(|entry| entry.oid.clone()),
        ];
        let entry = PathStatus {
            kind: super::StatusKind::Ordinary,
            path,
            original_path: None,
            status: Some([x, y]),
            modes: Some(modes),
            objects: Some(objects.clone()),
            conflict_stages: Vec::new(),
        };
        let selected = match scope.mode() {
            DiffMode::Head => true,
            DiffMode::Staged => x != b'.',
            DiffMode::Unstaged => y != b'.',
        };
        if !selected {
            continue;
        }
        status.tracked.push(entry.clone());
        // Left is always a committed/index side; right is a blob only for `Staged`, whose absent
        // side compares as empty bytes rather than the working-tree content.
        let (left, right) = match scope.mode() {
            DiffMode::Head => (objects[0].clone(), None),
            DiffMode::Staged => (
                objects[0].clone(),
                Some(index.map(|entry| entry.oid.clone())),
            ),
            DiffMode::Unstaged => (index.map(|entry| entry.oid.clone()), None),
        };
        compares.push(PendingCompare {
            entry,
            left,
            right,
            source: source.clone(),
        });
    }
    // Fetch each distinct comparison side once, then prove every fetched blob's bytes against its
    // requested full object name in one batched verification before any comparison uses them.
    let mut needed: Vec<GitObjectId> = compares
        .iter()
        .flat_map(|pending| {
            pending
                .left
                .iter()
                .cloned()
                .chain(pending.right.iter().flatten().cloned())
        })
        .collect();
    needed.sort();
    needed.dedup();
    let mut blobs = BTreeMap::new();
    let mut fetched: Vec<(GitObjectId, Vec<u8>)> = Vec::new();
    for oid in needed {
        let intent = SnapshotIntent::blob(scope.clone(), program, &oid)?;
        let bytes = intent.accept(runner.run(intent.clone()).await?)?;
        total_bytes += bytes.len();
        if total_bytes > MAX_SNAPSHOT_TOTAL_BYTES {
            return Err(GitError::EvidenceTooLarge);
        }
        fetched.push((oid.clone(), bytes.clone()));
        blobs.insert(oid, bytes);
    }
    if !fetched.is_empty() {
        let entries: Vec<(usize, Vec<u8>)> = fetched
            .iter()
            .enumerate()
            .map(|(index, (_, bytes))| (index, bytes.clone()))
            .collect();
        let verified = batch_hashes(&scope, program, 'v', entries, runner).await?;
        for ((oid, _), hash) in fetched.iter().zip(verified) {
            if oid != &hash {
                return Err(GitError::ObjectHashMismatch);
            }
        }
    }
    for pending in compares {
        let empty = Vec::new();
        let left = match &pending.left {
            Some(oid) => &blobs[oid],
            None => &empty,
        };
        let right = match &pending.right {
            Some(Some(oid)) => &blobs[oid],
            Some(None) => &empty,
            None => pending.source.contents(),
        };
        let intent = SnapshotIntent::compare(scope.clone(), program, left, right)?;
        let patch = intent.accept(runner.run(intent.clone()).await?)?;
        patch_bytes += patch.len();
        if patch_bytes > MAX_SNAPSHOT_PATCH_BYTES {
            return Err(GitError::EvidenceTooLarge);
        }
        paths.push(PathSnapshot {
            scope: scope.clone(),
            generation,
            status: pending.entry,
            patch,
            source: (scope.mode() != DiffMode::Staged).then_some(pending.source),
        });
    }
    // Exact safe reads cover every union path, not only paths Git's stat cache happened to mark dirty.
    for (path, source) in &sources {
        // T36B: repeat authorization before the consistency reread, exactly as before capture.
        runner.authorize_read_path(path).await?;
        let after = SnapshotSource::capture(scope.worktree(), scope.authority_epoch(), path, None)?;
        if after.read != source.read {
            return Err(GitError::UnstableSnapshot);
        }
    }
    for entry in status.untracked() {
        runner.authorize_read_path(entry.path()).await?;
        inspect_untracked(scope.worktree(), entry.path())?;
    }
    if metadata(&scope, program, runner).await? != before {
        return Err(GitError::UnstableSnapshot);
    }
    let head = evidence_identity(b"workspace-git-head-v1", &before[0]);
    let index = evidence_identity(b"workspace-git-index-v1", &before[1]);
    for component in &before[1..=3] {
        working.update(&(component.len() as u64).to_le_bytes());
        working.update(component);
    }
    let work = evidence_identity(
        b"workspace-git-raw-working-v1",
        working.finalize().as_bytes(),
    );
    let (left, right) = match scope.mode() {
        DiffMode::Head => (head, work),
        DiffMode::Staged => (head, index),
        DiffMode::Unstaged => (index, work),
    };
    let comparison = GitComparison::new(scope.clone(), left, right, baseline);
    Ok(GitSnapshot {
        scope,
        generation,
        operation: operation.to_owned(),
        comparison,
        status,
        paths,
    })
}

/// Rejects untracked symlink/special entries without reading bytes; disappearing paths trigger retry.
fn inspect_untracked(worktree: &WorktreeRef, path: &Path) -> Result<(), GitError> {
    crate::workspace::observation::inspect_authorized_source_kind(worktree, path).map_err(|error| {
        match error {
            ObservationError::SymlinkEscape | ObservationError::NotRegularFile => {
                GitError::UnsupportedSnapshot
            }
            ObservationError::Missing | ObservationError::RootIdentityChanged => {
                GitError::UnstableSnapshot
            }
            _ => GitError::SnapshotIo,
        }
    })
}

#[cfg(test)]
mod batch_tests {
    use super::{
        GitError, MAX_HASH_BATCH_ARGV_BYTES, hash_batch_prefix_bytes, hash_batch_ranges,
        parse_batch_hashes,
    };
    use crate::workspace::git::GitObjectId;

    /// The planning estimate covers the real argument strings: fixed flags plus one canonical
    /// scratch path, never smaller than the unresolved temp directory it is derived from.
    #[test]
    fn batch_prefix_budget_uses_the_canonical_scratch_path() {
        let prefix = hash_batch_prefix_bytes().unwrap();
        let resolved = std::env::temp_dir();
        let flags = super::HASH_BATCH_FLAGS
            .iter()
            .map(|f| f.len() + 1)
            .sum::<usize>();
        assert!(
            prefix > flags + resolved.as_os_str().len(),
            "prefix {prefix} must cover the flags and the resolved temp root"
        );
        // The reserved name shape (stem, pid digits, full-width counter) is part of the estimate.
        let canonical = std::fs::canonicalize(&resolved).unwrap();
        assert!(
            prefix
                >= flags
                    + canonical.as_os_str().len()
                    + super::SNAPSHOT_SCRATCH_STEM.len()
                    + std::process::id().to_string().len()
                    + 2
        );
    }

    /// Batch output must be exactly one terminal-LF object name per argument, in argument order.
    #[test]
    fn batch_hash_output_parses_in_argument_order() {
        let line = |index: u8| {
            (0..40)
                .map(|_| format!("{index:x}"))
                .collect::<String>()
                .into_bytes()
        };
        let mut output = Vec::new();
        // Indexes start at one: the all-zero name is the absent-object sentinel and fails closed.
        for index in 1u8..4 {
            output.extend(line(index));
            output.push(b'\n');
        }
        let parsed = parse_batch_hashes(&output, 3).unwrap();
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[1], GitObjectId::parse(&line(2)).unwrap().unwrap());
        // Missing lines, extra lines, a missing terminal LF, or a malformed name fail closed.
        assert!(parse_batch_hashes(&output, 2).is_err());
        assert!(parse_batch_hashes(&output[..output.len() - 1], 3).is_err());
        output.truncate(output.len() - 41);
        assert!(parse_batch_hashes(&output, 3).is_err());
    }

    /// Batches stay below the argv budget whatever the temp-root depth, never reorder entries,
    /// a single oversized entry still forms its own bounded batch, and an entry whose own argv
    /// would exceed the execution ceiling fails closed instead of planning any batch.
    #[test]
    fn batch_ranges_pack_below_the_argv_budget() {
        assert!(
            hash_batch_ranges(0, 100, |_| 4, usize::MAX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            hash_batch_ranges(3, 100, |_| 4, usize::MAX).unwrap(),
            vec![(0, 3)]
        );
        let ranges = hash_batch_ranges(100, 100, |_| 4, usize::MAX).unwrap();
        let mut cursor = 0usize;
        for (start, end) in &ranges {
            assert_eq!(*start, cursor, "ranges are consecutive and ordered");
            cursor = *end;
            let bytes: usize = 100 + (100 + 4 + 1) * (*end - *start);
            assert!(
                bytes <= MAX_HASH_BATCH_ARGV_BYTES,
                "batch argv {bytes} exceeds budget"
            );
        }
        assert_eq!(cursor, 100);
        // A 4096-byte scratch pathname cannot fit any shared batch; it still runs alone.
        assert_eq!(
            hash_batch_ranges(
                2,
                100,
                |index| if index == 0 { 4000 } else { 4 },
                usize::MAX
            )
            .unwrap(),
            vec![(0, 1), (1, 2)]
        );
        // An entry whose own argv (prefix + name + separator) exceeds the ceiling fails closed
        // before any batch — and therefore any scratch file — is planned.
        let error =
            hash_batch_ranges(2, 100, |index| if index == 0 { 5000 } else { 4 }, 4096).unwrap_err();
        assert!(matches!(error, GitError::EvidenceTooLarge));
        // Even a zero-byte name fails closed when the fixed prefix alone is past the ceiling.
        assert!(hash_batch_ranges(1, 5000, |_| 0, 4096).is_err());
    }
}
