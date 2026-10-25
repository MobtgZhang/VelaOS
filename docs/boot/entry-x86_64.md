# Entry on x86-64 — from firmware to ring 3

> **Scope:** the x86-64 boot sequence as it is implemented **today** in
> `boot/uefi-loader`, `boot/elf-loader`, `hal/x86_64` and `kernel/entry`.
> Every step below was read out of the source; where a step is described as
> "not yet" it is because the code does not do it.

---

## 1. The two binaries

| binary | format | runs under | source |
|---|---|---|---|
| `vela-uefi-loader` (`BOOTX64.EFI`) | PE/COFF | UEFI Boot Services | `boot/uefi-loader` |
| `vela-kernel` (`vela-kernel.elf`) | ELF64 | nothing — it *is* the kernel | `kernel/entry` |

They are separate binaries because the loader must run under a firmware ABI
and the kernel must not. The loader's job ends the instant it jumps; from
that point the kernel has no firmware at all.

---

## 2. The loader's sequence

The loader's own comments number its steps `B-01` through `B-08`, matching
the plan's table in `reports/VelaOS.md` §4.3.

### B-01 — EFI entry

`efi_main` is the `uefi` crate's `#[entry]` point. It installs the logger
first, so that everything after it is observable:

```
[INFO] VELA-BOOT VelaOS UEFI loader
```

The logger writes to the **debug console** (QEMU's `-debugcon`, port
`0xE9`), not to COM1. The reason is load-bearing: OVMF owns COM1 while Boot
Services run, so a `-serial file:` capture shows **nothing** from the loader.
The loader's marker and the kernel's markers therefore live in two different
files, and the boot smoke test checks them separately.

### B-02 — read the configuration and the kernel

The loader reads `\Vela\boot\vela.toml` through the Simple File System
protocol, then the kernel ELF from the path the config names.

The config format is parsed by a hand-rolled parser
(`boot/uefi-loader/src/config.rs`), not a TOML library: the loader cannot
afford a general-purpose deserializer before `ExitBootServices`, and the
config shape is tiny. Unknown keys are ignored, so a config written for a
newer loader still boots.

`\Vela\boot\vela.toml` as `xtask image` generates it:

```toml
[boot]
default = "vela"
timeout_seconds = 1
arch = "x86_64"

[entry.vela]
kernel = '\Vela\kernel\vela-kernel.elf'
video = "auto"
log_level = "info"
```

### B-03 — GOP and the splash

The loader opens the `GraphicsOutput` protocol, reads the current mode, and
draws the original VelaOS splash **directly into the GOP framebuffer**,
honouring the stride and the pixel format rather than assuming RGB.

The geometry it reports is exactly what the firmware gave it:

```
[INFO] GOP 1280x800 stride=5120B fmt=Bgr base=0x80000000 size=0x3e8000
```

Note `stride=5120B` for a 1280-pixel-wide mode: `1280 * 4` is 5120, so this
particular mode is **not** padded, but the loader reads the real stride and
would report a different one if the firmware gave it.

A mode whose byte length is smaller than `stride * height` is rejected with
`Status::BAD_BUFFER_SIZE` before the splash is drawn — a mode that lies
about its geometry would let the splash write out of bounds.

`Bgr` and `Rgb` are supported. `BLT_ONLY` and the YUV modes are refused:
they have no directly addressable framebuffer.

### B-04 — validate the kernel ELF

`boot/elf-loader` parses the ELF64 header and program headers and validates
them. The parser is a **pure function over a byte slice**, so it is fully
unit-testable on the host, and it has a dedicated hostile-input suite
(`boot/elf-loader/tests/hostile.rs`).

What it rejects is listed in that suite's own documentation; the ones that
matter for the boot path are the magic, the class, the machine type and the
executable type, the program-header table's offset and size, and any
segment whose alignment or bounds do not hold.

```
[INFO] kernel: 3 segments, span 0x9b000, entry 0xffff800000001f50
```

### B-05 — one pool, allocated before the exit

The loader takes **one** contiguous reservation from firmware memory before
`ExitBootServices`, and sub-allocates everything out of it:

* the kernel image;
* the kernel's boot stack (32 pages) and the loader's own stack (64 pages);
* the **kernel's heap** (1024 pages = 4 MiB);
* the **bootstrap page tables**, whose count is computed from the firmware's
  memory map rather than guessed;
* the persisted memory map and the boot-info page.

```
[INFO] reserved 1954 pages for the boot pool (463 for page tables)
```

Taking one pool, rather than several allocations, is what makes the
post-exit phase possible: after `ExitBootServices` there is no allocator, so
everything the kernel needs must already be allocated *and written*.

The heap is reserved **by the loader** rather than carved out by the kernel,
for a specific reason: the kernel's own page tables live in the heap, and
the kernel computes a page table's *physical* address from the heap pointer
that holds it. That is only sound if the heap is mapped at the same virtual
and physical address — which is exactly what the loader arranges and exactly
what the bootstrap page tables identity-map.

The kernel image is copied segment by segment. Each segment's physical
address is `kernel_paddr + (vaddr - image_vaddr)`, which preserves every
byte's offset from the start of the image.

### B-06 — BootInfo

The loader fills in the structure described in
[`boot-protocol-v1.md`](boot-protocol-v1.md). The framebuffer geometry and
the memory map are read **before** the exit, because the GOP protocol and
the firmware's memory map cease to exist afterwards.

The firmware's memory map is copied into the pool and **translated** on the
way out: each descriptor's `type` field is rewritten from the firmware's
`EFI_MEMORY_TYPE` into the boot ABI's own numbering. The kernel therefore
never has to know what the firmware's numbers are, and the
conventional-memory confusion (firmware type 7 versus ABI type 1) cannot
happen.

The map is also dumped to the debug console, one line per descriptor:

```
[INFO] memory map: 5568 bytes, 116 entries x 48 (version 1)
[INFO]   mem type 7 start 0x0 pages 135 (87000 bytes)
```

The dump is there because the kernel's physical memory plan depends entirely
on what the firmware reports, and a wrong assumption about it — that the
largest region is below 4 GiB, say — produces a boot that fails much later
and much less legibly.

### B-06b — the bootstrap page tables

These are built **before** `ExitBootServices`, because they are what make
the kernel's higher half reachable at all. The firmware's own tables map
low RAM only, and the kernel is linked at `0xFFFF_8000_0000_01000`.

```
[INFO] bootstrap pml4 at 0x1da34000 (29 pages)
```

They live in the pool, which the firmware identity-maps, so their addresses
are their physical addresses and the PML4's address can go straight into
`CR3` after the exit.

### B-07 — ExitBootServices

```
[INFO] exiting boot services
[INFO] boot services exited
```

The map key is obtained immediately before the call. Immediately afterwards,
the loader tells the logger to stop using the firmware's console handle,
because the firmware may reclaim it.

**Everything the kernel needs must already be allocated and written before
this call.** After it:

* the UEFI filesystem protocol, the GOP protocol, the Boot Services allocator
  and ordinary Boot Services are all gone;
* only conforming Runtime Services may still be used, and v0.1.0 does not
  depend on them;
* the loader's UEFI allocator and logger must stop depending on the
  firmware — the kernel uses its own allocator and its own serial driver.

### B-08 — the handoff

The loader writes the `VelaBootInfo` into a page its own pool owns, loads
the bootstrap PML4 into `CR3`, and jumps.

```
[INFO] entering kernel at 0xffff800000001f50 with cr3 0x1da34000
```

The jump itself, in `boot/uefi-loader/src/main.rs`:

```rust
core::arch::asm!(
    "mov cr3, {pml4}",
    "mfence",
    "mov rsp, {stack_top}",
    "and qword ptr [rsp - 8], 0",
    "mov rdi, {bi}",
    "xor ebp, ebp",
    "jmp {entry}",
    ...
);
```

The convention, which `kernel/entry` agrees with:

| register | meaning |
|---|---|
| `RDI` | the **physical** address of the `VelaBootInfo` |
| `RSP` | a private stack from the boot pool, 16-byte aligned, with the alignment slot cleared |
| `RBP` | zeroed, so no caller frame chain is implied |
| `CR3` | the bootstrap PML4, already written and fenced |

