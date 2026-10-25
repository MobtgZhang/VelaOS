# Third-party components and licences

> **Scope:** every external dependency that VelaOS links, in the kernel image
> or in the host-side tooling, together with the licence it is distributed
> under and where it is used.
>
> The list is derived from `Cargo.lock` and the workspace `Cargo.toml` at the
> time of writing, and it is regenerated with
> `cargo tree --workspace --edges normal,build --prefix none`.

---

## 1. VelaOS itself

The repository carries a single `LICENSE` file at the root: the **GNU Lesser
General Public License, version 2.1** (`LGPL-2.1-only`), 504 lines, in the
stock FSF text.

Note the discrepancy, because it is a real one and a distributor will hit
it: the workspace metadata in `Cargo.toml` declares

```toml
[workspace.package]
license = "MIT OR Apache-2.0"
```

while the repository's `LICENSE` file is LGPL-2.1. The two do not agree, and
a crate published from this metadata would carry a licence statement that
does not match the text in the repository. **This is reported rather than
fixed**, because the root `Cargo.toml` and the root `LICENSE` are outside
the ownership of the Phase 8 work. The owner should pick one:

* keep the metadata as `MIT OR Apache-2.0` and replace the `LICENSE` file
  with the Apache-2.0 text (or add a `LICENSE-MIT` alongside it), **or**
* change the metadata to `LGPL-2.1-only` and leave the file as it is.

Until then, this document describes the *dependencies*; it does not resolve
the project's own licence.

---

## 2. Direct dependencies

The workspace declares four external dependencies in `[workspace.dependencies]`:

| dependency | version | licence | used by |
|---|---|---|---|
| `spin` | 0.9.9 | MIT | the `no_std` crates, for `Once` and `SpinMutex` |
| `uefi` | 0.41.0 | MIT OR Apache-2.0 | `boot/uefi-loader` only |
| `log` | 0.4.34 | MIT OR Apache-2.0 | the logging facade used across the kernel |

and the host-side tooling and test crates add two more directly:

| dependency | version | licence | used by |
|---|---|---|---|
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 | `tools/xtask`, `tests/boot-smoke`, `tests/gui-golden` |
| `clap` | 4.6.7 | MIT OR Apache-2.0 | `tools/xtask` (the CLI parser) |
| `toml` | 0.8.23 | MIT OR Apache-2.0 | `tools/xtask` (the config reader) |

### 2.1 — Where `spin` is used

`spin` is the only external crate that ends up **inside the kernel image**.
It provides two things that a `no_std`, no-`std` kernel needs and that the
standard library does not offer in a `no_std` context:

* `spin::Once` — the one-time initialiser used for the kernel's singletons
  (the memory manager, the syscall dispatch table, the Exec registry);
* `spin::SpinMutex` — a simple spin lock used by the serial logger and by
  the shared kernel structures.

It is used in `kernel/core`, `kernel/memory`, `kernel/scheduler`,
`kernel/exec`, `kernel/dos`, `kernel/graphics`, `kernel/layers`,
`kernel/intuition`, `kernel/input`, `kernel/syscall`, `kernel/vfs`,
`hal/x86_64`, `fs/ramfs`, `graphics/text`, `sdk/vela-sys`,
`platforms/hosted` and `boot/uefi-loader`.

`spin` is **MIT licensed**, which is compatible with redistribution under
MIT, Apache-2.0 and LGPL-2.1.

### 2.2 — Where `uefi` is used

`uefi` is used by `boot/uefi-loader` and by **nothing else**. The loader is
a `no_std` UEFI application that:

* reads the configuration file from the boot volume;
* queries the Graphics Output Protocol for the framebuffer;
* loads the kernel ELF;
* calls `ExitBootServices`.

The `uefi` crate is `MIT OR Apache-2.0`, with the standard `LICENSE-MIT` and
`LICENSE-APACHE` files in the crate. It pulls in `uefi-raw` (MIT OR
Apache-2.0), `uefi-macros` (MIT OR Apache-2.0), `bitflags` (MIT OR
Apache-2.0), `bit_field` (Apache-2.0/MIT), `cfg-if` (MIT OR Apache-2.0),
`log` (MIT OR Apache-2.0), `ptr_meta` (MIT) and `ptr_meta_derive` (MIT).

The kernel proper does **not** depend on `uefi`: the loader hands the kernel
a plain structure (`VelaBootInfo`, see `docs/boot/boot-protocol-v1.md`) and
the kernel never sees a UEFI type. This is deliberate — it is what keeps
the kernel's dependency set down to `spin` and `log`.

