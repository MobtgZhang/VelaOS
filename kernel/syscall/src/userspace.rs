//! User address spaces: layout, loading, and entering ring 3.
//!
//! # What this module is for
//!
//! Phase 4 built the *pieces* a user process needs — page tables that can hold
//! ring-3 pages, a syscall gate, a scheduler with tasks that carry a CR3. But
//! the pieces are inert until something connects them: nothing created a user
//! address space, nothing loaded a user program into one, and nothing ever
//! lowered privilege. This module is that connection, and it is the smallest
//! thing that makes "user space" mean something rather than being a claim.
//!
//! # The layout
//!
//! ```text
//! 0x0000_0000_0000_0000  null guard — never mapped, so a null deref faults
//! 0x0000_1000_0000_0000  program image: text, rodata, data, bss
//! 0x0000_4000_0000_0000  the break: the heap grows up from here
//! 0x0000_7FFF_FFFF_F000  initial user stack top, growing down
//! 0x0000_8000_0000_0000  USER_SPACE_END — the kernel half begins
//! 0xFFFF_8000_0000_0000  kernel image (inherited from the kernel space)
//! ```
//!
//! Three constraints shaped the numbers, and each is a real constraint rather
//! than a round number chosen for looks:
//!
//! * **The image starts at 16 TiB.** A user space inherits the kernel's
//!   supervisor identity map, which occupies the virtual addresses equal to
//!   physical RAM. A program placed inside RAM would collide with it, so the
//!   image lives above every physical window.
//! * **The break starts at 64 TiB.** It has to clear the image, and it has to
//!   leave room below the stack for a program whose data grows. The gap is
//!   more than any v0.1.0 program needs and keeps the arithmetic in the page
//!   mapper simple.
//! * **The stack top is just under the 128 TiB user ceiling.** The ceiling is
//!   `USER_SPACE_END` from `kernel/syscall::copy`, which is the same bound the
//!   user-copy helpers validate against. If the two disagreed, a program could
//!   map memory the kernel refused to copy to — a hole in the validation.
//!
//! Address zero is still an unmapped null guard: a null or small-offset
//! pointer faults immediately instead of reading whatever the program happened
//! to put at its lowest address.
//!
//! # Why the loader is a separate step from the elf-loader crate
//!
//! `boot/elf-loader` parses and *validates* an ELF, and is deliberately free of
//! any knowledge of address spaces: it is called by the UEFI loader, before
//! paging exists. It relocates segments to physical addresses it is told to
//! use. That is the right job for a bootloader, and it is the wrong job here.
//!
//! Loading into a user process means choosing virtual addresses per segment,
//! honouring the segment's own permissions, zeroing `.bss`, and never letting
//! a program's segment land anywhere but the image region. So the parse comes
//! from `vela_elf_loader` and the placement is decided here, where the layout
//! above is in scope. The alternative — extending the bootloader's relocation
//! API with a second calling convention — would put user-space policy into a
//! crate whose entire value is that it has none.
//!
//! # W^X and the entry point
//!
//! Text is mapped `user_rx` and data `user_rw`, and the W^X rule in the page
//! mapper means a page can never be both writable and executable. The user
//! entry point is therefore called as code, and `.data` is never something the
//! CPU can jump into.

use alloc::vec::Vec;

use vela_elf_loader::{ph_flags, Elf64Image, ElfHeader, LoadedSegment, EM_X86_64, ET_EXEC};
use vela_hal_common::{MapError, PageFlags, PAGE_SIZE};
use vela_hal_x86_64::addrspace::AddressSpace;
use vela_hal_x86_64::regs;

use crate::task_space::FrameSource;
pub use crate::task_space::{
    TaskSpace, BREAK_LIMIT, BREAK_START, IMAGE_BASE, USER_SPACE_END, USER_STACK_SIZE,
    USER_STACK_TOP,
};

