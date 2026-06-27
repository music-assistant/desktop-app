//! System media controls integration.
//!
//! Routes to a per-platform backend selected at compile time:
//! - macOS: native objc2 backend (`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`)
//! - Linux: native zbus backend (`org.mpris.MediaPlayer2` on D-Bus)
//! - Windows: souvlaki (pending a native backend)
//!
//! Each backend exposes the same `init` / `update` / `clear` free functions;
//! the module system enforces that contract at compile time, so no runtime
//! trait object is needed (only one backend is ever compiled in).

use crate::now_playing::NowPlaying;
use std::sync::Arc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod souvlaki_backend;

/// Callback type for media control events (`"play"`, `"pause"`, `"toggle"`,
/// `"next"`, `"previous"`, `"stop"`).
pub type MediaControlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Runs a closure on the platform UI / main thread.
///
/// Required by the macOS backend: `MPRemoteCommandCenter` registration, its
/// handler blocks, and `MPNowPlayingInfoCenter` updates must occur on the
/// `NSApplication` main run loop. Other backends ignore it.
pub type MainThreadDispatch = Arc<dyn Fn(Box<dyn FnOnce() + Send + 'static>) + Send + Sync>;

/// `hwnd` is used only on Windows; `dispatch` is used only on macOS.
#[allow(unused_variables)]
pub fn init(
    callback: MediaControlCallback,
    hwnd: Option<*mut std::ffi::c_void>,
    dispatch: MainThreadDispatch,
) {
    #[cfg(target_os = "linux")]
    linux::init(callback, hwnd);
    #[cfg(target_os = "macos")]
    macos::init(callback, dispatch);
    #[cfg(target_os = "windows")]
    souvlaki_backend::init(callback, hwnd);
}

#[allow(unused_variables)]
pub fn update(np: &NowPlaying) {
    #[cfg(target_os = "linux")]
    linux::update(np);
    #[cfg(target_os = "macos")]
    macos::update(np);
    #[cfg(target_os = "windows")]
    souvlaki_backend::update(np);
}

#[allow(dead_code)]
pub fn clear() {
    #[cfg(target_os = "linux")]
    linux::clear();
    #[cfg(target_os = "macos")]
    macos::clear();
    #[cfg(target_os = "windows")]
    souvlaki_backend::clear();
}

// ---------------------------------------------------------------------------
// Platform-agnostic mapping core
//
// Pure translation from `NowPlaying` into a backend-neutral plan. Kept free of
// any FFI so it is fully unit-testable; the native backends are thin imperative
// shells that render this plan. Compiled where a consumer exists (macOS) or for
// tests on any host.
// ---------------------------------------------------------------------------

/// `MPNowPlayingInfoPropertyPlaybackRate` value while playing.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) const PLAYBACK_RATE_PLAYING: f64 = 1.0;
/// `MPNowPlayingInfoPropertyPlaybackRate` value while paused or stopped.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) const PLAYBACK_RATE_STOPPED: f64 = 0.0;

/// Coarse playback state shared by the native backends and unit tests.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlaybackState {
    Playing,
    Paused,
    #[default]
    Stopped,
}

/// Backend-neutral description of what the OS now-playing surface should show.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NowPlayingPlan {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<f64>,
    pub elapsed_secs: Option<f64>,
    pub state: PlaybackState,
    pub rate: f64,
    pub image_url: Option<String>,
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
pub(crate) fn plan(np: &NowPlaying) -> NowPlayingPlan {
    let state = if np.is_playing {
        PlaybackState::Playing
    } else if np.track.is_some() {
        PlaybackState::Paused
    } else {
        PlaybackState::Stopped
    };
    let rate = match state {
        PlaybackState::Playing => PLAYBACK_RATE_PLAYING,
        PlaybackState::Paused | PlaybackState::Stopped => PLAYBACK_RATE_STOPPED,
    };
    NowPlayingPlan {
        title: np.track.clone(),
        artist: np.artist.clone(),
        album: np.album.clone(),
        duration_secs: np.duration,
        elapsed_secs: np.elapsed,
        state,
        rate,
        image_url: np.image_url.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact comparisons against the rate constants
mod tests {
    use super::*;

    fn np(is_playing: bool, track: Option<&str>) -> NowPlaying {
        NowPlaying {
            is_playing,
            track: track.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn playing_with_track_reports_playing_rate() {
        let p = plan(&np(true, Some("Song")));
        assert_eq!(p.state, PlaybackState::Playing);
        assert_eq!(p.rate, PLAYBACK_RATE_PLAYING);
    }

    #[test]
    fn paused_when_not_playing_but_track_present() {
        let p = plan(&np(false, Some("Song")));
        assert_eq!(p.state, PlaybackState::Paused);
        assert_eq!(p.rate, PLAYBACK_RATE_STOPPED);
    }

    #[test]
    fn stopped_without_track() {
        let p = plan(&np(false, None));
        assert_eq!(p.state, PlaybackState::Stopped);
        assert_eq!(p.rate, PLAYBACK_RATE_STOPPED);
        assert!(p.title.is_none());
    }

    #[test]
    fn playing_flag_without_track_still_playing() {
        // Upstream filters this case, but the mapping must stay self-consistent.
        let p = plan(&np(true, None));
        assert_eq!(p.state, PlaybackState::Playing);
        assert_eq!(p.rate, PLAYBACK_RATE_PLAYING);
    }

    #[test]
    fn maps_metadata_and_timing_fields() {
        let mut n = np(true, Some("Song"));
        n.artist = Some("Artist".to_owned());
        n.album = Some("Album".to_owned());
        n.duration = Some(200.0);
        n.elapsed = Some(5.0);
        n.image_url = Some("http://host/cover.jpg".to_owned());

        let p = plan(&n);
        assert_eq!(p.artist.as_deref(), Some("Artist"));
        assert_eq!(p.album.as_deref(), Some("Album"));
        assert_eq!(p.duration_secs, Some(200.0));
        assert_eq!(p.elapsed_secs, Some(5.0));
        assert_eq!(p.image_url.as_deref(), Some("http://host/cover.jpg"));
    }
}
