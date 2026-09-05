use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex, RwLock};

/// Windows power-management integration for active playback.
///
/// `SetThreadExecutionState` is thread-scoped, so playback state changes are
/// forwarded to a dedicated worker that owns the execution-state assertion.
#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
mod power_management {
    use super::{get_now_playing, on_now_playing_change};
    use std::sync::{mpsc, Arc, Once};
    use std::thread;
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED, EXECUTION_STATE,
    };

    static START: Once = Once::new();

    const ACTIVE_STATE: EXECUTION_STATE = EXECUTION_STATE(ES_CONTINUOUS.0 | ES_SYSTEM_REQUIRED.0);
    const INACTIVE_STATE: EXECUTION_STATE = EXECUTION_STATE(ES_CONTINUOUS.0);

    pub(super) fn init() {
        START.call_once(|| {
            let (tx, rx) = mpsc::channel::<()>();
            thread::spawn(move || run_worker(rx));

            let callback_tx = tx.clone();
            on_now_playing_change(Arc::new(move |_now_playing| {
                let _ = callback_tx.send(());
            }));

            // Playback may have started before desktop services were initialized.
            let _ = tx.send(());
        });
    }

    fn run_worker(rx: mpsc::Receiver<()>) {
        let mut active = false;

        while rx.recv().is_ok() {
            // Coalesce metadata/progress updates; only the latest playback state
            // matters to the power assertion.
            while rx.try_recv().is_ok() {}
            let should_prevent_sleep = get_now_playing().is_playing;

            if should_prevent_sleep == active {
                continue;
            }

            let state = if should_prevent_sleep {
                ACTIVE_STATE
            } else {
                INACTIVE_STATE
            };
            let previous_state = unsafe { SetThreadExecutionState(state) };
            if previous_state == EXECUTION_STATE(0) {
                log::warn!(
                    "[PowerManagement] Failed to {} Windows sleep prevention",
                    if should_prevent_sleep {
                        "enable"
                    } else {
                        "disable"
                    }
                );
                continue;
            }

            active = should_prevent_sleep;
            log::debug!(
                "[PowerManagement] Windows sleep prevention {}",
                if active { "enabled" } else { "disabled" }
            );
        }

        // Release the assertion if the worker is ever shut down cleanly.
        if active {
            let _ = unsafe { SetThreadExecutionState(INACTIVE_STATE) };
        }
    }
}

