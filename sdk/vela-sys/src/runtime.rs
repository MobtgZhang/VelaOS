//! The guest runtime: the allocator and panic handler every VelaOS program needs.
//!
//! # Why this lives in the SDK and not in each program
//!
//! A bare-metal `no_std` binary needs exactly two things the language does not
//! provide: a `#[global_allocator]` and a `#[panic_handler]`. Neither is
//! program logic. Both are *policy* — this is what "a VelaOS program" means —
//! and having three programs each declare their own would mean three places to
//! change when the heap changes, and three chances to get it subtly different.
//!
//! So the SDK provides them, and a program opts in with:
//!
//! ```ignore
//! use vela_sys::runtime;
//!
//! #[global_allocator]
//! static HEAP: runtime::GuestAllocator = runtime::GuestAllocator;
//!
//! #[panic_handler]
//! fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
//!     runtime::panic(info)
//! }
//! ```
//!
//! # Why a panic powers the machine off
//!
//! A guest panic means the program hit a case it did not expect: a null
//! library handle, an out-of-range argument, an allocator that could not
//! satisfy a request. There is no other process to catch it, no shell to return
//! to, and — on the boot path — no user watching who could type a response.
//!
//! Continuing would be worse than stopping. A desktop drawing on a window whose
//! creation silently failed looks like a graphics bug, and the next symptom
//! would be some unrelated corruption an hour later. Stopping with the panic
//! message on the console localises the fault to the exact point it happened,
//! which is the only thing that makes a bare-metal panic debuggable at all.

use core::panic::PanicInfo;

use crate::heap::GuestHeap;

/// The global allocator a VelaOS program installs.
///
/// A `GuestHeap` behind a `static`, because `#[global_allocator]` requires a
/// `'static` `GlobalAlloc` and the heap's state is per-process by virtue of
/// being in the program's own address space. Two programs therefore get two
/// heaps for free — there is no shared kernel object here, which is the whole
/// point of a bump heap behind a syscall.
pub struct GuestAllocator;

// SAFETY: `GuestHeap` is `Sync` and `Send` by its own unsafe impls, whose
// reasoning is that every field is atomic or lock-guarded. That is exactly the
// bound `GlobalAlloc` needs, and `GuestAllocator` holds no state of its own.
unsafe impl core::alloc::GlobalAlloc for GuestAllocator {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // SAFETY: forwarded from this function's contract.
        HEAP.grow(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {
        // A bump allocator cannot reclaim. See `heap`'s module documentation —
        // the limitation is stated there, not hidden here.
    }

    unsafe fn realloc(
        &self,
        ptr: *mut u8,
        layout: core::alloc::Layout,
        new_size: usize,
    ) -> *mut u8 {
        // SAFETY: forwarded from this function's contract.
        unsafe { HEAP.realloc(ptr, layout, new_size) }
    }

    unsafe fn alloc_zeroed(&self, layout: core::alloc::Layout) -> *mut u8 {
        // The kernel zeroes pages as it maps them, so a fresh bump block is
        // already zeroed and this is just `alloc`.
        // SAFETY: forwarded from this function's contract.
        unsafe { HEAP.alloc_zeroed(layout) }
    }
}

/// The process-wide heap.
static HEAP: GuestHeap = GuestHeap::new();

/// Report a panic and stop the machine.
///
/// # Safety
///
/// Never returns. The signature is `-> !` and the kernel's `power_off` does
/// not come back.
pub fn panic(info: &PanicInfo<'_>) -> ! {
    // Write the message directly rather than through `println!`. Formatting
    // allocates, and the allocator is the thing most likely to have failed —
    // a panic handler that allocates during an out-of-memory panic is a
    // second fault inside a fault. `info`'s own `Display` impl writes straight
    // to the console through the SDK's `print`, so no `String` is built.
    crate::api::print("\nGUEST-PANIC: ");
    crate::api::print_fmt(format_args!("{info}"));
    crate::api::print("\n");

    // A distinct exit status from a clean `power_off()`, so the boot smoke
    // test can tell "the guest finished" from "the guest crashed" rather than
    // treating every stop as success.
    crate::api::power_off_with(EXIT_PANIC)
}

/// The exit status a panicking guest reports.
///
/// Chosen to be distinct from every `power_off_with` status a guest uses on a
/// normal exit, because the boot smoke test's whole value is telling those two
/// outcomes apart.
pub const EXIT_PANIC: u32 = 0xDEAD;

/// The exit status a guest that completed its work reports.
///
/// The success marker: a guest that reaches the end of its session and stops
/// the machine with this status has demonstrably finished.
pub const EXIT_SUCCESS: u32 = 0;

/// How many bytes the guest heap has handed out.
///
/// For a boot log: a guest whose heap climbed to the ceiling has a problem a
/// reader should see before the machine stops.
pub fn heap_used() -> usize {
    HEAP.used()
}
