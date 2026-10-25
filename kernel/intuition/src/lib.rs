// Intuition: screens, windows, gadgets, and IDCMP event routing.
//
// # The one thing this crate must get right
//
// The report is explicit, in §7.5 and again in §10.4:
//!
//! > 图形事件（IDCMP）应最终落到 Exec 端口/消息系统，而不是为 Workbench
//! > 单独实现一套私有事件管道。
//!
//! "IDCMP events must land on the Exec port/message system, not a private
//! event pipe of their own."
//
//! So there is no `Intuition::events()` iterator for a program to read, no
//! callback registry, no ring buffer on the side. Every input event becomes a
//! [`vela_abi_native64::Message`] on an Exec [`PortId`] owned by the window,
//! and the program reaches it with `WaitPort` and `GetMsg` — the same calls
//! it would use for any other message. That is the M4 acceptance criterion,
//! and it is why this crate depends on `vela-kernel-exec` rather than
//! queueing events of its own.
//
// # The event path, end to end
//
// ```text
// PS/2 byte -> vela_kernel_input::Keyboard/Mouse -> InputEvent
//          -> Intuition::dispatch_event        (this crate)
//          -> topmost layer under the pointer
//          -> that window's port, via Exec::put_msg
//          -> program: WaitPort -> GetMsg -> ReplyMsg
// ```
//
// Each arrow is a decision this crate makes, and each has a failure mode worth
// naming:
//
// * **Which window?** The topmost layer containing the point. Anything else
//!   delivers a click to a window the user cannot see.
// * **Which port?** The one the window was opened with. A window with no
//!   message port gets no events — that is what `IDCMPFlags` of zero means,
//!   and it is how a program that only wants drawing opts out.
// * **What message?** An `IntuiMessage` subclass packed into the core
//   message's payload block, so the value crosses the syscall boundary with
//   no pointer and no allocation on the sender's side.
// * **Who is woken?** The port's owner, via the port's signal bit. `put_msg`
//   raises it, and the program that called `WaitPort` returns.
//
// # Screens and windows
//
// A [`Screen`] owns a layer and a palette; a [`Window`] owns a layer, a
// message port, a list of gadgets and a title. Neither owns the other's
// memory, and both are owned by the [`Intuition`] service as tables of
// records — so a handle is an index into a table, never a pointer, which is
// what makes them safe to hand to user space.
//
// # Gadgets
//
// A gadget is a rectangle plus a kind plus an id. Hit-testing them is
// front-to-back, exactly like layers: a gadget drawn over another one owns
// the click. Only the kinds v0.1.0 needs are here — a button that clicks, a
// string gadget, and the window's own close box — because a gadget's job is
// to turn a rectangle into an `IDCMP_GADGET` message, and every other kind
// does the same thing with a different picture drawn inside it.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use spin::Mutex;

use vela_abi_native64::{
    buttons as abi_buttons, mouse_codes, IntuiMessage, LibraryHandle, Message,
};
use vela_kernel_exec::port::PortId;
use vela_kernel_exec::task::TaskId;
use vela_kernel_exec::Exec;
use vela_kernel_input::{Event, KeyCode, MouseEvent};
use vela_kernel_layers::{LayerId, LayerKind, LayerStack};
use vela_renderer2d::{Color, Rect};
use vela_theme_classic as theme;

/// A handle to a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScreenId(pub u32);

impl ScreenId {
    /// The id of "no screen".
    pub const NONE: ScreenId = ScreenId(0);

    /// True when this id names no screen.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// A handle to a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(pub u32);

impl WindowId {
    /// The id of "no window".
    pub const NONE: WindowId = WindowId(0);

    /// True when this id names no window.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// A handle to a gadget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GadgetId(pub u32);

impl GadgetId {
    /// The id of "no gadget".
    pub const NONE: GadgetId = GadgetId(0);

    /// True when this id names no gadget.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// The errors Intuition returns.
pub mod errors {
    /// The call succeeded.
    pub const OK: i32 = 0;
    /// No screen of that handle exists.
    pub const NO_OBJECT: i32 = -2;
    /// The object is already open.
    pub const OBJECT_IN_USE: i32 = -3;
    /// The object was not found.
    pub const NOT_FOUND: i32 = -6;
    /// The geometry is invalid.
    pub const BAD_RECT: i32 = -13;
    /// The window has no message port, so it cannot be given events.
    pub const NO_PORT: i32 = -24;
    /// A memory allocation failed.
    pub const NO_MEMORY: i32 = -12;
}

/// The window flags a program may set.
///
/// Re-exported from [`vela_abi_native64`] rather than declared here, because
/// the SDK needs the *same* bit assignments and a flag is a bit assignment:
/// two copies cannot disagree loudly, they just quietly give a program a
/// window with the wrong furniture. See [`vela_abi_native64::window_flags`].
pub use vela_abi_native64::window_flags;

/// The IDCMP classes, re-exported so a program and the kernel agree.
///
/// The values are the AmigaOS ones, defined once in `vela_abi_native64` and
/// re-exported rather than restated: two copies of `IDCMP_MOUSEMOVE` that
/// disagree is a bug that only shows up as a program ignoring the wrong
/// events.
pub mod idcmp {
    /// No events.
    pub const NONE: u16 = 0;
    /// Mouse button events and movement.
    pub const MOUSEBUTTONS: u16 = 1 << 0;
    /// Mouse movement.
    pub const MOUSEMOVE: u16 = 1 << 1;
    /// Keyboard events.
    pub const KEYBOARD: u16 = 1 << 8;
    /// Gadget events.
    pub const GADGET: u16 = 1 << 9;
    /// Intuition requests: close, change, menu.
    pub const REQUEST: u16 = 1 << 15;
    /// Mouse events and movement together.
    pub const MOUSE: u16 = MOUSEBUTTONS | MOUSEMOVE;
    /// The default set for a window that asks for mouse input.
    pub const MOUSEMOVE_BTBUTTONS: u16 = MOUSE;
    /// Everything.
    pub const ALL: u16 = MOUSEBUTTONS | MOUSEMOVE | KEYBOARD | GADGET | REQUEST;
}

/// The `RECIDCMP` codes, as the AmigaOS values.
///
/// The close box is the only one v0.1.0 produces: menu selection and window
/// changes come with the desktop in Phase 7, and inventing codes for them now
/// would mean fixing them later.
pub mod request_codes {
    /// The window's close gadget was clicked.
    pub const CLOSE: u16 = 0;
}

/// A gadget: a rectangle that turns a click into an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gadget {
    /// The handle a program knows this gadget by.
    pub id: GadgetId,
    /// What kind of gadget it is.
    pub kind: GadgetKind,
    /// The gadget's rectangle, in the window's coordinates.
    pub bounds: Rect,
    /// The gadget's label, for a string gadget.
    pub label: String,
    /// Whether the gadget responds at all.
    pub enabled: bool,
    /// Whether the gadget is currently held down.
    pub selected: bool,
}

/// The kinds of gadget v0.1.0 has.
///
/// Every kind produces an `IDCMP_GADGET` message when clicked; they differ in
/// what is drawn and in whether the click toggles anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GadgetKind {
    /// A button that is pressed and released.
    Button,
    /// A label. Clicking it still produces an event.
    Label,
    /// The window's close box.
    Close,
    /// A boolean that toggles on click.
    Toggle,
    /// A custom-drawn gadget, identified only by its id.
    Custom,
}

/// A screen: a palette, a layer, and a title.
#[derive(Debug, Clone)]
pub struct Screen {
    /// The handle.
    pub id: ScreenId,
    /// The screen's title.
    pub title: String,
    /// The screen's geometry.
    pub bounds: Rect,
    /// The layer the screen composites onto.
    pub layer: LayerId,
    /// The palette the screen's pens resolve against.
    pub palette: u16,
    /// Whether this is the screen that receives input.
    pub public: bool,
}

/// A window: a layer, a message port, gadgets and a title.
#[derive(Debug, Clone)]
pub struct Window {
    /// The handle.
    pub id: WindowId,
    /// The screen the window is on.
    pub screen: ScreenId,
    /// The layer the window draws into.
    pub layer: LayerId,
    /// The window's title.
    pub title: String,
    /// The window's geometry, in screen coordinates.
    pub bounds: Rect,
    /// The window's flags, from [`window_flags`].
    pub flags: u32,
    /// Which IDCMP classes the window wants.
    pub idcmp: u16,
    /// The message port events go to.
    pub port: PortId,
    /// The task that opened the window.
    pub owner: TaskId,
    /// The window's gadgets, in z-order: later ones are in front.
    pub gadgets: Vec<Gadget>,
    /// Whether the window is open.
    pub open: bool,
}

impl Window {
    /// True when the window wants events of class `class`.
    pub const fn wants(&self, class: u16) -> bool {
        self.idcmp & class != 0
    }

    /// True when the window takes input at all.
    pub const fn interactive(&self) -> bool {
        self.idcmp != idcmp::NONE
    }

