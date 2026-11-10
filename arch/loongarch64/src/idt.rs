//! Exception decoding, vector-slot arithmetic, and the trap-entry CSR table.
//!
//! # One entry point, arithmetic addressing
//!
//! LoongArch has no interrupt descriptor table and no descriptor loads: all
//! exceptions and interrupts enter at `CSR.EENTRY` plus an offset computed
//! from the *exception code* (`ESTAT.EC`):
//!
//! ```text
//! exceptions:  entry = EENTRY + EC × VECSIZE
//! interrupts:  entry = EENTRY + (64 + IS) × VECSIZE,   IS = ESTAT.IS line
//! VECSIZE      = (1 << ECFG.VS) × 4 bytes
//! ```
//!
//! Linux programs `ECFG.VS = 4` (VECSIZE = 64 bytes per slot) and installs
//! its handlers exactly at those slots — `handle_vint` at slots 64..=78 for
//! the fifteen interrupt lines, the generic exception handler at slot `EC`,
//! and its separate debugger entry at `EENTRY + 0x480`. VelaOS follows the
//! same layout so boot code and handler stubs stay interchangeable with
//! Linux-derived examples.
//!
//! # `EC` — the architectural exception codes
//!
//! (LoongArch Reference Manual Vol. 2, Chapter 3; identical numbers to
//! Linux's `EXCCODE_*`.)
//!
//! | EC | Mnemonic  | Meaning                                          |
//! |----|-----------|--------------------------------------------------|
//! | 0  | INT       | Interrupt (dispatched by `ESTAT.IS` instead)      |
//! | 1  | PIL       | TLB miss on load (software refill)                |
//! | 2  | PIS       | TLB miss on store (software refill)               |
//! | 3  | PIF       | TLB miss on fetch (software refill)               |
//! | 4  | PME       | TLB modified — store to a page with `D` clear      |
//! | 5  | PNR       | TLB read-inhibited (EntryLo `NR`)                 |
//! | 6  | PNX       | TLB execute-inhibited (EntryLo `NX`)              |
//! | 7  | PPI       | TLB privilege error (PLV too high for the entry)  |
//! | 8  | ADE       | Address error; `ESUBCODE` 0 = ADEF (fetch), 1 = ADEM (data) |
//! | 9  | ALE       | Unaligned access (atomics require alignment)      |
//! | 10 | BCE       | Bounds-check error (`ld.bf`/`st.bf`)              |
//! | 11 | SYS       | `syscall` — the user-kernel gate                  |
//! | 12 | BP        | `break`/debug breakpoint                          |
//! | 13 | INE       | Instruction not exists                            |
//! | 14 | IPE       | Instruction privileged error (PLV ≠ 0)            |
//! | 15 | FPDIS     | FPU disabled (`CSR.EUEN.FPE` clear)               |
//! | 16 | SXDIS     | LSX disabled                                      |
//! | 17 | LASXDIS   | LASX disabled                                     |
//! | 18 | FPE       | Floating-point exception                          |
//! | 19 | WATCH     | Watchpoint; subcode 0 = fetch, 1 = memory         |
//! | 20 | BTDIS     | Binary-translation disabled                       |
//! | 21 | BTE       | Binary-translation exception                      |
//! | 22 | GSPR      | Guest privileged instruction error                |
//! | 23 | HVC       | Hypercall                                         |
//! | 24 | GCM       | Guest CSR modified                                |
//! | 25 | SE        | Security exception                                |
//!
//! # CSRs involved in a trap
//!
//! | CSR | #     | Role at trap entry / `ertn`                          |
//! |-----|-------|------------------------------------------------------|
//! | `CRMD`   | 0x00  | `PLV`/`IE` of the *interrupted* context; hardware sets `PLV=0`, `IE=0` on entry |
//! | `PRMD`   | 0x01  | Saved `PPLV` (interrupted PLV) + `PIE` (interrupted IE); `ertn` restores both |
//! | `EUEN`   | 0x02  | FPU/LSX/LASX enables (FPDIS etc. fire when clear)     |
//! | `ECFG`   | 0x04  | `LIE` interrupt mask, `VS` vector spacing             |
//! | `ESTAT`  | 0x05  | `IS` interrupt lines, `EC`, `ESUBCODE`                |
//! | `ERA`    | 0x06  | Restart PC (exception return address)                 |
//! | `BADV`   | 0x07  | Faulting virtual address (TLB/ADR exceptions)         |
//! | `BADI`   | 0x08  | Bad instruction word (ADEF/BCE)                       |
//! | `EENTRY` | 0x0C  | Trap entry base                                       |
//!
//! Returning is a single `ertn`: it copies `PRMD.PPLV` → `CRMD.PLV`,
//! `PRMD.PIE` → `CRMD.IE` and jumps to `ERA`. See [`crate::userspace`] for
//! how VelaOS builds `PRMD` before entering ring 3.

