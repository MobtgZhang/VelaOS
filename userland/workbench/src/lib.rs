//! `workbench` — the desktop.
//!
//! # What this is
//!
//! A Workbench-*style* desktop, not a Workbench reimplementation. It has the
//! parts a desktop is recognisable by — a screen title bar, a menu bar with
//! real menus that drop down, an icon grid over the desktop background, and
//! windows that move, resize, raise and close — and every pixel in it is
//! original to VelaOS. The palette and the furniture drawing live in
//! `graphics/theme-classic`, which was written for this project rather than
//! derived from anything.
//!
//! # The shape of a desktop program
//!
//! A desktop is an **event loop**, and the whole design follows from that:
//!
//! 1. Put the desktop's own window behind everything else.
//! 2. Draw the screen's furniture — title bar, menu bar, desktop.
//! 3. Loop: wait on a port, dispatch what arrives, redraw what changed.
//!
//! Step 1 is what makes step 2 possible. Intuition composites layers back to
//! front, so the window that fills the screen must be at the *back* of the
//! stack; the menu bar then draws into it, and application windows composite
//! over the top. A desktop that drew its background last would have every
//! window painted over by its own wallpaper.
//!
//! # Menus
//!
//! Real menus, not a decoration. A menu title is a gadget on the desktop
//! window, so clicking one delivers an ordinary `IDCMP_GADGET` message through
//! the ordinary port. Opening the menu *adds* a gadget per item, at the item's
//! place on the screen; closing it removes them again. There is no
//! menu-specific code path — which means a menu cannot behave differently from
//! any other clickable thing, and cannot develop its own bugs.
//!
//! # Dragging and resizing
//!
//! Both are done with the same call the kernel offers for it:
//! `SetWindowBounds`. The desktop tracks the geometry of the window it opened,
//! because a user program has no way to *ask* the window manager where its
//! window is — the kernel returns the previous geometry from the call that
//! changes it, and there is no query.
//!
//! Dragging is done in *window-local* deltas, which is what keeps it correct
//! while the window is moving under the pointer: the pointer's local position
//! and the window's origin move together, so the delta between two successive
//! positions is the distance the pointer travelled.
//!
//! # Double-click to launch
//!
//! AmigaOS opens an application by double-clicking its icon. VelaOS tracks
//! the click sequence itself: a *press* on an icon, then a *release* on the
//! same icon, then a *second press* on it, is a double-click. The policy is
//! the program's, because the program owns it — the kernel delivers events and
//! knows nothing about what pairs with what.
//!
//! Counting *presses* rather than milliseconds is deliberate. There is no
//! clock syscall in v0.1.0, and a desktop that guessed a tick rate would be
//! wrong on any machine whose timer was not the one it guessed at.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;

use vela_sys::abi::{
    buttons, gadget_kind, idcmp, mouse_codes, request_codes, window_flags, IntuiMessage, Message,
    OpenWindowArgs, Rectangle,
};
use vela_sys::api::{self, Gadget, Pen, Port, Screen, Window};

/// The screen's geometry, as VelaOS's QEMU display is configured.
const SCREEN_W: i16 = 1280;
const SCREEN_H: i16 = 800;

/// The desktop window fills the screen exactly, so its client area is the
/// whole display with nothing showing through.
const DESKTOP_X: i16 = 0;
const DESKTOP_Y: i16 = 0;

/// The height of the *screen's* title bar, at the very top of the display.
///
/// The screen's title bar and the menu bar are two different pieces of
/// furniture, and a desktop that drew only one of them reads as a program
/// with a menu bar rather than as a desktop.
const TITLEBAR_H: i16 = 20;

/// How far the menu bar sits from the top of the screen.
const MENUBAR_Y: i16 = TITLEBAR_H;
/// The menu bar's height.
const MENUBAR_H: i16 = 20;

/// The icon grid's origin, and the spacing between icons.
///
/// Pinned to whole numbers on purpose: a grid that started at an odd pixel
/// would land half a pixel into a font cell and look permanently out of
/// alignment, which is the sort of detail that makes a desktop feel
/// unfinished.
const ICON_ORIGIN_X: i16 = 24;
const ICON_ORIGIN_Y: i16 = MENUBAR_Y + MENUBAR_H + 24;
const ICON_PITCH_X: i16 = 96;
const ICON_PITCH_Y: i16 = 88;

/// The width of an icon's label area, in pixels.
const ICON_W: u16 = 88;
/// The height of an icon's label area.
const ICON_H: u16 = 24;

/// The height of the icon graphic above its label.
const ICON_ART_H: u16 = 32;

/// The signal mask for the desktop's port.
///
/// `SIGB_MESSAGE` plus `SIGB_SINGLE` is what makes `WaitPort` return. A port
/// created with only `SIGB_MESSAGE` would queue messages without waking
/// anybody, which is the difference between a working desktop and a black
/// screen.
const SIGB: u32 = 0x4000;

/// How many icons fit across the grid.
///
/// Four, because the grid's origin and pitch put four comfortably inside a
/// 1280-pixel screen with a margin, and because a grid that could hold one more
/// column would be a grid whose last column is a single icon — which reads as
/// a mistake even when it is arithmetic.
const ICONS_PER_ROW: usize = 4;

