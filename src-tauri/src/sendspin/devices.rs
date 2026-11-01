//! Audio device enumeration and selection using cpal
//!
//! This module provides cross-platform audio device enumeration
//! for selecting output devices in the Sendspin client.

use cpal::traits::{DeviceTrait, HostTrait};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedOutputDevice {
    pub stable_id: Option<String>,
    pub legacy_name: Option<String>,
    pub legacy_migration_complete: bool,
}

pub fn legacy_match_id<'a>(
    devices: impl IntoIterator<Item = &'a AudioDevice>,
    name: &str,
) -> LegacyMatch<'a> {
    let mut matches = devices.into_iter().filter(|device| device.name == name);
    match (matches.next(), matches.next()) {
        (None, _) => LegacyMatch::Missing,
        (Some(device), None) => LegacyMatch::Unique(device),
        _ => LegacyMatch::Ambiguous,
    }
}

pub enum LegacyMatch<'a> {
    Missing,
    Unique(&'a AudioDevice),
    Ambiguous,
}

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

    let default_device_id = host
        .default_output_device()
        .and_then(|device| device_identity(&device));

    let devices = host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate devices: {}", e))?;

    let mut result = Vec::new();

    for device in devices {
        let Some(id) = device_identity(&device) else {
            continue;
        };
        let Ok(desc) = device.description() else {
            continue;
        };
        let name = desc.name().to_string();
        let is_default = default_device_id
            .as_ref()
            .is_some_and(|default| default == &id);

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
            a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id))
        }
    });

    if let Err(error) = crate::settings::migrate_legacy_audio_device(&result) {
        log::warn!("[Settings] Could not save audio device migration: {error}");
    }
    Ok(result)
}

/// Get a device by its exact CPAL identity.
pub fn get_device_by_id(device_id: &str) -> Result<cpal::Device, String> {
    let devices = cpal::default_host()
        .output_devices()
        .map_err(|e| format!("Failed to enumerate devices: {e}"))?;
    find_output_by_id(devices, device_id, |device| {
        device_identity(device).ok_or_else(|| "Device identity unavailable".to_string())
    })
}

pub fn resolve_selected_output_device(
    selection: &SelectedOutputDevice,
) -> Result<cpal::Device, String> {
    if let Some(id) = selection.stable_id.as_deref() {
        return resolve_output_device(Some(id));
    }
    if !selection.legacy_migration_complete {
        if let Some(name) = selection.legacy_name.as_deref() {
            let devices = list_devices();
            let effective = crate::settings::reconcile_selected_output_device(
                selection,
                &crate::settings::get_settings(),
            );
            if effective != *selection {
                return resolve_selected_output_device(&effective);
            }
            match devices {
                Ok(devices) => match legacy_match_id(&devices, name) {
                    LegacyMatch::Unique(device) => return resolve_output_device(Some(&device.id)),
                    LegacyMatch::Ambiguous => {
                        log::warn!(
                            "[Sendspin] Multiple outputs named {name}; using system default"
                        );
                    }
                    LegacyMatch::Missing => {
                        log::warn!(
                            "[Sendspin] Output named {name} is unavailable; using system default"
                        );
                    }
                },
                Err(error) => {
                    log::warn!("[Sendspin] Could not enumerate outputs: {error}");
                }
            }
        }
    }
    resolve_output_device(None)
}

/// Find an enumerated output by its stable cpal identity, never by its display name.
/// Skip outputs whose identities cannot be read; only an exact match may be used.
fn find_output_by_id<T>(
    outputs: impl IntoIterator<Item = T>,
    wanted: &str,
    mut identity: impl FnMut(&T) -> Result<String, String>,
) -> Result<T, String> {
    for output in outputs {
        if identity(&output).is_ok_and(|id| id == wanted) {
            return Ok(output);
        }
    }
    Err(format!(
        "Default output device {wanted} was not found among enumerated outputs"
    ))
}

