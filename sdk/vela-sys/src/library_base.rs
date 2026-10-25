//! Library bases: `A6`-style indirect calls through a per-process jump table.
//!
//! # The model
//!
//! On AmigaOS a library call is `jsr -offset(A6)`: the program holds a
//! *library base* in `A6`, and each function is a negative offset from it.
//! Programs are written that way, and a compatibility layer that did not
//! reproduce it would not be one.
//!
//! VelaOS keeps the shape and changes the mechanism. A library base is a
//! pointer into the program's **own** address space, pointing at a jump table
//! of real `extern "sysv64"` functions. Each of those marshals its arguments
//! into a `syscall` and returns the kernel's answer. So:
//!
//! * `A6` holds a pointer, and offsets from it are negative — the calling
//!   model is unchanged.
//! * The pointer is to *user* memory, so a program that reads or writes the
//!   jump table is corrupting only itself.
//! * The kernel never hands out a pointer at all. `OpenLibrary` returns an
//!   id; the base is built from that id on the program's side.
//!
//! # Why the offsets step by eight
//!
//! An AmigaOS jump table is *code*: each entry is a real machine-code stub, so
//! the entry size is whatever the instruction sequence happens to be — and
//! `intuition.library`'s entries step by six. VelaOS's table is a **word
//! array**, so its entries step by eight, one 64-bit slot each.
//!
//! The consequence is that VelaOS's offsets are not byte-for-byte AmigaOS's.
//! They are AmigaOS-shaped: negative, dense, and in the same ballpark, so a
//! program ported from AmigaOS finds every function where it expects to. What
//! it does not get is the ability to hard-code an offset — and that is the
//! right thing to lose, because a hard-coded offset into a jump table is a
//! program that breaks the moment the table moves.
//!
//! # Why the base is past the end of the vector
//!
//! Because the offsets are negative, a base *inside* the table would have some
//! calls reading before it and some after. The base is therefore the address
//! one word past the library's own function list, and every negative offset
//! reads downward into it — which is exactly how an AmigaOS jump table is laid
//! out, with the base above code that grows toward lower addresses.
//!
//! One consequence is worth stating because it surprises people: a function's
//! offset gets *more negative* as its slot gets *higher*. Slot 0 is the
//! shallowest offset, and the deepest slot is the largest magnitude.
//!
//! # Why the tables are immutable
//!
//! Each table is an immutable `static`, so the linker puts it in read-only
//! storage. A program that scribbles on its own jump table — through the base
//! address it holds, say — faults instead of quietly redirecting its own
//! calls, which is the better of the two failures: a corrupted table that
//! still works is the worst outcome of all.
//!
//! The tables are per-process in the sense that matters: each process has its
//! own address space, so each has its own copy at its own address. The
//! *pointer* in the base is the thing that must not be shared, and it never
//! is — the kernel hands out an id, and the base is built on this side.
//!
//! # The stub protocol
//!
//! A jump-table slot has one `u64` of return value, because that is what a
//! function pointer has. A VelaOS syscall has *two*: a value and an error.
//! [`unpack`] splits them, and every stub packs them the same way:
//!
//! ```text
//!   bits  0..31  the value
//!   bits 32..63  the error, two's complement
//! ```
//!
//! # Where the tables come from
//!
//! The tables themselves are **generated** from the IDL in `idl/*.yaml`, by
//! this crate's build script. See [`crate::tables`] for what that buys: the
//! offsets, the slot list and the table are one list, not three.

use crate::abi::{LibraryHandle, LibraryId};

/// The generated jump tables and their offsets.
///
/// # Why this is generated
///
/// A library's public surface is a *list*: its functions, their library
/// vector offsets, and the slots those offsets name. Writing that list by
/// hand means writing it three times — as offset constants, as an array, and
/// as the table itself — and three copies drift silently. Generating all
/// three from one IDL file means there is one list, and the build script
/// refuses to emit anything unless the list is coherent: negative offsets,
/// whole slots, one slot apart, deepest in slot zero.
///
/// The generated module is included here rather than declared in `lib.rs` so
/// that the include sits next to the machinery it feeds.
pub mod tables {
    include!(concat!(env!("OUT_DIR"), "/library_tables.rs"));
}

