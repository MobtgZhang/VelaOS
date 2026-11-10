//! A FAT12/FAT16/FAT32 filesystem reader.
//!
//! # What this crate is for
//!
//! It is VelaOS's first real filesystem: on top of a block device it parses
//! a FAT boot sector, walks the FAT chain, and exposes files and
//! directories through [`FatFs`] and [`File`]. It is read-only in v0.1.0;
//! create/write/delete are a future milestone.
//!
//! # The block device
//!
//! A block device implements [`BlockDevice`] — `read_block`/`write_block`
//! at a block index. The FAT code never touches the device directly: it asks
//! for blocks by number and converts them to byte offsets using the
//! device's block size. This keeps the filesystem testable with a fake block
//! device and lets any driver (NVMe, ATA, RAM disk) back it.
//!
//! # FAT variants
//!
//! FAT12, FAT16 and FAT32 share the same on-disk structures: a boot sector,
//! one or more FAT tables, a root directory (FAT12/16) or a root directory
//! cluster (FAT32), and data clusters. The variant is chosen from the
//! cluster count, so a single implementation handles all three.

#![no_std]

#[macro_use]
extern crate alloc;

use alloc::vec::Vec;
use core::cmp::min;

/// Errors the FAT filesystem returns.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// Not a FAT volume (bad signature or reserved area).
    pub const NOT_FAT: i32 = -1;
    /// The file or directory was not found.
    pub const NO_OBJECT: i32 = -2;
    /// The end of the file was reached.
    pub const EOF: i32 = -3;
    /// The path is not valid for this volume.
    pub const BAD_PATH: i32 = -4;
    /// A read from the block device failed.
    pub const IO_ERROR: i32 = -5;
    /// The operation is not supported (write, delete, …).
    pub const NOT_SUPPORTED: i32 = -6;
}

/// A block device, addressed by block index.
#[cfg(test)]
pub mod tests_support;

pub mod vfs;

pub trait BlockDevice {
    /// Read one block into `buf`, which must be `block_size()` bytes.
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<(), i32>;
    /// Write one block from `buf`. Read-only filesystems may refuse.
    fn write_block(&self, block: u64, _buf: &[u8]) -> Result<(), i32> {
        let _ = block;
        let _ = _buf;
        Err(errors::NOT_SUPPORTED)
    }
    /// The block size, in bytes.
    fn block_size(&self) -> u64;
}

/// One directory entry in a FAT directory table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    /// The name, space-padded (8.3 format).
    pub name: [u8; 8],
    /// The extension, space-padded.
    pub ext: [u8; 3],
    /// The attributes byte.
    pub attributes: u8,
    /// The first cluster of the file's data.
    pub first_cluster: u32,
    /// The file size, in bytes.
    pub file_size: u32,
}

const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_LABEL: u8 = 0x08;

/// An open file.
#[derive(Debug)]
pub struct File<'a, B: BlockDevice> {
    fs: &'a FatFs<B>,
    /// The directory entry that described this file when it was opened.
    entry: DirEntry,
    /// The cluster currently being read (0 means "at start, not yet reading").
    cluster: u32,
    /// The offset within the current cluster, in bytes.
    cluster_offset: u32,
    /// The total bytes read so far.
    position: u64,
}

/// The FAT filesystem.
#[allow(dead_code)]
#[derive(Debug)]
pub struct FatFs<B: BlockDevice> {
    device: B,
    /// The sector size (bytes per sector from the boot sector).
    sector_size: u32,
    /// The number of sectors per cluster.
    sectors_per_cluster: u32,
    /// The number of reserved sectors (boot sector + boot code).
    reserved_sectors: u32,
    /// The number of FAT copies.
    fat_count: u32,
    /// The size of one FAT, in sectors.
    fat_size_sectors: u32,
    /// The total sector count.
    total_sectors: u32,
    /// The cluster count of the data area.
    cluster_count: u32,
    /// The first sector of the data area.
    data_sector: u32,
    /// The first sector of the root directory (FAT12/16 only).
    root_dir_sector: u32,
    /// The number of root directory entries (FAT12/16 only).
    root_dir_entries: u32,
    /// The FAT variant detected at mount.
    variant: FatVariant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FatVariant {
    Fat12,
    Fat16,
    Fat32,
}

impl<B: BlockDevice> FatFs<B> {
    /// Mount a FAT volume from `device`.
    ///
    /// Reads the boot sector, validates the FAT signature (`0x55AA` at offset
    /// 510), and determines the FAT variant from the cluster count.
    ///
    /// # Errors
    ///
    /// Returns [`errors::NOT_FAT`] when the volume is not a FAT filesystem,
    /// and [`errors::IO_ERROR`] when the boot sector cannot be read.
    pub fn mount(device: B) -> Result<Self, i32> {
        let block_size = device.block_size();
        if block_size < 512 {
            return Err(errors::NOT_FAT);
        }
        let mut boot = vec![0u8; block_size as usize];
        device
            .read_block(0, &mut boot)
            .map_err(|_| errors::IO_ERROR)?;

        if boot[510] != 0x55 || boot[511] != 0xAA {
            return Err(errors::NOT_FAT);
        }

        let sector_size = u16::from_le_bytes([boot[11], boot[12]]) as u32;
        let sectors_per_cluster = boot[13] as u32;
        let reserved_sectors = u16::from_le_bytes([boot[14], boot[15]]) as u32;
        let fat_count = boot[16] as u32;
        let root_dir_entries = u16::from_le_bytes([boot[17], boot[18]]) as u32;
        let total_sectors16 = u16::from_le_bytes([boot[19], boot[20]]) as u32;
        let fat_size16 = u16::from_le_bytes([boot[22], boot[23]]) as u32;
        let total_sectors32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);
        let fat_size32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);

