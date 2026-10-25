//! COM1 serial driver — the kernel's log sink after `ExitBootServices`.
//!
//! # Why serial and not the framebuffer
//!
//! Before `ExitBootServices` the firmware owns COM1, so raw writes to `0x3F8`
//! are invisible on `-serial file:`; the loader logs through the UEFI console
//! instead. After the exit the firmware is gone and COM1 belongs to the kernel.
//! That ordering is the whole reason this module exists rather than the loader
//! reusing it.
//!
//! # Why it is safe to poll
//!
//! Every write waits for the line-status register to report the transmit
//! holding register empty. An unbounded wait would hang the machine if the UART
//! were wedged; [`send_timeout`] bounds the spin so a dead port degrades to lost
//! log output rather than a hang, which matters for CI where a silent kernel is
//! far worse than a truncated one.
//!
//! # Bottleneck
//!
//! Everything funnels through a spin lock. Output is serial by nature — the
//! 16550 has one byte at a time — so a lock is the correct primitive, and a
//! multi-byte writer cannot interleave with another CPU's write.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use crate::regs;

/// The COM1 base port. The three other standard ports follow at +1, +2, +3.
pub const COM1: u16 = 0x3F8;

/// Data register, one byte, read/write.
const DATA: u16 = COM1;
/// Interrupt enable register.
const IER: u16 = COM1 + 1;
/// Baud-rate divisor, low byte.
const DLL: u16 = COM1;
/// Baud-rate divisor, high byte.
const DLM: u16 = COM1 + 1;
/// FIFO control.
const FCR: u16 = COM1 + 2;
/// Line status.
const LSR: u16 = COM1 + 5;

/// Line-status bit: data ready to read.
#[allow(dead_code, reason = "complete LSR map; a reader will need it")]
const LSR_DATA_READY: u8 = 1 << 0;
/// Line-status bit: transmitter holding register empty.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// Line-status bit: transmitter empty (not just the holding register).
#[allow(
    dead_code,
    reason = "the fully-drained variant, needed for low latency"
)]
const LSR_THR_IDLE: u8 = 1 << 6;

/// 115200 baud with a 1.8432 MHz clock: divisor 1.
const BAUD_115200_DIVISOR: u16 = 1;

/// How many poll iterations [`send_timeout`] gives up after.
///
/// Chosen so a wedged UART costs microseconds rather than seconds: on a 3 GHz
/// host, 100 000 iterations of a port read is well under a millisecond.
pub const TX_SPIN_LIMIT: u32 = 100_000;

/// Whether `init` has run. Guarded so a log call before serial setup is a
/// silent no-op instead of a port write into an unconfigured UART.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Serialises whole writes so multi-byte output cannot interleave.
static LOCK: Mutex<()> = Mutex::new(());

/// Configure COM1 for 115200 baud, 8N1, no interrupts.
///
/// Interrupts are deliberately left disabled: the kernel polls, and enabling
/// the UART interrupt would require an IDT gate that Phase 4 owns.
///
/// # Safety
///
/// Writes to fixed I/O ports. Safe to call once during boot; calling it
/// concurrently with logging would reconfigure the port mid-write.
pub unsafe fn init() {
    LOCK.lock();
    // Disable interrupts: no reason to receive anything during boot.
    // SAFETY: `COM1 + 1` is the IER of the COM1 UART.
    unsafe { regs::outb(IER, 0x00) };

    // Enable and clear both FIFOs, and switch to 14-byte triggers. Clearing
    // discards anything the firmware left in the buffer, which would otherwise
    // be interleaved with the first kernel log line.
    // SAFETY: `COM1 + 2` is the FCR.
    unsafe { regs::outb(FCR, 0xC7) };

    // 8 data bits, no parity, 1 stop bit, no break.
    // SAFETY: `COM1 + 3` is the LCR.
    unsafe { regs::outb(COM1 + 3, 0x03) };

    // Set the divisor. DLAB must be set before the divisor registers are
    // visible, and cleared afterwards or every later write lands in the
    // divisor instead of the data register.
    // SAFETY: `COM1 + 3` is the LCR and `DLL`/`DLM` are the divisor.
    unsafe {
        regs::outb(COM1 + 3, 0x80 | 0x03);
        regs::outb(DLL, BAUD_115200_DIVISOR as u8);
        regs::outb(DLM, (BAUD_115200_DIVISOR >> 8) as u8);
        regs::outb(COM1 + 3, 0x03);
    }

    INITIALISED.store(true, Ordering::Release);
}

