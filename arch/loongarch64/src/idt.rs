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
//! `PRMD.PIE` → `CRMD.IE` and jumps to `ERA`. See
//! [`crate::syscall::userspace`] for how VelaOS builds `PRMD` before
//! entering ring 3.
//!
//! # The entry stub and the frame
//!
//! [`vela_trap_entry`] (assembly) is what every vector slot branches to. It
//! switches to the kernel stack when the trap came from ring 3, saves the
//! register file into a [`TrapFrame`] on that stack, and calls
//! [`vela_trap_handler`], which dispatches on the exception code. Before
//! returning, the stub re-installs `ERA` and `PRMD` *from the frame* and
//! finishes with a single `ertn` — so a handler that edits `frame.era`
//! (skipping a faulting instruction, for instance) changes where the
//! interrupted code resumes, and a nested trap's clobber of the two CSRs is
//! undone by the outer frame's saved values.

/// The register state a trap saves.
///
/// The LoongArch trap entry saves the general registers, the return address
/// (`ERA`), the exception status (`ESTAT`), the saved mode (`PRMD`) and the
/// trap-time mode (`CRMD`), and it switches to the kernel stack. The
/// dispatcher receives this frame so that
/// the *same* frame describes the state for every exception — a syscall
/// reads the arguments out of it, and a fault handler reads the faulting
/// address and the instruction out of it.
///
/// The frame is the kernel's own structure: the user program never sees it,
/// and the kernel writes it to a kernel-owned stack.
#[derive(Debug, Clone, Copy)]
#[repr(C, align(16))]
pub struct TrapFrame {
    /// `r1` — the return address.
    pub ra: u64,
    /// `r2` — the thread pointer.
    pub tp: u64,
    /// `r3` — the stack pointer.
    pub sp: u64,
    /// `r4` — the first argument register.
    pub a0: u64,
    /// `r5`.
    pub a1: u64,
    /// `r6`.
    pub a2: u64,
    /// `r7`.
    pub a3: u64,
    /// `r8`.
    pub a4: u64,
    /// `r9`.
    pub a5: u64,
    /// `r10`.
    pub a6: u64,
    /// `r11` — the syscall number.
    pub a7: u64,
    /// `r12`.
    pub t0: u64,
    /// `r13`.
    pub t1: u64,
    /// `r14`.
    pub t2: u64,
    /// `r15`.
    pub t3: u64,
    /// `r16`.
    pub t4: u64,
    /// `r17`.
    pub t5: u64,
    /// `r18`.
    pub t6: u64,
    /// `r19`.
    pub t7: u64,
    /// `r20`.
    pub t8: u64,
    /// `r22` — the frame pointer.
    pub fp: u64,
    /// `r23`.
    pub s0: u64,
    /// `r24`.
    pub s1: u64,
    /// `r25`.
    pub s2: u64,
    /// `r26`.
    pub s3: u64,
    /// `r27`.
    pub s4: u64,
    /// `r28`.
    pub s5: u64,
    /// `r29`.
    pub s6: u64,
    /// `r30`.
    pub s7: u64,
    /// `r31`.
    pub s8: u64,
    /// The return address.
    pub era: u64,
    /// The exception status.
    pub estat: u64,
    /// The `PRMD` the trap was taken with: the interrupted privilege level
    /// and interrupt enable. Restored before `ertn` (see the module docs).
    pub prmd: u64,
    /// The mode the processor was in.
    pub crmd: u64,
    /// The bad virtual address, for the address faults.
    pub badv: u64,
}

