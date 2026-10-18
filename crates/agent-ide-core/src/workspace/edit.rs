//! Descriptor-held, one-use replacement for a single authorized source file.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

use super::{
    authority::{AuthorityStamp, WorktreeRef},
    observation::{
        ObservationError, SourceBytes, SourceCoverage, SourceObservation, valid_relative_path,
    },
};

/// Maximum UTF-8 replacement content accepted by Workspace.
pub const MAX_EDIT_CONTENT_BYTES: usize = 1024 * 1024;

/// A completed-context binding to exact bytes (or exact absence) under one authority epoch.
///
/// This value is minted only from a complete Workspace observation. It retains the worktree
/// incarnation, path, byte digest/length, observation revision and authority epoch needed to
/// reject a reference from another file, activation, or older source state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditSourceRef {
    /// Worktree identity and incarnation observed by the completed context call.
    worktree: WorktreeRef,
    /// Authority generation current for that observation.
    authority_epoch: u64,
    /// Durable observation ordering value retained for later refresh correlation.
    sequence: u64,
    /// Opaque context result identifier; it is never interpreted as path authority.
    observation_ref: String,
    /// Workspace source revision distinct from Git and operation identifiers.
    source_revision: String,
    /// Exact relative UTF-8 path whose bytes or absence were observed.
    path: PathBuf,
    /// Digest and length of present bytes, or absence for an observed missing final component.
    bytes: Option<SourceBytes>,
}

impl EditSourceRef {
    /// Mints an edit reference from a complete observation; partial/unknown observations are stale.
    pub fn from_observation(observation: &SourceObservation) -> Result<Self, EditOutcome> {
        if observation.coverage() != SourceCoverage::Complete {
            return Err(EditOutcome::StaleSource);
        }
        Ok(Self {
            worktree: observation.worktree().clone(),
            authority_epoch: observation.authority_epoch(),
            sequence: observation.sequence(),
            observation_ref: observation.reference().as_str().to_owned(),
            source_revision: observation.source_revision().as_str().to_owned(),
            path: observation.path().to_path_buf(),
            bytes: observation.bytes().cloned(),
        })
    }

    /// Returns the worktree incarnation to which this reference is permanently scoped.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the authority epoch under which the completed observation was collected.
    pub const fn authority_epoch(&self) -> u64 {
        self.authority_epoch
    }

    /// Returns the monotonic durable observation sequence carried by the source reference.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the opaque completed-context correlation reference.
    pub fn observation_ref(&self) -> &str {
        &self.observation_ref
    }

    /// Returns the source revision, which is distinct from Git and operation identifiers.
    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    /// Returns the exact relative UTF-8 path observed for this reference.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns digest/length for a present source, or `None` for an observed missing target.
    pub fn bytes(&self) -> Option<&SourceBytes> {
        self.bytes.as_ref()
    }
}

/// Descriptor-derived metadata that must still match immediately before replacement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditMetadata {
    /// Device containing the retained original descriptor.
    device: u64,
    /// Inode of the retained original descriptor.
    inode: u64,
    /// Ordinary permission bits that replacement must preserve.
    mode: u32,
    /// Owner that replacement must preserve without accepting caller metadata.
    uid: u32,
    /// Group that replacement must preserve without accepting caller metadata.
    gid: u32,
    /// Complete descriptor-read xattrs copied before the private temporary file is installed.
    extended: Vec<(Vec<u8>, Vec<u8>)>,
}

impl EditMetadata {
    /// Returns the existing Unix permission bits preserved by a successful replacement.
    pub const fn mode(&self) -> u32 {
        self.mode
    }
}

/// A descriptor-held present file or eligible missing final component under an existing directory.
///
/// The parent and, for replacements, original file descriptors stay open until settlement. The
/// path is retained for bounded reporting only and is never used as ambient filesystem authority.
#[derive(Debug)]
pub struct CurrentEditTarget {
    /// Exact bounded relative UTF-8 path retained only for result correlation.
    path: PathBuf,
    /// Completed-context binding matched during resolution.
    source_ref: EditSourceRef,
    /// Owned final-parent descriptor used for every subsequent name operation.
    parent: File,
    /// Raw final component, known to contain neither slash nor NUL.
    name: Vec<u8>,
    /// Retained original regular descriptor, absent only for eligible creation.
    file: Option<File>,
    /// Preservable original metadata, absent only for eligible creation.
    metadata: Option<EditMetadata>,
}

impl CurrentEditTarget {
    /// Resolves a source reference beneath the authority root without following any path component.
    ///
    /// Present files must be regular, singly linked, owned by the effective user, free of special
    /// mode bits, and valid UTF-8; descriptor-readable xattrs are preserved. A missing final
    /// component may create missing parent directories beneath the worktree, then requires every
    /// parent to be a real writable directory. Any mismatch or unreadable metadata is returned
    /// before the target file is written. Batch callers resolve every target before the first write.
    pub fn resolve(
        authority: &AuthorityStamp,
        path: &Path,
        source_ref: EditSourceRef,
    ) -> Result<Self, EditOutcome> {
        Self::resolve_scope(authority.worktree(), authority.epoch(), path, source_ref)
    }

    /// Validates scope/reference before the descriptor-safe walk. An observed absence may create
    /// parents; batch callers must complete read-only admission before resolving any target.
    fn resolve_scope(
        worktree: &WorktreeRef,
        authority_epoch: u64,
        path: &Path,
        source_ref: EditSourceRef,
    ) -> Result<Self, EditOutcome> {
        if source_ref.worktree() != worktree
            || source_ref.authority_epoch() != authority_epoch
            || source_ref.path() != path
            || !valid_edit_path(path)
        {
            return Err(EditOutcome::StaleSource);
        }
        if source_ref.bytes().is_none() {
            create_edit_parents(worktree, path)?;
        }
        let (parent, name) = open_parent(worktree, path)?;
        if !parent_is_writable(&parent) {
            return Err(EditOutcome::UnsafeTarget);
        }
        match open_final(parent.as_raw_fd(), &name) {
            Ok(mut file) => {
                let metadata = safe_metadata(&file)?;
                let bytes = read_utf8(&mut file)?;
                if source_ref.bytes() != Some(&SourceBytes::from_bytes(&bytes)) {
                    return Err(EditOutcome::StaleSource);
                }
                Ok(Self {
                    path: path.to_path_buf(),
                    source_ref,
                    parent,
                    name,
                    file: Some(file),
                    metadata: Some(metadata),
                })
            }
            Err(EditOutcome::StaleSource) if source_ref.bytes().is_none() => Ok(Self {
                path: path.to_path_buf(),
                source_ref,
                parent,
                name,
                file: None,
                metadata: None,
            }),
            Err(EditOutcome::StaleSource) => Err(EditOutcome::StaleSource),
            Err(error) => Err(error),
        }
    }

    /// Returns the exact caller-visible relative path retained for outcome rendering.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the completed-context reference matched during descriptor-safe resolution.
    pub fn source_ref(&self) -> &EditSourceRef {
        &self.source_ref
    }

    /// Returns preserved metadata for replacement, or `None` for an eligible creation.
    pub fn metadata(&self) -> Option<&EditMetadata> {
        self.metadata.as_ref()
    }
}