    /// The close gadget's rectangle, if the window has one.
    pub fn close_gadget_rect(&self) -> Option<Rect> {
        if self.flags & window_flags::CLOSEGADGET == 0 {
            return None;
        }
        let size = theme::metrics::CLOSE_GADGET_SIZE;
        Some(Rect::new(
            self.bounds.x + theme::metrics::BORDER as i32,
            self.bounds.y + theme::metrics::BORDER as i32,
            size,
            size,
        ))
    }
}

/// The Intuition service.
///
/// Owns the screens, the windows and the gadget ids. The layer geometry lives
/// in [`LayerStack`]; the pixel buffers live in the layers themselves. So the
/// three collaborate rather than nest: Intuition decides *what* is where, the
/// stack decides *what is on top*, and Exec decides *where the event goes*.
pub struct Intuition {
    screens: Mutex<Vec<Screen>>,
    windows: Mutex<Vec<Window>>,
    gadgets: Mutex<Vec<(WindowId, Gadget)>>,
    next_screen: Mutex<u32>,
    next_window: Mutex<u32>,
    next_gadget: Mutex<u32>,
    layers: LayerStack,
    /// The window that has keyboard focus.
    active: Mutex<WindowId>,
}

impl core::fmt::Debug for Intuition {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Intuition")
            .field("screens", &self.screens.lock().len())
            .field("windows", &self.windows.lock().len())
            .field("active", &self.active.lock().0)
            .finish()
    }
}