/// The architectural exception codes (`ESTAT.EC`, bits 21:16).
pub mod ec {
    /// Interrupt — the real cause is in `ESTAT.IS`.
    pub const INT: u8 = 0;
    /// TLB miss on load.
    pub const PIL: u8 = 1;
    /// TLB miss on store.
    pub const PIS: u8 = 2;
    /// TLB miss on instruction fetch.
    pub const PIF: u8 = 3;
    /// TLB modified — store to a `D`-clear page.
    pub const PME: u8 = 4;
    /// TLB read-inhibited.
    pub const PNR: u8 = 5;
    /// TLB execute-inhibited.
    pub const PNX: u8 = 6;
    /// TLB privilege error.
    pub const PPI: u8 = 7;
    /// Address error (fetch or data, by `ESUBCODE`).
    pub const ADE: u8 = 8;
    /// Unaligned access.
    pub const ALE: u8 = 9;
    /// Bounds-check error.
    pub const BCE: u8 = 10;
    /// System call — the user-kernel gate.
    pub const SYS: u8 = 11;
    /// Breakpoint / `break` instruction.
    pub const BP: u8 = 12;
    /// Instruction not exists.
    pub const INE: u8 = 13;
    /// Instruction privileged error.
    pub const IPE: u8 = 14;
    /// FPU disabled.
    pub const FPDIS: u8 = 15;
    /// LSX disabled.
    pub const SXDIS: u8 = 16;
    /// LASX disabled.
    pub const LASXDIS: u8 = 17;
    /// Floating-point exception.
    pub const FPE: u8 = 18;
    /// Watchpoint (fetch or memory, by `ESUBCODE`).
    pub const WATCH: u8 = 19;
    /// Binary translation disabled.
    pub const BTDIS: u8 = 20;
    /// Binary translation exception.
    pub const BTE: u8 = 21;
    /// Guest privileged instruction error.
    pub const GSPR: u8 = 22;
    /// Hypercall.
    pub const HVC: u8 = 23;
    /// Guest CSR modified.
    pub const GCM: u8 = 24;
    /// Security exception.
    pub const SE: u8 = 25;
}

/// The architectural interrupt lines (`ESTAT.IS`, bits 14:0; the same bit
/// numbers index `ECFG.LIE` and the vector slots `64 + line`).
pub mod line {
    /// Software interrupt 0 (raised by `CSR.MSGIS`-style SWI mechanisms).
    pub const SWI0: u16 = 0;
    /// Software interrupt 1.
    pub const SWI1: u16 = 1;
    /// The first of the eight ExtIOI-delivered hardware lines (`HWI0`–
    /// `HWI7`, lines 2–9); individual devices pick their line through the
    /// ExtIOI routing registers (see [`crate::apic`]).
    pub const HWI0: u16 = 2;
    /// Performance-counter overflow.
    pub const PCOV: u16 = 10;
    /// The architectural timer (see [`crate::timer`]).
    pub const TIMER: u16 = 11;
    /// Inter-processor interrupt (see [`crate::apic`]).
    pub const IPI: u16 = 12;
    /// Non-maskable interrupt.
    pub const NMI: u16 = 13;
    /// Advanced vector extension interrupt.
    pub const AVEC: u16 = 14;
}

/// What the hardware reported in `ESTAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapKind {
    /// An interrupt line asserted in `ESTAT.IS`; carries the line number.
    Interrupt(u16),
    /// TLB miss on load (`PIL`).
    TlbLoadMiss,
    /// TLB miss on store (`PIS`).
    TlbStoreMiss,
    /// TLB miss on instruction fetch (`PIF`).
    TlbFetchMiss,
    /// Store to a `D`-clear page (`PME`).
    TlbModified,
    /// Load/fetch through an `NR` page (`PNR`).
    TlbNoRead,
    /// Fetch through an `NX` page (`PNX`).
    TlbNoExec,
    /// PLV violation against an EntryLo `PLV` (`PPI`).
    TlbPrivilege,
    /// Address error: fetch (`fetch = true`) or data access.
    AddressError {
        /// True when the faulting access was an instruction fetch.
        fetch: bool,
    },
    /// Unaligned access (`ALE`).
    Unaligned,
    /// Bounds-check error (`BCE`).
    BoundsCheck,
    /// `syscall` (`SYS`) — the user-kernel gate.
    Syscall,
    /// `break` / debug breakpoint (`BP`).
    Breakpoint,
    /// Instruction not exists (`INE`).
    InstructionNotExist,
    /// Instruction privileged error (`IPE`).
    InstructionPrivileged,
    /// FPU/LSX/LASX disabled (`FPDIS`/`SXDIS`/`LASXDIS`); carries which.
    ExtensionDisabled(u8),
    /// Floating-point exception (`FPE`).
    FpException,
    /// Watchpoint (`WATCH`): fetch (`fetch = true`) or memory.
    Watchpoint {
        /// True when the watchpoint was an instruction fetch.
        fetch: bool,
    },
    /// Binary translation disabled/exception (`BTDIS`/`BTE`).
    BinaryTranslation(u8),
    /// Guest errors, hypercall, guest CSR modified, security (`GSPR`, `HVC`,
    /// `GCM`, `SE`); carries the raw EC.
    Guest(u8),
    /// Anything in the reserved range.
    Reserved(u8),
}

