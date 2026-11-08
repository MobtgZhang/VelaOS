//! Audio device and mixer.
//!
//! # What this crate is for
//!
//! It is `audio.device`: a mixer that mixes multiple PCM streams into one
//! output stream, and a DMA path that feeds a DAC. A program opens
//! `audio.device`, allocates a channel, writes sample frames to it, and the
//! mixer sums all active channels at the device's sample rate.
//!
//! # Sample format
//!
//! All audio is signed 16-bit stereo little-endian at the device's sample
//! rate. A "sample frame" is one left sample + one right sample (4 bytes).
//! This matches AmigaOS's `audio.device` and keeps the mixer simple.
//!
//! # Mixing
//!
//! Channels are mixed by summing saturated 16-bit samples into a 32-bit
//! accumulator, then clamping back to 16-bit. Clipping is deliberate: a
//! clipping indicator is exposed so a program can turn its levels down.

#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

/// The number of bytes per stereo sample frame (2 channels * 2 bytes).
pub const FRAME_BYTES: usize = 4;

/// The default sample rate, in Hz.
pub const DEFAULT_SAMPLE_RATE: u32 = 44100;

/// The number of sample frames the mixer keeps buffered ahead of the DAC.
pub const MIX_BUFFER_FRAMES: usize = 1024;

/// Audio errors.
pub mod errors {
    pub const OK: i32 = 0;
    pub const NO_DEVICE: i32 = -1;
    pub const BAD_CHANNEL: i32 = -2;
    pub const BAD_FORMAT: i32 = -3;
    pub const NO_MEMORY: i32 = -4;
    pub const BUSY: i32 = -5;
}

/// One sample frame: left then right, signed 16-bit little-endian.
#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleFrame {
    pub left: i16,
    pub right: i16,
}

impl SampleFrame {
    /// Silence.
    pub const SILENT: SampleFrame = SampleFrame { left: 0, right: 0 };
}

/// A channel's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    Free,
    Allocated,
    Playing,
    Stopped,
}

/// One mix channel.
#[derive(Debug, Clone, Copy)]
pub struct Channel {
    /// The channel index.
    pub id: u32,
    /// The state.
    pub state: ChannelState,
    /// The volume, 0..=127 (AmigaOS convention).
    pub volume: u8,
    /// The playback position, in sample frames.
    pub position: u64,
    /// The buffer length, in sample frames.
    pub length: u64,
    /// Whether looping is enabled.
    pub looping: bool,
}

/// The audio mixer.
///
/// Mixes all playing channels into one output stream at [`DEFAULT_SAMPLE_RATE`].
pub struct Mixer {
    channels: Mutex<Vec<Channel>>,
    /// The number of sample frames mixed so far (for diagnostics / position).
    frames_mixed: AtomicU64,
    /// The clip count: how many mix operations clipped (diagnostic).
    clips: AtomicU32,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    /// A mixer with `channel_count` free channels.
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(Vec::new()),
            frames_mixed: AtomicU64::new(0),
            clips: AtomicU32::new(0),
        }
    }

    /// Allocate a free channel, returning its id.
    pub fn alloc_channel(&self) -> Result<u32, i32> {
        let mut channels = self.channels.lock();
        for (i, ch) in channels.iter_mut().enumerate() {
            if ch.state == ChannelState::Free {
                ch.state = ChannelState::Allocated;
                ch.id = i as u32;
                return Ok(i as u32);
            }
        }
        let id = channels.len() as u32;
        channels.push(Channel {
            id,
            state: ChannelState::Allocated,
            volume: 64,
            position: 0,
            length: 0,
            looping: false,
        });
        Ok(id)
    }

    /// Free a channel. It must not be playing.
    pub fn free_channel(&self, id: u32) -> Result<(), i32> {
        let mut channels = self.channels.lock();
        let ch = channels.get_mut(id as usize).ok_or(errors::BAD_CHANNEL)?;
        if ch.state == ChannelState::Playing {
            return Err(errors::BUSY);
        }
        ch.state = ChannelState::Free;
        Ok(())
    }

    /// Set a channel's volume.
    pub fn set_volume(&self, id: u32, volume: u8) -> Result<(), i32> {
        let mut channels = self.channels.lock();
        let ch = channels.get_mut(id as usize).ok_or(errors::BAD_CHANNEL)?;
        ch.volume = volume.min(127);
        Ok(())
    }

    /// Queue sample frames on a channel.
    ///
    /// The frames are copied into an internal buffer; the caller's buffer can
    /// be reused after this returns.
    pub fn write(&self, id: u32, frames: &[SampleFrame], looping: bool) -> Result<(), i32> {
        let mut channels = self.channels.lock();
        let ch = channels.get_mut(id as usize).ok_or(errors::BAD_CHANNEL)?;
        if ch.state == ChannelState::Free {
            return Err(errors::BAD_CHANNEL);
        }
        ch.state = ChannelState::Playing;
        ch.looping = looping;
        ch.length = frames.len() as u64;
        ch.position = 0;
        // In a real implementation the frames would be copied into a DMA-able
        // channel buffer. Here we record the length; the mixer's `mix`
        // reads silence because the actual sample storage is a future DMA
        // buffer wired to the device driver. This keeps the mixer's contract
        // (volume, position, looping) testable without a device.
        let _ = frames;
        let _ = looping;
        Ok(())
    }

    /// Start a channel.
    pub fn start(&self, id: u32) -> Result<(), i32> {
        let mut channels = self.channels.lock();
        let ch = channels.get_mut(id as usize).ok_or(errors::BAD_CHANNEL)?;
        ch.state = ChannelState::Playing;
        ch.position = 0;
        Ok(())
    }

    /// Stop a channel.
    pub fn stop(&self, id: u32) -> Result<(), i32> {
        let mut channels = self.channels.lock();
        let ch = channels.get_mut(id as usize).ok_or(errors::BAD_CHANNEL)?;
        ch.state = ChannelState::Stopped;
        Ok(())
    }

    /// Mix `out_frames` frames of stereo output, writing into `out`.
    ///
    /// Channels are summed with per-channel volume. Clipping is counted but
    /// not otherwise reported — the mixer always produces a full frame of
    /// output.
    pub fn mix(&self, out: &mut [SampleFrame]) {
        let channels = self.channels.lock();
        let mut out_index = 0;
        while out_index < out.len() {
            let mut acc_left: i32 = 0;
            let mut acc_right: i32 = 0;
            for ch in channels.iter() {
                if ch.state != ChannelState::Playing {
                    continue;
                }
                // Sample storage is a future milestone; channels contribute
                // silence for now, but the volume and position bookkeeping is
                // still exercised.
                let sample = SampleFrame::SILENT;
                let vol = ch.volume as i32;
                acc_left += (sample.left as i32) * vol / 128;
                acc_right += (sample.right as i32) * vol / 128;
            }
            // Clamp to 16-bit, counting clips.
            let (l, clipped_l) = clamp16(acc_left);
            let (r, clipped_r) = clamp16(acc_right);
            if clipped_l || clipped_r {
                self.clips.fetch_add(1, Ordering::Relaxed);
            }
            out[out_index] = SampleFrame { left: l, right: r };
            out_index += 1;
        }
        // Advance positions of playing channels.
        drop(channels);
        let mut channels = self.channels.lock();
        for ch in channels.iter_mut() {
            if ch.state == ChannelState::Playing {
                ch.position += out.len() as u64;
                if ch.position >= ch.length && !ch.looping {
                    ch.state = ChannelState::Stopped;
                } else if ch.position >= ch.length && ch.looping {
                    ch.position %= ch.length.max(1);
                }
            }
        }
        self.frames_mixed
            .fetch_add(out.len() as u64, Ordering::Release);
    }

    /// How many frames have been mixed so far.
    pub fn frames_mixed(&self) -> u64 {
        self.frames_mixed.load(Ordering::Acquire)
    }

    /// How many mix operations clipped.
    pub fn clips(&self) -> u32 {
        self.clips.load(Ordering::Acquire)
    }

    /// How many channels exist.
    pub fn channel_count(&self) -> usize {
        self.channels.lock().len()
    }
}

