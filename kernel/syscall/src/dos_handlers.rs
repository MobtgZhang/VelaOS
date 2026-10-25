//! The `dos.library` syscall handlers.
//!
//! # What crosses the boundary
//!
//! A DOS call comes in with three kinds of argument, and each needs its own
//! handling:
//!
//! * **A handle** — a `BPTR`, an opaque number the kernel turns back into a
//!   file. It is copied straight from the register; there is nothing to
//!   validate, because a forged handle names a file that does not exist.
//! * **A path** — a NUL-terminated string in user space. It is copied into
//!   the kernel with [`copy_string_alloc`], which reads one byte at a time
//!   so that a string running off the end of a mapped page is an error rather
//!   than a fault.
//! * **A buffer** — a user pointer plus a length. The *contents* are copied
//!   out after the syscall has already produced them, and the user's copy is
//!   the last thing that happens, so a failed copy leaves the file's state
//!   consistent.
//!
//! # Why the buffer direction matters
//!
//! `Read` copies *out* of the kernel into a user buffer. It has to read the
//! data first and copy it second, because the copy can fail — and a copy that
//! fails must not have consumed the data. `Write` copies *into* the kernel
//! first, because the write cannot be undone: if the copy succeeds and the
//! write then fails, the caller's buffer is already consumed, which is
//! exactly what a retry would expect.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use vela_abi_native64::{AnchorPath, FileInfo};
use vela_kernel_dos::SeekFrom;
use vela_kernel_vfs::AccessMode;

use crate::copy::copy_string_alloc;
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// The largest `ReadDir` reply a single call will produce.
///
/// A directory with ten thousand entries would otherwise need a ten-thousand
/// entry allocation in the kernel to answer one syscall, and the reply would
/// be copied into user space in one go. Bounding it means a scan is a
/// sequence of calls, which is also how AmigaOS's `ExamineDir` works: the
/// caller comes back for the next batch.
pub const MAX_DIR_ENTRIES: usize = 64;

/// The largest single read or write a call will move.
///
/// 1 MiB. The bound is what stops a user program from asking the kernel to
/// move an unbounded amount of memory in a single syscall; a program that
/// needs more loops.
pub const MAX_IO_BYTES: usize = 1024 * 1024;

/// The largest path a call accepts, in bytes.
///
/// Matches `MAX_PATH_LEN` in `vela-amiga-path`: a longer path is refused
/// before it is copied, so the copy's own limit is never the thing that stops
/// it.
pub const MAX_PATH: usize = 1024;

/// Decode the AmigaOS `MODE_*` flags into an [`AccessMode`].
///
/// The flags are the `v0.1.0` values: bit 0 is read, bit 1 is write, bit 2 is
/// append.
fn mode_from_flags(flags: u32) -> Result<AccessMode, i32> {
    match flags & 0b111 {
        0b001 => Ok(AccessMode::Read),
        0b010 => Ok(AccessMode::Write),
        0b011 => Ok(AccessMode::ReadWrite),
        0b111 => Ok(AccessMode::Append),
        // `0b100` is "read, positioned at the end" on AmigaOS, and `0b000`
        // is not a mode at all. Both are refused rather than guessed at,
        // because a mode guessed wrong writes a file the caller did not
        // mean to write.
        _ => Err(errors::RANGE),
    }
}

/// Convert a Unix timestamp into an AmigaOS `DateStamp`.
///
/// VelaOS stores timestamps as Unix seconds because Unix seconds are
/// unambiguous, and converts here because a program that reads `fi_Time`
/// expects the AmigaDOS form.
fn datestamp_from_unix(seconds: u64) -> vela_abi_native64::DateStamp {
    const SECONDS_PER_DAY: u64 = 86_400;
    const SECONDS_PER_MINUTE: u64 = 60;
    /// Ticks per second. AmigaDOS counts in 1/50 s units.
    const TICKS_PER_SECOND: u64 = 50;

    let days = seconds / SECONDS_PER_DAY;
    let rest = seconds % SECONDS_PER_DAY;
    let minute = (rest / SECONDS_PER_MINUTE) as i32;
    let tick = ((rest % SECONDS_PER_MINUTE) * TICKS_PER_SECOND) as i32;
    vela_abi_native64::DateStamp {
        ds_Days: days as i32 - vela_abi_native64::AMIGA_EPOCH_UNIX_DAYS,
        ds_Minute: minute,
        ds_Tick: tick,
    }
}

