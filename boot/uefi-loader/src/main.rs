//! VelaOS UEFI loader.
//!
//! Runs under firmware Boot Services and owns the whole pre-kernel sequence:
//!
//! * **B-01** set up logging.
//! * **B-02** read `\Vela\boot\vela.toml`, then the kernel ELF and initrd.
//! * **B-03** locate GOP and draw the original VelaOS splash.
//! * **B-04** validate the kernel ELF through `vela-elf-loader`.
//! * **B-05** reserve a single page pool for the image, stacks and BootInfo.
//! * **B-06** collect framebuffer geometry and the memory map.
//! * **B-07** exit Boot Services.
//! * **B-08** jump to the kernel entry point.
//!
//! Two things learned the hard way shape this file, and both are load-bearing:
//!
//! 1. The `uefi` crate's global allocator calls Boot Services, which cease to
//!    exist the instant `exit_boot_services` returns. Everything the kernel
//!    needs must therefore already be allocated *and written* before the call.
//!    In particular BootInfo is copied into the pool **after** ExitBoot
//!    Services, but into memory the pool already owns.
//! 2. The framebuffer pointer and memory map are only obtainable while Boot
//!    Services exist, so they must be recorded into the BootInfo struct *before*
//!    the exit, not looked up afterwards.

#![no_std]
#![no_main]

extern crate alloc;

mod config;
mod logging;
mod pagetables;
mod pool;
mod splash;

use core::mem;

use uefi::fs::FileSystem;
use uefi::mem::memory_map::{MemoryMap, MemoryType};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::runtime::ResetType;
use uefi::{entry, prelude::*};

use vela_boot_protocol::{arch_flags, PixelFormat as VelaPixelFormat, VelaBootInfo};
use vela_elf_loader::Elf64Image;
use vela_renderer2d::Surface;

/// Pages reserved for the loader's own stack.
const LOADER_STACK_PAGES: u64 = 64;
/// Pages reserved for the kernel's initial stack, handed over in
/// `BootInfo::stack_top`.
const KERNEL_STACK_PAGES: u64 = 32;
/// Pages reserved for the kernel's heap.
///
/// The heap is the one piece of kernel memory whose *physical* address the
/// kernel must know before it can build its own page tables, because the
/// tables themselves live in it. The loader reserves it here and hands it
/// over in the boot info.
const KERNEL_HEAP_PAGES: u64 = 1024;
/// The kernel image is placed here: 1 MiB aligned, well clear of the low
/// memory the firmware uses for itself.
const KERNEL_LOAD_ADDR: u64 = 0x0040_0000;

/// The loader's panic handler.
///
/// # Why the loader needs its own
///
/// A `no_std` binary must supply a `#[panic_handler]`, and `uefi` does not
/// provide one. Without it the loader does not link at all, so this is not an
/// edge case — it is the difference between a bootable image and none.
///
/// # Why it must not return
///
/// The failure this exists to report is a loader failure, and the only honest
/// report is to stop the machine. The firmware is still running, so
/// `runtime_reset` is the most forceful reset available; anything less would
/// drop the user back into a firmware boot loop with nothing on screen
/// explaining what went wrong.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    // Logged through the loader's logger, which writes to the debug console —
    // a port, which needs no Boot Services and allocates nothing. That is what
    // makes this safe to run *after* the exit, which is the case that matters:
    // a panic in the post-EBS handoff is exactly the failure a developer is
    // trying to diagnose.
    log::error!("VELA-BOOT PANIC: {info}");

    if logging::boot_services_exited() {
        // After the exit there is no firmware to ask, so the machine is left
        // halted rather than reset. A halted machine plus the message above is
        // strictly more information than a reboot loop that erases the framebuffer
        // and repeats, and it is the only honest response available.
        loop {
            // SAFETY: `hlt` halts until the next interrupt and has no operands
            // to be wrong about. Interrupts may or may not be enabled here —
            // either way the CPU stops executing, which is what a halted
            // machine means.
            unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
        }
    }

    // Still under Boot Services, so the firmware can be asked to reset.
    uefi::runtime::reset(ResetType::SHUTDOWN, Status::ABORTED, None);
}

