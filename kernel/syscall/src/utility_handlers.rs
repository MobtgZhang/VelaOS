//! The `utility.library` syscall handlers.
//!
//! # What `utility.library` is for
//!
//! On AmigaOS `utility.library` is the grab-bag of small helpers that do not
//! belong to any object: tag lists, string comparison, string copying. None of
//! them needs a service behind them — they are *operations on data the caller
//! already owns* — which is why they are the hardest library to get right
//! rather than the easiest.
//!
//! # Why they still go through the kernel
//!
//! Every one of them takes a user pointer. `FindTag` walks a list the program
//! handed over, `StrNCpy` writes into a buffer the program owns, and `StrCmp`
//! reads two strings. The kernel cannot dereference any of those directly: a
//! program that passed a kernel address would otherwise be reading or writing
//! kernel memory through a string helper. So each handler copies through
//! [`crate::copy`], which range-checks and probes, and each refuses a null
//! pointer outright — a null buffer is never a legitimate argument, and
//! refusing it before the probe keeps the failure mode identical on host and
//! on bare metal.
//!
//! # The bounds, and why each exists
//!
//! A tag list and a string are both *terminated by the data*, not by a length
//! the caller supplies. That makes them unbounded in a way a `Read` is not: a
//! list with no `TAG_END` in it, or a string with no NUL, would keep the
//! kernel walking until it hit an unmapped page. Each handler therefore has
//! its own explicit ceiling, and reaching it is an error rather than an
//! arbitrary stopping point.

use alloc::vec::Vec;

use vela_abi_native64::{tags, TagItem};

use crate::copy::{copy_from_user, copy_string_alloc, copy_to_user};
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// The largest number of tag items one walk will read.
///
/// A tag list is terminated by `TAG_END`, so a well-formed list is as long as
/// it is. This bound is what stops a list *without* one from keeping the
/// kernel reading: the walk stops with [`errors::RANGE`] rather than running
/// until it faults, which is the difference between an error a program can
/// see and an unbounded kernel loop.
pub const MAX_TAG_ITEMS: usize = 1024;

/// The largest number of `TAG_MORE` lists one walk will follow.
///
/// `TAG_MORE` hands the walk a pointer to another list, so a chain of them can
/// describe a graph rather than a list. Eight hops is far more than any real
/// caller needs and turns "a cycle in the tags" into an error instead of a
/// walk that never returns.
pub const MAX_TAG_HOPS: usize = 8;

/// `FindTag` — find a tag in a tag list.
///
/// Arguments: `tag` (the tag to look for), `list` (a user pointer to the first
/// [`TagItem`]). Returns the matching item's `ti_Data`, or
/// [`errors::NOT_FOUND`].
///
/// # Why it returns the data and not the item
///
/// AmigaOS returns a `struct TagItem *` — a pointer into the caller's own
/// list. VelaOS does not return pointers, ever: the kernel's first rule is
/// that a program receives an opaque value rather than an address, and a
/// pointer into a list the program passed in would be a pointer the kernel
/// validated once and the program could then move. Returning the value the
/// caller was actually after loses nothing: the only thing a caller does with
/// the found item is read its data.
///
/// # The reserved tags
///
/// `TAG_END` terminates the walk and `TAG_SKIP` marks an item to step over, so
/// neither can be *found* — a caller asking for tag `0` is asking for the
/// terminator. `TAG_MORE` continues the walk into the list its `ti_Data`
/// names, which is what makes a list of lists one list to a caller.
///
/// # The value's width
///
/// `ti_Data` is 64 bits here and the jump-table stub protocol carries 32 bits
/// of value (the other 32 hold the error), so a program that reads the result
/// through `utility.library`'s library base gets the low half. AmigaOS tag
/// data is 32-bit by construction, and a program storing a native64 pointer in
/// a tag and reading it back through the base should keep it within 32 bits or
/// read the item out of its own list.
pub fn find_tag(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
    let tag = args.get(0) as u32;
    let list = args.get(1);
    if list == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    match find_tag_in_user(list, tag) {
        Ok(Some(data)) => SyscallResult::ok(data),
        Ok(None) => SyscallResult::err(errors::NOT_FOUND),
        Err(code) => SyscallResult::err(code),
    }
}

