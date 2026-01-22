//! Audio device enumeration and selection using cpal
//!
//! This module provides cross-platform audio device enumeration
//! for selecting output devices in the Sendspin client.

use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};

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

    let default_device_name = host.default_output_device().and_then(|d| d.name().ok());

    let devices = host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate devices: {}", e))?;

    let mut result = Vec::new();

    for device in devices {
        let Ok(name) = device.name() else {
            continue; // Skip devices we can't get a name for
        };

        let is_default = default_device_name.as_ref().is_some_and(|d| d == &name);

        // Get supported configurations
        let (sample_rates, max_channels) = match device.supported_output_configs() {
            Ok(configs) => {
                let mut rates = Vec::new();
                let mut channels = 0u16;

                for config in configs {
                    // Collect common sample rates that are supported
                    let min_rate = config.min_sample_rate().0;
                    let max_rate = config.max_sample_rate().0;

                    for &rate in &[44100, 48000, 88200, 96000, 176400, 192000] {
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
        if let Ok(name) = device.name() {
            if name == device_id {
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
}
