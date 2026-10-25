//! The in-memory filesystem.
//!
//! # What it is
//!
//! `RAM:` is the AmigaDOS volume every program can write to and every
//! program can throw away. It is the natural place for a temporary file, and
//! it is where `ENV:` lives. On VelaOS it is a real filesystem held in
//! memory, not a special case in the VFS.
//!
//! # The rules it implements
//!
//! * **Case-insensitive lookup, case-preserving display.** The lookup folds
//!   case; the stored name keeps the case it was created with. This is the
//!   AmigaDOS rule, and it is the rule the report's §11.4 calls for.
//! * **Directories hold names, not paths.** A directory entry is a name and
//!   a kind, and the tree is walked by name. A file named `a/b` is not
//!   possible: a name may not contain a separator.
//! * **Timestamps are recorded.** A file's modification time is set when it
//!   is written.
//! * **Protection bits are recorded**, even though nothing enforces them
//!   yet — the bits are part of the metadata a program can read.
//!
//! # What it does not implement
//!
//! No hard links, no symbolic links that dangle, and no quotas. The
//! filesystem's size is reported as "unknown" rather than as a number,
//! because a ramfs grows with the heap and there is no fixed ceiling to
//! report.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use vela_kernel_vfs::{errors, DirEntry, FileSystem, NodeInfo, NodeKind};

/// A file's contents and metadata.
#[derive(Debug, Clone, Default)]
struct File {
    /// The file's contents.
    data: Vec<u8>,
    /// The file's modification time.
    mtime: u64,
    /// The file's protection bits.
    protection: u32,
    /// The file's comment.
    comment: String,
}

/// A directory's contents.
#[derive(Debug, Clone, Default)]
struct Dir {
    /// The entries, in insertion order.
    ///
    /// The order is the order the entries were created, because that is
    /// what makes a directory listing reproducible — a program that lists a
    /// directory twice sees the same order both times.
    entries: Vec<(String, Entry)>,
}

/// One directory entry.
#[derive(Debug, Clone)]
enum Entry {
    /// A subdirectory.
    Dir(Dir),
    /// A file.
    File(File),
}

impl Entry {
    fn kind(&self) -> NodeKind {
        match self {
            Self::Dir(_) => NodeKind::Directory,
            Self::File(_) => NodeKind::File,
        }
    }
}

/// The in-memory filesystem.
pub struct RamFs {
    /// The root directory.
    root: Mutex<Dir>,
    /// The clock, so that timestamps are monotonic within a run.
    clock: AtomicU64,
}

impl core::fmt::Debug for RamFs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RamFs")
            .field("root", &self.root.lock().entries.len())
            .finish()
    }
}

impl Default for RamFs {
    fn default() -> Self {
        Self::new()
    }
}

impl RamFs {
    /// A fresh, empty filesystem.
    pub const fn new() -> Self {
        Self {
            root: Mutex::new(Dir {
                entries: Vec::new(),
            }),
            clock: AtomicU64::new(0),
        }
    }

    /// The next timestamp.
    ///
    /// The clock counts from one rather than from the wall clock, because a
    /// ramfs has no relationship to real time and a *monotonic* counter is
    /// what the metadata contract actually needs.
    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Split a path into its components.
    ///
    /// The path is absolute within the filesystem. The empty path and `/`
    /// both name the root.
    fn components(path: &str) -> Vec<&str> {
        path.split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect()
    }

