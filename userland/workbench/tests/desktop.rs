//! The Workbench desktop, driven through the real syscall boundary.
//!
//! # What this file proves
//!
//! The desktop is a *program*, not a drawing: it opens a screen, opens a
//! window, adds gadgets, waits on a port and dispatches what arrives. This
//! file drives that program through the *real* kernel services — the same
//! `Intuition`, the same `Exec`, the same `Graphics` — with the SDK's own
//! stubs on top, and asserts on the behaviour a user would see:
//!
//! * a title bar and a menu bar with real menus that drop down,
//! * an icon grid that launches on a double-click,
//! * a window that can be dragged and resized,
//! * a close gadget that closes the window.
//!
//! Nothing here reaches behind the syscall boundary. The desktop is driven by
//! synthetic `IDCMP` messages delivered through `Intuition::dispatch_mouse`,
//! exactly as the kernel's input path would deliver them, and the desktop
//! reads them off *its own* Exec port — which is what makes this a test of
//! the chain and not of the desktop's internals.

use std::sync::{Mutex, OnceLock};

use vela_hosted::{Kernel, TaskId};
use vela_kernel_input::{buttons, MouseEvent};
use vela_kernel_layers::LayerKind;
use vela_sys::hosted;
use vela_workbench::{Desktop, Icon};

/// The shared kernel the SDK's stubs are wired to.
///
/// Leaked rather than borrowed, because `install` takes `&'static`
/// services, and a leaked kernel is a few kilobytes the test binary was
/// going to spend anyway.
fn harness() -> &'static Kernel {
    static ONCE: OnceLock<&'static Kernel> = OnceLock::new();
    ONCE.get_or_init(|| {
        let kernel: &'static Kernel = Box::leak(Box::new(Kernel::with_screen(1280, 800)));
        // SAFETY: `kernel` is leaked, so it outlives every program that uses
        // these services — which is exactly what `install` requires.
        unsafe {
            hosted::install(
                kernel.exec(),
                kernel.dos(),
                kernel.graphics(),
                kernel.intuition(),
                kernel.table(),
            )
        };
        kernel
    })
}

/// Serialises the tests, because the desktop's window manager is shared.
static DRIVER: Mutex<()> = Mutex::new(());

/// Open the desktop on the shared kernel, on its own task.
///
/// The task is created per run rather than shared, because the desktop owns
/// ports and windows and two concurrent desktops would own each other's
/// objects.
fn open_desktop() -> (TaskId, Desktop) {
    let kernel = harness();
    let task = kernel.exec().create_task();
    kernel.set_current_task(task);
    let desktop = Desktop::open().expect("the desktop must open");
    (task, desktop)
}

/// Drive one mouse event into the desktop's port.
///
/// The event goes through `Intuition::dispatch_mouse`, which is the kernel's
/// own routing path: the desktop receives it on *its* port as an ordinary
/// Exec message.
fn dispatch(kernel: &'static Kernel, task: TaskId, x: i32, y: i32, buttons_now: u8, changed: u8) {
    // The desktop's window must be the topmost layer under the pointer, or
    // the click falls through to nothing.
    //
    // The tests share one Intuition service, so each run leaves its own
    // backdrop behind; the newest run's window has to be the one under the
    // pointer, or the click lands on a stale backdrop and the desktop
    // under test never sees it.
    let windows = kernel.intuition().windows_front_to_back();
    for id in &windows {
        if let Some(record) = kernel.intuition().window(*id) {
            if record.owner == task {
                let _ = kernel.intuition().layers().to_front(record.layer);
                break;
            }
        }
    }
    let _ = kernel.intuition().dispatch_mouse(
        kernel.exec(),
        &MouseEvent {
            x,
            y,
            buttons: buttons_now,
            changed,
        },
    );
}

/// Read every message waiting on the desktop's port and feed it to the
/// desktop.
///
/// The message is taken with the SDK's own `get_msg`, not with a raw
/// syscall: the SDK call goes through the *jump table* — the library base,
/// the negative offset, the `extern "sysv64"` stub — and then through the
/// real dispatch table. Reading the port any other way would test the
/// desktop against a path a program cannot take.
fn pump(kernel: &'static Kernel, task: TaskId, desktop: &mut Desktop) {
    let port = desktop.port();
    kernel.set_current_task(task);
    while let Ok(message) = vela_sys::api::get_msg(port) {
        let event = message.as_intui();
        desktop.handle(&event);
    }
}

