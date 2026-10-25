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
//! 0x0000_0000_0010_0000  program image: text, rodata, data, bss
//! 0x0000_4000_0000_0000  the break: the heap grows up from here
//! 0x0000_7FFF_FFFF_F000  initial user stack top, growing down
//! 0x0000_8000_0000_0000  USER_SPACE_END — the kernel half begins
//! 0xFFFF_8000_0000_0000  kernel image (mapped by `AddressSpace::new`)
//! ```
//!
//! Three constraints shaped the numbers, and each is a real constraint rather
//! than a round number chosen for looks:
//!
//! * **The image starts at 64 KiB.** The first 64 KiB is left unmapped so that
//!   a null or small-offset pointer faults immediately, instead of reading
//!   whatever the program happened to put at its lowest address.
//! * **The break starts at 1 GiB.** It has to clear the image, and it has to
//!   leave room below the stack for a program whose data grows. 1 GiB leaves
//!   a gap of nearly a gibibyte, which is more than any v0.1.0 program needs
//!   and keeps the arithmetic in the page mapper simple.
//! * **The stack top is just under the 128 GiB user ceiling.** The ceiling is
//!   `USER_SPACE_END` from `kernel/syscall::copy`, which is the same bound the
//!   user-copy helpers validate against. If the two disagreed, a program could
//!   map memory the kernel refused to copy to — a hole in the validation.
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

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use vela_elf_loader::{program_headers, ProgramHeader, ELF64};
use vela_hal_common::{AddressSpaceId, FrameSource, MapError, PageFlags, PAGE_SIZE};
use vela_hal_x86_64::addrspace::{AddressSpace, IdentityPhys};
use vela_hal_x86_64::regs;

use crate::copy::{USER_SPACE_BASE, USER_SPACE_END};

/// Where a user program's image begins.
///
/// The first 64 KiB is deliberately left unmapped.
pub const IMAGE_BASE: u64 = 0x0000_0000_0010_0000;

/// Where a user program's heap begins.
pub const BREAK_START: u64 = 0x0000_4000_0000_0000;

/// The ceiling on how far the heap may grow.
///
/// Half the user half, so that the heap can never reach the stack. Heap and
/// stack colliding is a real failure mode, and the cheapest way to make it
/// impossible is to give each a hard ceiling the other cannot pass.
pub const BREAK_LIMIT: u64 = 0x0000_6000_0000_0000;

/// The size of a user program's stack, in bytes.
pub const USER_STACK_SIZE: u64 = 64 * 1024;

/// The user stack's top address.
///
/// Just below [`USER_SPACE_END`]. The stack grows down from here.
pub const USER_STACK_TOP: u64 = USER_STACK_SIZE - PAGE_SIZE + USER_SPACE_END - 1;

/// The number of user address spaces created so far.
///
/// A space's id is its PML4's physical address, but the allocator hands out
/// addresses in a way that lets two live spaces collide only if the counter
/// wraps. It never will in practice, and the distinctness only matters for
/// diagnostics.
static NEXT_SPACE_ID: AtomicU64 = AtomicU64::new(1);

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

/// A user address space and the bookkeeping a process needs to grow.
///
/// The [`AddressSpace`] itself is the hardware truth. What this adds is the
/// *policy*: which pages belong to the image, where the break is, and how a
/// `brk` call turns into mapping work.
pub struct UserSpace {
    /// The page tables.
    space: AddressSpace,
    /// The current program break.
    brk: u64,
    /// The base of the break region, so a `brk` can tell growth from shrink.
    brk_base: u64,
}

impl core::fmt::Debug for UserSpace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserSpace")
            .field("cr3", &format_args!("{:#x}", self.space.cr3()))
            .field("brk", &format_args!("{:#x}", self.brk))
            .field("mapped_frames", &self.space.mapped_frames())
            .finish()
    }
}