impl TrapFrame {
    /// A frame with nothing saved in it.
    pub const fn empty() -> Self {
        Self {
            ra: 0,
            tp: 0,
            sp: 0,
            a0: 0,
            a1: 0,
            a2: 0,
            a3: 0,
            a4: 0,
            a5: 0,
            a6: 0,
            a7: 0,
            t0: 0,
            t1: 0,
            t2: 0,
            t3: 0,
            t4: 0,
            t5: 0,
            t6: 0,
            t7: 0,
            t8: 0,
            fp: 0,
            s0: 0,
            s1: 0,
            s2: 0,
            s3: 0,
            s4: 0,
            s5: 0,
            s6: 0,
            s7: 0,
            s8: 0,
            era: 0,
            estat: 0,
            prmd: 0,
            crmd: 0,
            badv: 0,
        }
    }

    /// A frame with every register zero.
    pub const fn zeroed() -> Self {
        Self::empty()
    }

    /// The syscall number: the `a7` register.
    pub const fn syscall_number(&self) -> u64 {
        self.a7
    }

    /// The syscall arguments: `a0`–`a5`.
    pub const fn syscall_args(&self) -> [u64; 6] {
        [self.a0, self.a1, self.a2, self.a3, self.a4, self.a5]
    }

    /// The exception class the frame describes.
    pub fn kind(&self) -> TrapKind {
        decode_ec(estat_ec(self.estat))
    }
}

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

// ---------------------------------------------------------------------------
// The frame layout the entry stub builds.
// ---------------------------------------------------------------------------

/// The offset of each register in a [`TrapFrame`] — the contract between the
/// [`vela_trap_entry`] assembly and the Rust struct. The tests pin every
/// offset against the struct's actual field addresses.
pub mod off {
    /// `$r1` — the return address.
    pub const RA: usize = 0;
    /// `$r2` — the thread pointer.
    pub const TP: usize = 8;
    /// `$r3` — the stack pointer.
    pub const SP: usize = 16;
    /// `$r4`–`$r11` — the argument registers.
    pub const A0: usize = 24;
    /// `$r12`–`$r20` — the temporary registers.
    pub const T0: usize = 88;
    /// `$r22` — the frame pointer.
    pub const FP: usize = 160;
    /// `$r23`–`$r31` — the callee-saved registers.
    pub const S0: usize = 168;
    /// The saved `ERA`.
    pub const ERA: usize = 240;
    /// The saved `ESTAT`.
    pub const ESTAT: usize = 248;
    /// The saved `PRMD`.
    pub const PRMD: usize = 256;
    /// The saved `CRMD`.
    pub const CRMD: usize = 264;
    /// The saved `BADV`.
    pub const BADV: usize = 272;

    /// The `a0`–`a7` block's stride.
    pub const A_STRIDE: usize = 8;
    /// The `t0`–`t8` block's stride.
    pub const T_STRIDE: usize = 8;
    /// The `s0`–`s8` block's stride.
    pub const S_STRIDE: usize = 8;

    // The KS CSRs the entry stub uses as scratch. `KS0` parks the
    // interrupted `t0` while the stub computes; `KS4` parks the interrupted
    // `sp`; `KS5` holds this CPU's kernel stack top (see
    // [`set_kernel_stack`]).
    //
    // The KS registers are per-CPU by definition, so this scheme needs no
    // locks on an SMP machine — each core parks in its own CSRs.
    /// `KS0` — scratch for the interrupted `t0`.
    pub const KS_SCRATCH_T0: u16 = crate::regs::CSR_KS0;
    /// `KS4` — scratch for the interrupted `sp`.
    pub const KS_SCRATCH_SP: u16 = crate::regs::CSR_KS0 + 4;
    /// `KS5` — this CPU's kernel stack top.
    pub const KS_KERNEL_STACK: u16 = crate::regs::CSR_KS0 + 5;
}