        let total_sectors = if total_sectors16 != 0 {
            total_sectors16
        } else {
            total_sectors32
        };
        let fat_size_sectors = if fat_size16 != 0 {
            fat_size16
        } else {
            fat_size32
        };

        let root_dir_sectors = (root_dir_entries as u64 * 32).div_ceil(sector_size as u64);
        let data_sector = reserved_sectors + fat_count * fat_size_sectors + root_dir_sectors as u32;
        let data_sectors = total_sectors
            - reserved_sectors
            - (fat_count * fat_size_sectors)
            - root_dir_sectors as u32;
        let cluster_count = data_sectors / sectors_per_cluster;

        let variant = if cluster_count < 4085 {
            FatVariant::Fat12
        } else if cluster_count < 65525 {
            FatVariant::Fat16
        } else {
            FatVariant::Fat32
        };

        let root_dir_sector = match variant {
            FatVariant::Fat32 => {
                let root_dir_cluster = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);
                reserved_sectors
                    + fat_count * fat_size_sectors
                    + (root_dir_cluster - 2) * sectors_per_cluster
            }
            _ => reserved_sectors + fat_count * fat_size_sectors,
        };

        Ok(Self {
            device,
            sector_size,
            sectors_per_cluster,
            reserved_sectors,
            fat_count,
            fat_size_sectors,
            total_sectors,
            cluster_count,
            data_sector,
            root_dir_sector,
            root_dir_entries,
            variant,
        })
    }

    /// The sector size.
    pub const fn sector_size(&self) -> u32 {
        self.sector_size
    }

    /// The cluster count.
    pub const fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    /// The FAT variant.
    /// The volume's size in bytes.
    ///
    /// The size is the number of data sectors times the sector size: the
    /// clusters the filesystem can address, and nothing else. It is what a
    /// program asking "how big is this disk" is told.
    pub const fn total_bytes(&self) -> u64 {
        (self.total_sectors as u64 - self.data_sector as u64) * self.sector_size as u64
    }

    pub const fn variant(&self) -> FatVariant {
        self.variant
    }

    /// Convert a cluster number to its first data sector.
    const fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.data_sector + (cluster - 2) * self.sectors_per_cluster
    }

    /// Read one cluster into `buf`.
    fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), i32> {
        let sector = self.cluster_to_sector(cluster);
        let bytes = self.sectors_per_cluster as u64 * self.sector_size as u64;
        let mut offset = 0u64;
        while offset < bytes {
            let mut block = [0u8; 512];
            let n = min(bytes - offset, 512) as usize;
            self.device
                .read_block(sector as u64 + offset / 512, &mut block)
                .map_err(|_| errors::IO_ERROR)?;
            buf[offset as usize..offset as usize + n].copy_from_slice(&block[..n]);
            offset += n as u64;
        }
        Ok(())
    }

    /// Read the FAT entry for `cluster`, returning the next cluster or the
    /// end-of-chain marker.
    fn read_fat_entry(&self, cluster: u32) -> Result<u32, i32> {
        let fat_offset_bytes = match self.variant {
            FatVariant::Fat12 => cluster * 3 / 2,
            FatVariant::Fat16 => cluster * 2,
            FatVariant::Fat32 => cluster * 4,
        };
        let fat_sector = self.reserved_sectors + fat_offset_bytes / self.sector_size;
        let fat_offset_in_sector = fat_offset_bytes % self.sector_size;
        let mut block = [0u8; 512];
        self.device
            .read_block(fat_sector as u64, &mut block)
            .map_err(|_| errors::IO_ERROR)?;
        let next = match self.variant {
            FatVariant::Fat12 => {
                let b0 = block[fat_offset_in_sector as usize] as u32;
                let b1 = block[(fat_offset_in_sector as usize + 1) % 512] as u32;
                let raw = if cluster % 2 == 0 {
                    b0 | (b1 & 0x0F) << 8
                } else {
                    (b0 >> 4) | (b1 << 4)
                };
                raw & 0xFFF
            }
            FatVariant::Fat16 => u16::from_le_bytes([
                block[fat_offset_in_sector as usize],
                block[(fat_offset_in_sector as usize + 1) % 512],
            ]) as u32,
            FatVariant::Fat32 => {
                u32::from_le_bytes([
                    block[fat_offset_in_sector as usize],
                    block[(fat_offset_in_sector as usize + 1) % 512],
                    block[(fat_offset_in_sector as usize + 2) % 512],
                    block[(fat_offset_in_sector as usize + 3) % 512],
                ]) & 0x0FFF_FFFF
            }
        };
        Ok(next)
    }

    /// Is `cluster` an end-of-chain marker for this volume's variant?
    const fn is_eoc(&self, next: u32) -> bool {
        match self.variant {
            FatVariant::Fat12 => next >= 0xFF8,
            FatVariant::Fat16 => next >= 0xFFF8,
            FatVariant::Fat32 => next >= 0x0FFF_FFF8,
        }
    }

    /// Open the root directory.
    pub fn open_root(&self) -> File<'_, B> {
        File {
            fs: self,
            entry: DirEntry {
                name: *b"ROOT    ",
                ext: [b' '; 3],
                attributes: ATTR_DIRECTORY,
                first_cluster: match self.variant {
                    FatVariant::Fat32 => 2,
                    _ => 0,
                },
                file_size: 0,
            },
            cluster: match self.variant {
                FatVariant::Fat32 => 2,
                _ => 0,
            },
            cluster_offset: 0,
            position: 0,
        }
    }

    /// Open a file by path.
    ///
    /// Paths are `/`-separated, e.g. `/DIR/FILE.TXT`. The comparison is
    /// case-insensitive and matches the FAT 8.3 name. Returns
    /// [`errors::NO_OBJECT`] when nothing matches.
    pub fn open(&self, path: &str) -> Result<File<'_, B>, i32> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Ok(self.open_root());
        }
        let mut dir = self.open_root();
        for comp in path.split('/') {
            if comp.is_empty() {
                continue;
            }
            let found = dir.find_entry(comp)?;
            dir = File {
                fs: self,
                entry: found,
                cluster: found.first_cluster,
                cluster_offset: 0,
                position: 0,
            };
        }
        Ok(dir)
    }
}