impl UserSpace {
    /// Create an empty user space with its own page tables.
    ///
    /// The kernel half is mapped by [`AddressSpace::new`], so the space is
    /// able to take an interrupt or a syscall the instant it exists — which is
    /// what makes it safe to activate before the image is loaded.
    pub fn new() -> Self {
        let id = NEXT_SPACE_ID.fetch_add(1, Ordering::Relaxed);
        let mut space = AddressSpace::new(AddressSpaceId(id << 12));
        // The heap is identity-mapped during boot, so the PML4's physical
        // address is its virtual address. `set_pml4_base` refines this once
        // the kernel knows better; for a user space it is correct for the
        // lifetime of the boot heap.
        let pa = space.pml4_phys(&IdentityPhys);
        space.set_pml4_base(pa);
        Self {
            space,
            brk: BREAK_START,
            brk_base: BREAK_START,
        }
    }

    /// The underlying address space.
    pub fn address_space(&self) -> &AddressSpace {
        &self.space
    }

    /// The value to load into CR3 for this space.
    pub const fn cr3(&self) -> u64 {
        self.space.cr3()
    }

    /// The current program break.
    pub const fn brk(&self) -> u64 {
        self.brk
    }

    /// True when `va` is inside the region a process may map.
    ///
    /// The test is deliberately the same bound `kernel/syscall::copy` uses, so
    /// "may be mapped" and "may be copied to" cannot disagree.
    pub const fn is_user_range(va: u64, len: u64) -> bool {
        va >= USER_SPACE_BASE && len <= USER_SPACE_END.saturating_sub(va)
    }

    /// Map one page of user memory, returning its virtual address.
    ///
    /// The page is zeroed, because a process has no way to know what was in a
    /// physical frame the kernel handed it a moment ago, and handing back
    /// another process's leftovers is an information leak.
    ///
    /// # Safety
    ///
    /// `frames` must be the kernel's frame allocator: handing a process memory
    /// from any other source would let two processes share a frame.
    pub unsafe fn map_user_page(&self, va: u64, frames: &mut dyn FrameSource) -> Result<u64, MapError> {
        if va % PAGE_SIZE != 0 {
            return Err(MapError::MisalignedVirtual);
        }
        if !Self::is_user_range(va, PAGE_SIZE as u64) {
            return Err(MapError::MisalignedVirtual);
        }
        let pa = match frames.alloc_frame() {
            Ok(pa) => pa,
            Err(e) => return Err(e),
        };
        if let Err(e) = self.space.map_page(va, pa, PageFlags::user_rw()) {
            // The frame is ours and nothing maps it, so returning it is not a
            // use-after-free: no address space references it.
            // SAFETY: as above.
            unsafe { frames.free_frame(pa) };
            return Err(e);
        }
        // Zero the page in place. This runs in ring 0 against a supervisor
        // alias of the same physical frame, so it does not depend on the
        // process having the page mapped yet.
        // SAFETY: the frame was just mapped, is `user_rw`, and the kernel's
        // higher-half mapping of the same physical memory is what makes this
        // write valid.
        unsafe { zero_physical(pa, PAGE_SIZE as usize) };
        Ok(va)
    }

    /// Unmap one page of user memory, returning its frame to `frames`.
    pub fn unmap_user_page(&self, va: u64, frames: &mut dyn FrameSource) -> Result<(), MapError> {
        let pa = self.space.unmap_page(va)?;
        // SAFETY: `unmap_page` removed the only mapping of `pa`, so no address
        // space references it and returning it is sound.
        unsafe { frames.free_frame(pa) };
        Ok(())
    }

    /// Set the program break to `addr`, returning the break actually in force.
    ///
    /// Classic `brk` semantics, and the return value is what makes them
    /// usable: a program that asks for more memory than exists gets back the
    /// break it really got, so it can carry on with what it has rather than
    /// discovering the failure later at an address it cannot write.
    pub fn set_brk(
        &mut self,
        addr: u64,
        frames: &mut dyn FrameSource,
    ) -> Result<u64, LoadError> {
        // A request inside the image, or past the ceiling, cannot be honoured.
        // Reporting the current break rather than an error is the classic
        // behaviour and is more useful to the caller than a failure.
        if addr < self.brk_base || addr > BREAK_LIMIT || addr % PAGE_SIZE != 0 {
            return Ok(self.brk);
        }

        if addr > self.brk {
            // Growing: map the pages between the old break and the new one.
            // Mapping one at a time and stopping at the first failure means a
            // request that only partly succeeds leaves the break at the point
            // it actually reached, which is what we return.
            while self.brk < addr {
                // SAFETY: the caller guarantees `frames` is the kernel's frame
                // allocator, and `self.brk` is inside the user range because
                // `BREAK_LIMIT` is.
                if unsafe { self.map_user_page(self.brk, frames) }.is_err() {
                    break;
                }
                self.brk += PAGE_SIZE;
            }
        } else {
            // Shrinking: release the tail. Growing again re-maps zeroed pages,
            // so a shrink-then-grow cycle cannot resurrect old data.
            while self.brk > addr {
                self.brk -= PAGE_SIZE;
                // A failed unmap would mean the page is not mapped, which cannot
                // happen for a page between `brk_base` and `brk`. Ignoring the
                // error keeps the break consistent with reality.
                let _ = self.unmap_user_page(self.brk, frames);
            }
        }
        Ok(self.brk)
    }