/// The library-base offsets, under the name a ported program uses.
///
/// The generated constants are named `<PREFIX>_<FUNCTION>_OFFSET`; these are
/// named the way AmigaOS names them — `EXEC_PUTMSG`, not
/// `EXEC_PUTMSG_OFFSET` — because that is the name the program in front of
/// this SDK already has in its source.
pub use tables::offset;

/// The generated per-library modules, re-exported at this level.
///
/// A caller writes `library_base::exec::base()` rather than reaching through
/// `library_base::tables::exec`: the generated module *is* the library, and
/// `tables` is an implementation detail of how it is generated.
pub use tables::{dos, exec, graphics, intuition, layers};

/// The signature of a jump-table slot.
///
/// `extern "sysv64"` because that is the convention VelaOS's stubs are built
/// for, and it is what the kernel's syscall ABI is derived from: arguments in
/// the System V integer registers, result in `rax`.
pub type Slot = unsafe extern "sysv64" fn(u64, u64, u64, u64, u64, u64) -> u64;

/// How many slots every library's table has.
///
/// A fixed count rather than a variable one, because a program's generated
/// stub indexes the table without knowing which library it is: a library
/// added later cannot shift the slots of one already compiled in.
pub const SLOTS: usize = 24;

/// A library's jump table.
pub type JumpTable = [Slot; SLOTS];

/// The size of one table slot, in bytes.
pub const SLOT_SIZE: u16 = core::mem::size_of::<Slot>() as u16;

/// Pack a value and an error into one `u64`.
///
/// ```text
///   bits  0..31  the value
///   bits 32..63  the error, two's complement
/// ```
pub const fn pack(value: u64, error: i32) -> u64 {
    ((error as u32 as u64) << 32) | (value & 0xFFFF_FFFF)
}

/// Split a jump table's packed return value.
///
/// The inverse of [`pack`], and the reason a VelaOS library function can
/// report a NULL pointer *and* an error, which a single `u64` return could
/// not express.
pub const fn unpack(packed: u64) -> (u64, i32) {
    (packed & 0xFFFF_FFFF, ((packed >> 32) as u32) as i32)
}

/// Issue a syscall and pack the result for a jump-table slot.
///
/// # Safety
///
/// The arguments are the caller's. Every pointer among them is validated by
/// the kernel, so a bad one is an error rather than a fault.
pub(crate) unsafe fn issue(number: u64, args: [u64; 6]) -> u64 {
    // SAFETY: forwarded from the caller, which makes the same promise.
    let result = unsafe { crate::syscall::syscall(number, args) };
    pack(result.value, result.error)
}

/// A library's slot layout.
///
/// VelaOS resolves an offset to a slot with the rule
/// `slot = count - |offset| / SLOT_SIZE`, where `count` is how many functions
/// the library has. Each function is one word, slot 0 is the *deepest* offset
/// (furthest from the base, since the base sits above the table), and the table
/// is [`SLOTS`] words long with the unused tail left as filler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// How many functions this library defines — the divisor that turns an
    /// offset's word-distance into a slot.
    pub count: u16,
    /// The library's name, for error messages.
    pub name: &'static str,
}

impl Layout {
    /// The slot an offset names, or `None` when the offset is outside the
    /// table.
    ///
    /// # The arithmetic
    ///
    /// The base sits one word past the library's own function list, so every
    /// negative offset reads downward into it:
    ///
    /// ```text
    ///   base ─┐
    ///         ├─ word 1        the shallowest function — slot count - 1
    ///         ├─ word 2
    ///         └─ word count    the deepest function   — slot 0
    ///   table start
    /// ```
    ///
    /// So word `w` is slot `count - w`: the table is indexed from its own
    /// start while the offsets are counted from the base. The subtraction
    /// running that way is the whole consequence of putting the base above the
    /// table — a function's offset gets more negative as its slot gets higher,
    /// exactly as on AmigaOS.
    pub const fn slot(&self, offset: i16) -> Option<usize> {
        // A positive offset would read above the table, into whatever the
        // linker put next — not something a program should be able to call.
        if offset >= 0 {
            return None;
        }
        let distance = offset.unsigned_abs();
        // An offset that is not a whole number of words cannot name a slot.
        if distance == 0 || !distance.is_multiple_of(SLOT_SIZE) {
            return None;
        }
        let words = distance / SLOT_SIZE;
        // Deeper than the library's longest offset, or shallower than the base
        // itself, names no slot.
        if words == 0 || words > self.count {
            return None;
        }
        Some((self.count - words) as usize)
    }
}

