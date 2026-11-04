//! `workbench.library`, end to end.
//!
//! # What these tests drive
//!
//! The Workbench is a *registry*, not a drawing: a program registers icons,
//! menus, windows and drop zones, and the desktop reads the registry. Every
//! call below goes through the SDK, the generated jump table and the kernel's
//! real handler, and lands in the real `Workbench` service.
//!
//! # Why messages go through Exec
//!
//! `NotifyWorkbench` and `SendAppWindowMessage` put ordinary Exec messages on
//! a port. The tests take those messages off the port with `GetMsg`, which is
//! what proves the notification path is Exec rather than a private side
//! channel the desktop alone can read.

use std::sync::{Mutex, MutexGuard, OnceLock};

use vela_sys::abi::{OpenWindowArgs, Rectangle, SIGB};
use vela_sys::api;
use vela_sys::api::workbench::{
    self, workbench_control, AppIconHandle, AppMenuItemHandle, AppWindowHandle, DropZoneHandle,
};
use vela_sys::errors;
use vela_sys::hosted;
use vela_test_api_conformance::harness;

/// Guards every test in this file.
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

/// The control query, as a `u64`.
fn control(operation: u32) -> u64 {
    workbench::workbench_control(operation).expect("WorkbenchControl")
}

/// `RegisterWorkbench`, `StartWorkbench`, `UnregisterWorkbench`,
/// `NotifyWorkbench`, `QuoteWorkbench`, `WBConfig`, `WBInfo` and
/// `WorkbenchControl`.
///
/// A registration binds the calling task's port. The notification test then
/// proves the binding is real: the message arrives on that port with the
/// action and data words the caller passed.
#[test]
fn workbench_register_start_and_notify() {
    let _guard = ready();

    let port = api::create_port(SIGB).expect("CreateMsgPort");
    assert_eq!(control(workbench_control::REGISTERED), 0);

    workbench::register_workbench(port.raw() as u32).expect("RegisterWorkbench");
    assert_eq!(control(workbench_control::REGISTERED), 1);
    assert_eq!(control(workbench_control::RUNNING), 0);

    workbench::start_workbench().expect("StartWorkbench");
    assert_eq!(control(workbench_control::RUNNING), 1);

    workbench::notify_workbench(5, 0xC0DE).expect("NotifyWorkbench");
    let message = api::get_msg(port).expect("GetMsg");
    assert_eq!(message.mn_payload[0], 5);
    assert_eq!(message.mn_payload[1], 0xC0DE);

    let config = workbench::wb_config().expect("WBConfig");
    assert!(
        config.contains("running=true"),
        "the configuration line must report the running state: {config}"
    );
    let info = workbench::wb_info().expect("WBInfo");
    assert!(
        info.contains("registered=true") && info.contains("running=true"),
        "the information line must report the registration: {info}"
    );

    // Quoting toggles and reports the previous state; two calls restore it.
    assert!(!workbench::quote_workbench().expect("QuoteWorkbench"));
    assert!(workbench::quote_workbench().expect("QuoteWorkbench"));

    // The live counts are consistent with the registry.
    assert_eq!(control(workbench_control::APP_ICONS), 0);
    assert_eq!(control(workbench_control::MENU_ITEMS), 0);
    assert_eq!(control(workbench_control::APP_WINDOWS), 0);
    assert_eq!(control(workbench_control::DROP_ZONES), 0);

    workbench::unregister_workbench().expect("UnregisterWorkbench");
    assert_eq!(control(workbench_control::REGISTERED), 0);
    assert_eq!(control(workbench_control::RUNNING), 0);
    assert_eq!(
        workbench::notify_workbench(1, 0)
            .expect_err("notifying after unregister")
            .code,
        errors::NO_PORT,
    );
    assert_eq!(
        workbench::unregister_workbench()
            .expect_err("unregistering twice")
            .code,
        errors::NO_OBJECT,
    );
    assert_eq!(
        workbench::start_workbench()
            .expect_err("starting an unregistered Workbench")
            .code,
        errors::NO_OBJECT,
    );

    api::delete_port(port).expect("DeleteMsgPort");
}

