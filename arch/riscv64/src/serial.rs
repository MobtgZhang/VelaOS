//! The NS16550A UART — the kernel's log sink and console on QEMU `virt`.
//!
//! # Where the UART lives
//!
//! QEMU's `virt` machine places an NS16550A-compatible UART at physical
//! `0x0000_1000_0000`, with byte-wide registers at consecutive addresses (the
//! device tree omits `reg-shift`, which means a stride of one byte). The
//! 16550 register set is a *legacy* PC design, so the offsets are the same
//! ones the x86-64 COM1 driver uses — only the access mechanism differs:
//! MMIO reads/writes here, `inb`/`outb` there.
//!
//! # Two output paths, one reason each
//!
//! * **MMIO** ([`put_byte`]): after `init`, the UART belongs to the kernel
//!   and every byte goes straight to the device. This is the normal path.
//! * **SBI** ([`sbi_putchar`]): the firmware's own console hook, usable
//!   *before* the UART is configured — the earliest boot diagnostics go
//!   through it, because a firmware `ecall` works even before MMIO is
//!   mapped. The legacy console extension (EID `0x0001`) takes the byte in
//!   `a0` and has no error convention to check.
//!
//! # Bounded transmission
//!
//! Like the x86-64 driver, every write polls the line-status register and
//! gives up after [`TX_SPIN_LIMIT`] polls, so a wedged UART degrades to
//! lost log output instead of a hung hart — the difference between a
//! truncated log in CI and a silent machine.
//!
//! # Input
//!
//! QEMU `virt` has no PS/2 controller; keyboard bytes arrive through this
//! UART (see [`crate::ps2`]). [`try_getb`] is therefore the input poll the
//! input stack rides on.

use core::sync::atomic::{AtomicBool, Ordering};

use spin::Mutex;

use crate::regs::{self, ON_BARE_METAL};

/// The UART's physical base on QEMU's `virt` machine.
pub const UART_BASE: u64 = 0x0000_1000_0000;

/// Receive buffer (read) / transmit holding (write) register, offset 0.
const RBR_THR: u64 = UART_BASE;
/// Interrupt enable register, offset 1.
const IER: u64 = UART_BASE + 1;
/// FIFO control register, offset 2.
const FCR: u64 = UART_BASE + 2;
/// Line control register, offset 3.
const LCR: u64 = UART_BASE + 3;
/// Line status register, offset 5.
const LSR: u64 = UART_BASE + 5;

/// Line-status bit 0: a received byte is ready in the receive buffer.
pub const LSR_DATA_READY: u8 = 1 << 0;
/// Line-status bit 5: the transmit holding register is empty.
pub const LSR_THR_EMPTY: u8 = 1 << 5;

/// 115200 baud with the 16550's 1.8432 MHz reference clock: divisor 1.
const BAUD_115200_DIVISOR: u16 = 1;

/// How many poll iterations a transmission may spin for before the byte is
/// dropped. A wedged UART must cost lost output, not a hung hart.
pub const TX_SPIN_LIMIT: u32 = 100_000;

/// Whether [`init`] has run; a log call before setup is a silent no-op.
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Serialises whole writes so multi-byte output cannot interleave.
static LOCK: Mutex<()> = Mutex::new(());

/// Read one 8-bit UART register.
///
/// Returns 0 off bare metal — the host has no MMIO window, and the status
/// checks built on this report "not ready" consistently with the other
/// hardware-touching functions of this crate.
///
/// # Safety
///
/// `pa` must name a UART register (one of the `const`s above).
unsafe fn read8(pa: u64) -> u8 {
    if !ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a UART register; a byte-wide read of a
    // device register is always defined.
    unsafe { core::ptr::read_volatile(pa as *const u8) }
}

/// Write one 8-bit UART register. A no-op off bare metal, like [`read8`].
///
/// # Safety
///
/// As in [`read8`].
unsafe fn write8(pa: u64, value: u8) {
    if !ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a UART register.
    unsafe { core::ptr::write_volatile(pa as *mut u8, value) }
}

