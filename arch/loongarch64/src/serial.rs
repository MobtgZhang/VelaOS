//! The LoongArch64 console: an NS16550A UART.
//!
//! # Where the UART is
//!
//! QEMU's `loongarch-virt` machine puts a 16550-compatible UART in the
//! memory-mapped I/O space. The base is a *property* of the machine, so
//! the driver takes it as a parameter and the boot code passes the one the
//! board has.
//!
//! # Off the target
//!
//! Every register access is compiled out on a host build and the driver
//! answers "no byte" and "ready to send". A writer that spun on a status
//! bit that never changes would hang the test runner.

use core::fmt::Write;

/// The UART's physical base on QEMU's `loongarch-virt` machine.
///
/// The machine documentation places the ISA-compatible I/O devices in the
/// `0x1FE0_0000` region; the UART is the first of them.
pub const UART_BASE: u64 = 0x1FE0_0000;

/// Receive buffer (read) / transmit holding (write) register, offset 0.
const RBR_THR: u64 = UART_BASE;
/// The interrupt-enable register, offset 1.
const IER: u64 = UART_BASE + 1;
/// The FIFO-control register, offset 2.
const FCR: u64 = UART_BASE + 2;
/// The line-control register, offset 3.
const LCR: u64 = UART_BASE + 3;
/// The modem-control register, offset 4.
const MCR: u64 = UART_BASE + 4;
/// The line-status register, offset 5.
const LSR: u64 = UART_BASE + 5;
/// The scratch register, offset 7.
const SCR: u64 = UART_BASE + 7;

/// The divisor latch access bit in the line-control register.
const LCR_DLAB: u8 = 1 << 7;
/// 8 data bits, no parity, one stop bit.
const LCR_8N1: u8 = 0x03;
/// The transmitter-empty bit in the line-status register.
const LSR_THR_EMPTY: u8 = 1 << 5;
/// The data-ready bit in the line-status register.
const LSR_DATA_READY: u8 = 1;

/// The divisor for 115200 baud at the conventional 1.8432 MHz clock.
///
/// The QEMU machine does not check the timing, but a real board would, so
/// the divisor is computed rather than left at zero.
const DIVISOR_115200: u16 = 1;

/// How many polls a transmission may spin for before the byte is dropped.
///
/// A UART whose status never changes must not hang the writer: the byte is
/// dropped after this many polls and the caller carries on.
const TX_SPIN_LIMIT: u32 = 10_000;

/// Read one 8-bit UART register.
unsafe fn read8(pa: u64) -> u8 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a UART register; a byte-wide read of a
    // device register is always defined.
    unsafe { core::ptr::read_volatile(pa as *const u8) }
}

/// Write one 8-bit UART register.
unsafe fn write8(pa: u64, value: u8) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a UART register.
    unsafe { core::ptr::write_volatile(pa as *mut u8, value) }
}

/// Configure the UART for 115200 baud, 8N1, with both FIFOs enabled.
///
/// # Safety
///
/// Call once during boot, before the first log write.
pub unsafe fn init() {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees this runs once, before any logging.
    unsafe {
        // Disable interrupts: the kernel polls.
        write8(IER, 0);
        // Enable and clear both FIFOs.
        write8(FCR, 0x07);
        // The divisor latch, then the divisor, then 8N1 with the latch
        // closed.
        write8(LCR, LCR_DLAB | LCR_8N1);
        write8(RBR_THR, (DIVISOR_115200 & 0xFF) as u8);
        write8(IER, (DIVISOR_115200 >> 8) as u8);
        write8(LCR, LCR_8N1);
        // DTR and RTS: the line is up.
        write8(MCR, 0x03);
        // The scratch register is a spare byte on the 16550; writing it is
        // the classic "is this really a UART" probe.
        write8(SCR, 0x55);
    }
}

