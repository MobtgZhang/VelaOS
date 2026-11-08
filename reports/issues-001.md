# VelaOS Subsystem Analysis: Gaps vs AROS/AmigaOS Design

## Executive Summary

VelaOS is an Amiga-inspired OS written in pure Rust. It targets x86-64 with a
UEFI 2.11 boot path and a custom unified boot protocol; aarch64, riscv64 and
loongarch64 are compile-checked only. It deliberately avoids copying AROS or
AmigaOS source: the API surface is Amiga-shaped, but the implementation is new.

The subsystems now exist in a functional first-generation form. Memory and page
tables, SMP AP bring-up, a RAM filesystem, a read-only FAT reader, Ethernet /
IPv4 / UDP / ARP processing, BSD-style socket syscalls, DMA allocation, PCI
enumeration, a graphics/layers/intuition/workbench stack, and an audio mixer are
all present. What remains is mostly **runtime verification, real hardware
backends, and missing protocol layers** (TCP, real virtqueues, reclaiming DMA,
audio syscalls, real AMD/Intel/Nouveau and HDA/AC97 drivers).

**Validation status:** `cargo build`, `cargo test --lib`, `cargo test --workspace`
(including the boot smoke suite), `cargo fmt --all -- --check`, and
`cargo clippy --workspace --all-targets -- -D warnings` pass. The boot smoke test
boots the UEFI loader, enters the kernel, selects the desktop by default, reaches
user mode, starts `init`, and brings up the Workbench. The keyboard round trip
explicitly selects the terminal entry and verifies `ver` through the shell.

One important runtime limit remains: QEMU/OVMF reports only one Local APIC entry
in its MADT even when `-smp 2` is requested, so AP bring-up and real multicore
task dispatch are compile-verified and architecturally implemented but not
runtime-verified on this host.

---

## 1. Memory Management and Page Tables

### Current Implementation

- **Frame allocator**: `kernel/memory/src/lib.rs` has a bitmap-based
  `FrameAllocator` with `alloc_frame`, `free_frame`, `reserve_range`, a rotating
  allocation hint, free-frame accounting, and double-free tolerance.
- **Boot memory plan**: `MemoryPlan` carves the bitmap and kernel heap out of the
  firmware memory map and reserves the null page, kernel image, initrd,
  framebuffer and MMIO windows. `BootReservations` lets the loader/kernel reserve
  specific regions.
- **Kernel heap**: `KernelHeap` is a bump allocator over the loader-reserved heap
  region, with alignment handling and exhaustion detection. It is the kernel's
  global allocator.
- **Page tables**: `arch/x86_64/src/addrspace.rs` implements the real x86-64
  `AddressSpace` (PML4, page-directory and page-table mapping, unmapping,
  translation, large-page support and teardown). `kernel/memory/src/lib.rs` adds
  a `PageMapper` facade over it.
- **Architecture-neutral contract**: `arch/common/src/lib.rs` defines
  `PAGE_SIZE`, `PageFlags`, `AddressSpaceId`, `MapError`, the `CpuArch` trait and
  the `VirtualMemory` trait. The kernel talks to the hardware through the
  `vela-arch` facade, not a concrete architecture crate.
- **Per-process address spaces**: `kernel/syscall/src/task_space.rs` implements
  `TaskSpace` with an image base, program break, user stack, memory pools and a
  `FrameSource`-backed `brk`/`alloc_mem`/`free_mem`/pool API. `SpaceRegistry`
  stores address spaces by `TaskId`. `kernel/process/src/lib.rs` creates one
  `TaskSpace` per `Process` and loads ELF images into it.
- **Higher-half split**: the kernel image is mapped at
  `0xFFFF_8000_0000_0000`; user space lives in the low address range. The boot
  loader passes a populated PML4 to the kernel through `VelaBootInfo`.

### Gaps vs AROS/AmigaOS

- **AROS** has a full virtual-memory system with pagelists, memory protection,
  swap support and per-address-space lifecycle.
- **AmigaOS 3.x** uses a flat 24-bit physical address space with `MEMF_*` memory
  classes and no paging.