/// `AddAppIcon`, `GetNextAppIcon` and `RemoveAppIcon`.
///
/// The walk passes the last handle back as the cursor, which is how a program
/// iterates: zero starts, and zero from the walk ends it. Removing the icon the
/// walk started at must skip over the hole rather than stop.
#[test]
fn workbench_app_icons_walk_and_remove() {
    let _guard = ready();
    let before = control(workbench_control::APP_ICONS);

    let first = workbench::add_app_icon("System", "SYS:System", 3, 10, 20).expect("AddAppIcon");
    let second = workbench::add_app_icon("Tools", "SYS:Tools", 2, 30, 40).expect("AddAppIcon");
    assert!(!first.is_none() && !second.is_none());
    assert_eq!(control(workbench_control::APP_ICONS), before + 2);

    assert_eq!(
        workbench::get_next_app_icon(0).expect("GetNextAppIcon"),
        Some(first),
    );
    assert_eq!(
        workbench::get_next_app_icon(first.raw() as u64).expect("GetNextAppIcon"),
        Some(second),
    );
    assert_eq!(
        workbench::get_next_app_icon(second.raw() as u64).expect("GetNextAppIcon"),
        None,
        "zero is the normal end of the walk",
    );

    assert_eq!(
        workbench::add_app_icon("", "SYS:Empty", 3, 0, 0)
            .expect_err("an empty name")
            .code,
        errors::INVALID_NAME,
    );
    assert_eq!(
        workbench::add_app_icon("Bad", "SYS:Bad", 99, 0, 0)
            .expect_err("an unknown kind")
            .code,
        errors::INVALID_NAME,
    );

    workbench::remove_app_icon(first).expect("RemoveAppIcon");
    assert_eq!(control(workbench_control::APP_ICONS), before + 1);
    assert_eq!(
        workbench::get_next_app_icon(0).expect("GetNextAppIcon"),
        Some(second),
        "the walk must skip the removed slot",
    );
    assert_eq!(
        workbench::remove_app_icon(first)
            .expect_err("removing twice")
            .code,
        errors::NO_OBJECT,
    );

    workbench::remove_app_icon(second).expect("RemoveAppIcon");
    assert_eq!(control(workbench_control::APP_ICONS), before);

    assert_eq!(
        AppIconHandle::NONE.raw(),
        0,
        "zero names no application icon",
    );
}

/// `AddAppMenuItem` and `RemoveAppMenuItem`.
#[test]
fn workbench_menu_items_lifecycle() {
    let _guard = ready();
    let before = control(workbench_control::MENU_ITEMS);

    let item = workbench::add_app_menu_item("About VelaOS", 1).expect("AddAppMenuItem");
    assert!(!item.is_none());
    assert_eq!(control(workbench_control::MENU_ITEMS), before + 1);

    assert_eq!(
        workbench::add_app_menu_item("", 2)
            .expect_err("an empty label")
            .code,
        errors::INVALID_NAME,
    );

    workbench::remove_app_menu_item(item).expect("RemoveAppMenuItem");
    assert_eq!(control(workbench_control::MENU_ITEMS), before);
    assert_eq!(
        workbench::remove_app_menu_item(item)
            .expect_err("removing twice")
            .code,
        errors::NO_OBJECT,
    );

    assert_eq!(AppMenuItemHandle::NONE.raw(), 0);
}

/// `OpenWorkbenchObject`, `CloseWorkbenchObject`,
/// `MakeWorkbenchObjectVisible` and `UpdateWorkbenchObject`.
#[test]
fn workbench_objects_open_update_close() {
    let _guard = ready();
    let before = control(workbench_control::OBJECTS);

    let object =
        workbench::open_workbench_object("System", "SYS:System", 2).expect("OpenWorkbenchObject");
    assert!(!object.is_none());
    assert_eq!(control(workbench_control::OBJECTS), before + 1);

    assert_eq!(
        workbench::open_workbench_object("", "SYS:Empty", 2)
            .expect_err("an empty name")
            .code,
        errors::INVALID_NAME,
    );
    assert_eq!(
        workbench::open_workbench_object("Empty", "", 2)
            .expect_err("an empty path")
            .code,
        errors::INVALID_NAME,
    );

    workbench::make_workbench_object_visible(object, false).expect("MakeWorkbenchObjectVisible");
    workbench::make_workbench_object_visible(object, true).expect("MakeWorkbenchObjectVisible");
    assert_eq!(
        workbench::make_workbench_object_visible(workbench::WorkbenchHandle::from_raw(9999), true)
            .expect_err("an unknown object")
            .code,
        errors::NO_OBJECT,
    );

    workbench::update_workbench_object(object, "RAM:System", 3).expect("UpdateWorkbenchObject");
    assert_eq!(
        workbench::update_workbench_object(object, "", 3)
            .expect_err("an empty path")
            .code,
        errors::INVALID_NAME,
    );
    assert_eq!(
        workbench::update_workbench_object(
            workbench::WorkbenchHandle::from_raw(9999),
            "RAM:Nope",
            3,
        )
        .expect_err("an unknown object")
        .code,
        errors::NO_OBJECT,
    );

    workbench::close_workbench_object(object).expect("CloseWorkbenchObject");
    assert_eq!(
        workbench::close_workbench_object(workbench::WorkbenchHandle::from_raw(9999))
            .expect_err("closing an unknown object")
            .code,
        errors::NO_OBJECT,
    );
}

