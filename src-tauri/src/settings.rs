use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub discord_rpc_enabled: bool,
    pub start_minimized: bool,
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
}

fn default_player_name() -> String {
    // Use system hostname as default player name, stripped of common suffixes
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|name| {
            // Strip common suffixes like .local, .lan, .home
            name.trim_end_matches(".local")
                .trim_end_matches(".lan")
                .trim_end_matches(".home")
                .trim_end_matches(".localdomain")
                .to_string()
        })
        .unwrap_or_else(|| "Music Assistant Companion".to_string())
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            discord_rpc_enabled: true,
            start_minimized: false,
            autostart: false,
            last_server_url: None,
            last_server_name: None,
            sendspin_enabled: true, // Enabled by default - main purpose of companion app
            sendspin_player_id: None,
            sendspin_player_name: default_player_name(),
            sendspin_server_url: None,
            audio_device_id: None,
            sync_delay_ms: 0,
        }
    }
}

static SETTINGS: RwLock<Settings> = RwLock::new(Settings {
    discord_rpc_enabled: true,
    start_minimized: false,
    autostart: false,
    last_server_url: None,
    last_server_name: None,
    sendspin_enabled: true, // Enabled by default
    sendspin_player_id: None,
    sendspin_player_name: String::new(), // Will be replaced by load_settings
    sendspin_server_url: None,
    audio_device_id: None,
    sync_delay_ms: 0,
});

fn get_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("music-assistant-companion").join("settings.json"))
}

pub fn load_settings() -> Settings {
    let settings = if let Some(path) = get_settings_path() {
        match fs::read_to_string(&path) {
            Ok(content) => {
                serde_json::from_str::<Settings>(&content).unwrap_or_default()
            }
            Err(_) => Settings::default()
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
    let path = get_settings_path()
        .ok_or_else(|| "Could not determine settings path".to_string())?;

    // Create parent directory if needed
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create settings dir: {}", e))?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize settings: {}", e))?;
    fs::write(&path, &content)
        .map_err(|e| format!("Failed to write settings file: {}", e))?;

    // Update in-memory settings
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    Ok(())
}

pub fn get_settings() -> Settings {
    SETTINGS.read().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_setting(key: &str, value: bool) -> Result<(), String> {
    let mut settings = get_settings();

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
        "autostart" => {
            settings.autostart = value;
            // Handle autostart registration
            #[cfg(desktop)]
            {
                let _ = set_autostart(value);
            }
        }
        "sendspin_enabled" => {
            settings.sendspin_enabled = value;
            crate::sendspin::set_enabled(value);
            // When disabled, stop the client
            // When enabled, the frontend will call configure_sendspin with auth token
            if !value {
                tauri::async_runtime::spawn(async {
                    crate::sendspin::stop().await;
                });
            }
            // Note: When enabling, frontend should reload or call configure_sendspin
            // to start the client with proper auth token
        }
        _ => return Err(format!("Unknown boolean setting: {}", key)),
    }

    save_settings(&settings)
}

/// Set a string setting value
pub fn set_string_setting(key: &str, value: Option<String>) -> Result<(), String> {
    let mut settings = get_settings();

    match key {
        "last_server_url" => settings.last_server_url = value,
        "last_server_name" => settings.last_server_name = value,
        "sendspin_player_id" => settings.sendspin_player_id = value,
        "sendspin_player_name" => {
            settings.sendspin_player_name = value.unwrap_or_else(default_player_name)
        }
        "sendspin_server_url" => settings.sendspin_server_url = value,
        "audio_device_id" => settings.audio_device_id = value,
        _ => return Err(format!("Unknown string setting: {}", key)),
    }

    save_settings(&settings)
}

/// Set a numeric setting value
pub fn set_int_setting(key: &str, value: i32) -> Result<(), String> {
    let mut settings = get_settings();

    match key {
        "sync_delay_ms" => settings.sync_delay_ms = value,
        _ => return Err(format!("Unknown int setting: {}", key)),
    }

    save_settings(&settings)
}

#[cfg(desktop)]
fn set_autostart(_enabled: bool) -> Result<(), String> {
    // TODO: Platform-specific autostart implementation
    // macOS: launchd or Login Items
    // Windows: registry or Task Scheduler
    // Linux: .desktop file in autostart
    Ok(())
}
