# VelaOS Subsystem Analysis: Gaps vs AROS/AmigaOS Design

## Executive Summary

VelaOS is a new Amiga-inspired OS written in pure Rust, targeting x86-64 with UEFI boot (with aarch64/riscv64/loongarch64 compile-checked only). Compared to AROS and AmigaOS 3.1, VelaOS has a minimal viable core with significant gaps in filesystems, networking, audio, and multi-core task scheduling. Where AROS provides a near-class Amiga API with POSIX-like porting layers, and AmigaOS provides the classic 68000-centric architecture, VelaOS deliberately forges its own path: pure Rust, UEFI 2.11, ELF loading, and a custom unified boot protocol — no Multiboot2, no CMake, no shared-core, no CMakeLists.

This document analyzes the current state across seven subsystems and identifies what must be built to reach a functional v0.2.0.

**Build status:** `cargo build`, `cargo test --lib`, and `cargo clippy --workspace --all-targets -- -D warnings` all pass. The only warnings are the accepted baseline `rust-version` field and unused workspace dependencies (the non-booting arch crates).

---

## 1. Memory Management (4-level page table management)

### Current Implementation
- **Frame allocator**: `kernel/memory/src/lib.rs` — a static bitmap-based allocator with `alloc_frame/free_frame/reserve_range`, double-free tolerant, rotating hint for O(1) allocation. Supports reservation of non-allocatable regions.
- **Kernel heap**: `KernelHeap` — a bump allocator over a pre-reserved region, with alignment honoring and exhaustion detection. Used as the global `#[global_allocator]`.
- **MemoryPlan**: Builder that carves bitmap + heap regions out of the firmware memory map, reserves the null page, kernel image, initrd, framebuffer, and MMIO windows. Supports caller-specified reservations via `BootReservations`.
- **Address space**: `kernel/entry/src/lib.rs` builds a **higher-half** kernel: the kernel image is mapped at `0xFFFF_8000_0000_0000`, user space occupies the low 128 TiB. Page tables are built from scratch after `ExitBootServices`.
- **FrameSource bridge**: `kernel/entry/src/lib.rs` provides `UserFrames` wrapping `FrameAllocator` so the syscall layer (`kernel/syscall`) can allocate frames for `brk`/`mmap` without depending on the memory layer directly (avoiding a circular dependency).
- **Architecture boundary**: page tables, `AddressSpace`, `FrameSource`, `MapError`, `PageFlags`, `PAGE_SIZE` live in `arch/common` (`vela-arch-common`) and `arch/x86_64` (`vela-arch-x86_64`); the kernel depends only on the `vela-arch` facade, not on a concrete arch crate.

### Gaps vs AROS/AmigaOS
- **AROS**: Has a full virtual memory system with pagelists, swap support, and memory protection via page tables. Supports `SA` (shared architecture) modules that can be loaded at different addresses.
- **AmigaOS 3.x**: Uses a flat 24-bit physical address space with `MEMF_*` flags (CHIP, FAST, PUBLIC, PRIVATE). No virtual memory, no paging. Memory is allocated from `exec.library` using `CreateExecBase()` and the `memlist` linked list.
- **VelaOS Gaps**:
  - **No per-process page directory management**. The kernel uses a higher-half layout, but there is no per-process page table lifecycle beyond `TaskSpace`. User processes share the kernel's supervisor identity map plus their own user window.
  - **No memory protection isolation**: All user processes share the same virtual address space; there is no per-process ASID or switched CR3 on context switch beyond what `TaskSpace` tracks.
  - **No heap coalescing**: The kernel heap is a bump allocator that never reclaims.
  - **No `mmap`/`munmap` beyond `map_page`/`unmap_page`**: The page mapper exists but is not hooked into a user-VM layer.

### What Must Be Built
- Per-process `AddressSpace` with page directory/table lifecycle and CR3 switching on context switch.
- `brk`/`mmap` syscall wiring to actually allocate/deallocate frames via the page mapper (exec memory syscalls 22–30 are wired to `TaskSpace`, but the frame-allocator backing is partial).
- Memory protection (WP, US rings) if the MMU is to be used for isolation.

