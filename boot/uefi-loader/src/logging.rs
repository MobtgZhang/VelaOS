//! Logging for the loader, on the one device that works during Boot Services.
//!
//! # Why this file exists
//!
//! The loader was calling `log::info!` and `log::error!` throughout, and
//! `log::set_max_level` was being called to turn the messages on — but no
//! *logger* was ever installed. `set_max_level` sets the filter; it does not
//! provide anything to filter. With no logger, every `log::` macro expands to
//! a call on a no-op, so the loader ran, failed, and said nothing. That is the
//! worst possible failure mode for a bootloader: not a crash, but silence.
//!
//! # Why the serial port is not the answer
//!
//! Raw writes to port `0x3F8` are **invisible** while firmware Boot Services
//! run, because the firmware owns COM1 for its own console. A `-serial
//! file:` capture shows nothing. This was verified on this machine before the
//! design was chosen, and it is why the loader logs through
//! [`log::Logger::log`] to the *debug console* instead.
//!
//! # Why the debug console
//!
//! Port `0xE9` is QEMU's debug console: it is a port, not a device, so nothing
//! initialises it and nothing else competes for it. QEMU's `-debugcon
//! file:PATH` captures every byte written to it, which gives the loader a log
//! channel that survives firmware console ownership.
//!
//! The kernel cannot use this port — after `ExitBootServices` the loader's own
//! serial driver takes COM1 — so the two stages log through different devices
//! by necessity, not by preference. `boot-smoke` therefore asserts on the
//! loader's markers in the debugcon capture and the kernel's in the serial one.
//!
//! # Why also write to the UEFI console
//!
//! The debug console is invisible to a human. `uefi::println!` draws on the
//! screen, so a failure is readable by a person at the machine without any log
//! plumbing at all. The two together mean a loader failure is visible whether
//! or not anyone captured a debugcon file.

use alloc::string::String;
use core::fmt::Write as _;
use core::sync::atomic::Ordering;

/// The QEMU/Bochs debug console I/O port.
const DEBUGCON_PORT: u16 = 0xE9;

/// The loader's logger.
///
/// A unit struct because it holds no state: it writes to a port, and a port
/// needs no configuration. A `Logger` must be `'static` because `log::set_logger`
/// takes a `&'static` reference for the process's lifetime, which is exactly
/// right here — there is one logger and it exists from the first instruction.
pub struct LoaderLog;

/// Whether the firmware console and the `uefi` allocator are still usable.
///
/// Starts true and is cleared by [`boot_services_gone`] immediately after
/// `ExitBootServices`. The flag is the *only* thing standing between a log call
/// and an allocation on a dead allocator, so it is checked on every message
/// rather than cached in a handle — a cached handle would still be a pointer
/// into memory the firmware owns.
static CONSOLE_ALIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

/// Stop the logger from touching the firmware console or the `uefi` allocator.
///
/// Call immediately after `ExitBootServices` and before any other logging. The
/// debug console keeps working — it is a port, not firmware memory — so the
/// post-exit handoff remains observable, which is what makes a failure at that
/// exact point diagnosable instead of silent.
pub fn boot_services_gone() {
    CONSOLE_ALIVE.store(false, core::sync::atomic::Ordering::Relaxed);
}

/// True once [`boot_services_gone`] has been called.
///
/// Read by the panic handler to decide whether the firmware can still be asked
/// to reset — it cannot after the exit, and calling into a reclaimed firmware
/// table from a panic is a fault inside a fault.
pub fn boot_services_exited() -> bool {
    !CONSOLE_ALIVE.load(core::sync::atomic::Ordering::Relaxed)
}

// SAFETY: `LoaderLog` contains no data and all its methods are pure port writes
// and console output. `log::Log` requires `Sync` because logging can happen
// from any thread; there is no state to synchronise.
impl log::Log for LoaderLog {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        // The filter is already applied by `log::set_max_level`, so the only
        // question left is whether there is anywhere to write. Always true:
        // the debug console always exists on QEMU, and the UEFI console
        // always exists under Boot Services.
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        // Port `0xE9`. Written one byte at a time because the debug console
        // has no line buffering — it takes a byte and emits it immediately.
        //
        // SAFETY: writing a byte to the debug console is side-effecting but
        // cannot fault: the port exists on QEMU and Bochs, and a machine
        // without it ignores writes to an unclaimed port. A panic handler that
        // could fault would be worse than a lost log line.
        unsafe { write_debugcon(record.level(), record.args()) };

