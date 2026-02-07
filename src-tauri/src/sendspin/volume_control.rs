//! Hardware volume control for audio playback
//!
//! This module provides platform-specific hardware/system volume control.
//! If hardware volume control is not available, volume capability is not advertised.
//!
//! - Windows: Controls application audio session volume via WASAPI
//! - macOS: Controls output device volume via `CoreAudio`
//! - Linux: Controls sink volume via `PulseAudio`
//!
//! Note: Platform-specific volume control requires unsafe code to interface with
//! system APIs. This module explicitly allows unsafe code for this purpose.

#![allow(unsafe_code)]

use parking_lot::Mutex;
use std::sync::mpsc;
use std::sync::Arc;

/// Type for volume change notifications: (volume: u8, muted: bool)
pub type VolumeChangeCallback = mpsc::Sender<(u8, bool)>;

/// Hardware volume controller
pub struct VolumeController {
    inner: Arc<Mutex<Box<dyn VolumeControlImpl + Send>>>,
}

impl VolumeController {
    /// Create a new volume controller
    /// Returns None if hardware volume control is not available on this platform
    pub fn new() -> Option<Self> {
        let inner = create_platform_controller()?;
        Some(Self {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    /// Set up a callback to be notified when the OS volume changes
    /// The callback will receive (volume: u8, muted: bool) when changes are detected
    pub fn set_change_callback(&self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.inner.lock().set_change_callback(callback)
    }

    /// Set volume level (0-100)
    pub fn set_volume(&self, volume: u8) -> Result<(), String> {
        let volume = volume.min(100);
        self.inner.lock().set_volume(volume)
    }

    /// Set mute state
    pub fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.inner.lock().set_mute(muted)
    }

    /// Get current volume level (0-100)
    pub fn get_volume(&self) -> Result<u8, String> {
        self.inner.lock().get_volume()
    }

    /// Get current mute state
    pub fn get_mute(&self) -> Result<bool, String> {
        self.inner.lock().get_mute()
    }

    /// Check if hardware volume control is available
    pub fn is_available(&self) -> bool {
        self.inner.lock().is_available()
    }
}

/// Trait for platform-specific volume control implementations
trait VolumeControlImpl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String>;
    fn set_mute(&mut self, muted: bool) -> Result<(), String>;
    fn get_volume(&self) -> Result<u8, String>;
    fn get_mute(&self) -> Result<bool, String>;
    fn is_available(&self) -> bool;
    /// Set up a callback to be notified when the OS volume changes
    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String>;
}

// ============================================================================
// Windows Implementation (WASAPI)
// ============================================================================

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::{VolumeChangeCallback, VolumeControlImpl};
    use parking_lot::Mutex;
    use std::sync::Arc;
    use windows::core::{implement, Interface, GUID};
    use windows::Win32::Media::Audio::Endpoints::{
        IAudioEndpointVolume, IAudioEndpointVolumeCallback,
    };
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
    }

    impl WindowsVolumeControl {
        #[allow(clippy::new_ret_no_self)]
        pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
            match Self::initialize() {
                Ok(control) => {
                    eprintln!(
                        "[VolumeControl] Windows WASAPI volume control initialized successfully"
                    );
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
            let endpoint_volume: IAudioEndpointVolume =
                unsafe { device.Activate(CLSCTX_ALL, None) }
                    .map_err(|e| format!("Failed to activate endpoint volume: {}", e))?;

            eprintln!("[VolumeControl] Windows endpoint volume control initialized successfully");

            Ok(Self {
                endpoint_volume: Some(SendableEndpointVolume(endpoint_volume)),
                com_initialized,
            })
        }
    }

    impl VolumeControlImpl for WindowsVolumeControl {
        fn set_volume(&mut self, volume: u8) -> Result<(), String> {
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
            let events: IAudioEndpointVolumeCallback = EndpointVolumeCallback::new(callback).into();

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
    }

    impl EndpointVolumeCallback {
        fn new(callback: VolumeChangeCallback) -> Self {
            Self {
                callback: Arc::new(Mutex::new(callback)),
            }
        }
    }

    #[allow(non_snake_case)]
    impl IAudioEndpointVolumeCallback_Impl for EndpointVolumeCallback_Impl {
        fn OnNotify(
            &self,
            pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA,
        ) -> windows::core::Result<()> {
            if pnotify.is_null() {
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
}

// ============================================================================
// macOS Implementation (CoreAudio)
// ============================================================================

#[cfg(target_os = "macos")]
mod macos_impl {
    use super::{VolumeChangeCallback, VolumeControlImpl};
    use coreaudio_sys::*;
    use parking_lot::Mutex;
    use std::mem;
    use std::ptr;
    use std::sync::Arc;

    // Data passed to the property listener callback
    struct ListenerData {
        // Channel to signal that a change occurred, without blocking audio thread
        change_signal: std::sync::mpsc::Sender<()>,
    }

    pub struct MacOSVolumeControl {
        device_id: AudioDeviceID,
        listener_data: Option<Arc<Mutex<ListenerData>>>,
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
                listener_data: None,
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

                // Reconstruct the Arc from the raw pointer (but keep it alive)
                let data_arc = Arc::from_raw(client_data as *const Mutex<ListenerData>);

                // Just send a signal - don't do any heavy work on audio thread
                {
                    let data = data_arc.lock();
                    let _ = data.change_signal.send(());
                }

                // Keep the Arc alive for next callback
                mem::forget(data_arc);

                0
            }

            // Create a channel for signaling changes from audio thread
            let (change_tx, change_rx) = std::sync::mpsc::channel::<()>();

            // Create listener data
            let listener_data = Arc::new(Mutex::new(ListenerData {
                change_signal: change_tx,
            }));

            self.listener_data = Some(Arc::clone(&listener_data));

            // Spawn worker thread to handle volume reading off the audio thread
            let device_id = self.device_id;
            let worker_thread = std::thread::spawn(move || {
                while let Ok(()) = change_rx.recv() {
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

                    // Send notification if we successfully read both values
                    if let (Some(volume), Some(muted)) = (volume_result, mute_result) {
                        let _ = callback.send((volume, muted));
                    }
                }
            });

            self._worker_thread = Some(worker_thread);

            // Register listener for volume changes
            let volume_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyVolumeScalar,
                mScope: kAudioDevicePropertyScopeOutput,
                mElement: kAudioObjectPropertyElementMain,
            };

            let client_data = Arc::into_raw(Arc::clone(&listener_data)) as *mut std::ffi::c_void;

            unsafe {
                let status = AudioObjectAddPropertyListener(
                    self.device_id,
                    &volume_address,
                    Some(property_listener),
                    client_data,
                );

                if status != 0 {
                    // Clean up the Arc we created
                    let _ = Arc::from_raw(client_data as *const Mutex<ListenerData>);
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
                let client_data = Arc::into_raw(listener_data) as *mut std::ffi::c_void;

                unsafe {
                    let status = AudioObjectAddPropertyListener(
                        self.device_id,
                        &mute_address,
                        Some(property_listener),
                        client_data,
                    );

                    if status != 0 {
                        // Clean up the Arc we created
                        let _ = Arc::from_raw(client_data as *const Mutex<ListenerData>);
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
}

// ============================================================================
// Linux Implementation (PulseAudio)
// ============================================================================

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::{VolumeChangeCallback, VolumeControlImpl};
    use libpulse_binding::{
        callbacks::ListResult,
        context::{
            subscribe::{Facility, InterestMaskSet, Operation},
            Context, FlagSet as ContextFlagSet,
        },
        mainloop::threaded::Mainloop,
        proplist::Proplist,
        volume::Volume,
    };
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    enum VolumeCommand {
        SetVolume(u8, Sender<Result<(), String>>),
        SetMute(bool, Sender<Result<(), String>>),
        GetVolume(Sender<Result<u8, String>>),
        GetMute(Sender<Result<bool, String>>),
        IsAvailable(Sender<bool>),
        SetChangeCallback(VolumeChangeCallback, Sender<Result<(), String>>),
        Shutdown,
    }

    pub struct LinuxVolumeControl {
        command_tx: Sender<VolumeCommand>,
    }

    impl LinuxVolumeControl {
        #[allow(clippy::new_ret_no_self)]
        #[allow(clippy::unnecessary_wraps)]
        pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
            let control = Self::initialize();
            eprintln!("[VolumeControl] Linux PulseAudio volume control initialized successfully");
            Some(Box::new(control))
        }

        fn initialize() -> Self {
            let (command_tx, command_rx) = channel::<VolumeCommand>();

            // Spawn a background thread to handle PulseAudio operations
            // This is necessary because PulseAudio types (Mainloop, Context) are not Send
            thread::spawn(move || {
                // Create mainloop
                let Some(mut mainloop) = Mainloop::new() else {
                    eprintln!("[VolumeControl] Failed to create PulseAudio mainloop");
                    return;
                };

                // Create context
                let mut proplist = Proplist::new().unwrap();
                proplist
                    .set_str(
                        libpulse_binding::proplist::properties::APPLICATION_NAME,
                        "Music Assistant",
                    )
                    .unwrap();

                let Some(mut context) =
                    Context::new_with_proplist(&mainloop, "MusicAssistantContext", &proplist)
                else {
                    eprintln!("[VolumeControl] Failed to create PulseAudio context");
                    return;
                };

                // Connect to PulseAudio server
                if context
                    .connect(None, ContextFlagSet::NOFLAGS, None)
                    .is_err()
                {
                    eprintln!("[VolumeControl] Failed to connect to PulseAudio server");
                    return;
                }

                // Start mainloop
                if mainloop.start().is_err() {
                    eprintln!("[VolumeControl] Failed to start PulseAudio mainloop");
                    return;
                }

                // Wait for context to be ready
                loop {
                    match context.get_state() {
                        libpulse_binding::context::State::Ready => break,
                        libpulse_binding::context::State::Failed
                        | libpulse_binding::context::State::Terminated => {
                            eprintln!("[VolumeControl] PulseAudio context failed");
                            return;
                        }
                        _ => thread::sleep(Duration::from_millis(10)),
                    }
                }

                eprintln!("[VolumeControl] PulseAudio context ready");

                // Store the default sink index (output device)
                let sink_idx = Arc::new(Mutex::new(None::<u32>));

                // Get default sink immediately
                let sink_idx_clone = sink_idx.clone();
                let (init_tx, init_rx) = channel();
                let init_tx = Arc::new(Mutex::new(Some(init_tx)));

                let introspect = context.introspect();
                let introspect_clone = context.introspect();
                introspect.get_server_info(move |server_info| {
                    if let Some(default_sink_name) = &server_info.default_sink_name {
                        eprintln!("[VolumeControl] Default sink: {:?}", default_sink_name);
                        // Look up the sink by name to get its index
                        let sink_name = default_sink_name.clone();
                        let sink_idx_clone2 = sink_idx_clone.clone();
                        let init_tx_clone = init_tx.clone();
                        introspect_clone.get_sink_info_by_name(&sink_name, move |list_result| {
                            if let libpulse_binding::callbacks::ListResult::Item(sink_info) =
                                list_result
                            {
                                *sink_idx_clone2.lock().unwrap() = Some(sink_info.index);
                                if let Some(tx) = init_tx_clone.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                            }
                        });
                    }
                });

                // Wait for initial sink to be found
                let _ = init_rx.recv_timeout(Duration::from_secs(1));

                // Store change callback (if set)
                let change_callback: Arc<Mutex<Option<VolumeChangeCallback>>> =
                    Arc::new(Mutex::new(None));

                // Process commands
                while let Ok(command) = command_rx.recv() {
                    match command {
                        VolumeCommand::SetVolume(volume, response_tx) => {
                            let result = Self::handle_set_volume(&context, &sink_idx, volume);
                            let _ = response_tx.send(result);
                        }
                        VolumeCommand::SetMute(muted, response_tx) => {
                            let result = Self::handle_set_mute(&context, &sink_idx, muted);
                            let _ = response_tx.send(result);
                        }
                        VolumeCommand::GetVolume(response_tx) => {
                            let result = Self::handle_get_volume(&context, &sink_idx);
                            let _ = response_tx.send(result);
                        }
                        VolumeCommand::GetMute(response_tx) => {
                            let result = Self::handle_get_mute(&context, &sink_idx);
                            let _ = response_tx.send(result);
                        }
                        VolumeCommand::IsAvailable(response_tx) => {
                            let available =
                                context.get_state() == libpulse_binding::context::State::Ready;
                            let _ = response_tx.send(available);
                        }
                        VolumeCommand::SetChangeCallback(callback, response_tx) => {
                            let result = Self::handle_set_change_callback(
                                &mut context,
                                &sink_idx,
                                &change_callback,
                                callback,
                            );
                            let _ = response_tx.send(result);
                        }
                        VolumeCommand::Shutdown => {
                            break;
                        }
                    }
                }

                // Cleanup
                mainloop.stop();
                context.disconnect();
            });

            Self { command_tx }
        }

        fn handle_set_volume(
            context: &Context,
            sink_idx: &Arc<Mutex<Option<u32>>>,
            volume: u8,
        ) -> Result<(), String> {
            use libpulse_binding::volume::ChannelVolumes;

            let idx = *sink_idx.lock().unwrap();
            if idx.is_none() {
                return Err("Sink not found".to_string());
            }

            let idx = idx.unwrap();

            let (result_tx, result_rx) = channel::<Result<ChannelVolumes, String>>();
            let result_tx = Arc::new(Mutex::new(Some(result_tx)));

            // Get current sink info to determine channel count
            let result_tx_clone = result_tx.clone();
            let introspect = context.introspect();
            introspect.get_sink_info_by_index(idx, move |result| {
                if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                    let mut new_volume = info.volume;
                    let volume_norm = Volume(Volume::NORMAL.0 * u32::from(volume) / 100);
                    new_volume.set(new_volume.len(), volume_norm);

                    if let Some(tx) = result_tx_clone.lock().unwrap().take() {
                        let _ = tx.send(Ok(new_volume));
                    }
                }
            });

            let new_volume = result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout getting sink info".to_string())??;

            // Set the sink volume
            let (set_result_tx, set_result_rx) = channel();
            let set_result_tx = Arc::new(Mutex::new(Some(set_result_tx)));

            let mut introspect = context.introspect();
            introspect.set_sink_volume_by_index(
                idx,
                &new_volume,
                Some(Box::new(move |success| {
                    if let Some(tx) = set_result_tx.lock().unwrap().take() {
                        let _ = tx.send(success);
                    }
                })),
            );

            let success = set_result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout setting volume".to_string())?;

            if success {
                Ok(())
            } else {
                Err("Failed to set volume".to_string())
            }
        }

        fn handle_set_mute(
            context: &Context,
            sink_idx: &Arc<Mutex<Option<u32>>>,
            muted: bool,
        ) -> Result<(), String> {
            let idx = *sink_idx.lock().unwrap();
            if idx.is_none() {
                return Err("Sink not found".to_string());
            }

            let idx = idx.unwrap();

            // Set the sink mute state
            let (result_tx, result_rx) = channel();
            let result_tx = Arc::new(Mutex::new(Some(result_tx)));

            let mut introspect = context.introspect();
            introspect.set_sink_mute_by_index(
                idx,
                muted,
                Some(Box::new(move |success| {
                    if let Some(tx) = result_tx.lock().unwrap().take() {
                        let _ = tx.send(success);
                    }
                })),
            );

            let success = result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout setting mute".to_string())?;

            if success {
                Ok(())
            } else {
                Err("Failed to set mute".to_string())
            }
        }

        fn handle_get_volume(
            context: &Context,
            sink_idx: &Arc<Mutex<Option<u32>>>,
        ) -> Result<u8, String> {
            let idx = *sink_idx.lock().unwrap();
            if idx.is_none() {
                return Err("Sink not found".to_string());
            }

            let idx = idx.unwrap();

            // Get the sink volume
            let (result_tx, result_rx) = channel();
            let result_tx = Arc::new(Mutex::new(Some(result_tx)));

            let introspect = context.introspect();
            introspect.get_sink_info_by_index(idx, move |result| {
                if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                    let avg_volume = info.volume.avg();
                    let volume_percent = (avg_volume.0 * 100 / Volume::NORMAL.0) as u8;
                    if let Some(tx) = result_tx.lock().unwrap().take() {
                        let _ = tx.send(volume_percent);
                    }
                }
            });

            result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout getting volume".to_string())
        }

        fn handle_get_mute(
            context: &Context,
            sink_idx: &Arc<Mutex<Option<u32>>>,
        ) -> Result<bool, String> {
            let idx = *sink_idx.lock().unwrap();
            if idx.is_none() {
                return Err("Sink not found".to_string());
            }

            let idx = idx.unwrap();

            // Get the sink mute state
            let (result_tx, result_rx) = channel();
            let result_tx = Arc::new(Mutex::new(Some(result_tx)));

            let introspect = context.introspect();
            introspect.get_sink_info_by_index(idx, move |result| {
                if let libpulse_binding::callbacks::ListResult::Item(info) = result {
                    if let Some(tx) = result_tx.lock().unwrap().take() {
                        let _ = tx.send(info.mute);
                    }
                }
            });

            result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout getting mute state".to_string())
        }

        fn handle_set_change_callback(
            context: &mut Context,
            sink_idx: &Arc<Mutex<Option<u32>>>,
            change_callback: &Arc<Mutex<Option<VolumeChangeCallback>>>,
            callback: VolumeChangeCallback,
        ) -> Result<(), String> {
            // Store the callback
            *change_callback.lock().unwrap() = Some(callback);

            let idx = *sink_idx.lock().unwrap();
            if idx.is_none() {
                return Err("Sink not found".to_string());
            }

            // Subscribe to sink events
            let interest = InterestMaskSet::SINK;
            let (result_tx, result_rx) = channel();
            let result_tx = Arc::new(Mutex::new(Some(result_tx)));

            context.subscribe(interest, move |success| {
                if let Some(tx) = result_tx.lock().unwrap().take() {
                    let _ = tx.send(success);
                }
            });

            let success = result_rx
                .recv_timeout(Duration::from_secs(1))
                .map_err(|_| "Timeout subscribing to events".to_string())?;

            if !success {
                return Err("Failed to subscribe to sink events".to_string());
            }

            // Set up subscription callback
            let sink_idx_clone = sink_idx.clone();
            let change_callback_clone = change_callback.clone();
            let introspect = context.introspect();

            context.set_subscribe_callback(Some(Box::new(move |facility, operation, idx| {
                // Only handle sink changes
                if facility != Some(Facility::Sink) {
                    return;
                }

                // Check if this is our sink
                let our_idx = *sink_idx_clone.lock().unwrap();
                if our_idx != Some(idx) {
                    return;
                }

                // Only handle change operations
                if operation != Some(Operation::Changed) {
                    return;
                }

                // Query the sink to get updated volume/mute
                let callback_clone = change_callback_clone.clone();
                introspect.get_sink_info_by_index(idx, move |result| {
                    if let ListResult::Item(info) = result {
                        let avg_volume = info.volume.avg();
                        let volume_percent = (avg_volume.0 * 100 / Volume::NORMAL.0) as u8;
                        let muted = info.mute;

                        if let Some(ref cb) = *callback_clone.lock().unwrap() {
                            let _ = cb.send((volume_percent, muted));
                        }
                    }
                });
            })));

            eprintln!("[VolumeControl] Linux PulseAudio sink volume change listener registered");
            Ok(())
        }
    }

    impl VolumeControlImpl for LinuxVolumeControl {
        fn set_volume(&mut self, volume: u8) -> Result<(), String> {
            let (response_tx, response_rx) = channel();
            self.command_tx
                .send(VolumeCommand::SetVolume(volume, response_tx))
                .map_err(|_| "Failed to send command".to_string())?;
            response_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Timeout waiting for response".to_string())?
        }

        fn set_mute(&mut self, muted: bool) -> Result<(), String> {
            let (response_tx, response_rx) = channel();
            self.command_tx
                .send(VolumeCommand::SetMute(muted, response_tx))
                .map_err(|_| "Failed to send command".to_string())?;
            response_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Timeout waiting for response".to_string())?
        }

        fn get_volume(&self) -> Result<u8, String> {
            let (response_tx, response_rx) = channel();
            self.command_tx
                .send(VolumeCommand::GetVolume(response_tx))
                .map_err(|_| "Failed to send command".to_string())?;
            response_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Timeout waiting for response".to_string())?
        }

        fn get_mute(&self) -> Result<bool, String> {
            let (response_tx, response_rx) = channel();
            self.command_tx
                .send(VolumeCommand::GetMute(response_tx))
                .map_err(|_| "Failed to send command".to_string())?;
            response_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Timeout waiting for response".to_string())?
        }

        fn is_available(&self) -> bool {
            let (response_tx, response_rx) = channel();
            if self
                .command_tx
                .send(VolumeCommand::IsAvailable(response_tx))
                .is_err()
            {
                return false;
            }
            response_rx
                .recv_timeout(Duration::from_millis(500))
                .unwrap_or(false)
        }

        fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String> {
            let (response_tx, response_rx) = channel();
            self.command_tx
                .send(VolumeCommand::SetChangeCallback(callback, response_tx))
                .map_err(|_| "Failed to send command".to_string())?;
            response_rx
                .recv_timeout(Duration::from_secs(2))
                .map_err(|_| "Timeout waiting for response".to_string())?
        }
    }

    impl Drop for LinuxVolumeControl {
        fn drop(&mut self) {
            let _ = self.command_tx.send(VolumeCommand::Shutdown);
        }
    }
}

// ============================================================================
// Platform Selection
// ============================================================================

fn create_platform_controller() -> Option<Box<dyn VolumeControlImpl + Send>> {
    #[cfg(target_os = "windows")]
    return windows_impl::WindowsVolumeControl::new();

    #[cfg(target_os = "macos")]
    return macos_impl::MacOSVolumeControl::new();

    #[cfg(target_os = "linux")]
    return linux_impl::LinuxVolumeControl::new();

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        eprintln!("[VolumeControl] Platform not supported - volume control not available");
        None
    }
}
