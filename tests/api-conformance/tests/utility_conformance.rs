//! `utility.library`, end to end.
//!
//! # What "end to end" means here
//!
//! Every assertion below travels the whole chain a real program would: the
//! SDK's `vela_sys::api::utility` → `library_base::utility::base()` → the
//! generated jump table → the generated stub → `hosted::dispatch` → the
//! kernel's real handler in `vela_kernel_syscall::utility_handlers`. Nothing
//! on that path is a test double, so a missing table entry, a stub that
//! returns a constant, or a handler that disagrees with the SDK's documented
//! argument order all fail here.
//!
//! # Why the last test bypasses the SDK
//!
//! `utility.library`'s three calls are the ones that take raw pointers, so
//! the case that matters most — a program handing the kernel an address it
//! must not touch — cannot be reached through a typed Rust API at all: the
//! SDK will only ever hand over pointers it owns. The final test therefore
//! dispatches the syscall by number, which is the only way to assert the
//! kernel's own refusals.

use std::sync::{Mutex, MutexGuard, OnceLock};

use vela_kernel_syscall::numbers::{self, SyscallArgs};
use vela_sys::abi::{tags, TagItem};
use vela_sys::api::utility::{find_tag, strcmp, strncpy};
use vela_sys::errors;
use vela_sys::hosted;
use vela_test_api_conformance::harness;

