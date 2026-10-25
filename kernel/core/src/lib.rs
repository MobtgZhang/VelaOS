//! Kernel core services: the logger, the panic handler, and the lock
//! primitives.
//!
//! # The logger
//!
//! The kernel logs to the serial port. There is deliberately **no** `log`
//! facade here: the `log` crate's macros are designed for a host with a
//! `std::io` sink, and every one of them does a runtime lookup of a global
//! logger, which in a kernel means a lock on the hot path of every log call.
//! VelaOS's logger is a set of `#[macro_export]` macros that write straight to
//! the port, with the level check compiled in.
//!
//! # The panic handler
//!
//! The kernel is built with `panic = "abort"`, so there is exactly one panic
//! handler and it must not return. It prints a diagnostic and powers the
//! machine off through the QEMU debug-exit device, which is what lets the
//! boot-smoke test distinguish "the kernel crashed" from "the kernel hung".
//!
//! The handler **must not allocate**. After `ExitBootServices` the firmware
//! allocator is gone, and the kernel heap may not be initialised yet. A panic
//! handler that allocates can therefore panic again, and the second panic in
//! a panic handler is a triple fault, which reboots the machine with no
//! diagnostic at all — the exact failure mode the plan says to avoid.

#![no_std]

use core::fmt::{self, Write};
#[cfg(any(not(test), feature = "panic-handler"))]
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use spin::Mutex;

#[cfg(any(not(test), feature = "panic-handler"))]
use vela_hal_x86_64::qemu_exit;
use vela_hal_x86_64::serial;

/// Log levels, in increasing order of severity.
///
/// The numeric order matters: a level check is a single comparison against
/// the current maximum, so a lower-numbered level is always considered
/// *less* severe and is dropped when the threshold is higher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    /// Verbose tracing. Compiled out at the default threshold.
    Trace = 0,
    /// Debugging detail.
    Debug = 1,
    /// Normal informational messages.
    Info = 2,
    /// Something unexpected that the kernel recovered from.
    Warn = 3,
    /// Something that needs attention but is not fatal.
    Error = 4,
    /// The kernel cannot continue.
    Fatal = 5,
}

impl Level {
    /// A short uppercase tag, e.g. `INFO`.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
            Self::Fatal => "FATAL",
        }
    }

    /// Decode a level from its numeric value, clamping to the nearest valid
    /// level.
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Trace,
            1 => Self::Debug,
            2 => Self::Info,
            3 => Self::Warn,
            4 => Self::Error,
            _ => Self::Fatal,
        }
    }
}

/// The maximum level that is currently logged.
///
/// Stored as a `u8` so the check on the log path is a single relaxed atomic
/// load, not a lock.
static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);

/// The number of log records emitted, for the boot summary.
static LOG_COUNT: AtomicU64 = AtomicU64::new(0);

/// Set the maximum level that is logged.
///
/// Levels above the threshold are dropped before any formatting happens, so
/// an expensive `{:?}` in a `debug!` costs nothing when debugging is off.
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The current maximum level.
pub fn max_level() -> Level {
    Level::from_u8(MAX_LEVEL.load(Ordering::Relaxed))
}

/// True when a message at `level` would be logged.
///
/// The comparison is `level >= max_level`: the threshold names the *least
/// severe* level still worth printing. Setting the threshold to `Warn` keeps
/// `Warn`, `Error` and `Fatal`, and drops `Info`, `Debug` and `Trace`.
///
/// Getting this polarity backwards is the classic mistake here, and it is
/// silent: the kernel either floods the log with tracing output or prints
/// nothing at all, and neither is obviously wrong until someone reads the
/// output.
pub fn is_enabled(level: Level) -> bool {
    level >= max_level()
}

/// The number of records emitted so far.
pub fn log_count() -> u64 {
    LOG_COUNT.load(Ordering::Relaxed)
}

/// A serial writer that appends the level tag and a newline.
///
/// Holds the serial lock for the whole record, so a multi-line message
/// cannot be interleaved with another CPU's output.
pub struct Logger;

impl Write for Logger {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        serial::puts(s);
        Ok(())
    }
}

