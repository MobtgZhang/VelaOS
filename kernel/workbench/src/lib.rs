//! Workbench objects: the `workbench.library` service.
//!
//! # What this crate is for
//!
//! The Workbench library is the registry behind the desktop. A program that
//! wants its icon on the desktop registers it here; a program that wants its
//! menu or its window to be a Workbench window registers that too. The service
//! remembers the records and the handles, and the desktop draws them.
//!
//! VelaOS keeps the *shape* of the AmigaOS Workbench API — a registration, a
//! set of app icons and menu items, window and drop-zone records, and
//! notification through the Exec message system — but it does not pretend to
//! be the AmigaOS implementation. Every record is a kernel-owned value and
//! every call takes an opaque handle back. Nothing here returns a pointer.
//!
//! # Notification
//!
//! A registered Workbench owns a message port. `NotifyWorkbench` sends a
//! message there through [`vela_kernel_exec::Exec`], exactly as every other
//! event in VelaOS does. There is no private desktop event pipe: the report's
//! rule that IDCMP events land on Exec ports applies to Workbench messages too.
//!
//! # What is not drawn
//!
//! This crate records geometry and state. It does not paint. The desktop
//! program is the renderer, and the renderer reads these records rather than
//! owning a second copy of them.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use spin::Mutex;

use vela_kernel_exec::port::PortId;
use vela_kernel_exec::task::TaskId;
use vela_kernel_intuition::WindowId;
use vela_renderer2d::Rect;

/// The error codes this service reports.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// The handle names no record.
    pub const NO_OBJECT: i32 = -2;
    /// The record is already registered.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The name or path is invalid.
    pub const INVALID_NAME: i32 = -5;
    /// The object does not exist.
    pub const NOT_FOUND: i32 = -6;
    /// The rectangle is invalid.
    pub const BAD_RECT: i32 = -13;
    /// The caller is not the owner.
    pub const NOT_OWNER: i32 = -24;
    /// The workbench has no port.
    pub const NO_PORT: i32 = -33;
}

/// An opaque handle to a Workbench object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkbenchHandle(pub u32);

impl WorkbenchHandle {
    /// The handle that names no object.
    pub const NONE: WorkbenchHandle = WorkbenchHandle(0);

    /// True when this handle names no object.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// An opaque handle to a registered application icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppIconHandle(pub u32);

impl AppIconHandle {
    /// The handle that names no app icon.
    pub const NONE: AppIconHandle = AppIconHandle(0);

    /// True when this handle names no app icon.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// An opaque handle to a Workbench menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppMenuItemHandle(pub u32);

impl AppMenuItemHandle {
    /// The handle that names no menu item.
    pub const NONE: AppMenuItemHandle = AppMenuItemHandle(0);

    /// True when this handle names no menu item.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// An opaque handle to an application window record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AppWindowHandle(pub u32);

impl AppWindowHandle {
    /// The handle that names no app window.
    pub const NONE: AppWindowHandle = AppWindowHandle(0);

    /// True when this handle names no app window.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// An opaque handle to a drop zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DropZoneHandle(pub u32);

impl DropZoneHandle {
    /// The handle that names no drop zone.
    pub const NONE: DropZoneHandle = DropZoneHandle(0);

    /// True when this handle names no drop zone.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// One Workbench object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchObject {
    /// The object's display name.
    pub name: String,
    /// The path the object names.
    pub path: String,
    /// The object type, from `icon.library`'s kinds.
    pub icon_type: u32,
    /// Whether the object is open.
    pub open: bool,
    /// Whether the object is visible on the desktop.
    pub visible: bool,
}

/// One application icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppIcon {
    /// The icon's display name.
    pub name: String,
    /// The path the icon launches.
    pub path: String,
    /// The icon type.
    pub icon_type: u32,
    /// The icon's x position.
    pub x: i32,
    /// The icon's y position.
    pub y: i32,
}

/// One menu item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppMenuItem {
    /// The item's label.
    pub label: String,
    /// The item's action word.
    pub action: u32,
}

/// One application window record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppWindow {
    /// The Intuition window handle.
    pub window: WindowId,
    /// The task that owns the window.
    pub owner: TaskId,
}

/// One drop zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropZone {
    /// The window the zone belongs to.
    pub window: WindowId,
    /// The zone's rectangle.
    pub rect: Rect,
}
/// The `WorkbenchControl` operations.
pub mod control {
    /// Query whether the Workbench is running.
    pub const RUNNING: u32 = 1;
    /// Query the number of Workbench objects.
    pub const OBJECTS: u32 = 2;
    /// Query the number of application icons.
    pub const APP_ICONS: u32 = 3;
    /// Query the number of menu items.
    pub const MENU_ITEMS: u32 = 4;
    /// Query the number of application windows.
    pub const APP_WINDOWS: u32 = 5;
    /// Query the number of drop zones.
    pub const DROP_ZONES: u32 = 6;
    /// Query whether the Workbench is registered.
    pub const REGISTERED: u32 = 7;
}

