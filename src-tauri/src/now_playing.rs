use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};

/// Current now-playing information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NowPlaying {
    /// Whether something is currently playing
    pub is_playing: bool,
    /// Track name
    pub track: Option<String>,
    /// Artist name
    pub artist: Option<String>,
    /// Album name
    pub album: Option<String>,
    /// Image URL
    pub image_url: Option<String>,
    /// Player name
    pub player_name: Option<String>,
    /// Player ID
    pub player_id: Option<String>,
    /// Duration in seconds
    pub duration: Option<f64>,
    /// Elapsed time in seconds
    pub elapsed: Option<f64>,
    /// Whether play action is available
    #[serde(default)]
    pub can_play: bool,
    /// Whether pause action is available
    #[serde(default)]
    pub can_pause: bool,
    /// Whether next track action is available
    #[serde(default)]
    pub can_next: bool,
    /// Whether previous track action is available
    #[serde(default)]
    pub can_previous: bool,
}

/// Callback type for now-playing updates
pub type NowPlayingCallback = Arc<dyn Fn(&NowPlaying) + Send + Sync>;

/// Global now-playing state
static NOW_PLAYING: RwLock<NowPlaying> = RwLock::new(NowPlaying {
    is_playing: false,
    track: None,
    artist: None,
    album: None,
    image_url: None,
    player_name: None,
    player_id: None,
    duration: None,
    elapsed: None,
    can_play: false,
    can_pause: false,
    can_next: false,
    can_previous: false,
});

/// Callbacks to notify when now-playing changes
static CALLBACKS: Mutex<Vec<NowPlayingCallback>> = Mutex::new(Vec::new());

/// Get the current now-playing state
pub fn get_now_playing() -> NowPlaying {
    NOW_PLAYING.read().unwrap().clone()
}

/// Register a callback to be notified when now-playing changes
pub fn on_now_playing_change(callback: NowPlayingCallback) {
    if let Ok(mut callbacks) = CALLBACKS.lock() {
        callbacks.push(callback);
    }
}

/// Update the now-playing state (called from frontend via Tauri command)
pub fn update_now_playing(now_playing: NowPlaying) {
    // Skip updates where playback is active but track info is missing (race condition)
    // This prevents showing "Unknown - Unknown" in the tray while data is loading
    if now_playing.is_playing && now_playing.track.is_none() {
        return;
    }

    // Update global state
    if let Ok(mut state) = NOW_PLAYING.write() {
        *state = now_playing.clone();
    }

    // Notify all callbacks (tray tooltip, Discord RPC, etc.)
    if let Ok(callbacks) = CALLBACKS.lock() {
        for callback in callbacks.iter() {
            callback(&now_playing);
        }
    }
}

/// Format now-playing info for display (e.g., tray tooltip)
#[allow(dead_code)]
pub fn format_now_playing(np: &NowPlaying) -> String {
    if !np.is_playing {
        return "Music Assistant - Not Playing".to_string();
    }

    match (&np.artist, &np.track) {
        (Some(artist), Some(track)) => format!("{} - {}", artist, track),
        (None, Some(track)) => track.clone(),
        _ => "Music Assistant - Playing".to_string(),
    }
}