impl Intuition {
    /// A service over a screen of `width` by `height`.
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            screens: Mutex::new(Vec::new()),
            windows: Mutex::new(Vec::new()),
            gadgets: Mutex::new(Vec::new()),
            next_screen: Mutex::new(1),
            next_window: Mutex::new(1),
            next_gadget: Mutex::new(1),
            layers: LayerStack::new(width, height),
            active: Mutex::new(WindowId::NONE),
        }
    }

    /// The layer stack the windows live in.
    pub fn layers(&self) -> &LayerStack {
        &self.layers
    }

    /// The screen's geometry.
    pub fn screen_rect(&self) -> Rect {
        self.layers.screen()
    }

    // --- Screens -----------------------------------------------------------

    /// `OpenScreen` — open a screen.
    ///
    /// The screen layer goes to the back of the stack: it is the backdrop
    /// every window composites onto, and a window opened before any screen
    /// exists still has to have something behind it.
    pub fn open_screen(&self, title: &str, bounds: Rect, palette: u16) -> Result<ScreenId, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let id = {
            let mut next = self.next_screen.lock();
            let id = ScreenId(*next);
            *next += 1;
            id
        };
        let layer = self.layers.create_layer(LayerKind::Screen, bounds)?;
        self.layers.move_layer(layer, bounds.x, bounds.y)?;
        {
            let mut screens = self.screens.lock();
            screens.push(Screen {
                id,
                title: String::from(title),
                bounds,
                layer,
                palette,
                public: true,
            });
        }
        Ok(id)
    }

    /// `CloseScreen` — close a screen and every window on it.
    ///
    /// Closing a screen must close its windows: their layers live on the
    /// screen, and leaving a window open on a screen that is gone is a handle
    /// that names nothing.
    pub fn close_screen(&self, exec: &Exec, screen: ScreenId) -> Result<usize, i32> {
        let record = {
            let mut screens = self.screens.lock();
            let index = screens
                .iter()
                .position(|s| s.id == screen)
                .ok_or(errors::NO_OBJECT)?;
            screens.remove(index)
        };
        let mut closed = 0;
        loop {
            let victim = self
                .windows
                .lock()
                .iter()
                .find(|w| w.screen == screen && w.open)
                .map(|w| w.id);
            match victim {
                Some(id) => {
                    self.close_window(exec, id)?;
                    closed += 1;
                }
                None => break,
            }
        }
        let _ = self.layers.destroy_layer(record.layer);
        Ok(closed)
    }

    /// A snapshot of a screen.
    pub fn screen(&self, screen: ScreenId) -> Option<Screen> {
        self.screens.lock().iter().find(|s| s.id == screen).cloned()
    }

    /// How many screens are open.
    pub fn screen_count(&self) -> usize {
        self.screens.lock().len()
    }

    // --- Windows -----------------------------------------------------------

    /// `OpenWindow` — open a window.
    ///
    /// `port` is the message port events go to. A window with
    /// [`PortId::NONE`] is a *drawing-only* window: it still composites, but
    /// no event can be delivered to it, because there is nowhere to deliver
    /// to. That is not an error — it is how a splash screen is drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn open_window(
        &self,
        screen: ScreenId,
        title: &str,
        bounds: Rect,
        flags: u32,
        idcmp: u16,
        port: PortId,
        owner: TaskId,
    ) -> Result<WindowId, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        if self.screen(screen).is_none() {
            return Err(errors::NO_OBJECT);
        }
        if idcmp != idcmp::NONE && port.is_none() {
            // Asking for events with nowhere to put them is a programming
            // error, and silently dropping the events would make the program
            // hang waiting for something that can never arrive.
            return Err(errors::NO_PORT);
        }
        let id = {
            let mut next = self.next_window.lock();
            let id = WindowId(*next);
            *next += 1;
            id
        };
        let layer = self
            .layers
            .create_buffered_layer(LayerKind::Window, bounds, 0)?;
        self.layers.move_layer(layer, bounds.x, bounds.y)?;
        {
            let mut windows = self.windows.lock();
            windows.push(Window {
                id,
                screen,
                layer,
                title: String::from(title),
                bounds,
                flags,
                idcmp,
                port,
                owner,
                gadgets: Vec::new(),
                open: true,
            });
        }
        if flags & window_flags::ACTIVE != 0 {
            self.set_active(id);
        }
        Ok(id)
    }

    /// `CloseWindow` — close a window.
    ///
    /// The window's message port is *not* deleted here: the port belongs to
    /// the program's own Exec setup, and a window that closed while another
    /// window still names the same port would otherwise take it down with it.
    pub fn close_window(&self, exec: &Exec, window: WindowId) -> Result<(), i32> {
        let record = {
            let mut windows = self.windows.lock();
            let index = windows
                .iter()
                .position(|w| w.id == window)
                .ok_or(errors::NO_OBJECT)?;
            windows.remove(index)
        };
        self.gadgets.lock().retain(|(w, _)| *w != window);
        let _ = self.layers.destroy_layer(record.layer);
        if self.active.lock().0 == window.0 {
            // The keyboard focus cannot stay on a window that no longer
            // exists: the next key would go to a dead handle.
            let next = self
                .windows
                .lock()
                .iter()
                .rev()
                .find(|w| w.flags & window_flags::ACTIVE != 0)
                .map(|w| w.id)
                .unwrap_or(WindowId::NONE);
            *self.active.lock() = next;
        }
        // Queued events for a window that no longer exists are discarded.
        //
        // This has to happen here rather than at port teardown, because the
        // port is the *program's* — it outlives the window, and it may be
        // shared with another window the program still has open. A program
        // that closed a window and then read its port would otherwise find
        // messages naming `im_Source` values that resolve to nothing, and the
        // sensible thing to do with such a message — ignore it — is exactly
        // what this discard does on its behalf.
        discard_events_for(exec, record.port, window.0 as u64);
        Ok(())
    }

    /// A snapshot of a window.
    pub fn window(&self, window: WindowId) -> Option<Window> {
        self.windows.lock().iter().find(|w| w.id == window).cloned()
    }

    /// How many windows are open.
    pub fn window_count(&self) -> usize {
        self.windows.lock().len()
    }

    /// The windows, front to back.
    pub fn windows_front_to_back(&self) -> Vec<WindowId> {
        self.layers
            .order()
            .into_iter()
            .rev()
            .filter_map(|layer| {
                self.windows
                    .lock()
                    .iter()
                    .find(|w| w.layer == layer)
                    .map(|w| w.id)
            })
            .collect()
    }

    /// Set a window's title.
    pub fn set_title(&self, window: WindowId, title: &str) -> Result<(), i32> {
        let mut windows = self.windows.lock();
        let w = windows
            .iter_mut()
            .find(|w| w.id == window)
            .ok_or(errors::NO_OBJECT)?;
        w.title = String::from(title);
        self.layers.invalidate(w.layer)?;
        Ok(())
    }

    /// Set a window's geometry, moving and resizing its layer.
    ///
    /// The old rectangle is returned so the caller can repaint what the window
    /// used to cover — otherwise the old position stays on screen as a ghost.
    pub fn set_window_bounds(&self, window: WindowId, bounds: Rect) -> Result<Rect, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let layer = {
            let mut windows = self.windows.lock();
            let w = windows
                .iter_mut()
                .find(|w| w.id == window)
                .ok_or(errors::NO_OBJECT)?;
            w.bounds = bounds;
            w.layer
        };
        self.layers.resize_layer(layer, bounds)?;
        self.layers.move_layer(layer, bounds.x, bounds.y)?;
        Ok(bounds)
    }

    /// Set a window's flags and IDCMP mask.
    ///
    /// Narrowing `idcmp` stops events immediately: a program that wants to
    /// stop receiving mouse movement does so by clearing the bit, and the
    /// *next* event is already refused.
    pub fn set_attrs(&self, window: WindowId, flags: u32, idcmp: u16) -> Result<(), i32> {
        if idcmp != idcmp::NONE {
            let has_port = {
                let windows = self.windows.lock();
                windows
                    .iter()
                    .find(|w| w.id == window)
                    .map(|w| !w.port.is_none())
                    .unwrap_or(false)
            };
            if !has_port {
                return Err(errors::NO_PORT);
            }
        }
        let layer = {
            let mut windows = self.windows.lock();
            let w = windows
                .iter_mut()
                .find(|w| w.id == window)
                .ok_or(errors::NO_OBJECT)?;
            w.flags = flags;
            w.idcmp = idcmp;
            w.layer
        };
        self.layers.invalidate(layer)?;
        if flags & window_flags::ACTIVE != 0 {
            self.set_active(window);
        }
        Ok(())
    }

    /// Make a window the keyboard-focus window.
    pub fn set_active(&self, window: WindowId) {
        if self.window(window).is_none() {
            return;
        }
        *self.active.lock() = window;
    }

    /// The window with keyboard focus.
    pub fn active_window(&self) -> WindowId {
        *self.active.lock()
    }

    // --- Gadgets -----------------------------------------------------------

    /// Add a gadget to a window.
    pub fn add_gadget(
        &self,
        window: WindowId,
        kind: GadgetKind,
        bounds: Rect,
        label: &str,
    ) -> Result<GadgetId, i32> {
        if bounds.width == 0 || bounds.height == 0 {
            return Err(errors::BAD_RECT);
        }
        let id = {
            let mut next = self.next_gadget.lock();
            let id = GadgetId(*next);
            *next += 1;
            id
        };
        let gadget = Gadget {
            id,
            kind,
            bounds,
            label: String::from(label),
            enabled: true,
            selected: false,
        };
        {
            let mut windows = self.windows.lock();
            let w = windows
                .iter_mut()
                .find(|w| w.id == window)
                .ok_or(errors::NO_OBJECT)?;
            w.gadgets.push(gadget.clone());
        }
        self.gadgets.lock().push((window, gadget));
        let _ = self.windows.lock();
        if let Some(layer) = self.window(window).map(|w| w.layer) {
            self.layers.invalidate(layer)?;
        }
        Ok(id)
    }

    /// Remove a gadget from its window.
    pub fn remove_gadget(&self, gadget: GadgetId) -> Result<(), i32> {
        let mut gadgets = self.gadgets.lock();
        let window = gadgets
            .iter()
            .find(|(_, g)| g.id == gadget)
            .map(|(w, _)| *w)
            .ok_or(errors::NO_OBJECT)?;
        gadgets.retain(|(_, g)| g.id != gadget);
        drop(gadgets);
        let mut windows = self.windows.lock();
        if let Some(w) = windows.iter_mut().find(|w| w.id == window) {
            w.gadgets.retain(|g| g.id != gadget);
        }
        Ok(())
    }

    /// A snapshot of a gadget.
    pub fn gadget(&self, gadget: GadgetId) -> Option<Gadget> {
        self.gadgets
            .lock()
            .iter()
            .find(|(_, g)| g.id == gadget)
            .map(|(_, g)| g.clone())
    }

    /// The topmost gadget of `window` containing the window-local point.
    ///
    /// Front-to-back, like layers: a gadget drawn over another owns the
    /// click. A disabled gadget is skipped, which is how a program greys out a
    /// button without having to remove it.
    pub fn gadget_at(&self, window: WindowId, local_x: i32, local_y: i32) -> Option<Gadget> {
        let windows = self.windows.lock();
        let w = windows.iter().find(|w| w.id == window)?;
        w.gadgets
            .iter()
            .rev()
            .find(|g| g.enabled && g.bounds.contains(local_x, local_y))
            .cloned()
    }

    /// Set a gadget's enabled state.
    pub fn set_gadget_enabled(&self, gadget: GadgetId, enabled: bool) -> Result<(), i32> {
        {
            let mut gadgets = self.gadgets.lock();
            let entry = gadgets
                .iter_mut()
                .find(|(_, g)| g.id == gadget)
                .ok_or(errors::NO_OBJECT)?;
            entry.1.enabled = enabled;
        }
        let mut windows = self.windows.lock();
        if let Some(w) = windows
            .iter_mut()
            .find(|w| w.gadgets.iter().any(|g| g.id == gadget))
        {
            if let Some(g) = w.gadgets.iter_mut().find(|g| g.id == gadget) {
                g.enabled = enabled;
            }
        }
        Ok(())
    }

    // --- Event routing -----------------------------------------------------

    /// The window whose layer is topmost at the screen point.
    fn window_at_screen(&self, x: i32, y: i32) -> Option<Window> {
        let layer = self.layers.topmost_at(x, y)?;
        let windows = self.windows.lock();
        windows.iter().find(|w| w.layer == layer).cloned()
    }

    /// Route one mouse event.
    ///
    /// This is where the report's "IDCMP lands on an Exec port" requirement is
    /// actually met: the event is *put on a message port*, not returned to a
    /// caller. There is no other path for a mouse event to take.
    ///
    /// Returns the window that received it, if any.
    pub fn dispatch_mouse(&self, exec: &Exec, event: &MouseEvent) -> Option<WindowId> {
        let (x, y) = (event.x, event.y);
        let window = self.window_at_screen(x, y)?;
        let layer = self.layers.layer(window.layer)?;
        let (lx, ly) = layer.to_local(x, y)?;

        // The close gadget is checked before the registered gadgets: it is
        // drawn on top of the title bar, so a gadget at the same coordinates
        // must lose the click to it.
        if self.close_gadget_at(window.id, x, y) && window.wants(idcmp::REQUEST) {
            self.deliver(
                exec,
                &window,
                IntuiMessage {
                    im_Type: IntuiMessage::REQUEST,
                    im_Code: request_codes::CLOSE,
                    im_Source: window.id.0 as u64,
                    im_MouseX: lx as i16,
                    im_MouseY: ly as i16,
                    im_Button: button_mask(event.buttons),
                    im_Flags: 0,
                    im_sig: 0,
                },
            );
            return Some(window.id);
        }

        // A gadget under the point wins over the plain mouse event, because a
        // gadget that is not told about the click will never fire.
        if window.wants(idcmp::GADGET) {
            if let Some(gadget) = self.gadget_at(window.id, lx, ly) {
                self.deliver(
                    exec,
                    &window,
                    IntuiMessage {
                        im_Type: IntuiMessage::GADGET,
                        im_Code: gadget.id.0 as u16,
                        im_Source: window.id.0 as u64,
                        im_MouseX: lx as i16,
                        im_MouseY: ly as i16,
                        im_Button: button_mask(event.buttons),
                        im_Flags: 0,
                        im_sig: 0,
                    },
                );
                if gadget.kind == GadgetKind::Toggle {
                    // A toggle flips on the press, not on the release: the
                    // program is deciding a state, not completing a gesture,
                    // and a toggle that waits for the release feels broken
                    // when the button is held down.
                    //
                    // "Selected" means *on*, not *held*: a toggle that goes
                    // dark when the mouse button comes up looks like it was
                    // never clicked at all.
                    let left_went_down = event.changed & 0x01 != 0 && event.buttons & 0x01 != 0;
                    if left_went_down {
                        self.mark_gadget_selected(gadget.id, !gadget.selected);
                    }
                } else {
                    // A plain button is *held*: selected tracks the buttons
                    // being down, so the release clears it.
                    self.mark_gadget_selected(gadget.id, event.buttons != 0);
                }
                return Some(window.id);
            }
        }

        // Movement and button changes are separate classes, because a program
        // that wants clicks without tracking does not want to be woken on
        // every mouse packet - and one that wants tracking does not want a
        // click message for every pixel the pointer crosses.
        let intui = if event.changed != 0 {
            if !window.wants(idcmp::MOUSEBUTTONS) {
                return None;
            }
            IntuiMessage {
                im_Type: IntuiMessage::MOUSE,
                im_Code: button_code(event.buttons, event.changed),
                im_Source: window.id.0 as u64,
                im_MouseX: lx as i16,
                im_MouseY: ly as i16,
                im_Button: button_mask(event.buttons),
                im_Flags: 0,
                im_sig: 0,
            }
        } else {
            if !window.wants(idcmp::MOUSEMOVE) {
                return None;
            }
            IntuiMessage {
                im_Type: IntuiMessage::MOUSE,
                im_Code: mouse_codes::MOVE,
                im_Source: window.id.0 as u64,
                im_MouseX: lx as i16,
                im_MouseY: ly as i16,
                im_Button: button_mask(event.buttons),
                im_Flags: 0,
                im_sig: 0,
            }
        };
        self.deliver(exec, &window, intui);
        Some(window.id)
    }

    /// Route one keyboard event.
    ///
    /// Keyboard events go to the *active* window, not to the window under the
    /// pointer: there is no pointer in a keyboard event, and the AmigaOS rule
    /// is that the active window has focus.
    pub fn dispatch_key(&self, exec: &Exec, event: &KeyEventLike) -> Option<WindowId> {
        let window = self.window(self.active_window())?;
        if !window.wants(idcmp::KEYBOARD) {
            return None;
        }
        let mut intui =
            IntuiMessage::keyboard(event.code.raw() as u16, if event.released { 1 } else { 0 });
        intui.im_Source = window.id.0 as u64;
        self.deliver(exec, &window, intui);
        Some(window.id)
    }

    /// Route one raw input event.
    ///
    /// The single entry point the kernel's input path calls, so that there is
    /// exactly one place where "which window" and "which port" are decided.
    pub fn dispatch_event(&self, exec: &Exec, event: &Event) -> Option<WindowId> {
        match event {
            Event::Mouse(m) => self.dispatch_mouse(exec, m),
            Event::Key(k) => self.dispatch_key(
                exec,
                &KeyEventLike {
                    code: k.code,
                    released: k.released,
                },
            ),
        }
    }

    /// Pack an `IntuiMessage` and put it on the window's message port.
    ///
    /// `mn_reply_port` is set to the window's port. On AmigaOS an IDCMP
    /// message carries no reply port - the program is not expected to reply
    /// to the window manager - but VelaOS sets it anyway, so that a program
    /// which *does* call `ReplyMsg` (which the report's acceptance path does)
    /// reaches the port it was sent from rather than failing with
    /// `NO_REPLY_PORT`. The window manager never waits for a reply, so nothing
    /// is lost by the program sending one.
    fn deliver(&self, exec: &Exec, window: &Window, intui: IntuiMessage) {
        if window.port.is_none() {
            return;
        }
        let mut m = Message::with_intui(intui);
        m.mn_reply_port = window.port.0 as u64;
        // `put_msg` raises the port owner's message signal, which is what
        // wakes a task blocked in `WaitPort`. If it fails the event is
        // dropped: there is nowhere else it could go, and the report
        // explicitly forbids a private event pipe as a fallback.
        let _ = exec.put_msg(window.port, m);
    }

    fn mark_gadget_selected(&self, gadget: GadgetId, selected: bool) {
        {
            let mut gadgets = self.gadgets.lock();
            if let Some(entry) = gadgets.iter_mut().find(|(_, g)| g.id == gadget) {
                entry.1.selected = selected;
            }
        }
        let mut windows = self.windows.lock();
        for w in windows.iter_mut() {
            if let Some(g) = w.gadgets.iter_mut().find(|g| g.id == gadget) {
                g.selected = selected;
            }
        }
    }

    /// Clear every gadget's selected state.
    ///
    /// A window manager calls this once per event, before routing: a gadget
    /// left selected after the mouse left it would render pressed forever.
    pub fn clear_selection(&self) {
        let mut gadgets = self.gadgets.lock();
        for (_, g) in gadgets.iter_mut() {
            g.selected = false;
        }
        let mut windows = self.windows.lock();
        for w in windows.iter_mut() {
            for g in w.gadgets.iter_mut() {
                g.selected = false;
            }
        }
    }

    /// Whether a window's close gadget is under the screen point.
    pub fn close_gadget_at(&self, window: WindowId, x: i32, y: i32) -> bool {
        self.window(window)
            .and_then(|w| w.close_gadget_rect())
            .map(|r| r.contains(x, y))
            .unwrap_or(false)
    }

    /// Repaint a window's layer through the theme.
    ///
    /// This is the *system* drawing, not a program's: a window that has just
    /// been opened shows its border, title bar and gadgets before the program
    /// has drawn anything, because a window that appears blank looks broken.
    /// A program that wants to draw over it just draws over it — its layer is
    /// above the system's.
    pub fn repaint_window(&self, window: WindowId) -> Result<(), i32> {
        let w = self.window(window).ok_or(errors::NO_OBJECT)?;
        let layer = w.layer;
        self.layers.with_layer(layer, |l| {
            let pack = LayerStack::pack;
            let bounds = l.bounds;
            l.clear(pack(theme::PANEL_BG));
            // The border.
            for i in 0..theme::metrics::BORDER {
                let top = i as i32;
                l.put(top, top, pack(theme::PANEL_BORDER));
                l.put(
                    top,
                    bounds.height as i32 - 1 - top,
                    pack(theme::PANEL_BORDER),
                );
                l.put(
                    bounds.width as i32 - 1 - top,
                    top,
                    pack(theme::PANEL_BORDER),
                );
                l.put(
                    bounds.width as i32 - 1 - top,
                    bounds.height as i32 - 1 - top,
                    pack(theme::PANEL_BORDER),
                );
            }
            // The title bar.
            let bar_h = theme::metrics::TITLEBAR_HEIGHT;
            let active = self.active.lock().0 == window.0;
            let bar_color = if active {
                theme::TITLEBAR_ACTIVE_BG
            } else {
                theme::TITLEBAR_BG
            };
            let bar = Rect::new(
                theme::metrics::BORDER as i32,
                theme::metrics::BORDER as i32,
                bounds.width.saturating_sub(2 * theme::metrics::BORDER),
                bar_h.min(bounds.height.saturating_sub(2 * theme::metrics::BORDER)),
            );
            for y in 0..bar.height {
                for x in 0..bar.width {
                    l.put(bar.x + x as i32, bar.y + y as i32, pack(bar_color));
                }
            }
            // The title, centred.
            let text_y = bar.y + ((bar.height as i32) - 16) / 2;
            self.draw_string_in_layer(l, &w.title, bar.x + 8, text_y, pack(theme::TEXT_FG));
            // The close gadget.
            if let Some(close) = w.close_gadget_rect() {
                let lx = close.x - w.bounds.x;
                let ly = close.y - w.bounds.y;
                for y in 0..close.height {
                    for x in 0..close.width {
                        l.put(lx + x as i32, ly + y as i32, pack(theme::CLOSE_GADGET));
                    }
                }
            }
        })?;
        self.layers.invalidate(layer)?;
        Ok(())
    }

    /// Draw a string into a layer's buffer at layer-local coordinates.
    ///
    /// The font is the original 8x16 bitmap in `vela-text`; this walks its
    /// rows directly into the layer's `u32` buffer rather than going through
    /// the framebuffer renderer, because a layer is not a framebuffer — it has
    /// no stride padding and no pixel-format byte order.
    pub fn draw_string_in_layer(
        &self,
        layer: &mut vela_kernel_layers::Layer,
        text: &str,
        x: i32,
        y: i32,
        color: u32,
    ) {
        let mut cx = x;
        for ch in text.chars() {
            let glyph = vela_text::glyph(ch);
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..vela_text::GLYPH_WIDTH as u8 {
                    if bits & (1 << (vela_text::GLYPH_WIDTH as u8 - 1 - col)) != 0 {
                        layer.put(cx + col as i32, y + row as i32, color);
                    }
                }
            }
            cx += vela_text::GLYPH_WIDTH as i32;
        }
    }

    /// Repaint every open window.
    pub fn repaint_all(&self) {
        let ids: Vec<WindowId> = self.windows.lock().iter().map(|w| w.id).collect();
        for id in ids {
            let _ = self.repaint_window(id);
        }
    }

    /// The layer stack, for the compositor.
    pub fn layer_stack(&self) -> &LayerStack {
        &self.layers
    }

    /// Composite the stack into the display's framebuffer.
    ///
    /// The framebuffer is *the kernel's*, supplied at boot by the display
    /// driver. A program cannot name it, cannot write to it directly, and
    /// cannot substitute a buffer of its own: compositing is the window
    /// manager's job, and the only thing a program can ask for is "refresh".
    ///
    /// Returns the rectangle that was touched, or `None` when nothing was
    /// dirty.
    ///
    /// # Safety of the framebuffer access
    ///
    /// This is the one place in the crate that touches the framebuffer, and it
    /// does so through a raw pointer the kernel installed. The pointer comes
    /// from the boot handoff rather than from a user pointer, and the geometry
    /// was validated when the framebuffer was mapped.
    pub fn composite_to_framebuffer(&self) -> Option<Rect> {
        let fb = framebuffer();
        if fb.address == 0 || fb.width == 0 || fb.height == 0 {
            return None;
        }
        let stride_bytes = fb.stride_px * 4;
        // Checked: a hostile or merely wrong stride must not make the slice
        // length wrap, because a wrapped length is a slice the compiler will
        // happily hand to `composite` and it will happily write through.
        let len = (stride_bytes as usize).checked_mul(fb.height as usize)?;
        // SAFETY: the framebuffer is mapped for the whole of the kernel's life
        // and its geometry is the boot handoff's, which the display driver
        // validated. `len` is the stride times the height, which is the
        // framebuffer's own length, and the address is the kernel's — never a
        // user pointer.
        unsafe {
            let base = fb.address as *mut u32;
            let slice = core::slice::from_raw_parts_mut(base, len);
            self.layers.composite(slice, fb.stride_px).ok()
        }
    }
}

