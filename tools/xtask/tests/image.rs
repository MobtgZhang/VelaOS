//! Tests for the disk-image builder's pure helpers.
//!
//! # Why these live in an integration test
//!
//! The helpers being tested — the `mdir` size parser and the mtools path
//! converter — are the two places in the image builder where a mistake
//! produces a *wrong image* rather than an error. A `mdir` size that is
//! parsed as zero makes the verifier accept a truncated file; a path with a
//! backslash makes `mcopy` write to the wrong place.
//!
//! The project bans `#[cfg]` outside `hal/` and `boot/`, so a `#[cfg(test)]`
//! module inside `src/image.rs` is not available. The helpers are therefore
//! `pub` and the tests live here, where cargo builds them as a separate test
//! binary — no conditional compilation anywhere.

use std::path::Path;

use xtask::image::{parse_mdir_size, rel_mtools, DISK_SIZE_MB, ESP_SIZE_MB};

/// The ESP has to be big enough for a real FAT32 filesystem.
///
/// FAT32 requires at least 65525 clusters. With one sector per cluster and
/// 512-byte sectors that is about 32 MiB of usable space, so a 64 MiB
/// ESP is the floor. The constant is compared at compile time, so the
/// assertion is about the *value*, not about a runtime condition.
#[test]
fn the_esp_is_large_enough_for_fat32() {
    const _: () = assert!(ESP_SIZE_MB >= 64, "the ESP is too small for a real FAT32");
    const _: () = assert!(DISK_SIZE_MB > ESP_SIZE_MB);
}

/// The mtools path is relative to the staging root and uses forward slashes.
///
/// `mtools` interprets a backslash in a path as an escape rather than as a
/// separator, so a path that keeps the host's separator would silently
/// write the file somewhere else.
#[test]
fn the_mtools_path_is_relative_and_uses_forward_slashes() {
    let staging = Path::new("/staging");
    assert_eq!(
        rel_mtools(staging, Path::new("/staging/EFI/BOOT/BOOTX64.EFI")),
        "EFI/BOOT/BOOTX64.EFI"
    );
    assert_eq!(rel_mtools(staging, Path::new("/staging")), "");
}

/// The `mdir` size parser survives the comma thousands separator.
///
/// `mdir` formats its sizes as `15,360`, and the size sits in a column that
/// shifts with the date, so a naive whitespace split picks up the wrong
/// token. This is the exact output the builder sees.
#[test]
fn the_mdir_size_parser_survives_the_comma_grouping() {
    let text = "10/23/2026  13:53              15,360  BOOTX64.EFI\n";
    assert_eq!(parse_mdir_size(text), Some(15360));
}

/// The `mdir` size parser returns nothing for output that has no size.
///
/// The header lines `mdir` prints before the file list carry no size at
/// all, and treating one of them as a size would make the verifier compare
/// against a bogus number.
#[test]
fn the_mdir_size_parser_returns_nothing_for_noise() {
    assert_eq!(parse_mdir_size("Volume in drive: vela\n"), None);
    assert_eq!(parse_mdir_size(""), None);
}

/// The `mdir` size parser takes the largest size when several are present.
///
/// A directory listing can contain more than one file, and the verifier
/// wants the size of the file it is checking. Taking the maximum is what
/// makes "at least this many bytes" mean something when the listing is
/// noisy.
#[test]
fn the_mdir_size_parser_takes_the_largest_size() {
    let text = "10/23/2026  13:53              15,360  BOOTX64.EFI\n\
                10/23/2026  13:53             105,752  vela-kernel\n";
    assert_eq!(parse_mdir_size(text), Some(105752));
}