impl<'a, B: BlockDevice> File<'a, B> {
    /// Find a directory entry by name (case-insensitive 8.3 match).
    fn find_entry(&self, name: &str) -> Result<DirEntry, i32> {
        // Build an 11-byte FAT name from `name`, uppercased.
        let mut fat_name = [b' '; 11];
        if let Some(dot) = name.rfind('.') {
            let base = &name[..dot];
            let ext = &name[dot + 1..];
            if base.len() > 8 || ext.len() > 3 {
                return Err(errors::BAD_PATH);
            }
            fat_name[..base.len()].copy_from_slice(base.as_bytes());
            fat_name[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
        } else {
            if name.len() > 8 {
                return Err(errors::BAD_PATH);
            }
            fat_name[..name.len()].copy_from_slice(name.as_bytes());
        }
        for b in fat_name.iter_mut() {
            *b = b.to_ascii_uppercase();
        }

        // Scan the directory this handle names.
        if self.entry.attributes & ATTR_DIRECTORY == 0 {
            return Err(errors::NO_OBJECT);
        }

        // FAT12/16 root directory.
        if self.cluster == 0 && self.entry.first_cluster == 0 {
            let total = (self.fs.root_dir_entries as usize) * 32;
            let mut buf = vec![0u8; total];
            let mut offset = 0u64;
            while offset < total as u64 {
                let mut block = [0u8; 512];
                let n = min(total as u64 - offset, 512) as usize;
                self.fs
                    .device
                    .read_block(self.fs.root_dir_sector as u64 + offset / 512, &mut block)
                    .map_err(|_| errors::IO_ERROR)?;
                buf[offset as usize..offset as usize + n].copy_from_slice(&block[..n]);
                offset += n as u64;
            }
            for i in 0..self.fs.root_dir_entries as usize {
                let e = &buf[i * 32..];
                if e[0] == 0x00 {
                    break;
                }
                if e[0] == 0xE5 || e[11] & ATTR_VOLUME_LABEL != 0 {
                    continue;
                }
                if entry_name_matches(e, &fat_name) {
                    return Ok(dir_entry_from_raw(e));
                }
            }
            return Err(errors::NO_OBJECT);
        }

        // Cluster-based directory.
        let mut cluster = self.cluster;
        loop {
            let cluster_bytes =
                (self.fs.sectors_per_cluster as u64 * self.fs.sector_size as u64) as usize;
            let mut buf = vec![0u8; cluster_bytes];
            self.fs
                .read_cluster(cluster, &mut buf)
                .map_err(|_| errors::IO_ERROR)?;
            let mut i = 0;
            while i + 32 <= buf.len() {
                let e = &buf[i..];
                i += 32;
                if e[0] == 0x00 {
                    break;
                }
                if e[0] == 0xE5 || e[11] & ATTR_VOLUME_LABEL != 0 {
                    continue;
                }
                if entry_name_matches(e, &fat_name) {
                    return Ok(dir_entry_from_raw(e));
                }
            }
            let next = self.fs.read_fat_entry(cluster)?;
            if self.fs.is_eoc(next) {
                break;
            }
            cluster = next;
        }
        Err(errors::NO_OBJECT)
    }

    /// List the entries of the directory this file handle names.
    ///
    /// Returns the directory entries (volume labels and long-name entries are
    /// skipped). The handle must name a directory.
    pub fn read_dir(&self) -> Result<Vec<DirEntry>, i32> {
        if self.entry.attributes & ATTR_DIRECTORY == 0 {
            return Err(errors::NO_OBJECT);
        }
        let mut entries = Vec::new();

        // FAT12/16 root directory: a fixed region at root_dir_sector, not
        // cluster-based.
        if self.cluster == 0 && self.entry.first_cluster == 0 {
            let total = (self.fs.root_dir_entries as usize) * 32;
            let mut buf = vec![0u8; total];
            let mut offset = 0u64;
            while offset < total as u64 {
                let mut block = [0u8; 512];
                let n = min(total as u64 - offset, 512) as usize;
                self.fs
                    .device
                    .read_block(self.fs.root_dir_sector as u64 + offset / 512, &mut block)
                    .map_err(|_| errors::IO_ERROR)?;
                buf[offset as usize..offset as usize + n].copy_from_slice(&block[..n]);
                offset += n as u64;
            }
            for i in 0..self.fs.root_dir_entries as usize {
                let e = &buf[i * 32..];
                if e[0] == 0x00 {
                    break;
                }
                if e[0] == 0xE5 || e[11] & ATTR_VOLUME_LABEL != 0 {
                    continue;
                }
                entries.push(dir_entry_from_raw(e));
            }
            return Ok(entries);
        }

        // Cluster-based directories (FAT32 root and subdirectories).
        let mut cluster = self.cluster;
        loop {
            let cluster_bytes =
                (self.fs.sectors_per_cluster as u64 * self.fs.sector_size as u64) as usize;
            let mut buf = vec![0u8; cluster_bytes];
            self.fs
                .read_cluster(cluster, &mut buf)
                .map_err(|_| errors::IO_ERROR)?;
            let mut i = 0;
            while i + 32 <= buf.len() {
                let e = &buf[i..];
                i += 32;
                if e[0] == 0x00 {
                    return Ok(entries);
                }
                if e[0] == 0xE5 || e[11] & ATTR_VOLUME_LABEL != 0 {
                    continue;
                }
                entries.push(dir_entry_from_raw(e));
            }
            let next = self.fs.read_fat_entry(cluster)?;
            if self.fs.is_eoc(next) {
                return Ok(entries);
            }
            cluster = next;
        }
    }

    /// The file's size, in bytes.
    pub const fn len(&self) -> u64 {
        self.entry.file_size as u64
    }

    /// The attributes of the directory entry that named this file.
    ///
    /// The attribute byte is what says whether the entry is a directory, so
    /// a caller that has opened a path needs it to answer "what did I open".
    pub const fn attributes(&self) -> u8 {
        self.entry.attributes
    }

    /// True when the file is empty.
    pub const fn is_empty(&self) -> bool {
        self.entry.file_size == 0
    }

    /// Read up to `buf.len()` bytes into `buf`.
    ///
    /// Returns the number of bytes read, or [`errors::EOF`] when at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, i32> {
        if self.position >= self.len() {
            return Err(errors::EOF);
        }
        let mut total = 0usize;
        while total < buf.len() && self.position < self.len() {
            // Ensure we have a current cluster.
            if self.cluster == 0 && self.position == 0 {
                self.cluster = self.entry.first_cluster;
                self.cluster_offset = 0;
            }
            let cluster_bytes =
                (self.fs.sectors_per_cluster as u64 * self.fs.sector_size as u64) as usize;
            let offset_in_cluster = self.cluster_offset as usize;
            let bytes_in_cluster = cluster_bytes - offset_in_cluster;
            let remaining = (self.len() - self.position) as usize;
            let want = min(buf.len() - total, remaining);
            let n = min(want, bytes_in_cluster);
            // Read the sector(s) covering [offset_in_cluster, offset_in_cluster + n).
            let sector = self.fs.cluster_to_sector(self.cluster);
            let mut copied = 0usize;
            while copied < n {
                let mut block = [0u8; 512];
                let block_offset = (offset_in_cluster + copied) % 512;
                let sector_in_cluster = (offset_in_cluster + copied) / 512;
                let block_sector = sector + sector_in_cluster as u32;
                self.fs
                    .device
                    .read_block(block_sector as u64, &mut block)
                    .map_err(|_| errors::IO_ERROR)?;
                let take = min(n - copied, 512 - block_offset);
                buf[total + copied..total + copied + take]
                    .copy_from_slice(&block[block_offset..block_offset + take]);
                copied += take;
            }
            total += n;
            self.position += n as u64;
            self.cluster_offset += n as u32;
            if self.cluster_offset as usize >= cluster_bytes {
                let next = self.fs.read_fat_entry(self.cluster)?;
                if self.fs.is_eoc(next) {
                    break;
                }
                self.cluster = next;
                self.cluster_offset = 0;
            }
        }
        if total == 0 {
            Err(errors::EOF)
        } else {
            Ok(total)
        }
    }
}