/// Linux idle / sleep inhibition for active playback.
///
/// Prefers the XDG desktop portal's `org.freedesktop.portal.Inhibit` interface
/// with the `SUSPEND | IDLE` flags. Unlike a bare `org.freedesktop.ScreenSaver`
/// inhibition — which on KDE Plasma only maps to the `ChangeScreenSettings`
/// power-management policy, blocking screen-blanking and the lock but never
/// automatic suspend — the portal backends translate those flags into a full
/// set of inhibitions on every desktop (on KDE, `InterruptSession` +
/// `ChangeScreenSettings`). The portal is also always reachable from inside a
/// Flatpak sandbox without any extra permission.
///
/// Where the portal is unavailable (older or minimal non-Flatpak sessions) it
/// falls back to `org.freedesktop.ScreenSaver`, which still keeps the screen on
/// and unlocked but, on KDE, does not stop idle auto-suspend.
///
/// A deliberate, user-initiated suspend is always still allowed.
///
/// Like the Windows backend, the inhibition handle is owned by a dedicated
/// worker thread and playback-state changes are forwarded to it over a
/// channel; only the latest state matters, so bursts of metadata / progress
/// updates are coalesced.
#[cfg(target_os = "linux")]
mod power_management {
    use super::{get_now_playing, on_now_playing_change};
    use std::collections::HashMap;
    use std::sync::{mpsc, Arc, Once};
    use std::thread;
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedObjectPath, Value};

    static START: Once = Once::new();

    /// Name shown to the user by the desktop's "applications currently
    /// preventing sleep" UI.
    const APP_NAME: &str = "Music Assistant";
    const REASON: &str = "Playing media";

    /// XDG desktop portal — reachable unchanged from inside a Flatpak sandbox.
    const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
    const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
    const PORTAL_INHIBIT_INTERFACE: &str = "org.freedesktop.portal.Inhibit";
    const PORTAL_REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";

    /// `SUSPEND` (4) | `IDLE` (8) from the portal's inhibit flags — together
    /// they cover idle screen-blanking, the screen lock, and idle auto-suspend.
    const PORTAL_FLAGS: u32 = 4 | 8;

    /// Well-known bus name and interface name (identical) of the screensaver
    /// service used as a fallback.
    const SCREENSAVER_NAME: &str = "org.freedesktop.ScreenSaver";

    /// `org.freedesktop.ScreenSaver` is registered at one of these object paths
    /// depending on the desktop (KDE historically only served `/ScreenSaver`;
    /// the freedesktop-namespaced path is the modern one and what GNOME uses).
    /// Probe them in order and keep the first that answers.
    const SCREENSAVER_PATHS: [&str; 2] = ["/org/freedesktop/ScreenSaver", "/ScreenSaver"];

    /// A currently-held inhibition, tagged by the mechanism that produced it so
    /// it can be released the same way.
    enum Inhibition {
        /// Portal request object, released by calling `Close` on it.
        Portal(OwnedObjectPath),
        /// `org.freedesktop.ScreenSaver` cookie, released via `UnInhibit`.
        ScreenSaver(u32),
    }

    /// The inhibition mechanism resolved for this session.
    enum Backend {
        Portal(Proxy<'static>),
        ScreenSaver(Proxy<'static>),
    }

    impl Backend {
        fn label(&self) -> &'static str {
            match self {
                Backend::Portal(_) => "xdg-desktop-portal Inhibit (suspend + idle)",
                Backend::ScreenSaver(_) => {
                    "org.freedesktop.ScreenSaver (screen only; no auto-suspend on KDE)"
                }
            }
        }

        /// Take an inhibition. Called on each transition into playback.
        fn engage(&self) -> zbus::Result<Inhibition> {
            match self {
                Backend::Portal(proxy) => {
                    let mut options: HashMap<&str, Value> = HashMap::new();
                    options.insert("reason", Value::from(REASON));
                    let handle = proxy
                        .call::<_, _, OwnedObjectPath>("Inhibit", &("", PORTAL_FLAGS, options))?;
                    Ok(Inhibition::Portal(handle))
                }
                Backend::ScreenSaver(proxy) => {
                    let cookie = proxy.call::<_, _, u32>("Inhibit", &(APP_NAME, REASON))?;
                    Ok(Inhibition::ScreenSaver(cookie))
                }
            }
        }

        /// Release a previously-taken inhibition. The worker only ever pairs a
        /// backend with an inhibition of its own kind.
        fn release(&self, connection: &Connection, inhibition: Inhibition) -> zbus::Result<()> {
            match inhibition {
                Inhibition::Portal(handle) => {
                    let request = Proxy::new(
                        connection,
                        PORTAL_NAME,
                        handle.as_str(),
                        PORTAL_REQUEST_INTERFACE,
                    )?;
                    request.call::<_, _, ()>("Close", &())
                }
                Inhibition::ScreenSaver(cookie) => {
                    let Backend::ScreenSaver(proxy) = self else {
                        return Ok(());
                    };
                    proxy.call::<_, _, ()>("UnInhibit", &(cookie,))
                }
            }
        }
    }

    pub(super) fn init() {
        START.call_once(|| {
            let (tx, rx) = mpsc::channel::<()>();
            thread::spawn(move || run_worker(rx));

            let callback_tx = tx.clone();
            on_now_playing_change(Arc::new(move |_now_playing| {
                let _ = callback_tx.send(());
            }));

            // Playback may have started before desktop services were initialized.
            let _ = tx.send(());
        });
    }

    /// Resolve the best available inhibition mechanism, preferring the portal.
    /// Returns `None` when nothing is reachable (headless session, a desktop
    /// without either interface, no session bus).
    fn resolve_backend(connection: &Connection) -> Option<Backend> {
        // The portal: probe by reading the `version` property, which confirms
        // the interface is there without taking (and dropping) a real
        // inhibition.
        match Proxy::new(
            connection,
            PORTAL_NAME,
            PORTAL_PATH,
            PORTAL_INHIBIT_INTERFACE,
        ) {
            Ok(proxy) => match proxy.get_property::<u32>("version") {
                Ok(_) => return Some(Backend::Portal(proxy)),
                Err(e) => {
                    log::debug!("[PowerManagement] xdg-desktop-portal Inhibit unavailable: {e}");
                }
            },
            Err(e) => log::debug!("[PowerManagement] Cannot reach the desktop portal: {e}"),
        }

        // Fallback: org.freedesktop.ScreenSaver. Probe each candidate path by
        // taking an inhibition and releasing it straight away.
        for path in SCREENSAVER_PATHS {
            let Ok(proxy) = Proxy::new(connection, SCREENSAVER_NAME, path, SCREENSAVER_NAME) else {
                continue;
            };
            let backend = Backend::ScreenSaver(proxy);
            match backend.engage() {
                Ok(inhibition) => {
                    let _ = backend.release(connection, inhibition);
                    log::debug!("[PowerManagement] Using ScreenSaver inhibitor at {path}");
                    return Some(backend);
                }
                Err(e) => log::debug!("[PowerManagement] ScreenSaver at {path} unavailable: {e}"),
            }
        }
        None
    }

    fn run_worker(rx: mpsc::Receiver<()>) {
        let connection = match Connection::session() {
            Ok(connection) => connection,
            Err(e) => {
                log::info!("[PowerManagement] No session bus for sleep inhibition: {e}");
                return;
            }
        };

        let Some(backend) = resolve_backend(&connection) else {
            log::info!("[PowerManagement] No idle/suspend inhibitor available; playback will not keep this system awake");
            return;
        };
        log::debug!(
            "[PowerManagement] Sleep inhibition backend: {}",
            backend.label()
        );

        let mut inhibition: Option<Inhibition> = None;

        while rx.recv().is_ok() {
            // Coalesce metadata / progress updates; only the latest playback
            // state matters to the inhibition.
            while rx.try_recv().is_ok() {}
            let should_inhibit = get_now_playing().is_playing;

            if should_inhibit == inhibition.is_some() {
                continue;
            }

            if should_inhibit {
                match backend.engage() {
                    Ok(held) => {
                        inhibition = Some(held);
                        log::debug!("[PowerManagement] Linux sleep inhibition enabled");
                    }
                    Err(e) => log::warn!("[PowerManagement] Failed to inhibit idle sleep: {e}"),
                }
            } else if let Some(held) = inhibition.take() {
                if let Err(e) = backend.release(&connection, held) {
                    log::warn!("[PowerManagement] Failed to release sleep inhibition: {e}");
                }
                log::debug!("[PowerManagement] Linux sleep inhibition disabled");
            }
        }

        // Release the inhibition if the worker is ever shut down cleanly.
        if let Some(held) = inhibition.take() {
            let _ = backend.release(&connection, held);
        }
    }
}