/// Why a user program could not be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadError {
    /// The ELF could not be parsed.
    Malformed,
    /// The ELF was not the right class, machine or type.
    WrongFormat,
    /// A segment's virtual address fell outside the image region.
    SegmentOutOfRange,
    /// A segment's file extent ran past the end of the image.
    SegmentTruncated,
    /// Two segments overlapped in the image.
    SegmentOverlap,
    /// The image had no loadable segment, so it had no entry point.
    NoLoadableSegment,
    /// The physical frame allocator refused a frame.
    OutOfMemory,
    /// The page mapper refused a mapping.
    Map(MapError),
    /// The entry point did not land inside a mapped segment.
    EntryPointUnmapped,
}

impl From<MapError> for LoadError {
    fn from(e: MapError) -> Self {
        LoadError::Map(e)
    }
}

impl core::fmt::Display for LoadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Malformed => f.write_str("the image is not a valid ELF64 file"),
            Self::WrongFormat => {
                f.write_str("the image is not a native64 executable for this machine")
            }
            Self::SegmentOutOfRange => f.write_str("a segment falls outside the image region"),
            Self::SegmentTruncated => f.write_str("a segment runs past the end of the image"),
            Self::SegmentOverlap => f.write_str("two segments overlap in the image"),
            Self::NoLoadableSegment => f.write_str("the image has no loadable segment"),
            Self::OutOfMemory => f.write_str("the physical frame allocator is exhausted"),
            Self::Map(e) => write!(f, "the page mapper refused a mapping: {e}"),
            Self::EntryPointUnmapped => {
                f.write_str("the entry point is not inside any mapped segment")
            }
        }
    }
}

/// Where a program's image ended up, and where to start it.
#[derive(Debug, Clone, Copy)]
pub struct LoadedImage {
    /// The address of the first byte of the image.
    pub entry: u64,
    /// The highest user virtual address the image occupies, exclusive.
    ///
    /// This is the starting break: the heap begins where the image ends, so a
    /// program can never allocate over its own data.
    pub brk: u64,
}

/// One loadable segment of a user program, as placed in the address space.
#[derive(Debug, Clone, Copy)]
struct Placement {
    /// First user virtual address of the segment.
    va: u64,
    /// Length of the segment in memory, which may exceed its file bytes
    /// because `.bss` has no file extent.
    mem_len: u64,
    /// The flags to map it with.
    flags: PageFlags,
}

/// Decide where each loadable segment goes, without touching memory.
///
/// Splitting placement from loading is not ceremony: the placement rules are
/// where every interesting rejection lives (out of range, overlapping, no
/// entry point), and they are pure arithmetic over the ELF's headers. Testing
/// them needs no allocator and no page tables, so they are tested directly.
fn plan_placements(
    segments: &[LoadedSegment],
    entry: u64,
) -> Result<Vec<(Placement, usize)>, LoadError> {
    let mut placed: Vec<(Placement, usize)> = Vec::new();
    // The image region ends where the break starts, so an image can never
    // overlap the heap. The gap between them is deliberate; see the module
    // documentation.
    let image_end = BREAK_START;

    for (i, h) in segments.iter().enumerate() {
        if h.mem_size == 0 && h.file_size == 0 {
            continue;
        }
        if h.mem_size == 0 {
            // A file extent with no memory extent is meaningless; treating it
            // as a no-op would silently drop a segment the program relies on.
            return Err(LoadError::SegmentTruncated);
        }
        if h.file_size > h.mem_size {
            // `.bss` is the other way round: more memory than file. Fewer is
            // the case that cannot be represented.
            return Err(LoadError::SegmentTruncated);
        }

        let va = h.vaddr;
        if va < IMAGE_BASE || va.checked_add(h.mem_size).is_none() {
            return Err(LoadError::SegmentOutOfRange);
        }
        if va >= image_end || va + h.mem_size > image_end {
            return Err(LoadError::SegmentOutOfRange);
        }

        // W^X: anything with a file extent is program content. Mapping it
        // writable would let a program rewrite its own text, which is both a
        // security hole and a surprise. Nothing in a native image needs a
        // writable text segment, so the rule is uniform.
        let flags = if h.flags & ph_flags::EXEC != 0 {
            PageFlags::user_rx()
        } else {
            PageFlags::user_rw()
        };

        let p = Placement {
            va,
            mem_len: h.mem_size,
            flags,
        };
        // Overlap check against everything already placed.
        for (other, _) in &placed {
            let disjoint = p.va + p.mem_len <= other.va || other.va + other.mem_len <= p.va;
            if !disjoint {
                return Err(LoadError::SegmentOverlap);
            }
        }
        placed.push((p, i));
    }

    if placed.is_empty() {
        return Err(LoadError::NoLoadableSegment);
    }

    // The entry point must be inside a mapped segment. A program whose entry
    // point is in the gap between segments would fault on its first
    // instruction, which is a much worse failure than a load-time error.
    let entry_in_image = placed
        .iter()
        .any(|(p, _)| entry >= p.va && entry < p.va + p.mem_len);
    if !entry_in_image {
        return Err(LoadError::EntryPointUnmapped);
    }

    Ok(placed)
}

