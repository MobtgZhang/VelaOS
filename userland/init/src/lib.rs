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
///
/// The shape is the classic AmigaOS 3.x / AROS system volume (see
/// `reports/System-all.md` §3.1): functional directories — `C` for shell
/// commands, `S` for startup scripts, `Libs` for shared libraries, `Devs`
/// for device drivers, `Classes` for class libraries (with the
/// `Datatypes`, `Gadgets` and `Zune` subdirectories a class library needs),
/// `L` for filesystems and handlers — rather than Linux's `/bin`, `/etc`,
/// `/usr` split. Alongside them sit the directories a desktop expects to
/// find: `boot`, `Fonts`, `Locale`, `Development`, `Prefs` (with
/// `Env-Archive`, where `ENVARC:` points), `System`, `Tools` (with
/// `Commodities`), `Utilities`, `Workbench`, `Demos`, `Extras`,
/// `Trashcan`, `Storage`, `Expansion` and `WBStartup`. The transient `T:`
/// and the environment `ENV:` live under `RAM:` instead, which is where the
/// kernel's assigns point them.
///
/// `Devs` carries the subdirectories the desktop expects (`DOSDrivers`,
/// `DataTypes`, `Drivers`, `Keymaps`, `Monitors`, `Printers`).
///
/// Written out rather than generated so that a change to the tree is a diff a
/// reviewer can read, and so the order is deliberate: parents before children,
/// because `MakeDir` refuses a parent that is not there and this program would
/// otherwise have to tolerate — and hide — a partial failure.
pub const SYS_TREE: [&str; 33] = [
    "SYS:",
    "SYS:boot",
    "SYS:C",
    "SYS:Classes",
    "SYS:Classes/Datatypes",
    "SYS:Classes/Gadgets",
    "SYS:Classes/Zune",
    "SYS:Development",
    "SYS:Demos",
    "SYS:Devs",
    "SYS:Devs/DataTypes",
    "SYS:Devs/DOSDrivers",
    "SYS:Devs/Drivers",
    "SYS:Devs/Keymaps",
    "SYS:Devs/Monitors",
    "SYS:Devs/Printers",
    "SYS:Expansion",
    "SYS:Extras",
    "SYS:Fonts",
    "SYS:L",
    "SYS:Libs",
    "SYS:Locale",
    "SYS:Prefs",
    "SYS:Prefs/Env-Archive",
    "SYS:S",
    "SYS:System",
    "SYS:Storage",
    "SYS:Tools",
    "SYS:Tools/Commodities",
    "SYS:Trashcan",
    "SYS:Utilities",
    "SYS:WBStartup",
    "SYS:Workbench",
];

/// The directories created under `RAM:`.
///
/// `RAM:` is the writable half of the namespace: the transient state a boot
/// session wants, and exactly the set AmigaOS and AROS both put on a RAM
/// disk — scratch space under `T:`, the clipboard under `Clipboards`, the
/// live environment under `Env:` — all of which a ramfs rebuilds empty on
/// every boot. Written out rather than generated so that a change to the
/// tree is a diff a reviewer can read, and so the order is deliberate:
/// parents before children, because `MakeDir` refuses a parent that is not
/// there and this program would otherwise have to tolerate — and hide — a
/// partial failure.
pub const RAM_TREE: [&str; 4] = ["RAM:", "RAM:T", "RAM:Clipboards", "RAM:Env"];

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

    seed_readme(&mut report);
    seed_startup_scripts(&mut report);

    report
}

/// Write one seeded text file, counting the outcome.
///
/// A fresh AmigaOS boots into a filesystem with content — AmigaOS used to
/// drop a `ReadMe` into `RAM:T` — and a VelaOS shell whose `catalog` has no
/// files in it leaves its file commands (`cat`, redirection-adjacent paths)
/// more hypothetical than demonstrable. Their absence is no failure:
/// `SYS:` and `RAM:` are ramfs-backed, so every boot starts empty anyway
/// and re-seeding is idempotent. Opening in write mode creates the file,
/// so the parent directory only has to exist — which is what the trees
/// above guarantee.
fn seed_readme(report: &mut Report) {
    let Ok(file) = api::open("RAM:T/ReadMe", api::mode::WRITE) else {
        report.failures += 1;
        return;
    };
    let written = api::write(file, READ_ME.as_bytes());
    // The handle must be closed whether the write worked or not: a computer
    // that reboots cannot leak handles, but the *code* should not depend on
    // that.
    let _ = api::close(file);
    match written {
        Ok(_) => {}
        Err(_) => report.failures += 1,
    }
}

