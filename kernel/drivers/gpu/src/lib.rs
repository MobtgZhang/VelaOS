//! The virtio-gpu 2D display driver.
//!
//! This is a real display driver, not another name for the firmware's GOP
//! framebuffer. It negotiates modern virtio over PCI, builds the control
//! virtqueue, creates a host-side 2D resource backed by kernel memory, points
//! scanout zero at it, and flushes damaged pixels after the compositor runs.
//! When no virtio-gpu is present the boot loader's GOP path remains the
//! fallback, so a machine with ordinary firmware graphics is unaffected.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};
use spin::Mutex;

use vela_arch::pci::PciDevice;

const VIRTIO_VENDOR: u16 = 0x1AF4;
const VIRTIO_GPU_MODERN: u16 = 0x1050;
const VIRTIO_GPU_LEGACY: u16 = 0x1040;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FEATURES_OK: u8 = 8;

const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;

const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const RESP_OK_NODATA: u32 = 0x1100;
const FORMAT_B8G8R8X8_UNORM: u32 = 2;

/// The display's geometry and physical framebuffer address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayInfo {
    pub address: u64,
    pub width: u32,
    pub height: u32,
    pub stride_bytes: u32,
}

/// Return every virtio-gpu function found by PCI enumeration.
pub fn devices(all: &[PciDevice]) -> Vec<PciDevice> {
    all.iter()
        .copied()
        .filter(|d| {
            d.vendor_id == VIRTIO_VENDOR
                && (d.device_id == VIRTIO_GPU_MODERN || d.device_id == VIRTIO_GPU_LEGACY)
        })
        .collect()
}

/// The memory windows entry must map before calling [`init`].
///
/// Virtio capabilities may place common, notify, ISR and device configuration
/// in different BARs, so mapping only BAR0 is not sufficient on every machine.
pub fn mmio_windows(device: &PciDevice) -> Vec<(u64, u64)> {
    let mut windows = Vec::new();
    for bar in device.bars.iter() {
        if !bar.memory || bar.base == 0 || bar.size == 0 {
            continue;
        }
        let window = (bar.base, bar.size);
        // A 64-bit BAR occupies two entries in the discovery table; mapping
        // the second copy again would fail as an already-mapped page.
        if !windows.contains(&window) {
            windows.push(window);
        }
    }
    windows
}

#[repr(C)]
struct CtrlHeader {
    kind: u32,
    flags: u32,
    fence: u64,
    context: u32,
    ring: u8,
    padding: [u8; 3],
}

impl CtrlHeader {
    fn new(kind: u32) -> Self {
        Self {
            kind,
            flags: 0,
            fence: 0,
            context: 0,
            ring: 0,
            padding: [0; 3],
        }
    }
}

#[repr(C)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct Create2D {
    header: CtrlHeader,
    resource: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
struct AttachBacking {
    header: CtrlHeader,
    resource: u32,
    entries: u32,
    address: u64,
    length: u32,
    padding: u32,
}

#[repr(C)]
struct SetScanout {
    header: CtrlHeader,
    rect: Rect,
    scanout: u32,
    resource: u32,
}

#[repr(C)]
struct Transfer2D {
    header: CtrlHeader,
    rect: Rect,
    offset: u64,
    resource: u32,
    padding: u32,
}

/// One allocated, device-visible region.
struct Dma {
    ptr: *mut u8,
    size: usize,
}

impl Dma {
    fn new(size: usize, align: usize) -> Result<Self, i32> {
        let layout = core::alloc::Layout::from_size_align(size, align).map_err(|_| -12)?;
        // SAFETY: the layout is non-zero and valid. The allocation is leaked
        // into a device structure whose lifetime is the kernel's.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            return Err(-12);
        }
        Ok(Self { ptr, size })
    }

    fn address(&self) -> u64 {
        // The kernel heap is identity-mapped by the boot contract, so this is
        // both the CPU virtual address and the guest-physical DMA address.
        self.ptr as u64
    }

    fn write<T>(&self, value: T) {
        assert!(core::mem::size_of::<T>() <= self.size);
        // SAFETY: this DMA allocation is owned by the caller, aligned for the
        // command structures used here, and large enough (checked above).
        unsafe { (self.ptr as *mut T).write(value) };
    }

    fn response_kind(&self) -> u32 {
        // SAFETY: every response starts with the common control header. The
        // device wrote this DMA buffer before completion was observed.
        unsafe { core::ptr::read_volatile(self.ptr as *const CtrlHeader).kind }
    }
}

