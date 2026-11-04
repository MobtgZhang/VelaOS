//! `icon.library`, end to end.
//!
//! # What these tests hold
//!
//! Every call here travels the chain a real program uses: the SDK's
//! [`vela_sys::api::icon`] → `library_base::icon::base()` → the generated jump
//! table → the generated stub → `hosted::dispatch` → the kernel's handler in
//! `vela_kernel_syscall::icon_handlers` → the `IconService`. A missing table
//! slot, a stub that ignores an argument, or a handler that disagrees with the
//! SDK's argument order fails here.
//!
//! # Why the objects are named
//!
//! An icon handle is an opaque integer, so the only way to prove that
//! `PutDiskObject` did anything is to ask `GetDiskObject` for the new name.
//! Every lifecycle test therefore works through names, which is also how a
//! real program finds an icon.

use std::sync::{Mutex, MutexGuard, OnceLock};

use vela_kernel_syscall::numbers::{self, errors, SyscallArgs};
use vela_sys::abi::{tags, TagItem};
use vela_sys::api::icon::{self, icon_control, icon_kind};
use vela_sys::errors as sdk_errors;
use vela_sys::hosted;
use vela_test_api_conformance::harness;

/// Guards the process-global hosted services across tests.
fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The one kernel installed for the life of the process.
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

/// Take the lock, install the services, and name a task for this thread.
fn ready() -> MutexGuard<'static, ()> {
    let guard = serial();
    let kernel = services();
    let task = harness::task(kernel);
    kernel.exec().set_current_task(task);
    guard
}

/// `NewDiskObject`, `PutDiskObject`, `GetDiskObject`, `GetDiskObjectNew`,
/// `DupDiskObject`, `FreeDiskObject` and `DeleteDiskObject`.
///
/// The lifecycle is: create, name, find by name, copy, release. Deleting the
/// named object makes the lookup report `NOT_FOUND`, which is the only
/// observable proof that the record is gone rather than merely unnamed.
#[test]
fn icon_disk_object_lifecycle_names_and_duplicates() {
    let _guard = ready();

    let created = icon::new_disk_object(icon_kind::DISK).expect("NewDiskObject");
    assert!(!created.is_none(), "a new icon must have a handle");

    icon::put_disk_object(created, "SYS:Icons/Test.info").expect("PutDiskObject");
    let found = icon::get_disk_object("SYS:Icons/Test.info").expect("GetDiskObject");
    assert_eq!(
        found, created,
        "the name must find the object it was put on"
    );

    let copy = icon::dup_disk_object(created).expect("DupDiskObject");
    assert_ne!(copy, created, "a duplicate must be a distinct handle");

    let created_by_name =
        icon::get_disk_object_new("SYS:Icons/Created.info").expect("GetDiskObjectNew");
    let again = icon::get_disk_object_new("SYS:Icons/Created.info").expect("GetDiskObjectNew");
    assert_eq!(
        created_by_name, again,
        "GetDiskObjectNew must return the existing object on the second call"
    );

    icon::free_disk_object(copy).expect("FreeDiskObject");
    assert_eq!(
        icon::free_disk_object(copy)
            .expect_err("freeing twice")
            .code,
        sdk_errors::NO_OBJECT,
        "a freed handle must report NO_OBJECT"
    );

    icon::delete_disk_object("SYS:Icons/Test.info").expect("DeleteDiskObject");
    assert_eq!(
        icon::get_disk_object("SYS:Icons/Test.info")
            .expect_err("finding a deleted icon")
            .code,
        sdk_errors::NOT_FOUND,
        "a deleted icon must be gone from the registry"
    );

    assert_eq!(
        icon::new_disk_object(99).expect_err("unknown kind").code,
        sdk_errors::INVALID_NAME,
    );
    assert_eq!(
        icon::get_disk_object("").expect_err("empty name").code,
        sdk_errors::INVALID_NAME,
    );
    assert_eq!(
        icon::delete_disk_object("SYS:Icons/Never.info")
            .expect_err("deleting a missing icon")
            .code,
        sdk_errors::NOT_FOUND,
    );
}