- **VelaOS gaps**:
  - **CR3 switching is not wired into the scheduler's context switch**. Tasks
    record a `cr3`, and `TaskSpace` owns a real address space, but the scheduler
    path does not yet switch CR3 per task.
  - **The frame allocator does not reclaim**. `dma_free` is a no-op and the
    kernel heap is bump-only, so "free" means "not reused".
  - **No swap or pagelist model**.
  - **`mmap`/`munmap` are not first-class syscalls**; the page mapper and `brk`
    infrastructure exist, but the full user-VM surface is not exposed.
  - **No per-process quota or memory-pressure accounting**.

### What Must Be Built

- Wire CR3 switching into the scheduler and process switch path.
- Add a reclaiming frame allocator or an explicit `dma_free` policy.
- Expose `mmap`/`munmap` and a user-VM API over `TaskSpace`.
- Add swap/pagelist support if VelaOS is to match AROS's VM model.

---

## 2. SMP (Multicore / Multithread Management)

### Current Implementation

- **Trampoline and AP bring-up**: `arch/x86_64/src/smp.rs` contains the real
  x86-64 trampoline at `0x7000`, the per-AP configuration table at
  `0x8000`–`0x9FC0`, the INIT / INIT-deassert / SIPI IPI sequence, and the
  `ap_entry` long-mode entry point.
- **Per-AP boot**: each AP installs the GDT and IDT under `AP_INIT_LOCK`,
  initializes its APIC, registers its CPU index, marks itself ready, signals the
  BSP with an atomic ready count, then idles with `sti` / `hlt`.
- **APIC**: `arch/x86_64/src/apic.rs` implements IPI sending, LVT programming,
  the periodic timer, tick counting and a reschedule IPI vector.
- **Scheduler SMP layer**: `kernel/scheduler/src/smp.rs` has per-CPU run queues
  (`RUN_QUEUES`), `init_this_cpu`, `enqueue_on`, `pick_next_on`,
  `peek_next_on`, `remove_from_queue`, reschedule flags and
  `with_preemption_disabled`.
- **Task model**: `kernel/scheduler/src/task.rs` stores a per-task `cr3`, user
  stack, kernel stack, priority and run/preemption counters.
- **MADT parsing**: `boot/uefi-loader/src/main.rs` reads the RSDP, follows it to
  the XSDT or RSDT, walks the ACPI table entries and counts type-0 Local APIC
  entries. The loader now uses unaligned reads for the RSDP and table entries,
  which fixes the previous `misaligned pointer dereference` panic.

### Gaps vs AROS/AmigaOS

- **AROS** has SMP-safe kernel locks, per-CPU arrays and inter-processor
  scheduling. **AmigaOS 4.x** has PPC multiprocessing, but classic AmigaOS is
  essentially single-core.
- **VelaOS gaps**:
  - **AP bring-up is not runtime-verified** in this QEMU/OVMF environment. The
    firmware MADT reports one Local APIC entry even with `-smp 2`.
  - **APs do not join the scheduler's run loop**. `ap_entry` spins in an idle
    `hlt` loop; per-CPU queues exist but no AP picks a task from them.
  - **Reschedule handling is defined but not fully wired** into the AP idle loop.
  - **No per-CPU cache-affinity policy or shared-data locking beyond boot-time
    `AP_INIT_LOCK`.**

### What Must Be Built

- Verify AP bring-up on firmware whose MADT reports more than one CPU.
- Wire the per-CPU run queues into the AP idle/context-switch loop.
- Add real reschedule IPI consumption on APs.
- Expand per-CPU state (current task, timer, stack and APIC state).

---

## 3. Filesystems and Storage

### Current Implementation

- **VFS contract**: `kernel/vfs/src/lib.rs` defines `FileSystem` (lookup,
  `read_dir`, `read_all`, `read_at`, `write_all`, `write_at`, `create_dir`,
  `remove`, `rename`, `set_comment`, size queries), `Mount` and `MountTable`.
- **RAM filesystem**: `fs/ramfs/src/lib.rs` implements a real in-memory
  filesystem. It is mounted by the kernel at `/ramfs`; `init` populates `RAM:`
  and `SYS:` trees on top of it.