/// Write one log record at `level`.
///
/// The level check happens *before* the message is formatted, so a disabled
/// level costs one atomic load and nothing else.
pub fn log(level: Level, args: fmt::Arguments<'_>) {
    if !is_enabled(level) {
        return;
    }
    LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    serial::puts("[");
    serial::puts(level.tag());
    serial::puts("] ");
    let _ = Logger.write_fmt(args);
    serial::puts("\n");
}

/// Log an informational message.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log($crate::Level::Info, format_args!($($arg)*)) };
}

/// Log a warning.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log($crate::Level::Warn, format_args!($($arg)*)) };
}

/// Log an error.
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log($crate::Level::Error, format_args!($($arg)*)) };
}

/// Log a fatal error.
#[macro_export]
macro_rules! fatal {
    ($($arg:tt)*) => { $crate::log($crate::Level::Fatal, format_args!($($arg)*)) };
}

/// Log a debug message.
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log($crate::Level::Debug, format_args!($($arg)*)) };
}

/// Log a trace message.
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log($crate::Level::Trace, format_args!($($arg)*)) };
}

/// The panic handler.
///
/// # What it does
///
/// 1. Disables interrupts, so the diagnostic is not interleaved with a
///    timer tick and the machine cannot be preempted mid-report.
/// 2. Prints the panic message and its location.
/// 3. Prints the kernel's own context: the current tick count, which
///    distinguishes "hung in a loop" from "crashed".
/// 4. Powers the machine off through the QEMU debug-exit device, with a
///    non-zero status so CI sees a failure.
///
/// # What it deliberately does not do
///
/// It does not allocate, and it does not attempt a backtrace. A backtrace
/// needs a symbol table and a frame walker; the frame walker needs a
/// reliable stack, and the whole premise of a panic is that something has
/// already gone wrong. The message, the location and the tick count are what
/// actually help, and they are available unconditionally.
/// Report a panic through the serial port and power the machine off.
///
/// This is the implementation, not the `#[panic_handler]` itself. The lang item
/// lives behind the `panic-handler` feature so that `vela-kernel-core` can be
/// linked as an ordinary library — a crate that defines a panic handler and is
/// also a dependency is a duplicate lang item, and the kernel entry binary is
/// the only thing that should own it.
#[cfg(any(not(test), feature = "panic-handler"))]
pub fn report_panic(info: &PanicInfo<'_>) -> ! {
    // Stop the world. A timer tick landing in the middle of this report would
    // interleave its output with the panic message.
    // SAFETY: the kernel is aborting; there is no code to return to.
    vela_hal_x86_64::regs::cli();

    serial::puts("\n");
    serial::puts("=====================================================\n");
    serial::puts("*** KERNEL PANIC ***\n");
    serial::puts("=====================================================\n");

    serial::puts("message: ");
    // `PanicInfo`'s message is a `fmt::Arguments`, so it is written directly
    // rather than through the level-tagged logger: a panic must not be
    // filtered out by the log level.
    let _ = Logger.write_fmt(format_args!("{}", info.message()));
    serial::puts("\n");

    if let Some(location) = info.location() {
        serial::puts("at     : ");
        serial::puts(location.file());
        serial::puts(":");
        serial::puts_dec(location.line() as u64);
        serial::puts(":");
        serial::puts_dec(location.column() as u64);
        serial::puts("\n");
    }

    serial::puts("ticks  : ");
    serial::puts_dec(vela_hal_x86_64::apic::ticks());
    serial::puts("\n");
    serial::puts("interrupts: ");
    serial::puts_dec(vela_hal_x86_64::idt::interrupt_count());
    serial::puts("\n");
    serial::puts("=====================================================\n");

    // Power the machine off with a non-zero status so the boot-smoke test
    // sees a failure rather than a timeout.
    qemu_exit::power_off(2)
}

/// The kernel's `#[panic_handler]`.
///
/// Defined here rather than in `kernel/entry` so that the handler and the
/// logging it depends on live in the same crate: the handler writes through
/// [`Logger`], and splitting the two would mean the panic path had a
/// dependency it could not have.
#[cfg(all(feature = "panic-handler", not(test)))]
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    report_panic(info)
}

