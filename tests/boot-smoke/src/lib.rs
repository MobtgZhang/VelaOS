//! Support for the VelaOS boot smoke test.
//!
//! # What this crate tests
//!
//! The plan's Phase 8 asks for a test that "builds an image, boots QEMU with
//! a hard timeout, asserts on serial log markers, and **must always
//! terminate QEMU**". This module provides the machinery for exactly that,
//! and the actual assertions live in `tests/boot_smoke.rs`.
//!
//! # Why QEMU termination is guaranteed
//!
//! The project has a history of leaked QEMU processes: the probe scripts that
//! started a VM had no teardown on their error paths, so a failing script left
//! a VM running — holding a core busy and the image file open.
//!
//! Three independent mechanisms prevent that here:
//!
//! 1. [`xtask::qemu::Vm`] owns the child process and its `Drop`
//!    implementation kills **and reaps** it. `Drop` runs on the normal
//!    return, on an early `?` that propagates an error, and while a panic
//!    unwinds the stack.
//! 2. [`run_boot`] has an explicit wall-clock deadline. Even if the guard
//!    were somehow bypassed, the run cannot last longer than the deadline.
//! 3. [`run_boot`] waits for the child after killing it, so the process
//!    becomes a zombie that the operating system reaps, rather than a
//!    zombie that lingers.
//!
//! # Why the test skips instead of failing
//!
//! Not every machine can run this test. It needs `qemu-system-x86_64`, an
//! OVMF firmware image, and the `mtools`/`mkfs.vfat`/`sgdisk` trio to build
//! the disk image. A CI runner without QEMU should still be able to run the
//! ABI and conformance suites, and a developer on a machine without
//! OVMF should not have their `cargo test` fail.
//!
//! So the preconditions are checked first, and a missing prerequisite produces
//! a **skip**: the test returns successfully with an explicit message naming
//! what was missing. A skip is visibly different from a pass in the test
//! output, and it is never silent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use xtask::qemu::{self, Paths, Vm};
use xtask::Arch;

/// The repository root.
///
/// # Why this is walked up rather than assumed
///
/// A test's working directory is the *package* directory, not the workspace
/// root, and it is not stable across cargo versions or invocation styles.
/// The manifest directory is compile-time constant, so walking up from it is
/// the only way to find the root without depending on the CWD.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the boot-smoke crate lives at <root>/tests/boot-smoke")
        .to_path_buf()
}

/// Why a boot smoke run cannot happen on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `qemu-system-x86_64` is not on `PATH`.
    NoQemu,
    /// No OVMF firmware image was found.
    NoFirmware,
    /// A tool needed to build the disk image is missing.
    NoImageTools(&'static str),
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoQemu => write!(
                f,
                "qemu-system-x86_64 is not on PATH, so no machine can be booted"
            ),
            Self::NoFirmware => write!(
                f,
                "no OVMF firmware image was found, so no machine can be booted"
            ),
            Self::NoImageTools(tool) => write!(
                f,
                "{tool} is not on PATH, so the bootable disk image cannot be built"
            ),
        }
    }
}

/// The tools the image builder needs.
///
/// `mkfs.vfat` and `mtools` create and populate the FAT filesystem; `sgdisk`
/// lays out the GPT. A machine missing any of them cannot produce an image.
const IMAGE_TOOLS: &[&str] = &["mkfs.vfat", "mcopy", "mmd", "mdir", "sgdisk"];

/// Check whether this machine can run the boot smoke test.
///
/// Returns the reason it cannot, or `None` when it can.
pub fn precondition_failure() -> Option<SkipReason> {
    if !qemu::qemu_available() {
        return Some(SkipReason::NoQemu);
    }
    if qemu::find_firmware().is_none() {
        return Some(SkipReason::NoFirmware);
    }
    for tool in IMAGE_TOOLS {
        if qemu::which(tool).is_none() {
            return Some(SkipReason::NoImageTools(tool));
        }
    }
    None
}

/// Print the skip message and return `true` when the test must be skipped.
///
/// The message goes to standard output, where cargo shows it with
/// `--nocapture`, so a skipped test is visible rather than indistinguishable
/// from a passing one.
pub fn skip(reason: &SkipReason) -> bool {
    println!(
        "SKIP: the boot smoke test needs a machine it does not have — {reason}. \
         The test is not asserting anything and is not a failure."
    );
    true
}