/// The height of one row in a dropped-down menu.
const MENU_ITEM_H: i16 = 20;

/// The padding around a dropped-down menu's items.
const MENU_PANEL_PAD: i16 = 6;

/// The smallest a hosted application window may become.
///
/// A window that could be resized to nothing would leave a layer with no area
/// and a gadget that cannot be reached, which is a dead end for the desktop
/// and for the program inside it.
const MIN_WINDOW_W: u16 = 160;
const MIN_WINDOW_H: u16 = 120;

/// The side length of the resize grip, in the bottom-right corner.
const GRIP_SIZE: u16 = 16;

/// The window border's thickness, as the theme draws it.
const BORDER: i32 = 2;

/// One thing on the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// The `ramfs` volume.
    RamDisk,
    /// The `sysfs` volume.
    System,
    /// The text editor.
    Edit,
    /// The acceptance demo application.
    GuiTest,
    /// The shell.
    Shell,
}

impl Icon {
    /// Every icon, in the order they are laid out by default.
    ///
    /// The order is the *grid* order — left to right, then down — because that
    /// is what a user reading the desktop sees, and an icon list ordered by
    /// insertion would scramble the grid the moment a name changed.
    pub const ALL: [Icon; 5] = [
        Icon::RamDisk,
        Icon::System,
        Icon::Edit,
        Icon::Shell,
        Icon::GuiTest,
    ];

    /// The icon's displayed name.
    pub const fn name(self) -> &'static str {
        match self {
            Icon::RamDisk => "ramfs",
            Icon::System => "sysfs",
            Icon::Edit => "Edit",
            Icon::Shell => "Shell",
            Icon::GuiTest => "guitest",
        }
    }

    /// What launching it does.
    pub const fn target(self) -> &'static str {
        match self {
            Icon::RamDisk => "ramfs",
            Icon::System => "sysfs",
            Icon::Edit => "Edit",
            Icon::Shell => "Shell",
            Icon::GuiTest => "guitest",
        }
    }
}

/// One menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Menu {
    /// The menu's title, as it appears in the bar.
    pub title: &'static str,
    /// The items, in order.
    pub items: &'static [MenuItem],
}

/// One menu item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem {
    /// The item's label.
    pub label: &'static str,
    /// What it does.
    pub action: Action,
}

/// What a menu item or an icon does.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Action {
    /// Do nothing. A real menu item that is not yet wired up, so the menu bar
    /// can show the full set without every item having to work.
    Nothing,
    /// Open the file system drawer.
    Drawer,
    /// Close the frontmost window.
    CloseFront,
    /// Rearrange the icons.
    Rearrange,
    /// Open the README in a text window.
    Readme,
    /// Go up a directory. Always a no-op at the root, which is the whole point
    /// of it being an action rather than a special case.
    Up,
    /// End the session.
    Quit,
}

/// The menu bar's menus, left to right.
const MENUS: [Menu; 3] = [
    Menu {
        title: "Project",
        items: &[
            MenuItem {
                label: "ReadMe",
                action: Action::Readme,
            },
            MenuItem {
                label: "Rearrange",
                action: Action::Rearrange,
            },
        ],
    },
    Menu {
        title: "Window",
        items: &[
            MenuItem {
                label: "Open Drawer",
                action: Action::Drawer,
            },
            MenuItem {
                label: "Close Front",
                action: Action::CloseFront,
            },
            MenuItem {
                label: "Up",
                action: Action::Up,
            },
        ],
    },
    Menu {
        title: "System",
        items: &[
            MenuItem {
                label: "About VelaOS",
                action: Action::Nothing,
            },
            MenuItem {
                label: "Quit",
                action: Action::Quit,
            },
        ],
    },
];

/// Where a menu title sits in the menu bar.
#[derive(Debug, Clone, Copy)]
struct MenuGeometry {
    /// The title's left edge, in screen coordinates.
    x: i16,
}

/// An application window the desktop hosts.
#[derive(Debug, Clone, Copy)]
struct AppWindow {
    /// The window handle.
    window: Window,
    /// Its left edge, in screen coordinates.
    x: i16,
    /// Its top edge, in screen coordinates.
    y: i16,
    /// Its width.
    w: u16,
    /// Its height.
    h: u16,
    /// The resize grip in its bottom-right corner.
    grip: Gadget,
}

/// What a mouse drag is doing.
#[derive(Debug, Clone, Copy)]
enum Drag {
    /// Moving the window. The offsets are the distance from the window's
    /// origin to the point that was grabbed, so a move is "put the grabbed
    /// point back under the pointer".
    Move { grab_dx: i32, grab_dy: i32 },
}