/// The Workbench service.
pub struct Workbench {
    /// Workbench objects, indexed by handle.
    objects: Mutex<Vec<Option<WorkbenchObject>>>,
    /// Application icons, indexed by handle.
    app_icons: Mutex<Vec<Option<AppIcon>>>,
    /// Menu items, indexed by handle.
    menu_items: Mutex<Vec<Option<AppMenuItem>>>,
    /// Application windows, indexed by handle.
    app_windows: Mutex<Vec<Option<AppWindow>>>,
    /// Drop zones, indexed by handle.
    drop_zones: Mutex<Vec<Option<DropZone>>>,
    /// The next object id.
    next_object: Mutex<u32>,
    /// The next app-icon id.
    next_icon: Mutex<u32>,
    /// The next menu-item id.
    next_menu: Mutex<u32>,
    /// The next app-window id.
    next_window: Mutex<u32>,
    /// The next drop-zone id.
    next_zone: Mutex<u32>,
    /// The registered owner and message port.
    owner: Mutex<Option<(TaskId, PortId)>>,
    /// Whether the Workbench has been started.
    running: Mutex<bool>,
    /// Whether Workbench paths are quoted.
    quoted: Mutex<bool>,
}

impl core::fmt::Debug for Workbench {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Workbench")
            .field("objects", &self.objects.lock().len())
            .field("app_icons", &self.app_icons.lock().len())
            .field("menu_items", &self.menu_items.lock().len())
            .field("app_windows", &self.app_windows.lock().len())
            .field("drop_zones", &self.drop_zones.lock().len())
            .field("running", &self.running.lock())
            .field("quoted", &self.quoted.lock())
            .finish()
    }
}

impl Default for Workbench {
    fn default() -> Self {
        Self::new()
    }
}

