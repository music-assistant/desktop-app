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
use std::sync::Arc;

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
}

// ============================================================================
// Windows Implementation (WASAPI)
// ============================================================================

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::VolumeControlImpl;
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eRender, ERole, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
        IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    pub struct WindowsVolumeControl {
        volume_interface: Option<ISimpleAudioVolume>,
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
            let com_initialized = com_result.is_ok()
                || com_result
                    == Err(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::S_FALSE,
                    ));

            if !com_initialized {
                return Err("Failed to initialize COM".to_string());
            }

            // Get the default audio endpoint
            let device_enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                    .map_err(|e| format!("Failed to create device enumerator: {}", e))?;

            let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, ERole(0)) }
                .map_err(|e| format!("Failed to get default audio endpoint: {}", e))?;

            // Get the session manager
            let session_manager: IAudioSessionManager2 =
                unsafe { device.Activate(CLSCTX_ALL, None) }
                    .map_err(|e| format!("Failed to activate session manager: {}", e))?;

            // Get current process ID
            let current_pid = std::process::id();

            // Enumerate audio sessions to find our process
            let session_enumerator = unsafe { session_manager.GetSessionEnumerator() }
                .map_err(|e| format!("Failed to get session enumerator: {}", e))?;

            let session_count = unsafe { session_enumerator.GetCount() }
                .map_err(|e| format!("Failed to get session count: {}", e))?;

            // Try to find our process's audio session
            for i in 0..session_count {
                if let Ok(session_control) = unsafe { session_enumerator.GetSession(i) } {
                    // Try to get session control 2 for process ID
                    if let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>() {
                        if let Ok(session_pid) = unsafe { session_control2.GetProcessId() } {
                            if session_pid == current_pid {
                                // Found our session! Get the volume interface
                                if let Ok(volume) = session_control.cast::<ISimpleAudioVolume>() {
                                    eprintln!("[VolumeControl] Found audio session for current process (PID: {})", current_pid);
                                    return Ok(Self {
                                        volume_interface: Some(volume),
                                        com_initialized,
                                    });
                                }
                            }
                        }
                    }
                }
            }

            // If we didn't find an existing session, we'll create volume control on-demand
            // when audio actually starts playing. For now, return without a session.
            eprintln!("[VolumeControl] No active audio session found yet (will be available when playback starts)");
            Ok(Self {
                volume_interface: None,
                com_initialized,
            })
        }

        fn ensure_session(&mut self) -> Result<&ISimpleAudioVolume, String> {
            // If we already have a session, return it
            if let Some(ref volume) = self.volume_interface {
                return Ok(volume);
            }

            // Try to find our session again (it may have been created since initialization)
            let device_enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                    .map_err(|e| format!("Failed to create device enumerator: {}", e))?;

            let device = unsafe { device_enumerator.GetDefaultAudioEndpoint(eRender, ERole(0)) }
                .map_err(|e| format!("Failed to get default audio endpoint: {}", e))?;

            let session_manager: IAudioSessionManager2 =
                unsafe { device.Activate(CLSCTX_ALL, None) }
                    .map_err(|e| format!("Failed to activate session manager: {}", e))?;

            let current_pid = std::process::id();
            let session_enumerator = unsafe { session_manager.GetSessionEnumerator() }
                .map_err(|e| format!("Failed to get session enumerator: {}", e))?;

            let session_count = unsafe { session_enumerator.GetCount() }
                .map_err(|e| format!("Failed to get session count: {}", e))?;

            for i in 0..session_count {
                if let Ok(session_control) = unsafe { session_enumerator.GetSession(i) } {
                    if let Ok(session_control2) = session_control.cast::<IAudioSessionControl2>() {
                        if let Ok(session_pid) = unsafe { session_control2.GetProcessId() } {
                            if session_pid == current_pid {
                                if let Ok(volume) = session_control.cast::<ISimpleAudioVolume>() {
                                    self.volume_interface = Some(volume);
                                    return Ok(self.volume_interface.as_ref().unwrap());
                                }
                            }
                        }
                    }
                }
            }

            Err(
                "Audio session not found - volume control will be available when playback starts"
                    .to_string(),
            )
        }
    }

    impl VolumeControlImpl for WindowsVolumeControl {
        fn set_volume(&mut self, volume: u8) -> Result<(), String> {
            // Try to ensure session, retry up to 3 times with small delays
            let mut last_error = String::new();
            for attempt in 0..3 {
                match self.ensure_session() {
                    Ok(volume_interface) => {
                        let volume_scalar = (volume as f32) / 100.0;

                        unsafe {
                            volume_interface.SetMasterVolume(volume_scalar, std::ptr::null())
                        }
                        .map_err(|e| format!("Failed to set volume: {}", e))?;

                        if attempt > 0 {
                            eprintln!(
                                "[VolumeControl] Successfully set volume after {} retries",
                                attempt
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        last_error = e;
                        if attempt < 2 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            }

            Err(format!(
                "Failed to set volume after retries: {}",
                last_error
            ))
        }

        fn set_mute(&mut self, muted: bool) -> Result<(), String> {
            // Try to ensure session, retry up to 3 times with small delays
            let mut last_error = String::new();
            for attempt in 0..3 {
                match self.ensure_session() {
                    Ok(volume_interface) => {
                        unsafe { volume_interface.SetMute(muted, std::ptr::null()) }
                            .map_err(|e| format!("Failed to set mute: {}", e))?;

                        if attempt > 0 {
                            eprintln!(
                                "[VolumeControl] Successfully set mute after {} retries",
                                attempt
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        last_error = e;
                        if attempt < 2 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            }

            Err(format!("Failed to set mute after retries: {}", last_error))
        }

        fn get_volume(&self) -> Result<u8, String> {
            if let Some(ref volume_interface) = self.volume_interface {
                let volume_scalar = unsafe { volume_interface.GetMasterVolume() }
                    .map_err(|e| format!("Failed to get volume: {}", e))?;

                Ok((volume_scalar * 100.0) as u8)
            } else {
                Err("Audio session not available".to_string())
            }
        }

        fn get_mute(&self) -> Result<bool, String> {
            if let Some(ref volume_interface) = self.volume_interface {
                let muted = unsafe { volume_interface.GetMute() }
                    .map_err(|e| format!("Failed to get mute state: {}", e))?;

                Ok(muted.as_bool())
            } else {
                Err("Audio session not available".to_string())
            }
        }

        fn is_available(&self) -> bool {
            // Volume control is available as long as COM is initialized
            // The session will be found when playback starts
            self.com_initialized
        }
    }

    impl Drop for WindowsVolumeControl {
        fn drop(&mut self) {
            self.volume_interface = None;
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
    use super::VolumeControlImpl;
    use coreaudio_sys::*;
    use std::mem;
    use std::ptr;

    pub struct MacOSVolumeControl {
        device_id: AudioDeviceID,
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

            Ok(Self { device_id })
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
    }
}

// ============================================================================
// Linux Implementation (PulseAudio)
// ============================================================================

#[cfg(target_os = "linux")]
mod linux_impl {
    use super::VolumeControlImpl;
    use libpulse_binding as pulse;
    use libpulse_binding::context::{Context, FlagSet as ContextFlagSet};
    use libpulse_binding::mainloop::threaded::Mainloop;
    use libpulse_binding::proplist::Proplist;
    use libpulse_binding::volume::{ChannelVolumes, Volume};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    pub struct LinuxVolumeControl {
        mainloop: Arc<Mutex<Mainloop>>,
        context: Arc<Mutex<Rc<RefCell<Context>>>>,
        sink_input_idx: Arc<Mutex<Option<u32>>>,
        current_volume: Arc<Mutex<u8>>,
        current_muted: Arc<Mutex<bool>>,
    }

    impl LinuxVolumeControl {
        #[allow(clippy::new_ret_no_self)]
        pub fn new() -> Option<Box<dyn VolumeControlImpl + Send>> {
            match Self::initialize() {
                Ok(control) => {
                    eprintln!(
                        "[VolumeControl] Linux PulseAudio volume control initialized successfully"
                    );
                    Some(Box::new(control))
                }
                Err(e) => {
                    eprintln!(
                        "[VolumeControl] Failed to initialize Linux volume control: {}",
                        e
                    );
                    None
                }
            }
        }

        fn initialize() -> Result<Self, String> {
            // Create mainloop
            let mut mainloop = Mainloop::new()
                .ok_or_else(|| "Failed to create PulseAudio mainloop".to_string())?;

            // Create context
            let mut proplist =
                Proplist::new().ok_or_else(|| "Failed to create proplist".to_string())?;
            proplist
                .set_str(
                    pulse::proplist::properties::APPLICATION_NAME,
                    "Music Assistant",
                )
                .map_err(|_| "Failed to set application name".to_string())?;

            let context = Rc::new(RefCell::new(
                Context::new_with_proplist(mainloop.inner(), "MusicAssistantContext", &proplist)
                    .ok_or_else(|| "Failed to create PulseAudio context".to_string())?,
            ));

            // Connect to PulseAudio server
            context
                .borrow_mut()
                .connect(None, ContextFlagSet::NOFLAGS, None)
                .map_err(|e| format!("Failed to connect to PulseAudio: {:?}", e))?;

            // Start mainloop
            mainloop.lock();
            mainloop
                .start()
                .map_err(|_| "Failed to start mainloop".to_string())?;

            // Wait for context to be ready
            loop {
                match context.borrow().get_state() {
                    pulse::context::State::Ready => break,
                    pulse::context::State::Failed | pulse::context::State::Terminated => {
                        mainloop.unlock();
                        mainloop.stop();
                        return Err("PulseAudio context failed".to_string());
                    }
                    _ => mainloop.wait(),
                }
            }
            mainloop.unlock();

            Ok(Self {
                mainloop: Arc::new(Mutex::new(mainloop)),
                context: Arc::new(Mutex::new(context)),
                sink_input_idx: Arc::new(Mutex::new(None)),
                current_volume: Arc::new(Mutex::new(100)),
                current_muted: Arc::new(Mutex::new(false)),
            })
        }

        fn find_sink_input(&self) -> Result<u32, String> {
            // Check if we already have the sink input index
            if let Some(idx) = *self.sink_input_idx.lock().unwrap() {
                return Ok(idx);
            }

            // Get our process ID to find our sink input
            let current_pid = std::process::id();

            // This is a simplified implementation
            // In a full implementation, we'd enumerate sink inputs and find ours by PID
            // For now, return an error - volume control will work once we find the sink input
            Err(format!("Sink input not found for PID {} (volume control will be available when audio is playing)", current_pid))
        }

        fn set_sink_input_volume(&self, sink_idx: u32, volume: u8) -> Result<(), String> {
            let context_guard = self.context.lock().unwrap();
            let context = context_guard.borrow();
            let mainloop_guard = self.mainloop.lock().unwrap();

            // Convert percentage to PulseAudio volume
            let pa_volume = Volume((Volume::NORMAL.0 as f32 * (volume as f32 / 100.0)) as u32);
            let mut volumes = ChannelVolumes::default();
            volumes.set(2, pa_volume); // Assume stereo

            let mut introspector = context.introspect();
            introspector.set_sink_input_volume(sink_idx, &volumes, None);

            mainloop_guard.unlock();

            Ok(())
        }
    }

    impl VolumeControlImpl for LinuxVolumeControl {
        fn set_volume(&mut self, volume: u8) -> Result<(), String> {
            // Try to find sink input, retry up to 3 times
            let mut last_error = String::new();
            for attempt in 0..3 {
                match self.find_sink_input() {
                    Ok(sink_idx) => {
                        self.set_sink_input_volume(sink_idx, volume)?;
                        *self.current_volume.lock().unwrap() = volume;

                        if attempt > 0 {
                            eprintln!(
                                "[VolumeControl] Successfully set volume after {} retries",
                                attempt
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        last_error = e;
                        if attempt < 2 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            }

            Err(format!(
                "Failed to set volume after retries: {}",
                last_error
            ))
        }

        fn set_mute(&mut self, muted: bool) -> Result<(), String> {
            // Try to find sink input, retry up to 3 times
            let mut last_error = String::new();
            for attempt in 0..3 {
                match self.find_sink_input() {
                    Ok(sink_idx) => {
                        let context_guard = self.context.lock().unwrap();
                        let context = context_guard.borrow();
                        let mainloop_guard = self.mainloop.lock().unwrap();

                        let mut introspector = context.introspect();
                        introspector.set_sink_input_mute(sink_idx, muted, None);

                        mainloop_guard.unlock();

                        *self.current_muted.lock().unwrap() = muted;

                        if attempt > 0 {
                            eprintln!(
                                "[VolumeControl] Successfully set mute after {} retries",
                                attempt
                            );
                        }
                        return Ok(());
                    }
                    Err(e) => {
                        last_error = e;
                        if attempt < 2 {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                }
            }

            Err(format!("Failed to set mute after retries: {}", last_error))
        }

        fn get_volume(&self) -> Result<u8, String> {
            Ok(*self.current_volume.lock().unwrap())
        }

        fn get_mute(&self) -> Result<bool, String> {
            Ok(*self.current_muted.lock().unwrap())
        }

        fn is_available(&self) -> bool {
            // PulseAudio is initialized, volume control will be available when audio starts
            true
        }
    }

    impl Drop for LinuxVolumeControl {
        fn drop(&mut self) {
            let mut mainloop = self.mainloop.lock().unwrap();
            mainloop.stop();
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
