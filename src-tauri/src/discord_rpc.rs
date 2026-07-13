use crate::now_playing::{self, NowPlaying};
use crate::{i18n, ma_api, DISCORD_RPC_ENABLED};
use discord_rich_presence::{
    activity::{self, ActivityType, StatusDisplayType},
    error::Error as DiscordError,
    DiscordIpc, DiscordIpcClient,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// Discord client id for MASS application
const CLIENT_ID: &str = "1107294634507518023";

/// URL for the "Download companion" activity button
const COMPANION_URL: &str = "https://music-assistant.io/companion-app/";

/// How often the worker wakes up on its own to (re)connect to Discord and
/// re-apply the current state (e.g. after Discord was started or restarted).
const RETRY_INTERVAL: Duration = Duration::from_secs(15);
/// Retry failed artwork metadata lookups without retrying on every playback progress tick.
const ARTWORK_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Discord requires text fields (details, state, `large_text`) to be 2-128 chars.
const MAX_TEXT_CHARS: usize = 128;
/// Discord rejects asset URLs longer than 256 characters.
const MAX_URL_CHARS: usize = 256;
/// Discord rejects button labels longer than 32 characters.
const MAX_BUTTON_LABEL_CHARS: usize = 32;

// Discord IPC opcodes
const OPCODE_FRAME: u32 = 1;
const OPCODE_CLOSE: u32 = 2;
const OPCODE_PING: u32 = 3;
const OPCODE_PONG: u8 = 4;

/// Sender used to nudge the worker thread (e.g. when the user toggles the
/// Discord Rich Presence setting) so it re-evaluates the current state
/// immediately instead of waiting for the next now-playing update.
static WORKER_TX: Mutex<Option<Sender<NowPlaying>>> = Mutex::new(None);

pub fn refresh() {
    if let Ok(guard) = WORKER_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(now_playing::get_now_playing());
        }
    }
}

/// Start the Discord Rich Presence integration.
/// Subscribes to now-playing changes and updates Discord accordingly.
pub fn start_rpc() {
    // Use a channel to receive now-playing updates
    let (tx, rx) = std::sync::mpsc::channel::<NowPlaying>();

    if let Ok(mut guard) = WORKER_TX.lock() {
        *guard = Some(tx.clone());
    }

    // Register callback for now-playing changes
    now_playing::on_now_playing_change(Arc::new(move |np| {
        let _ = tx.send(np.clone());
    }));

    run_worker(&rx);
}

