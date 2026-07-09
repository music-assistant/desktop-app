//! Windows volume control implementation using WASAPI

use super::{VolumeChangeCallback, VolumeControlImpl};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::ThreadId;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

// SAFETY: `IAudioEndpointVolume` is free-threaded and internally synchronized.
// App-initiated calls are also serialized by `VolumeController`.
struct SendableEndpointVolume(IAudioEndpointVolume);
unsafe impl Send for SendableEndpointVolume {}
unsafe impl Sync for SendableEndpointVolume {}

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
        log::debug!(
            "[VolumeControl] COM already initialized with a different apartment; using existing apartment"
        );
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

struct ComUninitializeGuard {
    armed: bool,
}

impl ComUninitializeGuard {
    fn new(initialization: ComInitialization) -> Self {
        Self {
            armed: should_uninitialize_com(initialization),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ComUninitializeGuard {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

pub struct WindowsVolumeControl {
    endpoint_volume: Option<SendableEndpointVolume>,
    com_initialization: ComInitialization,
    com_thread_id: ThreadId,
    last_self_change: Arc<AtomicU64>,
    stop_flag: Arc<AtomicBool>,
    polling_thread: Option<std::thread::JoinHandle<()>>,
}

impl WindowsVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        match Self::initialize() {
            Ok(control) => {
                log::info!(
                    "[VolumeControl] Windows WASAPI volume control initialized successfully"
                );
                Some(Box::new(control))
            }
            Err(e) => {
                log::error!(
                    "[VolumeControl] Failed to initialize Windows volume control: {}",
                    e
                );
                None
            }
        }
    }

    fn initialize() -> Result<Self, String> {
        let com_initialization = initialize_com_for_volume_control()?;
        let mut com_guard = ComUninitializeGuard::new(com_initialization);
        let com_thread_id = std::thread::current().id();

        let device_enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("Failed to create device enumerator: {}", e))?;

        let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, ERole(0)) }
            .map_err(|e| format!("Failed to get default audio endpoint: {}", e))?;

        let endpoint_volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
            .map_err(|e| format!("Failed to activate endpoint volume: {}", e))?;

        log::info!("[VolumeControl] Windows endpoint volume control initialized successfully");
        com_guard.disarm();

        Ok(Self {
            endpoint_volume: Some(SendableEndpointVolume(endpoint_volume)),
            com_initialization,
            com_thread_id,
            last_self_change: Arc::new(AtomicU64::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            polling_thread: None,
        })
    }
}

impl VolumeControlImpl for WindowsVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_self_change.store(now, Ordering::Relaxed);

        let endpoint_volume = self
            .endpoint_volume
            .as_ref()
            .ok_or("Endpoint volume not available")?;

        let volume_scalar = f32::from(volume) / 100.0;

        unsafe {
            endpoint_volume
                .0
                .SetMasterVolumeLevelScalar(volume_scalar, std::ptr::null())
        }
        .map_err(|e| format!("Failed to set volume: {}", e))?;

        Ok(())
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
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
        self.endpoint_volume.is_some()
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.polling_thread.take() {
            let _ = thread.join();
        }
        self.stop_flag = Arc::new(AtomicBool::new(false));

        // Polling keeps volume-change behavior consistent across platforms.
        let endpoint_volume = Arc::new(SendableEndpointVolume(
            self.endpoint_volume
                .as_ref()
                .ok_or("Endpoint volume not available")?
                .0
                .clone(),
        ));
        let last_self_change = Arc::clone(&self.last_self_change);
        let stop_flag = Arc::clone(&self.stop_flag);

        // Read initial volume/mute so the polling thread doesn't fire a
        // spurious "changed" notification on its first tick.
        let initial_values = match (self.get_volume(), self.get_mute()) {
            (Ok(v), Ok(m)) => Some((v, m)),
            _ => None,
        };

        let polling_thread = std::thread::spawn(move || {
            use std::time::Duration;
            const POLL_INTERVAL: Duration = Duration::from_secs(2);
            const SELF_CHANGE_GRACE_PERIOD_MS: u64 = 1000;

            let com_initialization = match initialize_com_for_volume_control() {
                Ok(initialization) => initialization,
                Err(e) => {
                    log::error!("[VolumeControl] Failed to initialize COM on polling thread: {e}");
                    return;
                }
            };
            let _com_guard = ComUninitializeGuard::new(com_initialization);

            let mut last_values: Option<(u8, bool)> = initial_values;

            loop {
                std::thread::sleep(POLL_INTERVAL);

                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                let last_self_ms = last_self_change.load(Ordering::Relaxed);
                if now_ms.saturating_sub(last_self_ms) < SELF_CHANGE_GRACE_PERIOD_MS {
                    continue;
                }

                let volume_result = unsafe {
                    match endpoint_volume.0.GetMasterVolumeLevelScalar() {
                        Ok(scalar) => Some((scalar * 100.0) as u8),
                        Err(_) => None,
                    }
                };

                let mute_result = unsafe {
                    match endpoint_volume.0.GetMute() {
                        Ok(muted) => Some(muted.as_bool()),
                        Err(_) => None,
                    }
                };

                if let (Some(volume), Some(muted)) = (volume_result, mute_result) {
                    let current_values = (volume, muted);

                    if last_values != Some(current_values) {
                        if callback.send(current_values).is_ok() {
                            last_values = Some(current_values);
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        self.polling_thread = Some(polling_thread);

        log::info!("[VolumeControl] Windows volume polling enabled (2s interval)");
        Ok(())
    }
}

impl Drop for WindowsVolumeControl {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);

        if let Some(thread) = self.polling_thread.take() {
            let _ = thread.join();
        }

        self.endpoint_volume = None;

        // COM init counts are thread-local; never balance ours from a different
        // Tokio worker, and never balance `RPC_E_CHANGED_MODE`.
        if should_uninitialize_com(self.com_initialization)
            && std::thread::current().id() == self.com_thread_id
        {
            unsafe {
                CoUninitialize();
            }
        }
    }
}