- **FAT reader**: `fs/fat/src/lib.rs` implements a read-only FAT12/16/32 reader
  over a `BlockDevice` trait. It supports root open, path open, directory
  listing and file reading.
- **DOS service**: `kernel/dos/src/lib.rs` provides `Bptr` file handles,
  path resolution through `vela-amiga-path`, open/close/read/write/seek,
  examine, `read_dir`, create/delete/rename, current-directory handling and
  segment loading (`load_seg` / `unload_seg`).
- **Block device layer**: `kernel/drivers/storage/src/lib.rs` defines a
  `BlockDevice` with sector count, sector size and read-only flag, plus
  `create_ramdisk`, `read_sectors` and `write_sectors`.
- **Storage drivers**: AHCI/SATA, NVMe and ATA drivers are registered with the
  Device Manager. Their discovery logic is present; the actual command
  submission paths are stubs.
- **Mounted boot state**: the kernel mounts RAMFS for `RAM:` and `SYS:` during
  boot, and `init` reports `RAM:` / `SYS:` population counts on the serial log.

### Gaps vs AROS/AmigaOS

- **AROS** has a full DOS library with `Assign`, `Volume`, `Examine`, `Open`,
  `Read`, `Write`, `Seek`, `DeleteFile`, and multiple real filesystems. It also
  supports POSIX-style paths and block devices.
- **AmigaOS 3.x** has `dos.library` with `BPTR` paths, volumes and assigns, and
  device-based filesystems.
- **VelaOS gaps**:
  - **FAT is read-only**. There is no FAT writer, no allocation of clusters and
    no mount of a real FAT disk at boot.
  - **AHCI/NVMe/ATA are stubs**. The block-device abstraction exists, but the
    hardware command paths do not yet read or write real disks.
  - **No EXT2/3 or other native filesystem**.
  - **No journaling or crash consistency**.
  - **RAMFS is memory-only**; it does not persist across boots.
  - **`assign`/volume support exists at the path layer**, but the DOS service is
    still a first-generation subset of the full AmigaDOS surface.

### What Must Be Built

- A writable FAT or native filesystem over a real block device.
- Real AHCI/NVMe/ATA command submission.
- Persistent-disk boot images and mount of disk-backed volumes.
- Journaling / crash-consistency policy.
- Full DOS assign/volume behaviour and `Examine` semantics.

---

## 4. Network Stack

### Current Implementation

- **Protocol processing**: `kernel/net/src/lib.rs` implements Ethernet framing,
  IPv4 header parsing/building with checksum verification, UDP datagrams with
  the pseudo-header checksum, and ARP request/reply parsing and building. It
  takes a `NetDevice` and transmits completed frames.
- **BSD-style sockets**: `kernel/socket/src/lib.rs` implements `socket`, `bind`,
  `connect`, `listen`, `accept`, `sendto`, `recvfrom`, `close` and
  `setsockopt` with `AF_INET`, `SOCK_STREAM`, `SOCK_DGRAM`, `IPPROTO_UDP` and
  `IPPROTO_TCP` constants, error codes and a socket registry. `deliver_datagram`
  feeds received UDP payloads to bound sockets.
- **Syscall group**: `kernel/syscall/src/numbers.rs` defines `NET_SOCKET` through
  `NET_SETSOCKOPT` (33–41); `kernel/syscall/src/dispatch.rs` installs all nine
  handlers.
- **Driver path**: `drivers/virtio-net/src/lib.rs` performs real PCI discovery
  for virtio vendor `0x1AF4` and modern/legacy virtio-net device IDs. It accepts
  frames into a transmit queue and implements `NetDevice`.
- **Other NICs**: `kernel/drivers/network/src/lib.rs` registers e1000 and RTL8139
  probe stubs; the primary virtio device is installed as the global transmit
  endpoint when present.
- **Shell commands**: `ping`, `ifconfig` and `netstat` are still reported as
  stubs in the terminal, even though the kernel network services and syscalls
  exist.

### Gaps vs AROS/AmigaOS

- **AROS** has a BSD-socket-compatible stack, an `inet` module, `netif`,
  Ethernet drivers, ARP and DNS (`netdb`).
