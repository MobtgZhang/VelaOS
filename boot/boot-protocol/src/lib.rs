//! VelaOS boot handoff ABI.
//!
//! This crate defines the versioned contract between the UEFI loader and the
//! kernel. It is deliberately free of any UEFI dependency so that the kernel
//! can consume it without linking firmware types, and so that host tests can
//! assert the layout without a firmware stub.
//!
//! # Contract rules
//!
//! 1. Every pointer-shaped field is a **physical address**, never a virtual one
//!    and never a host pointer. The kernel establishes its own mapping first.
//! 2. Any layout change bumps [`BOOT_ABI_VERSION`]. Fields are never silently
//!    redefined or repurposed.
//! 3. `struct_size` lets a newer loader hand off to an older kernel; the kernel
//!    must reject a `struct_size` smaller than the fields it requires.
//! 4. The memory map is walked using `memory_desc_size` as the stride. It is
//!    **not** guaranteed to equal `size_of::<MemoryDescriptor>()`: the firmware
//!    is allowed to append fields, and some firmware versions pad.

#![no_std]

use core::mem;

/// `0x56_45_4C_41_42_4F_4F_54` — the ASCII bytes of "VELABOOT" read as a
/// little-endian integer, which is how the value appears in a
/// little-endian memory dump.
pub const BOOT_MAGIC: u64 = u64::from_le_bytes(*b"VELABOOT");

/// ABI version. Bump on every incompatible layout change.
///
/// Version 2 added the `kernel_vaddr_base`, `kernel_text_bytes`, `heap_*` and
/// `pool_*` fields at the end of the structure, so the struct grew from 184
/// to 216 bytes.
///
/// Version 3 changed no layout, but changed the *meaning* of the memory-map
/// buffer: the `type` field of each descriptor is now the ABI's own
/// [`mem_type`] discriminant rather than the firmware's `EFI_MEMORY_TYPE`
/// value. The loader translates on the way out, so the kernel sees one
/// numbering regardless of what the firmware said.
pub const BOOT_ABI_VERSION: u32 = 3;

/// The memory-type discriminants the boot handoff uses.
///
/// # Why this is not the firmware's `EFI_MEMORY_TYPE`
///
/// The firmware's numbering and the numbering a kernel wants to reason about
/// are not the same thing, and mixing them up is a silent failure: the kernel
/// asks "is this region allocatable?", the firmware says "type 7", and a
/// table that believes type 7 is *runtime services data* concludes that the
/// machine has no ordinary RAM at all — and then reports that the largest
/// usable region is a few megabytes, which is true and completely useless.
///
/// So the boot handoff carries its **own** numbering, and the loader
/// translates the firmware's values into it as the map is copied into the
/// boot pool. The kernel therefore never has to know what the firmware's
/// numbers are.
///
/// The numbering is:
///
/// | value | meaning | allocatable |
/// |---|---|---|
/// | 0 | reserved by the firmware | no |
/// | 1 | conventional memory — ordinary RAM | **yes** |
/// | 2 | the loader's own code | **yes** |
/// | 3 | the loader's own data | **yes** |
/// | 4 | boot-services code | **yes** |
/// | 5 | boot-services data | **yes** |
/// | 6 | runtime-services code | no |
/// | 7 | runtime-services data | no |
/// | 8 | unusable | no |
/// | 9 | ACPI reclaimable | no |
/// | 10 | ACPI memory (NVS) | no |
/// | 11 and above | device memory and firmware-specific types | no |
///
/// Types 2 through 5 are allocatable because the loader has finished with
/// them by the time the kernel runs: reclaiming them is what frees the boot
/// pool. Type 6 and 7 are not, because the firmware's runtime services are
/// still live — the kernel calls them.
pub mod mem_type {
    /// Reserved by the firmware for itself.
    pub const RESERVED: u32 = 0;
    /// Ordinary RAM.
    pub const CONVENTIONAL: u32 = 1;
    /// Code belonging to the loader.
    pub const LOADER_CODE: u32 = 2;
    /// Data belonging to the loader.
    pub const LOADER_DATA: u32 = 3;
    /// Code belonging to the boot-services drivers.
    pub const BOOT_SERVICES_CODE: u32 = 4;
    /// Data belonging to the boot-services drivers.
    pub const BOOT_SERVICES_DATA: u32 = 5;
    /// Code belonging to the firmware's runtime services.
    pub const RUNTIME_SERVICES_CODE: u32 = 6;
    /// Data belonging to the firmware's runtime services.
    pub const RUNTIME_SERVICES_DATA: u32 = 7;
    /// Memory the firmware must not let anyone touch.
    pub const UNUSABLE: u32 = 8;
    /// Memory the firmware may reclaim after the ACPI tables are read.
    pub const ACPI_RECLAIMABLE: u32 = 9;
    /// ACPI non-volatile storage, which must survive until the kernel reads it.
    pub const ACPI_MEMORY_NVS: u32 = 10;
    /// Device memory.
    pub const MMIO: u32 = 11;

