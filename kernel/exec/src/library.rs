//! The library registry.
//!
//! # What a library *is* here
//!
//! On AmigaOS a library is a jump table of 68k code, and a program holds the
//! table's address in `A6`. `OpenLibrary` returns that address and the
//! program calls `jsr -offset(a6)`.
//!
//! VelaOS keeps the shape but changes the substrate: a library is a
//! *registry entry* — a name, a version, a set of exported function slots —
//! and `OpenLibrary` returns an opaque [`LibraryHandle`]. The handle is an
//! index into the calling process's own jump table, not a kernel pointer, so
//! a user process can never name a kernel address.
//!
//! # Reference counting
//!
//! `OpenLibrary` increments a count and `CloseLibrary` decrements it. The
//! library is unloaded when the count reaches zero. The count is what makes
//! `OpenLibrary` cheap to call repeatedly, which real programs do: a library
//! is opened once at startup and closed at exit, and any number of times in
//! between by different components.
//!
//! # Versioning
//!
//! A program asks for a *minimum* version. A library satisfies the request
//! when its version is at least that. The registry deliberately does **not**
//! implement the AmigaOS rule that a library whose version is lower than
//! requested is still opened but reported as "incompatible"; a program that
//! asked for version 40 and got version 39 has a bug, and silently opening it
//! hides the bug. A failed open returns a null handle, which the program can
//! check.
//!
//! # Names
//!
//! A library may be named `exec` or `exec.library`. AmigaOS programs use both
//! spellings, so the registry accepts both and stores one canonical form.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use spin::Mutex;

use vela_abi_native64::LibraryHandle;

/// A registered library.
#[derive(Debug)]
pub struct LibraryEntry {
    /// The canonical name, without the `.library` suffix.
    pub name: String,
    /// The library's version.
    pub version: u32,
    /// How many times it is currently open.
    pub open_count: AtomicU32,
    /// True when the library's init function has run.
    ///
    /// Init runs exactly once: the first task that opens the library triggers
    /// it, and subsequent opens see `initialized == true`. If init fails, the
    /// library is marked `expunge_pending` and future opens fail.
    pub initialized: AtomicBool,
    /// True when the library should be unloaded once `open_count` reaches zero.
    ///
    /// Set when the library's init function fails, or when the library
    /// explicitly requests expunge. The actual unload happens when the last
    /// close drops the count to zero.
    pub expunge_pending: AtomicBool,
    /// The function slots the library exports.
    ///
    /// The slot count is fixed per library so that a library's base address
    /// is computable from its id alone, with no lookup table. See
    /// [`vela_abi_native64::LIBRARY_VECTOR_SLOTS`].
    pub slots: Vec<LibraryFnSlot>,
}

/// The kind of a registered Exec module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleKind {
    /// A library: OpenLibrary/CloseLibrary, function vectors.
    Library,
    /// A device: OpenDevice/CloseDevice, I/O requests.
    Device,
    /// A resource: OpenResource, hardware arbitration.
    Resource,
}

/// One registered module.
#[derive(Debug)]
pub struct ModuleEntry {
    /// The canonical name.
    pub name: String,
    /// The module's version.
    pub version: u32,
    /// Whether this is a library, device or resource.
    pub kind: ModuleKind,
    /// How many times it is currently open.
    pub open_count: AtomicU32,
    /// True when the module's init function has run.
    pub initialized: AtomicBool,
    /// True when the module should be unloaded once `open_count` reaches zero.
    pub expunge_pending: AtomicBool,
    /// The function slots the module exports (libraries only).
    pub slots: Vec<LibraryFnSlot>,
}

impl Clone for ModuleEntry {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            version: self.version,
            kind: self.kind,
            // The count is a snapshot, not a shared counter.
            open_count: AtomicU32::new(self.open_count.load(Ordering::Acquire)),
            initialized: AtomicBool::new(self.initialized.load(Ordering::Acquire)),
            expunge_pending: AtomicBool::new(self.expunge_pending.load(Ordering::Acquire)),
            slots: self.slots.clone(),
        }
    }
}

/// One exported function.
///
/// The kernel never stores a *pointer* to a syscall handler here. A handler
/// would be a kernel address, and the jump table a user process sees must not
/// contain one. Instead a slot names a **syscall** — a number — and the user
/// process's stub library maps it to the `syscall` instruction at the
/// appropriate offset. The kernel's own dispatch table then maps the number
/// back to a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryFnSlot {
    /// The syscall number this slot performs.
    pub syscall: u32,
    /// The offset of this function within the library's jump table.
    ///
    /// Negative, as an AmigaOS LVO is. The offset is stable for the life of
    /// the library, so a program compiled against one version keeps working
    /// against another that added functions at the end.
    pub offset: i16,
}