/// A one-use Workspace capability tied to an operation and target path.
#[derive(Debug)]
pub struct EditPermit {
    /// Bounded stable operation identifier used for uncertain-result correlation.
    operation_id: String,
    /// Exact path that the permit may affect once.
    path: PathBuf,
}

impl EditPermit {
    /// Mints a bounded permit after Changes has durably prepared the same operation and path.
    pub fn new(operation_id: impl Into<String>, path: PathBuf) -> Result<Self, EditOutcome> {
        let operation_id = operation_id.into();
        if operation_id.is_empty() || operation_id.len() > 128 || !valid_edit_path(&path) {
            return Err(EditOutcome::UnsafeTarget);
        }
        Ok(Self { operation_id, path })
    }

    /// Returns the operation identifier used only for uncertain-result correlation.
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

/// Exact post-read evidence returned after a known single-file effect.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EditPostRead {
    /// Exact relative path re-opened under the retained parent descriptor.
    path: PathBuf,
    /// Digest and length computed from the exact returned final bytes.
    bytes: SourceBytes,
    /// Exact bounded UTF-8 final bytes supplied to observation refresh.
    contents: Vec<u8>,
}

impl EditPostRead {
    /// Returns the exact relative path re-opened through the retained parent descriptor.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the digest and length of the exact final descriptor bytes.
    pub fn bytes(&self) -> &SourceBytes {
        &self.bytes
    }

    /// Returns the exact bounded UTF-8 final bytes for observation refresh.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }
}

/// Closed Workspace-local settlement of one permitted edit attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditOutcome {
    /// A missing final component was created and exactly post-read.
    Created(EditPostRead),
    /// The original regular file was atomically replaced and exactly post-read.
    Replaced(EditPostRead),
    /// Requested bytes already matched the current descriptor; no filesystem write occurred.
    Unchanged(EditPostRead),
    /// Authority, path, descriptor identity, or pre-write bytes no longer match the reference.
    StaleSource,
    /// The target or metadata cannot be handled without unsafe or ambiguous effects.
    UnsafeTarget,
    /// Cancellation was observed before any target effect.
    CancelledNoEffect,
    /// Deadline expiry was observed before any target effect.
    DeadlineNoEffect,
    /// Capacity refusal was observed before any target effect.
    CapacityNoEffect,
    /// A write may have occurred but exact completion and post-state could not be established.
    OutcomeUnknown {
        /// Stable operation requiring target inspection rather than blind replay.
        operation_id: String,
        /// Exact bounded target path whose current state must be inspected.
        path: PathBuf,
    },
}

/// Applies one full-content edit through retained descriptors and consumes the permit on every path.
///
/// `still_authorized` and `continue_before_effect` are evaluated immediately before any target
/// effect. False authority is stale; false continuation is cancellation with zero writes. The
/// requested content must be UTF-8 and at most 128 KiB on the wire (the facade's
/// `MAX_EDIT_ARGUMENT_CONTENT_BYTES`; this function's own splice ceiling is
/// `MAX_EDIT_CONTENT_BYTES`, 1 MiB, for internally spliced whole-file content). Temporary-file
/// failures before rename are `unsafe_target`; any failure after rename is
/// `outcome_unknown(operation_id, path)`.
pub fn replace_if_current(
    permit: EditPermit,
    target: CurrentEditTarget,
    source_ref: &EditSourceRef,
    content: &[u8],
    still_authorized: impl FnOnce() -> bool,
    continue_before_effect: impl FnOnce() -> bool,
) -> EditOutcome {
    replace_if_current_with_checkpoint(
        permit,
        target,
        source_ref,
        content,
        still_authorized,
        continue_before_effect,
        || {},
    )
}

/// Applies one replacement and runs `after_prepare` only after private temporary bytes are durable.
///
/// Production uses [`replace_if_current`]'s no-op checkpoint. The test-only observable checkpoint
/// proves a native edit during preparation is rejected by the final identity check. Cancellation is
/// checked immediately before installation, so private preparation cannot overwrite a target.
fn replace_if_current_with_checkpoint(
    permit: EditPermit,
    mut target: CurrentEditTarget,
    source_ref: &EditSourceRef,
    content: &[u8],
    still_authorized: impl FnOnce() -> bool,
    continue_before_effect: impl FnOnce() -> bool,
    after_prepare: impl FnOnce(),
) -> EditOutcome {
    if permit.path != target.path || source_ref != &target.source_ref || !still_authorized() {
        return EditOutcome::StaleSource;
    }
    if content.len() > MAX_EDIT_CONTENT_BYTES || std::str::from_utf8(content).is_err() {
        return EditOutcome::UnsafeTarget;
    }
    if recheck_target(&mut target).is_err() {
        return EditOutcome::StaleSource;
    }
    if target
        .file
        .as_mut()
        .is_some_and(|file| descriptor_equals(file, content))
    {
        return post_read(&target).map_or(EditOutcome::StaleSource, EditOutcome::Unchanged);
    }
    let created = target.file.is_none();
    match write_replacement(&mut target, content, continue_before_effect, after_prepare) {
        Ok(()) => match post_read(&target) {
            Ok(read) if created => EditOutcome::Created(read),
            Ok(read) => EditOutcome::Replaced(read),
            Err(_) => EditOutcome::OutcomeUnknown {
                operation_id: permit.operation_id,
                path: target.path,
            },
        },
        Err(WriteFailure::BeforeEffect) => EditOutcome::UnsafeTarget,
        Err(WriteFailure::StaleSource) => EditOutcome::StaleSource,
        Err(WriteFailure::CancelledNoEffect) => EditOutcome::CancelledNoEffect,
        Err(WriteFailure::AfterPossibleEffect) => EditOutcome::OutcomeUnknown {
            operation_id: permit.operation_id,
            path: target.path,
        },
    }
}

/// Validates the public edit path as relative UTF-8 in addition to Workspace's raw path rules.
fn valid_edit_path(path: &Path) -> bool {
    valid_relative_path(path) && path.to_str().is_some()
}

/// Device and inode read from a retained descriptor, used only within one preflight attempt.
type EditNativeIdentity = [u64; 2];

/// One admitted destination's native anchor and unresolved name components, with no path grant.
struct EditDestination {
    /// Deepest existing no-follow directory; retained through native name comparison.
    parent: File,
    /// Device/inode of that retained parent, so alternate parent spellings share one anchor.
    anchor: EditNativeIdentity,
    /// Missing suffix, or the existing target's final component, all already validated as UTF-8.
    suffix: Vec<Vec<u8>>,
    /// Existing regular target's device/inode; `None` means its final component is absent.
    file: Option<EditNativeIdentity>,
}

/// Captures descriptor identity and owns a copy of validated suffix components without writing.
/// Metadata failure refuses admission; `file` is present only for a safely opened regular target.
fn edit_destination(
    parent: File,
    suffix: &[&[u8]],
    file: Option<EditNativeIdentity>,
) -> Result<EditDestination, &'static str> {
    let metadata = parent
        .metadata()
        .map_err(|_| "parent_unavailable: cannot identify the existing directory")?;
    Ok(EditDestination {
        parent,
        anchor: [metadata.dev(), metadata.ino()],
        suffix: suffix.iter().map(|part| part.to_vec()).collect(),
        file,
    })
}

