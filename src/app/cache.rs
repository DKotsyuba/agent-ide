//! Private cache directory mechanics whose lifecycle decisions remain with peer domains.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::{AppError, effective_uid};

const MAX_NAMESPACE_BYTES: usize = 128;

/// Owns a private cache root used only to create and retire bounded opaque namespaces.
#[derive(Debug)]
pub struct CacheRoot {
    root: PathBuf,
}

impl CacheRoot {
    /// Creates or validates `path` as a private real cache root without deciding any cache policy.
    pub fn prepare(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let root = path.as_ref();
        match fs::symlink_metadata(root) {
            Ok(metadata) => validate_private_directory(root, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.mode(0o700).create(root)?;
                validate_private_directory(root, &fs::symlink_metadata(root)?)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    /// Creates or reopens one private opaque namespace supplied by the owning peer domain.
    pub fn retain(&self, namespace: CacheNamespaceId) -> Result<CacheNamespace, AppError> {
        let path = self.root.join(namespace.as_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) => validate_private_directory(&path, &metadata)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut builder = DirBuilder::new();
                builder.mode(0o700).create(&path)?;
                validate_private_directory(&path, &fs::symlink_metadata(&path)?)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(CacheNamespace { path })
    }
}

/// Represents a bounded opaque namespace chosen from a peer's canonical identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheNamespaceId(String);

impl CacheNamespaceId {
    /// Accepts an opaque ASCII directory component of at most 128 bytes and rejects path syntax.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()
            && value.len() <= MAX_NAMESPACE_BYTES
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')))
        .then_some(Self(value))
    }

    /// Returns the opaque component without assigning it worktree or provider semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Captures an explicit peer-supplied verified reason that permits cache retirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedCacheRetirement {
    /// The peer verified the canonical worktree incarnation has closed.
    Closed,
    /// The peer verified an explicit cache reset for the canonical worktree incarnation.
    Reset,
}

/// Represents one retained private cache namespace and exposes no compatibility interpretation.
#[derive(Debug)]
pub struct CacheNamespace {
    path: PathBuf,
}

impl CacheNamespace {
    /// Returns this namespace's private directory for peer-owned cache contents.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Retires this namespace only after a peer supplies a verified closure or reset fact.
    ///
    /// The fact is intentionally required at the call boundary: stop, handoff, a missing path,
    /// provider incompatibility, and Application failures are not retirement evidence. The
    /// namespace is borrowed so a caller retains its lifecycle handle and can retry after a
    /// temporary filesystem validation or removal failure.
    pub fn retire(&self, _verified: VerifiedCacheRetirement) -> Result<(), AppError> {
        let metadata = fs::symlink_metadata(&self.path)?;
        validate_private_directory(&self.path, &metadata)?;
        fs::remove_dir_all(&self.path)?;
        Ok(())
    }
}

/// Rejects cache roots and namespaces that are not private real directories owned by this user.
fn validate_private_directory(path: &Path, metadata: &fs::Metadata) -> Result<(), AppError> {
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != effective_uid()
        || metadata.permissions().mode() & 0o077 != 0
        || path.parent().is_none()
    {
        return Err(AppError::UnsafeRuntimeDirectory);
    }
    Ok(())
}