/// UEFI entry point.
///
/// The `uefi` 0.41 `#[entry]` macro takes no arguments: the image handle and
/// system table are obtained through [`uefi::boot`] inside the function.
#[entry]
fn efi_main() -> Status {
    // The logger first, so that everything after this point is observable.
    // `set_max_level` alone would set the filter with nothing to filter
    // through, and every message below would vanish.
    //
    // SAFETY: called once, before any other thread exists.
    unsafe { logging::install() };
    log::info!("VELA-BOOT VelaOS UEFI loader");

    let mut fs = match uefi::boot::get_image_file_system(uefi::boot::image_handle()) {
        Ok(fs) => FileSystem::from(fs),
        Err(e) => {
            log::error!("no image file system: {e}");
            return e.status();
        }
    };

    match run(&mut fs) {
        Ok(never) => match never {},
        Err(status) => {
            // Still under Boot Services here, so logging works.
            log::error!("loader failed: {status:?}");
            uefi::boot::stall(core::time::Duration::from_millis(3000));
            status
        }
    }
}

/// Reads the config, loads the kernel, draws the splash, exits Boot Services and
/// transfers control.
///
/// The `Ok` variant is uninhabited: this function only returns by jumping.
fn run(fs: &mut FileSystem) -> Result<core::convert::Infallible, Status> {
    // --- B-02: configuration ------------------------------------------------
    let cfg_bytes = match fs.read(cstr16!("\\Vela\\boot\\vela.toml")) {
        Ok(b) => b,
        Err(_) => return Err(Status::NOT_FOUND),
    };
    let cfg_text = match core::str::from_utf8(&cfg_bytes) {
        Ok(s) => s,
        Err(_) => return Err(Status::BAD_BUFFER_SIZE),
    };
    let cfg = match config::parse(cfg_text) {
        Ok(c) => c,
        Err(_) => return Err(Status::BAD_BUFFER_SIZE),
    };
    log::info!(
        "config default={:?} entries={}",
        cfg.default,
        cfg.entries.len()
    );

    let entry_cfg = match cfg.entries.iter().find(|e| e.name == cfg.default) {
        Some(e) => e,
        None => return Err(Status::NOT_FOUND),
    };

    // --- B-02: kernel image -------------------------------------------------
    let kernel_path = OwnedPath::new(&entry_cfg.kernel_full_path());
    let kernel_bytes = match fs.read(kernel_path.as_cstr()) {
        Ok(b) => b,
        Err(_) => return Err(Status::NOT_FOUND),
    };
    log::info!("kernel: {} bytes", kernel_bytes.len());

    // --- B-03: GOP and splash -----------------------------------------------
    let gop_handle =
        uefi::boot::get_handle_for_protocol::<GraphicsOutput>().map_err(|e| e.status())?;
    let mut gop = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(gop_handle)
        .map_err(|e| e.status())?;

    let (fb_paddr, fb_bytes, w, h, stride_bytes, vela_fmt) = {
        let mode = gop.current_mode_info();
        let px = mode.pixel_format();
        let pixel_format = match px {
            PixelFormat::Rgb => VelaPixelFormat::Rgb,
            PixelFormat::Bgr => VelaPixelFormat::Bgr,
            // BLT_ONLY and the YUV modes have no addressable framebuffer, so
            // there is nothing to draw on and nothing for the kernel to use.
            other => {
                log::error!("unsupported GOP pixel format {other:?}");
                return Err(Status::UNSUPPORTED);
            }
        };
        let (w, h) = mode.resolution();
        // `stride()` is in pixels; each pixel is 4 bytes.
        let stride_px = mode.stride() as u32;
        let stride_bytes = (stride_px as u64).saturating_mul(4);

        let (fb_paddr, fb_bytes) = {
            let mut fb = gop.frame_buffer();
            // Before `ExitBootServices` the firmware hands us identity-mapped
            // memory, so this pointer *is* the physical framebuffer address.
            let fb_paddr = fb.as_mut_ptr() as u64;
            let fb_bytes = fb.size() as u64;
            log::info!(
                "GOP {w}x{h} stride={stride_bytes}B fmt={px:?} base={fb_paddr:#x} size={fb_bytes:#x}"
            );

            match Surface::new(
                fb.as_mut_ptr(),
                fb.size(),
                w as u32,
                h as u32,
                stride_px,
                pixel_format,
            ) {
                Some(mut surface) => splash::draw(&mut surface),
                None => {
                    log::error!("framebuffer geometry rejected");
                    return Err(Status::BAD_BUFFER_SIZE);
                }
            }
            (fb_paddr, fb_bytes)
        };

        // Guard against a mode that lies about its geometry: a framebuffer
        // smaller than `stride * height` would let the splash write out of
        // bounds, and the kernel would later map a bogus range.
        let needed = stride_bytes.saturating_mul(h as u64);
        if fb_bytes < needed {
            log::error!("framebuffer {fb_bytes} bytes < {needed} required");
            return Err(Status::BAD_BUFFER_SIZE);
        }

        (
            fb_paddr,
            fb_bytes,
            w as u32,
            h as u32,
            stride_bytes,
            pixel_format,
        )
    };
    // Release GOP: after this the framebuffer is only reachable by address.
    drop(gop);

    // --- B-04: validate the kernel ELF --------------------------------------
    let image = Elf64Image::parse_owned(kernel_bytes).map_err(|e| {
        log::error!("kernel ELF rejected: {e:?}");
        Status::BAD_BUFFER_SIZE
    })?;
    let span = image.total_span();
    log::info!(
        "kernel: {} segments, span {span:#x}, entry {:#x}",
        image.load_segments.len(),
        image.entry()
    );

    // --- B-05: one pool, allocated before the exit --------------------------
    //
    // The pool has to cover more than it used to. It now holds:
    //
    // * the kernel image,
    // * the kernel's boot stack and the loader's own stack,
    // * the **kernel's heap**, which the loader reserves so that the heap
    //   physically lives at an address the kernel knows and can therefore
    //   map at the *same* virtual and physical address;
    // * the **bootstrap page tables**, which must exist before the kernel
    //   runs because the kernel's higher half is not mapped until they do;
    // * the persisted memory map and the boot-info page.
    //
    // The page-table count is computed from the firmware's memory map rather
    // than guessed, so a change in the map cannot silently overflow the pool.
    let memory_map = uefi::boot::memory_map(MemoryType::LOADER_DATA).map_err(|e| e.status())?;
    let table_pages = pagetables::pages_needed(&memory_map, span);
    let need_pages = span.div_ceil(4096)
        + KERNEL_STACK_PAGES
        + LOADER_STACK_PAGES
        + KERNEL_HEAP_PAGES
        + table_pages
        + mem::size_of::<VelaBootInfo>() as u64;
    let mut pool = match pool::Pool::new(need_pages) {
        Ok(p) => p,
        Err(_) => return Err(Status::OUT_OF_RESOURCES),
    };
    log::info!(
        "reserved {} pages for the boot pool ({} for page tables)",
        pool.pages(),
        table_pages
    );

    // Place and copy the kernel image.
    //
    // The kernel is *linked* in the higher half and the loader places the image
    // at a low *physical* address; the kernel maps the physical range at the
    // linked virtual range itself. So the virtual addresses are used exactly as
    // the linker produced them and only the physical placement is chosen here.
    //
    // Each segment's physical address is `phys_base + (vaddr - image_vaddr)`,
    // which preserves every byte's offset from the start of the image — the
    // same invariant `place` maintains, expressed for the case where the
    // virtual base is fixed at link time rather than chosen at load time.
    let image_vaddr = match image.lowest_vaddr() {
        Some(v) => v,
        None => return Err(Status::BAD_BUFFER_SIZE),
    };
    // Validate the image before committing any memory to it. `phys_base` is 0
    // here because the validation does not depend on where the image lands —
    // only the alignment and the span are checked — and the real allocation
    // comes from the pool a moment later.
    if image.place_higher_half(0, span).is_err() {
        log::error!("kernel image rejected: overlap or span {span:#x}");
        return Err(Status::BAD_BUFFER_SIZE);
    }
    let kernel_paddr = match pool.alloc_pages(span, 0x1000) {
        Ok(p) => p,
        Err(_) => return Err(Status::OUT_OF_RESOURCES),
    };
    let entry_addr = image.entry();
    for seg in &image.load_segments {
        let src = image.segment_bytes(seg);
        let dst_paddr = kernel_paddr + (seg.vaddr - image_vaddr);
        // SAFETY: `place` proved `dst_paddr` lies inside the pool reservation
        // this module sized, and `Elf64Image::parse_owned` proved `src` is a
        // slice of the owned buffer. Both bounds are machine-checked above.
        unsafe {
            let dst = dst_paddr as *mut u8;
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
            let zf = seg.zero_fill_len() as usize;
            if zf > 0 {
                core::ptr::write_bytes(dst.add(src.len()), 0, zf);
            }
        }
        log::info!(
            "  seg {:#x}+{:#x} (file {:#x}) flags {:#x}",
            seg.vaddr,
            seg.mem_size,
            seg.file_size,
            seg.flags
        );
    }
    log::info!("kernel at {kernel_paddr:#x}, entry {entry_addr:#x}");

    // The size of the image's *executable* prefix, rounded up to a page.
    //
    // The kernel maps the image read-execute up to this point and
    // read-write after it, which is what gives the kernel's own image W^X.
    // The boundary is the end of the last executable segment: everything the
    // linker placed after it is data or `.bss`.
    let text_bytes = executable_prefix_bytes(&image, image_vaddr);
    log::info!("kernel text prefix {text_bytes:#x} bytes");

    let kernel_stack_paddr = pool.alloc_pages(KERNEL_STACK_PAGES * 4096, 16)?;
    let stack_top = kernel_stack_paddr + KERNEL_STACK_PAGES * 4096;
    let loader_stack_paddr = pool.alloc_pages(LOADER_STACK_PAGES * 4096, 16)?;

    // The kernel's heap.
    //
    // The loader reserves it rather than letting the kernel carve one out of
    // the memory map, because of one specific requirement: the heap must be
    // mapped at the *same* virtual and physical address in the kernel's own
    // page tables. That is what makes it sound for the kernel to compute a
    // page table's physical address from the heap pointer that holds it — the
    // translation is the identity, and no lookup is involved.
    //
    // The heap is page-aligned and physically contiguous, which the bump
    // allocator requires.
    let heap_paddr = pool.alloc_pages(KERNEL_HEAP_PAGES * 4096, 4096)?;
    let heap_bytes = KERNEL_HEAP_PAGES * 4096;
    log::info!("kernel heap {heap_bytes} bytes at {heap_paddr:#x}");

    // --- initrd (optional) ---------------------------------------------------
    let (initrd_paddr, initrd_len) = match entry_cfg.initrd_full_path() {
        None => (0u64, 0u64),
        Some(path) => {
            let c = OwnedPath::new(&path);
            match fs.read(c.as_cstr()) {
                Err(_) => {
                    log::warn!("initrd {path} not found");
                    (0, 0)
                }
                Ok(bytes) => {
                    let len = bytes.len() as u64;
                    let p = pool.alloc_pages(len, 8)?;
                    // SAFETY: the pool was sized to include the initrd, and
                    // `bytes` is a live allocation of exactly `len` bytes.
                    unsafe {
                        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, len as usize);
                    }
                    log::info!("initrd {len} bytes at {p:#x}");
                    (p, len)
                }
            }
        }
    };

    // --- B-06: BootInfo ------------------------------------------------------
    let mut bi = VelaBootInfo::zeroed();
    bi.framebuffer_paddr = fb_paddr;
    bi.framebuffer_bytes = fb_bytes;
    bi.framebuffer_width = w;
    bi.framebuffer_height = h;
    bi.framebuffer_stride_bytes = stride_bytes as u32;
    bi.framebuffer_pixel_format = vela_fmt as u32;
    bi.kernel_paddr = kernel_paddr;
    bi.kernel_bytes = span;
    bi.initrd_paddr = initrd_paddr;
    bi.initrd_bytes = initrd_len;
    bi.stack_top = stack_top;
    bi.arch_flags = arch_flags::PROTECTED_MODE;
    bi.kernel_vaddr_base = image_vaddr;
    bi.kernel_text_bytes = text_bytes;
    bi.heap_paddr = heap_paddr;
    bi.heap_bytes = heap_bytes;
    bi.pool_paddr = pool.base();
    bi.pool_bytes = pool.len();
    log::info!("bootinfo staged");

    // Persist the memory map while Boot Services still exist.
    let (map_paddr, map_len, desc_size, desc_version) = persist_memory_map(&mut pool)?;

    // --- B-06b: the bootstrap page tables ----------------------------------
    //
    // These are built *before* `ExitBootServices`, because they are what make
    // the kernel's higher half reachable at all. The firmware's tables map
    // low RAM only, and the kernel is linked at `0xFFFF800000001000`.
    //
    // They live in the pool, which the firmware identity-maps, so their
    // addresses are their physical addresses and the PML4's address can go
    // straight into `CR3` after the exit.
    let image_region = pagetables::ImageRegion {
        vaddr: image_vaddr,
        paddr: kernel_paddr,
        len: span,
        text_bytes,
    };
    let tables = pagetables::build(&mut pool, &memory_map, &image_region, fb_paddr, fb_bytes);
    bi.page_table_paddr = tables.pml4_paddr();
    log::info!(
        "bootstrap pml4 at {:#x} ({} pages)",
        tables.pml4_paddr(),
        tables.pages()
    );

    // --- B-07: ExitBootServices ---------------------------------------------
    // From here on, Boot Services are gone. Nothing may allocate.
    log::info!("exiting boot services");
    let _m = unsafe { uefi::boot::exit_boot_services(None) };
    // Boot Services are gone: the firmware console handle and the `uefi`
    // global allocator both live in memory the firmware may now reclaim, so
    // the logger must stop using them. The debug console is unaffected.
    logging::boot_services_gone();
    log::info!("boot services exited");

    // Now that we own the pool outright, write BootInfo into it. This is the
    // only moment this is legal, because `bi` itself lives in firmware memory
    // that may already be reclaimed.
    let bi_paddr = pool
        .alloc_pages(mem::size_of::<VelaBootInfo>() as u64, 8)
        .unwrap_or(0);
    // SAFETY: `bi_paddr` is inside the pool reservation, which is page aligned
    // and large enough for the struct. The copy is of a plain-old-data type.
    unsafe {
        let dst = &mut *(bi_paddr as *mut VelaBootInfo);
        *dst = bi;
        dst.memory_map_paddr = map_paddr;
        dst.memory_map_bytes = map_len;
        dst.memory_desc_size = desc_size;
        dst.memory_desc_version = desc_version;
    }

    // --- B-08: hand off ------------------------------------------------------
    //
    // The new PML4 is loaded into CR3 *before* the jump. Two things depend on
    // the order:
    //
    // 1. The kernel's entry point is at `0xFFFF800000001920`, which the
    //    firmware's tables do not map. Without this the very first
    //    instruction fetch page-faults, which is the failure this whole
    //    design exists to fix.
    // 2. The pool — which holds the loader's own stack, the boot info and
    //    the page tables themselves — is identity-mapped by the new tables
    //    and by the firmware's, so the loader keeps running either way.
    //
    // The CR3 write is followed by a serialising fence, so the stores that
    // filled the page tables cannot be reordered past it and the CPU cannot
    // walk a table that is not yet written.
    log::info!(
        "entering kernel at {entry_addr:#x} with cr3 {:#x}",
        tables.pml4_paddr()
    );
    handoff(
        entry_addr,
        bi_paddr,
        loader_stack_paddr,
        tables.pml4_paddr(),
    )
}

