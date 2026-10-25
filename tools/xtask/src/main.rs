//! `xtask` — the single developer entry point for VelaOS.
//!
//! Subcommands:
//!
//! * `build`  — compile the loader, kernel and userland for a target.
//! * `image`  — build a bootable GPT disk with a FAT ESP.
//! * `qemu`   — boot the image under QEMU/OVMF with a hard timeout.
//! * `test`   — run a named test suite.
//! * `run`    — `build` + `image` + `qemu`.
//!
//! Everything here is deliberately plain: `std`, `std::process::Command`, and
//! no build-script magic, so a failing build step is visible in the output.
//!
//! The implementation lives in the library half of this crate
//! (`src/lib.rs`), because the automated tests in `tests/` must drive the
//! same image builder and the same QEMU runner that a developer uses.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use xtask::{build_kernel, build_loader, check_other_arches, image, qemu, repo_root, Arch};

#[derive(Debug, Parser)]
#[command(name = "xtask", about = "VelaOS build automation", version)]
struct Cli {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Debug, Subcommand)]
enum Command_ {
    /// Compile the loader, kernel and userland.
    Build {
        #[arg(long, value_enum, default_value_t = Arch::X86_64)]
        arch: Arch,
        #[arg(long, default_value = "dev")]
        profile: String,
    },
    /// Compile everything needed to boot.
    All {
        #[arg(long, value_enum, default_value_t = Arch::X86_64)]
        arch: Arch,
        #[arg(long, default_value = "dev")]
        profile: String,
    },
    /// Build a bootable disk image.
    Image {
        #[arg(long, value_enum, default_value_t = Arch::X86_64)]
        arch: Arch,
        #[arg(long, default_value = "release")]
        profile: String,
        /// Output image path.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Boot an image under QEMU.
    Qemu {
        #[arg(long)]
        image: Option<PathBuf>,
        /// Use SDL instead of a hidden display.
        #[arg(long)]
        display: bool,
        /// Seconds before the VM is killed regardless of progress.
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// build + image + qemu.
    Run {
        #[arg(long, value_enum, default_value_t = Arch::X86_64)]
        arch: Arch,
        #[arg(long, default_value = "release")]
        profile: String,
        #[arg(long)]
        display: bool,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    /// Run a named test suite.
    Test {
        #[arg(long)]
        suite: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = repo_root();

    match cli.command {
        Command_::Build { arch, profile } => {
            build_loader(&root, arch, &profile)?;
            build_kernel(&root, arch, &profile)?;
        }
        Command_::All { arch, profile } => {
            build_loader(&root, arch, &profile)?;
            build_kernel(&root, arch, &profile)?;
            check_other_arches(&root)?;
        }
        Command_::Image { arch, profile, out } => {
            let out = out.unwrap_or_else(|| root.join("target/vela.img"));
            build_loader(&root, arch, &profile)?;
            build_kernel(&root, arch, &profile)?;
            image::build(&root, arch, &profile, &out)?;
        }
        Command_::Qemu {
            image,
            display,
            timeout,
        } => {
            let img = image.unwrap_or_else(|| root.join("target/vela.img"));
            qemu::run(&root, &img, display, timeout)?;
        }
        Command_::Run {
            arch,
            profile,
            display,
            timeout,
        } => {
            let out = root.join("target/vela.img");
            build_loader(&root, arch, &profile)?;
            build_kernel(&root, arch, &profile)?;
            image::build(&root, arch, &profile, &out)?;
            qemu::run(&root, &out, display, timeout)?;
        }
        Command_::Test { suite } => run_test(&root, &suite)?,
    }
    Ok(())
}

/// The test suites `xtask test --suite` knows about.
///
/// The packages are listed explicitly rather than discovered, so that a typo
/// in a suite name is an error instead of a silent "no tests ran".
const SUITES: &[(&str, &str)] = &[
    ("abi-layout", "vela-test-abi-layout"),
    ("api-conformance", "vela-test-api-conformance"),
    ("boot-smoke", "vela-test-boot-smoke"),
    ("gui-golden", "vela-test-gui-golden"),
];

fn run_test(root: &std::path::Path, suite: &str) -> Result<()> {
    let Some((_, pkg)) = SUITES.iter().find(|(name, _)| *name == suite) else {
        let names: Vec<&str> = SUITES.iter().map(|(n, _)| *n).collect();
        bail!("unknown suite {suite}; expected one of {names:?}");
    };
    println!("==> cargo test -p {pkg}");
    let status = xtask::cargo(root)
        .args(["test", "-p", pkg])
        .status()
        .context("failed to run cargo test")?;
    if !status.success() {
        bail!("test suite {suite} failed");
    }
    Ok(())
}