    /// The firmware's `EFI_MEMORY_TYPE` value for conventional memory.
    ///
    /// Named separately because it is the value that is most often got wrong:
    /// it is `7` in the firmware's numbering and `1` in this one.
    pub const FIRMWARE_CONVENTIONAL: u32 = 7;

    /// Translate a firmware `EFI_MEMORY_TYPE` into this ABI's numbering.
    ///
    /// The mapping is a pure function of the input, so it can be applied to
    /// every descriptor of the memory map without consulting any state.
    ///
    /// Values the firmware defines but this ABI has no name for are passed
    /// through unchanged: they are all device memory or firmware-specific
    /// types, and every one of them is non-allocatable either way. The one
    /// thing that matters is that the *allocatable* classes land on the
    /// values the kernel reasons about, and that `7` — the firmware's
    /// conventional memory, which is most of the machine — becomes `1`.
    pub const fn from_firmware(kind: u32) -> u32 {
        match kind {
            0 => RESERVED,
            // The firmware's loader code and data are the ABI's loader code
            // and data, one class lower because this ABI reserves 1 for
            // conventional memory.
            1 => LOADER_CODE,
            2 => LOADER_DATA,
            3 => BOOT_SERVICES_CODE,
            4 => BOOT_SERVICES_DATA,
            5 => RUNTIME_SERVICES_CODE,
            6 => RUNTIME_SERVICES_DATA,
            // The important one: the firmware's conventional memory is the
            // ABI's conventional memory.
            7 => CONVENTIONAL,
            other => other,
        }
    }

    /// True when a region of this type may be allocated from.
    ///
    /// Ordinary RAM, and the code and data the loader used — which the loader
    /// has finished with by the time the kernel runs.
    ///
    /// Everything else is excluded: the firmware's runtime services are still
    /// live, the ACPI tables must survive until the kernel reads them, and
    /// the `MMIO` family holds device windows that must never be handed to a
    /// page table.
    pub const fn is_allocatable(kind: u32) -> bool {
        matches!(
            kind,
            CONVENTIONAL | LOADER_CODE | LOADER_DATA | BOOT_SERVICES_CODE | BOOT_SERVICES_DATA
        )
    }
}

/// Kernel release string, mirrored from the `v0.1.0` plan.
pub const KERNEL_VERSION: &str = "VelaOS 0.1.0";

/// Framebuffer pixel formats the loader may report.
///
/// Mirrors the subset of UEFI `EFI_GRAPHICS_PIXEL_FORMAT` that VelaOS renders.
/// `BltOnly` is deliberately absent: it means the mode has no directly
/// addressable framebuffer, so the loader must refuse to report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PixelFormat {
    /// Red, Green, Blue, Reserved — 32 bits per pixel.
    Rgb = 0,
    /// Blue, Green, Red, Reserved — 32 bits per pixel.
    Bgr = 1,
    /// Bit mask layout, described by `framebuffer_pixel_mask`.
    RgbMask = 2,
}

impl PixelFormat {
    /// Decode a raw UEFI pixel-format value.
    ///
    /// Returns `None` for modes VelaOS cannot render directly, including
    /// `BLT_ONLY` (3) and the YUV formats (4..=5).
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Rgb),
            1 => Some(Self::Bgr),
            2 => Some(Self::RgbMask),
            _ => None,
        }
    }
}

/// Architecture flags in [`VelaBootInfo::arch_flags`].
pub mod arch_flags {
    /// Firmware already left the MMU enabled with a usable mapping.
    pub const MMU_ACTIVE: u64 = 1 << 0;
    /// Firmware already left interrupts unmasked.
    pub const INTERRUPTS_ENABLED: u64 = 1 << 1;
    /// Firmware entered the loader in 64-bit long mode.
    pub const LONG_MODE: u64 = 1 << 2;
    /// Firmware entered the loader in 32-bit protected mode.
    pub const PROTECTED_MODE: u64 = 1 << 3;
    /// A GDT with usable 64-bit code and data descriptors was provided.
    pub const GDT_PROVIDED: u64 = 1 << 4;
    /// A usable page table base was provided in `arch_flags`' sibling field.
    pub const PAGE_TABLES_PROVIDED: u64 = 1 << 5;
}