/// Transfer control to the kernel. Never returns.
///
/// # Safety contract
///
/// This is the x86-64 handoff convention agreed with `kernel/entry`:
/// `RDI` receives the physical address of `VelaBootInfo`, and `RSP` is set to
/// a private stack from the boot pool. The kernel maps the BootInfo page
/// itself before dereferencing it, because at this point it is still only a
/// physical address.
///
/// `pml4_paddr` is the physical address of a fully populated PML4 that maps
/// the kernel's linked virtual base, and it becomes the new `CR3` before the
/// jump.
fn handoff(entry: u64, boot_info_paddr: u64, stack_paddr: u64, pml4_paddr: u64) -> ! {
    let stack_top = stack_paddr + LOADER_STACK_PAGES * 4096;
    // SAFETY: this is the documented handoff sequence. `rsp` is aligned to 16
    // and the alignment slot is cleared so the kernel starts with a valid
    // frame; `rbp` is zeroed so no caller frame chain is implied.
    //
    // The CR3 write is the loader's last act of address-space management: it
    // swaps the firmware's tables for the loader's, which map the kernel's
    // linked address. The `mfence` after it orders the page-table stores
    // before the CR3 store, so the CPU never sees a partially written tree.
    unsafe {
        core::arch::asm!(
            "mov cr3, {pml4}",
            "mfence",
            "mov rsp, {stack_top}",
            "and qword ptr [rsp - 8], 0",
            "mov rdi, {bi}",
            "xor ebp, ebp",
            "jmp {entry}",
            pml4 = in(reg) pml4_paddr,
            stack_top = in(reg) stack_top,
            bi = in(reg) boot_info_paddr,
            entry = in(reg) entry,
            options(noreturn)
        )
    }
}

