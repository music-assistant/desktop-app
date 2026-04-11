//! Audio device enumeration and selection using cpal
//!
//! This module provides cross-platform audio device enumeration
//! for selecting output devices in the Sendspin client.

use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Sendspin PCM format candidate derived from device capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SupportedPcmFormat {
    pub channels: u16,
    pub sample_rate: u32,
    pub bit_depth: u16,
}

/// Information about an audio output device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    /// Unique identifier for the device
    pub id: String,
    /// Human-readable device name
    pub name: String,
    /// Whether this is the system default device
    pub is_default: bool,
    /// Supported sample rates (common ones)
    pub sample_rates: Vec<u32>,
    /// Maximum number of output channels
    pub max_channels: u16,
}

/// List all available audio output devices
pub fn list_devices() -> Result<Vec<AudioDevice>, String> {
    let host = cpal::default_host();

    let default_device_name = host
        .default_output_device()
        .and_then(|d| d.description().ok().map(|desc| desc.name().to_string()));

    let devices = host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate devices: {}", e))?;

    let mut result = Vec::new();

    for device in devices {
        let Ok(desc) = device.description() else {
            continue; // Skip devices we can't get a description for
        };
        let name = desc.name().to_string();

        let is_default = default_device_name.as_ref().is_some_and(|d| d == &name);

        // Get supported configurations
        let (sample_rates, max_channels) = match device.supported_output_configs() {
            Ok(configs) => {
                let mut rates = Vec::new();
                let mut channels = 0u16;

                for config in configs {
                    // Collect common sample rates that are supported
                    let min_rate = config.min_sample_rate();
                    let max_rate = config.max_sample_rate();

                    for &rate in &[44100, 48000, 88200, 96000, 176400, 192000, 384000] {
                        if rate >= min_rate && rate <= max_rate && !rates.contains(&rate) {
                            rates.push(rate);
                        }
                    }

                    if config.channels() > channels {
                        channels = config.channels();
                    }
                }

                rates.sort_unstable();
                (rates, channels)
            }
            Err(_) => (vec![44100, 48000], 2), // Fallback defaults
        };

        // Use device name as ID (cpal doesn't provide stable IDs)
        let id = name.clone();

        result.push(AudioDevice {
            id,
            name,
            is_default,
            sample_rates,
            max_channels,
        });
    }

    // Sort with default device first
    result.sort_by(|a, b| {
        if a.is_default && !b.is_default {
            std::cmp::Ordering::Less
        } else if !a.is_default && b.is_default {
            std::cmp::Ordering::Greater
        } else {
            a.name.cmp(&b.name)
        }
    });

    Ok(result)
}

/// Get device by ID (name)
pub fn get_device_by_id(device_id: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();

    let devices = host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate devices: {}", e))?;

    for device in devices {
        if let Ok(desc) = device.description() {
            if desc.name() == device_id {
                return Ok(device);
            }
        }
    }

    Err(format!("Device not found: {}", device_id))
}

/// Get the default output device
#[allow(dead_code)]
pub fn get_default_device() -> Result<cpal::Device, String> {
    let host = cpal::default_host();

    host.default_output_device()
        .ok_or_else(|| "No default output device available".to_string())
}

/// Resolve output device based on optional device ID.
/// Falls back to default output device if the requested device is not available.
pub fn resolve_output_device(device_id: Option<&str>) -> Option<cpal::Device> {
    if let Some(id) = device_id {
        match get_device_by_id(id) {
            Ok(device) => {
                let name = device.description().ok().map_or_else(
                    || "<unknown device>".to_string(),
                    |desc| desc.name().to_string(),
                );
                eprintln!("[Sendspin] Using configured output device: {}", name);
                return Some(device);
            }
            Err(e) => {
                eprintln!(
                    "[Sendspin] Failed to get device {}: {}, falling back to default output",
                    id, e
                );
            }
        }
    }

    match get_default_device() {
        Ok(device) => {
            let name = device.description().ok().map_or_else(
                || "<unknown device>".to_string(),
                |desc| desc.name().to_string(),
            );
            eprintln!("[Sendspin] Using default output device: {}", name);
            Some(device)
        }
        Err(e) => {
            eprintln!("[Sendspin] Failed to get default output device: {}", e);
            None
        }
    }
}