/// Checks a destination and every existing parent without changing the worktree or source bytes.
///
/// `path` must be a bounded relative UTF-8 file path beneath `worktree`. Missing suffixes are
/// admitted only below a writable real directory, with every component within the filesystem's
/// name limit. Existing targets must satisfy replacement metadata rules. Static refusal reasons
/// distinguish invalid paths, unsafe parents/targets, unavailable roots and unwritable parents.
/// This is admission evidence only: later descriptor resolution rechecks authority and races.
pub(crate) fn preflight_edit_path(worktree: &WorktreeRef, path: &Path) -> Result<(), &'static str> {
    preflight_edit_destination(worktree, path).map(|_| ())
}

/// Performs the same read-only destination admission as `preflight_edit_path`, retaining native
/// identity and unresolved suffixes for whole-batch comparisons. Missing parents are never made.
fn preflight_edit_destination(
    worktree: &WorktreeRef,
    path: &Path,
) -> Result<EditDestination, &'static str> {
    if !valid_edit_path(path) {
        return Err(
            "invalid_path: use a nonempty worktree-relative file path without dot or parent components",
        );
    }
    let mut parent = super::observation::open_root_directory(worktree.worktree_path())
        .map_err(|_| "root_unavailable: reactivate the worktree")?;
    if let Some(expected) = worktree.native_root_identity()
        && super::observation::native_directory_identity(&parent)
            .map_err(|_| "root_unavailable: cannot identify the worktree")?
            != expected
    {
        return Err("root_identity_changed: reactivate the worktree");
    }
    let parts: Vec<&[u8]> = path
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .collect();
    // SAFETY: parent is an owned, open directory descriptor; fpathconf does not modify it.
    let name_limit = unsafe { libc::fpathconf(parent.as_raw_fd(), libc::_PC_NAME_MAX) };
    if name_limit < 0 {
        return Err("path_unavailable: cannot determine the filesystem name limit");
    }
    if parts.iter().any(|part| part.len() > name_limit as usize) {
        return Err("name_too_long: shorten the file or parent name");
    }
    for (index, part) in parts[..parts.len() - 1].iter().enumerate() {
        match super::observation::open_directory(parent.as_raw_fd(), OsStr::from_bytes(part)) {
            Ok(descriptor) => {
                // SAFETY: open_directory returned a new descriptor owned solely by this File.
                parent = unsafe { File::from_raw_fd(descriptor) };
            }
            Err(ObservationError::Missing) => {
                return if parent_is_writable(&parent) {
                    edit_destination(parent, &parts[index..], None)
                } else {
                    Err("parent_not_writable: permit directory creation in the existing parent")
                };
            }
            Err(ObservationError::SymlinkEscape) => {
                return Err(
                    "unsafe_parent: use real directories without symlinks or file components",
                );
            }
            Err(_) => return Err("parent_unavailable: check directory access permissions"),
        }
    }
    if !parent_is_writable(&parent) {
        return Err("parent_not_writable: permit replacement or creation in the parent directory");
    }
    let file = match open_final(parent.as_raw_fd(), parts.last().expect("validated path")) {
        Ok(file) => {
            let metadata = safe_metadata(&file).map_err(
                |_| "unsafe_target: use an owned singly linked regular file with preservable metadata",
            )?;
            Some([metadata.device, metadata.inode])
        }
        Err(EditOutcome::StaleSource) => None,
        Err(_) => {
            return Err(
                "unsafe_target: check target permissions and remove symlink or special-file aliases",
            );
        }
    };
    edit_destination(parent, &parts[parts.len() - 1..], file)
}

/// Preflights a facade-bounded batch before any destination-parent creation or source write.
///
/// Returns zero-based offending entries and actionable reasons, or an empty vector. Existing
/// aliases compare no-follow device/inode identities. Multiple unresolved destinations with one
/// native anchor compare exact kernel name lookups in private, empty scratch on the same volume
/// and with matching case semantics. The existing scratch owner cleans up when comparison ends;
/// no source bytes are copied. An unavailable/mismatched oracle refuses that future group, while
/// single future destinations and distinct existing files need no scratch. Later source races
/// still require the ordinary descriptor/freshness checks; this is not crash atomicity.
pub(crate) fn preflight_edit_paths(
    worktree: &WorktreeRef,
    paths: &[&Path],
) -> Vec<(usize, &'static str)> {
    let destinations: Vec<_> = paths
        .iter()
        .map(|path| preflight_edit_destination(worktree, path))
        .collect();
    let mut refusals: Vec<_> = destinations
        .iter()
        .enumerate()
        .filter_map(|(index, result)| result.as_ref().err().map(|reason| (index, *reason)))
        .collect();
    if !refusals.is_empty() {
        return refusals;
    }
    let destinations: Vec<_> = destinations
        .into_iter()
        .map(|entry| entry.expect("every destination was admitted"))
        .collect();
    for (left_index, left) in destinations.iter().enumerate() {
        for (right_index, right) in destinations.iter().enumerate().skip(left_index + 1) {
            if left.file.is_some() && left.file == right.file {
                refusals.push((
                    right_index,
                    "repeated_file: another entry names the same filesystem destination",
                ));
            } else if left.anchor == right.anchor {
                if left.suffix.starts_with(&right.suffix) {
                    refusals.push((
                        right_index,
                        "future_file_parent: another target needs this file path as a directory",
                    ));
                } else if right.suffix.starts_with(&left.suffix) {
                    refusals.push((
                        left_index,
                        "future_file_parent: another target needs this file path as a directory",
                    ));
                }
            }
        }
    }
    if !refusals.is_empty() {
        refusals.sort_unstable_by_key(|(index, _)| *index);
        refusals.dedup_by_key(|(index, _)| *index);
        return refusals;
    }
    let mut groups: BTreeMap<EditNativeIdentity, Vec<usize>> = BTreeMap::new();
    for (index, destination) in destinations
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.file.is_none())
    {
        groups.entry(destination.anchor).or_default().push(index);
    }
    for indices in groups.values().filter(|group| group.len() > 1) {
        let entries: Vec<_> = indices.iter().map(|index| &destinations[*index]).collect();
        let native = match probe_future_edit_names(worktree, &entries) {
            Ok(native) => native,
            Err(reason) => {
                refusals.push((indices[0], reason));
                continue;
            }
        };
        for (left_index, left) in native.iter().enumerate() {
            for (right_index, right) in native.iter().enumerate().skip(left_index + 1) {
                let left_final = left.last().expect("validated suffix is nonempty");
                let right_final = right.last().expect("validated suffix is nonempty");
                if left_final == right_final {
                    refusals.push((
                        indices[right_index],
                        "repeated_file: another entry names the same filesystem destination",
                    ));
                } else if left.contains(right_final) {
                    refusals.push((
                        indices[right_index],
                        "future_file_parent: another target needs this file path as a directory",
                    ));
                } else if right.contains(left_final) {
                    refusals.push((
                        indices[left_index],
                        "future_file_parent: another target needs this file path as a directory",
                    ));
                }
            }
        }
    }
    refusals.sort_unstable_by_key(|(index, _)| *index);
    refusals.dedup_by_key(|(index, _)| *index);
    refusals
}

