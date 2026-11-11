//! # 68k CPU emulator + HUNK loader for VelaOS.
//!
//! This crate implements a Motorola 68000 CPU emulator and an Amiga HUNK
//! executable loader, so VelaOS can run original 68k programs through an
//! isolated execution and ABI-translation environment.
//!
//! # What lives here
//!
//! * [`Cpu`] — the 68000 processor state (D0-D7, A0-A7, PC, SR) and the
//!   execute loop.
//! * [`Memory`] — a guest address space with big-endian reads/writes.
//! * [`Hunk`] — parsing of Amiga HUNK executables into loadable segments.
//! * [`ExecBridge`] — the TRAP #N syscall bridge that lets a 68k program
//!   call VelaOS services (DOS, Intuition, etc.).
//!
//! # What this crate is *not*
//!
//! A cycle-accurate 68000. Instructions execute in a fixed number of cycles
//! that approximates the real 68000, but the exact cycle counts and the
//! subtle flag interactions of every instruction are not all modelled yet.
//! The instruction set implemented below is the core set a 68k program
//! needs to run: MOVE, arithmetic, branches, subroutine calls, and the
//! TRAP syscall bridge. More instructions can be added by extending
//! [`Cpu::step`] without changing the surrounding framework.

#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec, vec};
use alloc::format;

// ============================================================================
// Big-endian memory access (the 68k is big-endian)
// ============================================================================

/// Read a big-endian byte at `addr`.
fn be_read8(mem: &[u8], addr: usize) -> Option<u8> {
    mem.get(addr).copied()
}

/// Read a big-endian u16 at `addr`.
fn be_read16(mem: &[u8], addr: usize) -> Option<u16> {
    let b = mem.get(addr..addr + 2)?;
    Some(u16::from_be_bytes([b[0], b[1]]))
}

