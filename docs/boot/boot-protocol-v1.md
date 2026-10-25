# Boot protocol v1 — the loader/kernel handoff

> **Scope:** this document describes the handoff contract between the VelaOS
> UEFI loader and the VelaOS kernel on x86-64, as it is implemented **today**.
> Every field, offset and constant here was read out of
> `boot/boot-protocol/src/lib.rs`; nothing in it is aspirational.

---

## 1. Why there is a versioned structure at all

The loader and the kernel are **separate binaries**. They are built at
different times, they may be built from different checkouts, and a kernel
that is older than the loader it is booted by must not misinterpret what it
is handed.

The handoff therefore carries:

* a **magic** value, so a random pointer is rejected rather than
  dereferenced;
* an **ABI version**, so an incompatible layout is rejected rather than
  misread;
* a **structure size**, so a *newer* loader can hand off to an *older* kernel
  by appending fields, while an older loader cannot silently hand a short
  structure to a newer kernel.

The rules, restated from the crate's own documentation:

1. Every pointer-shaped field is a **physical address** — never a virtual
   address, and never a host pointer. The kernel establishes its own mapping
   before it dereferences any of them.
2. Any layout change bumps `BOOT_ABI_VERSION`. Fields are never silently
   redefined or repurposed.
3. `struct_size` lets a newer loader hand off to an older kernel. The kernel
   **rejects** a `struct_size` smaller than the fields it requires, and
   **accepts** a larger one.

---

## 2. The structure

`#[repr(C)]`, 216 bytes, 8-byte aligned, little-endian.

| offset | size | field | meaning |
|---|---|---|---|
| 0 | 8 | `magic` | must equal `BOOT_MAGIC` |
| 8 | 4 | `abi_version` | must equal `BOOT_ABI_VERSION` |
| 12 | 4 | `struct_size` | `size_of::<VelaBootInfo>()` as produced by the loader |
| 16 | 8 | `memory_map_paddr` | physical address of the memory-map descriptor buffer |
| 24 | 8 | `memory_map_bytes` | total byte length of the descriptor buffer |
| 32 | 4 | `memory_desc_size` | stride between descriptors, in bytes — **authoritative** |
| 36 | 4 | `memory_desc_version` | the firmware's `MemoryDescriptor` structure version |
| 40 | 8 | `framebuffer_paddr` | physical address of the linear framebuffer |
| 48 | 8 | `framebuffer_bytes` | framebuffer length in bytes |
| 56 | 4 | `framebuffer_width` | pixels |
| 60 | 4 | `framebuffer_height` | pixels |
| 64 | 4 | `framebuffer_stride_bytes` | bytes per scanline; often **greater** than `width * 4` |
| 68 | 4 | `framebuffer_pixel_format` | a `PixelFormat` discriminant as its `repr(u32)` value |
| 72 | 4 | `framebuffer_pixel_mask` | reserved; must be zero in v1 |
| 80 | 8 | `acpi_rsdp_paddr` | physical address of the ACPI RSDP, or 0 |
| 88 | 8 | `dtb_paddr` | physical address of a flattened device tree, or 0 |
| 96 | 8 | `kernel_paddr` | physical address of the loaded kernel image |
| 104 | 8 | `kernel_bytes` | kernel image length in bytes |
| 112 | 8 | `initrd_paddr` | physical address of the initrd, or 0 |
| 120 | 8 | `initrd_bytes` | initrd length in bytes, or 0 |
| 128 | 8 | `arch_flags` | bitwise OR of the `arch_flags` values |
| 136 | 8 | `stack_top` | virtual address of the kernel's initial stack top, or 0 |
| 144 | 8 | `page_table_paddr` | physical address of the loader's bootstrap PML4 |
| 152 | 8 | `kernel_vaddr_base` | the **linked** virtual base of the kernel image |
| 160 | 8 | `kernel_text_bytes` | length of the image's executable prefix, rounded up to a page |
| 168 | 8 | `heap_paddr` | physical base of the kernel heap the loader reserved |
| 176 | 8 | `heap_bytes` | size of the kernel heap, in bytes |
| 184 | 8 | `pool_paddr` | physical base of the loader's boot pool |
| 192 | 8 | `pool_bytes` | size of the loader's boot pool, in bytes |
| 200 | 16 | `reserved` | two `u64`s; must be zero |

There is **no implicit padding**: every gap between the fields above is
accounted for by the table, and the total is exactly 216 bytes.

### The constants

