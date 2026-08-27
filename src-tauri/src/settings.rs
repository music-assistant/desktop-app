use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::path::PathBuf;
use std::sync::{Mutex, RwLock};
use tauri_plugin_autostart::ManagerExt;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum VolumeControlMode {
    /// Auto: use hardware volume when available, fall back to software
    #[default]
    Auto,
    /// Hardware/system volume control only (best quality)
    Hardware,
    /// Software volume control (fallback, reduces quality)
    Software,
    /// Disable volume control entirely
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TrayIconTheme {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub discord_rpc_enabled: bool,
    pub start_minimized: bool,
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
    pub autostart: bool,
    // Last connected server (HTTP URL for launcher to reconnect)
    #[serde(default)]
    pub last_server_url: Option<String>,
    #[serde(default)]
    pub last_server_name: Option<String>,
    // Sendspin/audio settings
    #[serde(default)]
    pub sendspin_enabled: bool,
    #[serde(default)]
    pub sendspin_player_id: Option<String>,
    #[serde(default = "default_player_name")]
    pub sendspin_player_name: String,
    #[serde(default)]
    pub sendspin_server_url: Option<String>,
    #[serde(default)]
    pub audio_device_id: Option<String>,
    #[serde(default)]
    pub sync_delay_ms: i32,
    // Volume control mode
    #[serde(default)]
    pub volume_control_mode: VolumeControlMode,
    // Persisted software volume (0-100). Used to restore volume across
    // reconnects, which happen on every track change. Only written in
    // software volume mode; hardware volume uses the OS as source of truth.
    #[serde(default = "default_software_volume")]
    pub software_volume: u8,
    // Persisted mute state. Shared across hardware and software modes
    // since mute is lost on every reconnect (new connection per track).
    #[serde(default)]
    pub muted: bool,
    // Whether to show the menubar/system tray icon
    #[serde(default = "default_show_tray_icon")]
    pub show_tray_icon: bool,
    // Linux tray icon appearance. Auto follows the desktop color-scheme preference.
    #[serde(default)]
    pub tray_icon_theme: TrayIconTheme,
    // Whether to show now-playing text next to the menubar/system tray icon
    #[serde(default)]
    pub show_tray_now_playing: bool,
    // Whether verbose debug logging is enabled.
    #[serde(default)]
    pub debug_logging: bool,
    // Whether very verbose trace logging is enabled. Only effective when debug logging is enabled.
    #[serde(default)]
    pub trace_logging: bool,
}

fn default_close_to_tray() -> bool {
    false
}

fn default_software_volume() -> u8 {
    100
}

fn default_show_tray_icon() -> bool {
    true
}

fn normalize_platform_settings(settings: Settings) -> Settings {
    // Linux and Windows do not have an application menu or in-app chrome that
    // provides another reliable route to the settings window.
    #[cfg(not(target_os = "macos"))]
    {
        let mut settings = settings;
        settings.show_tray_icon = true;
        settings
    }

    #[cfg(target_os = "macos")]
    settings
}

fn default_player_name() -> String {
    // Use system hostname as default player name, stripped of common suffixes
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map_or_else(
            || crate::i18n::tr("desktop.app.companion_name"),
            |name| {
                // Strip common suffixes like .local, .lan, .home
                name.trim_end_matches(".local")
                    .trim_end_matches(".lan")
                    .trim_end_matches(".home")
                    .trim_end_matches(".localdomain")
                    .to_string()
            },
        )
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discord_rpc_enabled: true,
            start_minimized: false,
            close_to_tray: false,
            autostart: false,
            last_server_url: None,
            last_server_name: None,
            sendspin_enabled: true, // Enabled by default - main purpose of companion app
            sendspin_player_id: None,
            sendspin_player_name: default_player_name(),
            sendspin_server_url: None,
            audio_device_id: None,
            sync_delay_ms: 0,
            volume_control_mode: VolumeControlMode::default(),
            software_volume: default_software_volume(),
            muted: false,
            show_tray_icon: true,
            tray_icon_theme: TrayIconTheme::default(),
            show_tray_now_playing: false,
            debug_logging: false,
            trace_logging: false,
        }
    }
}

