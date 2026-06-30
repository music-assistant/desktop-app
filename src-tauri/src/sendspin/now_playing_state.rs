//! Transport-agnostic accumulator that folds Sendspin protocol messages into a
//! [`NowPlaying`] snapshot.
//!
//! Two protocol facts drive the design:
//! - `server/state` carries **merge deltas**: a progress tick re-sends
//!   `metadata` with only `progress` set and `title`/`artist` omitted. We MERGE
//!   (absent field = keep existing) rather than rebuilding, so progress ticks
//!   don't wipe the track.
//! - `group/update.playback_state` is the **only** authoritative play/stop
//!   signal. `stream/end` arrives late mid-transition and must not touch
//!   now-playing state.

use crate::now_playing::NowPlaying;
use sendspin::protocol::messages::{GroupUpdate, MetadataState, PlaybackState};

/// Server progress fields are milliseconds; `NowPlaying` is seconds.
const MILLIS_PER_SEC: f64 = 1000.0;

/// Folds protocol messages into a coherent now-playing view.
///
/// `is_playing` is driven exclusively by `group/update`; metadata fields are
/// merged from `server/state` deltas.
pub struct NowPlayingState {
    player_id: String,
    player_name: String,
    is_playing: bool,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    image_url: Option<String>,
    duration: Option<f64>,
    elapsed: Option<f64>,
}

impl NowPlayingState {
    pub fn new(player_id: String, player_name: String) -> Self {
        Self {
            player_id,
            player_name,
            is_playing: false,
            title: None,
            artist: None,
            album: None,
            image_url: None,
            duration: None,
            elapsed: None,
        }
    }

    /// Apply a `group/update`. Only `playback_state` is authoritative for
    /// play/stop; an update without it leaves state untouched.
    pub fn apply_group_update(&mut self, gu: &GroupUpdate) {
        if let Some(ps) = &gu.playback_state {
            self.is_playing = matches!(ps, PlaybackState::Playing);
        }
    }

    /// Merge a `server/state` metadata delta: a present field overwrites, an
    /// absent (`None`) field keeps the existing value. serde cannot distinguish
    /// "absent (keep)" from `"field": null` (clear); we deliberately choose
    /// keep, since the server signals clears via `group/update: Stopped`, not
    /// null titles.
    pub fn apply_metadata(&mut self, md: &MetadataState) {
        if let Some(title) = &md.title {
            self.title = Some(title.clone());
        }
        if let Some(artist) = &md.artist {
            self.artist = Some(artist.clone());
        }
        if let Some(album) = &md.album {
            self.album = Some(album.clone());
        }
        if let Some(artwork_url) = &md.artwork_url {
            self.image_url = Some(artwork_url.clone());
        }
        if let Some(p) = &md.progress {
            // Don't crash on negative values
            self.elapsed = Some(p.track_progress.max(0) as f64 / MILLIS_PER_SEC);
            // 0 = live/unknown stream (no finite length). Represent as absent
            // rather than a bogus zero-length track so the UI can show
            // elapsed-only instead of a 0:00/0:00 progress bar.
            self.duration =
                (p.track_duration > 0).then(|| p.track_duration as f64 / MILLIS_PER_SEC);
        }
    }

