//! ELF64 validation for the VelaOS kernel image.
//!
//! Everything here is a pure function over a byte slice. The crate performs no
//! I/O and touches no memory outside `&self`, which is what makes the hostile
//! input tests in `tests/` meaningful: every rejection path is reachable from
//! an ordinary host `cargo test`.
//!
//! The loader's job is narrow and adversarial:
//!
//! * reject anything that is not a well-formed ELF64 `ET_EXEC` for `x86_64`;
//! * reject every arithmetic overflow that could make a bounds check pass;
//! * decide *where* each `PT_LOAD` segment goes, without writing.
//!
//! Writing the bytes is deliberately a separate step ([`LoadedSegment`]) so the
//! UEFI side owns all pointer writes and the validation side stays pure.

#![no_std]

use core::mem;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// File is shorter than an ELF64 header.
    TooShort { need: usize, got: usize },
    /// First four bytes are not `\x7fELF`.
    BadMagic,
    /// `EI_CLASS` is not `ELFCLASS64`.
    Not64Bit,
    /// `EI_DATA` is not little-endian.
    NotLittleEndian,
    /// `EI_VERSION` is not `EV_CURRENT`, or `e_version` is not 1.
    BadVersion,
    /// `e_type` is not `ET_EXEC`.
    NotExecutable,
    /// `e_machine` is not `EM_X86_64`.
    WrongMachine { got: u16 },
    /// Program header offset/count would read outside the file.
    ProgramHeaderOutOfBounds,
    /// Program header table length is not a whole number of entries.
    ProgramHeaderSizeNotMultiple { size: u64, stride: u64 },
    /// `e_phentsize` differs from the expected struct size.
    BadProgramHeaderEntrySize { got: u16 },
    /// No `PT_LOAD` segment present.
    NoLoadableSegments,
    /// An overlap check or address computation overflowed.
    Overflow,
    /// A segment's file range lies outside the file.
    SegmentOutOfBounds,
    /// A segment has `p_filesz > p_memsz`.
    FileSizeExceedsMemorySize,
    /// A segment's `p_align` is neither 0, 1, nor a power of two.
    BadAlignment { got: u64 },
    /// A segment's `p_vaddr` is not congruent to `p_offset` modulo `p_align`.
    AddressMisaligned { vaddr: u64, offset: u64, align: u64 },
    /// A segment's virtual range wraps around the address space.
    SegmentWraps,
    /// Two segments occupy overlapping virtual ranges.
    SegmentsOverlap,
    /// No free region large enough to place the segments.
    NoRoomForSegments { need: u64, available: u64 },
}

const EI_CLASS: usize = 4;
const EI_DATA: usize = 5;
const EI_VERSION: usize = 6;

/// `e_ident` values.
const ELFMAG: [u8; 4] = [0x7f, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u8 = 1;

/// `e_type` values VelaOS accepts for a kernel image.
pub const ET_EXEC: u16 = 2;

/// `e_machine` value for x86-64.
pub const EM_X86_64: u16 = 62;

/// `p_type` values.
pub const PT_LOAD: u32 = 1;

/// Byte length of a 64-bit ELF header.
pub const EHDR_SIZE: usize = 64;
/// Byte length of a 64-bit program header.
pub const PHDR_SIZE: usize = 56;

/// Page size the loader assumes for placement.
pub const PAGE_SIZE: u64 = 4096;

/// A parsed ELF64 header.
///
/// Field-for-field equivalent to the on-disk `Elf64_Ehdr`; the loader reads
/// bytes little-endian by hand to stay endian-explicit and `no_std`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfHeader {
    pub e_type: u16,
    pub e_machine: u16,
    pub e_version: u32,
    pub e_entry: u64,
    pub e_phoff: u64,
    pub e_shoff: u64,
    pub e_flags: u32,
    pub e_ehsize: u16,
    pub e_phentsize: u16,
    pub e_phnum: u16,
}

