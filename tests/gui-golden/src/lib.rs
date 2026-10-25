//! Support for the VelaOS framebuffer golden test.
//!
//! # What this crate tests
//!
//! The plan's Phase 8 asks for a test that "boots with a scripted synthetic
//! mouse move, captures the framebuffer via QEMU `screendump`, and compares
//! against a stored reference". This module provides the machinery, and the
//! assertions live in `tests/golden.rs`.
//!
//! # The approach: a committed reference, with a documented tolerance
//!
//! The golden reference is **committed to the repository**, not generated on
//! first run. The reasons are worth stating, because the opposite choice is
//! defensible and was considered:
//!
//! * A golden test that generates its reference on first run is not a golden
//!   test. It asserts only that the framebuffer is *stable* between two
//!   runs, which is a real property but a much weaker one — and it silently
//!   accepts whatever the framebuffer happens to be, including a black
//!   screen.
//! * A committed reference makes a change to the desktop a *visible*
//!   change: the diff is a failing test with a picture attached, and the
//!   reviewer decides whether the change was intended.
//!
//! The cost is that the reference has to be regenerated whenever the desktop
//! legitimately changes. `tests/golden.rs` provides a way to do that, and
//! the reference file records what it was captured from.
//!
//! # Why the comparison has a tolerance
//!
//! The reference is captured from a real QEMU run, and the pixels depend on
//! things that are not part of the kernel's behaviour: the exact framebuffer
//! the firmware hands over, and the moment the capture happened relative to
//! the desktop's repaints. A strict "every pixel must match" would fail on a
//! pixel of animation timing, which is noise, not a regression.
//!
//! So the comparison is a **fraction of differing pixels** with a threshold,
//! and the test reports the number so a change that is under the threshold is
//! still visible in the output.
//!
//! # The mouse move
//!
//! The plan asks for "a scripted synthetic mouse move". QEMU's monitor has a
//! `mouse_move` command that injects the same PS/2 packets a real mouse
//! would, so the kernel's input path — the PS/2 controller, the packet
//! decoder, the event queue and the IDCMP delivery — is exercised end to end
//! without a human at the keyboard.

pub mod ppm;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use xtask::qemu::{self, Paths, Vm};
use xtask::Arch;

pub use ppm::Image;

/// The repository root.
///
/// # Why this is walked up rather than assumed
///
/// A test's working directory is the package directory, not the workspace
/// root, and that is not stable across cargo versions or invocation styles.
/// The manifest directory is a compile-time constant, so walking up from it
/// is the only way to find the root without depending on the CWD.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the gui-golden crate lives at <root>/tests/gui-golden")
        .to_path_buf()
}

/// The directory the golden references live in.
pub fn reference_dir() -> PathBuf {
    workspace_root().join("tests/gui-golden/reference")
}

/// The reference for the desktop screen.
pub fn desktop_reference() -> PathBuf {
    reference_dir().join("desktop.ppm")
}

/// The reference for the screen after a scripted mouse move.
pub fn mouse_reference() -> PathBuf {
    reference_dir().join("desktop-after-mouse.ppm")
}

/// Why a golden test cannot run on this machine.
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
                "qemu-system-x86_64 is not on PATH, so no framebuffer can be captured"
            ),
            Self::NoFirmware => write!(
                f,
                "no OVMF firmware image was found, so no framebuffer can be captured"
            ),
            Self::NoImageTools(tool) => write!(
                f,
                "{tool} is not on PATH, so the bootable disk image cannot be built"
            ),
        }
    }
}

/// The tools the image builder needs.
const IMAGE_TOOLS: &[&str] = &["mkfs.vfat", "mcopy", "mmd", "mdir", "sgdisk"];

/// Check whether this machine can run the golden test.
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

/// Print the skip message and return `true`.
pub fn skip(reason: &SkipReason) -> bool {
    println!(
        "SKIP: the golden test needs a machine it does not have — {reason}. \
         The test is not asserting anything and is not a failure."
    );
    true
}

/// Build the loader, the kernel, the userland and the disk image.
pub fn build_image(root: &Path, tag: &str, profile: &str) -> Result<PathBuf> {
    xtask::build_loader(root, Arch::X86_64, profile).context("building the UEFI loader")?;
    xtask::build_kernel(root, Arch::X86_64, profile)
        .context("building the kernel and the userland")?;
    let out = root.join(format!("target/vela-{tag}.img"));
    xtask::image::build_tagged(root, Arch::X86_64, profile, &out, tag)
        .context("building the bootable disk image")?;
    Ok(out)
}

