// ABOUTME: FLAC decoder implementation
// ABOUTME: Frame-at-a-time streaming FLAC decoding via the pure-Rust flac-codec crate

use crate::audio::decode::Decoder;
use crate::error::Error;
use flac_codec::decode::FlacStreamReader;
use std::sync::Arc;

/// Stream parameters, either parsed from the codec header (STREAMINFO) or
/// carried by an individual FLAC frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamParams {
    sample_rate: u32,
    channels: u8,
    bits_per_sample: u32,
}

/// FLAC audio decoder for chunked streams.
///
/// Sendspin delivers FLAC as a one-time codec header (`fLaC` magic +
/// STREAMINFO metadata block) followed by binary chunks containing one or
/// more complete FLAC frames. Because FLAC frames are self-contained and
/// independent, each chunk is decoded stand-alone: [`FlacDecoder::decode`]
/// consumes every frame in the chunk and returns the interleaved samples.
///
/// Decoded samples are scaled to full `i32` range (matching
/// [`PcmDecoder`](crate::audio::decode::PcmDecoder), which widens 16-bit
/// samples by 16 bits and 24-bit samples by 8 bits).
///
/// # Limitations
///
/// Frame headers must be self-describing. FLAC allows a frame header to
/// defer its sample rate or bit depth to STREAMINFO instead of encoding
/// them inline; such frames are rejected rather than decoded. No mainstream
/// encoder (libflac, ffmpeg) emits them, but the limitation is inherited
/// from the headerless stream reader used underneath.
#[derive(Clone)]
pub struct FlacDecoder {
    /// Expected stream parameters from the codec header, if provided.
    /// When present, every decoded frame is validated against them.
    expected: Option<StreamParams>,
}

impl FlacDecoder {
    /// Create a FLAC decoder without a codec header.
    ///
    /// Frames are decoded using only their own (self-describing) frame
    /// headers, with no cross-checking against stream-level metadata.
    pub fn new() -> Self {
        Self { expected: None }
    }

    /// Create a FLAC decoder from the stream's codec header.
    ///
    /// `header` must be the raw (already base64-decoded) codec header from
    /// `stream/start`: the `fLaC` marker followed by the STREAMINFO metadata
    /// block. The decoder validates every frame against the header's sample
    /// rate, channel count, and bit depth, surfacing format drift as a
    /// protocol error.
    ///
    /// If the stream provides no codec header, use [`FlacDecoder::new`]
    /// instead — FLAC frames are self-describing and decode fine without
    /// one, just without cross-validation.
    pub fn with_header(header: &[u8]) -> Result<Self, Error> {
        let info = flac_codec::metadata::read_info(header)
            .map_err(|e| Error::Protocol(format!("Invalid FLAC codec header: {e}")))?;

        Ok(Self {
            expected: Some(StreamParams {
                sample_rate: info.sample_rate,
                channels: info.channels.get(),
                bits_per_sample: info.bits_per_sample.into(),
            }),
        })
    }
}

