//! Windows volume control implementation using WASAPI

use super::{VolumeChangeCallback, VolumeControlImpl};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::GUID;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eRender, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PropVariantToStringAlloc};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const SELF_CHANGE_GRACE_PERIOD: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComInitialization {
    /// `S_OK`: this thread now owns a COM initialization count.
    Initialized,
    /// `S_FALSE`: still a successful call and still needs balancing.
    AlreadyInitialized,
    /// `RPC_E_CHANGED_MODE`: use the existing apartment; do not uninitialize.
    ExistingDifferentApartment,
}

fn initialize_com_for_volume_control() -> Result<ComInitialization, String> {
    let com_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if com_result == S_OK {
        Ok(ComInitialization::Initialized)
    } else if com_result == S_FALSE {
        Ok(ComInitialization::AlreadyInitialized)
    } else if com_result == RPC_E_CHANGED_MODE {
        Ok(ComInitialization::ExistingDifferentApartment)
    } else {
        Err(format!("Failed to initialize COM: {:?}", com_result))
    }
}

fn should_uninitialize_com(initialization: ComInitialization) -> bool {
    matches!(
        initialization,
        ComInitialization::Initialized | ComInitialization::AlreadyInitialized
    )
}

struct ComUninitializeGuard(ComInitialization);

impl Drop for ComUninitializeGuard {
    fn drop(&mut self) {
        if should_uninitialize_com(self.0) {
            unsafe { CoUninitialize() };
        }
    }
}

// CPAL's Windows description.name uses Device_FriendlyName, then DeviceDesc.
// Both properties share this format ID (PIDs 14 and 2 respectively).
fn endpoint_name(device: &IMMDevice) -> Option<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    for pid in [14, 2] {
        let key = PROPERTYKEY {
            fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
            pid,
        };
        if let Ok(mut value) = unsafe { store.GetValue(&key) } {
            if unsafe { value.Anonymous.Anonymous.vt } != windows::Win32::System::Variant::VT_LPWSTR
            {
                let _ = unsafe { PropVariantClear(&mut value) };
                continue;
            }
            let text = unsafe { PropVariantToStringAlloc(&value) };
            let _ = unsafe { PropVariantClear(&mut value) };
            if let Ok(text) = text {
                let name = unsafe { text.to_string() }.ok();
                unsafe { CoTaskMemFree(Some(text.0.cast())) };
                if name.is_some() {
                    return name;
                }
            }
        }
    }
    None
}

fn matching_output(configured: Option<&str>, names: &[Option<String>]) -> Option<usize> {
    let configured = configured?;
    names
        .iter()
        .position(|name| name.as_deref() == Some(configured))
}

fn target_device(enumerator: &IMMDeviceEnumerator) -> Result<IMMDevice, String> {
    if let Some(name) = crate::settings::get_settings().audio_device_id {
        let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
            .map_err(|e| format!("Failed to enumerate output endpoints: {e}"))?;
        let count = unsafe { devices.GetCount() }
            .map_err(|e| format!("Failed to count output endpoints: {e}"))?;
        let mut outputs = Vec::new();
        let mut names = Vec::new();
        for index in 0..count {
            let device = unsafe { devices.Item(index) }
                .map_err(|e| format!("Failed to read output endpoint: {e}"))?;
            names.push(endpoint_name(&device));
            outputs.push(device);
        }
        // Playback also chooses the first exact name match. Name-based settings
        // cannot distinguish duplicate names; do not guess from model strings.
        if let Some(index) = matching_output(Some(&name), &names) {
            return Ok(outputs.remove(index));
        }
    }
    // Match playback's fallback when the configured output is absent.
    unsafe { enumerator.GetDefaultAudioEndpoint(eRender, ERole(0)) }
        .map_err(|e| format!("Failed to get default audio endpoint: {e}"))
}

// COM interfaces remain on the calling thread and are released before its COM
// initialization is balanced. Never retain an endpoint across operations: the
// selected or default output can change while the previous device remains connected.
fn with_default_endpoint<T>(
    operation: impl FnOnce(&IMMDevice, &IAudioEndpointVolume) -> Result<T, String>,
) -> Result<T, String> {
    let _com_guard = ComUninitializeGuard(initialize_com_for_volume_control()?);
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("Failed to create device enumerator: {e}"))?;
    let device = target_device(&enumerator)?;
    let endpoint: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| format!("Failed to activate endpoint volume: {e}"))?;
    operation(&device, &endpoint)
}

