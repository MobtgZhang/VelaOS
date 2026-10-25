//! Layout tests for the boot handoff structure.
//!
//! `VelaBootInfo` is the contract between the UEFI loader and the kernel.
//! The loader writes it, the kernel reads it, and both are separate binaries
//! that may be built at different times — so the *offsets* are as much a
//! public interface as the field names are.
//!
//! The expected offsets here are the ones documented in the design document
//! §4.4 and in `docs/boot/boot-protocol-v1.md`. A change to any of them
//! requires a `BOOT_ABI_VERSION` bump; that is the whole point of the
//! version field.

use core::mem::{align_of, offset_of, size_of};

use vela_boot_protocol::{
    arch_flags, mem_type, BootInfoError, PixelFormat, VelaBootInfo, BOOT_ABI_VERSION, BOOT_MAGIC,
};

/// The structure's size and alignment.
#[test]
fn the_boot_info_size_and_alignment_are_stable() {
    assert_eq!(size_of::<VelaBootInfo>(), 216);
    assert_eq!(align_of::<VelaBootInfo>(), 8);
}

/// The offset of every field.
#[test]
fn the_boot_info_field_offsets_are_stable() {
    assert_eq!(offset_of!(VelaBootInfo, magic), 0);
    assert_eq!(offset_of!(VelaBootInfo, abi_version), 8);
    assert_eq!(offset_of!(VelaBootInfo, struct_size), 12);
    assert_eq!(offset_of!(VelaBootInfo, memory_map_paddr), 16);
    assert_eq!(offset_of!(VelaBootInfo, memory_map_bytes), 24);
    assert_eq!(offset_of!(VelaBootInfo, memory_desc_size), 32);
    assert_eq!(offset_of!(VelaBootInfo, memory_desc_version), 36);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_paddr), 40);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_bytes), 48);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_width), 56);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_height), 60);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_stride_bytes), 64);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_pixel_format), 68);
    assert_eq!(offset_of!(VelaBootInfo, framebuffer_pixel_mask), 72);
    assert_eq!(offset_of!(VelaBootInfo, acpi_rsdp_paddr), 80);
    assert_eq!(offset_of!(VelaBootInfo, dtb_paddr), 88);
    assert_eq!(offset_of!(VelaBootInfo, kernel_paddr), 96);
    assert_eq!(offset_of!(VelaBootInfo, kernel_bytes), 104);
    assert_eq!(offset_of!(VelaBootInfo, initrd_paddr), 112);
    assert_eq!(offset_of!(VelaBootInfo, initrd_bytes), 120);
    assert_eq!(offset_of!(VelaBootInfo, arch_flags), 128);
    assert_eq!(offset_of!(VelaBootInfo, stack_top), 136);
    assert_eq!(offset_of!(VelaBootInfo, page_table_paddr), 144);
    assert_eq!(offset_of!(VelaBootInfo, kernel_vaddr_base), 152);
    assert_eq!(offset_of!(VelaBootInfo, kernel_text_bytes), 160);
    assert_eq!(offset_of!(VelaBootInfo, heap_paddr), 168);
    assert_eq!(offset_of!(VelaBootInfo, heap_bytes), 176);
    assert_eq!(offset_of!(VelaBootInfo, pool_paddr), 184);
    assert_eq!(offset_of!(VelaBootInfo, pool_bytes), 192);
    assert_eq!(offset_of!(VelaBootInfo, reserved), 200);
}