/// Start the platform-specific playback power-management integration.
pub fn init_power_management() {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    power_management::init();
}

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

fn valid_seconds(value: Option<f64>) -> Option<f64> {
    value.filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
}

fn sanitize_now_playing(mut now_playing: NowPlaying) -> NowPlaying {
    now_playing.duration = valid_seconds(now_playing.duration);
    now_playing.elapsed = valid_seconds(now_playing.elapsed);
    now_playing
}

/// Update the now-playing state (called from frontend via Tauri command)
pub fn update_now_playing(now_playing: NowPlaying) {
    let now_playing = sanitize_now_playing(now_playing);

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
pub fn format_now_playing(np: &NowPlaying) -> String {
    if !np.is_playing {
        return crate::i18n::tr("desktop.tray.not_playing")
            .trim_start_matches('♪')
            .trim()
            .to_string();
    }

    match (&np.artist, &np.track) {
        (Some(artist), Some(track)) => format!("{artist} - {track}"),
        (None, Some(track)) => track.clone(),
        _ => "Playing".to_string(),
    }
}

/// Format now-playing info with player name
pub fn format_now_playing_with_player(np: &NowPlaying) -> String {
    if np.is_playing {
        let track_info = match (&np.artist, &np.track) {
            (Some(artist), Some(track)) => format!("{} - {}", artist, track),
            (None, Some(track)) => track.clone(),
            _ => crate::i18n::tr("desktop.discord.unknown_track"),
        };

        match &np.player_name {
            Some(name) => format!("{}\n{}", track_info, name),
            None => track_info,
        }
    } else {
        match &np.player_name {
            Some(name) => format!(
                "{} - {}",
                name,
                crate::i18n::tr("desktop.tray.not_playing")
                    .trim_start_matches('♪')
                    .trim()
            ),
            None => crate::i18n::tr("desktop.app.name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_sanitize_now_playing_removes_invalid_timing_values() {
        let sanitized = sanitize_now_playing(NowPlaying {
            duration: Some(f64::NAN),
            elapsed: Some(f64::INFINITY),
            ..Default::default()
        });
        assert_eq!(sanitized.duration, None);
        assert_eq!(sanitized.elapsed, None);

        let sanitized = sanitize_now_playing(NowPlaying {
            duration: Some(-1.0),
            elapsed: Some(-0.25),
            ..Default::default()
        });
        assert_eq!(sanitized.duration, None);
        assert_eq!(sanitized.elapsed, None);
    }

    #[test]
    fn test_sanitize_now_playing_keeps_valid_timing_values() {
        let sanitized = sanitize_now_playing(NowPlaying {
            duration: Some(180.0),
            elapsed: Some(0.0),
            ..Default::default()
        });

        assert_eq!(sanitized.duration, Some(180.0));
        assert_eq!(sanitized.elapsed, Some(0.0));
    }

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
