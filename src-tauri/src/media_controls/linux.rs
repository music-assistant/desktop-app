//! Native Linux MPRIS media-controls backend over zbus.
//!
//! Registers `org.mpris.MediaPlayer2.io_music_assistant_companion.instance<PID>`
//! at the standard `/org/mpris/MediaPlayer2` path, sharing the playback-state
//! mapping in [`super::plan`] with the macOS backend.

use super::{plan, MediaControlCallback, PlaybackState};
use crate::now_playing::NowPlaying;
use crate::sendspin;
use parking_lot::Mutex;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::Instant;
use tokio::sync::mpsc::{self, UnboundedSender};
use zbus::fdo;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{connection, interface};

const BUS_NAME_BASE: &str = "org.mpris.MediaPlayer2.io_music_assistant_companion";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const IDENTITY: &str = "Music Assistant";

/// How far a freshly reported position may deviate from the extrapolated one
/// before it counts as a seek (and must be announced via the `Seeked` signal)
/// rather than normal playback progression. Upstream position reports jitter
/// by over a second around play/pause transitions, so the threshold must
/// comfortably exceed that; genuine seeks jump by far more.
const SEEK_JUMP_THRESHOLD_US: i64 = 2_000_000;

/// Basename (without `.desktop`) of the installed desktop file, per bundle:
/// the flatpak manifest installs `io.music_assistant.Companion.desktop`, while
/// tauri-bundler (deb/rpm/AppImage) names the file after `productName` —
/// literally `Music Assistant.desktop`, space included.
fn desktop_entry() -> &'static str {
    match option_env!("MUSIC_ASSISTANT_DISTRIBUTION") {
        Some("flatpak") => "io.music_assistant.Companion",
        _ => "Music Assistant",
    }
}
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

static SERVICE_TX: Mutex<Option<UnboundedSender<ServiceCommand>>> = Mutex::new(None);

/// Last volume observed or requested, as an MPRIS `Volume` value; `None`
/// until first seeded.
///
/// Written only by the sendspin volume listener, an optimistic `SetVolume`
/// write, and a one-time seed from the live sendspin snapshot. The getter
/// must never overwrite it from the live snapshot: zbus re-reads the getter
/// to build the automatic `PropertiesChanged` after a property set, and
/// sendspin applies volume asynchronously, so a getter-side overwrite would
/// re-emit the stale value (volume-slider snap-back). Never reset, not even
/// on disconnect: an unseeded getter falls back to full volume, which is
/// worse than briefly reporting the previous player's level.
static LAST_VOLUME: Mutex<Option<f64>> = Mutex::new(None);

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
                *SERVICE_TX.lock() = None;
                return;
            }
        };

        if let Err(e) = runtime.block_on(run_service(bus_name, callback, rx)) {
            log::error!("[MediaControls] Linux MPRIS service stopped: {e}");
        }

        // The receiver is gone (bus-name acquisition failed, no session bus,
        // …); drop the sender too so subsequent updates become cheap no-ops
        // instead of a per-tick send-error warning against a closed channel.
        *SERVICE_TX.lock() = None;
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

    // The last snapshot whose properties were emitted; lets updates emit only
    // the properties that actually changed instead of the full fixed set on
    // every progress tick.
    let mut last_emitted: Option<EmittedProperties> = None;

    // Async channel keeps the runtime cooperative between updates so inbound
    // method calls (Next/Play/…) are serviced even while idle.
    while let Some(command) = rx.recv().await {
        match command {
            ServiceCommand::Update(np) => {
                let now = Instant::now();
                let previous = shared.snapshot();
                shared.update(np);
                let current = shared.snapshot();

                // Properties (Metadata in particular) go out first: on a
                // track change clients reset their position when they see the
                // new trackid, so a Seeked sent before the Metadata change
                // would be applied to the old track and then discarded.
                emit_player_properties(&emitter, &player_iface, &current, &mut last_emitted).await;

                // Clients extrapolate Position from the last read value and
                // Rate; any jump inconsistent with that must be announced.
                if let Some(position) = seek_jump(&previous, &current, now) {
                    if let Err(e) = MediaPlayer2Player::seeked(&emitter, position).await {
                        log::warn!("[MediaControls] Failed to emit Linux MPRIS Seeked: {e}");
                    }
                }
            }
            ServiceCommand::Clear => {
                shared.clear();
                let current = shared.snapshot();
                emit_player_properties(&emitter, &player_iface, &current, &mut last_emitted).await;
            }
            ServiceCommand::VolumeChanged(volume) => {
                let volume = percent_to_mpris_volume(volume);
                *LAST_VOLUME.lock() = Some(volume);
                let changed = HashMap::from([("Volume", Value::from(volume))]);
                if emit_properties_changed(&emitter, &player_iface, changed).await {
                    if let Some(last) = last_emitted.as_mut() {
                        last.volume = volume;
                    }
                }
            }
        }
    }

    Ok(())
}