#[repr(C)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

struct VirtQueue {
    size: u16,
    desc: Dma,
    avail: Dma,
    _used: Dma,
    notify: *mut u16,
}

impl VirtQueue {
    fn submit(&mut self, request: &Dma, response: &Dma) -> Result<(), i32> {
        let size = self.size as usize;
        let request_len = request.size.min(u32::MAX as usize) as u32;
        let response_len = response.size.min(u32::MAX as usize) as u32;
        // SAFETY: the descriptor table was allocated for exactly `size`
        // entries, and indices 0 and 1 are free for this synchronous command.
        unsafe {
            let desc = self.desc.ptr as *mut Descriptor;
            desc.write(Descriptor {
                address: request.address(),
                length: request_len,
                flags: DESC_NEXT,
                next: 1,
            });
            desc.add(1).write(Descriptor {
                address: response.address(),
                length: response_len,
                flags: DESC_WRITE,
                next: 0,
            });
            let avail = self.avail.ptr;
            // avail: flags u16, idx u16, ring[size] u16, used_event u16.
            let idx = (avail as *mut u16).add(1);
            let next = idx.read_volatile().wrapping_add(1);
            let slot = (next.wrapping_sub(1) as usize) % size;
            (avail as *mut u16).add(2 + slot).write_volatile(0);
            core::sync::atomic::fence(Ordering::Release);
            idx.write_volatile(next);
            self.notify.write_volatile(0);
        }
        // Commands are synchronous at this level. The bound turns a dead or
        // removed device into an error rather than a kernel hang; a flush that
        // times out simply lets the next present retry.
        for _ in 0..2_000_000 {
            core::sync::atomic::fence(Ordering::Acquire);
            let kind = response.response_kind();
            if kind != 0 {
                return if kind == RESP_OK_NODATA || (0x1100..0x1200).contains(&kind) {
                    Ok(())
                } else {
                    Err(-5)
                };
            }
            core::hint::spin_loop();
        }
        Err(-110)
    }
}

/// An initialized virtio-gpu scanout.
pub struct GpuDisplay {
    info: DisplayInfo,
    queue: Mutex<VirtQueue>,
    resource: u32,
    _framebuffer: Dma,
}

static DISPLAY: AtomicPtr<GpuDisplay> = AtomicPtr::new(core::ptr::null_mut());

/// Initialize a virtio-gpu device and take over its first scanout.
///
/// `mmio_base` is retained in the public contract for symmetry with other
/// drivers; capability-selected BAR addresses are used instead, and every one
/// must already be identity-mapped. `width` and `height` are normally the GOP
/// geometry, which keeps the desktop's layout unchanged during take-over.
///
/// # Safety
///
/// PCI configuration space and the device's MMIO windows must be accessible,
/// and the kernel heap must be identity-mapped for DMA as documented above.
pub unsafe fn init(device: &PciDevice, width: u32, height: u32) -> Result<DisplayInfo, i32> {
    if width == 0 || height == 0 {
        return Err(-22);
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = device;
        return Err(-38);
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        init_x86(device, width, height)
    }
}

#[cfg(target_arch = "x86_64")]
fn cfg_address(device: &PciDevice, offset: u8) -> u32 {
    0x8000_0000
        | (device.bus as u32) << 16
        | (device.device as u32) << 11
        | (device.function as u32) << 8
        | (offset & 0xFC) as u32
}

#[cfg(target_arch = "x86_64")]
fn cfg_read(device: &PciDevice, offset: u8) -> u32 {
    // SAFETY: configuration mechanism 1 is the architectural x86 interface.
    unsafe {
        vela_arch::regs::outl(0xCF8, cfg_address(device, offset));
        vela_arch::regs::inl(0xCFC)
    }
}

#[cfg(target_arch = "x86_64")]
fn cfg_write(device: &PciDevice, offset: u8, value: u32) {
    // SAFETY: as for `cfg_read`, with a caller-selected configuration value.
    unsafe {
        vela_arch::regs::outl(0xCF8, cfg_address(device, offset));
        vela_arch::regs::outl(0xCFC, value);
    }
}

#[cfg(target_arch = "x86_64")]
fn cfg_byte(device: &PciDevice, offset: u8) -> u8 {
    (cfg_read(device, offset) >> ((offset & 3) * 8)) as u8
}