/// Copy the UEFI memory map into the boot pool, translating the types.
///
/// The map must outlive Boot Services, so it cannot live in the `uefi` heap.
/// The descriptor count and stride are read from the firmware's own metadata
/// rather than assumed, because `desc_size` is frequently larger than
/// `size_of::<MemoryDescriptor>()`.
///
/// `ExitBootServices` invalidates the firmware's own map, so the kernel needs
/// its own copy of the descriptors.
///
/// # Why the types are translated
///
/// The kernel does not speak the firmware's `EFI_MEMORY_TYPE` numbering. The
/// boot handoff carries its own — see [`vela_boot_protocol::mem_type`] — and
/// this function is where the two meet. The translation happens here, once,
/// while Boot Services still exist and the raw descriptors are still readable,
/// so the kernel never has to know what the firmware's numbers are.
///
/// The translation is a *rewrite of the `type` field in place*: the
/// descriptor's layout is otherwise byte-for-byte the firmware's, and
/// rewriting one four-byte field is both cheaper and less error-prone than
/// re-encoding every descriptor through a struct.
fn persist_memory_map(pool: &mut pool::Pool) -> Result<(u64, u64, u32, u32), Status> {
    let mm = uefi::boot::memory_map(MemoryType::LOADER_DATA).map_err(|e| e.status())?;
    let meta = mm.meta();
    let desc_size = meta.desc_size;
    let desc_version = meta.desc_version;
    // `buffer()` is the firmware's raw byte image of the map, so it is copied
    // verbatim: re-encoding through `MemoryDescriptor` would drop any
    // descriptors this firmware extension added beyond the known struct.
    let raw: &[u8] = mm.buffer();
    let total = raw.len() as u64;
    log::info!(
        "memory map: {} bytes, {} entries x {desc_size} (version {desc_version})",
        total,
        meta.entry_count()
    );
    dump_memory_map(raw, desc_size as usize);

    // Allocate the copy *first*, in pool memory that survives the exit...
    let paddr = pool.alloc_pages(total, 16)?;
    // SAFETY: `paddr` covers `total` bytes, which is exactly `raw.len()`, and
    // `mm` is still alive so `raw` is a valid borrow of the firmware's buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(raw.as_ptr(), paddr as *mut u8, raw.len());
    }

    // Now translate the type of every descriptor in the copy.
    //
    // The offset of the `type` field is zero — it is the first field of the
    // `EFI_MEMORY_DESCRIPTOR` structure — and the stride is the firmware's
    // own, which may exceed the size of the struct.
    //
    // SAFETY: the copy is `total` bytes long and the firmware's map had
    // `total` bytes, so the same number of descriptors is present. Every
    // write is to a four-byte field at the start of a descriptor, and the
    // descriptor is at least 40 bytes long — so the write stays inside the
    // buffer.
    unsafe {
        translate_memory_types(paddr, total, desc_size as usize);
    }

    Ok((paddr, total, desc_size as u32, desc_version))
}

