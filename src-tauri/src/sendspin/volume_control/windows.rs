//! Windows volume control implementation using WASAPI

use super::{VolumeChangeCallback, VolumeControlImpl};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{S_FALSE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

// Wrapper to make IAudioEndpointVolume Send
// SAFETY: COM objects are thread-safe when used with COINIT_MULTITHREADED
struct SendableEndpointVolume(IAudioEndpointVolume);
unsafe impl Send for SendableEndpointVolume {}

pub struct WindowsVolumeControl {
    endpoint_volume: Option<SendableEndpointVolume>,
    com_initialized: bool,
    // Timestamp of last self-initiated volume change (to prevent feedback loops)
    last_self_change: Arc<AtomicU64>,
    // Handle to the polling thread (kept alive for duration of controller)
    #[allow(clippy::used_underscore_binding)]
    _polling_thread: Option<std::thread::JoinHandle<()>>,
}

impl WindowsVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        match Self::initialize() {
            Ok(control) => {
                eprintln!("[VolumeControl] Windows WASAPI volume control initialized successfully");
                Some(Box::new(control))
            }
            Err(e) => {
                eprintln!(
                    "[VolumeControl] Failed to initialize Windows volume control: {}",
                    e
                );
                None
            }
        }
    }

    fn initialize() -> Result<Self, String> {
        // Initialize COM
        let com_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        // S_OK or S_FALSE (already initialized) are both acceptable
        let com_initialized = com_result == S_OK || com_result == S_FALSE;

        if !com_initialized {
            return Err(format!("Failed to initialize COM: {:?}", com_result));
        }

        // Get the default audio endpoint
        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("Failed to create device enumerator: {}", e))?;

        let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, ERole(0)) }
            .map_err(|e| format!("Failed to get default audio endpoint: {}", e))?;

        // Get the endpoint volume interface
        let endpoint_volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| format!("Failed to activate endpoint volume: {}", e))?;

        eprintln!("[VolumeControl] Windows endpoint volume control initialized successfully");

        Ok(Self {
            endpoint_volume: Some(SendableEndpointVolume(endpoint_volume)),
            com_initialized,
            last_self_change: Arc::new(AtomicU64::new(0)),
            _polling_thread: None,
        })
    }
}

impl VolumeControlImpl for WindowsVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        // Record timestamp to prevent feedback loop
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_self_change.store(now, Ordering::Relaxed);

        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        let volume_scalar = (volume as f32) / 100.0;

        unsafe {
            endpoint_volume
                .0
                .SetMasterVolumeLevelScalar(volume_scalar, std::ptr::null())
        }
        .map_err(|e| format!("Failed to set volume: {}", e))?;

        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        // Record timestamp to prevent feedback loop
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_self_change.store(now, Ordering::Relaxed);

        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        unsafe { endpoint_volume.0.SetMute(muted, std::ptr::null()) }
            .map_err(|e| format!("Failed to set mute: {}", e))?;

        Ok(())
    }

    fn get_volume(&self) -> Result<u8, String> {
        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        let volume_scalar = unsafe { endpoint_volume.0.GetMasterVolumeLevelScalar() }
            .map_err(|e| format!("Failed to get volume: {}", e))?;

        Ok((volume_scalar * 100.0) as u8)
    }

    fn get_mute(&self) -> Result<bool, String> {
        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        let muted = unsafe { endpoint_volume.0.GetMute() }
            .map_err(|e| format!("Failed to get mute state: {}", e))?;

        Ok(muted.as_bool())
    }

    fn is_available(&self) -> bool {
        self.endpoint_volume.is_some() && self.com_initialized
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        // Use polling instead of COM callbacks for consistency across platforms
        let endpoint_volume = SendableEndpointVolume(
            self.endpoint_volume
                .as_ref()
                .ok_or("Endpoint volume not available")?
                .0
                .clone(),
        );
        let last_self_change = Arc::clone(&self.last_self_change);

        let polling_thread = std::thread::spawn(move || {
            use std::time::Duration;

            const POLL_INTERVAL: Duration = Duration::from_secs(2);
            const SELF_CHANGE_GRACE_PERIOD: u64 = 1000; // milliseconds

            let mut last_values: Option<(u8, bool)> = None;

            loop {
                std::thread::sleep(POLL_INTERVAL);

                // Check if this was recently self-initiated
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let last_self_ms = last_self_change.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_self_ms) < SELF_CHANGE_GRACE_PERIOD {
                    // Skip - recently set by us
                    continue;
                }

                // Read current volume
                let volume_result = unsafe {
                    match endpoint_volume.0.GetMasterVolumeLevelScalar() {
                        Ok(scalar) => Some((scalar * 100.0) as u8),
                        Err(_) => None,
                    }
                };

                // Read current mute state
                let mute_result = unsafe {
                    match endpoint_volume.0.GetMute() {
                        Ok(muted) => Some(muted.as_bool()),
                        Err(_) => None,
                    }
                };

                // Send notification only if values changed
                if let (Some(volume), Some(muted)) = (volume_result, mute_result) {
                    let current_values = (volume, muted);

                    if last_values != Some(current_values) {
                        if callback.send(current_values).is_ok() {
                            last_values = Some(current_values);
                        } else {
                            // Channel closed, exit thread
                            break;
                        }
                    }
                }
            }
        });

        self._polling_thread = Some(polling_thread);

        eprintln!("[VolumeControl] Windows volume polling enabled (2s interval)");
        Ok(())
    }
}

impl Drop for WindowsVolumeControl {
    fn drop(&mut self) {
        self.endpoint_volume = None;
        if self.com_initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