/// Encode the `b` (unconditional PC-relative branch) word that jumps from
/// `from` to `to`.
///
/// `b` is opcode `0b010100` — word `0x5000_0000` — with a signed 26-bit
/// offset in units of 4 bytes. The immediate is split across the word: bits
/// 25:10 hold `offs[15:0]` and bits 9:0 hold `offs[25:16]`, where `offs` is
/// `(to - from) >> 2`. The encoding was pinned against GNU as
/// (`loongarch64-linux-gnu-as`): `b .+4` assembles to `0x5000_0400` and
/// `b .-4` to `0x53ff_ffff`.
///
/// The branch range is ±(2²⁵ − 1) words = ±(128 MiB − 4) bytes (verified
/// against GNU as: a delta of exactly ±2²⁵ words overflows); `install`
/// asserts the vector page and the entry stub are within that of each
/// other.
pub const fn branch_word(from: u64, to: u64) -> u32 {
    let offs = ((to as i64) - (from as i64)) >> 2;
    let offs = offs as u64;
    0x5000_0000 | (((offs & 0xFFFF) << 10) as u32) | (((offs >> 16) & 0x3FF) as u32)
}

/// Park this CPU's kernel stack top where the trap entry finds it.
///
/// The entry stub switches to this stack for any trap that arrives from
/// ring 3, so it must be called before user code runs or interrupts are
/// enabled on this CPU. The value lives in the `KS5` CSR, which is per-CPU
/// by definition — each core parks its own stack without sharing anything.
///
/// # Safety
///
/// `top` must be the top of a mapped, writable, 16-byte-aligned kernel
/// stack owned by this CPU.
pub unsafe fn set_kernel_stack(top: u64) {
    if !crate::regs::ON_BARE_METAL {
        return;
    }
    // SAFETY: the caller guarantees the stack exists and belongs to this
    // CPU; the CSR is per-CPU state.
    unsafe {
        crate::regs::write_csr::<{ off::KS_KERNEL_STACK }>(top);
    }
}

#[cfg(target_arch = "loongarch64")]
extern "C" {
    /// The single trap-entry stub, in assembly below.
    fn vela_trap_entry();
}

/// Install the trap-entry page: point `EENTRY` at `base`, program
/// `ECFG.VS`, and fill every vector slot with a branch to the entry stub.
///
/// Every exception slot (`0..=25`) and every interrupt slot (`64..=78`,
/// i.e. `EENTRY + (64 + line) × VECSIZE`) receives one `b` instruction —
/// the first word of its 64-byte slot — branching to
/// `vela_trap_entry`, which then dispatches on the hardware-supplied
/// exception code. Filling every slot with the *same* target keeps the
/// install a flat loop: the slot itself carries no information the stub
/// does not also find in `ESTAT`.
///
/// # Safety
///
/// `base` must be page-aligned, kernel-writable and executable, with at
/// least `(64 + 15) × 64` bytes free at it, and must sit within ±128 MiB of
/// the entry stub (the reach of a `b`). Called before interrupts are
/// enabled: between the slot fill and the `EENTRY` write a trap would land
/// in half-installed vectors.
pub unsafe fn install(base: u64) {
    #[cfg(target_arch = "loongarch64")]
    // SAFETY: the caller guarantees the base address, alignment and that
    // interrupts are off; the writes only touch the vector page.
    unsafe {
        let entry = vela_trap_entry as *const () as usize as u64;
        let mut slot = base;
        // Exception slots 0..=25, then the interrupt slots 64..=78.
        for ec in 0..=25u8 {
            let delta = (entry as i64) - (slot as i64);
            assert!(
                delta.abs() <= ((1i64 << 25) - 1) * 4,
                "vector slot {ec} out of b's ±128 MiB reach"
            );
            core::ptr::write_volatile(slot as *mut u32, branch_word(slot, entry));
            slot += vec_size(VS_LINUX);
        }
        for line in 0..=14u16 {
            let slot = base + interrupt_entry_offset(line, VS_LINUX);
            core::ptr::write_volatile(slot as *mut u32, branch_word(slot, entry));
        }
        // The branch words are new instruction memory: sync it before the
        // CPU can fetch any of it.
        crate::regs::dbar();
        crate::regs::ibar();

        crate::regs::write_csr::<{ crate::regs::CSR_EENTRY }>(base);
        // `ECFG.VS` decides the slot arithmetic above; `LIE` (bits 14:0) is
        // left as `apic::enable_line` programmed it.
        crate::regs::xchg_csr::<{ crate::regs::CSR_ECFG }>(
            VS_LINUX << VS_SHIFT,
            0b111u64 << VS_SHIFT,
        );
    }
    #[cfg(not(target_arch = "loongarch64"))]
    {
        // Hosted builds have no vector page to install; the pure pieces
        // ([`branch_word`], the slot arithmetic) are what the tests cover.
        let _ = base;
    }
}

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    r#"
    .globl vela_trap_entry
    .type vela_trap_entry, @function