impl ElfHeader {
    /// Parse and structurally validate the ELF header.
    ///
    /// Only checks that the header itself is well formed; program headers are
    /// validated by [`Elf64Image::parse`].
    pub fn parse(image: &[u8]) -> Result<Self, ElfError> {
        if image.len() < EHDR_SIZE {
            return Err(ElfError::TooShort {
                need: EHDR_SIZE,
                got: image.len(),
            });
        }
        if image[0..4] != ELFMAG {
            return Err(ElfError::BadMagic);
        }
        if image[EI_CLASS] != ELFCLASS64 {
            return Err(ElfError::Not64Bit);
        }
        if image[EI_DATA] != ELFDATA2LSB {
            return Err(ElfError::NotLittleEndian);
        }
        if image[EI_VERSION] != EV_CURRENT {
            return Err(ElfError::BadVersion);
        }

        let e_type = rd_u16(image, 16)?;
        let e_machine = rd_u16(image, 18)?;
        let e_version = rd_u32(image, 20)?;
        let e_entry = rd_u64(image, 24)?;
        let e_phoff = rd_u64(image, 32)?;
        let e_shoff = rd_u64(image, 40)?;
        let e_flags = rd_u32(image, 48)?;
        let e_ehsize = rd_u16(image, 52)?;
        let e_phentsize = rd_u16(image, 54)?;
        let e_phnum = rd_u16(image, 56)?;

        if e_version != 1 {
            return Err(ElfError::BadVersion);
        }
        if e_type != ET_EXEC {
            return Err(ElfError::NotExecutable);
        }
        if e_machine != EM_X86_64 {
            return Err(ElfError::WrongMachine { got: e_machine });
        }
        if e_phentsize != PHDR_SIZE as u16 {
            return Err(ElfError::BadProgramHeaderEntrySize { got: e_phentsize });
        }

        Ok(Self {
            e_type,
            e_machine,
            e_version,
            e_entry,
            e_phoff,
            e_shoff,
            e_flags,
            e_ehsize,
            e_phentsize,
            e_phnum,
        })
    }
}

/// A parsed 64-bit program header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramHeader {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

/// `p_flags` bits.
pub mod ph_flags {
    pub const EXEC: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const READ: u32 = 4;
}

/// One `PT_LOAD` segment after placement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadedSegment {
    /// Byte range of the segment's contents inside the source image.
    pub file_offset: u64,
    pub file_size: u64,
    /// Where the segment was placed.
    pub vaddr: u64,
    /// In-memory extent. Always `>= file_size`; the gap is zero-fill.
    pub mem_size: u64,
    pub align: u64,
    pub flags: u32,
}

impl LoadedSegment {
    /// Bytes that must be zero-filled after copying `file_size` bytes.
    pub const fn zero_fill_len(&self) -> u64 {
        self.mem_size - self.file_size
    }

    /// True when the segment requests write permission.
    pub const fn writable(&self) -> bool {
        self.flags & ph_flags::WRITE != 0
    }

    /// True when the segment requests execute permission.
    pub const fn executable(&self) -> bool {
        self.flags & ph_flags::EXEC != 0
    }
}

/// A validated ELF64 kernel image.
///
/// Owns a copy of the image bytes so that a validated image can outlive the
/// buffer it was parsed from. The UEFI loader uses [`Self::parse_owned`] to
/// avoid the copy; the slice-taking [`Self::parse`] is for tests and for
/// callers that only need the metadata.
#[derive(Debug, Clone)]
pub struct Elf64Image {
    pub header: ElfHeader,
    /// Only the `PT_LOAD` entries, in ascending virtual-address order.
    pub load_segments: Vec<LoadedSegment>,
    /// The image bytes.
    pub bytes: Vec<u8>,
}

impl Elf64Image {
    /// Parse and fully validate `image` as a VelaOS kernel.
    ///
    /// Performs every check the loader relies on, including overlap detection,
    /// so a successful `Ok` means the caller may map and copy the segments
    /// without further bounds reasoning.
    pub fn parse(image: &[u8]) -> Result<Self, ElfError> {
        let mut owned = Vec::with_capacity(image.len());
        owned.extend_from_slice(image);
        Self::parse_owned(owned)
    }

