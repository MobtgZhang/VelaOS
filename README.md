# VelaOS

**A Classic Spirit. A Modern Core.**

VelaOS is an experimental, independent operating system project written primarily in Rust. It aims to bring the programming model and desktop experience of **AmigaOS 3.1** to modern 64-bit platforms, with a clean kernel architecture, UEFI boot support, and a carefully designed compatibility layer.

> **Project status:** Early planning and development. Features described below are goals, not claims of current implementation.

## Goals

- Build an independent, primarily Rust-based `no_std` kernel.
- Support **x86-64, AArch64, RISC-V64, and LoongArch64**.
- Boot through UEFI, using architecture-specific loaders where necessary.
- Implement compatible interfaces for key **AmigaOS 3.1 APIs**.
- Provide a classic Workbench-inspired graphical desktop.
- Support native 64-bit Rust and C applications.
- Eventually run original Motorola 68k applications through an isolated emulation and ABI-translation environment.

## Architecture

```text
                 UEFI Firmware
                       |
                VelaOS Bootloader
                       |
                   Vela Kernel
         +-------------+-------------+
         |             |             |
    Memory / IPC   Scheduler     VFS / Drivers
         +-------------+-------------+
                       |
              Hardware Abstraction
          x86-64 / AArch64 / RISC-V64
                  / LoongArch64

         AmigaOS 3.1 API Compatibility
          Exec / DOS / Graphics /
           Layers / Intuition / ...
                 /          \
       Native Applications   68k Runtime
                 \          /
              Vela Workbench
```

The design separates the native 64-bit ABI from the original 32-bit, big-endian Motorola 68k ABI. Shared system services provide the underlying functionality, while explicit adapters handle calling conventions, pointer representations, structure layouts, and guest memory.

## Target Platforms

| Architecture | Kernel target (planned) | UEFI strategy |
| --- | --- | --- |
| x86-64 | `x86_64-unknown-none` | Rust `x86_64-unknown-uefi` |
| AArch64 | `aarch64-unknown-none` | Rust `aarch64-unknown-uefi` |
| RISC-V64 | `riscv64gc-unknown-none-elf` | Custom/ported UEFI target and firmware integration |
| LoongArch64 | `loongarch64-unknown-none` | Custom/ported UEFI target and firmware integration |

Support for a UEFI architecture does not automatically imply that a ready-to-use Rust UEFI compilation target, firmware image, or device driver exists.

## AmigaOS Compatibility

VelaOS targets **AmigaOS 3.1 API compatibility**, beginning with:

| Library | Planned functionality |
| --- | --- |
| `exec.library` | Tasks, signals, memory, message ports, libraries |
| `dos.library` | Files, directories, processes, assigns, program loading |
| `utility.library` | Tags, hooks, and common utilities |
| `graphics.library` | Bitmaps, drawing, fonts, RastPorts |
| `layers.library` | Clipping and layered windows |
| `intuition.library` | Screens, windows, gadgets, IDCMP events |
| `icon.library` / `workbench.library` | Icons, ToolTypes, desktop integration |

**Compatibility has multiple levels:** API compatibility, source-level portability, original 68k binary execution, and full classic Amiga hardware emulation. These are distinct milestones. VelaOS does **not** currently claim complete compatibility at any of these levels.

### Motorola 68k Applications

The planned compatibility runtime will include:

1. An Amiga HUNK executable loader and relocator.
2. A Motorola 68k interpreter or appropriately licensed execution backend.
3. A validated 32-bit, big-endian guest memory model.
4. Library Vector Offset (LVO) and register-based call dispatch.
5. Translation between guest ABI values and native VelaOS services.

Programs that directly access original Amiga custom chips may require additional hardware emulation.

## Boot Process

```text
UEFI firmware
  -> VelaOS EFI loader
  -> Classic-inspired boot splash
  -> Load architecture-specific kernel ELF and initrd
  -> Collect memory map, framebuffer, ACPI/DTB information
  -> ExitBootServices
  -> Enter Vela Kernel
  -> Initialize memory, interrupts, scheduler, drivers and VFS
  -> Start system services
  -> Launch Vela Workbench
```

The bootloader and kernel are separate binaries. After exiting UEFI Boot Services, the kernel must provide its own runtime facilities and drivers.

## Repository Layout

The proposed Cargo workspace is organized as follows:

```text
velaos/
├── boot/             # UEFI loader, ELF loader, boot protocol
├── kernel/           # Core, memory, scheduler, IPC, syscalls, VFS
├── hal/              # Architecture-specific implementations
├── drivers/          # Framebuffer, serial, input, block, VirtIO
├── amiga/            # AmigaOS API specs, ABIs and libraries
├── compatibility/    # 68k CPU, HUNK loader, guest memory, LVO bridge
├── graphics/         # 2D renderer, fonts, compositor, classic theme
├── userland/         # Init, shell, Workbench and utilities
├── fs/               # RAMFS, FAT, AmigaDOS path translation
├── sdk/              # Rust/C APIs, headers and examples
├── platforms/        # Hosted testing environment
├── tools/            # Build and image tooling
├── tests/            # ABI, API, boot and integration tests
└── docs/             # Architecture and implementation documents
```

This layout is a **proposed structure** and may evolve as development progresses.

## Filesystem Design

VelaOS plans to use GPT with an EFI System Partition and a separate system volume. A compatibility layer will expose AmigaDOS-style paths and assigns, including:

| Path | Purpose |
| --- | --- |
| `SYS:` | System volume |
| `C:` | Commands |
| `S:` | Startup scripts |
| `LIBS:` | System libraries |
| `DEVS:` | Devices and configuration |
| `FONTS:` | Fonts |
| `RAM:` | Temporary RAM disk |
| `ENV:` / `ENVARC:` | Runtime and persistent settings |
| `PROGDIR:` | Current program directory |

The first implementation may use FAT and RAMFS, with more advanced filesystems introduced later.

## Development Roadmap

- [ ] **M0 — Foundation:** Cargo workspace, tooling, UEFI proof of concept.
- [ ] **M1 — Kernel Boot:** ELF loading, BootInfo, memory management, exceptions, serial logging.
- [ ] **M2 — Core APIs:** Exec tasks, signals, message ports, DOS and basic VFS.
- [ ] **M3 — Graphics:** Framebuffer renderer, Graphics, Layers, Intuition, IDCMP.
- [ ] **M4 — Desktop & SDK:** Workbench-inspired desktop and native Rust/C application SDK.
- [ ] **M5 — Multi-Architecture:** QEMU boot and API tests on all four target architectures.
- [ ] **M6 — 68k Compatibility:** HUNK loading, CPU execution, guest ABI and library bridging.
- [ ] **M7 — Expansion:** Additional drivers, SMP, networking, audio and broader compatibility.

### First Demonstration Target

Boot VelaOS in **QEMU with OVMF on x86-64**, display a classic-inspired desktop, open a window through the Amiga-compatible Intuition interface, and deliver a mouse event to the application through an Exec message port.

## Building

The build system and bootable images are not yet established. The intended developer workflow is:

```bash
# Planned commands; not yet implemented
cargo xtask build --arch x86_64 --profile dev
cargo xtask image --arch x86_64
cargo xtask qemu --arch x86_64
cargo xtask test --suite api-conformance
```

Once the repository is bootstrapped, this section will include tested toolchain versions, firmware requirements, and reproducible build instructions.

## Contributing

Contributions, design discussions, tests, and documentation improvements are welcome as the project develops. In particular, help with Rust kernel engineering, architecture ports, AmigaOS API conformance, 68k emulation, device drivers, and classic UI design will be valuable.

Please open an issue before beginning a large architectural change. Keep `unsafe` code minimal, document safety invariants, and include tests for ABI or compatibility-sensitive changes.

## License

**VelaOS is licensed under the GNU Lesser General Public License, version 2.1 (LGPL-2.1-only).**

See [`LICENSE`](LICENSE) for the complete license text. The `LICENSE` file must contain the full official GNU LGPL v2.1 license before the repository is published.

Third-party components, firmware, fonts, artwork, and emulator code may have separate licenses. All imported components must be reviewed for license compatibility and attribution requirements.

Official license text: https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html

## Disclaimer

VelaOS is an independent project inspired by classic Amiga systems. It is not affiliated with, endorsed by, or an official distribution of AmigaOS, AROS, or their respective rights holders. AmigaOS, Amiga, Workbench, and other names may be trademarks of their respective owners.

Original Kickstart ROMs, Workbench artwork, fonts, and other copyrighted assets are **not** included unless redistribution rights are explicitly secured.

---

**VelaOS — A Classic Spirit. A Modern Core.**