vela_trap_entry:
    # Hardware has already set CRMD.PLV=0, CRMD.IE=0, PRMD (the interrupted
    # PLV/IE), ERA, ESTAT and BADV/BADI. The general registers are still the
    # interrupted context's.
    #
    # Park the two registers the stub needs as scratch — the interrupted sp
    # and t0 — in the KS CSRs, which swap rather than clobber.
    csrwr  $sp, {ks_sp}
    csrwr  $t0, {ks_t0}

    # Choose the stack. From ring 3 the trap must not run on the (unmapped
    # or user-writable) user stack: switch to this CPU's kernel stack, which
    # set_kernel_stack parked in KS5. From ring 0 the interrupted stack is
    # already a kernel stack; $sp still holds it.
    csrrd  $t0, {prmd_csr}
    andi   $t0, $t0, 0x3          # PRMD.PPLV, bits 1:0
    bne    $t0, $zero, 1f
    b      2f
1:  csrrd  $sp, {ks_stack}
2:
    # The frame sits below the (possibly new) stack pointer.
    addi.d $sp, $sp, -{frame_size}

    # The two parked values come back out of the KS CSRs. $t0 is dead from
    # here on — the interrupted t0 lives in the frame until the restore.
    csrrd  $t0, {ks_sp}
    st.d   $t0, $sp, {off_sp}
    csrrd  $t0, {ks_t0}
    st.d   $t0, $sp, {off_t0}

    # The rest of the register file, saved in place (nothing below clobbers
    # a register before its store).
    st.d   $ra, $sp, {off_ra}
    st.d   $tp, $sp, {off_tp}
    st.d   $a0, $sp, {off_a0}
    st.d   $a1, $sp, {off_a0} + 8
    st.d   $a2, $sp, {off_a0} + 16
    st.d   $a3, $sp, {off_a0} + 24
    st.d   $a4, $sp, {off_a0} + 32
    st.d   $a5, $sp, {off_a0} + 40
    st.d   $a6, $sp, {off_a0} + 48
    st.d   $a7, $sp, {off_a0} + 56
    st.d   $t1, $sp, {off_t0} + 8
    st.d   $t2, $sp, {off_t0} + 16
    st.d   $t3, $sp, {off_t0} + 24
    st.d   $t4, $sp, {off_t0} + 32
    st.d   $t5, $sp, {off_t0} + 40
    st.d   $t6, $sp, {off_t0} + 48
    st.d   $t7, $sp, {off_t0} + 56
    st.d   $t8, $sp, {off_t0} + 64
    st.d   $fp, $sp, {off_fp}
    st.d   $s0, $sp, {off_s0}
    st.d   $s1, $sp, {off_s0} + 8
    st.d   $s2, $sp, {off_s0} + 16
    st.d   $s3, $sp, {off_s0} + 24
    st.d   $s4, $sp, {off_s0} + 32
    st.d   $s5, $sp, {off_s0} + 40
    st.d   $s6, $sp, {off_s0} + 48
    st.d   $s7, $sp, {off_s0} + 56
    st.d   $s8, $sp, {off_s0} + 64

    # The CSR state, saved early — the handler may clobber all of these.
    csrrd  $t0, {era_csr}
    st.d   $t0, $sp, {off_era}
    csrrd  $t0, {estat_csr}
    st.d   $t0, $sp, {off_estat}
    csrrd  $t0, {prmd_csr}
    st.d   $t0, $sp, {off_prmd}
    csrrd  $t0, {crmd_csr}
    st.d   $t0, $sp, {off_crmd}
    csrrd  $t0, {badv_csr}
    st.d   $t0, $sp, {off_badv}

    # Dispatch: $a0 = the frame, and the handler returns the frame to
    # restore in $a0 (it may have switched stacks or tasks).
    move   $a0, $sp
    la.abs $t8, vela_trap_handler
    jirl   $ra, $t8, 0

    # Restore. The returned frame pointer is parked in $a7, whose saved
    # value is restored last, self-referentially ($a7 is both base and
    # destination of that load).
    move   $a7, $a0
    ld.d   $ra, $a7, {off_ra}
    ld.d   $tp, $a7, {off_tp}
    ld.d   $a0, $a7, {off_a0}
    ld.d   $a1, $a7, {off_a0} + 8
    ld.d   $a2, $a7, {off_a0} + 16
    ld.d   $a3, $a7, {off_a0} + 24
    ld.d   $a4, $a7, {off_a0} + 32
    ld.d   $a5, $a7, {off_a0} + 40
    ld.d   $a6, $a7, {off_a0} + 48
    ld.d   $t1, $a7, {off_t0} + 8
    ld.d   $t2, $a7, {off_t0} + 16
    ld.d   $t3, $a7, {off_t0} + 24
    ld.d   $t4, $a7, {off_t0} + 32
    ld.d   $t5, $a7, {off_t0} + 40
    ld.d   $t6, $a7, {off_t0} + 48
    ld.d   $t7, $a7, {off_t0} + 56
    ld.d   $t8, $a7, {off_t0} + 64
    ld.d   $fp, $a7, {off_fp}
    ld.d   $s0, $a7, {off_s0}
    ld.d   $s1, $a7, {off_s0} + 8
    ld.d   $s2, $a7, {off_s0} + 16
    ld.d   $s3, $a7, {off_s0} + 24
    ld.d   $s4, $a7, {off_s0} + 32
    ld.d   $s5, $a7, {off_s0} + 40
    ld.d   $s6, $a7, {off_s0} + 48
    ld.d   $s7, $a7, {off_s0} + 56
    ld.d   $s8, $a7, {off_s0} + 64

    # ERA and PRMD are re-installed from the frame: a handler may have
    # edited them (skipping an instruction, switching tasks) and a nested
    # trap has clobbered them.
    ld.d   $t0, $a7, {off_era}
    csrwr  $t0, {era_csr}
    ld.d   $t0, $a7, {off_prmd}
    csrwr  $t0, {prmd_csr}

    # $t0 now holds the old ERA value (dead), so the real t0 is restored
    # after the CSR writes. $sp follows, then $a7 restores itself last —
    # every load above still reads the frame through it.
    ld.d   $t0, $a7, {off_t0}
    ld.d   $sp, $a7, {off_sp}
    ld.d   $a7, $a7, {off_a0} + 56
    ertn
    .size vela_trap_entry, . - vela_trap_entry
    "#,
    ks_t0 = const off::KS_SCRATCH_T0,
    ks_sp = const off::KS_SCRATCH_SP,
    ks_stack = const off::KS_KERNEL_STACK,
    prmd_csr = const crate::regs::CSR_PRMD,
    era_csr = const crate::regs::CSR_ERA,
    estat_csr = const crate::regs::CSR_ESTAT,
    crmd_csr = const crate::regs::CSR_CRMD,
    badv_csr = const crate::regs::CSR_BADV,
    frame_size = const core::mem::size_of::<TrapFrame>(),
    off_ra = const off::RA,
    off_tp = const off::TP,
    off_sp = const off::SP,
    off_a0 = const off::A0,
    off_t0 = const off::T0,
    off_fp = const off::FP,
    off_s0 = const off::S0,
    off_era = const off::ERA,
    off_estat = const off::ESTAT,
    off_prmd = const off::PRMD,
    off_crmd = const off::CRMD,
    off_badv = const off::BADV,
);