- **AmigaOS 3.x** has a `bsdsocket`-compatible TCP/IP stack with ICMP, UDP, TCP,
  ARP and DNS.
- **VelaOS gaps**:
  - **TCP is not implemented**. `connect`/`accept`/`listen` on `SOCK_STREAM`
    return `EOPNOTSUPP` or `EWOULDBLOCK` rather than performing a handshake.
  - **ICMP is not implemented**, so there is no real `ping`.
  - **No DNS resolver** (`gethostbyname`).
  - **No interface configuration or routing table** for `ifconfig`/`netstat`.
  - **virtio-net has no virtqueue ring**; frames are queued in memory rather
    than transmitted through a real virtqueue.
  - **No e1000/RTL8139 hardware implementation** beyond registration/probes.
  - **No DHCP or network boot configuration**.

### What Must Be Built

- TCP handshake and connection state machine.
- ICMP echo/reply for `ping`.
- DNS resolver and an interface/route table.
- Real virtqueue setup and DMA-backed NIC transmission.
- Real e1000/RTL8139 backends or removal of their stub registrations.
- Shell commands that use the socket API instead of reporting a stub.

---

## 5. I/O and Driver Management

### Current Implementation

- **PCI enumeration**: `arch/x86_64/src/pci.rs` performs real PCI configuration
  space access through ports `0xCF8` / `0xCFC`, returning bus/device/function,
  vendor ID, device ID, class and subclass. The boot log reports enumerated PCI
  devices.
- **Device Manager**: `kernel/device/src/lib.rs` registers drivers and device
  instances, probes by vendor/class, and dispatches `OpenDevice`, `CloseDevice`,
  `SendIO`, `DoIO`, `WaitIO` and `AbortIO`.
- **IORequest state machine**: full `New → Opened → Submitted → Done` lifecycle
  with cancellation, reuse and error state.
- **DMA API**: `kernel/dma/src/lib.rs` provides `dma_alloc`, `dma_free`,
  `dma_map`, `dma_unmap`, `dma_sync_for_device` and `dma_sync_for_cpu` over a
  `FrameSource`. On the current x86-64 build the device view is identity-mapped
  RAM and cache sync is a no-op because x86-64 is coherent for normal RAM.
- **Storage**: AHCI/SATA, NVMe and ATA drivers register; the `BlockDevice`
  abstraction and RAM disk support real sector reads/writes.
- **Graphics**: VGA/VESA framebuffer driver plus AMD, Intel and Nouveau/NVIDIA
  probe stubs registered with the Device Manager.
- **Audio**: HDA, AC97 and a null audio backend registered; the null backend
  accepts PCM and the HDA/AC97 paths are stubs.
- **Network**: virtio-net PCI discovery plus e1000/RTL8139 stubs.
- **Input**: PS/2 port access lives in `arch/x86_64/src/ps2.rs`; the
  architecture-neutral `kernel/input` crate decodes keyboard and mouse byte
  streams. The boot menu polls the PS/2 keyboard and COM1.

### Gaps vs AROS/AmigaOS

- **AROS** has the HIDD hardware-independent driver interface, PCI/USB
  enumeration and a unified device tree.
- **AmigaOS 3.x** has library-based devices (`console.device`, `keyboard.device`,
  `mouse.device`, `audio.device`, `disk.device`).
- **VelaOS gaps**:
  - **AMD, Intel and Nouveau graphics are probe stubs**, not real GPU drivers.
  - **HDA and AC97 are stubs**; there is no real audio hardware path.
  - **e1000 and RTL8139 are stubs**; only virtio-net discovery is real.
  - **AHCI/NVMe/ATA do not submit real commands**.
  - **`dma_free` is a no-op** while the frame allocator is bump-style.
  - **No USB/xHCI support**.
  - **No MSI-X or interrupt routing per device**.
  - **No real `input.device` / keyboard.device / mouse.device class drivers**.

### What Must Be Built