/// Decode a raw `EC` value (already extracted from `ESTAT`).
pub fn decode_ec(ec: u8) -> TrapKind {
    match ec {
        ec::PIL => TrapKind::TlbLoadMiss,
        ec::PIS => TrapKind::TlbStoreMiss,
        ec::PIF => TrapKind::TlbFetchMiss,
        ec::PME => TrapKind::TlbModified,
        ec::PNR => TrapKind::TlbNoRead,
        ec::PNX => TrapKind::TlbNoExec,
        ec::PPI => TrapKind::TlbPrivilege,
        ec::ADE => TrapKind::AddressError { fetch: false },
        ec::ALE => TrapKind::Unaligned,
        ec::BCE => TrapKind::BoundsCheck,
        ec::SYS => TrapKind::Syscall,
        ec::BP => TrapKind::Breakpoint,
        ec::INE => TrapKind::InstructionNotExist,
        ec::IPE => TrapKind::InstructionPrivileged,
        ec::FPDIS => TrapKind::ExtensionDisabled(ec::FPDIS),
        ec::SXDIS => TrapKind::ExtensionDisabled(ec::SXDIS),
        ec::LASXDIS => TrapKind::ExtensionDisabled(ec::LASXDIS),
        ec::FPE => TrapKind::FpException,
        ec::WATCH => TrapKind::Watchpoint { fetch: false },
        ec::BTDIS => TrapKind::BinaryTranslation(ec::BTDIS),
        ec::BTE => TrapKind::BinaryTranslation(ec::BTE),
        ec::GSPR | ec::HVC | ec::GCM | ec::SE => TrapKind::Guest(ec),
        other => TrapKind::Reserved(other),
    }
}

/// The `EC` field of a raw `ESTAT` word (bits 21:16).
pub fn estat_ec(estat: u64) -> u8 {
    ((estat >> 16) & 0b11_1111) as u8
}

/// The `ESUBCODE` field of a raw `ESTAT` word (bits 31:22) — refines `ADE`
/// (0 = fetch / ADEF, 1 = data / ADEM), `WATCH`, and `FPE`.
pub fn estat_subcode(estat: u64) -> u8 {
    (estat >> 22) as u8
}

/// The asserted interrupt lines of a raw `ESTAT` word (`IS`, bits 14:0).
pub fn estat_is(estat: u64) -> u16 {
    (estat & 0x7FFF) as u16
}

/// Is this exception one of the three software-refill TLB misses? These are
/// the traps a TLB refill handler exists for; everything else is fatal or
/// scheduler business.
pub fn is_page_exception(ec: u8) -> bool {
    matches!(ec, ec::PIL | ec::PIS | ec::PIF)
}

// ---------------------------------------------------------------------------
// Vector-slot arithmetic.
// ---------------------------------------------------------------------------

/// Linux's `ECFG.VS` setting: 2^4 × 4 = 64 bytes per vector slot.
pub const VS_LINUX: u64 = 4;

/// The vector slot size in bytes for a given `ECFG.VS` exponent.
pub const fn vec_size(vs: u64) -> u64 {
    (1 << vs) * 4
}

/// The byte offset from `EENTRY` where exception `ec` enters.
pub const fn exception_entry_offset(ec: u8, vs: u64) -> u64 {
    (ec as u64) * vec_size(vs)
}

/// The byte offset from `EENTRY` where interrupt line `is` enters — slots
/// 64..=78, above the 64 exception slots the architecture reserves.
///
/// The slot index itself (`64 + is`) is what Linux installs `handle_vint`
/// handlers at.
pub const fn interrupt_entry_offset(is: u16, vs: u64) -> u64 {
    (64 + is as u64) * vec_size(vs)
}

/// The slot index for an interrupt line (as opposed to its byte offset).
pub const fn interrupt_slot(is: u16) -> u64 {
    64 + is as u64
}