/// Drop every queued event on `port` whose `im_Source` is `source`.
///
/// # Why this drains and re-enqueues rather than removing in place
///
/// A port's queue is FIFO with no interior removal, so the only way to drop a
/// message from the middle is to take everything out and put back what
/// survives. The survivors go back in the order they came out, so the
/// remaining events are in the order a program would have seen them had the
/// discard not happened — which matters because a program that closes one
/// window while another window's events are still arriving must not see those
/// events reordered.
///
/// A message that is *not* an Intuition event — a plain `Message` from
/// `SendMsg`, say — has `im_Source == 0` and is kept: it was not this
/// window's, so closing this window does not license discarding it.
///
/// # Why the length is read once
///
/// The drain is bounded by the queue's length as of the call. That is not a
/// fudge: nothing else can enqueue onto this port while the caller holds the
/// Exec service's lock — which `Intuition::close_window` does, because it is
/// called from the syscall dispatcher, which serialises. A bounded loop is
/// still written rather than looping until `get_msg` fails, so that a future
/// caller without that lock fails visibly instead of spinning.
fn discard_events_for(exec: &Exec, port: PortId, source: u64) {
    if port.is_none() || source == 0 {
        return;
    }
    let mut remaining = exec.ports_queue_len(port);
    if remaining == 0 {
        return;
    }
    let mut keep: Vec<Message> = Vec::with_capacity(remaining);
    while remaining > 0 {
        let Ok(message) = exec.get_msg(port) else {
            break;
        };
        remaining -= 1;
        if message.as_intui().im_Source != source {
            keep.push(message);
        }
    }
    for message in keep {
        let _ = exec.put_msg(port, message);
    }
}