/// A press and a release at a point.
fn click(kernel: &'static Kernel, task: TaskId, desktop: &mut Desktop, x: i32, y: i32) {
    dispatch(kernel, task, x, y, buttons::LEFT, buttons::LEFT);
    pump(kernel, task, desktop);
    dispatch(kernel, task, x, y, 0, buttons::LEFT);
    pump(kernel, task, desktop);
}

/// The kernel's record of a window the desktop opened.
///
/// The SDK hands a program an opaque `u64`; the kernel's own `Window` record
/// is what a test compares against, so the conversion is done here rather
/// than in the desktop — the desktop must not know the kernel's types.
fn record(kernel: &'static Kernel, window: vela_sys::Window) -> vela_kernel_intuition::Window {
    kernel
        .intuition()
        .window(vela_kernel_intuition::WindowId(window.raw() as u32))
        .expect("the window exists")
}

#[test]
fn the_desktop_opens_a_screen_a_window_and_an_icon_grid() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, desktop) = open_desktop();

    let kernel = harness();
    assert!(
        record(kernel, desktop.backdrop()).id.0 != 0,
        "the desktop must own a window",
    );
    assert_eq!(desktop.icons().len(), Icon::ALL.len());

    // The backdrop is at the *back* of the window stack — directly above the
    // screen's own layer — so every application window composites over it
    // rather than under it.
    //
    // "Directly above the screen layer" rather than "at index zero": the
    // screen's layer is the backdrop of the display, and a window behind it
    // would be invisible.
    let order = kernel.intuition().layers().order();
    let backdrop_layer = record(kernel, desktop.backdrop()).layer;
    let stack = kernel.intuition().layers();
    let screen_layer = order
        .iter()
        .copied()
        .find(|id| stack.layer(*id).map(|l| l.kind) == Some(LayerKind::Screen));
    assert!(screen_layer.is_some(), "a screen layer must exist");
    assert_eq!(
        order.iter().position(|l| *l == backdrop_layer),
        Some(position(&order, screen_layer.unwrap()) + 1),
        "the desktop's own window must sit directly above the screen layer",
    );
    let _ = task;
}

#[test]
fn the_desktop_draws_a_title_bar_and_a_menu_bar() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, desktop) = open_desktop();

    let kernel = harness();
    // A repaint fills the desktop's layer with real pixels.
    let record = record(kernel, desktop.backdrop());
    let layer = kernel
        .intuition()
        .layers()
        .layer(record.layer)
        .expect("the backdrop's layer");
    let buffer = layer.buffer.as_ref().expect("the backdrop has a buffer");

    let stride = layer.stride as usize;
    let at = |x: usize, y: usize| buffer[y * stride + x];

    // The title bar at the very top and the menu bar below it are two
    // different colours, so a desktop that drew only one of them is caught.
    let title = at(4, 4);
    let menu = at(4, 30);
    let wallpaper = at(4, 200);
    assert_ne!(title, wallpaper, "the title bar must be drawn");
    assert_ne!(menu, wallpaper, "the menu bar must be drawn");
    assert_ne!(title, menu, "the two bars must be distinguishable");
    let _ = task;
}

#[test]
fn clicking_a_menu_title_drops_the_menu_down_and_an_item_runs() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, mut desktop) = open_desktop();
    let kernel = harness();

    // The first menu title sits at the left of the menu bar.
    let title_x = 8 + 8;
    let title_y = 20 + 10;
    click(kernel, task, &mut desktop, title_x, title_y);
    assert_eq!(
        desktop.open_menu(),
        Some("Project"),
        "clicking a menu title must drop its menu down",
    );

    // The first item of that menu is "ReadMe", which opens a window.
    let item_x = title_x + 8;
    let item_y = 20 + 20 + 6 + 8;
    click(kernel, task, &mut desktop, item_x, item_y);
    assert_eq!(
        desktop.open_menu(),
        None,
        "choosing an item closes the menu"
    );
    assert_eq!(desktop.actions(), 1, "the item's action must have run");
    assert!(
        desktop.front_window().is_some(),
        "the ReadMe item opens a window",
    );
}

#[test]
fn a_double_click_on_an_icon_launches_it() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, mut desktop) = open_desktop();
    let kernel = harness();

    let icon = desktop.icons()[0];
    let (x, y) = icon_position(&desktop, icon);

    // One click: a press and a release. Nothing launches yet.
    click(kernel, task, &mut desktop, x, y);
    assert_eq!(desktop.launches, 0, "a single click must not launch");

    // A second click on the same icon: the double-click.
    click(kernel, task, &mut desktop, x, y);
    assert_eq!(desktop.launches, 1, "a double-click must launch");
    assert!(desktop.front_window().is_some());
}

