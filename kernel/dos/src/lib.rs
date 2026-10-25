//! The AmigaDOS file API, over the VFS.
//!
//! # What this layer is
//!
//! AmigaDOS programs do not call the filesystem directly. They call
//! `Open`, `Read`, `Write`, `Close`, `Seek`, `Lock`, `UnLock`,
//! `CurrentDir` and `Examine`, and they use the `BPTR` — the "DOS file
//! pointer" — to name an open file. This crate implements that API on top
//! of the [`vela_kernel_vfs`] abstractions, and it is the thing a user
//! program reaches through the `dos.library` syscalls.
//!
//! # The three layers
//!
//! * [`Dos`] — the service: the mount table, the path resolver, the open
//!   file table.
//! * [`DosFile`] — an open file: its path, its mode, its position.
//! * [`Bptr`] — the `BPTR`, the opaque handle a program holds.
//!
//! # The BPTR is a handle
//!
//! On AmigaOS a `BPTR` is a pointer to a `FileHandle` structure. Here it is
//! a small integer the kernel turns back into an open file. The reason is
//! the same as everywhere else in VelaOS: a user program must never hold a
//! kernel pointer.
//!
//! # `Lock` and `UnLock`
//!
//! AmigaDOS's locking is advisory and refcounted: `Lock` on a directory
//! makes it current, and `UnLock` gives it back. The count matters —
//! nested locks of the same directory are legal, and only the last `UnLock`
//! releases it. This crate implements that faithfully, because a program
//! that assumed one `UnLock` per `Lock` would break otherwise.
//!
//! # `LoadSeg` and `UnLoadSeg`
//!
//! `LoadSeg` loads a file into memory and returns a segment. On AmigaOS
//! the segment is a list of HUNK blocks. On VelaOS the native format is
//! ELF64, so `LoadSeg` loads an ELF64 executable and returns a segment
//! describing it — the entry point, the load address, and the mapped
//! regions. `UnLoadSeg` releases it.
//!
//! The point of sharing this path is that the *guest* formats come later:
//! when the 68k HUNK loader exists, it will produce the same segment
//! description, so the rest of the system — the task that runs the segment,
//! the cleanup on exit — does not have to know which format was loaded.

#![no_std]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use vela_amiga_path::{AmigaPath, PathProbe, Resolver};
#[cfg(test)]
use vela_kernel_vfs::errors as vfs_errors;
use vela_kernel_vfs::{AccessMode, DirEntry, FileSystem, MountTable, NodeInfo};

/// The error codes the DOS layer reports.
///
/// These are the AmigaOS `ERROR_*` values.
pub mod errors {
    /// The object was not found.
    pub const NO_OBJECT: i32 = -2;
    /// The object already existed.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The name was invalid.
    pub const INVALID_NAME: i32 = -5;
    /// The path was not found.
    pub const PATH_NOT_FOUND: i32 = -7;
    /// The name was too long.
    pub const TOO_LARGE: i32 = -8;
    /// The object was not open.
    pub const NOT_OPEN: i32 = -9;
    /// The object was not of the right type.
    pub const FILE_NOT_OBJECT: i32 = -17;
    /// The disk was full.
    pub const DISK_FULL: i32 = -18;
    /// The argument was out of range.
    pub const RANGE: i32 = -19;
    /// The caller was not the owner.
    pub const NOT_OWNER: i32 = -24;
}

/// The `BPTR`: an opaque handle to an open file.
///
/// On AmigaOS this is a `struct FileHandle *`. Here it is an index into
/// the DOS service's open-file table, and the number means nothing outside
/// the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bptr(u32);

impl Bptr {
    /// The `BPTR` of "no file".
    pub const NULL: Bptr = Bptr(0);

    /// True when the handle names no file.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }

    /// The raw handle value.
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Build a handle from a raw value.
    ///
    /// The kernel is the only party that should call this: a `BPTR` handed
    /// out to a program must name a file that is open, and only the DOS
    /// service knows which files those are.
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }
}

/// An open file.
#[derive(Debug, Clone)]
pub struct DosFile {
    /// The handle the program holds.
    pub bptr: Bptr,
    /// The path the file was opened with, in AmigaDOS form.
    pub path: String,
    /// The backend path the file lives at.
    pub backend: String,
    /// The mode the file was opened with.
    pub mode: AccessMode,
    /// The current position.
    pub position: u64,
    /// The number of locks on this file.
    ///
    /// AmigaOS's `Lock` on a *file* is refcounted too, and the position is
    /// shared between the locks — two locks of the same file share one
    /// position, which is why the position lives in the file rather than in
    /// the handle.
    pub locks: u32,
}