/// The boot handoff structure, version 2.
///
/// Laid out `#[repr(C)]` with explicit padding. All fields are little-endian,
/// which matches every architecture VelaOS v0.1.0 targets.
///
/// # Version history
///
/// * **Version 1** — the original layout, 184 bytes.
/// * **Version 2** — added [`Self::kernel_vaddr_base`], [`Self::heap_paddr`],
///   [`Self::heap_bytes`], [`Self::pool_paddr`] and [`Self::pool_bytes`], and
///   gave [`Self::page_table_paddr`] a defined meaning. The struct is now
///   224 bytes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct VelaBootInfo {
    /// Must equal [`BOOT_MAGIC`].
    pub magic: u64,
    /// Must equal [`BOOT_ABI_VERSION`].
    pub abi_version: u32,
    /// `size_of::<VelaBootInfo>()` as produced by the loader.
    pub struct_size: u32,

    // --- memory map ---
    /// Physical address of the memory-map descriptor buffer.
    pub memory_map_paddr: u64,
    /// Total byte length of the descriptor buffer.
    pub memory_map_bytes: u64,
    /// Stride between descriptors, in bytes. **Authoritative.**
    pub memory_desc_size: u32,
    /// Firmware's `MemoryDescriptor` structure version.
    pub memory_desc_version: u32,

    // --- framebuffer ---
    /// Physical address of the linear framebuffer.
    pub framebuffer_paddr: u64,
    /// Framebuffer length in bytes.
    pub framebuffer_bytes: u64,
    pub framebuffer_width: u32,
    pub framebuffer_height: u32,
    /// Bytes per scanline. Frequently **greater** than
    /// `width * 4`; the renderer must honour it.
    pub framebuffer_stride_bytes: u32,
    /// A [`PixelFormat`] discriminant as its `repr(u32)` value.
    pub framebuffer_pixel_format: u32,
    /// Reserved; must be zero in v1.
    pub framebuffer_pixel_mask: u32,

    // --- firmware tables ---
    /// Physical address of the ACPI RSDP, or 0 if unavailable.
    pub acpi_rsdp_paddr: u64,
    /// Physical address of a flattened device tree, or 0 if unavailable.
    pub dtb_paddr: u64,

    // --- bundled payloads ---
    /// Physical address of the loaded kernel image.
    pub kernel_paddr: u64,
    /// Kernel image length in bytes.
    pub kernel_bytes: u64,
    /// Physical address of the initrd, or 0 if none.
    pub initrd_paddr: u64,
    /// Initrd length in bytes, or 0.
    pub initrd_bytes: u64,

    // --- handoff state ---
    /// Bitwise OR of [`arch_flags`] values.
    pub arch_flags: u64,
    /// Virtual address of the kernel's initial stack top, or 0 to let the
    /// kernel choose from the memory map.
    pub stack_top: u64,
    /// Physical address of a firmware-provided page table, or 0.
    ///
    /// Version 2: the loader always fills this in, with the physical address
    /// of the bootstrap PML4 it built before `ExitBootServices`. The kernel
    /// loads it into CR3 as its first action, which is what makes the
    /// higher-half kernel address reachable at all — the firmware's own
    /// tables do not map the higher half.
    pub page_table_paddr: u64,

    // --- version 2: the loader's reservations ---
    //
    // The loader takes one contiguous pool before `ExitBootServices` and
    // sub-allocates the kernel image, the stacks, the memory-map copy and the
    // kernel heap out of it. The kernel needs to know where that pool is for
    // two reasons: the heap must be mapped at the *same* virtual and physical
    // address (that is what makes translating a heap pointer into a physical
    // address honest), and the pool's frames must never be handed out by the
    // frame allocator.
    /// The linked virtual base of the kernel image.
    ///
    /// The lowest `PT_LOAD` virtual address, which is where the image is
    /// *linked* — and therefore where it must be *mapped*. The physical
    /// placement is [`Self::kernel_paddr`] and is unrelated to it.
    pub kernel_vaddr_base: u64,
    /// The length of the image's executable prefix, in bytes, rounded up to a
    /// page.
    ///
    /// The kernel maps `[kernel_vaddr_base, kernel_vaddr_base +
    /// kernel_text_bytes)` read-execute and everything after it
    /// read-write. The split exists so that W^X can be enforced on the
    /// kernel's own image: the loader knows which segments are executable and
    /// the kernel does not parse its own ELF.
    pub kernel_text_bytes: u64,
    /// Physical base of the kernel heap the loader reserved.
    pub heap_paddr: u64,
    /// Size of the kernel heap, in bytes.
    pub heap_bytes: u64,
    /// Physical base of the loader's boot pool.
    pub pool_paddr: u64,
    /// Size of the loader's boot pool, in bytes.
    pub pool_bytes: u64,

    /// Reserved; must be zero.
    pub reserved: [u64; 2],
}

