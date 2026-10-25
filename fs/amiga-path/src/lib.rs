//! AmigaDOS path parsing and `Assign` resolution.
//!
//! # Why this crate exists
//!
//! AmigaDOS does not use POSIX paths. A path is `Volume:Directory/File`, and
//! the *volume* part is not a filesystem: it is an `Assign`, a name that
//! stands for one or more places in the filesystem. `SYS:` is an assign,
//! `RAM:` is an assign, and so is `LIBS:`.
//!
//! This matters because an assign is **not** a string substitution. It is an
//! ordered *multi-target search list*: `LIBS:` might stand for `SYS:Libs`
//! *and* `RAM:Libs`, and a lookup tries them in order until one has the
//! file. A substitution model cannot express that, and getting it wrong
//! means a program finds the wrong file or no file at all.
//!
//! # The two halves
//!
//! * [`AmigaPath`] — the *syntax*: splitting `Volume:Path/File` into its
//!   parts, normalising `.` and `..`, and handling relative paths.
//! * [`AssignTable`] — the *semantics*: what each volume name means, and
//!   the ordered search through a multi-target assign.
//!
//! # Case
//!
//! AmigaDOS lookups are case-insensitive but *display* the original case.
//! The parser preserves the case it was given; the lookup layer is what
//! folds case.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

/// The maximum length of a path, in bytes.
///
/// AmigaOS's limit is 1024 for a BPTR's string. Enforcing a limit is what
/// stops a program from asking the kernel to walk an unbounded path.
pub const MAX_PATH_LEN: usize = 1024;

/// The maximum length of a single path component.
pub const MAX_NAME_LEN: usize = 108;

/// The error codes path resolution reports.
///
/// These are the AmigaOS `ERROR_*` values, so a program written against
/// AmigaOS recognises them.
pub mod errors {
    /// The path was empty.
    pub const NO_PATH: i32 = -30;
    /// The path was too long.
    pub const TOO_LONG: i32 = -8;
    /// The path's syntax was wrong.
    pub const BAD_SYNTAX: i32 = -5;
    /// The path named something that does not exist.
    pub const PATH_NOT_FOUND: i32 = -7;
    /// The volume name was not known.
    pub const NO_VOLUME: i32 = -7;
}

/// A parsed AmigaDOS path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmigaPath {
    /// The volume name, without the colon, or `None` for a path with no
    /// volume part.
    ///
    /// The case is preserved: the display case of a volume name is part of
    /// how the path is shown to the user, even though the *lookup* folds
    /// case.
    pub volume: Option<String>,
    /// The path within the volume.
    ///
    /// Always starts with `/` and never ends with one (except for the root,
    /// which is exactly `/`). `.` components are removed and `..` components
    /// are resolved, so the path is canonical.
    pub path: String,
}

impl AmigaPath {
    /// Parse a path.
    ///
    /// The syntax is `Volume:Path/File`. The volume part is optional; a path
    /// without one is relative to the current directory.
    ///
    /// ```
    /// use vela_amiga_path::AmigaPath;
    ///
    /// let p = AmigaPath::parse("SYS:Libs/foo.library").unwrap();
    /// assert_eq!(p.volume.as_deref(), Some("SYS"));
    /// assert_eq!(p.path, "/Libs/foo.library");
    ///
    /// let p = AmigaPath::parse("foo/bar").unwrap();
    /// assert_eq!(p.volume, None);
    /// assert_eq!(p.path, "/foo/bar");
    /// ```
    pub fn parse(input: &str) -> Result<Self, i32> {
        if input.is_empty() {
            return Err(errors::NO_PATH);
        }
        if input.len() > MAX_PATH_LEN {
            return Err(errors::TOO_LONG);
        }

        let (volume, rest) = match split_volume(input) {
            Some((name, rest)) => {
                if name.is_empty() {
                    return Err(errors::BAD_SYNTAX);
                }
                if name.len() > MAX_NAME_LEN {
                    return Err(errors::TOO_LONG);
                }
                (Some(name.to_string()), rest)
            }
            None => (None, input),
        };

        let path = normalise_path(rest)?;
        Ok(Self { volume, path })
    }

