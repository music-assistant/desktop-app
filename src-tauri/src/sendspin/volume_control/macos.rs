//! macOS volume control implementation using `CoreAudio`

use super::{VolumeChangeCallback, VolumeControlImpl, VolumeSample, VolumeTarget};
use coreaudio_sys::*;
use objc2_core_foundation::{CFRetained, CFString};
use parking_lot::RwLock;

const VIRTUAL_MAIN_VOLUME: AudioObjectPropertySelector = 0x766d_7663;

fn select_uid_output<'a>(
    uid: &str,
    devices: impl Iterator<Item = (AudioDeviceID, &'a str, bool)>,
) -> Option<AudioDeviceID> {
    devices
        .filter(|(_, candidate, output)| *output && *candidate == uid)
        .map(|(id, _, _)| id)
        .next()
}

fn select_volume_property(
    mut writable: impl FnMut(AudioObjectPropertySelector) -> bool,
) -> Option<AudioObjectPropertySelector> {
    [kAudioDevicePropertyVolumeScalar, VIRTUAL_MAIN_VOLUME]
        .into_iter()
        .find(|selector| writable(*selector))
}
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct MacOSVolumeControl {
    target: Arc<RwLock<VolumeTarget>>,
    generation: Arc<AtomicU64>,
    // Handle to the worker thread (joined on drop)
    worker_thread: Option<std::thread::JoinHandle<()>>,
    // Timestamp of last successful self-initiated change (to prevent feedback loops)
    last_self_change: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OutputState {
    device_id: AudioDeviceID,
    volume: u8,
    muted: bool,
}

// A device switch must be reported even during the self-change grace period or
// when both devices happen to have the same volume and mute values.
fn should_report(previous: Option<OutputState>, current: OutputState, recent_change: bool) -> bool {
    previous.is_none_or(|previous| {
        previous.device_id != current.device_id || (!recent_change && previous != current)
    })
}

struct StateCache {
    revision: u64,
    state: Option<OutputState>,
}

impl StateCache {
    fn reset_for(&mut self, revision: u64) {
        if self.revision != revision {
            self.revision = revision;
            self.state = None;
        }
    }

    fn should_report(
        &mut self,
        revision: u64,
        current_revision: u64,
        current: OutputState,
        recent_change: bool,
    ) -> bool {
        self.reset_for(current_revision);
        revision == current_revision && should_report(self.state, current, recent_change)
    }
}

// No native mute property means unmuted, not a failed volume sample. Only
// absence gets this fallback; failures reading an existing property propagate.
fn read_optional_mute(
    has_property: bool,
    read: impl FnOnce() -> Result<u32, String>,
) -> Result<bool, String> {
    if has_property {
        read().map(|value| value != 0)
    } else {
        Ok(false)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl MacOSVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        target: Arc<RwLock<VolumeTarget>>,
        generation: Arc<AtomicU64>,
    ) -> Box<dyn VolumeControlImpl + Send> {
        Box::new(Self {
            target,
            generation,
            worker_thread: None,
            last_self_change: Arc::new(AtomicU64::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    fn target_device(target: &VolumeTarget) -> Result<AudioDeviceID, String> {
        let VolumeTarget::Device { native_id } = target else {
            return Err("Opened output has no concrete CoreAudio UID".into());
        };
        if native_id.is_empty() {
            return Err("Opened output UID is empty".into());
        }
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let devices = Self::object_list(kAudioObjectSystemObject, &address)?;
        let candidates: Vec<_> = devices
            .into_iter()
            .filter_map(|id| {
                let uid = Self::device_uid(id).ok()?;
                let address = AudioObjectPropertyAddress {
                    mSelector: kAudioDevicePropertyStreams,
                    mScope: kAudioDevicePropertyScopeOutput,
                    mElement: kAudioObjectPropertyElementMain,
                };
                let output =
                    Self::object_list(id, &address).is_ok_and(|streams| !streams.is_empty());
                Some((id, uid, output))
            })
            .collect();
        select_uid_output(
            native_id,
            candidates
                .iter()
                .map(|(id, uid, output)| (*id, uid.as_str(), *output)),
        )
        .ok_or_else(|| format!("Opened CoreAudio output UID {native_id:?} unavailable"))
    }

    fn object_list(
        object: AudioObjectID,
        address: &AudioObjectPropertyAddress,
    ) -> Result<Vec<AudioObjectID>, String> {
        let mut size = 0;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(object, address, 0, ptr::null(), &raw mut size)
        };
        if status != 0 {
            return Err(format!("Failed to enumerate CoreAudio objects: {}", status));
        }
        let mut objects = vec![0; size as usize / mem::size_of::<AudioObjectID>()];
        if objects.is_empty() {
            return Ok(objects);
        }
        // Typed storage keeps the property buffer aligned. A topology change
        // that grows the list causes a CoreAudio error, not an oversized write.
        size = (objects.len() * mem::size_of::<AudioObjectID>()) as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                address,
                0,
                ptr::null(),
                &raw mut size,
                objects.as_mut_ptr().cast(),
            )
        };
        if status != 0 {
            return Err(format!("Failed to read CoreAudio objects: {}", status));
        }
        objects.truncate(size as usize / mem::size_of::<AudioObjectID>());
        Ok(objects)
    }

    fn device_uid(device: AudioDeviceID) -> Result<String, String> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceUID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut name: *mut CFString = ptr::null_mut();
        let mut size = mem::size_of_val(&name) as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                &raw const address,
                0,
                ptr::null(),
                &raw mut size,
                std::ptr::addr_of_mut!(name).cast(),
            )
        };
        if status != 0 {
            return Err(format!("Failed to read CoreAudio device name: {}", status));
        }
        let name = std::ptr::NonNull::new(name)
            .ok_or_else(|| "CoreAudio returned a null device name".to_string())?;
        // CoreAudio transfers ownership of CFString-valued properties to callers.
        let name = unsafe { CFRetained::from_raw(name) };
        Ok(name.to_string())
    }

    fn volume_property(device: AudioDeviceID) -> Result<AudioObjectPropertyAddress, String> {
        let selector = select_volume_property(|selector| {
            Self::require_property(device, selector, true).is_ok()
        }).ok_or_else(|| format!(
            "Output device {} has no writable main volm or virtual main vmvc volume; hardware volume unavailable",
            device
        ))?;
        log::trace!(
            "[VolumeControl] Output device {} volume path: {}",
            device,
            if selector == VIRTUAL_MAIN_VOLUME {
                "vmvc"
            } else {
                "volm"
            }
        );
        Self::require_property(device, selector, true)
    }

    fn require_property(
        device_id: AudioDeviceID,
        selector: AudioObjectPropertySelector,
        writable: bool,
    ) -> Result<AudioObjectPropertyAddress, String> {
        let address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };
        unsafe {
            if AudioObjectHasProperty(device_id, &raw const address) == 0 {
                return Err(format!(
                    "Default output device does not support property {}",
                    selector
                ));
            }
            if writable {
                let mut settable = 0;
                let status =
                    AudioObjectIsPropertySettable(device_id, &raw const address, &raw mut settable);
                if status != 0 || settable == 0 {
                    return Err(format!(
                        "Default output property {} is not writable (status {})",
                        selector, status
                    ));
                }
            }
        }
        Ok(address)
    }

    fn set_volume_scalar(device_id: AudioDeviceID, volume_scalar: f32) -> Result<(), String> {
        let address = Self::volume_property(device_id)?;
        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &raw const address,
                0,
                ptr::null(),
                mem::size_of::<f32>() as u32,
                std::ptr::addr_of!(volume_scalar).cast(),
            )
        };
        if status != 0 {
            return Err(format!("Failed to set volume: {}", status));
        }
        Ok(())
    }

    fn read_volume(device_id: AudioDeviceID) -> Result<u8, String> {
        let address = Self::volume_property(device_id)?;
        let mut volume: f32 = 0.0;
        let mut size = mem::size_of::<f32>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                device_id,
                &raw const address,
                0,
                ptr::null(),
                &raw mut size,
                std::ptr::addr_of_mut!(volume).cast(),
            )
        };
        if status != 0 {
            return Err(format!("Failed to get volume: {}", status));
        }
        if !volume.is_finite() {
            return Err("Default output device returned invalid volume".to_string());
        }
        Ok((volume.clamp(0.0, 1.0) * 100.0) as u8)
    }

    fn read_mute(device_id: AudioDeviceID) -> Result<bool, String> {
        let address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };
        let has_property = unsafe { AudioObjectHasProperty(device_id, &raw const address) != 0 };
        read_optional_mute(has_property, || {
            let mut mute_value: u32 = 0;
            let mut size = mem::size_of::<u32>() as u32;
            let status = unsafe {
                AudioObjectGetPropertyData(
                    device_id,
                    &raw const address,
                    0,
                    ptr::null(),
                    &raw mut size,
                    std::ptr::addr_of_mut!(mute_value).cast(),
                )
            };
            if status != 0 {
                return Err(format!("Failed to get mute state: {}", status));
            }
            Ok(mute_value)
        })
    }

    fn read_output_state(target: &Arc<RwLock<VolumeTarget>>) -> Result<OutputState, String> {
        let selected = target.read().clone();
        let device_id = Self::target_device(&selected)?;
        let state = OutputState {
            device_id,
            volume: Self::read_volume(device_id)?,
            muted: Self::read_mute(device_id)?,
        };
        // Do not publish a sample of an output that stopped being the target
        // while its properties were being read. Retry on the next polling tick.
        if *target.read() != selected || Self::target_device(&selected)? != device_id {
            return Err("Target output changed while reading volume state".to_string());
        }
        Ok(state)
    }
}