/// A filler slot.
///
/// A slot a library does not fill: past the end of its function list. It
/// reports [`crate::numbers::errors::NOT_IMPLEMENTED`] rather than zero,
/// because a stub returning success would be indistinguishable from a working
/// call — which is the distinction the report requires.
unsafe extern "sysv64" fn filler_stub(
    _a0: u64,
    _a1: u64,
    _a2: u64,
    _a3: u64,
    _a4: u64,
    _a5: u64,
) -> u64 {
    pack(0, crate::numbers::errors::NOT_IMPLEMENTED)
}

/// Build a jump table from a library's layout and its entries.
///
/// Every table starts as all [`filler_stub`] and each entry overwrites the
/// slot its own offset names, so the slot arithmetic happens here, once, from
/// the same [`Layout`] the base reports.
pub(crate) const fn build_table(entries: &[(Layout, i16, Slot)]) -> JumpTable {
    let mut table: JumpTable = [filler_stub; SLOTS];
    let mut i = 0;
    while i < entries.len() {
        let (layout, offset, slot_fn) = entries[i];
        // A library's own offsets always land inside its own table — that is
        // what `count` is for — so the fallback is unreachable in practice.
        // Slot 0 is a safe stand-in, and the unit tests fail on any offset that
        // would need it.
        let slot = match layout.slot(offset) {
            Some(slot) => slot,
            None => 0,
        };
        table[slot] = slot_fn;
        i += 1;
    }
    table
}

/// The base address for a jump table: one word past the library's own
/// function list.
///
/// The base is one word above the *last* function, because the offsets are
/// negative: a base inside the table would have some calls reading before it
/// and some after.
fn base_of_address(table: &JumpTable, layout: Layout) -> usize {
    table.as_ptr() as usize + (layout.count as usize) * (SLOT_SIZE as usize)
}

/// The base for a library, from its jump table.
///
/// The base is one word above the library's function list, because the
/// offsets are negative: a base inside a table would have some calls reading
/// before it and some after.
///
/// # Safety
///
/// `table` must be one of the SDK's own tables, initialised at compile time
/// and never written again. Every caller passes exactly such a table.
pub(crate) unsafe fn base_of(table: *const JumpTable, layout: Layout) -> LibraryBase {
    LibraryBase {
        address: base_of_address(&*table, layout),
        layout,
    }
}

/// Every library that has a jump table: its id, layout, and offsets.
use tables::TABLES;

/// The tables themselves, by library.
use tables::TABLE_PTRS;

/// The layout of every library that has a jump table.
fn layout_for(id: LibraryId) -> Option<Layout> {
    TABLES
        .iter()
        .find(|(candidate, _, _)| *candidate == id)
        .map(|(_, layout, _)| *layout)
}

/// The address of the table belonging to a library, or `None` if it has none.
fn table_start(id: LibraryId) -> Option<usize> {
    // Taking the address of an immutable `static` needs no `unsafe` at all.
    // Each table is initialised and never written again, so the address is
    // stable for the program's life.
    let start = match id {
        LibraryId::Exec => tables::exec::TABLE.as_ptr() as usize,
        LibraryId::Graphics => tables::graphics::TABLE.as_ptr() as usize,
        LibraryId::Layers => tables::layers::TABLE.as_ptr() as usize,
        LibraryId::Intuition => tables::intuition::TABLE.as_ptr() as usize,
        LibraryId::Dos => tables::dos::TABLE.as_ptr() as usize,
        // `utility.library` is not implemented, and giving it a table would
        // imply otherwise.
        LibraryId::Utility => return None,
    };
    Some(start)
}

