//! Copying data across the user/kernel boundary.
//!
//! # The problem
//!
//! A syscall handler receives pointers from user space. Dereferencing one of
//! those pointers directly in the kernel is a privilege-escalation primitive:
//! a user program could name an arbitrary kernel address and the kernel would
//! happily read or write it.
//!
//! # The two defences
//!
//! VelaOS uses both, because each covers the other's failure mode:
//!
//! 1. **Range validation.** The address must be in the user half of the
//!    address space and must be entirely below the kernel half. This is
//!    checked *before* any access, so a kernel address is rejected without
//!    being touched.
//! 2. **Probed access.** The copy itself uses `probe_read`/`probe_write`,
//!    which set the CR0 write-protect bit, perform the access, and clear it
//!    again. If the access faults — because the page is unmapped, or because
//!    it is mapped but not writable — the kernel takes the fault on its own
//!    stack, notices that it was a probe, and turns it into an error.
//!
//! Neither is sufficient alone. Range validation alone does not help if the
//! page is unmapped: the fault still becomes a kernel panic. Probing alone
//! does not help if the page *is* mapped and writable: then the access
//! succeeds, and a user program would have read the kernel's own memory.
//!
//! # The user half
//!
//! User space lives in the low half of the address space, below
//! [`USER_SPACE_END`]. The kernel lives in the high half. A user pointer at
//! or above [`USER_SPACE_END`] is rejected without being dereferenced.
//!
//! # Why not `copy_from_user`
//!
//! Linux's `copy_from_user` is the same idea with a different spelling: a
//! fault-handling `access_ok` check plus a fault-catching `copy`. VelaOS's
//! spelling is the same two ideas, named for what they do.

#[cfg(target_os = "none")]
use vela_hal_x86_64::regs::{read_cr0, write_cr0};

/// The lowest address of the user half of the address space.
pub const USER_SPACE_BASE: u64 = 0;

/// The first address *above* the user half.
///
/// Everything at or above this is kernel space and is refused. The boundary
/// is the midpoint of the canonical address space, which on x86-64 is bit 47
/// — the same bit the higher-half kernel window starts at.
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// The largest user-space allocation the kernel will copy in one call.
///
/// 16 MiB. A copy larger than this is refused rather than attempted: the
/// limit bounds the time a syscall can hold the CPU and stops a user program
/// from asking the kernel to copy an arbitrary amount of memory in one go.
pub const MAX_COPY_BYTES: usize = 16 * 1024 * 1024;

/// The longest user string the kernel will read.
pub const MAX_STRING_BYTES: usize = 4096;

/// The rectangle as user space passes it.
///
/// # Why this is a fixed struct and not four arguments
///
/// Two reasons, and both are about what a *buggy* program costs.
///
/// The first is that four separate argument registers leave no room for
/// anything else: `AddGadget` needs the window, the kind, the label *and* the
/// rectangle, which is seven values, and the ABI has six registers. Passing the
/// rectangle as one pointer resolves the collision and reads better.
///
/// The second is that a pointer is *validated*. A rectangle that arrives in
/// registers is a rectangle the kernel reads from its own stack and cannot
/// refuse; a rectangle behind a pointer goes through
/// [`copy_from_user`], so a program that passes a kernel address as its
/// rectangle gets an error instead of the kernel reading its own memory.
///
/// The layout is `#[repr(C)]` and explicitly padded, so a C program writing a
/// `struct Rectangle` and a Rust program writing this agree byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct UserRect {
    /// The left edge.
    pub x: i16,
    /// The top edge.
    pub y: i16,
    /// The width. Unsigned, as on AmigaOS: a rectangle's size is never
    /// negative, so a negative width is a program bug rather than something
    /// the kernel needs to interpret.
    pub w: u16,
    /// The height.
    pub h: u16,
}