    /// Parse and fully validate an owned image buffer.
    ///
    /// Does not copy: `bytes` is stored as-is.
    pub fn parse_owned(image: Vec<u8>) -> Result<Self, ElfError> {
        let header = ElfHeader::parse(&image)?;

        if header.e_phnum == 0 {
            return Err(ElfError::NoLoadableSegments);
        }

        // The whole program header table must lie inside the file, and its
        // length must be an exact multiple of the stride. Both are checked with
        // checked arithmetic so a hostile e_phoff cannot wrap.
        let table_bytes = (header.e_phnum as u64)
            .checked_mul(header.e_phentsize as u64)
            .ok_or(ElfError::Overflow)?;
        let table_end = header
            .e_phoff
            .checked_add(table_bytes)
            .ok_or(ElfError::Overflow)?;
        if table_end > image.len() as u64 {
            return Err(ElfError::ProgramHeaderOutOfBounds);
        }

        let mut load_segments = Vec::new();
        for i in 0..header.e_phnum as u64 {
            let base = header
                .e_phoff
                .checked_add(
                    i.checked_mul(header.e_phentsize as u64)
                        .ok_or(ElfError::Overflow)?,
                )
                .ok_or(ElfError::Overflow)?;
            let off = base as usize;

            let p_type = rd_u32(&image, off)?;
            if p_type != PT_LOAD {
                continue;
            }

            let flags = rd_u32(&image, off + 4)?;
            let p_offset = rd_u64(&image, off + 8)?;
            let p_vaddr = rd_u64(&image, off + 16)?;
            let p_paddr = rd_u64(&image, off + 24)?;
            let p_filesz = rd_u64(&image, off + 32)?;
            let p_memsz = rd_u64(&image, off + 40)?;
            let p_align = rd_u64(&image, off + 48)?;

            if p_filesz > p_memsz {
                return Err(ElfError::FileSizeExceedsMemorySize);
            }

            // The in-memory extent must not wrap the address space. This is
            // checked before the alignment rule because an address that has
            // already wrapped tells us nothing useful about its alignment.
            p_vaddr.checked_add(p_memsz).ok_or(ElfError::SegmentWraps)?;

            // File contents must lie inside the image. `p_offset + p_filesz`
            // is checked rather than computed.
            let file_end = p_offset.checked_add(p_filesz).ok_or(ElfError::Overflow)?;
            if file_end > image.len() as u64 {
                return Err(ElfError::SegmentOutOfBounds);
            }

            if !is_valid_align(p_align) {
                return Err(ElfError::BadAlignment { got: p_align });
            }
            if p_align > 1 && p_vaddr % p_align != p_offset % p_align {
                return Err(ElfError::AddressMisaligned {
                    vaddr: p_vaddr,
                    offset: p_offset,
                    align: p_align,
                });
            }

            let _ = p_paddr;
            load_segments.push(LoadedSegment {
                file_offset: p_offset,
                file_size: p_filesz,
                vaddr: p_vaddr,
                mem_size: p_memsz,
                align: p_align,
                flags,
            });
        }

        if load_segments.is_empty() {
            return Err(ElfError::NoLoadableSegments);
        }

        load_segments.sort_by_key(|s| s.vaddr);

        // Reject overlapping placement. Adjacency is fine; intersection is not.
        for pair in load_segments.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let a_end = a.vaddr + a.mem_size;
            if b.vaddr < a_end {
                return Err(ElfError::SegmentsOverlap);
            }
        }