/// Worker loop: owns the (single) IPC connection to Discord.
fn run_worker(rx: &Receiver<NowPlaying>) {
    let mut client: Option<DiscordIpcClient> = None;
    let mut artwork_resolver = ArtworkResolver::default();
    // Fingerprint of the last state pushed to Discord
    let mut last_applied: Option<String> = None;

    loop {
        let mut np = match rx.recv_timeout(RETRY_INTERVAL) {
            Ok(np) => np,
            Err(RecvTimeoutError::Timeout) => now_playing::get_now_playing(),
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Coalesce bursts of updates: only the latest state matters.
        while let Ok(newer) = rx.try_recv() {
            np = newer;
        }

        let enabled = DISCORD_RPC_ENABLED.load(Ordering::SeqCst);
        let show_activity = enabled && np.is_playing;
        let image_url = if show_activity {
            artwork_resolver.resolve(&np)
        } else {
            None
        };
        let fingerprint = state_fingerprint(&np, enabled, image_url.as_deref());

        if last_applied.as_deref() == Some(fingerprint.as_str())
            && (client.is_some() || !show_activity)
        {
            continue;
        }

        let desired = if show_activity {
            Some(ActivityFields::from_now_playing(&np, image_url))
        } else {
            None
        };

        // A connection we don't have shows no activity; nothing to clear.
        if desired.is_none() && client.is_none() {
            last_applied = Some(fingerprint);
            continue;
        }

        if apply_with_reconnect(&mut client, desired.as_ref()) {
            last_applied = Some(fingerprint);
        } else {
            // Retry on the next update or periodic wake-up
            last_applied = None;
        }
    }
}

fn apply_with_reconnect(
    client_slot: &mut Option<DiscordIpcClient>,
    desired: Option<&ActivityFields>,
) -> bool {
    for attempt in 0..2u8 {
        if client_slot.is_none() {
            // A fresh connection shows no activity; nothing to clear.
            if desired.is_none() {
                return true;
            }
            *client_slot = connect_client();
        }
        let Some(client) = client_slot.as_mut() else {
            return false;
        };
        match apply_activity(client, desired) {
            Ok(()) => return true,
            Err(err) => {
                // Connection-level failure (Discord quit or restarted):
                // drop the client and retry once with a fresh connection.
                if attempt == 0 {
                    log::info!("[Discord] Connection lost ({err}); reconnecting");
                } else {
                    log::info!("[Discord] Connection lost ({err}); will retry later");
                }
                *client_slot = None;
            }
        }
    }
    false
}

/// Open a new IPC connection to Discord.
fn connect_client() -> Option<DiscordIpcClient> {
    let mut client = DiscordIpcClient::new(CLIENT_ID);
    match client.connect() {
        Ok(()) => {
            log::info!("[Discord] Connected to Discord client");
            Some(client)
        }
        Err(err) => {
            // Expected whenever Discord isn't running; keep it quiet.
            log::debug!("[Discord] Discord not reachable: {err}");
            None
        }
    }
}

/// Push the desired activity (or clear it) over an established connection.
///
/// `SET_ACTIVITY` can be rejected by Discord as a whole (observed as
/// `"Unknown Error" (code 1000)`, e.g. for image URLs its media proxy cannot
/// use). The library never reads those responses, so we check them ourselves
/// and fall back to progressively simpler payloads instead of silently showing
/// nothing.
///
/// Returns `Err` only for connection-level failures.
fn apply_activity(
    client: &mut DiscordIpcClient,
    desired: Option<&ActivityFields>,
) -> Result<(), DiscordError> {
    let Some(fields) = desired else {
        client.clear_activity()?;
        let response = recv_command_response(client)?;
        if let Some((code, message)) = response_error(&response) {
            log::warn!("[Discord] Failed to clear activity: {message} (code {code})");
        } else {
            log::debug!("[Discord] Cleared activity");
        }
        return Ok(());
    };

    // Full payload first, then progressively simpler fallbacks.
    let variants: &[PayloadVariant] = if fields.image_url.is_some() {
        &[
            PayloadVariant::Full,
            PayloadVariant::NoAssets,
            PayloadVariant::Minimal,
        ]
    } else {
        &[PayloadVariant::NoAssets, PayloadVariant::Minimal]
    };

    for variant in variants {
        client.set_activity(build_activity(fields, *variant))?;
        let response = recv_command_response(client)?;
        match response_error(&response) {
            None => {
                log::debug!(
                    "[Discord] Activity updated ({variant:?}): {} - {}",
                    fields.details,
                    fields.state
                );
                return Ok(());
            }
            Some((code, message)) => {
                log::warn!(
                    "[Discord] SET_ACTIVITY rejected ({variant:?} payload): {message} (code {code})"
                );
            }
        }
    }

    log::warn!("[Discord] All activity payload variants rejected; giving up until state changes");
    Ok(())
}

fn recv_command_response(client: &mut DiscordIpcClient) -> Result<Value, DiscordError> {
    loop {
        let (opcode, payload) = client.recv()?;
        match opcode {
            OPCODE_FRAME => return Ok(payload),
            OPCODE_PING => client.send(payload, OPCODE_PONG)?,
            OPCODE_CLOSE => return Err(DiscordError::NotConnected),
            _ => {}
        }
    }
}

/// Extract `(code, message)` if the response reports an error.
fn response_error(response: &Value) -> Option<(i64, String)> {
    if response.get("evt").and_then(Value::as_str) != Some("ERROR") {
        return None;
    }
    let code = response
        .pointer("/data/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let message = response
        .pointer("/data/message")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    Some((code, message))
}

/// Which parts of the activity payload to include (fallback ladder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PayloadVariant {
    /// Artwork, buttons, status display type
    Full,
    /// No artwork (Discord rejects image URLs its media proxy cannot use)
    NoAssets,
    /// Bare minimum: track, artist and timestamps only
    Minimal,
}

#[derive(Debug, Deserialize)]
struct MaQueueItem {
    image: Option<MaImage>,
}

#[derive(Debug, Deserialize)]
struct MaPlayerQueue {
    current_item: Option<MaQueueItem>,
}

#[derive(Debug, Deserialize)]
struct MaImage {
    path: Option<String>,
    #[serde(default)]
    remotely_accessible: bool,
}

/// Owned, sanitized field values for a Discord activity payload.
struct ActivityFields {
    /// Track name (2-128 chars)
    details: String,
    /// Artist name (2-128 chars)
    state: String,
    /// Album name for artwork hover text (2-128 chars), if known
    large_text: Option<String>,
    /// Artwork URL, only if Discord's media proxy has a chance of using it
    image_url: Option<String>,
    /// Unix ms when playback of this track started
    start_ms: i64,
    /// Unix ms when the track will end, if the duration is known
    end_ms: Option<i64>,
    /// Label for the "Download companion" button (max 32 chars)
    button_label: String,
}

impl ActivityFields {
    fn from_now_playing(np: &NowPlaying, image_url: Option<String>) -> Self {
        let details = sanitize_text(
            np.track.as_deref().unwrap_or_default(),
            &i18n::tr("desktop.discord.unknown_track"),
        );
        let state = sanitize_text(
            np.artist.as_deref().unwrap_or_default(),
            &i18n::tr("desktop.discord.unknown_artist"),
        );
        let large_text = np
            .album
            .as_deref()
            .map(str::trim)
            .filter(|album| !album.is_empty())
            .map(|album| sanitize_text(album, ""));
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let (start_ms, end_ms) =
            calculate_discord_timestamps(np.elapsed, np.duration, current_time);

        let button_label: String = i18n::tr("desktop.discord.download_companion")
            .chars()
            .take(MAX_BUTTON_LABEL_CHARS)
            .collect();

        Self {
            details,
            state,
            large_text,
            image_url,
            start_ms,
            end_ms,
            button_label,
        }
    }
}

/// Build one payload variant borrowing from the sanitized fields.
fn build_activity(fields: &ActivityFields, variant: PayloadVariant) -> activity::Activity<'_> {
    let mut timestamps = activity::Timestamps::new().start(fields.start_ms);
    if let Some(end) = fields.end_ms {
        timestamps = timestamps.end(end);
    }

    let mut payload = activity::Activity::new()
        .activity_type(ActivityType::Listening)
        .details(&fields.details)
        .state(&fields.state)
        .timestamps(timestamps);

    if variant != PayloadVariant::Minimal {
        payload = payload
            .status_display_type(StatusDisplayType::Details)
            .buttons(vec![activity::Button::new(
                &fields.button_label,
                COMPANION_URL,
            )]);
    }

    if variant == PayloadVariant::Full {
        if let Some(url) = &fields.image_url {
            let mut assets = activity::Assets::new().large_image(url);
            if let Some(text) = &fields.large_text {
                assets = assets.large_text(text);
            }
            payload = payload.assets(assets);
        }
    }

    payload
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArtworkCacheKey {
    player_id: String,
    track: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u64>,
}

impl ArtworkCacheKey {
    fn from_now_playing(np: &NowPlaying) -> Option<Self> {
        let player_id = np.player_id.as_deref()?.trim();
        if player_id.is_empty() {
            return None;
        }
        Some(Self {
            player_id: player_id.to_string(),
            track: normalized_optional_string(np.track.as_deref()),
            artist: normalized_optional_string(np.artist.as_deref()),
            album: normalized_optional_string(np.album.as_deref()),
            duration_ms: np.duration.map(|seconds| (seconds * 1000.0).round() as u64),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtworkFetchStatus {
    Fetched,
    Failed,
}

#[derive(Clone, Debug)]
struct ArtworkCacheEntry {
    key: ArtworkCacheKey,
    image_url: Option<String>,
    status: ArtworkFetchStatus,
    fetched_at: Instant,
}

#[derive(Default)]
struct ArtworkResolver {
    cache: Option<ArtworkCacheEntry>,
}

impl ArtworkResolver {
    fn resolve(&mut self, np: &NowPlaying) -> Option<String> {
        let Some(key) = ArtworkCacheKey::from_now_playing(np) else {
            self.cache = None;
            return None;
        };

        if let Some(entry) = &self.cache {
            if entry.key == key
                && (entry.status == ArtworkFetchStatus::Fetched
                    || entry.fetched_at.elapsed() < ARTWORK_FAILURE_RETRY_INTERVAL)
            {
                return entry.image_url.clone();
            }
        }

        let (image_url, status) = match fetch_public_artwork_url_for_player(&key.player_id) {
            Ok(image_url) => (image_url, ArtworkFetchStatus::Fetched),
            Err(err) => {
                log::debug!("[Discord RPC] Could not fetch public artwork metadata: {err}");
                (
                    self.cache.as_ref().and_then(|entry| {
                        if entry.key == key {
                            entry.image_url.clone()
                        } else {
                            None
                        }
                    }),
                    ArtworkFetchStatus::Failed,
                )
            }
        };

        self.cache = Some(ArtworkCacheEntry {
            key,
            image_url: image_url.clone(),
            status,
            fetched_at: Instant::now(),
        });
        image_url
    }
}

fn normalized_optional_string(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Fingerprint of the state a now-playing snapshot asks Discord to show.
/// Deliberately built from the raw snapshot (not from computed timestamps,
/// which shift with the wall clock) so identical states compare equal.
fn state_fingerprint(np: &NowPlaying, enabled: bool, image_url: Option<&str>) -> String {
    if !enabled || !np.is_playing {
        return "cleared".to_string();
    }
    format!(
        "{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}",
        np.track, np.artist, np.album, np.player_id, np.elapsed, np.duration
    ) + "\u{1}"
        + &format!("{image_url:?}")
}

fn public_artwork_url_from_queue(queue: MaPlayerQueue) -> Option<String> {
    let image = queue.current_item?.image?;
    if !image.remotely_accessible {
        return None;
    }
    image.path.as_deref().and_then(sanitize_image_url)
}

fn public_artwork_url_from_api_response(response_body: &str) -> Result<Option<String>, String> {
    serde_json::from_str::<Option<MaPlayerQueue>>(response_body)
        .map(|queue| queue.and_then(public_artwork_url_from_queue))
        .map_err(|err| err.to_string())
}

fn fetch_public_artwork_url_for_player(player_id: &str) -> Result<Option<String>, String> {
    let response_body = ma_api::get_active_queue(player_id)?;
    public_artwork_url_from_api_response(&response_body)
}

/// Clamp text to Discord's 2-128 character requirement, substituting
/// `fallback` for empty values and padding single characters.
fn sanitize_text(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    let base = if trimmed.is_empty() {
        fallback
    } else {
        trimmed
    };
    let mut text: String = base.chars().take(MAX_TEXT_CHARS).collect();
    if text.chars().count() == 1 {
        // Discord requires at least 2 characters when the field is present
        text.push(' ');
    }
    text
}

/// Only pass artwork URLs that Discord's media proxy can actually use:
/// it requires a publicly fetchable https URL of at most 256 characters.
/// Anything else (e.g. a plain-http server URL) makes Discord reject the
/// whole `SET_ACTIVITY` command.
fn sanitize_image_url(url: &str) -> Option<String> {
    let url = url.trim();
    let is_https = url
        .get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    if !is_https || url.chars().count() > MAX_URL_CHARS {
        return None;
    }
    let after_scheme = &url[8..];
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if host.is_empty()
        || host.contains('@')
        || url.chars().any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return None;
    }
    Some(url.to_string())
}

/// Calculate Discord activity timestamps.
fn calculate_discord_timestamps(
    elapsed_secs: Option<f64>,
    duration_secs: Option<f64>,
    current_time_ms: i64,
) -> (i64, Option<i64>) {
    let elapsed_ms = (elapsed_secs.unwrap_or(0.0) * 1000.0) as i64;
    let duration_ms = (duration_secs.unwrap_or(0.0) * 1000.0) as i64;
    let started = current_time_ms - elapsed_ms;
    let end = current_time_ms + (duration_ms - elapsed_ms);
    if duration_ms > 0 && end > current_time_ms && end > started {
        (started, Some(end))
    } else {
        (started, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_discord_timestamps() {
        type TimestampCase = (Option<f64>, Option<f64>, i64, i64, Option<i64>);
        let t = 100_000i64;
        // (elapsed, duration, current_time) → (expected_start, expected_end)
        let cases: Vec<TimestampCase> = vec![
            // Normal playback: 30s into a 180s track
            (Some(30.0), Some(180.0), t, t - 30_000, Some(t + 150_000)),
            // No duration (e.g. radio stream) → no end timestamp
            (Some(30.0), None, t, t - 30_000, None),
            // Zero duration → no end timestamp
            (Some(30.0), Some(0.0), t, t - 30_000, None),
            // No elapsed, no duration → start=current, no end
            (None, None, t, t, None),
            // Elapsed exceeds duration (track overran): end would precede
            // start, which Discord rejects → no end timestamp
            (Some(180.0), Some(120.0), t, t - 180_000, None),
            // Large values (1hr into 2hr track)
            (
                Some(3600.0),
                Some(7200.0),
                1_000_000_000,
                1_000_000_000 - 3_600_000,
                Some(1_000_000_000 + 3_600_000),
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

    #[test]
    fn test_sanitize_image_url_requires_https() {
        // Plain-http URLs make Discord reject the whole activity (issue #54)
        assert_eq!(
            sanitize_image_url("http://musicassistant:8095/imageproxy/abc?size=512"),
            None
        );
        assert_eq!(
            sanitize_image_url("https://my.server.example/imageproxy/abc?size=512"),
            Some("https://my.server.example/imageproxy/abc?size=512".to_string())
        );
        // Scheme check is case-insensitive
        assert!(sanitize_image_url("HTTPS://my.server.example/img").is_some());
        assert_eq!(sanitize_image_url(""), None);
        assert_eq!(sanitize_image_url("not a url"), None);
        assert_eq!(sanitize_image_url("https://"), None);
        assert_eq!(sanitize_image_url("https:///cover.jpg"), None);
        assert_eq!(
            sanitize_image_url("https://example.com/cover with space.jpg"),
            None
        );
        assert_eq!(
            sanitize_image_url("https://user@example.com/cover.jpg"),
            None
        );
    }

    #[test]
    fn test_sanitize_image_url_rejects_overlong_urls() {
        let long_url = format!("https://example.com/{}", "a".repeat(300));
        assert_eq!(sanitize_image_url(&long_url), None);
    }

    #[test]
    fn test_public_artwork_url_from_queue_uses_only_remote_https() {
        let public_queue = MaPlayerQueue {
            current_item: Some(MaQueueItem {
                image: Some(MaImage {
                    path: Some("https://f4.bcbits.com/img/a2603313414_0.jpg".to_string()),
                    remotely_accessible: true,
                }),
            }),
        };
        assert_eq!(
            public_artwork_url_from_queue(public_queue),
            Some("https://f4.bcbits.com/img/a2603313414_0.jpg".to_string())
        );

        let local_queue = MaPlayerQueue {
            current_item: Some(MaQueueItem {
                image: Some(MaImage {
                    path: Some("https://musicassistant.local/imageproxy/abc".to_string()),
                    remotely_accessible: false,
                }),
            }),
        };
        assert_eq!(public_artwork_url_from_queue(local_queue), None);

        let plain_http_queue = MaPlayerQueue {
            current_item: Some(MaQueueItem {
                image: Some(MaImage {
                    path: Some("http://example.com/cover.jpg".to_string()),
                    remotely_accessible: true,
                }),
            }),
        };
        assert_eq!(public_artwork_url_from_queue(plain_http_queue), None);
    }

    #[test]
    fn test_public_artwork_url_from_api_response_handles_raw_queue_and_null() {
        let response = r#"{
            "current_item": {
                "image": {
                    "path": "https://f4.bcbits.com/img/a2603313414_0.jpg",
                    "remotely_accessible": true
                }
            }
        }"#;
        assert_eq!(
            public_artwork_url_from_api_response(response).unwrap(),
            Some("https://f4.bcbits.com/img/a2603313414_0.jpg".to_string())
        );
        assert_eq!(public_artwork_url_from_api_response("null").unwrap(), None);
    }

    #[test]
    fn test_artwork_cache_key_ignores_elapsed_and_proxy_url() {
        let base = NowPlaying {
            is_playing: true,
            track: Some("Track".to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            image_url: Some("http://musicassistant/imageproxy/one".to_string()),
            player_id: Some("player-1".to_string()),
            duration: Some(123.0),
            elapsed: Some(1.0),
            ..Default::default()
        };
        let changed_progress = NowPlaying {
            image_url: Some("http://musicassistant/imageproxy/two".to_string()),
            elapsed: Some(42.0),
            ..base.clone()
        };
        assert_eq!(
            ArtworkCacheKey::from_now_playing(&base),
            ArtworkCacheKey::from_now_playing(&changed_progress)
        );
    }

    #[test]
    fn test_sanitize_text_clamps_and_pads() {
        // Empty values fall back
        assert_eq!(sanitize_text("", "Unknown Track"), "Unknown Track");
        assert_eq!(sanitize_text("   ", "Unknown Track"), "Unknown Track");
        // Single characters are padded to Discord's 2-char minimum
        assert_eq!(sanitize_text("X", "fallback"), "X ");
        // Long values are clamped to 128 characters
        let long = "a".repeat(200);
        assert_eq!(sanitize_text(&long, "fallback").chars().count(), 128);
        // Normal values pass through
        assert_eq!(sanitize_text("Zenzenzense", "fallback"), "Zenzenzense");
    }

    #[test]
    fn test_activity_fields_use_resolved_artwork() {
        let np = NowPlaying {
            is_playing: true,
            track: Some("Zenzenzense".to_string()),
            artist: Some("Vaundy".to_string()),
            album: None,
            image_url: Some("http://musicassistant:8095/imageproxy/abc".to_string()),
            duration: Some(263.0),
            elapsed: Some(10.0),
            ..Default::default()
        };
        let fields = ActivityFields::from_now_playing(
            &np,
            Some("https://f4.bcbits.com/img/a2603313414_0.jpg".to_string()),
        );
        assert_eq!(
            fields.image_url,
            Some("https://f4.bcbits.com/img/a2603313414_0.jpg".to_string())
        );
        assert_eq!(fields.large_text, None);
        assert_eq!(fields.details, "Zenzenzense");
        assert_eq!(fields.state, "Vaundy");
        assert!(fields.end_ms.is_some());
    }

    #[test]
    fn test_state_fingerprint() {
        let np = NowPlaying {
            is_playing: true,
            track: Some("Track".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        };

        // Stable for identical states
        assert_eq!(
            state_fingerprint(&np, true, None),
            state_fingerprint(&np, true, None)
        );

        // Disabled or stopped states collapse to "cleared"
        assert_eq!(state_fingerprint(&np, false, None), "cleared");
        let stopped = NowPlaying {
            is_playing: false,
            ..np.clone()
        };
        assert_eq!(state_fingerprint(&stopped, true, None), "cleared");

        // Any relevant change yields a different fingerprint
        let other_track = NowPlaying {
            track: Some("Other".to_string()),
            ..np.clone()
        };
        assert_ne!(
            state_fingerprint(&np, true, None),
            state_fingerprint(&other_track, true, None)
        );
        let seeked = NowPlaying {
            elapsed: Some(42.0),
            ..np.clone()
        };
        assert_ne!(
            state_fingerprint(&np, true, None),
            state_fingerprint(&seeked, true, None)
        );

        // Resolved artwork affects the payload, but raw MA proxy URL churn does not.
        assert_ne!(
            state_fingerprint(&np, true, None),
            state_fingerprint(&np, true, Some("https://example.com/cover.jpg"))
        );
        let proxy_refreshed = NowPlaying {
            image_url: Some("http://musicassistant/imageproxy/refreshed".to_string()),
            ..np.clone()
        };
        assert_eq!(
            state_fingerprint(&np, true, None),
            state_fingerprint(&proxy_refreshed, true, None)
        );
    }
}
