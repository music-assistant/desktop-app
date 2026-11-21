// ABOUTME: Opus decoder implementation
// ABOUTME: Stateful pure-Rust packet decoding through opus-rs

use crate::audio::decode::Decoder;
use crate::error::Error;
use parking_lot::Mutex;
use std::sync::Arc;

/// Stateful Opus decoder for raw Sendspin Opus packets.
///
/// Sendspin carries one complete Opus packet per binary audio chunk. The
/// decoder is kept behind a mutex because the public [`Decoder`] trait is
/// shared by the player's message and audio handling paths while Opus itself
/// requires mutable state between packets.
pub struct OpusDecoder {
    decoder: Mutex<opus_rs::OpusDecoder>,
    channels: usize,
    sample_rate: u32,
}

impl OpusDecoder {
    /// Create an Opus decoder for the stream's sample rate and channel count.
    pub fn new(sample_rate: u32, channels: u8) -> Result<Self, Error> {
        let channels = usize::from(channels);
        if !matches!(sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
            return Err(Error::Protocol(format!(
                "Unsupported Opus sample rate: {sample_rate}"
            )));
        }
        if !matches!(channels, 1 | 2) {
            return Err(Error::Protocol(format!(
                "Unsupported Opus channel count: {channels}"
            )));
        }

        let decoder = opus_rs::OpusDecoder::new(sample_rate as i32, channels).map_err(|error| {
            Error::Protocol(format!("Opus decoder initialization failed: {error}"))
        })?;

        Ok(Self {
            decoder: Mutex::new(decoder),
            channels,
            sample_rate,
        })
    }

    fn frame_size_from_toc(&self, toc: u8) -> Result<usize, Error> {
        let config = usize::from(toc >> 3);
        let frame_size = match config {
            0..=11 => [
                self.sample_rate / 100,
                self.sample_rate / 50,
                self.sample_rate / 25,
                self.sample_rate * 3 / 50,
            ][config & 3],
            12..=15 => [self.sample_rate / 100, self.sample_rate / 50][config & 1],
            16..=31 => [
                self.sample_rate / 400,
                self.sample_rate / 200,
                self.sample_rate / 100,
                self.sample_rate / 50,
            ][config & 3],
            _ => unreachable!(),
        };
        if frame_size == 0 {
            Err(Error::Protocol("Invalid Opus frame size".to_string()))
        } else {
            Ok(frame_size as usize)
        }
    }
}

impl Decoder for OpusDecoder {
    fn decode(&self, data: &[u8]) -> Result<Arc<[i32]>, Error> {
        if data.is_empty() {
            return Err(Error::Protocol("Empty Opus packet".to_string()));
        }

        let frame_size = self.frame_size_from_toc(data[0])?;
        let frame_count = match data[0] & 0x03 {
            0 => 1,
            1 | 2 => 2,
            3 => usize::from(
                data.get(1).ok_or_else(|| {
                    Error::Protocol("Opus code-3 packet is missing its frame count".to_string())
                })? & 0x3f,
            ),
            _ => unreachable!(),
        };
        if frame_count == 0 || frame_count > 48 {
            return Err(Error::Protocol(format!(
                "Invalid Opus frame count: {frame_count}"
            )));
        }
        let mut output = vec![0.0f32; frame_size * self.channels * frame_count];
        let mut decoder = self.decoder.lock();
        let samples_per_channel = decoder
            .decode(data, frame_size * frame_count, &mut output)
            .map_err(|error| Error::Protocol(format!("Opus decode error: {error}")))?;
        output.truncate(samples_per_channel * self.channels);

        let samples = output
            .into_iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * i32::MAX as f32).round() as i32)
            .collect::<Vec<_>>();
        Ok(Arc::from(samples.into_boxed_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_opus_rs_packet() {
        let mut encoder =
            opus_rs::OpusEncoder::new(48_000, 2, opus_rs::Application::Audio).unwrap();
        encoder.bitrate_bps = 96_000;
        let input = vec![0.0f32; 960 * 2];
        let mut packet = vec![0u8; 1275];
        let length = encoder.encode(&input, 960, &mut packet).unwrap();

        let decoder = OpusDecoder::new(48_000, 2).unwrap();
        let decoded = decoder.decode(&packet[..length]).unwrap();
        assert_eq!(decoded.len(), 960 * 2);
    }

    #[test]
    fn rejects_invalid_stream_parameters() {
        assert!(OpusDecoder::new(44_100, 2).is_err());
        assert!(OpusDecoder::new(48_000, 6).is_err());
    }
}
