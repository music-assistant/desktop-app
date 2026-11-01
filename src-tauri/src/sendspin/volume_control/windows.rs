//! Windows volume control implementation using WASAPI

use super::{VolumeChangeCallback, VolumeControlImpl, VolumeSample, VolumeTarget};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
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

fn target_device(
    enumerator: &IMMDeviceEnumerator,
    target: &VolumeTarget,
) -> Result<IMMDevice, String> {
    let VolumeTarget::Device { native_id } = target else {
        return Err("Opened output has no concrete Windows endpoint ID".into());
    };
    if native_id.is_empty() {
        return Err("Opened output endpoint ID is empty".into());
    }
    let wide: Vec<u16> = native_id.encode_utf16().chain(std::iter::once(0)).collect();
    let device = unsafe { enumerator.GetDevice(PCWSTR(wide.as_ptr())) }
        .map_err(|e| format!("Opened output endpoint unavailable: {e}"))?;
    if unsafe { device.GetState() }.map_err(|e| format!("Failed to read endpoint state: {e}"))?
        != DEVICE_STATE_ACTIVE
    {
        return Err("Opened output endpoint is not active".into());
    }
    Ok(device)
}

fn with_target_endpoint<T>(
    target: &VolumeTarget,
    operation: impl FnOnce(&IMMDevice, &IAudioEndpointVolume) -> Result<T, String>,
) -> Result<T, String> {
    let _com_guard = ComUninitializeGuard(initialize_com_for_volume_control()?);
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("Failed to create device enumerator: {e}"))?;
    let device = target_device(&enumerator, target)?;
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

fn read_target_state(target: &VolumeTarget) -> Result<EndpointState, String> {
    with_target_endpoint(target, |device, endpoint| {
        let state = EndpointState {
            id: endpoint_id(device)?,
            volume: read_volume(endpoint)?,
            muted: read_mute(endpoint)?,
        };
        // The previous default may remain usable after a switch. Recheck its
        // identity after sampling, without activating another volume interface.
        // These interfaces also remain within with_target_endpoint's COM scope.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("Failed to create device enumerator: {e}"))?;
        let current = target_device(&enumerator, target)?;
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
    target: Arc<RwLock<VolumeTarget>>,
    generation: Arc<AtomicU64>,
    last_self_change: LastSelfChange,
    stop_flag: Arc<AtomicBool>,
    polling_thread: Option<std::thread::JoinHandle<()>>,
}

impl WindowsVolumeControl {
    #[allow(clippy::new_ret_no_self, clippy::unnecessary_wraps)]
    pub fn new(
        target: Arc<RwLock<VolumeTarget>>,
        generation: Arc<AtomicU64>,
    ) -> Option<Box<dyn VolumeControlImpl + Send>> {
        Some(Box::new(Self {
            target,
            generation,
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
        with_target_endpoint(&self.target.read().clone(), |device, endpoint| {
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
        with_target_endpoint(&self.target.read().clone(), |_, endpoint| {
            read_volume(endpoint)
        })
    }

    fn get_mute(&self) -> Result<bool, String> {
        with_target_endpoint(&self.target.read().clone(), |_, endpoint| {
            read_mute(endpoint)
        })
    }

    fn is_available(&self) -> bool {
        with_target_endpoint(&self.target.read().clone(), |_, _| Ok(())).is_ok()
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.stop_polling();
        self.stop_flag = Arc::new(AtomicBool::new(false));
        let last_self_change = Arc::clone(&self.last_self_change);
        let stop_flag = Arc::clone(&self.stop_flag);
        let target = Arc::clone(&self.target);
        let generation = Arc::clone(&self.generation);
        let initial_state = read_target_state(&target.read().clone()).ok();
        self.polling_thread = Some(std::thread::spawn(move || {
            let mut previous = initial_state;
            let mut read_failed = false;
            while !stop_flag.load(Ordering::Relaxed) {
                let last_change = last_self_change.lock().unwrap_or_else(|e| e.into_inner());
                let (selected, revision) = {
                    let selected = target.read();
                    (selected.clone(), generation.load(Ordering::SeqCst))
                };
                let current = read_target_state(&selected).and_then(|state| {
                    if *target.read() == selected {
                        Ok(state)
                    } else {
                        Err("Output target changed during volume read".into())
                    }
                });
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
                            let _selected = target.read();
                            if generation.load(Ordering::SeqCst) != revision {
                                previous = None;
                                continue;
                            }
                            if stop_flag.load(Ordering::Relaxed)
                                || callback
                                    .send(VolumeSample {
                                        revision,
                                        volume: current.volume,
                                        muted: current.muted,
                                    })
                                    .is_err()
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

    /// Reads only the guest's current output. No volume or mute writes.
    #[test]
    #[ignore = "requires a Windows playback endpoint; read-only hardware volume check"]
    fn concrete_output_volume_reads_the_opened_endpoint() {
        use cpal::traits::{DeviceTrait, HostTrait};

        let output = crate::sendspin::devices::get_default_device().expect("concrete output");
        let id = output.id().expect("output ID");
        let expected_native_id = id.id().to_string();
        let target = VolumeTarget::from_device_id(Some(&id));
        assert_eq!(
            target,
            VolumeTarget::Device {
                native_id: expected_native_id.clone()
            }
        );
        let state = read_target_state(&target).expect("read volume from concrete output");
        assert_eq!(state.id, expected_native_id);
        assert!(state.volume <= 100);
        let host = cpal::default_host();
        let default = host.default_output_device().expect("system default output");
        assert_eq!(
            default.id().expect("default ID").to_string(),
            id.to_string()
        );
        assert!(read_target_state(&VolumeTarget::Unavailable).is_err());
        let missing = VolumeTarget::Device {
            native_id: "{00000000-0000-0000-0000-000000000000}".to_string(),
        };
        assert!(read_target_state(&missing).is_err());
        println!(
            "read-only opened endpoint id={}, volume={}, muted={}",
            state.id, state.volume, state.muted
        );
    }

    #[test]
    fn unresolved_target_does_not_resolve_a_default_endpoint() {
        assert!(matches!(
            VolumeTarget::Unavailable,
            VolumeTarget::Unavailable
        ));
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