The `mfence` after the `CR3` write orders the page-table stores before the
`CR3` store, so the CPU never walks a table that is not yet written.

The order matters for two reasons:

1. The kernel's entry point is in the higher half, which the firmware's
   tables do not map. Without the `CR3` write the very first instruction
   fetch page-faults.
2. The pool — which holds the loader's own stack, the boot info and the page
   tables themselves — is identity-mapped by the new tables *and* by the
   firmware's, so the loader keeps running either way.

---

## 3. The kernel's sequence

`kernel/entry/src/lib.rs`, `kernel_entry(boot_info_paddr)`.

### 3.1 — Serial

The kernel installs its own COM1 driver and takes the port over from the
firmware:

```
VELA-KERNEL: entering
```

### 3.2 — BootInfo

Paging is still off, so the boot info's physical address is directly
dereferenceable. It is read **once** and copied out, because the page it
lives in is inside the loader's pool and will be reclaimed.

`check_identity()` rejects a bad magic, an unsupported ABI version or a
`struct_size` that is too small; each failure prints a diagnostic and powers
the machine off with a distinct non-zero exit status.

```
VELA-KERNEL: boot info accepted
```

### 3.3 — The heap, before anything allocates

The heap is installed **before** the memory map is parsed, because
`MemoryMap::parse` collects the descriptors into a `Vec`, and a `Vec`
allocates. An earlier version of this sequence parsed the map first, and the
kernel's very first allocation failed — a panic before the kernel had said
anything about itself.

The heap's *metadata* lives in the first bytes of the loader's reservation
and the bump region begins immediately after it. Putting the metadata there
is what makes the heap's virtual address and its physical address **the same
number**, which is what makes the identity translation of the heap — and
therefore the kernel's `pml4_phys` — honest rather than a placeholder.

```
VELA-KERNEL: heap installed at 1d632000
```

### 3.4 — The memory map and the physical plan

```
VELA-KERNEL: memory regions=116 total=13055 MiB usable=505 MiB
VELA-KERNEL: plan regions=116 total=13055 MiB usable=505 MiB frames=268435456 bitmap=32768 KiB heap=0 KiB allocatable=466 MiB
VELA-KERNEL: 265211553 frames free
```

### 3.5 — The identity map

```
VELA-KERNEL: identity map 6333 chunks
```

The map is planned **jointly**, over the whole machine, rather than region by
region. A 2 MiB page must be 2 MiB-aligned at both ends, so every region has
a head and a tail that only 4 KiB pages can cover. Two regions can share a
2 MiB slot — one's tail and the next one's head can both live in it — and if
either of them is mapped as a large page the other's 4 KiB pages have nowhere
to go.

The rule is therefore: a 2 MiB slot is mapped as a large page when *every*
byte of it is ordinary, usable RAM. If any part of the slot is a region's
head or tail, the whole slot is mapped 4 KiB at a time.

The framebuffer and the local APIC are **excluded** from the identity map
and mapped separately, uncached. A cacheable mapping of the same physical
page would be a second, conflicting view of the same bytes.

The loader's own reservations — the heap and the pool — are checked against
the identity map. If the loader had reserved the heap outside the RAM it
reported, the identity map would not cover it, and the kernel's own page
tables would become unreachable the instant `CR3` changed: a silent,
catastrophic triple fault. The check is done here, where it can still be
reported.

### 3.6 — The kernel image, W^X

```
VELA-KERNEL: kernel image mapped at ffff800000001000
```

The image is mapped at its **linked** virtual base (`kernel_vaddr_base`) from
its **physical** placement (`kernel_paddr`). These are unrelated numbers: the
linker script links the image at `0xFFFF_8000_0000_01000`, and the loader
places it wherever the pool gave it.

The image's *executable prefix* is mapped read-execute and everything after
it read-write. The prefix length is computed by the loader from the ELF's
segments — the end of the last segment that is both executable and has a
file extent — because the kernel does not parse its own ELF. A segment with
no file extent is `.bss`, and is data by definition.

