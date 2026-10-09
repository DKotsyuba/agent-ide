//! Sharing-aware storage accounting for one cache family.
//!
//! A cache tree is not what it costs: sibling worktree caches are APFS copy-on-write clones of one
//! another and a cargo target holds hard links, so summing `st_blocks` per entry charges the same
//! extents again for every clone and link. Three numbers describe a family honestly:
//!
//! - **logical**: the sum of file lengths, every link and clone counted;
//! - **charged**: allocation with each hard-linked inode and each perfect-clone stream counted
//!   once. This is what the budget bounds. It is an upper bound of the physical footprint
//!   (partially shared extents stay overcharged) and falls back to per-inode allocation where the
//!   clone attributes are unavailable;
//! - **private**: an estimate of what the volume gets back at once when everything is removed (APFS
//!   private size per inode). It explains reclaim; it never replaces the charge.
//!
//! Removing one entry only frees the groups no other entry still holds, so the family charge is
//! recomputed from the surviving holders after every removal, never decremented by the removed
//! entry's own size.

use std::collections::{HashMap, HashSet};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// What a regular file's data allocation is shared by.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum GroupKey {
    /// A perfect-clone stream: device and APFS clone id; hard links of one inode agree on it.
    Clone(u64, u64),
    /// One inode, when no clone id is available.
    Inode(u64, u64),
}

/// One regular file whose allocation may be shared with another entry.
#[derive(Clone, Copy, Debug)]
pub(super) struct FileRec {
    /// The group charged once.
    key: GroupKey,
    /// Device and inode, so private bytes are summed once per inode.
    inode: (u64, u64),
    /// Allocated bytes of the inode.
    alloc: u64,
    /// APFS private bytes of the inode, when the volume reports them.
    private: Option<u64>,
}

/// The sharing-relevant measurement of one entry's tree.
#[derive(Clone, Debug, Default)]
pub(super) struct Usage {
    /// Sum of file lengths.
    pub(super) logical: u64,
    /// Allocation that cannot be shared: directories, symlinks and single-link files without
    /// clone attributes.
    pub(super) own: u64,
    /// Files that may share allocation with files of other entries.
    pub(super) files: Vec<FileRec>,
}

impl Usage {
    /// Adds one directory or symlink.
    pub(super) fn add_other(&mut self, metadata: &Metadata) {
        self.own = self
            .own
            .saturating_add(metadata.blocks().saturating_mul(512));
    }

    /// Adds one regular file, asking the volume for its clone stream and private size.
    pub(super) fn add_file(&mut self, path: &Path, metadata: &Metadata) {
        self.logical = self.logical.saturating_add(metadata.len());
        let alloc = metadata.blocks().saturating_mul(512);
        let (dev, ino) = (metadata.dev(), metadata.ino());
        let attributes = if alloc > 0 {
            clone_attributes(path)
        } else {
            None
        };
        if attributes.is_none() && metadata.nlink() <= 1 {
            self.own = self.own.saturating_add(alloc);
            return;
        }
        self.files.push(FileRec {
            key: attributes.map_or(GroupKey::Inode(dev, ino), |(id, _)| {
                GroupKey::Clone(dev, id)
            }),
            inode: (dev, ino),
            alloc,
            private: attributes.map(|(_, private)| private),
        });
    }

    /// Allocation of this tree alone, as if no other entry existed.
    pub(super) fn standalone(&self) -> u64 {
        self.own.saturating_add(self.groups().values().sum())
    }

    /// Distinct groups of this tree with the largest allocation seen in each.
    fn groups(&self) -> HashMap<GroupKey, u64> {
        let mut groups = HashMap::new();
        for file in &self.files {
            let alloc = groups.entry(file.key).or_insert(0);
            *alloc = file.alloc.max(*alloc);
        }
        groups
    }
}

/// Reads the APFS clone id and private size of a regular file without following a symlink.
#[cfg(target_os = "macos")]
fn clone_attributes(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    /// `ATTR_CMN_RETURNED_ATTRS`: the reply starts with the set of attributes actually returned.
    const RETURNED_ATTRS: u32 = 0x8000_0000;
    /// `ATTR_CMNEXT_PRIVATESIZE`.
    const PRIVATE_SIZE: u32 = 0x8;
    /// `ATTR_CMNEXT_CLONEID`.
    const CLONE_ID: u32 = 0x100;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `attrlist` is plain data; every field the call reads is set below.
    let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
    request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    request.commonattr = RETURNED_ATTRS;
    request.forkattr = PRIVATE_SIZE | CLONE_ID;
    let mut reply = [0u8; 64];
    // SAFETY: `path` is NUL-terminated, `request` and `reply` outlive the call and the length passed
    // is `reply`'s.
    let status = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            std::ptr::from_mut(&mut request).cast(),
            reply.as_mut_ptr().cast(),
            reply.len(),
            libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_PACK_INVAL_ATTRS | libc::FSOPT_NOFOLLOW,
        )
    };
    let word = |at: usize| u32::from_ne_bytes(reply[at..at + 4].try_into().expect("four bytes"));
    // Layout: u32 length, five u32 returned bitmaps (fork bitmap last), then the values in
    // attribute-bit order: private size (i64), clone id (u64).
    if status != 0
        || word(0) < 40
        || word(20) & (PRIVATE_SIZE | CLONE_ID) != PRIVATE_SIZE | CLONE_ID
    {
        return None;
    }
    let private = i64::from_ne_bytes(reply[24..32].try_into().expect("eight bytes"));
    let clone = u64::from_ne_bytes(reply[32..40].try_into().expect("eight bytes"));
    Some((clone, u64::try_from(private).ok()?))
}

