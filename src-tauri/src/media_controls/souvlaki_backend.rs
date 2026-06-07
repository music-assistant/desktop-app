//! souvlaki-backed media controls (Windows + Linux).
//!
//! Provides:
//! - Windows: System Media Transport Controls
//! - Linux: MPRIS D-Bus interface
//!
//! Retained behind `cfg(not(target_os = "macos"))` until the remaining
//! per-platform native backends land. macOS uses the native objc2 backend.

use super::MediaControlCallback;
use crate::now_playing::NowPlaying;
use parking_lot::Mutex;
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use std::time::Duration;

static MEDIA_CONTROLS: Mutex<Option<MediaControls>> = Mutex::new(None);
static EVENT_CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);

#[allow(unused_variables)]
pub fn init(callback: MediaControlCallback, hwnd_param: Option<*mut std::ffi::c_void>) {
    {
        let mut cb = EVENT_CALLBACK.lock();
        *cb = Some(callback);
    }

    #[cfg(target_os = "windows")]
    let hwnd = {
        // SMTC requires a valid HWND to anchor the controls.
        if hwnd_param.is_none() {
            log::error!("[MediaControls] Disabled on Windows (no HWND available)");
            return;
        }
        hwnd_param
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
            if let Err(e) = controls.attach(handle_media_event) {
                log::error!("[MediaControls] Failed to attach event handler: {:?}", e);
                return;
            }

            let mut mc = MEDIA_CONTROLS.lock();
            *mc = Some(controls);
        }
        Err(e) => {
            log::error!("[MediaControls] Failed to initialize: {:?}", e);
        }
    }
}

fn handle_media_event(event: MediaControlEvent) {
    let command = match event {
        MediaControlEvent::Play => "play",
        MediaControlEvent::Pause => "pause",
        MediaControlEvent::Toggle => "toggle",
        MediaControlEvent::Next => "next",
        MediaControlEvent::Previous => "previous",
        MediaControlEvent::Stop => "stop",
        _ => return,
    };

    if let Some(ref callback) = *EVENT_CALLBACK.lock() {
        callback(command);
    }
}

pub fn update(np: &NowPlaying) {
    let mut controls = MEDIA_CONTROLS.lock();
    let Some(ref mut controls) = *controls else {
        return;
    };

    if np.track.is_some() || np.artist.is_some() {
        let metadata = MediaMetadata {
            title: np.track.as_deref(),
            artist: np.artist.as_deref(),
            album: np.album.as_deref(),
            cover_url: np.image_url.as_deref(),
            duration: np.duration.map(Duration::from_secs_f64),
        };

        if let Err(e) = controls.set_metadata(metadata) {
            log::error!("[MediaControls] Failed to set metadata: {:?}", e);
        }
    }

    // Report elapsed position so the OS scrubber can extrapolate between our
    // ~1/sec updates.
    let progress = np
        .elapsed
        .map(|secs| MediaPosition(Duration::from_secs_f64(secs)));
    let playback = if np.is_playing {
        MediaPlayback::Playing { progress }
    } else if np.track.is_some() {
        MediaPlayback::Paused { progress }
    } else {
        MediaPlayback::Stopped
    };

    if let Err(e) = controls.set_playback(playback) {
        log::error!("[MediaControls] Failed to set playback state: {:?}", e);
    }
}

#[allow(dead_code)]
pub fn clear() {
    let mut controls = MEDIA_CONTROLS.lock();
    if let Some(ref mut controls) = *controls {
        let _ = controls.set_playback(MediaPlayback::Stopped);
    }
}
