//! Hostile-input tests for the ELF64 parser.
//!
//! Every case here is a byte slice that a malicious or corrupt kernel image
//! could be. The parser is pure, so all of them run under plain `cargo test`
//! with no firmware involved.

use vela_elf_loader::*;

type RawPhdr = (u32, u32, u64, u64, u64, u64, u64, u64);

/// Builder for synthetic ELF64 images so each test states only what it varies.
struct ElfBuilder {
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_phentsize: u16,
    e_phnum: u16,
    e_ident_class: u8,
    e_ident_data: u8,
    e_ident_version: u8,
    magic: [u8; 4],
    /// Program headers as (p_type, p_flags, p_offset, p_vaddr, p_paddr,
    /// p_filesz, p_memsz, p_align).
    phdrs: Vec<RawPhdr>,
    /// Bytes of segment payload appended after the headers.
    payload: usize,
    /// When set, the built image is truncated to this many bytes. Used to
    /// build images whose headers point past the end of the file.
    truncate_to: Option<usize>,
}

impl Default for ElfBuilder {
    fn default() -> Self {
        // The default segment lives right after the program header table.
        // p_offset = 0x1000 (4 KiB in) and p_vaddr = 0x100000 (1 MiB in),
        // which satisfies the 0x1000 alignment congruence rule.
        Self {
            e_type: ET_EXEC,
            e_machine: EM_X86_64,
            e_version: 1,
            e_entry: 0x1000,
            e_phoff: EHDR_SIZE as u64,
            e_phentsize: PHDR_SIZE as u16,
            e_phnum: 1,
            e_ident_class: 2,
            e_ident_data: 1,
            e_ident_version: 1,
            magic: [0x7f, b'E', b'L', b'F'],
            phdrs: vec![(PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x10, 0x10, 0x1000)],
            payload: 0x10,
            truncate_to: None,
        }
    }
}

impl ElfBuilder {
    /// Build a file image that actually contains the declared segments.
    ///
    /// The buffer is sized to cover every segment's file range, and each
    /// segment's payload is written at its declared `p_offset`. A builder that
    /// ignored this would produce images whose segments point past the end of
    /// the file, and every "valid" case would be rejected for the wrong
    /// reason.
    fn build(&self) -> Vec<u8> {
        let table = self.phdrs.len() * PHDR_SIZE;
        let phoff = self.e_phoff as usize;
        let headers_end = phoff.saturating_add(table);

        // The file must be large enough for the headers and for every segment.
        // Every bound is clamped: a hostile `e_phoff` or `p_offset` must not
        // make the *test builder* panic, or the test would be asserting on the
        // helper's behaviour instead of the parser's.
        let mut size = headers_end;
        for p in &self.phdrs {
            let end = (p.2.saturating_add(p.5)) as usize; // p_offset + p_filesz
            size = size.max(end);
        }
        size = size.max(headers_end.saturating_add(self.payload));
        // Cap the buffer so a hostile value cannot ask for gigabytes.
        let size = size.min(1 << 20);

        let mut buf = vec![0u8; size];

        // A hostile `e_phoff` can leave the file too small to hold even the
        // ELF header. Skip the writes in that case: the parser must reject the
        // truncated image on its own, which is exactly the behaviour under test.
        if buf.len() < EHDR_SIZE {
            return buf;
        }

        buf[0..4].copy_from_slice(&self.magic);
        buf[4] = self.e_ident_class;
        buf[5] = self.e_ident_data;
        buf[6] = self.e_ident_version;

        put_u16(&mut buf, 16, self.e_type);
        put_u16(&mut buf, 18, self.e_machine);
        put_u32(&mut buf, 20, self.e_version);
        put_u64(&mut buf, 24, self.e_entry);
        put_u64(&mut buf, 32, self.e_phoff);
        put_u16(&mut buf, 52, EHDR_SIZE as u16);
        put_u16(&mut buf, 54, self.e_phentsize);
        put_u16(&mut buf, 56, self.e_phnum);

        for (i, p) in self.phdrs.iter().enumerate() {
            let o = phoff.saturating_add(i.saturating_mul(PHDR_SIZE));
            if o.saturating_add(PHDR_SIZE) > buf.len() {
                continue;
            }
            put_u32(&mut buf, o, p.0);
            put_u32(&mut buf, o + 4, p.1);
            put_u64(&mut buf, o + 8, p.2);
            put_u64(&mut buf, o + 16, p.3);
            put_u64(&mut buf, o + 24, p.4);
            put_u64(&mut buf, o + 32, p.5);
            put_u64(&mut buf, o + 40, p.6);
            put_u64(&mut buf, o + 48, p.7);
        }

        // Fill each segment's file range with a recognisable pattern, so a
        // loader that copies the wrong bytes is caught.
        for p in &self.phdrs {
            if p.5 == 0 {
                continue;
            }
            let start = p.2 as usize;
            let len = p.5 as usize;
            if start.saturating_add(len) > buf.len() {
                continue;
            }
            for (k, byte) in buf[start..start + len].iter_mut().enumerate() {
                *byte = (k & 0xFF) as u8;
            }
        }

        // Apply any requested truncation last, so the resulting file really is
        // shorter than the headers claim.
        if let Some(n) = self.truncate_to {
            buf.truncate(n.min(buf.len()));
        }
        buf
    }

