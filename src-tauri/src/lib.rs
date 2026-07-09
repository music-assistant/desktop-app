use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

mod discord_rpc;
mod i18n;
#[cfg(target_os = "linux")]
mod linux_theme;
mod logging;
mod mdns_discovery;
mod media_controls;
mod now_playing;
mod sendspin;
mod settings;

use mdns_discovery::DiscoveredServer;
use now_playing::NowPlaying;
use tauri_plugin_autostart::MacosLauncher;

static SERVICES_STARTER: Once = Once::new();

// Global app handle for media controls callback
static APP_HANDLE: Mutex<Option<tauri::AppHandle>> = Mutex::new(None);

// Global tray icon reference for updating tooltip
static TRAY_ICON: Mutex<Option<TrayIcon>> = Mutex::new(None);

// Global menu item reference for updating now-playing text
static NOW_PLAYING_MENU_ITEM: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>> = Mutex::new(None);

// Global menu item references for playback controls
static PLAY_PAUSE_MENU_ITEM: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>> = Mutex::new(None);
static PREV_TRACK_MENU_ITEM: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>> = Mutex::new(None);
static NEXT_TRACK_MENU_ITEM: Mutex<Option<tauri::menu::MenuItem<tauri::Wry>>> = Mutex::new(None);

// Global menu item reference for the Discord Rich Presence checkbox
static DISCORD_RPC_MENU_ITEM: Mutex<Option<tauri::menu::CheckMenuItem<tauri::Wry>>> =
    Mutex::new(None);

// Discord RPC enabled state
pub static DISCORD_RPC_ENABLED: AtomicBool = AtomicBool::new(true);

#[derive(Clone)]
struct CurrentMaSession {
    server_base_url: String,
    auth_token: String,
}

static CURRENT_MA_SESSION: Mutex<Option<CurrentMaSession>> = Mutex::new(None);

// Companion readiness tracking
// Timestamp (unix ms) when server connection started, 0 if not connecting
static SERVER_CONNECT_TIME: AtomicU64 = AtomicU64::new(0);
// Whether the frontend has reported companion ready
static COMPANION_READY: AtomicBool = AtomicBool::new(false);
// Timeout in seconds before warning that companion feature detection did not complete
const COMPANION_READY_TIMEOUT_SECS: u64 = 30;

/// Check if running in the desktop companion app.
/// Frontend can use this to enable companion-specific features
/// and disable the built-in Sendspin player
#[tauri::command]
fn is_companion_app() -> bool {
    true
}

// Keep old name for backwards compatibility
#[tauri::command]
fn is_desktop_app() -> bool {
    true
}

/// Get the app version
///
/// Sourced from the Tauri config (`tauri.conf.json`) via `package_info`, which the
/// release workflow bumps from the git tag. `CARGO_PKG_VERSION` is deliberately avoided
/// because `Cargo.toml` is not bumped on release.
#[tauri::command]
fn get_app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
fn get_i18n_bundle() -> i18n::I18nBundle {
    i18n::bundle()
}

/// Called by launcher when navigating to a server
/// Starts the companion readiness timeout check
#[tauri::command]
fn server_connecting(app: tauri::AppHandle, url: String) {
    log::info!("[Launcher] Connecting to server: {url}");

    // Reset state
    COMPANION_READY.store(false, Ordering::SeqCst);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    SERVER_CONNECT_TIME.store(now, Ordering::SeqCst);

    // Start timeout check in background
    thread::spawn(move || {
        // Wait for timeout
        thread::sleep(std::time::Duration::from_secs(COMPANION_READY_TIMEOUT_SECS));

        // Check if companion became ready
        if !COMPANION_READY.load(Ordering::SeqCst) {
            // Check if we're still waiting for the same connection
            let connect_time = SERVER_CONNECT_TIME.load(Ordering::SeqCst);
            if connect_time > 0 {
                // Show native warning dialog for companion feature detection timeout
                app.dialog()
                    .message(i18n::tr("desktop.dialog.companion_server_message"))
                    .title(i18n::tr("desktop.dialog.companion_server_title"))
                    .kind(MessageDialogKind::Warning)
                    .blocking_show();
            }
        }
    });
}

/// Called by the launcher when a connection attempt fails preflight (unreachable
/// server, wrong scheme, or timeout) so the failure is captured in the file log.
#[tauri::command]
fn server_connect_failed(url: String, error: String) {
    log::error!("[Launcher] Connection to {url} failed: {error}");
}

