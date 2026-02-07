//! macOS volume control implementation using `CoreAudio`

use super::{VolumeChangeCallback, VolumeControlImpl};
use coreaudio_sys::*;
use std::mem;
use std::ptr;
use std::sync::Arc;

pub struct MacOSVolumeControl {
    device_id: AudioDeviceID,
    // Channel sender kept alive for duration of controller
    #[allow(clippy::used_underscore_binding)]
    _change_signal: Option<std::sync::mpsc::Sender<()>>,
    // Handle to the worker thread (kept alive for duration of controller)
    #[allow(clippy::used_underscore_binding)]
    _worker_thread: Option<std::thread::JoinHandle<()>>,
}

impl MacOSVolumeControl {
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
        match Self::initialize() {
            Ok(control) => {
                eprintln!(
                    "[VolumeControl] macOS CoreAudio volume control initialized successfully"
                );
                Some(Box::new(control))
            }
            Err(e) => {
                eprintln!(
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
            _worker_thread: None,
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
        let volume_scalar = f32::from(volume) / 100.0;
        self.set_volume_scalar(volume_scalar)
    }

    fn set_mute(&mut self, muted: bool) -> Result<(), String> {
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
        // Property listener callback - called when volume or mute changes
        // CRITICAL: This runs on CoreAudio's real-time audio thread and must be FAST
        // Do minimal work here - just signal that a change occurred
        // This callback is LOCK-FREE - no mutexes, no allocations
        #[allow(clippy::items_after_statements)]
        unsafe extern "C" fn property_listener(
            _device_id: AudioObjectID,
            _num_addresses: u32,
            _addresses: *const AudioObjectPropertyAddress,
            client_data: *mut std::ffi::c_void,
        ) -> OSStatus {
            if client_data.is_null() {
                return 0;
            }

            // Reconstruct the Arc<Sender> from the raw pointer (but keep it alive)
            let sender_arc = Arc::from_raw(client_data as *const std::sync::mpsc::Sender<()>);

            // Send signal - this is non-blocking on unbounded channels
            // If send fails, just ignore it (channel closed, controller dropped)
            let _ = sender_arc.send(());

            // Keep the Arc alive for next callback
            mem::forget(sender_arc);

            0
        }

        // Create a channel for signaling changes from audio thread
        let (change_tx, change_rx) = std::sync::mpsc::channel::<()>();

        // Keep the sender alive for the duration of the controller
        self._change_signal = Some(change_tx.clone());

        // Spawn worker thread to handle volume reading off the audio thread
        let device_id = self.device_id;
        let worker_thread = std::thread::spawn(move || {
            use std::time::{Duration, Instant};

            // Rate limiting: minimum time between notifications
            const MIN_NOTIFICATION_INTERVAL: Duration = Duration::from_millis(50);

            let mut last_notification = Instant::now();
            let mut last_values: Option<(u8, bool)> = None;

            while let Ok(()) = change_rx.recv() {
                // Drain any pending signals to coalesce rapid-fire events
                while change_rx.try_recv().is_ok() {}

                // Rate limit: only process if enough time has passed
                if last_notification.elapsed() < MIN_NOTIFICATION_INTERVAL {
                    continue;
                }

                // Read current volume and mute state (off audio thread)
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

                // Send notification only if values changed and we successfully read both
                if let (Some(volume), Some(muted)) = (volume_result, mute_result) {
                    let current_values = (volume, muted);

                    // Only notify if values actually changed
                    if last_values != Some(current_values) && callback.send(current_values).is_ok()
                    {
                        last_values = Some(current_values);
                        last_notification = Instant::now();
                    }
                }
            }
        });

        self._worker_thread = Some(worker_thread);

        // Wrap the change sender in Arc for sharing across callbacks
        let sender_arc = Arc::new(change_tx);

        // Register listener for volume changes
        let volume_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyVolumeScalar,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };

        let client_data = Arc::into_raw(Arc::clone(&sender_arc)) as *mut std::ffi::c_void;

        unsafe {
            let status = AudioObjectAddPropertyListener(
                self.device_id,
                &volume_address,
                Some(property_listener),
                client_data,
            );

            if status != 0 {
                // Clean up the Arc we created
                let _ = Arc::from_raw(client_data as *const std::sync::mpsc::Sender<()>);
                return Err(format!(
                    "Failed to add volume property listener: {}",
                    status
                ));
            }
        }

        // Register listener for mute changes (if supported)
        let mute_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyMute,
            mScope: kAudioDevicePropertyScopeOutput,
            mElement: kAudioObjectPropertyElementMain,
        };

        if unsafe { AudioObjectHasProperty(self.device_id, &mute_address) } != 0 {
            let client_data = Arc::into_raw(sender_arc) as *mut std::ffi::c_void;

            unsafe {
                let status = AudioObjectAddPropertyListener(
                    self.device_id,
                    &mute_address,
                    Some(property_listener),
                    client_data,
                );

                if status != 0 {
                    // Clean up the Arc we created
                    let _ = Arc::from_raw(client_data as *const std::sync::mpsc::Sender<()>);
                    eprintln!(
                        "[VolumeControl] Warning: Failed to add mute property listener: {}",
                        status
                    );
                }
            }
        }

        eprintln!("[VolumeControl] macOS volume change listener registered");
        Ok(())
    }
}