/// `AddAppWindow`, `AddAppWindowDropZone`, `SendAppWindowMessage`,
/// `RemoveAppWindow` and `RemoveAppWindowDropZone`.
///
/// The window is a real Intuition window with a real Exec port, so the message
/// the Workbench sends arrives where a desktop would read it. The second task
/// proves the ownership check: a window record belongs to the task that
/// registered it, and no other task can message or remove it.
#[test]
fn workbench_app_windows_drop_zones_and_messages() {
    let _guard = ready();
    let kernel = services();
    let owner = kernel.exec().current_task();

    let port = api::create_port(SIGB).expect("CreateMsgPort");
    let screen = api::open_screen(
        "Workbench",
        &Rectangle {
            min_x: 0,
            min_y: 0,
            max_x: 640,
            max_y: 480,
        },
    )
    .expect("OpenScreen");
    let window = api::open_window(
        screen,
        "App",
        &OpenWindowArgs {
            x: 0,
            y: 0,
            width: 320,
            height: 240,
            idcmp: 0,
            flags: 0,
        },
        port,
    )
    .expect("OpenWindow");

    let before_windows = control(workbench_control::APP_WINDOWS);
    let before_zones = control(workbench_control::DROP_ZONES);

    let app_window = workbench::add_app_window(window.raw() as u32).expect("AddAppWindow");
    assert!(!app_window.is_none());
    assert_eq!(control(workbench_control::APP_WINDOWS), before_windows + 1,);
    assert_eq!(
        workbench::add_app_window(0)
            .expect_err("a null window")
            .code,
        errors::NO_OBJECT,
    );
    assert_eq!(
        workbench::add_app_window(9999)
            .expect_err("an unknown window")
            .code,
        errors::NO_OBJECT,
    );

    let zone = workbench::add_app_window_drop_zone(window.raw() as u32, 1, 2, 10, 10)
        .expect("AddAppWindowDropZone");
    assert_eq!(control(workbench_control::DROP_ZONES), before_zones + 1);
    assert_eq!(
        workbench::add_app_window_drop_zone(window.raw() as u32, 0, 0, 0, 10)
            .expect_err("a zero-width drop zone")
            .code,
        errors::BAD_RECT,
    );
    assert_eq!(
        workbench::add_app_window_drop_zone(9999, 1, 2, 10, 10)
            .expect_err("an unknown window")
            .code,
        errors::NO_OBJECT,
    );

    workbench::send_app_window_message(app_window, 7, 0x55).expect("SendAppWindowMessage");
    let message = api::get_msg(port).expect("GetMsg");
    assert_eq!(message.mn_payload[0], 7);
    assert_eq!(message.mn_payload[1], 0x55);

    let other = harness::task(kernel);
    kernel.exec().set_current_task(other);
    assert_eq!(
        workbench::send_app_window_message(app_window, 8, 0)
            .expect_err("messaging another task's window")
            .code,
        errors::NOT_OWNER,
    );
    assert_eq!(
        workbench::remove_app_window(app_window)
            .expect_err("removing another task's window")
            .code,
        errors::NOT_OWNER,
    );
    kernel.exec().set_current_task(owner);

    workbench::remove_app_window_drop_zone(zone).expect("RemoveAppWindowDropZone");
    assert_eq!(control(workbench_control::DROP_ZONES), before_zones);
    assert_eq!(
        workbench::remove_app_window_drop_zone(zone)
            .expect_err("removing twice")
            .code,
        errors::NO_OBJECT,
    );

    workbench::remove_app_window(app_window).expect("RemoveAppWindow");
    assert_eq!(control(workbench_control::APP_WINDOWS), before_windows,);
    assert_eq!(
        workbench::remove_app_window(app_window)
            .expect_err("removing twice")
            .code,
        errors::NO_OBJECT,
    );

    api::close_window(window).expect("CloseWindow");
    api::close_screen(screen).expect("CloseScreen");
    api::delete_port(port).expect("DeleteMsgPort");

    assert_eq!(DropZoneHandle::NONE.raw(), 0);
    assert_eq!(AppWindowHandle::NONE.raw(), 0);
}
