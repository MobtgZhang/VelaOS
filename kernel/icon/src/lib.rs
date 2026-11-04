//! Icon objects: the `icon.library` service.
//!
//! # What this crate is for
//!
//! `icon.library` is the data half of the desktop: a `DiskObject` is the
//! record behind one icon — its filename, its type, its default tool, its
//! ToolTypes, and its position. The desktop draws from those records, and a
//! program that launches an object reads them.
//!
//! VelaOS does not copy AmigaOS's `DiskObject` pointer model into user space.
//! A program gets an opaque [`IconHandle`], and every call takes that handle
//! back. The object itself is owned by the kernel, so a program cannot hand
//! the kernel a pointer to an icon record and make the kernel read some other
//! task's memory.
//!
//! # What it deliberately does not do
//!
//! It does not draw. Rendering belongs to the desktop, which composites
//! through Intuition and Layers. The icon service records *what* an icon is,
//! and the geometry it occupies, and leaves the pixels to the drawing code.
//! That is the VelaOS-native split: the data model is one thing and the
//! renderer is another.
//!
//! # ToolTypes
//!
//! A ToolType is a `NAME=value` string. The service stores the two halves
//! separately so that a lookup is a comparison rather than a parse of the
//! caller's memory every time. Lookups are case-insensitive, as AmigaOS's
//! `FindToolType` and `MatchToolValue` are.

#![no_std]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use spin::Mutex;

use vela_abi_native64::TagItem;

/// The error codes this service reports.
///
/// They are the native64 `status` values, so a caller reaches the same error
/// through a service method and through a syscall.
pub mod errors {
    use vela_abi_native64::status;

    /// The call succeeded.
    pub const OK: i32 = status::OK;
    /// The handle names no icon.
    pub const NO_OBJECT: i32 = status::NO_OBJECT;
    /// The name or ToolType is invalid.
    pub const INVALID_NAME: i32 = status::INVALID_NAME;
    /// The icon does not exist.
    pub const NOT_FOUND: i32 = status::NOT_FOUND;
    /// The list or object is too large.
    pub const TOO_LARGE: i32 = status::TOO_LARGE;
    /// The rectangle is invalid.
    pub const BAD_RECT: i32 = status::BAD_ITEM;
    /// The tag list is out of range.
    pub const RANGE: i32 = status::RANGE;
}

/// An opaque handle to an icon object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IconHandle(pub u32);

impl IconHandle {
    /// The handle that names no icon.
    pub const NONE: IconHandle = IconHandle(0);

    /// True when this handle names no icon.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }
}

/// The four default object kinds.
///
/// These are the AmigaOS `do_Type` values a default object is selected by.
/// The constants are in the SDK as well, so a caller can name them without
/// importing the kernel crate.
pub mod kind {
    /// A disk object — the base record.
    pub const DISK: u32 = 1;
    /// A drawer.
    pub const DRAWER: u32 = 2;
    /// A tool, launched by a double-click.
    pub const TOOL: u32 = 3;
    /// An application icon.
    pub const APP: u32 = 4;
}

/// One ToolType.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolType {
    /// The name before the `=`.
    pub name: String,
    /// The value after the `=`.
    pub value: String,
}

/// An icon's rectangle, in screen coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct IconRect {
    /// The left edge.
    pub x: i32,
    /// The top edge.
    pub y: i32,
    /// The width.
    pub w: u32,
    /// The height.
    pub h: u32,
}

impl IconRect {
    /// A rectangle at `(x, y)` of `w` by `h`.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    /// Pack four 16-bit fields into one word, in the kernel's rectangle order.
    pub const fn pack(self) -> u64 {
        (self.x as u16 as u64)
            | ((self.y as u16 as u64) << 16)
            | ((self.w as u64) << 32)
            | ((self.h as u64) << 48)
    }

    /// Unpack the four 16-bit fields.
    pub const fn unpack(packed: u64) -> Self {
        Self {
            x: (packed as u16) as i16 as i32,
            y: ((packed >> 16) as u16) as i16 as i32,
            w: ((packed >> 32) as u16) as u32,
            h: ((packed >> 48) as u16) as u32,
        }
    }

    /// The default icon rectangle.
    pub const fn default_rect() -> Self {
        Self::new(0, 0, 80, 72)
    }
}