/// The property values included in the previous `PropertiesChanged` emission,
/// used to emit only deltas on subsequent updates.
struct EmittedProperties {
    state: MprisState,
    volume: f64,
}

/// The properties whose current values differ from the previously emitted
/// ones. Pure so the delta logic is unit-testable; `None` for `last` (first
/// emission) yields the full set.
///
/// `Position` is omitted on purpose: the spec says clients must track it via
/// Rate-based extrapolation plus the Seeked signal, never through
/// `PropertiesChanged`. Everything else on the interface is constant.
fn changed_properties(
    last: Option<&EmittedProperties>,
    snapshot: &MprisState,
    volume: f64,
) -> HashMap<&'static str, Value<'static>> {
    let mut changed: HashMap<&'static str, Value<'static>> = HashMap::new();

    if last.is_none_or(|l| l.state.playback_status != snapshot.playback_status) {
        changed.insert("PlaybackStatus", Value::from(snapshot.playback_status()));
    }
    if last.is_none_or(|l| !l.state.same_metadata(snapshot)) {
        changed.insert("Metadata", Value::from(snapshot.metadata()));
    }
    if last.is_none_or(|l| l.state.can_play != snapshot.can_play) {
        changed.insert("CanPlay", Value::from(snapshot.can_play));
    }
    if last.is_none_or(|l| l.state.can_pause != snapshot.can_pause) {
        changed.insert("CanPause", Value::from(snapshot.can_pause));
    }
    if last.is_none_or(|l| l.state.can_next != snapshot.can_next) {
        changed.insert("CanGoNext", Value::from(snapshot.can_next));
    }
    if last.is_none_or(|l| l.state.can_previous != snapshot.can_previous) {
        changed.insert("CanGoPrevious", Value::from(snapshot.can_previous));
    }
    // Bit-exact comparison on purpose: this deduplicates emissions of the
    // same stored value, not a numeric tolerance check.
    if last.is_none_or(|l| l.volume.to_bits() != volume.to_bits()) {
        changed.insert("Volume", Value::from(volume));
    }

    changed
}

async fn emit_player_properties(
    emitter: &SignalEmitter<'static>,
    interface: &InterfaceName<'static>,
    snapshot: &MprisState,
    last_emitted: &mut Option<EmittedProperties>,
) {
    let volume = current_mpris_volume();
    let changed = changed_properties(last_emitted.as_ref(), snapshot, volume);

    // Advance the ledger only after a successful emission (or when there was
    // nothing to say), so a transient D-Bus failure is retried on the next
    // update instead of being permanently suppressed for that value.
    if changed.is_empty() || emit_properties_changed(emitter, interface, changed).await {
        *last_emitted = Some(EmittedProperties {
            state: snapshot.clone(),
            volume,
        });
    }
}

/// Returns whether the emission succeeded.
async fn emit_properties_changed(
    emitter: &SignalEmitter<'static>,
    interface: &InterfaceName<'static>,
    changed: HashMap<&str, Value<'_>>,
) -> bool {
    match fdo::Properties::properties_changed(
        emitter,
        interface.clone(),
        changed,
        std::borrow::Cow::Borrowed(&[]),
    )
    .await
    {
        Ok(()) => true,
        Err(e) => {
            log::warn!("[MediaControls] Failed to emit Linux MPRIS property change: {e}");
            false
        }
    }
}

