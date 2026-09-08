//! Bounded, raw source observations rooted in one authoritative worktree.

use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read},
    os::{fd::FromRawFd, unix::ffi::OsStrExt},
    path::{Component, Path, PathBuf},
};

use super::authority::WorktreeRef;

/// The largest raw relative pathname accepted by the v0.1 source reader.
pub const MAX_SOURCE_PATH_BYTES: usize = 4096;

/// The largest source byte payload accepted by the v0.1 source reader.
pub const MAX_SOURCE_BYTES: usize = 1024 * 1024;

/// States whether an observation captures the complete requested source scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceCoverage {
    /// The requested registered path was read or observed missing without a known omission.
    Complete,
    /// A bound or caller filter omitted part of the requested source scope.
    Partial,
    /// The collector cannot establish whether its requested source scope is complete.
    Unknown,
}

impl SourceCoverage {
    /// Returns whether this coverage can participate in a current observation claim.
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Returns the stable SQLite representation owned by Workspace.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
        }
    }

    /// Decodes a persisted Workspace coverage value without treating corruption as complete.
    pub(crate) const fn from_str(value: &str) -> Self {
        match value {
            "complete" => Self::Complete,
            "partial" => Self::Partial,
            _ => Self::Unknown,
        }
    }
}

/// States whether a registered source path currently has bytes or is explicitly missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservedState {
    /// The path was read as a regular file beneath the authorized worktree root.
    Present,
    /// The registered path was absent; this is an observation and never a worktree-closure claim.
    Missing,
}

impl ObservedState {
    /// Returns the stable SQLite representation owned by Workspace.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Missing => "missing",
        }
    }

    /// Decodes a persisted Workspace state value without turning corrupt data into presence.
    pub(crate) const fn from_str(value: &str) -> Self {
        match value {
            "present" => Self::Present,
            _ => Self::Missing,
        }
    }
}

/// Identifies one source revision without conflating it with Git or task revision identities.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRevision(String);

impl SourceRevision {
    /// Validates a nonempty opaque source revision up to 128 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, ObservationError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(ObservationError::InvalidRevision);
        }
        Ok(Self(value))
    }

    /// Returns the opaque source revision without assigning it Git or baseline semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Identifies one opaque observation for later source or Intelligence correlation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationRef(String);

impl ObservationRef {
    /// Validates a nonempty opaque observation reference up to 128 UTF-8 bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, ObservationError> {
        let value = value.into();
        if value.is_empty() || value.len() > 128 {
            return Err(ObservationError::InvalidReference);
        }
        Ok(Self(value))
    }

    /// Returns the opaque reference without treating it as a filesystem path or operation ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Stores the BLAKE3 digest and byte length of a bounded present source payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceBytes {
    /// BLAKE3 digest of the exact returned byte sequence.
    digest: [u8; 32],
    /// Length of the exact returned byte sequence, bounded by the configured reader limit.
    length: u64,
}

impl SourceBytes {
    /// Builds digest metadata from already bounded source bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            digest: *blake3::hash(bytes).as_bytes(),
            length: bytes.len() as u64,
        }
    }

    /// Rebuilds trusted persisted metadata after validating its fixed digest and nonnegative length.
    pub(crate) fn from_persisted(digest: Vec<u8>, length: i64) -> Result<Self, ObservationError> {
        let digest = digest
            .try_into()
            .map_err(|_| ObservationError::CorruptPersistence)?;
        let length = u64::try_from(length).map_err(|_| ObservationError::CorruptPersistence)?;
        Ok(Self { digest, length })
    }

    /// Returns the exact digest bytes for persistence or byte-equality comparison.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }

    /// Returns the bounded source byte count.
    pub const fn length(&self) -> u64 {
        self.length
    }
}

/// Carries a complete typed source observation suitable for later semantic change application.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceObservation {
    /// Worktree identity and incarnation that scope this observation.
    worktree: WorktreeRef,
    /// Authority generation that scoped collection without claiming the authority remains live.
    authority_epoch: u64,
    /// Monotonic per-worktree source sequence allocated only by durable persistence.
    sequence: u64,
    /// Stable caller-supplied correlation reference.
    reference: ObservationRef,
    /// Raw relative Unix path beneath the authorized worktree root.
    path: PathBuf,
    /// Digest and length for present bytes, absent for a missing-path observation.
    bytes: Option<SourceBytes>,
    /// Opaque source revision distinct from Git and task revisions.
    source_revision: SourceRevision,
    /// Explicit collection completeness; partial and unknown observations are never current.
    coverage: SourceCoverage,
    /// Whether the registered path was present or explicitly missing.
    state: ObservedState,
}