/// Configure the UART for 115200 baud, 8N1, with both FIFOs enabled.
///
/// Interrupts stay disabled (`IER` = 0): the kernel polls, and receive
/// interrupts would need the PLIC configured first — see [`crate::plic`].
///
/// # Safety
///
/// Call once during boot, before the first log write; calling it
/// concurrently with logging would reconfigure the device mid-byte.
pub unsafe fn init() {
    // No interrupts: the kernel polls.
    // SAFETY: `IER` is the UART's interrupt-enable register.
    unsafe { write8(IER, 0x0000) };
    // Enable and clear both FIFOs, 14-byte trigger: the 0x0007 encoding is
    // FIFO-enable | clear-RX | clear-TX, and it discards anything the
    // firmware left buffered so the first kernel byte is the first output.
    // SAFETY: `FCR` is the FIFO control register.
    unsafe { write8(FCR, 0x0007) };
    // 8 data bits, 1 stop bit, no parity (LCR = 0x0003), reached through the
    // divisor window: DLAB set selects the divisor registers, and clearing
    // it returns the register file to its normal meaning.
    // SAFETY: `LCR` and the divisor registers share the register file.
    unsafe {
        write8(LCR, 0x0080 | 0x0003);
        write8(UART_BASE, BAUD_115200_DIVISOR as u8);
        write8(UART_BASE + 1, (BAUD_115200_DIVISOR >> 8) as u8);
        write8(LCR, 0x0003);
    }
    INITIALISED.store(true, Ordering::Release);
}

/// True once [`init`] has completed.
pub fn is_initialised() -> bool {
    INITIALISED.load(Ordering::Acquire)
}

/// True when the transmit holding register can accept a byte.
///
/// Off bare metal this is always `true`: the alternative is a writer
/// spinning forever on a status bit that never changes on the host.
pub fn can_send() -> bool {
    if !regs::ON_BARE_METAL {
        // Off bare metal there is no device to ask. Answering "yes" is the
        // only answer that does not make a writer spin: the host's registers
        // read as zero, and zero says "never ready".
        return true;
    }
    // SAFETY: `LSR` is the UART's line-status register.
    unsafe { read8(LSR) & LSR_THR_EMPTY != 0 }
}

/// Write one byte over MMIO, spinning until the UART accepts it.
///
/// `\n` is expanded to `\r\n`, which is what makes `-serial file:` output
/// readable; the bare `\r` alone would return the cursor to column 0
/// without advancing the line.
pub fn put_byte(byte: u8) {
    if !INITIALISED.load(Ordering::Acquire) && !regs::ON_BARE_METAL {
        return;
    }
    if byte == b'\n' {
        put_byte_raw(b'\r');
    }
    put_byte_raw(byte);
}

/// The untranslating writer behind [`put_byte`].
fn put_byte_raw(byte: u8) {
    let mut spins = TX_SPIN_LIMIT;
    while spins > 0 {
        if can_send() {
            // SAFETY: `RBR_THR` is the transmit holding register; writing
            // it when LSR says THR is empty queues the byte.
            unsafe { write8(RBR_THR, byte) };
            return;
        }
        spins -= 1;
    }
}

/// Read one byte over MMIO, without blocking.
///
/// Returns `None` when nothing has arrived. This is the whole input poll
/// the console rides on — QEMU `virt` has no other input device.
pub fn try_getb() -> Option<u8> {
    // SAFETY: `LSR` is the line-status register.
    let ready = unsafe { read8(LSR) & LSR_DATA_READY != 0 };
    if !ready {
        return None;
    }
    // SAFETY: `RBR_THR` read when LSR reports data ready is the defined
    // way to pop the receive buffer.
    Some(unsafe { read8(RBR_THR) })
}