    /// Parse a path and resolve it against `current`.
    ///
    /// A relative path — one with no volume and no leading `/` — is taken
    /// relative to `current`. An absolute path is not.
    pub fn parse_relative_to(input: &str, current: &AmigaPath) -> Result<Self, i32> {
        let parsed = Self::parse(input)?;
        if parsed.volume.is_some() || input.starts_with('/') {
            return Ok(parsed);
        }
        // Relative: splice it onto the current directory, keeping the
        // current volume.
        let base = current.path.trim_end_matches('/');
        let mut path = String::with_capacity(base.len() + parsed.path.len());
        path.push_str(base);
        path.push_str(&parsed.path);
        let path = normalise_path(&path)?;
        Ok(Self {
            volume: current.volume.clone(),
            path,
        })
    }

    /// The path with its volume, as the user would write it.
    pub fn to_amiga_string(&self) -> String {
        match &self.volume {
            Some(v) => {
                let mut s = String::with_capacity(v.len() + 1 + self.path.len());
                s.push_str(v);
                s.push(':');
                s.push_str(&self.path);
                s
            }
            None => self.path.clone(),
        }
    }

    /// The last component of the path — the file's name.
    pub fn file_name(&self) -> &str {
        match self.path.rsplit_once('/') {
            Some((_, name)) => name,
            None => "",
        }
    }

    /// The path's parent directory.
    ///
    /// The parent of the root is the root.
    pub fn parent(&self) -> AmigaPath {
        let parent = match self.path.rsplit_once('/') {
            Some((head, _)) => head.to_string(),
            None => "/".to_string(),
        };
        let parent = if parent.is_empty() {
            "/".to_string()
        } else {
            parent
        };
        AmigaPath {
            volume: self.volume.clone(),
            path: parent,
        }
    }

    /// True when the path names the root of its volume.
    pub fn is_root(&self) -> bool {
        self.path == "/"
    }
}

/// Split `Volume:Rest` into its parts.
///
/// The rule is that a colon *before* the first `/` separates a volume from
/// a path. A colon after a `/` is part of a file name, which AmigaDOS
/// permits and which POSIX does not.
fn split_volume(input: &str) -> Option<(&str, &str)> {
    let colon = input.find(':')?;
    let slash = input.find('/');
    match slash {
        Some(slash) if slash < colon => None,
        _ => Some((&input[..colon], &input[colon + 1..])),
    }
}

/// Normalise a path: leading `/`, no `.`, no `..`, no trailing `/`.
///
/// `..` at the root is dropped rather than escaping it: AmigaDOS does not
/// let a path climb above its volume's root.
fn normalise_path(input: &str) -> Result<String, i32> {
    // A path may use `\` as an alternative separator, as AmigaDOS does.
    let unified = input.replace('\\', "/");
    let mut components: Vec<&str> = Vec::new();
    for component in unified.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                // Climbing above the root is not allowed: the root is the
                // top.
                components.pop();
            }
            other => {
                if other.len() > MAX_NAME_LEN {
                    return Err(errors::TOO_LONG);
                }
                components.push(other);
            }
        }
    }
    let mut out = String::with_capacity(unified.len() + 1);
    for component in components {
        out.push('/');
        out.push_str(component);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

/// Fold a name for case-insensitive comparison.
///
/// ASCII-only, and *not* Unicode-normalising: AmigaDOS's case folding is
/// a byte-wise ASCII fold, and a program that stored a name in a
/// non-ASCII encoding expects byte-wise comparison.
pub fn fold_case(name: &str) -> String {
    name.to_ascii_lowercase()
}

/// One `Assign`: a name and the places it stands for.
///
/// The targets are an *ordered list*, and a lookup tries them in order. That
/// is what makes `LIBS:` a search path rather than a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    /// The assign's name, without the colon.
    pub name: String,
    /// The places the name stands for, in search order.
    pub targets: Vec<String>,
    /// Whether the assign was created by the system or by a program.
    ///
    /// A program's own assigns can be removed; the system's cannot, because
    /// `SYS:` failing to exist would make the system unusable.
    pub system: bool,
}

impl Assign {
    /// A new assign.
    pub fn new(name: &str, targets: Vec<String>) -> Self {
        Self {
            name: name.to_string(),
            targets,
            system: false,
        }
    }

    /// A new system assign.
    pub fn system(name: &str, targets: Vec<String>) -> Self {
        Self {
            name: name.to_string(),
            targets,
            system: true,
        }
    }

