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
    Auto,
    /// Hardware/system volume control only (best quality)
    Hardware,
    /// Software volume control, independent of system volume
    #[default]
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
#[serde(default)]
pub struct Settings {
    pub discord_rpc_enabled: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub autostart: bool,
    // Last connected server (HTTP URL for launcher to reconnect)
    pub last_server_url: Option<String>,
    pub last_server_name: Option<String>,
    // Sendspin/audio settings
    pub sendspin_enabled: bool,
    pub sendspin_player_id: Option<String>,
    pub sendspin_player_name: String,
    pub sendspin_server_url: Option<String>,
    pub audio_device_id: Option<String>,
    pub sync_delay_ms: i32,
    // Volume control mode
    pub volume_control_mode: VolumeControlMode,
    // Persisted software volume (0-100). Used to restore volume across
    // reconnects, which happen on every track change. Only written in
    // software volume mode; hardware volume uses the OS as source of truth.
    pub software_volume: u8,
    // Persisted mute state. Shared across hardware and software modes
    // since mute is lost on every reconnect (new connection per track).
    pub muted: bool,
    // Whether to show the menubar/system tray icon
    pub show_tray_icon: bool,
    pub hide_dock_icon_when_hidden: bool,
    // Linux tray icon appearance. Auto follows the desktop color-scheme preference.
    pub tray_icon_theme: TrayIconTheme,
    // Whether to show now-playing text next to the menubar/system tray icon
    pub show_tray_now_playing: bool,
    // Whether verbose debug logging is enabled.
    pub debug_logging: bool,
    // Whether very verbose trace logging is enabled. Only effective when debug logging is enabled.
    pub trace_logging: bool,
}

fn default_software_volume() -> u8 {
    100
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
            close_to_tray: true,
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
            hide_dock_icon_when_hidden: true,
            tray_icon_theme: TrayIconTheme::default(),
            show_tray_now_playing: false,
            debug_logging: false,
            trace_logging: false,
        }
    }
}

static SETTINGS_LOAD_ERROR: RwLock<Option<String>> = RwLock::new(None);
static SETTINGS_MUTATION_LOCK: Mutex<()> = Mutex::new(());

static SETTINGS: RwLock<Settings> = RwLock::new(Settings {
    discord_rpc_enabled: true,
    start_minimized: false,
    close_to_tray: true,
    autostart: false,
    last_server_url: None,
    last_server_name: None,
    sendspin_enabled: true, // Enabled by default
    sendspin_player_id: None,
    sendspin_player_name: String::new(), // Will be replaced by load_settings
    sendspin_server_url: None,
    audio_device_id: None,
    sync_delay_ms: 0,
    volume_control_mode: VolumeControlMode::Software,
    software_volume: 100,
    muted: false,
    show_tray_icon: true,
    hide_dock_icon_when_hidden: true,
    tray_icon_theme: TrayIconTheme::Auto,
    show_tray_now_playing: false,
    debug_logging: false,
    trace_logging: false,
});

fn get_settings_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("music-assistant-companion").join("settings.json"))
}

fn read_settings(path: &std::path::Path) -> Result<Settings, String> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            // Serde's full error may contain user-provided values, including secrets.
            format!(
                "Invalid settings at line {}, column {}. Repair or remove {} before saving settings.",
                error.line(),
                error.column(),
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("Failed to read {}: {error}", path.display())),
    }
}

pub fn load_settings() -> Settings {
    let loaded =
        get_settings_path().map_or_else(|| Ok(Settings::default()), |path| read_settings(&path));
    if let Ok(mut error) = SETTINGS_LOAD_ERROR.write() {
        // A startup failure is sticky: repaired settings must be loaded by a
        // restart before fallback defaults may be persisted.
        if error.is_none() {
            *error = loaded.as_ref().err().cloned();
        }
    }
    let can_save = loaded.is_ok();
    // The caller logs the retained error after installing the logging plugin.
    let mut settings = loaded.unwrap_or_default();

    settings = normalize_platform_settings(settings);

    if !settings.debug_logging {
        settings.trace_logging = false;
    }

    // Update in-memory settings
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    // Fill in omitted fields only when the original file was loaded successfully.
    if can_save {
        let _ = save_settings(&settings);
    }

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

    let startup_error = SETTINGS_LOAD_ERROR
        .read()
        .map_err(|_| "Settings load error lock is poisoned".to_string())?;
    write_settings(&path, settings, startup_error.as_deref())?;

    // Update in-memory settings only after a successful save.
    if let Ok(mut s) = SETTINGS.write() {
        *s = settings.clone();
    }

    Ok(())
}

