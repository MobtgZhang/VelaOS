//! The `workbench.library` syscall handlers.
//!
//! # The boundary rule
//!
//! Workbench calls take one of two things: an opaque handle, or a user
//! pointer. A handle is looked up in the kernel's Workbench table; a pointer
//! is copied through [`crate::copy`] before the kernel uses it. Neither a
//! user pointer nor a kernel pointer ever crosses the boundary.
//!
//! # Notification through Exec
//!
//! A registered Workbench owns an Exec message port. `NotifyWorkbench` and
//! `SendAppWindowMessage` put messages on that port through
//! [`vela_kernel_exec::Exec::put_msg`], so the desktop's events are ordinary
//! Exec messages rather than a private desktop pipe. The payload carries the
//! action and the data word; a program that waits on the port sees them in
//! `mn_payload`.

use alloc::string::String;

use vela_abi_native64::Message;

use vela_kernel_exec::port::PortId;
use vela_kernel_icon::kind;
use vela_kernel_workbench::{
    AppIconHandle, AppMenuItemHandle, AppWindowHandle, DropZoneHandle, WorkbenchHandle,
};
use vela_renderer2d::Rect;

use crate::copy::{copy_string_alloc, copy_to_user};
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// Read a required user string, returning the error rather than a panic.
fn text(args: SyscallArgs, index: usize) -> Result<String, i32> {
    let ptr = args.get(index);
    if ptr == 0 {
        return Err(errors::BAD_POINTER);
    }
    copy_string_alloc(ptr)
}

/// Copy a kernel-owned text value into a user buffer.
///
/// Returns the number of bytes written, including the terminator.
fn write_text(dest: u64, len: usize, value: &str) -> Result<usize, i32> {
    let bytes = value.as_bytes();
    let needed = bytes.len() + 1;
    if dest == 0 {
        return Err(errors::BAD_POINTER);
    }
    if len < needed {
        return Err(errors::RANGE);
    }
    let mut buffer = bytes.to_vec();
    buffer.push(0);
    unsafe { copy_to_user(dest, buffer.as_ptr(), buffer.len()) }
}

/// Build the Workbench notification message.
fn notification(action: u32, data: u64) -> Message {
    let mut message = Message::empty();
    message.mn_payload[0] = action as u64;
    message.mn_payload[1] = data;
    message
}