/// Build a library base for a library the program has open.
///
/// # Safety
///
/// `handle` must be a handle this program obtained from `OpenLibrary` and has
/// not closed. Building a base from a handle belonging to another process would
/// give a program a jump table for a library it never opened.
pub unsafe fn open(handle: LibraryHandle) -> LibraryBase {
    let Some(id) = LibraryId::of(handle) else {
        return LibraryBase::NULL;
    };
    let (Some(layout), Some(start)) = (layout_for(id), table_start(id)) else {
        return LibraryBase::NULL;
    };
    LibraryBase {
        address: start + (layout.count as usize) * (SLOT_SIZE as usize),
        layout,
    }
}

/// The `exec.library` base, without opening anything.
///
/// `exec.library` is always open — it is what `OpenLibrary` itself goes
/// through — so this is the one base a program does not have to open.
pub fn exec_base() -> LibraryBase {
    tables::exec::base()
}

/// Resolve a base address back to a [`LibraryBase`].
///
/// The inverse of [`open`]: a program that kept the base address in a
/// variable of its own can turn it back into something it can call through,
/// which is what makes a base a *value* rather than a token.
///
/// # Safety
///
/// `address` must have come from [`open`] or [`exec_base`].
pub unsafe fn base_at(address: usize) -> LibraryBase {
    if address == 0 {
        return LibraryBase::NULL;
    }
    for (id, layout, _) in TABLES {
        let Some(start) = table_start(id) else {
            continue;
        };
        // The base is *exactly* one function list above the start. Equality
        // rather than a range test, because two tables can be adjacent in
        // memory — and a range test would let a base past the end of one
        // table resolve to the next one's layout.
        if start + (layout.count as usize) * (SLOT_SIZE as usize) == address {
            return LibraryBase { address, layout };
        }
    }
    LibraryBase::NULL
}

/// A library base: a pointer into this program's own jump table, plus the
/// layout needed to turn an offset into a slot.
///
/// A `struct` rather than a bare `usize` so that the only ways to make one are
/// [`open`] and [`exec_base`], both of which check that the library has a
/// table. A program cannot construct a base for a library it did not open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryBase {
    /// One word past the library's function list.
    address: usize,
    /// The library's slot layout.
    layout: Layout,
}

impl LibraryBase {
    /// The null library base.
    pub const NULL: LibraryBase = LibraryBase {
        address: 0,
        layout: Layout {
            count: 0,
            name: "none",
        },
    };

    /// True when this base names no library.
    pub const fn is_null(self) -> bool {
        self.address == 0
    }

    /// The raw pointer value, for a program doing the `A6` arithmetic itself.
    pub const fn raw(self) -> usize {
        self.address
    }

