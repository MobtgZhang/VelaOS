/* VelaOS kernel link script.
 *
 * The kernel is a higher-half kernel: it is *linked* to run at
 * `0xFFFF_8000_0000_0000` but the UEFI loader places it at a low physical
 * address and the kernel maps it into the higher half itself. This script
 * therefore does two things that a default link would not:
 *
 *  1. It sets the default load address to the higher half, so every symbol the
 *     kernel computes — a static's address, a string literal's address, a
 *     function pointer stored in a table — is already a higher-half address.
 *     Without this the kernel would be full of low addresses that become
 *     invalid the instant it switches page tables.
 *
 *  2. It places the ELF entry point at `kernel_entry`, which is the symbol the
 *     loader jumps to. `ENTRY` also keeps the linker from warning about a
 *     missing `_start`, which a `no_main` binary genuinely does not have.
 *
 * # The virtual/physical split
 *
 * `.` is the *virtual* location counter; `AT(...)` overrides the *physical*
 * (load) address. The kernel image therefore occupies the same offsets in both
 * spaces, which is what lets the loader read the physical side and the kernel
 * map it at the virtual side with a single linear mapping.
 *
 * # Why `.data` is its own output section
 *
 * This is the reason the script does not put everything in one section, and
 * it is a W^X requirement rather than a matter of taste.
 *
 * `.text` and `.data` cannot share a *page* if the text is read-execute and
 * the data is read-write: a page is either executable or it is writable, and
 * a page that is both would be a hole in W^X. If `.data` and `.got` lived
 * inside the `.text` output section — as an earlier version of this script
 * had them — then the loader could only see one `PT_LOAD` segment covering
 * both, and it could not tell where the executable part ends. It would have
 * to map the whole segment as one thing, and the kernel's own mutable
 * statics (the GDT, for one) would end up in a read-only page.
 *
 * Splitting them into separate output sections gives the linker no choice:
 * it emits a separate `PT_LOAD` for each, with the flags each one deserves.
 * The loader then computes the executable prefix from the *segments* rather
 * than guessing, and the kernel maps the prefix read-execute and the
 * remainder read-write.
 *
 * The boundary is page-aligned because `.data` is `ALIGN(4K)`: the linker
 * pads `.text` up to the page boundary, so no page holds both code and data.
 *
 * `.bss` has no file extent, so it needs no `AT()` of its own beyond keeping
 * the physical counter moving in step with the virtual one. Giving it an
 * explicit `AT()` that restarts from `KERNEL_PHYS_BASE` would make the two
 * ranges overlap, and the linker correctly refuses it.
 */

ENTRY(kernel_entry)

/* The higher half. Must match `kernel/entry`'s `KERNEL_VIRT_BASE`. */
KERNEL_BASE = 0xFFFF800000000000;

/* Where the loader places the kernel image physically. */
KERNEL_PHYS_BASE = 0x0000000001000000;

/* The page offset of the first section. Page 0 of the window holds the ELF
 * program headers, which are never mapped. */
IMAGE_OFFSET = 0x1000;

SECTIONS
{
    . = KERNEL_BASE + IMAGE_OFFSET;

    /* The executable image: text and read-only data.
     *
     * `.rodata` is here rather than in `.data` because it is read-only, and
     * the linker is free to give it the same permissions as the code it sits
     * beside. */
    .text ALIGN(4K) : AT(KERNEL_PHYS_BASE + IMAGE_OFFSET)
    {
        *(.text .text.*)
        *(.rodata .rodata.*)

        /* The entry-point anchor: a pointer to `kernel_entry` in a section
         * with no other purpose. It exists so `--gc-sections` cannot collect
         * the function the loader jumps to, since nothing *calls* it.
         *
         * It lives *inside* `.text` rather than in a sibling output section
         * because a sibling section with no `AT()` inherits the physical
         * location counter from the preceding section, and the linker then
         * sees two sections claiming the same physical bytes. Placing it
         * inside `.text` makes its load address unambiguous by construction. */
        . = ALIGN(8);
        KEEP(*(.vela.anchor))
    }

    /* Initialised data and the global offset table.
     *
     * Separate from `.text` so that it can be mapped read-write while the
     * text is mapped read-execute. See the header comment. */
    .data ALIGN(4K) : AT(LOADADDR(.text) + SIZEOF(.text))
    {
        *(.data .data.*)
        *(.got .got.plt)
    }

    /* Zero-initialised statics. Not loaded from the image; the loader zeroes
     * them, so they contribute only to the memory size. */
    .bss ALIGN(4K) : AT(LOADADDR(.data) + SIZEOF(.data))
    {
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4K);
    }

    /* The end of the image. The loader reads this to learn how much physical
     * memory the kernel occupies, and the kernel uses it to map itself. */
    _kernel_image_end = .;

    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
        *(.debug_*)
    }
}