/// Format now-playing info with player name
pub fn format_now_playing_with_player(np: &NowPlaying) -> String {
    if np.is_playing {
        let track_info = match (&np.artist, &np.track) {
            (Some(artist), Some(track)) => format!("{} - {}", artist, track),
            (None, Some(track)) => track.clone(),
            _ => "Unknown Track".to_string(),
        };

        match &np.player_name {
            Some(name) => format!("{}\n{}", track_info, name),
            None => track_info,
        }
    } else {
        match &np.player_name {
            Some(name) => format!("{} - Not Playing", name),
            None => "Music Assistant".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_update_skips_playing_without_track() {
        // Save current state to restore later
        let original_state = get_now_playing();

        // Reset global state to a known state
        {
            if let Ok(mut state) = NOW_PLAYING.write() {
                *state = NowPlaying {
                    is_playing: false,
                    track: Some("SomeTrack".to_string()),
                    ..Default::default()
                };
            }
        }

        // Verify state before update
        let before_update = get_now_playing();
        let before_track = before_update.track.clone();

        // Call update_now_playing with is_playing=true but no track (should be skipped)
        let update = NowPlaying {
            is_playing: true,
            track: None,
            ..Default::default()
        };
        update_now_playing(update);

        // Verify state unchanged - track should still be present
        let after_update = get_now_playing();
        assert_eq!(
            after_update.track, before_track,
            "track should remain unchanged"
        );
        assert!(!after_update.is_playing, "is_playing should remain false");

        // Restore original state
        {
            if let Ok(mut state) = NOW_PLAYING.write() {
                *state = original_state;
            }
        }
    }

    #[test]
    fn test_callback_invoked_on_update() {
        // Reset global state and callbacks
        {
            if let Ok(mut state) = NOW_PLAYING.write() {
                *state = NowPlaying::default();
            }
        }
        {
            if let Ok(mut callbacks) = CALLBACKS.lock() {
                callbacks.clear();
            }
        }

        // Create a flag to track callback invocation
        let callback_invoked = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&callback_invoked);

        // Register callback that sets the flag to true
        let callback: NowPlayingCallback = Arc::new(move |_np| {
            flag_clone.store(true, Ordering::SeqCst);
        });
        on_now_playing_change(callback);

        // Call update_now_playing with valid data
        let update = NowPlaying {
            is_playing: true,
            track: Some("Test Track".to_string()),
            ..Default::default()
        };
        update_now_playing(update);

        // Verify callback was invoked
        assert!(callback_invoked.load(Ordering::SeqCst));
    }

    #[test]
    fn test_format_now_playing_with_player_all_branches() {
        // Test 1: is_playing=true, artist=Some, track=Some, player_name=Some
        let np1 = NowPlaying {
            is_playing: true,
            artist: Some("Artist1".to_string()),
            track: Some("Track1".to_string()),
            player_name: Some("Player1".to_string()),
            ..Default::default()
        };
        assert_eq!(
            format_now_playing_with_player(&np1),
            "Artist1 - Track1\nPlayer1"
        );

        // Test 2: is_playing=true, artist=Some, track=Some, player_name=None
        let np2 = NowPlaying {
            is_playing: true,
            artist: Some("Artist2".to_string()),
            track: Some("Track2".to_string()),
            player_name: None,
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np2), "Artist2 - Track2");

        // Test 3: is_playing=true, artist=None, track=Some, player_name=Some
        let np3 = NowPlaying {
            is_playing: true,
            artist: None,
            track: Some("Track3".to_string()),
            player_name: Some("Player3".to_string()),
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np3), "Track3\nPlayer3");

        // Test 4: is_playing=true, artist=None, track=Some, player_name=None
        let np4 = NowPlaying {
            is_playing: true,
            artist: None,
            track: Some("Track4".to_string()),
            player_name: None,
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np4), "Track4");

        // Test 5: is_playing=true, artist=Some, track=None, player_name=None
        let np5 = NowPlaying {
            is_playing: true,
            artist: Some("Artist5".to_string()),
            track: None,
            player_name: None,
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np5), "Unknown Track");

        // Test 6: is_playing=true, artist=None, track=None, player_name=None
        let np6 = NowPlaying {
            is_playing: true,
            artist: None,
            track: None,
            player_name: None,
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np6), "Unknown Track");

        // Test 7: is_playing=false, player_name=Some("MyPlayer")
        let np7 = NowPlaying {
            is_playing: false,
            player_name: Some("MyPlayer".to_string()),
            ..Default::default()
        };
        assert_eq!(
            format_now_playing_with_player(&np7),
            "MyPlayer - Not Playing"
        );

        // Test 8: is_playing=false, player_name=None
        let np8 = NowPlaying {
            is_playing: false,
            player_name: None,
            ..Default::default()
        };
        assert_eq!(format_now_playing_with_player(&np8), "Music Assistant");
    }
}