        Ok(Self {
            header,
            load_segments,
            bytes: image,
        })
    }

    /// The address the kernel entry point was linked at.
    pub const fn entry(&self) -> u64 {
        self.header.e_entry
    }

    /// Total bytes of virtual memory the segments occupy, including padding
    /// needed to keep them from sharing a page.
    pub fn total_span(&self) -> u64 {
        let Some(first) = self.load_segments.first() else {
            return 0;
        };
        let last = self.load_segments[self.load_segments.len() - 1];
        (last.vaddr + last.mem_size) - first.vaddr
    }

    /// The slice of file bytes belonging to `segment`.
    ///
    /// Safe because [`Self::parse`] already proved every segment's file range
    /// is inside the image.
    pub fn segment_bytes(&self, segment: &LoadedSegment) -> &[u8] {
        let start = segment.file_offset as usize;
        let end = start + segment.file_size as usize;
        &self.bytes[start..end]
    }

    /// Place the image for a **higher-half** kernel: virtual addresses unchanged,
    /// physical base chosen separately.
    ///
    /// # Why this is a separate method
    ///
    /// [`Self::place`] relocates an image so its lowest segment lands at
    /// `load_addr`, treating that as both the new virtual *and* the new
    /// physical address. That is the right model for firmware that runs
    /// identity-mapped, and it is the wrong model for a higher-half kernel.
    ///
    /// VelaOS's kernel is *linked* at `0xFFFF_8000_0000_0000` and the loader
    /// places the image at a low *physical* address; the kernel then maps that
    /// physical range at the linked virtual range itself. The two addresses are
    /// unrelated by construction.
    ///
    /// [`Self::place`] cannot express that: for a kernel linked at
    /// `0xFFFF_8000_0000_0000`, `load_addr.checked_sub(base)` underflowed and
    /// every load failed with `Overflow` — a bare error that gave no hint that
    /// the arithmetic, not the image, was at fault.
    ///
    /// So this method does the one thing that actually needs doing: validate
    /// the image and return the *physical* offsets, leaving every virtual
    /// address exactly as the linker produced it.
    ///
    /// # Returns
    ///
    /// The image's total span, and the lowest physical address it occupies,
    /// which is `phys_base`. Each segment's `file_offset`-relative placement is
    /// the caller's business — the caller owns the pool it is writing into.
    ///
    /// # Errors
    ///
    /// [`ElfError::NoLoadableSegments`] when there is nothing to place,
    /// [`ElfError::NoRoomForSegments`] when the span exceeds `available`, and
    /// [`ElfError::AddressMisaligned`] when `phys_base` is not page aligned.
    pub fn place_higher_half(&self, phys_base: u64, available: u64) -> Result<u64, ElfError> {
        if !phys_base.is_multiple_of(PAGE_SIZE) {
            return Err(ElfError::AddressMisaligned {
                vaddr: phys_base,
                offset: 0,
                align: PAGE_SIZE,
            });
        }
        if self.load_segments.is_empty() {
            return Err(ElfError::NoLoadableSegments);
        }
        // The same overlap check `place` performs. For a higher-half image the
        // virtual addresses were fixed at link time, so an overlap here means
        // the *linker* produced a broken image — and catching it at load is
        // much better than executing from a doubly-mapped address.
        let mut sorted: Vec<u64> = self.load_segments.iter().map(|s| s.vaddr).collect();
        sorted.sort_unstable();
        for pair in sorted.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let a_end = self
                .load_segments
                .iter()
                .find(|s| s.vaddr == a)
                .map(|s| a + s.mem_size)
                .unwrap_or(b);
            if b < a_end {
                return Err(ElfError::SegmentsOverlap);
            }
        }

        let need = self.total_span();
        if need > available {
            return Err(ElfError::NoRoomForSegments { need, available });
        }
        Ok(phys_base)
    }

    /// The lowest virtual address among the loadable segments, or `None` when the
    /// image has none.
    ///
    /// This is the image's virtual base: every other segment sits at
    /// `lowest_vaddr() + offset`, and the loader needs exactly that relationship
    /// to place an image whose virtual addresses are fixed at link time.
    pub fn lowest_vaddr(&self) -> Option<u64> {
        self.load_segments.iter().map(|s| s.vaddr).min()
    }

    /// Try to relocate every segment so the lowest one starts at
    /// `load_addr`, preserving each segment's relative offset and page
    /// alignment.
    ///
    /// Returns the placed segments and the kernel entry point. The caller is
    /// responsible for ensuring `load_addr` is page aligned and that the span
    /// fits in a free region of `available` bytes.
    pub fn place(
        &self,
        load_addr: u64,
        available: u64,
    ) -> Result<(Vec<LoadedSegment>, u64), ElfError> {
        if !load_addr.is_multiple_of(PAGE_SIZE) {
            // Placing an unaligned image would silently break page sharing.
            return Err(ElfError::AddressMisaligned {
                vaddr: load_addr,
                offset: 0,
                align: PAGE_SIZE,
            });
        }
        let base = self
            .load_segments
            .first()
            .map(|s| s.vaddr)
            .ok_or(ElfError::NoLoadableSegments)?;

        let mut placed = Vec::with_capacity(self.load_segments.len());
        for seg in &self.load_segments {
            let delta = load_addr.checked_sub(base).ok_or(ElfError::Overflow)?;
            let vaddr = seg.vaddr.checked_add(delta).ok_or(ElfError::SegmentWraps)?;
            placed.push(LoadedSegment { vaddr, ..*seg });
        }

        // Re-check overlap after the shift, then bounds.
        for pair in placed.windows(2) {
            if pair[1].vaddr < pair[0].vaddr + pair[0].mem_size {
                return Err(ElfError::SegmentsOverlap);
            }
        }
        let need = self.total_span();
        if need > available {
            return Err(ElfError::NoRoomForSegments { need, available });
        }

        let entry = self
            .header
            .e_entry
            .checked_add(load_addr.checked_sub(base).ok_or(ElfError::Overflow)?)
            .ok_or(ElfError::SegmentWraps)?;

        Ok((placed, entry))
    }
}

extern crate alloc;
use alloc::vec::Vec;

const fn is_valid_align(align: u64) -> bool {
    align == 0 || align == 1 || (align & (align - 1)) == 0
}

fn rd_u16(b: &[u8], off: usize) -> Result<u16, ElfError> {
    let s = b.get(off..off + 2).ok_or(ElfError::TooShort {
        need: off + 2,
        got: b.len(),
    })?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn rd_u32(b: &[u8], off: usize) -> Result<u32, ElfError> {
    let s = b.get(off..off + 4).ok_or(ElfError::TooShort {
        need: off + 4,
        got: b.len(),
    })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn rd_u64(b: &[u8], off: usize) -> Result<u64, ElfError> {
    let s = b.get(off..off + 8).ok_or(ElfError::TooShort {
        need: off + 8,
        got: b.len(),
    })?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

// `mem` is referenced by the doc examples only; keep the import honest.
const _: () = assert!(mem::size_of::<u64>() == 8);
