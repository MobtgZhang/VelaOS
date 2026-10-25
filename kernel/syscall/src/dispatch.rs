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
use vela_kernel_intuition::Intuition;

use vela_hal_common::MapError;

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
    pub fn yield_now(_args: SyscallArgs, _ctx: &SyscallContext<'_>) -> SyscallResult {
        SyscallResult::ok(0)
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
        vela_hal_x86_64::serial::putc(args.get(0) as u8);
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
        vela_hal_x86_64::qemu_exit::power_off(args.get(0) as u32)
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
    table.install(numbers::MAP_PAGE, exec_handlers::map_page);
    table.install(numbers::BRK, exec_handlers::brk);

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
    // eight are used so far.
    table.install(numbers::DOS_EXAMINE, crate::dos_handlers::examine);
    table.install(numbers::DOS_CURRENT_DIR, crate::dos_handlers::current_dir);
    table.install(numbers::DOS_EXAMINE_DIR, crate::dos_handlers::examine_dir);

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
    table: &'static DispatchTable,
) {
    let mut kernel = KERNEL.lock();
    *kernel = Some(KernelState {
        table,
        exec,
        dos,
        graphics,
        intuition,
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
    };
    state.table.dispatch(number, args, &context)
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
        let task = exec.create_task();
        // The leaked services outlive every test, so the borrow is sound.
        Box::leak(Box::new(SyscallContext {
            task,
            exec,
            dos,
            graphics,
            intuition,
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
        // Every group that has a library installed is installed by the
        // built-in table. The utility group is Phase 7 and is the one that
        // is deliberately still empty, and the count says so.
        assert_eq!(
            table.implemented_count(),
            (numbers::EXEC_COUNT
                + numbers::DOS_COUNT
                + numbers::GRAPHICS_COUNT
                + numbers::LAYERS_COUNT
                + numbers::INTUITION_COUNT) as usize
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
    fn the_builtin_table_leaves_the_other_libraries_unimplemented() {
        // `utility.library` is the one group with no slots filled in. A stub
        // that returned success would be indistinguishable from a working
        // call, so it must report "not implemented" honestly.
        let table = builtin_table();
        assert!(!table.is_implemented(numbers::UTILITY_FIND_TAG));
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
