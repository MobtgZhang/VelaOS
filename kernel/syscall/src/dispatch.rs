//! The syscall dispatch table.
//!
//! # How a syscall is dispatched
//!
//! The gate receives the syscall number in `rax` and the arguments in the
//! argument registers. It looks the number up in the dispatch table and calls
//! the handler.
//!
//! # Why the table is a function-pointer array
//!
//! A `match` over 60 numbers would compile to a jump table, which is exactly
//! what the dispatch table is — but writing it as an array makes the
//! *dispatch* the thing that is uniform. Every syscall is reached the same
//! way, so there is exactly one place where the number becomes a handler, and
//! exactly one place where the handler's return value becomes the result.
//!
//! # The handler signature
//!
//! A handler takes the arguments and a context, and returns a
//! [`SyscallResult`]. The context carries the calling task's identity and a
//! way to reach the Exec service — a syscall that needs to open a library or
//! touch a port needs both, and threading them through as parameters would
//! make every signature longer for no benefit.
//!
//! # The unimplemented slots
//!
//! Slots for syscalls that are not implemented yet hold `None`, and the
//! dispatcher returns [`errors::NOT_IMPLEMENTED`]. The plan requires this
//! distinction: "a `stub` is not compatible", and a stub that silently
//! returned success would be indistinguishable from a working call.

use alloc::vec::Vec;

use spin::Mutex;

use vela_kernel_dos::{Bptr, Dos};
use vela_kernel_exec::Exec;
use vela_kernel_graphics::Graphics;
use vela_kernel_icon::IconService;
use vela_kernel_intuition::Intuition;
use vela_kernel_workbench::Workbench;

use vela_arch_common::MapError;

use crate::numbers::{self, errors, SyscallArgs, SyscallResult};

/// The context a syscall handler receives.
pub struct SyscallContext<'a> {
    /// The calling task's identifier.
    pub task: vela_kernel_exec::task::TaskId,
    /// The Exec service.
    pub exec: &'a Exec,
    /// The DOS service.
    pub dos: &'a Dos,
    /// The Graphics service.
    pub graphics: &'a Graphics,
    /// The Intuition service.
    ///
    /// Layers is reached through Intuition rather than being a separate
    /// context field: the window manager owns the layer stack, and a program
    /// that manipulates layers is manipulating the same stack its windows are
    /// on. Two references to "the layer stack" would be two ways to be wrong.
    pub intuition: &'a Intuition,
    /// The icon service.
    pub icon: &'a IconService,
    /// The Workbench service.
    pub workbench: &'a Workbench,
}

impl SyscallContext<'_> {
    /// Decode an argument as a file handle.
    ///
    /// A `BPTR` is opaque, so there is nothing to validate about the value
    /// itself — a forged handle simply names a file that is not open, and
    /// the DOS service says so. The decode exists so that every handler
    /// spells the conversion the same way, and so that the type is visible
    /// at the call site rather than implied by an `as u32`.
    pub fn dos_bptr(&self, raw: u64) -> Bptr {
        Bptr::from_raw(raw as u32)
    }
}

impl core::fmt::Debug for SyscallContext<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SyscallContext")
            .field("task", &self.task)
            .field("dos", &self.dos)
            .field("graphics", &self.graphics)
            .field("intuition", &self.intuition)
            .field("icon", &self.icon)
            .field("workbench", &self.workbench)
            .finish()
    }
}

/// A syscall handler.
pub type Handler = fn(SyscallArgs, &SyscallContext<'_>) -> SyscallResult;

/// The dispatch table.
pub struct DispatchTable {
    /// The handlers, indexed by syscall number.
    ///
    /// A `Vec` rather than a fixed array because the table grows as
    /// libraries are implemented, and a fixed array would need a size that
    /// is either too large or has to be changed in two places.
    handlers: Mutex<Vec<Option<Handler>>>,
}

impl core::fmt::Debug for DispatchTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let handlers = self.handlers.lock();
        let implemented = handlers.iter().filter(|h| h.is_some()).count();
        f.debug_struct("DispatchTable")
            .field("slots", &handlers.len())
            .field("implemented", &implemented)
            .finish()
    }
}

impl Default for DispatchTable {
    fn default() -> Self {
        Self::new()
    }
}

impl DispatchTable {
    /// An empty dispatch table.
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(Vec::new()),
        }
    }

    /// Install a handler for a syscall number.
    ///
    /// Installing a handler twice replaces the first, which is what a
    /// library that is loaded and unloaded needs.
    pub fn install(&self, number: u64, handler: Handler) {
        let mut handlers = self.handlers.lock();
        let index = number as usize;
        if handlers.len() <= index {
            handlers.resize(index + 1, None);
        }
        handlers[index] = Some(handler);
    }

    /// Remove a handler.
    pub fn remove(&self, number: u64) {
        let mut handlers = self.handlers.lock();
        let index = number as usize;
        if index < handlers.len() {
            handlers[index] = None;
        }
    }

    /// True when a handler is installed for `number`.
    pub fn is_implemented(&self, number: u64) -> bool {
        let handlers = self.handlers.lock();
        handlers
            .get(number as usize)
            .map(|h| h.is_some())
            .unwrap_or(false)
    }

    /// How many slots the table has.
    pub fn slot_count(&self) -> usize {
        self.handlers.lock().len()
    }

    /// How many slots have a handler.
    pub fn implemented_count(&self) -> usize {
        self.handlers.lock().iter().filter(|h| h.is_some()).count()
    }

    /// Dispatch a syscall.
    ///
    /// This is the whole of the gate's work once the registers are in place.
    /// An unknown number, or one with no handler, returns
    /// [`errors::NOT_IMPLEMENTED`] — never a panic, because a panic at ring 0
    /// on a user stack takes the kernel down with it.
    pub fn dispatch(
        &self,
        number: u64,
        args: SyscallArgs,
        context: &SyscallContext<'_>,
    ) -> SyscallResult {
        let handler = {
            let handlers = self.handlers.lock();
            handlers.get(number as usize).copied().flatten()
        };
        match handler {
            Some(handler) => handler(args, context),
            None => SyscallResult::err(errors::NOT_IMPLEMENTED),
        }
    }
}

/// The Exec-library handlers.
///
/// These are the calls that make up the AmigaOS Exec API as VelaOS exposes
/// it. Each one is a thin adapter: it validates its arguments, calls into
/// [`Exec`], and turns the result into a [`SyscallResult`].
///
/// The adapters are where the *user-facing* validation lives. The Exec
/// service itself is trusted and does not re-check what its own internal
/// callers already know.
pub mod exec_handlers {
    use super::*;
    use crate::copy::{copy_from_user, copy_string_alloc, copy_to_user};
    use vela_abi_native64::Message;

    /// `CreateMsgPort` — create a message port.
    ///
    /// Arguments: `sig` (the signal mask), `sig_wait` (the wait mask),
    /// `flags` (port flags). Returns the port's handle.
    pub fn create_msg_port(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let sig = args.get(0) as u32;
        let sig_wait = args.get(1) as u32;
        let flags = args.get(2) as u32;
        let port = ctx.exec.create_port(ctx.task, sig, sig_wait, flags);
        SyscallResult::ok(port.0 as u64)
    }