---

## 2. SMP (Multicore / Multithread Management)

### Current Implementation
- **Architecture boundary**: the trampoline, AP bring-up, GDT/IDT, APIC, and context switch live in `arch/x86_64` (`vela-arch-x86_64`), exported through the `vela-arch` facade as `smp`, `context`, `gdt`, `idt`, `apic`. `kernel/smp` has been removed; its code moved to `arch/x86_64/src/smp.rs`.
- **Real-mode trampoline**: `arch/x86_64/src/smp.rs` — hand-assembled bytes at `0x7000` that disable interrupts, set segments, load GDT, enter protected mode, read APIC ID, load CR3 (BSP PML4), enable PAE and long mode, far-jump to `ap_entry`.
- **INIT/INIT-deassert/SIPI IPI sequence**: `bring_up_aps()` sends the documented 3-step startup: INIT (resets AP), INIT-deassert (resynchronizes), SIPI (first and second attempts, vector 7 = trampoline at `0x7000`).
- **Per-AP configuration table**: Low memory at `0x8000`–`0x9FC0` holds PML4 base, AP entry point, stack top, and APIC ID per entry (`CONFIG_STRIDE = 32` bytes).
- **AP entry point**: `ap_entry(apic_id)` registers the CPU, installs GDT/IDT (protected by `AP_INIT_LOCK`), initializes APIC, marks ready, and spins in the idle loop with `sti`/`hlt`.
- **Ready-count synchronization**: `AP_READY_COUNT` (AtomicU32) with a timeout loop using APIC timer ticks.
- **BSP detects AP count**: Reads MADT from RSDP, counts Local APIC type-0 entries.
- **Scheduler SMP layer** (`kernel/scheduler/src/smp.rs`): per-CPU run queues (`RUN_QUEUES` indexed by CPU index), `init_this_cpu`, `enqueue_on`, `pick_next_on`, `peek_next_on`, `remove_from_queue`, `send_reschedule_ipi`, `needs_reschedule`, `acknowledge_reschedule`.
- **Reschedule IPI wiring**: `arch/x86_64/src/apic.rs` exposes `RESCHEDULE_VECTOR (0xF0)`, `send_ipi`, and a per-CPU `RESCHEDULE_PENDING` flag set by `on_reschedule_ipi` (which also sends EOI). `arch/x86_64/src/idt.rs` installs a handler for the reschedule vector that calls `on_reschedule_ipi`.
- **Syscall gate**: moved to `arch/x86_64/src/syscall/gate.rs`; the kernel registers its dispatch function via `gate::install(dispatch)`, so the arch owns the entry mechanism while the kernel owns dispatch policy.

### Cause analysis: why QEMU/OVMF reports 1 CPU
The loader's MADT parser (`boot/uefi-loader/src/main.rs::parse_madt_cpu_count`) had three defects:
1. **Mis-detected ACPI revision**: it keyed off the RSDP signature (`"RSD PTR "`), which is identical in ACPI 1.0 and 2.0+, so it could not choose between the RSDT (offset 16) and XSDT (offset 24). It now reads the revision byte at RSDP offset 15.
2. **Wrong table offsets**: it read XSDT at offset 36 and RSDT at offset 16; the architectural offsets are XSDT at 24 and RSDT at 16. It now uses offset 24 for XSDT.
3. **Cast instead of dereference**: it cast `(rsdp_paddr + offset) as *const u32 as u32` (the address value) rather than dereferencing the stored table address. It now dereferences with `*(ptr).add(N)`.
4. **MADT entry walk started at the wrong byte offset**: XSDT/RSDT entries begin at byte offset 36 (after the 36-byte ACPI table header), but the walk read from offset 32, producing garbage entry addresses and a `#GP`. It now reads entries at offset `36 + N * entry_size`.