/// Rewrite the `type` field of every descriptor in a copied memory map.
///
/// # Why this exists
///
/// The firmware numbers its memory types one way and the boot handoff numbers
/// them another. If the loader copied the firmware's numbers through
/// unchanged, the kernel would apply its own table to the wrong values — and
/// the most damaging case is silent: the firmware's type `7` is
/// *conventional memory*, which is most of the machine, and a kernel whose
/// table believes `7` means "runtime services data" concludes that the
/// machine has almost no RAM. It then fails with a message about the
/// "largest usable region" being a few megabytes, which is true and tells
/// the reader nothing about the cause.
///
/// # Safety
///
/// `paddr` must be a readable, writable region of at least `total` bytes,
/// `total` must be the exact length of the copied descriptor buffer, and
/// `stride` must be the distance between consecutive descriptors.
unsafe fn translate_memory_types(paddr: u64, total: u64, stride: usize) {
    if stride < 40 {
        // A stride this small cannot describe a complete descriptor, so there
        // is no type field to rewrite. The kernel rejects such a map itself.
        return;
    }
    let mut offset = 0usize;
    while (offset as u64) + (stride as u64) <= total {
        let slot = (paddr as usize) + offset;
        // SAFETY: `slot` names the first four bytes of a descriptor inside the
        // copied buffer, and the loop condition proves the whole descriptor is
        // inside it.
        let firmware_kind = unsafe { core::ptr::read_unaligned(slot as *const u32) };
        let translated = vela_boot_protocol::mem_type::from_firmware(firmware_kind);
        // SAFETY: as above — a four-byte write to the same field.
        unsafe { core::ptr::write_unaligned(slot as *mut u32, translated) };
        offset += stride;
    }
}