    /// `DeleteMsgPort` — delete a message port.
    ///
    /// Arguments: `port`. Returns zero.
    pub fn delete_msg_port(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let port = crate::ids::PortId(args.get(0) as u32);
        match ctx.exec.delete_port(port) {
            Ok(_) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `PutMsg` — enqueue a message on a port.
    ///
    /// Arguments: `port`, `message_ptr` (a user pointer to a
    /// [`Message`]). Returns zero.
    pub fn put_msg(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let port = crate::ids::PortId(args.get(0) as u32);
        let message_ptr = args.get(1);
        let mut message = Message::empty();
        // SAFETY: the message is copied *from* user space into a local, so
        // the kernel never dereferences the user's pointer itself.
        match unsafe {
            copy_from_user(
                &mut message as *mut Message as *mut u8,
                message_ptr,
                core::mem::size_of::<Message>(),
            )
        } {
            Ok(_) => {}
            Err(e) => return SyscallResult::err(e),
        }
        match ctx.exec.put_msg(port, message) {
            Ok(_) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `GetMsg` — dequeue a message from a port.
    ///
    /// Arguments: `port`, `message_ptr` (a user pointer to receive the
    /// message). Returns zero.
    ///
    /// The message is copied out *before* it is removed from the queue. A
    /// copy that fails must not consume the message: the program passed a
    /// bad pointer, and losing the message as well would turn a recoverable
    /// mistake into a lost request.
    pub fn get_msg(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let port = crate::ids::PortId(args.get(0) as u32);
        let message_ptr = args.get(1);
        // The message is copied out and *then* removed. A copy that fails
        // must not consume the message: the program passed a bad pointer, and
        // losing the message as well would turn a recoverable mistake into a
        // lost request.
        //
        // The copy happens inside `get_msg_if`'s callback, so the peek and
        // the removal are one atomic step with respect to the port.
        let mut failure: i32 = 0;
        let result = ctx.exec.get_msg_if(port, |message| {
            // SAFETY: the destination is a user pointer that was validated
            // by `copy_to_user`.
            match unsafe {
                copy_to_user(
                    message_ptr,
                    message as *const Message as *const u8,
                    core::mem::size_of::<Message>(),
                )
            } {
                Ok(_) => true,
                Err(e) => {
                    failure = e;
                    false
                }
            }
        });
        match result {
            Ok(_) => SyscallResult::ok(0),
            // A copy failure is reported as the copy's error, not as "no
            // message": the message is still there.
            Err(_) if failure != 0 => SyscallResult::err(failure),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `ReplyMsg` — reply to a message.
    ///
    /// Arguments: `request_ptr` (a user pointer to the request),
    /// `reply_ptr` (a user pointer to the reply). Returns zero.
    pub fn reply_msg(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let request_ptr = args.get(0);
        let reply_ptr = args.get(1);
        let mut request = Message::empty();
        let mut reply = Message::empty();
        // SAFETY: both copies are from user space into locals.
        unsafe {
            if copy_from_user(
                &mut request as *mut Message as *mut u8,
                request_ptr,
                core::mem::size_of::<Message>(),
            )
            .is_err()
            {
                return SyscallResult::err(errors::BAD_POINTER);
            }
            if copy_from_user(
                &mut reply as *mut Message as *mut u8,
                reply_ptr,
                core::mem::size_of::<Message>(),
            )
            .is_err()
            {
                return SyscallResult::err(errors::BAD_POINTER);
            }
        }
        match ctx.exec.reply_msg(&request, reply) {
            Ok(_) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `WaitPort` — wait for a message on a port.
    ///
    /// Arguments: `port`, `message_ptr` (a user pointer to receive the
    /// message). Returns zero if a message was received, or one if the wait
    /// ended without one.
    pub fn wait_port(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let port = crate::ids::PortId(args.get(0) as u32);
        let message_ptr = args.get(1);
        match ctx.exec.wait_port(port, ctx.task) {
            Ok(Some(message)) => {
                // SAFETY: the destination is a user pointer validated by
                // `copy_to_user`.
                match unsafe {
                    copy_to_user(
                        message_ptr,
                        &message as *const Message as *const u8,
                        core::mem::size_of::<Message>(),
                    )
                } {
                    Ok(_) => SyscallResult::ok(0),
                    Err(e) => SyscallResult::err(e),
                }
            }
            // The wait ended without a message: the caller must handle that,
            // and the value `1` says so.
            Ok(None) => SyscallResult::ok(1),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `Wait` — wait for a signal.
    ///
    /// Arguments: `mask`. Returns the mask of satisfied bits.
    pub fn wait(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let mask = args.get(0) as u32;
        match ctx.exec.wait(ctx.task, mask) {
            Ok(Some(satisfied)) => SyscallResult::ok(satisfied as u64),
            // Still blocked: the scheduler will resume the task when the
            // signal arrives.
            Ok(None) => SyscallResult::ok(u64::MAX),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `SetSignal` — set bits in the calling task's signal set.
    ///
    /// Arguments: `mask`. Returns the new signal set.
    pub fn set_signal(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let mask = args.get(0) as u32;
        match ctx.exec.set_signal(ctx.task, mask) {
            Ok(current) => SyscallResult::ok(current as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `SetExcept` — clear bits in the calling task's signal set.
    ///
    /// Arguments: `mask`. Returns the new signal set.
    pub fn set_except(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let mask = args.get(0) as u32;
        match ctx.exec.set_except(ctx.task, mask) {
            Ok(current) => SyscallResult::ok(current as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `Signal` — raise a signal in another task's set.
    ///
    /// Arguments: `task`, `mask`. Returns the new signal set.
    pub fn signal(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let task = vela_kernel_exec::task::TaskId(args.get(0) as u32);
        let mask = args.get(1) as u32;
        match ctx.exec.signal(task, mask) {
            Ok(current) => SyscallResult::ok(current as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `OpenLibrary` — open a library.
    ///
    /// Arguments: `name_ptr` (a user pointer to a NUL-terminated string),
    /// `version`. Returns the library handle, or zero on failure.
    pub fn open_library(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        let version = args.get(1) as u32;
        let name = match copy_string_alloc(name_ptr) {
            Ok(n) => n,
            Err(e) => return SyscallResult::err(e),
        };
        let handle = ctx.exec.open_library(&name, version);
        // A null handle is a *successful* open that found nothing: the
        // caller distinguishes it from an error by the handle being zero.
        SyscallResult::ok(handle.raw() as u64)
    }

    /// `CloseLibrary` — release a library.
    ///
    /// Arguments: `handle`. Returns zero on success, or one if the library
    /// could not be closed.
    pub fn close_library(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let handle = vela_abi_native64::LibraryHandle::from_raw(args.get(0) as u32);
        if ctx.exec.close_library(handle) {
            SyscallResult::ok(0)
        } else {
            SyscallResult::err(errors::NO_OBJECT)
        }
    }

    /// `Forbid` — enter the classic critical section.
    pub fn forbid(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        SyscallResult::ok(ctx.exec.forbid() as u64)
    }

    /// `Permit` — leave the classic critical section.
    pub fn permit(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        SyscallResult::ok(ctx.exec.permit() as u64)
    }

    /// `Yield` — give up the rest of the time slice.
    ///
    /// The call records the request and returns; the scheduler is what
    /// honours it. Recording rather than switching here is the difference
    /// between a syscall and a context switch: a handler runs on the
    /// *caller's* kernel stack, and switching stacks in the middle of a
    /// handler would leave that stack's frame where nothing would ever pop
    /// it. The scheduler, which already owns the run queue, requeues the task
    /// as soon as the body returns — which is after this call has returned to
    /// the program, so the program still sees a normal successful return.
    ///
    /// The value is always zero: a yield has no result to report, and
    /// reporting "how long we yielded" would be a number the kernel cannot
    /// honestly compute.
    pub fn yield_now(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        ctx.exec.yield_task();
        SyscallResult::ok(0)
    }

    /// `AddPort` — put a port into service and report its signal mask.
    ///
    /// Arguments: `port`. Returns the port's `mp_sig` mask.
    ///
    /// # Why the checks are in this order
    ///
    /// A handle that names no port is [`errors::NO_OBJECT`] and a handle that
    /// names someone else's port is [`errors::NOT_OWNER`], and the order
    /// matters: telling a caller "that is not yours" about a handle that does
    /// not exist at all would leak nothing but would send it looking for an
    /// owner it will never find. Existence first, ownership second.
    ///
    /// # Why the mask and not a bit
    ///
    /// A port may raise several signal bits, and a caller that waited on only
    /// one of them would sleep through the others. The whole mask is what the
    /// caller needs to build its wait from, and it is what the kernel will
    /// actually raise — so the value cannot disagree with the behaviour.
    pub fn add_port(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let port = crate::ids::PortId(args.get(0) as u32);
        let Some(owner) = ctx.exec.ports.port_owner(port) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        if owner != ctx.task {
            return SyscallResult::err(errors::NOT_OWNER);
        }
        let mask = ctx.exec.ports.port_sig(port).unwrap_or(0);
        SyscallResult::ok(mask as u64)
    }

    /// `GetTaskID` — the calling task's identifier.
    pub fn get_task_id(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        SyscallResult::ok(ctx.task.0 as u64)
    }

    /// `PutChar` — write one byte to the console.
    ///
    /// Arguments: `byte`.
    ///
    /// # Why this is a syscall at all
    ///
    /// A user program has no console of its own. Its alternatives are worse:
    /// a private buffer it can fill and never flush, or a shared one it has to
    /// lock and cannot prove the kernel will ever read. Writing the byte
    /// straight to the port costs one syscall per byte and is the only option
    /// where a program's output interleaves *correctly* with the kernel's
    /// rather than arriving as a block that may or may not be ordered against
    /// a concurrent log line.
    ///
    /// The cost is real and is why a program that prints in a loop should
    /// batch — but batching is a decision the program can make, and not having
    /// the ability to print at all is not.
    pub fn put_char(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        // Only the low byte is taken. A program passing a wider value is not
        // an error — it is passing a number, and truncating it is the defined
        // behaviour rather than a fault, exactly as `putchar` would.
        vela_arch::serial::putc(args.get(0) as u8);
        // The same byte onto the framebuffer console, so the output is
        // visible in the QEMU window rather than only in the serial log.
        // When there is no display the console is a no-op, and serial alone
        // is the whole story — the headless configuration.
        vela_kernel_console::putc(args.get(0) as u8);
        SyscallResult::ok(0)
    }

    /// `PowerOff` — stop the machine.
    ///
    /// Arguments: `status`.
    ///
    /// The exit status travels through QEMU's debug-exit device, so a boot
    /// smoke test can tell a program that finished from one that was killed by
    /// its timeout. That distinction is the reason this call exists: without
    /// it, "the guest stopped" and "the guest hung" look identical.
    pub fn power_off(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        vela_arch::qemu_exit::power_off(args.get(0) as u32)
    }

    /// `GetChar` — read one byte from the console.
    ///
    /// No arguments. Returns the byte as the value, or `RANGE` when nothing
    /// has arrived yet. The PS/2 keyboard is polled first — the keys the user
    /// types into the QEMU window — and the serial port falls back second, so
    /// a host-driven session through `-serial stdio` still works. The UART is
    /// polled, so this never blocks: a shell spins on it between redraws.
    pub fn get_char(_args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        if let Some(byte) = vela_kernel_console::keyboard::read_char() {
            return SyscallResult::ok(byte as u64);
        }
        match vela_arch::serial::getc() {
            Some(byte) => SyscallResult::ok(byte as u64),
            None => SyscallResult::err(errors::RANGE),
        }
    }

    /// `MapPage` — map one page of user memory.
    ///
    /// Arguments: `va` (the virtual address to map). Returns the address that
    /// was mapped, or an error.
    ///
    /// # Why a program cannot map its own memory
    ///
    /// A process needs memory, and a process cannot be handed memory it has
    /// no way to obtain more of. So the boundary gives it exactly two ways to
    /// grow: map a page, or extend the break. Both go through the kernel
    /// because both consume physical frames, and only the kernel's allocator
    /// owns those.
    ///
    /// # Why the page is zeroed
    ///
    /// The frame the kernel hands out may have held another process's data a
    /// moment ago. Handing it back unread would leak that data, so the page
    /// is zeroed before the program sees it.
    pub fn map_page(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let va = args.get(0);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            // The task has no address space. That is not a "not implemented"
            // — it is a task the kernel never gave a space to, which is a
            // kernel bug rather than a program error, and the honest report
            // is "no object".
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        // SAFETY: the installed frame source is the kernel's own allocator,
        // installed once during boot and alive for the kernel's lifetime. It
        // is the only thing that hands out physical frames to user spaces.
        let result = unsafe { space.map_user_page(va, frames) };
        match result {
            Ok(va) => SyscallResult::ok(va),
            Err(MapError::MisalignedVirtual) => SyscallResult::err(errors::RANGE),
            Err(_) => SyscallResult::err(errors::NO_MEMORY),
        }
    }

    /// `Brk` — move the program break.
    ///
    /// Arguments: `addr` (zero queries; otherwise the requested break).
    /// Returns the break *actually in force*.
    ///
    /// # Why the return value is the break rather than a status
    ///
    /// The call is transactional: a program that asks for more memory than
    /// exists learns the real limit from the return value, rather than
    /// discovering the failure later at an address it cannot write. A
    /// program that wants to know whether the request succeeded compares the
    /// result with what it asked for.
    pub fn brk(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let addr = args.get(0);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.set_brk(addr, frames) {
            Ok(brk) => SyscallResult::ok(brk),
            Err(_) => SyscallResult::err(errors::NO_MEMORY),
        }
    }

    /// `AllocMem` — allocate memory in the calling task's space.
    ///
    /// Arguments: `size`, `flags`. Returns the allocation's address.
    ///
    /// The allocation is mapped into the task's arena and zeroed. `flags`
    /// selects the `MEMF_*` policy; this kernel has one arena, so the flags do
    /// not change where the allocation lands.
    pub fn alloc_mem(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let size = args.get(0);
        let flags = args.get(1) as u32;
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.alloc_mem(size, flags, frames) {
            Ok(va) => SyscallResult::ok(va),
            Err(MapError::MisalignedVirtual) => SyscallResult::err(errors::RANGE),
            Err(_) => SyscallResult::err(errors::NO_MEMORY),
        }
    }

    /// `FreeMem` — free an allocation made by `AllocMem`.
    ///
    /// Arguments: `address`, `size`. Returns zero.
    pub fn free_mem(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let va = args.get(0);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.free_mem(va, frames) {
            Ok(_) => SyscallResult::ok(0),
            Err(_) => SyscallResult::err(errors::BAD_POINTER),
        }
    }

    /// `AllocVec` — allocate a vector, with the same semantics as `AllocMem`.
    ///
    /// Arguments: `size`, `flags`. Returns the allocation's address.
    pub fn alloc_vec(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        alloc_mem(args, ctx)
    }

    /// `FreeVec` — free a vector made by `AllocVec`.
    ///
    /// Arguments: `address`. Returns zero.
    pub fn free_vec(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        free_mem(args, ctx)
    }

    /// `AvailMem` — report the uncommitted bytes in the task's arena.
    ///
    /// Arguments: `flags`. Returns the number of bytes the arena can still
    /// hand out.
    pub fn avail_mem(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        SyscallResult::ok(space.avail_mem())
    }

    /// `CreatePool` — create a memory pool for the calling task.
    ///
    /// Arguments: `flags`. Returns the pool's opaque handle.
    pub fn create_pool(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let flags = args.get(0) as u32;
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        SyscallResult::ok(space.create_pool(flags))
    }

    /// `AllocPooled` — allocate from a pool.
    ///
    /// Arguments: `pool`, `size`. Returns the allocation's address.
    pub fn alloc_pooled(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let pool = args.get(0);
        let size = args.get(1);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.alloc_pooled(pool, size, frames) {
            Ok(va) => SyscallResult::ok(va),
            Err(MapError::MisalignedVirtual) => SyscallResult::err(errors::RANGE),
            Err(_) => SyscallResult::err(errors::NO_MEMORY),
        }
    }

    /// `FreePooled` — free an allocation from a pool.
    ///
    /// Arguments: `pool`, `address`. Returns zero.
    pub fn free_pooled(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let pool = args.get(0);
        let va = args.get(1);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.free_pooled(pool, va, frames) {
            Ok(_) => SyscallResult::ok(0),
            Err(_) => SyscallResult::err(errors::BAD_POINTER),
        }
    }

    /// `DeletePool` — delete a pool and free everything allocated from it.
    ///
    /// Arguments: `pool`. Returns zero.
    pub fn delete_pool(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let pool = args.get(0);
        let Some(space) = crate::task_space::spaces().get(ctx.task) else {
            return SyscallResult::err(errors::NO_OBJECT);
        };
        let Some(frames) = crate::task_space::frames() else {
            return SyscallResult::err(errors::NO_MEMORY);
        };
        match space.delete_pool(pool, frames) {
            Ok(_) => SyscallResult::ok(0),
            Err(_) => SyscallResult::err(errors::BAD_POINTER),
        }
    }

    /// `ClearScreen` — clear the console screen.
    ///
    /// No arguments. Returns zero on success.
    ///
    /// The framebuffer console erases its own grid and homes its cursor,
    /// which is the clear the user in front of the QEMU window sees. The
    /// serial log additionally receives the ANSI clear sequence, because a
    /// serial console has no grid to erase — the terminal emulator on the
    /// far end is the thing that clears itself.
    pub fn clear_screen(_args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        // ANSI ESC [ 2 J / ESC [ H for terminal emulators on the serial log.
        crate::print_console("\x1b[2J\x1b[H");
        // The real clear: erase the framebuffer console's grid. With no
        // display this is a no-op, and the ANSI sequence above is the whole
        // story — the headless configuration.
        vela_kernel_console::clear();
        SyscallResult::ok(0)
    }

    /// `GetHostname` — read the machine's hostname into a user buffer.
    ///
    /// Arguments: `buf` (a user pointer), `len` (the buffer size). Returns
    /// the number of bytes written, excluding the NUL terminator, or `RANGE`
    /// when the buffer is empty.
    pub fn get_boot_choice(_args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        SyscallResult::ok(crate::boot_choice() as u64)
    }

    pub fn get_hostname(args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        let buf = args.get(0);
        let len = args.get(1) as usize;
        if len == 0 {
            return SyscallResult::err(errors::RANGE);
        }
        let name = crate::hostname();
        let bytes = name.as_bytes();
        let count = bytes.len().min(len - 1);
        // SAFETY: the kernel validates user pointers before writing; the
        // copy helper checks the range and rejects bad pointers.
        match unsafe { crate::copy::copy_to_user(buf, bytes.as_ptr(), count) } {
            Ok(_) => SyscallResult::ok(count as u64),
            Err(_) => SyscallResult::err(errors::BAD_POINTER),
        }
    }

    /// `FindTask` — find a task by name.
    ///
    /// Arguments: `name_ptr` (a user pointer to a NUL-terminated name, or
    /// zero for "the calling task"). Returns the task id, or zero when no
    /// task carries that name.
    ///
    /// # Why zero and not an error
    ///
    /// AmigaOS's `FindTask` returned `NULL` for a name nobody carries, and a
    /// program probing for a task expects exactly that answer. An error code
    /// would be a different answer to the same question, and a program
    /// ported from AmigaOS would have to learn a new convention to use it.
    pub fn find_task(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        if name_ptr == 0 {
            // The AROS spelling: a null name means "the current task", and
            // it is the quick path — no table walk at all.
            return SyscallResult::ok(ctx.task.0 as u64);
        }
        let name = match copy_string_alloc(name_ptr) {
            Ok(name) => name,
            Err(e) => return SyscallResult::err(e),
        };
        match ctx.exec.find_task(Some(&name)) {
            Some(task) => SyscallResult::ok(task.0 as u64),
            None => SyscallResult::ok(0),
        }
    }

    /// `SetTaskName` — name the calling task.
    ///
    /// Arguments: `name_ptr`. Returns zero.
    ///
    /// A task names itself, exactly as it sets `tc_Node.ln_Name` on AmigaOS,
    /// and `FindTask` is what reads the name back.
    pub fn set_task_name(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        let name = match copy_string_alloc(name_ptr) {
            Ok(name) => name,
            Err(e) => return SyscallResult::err(e),
        };
        match ctx.exec.set_task_name(ctx.task, &name) {
            Ok(()) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `InitSemaphore` — create a semaphore.
    ///
    /// Arguments: `priority`. Returns the semaphore's id.
    ///
    /// The classic call initialised a structure in the caller's memory; the
    /// table is kernel-owned here, so the call creates the semaphore and
    /// hands back its id. The `priority` argument orders the wait queue on
    /// AmigaOS and is kept for the ABI's shape.
    pub fn create_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let priority = args.get(0) as u8 as i8;
        let semaphore = ctx.exec.init_semaphore(priority);
        SyscallResult::ok(semaphore.0 as u64)
    }

    /// `ObtainSemaphore` — take a semaphore, waiting for it if necessary.
    ///
    /// Arguments: `semaphore`, `shared` (non-zero asks for a shared grant).
    /// Returns `1` when the lock is held, `0` when the caller was queued.
    ///
    /// # The retry protocol
    ///
    /// A queued caller is woken with `SIGF_SINGLE` and calls again. The
    /// second call is a *confirmation*, not a second acquire: the hand-off
    /// already happened inside `ReleaseSemaphore`, so re-acquiring would
    /// nest one level deeper than the first request asked for. The handler
    /// therefore checks for a pending grant first and answers from it.
    ///
    /// A queued caller that is woken by something else gets `0` again and
    /// re-queues, which is the same answer `WaitPort` gives when it returns
    /// `NULL`.
    pub fn obtain_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let semaphore = vela_kernel_exec::semaphore::SemaphoreId(args.get(0) as u32);
        let shared = args.get(1) != 0;
        // A pending grant is the retry path. `confirm_grant` consumes the
        // wait, so a stale `SIGF_SINGLE` cannot be mistaken for a grant
        // twice.
        match ctx.exec.confirm_grant(ctx.task, semaphore) {
            Ok(true) => return SyscallResult::ok(1),
            Ok(false) => {}
            Err(e) => return SyscallResult::err(e),
        }
        match ctx.exec.obtain_semaphore(ctx.task, semaphore, shared) {
            Ok(vela_kernel_exec::semaphore::Obtain::Obtained) => SyscallResult::ok(1),
            Ok(vela_kernel_exec::semaphore::Obtain::Queued) => {
                // The caller must sleep until it is granted. The wait is
                // published *before* the return, so a release that happens
                // immediately after cannot find no waiter and strand the
                // grant.
                if let Some(signals) = ctx.exec.task_signals(ctx.task) {
                    signals.begin_wait(
                        vela_kernel_exec::semaphore::SIGF_SINGLE,
                        vela_kernel_exec::task::BlockedOn::Semaphore(semaphore.0),
                    );
                }
                SyscallResult::ok(0)
            }
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `ConfirmGrant` — confirm a semaphore grant after being woken.
    ///
    /// Arguments: `semaphore`. Returns `1` when the hand-off named the
    /// caller, `0` when the wake was not a grant.
    pub fn confirm_grant(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let semaphore = vela_kernel_exec::semaphore::SemaphoreId(args.get(0) as u32);
        match ctx.exec.confirm_grant(ctx.task, semaphore) {
            Ok(granted) => SyscallResult::ok(granted as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `AttemptSemaphore` — take a semaphore without waiting.
    ///
    /// Arguments: `semaphore`, `shared`. Returns `1` when the lock was taken
    /// and `0` when it was held — a failed attempt is an answer, not an
    /// error, which is exactly what AmigaOS's `AttemptSemaphore` reported.
    pub fn attempt_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let semaphore = vela_kernel_exec::semaphore::SemaphoreId(args.get(0) as u32);
        let shared = args.get(1) != 0;
        match ctx.exec.attempt_semaphore(ctx.task, semaphore, shared) {
            Ok(taken) => SyscallResult::ok(taken as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `ReleaseSemaphore` — give up one nesting level of a held semaphore.
    ///
    /// Arguments: `semaphore`. Returns zero.
    ///
    /// When the nest count reaches zero the lock is granted onward and the
    /// grantees are woken with `SIGF_SINGLE`.
    pub fn release_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let semaphore = vela_kernel_exec::semaphore::SemaphoreId(args.get(0) as u32);
        match ctx.exec.release_semaphore(ctx.task, semaphore) {
            Ok(()) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `DeleteSemaphore` — delete a semaphore.
    ///
    /// Arguments: `semaphore`. Returns zero.
    ///
    /// The waiters are woken so their next attempt reports `NO_OBJECT`
    /// rather than sleeping on a lock that will never be granted.
    pub fn delete_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let semaphore = vela_kernel_exec::semaphore::SemaphoreId(args.get(0) as u32);
        match ctx.exec.delete_semaphore(semaphore) {
            Ok(()) => SyscallResult::ok(0),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `AddSemaphore` — create a semaphore and list it under a name.
    ///
    /// Arguments: `name_ptr` (a user pointer to a NUL-terminated name),
    /// `priority`. Returns the semaphore's id.
    ///
    /// This is AROS's `AddSemaphore`: the semaphore joins the system's
    /// public list, where `FindSemaphore` finds it by name. A name already
    /// in use is `NAME_TAKEN` — the public list holds one semaphore per
    /// name, and two semaphores answering to one name would make the
    /// lookup a coin toss.
    pub fn add_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        let priority = args.get(1) as u8 as i8;
        let name = match copy_string_alloc(name_ptr) {
            Ok(name) => name,
            Err(e) => return SyscallResult::err(e),
        };
        match ctx.exec.add_semaphore(&name, priority) {
            Ok(semaphore) => SyscallResult::ok(semaphore.0 as u64),
            Err(e) => SyscallResult::err(e),
        }
    }

    /// `FindSemaphore` — find a semaphore by name.
    ///
    /// Arguments: `name_ptr`. Returns the semaphore's id, or zero when no
    /// semaphore carries that name.
    pub fn find_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        let name = match copy_string_alloc(name_ptr) {
            Ok(name) => name,
            Err(e) => return SyscallResult::err(e),
        };
        match ctx.exec.find_semaphore(&name) {
            Some(semaphore) => SyscallResult::ok(semaphore.0 as u64),
            None => SyscallResult::ok(0),
        }
    }

    /// `RemSemaphore` — take a semaphore out of the public list.
    ///
    /// Arguments: `name_ptr`. Returns the semaphore's id, or zero when no
    /// semaphore carries that name.
    ///
    /// The semaphore itself lives on: it is still usable by id, and is no
    /// longer findable by name — which is what AROS's `RemSemaphore` does.
    pub fn remove_semaphore(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let name_ptr = args.get(0);
        let name = match copy_string_alloc(name_ptr) {
            Ok(name) => name,
            Err(e) => return SyscallResult::err(e),
        };
        match ctx.exec.remove_semaphore(&name) {
            Some(semaphore) => SyscallResult::ok(semaphore.0 as u64),
            None => SyscallResult::ok(0),
        }
    }

    /// `Alert` — raise an alert.
    ///
    /// Arguments: `alert` (an `AN_*`/`AG_*` number). Returns zero.
    ///
    /// # What an alert does here
    ///
    /// On AmigaOS an alert is a dialog on the screen, and a dead alert halts
    /// the machine. There is no screen here yet, so the alert is written to
    /// the console — where the person at the keyboard, or the boot log,
    /// will see it — with its decoded name. A dead alert also ends the
    /// calling task, because that is what `AT_DeadEnd` means: the task
    /// cannot continue.
    pub fn alert(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
        let alert = args.get(0) as u32;
        let name = vela_abi_native64::alerts::name(alert);
        let text = match name {
            Some(name) => alloc::format!(
                "Alert: {} (0x{:08x}, subsystem 0x{:02x}, class 0x{:02x}, code {})\n",
                name,
                alert,
                vela_abi_native64::alerts::subsystem(alert),
                vela_abi_native64::alerts::class(alert),
                vela_abi_native64::alerts::code(alert),
            ),
            None => alloc::format!(
                "Alert: 0x{:08x} (subsystem 0x{:02x}, class 0x{:02x}, code {})\n",
                alert,
                vela_abi_native64::alerts::subsystem(alert),
                vela_abi_native64::alerts::class(alert),
                vela_abi_native64::alerts::code(alert),
            ),
        };
        crate::print_console(&text);
        if vela_abi_native64::alerts::is_dead(alert) {
            // A dead alert ends the task. The messages it owned are dropped
            // and returned, so a reply in flight toward it has nowhere to go
            // and says so.
            ctx.exec.exit_task(ctx.task);
        }
        SyscallResult::ok(0)
    }
}

/// The built-in dispatch table, with every Exec syscall installed.
pub fn builtin_table() -> DispatchTable {
    let table = DispatchTable::new();
    table.install(numbers::CREATE_MSG_PORT, exec_handlers::create_msg_port);
    table.install(numbers::DELETE_MSG_PORT, exec_handlers::delete_msg_port);
    table.install(numbers::PUT_MSG, exec_handlers::put_msg);
    table.install(numbers::GET_MSG, exec_handlers::get_msg);
    table.install(numbers::REPLY_MSG, exec_handlers::reply_msg);
    table.install(numbers::WAIT_PORT, exec_handlers::wait_port);
    table.install(numbers::WAIT, exec_handlers::wait);
    table.install(numbers::SET_SIGNAL, exec_handlers::set_signal);
    table.install(numbers::SET_EXCEPT, exec_handlers::set_except);
    table.install(numbers::SIGNAL, exec_handlers::signal);
    table.install(numbers::OPEN_LIBRARY, exec_handlers::open_library);
    table.install(numbers::CLOSE_LIBRARY, exec_handlers::close_library);
    table.install(numbers::FORBID, exec_handlers::forbid);
    table.install(numbers::PERMIT, exec_handlers::permit);
    table.install(numbers::YIELD, exec_handlers::yield_now);
    table.install(numbers::GET_TASK_ID, exec_handlers::get_task_id);
    table.install(numbers::PUTCHAR, exec_handlers::put_char);
    table.install(numbers::POWER_OFF, exec_handlers::power_off);
    table.install(numbers::GETCHAR, exec_handlers::get_char);
    table.install(numbers::MAP_PAGE, exec_handlers::map_page);
    table.install(numbers::BRK, exec_handlers::brk);
    table.install(numbers::ADD_PORT, exec_handlers::add_port);
    // Exec memory management syscalls (numbers 22-30). `AllocMem` and
    // `AllocVec` share one implementation; `FreeVec` and `FreeMem` likewise.
    table.install(numbers::ALLOC_MEM, exec_handlers::alloc_mem);
    table.install(numbers::FREE_MEM, exec_handlers::free_mem);
    table.install(numbers::ALLOC_VEC, exec_handlers::alloc_vec);
    table.install(numbers::FREE_VEC, exec_handlers::free_vec);
    table.install(numbers::AVAIL_MEM, exec_handlers::avail_mem);
    table.install(numbers::CREATE_POOL, exec_handlers::create_pool);
    table.install(numbers::ALLOC_POOLED, exec_handlers::alloc_pooled);
    table.install(numbers::FREE_POOLED, exec_handlers::free_pooled);
    table.install(numbers::DELETE_POOL, exec_handlers::delete_pool);
    table.install(numbers::CLEAR_SCREEN, exec_handlers::clear_screen);
    table.install(numbers::GET_HOSTNAME, exec_handlers::get_hostname);
    table.install(numbers::GET_BOOT_CHOICE, exec_handlers::get_boot_choice);
    table.install(numbers::FIND_TASK, exec_handlers::find_task);
    table.install(numbers::SET_TASK_NAME, exec_handlers::set_task_name);
    table.install(numbers::CREATE_SEMAPHORE, exec_handlers::create_semaphore);
    table.install(numbers::OBTAIN_SEMAPHORE, exec_handlers::obtain_semaphore);
    table.install(numbers::CONFIRM_GRANT, exec_handlers::confirm_grant);
    table.install(numbers::ATTEMPT_SEMAPHORE, exec_handlers::attempt_semaphore);
    table.install(numbers::RELEASE_SEMAPHORE, exec_handlers::release_semaphore);
    table.install(numbers::DELETE_SEMAPHORE, exec_handlers::delete_semaphore);
    table.install(numbers::ALERT, exec_handlers::alert);
    table.install(numbers::ADD_SEMAPHORE, exec_handlers::add_semaphore);
    table.install(numbers::FIND_SEMAPHORE, exec_handlers::find_semaphore);
    table.install(numbers::REMOVE_SEMAPHORE, exec_handlers::remove_semaphore);

    // The `utility.library` group:

    // The `utility.library` group: tag lists and string helpers. They take
    // user pointers rather than handles, so they live in their own module —
    // the validation they need is the copy helpers' range check and probe,
    // not a lookup in a service's table.
    table.install(numbers::UTILITY_FIND_TAG, crate::utility_handlers::find_tag);
    table.install(numbers::UTILITY_STRNCPY, crate::utility_handlers::strncpy);
    table.install(numbers::UTILITY_STRCMP, crate::utility_handlers::strcmp);

    // The `dos.library` group. The calls that need a user buffer or a path
    // are the adapters in [`crate::dos_handlers`]; they exist as a separate
    // module because the argument shapes are different from Exec's and the
    // validation is more involved.
    table.install(numbers::DOS_OPEN, crate::dos_handlers::open);
    table.install(numbers::DOS_CLOSE, crate::dos_handlers::close);
    table.install(numbers::DOS_READ, crate::dos_handlers::read);
    table.install(numbers::DOS_WRITE, crate::dos_handlers::write);
    table.install(numbers::DOS_SEEK, crate::dos_handlers::seek);
    table.install(numbers::DOS_READ_DIR, crate::dos_handlers::read_dir);
    table.install(numbers::DOS_DELETE, crate::dos_handlers::delete);
    table.install(numbers::DOS_MKDIR, crate::dos_handlers::mkdir);
    // The calls with no slot in the base range are installed at the numbers
    // that continue the group, because the group reserves 64 slots and only
    // sixteen are used so far.
    table.install(numbers::DOS_EXAMINE, crate::dos_handlers::examine);
    table.install(numbers::DOS_CURRENT_DIR, crate::dos_handlers::current_dir);
    table.install(numbers::DOS_EXAMINE_DIR, crate::dos_handlers::examine_dir);
    table.install(
        numbers::DOS_SET_CURRENT_DIR,
        crate::dos_handlers::set_current_dir,
    );
    table.install(numbers::DOS_RENAME, crate::dos_handlers::rename);
    table.install(numbers::DOS_ADD_ASSIGN, crate::dos_handlers::add_assign);
    table.install(
        numbers::DOS_REMOVE_ASSIGN,
        crate::dos_handlers::remove_assign,
    );
    table.install(numbers::DOS_LIST_ASSIGNS, crate::dos_handlers::list_assigns);

    // The `net.library` group: BSD sockets. The handlers live in
    // [`crate::socket_handlers`] and reach the global socket service
    // ([`vela_kernel_socket::service`]), which kernel/entry installs during
    // boot.
    table.install(numbers::NET_SOCKET, crate::socket_handlers::socket);
    table.install(numbers::NET_BIND, crate::socket_handlers::bind);
    table.install(numbers::NET_CONNECT, crate::socket_handlers::connect);
    table.install(numbers::NET_LISTEN, crate::socket_handlers::listen);
    table.install(numbers::NET_ACCEPT, crate::socket_handlers::accept);
    table.install(numbers::NET_SENDTO, crate::socket_handlers::sendto);
    table.install(numbers::NET_RECVFROM, crate::socket_handlers::recvfrom);
    table.install(numbers::NET_CLOSE, crate::socket_handlers::close);
    table.install(numbers::NET_SETSOCKOPT, crate::socket_handlers::setsockopt);

    // The `graphics.library` group: pens and raster drawing. Every drawing
    // call names a *window* rather than a rast port, because a program does
    // not get to name the framebuffer — only the window manager knows where
    // the pixels are.
    table.install(
        numbers::GRAPHICS_ALLOC_PEN,
        crate::graphics_handlers::alloc_pen,
    );
    table.install(
        numbers::GRAPHICS_FREE_PEN,
        crate::graphics_handlers::free_pen,
    );
    table.install(numbers::GRAPHICS_SET_PEN, crate::graphics_handlers::set_pen);
    table.install(numbers::GRAPHICS_RECT, crate::graphics_handlers::rect);
    table.install(
        numbers::GRAPHICS_FILL_RECT,
        crate::graphics_handlers::fill_rect,
    );
    table.install(numbers::GRAPHICS_TEXT, crate::graphics_handlers::text);
    table.install(numbers::GRAPHICS_LINE, crate::graphics_handlers::line);
    table.install(
        numbers::GRAPHICS_SET_CLIP,
        crate::graphics_handlers::set_clip,
    );
    table.install(
        numbers::GRAPHICS_PEN_COLOR,
        crate::graphics_handlers::set_pen,
    );
    table.install(
        numbers::GRAPHICS_WRITE_PIXEL,
        crate::graphics_handlers::write_pixel,
    );
    table.install(numbers::GRAPHICS_MOVE, crate::graphics_handlers::move_to);
    table.install(
        numbers::GRAPHICS_SET_PALETTE,
        crate::graphics_handlers::set_palette,
    );
    table.install(
        numbers::GRAPHICS_RASTER_INFO,
        crate::graphics_handlers::raster_info,
    );

    // The `layers.library` group. Compositing is installed here rather than
    // being implicit: a program asking for a refresh is a normal thing to do,
    // and it is the only path by which the window manager's pixels reach the
    // display.
    table.install(numbers::LAYERS_CREATE, crate::layers_handlers::create);
    table.install(numbers::LAYERS_DESTROY, crate::layers_handlers::destroy);
    table.install(numbers::LAYERS_SET_CLIP, crate::layers_handlers::set_clip);
    table.install(numbers::LAYERS_FRONT, crate::layers_handlers::front);
    table.install(numbers::LAYERS_BACK, crate::layers_handlers::back);
    table.install(
        numbers::LAYERS_SET_VISIBLE,
        crate::layers_handlers::set_visible,
    );
    table.install(numbers::LAYERS_MOVE, crate::layers_handlers::move_layer);
    table.install(numbers::LAYERS_COMPOSITE, crate::layers_handlers::composite);
    table.install(numbers::LAYERS_ORDER, crate::layers_handlers::order);
    table.install(numbers::LAYERS_TOPMOST, crate::layers_handlers::topmost);

    // The `intuition.library` group: screens, windows, gadgets, and the IDCMP
    // events that arrive as Exec messages on a window's port.
    table.install(
        numbers::INTUITION_OPEN_SCREEN,
        crate::intuition_handlers::open_screen,
    );
    table.install(
        numbers::INTUITION_CLOSE_SCREEN,
        crate::intuition_handlers::close_screen,
    );
    table.install(
        numbers::INTUITION_OPEN_WINDOW,
        crate::intuition_handlers::open_window,
    );
    table.install(
        numbers::INTUITION_CLOSE_WINDOW,
        crate::intuition_handlers::close_window,
    );
    table.install(
        numbers::INTUITION_SET_TITLE,
        crate::intuition_handlers::set_title,
    );
    table.install(
        numbers::INTUITION_REFRESH,
        crate::intuition_handlers::refresh_all,
    );
    table.install(
        numbers::INTUITION_SET_ATTRS,
        crate::intuition_handlers::set_attrs,
    );
    table.install(
        numbers::INTUITION_ADD_GADGET,
        crate::intuition_handlers::add_gadget,
    );
    table.install(
        numbers::INTUITION_REMOVE_GADGET,
        crate::intuition_handlers::remove_gadget,
    );
    table.install(
        numbers::INTUITION_SET_BOUNDS,
        crate::intuition_handlers::set_bounds,
    );
    table.install(numbers::INTUITION_FRONT, crate::intuition_handlers::front);
    table.install(numbers::INTUITION_BACK, crate::intuition_handlers::back);
    table.install(
        numbers::INTUITION_REPAINT,
        crate::intuition_handlers::repaint,
    );

    // The `icon.library` group. Every call here names an icon by an opaque
    // handle or a name in a user buffer; the adapters copy the name into
    // kernel-owned storage before it is used.
    table.install(
        numbers::ICON_GET_DISK_OBJECT,
        crate::icon_handlers::get_disk_object,
    );
    table.install(
        numbers::ICON_GET_DISK_OBJECT_NEW,
        crate::icon_handlers::get_disk_object_new,
    );
    table.install(
        numbers::ICON_PUT_DISK_OBJECT,
        crate::icon_handlers::put_disk_object,
    );
    table.install(
        numbers::ICON_FREE_DISK_OBJECT,
        crate::icon_handlers::free_disk_object,
    );
    table.install(
        numbers::ICON_DUP_DISK_OBJECT,
        crate::icon_handlers::dup_disk_object,
    );
    table.install(
        numbers::ICON_DELETE_DISK_OBJECT,
        crate::icon_handlers::delete_disk_object,
    );
    table.install(
        numbers::ICON_GET_DEF_DISK_OBJECT,
        crate::icon_handlers::get_def_disk_object,
    );
    table.install(
        numbers::ICON_PUT_DEF_DISK_OBJECT,
        crate::icon_handlers::put_def_disk_object,
    );
    table.install(
        numbers::ICON_NEW_DISK_OBJECT,
        crate::icon_handlers::new_disk_object,
    );
    table.install(
        numbers::ICON_FIND_TOOL_TYPE,
        crate::icon_handlers::find_tool_type,
    );
    table.install(
        numbers::ICON_MATCH_TOOL_VALUE,
        crate::icon_handlers::match_tool_value,
    );
    table.install(
        numbers::ICON_GET_ICON_TAG_LIST,
        crate::icon_handlers::get_icon_tag_list,
    );
    table.install(
        numbers::ICON_PUT_ICON_TAG_LIST,
        crate::icon_handlers::put_icon_tag_list,
    );
    table.install(
        numbers::ICON_BUMP_REVISION,
        crate::icon_handlers::bumps_revision,
    );
    table.install(numbers::ICON_LAYOUT_ICON, crate::icon_handlers::layout_icon);
    table.install(
        numbers::ICON_GET_ICON_RECTANGLE,
        crate::icon_handlers::get_icon_rectangle,
    );
    table.install(
        numbers::ICON_ICON_CONTROL,
        crate::icon_handlers::icon_control,
    );
    table.install(
        numbers::ICON_DRAW_ICON_STATE,
        crate::icon_handlers::draw_icon_state,
    );
    table.install(
        numbers::ICON_ADD_FREE_LIST,
        crate::icon_handlers::add_free_list,
    );
    table.install(
        numbers::ICON_FREE_FREE_LIST,
        crate::icon_handlers::free_free_list,
    );
    table.install(
        numbers::ICON_CHANGE_TO_SELECTED_ICON_COLOR,
        crate::icon_handlers::change_to_selected_icon_color,
    );

    // The `workbench.library` group. The Workbench is one service with a
    // registry of application icons, menu items, windows and drop zones;
    // the handles it hands out are opaque to the caller.
    table.install(
        numbers::WORKBENCH_ADD_APP_ICON,
        crate::workbench_handlers::add_app_icon,
    );
    table.install(
        numbers::WORKBENCH_ADD_APP_MENU_ITEM,
        crate::workbench_handlers::add_app_menu_item,
    );
    table.install(
        numbers::WORKBENCH_ADD_APP_WINDOW,
        crate::workbench_handlers::add_app_window,
    );
    table.install(
        numbers::WORKBENCH_ADD_APP_WINDOW_DROP_ZONE,
        crate::workbench_handlers::add_app_window_drop_zone,
    );
    table.install(
        numbers::WORKBENCH_CLOSE_WORKBENCH_OBJECT,
        crate::workbench_handlers::close_workbench_object,
    );
    table.install(
        numbers::WORKBENCH_GET_NEXT_APP_ICON,
        crate::workbench_handlers::get_next_app_icon,
    );
    table.install(
        numbers::WORKBENCH_MAKE_WORKBENCH_OBJECT_VISIBLE,
        crate::workbench_handlers::make_workbench_object_visible,
    );
    table.install(
        numbers::WORKBENCH_NOTIFY_WORKBENCH,
        crate::workbench_handlers::notify_workbench,
    );
    table.install(
        numbers::WORKBENCH_OPEN_WORKBENCH_OBJECT,
        crate::workbench_handlers::open_workbench_object,
    );
    table.install(
        numbers::WORKBENCH_QUOTE_WORKBENCH,
        crate::workbench_handlers::quote_workbench,
    );
    table.install(
        numbers::WORKBENCH_REGISTER_WORKBENCH,
        crate::workbench_handlers::register_workbench,
    );
    table.install(
        numbers::WORKBENCH_REMOVE_APP_ICON,
        crate::workbench_handlers::remove_app_icon,
    );
    table.install(
        numbers::WORKBENCH_REMOVE_APP_MENU_ITEM,
        crate::workbench_handlers::remove_app_menu_item,
    );
    table.install(
        numbers::WORKBENCH_REMOVE_APP_WINDOW,
        crate::workbench_handlers::remove_app_window,
    );
    table.install(
        numbers::WORKBENCH_REMOVE_APP_WINDOW_DROP_ZONE,
        crate::workbench_handlers::remove_app_window_drop_zone,
    );
    table.install(
        numbers::WORKBENCH_SEND_APP_WINDOW_MESSAGE,
        crate::workbench_handlers::send_app_window_message,
    );
    table.install(
        numbers::WORKBENCH_START_WORKBENCH,
        crate::workbench_handlers::start_workbench,
    );
    table.install(
        numbers::WORKBENCH_UNREGISTER_WORKBENCH,
        crate::workbench_handlers::unregister_workbench,
    );
    table.install(
        numbers::WORKBENCH_UPDATE_WORKBENCH_OBJECT,
        crate::workbench_handlers::update_workbench_object,
    );
    table.install(
        numbers::WORKBENCH_WB_CONFIG,
        crate::workbench_handlers::wb_config,
    );
    table.install(
        numbers::WORKBENCH_WB_INFO,
        crate::workbench_handlers::wb_info,
    );
    table.install(
        numbers::WORKBENCH_WORKBENCH_CONTROL,
        crate::workbench_handlers::workbench_control,
    );
    table
}

/// The kernel's live dispatch table and Exec service.
///
/// The gate reaches these through [`handle_syscall`]. They are process-wide
/// because the kernel is: there is one Exec service, one registry of
/// libraries, and one dispatch table.
static KERNEL: Mutex<Option<KernelState>> = Mutex::new(None);

/// The kernel-wide syscall state.
///
/// The services are held by reference rather than by value: the kernel owns
/// them in leaked allocations, and copying them would copy the mutexes and
/// the tables inside them — which is not just wasteful but wrong, because
/// the copy would be a *different* set of ports and files from the one the
/// rest of the kernel uses.
struct KernelState {
    table: &'static DispatchTable,
    exec: &'static Exec,
    dos: &'static Dos,
    graphics: &'static Graphics,
    intuition: &'static Intuition,
    icon: &'static IconService,
    workbench: &'static Workbench,
}

/// Install the kernel's dispatch table and services.
///
/// The services must outlive the kernel, which is why they are taken by
/// reference: the caller leaks them into `'static` allocations.
///
/// # Safety
///
/// Must be called once, during boot, before any user code runs. The
/// references must be valid for the rest of the kernel's life.
pub unsafe fn install(
    exec: &'static Exec,
    dos: &'static Dos,
    graphics: &'static Graphics,
    intuition: &'static Intuition,
    icon: &'static IconService,
    workbench: &'static Workbench,
    table: &'static DispatchTable,
) {
    let mut kernel = KERNEL.lock();
    *kernel = Some(KernelState {
        table,
        exec,
        dos,
        graphics,
        intuition,
        icon,
        workbench,
    });
}

/// True once [`install`] has run.
pub fn is_installed() -> bool {
    KERNEL.lock().is_some()
}

/// Dispatch a syscall through the kernel's table.
///
/// This is the function the gate calls. It returns
/// [`errors::NOT_IMPLEMENTED`] if the kernel has not been installed, so a
/// stray syscall before boot completes is a clean error rather than a null
/// dereference.
pub fn handle_syscall(number: u64, args: SyscallArgs) -> SyscallResult {
    // The lock is taken for the whole dispatch, which serialises syscalls.
    //
    // That serialisation is correct for v0.1.0: the kernel is single-CPU, so
    // a syscall cannot be preempted by another syscall anyway. It becomes
    // wrong the moment SMP arrives, and at that point the table and the Exec
    // service each need their own finer-grained locking.
    let kernel = KERNEL.lock();
    let Some(state) = kernel.as_ref() else {
        return SyscallResult::err(errors::NOT_IMPLEMENTED);
    };
    // The calling task is the one the kernel is currently running on.
    let task = state.exec.current_task();
    let context = SyscallContext {
        task,
        exec: state.exec,
        dos: state.dos,
        graphics: state.graphics,
        intuition: state.intuition,
        icon: state.icon,
        workbench: state.workbench,
    };
    state.table.dispatch(number, args, &context)
}

/// The raw `syscall` entry point called by the architecture's gate stub.
///
/// The gate stub (in `vela-arch-x86_64::syscall::gate`) has already run
/// `swapgs`, switched to the kernel stack, and saved the user return state.
/// This function reads the ABI registers, dispatches through [`handle_syscall`],
/// and writes the result back into `RAX`/`RDX`; the stub then restores `RCX`/
/// `R11` and executes `sysretq`.
///
/// # Safety
///
/// Only the architecture gate stub may call this, and only from the gate
/// entry stub. The register reads assume the registers hold the values the
/// `syscall` instruction left them with.
pub unsafe extern "C" fn syscall_dispatch() {
    // The syscall number is in `rax`, and the arguments are in `rdi`, `rsi`,
    // `rdx`, `r10`, `r8` and `r9` — the System V argument registers except
    // for `r10` in place of `rcx`, because `rcx` holds the return address.
    let number: u64;
    let a0: u64;
    let a1: u64;
    let a2: u64;
    let a3: u64;
    let a4: u64;
    let a5: u64;
    // SAFETY: the `syscall` instruction leaves these registers as the ABI
    // specifies, and reading them is free of side effects. Each output is
    // bound to the exact register it reads, and the `nop` writes nothing: a
    // free `out(reg)` could be allocated *to a not-yet-read source register*,
    // and the move into it would destroy an argument before it is read.
    unsafe {
        core::arch::asm!(
            "nop",
            out("rax") number,
            out("rdi") a0,
            out("rsi") a1,
            out("rdx") a2,
            out("r10") a3,
            out("r8") a4,
            out("r9") a5,
            options(nostack, preserves_flags),
        );
    }

    let args = crate::numbers::SyscallArgs::new(a0, a1, a2, a3, a4, a5);
    let result = handle_syscall(number, args);

    // The result goes back in `rax` and `rdx`, which the stub's `sysretq`
    // delivers to the caller.
    //
    // The registers are named, not left to the allocator: a free `in(reg)`
    // input could be placed in `rax` or `rdx` itself, and the hardcoded move
    // into the *other* register would then overwrite it — a release-only
    // corruption that read back as `error == low 32 bits of value`. The `nop`
    // writes nothing, so the bound values survive to the `ret` the stub reads
    // from.
    //
    // SAFETY: the registers are bound explicitly and the `nop` has no effects.
    unsafe {
        core::arch::asm!(
            "nop",
            in("rax") result.value,
            in("rdx") result.error as u64,
            options(nostack, preserves_flags),
        );
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numbers::SyscallArgs;
    use alloc::boxed::Box;

    fn context() -> &'static SyscallContext<'static> {
        // The services are leaked so the context can borrow them for the
        // lifetime of the test.
        let exec: &'static Exec = Box::leak(Box::new(Exec::new()));
        let dos: &'static Dos = Box::leak(Box::new(Dos::new()));
        let graphics: &'static Graphics = Box::leak(Box::new(Graphics::new()));
        let intuition: &'static Intuition = Box::leak(Box::new(Intuition::new(640, 480)));
        let icon: &'static IconService = Box::leak(Box::new(IconService::new()));
        let workbench: &'static Workbench = Box::leak(Box::new(Workbench::new()));
        let task = exec.create_task();
        // The leaked services outlive every test, so the borrow is sound.
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

    #[test]
    fn an_empty_table_reports_not_implemented() {
        let table = DispatchTable::new();
        let ctx = context();
        let result = table.dispatch(0, SyscallArgs::default(), ctx);
        assert_eq!(result.error, errors::NOT_IMPLEMENTED);
    }

    #[test]
    fn an_unknown_syscall_number_reports_not_implemented() {
        let table = builtin_table();
        let ctx = context();
        // A number far beyond any defined syscall.
        let result = table.dispatch(9999, SyscallArgs::default(), ctx);
        assert_eq!(result.error, errors::NOT_IMPLEMENTED);
    }

    #[test]
    fn the_builtin_table_installs_every_exec_syscall() {
        let table = builtin_table();
        for number in 0..numbers::EXEC_COUNT {
            assert!(
                table.is_implemented(number),
                "exec syscall {number} has no handler",
            );
        }
        // The boot-policy syscall is outside the numbered library ranges.
        assert!(
            table.is_implemented(numbers::GET_BOOT_CHOICE),
            "boot-choice syscall has no handler",
        );
        // Every group that has a library installed is installed by the
        // built-in table, and the count says so: every number the kernel
        // defines has a handler, and no number it does not define has one.
        assert_eq!(
            table.implemented_count(),
            (numbers::EXEC_COUNT
                + numbers::DOS_COUNT
                + numbers::GRAPHICS_COUNT
                + numbers::LAYERS_COUNT
                + numbers::INTUITION_COUNT
                + numbers::UTILITY_COUNT
                + numbers::ICON_COUNT
                + numbers::WORKBENCH_COUNT
                + numbers::NET_COUNT
                + numbers::BOOT_CHOICE_COUNT) as usize
        );
    }

    #[test]
    fn the_builtin_table_installs_every_dos_syscall() {
        let table = builtin_table();
        for number in numbers::DOS_OPEN..numbers::DOS_OPEN + numbers::DOS_COUNT {
            assert!(
                table.is_implemented(number),
                "dos syscall {number} has no handler",
            );
        }
    }

    #[test]
    fn the_builtin_table_installs_every_graphics_syscall() {
        let table = builtin_table();
        for number in
            numbers::GRAPHICS_ALLOC_PEN..numbers::GRAPHICS_ALLOC_PEN + numbers::GRAPHICS_COUNT
        {
            assert!(
                table.is_implemented(number),
                "graphics syscall {number} has no handler",
            );
        }
    }

    #[test]
    fn the_builtin_table_installs_every_layers_syscall() {
        let table = builtin_table();
        for number in numbers::LAYERS_CREATE..numbers::LAYERS_CREATE + numbers::LAYERS_COUNT {
            assert!(
                table.is_implemented(number),
                "layers syscall {number} has no handler",
            );
        }
    }

    #[test]
    fn the_builtin_table_installs_every_intuition_syscall() {
        let table = builtin_table();
        for number in numbers::INTUITION_OPEN_SCREEN
            ..numbers::INTUITION_OPEN_SCREEN + numbers::INTUITION_COUNT
        {
            assert!(
                table.is_implemented(number),
                "intuition syscall {number} has no handler",
            );
        }
    }

    #[test]
    fn the_builtin_table_leaves_the_reserved_slots_unimplemented() {
        // "A stub is not compatible": a number the kernel reserves but has
        // not defined a call for must report that it does not exist, rather
        // than answering as though it did. The three utility calls are
        // implemented; the slot immediately after them is not.
        let table = builtin_table();
        assert!(table.is_implemented(numbers::UTILITY_STRCMP));
        assert!(!table.is_implemented(numbers::UTILITY_STRCMP + 1));
        let ctx = context();
        let result = table.dispatch(numbers::UTILITY_STRCMP + 1, SyscallArgs::default(), ctx);
        assert_eq!(result.error, errors::NOT_IMPLEMENTED);
    }

    #[test]
    fn the_utility_handlers_are_installed() {
        let table = builtin_table();
        for number in numbers::UTILITY_FIND_TAG..numbers::UTILITY_FIND_TAG + numbers::UTILITY_COUNT
        {
            assert!(
                table.is_implemented(number),
                "utility syscall {number} has no handler",
            );
        }
    }

    #[test]
    fn a_handler_can_be_installed_and_removed() {
        let table = DispatchTable::new();
        assert!(!table.is_implemented(500));
        table.install(500, exec_handlers::forbid);
        assert!(table.is_implemented(500));
        table.remove(500);
        assert!(!table.is_implemented(500));
    }

    #[test]
    fn installing_a_handler_twice_replaces_it() {
        // A library that is unloaded and reloaded must be able to replace
        // its handler.
        let table = DispatchTable::new();
        table.install(500, exec_handlers::forbid);
        table.install(500, exec_handlers::permit);
        assert_eq!(table.implemented_count(), 1);
    }

    #[test]
    fn get_task_id_returns_the_calling_task() {
        let table = builtin_table();
        let ctx = context();
        let result = table.dispatch(numbers::GET_TASK_ID, SyscallArgs::default(), ctx);
        assert!(result.is_ok());
        assert_eq!(result.value, ctx.task.0 as u64);
    }

    #[test]
    fn a_port_can_be_created_and_deleted_through_syscalls() {
        let table = builtin_table();
        let ctx = context();
        let created = table.dispatch(
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(1, 0, 0, 0, 0, 0),
            ctx,
        );
        assert!(created.is_ok());
        assert_ne!(created.value, 0, "a created port must have a handle");

        let deleted = table.dispatch(
            numbers::DELETE_MSG_PORT,
            SyscallArgs::new(created.value, 0, 0, 0, 0, 0),
            ctx,
        );
        assert!(deleted.is_ok());
    }

    #[test]
    fn a_bad_user_pointer_is_refused_not_crashed() {
        // The plan requires this case explicitly: "a `api-conformance` case
        // deliberately passes invalid pointers and asserts a clean error".
        let table = builtin_table();
        let ctx = context();
        let created = table.dispatch(
            numbers::CREATE_MSG_PORT,
            SyscallArgs::new(1, 0, 0, 0, 0, 0),
            ctx,
        );
        // A message pointer in the kernel half must be refused.
        let result = table.dispatch(
            numbers::PUT_MSG,
            SyscallArgs::new(created.value, 0xFFFF_8000_0000_0000, 0, 0, 0, 0),
            ctx,
        );
        assert_eq!(result.error, errors::BAD_POINTER);
    }

    #[test]
    fn opening_a_library_with_a_bad_name_pointer_is_refused() {
        let table = builtin_table();
        let ctx = context();
        let result = table.dispatch(
            numbers::OPEN_LIBRARY,
            SyscallArgs::new(0xFFFF_8000_0000_0000, 40, 0, 0, 0, 0),
            ctx,
        );
        assert_eq!(result.error, errors::BAD_POINTER);
    }

    #[test]
    fn forbid_and_permit_are_balanced() {
        let table = builtin_table();
        let ctx = context();
        let depth = table.dispatch(numbers::FORBID, SyscallArgs::default(), ctx);
        assert!(depth.is_ok());
        assert_eq!(depth.value, 1);
        let remaining = table.dispatch(numbers::PERMIT, SyscallArgs::default(), ctx);
        assert!(remaining.is_ok());
        assert_eq!(remaining.value, 0);
    }
}