fn endpoint_id(device: &IMMDevice) -> Result<String, String> {
    let id =
        unsafe { device.GetId() }.map_err(|e| format!("Failed to get audio endpoint ID: {e}"))?;
    let result = unsafe { id.to_string() }.map_err(|e| format!("Invalid audio endpoint ID: {e}"));
    unsafe { CoTaskMemFree(Some(id.0.cast())) };
    result
}

#[derive(Debug, PartialEq, Eq)]
struct EndpointState {
    id: String,
    volume: u8,
    muted: bool,
}

fn read_default_state() -> Result<EndpointState, String> {
    with_default_endpoint(|device, endpoint| {
        let state = EndpointState {
            id: endpoint_id(device)?,
            volume: read_volume(endpoint)?,
            muted: read_mute(endpoint)?,
        };
        // The previous default may remain usable after a switch. Recheck its
        // identity after sampling, without activating another volume interface.
        // These interfaces also remain within with_default_endpoint's COM scope.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("Failed to create device enumerator: {e}"))?;
        let current = target_device(&enumerator)?;
        validate_snapshot_endpoint(&state.id, &endpoint_id(&current)?)?;
        Ok(state)
    })
}

fn validate_snapshot_endpoint(snapshot_id: &str, current_id: &str) -> Result<(), String> {
    if snapshot_id == current_id {
        Ok(())
    } else {
        Err("Audio output target changed while reading volume; retrying".into())
    }
}

fn read_volume(endpoint: &IAudioEndpointVolume) -> Result<u8, String> {
    unsafe { endpoint.GetMasterVolumeLevelScalar() }
        .map(|scalar| (scalar * 100.0) as u8)
        .map_err(|e| format!("Failed to get volume: {e}"))
}

fn read_mute(endpoint: &IAudioEndpointVolume) -> Result<bool, String> {
    unsafe { endpoint.GetMute() }
        .map(|muted| muted.as_bool())
        .map_err(|e| format!("Failed to get mute state: {e}"))
}

fn should_publish(
    previous: Option<&EndpointState>,
    current: &EndpointState,
    recent_self_change: bool,
) -> bool {
    match previous {
        // A new default must be reported even if its levels match the old one,
        // or the old endpoint was changed by us very recently.
        Some(previous) if previous.id == current.id => previous != current && !recent_self_change,
        _ => true,
    }
}

type LastSelfChange = Arc<Mutex<Option<(String, Instant)>>>;

pub struct WindowsVolumeControl {
    last_self_change: LastSelfChange,
    stop_flag: Arc<AtomicBool>,
    polling_thread: Option<std::thread::JoinHandle<()>>,
}

