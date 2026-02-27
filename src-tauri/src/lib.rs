use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_updater::UpdaterExt;

mod discord_rpc;
mod mdns_discovery;
mod media_controls;
mod now_playing;
mod sendspin;
mod settings;

use mdns_discovery::DiscoveredServer;
use now_playing::NowPlaying;

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

// Discord RPC enabled state
pub static DISCORD_RPC_ENABLED: AtomicBool = AtomicBool::new(true);

// Companion readiness tracking
// Timestamp (unix ms) when server connection started, 0 if not connecting
static SERVER_CONNECT_TIME: AtomicU64 = AtomicU64::new(0);
// Whether the frontend has reported companion ready
static COMPANION_READY: AtomicBool = AtomicBool::new(false);
// Timeout in seconds before showing outdated server warning
const COMPANION_READY_TIMEOUT_SECS: u64 = 30;

/// Check if running in a companion app (desktop, mobile, etc.)
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
#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Called by launcher when navigating to a server
/// Starts the companion readiness timeout check
#[tauri::command]
fn server_connecting(app: tauri::AppHandle) {
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
                // Show native warning dialog
                app.dialog()
                    .message("The Music Assistant server you connected to may be running an older version \
                              that doesn't support all companion app features.\n\n\
                              Please update your Music Assistant server for the best experience.")
                    .title("Outdated Server")
                    .kind(MessageDialogKind::Warning)
                    .blocking_show();
            }
        }
    });
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
    let new_window = tauri::WebviewWindowBuilder::new(
        &app,
        new_name,
        tauri::WebviewUrl::App("index.html".into()),
    )
    .title("Music Assistant")
    .inner_size(1200.0, 800.0)
    .min_inner_size(600.0, 400.0)
    .resizable(true)
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
        // Register callback to update tray tooltip and media controls when now-playing changes
        now_playing::on_now_playing_change(Arc::new(|np| {
            update_tray_tooltip(np);
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

        // Initialize media controls with callback for control events
        media_controls::init(Arc::new(|command| {
            // Route media control events to frontend
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
        }), hwnd);

        // Start Discord RPC in a separate thread
        thread::spawn(|| {
            discord_rpc::start_rpc();
        });
    });
}