/// True when the transmitter can accept a byte.
pub fn can_send() -> bool {
    if !crate::regs::ON_BARE_METAL {
        // Off bare metal there is no device to ask. Answering "yes" is the
        // only answer that does not make a writer spin: the host's
        // registers read as zero, and zero says "never ready".
        return true;
    }
    // SAFETY: `LSR` is the UART's line-status register.
    unsafe { read8(LSR) & LSR_THR_EMPTY != 0 }
}

/// Write one byte over MMIO, spinning until the UART accepts it.
///
/// `\n` is expanded to `\r\n`, which is what makes `-serial file:` output
/// readable.
pub fn put_byte(byte: u8) {
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
            // it when the line status says the transmitter is empty queues
            // the byte.
            unsafe { write8(RBR_THR, byte) };
            return;
        }
        spins -= 1;
    }
}

/// Read one byte over MMIO, without blocking.
///
/// Returns `None` when nothing has arrived.
pub fn try_get_byte() -> Option<u8> {
    if !crate::regs::ON_BARE_METAL {
        return None;
    }
    // SAFETY: `LSR` is the UART's line-status register.
    let ready = unsafe { read8(LSR) & LSR_DATA_READY != 0 };
    if !ready {
        return None;
    }
    // SAFETY: `RBR_THR` read when the line status reports data ready is
    // the defined way to pop the receive buffer.
    Some(unsafe { read8(RBR_THR) })
}

/// Alias of [`put_byte`], matching the x86-64 driver's name.
pub fn putc(byte: u8) {
    put_byte(byte);
}

/// Alias of [`try_get_byte`], matching the x86-64 driver's name.
pub fn getc() -> Option<u8> {
    try_get_byte()
}

/// Write a string.
pub fn puts(text: &str) {
    for byte in text.bytes() {
        put_byte(byte);
    }
}

/// Write a number in hexadecimal.
pub fn puts_hex(value: u64) {
    let mut buffer = [0u8; 16];
    let mut index = 16;
    let mut remaining = value;
    while index > 0 {
        index -= 1;
        buffer[index] = b"0123456789abcdef"[(remaining & 0xF) as usize];
        remaining >>= 4;
    }
    for byte in buffer {
        put_byte(byte);
    }
}

/// Write a number in decimal.
pub fn puts_dec(value: u64) {
    let mut buffer = [0u8; 20];
    let mut index = 20;
    let mut remaining = value;
    loop {
        index -= 1;
        buffer[index] = b'0' + (remaining % 10) as u8;
        remaining /= 10;
        if remaining == 0 {
            break;
        }
    }
    for byte in &buffer[index..] {
        put_byte(*byte);
    }
}

/// The console as a [`Write`] sink, so the kernel's logging can use it.
pub struct Console;

impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        puts(text);
        Ok(())
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_device_access_is_inert_off_bare_metal() {
        // The whole volatile surface must be callable on the host: reads
        // report "no byte" and the transmitter reports itself ready.
        assert!(can_send(), "the host must not spin on a dead UART");
        assert_eq!(try_get_byte(), None);
        put_byte(b'x');
        put_byte(b'\n');
        puts("hello\n");
        puts_hex(0xBEEF);
        puts_dec(1234);
    }

    #[test]
    fn the_register_offsets_are_the_16550_ones() {
        assert_eq!(RBR_THR, UART_BASE);
        assert_eq!(IER, UART_BASE + 1);
        assert_eq!(FCR, UART_BASE + 2);
        assert_eq!(LCR, UART_BASE + 3);
        assert_eq!(MCR, UART_BASE + 4);
        assert_eq!(LSR, UART_BASE + 5);
    }

    #[test]
    fn the_line_status_bits_are_the_16550_ones() {
        // Transmitter empty is bit 5 and data ready is bit 0.
        assert_eq!(LSR_THR_EMPTY, 1 << 5);
        assert_eq!(LSR_DATA_READY, 1);
    }
}