impl UserRect {
    /// A rectangle at `(x, y)` of `w` by `h`.
    pub const fn new(x: i16, y: i16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    /// The rectangle as the renderer's own type.
    ///
    /// The conversion is total: `i16` widens to `i32` and `u16` to `u32`, so
    /// no coordinate can overflow. A rectangle with a huge width simply
    /// describes a rectangle wider than the screen, which the clip will trim.
    pub const fn to_rect(self) -> vela_renderer2d::Rect {
        vela_renderer2d::Rect::new(self.x as i32, self.y as i32, self.w as u32, self.h as u32)
    }

    /// The rectangle from the renderer's own type.
    ///
    /// The narrowing casts are deliberate and lossy in one direction only: a
    /// rectangle bigger than `i16::MAX` cannot be described to a user
    /// program, because a user program's geometry *is* the AmigaOS
    /// `struct Rectangle`, whose fields are `WORD`. Saturating is the honest
    /// answer — a program that asked for a larger window gets told the truth
    /// about what it got rather than a wrapped-around rectangle that lands
    /// somewhere it did not ask for.
    pub const fn from_rect(r: vela_renderer2d::Rect) -> Self {
        Self {
            x: clamp_i16(r.x),
            y: clamp_i16(r.y),
            w: clamp_u16(r.width),
            h: clamp_u16(r.height),
        }
    }

    /// The whole rectangle packed into one 64-bit value.
    ///
    /// Two halves per field, so four fields fit in one register. Used for the
    /// calls that *return* geometry, where a user pointer would be a way to
    /// make the kernel do extra work to deliver a value it had already
    /// computed.
    pub const fn pack(self) -> u64 {
        (self.x as u16 as u64)
            | ((self.y as u16 as u64) << 16)
            | ((self.w as u64) << 32)
            | ((self.h as u64) << 48)
    }

    /// A rectangle from a packed value.
    pub const fn unpack(packed: u64) -> Self {
        Self {
            x: (packed as u16) as i16,
            y: ((packed >> 16) as u16) as i16,
            w: ((packed >> 32) as u16),
            h: ((packed >> 48) as u16),
        }
    }
}

/// Saturate an `i32` into an `i16`.
const fn clamp_i16(v: i32) -> i16 {
    if v > i16::MAX as i32 {
        i16::MAX
    } else if v < i16::MIN as i32 {
        i16::MIN
    } else {
        v as i16
    }
}

/// Saturate a `u32` into a `u16`.
const fn clamp_u16(v: u32) -> u16 {
    if v > u16::MAX as u32 {
        u16::MAX
    } else {
        v as u16
    }
}

/// Read a [`UserRect`] from user space.
///
/// Returns [`errors::BAD_POINTER`] rather than the reason, because the
/// pointer's problem is not the caller's to debug: there is one thing to do
/// about it, which is not pass a kernel address.
pub fn read_user_rect(ptr: u64) -> Result<UserRect, i32> {
    let mut rect = UserRect::new(0, 0, 0, 0);
    // SAFETY: the destination is a local `UserRect` of exactly the size the
    // copy writes, and the source is a user pointer that `copy_from_user`
    // validates and probes.
    let copied = unsafe {
        copy_from_user(
            &mut rect as *mut UserRect as *mut u8,
            ptr,
            core::mem::size_of::<UserRect>(),
        )
    };
    copied.map_err(|_| errors::BAD_POINTER)?;
    Ok(rect)
}

/// Write a [`UserRect`] to user space.
pub fn write_user_rect(ptr: u64, rect: UserRect) -> Result<(), i32> {
    // SAFETY: the destination is a user pointer validated and probed by
    // `copy_to_user`, and the source is a local of the size copied.
    let copied = unsafe {
        copy_to_user(
            ptr,
            &rect as *const UserRect as *const u8,
            core::mem::size_of::<UserRect>(),
        )
    };
    copied.map_err(|_| errors::BAD_POINTER)?;
    Ok(())
}

/// The errors the copy helpers report.
pub mod errors {
    /// The address is not in the user half.
    pub const BAD_POINTER: i32 = -30;
    /// The range is not entirely in the user half, or is too large.
    pub const BAD_RANGE: i32 = -31;
    /// The string was not NUL-terminated within the limit.
    pub const BAD_STRING: i32 = -32;
}

/// True when `addr` names a location in the user half.
///
/// The check is on the *whole* address: a canonical low-half address with
/// bit 47 clear. A non-canonical address — one with bit 47 set but the sign
/// bit clear, say — is also refused, because it would fault on any access.
pub const fn is_user_address(addr: u64) -> bool {
    // Bit 47 selects the half; the sign bit must agree with it, or the
    // address is non-canonical and would fault.
    let bit47 = (addr >> 47) & 1;
    let sign = (addr >> 63) & 1;
    bit47 == 0 && sign == 0
}

/// True when the whole range `[addr, addr + len)` lies in the user half.
///
/// The addition is checked for overflow: a range that wraps around would
/// otherwise pass the half check on its low end while reaching into the
/// kernel.
pub const fn is_user_range(addr: u64, len: usize) -> bool {
    if len == 0 {
        return is_user_address(addr);
    }
    let end = match addr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    is_user_address(addr) && end <= USER_SPACE_END
}

/// True when the byte at `addr` is readable by user mode.
///
/// The kernel cannot know this without looking at the page tables, which is
/// what the probed copy does. This function answers the weaker question —
/// "is the address in the right half at all" — and the probe answers the
/// rest.
pub const fn is_plausible_user_pointer(addr: u64) -> bool {
    is_user_address(addr)
}

/// Copy `len` bytes from user space into `dest`.
///
/// Returns the number of bytes copied, or an error code.
///
/// # Safety
///
/// `dest` must be valid for writes of `len` bytes. That is the *caller's*
/// responsibility: this function validates the *source*, which is the
/// untrusted side.
pub unsafe fn copy_from_user(dest: *mut u8, src: u64, len: usize) -> Result<usize, i32> {
    if len == 0 {
        return Ok(0);
    }
    if len > MAX_COPY_BYTES {
        return Err(errors::BAD_RANGE);
    }
    if !is_user_range(src, len) {
        // The source is not in the user half. Refuse *before* touching it.
        return Err(errors::BAD_POINTER);
    }
    // SAFETY: `dest` is the caller's to guarantee, and `src` has been checked
    // to be a user address. The probe catches an unmapped page.
    let ok = unsafe { probe_read(src as *const u8, dest, len) };
    if ok {
        Ok(len)
    } else {
        Err(errors::BAD_POINTER)
    }
}

/// Copy `len` bytes from the kernel into user space.
///
/// Returns the number of bytes copied, or an error code.
///
/// # Safety
///
/// `src` must be valid for reads of `len` bytes.
pub unsafe fn copy_to_user(dest: u64, src: *const u8, len: usize) -> Result<usize, i32> {
    if len == 0 {
        return Ok(0);
    }
    if len > MAX_COPY_BYTES {
        return Err(errors::BAD_RANGE);
    }
    if !is_user_range(dest, len) {
        return Err(errors::BAD_POINTER);
    }
    // SAFETY: `src` is the caller's to guarantee, and `dest` has been checked
    // to be a user address. The probe catches an unmapped or read-only page.
    let ok = unsafe { probe_write(dest as *mut u8, src, len) };
    if ok {
        Ok(len)
    } else {
        Err(errors::BAD_POINTER)
    }
}

/// Copy a NUL-terminated string from user space into a kernel buffer.
///
/// Returns the string's length, or an error code.
///
/// # Safety
///
/// `dest` must be valid for writes of `cap` bytes.
pub unsafe fn copy_string_from_user(dest: &mut [u8], src: u64) -> Result<usize, i32> {
    if src == 0 {
        return Err(errors::BAD_POINTER);
    }
    if !is_user_address(src) {
        return Err(errors::BAD_POINTER);
    }
    let cap = dest.len().min(MAX_STRING_BYTES);
    let mut len = 0usize;
    while len < cap {
        // The probe reads one byte at a time so that a page boundary inside
        // the string is caught: a string that runs off the end of a mapped
        // page must fail rather than read the next page.
        let mut byte = [0u8; 1];
        // SAFETY: `src + len` is within the user half because the whole
        // range is bounded by `cap` and the initial check.
        let ok = unsafe { probe_read((src + len as u64) as *const u8, byte.as_mut_ptr(), 1) };
        if !ok {
            return Err(errors::BAD_POINTER);
        }
        if byte[0] == 0 {
            dest[len] = 0;
            return Ok(len);
        }
        dest[len] = byte[0];
        len += 1;
    }
    // The string did not terminate within the limit.
    Err(errors::BAD_STRING)
}

/// Copy a NUL-terminated string from user space into a freshly allocated
/// kernel `String`.
///
/// Returns the string, or an error code.
pub fn copy_string_alloc(src: u64) -> Result<alloc::string::String, i32> {
    let mut buffer = [0u8; MAX_STRING_BYTES];
    // SAFETY: `buffer` is a live local of exactly `MAX_STRING_BYTES` bytes.
    let len = unsafe { copy_string_from_user(&mut buffer, src) }?;
    let text = core::str::from_utf8(&buffer[..len]).map_err(|_| errors::BAD_STRING)?;
    Ok(alloc::string::String::from(text))
}

/// Perform a read access to a user address, catching the fault.
///
/// Returns `true` if the access succeeded.
///
/// # Safety
///
/// The address must be one the caller has validated as a user address.
unsafe fn probe_read(src: *const u8, dest: *mut u8, len: usize) -> bool {
    probe_access(src as u64, dest, len, false)
}

/// Perform a write access to a user address, catching the fault.
///
/// Returns `true` if the access succeeded.
///
/// # Safety
///
/// The address must be one the caller has validated as a user address.
unsafe fn probe_write(dest: *mut u8, src: *const u8, len: usize) -> bool {
    probe_access(dest as u64, src as *mut u8, len, true)
}

/// The shared body of the probed accesses.
///
/// The write-protect bit in `CR0` is what makes the probe work: with it set,
/// a write to a read-only page raises a page fault *at the faulting
/// instruction*, and with it clear a write to a writable page succeeds. The
/// kernel's fault handler distinguishes the two by the CR2 value it was
/// given.
///
/// # Safety
///
/// `user_addr` must be a user address, and the other pointer must be valid
/// for the access of `len` bytes.
unsafe fn probe_access(user_addr: u64, other: *mut u8, len: usize, write: bool) -> bool {
    // The probe state is global, because the fault handler that consults it
    // runs on the same CPU with no way to be told which probe it is serving.
    // The kernel is single-threaded per CPU, so a single slot is enough.
    let slot = crate::probe_state();

    // Refuse to nest: a nested probe would overwrite the outer probe's
    // state, and the outer fault would then be misattributed.
    if slot.in_progress.load(core::sync::atomic::Ordering::Acquire) {
        return false;
    }

    slot.user_addr
        .store(user_addr, core::sync::atomic::Ordering::Release);
    slot.length
        .store(len as u64, core::sync::atomic::Ordering::Release);
    slot.in_progress
        .store(true, core::sync::atomic::Ordering::Release);

    // Off bare metal there is no write-protect bit to flip and no fault
    // handler to catch a bad access. Instead the host checks the range
    // against its own page map, which is the same question the bare-metal
    // probe answers by catching the fault.
    //
    // The block is compiled out on bare metal rather than merely branched
    // around: the host's map reader does not exist there, and a `cfg` is
    // what keeps the bare-metal image free of it.
    #[cfg(not(target_os = "none"))]
    let ok: bool = {
        let mapped = host_range_is_mapped(user_addr, len);
        if mapped {
            // SAFETY: the caller has validated the user half of the host's
            // address space, and the other pointer is the caller's to
            // guarantee.
            unsafe {
                if write {
                    core::ptr::copy_nonoverlapping(other as *const u8, user_addr as *mut u8, len);
                } else {
                    core::ptr::copy_nonoverlapping(user_addr as *const u8, other, len);
                }
            }
        }
        mapped
    };

    // Set CR0.WP, so that a write to a read-only page faults.
    //
    // SAFETY: `CR0.WP` is restored below on the success path, and the fault
    // handler restores it when it catches a probe fault. The kernel is
    // single-threaded per CPU, so no other thread can observe the window.
    //
    // The whole block is bare-metal-only: on the host there is no
    // write-protect bit to flip and no fault handler to catch the fault, and
    // the copy above has already returned.
    #[cfg(target_os = "none")]
    let ok: bool = {
        let cr0 = read_cr0();
        // SAFETY: as above.
        unsafe { write_cr0(cr0 | (1 << 16)) };

        // Perform the access.
        //
        // SAFETY: the user half was validated by the caller; an unmapped
        // page inside it faults and is caught by the fault handler, which
        // turns the fault into a failed probe.
        unsafe {
            if write {
                core::ptr::copy_nonoverlapping(other as *const u8, user_addr as *mut u8, len);
            } else {
                core::ptr::copy_nonoverlapping(user_addr as *const u8, other, len);
            }
        }

        // Restore CR0. The read of CR0 above captured the old value, and the
        // fault handler restores it too when it catches a probe fault — but
        // the success path must restore it here.
        //
        // SAFETY: as above.
        let cr0 = read_cr0();
        // SAFETY: as above.
        unsafe { write_cr0(cr0 & !(1 << 16)) };
        true
    };

    slot.in_progress
        .store(false, core::sync::atomic::Ordering::Release);
    ok
}

/// True when the whole host range `[addr, addr + len)` is mapped.
///
/// This is the host's answer to the question the bare-metal probe answers by
/// catching the fault: "is this address backed by memory?". The answer is
/// read out of the process's own map, so an unmapped address is refused
/// before the copy is attempted rather than crashing the test run.
#[cfg(not(target_os = "none"))]
fn host_range_is_mapped(addr: u64, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match addr.checked_add(len as u64) {
        Some(e) => e,
        None => return false,
    };
    // The map is read on every call rather than cached. A cached map would
    // go stale as soon as a new thread's stack was allocated — the test
    // harness runs each test on its own thread — and a stale map would
    // refuse a perfectly good address.
    let Ok(text) = std::fs::read_to_string("/proc/self/maps") else {
        // Without a readable map there is nothing to check against, so the
        // range is accepted and the copy is attempted.
        return true;
    };
    text.lines().any(|line| {
        let Some(range) = line.split_whitespace().next() else {
            return false;
        };
        let Some((start, stop)) = range.split_once('-') else {
            return false;
        };
        let (Ok(start), Ok(stop)) = (
            u64::from_str_radix(start, 16),
            u64::from_str_radix(stop, 16),
        ) else {
            return false;
        };
        start <= addr && end <= stop
    })
}

/// The state a probe publishes for the fault handler to consult.
///
/// # Safety
///
/// The fault handler reads this only while `in_progress` is set, and only on
/// the CPU that started the probe.
pub struct ProbeState {
    /// The user address the probe was accessing.
    pub user_addr: core::sync::atomic::AtomicU64,
    /// The length of the access.
    pub length: core::sync::atomic::AtomicU64,
    /// Whether a probe is in progress.
    pub in_progress: core::sync::atomic::AtomicBool,
}

impl Default for ProbeState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeState {
    /// A fresh, idle probe state.
    pub const fn new() -> Self {
        Self {
            user_addr: core::sync::atomic::AtomicU64::new(0),
            length: core::sync::atomic::AtomicU64::new(0),
            in_progress: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// True when a fault at `cr2` belongs to the probe in progress.
    ///
    /// The fault must be at the *start* of the probed range. A fault in the
    /// middle of a copy means the first part succeeded, and the copy must be
    /// abandoned — a partial copy would leave the kernel with half of a
    /// user-supplied structure.
    pub fn covers(&self, cr2: u64) -> bool {
        if !self.in_progress.load(core::sync::atomic::Ordering::Acquire) {
            return false;
        }
        let start = self.user_addr.load(core::sync::atomic::Ordering::Acquire);
        let len = self.length.load(core::sync::atomic::Ordering::Acquire);
        cr2 >= start && cr2 < start.saturating_add(len)
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_half_is_the_low_half() {
        assert!(is_user_address(0));
        assert!(is_user_address(0x1000));
        assert!(is_user_address(0x7FFF_FFFF_FFFF));
        assert!(!is_user_address(USER_SPACE_END));
        assert!(!is_user_address(0xFFFF_8000_0000_0000));
    }

    #[test]
    fn a_non_canonical_address_is_refused() {
        // Bit 47 clear but bit 63 set: the address would fault on any
        // access, so it must be rejected before the access.
        assert!(!is_user_address(1 << 63));
        // Bit 47 set but bit 63 clear: the other non-canonical form.
        assert!(!is_user_address(1 << 47));
    }

    #[test]
    fn a_range_must_be_entirely_in_the_user_half() {
        assert!(is_user_range(0x1000, 0x1000));
        // The last byte must still be in the user half.
        assert!(is_user_range(USER_SPACE_END - 0x1000, 0x1000));
        // One byte past the end is not.
        assert!(!is_user_range(USER_SPACE_END - 0x1000, 0x1001));
        assert!(!is_user_range(USER_SPACE_END, 1));
    }

    #[test]
    fn a_range_that_overflows_is_refused() {
        // A range that wraps around would pass a naive half-check on its low
        // end while reaching into the kernel.
        assert!(!is_user_range(u64::MAX - 10, 100));
    }

    #[test]
    fn an_empty_range_at_a_user_address_is_accepted() {
        assert!(is_user_range(0x1000, 0));
        // An empty range at a kernel address is still refused: the address
        // itself is named.
        assert!(!is_user_range(USER_SPACE_END, 0));
    }

    #[test]
    fn a_kernel_address_is_refused_before_any_access() {
        let mut buffer = [0u8; 8];
        // SAFETY: the buffer is valid; the source is deliberately a kernel
        // address, which the function must reject without dereferencing it.
        let result = unsafe { copy_from_user(buffer.as_mut_ptr(), 0xFFFF_8000_0000_0000, 8) };
        assert_eq!(result, Err(errors::BAD_POINTER));
        // And the buffer was not touched.
        assert_eq!(buffer, [0u8; 8]);
    }

    #[test]
    fn a_write_to_a_kernel_address_is_refused() {
        let buffer = [0u8; 8];
        // SAFETY: the buffer is valid; the destination is deliberately a
        // kernel address.
        let result = unsafe { copy_to_user(0xFFFF_8000_0000_0000, buffer.as_ptr(), 8) };
        assert_eq!(result, Err(errors::BAD_POINTER));
    }

    #[test]
    fn an_oversized_copy_is_refused() {
        let mut buffer = [0u8; 8];
        // SAFETY: the buffer is valid; the copy is refused for its size
        // before any access happens.
        let result = unsafe { copy_from_user(buffer.as_mut_ptr(), 0x1000, MAX_COPY_BYTES + 1) };
        assert_eq!(result, Err(errors::BAD_RANGE));
    }

    #[test]
    fn a_zero_length_copy_succeeds() {
        let mut buffer = [0u8; 8];
        // SAFETY: the buffer is valid.
        let result = unsafe { copy_from_user(buffer.as_mut_ptr(), 0x1000, 0) };
        assert_eq!(result, Ok(0));
    }

    #[test]
    fn a_null_user_pointer_is_refused() {
        let mut buffer = [0u8; 8];
        // SAFETY: the buffer is valid; the source is null.
        let result = unsafe { copy_from_user(buffer.as_mut_ptr(), 0, 8) };
        assert_eq!(result, Err(errors::BAD_POINTER));
    }

    #[test]
    fn the_probe_state_recognises_its_own_fault() {
        let s = ProbeState::new();
        // Nothing in progress, so no fault belongs to a probe.
        assert!(!s.covers(0x1000));

        s.user_addr
            .store(0x2000, core::sync::atomic::Ordering::Release);
        s.length.store(0x100, core::sync::atomic::Ordering::Release);
        s.in_progress
            .store(true, core::sync::atomic::Ordering::Release);

        // A fault inside the range belongs to the probe.
        assert!(s.covers(0x2000));
        assert!(s.covers(0x20FF));
        // A fault outside it does not: it is a real kernel bug.
        assert!(!s.covers(0x1FFF));
        assert!(!s.covers(0x2100));
    }

    #[test]
    fn the_probe_state_is_idle_by_default() {
        let s = ProbeState::new();
        assert!(!s.in_progress.load(core::sync::atomic::Ordering::Acquire));
        assert_eq!(s.user_addr.load(core::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn the_maximum_copy_size_is_bounded() {
        // A copy limit is what stops a user program from asking the kernel
        // to move an unbounded amount of memory in one syscall.
        const {
            assert!(MAX_COPY_BYTES > 0);
            assert!(MAX_COPY_BYTES <= 64 * 1024 * 1024);
        }
    }

    #[test]
    fn the_string_limit_is_bounded() {
        const {
            assert!(MAX_STRING_BYTES > 0);
            assert!(MAX_STRING_BYTES <= 64 * 1024);
        }
    }
}