/// Build a [`DirEntry`] from a raw 32-byte directory entry.
fn dir_entry_from_raw(e: &[u8]) -> DirEntry {
    let mut name = [b' '; 8];
    let mut ext = [b' '; 3];
    name.copy_from_slice(&e[0..8]);
    ext.copy_from_slice(&e[8..11]);
    let first_cluster = u16::from_le_bytes([e[26], e[27]]) as u32
        | (u32::from_le_bytes([e[20], e[21], e[22], e[23]]) << 16);
    let file_size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);
    DirEntry {
        name,
        ext,
        attributes: e[11],
        first_cluster,
        file_size,
    }
}

/// Compare a 32-byte directory entry's 8.3 name against an 11-byte FAT name.
fn entry_name_matches(entry: &[u8], fat_name: &[u8; 11]) -> bool {
    if entry.len() < 11 {
        return false;
    }
    entry[0..11]
        .iter()
        .zip(fat_name.iter())
        .all(|(a, b)| a.to_ascii_uppercase() == *b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::*;

    #[test]
    fn mount_reads_boot_sector_and_detects_fat12() {
        let dev = make_fat12_volume();
        let fs = FatFs::mount(dev).unwrap();
        assert_eq!(fs.variant(), FatVariant::Fat12);
        assert_eq!(fs.sector_size(), 512);
    }

    #[test]
    fn open_root_and_read_dir_finds_the_file() {
        let fs = FatFs::mount(make_fat12_volume()).unwrap();
        let root = fs.open_root();
        let entries = root.read_dir().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_size, 5);
    }

    #[test]
    fn open_and_read_file_returns_its_contents() {
        let fs = FatFs::mount(make_fat12_volume()).unwrap();
        let mut file = fs.open("/HELLO.TXT").unwrap();
        assert_eq!(file.len(), 5);
        let mut buf = [0u8; 10];
        let n = file.read(&mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf[..n], b"hello");
        // A second read hits EOF.
        assert_eq!(file.read(&mut buf).unwrap_err(), errors::EOF);
    }

    #[test]
    fn opening_a_missing_file_is_an_error() {
        let fs = FatFs::mount(make_fat12_volume()).unwrap();
        assert_eq!(fs.open("/NOPE.TXT").unwrap_err(), errors::NO_OBJECT);
    }

    #[test]
    fn a_non_fat_volume_is_refused() {
        let dev = FakeBlock::new(512, 512);
        assert_eq!(FatFs::mount(dev).unwrap_err(), errors::NOT_FAT);
    }
}