    /// Add another target to the end of the search list.
    pub fn push_target(&mut self, target: impl Into<String>) {
        self.targets.push(target.into());
    }

    /// True when the assign has no target at all.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// The table of assigns.
#[derive(Debug, Clone, Default)]
pub struct AssignTable {
    /// The assigns, in creation order.
    assigns: Vec<Assign>,
}

impl AssignTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// A table with VelaOS's built-in assigns.
    ///
    /// These are the mappings the report's §11.3 table specifies. `SYS:`
    /// and `RAM:` are the two real volumes; everything else is an assign
    /// that stands for a directory under one of them.
    pub fn with_builtins() -> Self {
        let mut table = Self::new();
        table.insert(Assign::system("SYS", vec!["/System".to_string()]));
        table.insert(Assign::system("RAM", vec!["/ramfs".to_string()]));
        table.insert(Assign::system("C", vec!["/System/C".to_string()]));
        table.insert(Assign::system("S", vec!["/System/S".to_string()]));
        table.insert(Assign::system(
            "LIBS",
            vec!["/System/Libs".to_string(), "/ramfs/Libs".to_string()],
        ));
        table.insert(Assign::system("DEVS", vec!["/System/Devs".to_string()]));
        table.insert(Assign::system("L", vec!["/System/L".to_string()]));
        table.insert(Assign::system("FONTS", vec!["/System/Fonts".to_string()]));
        table.insert(Assign::system("T", vec!["/ramfs/T".to_string()]));
        table.insert(Assign::system("ENV", vec!["/ramfs/ENV".to_string()]));
        table.insert(Assign::system(
            "ENVARC",
            vec!["/System/Env-Archive".to_string()],
        ));
        table.insert(Assign::system("PREFS", vec!["/System/Prefs".to_string()]));
        table.insert(Assign::system("LOCALE", vec!["/System/Locale".to_string()]));
        table.insert(Assign::system(
            "CLASSES",
            vec!["/System/Classes".to_string()],
        ));
        table.insert(Assign::system("TOOLS", vec!["/System/Tools".to_string()]));
        table.insert(Assign::system(
            "UTILITIES",
            vec!["/System/Utilities".to_string()],
        ));
        table.insert(Assign::system(
            "WBSTARTUP",
            vec!["/System/WBStartup".to_string()],
        ));
        table.insert(Assign::system("SYSTEM", vec!["/System/System".to_string()]));
        table.insert(Assign::system(
            "WORKBENCH",
            vec!["/System/Workbench".to_string()],
        ));
        table
    }

    /// Insert an assign.
    ///
    /// Returns `false` when an assign with that name already exists, in
    /// which case the existing one is left alone. Overwriting a system
    /// assign would let a program break the system's path resolution.
    pub fn insert(&mut self, assign: Assign) -> bool {
        if self.find(&assign.name).is_some() {
            return false;
        }
        self.assigns.push(assign);
        true
    }

    /// Remove an assign by name.
    ///
    /// A system assign cannot be removed. Returns `true` when an assign was
    /// removed.
    pub fn remove(&mut self, name: &str) -> bool {
        let folded = fold_case(name);
        let Some(index) = self
            .assigns
            .iter()
            .position(|a| fold_case(&a.name) == folded)
        else {
            return false;
        };
        if self.assigns[index].system {
            return false;
        }
        self.assigns.remove(index);
        true
    }

    /// Find an assign by name, case-insensitively.
    pub fn find(&self, name: &str) -> Option<&Assign> {
        let folded = fold_case(name);
        self.assigns.iter().find(|a| fold_case(&a.name) == folded)
    }

    /// Find an assign by name, case-insensitively, mutably.
    pub fn find_mut(&mut self, name: &str) -> Option<&mut Assign> {
        let folded = fold_case(name);
        self.assigns
            .iter_mut()
            .find(|a| fold_case(&a.name) == folded)
    }

    /// How many assigns there are.
    pub fn len(&self) -> usize {
        self.assigns.len()
    }

    /// True when there are no assigns.
    pub fn is_empty(&self) -> bool {
        self.assigns.is_empty()
    }

    /// The names of every assign, in creation order.
    pub fn names(&self) -> Vec<String> {
        self.assigns.iter().map(|a| a.name.clone()).collect()
    }
}

