//! Native Linux MPRIS media-controls backend over zbus.
//!
//! Registers `org.mpris.MediaPlayer2.music_assistant.instance<PID>` at the
//! standard `/org/mpris/MediaPlayer2` path, sharing the playback-state mapping
//! in [`super::plan`] with the macOS backend.

use super::{plan, MediaControlCallback, PlaybackState};
use crate::now_playing::NowPlaying;
use crate::sendspin;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use tokio::sync::mpsc::{self, UnboundedSender};
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{connection, interface};

const BUS_NAME_BASE: &str = "org.mpris.MediaPlayer2.music_assistant";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const DESKTOP_ENTRY: &str = "music-assistant";
const IDENTITY: &str = "Music Assistant";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

static SERVICE_TX: Mutex<Option<UnboundedSender<ServiceCommand>>> = Mutex::new(None);

#[derive(Debug)]
enum ServiceCommand {
    Update(NowPlaying),
    Clear,
    /// The player volume changed (percent, 0-100); re-emit only `Volume`.
    VolumeChanged(u8),
}

pub fn init(callback: MediaControlCallback, _hwnd_param: Option<*mut std::ffi::c_void>) {
    let mut tx_guard = SERVICE_TX.lock();
    if tx_guard.is_some() {
        return;
    }

    let (tx, rx) = mpsc::unbounded_channel();
    *tx_guard = Some(tx);
    drop(tx_guard);

    // Forward sendspin volume changes to the bus so desktop volume sliders
    // stay in sync between NowPlaying updates.
    sendspin::set_volume_listener(|volume| {
        send_command(ServiceCommand::VolumeChanged(volume));
    });

    // Per-instance suffix (MPRIS2 recommendation) lets a second copy coexist.
    let bus_name = format!("{BUS_NAME_BASE}.instance{}", std::process::id());

    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                log::error!("[MediaControls] Failed to create Linux MPRIS runtime: {e}");
                return;
            }
        };

        if let Err(e) = runtime.block_on(run_service(bus_name, callback, rx)) {
            log::error!("[MediaControls] Linux MPRIS service stopped: {e}");
        }
    });
}

pub fn update(np: &NowPlaying) {
    send_command(ServiceCommand::Update(np.clone()));
}

#[allow(dead_code)]
pub fn clear() {
    send_command(ServiceCommand::Clear);
}

fn send_command(command: ServiceCommand) {
    if let Some(tx) = SERVICE_TX.lock().as_ref() {
        if let Err(e) = tx.send(command) {
            log::warn!("[MediaControls] Failed to send Linux MPRIS update: {e}");
        }
    }
}

async fn run_service(
    bus_name: String,
    callback: MediaControlCallback,
    mut rx: mpsc::UnboundedReceiver<ServiceCommand>,
) -> zbus::Result<()> {
    let shared = SharedState::default();
    let connection = connection::Builder::session()?
        .name(bus_name.as_str())?
        .serve_at(OBJECT_PATH, MediaPlayer2Root)?
        .serve_at(
            OBJECT_PATH,
            MediaPlayer2Player {
                callback,
                state: shared.clone(),
            },
        )?
        .build()
        .await?;

    let emitter = SignalEmitter::new(&connection, OBJECT_PATH)?.to_owned();
    let player_iface: InterfaceName<'static> =
        PLAYER_IFACE.try_into().expect("valid interface name");
    log::info!("[MediaControls] Linux MPRIS service registered as {bus_name}");

    // Async channel keeps the runtime cooperative between updates so inbound
    // method calls (Next/Play/…) are serviced even while idle.
    while let Some(command) = rx.recv().await {
        match command {
            ServiceCommand::Update(np) => {
                shared.update(np);
                emit_player_properties(&emitter, &player_iface, &shared).await;
            }
            ServiceCommand::Clear => {
                shared.clear();
                emit_player_properties(&emitter, &player_iface, &shared).await;
            }
            ServiceCommand::VolumeChanged(volume) => {
                let changed =
                    HashMap::from([("Volume", Value::from(percent_to_mpris_volume(volume)))]);
                emit_properties_changed(&emitter, &player_iface, changed).await;
            }
        }
    }

    Ok(())
}

async fn emit_player_properties(
    emitter: &SignalEmitter<'static>,
    interface: &InterfaceName<'static>,
    state: &SharedState,
) {
    let snapshot = state.snapshot();
    let mut changed: HashMap<&str, Value<'_>> = HashMap::new();
    changed.insert("PlaybackStatus", Value::from(snapshot.playback_status()));
    changed.insert("Metadata", Value::from(snapshot.metadata()));
    changed.insert("CanPlay", Value::from(snapshot.can_play));
    changed.insert("CanPause", Value::from(snapshot.can_pause));
    changed.insert("CanGoNext", Value::from(snapshot.can_next));
    changed.insert("CanGoPrevious", Value::from(snapshot.can_previous));
    changed.insert("Volume", Value::from(current_mpris_volume()));

    // `Position` is omitted on purpose: the spec says clients should track it
    // via the Seeked signal, and CanSeek is false here.
    emit_properties_changed(emitter, interface, changed).await;
}

