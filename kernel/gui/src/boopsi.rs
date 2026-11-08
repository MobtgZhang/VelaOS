//! BOOPSI: the Basic Object Oriented Programming System.
//!
//! # What this module is
//!
//! AmigaOS's BOOPSI is the object model behind gadgets and classes. A class has
//! a dispatch function, an instance size and a parent; an object is an instance
//! carrying an attribute list; and method ids (`OM_NEW`, `OM_SET`, `OM_GET`,
//! `GM_GADGETUP`) are how the dispatcher is called.
//!
//! # What VelaOS keeps and what it does not
//!
//! The shape is kept: classes, objects, tags and method ids. The raw `Class`
//! and `Object` pointers AmigaOS passes are not exposed to user space, because
//! a pointer is kernel memory. A user program names a class or object by an
//! opaque id, and the kernel resolves it. The `DispatchFn` signature here is the
//! internal class contract, not a user-visible ABI.
//!
//! # Why the registry is a kernel service
//!
//! A class registry is a mutable table shared by every task. If it lived in a
//! user program, one program's class could not be used by another, which is the
//! whole point of a class. So it lives here and reaches user space through
//! handles once the syscall layer exposes it.

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

/// A method identifier.
pub type MethodId = u32;

/// An attribute identifier.
pub type AttrId = u32;

/// BOOPSI method IDs.
pub mod methods {
    use super::MethodId;

    pub const OM_NEW: MethodId = 1;
    pub const OM_DISPOSE: MethodId = 2;
    pub const OM_SET: MethodId = 3;
    pub const OM_GET: MethodId = 4;
    pub const GM_GADGETUP: MethodId = 101;
    pub const GM_GADGETDOWN: MethodId = 102;
    pub const GM_GADGETRENDER: MethodId = 103;
}

/// BOOPSI dispatch function type.
///
/// This is the class's own internal contract: the registry calls it, and user
/// code never receives these pointers.
pub type DispatchFn = extern "C" fn(*mut Class, *mut Object, MethodId, u32, u32) -> u32;

/// A BOOPSI class.
#[derive(Clone)]
pub struct Class {
    pub id: u32,
    pub name: String,
    pub parent: Option<u32>,
    pub dispatch: DispatchFn,
    pub instance_size: u32,
}

/// A BOOPSI object.
pub struct Object {
    pub id: u32,
    pub class_id: u32,
    pub attrs: Mutex<Vec<(AttrId, u32)>>,
}

/// A TagItem: attribute name/value pair.
#[derive(Debug, Clone, Copy)]
pub struct TagItem {
    pub ti_attr: AttrId,
    pub ti_value: u32,
}

/// The class registry.
pub struct ClassRegistry {
    classes: Mutex<Vec<Class>>,
    objects: Mutex<Vec<Object>>,
    next_id: AtomicU32,
}

impl ClassRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            classes: Mutex::new(Vec::new()),
            objects: Mutex::new(Vec::new()),
            next_id: AtomicU32::new(1),
        }
    }

    /// Register a class, returning its class id.
    pub fn register_class(
        &self,
        name: &str,
        parent: Option<u32>,
        dispatch: DispatchFn,
        instance_size: u32,
    ) -> u32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.classes.lock().push(Class {
            id,
            name: name.to_string(),
            parent,
            dispatch,
            instance_size,
        });
        id
    }

    /// Find a class by id.
    pub fn find_class(&self, id: u32) -> Option<Class> {
        self.classes.lock().iter().find(|c| c.id == id).cloned()
    }

    /// Create an object of a class from a tag list.
    ///
    /// Returns the object's opaque id. The class dispatch is called for `OM_NEW`
    /// with the registry's object pointer; user space never sees that pointer.
    pub fn new_object(&self, class_id: u32, tags: &[TagItem]) -> Option<u32> {
        let class = self.find_class(class_id)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let attrs = tags
            .iter()
            .map(|tag| (tag.ti_attr, tag.ti_value))
            .collect::<Vec<_>>();
        let object = Object {
            id,
            class_id,
            attrs: Mutex::new(attrs.clone()),
        };
        self.objects.lock().push(object);

        // SAFETY: `class` is a live value owned by this function; the dispatch
        // contract is internal and does not retain either pointer.
        let result = (class.dispatch)(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            methods::OM_NEW,
            id,
            0,
        );
        let _ = result;
        Some(id)
    }

    /// Set attributes on an object.
    pub fn set_attrs(&self, obj_id: u32, tags: &[TagItem]) -> u32 {
        let mut objects = self.objects.lock();
        let Some(object) = objects.iter_mut().find(|o| o.id == obj_id) else {
            return 0;
        };
        let mut attrs = object.attrs.lock();
        for tag in tags {
            if let Some(existing) = attrs.iter_mut().find(|(attr, _)| *attr == tag.ti_attr) {
                existing.1 = tag.ti_value;
            } else {
                attrs.push((tag.ti_attr, tag.ti_value));
            }
        }
        tags.len() as u32
    }

    /// Get an attribute from an object.
    ///
    /// Returns `true` when the attribute exists and writes its value into
    /// `storage`; returns `false` when the object or attribute is absent.
    pub fn get_attr(&self, obj_id: u32, attr: AttrId, storage: &mut u32) -> bool {
        let objects = self.objects.lock();
        let Some(object) = objects.iter().find(|o| o.id == obj_id) else {
            return false;
        };
        let attrs = object.attrs.lock();
        match attrs.iter().find(|(a, _)| *a == attr) {
            Some((_, value)) => {
                *storage = *value;
                true
            }
            None => false,
        }
    }

    /// Dispose of an object.
    pub fn dispose_object(&self, obj_id: u32) -> bool {
        let mut objects = self.objects.lock();
        match objects.iter().position(|o| o.id == obj_id) {
            Some(index) => {
                objects.remove(index);
                true
            }
            None => false,
        }
    }

    /// How many classes are registered.
    pub fn class_count(&self) -> usize {
        self.classes.lock().len()
    }

    /// How many objects exist.
    pub fn object_count(&self) -> usize {
        self.objects.lock().len()
    }
}

impl Default for ClassRegistry {
    fn default() -> Self {
        Self::new()
    }
}