/// Without APFS clone attributes only hard links are recognized.
#[cfg(not(target_os = "macos"))]
fn clone_attributes(_path: &Path) -> Option<(u64, u64)> {
    None
}

/// The family-wide view: which groups are held by how many surviving entries.
#[derive(Debug, Default)]
pub(super) struct Sharing {
    /// Charge and number of entries still holding each group.
    groups: HashMap<GroupKey, (u64, u32)>,
    /// Unshareable allocation of the surviving entries.
    own: u64,
    /// Charged bytes of the survivors.
    charged: u64,
    /// Sum of file lengths of every entry given to [`Sharing::new`].
    logical: u64,
    /// Private bytes estimate of every entry given to [`Sharing::new`].
    private: u64,
}

impl Sharing {
    /// Builds the view of a family from the measurements of all its entries.
    pub(super) fn new<'a>(usages: impl Iterator<Item = &'a Usage>) -> Self {
        let mut view = Self::default();
        let mut inodes: HashMap<(u64, u64), u64> = HashMap::new();
        for usage in usages {
            view.own = view.own.saturating_add(usage.own);
            view.logical = view.logical.saturating_add(usage.logical);
            view.private = view.private.saturating_add(usage.own);
            for (key, alloc) in usage.groups() {
                let group = view.groups.entry(key).or_insert((0, 0));
                group.0 = group.0.max(alloc);
                group.1 += 1;
            }
            for file in &usage.files {
                inodes.insert(file.inode, file.private.unwrap_or(0));
            }
        }
        view.private = view.private.saturating_add(
            inodes
                .values()
                .fold(0, |sum, bytes| sum.saturating_add(*bytes)),
        );
        view.charged = view
            .groups
            .values()
            .fold(view.own, |sum, (charge, _)| sum.saturating_add(*charge));
        view
    }

    /// Charged bytes of the entries not yet released.
    pub(super) const fn charged(&self) -> u64 {
        self.charged
    }

    /// Sum of file lengths at construction.
    pub(super) const fn logical(&self) -> u64 {
        self.logical
    }

    /// Private bytes estimate at construction.
    pub(super) const fn private(&self) -> u64 {
        self.private
    }

    /// Bytes of charge that disappear if `usage` alone is removed now.
    pub(super) fn freed(&self, usage: &Usage) -> u64 {
        let exclusive = usage
            .groups()
            .keys()
            .filter_map(|key| self.groups.get(key))
            .filter(|(_, holders)| *holders == 1)
            .fold(0u64, |sum, (charge, _)| sum.saturating_add(*charge));
        usage.own.saturating_add(exclusive)
    }

    /// Adds `usage`, a new measurement of an entry already given to [`Sharing::new`].
    pub(super) fn add(&mut self, usage: &Usage) {
        self.own = self.own.saturating_add(usage.own);
        self.charged = self.charged.saturating_add(usage.own);
        for (key, alloc) in usage.groups() {
            let group = self.groups.entry(key).or_insert((0, 0));
            if group.1 == 0 {
                self.charged = self.charged.saturating_add(alloc);
                group.0 = alloc;
            } else if alloc > group.0 {
                self.charged = self.charged.saturating_add(alloc - group.0);
                group.0 = alloc;
            }
            group.1 += 1;
        }
    }

    /// Removes `usage` from the survivors and returns the charge that disappeared with it.
    pub(super) fn release(&mut self, usage: &Usage) -> u64 {
        let mut freed = usage.own;
        self.own = self.own.saturating_sub(usage.own);
        for key in usage.groups().into_keys().collect::<HashSet<_>>() {
            let Some(group) = self.groups.get_mut(&key) else {
                continue;
            };
            group.1 = group.1.saturating_sub(1);
            if group.1 == 0 {
                freed = freed.saturating_add(group.0);
                self.groups.remove(&key);
            }
        }
        self.charged = self.charged.saturating_sub(freed);
        freed
    }
}