| constant | value | meaning |
|---|---|---|
| `BOOT_MAGIC` | `0x5654_4C41_424F_4F54` | the ASCII bytes `VELABOOT`, read as a little-endian integer |
| `BOOT_ABI_VERSION` | `3` | the current ABI version |

`BOOT_MAGIC` is defined as `u64::from_le_bytes(*b"VELABOOT")`, so a hex dump
of the structure in memory reads `VELABOOT` in its first eight bytes — which
is how a dump is inspected by eye.

---

## 3. Version history

| version | change |
|---|---|
| 1 | the original layout, 184 bytes |
| 2 | added `kernel_vaddr_base`, `kernel_text_bytes`, `heap_paddr`, `heap_bytes`, `pool_paddr` and `pool_bytes`; gave `page_table_paddr` a defined meaning. The structure grew to 216 bytes. |
| 3 | **no layout change.** The *meaning* of the memory-map buffer changed: the `type` field of each descriptor is now this ABI's own `mem_type` discriminant rather than the firmware's `EFI_MEMORY_TYPE` value. The loader translates on the way out, so the kernel sees one numbering regardless of what the firmware said. |

The crate's own documentation records a slightly different sentence about
version 2 ("grew from 184 to 216 bytes"), and the struct's doc comment says
"224 bytes". The **tested** value is 216, and 216 is what this document uses:
`tests/abi-layout/tests/boot_info_layout.rs` asserts it, and
`boot/boot-protocol/src/lib.rs`'s own unit test asserts it.

---

## 4. The memory-map numbering

The handoff carries its **own** memory-type numbering, and the loader
translates the firmware's into it while the map is copied into the boot pool.

| value | meaning | allocatable |
|---|---|---|
| 0 | reserved by the firmware | no |
| 1 | conventional memory — ordinary RAM | **yes** |
| 2 | the loader's own code | **yes** |
| 3 | the loader's own data | **yes** |
| 4 | boot-services code | **yes** |
| 5 | boot-services data | **yes** |
| 6 | runtime-services code | no |
| 7 | runtime-services data | no |
| 8 | unusable | no |
| 9 | ACPI reclaimable | no |
| 10 | ACPI memory (NVS) | no |
| 11 and above | device memory and firmware-specific types | no |

Types 2 through 5 are allocatable because the loader has finished with them
by the time the kernel runs; reclaiming them is what frees the boot pool.
Types 6 and 7 are not, because the firmware's runtime services are still
live — the kernel calls them.

### Why the translation exists

The firmware's numbering and the numbering a kernel wants to reason about are
not the same, and mixing them up is a **silent** failure. The firmware's
conventional memory is type `7`; this ABI's conventional memory is type
`1`. A table that believes `7` means "runtime-services data" concludes that
the machine has almost no RAM, and then reports that the largest usable
region is a few megabytes — true, and completely useless.

`mem_type::from_firmware` is a pure function of its input:

| firmware | this ABI |
|---|---|
| 0 | 0 (`RESERVED`) |
| 1 | 2 (`LOADER_CODE`) |
| 2 | 3 (`LOADER_DATA`) |
| 3 | 4 (`BOOT_SERVICES_CODE`) |
| 4 | 5 (`BOOT_SERVICES_DATA`) |
| 5 | 6 (`RUNTIME_SERVICES_CODE`) |
| 6 | 7 (`RUNTIME_SERVICES_DATA`) |
| 7 | 1 (`CONVENTIONAL`) |
| anything else | passed through unchanged |

The values the firmware defines but this ABI has no name for are all device
memory or firmware-specific types, and every one of them is non-allocatable
either way.

---

## 5. The pixel formats

`framebuffer_pixel_format` carries a `PixelFormat` discriminant as its
`repr(u32)` value.

| value | format |
|---|---|
| 0 | `Rgb` — red, green, blue, reserved |
| 1 | `Bgr` — blue, green, red, reserved |
| 2 | `RgbMask` — described by `framebuffer_pixel_mask` |
| 3 | `BLT_ONLY` — **rejected**; there is no addressable framebuffer |
| 4, 5 | the YUV formats — **rejected** |

`PixelFormat::from_raw` returns `None` for 3, 4, 5 and `0xFFFF_FFFF`. The
loader refuses a mode it cannot render directly, rather than handing the
kernel a framebuffer it cannot draw on.

---

## 6. The architecture flags

`arch_flags` is a bitwise OR of the values in `arch_flags`:

