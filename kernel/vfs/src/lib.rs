//! The virtual filesystem: a mount table and the traits a backend implements.
//!
//! # What the VFS is
//!
//! The VFS is the layer between AmigaDOS paths and the filesystems behind
//! them. It owns:
//!
//! * a **mount table** — a mapping from a mount point to a backend,
//! * the **node** abstraction — a file or a directory, with the metadata a
//!   program can ask about,
//! * the **handle** abstraction — an open file, identified by an opaque
//!   number rather than a pointer.
//!
//! # Why handles and not pointers
//!
//! The same reason as everywhere else in VelaOS: a user program must never
//! hold a kernel pointer. A [`FileHandle`] is a small integer the kernel
//! turns back into a file. A program that forges one names a file that
//! does not exist, and the kernel says so.
//!
//! # The node traits
//!
//! A backend implements [`FileSystem`], which is a trait object so that the
//! mount table can hold a ramfs and a disk filesystem side by side. The
//! filesystem's operations are *path-based* rather than handle-based,
//! because the path is what AmigaDOS programs think in; the handle is
//! derived from it.
//!
//! # Case
//!
//! Lookups are case-insensitive and the *display* case is preserved. That
//! is an AmigaDOS rule, and it is implemented by the backend rather than by
//! the VFS, because the VFS does not know what a name means.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use spin::Mutex;

/// The error codes the VFS reports.
///
/// These are the AmigaOS `ERROR_*` values.
pub mod errors {
    /// The object was not found.
    pub const NO_OBJECT: i32 = -2;
    /// The object already exists.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The name was invalid.
    pub const INVALID_NAME: i32 = -5;
    /// The path was not found.
    pub const PATH_NOT_FOUND: i32 = -7;
    /// The name was too long.
    pub const TOO_LARGE: i32 = -8;
    /// The object was not open.
    pub const NOT_OPEN: i32 = -9;
    /// The object exists but is not a file.
    pub const FILE_NOT_OBJECT: i32 = -17;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -11;
    /// The operation was not permitted.
    pub const NOT_READY: i32 = -12;
    /// The disk is full.
    pub const DISK_FULL: i32 = -18;
    /// The argument was out of range.
    pub const RANGE: i32 = -19;
    /// The caller is not the owner.
    pub const NOT_OWNER: i32 = -24;
}

/// The access mode a file is opened with.
///
/// The modes are AmigaOS's `MODE_*` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Read only.
    Read,
    /// Write only.
    Write,
    /// Read and write.
    ReadWrite,
    /// Read and write, positioned at the end.
    Append,
}

impl AccessMode {
    /// The AmigaOS `MODE_*` value.
    pub const fn as_flags(self) -> u32 {
        match self {
            Self::Read => 1 << 0,
            Self::Write => 1 << 1,
            Self::ReadWrite => (1 << 0) | (1 << 1),
            Self::Append => (1 << 0) | (1 << 1) | (1 << 2),
        }
    }

    /// True when the mode permits reading.
    pub const fn can_read(self) -> bool {
        self.as_flags() & (1 << 0) != 0
    }

    /// True when the mode permits writing.
    pub const fn can_write(self) -> bool {
        self.as_flags() & (1 << 1) != 0
    }
}

/// The kind of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A directory.
    Directory,
    /// A regular file.
    File,
    /// A device or a handler — `CON:`, `NIL:`, and the like.
    Device,
    /// A symbolic link.
    Link,
}

/// A node's metadata.
///
/// The AmigaOS `FileInfo` structure, minus the fields that only make sense
/// for a real Amiga's date format: VelaOS uses Unix epoch seconds, which is
/// unambiguous and convertible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// The node's name, with its original case.
    pub name: String,
    /// The kind of node.
    pub kind: NodeKind,
    /// The node's size in bytes. A directory's size is the number of
    /// entries it holds.
    pub size: u64,
    /// The modification time, in seconds since the Unix epoch.
    pub mtime: u64,
    /// The protection bits.
    ///
    /// The AmigaOS layout: the low four bits are the read/write/execute
    /// flags, and the rest are per-user bits.
    pub protection: u32,
    /// The comment, if the file has one.
    pub comment: String,
}

impl NodeInfo {
    /// A file's metadata.
    pub fn file(name: &str, size: u64) -> Self {
        Self {
            name: name.to_string(),
            kind: NodeKind::File,
            size,
            mtime: 0,
            protection: 0,
            comment: String::new(),
        }
    }

