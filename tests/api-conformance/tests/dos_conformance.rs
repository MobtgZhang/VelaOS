//! The `dos.library` conformance behaviour tests.
//!
//! The DOS tests exercise the AmigaDOS file API through the syscall
//! boundary, where the interesting behaviour is *at* the boundary: a path is
//! copied rather than trusted, a buffer is copied rather than trusted, and a
//! handle is opaque.

use std::sync::{Mutex, MutexGuard, OnceLock};

use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_sys::abi::AnchorPath;
use vela_sys::api;
use vela_sys::hosted;

use vela_test_api_conformance::harness::{self, ok};

/// Guards the SDK's process-global hosted services.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The one kernel installed for the SDK-driven tests in this file.
fn services() -> &'static vela_hosted::Kernel {
    static SERVICES: OnceLock<&'static vela_hosted::Kernel> = OnceLock::new();
    SERVICES.get_or_init(|| {
        let kernel: &'static vela_hosted::Kernel = Box::leak(Box::new(harness::kernel()));
        // SAFETY: the kernel is leaked, so every reference handed to
        // `install` outlives the process.
        unsafe {
            hosted::install(
                kernel.exec(),
                kernel.dos(),
                kernel.graphics(),
                kernel.intuition(),
                kernel.icon(),
                kernel.workbench(),
                kernel.table(),
            );
        }
        kernel
    })
}

/// Install the SDK services and name a task for the current thread.
fn ready() -> MutexGuard<'static, ()> {
    let guard = serial();
    let kernel = services();
    let task = harness::task(kernel);
    kernel.exec().set_current_task(task);
    guard
}

/// `Open`, `Write`, `Read` and `Close`: the whole file round trip.
///
/// Opening for writing creates the file, because a program that opens a file
/// to write it expects the file to be there afterwards. Closing a handle that
/// is not open reports `NO_OBJECT` rather than succeeding.
#[test]
fn dos_open_write_read_close() {
    let k = harness::kernel();
    let t = harness::task(&k);

    // `0b011` is read-and-write.
    let opened = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_OPEN,
            SyscallArgs::new(harness::cstring("RAM:test.txt"), 0b011, 0, 0, 0, 0),
        ),
        "Open",
    );
    assert_ne!(opened.value, 0, "an opened file must have a handle");

    let data = b"hello";
    let written = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_WRITE,
            SyscallArgs::new(
                opened.value,
                harness::bytes(data),
                data.len() as u64,
                0,
                0,
                0,
            ),
        ),
        "Write",
    );
    assert_eq!(written.value, data.len() as u64);

    // The write advanced the position, so the read has to seek back.
    ok(
        harness::call(
            &k,
            t,
            numbers::DOS_SEEK,
            SyscallArgs::new(opened.value, 0, 0, 0, 0, 0),
        ),
        "Seek",
    );

    let out = vela_test_api_conformance::ByteBuffer::zeroed(5);
    let read = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_READ,
            SyscallArgs::new(opened.value, out.addr(), 5, 0, 0, 0),
        ),
        "Read",
    );
    assert_eq!(read.value, 5);
    assert_eq!(out.read(), b"hello".to_vec());

    ok(
        harness::call(
            &k,
            t,
            numbers::DOS_CLOSE,
            SyscallArgs::new(opened.value, 0, 0, 0, 0, 0),
        ),
        "Close",
    );

    // Closing again must fail: the handle names nothing.
    let twice = harness::call(
        &k,
        t,
        numbers::DOS_CLOSE,
        SyscallArgs::new(opened.value, 0, 0, 0, 0, 0),
    );
    assert!(twice.is_err(), "closing a closed file must fail");
    assert_eq!(twice.error, errors::NO_OBJECT);
}

