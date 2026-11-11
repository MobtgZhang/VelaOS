//! Audio drivers: HDA (Intel HD Audio), AC97, and null backend.
//!
//! # What this crate is
//!
//! It registers audio devices with the Device Manager and provides PCM
//! playback backends. The null backend always accepts data (for testing
//! the mixer without real hardware). HDA and AC97 are stubs that register
//! as devices but report "no hardware present" on QEMU.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

use vela_kernel_device::{cmd, errors, DeviceInfo, DriverOps, IORequest, IoState};

/// Helper: extract base class from `DeviceInfo::class_code`.
fn base_class(class_code: u32) -> u8 {
    ((class_code >> 16) & 0xFF) as u8
}

/// The null audio backend: accepts all PCM data but produces no sound.
/// Used for testing the mixer end-to-end without real hardware.
pub struct NullAudioBackend;

impl NullAudioBackend {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for NullAudioBackend {
    fn probe(&self, _info: &DeviceInfo) -> bool {
        true // null backend is always available.
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Ok(0x4000_0000)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        match req.io_command {
            cmd::STATUS => {
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
                req.io_error = errors::OK;
                Ok(())
            }
            cmd::WRITE => {
                // Accept the data but discard it (null sink).
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
                req.io_error = errors::OK;
                Ok(())
            }
            _ => {
                req.io_state = IoState::Aborted;
                Err(errors::BAD_CMD)
            }
        }
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// Intel HDA (High Definition Audio) driver stub.
pub struct HdaDriver;

impl HdaDriver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for HdaDriver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // HDA: vendor varies, class 0x04 (multimedia), subclass 0x03 (audio).
        info.bus == "pci" && base_class(info.class_code) == 0x04 && (info.class_code & 0xFF) == 0x03
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Err(errors::NO_DEVICE) // No real hardware present on QEMU.
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        req.io_state = IoState::Aborted;
        Err(errors::BAD_CMD)
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// AC97 audio driver stub.
pub struct Ac97Driver;

impl Ac97Driver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl DriverOps for Ac97Driver {
    fn probe(&self, info: &DeviceInfo) -> bool {
        // AC97: vendor varies, class 0x04 (multimedia), subclass 0x01 (audio).
        info.bus == "pci" && base_class(info.class_code) == 0x04 && (info.class_code & 0xFF) == 0x01
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Err(errors::NO_DEVICE)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        req.io_state = IoState::Aborted;
        Err(errors::BAD_CMD)
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

/// Register all audio drivers with the Device Manager.
pub fn register_all() {
    let null = NullAudioBackend::new();
    vela_kernel_device::DeviceManager::register_driver(
        "audio.null",
        Arc::clone(&null) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("audio.null", 0, 0x4000_0000);

    let hda = HdaDriver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "audio.hda",
        Arc::clone(&hda) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("audio.hda", 0, 0);

    let ac97 = Ac97Driver::new();
    vela_kernel_device::DeviceManager::register_driver(
        "audio.ac97",
        Arc::clone(&ac97) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("audio.ac97", 0, 0);

    // The generic `audio.device`: a user-facing PCM device that mixes through
    // the global audio mixer. Programs open it with OpenDevice("audio.device",
    // unit) and write PCM sample frames (s16 LE stereo) to play sound.
    let audio_device = AudioDevice::new();
    vela_kernel_device::DeviceManager::register_driver(
        "audio.device",
        Arc::clone(&audio_device) as Arc<dyn DriverOps>,
    );
    vela_kernel_device::DeviceManager::register_device("audio.device", 0, 0);
}

/// The audio mixer service: mixes multiple PCM streams into one output.
pub struct AudioMixer {
    sample_rate: u32,
    channels: u16,
    /// The next stream id.
    next_stream: AtomicU32,
    /// Active streams.
    streams: Mutex<Vec<AudioStream>>,
}

/// One PCM audio stream.
struct AudioStream {
    id: u32,
    sample_rate: u32,
    channels: u16,
    format: u16,
    volume_q16: AtomicU32, // 0x10000 = 1.0
    paused: AtomicU32,
    /// PCM data waiting to be mixed (s16 LE stereo frames).
    buffer: Mutex<Vec<i16>>,
}

impl AudioMixer {
    pub fn new(sample_rate: u32, channels: u16) -> Arc<Self> {
        Arc::new(Self {
            sample_rate,
            channels,
            next_stream: AtomicU32::new(1),
            streams: Mutex::new(Vec::new()),
        })
    }

    /// The mixer's output sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The mixer's output channel count.
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Open a new PCM stream.
    pub fn open_stream(&self, sample_rate: u32, channels: u16, format: u16) -> u32 {
        let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
        let stream = AudioStream {
            id,
            sample_rate,
            channels,
            format,
            volume_q16: AtomicU32::new(0x10000),
            paused: AtomicU32::new(0),
            buffer: Mutex::new(Vec::new()),
        };
        self.streams.lock().push(stream);
        id
    }

    /// Write PCM frames to a stream.
    pub fn write_stream(&self, id: u32, pcm: &[i16]) -> Result<usize, i32> {
        let mut streams = self.streams.lock();
        let stream = streams
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(errors::NO_DEVICE)?;
        if stream.paused.load(Ordering::Acquire) != 0 {
            return Err(errors::BUSY);
        }
        let mut buf = stream.buffer.lock();
        let old_len = buf.len();
        buf.extend_from_slice(pcm);
        Ok(buf.len() - old_len)
    }

    /// Set stream volume (Q16 fixed point, 0x10000 = 1.0).
    pub fn set_stream_volume(&self, id: u32, volume_q16: u32) -> Result<(), i32> {
        let streams = self.streams.lock();
        let stream = streams
            .iter()
            .find(|s| s.id == id)
            .ok_or(errors::NO_DEVICE)?;
        stream.volume_q16.store(volume_q16, Ordering::Release);
        Ok(())
    }

    /// Mix `out_frames` frames of stereo output into `out`.
    pub fn mix(&self, out: &mut [i16]) {
        let streams = self.streams.lock();
        let frames = out.len() / 2; // stereo frames
        for i in 0..frames {
            let mut acc_l: i32 = 0;
            let mut acc_r: i32 = 0;
            let mut active = 0;
            for s in streams.iter() {
                if s.paused.load(Ordering::Acquire) != 0 {
                    continue;
                }
                let buf = s.buffer.lock();
                if buf.len() < (i + 1) * 2 {
                    continue;
                }
                let vol = s.volume_q16.load(Ordering::Acquire) as i32;
                let l = buf[i * 2] as i32 * vol / 0x10000;
                let r = buf[i * 2 + 1] as i32 * vol / 0x10000;
                acc_l += l;
                acc_r += r;
                active += 1;
                drop(buf);
            }
            if active > 0 {
                // Clamp to i16 range.
                let l = acc_l.clamp(-32768, 32767) as i16;
                let r = acc_r.clamp(-32768, 32767) as i16;
                out[i * 2] = l;
                out[i * 2 + 1] = r;
            } else {
                out[i * 2] = 0;
                out[i * 2 + 1] = 0;
            }
        }
        // Consume mixed frames from all streams.
        let mut streams = self.streams.lock();
        for s in streams.iter_mut() {
            if s.paused.load(Ordering::Acquire) != 0 {
                continue;
            }
            let mut buf = s.buffer.lock();
            if buf.len() >= frames * 2 {
                buf.drain(..frames * 2);
            }
        }
    }

    /// Pause/resume a stream.
    pub fn set_stream_paused(&self, id: u32, paused: bool) -> Result<(), i32> {
        let mut streams = self.streams.lock();
        let stream = streams
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or(errors::NO_DEVICE)?;
        stream
            .paused
            .store(if paused { 1 } else { 0 }, Ordering::Release);
        Ok(())
    }

    /// A stream's declared sample rate, channel count and format.
    pub fn stream_info(&self, id: u32) -> Option<(u32, u16, u16)> {
        let streams = self.streams.lock();
        streams
            .iter()
            .find(|s| s.id == id)
            .map(|s| (s.sample_rate, s.channels, s.format))
    }

    /// How many streams exist.
    pub fn stream_count(&self) -> usize {
        self.streams.lock().len()
    }
}

/// The global audio mixer.
///
/// `AudioMixer::new` returns an `Arc`, because a mixer is shared by the audio
/// device service and by whichever task writes PCM. The `Once` therefore stores
/// the `Arc`, not a bare mixer.
static AUDIO_MIXER: spin::Once<Arc<AudioMixer>> = spin::Once::new();

/// Get the global audio mixer.
pub fn audio_mixer() -> &'static AudioMixer {
    AUDIO_MIXER
        .get()
        .expect("audio mixer not installed; kernel/entry must call init_audio_mixer")
        .as_ref()
}

/// Install the global audio mixer.
///
/// # Safety
///
/// Must be called once during boot, before any task writes PCM. `call_once`
/// makes repeated calls harmless, but the mixer is a process-wide singleton.
pub unsafe fn init_audio_mixer(sample_rate: u32, channels: u16) {
    AUDIO_MIXER.call_once(|| AudioMixer::new(sample_rate, channels));
}

// ---------------------------------------------------------------------------
// audio.device: user-facing PCM device
// ---------------------------------------------------------------------------

/// The generic `audio.device`: programs open it with
/// `OpenDevice("audio.device", unit)` and write PCM sample frames
/// (s16 LE stereo) to play sound.
///
/// The device allocates a mixer stream per `io_unit` value and mixes all
/// active streams through the global audio mixer.
pub struct AudioDevice {
    next_stream: AtomicU32,
}

impl AudioDevice {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            next_stream: AtomicU32::new(1),
        })
    }
}

impl DriverOps for AudioDevice {
    fn probe(&self, _info: &DeviceInfo) -> bool {
        true
    }

    fn open(&self, _device: u64, _unit: u32) -> Result<u64, i32> {
        Ok(0x7000_0000)
    }

    fn submit(&self, _cookie: u64, req: &mut IORequest) -> Result<(), i32> {
        let mixer = audio_mixer();
        match req.io_command {
            cmd::STATUS => {
                req.io_state = IoState::Done;
                req.io_actual = 0;
                req.io_error = errors::OK;
                Ok(())
            }
            cmd::WRITE => {
                // Write PCM sample frames (s16 LE stereo) to a mixer stream.
                let stream_id = req.io_unit;
                let frame_count = req.io_length as usize / 4;
                if frame_count == 0 {
                    req.io_state = IoState::Done;
                    req.io_actual = 0;
                    req.io_error = errors::OK;
                    return Ok(());
                }
                // Allocate a stream for this unit on first write.
                let _ = mixer.open_stream(
                    44100, // DEFAULT_SAMPLE_RATE
                    2,     // stereo
                    0,     // format: s16 LE stereo
                );
                let buf = unsafe {
                    core::slice::from_raw_parts(req.io_data as *const i16, frame_count * 2)
                };
                mixer.write_stream(stream_id, buf).map_err(|_| errors::BUSY)?;
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
                req.io_error = errors::OK;
                Ok(())
            }
            cmd::READ => {
                // Read mixed output from the mixer into the caller's buffer.
                let frame_count = req.io_length as usize / 4;
                if frame_count == 0 {
                    req.io_state = IoState::Done;
                    req.io_actual = 0;
                    req.io_error = errors::OK;
                    return Ok(());
                }
                let mut out = vec![0i16; frame_count * 2];
                mixer.mix(&mut out);
                // Copy back to the caller's buffer.
                let dst = unsafe {
                    core::slice::from_raw_parts_mut(req.io_data as *mut i16, frame_count * 2)
                };
                dst.copy_from_slice(&out);
                req.io_state = IoState::Done;
                req.io_actual = req.io_length;
                req.io_error = errors::OK;
                Ok(())
            }
            _ => {
                req.io_state = IoState::Aborted;
                Err(errors::BAD_CMD)
            }
        }
    }

    fn cancel(&self, _cookie: u64, _req: &mut IORequest) -> bool {
        false
    }

    fn remove(&self, _cookie: u64) {}
}