/// Reasons [`VelaBootInfo::check_identity`] rejects a handoff.
impl VelaBootInfo {
    /// Build a zeroed, version-2 instance.
    ///
    /// Every field other than the identity fields starts at 0, which the
    /// kernel reads as "not provided". Used by the loader as its starting
    /// state and by tests as a base to mutate.
    pub const fn zeroed() -> Self {
        Self {
            magic: BOOT_MAGIC,
            abi_version: BOOT_ABI_VERSION,
            struct_size: mem::size_of::<Self>() as u32,
            memory_map_paddr: 0,
            memory_map_bytes: 0,
            memory_desc_size: 0,
            memory_desc_version: 0,
            framebuffer_paddr: 0,
            framebuffer_bytes: 0,
            framebuffer_width: 0,
            framebuffer_height: 0,
            framebuffer_stride_bytes: 0,
            framebuffer_pixel_format: 0,
            framebuffer_pixel_mask: 0,
            acpi_rsdp_paddr: 0,
            dtb_paddr: 0,
            kernel_paddr: 0,
            kernel_bytes: 0,
            initrd_paddr: 0,
            initrd_bytes: 0,
            arch_flags: 0,
            stack_top: 0,
            page_table_paddr: 0,
            kernel_vaddr_base: 0,
            kernel_text_bytes: 0,
            heap_paddr: 0,
            heap_bytes: 0,
            pool_paddr: 0,
            pool_bytes: 0,
            reserved: [0; 2],
        }
    }

    /// Check the identity fields the kernel depends on before touching
    /// anything else.
    ///
    /// This deliberately does **not** validate the memory map or framebuffer;
    /// the kernel validates those itself once it can report a useful error.
    pub fn check_identity(&self) -> Result<(), BootInfoError> {
        if self.magic != BOOT_MAGIC {
            return Err(BootInfoError::BadMagic { got: self.magic });
        }
        if self.abi_version != BOOT_ABI_VERSION {
            return Err(BootInfoError::UnsupportedAbi {
                got: self.abi_version,
            });
        }
        let required = mem::size_of::<Self>() as u32;
        if self.struct_size < required {
            return Err(BootInfoError::StructTooSmall {
                got: self.struct_size,
                need: required,
            });
        }
        Ok(())
    }

    /// Interpret [`Self::framebuffer_pixel_format`].
    pub fn pixel_format(&self) -> Option<PixelFormat> {
        PixelFormat::from_raw(self.framebuffer_pixel_format)
    }

    /// True when the loader reported a directly addressable framebuffer.
    pub fn has_framebuffer(&self) -> bool {
        self.framebuffer_paddr != 0
            && self.framebuffer_bytes != 0
            && self.framebuffer_width != 0
            && self.framebuffer_height != 0
    }

    /// True when an arch flag bit is set.
    pub const fn arch_flag(&self, flag: u64) -> bool {
        self.arch_flags & flag != 0
    }

    /// Bytes needed for one full scanline of pixel data, i.e. the minimum the
    /// renderer may write.
    pub const fn row_bytes(&self) -> u64 {
        self.framebuffer_width as u64 * 4
    }

    /// Total bytes the renderer may touch: `stride * height`, which can exceed
    /// `row_bytes * height` when the firmware pads scanlines.
    pub const fn surface_bytes(&self) -> u64 {
        self.framebuffer_stride_bytes as u64 * self.framebuffer_height as u64
    }
}

/// Reasons [`VelaBootInfo::check_identity`] rejects a handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootInfoError {
    /// `magic` did not match [`BOOT_MAGIC`].
    BadMagic { got: u64 },
    /// `abi_version` is not [`BOOT_ABI_VERSION`].
    UnsupportedAbi { got: u32 },
    /// `struct_size` is too small for the fields this kernel requires.
    StructTooSmall { got: u32, need: u32 },
}

