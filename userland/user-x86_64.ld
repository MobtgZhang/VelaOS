/*
 * VelaOS userland layout.
 *
 * Every user program links at `IMAGE_BASE` in `kernel/syscall::userspace`,
 * far above the kernel's identity map of physical RAM. The image is laid out
 * as four page-aligned segments so the loader can map each one with its own
 * permissions:
 *
 *   .text    read + execute
 *   .rodata  read only
 *   .data    read + write
 *   .bss     read + write, zero-filled
 *
 * Page-aligning the segments is not cosmetic. The loader maps a segment a page
 * at a time and refuses a segment whose virtual address is not page-aligned; a
 * segment that shares a page with its neighbour would also force the two
 * segments' permissions to be merged, and a writable page containing read-only
 * data is a W^X hole.
 *
 * `ENTRY(_start)` names the Rust entry point the guest runtime provides, so the
 * ELF's entry address is the program's first instruction rather than an
 * arbitrary symbol.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x0000100000000000;

    .text : {
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata : {
        *(.rodata .rodata.*)
        *(.eh_frame_hdr)
        *(.eh_frame)
    }

    . = ALIGN(4096);
    .data : {
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.*)
        *(.data .data.*)
    }

    . = ALIGN(4096);
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
    }
}