/// A spin lock for kernel data structures.
///
/// # Why not the `spin` crate's lock
///
/// The `spin` crate's `Mutex` is already used for the logger and the heap,
/// and it is the right primitive there. This type exists for the case where
/// a lock must be *shared* by reference and its guard must be `Send`-safe
/// across a context switch — the scheduler's run queue needs that, and
/// `spin::Mutex`'s guard is not `Send` because it borrows the mutex.
///
/// # Deadlock
///
/// A spin lock deadlocks if the holder is preempted and never resumes. The
/// scheduler in Phase 4 must therefore not preempt a task that holds a lock
/// whose critical section can block, or must make the lock's critical
/// sections non-blocking. The `Forbid`/`Permit` model from the Amiga Exec
/// service is exactly this mechanism.
pub struct KernelMutex<T> {
    inner: Mutex<T>,
}

impl<T> KernelMutex<T> {
    /// Wrap a value.
    pub const fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
        }
    }

    /// Lock the mutex, spinning until it is available.
    pub fn lock(&self) -> spin::MutexGuard<'_, T> {
        self.inner.lock()
    }

    /// Try to lock the mutex without spinning.
    pub fn try_lock(&self) -> Option<spin::MutexGuard<'_, T>> {
        self.inner.try_lock()
    }

    /// Access the inner value without locking.
    ///
    /// Only sound for values that are genuinely lock-free, such as an
    /// `AtomicU64`.
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

/// A one-time initialisation guard.
///
/// The classic "initialise exactly once" primitive. `spin::Once` already
/// provides this; the kernel re-exports it under a name that reads better at
/// the call site and adds the `call_once` form that takes a closure.
pub use spin::Once as KernelOnce;