impl SourceObservation {
    /// Builds an observation after persistence allocated `sequence` for a validated relative path.
    pub(crate) fn new(
        worktree: WorktreeRef,
        authority_epoch: u64,
        sequence: u64,
        reference: ObservationRef,
        path: PathBuf,
        bytes: Option<SourceBytes>,
        source_revision: SourceRevision,
        coverage: SourceCoverage,
        state: ObservedState,
    ) -> Result<Self, ObservationError> {
        if authority_epoch == 0 || sequence == 0 || !valid_relative_path(&path) {
            return Err(ObservationError::InvalidObservation);
        }
        if matches!(state, ObservedState::Present) != bytes.is_some() {
            return Err(ObservationError::InvalidObservation);
        }
        Ok(Self {
            worktree,
            authority_epoch,
            sequence,
            reference,
            path,
            bytes,
            source_revision,
            coverage,
            state,
        })
    }

    /// Returns the worktree identity and incarnation that scope this observation.
    pub fn worktree(&self) -> &WorktreeRef {
        &self.worktree
    }

    /// Returns the authority epoch that scoped collection.
    pub const fn authority_epoch(&self) -> u64 {
        self.authority_epoch
    }

    /// Returns the monotonic source sequence allocated for this worktree.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the opaque caller correlation reference.
    pub fn reference(&self) -> &ObservationRef {
        &self.reference
    }

    /// Returns the raw relative Unix path without UTF-8 conversion.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns present byte metadata, or `None` when the registered path was missing.
    pub fn bytes(&self) -> Option<&SourceBytes> {
        self.bytes.as_ref()
    }

    /// Returns the opaque source revision, distinct from all Git and task revisions.
    pub fn source_revision(&self) -> &SourceRevision {
        &self.source_revision
    }

    /// Returns explicit collection coverage.
    pub const fn coverage(&self) -> SourceCoverage {
        self.coverage
    }

    /// Returns whether this registered path was present or missing.
    pub const fn state(&self) -> ObservedState {
        self.state
    }
}

/// Describes the only typed source facts later needed to apply didOpen, didChange, or didClose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceChange {
    /// A newly registered present path supplies bytes for a later didOpen.
    Open(SourceObservation),
    /// A present path changed bytes, revision, or coverage and supplies a later didChange input.
    Change {
        /// The superseded registered observation.
        previous: SourceObservation,
        /// The newly collected observation.
        current: SourceObservation,
    },
    /// A previously present registered path is now missing and supplies a later didClose input.
    Close {
        /// The previous present observation; Workspace does not retire it or the worktree.
        previous: SourceObservation,
        /// The explicit missing-path observation.
        missing: SourceObservation,
    },
    /// An explicitly proven old/new identity move supplies a later rename-aware close/open pair.
    Rename {
        /// The old-path missing observation.
        old: SourceObservation,
        /// The new-path present observation.
        new: SourceObservation,
    },
}

/// Bounds raw Unix path and source-byte work before any digest or return value is produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceReadLimits {
    /// Maximum raw relative Unix pathname bytes, inclusive.
    max_path_bytes: usize,
    /// Maximum source payload bytes, inclusive.
    max_bytes: usize,
}

impl SourceReadLimits {
    /// Validates positive reader limits that cannot exceed the v0.1 hard caps.
    pub fn new(max_path_bytes: usize, max_bytes: usize) -> Result<Self, ObservationError> {
        if max_path_bytes == 0
            || max_bytes == 0
            || max_path_bytes > MAX_SOURCE_PATH_BYTES
            || max_bytes > MAX_SOURCE_BYTES
        {
            return Err(ObservationError::InvalidLimits);
        }
        Ok(Self {
            max_path_bytes,
            max_bytes,
        })
    }
}

/// Reports a bounded authorized read without decoding its raw Unix pathname or bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceRead {
    /// Exact validated relative Unix pathname supplied to the reader.
    path: PathBuf,
    /// Exact bounded file bytes; hashing happens only after this limit succeeds.
    contents: Vec<u8>,
    /// Digest and length for `contents`.
    bytes: SourceBytes,
}

impl SourceRead {
    /// Returns the raw relative Unix pathname.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the exact bounded source bytes.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }

    /// Returns digest and length metadata for the returned bytes.
    pub fn bytes(&self) -> &SourceBytes {
        &self.bytes
    }
}