static SETTINGS_MUTATION_LOCK: Mutex<()> = Mutex::new(());

static SETTINGS: RwLock<Settings> = RwLock::new(Settings {
    discord_rpc_enabled: true,
    start_minimized: false,
    close_to_tray: false,
    autostart: false,
    last_server_url: None,
    last_server_name: None,
    sendspin_enabled: true, // Enabled by default
    sendspin_player_id: None,
    sendspin_player_name: String::new(), // Will be replaced by load_settings
    sendspin_server_url: None,
    audio_device_id: None,
    sync_delay_ms: 0,
    volume_control_mode: VolumeControlMode::Auto,
    software_volume: 100,
    muted: false,
    show_tray_icon: true,
    tray_icon_theme: TrayIconTheme::Auto,
    show_tray_now_playing: false,
    debug_logging: false,
    trace_logging: false,
});

fn get_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("music-assistant-companion").join("settings.json"))
}

pub fn load_settings() -> Settings {
    let mut settings = if let Some(path) = get_settings_path() {
        match fs::read_to_string(&path) {
            Ok(content) => match serde_json::from_str::<Settings>(&content) {
                Ok(settings) => settings,
                Err(error) => {
                    log::error!("[Settings] Failed to parse settings file: {error}");
                    Settings::default()
                }
            },
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    log::error!("[Settings] Failed to read settings file: {error}");
                }
                Settings::default()
            }
        }
    } else {
        Settings::default()
    };

    settings = normalize_platform_settings(settings);

    if !settings.debug_logging {
        settings.trace_logging = false;
    }

    // Update in-memory settings
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    // Write settings back to file to ensure all fields are persisted
    let _ = save_settings(&settings);

    settings
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let _mutation_guard = SETTINGS_MUTATION_LOCK
        .lock()
        .map_err(|_| "Settings mutation lock is poisoned".to_string())?;
    save_settings_unlocked(settings)
}

pub fn update_settings<F>(update: F) -> Result<(), String>
where
    F: FnOnce(&mut Settings),
{
    let _mutation_guard = SETTINGS_MUTATION_LOCK
        .lock()
        .map_err(|_| "Settings mutation lock is poisoned".to_string())?;
    let mut settings = get_settings();
    update(&mut settings);
    save_settings_unlocked(&settings)
}

fn save_settings_unlocked(settings: &Settings) -> Result<(), String> {
    let path =
        get_settings_path().ok_or_else(|| "Could not determine settings path".to_string())?;

    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create settings dir: {}", e))?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    let temp_path = path.with_extension("json.tmp");
    let mut temp_file = File::create(&temp_path)
        .map_err(|e| format!("Failed to create temporary settings file: {}", e))?;
    std::io::Write::write_all(&mut temp_file, content.as_bytes())
        .map_err(|e| format!("Failed to write temporary settings file: {}", e))?;
    temp_file
        .sync_all()
        .map_err(|e| format!("Failed to flush temporary settings file: {}", e))?;
    fs::rename(&temp_path, &path).map_err(|e| format!("Failed to replace settings file: {}", e))?;

    // Update in-memory settings
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    Ok(())
}

pub fn get_settings() -> Settings {
    SETTINGS
        .read()
        .map_or_else(|_| Settings::default(), |s| s.clone())
}