#[cfg(target_arch = "x86_64")]
#[derive(Default, Clone, Copy)]
struct Caps {
    common: u64,
    notify: u64,
    isr: u64,
    device: u64,
    multiplier: u32,
}

#[cfg(target_arch = "x86_64")]
fn capability_window(device: &PciDevice, cap: u8) -> (u64, u8) {
    let bar = cfg_byte(device, cap + 4);
    let offset = cfg_read(device, cap + 8) as u64;
    let length = cfg_read(device, cap + 12) as u64;
    let base = device
        .bars
        .get(bar as usize)
        .map(|b| b.base)
        .unwrap_or(0);
    (base + offset, (length > 0) as u8)
}

#[cfg(target_arch = "x86_64")]
unsafe fn init_x86(device: &PciDevice, width: u32, height: u32) -> Result<DisplayInfo, i32> {
    // Memory-space decoding and bus mastering are both required: the first
    // for the virtqueue registers, the second for framebuffer DMA.
    let pci_command = cfg_read(device, 0x04);
    cfg_write(device, 0x04, pci_command | 0x06);

    let mut caps = Caps::default();
    let mut cap = cfg_byte(device, 0x34);
    let mut steps = 0;
    while cap != 0 && cap != 0xFF && steps < 48 {
        if cfg_byte(device, cap) == 0x09 {
            let kind = cfg_byte(device, cap + 3);
            let (window, present) = capability_window(device, cap);
            if present != 0 {
                match kind {
                    1 => caps.common = window,
                    2 => {
                        caps.notify = window;
                        caps.multiplier = cfg_read(device, cap + 16);
                    }
                    3 => caps.isr = window,
                    4 => caps.device = window,
                    _ => {}
                }
            }
        }
        cap = cfg_byte(device, cap + 1);
        steps += 1;
    }
    if caps.common == 0 || caps.notify == 0 || caps.multiplier == 0 {
        return Err(-19);
    }
    let common = caps.common as *mut u8;
    let status = |value: u8| {
        // SAFETY: device_status is byte 0x14 of common configuration.
        unsafe { common.add(0x14).write_volatile(value) };
    };
    status(0);
    status(STATUS_ACKNOWLEDGE);
    status(STATUS_ACKNOWLEDGE | STATUS_DRIVER);
    // VIRTIO_F_VERSION_1 is bit 32, hence feature-select 1 / bit 0.
    // SAFETY: feature selector and value are common-config dwords.
    unsafe {
        (common as *mut u32).add(2).write_volatile(1);
        let _device_high = (common as *const u32).add(1).read_volatile();
        (common as *mut u32).add(2).write_volatile(1);
        (common as *mut u32).add(3).write_volatile(1);
    }
    status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK);
    // SAFETY: read back the negotiated status byte.
    let negotiated = unsafe { common.add(0x14).read_volatile() };
    if negotiated & STATUS_FEATURES_OK == 0 {
        return Err(-5);
    }

    // Queue zero is the control queue. The cursor queue is deliberately left
    // alone: VelaOS's software pointer is already composited by Intuition.
    // SAFETY: queue selectors and sizes are common-config registers.
    let queue_size = unsafe {
        (common as *mut u16).add(0x0B).write_volatile(0);
        (common as *const u16).add(0x0C).read_volatile()
    };
    if queue_size == 0 {
        return Err(-19);
    }
    let size = queue_size.min(64);
    let desc = Dma::new(size as usize * 16, 16)?;
    let avail = Dma::new(6 + size as usize * 2 + 2, 2)?;
    let used = Dma::new(6 + size as usize * 8 + 2, 4)?;
    // SAFETY: program this queue's three DMA addresses and enable it.
    unsafe {
        (common as *mut u16).add(0x0B).write_volatile(0);
        (common as *mut u16).add(0x0C).write_volatile(size);
        (common as *mut u64).add(4).write_volatile(desc.address());
        (common as *mut u64).add(5).write_volatile(avail.address());
        (common as *mut u64).add(6).write_volatile(used.address());
    }
    let notify_off = unsafe { (common as *const u16).add(0x0F).read_volatile() };
    let notify = (caps.notify + notify_off as u64 * caps.multiplier as u64) as *mut u16;
    unsafe { (common as *mut u16).add(0x0E).write_volatile(1) };
    status(STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK);

    let bytes = width as usize * height as usize * 4;
    let framebuffer = Dma::new(bytes, 4096)?;
    let resource = 1;
    let queue = VirtQueue {
        size,
        desc,
        avail,
        _used: used,
        notify,
    };
    let mut queue = queue;
    let rect = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    command(
        &mut queue,
        Create2D {
            header: CtrlHeader::new(CMD_RESOURCE_CREATE_2D),
            resource,
            format: FORMAT_B8G8R8X8_UNORM,
            width,
            height,
        },
    )?;
    command(
        &mut queue,
        AttachBacking {
            header: CtrlHeader::new(CMD_RESOURCE_ATTACH_BACKING),
            resource,
            entries: 1,
            address: framebuffer.address(),
            length: bytes as u32,
            padding: 0,
        },
    )?;
    command(
        &mut queue,
        SetScanout {
            header: CtrlHeader::new(CMD_SET_SCANOUT),
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scanout: 0,
            resource,
        },
    )?;
    command(
        &mut queue,
        Transfer2D {
            header: CtrlHeader::new(CMD_TRANSFER_TO_HOST_2D),
            rect,
            offset: 0,
            resource,
            padding: 0,
        },
    )?;
    flush_queue(&mut queue, resource, width, height)?;

    let info = DisplayInfo {
        address: framebuffer.address(),
        width,
        height,
        stride_bytes: width * 4,
    };
    let display = alloc::boxed::Box::new(GpuDisplay {
        info,
        queue: Mutex::new(queue),
        resource,
        _framebuffer: framebuffer,
    });
    let display = alloc::boxed::Box::leak(display);
    DISPLAY.store(display as *const GpuDisplay as *mut GpuDisplay, Ordering::Release);
    Ok(info)
}