After the fix, the parser is spec-compliant and no longer crashes. In the current QEMU/OVMF environment the firmware's MADT describes a single Local APIC entry, so `cpu_count` is reported as 1 even with `-smp 2`; on real firmware the corrected parser will report the true count.

### Gaps vs AROS/AmigaOS
- **AROS**: Multiprocessor support via `aros::smp` with per-CPU arrays, IPIs for inter-processor communication, and SMP-safe kernel locks. Designed for x86/ARM64.
- **AmigaOS 4.x**: Uses PPC multiprocessing with the `mp` library, but classic AmigaOS is essentially single-tasking — SMP is not a native concept.
- **VelaOS Gaps**:
  - **AP bring-up is not runtime-verified**: the trampoline code compiles and boots, but QEMU/OVMF reports 1 CPU, so AP bring-up has not been verified on multi-core hardware.
  - **No inter-processor task scheduling**: per-CPU run queues exist in `kernel/scheduler/src/smp.rs`, but the scheduler is not yet wired into the boot idle loop, so APs that boot land in an idle loop and no work is dispatched to them.
  - **No per-CPU variable partitioning or cache-affine scheduling**.
  - **Lock granularity**: `AP_INIT_LOCK` (`Mutex<()>`) serializes GDT/IDT installation during AP boot; there is no fine-grained locking for shared data structures across CPUs.

### What Must Be Built
- Verify AP bring-up on real multi-core hardware/firmware whose MADT reports >1 CPU.
- Wire per-CPU run queues into the scheduler's context-switch/idle loop so APs actually run tasks.
- Real IPI handling for rescheduling and inter-processor communication (the reschedule vector is defined and the flag exists; the scheduler consultation point needs wiring).
- Expand per-CPU data structures (current task, timer state).

---

## 3. File Management (AMIGADOS/DOS layer)

### Current Implementation
- **Syscalls 256–267** (`D_OPEN`–`D_SET_CURRENT_DIR`) wired in the dispatch table.
- **Exec memory syscalls (22–30)** wired to `TaskSpace` methods (`alloc_mem`, `alloc_pooled`, `free_mem`, `free_pooled`, `create_pool`, `delete_pool`, `avail_mem`) with conformance tests.
- **DOS library in SDK**: `open`, `close`, `read`, `write`, `seek`, `delete_file`, `create_dir`, `examine`, `current_dir`, `set_current_dir`, `read_dir`, `examine_dir`.
- **`IORequest` state machine** with `IoState` enum (`New → Opened → Submitted → [QUICK_DONE] → REUSABLE`, with `QUEUED → IN_FLIGHT → DONE → REPLIED`).
- **`DeviceManager`**: registers drivers, matches by name/unit, dispatches `OpenDevice`/`CloseDevice`/`SendIO`/`DoIO`/`WaitIO`/`AbortIO`.
- **Driver stubs**: ATA/SATA/NVMe/VGA registered; `OpenDevice` succeeds with unit counts.
- **`read_dir`** returns `DirEntry` records with `de_size`, `de_type`, `de_name_len`, `de_name`.

### Gaps vs AROS/AmigaOS
- **AROS**: Has `dos.library` with full `BPTR` path handling, `Assign`, `Volume`, `Path` construction, `ExamineSeg/Examine`, `OpenFile/ExFData`, `Seek`, `Write`, `Read`, `CurrentDir`, `SetDate`, `DeleteFile`. Supports both AmigaDOS and POSIX-style paths.
- **AmigaOS 3.x**: `dos.library` with `DOpen`, `DClose`, `DRead`, `DWrite`, `DSeek`, `DExamine`, `DCreateDir`, `DDeleteDir`, `CurrentDir`, `SetScript`, `Answerfile`. Paths are `BPTR`-based with `Volume` and `Assign` concepts.
- **VelaOS Gaps**:
  - **No actual filesystem**: RAMFS is planned but **not implemented**. All file operations against real storage fail.
  - **No block device layer**: No `BlockDevice` interface, no IOMMU/DMA mapping for real disks. The NVMe/ATA drivers exist but do not implement `DriverOps` fully.
  - **No volume/assign abstraction**: No volume lists, assign names, or path resolution beyond simple string matching.
  - **No file system stack**: No FAT, EXT2/3, RAMFS, or any actual filesystem driver. The `read_dir`/`examine_dir` calls have no backing store.
  - **No symlink/soft link support**: Path resolution is flat string comparison.