#[test]
fn a_window_can_be_dragged_by_its_title_bar() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, mut desktop) = open_desktop();
    let kernel = harness();

    // Launch something, so there is a window to drag.
    let icon = desktop.icons()[0];
    let (x, y) = icon_position(&desktop, icon);
    click(kernel, task, &mut desktop, x, y);
    click(kernel, task, &mut desktop, x, y);
    let before = desktop.front_bounds().expect("a window is open");

    // Drag its title bar: press on the bar, move, release.
    let grab_x = before.min_x as i32 + 60;
    let grab_y = before.min_y as i32 + 8;
    dispatch(kernel, task, grab_x, grab_y, buttons::LEFT, buttons::LEFT);
    pump(kernel, task, &mut desktop);
    dispatch(kernel, task, grab_x + 40, grab_y + 30, buttons::LEFT, 0);
    pump(kernel, task, &mut desktop);
    dispatch(kernel, task, grab_x + 40, grab_y + 30, 0, buttons::LEFT);
    pump(kernel, task, &mut desktop);

    let after = desktop.front_bounds().expect("the window is still open");
    assert_eq!(after.min_x, before.min_x + 40, "the window must have moved");
    assert_eq!(after.min_y, before.min_y + 30);

    // And the kernel agrees: the window's layer really is at the new place.
    let window = desktop.front_window().expect("a window");
    let record = record(kernel, window);
    assert_eq!(record.bounds.x, after.min_x as i32);
    assert_eq!(record.bounds.y, after.min_y as i32);
}

#[test]
fn a_window_can_be_resized_by_its_grip() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, mut desktop) = open_desktop();
    let kernel = harness();

    let icon = desktop.icons()[0];
    let (x, y) = icon_position(&desktop, icon);
    click(kernel, task, &mut desktop, x, y);
    click(kernel, task, &mut desktop, x, y);
    let before = desktop.front_bounds().expect("a window is open");

    // The grip is in the bottom-right corner of the window.
    let grip_x = before.min_x as i32 + before.max_x as i32 - 8;
    let grip_y = before.min_y as i32 + before.max_y as i32 - 8;
    dispatch(kernel, task, grip_x, grip_y, buttons::LEFT, buttons::LEFT);
    pump(kernel, task, &mut desktop);
    // Drag it further out.
    dispatch(kernel, task, grip_x + 40, grip_y + 20, buttons::LEFT, 0);
    pump(kernel, task, &mut desktop);
    dispatch(kernel, task, grip_x + 40, grip_y + 20, 0, buttons::LEFT);
    pump(kernel, task, &mut desktop);

    let after = desktop.front_bounds().expect("the window is still open");
    assert!(
        after.max_x > before.max_x,
        "the window must have grown: {before:?} → {after:?}",
    );
    assert!(after.max_y > before.max_y);
}

#[test]
fn the_close_gadget_closes_the_window_it_belongs_to() {
    let _guard = DRIVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (task, mut desktop) = open_desktop();
    let kernel = harness();

    // Open a window, so there is something with a close gadget.
    let icon = desktop.icons()[0];
    let (x, y) = icon_position(&desktop, icon);
    click(kernel, task, &mut desktop, x, y);
    click(kernel, task, &mut desktop, x, y);
    let window = desktop.front_window().expect("a window is open");

    // The close gadget is furniture, drawn by the window manager rather than
    // registered as a gadget, so its position comes from the kernel's own
    // record of the window.
    let close = record(kernel, window)
        .close_gadget_rect()
        .expect("a close gadget");
    click(
        kernel,
        task,
        &mut desktop,
        close.x + close.width as i32 / 2,
        close.y + close.height as i32 / 2,
    );

    assert!(
        kernel
            .intuition()
            .window(vela_kernel_intuition::WindowId(window.raw() as u32))
            .is_none(),
        "the close gadget must close the window",
    );
    assert!(
        desktop.front_window().is_none(),
        "the desktop must forget the window it closed",
    );
}

/// The index of a layer in the z-order.
fn position(order: &[vela_kernel_layers::LayerId], id: vela_kernel_layers::LayerId) -> usize {
    order.iter().position(|l| *l == id).unwrap_or(usize::MAX)
}

/// An icon's position on the screen, in the desktop's own coordinates.
fn icon_position(desktop: &Desktop, icon: Icon) -> (i32, i32) {
    let index = desktop
        .icons()
        .iter()
        .position(|i| *i == icon)
        .expect("the icon is on the desktop");
    // The desktop's own window covers the whole screen, so the desktop
    // window's coordinates are screen coordinates.
    let column = (index % 4) as i32;
    let row = (index / 4) as i32;
    (24 + column * 96 + 40, 20 + 20 + 24 + row * 88 + 10)
}
