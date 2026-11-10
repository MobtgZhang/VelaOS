//! The FAT volume as the kernel's VFS sees it.
//!
//! # Why this file exists
//!
//! The FAT reader in this crate has always been a *reader*: it understands
//! the on-disk layout and hands back files and directories. It was never
//! connected to the kernel's filesystem interface, so a FAT volume could
//! not be mounted and the code it contained could not run.
//!
//! This module is the adapter. It presents a mounted FAT volume through
//! [`FileSystem`], the interface the kernel's DOS layer mounts and reads,
//! so the volume behaves like any other mount: paths are `/`-separated,
//! lookups are case-insensitive and case-preserving, and a directory lists
//! its entries.
//!
//! # What it does not do
//!
//! It does not write. The FAT reader has no write path at all — it can
//! read a volume and nothing else — so the mutating operations of the
//! interface report [`errors::READ_ONLY`] rather than pretending to work.
//! A read-only mount is an honest thing to have: it is what a CD-ROM is,
//! and it is what a FAT volume on a write-protected medium is.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vela_kernel_vfs::{errors, DirEntry, FileSystem, NodeInfo, NodeKind};

use crate::{BlockDevice, FatFs};

/// A mounted FAT volume.
///
/// The volume is mounted once, when it is created, and the boot sector is
/// read then: a volume whose boot sector is not a FAT boot sector is not a
/// volume at all, and finding that out at mount time is much cheaper than
/// finding it out on the first directory read.
#[derive(Debug)]
pub struct FatVolume<B: BlockDevice> {
    /// The reader.
    fs: FatFs<B>,
}

impl<B: BlockDevice> FatVolume<B> {
    /// Mount a FAT volume from a block device.
    ///
    /// # Errors
    ///
    /// Returns [`crate::errors::NOT_FAT`] when the device does not hold a
    /// FAT filesystem, and [`crate::errors::IO_ERROR`] when the boot sector
    /// cannot be read.
    pub fn mount(device: B) -> Result<Self, i32> {
        Ok(Self {
            fs: FatFs::mount(device)?,
        })
    }

    /// The mounted reader, for the operations the interface does not cover.
    pub fn reader(&self) -> &FatFs<B> {
        &self.fs
    }

    /// The FAT variant this volume turned out to be.
    pub fn variant(&self) -> crate::FatVariant {
        self.fs.variant()
    }

    /// Read a file's whole contents.
    fn read_whole(&self, path: &str) -> Result<Vec<u8>, i32> {
        let mut file = self.fs.open(path).map_err(map_error)?;
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                // The end of the file is the end of the read, not a failure.
                Err(e) if e == crate::errors::EOF => break,
                Err(e) => return Err(map_error(e)),
            }
        }
        Ok(out)
    }
}

/// Translate a FAT error into the VFS's error set.
///
/// The two sets overlap where it matters — "no such file" is "no such file"
/// in both — but the VFS spells a missing path differently from a missing
/// object, and the DOS layer reports the VFS's spelling.
fn map_error(error: i32) -> i32 {
    match error {
        crate::errors::NO_OBJECT => errors::PATH_NOT_FOUND,
        crate::errors::IO_ERROR => errors::NOT_READY,
        crate::errors::NOT_SUPPORTED => errors::READ_ONLY,
        other => other,
    }
}

/// The name a directory entry carries, in the AmigaDOS spelling.
///
/// A FAT entry holds an eight-character name and a three-character
/// extension; the name a program sees is the two joined by a dot, with the
/// extension's padding removed. An entry with no extension is just the
/// name.
fn entry_name(name: &[u8; 8], ext: &[u8; 3]) -> String {
    let base = trim_padding(name);
    let extension = trim_padding(ext);
    if extension.is_empty() {
        base.to_string()
    } else {
        let mut out = String::with_capacity(base.len() + extension.len() + 1);
        out.push_str(base);
        out.push('.');
        out.push_str(extension);
        out
    }
}

/// The name without its space padding.
fn trim_padding(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|b| *b == b' ').unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("")
}

/// The kind a FAT attribute byte names.
fn entry_kind(attributes: u8) -> NodeKind {
    if attributes & 0x10 != 0 {
        NodeKind::Directory
    } else {
        NodeKind::File
    }
}

impl<B: BlockDevice + Send + Sync> FileSystem for FatVolume<B> {
    /// The backend's name, for diagnostics.
    fn name(&self) -> &str {
        "fat"
    }

    /// Look up a node.
    ///
    /// # Errors
    ///
    /// Returns [`errors::PATH_NOT_FOUND`] when nothing matches.
    fn lookup(&self, path: &str) -> Result<NodeInfo, i32> {
        let file = self.fs.open(path).map_err(map_error)?;
        let name = path
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or("")
            .to_string();
        Ok(NodeInfo {
            name,
            kind: entry_kind(file.attributes()),
            size: file.len(),
            // FAT records a creation and a modification date in DOS format,
            // and this reader does not decode them yet: reporting zero is
            // honest, and a wrong date is worse than none.
            mtime: 0,
            protection: 0,
            comment: String::new(),
        })
    }

