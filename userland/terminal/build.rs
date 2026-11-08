//! Link the terminal binary with the userland layout.
//!
//! See `userland/init/build.rs` for why this is a build script rather than a
//! workspace-wide flag. Hosted test builds are untouched: a bare-metal linker
//! script would strip the host's startup code and crash the test harness.

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