/// Dump the firmware's memory map to the debug console.
///
/// # Why the loader dumps the map at all
///
/// The kernel's physical memory plan depends entirely on what the firmware
/// reports, and a wrong assumption about it — that the largest region is
/// below 4 GiB, say, or that the low megabytes are free — produces a boot
/// that fails much later and much less legibly. The map is the ground truth,
/// so it is logged: one line per descriptor, in the firmware's own order.
///
/// The dump is written before the exit, while the firmware's console is
/// still alive, and it goes to the debug console as well, so it survives into
/// the capture file.
fn dump_memory_map(raw: &[u8], desc_size: usize) {
    if desc_size < 40 {
        return;
    }
    let mut offset = 0usize;
    while offset + desc_size <= raw.len() {
        let kind = u32::from_le_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ]);
        let physical_start = u64::from_le_bytes([
            raw[offset + 8],
            raw[offset + 9],
            raw[offset + 10],
            raw[offset + 11],
            raw[offset + 12],
            raw[offset + 13],
            raw[offset + 14],
            raw[offset + 15],
        ]);
        let page_count = u64::from_le_bytes([
            raw[offset + 24],
            raw[offset + 25],
            raw[offset + 26],
            raw[offset + 27],
            raw[offset + 28],
            raw[offset + 29],
            raw[offset + 30],
            raw[offset + 31],
        ]);
        log::info!(
            "  mem type {kind} start {physical_start:#x} pages {page_count} ({:x} bytes)",
            page_count * 4096
        );
        offset += desc_size;
    }
}

