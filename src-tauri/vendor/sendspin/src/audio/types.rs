// ABOUTME: Core audio type definitions
// ABOUTME: AudioFormat, AudioBuffer for zero-copy audio data

use std::sync::Arc;

/// Sample type for audio data.
///
/// Represents a single audio sample. The value is expected to be in the range [-1.0, 1.0]
/// when normalized, but stored as i32 for compatibility with cpal's sample format.
pub type Sample = i32;

/// Audio codec type
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    /// Uncompressed PCM audio
    Pcm,
    /// Opus compressed audio
    Opus,
    /// FLAC lossless compressed audio
    Flac,
    /// MP3 compressed audio
    Mp3,
}

/// Audio format specification
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    /// Audio codec used
    pub codec: Codec,
    /// Sample rate in Hz (e.g., 48000)
    pub sample_rate: u32,
    /// Number of audio channels (1 = mono, 2 = stereo)
    pub channels: u8,
    /// Bit depth per sample (16 or 24)
    pub bit_depth: u8,
    /// Optional codec-specific header data
    pub codec_header: Option<Vec<u8>>,
}

impl AudioFormat {
    /// Duration of `num_samples` total samples (interleaved) in microseconds.
    ///
    /// `num_samples` is the **total** sample count (frames × channels).
    pub fn duration_us(&self, num_samples: usize) -> i64 {
        debug_assert!(self.channels > 0, "AudioFormat with 0 channels");
        debug_assert!(self.sample_rate > 0, "AudioFormat with 0 sample_rate");
        let frames = num_samples / self.channels.max(1) as usize;
        let rate = self.sample_rate.max(1) as i64;
        // Round to nearest instead of truncating so that duration_us stays
        // consistent with the remainder-tracking in advance_cursor().
        (frames as i64 * 1_000_000 + rate / 2) / rate
    }
}

/// Audio buffer with a server-loop timestamp (zero-copy via Arc).
///
/// Scheduling keys on `timestamp`; [`SyncedPlayer`](crate::audio::SyncedPlayer)
/// converts it to a local playback instant live in the output callback, applying
/// the current clock-sync estimate at play time rather than baking it in here.
pub struct AudioBuffer {
    /// Server loop timestamp in microseconds.
    pub timestamp: i64,
    /// Immutable, shareable sample data.
    pub samples: Arc<[Sample]>,
    /// Audio format specification.
    pub format: AudioFormat,
}

impl AudioBuffer {
    /// Duration of this buffer in microseconds, derived from sample count and format.
    pub fn duration_us(&self) -> i64 {
        self.format.duration_us(self.samples.len())
    }
}