### What Must Be Built
- **RAMFS** implementation (minimum: create/read/write/ls).
- **Block device interface** for NVMe/ATA/SATA (implement `DriverOps::probe`/`open`/`submit`).
- **FAT filesystem** minimum (open/read/close).
- **Volume/Assign** abstraction for path resolution.
- **Directory entry caching** for `ls`/`cd` performance.

---

## 4. Network Stack

### Current Implementation
- **Zero**. The network stack does not exist.
- Shell commands `ping`, `ifconfig`, `netstat` all report: "network stack not available in v0.1.0".
- No TCP/UDP syscalls are wired; no socket API is defined in `numbers.rs`.
- No device drivers for network controllers.
- No IP stack, no ARP, no ICMP, no UDP/TCP.

### Gaps vs AROS/AmigaOS
- **AROS**: Has a BSD-socket-compatible network stack with `socket()`/`bind()`/`connect()`/`sendto()`/`recvfrom()` calls, `inet` module for IP processing, `netif` for interface management, `eth` driver framework, and `arp` support. AROS also has `netdb` for DNS resolution.
- **AmigaOS 3.x**: `network.library` with TCP/IP stack, `ICMP`, `UDP`, `TCP`. Socket API is `bsdsocket` compatible. `arp` and `rarp` support. `netdb` for DNS.
- **VelaOS Gaps**:
  - **No network stack at all**: No socket API, no protocol processing, no device drivers for NICs.
  - **No BSD socket syscalls**: No networking syscalls are defined in `numbers.rs`.
  - **No network device drivers**: The `network.md` report describes the design, but no driver code exists.

### What Must Be Built
- BSD socket syscalls (`socket`, `bind`, `connect`, `accept`, `sendto`, `recvfrom`).
- IP protocol stack (IPv4, ICMP).
- UDP and TCP implementations.
- Network device drivers (at minimum a `virtio-net` or simple PCI Ethernet driver).
- DNS resolver (`gethostbyname`).
- Network configuration (`ifconfig`/`netstat` actual implementation).

---

## 5. Input/Output and Driver Management

### Current Implementation
- **DeviceManager**: `Mutex<Vec<DriverEntry>>`, `Mutex<Vec<DeviceInstance>>`. Registers drivers via `register_driver(name, ops)`, probes devices via `probe_device(info)`, dispatches `OpenDevice`/`CloseDevice`/`SubmitIO`/`CompleteIO`/`AbortIO`.
- **IORequest state machine**: Full enum with all transition states.
- **DriverOps trait**: `probe`, `open`, `submit`, `cancel`, `remove`.
- **Registered drivers**: NVMe, ATA, SATA, VGA, HDMI/DVI. `OpenDevice` succeeds for these.
- **TimerDriver**: `DriverOps` implementation that wraps `timer_device::dispatch` for `TR_ADDREQUEST`/`TR_GETSYSTIME`/`TR_SETSYSTIME`.
- **PS/2 port access**: moved to `arch/x86_64/src/ps2.rs` (`DATA_PORT`, `STATUS_PORT`, `read_status`, `read_data`, `write_command`); the byte-stream decoding (`Keyboard`, `Mouse`) remains in the architecture-neutral `kernel/input`.
- **apic.rs**: IPI sending (`send_init_ipi`, `send_startup_ipi`, `send_ipi`), LVT configuration, timer init, present detection.