/// The desktop's own state.
///
/// Everything the event loop needs, and nothing it does not. In particular
/// there is no "current mode": the desktop's response to a click is derived
/// from *what was clicked*, which is what stops it from ever being stuck in a
/// state it cannot leave.
pub struct Desktop {
    /// The port events arrive on.
    port: Port,
    /// The screen everything is on.
    screen: Screen,
    /// The window the desktop draws into: full-screen, at the back.
    backdrop: Window,
    /// The icons, in the order they are laid out.
    icons: Vec<Icon>,
    /// The handle of each icon's gadget, parallel to [`Self::icons`].
    icon_gadgets: Vec<Gadget>,
    /// The handle of each menu title's gadget, parallel to [`MENUS`].
    menu_gadgets: Vec<Gadget>,
    /// Where each menu title sits, parallel to [`MENUS`].
    menu_geometry: Vec<MenuGeometry>,
    /// The menu that is currently dropped down, if any.
    open_menu: Option<usize>,
    /// The gadgets that make up the dropped-down menu's items.
    menu_item_gadgets: Vec<(Gadget, Action)>,
    /// The frontmost application window, if there is one.
    front: Option<AppWindow>,
    /// The drag in progress, if any.
    drag: Option<Drag>,
    /// The icon that is currently held down.
    pressed_icon: Option<Icon>,
    /// The icon that was clicked and released most recently.
    last_click: Option<Icon>,
    /// Whether the session has been asked to end.
    quit: bool,
    /// How many events the loop has handled.
    pub events: u32,
    /// How many icon launches have happened.
    pub launches: u32,
    /// How many actions were dispatched.
    pub actions: u32,
}

impl Desktop {
    /// Build the desktop: open the screen, the window, the menu bar and the
    /// icons, and paint them.
    pub fn open() -> Result<Desktop, vela_sys::Error> {
        let port = api::create_port(SIGB)?;
        api::open_library("intuition.library").ok();
        api::open_library("graphics.library").ok();

        let screen = api::open_screen(
            "Workbench",
            &Rectangle::new(0, 0, SCREEN_W as u16, SCREEN_H as u16),
        )?;

        // The backdrop is full-screen and sent to the back *before* anything
        // else exists. Everything else composites over it, so whatever order
        // the rest of the desktop is built in, the backdrop cannot end up in
        // front.
        let args = OpenWindowArgs {
            x: DESKTOP_X,
            y: DESKTOP_Y,
            width: SCREEN_W as u16,
            height: SCREEN_H as u16,
            idcmp: idcmp::ALL,
            // No close gadget: the desktop has no title bar to close, and a
            // close gadget drawn at the screen's origin would look like a bug.
            flags: window_flags::NONE,
        };
        let backdrop = api::open_window(screen, "Workbench", &args, port)?;
        api::send_window_to_back(backdrop)?;

        let mut desktop = Desktop {
            port,
            screen,
            backdrop,
            icons: Vec::new(),
            icon_gadgets: Vec::new(),
            menu_gadgets: Vec::new(),
            menu_geometry: Vec::new(),
            open_menu: None,
            menu_item_gadgets: Vec::new(),
            front: None,
            drag: None,
            pressed_icon: None,
            last_click: None,
            quit: false,
            events: 0,
            launches: 0,
            actions: 0,
        };
        desktop.build_menu_bar()?;
        desktop.build_icons();
        desktop.paint();
        Ok(desktop)
    }

    /// The port events arrive on.
    pub fn port(&self) -> Port {
        self.port
    }

    /// The backdrop window.
    pub fn backdrop(&self) -> Window {
        self.backdrop
    }

    /// The frontmost application window.
    pub fn front_window(&self) -> Option<Window> {
        self.front.map(|f| f.window)
    }

    /// The frontmost application window's geometry, in screen coordinates.
    ///
    /// Published because a user program cannot ask the window manager where
    /// its window is, and a test that wanted to check that a drag moved the
    /// window would otherwise have nothing to compare against.
    pub fn front_bounds(&self) -> Option<Rectangle> {
        self.front.map(|f| Rectangle::new(f.x, f.y, f.w, f.h))
    }

    /// The icons, in the order they are laid out.
    pub fn icons(&self) -> &[Icon] {
        &self.icons
    }

    /// How many menu actions have been carried out.
    pub fn actions(&self) -> u32 {
        self.actions
    }

    /// Whether the session has been asked to end.
    pub fn is_quitting(&self) -> bool {
        self.quit
    }

