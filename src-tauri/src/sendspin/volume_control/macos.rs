//! macOS volume control implementation using `CoreAudio`

use super::{VolumeChangeCallback, VolumeControlImpl};
use coreaudio_sys::*;
use std::mem;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct MacOSVolumeControl {
    device_id: AudioDeviceID,
    // Channel sender kept alive for duration of controller
    _change_signal: Option<std::sync::mpsc::Sender<()>>,
    // Handle to the worker thread (joined on drop)
    worker_thread: Option<std::thread::JoinHandle<()>>,
    // Timestamp of last self-initiated volume change (to prevent feedback loops)
    last_self_change: Arc<AtomicU64>,
    // Flag to signal the worker thread to stop
    stop_flag: Arc<AtomicBool>,
}

impl MacOSVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        match Self::initialize() {
            Ok(control) => {
                log::error!(
                    "[VolumeControl] macOS CoreAudio volume control initialized successfully"
                );
                Some(Box::new(control))
            }
            Err(e) => {
                log::error!(
                    "[VolumeControl] Failed to initialize macOS volume control: {}",
                    e
                );
                None
            }
        }
    }

    fn initialize() -> Result<Self, String> {
        // Get the default output device
        let device_id = unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioHardwarePropertyDefaultOutputDevice,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut device_id: AudioDeviceID = 0;
            let mut size = mem::size_of::<AudioDeviceID>() as u32;

            let status = AudioObjectGetPropertyData(
                kAudioObjectSystemObject,
                &property_address,
                0,
                ptr::null(),
                &mut size,
                std::ptr::addr_of_mut!(device_id).cast(),
            );

            if status != 0 {
                return Err(format!("Failed to get default output device: {}", status));
            }

            device_id
        };

        if device_id == kAudioObjectUnknown {
            return Err("No default output device found".to_string());
        }

        // Verify the device has volume control
        let has_volume = unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            AudioObjectHasProperty(device_id, &property_address) != 0
        };

        if !has_volume {
            return Err("Default output device does not support volume control".to_string());
        }

        Ok(Self {
            device_id,
            _change_signal: None,
            worker_thread: None,
            last_self_change: Arc::new(AtomicU64::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    fn set_volume_scalar(&self, volume_scalar: f32) -> Result<(), String> {
        unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            let status = AudioObjectSetPropertyData(
                self.device_id,
                &property_address,
                0,
                ptr::null(),
                mem::size_of::<f32>() as u32,
                std::ptr::addr_of!(volume_scalar).cast(),
            );

            if status != 0 {
                return Err(format!("Failed to set volume: {}", status));
            }

            Ok(())
        }
    }

    fn get_volume_scalar(&self) -> Result<f32, String> {
        unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            let mut volume: f32 = 0.0;
            let mut size = mem::size_of::<f32>() as u32;

            let status = AudioObjectGetPropertyData(
                self.device_id,
                &property_address,
                0,
                ptr::null(),
                &mut size,
                std::ptr::addr_of_mut!(volume).cast(),
            );

            if status != 0 {
                return Err(format!("Failed to get volume: {}", status));
            }

            Ok(volume)
        }
    }
}

impl VolumeControlImpl for MacOSVolumeControl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String> {
        // Record timestamp to prevent feedback loop
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_self_change.store(now, Ordering::Relaxed);

        let volume_scalar = f32::from(volume) / 100.0;
        self.set_volume_scalar(volume_scalar)
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        // Record timestamp to prevent feedback loop
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        self.last_self_change.store(now, Ordering::Relaxed);

        unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyMute,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            // Check if device supports mute
            if AudioObjectHasProperty(self.device_id, &property_address) == 0 {
                return Err("Device does not support mute".to_string());
            }

            let mute_value: u32 = u32::from(muted);

            let status = AudioObjectSetPropertyData(
                self.device_id,
                &property_address,
                0,
                ptr::null(),
                mem::size_of::<u32>() as u32,
                std::ptr::addr_of!(mute_value).cast(),
            );