/// The layout has exactly one implicit hole, and it is in a known place.
///
/// The `u32` pair `framebuffer_pixel_format`/`framebuffer_pixel_mask` ends
/// at offset 76, and the next field is the 8-byte-aligned `acpi_rsdp_paddr`
/// at 80 — so the compiler inserted four bytes of padding between them.
///
/// This is worth pinning down precisely. The structure is otherwise
/// gap-free, so an *unexpected* hole would mean a field had moved without
/// anyone noticing; and this *expected* hole is at a fixed offset, so a
/// loader and a kernel built from different revisions still agree on where
/// every field lives.
///
/// The one hole is documented here rather than eliminated, because removing
/// it would change the structure's size and therefore require a
/// `BOOT_ABI_VERSION` bump.
#[test]
fn the_boot_info_layout_has_exactly_one_documented_hole() {
    // Walk the fields in offset order and record every gap.
    let fields: &[(usize, usize)] = &[
        (offset_of!(VelaBootInfo, magic), 8),
        (offset_of!(VelaBootInfo, abi_version), 4),
        (offset_of!(VelaBootInfo, struct_size), 4),
        (offset_of!(VelaBootInfo, memory_map_paddr), 8),
        (offset_of!(VelaBootInfo, memory_map_bytes), 8),
        (offset_of!(VelaBootInfo, memory_desc_size), 4),
        (offset_of!(VelaBootInfo, memory_desc_version), 4),
        (offset_of!(VelaBootInfo, framebuffer_paddr), 8),
        (offset_of!(VelaBootInfo, framebuffer_bytes), 8),
        (offset_of!(VelaBootInfo, framebuffer_width), 4),
        (offset_of!(VelaBootInfo, framebuffer_height), 4),
        (offset_of!(VelaBootInfo, framebuffer_stride_bytes), 4),
        (offset_of!(VelaBootInfo, framebuffer_pixel_format), 4),
        (offset_of!(VelaBootInfo, framebuffer_pixel_mask), 4),
        (offset_of!(VelaBootInfo, acpi_rsdp_paddr), 8),
        (offset_of!(VelaBootInfo, dtb_paddr), 8),
        (offset_of!(VelaBootInfo, kernel_paddr), 8),
        (offset_of!(VelaBootInfo, kernel_bytes), 8),
        (offset_of!(VelaBootInfo, initrd_paddr), 8),
        (offset_of!(VelaBootInfo, initrd_bytes), 8),
        (offset_of!(VelaBootInfo, arch_flags), 8),
        (offset_of!(VelaBootInfo, stack_top), 8),
        (offset_of!(VelaBootInfo, page_table_paddr), 8),
        (offset_of!(VelaBootInfo, kernel_vaddr_base), 8),
        (offset_of!(VelaBootInfo, kernel_text_bytes), 8),
        (offset_of!(VelaBootInfo, heap_paddr), 8),
        (offset_of!(VelaBootInfo, heap_bytes), 8),
        (offset_of!(VelaBootInfo, pool_paddr), 8),
        (offset_of!(VelaBootInfo, pool_bytes), 8),
        (offset_of!(VelaBootInfo, reserved), 16),
    ];
    let mut expected = 0usize;
    let mut holes: Vec<(usize, usize)> = Vec::new();
    for (offset, size) in fields {
        if *offset > expected {
            holes.push((expected, offset - expected));
            expected = *offset;
        }
        assert_eq!(*offset, expected, "a field moved");
        expected += size;
    }
    assert_eq!(expected, size_of::<VelaBootInfo>());
    // The single expected hole: four bytes before `acpi_rsdp_paddr`.
    assert_eq!(holes, vec![(76, 4)]);
}

/// The magic reads back as the ASCII bytes "VELABOOT" in memory order.
#[test]
fn the_magic_is_the_ascii_bytes_velopoot() {
    assert_eq!(&BOOT_MAGIC.to_le_bytes(), b"VELABOOT");
}

/// The ABI version is 3, and the version is what the structure's layout
/// implies.
///
/// Version 3 changed no layout — version 2 was the last change — so the
/// structure's size has been 216 bytes since version 2.
#[test]
fn the_abi_version_matches_the_structure() {
    assert_eq!(BOOT_ABI_VERSION, 3);
    // Version 2 grew the structure to 216 bytes; version 3 only changed the
    // meaning of the memory-map's type field.
    assert_eq!(size_of::<VelaBootInfo>(), 216);
}

/// A zeroed structure passes the identity check.
#[test]
fn a_zeroed_boot_info_passes_the_identity_check() {
    assert_eq!(VelaBootInfo::zeroed().check_identity(), Ok(()));
}

/// The zeroed structure reports its own size.
#[test]
fn the_zeroed_boot_info_reports_its_own_size() {
    let bi = VelaBootInfo::zeroed();
    assert_eq!(bi.struct_size, size_of::<VelaBootInfo>() as u32);
    assert_eq!(bi.abi_version, BOOT_ABI_VERSION);
    assert_eq!(bi.magic, BOOT_MAGIC);
}

/// A wrong magic is rejected.
#[test]
fn a_wrong_magic_is_rejected() {
    let mut bi = VelaBootInfo::zeroed();
    bi.magic = 0xDEAD_BEEF_DEAD_BEEF;
    assert_eq!(
        bi.check_identity(),
        Err(BootInfoError::BadMagic {
            got: 0xDEAD_BEEF_DEAD_BEEF
        })
    );
}

/// A future ABI version is rejected.
#[test]
fn a_future_abi_version_is_rejected() {
    let mut bi = VelaBootInfo::zeroed();
    bi.abi_version = BOOT_ABI_VERSION + 1;
    assert_eq!(
        bi.check_identity(),
        Err(BootInfoError::UnsupportedAbi {
            got: BOOT_ABI_VERSION + 1
        })
    );
}

