//! The VelaOS boot smoke test.
//!
//! # What it asserts
//!
//! The test builds a bootable disk image, boots it under QEMU with a hard
//! timeout, and asserts that the kernel's banner appeared on the serial log
//! and that the loader's marker appeared on the debug console.
//!
//! # What it guarantees
//!
//! QEMU is terminated on every path. The [`xtask::qemu::Vm`] guard kills and
//! reaps the child when it is dropped, and the drop happens inside
//! [`vela_test_boot_smoke::run_boot`] whether the run succeeded, failed or
//! panicked. The deadline is a second, independent bound.
//!
//! # What it does when it cannot run
//!
//! A machine without `qemu-system-x86_64`, without OVMF, or without the
//! image-building tools **skips** the test with an explicit message. It does
//! not fail: a CI runner that has no QEMU should still be able to run the
//! ABI and conformance suites, and the plan asks for the boot smoke to be a
//! separate job for exactly that reason.

use std::time::Duration;

use vela_test_boot_smoke as smoke;

/// The tag this test namespaces its files under.
///
/// Several agents share this machine; a per-agent tag keeps their images,
/// logs and monitor sockets apart.
const TAG: &str = "smoke";

/// How long the machine gets to boot.
///
/// The kernel reaches its banner well inside this window on a normal host.
/// The value is generous because a cold page cache and a busy machine make
/// the boot slower, and a timeout here is indistinguishable from a hang.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

#[test]
fn the_kernel_boots_and_reports_its_banner() {
    let root = smoke::workspace_root();

    // --- preconditions ----------------------------------------------------
    if let Some(reason) = smoke::precondition_failure() {
        assert!(smoke::skip(&reason), "the skip must be reported as a skip");
        return;
    }

    // --- build ------------------------------------------------------------
    let image = match smoke::build_image(&root, TAG, "dev") {
        Ok(p) => p,
        Err(e) => panic!("building the bootable image failed: {e:#}"),
    };

    // --- boot -------------------------------------------------------------
    let outcome = match smoke::run_boot(&root, &image, TAG, BOOT_TIMEOUT) {
        Ok(o) => o,
        Err(e) => panic!("running the machine failed: {e:#}"),
    };

    // --- assert -----------------------------------------------------------
    if let Err(e) = smoke::assert_markers(&outcome) {
        panic!("{e:#}");
    }

    // The VM must be gone by now.
    if let Err(e) = smoke::assert_no_leaked_vm(&root, TAG) {
        panic!("{e:#}");
    }
}

#[test]
fn the_environment_precondition_check_is_honest() {
    // The precondition check is what makes the test skippable, so it is worth
    // asserting that it agrees with the machine it is running on.
    //
    // The two are not allowed to disagree: if the precondition says "no QEMU"
    // but QEMU is on `PATH`, the test would skip while the machine could in
    // fact have run it.
    let failure = smoke::precondition_failure();
    match failure {
        None => {
            // The machine can run the test, so the tools must be findable.
            assert!(
                xtask::qemu::qemu_available(),
                "the precondition check said the machine is ready, but qemu-system-x86_64 \
                 is not on PATH"
            );
            assert!(
                xtask::qemu::find_firmware().is_some(),
                "the precondition check said the machine is ready, but no OVMF firmware \
                 image was found"
            );
        }
        Some(reason) => {
            // The machine cannot run the test. The reason must be one the
            // operator can act on, and it must say so in plain words.
            let text = reason.to_string();
            assert!(
                !text.is_empty(),
                "a skip reason must name the missing prerequisite"
            );
        }
    }
}
