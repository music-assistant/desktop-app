//! Hardware volume control for audio playback
//!
//! This module provides platform-specific hardware/system volume control.
//! If hardware volume control is not available, volume capability is not advertised.
//!
//! - Windows: Controls endpoint volume via WASAPI
//! - macOS: Controls output device volume via `CoreAudio`
//! - Linux: Controls sink volume via `PulseAudio`
//!
//! Note: Platform-specific volume control requires unsafe code to interface with
//! system APIs. This module explicitly allows unsafe code for this purpose.

#![allow(unsafe_code)]

use parking_lot::Mutex;
use std::sync::mpsc;
use std::sync::Arc;

// Platform-specific implementations
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

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

/// Create a platform-specific volume controller
fn create_platform_controller() -> Option<Box<dyn VolumeControlImpl + Send>> {
    #[cfg(target_os = "windows")]
    return windows::WindowsVolumeControl::new();

    #[cfg(target_os = "macos")]
    return macos::MacOSVolumeControl::new();

    #[cfg(target_os = "linux")]
    return linux::LinuxVolumeControl::new();

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        eprintln!("[VolumeControl] Platform not supported - volume control not available");
        None
    }
}