/// The framebuffer the kernel composites onto.
///
/// Installed once at boot from the `VelaBootInfo` handoff. Held as a `static`
/// because every compositing path needs it and threading it through the
/// window manager, the layer stack and every syscall handler would make it a
/// parameter of half the crate.
static FRAMEBUFFER: spin::Mutex<Option<FramebufferInfo>> = spin::Mutex::new(None);

/// Where the display's pixels are, and how they are addressed.
#[derive(Debug, Clone, Copy)]
pub struct FramebufferInfo {
    /// The framebuffer's virtual address.
    pub address: u64,
    /// Its width in pixels.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// Its stride in **pixels**, not bytes.
    pub stride_px: u32,
}

/// Install the framebuffer geometry.
///
/// # Safety
///
/// Must be called once during boot, before any compositing, with values the
/// display driver validated. The address must remain mapped for the rest of
/// the kernel's life.
pub unsafe fn set_framebuffer(address: u64, width: u32, height: u32, stride_bytes: u32) {
    let mut fb = FRAMEBUFFER.lock();
    *fb = Some(FramebufferInfo {
        address,
        width,
        height,
        stride_px: if stride_bytes == 0 {
            width
        } else {
            stride_bytes / 4
        },
    });
}

/// True when a framebuffer has been installed.
pub fn has_framebuffer() -> bool {
    FRAMEBUFFER.lock().is_some()
}

/// A copy of the installed framebuffer geometry, or zeroes when there is none.
///
/// The zeroes are deliberate rather than a `panic`: the hosted configuration
/// has no framebuffer, and every caller of this already handles the
/// "no display" case by not drawing.
fn framebuffer() -> FramebufferInfo {
    match *FRAMEBUFFER.lock() {
        Some(fb) => fb,
        None => FramebufferInfo {
            address: 0,
            width: 0,
            height: 0,
            stride_px: 0,
        },
    }
}

/// The keyboard event shape Intuition needs.
///
/// Deliberately a two-field struct rather than a reuse of
/// `vela_kernel_input::KeyEvent`: the input crate's event also carries the
/// modifier state, and Intuition must not be able to see it — a window
/// manager that could see shift would be able to synthesise a shift-click,
/// and that is not something the input layer should be handing out.
#[derive(Debug, Clone, Copy)]
pub struct KeyEventLike {
    /// The key.
    pub code: KeyCode,
    /// Whether the key came up.
    pub released: bool,
}

/// The AmigaOS mouse code for a button transition.
///
/// The *transition* is what matters, not the state: a click that starts with
/// both buttons down must report the one that changed, and reporting the whole
/// mask would say "left and right went down" for a left click.
fn button_code(buttons: u8, changed: u8) -> u16 {
    if changed & 0x01 != 0 {
        if buttons & 0x01 != 0 {
            mouse_codes::LEFT_DOWN
        } else {
            mouse_codes::LEFT_UP
        }
    } else if changed & 0x02 != 0 {
        if buttons & 0x02 != 0 {
            mouse_codes::RIGHT_DOWN
        } else {
            mouse_codes::RIGHT_UP
        }
    } else if changed & 0x04 != 0 {
        if buttons & 0x04 != 0 {
            mouse_codes::MIDDLE_DOWN
        } else {
            mouse_codes::MIDDLE_UP
        }
    } else {
        mouse_codes::MOVE
    }
}

/// The AmigaOS button mask, as a program sees it.
pub fn button_mask(buttons: u8) -> u16 {
    let mut out = 0u16;
    if buttons & 0x01 != 0 {
        out |= abi_buttons::LEFT;
    }
    if buttons & 0x02 != 0 {
        out |= abi_buttons::RIGHT;
    }
    if buttons & 0x04 != 0 {
        out |= abi_buttons::MIDDLE;
    }
    out
}

/// The library handle a program gets for `intuition.library`.
pub fn intuition_library_handle() -> LibraryHandle {
    LibraryHandle::for_library(vela_abi_native64::LibraryId::Intuition)
}