/// The length of the image's executable prefix, rounded up to a page.
///
/// # Why this is computed from the *segments*
///
/// The kernel's linker script gives `.text` and `.data` separate output
/// sections, which the linker turns into separate `PT_LOAD` segments. That
/// split is what makes W^X possible: the executable prefix can be mapped
/// read-execute and the data that follows it read-write, without any page
/// holding both.
///
/// The boundary is therefore the end of the last segment that is
/// *executable*. A segment with no file extent (the `.bss` one) is data by
/// definition, and a segment that is not executable is data however it was
/// laid out.
///
/// # Why the executable prefix starts at the image's base
///
/// The prefix is measured from [`image_vaddr`], the image's lowest virtual
/// address, because that is the address the kernel maps. A segment that
/// starts above the base leaves a gap, and that gap is data — the kernel
/// image's first page holds the ELF program headers, which are never mapped,
/// but the mapping has to cover the range anyway.
fn executable_prefix_bytes(image: &Elf64Image, image_vaddr: u64) -> u64 {
    // The end of the last executable segment that has a file extent.
    let mut end = image_vaddr;
    for seg in &image.load_segments {
        if seg.file_size == 0 {
            // NOBITS: this is `.bss`, and everything from here on is data.
            continue;
        }
        if !seg.executable() {
            // A non-executable segment is data, whatever its position.
            continue;
        }
        let seg_end = seg.vaddr + seg.file_size;
        if seg_end > end {
            end = seg_end;
        }
    }
    if end <= image_vaddr {
        // No executable content at all: the whole image is treated as data,
        // which is the safe direction to be wrong in.
        return 0;
    }
    (end - image_vaddr).div_ceil(4096) * 4096
}

