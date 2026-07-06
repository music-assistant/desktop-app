//! Follow the desktop's light/dark preference on Linux.
//!
//! Linux-only: on macOS and Windows the OS notifies the app of theme changes
//! natively and Tauri/WebView pick them up without help.
//!
//! GTK3 (and therefore `WebKitGTK`'s `prefers-color-scheme`) only honors the
//! legacy `gtk-theme-name` setting, but modern desktops (GNOME 42+, KDE, ...)
//! express dark mode through the `org.freedesktop.appearance color-scheme`
//! setting exposed by the xdg-desktop-portal. Browsers read the portal;
//! plain GTK3 apps do not.
//!
//! This module reads the portal setting, maps it onto Tauri's theme (which
//! sets `gtk-application-prefer-dark-theme`, restyling every window's
//! decorations and flipping the webview's `prefers-color-scheme`), and tracks
//! `SettingChanged` signals so live theme switches propagate immediately.

use futures_util::StreamExt;
use tauri::{AppHandle, Theme};
use zbus::zvariant::{OwnedValue, Value};

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS_IFACE: &str = "org.freedesktop.portal.Settings";
const APPEARANCE_NS: &str = "org.freedesktop.appearance";
const COLOR_SCHEME_KEY: &str = "color-scheme";

/// Start tracking the system color scheme on a background thread.
///
/// Best-effort: if no portal is available (very old distros, stripped-down
/// sessions) the thread logs once and exits, leaving GTK's default theme
/// resolution untouched.
pub fn init(app: AppHandle) {
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                log::error!("[Theme] Failed to create color-scheme runtime: {e}");
                return;
            }
        };

        if let Err(e) = runtime.block_on(run(app)) {
            log::info!("[Theme] System color-scheme tracking unavailable: {e}");
        }
    });
}

async fn run(app: AppHandle) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, PORTAL_BUS, PORTAL_PATH, SETTINGS_IFACE).await?;

    // Subscribe before the initial read so a change racing the read is not lost.
    let mut changes = proxy.receive_signal("SettingChanged").await?;

    apply(&app, read_color_scheme(&proxy).await);

    while let Some(message) = changes.next().await {
        let Ok((namespace, key, value)) =
            message.body().deserialize::<(String, String, OwnedValue)>()
        else {
            continue;
        };
        if namespace == APPEARANCE_NS && key == COLOR_SCHEME_KEY {
            apply(&app, scheme_from_value(&value));
        }
    }
    Ok(())
}

/// Read the current color scheme from the portal.
async fn read_color_scheme(proxy: &zbus::Proxy<'_>) -> Option<u32> {
    let args = (APPEARANCE_NS, COLOR_SCHEME_KEY);
    let value: OwnedValue = match proxy.call("ReadOne", &args).await {
        Ok(value) => value,
        // Portal versions < 2 only expose the deprecated `Read`, which wraps
        // the result in an extra variant layer (handled by scheme_from_value).
        Err(_) => match proxy.call("Read", &args).await {
            Ok(value) => value,
            Err(e) => {
                log::info!("[Theme] Portal has no color-scheme setting: {e}");
                return None;
            }
        },
    };
    scheme_from_value(&value)
}

/// Unwrap (possibly nested) variants down to the `u` color-scheme value.
fn scheme_from_value(value: &Value<'_>) -> Option<u32> {
    match value {
        Value::Value(inner) => scheme_from_value(inner),
        Value::U32(scheme) => Some(*scheme),
        _ => None,
    }
}

/// Map the portal value onto a Tauri theme and apply it to all windows.
fn apply(app: &AppHandle, scheme: Option<u32>) {
    // 0 = no preference, 1 = prefer dark, 2 = prefer light.
    let theme = match scheme {
        Some(1) => Some(Theme::Dark),
        Some(2) => Some(Theme::Light),
        // No preference (or unreadable): tao maps None to
        // prefer-dark=false, GTK's default. Legacy dark setups still work
        // because this doesn't touch `gtk-theme-name`.
        _ => None,
    };
    log::info!("[Theme] System color-scheme {scheme:?} -> {theme:?}");

    let app_handle = app.clone();
    // GTK settings must be touched from the main thread.
    let _ = app.run_on_main_thread(move || {
        app_handle.set_theme(theme);
    });
}