fn write_settings(
    path: &std::path::Path,
    settings: &Settings,
    startup_error: Option<&str>,
) -> Result<(), String> {
    // Never overwrite repaired settings with the fallback defaults still in
    // memory. Removing the rejected file also requires a restart to reset it.
    if let Some(error) = startup_error {
        return Err(format!(
            "{error} Restart the app after repairing or removing the settings file before saving settings."
        ));
    }

    // Also protect files that become unreadable or invalid after startup.
    read_settings(path)?;

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
    fs::rename(&temp_path, path).map_err(|e| format!("Failed to replace settings file: {}", e))?;

    Ok(())
}

/// Keep startup failures visible even after the file is repaired or removed:
/// saving remains blocked until a restart loads corrected settings or defaults.
pub fn get_settings_load_error() -> Option<String> {
    if let Some(error) = SETTINGS_LOAD_ERROR
        .read()
        .ok()
        .and_then(|error| error.clone())
    {
        return Some(error);
    }
    // Also catch a file that became invalid after startup.
    get_settings_path().and_then(|path| read_settings(&path).err())
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
        "hide_dock_icon_when_hidden" => settings.hide_dock_icon_when_hidden = value,
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
        crate::set_tray_visible(app.clone(), value);
        #[cfg(not(target_os = "macos"))]
        crate::set_tray_visible(app.clone(), true);
    }
    #[cfg(target_os = "macos")]
    if should_apply_tray_visibility || key == "hide_dock_icon_when_hidden" {
        crate::sync_dock_visibility(&app);
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
    fn volume_control_mode_defaults_to_software() {
        assert_eq!(VolumeControlMode::default(), VolumeControlMode::Software);
        assert_eq!(
            Settings::default().volume_control_mode,
            VolumeControlMode::Software
        );
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.volume_control_mode, VolumeControlMode::Software);
    }

    #[test]
    fn saved_volume_control_modes_are_preserved() {
        for mode in ["auto", "hardware", "software", "disabled"] {
            let json = serde_json::json!({"volume_control_mode": mode});
            let settings: Settings = serde_json::from_value(json).unwrap();
            assert_eq!(
                serde_json::to_value(settings).unwrap()["volume_control_mode"],
                mode
            );
        }
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
    fn close_and_dock_preferences_default_on_but_preserve_explicit_false() {
        let defaults = Settings::default();
        assert!(defaults.close_to_tray);
        assert!(defaults.hide_dock_icon_when_hidden);

        let missing: Settings = serde_json::from_str(r#"{"start_minimized":true}"#).unwrap();
        assert!(missing.close_to_tray);
        assert!(missing.hide_dock_icon_when_hidden);

        let disabled: Settings =
            serde_json::from_str(r#"{"close_to_tray":false,"hide_dock_icon_when_hidden":false}"#)
                .unwrap();
        assert!(!disabled.close_to_tray);
        assert!(!disabled.hide_dock_icon_when_hidden);
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert!(!restored.close_to_tray);
        assert!(!restored.hide_dock_icon_when_hidden);
    }

    #[test]
    fn empty_json_uses_application_defaults() {
        let settings: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            serde_json::to_value(Settings::default()).unwrap()
        );
    }

    #[test]
    fn partial_json_preserves_provisioned_settings() {
        let settings: Settings = serde_json::from_str(
            r#"{"last_server_url":"http://music.local:8095","sendspin_player_name":"Kitchen"}"#,
        )
        .unwrap();
        assert_eq!(
            settings.last_server_url.as_deref(),
            Some("http://music.local:8095")
        );
        assert_eq!(settings.sendspin_player_name, "Kitchen");
        assert!(settings.sendspin_enabled);
        assert!(settings.discord_rpc_enabled);
        assert!(!settings.autostart);
    }

    #[test]
    fn explicit_false_overrides_application_defaults() {
        let settings: Settings = serde_json::from_str(
            r#"{"sendspin_enabled":false,"discord_rpc_enabled":false,"software_volume":0}"#,
        )
        .unwrap();
        assert!(!settings.sendspin_enabled);
        assert!(!settings.discord_rpc_enabled);
        assert_eq!(settings.software_volume, 0);
    }

    struct TestSettingsFile(PathBuf);

    impl TestSettingsFile {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ma-settings-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("settings.json"))
        }
    }

    impl Drop for TestSettingsFile {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    #[test]
    fn rejected_settings_survive_loading_and_subsequent_saves() {
        for content in [
            "not valid json",
            r#"{"sendspin_enabled":"secret-value"}"#,
            r#"{"volume_control_mode":"secret-value"}"#,
        ] {
            let file = TestSettingsFile::new();
            fs::write(&file.0, content).unwrap();
            let error = read_settings(&file.0).unwrap_err();
            assert!(!error.contains("secret-value"));
            assert!(write_settings(&file.0, &Settings::default(), Some(&error)).is_err());
            assert_eq!(fs::read_to_string(&file.0).unwrap(), content);
            assert!(!file.0.with_extension("json.tmp").exists());

            let repaired = r#"{"sendspin_enabled":false,"sendspin_player_name":"Kitchen"}"#;
            fs::write(&file.0, repaired).unwrap();
            for _ in 0..2 {
                let save_error =
                    write_settings(&file.0, &Settings::default(), Some(&error)).unwrap_err();
                assert!(save_error.contains("Restart"));
                assert_eq!(fs::read_to_string(&file.0).unwrap(), repaired);
                assert!(!file.0.with_extension("json.tmp").exists());
            }

            // Simulate restarting and loading the repaired values, not defaults.
            let reloaded = read_settings(&file.0).unwrap();
            write_settings(&file.0, &reloaded, None).unwrap();
            let saved = read_settings(&file.0).unwrap();
            assert!(!saved.sendspin_enabled);
            assert_eq!(saved.sendspin_player_name, "Kitchen");
        }
    }

    #[test]
    fn missing_settings_can_be_created() {
        let file = TestSettingsFile::new();
        assert!(read_settings(&file.0).unwrap().sendspin_enabled);
        write_settings(&file.0, &Settings::default(), None).unwrap();
        assert!(file.0.is_file());
    }

    #[test]
    fn unreadable_settings_are_not_replaced() {
        let file = TestSettingsFile::new();
        // A directory produces a read error even when tests run as root.
        fs::create_dir(&file.0).unwrap();
        let error = read_settings(&file.0).unwrap_err();
        assert!(write_settings(&file.0, &Settings::default(), Some(&error)).is_err());
        assert!(file.0.is_dir());

        fs::remove_dir(&file.0).unwrap();
        let repaired = r#"{"software_volume":42,"sendspin_player_name":"Kitchen"}"#;
        fs::write(&file.0, repaired).unwrap();
        assert!(write_settings(&file.0, &Settings::default(), Some(&error)).is_err());
        assert_eq!(fs::read_to_string(&file.0).unwrap(), repaired);
        assert!(!file.0.with_extension("json.tmp").exists());
    }

    #[test]
    fn removing_rejected_settings_requires_restart_before_reset() {
        let file = TestSettingsFile::new();
        fs::write(&file.0, "invalid json").unwrap();
        let error = read_settings(&file.0).unwrap_err();
        fs::remove_file(&file.0).unwrap();

        assert!(write_settings(&file.0, &Settings::default(), Some(&error)).is_err());
        assert!(!file.0.exists());
        assert!(!file.0.with_extension("json.tmp").exists());

        // With no startup failure in a new process, a missing file resets normally.
        let reloaded = read_settings(&file.0).unwrap();
        write_settings(&file.0, &reloaded, None).unwrap();
        assert!(read_settings(&file.0).unwrap().sendspin_enabled);
    }

    #[test]
    fn settings_that_become_invalid_or_unreadable_after_startup_are_not_replaced() {
        let file = TestSettingsFile::new();
        fs::write(&file.0, "{}").unwrap();
        let loaded = read_settings(&file.0).unwrap();

        fs::write(&file.0, "invalid json").unwrap();
        assert!(write_settings(&file.0, &loaded, None).is_err());
        assert_eq!(fs::read_to_string(&file.0).unwrap(), "invalid json");
        assert!(!file.0.with_extension("json.tmp").exists());

        fs::remove_file(&file.0).unwrap();
        fs::create_dir(&file.0).unwrap();
        assert!(write_settings(&file.0, &loaded, None).is_err());
        assert!(file.0.is_dir());
        assert!(!file.0.with_extension("json.tmp").exists());
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