/// The Rust side of every trap: dispatch on the exception code.
///
/// Called by [`vela_trap_entry`] with `$a0` = the frame; the returned
/// pointer (again `$a0`) is the frame the stub restores — normally the same
/// one, but the stub does not assume it.
///
/// The dispatch mirrors the x86-64 crate's split:
///
/// * **Interrupts** are not faults — the machine must resume. The timer
///   line is acknowledged (`CSR.TICLR`, which does not clear itself); any
///   *other* line is reported and the machine stops, because an
///   unacknowledged asserted line re-fires the moment `ertn` re-enables
///   interrupts, and this crate has no registered device handlers to
///   acknowledge the rest.
/// * **`syscall` (EC 11)** goes to [`crate::syscall::gate::dispatch`], which
///   calls the kernel-registered handler and returns through the frame.
/// * **Page exceptions** and **everything else** are reported (exception
///   code, `BADV`, `ERA`) and the machine stops: a fault the kernel cannot
///   fix would re-execute the same instruction forever.
/// # Safety
///
/// The frame must be a live `TrapFrame` built by [`vela_trap_entry`] on the
/// kernel stack — the stub is this function's only caller. The returned
/// pointer names the frame the stub restores; it is valid until `ertn`.
pub unsafe extern "C" fn vela_trap_handler(frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: the assembly stub built this frame on the kernel stack and
    // keeps it alive for the whole call.
    let frame = unsafe { &mut *frame };
    let ec = estat_ec(frame.estat);

    if ec == ec::INT {
        let lines = estat_is(frame.estat);
        if lines & (1 << line::TIMER) != 0 {
            // SAFETY: the timer interrupt is this CPU's to acknowledge; the
            // write clears the pending line so it does not re-fire.
            unsafe { crate::timer::acknowledge() };
        } else {
            report_unhandled_interrupt(lines, frame);
        }
    } else if ec == ec::SYS {
        // SAFETY: the frame is the stub's; the gate reads it and writes the
        // syscall result back into it.
        unsafe { crate::syscall::gate::dispatch(frame) };
    } else if is_page_exception(ec) {
        report_page_exception(frame);
    } else {
        report_trap(frame);
    }
    frame
}