/// `GetDefDiskObject`, `PutDefDiskObject`, `NewDiskObject`, `FindToolType`
/// and `MatchToolValue`.
///
/// Each default carries `TYPE=<kind>` as a ToolType, which is what makes a
/// default useful: a program can ask what kind an icon is without trusting a
/// separate field. `PutDefDiskObject` must be observable through the next
/// `GetDefDiskObject`, and the test restores the original default before it
/// ends so that later tests see the table they expect.
#[test]
fn icon_defaults_and_tool_types() {
    let _guard = ready();

    // Keep a copy of the untouched drawer default so the replacement below
    // can be undone: a fresh drawer object would be a copy of whatever the
    // default currently holds, not of the original.
    let original_drawer = icon::get_def_disk_object(icon_kind::DRAWER).expect("GetDefDiskObject");
    assert_eq!(
        icon::find_tool_type(original_drawer, "TYPE")
            .expect("FindToolType")
            .as_deref(),
        Some("DRAWER"),
    );

    for (kind, expected) in [
        (icon_kind::DISK, "DISK"),
        (icon_kind::TOOL, "TOOL"),
        (icon_kind::APP, "APP"),
    ] {
        let handle = icon::get_def_disk_object(kind).expect("GetDefDiskObject");
        let value = icon::find_tool_type(handle, "TYPE")
            .expect("FindToolType")
            .expect("every default has a TYPE ToolType");
        assert_eq!(value, expected, "the default ToolType must name its kind");
        icon::free_disk_object(handle).expect("FreeDiskObject");
    }

    let tool = icon::new_disk_object(icon_kind::TOOL).expect("NewDiskObject");
    assert_eq!(
        icon::find_tool_type(tool, "type").expect("FindToolType"),
        Some(String::from("TOOL")),
        "ToolType lookup must be case-insensitive",
    );
    assert!(
        icon::match_tool_value(tool, "TYPE", "tool").expect("MatchToolValue"),
        "the value comparison must be case-insensitive",
    );
    assert!(
        !icon::match_tool_value(tool, "TYPE", "DRAWER").expect("MatchToolValue"),
        "a different value must not match",
    );
    assert_eq!(
        icon::find_tool_type(tool, "MISSING").expect("FindToolType"),
        None,
        "an absent ToolType is None, not an empty value",
    );

    let replacement = icon::new_disk_object(icon_kind::DRAWER).expect("NewDiskObject");
    icon::icon_control(replacement, icon_control::SET_TYPE, icon_kind::APP)
        .expect("IconControl SET_TYPE");
    icon::put_def_disk_object(icon_kind::DRAWER, replacement).expect("PutDefDiskObject");

    let observed = icon::get_def_disk_object(icon_kind::DRAWER).expect("GetDefDiskObject");
    assert_eq!(
        icon::icon_control(observed, icon_control::GET_TYPE, 0).expect("IconControl GET_TYPE"),
        u64::from(icon_kind::APP),
        "the replacement default must be what GetDefDiskObject returns",
    );
    icon::free_disk_object(observed).expect("FreeDiskObject");

    // Restore the default with the copy taken before the replacement.
    icon::put_def_disk_object(icon_kind::DRAWER, original_drawer).expect("PutDefDiskObject");
    let restored = icon::get_def_disk_object(icon_kind::DRAWER).expect("GetDefDiskObject");
    assert_eq!(
        icon::icon_control(restored, icon_control::GET_TYPE, 0).expect("IconControl GET_TYPE"),
        u64::from(icon_kind::DRAWER),
        "the table must be restored for the other tests",
    );
    icon::free_disk_object(restored).expect("FreeDiskObject");
    icon::free_disk_object(original_drawer).expect("FreeDiskObject");
    icon::free_disk_object(tool).expect("FreeDiskObject");
    icon::free_disk_object(replacement).expect("FreeDiskObject");
}

/// `PutIconTagList` and `GetIconTagList` round-trip, and the cap is enforced
/// before the kernel is asked to copy anything.
#[test]
fn icon_tag_list_round_trip() {
    let _guard = ready();

    let handle = icon::new_disk_object(icon_kind::DISK).expect("NewDiskObject");
    assert!(
        icon::get_icon_tag_list(handle)
            .expect("GetIconTagList")
            .is_empty(),
        "a new icon starts with no tags",
    );

    let list = [
        TagItem {
            ti_Tag: 10,
            ti_Data: 42,
        },
        TagItem {
            ti_Tag: 11,
            ti_Data: 0xDEAD_BEEF,
        },
    ];
    icon::put_icon_tag_list(handle, &list).expect("PutIconTagList");
    assert_eq!(
        icon::get_icon_tag_list(handle).expect("GetIconTagList"),
        list,
        "the stored tags must be the ones that were put",
    );

    let oversized = vec![
        TagItem {
            ti_Tag: tags::TAG_END,
            ti_Data: 0,
        };
        icon::MAX_TAGS + 1
    ];
    assert_eq!(
        icon::put_icon_tag_list(handle, &oversized)
            .expect_err("an oversized tag list")
            .code,
        sdk_errors::TOO_LARGE,
    );

    icon::free_disk_object(handle).expect("FreeDiskObject");
}