impl Default for FlacDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for FlacDecoder {
    fn decode(&self, data: &[u8]) -> Result<Arc<[i32]>, Error> {
        if data.is_empty() {
            return Err(Error::Protocol("Empty FLAC data".to_string()));
        }

        let mut remaining: &[u8] = data;
        let mut out: Vec<i32> = Vec::new();

        while !remaining.is_empty() {
            // The reader resyncs past garbage while hunting for a frame
            // sync — right for damaged files, wrong for an exactly-framed
            // protocol where skipped bytes are silent sample loss. Require
            // each frame to start at a sync boundary so corruption fails
            // loudly instead.
            if remaining.len() < 2 || remaining[0] != 0xFF || remaining[1] >> 1 != 0b1111100 {
                return Err(Error::Protocol(
                    "FLAC chunk does not start at a frame boundary".to_string(),
                ));
            }

            // Frames are independent, so a fresh reader per frame is fine.
            // Reading through `&mut remaining` leaves the slice at the next
            // frame boundary: empty means clean end of chunk, so any read
            // error below is real corruption rather than end-of-data.
            let mut reader = FlacStreamReader::new(&mut remaining);
            let frame = reader
                .read()
                .map_err(|e| Error::Protocol(format!("FLAC decode error: {e}")))?;

            if let Some(expected) = self.expected {
                let actual = StreamParams {
                    sample_rate: frame.sample_rate,
                    channels: frame.channels,
                    bits_per_sample: frame.bits_per_sample,
                };
                if actual != expected {
                    return Err(Error::Protocol(format!(
                        "FLAC frame format {actual:?} does not match stream header {expected:?}"
                    )));
                }
            }

            // Widen native-depth samples to the full-scale i32 the pipeline
            // expects (matching PcmDecoder: 16-bit << 16, 24-bit << 8). The
            // saturating/wrapping ops keep a hostile bits_per_sample from
            // panicking; the parser already bounds it to 4..=32.
            let shift = 32u32.saturating_sub(frame.bits_per_sample);
            out.extend(frame.samples.iter().map(|s| s.wrapping_shl(shift)));
        }

        Ok(Arc::from(out.into_boxed_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flac_codec::encode::{FlacStreamWriter, Options};

    /// Encode one FLAC frame containing `samples` (interleaved, native scale).
    ///
    /// `FlacStreamWriter` emits exactly one raw frame per `write` call with
    /// no file header, which matches how Sendspin chunks arrive on the wire.
    fn encode_frame(
        sample_rate: u32,
        channels: u8,
        bits_per_sample: u32,
        samples: &[i32],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut writer = FlacStreamWriter::new(&mut buf, Options::default());
        writer
            .write(sample_rate, channels, bits_per_sample, samples)
            .unwrap();
        drop(writer); // release the writer's borrow of `buf`
        buf
    }

    /// A short deterministic 16-bit stereo test signal (interleaved).
    ///
    /// Non-repeating and full-range enough that the encoder produces real
    /// predicted subframes rather than degenerate constant/verbatim ones.
    fn test_signal_16(frames: usize) -> Vec<i32> {
        (0..frames)
            .flat_map(|i| {
                let left = ((i as i32 * 373) % 32767) - 16384;
                let right = -(((i as i32 * 151) % 32767) - 16384);
                [left, right]
            })
            .collect()
    }

    #[test]
    fn round_trip_single_frame_16bit() {
        let samples = test_signal_16(1024);
        let chunk = encode_frame(48000, 2, 16, &samples);

        let decoder = FlacDecoder::new();
        let decoded = decoder.decode(&chunk).unwrap();

        assert_eq!(decoded.len(), samples.len());
        for (d, s) in decoded.iter().zip(samples.iter()) {
            assert_eq!(*d, s << 16);
        }
    }

    #[test]
    fn round_trip_multiple_frames_in_one_chunk() {
        let a = test_signal_16(512);
        let b = test_signal_16(400);
        let mut chunk = encode_frame(48000, 2, 16, &a);
        chunk.extend(encode_frame(48000, 2, 16, &b));

        let decoder = FlacDecoder::new();
        let decoded = decoder.decode(&chunk).unwrap();

        assert_eq!(decoded.len(), a.len() + b.len());
        let expected: Vec<i32> = a.iter().chain(b.iter()).map(|s| s << 16).collect();
        assert_eq!(decoded.as_ref(), expected.as_slice());
    }

    #[test]
    fn chunk_per_frame_stream() {
        let decoder = FlacDecoder::new();
        for len in [512usize, 256, 1024] {
            let samples = test_signal_16(len);
            let chunk = encode_frame(44100, 2, 16, &samples);
            let decoded = decoder.decode(&chunk).unwrap();
            assert_eq!(decoded.len(), samples.len());
            assert_eq!(decoded[0], samples[0] << 16);
        }
    }

    #[test]
    fn scales_24bit_to_full_i32() {
        // Distinctive 24-bit values, including extremes.
        let samples: Vec<i32> = vec![8_388_607, -8_388_608, 0, 1, -1, 4096, -4096, 42];
        let chunk = encode_frame(96000, 1, 24, &samples);

        let decoder = FlacDecoder::new();
        let decoded = decoder.decode(&chunk).unwrap();

        assert_eq!(decoded.len(), samples.len());
        for (d, s) in decoded.iter().zip(samples.iter()) {
            assert_eq!(*d, s << 8);
        }
    }

    #[test]
    fn empty_data_is_error() {
        let decoder = FlacDecoder::new();
        assert!(decoder.decode(&[]).is_err());
    }

    #[test]
    fn garbage_data_is_error() {
        let decoder = FlacDecoder::new();
        let garbage: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        assert!(decoder.decode(&garbage).is_err());
    }

    /// Garbage that opens with a byte-valid frame sync pattern must still be
    /// rejected — this exercises the header-parse/CRC failure path rather
    /// than the boundary check.
    #[test]
    fn fake_sync_garbage_is_error() {
        let decoder = FlacDecoder::new();
        let mut garbage = vec![0xFF, 0xF8];
        garbage.extend((0..1024u32).map(|i| (i % 251) as u8));
        assert!(decoder.decode(&garbage).is_err());
    }

    #[test]
    fn leading_garbage_before_valid_frame_is_error() {
        let samples = test_signal_16(256);
        let frame = encode_frame(48000, 2, 16, &samples);
        let mut chunk = vec![0x00, 0x01, 0x02, 0x03];
        chunk.extend(&frame);

        // Must refuse, not resync past the garbage and decode the frame.
        let decoder = FlacDecoder::new();
        assert!(decoder.decode(&chunk).is_err());
    }

    #[test]
    fn garbage_between_frames_is_error() {
        let a = test_signal_16(256);
        let b = test_signal_16(256);
        let mut chunk = encode_frame(48000, 2, 16, &a);
        chunk.extend_from_slice(&[0x13, 0x37]);
        chunk.extend(encode_frame(48000, 2, 16, &b));

        let decoder = FlacDecoder::new();
        assert!(decoder.decode(&chunk).is_err());
    }

    /// Locks the generic `32 - bits_per_sample` scaling path for a depth the
    /// protocol doesn't use (8-bit widens by 24).
    #[test]
    fn scales_8bit_to_full_i32() {
        let samples: Vec<i32> = vec![127, -128, 0, 1, -1, 64, -64];
        let chunk = encode_frame(48000, 1, 8, &samples);

        let decoder = FlacDecoder::new();
        let decoded = decoder.decode(&chunk).unwrap();

        assert_eq!(decoded.len(), samples.len());
        for (d, s) in decoded.iter().zip(samples.iter()) {
            assert_eq!(*d, s << 24);
        }
    }

    #[test]
    fn truncated_frame_is_error() {
        let samples = test_signal_16(1024);
        let chunk = encode_frame(48000, 2, 16, &samples);
        let truncated = &chunk[..chunk.len() - 16];

        let decoder = FlacDecoder::new();
        assert!(decoder.decode(truncated).is_err());
    }

    #[test]
    fn trailing_garbage_after_valid_frame_is_error() {
        let samples = test_signal_16(256);
        let mut chunk = encode_frame(48000, 2, 16, &samples);
        chunk.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let decoder = FlacDecoder::new();
        assert!(decoder.decode(&chunk).is_err());
    }

    /// Build a codec header (fLaC + STREAMINFO) for the given format by
    /// encoding a minimal in-memory FLAC file and keeping only its header —
    /// less brittle than assembling STREAMINFO bit fields by hand.
    fn make_header(sample_rate: u32, channels: u8, bits_per_sample: u32) -> Vec<u8> {
        use flac_codec::encode::FlacSampleWriter;
        use std::io::Cursor;

        let mut flac = Cursor::new(Vec::new());
        let mut writer = FlacSampleWriter::new(
            &mut flac,
            Options::default(),
            sample_rate,
            bits_per_sample,
            channels,
            None,
        )
        .unwrap();
        writer.write(&vec![0i32; 16 * channels as usize]).unwrap();
        writer.finalize().unwrap();

        // "fLaC" (4) + block header (4) + STREAMINFO (34); STREAMINFO is
        // always the first block, per spec.
        let bytes = flac.into_inner();
        bytes[..42].to_vec()
    }

    #[test]
    fn with_header_accepts_matching_frames() {
        let header = make_header(48000, 2, 16);
        let decoder = FlacDecoder::with_header(&header).unwrap();

        let samples = test_signal_16(512);
        let chunk = encode_frame(48000, 2, 16, &samples);
        let decoded = decoder.decode(&chunk).unwrap();
        assert_eq!(decoded.len(), samples.len());
    }

    #[test]
    fn with_header_rejects_mismatched_frames() {
        let header = make_header(44100, 2, 16);
        let decoder = FlacDecoder::with_header(&header).unwrap();

        let samples = test_signal_16(512);
        let chunk = encode_frame(48000, 2, 16, &samples);
        assert!(decoder.decode(&chunk).is_err());
    }

    #[test]
    fn invalid_header_is_error() {
        assert!(FlacDecoder::with_header(b"not a flac header").is_err());
        assert!(FlacDecoder::with_header(&[]).is_err());
    }
}