### 3.7 — The device windows

The local APIC at `0xFEE0_0000` and the framebuffer are mapped uncached.
Without the APIC mapping, `apic::init` faults on its very first register
access — and because the IDT is installed by then, the fault is reported
loudly rather than silently black-screening.

### 3.8 — The stack and the address-space switch

```
VELA-KERNEL: stack at 1d6c4010
```

The boot stack is allocated from the heap, so it is mapped with the rest of
the kernel's data. It is allocated *after* the image mapping so that the
heap pointer is where the stack will be.

`activate(&space)` then loads the kernel's own CR3. From that point the
kernel runs from the higher half.

### 3.9 — The GDT and the syscall gate

```
VELA-KERNEL: gdt and syscall gate installed
```

The GDT must be installed before anything depends on a segment selector. The
firmware's GDT is still in place at this point and the firmware's `CS` is
`0x38` — a selector the kernel's own table does not contain. That is fine as
long as nothing reloads a segment, but the `syscall` gate's `IA32_STAR` names
selectors from the *kernel's* table, so the two have to agree first.

Interrupts are still disabled here, which the GDT's own documentation
requires: between the `LGDT` and the `CS` reload there is a window in which
an interrupt would return through a selector the new table does not have.

The GDT the kernel installs:

| index | selector | contents |
|---|---|---|
| 0 | `0x00` | the null descriptor |
| 1 | `0x08` | kernel code (ring 0) |
| 2 | `0x10` | kernel data (ring 0, read/write) |
| 3 | `0x18` | user data (ring 3, read/write) |
| 4 | `0x20` | user code (ring 3, 64-bit) |
| 5 | `0x28` | TSS, low half |

The order is not arbitrary. `IA32_STAR[63:48] = 0x10` selects the *user*
descriptors on the way back (`sysret` loads `CS` from `STAR[63:48] + 16` and
`SS` from `STAR[63:48] + 8`), and `IA32_STAR[47:32] = 0x08` selects the
*kernel* descriptors on the way in. With the table above, `sysret` returns to
`CS = 0x20` and `SS = 0x18` — the ring-3 descriptors — and `syscall` enters
at `CS = 0x08`, `SS = 0x10`.

The gate itself is in `kernel/syscall/src/gate.rs`:

* `syscall` does **not** switch the GS base. The kernel `swapgs`
  immediately on entry, before it trusts GS, and `swapgs` again immediately
  before `sysret`.
* `sysret` pops the return address from `RCX` and the flags from `R11`.
* The `RFLAGS` mask clears the interrupt-enable flag, so a syscall cannot
  leave interrupts disabled in ring 3.

### 3.10 — Interrupts

```
[idt] exception handlers installed
[apic] version 50014 id 0 window 0 live
[apic] version 50014 lvt 20030 initcnt 61a80 curcnt bef7 div b
```

`idt::init()` installs a gate for every vector: the exception stubs for
0–31, the timer stub at the APIC's vector, and the generic interrupt stub
for the rest, so that a stray IRQ is counted rather than being an unhandled
gate.

`apic::init()` brings the local APIC into **xAPIC** mode — firmware commonly
leaves it in x2APIC mode, in which the MMIO window at `0xFEE0_0000` is dead,
and a timer that never fires looks exactly like a timer that was never
programmed. The driver writes the divide register *before* the timer starts,
writes the initial count (which loads the counter and starts it) and does
**not** write the current count, because writing it would stop the timer
again.

The APIC's LVT timer gets the **periodic** bit as well as the vector:
without it the timer is one-shot, which looks like a working clock right up
until the second tick.

### 3.11 — The services and the banner

```
VELA-KERNEL: services installed
VELA-KERNEL: screen 1280x800
VELA-KERNEL: VelaOS 0.1.0 ready
```

The Exec service, the DOS service, the Graphics service, the Intuition
service and the syscall dispatch table are installed **before** the banner,
so that a user program started after the banner can be sure the boundary
exists. They live in leaked allocations, because they must outlive every
task that uses them.