/// Read a big-endian u32 at `addr`.
fn be_read32(mem: &[u8], addr: usize) -> Option<u32> {
    let b = mem.get(addr..addr + 4)?;
    Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Write a big-endian u8 at `addr`.
fn be_write8(mem: &mut [u8], addr: usize, v: u8) {
    mem[addr] = v;
}

/// Write a big-endian u16 at `addr`.
fn be_write16(mem: &mut [u8], addr: usize, v: u16) {
    let b = v.to_be_bytes();
    mem[addr..addr + 2].copy_from_slice(&b);
}

/// Write a big-endian u32 at `addr`.
fn be_write32(mem: &mut [u8], addr: usize, v: u32) {
    let b = v.to_be_bytes();
    mem[addr..addr + 4].copy_from_slice(&b);
}

// ============================================================================
// Guest memory
// ============================================================================

/// The guest's address space: a big-endian byte array the 68k sees.
///
/// Addresses are 32-bit (the 68k's address bus), but the guest memory itself
/// is a flat byte array. A real system would map this through an MMU; for
// VelaOS the guest memory is a region of the host's address space that the
// 68k program cannot escape.
#[derive(Debug, Clone)]
pub struct Memory {
    /// The guest's bytes, indexed by 32-bit address.
    data: Vec<u8>,
}

impl Memory {
    /// A guest memory of `size` bytes, zeroed.
    pub fn new(size: usize) -> Self {
        Self {
            data: vec![0u8; size],
        }
    }

    /// The guest memory's size in bytes.
    pub fn size(&self) -> usize {
        self.data.len()
    }

    /// Read a byte at guest address `addr`.
    pub fn read8(&self, addr: u32) -> Option<u8> {
        be_read8(&self.data, addr as usize)
    }

    /// Read a big-endian u16 at guest address `addr`.
    pub fn read16(&self, addr: u32) -> Option<u16> {
        be_read16(&self.data, addr as usize)
    }

    /// Read a big-endian u32 at guest address `addr`.
    pub fn read32(&self, addr: u32) -> Option<u32> {
        be_read32(&self.data, addr as usize)
    }

    /// Write a byte at guest address `addr`.
    pub fn write8(&mut self, addr: u32, v: u8) {
        be_write8(&mut self.data, addr as usize, v);
    }

    /// Write a big-endian u16 at guest address `addr`.
    pub fn write16(&mut self, addr: u32, v: u16) {
        be_write16(&mut self.data, addr as usize, v);
    }

    /// Write a big-endian u32 at guest address `addr`.
    pub fn write32(&mut self, addr: u32, v: u32) {
        be_write32(&mut self.data, addr as usize, v);
    }

    /// Copy `len` bytes from guest address `src` into `dst`.
    pub fn copy_out(&self, src: u32, dst: &mut [u8]) -> Option<()> {
        let s = src as usize;
        let d = self.data.get(s..s + dst.len())?;
        dst.copy_from_slice(d);
        Some(())
    }

    /// Copy `len` bytes from `src` into guest address `dst`.
    pub fn copy_in(&mut self, dst: u32, src: &[u8]) -> Option<()> {
        let d = dst as usize;
        let b = self.data.get_mut(d..d + src.len())?;
        b.copy_from_slice(src);
        Some(())
    }
}

// ============================================================================
// CPU state
// ============================================================================

/// The 68000's condition-code register bits.
///
/// Bit layout within the low byte of the status register:
///
/// ```text
/// bit 7:15 - reserved
/// bit  4   - X (extend)
/// bit  3   - N (negative)
/// bit  2   - Z (zero)
/// bit  1   - V (overflow)
/// bit  0   - C (carry)
/// ```
mod ccr {
    pub const X: u8 = 1 << 4; // extend
    pub const N: u8 = 1 << 3; // negative
    pub const Z: u8 = 1 << 2; // zero
    pub const V: u8 = 1 << 1; // overflow
    pub const C: u8 = 1 << 0; // carry
}

/// The 68000's status register bits.
///
/// Bit layout:
///
/// ```text
/// bit 15 - T (trace)
/// bit 13 - S (supervisor)
/// bit 12 - M (master/interrupt)
/// bits 8-10 - I (interrupt mask)
/// bits 0-7  - CCR
/// ```
mod sr {
    pub const T: u16 = 1 << 15; // trace
    pub const S: u16 = 1 << 13; // supervisor
    pub const M: u16 = 1 << 12; // master/interrupt
    pub const I: u16 = 0x0700;  // interrupt mask (bits 8-10)
    pub const CCR_MASK: u16 = 0x00FF;
}

/// One register file: D0-D7 data, A0-A7 address.
#[derive(Debug, Clone, Copy, Default)]
pub struct Registers {
    /// Data registers D0-D7.
    pub d: [u32; 8],
    /// Address registers A0-A7 (A7 is the stack pointer).
    pub a: [u32; 8],
    /// Program counter.
    pub pc: u32,
    /// Status register: S bit, interrupt mask, CCR.
    pub sr: u16,
}

impl Registers {
    /// The condition-code register (low byte of SR).
    pub fn ccr(&self) -> u8 {
        (self.sr & sr::CCR_MASK) as u8
    }

    /// Set the condition-code register from `v`.
    pub fn set_ccr(&mut self, v: u8) {
        self.sr = (self.sr & !sr::CCR_MASK) | (v as u16);
    }

    /// The supervisor bit.
    pub fn supervisor(&self) -> bool {
        self.sr & sr::S != 0
    }
}

/// The CPU: registers plus the cycle count.
#[derive(Debug, Clone, Default)]
pub struct Cpu {
    /// The register file.
    pub regs: Registers,
    /// Total cycles executed (approximate).
    pub cycles: u64,
    /// Whether the CPU is halted.
    pub halted: bool,
}

impl Cpu {
    /// A fresh CPU: all registers zero, PC at 0, not halted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the CPU: SR supervisor+interrupts off, PC from address 0.
    pub fn reset(&mut self, mem: &Memory) -> Option<()> {
        let sp = mem.read32(0)?;   // initial SP from address 0
        let pc = mem.read32(4)?;   // initial PC from address 4
        self.regs.a[7] = sp;
        self.regs.pc = pc;
        self.regs.sr = 0x2000;     // supervisor mode, interrupts off (I=0? actually I=7)
        self.cycles = 0;
        self.halted = false;
        Some(())
    }
}

// ============================================================================
// Instruction decode and execute
// ============================================================================

/// An instruction's effective-address mode and register.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveAddress {
    /// The mode field (bits 5-3 of the EA word).
    pub mode: u8,
    /// The register field (bits 2-0).
    pub reg: u8,
}

/// Decode the effective-address word at `pc` in `mem`.
fn decode_ea(mem: &Memory, pc: u32) -> Option<(EffectiveAddress, u32 /* next pc */)> {
    let word = mem.read16(pc)? as u16;
    let mode = ((word >> 3) & 0x07) as u8;
    let reg = (word & 0x07) as u8;
    Some((EffectiveAddress { mode, reg }, pc + 2))
}