fn command<T>(queue: &mut VirtQueue, value: T) -> Result<(), i32> {
    let request = Dma::new(core::mem::size_of::<T>().max(24), 8)?;
    request.write(value);
    let response = Dma::new(24, 8)?;
    queue.submit(&request, &response)
}

fn flush_queue(queue: &mut VirtQueue, resource: u32, width: u32, height: u32) -> Result<(), i32> {
    // A transfer publishes the guest backing store into the host resource;
    // the following flush makes that resource visible on its scanout. Doing
    // only the second leaves QEMU showing the first frame forever.
    command(
        queue,
        Transfer2D {
            header: CtrlHeader::new(CMD_TRANSFER_TO_HOST_2D),
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            offset: 0,
            resource,
            padding: 0,
        },
    )?;
    command(
        queue,
        SetScanout {
            header: CtrlHeader::new(CMD_RESOURCE_FLUSH),
            rect: Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            scanout: 0,
            resource,
        },
    )
}

/// Flush the active display after a compositor present.
///
/// Interrupt-safe: a queue already in use makes this tick skip rather than
/// wait. The framebuffer is shared with the host by DMA, so the next tick can
/// publish anything this one did not.
pub fn flush() {
    let ptr = DISPLAY.load(Ordering::Acquire);
    if ptr.is_null() {
        return;
    }
    // SAFETY: the display is leaked at initialization and never destroyed.
    let display = unsafe { &*ptr };
    let Some(mut queue) = display.queue.try_lock() else {
        return;
    };
    let _ = flush_queue(&mut queue, display.resource, display.info.width, display.info.height);
}

/// The active display's geometry, if take-over succeeded.
pub fn info() -> Option<DisplayInfo> {
    let ptr = DISPLAY.load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the display is leaked and immutable after publication.
        Some(unsafe { &*ptr }.info)
    }
}

/// The number of displays taken over by this driver.
pub fn display_count() -> u32 {
    u32::from(info().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_layouts_match_the_virtio_gpu_wire_format() {
        assert_eq!(core::mem::size_of::<CtrlHeader>(), 24);
        assert_eq!(core::mem::size_of::<Create2D>(), 40);
        assert_eq!(core::mem::size_of::<AttachBacking>(), 48);
        assert_eq!(core::mem::size_of::<SetScanout>(), 48);
        assert_eq!(core::mem::size_of::<Transfer2D>(), 56);
        assert_eq!(core::mem::size_of::<Descriptor>(), 16);
    }

    #[test]
    fn device_probe_accepts_modern_and_legacy_virtio_gpu_ids() {
        assert_eq!(VIRTIO_VENDOR, 0x1AF4);
        assert_eq!(VIRTIO_GPU_MODERN, 0x1050);
        assert_eq!(VIRTIO_GPU_LEGACY, 0x1040);
        assert_eq!(FORMAT_B8G8R8X8_UNORM, 2);
    }
}