        // And to the firmware's console, so the message is on screen for anyone
        // watching the machine rather than only in a captured file.
        //
        // **Only before `ExitBootServices`.** The UEFI console handle lives in
        // firmware memory that the firmware may reclaim the instant Boot
        // Services go, and `uefi::print!` allocates on the `uefi` global
        // allocator — which is backed by Boot Services and is dead afterwards.
        // Both were observed as `memory allocation of 11 bytes failed` at the
        // two log calls immediately after the exit: the `String` the
        // formatting needed could not be built.
        //
        // So the console write is gated on a flag the loader clears right after
        // the exit, and the debug console — which is a *port*, not firmware
        // memory, and allocates nothing — keeps working. That is the whole
        // reason the debugcon path exists, and this is where it earns it.
        if CONSOLE_ALIVE.load(Ordering::Relaxed) {
            // SAFETY: the flag being true is precisely the guarantee that Boot
            // Services exist, which is this function's precondition.
            unsafe { write_uefi_console(record.level(), record.args()) };
        }
    }

    fn flush(&self) {
        // Both sinks are unbuffered: port writes are immediate and the UEFI
        // console draws as it is given characters. Nothing to flush, and
        // claiming so is more honest than an empty loop that implies buffering
        // that does not exist.
    }
}

/// A fixed-capacity byte buffer that is also a `fmt::Write` sink.
///
/// Exists so `write_debugcon` has one local type instead of a closure and an
/// inline `fmt::Write` adapter that would both borrow the buffer mutably. The
/// closure version did not compile for exactly that reason, and the borrow
/// checker was right: two mutable views of one buffer is the bug, not the
/// error.
struct FixedBuf {
    bytes: [u8; 256],
    len: usize,
}

impl FixedBuf {
    /// Append one byte, dropping it if the buffer is full.
    fn push(&mut self, b: u8) {
        if self.len < self.bytes.len() {
            self.bytes[self.len] = b;
            self.len += 1;
        }
    }
}

impl core::fmt::Write for FixedBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Writes past the end are truncated rather than reported as an error.
        // A log line that is too long is a nuisance; a `log::Log` that returns
        // `Err` can propagate a failure into code that has no way to handle it.
        let room = self.bytes.len().saturating_sub(self.len);
        let n = s.len().min(room);
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Write `args` to the debug console at `0xE9`.
unsafe fn write_debugcon(level: log::Level, args: &core::fmt::Arguments<'_>) {
    // Formatted into a fixed buffer rather than a `String`: a logger that can
    // fail to allocate cannot report an out-of-memory condition. 256 bytes is
    // generous for a log line, and a line longer than that is truncated
    // rather than dropped — a truncated message still identifies the failure.
    let mut buf = FixedBuf {
        bytes: [0u8; 256],
        len: 0,
    };

    // The level prefix mirrors the kernel's format, so one reader can parse
    // both stages' output with the same rule.
    buf.push(b'[');
    for b in level.as_str().as_bytes() {
        buf.push(*b);
    }
    buf.push(b']');
    buf.push(b' ');

    // A short write is silently truncated: there is nothing useful to do about
    // a full log buffer at this point, and failing the log call would be worse
    // than losing the tail of one line.
    let _ = buf.write_fmt(*args);
    buf.push(b'\n');

    for &b in &buf.bytes[..buf.len] {
        // SAFETY: forwarded from this function's contract — port `0xE9` is
        // always available and writing a byte to it cannot fault.
        unsafe {
            core::arch::asm!("out dx, al", in("dx") DEBUGCON_PORT, in("al") b, options(nomem, nostack, preserves_flags));
        }
    }
}

/// Write one line to the firmware console, allocating one line's worth.
///
/// # Safety
///
/// Only callable while Boot Services exist: it allocates through the `uefi`
/// global allocator and reaches the firmware's console handle.
unsafe fn write_uefi_console(level: log::Level, args: &core::fmt::Arguments<'_>) {
    // One line, allocated once. Not `write!` per fragment — that would
    // allocate per argument and fail differently on a nearly-full heap.
    let mut line = String::new();
    let _ = writeln!(line, "VELA-BOOT [{level}] {args}");
    for ch in line.chars() {
        write_console_char(ch);
    }
}

/// Write one character to the firmware console.
fn write_console_char(ch: char) {
    // The UEFI console takes UCS-2, so a character outside the BMP cannot be
    // represented. It is dropped rather than substituted, because a log line
    // is not the place to invent a replacement for text the loader did not
    // produce.
    let Ok(unit) = u16::try_from(ch) else {
        return;
    };
    // `uefi::console` writes through the active console handle. Going through
    // the crate rather than an `out dx, al` to the firmware's own serial port
    // is deliberate: that port is the one the firmware is holding.
    uefi::print!("{unit}");
}

/// Install the loader's logger.
///
/// # Safety
///
/// Must be called exactly once, before any other thread exists. Calling it
/// twice panics inside `log::set_logger`, which the panic handler turns into a
/// reset — loud, and correct, because two loggers would both be writing to the
/// debug console.
pub unsafe fn install() {
    static LOGGER: LoaderLog = LoaderLog;
    // SAFETY: forwarded from this function's contract — called once, with no
    // other threads running.
    unsafe {
        let _ = log::set_logger(&LOGGER);
    }
    log::set_max_level(log::LevelFilter::Info);
}