    /// Find an entry by name, folding case.
    ///
    /// This is the case-insensitive lookup rule. The *stored* name is
    /// returned separately by the caller, so that the display case is
    /// preserved.
    fn find<'a>(dir: &'a Dir, name: &str) -> Option<&'a str> {
        dir.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(n, _)| n.as_str())
    }

    /// Find an entry by name, folding case, mutably.
    fn find_mut<'a>(dir: &'a mut Dir, name: &str) -> Option<&'a mut Entry> {
        dir.entries
            .iter_mut()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, e)| e)
    }

    /// Walk to a node's parent directory.
    ///
    /// `parents` is the list of components *above* the node, so every one of
    /// them is walked. The caller has already split the node's own name
    /// off.
    fn parent_of<'a>(root: &'a mut Dir, parents: &[&str]) -> Result<&'a mut Dir, i32> {
        let mut current = root;
        for component in parents {
            let found = Self::find_mut(current, component).ok_or(errors::PATH_NOT_FOUND)?;
            match found {
                Entry::Dir(d) => current = d,
                Entry::File(_) => return Err(errors::FILE_NOT_OBJECT),
            }
        }
        Ok(current)
    }

    /// Walk to a node.
    fn node_of<'a>(root: &'a mut Dir, components: &[&str]) -> Result<&'a mut Entry, i32> {
        let (last, parents) = match components.split_last() {
            Some((last, parents)) => (last, parents),
            None => return Err(errors::PATH_NOT_FOUND),
        };
        let parent = Self::parent_of(root, parents)?;
        Self::find_mut(parent, last).ok_or(errors::PATH_NOT_FOUND)
    }
}

impl FileSystem for RamFs {
    fn name(&self) -> &str {
        "ramfs"
    }

