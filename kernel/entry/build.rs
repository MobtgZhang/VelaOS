//! Build script for the kernel binary.
//!
//! # Why the linker script path needs a build script
//!
//! Cargo runs the linker with the *package* directory as the working
//! directory, but a path passed through `rustflags` in `.cargo/config.toml` is
//! resolved relative to wherever cargo was invoked from — the workspace root.
//! Neither location is stable for every invocation: `cargo build` from the
//! root and `cargo build -p vela-kernel-entry` from inside the crate resolve
//! the same relative path to two different files.
//!
//! `cargo:rustc-link-arg` solves this because the value is passed to the
//! linker as an opaque token and the build script can compute an absolute
//! path from `CARGO_MANIFEST_DIR`, which is always correct.
//!
//! The alternative — asking every developer to run cargo from the workspace
//! root — is exactly the kind of invisible footgun that produces a linker
//! error on someone else's machine and nowhere else.

use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let script = manifest_dir.join("kernel-x86_64.ld");

    // Rebuild when the script changes. Without this, editing the linker
    // script does not trigger a relink, and the developer sees a stale binary
    // and concludes the change had no effect.
    println!("cargo:rerun-if-changed={}", script.display());

    assert!(
        script.exists(),
        "linker script missing: {}",
        script.display()
    );

    // The script describes the *bare-metal* kernel image: a higher-half
    // address, a fixed load address, and no C runtime. Applying it to a host
    // build — which is what `cargo test` does — is wrong twice over: the
    // host's linker would place the image at `0xFFFF8000_0000_0000` and fail,
    // and the host's `.fini_array` would overlap the script's `.bss`.
    //
    // So the script is applied only when the target is the bare-metal one.
    let target = std::env::var("TARGET").expect("TARGET is set by cargo");
    if target != "x86_64-unknown-none" {
        return;
    }

    println!("cargo:rustc-link-arg=-T{}", script.display());
}