| bit | value | meaning |
|---|---|---|
| 0 | `1 << 0` | `MMU_ACTIVE` — the firmware left the MMU enabled with a usable mapping |
| 1 | `1 << 1` | `INTERRUPTS_ENABLED` — the firmware left interrupts unmasked |
| 2 | `1 << 2` | `LONG_MODE` — the firmware entered the loader in 64-bit long mode |
| 3 | `1 << 3` | `PROTECTED_MODE` — the firmware entered the loader in 32-bit protected mode |
| 4 | `1 << 4` | `GDT_PROVIDED` — a usable GDT with 64-bit code and data descriptors was provided |
| 5 | `1 << 5` | `PAGE_TABLES_PROVIDED` — a usable page-table base was provided |

The loader currently sets `arch_flags = arch_flags::PROTECTED_MODE` and
nothing else. The other bits are defined and tested but not yet claimed by
the loader.

---

## 7. The rejection rules

`VelaBootInfo::check_identity` checks exactly three things, and
deliberately does **not** validate the memory map or the framebuffer — the
kernel validates those itself, once it can report a useful error.

| condition | error |
|---|---|
| `magic != BOOT_MAGIC` | `BootInfoError::BadMagic { got }` |
| `abi_version != BOOT_ABI_VERSION` | `BootInfoError::UnsupportedAbi { got }` |
| `struct_size < 216` | `BootInfoError::StructTooSmall { got, need: 216 }` |

A `struct_size` **larger** than 216 is accepted: a newer loader may append
fields, and the older kernel ignores what it does not know.

---

## 8. The helper accessors

| method | meaning |
|---|---|
| `pixel_format()` | the `framebuffer_pixel_format` field as an `Option<PixelFormat>`, or `None` for a format VelaOS cannot render |
| `has_framebuffer()` | true when the framebuffer address, size, width and height are all non-zero |
| `arch_flag(flag)` | true when the bit is set |
| `row_bytes()` | `width * 4` — the **minimum** the renderer may write |
| `surface_bytes()` | `stride_bytes * height` — the **total** the renderer may touch, which exceeds `row_bytes * height` when the firmware pads scanlines |

The distinction between `row_bytes` and `surface_bytes` is the whole reason
`framebuffer_stride_bytes` is in the structure: a renderer that assumes
`stride == width * 4` draws a skewed image on a padded mode.

---

## 9. Who writes what

The loader writes the structure in `boot/uefi-loader/src/main.rs`, in this
order:

1. `VelaBootInfo::zeroed()` — every field starts at 0, which the kernel
   reads as "not provided".
2. The framebuffer geometry, read from GOP **before** `ExitBootServices`,
   because the GOP protocol ceases to exist afterwards.
3. The kernel's placement (`kernel_paddr`, `kernel_bytes`), the initrd, the
   stack top, and `arch_flags`.
4. The loader's reservations: `kernel_vaddr_base`, `kernel_text_bytes`,
   `heap_paddr`, `heap_bytes`, `pool_paddr`, `pool_bytes`.
5. The bootstrap page tables' PML4 address, in `page_table_paddr`.
6. **After** `ExitBootServices`, the structure is copied into a page the
   loader's own pool owns, and the memory-map fields are filled in.

Step 6 is the only moment the copy is legal, because the structure itself
lives in firmware memory the firmware may reclaim the moment Boot Services
end.

---

## 10. What the kernel does with it

In `kernel/entry/src/lib.rs`, `kernel_entry(boot_info_paddr)`:

1. Initialises the serial port and prints `VELA-KERNEL: entering`.
2. Dereferences the boot info **once** — paging is still off at that point,
   so the physical address is directly dereferenceable — and copies it out,
   because the page it lives in is inside the loader's pool and will be
   reclaimed.
3. Calls `check_identity()`. A failure prints `VELA-KERNEL: bad boot
   handoff:` and powers the machine off with a non-zero exit status.
4. Installs the heap from `heap_paddr`/`heap_bytes` **before** anything
   allocates, because parsing the memory map allocates a `Vec`.
5. Parses the memory map with `memory_desc_size` as the stride — never with
   `size_of::<MemoryDescriptor>()`.

The rest of the kernel's use of the structure is described in
[`entry-x86_64.md`](entry-x86_64.md).

---

## 11. The tests that hold this contract

| file | what it asserts |
|---|---|
| `boot/boot-protocol/src/lib.rs` (`mod tests`) | the size, the alignment and every field offset; the three rejection rules; the pixel-format decoding; that the stride can exceed `width * 4` |
| `tests/abi-layout/tests/boot_info_layout.rs` | the same layout, from outside the crate, so a change in the crate's own tests cannot hide a change in the layout |