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
    pub duration: Option<u64>,
    /// Elapsed time in seconds
    pub elapsed: Option<u64>,
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
    if !np.is_playing {
        match &np.player_name {
            Some(name) => format!("{} - Not Playing", name),
            None => "Music Assistant".to_string(),
        }
    } else {
        let track_info = match (&np.artist, &np.track) {
            (Some(artist), Some(track)) => format!("{} - {}", artist, track),
            (None, Some(track)) => track.clone(),
            _ => "Unknown Track".to_string(),
        };

        match &np.player_name {
            Some(name) => format!("{}\n{}", track_info, name),
            None => track_info,
        }
    }
}