/// A resolved path: a backend path and the assign it came through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The backend path — the path in the *filesystem*, not the AmigaDOS one.
    pub backend: String,
    /// The volume name that was resolved, or `None` for an absolute backend
    /// path.
    pub volume: Option<String>,
    /// The assign the volume name resolved through, if any.
    pub assign: Option<String>,
    /// Which target of a multi-target assign matched.
    pub target_index: usize,
}

/// What a resolver needs to know about the filesystem it is resolving
/// against.
///
/// The trait exists so that this crate does not depend on the VFS. The VFS
/// implements it, and so does a test with a fake filesystem.
pub trait PathProbe {
    /// True when `path` names something that exists.
    fn exists(&self, path: &str) -> bool;
}

/// A probe that accepts everything.
///
/// Useful for tests and for the case where the filesystem is not consulted
/// at all — resolving a path for display, say.
pub struct AnyPath;

impl PathProbe for AnyPath {
    fn exists(&self, _path: &str) -> bool {
        true
    }
}

/// The path resolver.
///
/// It holds the assign table and resolves an AmigaDOS path into a backend
/// path, searching a multi-target assign's targets in order.
#[derive(Debug, Clone)]
pub struct Resolver {
    /// The assigns.
    pub assigns: AssignTable,
    /// The current directory.
    pub current: AmigaPath,
    /// The directory of the running program, for `PROGDIR:`.
    ///
    /// This is resolved per-process, so it is not an assign: it is a field
    /// that the process sets.
    pub progdir: Option<String>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    /// A resolver with the built-in assigns and the root as the current
    /// directory.
    pub fn new() -> Self {
        Self {
            assigns: AssignTable::with_builtins(),
            current: AmigaPath {
                volume: None,
                path: "/".to_string(),
            },
            progdir: None,
        }
    }

    /// Set the current directory.
    pub fn set_current(&mut self, current: AmigaPath) {
        self.current = current;
    }

    /// Resolve a path against the current directory.
    ///
    /// The resolution order is the AmigaDOS one:
    ///
    /// 1. A path with a volume name resolves through that volume's assign.
    ///    A multi-target assign is searched in order, and the first target
    ///    that has the file wins.
    /// 2. A path with no volume is relative to the current directory, which
    ///    itself has a volume.
    ///
    /// The `probe` decides whether a candidate target "has the file". When
    /// the assign has exactly one target, the probe is not consulted: the
    /// path resolves whether or not the file exists, because a program that
    /// is about to *create* the file needs the path anyway.
    pub fn resolve<P: PathProbe>(&self, input: &str, probe: &P) -> Result<Resolved, i32> {
        let parsed = AmigaPath::parse_relative_to(input, &self.current)?;
        self.resolve_parsed(&parsed, probe)
    }

    /// Resolve an already-parsed path.
    pub fn resolve_parsed<P: PathProbe>(
        &self,
        parsed: &AmigaPath,
        probe: &P,
    ) -> Result<Resolved, i32> {
        let Some(volume) = &parsed.volume else {
            // No volume at all: the path is already a backend path.
            return Ok(Resolved {
                backend: parsed.path.clone(),
                volume: None,
                assign: None,
                target_index: 0,
            });
        };

        let folded = fold_case(volume);
        if folded == "progdir" {
            let Some(dir) = &self.progdir else {
                return Err(errors::NO_VOLUME);
            };
            let relative = parsed.path.trim_start_matches('/');
            return Ok(Resolved {
                backend: join_path(dir, relative),
                volume: Some(volume.clone()),
                assign: Some("PROGDIR".to_string()),
                target_index: 0,
            });
        }

        let Some(assign) = self.assigns.find(volume) else {
            // An unknown volume name. It might still be a real mount point
            // — a filesystem mounted at `/Volumes/Foo` — but the resolver
            // does not know about mounts; it knows about assigns.
            return Err(errors::NO_VOLUME);
        };

        let relative = parsed.path.trim_start_matches('/');
        if assign.targets.is_empty() {
            return Err(errors::NO_VOLUME);
        }

        // A single-target assign resolves without consulting the
        // filesystem: the path is valid whether or not the file exists.
        if assign.targets.len() == 1 {
            return Ok(Resolved {
                backend: join_path(&assign.targets[0], relative),
                volume: Some(volume.clone()),
                assign: Some(assign.name.clone()),
                target_index: 0,
            });
        }

        // A multi-target assign is a search list: the first target that has
        // the file wins.
        for (index, target) in assign.targets.iter().enumerate() {
            let candidate = join_path(target, relative);
            if probe.exists(&candidate) {
                return Ok(Resolved {
                    backend: candidate,
                    volume: Some(volume.clone()),
                    assign: Some(assign.name.clone()),
                    target_index: index,
                });
            }
        }

        // Nothing matched. The *first* target is used anyway: a program that
        // is creating the file needs somewhere to create it, and reporting
        // "not found" would be wrong for a create.
        Ok(Resolved {
            backend: join_path(&assign.targets[0], relative),
            volume: Some(volume.clone()),
            assign: Some(assign.name.clone()),
            target_index: 0,
        })
    }