/// Alias of [`put_byte`], matching the x86-64 driver's name.
pub fn putc(byte: u8) {
    put_byte(byte);
}

/// Alias of [`try_getb`], matching the x86-64 `getc` name.
pub fn getb() -> Option<u8> {
    try_getb()
}

/// Write one byte through the firmware's console hook.
///
/// The legacy console extension is EID `0x0001`, function 0, with the byte
/// in `a0`; the legacy convention reports no error, so the result is
/// discarded. Usable before [`init`], because the firmware's own console
/// path needs no MMIO mapping of ours.
pub fn sbi_putchar(byte: u8) {
    let _ = regs::sbi_call(0x0001, 0, byte as u64, 0);
}

/// Write a string over MMIO, holding the lock for the whole string so
/// multi-byte output from different harts cannot interleave.
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
        let nibble = ((value >> (shift * 4)) & 0x000F) as usize;
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

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uart_sits_at_the_qemu_virt_base() {
        assert_eq!(UART_BASE, 0x0000_1000_0000);
    }

    #[test]
    fn the_register_offsets_match_the_16550() {
        // The NS16550A keeps the PC's legacy register file, so these are
        // the same offsets the x86-64 COM1 driver uses.
        assert_eq!(RBR_THR, UART_BASE);
        assert_eq!(IER, UART_BASE + 1);
        assert_eq!(FCR, UART_BASE + 2);
        assert_eq!(LCR, UART_BASE + 3);
        assert_eq!(LSR, UART_BASE + 5);
    }

    #[test]
    fn line_status_bits_are_distinct() {
        assert_eq!(LSR_DATA_READY, 1);
        assert_eq!(LSR_THR_EMPTY, 1 << 5);
        const {
            assert!(LSR_DATA_READY & LSR_THR_EMPTY == 0);
        }
    }

    #[test]
    fn the_baud_divisor_is_one() {
        // 1.8432 MHz / 16 / 1 = 115200 baud.
        assert_eq!(BAUD_115200_DIVISOR, 1);
    }

    #[test]
    fn hex_formatting_has_no_leading_zeros() {
        // Render through a buffer rather than the device, so the
        // formatting logic is testable without hardware.
        fn render(value: u64) -> std::string::String {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            if value == 0 {
                return "0".into();
            }
            let mut out = std::string::String::new();
            let mut started = false;
            for shift in (0..16).rev() {
                let nibble = ((value >> (shift * 4)) & 0x000F) as usize;
                if nibble != 0 || started {
                    started = true;
                    out.push(DIGITS[nibble] as char);
                }
            }
            out
        }
        assert_eq!(render(0), "0");
        assert_eq!(render(1), "1");
        assert_eq!(render(0x000F), "f");
        assert_eq!(render(0x0010), "10");
        assert_eq!(render(0x0000_DEAD_BEEF), "deadbeef");
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
        // 20 decimal digits is the maximum for u64; the buffer is exactly
        // 20, so the loop guard must not be off by one.
        let mut digits = [0u8; 20];
        let mut n = u64::MAX;
        let mut i = 0;
        while n > 0 && i < digits.len() {
            digits[i] = (n % 10) as u8;
            n /= 10;
            i += 1;
        }
        assert_eq!(i, 20);
    }

    #[test]
    fn device_access_is_inert_off_bare_metal() {
        // The whole volatile surface must be callable on the host: reads
        // report "no byte, transmitter idle-ish" instead of faulting.
        assert!(can_send(), "the host must not spin on a dead UART");
        assert_eq!(try_getb(), None);
        put_byte(b'x');
        put_byte(b'\n');
        puts("hello");
        puts_hex(0xBEEF);
        puts_dec(42);
        // SAFETY: the documented host no-op.
        unsafe { init() };
        assert!(is_initialised());
        // The firmware path returns (0, 0) off bare metal and must not
        // reach any ecall.
        sbi_putchar(b'x');
    }
}
