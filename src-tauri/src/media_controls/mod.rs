//! System media controls integration.
//!
//! Routes to a per-platform backend selected at compile time:
//! - macOS: native objc2 backend (`MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`)
//! - Linux: native zbus backend (`org.mpris.MediaPlayer2` on D-Bus)
//! - Windows: native `windows` crate backend (System Media Transport Controls)
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
mod windows;

/// Callback type for media control events (`"play"`, `"pause"`, `"toggle"`,
/// `"next"`, `"previous"`, `"stop"`).
pub type MediaControlCallback = Arc<dyn Fn(&str) + Send + Sync>;

/// Runs a closure on the platform UI / main thread.
///
/// Required by native backends with UI-thread-bound framework work: macOS uses
/// it for `MPRemoteCommandCenter`/`MPNowPlayingInfoCenter`, and Windows uses it
/// to keep System Media Transport Controls calls on the Tauri window thread.
pub type MainThreadDispatch = Arc<dyn Fn(Box<dyn FnOnce() + Send + 'static>) + Send + Sync>;

/// `hwnd` is used only on Windows; `dispatch` is used by macOS and Windows.
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
    windows::init(callback, hwnd, dispatch);
}

#[allow(unused_variables)]
pub fn update(np: &NowPlaying) {
    #[cfg(target_os = "linux")]
    linux::update(np);
    #[cfg(target_os = "macos")]
    macos::update(np);
    #[cfg(target_os = "windows")]
    windows::update(np);
}

/// Re-bind the OS media controls to a new native window handle.
///
/// Only meaningful on Windows, where SMTC is attached to an HWND whose
/// lifetime we don't control: logout/server-switch destroys the window the
/// controls were bound to and creates a replacement, which would otherwise
/// silently kill media keys and the SMTC flyout for the rest of the process.
#[cfg_attr(not(target_os = "windows"), allow(dead_code, unused_variables))]
pub fn rebind(hwnd: Option<*mut std::ffi::c_void>) {
    #[cfg(target_os = "windows")]
    windows::rebind(hwnd);
}

#[allow(dead_code)]
pub fn clear() {
    #[cfg(target_os = "linux")]
    linux::clear();
    #[cfg(target_os = "macos")]
    macos::clear();
    #[cfg(target_os = "windows")]
    windows::clear();
}

// ---------------------------------------------------------------------------
// Platform-agnostic mapping core
//
// Pure translation from `NowPlaying` into a backend-neutral plan. Kept free of
// any FFI so it is fully unit-testable; the native backends are thin imperative
// shells that render this plan.
// ---------------------------------------------------------------------------

/// `MPNowPlayingInfoPropertyPlaybackRate` value while playing.
pub(crate) const PLAYBACK_RATE_PLAYING: f64 = 1.0;
/// `MPNowPlayingInfoPropertyPlaybackRate` value while paused or stopped.
pub(crate) const PLAYBACK_RATE_STOPPED: f64 = 0.0;

/// Coarse playback state shared by the native backends and unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PlaybackState {
    Playing,
    Paused,
    #[default]
    Stopped,
}

/// Backend-neutral description of what the OS now-playing surface should show.
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
    /// Whether the current item supports play, independent of playback state.
    pub can_play: bool,
    /// Whether the current item supports pause, independent of playback state.
    pub can_pause: bool,
    /// Whether skipping to the next queue item is available.
    pub can_next: bool,
    /// Whether skipping to the previous queue item is available.
    pub can_previous: bool,
    /// Whether the current item is a live/unknown-length stream (a track
    /// present but no usable duration).
    pub live: bool,
}

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

    // OS media surfaces treat play/pause availability as intrinsic to the
    // current item (the MPRIS spec forbids deriving it from playback state),
    // but the upstream flags flip with the transport (can_play only while
    // paused, can_pause only while playing). Fold them back into one
    // state-independent capability.
    let has_track = np.track.is_some();
    let controllable = has_track && (np.can_play || np.can_pause || np.is_playing);

    // A non-positive or non-finite duration means "no known length" (live or
    // unknown stream), not a zero-length track.
    let duration_secs = np.duration.filter(|d| d.is_finite() && *d > 0.0);

    NowPlayingPlan {
        title: np.track.clone(),
        artist: np.artist.clone(),
        album: np.album.clone(),
        duration_secs,
        elapsed_secs: np.elapsed,
        state,
        rate,
        image_url: np.image_url.clone(),
        can_play: controllable,
        can_pause: controllable,
        // Queue navigation only applies while an item exists; mask stray
        // upstream flags so no surface offers next/previous on an empty state.
        can_next: has_track && np.can_next,
        can_previous: has_track && np.can_previous,
        live: has_track && duration_secs.is_none(),
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
    fn can_play_pause_are_state_independent() {
        // Playing with only can_pause set upstream: play must stay available.
        let mut playing = np(true, Some("Song"));
        playing.can_pause = true;
        let p = plan(&playing);
        assert!(p.can_play);
        assert!(p.can_pause);

        // Paused with only can_play set upstream: pause must stay available.
        let mut paused = np(false, Some("Song"));
        paused.can_play = true;
        let p = plan(&paused);
        assert!(p.can_play);
        assert!(p.can_pause);

        // No track: neither action applies, even with stray upstream flags.
        let mut empty = np(false, None);
        empty.can_play = true;
        empty.can_next = true;
        empty.can_previous = true;
        let p = plan(&empty);
        assert!(!p.can_play);
        assert!(!p.can_pause);
        assert!(!p.can_next, "queue flags are masked without a track");
        assert!(!p.can_previous);
    }

    #[test]
    fn maps_metadata_and_timing_fields() {
        let mut n = np(true, Some("Song"));
        n.artist = Some("Artist".to_owned());
        n.album = Some("Album".to_owned());
        n.duration = Some(200.0);
        n.elapsed = Some(5.0);
        n.image_url = Some("http://host/cover.jpg".to_owned());
        n.can_next = true;

        let p = plan(&n);
        assert_eq!(p.artist.as_deref(), Some("Artist"));
        assert_eq!(p.album.as_deref(), Some("Album"));
        assert_eq!(p.duration_secs, Some(200.0));
        assert_eq!(p.elapsed_secs, Some(5.0));
        assert_eq!(p.image_url.as_deref(), Some("http://host/cover.jpg"));
        assert!(p.can_next);
        assert!(!p.can_previous);
        assert!(!p.live, "finite duration is not a live stream");
    }

    #[test]
    fn live_stream_is_track_without_duration() {
        let p = plan(&np(true, Some("Radio")));
        assert!(p.live);

        // No track at all is Stopped, not live.
        let p = plan(&np(false, None));
        assert!(!p.live);
    }

    #[test]
    fn degenerate_durations_normalize_to_live() {
        for degenerate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut n = np(true, Some("Radio"));
            n.duration = Some(degenerate);
            let p = plan(&n);
            assert_eq!(
                p.duration_secs, None,
                "duration {degenerate} is not a length"
            );
            assert!(p.live, "duration {degenerate} must map to live");
        }
    }
}
