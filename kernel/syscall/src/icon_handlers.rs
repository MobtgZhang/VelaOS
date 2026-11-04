//! The `icon.library` syscall handlers.
//!
//! # The boundary rule
//!
//! Every call here takes either an opaque [`IconHandle`] or a user pointer.
//! A handle is looked up in the kernel's own icon table; a pointer is copied
//! through [`crate::copy`], which validates and probes it. The kernel never
//! retains a user pointer, and it never returns one.
//!
//! # Why strings are copied before lookup
//!
//! `GetDiskObject`, `PutDiskObject` and `DeleteDiskObject` need a name. The
//! name is read into a kernel-owned `String` before it is used, so the object
//! registry is not a pointer-holding registry. The same is true of ToolType
//! values and tag lists.

use alloc::string::String;
use alloc::vec::Vec;

use vela_abi_native64::TagItem;
use vela_kernel_icon::{IconHandle, IconRect};

use crate::copy::{copy_from_user, copy_string_alloc, copy_to_user};
use crate::dispatch::SyscallContext;
use crate::numbers::{errors, SyscallArgs, SyscallResult};

/// The largest number of tags one icon may carry.
///
/// The limit bounds the kernel's storage and keeps a single syscall from
/// asking it to copy an arbitrary amount of user memory.
pub const MAX_ICON_TAGS: usize = 64;

/// Read a required user string, returning the error rather than a panic.
fn name(args: SyscallArgs, index: usize) -> Result<String, i32> {
    let ptr = args.get(index);
    if ptr == 0 {
        return Err(errors::BAD_POINTER);
    }
    copy_string_alloc(ptr)
}