async fn emit_properties_changed(
    emitter: &SignalEmitter<'static>,
    interface: &InterfaceName<'static>,
    changed: HashMap<&str, Value<'_>>,
) {
    if let Err(e) = fdo::Properties::properties_changed(
        emitter,
        interface.clone(),
        changed,
        std::borrow::Cow::Borrowed(&[]),
    )
    .await
    {
        log::warn!("[MediaControls] Failed to emit Linux MPRIS property change: {e}");
    }
}

#[derive(Clone, Default)]
struct SharedState(std::sync::Arc<Mutex<MprisState>>);

impl SharedState {
    fn snapshot(&self) -> MprisState {
        self.0.lock().clone()
    }

    fn update(&self, np: NowPlaying) {
        *self.0.lock() = MprisState::from_now_playing(&np);
    }

    fn clear(&self) {
        *self.0.lock() = MprisState::default();
    }
}

#[derive(Debug, Clone, Default)]
struct MprisState {
    playback_status: PlaybackState,
    track: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    art_url: Option<String>,
    length_us: Option<i64>,
    position_us: i64,
    can_play: bool,
    can_pause: bool,
    can_next: bool,
    can_previous: bool,
}

impl MprisState {
    fn from_now_playing(np: &NowPlaying) -> Self {
        // Delegate the shared mapping to super::plan so it stays consistent with macOS.
        let plan = plan(np);

        Self {
            playback_status: plan.state,
            track: plan.title,
            artist: plan.artist,
            album: plan.album,
            art_url: plan.image_url,
            length_us: plan.duration_secs.map(seconds_to_microseconds),
            position_us: plan.elapsed_secs.map_or(0, seconds_to_microseconds),
            can_play: np.can_play || !np.is_playing,
            can_pause: np.can_pause || np.is_playing,
            can_next: np.can_next,
            can_previous: np.can_previous,
        }
    }

    fn playback_status(&self) -> &'static str {
        match self.playback_status {
            PlaybackState::Playing => "Playing",
            PlaybackState::Paused => "Paused",
            PlaybackState::Stopped => "Stopped",
        }
    }

    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let mut metadata = HashMap::new();

        // No current track => empty Metadata, per the MPRIS spec. A trackid is
        // only valid when a track is present, so we must not synthesize one
        // here (and the `/.../TrackList/NoTrack` sentinel belongs to the
        // TrackList interface, not Player.Metadata).
        if self.track.is_none() {
            return metadata;
        }

        // mpris:trackid must be stable per track and must not be "/". A content
        // hash differs across tracks but not across updates of one.
        let track_id = ObjectPath::try_from(track_id_path(
            self.track.as_deref(),
            self.artist.as_deref(),
            self.album.as_deref(),
        ))
        .expect("valid object path");
        metadata.insert("mpris:trackid".to_string(), owned_value(track_id));

        if let Some(title) = &self.track {
            metadata.insert("xesam:title".to_string(), owned_value(title.clone()));
        }
        if let Some(artist) = &self.artist {
            metadata.insert(
                "xesam:artist".to_string(),
                owned_value(vec![artist.clone()]),
            );
        }
        if let Some(album) = &self.album {
            metadata.insert("xesam:album".to_string(), owned_value(album.clone()));
        }
        if let Some(art_url) = &self.art_url {
            metadata.insert("mpris:artUrl".to_string(), owned_value(art_url.clone()));
        }
        if let Some(length_us) = self.length_us {
            metadata.insert("mpris:length".to_string(), owned_value(length_us));
        }

        metadata
    }

    fn position_us(&self) -> i64 {
        self.position_us
    }
}

fn track_id_path(track: Option<&str>, artist: Option<&str>, album: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    track.hash(&mut hasher);
    artist.hash(&mut hasher);
    album.hash(&mut hasher);
    format!(
        "/org/music_assistant/desktop/track_{:016x}",
        hasher.finish()
    )
}

fn seconds_to_microseconds(secs: f64) -> i64 {
    if secs.is_finite() && secs > 0.0 {
        (secs * 1_000_000.0).round() as i64
    } else {
        0
    }
}

