//! Tests for the QEMU runner's log handling.
//!
//! # Why these live in an integration test
//!
//! The functions under test — the ANSI stripper, the success predicate and
//! the loader-marker predicate — are what decide whether a boot is judged a
//! success. A mistake in them does not produce a crash; it produces a
//! *green run that proved nothing*, which is the worst failure mode a
//! smoke test has.
//!
//! The project bans `#[cfg]` outside `hal/` and `boot/`, so an in-source
//! `#[cfg(test)]` module is not available. The items are therefore `pub`
//! and the tests live here, in a separate test binary.

use std::path::Path;

use xtask::qemu::{
    loader_marker_present, looks_like_success, strip_ansi, Paths, BOOT_MARKERS,
    FIRMWARE_CANDIDATES, LOADER_MARKER,
};

/// The stripper removes the cursor-control sequences OVMF and the kernel emit.
#[test]
fn ansi_escapes_are_stripped() {
    assert_eq!(strip_ansi("\u{1b}[2J\u{1b}[001Hhello"), "hello");
    assert_eq!(strip_ansi("plain"), "plain");
    assert_eq!(strip_ansi("\u{1b}[=3hbody"), "body");
}

/// The stripper normalises the carriage returns a terminal-style log has.
#[test]
fn carriage_returns_are_removed() {
    assert_eq!(strip_ansi("a\r\nb"), "a\nb");
}

/// Success requires **every** marker, not most of them.
#[test]
fn success_requires_every_marker() {
    let all = BOOT_MARKERS.join("\n");
    assert!(looks_like_success(&all));
    // One missing marker is a failure: a kernel that boots but never reaches
    // the desktop is not a passing smoke test.
    let partial = BOOT_MARKERS[..2].join("\n");
    assert!(!looks_like_success(&partial));
    assert!(!looks_like_success(""));
}

/// The markers are found even through the ANSI noise the kernel's log has.
#[test]
fn markers_are_found_through_ansi_noise() {
    let noisy = format!("\u{1b}[2J\u{1b}[001H{}\u{1b}[0m", BOOT_MARKERS.join("\n"));
    assert!(looks_like_success(&noisy));
}

/// The firmware search covers both of the naming conventions in the wild.
#[test]
fn the_firmware_candidates_include_both_naming_schemes() {
    // Distributions split OVMF as OVMF_CODE/OVMF_VARS as often as they ship
    // a combined image.
    let joined = FIRMWARE_CANDIDATES.join(" ");
    assert!(joined.contains("OVMF.fd"));
    assert!(joined.contains("OVMF_CODE.fd"));
}

/// The loader's marker lives in the debug-console log, not the serial log.
///
/// The loader runs under OVMF, which owns COM1, so its output cannot appear
/// on the serial port. The two logs must therefore be checked for different
/// markers, and the kernel's markers must not include the loader's.
#[test]
fn the_loader_marker_lives_in_the_debug_console_log() {
    assert!(!BOOT_MARKERS.contains(&LOADER_MARKER));
    assert!(loader_marker_present("[INFO] VELA-BOOT VelaOS UEFI loader"));
    assert!(!loader_marker_present("[INFO] something else"));
    assert!(!loader_marker_present(""));
}

/// A kernel log alone does not satisfy the loader marker.
///
/// The failure this guards against: looking for the loader's marker in the
/// serial log, where it never appears, so a good boot looks like a failed
/// one.
#[test]
fn a_kernel_log_alone_does_not_satisfy_the_loader_marker() {
    let kernel_log = BOOT_MARKERS.join("\n");
    assert!(looks_like_success(&kernel_log));
    assert!(!loader_marker_present(&kernel_log));
}

/// Two runs with different tags do not share a file.
///
/// Several agents share this machine, so the serial log, the debug-console
/// log and the monitor socket are all per-tag. A collision would mean one
/// run's log being overwritten while another was reading it.
#[test]
fn the_tagged_paths_do_not_collide() {
    let root = Path::new("/tmp/vela");
    let a = Paths::tagged(root, "a");
    let b = Paths::tagged(root, "b");
    assert_ne!(a.serial, b.serial);
    assert_ne!(a.debugcon, b.debugcon);
    assert_ne!(a.monitor, b.monitor);
}