impl core::fmt::Display for BootInfoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadMagic { got } => {
                write!(f, "bad boot magic {got:#018x}, expected {BOOT_MAGIC:#018x}")
            }
            Self::UnsupportedAbi { got } => {
                write!(
                    f,
                    "unsupported boot ABI version {got}, expected {BOOT_ABI_VERSION}"
                )
            }
            Self::StructTooSmall { got, need } => {
                write!(
                    f,
                    "BootInfo struct_size {got} is smaller than required {need}"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn layout_is_stable() {
        // Guard the ABI: these offsets are baked into the loader and the
        // kernel. A change here requires a BOOT_ABI_VERSION bump.
        assert_eq!(size_of::<VelaBootInfo>(), 216);
        assert_eq!(align_of::<VelaBootInfo>(), 8);

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

    #[test]
    fn no_implicit_padding() {
        // Every gap must be intentional. `struct_size` already covers the
        // struct, but a hole between a u32 pair and the next u64 would mean
        // silent padding that the ABI does not describe.
        assert_eq!(size_of::<VelaBootInfo>(), 216);
    }

    #[test]
    fn zeroed_passes_identity() {
        assert_eq!(VelaBootInfo::zeroed().check_identity(), Ok(()));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bi = VelaBootInfo::zeroed();
        bi.magic = 0xdead_beef;
        assert_eq!(
            bi.check_identity(),
            Err(BootInfoError::BadMagic { got: 0xdead_beef })
        );
    }

    #[test]
    fn rejects_future_abi() {
        let mut bi = VelaBootInfo::zeroed();
        bi.abi_version = BOOT_ABI_VERSION + 1;
        assert_eq!(
            bi.check_identity(),
            Err(BootInfoError::UnsupportedAbi {
                got: BOOT_ABI_VERSION + 1
            })
        );
    }

    #[test]
    fn rejects_truncated_struct() {
        let mut bi = VelaBootInfo::zeroed();
        bi.struct_size = 8;
        assert_eq!(
            bi.check_identity(),
            Err(BootInfoError::StructTooSmall { got: 8, need: 216 })
        );
    }

    #[test]
    fn accepts_larger_struct_from_newer_loader() {
        // Forward compatibility: a newer loader may append fields.
        let mut bi = VelaBootInfo::zeroed();
        bi.struct_size = 256;
        assert_eq!(bi.check_identity(), Ok(()));
    }

    #[test]
    fn pixel_format_decoding() {
        assert_eq!(PixelFormat::from_raw(0), Some(PixelFormat::Rgb));
        assert_eq!(PixelFormat::from_raw(1), Some(PixelFormat::Bgr));
        assert_eq!(PixelFormat::from_raw(2), Some(PixelFormat::RgbMask));
        // BLT_ONLY has no linear framebuffer.
        assert_eq!(PixelFormat::from_raw(3), None);
        assert_eq!(PixelFormat::from_raw(4), None);
        assert_eq!(PixelFormat::from_raw(0xFFFF_FFFF), None);
    }

    #[test]
    fn stride_can_exceed_width_times_four() {
        let mut bi = VelaBootInfo::zeroed();
        bi.framebuffer_paddr = 0x8000_0000;
        bi.framebuffer_bytes = 4096 * 100;
        bi.framebuffer_width = 100;
        bi.framebuffer_height = 10;
        bi.framebuffer_stride_bytes = 4096;
        assert!(bi.has_framebuffer());
        assert_eq!(bi.row_bytes(), 400);
        // The renderer is allowed to touch the padded span, not just 4000 bytes.
        assert_eq!(bi.surface_bytes(), 40_960);
    }

    #[test]
    fn no_framebuffer_when_unset() {
        let bi = VelaBootInfo::zeroed();
        assert!(!bi.has_framebuffer());
    }

    #[test]
    fn arch_flags_are_independent_bits() {
        let mut bi = VelaBootInfo::zeroed();
        assert!(!bi.arch_flag(arch_flags::LONG_MODE));
        bi.arch_flags = arch_flags::LONG_MODE | arch_flags::GDT_PROVIDED;
        assert!(bi.arch_flag(arch_flags::LONG_MODE));
        assert!(bi.arch_flag(arch_flags::GDT_PROVIDED));
        assert!(!bi.arch_flag(arch_flags::MMU_ACTIVE));
    }

    #[test]
    fn magic_is_the_ascii_bytes_velopoot() {
        // The magic must read back as "VELABOOT" when the structure is dumped
        // in memory order, because that is how a hex dump is inspected.
        assert_eq!(&BOOT_MAGIC.to_le_bytes(), b"VELABOOT");
    }
}