/// Resolves only empty validated name components in the existing private temporary owner.
/// Scratch must be outside the worktree, freshly owned with mode0700, and qualified against every
/// anchor's volume/case rules. Each returned row lists native directory identities for its suffix;
/// the kernel therefore decides case and Unicode equivalence without a custom text comparator.
fn probe_future_edit_names(
    worktree: &WorktreeRef,
    destinations: &[&EditDestination],
) -> Result<Vec<Vec<EditNativeIdentity>>, &'static str> {
    let owner = super::git::snapshot::SnapshotDirectory::for_worktree(worktree)
        .map_err(|_| "preflight_unavailable: select a writable temporary directory outside the worktree for future-name comparison")?;
    let root = super::observation::open_root_directory(owner.path())
        .map_err(|_| "preflight_unavailable: cannot retain the private filename probe directory")?;
    let metadata = root
        .metadata()
        .map_err(|_| "preflight_unavailable: cannot identify filename probe ownership")?;
    // SAFETY: geteuid has no arguments, borrows no memory and performs no mutation.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err(
            "preflight_unavailable: filename probe requires a fresh private owned directory",
        );
    }
    if destinations
        .iter()
        .any(|destination| !same_edit_name_semantics(&destination.parent, &root))
    {
        return Err(
            "preflight_unavailable: temporary and destination directories must share the same volume and case semantics",
        );
    }
    let mut identities = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let mut parent = root
            .try_clone()
            .map_err(|_| "preflight_unavailable: cannot retain filename probe descriptor")?;
        let mut row = Vec::with_capacity(destination.suffix.len());
        for part in &destination.suffix {
            let name = nul_name(part);
            // SAFETY: parent is an owned private directory descriptor and name is a validated,
            // live NUL-terminated single component. Only empty scratch directories are created.
            let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr().cast(), 0o700) };
            if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(
                    "preflight_unavailable: filesystem could not compare the future filename components",
                );
            }
            let descriptor =
                super::observation::open_directory(parent.as_raw_fd(), OsStr::from_bytes(part))
                    .map_err(|_| "preflight_unavailable: filename probe lookup failed")?;
            // SAFETY: open_directory returned a new owned descriptor; File closes it exactly once.
            parent = unsafe { File::from_raw_fd(descriptor) };
            let metadata = parent
                .metadata()
                .map_err(|_| "preflight_unavailable: filename probe identity is unavailable")?;
            row.push([metadata.dev(), metadata.ino()]);
        }
        identities.push(row);
    }
    Ok(identities)
}

/// Qualifies a scratch oracle against the actual retained parent on macOS: equal device proves
/// the same volume and _PC_CASE_SENSITIVE must be known and equal. No source or temp bytes change.
#[cfg(target_os = "macos")]
fn same_edit_name_semantics(destination: &File, scratch: &File) -> bool {
    let (Ok(destination_metadata), Ok(scratch_metadata)) =
        (destination.metadata(), scratch.metadata())
    else {
        return false;
    };
    // SAFETY: both descriptors are live owned directories; fpathconf only reads their properties.
    let destination_case =
        unsafe { libc::fpathconf(destination.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
    // SAFETY: scratch remains a live owned directory for this read-only property query.
    let scratch_case = unsafe { libc::fpathconf(scratch.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
    destination_metadata.dev() == scratch_metadata.dev()
        && matches!(destination_case, 0 | 1)
        && destination_case == scratch_case
}

/// Other targets have no qualified per-directory filename-semantics query in this macOS product;
/// ambiguous future batches fail admission rather than guessing case or Unicode equivalence.
#[cfg(not(target_os = "macos"))]
fn same_edit_name_semantics(_destination: &File, _scratch: &File) -> bool {
    false
}

/// Creates missing parents with mode 0755 beneath the authorized root without following links.
/// Already-created empty parents may remain when a later component or file operation fails.
fn create_edit_parents(worktree: &WorktreeRef, path: &Path) -> Result<(), EditOutcome> {
    if !valid_edit_path(path) {
        return Err(EditOutcome::StaleSource);
    }
    let mut parent = super::observation::open_root_directory(worktree.worktree_path())
        .map_err(map_observation)?;
    if let Some(expected) = worktree.native_root_identity()
        && super::observation::native_directory_identity(&parent).map_err(map_observation)?
            != expected
    {
        return Err(EditOutcome::UnsafeTarget);
    }
    let parts: Vec<&[u8]> = path
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .collect();
    for part in &parts[..parts.len() - 1] {
        let component = OsStr::from_bytes(part);
        match super::observation::open_directory(parent.as_raw_fd(), component) {
            Ok(descriptor) => {
                // SAFETY: open_directory returned a new descriptor owned solely by this File.
                parent = unsafe { File::from_raw_fd(descriptor) };
            }
            Err(ObservationError::Missing) => {
                let name = nul_name(part);
                // SAFETY: name is NUL-terminated and parent is an owned directory descriptor.
                let created =
                    unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr().cast(), 0o755) };
                if created != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(EditOutcome::UnsafeTarget);
                }
                let descriptor = super::observation::open_directory(parent.as_raw_fd(), component)
                    .map_err(map_observation)?;
                // SAFETY: open_directory returned a new descriptor owned solely by this File.
                parent = unsafe { File::from_raw_fd(descriptor) };
            }
            Err(error) => return Err(map_observation(error)),
        }
    }
    Ok(())
}

/// Checks current W_OK/X_OK access to the retained directory as this effective user.
/// This advisory permission check does not prevent later I/O errors or filesystem races.
fn parent_is_writable(parent: &File) -> bool {
    let dot = nul_name(b".");
    // SAFETY: dot is NUL-terminated and parent is an owned directory descriptor.
    unsafe {
        libc::faccessat(
            parent.as_raw_fd(),
            dot.as_ptr().cast(),
            libc::W_OK | libc::X_OK,
            libc::AT_EACCESS,
        ) == 0
    }
}

/// Opens and retains the final parent directory plus the raw final component.
fn open_parent(worktree: &WorktreeRef, path: &Path) -> Result<(File, Vec<u8>), EditOutcome> {
    let mut parent = super::observation::open_root_directory(worktree.worktree_path())
        .map_err(map_observation)?;
    if let Some(expected) = worktree.native_root_identity()
        && super::observation::native_directory_identity(&parent).map_err(map_observation)?
            != expected
    {
        return Err(EditOutcome::UnsafeTarget);
    }
    let parts: Vec<&[u8]> = path
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .collect();
    for part in &parts[..parts.len() - 1] {
        let descriptor =
            super::observation::open_directory(parent.as_raw_fd(), OsStr::from_bytes(part))
                .map_err(map_observation)?;
        // SAFETY: `open_directory` returned a new descriptor owned solely by this `File`.
        parent = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok((parent, parts.last().expect("validated path").to_vec()))
}

/// Opens the current final component without following symlinks or blocking on special files.
fn open_final(parent: i32, name: &[u8]) -> Result<File, EditOutcome> {
    let name = nul_name(name);
    // SAFETY: `name` is live and NUL terminated; a successful descriptor is uniquely owned below.
    let descriptor = unsafe {
        libc::openat(
            parent,
            name.as_ptr().cast(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if descriptor >= 0 {
        // SAFETY: the successful open returned one new owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    } else {
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => Err(EditOutcome::StaleSource),
            _ => Err(EditOutcome::UnsafeTarget),
        }
    }
}

/// Rejects metadata that cannot be preserved conservatively before any target write.
fn safe_metadata(file: &File) -> Result<EditMetadata, EditOutcome> {
    let metadata = file.metadata().map_err(|_| EditOutcome::UnsafeTarget)?;
    let mode = metadata.mode();
    // SAFETY: `geteuid` has no preconditions and observes this process's effective user.
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || mode & 0o7000 != 0
    {
        return Err(EditOutcome::UnsafeTarget);
    }
    Ok(EditMetadata {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: mode & 0o777,
        uid: metadata.uid(),
        gid: metadata.gid(),
        extended: extended_metadata(file)?,
    })
}

/// Ordered descriptor xattr name/value bytes preserved across one safe replacement.
///
/// Names exclude their terminating NUL; values retain their complete opaque bytes. An empty vector
/// means the descriptor has no extended attributes, not that observation was unavailable.
type ExtendedMetadata = Vec<(Vec<u8>, Vec<u8>)>;

/// Reads all descriptor xattrs so replacement can preserve them or refuse before rename.
#[cfg(target_os = "macos")]
fn extended_metadata(file: &File) -> Result<ExtendedMetadata, EditOutcome> {
    // SAFETY: null buffer with zero size asks only for the required list length.
    let result = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0, 0) };
    if result < 0 {
        return Err(EditOutcome::UnsafeTarget);
    }
    let mut names = vec![0u8; result as usize];
    // SAFETY: `names` exposes exactly the writable length reported by the prior call.
    if result > 0
        && unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len(), 0) }
            != result
    {
        return Err(EditOutcome::UnsafeTarget);
    }
    read_extended_values(file, &names)
}

