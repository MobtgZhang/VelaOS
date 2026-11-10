//! The test helpers, shared by this crate's test modules.
//!
//! The fake block device and the FAT12 image it carries are what the
//! reader's own tests and the VFS adapter's tests both need. A copy of each
//! would be a second thing to keep in step, and the image in particular is
//! a fiddly thing to build: a boot sector, two FATs, a root directory and
//! one cluster of data.

use alloc::vec;
use alloc::vec::Vec;

use crate::errors;
use crate::BlockDevice;

/// A fake block device backed by a byte vector.
#[derive(Debug)]
pub struct FakeBlock {
    /// The device's contents.
    data: Vec<u8>,
    /// The block size, in bytes.
    block_size: u64,
}

impl FakeBlock {
    /// A device of `capacity` bytes, with `block_size`-byte blocks.
    pub fn new(block_size: u64, capacity: usize) -> Self {
        Self {
            data: vec![0u8; capacity],
            block_size,
        }
    }

    /// Write `bytes` at `block`.
    pub fn write_blocks(&mut self, block: u64, bytes: &[u8]) {
        let start = (block * self.block_size) as usize;
        self.data[start..start + bytes.len()].copy_from_slice(bytes);
    }
}

impl BlockDevice for FakeBlock {
    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<(), i32> {
        let start = (block * self.block_size) as usize;
        let end = start + buf.len();
        if end > self.data.len() {
            return Err(errors::IO_ERROR);
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn block_size(&self) -> u64 {
        self.block_size
    }
}

/// A minimal FAT12 volume with one file, `HELLO.TXT`, holding "hello".
///
/// The layout is the smallest that exercises the reader: one sector per
/// cluster, two FATs, a fixed root directory, and a single file's worth of
/// data in the first data cluster.
pub fn make_fat12_volume() -> FakeBlock {
    let bs = 512;
    // 34 sectors: 33 addressable, plus the data sector at block 33.
    let mut dev = FakeBlock::new(bs, (bs as usize) * 34);

    // The boot sector.
    let mut boot = [0u8; 512];
    boot[0..3].copy_from_slice(b"EBR"); // jump
    boot[11..13].copy_from_slice(&(bs as u16).to_le_bytes()); // sector size
    boot[13] = 1; // sectors per cluster
    boot[14..16].copy_from_slice(&1u16.to_le_bytes()); // reserved sectors
    boot[16] = 2; // two FATs
    boot[17..19].copy_from_slice(&(224u16).to_le_bytes()); // root dir entries
    boot[19..21].copy_from_slice(&(2880u16).to_le_bytes()); // total sectors 16
    boot[21] = 0xF0; // media
    boot[22..24].copy_from_slice(&(9u16).to_le_bytes()); // fat size 16
    boot[24..26].copy_from_slice(&(18u16).to_le_bytes()); // sectors per track
    boot[26..28].copy_from_slice(&(2u16).to_le_bytes()); // heads
    boot[510] = 0x55;
    boot[511] = 0xAA;
    dev.write_blocks(0, &boot);

    // FAT 1: reserve clusters 0 and 1, mark cluster 2 as end-of-chain.
    let fat_start = 1;
    let mut fat = [0u8; 512 * 9];
    fat[0] = 0xF0;
    fat[1] = 0xFF;
    fat[2] = 0xFF;
    fat[3] = 0xFF; // cluster 2 entry, low byte
    fat[4] = 0x0F; // cluster 2 entry, high nibble -> 0xFFF
    dev.write_blocks(fat_start, &fat);
    // The second copy of the FAT.
    dev.write_blocks(fat_start + 9, &fat);

    // The root directory starts at sector 1 + 2*9 = 19.
    let mut root = [0u8; 512];
    let mut name = [b' '; 11];
    name[0..5].copy_from_slice(b"HELLO");
    name[8..11].copy_from_slice(b"TXT");
    root[0..11].copy_from_slice(&name);
    root[11] = 0x20; // archive attribute
    root[26..28].copy_from_slice(&(2u16).to_le_bytes()); // first cluster = 2
    root[28..32].copy_from_slice(&(5u32).to_le_bytes()); // file size = 5
    dev.write_blocks(19, &root);

    // The data region: the root directory takes 14 sectors, so the data
    // starts at 1 + 18 + 14 = 33, and cluster 2 is sector 33.
    let mut data = [0u8; 512];
    data[0..5].copy_from_slice(b"hello");
    dev.write_blocks(33, &data);

    dev
}
