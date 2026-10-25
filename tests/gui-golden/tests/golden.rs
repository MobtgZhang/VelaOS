//! The VelaOS framebuffer golden test.
//!
//! # What it does
//!
//! 1. Builds a bootable disk image.
//! 2. Boots it under QEMU with a hard timeout.
//! 3. Waits for the kernel's ready banner and for the desktop to settle.
//! 4. Captures the framebuffer with QEMU's `screendump`.
//! 5. Compares the capture against the committed reference.
//! 6. Moves the emulated mouse, captures again, and compares that too.
//!
//! # The reference
//!
//! The reference is **committed** to the repository, at
//! `tests/gui-golden/reference/`. It is not generated on first run: a test
//! that generates its own reference asserts only that the framebuffer is
//! stable between two runs, and would happily accept a black screen.
//!
//! # Regenerating the reference
//!
//! When the desktop legitimately changes, the reference is regenerated with:
//!
//! ```text
//! VELA_UPDATE_GOLDEN=1 cargo test -p vela-test-gui-golden
//! ```
//!
//! The environment variable is checked explicitly rather than inferred from
//! the presence or absence of the reference file, so a *missing* reference is
//! a visible skip with an explanation rather than a silent regeneration that
//! would make the test vacuous.
//!
//! # What it guarantees
//!
//! QEMU is terminated on every path: the [`xtask::qemu::Vm`] guard kills and
//! reaps the child when it is dropped, and the drop happens inside the
//! capture function whether the capture succeeded or failed.

use vela_test_gui_golden as golden;

/// The tag this test namespaces its files under.
const TAG: &str = "golden";

/// The scripted synthetic mouse move.
///
/// A single move, far enough to cross the desktop's gadget areas and back.
/// The mouse has no button held, so the desktop's own logic treats it as a
/// movement rather than a click — which is exactly what the test wants: the
/// input path is exercised, and the desktop's state is unchanged.
const MOUSE_MOVES: &[(i32, i32)] = &[(64, 32), (64, 32)];

#[test]
fn the_desktop_matches_the_stored_reference() {
    let root = golden::workspace_root();

    if let Some(reason) = golden::precondition_failure() {
        assert!(golden::skip(&reason), "the skip must be reported as a skip");
        return;
    }

    // --- references -------------------------------------------------------
    let desktop_ref_path = golden::desktop_reference();
    let mouse_ref_path = golden::mouse_reference();
    let updating = std::env::var_os("VELA_UPDATE_GOLDEN").is_some();

    if !updating && !desktop_ref_path.exists() {
        println!(
            "SKIP: the golden reference {} does not exist, so there is nothing to \
             compare against. Generate it with `VELA_UPDATE_GOLDEN=1 cargo test -p \
             vela-test-gui-golden`.",
            desktop_ref_path.display()
        );
        return;
    }
    if !updating && !mouse_ref_path.exists() {
        println!(
            "SKIP: the golden reference {} does not exist, so there is nothing to \
             compare against. Generate it with `VELA_UPDATE_GOLDEN=1 cargo test -p \
             vela-test-gui-golden`.",
            mouse_ref_path.display()
        );
        return;
    }

    // --- build ------------------------------------------------------------
    let image = match golden::build_image(&root, TAG, "dev") {
        Ok(p) => p,
        Err(e) => panic!("building the bootable image failed: {e:#}"),
    };

    // --- capture ----------------------------------------------------------
    let shot = root.join(format!("target/golden-{TAG}.ppm"));
    let capture = match golden::capture(&root, &image, TAG, &shot, &[]) {
        Ok(c) => c,
        Err(e) => panic!("capturing the framebuffer failed: {e:#}"),
    };

    if updating {
        if let Err(e) = golden::write_ppm(&desktop_ref_path, &capture.image) {
            panic!("writing the reference failed: {e:#}");
        }
        println!(
            "wrote the reference {} ({}x{})",
            desktop_ref_path.display(),
            capture.image.width,
            capture.image.height
        );
    } else {
        let reference = match golden::read_ppm(&desktop_ref_path) {
            Ok(r) => r,
            Err(e) => panic!("reading the reference failed: {e:#}"),
        };
        if let Err(e) = golden::compare(&capture.image, &reference, golden::DEFAULT_TOLERANCE) {
            panic!("{e:#}");
        }
    }

    // --- the scripted mouse move -------------------------------------------
    let moved_shot = root.join(format!("target/golden-{TAG}-moved.ppm"));
    let moved = match golden::capture(&root, &image, TAG, &moved_shot, MOUSE_MOVES) {
        Ok(c) => c,
        Err(e) => panic!("capturing the framebuffer after the mouse move failed: {e:#}"),
    };

    if updating {
        if let Err(e) = golden::write_ppm(&mouse_ref_path, &moved.image) {
            panic!("writing the reference failed: {e:#}");
        }
        println!(
            "wrote the reference {} ({}x{})",
            mouse_ref_path.display(),
            moved.image.width,
            moved.image.height
        );
    } else {
        let reference = match golden::read_ppm(&mouse_ref_path) {
            Ok(r) => r,
            Err(e) => panic!("reading the reference failed: {e:#}"),
        };
        if let Err(e) = golden::compare(&moved.image, &reference, golden::DEFAULT_TOLERANCE) {
            panic!("{e:#}");
        }
    }
}