/// Reads all descriptor xattrs so replacement can preserve them or refuse before rename.
#[cfg(not(target_os = "macos"))]
fn extended_metadata(file: &File) -> Result<ExtendedMetadata, EditOutcome> {
    // SAFETY: null buffer with zero size asks only for the required list length.
    let result = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    if result < 0 {
        return Err(EditOutcome::UnsafeTarget);
    }
    let mut names = vec![0u8; result as usize];
    // SAFETY: `names` exposes exactly the writable length reported by the prior call.
    if result > 0
        && unsafe { libc::flistxattr(file.as_raw_fd(), names.as_mut_ptr().cast(), names.len()) }
            != result
    {
        return Err(EditOutcome::UnsafeTarget);
    }
    read_extended_values(file, &names)
}

/// Reads every NUL-separated xattr value from one already-open descriptor.
fn read_extended_values(file: &File, names: &[u8]) -> Result<ExtendedMetadata, EditOutcome> {
    let mut values = Vec::new();
    for name in names
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name_nul = nul_name(name);
        let length = get_xattr(file.as_raw_fd(), &name_nul, std::ptr::null_mut(), 0)?;
        let mut value = vec![0u8; length];
        if length > 0
            && get_xattr(
                file.as_raw_fd(),
                &name_nul,
                value.as_mut_ptr().cast(),
                value.len(),
            )? != length
        {
            return Err(EditOutcome::UnsafeTarget);
        }
        values.push((name.to_vec(), value));
    }
    Ok(values)
}

/// Calls the platform `fgetxattr` variant and returns the exact nonnegative byte count.
#[cfg(target_os = "macos")]
fn get_xattr(
    fd: i32,
    name: &[u8],
    value: *mut libc::c_void,
    length: usize,
) -> Result<usize, EditOutcome> {
    // SAFETY: `name` is NUL-terminated and `value` is null or writable for `length` bytes.
    let result = unsafe { libc::fgetxattr(fd, name.as_ptr().cast(), value, length, 0, 0) };
    usize::try_from(result).map_err(|_| EditOutcome::UnsafeTarget)
}

/// Calls the platform `fgetxattr` variant and returns the exact nonnegative byte count.
#[cfg(not(target_os = "macos"))]
fn get_xattr(
    fd: i32,
    name: &[u8],
    value: *mut libc::c_void,
    length: usize,
) -> Result<usize, EditOutcome> {
    // SAFETY: `name` is NUL-terminated and `value` is null or writable for `length` bytes.
    let result = unsafe { libc::fgetxattr(fd, name.as_ptr().cast(), value, length) };
    usize::try_from(result).map_err(|_| EditOutcome::UnsafeTarget)
}

