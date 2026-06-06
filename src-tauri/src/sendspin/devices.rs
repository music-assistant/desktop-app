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
/// - Anchor on the device's native output config so the rate/format it's
///   actually running at is always advertised and ranked first. Without
///   this anchor, the server picks from a hardcoded preference list (48k
///   first) and cpal/the OS ends up resampling into the device's native
///   rate.
/// - Supplement with other common rates that `supported_output_configs()`
///   confirms the device can handle, for source material whose rate happens
///   to match one of them exactly.
/// - Advertise stereo only, matching the current playback path. A device
///   whose native config is mono is skipped for the native anchor (a stereo
///   stream won't open on a mono device anyway).
/// - Prefer 24-bit at the native rate (higher quality through the matched-
///   rate, no-resample path), but prefer 16-bit at non-native rates for
///   broadest server/codec compatibility.
///
/// Windows shared-mode WASAPI makes the anchor doubly important:
/// `supported_output_configs()` there only reports the device's currently
/// configured rate, so a DAC set to an uncommon rate (352800Hz, 384000Hz,
/// etc.) would otherwise fall through to the 48/44.1kHz fallback and fail
/// to open a stream.
pub fn derive_supported_pcm_formats(device: Option<&cpal::Device>) -> Vec<SupportedPcmFormat> {
    let Some(device) = device else {
        return vec![];
    };

    let caps = extract_capabilities(device);
    build_formats(&caps)
}

/// Native (current default) output format of a device, as far as we care
/// for negotiation. Intentionally decoupled from cpal types so `build_formats`
/// can be unit-tested with synthetic inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NativeFormat {
    channels: u16,
    sample_rate: u32,
    supports_24bit: bool,
}

/// One channels/rate/format range reported by the device, in the same
/// cpal-decoupled shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfigRange {
    channels: u16,
    min_sample_rate: u32,
    max_sample_rate: u32,
    supports_24bit: bool,
}

/// Aggregate device capabilities — everything `build_formats` needs, with
/// no cpal types leaking in. `Default` gives us the "device reported
/// nothing" case for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DeviceCapabilities {
    native: Option<NativeFormat>,
    ranges: Vec<ConfigRange>,
}

/// Glue layer between cpal and `build_formats`. Also logs the detected
/// native config so this detail is visible in the runtime logs without
/// requiring `build_formats` to know about cpal types.
fn extract_capabilities(device: &cpal::Device) -> DeviceCapabilities {
    let default_cfg = device.default_output_config().ok();

    if let Some(cfg) = default_cfg.as_ref() {
        eprintln!(
            "[Sendspin] Device native output config: {}Hz, {:?}, {}ch",
            cfg.sample_rate(),
            cfg.sample_format(),
            cfg.channels()
        );
    }

    let native = default_cfg.map(|c| NativeFormat {
        channels: c.channels(),
        sample_rate: c.sample_rate(),
        supports_24bit: sample_format_supports_24bit(c.sample_format()),
    });

    let ranges = device
        .supported_output_configs()
        .map(|configs| {
            configs
                .map(|cfg| ConfigRange {
                    channels: cfg.channels(),
                    min_sample_rate: cfg.min_sample_rate(),
                    max_sample_rate: cfg.max_sample_rate(),
                    supports_24bit: sample_format_supports_24bit(cfg.sample_format()),
                })
                .collect()
        })
        .unwrap_or_default();

    DeviceCapabilities { native, ranges }
}

/// Produce the advertised format list from extracted device capabilities.
/// Exists as a separate function so it can be tested against synthetic inputs.
fn build_formats(caps: &DeviceCapabilities) -> Vec<SupportedPcmFormat> {
    let mut collected = BTreeSet::new();

    // Native anchor — the no-resample path. Skip if the device's current
    // config is mono; a stereo stream can't open on a mono output and
    // advertising it would invite a confusing stream-open failure.
    if let Some(native) = caps.native {
        if native.channels >= 2 {
            if native.supports_24bit {
                collected.insert(SupportedPcmFormat {
                    channels: 2,
                    sample_rate: native.sample_rate,
                    bit_depth: 24,
                });
            }
            collected.insert(SupportedPcmFormat {
                channels: 2,
                sample_rate: native.sample_rate,
                bit_depth: 16,
            });
        }
    }

    // Supplement with other common rates the device reports as supported,
    // for source material whose rate happens to match one of them exactly.
    let preferred_rates = [48_000u32, 44_100u32, 96_000u32, 192_000u32, 384_000u32];
    for range in &caps.ranges {
        if range.channels < 2 {
            continue;
        }
        for rate in preferred_rates {
            if rate < range.min_sample_rate || rate > range.max_sample_rate {
                continue;
            }
            collected.insert(SupportedPcmFormat {
                channels: 2,
                sample_rate: rate,
                bit_depth: 16,
            });
            if range.supports_24bit {
                collected.insert(SupportedPcmFormat {
                    channels: 2,
                    sample_rate: rate,
                    bit_depth: 24,
                });
            }
        }
    }

    let native_rate = caps.native.map(|n| n.sample_rate);
    let mut result: Vec<_> = collected.into_iter().collect();
    result.sort_by_key(|f| sort_key(*f, native_rate));
    result
}