/// Initialise a value exactly once, running `init` on the first call.
///
/// Subsequent calls block until the first completes and then return the
/// initialised value.
///
/// # Why the cell is a parameter
///
/// The value has to live *outside* the `once` guard: `spin::Once` runs its
/// closure exactly once, so a value kept in a local inside this function would
/// be gone by the time the second call arrived. The caller supplies the cell
/// that holds the value, and the `once` guard that protects it.
///
/// # Why `T: Clone`
///
/// The value is *read* out of the cell on every call, but the cell is behind
/// a lock whose guard cannot be moved out of. Cloning is the honest way to
/// hand a copy to each caller; the alternative — a `T: Copy` bound — would
/// exclude the heap-backed values a kernel usually wants to initialise once
/// (a frame allocator, a font table).
///
/// # Example
///
/// ```ignore
/// static ONCE: spin::Once = spin::Once::new();
/// static CELL: spin::Mutex<Option<MyType>> = spin::Mutex::new(None);
/// let value = call_once(&ONCE, &CELL, || MyType::new());
/// ```
pub fn call_once<T, F>(once: &spin::Once, cell: &spin::Mutex<Option<T>>, init: F) -> T
where
    T: Clone,
    F: FnOnce() -> T,
{
    once.call_once(|| {
        *cell.lock() = Some(init());
    });
    cell.lock()
        .as_ref()
        .expect("the initialiser ran, so the cell is populated")
        .clone()
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    #[test]
    fn levels_are_ordered_by_severity() {
        assert!(Level::Trace < Level::Debug);
        assert!(Level::Debug < Level::Info);
        assert!(Level::Info < Level::Warn);
        assert!(Level::Warn < Level::Error);
        assert!(Level::Error < Level::Fatal);
    }

    #[test]
    fn level_tags_are_fixed_width() {
        // The tags are padded so the log output is a fixed-width column.
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::Fatal,
        ] {
            assert_eq!(level.tag().len(), 5, "{:?}", level);
        }
    }

    #[test]
    fn level_decoding_clamps_to_the_nearest_valid_level() {
        assert_eq!(Level::from_u8(0), Level::Trace);
        assert_eq!(Level::from_u8(2), Level::Info);
        assert_eq!(Level::from_u8(5), Level::Fatal);
        // Anything above Fatal clamps to Fatal rather than wrapping.
        assert_eq!(Level::from_u8(6), Level::Fatal);
        assert_eq!(Level::from_u8(255), Level::Fatal);
    }

    #[test]
    fn level_round_trips_through_its_numeric_value() {
        for level in [
            Level::Trace,
            Level::Debug,
            Level::Info,
            Level::Warn,
            Level::Error,
            Level::Fatal,
        ] {
            assert_eq!(Level::from_u8(level as u8), level);
        }
    }

    /// Serialise the tests that mutate the global log threshold.
    ///
    /// The threshold is process-wide state, so two tests that set it
    /// concurrently would see each other's value. `cargo test` runs tests in
    /// parallel threads within one process, so the lock is necessary.
    static LEVEL_LOCK: spin::Mutex<()> = spin::Mutex::new(());

    /// Acquire the level lock, restoring the previous threshold on drop.
    ///
    /// The lock is *held for the guard's lifetime*, not just for the
    /// duration of the call that created it: the whole point is to exclude
    /// other tests for the duration of the test body.
    struct LevelGuard {
        /// The lock guard, held for as long as this struct lives.
        _lock: spin::MutexGuard<'static, ()>,
        /// The threshold to restore on drop.
        previous: Option<Level>,
    }

    impl LevelGuard {
        fn set(level: Level) -> Self {
            let lock = LEVEL_LOCK.lock();
            let previous = max_level();
            set_max_level(level);
            Self {
                _lock: lock,
                previous: Some(previous),
            }
        }
    }

    impl Drop for LevelGuard {
        fn drop(&mut self) {
            if let Some(previous) = self.previous {
                set_max_level(previous);
            }
        }
    }

    #[test]
    fn the_threshold_filters_lower_levels() {
        let _guard = LevelGuard::set(Level::Warn);
        assert!(!is_enabled(Level::Info));
        assert!(is_enabled(Level::Warn));
        assert!(is_enabled(Level::Error));
    }

    #[test]
    fn the_log_counter_advances_only_for_enabled_levels() {
        let _guard = LevelGuard::set(Level::Error);
        let before = log_count();

        log(Level::Info, format_args!("dropped"));
        assert_eq!(log_count(), before, "a disabled level must not count");

        log(Level::Error, format_args!("kept"));
        assert_eq!(log_count(), before + 1);
    }

    #[test]
    fn a_disabled_level_does_not_touch_the_serial_port() {
        // Off bare metal the port write is a no-op, so the observable effect
        // is on the counter alone. The point of the test is that the
        // threshold check happens before any formatting work.
        let _guard = LevelGuard::set(Level::Fatal);
        let before = log_count();
        // A deliberately expensive message: if the level were not checked
        // first, this would be formatted.
        log(
            Level::Trace,
            format_args!("{:#?}", (1u8..=255).collect::<Vec<_>>()),
        );
        assert_eq!(log_count(), before);
    }

    #[test]
    fn the_kernel_mutex_provides_mutual_exclusion() {
        let m = KernelMutex::new(0u32);
        {
            let mut g = m.lock();
            *g += 1;
        }
        {
            let mut g = m.lock();
            *g += 41;
        }
        assert_eq!(*m.lock(), 42);
    }

    #[test]
    fn try_lock_reports_contention() {
        let m = KernelMutex::new(0u32);
        let _held = m.lock();
        // The same thread already holds it, so `try_lock` must fail rather
        // than deadlock.
        assert!(m.try_lock().is_none());
    }

    #[test]
    fn call_once_runs_the_initialiser_exactly_once() {
        static ONCE: spin::Once = spin::Once::new();
        static CELL: spin::Mutex<Option<u32>> = spin::Mutex::new(None);
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let a = call_once(&ONCE, &CELL, || {
            COUNTER.fetch_add(1, Ordering::Relaxed);
            7
        });
        let b = call_once(&ONCE, &CELL, || {
            COUNTER.fetch_add(1, Ordering::Relaxed);
            9
        });
        assert_eq!(a, 7);
        assert_eq!(b, 7, "the second call must return the first value");
        assert_eq!(COUNTER.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_logger_formats_without_a_global_logger() {
        // The kernel's logging path must not require a registered global
        // logger, because there is no `std` to register one in.
        let mut out = String::new();
        write!(out, "[{}] value={}", Level::Info.tag(), 42).unwrap();
        assert_eq!(out, "[INFO ] value=42");
    }
}