### Gaps vs AROS/AmigaOS
- **AROS**: HIDD (Hardware Independent Driver Interface) framework, PCI/USB bus enumeration, `device_model` for each hardware class. Driver model with `open/close/read/write/ioctl`. Unified device tree enumeration.
- **AmigaOS 3.x**: `exec.device` devices (`console.device`, `keyboard.device`, `mouse.device`, `disk.device`, `fd.device`, `trackdisk.device`, `audio.device`). Drivers are library-based with function vector tables.
- **VelaOS Gaps**:
  - **No device enumeration framework**: No PCI bus scan, no USB enumeration, no ACPI enumeration beyond MADT reading. Drivers are manually registered, not auto-discovered.
  - **No DMA API**: No `dma_map`, `dma_unmap`, cache-sync primitives.
  - **No HIDD equivalent**: No input event pipeline, no commodity library.
  - **No audio pipeline**: `audio.md` exists as design but no code.
  - **No keyboard/mouse class drivers**: The shell reads `getchar()` from the console, but there is no `input.device` abstraction.

### What Must Be Built
- PCI bus enumeration and driver attachment (DMA API, MSI-X).
- USB host controller driver (xHCI).
- `input.device` with `keyboard.device` and `mouse.device`.
- `audio.device` with mixing and DMA.
- DMA API with cache-coherency operations.

---

## 6. Graphics and Desktop

### Current Implementation
- **VGA driver**: Registered, simple framebuffer-like support.
- **Splash screen**: `boot/uefi-loader/src/splash.rs` draws into the GOP framebuffer.
- **Shell**: Dynamic prompt `hostname@user> cwd`, `cmd_clear` using form feed + newlines (works over serial).
- **Commands**: `clear`, `help`, `exit`, `pwd`, `ls`, `cd`, `echo`, `cat`, `whoami`, `hostname`, `ver`, `ping`, `ifconfig`, `netstat`.
- **Syscalls 31 (`CLEAR_SCREEN`) and 32 (`GET_HOSTNAME`)** wired in kernel, dispatch, and SDK with parity tests.
- **No graphics library**: `graphics.library` syscalls (64–75) exist in SDK numbers but no kernel implementation.
- **No intuition.screen/window**: No `intuition.library` implementation.
- **No layers/raster**: No BitMap, RastPort, Layer composition.

### Gaps vs AROS/AmigaOS
- **AROS**: Full `graphics.library` (RastPort, BitMap, ViewPort, Pen, Line, Rect, Text), `layers.library` (Layer creation, composition, z-ordering), `intuition.library` (Screens, Windows, Gadgets, IDCMP), BOOPSI object model. GOP/VBE framebuffer initialization.
- **AmigaOS 3.x**: `graphics.library` (BitMap, RastPort, Pen, Line, Rect, Circle, Text), `layers.library` (Layer, compositing, Bobs), `intuition.library` (Screen, Window, Gadget, IDCMP), sprites support. Agnus/Denise/Alice custom chips.
- **VelaOS Gaps**:
  - **No framebuffer pipeline**: The GOP framebuffer physical address is passed to the kernel via `BootInfo`, but there is no `graphics.library` implementation to map it, handle clipping, or support double-buffering.
  - **No palette/colormap management**: Color registers are not virtualized.
  - **No text rendering**: No font rasterization hooked into the graphics library (the font engine exists in `graphics/font` but is not wired).
  - **No window system**: No screen/windows/gadgets framework.
  - **No double-buffering or page flipping**: Critical for video output.

### What Must Be Built
- `graphics.library` core: `G_ALLOC_PEN`, `G_SET_PEN`, `G_RECT`, `G_FILL_RECT`, `G_TEXT`, `G_LINE`, `G_MOVE`, `G_SET_CLIP`, `G_PEN_COLOR`, `G_WRITE_PIXEL`.
- Framebuffer mapping from physical to virtual.
- Double-buffered software raster.
- Basic `intuition.library`: `I_OPEN_SCREEN`, `I_OPEN_WINDOW`, `I_SET_ATTRS`, `I_SET_TITLE`, `I_REFRESH`.
- Color palette management.

---