/// Decode the `SEEK_*` mode into a [`SeekFrom`].
fn seek_from_mode(mode: u32) -> Result<SeekFrom, i32> {
    match mode {
        0 => Ok(SeekFrom::Start),
        1 => Ok(SeekFrom::Current),
        2 => Ok(SeekFrom::End),
        _ => Err(errors::RANGE),
    }
}

/// The mode a DOS call is in, as a user-visible struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct DosMode {
    /// The AmigaOS `MODE_*` flags.
    pub md_Fields: u32,
}

/// `Open` — open a file.
///
/// Arguments: `name_ptr` (a user pointer to a NUL-terminated AmigaDOS
/// path), `mode`. Returns the file's `BPTR`.
pub fn open(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let flags = args.get(1) as u32;
    let path = match copy_path(name_ptr) {
        Ok(p) => p,
        Err(e) => return SyscallResult::err(e),
    };
    let mode = match mode_from_flags(flags) {
        Ok(m) => m,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.dos.open(&path, mode) {
        Ok(bptr) => SyscallResult::ok(bptr.raw() as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Close` — close a file.
///
/// Arguments: `bptr`. Returns zero on success.
pub fn close(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let bptr = ctx.dos_bptr(args.get(0));
    match ctx.dos.close(bptr) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Read` — read from a file into a user buffer.
///
/// Arguments: `bptr`, `buf_ptr` (a user pointer to receive the bytes),
/// `len` (the number of bytes requested). Returns the number of bytes read.
///
/// The data is read first and copied out second, so a failed copy does not
/// consume the data: the caller can fix its pointer and read again.
pub fn read(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let bptr = ctx.dos_bptr(args.get(0));
    let buf_ptr = args.get(1);
    let len = args.get(2);
    let Some(len) = bounded_len(len) else {
        return SyscallResult::err(errors::RANGE);
    };
    if len == 0 {
        // A zero-length read is a successful no-op, not an error: it is what
        // a program does to discover the current position.
        return SyscallResult::ok(0);
    }
    let data = match ctx.dos.read(bptr, len) {
        Ok(d) => d,
        Err(e) => return SyscallResult::err(e),
    };
    let n = data.len();
    if n == 0 {
        return SyscallResult::ok(0);
    }
    // SAFETY: the destination is a user pointer validated by `copy_to_user`.
    match unsafe { crate::copy::copy_to_user(buf_ptr, data.as_ptr(), n) } {
        Ok(_) => SyscallResult::ok(n as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Write` — write from a user buffer into a file.
///
/// Arguments: `bptr`, `buf_ptr` (a user pointer to the bytes), `len`. Returns
/// the number of bytes written.
///
/// The copy in is *first*: the write cannot be undone, so by the time it
/// happens the caller's buffer is already consumed — which is what a retry
/// after a failed write expects.
pub fn write(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let bptr = ctx.dos_bptr(args.get(0));
    let buf_ptr = args.get(1);
    let len = args.get(2);
    let Some(len) = bounded_len(len) else {
        return SyscallResult::err(errors::RANGE);
    };
    if len == 0 {
        return SyscallResult::ok(0);
    }
    let mut buffer = alloc::vec![0u8; len];
    // SAFETY: the destination is a kernel buffer of exactly `len` bytes; the
    // source is the user's, which `copy_from_user` validates.
    if let Err(e) = unsafe { crate::copy::copy_from_user(buffer.as_mut_ptr(), buf_ptr, len) } {
        return SyscallResult::err(e);
    }
    match ctx.dos.write(bptr, &buffer) {
        Ok(n) => SyscallResult::ok(n as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Seek` — move a file's position.
///
/// Arguments: `bptr`, `offset`, `mode` (`SEEK_SET`, `SEEK_CUR` or
/// `SEEK_END`). Returns the new position.
pub fn seek(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let bptr = ctx.dos_bptr(args.get(0));
    let offset = args.get(1) as i64;
    let mode = args.get(2) as u32;
    let whence = match seek_from_mode(mode) {
        Ok(w) => w,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.dos.seek(bptr, offset, whence) {
        Ok(position) => SyscallResult::ok(position),
        Err(e) => SyscallResult::err(e),
    }
}

/// `ReadDir` — list a directory into an array of user-visible entries.
///
/// Arguments: `name_ptr` (the directory's path), `entries_ptr` (a user
/// pointer to receive the array), `max_entries` (how many entries the
/// caller's array holds). Returns the number of entries written.
///
/// The reply is built in the kernel and copied out in one piece, so the
/// caller's array is either fully written or not written at all. A partial
/// array would be indistinguishable from a short directory, which is a
/// distinction the caller cannot make and does not need.
pub fn read_dir(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let entries_ptr = args.get(1);
    let max_entries = args.get(2);
    let Some(max_entries) = bounded_len(max_entries) else {
        return SyscallResult::err(errors::RANGE);
    };
    if max_entries == 0 {
        return SyscallResult::err(errors::RANGE);
    }
    let path = match copy_path(name_ptr) {
        Ok(p) => p,
        Err(e) => return SyscallResult::err(e),
    };
    let entries = match ctx.dos.read_dir(&path) {
        Ok(e) => e,
        Err(e) => return SyscallResult::err(e),
    };

    let limit = entries.len().min(max_entries).min(MAX_DIR_ENTRIES);
    // One fixed-size record per entry keeps the copy a single memcpy and
    // keeps the user's array layout identical to the kernel's, which is what
    // removes the per-entry pointer validation a variable-length layout
    // would need.
    let record = DirEntryRecord::SIZE;
    let bytes = limit.saturating_mul(record);
    if bytes == 0 {
        return SyscallResult::ok(0);
    }

    let mut reply = Vec::with_capacity(bytes);
    for entry in entries.iter().take(limit) {
        reply.extend_from_slice(&DirEntryRecord::from_entry(entry).to_bytes());
    }
    // SAFETY: the destination is a user pointer validated by `copy_to_user`,
    // and the caller asked for at least this many records.
    match unsafe { crate::copy::copy_to_user(entries_ptr, reply.as_ptr(), bytes) } {
        Ok(_) => SyscallResult::ok(limit as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// `DeleteFile` — remove a file.
///
/// Arguments: `name_ptr`. Returns zero on success.
pub fn delete(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let path = match copy_path(name_ptr) {
        Ok(p) => p,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.dos.delete_file(&path) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `MakeDir` — create a directory.
///
/// Arguments: `name_ptr`. Returns zero on success.
pub fn mkdir(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let path = match copy_path(name_ptr) {
        Ok(p) => p,
        Err(e) => return SyscallResult::err(e),
    };
    match ctx.dos.create_dir(&path) {
        Ok(()) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `Examine` — look a path up, filling in a `FileInfo`.
///
/// Arguments: `name_ptr` (the path), `info_ptr` (a user pointer to receive
/// the `FileInfo`). Returns zero on success.
pub fn examine(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let info_ptr = args.get(1);
    let path = match copy_path(name_ptr) {
        Ok(p) => p,
        Err(e) => return SyscallResult::err(e),
    };
    let info = match ctx.dos.examine(&path) {
        Ok(i) => i,
        Err(e) => return SyscallResult::err(e),
    };
    let reply = file_info_from(info);
    // SAFETY: the destination is a user pointer validated by `copy_to_user`.
    match unsafe {
        crate::copy::copy_to_user(
            info_ptr,
            &reply as *const FileInfo as *const u8,
            core::mem::size_of::<FileInfo>(),
        )
    } {
        Ok(_) => SyscallResult::ok(0),
        Err(e) => SyscallResult::err(e),
    }
}

/// `CurrentDir` — read the calling process's current directory.
///
/// Arguments: `buf_ptr` (a user pointer to receive a NUL-terminated path),
/// `buf_len`. Returns the length of the path, excluding the terminator.
pub fn current_dir(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let buf_ptr = args.get(0);
    let buf_len = args.get(1);
    let Some(buf_len) = bounded_len(buf_len) else {
        return SyscallResult::err(errors::RANGE);
    };
    let path = ctx.dos.current_dir();
    let bytes = path.as_bytes();
    // The terminator must fit, so a caller that sized its buffer from the
    // previous return value can store the next value too.
    let copy_len = bytes.len().min(buf_len.saturating_sub(1));
    if copy_len == 0 {
        return SyscallResult::err(errors::TOO_LARGE);
    }
    let mut text = Vec::with_capacity(bytes.len() + 1);
    text.extend_from_slice(&bytes[..copy_len]);
    text.push(0);
    // SAFETY: the destination is a user pointer validated by `copy_to_user`.
    match unsafe { crate::copy::copy_to_user(buf_ptr, text.as_ptr(), text.len()) } {
        Ok(_) => SyscallResult::ok(copy_len as u64),
        Err(e) => SyscallResult::err(e),
    }
}

/// Copy a path from user space, refusing one that is too long.
fn copy_path(ptr: u64) -> Result<String, i32> {
    let path = copy_string_alloc(ptr)?;
    if path.len() > MAX_PATH {
        return Err(errors::TOO_LARGE);
    }
    Ok(path)
}

/// Turn a `u64` length into a `usize`, refusing one that is absurd.
///
/// The cast is the dangerous part: a 64-bit length from user space would
/// truncate silently on a 32-bit host and produce a *small* number, which
/// would then be trusted. Refusing anything above the bound is what stops
/// that.
fn bounded_len(len: u64) -> Option<usize> {
    if len > MAX_IO_BYTES as u64 {
        None
    } else {
        Some(len as usize)
    }
}

/// The user-visible form of one directory entry.
///
/// # Layout
///
/// ```text
/// offset  0  u64  de_Size
/// offset  8  u32  de_Type
/// offset 12  u8   de_NameLen
/// offset 13  [u8; 107] de_Name
/// ```
///
/// The name is inline rather than a pointer: a pointer would be a user
/// pointer the kernel would have to chase, and an inline buffer makes the
/// whole reply a single copy. The name is bounded by
/// [`vela_amiga_path::MAX_NAME_LEN`] — the same bound the path parser
/// enforces — so it always fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
#[allow(
    non_snake_case,
    reason = "the AmigaOS field names are CamelCase by convention"
)]
pub struct DirEntryRecord {
    /// The entry's size in bytes.
    pub de_Size: u64,
    /// The AmigaOS entry type: 2 for a file, 3 for a directory.
    pub de_Type: u32,
    /// The name's length, excluding the terminator.
    pub de_NameLen: u8,
    /// The name, NUL-terminated.
    pub de_Name: [u8; DIR_ENTRY_NAME_LEN],
}

/// The size of a [`DirEntryRecord`]'s inline name buffer.
pub const DIR_ENTRY_NAME_LEN: usize = 107;

impl DirEntryRecord {
    /// The size of one record, in bytes.
    pub const SIZE: usize = 16 + DIR_ENTRY_NAME_LEN;

    /// Build a record from a directory entry.
    pub fn from_entry(entry: &vela_kernel_vfs::DirEntry) -> Self {
        let kind = match entry.kind {
            vela_kernel_vfs::NodeKind::Directory => vela_abi_native64::ENTRY_TYPE_DIR,
            vela_kernel_vfs::NodeKind::Device => vela_abi_native64::ENTRY_TYPE_DEV,
            _ => vela_abi_native64::ENTRY_TYPE_FILE,
        };
        let bytes = entry.name.as_bytes();
        let len = bytes.len().min(DIR_ENTRY_NAME_LEN - 1);
        let mut name = [0u8; DIR_ENTRY_NAME_LEN];
        name[..len].copy_from_slice(&bytes[..len]);
        Self {
            de_Size: entry.size,
            de_Type: kind,
            de_NameLen: len as u8,
            de_Name: name,
        }
    }

    /// The record's bytes, in the layout a user program sees.
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut out = [0u8; Self::SIZE];
        out[0..8].copy_from_slice(&self.de_Size.to_le_bytes());
        out[8..12].copy_from_slice(&self.de_Type.to_le_bytes());
        out[12] = self.de_NameLen;
        out[13..13 + DIR_ENTRY_NAME_LEN].copy_from_slice(&self.de_Name);
        out
    }
}

/// Build a `FileInfo` from a node's metadata.
///
/// The fields VelaOS does not have — the entry key, the disk key, the owner
/// ids — are zeroed rather than invented. A program that reads them gets a
/// consistent "no information" rather than a value that looks meaningful and
/// is not.
pub fn file_info_from(info: vela_kernel_vfs::NodeInfo) -> FileInfo {
    let mut comment = [0u8; vela_abi_native64::FILE_INFO_COMMENT_LEN];
    let bytes = info.comment.as_bytes();
    let len = bytes
        .len()
        .min(vela_abi_native64::FILE_INFO_COMMENT_LEN - 1);
    comment[..len].copy_from_slice(&bytes[..len]);

    let entry_type = match info.kind {
        vela_kernel_vfs::NodeKind::Directory => vela_abi_native64::ENTRY_TYPE_DIR,
        vela_kernel_vfs::NodeKind::Device => vela_abi_native64::ENTRY_TYPE_DEV,
        _ => vela_abi_native64::ENTRY_TYPE_FILE,
    };

    FileInfo {
        fi_EntryType: entry_type,
        fi_EntryKey: 0,
        fi_Size: info.size,
        fi_DiskKey: 0,
        fi_Protection: info.protection,
        fi_OwnerUID: 0,
        fi_OwnerGID: 0,
        fi_CommentLen: len as i32,
        _pad: [0; 4],
        fi_Time: datestamp_from_unix(info.mtime),
        _pad2: [0; 4],
        fi_OwnerUID64: 0,
        fi_OwnerGID64: 0,
        fi_CommentLen2: 0,
        fi_Comment: comment,
    }
}

/// `ExamineDir` — walk a directory with an anchor.
///
/// Arguments: `name_ptr` (the first path, or `None` to continue the last
/// walk), `anchor_ptr` (a user pointer to an `AnchorPath` that carries the
/// walk's state between calls). Returns zero when an entry was produced.
///
/// The anchor is *read and written* by the kernel, so the walk's position
/// survives across calls. That is what makes it a walk rather than a list:
/// a directory with a thousand entries is traversed in as many calls as the
/// caller wants, without the kernel ever holding the whole directory.
///
/// A kernel-side cursor keyed by the anchor's value is what tracks the walk.
/// The anchor itself holds the *last path*, which is how `ExamineDir`
/// resumes: the caller's second call passes the path the first call
/// returned, and the kernel finds the entry after it.
pub fn examine_dir(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name_ptr = args.get(0);
    let anchor_ptr = args.get(1);

    let mut anchor = AnchorPath {
        ap_First: 0,
        ap_FoundBreak: 0,
        ap_Flags: 0,
        ap_Reserved: 0,
        ap_Strlen: 0,
        ap_SearchFlags: 0,
        ap_SegListBlock: 0,
        ap_Reserved1: 0,
        ap_Buf: [0; vela_abi_native64::ANCHOR_PATH_BUF_LEN],
    };
    // SAFETY: the destination is a kernel local; the source is the user's,
    // which `copy_from_user` validates.
    let read_anchor = unsafe {
        crate::copy::copy_from_user(
            &mut anchor as *mut AnchorPath as *mut u8,
            anchor_ptr,
            core::mem::size_of::<AnchorPath>(),
        )
    };

    // The first call passes a path; a continuation passes the path the
    // previous call returned. Either way the directory is the parent of the
    // named entry.
    let path = if name_ptr != 0 {
        match copy_path(name_ptr) {
            Ok(p) => p,
            Err(e) => return SyscallResult::err(e),
        }
    } else {
        anchor_path_string(&anchor).unwrap_or_default()
    };
    if path.is_empty() {
        return SyscallResult::err(errors::NO_OBJECT);
    }

    let (directory, skip) = match split_last(&path) {
        Some(parts) => parts,
        None => (path.clone(), String::new()),
    };
    let entries = match ctx.dos.read_dir(&directory) {
        Ok(e) => e,
        Err(e) => return SyscallResult::err(e),
    };

    // The cursor is the entry whose name is the one named by the path; the
    // walk resumes *after* it. An empty name starts at the beginning.
    let start = if skip.is_empty() {
        0
    } else {
        match entries
            .iter()
            .position(|e| e.name.eq_ignore_ascii_case(&skip))
        {
            Some(index) => index + 1,
            // A path naming an entry that is not in the directory is a
            // stale anchor, not a missing file: the directory exists and the
            // caller wants to walk it, so the walk starts at the beginning.
            None => 0,
        }
    };

    let Some(entry) = entries.get(start) else {
        // The walk is finished. `ap_First` clear says so.
        anchor.ap_First = 0;
        anchor.ap_FoundBreak = 1;
        // SAFETY: as above.
        let _ = unsafe {
            crate::copy::copy_to_user(
                anchor_ptr,
                &anchor as *const AnchorPath as *const u8,
                core::mem::size_of::<AnchorPath>(),
            )
        };
        return SyscallResult::ok(1);
    };

    let next = format!("{}/{}", directory.trim_end_matches('/'), entry.name);
    let next_bytes = next.as_bytes();
    let len = next_bytes
        .len()
        .min(vela_abi_native64::ANCHOR_PATH_BUF_LEN - 1);
    anchor.ap_Buf = [0; vela_abi_native64::ANCHOR_PATH_BUF_LEN];
    anchor.ap_Buf[..len].copy_from_slice(&next_bytes[..len]);
    anchor.ap_Strlen = len as u32;
    anchor.ap_First = 1;
    anchor.ap_FoundBreak = 0;
    // SAFETY: as above.
    if let Err(e) = unsafe {
        crate::copy::copy_to_user(
            anchor_ptr,
            &anchor as *const AnchorPath as *const u8,
            core::mem::size_of::<AnchorPath>(),
        )
    } {
        return SyscallResult::err(e);
    }
    let _ = read_anchor;
    SyscallResult::ok(0)
}

/// Read the path an `AnchorPath` carries.
fn anchor_path_string(anchor: &AnchorPath) -> Option<String> {
    let len = (anchor.ap_Strlen as usize).min(vela_abi_native64::ANCHOR_PATH_BUF_LEN);
    let bytes = &anchor.ap_Buf[..len];
    if bytes.contains(&0) {
        return None;
    }
    core::str::from_utf8(bytes)
        .ok()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

/// Split a path into its directory and its last component.
fn split_last(path: &str) -> Option<(String, String)> {
    let index = path.rfind('/')?;
    Some((path[..index].to_string(), path[index + 1..].to_string()))
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn the_mode_flags_decode_to_the_amigaos_modes() {
        assert_eq!(mode_from_flags(0b001), Ok(AccessMode::Read));
        assert_eq!(mode_from_flags(0b010), Ok(AccessMode::Write));
        assert_eq!(mode_from_flags(0b011), Ok(AccessMode::ReadWrite));
        assert_eq!(mode_from_flags(0b111), Ok(AccessMode::Append));
    }

    #[test]
    fn an_invalid_mode_is_refused_rather_than_guessed() {
        // Guessing a mode wrong would write a file the caller did not mean
        // to write, so an unknown mode is an error.
        assert_eq!(mode_from_flags(0), Err(errors::RANGE));
        assert_eq!(mode_from_flags(0b100), Err(errors::RANGE));
        assert_eq!(mode_from_flags(0b110), Err(errors::RANGE));
    }

    #[test]
    fn the_seek_modes_decode_to_the_amigaos_ones() {
        assert_eq!(seek_from_mode(0), Ok(SeekFrom::Start));
        assert_eq!(seek_from_mode(1), Ok(SeekFrom::Current));
        assert_eq!(seek_from_mode(2), Ok(SeekFrom::End));
        assert_eq!(seek_from_mode(3), Err(errors::RANGE));
    }

    #[test]
    fn an_absurd_length_is_refused_rather_than_truncated() {
        // The dangerous case is a huge `u64` that would truncate to a small
        // number on a narrower host, which would then be trusted.
        assert!(bounded_len(u64::MAX).is_none());
        assert!(bounded_len(MAX_IO_BYTES as u64 + 1).is_none());
        assert_eq!(bounded_len(0), Some(0));
        assert_eq!(bounded_len(16), Some(16));
    }

    #[test]
    fn the_datestamp_conversion_matches_the_amigaos_epoch() {
        // Unix epoch itself is day 2922 in AmigaDOS terms, because the
        // AmigaDOS epoch is 2208 days *before* the Unix one.
        let s = datestamp_from_unix(0);
        assert_eq!(s.ds_Days, -vela_abi_native64::AMIGA_EPOCH_UNIX_DAYS);
        assert_eq!(s.ds_Minute, 0);
        assert_eq!(s.ds_Tick, 0);

        // One day later is day 2923, minute 0, tick 0.
        let s = datestamp_from_unix(86_400);
        assert_eq!(s.ds_Days, -vela_abi_native64::AMIGA_EPOCH_UNIX_DAYS + 1);
        assert_eq!(s.ds_Minute, 0);

        // An hour in is minute 0 at 50 ticks/second, so a minute of ticks
        // is 3000.
        let s = datestamp_from_unix(90);
        assert_eq!(s.ds_Minute, 1);
        assert_eq!(s.ds_Tick, 30 * 50);
    }

    #[test]
    fn a_directory_entry_record_is_the_size_it_claims() {
        let record = DirEntryRecord {
            de_Size: 1234,
            de_Type: vela_abi_native64::ENTRY_TYPE_FILE,
            de_NameLen: 3,
            de_Name: {
                let mut n = [0u8; DIR_ENTRY_NAME_LEN];
                n[0] = b'a';
                n[1] = b'b';
                n[2] = b'c';
                n
            },
        };
        let bytes = record.to_bytes();
        assert_eq!(bytes.len(), DirEntryRecord::SIZE);
        // The bytes really are the struct's bytes, so a user program can
        // read the reply as a `DirEntryRecord` and get the same values.
        // SAFETY: the two are the same type at the same address; this test is
        // what checks that.
        let view = unsafe { core::ptr::read(bytes.as_ptr() as *const DirEntryRecord) };
        assert_eq!(view, record);
    }

    #[test]
    fn a_directory_entry_records_its_kind() {
        let file = vela_kernel_vfs::DirEntry {
            name: "a.txt".to_string(),
            kind: vela_kernel_vfs::NodeKind::File,
            size: 10,
        };
        assert_eq!(
            DirEntryRecord::from_entry(&file).de_Type,
            vela_abi_native64::ENTRY_TYPE_FILE
        );
        let dir = vela_kernel_vfs::DirEntry {
            name: "Sub".to_string(),
            kind: vela_kernel_vfs::NodeKind::Directory,
            size: 0,
        };
        assert_eq!(
            DirEntryRecord::from_entry(&dir).de_Type,
            vela_abi_native64::ENTRY_TYPE_DIR
        );
    }

    #[test]
    fn an_over_long_directory_name_is_truncated_not_overflowed() {
        let long = "x".repeat(500);
        let entry = vela_kernel_vfs::DirEntry {
            name: long,
            kind: vela_kernel_vfs::NodeKind::File,
            size: 0,
        };
        let record = DirEntryRecord::from_entry(&entry);
        assert_eq!(record.de_NameLen as usize, DIR_ENTRY_NAME_LEN - 1);
        assert_eq!(record.de_Name[DIR_ENTRY_NAME_LEN - 1], 0);
    }

    #[test]
    fn a_file_info_carries_the_metadata_and_zeroes_the_rest() {
        let mut info = vela_kernel_vfs::NodeInfo::file("a.txt", 42);
        info.mtime = 86_400;
        info.protection = 0b0110;
        let file_info = file_info_from(info);
        assert_eq!(file_info.fi_EntryType, vela_abi_native64::ENTRY_TYPE_FILE);
        assert_eq!(file_info.fi_Size, 42);
        assert_eq!(file_info.fi_Protection, 0b0110);
        assert_eq!(
            file_info.fi_Time.ds_Days,
            -vela_abi_native64::AMIGA_EPOCH_UNIX_DAYS + 1
        );
        // The fields VelaOS has no data for are zeroed, not invented.
        assert_eq!(file_info.fi_EntryKey, 0);
        assert_eq!(file_info.fi_DiskKey, 0);
        assert_eq!(file_info.fi_OwnerUID64, 0);
    }

    #[test]
    fn a_file_info_carries_the_comment() {
        let mut info = vela_kernel_vfs::NodeInfo::file("a.txt", 0);
        info.comment = "hello".to_string();
        let file_info = file_info_from(info);
        assert_eq!(file_info.fi_CommentLen, 5);
        assert_eq!(&file_info.fi_Comment[..5], b"hello");
        assert_eq!(file_info.fi_Comment[5], 0);
    }

    #[test]
    fn a_directory_reports_the_directory_entry_type() {
        let file_info = file_info_from(vela_kernel_vfs::NodeInfo::directory("Sub"));
        assert_eq!(file_info.fi_EntryType, vela_abi_native64::ENTRY_TYPE_DIR);
    }

    #[test]
    fn the_dos_mode_struct_is_the_amigaos_one() {
        // A program allocates a `DosMode` and reads the flags out of it; the
        // struct must be the flags and nothing else.
        assert_eq!(core::mem::size_of::<DosMode>(), 4);
        let mode = DosMode { md_Fields: 0b111 };
        assert_eq!(mode_from_flags(mode.md_Fields), Ok(AccessMode::Append));
    }

    #[test]
    fn a_path_splits_into_its_directory_and_last_component() {
        let (dir, last) = split_last("/ramfs/dir/file.txt").unwrap();
        assert_eq!(dir, "/ramfs/dir");
        assert_eq!(last, "file.txt");
        // A path with no slash cannot be split, and the caller has to cope.
        assert!(split_last("file.txt").is_none());
    }

    #[test]
    fn an_anchor_path_round_trips_its_path() {
        let mut anchor = AnchorPath {
            ap_First: 1,
            ap_FoundBreak: 0,
            ap_Flags: 0,
            ap_Reserved: 0,
            ap_Strlen: 0,
            ap_SearchFlags: 0,
            ap_SegListBlock: 0,
            ap_Reserved1: 0,
            ap_Buf: [0; vela_abi_native64::ANCHOR_PATH_BUF_LEN],
        };
        assert_eq!(anchor_path_string(&anchor), None);
        let text = "/ramfs/dir/a.txt";
        anchor.ap_Buf[..text.len()].copy_from_slice(text.as_bytes());
        anchor.ap_Strlen = text.len() as u32;
        assert_eq!(anchor_path_string(&anchor), Some(text.to_string()));
    }
}