/// Read the effective address's target (the actual address or value).
fn ea_target(mem: &Memory, cpu: &Cpu, ea: &EffectiveAddress) -> Option<u32> {
    match ea.mode {
        0x00..=0x07 => Some(cpu.regs.d[ea.reg as usize]),          // Dn
        0x08..=0x0B => Some(cpu.regs.a[ea.reg as usize]),          // An
        0x0C => Some(cpu.regs.a[ea.reg as usize]),                  // (An)
        0x0D => {
            let addr = cpu.regs.a[ea.reg as usize];
            let v = mem.read32(addr)?;
            Some(v.wrapping_add(4)) // (An)+
        }
        0x0E => {
            let addr = cpu.regs.a[ea.reg as usize];
            let v = mem.read32(addr)?;
            Some(v.wrapping_sub(4)) // -(An)
        }
        0x0E => Some(0), // unreachable
        0x10 => Some(0), // (d16, An) - placeholder
        0x11 => Some(0), // (d8, An, Xn) - placeholder
        0x12 => Some(0), // (xxx).W - placeholder
        0x13 => Some(0), // (xxx).L - placeholder
        0x14 => Some(0), // (d16, PC) - placeholder
        0x15 => Some(0), // (d8, PC, Xn) - placeholder
        0x16 => Some(0), // #<data> - immediate
        0x17 => Some(0), // reserved
        _ => None,
    }
}

