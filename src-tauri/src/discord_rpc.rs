use crate::now_playing::{self, NowPlaying};
use crate::DISCORD_RPC_ENABLED;
use discord_rich_presence::{
    activity::{self, StatusDisplayType},
    DiscordIpc, DiscordIpcClient,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

// Discord client id for MASS application
const CLIENT_ID: &str = "1107294634507518023";

// Global Discord client for clearing activity
static DISCORD_CLIENT: Mutex<Option<DiscordIpcClient>> = Mutex::new(None);

/// Extract Discord fields from `NowPlaying` struct
/// Returns (track, artist, album, `image_url`) with defaults for missing values
fn extract_discord_fields(np: &NowPlaying) -> (&str, &str, &str, &str) {
    let track = np.track.as_deref().unwrap_or("Unknown Track");
    let artist = np.artist.as_deref().unwrap_or("Unknown Artist");
    let album = np.album.as_deref().unwrap_or("");
    let image_url = np.image_url.as_deref().unwrap_or("");
    (track, artist, album, image_url)
}

/// Calculate Discord activity timestamps
/// Takes elapsed and duration in seconds, returns (`start_timestamp`, `end_timestamp`) in milliseconds
/// `current_time_ms` is the current Unix timestamp in milliseconds (allows for testing with fixed time)
fn calculate_discord_timestamps(
    elapsed_secs: Option<f64>,
    duration_secs: Option<f64>,
    current_time_ms: i64,
) -> (i64, i64) {
    let elapsed_ms = (elapsed_secs.unwrap_or(0.0) * 1000.0) as i64;
    let duration_ms = (duration_secs.unwrap_or(0.0) * 1000.0) as i64;
    let started = current_time_ms - elapsed_ms;
    let end = if duration_ms > 0 {
        current_time_ms + (duration_ms - elapsed_ms)
    } else {
        0
    };
    (started, end)
}

/// Clear the Discord activity (called when Discord RPC is disabled)
pub fn clear_activity() {
    if let Ok(mut client_guard) = DISCORD_CLIENT.lock() {
        if let Some(ref mut client) = *client_guard {
            let _ = client.clear_activity();
        }
    }
}

/// Start the Discord Rich Presence integration
/// Subscribes to now-playing changes and updates Discord accordingly
pub fn start_rpc() {
    // Create the Discord RPC client
    let mut client = DiscordIpcClient::new(CLIENT_ID);

    // Connect to the Discord Rich Presence socket
    if client.connect().is_err() {
        return;
    }

    // Store client reference for clear_activity
    if let Ok(mut client_guard) = DISCORD_CLIENT.lock() {
        *client_guard = Some(DiscordIpcClient::new(CLIENT_ID));
        if let Some(ref mut c) = *client_guard {
            let _ = c.connect();
        }
    }

    // Use a channel to receive now-playing updates
    let (tx, rx) = std::sync::mpsc::channel::<NowPlaying>();

    // Register callback for now-playing changes
    now_playing::on_now_playing_change(Arc::new(move |np| {
        let _ = tx.send(np.clone());
    }));

    // Process updates
    while let Ok(np) = rx.recv() {
        // Check if Discord RPC is enabled
        if !DISCORD_RPC_ENABLED.load(Ordering::SeqCst) {
            continue;
        }

        let _ = update_discord_activity(&mut client, &np);
    }
}

fn update_discord_activity(
    client: &mut DiscordIpcClient,
    np: &NowPlaying,
) -> Result<(), Box<dyn std::error::Error>> {
    // Clear activity if not playing
    if !np.is_playing {
        client.clear_activity()?;
        return Ok(());
    }

    // Get track info
    let (track_name, artist_name, album_name, image_url) = extract_discord_fields(np);

    // Calculate timestamps
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let (started, end) = calculate_discord_timestamps(np.elapsed, np.duration, current_time);

    // Build assets
    let mut assets = activity::Assets::new();
    if !image_url.is_empty() {
        assets = assets.large_image(image_url).large_text(album_name);
    }

    // Build timestamps
    let timestamps = activity::Timestamps::new().start(started).end(end);

    // Build buttons
    let buttons = vec![activity::Button::new(
        "Download companion",
        "https://music-assistant.io/companion-app/",
    )];

    // Build activity
    let payload = activity::Activity::new()
        .state(artist_name)
        .details(track_name)
        .assets(assets)
        .buttons(buttons)
        .timestamps(timestamps)
        .status_display_type(StatusDisplayType::Details);

    client.set_activity(payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_discord_timestamps() {
        type TimestampCase = (Option<f64>, Option<f64>, i64, i64, i64);
        let t = 100_000i64;
        // (elapsed, duration, current_time) → (expected_start, expected_end)
        let cases: Vec<TimestampCase> = vec![
            // Normal playback: 30s into a 180s track
            (Some(30.0), Some(180.0), t, t - 30_000, t + 150_000),
            // No duration → end=0
            (Some(30.0), None, t, t - 30_000, 0),
            // Zero duration → end=0
            (Some(30.0), Some(0.0), t, t - 30_000, 0),
            // No elapsed, no duration → start=current, end=0
            (None, None, t, t, 0),
            // Elapsed exceeds duration (track overran)
            (Some(180.0), Some(120.0), t, t - 180_000, t - 60_000),
            // Large values (1hr into 2hr track)
            (
                Some(3600.0),
                Some(7200.0),
                1_000_000_000,
                1_000_000_000 - 3_600_000,
                1_000_000_000 + 3_600_000,
            ),
        ];
        for (elapsed, duration, now, exp_start, exp_end) in cases {
            let (started, end) = calculate_discord_timestamps(elapsed, duration, now);
            assert_eq!(
                started, exp_start,
                "start: elapsed={elapsed:?} duration={duration:?}"
            );
            assert_eq!(
                end, exp_end,
                "end: elapsed={elapsed:?} duration={duration:?}"
            );
        }
    }
}
