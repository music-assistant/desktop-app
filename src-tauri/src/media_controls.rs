//! System media controls integration
//!
//! This module provides integration with the OS media controls:
//! - macOS: Now Playing in Control Center, media keys
//! - Windows: System Media Transport Controls
//! - Linux: MPRIS D-Bus interface
//!
//! Uses the souvlaki crate for cross-platform support.

use crate::now_playing::NowPlaying;
use parking_lot::Mutex;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
use std::sync::Arc;

/// Callback type for media control events
pub type MediaControlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Global media controls instance
static MEDIA_CONTROLS: Mutex<Option<MediaControls>> = Mutex::new(None);

/// Callback for media control events
static EVENT_CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);

/// Initialize media controls
pub fn init(callback: MediaControlCallback, _hwnd: Option<*mut std::ffi::c_void>) {
    // Store the callback
    {
        let mut cb = EVENT_CALLBACK.lock();
        *cb = Some(callback);
    }

    // Platform-specific configuration
    #[cfg(target_os = "windows")]
    let hwnd = {
        // On Windows, MediaControls requires a valid HWND
        if _hwnd.is_none() {
             log::error!("[MediaControls] Disabled on Windows (no HWND available)");
            return;
        }
        _hwnd
    };

    #[cfg(not(target_os = "windows"))]
    let hwnd = None;

    let config = PlatformConfig {
        dbus_name: "music_assistant",
        display_name: "Music Assistant",
        hwnd,
    };

    match MediaControls::new(config) {
        Ok(mut controls) => {
            // Attach event handler
            if let Err(e) = controls.attach(handle_media_event) {
                 log::error!("[MediaControls] Failed to attach event handler: {:?}", e);
                return;
            }

            // Store the controls
            let mut mc = MEDIA_CONTROLS.lock();
            *mc = Some(controls);
        }
        Err(e) => {
             log::error!("[MediaControls] Failed to initialize: {:?}", e);
        }
    }
}

/// Handle media control events from the OS
fn handle_media_event(event: MediaControlEvent) {
    let command = match event {
        MediaControlEvent::Play => "play",
        MediaControlEvent::Pause => "pause",
        MediaControlEvent::Toggle => "toggle",
        MediaControlEvent::Next => "next",
        MediaControlEvent::Previous => "previous",
        MediaControlEvent::Stop => "stop",
        _ => return, // Ignore other events like seek, volume, etc.
    };

    // Call the registered callback
    if let Some(ref callback) = *EVENT_CALLBACK.lock() {
        callback(command);
    }
}

/// Update media controls with now-playing info
pub fn update(np: &NowPlaying) {
    let mut controls = MEDIA_CONTROLS.lock();
    let Some(ref mut controls) = *controls else {
        return;
    };

    // Update playback state
    let playback = if np.is_playing {
        MediaPlayback::Playing { progress: None }
    } else if np.track.is_some() {
        MediaPlayback::Paused { progress: None }
    } else {
        MediaPlayback::Stopped
    };

    if let Err(e) = controls.set_playback(playback) {
         log::error!("[MediaControls] Failed to set playback state: {:?}", e);
    }

    // Update metadata if we have track info
    if np.track.is_some() || np.artist.is_some() {
        let metadata = MediaMetadata {
            title: np.track.as_deref(),
            artist: np.artist.as_deref(),
            album: np.album.as_deref(),
            // Cover URL - souvlaki supports URLs on some platforms
            cover_url: np.image_url.as_deref(),
            duration: np.duration.map(std::time::Duration::from_secs),
        };

        if let Err(e) = controls.set_metadata(metadata) {
             log::error!("[MediaControls] Failed to set metadata: {:?}", e);
        }
    }
}

/// Clear media controls (when stopping playback or disconnecting)
#[allow(dead_code)]
pub fn clear() {
    let mut controls = MEDIA_CONTROLS.lock();
    if let Some(ref mut controls) = *controls {
        let _ = controls.set_playback(MediaPlayback::Stopped);
    }
}
