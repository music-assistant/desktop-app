use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;
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
    // Whether to show now-playing text next to the menubar/system tray icon
    #[serde(default)]
    pub show_tray_now_playing: bool,
    // Whether verbose debug logging is enabled.
    #[serde(default)]
    pub debug_logging: bool,
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
            show_tray_now_playing: false,
            debug_logging: false,
        }
    }
}

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
    show_tray_now_playing: false,
    debug_logging: false,
});

fn get_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("music-assistant-companion").join("settings.json"))
}

pub fn load_settings() -> Settings {
    let settings = if let Some(path) = get_settings_path() {
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str::<Settings>(&content).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    } else {
        Settings::default()
    };

    // Update in-memory settings
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    // Write settings back to file to ensure all fields are persisted
    let _ = save_settings(&settings);

    settings
}

pub fn save_settings(settings: &Settings) -> Result<(), String> {
    let path =
        get_settings_path().ok_or_else(|| "Could not determine settings path".to_string())?;

    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create settings dir: {}", e))?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    fs::write(&path, &content).map_err(|e| format!("Failed to write settings file: {}", e))?;

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
    let mut settings = get_settings();
    let mut should_refresh_tray_now_playing = false;

    match key {
        "discord_rpc_enabled" => {
            settings.discord_rpc_enabled = value;
            // Update the global flag
            crate::DISCORD_RPC_ENABLED.store(value, std::sync::atomic::Ordering::SeqCst);
            if !value {
                crate::discord_rpc::clear_activity();
            }
        }
        "start_minimized" => settings.start_minimized = value,
        "close_to_tray" => settings.close_to_tray = value,
        "autostart" => {
            // Update the platform autostart registration before persisting the
            // setting, so a portal/plugin failure is surfaced to the UI instead
            // of saving a state the OS did not actually apply.
            #[cfg(desktop)]
            {
                set_autostart(value, app)?;
            }
            settings.autostart = value;
        }
        "sendspin_enabled" => {
            settings.sendspin_enabled = value;
            crate::sendspin::set_enabled(value);
            if value {
                log::info!("[Sendspin] Native player enabled");
            } else {
                log::info!("[Sendspin] Native player disabled; stopping local client");
                tauri::async_runtime::spawn(async {
                    crate::sendspin::stop().await;
                });
            }
        }
        "show_tray_icon" => {
            settings.show_tray_icon = value;
            crate::set_tray_visible(value);
        }
        "show_tray_now_playing" => {
            settings.show_tray_now_playing = value;
            should_refresh_tray_now_playing = true;
        }
        "debug_logging" => {
            settings.debug_logging = value;
            // Apply the new verbosity immediately (live toggle).
            crate::logging::set_debug_enabled(value);
            log::info!(
                "[App] Debug logging {}",
                if value { "enabled" } else { "disabled" }
            );
        }
        _ => return Err(format!("Unknown boolean setting: {}", key)),
    }

    save_settings(&settings)?;

    if should_refresh_tray_now_playing {
        crate::refresh_tray_now_playing();
    }

    Ok(())
}

/// Set a string setting value
pub fn set_string_setting(key: &str, value: Option<String>) -> Result<(), String> {
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
        _ => return Err(format!("Unknown string setting: {}", key)),
    }

    save_settings(&settings)?;

    if should_restart_sendspin && settings.sendspin_enabled {
        tauri::async_runtime::spawn(async {
            crate::sendspin::restart().await;
        });
    }

    Ok(())
}

/// Set a numeric setting value
pub fn set_int_setting(key: &str, value: i32) -> Result<(), String> {
    let mut settings = get_settings();

    match key {
        "sync_delay_ms" => {
            settings.sync_delay_ms = value;
        }
        _ => return Err(format!("Unknown int setting: {}", key)),
    }

    save_settings(&settings)?;

    if settings.sendspin_enabled {
        tauri::async_runtime::spawn(async {
            crate::sendspin::restart().await;
        });
    }

    Ok(())
}

#[cfg(desktop)]
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
    // $HOME/.config/autostart to reach the host XDG autostart directory.
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