/// Sort key for advertised format preference.
///
/// Rate ordering: the device's native rate ranks first (zero resampling),
/// then the common-rate ladder, then any other rates the device reported.
///
/// Depth ordering within a rate is asymmetric on purpose:
/// - At the native rate, 24-bit ranks first — we trust the no-resample
///   path enough to prefer the higher-quality representation.
/// - At non-native rates, 16-bit ranks first — more conservative, broader
///   server/codec compatibility for the resample-required path.
fn sort_key(f: SupportedPcmFormat, native_rate: Option<u32>) -> (u32, u32, u32, u16) {
    let rate_rank = if Some(f.sample_rate) == native_rate {
        0
    } else {
        match f.sample_rate {
            48_000 => 1,
            44_100 => 2,
            96_000 => 3,
            192_000 => 4,
            384_000 => 5,
            _ => 6,
        }
    };
    let depth_rank = if rate_rank == 0 {
        // Native rate: 24-bit first.
        u32::from(f.bit_depth == 16)
    } else {
        // Non-native rate: 16-bit first.
        u32::from(f.bit_depth != 16)
    };
    (rate_rank, depth_rank, f.sample_rate, f.bit_depth)
}

/// Whether a cpal sample format can carry 24-bit PCM content.
///
/// Currently limited to the explicit 24-bit integer formats. Broader
/// detection — F32/F64/I32/U32/I64/U64 all carry at least 24 bits of
/// precision and so could safely advertise 24-bit — is a follow-up.
fn sample_format_supports_24bit(fmt: cpal::SampleFormat) -> bool {
    matches!(fmt, cpal::SampleFormat::I24 | cpal::SampleFormat::U24)
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

    fn pcm(rate: u32, depth: u16) -> SupportedPcmFormat {
        SupportedPcmFormat {
            channels: 2,
            sample_rate: rate,
            bit_depth: depth,
        }
    }

    // ---- sort_key ---------------------------------------------------------

    #[test]
    fn sort_key_puts_native_rate_first() {
        // When the device is running at 96kHz, 96kHz must outrank 48kHz even
        // though 48kHz is the top of the "common preference" ladder.
        assert!(sort_key(pcm(96_000, 16), Some(96_000)) < sort_key(pcm(48_000, 16), Some(96_000)));
    }

    #[test]
    fn sort_key_follows_common_ladder_when_no_native_rate() {
        // With no default config available, fall back to the hardcoded order.
        assert!(sort_key(pcm(48_000, 16), None) < sort_key(pcm(44_100, 16), None));
        assert!(sort_key(pcm(44_100, 16), None) < sort_key(pcm(96_000, 16), None));
        assert!(sort_key(pcm(96_000, 16), None) < sort_key(pcm(192_000, 16), None));
        assert!(sort_key(pcm(192_000, 16), None) < sort_key(pcm(384_000, 16), None));
    }

    #[test]
    fn sort_key_handles_uncommon_rate_at_native() {
        // A device natively at a rate that isn't on the common ladder
        // (e.g. 88.2kHz, 176.4kHz, or anything else) must still anchor
        // correctly — the rate doesn't have to be enumerated in the ladder
        // for the anchor to put it first.
        assert!(sort_key(pcm(88_200, 16), Some(88_200)) < sort_key(pcm(48_000, 16), Some(88_200)));
        assert!(
            sort_key(pcm(176_400, 24), Some(176_400)) < sort_key(pcm(192_000, 16), Some(176_400))
        );
    }

    #[test]
    fn sort_key_prefers_24bit_at_native_rate() {
        // At the native rate, 24-bit ranks first — the no-resample path is
        // trusted enough to prefer the higher-quality representation.
        assert!(sort_key(pcm(96_000, 24), Some(96_000)) < sort_key(pcm(96_000, 16), Some(96_000)));
        assert!(sort_key(pcm(44_100, 24), Some(44_100)) < sort_key(pcm(44_100, 16), Some(44_100)));
    }

    #[test]
    fn sort_key_prefers_16bit_at_non_native_rates() {
        // At non-native rates, 16-bit ranks first for broader compatibility.
        assert!(sort_key(pcm(48_000, 16), None) < sort_key(pcm(48_000, 24), None));
        // Even when a different native rate is set, non-native 16-bit still
        // outranks non-native 24-bit.
        assert!(sort_key(pcm(48_000, 16), Some(96_000)) < sort_key(pcm(48_000, 24), Some(96_000)));
    }

    #[test]
    fn sort_key_native_rate_at_any_depth_outranks_any_non_native_rate() {
        // Native rate always wins on rate_rank (0 vs ≥1), regardless of
        // bit depth on either side.
        assert!(sort_key(pcm(96_000, 24), Some(96_000)) < sort_key(pcm(48_000, 16), Some(96_000)));
        assert!(sort_key(pcm(96_000, 16), Some(96_000)) < sort_key(pcm(48_000, 24), Some(96_000)));
    }

    // ---- sample_format_supports_24bit ------------------------------------

    #[test]
    fn sample_format_supports_24bit_includes_explicit_24bit_formats() {
        assert!(sample_format_supports_24bit(cpal::SampleFormat::I24));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::U24));
    }

    #[test]
    fn sample_format_supports_24bit_excludes_low_precision_formats() {
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::I8));
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::U8));
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::I16));
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::U16));
    }

    // ---- derive_supported_pcm_formats (entry point) ----------------------

    #[test]
    fn derive_returns_empty_when_device_is_none() {
        assert_eq!(
            derive_supported_pcm_formats(None),
            Vec::<SupportedPcmFormat>::new()
        );
    }

    // ---- build_formats (pure logic over extracted capabilities) ---------

    #[test]
    fn build_formats_returns_empty_when_capabilities_are_empty() {
        assert!(build_formats(&DeviceCapabilities::default()).is_empty());
    }

    #[test]
    fn build_formats_anchors_on_stereo_native_with_24bit_first() {
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 96_000,
                supports_24bit: true,
            }),
            ranges: vec![],
        };
        let formats = build_formats(&caps);
        assert_eq!(formats, vec![pcm(96_000, 24), pcm(96_000, 16)]);
    }

    #[test]
    fn build_formats_anchors_on_stereo_native_without_24bit_support() {
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 48_000,
                supports_24bit: false,
            }),
            ranges: vec![],
        };
        assert_eq!(build_formats(&caps), vec![pcm(48_000, 16)]);
    }

    #[test]
    fn build_formats_skips_mono_native() {
        // A stereo stream can't open on a mono output — advertising it
        // would invite a confusing stream-open failure.
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 1,
                sample_rate: 48_000,
                supports_24bit: true,
            }),
            ranges: vec![],
        };
        assert!(build_formats(&caps).is_empty());
    }

    #[test]
    fn build_formats_skips_mono_ranges() {
        let caps = DeviceCapabilities {
            native: None,
            ranges: vec![ConfigRange {
                channels: 1,
                min_sample_rate: 44_100,
                max_sample_rate: 192_000,
                supports_24bit: true,
            }],
        };
        assert!(build_formats(&caps).is_empty());
    }

    #[test]
    fn build_formats_uses_only_ranges_when_native_missing() {
        // No default_output_config — WASAPI could be in an odd state, or
        // a non-cpal-friendly driver. We should still supplement from the
        // supported_output_configs() ranges.
        let caps = DeviceCapabilities {
            native: None,
            ranges: vec![ConfigRange {
                channels: 2,
                min_sample_rate: 44_100,
                max_sample_rate: 96_000,
                supports_24bit: false,
            }],
        };
        let formats = build_formats(&caps);
        // Preferred rates within [44100, 96000] are 48k, 44.1k, 96k — all
        // at 16-bit since the range doesn't carry 24-bit. Order: the
        // non-native ladder, 48k first.
        assert_eq!(
            formats,
            vec![pcm(48_000, 16), pcm(44_100, 16), pcm(96_000, 16)]
        );
    }

    #[test]
    fn build_formats_combines_native_and_ranges_with_native_first() {
        // Device at 96kHz native, ranges cover a broader span including
        // 48k and 44.1k. Expected: native 96k at 24-bit first, then native
        // 96k at 16-bit, then non-native rates from the ladder.
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 96_000,
                supports_24bit: true,
            }),
            ranges: vec![ConfigRange {
                channels: 2,
                min_sample_rate: 44_100,
                max_sample_rate: 96_000,
                supports_24bit: false,
            }],
        };
        let formats = build_formats(&caps);
        // 96k/16 is in the set from both the anchor and the range, dedup'd
        // by BTreeSet. Final order: native 96k/24, native 96k/16, then
        // non-native ladder (48k, 44.1k — within the declared range,
        // 16-bit only because the range's sample format doesn't carry
        // 24-bit).
        assert_eq!(
            formats,
            vec![
                pcm(96_000, 24),
                pcm(96_000, 16),
                pcm(48_000, 16),
                pcm(44_100, 16),
            ]
        );
    }

    #[test]
    fn build_formats_dedups_when_native_and_range_overlap() {
        // The ranges include the native rate — ensure we don't emit both
        // a "native 48k/16" and a "non-native 48k/16" (they'd be the same
        // entry), and that the final one is ordered as native.
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 48_000,
                supports_24bit: false,
            }),
            ranges: vec![ConfigRange {
                channels: 2,
                min_sample_rate: 44_100,
                max_sample_rate: 48_000,
                supports_24bit: false,
            }],
        };
        let formats = build_formats(&caps);
        // 48k/16 appears exactly once. Native comes first.
        assert_eq!(formats.iter().filter(|f| **f == pcm(48_000, 16)).count(), 1);
        assert_eq!(formats[0], pcm(48_000, 16));
        assert!(formats.contains(&pcm(44_100, 16)));
    }
}