pub fn set_setting(app: tauri::AppHandle, key: &str, value: bool) -> Result<(), String> {
    let _mutation_guard = SETTINGS_MUTATION_LOCK
        .lock()
        .map_err(|_| "Settings mutation lock is poisoned".to_string())?;
    let previous_settings = get_settings();
    let mut settings = previous_settings.clone();
    let mut should_refresh_tray_now_playing = false;
    let mut should_apply_discord_rpc = false;
    let mut should_apply_tray_visibility = false;
    let mut should_apply_logging = false;

    match key {
        "discord_rpc_enabled" => {
            settings.discord_rpc_enabled = value;
            should_apply_discord_rpc = true;
        }
        "start_minimized" => settings.start_minimized = value,
        "close_to_tray" => settings.close_to_tray = value,
        "autostart" => {
            settings.autostart = value;
        }
        "sendspin_enabled" => {
            settings.sendspin_enabled = value;
        }
        "show_tray_icon" => {
            #[cfg(target_os = "macos")]
            {
                settings.show_tray_icon = value;
                should_apply_tray_visibility = true;
            }
            #[cfg(not(target_os = "macos"))]
            {
                settings.show_tray_icon = true;
                should_apply_tray_visibility = true;
            }
        }
        "show_tray_now_playing" => {
            settings.show_tray_now_playing = value;
            should_refresh_tray_now_playing = true;
        }
        "debug_logging" => {
            settings.debug_logging = value;
            if !value {
                settings.trace_logging = false;
            }
            should_apply_logging = true;
        }
        "trace_logging" => {
            settings.trace_logging = value && settings.debug_logging;
            should_apply_logging = true;
        }
        _ => return Err(format!("Unknown boolean setting: {}", key)),
    }

    save_settings_unlocked(&settings)?;

    if key == "autostart" {
        if let Err(error) = set_autostart(value, app.clone()) {
            if let Err(rollback_error) = save_settings_unlocked(&previous_settings) {
                log::error!("[Settings] Failed to roll back autostart setting: {rollback_error}");
            }
            return Err(error);
        }
    }
    if should_apply_discord_rpc {
        crate::DISCORD_RPC_ENABLED.store(value, std::sync::atomic::Ordering::SeqCst);
        crate::set_discord_rpc_tray_checked(value);
        crate::discord_rpc::refresh();
    }
    if should_apply_logging {
        crate::logging::set_verbosity(crate::logging::verbosity_from_settings(
            settings.debug_logging,
            settings.trace_logging,
        ));
        if key == "debug_logging" {
            log::info!(
                "[App] Debug logging {}",
                if value { "enabled" } else { "disabled" }
            );
        } else {
            log::info!(
                "[App] Trace logging {}",
                if settings.trace_logging {
                    "enabled"
                } else {
                    "disabled"
                }
            );
        }
    }
    if key == "sendspin_enabled" {
        crate::sendspin::set_enabled(value);
        if value {
            log::info!("[Sendspin] Native player enabled");
        } else {
            log::info!("[Sendspin] Native player disabled; stopping local client");
            tauri::async_runtime::spawn(async {
                crate::sendspin::stop_if_disabled().await;
            });
        }
    }
    if should_apply_tray_visibility {
        #[cfg(target_os = "macos")]
        crate::set_tray_visible(value);
        #[cfg(not(target_os = "macos"))]
        crate::set_tray_visible(true);
    }
    if should_refresh_tray_now_playing {
        crate::refresh_tray_now_playing();
    }

    Ok(())
}

