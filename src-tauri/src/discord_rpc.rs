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
    loop {
        match rx.recv() {
            Ok(np) => {
                // Check if Discord RPC is enabled
                if !DISCORD_RPC_ENABLED.load(Ordering::SeqCst) {
                    continue;
                }

                let _ = update_discord_activity(&mut client, &np);
            }
            Err(_) => {
                // Channel closed
                break;
            }
        }
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
    let track_name = np.track.as_deref().unwrap_or("Unknown Track");
    let artist_name = np.artist.as_deref().unwrap_or("Unknown Artist");
    let album_name = np.album.as_deref().unwrap_or("");
    let image_url = np.image_url.as_deref().unwrap_or("");

    // Calculate timestamps
    let current_time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let elapsed_ms = np.elapsed.unwrap_or(0) as i64 * 1000;
    let duration_ms = np.duration.unwrap_or(0) as i64 * 1000;

    let started = current_time - elapsed_ms;
    let end = if duration_ms > 0 {
        current_time + (duration_ms - elapsed_ms)
    } else {
        0
    };

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
