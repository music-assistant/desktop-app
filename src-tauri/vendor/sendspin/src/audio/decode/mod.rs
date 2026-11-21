// ABOUTME: Audio decoder implementations
// ABOUTME: PCM, Opus, and FLAC decoders

/// FLAC decoder implementation
pub mod flac;
/// Opus decoder implementation
pub mod opus;
/// PCM decoder implementation
pub mod pcm;

pub use flac::FlacDecoder;
pub use opus::OpusDecoder;
pub use pcm::{PcmDecoder, PcmEndian};

use crate::error::Error;
use std::sync::Arc;

/// Decoder trait for audio codecs
pub trait Decoder {
    /// Decode raw audio data into samples
    fn decode(&self, data: &[u8]) -> Result<Arc<[i32]>, Error>;
}