    /// A directory's metadata.
    pub fn directory(name: &str) -> Self {
        Self {
            name: name.to_string(),
            kind: NodeKind::Directory,
            size: 0,
            mtime: 0,
            protection: 0,
            comment: String::new(),
        }
    }

    /// True when the node is a directory.
    pub fn is_dir(&self) -> bool {
        self.kind == NodeKind::Directory
    }

    /// True when the node is a regular file.
    pub fn is_file(&self) -> bool {
        self.kind == NodeKind::File
    }
}

/// The result of a directory scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's name, with its original case.
    pub name: String,
    /// The kind of node.
    pub kind: NodeKind,
    /// The entry's size.
    pub size: u64,
}

/// An open file's handle.
///
/// Opaque by construction: the number means nothing outside the VFS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileHandle(pub u32);

impl FileHandle {
    /// The handle of "no file".
    pub const NONE: FileHandle = FileHandle(0);

    /// True when the handle names no file.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// A filesystem backend.
///
/// The trait is object-safe so the mount table can hold several different
/// backends at once.
pub trait FileSystem: Send + Sync {
    /// The backend's name, for diagnostics.
    fn name(&self) -> &str;

    /// Look up a node.
    ///
    /// The path is absolute within this filesystem and uses `/` as its
    /// separator. The lookup is case-insensitive; the returned name keeps
    /// its original case.
    fn lookup(&self, path: &str) -> Result<NodeInfo, i32>;

    /// List a directory.
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, i32>;

    /// Read a file's whole contents.
    fn read_all(&self, path: &str) -> Result<Vec<u8>, i32>;

    /// Read part of a file.
    ///
    /// Reading past the end returns what is there; it is not an error.
    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, i32>;

    /// Write a file's whole contents, replacing any previous contents.
    fn write_all(&self, path: &str, data: &[u8]) -> Result<(), i32>;

    /// Write part of a file, extending it if necessary.
    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), i32>;

    /// Create a directory.
    fn create_dir(&self, path: &str) -> Result<(), i32>;

    /// Remove a node.
    fn remove(&self, path: &str) -> Result<(), i32>;

    /// Rename a node.
    fn rename(&self, from: &str, to: &str) -> Result<(), i32>;

    /// Set a node's comment.
    fn set_comment(&self, path: &str, comment: &str) -> Result<(), i32>;

    /// The filesystem's total size in bytes, or `None` when it is unknown.
    fn total_size(&self) -> Option<u64> {
        None
    }

    /// The filesystem's free size in bytes, or `None` when it is unknown.
    fn free_size(&self) -> Option<u64> {
        None
    }
}

/// A mounted filesystem.
#[derive(Clone)]
pub struct Mount {
    /// The mount point, as an absolute path.
    pub point: String,
    /// The name of the volume at the mount point.
    ///
    /// This is the AmigaDOS volume name: `SYS`, `RAM`, and so on.
    pub volume: String,
    /// The backend.
    pub fs: alloc::sync::Arc<dyn FileSystem>,
}

impl core::fmt::Debug for Mount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mount")
            .field("point", &self.point)
            .field("volume", &self.volume)
            .field("fs", &self.fs.name())
            .finish()
    }
}

/// The mount table.
///
/// Mount points are absolute paths with `/` separators, and a mount point is
/// a *prefix* of the paths it covers: `/System` covers `/System/Libs/foo`.
/// The longest matching mount point wins, so a filesystem can be mounted
/// inside another one.
#[derive(Default)]
pub struct MountTable {
    /// The mounts, in mount order.
    mounts: Mutex<Vec<Mount>>,
}