/// Set a string setting value
pub fn set_string_setting(key: &str, value: Option<String>) -> Result<(), String> {
    let _mutation_guard = SETTINGS_MUTATION_LOCK
        .lock()
        .map_err(|_| "Settings mutation lock is poisoned".to_string())?;
    let mut settings = get_settings();
    let mut should_restart_sendspin = false;

    match key {
        "last_server_url" => settings.last_server_url = value,
        "last_server_name" => settings.last_server_name = value,
        "sendspin_player_id" => settings.sendspin_player_id = value,
        "sendspin_player_name" => {
            settings.sendspin_player_name = value.unwrap_or_else(default_player_name);
            should_restart_sendspin = true;
        }
        "sendspin_server_url" => settings.sendspin_server_url = value,
        "audio_device_id" => {
            settings.audio_device_id = value;
            should_restart_sendspin = true;
        }
        "volume_control_mode" => {
            if let Some(mode_str) = value {
                settings.volume_control_mode = match mode_str.as_str() {
                    "auto" => VolumeControlMode::Auto,
                    "hardware" => VolumeControlMode::Hardware,
                    "software" => VolumeControlMode::Software,
                    "disabled" => VolumeControlMode::Disabled,
                    _ => return Err(format!("Invalid volume control mode: {}", mode_str)),
                };
            }
        }
        "tray_icon_theme" => {
            settings.tray_icon_theme = match value.as_deref() {
                None | Some("auto") => TrayIconTheme::Auto,
                Some("light") => TrayIconTheme::Light,
                Some("dark") => TrayIconTheme::Dark,
                Some(theme) => return Err(format!("Invalid tray icon theme: {theme}")),
            };
        }
        _ => return Err(format!("Unknown string setting: {}", key)),
    }

    save_settings_unlocked(&settings)?;

    if key == "tray_icon_theme" {
        #[cfg(target_os = "linux")]
        crate::refresh_linux_tray_icon();
    }

    if should_restart_sendspin && settings.sendspin_enabled {
        tauri::async_runtime::spawn(async {
            crate::sendspin::restart().await;
        });
    }

    Ok(())
}

/// Set a numeric setting value
pub fn set_int_setting(key: &str, value: i32) -> Result<(), String> {
    let _mutation_guard = SETTINGS_MUTATION_LOCK
        .lock()
        .map_err(|_| "Settings mutation lock is poisoned".to_string())?;
    let previous_settings = get_settings();
    let mut settings = previous_settings.clone();

    match key {
        "sync_delay_ms" => {
            settings.sync_delay_ms = value.clamp(0, 5_000);
        }
        _ => return Err(format!("Unknown int setting: {}", key)),
    }

    save_settings_unlocked(&settings)?;

    if settings.sendspin_enabled {
        if let Err(error) = crate::sendspin::set_static_delay(value) {
            if let Err(rollback_error) = save_settings_unlocked(&previous_settings) {
                log::error!("[Settings] Failed to roll back sync delay: {rollback_error}");
            }
            return Err(error);
        }
    }

    Ok(())
}

fn set_autostart(enabled: bool, app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("FLATPAK_ID").is_some() {
        return set_flatpak_autostart(enabled).map_err(|error| {
            let message = format!("Failed to update Flatpak autostart: {error}");
            log::warn!("[Settings] {message}");
            message
        });
    }

    let autostart_manager = app.autolaunch();

    let result = if enabled {
        autostart_manager.enable()
    } else {
        autostart_manager.disable()
    };

    result.map_err(|error| {
        let message = format!("Failed to update autostart: {error}");
        log::warn!("[Settings] {message}");
        message
    })
}

