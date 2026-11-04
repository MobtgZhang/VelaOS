//! Link the init program with the userland layout.
//!
//! # Why a build script rather than `.cargo/config.toml`
//!
//! The workspace's target flags are shared by the kernel and every userland
//! binary. The kernel has its own linker script, and adding a second `-T`
//! option for the userland would apply to the kernel too. A build script runs
//! per package, so this argument reaches the `init` link and nothing else.
//!
//! # Why a linker script instead of `--Ttext`
//!
//! A user address space inherits the kernel's supervisor identity map, so the
//! program is linked at 16 TiB, above every physical window. `--Ttext` moves
//! only `.text`; the read-only and data segments stay wherever the default
//! layout put them, and their virtual addresses are not page-aligned. The
//! loader maps each segment a page at a time and needs the segments aligned so
//! each can carry its own permissions. The script lays out four aligned
//! segments; see `userland/user-x86_64.ld`.
//!
//! Hosted builds are untouched: the tests link for the host, where none of
//! these addresses exist.

use std::path::Path;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os == "none" && target_arch == "x86_64" {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../user-x86_64.ld");
        println!("cargo:rerun-if-changed=../user-x86_64.ld");
        println!("cargo:rustc-link-arg=-T");
        println!("cargo:rustc-link-arg={}", script.display());
    }
}