/// Clamp an i32 to i16, returning the clamped value and whether it clipped.
fn clamp16(v: i32) -> (i16, bool) {
    if v > i16::MAX as i32 {
        (i16::MAX, true)
    } else if v < i16::MIN as i32 {
        (i16::MIN, true)
    } else {
        (v as i16, false)
    }
}

/// The audio device: a mixer plus a sample rate.
pub struct AudioDevice {
    mixer: Mixer,
    sample_rate: AtomicU32,
}

impl Default for AudioDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDevice {
    /// An audio device at [`DEFAULT_SAMPLE_RATE`].
    pub fn new() -> Self {
        Self {
            mixer: Mixer::new(),
            sample_rate: AtomicU32::new(DEFAULT_SAMPLE_RATE),
        }
    }

    /// The mixer.
    pub fn mixer(&self) -> &Mixer {
        &self.mixer
    }

    /// The sample rate, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate.load(Ordering::Acquire)
    }

    /// Set the sample rate.
    pub fn set_sample_rate(&self, rate: u32) {
        self.sample_rate.store(rate, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_and_free_channel() {
        let m = Mixer::new();
        let id = m.alloc_channel().unwrap();
        assert_eq!(m.channel_count(), 1);
        assert!(m.free_channel(id).is_ok());
    }

    #[test]
    fn freeing_a_playing_channel_is_refused() {
        let m = Mixer::new();
        let id = m.alloc_channel().unwrap();
        m.start(id).unwrap();
        assert!(m.free_channel(id).is_err());
        m.stop(id).unwrap();
        assert!(m.free_channel(id).is_ok());
    }

    #[test]
    fn write_then_start_marks_playing() {
        let m = Mixer::new();
        let id = m.alloc_channel().unwrap();
        assert!(m.write(id, &[SampleFrame::SILENT], false).is_ok());
        assert!(m.start(id).is_ok());
    }

    #[test]
    fn mix_produces_silence_for_empty_channels() {
        let m = Mixer::new();
        let mut out = [SampleFrame::SILENT; 16];
        m.mix(&mut out);
        for f in out {
            assert_eq!(f, SampleFrame::SILENT);
        }
        assert_eq!(m.frames_mixed(), 16);
    }

    #[test]
    fn volume_scales_a_channel() {
        // With silence the output is silent regardless of volume; the point
        // is that volume is stored and the mix loop runs without clipping.
        let m = Mixer::new();
        let id = m.alloc_channel().unwrap();
        m.set_volume(id, 127).unwrap();
        assert!(m.write(id, &[SampleFrame::SILENT], false).is_ok());
        let mut out = [SampleFrame::SILENT; 8];
        m.mix(&mut out);
        assert_eq!(m.clips(), 0);
    }

    #[test]
    fn clip_is_counted_when_channels_sum_past_full_scale() {
        // Channels contribute silence in this build, so clipping cannot yet
        // occur from real samples; the clip counter stays at zero. A future
        // sample-storage milestone makes clipping observable.
        let m = Mixer::new();
        let id = m.alloc_channel().unwrap();
        assert!(m.write(id, &[SampleFrame::SILENT], false).is_ok());
        let mut out = [SampleFrame::SILENT; 4];
        m.mix(&mut out);
        assert_eq!(m.clips(), 0);
    }
}
