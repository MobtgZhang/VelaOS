//! VelaOS build automation, as a library.
//!
//! `xtask` is the single developer entry point for VelaOS. The binary in
//! `src/main.rs` is a thin CLI over this library, and the split exists for
//! one reason: the automated tests in `tests/` must exercise the *same* image
//! builder and the *same* QEMU runner the developer uses. A test that
//! re-implemented "build an image and boot it" would be testing its own
//! copy, and the two would drift.
//!
//! The subcommands the CLI exposes:
//!
//! * `build`  — compile the loader, kernel and userland for a target.
//! * `image`  — build a bootable GPT disk with a FAT ESP.
//! * `qemu`   — boot the image under QEMU/OVMF with a hard timeout.
//! * `test`   — run a named test suite.
//! * `run`    — `build` + `image` + `qemu`.

pub mod image;
pub mod qemu;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use clap::ValueEnum;

/// Architectures VelaOS v0.1.0 can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Arch {
    /// x86-64 — the only architecture with a working UEFI target and drivers in
    /// v0.1.0. All other architectures are compile-checked only.
    X86_64,
}

impl Arch {
    /// The bare-metal Rust target the kernel and the userland binaries use.
    pub fn bare_target(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-none",
        }
    }

    /// The UEFI target used for the loader.
    pub fn uefi_target(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-uefi",
        }
    }

    /// Removable-media bootloader filename the firmware looks for.
    pub fn bootloader_name(self) -> &'static str {
        match self {
            Self::X86_64 => "BOOTX64.EFI",
        }
    }

    /// The architectures that CI only needs to `cargo check`.
    pub const fn check_only_targets() -> &'static [&'static str] {
        &[
            "aarch64-unknown-none",
            "riscv64gc-unknown-none-elf",
            "loongarch64-unknown-none",
        ]
    }
}

/// Repository root, derived from the manifest directory so `xtask` works from
/// any current directory.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask lives at <root>/tools/xtask")
        .to_path_buf()
}

/// A `cargo` invocation rooted at `root`.
pub fn cargo(root: &Path) -> Command {
    let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    c.current_dir(root);
    c
}

/// Build the UEFI loader.
pub fn build_loader(root: &Path, arch: Arch, profile: &str) -> Result<()> {
    println!("==> building loader for {}", arch.uefi_target());
    let status = cargo(root)
        .args(["build", "-p", "vela-uefi-loader", "--features", "uefi"])
        .args(["--target", arch.uefi_target()])
        .args(["--profile", profile])
        .status()
        .context("failed to run cargo for vela-uefi-loader")?;
    if !status.success() {
        bail!("loader build failed");
    }
    Ok(())
}

/// Build the kernel and the userland binaries it loads.
pub fn build_kernel(root: &Path, arch: Arch, profile: &str) -> Result<()> {
    println!("==> building kernel for {}", arch.bare_target());
    let status = cargo(root)
        .args([
            "build",
            "-p",
            "vela-kernel-entry",
            "-p",
            "vela-init",
            "-p",
            "vela-workbench",
            "-p",
            "vela-guitest",
            "--target",
            arch.bare_target(),
        ])
        .args(["--profile", profile])
        .status()
        .context("failed to run cargo for the kernel and userland")?;
    if !status.success() {
        bail!("kernel build failed");
    }
    Ok(())
}

/// `cargo check` the other three architectures.
///
/// v0.1.0 does not boot them, so this only proves the `no_std` crates stay
/// portable. Failures here are CI failures because portability is the point
/// of the trait boundaries.
pub fn check_other_arches(root: &Path) -> Result<()> {
    for target in Arch::check_only_targets() {
        println!("==> cargo check {target}");
        let status = cargo(root)
            .args(["check", "-p", "vela-hal-common", "--target", target])
            .status()
            .context("failed to run cargo check")?;
        if !status.success() {
            bail!("cross-arch check failed for {target}");
        }
    }
    Ok(())
}

/// The directory cargo writes a profile's artifacts into.
///
/// # Why this mapping is needed
///
/// Cargo's profile names and its output directory names are not the same:
/// the `dev` profile is written to `target/<triple>/debug/`, and the
/// `release` profile to `target/<triple>/release/`. Looking for
/// `target/<triple>/dev/` therefore finds nothing, and the artifact lookup
/// reports "not found" immediately after a build that plainly succeeded —
/// which is exactly the failure this mapping removes.
pub fn profile_dir(profile: &str) -> &str {
    match profile {
        "dev" | "test" => "debug",
        other => other,
    }
}

/// Locate a built artifact for `profile` and `target`, trying the
/// `target/<target>/<profile>` and `target/<profile>` layouts cargo uses.
pub fn artifact(
    root: &Path,
    package: &str,
    binary: &str,
    ext: &str,
    arch: Arch,
    profile: &str,
) -> Result<PathBuf> {
    let file = format!("{binary}.{ext}");
    // A bare-metal binary built by cargo has **no extension** — cargo writes
    // `vela-kernel`, not `vela-kernel.elf`, even though it is an ELF. The
    // `.elf` name is what the *image* wants it called inside the ESP, so the
    // packaging step renames it. Looking only for `{binary}.{ext}` therefore
    // searched for a file cargo never produces, and reported the artifact as
    // missing even though the build had just succeeded.
    let bare = binary.to_string();
    let names = if file == bare {
        vec![file.clone()]
    } else {
        vec![file.clone(), bare]
    };
    let dir = profile_dir(profile);
    let mut candidates = Vec::new();
    for name in &names {
        // The bare-metal layout, for the kernel and the userland binaries.
        candidates.push(root.join(format!("target/{}/{dir}/{name}", arch.bare_target())));
        // The UEFI layout, for the loader. It is built for a *different*
        // target than the kernel, so it lands in its own directory — and
        // because it was the only artifact searched for with the bare-metal
        // path alone, `xtask image` could not find the EFI image it had just
        // built successfully.
        candidates.push(root.join(format!("target/{}/{dir}/{name}", arch.uefi_target())));
        // The host layout, for a build without an explicit `--target`.
        candidates.push(root.join(format!("target/{dir}/{name}")));
    }
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(anyhow!(
        "artifact {file} for package {package} not found under target/{}/ or target/ ({dir})",
        arch.bare_target()
    ))
}

/// Read `key = value` pairs from the loader's generated build info.
pub fn read_manifest(root: &Path) -> Result<toml::Value> {
    let path = root.join("vela.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    text.parse::<toml::Value>()
        .with_context(|| format!("parsing {}", path.display()))
}