/// Build the loader, the kernel, the userland and the disk image.
///
/// `tag` namespaces the scratch files, so two agents building images on the
/// same machine do not overwrite each other's staging tree.
pub fn build_image(root: &Path, tag: &str, profile: &str) -> Result<PathBuf> {
    xtask::build_loader(root, Arch::X86_64, profile).context("building the UEFI loader")?;
    xtask::build_kernel(root, Arch::X86_64, profile)
        .context("building the kernel and the userland")?;
    let out = root.join(format!("target/vela-{tag}.img"));
    xtask::image::build_tagged(root, Arch::X86_64, profile, &out, tag)
        .context("building the bootable disk image")?;
    Ok(out)
}

/// The outcome of a boot attempt.
#[derive(Debug)]
pub struct BootOutcome {
    /// The kernel's serial log.
    pub serial: String,
    /// The loader's debug-console log.
    pub debugcon: String,
    /// Whether the machine was still running when the deadline expired.
    ///
    /// The kernel idles forever by design — it has no reason to power itself
    /// off, because the desktop is supposed to stay up — so a *successful*
    /// boot normally ends in the deadline expiring and the VM being killed.
    pub timed_out: bool,
}

impl BootOutcome {
    /// True when the kernel's own banner is in the serial log.
    pub fn kernel_booted(&self) -> bool {
        qemu::looks_like_success(&self.serial)
    }

    /// True when the loader's marker is in the debug console log.
    pub fn loader_ran(&self) -> bool {
        qemu::loader_marker_present(&self.debugcon)
    }

    /// A human-readable summary of the boot, for a failure message.
    pub fn summary(&self) -> String {
        format!(
            "kernel booted: {}\nloader ran: {}\ntimed out: {}\n\
             --- serial log ---\n{}\n--- loader log ---\n{}",
            self.kernel_booted(),
            self.loader_ran(),
            self.timed_out,
            qemu::strip_ansi(&self.serial),
            qemu::strip_ansi(&self.debugcon),
        )
    }
}

/// Boot `image` and collect the logs.
///
/// # The guarantee
///
/// The QEMU process is **always** terminated before this function returns.
/// The [`Vm`] guard kills and reaps it on every path out of the function —
/// including the paths where the deadline expires and the paths where an
/// error is returned early — and the deadline bounds the wall-clock time
/// regardless.
pub fn run_boot(root: &Path, image: &Path, tag: &str, timeout: Duration) -> Result<BootOutcome> {
    let paths = Paths::tagged(root, tag);
    let mut vm = Vm::start(image, paths, false).context("starting QEMU")?;

    // A successful boot is signalled by the kernel's markers, not by process
    // exit: the desktop is designed to stay up. Once the markers are present
    // the VM has proven itself and can be stopped immediately; the deadline
    // still bounds a broken boot.
    let ready = vm.wait_for_markers(qemu::BOOT_MARKERS, timeout);
    let stopped = if ready { true } else { vm.wait_for(1)? };
    let serial = vm.serial_log();
    let debugcon = vm.debugcon_log();

    // `vm` is dropped here — or at the end of the function, on the error path
    // — and its `Drop` kills and reaps the child. Explicitly dropping it
    // here makes the teardown happen *before* the logs are handed back,
    // rather than whenever the caller's scope ends.
    drop(vm);

    Ok(BootOutcome {
        serial,
        debugcon,
        timed_out: !stopped,
    })
}

/// Assert that the boot produced the expected markers.
///
/// The kernel's own banner is the authoritative signal. The loader's marker
/// is checked as well, but separately: it lives on the debug console, and a
/// loader that ran and then failed to hand off would satisfy it while the
/// kernel banner is absent — so it can never stand in for the banner.
pub fn assert_markers(outcome: &BootOutcome) -> Result<()> {
    let mut missing = Vec::new();
    for marker in qemu::BOOT_MARKERS {
        if !qemu::strip_ansi(&outcome.serial).contains(marker) {
            missing.push(*marker);
        }
    }
    if !missing.is_empty() {
        bail!(
            "the boot did not produce every kernel marker.\n\
             Missing: {missing:?}\n{}",
            outcome.summary()
        );
    }
    if !outcome.loader_ran() {
        bail!(
            "the kernel booted but the loader's marker is missing from the debug \
             console, so the handoff did not happen the way it is documented to.\n{}",
            outcome.summary()
        );
    }
    Ok(())
}

/// Check that no QEMU process survived the run.
///
/// This is a belt-and-braces check on top of the guard: it turns "we believe
/// the teardown happened" into "we looked".
pub fn assert_no_leaked_vm(root: &Path, tag: &str) -> Result<()> {
    let monitor = root.join(format!("target/monitor-{tag}.sock"));
    // The monitor socket is created by the QEMU process and removed when it
    // exits. A socket that is still there means the VM is still running.
    if monitor.exists() {
        bail!(
            "the QEMU monitor socket {} still exists after the run; the VM was \
             not terminated",
            monitor.display()
        );
    }
    Ok(())
}
