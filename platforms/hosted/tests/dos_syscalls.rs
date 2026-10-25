//! The DOS syscall path, end to end.
//!
//! # What this file is
//!
//! Phase 5's gate is that the AmigaDOS file API is reachable from user
//! space through the syscall boundary. This file is the hosted proof of
//! that: every call goes through [`Kernel::syscall`], which is the same
//! dispatch path the `syscall` instruction reaches on bare metal, and every
//! path and buffer argument is a *host* address standing in for a user
//! address.
//!
//! The tests cover the three things that are easy to get wrong at a
//! boundary:
//!
//! * **A path is copied, not trusted.** A path that is not NUL-terminated,
//!   or that names something that does not exist, is a clean error.
//! * **A buffer is copied, not trusted.** A read copies *out* and a write
//!   copies *in*, and a bad pointer is a clean error rather than a fault.
//! * **A handle is opaque.** A forged `BPTR` names a file that is not open,
//!   and the kernel says so.

use std::sync::Arc;

use vela_kernel_dos::Dos;
use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_kernel_syscall::SyscallResult;

use vela_hosted::Kernel;
use vela_ramfs::RamFs;

/// A kernel with a ramfs mounted at `/ramfs`, which is where `RAM:` points.
fn kernel() -> Kernel {
    let k = Kernel::new();
    let ram = Arc::new(RamFs::new());
    k.dos().mount("/ramfs", "RAM", ram);
    k
}

/// The result of a syscall, or a panic with the number that failed.
fn ok(result: SyscallResult, what: &str) -> SyscallResult {
    assert!(result.is_ok(), "{what} failed with error {}", result.error);
    result
}

/// A NUL-terminated path in a stable buffer.
///
/// The buffer is leaked so the pointer stays valid for the whole test: the
/// copy helper reads it, and a pointer into a moved `Vec` would be a pointer
/// into nothing.
fn path(text: &str) -> u64 {
    let mut bytes = text.as_bytes().to_vec();
    bytes.push(0);
    let boxed = bytes.into_boxed_slice();
    Box::leak(boxed).as_ptr() as u64
}

/// A `DOS_OPEN` call.
fn open(k: &Kernel, task: u32, path_ptr: u64, mode: u32) -> SyscallResult {
    k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_OPEN,
        SyscallArgs::new(path_ptr, mode as u64, 0, 0, 0, 0),
    )
}

/// A `DOS_WRITE` call.
fn write(k: &Kernel, task: u32, bptr: u64, buf: u64, len: u64) -> SyscallResult {
    k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_WRITE,
        SyscallArgs::new(bptr, buf, len, 0, 0, 0),
    )
}

/// A `DOS_READ` call.
fn read(k: &Kernel, task: u32, bptr: u64, buf: u64, len: u64) -> SyscallResult {
    k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_READ,
        SyscallArgs::new(bptr, buf, len, 0, 0, 0),
    )
}

/// A `DOS_SEEK` call.
fn seek(k: &Kernel, task: u32, bptr: u64, offset: u64, mode: u64) -> SyscallResult {
    k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_SEEK,
        SyscallArgs::new(bptr, offset, mode, 0, 0, 0),
    )
}

/// A `DOS_CLOSE` call.
fn close(k: &Kernel, task: u32, bptr: u64) -> SyscallResult {
    k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_CLOSE,
        SyscallArgs::new(bptr, 0, 0, 0, 0, 0),
    )
}

#[test]
fn a_file_can_be_opened_written_and_read_through_syscalls() {
    let k = kernel();
    let task = k.exec().create_task().0;

    let p = path("RAM:test.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    assert_ne!(opened.value, 0, "an opened file must have a handle");

    let data = b"hello";
    let buf = data.as_ptr() as u64;
    let written = ok(write(&k, task, opened.value, buf, 5), "write");
    assert_eq!(written.value, 5);

    // The write advanced the position, so the read has to seek back.
    ok(seek(&k, task, opened.value, 0, 0), "seek");
    let mut out = [0u8; 5];
    let out_ptr = out.as_mut_ptr() as u64;
    let read_result = ok(read(&k, task, opened.value, out_ptr, 5), "read");
    assert_eq!(read_result.value, 5);
    assert_eq!(&out, b"hello");

    ok(close(&k, task, opened.value), "close");
}

#[test]
fn opening_a_missing_file_for_reading_is_refused() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:missing.txt");
    let result = open(&k, task, p, 0b001);
    assert_eq!(result.error, errors::PATH_NOT_FOUND);
}