The Intuition service's screen is the display's geometry, taken from the
boot handoff rather than from a fixed constant. A window manager sized for
the wrong screen clips everything that matters.

### 3.12 — Idle

The kernel has no userland yet in the current tree, so it idles with
interrupts enabled:

```rust
loop {
    regs::hlt();
}
```

The timer tick keeps the machine alive and gives the boot smoke test
something to observe.

---

## 4. Ring 3

The ring-3 machinery is implemented in `kernel/syscall/src/userspace.rs` and
`hal/x86_64/src/regs.rs`, but **the kernel does not yet start a user
program**: `kernel_entry` ends at the idle loop above. What exists today:

### 4.1 — The user address space

A user address space has its own page tables, and its own CR3. The kernel's
half is shared at the top.

The layout:

| region | address |
|---|---|
| the image | `0x0000_0000_0000_1000` |
| the heap | `0x0000_0000_4000_0000` |
| the stack top | `0x0000_7FFF_FFFF_F000`, growing down |
| the user ceiling | `0x0000_8000_0000_0000` |

The stack top is just under the 128 GiB user ceiling, and the ceiling is the
same number the user-copy helpers validate against. If the two disagreed, a
program could name an address the kernel would accept and the CPU would
fault on.

Text is mapped `user_rx` and data `user_rw`, and the page mapper's W^X rule
means a page can never be both writable and executable.

### 4.2 — The transition

`user_context(entry, stack_top)` lays out the exact frame `iretq` consumes,
and `enter_user(context)` drops to ring 3. The frame is:

| pushed | value |
|---|---|
| `SS` | `USER_DATA_SELECTOR \| 0b11` |
| `RFLAGS` | `RFLAGS_IF \| 0b10` |
| `CS` | `USER_CODE_SELECTOR \| 0b11` |
| `RIP` | the program's entry point |

The selectors come from the HAL's GDT, so they cannot drift away from the
descriptors the kernel actually installed. The RPL is added here because the
GDT holds ring-0 descriptors and the CPU requires the *loaded* selectors to
name a descriptor whose DPL matches the privilege level being entered.

### 4.3 — The TSS

`syscall`/`sysret` and `iretq` into ring 3 do not consult the TSS on entry,
but an interrupt taken *from* ring 3 loads `SS`/`RSP` from the TSS's
privilege stack. Without a TSS, the first timer interrupt while a user
program runs faults. The GDT and the TSS live in the HAL's static storage,
because they must survive a page-table switch and heap allocation would be
the wrong lifetime.

### 4.4 — The syscall boundary

`syscall_entry` is the ring-0 entry point. Its contract:

* the number in `RAX` selects the service;
* arguments come in `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9`;
* the result comes back in `RAX`;
* every pointer a user program passes is validated by `probe_read` /
  `probe_write`, so a bad user pointer yields an **error**, never a kernel
  fault.

That last point is the one the plan calls out explicitly, and it is tested:
`tests/api-conformance/tests/hostile_pointers.rs` passes deliberately
invalid pointers and asserts a clean `BAD_POINTER` error.

---

## 5. What is not implemented yet

Stated plainly, because a boot document that claims more than the code does
is worse than none:

* the kernel does not yet **start** a user program — `kernel_entry` ends at
  the idle loop;
* there is no `RunSeg`, and `userland/init` links the desktop into itself
  rather than spawning it;
* the `initrd` field is carried through the handoff, and the loader will load
  one if the config names it, but nothing consumes it yet;
* `acpi_rsdp_paddr` and `dtb_paddr` are carried through the handoff and are
  not read by the kernel;
* the other three architectures are compile-checked only; this document
  describes x86-64.

---

## 6. How to see it

```bash
cargo run -p xtask -- image --out target/vela.img
cargo run -p xtask -- qemu --image target/vela.img --timeout 30
```

Or, as the automated test does it:

```bash
cargo test -p vela-test-boot-smoke -- --nocapture
```

The test builds the image, boots it, asserts on the serial markers, and
**always** terminates QEMU — see
[`tests/boot-smoke/src/lib.rs`](../../tests/boot-smoke/src/lib.rs) for how
that is guaranteed.