#[cfg(all(desktop, target_os = "linux"))]
fn set_flatpak_autostart(enabled: bool) -> std::io::Result<()> {
    const DESKTOP_FILE_NAME: &str = "io.music_assistant.Companion.desktop";
    const AUTOSTART_DESKTOP_ENTRY: &str = include_str!("../templates/flatpak-autostart.desktop");

    // In a Flatpak sandbox, XDG_CONFIG_HOME points at the app-private config
    // dir. The manifest grants `xdg-config/autostart:create`, so write through
    // $HOME/.config/autostart to reach the host XDG autostart directory using
    // the canonical application desktop-entry ID.
    let autostart_dir = dirs::home_dir()
        .ok_or_else(|| std::io::Error::other("Could not determine home directory"))?
        .join(".config")
        .join("autostart");
    let autostart_file = autostart_dir.join(DESKTOP_FILE_NAME);

    if !enabled {
        match std::fs::remove_file(&autostart_file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        return Ok(());
    }

    std::fs::create_dir_all(&autostart_dir)?;

    let temp_file = autostart_file.with_extension("desktop.tmp");
    std::fs::write(&temp_file, AUTOSTART_DESKTOP_ENTRY)?;
    std::fs::rename(temp_file, autostart_file)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_control_mode_default_is_auto() {
        assert_eq!(VolumeControlMode::default(), VolumeControlMode::Auto);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_normalizes_tray_icon_to_visible() {
        let settings = Settings {
            show_tray_icon: false,
            ..Settings::default()
        };
        assert!(normalize_platform_settings(settings).show_tray_icon);
    }

    #[test]
    fn tray_icon_theme_default_is_auto() {
        assert_eq!(TrayIconTheme::default(), TrayIconTheme::Auto);
    }

    #[test]
    fn tray_icon_theme_serde_roundtrip() {
        for (theme, expected_json) in [
            (TrayIconTheme::Auto, "\"auto\""),
            (TrayIconTheme::Light, "\"light\""),
            (TrayIconTheme::Dark, "\"dark\""),
        ] {
            let json = serde_json::to_string(&theme).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: TrayIconTheme = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, theme);
        }
    }

    #[test]
    fn software_volume_default_is_100() {
        let settings = Settings::default();
        assert_eq!(settings.software_volume, 100);
    }

    #[test]
    fn muted_default_is_false() {
        let settings = Settings::default();
        assert!(!settings.muted);
    }

    #[test]
    fn software_volume_serde_roundtrip() {
        let settings = Settings {
            software_volume: 42,
            muted: true,
            ..Settings::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        let deserialized: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.software_volume, 42);
        assert!(deserialized.muted);
    }

    #[test]
    fn software_volume_missing_from_json_uses_default() {
        // Simulate loading settings from an older version without these fields
        let json = r#"{"discord_rpc_enabled":true,"start_minimized":false,"autostart":false,"sendspin_enabled":true,"sendspin_player_name":"test","sync_delay_ms":0,"volume_control_mode":"auto"}"#;
        let settings: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(settings.software_volume, 100);
        assert!(!settings.muted);
    }

    #[test]
    fn volume_control_mode_serde_roundtrip() {
        // Verify all variants serialize to lowercase and deserialize back
        let modes = vec![
            (VolumeControlMode::Auto, "\"auto\""),
            (VolumeControlMode::Hardware, "\"hardware\""),
            (VolumeControlMode::Software, "\"software\""),
            (VolumeControlMode::Disabled, "\"disabled\""),
        ];
        for (mode, expected_json) in modes {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, expected_json);
            let deserialized: VolumeControlMode = serde_json::from_str(&json).unwrap();
            assert_eq!(deserialized, mode);
        }
    }

    #[test]
    fn test_invalid_volume_control_mode_returns_error() {
        let result = set_string_setting("volume_control_mode", Some("invalid".to_string()));
        assert!(result.is_err());
        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("Invalid volume control mode"),
            "Expected error to contain 'Invalid volume control mode', got: {}",
            error_msg
        );
    }

    #[test]
    fn test_malformed_json_deserializes_to_defaults() {
        // Test that malformed JSON returns Err
        let result = serde_json::from_str::<Settings>("not valid json");
        assert!(result.is_err());

        // Test that unwrap_or_default gives defaults
        let settings = serde_json::from_str::<Settings>("not valid json").unwrap_or_default();
        assert!(settings.discord_rpc_enabled);
        assert_eq!(settings.software_volume, 100);
        assert!(!settings.muted);
    }

    #[test]
    fn test_unknown_setting_keys_return_errors() {
        // Test unknown string setting key
        let result = set_string_setting("nonexistent_key", Some("value".to_string()));
        assert!(result.is_err());
        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("Unknown string setting"),
            "Expected error to contain 'Unknown string setting', got: {}",
            error_msg
        );

        // Test unknown int setting key
        let result = set_int_setting("nonexistent_key", 42);
        assert!(result.is_err());
        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("Unknown int setting"),
            "Expected error to contain 'Unknown int setting', got: {}",
            error_msg
        );
    }
}