## 7. Audio

### Current Implementation
- **None**. No audio device, no mixer, no DMA audio path. The `audio.md` report exists as a design document only.

### Gaps vs AROS/AmigaOS
- **AROS**: `audio.device` with `ADC`, `DAC`, `write`/`read` sample IO, DMA channel allocation.
- **AmigaOS 3.x**: `audio.device` with four hardware sound channels, volume, period, frequency control.
- **VelaOS Gaps**: Complete absence of audio. No `audio.device`, no mixer, no DMA audio path, no syscalls for audio.

### What Must Be Built
- `audio.device` with DMA and channel allocation.
- Mixer and sample IO syscalls.

---

## 8. Summary of Critical Blockers to v0.2.0

| Subsystem | Status | Blocking Issue |
|-----------|--------|----------------|
| Memory management (user VM) | Partial | No per-process page tables; `brk`/`mmap` not fully wired to frame allocator |
| SMP | Compiles, boot works | AP bring-up not runtime-verified (QEMU MADT reports 1 CPU); no AP task scheduling |
| Filesystem | Design only | No RAMFS or block device drivers; all file syscalls fail |
| Network stack | None | Complete stub; `ping`/`ifconfig`/`netstat` report unavailable |
| Input drivers | Minimal | No `input.device`, no keyboard/mouse class drivers |
| Graphics | Minimal | No `graphics.library`, no framebuffer pipeline, no intuition |
| Audio | None | `audio.md` exists as design only |

### Architectural Philosophy Notes

1. **No CMake, no shared-core, no CMakeLists** — every crate is built directly via `xtask`, and the build system is Rust-native.
2. **UEFI 2.11 + custom Boot Protocol** — the loader (`boot/uefi-loader`) uses UEFI `GraphicsOutput` + `SimpleTextInputEx` + its own `vela.toml` configuration format. No Multiboot2.
3. **Pure Rust, `no_std` + `extern crate alloc`** for driver crates and the kernel.
4. **Architecture facade (`vela-arch`)** — kernel code imports hardware operations through the facade, not a concrete arch crate. The facade selects the implementation per target (`arch/x86_64` for x86-64; placeholder `arch/aarch64`, `arch/riscv64`, `arch/loongarch64` for compile-check).
5. **Higher-half kernel** — kernel image mapped at `0xFFFF_8000_0000_0000`, low 128 TiB for user processes.
6. **SMP uses real trampoline IPIs** — INIT/SIPI sequence is architecturally correct for x86-64, but "real multicore" is compile-verified; runtime multi-core verification depends on firmware MADT reporting >1 CPU.
7. **Amiga-inspired API surface** — syscall numbers, `ERROR_*` codes, `MEMF_*` flags, `IORequest` state machine, and `dos.library` operations modeled on AmigaOS 3.1, implemented in Rust with no 68000 compatibility shims.
8. **Four-architecture support via `xtask::Arch`** — X86_64 (full), Aarch64/Loongarch64/Riscv64 compile-check only.

### Recommendations for v0.2.0

1. **Priority 1**: Implement RAMFS (minimum viable filesystem) so `read_dir`, `open`, `write`, `seek` actually work. This unblocks the shell and conformance tests.
2. **Priority 2**: Wire `brk`/`mmap` syscalls to the frame allocator through the page mapper, enabling user processes to allocate heap memory.
3. **Priority 3**: Implement a `virtio-net` or simple PCI Ethernet driver + BSD socket API so `ping`, `ifconfig`, `netstat` function.
4. **Priority 4**: Add `graphics.library` core functions mapped to the GOP framebuffer, enabling basic drawing and text output.
5. **Priority 5**: Verify SMP on multi-core firmware; add per-CPU task dispatch so APs run tasks.
6. **Design**: Finalize RAMFS layout (inode structure, directory entries), block device interface, and network protocol suite before writing code.

---

*Analysis for /home/mobtgzhang/VelaOS/reports/issues-001.md*
*Updated: 2026-11-08*