/// `Seek`: the three AmigaOS seek modes are the three the API documents.
///
/// `SEEK_SET` is zero, `SEEK_CUR` is one and `SEEK_END` is two, and a mode
/// outside that range is refused rather than guessed at.
#[test]
fn dos_seek_moves_the_position() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let opened = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_OPEN,
            SyscallArgs::new(harness::cstring("RAM:seek.bin"), 0b011, 0, 0, 0, 0),
        ),
        "Open",
    );

    let data = b"0123456789";
    ok(
        harness::call(
            &k,
            t,
            numbers::DOS_WRITE,
            SyscallArgs::new(
                opened.value,
                harness::bytes(data),
                data.len() as u64,
                0,
                0,
                0,
            ),
        ),
        "Write",
    );

    // `SEEK_SET` to 4.
    let at = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_SEEK,
            SyscallArgs::new(opened.value, 4, 0, 0, 0, 0),
        ),
        "Seek (set)",
    );
    assert_eq!(at.value, 4);

    // `SEEK_CUR` by 2 lands on 6.
    let cur = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_SEEK,
            SyscallArgs::new(opened.value, 2, 1, 0, 0, 0),
        ),
        "Seek (cur)",
    );
    assert_eq!(cur.value, 6);

    // `SEEK_END` with offset 0 lands on the end.
    let end = ok(
        harness::call(
            &k,
            t,
            numbers::DOS_SEEK,
            SyscallArgs::new(opened.value, 0, 2, 0, 0, 0),
        ),
        "Seek (end)",
    );
    assert_eq!(end.value, data.len() as u64);

    // An unknown mode is refused.
    let bad = harness::call(
        &k,
        t,
        numbers::DOS_SEEK,
        SyscallArgs::new(opened.value, 0, 7, 0, 0, 0),
    );
    assert!(bad.is_err(), "an unknown seek mode must be refused");
    assert_eq!(bad.error, errors::RANGE);
}

/// `Open` on a file that does not exist and cannot be created is an error.
///
/// Reading a file that is not there must not create it: a program that opens
/// for reading and gets an empty file would silently lose its data.
#[test]
fn dos_open_of_a_missing_file_for_reading_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(
        &k,
        t,
        numbers::DOS_OPEN,
        SyscallArgs::new(harness::cstring("RAM:absent.txt"), 0b001, 0, 0, 0, 0),
    );
    assert!(
        result.is_err(),
        "opening a missing file for reading must fail"
    );
}

/// `Open` with a mode that is not a mode is refused.
///
/// The valid modes are read, write, read-write and append. A mode outside
/// that set is refused rather than guessed at, because a mode guessed wrong
/// writes a file the caller did not mean to write.
#[test]
fn dos_open_with_an_invalid_mode_is_refused() {
    let k = harness::kernel();
    let t = harness::task(&k);

    let result = harness::call(
        &k,
        t,
        numbers::DOS_OPEN,
        SyscallArgs::new(harness::cstring("RAM:mode.txt"), 0b100, 0, 0, 0, 0),
    );
    assert!(result.is_err(), "an invalid mode must be refused");
    assert_eq!(result.error, errors::RANGE);
}

/// `ExamineDir` walks a directory from an anchor, one entry per call.
///
/// The SDK call drives the whole chain from `vela_sys::api::examine_dir`
/// through the DOS library base, its generated jump table, the stub, the
/// hosted dispatch and the kernel's handler. The anchor is read and written
/// by the kernel, so the second call resumes after the first entry rather
/// than re-reading the directory.
#[test]
fn dos_examine_dir_walks_a_directory_from_an_anchor() {
    let _guard = ready();

    api::create_dir("RAM:walk").expect("CreateDir");
    for name in ["a.txt", "b.txt"] {
        let file =
            api::open(&format!("RAM:walk/{name}"), api::mode::WRITE).expect("Open for writing");
        api::close(file).expect("Close");
    }

    let mut anchor = AnchorPath::empty();
    let first = api::examine_dir(&mut anchor, Some("RAM:walk")).expect("ExamineDir");
    assert_eq!(
        first.as_deref(),
        Some("RAM:walk/a.txt"),
        "the first call must produce the first entry"
    );
    assert_eq!(anchor.ap_First, 1, "an entry was found");

    // A continuation passes no path: the kernel resumes from the anchor.
    let second = api::examine_dir(&mut anchor, None).expect("ExamineDir");
    assert_eq!(
        second.as_deref(),
        Some("RAM:walk/b.txt"),
        "the second call must resume after the first entry"
    );

    let end = api::examine_dir(&mut anchor, None).expect("ExamineDir");
    assert_eq!(end, None, "the walk must finish rather than repeat");
    assert_eq!(anchor.ap_FoundBreak, 1, "the end of the walk is recorded");
}