- Real AMD/Intel/Nouveau GPU initialization and framebuffer mode setting.
- Real HDA/AC97 codec and DMA audio paths.
- Real e1000/RTL8139 transmit/receive rings.
- Real AHCI/NVMe/ATA command queues and interrupt handling.
- Reclaiming DMA memory and IOMMU mapping where available.
- USB/xHCI and a proper `input.device` event pipeline.

---

## 6. Graphics and Desktop

### Current Implementation

- **Graphics service**: `kernel/graphics/src/lib.rs` implements palettes,
  pens, raster registration, `RastPort` drawing (`put_pixel`, `line`,
  `fill_rect`, `stroke_rect`, `text`, clip and mask handling) and error codes.
- **Layers/compositor**: `kernel/layers/src/lib.rs` implements layer creation,
  destruction, clipping, front/back ordering, visibility, movement, damage
  tracking and compositing into a target buffer.
- **Intuition/window manager**: `kernel/intuition/src/lib.rs` implements
  screens, windows, titles, bounds, attributes, gadgets, IDCMP-style events,
  active-window selection, repaint and framebuffer compositing.
- **Workbench**: `kernel/workbench/src/lib.rs` implements application icons,
  menus, windows, drop zones and an object registry.
- **BOOPSI**: `kernel/gui/src/boopsi.rs` implements class IDs, method IDs,
  object lookup and a registry as the Amiga-style object model.
- **Facade**: `kernel/gui/src/lib.rs` re-exports `graphics`, `layers`,
  `intuition` and `workbench` rather than duplicating them.
- **Syscalls**: graphics syscalls 64–76, layers 128–137, intuition 192–204 and
  workbench 448–469 are installed in the dispatch table.
- **Boot display**: the loader passes GOP framebuffer geometry and physical
  address in `VelaBootInfo`; `kernel/entry` maps the framebuffer, installs the
  boot console and creates the Graphics, Intuition and Workbench services.
- **Desktop boot**: `init` reads the boot choice and starts `vela_workbench::run`
  for the desktop. The serial log says `workbench: ready`.
- **Terminal**: the shell uses SDK graphics `text` and `write_pixel` calls to
  render its scrollback and cursor; `clear` is implemented.

### Gaps vs AROS/AmigaOS

- **AROS** has a full `graphics.library`, `layers.library`, `intuition.library`,
  BOOPSI and hardware framebuffer initialization.
- **AmigaOS 3.x** has `graphics.library`, `layers.library`, `intuition.library`
  and custom-chip sprite/display hardware.
- **VelaOS gaps**:
  - **AMD/Intel/Nouveau graphics paths are stubs**; the real display is the
    firmware GOP framebuffer through the VGA driver.
  - **No hardware acceleration or page flipping**; compositing is software.
  - **No sprite support**.
  - **No full font rasterization pipeline** in the graphics library; the font
    crate exists but is not a complete graphics.library text stack.
  - **Palette/color-register virtualization is limited** to the service's
    palette table, not hardware color registers.
  - **No full IDCMP message delivery to user programs** is wired through the
    syscall surface; events are modelled in Intuition.

### What Must Be Built

- Real GPU drivers and mode setting for AMD/Intel/Nouveau.
- Hardware double-buffering / page flipping.
- Sprite support and hardware color registers.
- A complete graphics.library text/font path.
- Full IDCMP event delivery through user-space syscalls.

---

## 7. Audio

### Current Implementation

- **audio.device-style service**: `kernel/audio/src/lib.rs` implements
  `AudioDevice` and `Mixer` with channel allocation, channel free, volume,
  PCM `SampleFrame` writes, start/stop, mixing, clipping counters and sample
  rate.
- **Mixer backend**: `kernel/drivers/audio/src/lib.rs` implements
  `AudioMixer` with PCM streams, per-stream volume (Q16), pause/resume, mixing
  and clipping, plus a null backend that accepts PCM without hardware.
- **Hardware stubs**: HDA (Intel HD Audio) and AC97 drivers register as
  multimedia devices; they are probe/registration stubs, not codec drivers.
- **Boot registration**: `kernel/entry` registers audio drivers and installs
  the mixer.

### Gaps vs AROS/AmigaOS

- **AROS** has `audio.device` with ADC/DAC sample IO and DMA channel
  allocation.
