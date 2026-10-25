//! `init`'s testable half: the bootstrap work, separated from the entry point.
//!
//! # Why this is a library and not just a binary
//!
//! A user program that does its work in `_start` can only be tested by booting
//! it. That is a real test — it is how `boot-smoke` proves the whole chain
//! works — but it is a terrible way to test the *decisions* `init` makes,
//! because every failure looks the same from outside: a machine that stops.
//!
//! So the decisions live here, as [`run`], and return a [`Report`] the caller
//! inspects. `main.rs` keeps only `_start` and the handover. The hosted tests
//! drive [`run`] directly and assert on which directories were created; the
//! boot smoke test drives `_start` and asserts that the machine reaches
//! `power_off`. Neither is a substitute for the other, and both are cheap.

#![cfg_attr(target_os = "none", no_std)]

extern crate alloc;

use vela_sys::api;

/// The directories created under `SYS:`.
///
/// `SYS:` is the read-only half of the namespace — the system's own
/// configuration and the programs a user cannot delete. Created empty, but
/// present, because a program that probes for a path's *existence* gets a
/// meaningful answer rather than a failure it has to interpret.
pub const SYS_TREE: [&str; 3] = ["SYS:", "SYS:System", "SYS:System/Prefs"];

/// The directories created under `RAM:`.
///
/// Written out rather than generated so that a change to the tree is a diff a
/// reviewer can read, and so the order is deliberate: parents before children,
/// because `MakeDir` refuses a parent that is not there and this program would
/// otherwise have to tolerate — and hide — a partial failure.
pub const RAM_TREE: [&str; 6] = [
    "RAM:",
    "RAM:T",
    "RAM:Utilities",
    "RAM:Utilities/Clipboards",
    "RAM:Clipboards",
    "RAM:Env",
];

/// What `init` did, as data.
///
/// Returned rather than printed, so a test can assert on the *work* rather
/// than on the log — a program that reports success in its output while doing
/// nothing would otherwise pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    /// Directories successfully created under `SYS:`.
    pub sys_ready: u32,
    /// Directories successfully created under `RAM:`.
    pub ram_ready: u32,
    /// Directories that could not be created for a reason other than "already
    /// there".
    pub failures: u32,
}

impl Report {
    /// True when the namespace is complete enough to start a desktop.
    ///
    /// `SYS:` is the one that matters: a desktop without it produces windows
    /// that cannot find their configuration, which presents as a graphics bug
    /// rather than a filesystem one. `RAM:` being short is tolerable — a
    /// program that needs a scratch directory can make one.
    pub const fn is_usable(&self) -> bool {
        self.sys_ready == SYS_TREE.len() as u32
    }
}

/// Populate the DOS namespace.
///
/// # What `init` does *not* do
///
/// It does not mount anything. VelaOS's kernel mounts its filesystems before
/// any user program runs, because a kernel that let a user program choose what
/// to mount would be a kernel with a sandbox escape in it. What is genuinely
/// user-space policy is the *contents* of the namespace, which is what this
/// function builds.
pub fn run() -> Report {
    let mut report = Report::default();

    // `exec.library` is always open. Everything else is opened by name, and a
    // null handle means "not here" rather than an error, which is what makes
    // the AmigaOS `if (lib = OpenLibrary(...))` idiom work. A null handle for
    // `dos.library` is therefore tolerated: the kernel's DOS service answers
    // the calls either way, and refusing to continue would turn a missing
    // optimisation into an unusable machine.
    api::open_library("dos.library").ok();

    // Populate the namespace, parents first within each tree.
    for path in SYS_TREE {
        report.sys_ready += create(path, &mut report);
    }
    for path in RAM_TREE {
        report.ram_ready += create(path, &mut report);
    }

    report
}

/// Create one directory, counting the outcome.
///
/// A directory that already exists is a **success**, not a failure: this runs
/// on every boot against a `RAM:` that may or may not have survived, and
/// "already there" is the outcome the caller wanted either way. Treating it as
/// an error would mean reporting an error on every single boot, which trains
/// whoever reads the log to ignore it.
fn create(path: &str, report: &mut Report) -> u32 {
    match api::create_dir(path) {
        Ok(()) => 1,
        // "It exists" is the one error code that means the goal was met.
        Err(e) if e.code == api::errors::OBJECT_IN_USE => 1,
        Err(_) => {
            report.failures += 1;
            0
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn the_trees_are_ordered_parents_before_children() {
        // `MakeDir` refuses a parent that is not there, so this ordering is
        // load-bearing rather than cosmetic. Iterated as slices because the two
        // trees have different lengths and an array of arrays would force them
        // to match.
        for tree in [&SYS_TREE[..], &RAM_TREE[..]] {
            for (i, path) in tree.iter().enumerate() {
                if let Some((parent, _)) = path.rsplit_once('/') {
                    assert!(
                        tree[..i].contains(&parent),
                        "{path} is created before its parent {parent}",
                    );
                }
            }
        }
    }

    #[test]
    fn a_fresh_report_is_not_usable() {
        let r = Report::default();
        assert!(!r.is_usable());
        assert_eq!(r.failures, 0);
    }

    #[test]
    fn usability_is_about_sys_not_ram() {
        let r = Report {
            ram_ready: RAM_TREE.len() as u32,
            ..Default::default()
        };
        // A complete RAM: tree with no SYS: is not enough to start a desktop.
        assert!(!r.is_usable());

        // A complete SYS: tree is, even with an empty RAM:.
        let r = Report {
            sys_ready: SYS_TREE.len() as u32,
            ..Default::default()
        };
        assert!(r.is_usable());
    }

    #[test]
    fn a_partial_sys_tree_is_not_usable() {
        // One missing directory out of three must not read as ready, or a
        // desktop starts against a namespace it cannot use.
        let r = Report {
            sys_ready: SYS_TREE.len() as u32 - 1,
            ..Default::default()
        };
        assert!(!r.is_usable());
    }

    #[test]
    fn the_trees_cover_the_volumes_a_desktop_expects() {
        // A regression guard on the trees themselves: the volume prefixes are
        // the contract with the kernel's assigns, and a typo here would produce
        // directories on the wrong volume.
        let all: Vec<&str> = SYS_TREE.iter().chain(RAM_TREE.iter()).copied().collect();
        for path in &all {
            assert!(
                path.starts_with("SYS:") || path.starts_with("RAM:"),
                "{path} is not on a known volume",
            );
        }
        assert!(all.contains(&"SYS:System/Prefs"));
        assert!(all.contains(&"RAM:T"));
    }

    #[test]
    fn the_two_trees_do_not_overlap() {
        // `SYS:` and `RAM:` are different mounts, so a path cannot be in both.
        for sys in SYS_TREE {
            assert!(
                !RAM_TREE.contains(&sys),
                "{sys} appears in both trees, so it would be created twice",
            );
        }
    }
}