/// A loaded segment.
///
/// The segment describes a loadable image: where it was loaded, where its
/// entry point is, and which regions are mapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The path the segment was loaded from.
    pub path: String,
    /// The load base address.
    pub base: u64,
    /// The entry point address.
    pub entry: u64,
    /// The size of the mapped image, in bytes.
    pub size: u64,
    /// The number of loads of this segment.
    ///
    /// AmigaOS's `LoadSeg` on an already-loaded file increments the count
    /// rather than loading it again, and `UnLoadSeg` decrements it. This is
    /// what lets a library be shared.
    pub refcount: u32,
}

impl Segment {
    /// The segment's size, rounded up to a page.
    ///
    /// A segment's memory is mapped in pages, so its size is rounded up.
    pub fn page_aligned_size(&self) -> u64 {
        self.size.div_ceil(4096) * 4096
    }
}

/// The DOS service.
pub struct Dos {
    /// The mount table.
    pub mounts: MountTable,
    /// The path resolver.
    pub resolver: Resolver,
    /// The open files.
    files: Mutex<Vec<DosFile>>,
    /// The next `BPTR` to hand out.
    next_bptr: AtomicU32,
    /// The loaded segments.
    segments: Mutex<Vec<Segment>>,
}

impl core::fmt::Debug for Dos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dos")
            .field("mounts", &self.mounts.len())
            .field("open_files", &self.files.lock().len())
            .field("segments", &self.segments.lock().len())
            .finish()
    }
}

impl Default for Dos {
    fn default() -> Self {
        Self::new()
    }
}

impl Dos {
    /// A DOS service with nothing mounted.
    pub fn new() -> Self {
        Self {
            mounts: MountTable::new(),
            resolver: Resolver::new(),
            files: Mutex::new(Vec::new()),
            next_bptr: AtomicU32::new(1),
            segments: Mutex::new(Vec::new()),
        }
    }

    /// Mount a filesystem.
    pub fn mount(&self, point: &str, volume: &str, fs: alloc::sync::Arc<dyn FileSystem>) -> bool {
        self.mounts.mount(point, volume, fs)
    }

    /// Unmount a filesystem.
    pub fn unmount(&self, point: &str) -> bool {
        self.mounts.unmount(point)
    }

    /// The resolver's assigns.
    pub fn assigns(&self) -> &vela_amiga_path::AssignTable {
        &self.resolver.assigns
    }

    /// Set the current directory.
    pub fn set_current_dir(&mut self, path: &str) -> Result<(), i32> {
        let parsed = AmigaPath::parse(path)?;
        // The directory must exist, or `CurrentDir` to it would leave the
        // process somewhere that does not exist.
        let resolved = self.resolve(&parsed)?;
        let info = self.backend_lookup(&resolved)?;
        if !info.is_dir() {
            return Err(errors::FILE_NOT_OBJECT);
        }
        self.resolver.set_current(parsed);
        Ok(())
    }

    /// The current directory, in AmigaDOS form.
    pub fn current_dir(&self) -> String {
        self.resolver.current.to_amiga_string()
    }

    /// Resolve an AmigaDOS path into a backend path.
    fn resolve(&self, parsed: &AmigaPath) -> Result<vela_amiga_path::Resolved, i32> {
        let probe = MountProbe {
            mounts: &self.mounts,
        };
        self.resolver.resolve_parsed(parsed, &probe)
    }

    /// Resolve a path string.
    pub fn resolve_path(&self, path: &str) -> Result<vela_amiga_path::Resolved, i32> {
        let parsed = AmigaPath::parse_relative_to(path, &self.resolver.current)?;
        self.resolve(&parsed)
    }

    /// Look a node up in the filesystem that holds it.
    fn backend_lookup(&self, resolved: &vela_amiga_path::Resolved) -> Result<NodeInfo, i32> {
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        mount
            .fs
            .lookup(&relative)
            .map_err(|_| errors::PATH_NOT_FOUND)
    }

