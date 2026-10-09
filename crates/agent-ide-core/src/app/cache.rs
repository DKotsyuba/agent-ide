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

    /// Returns whether this namespace already exists under the root, without creating anything.
    ///
    /// Callers use it to distinguish a namespace they are about to create from one that was
    /// already retained, so a failed operation can roll back only its own new directories.
    pub fn contains(&self, namespace: &CacheNamespaceId) -> bool {
        fs::symlink_metadata(self.root.join(namespace.as_str())).is_ok()
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

/// Carries an unforgeable peer-minted fact that permits one cache retirement attempt.
///
/// The single field is private and the constructor is crate-internal, so no caller outside this
/// crate can name a variant and thereby claim retirement authority it never proved. Application
/// deliberately learns nothing about *which* lifecycle fact was verified: deciding that a closure
/// is real, exact, and complete stays with the owning peer domain, and Application only refuses to
/// delete anything without the token. The token is neither `Copy` nor `Clone`, so each retirement
/// attempt consumes a freshly minted fact instead of replaying an old one.
#[derive(Debug)]
pub struct VerifiedCacheRetirement(());

impl VerifiedCacheRetirement {
    /// Mints the token for a peer domain that has already verified an exact lifecycle closure.
    ///
    /// Callers must have matched the closure against the canonical worktree incarnation that owns
    /// the namespace and must have completed admission revocation and provider quiescence first;
    /// this constructor performs no check of its own and grants no policy authority.
    pub(crate) const fn verified() -> Self {
        Self(())
    }
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

    /// Retires this namespace only after a peer mints an unforgeable verified-closure token.
    ///
    /// The fact is intentionally required at the call boundary: stop, handoff, a missing path,
    /// provider incompatibility, and Application failures are not retirement evidence. Because the
    /// token cannot be constructed outside this crate, an untrusted caller has no reset or closure
    /// spelling that reaches this deletion. The namespace is borrowed so a caller retains its
    /// lifecycle handle and can retry after a temporary filesystem validation or removal failure.
    pub fn retire(&self, _verified: VerifiedCacheRetirement) -> Result<(), AppError> {
        let metadata = fs::symlink_metadata(&self.path)?;
        validate_private_directory(&self.path, &metadata)?;
        fs::remove_dir_all(&self.path)?;
        Ok(())
    }
}

/// Removes one private directory only while it is still empty, reporting whether it is now gone.
///
/// This is deliberately not a retirement: it carries no verified-closure fact and therefore may
/// only be used on a directory the caller itself just created. `remove_dir` refuses a non-empty
/// directory, so any content another owner wrote concurrently stops the removal instead of being
/// destroyed, and the same private-directory validation as every other path in this module rejects
/// a symlinked or foreign-owned target. An already-absent directory reports success.
pub fn discard_empty_namespace_directory(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_private_directory(path, &metadata).is_ok() && fs::remove_dir(path).is_ok()
        }
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

/// Rejects cache roots and namespaces that are not private real directories owned by this user.
pub(crate) fn validate_private_directory(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), AppError> {
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
