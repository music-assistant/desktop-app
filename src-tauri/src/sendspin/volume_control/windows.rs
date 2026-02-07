//! Windows volume control implementation using WASAPI

use super::{VolumeChangeCallback, VolumeControlImpl};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::{implement, Interface};
use windows::Win32::Media::Audio::Endpoints::{IAudioEndpointVolume, IAudioEndpointVolumeCallback};
use windows::Win32::Media::Audio::{
    eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA,
};
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

        // S_FALSE means already initialized, which is okay
        use windows::Win32::Foundation::S_FALSE;
        let com_initialized = match com_result {
            Ok(()) => true,
            Err(e) => e.code() == S_FALSE,
        };

        if !com_initialized {
            return Err("Failed to initialize COM".to_string());
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
        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        // Create the event handler
        let events: IAudioEndpointVolumeCallback =
            EndpointVolumeCallback::new(callback, Arc::clone(&self.last_self_change)).into();

        // Register for endpoint volume notifications
        unsafe {
            endpoint_volume
                .0
                .RegisterControlChangeNotify(&events)
                .map_err(|e| format!("Failed to register volume notifications: {}", e))?;
        }

        eprintln!("[VolumeControl] Windows endpoint volume change listener registered");
        Ok(())
    }
}

// IAudioEndpointVolumeCallback implementation
#[implement(IAudioEndpointVolumeCallback)]
struct EndpointVolumeCallback {
    callback: Arc<Mutex<VolumeChangeCallback>>,
    last_self_change: Arc<AtomicU64>,
}

impl EndpointVolumeCallback {
    fn new(callback: VolumeChangeCallback, last_self_change: Arc<AtomicU64>) -> Self {
        Self {
            callback: Arc::new(Mutex::new(callback)),
            last_self_change,
        }
    }
}

#[allow(non_snake_case)]
impl IAudioEndpointVolumeCallback_Impl for EndpointVolumeCallback_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if pnotify.is_null() {
            return Ok(());
        }

        // Check if this change was self-initiated (within grace period)
        const SELF_CHANGE_GRACE_PERIOD: u64 = 200; // milliseconds
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let last_self_ms = self.last_self_change.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last_self_ms) < SELF_CHANGE_GRACE_PERIOD {
            // Skip notification - this was triggered by our own volume change
            return Ok(());
        }

        unsafe {
            let data = &*pnotify;
            let volume = (data.fMasterVolume * 100.0) as u8;
            let muted = data.bMuted.as_bool();

            let callback = self.callback.lock();
            let _ = callback.send((volume, muted));
        }

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
