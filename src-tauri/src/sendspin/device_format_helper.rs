use super::devices;
use cpal::{traits::DeviceTrait, SampleFormat, SampleRate};
use sendspin::protocol::messages::AudioFormatSpec;

pub fn get_device_formats(device_id: &str) -> Vec<AudioFormatSpec> {
    let device = devices::get_device_by_id(device_id).unwrap();
    enumerate_supported_formats(&device)
}

/// Get supported formats
fn enumerate_supported_formats(device: &cpal::Device) -> Vec<AudioFormatSpec> {
    // Candidate sample rates to test
    const COMMON_RATES: &[u32] = &[
        8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000,
    ];

    let mut out = Vec::new();

    let Ok(configs) = device.supported_output_configs() else {
        return out;
    };

    for range in configs {
        let channels = range.channels();
        let sample_format = range.sample_format();
        let bit_depth = bit_depth_from_sample_format(sample_format);

        let min = range.min_sample_rate().0;
        let max = range.max_sample_rate().0;

        for &sr in COMMON_RATES {
            if sr < min || sr > max {
                continue;
            }

            // Build a concrete config
            let config = cpal::StreamConfig {
                channels,
                sample_rate: SampleRate(sr),
                buffer_size: cpal::BufferSize::Default,
            };

            // Try to open a stream to test if it's truly supported
            let attempt = match sample_format {
                SampleFormat::I16 => {
                    device.build_output_stream(&config, |_data: &mut [i16], _| {}, |_err| {}, None)
                }
                SampleFormat::U16 => {
                    device.build_output_stream(&config, |_data: &mut [u16], _| {}, |_err| {}, None)
                }
                SampleFormat::F32 => {
                    device.build_output_stream(&config, |_data: &mut [f32], _| {}, |_err| {}, None)
                }
                _ => continue,
            };

            if attempt.is_ok() {
                out.push(AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: channels as u8,
                    sample_rate: sr,
                    bit_depth,
                });
            }
        }
    }

    out
}

fn bit_depth_from_sample_format(fmt: SampleFormat) -> u8 {
    match fmt {
        SampleFormat::I16 | SampleFormat::U16 => 16,
        SampleFormat::F32 => 32,
        // If CPAL adds more formats in the future:
        _ => 0,
    }
}