/// Build supported PCM stream formats for Sendspin negotiation.
///
/// Strategy:
/// - Prefer stereo stream formats for compatibility with current playback path.
/// - Use stable/common rates first.
/// - Advertise 24-bit only if the output config clearly supports 24-bit integer samples.
/// - Always include 16-bit for broad compatibility where possible.
pub fn derive_supported_pcm_formats(device: Option<&cpal::Device>) -> Vec<SupportedPcmFormat> {
    let Some(device) = device else {
        return vec![];
    };

    let Ok(configs) = device.supported_output_configs() else {
        return vec![];
    };

    let mut collected = BTreeSet::new();
    let preferred_rates = [48_000u32, 44_100u32, 96_000u32, 192_000u32, 384_000u32];

    for cfg in configs {
        // Keep negotiation aligned with the stereo playback path.
        if cfg.channels() < 2 {
            continue;
        }

        let min_rate = cfg.min_sample_rate();
        let max_rate = cfg.max_sample_rate();
        let supports_24bit = matches!(
            cfg.sample_format(),
            cpal::SampleFormat::I24 | cpal::SampleFormat::U24,
        );

        for rate in preferred_rates {
            if rate < min_rate || rate > max_rate {
                continue;
            }

            collected.insert(SupportedPcmFormat {
                channels: 2,
                sample_rate: rate,
                bit_depth: 16,
            });

            if supports_24bit {
                collected.insert(SupportedPcmFormat {
                    channels: 2,
                    sample_rate: rate,
                    bit_depth: 24,
                });
            }
        }
    }

    let mut result: Vec<_> = collected.into_iter().collect();
    result.sort_by_key(|f| {
        let rate_rank = match f.sample_rate {
            48_000 => 0,
            44_100 => 1,
            96_000 => 2,
            192_000 => 3,
            384_000 => 4,
            _ => 5,
        };
        let depth_rank = i32::from(f.bit_depth != 16);
        (rate_rank, depth_rank, f.sample_rate, f.bit_depth)
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_devices() {
        let devices = list_devices();
        println!("Found devices: {:?}", devices);
        // This test just checks that enumeration doesn't panic
        assert!(devices.is_ok());
    }

    #[test]
    fn test_device_sorting_default_first_and_alphabetical() {
        let mut devices = [
            AudioDevice {
                id: "z".into(),
                name: "Zebra".into(),
                is_default: false,
                sample_rates: vec![],
                max_channels: 2,
            },
            AudioDevice {
                id: "a".into(),
                name: "Apple".into(),
                is_default: false,
                sample_rates: vec![],
                max_channels: 2,
            },
            AudioDevice {
                id: "d".into(),
                name: "Default Speaker".into(),
                is_default: true,
                sample_rates: vec![],
                max_channels: 2,
            },
            AudioDevice {
                id: "m".into(),
                name: "Monitor".into(),
                is_default: false,
                sample_rates: vec![],
                max_channels: 2,
            },
        ]
        .to_vec();

        // Apply the same sort as list_devices
        devices.sort_by(|a, b| {
            if a.is_default && !b.is_default {
                std::cmp::Ordering::Less
            } else if !a.is_default && b.is_default {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });

        assert_eq!(devices[0].name, "Default Speaker");
        assert!(devices[0].is_default);
        assert_eq!(devices[1].name, "Apple");
        assert_eq!(devices[2].name, "Monitor");
        assert_eq!(devices[3].name, "Zebra");
    }

    #[test]
    fn test_get_device_by_id_nonexistent_returns_error() {
        let result = get_device_by_id("definitely_not_a_real_device_12345");
        match result {
            Err(err) => {
                assert!(
                    err.contains("definitely_not_a_real_device_12345"),
                    "Error should contain the device ID, got: {}",
                    err
                );
            }
            Ok(_) => panic!("Expected error for nonexistent device"),
        }
    }
}