impl Workbench {
    /// An empty Workbench service.
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(Vec::new()),
            app_icons: Mutex::new(Vec::new()),
            menu_items: Mutex::new(Vec::new()),
            app_windows: Mutex::new(Vec::new()),
            drop_zones: Mutex::new(Vec::new()),
            next_object: Mutex::new(1),
            next_icon: Mutex::new(1),
            next_menu: Mutex::new(1),
            next_window: Mutex::new(1),
            next_zone: Mutex::new(1),
            owner: Mutex::new(None),
            running: Mutex::new(false),
            quoted: Mutex::new(false),
        }
    }

    /// Take the next id from `next`.
    fn next_id(next: &Mutex<u32>) -> u32 {
        let mut next = next.lock();
        let id = *next;
        *next += 1;
        id
    }

    /// `RegisterWorkbench` — register the desktop's owner and port.
    pub fn register(&self, owner: TaskId, port: PortId) -> Result<(), i32> {
        if port.is_none() {
            return Err(errors::NO_PORT);
        }
        let mut slot = self.owner.lock();
        if slot.is_some() {
            return Err(errors::OBJECT_IN_USE);
        }
        *slot = Some((owner, port));
        Ok(())
    }

    /// `UnregisterWorkbench` — drop the registration.
    pub fn unregister(&self) -> Result<(), i32> {
        let mut slot = self.owner.lock();
        if slot.is_none() {
            return Err(errors::NO_OBJECT);
        }
        *slot = None;
        *self.running.lock() = false;
        Ok(())
    }

    /// `StartWorkbench` — mark the desktop as started.
    pub fn start(&self) -> Result<(), i32> {
        if self.owner.lock().is_none() {
            return Err(errors::NO_OBJECT);
        }
        *self.running.lock() = true;
        Ok(())
    }

    /// Whether the Workbench is running.
    pub fn is_running(&self) -> bool {
        *self.running.lock()
    }

    /// The port a Workbench notification goes to.
    pub fn notification_port(&self) -> Option<PortId> {
        self.owner.lock().as_ref().map(|(_, port)| *port)
    }

    /// The registered owner.
    pub fn owner(&self) -> Option<TaskId> {
        self.owner.lock().as_ref().map(|(owner, _)| *owner)
    }

    /// `OpenWorkbenchObject` — create and open a Workbench object.
    pub fn open_object(
        &self,
        name: &str,
        path: &str,
        icon_type: u32,
    ) -> Result<WorkbenchHandle, i32> {
        if name.is_empty() || path.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let id = Self::next_id(&self.next_object);
        self.objects.lock().push(Some(WorkbenchObject {
            name: String::from(name),
            path: String::from(path),
            icon_type,
            open: true,
            visible: true,
        }));
        Ok(WorkbenchHandle(id))
    }

    /// `CloseWorkbenchObject` — close a Workbench object.
    pub fn close_object(&self, handle: WorkbenchHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut objects = self.objects.lock();
        let Some(Some(object)) = objects.get_mut(index) else {
            return Err(errors::NO_OBJECT);
        };
        object.open = false;
        object.visible = false;
        Ok(())
    }

    /// `MakeWorkbenchObjectVisible` — set visibility.
    pub fn make_visible(&self, handle: WorkbenchHandle, visible: bool) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut objects = self.objects.lock();
        let Some(Some(object)) = objects.get_mut(index) else {
            return Err(errors::NO_OBJECT);
        };
        object.visible = visible;
        Ok(())
    }

    /// `UpdateWorkbenchObject` — change an object's path and type.
    pub fn update_object(
        &self,
        handle: WorkbenchHandle,
        path: &str,
        icon_type: u32,
    ) -> Result<(), i32> {
        if path.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut objects = self.objects.lock();
        let Some(Some(object)) = objects.get_mut(index) else {
            return Err(errors::NO_OBJECT);
        };
        object.path = String::from(path);
        object.icon_type = icon_type;
        Ok(())
    }

    /// `AddAppIcon` — register an application icon.
    pub fn add_app_icon(
        &self,
        name: &str,
        path: &str,
        icon_type: u32,
        x: i32,
        y: i32,
    ) -> Result<AppIconHandle, i32> {
        if name.is_empty() || path.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let id = Self::next_id(&self.next_icon);
        self.app_icons.lock().push(Some(AppIcon {
            name: String::from(name),
            path: String::from(path),
            icon_type,
            x,
            y,
        }));
        Ok(AppIconHandle(id))
    }

    /// `RemoveAppIcon` — remove an application icon.
    pub fn remove_app_icon(&self, handle: AppIconHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut icons = self.app_icons.lock();
        match icons.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// `GetNextAppIcon` — the next registered icon after `cursor`.
    pub fn get_next_app_icon(&self, cursor: u64) -> Option<AppIconHandle> {
        let icons = self.app_icons.lock();
        let start = cursor as usize;
        for (index, icon) in icons.iter().enumerate().skip(start) {
            if icon.is_some() {
                return Some(AppIconHandle(index as u32 + 1));
            }
        }
        None
    }

    /// `AddAppMenuItem` — register a menu item.
    pub fn add_app_menu_item(&self, label: &str, action: u32) -> Result<AppMenuItemHandle, i32> {
        if label.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let id = Self::next_id(&self.next_menu);
        self.menu_items.lock().push(Some(AppMenuItem {
            label: String::from(label),
            action,
        }));
        Ok(AppMenuItemHandle(id))
    }

    /// `RemoveAppMenuItem` — remove a menu item.
    pub fn remove_app_menu_item(&self, handle: AppMenuItemHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut items = self.menu_items.lock();
        match items.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// `AddAppWindow` — register an application window.
    pub fn add_app_window(&self, window: WindowId, owner: TaskId) -> Result<AppWindowHandle, i32> {
        let id = Self::next_id(&self.next_window);
        self.app_windows
            .lock()
            .push(Some(AppWindow { window, owner }));
        Ok(AppWindowHandle(id))
    }

    /// `RemoveAppWindow` — remove an application window record.
    pub fn remove_app_window(&self, handle: AppWindowHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut windows = self.app_windows.lock();
        match windows.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// The window record behind a handle.
    pub fn app_window(&self, handle: AppWindowHandle) -> Option<AppWindow> {
        if handle.is_none() {
            return None;
        }
        let index = handle.0 as usize - 1;
        self.app_windows.lock().get(index).and_then(|w| *w)
    }

    /// `AddAppWindowDropZone` — register a drop zone on a window.
    pub fn add_drop_zone(&self, window: WindowId, rect: Rect) -> Result<DropZoneHandle, i32> {
        if rect.width == 0 || rect.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let id = Self::next_id(&self.next_zone);
        self.drop_zones.lock().push(Some(DropZone { window, rect }));
        Ok(DropZoneHandle(id))
    }

    /// `RemoveAppWindowDropZone` — remove a drop zone.
    pub fn remove_drop_zone(&self, handle: DropZoneHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut zones = self.drop_zones.lock();
        match zones.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// The drop-zone record behind a handle.
    pub fn drop_zone(&self, handle: DropZoneHandle) -> Option<DropZone> {
        if handle.is_none() {
            return None;
        }
        let index = handle.0 as usize - 1;
        self.drop_zones.lock().get(index).and_then(|z| *z)
    }

    /// `QuoteWorkbench` — toggle path quoting and report the previous state.
    pub fn quote(&self) -> bool {
        let mut quoted = self.quoted.lock();
        let previous = *quoted;
        *quoted = !previous;
        previous
    }

    /// Whether Workbench paths are quoted.
    pub fn is_quoted(&self) -> bool {
        *self.quoted.lock()
    }

    /// The Workbench configuration line.
    pub fn config_text(&self) -> String {
        let running = *self.running.lock();
        let quoted = *self.quoted.lock();
        let objects = self.objects.lock().iter().filter(|o| o.is_some()).count();
        let icons = self.app_icons.lock().iter().filter(|i| i.is_some()).count();
        let menus = self
            .menu_items
            .lock()
            .iter()
            .filter(|m| m.is_some())
            .count();
        let windows = self
            .app_windows
            .lock()
            .iter()
            .filter(|w| w.is_some())
            .count();
        let zones = self
            .drop_zones
            .lock()
            .iter()
            .filter(|z| z.is_some())
            .count();
        alloc::format!(
            "workbench running={running} quoted={quoted} objects={objects} icons={icons} menus={menus} windows={windows} drop_zones={zones}"
        )
    }

    /// The Workbench information line.
    pub fn info_text(&self) -> String {
        let registered = self.owner.lock().is_some();
        let running = *self.running.lock();
        let quoted = *self.quoted.lock();
        alloc::format!(
            "VelaOS Workbench 0.1.0 registered={registered} running={running} quoted={quoted}"
        )
    }

    /// `WorkbenchControl` — query a control value.
    pub fn control(&self, operation: u32) -> Result<u64, i32> {
        match operation {
            control::RUNNING => Ok(*self.running.lock() as u64),
            control::OBJECTS => {
                Ok(self.objects.lock().iter().filter(|o| o.is_some()).count() as u64)
            }
            control::APP_ICONS => {
                Ok(self.app_icons.lock().iter().filter(|i| i.is_some()).count() as u64)
            }
            control::MENU_ITEMS => Ok(self
                .menu_items
                .lock()
                .iter()
                .filter(|m| m.is_some())
                .count() as u64),
            control::APP_WINDOWS => Ok(self
                .app_windows
                .lock()
                .iter()
                .filter(|w| w.is_some())
                .count() as u64),
            control::DROP_ZONES => Ok(self
                .drop_zones
                .lock()
                .iter()
                .filter(|z| z.is_some())
                .count() as u64),
            control::REGISTERED => Ok(self.owner.lock().is_some() as u64),
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// The number of live Workbench objects.
    pub fn object_count(&self) -> usize {
        self.objects.lock().iter().filter(|o| o.is_some()).count()
    }

    /// The number of live app icons.
    pub fn app_icon_count(&self) -> usize {
        self.app_icons.lock().iter().filter(|i| i.is_some()).count()
    }
}