/// A truncated structure is rejected: the kernel must not read past what the
/// loader actually wrote.
#[test]
fn a_truncated_structure_is_rejected() {
    let mut bi = VelaBootInfo::zeroed();
    bi.struct_size = 200;
    assert_eq!(
        bi.check_identity(),
        Err(BootInfoError::StructTooSmall {
            got: 200,
            need: 216
        })
    );
}

/// A larger structure from a newer loader is accepted, so a newer loader can
/// hand off to an older kernel.
#[test]
fn a_larger_structure_from_a_newer_loader_is_accepted() {
    let mut bi = VelaBootInfo::zeroed();
    bi.struct_size = 256;
    assert_eq!(bi.check_identity(), Ok(()));
}

/// The framebuffer geometry helpers agree with the fields they derive from.
#[test]
fn the_framebuffer_helpers_agree_with_the_fields() {
    let mut bi = VelaBootInfo::zeroed();
    bi.framebuffer_paddr = 0x8000_0000;
    bi.framebuffer_bytes = 1280 * 800 * 4;
    bi.framebuffer_width = 1280;
    bi.framebuffer_height = 800;
    bi.framebuffer_stride_bytes = 1280 * 4;
    bi.framebuffer_pixel_format = PixelFormat::Bgr as u32;
    assert!(bi.has_framebuffer());
    assert_eq!(bi.pixel_format(), Some(PixelFormat::Bgr));
    assert_eq!(bi.row_bytes(), 1280 * 4);
    assert_eq!(bi.surface_bytes(), 1280 * 4 * 800);
}

/// A padded stride is larger than the visible row, and the surface size
/// accounts for the padding.
#[test]
fn a_padded_stride_is_accounted_for() {
    let mut bi = VelaBootInfo::zeroed();
    bi.framebuffer_paddr = 0x8000_0000;
    bi.framebuffer_bytes = 4096 * 100;
    bi.framebuffer_width = 100;
    bi.framebuffer_height = 10;
    bi.framebuffer_stride_bytes = 4096;
    assert_eq!(bi.row_bytes(), 400);
    assert_eq!(bi.surface_bytes(), 40_960);
}

/// The memory-type numbering is the ABI's own, not the firmware's.
#[test]
fn the_memory_type_numbering_is_the_abis_own() {
    // The firmware's conventional memory is type 7; the ABI's is type 1.
    assert_eq!(mem_type::FIRMWARE_CONVENTIONAL, 7);
    assert_eq!(mem_type::CONVENTIONAL, 1);
    assert_eq!(mem_type::from_firmware(7), mem_type::CONVENTIONAL);
    assert_eq!(mem_type::from_firmware(0), mem_type::RESERVED);
    assert_eq!(mem_type::from_firmware(1), mem_type::LOADER_CODE);
    assert_eq!(mem_type::from_firmware(6), mem_type::RUNTIME_SERVICES_DATA);
}

/// Only the ordinary RAM classes are allocatable.
#[test]
fn only_the_ordinary_memory_classes_are_allocatable() {
    assert!(mem_type::is_allocatable(mem_type::CONVENTIONAL));
    assert!(mem_type::is_allocatable(mem_type::LOADER_CODE));
    assert!(mem_type::is_allocatable(mem_type::LOADER_DATA));
    assert!(mem_type::is_allocatable(mem_type::BOOT_SERVICES_CODE));
    assert!(mem_type::is_allocatable(mem_type::BOOT_SERVICES_DATA));
    assert!(!mem_type::is_allocatable(mem_type::RUNTIME_SERVICES_CODE));
    assert!(!mem_type::is_allocatable(mem_type::RUNTIME_SERVICES_DATA));
    assert!(!mem_type::is_allocatable(mem_type::UNUSABLE));
    assert!(!mem_type::is_allocatable(mem_type::MMIO));
}

/// The architecture flags are independent bits.
#[test]
fn the_architecture_flags_are_independent_bits() {
    let mut bi = VelaBootInfo::zeroed();
    assert!(!bi.arch_flag(arch_flags::MMU_ACTIVE));
    assert!(!bi.arch_flag(arch_flags::LONG_MODE));
    bi.arch_flags = arch_flags::LONG_MODE | arch_flags::GDT_PROVIDED;
    assert!(bi.arch_flag(arch_flags::LONG_MODE));
    assert!(bi.arch_flag(arch_flags::GDT_PROVIDED));
    assert!(!bi.arch_flag(arch_flags::MMU_ACTIVE));
    assert!(!bi.arch_flag(arch_flags::INTERRUPTS_ENABLED));
}