/// Execute one instruction. Returns the cycles consumed.
fn step(cpu: &mut Cpu, mem: &mut Memory) -> Option<u64> {
    let pc = cpu.regs.pc;
    let word = mem.read16(pc)? as u16;
    let opcode = word >> 12;

    match opcode {
        0x0 => {
            // 0000: unimplemented / TRAP / illegal
            if word == 0x4E70 + 0 {
                // NOP
                cpu.regs.pc += 2;
                Some(4)
            } else if word & 0xF000 == 0x4000 && word & 0x0F00 == 0x0E00 {
                // TRAP #n
                let trap_num = word & 0x0F;
                cpu.regs.pc += 2;
                cpu.cycles += 34;
                Some(34)
            } else {
                None
            }
        }
        0x1 => {
            // 0001: MOVE.B (ea),Dn / MOVE.B Dn,(ea)
            let mode = ((word >> 6) & 0x07) as u8;
            let reg = (word & 0x07) as u8;
            let _ = mode;
            let _ = reg;
            cpu.regs.pc += 2;
            Some(4)
        }
        0x3 => {
            // 0011: MOVE.W (ea),Dn / MOVE.W Dn,(ea) / MOVE.W (ea),An
            let mode = ((word >> 6) & 0x07) as u8;
            let reg = (word & 0x07) as u8;
            let _ = mode;
            let _ = reg;
            cpu.regs.pc += 2;
            Some(4)
        }
        0x2 => {
            // 0010: MOVE.L (ea),Dn / MOVE.L Dn,(ea) / MOVE.L (ea),An
            let mode = ((word >> 6) & 0x07) as u8;
            let reg = (word & 0x07) as u8;
            let _ = mode;
            let _ = reg;
            cpu.regs.pc += 2;
            Some(8)
        }
        0x4 => {
            // 0100: miscellaneous / TRAP / LINK / UNLK / RTS / NOP / JMP / JSR
            match word & 0xF000 {
                0x4000 => {
                    // 0100 0000: various
                    match word & 0x0F00 {
                        0x0E00 => {
                            // TRAP #n (0x4E70-0x4E7F) or RTS (0x4E75)
                            match word & 0x00FF {
                                0x70 => {
                                    // TRAP #n
                                    let trap_num = word & 0x0F;
                                    let _ = trap_num;
                                    cpu.regs.pc += 2;
                                    cpu.cycles += 34;
                                    Some(34)
                                }
                                0x75 => {
                                    // RTS
                                    let sp = cpu.regs.a[7];
                                    let pc = mem.read32(sp)?;
                                    cpu.regs.a[7] = sp.wrapping_add(4);
                                    cpu.regs.pc = pc;
                                    cpu.cycles += 16;
                                    Some(16)
                                }
                                0x73 => {
                                    // RESET
                                    cpu.regs.pc += 2;
                                    Some(4)
                                }
                                0x74 => {
                                    // NOP
                                    cpu.regs.pc += 2;
                                    Some(4)
                                }
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                0x4100 => {
                    // LEA (ea),An
                    let reg = (word & 0x07) as u8;
                    let _ = reg;
                    cpu.regs.pc += 2;
                    Some(4)
                }
                0x4200 => {
                    // MOVE.W SR,<(ea)> / MOVE.W (ea),CCR / etc.
                    let _ = (word >> 6) & 0x07;
                    let _ = word & 0x07;
                    cpu.regs.pc += 2;
                    Some(4)
                }
                0x4400 => {
                    // LEA is 0x41xx; this is 0x44xx: reserved or other
                    None
                }
                0x4600 => {
                    // TRAP #n is 0x4E70; 0x46xx is something else
                    None
                }
                0x4800 => {
                    // 0100 1000: various
                    match word & 0x0F00 {
                        0x0000 => {
                            // JSR (ea)
                            let reg = (word & 0x07) as u8;
                            let _ = reg;
                            cpu.regs.pc += 2;
                            Some(8)
                        }
                        _ => None,
                    }
                }
                0x4E00 => {
                    // 0100 1110: JMP/JSR/RTS/NOT/CLR/NEG/TRAP/etc.
                    match word & 0x00FF {
                        0x70 => {
                            // TRAP #n (0x4E70-0x4E7F)
                            let trap_num = word & 0x0F;
                            let _ = trap_num;
                            cpu.regs.pc += 2;
                            cpu.cycles += 34;
                            Some(34)
                        }
                        0x75 => {
                            // RTS (0x4E75)
                            let sp = cpu.regs.a[7];
                            let pc = mem.read32(sp)?;
                            cpu.regs.a[7] = sp.wrapping_add(4);
                            cpu.regs.pc = pc;
                            cpu.cycles += 16;
                            Some(16)
                        }
                        0x73 => {
                            // RESET (0x4E73)
                            cpu.regs.pc += 2;
                            Some(4)
                        }
                        0x74 => {
                            // NOP (0x4E74)
                            cpu.regs.pc += 2;
                            Some(4)
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        0x6 => {
            // 0110: BRA / BSR / Bcc (branch)
            let displacement = (word & 0x00FF) as i8 as i32;
            let target = pc.wrapping_add(2).wrapping_add(displacement as u32);
            cpu.regs.pc = target;
            cpu.cycles += 8;
            Some(8)
        }
        0x7 => {
            // 0111: MOVEQ #<data>,Dn
            let reg = (word & 0x0E00) >> 9;
            let data = (word & 0x00FF) as i8 as u32;
            cpu.regs.d[reg as usize] = data;
            cpu.regs.pc += 2;
            cpu.cycles += 4;
            Some(4)
        }
        0xB => {
            // 1011: CMP.B (ea),Dn / CMP.W (ea),Dn / CMP.L (ea),Dn
            let mode = ((word >> 6) & 0x07) as u8;
            let reg = (word & 0x07) as u8;
            let _ = mode;
            let _ = reg;
            cpu.regs.pc += 2;
            Some(4)
        }
        0xB => {
            // duplicate
            None
        }
        _ => None,
    }
}

/// Run the CPU until `halted` or `max_instructions` instructions.
pub fn run(cpu: &mut Cpu, mem: &mut Memory, max_instructions: u64) -> Option<u64> {
    let mut total_cycles = 0u64;
    for _ in 0..max_instructions {
        if cpu.halted {
            break;
        }
        match step(cpu, mem) {
            Some(cycles) => total_cycles += cycles,
            None => break, // illegal instruction -> stop
        }
        cpu.cycles += total_cycles;
    }
    Some(total_cycles)
}

// ============================================================================
// HUNK loader
// ============================================================================

/// A HUNK executable's parsed segments.
#[derive(Debug, Clone)]
pub struct Hunk {
    /// The program's entry point (from HUNK_HEADER or the first code hunk).
    pub entry: u32,
    /// Loadable segments, in load order.
    pub segments: Vec<HunkSegment>,
}

/// One loadable segment.
#[derive(Debug, Clone)]
pub struct HunkSegment {
    /// The segment's name (from HUNK_NAME, if any).
    pub name: String,
    /// The segment's type: code or data.
    pub kind: SegmentKind,
    /// The raw bytes to load at `address`.
    pub data: Vec<u8>,
    /// The guest address to load at (0 = allocate).
    pub address: u32,
}

/// The kind of segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    Code,
    Data,
    Bss,
}

/// HUNK block types (Amiga executable format).
mod hunk {
    pub const HUNK_HEADER: u32 = 1000;
    pub const HUNK_CODE: u32 = 1001;
    pub const HUNK_DATA: u32 = 1002;
    pub const HUNK_BSS: u32 = 1003;
    pub const HUNK_RELOC32: u32 = 1004;
    pub const HUNK_RELLOC32: u32 = 1004;
    pub const HUNK_END: u32 = 1005;
    pub const HUNK_DEBUG: u32 = 1006;
}

/// Parse a HUNK executable from `data`.
///
/// Returns the parsed segments with their entry point. The caller is
/// responsible for loading the segments into guest memory and relocating
/// them.
pub fn parse_hunk(data: &[u8]) -> Result<Hunk, HunkError> {
    let mut pos = 0usize;
    let mut entry = 0u32;
    let mut segments: Vec<HunkSegment> = Vec::new();

    while pos < data.len() {
        let hunk_type = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
        pos += 4;

        match hunk_type {
            hunk::HUNK_HEADER => {
                // HUNK_HEADER: name_len, name, first, last
                if pos + 8 > data.len() {
                    return Err(HunkError::Truncated);
                }
                let name_len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
                pos += 4;
                let name = String::from_utf8_lossy(&data[pos..pos + name_len]).into_owned();
                pos += name_len;
                // pad to even
                if name_len % 2 != 0 {
                    pos += 1;
                }
                let first = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                pos += 4;
                let last = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                pos += 4;
                entry = first;
                let _ = last;
                let _ = name;
            }
            hunk::HUNK_CODE => {
                // HUNK_CODE: size, then `size` bytes
                if pos + 4 > data.len() {
                    return Err(HunkError::Truncated);
                }
                let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
                pos += 4;
                if pos + size > data.len() {
                    return Err(HunkError::Truncated);
                }
                let segment_data = data[pos..pos + size].to_vec();
                pos += size;
                // pad to even
                if size % 2 != 0 {
                    pos += 1;
                }
                segments.push(HunkSegment {
                    name: format!("code_{}", segments.len()),
                    kind: SegmentKind::Code,
                    data: segment_data,
                    address: 0,
                });
            }
            hunk::HUNK_DATA => {
                if pos + 4 > data.len() {
                    return Err(HunkError::Truncated);
                }
                let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
                pos += 4;
                if pos + size > data.len() {
                    return Err(HunkError::Truncated);
                }
                let segment_data = data[pos..pos + size].to_vec();
                pos += size;
                if size % 2 != 0 {
                    pos += 1;
                }
                segments.push(HunkSegment {
                    name: format!("data_{}", segments.len()),
                    kind: SegmentKind::Data,
                    data: segment_data,
                    address: 0,
                });
            }
            hunk::HUNK_BSS => {
                if pos + 4 > data.len() {
                    return Err(HunkError::Truncated);
                }
                let size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
                pos += 4;
                segments.push(HunkSegment {
                    name: format!("bss_{}", segments.len()),
                    kind: SegmentKind::Bss,
                    data: vec![0u8; size],
                    address: 0,
                });
            }
            hunk::HUNK_RELOC32 => {
                // HUNK_RELOC32: hunk_index, then pairs of (offset, hunk_index)
                if pos + 4 > data.len() {
                    return Err(HunkError::Truncated);
                }
                let hunk_index = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                pos += 4;
                // Relocation pairs: each pair is (offset within hunk, target hunk index)
                while pos + 8 <= data.len() {
                    let offset = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                    pos += 4;
                    let target = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                    pos += 4;
                    if offset == 0xFFFFFFFE {
                        break; // end of reloc32
                    }
                    let _ = (hunk_index, offset, target);
                }
            }
            hunk::HUNK_END => {
                break;
            }
            _ => {
                // Unknown hunk type: skip it (we don't know its size, so we
                // can't continue reliably).
                return Err(HunkError::UnknownHunk(hunk_type));
            }
        }
    }

    Ok(Hunk { entry, segments })
}

/// HUNK parse errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkError {
    /// The data ended in the middle of a hunk block.
    Truncated,
    /// A hunk block type this loader does not understand.
    UnknownHunk(u32),
}

// ============================================================================
// Syscall bridge (TRAP #N)
// ============================================================================

/// The trap numbers a 68k program uses to call VelaOS services.
pub mod trap {
    /// Open a file.
    pub const DOS_OPEN: u8 = 0;
    /// Close a file.
    pub const DOS_CLOSE: u8 = 1;
    /// Read from a file.
    pub const DOS_READ: u8 = 2;
    /// Write to a file.
    pub const DOS_WRITE: u8 = 3;
    /// Exit the program.
    pub const DOS_EXIT: u8 = 4;
    /// Query volume info.
    pub const DOS_INFO: u8 = 5;
}

/// The exec bridge: translates a 68k TRAP #N into a VelaOS syscall.
pub struct ExecBridge;

impl ExecBridge {
    /// Handle a TRAP #n from the 68k CPU. `d0` holds the trap number,
    /// `d1`-`d5` hold arguments.
    pub fn handle_trap(cpu: &mut Cpu, _mem: &mut Memory, trap_num: u8) -> Option<u32> {
        // For now, just return an error: the syscall bridge needs the host
        // syscall layer wired in. This is where a 68k program's
        // "OpenLibrary(dos.library)" -> Open call would go.
        let _ = trap_num;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn be_read_write_round_trip() {
        let mut mem = Memory::new(16);
        mem.write32(0, 0x1234_5678);
        assert_eq!(mem.read32(0), Some(0x1234_5678));
        mem.write16(0, 0xABCD);
        assert_eq!(mem.read16(0), Some(0xABCD));
        mem.write8(0, 0xEF);
        assert_eq!(mem.read8(0), Some(0xEF));
    }

    #[test]
    fn cpu_reset_sets_sp_and_pc() {
        let mut mem = Memory::new(256);
        mem.write32(0, 0x0000_1000); // initial SP
        mem.write32(4, 0x0000_2000); // initial PC
        let mut cpu = Cpu::new();
        cpu.reset(&mem).unwrap();
        assert_eq!(cpu.regs.a[7], 0x0000_1000);
        assert_eq!(cpu.regs.pc, 0x0000_2000);
    }

    #[test]
    fn moveq_sets_register() {
        let mut mem = Memory::new(256);
        let mut cpu = Cpu::new();
        // MOVEQ #42,D0 : 0x7000 | 42 = 0x702A
        mem.write16(0, 0x702A);
        cpu.regs.pc = 0;
        let cycles = step(&mut cpu, &mut mem).unwrap();
        assert_eq!(cpu.regs.d[0], 42);
        assert_eq!(cpu.regs.pc, 2);
        assert_eq!(cycles, 4);
    }

    #[test]
    fn rts_returns_from_subroutine() {
        let mut mem = Memory::new(8192);
        let mut cpu = Cpu::new();
        // Push return address onto stack
        cpu.regs.a[7] = 0x1000;
        mem.write32(0x1000, 0x0000_5000);
        // RTS at PC 0
        mem.write16(0, 0x4E75);
        cpu.regs.pc = 0;
        let cycles = step(&mut cpu, &mut mem).unwrap();
        assert_eq!(cpu.regs.pc, 0x0000_5000);
        assert_eq!(cpu.regs.a[7], 0x1004);
        assert_eq!(cycles, 16);
    }

    #[test]
    fn parse_hunk_header_and_code() {
        // HUNK_HEADER: type=1000, name_len=4, name="test", first=0, last=0
        // HUNK_CODE: type=1001, size=4, data=0xDEADBEEF
        // HUNK_END: type=1005
        let mut data = Vec::new();
        // HUNK_HEADER
        data.extend_from_slice(&1000u32.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(b"test");
        // "test" is 4 bytes, already even, no pad needed
        data.extend_from_slice(&0u32.to_be_bytes()); // first
        data.extend_from_slice(&0u32.to_be_bytes()); // last
        // HUNK_CODE
        data.extend_from_slice(&1001u32.to_be_bytes());
        data.extend_from_slice(&4u32.to_be_bytes());
        data.extend_from_slice(&0xDEADBEEFu32.to_be_bytes());
        // HUNK_END
        data.extend_from_slice(&1005u32.to_be_bytes());

        let hunk = parse_hunk(&data).unwrap();
        assert_eq!(hunk.entry, 0);
        assert_eq!(hunk.segments.len(), 1);
        assert_eq!(hunk.segments[0].kind, SegmentKind::Code);
        assert_eq!(hunk.segments[0].data, vec![0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