/// Load a user program into a fresh address space, ready to run.
///
/// The returned space has the image mapped with its W^X permissions, the
/// user stack mapped, and its break set to the image's end — so the caller
/// only has to register it, activate it, and build the entry context. The
/// stack is the loader's job rather than the caller's because a space whose
/// program cannot take a syscall without faulting is not a load that
/// succeeded; it is a load that has not finished.
///
/// # Safety
///
/// `frames` must be the kernel's frame allocator, and `image` must be a
/// complete ELF file — its bytes are copied into the address space, so it
/// does not need to outlive the call.
pub unsafe fn load_user_program(
    image: &[u8],
    frames: &dyn FrameSource,
) -> Result<(TaskSpace, LoadedImage), LoadError> {
    // The header is parsed separately so a wrong-format image is reported as
    // such rather than as a generic malformation. `Elf64Image::parse` would
    // reject it for us, but the caller wants to know *why* it was refused.
    let header = ElfHeader::parse(image).map_err(|_| LoadError::Malformed)?;
    if header.e_type != ET_EXEC || header.e_machine != EM_X86_64 {
        return Err(LoadError::WrongFormat);
    }
    let elf = Elf64Image::parse(image).map_err(|_| LoadError::Malformed)?;
    let entry = elf.entry();
    let segments = &elf.load_segments;
    let placed = plan_placements(segments, entry)?;

    // The heap starts where the image ended, so an allocation can never land
    // on top of the program's own data. The break is decided *before* the
    // space is built, because the break's floor is part of the space: a
    // `brk` that reaches down into the image must be refused, and the refusal
    // is only possible if the floor is known when the space is created.
    let image_end = placed
        .iter()
        .map(|(p, _)| p.va + p.mem_len)
        .max()
        .unwrap_or(BREAK_START);
    let brk = image_end.next_multiple_of(PAGE_SIZE).max(BREAK_START);
    let space = TaskSpace::with_break(brk);

    for (placement, index) in &placed {
        let h = &segments[*index];
        // Map the segment's pages first.
        let pages = (placement.mem_len as usize).div_ceil(PAGE_SIZE as usize);
        let mut va = placement.va;
        for _ in 0..pages {
            // SAFETY: `frames` is the kernel's allocator per this function's
            // contract, and `va` is inside the user range because the placement
            // plan proved the whole segment is.
            space.map_user_page_with(va, frames, placement.flags)?;
            va += PAGE_SIZE;
        }

        // Copy the file extent in, through the frames' identity-mapped
        // physical addresses; see [`write_user`]. The target space is not
        // active yet, so its user virtual addresses do not exist in the
        // current `CR3`.
        let copy_len = h.file_size as usize;
        if copy_len > 0 {
            let src = image
                .get(h.file_offset as usize..)
                .ok_or(LoadError::SegmentTruncated)?
                .get(..copy_len)
                .ok_or(LoadError::SegmentTruncated)?;
            // SAFETY: the segment's pages were just mapped, so every byte of
            // `[va, va + copy_len)` has a mapping. The write is within it.
            unsafe {
                write_user(space.address_space(), placement.va, src)?;
            }
        }
    }

    // Map the user stack, one page at a time up from its lowest address. The
    // page *at* `USER_SPACE_END` is never mapped — `USER_STACK_TOP` is one
    // page below it on purpose, so an overrun faults on an unmapped page.
    let stack_bottom = USER_STACK_TOP - USER_STACK_SIZE;
    let mut va = stack_bottom;
    while va < USER_STACK_TOP {
        // SAFETY: `frames` is the kernel's allocator per this function's
        // contract, and the stack region is inside the user range by
        // construction.
        space.map_user_page(va, frames)?;
        va += PAGE_SIZE;
    }

    Ok((space, LoadedImage { entry, brk }))
}