/// Copy a Rust path into a NUL-terminated UTF-16 buffer.
///
/// UEFI paths are UTF-16 while the config file is UTF-8, so every path read
/// out of the config has to be converted. This uses a stack buffer and no
/// allocation, because the allocator stops working the instant Boot Services
/// are exited.
///
/// # Panics
///
/// Panics if the path does not fit, which cannot happen for the config
/// `xtask image` generates and is a build-time mistake otherwise.
struct OwnedPath {
    buf: [uefi::Char16; 128],
    /// Length of the buffer in use, including the terminator.
    len: usize,
}

impl OwnedPath {
    fn new(s: &str) -> Self {
        let mut buf = [0u16; 128];
        let mut used = 0usize;
        for u in s.encode_utf16() {
            assert!(used < buf.len() - 1, "UEFI path too long: {s:?}");
            buf[used] = u;
            used += 1;
        }
        // The terminator, explicitly.
        //
        // `buf` starts as all zeros, so it *was* terminated — but only because
        // the array's initialiser is zero, and only at an index past the last
        // character written. Writing the NUL at `used` states the invariant
        // instead of depending on the initialiser, and `as_cstr`'s
        // `InteriorNul` panic was the bug that came from not stating it:
        // `from_char16_with_nul` requires a NUL *within* the slice, and a path
        // written right up to the array's end produced none.
        //
        // An interior NUL in the input is also rejected here rather than
        // silently truncating the path: a path with a NUL in it is malformed,
        // and truncating it would open a different file than the one named.
        assert!(
            !s.contains('\0'),
            "UEFI path contains an interior NUL: {s:?}",
        );
        buf[used] = 0;
        // SAFETY: `uefi::Char16` is a `#[repr(transparent)]` newtype over
        // `u16`, so the array has an identical layout to the `u16` buffer
        // filled above.
        let buf = unsafe { core::mem::transmute::<[u16; 128], [uefi::Char16; 128]>(buf) };
        Self { buf, len: used + 1 }
    }

    fn as_cstr(&self) -> &uefi::CStr16 {
        // The slice is trimmed to the terminator, and *that* is what makes the
        // checked constructor succeed.
        //
        // `from_char16_with_nul` does not look for a terminator and accept it —
        // it requires the first NUL it finds to be the **last element of the
        // slice**, returning `InteriorNul(index)` for anything else. Passing
        // the whole 128-entry buffer therefore always failed: the path's own
        // terminator sat at, say, index 29 with 98 more zeroed entries behind
        // it, which is precisely an "interior NUL" by that definition. The
        // error message pointed at index 29 — the *correct* terminator — which
        // is what made this so confusing to read.
        //
        // So the length is recorded at construction and the slice handed over
        // ends exactly at the terminator.
        //
        // `buf` is live for as long as `self` is, and `as_cstr` borrows from
        // `self`, so the returned reference cannot outlive the buffer.
        uefi::CStr16::from_char16_with_nul(&self.buf[..self.len])
            .expect("OwnedPath::new records a terminator at the end of its length")
    }
}