/// Decide whether the transition `previous` -> `current` constitutes a seek
/// that must be announced with the `Seeked` signal, and if so at what position.
///
/// Same track: any deviation beyond [`SEEK_JUMP_THRESHOLD_US`] from the
/// position clients extrapolate on their own. New track: only a start away
/// from 0 (clients reset their position on a Metadata/trackid change).
fn seek_jump(previous: &MprisState, current: &MprisState, now: Instant) -> Option<i64> {
    current.track.as_ref()?;

    let new_position = current.position_at(now);
    let same_track = previous.track.is_some()
        && previous.track == current.track
        && previous.artist == current.artist
        && previous.album == current.album;

    if same_track {
        let extrapolated = previous.position_at(now);
        let deviation = if previous.playback_status == current.playback_status {
            (new_position - extrapolated).abs()
        } else {
            // Across a play/pause transition the reported position lags the
            // extrapolation by the upstream update latency. Only call it a
            // seek when the jump is inconsistent with BOTH the extrapolated
            // and the frozen last-reported position — a plain pause/resume is
            // near one of them, a genuine seek is far from both.
            let frozen = previous.position_us;
            (new_position - extrapolated)
                .abs()
                .min((new_position - frozen).abs())
        };
        (deviation > SEEK_JUMP_THRESHOLD_US).then_some(new_position)
    } else {
        (new_position > SEEK_JUMP_THRESHOLD_US).then_some(new_position)
    }
}

#[derive(Clone, Default)]
struct SharedState(std::sync::Arc<Mutex<MprisState>>);

impl SharedState {
    fn snapshot(&self) -> MprisState {
        self.0.lock().clone()
    }