/// One icon record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconObject {
    /// The object's filename or path.
    pub name: String,
    /// The object kind, from [`kind`].
    pub icon_type: u32,
    /// The program a tool launches.
    pub default_tool: String,
    /// The ToolTypes.
    pub tool_types: Vec<ToolType>,
    /// The tags stored on the object.
    pub tags: Vec<TagItem>,
    /// The revision count.
    pub revision: u32,
    /// The icon's rectangle.
    pub rect: IconRect,
    /// Whether the icon is drawn as selected.
    pub selected: bool,
    /// The selected state's colour word.
    pub selected_color: u32,
}

impl IconObject {
    /// A default object for `icon_type`.
    pub fn default_for(icon_type: u32) -> Self {
        let tool = match icon_type {
            kind::DRAWER => "DRAWER",
            kind::TOOL => "TOOL",
            kind::APP => "APP",
            _ => "DISK",
        };
        Self {
            name: String::new(),
            icon_type,
            default_tool: String::new(),
            tool_types: Vec::new(),
            tags: Vec::new(),
            revision: 1,
            rect: IconRect::default_rect(),
            selected: false,
            selected_color: 0,
        }
        .with_default_tool_type(tool)
    }

    /// Add a `TYPE=<tool>` ToolType to the default object.
    fn with_default_tool_type(self, tool: &str) -> Self {
        let mut object = self;
        object.tool_types.push(ToolType {
            name: String::from("TYPE"),
            value: String::from(tool),
        });
        object
    }
}

/// The `IconControl` operations.
pub mod control {
    /// Draw the icon as selected.
    pub const SELECT: u32 = 1;
    /// Draw the icon as unselected.
    pub const DESELECT: u32 = 2;
    /// Query the selected state.
    pub const GET_SELECTED: u32 = 3;
    /// Query the object type.
    pub const GET_TYPE: u32 = 4;
    /// Set the object type.
    pub const SET_TYPE: u32 = 5;
}

/// The icon service.
pub struct IconService {
    /// Icon records, indexed by handle. A freed handle leaves a hole so that
    /// other handles do not shift.
    objects: Mutex<Vec<Option<IconObject>>>,
    /// The next handle value.
    next: Mutex<u32>,
    /// The default objects, one per kind.
    defaults: Mutex<Vec<IconObject>>,
    /// The legacy free list, kept only for the `AddFreeList`/`FreeFreeList`
    /// calls. It records handles whose records were replaced, not raw memory.
    free_list: Mutex<Vec<IconHandle>>,
    /// The service-wide colour word used for selected icons.
    selected_color: Mutex<u32>,
}

impl core::fmt::Debug for IconService {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IconService")
            .field("objects", &self.objects.lock().len())
            .field("defaults", &self.defaults.lock().len())
            .finish()
    }
}

impl Default for IconService {
    fn default() -> Self {
        Self::new()
    }
}

impl IconService {
    /// A service with the four default objects.
    pub fn new() -> Self {
        Self {
            objects: Mutex::new(Vec::new()),
            next: Mutex::new(1),
            defaults: Mutex::new(vec![
                IconObject::default_for(kind::DISK),
                IconObject::default_for(kind::DRAWER),
                IconObject::default_for(kind::TOOL),
                IconObject::default_for(kind::APP),
            ]),
            free_list: Mutex::new(Vec::new()),
            selected_color: Mutex::new(0),
        }
    }

    /// Allocate a handle for an object.
    fn allocate(&self, object: IconObject) -> IconHandle {
        let id = {
            let mut next = self.next.lock();
            let id = *next;
            *next += 1;
            id
        };
        self.objects.lock().push(Some(object));
        IconHandle(id)
    }

    /// Look up an object by handle.
    fn get(&self, handle: IconHandle) -> Option<IconObject> {
        if handle.is_none() {
            return None;
        }
        let index = handle.0 as usize - 1;
        let objects = self.objects.lock();
        objects.get(index).and_then(|o| o.clone())
    }

    /// Mutate an object by handle.
    fn with_object<F>(&self, handle: IconHandle, f: F) -> Result<IconObject, i32>
    where
        F: FnOnce(&mut IconObject),
    {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut objects = self.objects.lock();
        let Some(Some(object)) = objects.get_mut(index) else {
            return Err(errors::NO_OBJECT);
        };
        f(object);
        Ok(object.clone())
    }