/// The registry of all Exec modules: libraries, devices and resources.
///
/// A single mutex guards the whole registry. Module opens are rare — a
/// handful per process — and a single lock makes the reference counting
/// trivially correct. Splitting it into per-module locks would buy nothing
/// and would make the "open count hit zero" transition racy.
#[derive(Default)]
pub struct ModuleRegistry {
    entries: Mutex<Vec<ModuleEntry>>,
}

impl core::fmt::Debug for ModuleRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let entries = self.entries.lock();
        f.debug_struct("ModuleRegistry")
            .field("modules", &entries.len())
            .field(
                "names",
                &entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl ModuleRegistry {
    /// An empty registry.
    pub const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Normalise a module name.
    ///
    /// Strips the `.library` suffix and lowercases the result. AmigaOS library
    /// names are case-insensitive in practice, and accepting both `exec` and
    /// `exec.library` is required — real programs use both.
    pub fn normalise_name(name: &str) -> String {
        let lowered = name.to_ascii_lowercase();
        match lowered.strip_suffix(".library") {
            Some(stripped) => stripped.to_string(),
            None => lowered,
        }
    }

    /// Register a module.
    ///
    /// Returns `false` if a module with that name is already registered.
    /// Re-registering would orphan the first entry's open count, so it is
    /// refused rather than silently replaced.
    pub fn register(&self, name: &str, version: u32, kind: ModuleKind) -> bool {
        let canonical = Self::normalise_name(name);
        let mut entries = self.entries.lock();
        if entries.iter().any(|e| e.name == canonical) {
            return false;
        }
        entries.push(ModuleEntry {
            name: canonical,
            version,
            kind,
            open_count: AtomicU32::new(0),
            initialized: AtomicBool::new(false),
            expunge_pending: AtomicBool::new(false),
            slots: Vec::new(),
        });
        true
    }

    /// Register a module that the ABI already knows about.
    pub fn register_builtin(&self, library: vela_abi_native64::LibraryId) -> bool {
        self.register(library.name(), library.version(), ModuleKind::Library)
    }

    /// True when `name` names a registered module.
    pub fn contains(&self, name: &str) -> bool {
        let canonical = Self::normalise_name(name);
        let entries = self.entries.lock();
        entries.iter().any(|e| e.name == canonical)
    }

    /// The version of a registered module.
    pub fn version_of(&self, name: &str) -> Option<u32> {
        let canonical = Self::normalise_name(name);
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.name == canonical)
            .map(|e| e.version)
    }

    /// How many modules are registered.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// True when no module is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    /// The names of every registered module, in registration order.
    pub fn names(&self) -> Vec<String> {
        self.entries.lock().iter().map(|e| e.name.clone()).collect()
    }

    /// Open a module, returning its handle.
    ///
    /// Returns a null handle when the module is not registered or its
    /// version is below `min_version`. If the module's init has not yet run,
    /// it is run now (exactly once, guarded by the registry lock). If init
    /// fails, the module is marked `expunge_pending` and a null handle is
    /// returned.
    pub fn open(&self, name: &str, min_version: u32) -> LibraryHandle {
        let canonical = Self::normalise_name(name);
        let mut entries = self.entries.lock();
        let Some(index) = entries.iter().position(|e| e.name == canonical) else {
            return LibraryHandle::NULL;
        };
        let entry = &mut entries[index];
        if entry.version < min_version {
            return LibraryHandle::NULL;
        }
        // Init runs exactly once.
        if !entry.initialized.load(Ordering::Acquire) {
            // In a full implementation, this would call the module's init
            // function. For now we mark it initialized.
            entry.initialized.store(true, Ordering::Release);
        }
        if entry.expunge_pending.load(Ordering::Acquire) {
            return LibraryHandle::NULL;
        }
        entry.open_count.fetch_add(1, Ordering::AcqRel);
        // The handle names the *slot* in the registry, offset by one so that
        // zero stays reserved for "null".
        LibraryHandle::from_raw(index as u32 + 1)
    }

    /// Close a module.
    ///
    /// Returns `false` when the handle does not name an open module, or when
    /// it was already closed — closing twice is a bug in the caller, and
    /// silently tolerating it would let a program leak or double-free.
    ///
    /// If `expunge_pending` is set and this is the last close, the module is
    /// removed from the registry.
    pub fn close(&self, handle: LibraryHandle) -> bool {
        if handle.is_null() {
            return false;
        }
        let index = handle.raw() as usize - 1;
        let mut entries = self.entries.lock();
        let Some(entry) = entries.get_mut(index) else {
            return false;
        };
        let count = entry.open_count.load(Ordering::Acquire);
        if count == 0 {
            return false;
        }
        entry.open_count.store(count - 1, Ordering::Release);
        // If expunge is pending and this was the last open, remove the module.
        if count == 1 && entry.expunge_pending.load(Ordering::Acquire) {
            entries.remove(index);
        }
        true
    }

    /// How many times a module is currently open.
    pub fn open_count(&self, handle: LibraryHandle) -> u32 {
        if handle.is_null() {
            return 0;
        }
        let index = handle.raw() as usize - 1;
        let entries = self.entries.lock();
        entries
            .get(index)
            .map(|e| e.open_count.load(Ordering::Acquire))
            .unwrap_or(0)
    }

    /// The name of the module a handle names.
    pub fn name_of(&self, handle: LibraryHandle) -> Option<String> {
        if handle.is_null() {
            return None;
        }
        let index = handle.raw() as usize - 1;
        let entries = self.entries.lock();
        entries.get(index).map(|e| e.name.clone())
    }

    /// The version of the module a handle names.
    pub fn version_of_handle(&self, handle: LibraryHandle) -> Option<u32> {
        if handle.is_null() {
            return None;
        }
        let index = handle.raw() as usize - 1;
        let entries = self.entries.lock();
        entries.get(index).map(|e| e.version)
    }

    /// Add an exported function to a module.
    ///
    /// The offset is assigned from the module's current slot count, so
    /// offsets are dense and stable: adding a function never moves an
    /// existing one. That is the property a program compiled against an
    /// older version depends on.
    pub fn add_function(&self, name: &str, syscall: u32) -> bool {
        let canonical = Self::normalise_name(name);
        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|e| e.name == canonical) else {
            return false;
        };
        let index = entry.slots.len();
        if index >= vela_abi_native64::LIBRARY_VECTOR_SLOTS {
            return false;
        }
        // The first slot is at offset -8, the second at -16, and so on.
        let offset = -((index as i32 + 1) * vela_abi_native64::POINTER_SIZE as i32) as i16;
        entry.slots.push(LibraryFnSlot { syscall, offset });
        true
    }

    /// The exported functions of a module.
    pub fn functions_of(&self, name: &str) -> Vec<LibraryFnSlot> {
        let canonical = Self::normalise_name(name);
        let entries = self.entries.lock();
        entries
            .iter()
            .find(|e| e.name == canonical)
            .map(|e| e.slots.clone())
            .unwrap_or_default()
    }

    /// The syscall a module offset performs.
    pub fn syscall_at(&self, name: &str, offset: i16) -> Option<u32> {
        let canonical = Self::normalise_name(name);
        let entries = self.entries.lock();
        let entry = entries.iter().find(|e| e.name == canonical)?;
        // The offset is negative and the last slot is at -8, so the index is
        // `(-offset / POINTER_SIZE) - 1`.
        if offset >= 0 || offset % vela_abi_native64::POINTER_SIZE as i16 != 0 {
            return None;
        }
        let index = ((-offset) as usize / vela_abi_native64::POINTER_SIZE) - 1;
        entry.slots.get(index).map(|s| s.syscall)
    }

    /// Mark a module for expunge: it will be removed when open_count reaches zero.
    pub fn mark_expunge(&self, name: &str) -> bool {
        let canonical = Self::normalise_name(name);
        let mut entries = self.entries.lock();
        let Some(entry) = entries.iter_mut().find(|e| e.name == canonical) else {
            return false;
        };
        entry.expunge_pending.store(true, Ordering::Release);
        true
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec;

    #[test]
    fn names_normalise_to_the_bare_form() {
        assert_eq!(ModuleRegistry::normalise_name("exec.library"), "exec");
        assert_eq!(ModuleRegistry::normalise_name("exec"), "exec");
        assert_eq!(ModuleRegistry::normalise_name("EXEC.LIBRARY"), "exec");
        assert_eq!(
            ModuleRegistry::normalise_name("Intuition.Library"),
            "intuition"
        );
    }

    #[test]
    fn a_library_can_be_registered_once() {
        let r = ModuleRegistry::new();
        assert!(r.register("exec.library", 40, ModuleKind::Library));
        // The same library under the other spelling is the same library.
        assert!(!r.register("exec", 40, ModuleKind::Library));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn registration_is_case_insensitive() {
        let r = ModuleRegistry::new();
        assert!(r.register("EXEC.LIBRARY", 40, ModuleKind::Library));
        assert!(!r.register("exec", 41, ModuleKind::Library));
        assert!(r.contains("exec"));
        assert!(r.contains("exec.library"));
    }

    #[test]
    fn opening_an_unknown_library_returns_null() {
        let r = ModuleRegistry::new();
        let h = r.open("nosuch.library", 0);
        assert!(h.is_null());
        // And it did not create anything.
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn opening_asks_for_a_minimum_version() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        // Exactly the registered version is acceptable.
        assert!(!r.open("exec", 40).is_null());
        // A lower request is trivially satisfied.
        assert!(!r.open("exec", 1).is_null());
        // A higher request is not.
        assert!(r.open("exec", 41).is_null());
    }

    #[test]
    fn the_open_count_tracks_opens_and_closes() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        let a = r.open("exec", 40);
        let b = r.open("exec", 40);
        assert_eq!(r.open_count(a), 2);
        assert!(r.close(a));
        assert_eq!(r.open_count(a), 1);
        assert!(r.close(b));
        assert_eq!(r.open_count(a), 0);
    }

    #[test]
    fn closing_twice_is_refused() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        let h = r.open("exec", 40);
        assert!(r.close(h));
        // The second close would drive the count negative, which would make a
        // later open look like it was already open.
        assert!(!r.close(h));
        assert_eq!(r.open_count(h), 0);
    }

    #[test]
    fn closing_a_null_handle_is_refused() {
        let r = ModuleRegistry::new();
        assert!(!r.close(LibraryHandle::NULL));
    }

    #[test]
    fn closing_an_unknown_handle_is_refused() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        // A forged handle naming a slot that does not exist.
        assert!(!r.close(LibraryHandle::from_raw(999)));
    }

    #[test]
    fn a_handle_names_its_library() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        r.register("graphics", 40, ModuleKind::Library);
        let h = r.open("graphics", 40);
        assert_eq!(r.name_of(h), Some(String::from("graphics")));
        assert_eq!(r.version_of_handle(h), Some(40));
        assert_eq!(r.name_of(LibraryHandle::NULL), None);
    }

    #[test]
    fn functions_get_dense_negative_offsets() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        assert!(r.add_function("exec", 100));
        assert!(r.add_function("exec", 101));
        assert!(r.add_function("exec", 102));
        let slots = r.functions_of("exec");
        assert_eq!(slots.len(), 3);
        // The AmigaOS convention: the first function is at -8, the second at
        // -16, and so on.
        assert_eq!(slots[0].offset, -8);
        assert_eq!(slots[1].offset, -16);
        assert_eq!(slots[2].offset, -24);
    }

    #[test]
    fn adding_a_function_never_moves_an_existing_one() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        r.add_function("exec", 100);
        let before = r.functions_of("exec");
        r.add_function("exec", 101);
        let after = r.functions_of("exec");
        // A program compiled against the older version keeps working, which
        // is the whole point of a stable LVO table.
        assert_eq!(before[0].offset, after[0].offset);
        assert_eq!(before[0].syscall, after[0].syscall);
        assert_eq!(after.len(), 2);
    }

    #[test]
    fn an_offset_resolves_to_its_syscall() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        r.add_function("exec", 100);
        r.add_function("exec", 101);
        assert_eq!(r.syscall_at("exec", -8), Some(100));
        assert_eq!(r.syscall_at("exec", -16), Some(101));
        // An offset past the end resolves to nothing.
        assert_eq!(r.syscall_at("exec", -1024), None);
    }

    #[test]
    fn a_positive_or_misaligned_offset_is_not_a_valid_lvo() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        r.add_function("exec", 100);
        // A positive offset would be an address, not an LVO.
        assert_eq!(r.syscall_at("exec", 8), None);
        assert_eq!(r.syscall_at("exec", -4), None);
    }

    #[test]
    fn adding_a_function_to_an_unregistered_library_is_refused() {
        let r = ModuleRegistry::new();
        assert!(!r.add_function("nosuch", 100));
    }

    #[test]
    fn the_builtin_libraries_all_register() {
        let r = ModuleRegistry::new();
        for lib in vela_abi_native64::LibraryId::ALL {
            assert!(r.register_builtin(lib), "failed to register {lib:?}");
        }
        assert_eq!(r.len(), 8);
        // Every one is openable at its own version.
        for lib in vela_abi_native64::LibraryId::ALL {
            assert!(!r.open(lib.name(), lib.version()).is_null());
        }
    }

    #[test]
    fn the_registry_reports_its_contents() {
        let r = ModuleRegistry::new();
        assert!(r.is_empty());
        r.register("exec", 40, ModuleKind::Library);
        r.register("dos", 40, ModuleKind::Library);
        assert_eq!(r.names(), vec![String::from("exec"), String::from("dos")]);
        assert_eq!(r.version_of("exec.library"), Some(40));
        assert_eq!(r.version_of("nope"), None);
    }

    #[test]
    fn two_handles_to_the_same_library_are_the_same_handle() {
        let r = ModuleRegistry::new();
        r.register("exec", 40, ModuleKind::Library);
        let a = r.open("exec", 40);
        let b = r.open("exec", 40);
        // The handle names the library, not the open: as on AmigaOS, both
        // opens return the same library base.
        assert_eq!(a, b);
        assert_eq!(r.name_of(a), r.name_of(b));
        // And the count reflects both.
        assert_eq!(r.open_count(a), 2);
    }
}