/// `AddAppIcon` — register an application icon.
///
/// Arguments: `name_ptr`, `path_ptr`, `icon_type`, `x`, `y`.
pub fn add_app_icon(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name = match text(args, 0) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    let path = match text(args, 1) {
        Ok(path) => path,
        Err(code) => return SyscallResult::err(code),
    };
    let icon_type = args.get(2) as u32;
    let x = args.get(3) as i32;
    let y = args.get(4) as i32;
    if icon_type != kind::DISK
        && icon_type != kind::DRAWER
        && icon_type != kind::TOOL
        && icon_type != kind::APP
    {
        return SyscallResult::err(errors::INVALID_NAME);
    }
    match ctx.workbench.add_app_icon(&name, &path, icon_type, x, y) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `AddAppMenuItem` — register a Workbench menu item.
///
/// Arguments: `label_ptr`, `action`.
pub fn add_app_menu_item(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let label = match text(args, 0) {
        Ok(label) => label,
        Err(code) => return SyscallResult::err(code),
    };
    let action = args.get(1) as u32;
    match ctx.workbench.add_app_menu_item(&label, action) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `AddAppWindow` — register an application window.
///
/// Arguments: `window`. The owner is the calling task, so a program cannot
/// register another task's window as its own.
pub fn add_app_window(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = vela_kernel_intuition::WindowId(args.get(0) as u32);
    if window.is_none() {
        return SyscallResult::err(errors::NO_OBJECT);
    }
    if ctx.intuition.window(window).is_none() {
        return SyscallResult::err(errors::NO_OBJECT);
    }
    match ctx.workbench.add_app_window(window, ctx.task) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `AddAppWindowDropZone` — register a drop zone on a window.
///
/// Arguments: `window`, `x`, `y`, `width`, `height`.
pub fn add_app_window_drop_zone(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let window = vela_kernel_intuition::WindowId(args.get(0) as u32);
    let rect = Rect::new(
        args.get(1) as i32,
        args.get(2) as i32,
        args.get(3) as u32,
        args.get(4) as u32,
    );
    if ctx.intuition.window(window).is_none() {
        return SyscallResult::err(errors::NO_OBJECT);
    }
    match ctx.workbench.add_drop_zone(window, rect) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `CloseWorkbenchObject` — close a Workbench object.
pub fn close_workbench_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = WorkbenchHandle(args.get(0) as u32);
    match ctx.workbench.close_object(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `GetNextAppIcon` — the next registered icon after `cursor`.
///
/// Returns the handle, or zero when there is no next icon. A zero handle is
/// a normal end of iteration, not an error.
pub fn get_next_app_icon(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let cursor = args.get(0);
    match ctx.workbench.get_next_app_icon(cursor) {
        Some(handle) => SyscallResult::ok(handle.0 as u64),
        None => SyscallResult::ok(0),
    }
}

/// `MakeWorkbenchObjectVisible` — set whether an object is visible.
pub fn make_workbench_object_visible(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = WorkbenchHandle(args.get(0) as u32);
    let visible = args.get(1) as u32 != 0;
    match ctx.workbench.make_visible(handle, visible) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `NotifyWorkbench` — send a notification to the Workbench's Exec port.
///
/// Arguments: `action`, `data`. The message goes to the port the Workbench
/// registered. A Workbench with no port reports `NO_PORT`.
pub fn notify_workbench(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let Some(port) = ctx.workbench.notification_port() else {
        return SyscallResult::err(errors::NO_PORT);
    };
    let message = notification(args.get(0) as u32, args.get(1));
    match ctx.exec.put_msg(port, message) {
        Ok(_) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `OpenWorkbenchObject` — create and open a Workbench object.
///
/// Arguments: `name_ptr`, `path_ptr`, `icon_type`.
pub fn open_workbench_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name = match text(args, 0) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    let path = match text(args, 1) {
        Ok(path) => path,
        Err(code) => return SyscallResult::err(code),
    };
    let icon_type = args.get(2) as u32;
    if icon_type != kind::DISK
        && icon_type != kind::DRAWER
        && icon_type != kind::TOOL
        && icon_type != kind::APP
    {
        return SyscallResult::err(errors::INVALID_NAME);
    }
    match ctx.workbench.open_object(&name, &path, icon_type) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `QuoteWorkbench` — toggle path quoting and report the previous state.
pub fn quote_workbench(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    SyscallResult::ok(ctx.workbench.quote() as u64)
}

/// `RegisterWorkbench` — register the Workbench's owner and message port.
///
/// Arguments: `port`. The port must exist and belong to the calling task, so
/// a program cannot register a port it does not own.
pub fn register_workbench(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let port = PortId(args.get(0) as u32);
    if port.is_none() {
        return SyscallResult::err(errors::NO_PORT);
    }
    match ctx.exec.ports.port_owner(port) {
        Some(owner) if owner == ctx.task => {}
        Some(_) => return SyscallResult::err(errors::NOT_OWNER),
        None => return SyscallResult::err(errors::NO_OBJECT),
    }
    match ctx.workbench.register(ctx.task, port) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `RemoveAppIcon` — remove an application icon.
pub fn remove_app_icon(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = AppIconHandle(args.get(0) as u32);
    match ctx.workbench.remove_app_icon(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `RemoveAppMenuItem` — remove a menu item.
pub fn remove_app_menu_item(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = AppMenuItemHandle(args.get(0) as u32);
    match ctx.workbench.remove_app_menu_item(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `RemoveAppWindow` — remove an application window record.
pub fn remove_app_window(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = AppWindowHandle(args.get(0) as u32);
    match ctx.workbench.app_window(handle) {
        Some(record) if record.owner == ctx.task => {}
        Some(_) => return SyscallResult::err(errors::NOT_OWNER),
        None => return SyscallResult::err(errors::NO_OBJECT),
    }
    match ctx.workbench.remove_app_window(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `RemoveAppWindowDropZone` — remove a drop zone.
pub fn remove_app_window_drop_zone(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = DropZoneHandle(args.get(0) as u32);
    match ctx.workbench.remove_drop_zone(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `SendAppWindowMessage` — send a message to the window's Exec port.
///
/// Arguments: `app_window`, `action`, `data`. The caller must own the
/// registered window; the message goes to the window's `port`.
pub fn send_app_window_message(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = AppWindowHandle(args.get(0) as u32);
    let Some(record) = ctx.workbench.app_window(handle) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    if record.owner != ctx.task {
        return SyscallResult::err(errors::NOT_OWNER);
    }
    let Some(window) = ctx.intuition.window(record.window) else {
        return SyscallResult::err(errors::NO_OBJECT);
    };
    let message = notification(args.get(1) as u32, args.get(2));
    match ctx.exec.put_msg(window.port, message) {
        Ok(_) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `StartWorkbench` — mark the desktop as started.
pub fn start_workbench(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    match ctx.workbench.start() {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `UnregisterWorkbench` — drop the registration.
pub fn unregister_workbench(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    match ctx.workbench.unregister() {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `UpdateWorkbenchObject` — change an object's path and type.
///
/// Arguments: `handle`, `path_ptr`, `icon_type`.
pub fn update_workbench_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = WorkbenchHandle(args.get(0) as u32);
    let path = match text(args, 1) {
        Ok(path) => path,
        Err(code) => return SyscallResult::err(code),
    };
    let icon_type = args.get(2) as u32;
    if icon_type != kind::DISK
        && icon_type != kind::DRAWER
        && icon_type != kind::TOOL
        && icon_type != kind::APP
    {
        return SyscallResult::err(errors::INVALID_NAME);
    }
    match ctx.workbench.update_object(handle, &path, icon_type) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `WBConfig` — copy the Workbench configuration line into a user buffer.
///
/// Arguments: `dest_ptr`, `dest_len`. Returns the byte count including the
/// terminator.
pub fn wb_config(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let dest = args.get(0);
    let len = args.get(1) as usize;
    match write_text(dest, len, &ctx.workbench.config_text()) {
        Ok(count) => SyscallResult::ok(count as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `WBInfo` — copy the Workbench information line into a user buffer.
///
/// Arguments: `dest_ptr`, `dest_len`. Returns the byte count including the
/// terminator.
pub fn wb_info(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let dest = args.get(0);
    let len = args.get(1) as usize;
    match write_text(dest, len, &ctx.workbench.info_text()) {
        Ok(count) => SyscallResult::ok(count as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `WorkbenchControl` — query a control value.
///
/// Arguments: `operation`.
pub fn workbench_control(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let operation = args.get(0) as u32;
    match ctx.workbench.control(operation) {
        Ok(value) => SyscallResult::ok(value),
        Err(code) => SyscallResult::err(code),
    }
}
