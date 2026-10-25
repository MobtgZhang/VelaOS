//! The VelaOS kernel binary.
//!
//! This is the ELF the UEFI loader loads and jumps to. Its only job is to own
//! the three things that must exist exactly once in a kernel image:
//!
//! * the `#[global_allocator]`, backed by the kernel heap,
//! * the `#[panic_handler]`, which must not allocate and must power the
//!   machine off rather than reboot,
//! * the entry symbol, `kernel_entry`, which the loader jumps to.
//!
//! Everything those three need lives in the library half of this crate
//! (`src/lib.rs`) so it can be unit tested on the host, where a `no_std`
//! kernel entry point cannot be run at all.

// The binary only exists for the bare-metal kernel. Under `cargo test` the
// crate is built for the host, where `no_main` has no meaning and the host
// provides its own `main`; compiling the kernel binary there would fail to
// link and would also conflict with `std`'s panic handler.
//
// The gate is the *target*, not the test mode: `cargo build --workspace` on
// the host builds this binary too, and it must not try to be a kernel there.
// A host build of this file therefore produces an empty program, and the
// kernel is built by `xtask` with `--target x86_64-unknown-none`.
#![cfg_attr(all(not(test), target_os = "none"), no_std)]
#![cfg_attr(all(not(test), target_os = "none"), no_main)]

#[cfg(all(not(test), target_os = "none"))]
use vela_kernel_memory::HeapGlobalAlloc;

/// The kernel heap is the only allocator a kernel has after
/// `ExitBootServices`.
#[cfg(all(not(test), target_os = "none"))]
#[global_allocator]
static GLOBAL: HeapGlobalAlloc = HeapGlobalAlloc;

/// The kernel's panic handler.
///
/// The implementation lives in `vela-kernel-core` behind the `panic-handler`
/// feature, which this binary enables. Keeping the lang item here rather than
/// in the library is what lets `vela-kernel-entry` be linked as a dependency
/// of the hosted test harness without becoming a duplicate lang item.
#[cfg(all(not(test), target_os = "none"))]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    vela_kernel_core::report_panic(info)
}

/// Anchor the kernel entry point.
///
/// # Why this exists
///
/// The kernel is loaded and jumped to by absolute address, not called through
/// a linker-provided entry point, so the symbol must survive `--gc-sections`.
/// Nothing in a `no_main` binary *calls* `kernel_entry`, and an unreferenced
/// function in a section with no roots is collected — which produces an ELF
/// with no entry point at all. Taking its address from a `#[used]` static makes
/// it reachable from a root, which is exactly what is needed here.
///
/// The static goes in its own `.vela.anchor` section rather than
/// `.init_array`. `.init_array` is *run* by the C runtime before `main`, and
/// a `no_std` kernel has no C runtime to run it — so an entry point in
/// `.init_array` would be linked correctly and never called. The linker script
/// places `.vela.anchor` inside the loaded image and discards nothing, which
/// is what a pure reachability anchor needs.
#[cfg(all(not(test), target_os = "none"))]
mod entry_anchor {
    /// A forced reference to the kernel entry point.
    ///
    /// `#[used]` keeps the linker from garbage-collecting the section.
    #[used]
    #[cfg_attr(target_os = "none", link_section = ".vela.anchor")]
    static ANCHOR: unsafe extern "C" fn(u64) -> ! = vela_kernel_entry::kernel_entry;
}

/// The host build's entry point.
///
/// The kernel binary is not a host program, so this does nothing. It exists
/// so that `cargo build --workspace` on the host produces a binary that
/// links, rather than one that tries to be a kernel and fails.
#[cfg(not(all(not(test), target_os = "none")))]
fn main() {}