    fn update(&self, np: NowPlaying) {
        *self.0.lock() = MprisState::from_now_playing(&np, Instant::now());
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
    /// Position reported by the last update, valid as of `updated_at`.
    position_us: i64,
    /// When `position_us` was captured; `None` only for the default state.
    updated_at: Option<Instant>,
    can_play: bool,
    can_pause: bool,
    can_next: bool,
    can_previous: bool,
}

impl MprisState {
    fn from_now_playing(np: &NowPlaying, now: Instant) -> Self {
        // super::plan owns the mapping shared with the other backends,
        // including the state-independent CanPlay/CanPause semantics.
        let plan = plan(np);

        Self {
            playback_status: plan.state,
            track: plan.title,
            artist: plan.artist,
            album: plan.album,
            art_url: plan.image_url,
            length_us: plan.duration_secs.map(seconds_to_microseconds),
            position_us: plan.elapsed_secs.map_or(0, seconds_to_microseconds),
            updated_at: Some(now),
            can_play: plan.can_play,
            can_pause: plan.can_pause,
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

    /// Whether the Metadata-relevant fields match `other` (position/timing and
    /// capability fields are excluded).
    fn same_metadata(&self, other: &Self) -> bool {
        self.track == other.track
            && self.artist == other.artist
            && self.album == other.album
            && self.art_url == other.art_url
            && self.length_us == other.length_us
    }

    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let mut metadata = HashMap::new();

        // No current track => empty Metadata, per the MPRIS spec (a trackid is
        // only valid when a track is present, so none may be synthesized).
        if self.track.is_none() {
            return metadata;
        }

        // mpris:trackid must be stable per track and must not be "/". A
        // content hash differs across tracks but not across updates of one;
        // the upstream payload carries no queue-item identity, so an
        // identical track replayed back-to-back keeps the same id.
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
        if let Some(art_url) = self.art_url.as_deref().and_then(sanitize_art_url) {
            metadata.insert("mpris:artUrl".to_string(), owned_value(art_url));
        }
        if let Some(length_us) = self.length_us {
            metadata.insert("mpris:length".to_string(), owned_value(length_us));
        }

        metadata
    }

    /// Current playback position, extrapolated from the last reported value
    /// when playing (clients do the same, per the spec: position progresses
    /// with Rate between Seeked signals) and clamped to `[0, mpris:length]`.
    fn position_at(&self, now: Instant) -> i64 {
        let mut position = self.position_us;
        if self.playback_status == PlaybackState::Playing {
            if let Some(updated_at) = self.updated_at {
                let elapsed = now.saturating_duration_since(updated_at);
                position =
                    position.saturating_add(elapsed.as_micros().min(i64::MAX as u128) as i64);
            }
        }
        if let Some(length_us) = self.length_us {
            position = position.min(length_us);
        }
        position.max(0)
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

/// Validate an artwork URL for `mpris:artUrl`: `data:` URL support varies
/// across desktop environments and relative/bare paths are not valid URLs at
/// all, so only pass through the schemes that work everywhere.
fn sanitize_art_url(url: &str) -> Option<String> {
    let scheme_ok = ["http:", "https:", "file:"]
        .iter()
        .any(|scheme| url.len() > scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme));
    scheme_ok.then(|| url.to_string())
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

/// Current player volume as an MPRIS `Volume` value: the last value observed
/// via the sendspin listener or requested via `SetVolume`, seeded once from
/// the live sendspin snapshot. See [`LAST_VOLUME`] for why this getter never
/// overwrites an already-seeded value. Full volume when nothing is known yet.
fn current_mpris_volume() -> f64 {
    let mut last = LAST_VOLUME.lock();
    if let Some(volume) = *last {
        return volume;
    }
    match sendspin::get_volume_percent() {
        Ok(percent) => {
            let volume = percent_to_mpris_volume(percent);
            *last = Some(volume);
            volume
        }
        // Not seeded and no live value: report full volume but do NOT seed,
        // so the first real report can still take over.
        Err(_) => 1.0,
    }
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
    fn raise(&self) {
        crate::raise_main_window();
    }

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
    fn set_fullscreen(&self, fullscreen: bool) -> fdo::Result<()> {
        // CanSetFullscreen is false; per the spec clients may not call this,
        // and silently swallowing the write would leave them out of sync.
        Err(fdo::Error::NotSupported(
            "Fullscreen cannot be set".to_string(),
        ))
    }

    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
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
        desktop_entry()
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

    // CanSeek is false: per the spec, Seek and SetPosition then "have no
    // effect" — a silent no-op is the sanctioned behavior, unlike the
    // writable-property stubs below which must error instead.
    fn seek(&self, offset: i64) {}

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {}

    fn open_uri(&self, uri: &str) {}

    /// Position changed in a way that must not be extrapolated from Rate
    /// (i.e. a seek). Emitted by the service loop, never spontaneously here.
    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.state.snapshot().playback_status().to_string()
    }

    #[zbus(property)]
    fn loop_status(&self) -> &'static str {
        "None"
    }

    #[zbus(property)]
    fn set_loop_status(&self, loop_status: &str) -> fdo::Result<()> {
        // MPRIS has no CanLoop guard; erroring (instead of silently ignoring
        // the write) is the only way to tell clients the toggle didn't take.
        Err(fdo::Error::NotSupported(
            "Loop status cannot be changed".to_string(),
        ))
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, rate: f64) {
        // Spec: a client setting Rate to 0.0 must be treated as Pause; other
        // values outside [MinimumRate, MaximumRate] (both 1.0 here) should
        // simply not take effect.
        if rate == 0.0 {
            self.command("pause");
        }
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "Shuffle cannot be changed".to_string(),
        ))
    }

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
        // Optimistically record the requested value so the automatic
        // PropertiesChanged from this set carries it (instead of a stale
        // snapshot the applet would briefly snap back to); the applied value
        // then flows back through the sendspin volume listener, which
        // re-emits `Volume` with the authoritative number.
        *LAST_VOLUME.lock() = Some(sanitize_mpris_volume(volume));
        if let Err(e) = sendspin::set_volume_percent(mpris_volume_to_percent(volume)) {
            log::warn!("[MediaControls] Failed to set Linux MPRIS volume: {e}");
        }
    }

    // The MPRIS spec annotates Position with EmitsChangedSignal=false:
    // clients track it via Rate extrapolation and the Seeked signal, and we
    // never emit it in PropertiesChanged — advertise that in introspection.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.state.snapshot().position_at(Instant::now())
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
    use std::time::Duration;

    fn state_from(np: &NowPlaying) -> MprisState {
        MprisState::from_now_playing(np, Instant::now())
    }

    #[test]
    fn canonical_linux_identity_matches_packaging() {
        assert_eq!(
            BUS_NAME_BASE,
            "org.mpris.MediaPlayer2.io_music_assistant_companion"
        );
        let expected_desktop_entry = match option_env!("MUSIC_ASSISTANT_DISTRIBUTION") {
            Some("flatpak") => "io.music_assistant.Companion",
            _ => "Music Assistant",
        };
        assert_eq!(desktop_entry(), expected_desktop_entry);
    }

    #[test]
    fn stopped_without_track_has_stopped_status() {
        let state = state_from(&NowPlaying::default());
        assert_eq!(state.playback_status(), "Stopped");
        assert_eq!(state.position_at(Instant::now()), 0);
        // With no current track, MPRIS Metadata must be empty (no trackid).
        assert!(state.metadata().is_empty());
        // And no current track means nothing can be played or paused.
        assert!(!state.can_play);
        assert!(!state.can_pause);
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

        let now = Instant::now();
        let state = MprisState::from_now_playing(&np, now);
        let metadata = state.metadata();

        assert_eq!(state.playback_status(), "Playing");
        assert_eq!(state.position_at(now), 5_500_000);
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
    fn can_play_and_can_pause_are_state_independent() {
        // Spec: CanPlay/CanPause are intrinsic to the current track and must
        // not flip with is_playing, even though the upstream flags do.
        let playing = state_from(&NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            can_pause: true,
            ..Default::default()
        });
        assert!(playing.can_play, "CanPlay must stay true while playing");
        assert!(playing.can_pause);

        let paused = state_from(&NowPlaying {
            is_playing: false,
            track: Some("Song".to_string()),
            can_play: true,
            ..Default::default()
        });
        assert!(paused.can_play);
        assert!(paused.can_pause, "CanPause must stay true while paused");

        // Playing with both upstream flags unset: is_playing alone must keep
        // the pair true.
        let playing_no_flags = state_from(&NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            ..Default::default()
        });
        assert!(playing_no_flags.can_play);
        assert!(playing_no_flags.can_pause);
    }

    /// GNOME Shell removes the media card the moment `CanPlay` goes false and
    /// resets its position tracking when `Metadata` changes, so a pause<->play
    /// transition must change neither the capability pair nor the `Metadata`.
    #[test]
    fn paused_to_playing_preserves_track_identity_and_metadata() {
        let base = NowPlaying {
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
        let paused = state_from(&NowPlaying {
            is_playing: false,
            can_play: true,
            ..base.clone()
        });
        let playing = state_from(&NowPlaying {
            is_playing: true,
            can_pause: true,
            ..base
        });

        assert_eq!(paused.playback_status(), "Paused");
        assert_eq!(playing.playback_status(), "Playing");
        assert!(playing.can_play);
        assert!(paused.can_pause);
        assert_eq!(paused.metadata(), playing.metadata());
        assert!(playing.metadata().contains_key("mpris:trackid"));
    }

    #[test]
    fn position_extrapolates_while_playing_and_clamps_to_length() {
        let now = Instant::now();
        let mut state = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                track: Some("Song".to_string()),
                duration: Some(100.0),
                elapsed: Some(98.0),
                ..Default::default()
            },
            now,
        );

        // 1 second into playback: extrapolated forward.
        let later = now + Duration::from_secs(1);
        assert_eq!(state.position_at(later), 99_000_000);

        // 5 seconds in: clamped to mpris:length.
        let much_later = now + Duration::from_secs(5);
        assert_eq!(state.position_at(much_later), 100_000_000);

        // Paused: frozen at the reported value.
        state.playback_status = PlaybackState::Paused;
        assert_eq!(state.position_at(much_later), 98_000_000);
    }

    #[test]
    fn seek_jump_detected_for_same_track_deviation() {
        let now = Instant::now();
        let np = NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            duration: Some(300.0),
            elapsed: Some(10.0),
            ..Default::default()
        };
        let previous = MprisState::from_now_playing(&np, now);

        // Progress tick consistent with extrapolation: no Seeked.
        let ticked = MprisState::from_now_playing(
            &NowPlaying {
                elapsed: Some(10.2),
                ..np.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &ticked, now), None);

        // Jump well beyond the threshold: Seeked at the new position.
        let sought = MprisState::from_now_playing(
            &NowPlaying {
                elapsed: Some(120.0),
                ..np.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &sought, now), Some(120_000_000));
    }

    #[test]
    fn seek_jump_on_track_change_only_when_starting_off_zero() {
        let now = Instant::now();
        let previous = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                track: Some("Song".to_string()),
                elapsed: Some(100.0),
                ..Default::default()
            },
            now,
        );

        // New track starting at 0: clients reset on the Metadata change.
        let next_track = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                track: Some("Other".to_string()),
                elapsed: Some(0.0),
                ..Default::default()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &next_track, now), None);

        // New track resuming mid-way: must be announced.
        let resumed = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                track: Some("Other".to_string()),
                elapsed: Some(42.0),
                ..Default::default()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &resumed, now), Some(42_000_000));

        // No track at all: nothing to announce.
        let cleared = MprisState::default();
        assert_eq!(seek_jump(&previous, &cleared, now), None);
    }

    #[test]
    fn seek_jump_uses_extrapolation_for_stale_previous_state() {
        // The previous state was captured 10s ago while playing; clients have
        // extrapolated 10s forward on their own since then.
        let start = Instant::now();
        let np = NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            duration: Some(300.0),
            elapsed: Some(10.0),
            ..Default::default()
        };
        let previous = MprisState::from_now_playing(&np, start);
        let now = start + Duration::from_secs(10);

        // Fresh report consistent with extrapolation (10 + 10): no Seeked.
        let ticked = MprisState::from_now_playing(
            &NowPlaying {
                elapsed: Some(20.0),
                ..np.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &ticked, now), None);

        // Fresh report far off the extrapolation: Seeked.
        let sought = MprisState::from_now_playing(
            &NowPlaying {
                elapsed: Some(40.0),
                ..np.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &sought, now), Some(40_000_000));
    }

    #[test]
    fn pause_transition_does_not_emit_spurious_seeked() {
        // Last Playing update 3s ago at 10s; the user pauses and the frontend
        // reports the position as of the pause (~10-13s), which lags the 13s
        // extrapolation. That is not a seek.
        let start = Instant::now();
        let playing = NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            duration: Some(300.0),
            elapsed: Some(10.0),
            ..Default::default()
        };
        let previous = MprisState::from_now_playing(&playing, start);
        let now = start + Duration::from_secs(3);

        let paused_at_report = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: false,
                elapsed: Some(10.2),
                ..playing.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&previous, &paused_at_report, now), None);

        // A genuine seek-while-pausing (far from both the frozen and the
        // extrapolated view) must still be announced.
        let paused_after_seek = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: false,
                elapsed: Some(120.0),
                ..playing.clone()
            },
            now,
        );
        assert_eq!(
            seek_jump(&previous, &paused_after_seek, now),
            Some(120_000_000)
        );

        // Resume whose first report runs slightly ahead of the frozen pause
        // position (normal upstream jitter): tolerated, not a seek.
        let resumed_with_jitter = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                elapsed: Some(11.2),
                ..playing.clone()
            },
            now,
        );
        let frozen = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: false,
                elapsed: Some(10.0),
                ..playing.clone()
            },
            now,
        );
        assert_eq!(seek_jump(&frozen, &resumed_with_jitter, now), None);
    }

    #[test]
    fn zero_duration_is_no_length_not_zero_length() {
        let now = Instant::now();
        let state = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: true,
                track: Some("Radio".to_string()),
                duration: Some(0.0),
                elapsed: Some(45.0),
                ..Default::default()
            },
            now,
        );
        assert_eq!(state.length_us, None, "0 duration is live/unknown");
        // Position must not be clamped to a bogus zero-length track.
        assert_eq!(state.position_at(now), 45_000_000);
        assert!(!state.metadata().contains_key("mpris:length"));
    }

    #[test]
    fn changed_properties_diffs_against_last_emission() {
        let now = Instant::now();
        let np = NowPlaying {
            is_playing: true,
            track: Some("Song".to_string()),
            duration: Some(300.0),
            elapsed: Some(10.0),
            can_next: true,
            ..Default::default()
        };
        let snapshot = MprisState::from_now_playing(&np, now);

        // First emission: everything goes out.
        let first = changed_properties(None, &snapshot, 0.5);
        for key in [
            "PlaybackStatus",
            "Metadata",
            "CanPlay",
            "CanPause",
            "CanGoNext",
            "CanGoPrevious",
            "Volume",
        ] {
            assert!(first.contains_key(key), "first emission must include {key}");
        }

        let last = EmittedProperties {
            state: snapshot.clone(),
            volume: 0.5,
        };

        // Progress tick: same metadata/status/caps/volume => nothing at all
        // (Position is deliberately never part of PropertiesChanged).
        let tick = MprisState::from_now_playing(
            &NowPlaying {
                elapsed: Some(11.0),
                ..np.clone()
            },
            now,
        );
        assert!(changed_properties(Some(&last), &tick, 0.5).is_empty());

        // Pause: only PlaybackStatus (caps are state-independent). The
        // frontend flips its capability flags with state, so a real pause
        // arrives with can_play set instead of is_playing.
        let paused = MprisState::from_now_playing(
            &NowPlaying {
                is_playing: false,
                can_play: true,
                ..np.clone()
            },
            now,
        );
        let diff = changed_properties(Some(&last), &paused, 0.5);
        assert!(diff.contains_key("PlaybackStatus"));
        assert!(!diff.contains_key("Metadata"));
        assert!(!diff.contains_key("CanPlay"));

        // Volume delta is bit-exact.
        let diff = changed_properties(Some(&last), &snapshot, 0.51);
        assert_eq!(diff.len(), 1);
        assert!(diff.contains_key("Volume"));
    }

    #[test]
    fn art_url_schemes_are_sanitized() {
        assert_eq!(
            sanitize_art_url("https://host/cover.jpg").as_deref(),
            Some("https://host/cover.jpg")
        );
        assert_eq!(
            sanitize_art_url("HTTP://host/cover.jpg").as_deref(),
            Some("HTTP://host/cover.jpg")
        );
        assert_eq!(
            sanitize_art_url("file:///tmp/cover.jpg").as_deref(),
            Some("file:///tmp/cover.jpg")
        );
        assert_eq!(sanitize_art_url("data:image/png;base64,AAAA"), None);
        assert_eq!(sanitize_art_url("/relative/path.jpg"), None);
        assert_eq!(sanitize_art_url(""), None);

        let state = state_from(&NowPlaying {
            track: Some("Song".to_string()),
            image_url: Some("data:image/png;base64,AAAA".to_string()),
            ..Default::default()
        });
        assert!(!state.metadata().contains_key("mpris:artUrl"));
    }

    #[test]
    fn trackid_is_stable_per_track_but_differs_across_tracks() {
        let a = state_from(&NowPlaying {
            track: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
        let a_dup = state_from(&NowPlaying {
            track: Some("Song".to_string()),
            artist: Some("Artist".to_string()),
            ..Default::default()
        });
        let b = state_from(&NowPlaying {
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