/// Report an interrupt line nobody in this crate claims, then stop.
fn report_unhandled_interrupt(lines: u16, frame: &TrapFrame) -> ! {
    serial_report(
        "\n*** UNHANDLED INTERRUPT ***\n",
        frame,
        Some((lines, "lines")),
    )
}

/// Report a TLB refill exception, then stop.
///
/// VelaOS fills its TLB in software ([`crate::paging`]), but the refill
/// handler needs the kernel's address-space registry, which lives above
/// this crate — so a refill that reaches here is a mapping bug and is
/// reported with the faulting address (`BADV`) rather than retried.
fn report_page_exception(frame: &TrapFrame) -> ! {
    serial_report("\n*** PAGE EXCEPTION (TLB REFILL) ***\n", frame, None)
}

/// Report any other unhandled exception, then stop.
fn report_trap(frame: &TrapFrame) -> ! {
    serial_report("\n*** UNHANDLED EXCEPTION ***\n", frame, None)
}

/// Print the diagnostic and stop the machine.
///
/// `extra` is an optional `(value, name)` pair printed after the standard
/// fields — the asserted interrupt lines, for the interrupt report.
fn serial_report(header: &str, frame: &TrapFrame, extra: Option<(u16, &str)>) -> ! {
    crate::serial::puts(header);
    crate::serial::puts("    ESTAT      = ");
    crate::serial::puts_hex(frame.estat);
    crate::serial::puts("\n    EC         = ");
    crate::serial::puts_dec(u64::from(estat_ec(frame.estat)));
    crate::serial::puts(" (");
    crate::serial::puts(kind_name(frame.kind()));
    crate::serial::puts(")\n    ERA        = ");
    crate::serial::puts_hex(frame.era);
    crate::serial::puts("\n    BADV       = ");
    crate::serial::puts_hex(frame.badv);
    crate::serial::puts("\n    PRMD       = ");
    crate::serial::puts_hex(frame.prmd);
    if let Some((value, name)) = extra {
        crate::serial::puts("\n    ");
        crate::serial::puts(name);
        crate::serial::puts("       = 0x");
        crate::serial::puts_hex(u64::from(value));
    }
    crate::serial::puts("\n");
    // SAFETY: the machine stops on purpose; there is no handler to return
    // to and resuming would re-execute the same fault forever.
    unsafe { crate::qemu_exit::power_off(1) }
}