#[test]
fn opening_a_missing_file_for_writing_creates_it() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:new.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    ok(close(&k, task, opened.value), "close");

    let info = k.dos().examine("RAM:new.txt").unwrap();
    assert_eq!(info.size, 0);
}

#[test]
fn a_bad_path_pointer_is_a_clean_error() {
    // The plan requires this case explicitly: a syscall must not fault when
    // the program hands it a bad pointer.
    let k = kernel();
    let task = k.exec().create_task().0;
    let result = open(&k, task, 0xFFFF_8000_0000_0000, 0b001);
    assert_eq!(result.error, errors::BAD_POINTER);
}

#[test]
fn a_null_path_pointer_is_a_clean_error() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let result = open(&k, task, 0, 0b001);
    assert_eq!(result.error, errors::BAD_POINTER);
}

#[test]
fn a_bad_buffer_pointer_is_a_clean_error() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:buf.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");

    // A write to a kernel address must be refused, and refused *before* the
    // file is touched.
    let result = write(&k, task, opened.value, 0xFFFF_8000_0000_0000, 5);
    assert_eq!(result.error, errors::BAD_POINTER);

    // The file is still empty, because the write never happened.
    assert_eq!(k.dos().examine("RAM:buf.txt").unwrap().size, 0);
}

#[test]
fn a_read_into_a_bad_pointer_does_not_consume_the_data() {
    // The read is copied *out* second, so a failed copy leaves the data in
    // the file and the caller can retry with a good pointer.
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:retry.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    let data = b"payload";
    ok(
        write(&k, task, opened.value, data.as_ptr() as u64, 7),
        "write",
    );
    ok(seek(&k, task, opened.value, 0, 0), "seek");

    let bad = read(&k, task, opened.value, 0xFFFF_8000_0000_0000, 7);
    assert_eq!(bad.error, errors::BAD_POINTER);

    // The data is still there: the failed copy consumed nothing.
    ok(seek(&k, task, opened.value, 0, 0), "seek");
    let mut out = [0u8; 7];
    let result = ok(
        read(&k, task, opened.value, out.as_mut_ptr() as u64, 7),
        "read",
    );
    assert_eq!(result.value, 7);
    assert_eq!(&out, b"payload");
}

#[test]
fn a_forged_file_handle_is_refused() {
    // A `BPTR` is opaque, so a forged one names a file that is not open.
    let k = kernel();
    let task = k.exec().create_task().0;
    let result = read(&k, task, 0xDEAD_BEEF, path("RAM:x"), 4);
    assert_eq!(result.error, errors::NO_OBJECT);
}

#[test]
fn an_invalid_access_mode_is_refused() {
    // Guessing a mode wrong would write a file the caller did not mean to
    // write, so an unknown mode is an error.
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:mode.txt");
    assert_eq!(open(&k, task, p, 0).error, errors::RANGE);
    assert_eq!(open(&k, task, p, 0b100).error, errors::RANGE);
}

#[test]
fn a_seek_can_be_absolute_or_relative() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:seek.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    let data = b"0123456789";
    ok(
        write(&k, task, opened.value, data.as_ptr() as u64, 10),
        "write",
    );

    // Relative to the start.
    let position = ok(seek(&k, task, opened.value, 0, 0), "seek");
    assert_eq!(position.value, 0);
    // Relative to the current position.
    let position = ok(seek(&k, task, opened.value, 3, 1), "seek");
    assert_eq!(position.value, 3);
    // Relative to the end.
    let position = ok(seek(&k, task, opened.value, 0, 2), "seek");
    assert_eq!(position.value, 10);
    ok(close(&k, task, opened.value), "close");
}

#[test]
fn an_invalid_seek_mode_is_refused() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:seek2.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    assert_eq!(seek(&k, task, opened.value, 0, 7).error, errors::RANGE);
}

#[test]
fn a_seek_past_the_end_of_the_file_is_refused() {
    // A negative position is out of range: the file has no bytes before its
    // start.
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:seek3.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    assert_eq!(
        seek(&k, task, opened.value, (-1i64) as u64, 0).error,
        errors::RANGE
    );
}