### 2.3 — Where `log` is used

`log` is the logging *facade*. It has no back end of its own; the kernel
supplies one (`kernel/core`'s logger writes to the serial port and, in the
loader, to the UEFI debug console through `uefi`'s `logger` feature).

### 2.4 — Host-side dependencies

`anyhow`, `clap` and `toml` are used only by `tools/xtask` and the test
crates. They are **not** linked into the kernel or into any user-space
program; they are build-time and test-time tools. `clap` and `toml` pull in
`serde`, `serde_derive`, `serde_core`, `serde_spanned`, `toml_edit`,
`toml_datetime`, `toml_write`, `winnow`, `indexmap`, `hashbrown`,
`equivalent`, `memchr`, and the `proc-macro2`/`quote`/`syn`/`unicode-ident`
macro stack — all host-only.

---

## 3. The complete transitive set

Every package in `Cargo.lock` that comes from crates.io, with the licence
declared in its own `Cargo.toml`:

| package | version | licence |
|---|---|---|
| `anstream` | 1.0.0 | MIT OR Apache-2.0 |
| `anstyle` | 1.0.14 | MIT OR Apache-2.0 |
| `anstyle-parse` | 1.0.0 | MIT OR Apache-2.0 |
| `anstyle-query` | 1.1.5 | MIT OR Apache-2.0 |
| `anstyle-wincon` | 3.0.11 | MIT OR Apache-2.0 |
| `anyhow` | 1.0.104 | MIT OR Apache-2.0 |
| `bit_field` | 0.10.3 | Apache-2.0/MIT |
| `bitflags` | 2.13.2 | MIT OR Apache-2.0 |
| `cfg-if` | 1.0.5 | MIT OR Apache-2.0 |
| `clap` | 4.6.7 | MIT OR Apache-2.0 |
| `clap_builder` | 4.6.7 | MIT OR Apache-2.0 |
| `clap_derive` | 4.6.7 | MIT OR Apache-2.0 |
| `clap_lex` | 1.1.1 | MIT OR Apache-2.0 |
| `colorchoice` | 1.0.5 | MIT OR Apache-2.0 |
| `equivalent` | 1.0.3 | Apache-2.0 OR MIT |
| `hashbrown` | 0.17.1 | MIT OR Apache-2.0 |
| `heck` | 0.5.0 | MIT OR Apache-2.0 |
| `indexmap` | 2.14.2 | Apache-2.0 OR MIT |
| `is_terminal_polyfill` | 1.70.2 | MIT OR Apache-2.0 |
| `log` | 0.4.34 | MIT OR Apache-2.0 |
| `memchr` | 2.8.3 | Unlicense OR MIT |
| `once_cell_polyfill` | 1.70.2 | MIT OR Apache-2.0 |
| `proc-macro2` | 1.0.107 | MIT OR Apache-2.0 |
| `ptr_meta` | 0.3.2 | MIT |
| `ptr_meta_derive` | 0.3.2 | MIT |
| `quote` | 1.0.47 | MIT OR Apache-2.0 |
| `serde` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_core` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_derive` | 1.0.229 | MIT OR Apache-2.0 |
| `serde_spanned` | 0.6.9 | MIT OR Apache-2.0 |
| `spin` | 0.9.9 | MIT |
| `strsim` | 0.11.1 | MIT |
| `syn` | 3.0.9 | MIT OR Apache-2.0 |
| `toml` | 0.8.23 | MIT OR Apache-2.0 |
| `toml_datetime` | 0.6.11 | MIT OR Apache-2.0 |
| `toml_edit` | 0.22.27 | MIT OR Apache-2.0 |
| `toml_write` | 0.1.2 | MIT OR Apache-2.0 |
| `ucs2` | 0.3.3 | MPL-2.0 |
| `uefi` | 0.41.0 | MIT OR Apache-2.0 |
| `uefi-macros` | 0.20.0 | MIT OR Apache-2.0 |
| `uefi-raw` | 0.17.0 | MIT OR Apache-2.0 |
| `uguid` | 2.2.1 | MIT OR Apache-2.0 |
| `unicode-ident` | 1.0.27 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| `utf8parse` | 0.2.2 | Apache-2.0 OR MIT |
| `winnow` | 0.7.15 | MIT |

`windows-link` and `windows-sys` are also present in `Cargo.lock`, but they
are **Windows-only** (`[target.'cfg(windows)'.dependencies]` inside
`anstyle-wincon`) and are never built on the Linux or macOS hosts this
project targets.

---

## 4. Licence compatibility

The set is a **permissive** set. Every dependency is MIT, Apache-2.0,
Unlicense, or Unicode-3.0, with one exception:

* **`ucs2` is MPL-2.0.** MPL-2.0 is a file-level copyleft licence. It is
  compatible with MIT, Apache-2.0 and LGPL-2.1 distribution, provided the
  MPL-covered files are not modified in a way that would require the whole
  combined work to be published under MPL. `ucs2` is reached only through
  `serde_derive` → `syn` → `proc-macro2` → `unicode-ident`, and it is a
  **host-only, build-time** dependency: it is compiled into the proc-macro
  that VelaOS's own build uses, and it is **not** linked into the kernel
  image, into any user-space program, or into any shipped binary. It is
  therefore not distributed with VelaOS's runtime artefacts and creates no
  obligation on them.

No dependency is GPL-licensed. Nothing in the tree pulls in the
`kernel/`, `hal/` or `userland/` code under a copyleft obligation.

### 4.1 — Obligations, in summary

If VelaOS is distributed:

* the **MIT** dependencies require the copyright notice and permission
  notice to be included;
* the **Apache-2.0** dependencies require the notice to be included and
  require the Apache-2.0 to be clearly stated as the licence of the
  dependency;
* the **Unlicense** dependency imposes no obligation at all;
* the **Unicode-3.0** terms of `unicode-ident` require the notice to be
  included.

All of these are satisfied by including the upstream licence texts. A
distributor should ship a `THIRD_PARTY_LICENSES/` directory containing the
`LICENSE-MIT` and `LICENSE-APACHE` files from each dependency, and — if
VelaOS's own licence stays LGPL-2.1 — an LGPL-2.1 text for the project
itself, which is already the root `LICENSE`.

---

## 5. Non-Rust components

The following are used by the tooling and the CI, and are **not** Rust
crates. They are not redistributed as part of VelaOS's source tree.

| component | role | licence |
|---|---|---|
| `qemu-system-x86_64` | runs the kernel in the boot-smoke and gui-golden tests | GPL-2.0-only |
| OVMF (`OVMF.fd`) | the UEFI firmware used to boot the loader | BSD-2-Clause (and GPL-2.0 for some builds) |
| `mtools` (`mcopy`, `mmd`) | writes files into the FAT image | GPL-2.0-only |
| `mkfs.vfat` (`dosfstools`) | creates the FAT image | GPL-2.0-only |
| `rustc` / `cargo` / `rustfmt` / `clippy` | the toolchain | Apache-2.0 OR MIT |

QEMU, OVMF, `mtools` and `dosfstools` are **invoked as tools**; none of them
is linked into VelaOS. They run on the developer's or CI runner's machine,
and their licences do not extend to VelaOS's output. OVMF is the one case
worth calling out: it is *loaded into* the emulated machine at run time, but
it is not copied into the VelaOS source tree or into the produced image, so
it is a run-time dependency of the *test environment*, not a build-time
dependency of the artefact.

---

## 6. Original assets

Everything else in the tree is original work and is covered by VelaOS's own
licence:

* the **bitmap font** in `graphics/text` — an original 8x16 font, drawn for
  this project. It is **not** derived from the AmigaOS Topaz font, from
  X11's `misc-fixed`, or from any other existing font file;
* the **classic palette and metrics** in `graphics/theme-classic` — an
  original Workbench-*inspired* palette. The colour values were chosen for
  this project;
* the **splash screen** rendered by `boot/uefi-loader` — drawn in code by
  the loader itself.

No bitmap, font, icon or other binary asset is vendored into the repository
at all. The only `include_bytes!`/`include_str!` in the tree is
`hal/x86_64/src/apic.rs`, which includes its own source text for a
self-check.

---

## 7. Regenerating this document

```bash
# The exact set of external crates that are built.
cargo tree --workspace --edges normal,build --prefix none | sort -u

# The exact versions, and which of them came from crates.io.
grep -A2 '^name = ' Cargo.lock | grep -B1 'registry+'

# The licence each one declares.
for c in $(ls ~/.cargo/registry/src/*/); do
  printf '%-30s %s\n' "$c" "$(grep -m1 '^license' "$c/Cargo.toml" 2>/dev/null)"
done
```

---

## 8. References

* `Cargo.toml` — the workspace dependency declarations.
* `Cargo.lock` — the exact resolved versions.
* `LICENSE` — the project's own licence text (LGPL-2.1).
* `docs/boot/boot-protocol-v1.md` — the loader/kernel handoff, which is
  what keeps `uefi` out of the kernel.
* `docs/architecture/ADR-001-kernel-model.md` — the kernel model these
  dependencies serve.