/// The `Debug`-free name of a trap kind, for the diagnostic above.
fn kind_name(kind: TrapKind) -> &'static str {
    match kind {
        TrapKind::Interrupt(_) => "interrupt",
        TrapKind::TlbLoadMiss => "TLB miss on load",
        TrapKind::TlbStoreMiss => "TLB miss on store",
        TrapKind::TlbFetchMiss => "TLB miss on fetch",
        TrapKind::TlbModified => "TLB modified",
        TrapKind::TlbNoRead => "TLB read-inhibited",
        TrapKind::TlbNoExec => "TLB execute-inhibited",
        TrapKind::TlbPrivilege => "TLB privilege error",
        TrapKind::AddressError { fetch: true } => "address error (fetch)",
        TrapKind::AddressError { fetch: false } => "address error (data)",
        TrapKind::Unaligned => "unaligned access",
        TrapKind::BoundsCheck => "bounds-check error",
        TrapKind::Syscall => "syscall",
        TrapKind::Breakpoint => "breakpoint",
        TrapKind::InstructionNotExist => "instruction not exists",
        TrapKind::InstructionPrivileged => "instruction privileged error",
        TrapKind::ExtensionDisabled(_) => "extension disabled",
        TrapKind::FpException => "floating-point exception",
        TrapKind::Watchpoint { .. } => "watchpoint",
        TrapKind::BinaryTranslation(_) => "binary translation",
        TrapKind::Guest(_) => "guest error",
        TrapKind::Reserved(_) => "reserved exception code",
    }
}

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

    #[test]
    fn the_frame_offsets_are_where_the_fields_are() {
        let frame = TrapFrame::empty();
        let base = &frame as *const TrapFrame as usize;
        assert_eq!(&frame.ra as *const u64 as usize - base, off::RA);
        assert_eq!(&frame.tp as *const u64 as usize - base, off::TP);
        assert_eq!(&frame.sp as *const u64 as usize - base, off::SP);
        assert_eq!(&frame.a0 as *const u64 as usize - base, off::A0);
        assert_eq!(&frame.t0 as *const u64 as usize - base, off::T0);
        assert_eq!(&frame.fp as *const u64 as usize - base, off::FP);
        assert_eq!(&frame.s0 as *const u64 as usize - base, off::S0);
        assert_eq!(&frame.era as *const u64 as usize - base, off::ERA);
        assert_eq!(&frame.estat as *const u64 as usize - base, off::ESTAT);
        assert_eq!(&frame.prmd as *const u64 as usize - base, off::PRMD);
        assert_eq!(&frame.crmd as *const u64 as usize - base, off::CRMD);
        assert_eq!(&frame.badv as *const u64 as usize - base, off::BADV);
        // The blocks are contiguous: a0..a7, t0..t8 and s0..s8 each stride
        // by one word, which is what the stub's `+ 8` offsets count on.
        for i in 0..8u64 {
            assert_eq!(
                off::A0 + (i as usize) * off::A_STRIDE,
                off::A0 + i as usize * 8
            );
        }
        assert_eq!(off::T0 + 8 * off::T_STRIDE, off::T0 + 64, "t8's slot");
        assert_eq!(off::S0 + 8 * off::S_STRIDE, off::S0 + 64, "s8's slot");
        // a7's slot is the stub's final self-referential restore.
        assert_eq!(off::A0 + 7 * off::A_STRIDE, 80);
    }

    #[test]
    fn the_frame_is_the_size_the_stub_pushes() {
        // The stub carves the frame off the kernel stack with a single
        // `addi.d` — 12-bit signed immediate — so the size must stay under
        // 2048 and a multiple of 16 (the stack-alignment contract).
        let size = core::mem::size_of::<TrapFrame>();
        assert_eq!(size % 16, 0);
        assert!(size < 2048);
        assert_eq!(size, 288);
    }

    #[test]
    fn branch_words_match_the_assembler() {
        // Pinned against `loongarch64-linux-gnu-as`: `b` is 0x50000000 with
        // a 26-bit offset (in 4-byte units) split as offs[15:0] in bits
        // 25:10 and offs[25:16] in bits 9:0.
        assert_eq!(branch_word(0x1000, 0x1004), 0x5000_0400, "b .+4");
        assert_eq!(branch_word(0x1000, 0x1000), 0x5000_0000, "b .");
        assert_eq!(branch_word(4, 20), 0x5000_1000, "b .+16");
        assert_eq!(branch_word(4, 64), 0x5000_3c00, "b .+60");
        assert_eq!(branch_word(4, 0), 0x53ff_ffff, "b .-4 (all-ones offset)");
        // A backward branch of -20 words was assembled as 0x5bffec00 for
        // `beq`; the same split applies to `b`.
        assert_eq!(branch_word(0, 80), 0x5000_5000, "slot 0 to slot 20");
    }

    #[test]
    fn branch_words_reach_across_the_whole_layout() {
        // Every slot's branch must land on the same entry: decode each
        // word back and check the target. The delta below sits inside
        // `b`'s ±128 MiB reach (2²⁵ words) and has non-zero halves in
        // *both* immediate fields (offs[15:0] = 0xd15a, offs[25:16] =
        // 0x48), so a field-split mistake cannot cancel out.
        let base = 0x9000_0000_1000_0000;
        let entry = base + 0x123_4568;
        let decode = |word: u32| -> i64 {
            let low = ((word >> 10) & 0xFFFF) as i64;
            let high = ((word & 0x3FF) as i64) << 16;
            // The 26-bit immediate is signed: extend bit 25.
            (((low | high) << 38) >> 38) << 2
        };
        let mut slot = base;
        for ec in 0..=25u8 {
            let word = branch_word(slot, entry);
            assert_eq!(word & 0xFC00_0000, 0x5000_0000, "b opcode");
            assert_eq!(decode(word), entry as i64 - slot as i64, "slot for EC {ec}");
            slot += vec_size(VS_LINUX);
        }
        for line in 0..=14u16 {
            let slot = base + interrupt_entry_offset(line, VS_LINUX);
            let word = branch_word(slot, entry);
            assert_eq!(
                decode(word),
                entry as i64 - slot as i64,
                "slot for line {line}"
            );
        }
        // And a backward branch decodes with the sign restored.
        let word = branch_word(entry, base);
        assert_eq!(decode(word), base as i64 - entry as i64, "backward branch");
    }

    #[test]
    fn the_ks_scratch_is_the_architectural_ks_block() {
        // KS0..KS7 are CSRs 0x30..0x37; the stub parks sp in KS4 and the
        // kernel stack top in KS5, and none of them collide with the
        // architectural CSRs below 0x30 (TMID at 0x40 above them).
        assert_eq!(off::KS_SCRATCH_T0, 0x30);
        assert_eq!(off::KS_SCRATCH_SP, 0x34);
        assert_eq!(off::KS_KERNEL_STACK, 0x35);
    }
}