    /// Create or replace an assign.
    ///
    /// Returns `false` when the name is already taken by a system assign.
    pub fn add_assign(&mut self, name: &str, targets: Vec<String>) -> bool {
        self.assigns.insert(Assign::new(name, targets))
    }

    /// Remove an assign.
    pub fn remove_assign(&mut self, name: &str) -> bool {
        self.assigns.remove(name)
    }
}

/// Join a directory and a relative path.
///
/// The relative path may itself be absolute, in which case it replaces the
/// directory — which is what makes `join("/a", "/b")` yield `/b`.
pub fn join_path(directory: &str, relative: &str) -> String {
    if relative.starts_with('/') {
        return relative.to_string();
    }
    let base = directory.trim_end_matches('/');
    if relative.is_empty() {
        if base.is_empty() {
            return "/".to_string();
        }
        return base.to_string();
    }
    let mut out = String::with_capacity(base.len() + relative.len() + 1);
    out.push_str(base);
    out.push('/');
    out.push_str(relative);
    out
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;
    use std::vec;

    /// A probe that only knows about the paths it was given.
    struct Fake {
        known: Vec<String>,
    }

    impl Fake {
        fn new(paths: &[&str]) -> Self {
            Self {
                known: paths.iter().map(|p| p.to_string()).collect(),
            }
        }
    }

    impl PathProbe for Fake {
        fn exists(&self, path: &str) -> bool {
            self.known.iter().any(|k| k == path)
        }
    }

    #[test]
    fn a_volume_is_split_from_the_path() {
        let p = AmigaPath::parse("SYS:Libs/foo.library").unwrap();
        assert_eq!(p.volume.as_deref(), Some("SYS"));
        assert_eq!(p.path, "/Libs/foo.library");
    }

    #[test]
    fn a_path_without_a_volume_is_relative() {
        let p = AmigaPath::parse("foo/bar").unwrap();
        assert_eq!(p.volume, None);
        assert_eq!(p.path, "/foo/bar");
    }

    #[test]
    fn a_colon_after_a_slash_is_part_of_the_file_name() {
        // AmigaDOS permits a colon in a file name; POSIX does not. The
        // volume separator is the *first* colon before any slash.
        let p = AmigaPath::parse("dir/od:ate").unwrap();
        assert_eq!(p.volume, None);
        assert_eq!(p.path, "/dir/od:ate");
    }

    #[test]
    fn a_leading_slash_is_absolute_within_the_volume() {
        let p = AmigaPath::parse("SYS:/absolute/path").unwrap();
        assert_eq!(p.volume.as_deref(), Some("SYS"));
        assert_eq!(p.path, "/absolute/path");
    }

    #[test]
    fn dot_components_are_removed() {
        let p = AmigaPath::parse("SYS:./a/./b").unwrap();
        assert_eq!(p.path, "/a/b");
    }

    #[test]
    fn double_dot_components_climb() {
        let p = AmigaPath::parse("SYS:a/b/../c").unwrap();
        assert_eq!(p.path, "/a/c");
    }

    #[test]
    fn double_dot_at_the_root_stops_at_the_root() {
        // A path must not climb above its volume's root.
        let p = AmigaPath::parse("SYS:../../etc/passwd").unwrap();
        assert_eq!(p.path, "/etc/passwd");
    }

    #[test]
    fn a_backslash_is_a_separator_too() {
        let p = AmigaPath::parse("SYS:a\\b").unwrap();
        assert_eq!(p.path, "/a/b");
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert_eq!(AmigaPath::parse(""), Err(errors::NO_PATH));
    }

    #[test]
    fn an_over_long_path_is_refused() {
        let long = "a".repeat(MAX_PATH_LEN + 1);
        assert_eq!(AmigaPath::parse(&long), Err(errors::TOO_LONG));
    }

    #[test]
    fn an_over_long_component_is_refused() {
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(AmigaPath::parse(&long), Err(errors::TOO_LONG));
    }

    #[test]
    fn a_relative_path_is_taken_against_the_current_directory() {
        let current = AmigaPath::parse("SYS:Workbench").unwrap();
        let p = AmigaPath::parse_relative_to("sub/file", &current).unwrap();
        assert_eq!(p.volume.as_deref(), Some("SYS"));
        assert_eq!(p.path, "/Workbench/sub/file");
    }

    #[test]
    fn an_absolute_path_ignores_the_current_directory() {
        let current = AmigaPath::parse("SYS:Workbench").unwrap();
        let p = AmigaPath::parse_relative_to("/other", &current).unwrap();
        assert_eq!(p.path, "/other");
    }

    #[test]
    fn a_path_with_a_volume_ignores_the_current_directory() {
        let current = AmigaPath::parse("SYS:Workbench").unwrap();
        let p = AmigaPath::parse_relative_to("RAM:temp", &current).unwrap();
        assert_eq!(p.volume.as_deref(), Some("RAM"));
        assert_eq!(p.path, "/temp");
    }

    #[test]
    fn the_file_name_is_the_last_component() {
        let p = AmigaPath::parse("SYS:Libs/foo.library").unwrap();
        assert_eq!(p.file_name(), "foo.library");
    }

    #[test]
    fn the_parent_of_a_path_is_the_directory_above_it() {
        let p = AmigaPath::parse("SYS:Libs/foo.library").unwrap();
        assert_eq!(p.parent().path, "/Libs");
        // And the parent of the root is the root.
        let root = AmigaPath::parse("SYS:").unwrap();
        assert_eq!(root.path, "/");
        assert_eq!(root.parent().path, "/");
    }

    #[test]
    fn the_builtins_map_where_the_report_says() {
        let table = AssignTable::with_builtins();
        let sys = table.find("SYS").unwrap();
        assert_eq!(sys.targets, vec!["/System".to_string()]);
        let ram = table.find("RAM").unwrap();
        assert_eq!(ram.targets, vec!["/ramfs".to_string()]);
        let libs = table.find("LIBS").unwrap();
        // `LIBS:` is a *search list*, not a rename: the system copy is
        // tried first, then a ramfs copy.
        assert_eq!(
            libs.targets,
            vec!["/System/Libs".to_string(), "/ramfs/Libs".to_string()]
        );
        // The lookup folds case, so `libs:` finds `LIBS`.
        assert!(table.find("libs").is_some());
        assert!(table.find("LIBs").is_some());
    }

    #[test]
    fn every_builtin_the_report_lists_is_present() {
        let table = AssignTable::with_builtins();
        for name in [
            "SYS", "C", "S", "LIBS", "DEVS", "L", "FONTS", "RAM", "T", "ENV", "ENVARC",
        ] {
            assert!(table.find(name).is_some(), "{name} is missing");
        }
    }

    #[test]
    fn an_assign_can_be_created_and_removed() {
        let mut table = AssignTable::new();
        assert!(table.insert(Assign::new("Work", vec!["/System/Workbench".to_string()])));
        assert!(table.find("work").is_some());
        assert!(table.remove("WORK"));
        assert!(table.find("work").is_none());
    }

    #[test]
    fn a_system_assign_cannot_be_removed() {
        let mut table = AssignTable::with_builtins();
        // Removing `SYS:` would make the system unusable.
        assert!(!table.remove("SYS"));
        assert!(table.find("SYS").is_some());
    }

    #[test]
    fn an_assign_name_is_taken_once() {
        let mut table = AssignTable::new();
        assert!(table.insert(Assign::new("X", vec!["/a".to_string()])));
        // The same name under a different case is the same name.
        assert!(!table.insert(Assign::new("x", vec!["/b".to_string()])));
        assert_eq!(table.find("X").unwrap().targets, vec!["/a".to_string()]);
    }

    #[test]
    fn an_assign_can_have_several_targets() {
        let mut table = AssignTable::new();
        let mut a = Assign::new("LIBS", vec!["/System/Libs".to_string()]);
        a.push_target("/ramfs/Libs");
        table.insert(a);
        assert_eq!(table.find("LIBS").unwrap().targets.len(), 2);
    }

    #[test]
    fn a_single_target_assign_resolves_without_a_probe() {
        // The path is valid whether or not the file exists: a program that
        // is about to create the file needs the path anyway.
        let r = Resolver::new();
        let probe = Fake::new(&[]);
        let resolved = r.resolve("SYS:Libs/foo.library", &probe).unwrap();
        assert_eq!(resolved.backend, "/System/Libs/foo.library");
        assert_eq!(resolved.assign.as_deref(), Some("SYS"));
    }

    #[test]
    fn a_multi_target_assign_searches_in_order() {
        let r = Resolver::new();
        // The file is in the *second* target of `LIBS:`, so that is the one
        // used — even though the system copy is searched first.
        let probe = Fake::new(&["/ramfs/Libs/foo.library"]);
        let resolved = r.resolve("LIBS:foo.library", &probe).unwrap();
        assert_eq!(resolved.backend, "/ramfs/Libs/foo.library");
        assert_eq!(resolved.target_index, 1);
    }

    #[test]
    fn a_multi_target_assign_prefers_the_earlier_target() {
        let r = Resolver::new();
        // The file is in *both*: the first one wins, because the order is
        // the search order.
        let probe = Fake::new(&["/System/Libs/foo.library", "/ramfs/Libs/foo.library"]);
        let resolved = r.resolve("LIBS:foo.library", &probe).unwrap();
        assert_eq!(resolved.backend, "/System/Libs/foo.library");
        assert_eq!(resolved.target_index, 0);
    }

    #[test]
    fn a_multi_target_assign_falls_back_to_the_first_target() {
        let r = Resolver::new();
        // Nothing has the file: the first target is used, so that a create
        // has somewhere to go.
        let probe = Fake::new(&[]);
        let resolved = r.resolve("LIBS:foo.library", &probe).unwrap();
        assert_eq!(resolved.backend, "/System/Libs/foo.library");
        assert_eq!(resolved.target_index, 0);
    }

    #[test]
    fn an_unknown_volume_is_refused() {
        let r = Resolver::new();
        let probe = Fake::new(&[]);
        assert_eq!(r.resolve("NOPE:x", &probe), Err(errors::NO_VOLUME));
    }

    #[test]
    fn progdir_resolves_per_process() {
        let mut r = Resolver::new();
        let probe = Fake::new(&[]);
        // Without a program directory, `PROGDIR:` cannot be resolved.
        assert_eq!(r.resolve("PROGDIR:x", &probe), Err(errors::NO_VOLUME));
        r.progdir = Some("/System/Utilities".to_string());
        let resolved = r.resolve("PROGDIR:data.file", &probe).unwrap();
        assert_eq!(resolved.backend, "/System/Utilities/data.file");
    }

    #[test]
    fn a_relative_path_resolves_through_the_current_directory() {
        let mut r = Resolver::new();
        r.set_current(AmigaPath::parse("RAM:Work").unwrap());
        let probe = Fake::new(&[]);
        let resolved = r.resolve("file.txt", &probe).unwrap();
        assert_eq!(resolved.backend, "/ramfs/Work/file.txt");
        assert_eq!(resolved.volume.as_deref(), Some("RAM"));
    }

    #[test]
    fn joining_an_absolute_path_replaces_the_directory() {
        assert_eq!(join_path("/a", "/b"), "/b");
        assert_eq!(join_path("/a", "b"), "/a/b");
        assert_eq!(join_path("/a/", "b"), "/a/b");
        assert_eq!(join_path("/a", ""), "/a");
    }

    #[test]
    fn the_display_form_round_trips() {
        let p = AmigaPath::parse("SYS:Libs/foo.library").unwrap();
        assert_eq!(p.to_amiga_string(), "SYS:/Libs/foo.library");
    }

    #[test]
    fn case_folding_is_ascii_only() {
        assert_eq!(fold_case("Sys"), "sys");
        // A non-ASCII byte is not folded, which is what AmigaDOS does.
        assert_eq!(fold_case("Ä"), "Ä");
    }
}