    fn lookup(&self, path: &str) -> Result<NodeInfo, i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        if components.is_empty() {
            let mut info = NodeInfo::directory("/");
            info.size = root.entries.len() as u64;
            return Ok(info);
        }
        let (last, parents) = components.split_last().unwrap();
        // The *stored* name is the one that is displayed, so it is the one
        // the caller sees — not the spelling the caller happened to use.
        let parent = Self::parent_of(&mut root, parents)?;
        let stored = Self::find(parent, last)
            .ok_or(errors::PATH_NOT_FOUND)?
            .to_string();
        let entry = Self::find_mut(parent, last).ok_or(errors::PATH_NOT_FOUND)?;
        Ok(match entry {
            Entry::Dir(d) => {
                let mut info = NodeInfo::directory(&stored);
                info.size = d.entries.len() as u64;
                info
            }
            Entry::File(f) => {
                let mut info = NodeInfo::file(&stored, f.data.len() as u64);
                info.mtime = f.mtime;
                info.protection = f.protection;
                info.comment = f.comment.clone();
                info
            }
        })
    }

    fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        let dir = if components.is_empty() {
            &mut *root
        } else {
            match Self::node_of(&mut root, &components) {
                Ok(Entry::Dir(d)) => d,
                Ok(Entry::File(_)) => return Err(errors::FILE_NOT_OBJECT),
                Err(e) => return Err(e),
            }
        };
        Ok(dir
            .entries
            .iter()
            .map(|(name, entry)| DirEntry {
                name: name.clone(),
                kind: entry.kind(),
                size: match entry {
                    Entry::Dir(d) => d.entries.len() as u64,
                    Entry::File(f) => f.data.len() as u64,
                },
            })
            .collect())
    }

    fn read_all(&self, path: &str) -> Result<Vec<u8>, i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        match Self::node_of(&mut root, &components) {
            Ok(Entry::File(f)) => Ok(f.data.clone()),
            Ok(Entry::Dir(_)) => Err(errors::FILE_NOT_OBJECT),
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        match Self::node_of(&mut root, &components) {
            Ok(Entry::File(f)) => {
                let start = (offset as usize).min(f.data.len());
                let end = (start + len).min(f.data.len());
                Ok(f.data[start..end].to_vec())
            }
            Ok(Entry::Dir(_)) => Err(errors::FILE_NOT_OBJECT),
            Err(e) => Err(e),
        }
    }

    fn write_all(&self, path: &str, data: &[u8]) -> Result<(), i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        if components.is_empty() {
            // The root cannot be written as a file.
            return Err(errors::FILE_NOT_OBJECT);
        }
        let mtime = self.tick();
        let (last, parents) = components.split_last().unwrap();
        let parent = Self::parent_of(&mut root, parents)?;
        match Self::find_mut(parent, last) {
            Some(entry) => match entry {
                Entry::File(f) => {
                    f.data.clear();
                    f.data.extend_from_slice(data);
                    f.mtime = mtime;
                    Ok(())
                }
                Entry::Dir(_) => Err(errors::FILE_NOT_OBJECT),
            },
            None => {
                parent.entries.push((
                    last.to_string(),
                    Entry::File(File {
                        data: data.to_vec(),
                        mtime,
                        protection: 0,
                        comment: String::new(),
                    }),
                ));
                Ok(())
            }
        }
    }

    fn write_at(&self, path: &str, offset: u64, data: &[u8]) -> Result<(), i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        if components.is_empty() {
            return Err(errors::FILE_NOT_OBJECT);
        }
        let mtime = self.tick();
        let (last, parents) = components.split_last().unwrap();
        let parent = Self::parent_of(&mut root, parents)?;
        match Self::find_mut(parent, last) {
            Some(entry) => match entry {
                Entry::File(f) => {
                    let start = offset as usize;
                    if f.data.len() < start {
                        // Writing past the end extends the file with zeroes,
                        // which is what a seek-then-write does.
                        f.data.resize(start, 0);
                    }
                    let end = start + data.len();
                    if f.data.len() < end {
                        f.data.resize(end, 0);
                    }
                    f.data[start..end].copy_from_slice(data);
                    f.mtime = mtime;
                    Ok(())
                }
                Entry::Dir(_) => Err(errors::FILE_NOT_OBJECT),
            },
            None => {
                let mut file = File {
                    data: Vec::new(),
                    mtime,
                    protection: 0,
                    comment: String::new(),
                };
                let start = offset as usize;
                file.data.resize(start, 0);
                file.data.extend_from_slice(data);
                parent.entries.push((last.to_string(), Entry::File(file)));
                Ok(())
            }
        }
    }

    fn create_dir(&self, path: &str) -> Result<(), i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        if components.is_empty() {
            // The root already exists.
            return Ok(());
        }
        let (last, parents) = components.split_last().unwrap();
        let parent = Self::parent_of(&mut root, parents)?;
        if Self::find(parent, last).is_some() {
            // Creating a directory that exists is not an error on AmigaOS —
            // `MakeDir` on an existing directory returns success.
            return Ok(());
        }
        parent
            .entries
            .push((last.to_string(), Entry::Dir(Dir::default())));
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<(), i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        if components.is_empty() {
            // The root cannot be removed.
            return Err(errors::INVALID_NAME);
        }
        let (last, parents) = components.split_last().unwrap();
        let parent = Self::parent_of(&mut root, parents)?;
        let Some(index) = parent
            .entries
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(last))
        else {
            return Err(errors::PATH_NOT_FOUND);
        };
        parent.entries.remove(index);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), i32> {
        let mut root = self.root.lock();
        let from_components = Self::components(from);
        let to_components = Self::components(to);
        if from_components.is_empty() || to_components.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        // Take the entry out of its old place.
        let (from_last, from_parents) = from_components.split_last().unwrap();
        let source_parent = Self::parent_of(&mut root, from_parents)?;
        let Some(index) = source_parent
            .entries
            .iter()
            .position(|(n, _)| n.eq_ignore_ascii_case(from_last))
        else {
            return Err(errors::PATH_NOT_FOUND);
        };
        let (_, entry) = source_parent.entries.remove(index);

        // And put it in the new place.
        let (to_last, to_parents) = to_components.split_last().unwrap();
        let dest_parent = Self::parent_of(&mut root, to_parents)?;
        dest_parent.entries.push((to_last.to_string(), entry));
        Ok(())
    }

    fn set_comment(&self, path: &str, comment: &str) -> Result<(), i32> {
        let mut root = self.root.lock();
        let components = Self::components(path);
        match Self::node_of(&mut root, &components) {
            Ok(Entry::File(f)) => {
                f.comment = comment.to_string();
                Ok(())
            }
            Ok(Entry::Dir(_)) => Err(errors::FILE_NOT_OBJECT),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn fs() -> RamFs {
        RamFs::new()
    }

    #[test]
    fn a_new_filesystem_is_empty() {
        let f = fs();
        assert_eq!(f.read_dir("/").unwrap().len(), 0);
        assert!(f.lookup("/nothing").is_err());
    }

    #[test]
    fn a_file_can_be_written_and_read_back() {
        let f = fs();
        f.write_all("/hello.txt", b"hello").unwrap();
        assert_eq!(f.read_all("/hello.txt").unwrap(), b"hello".to_vec());
        assert_eq!(f.lookup("/hello.txt").unwrap().size, 5);
    }

    #[test]
    fn a_write_replaces_the_previous_contents() {
        let f = fs();
        f.write_all("/a.txt", b"first").unwrap();
        f.write_all("/a.txt", b"second").unwrap();
        assert_eq!(f.read_all("/a.txt").unwrap(), b"second".to_vec());
    }

    #[test]
    fn a_file_is_created_its_parent_must_exist() {
        let f = fs();
        assert!(f.write_all("/missing/a.txt", b"x").is_err());
    }

    #[test]
    fn directories_can_be_created() {
        let f = fs();
        f.create_dir("/dir").unwrap();
        f.write_all("/dir/a.txt", b"x").unwrap();
        assert_eq!(f.read_all("/dir/a.txt").unwrap(), b"x".to_vec());
    }

    #[test]
    fn creating_an_existing_directory_is_not_an_error() {
        // AmigaOS's `MakeDir` succeeds on an existing directory.
        let f = fs();
        f.create_dir("/dir").unwrap();
        assert!(f.create_dir("/dir").is_ok());
    }

    #[test]
    fn a_lookup_folds_case() {
        let f = fs();
        f.write_all("/Hello.txt", b"x").unwrap();
        assert!(f.lookup("/hello.txt").is_ok());
        assert!(f.lookup("/HELLO.TXT").is_ok());
    }

    #[test]
    fn a_lookup_preserves_the_display_case() {
        // The rule is *case-insensitive lookup, case-preserving display*.
        let f = fs();
        f.write_all("/Hello.txt", b"x").unwrap();
        let info = f.lookup("/hello.TXT").unwrap();
        assert_eq!(info.name, "Hello.txt");
    }

    #[test]
    fn a_directory_listing_keeps_the_creation_order() {
        let f = fs();
        f.create_dir("/d").unwrap();
        f.write_all("/d/zebra", b"").unwrap();
        f.write_all("/d/apple", b"").unwrap();
        f.write_all("/d/mango", b"").unwrap();
        let names: Vec<String> = f
            .read_dir("/d")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["zebra", "apple", "mango"]);
    }

    #[test]
    fn a_directory_listing_reports_the_kinds() {
        let f = fs();
        f.create_dir("/d").unwrap();
        f.create_dir("/d/sub").unwrap();
        f.write_all("/d/file", b"").unwrap();
        // The listing shows the directory's *contents*: the subdirectory and
        // the file, in creation order.
        let entries = f.read_dir("/d").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "sub");
        assert_eq!(entries[0].kind, NodeKind::Directory);
        assert_eq!(entries[1].name, "file");
        assert_eq!(entries[1].kind, NodeKind::File);
    }

    #[test]
    fn a_file_can_be_removed() {
        let f = fs();
        f.write_all("/a.txt", b"x").unwrap();
        f.remove("/a.txt").unwrap();
        assert!(f.lookup("/a.txt").is_err());
        // And removing it again is an error.
        assert!(f.remove("/a.txt").is_err());
    }

    #[test]
    fn a_removed_name_frees_the_slot_for_a_new_file() {
        let f = fs();
        f.write_all("/a.txt", b"one").unwrap();
        f.remove("/a.txt").unwrap();
        f.write_all("/a.txt", b"two").unwrap();
        assert_eq!(f.read_all("/a.txt").unwrap(), b"two".to_vec());
    }

    #[test]
    fn a_file_can_be_renamed() {
        let f = fs();
        f.write_all("/old.txt", b"data").unwrap();
        f.rename("/old.txt", "/new.txt").unwrap();
        assert!(f.lookup("/old.txt").is_err());
        assert_eq!(f.read_all("/new.txt").unwrap(), b"data".to_vec());
    }

    #[test]
    fn a_rename_keeps_the_new_name_case() {
        let f = fs();
        f.write_all("/old.txt", b"data").unwrap();
        f.rename("/old.txt", "/New.txt").unwrap();
        assert_eq!(f.lookup("/new.txt").unwrap().name, "New.txt");
    }

    #[test]
    fn a_partial_read_reads_only_what_was_asked_for() {
        let f = fs();
        f.write_all("/a.txt", b"0123456789").unwrap();
        assert_eq!(f.read_at("/a.txt", 2, 3).unwrap(), b"234".to_vec());
        assert_eq!(f.read_at("/a.txt", 8, 5).unwrap(), b"89".to_vec());
    }

    #[test]
    fn a_partial_read_past_the_end_returns_what_is_there() {
        let f = fs();
        f.write_all("/a.txt", b"abc").unwrap();
        assert_eq!(f.read_at("/a.txt", 1, 100).unwrap(), b"bc".to_vec());
        assert_eq!(f.read_at("/a.txt", 100, 10).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn a_write_at_an_offset_overwrites_in_place() {
        let f = fs();
        f.write_all("/a.txt", b"aaaa").unwrap();
        f.write_at("/a.txt", 1, b"bb").unwrap();
        assert_eq!(f.read_all("/a.txt").unwrap(), b"abba".to_vec());
    }

    #[test]
    fn a_write_past_the_end_extends_the_file() {
        let f = fs();
        f.write_all("/a.txt", b"ab").unwrap();
        f.write_at("/a.txt", 4, b"z").unwrap();
        let data = f.read_all("/a.txt").unwrap();
        assert_eq!(data.len(), 5);
        assert_eq!(&data[0..2], b"ab");
        assert_eq!(data[4], b'z');
        // The gap is zeroed.
        assert_eq!(data[2], 0);
    }

    #[test]
    fn a_timestamps_is_recorded_and_changes_on_write() {
        let f = fs();
        f.write_all("/a.txt", b"x").unwrap();
        let first = f.lookup("/a.txt").unwrap().mtime;
        assert!(first > 0, "a written file has a timestamp");
        f.write_all("/a.txt", b"y").unwrap();
        let second = f.lookup("/a.txt").unwrap().mtime;
        assert!(second > first, "a rewrite updates the timestamp");
    }

    #[test]
    fn a_comment_can_be_stored_and_read_back() {
        let f = fs();
        f.write_all("/a.txt", b"x").unwrap();
        f.set_comment("/a.txt", "a description").unwrap();
        assert_eq!(f.lookup("/a.txt").unwrap().comment, "a description");
    }

    #[test]
    fn reading_a_directory_as_a_file_is_refused() {
        let f = fs();
        f.create_dir("/d").unwrap();
        assert_eq!(f.read_all("/d"), Err(errors::FILE_NOT_OBJECT));
    }

    #[test]
    fn the_root_cannot_be_removed() {
        let f = fs();
        assert!(f.remove("/").is_err());
    }

    #[test]
    fn a_deep_tree_works() {
        let f = fs();
        f.create_dir("/a").unwrap();
        f.create_dir("/a/b").unwrap();
        f.create_dir("/a/b/c").unwrap();
        f.write_all("/a/b/c/deep.txt", b"deep").unwrap();
        assert_eq!(f.read_all("/a/b/c/deep.txt").unwrap(), b"deep".to_vec());
    }

    #[test]
    fn the_filesystem_reports_its_name() {
        assert_eq!(fs().name(), "ramfs");
    }
}