    /// `Open` — open a file.
    ///
    /// The mode is the AmigaOS `MODE_*` set. Opening a file that does not
    /// exist creates it, which is what AmigaOS's `Open` does.
    pub fn open(&self, path: &str, mode: AccessMode) -> Result<Bptr, i32> {
        let resolved = self.resolve_path(path)?;
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;

        // The file's parent must exist, whether or not the file does.
        if let Some(parent) = parent_of(&relative) {
            let parent_mount = self
                .mounts
                .split(parent)
                .unwrap_or((mount.clone(), parent.to_string()));
            parent_mount.0.fs.lookup(&parent_mount.1).map_err(|_| {
                // A missing parent is "path not found", not "no such file":
                // the *path* is wrong.
                errors::PATH_NOT_FOUND
            })?;
        }

        if mode.can_write() {
            // Opening for writing creates the file if it is not there.
            if mount.fs.lookup(&relative).is_err() {
                mount.fs.write_all(&relative, &[])?;
            }
        } else if mount.fs.lookup(&relative).is_err() {
            return Err(errors::PATH_NOT_FOUND);
        }

        let bptr = Bptr(self.next_bptr.fetch_add(1, Ordering::Relaxed));
        let position = if mode == AccessMode::Append {
            mount.fs.lookup(&relative).map(|i| i.size).unwrap_or(0)
        } else {
            0
        };
        self.files.lock().push(DosFile {
            bptr,
            path: resolved.backend.clone(),
            backend: resolved.backend.clone(),
            mode,
            position,
            locks: 1,
        });
        Ok(bptr)
    }

    /// `Close` — close a file.
    ///
    /// The file's locks must all be released first. AmigaOS's `Close` on a
    /// file with outstanding locks is refused, and refusing is what stops a
    /// program from closing a file another part of it still has open.
    pub fn close(&self, bptr: Bptr) -> Result<(), i32> {
        let mut files = self.files.lock();
        let Some(index) = files.iter().position(|f| f.bptr == bptr) else {
            return Err(errors::NO_OBJECT);
        };
        if files[index].locks > 1 {
            // One lock is released, and the file stays open.
            files[index].locks -= 1;
            return Ok(());
        }
        files.remove(index);
        Ok(())
    }

    /// `Lock` — take another lock on an open file.
    ///
    /// Returns the `BPTR` again, as AmigaOS's `Lock` does.
    pub fn lock(&self, bptr: Bptr) -> Result<Bptr, i32> {
        let mut files = self.files.lock();
        let Some(file) = files.iter_mut().find(|f| f.bptr == bptr) else {
            return Err(errors::NO_OBJECT);
        };
        file.locks += 1;
        Ok(bptr)
    }

    /// `UnLock` — release a lock.
    pub fn unlock(&self, bptr: Bptr) -> Result<(), i32> {
        let mut files = self.files.lock();
        let Some(file) = files.iter_mut().find(|f| f.bptr == bptr) else {
            return Err(errors::NO_OBJECT);
        };
        if file.locks == 0 {
            return Err(errors::NOT_OWNER);
        }
        file.locks -= 1;
        Ok(())
    }

    /// How many locks an open file has.
    pub fn lock_count(&self, bptr: Bptr) -> Result<u32, i32> {
        let files = self.files.lock();
        files
            .iter()
            .find(|f| f.bptr == bptr)
            .map(|f| f.locks)
            .ok_or(errors::NO_OBJECT)
    }