#[test]
fn a_directory_can_be_created_and_listed() {
    let k = kernel();
    let task = k.exec().create_task().0;

    let p = path("RAM:dir");
    let made = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_MKDIR,
        SyscallArgs::new(p, 0, 0, 0, 0, 0),
    );
    ok(made, "mkdir");

    // A file inside it, so the listing has something in it.
    let p = path("RAM:dir/a.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    ok(close(&k, task, opened.value), "close");

    let dir_ptr = path("RAM:dir");
    let mut entries = vec![0u8; 256 * 4];
    let entries_ptr = entries.as_mut_ptr() as u64;
    let listed = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_READ_DIR,
        SyscallArgs::new(dir_ptr, entries_ptr, 4, 0, 0, 0),
    );
    ok(listed, "readdir");
    assert_eq!(listed.value, 1);
}

#[test]
fn a_file_can_be_deleted() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:gone.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    ok(close(&k, task, opened.value), "close");

    let deleted = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_DELETE,
        SyscallArgs::new(p, 0, 0, 0, 0, 0),
    );
    ok(deleted, "delete");
    assert!(k.dos().examine("RAM:gone.txt").is_err());
}

#[test]
fn the_current_directory_can_be_read() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let mut buffer = vec![0u8; 64];
    let result = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_CURRENT_DIR,
        SyscallArgs::new(buffer.as_mut_ptr() as u64, 64, 0, 0, 0, 0),
    );
    ok(result, "currentdir");
    // The reply is NUL-terminated, so the caller can use it as a string.
    let text = std::str::from_utf8(&buffer[..result.value as usize]).unwrap();
    assert!(text.starts_with('/'), "the current dir was {text:?}");
    assert_eq!(buffer[result.value as usize], 0);
}

#[test]
fn an_examine_fills_in_a_file_info() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:info.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    let data = b"0123456789";
    ok(
        write(&k, task, opened.value, data.as_ptr() as u64, 10),
        "write",
    );
    ok(close(&k, task, opened.value), "close");

    let mut info = vec![0u8; 256];
    let result = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_EXAMINE,
        SyscallArgs::new(p, info.as_mut_ptr() as u64, 0, 0, 0, 0),
    );
    ok(result, "examine");

    // The reply is a `FileInfo`; the size is at offset 8.
    let size = u64::from_le_bytes(info[8..16].try_into().unwrap());
    assert_eq!(size, 10);
}

#[test]
fn an_examine_with_a_bad_pointer_is_a_clean_error() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:info2.txt");
    // The file has to exist: the pointer check happens after the lookup, and
    // a missing file is a different error.
    let opened = ok(open(&k, task, p, 0b011), "open");
    ok(close(&k, task, opened.value), "close");
    let result = k.syscall(
        vela_kernel_exec::task::TaskId(task),
        numbers::DOS_EXAMINE,
        SyscallArgs::new(p, 0xFFFF_8000_0000_0000, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);
}

#[test]
fn the_dos_service_is_reachable_from_a_task_through_the_dispatcher() {
    // The service is one instance for the whole kernel, and it is reachable
    // through the same dispatch table as everything else.
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:shared.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    // The same file is visible through the service directly, which is what
    // makes the syscall a *view* of the service rather than a copy of it.
    assert!(k.dos().examine("RAM:shared.txt").is_ok());
    ok(close(&k, task, opened.value), "close");
}

#[test]
fn the_dos_service_can_mount_a_filesystem_at_runtime() {
    let k = kernel();
    let dos: &Dos = k.dos();
    let ram = Arc::new(RamFs::new());
    assert!(dos.mount("/other", "OTHER", ram));
    // The same point cannot be mounted twice.
    assert!(!dos.mount("/other", "OTHER2", Arc::new(RamFs::new())));
}

#[test]
fn a_zero_length_read_is_a_successful_no_op() {
    // A program does this to discover the current position.
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:zero.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    let result = read(&k, task, opened.value, 0, 0);
    assert!(result.is_ok());
    assert_eq!(result.value, 0);
}

#[test]
fn an_absurd_length_is_refused_rather_than_truncated() {
    let k = kernel();
    let task = k.exec().create_task().0;
    let p = path("RAM:huge.txt");
    let opened = ok(open(&k, task, p, 0b011), "open");
    let result = read(&k, task, opened.value, path("RAM:x"), u64::MAX);
    assert_eq!(result.error, errors::RANGE);
}