fn sanitize_mpris_volume(volume: f64) -> f64 {
    if volume.is_finite() {
        volume.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn mpris_volume_to_percent(volume: f64) -> u8 {
    (sanitize_mpris_volume(volume) * 100.0).round() as u8
}

fn percent_to_mpris_volume(volume: u8) -> f64 {
    f64::from(volume.min(100)) / 100.0
}

/// Current player volume as an MPRIS `Volume` value. Falls back to full
/// volume when the sendspin client is not connected or hasn't reported yet.
fn current_mpris_volume() -> f64 {
    sendspin::get_volume_percent()
        .map(percent_to_mpris_volume)
        .unwrap_or(1.0)
}

fn owned_value<'a, T>(value: T) -> OwnedValue
where
    T: Into<Value<'a>>,
{
    OwnedValue::try_from(value.into()).expect("MPRIS value should be ownable")
}

struct MediaPlayer2Root;

// MPRIS interface methods: the `#[interface]` macro fixes these signatures
// (the `&self` receiver and the named parameters it deserializes incoming
// messages into), so several stubs legitimately ignore `self`/their args. We
// keep the parameters un-prefixed because the macro generates code that reads
// them by name (an `_`-prefix would trip `clippy::used_underscore_binding` in
// that generated code, which an impl-level `allow` cannot reach).
#[allow(clippy::unused_self, unused_variables)]
#[interface(name = "org.mpris.MediaPlayer2")]
impl MediaPlayer2Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_fullscreen(&self, fullscreen: bool) {}

    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> &str {
        IDENTITY
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        DESKTOP_ENTRY
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<&str> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<&str> {
        Vec::new()
    }
}

struct MediaPlayer2Player {
    callback: MediaControlCallback,
    state: SharedState,
}

// See the note on `MediaPlayer2Root`: the macro dictates these signatures, so
// some methods ignore `self` and their (un-prefixed) message parameters.
#[allow(clippy::unused_self, unused_variables)]
#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MediaPlayer2Player {
    fn next(&self) {
        self.command("next");
    }

    fn previous(&self) {
        self.command("previous");
    }

    fn pause(&self) {
        self.command("pause");
    }

    fn play_pause(&self) {
        self.command("toggle");
    }

    fn stop(&self) {
        self.command("stop");
    }

    fn play(&self) {
        self.command("play");
    }

    fn seek(&self, offset: i64) {}

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {}

    fn open_uri(&self, uri: &str) {}

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.state.snapshot().playback_status().to_string()
    }

    #[zbus(property)]
    fn loop_status(&self) -> &'static str {
        "None"
    }

    #[zbus(property)]
    fn set_loop_status(&self, loop_status: &str) {}

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, rate: f64) {}

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) {}

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.state.snapshot().metadata()
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        current_mpris_volume()
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        // Fire-and-forget: the applied value flows back through the sendspin
        // volume listener, which re-emits the `Volume` property.
        if let Err(e) = sendspin::set_volume_percent(mpris_volume_to_percent(volume)) {
            log::warn!("[MediaControls] Failed to set Linux MPRIS volume: {e}");
        }
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        self.state.snapshot().position_us()
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.state.snapshot().can_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.state.snapshot().can_previous
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.state.snapshot().can_play
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.state.snapshot().can_pause
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

impl MediaPlayer2Player {
    fn command(&self, command: &str) {
        (self.callback)(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stopped_without_track_has_stopped_status() {
        let state = MprisState::from_now_playing(&NowPlaying::default());
        assert_eq!(state.playback_status(), "Stopped");
        assert_eq!(state.position_us(), 0);
        // With no current track, MPRIS Metadata must be empty (no trackid).
        assert!(state.metadata().is_empty());
    }

    #[test]
    fn playing_maps_metadata_and_microseconds() {
        let np = NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            image_url: Some("https://example.test/cover.jpg".to_string()),
            duration: Some(123.4),
            elapsed: Some(5.5),
            can_next: true,
            can_previous: true,
            ..Default::default()
        };

        let state = MprisState::from_now_playing(&np);
        let metadata = state.metadata();

        assert_eq!(state.playback_status(), "Playing");
        assert_eq!(state.position_us(), 5_500_000);
        assert_eq!(state.length_us, Some(123_400_000));
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(metadata.contains_key("xesam:title"));
        assert!(metadata.contains_key("xesam:artist"));
        assert!(metadata.contains_key("xesam:album"));
        assert!(metadata.contains_key("mpris:artUrl"));
        assert!(metadata.contains_key("mpris:length"));
        assert!(state.can_next);
        assert!(state.can_previous);
    }

    #[test]
    fn trackid_is_stable_per_track_but_differs_across_tracks() {
        let a = MprisState::from_now_playing(&NowPlaying {
            track: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
        let a_dup = MprisState::from_now_playing(&NowPlaying {
            track: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
        let b = MprisState::from_now_playing(&NowPlaying {
            track: Some("Other".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });

        let id_a = track_id_path(a.track.as_deref(), a.artist.as_deref(), a.album.as_deref());
        let id_a_dup = track_id_path(
            a_dup.track.as_deref(),
            a_dup.artist.as_deref(),
            a_dup.album.as_deref(),
        );
        let id_b = track_id_path(b.track.as_deref(), b.artist.as_deref(), b.album.as_deref());

        assert_eq!(id_a, id_a_dup);
        assert_ne!(id_a, id_b);
        assert!(id_a.starts_with("/org/music_assistant/desktop/track_"));
    }
}