    /// Parse the built image.
    ///
    /// The image owns its bytes, so no lifetime plumbing is needed here.
    fn parse(&self) -> Result<Elf64Image, ElfError> {
        Elf64Image::parse_owned(self.build())
    }
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

// --- happy path -----------------------------------------------------------

#[test]
fn accepts_a_minimal_well_formed_image() {
    let img = ElfBuilder::default().parse().expect("should parse");
    assert_eq!(img.entry(), 0x1000);
    assert_eq!(img.load_segments.len(), 1);
    assert_eq!(img.load_segments[0].vaddr, 0x100000);
    assert_eq!(img.load_segments[0].mem_size, 0x10);
}

#[test]
fn ignores_non_load_segments() {
    let b = ElfBuilder {
        e_phnum: 2,
        phdrs: vec![
            // A PT_NOTE segment whose file range is inside the file.
            (4, 4, 0x1000, 0x100000, 0x100000, 0x10, 0x10, 0x1000),
            (PT_LOAD, 5, 0x2000, 0x200000, 0x200000, 0x10, 0x10, 0x1000),
        ],
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert_eq!(img.load_segments.len(), 1);
    assert_eq!(img.load_segments[0].vaddr, 0x200000);
}

#[test]
fn segments_are_sorted_by_address() {
    let b = ElfBuilder {
        e_phnum: 2,
        phdrs: vec![
            (PT_LOAD, 5, 0x3000, 0x300000, 0x300000, 0x10, 0x10, 0x1000),
            (PT_LOAD, 6, 0x1000, 0x100000, 0x100000, 0x10, 0x10, 0x1000),
        ],
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert_eq!(img.load_segments[0].vaddr, 0x100000);
    assert_eq!(img.load_segments[1].vaddr, 0x300000);
}

#[test]
fn bss_gap_reports_zero_fill() {
    let b = ElfBuilder {
        phdrs: vec![(PT_LOAD, 6, 0x1000, 0x100000, 0x100000, 0x10, 0x2000, 0x1000)],
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert_eq!(img.load_segments[0].zero_fill_len(), 0x1FF0);
}

#[test]
fn segment_flags_are_decoded() {
    let b = ElfBuilder {
        phdrs: vec![
            (PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x10, 0x10, 0x1000), // r-x
            (PT_LOAD, 6, 0x2000, 0x200000, 0x200000, 0x10, 0x10, 0x1000), // rw-
        ],
        e_phnum: 2,
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert!(img.load_segments[0].executable());
    assert!(!img.load_segments[0].writable());
    assert!(img.load_segments[1].writable());
    assert!(!img.load_segments[1].executable());
}

#[test]
fn adjacent_segments_do_not_overlap() {
    // Ending exactly where the next begins is legal.
    let b = ElfBuilder {
        e_phnum: 2,
        phdrs: vec![
            (
                PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x1000, 0x1000, 0x1000,
            ),
            (
                PT_LOAD, 6, 0x2000, 0x101000, 0x101000, 0x1000, 0x1000, 0x1000,
            ),
        ],
        ..Default::default()
    };
    assert!(b.parse().is_ok());
}

#[test]
fn placement_preserves_relative_layout() {
    let b = ElfBuilder {
        // Entry inside the first (text) segment, 0x100 bytes in. This is the
        // realistic case: an entry point outside every segment would make the
        // relocation arithmetic untestable.
        e_entry: 0x100100,
        e_phnum: 2,
        phdrs: vec![
            (
                PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x1000, 0x1000, 0x1000,
            ),
            (
                PT_LOAD, 6, 0x2000, 0x102000, 0x102000, 0x1000, 0x1000, 0x1000,
            ),
        ],
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    let (placed, entry) = img.place(0x400000, 0x100000).expect("should place");
    assert_eq!(placed[0].vaddr, 0x400000);
    assert_eq!(placed[1].vaddr, 0x402000);
    // The segment base moved by 0x300000, so the entry moves with it.
    assert_eq!(entry, 0x400100);
}

// --- identity and header rejection ----------------------------------------

#[test]
fn rejects_empty_input() {
    assert_eq!(
        Elf64Image::parse(&[]).unwrap_err(),
        ElfError::TooShort {
            need: EHDR_SIZE,
            got: 0
        }
    );
}

#[test]
fn rejects_truncated_header() {
    let b = ElfBuilder::default().build();
    for n in [1usize, 16, 32, 63] {
        assert!(
            Elf64Image::parse(&b[..n]).is_err(),
            "len {n} must not parse"
        );
    }
}

#[test]
fn rejects_bad_magic() {
    for m in [
        [0u8; 4],
        [0x7f, b'L', b'E', b'F'],
        [0x7f, b'E', b'L', 0xff],
        [b'M', b'Z', 0, 0],
    ] {
        let b = ElfBuilder {
            magic: m,
            ..Default::default()
        };
        assert_eq!(b.parse().unwrap_err(), ElfError::BadMagic);
    }
}

#[test]
fn rejects_32_bit_class() {
    let b = ElfBuilder {
        e_ident_class: 1,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::Not64Bit);
}

#[test]
fn rejects_big_endian() {
    let b = ElfBuilder {
        e_ident_data: 2,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::NotLittleEndian);
}

#[test]
fn rejects_bad_ident_version() {
    let b = ElfBuilder {
        e_ident_version: 0,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::BadVersion);
}

#[test]
fn rejects_bad_header_version() {
    let b = ElfBuilder {
        e_version: 7,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::BadVersion);
}

#[test]
fn rejects_relocatable_and_shared_objects() {
    // ET_REL = 1, ET_DYN = 3. A kernel must be ET_EXEC.
    for t in [1u16, 3, 4] {
        let b = ElfBuilder {
            e_type: t,
            ..Default::default()
        };
        assert_eq!(b.parse().unwrap_err(), ElfError::NotExecutable);
    }
}

#[test]
fn rejects_foreign_machine_types() {
    for m in [3u16, 40, 183, 243] {
        let b = ElfBuilder {
            e_machine: m,
            ..Default::default()
        };
        assert_eq!(b.parse().unwrap_err(), ElfError::WrongMachine { got: m });
    }
}

#[test]
fn rejects_wrong_phentsize() {
    for n in [0u16, 32, 55, 57, 72] {
        let b = ElfBuilder {
            e_phentsize: n,
            ..Default::default()
        };
        assert_eq!(
            b.parse().unwrap_err(),
            ElfError::BadProgramHeaderEntrySize { got: n }
        );
    }
}

// --- program header table bounds ------------------------------------------

#[test]
fn rejects_zero_program_headers() {
    let b = ElfBuilder {
        e_phnum: 0,
        phdrs: vec![],
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::NoLoadableSegments);
}

#[test]
fn rejects_phoff_past_end_of_file() {
    // A `p_offset`/`e_phoff` far past the (capped) end of the real file.
    for off in [0x400000u64, 0x8000_0000, 0x4000_0000_0000] {
        let b = ElfBuilder {
            e_phoff: off,
            ..Default::default()
        };
        assert_eq!(b.parse().unwrap_err(), ElfError::ProgramHeaderOutOfBounds);
    }
}

#[test]
fn rejects_phoff_that_wraps() {
    let b = ElfBuilder {
        e_phoff: u64::MAX,
        e_phnum: 0xFFFF,
        ..Default::default()
    };
    // Either the multiply or the add overflows; both are rejections.
    let e = b.parse().unwrap_err();
    assert!(
        matches!(e, ElfError::Overflow | ElfError::ProgramHeaderOutOfBounds),
        "unexpected {e:?}"
    );
}

#[test]
fn rejects_phnum_larger_than_table() {
    let b = ElfBuilder {
        e_phnum: 100,
        phdrs: vec![(PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x10, 0x10, 0x1000)],
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::ProgramHeaderOutOfBounds);
}

#[test]
fn rejects_image_with_headers_but_no_load_segment() {
    let b = ElfBuilder {
        e_phnum: 1,
        phdrs: vec![(0, 0, 0, 0, 0, 0, 0, 0)],
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::NoLoadableSegments);
}

// --- segment bounds and arithmetic ----------------------------------------

#[test]
fn rejects_segment_past_end_of_file() {
    for (off, sz) in [(0x1_0000u64, 0x10u64), (0x2000, 0x10), (u64::MAX - 4, 0x20)] {
        let b = ElfBuilder {
            phdrs: vec![(PT_LOAD, 5, off, 0x100000, 0x100000, sz, sz, 0x1000)],
            // The file really is short, so the segment genuinely lies outside.
            truncate_to: Some(0x2000),
            ..Default::default()
        };
        let e = b.parse().unwrap_err();
        assert!(
            matches!(e, ElfError::SegmentOutOfBounds | ElfError::Overflow),
            "off {off:#x} sz {sz:#x} gave {e:?}"
        );
    }
}

#[test]
fn rejects_filesize_greater_than_memsize() {
    let b = ElfBuilder {
        phdrs: vec![(PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x100, 0x50, 0x1000)],
        payload: 0x100,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::FileSizeExceedsMemorySize);
}

#[test]
fn rejects_non_power_of_two_alignment() {
    for a in [3u64, 5, 6, 7, 100, 0x1001] {
        let b = ElfBuilder {
            phdrs: vec![(PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x10, 0x10, a)],
            ..Default::default()
        };
        assert_eq!(b.parse().unwrap_err(), ElfError::BadAlignment { got: a });
    }
}

#[test]
fn accepts_zero_and_one_alignment() {
    for a in [0u64, 1] {
        // With p_align <= 1 the congruence rule does not apply.
        let b = ElfBuilder {
            phdrs: vec![(PT_LOAD, 5, 0x1001, 0x100003, 0x100003, 0x10, 0x10, a)],
            ..Default::default()
        };
        assert!(b.parse().is_ok(), "align {a} should be accepted");
    }
}

#[test]
fn rejects_misaligned_vaddr() {
    // p_vaddr and p_offset must be congruent modulo p_align.
    let b = ElfBuilder {
        phdrs: vec![(PT_LOAD, 5, 0x1001, 0x100000, 0x100000, 0x10, 0x10, 0x1000)],
        ..Default::default()
    };
    assert_eq!(
        b.parse().unwrap_err(),
        ElfError::AddressMisaligned {
            vaddr: 0x100000,
            offset: 0x1001,
            align: 0x1000
        }
    );
}

#[test]
fn rejects_segment_wrapping_the_address_space() {
    let b = ElfBuilder {
        phdrs: vec![(
            PT_LOAD,
            5,
            0x1000,
            u64::MAX - 0x100,
            u64::MAX - 0x100,
            0x10,
            0x1000,
            0x1000,
        )],
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::SegmentWraps);
}

#[test]
fn rejects_overlapping_segments() {
    let b = ElfBuilder {
        e_phnum: 2,
        phdrs: vec![
            (
                PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x1000, 0x2000, 0x1000,
            ),
            (
                PT_LOAD, 6, 0x2000, 0x101000, 0x101000, 0x1000, 0x1000, 0x1000,
            ),
        ],
        payload: 0x3000,
        ..Default::default()
    };
    assert_eq!(b.parse().unwrap_err(), ElfError::SegmentsOverlap);
}

// --- placement ------------------------------------------------------------

#[test]
fn placement_rejects_unaligned_base() {
    let img = ElfBuilder::default().parse().expect("should parse");
    assert!(matches!(
        img.place(0x400001, 0x100000),
        Err(ElfError::AddressMisaligned { .. })
    ));
}

#[test]
fn placement_rejects_insufficient_space() {
    let b = ElfBuilder {
        phdrs: vec![(
            PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x1000, 0x4000, 0x1000,
        )],
        payload: 0x2000,
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert_eq!(
        img.place(0x400000, 0x1000),
        Err(ElfError::NoRoomForSegments {
            need: 0x4000,
            available: 0x1000
        })
    );
}

#[test]
fn total_span_matches_placement_extent() {
    let b = ElfBuilder {
        e_phnum: 2,
        phdrs: vec![
            (
                PT_LOAD, 5, 0x1000, 0x100000, 0x100000, 0x1000, 0x1000, 0x1000,
            ),
            (
                PT_LOAD, 6, 0x2000, 0x101000, 0x101000, 0x1000, 0x1000, 0x1000,
            ),
        ],
        payload: 0x3000,
        ..Default::default()
    };
    let img = b.parse().expect("should parse");
    assert_eq!(img.total_span(), 0x2000);
    // Placement must succeed when exactly the span is available.
    assert!(img.place(0x400000, img.total_span()).is_ok());
}

// --- fuzz-ish: truncations of a valid image ------------------------------

#[test]
fn every_truncation_of_a_valid_image_is_rejected_or_valid() {
    let full = ElfBuilder::default().build();
    // Nothing here should panic; either outcome is acceptable as long as the
    // ones that are "valid" really are self-consistent.
    for n in 0..full.len() {
        let r = Elf64Image::parse(&full[..n]);
        if let Ok(img) = r {
            for seg in &img.load_segments {
                assert!(seg.file_offset + seg.file_size <= n as u64);
            }
        }
    }
}

#[test]
fn single_byte_corruption_never_panics() {
    let full = ElfBuilder::default().build();
    for i in 0..full.len() {
        for v in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
            let mut c = full.clone();
            c[i] = v;
            let _ = Elf64Image::parse(&c);
        }
    }
}