    /// List a directory.
    ///
    /// # Errors
    ///
    /// Returns [`errors::PATH_NOT_FOUND`] when the path names nothing, and
    /// [`errors::FILE_NOT_OBJECT`] when it names a file rather than a
    /// directory.
    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let file = self.fs.open(path).map_err(map_error)?;
        let entries = file.read_dir().map_err(map_error)?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry_name(&entry.name, &entry.ext),
                kind: entry_kind(entry.attributes),
                size: entry.file_size as u64,
            })
            .collect())
    }

    /// Read a file's whole contents.
    fn read_all(&self, path: &str) -> Result<Vec<u8>, i32> {
        self.read_whole(path)
    }

    /// Read part of a file.
    ///
    /// Reading past the end returns what is there; it is not an error.
    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, i32> {
        let data = self.read_whole(path)?;
        let start = (offset as usize).min(data.len());
        let end = (start + len).min(data.len());
        Ok(data[start..end].to_vec())
    }

    /// Write a file's contents — refused: the volume is read-only.
    fn write_all(&self, _path: &str, _data: &[u8]) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// Write part of a file — refused: the volume is read-only.
    fn write_at(&self, _path: &str, _offset: u64, _data: &[u8]) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// Create a directory — refused: the volume is read-only.
    fn create_dir(&self, _path: &str) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// Remove a node — refused: the volume is read-only.
    fn remove(&self, _path: &str) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// Rename a node — refused: the volume is read-only.
    fn rename(&self, _from: &str, _to: &str) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// Set a node's comment — refused: the volume is read-only.
    fn set_comment(&self, _path: &str, _comment: &str) -> Result<(), i32> {
        Err(errors::READ_ONLY)
    }

    /// The volume's size in bytes.
    ///
    /// The reader knows the sector and cluster counts, so the size is
    /// known: it is the number of data sectors times the sector size.
    fn total_size(&self) -> Option<u64> {
        Some(self.fs.total_bytes())
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::{make_fat12_volume, FakeBlock};

    /// A volume with one file in its root.
    fn volume() -> FatVolume<FakeBlock> {
        FatVolume::mount(make_fat12_volume()).expect("the volume mounts")
    }

    #[test]
    fn the_volume_mounts_and_names_itself() {
        let volume = volume();
        assert_eq!(volume.name(), "fat");
        assert_eq!(volume.variant(), crate::FatVariant::Fat12);
    }

    #[test]
    fn a_file_is_found_and_read() {
        let volume = volume();
        let info = volume.lookup("/HELLO.TXT").expect("the file is found");
        assert_eq!(info.name, "HELLO.TXT");
        assert_eq!(info.kind, NodeKind::File);
        assert_eq!(info.size, 5);
        // And its contents come back whole.
        let data = volume.read_all("/HELLO.TXT").expect("the file reads");
        assert_eq!(data, b"hello".to_vec());
    }

    #[test]
    fn the_lookup_is_case_insensitive_and_preserves_the_case() {
        let volume = volume();
        let info = volume.lookup("/hello.txt").expect("the lookup folds case");
        assert_eq!(info.name, "hello.txt", "the name is the one asked for");
        // The *stored* name keeps its case in the directory listing.
        let entries = volume.read_dir("/").expect("the root lists");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "HELLO.TXT");
        assert_eq!(entries[0].kind, NodeKind::File);
        assert_eq!(entries[0].size, 5);
    }

    #[test]
    fn a_partial_read_stops_at_the_end() {
        let volume = volume();
        let data = volume.read_at("/HELLO.TXT", 1, 3).expect("the file reads");
        assert_eq!(data, b"ell".to_vec());
        // Reading past the end returns what is there, not an error.
        let tail = volume
            .read_at("/HELLO.TXT", 3, 100)
            .expect("the file reads");
        assert_eq!(tail, b"lo".to_vec());
    }

    #[test]
    fn a_missing_path_says_so() {
        let volume = volume();
        let missing = volume.lookup("/NOPE.TXT").unwrap_err();
        assert_eq!(missing, errors::PATH_NOT_FOUND);
    }

    #[test]
    fn the_volume_refuses_to_write() {
        let volume = volume();
        assert_eq!(
            volume.write_all("/HELLO.TXT", b"x").unwrap_err(),
            errors::READ_ONLY
        );
        assert_eq!(volume.create_dir("/DIR").unwrap_err(), errors::READ_ONLY);
        assert_eq!(volume.remove("/HELLO.TXT").unwrap_err(), errors::READ_ONLY);
        assert_eq!(
            volume.rename("/HELLO.TXT", "/OTHER.TXT").unwrap_err(),
            errors::READ_ONLY
        );
        assert_eq!(
            volume.set_comment("/HELLO.TXT", "hi").unwrap_err(),
            errors::READ_ONLY
        );
    }

    #[test]
    fn the_volume_reports_its_size() {
        let volume = volume();
        assert!(volume.total_size().unwrap_or(0) > 0);
    }
}