- **AmigaOS 3.x** has `audio.device` with four hardware sound channels,
  volume, period and frequency control.
- **VelaOS gaps**:
  - **No audio syscalls** are defined in `kernel/syscall/src/numbers.rs`; user
    programs cannot open or write the audio device through the SDK.
  - **HDA/AC97 are stubs**; there is no real codec initialization, DMA ring or
    hardware playback.
  - **No audio DMA path** is wired from the mixer to a device.
  - **No sample-format conversion** beyond the mixer's `i16` stereo frames.

### What Must Be Built

- Audio syscalls (`open`, `write`, `close`, volume, channel allocation).
- Real HDA/AC97 codec initialization and DMA playback.
- Format conversion and a user-space audio API.
- A real audio device pipeline in the desktop.

---

## 8. Summary of Remaining Blockers and Status

| Subsystem | Status | Remaining blocker |
|---|---|---|
| Memory / page tables | Functional first generation | CR3 switching not wired; no reclaiming allocator; no `mmap`/swap surface |
| SMP | Implemented, compile-verified | AP bring-up not runtime-verified (QEMU MADT reports 1 CPU); APs do not run scheduler tasks |
| Filesystems | RAMFS + read-only FAT | FAT not writable; AHCI/NVMe/ATA command paths are stubs; no persistent disk mount |
| Network | Ethernet/IPv4/UDP/ARP + sockets | TCP/ICMP/DNS absent; virtio-net has no virtqueue ring; e1000/RTL8139 are stubs; shell commands remain stubs |
| I/O and drivers | PCI, Device Manager, DMA API, driver registration | AMD/Intel/Nouveau, HDA/AC97, e1000/RTL8139 and AHCI/NVMe/ATA are stubs; no USB; `dma_free` no-op |
| Graphics | Graphics/layers/intuition/workbench + BOOPSI | Real GPU drivers absent; no hardware acceleration, sprites or full font pipeline |
| Audio | Mixer and audio.device service | No audio syscalls; HDA/AC97 and DMA audio path are stubs |
| Boot / desktop | Works | Boot menu terminal-vs-desktop, default desktop, boot-choice syscall `GET_BOOT_CHOICE = 42`, boot ABI version 4 |
| Shell | Works | `clear`, dynamic prompt, `ver`; network commands are explicit stubs |

### Architectural Philosophy Notes

1. **No CMake, no shared-core, no Multiboot2** — the build is Rust-native and the
   boot path is UEFI 2.11 plus a custom unified boot protocol.
2. **Architecture-neutral kernel APIs** — hardware operations go through the
   `vela-arch` facade; `arch/*` holds architecture-specific code.
3. **Pure Rust, `no_std` + `alloc`** for the kernel and drivers.
4. **Higher-half kernel** — kernel at `0xFFFF_8000_0000_0000`; low address range
   for user programs.
5. **Real SMP trampoline IPIs** — INIT/deassert/SIPI is implemented, but runtime
   multicore scheduling remains unverified in this QEMU/OVMF environment.
6. **Amiga-inspired API surface** — syscall numbers, error codes, `IORequest`
   state machine, DOS operations and BOOPSI are modelled on AmigaOS/AROS but
   implemented independently in Rust.
7. **Four target architectures** — x86-64 is the boot target; aarch64, riscv64
   and loongarch64 are compile-checked only.

### Recommendations for v0.2.0

1. Wire CR3 switching and reclaiming memory so user processes are isolated and
   memory can be reused.
2. Implement TCP/ICMP/DNS and real virtqueues so networking is not just a
   registration path.
3. Implement real AHCI/NVMe/ATA command submission and a writable FAT or native
   filesystem.
4. Add audio syscalls and real HDA/AC97 DMA playback.
5. Verify AP bring-up on firmware that reports more than one CPU and connect APs
   to the per-CPU scheduler queues.
6. Replace AMD/Intel/Nouveau and e1000/RTL8139 stubs with real drivers where the
   hardware is available.
7. Add USB/xHCI and a proper `input.device` event pipeline.

---

*Analysis for /home/mobtgzhang/VelaOS/reports/issues-001.md*
*Updated: 2026-11-09*