/// Copy `src` into user memory at `va`.
///
/// # Why the copy goes through the frame's physical address
///
/// The target space is not the active one while a program is being loaded: the
/// kernel is still running under its own `CR3`, and the program's user
/// mappings do not exist there. Writing through `va` would therefore fault on
/// the first byte, even though the page is mapped in the space being built.
///
/// The kernel's own space identity-maps physical RAM, so the frame behind each
/// user page is reachable at its physical address regardless of which space is
/// active. The copy translates the virtual address in `space` and writes
/// through that physical address.
///
/// No page flags are touched: the physical write is a supervisor access to
/// RAM, and the user page's `W^X` permissions stay exactly as the loader set
/// them.
///
/// # Safety
///
/// Every page of `[va, va + src.len())` must already be mapped in `space`, and
/// the physical frames they name must be within the kernel's identity map.
pub unsafe fn write_user(space: &AddressSpace, va: u64, src: &[u8]) -> Result<(), LoadError> {
    if src.is_empty() {
        return Ok(());
    }
    let mut cursor = va;
    for chunk in src.chunks(PAGE_SIZE as usize) {
        let in_page = (PAGE_SIZE - (cursor % PAGE_SIZE)) as usize;
        let n = in_page.min(chunk.len());
        let pa = space
            .translate(cursor)
            .ok_or(LoadError::Map(MapError::NotMapped))?;
        let dst = pa as *mut u8;
        // SAFETY: `n` bytes at `dst` are inside the frame the caller proved
        // mapped, and `chunk` has at least `n` bytes by the `chunks` bound.
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, n) };
        cursor += n as u64;
    }
    Ok(())
}