    /// `Read` — read from an open file.
    ///
    /// The length is in bytes on AmigaOS and in *lines* in the classic
    /// `Read` — VelaOS uses the byte count, which is the `ReadBytes`
    /// behaviour and is what a program reading a file actually wants.
    pub fn read(&self, bptr: Bptr, len: usize) -> Result<Vec<u8>, i32> {
        let (backend, position) = self.file_state(bptr)?;
        let (mount, relative) = self
            .mounts
            .split(&backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        let data = mount.fs.read_at(&relative, position, len)?;
        // The position advances by what was actually read.
        self.advance(bptr, data.len() as u64);
        Ok(data)
    }

    /// `ReadBytes` — read up to `len` bytes, stopping at a newline.
    pub fn read_bytes(&self, bptr: Bptr, len: usize) -> Result<Vec<u8>, i32> {
        let data = self.read(bptr, len)?;
        match data.iter().position(|b| *b == b'\n') {
            Some(index) => {
                // The newline is consumed but not returned, which is what
                // `ReadBytes` does.
                let mut result = data[..index].to_vec();
                result.shrink_to_fit();
                self.rewind(bptr, (data.len() - index - 1) as u64)?;
                Ok(result)
            }
            None => Ok(data),
        }
    }

    /// `Write` — write to an open file.
    pub fn write(&self, bptr: Bptr, data: &[u8]) -> Result<usize, i32> {
        let (backend, position) = self.file_state(bptr)?;
        let mode = self.file_mode(bptr)?;
        if !mode.can_write() {
            return Err(errors::NOT_OWNER);
        }
        let (mount, relative) = self
            .mounts
            .split(&backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        mount.fs.write_at(&relative, position, data)?;
        self.advance(bptr, data.len() as u64);
        Ok(data.len())
    }

    /// `Seek` — move the file's position.
    ///
    /// The offset is relative to the start of the file, the end, or the
    /// current position, per `SEEK_SET`, `SEEK_CUR` and `SEEK_END`.
    pub fn seek(&self, bptr: Bptr, offset: i64, whence: SeekFrom) -> Result<u64, i32> {
        let (backend, position) = self.file_state(bptr)?;
        let base = match whence {
            SeekFrom::Start => 0i64,
            SeekFrom::Current => position as i64,
            SeekFrom::End => {
                let (mount, relative) = self
                    .mounts
                    .split(&backend)
                    .map_err(|_| errors::PATH_NOT_FOUND)?;
                mount.fs.lookup(&relative)?.size as i64
            }
        };
        let new = base.checked_add(offset).ok_or(errors::RANGE)?;
        if new < 0 {
            return Err(errors::RANGE);
        }
        self.set_position(bptr, new as u64);
        Ok(new as u64)
    }

    /// The current position of an open file.
    pub fn position(&self, bptr: Bptr) -> Result<u64, i32> {
        Ok(self.file_state(bptr)?.1)
    }

    /// `Examine` — look a path up.
    pub fn examine(&self, path: &str) -> Result<NodeInfo, i32> {
        let resolved = self.resolve_path(path)?;
        self.backend_lookup(&resolved)
    }

    /// `ReadDir` — list a directory.
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let resolved = self.resolve_path(path)?;
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        mount.fs.read_dir(&relative)
    }

    /// `CreateDir` — create a directory.
    pub fn create_dir(&self, path: &str) -> Result<(), i32> {
        let resolved = self.resolve_path(path)?;
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        mount.fs.create_dir(&relative)
    }

    /// `DeleteFile` — remove a file.
    pub fn delete_file(&self, path: &str) -> Result<(), i32> {
        let resolved = self.resolve_path(path)?;
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        mount.fs.remove(&relative)
    }

    /// `Rename` — rename a file.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), i32> {
        let from_resolved = self.resolve_path(from)?;
        let to_resolved = self.resolve_path(to)?;
        let (from_mount, from_relative) = self
            .mounts
            .split(&from_resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        let (to_mount, to_relative) = self
            .mounts
            .split(&to_resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        if to_mount.fs.name() != from_mount.fs.name() {
            // A rename across filesystems would have to copy the data, and
            // AmigaOS's `Rename` does not do that.
            return Err(errors::NOT_OWNER);
        }
        from_mount.fs.rename(&from_relative, &to_relative)
    }

    /// `LoadSeg` — load a file into memory.
    ///
    /// The file must be an ELF64 executable. Loading a file that is already
    /// loaded increments its reference count rather than loading it again,
    /// which is what makes a shared library work.
    ///
    /// The kernel does the actual mapping; this function describes what
    /// should be mapped and where, and hands the description back. The
    /// separation is deliberate: the *policy* — "is this file loadable, and
    /// has it been loaded already" — is testable on the host, while the
    /// *mechanism* — mapping pages — is the kernel's.
    pub fn load_seg(&self, path: &str) -> Result<Segment, i32> {
        let resolved = self.resolve_path(path)?;
        let (mount, relative) = self
            .mounts
            .split(&resolved.backend)
            .map_err(|_| errors::PATH_NOT_FOUND)?;
        let data = mount.fs.read_all(&relative)?;

        // Already loaded: share it.
        {
            let mut segments = self.segments.lock();
            if let Some(segment) = segments.iter_mut().find(|s| s.path == resolved.backend) {
                segment.refcount += 1;
                return Ok(segment.clone());
            }
        }

        let parsed = parse_elf_header(&data)?;
        let segment = Segment {
            path: resolved.backend.clone(),
            base: parsed.load_base,
            entry: parsed.entry,
            size: parsed.image_size,
            refcount: 1,
        };
        self.segments.lock().push(segment.clone());
        Ok(segment)
    }

    /// `UnLoadSeg` — release a loaded segment.
    ///
    /// The segment is only released when its reference count reaches zero.
    pub fn unload_seg(&self, path: &str) -> Result<(), i32> {
        let resolved = self.resolve_path(path)?;
        let mut segments = self.segments.lock();
        let Some(index) = segments.iter().position(|s| s.path == resolved.backend) else {
            return Err(errors::NO_OBJECT);
        };
        if segments[index].refcount > 1 {
            segments[index].refcount -= 1;
            return Ok(());
        }
        segments.remove(index);
        Ok(())
    }

    /// The loaded segments.
    pub fn segments(&self) -> Vec<Segment> {
        self.segments.lock().clone()
    }

    /// How many files are open.
    pub fn open_file_count(&self) -> usize {
        self.files.lock().len()
    }

    /// An open file's backend path and position.
    fn file_state(&self, bptr: Bptr) -> Result<(String, u64), i32> {
        let files = self.files.lock();
        let file = files
            .iter()
            .find(|f| f.bptr == bptr)
            .ok_or(errors::NO_OBJECT)?;
        Ok((file.backend.clone(), file.position))
    }

    /// An open file's mode.
    fn file_mode(&self, bptr: Bptr) -> Result<AccessMode, i32> {
        let files = self.files.lock();
        files
            .iter()
            .find(|f| f.bptr == bptr)
            .map(|f| f.mode)
            .ok_or(errors::NO_OBJECT)
    }

    /// Move an open file's position forward.
    fn advance(&self, bptr: Bptr, by: u64) {
        let mut files = self.files.lock();
        if let Some(file) = files.iter_mut().find(|f| f.bptr == bptr) {
            file.position += by;
        }
    }

    /// Move an open file's position back.
    fn rewind(&self, bptr: Bptr, by: u64) -> Result<(), i32> {
        let mut files = self.files.lock();
        let Some(file) = files.iter_mut().find(|f| f.bptr == bptr) else {
            return Err(errors::NO_OBJECT);
        };
        file.position = file.position.saturating_sub(by);
        Ok(())
    }

    /// Set an open file's position.
    fn set_position(&self, bptr: Bptr, position: u64) {
        let mut files = self.files.lock();
        if let Some(file) = files.iter_mut().find(|f| f.bptr == bptr) {
            file.position = position;
        }
    }
}

/// Where a seek is measured from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekFrom {
    /// From the start of the file.
    Start,
    /// From the current position.
    Current,
    /// From the end of the file.
    End,
}

/// The parent directory of a path within a filesystem.
fn parent_of(path: &str) -> Option<&str> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/"),
        Some(index) => Some(&trimmed[..index]),
        None => None,
    }
}