/// Update the tray tooltip and menu item with now-playing info
/// Spawns on a separate thread to avoid blocking the caller, since
/// tray operations on macOS dispatch synchronously to the main thread
fn update_tray_tooltip(np: &NowPlaying) {
    let np = np.clone();

    // Spawn tray update on a separate thread to never block the caller
    thread::spawn(move || {
        let tooltip = now_playing::format_now_playing_with_player(&np);

        // Update tooltip - use try_lock to avoid blocking
        if let Ok(tray_guard) = TRAY_ICON.try_lock() {
            if let Some(ref tray) = *tray_guard {
                let _ = tray.set_tooltip(Some(&tooltip));
            }
        }

        // Update menu item text
        let menu_text = if np.is_playing {
            let track = np.track.as_deref().unwrap_or("Unknown");
            let artist = np.artist.as_deref().unwrap_or("Unknown");
            format!("♪ {} - {}", artist, track)
        } else {
            "♪ Not Playing".to_string()
        };

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
                    "⏸ Pause"
                } else {
                    "▶ Play"
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
fn discover_servers(timeout_secs: Option<u64>) -> Result<Vec<DiscoveredServer>, String> {
    let timeout = timeout_secs.unwrap_or(3);
    mdns_discovery::discover_servers(timeout)
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
fn set_setting(key: String, value: bool) -> Result<(), String> {
    settings::set_setting(&key, value)
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

/// Configure and optionally start the Sendspin client with server URL from frontend
/// This is called by the frontend when it connects to the MA server
#[tauri::command]
async fn configure_sendspin(
    server_base_url: String,
    auth_token: String,
) -> Result<Option<String>, String> {
    let loaded_settings = settings::get_settings();

    // Build Sendspin WebSocket URL from the MA server base URL
    let ws_scheme = if server_base_url.starts_with("https") {
        "wss"
    } else {
        "ws"
    };
    let url_without_scheme = server_base_url
        .replace("https://", "")
        .replace("http://", "");
    let sendspin_url = format!("{}://{}/sendspin", ws_scheme, url_without_scheme);

    // Save the URL to settings
    let _ = settings::set_string_setting("sendspin_server_url", Some(sendspin_url.clone()));

    // If sendspin is enabled, start the client
    if loaded_settings.sendspin_enabled {
        // Use hostname as fallback if player name is empty
        let player_name = if loaded_settings.sendspin_player_name.is_empty() {
            hostname::get()
                .ok()
                .and_then(|h| h.into_string().ok())
                .map_or_else(
                    || "Music Assistant Companion".to_string(),
                    |name| {
                        // Strip common suffixes like .local, .lan, .home
                        name.trim_end_matches(".local")
                            .trim_end_matches(".lan")
                            .trim_end_matches(".home")
                            .trim_end_matches(".localdomain")
                            .to_string()
                    },
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
        };

        return sendspin::start(config).await.map(Some);
    }

    Ok(None)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let context = tauri::generate_context!();
    let mut builder = tauri::Builder::default();

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;

            if let Some(window) = app
                .get_webview_window("main")
                .or_else(|| app.get_webview_window("launcher"))
            {
                let _ = window.set_focus();
                let _ = window.show();
            }
        }));
    }

    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_window_state::Builder::new().build());
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            is_companion_app,
            is_desktop_app,
            get_app_version,
            server_connecting,
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
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            // Load settings - Sendspin connection will be started by frontend via configure_sendspin
            // because we need the auth token which the frontend has after authentication
            let loaded_settings = settings::load_settings();

            // Update runtime state flags from settings
            DISCORD_RPC_ENABLED.store(loaded_settings.discord_rpc_enabled, Ordering::SeqCst);
            sendspin::set_enabled(loaded_settings.sendspin_enabled);

            // Build tray menu
            let now_playing_item =
                MenuItemBuilder::with_id("now_playing", "♪ Not Playing").build(app)?;
            let separator1 = PredefinedMenuItem::separator(app)?;
            // Playback controls - start disabled until we have an active player
            let play_pause = MenuItemBuilder::with_id("play_pause", "▶ Play")
                .enabled(false)
                .build(app)?;
            let prev_track = MenuItemBuilder::with_id("prev_track", "⏮ Previous")
                .enabled(false)
                .build(app)?;
            let next_track = MenuItemBuilder::with_id("next_track", "⏭ Next")
                .enabled(false)
                .build(app)?;
            let separator_playback = PredefinedMenuItem::separator(app)?;
            let show = MenuItemBuilder::with_id("show", "Show").build(app)?;
            let hide = MenuItemBuilder::with_id("hide", "Hide").build(app)?;
            let switch_server = MenuItemBuilder::with_id("switch_server", "Switch Server...").build(app)?;
            let separator2 = PredefinedMenuItem::separator(app)?;
            let discord_rpc_item = CheckMenuItemBuilder::with_id("discord_rpc", "Discord Rich Presence")
                .checked(DISCORD_RPC_ENABLED.load(Ordering::SeqCst))
                .build(app)?;
            let separator3 = PredefinedMenuItem::separator(app)?;
            let settings = MenuItemBuilder::with_id("settings", "Settings...").build(app)?;
            let update = MenuItemBuilder::with_id("update", "Check for updates").build(app)?;
            let relaunch = MenuItemBuilder::with_id("relaunch", "Relaunch").build(app)?;
            let separator4 = PredefinedMenuItem::separator(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

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
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "quit" => {
                        app.exit(0);
                    }
                    "hide" => {
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            let _ = window.hide();
                        }
                    }
                    "show" => {
                        if let Some(window) = app
                            .get_webview_window("main")
                            .or_else(|| app.get_webview_window("launcher"))
                        {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "switch_server" => {
                        // Reset companion ready state
                        COMPANION_READY.store(false, Ordering::SeqCst);
                        SERVER_CONNECT_TIME.store(0, Ordering::SeqCst);

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
                        if let Ok(new_window) = tauri::WebviewWindowBuilder::new(
                            app,
                            new_name,
                            tauri::WebviewUrl::App("index.html".into()),
                        )
                        .title("Music Assistant")
                        .inner_size(1200.0, 800.0)
                        .min_inner_size(600.0, 400.0)
                        .resizable(true)
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
                        // Toggle Discord RPC
                        let current = DISCORD_RPC_ENABLED.load(Ordering::SeqCst);
                        let new_state = !current;
                        DISCORD_RPC_ENABLED.store(new_state, Ordering::SeqCst);

                        if !new_state {
                            discord_rpc::clear_activity();
                        }
                    }
                    "settings" => {
                        // Open or focus settings window
                        if let Some(window) = app.get_webview_window("settings") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        } else {
                            // Use Tauri's App URL which has proper origin for IPC
                            let _ = tauri::WebviewWindowBuilder::new(
                                app,
                                "settings",
                                tauri::WebviewUrl::App("settings.html".into()),
                            )
                            .title("Music Assistant - Settings")
                            .inner_size(600.0, 700.0)
                            .resizable(true)
                            .build();
                        }
                    }
                    "relaunch" => {
                        tauri::process::restart(&app.env());
                    }
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

            Ok(())
        })
        .run(context)
        .expect("Error while running Music Assistant companion");
}