impl VolumeControlImpl for MacOSVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        let device_id = Self::target_device(&self.target.read().clone())?;
        Self::set_volume_scalar(device_id, f32::from(volume.min(100)) / 100.0)?;
        self.last_self_change.store(now_ms(), Ordering::Relaxed);
        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        let device_id = Self::target_device(&self.target.read().clone())?;
        let address = Self::require_property(device_id, kAudioDevicePropertyMute, true)?;
        let mute_value: u32 = u32::from(muted);
        let status = unsafe {
            AudioObjectSetPropertyData(
                device_id,
                &raw const address,
                0,
                ptr::null(),
                mem::size_of::<u32>() as u32,
                std::ptr::addr_of!(mute_value).cast(),
            )
        };
        if status != 0 {
            return Err(format!("Failed to set mute: {}", status));
        }
        self.last_self_change.store(now_ms(), Ordering::Relaxed);
        Ok(())
    }

    fn get_volume(&self) -> Result<u8, String> {
        Self::read_volume(Self::target_device(&self.target.read().clone())?)
    }

    fn get_mute(&self) -> Result<bool, String> {
        Self::read_mute(Self::target_device(&self.target.read().clone())?)
    }

    fn is_available(&self) -> bool {
        Self::target_device(&self.target.read().clone())
            .and_then(Self::volume_property)
            .is_ok()
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
        self.stop_flag = Arc::new(AtomicBool::new(false));

        // Use polling instead of property listeners to avoid interfering with audio playback.
        // CoreAudio property listeners were causing static noise during playback.
        let last_self_change = Arc::clone(&self.last_self_change);
        let stop_flag = Arc::clone(&self.stop_flag);
        let target = Arc::clone(&self.target);
        let generation = Arc::clone(&self.generation);
        let initial_revision = {
            let _selected = target.read();
            generation.load(Ordering::SeqCst)
        };
        let initial_state = Self::read_output_state(&target).ok();
        let current_revision = {
            let _selected = target.read();
            generation.load(Ordering::SeqCst)
        };
        let mut cache = StateCache {
            revision: initial_revision,
            state: initial_state,
        };
        cache.reset_for(current_revision);
        self.worker_thread = Some(std::thread::spawn(move || {
            use std::time::Duration;
            const POLL_INTERVAL: Duration = Duration::from_secs(2);
            const SELF_CHANGE_GRACE_PERIOD: u64 = 1000; // milliseconds
            loop {
                std::thread::sleep(POLL_INTERVAL);
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let revision = {
                    let _selected = target.read();
                    generation.load(Ordering::SeqCst)
                };
                cache.reset_for(revision);
                let current = Self::read_output_state(&target);
                let _selected = target.read();
                let current_revision = generation.load(Ordering::SeqCst);
                if revision != current_revision {
                    cache.reset_for(current_revision);
                    continue;
                }
                let Ok(current) = current else {
                    cache.state = None;
                    continue;
                };
                let recent_change = now_ms()
                    .saturating_sub(last_self_change.load(Ordering::Relaxed))
                    < SELF_CHANGE_GRACE_PERIOD;
                if cache.should_report(revision, current_revision, current, recent_change) {
                    if callback
                        .send(VolumeSample {
                            revision,
                            volume: current.volume,
                            muted: current.muted,
                        })
                        .is_err()
                    {
                        break;
                    }
                    cache.state = Some(current);
                }
            }
        }));
        log::info!("[VolumeControl] macOS volume polling enabled (2s interval)");
        Ok(())
    }
}