/// Reachability preflight before the launcher webview navigates to `url`: a bare
/// `window.location.href` to a dead host hangs `WKWebView` forever. Done natively
/// rather than a webview `fetch` because `WebKit` blocks private/Tailscale HTTP hosts
/// from the launcher origin
#[tauri::command]
async fn check_server_reachable(url: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(5)))
                .build(),
        );
        // Any HTTP response means the host answered; only transport/timeout/TLS
        // failures count as unreachable.
        match agent.head(&url).call() {
            Ok(_) | Err(ureq::Error::StatusCode(_)) => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Called by frontend to signal companion integration is ready
#[tauri::command]
fn companion_ready() {
    COMPANION_READY.store(true, Ordering::SeqCst);
    SERVER_CONNECT_TIME.store(0, Ordering::SeqCst);
}

/// Navigate back to the server selection screen (logout)
/// This clears the last server setting and recreates the window
#[tauri::command]
async fn navigate_to_launcher(app: tauri::AppHandle) -> Result<(), String> {
    // Reset companion ready state
    COMPANION_READY.store(false, Ordering::SeqCst);
    SERVER_CONNECT_TIME.store(0, Ordering::SeqCst);
    if let Ok(mut session) = CURRENT_MA_SESSION.lock() {
        *session = None;
    }

    // Clear last server settings so user sees the server selection
    settings::set_string_setting("last_server_url", None)
        .map_err(|e| format!("Failed to clear last_server_url: {}", e))?;
    settings::set_string_setting("last_server_name", None)
        .map_err(|e| format!("Failed to clear last_server_name: {}", e))?;

    // Stop Sendspin if running
    sendspin::stop().await;

    // Find the current window (could be "main" or "launcher" depending on how we got here)
    let old_window = app
        .get_webview_window("main")
        .or_else(|| app.get_webview_window("launcher"));

    // Choose a name that doesn't conflict with the current window
    let new_name = if app.get_webview_window("main").is_some() {
        "launcher"
    } else {
        "main"
    };

    // Create new window with launcher URL
    let new_window = apply_window_defaults(tauri::WebviewWindowBuilder::new(
        &app,
        new_name,
        tauri::WebviewUrl::App("index.html".into()),
    ))
    .inner_size(1200.0, 800.0)
    .min_inner_size(600.0, 400.0)
    .build()
    .map_err(|e| format!("Failed to create window: {}", e))?;

    // Show the new window
    let _ = new_window.show();
    let _ = new_window.set_focus();

    // Now close the old window
    if let Some(old) = old_window {
        let _ = old.destroy();
    }

    Ok(())
}

/// Get current now-playing information
#[tauri::command]
fn get_now_playing() -> NowPlaying {
    now_playing::get_now_playing()
}

/// Update now-playing information (called from frontend when track changes)
#[tauri::command]
fn update_now_playing(now_playing: NowPlaying) {
    let sendspin_player_id = sendspin::get_player_id();
    let current_now_playing = now_playing::get_now_playing();

    // Filter out frontend updates when Sendspin is active
    if current_now_playing.player_id.as_deref() == sendspin_player_id.as_deref()
        && current_now_playing.is_playing
    {
        log::debug!("[Tauri] Ignoring now-playing update from frontend because Sendspin is active");
        return;
    }

    now_playing::update_now_playing(now_playing);
}

/// Initialize desktop integrations (Discord RPC, tray updates, media controls)
/// Call this after connecting to the MA server
#[tauri::command]
fn start_desktop_services(app: tauri::AppHandle) {
    start_services(app);
}

// Keep old command names for backwards compatibility
#[tauri::command]
fn start_discord_rpc(app: tauri::AppHandle, _websocket_url: String, _auth_token: Option<String>) {
    start_services(app);
}

#[tauri::command]
fn start_rpc(app: tauri::AppHandle, _websocket: String) {
    start_services(app);
}

/// Start all background services (tray tooltip updates, Discord RPC, media controls)
fn start_services(app_handle: tauri::AppHandle) {
    // Store app handle for media controls callback
    {
        let mut handle = APP_HANDLE.lock().unwrap();
        *handle = Some(app_handle);
    }

    SERVICES_STARTER.call_once(move || {
        log::info!("[App] Starting desktop services (media controls, Discord RPC)");
        // Register callback to update tray now-playing state and media controls when playback changes
        now_playing::on_now_playing_change(Arc::new(|np| {
            update_tray_now_playing(np);
            media_controls::update(np);
        }));

        // Get HWND for Windows media controls
        #[cfg(target_os = "windows")]
        let hwnd = {
            if let Some(ref app) = *APP_HANDLE.lock().unwrap() {
                if let Some(window) = app.get_webview_window("main")
                    .or_else(|| app.get_webview_window("launcher")) {
                    // Get the HWND from the window
                    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                    window.window_handle().ok().and_then(|handle| {
                        if let RawWindowHandle::Win32(win32_handle) = handle.as_ref() {
                            Some(win32_handle.hwnd.get() as *mut std::ffi::c_void)
                        } else {
                            None
                        }
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };

        #[cfg(not(target_os = "windows"))]
        let hwnd = None;

        // Dispatcher onto the NSApplication main run loop. Used by the macOS
        // media-controls backend (objc2 calls must run there); ignored by the
        // native backends on other platforms.
        let dispatch: media_controls::MainThreadDispatch = {
            let app = APP_HANDLE.lock().unwrap().clone();
            Arc::new(move |f| {
                if let Some(ref app) = app {
                    let _ = app.run_on_main_thread(f);
                }
            })
        };

        // Initialize media controls with callback for control events
        media_controls::init(Arc::new(|command| {
            // Route media control events (from OS Now Playing / media keys) to frontend
            log::debug!("[MediaControls] OS media command: {command}");
            if let Some(ref app) = *APP_HANDLE.lock().unwrap() {
                if let Some(window) = app.get_webview_window("main")
                    .or_else(|| app.get_webview_window("launcher")) {
                    let cmd = if command == "toggle" {
                        // For toggle, check current state
                        let np = now_playing::get_now_playing();
                        if np.is_playing { "pause" } else { "play" }
                    } else {
                        command
                    };
                    let _ = window.eval(format!(
                        "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('{cmd}');",
                    ));
                }
            }
        }), hwnd, dispatch);

        // Start Discord RPC in a separate thread
        thread::spawn(|| {
            discord_rpc::start_rpc();
        });
    });
}

/// Snapshot of the main window's position captured on hide, restored on the
/// next show. Best-effort: the WM may re-place on map.
static HIDDEN_WINDOW_POSITION: Mutex<Option<tauri::PhysicalPosition<i32>>> = Mutex::new(None);

/// Record a window position to restore on the next `show()`.
fn stash_window_position(position: tauri::PhysicalPosition<i32>) {
    if let Ok(mut guard) = HIDDEN_WINDOW_POSITION.lock() {
        *guard = Some(position);
    }
}

/// Take (and clear) the stashed hidden-window position, if any.
fn take_hidden_window_position() -> Option<tauri::PhysicalPosition<i32>> {
    HIDDEN_WINDOW_POSITION
        .lock()
        .ok()
        .and_then(|mut guard| guard.take())
}

pub fn set_tray_visible(visible: bool) {
    if let Ok(tray_guard) = TRAY_ICON.try_lock() {
        if let Some(ref tray) = *tray_guard {
            let _ = tray.set_visible(visible);
        }
    }
}

pub(crate) fn refresh_tray_now_playing() {
    update_tray_now_playing(&now_playing::get_now_playing());
}

/// Keep the tray "Discord Rich Presence" checkbox in sync with the setting.
pub(crate) fn set_discord_rpc_tray_checked(checked: bool) {
    if let Ok(item_guard) = DISCORD_RPC_MENU_ITEM.lock() {
        if let Some(ref item) = *item_guard {
            let _ = item.set_checked(checked);
        }
    }
}

/// Ask an open settings window to re-read the persisted settings so its
/// toggles stay in sync with changes made elsewhere (e.g. the tray menu).
fn refresh_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.eval("window.__MA_RELOAD_SETTINGS__ && window.__MA_RELOAD_SETTINGS__();");
    }
}

const TRAY_TITLE_MAX_CHARS: usize = 40;

/// Clamp a string to `TRAY_TITLE_MAX_CHARS` characters on a UTF-8 boundary,
/// appending an ellipsis when truncated.
fn truncate_tray_title(s: &str) -> String {
    if s.chars().count() <= TRAY_TITLE_MAX_CHARS {
        return s.to_string();
    }
    let take = TRAY_TITLE_MAX_CHARS.saturating_sub(1);
    let truncated: String = s.chars().take(take).collect();
    format!("{truncated}…")
}

/// Update the tray title, tooltip, and menu item with now-playing info
/// Spawns on a separate thread to avoid blocking the caller, since
/// tray operations on macOS dispatch synchronously to the main thread
fn update_tray_now_playing(np: &NowPlaying) {
    let np = np.clone();

    // Spawn tray update on a separate thread to never block the caller
    thread::spawn(move || {
        let tooltip = now_playing::format_now_playing_with_player(&np);
        let title = if settings::get_settings().show_tray_now_playing && np.is_playing {
            Some(truncate_tray_title(&now_playing::format_now_playing(&np)))
        } else {
            None
        };

        // Update tray metadata - use try_lock to avoid blocking
        if let Ok(tray_guard) = TRAY_ICON.try_lock() {
            if let Some(ref tray) = *tray_guard {
                // TODO: Remove unwrapping need when https://github.com/tauri-apps/tray-icon/issues/322 gets fixed
                let _ = tray.set_title(Some(title.as_deref().unwrap_or("")));
                let _ = tray.set_tooltip(Some(&tooltip));
            }
        }

        let now_playing_text = now_playing::format_now_playing(&np);
        let menu_text = format!("♪ {now_playing_text}");

        if let Ok(item_guard) = NOW_PLAYING_MENU_ITEM.try_lock() {
            if let Some(ref item) = *item_guard {
                let _ = item.set_text(&menu_text);
            }
        }

        // Update playback control enabled states
        let has_player = np.player_id.is_some();

        // Play/Pause - show appropriate text and enable if action is available
        if let Ok(item_guard) = PLAY_PAUSE_MENU_ITEM.try_lock() {
            if let Some(ref item) = *item_guard {
                let can_toggle = np.can_play || np.can_pause;
                let _ = item.set_enabled(has_player && can_toggle);
                let text = if np.is_playing {
                    i18n::tr("desktop.tray.pause")
                } else {
                    i18n::tr("desktop.tray.play")
                };
                let _ = item.set_text(text);
            }
        }

        // Previous track
        if let Ok(item_guard) = PREV_TRACK_MENU_ITEM.try_lock() {
            if let Some(ref item) = *item_guard {
                let _ = item.set_enabled(has_player && np.can_previous);
            }
        }

        // Next track
        if let Ok(item_guard) = NEXT_TRACK_MENU_ITEM.try_lock() {
            if let Some(ref item) = *item_guard {
                let _ = item.set_enabled(has_player && np.can_next);
            }
        }
    });
}

/// Discover Music Assistant servers on the local network via mDNS
/// Returns a list of discovered servers
#[tauri::command]
async fn discover_servers(timeout_secs: Option<u64>) -> Result<Vec<DiscoveredServer>, String> {
    let timeout = timeout_secs.unwrap_or(3);
    tokio::task::spawn_blocking(move || mdns_discovery::discover_servers(timeout))
        .await
        .map_err(|e| format!("Discovery task failed: {e}"))?
}

/// Get all settings (with actual runtime state for some fields)
#[tauri::command]
fn get_settings() -> settings::Settings {
    let mut s = settings::get_settings();
    // Override with actual runtime state
    s.discord_rpc_enabled = DISCORD_RPC_ENABLED.load(std::sync::atomic::Ordering::SeqCst);
    s.sendspin_enabled = sendspin::is_enabled();
    s
}

/// Set a single setting
#[tauri::command]
async fn set_setting(app: tauri::AppHandle, key: String, value: bool) -> Result<(), String> {
    settings::set_setting(app.clone(), &key, value)?;

    if key == "sendspin_enabled" && value {
        reconfigure_sendspin_from_current_session(app).await?;
    }

    Ok(())
}

async fn reconfigure_sendspin_from_current_session(app: tauri::AppHandle) -> Result<(), String> {
    let session = CURRENT_MA_SESSION
        .lock()
        .ok()
        .and_then(|session| session.clone());

    let Some(session) = session else {
        log::warn!(
            "[Sendspin] Native player enabled, but no current MA session is available to configure Sendspin"
        );
        return Ok(());
    };

    log::info!("[Sendspin] Reconfiguring native player from current MA session");
    match configure_sendspin_for_session(app, session.server_base_url, session.auth_token).await {
        Ok(Some(player_id)) => {
            log::info!("[Sendspin] Native player re-enabled as {}", player_id);
            Ok(())
        }
        Ok(None) => {
            log::warn!("[Sendspin] Native player re-enable requested, but Sendspin is disabled in settings");
            Ok(())
        }
        Err(e) => {
            log::error!("[Sendspin] Failed to re-enable native player: {}", e);
            Err(e)
        }
    }
}

/// Set a string setting
#[tauri::command]
fn set_string_setting(key: String, value: Option<String>) -> Result<(), String> {
    settings::set_string_setting(&key, value)
}

/// Set an integer setting
#[tauri::command]
fn set_int_setting(key: String, value: i32) -> Result<(), String> {
    settings::set_int_setting(&key, value)
}

// ============ Sendspin Commands ============

/// List available audio output devices
#[tauri::command]
fn list_audio_devices() -> Result<Vec<sendspin::devices::AudioDevice>, String> {
    sendspin::devices::list_devices()
}

/// Stop the Sendspin client
#[tauri::command]
async fn stop_sendspin() {
    sendspin::stop().await;
}

/// Restart the Sendspin client
#[tauri::command]
async fn restart_sendspin() -> Result<(), String> {
    sendspin::restart().await;
    Ok(())
}

/// Get Sendspin connection status
#[tauri::command]
fn get_sendspin_status() -> sendspin::ConnectionStatus {
    sendspin::get_status()
}

/// Send a playback command to Sendspin
#[tauri::command]
fn sendspin_command(command: String) -> Result<(), String> {
    sendspin::send_command(&command)
}

/// Get the Sendspin player ID (for frontend "this device" badge)
#[tauri::command]
fn get_sendspin_player_id() -> Option<String> {
    sendspin::get_player_id()
}

/// Configure and optionally start the Sendspin client with server URL from frontend.
/// This is called by the frontend when it connects to the MA server.
#[tauri::command]
async fn configure_sendspin(
    app: tauri::AppHandle,
    server_base_url: String,
    auth_token: String,
) -> Result<Option<String>, String> {
    remember_current_ma_session(server_base_url.clone(), auth_token.clone());
    configure_sendspin_for_session(app, server_base_url, auth_token).await
}

fn remember_current_ma_session(server_base_url: String, auth_token: String) {
    if let Ok(mut session) = CURRENT_MA_SESSION.lock() {
        *session = Some(CurrentMaSession {
            server_base_url,
            auth_token,
        });
    } else {
        log::warn!(
            "[Sendspin] Failed to store current MA session for later native-player re-enable"
        );
    }
}

async fn configure_sendspin_for_session(
    app: tauri::AppHandle,
    server_base_url: String,
    auth_token: String,
) -> Result<Option<String>, String> {
    let loaded_settings = settings::get_settings();

    let sendspin_url = build_sendspin_ws_url(&server_base_url);

    // Save the URL to settings
    let _ = settings::set_string_setting("sendspin_server_url", Some(sendspin_url.clone()));

    log::info!(
        "[Sendspin] Configuring client (enabled={}, url={})",
        loaded_settings.sendspin_enabled,
        sendspin_url
    );

    // If sendspin is enabled, start the client
    if loaded_settings.sendspin_enabled {
        // Use hostname as fallback if player name is empty
        let player_name = if loaded_settings.sendspin_player_name.is_empty() {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .map_or_else(
                    || i18n::tr("desktop.app.companion_name"),
                    |name| strip_hostname_suffix(&name),
                )
        } else {
            loaded_settings.sendspin_player_name.clone()
        };

        // Get or generate a persistent player ID
        let player_id = if let Some(id) = loaded_settings.sendspin_player_id.clone() {
            id
        } else {
            let new_id = format!("ma_companion_{}", uuid::Uuid::new_v4());
            // Save the generated ID so it persists across restarts
            let _ = settings::set_string_setting("sendspin_player_id", Some(new_id.clone()));
            new_id
        };

        let config = sendspin::SendspinConfig {
            player_id,
            player_name,
            server_url: sendspin_url,
            audio_device_id: loaded_settings.audio_device_id.clone(),
            sync_delay_ms: loaded_settings.sync_delay_ms,
            auth_token,
            app_version: app.package_info().version.to_string(),
        };

        return sendspin::start(config).await.map(Some);
    }

    Ok(None)
}

/// Build a WebSocket URL for Sendspin from an HTTP(S) server base URL
fn build_sendspin_ws_url(server_base_url: &str) -> String {
    let trimmed_url = server_base_url.trim_end_matches('/');
    let lower_url = trimmed_url.to_ascii_lowercase();

    let (ws_scheme, url_without_scheme) = if lower_url.starts_with("https://") {
        ("wss", &trimmed_url["https://".len()..])
    } else if lower_url.starts_with("http://") {
        ("ws", &trimmed_url["http://".len()..])
    } else {
        ("ws", trimmed_url)
    };

    format!("{}://{}/sendspin", ws_scheme, url_without_scheme)
}

/// Strip common local network suffixes from a hostname
fn strip_hostname_suffix(name: &str) -> String {
    name.trim_end_matches(".local")
        .trim_end_matches(".lan")
        .trim_end_matches(".home")
        .trim_end_matches(".localdomain")
        .to_string()
}

/// Open or focus the companion app's settings window.
fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        // The window may have been hidden (close-to-tray) while settings
        // changed elsewhere (e.g. the tray menu); re-read them before showing
        refresh_settings_window(app);
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        let _ = tauri::WebviewWindowBuilder::new(
            app,
            "settings",
            tauri::WebviewUrl::App("settings.html".into()),
        )
        .title(i18n::tr("desktop.app.settings_title"))
        .inner_size(600.0, 700.0)
        .resizable(true)
        .build();
    }
}

/// Apply the shared configuration every MA-frontend content window must carry.
/// Callers add per-window specifics (size, min size, zoom hotkeys) after this.
fn apply_window_defaults<R: tauri::Runtime, M: tauri::Manager<R>>(
    mut builder: tauri::WebviewWindowBuilder<'_, R, M>,
) -> tauri::WebviewWindowBuilder<'_, R, M> {
    builder = builder
        .title(i18n::tr("desktop.app.name"))
        .resizable(true)
        .initialization_script(include_str!("../resources/clipboard-polyfill.js"));
    builder
}