/// Explains a rejected source observation, unsafe native path, or corrupt durable row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationError {
    /// An opaque source revision was empty or over its bounded local limit.
    InvalidRevision,
    /// An opaque observation reference was empty or over its bounded local limit.
    InvalidReference,
    /// An observation had an invalid path, state/bytes combination, or zero sequence/epoch.
    InvalidObservation,
    /// Reader limits were zero or exceeded v0.1 hard caps.
    InvalidLimits,
    /// The relative path was absolute, empty, dot/parent-containing, NUL-containing, or too long.
    InvalidPath,
    /// A root component, intermediate component, or final file was a symlink or non-directory escape.
    SymlinkEscape,
    /// The final object was not a regular file.
    NotRegularFile,
    /// The source payload exceeded its configured byte ceiling before hashing or return.
    TooLarge,
    /// The registered source path was absent; this does not claim worktree closure.
    Missing,
    /// A durable row could not be decoded into the Workspace observation contract.
    CorruptPersistence,
    /// A native filesystem operation failed without a more specific safe classification.
    Io,
}

/// Reads one validated relative regular file beneath `worktree` without following symlinks.
///
/// The caller supplies a `WorktreeRef` already authorized by Workspace and a raw relative Unix
/// path. The reader rejects empty, absolute, dot, parent, NUL, and over-limit paths; uses
/// `openat` with `O_NOFOLLOW` for every component; caps bytes before hashing; and never scans.
/// Missing paths return `ObservationError::Missing` rather than any lifecycle conclusion.
pub fn read_authorized_source(
    worktree: &WorktreeRef,
    path: &Path,
    limits: SourceReadLimits,
) -> Result<SourceRead, ObservationError> {
    if !valid_relative_path(path) || path.as_os_str().as_bytes().len() > limits.max_path_bytes {
        return Err(ObservationError::InvalidPath);
    }
    let root = open_directory(libc::AT_FDCWD, worktree.worktree_path().as_os_str())?;
    let mut directory = root;
    let components: Vec<&OsStr> = path
        .components()
        .map(|component| component.as_os_str())
        .collect();
    for component in &components[..components.len() - 1] {
        let next = open_directory(directory, component)?;
        // SAFETY: `directory` was returned by `open`/`openat` and is replaced exactly once here.
        unsafe { libc::close(directory) };
        directory = next;
    }
    let file = open_file(
        directory,
        components.last().expect("validated path has a component"),
    )?;
    // SAFETY: `directory` was returned by `open`/`openat` and is no longer needed.
    unsafe { libc::close(directory) };
    // SAFETY: `file` is an owned successful open descriptor and File takes sole ownership.
    let mut file = unsafe { File::from_raw_fd(file) };
    let metadata = file.metadata().map_err(classify_io)?;
    if !metadata.is_file() {
        return Err(ObservationError::NotRegularFile);
    }
    let mut contents = Vec::with_capacity(limits.max_bytes.min(8192));
    file.by_ref()
        .take((limits.max_bytes as u64).saturating_add(1))
        .read_to_end(&mut contents)
        .map_err(classify_io)?;
    if contents.len() > limits.max_bytes {
        return Err(ObservationError::TooLarge);
    }
    let bytes = SourceBytes::from_bytes(&contents);
    Ok(SourceRead {
        path: path.to_path_buf(),
        contents,
        bytes,
    })
}

/// Opens a directory component while refusing symlinks and preserving raw Unix bytes.
fn open_directory(parent: libc::c_int, component: &OsStr) -> Result<libc::c_int, ObservationError> {
    open_at(
        parent,
        component,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

/// Opens the final source component while refusing a final symlink.
fn open_file(parent: libc::c_int, component: &OsStr) -> Result<libc::c_int, ObservationError> {
    open_at(
        parent,
        component,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

/// Calls `openat` with a NUL-terminated raw Unix component and classifies containment failures.
fn open_at(
    parent: libc::c_int,
    component: &OsStr,
    flags: libc::c_int,
) -> Result<libc::c_int, ObservationError> {
    let mut name = component.as_bytes().to_vec();
    name.push(0);
    // SAFETY: `name` has one final NUL, is live for the call, and `parent` is an owned/open fd.
    let descriptor = unsafe { libc::openat(parent, name.as_ptr().cast(), flags) };
    if descriptor >= 0 {
        Ok(descriptor)
    } else {
        Err(classify_errno())
    }
}

/// Classifies a native open failure conservatively without exposing operating-system text as contract.
fn classify_errno() -> ObservationError {
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ELOOP) | Some(libc::ENOTDIR) => ObservationError::SymlinkEscape,
        Some(libc::ENOENT) => ObservationError::Missing,
        _ => ObservationError::Io,
    }
}

/// Classifies ordinary file I/O failures, retaining missing-path semantics for reconciliation.
fn classify_io(error: io::Error) -> ObservationError {
    if error.kind() == io::ErrorKind::NotFound {
        ObservationError::Missing
    } else {
        ObservationError::Io
    }
}

/// Validates one raw Unix relative path without normalizing or accepting lexical escape components.
pub(crate) fn valid_relative_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SOURCE_PATH_BYTES
        && !bytes.contains(&0)
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}