/// The `ECFG.VS` field's position (bits 18:16) as a shift — kept next to the
/// slot arithmetic because the two decide the whole entry layout together.
pub const VS_SHIFT: u32 = 16;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exception_codes_match_linux() {
        // These numbers are load-bearing: the syscall gate and the TLB
        // refill handler both dispatch on them.
        assert_eq!(ec::INT, 0);
        assert_eq!(ec::PIL, 1);
        assert_eq!(ec::PIS, 2);
        assert_eq!(ec::PIF, 3);
        assert_eq!(ec::PME, 4);
        assert_eq!(ec::PNR, 5);
        assert_eq!(ec::PNX, 6);
        assert_eq!(ec::PPI, 7);
        assert_eq!(ec::ADE, 8);
        assert_eq!(ec::ALE, 9);
        assert_eq!(ec::BCE, 10);
        assert_eq!(ec::SYS, 11);
        assert_eq!(ec::BP, 12);
        assert_eq!(ec::INE, 13);
        assert_eq!(ec::IPE, 14);
        assert_eq!(ec::FPDIS, 15);
        assert_eq!(ec::FPE, 18);
        assert_eq!(ec::WATCH, 19);
        assert_eq!(ec::SE, 25);
    }

    #[test]
    fn interrupt_lines_match_linux() {
        // Linux `INT_SWI0`..`INT_AVEC`; the same numbers index `ECFG.LIE`
        // and the interrupt vector slots.
        assert_eq!(line::SWI0, 0);
        assert_eq!(line::SWI1, 1);
        assert_eq!(line::HWI0, 2);
        assert_eq!(line::PCOV, 10);
        assert_eq!(line::TIMER, 11);
        assert_eq!(line::IPI, 12);
        assert_eq!(line::NMI, 13);
        assert_eq!(line::AVEC, 14);
    }

    #[test]
    fn estat_fields_extract() {
        // EC 11 (SYS), subcode 3, IS = timer|ipi lines.
        let estat = (11u64 << 16) | (3u64 << 22) | (1 << 11) | (1 << 12);
        assert_eq!(estat_ec(estat), 11);
        assert_eq!(estat_subcode(estat), 3);
        assert_eq!(estat_is(estat), (1 << 11) | (1 << 12));
    }

    #[test]
    fn page_exceptions_are_the_three_refills() {
        assert!(is_page_exception(ec::PIL));
        assert!(is_page_exception(ec::PIS));
        assert!(is_page_exception(ec::PIF));
        assert!(!is_page_exception(ec::PME), "PME is a dirty-bit fault");
        assert!(!is_page_exception(ec::SYS));
        assert!(!is_page_exception(ec::INT));
    }

    #[test]
    fn decode_covers_the_table() {
        assert_eq!(decode_ec(ec::SYS), TrapKind::Syscall);
        assert_eq!(decode_ec(ec::PIS), TrapKind::TlbStoreMiss);
        assert_eq!(decode_ec(ec::BP), TrapKind::Breakpoint);
        assert_eq!(decode_ec(ec::IPE), TrapKind::InstructionPrivileged);
        assert_eq!(decode_ec(ec::ADE), TrapKind::AddressError { fetch: false });
        assert_eq!(decode_ec(ec::FPDIS), TrapKind::ExtensionDisabled(ec::FPDIS));
        assert_eq!(decode_ec(ec::HVC), TrapKind::Guest(ec::HVC));
        assert_eq!(decode_ec(42), TrapKind::Reserved(42));
    }

    #[test]
    fn slot_arithmetic_matches_linux_layout() {
        // VECSIZE = 64 at VS = 4: exceptions enter at EC × 64.
        assert_eq!(vec_size(VS_LINUX), 64);
        assert_eq!(exception_entry_offset(ec::SYS, VS_LINUX), 11 * 64);
        // Interrupts enter above the 64 exception slots: 64 + line.
        assert_eq!(interrupt_slot(line::TIMER), 64 + 11);
        assert_eq!(
            interrupt_entry_offset(line::TIMER, VS_LINUX),
            (64 + 11) * 64
        );
        assert_eq!(interrupt_entry_offset(line::IPI, VS_LINUX), (64 + 12) * 64);
        // Linux's debug entry sits at EENTRY + 0x480 — slot 18 at VS=4.
        assert_eq!(18 * vec_size(VS_LINUX), 0x480);
    }

    #[test]
    fn slot_arithmetic_scales_with_vs() {
        assert_eq!(vec_size(0), 4);
        assert_eq!(vec_size(7), 512);
        assert_eq!(exception_entry_offset(3, 7), 3 * 512);
        assert_eq!(interrupt_entry_offset(0, 7), 64 * 512);
    }
}