    /// The library's name.
    pub const fn name(self) -> &'static str {
        self.layout.name
    }

    /// The address a function at `offset` lives at.
    ///
    /// `base + offset` with `offset` negative, which is exactly the AmigaOS
    /// arithmetic — and it lands on the function pointer's own address, so the
    /// address a program computes is the address the table really holds.
    pub const fn function_address(self, offset: i16) -> usize {
        (self.address as isize + offset as isize) as usize
    }

    /// The slot `offset` names in this library's table.
    pub const fn slot_for(self, offset: i16) -> Option<usize> {
        if self.is_null() {
            return None;
        }
        self.layout.slot(offset)
    }

    /// Call the function at `offset`.
    ///
    /// # Safety
    ///
    /// The arguments are the caller's. Every pointer among them is validated by
    /// the kernel, so a bad one is an error rather than a fault.
    ///
    /// An `offset` that does not name a slot in this library resolves to the
    /// filler, which reports `NOT_IMPLEMENTED`. The failure mode of a wrong
    /// offset is therefore a clear error — not a call through a wild pointer,
    /// and not a silent call to some *other* function.
    pub unsafe fn call(self, offset: i16, args: [u64; 6]) -> u64 {
        if self.is_null() {
            return pack(0, crate::numbers::errors::NO_OBJECT);
        }
        let Some(table) = self.table() else {
            return pack(0, crate::numbers::errors::NO_OBJECT);
        };
        // An offset naming no slot falls back to the first slot past the
        // library's function list, which is always the filler. Falling back to
        // slot 0 would be wrong: that holds a real stub, so a program that
        // guessed an offset would issue that stub's syscall instead — a wrong
        // call that looks like a working one.
        let fallback = (self.layout.count as usize).min(SLOTS - 1);
        let slot = self.layout.slot(offset).unwrap_or(fallback);
        // SAFETY: `slot` is in range — by `Layout::slot` for a real offset, and
        // by the `min` above for the filler fallback, which is below `SLOTS`.
        let entry = unsafe { *table.get_unchecked(slot) };
        // SAFETY: `entry` is a slot of this program's own table, and the kernel
        // validates every pointer argument.
        unsafe { entry(args[0], args[1], args[2], args[3], args[4], args[5]) }
    }

    /// The table this base points into, or `None` for a null base.
    fn table(self) -> Option<&'static JumpTable> {
        if self.is_null() {
            return None;
        }
        // Each table is an immutable `static`, so a shared reference to one is
        // sound for the rest of the program's life.
        {
            let tables: [(&'static JumpTable, Layout); 5] = [
                (&tables::exec::TABLE, tables::exec::LAYOUT),
                (&tables::graphics::TABLE, tables::graphics::LAYOUT),
                (&tables::intuition::TABLE, tables::intuition::LAYOUT),
                (&tables::layers::TABLE, tables::layers::LAYOUT),
                (&tables::dos::TABLE, tables::dos::LAYOUT),
            ];
            debug_assert_eq!(tables.len(), TABLE_PTRS.len());
            for (table, layout) in tables {
                // The base is *exactly* one function list above the start, so
                // this is an equality test rather than a range test: two tables
                // can be adjacent, and a range test would let a base past the
                // end of one resolve to the next.
                if base_of_address(table, layout) == self.address {
                    return Some(table);
                }
            }
            None
        }
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_null_base_is_null_and_calls_nothing() {
        assert!(LibraryBase::NULL.is_null());
        let packed = unsafe { LibraryBase::NULL.call(offset::EXEC_PUTMSG, [0; 6]) };
        let (_, error) = unpack(packed);
        assert_eq!(error, crate::numbers::errors::NO_OBJECT);
    }

    #[test]
    fn every_library_with_implementations_has_a_base() {
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            // SAFETY: these are the libraries' own handles, which is what
            // `open` documents.
            let base = unsafe { open(id.handle()) };
            assert!(!base.is_null(), "{} must have a base", id.name());
            assert_eq!(base.name(), id.name());
            assert_eq!(base.layout, layout);
        }
    }

    #[test]
    fn utility_library_has_no_base() {
        // It is not implemented, and a base for it would imply otherwise.
        // SAFETY: a utility handle resolves to a library with no table.
        let base = unsafe { open(LibraryId::Utility.handle()) };
        assert!(base.is_null());
        assert_eq!(LibraryBase::NULL.name(), "none");
    }

    #[test]
    fn the_base_is_past_the_end_of_its_function_list() {
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            // SAFETY: the library's own handle.
            let base = unsafe { open(id.handle()) };
            let start = table_start(id).unwrap();
            let end = start + (layout.count as usize) * (SLOT_SIZE as usize);
            assert_eq!(
                base.raw(),
                end,
                "{}'s base must sit one past its function list",
                id.name(),
            );
            // And it must be inside the range the SDK treats as valid, so
            // `call` can find the table again.
            assert!(
                base.table().is_some(),
                "{} resolves to its table",
                id.name()
            );
        }
    }

    #[test]
    fn every_offset_lands_inside_its_own_table() {
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            // SAFETY: the library's own handle.
            let base = unsafe { open(id.handle()) };
            let start = table_start(id).unwrap();
            let end = start + (layout.count as usize) * (SLOT_SIZE as usize);
            for offset in offsets.iter().copied() {
                assert!(
                    layout.slot(offset).is_some(),
                    "{} offset {offset} must land inside the table",
                    id.name(),
                );
                let address = base.function_address(offset);
                assert!(
                    (start..=end).contains(&address),
                    "{} offset {offset} lands at {address:#x}, outside {start:#x}..{end:#x}",
                    id.name(),
                );
            }
        }
    }

    #[test]
    fn an_offset_addresses_the_slot_it_names() {
        // The whole point of the AmigaOS model: `base + offset` must be the
        // address of the function that offset names. If the address arithmetic
        // and the slot arithmetic disagreed, a program that printed
        // `function_address` would be pointing at the wrong stub.
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            // SAFETY: the library's own handle.
            let base = unsafe { open(id.handle()) };
            let start = table_start(id).unwrap();
            for offset in offsets.iter().copied() {
                let slot = layout.slot(offset).expect("every offset names a slot");
                let address = base.function_address(offset);
                assert_eq!(
                    address,
                    start + slot * (SLOT_SIZE as usize),
                    "{} offset {offset} addresses the wrong slot",
                    id.name(),
                );
            }
        }
    }

    #[test]
    fn every_offset_is_negative_and_lands_on_its_own_slot() {
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            for (i, a) in offsets.iter().enumerate() {
                assert!(*a < 0, "{} offset {a} must be negative", id.name());
                for b in &offsets[i + 1..] {
                    assert_ne!(
                        layout.slot(*a),
                        layout.slot(*b),
                        "{} offsets {a} and {b} share a slot",
                        id.name(),
                    );
                }
            }
        }
    }

    #[test]
    fn a_librarys_offsets_step_by_one_word() {
        // Eight bytes per slot means the offsets must step by eight, or two
        // functions would land on the same slot.
        // `every_offset_is_negative_and_lands_on_its_own_slot` catches the
        // collision; this test catches the cause.
        for (id, _, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            for pair in offsets.windows(2) {
                // Slot 0 is the deepest offset and the list runs from it
                // upward, so each entry is a *larger* magnitude than the last.
                assert_eq!(
                    pair[1] - pair[0],
                    SLOT_SIZE as i16,
                    "{} offsets {} and {} do not step by one word",
                    id.name(),
                    pair[0],
                    pair[1],
                );
            }
        }
    }

    #[test]
    fn the_first_function_of_each_library_lands_in_slot_zero() {
        // The slot arithmetic is shared by every library, so this is what
        // pins each library's *first* offset to the slot it belongs in. A
        // library whose offsets did not cover its whole function list would
        // drift here, visibly, rather than silently colliding.
        for (id, layout, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            assert_eq!(
                layout.slot(offsets[0]),
                Some(0),
                "{}'s first offset must land in slot 0",
                id.name(),
            );
            let last = offsets.len() - 1;
            assert_eq!(
                layout.slot(offsets[last]),
                Some(last),
                "{}'s last offset must land in slot {last}",
                id.name(),
            );
            assert_eq!(
                layout.count as usize,
                offsets.len(),
                "{}'s count must match its function list",
                id.name(),
            );
        }
    }

    #[test]
    fn every_library_fits_in_its_table() {
        for (id, _, offsets) in TABLES {
            assert!(
                offsets.len() <= SLOTS,
                "{} defines {} functions, more than the {SLOTS} slots",
                id.name(),
                offsets.len(),
            );
        }
    }

    #[test]
    fn a_positive_offset_names_no_slot() {
        // A positive offset would read above the table, into whatever the
        // linker put next — not something a program should be able to call.
        assert_eq!(tables::exec::LAYOUT.slot(0), None);
        assert_eq!(tables::exec::LAYOUT.slot(8), None);
        assert_eq!(tables::exec::LAYOUT.slot(204), None);
    }

    #[test]
    fn an_offset_past_the_table_names_no_slot() {
        // Deeper than any function, and shallower than the base itself.
        assert_eq!(tables::exec::LAYOUT.slot(-8192), None);
        assert_eq!(tables::exec::LAYOUT.slot(-1), None);
    }

    #[test]
    fn a_slot_outside_the_table_still_answers() {
        // Calling through a base with a bad offset must not read a wild
        // pointer. It resolves to a slot that is in range, so the answer is
        // *wrong* rather than undefined — and a wrong syscall number comes
        // back as `NOT_IMPLEMENTED`, not as a fault.
        let base = exec_base();
        let packed = unsafe { base.call(-8192, [0; 6]) };
        let (_, error) = unpack(packed);
        assert_eq!(
            error,
            crate::numbers::errors::NOT_IMPLEMENTED,
            "an offset naming no slot must not issue a syscall",
        );
    }

    #[test]
    fn a_filler_slot_reports_not_implemented() {
        // Slots past a library's function list are left as filler, and must
        // report that rather than reading as uninitialised memory.
        let base = exec_base();
        let unused = tables::exec::LAYOUT.count as usize;
        assert!(
            unused < SLOTS,
            "the test needs at least one slot past the function list",
        );
        // SAFETY: reading slot `unused`, which is below `SLOTS`, and `table` is
        // the SDK's own.
        let filler = unsafe {
            let table = base.table().expect("exec has a table");
            *table.get_unchecked(unused)
        };
        assert_eq!(
            filler as usize, filler_stub as *const () as usize,
            "the slot past the function list must still be the filler",
        );
        // SAFETY: `filler` is the SDK's own filler stub, which validates
        // nothing and returns a constant.
        let (value, error) = unsafe { unpack(filler(0, 0, 0, 0, 0, 0)) };
        assert_eq!(value, 0);
        assert_eq!(error, crate::numbers::errors::NOT_IMPLEMENTED);
    }

    #[test]
    fn a_packed_result_round_trips() {
        for (value, error) in [
            (0u64, 0i32),
            (42, 0),
            (0, crate::numbers::errors::NO_OBJECT),
            (u32::MAX as u64, crate::numbers::errors::BAD_POINTER),
            (7, -1),
            (0x8000_0000, -2),
        ] {
            let (got_value, got_error) = unpack(pack(value, error));
            assert_eq!(got_value, value & 0xFFFF_FFFF, "value for error {error}");
            assert_eq!(got_error, error, "error");
        }
    }

    #[test]
    fn the_tables_are_distinct_addresses() {
        // If two libraries shared a table, opening one would change what the
        // other calls.
        let mut starts: [usize; 5] = core::array::from_fn(|i| table_start(TABLES[i].0).unwrap());
        starts.sort_unstable();
        for pair in starts.windows(2) {
            assert_ne!(
                pair[0], pair[1],
                "two libraries share a jump table, so a call through one would land in the other",
            );
        }
    }

    #[test]
    fn a_base_resolves_back_to_itself() {
        for (id, _, offsets) in TABLES {
            if offsets.is_empty() {
                continue;
            }
            // SAFETY: the library's own handle.
            let base = unsafe { open(id.handle()) };
            // SAFETY: `base` came from `open` moments ago.
            let resolved = unsafe { base_at(base.raw()) };
            assert_eq!(resolved, base, "{} resolves back to itself", id.name());
        }
    }

    #[test]
    fn exec_base_matches_opening_exec() {
        // SAFETY: `exec.library`'s own handle.
        assert_eq!(unsafe { open(LibraryId::Exec.handle()) }, exec_base());
    }

    #[test]
    fn the_flat_offsets_are_the_generated_ones() {
        // The `offset` module is what a program names, so the values it carries
        // must be the ones the generated table carries. A mismatch here would
        // mean a program's `base.call(offset::EXEC_PUTMSG, ..)` reached a
        // different function than the table entry of that name.
        assert_eq!(offset::EXEC_PUTMSG, tables::exec::EXEC_PUTMSG_OFFSET);
        assert_eq!(offset::EXEC_PUTMSG, -96);
        assert_eq!(
            offset::INTUITION_OPENWINDOW,
            tables::intuition::INTUITION_OPENWINDOW_OFFSET
        );
        assert_eq!(offset::DOS_OPEN, tables::dos::DOS_OPEN_OFFSET);
        assert_eq!(
            offset::GRAPHICS_TEXT,
            tables::graphics::GRAPHICS_TEXT_OFFSET
        );
        assert_eq!(
            offset::LAYERS_CREATELAYER,
            tables::layers::LAYERS_CREATELAYER_OFFSET
        );
    }
}