/// Walk a user tag list looking for `wanted`.
///
/// Returns the matching item's data, `None` when the list ends without a
/// match, or an error code when the list itself cannot be read.
fn find_tag_in_user(list: u64, wanted: u32) -> Result<Option<u64>, i32> {
    let mut current = list;
    let mut read = 0usize;
    let mut hops = 0usize;
    loop {
        if read >= MAX_TAG_ITEMS {
            return Err(errors::RANGE);
        }
        read += 1;
        let mut item = TagItem {
            ti_Tag: 0,
            ti_Data: 0,
        };
        // SAFETY: the destination is a local of exactly the size being
        // copied, and `current` is validated by `copy_from_user`.
        unsafe {
            copy_from_user(
                &mut item as *mut TagItem as *mut u8,
                current,
                core::mem::size_of::<TagItem>(),
            )?;
        }
        match item.ti_Tag {
            tags::TAG_END => return Ok(None),
            tags::TAG_SKIP => {}
            tags::TAG_MORE => {
                hops += 1;
                if hops > MAX_TAG_HOPS {
                    return Err(errors::RANGE);
                }
                current = item.ti_Data;
                continue;
            }
            tag if tag == wanted => return Ok(Some(item.ti_Data)),
            _ => {}
        }
        current += core::mem::size_of::<TagItem>() as u64;
    }
}