    /// The menu that is currently dropped down, if any.
    pub fn open_menu(&self) -> Option<&'static str> {
        self.open_menu
            .and_then(|index| MENUS.get(index))
            .map(|menu| menu.title)
    }

    /// Add the menu bar's gadgets and labels.
    fn build_menu_bar(&mut self) -> Result<(), vela_sys::Error> {
        let mut x = MENUBAR_ORIGIN_X;
        for menu in MENUS.iter() {
            // The width is derived from the title's length rather than stored,
            // so a label that changes size resizes its own click target — which
            // is the failure mode of a hand-written constant: the label grows,
            // the highlight stays where it was, and the menu stops being
            // clickable on its last two letters.
            let width = (menu.title.len() as i16 + 2) * FONT_CELL_W;
            let bounds = Rectangle::new(x, MENUBAR_Y, width as u16, MENUBAR_H as u16);
            let gadget = api::add_gadget(self.backdrop, gadget_kind::BUTTON, &bounds, menu.title)?;
            self.menu_gadgets.push(gadget);
            self.menu_geometry.push(MenuGeometry { x });
            x += width + MENU_GAP;
        }
        Ok(())
    }

    /// Add the icon grid's gadgets.
    fn build_icons(&mut self) {
        for icon in Icon::ALL.iter() {
            let bounds = self.icon_bounds(self.icons.len());
            // `unwrap` is right here and nowhere else: the backdrop is not
            // interactive as far as the kernel is concerned — it is a window
            // this task owns, which is all `AddGadget` checks — and a failure
            // would mean the kernel rejected a rectangle that is visibly on
            // screen. There is no recovery for that, and inventing one would
            // hide it.
            let gadget = api::add_gadget(self.backdrop, gadget_kind::BUTTON, &bounds, icon.name())
                .expect("the desktop's own window always accepts a gadget");
            self.icon_gadgets.push(gadget);
            self.icons.push(*icon);
        }
    }

    /// An icon's rectangle, in the desktop window's coordinates.
    fn icon_bounds(&self, index: usize) -> Rectangle {
        let column = (index % ICONS_PER_ROW) as i16;
        let row = (index / ICONS_PER_ROW) as i16;
        Rectangle::new(
            ICON_ORIGIN_X + column * ICON_PITCH_X,
            ICON_ORIGIN_Y + row * ICON_PITCH_Y,
            ICON_W,
            ICON_H,
        )
    }

    /// Which icon a gadget handle names, if any.
    fn icon_of(&self, gadget: u64) -> Option<Icon> {
        self.icon_gadgets
            .iter()
            .position(|g| g.raw() == gadget)
            .and_then(|i| self.icons.get(i).copied())
    }

    /// Which menu a gadget handle is a title of, if any.
    fn menu_of(&self, gadget: u64) -> Option<usize> {
        self.menu_gadgets
            .iter()
            .position(|g| g.raw() == gadget)
            .filter(|index| MENUS.get(*index).is_some())
    }

    /// Paint the whole desktop.
    ///
    /// Repainting everything rather than invalidating regions is the right
    /// trade at this scale: the desktop is a few hundred drawing calls, and a
    /// partial repaint would need its own invalidation bookkeeping — which is
    /// exactly the kind of state that is wrong in one place and looks like a
    /// graphics bug everywhere else.
    fn paint(&self) {
        use api::colors as c;

        // The background. One filled rectangle, so a later repaint cannot
        // leave a stray line behind where a window used to be.
        if let Some(pen) = pen(c::DESKTOP) {
            let _ = api::graphics::fill_rect(self.backdrop, pen, &self.full());
        }

        // The screen's title bar, in its own colour so it reads as furniture
        // rather than as part of the wallpaper.
        if let Some(pen) = pen(c::TITLE) {
            let _ = api::graphics::fill_rect(
                self.backdrop,
                pen,
                &Rectangle::new(0, 0, SCREEN_W as u16, TITLEBAR_H as u16),
            );
        }
        if let Some(pen) = pen(c::WHITE) {
            let _ = api::graphics::text(self.backdrop, pen, 8, 2, "VelaOS Workbench");
        }

        // The menu bar's strip, in the title colour's lighter cousin so the
        // two bars are distinguishable.
        if let Some(pen) = pen(c::GADGET) {
            let _ = api::graphics::fill_rect(
                self.backdrop,
                pen,
                &Rectangle::new(0, MENUBAR_Y, SCREEN_W as u16, MENUBAR_H as u16),
            );
        }

        // The menu titles, each in the middle of its own gadget so the text and
        // the click target cannot disagree.
        //
        // The first title is drawn in the highlight colour because the desktop
        // is frontmost when it first paints, and a menu bar whose first item is
        // dimmed looks broken even though nothing is.
        for (index, menu) in MENUS.iter().enumerate() {
            let geometry = match self.menu_geometry.get(index) {
                Some(geometry) => geometry,
                None => continue,
            };
            let color = if index == 0 { c::HIGHLIGHT } else { c::WHITE };
            // `if let` rather than `unwrap`: a palette with no white in it must
            // produce a desktop with no menu text, not a panic. The two are
            // different bugs and only one of them is recoverable.
            if let Some(pen) = pen(color) {
                let _ = api::graphics::text(
                    self.backdrop,
                    pen,
                    geometry.x + FONT_CELL_W,
                    MENUBAR_Y + 4,
                    menu.title,
                );
            }
        }

        // The icon grid.
        for index in 0..self.icons.len() {
            self.paint_icon(index);
        }

        // The dropped-down menu, over everything else.
        self.paint_open_menu();
    }

    /// Paint one icon: a frame, a glyph block, and its name.
    fn paint_icon(&self, index: usize) {
        use api::colors as c;
        let Some(icon) = self.icons.get(index).copied() else {
            return;
        };
        let b = self.icon_bounds(index);
        // The icon's body is the label area plus the art above it, so the
        // selection frame a click would draw encloses the whole icon rather
        // than only the text under it.
        let body = Rectangle::new(
            b.min_x,
            b.min_y - ICON_ART_H as i16,
            b.max_x,
            ICON_H + ICON_ART_H,
        );

        if let Some(pen) = pen(c::GADGET) {
            let _ = api::graphics::rect(self.backdrop, pen, &body);
        }
        if let Some(pen) = pen(c::SHADOW) {
            // A two-tone block for the icon's "artwork". A real icon is a
            // bitmap; this is one rectangle, because inventing artwork is
            // outside what v0.1.0 is for and a solid block is honest about
            // being a placeholder rather than pretending to be a picture.
            let art = Rectangle::new(body.min_x + 12, body.min_y + 6, ICON_W - 24, ICON_ART_H - 8);
            let _ = api::graphics::fill_rect(self.backdrop, pen, &art);
        }
        if let Some(pen) = pen(c::WHITE) {
            let _ = api::graphics::text(
                self.backdrop,
                pen,
                b.min_x,
                b.min_y + FONT_CELL_H,
                icon.name(),
            );
        }
    }

    /// Paint the dropped-down menu, if one is open.
    fn paint_open_menu(&self) {
        use api::colors as c;
        let Some(index) = self.open_menu else {
            return;
        };
        let Some(menu) = MENUS.get(index) else {
            return;
        };
        let Some(geometry) = self.menu_geometry.get(index) else {
            return;
        };
        let panel = self.menu_panel(index);
        if let Some(pen) = pen(c::GADGET) {
            let _ = api::graphics::fill_rect(self.backdrop, pen, &panel);
        }
        if let Some(pen) = pen(c::GADGET_BORDER) {
            let _ = api::graphics::rect(self.backdrop, pen, &panel);
        }
        for (offset, item) in menu.items.iter().enumerate() {
            if let Some(pen) = pen(c::WHITE) {
                let _ = api::graphics::text(
                    self.backdrop,
                    pen,
                    geometry.x + MENU_PANEL_PAD,
                    MENUBAR_Y + MENUBAR_H + offset as i16 * MENU_ITEM_H + 2,
                    item.label,
                );
            }
        }
    }

    /// The rectangle a dropped-down menu occupies.
    fn menu_panel(&self, index: usize) -> Rectangle {
        let Some(menu) = MENUS.get(index) else {
            return Rectangle::new(0, 0, 0, 0);
        };
        let x = self
            .menu_geometry
            .get(index)
            .map(|geometry| geometry.x)
            .unwrap_or(0);
        let text_width = menu
            .items
            .iter()
            .map(|item| item.label.len() as i16 * FONT_CELL_W)
            .max()
            .unwrap_or(0);
        Rectangle::new(
            x,
            MENUBAR_Y + MENUBAR_H,
            (text_width + MENU_PANEL_PAD * 2) as u16,
            (menu.items.len() as i16 * MENU_ITEM_H + MENU_PANEL_PAD * 2) as u16,
        )
    }

    /// The whole screen, as a rectangle.
    fn full(&self) -> Rectangle {
        Rectangle::new(DESKTOP_X, DESKTOP_Y, SCREEN_W as u16, SCREEN_H as u16)
    }

    /// Handle one event.
    ///
    /// Returns `true` when something changed and a repaint is warranted. A
    /// repaint is not unconditional: a mouse *movement* with no button down
    /// changes nothing on this desktop, and repainting on every one of them
    /// would make the whole system redraw several hundred times a second for
    /// no visible difference.
    pub fn handle(&mut self, event: &IntuiMessage) -> bool {
        self.events += 1;
        match event.im_Type {
            t if t == IntuiMessage::GADGET => self.on_gadget(event),
            t if t == IntuiMessage::REQUEST => self.on_request(event),
            t if t == IntuiMessage::MOUSE => self.on_mouse(event),
            _ => false,
        }
    }

    /// A click landed on a gadget — a menu title, a menu item, an icon, or the
    /// resize grip.
    fn on_gadget(&mut self, event: &IntuiMessage) -> bool {
        let gadget = event.im_Code as u64;
        // The resize grip first: it belongs to the front window, and a drag
        // that starts on it resizes rather than moves.
        //
        // The grip is a gadget, so the kernel delivers *every* pointer event
        // over it as a gadget event — including the movements that make up
        // the drag. The resize is therefore driven from here rather than from
        // the plain-mouse path, which never sees a pointer that is over a
        // gadget.
        if let Some(front) = self.front.as_ref() {
            if front.grip.raw() == gadget {
                let held = event.im_Button & buttons::LEFT != 0;
                if held {
                    self.resize_to(event.im_MouseX as i32, event.im_MouseY as i32);
                }
                return true;
            }
        }
        // A menu item next: an open menu's items sit over the desktop, and
        // they are the most specific thing under the pointer.
        //
        // Only the *press* acts. A click is a press and a release, and the
        // release arrives as a second gadget event on the same gadget; acting
        // on it as well would run the item's action twice.
        let pressed = event.im_Button & buttons::LEFT != 0;
        if let Some(index) = self
            .menu_item_gadgets
            .iter()
            .position(|(g, _)| g.raw() == gadget)
        {
            if !pressed {
                return true;
            }
            let action = self.menu_item_gadgets[index].1;
            self.close_menu();
            self.run_action(action);
            return true;
        }
        if let Some(icon) = self.icon_of(gadget) {
            return self.on_icon(icon, event);
        }
        if let Some(index) = self.menu_of(gadget) {
            // Only the press opens or closes a menu, for the same reason: the
            // release of the same click would otherwise toggle the menu back
            // shut, so a menu would flash open and close again.
            if !pressed {
                return true;
            }
            return self.on_menu_title(index);
        }
        false
    }

    /// A menu title was clicked: drop the menu down, or close it.
    fn on_menu_title(&mut self, index: usize) -> bool {
        if self.open_menu == Some(index) {
            self.close_menu();
            return true;
        }
        self.close_menu();
        self.open_menu = Some(index);
        self.build_menu_items(index);
        true
    }

    /// Add a gadget for each item of the menu about to be shown.
    fn build_menu_items(&mut self, index: usize) {
        let Some(menu) = MENUS.get(index) else {
            return;
        };
        let panel = self.menu_panel(index);
        for (offset, item) in menu.items.iter().enumerate() {
            let bounds = Rectangle::new(
                panel.min_x + MENU_PANEL_PAD,
                MENUBAR_Y + MENUBAR_H + MENU_PANEL_PAD + offset as i16 * MENU_ITEM_H,
                panel.max_x.saturating_sub((MENU_PANEL_PAD * 2) as u16),
                MENU_ITEM_H as u16,
            );
            let gadget =
                match api::add_gadget(self.backdrop, gadget_kind::BUTTON, &bounds, item.label) {
                    Ok(gadget) => gadget,
                    Err(_) => continue,
                };
            self.menu_item_gadgets.push((gadget, item.action));
        }
    }

    /// Close the dropped-down menu, removing its items' gadgets.
    fn close_menu(&mut self) {
        for (gadget, _) in self.menu_item_gadgets.drain(..) {
            let _ = api::remove_gadget(self.backdrop, gadget);
        }
        self.open_menu = None;
    }

    /// Carry out a menu item's action.
    fn run_action(&mut self, action: Action) {
        self.actions += 1;
        match action {
            Action::Nothing | Action::Up => {}
            Action::Drawer => {
                self.open_front_window("ramfs", 140, 140, 360, 240);
            }
            Action::Readme => {
                if let Some(front) = self.open_front_window("ReadMe", 200, 180, 420, 260) {
                    let pen = api::graphics::alloc_pen(0, api::colors::WHITE);
                    if pen.is_valid() {
                        let _ = api::graphics::text(front, pen, 16, 40, "VelaOS 0.1.0 - Workbench");
                    }
                }
            }
            Action::CloseFront => self.close_front_window(),
            Action::Rearrange => self.rearrange_icons(),
            Action::Quit => self.quit = true,
        }
    }

    /// Reverse the icon order, rebuilding the grid's gadgets.
    fn rearrange_icons(&mut self) {
        for gadget in self.icon_gadgets.drain(..) {
            let _ = api::remove_gadget(self.backdrop, gadget);
        }
        self.icons.clear();
        for icon in Icon::ALL.iter().rev() {
            let bounds = self.icon_bounds(self.icons.len());
            let gadget = api::add_gadget(self.backdrop, gadget_kind::BUTTON, &bounds, icon.name())
                .expect("the desktop's own window always accepts a gadget");
            self.icon_gadgets.push(gadget);
            self.icons.push(*icon);
        }
    }

    /// An icon was clicked: track the click sequence, and launch on the second
    /// press of a pair.
    fn on_icon(&mut self, icon: Icon, event: &IntuiMessage) -> bool {
        // A click with no button in it is a movement that happened to land on
        // a gadget, not a click.
        let pressed = event.im_Button & buttons::LEFT != 0;
        if pressed {
            if self.last_click == Some(icon) {
                // The second press of a pair. Cleared *before* launching, so a
                // third click starts a fresh pair rather than being a third
                // click of the previous one — which is what makes rapid
                // triple-clicking launch once rather than twice.
                self.last_click = None;
                self.pressed_icon = None;
                self.launch(icon);
            } else {
                self.last_click = None;
                self.pressed_icon = Some(icon);
            }
        } else if self.pressed_icon == Some(icon) {
            self.pressed_icon = None;
            self.last_click = Some(icon);
        }
        true
    }

    /// Open whatever an icon names.
    fn launch(&mut self, icon: Icon) {
        self.launches += 1;
        // A drawer is a window; an application is a window this desktop hosts
        // on the application's behalf. Both are the same thing structurally —
        // which is the point: v0.1.0's desktop has one window type, and the
        // icons differ in what they put in it rather than in how they open it.
        let title = icon.target();
        self.open_front_window(title, 120, 120, 360, 240);
    }

    /// Open a window the desktop hosts, and make it the front window.
    fn open_front_window(&mut self, title: &str, x: i16, y: i16, w: u16, h: u16) -> Option<Window> {
        let args = OpenWindowArgs {
            x,
            y,
            width: w,
            height: h,
            idcmp: idcmp::ALL,
            flags: window_flags::CLOSEGADGET | window_flags::DEPTHGADGET,
        };
        let window = api::open_window(self.screen, title, &args, self.port).ok()?;
        // Bring it to the front *after* opening, so the z-order is explicit
        // rather than a side effect of when `OpenWindow` happened to run.
        let _ = api::bring_window_to_front(window);
        let grip = self.add_grip(window, w, h);
        let _ = api::repaint_window(window, true);
        self.front = Some(AppWindow {
            window,
            x,
            y,
            w,
            h,
            grip,
        });
        Some(window)
    }

    /// Add the resize grip to a window's bottom-right corner.
    fn add_grip(&self, window: Window, w: u16, h: u16) -> Gadget {
        let bounds = Rectangle::new(
            w.saturating_sub(GRIP_SIZE) as i16,
            h.saturating_sub(GRIP_SIZE) as i16,
            GRIP_SIZE,
            GRIP_SIZE,
        );
        api::add_gadget(window, gadget_kind::BUTTON, &bounds, " ")
            .expect("the desktop's own window always accepts a gadget")
    }

    /// Close the front window, if there is one.
    fn close_front_window(&mut self) {
        if let Some(front) = self.front.take() {
            let _ = api::close_window(front.window);
        }
    }

    /// An Intuition request — a close, from a window's close gadget.
    fn on_request(&mut self, event: &IntuiMessage) -> bool {
        if event.im_Code != request_codes::CLOSE {
            return false;
        }
        let window = Window(event.im_Source);
        if self.front.map(|front| front.window) == Some(window) {
            self.front = None;
            let _ = api::close_window(window);
            return true;
        }
        false
    }

    /// The screen position of a mouse event.
    ///
    /// The kernel reports mouse coordinates in the *source window's* frame, so
    /// turning them into screen coordinates needs the source window's origin.
    /// The desktop knows the origins of the two windows it is interested in —
    /// the backdrop, which is at the screen's origin, and the front window,
    /// whose position it tracks because it moved it.
    fn screen_pos(&self, event: &IntuiMessage) -> Option<(i32, i32)> {
        let source = Window(event.im_Source);
        let local_x = event.im_MouseX as i32;
        let local_y = event.im_MouseY as i32;
        if source == self.backdrop {
            return Some((local_x + DESKTOP_X as i32, local_y + DESKTOP_Y as i32));
        }
        let front = self.front.as_ref()?;
        if front.window != source {
            return None;
        }
        Some((local_x + front.x as i32, local_y + front.y as i32))
    }

    /// A mouse event that was not on a gadget.
    fn on_mouse(&mut self, event: &IntuiMessage) -> bool {
        let Some((sx, sy)) = self.screen_pos(event) else {
            return false;
        };
        match event.im_Code {
            code if code == mouse_codes::LEFT_DOWN => self.on_press(sx, sy),
            code if code == mouse_codes::LEFT_UP => {
                self.drag = None;
                false
            }
            code if code == mouse_codes::MOVE => {
                // A movement with no button down is the common case and is
                // ignored; a movement with a button down is a drag.
                if event.im_Button & buttons::LEFT == 0 {
                    return false;
                }
                self.on_drag(sx, sy)
            }
            _ => false,
        }
    }

    /// The left button went down somewhere.
    fn on_press(&mut self, sx: i32, sy: i32) -> bool {
        // Clicking an application window raises it, which is what a user
        // expects and what makes the next drag act on the window under the
        // pointer.
        if let Some(front) = self.front.as_ref() {
            if self.point_in_window(front, sx, sy) {
                let _ = api::bring_window_to_front(front.window);
                if self.on_title_bar(front, sx, sy) {
                    self.drag = Some(Drag::Move {
                        grab_dx: sx - front.x as i32,
                        grab_dy: sy - front.y as i32,
                    });
                    return true;
                }
            }
        }
        false
    }

    /// Whether a point is inside a window.
    fn point_in_window(&self, front: &AppWindow, sx: i32, sy: i32) -> bool {
        let x = sx - front.x as i32;
        let y = sy - front.y as i32;
        x >= 0 && x < front.w as i32 && y >= 0 && y < front.h as i32
    }

    /// Whether a point is on a window's title bar — and not on its close
    /// gadget, which the kernel handles itself.
    fn on_title_bar(&self, front: &AppWindow, sx: i32, sy: i32) -> bool {
        let x = sx - front.x as i32;
        let y = sy - front.y as i32;
        if y < BORDER || y >= BORDER + TITLEBAR_H as i32 {
            return false;
        }
        // The close gadget is drawn in the top-left corner of the title bar.
        // Dragging from it would both move the window and ask for a close.
        let close = 2 * (BORDER + 16);
        !(x < close)
    }

    /// The pointer moved with the button down.
    fn on_drag(&mut self, sx: i32, sy: i32) -> bool {
        let Some(Drag::Move { grab_dx, grab_dy }) = self.drag else {
            return false;
        };
        let Some(front) = self.front.as_ref() else {
            return false;
        };
        let x = (sx - grab_dx).clamp(0, (SCREEN_W as i32) - front.w as i32) as i16;
        let y = (sy - grab_dy).clamp(0, (SCREEN_H as i32) - front.h as i32) as i16;
        if x == front.x && y == front.y {
            return false;
        }
        let _ = api::set_window_bounds(front.window, x, y, 0, 0);
        if let Some(front) = self.front.as_mut() {
            front.x = x;
            front.y = y;
        }
        let _ = api::repaint_window(front_window_of(self), true);
        true
    }

    /// Resize the front window so that its grip follows the pointer.
    ///
    /// The coordinates are the front window's *local* ones, which is what an
    /// IDCMP message carries: the pointer's position inside the window that
    /// produced the event. The grip sits in the bottom-right corner, so the
    /// window's new size is the pointer's position plus the grip's own size.
    fn resize_to(&mut self, local_x: i32, local_y: i32) -> bool {
        let Some(front) = self.front.as_ref() else {
            return false;
        };
        let w = ((local_x + GRIP_SIZE as i32) as u16).max(MIN_WINDOW_W);
        let h = ((local_y + GRIP_SIZE as i32) as u16).max(MIN_WINDOW_H);
        if w == front.w && h == front.h {
            return false;
        }
        let _ = api::set_window_bounds(front.window, front.x, front.y, w, h);
        if let Some(front) = self.front.as_mut() {
            front.w = w;
            front.h = h;
        }
        self.move_grip();
        let _ = api::repaint_window(front_window_of(self), true);
        true
    }

    /// Put the resize grip back in the bottom-right corner after a resize.
    ///
    /// The grip is a gadget, and gadgets are positioned in window-local
    /// coordinates — so a window that grew has a grip in the wrong place
    /// unless it is moved. It is removed and added again rather than moved,
    /// because the kernel has no "move a gadget" call.
    fn move_grip(&mut self) {
        let Some(front) = self.front.as_ref() else {
            return;
        };
        let _ = api::remove_gadget(front.window, front.grip);
        let grip = self.add_grip(front.window, front.w, front.h);
        if let Some(front) = self.front.as_mut() {
            front.grip = grip;
        }
    }

    /// Run the event loop until the session ends.
    ///
    /// Bounded by [`MAX_EVENTS`] rather than looping forever: a desktop that
    /// waits forever on a port nobody posts to is indistinguishable from a
    /// hung kernel, and the difference matters enormously when diagnosing a
    /// failure.
    pub fn run(&mut self) {
        while self.events < MAX_EVENTS && !self.quit {
            let Ok(Some(mut msg)) = api::wait_port(self.port) else {
                // Either the wait ended with nothing, or the port is gone. The
                // first is ordinary; the second means there is nothing left to
                // read, so yielding is the only forward progress available.
                let _ = api::yield_now();
                continue;
            };
            let event = msg.as_intui();
            let dirty = self.handle(&event);

            // Reply to every message, including the ones the desktop ignored.
            // A message that is never replied to is a sender waiting forever,
            // and Intuition is a sender like any other.
            let mut reply = Message::empty();
            let _ = api::reply_msg(&mut msg, &mut reply);

            if dirty {
                self.paint();
            }
        }
        self.paint();
    }

    /// Close everything, back to front.
    pub fn shutdown(&mut self) {
        self.close_menu();
        self.close_front_window();
        let _ = api::close_window(self.backdrop);
        let _ = api::close_screen(self.screen);
        let _ = api::delete_port(self.port);
    }
}