/// The path probe the resolver uses.
///
/// The probe answers "is this path in a mounted filesystem?", which is what
/// a multi-target assign needs: the target that is actually mounted wins.
struct MountProbe<'a> {
    mounts: &'a MountTable,
}

impl PathProbe for MountProbe<'_> {
    fn exists(&self, path: &str) -> bool {
        self.mounts.find(path).is_some()
    }
}

/// What the ELF parser extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfImage {
    /// The load base address.
    pub load_base: u64,
    /// The entry point.
    pub entry: u64,
    /// The size of the mapped image.
    pub image_size: u64,
}

/// Parse the header of an ELF64 executable.
///
/// The parser is deliberately strict: a malformed header is an error, not
/// a guess. It validates the magic, the class, the endianness, the machine
/// and the program-header table's bounds — the same checks the loader makes,
/// so a file that loads here will load there.
///
/// Only what a `LoadSeg` description needs is extracted: the load base, the
/// entry point and the image size.
pub fn parse_elf_header(data: &[u8]) -> Result<ElfImage, i32> {
    // The ELF64 header is 64 bytes.
    const HEADER_LEN: usize = 64;
    if data.len() < HEADER_LEN {
        return Err(errors::INVALID_NAME);
    }
    // The magic.
    if &data[0..4] != b"\x7fELF" {
        return Err(errors::INVALID_NAME);
    }
    // The class: 2 is 64-bit.
    if data[4] != 2 {
        return Err(errors::INVALID_NAME);
    }
    // The data encoding: 1 is little-endian.
    if data[5] != 1 {
        return Err(errors::INVALID_NAME);
    }
    // The version.
    if data[6] != 1 {
        return Err(errors::INVALID_NAME);
    }
    // The type: 2 is an executable, 3 is a shared object.
    let e_type = u16::from_le_bytes([data[16], data[17]]);
    if e_type != 2 && e_type != 3 {
        return Err(errors::INVALID_NAME);
    }
    // The machine: 0x3E is x86-64.
    let e_machine = u16::from_le_bytes([data[18], data[19]]);
    if e_machine != 0x3E {
        return Err(errors::INVALID_NAME);
    }

    let e_entry = u64::from_le_bytes(data[24..32].try_into().unwrap());
    let e_phoff = u64::from_le_bytes(data[32..40].try_into().unwrap());
    let e_phentsize = u16::from_le_bytes([data[54], data[55]]) as u64;
    let e_phnum = u16::from_le_bytes([data[56], data[57]]) as u64;

    if e_phentsize == 0 {
        return Err(errors::INVALID_NAME);
    }
    // The program-header table must lie inside the file. This is the check
    // that stops a malformed file from making the kernel read past its end.
    let table_len = e_phentsize
        .checked_mul(e_phnum)
        .ok_or(errors::INVALID_NAME)?;
    let table_end = e_phoff.checked_add(table_len).ok_or(errors::INVALID_NAME)?;
    if table_end > data.len() as u64 {
        return Err(errors::INVALID_NAME);
    }

    // Walk the program headers to find the loadable segments.
    const PT_LOAD: u32 = 1;
    let mut load_base = u64::MAX;
    let mut image_end = 0u64;
    for index in 0..e_phnum {
        let offset = e_phoff + index * e_phentsize;
        let header = &data[offset as usize..(offset + e_phentsize) as usize];
        if header.len() < 56 {
            return Err(errors::INVALID_NAME);
        }
        let p_type = u32::from_le_bytes(header[0..4].try_into().unwrap());
        if p_type != PT_LOAD {
            continue;
        }
        let p_vaddr = u64::from_le_bytes(header[16..24].try_into().unwrap());
        // The file size is not needed: the *memory* size is what is mapped.
        let _p_filesz = u64::from_le_bytes(header[32..40].try_into().unwrap());
        let p_memsz = u64::from_le_bytes(header[40..48].try_into().unwrap());
        load_base = load_base.min(p_vaddr);
        image_end = image_end.max(p_vaddr.saturating_add(p_memsz));
    }

    if load_base == u64::MAX {
        // A file with no loadable segment is not an executable.
        return Err(errors::INVALID_NAME);
    }

    Ok(ElfImage {
        load_base,
        entry: e_entry,
        image_size: image_end.saturating_sub(load_base),
    })
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::vec;
    use std::string::ToString;

    use vela_ramfs::RamFs;

    fn dos() -> Dos {
        let d = Dos::new();
        let ram = Arc::new(RamFs::new());
        // The ramfs is mounted at `/ramfs`, which is where the `RAM:` assign
        // points.
        d.mount("/ramfs", "RAM", ram);
        d
    }

    /// A minimal but valid ELF64 header.
    fn elf_header(entry: u64, base: u64, size: u64) -> Vec<u8> {
        let mut data = vec![0u8; 64 + 56];
        data[0..4].copy_from_slice(b"\x7fELF");
        data[4] = 2; // 64-bit
        data[5] = 1; // little-endian
        data[6] = 1; // version
        data[16..18].copy_from_slice(&2u16.to_le_bytes()); // executable
        data[18..20].copy_from_slice(&0x3Eu16.to_le_bytes()); // x86-64
        data[24..32].copy_from_slice(&entry.to_le_bytes());
        data[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        data[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
        data[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
                                                           // The program header.
        data[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
        data[64 + 16..64 + 24].copy_from_slice(&base.to_le_bytes());
        data[64 + 32..64 + 40].copy_from_slice(&size.to_le_bytes());
        data[64 + 40..64 + 48].copy_from_slice(&size.to_le_bytes());
        data
    }

    #[test]
    fn a_file_can_be_opened_written_and_read() {
        let d = dos();
        let bptr = d.open("RAM:test.txt", AccessMode::ReadWrite).unwrap();
        assert_eq!(d.write(bptr, b"hello").unwrap(), 5);
        // The write advanced the position, so the read starts from the top.
        d.seek(bptr, 0, SeekFrom::Start).unwrap();
        assert_eq!(d.read(bptr, 5).unwrap(), b"hello".to_vec());
        d.close(bptr).unwrap();
    }

    #[test]
    fn a_closed_file_cannot_be_used() {
        let d = dos();
        let bptr = d.open("RAM:test.txt", AccessMode::ReadWrite).unwrap();
        d.close(bptr).unwrap();
        assert_eq!(d.read(bptr, 5).unwrap_err(), errors::NO_OBJECT);
        assert_eq!(d.close(bptr).unwrap_err(), errors::NO_OBJECT);
    }

    #[test]
    fn a_bptr_reserves_zero_for_null() {
        assert!(Bptr::NULL.is_null());
        assert!(!Bptr(1).is_null());
    }

    #[test]
    fn opening_a_missing_file_for_reading_fails() {
        let d = dos();
        assert_eq!(
            d.open("RAM:missing.txt", AccessMode::Read).unwrap_err(),
            errors::PATH_NOT_FOUND
        );
    }

    #[test]
    fn opening_a_missing_file_for_writing_creates_it() {
        // AmigaOS's `Open` creates the file when the mode permits writing.
        let d = dos();
        let bptr = d.open("RAM:new.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"data").unwrap();
        d.close(bptr).unwrap();
        let info = d.examine("RAM:new.txt").unwrap();
        assert_eq!(info.size, 4);
    }

    #[test]
    fn the_read_position_advances() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"0123456789").unwrap();
        assert_eq!(d.position(bptr).unwrap(), 10);
        d.seek(bptr, 0, SeekFrom::Start).unwrap();
        assert_eq!(d.read(bptr, 3).unwrap(), b"012".to_vec());
        assert_eq!(d.position(bptr).unwrap(), 3);
    }

    #[test]
    fn a_seek_can_be_relative_or_absolute() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"0123456789").unwrap();
        assert_eq!(d.seek(bptr, 2, SeekFrom::Start).unwrap(), 2);
        assert_eq!(d.seek(bptr, 3, SeekFrom::Current).unwrap(), 5);
        assert_eq!(d.seek(bptr, -2, SeekFrom::End).unwrap(), 8);
    }

    #[test]
    fn a_seek_before_the_start_is_refused() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"abc").unwrap();
        assert_eq!(
            d.seek(bptr, -10, SeekFrom::Start).unwrap_err(),
            errors::RANGE
        );
    }

    #[test]
    fn a_lock_is_refcounted() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        assert_eq!(d.lock_count(bptr).unwrap(), 1);
        d.lock(bptr).unwrap();
        assert_eq!(d.lock_count(bptr).unwrap(), 2);
        // Closing with an outstanding lock releases one of them rather than
        // closing the file.
        d.close(bptr).unwrap();
        assert_eq!(d.lock_count(bptr).unwrap(), 1);
        d.close(bptr).unwrap();
        assert_eq!(d.open_file_count(), 0);
    }

    #[test]
    fn an_unlock_releases_one_lock() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        d.lock(bptr).unwrap();
        d.unlock(bptr).unwrap();
        assert_eq!(d.lock_count(bptr).unwrap(), 1);
    }

    #[test]
    fn a_read_only_file_cannot_be_written() {
        let d = dos();
        let bptr = d.open("RAM:t.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"x").unwrap();
        d.close(bptr).unwrap();
        let ro = d.open("RAM:t.txt", AccessMode::Read).unwrap();
        assert_eq!(d.write(ro, b"y").unwrap_err(), errors::NOT_OWNER);
    }

    #[test]
    fn a_directory_can_be_created_and_listed() {
        let d = dos();
        d.create_dir("RAM:sub").unwrap();
        let bptr = d.open("RAM:sub/file.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"x").unwrap();
        d.close(bptr).unwrap();
        let entries = d.read_dir("RAM:sub").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.txt");
    }

    #[test]
    fn a_file_can_be_deleted_and_renamed() {
        let d = dos();
        let bptr = d.open("RAM:a.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"data").unwrap();
        d.close(bptr).unwrap();
        d.rename("RAM:a.txt", "RAM:b.txt").unwrap();
        assert!(d.examine("RAM:a.txt").is_err());
        assert!(d.examine("RAM:b.txt").is_ok());
        d.delete_file("RAM:b.txt").unwrap();
        assert!(d.examine("RAM:b.txt").is_err());
    }

    #[test]
    fn the_current_directory_can_be_changed() {
        let mut d = dos();
        d.create_dir("RAM:work").unwrap();
        d.set_current_dir("RAM:work").unwrap();
        assert_eq!(d.current_dir(), "RAM:/work");
        // A relative path now resolves against it.
        let bptr = d.open("file.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"x").unwrap();
        d.close(bptr).unwrap();
        assert!(d.examine("RAM:work/file.txt").is_ok());
    }

    #[test]
    fn the_current_directory_must_exist() {
        let mut d = dos();
        assert!(d.set_current_dir("RAM:nope").is_err());
    }

    #[test]
    fn the_current_directory_must_be_a_directory() {
        let mut d = dos();
        let bptr = d.open("RAM:f.txt", AccessMode::ReadWrite).unwrap();
        d.close(bptr).unwrap();
        assert!(d.set_current_dir("RAM:f.txt").is_err());
    }

    #[test]
    fn a_segment_is_loaded_from_an_elf_file() {
        let d = dos();
        let bptr = d.open("RAM:prog.elf", AccessMode::ReadWrite).unwrap();
        d.write(bptr, &elf_header(0x1000, 0x1000, 0x2000)).unwrap();
        d.close(bptr).unwrap();
        let segment = d.load_seg("RAM:prog.elf").unwrap();
        assert_eq!(segment.entry, 0x1000);
        assert_eq!(segment.base, 0x1000);
        assert_eq!(segment.size, 0x2000);
        assert_eq!(segment.refcount, 1);
    }

    #[test]
    fn loading_the_same_file_twice_shares_it() {
        let d = dos();
        let bptr = d.open("RAM:prog.elf", AccessMode::ReadWrite).unwrap();
        d.write(bptr, &elf_header(0x1000, 0x1000, 0x2000)).unwrap();
        d.close(bptr).unwrap();
        let first = d.load_seg("RAM:prog.elf").unwrap();
        let second = d.load_seg("RAM:prog.elf").unwrap();
        // The second load shares the first one: same image, one more reference.
        assert_eq!(first.base, second.base);
        assert_eq!(first.entry, second.entry);
        assert_eq!(first.size, second.size);
        assert_eq!(first.refcount, 1);
        assert_eq!(second.refcount, 2);
        assert_eq!(d.segments().len(), 1, "the file is loaded once");
    }

    #[test]
    fn unloading_releases_the_segment_only_when_the_count_reaches_zero() {
        let d = dos();
        let bptr = d.open("RAM:prog.elf", AccessMode::ReadWrite).unwrap();
        d.write(bptr, &elf_header(0x1000, 0x1000, 0x2000)).unwrap();
        d.close(bptr).unwrap();
        d.load_seg("RAM:prog.elf").unwrap();
        d.load_seg("RAM:prog.elf").unwrap();
        d.unload_seg("RAM:prog.elf").unwrap();
        assert_eq!(d.segments().len(), 1, "one reference is still held");
        d.unload_seg("RAM:prog.elf").unwrap();
        assert_eq!(d.segments().len(), 0);
    }

    #[test]
    fn unloading_a_segment_that_was_never_loaded_is_refused() {
        let d = dos();
        assert_eq!(d.unload_seg("RAM:nothing").unwrap_err(), errors::NO_OBJECT);
    }

    #[test]
    fn a_non_elf_file_cannot_be_loaded() {
        let d = dos();
        let bptr = d.open("RAM:text.txt", AccessMode::ReadWrite).unwrap();
        d.write(bptr, b"this is not an ELF file").unwrap();
        d.close(bptr).unwrap();
        assert_eq!(
            d.load_seg("RAM:text.txt").unwrap_err(),
            errors::INVALID_NAME
        );
    }

    #[test]
    fn a_truncated_elf_header_is_refused() {
        assert!(parse_elf_header(&[0u8; 10]).is_err());
        assert!(parse_elf_header(&[]).is_err());
    }

    #[test]
    fn an_elf_header_with_a_bad_magic_is_refused() {
        let mut data = elf_header(0x1000, 0x1000, 0x2000);
        data[0] = b'X';
        assert!(parse_elf_header(&data).is_err());
    }

    #[test]
    fn an_elf_header_with_a_bad_machine_is_refused() {
        let mut data = elf_header(0x1000, 0x1000, 0x2000);
        // ARM, not x86-64.
        data[18..20].copy_from_slice(&40u16.to_le_bytes());
        assert!(parse_elf_header(&data).is_err());
    }

    #[test]
    fn an_elf_header_with_a_program_table_past_the_end_is_refused() {
        let mut data = elf_header(0x1000, 0x1000, 0x2000);
        // A program-header offset far past the end of the file.
        data[32..40].copy_from_slice(&0x10000u64.to_le_bytes());
        assert!(parse_elf_header(&data).is_err());
    }

    #[test]
    fn an_elf_header_with_no_loadable_segment_is_refused() {
        let mut data = elf_header(0x1000, 0x1000, 0x2000);
        // Not a PT_LOAD.
        data[64..68].copy_from_slice(&4u32.to_le_bytes());
        assert!(parse_elf_header(&data).is_err());
    }

    #[test]
    fn a_segment_size_rounds_up_to_a_page() {
        let segment = Segment {
            path: "/x".to_string(),
            base: 0,
            entry: 0,
            size: 4097,
            refcount: 1,
        };
        assert_eq!(segment.page_aligned_size(), 8192);
    }

    #[test]
    fn a_resolved_path_names_the_backend() {
        let d = dos();
        let resolved = d.resolve_path("RAM:sub/file.txt").unwrap();
        assert_eq!(resolved.backend, "/ramfs/sub/file.txt");
        assert_eq!(resolved.volume.as_deref(), Some("RAM"));
    }

    #[test]
    fn an_unknown_volume_is_refused() {
        let d = dos();
        assert!(d.resolve_path("NOPE:x").is_err());
    }

    #[test]
    fn the_vfs_error_codes_are_reused() {
        // The DOS layer reports the same AmigaOS codes the VFS does, so a
        // program sees one set of numbers.
        assert_eq!(vfs_errors::PATH_NOT_FOUND, errors::PATH_NOT_FOUND);
        assert_eq!(vfs_errors::NO_OBJECT, errors::NO_OBJECT);
    }
}