/// `GetDiskObject` — find an icon by name.
pub fn get_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name = match name(args, 0) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    match ctx.icon.get_disk_object(&name) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `GetDiskObjectNew` — find an icon, creating a default if absent.
pub fn get_disk_object_new(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name = match name(args, 0) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    match ctx.icon.get_disk_object_new(&name) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `PutDiskObject` — save an icon under a name.
pub fn put_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let name = match name(args, 1) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    match ctx.icon.put_disk_object(handle, &name) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `FreeDiskObject` — release an icon handle.
pub fn free_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    match ctx.icon.free_disk_object(handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `DupDiskObject` — copy an icon into a new handle.
pub fn dup_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    match ctx.icon.dup_disk_object(handle) {
        Ok(copy) => SyscallResult::ok(copy.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `DeleteDiskObject` — remove an icon by name.
pub fn delete_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let name = match name(args, 0) {
        Ok(name) => name,
        Err(code) => return SyscallResult::err(code),
    };
    match ctx.icon.delete_disk_object(&name) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `GetDefDiskObject` — a copy of a default icon object.
pub fn get_def_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let icon_type = args.get(0) as u32;
    match ctx.icon.get_def_disk_object(icon_type) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `PutDefDiskObject` — replace a default icon object.
pub fn put_def_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let icon_type = args.get(0) as u32;
    let handle = IconHandle(args.get(1) as u32);
    match ctx.icon.put_def_disk_object(icon_type, handle) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `NewDiskObject` — a fresh default icon object.
pub fn new_disk_object(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let icon_type = args.get(0) as u32;
    match ctx.icon.new_disk_object(icon_type) {
        Ok(handle) => SyscallResult::ok(handle.0 as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `FindToolType` — write the named ToolType's NUL-terminated value into a
/// user buffer.
///
/// Arguments: `icon`, `type_name_ptr`, `dest_ptr`, `dest_len`. Returns `1`
/// when the ToolType exists and its value was written, `0` when it does not.
/// The value is written with its terminator, so the caller can treat the
/// buffer as a C string — which is what AmigaOS's `FindToolType` returns.
pub fn find_tool_type(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let key = match name(args, 1) {
        Ok(key) => key,
        Err(code) => return SyscallResult::err(code),
    };
    let dest = args.get(2);
    let dest_len = args.get(3) as usize;
    match ctx.icon.find_tool_type(handle, &key) {
        Ok(Some(value)) => {
            let mut bytes = value.into_bytes();
            bytes.push(0);
            if bytes.len() > dest_len || dest == 0 {
                return SyscallResult::err(if dest == 0 {
                    errors::BAD_POINTER
                } else {
                    errors::RANGE
                });
            }
            match unsafe { copy_to_user(dest, bytes.as_ptr(), bytes.len()) } {
                Ok(_) => SyscallResult::ok(1),
                Err(code) => SyscallResult::err(code),
            }
        }
        Ok(None) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `MatchToolValue` — compare a ToolType's value.
pub fn match_tool_value(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let key = match name(args, 1) {
        Ok(key) => key,
        Err(code) => return SyscallResult::err(code),
    };
    let value = match name(args, 2) {
        Ok(value) => value,
        Err(code) => return SyscallResult::err(code),
    };
    match ctx.icon.match_tool_value(handle, &key, &value) {
        Ok(matched) => SyscallResult::ok(matched as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `GetIconTagList` — copy an icon's tags into a user buffer.
///
/// Arguments: `icon`, `dest_ptr`, `max_count`. Returns the number of tags
/// actually copied, which is `min(max_count, stored tags)`.
pub fn get_icon_tag_list(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let dest = args.get(1);
    let max_count = args.get(2) as usize;
    let tags = match ctx.icon.get_icon_tag_list(handle) {
        Ok(tags) => tags,
        Err(code) => return SyscallResult::err(code),
    };
    let count = tags.len().min(max_count);
    if count == 0 {
        return SyscallResult::ok(0);
    }
    if dest == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let bytes = count * core::mem::size_of::<TagItem>();
    match unsafe { copy_to_user(dest, tags.as_ptr() as *const u8, bytes) } {
        Ok(_) => SyscallResult::ok(count as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `PutIconTagList` — replace an icon's tags from a user list.
pub fn put_icon_tag_list(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let tags_ptr = args.get(1);
    let count = args.get(2) as usize;
    if count > MAX_ICON_TAGS {
        return SyscallResult::err(errors::TOO_LARGE);
    }
    let mut tags: Vec<TagItem> = Vec::new();
    tags.resize(
        count,
        TagItem {
            ti_Tag: 0,
            ti_Data: 0,
        },
    );
    if count > 0 {
        if tags_ptr == 0 {
            return SyscallResult::err(errors::BAD_POINTER);
        }
        let bytes = count * core::mem::size_of::<TagItem>();
        match unsafe { copy_from_user(tags.as_mut_ptr() as *mut u8, tags_ptr, bytes) } {
            Ok(_) => {}
            Err(code) => return SyscallResult::err(code),
        }
    }
    match ctx.icon.put_icon_tag_list(handle, &tags) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `BumpRevision` — increment an icon's revision.
pub fn bumps_revision(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    match ctx.icon.bump_revision(handle) {
        Ok(revision) => SyscallResult::ok(revision as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `LayoutIcon` — set an icon's rectangle from a user buffer.
///
/// Arguments: `icon`, `rect_ptr`. The caller-owned [`IconRect`] is read,
/// validated and stored, then written back so the caller sees the rectangle
/// actually in force. A pointer rather than a packed return value because four
/// 16-bit fields need 64 bits, and a jump-table return reserves half its word
/// for the error code.
pub fn layout_icon(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let dest = args.get(1);
    if dest == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    let mut rect = IconRect::default_rect();
    // SAFETY: `rect` is a live local and `dest` is validated and probed by
    // `copy_from_user` before any byte is read.
    if unsafe {
        copy_from_user(
            &mut rect as *mut IconRect as *mut u8,
            dest,
            core::mem::size_of::<IconRect>(),
        )
    }
    .is_err()
    {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    match ctx.icon.layout_icon(handle, rect) {
        Ok(effective) => match unsafe {
            copy_to_user(
                dest,
                &effective as *const IconRect as *const u8,
                core::mem::size_of::<IconRect>(),
            )
        } {
            Ok(_) => SyscallResult::ok(0),
            Err(code) => SyscallResult::err(code),
        },
        Err(code) => SyscallResult::err(code),
    }
}

/// `GetIconRectangle` — write an icon's rectangle into a user buffer.
///
/// Arguments: `icon`, `rect_ptr`. Returns zero; the rectangle is the output.
pub fn get_icon_rectangle(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let dest = args.get(1);
    if dest == 0 {
        return SyscallResult::err(errors::BAD_POINTER);
    }
    match ctx.icon.get_icon_rectangle(handle) {
        Ok(rect) => match unsafe {
            copy_to_user(
                dest,
                &rect as *const IconRect as *const u8,
                core::mem::size_of::<IconRect>(),
            )
        } {
            Ok(_) => SyscallResult::ok(0),
            Err(code) => SyscallResult::err(code),
        },
        Err(code) => SyscallResult::err(code),
    }
}

/// `IconControl` — apply a control operation.
pub fn icon_control(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let operation = args.get(1) as u32;
    let value = args.get(2) as u32;
    match ctx.icon.icon_control(handle, operation, value) {
        Ok(result) => SyscallResult::ok(result),
        Err(code) => SyscallResult::err(code),
    }
}

/// `DrawIconState` — record an icon's selected state.
pub fn draw_icon_state(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    let selected = args.get(1) as u32;
    match ctx.icon.draw_icon_state(handle, selected) {
        Ok(()) => SyscallResult::ok(0),
        Err(code) => SyscallResult::err(code),
    }
}

/// `AddFreeList` — remember an icon handle.
pub fn add_free_list(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let handle = IconHandle(args.get(0) as u32);
    match ctx.icon.add_free_list(handle) {
        Ok(count) => SyscallResult::ok(count as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `FreeFreeList` — clear the remembered handles.
pub fn free_free_list(_args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    match ctx.icon.free_free_list() {
        Ok(count) => SyscallResult::ok(count as u64),
        Err(code) => SyscallResult::err(code),
    }
}

/// `ChangeToSelectedIconColor` — set the service's selected colour word.
pub fn change_to_selected_icon_color(args: SyscallArgs, ctx: &SyscallContext<'_>) -> SyscallResult {
    let color = args.get(0) as u32;
    match ctx.icon.change_to_selected_icon_color(color) {
        Ok(previous) => SyscallResult::ok(previous as u64),
        Err(code) => SyscallResult::err(code),
    }
}