/// The text `run` drops into `RAM:T/ReadMe`.
const READ_ME: &str = "This is the source for VelaOS's transient state.\n\
Boot a fresh session and this file returns.\n";

/// Seed the three startup scripts `S:` comes with.
///
/// AmigaOS boots by executing `S:Startup-Sequence`, which in turn runs
/// `S:User-Startup` for the user's own additions; AROS keeps the same
/// convention, and a shell's own startup runs `S:Shell-Startup` before its
/// first prompt. The shell's `Execute` runs these files' syntax — `.KEY`,
/// `IF EXISTS`, `Set`, one command per line — so they are written as real
/// scripts rather than as comments with ambitions, and `Execute
/// SYS:S/Shell-Startup` from the prompt is a working demonstration. Nothing
/// executes them *automatically* yet: a boot step that runs them is an
/// independent change. They are seeded as data because a boot script that
/// does not exist cannot be *read* either, and the point of the tree is that
/// a program probing for any of the report's paths gets a coherent answer.
fn seed_startup_scripts(report: &mut Report) {
    seed_file("SYS:S/Startup-Sequence", STARTUP_SEQUENCE, report);
    seed_file("SYS:S/User-Startup", USER_STARTUP, report);
    seed_file("SYS:S/Shell-Startup", SHELL_STARTUP, report);
}

/// Write one seeded text file, counting the outcome.
///
/// A fresh AmigaOS boots into a filesystem with content — AmigaOS used to
/// drop a `ReadMe` into `RAM:T` — and a VelaOS shell whose `catalog` has no
/// files in it leaves its file commands (`cat`, redirection-adjacent paths)
/// more hypothetical than demonstrable. Their absence is no failure:
/// `SYS:` and `RAM:` are ramfs-backed, so every boot starts empty anyway
/// and re-seeding is idempotent. Opening in write mode creates the file,
/// so the parent directory only has to exist — which is what the trees
/// above guarantee.
fn seed_file(path: &str, text: &str, report: &mut Report) {
    let Ok(file) = api::open(path, api::mode::WRITE) else {
        report.failures += 1;
        return;
    };
    let written = api::write(file, text.as_bytes());
    // The handle must be closed whether the write worked or not: a computer
    // that reboots cannot leak handles, but the *code* should not depend on
    // that.
    let _ = api::close(file);
    match written {
        Ok(_) => {}
        Err(_) => report.failures += 1,
    }
}

/// The text `run` drops into `SYS:S/Startup-Sequence`.
///
/// AmigaOS's `Startup-Sequence` is one long script that brings up
/// assignments, mounts, clocks and finally the desktop. VelaOS's kernel
/// already did the equivalents — memory, drivers, DOS, the mounts — before
/// this program ran, so the script records what order the work completed
/// in and what comes next, in the form AmigaOS and AROS both use: one
/// shell command per line, `;` for a comment.
const STARTUP_SEQUENCE: &str = "; VelaOS Startup-Sequence\n\
; Executed at boot, after the kernel has built the namespace.\n\
;\n\
; What the kernel did before this file was reachable:\n\
;   exec.library, the windowing system and the mounts were made ready\n\
;   in memory, and `init` created SYS: and RAM:.\n\
;\n\
; What runs here in later boots. One command per line:\n\
;   SetEnv  boot  ; copy ENV: into ENVARC:\n\
;   LoadWB      ; start the Workbench desktop\n";

/// The text `run` drops into `SYS:S/User-Startup`.
const USER_STARTUP: &str = "; VelaOS User-Startup\n\
; Run from Startup-Sequence, for the user's own additions.\n\
; Empty by default, as on AmigaOS.\n";

/// The text `run` drops into `SYS:S/Shell-Startup`.
///
/// A shell's own startup script, run before its first prompt: AmigaDOS
/// shells read `S:Shell-Startup` for the variables and assigns a session
/// wants to begin with. This one is written in the syntax the shell's
/// `Execute` actually runs, so it is a working script a user can point at,
/// not a decoration.
const SHELL_STARTUP: &str = "; VelaOS Shell-Startup\n\
; Run by the shell before its first prompt.\n\
;\n\
; The session's own assign, pointing at the user's work area.\n\
IF EXISTS SYS:Work\n\
  Assign WORK: SYS:Work\n\
ENDIF\n\
;\n\
; A variable every script can read.\n\
Set Editor Sys:Utilities/MuchMore\n";

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
        assert!(all.contains(&"SYS:System"));
        assert!(all.contains(&"SYS:Workbench"));
        assert!(all.contains(&"SYS:Prefs/Env-Archive"));
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