impl core::fmt::Debug for MountTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mounts = self.mounts.lock();
        f.debug_struct("MountTable")
            .field(
                "mounts",
                &mounts
                    .iter()
                    .map(|m| (m.volume.as_str(), m.point.as_str()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl MountTable {
    /// An empty mount table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount a filesystem.
    ///
    /// Returns `false` when something is already mounted at that point, or
    /// at a point that is *inside* an existing mount. Mounting inside an
    /// existing mount would shadow part of it, and the resolution rule —
    /// "the longest matching mount point wins" — would then depend on the
    /// order of the mounts rather than on the filesystem's shape.
    pub fn mount(&self, point: &str, volume: &str, fs: alloc::sync::Arc<dyn FileSystem>) -> bool {
        let point = normalise_mount_point(point);
        let mut mounts = self.mounts.lock();
        if mounts.iter().any(|m| m.point == point) {
            return false;
        }
        // A new mount must not be inside an existing one.
        for m in mounts.iter() {
            if point.starts_with(&m.point) && point.len() > m.point.len() {
                return false;
            }
        }
        mounts.push(Mount {
            point,
            volume: volume.to_string(),
            fs,
        });
        true
    }

    /// Unmount a filesystem.
    pub fn unmount(&self, point: &str) -> bool {
        let point = normalise_mount_point(point);
        let mut mounts = self.mounts.lock();
        let Some(index) = mounts.iter().position(|m| m.point == point) else {
            return false;
        };
        mounts.remove(index);
        true
    }

    /// How many filesystems are mounted.
    pub fn len(&self) -> usize {
        self.mounts.lock().len()
    }

    /// True when nothing is mounted.
    pub fn is_empty(&self) -> bool {
        self.mounts.lock().is_empty()
    }

    /// The volume names, in mount order.
    pub fn volumes(&self) -> Vec<String> {
        self.mounts
            .lock()
            .iter()
            .map(|m| m.volume.clone())
            .collect()
    }

    /// The mount points, in mount order.
    pub fn points(&self) -> Vec<String> {
        self.mounts.lock().iter().map(|m| m.point.clone()).collect()
    }

    /// Find the mount that covers `path`.
    ///
    /// The longest matching mount point wins.
    pub fn find(&self, path: &str) -> Option<Mount> {
        let mounts = self.mounts.lock();
        let mut best: Option<&Mount> = None;
        for m in mounts.iter() {
            if !path_covers(&m.point, path) {
                continue;
            }
            if best.map(|b| m.point.len() > b.point.len()).unwrap_or(true) {
                best = Some(m);
            }
        }
        best.cloned()
    }

    /// Split a path into the mount that covers it and the path *within* that
    /// mount.
    pub fn split(&self, path: &str) -> Result<(Mount, String), i32> {
        let mount = self.find(path).ok_or(errors::PATH_NOT_FOUND)?;
        let relative = &path[mount.point.len()..];
        let relative = if relative.is_empty() { "/" } else { relative };
        Ok((mount, relative.to_string()))
    }
}

/// Normalise a mount point: an absolute path with no trailing slash.
fn normalise_mount_point(point: &str) -> String {
    let trimmed = point.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// True when `mount_point` covers `path`.
///
/// The mount point covers the path when they are equal, or when the path
/// continues with a `/` after the mount point. The second condition is what
/// stops `/System` from covering `/Systematic`.
pub fn path_covers(mount_point: &str, path: &str) -> bool {
    if mount_point == "/" {
        return path.starts_with('/');
    }
    if !path.starts_with(mount_point) {
        return false;
    }
    let rest = &path[mount_point.len()..];
    rest.is_empty() || rest.starts_with('/')
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A filesystem that knows about three files.
    struct Fake {
        files: Vec<String>,
    }

    impl Fake {
        fn new(files: &[&str]) -> Self {
            Self {
                files: files.iter().map(|f| f.to_string()).collect(),
            }
        }
    }

    impl FileSystem for Fake {
        fn name(&self) -> &str {
            "fake"
        }

        fn lookup(&self, path: &str) -> Result<NodeInfo, i32> {
            let name = path.rsplit('/').next().unwrap_or(path);
            if self.files.iter().any(|f| f == path) {
                Ok(NodeInfo::file(name, 0))
            } else {
                Err(errors::PATH_NOT_FOUND)
            }
        }

        fn read_dir(&self, _path: &str) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }

        fn read_all(&self, path: &str) -> Result<Vec<u8>, i32> {
            if self.files.iter().any(|f| f == path) {
                Ok(b"hello".to_vec())
            } else {
                Err(errors::PATH_NOT_FOUND)
            }
        }

        fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, i32> {
            let all = self.read_all(path)?;
            let start = (offset as usize).min(all.len());
            let end = (start + len).min(all.len());
            Ok(all[start..end].to_vec())
        }

        fn write_all(&self, _path: &str, _data: &[u8]) -> Result<(), i32> {
            Ok(())
        }

        fn write_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<(), i32> {
            Ok(())
        }

        fn create_dir(&self, _path: &str) -> Result<(), i32> {
            Ok(())
        }

        fn remove(&self, _path: &str) -> Result<(), i32> {
            Ok(())
        }

        fn rename(&self, _from: &str, _to: &str) -> Result<(), i32> {
            Ok(())
        }

        fn set_comment(&self, _path: &str, _comment: &str) -> Result<(), i32> {
            Ok(())
        }
    }

    #[test]
    fn an_empty_table_has_no_mounts() {
        let t = MountTable::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn a_filesystem_can_be_mounted() {
        let t = MountTable::new();
        assert!(t.mount("/System", "SYS", Arc::new(Fake::new(&[]))));
        assert_eq!(t.len(), 1);
        assert_eq!(t.volumes(), vec!["SYS".to_string()]);
        assert_eq!(t.points(), vec!["/System".to_string()]);
    }

    #[test]
    fn mounting_the_same_point_twice_is_refused() {
        let t = MountTable::new();
        assert!(t.mount("/System", "SYS", Arc::new(Fake::new(&[]))));
        assert!(!t.mount("/System", "OTHER", Arc::new(Fake::new(&[]))));
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn mounting_inside_an_existing_mount_is_refused() {
        let t = MountTable::new();
        assert!(t.mount("/System", "SYS", Arc::new(Fake::new(&[]))));
        // A mount inside another mount would shadow part of it.
        assert!(!t.mount("/System/Libs", "LIBS", Arc::new(Fake::new(&[]))));
    }

    #[test]
    fn a_mount_can_be_removed() {
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        assert!(t.unmount("/System"));
        assert!(t.is_empty());
        assert!(!t.unmount("/System"));
    }

    #[test]
    fn a_mount_covers_its_own_point_and_its_children() {
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        assert!(t.find("/System").is_some());
        assert!(t.find("/System/Libs/foo").is_some());
    }

    #[test]
    fn a_mount_does_not_cover_a_sibling_with_a_shared_prefix() {
        // `/System` must not cover `/Systematic`, or the resolution would
        // pick the wrong filesystem for an unrelated path.
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        assert!(t.find("/Systematic").is_none());
    }

    #[test]
    fn the_longest_matching_mount_wins() {
        let t = MountTable::new();
        // Mount the child first, so the order cannot be what decides.
        t.mount("/System/Libs", "LIBS", Arc::new(Fake::new(&[])));
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        let found = t.find("/System/Libs/foo").unwrap();
        assert_eq!(found.volume, "LIBS");
        let found = t.find("/System/Other/foo").unwrap();
        assert_eq!(found.volume, "SYS");
    }

    #[test]
    fn splitting_a_path_gives_the_mount_and_the_relative_path() {
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        let (mount, relative) = t.split("/System/Libs/foo").unwrap();
        assert_eq!(mount.volume, "SYS");
        assert_eq!(relative, "/Libs/foo");
    }

    #[test]
    fn splitting_the_mount_point_itself_gives_the_root() {
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        let (_, relative) = t.split("/System").unwrap();
        assert_eq!(relative, "/");
    }

    #[test]
    fn splitting_an_unmounted_path_is_refused() {
        let t = MountTable::new();
        t.mount("/System", "SYS", Arc::new(Fake::new(&[])));
        assert!(t.split("/elsewhere/x").is_err());
    }

    #[test]
    fn the_access_modes_carry_the_amiga_flags() {
        assert!(AccessMode::Read.can_read());
        assert!(!AccessMode::Read.can_write());
        assert!(AccessMode::Write.can_write());
        assert!(!AccessMode::Write.can_read());
        assert!(AccessMode::ReadWrite.can_read());
        assert!(AccessMode::ReadWrite.can_write());
        assert!(AccessMode::Append.can_read());
        assert!(AccessMode::Append.can_write());
        // The flags are the AmigaOS `MODE_*` values.
        assert_eq!(AccessMode::Read.as_flags(), 1);
        assert_eq!(AccessMode::ReadWrite.as_flags(), 3);
    }

    #[test]
    fn a_node_reports_its_kind() {
        let f = NodeInfo::file("a.txt", 10);
        assert!(f.is_file());
        assert!(!f.is_dir());
        let d = NodeInfo::directory("d");
        assert!(d.is_dir());
        assert!(!d.is_file());
    }

    #[test]
    fn the_coverage_rule_is_exact() {
        assert!(path_covers("/a", "/a"));
        assert!(path_covers("/a", "/a/b"));
        assert!(!path_covers("/a", "/ab"));
        assert!(path_covers("/", "/anything"));
    }

    #[test]
    fn a_handle_reserves_zero_for_none() {
        assert!(FileHandle::NONE.is_none());
        assert!(!FileHandle(1).is_none());
    }
}
