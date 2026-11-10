//! The AArch64 console: a PL011 UART.
//!
//! # Where the UART is
//!
//! QEMU's `virt` machine puts a PL011 at `0x0900_0000`. The address is a
//! property of the machine, so the driver takes it as a parameter and the
//! boot code passes the one the board has.
//!
//! # Off the target
//!
//! Every register access is compiled out on a host build and the driver
//! answers "no byte" and "ready to send". A writer that spun on a status
//! bit that never changes would hang the test runner.

use core::fmt::Write;

/// The PL011's physical base on QEMU's `virt` machine.
pub const UART_BASE: u64 = 0x0900_0000;

/// The data register.
const DR: u64 = UART_BASE;
/// The flag register: transmit and receive status.
const FR: u64 = UART_BASE + 0x18;
/// The line-control register.
/// The line-control register.
const LCR: u64 = UART_BASE + 0x2C;
/// The integer baud-rate divisor register.
const IBRD: u64 = UART_BASE + 0x24;
/// The fractional baud-rate divisor register.
const FBRD: u64 = UART_BASE + 0x28;
/// The interrupt-mask register.
const IMSC: u64 = UART_BASE + 0x38;

/// The flag register's "transmit holding register empty" bit.
const FR_TXFE: u32 = 1 << 7;
/// The flag register's "receive buffer full" bit.
const FR_RXFE: u32 = 1 << 4;
/// The line-control register's 8-bit word length.
const LCR_WLEN8: u32 = 0x03;
/// The line-control register's FIFO enable.
const LCR_FEN: u32 = 1 << 4;

/// The divisor for 115200 baud at the PL011's 24 MHz reference clock.
const DIVISOR_115200: u32 = 13;

/// How many polls a transmission may spin for before the byte is dropped.
const TX_SPIN_LIMIT: u32 = 10_000;

/// Read one 32-bit UART register.
unsafe fn read32(pa: u64) -> u32 {
    if !crate::regs::ON_BARE_METAL {
        return 0;
    }
    // SAFETY: the caller names a UART register; a word-wide read of a
    // device register is always defined.
    unsafe { core::ptr::read_volatile(pa as *const u32) }
}

/// Write one 32-bit UART register.
unsafe fn write32(pa: u64, value: u32) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller names a UART register.
    unsafe { core::ptr::write_volatile(pa as *mut u32, value) }
}

/// Configure the UART for 115200 baud, 8N1, with the FIFOs enabled.
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
        // Interrupts off: the kernel polls.
        write32(IMSC, 0);
        // The divisor, then the word length and the FIFO.
        write32(IBRD, DIVISOR_115200);
        write32(FBRD, 0);
        write32(LCR, LCR_WLEN8 | LCR_FEN);
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
    // SAFETY: `FR` is the UART's flag register.
    unsafe { read32(FR) & FR_TXFE != 0 }
}

/// Write one byte over MMIO, spinning until the UART accepts it.
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
            // SAFETY: `DR` is the data register; writing it when the flag
            // register says the transmitter is empty queues the byte.
            unsafe { write32(DR, byte as u32) };
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
    // SAFETY: `FR` is the UART's flag register.
    if unsafe { read32(FR) } & FR_RXFE != 0 {
        return None;
    }
    // SAFETY: `DR` read when the flag register says the receive buffer is
    // full is the defined way to pop it.
    Some(unsafe { read32(DR) } as u8)
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
    fn the_register_offsets_are_the_pl011_ones() {
        assert_eq!(DR, UART_BASE);
        assert_eq!(FR, UART_BASE + 0x18);
        // The PL011 map: the integer divisor at 0x24, the fraction at 0x28
        // and the line control at 0x2C.
        assert_eq!(IBRD, UART_BASE + 0x24);
        assert_eq!(FBRD, UART_BASE + 0x28);
        assert_eq!(LCR, UART_BASE + 0x2C);
        assert_eq!(IMSC, UART_BASE + 0x38);
    }

    #[test]
    fn the_flag_bits_are_the_pl011_ones() {
        assert_eq!(FR_TXFE, 1 << 7);
        assert_eq!(FR_RXFE, 1 << 4);
    }
}