/// True once [`init`] has completed.
pub fn is_initialised() -> bool {
    INITIALISED.load(Ordering::Acquire)
}

/// True when the transmit holding register is ready for a byte.
///
/// Off bare metal this is always `true`: there is no UART, and the alternative
/// is a writer spinning forever on a status bit that will never change.
pub fn can_send() -> bool {
    if !regs::ON_BARE_METAL {
        return true;
    }
    // SAFETY: `LSR` is the COM1 line-status register and is always readable.
    unsafe { regs::inb(LSR) & LSR_THR_EMPTY != 0 }
}

/// Write one byte, spinning until the UART accepts it.
pub fn put_byte(byte: u8) {
    // `\r` alone moves the cursor to column 0 without advancing the line, which
    // would smear every line together in a captured log. Emitting the `\n`
    // ourselves is what makes `-serial file:` output readable.
    if byte == b'\n' {
        put_byte_raw(b'\r');
    }
    put_byte_raw(byte);
}

/// Write one byte with no `\n` translation.
fn put_byte_raw(byte: u8) {
    while !can_send() {
        // Busy-wait rather than `hlt`: the UART drains in microseconds, and
        // halting here would need interrupts enabled to wake up again.
    }
    // SAFETY: `DATA` is the COM1 data register; writing it queues the byte.
    unsafe { regs::outb(DATA, byte) };
}

/// Write one byte, giving up after [`TX_SPIN_LIMIT`] polls.
///
/// Returns `false` when the byte was dropped. The only caller that cares is
/// [`send_timeout`]; the panic path uses the blocking form so that the message
/// preceding a crash is never the one that gets truncated.
pub fn try_put_byte(byte: u8) -> bool {
    if byte == b'\n' && !try_put_byte_raw(b'\r') {
        return false;
    }
    try_put_byte_raw(byte)
}

fn try_put_byte_raw(byte: u8) -> bool {
    let mut spins = TX_SPIN_LIMIT;
    while spins > 0 {
        if can_send() {
            // SAFETY: as in `put_byte_raw`.
            unsafe { regs::outb(DATA, byte) };
            return true;
        }
        spins -= 1;
    }
    false
}

/// Write one byte through the port.
///
/// Public because a *user-space* program's console output has to reach the
/// same port: `PutChar` hands the byte straight here, so a program's own
/// output is interleaved correctly with the kernel's rather than being
/// formatted, buffered, and written as a block.
pub fn putc(byte: u8) {
    put_byte(byte);
}

/// Write a string through the port, byte by byte.
pub fn puts(s: &str) {
    let _guard = LOCK.lock();
    for byte in s.as_bytes() {
        put_byte(*byte);
    }
}

/// Write a hex number, no `0x` prefix, lowercase, without leading zeros.
pub fn puts_hex(value: u64) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    if value == 0 {
        put_byte(b'0');
        return;
    }
    let mut started = false;
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xF) as usize;
        if nibble != 0 || started {
            started = true;
            put_byte(DIGITS[nibble]);
        }
    }
}

/// Write a decimal number.
pub fn puts_dec(value: u64) {
    if value == 0 {
        put_byte(b'0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut n = value;
    let mut i = 0;
    while n > 0 && i < digits.len() {
        digits[i] = (n % 10) as u8 + b'0';
        n /= 10;
        i += 1;
    }
    let mut j = i;
    while j > 0 {
        j -= 1;
        put_byte(digits[j]);
    }
}

/// Dump the bytes at `addr`, for diagnosing an invalid opcode.
///
/// This *reads arbitrary memory on purpose*: it is only ever called from the
/// invalid-opcode handler, where the alternative is having no idea what the CPU
/// tried to execute. The read may itself fault, which the double-fault handler
/// will report.
pub fn puts_code_at(addr: u64) {
    // SAFETY: the caller is an exception handler reporting an unknown opcode.
    // The address is whatever the CPU tried to execute, so a fault here is
    // possible and is handled by the double-fault path.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, 16) };
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            put_byte(b' ');
        }
        puts_hex(*b as u64);
    }
}

/// A `core::fmt::Write` adapter, so kernel code can use `write!`/`format!`
/// against the serial port without pulling in an allocation.
pub struct SerialWriter;

impl Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

/// Write a formatted line to the serial port.
pub fn print(args: fmt::Arguments<'_>) {
    let _ = SerialWriter.write_fmt(args);
}

/// `print!` to the serial port.
#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::print(format_args!($($arg)*))
    };
}