/// The front window's handle, for the calls that need one.
fn front_window_of(desktop: &Desktop) -> Window {
    desktop
        .front
        .map(|front| front.window)
        .unwrap_or(Window::NULL)
}

/// How many events the loop handles before it gives up.
const MAX_EVENTS: u32 = 4096;

/// Where the menu bar's first title starts.
const MENUBAR_ORIGIN_X: i16 = 8;

/// The gap between menu titles, in pixels.
const MENU_GAP: i16 = 16;

/// The font's cell width, in pixels.
///
/// Eight, because that is the cell the built-in font is drawn in. It is
/// written down rather than asked for because there is no syscall that answers
/// "how wide is a character" in v0.1.0, and a program that measured it would
/// have to guess anyway.
const FONT_CELL_W: i16 = 8;

/// The font's cell height.
const FONT_CELL_H: i16 = 16;

/// Allocate a pen of a given colour, if the palette allows.
///
/// `None` on failure rather than a panic: a pen the palette cannot supply is
/// a rendering problem, not a reason to stop the desktop. Returning `None`
/// means the caller simply does not draw that element, which is a visibly
/// incomplete desktop rather than a black one.
fn pen(color: u32) -> Option<Pen> {
    let pen = api::graphics::alloc_pen(0, color);
    pen.is_valid().then_some(pen)
}

/// Open the desktop, run it, and close it.
pub fn run() {
    let mut desktop = match Desktop::open() {
        Ok(desktop) => desktop,
        Err(_) => {
            api::print("workbench: could not open the desktop\n");
            return;
        }
    };
    api::print("workbench: ready\n");
    desktop.run();
    api::print("workbench: events=");
    api::print_int(desktop.events as u64);
    api::print(" actions=");
    api::print_int(desktop.actions as u64);
    api::print(" launches=");
    api::print_int(desktop.launches as u64);
    api::print("\n");
    desktop.shutdown();
}