/// Build the initial user context for a program.
///
/// The context is what `iretq` consumes: it is the frame the CPU builds as it
/// drops to ring 3. Rather than hand-assembling a stack in the caller, this
/// function lays out the exact frame the transition needs and hands back the
/// stack pointer to it.
///
/// # Safety
///
/// `stack_top` must be the top of a mapped user stack, and the program image
/// must already be loaded at the address it will run from.
pub unsafe fn user_context(entry: u64, stack_top: u64) -> u64 {
    // The `iretq` frame, laid out from the top down. Each `push` the CPU will
    // pop is 8 bytes, so the pushes below are in reverse order.
    //
    // `SS` and `RSP` are user selectors with RPL 3. `CS` likewise. `RFLAGS`
    // has bit 1 (reserved, must be 1) and bit 0 (clear — we are entering the
    // ring-0 side of the transition as the target of `iretq`).
    //
    // The selectors come from the HAL's GDT, so they cannot drift away from
    // the descriptors the kernel actually installed. The RPL is added here
    // because the GDT holds ring-0 descriptors and the CPU requires the
    // *loaded* selectors to name a descriptor whose DPL matches the
    // privilege level being entered.
    const SEL_K64_CODE: u64 = vela_hal_x86_64::gdt::KERNEL_CODE_SELECTOR as u64;
    const SEL_K64_DATA: u64 = vela_hal_x86_64::gdt::KERNEL_DATA_SELECTOR as u64;
    const SEL_U64_CODE: u64 = (vela_hal_x86_64::gdt::USER_CODE_SELECTOR as u64) | 0b11;
    const SEL_U64_DATA: u64 = (vela_hal_x86_64::gdt::USER_DATA_SELECTOR as u64) | 0b11;
    const RFLAGS_IF: u64 = 1 << 9;

    let mut sp = stack_top;
    // A macro rather than a closure: a `move` closure captures `sp` by value,
    // so its decrements update the closure's private copy and the outer `sp`
    // returned at the end still names the top of the stack. That is a real bug
    // this code had, and it makes `iretq` read the frame one page above the
    // top.
    macro_rules! push {
        ($value:expr) => {{
            sp -= 8;
            // SAFETY: `sp` is decremented before the write, and the caller
            // guarantees the whole stack region is mapped. Writing below the
            // top is what a stack does.
            unsafe {
                *(sp as *mut u64) = $value;
            }
        }};
    }

    // Pushed in reverse: the CPU pops in the order written.
    //
    // `iretq` to a lower privilege level pops **five** values: RIP, CS,
    // RFLAGS, RSP and SS. The frame therefore has to contain a fresh user
    // stack pointer as well as the user `SS`; omitting it makes the CPU read
    // `SS` as `RSP` and then fault fetching the real `SS` from one past the
    // top of the stack.
    push!(SEL_U64_DATA); // SS
                         // The program's initial stack pointer. Sixteen bytes below the top keeps
                         // the usual 16-byte alignment and leaves the top of the stack itself
                         // readable; the page *at* `stack_top` is deliberately unmapped.
    push!(stack_top - 16); // RSP
                           // The RFLAGS the program will run with. Bit 1 is reserved and must be 1;
                           // bit 0 is the reserved "carry" flag that the CPU forces to 0 on the way
                           // in, and the interrupt-enable flag is set so the program can be
                           // interrupted.
    push!(RFLAGS_IF | 0b10); // RFLAGS
    push!(SEL_U64_CODE); // CS
    push!(entry); // RIP
                  // The frame `iretq` expects, from low to high, is RIP, CS, RFLAGS, RSP, SS,
                  // which is the reverse of this push order, so the layout above is correct.
    let _ = SEL_K64_CODE;
    let _ = SEL_K64_DATA;
    sp
}

/// Install a GDT and TSS sufficient for ring 3.
///
/// # Why a TSS is required at all
///
/// `syscall`/`sysret` and `iretq` into ring 3 do not consult the TSS on entry,
/// but an interrupt taken *from* ring 3 loads `SS`/`RSP` from the TSS's
/// privilege stack. Without one, the first timer interrupt while a user
/// program runs faults. So the TSS is not optional once user mode exists, and
/// this is the point in the boot where it becomes necessary.
///
/// The GDT and the TSS live in the HAL's static storage: they must be valid
/// for the life of the kernel, and they must survive a page-table switch, so
/// heap allocation would be the wrong lifetime. The HAL owns them because the
/// GDT's *order* is constrained by the `syscall`/`sysret` selector arithmetic,
/// and that arithmetic has to agree with [`crate::gate::STAR_VALUE`] — so the
/// two must live in the same place to be able to agree at all.
///
/// # Safety
///
/// Must be called once, with interrupts disabled.
pub unsafe fn install_gdt() {
    // SAFETY: forwarded from this function's own contract: this runs once
    // during boot, with interrupts disabled, before any user code exists.
    unsafe { vela_hal_x86_64::gdt::install() };
}

/// Drop to ring 3 at `context`.
///
/// The context is a stack pointer to an `iretq` frame, as built by
/// [`user_context`].
///
/// # Safety
///
/// The caller must be running in ring 0 with interrupts disabled, and every
/// frame the `iretq` frame names must be mapped in the *current* address
/// space. A violation is a fault, not a return.
pub unsafe fn enter_user(context: u64) -> ! {
    // SAFETY: the caller guarantees the context is a valid `iretq` frame in the
    // current address space, and that its segment selectors are the ones
    // [`install_gdt`] published. The instruction does not return.
    unsafe {
        regs::enter_user_mode(context);
    }
}

/// Read the current privilege level, for diagnostics and tests.
///
/// 3 in user mode, 0 in the kernel.
pub fn current_cpl() -> u8 {
    if !regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: reading CS does not change state.
    let cs = regs::read_cs();
    (cs & 0b11) as u8
}