/// How long the kernel gets to reach its ready banner.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the desktop is given to settle before the capture.
///
/// The desktop is drawn by the userland, which starts after the kernel's
/// banner. A capture taken too early catches a half-drawn screen, so the
/// test waits for the banner *and* for a settling period.
pub const SETTLE_TIME: Duration = Duration::from_secs(5);

/// The captured framebuffer, plus where it came from.
#[derive(Debug)]
pub struct Capture {
    /// The decoded image.
    pub image: Image,
    /// The file the image was read from.
    pub path: PathBuf,
}

/// Read a PPM file.
pub fn read_ppm(path: &Path) -> Result<Image> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    ppm::decode(&bytes).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))
}

/// Write a PPM file.
pub fn write_ppm(path: &Path, image: &Image) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, ppm::encode(image))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Boot the machine, capture the framebuffer, and stop it.
///
/// # The guarantee
///
/// The QEMU process is terminated before this function returns, on every
/// path. The [`Vm`] guard kills and reaps it on drop, and the drop happens
/// here whether the capture succeeded or failed.
pub fn capture(
    root: &Path,
    image: &Path,
    tag: &str,
    out: &Path,
    mouse_moves: &[(i32, i32)],
) -> Result<Capture> {
    let paths = Paths::tagged(root, tag);
    let vm = Vm::start(image, paths, false).context("starting QEMU")?;

    // Wait for the kernel to reach its ready banner before capturing anything.
    // Without this the capture is a picture of the boot splash.
    let booted = vm.wait_for_markers(qemu::BOOT_MARKERS, BOOT_TIMEOUT);
    if !booted {
        let log = vm.serial_log();
        drop(vm);
        bail!(
            "the machine never reached the kernel's ready banner, so there is no \
             desktop to capture.\n--- serial log ---\n{}",
            qemu::strip_ansi(&log)
        );
    }

    // Let the desktop finish drawing.
    std::thread::sleep(SETTLE_TIME);

    // The scripted synthetic mouse move.
    for (dx, dy) in mouse_moves {
        vm.mouse_move(*dx, *dy)
            .with_context(|| format!("moving the mouse by ({dx}, {dy})"))?;
        // A small pause so the kernel processes the packet and repaints
        // before the next one arrives.
        std::thread::sleep(Duration::from_millis(250));
    }
    if !mouse_moves.is_empty() {
        std::thread::sleep(SETTLE_TIME);
    }

    vm.screendump_ppm(out)
        .with_context(|| format!("capturing the framebuffer into {}", out.display()))?;

    let image = read_ppm(out)?;

    // Terminate the machine before handing the image back.
    drop(vm);

    Ok(Capture {
        image,
        path: out.to_path_buf(),
    })
}

/// Compare a capture against a reference.
///
/// `tolerance` is the largest fraction of pixels that may differ before the
/// comparison fails. The number of differing pixels is reported either way,
/// so a change that stays under the threshold is still visible.
pub fn compare(capture: &Image, reference: &Image, tolerance: f64) -> Result<()> {
    if capture.width != reference.width || capture.height != reference.height {
        bail!(
            "the captured framebuffer is {}x{} but the reference is {}x{}; the \
             display geometry changed, so the reference must be regenerated",
            capture.width,
            capture.height,
            reference.width,
            reference.height,
        );
    }
    let differing = capture.differing_pixels(reference);
    let ratio = capture.difference_ratio(reference);
    if ratio > tolerance {
        bail!(
            "the framebuffer differs from the reference in {differing} pixels \
             ({:.4} of the image), which is more than the {tolerance:.4} tolerance.",
            ratio
        );
    }
    println!(
        "the framebuffer matches the reference: {differing} differing pixels \
         ({:.6} of the image), tolerance {tolerance:.4}",
        ratio
    );
    Ok(())
}

/// The fraction of pixels that may differ before the golden comparison fails.
///
/// The value is deliberately not zero. The capture depends on the exact
/// framebuffer the firmware hands over and on when it was taken relative to
/// the desktop's repaints, and neither is part of the kernel's behaviour. A
/// threshold of a few thousand pixels on a 1280x800 screen absorbs that
/// jitter while still catching any real change to the desktop, which alters
/// tens of thousands of pixels.
pub const DEFAULT_TOLERANCE: f64 = 0.01;