/// `StrNCpy` — copy a string into a fixed-size buffer.
///
/// Arguments: `dst` (the user buffer), `src` (the NUL-terminated source), `n`
/// (the buffer's capacity). Returns the number of bytes written to `dst`,
/// counting the terminating NUL.
///
/// # The rule, stated exactly
///
/// At most `n` bytes are written: the source text truncated to `n - 1` bytes,
/// followed by a NUL. A `n` of zero writes nothing and returns zero. So the
/// destination is *always* NUL-terminated when there was room to terminate it,
/// which is the property a caller taking a fixed buffer actually needs — and
/// the return value is what tells a caller whether the text was truncated,
/// because it is the whole of what was written rather than the length of what
/// was asked for.
///
/// # Why the source is read first
///
/// The source is copied into the kernel before anything is written to the
/// destination. A source that is not readable is then an error that leaves the
/// caller's buffer untouched, rather than one that has already overwritten its
/// first bytes with a partial string.
///
/// # Errors
///
/// [`errors::BAD_POINTER`] for a null or out-of-range pointer on either side,
/// [`errors::BAD_STRING`] when the source is not NUL-terminated within
/// [`crate::copy::MAX_STRING_BYTES`].
pub fn strncpy(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
    let dst = args.get(0);
    let src = args.get(1);
    let n = args.get(2);
    if dst == 0 || src == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let text = match copy_string_alloc(src) {
        Ok(text) => text,
        Err(code) => return SyscallResult::err(code),
    };
    if n == 0 {
        return SyscallResult::ok(0);
    }
    // `n - 1` is safe: `n` is non-zero here. The bound is the smaller of the
    // source's length and the room the caller left for text.
    let take = (text.len() as u64).min(n - 1) as usize;
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(take + 1);
    out.extend_from_slice(&bytes[..take]);
    out.push(0);
    // SAFETY: `out` is a live local of `out.len()` bytes, and `dst` is
    // validated by `copy_to_user` before anything is written.
    match unsafe { copy_to_user(dst, out.as_ptr(), out.len()) } {
        Ok(_) => SyscallResult::ok(out.len() as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `StrCmp` — compare two strings.
///
/// Arguments: `a`, `b` (two NUL-terminated user strings). Returns the
/// comparison folded into a single integer:
///
/// | result | value |
/// |---|---|
/// | `a` sorts before `b` | `u64::MAX` (−1 in two's complement) |
/// | `a` equals `b` | `0` |
/// | `a` sorts after `b` | `1` |
///
/// # Why one integer rather than a signed result
///
/// A jump-table slot has one `u64` of return value, and a syscall has a value
/// *and* an error — so a negative "a sorts first" would have to live in the
/// value half while the error half said "no error", which is exactly the
/// ambiguity the split exists to remove. The three cases are therefore folded
/// into three unambiguous values, and the SDK turns them back into an
/// `Ordering`.
///
/// # The comparison itself
///
/// Byte-wise and unsigned, over the raw bytes rather than over decoded code
/// points. For the UTF-8 the SDK passes, byte order and code point order
/// agree, so a caller sees the ordering it would see from a C `strcmp`.
///
/// # Errors
///
/// [`errors::BAD_POINTER`] for a null or out-of-range pointer, and
/// [`errors::BAD_STRING`] when a string is not NUL-terminated within
/// [`crate::copy::MAX_STRING_BYTES`] or is not valid UTF-8.
pub fn strcmp(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
    let a = args.get(0);
    let b = args.get(1);
    if a == 0 || b == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let left = match copy_string_alloc(a) {
        Ok(text) => text,
        Err(code) => return SyscallResult::err(code),
    };
    let right = match copy_string_alloc(b) {
        Ok(text) => text,
        Err(code) => return SyscallResult::err(code),
    };
    SyscallResult::ok(fold_ordering(left.as_bytes().cmp(right.as_bytes())))
}

/// Fold an [`Ordering`] into the integer a syscall returns.
///
/// Less is `u64::MAX`, which is −1 in two's complement — the same answer a C
/// `strcmp` gives, widened to the register the result travels in.
pub const fn fold_ordering(ordering: core::cmp::Ordering) -> u64 {
    match ordering {
        core::cmp::Ordering::Less => u64::MAX,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_three_orderings_fold_to_three_distinct_values() {
        assert_eq!(fold_ordering(core::cmp::Ordering::Less), u64::MAX);
        assert_eq!(fold_ordering(core::cmp::Ordering::Equal), 0);
        assert_eq!(fold_ordering(core::cmp::Ordering::Greater), 1);
        assert_ne!(
            fold_ordering(core::cmp::Ordering::Less),
            fold_ordering(core::cmp::Ordering::Greater),
        );
    }

    #[test]
    fn less_folds_to_minus_one_in_two_s_complement() {
        // The value a C `strcmp` would return, so a program that treats the
        // result as a signed comparison gets the same answer it always did.
        assert_eq!(fold_ordering(core::cmp::Ordering::Less) as i64, -1);
        assert_eq!(fold_ordering(core::cmp::Ordering::Greater) as i64, 1);
        assert_eq!(fold_ordering(core::cmp::Ordering::Equal) as i64, 0);
    }

    #[test]
    fn a_null_pointer_is_refused_before_anything_is_read() {
        // Both handlers refuse a null pointer outright rather than letting the
        // probe discover it: a null buffer is never legitimate, and the answer
        // must be the same on the host, where the probe reads a page map, and
        // on bare metal, where it catches a fault.
        let ctx = context();
        let table = crate::dispatch::builtin_table();
        let result = table.dispatch(
            crate::numbers::UTILITY_STRNCPY,
            SyscallArgs::new(0, 0, 8, 0, 0, 0),
            ctx,
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        let result = table.dispatch(
            crate::numbers::UTILITY_STRCMP,
            SyscallArgs::new(0, 0, 0, 0, 0, 0),
            ctx,
        );
        assert_eq!(result.error, errors::BAD_POINTER);

        let result = table.dispatch(
            crate::numbers::UTILITY_FIND_TAG,
            SyscallArgs::new(0, 0, 0, 0, 0, 0),
            ctx,
        );
        assert_eq!(result.error, errors::BAD_POINTER);
    }

    /// A context borrowing leaked services, for the tests that need one.
    fn context() -> &'static SyscallContext<'static> {
        use alloc::boxed::Box;
        use vela_kernel_dos::Dos;
        use vela_kernel_exec::Exec;
        use vela_kernel_graphics::Graphics;
        use vela_kernel_icon::IconService;
        use vela_kernel_intuition::Intuition;
        use vela_kernel_workbench::Workbench;

        let exec: &'static Exec = Box::leak(Box::new(Exec::new()));
        let dos: &'static Dos = Box::leak(Box::new(Dos::new()));
        let graphics: &'static Graphics = Box::leak(Box::new(Graphics::new()));
        let intuition: &'static Intuition = Box::leak(Box::new(Intuition::new(640, 480)));
        let icon: &'static IconService = Box::leak(Box::new(IconService::new()));
        let workbench: &'static Workbench = Box::leak(Box::new(Workbench::new()));
        let task = exec.create_task();
        Box::leak(Box::new(SyscallContext {
            task,
            exec,
            dos,
            graphics,
            intuition,
            icon,
            workbench,
        }))
    }
}