    /// Move this CPU into this space.
    ///
    /// # Safety
    ///
    /// Must not race with another switch on the same CPU, and the space must
    /// outlive the CPU's time in it.
    pub unsafe fn activate(&self) {
        // SAFETY: forwarded from this function's own contract.
        unsafe { vela_hal_x86_64::addrspace::activate(&self.space) };
    }
}

impl Default for UserSpace {
    fn default() -> Self {
        Self::new()
    }
}

/// Zero `len` bytes at physical address `pa`.
///
/// # Safety
///
/// `pa` must be a frame the kernel owns, and the caller must have exclusive
/// access to it. The write goes through the kernel's identity mapping of low
/// physical memory.
unsafe fn zero_physical(pa: u64, len: usize) {
    // SAFETY: the caller guarantees the frame is a kernel-owned frame the
    // kernel can reach. `pa` is page-aligned and `len` is one page, so the
    // range stays inside that frame.
    unsafe {
        core::ptr::write_bytes(pa as *mut u8, 0, len);
    }
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
    headers: &[ProgramHeader],
    entry: u64,
) -> Result<Vec<(Placement, usize)>, LoadError> {
    let mut placed: Vec<(Placement, usize)> = Vec::new();
    // The image region ends where the break starts, so an image can never
    // overlap the heap. The gap between them is deliberate; see the module
    // documentation.
    let image_end = BREAK_START;

    for (i, h) in headers.iter().enumerate() {
        if h.mem_len == 0 && h.file_len == 0 {
            continue;
        }
        if h.mem_len == 0 {
            // A file extent with no memory extent is meaningless; treating it
            // as a no-op would silently drop a segment the program relies on.
            return Err(LoadError::SegmentTruncated);
        }
        if h.file_len > h.mem_len {
            // `.bss` is the other way round: more memory than file. Fewer is
            // the case that cannot be represented.
            return Err(LoadError::SegmentTruncated);
        }

        let va = h.vaddr;
        if va < IMAGE_BASE || va.checked_add(h.mem_len).is_none() {
            return Err(LoadError::SegmentOutOfRange);
        }
        if va >= image_end || va + h.mem_len > image_end {
            return Err(LoadError::SegmentOutOfRange);
        }

        // W^X: anything with a file extent is program content. Mapping it
        // writable would let a program rewrite its own text, which is both a
        // security hole and a surprise. Nothing in a native image needs a
        // writable text segment, so the rule is uniform.
        let flags = if h.flags.executable {
            PageFlags::user_rx()
        } else {
            PageFlags::user_rw()
        };

        let p = Placement {
            va,
            mem_len: h.mem_len,
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

/// Load a user program into a fresh address space.
///
/// # Safety
///
/// `frames` must be the kernel's frame allocator, and `image` must be a
/// complete ELF file that outlives this call — its bytes are copied into the
/// address space, so it does not need to outlive it.
pub unsafe fn load_user_program<F: FrameSource>(
    image: &[u8],
    frames: &mut F,
) -> Result<(UserSpace, LoadedImage), LoadError> {
    let elf = ELF64::parse(image).map_err(|_| LoadError::Malformed)?;
    if !elf.is_executable() {
        return Err(LoadError::WrongFormat);
    }
    let entry = elf.entry();
    let headers = program_headers(image).map_err(|_| LoadError::Malformed)?;
    let placed = plan_placements(&headers, entry)?;

    let mut space = UserSpace::new();

    for (placement, index) in &placed {
        let h = &headers[*index];
        // Map the segment's pages first.
        let pages = (placement.mem_len as usize).div_ceil(PAGE_SIZE);
        let mut va = placement.va;
        for _ in 0..pages {
            // SAFETY: `frames` is the kernel's allocator per this function's
            // contract, and `va` is inside the user range because the placement
            // plan proved the whole segment is.
            if unsafe { space.map_user_page(va, frames) }.is_err() {
                return Err(LoadError::OutOfMemory);
            }
            va += PAGE_SIZE;
        }

        // Copy the file extent in. The bytes are written through the kernel's
        // view of the page, which is mapped supervisor-writable in the kernel
        // half and user-accessible in the user half — so writing needs the
        // page to be temporarily made writable.
        let copy_len = h.file_len as usize;
        if copy_len > 0 {
            let src = image
                .get(h.offset as usize..)
                .ok_or(LoadError::SegmentTruncated)?
                .get(..copy_len)
                .ok_or(LoadError::SegmentTruncated)?;
            // SAFETY: the segment's pages were just mapped, so every byte of
            // `[va, va + copy_len)` has a mapping. The write is within it.
            unsafe {
                copy::write_user(space.address_space(), placement.va, src)?;
            }
        }
    }

    // The heap starts where the image ended, so an allocation can never land
    // on top of the program's own data.
    let image_end = placed
        .iter()
        .map(|(p, _)| p.va + p.mem_len)
        .max()
        .unwrap_or(BREAK_START);
    let brk = image_end.next_multiple_of(PAGE_SIZE as u64).max(BREAK_START);

    Ok((
        space,
        LoadedImage {
            entry,
            brk,
        },
    ))
}

/// Copy `src` into user memory at `va`.
///
/// The destination page is temporarily made supervisor-writable, because a
/// ring-3 page is `user_rw` and the kernel's own write through the identity
/// mapping would be a supervisor write to a user page — which the CPU rejects.
///
/// # Safety
///
/// Every page of `[va, va + src.len())` must already be mapped in `space`.
pub unsafe fn write_user(space: &AddressSpace, va: u64, src: &[u8]) -> Result<(), LoadError> {
    let end = va.checked_add(src.len() as u64).ok_or(LoadError::SegmentOutOfRange)?;
    let mut cursor = va;
    for chunk in src.chunks(PAGE_SIZE) {
        let in_page = (PAGE_SIZE as u64 - (cursor % PAGE_SIZE)) as usize;
        let n = in_page.min(chunk.len());
        // SAFETY: the caller guarantees the page is mapped, so making it
        // writable and back is well defined. The flags are restored even on
        // the error path because the write is the only thing that can fail.
        unsafe { space.set_flags(cursor, PageFlags::kernel_rw())? };
        let dst = cursor as *mut u8;
        // SAFETY: `n` bytes at `dst` are inside the mapped page, and `chunk`
        // has at least `n` bytes by the `chunks` bound.
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), dst, n) };
        // SAFETY: as above.
        unsafe { space.set_flags(cursor, PageFlags::user_rw())? };
        cursor += n as u64;
    }
    let _ = end;
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
    let mut push = move |v: u64| {
        sp -= 8;
        // SAFETY: `sp` is decremented before the write, and the caller
        // guarantees the whole stack region is mapped. Writing below the top
        // is what a stack does.
        unsafe {
            *(sp as *mut u64) = v;
        }
        sp
    };

    // Pushed in reverse: the CPU pops in the order written.
    let _ = push(SEL_U64_DATA); // SS
    // The RFLAGS the program will run with. Bit 1 is reserved and must be 1;
    // bit 0 is the reserved "carry" flag that the CPU forces to 0 on the way
    // in, and the interrupt-enable flag is set so the program can be
    // interrupted.
    let _ = push(RFLAGS_IF | 0b10); // RFLAGS
    let _ = push(SEL_U64_CODE); // CS
    let _ = push(entry); // RIP
    // ...and the frame `iretq` expects has SS, RFLAGS, CS, RIP, which is the
    // reverse of this push order, so the layout above is already correct.
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

/// A boxed space, for callers that want ownership without a lifetime.
pub type BoxedUserSpace = Box<UserSpace>;