/// A convenience alias for the colour type the theme uses.
pub type ThemeColor = Color;
#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use vela_kernel_input::Mouse;

    /// A window setup: service, Exec, and a program-owned port.
    struct Harness {
        exec: Exec,
        intui: Intuition,
        task: TaskId,
    }

    impl Harness {
        fn new() -> Self {
            let exec = Exec::with_builtins();
            let task = exec.create_task();
            exec.set_current_task(task);
            let intui = Intuition::new(640, 480);
            intui
                .open_screen("Workbench Screen", Rect::new(0, 0, 640, 480), 0)
                .unwrap();
            Self { exec, intui, task }
        }

        /// Open a window that takes mouse and gadget events.
        fn open(&self, title: &str, bounds: Rect, idcmp: u16) -> (WindowId, PortId) {
            let port = self.exec.create_port(self.task, 1 << 0, 1 << 0, 0);
            let window = self
                .intui
                .open_window(
                    self.intui
                        .windows_front_to_back()
                        .first()
                        .map(|_| ScreenId(1))
                        .unwrap_or(ScreenId(1)),
                    title,
                    bounds,
                    window_flags::ACTIVE | window_flags::CLOSEGADGET,
                    idcmp,
                    port,
                    self.task,
                )
                .unwrap();
            (window, port)
        }
    }

    /// A mouse event with the given position and button mask.
    fn mouse_at(x: i32, y: i32, buttons: u8, changed: u8) -> MouseEvent {
        MouseEvent {
            x,
            y,
            buttons,
            changed,
        }
    }

    // --- Screens ------------------------------------------------------------

    #[test]
    fn opening_a_screen_creates_a_layer() {
        let h = Harness::new();
        assert_eq!(h.intui.screen_count(), 1);
        let screen = h.intui.screen(ScreenId(1)).unwrap();
        assert_eq!(screen.title, "Workbench Screen");
        assert!(h.intui.layers().exists(screen.layer));
    }

    #[test]
    fn a_zero_sized_screen_is_refused() {
        let intui = Intuition::new(64, 64);
        assert_eq!(
            intui
                .open_screen("bad", Rect::new(0, 0, 0, 10), 0)
                .unwrap_err(),
            errors::BAD_RECT
        );
    }

    #[test]
    fn closing_a_screen_closes_its_windows() {
        // A window on a screen that is gone is a handle that names nothing,
        // so closing the screen has to take its windows with it.
        let h = Harness::new();
        let (window, _) = h.open("w", Rect::new(10, 10, 100, 50), idcmp::MOUSE);
        assert_eq!(h.intui.window_count(), 1);
        let closed = h.intui.close_screen(&h.exec, ScreenId(1)).unwrap();
        assert_eq!(closed, 1);
        assert_eq!(h.intui.window_count(), 0);
        assert!(h.intui.window(window).is_none());
    }

    #[test]
    fn closing_an_unknown_screen_is_an_error() {
        let h = Harness::new();
        assert!(h.intui.close_screen(&h.exec, ScreenId(99)).is_err());
    }

    // --- Windows ------------------------------------------------------------

    #[test]
    fn opening_a_window_creates_a_layer_on_the_screen() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(10, 20, 200, 100), idcmp::MOUSE);
        let w = h.intui.window(window).unwrap();
        assert_eq!(w.title, "Test");
        assert_eq!(w.bounds, Rect::new(10, 20, 200, 100));
        assert_eq!(w.port, port);
        assert!(w.open);
        assert!(h.intui.layers().exists(w.layer));
    }

    #[test]
    fn asking_for_events_with_no_port_is_refused() {
        // Silently dropping the events would leave the program waiting for
        // something that can never arrive, which is worse than an error.
        let h = Harness::new();
        let err = h
            .intui
            .open_window(
                ScreenId(1),
                "no port",
                Rect::new(0, 0, 100, 100),
                0,
                idcmp::MOUSE,
                PortId::NONE,
                h.task,
            )
            .unwrap_err();
        assert_eq!(err, errors::NO_PORT);
    }

    #[test]
    fn a_drawing_only_window_needs_no_port() {
        let h = Harness::new();
        let w = h
            .intui
            .open_window(
                ScreenId(1),
                "splash",
                Rect::new(0, 0, 100, 100),
                0,
                idcmp::NONE,
                PortId::NONE,
                h.task,
            )
            .unwrap();
        assert!(!h.intui.window(w).unwrap().interactive());
    }

    #[test]
    fn opening_a_window_on_an_unknown_screen_is_refused() {
        let h = Harness::new();
        let port = h.exec.create_port(h.task, 1, 1, 0);
        assert!(h
            .intui
            .open_window(
                ScreenId(99),
                "x",
                Rect::new(0, 0, 10, 10),
                0,
                idcmp::NONE,
                port,
                h.task
            )
            .is_err());
    }

    #[test]
    fn closing_a_window_removes_its_layer() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(10, 10, 100, 50), idcmp::MOUSE);
        let layer = h.intui.window(window).unwrap().layer;
        h.intui.close_window(&h.exec, window).unwrap();
        assert!(!h.intui.layers().exists(layer));
        assert!(h.intui.window(window).is_none());
        assert!(h.intui.close_window(&h.exec, window).is_err());
    }

    #[test]
    fn closing_a_window_discards_its_queued_events() {
        // A program that closes a window must not then read its port and find
        // events naming `im_Source` values that resolve to nothing.
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(10, 10, 100, 50), idcmp::MOUSE);
        for i in 0..3 {
            h.intui
                .dispatch_mouse(
                    &h.exec,
                    &MouseEvent {
                        x: 20 + i,
                        y: 20,
                        buttons: 0,
                        changed: 0,
                    },
                )
                .expect("the click must land");
        }
        let port = h.intui.window(window).unwrap().port;
        assert_eq!(h.exec.ports_queue_len(port), 3);

        h.intui.close_window(&h.exec, window).unwrap();
        assert_eq!(
            h.exec.ports_queue_len(port),
            0,
            "closing a window must discard its pending events",
        );
    }

    #[test]
    fn closing_one_window_leaves_another_windows_events_in_order() {
        // The discard drains and re-enqueues. If the survivors came back
        // reordered, a program sharing one port across two windows would see
        // its events in the wrong order — which is worse than leaking them.
        let h = Harness::new();
        let port = h.exec.create_port(h.task, 1, 1, 0);
        let keep = h
            .intui
            .open_window(
                ScreenId(1),
                "Keep",
                Rect::new(0, 60, 100, 50),
                0,
                idcmp::MOUSEMOVE,
                port,
                h.task,
            )
            .unwrap();
        let drop = h
            .intui
            .open_window(
                ScreenId(1),
                "Drop",
                Rect::new(0, 0, 100, 50),
                0,
                idcmp::MOUSEMOVE,
                port,
                h.task,
            )
            .unwrap();

        // Interleave events from the two windows on the shared port.
        h.intui.dispatch_mouse(
            &h.exec,
            &MouseEvent {
                x: 20,
                y: 70,
                buttons: 0,
                changed: 0,
            },
        );
        h.intui.dispatch_mouse(
            &h.exec,
            &MouseEvent {
                x: 20,
                y: 20,
                buttons: 0,
                changed: 0,
            },
        );
        h.intui.dispatch_mouse(
            &h.exec,
            &MouseEvent {
                x: 21,
                y: 70,
                buttons: 0,
                changed: 0,
            },
        );
        h.intui.dispatch_mouse(
            &h.exec,
            &MouseEvent {
                x: 21,
                y: 20,
                buttons: 0,
                changed: 0,
            },
        );
        assert_eq!(h.exec.ports_queue_len(port), 4);

        h.intui.close_window(&h.exec, drop).unwrap();

        assert_eq!(
            h.exec.ports_queue_len(port),
            2,
            "only the closed window's events are discarded",
        );
        // The survivors kept their order: x = 20 before x = 21.
        let first = h.exec.get_msg(port).unwrap().as_intui();
        let second = h.exec.get_msg(port).unwrap().as_intui();
        assert_eq!(first.im_Source, keep.0 as u64);
        assert_eq!(second.im_Source, keep.0 as u64);
        assert_eq!(first.im_MouseX, 20);
        assert_eq!(second.im_MouseX, 21);
    }

    #[test]
    fn closing_a_window_does_not_discard_a_programs_own_messages() {
        // A plain `PutMsg` from the program is not an Intuition event and has
        // no `im_Source`. Closing a window does not license discarding it.
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(10, 10, 100, 50), idcmp::MOUSE);
        let mine = Message::empty();
        h.exec.put_msg(port, mine).unwrap();

        h.intui.close_window(&h.exec, window).unwrap();
        assert_eq!(
            h.exec.ports_queue_len(port),
            1,
            "a message the program put itself must survive",
        );
    }

    #[test]
    fn closing_the_active_window_moves_focus() {
        // Keyboard focus cannot stay on a window that no longer exists.
        let h = Harness::new();
        let (first, _) = h.open("First", Rect::new(0, 0, 100, 50), idcmp::KEYBOARD);
        let port = h.exec.create_port(h.task, 1, 1, 0);
        let second = h
            .intui
            .open_window(
                ScreenId(1),
                "Second",
                Rect::new(0, 60, 100, 50),
                window_flags::ACTIVE,
                idcmp::KEYBOARD,
                port,
                h.task,
            )
            .unwrap();
        assert_eq!(h.intui.active_window(), second);
        h.intui.close_window(&h.exec, second).unwrap();
        assert_ne!(h.intui.active_window(), second);
        assert!(h.intui.window(h.intui.active_window()).is_some());
        h.intui.close_window(&h.exec, first).unwrap();
    }

    #[test]
    fn setting_a_title_marks_the_layer_dirty() {
        let h = Harness::new();
        let (window, _) = h.open("Old", Rect::new(0, 0, 200, 100), idcmp::NONE);
        h.intui
            .layers()
            .with_layer(h.intui.window(window).unwrap().layer, |l| l.dirty = false)
            .unwrap();
        h.intui.set_title(window, "New").unwrap();
        assert_eq!(h.intui.window(window).unwrap().title, "New");
        assert!(h.intui.layers().has_dirty());
    }

    #[test]
    fn setting_a_zero_sized_window_is_refused() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 100, 50), idcmp::NONE);
        assert_eq!(
            h.intui
                .set_window_bounds(window, Rect::new(0, 0, 0, 50))
                .unwrap_err(),
            errors::BAD_RECT
        );
    }

    #[test]
    fn moving_a_window_updates_its_layer_position() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(10, 10, 100, 50), idcmp::NONE);
        h.intui
            .set_window_bounds(window, Rect::new(200, 300, 100, 50))
            .unwrap();
        let w = h.intui.window(window).unwrap();
        let layer = h.intui.layers().layer(w.layer).unwrap();
        assert_eq!(layer.screen_rect(), Rect::new(200, 300, 100, 50));
    }

    #[test]
    fn narrowing_the_idcmp_mask_stops_events_immediately() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::MOUSE);
        assert!(h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 0, 0))
            .is_some());
        assert_eq!(h.exec.ports_queue_len(port), 1);
        h.intui.set_attrs(window, 0, 0).unwrap();
        assert!(h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 0, 0))
            .is_none());
        assert_eq!(h.exec.ports_queue_len(port), 1, "nothing new was queued");
    }

    #[test]
    fn set_attrs_to_a_mask_with_no_port_is_refused() {
        let h = Harness::new();
        let w = h
            .intui
            .open_window(
                ScreenId(1),
                "no port",
                Rect::new(0, 0, 100, 100),
                0,
                idcmp::NONE,
                PortId::NONE,
                h.task,
            )
            .unwrap();
        assert_eq!(
            h.intui.set_attrs(w, 0, idcmp::MOUSE).unwrap_err(),
            errors::NO_PORT
        );
    }

    // --- Gadgets ------------------------------------------------------------

    #[test]
    fn a_gadget_gets_an_handle_and_can_be_removed() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 200, 100), idcmp::GADGET);
        let g = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 30, 80, 20), "OK")
            .unwrap();
        assert_eq!(h.intui.gadget(g).unwrap().label, "OK");
        assert_eq!(h.intui.window(window).unwrap().gadgets.len(), 1);
        h.intui.remove_gadget(g).unwrap();
        assert!(h.intui.gadget(g).is_none());
        assert!(h.intui.remove_gadget(g).is_err());
    }

    #[test]
    fn a_zero_sized_gadget_is_refused() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 200, 100), idcmp::GADGET);
        assert!(h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(0, 0, 0, 10), "")
            .is_err());
    }

    #[test]
    fn hit_testing_a_gadget_prefers_the_frontmost() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 200, 100), idcmp::GADGET);
        let back = h
            .intui
            .add_gadget(
                window,
                GadgetKind::Label,
                Rect::new(10, 10, 100, 50),
                "back",
            )
            .unwrap();
        let front = h
            .intui
            .add_gadget(
                window,
                GadgetKind::Button,
                Rect::new(20, 20, 50, 20),
                "front",
            )
            .unwrap();
        // A point in both: the front one wins.
        assert_eq!(h.intui.gadget_at(window, 25, 25).unwrap().id, front);
        // A point only in the back one.
        assert_eq!(h.intui.gadget_at(window, 12, 12).unwrap().id, back);
        // A point in neither.
        assert!(h.intui.gadget_at(window, 190, 90).is_none());
    }

    #[test]
    fn a_disabled_gadget_does_not_take_the_click() {
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 200, 100), idcmp::GADGET);
        let g = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 10, 50, 20), "OK")
            .unwrap();
        h.intui.set_gadget_enabled(g, false).unwrap();
        assert!(h.intui.gadget_at(window, 20, 20).is_none());
    }

    // --- Event routing: the report's acceptance criterion --------------------

    #[test]
    fn a_mouse_move_reaches_the_windows_message_port() {
        // This is the report's §10.4 chain, minus the syscall: PS/2 event ->
        // Intuition routing -> IDCMP message -> Exec message port.
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(10, 10, 200, 100), idcmp::MOUSE);
        let hit = h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 40, 0, 0))
            .expect("the window is under the pointer");
        assert_eq!(hit, window);
        assert_eq!(h.exec.ports_queue_len(port), 1);
        let msg = h.exec.get_msg(port).unwrap();
        let intui = msg.as_intui();
        assert_eq!(intui.im_Type, IntuiMessage::MOUSE);
        assert_eq!(intui.im_Code, mouse_codes::MOVE);
        assert_eq!(intui.im_Source, window.0 as u64);
        // The coordinates are window-local, not screen coordinates: a program
        // that moved its window still sees (40, 30).
        assert_eq!(intui.im_MouseX, 40);
        assert_eq!(intui.im_MouseY, 30);
    }

    #[test]
    fn the_message_carries_the_reply_port_so_a_program_can_reply() {
        let h = Harness::new();
        let (_window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::MOUSE);
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 0, 0))
            .unwrap();
        let msg = h.exec.get_msg(port).unwrap();
        assert_eq!(msg.mn_reply_port, port.0 as u64);
        // A reply succeeds, so a program that follows the report's acceptance
        // path (receive -> process -> ReplyMsg) does not fail at the last step.
        assert!(h.exec.reply_msg(&msg, Message::empty()).is_ok());
    }

    #[test]
    fn a_click_on_a_gadget_becomes_an_idcmp_gadget_message() {
        let h = Harness::new();
        let (window, port) = h.open(
            "Test",
            Rect::new(0, 0, 200, 200),
            idcmp::GADGET | idcmp::MOUSEBUTTONS,
        );
        let gadget = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 10, 80, 20), "OK")
            .unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 20, 1, 1))
            .expect("the window is under the pointer");
        let msg = h.exec.get_msg(port).unwrap();
        let intui = msg.as_intui();
        assert_eq!(intui.im_Type, IntuiMessage::GADGET);
        assert_eq!(intui.im_Code, gadget.0 as u16);
        assert_eq!(intui.im_Source, window.0 as u64);
        assert_eq!(intui.im_Button, button_mask(1), "the left button is down");
    }

    #[test]
    fn a_click_on_the_close_gadget_becomes_a_close_request() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 100), idcmp::REQUEST);
        let close = h.intui.window(window).unwrap().close_gadget_rect().unwrap();
        let x = close.x + close.width as i32 / 2;
        let y = close.y + close.height as i32 / 2;
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(x, y, 1, 1))
            .unwrap();
        let intui = h.exec.get_msg(port).unwrap().as_intui();
        assert_eq!(intui.im_Type, IntuiMessage::REQUEST);
        assert_eq!(intui.im_Code, request_codes::CLOSE);
    }

    #[test]
    fn a_click_outside_every_window_reaches_nobody() {
        let h = Harness::new();
        h.open("Test", Rect::new(10, 10, 50, 50), idcmp::ALL);
        assert!(h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(400, 400, 1, 1))
            .is_none());
    }

    #[test]
    fn the_topmost_window_receives_the_click() {
        // Anything else delivers a click to a window the user cannot see.
        let h = Harness::new();
        let (back, back_port) = h.open("Back", Rect::new(10, 10, 200, 200), idcmp::MOUSE);
        let port = h.exec.create_port(h.task, 1, 1, 0);
        let front = h
            .intui
            .open_window(
                ScreenId(1),
                "Front",
                Rect::new(50, 50, 200, 200),
                window_flags::ACTIVE,
                idcmp::MOUSE,
                port,
                h.task,
            )
            .unwrap();
        let hit = h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(100, 100, 1, 1))
            .unwrap();
        assert_eq!(hit, front);
        assert_eq!(h.exec.ports_queue_len(port), 1);
        assert_eq!(
            h.exec.ports_queue_len(back_port),
            0,
            "the back window is occluded"
        );
        let _ = back;
    }

    #[test]
    fn a_click_outside_the_top_window_reaches_the_one_underneath() {
        let h = Harness::new();
        let (back, back_port) = h.open("Back", Rect::new(0, 0, 200, 200), idcmp::MOUSE);
        let port = h.exec.create_port(h.task, 1, 1, 0);
        h.intui
            .open_window(
                ScreenId(1),
                "Front",
                Rect::new(100, 100, 100, 100),
                0,
                idcmp::MOUSE,
                port,
                h.task,
            )
            .unwrap();
        let hit = h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 1, 1))
            .unwrap();
        assert_eq!(hit, back);
        assert_eq!(h.exec.ports_queue_len(back_port), 1);
        assert_eq!(h.exec.ports_queue_len(port), 0);
    }

    #[test]
    fn a_hidden_window_receives_nothing() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::MOUSE);
        h.intui
            .layers()
            .set_visible(h.intui.window(window).unwrap().layer, false)
            .unwrap();
        assert!(h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 1, 1))
            .is_none());
        assert_eq!(h.exec.ports_queue_len(port), 0);
    }

    #[test]
    fn a_window_that_asked_for_no_mouse_events_gets_none() {
        let h = Harness::new();
        let (_, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::KEYBOARD);
        assert!(h
            .intui
            .dispatch_mouse(&h.exec, &mouse_at(50, 50, 1, 1))
            .is_none());
        assert_eq!(h.exec.ports_queue_len(port), 0);
    }

    #[test]
    fn a_button_change_reports_the_button_that_changed() {
        // A click that starts with both buttons down must report the one that
        // changed, not the whole mask.
        assert_eq!(button_code(0x01, 0x01), mouse_codes::LEFT_DOWN);
        assert_eq!(button_code(0x00, 0x01), mouse_codes::LEFT_UP);
        assert_eq!(button_code(0x02, 0x02), mouse_codes::RIGHT_DOWN);
        assert_eq!(button_code(0x00, 0x02), mouse_codes::RIGHT_UP);
        assert_eq!(button_code(0x04, 0x04), mouse_codes::MIDDLE_DOWN);
        assert_eq!(button_code(0x00, 0x04), mouse_codes::MIDDLE_UP);
        assert_eq!(button_code(0x00, 0x00), mouse_codes::MOVE);
    }

    #[test]
    fn a_keyboard_event_goes_to_the_active_window() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::KEYBOARD);
        let hit = h
            .intui
            .dispatch_key(
                &h.exec,
                &KeyEventLike {
                    code: KeyCode::from_ascii('a'),
                    released: false,
                },
            )
            .unwrap();
        assert_eq!(hit, window);
        let intui = h.exec.get_msg(port).unwrap().as_intui();
        assert_eq!(intui.im_Type, IntuiMessage::KEYBOARD);
        assert_eq!(intui.im_Code, KeyCode::from_ascii('a').raw() as u16);
        assert_eq!(intui.im_Flags, 0, "a press");
        assert_eq!(intui.im_Source, window.0 as u64);
    }

    #[test]
    fn a_keyboard_release_is_flagged() {
        let h = Harness::new();
        let (_, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::KEYBOARD);
        h.intui
            .dispatch_key(
                &h.exec,
                &KeyEventLike {
                    code: KeyCode::from_ascii('a'),
                    released: true,
                },
            )
            .unwrap();
        assert_eq!(h.exec.get_msg(port).unwrap().as_intui().im_Flags, 1);
    }

    #[test]
    fn a_keyboard_event_with_no_active_window_goes_nowhere() {
        let h = Harness::new();
        // No window has been opened, so nothing is active.
        assert!(h
            .intui
            .dispatch_key(
                &h.exec,
                &KeyEventLike {
                    code: KeyCode::from_ascii('a'),
                    released: false,
                },
            )
            .is_none());
    }

    #[test]
    fn dispatch_event_routes_both_kinds() {
        // The kernel's input path calls exactly this one function, so it has
        // to handle both event kinds.
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::ALL);
        let mut m = Mouse::new();
        m.set_bounds(0, 0, 640, 480);
        m.feed(0x09);
        m.feed(0x10);
        let mouse = m.feed(0x08).unwrap();
        let hit = h
            .intui
            .dispatch_event(&h.exec, &Event::Mouse(mouse))
            .unwrap();
        assert_eq!(hit, window);
        let key = Event::Key(vela_kernel_input::KeyEvent {
            code: KeyCode::from_ascii('x'),
            released: false,
            shift: false,
            ctrl: false,
            alt: false,
        });
        h.intui.dispatch_event(&h.exec, &key).unwrap();
        assert_eq!(h.exec.ports_queue_len(port), 2);
    }

    #[test]
    fn events_arrive_in_order() {
        let h = Harness::new();
        let (_, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::MOUSE);
        for y in 10..15 {
            h.intui
                .dispatch_mouse(&h.exec, &mouse_at(50, y, 0, 0))
                .unwrap();
        }
        for y in 10..15 {
            let intui = h.exec.get_msg(port).unwrap().as_intui();
            assert_eq!(intui.im_MouseY, y as i16);
        }
    }

    #[test]
    fn a_toggle_gadget_stays_on_after_the_release() {
        // "Selected" on a toggle means *on*, not *held*: a toggle that went
        // dark when the mouse button came up would look like it was never
        // clicked at all.
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::GADGET);
        let g = h
            .intui
            .add_gadget(window, GadgetKind::Toggle, Rect::new(10, 10, 60, 20), "T")
            .unwrap();
        assert!(!h.intui.gadget(g).unwrap().selected);
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 1, 1))
            .unwrap();
        assert!(h.intui.gadget(g).unwrap().selected, "the press turns it on");
        h.exec.get_msg(port).unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 0, 1))
            .unwrap();
        assert!(
            h.intui.gadget(g).unwrap().selected,
            "the release must not turn it off"
        );
        // A second press turns it back off.
        h.exec.get_msg(port).unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 1, 1))
            .unwrap();
        assert!(!h.intui.gadget(g).unwrap().selected);
    }

    #[test]
    fn a_plain_button_is_selected_only_while_held() {
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::GADGET);
        let g = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 10, 60, 20), "OK")
            .unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 1, 1))
            .unwrap();
        assert!(h.intui.gadget(g).unwrap().selected, "held");
        h.exec.get_msg(port).unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 0, 1))
            .unwrap();
        assert!(!h.intui.gadget(g).unwrap().selected, "released");
    }

    #[test]
    fn a_gadget_event_fires_on_the_release_too() {
        // Both halves of a click reach the program: a program that only gets
        // the press cannot tell a completed click from a drag that started on
        // its gadget.
        let h = Harness::new();
        let (window, port) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::GADGET);
        h.intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 10, 60, 20), "OK")
            .unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 1, 1))
            .unwrap();
        h.intui
            .dispatch_mouse(&h.exec, &mouse_at(30, 20, 0, 1))
            .unwrap();
        let press = h.exec.get_msg(port).unwrap().as_intui();
        let release = h.exec.get_msg(port).unwrap().as_intui();
        assert_eq!(
            press.im_Button,
            button_mask(1),
            "the press has the button down"
        );
        assert_eq!(release.im_Button, 0, "the release has it up");
    }

    #[test]
    fn clearing_the_selection_releases_every_gadget() {
        // A gadget left selected after the mouse left would render pressed
        // forever.
        let h = Harness::new();
        let (window, _) = h.open("Test", Rect::new(0, 0, 200, 200), idcmp::GADGET);
        let a = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 10, 60, 20), "A")
            .unwrap();
        let b = h
            .intui
            .add_gadget(window, GadgetKind::Button, Rect::new(10, 40, 60, 20), "B")
            .unwrap();
        h.intui.mark_gadget_selected(a, true);
        h.intui.mark_gadget_selected(b, true);
        h.intui.clear_selection();
        assert!(!h.intui.gadget(a).unwrap().selected);
        assert!(!h.intui.gadget(b).unwrap().selected);
    }

    // --- Drawing ------------------------------------------------------------

    #[test]
    fn repainting_a_window_paints_its_title_bar() {
        let h = Harness::new();
        let (window, _) = h.open("Hi", Rect::new(0, 0, 200, 100), idcmp::NONE);
        h.intui.repaint_window(window).unwrap();
        let layer = h.intui.window(window).unwrap().layer;
        let l = h.intui.layers().layer(layer).unwrap();
        // The title bar fill.
        let bar_y = theme::metrics::BORDER as i32 + 2;
        let pixel = l.get(100, bar_y).unwrap();
        let active = theme::TITLEBAR_ACTIVE_BG;
        let bar_packed = LayerStack::pack(active);
        assert_eq!(
            pixel, bar_packed,
            "the title bar is painted in the active colour"
        );
        // The border.
        assert_eq!(
            l.get(0, 0).unwrap(),
            LayerStack::pack(theme::PANEL_BORDER),
            "the border is painted"
        );
        // Something was drawn inside the bar: the title's glyphs.
        let mut ink = 0;
        for y in theme::metrics::BORDER as i32..theme::metrics::TITLEBAR_HEIGHT as i32 {
            for x in 0..200 {
                if l.get(x, y).unwrap() != bar_packed {
                    ink += 1;
                }
            }
        }
        assert!(ink > 0, "the title text was drawn");
    }

    #[test]
    fn repainting_marks_the_layer_dirty() {
        let h = Harness::new();
        let (window, _) = h.open("Hi", Rect::new(0, 0, 200, 100), idcmp::NONE);
        h.intui.repaint_window(window).unwrap();
        assert!(h.intui.layers().has_dirty());
    }

    #[test]
    fn an_inactive_window_uses_the_inactive_title_bar_colour() {
        let h = Harness::new();
        let port = h.exec.create_port(h.task, 1, 1, 0);
        let active = h.open("A", Rect::new(0, 0, 200, 100), idcmp::NONE).0;
        let inactive = h
            .intui
            .open_window(
                ScreenId(1),
                "B",
                Rect::new(0, 120, 200, 100),
                0,
                idcmp::NONE,
                port,
                h.task,
            )
            .unwrap();
        h.intui.repaint_window(inactive).unwrap();
        let l = h
            .intui
            .layers()
            .layer(h.intui.window(inactive).unwrap().layer)
            .unwrap();
        let y = theme::metrics::BORDER as i32 + 2;
        assert_eq!(
            l.get(100, y).unwrap(),
            LayerStack::pack(theme::TITLEBAR_BG),
            "not the active colour"
        );
        let _ = active;
    }

    #[test]
    fn a_window_with_no_close_gadget_has_none() {
        let h = Harness::new();
        let w = h
            .intui
            .open_window(
                ScreenId(1),
                "no close",
                Rect::new(0, 0, 100, 100),
                0,
                idcmp::NONE,
                PortId::NONE,
                h.task,
            )
            .unwrap();
        assert!(h.intui.window(w).unwrap().close_gadget_rect().is_none());
        assert!(!h.intui.close_gadget_at(w, 5, 5));
    }

    #[test]
    fn the_library_handle_is_the_intuition_one() {
        assert_eq!(
            intuition_library_handle(),
            LibraryHandle::for_library(vela_abi_native64::LibraryId::Intuition)
        );
    }

    #[test]
    fn the_service_is_debuggable() {
        let h = Harness::new();
        let text = alloc::format!("{:?}", h.intui);
        assert!(text.contains("Intuition"), "{text}");
        assert!(text.contains("screens: 1"), "{text}");
    }

    #[test]
    fn the_button_mask_matches_the_abi() {
        assert_eq!(button_mask(0x01), abi_buttons::LEFT);
        assert_eq!(button_mask(0x02), abi_buttons::RIGHT);
        assert_eq!(button_mask(0x04), abi_buttons::MIDDLE);
        assert_eq!(
            button_mask(0x07),
            abi_buttons::LEFT | abi_buttons::RIGHT | abi_buttons::MIDDLE
        );
        assert_eq!(button_mask(0), 0);
    }
}