/// `BumpRevision`, `LayoutIcon`, `GetIconRectangle`, `IconControl` and
/// `DrawIconState`.
///
/// The rectangle travels packed through one word; this test is what holds the
/// SDK's packing to the kernel's, because a mismatch would only show up as a
/// window drawn in the wrong place.
#[test]
fn icon_geometry_revision_and_state() {
    let _guard = ready();

    let handle = icon::new_disk_object(icon_kind::DISK).expect("NewDiskObject");
    assert_eq!(
        icon::get_icon_rectangle(handle).expect("GetIconRectangle"),
        icon::IconRect::default_rect(),
    );

    let rect = icon::IconRect::new(10, 20, 40, 30);
    assert_eq!(icon::layout_icon(handle, rect).expect("LayoutIcon"), rect,);
    assert_eq!(
        icon::get_icon_rectangle(handle).expect("GetIconRectangle"),
        rect,
        "the stored rectangle must be the one that was laid out",
    );
    assert_eq!(
        icon::layout_icon(handle, icon::IconRect::new(0, 0, 0, 10))
            .expect_err("a zero-width rectangle")
            .code,
        sdk_errors::BAD_RECT,
    );

    assert_eq!(icon::bump_revision(handle).expect("BumpRevision"), 2);
    assert_eq!(icon::bump_revision(handle).expect("BumpRevision"), 3);

    assert_eq!(
        icon::icon_control(handle, icon_control::GET_SELECTED, 0)
            .expect("IconControl GET_SELECTED"),
        0,
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::SELECT, 0).expect("IconControl SELECT"),
        1,
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::GET_SELECTED, 0)
            .expect("IconControl GET_SELECTED"),
        1,
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::DESELECT, 0).expect("IconControl DESELECT"),
        0,
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::GET_TYPE, 0).expect("IconControl GET_TYPE"),
        u64::from(icon_kind::DISK),
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::SET_TYPE, icon_kind::APP)
            .expect("IconControl SET_TYPE"),
        u64::from(icon_kind::APP),
    );
    assert_eq!(
        icon::icon_control(handle, icon_control::SET_TYPE, 99)
            .expect_err("an unknown kind")
            .code,
        sdk_errors::INVALID_NAME,
    );
    assert_eq!(
        icon::icon_control(handle, 99, 0)
            .expect_err("an unknown operation")
            .code,
        sdk_errors::RANGE,
    );

    icon::draw_icon_state(handle, 1).expect("DrawIconState");
    assert_eq!(
        icon::icon_control(handle, icon_control::GET_SELECTED, 0)
            .expect("IconControl GET_SELECTED"),
        1,
    );
    icon::draw_icon_state(handle, 0).expect("DrawIconState");
    assert_eq!(
        icon::icon_control(handle, icon_control::GET_SELECTED, 0)
            .expect("IconControl GET_SELECTED"),
        0,
    );

    icon::free_disk_object(handle).expect("FreeDiskObject");
}

/// `AddFreeList`, `FreeFreeList` and `ChangeToSelectedIconColor`.
///
/// The free list records handles, not pointers, so adding the same handle
/// twice is idempotent. The colour call returns the previous value, which is
/// how a program restores the colour it replaced.
#[test]
fn icon_free_list_and_selected_colour() {
    let _guard = ready();

    let first = icon::new_disk_object(icon_kind::DISK).expect("NewDiskObject");
    let second = icon::new_disk_object(icon_kind::DRAWER).expect("NewDiskObject");

    assert_eq!(icon::add_free_list(first).expect("AddFreeList"), 1);
    assert_eq!(
        icon::add_free_list(first).expect("AddFreeList again"),
        1,
        "the same handle must not be recorded twice",
    );
    assert_eq!(icon::add_free_list(second).expect("AddFreeList"), 2);
    assert_eq!(icon::free_free_list().expect("FreeFreeList"), 2);
    assert_eq!(
        icon::free_free_list().expect("FreeFreeList again"),
        0,
        "the list must be empty after it is freed",
    );

    assert_eq!(
        icon::change_to_selected_icon_color(7).expect("ChangeToSelectedIconColor"),
        0,
    );
    assert_eq!(
        icon::change_to_selected_icon_color(3).expect("ChangeToSelectedIconColor"),
        7,
        "the call must return the colour it replaced",
    );

    icon::free_disk_object(first).expect("FreeDiskObject");
    icon::free_disk_object(second).expect("FreeDiskObject");
}

/// The kernel's refusals, reached by number because a typed API will not hand
/// over a bad pointer.
///
/// A syscall handler must never panic and must never read an address the caller
/// does not own. A null name and a kernel-half name are both `BAD_POINTER`;
/// an unknown handle is `NO_OBJECT`.
#[test]
fn icon_rejects_bad_pointers_and_handles() {
    let _guard = ready();
    let kernel = services();
    let task = kernel.exec().current_task();

    let null_name = harness::call(
        kernel,
        task,
        numbers::ICON_GET_DISK_OBJECT,
        SyscallArgs::new(0, 0, 0, 0, 0, 0),
    );
    assert_eq!(null_name.error, errors::BAD_POINTER);

    let kernel_name = harness::call(
        kernel,
        task,
        numbers::ICON_GET_DISK_OBJECT,
        SyscallArgs::new(harness::KERNEL_ADDRESS, 0, 0, 0, 0, 0),
    );
    assert_eq!(kernel_name.error, errors::BAD_POINTER);

    let unknown = harness::call(
        kernel,
        task,
        numbers::ICON_GET_ICON_RECTANGLE,
        SyscallArgs::new(9999, harness::bytes(&[0u8; 16]), 0, 0, 0, 0),
    );
    assert_eq!(unknown.error, errors::NO_OBJECT);
}