            if status != 0 {
                return Err(format!("Failed to set mute: {}", status));
            }

            Ok(())
        }
    }

    fn get_volume(&self) -> Result<u8, String> {
        let volume_scalar = self.get_volume_scalar()?;
        Ok((volume_scalar * 100.0) as u8)
    }

    fn get_mute(&self) -> Result<bool, String> {
        unsafe {
            let property_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyMute,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            // Check if device supports mute
            if AudioObjectHasProperty(self.device_id, &property_address) == 0 {
                return Ok(false); // Device doesn't support mute, treat as unmuted
            }

            let mut mute_value: u32 = 0;
            let mut size = mem::size_of::<u32>() as u32;

            let status = AudioObjectGetPropertyData(
                self.device_id,
                &property_address,
                0,
                ptr::null(),
                &mut size,
                std::ptr::addr_of_mut!(mute_value).cast(),
            );

            if status != 0 {
                return Err(format!("Failed to get mute state: {}", status));
            }

            Ok(mute_value != 0)
        }
    }

    fn is_available(&self) -> bool {
        true
    }

    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
        // Stop any existing polling thread before starting a new one
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
        self.stop_flag = Arc::new(AtomicBool::new(false));

        // Use polling instead of property listeners to avoid interfering with audio playback
        // CoreAudio property listeners were causing static noise during playback
        let device_id = self.device_id;
        let last_self_change = Arc::clone(&self.last_self_change);
        let stop_flag = Arc::clone(&self.stop_flag);

        let polling_thread = std::thread::spawn(move || {
            use std::time::Duration;

            const POLL_INTERVAL: Duration = Duration::from_secs(2);
            const SELF_CHANGE_GRACE_PERIOD: u64 = 1000; // milliseconds

            let mut last_values: Option<(u8, bool)> = None;

            loop {
                std::thread::sleep(POLL_INTERVAL);

                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }

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
                    let property_address = AudioObjectPropertyAddress {
                        mSelector: kAudioDevicePropertyVolumeScalar,
                        mScope: kAudioDevicePropertyScopeOutput,
                        mElement: kAudioObjectPropertyElementMain,
                    };

                    let mut volume: f32 = 0.0;
                    let mut size = mem::size_of::<f32>() as u32;

                    let status = AudioObjectGetPropertyData(
                        device_id,
                        &property_address,
                        0,
                        ptr::null(),
                        &mut size,
                        std::ptr::addr_of_mut!(volume).cast(),
                    );

                    if status == 0 {
                        Some((volume * 100.0) as u8)
                    } else {
                        None
                    }
                };

                // Read current mute state
                let mute_result = unsafe {
                    let property_address = AudioObjectPropertyAddress {
                        mSelector: kAudioDevicePropertyMute,
                        mScope: kAudioDevicePropertyScopeOutput,
                        mElement: kAudioObjectPropertyElementMain,
                    };

                    if AudioObjectHasProperty(device_id, &property_address) != 0 {
                        let mut mute_value: u32 = 0;
                        let mut size = mem::size_of::<u32>() as u32;

                        let status = AudioObjectGetPropertyData(
                            device_id,
                            &property_address,
                            0,
                            ptr::null(),
                            &mut size,
                            std::ptr::addr_of_mut!(mute_value).cast(),
                        );

                        if status == 0 {
                            Some(mute_value != 0)
                        } else {
                            None
                        }
                    } else {
                        Some(false)
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

        self.worker_thread = Some(polling_thread);

        log::error!("[VolumeControl] macOS volume polling enabled (2s interval)");
        Ok(())
    }
}

impl Drop for MacOSVolumeControl {
    fn drop(&mut self) {
        // Signal the worker thread to stop
        self.stop_flag.store(true, Ordering::Relaxed);

        // Join the worker thread
        if let Some(thread) = self.worker_thread.take() {
            let _ = thread.join();
        }
    }
}