/// Copies the resolved target's complete xattr set onto the private replacement descriptor.
fn apply_extended_metadata(file: &File, extended: &[(Vec<u8>, Vec<u8>)]) -> io::Result<()> {
    for (name, value) in extended {
        let name = nul_name(name);
        if set_xattr(file.as_raw_fd(), &name, value) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Calls the macOS `fsetxattr` variant without following any pathname.
#[cfg(target_os = "macos")]
fn set_xattr(fd: i32, name: &[u8], value: &[u8]) -> i32 {
    // SAFETY: `name` is NUL-terminated and `value` is readable for its reported length.
    unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr().cast(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    }
}

/// Calls the Unix `fsetxattr` variant without following any pathname.
#[cfg(not(target_os = "macos"))]
fn set_xattr(fd: i32, name: &[u8], value: &[u8]) -> i32 {
    // SAFETY: `name` is NUL-terminated and `value` is readable for its reported length.
    unsafe {
        libc::fsetxattr(
            fd,
            name.as_ptr().cast(),
            value.as_ptr().cast(),
            value.len(),
            0,
        )
    }
}

/// Reads a bounded descriptor from offset zero and requires valid UTF-8 content.
fn read_utf8(file: &mut File) -> Result<Vec<u8>, EditOutcome> {
    file.seek(SeekFrom::Start(0))
        .map_err(|_| EditOutcome::UnsafeTarget)?;
    let mut bytes = Vec::new();
    file.take((super::observation::MAX_SOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| EditOutcome::UnsafeTarget)?;
    if bytes.len() > super::observation::MAX_SOURCE_BYTES || std::str::from_utf8(&bytes).is_err() {
        return Err(EditOutcome::UnsafeTarget);
    }
    Ok(bytes)
}

/// Rechecks the retained descriptor, name, metadata and exact expected bytes before effect.
fn recheck_target(target: &mut CurrentEditTarget) -> Result<(), EditOutcome> {
    match (&mut target.file, &target.metadata) {
        (Some(file), Some(expected)) => {
            let current = safe_metadata(file)?;
            if &current != expected
                || !name_matches(target.parent.as_raw_fd(), &target.name, expected)
            {
                return Err(EditOutcome::StaleSource);
            }
            let bytes = read_utf8(file)?;
            (target.source_ref.bytes() == Some(&SourceBytes::from_bytes(&bytes)))
                .then_some(())
                .ok_or(EditOutcome::StaleSource)
        }
        (None, None) => match open_final(target.parent.as_raw_fd(), &target.name) {
            Err(EditOutcome::StaleSource) => Ok(()),
            _ => Err(EditOutcome::StaleSource),
        },
        _ => Err(EditOutcome::UnsafeTarget),
    }
}

/// Compares a named final component to the retained original descriptor identity without following it.
fn name_matches(parent: i32, name: &[u8], expected: &EditMetadata) -> bool {
    let name = nul_name(name);
    // SAFETY: `stat` is writable and `name` is a live NUL-terminated component.
    unsafe {
        let mut stat: libc::stat = std::mem::zeroed();
        libc::fstatat(
            parent,
            name.as_ptr().cast(),
            &mut stat,
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
            && stat.st_dev as u64 == expected.device
            && stat.st_ino as u64 == expected.inode
            && (stat.st_mode & libc::S_IFMT) == libc::S_IFREG
    }
}

/// Compares requested bytes to the retained descriptor without changing filesystem state.
fn descriptor_equals(file: &mut File, content: &[u8]) -> bool {
    read_utf8(file).is_ok_and(|bytes| bytes == content)
}

/// Distinguishes failures before rename from failures after the target may have changed.
enum WriteFailure {
    /// The target name was not affected.
    BeforeEffect,
    /// The target changed after private preparation but before confined installation.
    StaleSource,
    /// Cancellation was observed after private preparation but before any target effect.
    CancelledNoEffect,
    /// Rename may have taken effect and exact completion was lost.
    AfterPossibleEffect,
}

/// Writes, syncs and metadata-prepares one private temp, then renames it through the retained parent.
fn write_replacement(
    target: &mut CurrentEditTarget,
    content: &[u8],
    continue_before_effect: impl FnOnce() -> bool,
    after_prepare: impl FnOnce(),
) -> Result<(), WriteFailure> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut nonce))
        .map_err(|_| WriteFailure::BeforeEffect)?;
    let temp_name = format!(
        ".agent-ide-edit-{}",
        blake3::Hash::from_bytes({
            let mut full = [0u8; 32];
            full[..16].copy_from_slice(&nonce);
            full
        })
        .to_hex()
    );
    let temp = nul_name(temp_name.as_bytes());
    // SAFETY: names are live/NUL-terminated and the new descriptor is uniquely owned on success.
    let descriptor = unsafe {
        libc::openat(
            target.parent.as_raw_fd(),
            temp.as_ptr().cast(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if descriptor < 0 {
        return Err(WriteFailure::BeforeEffect);
    }
    // SAFETY: the successful open returned one new owned descriptor.
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    let before = (|| {
        file.write_all(content)?;
        if let Some(metadata) = &target.metadata {
            // SAFETY: the descriptor is owned and both metadata values came from the safe original.
            if unsafe { libc::fchown(file.as_raw_fd(), metadata.uid, metadata.gid) } != 0
                || unsafe { libc::fchmod(file.as_raw_fd(), metadata.mode as libc::mode_t) } != 0
            {
                return Err(io::Error::last_os_error());
            }
            apply_extended_metadata(&file, &metadata.extended)?;
        }
        file.sync_all()
    })();
    drop(file);
    if before.is_err() {
        unlink_temp(target.parent.as_raw_fd(), &temp);
        return Err(WriteFailure::BeforeEffect);
    }
    // Private bytes are durable but not installed. This is the last safe point to reject a
    // native write that raced preparation, immediately before the confined replacement.
    after_prepare();
    if !continue_before_effect() {
        unlink_temp(target.parent.as_raw_fd(), &temp);
        return Err(WriteFailure::CancelledNoEffect);
    }
    if recheck_target(target).is_err() {
        unlink_temp(target.parent.as_raw_fd(), &temp);
        return Err(WriteFailure::StaleSource);
    }
    let final_name = nul_name(&target.name);
    // SAFETY: both names are live/NUL-terminated and share the retained parent descriptor.
    let installed = if target.file.is_none() {
        // `linkat` is an atomic create-if-absent: a native creator racing this operation wins and
        // leaves the prepared bytes private, so no stale target can be overwritten.
        // SAFETY: both names are live/NUL-terminated beneath the same retained parent descriptor.
        let linked = unsafe {
            libc::linkat(
                target.parent.as_raw_fd(),
                temp.as_ptr().cast(),
                target.parent.as_raw_fd(),
                final_name.as_ptr().cast(),
                0,
            )
        };
        if linked == 0 {
            unlink_temp(target.parent.as_raw_fd(), &temp);
        }
        linked
    } else {
        // SAFETY: both names are live/NUL-terminated and share the retained parent descriptor.
        unsafe {
            libc::renameat(
                target.parent.as_raw_fd(),
                temp.as_ptr().cast(),
                target.parent.as_raw_fd(),
                final_name.as_ptr().cast(),
            )
        }
    };
    if installed != 0 {
        unlink_temp(target.parent.as_raw_fd(), &temp);
        return Err(WriteFailure::BeforeEffect);
    }
    target
        .parent
        .sync_all()
        .map_err(|_| WriteFailure::AfterPossibleEffect)
}

/// Removes only the uniquely named private temp after a known pre-effect failure.
fn unlink_temp(parent: i32, temp: &[u8]) {
    // SAFETY: `temp` is a live NUL-terminated single component under the retained parent.
    let _ = unsafe { libc::unlinkat(parent, temp.as_ptr().cast(), 0) };
}

/// Reopens and exactly reads the final target through the retained parent descriptor.
fn post_read(target: &CurrentEditTarget) -> Result<EditPostRead, EditOutcome> {
    let mut file = open_final(target.parent.as_raw_fd(), &target.name)?;
    let _ = safe_metadata(&file)?;
    let contents = read_utf8(&mut file)?;
    Ok(EditPostRead {
        path: target.path.clone(),
        bytes: SourceBytes::from_bytes(&contents),
        contents,
    })
}

/// Produces one NUL-terminated raw component for descriptor-relative syscalls.
fn nul_name(name: &[u8]) -> Vec<u8> {
    let mut value = name.to_vec();
    value.push(0);
    value
}

/// Maps source-resolution errors to the conservative edit outcome vocabulary.
fn map_observation(error: ObservationError) -> EditOutcome {
    match error {
        ObservationError::Missing => EditOutcome::StaleSource,
        _ => EditOutcome::UnsafeTarget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        assistance::host_binding::BindingRef,
        workspace::observation::{ObservationRef, SourceRevision},
    };
    use std::{fs, os::unix::fs::symlink, time::SystemTime};

    /// Creates one isolated real directory and returns it for automatic best-effort cleanup.
    fn temporary(tag: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "agent-ide-edit-{tag}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).expect("create temp root");
        path
    }

    /// Builds fixture authority over a real root; production callers use durable-minted authority.
    fn authority(root: &Path) -> AuthorityStamp {
        let root = fs::canonicalize(root).expect("canonical root");
        AuthorityStamp {
            worktree: WorktreeRef::from_discovery(root.clone(), root, PathBuf::from(".git"), 1)
                .expect("worktree"),
            binding: BindingRef::fixture("actor", "channel", 1),
            actor_id: "actor".into(),
            epoch: 1,
            activation_id: "activation".into(),
            owner_boot: None,
            role: crate::workspace::authority::StartRole::Writer,
        }
    }

    /// Mints a completed-context reference for present or explicitly missing test bytes.
    fn source_ref(authority: &AuthorityStamp, path: &Path, bytes: Option<&[u8]>) -> EditSourceRef {
        let observation = SourceObservation::new(
            authority.worktree().clone(),
            authority.epoch(),
            1,
            ObservationRef::new("context").expect("reference"),
            path.to_path_buf(),
            bytes.map(SourceBytes::from_bytes),
            SourceRevision::new("revision").expect("revision"),
            SourceCoverage::Complete,
            if bytes.is_some() {
                crate::workspace::observation::ObservedState::Present
            } else {
                crate::workspace::observation::ObservedState::Missing
            },
        )
        .expect("observation");
        EditSourceRef::from_observation(&observation).expect("edit source")
    }

    /// Preflight admits future directories without creating them and gives actionable reasons
    /// for invalid paths, long future names, unsafe existing parents and denied creation access.
    #[test]
    fn preflight_is_read_only_and_checks_all_future_parent_components() {
        use std::os::unix::fs::PermissionsExt;
        let root = temporary("preflight");
        let authority = authority(&root);
        let worktree = authority.worktree();
        assert_eq!(
            preflight_edit_path(worktree, Path::new("missing/deeper/new.rs")),
            Ok(())
        );
        assert!(!root.join("missing").exists());
        for invalid in ["", "../outside.rs", "nested/../outside.rs", "./new.rs"] {
            assert!(
                preflight_edit_path(worktree, Path::new(invalid))
                    .unwrap_err()
                    .starts_with("invalid_path:")
            );
        }
        let long = format!("missing/{}/new.rs", "x".repeat(256));
        assert!(
            preflight_edit_path(worktree, Path::new(&long))
                .unwrap_err()
                .starts_with("name_too_long:")
        );
        assert!(!root.join("missing").exists());
        fs::write(root.join("file.rs"), "old").expect("seed file");
        assert!(
            preflight_edit_path(worktree, Path::new("file.rs/child.rs"))
                .unwrap_err()
                .starts_with("unsafe_parent:")
        );
        symlink(root.parent().expect("outside parent"), root.join("escape")).expect("symlink");
        assert!(
            preflight_edit_path(worktree, Path::new("escape/child.rs"))
                .unwrap_err()
                .starts_with("unsafe_parent:")
        );
        fs::create_dir(root.join("locked")).expect("locked dir");
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o500))
            .expect("deny creation");
        let reason =
            preflight_edit_path(worktree, Path::new("locked/missing/child.rs")).unwrap_err();
        fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o700))
            .expect("restore permissions");
        assert!(reason.starts_with("parent_not_writable:"), "{reason}");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Native descriptor and future-name admission detects case/Unicode aliases and file/parent
    /// conflicts in both orders without worktree effects; genuinely distinct names stay admitted.
    #[cfg(target_os = "macos")]
    #[test]
    fn batch_preflight_compares_native_existing_and_future_destinations() {
        let root = temporary("native-alias");
        let authority = authority(&root);
        let worktree = authority.worktree();
        fs::write(root.join("Existing.rs"), "old").expect("existing target");
        fs::write(root.join("é.rs"), "unicode").expect("unicode target");
        let insensitive = fs::metadata(root.join("existing.rs")).is_ok();
        let normalization_insensitive = fs::metadata(root.join("e\u{301}.rs")).is_ok();
        for reversed in [false, true] {
            let mut existing = [Path::new("Existing.rs"), Path::new("existing.rs")];
            if reversed {
                existing.reverse();
            }
            let refusals = preflight_edit_paths(worktree, &existing);
            if insensitive {
                assert!(
                    refusals
                        .iter()
                        .any(|(_, reason)| reason.starts_with("repeated_file:")),
                    "{refusals:?}"
                );
            } else {
                assert!(refusals.is_empty(), "{refusals:?}");
            }
            for (pair, collides, reason) in [
                (
                    ["New/Child.rs", "new/child.rs"],
                    insensitive,
                    "repeated_file:",
                ),
                (
                    ["Future", "future/child.rs"],
                    insensitive,
                    "future_file_parent:",
                ),
                (
                    ["new-é.rs", "new-e\u{301}.rs"],
                    normalization_insensitive,
                    "repeated_file:",
                ),
                (
                    ["Parent-é", "Parent-e\u{301}/child.rs"],
                    normalization_insensitive,
                    "future_file_parent:",
                ),
            ] {
                let mut paths = pair.map(Path::new);
                if reversed {
                    paths.reverse();
                }
                let refusals = preflight_edit_paths(worktree, &paths);
                if collides {
                    assert!(
                        refusals
                            .iter()
                            .any(|(_, detail)| detail.starts_with(reason)),
                        "{pair:?}: {refusals:?}"
                    );
                } else {
                    assert!(refusals.is_empty(), "{pair:?}: {refusals:?}");
                }
                for path in pair {
                    assert!(!root.join(path).exists(), "{path}");
                }
            }
        }
        let distinct = [Path::new("one/deep/new.rs"), Path::new("two/deep/new.rs")];
        assert!(preflight_edit_paths(worktree, &distinct).is_empty());
        assert!(!root.join("one").exists());
        assert!(!root.join("two").exists());
        assert_eq!(fs::read(root.join("Existing.rs")).unwrap(), b"old");
        assert_eq!(fs::read(root.join("é.rs")).unwrap(), b"unicode");
        let directory = super::super::observation::open_root_directory(&root).unwrap();
        assert!(same_edit_name_semantics(&directory, &directory));
        fs::remove_dir_all(root).expect("cleanup owned fixture");
    }

    /// When the configured temporary parent lies inside the authority root, the existing owner
    /// refuses the unresolved group precisely before scratch, destination parents or source writes.
    /// Existing-only and single-future batches do not need that oracle and remain usable.
    #[test]
    fn batch_preflight_unavailable_oracle_has_no_worktree_effects() {
        let root = temporary("no-oracle");
        let temp = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary parent");
        let authority = authority(&temp);
        let relative = root
            .strip_prefix(&temp)
            .expect("owned child of task temporary parent");
        fs::write(root.join("present.rs"), "old").expect("existing target");
        let first = relative.join("one/new.rs");
        let second = relative.join("two/new.rs");
        let present = relative.join("present.rs");
        let refusals = preflight_edit_paths(authority.worktree(), &[&present, &first, &second]);
        assert!(
            refusals
                .iter()
                .any(|(_, reason)| reason.starts_with("preflight_unavailable:")),
            "{refusals:?}"
        );
        assert_eq!(fs::read(root.join("present.rs")).unwrap(), b"old");
        assert!(!root.join("one").exists());
        assert!(!root.join("two").exists());
        assert!(preflight_edit_paths(authority.worktree(), &[&present, &first]).is_empty());
        assert!(!root.join("one").exists());
        fs::remove_dir_all(root).expect("cleanup owned fixture");
    }

    /// A real permission change after admission causes a before-effect I/O refusal, preserving
    /// the target bytes; preflight does not promise immunity to later filesystem changes.
    #[test]
    fn io_failure_after_preflight_preserves_the_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = temporary("io-after-preflight");
        fs::create_dir(root.join("parent")).expect("parent");
        let path = PathBuf::from("parent/file.rs");
        fs::write(root.join(&path), "old").expect("seed");
        let authority = authority(&root);
        preflight_edit_path(authority.worktree(), &path).expect("initial admission");
        let source = source_ref(&authority, &path, Some(b"old"));
        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        fs::set_permissions(root.join("parent"), fs::Permissions::from_mode(0o500))
            .expect("deny write");
        let outcome = replace_if_current(
            EditPermit::new("io-failure", path.clone()).expect("permit"),
            target,
            &source,
            b"new",
            || true,
            || true,
        );
        fs::set_permissions(root.join("parent"), fs::Permissions::from_mode(0o700))
            .expect("restore permissions");
        assert_eq!(outcome, EditOutcome::UnsafeTarget);
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"old");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Proves replacement and creation return exact post-read bytes and safely create missing
    /// parents beneath the worktree without following links.
    #[test]
    fn creates_and_replaces_only_one_descriptor_confined_file() {
        let root = temporary("effects");
        fs::create_dir(root.join("src")).expect("parent");
        fs::write(root.join("src/a.rs"), "old").expect("seed");
        let authority = authority(&root);
        let path = PathBuf::from("src/a.rs");
        let source = source_ref(&authority, &path, Some(b"old"));
        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        let outcome = replace_if_current(
            EditPermit::new("replace", path.clone()).expect("permit"),
            target,
            &source,
            b"new",
            || true,
            || true,
        );
        assert!(matches!(outcome, EditOutcome::Replaced(ref read) if read.contents() == b"new"));
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"new");

        let created_path = PathBuf::from("src/new.rs");
        let missing = source_ref(&authority, &created_path, None);
        let target = CurrentEditTarget::resolve(&authority, &created_path, missing.clone())
            .expect("missing target");
        let outcome = replace_if_current(
            EditPermit::new("create", created_path.clone()).expect("permit"),
            target,
            &missing,
            b"created",
            || true,
            || true,
        );
        assert!(matches!(outcome, EditOutcome::Created(ref read) if read.contents() == b"created"));
        assert_eq!(
            fs::metadata(root.join(&created_path))
                .expect("metadata")
                .mode()
                & 0o777,
            0o600
        );

        let nested_path = PathBuf::from("absent/deeper/child.rs");
        let missing = source_ref(&authority, &nested_path, None);
        let target = CurrentEditTarget::resolve(&authority, &nested_path, missing.clone())
            .expect("missing parents created beneath worktree");
        assert!(root.join("absent/deeper").is_dir());
        let outcome = replace_if_current(
            EditPermit::new("nested-create", nested_path.clone()).expect("permit"),
            target,
            &missing,
            b"nested",
            || true,
            || true,
        );
        assert!(matches!(outcome, EditOutcome::Created(ref read) if read.contents() == b"nested"));
        assert_eq!(fs::read(root.join(&nested_path)).expect("read"), b"nested");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Proves stale bytes, cancellation, symlink swaps and hard links never alter the named target.
    #[test]
    fn races_and_unsafe_metadata_refuse_with_zero_target_writes() {
        let root = temporary("races");
        fs::write(root.join("a.rs"), "old").expect("seed");
        let authority = authority(&root);
        let path = PathBuf::from("a.rs");
        let source = source_ref(&authority, &path, Some(b"old"));
        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        fs::write(root.join(&path), "native").expect("native edit");
        assert_eq!(
            replace_if_current(
                EditPermit::new("stale", path.clone()).expect("permit"),
                target,
                &source,
                b"model",
                || true,
                || true,
            ),
            EditOutcome::StaleSource
        );
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"native");

        let source = source_ref(&authority, &path, Some(b"native"));
        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        assert_eq!(
            replace_if_current(
                EditPermit::new("cancel", path.clone()).expect("permit"),
                target,
                &source,
                b"model",
                || true,
                || false,
            ),
            EditOutcome::CancelledNoEffect
        );
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"native");

        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        fs::rename(root.join(&path), root.join("moved.rs")).expect("move");
        symlink("moved.rs", root.join(&path)).expect("symlink");
        assert_eq!(
            replace_if_current(
                EditPermit::new("swap", path.clone()).expect("permit"),
                target,
                &source,
                b"model",
                || true,
                || true,
            ),
            EditOutcome::StaleSource
        );
        assert_eq!(fs::read(root.join("moved.rs")).expect("read"), b"native");

        fs::remove_file(root.join(&path)).expect("unlink symlink");
        fs::hard_link(root.join("moved.rs"), root.join(&path)).expect("hard link");
        let linked = source_ref(&authority, &path, Some(b"native"));
        assert!(matches!(
            CurrentEditTarget::resolve(&authority, &path, linked),
            Err(EditOutcome::UnsafeTarget)
        ));
        assert_eq!(fs::read(root.join("moved.rs")).expect("read"), b"native");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Proves a creation based on an observed absence never overwrites a file that appeared since:
    /// one present at resolution and one created natively just before installation both refuse.
    #[test]
    fn creation_refuses_a_file_that_appeared_after_the_missing_observation() {
        let root = temporary("create-race");
        let authority = authority(&root);
        let path = PathBuf::from("new.rs");
        let missing = source_ref(&authority, &path, None);
        fs::write(root.join(&path), "native").expect("native create before resolve");
        assert!(matches!(
            CurrentEditTarget::resolve(&authority, &path, missing.clone()),
            Err(EditOutcome::StaleSource)
        ));
        fs::remove_file(root.join(&path)).expect("remove");

        let target =
            CurrentEditTarget::resolve(&authority, &path, missing.clone()).expect("missing target");
        assert_eq!(
            replace_if_current_with_checkpoint(
                EditPermit::new("create-race", path.clone()).expect("permit"),
                target,
                &missing,
                b"model",
                || true,
                || true,
                || fs::write(root.join(&path), "native").expect("native create"),
            ),
            EditOutcome::StaleSource
        );
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"native");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Proves a native write after temporary fsync but before installation is never overwritten.
    #[test]
    fn final_recheck_rejects_native_write_during_private_preparation() {
        let root = temporary("final-recheck");
        let path = PathBuf::from("a.rs");
        fs::write(root.join(&path), "old").expect("seed");
        let authority = authority(&root);
        let source = source_ref(&authority, &path, Some(b"old"));
        let target = CurrentEditTarget::resolve(&authority, &path, source.clone()).expect("target");
        assert_eq!(
            replace_if_current_with_checkpoint(
                EditPermit::new("race-after-fsync", path.clone()).expect("permit"),
                target,
                &source,
                b"model",
                || true,
                || true,
                || fs::write(root.join(&path), "native").expect("native write"),
            ),
            EditOutcome::StaleSource
        );
        assert_eq!(fs::read(root.join(&path)).expect("read"), b"native");
        fs::remove_dir_all(root).expect("cleanup");
    }
}