impl Drop for MacOSVolumeControl {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        read_optional_mute, select_uid_output, select_volume_property, should_report, OutputState,
        StateCache, VIRTUAL_MAIN_VOLUME,
    };
    use coreaudio_sys::kAudioDevicePropertyVolumeScalar;

    #[test]
    fn uid_selects_only_exact_output_even_with_duplicate_names() {
        let devices = [
            (50, "uid-input", false),
            (51, "uid-output-a", true),
            (57, "uid-output-b", true),
        ];
        assert_eq!(
            select_uid_output("uid-output-b", devices.into_iter()),
            Some(57)
        );
        assert_eq!(select_uid_output("uid-input", devices.into_iter()), None);
        assert_eq!(select_uid_output("missing", devices.into_iter()), None);
    }

    #[test]
    fn volume_path_prefers_writable_main_then_virtual_main() {
        assert_eq!(
            select_volume_property(|_| true),
            Some(kAudioDevicePropertyVolumeScalar)
        );
        assert_eq!(
            select_volume_property(|selector| selector == VIRTUAL_MAIN_VOLUME),
            Some(VIRTUAL_MAIN_VOLUME)
        );
        // Missing or read-only properties must not advertise hardware control.
        assert_eq!(select_volume_property(|_| false), None);
    }

    #[test]
    fn absent_mute_property_allows_volume_notifications() {
        let muted = read_optional_mute(false, || panic!("absent mute must not be read"))
            .expect("absent mute is unmuted");
        assert!(!muted);
        let previous = OutputState { muted, ..state(1) };
        let current = OutputState {
            volume: 60,
            ..previous
        };
        assert!(should_report(Some(previous), current, false));
        assert!(should_report(Some(previous), state(2), true));
    }

    #[test]
    fn existing_mute_property_preserves_values_and_read_errors() {
        assert_eq!(read_optional_mute(true, || Ok(0)), Ok(false));
        assert_eq!(read_optional_mute(true, || Ok(1)), Ok(true));
        let error = "Failed to get mute state: -50".to_string();
        assert_eq!(read_optional_mute(true, || Err(error.clone())), Err(error));
    }

    fn state(device_id: u32) -> OutputState {
        OutputState {
            device_id,
            volume: 50,
            muted: false,
        }
    }

    #[test]
    fn revision_cache_reports_same_output_once_even_during_grace() {
        let current = state(1);
        let mut cache = StateCache {
            revision: 7,
            state: Some(current),
        };
        assert!(!cache.should_report(7, 7, current, false));
        assert!(cache.should_report(8, 8, current, true));
        cache.state = Some(current);
        assert!(!cache.should_report(8, 8, current, false));
    }

    #[test]
    fn revision_change_during_query_does_not_cache_old_sample() {
        let current = state(1);
        let mut cache = StateCache {
            revision: 7,
            state: Some(current),
        };
        assert!(!cache.should_report(7, 8, current, false));
        assert_eq!(cache.revision, 8);
        assert_eq!(cache.state, None);
        assert!(cache.should_report(8, 8, current, true));
    }

    #[test]
    fn unchanged_output_does_not_report() {
        assert!(!should_report(Some(state(1)), state(1), false));
    }

    #[test]
    fn output_switch_reports_identical_values_even_during_grace_period() {
        assert!(should_report(Some(state(1)), state(2), true));
    }

    #[test]
    fn volume_and_mute_changes_report_outside_grace_period() {
        for changed in [
            OutputState {
                volume: 60,
                ..state(1)
            },
            OutputState {
                muted: true,
                ..state(1)
            },
        ] {
            assert!(should_report(Some(state(1)), changed, false));
            assert!(!should_report(Some(state(1)), changed, true));
        }
    }

    #[test]
    fn recovery_reports_fresh_state_even_during_grace_period() {
        assert!(should_report(None, state(1), true));
    }
}