/// Get the default output device. On Windows, the cpal default is a virtual
/// endpoint; resolve its identity to an enumerated, concrete output instead.
#[allow(dead_code)]
pub fn get_default_device() -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let default = host
        .default_output_device()
        .ok_or_else(|| "No default output device available".to_string())?;

    #[cfg(target_os = "windows")]
    {
        let id = default
            .id()
            .map_err(|e| format!("Failed to identify default output device: {e}"))?
            .to_string();
        let outputs = host
            .output_devices()
            .map_err(|e| format!("Failed to enumerate output devices: {e}"))?;
        find_output_by_id(outputs, &id, |device| {
            device
                .id()
                .map(|id| id.to_string())
                .map_err(|e| format!("Failed to identify enumerated output device: {e}"))
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        Ok(default)
    }
}

/// Resolve output device based on optional device ID. A missing configured
/// device falls back to the system default, but an unresolvable default is an
/// error: playback must not silently select a virtual or automatic endpoint.
pub fn resolve_output_device(device_id: Option<&str>) -> Result<cpal::Device, String> {
    if let Some(id) = device_id {
        match get_device_by_id(id) {
            Ok(device) => {
                let name = device.description().ok().map_or_else(
                    || "<unknown device>".to_string(),
                    |desc| desc.name().to_string(),
                );
                log::info!("[Sendspin] Using configured output device: {}", name);
                return Ok(device);
            }
            Err(e) => {
                log::warn!(
                    "[Sendspin] Failed to get device {}: {}, falling back to default output",
                    id,
                    e
                );
            }
        }
    }

    let device = get_default_device().map_err(|e| {
        log::error!("[Sendspin] Failed to resolve default output device: {e}");
        e
    })?;
    let name = device.description().ok().map_or_else(
        || "<unknown device>".to_string(),
        |desc| desc.name().to_string(),
    );
    log::info!("[Sendspin] Using default output device: {}", name);
    Ok(device)
}

/// Stable identifier for a resolved output device, or `None` if the backend
/// cannot provide one. Used to tell whether an already-open output stream is
/// still bound to the device a new stream would resolve to.
pub fn device_identity(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
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
        log::debug!(
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
/// Anything with at least 24 bits of precision qualifies, not just the explicit
/// 24-bit integer formats. This matters on `CoreAudio`, where cpal reports F32 for
/// every output device — so restricting the check to I24/U24 meant no macOS
/// device ever advertised 24-bit, however capable the hardware was.
///
/// F32 has a 24-bit mantissa, so it carries a 24-bit sample exactly; the wider
/// integer and F64 formats have headroom to spare. DSD formats are excluded:
/// they are 1-bit sigma-delta bitstreams, not PCM, and the playback path only
/// handles PCM.
fn sample_format_supports_24bit(fmt: cpal::SampleFormat) -> bool {
    matches!(
        fmt,
        cpal::SampleFormat::I24
            | cpal::SampleFormat::U24
            | cpal::SampleFormat::I32
            | cpal::SampleFormat::U32
            | cpal::SampleFormat::I64
            | cpal::SampleFormat::U64
            | cpal::SampleFormat::F32
            | cpal::SampleFormat::F64
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a guest-local Windows output endpoint. Never enqueues audio.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore = "requires a Windows playback endpoint; opens muted silent streams"]
    fn concrete_default_opens_and_reopens_silently() {
        use cpal::traits::{DeviceTrait, HostTrait};
        use parking_lot::Mutex;
        use sendspin::audio::{AudioFormat, Codec, SyncedPlayer, SyncedPlayerConfig};
        use sendspin::sync::ClockSync;
        use sendspin::DefaultClock;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let host = cpal::default_host();
        let default = host.default_output_device().expect("system default output");
        let default_id = default.id().expect("default endpoint ID").to_string();
        let resolved = resolve_output_device(None).expect("resolve concrete default output");
        assert_eq!(
            device_identity(&resolved).as_deref(),
            Some(default_id.as_str())
        );
        let enumerated = host.output_devices().expect("enumerate output devices");
        assert!(enumerated
            .into_iter()
            .any(|device| { device.id().is_ok_and(|id| id.to_string() == default_id) }));
        let description = resolved.description().expect("concrete description");
        let name = description.name().to_string();
        let configured =
            resolve_output_device(Some(&default_id)).expect("configured stable output");
        assert_eq!(device_identity(&configured), device_identity(&resolved));
        let native = resolved
            .default_output_config()
            .expect("native output format");
        let formats = derive_supported_pcm_formats(Some(&resolved));
        assert!(
            !formats.is_empty(),
            "concrete output has no advertised formats"
        );
        println!(
            "concrete default: {name}, id={default_id}, native={}ch {}Hz {:?}, formats={}",
            native.channels(),
            native.sample_rate(),
            native.sample_format(),
            formats.len()
        );

        for attempt in 1..=2 {
            let device = resolve_output_device(None).expect("re-resolve concrete output");
            assert_eq!(
                device_identity(&device).as_deref(),
                Some(default_id.as_str())
            );
            let callbacks = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&callbacks);
            let player = SyncedPlayer::with_process_callback(
                AudioFormat {
                    codec: Codec::Pcm,
                    channels: 2,
                    sample_rate: native.sample_rate(),
                    bit_depth: 16,
                    codec_header: None,
                },
                Arc::new(Mutex::new(ClockSync::new(Arc::new(DefaultClock::new())))),
                SyncedPlayerConfig {
                    device: Some(device),
                    muted: true,
                    ..SyncedPlayerConfig::new()
                },
                Box::new(move |samples| {
                    assert!(samples.iter().all(|sample| *sample == 0.0));
                    observed.fetch_add(1, Ordering::Relaxed);
                }),
            )
            .unwrap_or_else(|error| panic!("concrete output open {attempt} failed: {error}"));
            let deadline = Instant::now() + Duration::from_secs(2);
            while callbacks.load(Ordering::Relaxed) < 3 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !player.has_error(),
                "stream error: {:?}",
                player.take_error()
            );
            assert!(
                callbacks.load(Ordering::Relaxed) >= 3,
                "no silent callbacks on open {attempt}"
            );
            println!(
                "concrete output open {attempt}: {} silent callbacks",
                callbacks.load(Ordering::Relaxed)
            );
            drop(player);
        }
    }

    #[test]
    fn default_output_matches_stable_id_not_display_name() {
        let outputs = [
            ("other-id", "Same friendly name"),
            ("default-id", "Same friendly name"),
        ];
        assert_eq!(
            find_output_by_id(outputs, "default-id", |device| Ok(device.0.to_string())),
            Ok(("default-id", "Same friendly name"))
        );
    }

    #[test]
    fn missing_default_identity_does_not_select_virtual_or_first_output() {
        let outputs = [("other-id", "Default friendly name")];
        assert!(
            find_output_by_id(outputs, "virtual-id", |device| Ok(device.0.to_string()))
                .unwrap_err()
                .contains("virtual-id")
        );
        assert!(
            find_output_by_id(std::iter::empty::<(&str, &str)>(), "default-id", |device| {
                Ok(device.0.to_string())
            })
            .is_err()
        );
    }

    #[test]
    fn unreadable_unrelated_identity_does_not_hide_later_exact_match() {
        let outputs = [
            ("unreadable-id", "Default friendly name"),
            ("default-id", "Other"),
        ];
        assert_eq!(
            find_output_by_id(outputs, "default-id", |device| {
                if device.0 == "unreadable-id" {
                    Err("identity unavailable".to_string())
                } else {
                    Ok(device.0.to_string())
                }
            }),
            Ok(("default-id", "Other"))
        );
    }

    #[test]
    fn all_unreadable_identities_do_not_select_any_output() {
        let outputs = [("one", "Default friendly name"), ("two", "Other")];
        let error = find_output_by_id(outputs, "default-id", |_| {
            Err("identity unavailable".to_string())
        })
        .unwrap_err();
        assert!(error.contains("default-id"));
    }

    #[test]
    fn test_list_devices() {
        let devices = list_devices();
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
    fn duplicate_names_preserve_raw_names_and_exact_ids() {
        let devices = vec![
            AudioDevice {
                id: "id:one".into(),
                name: "Speakers".into(),
                is_default: true,
                sample_rates: vec![],
                max_channels: 2,
            },
            AudioDevice {
                id: "id:two".into(),
                name: "Speakers".into(),
                is_default: false,
                sample_rates: vec![],
                max_channels: 2,
            },
        ];
        assert_eq!(devices[0].name, devices[1].name);
        assert!(serde_json::to_value(&devices[0])
            .unwrap()
            .get("label")
            .is_none());
        assert_eq!(devices[0].id, "id:one");
        assert!(devices[0].is_default);
        assert!(matches!(
            legacy_match_id(&devices, "Speakers"),
            LegacyMatch::Ambiguous
        ));
        assert_eq!(
            find_output_by_id(&devices, "id:two", |device| Ok(device.id.clone()))
                .unwrap()
                .id,
            "id:two"
        );
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

    #[test]
    fn sample_format_supports_24bit_includes_wider_formats() {
        // F32 is the one that matters in practice: it is what cpal reports for
        // every output device on CoreAudio, so excluding it meant no macOS
        // device could ever advertise 24-bit.
        assert!(sample_format_supports_24bit(cpal::SampleFormat::F32));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::F64));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::I32));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::U32));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::I64));
        assert!(sample_format_supports_24bit(cpal::SampleFormat::U64));
    }

    #[test]
    fn sample_format_supports_24bit_excludes_dsd_formats() {
        // DSD is a 1-bit sigma-delta bitstream, not PCM. The playback path
        // rejects anything that is not 16- or 24-bit PCM, so advertising
        // 24-bit for a DSD-only config would promise a stream it cannot open.
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::DsdU8));
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::DsdU16));
        assert!(!sample_format_supports_24bit(cpal::SampleFormat::DsdU32));
    }

    #[test]
    fn build_formats_advertises_24bit_for_a_coreaudio_style_f32_device() {
        // Regression test for the macOS case: an F32-only device that supports
        // 24-bit hardware must still get a 24-bit entry at its native rate.
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 96_000,
                supports_24bit: sample_format_supports_24bit(cpal::SampleFormat::F32),
            }),
            ranges: vec![],
        };
        let formats = build_formats(&caps);
        assert!(
            formats
                .iter()
                .any(|f| f.bit_depth == 24 && f.sample_rate == 96_000),
            "expected a 24-bit entry at the native rate, got {formats:?}"
        );
        assert_eq!(
            formats.first().map(|f| f.bit_depth),
            Some(24),
            "24-bit must rank first at the native rate"
        );
    }

    #[test]
    fn build_formats_advertises_24bit_from_f32_ranges_with_native_rate_preference() {
        let caps = DeviceCapabilities {
            native: Some(NativeFormat {
                channels: 2,
                sample_rate: 96_000,
                supports_24bit: false,
            }),
            ranges: vec![ConfigRange {
                channels: 2,
                min_sample_rate: 44_100,
                max_sample_rate: 96_000,
                supports_24bit: sample_format_supports_24bit(cpal::SampleFormat::F32),
            }],
        };

        // The range, not the native config, supplies every 24-bit entry.
        // Prefer 24-bit at the native rate, but 16-bit at other rates.
        assert_eq!(
            build_formats(&caps),
            vec![
                pcm(96_000, 24),
                pcm(96_000, 16),
                pcm(48_000, 16),
                pcm(48_000, 24),
                pcm(44_100, 16),
                pcm(44_100, 24),
            ]
        );
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
        assert_eq!(build_formats(&DeviceCapabilities::default()), []);
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
        assert_eq!(build_formats(&caps), []);
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
        assert_eq!(build_formats(&caps), []);
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
