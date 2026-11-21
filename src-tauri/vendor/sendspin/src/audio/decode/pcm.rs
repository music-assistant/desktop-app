// ABOUTME: PCM decoder implementation
// ABOUTME: Supports 16-bit and 24-bit PCM decoding with zero-copy where possible

use crate::audio::decode::Decoder;
use crate::error::Error;
use cpal::Sample;
use std::sync::Arc;

/// PCM endianness
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmEndian {
    /// Little-endian byte order
    Little,
    /// Big-endian byte order
    Big,
}

/// PCM audio decoder supporting 16-bit and 24-bit formats
#[derive(Clone)]
pub struct PcmDecoder {
    bit_depth: u8,
    endian: PcmEndian,
}

impl PcmDecoder {
    /// Create a new PCM decoder with the specified bit depth (16 or 24), defaulting to little-endian
    pub fn new(bit_depth: u8) -> Self {
        Self {
            bit_depth,
            endian: PcmEndian::Little,
        }
    }

    /// Create a new PCM decoder with explicit endianness
    pub fn with_endian(bit_depth: u8, endian: PcmEndian) -> Self {
        Self { bit_depth, endian }
    }
}

impl Decoder for PcmDecoder {
    fn decode(&self, data: &[u8]) -> Result<Arc<[i32]>, Error> {
        let bytes_per_sample: usize = match self.bit_depth {
            16 => 2,
            24 => 3,
            other => return Err(Error::Protocol(format!("Unsupported bit depth: {}", other))),
        };

        if data.is_empty() {
            return Err(Error::Protocol("Empty PCM data".to_string()));
        }
        if !data.len().is_multiple_of(bytes_per_sample) {
            return Err(Error::Protocol(format!(
                "Truncated {}-bit PCM: {} bytes is not a multiple of {}",
                self.bit_depth,
                data.len(),
                bytes_per_sample,
            )));
        }

        match (self.bit_depth, self.endian) {
            (16, PcmEndian::Little) => {
                let samples: Vec<i32> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| {
                        let i16_val = i16::from_le_bytes([c[0], c[1]]);
                        i32::from_sample(i16_val)
                    })
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            (16, PcmEndian::Big) => {
                let samples: Vec<i32> = data
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|c| {
                        let i16_val = i16::from_be_bytes([c[0], c[1]]);
                        i32::from_sample(i16_val)
                    })
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            (24, PcmEndian::Little) => {
                let samples: Vec<i32> = data
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|c| {
                        let extended = i32::from_i24_le([c[0], c[1], c[2]]);
                        i32::from_sample(cpal::I24::new_unchecked(extended))
                    })
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            (24, PcmEndian::Big) => {
                let samples: Vec<i32> = data
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|c| {
                        let extended = i32::from_i24_be([c[0], c[1], c[2]]);
                        i32::from_sample(cpal::I24::new_unchecked(extended))
                    })
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            // Unreachable: bit_depth validated above
            _ => unreachable!(),
        }
    }
}

trait Conversion {
    fn from_i24_le(bytes: [u8; 3]) -> Self;
    fn from_i24_be(bytes: [u8; 3]) -> Self;
}

impl Conversion for i32 {
    /// Convert from 24-bit little-endian bytes
    #[inline]
    fn from_i24_le(bytes: [u8; 3]) -> Self {
        // Build 24-bit signed integer in i32
        let val = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
        // Sign-extend from 24-bit to 32-bit
        if val & 0x00800000 != 0 {
            val | 0xFF000000u32 as i32 // Negative: fill upper 8 bits with 1
        } else {
            val // Positive: upper 8 bits already 0
        }
    }

    /// Convert from 24-bit big-endian bytes
    #[inline]
    fn from_i24_be(bytes: [u8; 3]) -> Self {
        // Build 24-bit signed integer in i32 (big-endian order)
        let val = ((bytes[0] as i32) << 16) | ((bytes[1] as i32) << 8) | (bytes[2] as i32);
        // Sign-extend from 24-bit to 32-bit
        if val & 0x00800000 != 0 {
            val | 0xFF000000u32 as i32 // Negative: fill upper 8 bits with 1
        } else {
            val // Positive: upper 8 bits already 0
        }
    }
}

#[test]
fn test_sample_from_i24_le() {
    // 4096 in 24-bit little-endian
    let sample = i32::from_i24_le([0x00, 0x10, 0x00]);
    assert_eq!(sample, 4096);
}

#[test]
fn test_sample_from_i24_le_negative() {
    // -1 in 24-bit LE: 0xFF 0xFF 0xFF
    let sample = i32::from_i24_le([0xFF, 0xFF, 0xFF]);
    assert_eq!(sample, -1);
}

#[test]
fn test_sample_from_i24_le_boundary_values() {
    // Max 24-bit: 0x7FFFFF = 8388607
    let max_sample = i32::from_i24_le([0xFF, 0xFF, 0x7F]);
    assert_eq!(max_sample, 8388607);

    // Min 24-bit: 0x800000 = -8388608
    let min_sample = i32::from_i24_le([0x00, 0x00, 0x80]);
    assert_eq!(min_sample, -8388608);

    // Zero
    let zero = i32::from_i24_le([0x00, 0x00, 0x00]);
    assert_eq!(zero, 0);
}

#[test]
fn test_sample_from_i24_be_roundtrip() {
    // 4096 in 24-bit BE: 0x00 0x10 0x00
    let sample = i32::from_i24_be([0x00, 0x10, 0x00]);
    assert_eq!(sample, 4096);

    // -1 in 24-bit BE
    let neg = i32::from_i24_be([0xFF, 0xFF, 0xFF]);
    assert_eq!(neg, -1);
}

#[test]
fn test_sample_from_i24_be_boundary_values() {
    // Max 24-bit: 0x7FFFFF = 8388607
    let max_sample = i32::from_i24_be([0x7F, 0xFF, 0xFF]);
    assert_eq!(max_sample, 8388607);

    // Min 24-bit: 0x800000 = -8388608
    let min_sample = i32::from_i24_be([0x80, 0x00, 0x00]);
    assert_eq!(min_sample, -8388608);

    // Zero
    let zero = i32::from_i24_be([0x00, 0x00, 0x00]);
    assert_eq!(zero, 0);
}