impl WindowsVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        if let Err(e) = with_default_endpoint(|_, _| Ok(())) {
            log::error!("[VolumeControl] Failed to initialize Windows volume control: {e}");
            return None;
        }
        Some(Box::new(Self {
            last_self_change: Arc::new(Mutex::new(None)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            polling_thread: None,
        }))
    }

    fn write_default(
        &self,
        operation: impl FnOnce(&IAudioEndpointVolume) -> Result<(), String>,
    ) -> Result<(), String> {
        // Serialize writes and polling snapshots so a poll cannot observe a
        // successful write before its grace period has been recorded.
        let mut last_change = self
            .last_self_change
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_default_endpoint(|device, endpoint| {
            let id = endpoint_id(device)?;
            operation(endpoint)?;
            *last_change = Some((id, Instant::now()));
            Ok(())
        })
    }

    fn stop_polling(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.polling_thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl VolumeControlImpl for WindowsVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        self.write_default(|endpoint| {
            unsafe {
                endpoint.SetMasterVolumeLevelScalar(f32::from(volume) / 100.0, std::ptr::null())
            }
            .map_err(|e| format!("Failed to set volume: {e}"))
        })
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        self.write_default(|endpoint| {
            unsafe { endpoint.SetMute(muted, std::ptr::null()) }
                .map_err(|e| format!("Failed to set mute: {e}"))
        })
    }

    fn get_volume(&self) -> Result<u8, String> {
        with_default_endpoint(|_, endpoint| read_volume(endpoint))
    }

    fn get_mute(&self) -> Result<bool, String> {
        with_default_endpoint(|_, endpoint| read_mute(endpoint))
    }

    fn is_available(&self) -> bool {
        with_default_endpoint(|_, _| Ok(())).is_ok()
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.stop_polling();
        self.stop_flag = Arc::new(AtomicBool::new(false));
        let last_self_change = Arc::clone(&self.last_self_change);
        let stop_flag = Arc::clone(&self.stop_flag);
        // Capture both values and the identity from a single resolved endpoint.
        let initial_state = read_default_state().ok();
        self.polling_thread = Some(std::thread::spawn(move || {
            let mut previous = initial_state;
            let mut read_failed = false;
            while !stop_flag.load(Ordering::Relaxed) {
                let last_change = last_self_change.lock().unwrap_or_else(|e| e.into_inner());
                let current = read_default_state();
                let recent_self_change = match (&current, last_change.as_ref()) {
                    (Ok(current), Some((id, when))) => {
                        current.id == *id && when.elapsed() < SELF_CHANGE_GRACE_PERIOD
                    }
                    _ => false,
                };
                drop(last_change);
                match current {
                    Ok(current) => {
                        read_failed = false;
                        if should_publish(previous.as_ref(), &current, recent_self_change) {
                            if stop_flag.load(Ordering::Relaxed)
                                || callback.send((current.volume, current.muted)).is_err()
                            {
                                break;
                            }
                            previous = Some(current);
                        }
                    }
                    Err(e) => {
                        if !read_failed {
                            log::debug!(
                                "[VolumeControl] Cannot read default endpoint; retrying: {e}"
                            );
                        }
                        read_failed = true;
                        // Retry every tick and publish a fresh snapshot on recovery,
                        // even if the same device and levels return.
                        previous = None;
                    }
                }
                std::thread::park_timeout(POLL_INTERVAL);
            }
        }));
        log::info!("[VolumeControl] Windows volume polling enabled (250ms interval)");
        Ok(())
    }
}

impl Drop for WindowsVolumeControl {
    fn drop(&mut self) {
        self.stop_polling();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_output_requires_exact_name() {
        let names = vec![
            None,
            Some("Speakers (USB Audio)".into()),
            Some("Headphones".into()),
        ];
        assert_eq!(matching_output(Some("Headphones"), &names), Some(2));
        assert_eq!(matching_output(Some("USB Audio"), &names), None);
        assert_eq!(matching_output(Some("headphones"), &names), None);
    }

    #[test]
    fn missing_or_unconfigured_output_uses_default_fallback() {
        let names = vec![Some("Speakers".into())];
        assert_eq!(matching_output(None, &names), None);
        assert_eq!(matching_output(Some("Disconnected"), &names), None);
        assert_eq!(matching_output(Some("Speakers"), &[]), None);
    }

    #[test]
    fn duplicate_names_choose_first_like_playback() {
        let names = vec![None, Some("Speakers".into()), Some("Speakers".into())];
        assert_eq!(matching_output(Some("Speakers"), &names), Some(1));
    }

    fn state(id: &str, volume: u8, muted: bool) -> EndpointState {
        EndpointState {
            id: id.into(),
            volume,
            muted,
        }
    }

    #[test]
    fn snapshot_requires_the_endpoint_to_still_be_default() {
        assert!(validate_snapshot_endpoint("speaker", "speaker").is_ok());
        assert!(validate_snapshot_endpoint("speaker", "headphones").is_err());
    }

    #[test]
    fn switching_endpoints_bypasses_grace_even_with_identical_levels() {
        assert!(should_publish(
            Some(&state("old", 50, false)),
            &state("new", 50, false),
            true
        ));
    }

    #[test]
    fn same_endpoint_changes_wait_for_grace_to_expire() {
        let old = state("speaker", 50, false);
        let new = state("speaker", 25, true);
        assert!(!should_publish(Some(&old), &new, true));
        assert!(should_publish(Some(&old), &new, false));
        assert!(!should_publish(Some(&new), &new, false));
    }

    #[test]
    fn recovery_publishes_current_state() {
        assert!(should_publish(None, &state("speaker", 50, false), true));
    }

    #[test]
    fn only_successful_com_initializations_are_balanced() {
        assert!(should_uninitialize_com(ComInitialization::Initialized));
        assert!(should_uninitialize_com(
            ComInitialization::AlreadyInitialized
        ));
        assert!(!should_uninitialize_com(
            ComInitialization::ExistingDifferentApartment
        ));
    }
}