    /// The default object index for a kind.
    fn default_index(&self, icon_type: u32) -> Option<usize> {
        match icon_type {
            kind::DISK => Some(0),
            kind::DRAWER => Some(1),
            kind::TOOL => Some(2),
            kind::APP => Some(3),
            _ => None,
        }
    }

    /// `GetDiskObject` — find an object by name.
    pub fn get_disk_object(&self, name: &str) -> Result<IconHandle, i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let objects = self.objects.lock();
        for (index, object) in objects.iter().enumerate() {
            if let Some(object) = object {
                if object.name == name {
                    return Ok(IconHandle(index as u32 + 1));
                }
            }
        }
        Err(errors::NOT_FOUND)
    }

    /// `GetDiskObjectNew` — find an object, creating a default if absent.
    pub fn get_disk_object_new(&self, name: &str) -> Result<IconHandle, i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        if let Ok(handle) = self.get_disk_object(name) {
            return Ok(handle);
        }
        let mut object = IconObject::default_for(kind::DISK);
        object.name = String::from(name);
        Ok(self.allocate(object))
    }

    /// `NewDiskObject` — a fresh default object of `icon_type`.
    pub fn new_disk_object(&self, icon_type: u32) -> Result<IconHandle, i32> {
        let Some(index) = self.default_index(icon_type) else {
            return Err(errors::INVALID_NAME);
        };
        let object = self.defaults.lock()[index].clone();
        Ok(self.allocate(object))
    }

    /// `GetDefDiskObject` — a copy of the default object of `icon_type`.
    pub fn get_def_disk_object(&self, icon_type: u32) -> Result<IconHandle, i32> {
        let Some(index) = self.default_index(icon_type) else {
            return Err(errors::INVALID_NAME);
        };
        let object = self.defaults.lock()[index].clone();
        Ok(self.allocate(object))
    }

    /// `PutDefDiskObject` — replace the default object of `icon_type`.
    pub fn put_def_disk_object(&self, icon_type: u32, handle: IconHandle) -> Result<(), i32> {
        let Some(index) = self.default_index(icon_type) else {
            return Err(errors::INVALID_NAME);
        };
        let Some(object) = self.get(handle) else {
            return Err(errors::NO_OBJECT);
        };
        self.defaults.lock()[index] = object;
        Ok(())
    }

    /// `PutDiskObject` — save an object under `name`.
    pub fn put_disk_object(&self, handle: IconHandle, name: &str) -> Result<(), i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        self.with_object(handle, |object| {
            object.name = String::from(name);
        })?;
        Ok(())
    }

    /// `FreeDiskObject` — release an object handle.
    pub fn free_disk_object(&self, handle: IconHandle) -> Result<(), i32> {
        if handle.is_none() {
            return Err(errors::NO_OBJECT);
        }
        let index = handle.0 as usize - 1;
        let mut objects = self.objects.lock();
        match objects.get_mut(index) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                Ok(())
            }
            _ => Err(errors::NO_OBJECT),
        }
    }

    /// `DupDiskObject` — copy an object into a new handle.
    pub fn dup_disk_object(&self, handle: IconHandle) -> Result<IconHandle, i32> {
        let Some(object) = self.get(handle) else {
            return Err(errors::NO_OBJECT);
        };
        Ok(self.allocate(object))
    }

    /// `DeleteDiskObject` — remove an object by name.
    pub fn delete_disk_object(&self, name: &str) -> Result<(), i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let handle = self.get_disk_object(name)?;
        self.free_disk_object(handle)
    }

    /// `FindToolType` — the value of the named ToolType.
    pub fn find_tool_type(&self, handle: IconHandle, name: &str) -> Result<Option<String>, i32> {
        if name.is_empty() {
            return Err(errors::INVALID_NAME);
        }
        let Some(object) = self.get(handle) else {
            return Err(errors::NO_OBJECT);
        };
        Ok(object
            .tool_types
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .map(|t| t.value.clone()))
    }

    /// `MatchToolValue` — whether the named ToolType has `value`.
    pub fn match_tool_value(
        &self,
        handle: IconHandle,
        name: &str,
        value: &str,
    ) -> Result<bool, i32> {
        let found = self.find_tool_type(handle, name)?;
        Ok(found
            .as_deref()
            .map(|v| v.eq_ignore_ascii_case(value))
            .unwrap_or(false))
    }

    /// `PutIconTagList` — replace the object's tags.
    pub fn put_icon_tag_list(&self, handle: IconHandle, tags: &[TagItem]) -> Result<(), i32> {
        if tags.len() > 64 {
            return Err(errors::TOO_LARGE);
        }
        self.with_object(handle, |object| {
            object.tags = tags.to_vec();
        })?;
        Ok(())
    }

    /// `GetIconTagList` — the object's tags.
    pub fn get_icon_tag_list(&self, handle: IconHandle) -> Result<Vec<TagItem>, i32> {
        let Some(object) = self.get(handle) else {
            return Err(errors::NO_OBJECT);
        };
        Ok(object.tags)
    }

    /// `BumpRevision` — increment and return the revision.
    pub fn bump_revision(&self, handle: IconHandle) -> Result<u32, i32> {
        let object = self.with_object(handle, |object| {
            object.revision = object.revision.wrapping_add(1);
        })?;
        Ok(object.revision)
    }

    /// `LayoutIcon` — set the icon's rectangle.
    pub fn layout_icon(&self, handle: IconHandle, rect: IconRect) -> Result<IconRect, i32> {
        if rect.w == 0 || rect.h == 0 {
            return Err(errors::BAD_RECT);
        }
        let object = self.with_object(handle, |object| {
            object.rect = rect;
        })?;
        Ok(object.rect)
    }

    /// `GetIconRectangle` — the icon's rectangle.
    pub fn get_icon_rectangle(&self, handle: IconHandle) -> Result<IconRect, i32> {
        let Some(object) = self.get(handle) else {
            return Err(errors::NO_OBJECT);
        };
        Ok(object.rect)
    }

    /// `IconControl` — apply an operation to an icon.
    ///
    /// `value` is used by `SET_TYPE`; the other operations ignore it.
    pub fn icon_control(&self, handle: IconHandle, operation: u32, value: u32) -> Result<u64, i32> {
        match operation {
            control::SELECT | control::DESELECT => {
                let selected = operation == control::SELECT;
                self.with_object(handle, |object| object.selected = selected)?;
                Ok(selected as u64)
            }
            control::GET_SELECTED => {
                let Some(object) = self.get(handle) else {
                    return Err(errors::NO_OBJECT);
                };
                Ok(object.selected as u64)
            }
            control::GET_TYPE => {
                let Some(object) = self.get(handle) else {
                    return Err(errors::NO_OBJECT);
                };
                Ok(object.icon_type as u64)
            }
            control::SET_TYPE => {
                if self.default_index(value).is_none() {
                    return Err(errors::INVALID_NAME);
                }
                let object = self.with_object(handle, |object| {
                    object.icon_type = value;
                })?;
                Ok(object.icon_type as u64)
            }
            _ => Err(errors::RANGE),
        }
    }

    /// `DrawIconState` — record the icon's selected state.
    pub fn draw_icon_state(&self, handle: IconHandle, selected: u32) -> Result<(), i32> {
        self.with_object(handle, |object| {
            object.selected = selected != 0;
        })?;
        Ok(())
    }

    /// `ChangeToSelectedIconColor` — set the service's selected colour word.
    ///
    /// The pixels are drawn by the desktop, so the colour is a value the
    /// desktop reads rather than a palette mutation performed here. The
    /// previous colour is returned.
    pub fn change_to_selected_icon_color(&self, color: u32) -> Result<u32, i32> {
        let mut selected = self.selected_color.lock();
        let old = *selected;
        *selected = color;
        Ok(old)
    }

    /// `AddFreeList` — remember a freed handle.
    pub fn add_free_list(&self, handle: IconHandle) -> Result<u32, i32> {
        if self.get(handle).is_none() {
            return Err(errors::NO_OBJECT);
        }
        let mut list = self.free_list.lock();
        if !list.contains(&handle) {
            list.push(handle);
        }
        Ok(list.len() as u32)
    }

    /// `FreeFreeList` — drop the remembered handles.
    pub fn free_free_list(&self) -> Result<u32, i32> {
        let mut list = self.free_list.lock();
        let count = list.len() as u32;
        list.clear();
        Ok(count)
    }

    /// The number of live objects.
    pub fn object_count(&self) -> usize {
        self.objects.lock().iter().filter(|o| o.is_some()).count()
    }

    /// The number of default objects.
    pub fn default_count(&self) -> usize {
        self.defaults.lock().len()
    }
}
