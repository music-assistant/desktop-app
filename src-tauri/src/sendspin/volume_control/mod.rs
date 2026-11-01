//! Hardware volume control for the output currently used by playback.
#![allow(unsafe_code)]

use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeSample {
    pub revision: u64,
    pub volume: u8,
    pub muted: bool,
}

pub type VolumeChangeCallback = mpsc::Sender<VolumeSample>;

/// The native output opened by playback, not its display name or saved preference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VolumeTarget {
    Unavailable,
    #[cfg(target_os = "linux")]
    SystemDefault,
    Device {
        native_id: String,
    },
}

impl VolumeTarget {
    /// Convert an opened CPAL output's stable native identifier to a volume target.
    pub fn from_device_id(id: Option<&cpal::DeviceId>) -> Self {
        let Some(id) = id else {
            return Self::Unavailable;
        };
        #[cfg(target_os = "linux")]
        let expected = cpal::HostId::Alsa;
        #[cfg(target_os = "macos")]
        let expected = cpal::HostId::CoreAudio;
        #[cfg(target_os = "windows")]
        let expected = cpal::HostId::Wasapi;
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let expected = cpal::default_host().id();
        if id.host() != expected {
            return Self::Unavailable;
        }
        let native_id = id.id().to_string();
        if native_id.is_empty() {
            return Self::Unavailable;
        }
        #[cfg(target_os = "linux")]
        if matches!(native_id.as_str(), "default" | "pulse" | "pipewire") {
            return Self::SystemDefault;
        }
        Self::Device { native_id }
    }
}

pub struct VolumeController {
    inner: Arc<Mutex<Box<dyn VolumeControlImpl + Send>>>,
    target: Arc<RwLock<VolumeTarget>>,
    #[cfg(target_os = "linux")]
    target_tx: mpsc::Sender<linux::VolumeCommand>,
    generation: Arc<AtomicU64>,
}

impl VolumeController {
    #[allow(clippy::unnecessary_wraps)] // Windows initialization and unsupported platforms can fail.
    pub fn new(target: VolumeTarget) -> Option<Self> {
        let target = Arc::new(RwLock::new(target));
        let generation = Arc::new(AtomicU64::new(0));
        #[cfg(target_os = "linux")]
        let (inner, target_tx) =
            linux::LinuxVolumeControl::new(Arc::clone(&target), Arc::clone(&generation));
        #[cfg(target_os = "windows")]
        let inner =
            windows::WindowsVolumeControl::new(Arc::clone(&target), Arc::clone(&generation))?;
        #[cfg(target_os = "macos")]
        let inner = macos::MacOSVolumeControl::new(Arc::clone(&target), Arc::clone(&generation));
        #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
        return None;
        Some(Self {
            inner: Arc::new(Mutex::new(inner)),
            target,
            #[cfg(target_os = "linux")]
            target_tx,
            generation,
        })
    }

    /// Publish the opened output without waiting for an audio or volume worker.
    pub fn set_target(&self, target: VolumeTarget) {
        {
            let mut selected = self.target.write();
            *selected = target;
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        #[cfg(target_os = "linux")]
        let _ = self.target_tx.send(linux::VolumeCommand::SetTarget);
    }

    pub fn is_current_sample(&self, sample: &VolumeSample) -> bool {
        let _target = self.target.read();
        sample.revision == self.generation.load(Ordering::SeqCst)
    }

    pub fn set_change_callback(&self, callback: VolumeChangeCallback) -> Result<(), String> {
        self.inner.lock().set_change_callback(callback)
    }
    pub fn set_volume(&self, volume: u8) -> Result<(), String> {
        self.inner.lock().set_volume(volume.min(100))
    }
    pub fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.inner.lock().set_mute(muted)
    }
    pub fn get_volume(&self) -> Result<u8, String> {
        self.inner.lock().get_volume()
    }
    pub fn get_mute(&self) -> Result<bool, String> {
        self.inner.lock().get_mute()
    }
    pub fn is_available(&self) -> bool {
        self.inner.lock().is_available()
    }
}

trait VolumeControlImpl {
    fn set_volume(&mut self, volume: u8) -> Result<(), String>;
    fn set_mute(&mut self, muted: bool) -> Result<(), String>;
    fn get_volume(&self) -> Result<u8, String>;
    fn get_mute(&self) -> Result<bool, String>;
    fn is_available(&self) -> bool;
    fn set_change_callback(&mut self, callback: VolumeChangeCallback) -> Result<(), String>;
}

#[cfg(test)]
mod tests {
    use super::VolumeTarget;
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_logical_routes_use_system_default_but_concrete_pcm_does_not() {
        for route in ["default", "pulse", "pipewire"] {
            let id = cpal::DeviceId::new(cpal::HostId::Alsa, route);
            assert_eq!(
                VolumeTarget::from_device_id(Some(&id)),
                VolumeTarget::SystemDefault
            );
        }
        let concrete = cpal::DeviceId::new(cpal::HostId::Alsa, "hw:1,0");
        assert_eq!(
            VolumeTarget::from_device_id(Some(&concrete)),
            VolumeTarget::Device {
                native_id: "hw:1,0".into()
            }
        );
    }

    #[test]
    fn unresolved_output_is_never_a_default_target() {
        assert_eq!(
            VolumeTarget::from_device_id(None),
            VolumeTarget::Unavailable
        );
        assert_ne!(
            VolumeTarget::Unavailable,
            VolumeTarget::Device {
                native_id: String::new()
            }
        );
    }
}