/// Guards every test in this file.
///
/// The services `hosted::install` publishes are process-global, so two tests
/// installing at once would leave one of them dispatching into a kernel the
/// other had already replaced. The utility handlers are themselves stateless,
/// so this lock is about *installation* rather than about the handlers.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The one kernel, installed once for the life of the process.
///
/// Leaked rather than owned: `hosted::install` takes `&'static` because the
/// services it publishes are read by calls that outlive any stack frame, and
/// a kernel dropped at the end of the first test would leave the second one
/// dispatching into freed memory.
fn services() -> &'static vela_hosted::Kernel {
    static SERVICES: OnceLock<&'static vela_hosted::Kernel> = OnceLock::new();
    SERVICES.get_or_init(|| {
        let kernel: &'static vela_hosted::Kernel = Box::leak(Box::new(harness::kernel()));
        // SAFETY: the kernel is leaked, so every reference handed to
        // `install` outlives the process — which is exactly the promise
        // `install` asks for.
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

/// Take the lock, install the services, and name a task for this thread.
///
/// `Exec::current_task` is thread-local — each test runs on its own thread —
/// so it is set here rather than in [`services`], which would only ever set
/// it for whichever test happened to run first.
fn ready() -> MutexGuard<'static, ()> {
    let guard = serial();
    let kernel = services();
    let task = harness::task(kernel);
    kernel.exec().set_current_task(task);
    guard
}

/// `FindTag` finds the item, and only the item, that carries the tag.
#[test]
fn utility_find_tag_returns_the_matching_items_data() {
    let _guard = ready();

    // A list that continues in a second buffer: `TAG_MORE` names it, and the
    // kernel walks into it. Both buffers are locals that outlive the call, so
    // the pointers the kernel follows are live for as long as it follows them.
    let rest = [
        TagItem {
            ti_Tag: TAG_B,
            ti_Data: 0x2222,
        },
        TagItem {
            ti_Tag: tags::TAG_END,
            ti_Data: 0,
        },
    ];
    let head = [
        // Skipped, never returned: `TAG_SKIP` is how a caller blanks a slot.
        TagItem {
            ti_Tag: tags::TAG_SKIP,
            ti_Data: 0xDEAD,
        },
        TagItem {
            ti_Tag: TAG_A,
            ti_Data: 0x1111,
        },
        // The *first* match wins. A list built by layering an override over a
        // default depends on this: the override is read and the default never
        // is.
        TagItem {
            ti_Tag: TAG_A,
            ti_Data: 0x9999,
        },
        TagItem {
            ti_Tag: tags::TAG_MORE,
            ti_Data: rest.as_ptr() as u64,
        },
    ];

    assert_eq!(
        find_tag(TAG_A, &head),
        Ok(0x1111),
        "the first item carrying the tag must win"
    );
    assert_eq!(
        find_tag(TAG_B, &head),
        Ok(0x2222),
        "the walk must continue into the list TAG_MORE names"
    );
    assert_eq!(
        find_tag(tags::TAG_SKIP, &head),
        Err(vela_sys::Error {
            code: errors::NOT_FOUND
        }),
        "a skipped item must not be findable"
    );
    assert_eq!(
        find_tag(TAG_MISSING, &head),
        Err(vela_sys::Error {
            code: errors::NOT_FOUND
        }),
        "a tag no item carries must be NOT_FOUND, not a zero value"
    );

    // `TAG_END` ends the list: an item physically after it is not part of it.
    let bounded = [
        TagItem {
            ti_Tag: TAG_A,
            ti_Data: 0x1111,
        },
        TagItem {
            ti_Tag: tags::TAG_END,
            ti_Data: 0,
        },
        TagItem {
            ti_Tag: TAG_C,
            ti_Data: 0x3333,
        },
    ];
    assert_eq!(
        find_tag(TAG_C, &bounded),
        Err(vela_sys::Error {
            code: errors::NOT_FOUND
        }),
        "nothing past TAG_END belongs to the list"
    );
}

/// `StrNCpy` writes at most `n` bytes and says how many it wrote.
#[test]
fn utility_strncpy_copies_at_most_n_bytes() {
    let _guard = ready();

    // `n` counts the terminator, so a full copy of a six-byte string needs
    // seven bytes of buffer and reports seven — six characters plus the NUL.
    let mut dst = [b'!'; 8];
    assert_eq!(strncpy(&mut dst, "abcdef", 8), Ok(7));
    assert_eq!(&dst[..6], b"abcdef");
    assert_eq!(dst[6], 0, "the copy must be NUL-terminated");
    assert_eq!(dst[7], b'!', "the eighth byte was never asked for");

    // Truncated: `n - 1` characters plus the terminator.
    let mut four = [0u8; 8];
    assert_eq!(strncpy(&mut four, "abcdef", 4), Ok(4));
    assert_eq!(&four[..4], b"abc\0");

    // One character less still fits, and the return value follows it down.
    let mut three = [0u8; 8];
    assert_eq!(strncpy(&mut three, "abcdef", 3), Ok(3));
    assert_eq!(&three[..3], b"ab\0");

    // `n == 1` leaves room for nothing but the terminator.
    let mut one = [0u8; 8];
    one.fill(b'!');
    assert_eq!(strncpy(&mut one, "abcdef", 1), Ok(1));
    assert_eq!(one[0], 0);
    assert_eq!(one[1], b'!', "only the terminator may be written");

    // `n == 0` writes nothing at all, terminator included.
    let mut none = [0u8; 8];
    none.fill(b'!');
    assert_eq!(strncpy(&mut none, "abcdef", 0), Ok(0));
    assert_eq!(none, [b'!'; 8], "a zero-sized request must not touch dst");

    // The tail of the buffer is left alone: `StrNCpy` copies, it does not pad.
    let mut tail = [b'!'; 8];
    assert_eq!(strncpy(&mut tail, "ab", 8), Ok(3));
    assert_eq!(&tail[..3], b"ab\0");
    assert_eq!(&tail[3..], b"!!!!!");

    // The SDK knows how long `dst` really is and refuses `n` that would have
    // the kernel write past the end of it.
    let mut small = [0u8; 4];
    assert_eq!(
        strncpy(&mut small, "abcdef", 100),
        Err(vela_sys::Error {
            code: errors::RANGE
        }),
        "n larger than the destination must be refused before the syscall"
    );
}

/// `StrCmp` gives the ordering of the two strings, byte by byte.
#[test]
fn utility_strcmp_orders_two_strings() {
    let _guard = ready();

    assert_eq!(strcmp("alpha", "beta"), Ok(core::cmp::Ordering::Less));
    assert_eq!(strcmp("beta", "alpha"), Ok(core::cmp::Ordering::Greater));
    assert_eq!(strcmp("alpha", "alpha"), Ok(core::cmp::Ordering::Equal));

    // The ordering is decided at the first differing byte, and an empty
    // string sorts before everything.
    assert_eq!(strcmp("abc", "abd"), Ok(core::cmp::Ordering::Less));
    assert_eq!(strcmp("", ""), Ok(core::cmp::Ordering::Equal));
    assert_eq!(strcmp("", "a"), Ok(core::cmp::Ordering::Less));

    // A prefix sorts before the string that continues it — the rule that makes
    // `strcmp` an ordering rather than an equality test.
    assert_eq!(strcmp("ab", "abc"), Ok(core::cmp::Ordering::Less));
    assert_eq!(strcmp("abc", "ab"), Ok(core::cmp::Ordering::Greater));

    // Byte-wise and unsigned, so the answer is not case folded: `A` (0x41)
    // sorts before `a` (0x61).
    assert_eq!(strcmp("Alpha", "alpha"), Ok(core::cmp::Ordering::Less));
    assert_eq!(strcmp("alpha", "Alpha"), Ok(core::cmp::Ordering::Greater));

    // A multi-byte character is several bytes, and they are compared as bytes.
    assert_eq!(strcmp("é", "z"), Ok(core::cmp::Ordering::Greater));
}

/// The kernel's own refusals, which no typed API can be used to reach.
#[test]
fn utility_rejects_a_pointer_it_cannot_reach() {
    let _guard = ready();
    let kernel = services();
    let task = harness::task(kernel);

    // A kernel address is refused by the range check, before the probe would
    // ever touch it. `FindTag` takes its list behind a pointer, so a program
    // that could make the kernel read from here would be reading kernel
    // memory through a tag lookup.
    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_FIND_TAG,
        SyscallArgs::new(TAG_A as u64, harness::KERNEL_ADDRESS, 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // An address in the user half that is not mapped passes the range check
    // and is refused by the probe instead — the case a range check alone
    // cannot catch.
    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_STRNCPY,
        SyscallArgs::new(
            harness::UNMAPPED_USER_ADDRESS,
            harness::cstring("abc"),
            8,
            0,
            0,
            0,
        ),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // Null is refused outright, without a probe at all: a null buffer is
    // never a legitimate argument, and the answer must be the same here as
    // on bare metal.
    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_STRCMP,
        SyscallArgs::new(0, harness::cstring("abc"), 0, 0, 0, 0),
    );
    assert_eq!(result.error, errors::BAD_POINTER);

    // `StrCmp`'s answer travels in one register, and "a sorts first" is
    // `u64::MAX` — the value a C `strcmp` would give, widened. This is the
    // kernel's own encoding; the SDK's `strcmp` turns it back into an
    // `Ordering`, which is what the test above asserts.
    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_STRCMP,
        SyscallArgs::new(harness::cstring("a"), harness::cstring("b"), 0, 0, 0, 0),
    );
    assert!(result.is_ok());
    assert_eq!(result.value, u64::MAX, "less must fold to -1");

    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_STRCMP,
        SyscallArgs::new(harness::cstring("b"), harness::cstring("a"), 0, 0, 0, 0),
    );
    assert!(result.is_ok());
    assert_eq!(result.value, 1, "greater must fold to 1");

    let result = harness::call(
        kernel,
        task,
        numbers::UTILITY_STRCMP,
        SyscallArgs::new(harness::cstring("a"), harness::cstring("a"), 0, 0, 0, 0),
    );
    assert!(result.is_ok());
    assert_eq!(result.value, 0, "equal must fold to 0");
}

/// A tag that no list in this file carries.
const TAG_A: u32 = 0x5600_0001;
/// A second tag, so "first match wins" is distinguishable from "any match".
const TAG_B: u32 = 0x5600_0002;
/// A third tag, used only past a `TAG_END`.
const TAG_C: u32 = 0x5600_0003;
/// A tag nothing carries, for the `NOT_FOUND` path.
const TAG_MISSING: u32 = 0x5600_FFFF;