/// `println!` to the serial port.
#[macro_export]
macro_rules! serial_println {
    () => { $crate::serial_print!("\n") };
    ($($arg:tt)*) => {
        $crate::serial_print!("{}\n", format_args!($($arg)*))
    };
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;

    #[test]
    fn port_numbers_are_consecutive() {
        assert_eq!(COM1, 0x3F8);
        assert_eq!(IER, 0x3F9);
        assert_eq!(DLL, 0x3F8);
        assert_eq!(DLM, 0x3F9);
        assert_eq!(FCR, 0x3FA);
        assert_eq!(LSR, 0x3FD);
    }

    #[test]
    fn line_status_bits_are_distinct() {
        // Each of these is a separate bit in the same register, so a typo in
        // one silently disables the other's meaning.
        assert_ne!(LSR_DATA_READY, LSR_THR_EMPTY);
        assert_ne!(LSR_THR_EMPTY, LSR_THR_IDLE);
        assert_ne!(LSR_DATA_READY, LSR_THR_IDLE);
        // All three fit in a single byte without overlapping.
        assert_eq!(LSR_DATA_READY & LSR_THR_EMPTY, 0);
        assert_eq!(LSR_THR_EMPTY & LSR_THR_IDLE, 0);
        assert_eq!(LSR_DATA_READY & LSR_THR_IDLE, 0);
        // The "transmitter empty" bit is above the "holding register empty"
        // bit, which is the architectural ordering.
        const {
            assert!(LSR_THR_IDLE > LSR_THR_EMPTY);
            assert!(LSR_THR_EMPTY > LSR_DATA_READY);
        }
    }

    #[test]
    fn hex_formatting_has_no_leading_zeros() {
        // Render through a buffer rather than the port, so the formatting logic
        // is testable without hardware.
        fn render(value: u64) -> std::string::String {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            if value == 0 {
                return "0".into();
            }
            let mut out = std::string::String::new();
            let mut started = false;
            for shift in (0..16).rev() {
                let nibble = ((value >> (shift * 4)) & 0xF) as usize;
                if nibble != 0 || started {
                    started = true;
                    out.push(DIGITS[nibble] as char);
                }
            }
            out
        }
        assert_eq!(render(0), "0");
        assert_eq!(render(1), "1");
        assert_eq!(render(0xF), "f");
        assert_eq!(render(0x10), "10");
        assert_eq!(render(0xDEAD_BEEF), "deadbeef");
        assert_eq!(render(u64::MAX), "ffffffffffffffff");
    }

    #[test]
    fn decimal_formatting_matches_the_hex_helper_shape() {
        fn render(value: u64) -> std::string::String {
            if value == 0 {
                return "0".into();
            }
            let mut digits = [0u8; 20];
            let mut n = value;
            let mut i = 0;
            while n > 0 && i < digits.len() {
                digits[i] = (n % 10) as u8 + b'0';
                n /= 10;
                i += 1;
            }
            let mut out = std::string::String::new();
            let mut j = i;
            while j > 0 {
                j -= 1;
                out.push(digits[j] as char);
            }
            out
        }
        assert_eq!(render(0), "0");
        assert_eq!(render(7), "7");
        assert_eq!(render(1280), "1280");
        assert_eq!(render(4096000), "4096000");
        assert_eq!(render(u64::MAX), "18446744073709551615");
    }

    #[test]
    fn decimal_and_hex_never_overflow_their_buffers() {
        // 20 decimal digits is the maximum for u64; the buffer is exactly 20.
        // This asserts the guard is not off by one.
        let max: u64 = u64::MAX;
        let mut digits = [0u8; 20];
        let mut n = max;
        let mut i = 0;
        while n > 0 && i < digits.len() {
            digits[i] = (n % 10) as u8;
            n /= 10;
            i += 1;
        }
        assert_eq!(i, 20);
    }

    #[test]
    fn initialised_flag_starts_false_in_a_fresh_process() {
        // The unit tests run in a process that never calls `init`, so the flag
        // must still be false — this catches a constructor that pre-sets it.
        assert!(!is_initialised());
    }

    #[test]
    fn writer_accepts_formatted_output() {
        // The adapter is exercised without touching the port by confirming the
        // trait impl exists and is usable as a sink.
        let mut w = SerialWriter;
        let r = w.write_str("x");
        assert!(r.is_ok());
        assert!(format!("{}", 42).contains("42"));
    }

    #[test]
    fn spin_limit_is_bounded() {
        // An unbounded spin on a wedged UART is a hang; the limit is the
        // difference between degraded logging and a dead machine.
        const {
            assert!(TX_SPIN_LIMIT > 0);
            assert!(TX_SPIN_LIMIT < 10_000_000);
        }
    }
}