    /// Render the current accumulated state as a [`NowPlaying`] for the UI/tray.
    pub fn snapshot(&self) -> NowPlaying {
        NowPlaying {
            is_playing: self.is_playing,
            track: self.title.clone(),
            artist: self.artist.clone(),
            album: self.album.clone(),
            image_url: self.image_url.clone(),
            player_name: Some(self.player_name.clone()),
            player_id: Some(self.player_id.clone()),
            duration: self.duration,
            elapsed: self.elapsed,
            can_play: !self.is_playing,
            can_pause: self.is_playing,
            can_next: true,
            can_previous: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendspin::protocol::messages::TrackProgress;

    const PLAYER_ID: &str = "player-1";
    const PLAYER_NAME: &str = "Living Room";
    const TITLE: &str = "100 Days, 100 Nights";
    const ARTIST: &str = "Sharon Jones & The Dap-Kings";

    fn state() -> NowPlayingState {
        NowPlayingState::new(PLAYER_ID.to_string(), PLAYER_NAME.to_string())
    }

    /// Empty metadata delta with only `timestamp` and `progress` set.
    fn progress_delta(progress_ms: i64, duration_ms: i64) -> MetadataState {
        MetadataState {
            timestamp: 0,
            title: None,
            artist: None,
            album_artist: None,
            album: None,
            artwork_url: None,
            year: None,
            track: None,
            progress: Some(TrackProgress {
                track_progress: progress_ms,
                track_duration: duration_ms,
                playback_speed: 1000,
            }),
            repeat: None,
            shuffle: None,
        }
    }

    fn track_delta(title: &str, artist: &str) -> MetadataState {
        MetadataState {
            timestamp: 0,
            title: Some(title.to_string()),
            artist: Some(artist.to_string()),
            album_artist: None,
            album: None,
            artwork_url: None,
            year: None,
            track: None,
            progress: None,
            repeat: None,
            shuffle: None,
        }
    }

    fn group_update(ps: PlaybackState) -> GroupUpdate {
        GroupUpdate {
            playback_state: Some(ps),
            group_id: None,
            group_name: None,
        }
    }

    #[test]
    fn progress_only_delta_keeps_track_and_updates_position() {
        let mut s = state();
        s.apply_metadata(&track_delta(TITLE, ARTIST));

        let progress_ms = 30_000;
        let duration_ms = 210_000;
        s.apply_metadata(&progress_delta(progress_ms, duration_ms));

        let snap = s.snapshot();
        assert_eq!(snap.track.as_deref(), Some(TITLE));
        assert_eq!(snap.artist.as_deref(), Some(ARTIST));
        assert_eq!(snap.elapsed, Some(progress_ms as f64 / MILLIS_PER_SEC));
        assert_eq!(snap.duration, Some(duration_ms as f64 / MILLIS_PER_SEC));
    }

    #[test]
    fn live_stream_zero_duration_is_absent() {
        let mut s = state();
        let progress_ms = 45_000;
        s.apply_metadata(&progress_delta(progress_ms, 0));

        let snap = s.snapshot();
        assert_eq!(snap.elapsed, Some(progress_ms as f64 / MILLIS_PER_SEC));
        assert_eq!(
            snap.duration, None,
            "0 duration is live/unknown, not a zero-length track"
        );
    }

    #[test]
    fn negative_progress_clamps_to_track_start() {
        let mut s = state();
        s.apply_metadata(&progress_delta(-250, 210_000));

        let snap = s.snapshot();
        assert_eq!(snap.elapsed, Some(0.0));
        assert_eq!(snap.duration, Some(210.0));
    }

    #[test]
    fn group_update_drives_is_playing() {
        let mut s = state();

        s.apply_group_update(&group_update(PlaybackState::Playing));
        let playing = s.snapshot();
        assert!(playing.is_playing);
        assert!(playing.can_pause);
        assert!(!playing.can_play);

        s.apply_group_update(&group_update(PlaybackState::Stopped));
        let stopped = s.snapshot();
        assert!(!stopped.is_playing);
        assert!(!stopped.can_pause);
        assert!(stopped.can_play);
    }

    #[test]
    fn stopped_keeps_last_metadata() {
        let mut s = state();
        s.apply_metadata(&track_delta(TITLE, ARTIST));
        s.apply_group_update(&group_update(PlaybackState::Playing));
        s.apply_group_update(&group_update(PlaybackState::Stopped));

        let snap = s.snapshot();
        assert!(!snap.is_playing);
        assert_eq!(snap.track.as_deref(), Some(TITLE), "track survives stop");
        assert_eq!(snap.artist.as_deref(), Some(ARTIST));
    }

    #[test]
    fn metadata_delta_while_stopped_does_not_resume() {
        let mut s = state();
        s.apply_group_update(&group_update(PlaybackState::Stopped));
        s.apply_metadata(&track_delta(TITLE, ARTIST));

        let snap = s.snapshot();
        assert!(!snap.is_playing, "metadata must not flip play state");
        assert_eq!(snap.track.as_deref(), Some(TITLE));
    }

    #[test]
    fn snapshot_carries_player_identity() {
        let snap = state().snapshot();
        assert_eq!(snap.player_id.as_deref(), Some(PLAYER_ID));
        assert_eq!(snap.player_name.as_deref(), Some(PLAYER_NAME));
        assert!(snap.can_next);
        assert!(snap.can_previous);
    }
}