#[cfg(target_os = "linux")]
fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

#[cfg(target_os = "linux")]
fn webkit_has_legacy_renderer() -> bool {
    tauri::webview_version()
        .ok()
        .as_deref()
        .and_then(parse_major_minor)
        .is_some_and(|version| version < (2, 46))
}

#[cfg(target_os = "linux")]
fn running_gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .is_ok_and(|desktops| desktops.split(':').any(|d| d.eq_ignore_ascii_case("GNOME")))
}

pub fn run() {
    // Newer versions of WebKitGTK crash if this is set to 1 on a machine with a
    // real GPU. Older versions crash if it isn't. We can delete this and the
    // associate webkit_has_legacy_renderer in 2027, as everyone should have
    // upgraded off of the affected version by then. Hopefully.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() && webkit_has_legacy_renderer()
    {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    // The forced-XWayland fallback fixes GNOME/Wayland tray and
    // window-management quirks. Limit it to GNOME, the only desktop it is known
    // to be needed on; XWayland can degrade compositors that manage window
    // geometry themselves, such as the tiling ones, so default the rest to
    // native Wayland. Honor an explicit GDK_BACKEND override either way.
    #[cfg(target_os = "linux")]
    if std::env::var_os("GDK_BACKEND").is_none() && running_gnome() {
        std::env::set_var("GDK_BACKEND", "x11");

        // WebKit's DMABUF renderer paints a blank window under XWayland on
        // current WebKitGTK, so pin it off on the branch where we force XWayland.
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    let context = tauri::generate_context!();
    let mut builder = tauri::Builder::default();

    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Some(window) = app
            .get_webview_window("main")
            .or_else(|| app.get_webview_window("launcher"))
        {
            let _ = window.unminimize();
            let _ = window.show();
            if let Some(pos) = take_hidden_window_position() {
                let _ = window.set_position(pos);
            }
            let _ = window.set_focus();
        }
    }));

    builder = builder.plugin(tauri_plugin_window_state::Builder::new().build());

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::AppleScript,
            None,))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            is_companion_app,
            is_desktop_app,
            get_app_version,
            get_i18n_bundle,
            server_connecting,
            server_connect_failed,
            check_server_reachable,
            companion_ready,
            navigate_to_launcher,
            get_now_playing,
            update_now_playing,
            start_desktop_services,
            start_discord_rpc,
            start_rpc,
            discover_servers,
            get_settings,
            set_setting,
            set_string_setting,
            set_int_setting,
            // Sendspin commands
            list_audio_devices,
            stop_sendspin,
            restart_sendspin,
            get_sendspin_status,
            sendspin_command,
            get_sendspin_player_id,
            configure_sendspin
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if settings::get_settings().close_to_tray {
                    if let Ok(pos) = window.outer_position() {
                        stash_window_position(pos);
                    }
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .setup(|app| {
            // Load settings first so the persisted debug-logging flag is known
            // before the logger is installed
            let loaded_settings = settings::load_settings();

            // Always log to <app_log_dir>/logs.log so the "Open log file" tray
            // command has a stable target; mirror to stdout in dev builds.
            // Verbosity is governed by the live logging toggles.
            let log_verbosity = logging::verbosity_from_settings(
                loaded_settings.debug_logging,
                loaded_settings.trace_logging,
            );
            app.handle().plugin(logging::build_plugin(log_verbosity))?;
            // Installing the plugin resets the global max level, so re-apply the
            // persisted verbosity now.
            logging::apply_after_install(log_verbosity);
            i18n::init(app.handle());

            log::info!(
                "[App] Music Assistant Companion v{} starting (debug logging: {})",
                app.package_info().version,
                if loaded_settings.debug_logging {
                    "on"
                } else {
                    "off"
                }
            );

            // GNOME & co. express dark mode via the xdg-desktop-portal
            // color-scheme setting, which GTK3/WebKitGTK don't read on their
            // own; track it and apply it as the window theme.
            #[cfg(target_os = "linux")]
            linux_theme::init(app.handle().clone());

            // Create main window (companion bridge + clipboard polyfill applied
            // via apply_window_defaults; runs on every page load, including the
            // remote MA frontend loaded via window.location.href).
            let _main_window = apply_window_defaults(tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            ))
            .inner_size(800.0, 600.0)
            .zoom_hotkeys_enabled(true)
            .build()?;

            // Update runtime state flags from settings
            DISCORD_RPC_ENABLED.store(loaded_settings.discord_rpc_enabled, Ordering::SeqCst);
            sendspin::set_enabled(loaded_settings.sendspin_enabled);

            // "Start minimized": launch to the tray; Show / single-instance restore it.
            if loaded_settings.start_minimized {
                if let Some(main_window) = app.get_webview_window("main") {
                    let _ = main_window.hide();
                }
            }

            // Build tray menu
            let now_playing_item = MenuItemBuilder::with_id(
                "now_playing",
                i18n::tr("desktop.tray.not_playing"),
            )
            .build(app)?;
            let separator1 = PredefinedMenuItem::separator(app)?;
            // Playback controls - start disabled until we have an active player
            let play_pause = MenuItemBuilder::with_id("play_pause", i18n::tr("desktop.tray.play"))
                .enabled(false)
                .build(app)?;
            let prev_track = MenuItemBuilder::with_id("prev_track", i18n::tr("desktop.tray.previous"))
                .enabled(false)
                .build(app)?;
            let next_track = MenuItemBuilder::with_id("next_track", i18n::tr("desktop.tray.next"))
                .enabled(false)
                .build(app)?;
            let separator_playback = PredefinedMenuItem::separator(app)?;
            let show = MenuItemBuilder::with_id("show", i18n::tr("common.actions.show")).build(app)?;
            let hide = MenuItemBuilder::with_id("hide", i18n::tr("common.actions.hide")).build(app)?;
            let switch_server =
                MenuItemBuilder::with_id("switch_server", i18n::tr("desktop.tray.switch_server"))
                    .build(app)?;
            let separator2 = PredefinedMenuItem::separator(app)?;
            let discord_rpc_item = CheckMenuItemBuilder::with_id(
                "discord_rpc",
                i18n::tr("desktop.tray.discord_rich_presence"),
            )
            .checked(DISCORD_RPC_ENABLED.load(Ordering::SeqCst))
            .build(app)?;
            let separator3 = PredefinedMenuItem::separator(app)?;
            let settings =
                MenuItemBuilder::with_id("settings", i18n::tr("desktop.tray.settings")).build(app)?;
            let update = MenuItemBuilder::with_id(
                "update",
                i18n::tr("desktop.tray.check_for_updates"),
            )
            .build(app)?;
            let relaunch =
                MenuItemBuilder::with_id("relaunch", i18n::tr("desktop.tray.relaunch")).build(app)?;
            let open_log = MenuItemBuilder::with_id(
                "open_log",
                i18n::tr("desktop.tray.open_log_file"),
            )
            .build(app)?;
            let separator4 = PredefinedMenuItem::separator(app)?;
            let quit = MenuItemBuilder::with_id("quit", i18n::tr("common.actions.quit")).build(app)?;

            // Store menu items for later updates
            if let Ok(mut item_guard) = NOW_PLAYING_MENU_ITEM.lock() {
                *item_guard = Some(now_playing_item.clone());
            }
            if let Ok(mut item_guard) = PLAY_PAUSE_MENU_ITEM.lock() {
                *item_guard = Some(play_pause.clone());
            }
            if let Ok(mut item_guard) = PREV_TRACK_MENU_ITEM.lock() {
                *item_guard = Some(prev_track.clone());
            }
            if let Ok(mut item_guard) = NEXT_TRACK_MENU_ITEM.lock() {
                *item_guard = Some(next_track.clone());
            }
            if let Ok(mut item_guard) = DISCORD_RPC_MENU_ITEM.lock() {
                *item_guard = Some(discord_rpc_item.clone());
            }

            let menu = MenuBuilder::new(app)
                .items(&[
                    &now_playing_item,
                    &separator1,
                    &play_pause,
                    &prev_track,
                    &next_track,
                    &separator_playback,
                    &show,
                    &hide,
                    &switch_server,
                    &separator2,
                    &discord_rpc_item,
                    &separator3,
                    &settings,
                    &update,
                    &relaunch,
                    &open_log,
                    &separator4,
                    &quit,
                ])
                .build()?;

            // Load dedicated tray icon (without padding, for better menu bar visibility)
            let tray_icon = {
                let png_bytes = include_bytes!("../icons/tray-icon@2x.png");
                let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
                let mut reader = decoder.read_info().expect("Failed to read PNG info");
                let mut buf = vec![0; reader.output_buffer_size()];
                let info = reader.next_frame(&mut buf).expect("Failed to decode PNG");
                let rgba = buf[..info.buffer_size()].to_vec();
                tauri::image::Image::new_owned(rgba, info.width, info.height)
            };

            let tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("Music Assistant")
                .icon(tray_icon)
                .on_menu_event(move |app, event| {
                  log::debug!("[Tray] Menu item clicked: {}", event.id().as_ref());
                  match event.id().as_ref() {
                    "quit" => {
                        app.exit(0);
                    }
                    "hide" => {
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            if let Ok(pos) = window.outer_position() {
                                stash_window_position(pos);
                            }
                            let _ = window.hide();
                        }
                    }
                    "show" => {
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            // Undo both hide states: an app-level hide
                            // (unmapped) and a window-manager minimize.
                            let _ = window.unminimize();
                            let _ = window.show();
                            // Restore prior position; the WM may still re-place on map.
                            if let Some(pos) = take_hidden_window_position() {
                                let _ = window.set_position(pos);
                            }
                            let _ = window.set_focus();
                        }
                    }
                    "switch_server" => {
                        // Reset companion ready state
                        COMPANION_READY.store(false, Ordering::SeqCst);
                        SERVER_CONNECT_TIME.store(0, Ordering::SeqCst);
                        if let Ok(mut session) = CURRENT_MA_SESSION.lock() {
                            *session = None;
                        }

                        // Clear last server so we don't auto-connect again
                        let _ = settings::set_string_setting("last_server_url", None);
                        let _ = settings::set_string_setting("last_server_name", None);

                        // Stop Sendspin client
                        tauri::async_runtime::spawn(async {
                            sendspin::stop().await;
                        });

                        // Find the current window (could be "main" or "launcher")
                        let old_window = app.get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"));

                        // Choose a name that doesn't conflict
                        let new_name = if app.get_webview_window("main").is_some() {
                            "launcher"
                        } else {
                            "main"
                        };

                        // Create new window with launcher URL
                        if let Ok(new_window) = apply_window_defaults(tauri::WebviewWindowBuilder::new(
                            app,
                            new_name,
                            tauri::WebviewUrl::App("index.html".into()),
                        ))
                        .inner_size(1200.0, 800.0)
                        .min_inner_size(600.0, 400.0)
                        .build() {
                            let _ = new_window.show();
                            let _ = new_window.set_focus();

                            // Now close the old window
                            if let Some(old) = old_window {
                                let _ = old.destroy();
                            }
                        }
                    }
                    "play_pause" => {
                        // Call frontend function to control active player
                        let np = now_playing::get_now_playing();
                        let cmd = if np.is_playing { "pause" } else { "play" };
                        if let Some(window) = app.get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher")) {
                            let _ = window.eval(format!(
                                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('{cmd}');",
                            ));
                        }
                    }
                    "prev_track" => {
                        if let Some(window) = app.get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher")) {
                            let _ = window.eval(
                                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('previous');"
                            );
                        }
                    }
                    "next_track" => {
                        if let Some(window) = app.get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher")) {
                            let _ = window.eval(
                                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('next');"
                            );
                        }
                    }
                    "discord_rpc" => {
                        // Toggle Discord RPC; persist through the settings so
                        // the change survives restarts, the activity is
                        // cleared/restored and the checkbox state stays
                        // consistent
                        let new_state = !DISCORD_RPC_ENABLED.load(Ordering::SeqCst);
                        if let Err(e) =
                            settings::set_setting(app.clone(), "discord_rpc_enabled", new_state)
                        {
                            log::error!("[Tray] Failed to save Discord RPC setting: {e}");
                        }
                        // Sync an already-open settings window
                        refresh_settings_window(app);
                    }
                    "settings" => {
                        open_settings_window(app);
                    }
                    "relaunch" => {
                        tauri::process::restart(&app.env());
                    }
                    "open_log" => match app.path().app_log_dir() {
                        Ok(log_dir) => {
                            let log_file =
                                log_dir.join(format!("{}.log", logging::LOG_FILE_STEM));
                            if let Err(e) =
                                app.opener().open_path(log_file.to_string_lossy(), None::<&str>)
                            {
                                log::error!("[Tray] Failed to open log file: {}", e);
                            }
                        }
                        Err(e) => log::error!("[Tray] Could not resolve log directory: {}", e),
                    },
                    "update" => {
                        let handle = app.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = handle.updater().unwrap().check().await;
                        });
                    }
                    "now_playing" => {
                        // Click on now-playing opens the app
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    _ => (),
                  }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Store tray reference for tooltip updates
            if let Ok(mut tray_guard) = TRAY_ICON.lock() {
                *tray_guard = Some(tray);
            }

            // Apply initial tray visibility from settings
            if !loaded_settings.show_tray_icon {
                set_tray_visible(false);
            }

            // Add "Preferences..." (CmdOrCtrl+,) to the default menu bar.
            // macOS: app submenu (first submenu), after About
            // Windows/Linux: Edit submenu
            if let Some(menu) = app.menu() {
                let items = menu.items()?;

                #[cfg(target_os = "macos")]
                let target = items.into_iter().find_map(|item| match item {
                    tauri::menu::MenuItemKind::Submenu(s) => Some(s),
                    _ => None,
                });

                #[cfg(not(target_os = "macos"))]
                let target = items.into_iter().find_map(|item| match item {
                    tauri::menu::MenuItemKind::Submenu(s)
                        if s.text().is_ok_and(|t| t == "Edit") =>
                    {
                        Some(s)
                    }
                    _ => None,
                });

                if let Some(submenu) = target {
                    let separator = PredefinedMenuItem::separator(app)?;
                    let prefs = MenuItemBuilder::with_id(
                        "app_preferences",
                        i18n::tr("desktop.tray.preferences"),
                    )
                    .accelerator("CmdOrCtrl+,")
                        .build(app)?;

                    #[cfg(target_os = "macos")]
                    {
                        submenu.insert(&separator, 1)?;
                        submenu.insert(&prefs, 2)?;
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        submenu.append(&separator)?;
                        submenu.append(&prefs)?;
                    }
                }
            }
            app.on_menu_event(move |app, event| {
                if event.id().as_ref() == "app_preferences" {
                    open_settings_window(app);
                }
            });

            Ok(())
        })
        .build(context)
        .expect("Error while building Music Assistant companion")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows, .. } = event {
                if !has_visible_windows {
                    if let Some(window) = app
                        .get_webview_window("main")
                        .or_else(|| app.get_webview_window("launcher"))
                    {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app, event);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sendspin_ws_url_https() {
        assert_eq!(
            build_sendspin_ws_url("https://192.168.1.47:8095"),
            "wss://192.168.1.47:8095/sendspin"
        );
    }

    #[test]
    fn test_build_sendspin_ws_url_http() {
        assert_eq!(
            build_sendspin_ws_url("http://192.168.1.47:8095"),
            "ws://192.168.1.47:8095/sendspin"
        );
    }

    #[test]
    fn test_build_sendspin_ws_url_with_trailing_slash() {
        assert_eq!(
            build_sendspin_ws_url("http://192.168.1.47:8095/"),
            "ws://192.168.1.47:8095/sendspin"
        );
    }

    #[test]
    fn test_build_sendspin_ws_url_scheme_is_case_insensitive() {
        assert_eq!(
            build_sendspin_ws_url("HTTPS://server.example.com"),
            "wss://server.example.com/sendspin"
        );
    }

    #[test]
    fn tray_title_short_string_is_unchanged() {
        assert_eq!(
            truncate_tray_title("Coldplay - Yellow"),
            "Coldplay - Yellow"
        );
    }

    #[test]
    fn tray_title_long_string_is_clamped_with_ellipsis() {
        let long = "Coldplay - Champion of the World (Live at NPR's Tiny Desk)";
        let out = truncate_tray_title(long);
        assert_eq!(out.chars().count(), TRAY_TITLE_MAX_CHARS);
        assert!(out.ends_with('…'));
        // Prefix preserved up to the ellipsis.
        assert!(out.starts_with("Coldplay - Champion of the Worl"));
    }

    #[test]
    fn tray_title_clamps_on_utf8_boundary() {
        // Each emoji is 4 bytes / 1 char; clamping must not split a codepoint.
        let wide = "🎵".repeat(TRAY_TITLE_MAX_CHARS + 10);
        let out = truncate_tray_title(&wide);
        assert_eq!(out.chars().count(), TRAY_TITLE_MAX_CHARS);
        assert!(out.ends_with('…'));
    }
}
