#[cfg(target_os = "linux")]
fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.').map(|part| part.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??))
}

fn parse_full_version(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn should_disable_explicit_sync(version: Option<&str>, override_present: bool) -> bool {
    !override_present
        && version
            .and_then(parse_full_version)
            .is_some_and(|v| v < (2, 54, 2))
}

fn desktop_list_contains(desktops: &str, target: &str) -> bool {
    desktops
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case(target))
}

fn should_force_gdk_x11(gdk_backend_is_set: bool, current_desktop: Option<&str>) -> bool {
    !gdk_backend_is_set
        && current_desktop.is_some_and(|desktops| desktop_list_contains(desktops, "GNOME"))
}

fn is_lxqt_x11_session(
    session_type: Option<&str>,
    current_desktop: Option<&str>,
    session_desktop: Option<&str>,
    menu_prefix: Option<&str>,
) -> bool {
    session_type.is_some_and(|session_type| session_type.eq_ignore_ascii_case("x11"))
        && (current_desktop.is_some_and(|desktops| desktop_list_contains(desktops, "LXQt"))
            || session_desktop.is_some_and(|desktop| desktop.eq_ignore_ascii_case("LXQt"))
            || menu_prefix.is_some_and(|prefix| prefix.eq_ignore_ascii_case("lxqt-")))
}

fn runs_under_xwayland(
    selected_display_type: Option<&str>,
    session_type: Option<&str>,
    wayland_display: Option<&str>,
    wayland_socket_present: bool,
) -> bool {
    // GTK's selected display is authoritative; session hints only distinguish
    // XWayland from native X11. Do not probe or consume inherited sockets.
    selected_display_type == Some("GdkX11Display")
        && (session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
            || wayland_display.is_some_and(|value| !value.is_empty())
            || wayland_socket_present)
}

#[allow(clippy::too_many_arguments)]
fn dmabuf_disable_reason_for(
    webkit_has_legacy_renderer: bool,
    forced_gdk_x11: bool,
    runs_under_xwayland: bool,
    session_type: Option<&str>,
    current_desktop: Option<&str>,
    session_desktop: Option<&str>,
    menu_prefix: Option<&str>,
) -> Option<&'static str> {
    if webkit_has_legacy_renderer {
        // Older WebKitGTK versions crash unless the legacy renderer is forced.
        return Some("legacy WebKitGTK renderer");
    }
    if forced_gdk_x11 {
        return Some("GNOME forced XWayland");
    }
    if runs_under_xwayland {
        return Some("XWayland fallback");
    }
    if is_lxqt_x11_session(session_type, current_desktop, session_desktop, menu_prefix) {
        return Some("LXQt/X11 blank WebKit window workaround");
    }
    None
}

#[cfg(target_os = "linux")]
pub(crate) struct EarlyGraphicsState {
    forced_gdk_x11: bool,
    wayland_socket_present: bool,
    webkit_version: Option<String>,
    explicit_sync_override: bool,
    disabled_explicit_sync: bool,
}

#[cfg(target_os = "linux")]
pub(crate) struct GraphicsReport {
    early: EarlyGraphicsState,
    dmabuf_override: bool,
    dmabuf_disable_reason: Option<&'static str>,
}

#[cfg(target_os = "linux")]
pub(crate) fn before_gtk() -> EarlyGraphicsState {
    let webkit_version = tauri::webview_version().ok();
    let explicit_sync_override = std::env::var_os("__NV_DISABLE_EXPLICIT_SYNC").is_some();
    let disabled_explicit_sync =
        should_disable_explicit_sync(webkit_version.as_deref(), explicit_sync_override);
    if disabled_explicit_sync {
        std::env::set_var("__NV_DISABLE_EXPLICIT_SYNC", "1");
    }

    let current_desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let forced_gdk_x11 = should_force_gdk_x11(
        std::env::var_os("GDK_BACKEND").is_some(),
        current_desktop.as_deref(),
    );
    if forced_gdk_x11 {
        std::env::set_var("GDK_BACKEND", "x11");
    }
    // libwayland consumes WAYLAND_SOCKET during GTK initialization.
    let wayland_socket_present = std::env::var_os("WAYLAND_SOCKET").is_some();
    EarlyGraphicsState {
        forced_gdk_x11,
        wayland_socket_present,
        webkit_version,
        explicit_sync_override,
        disabled_explicit_sync,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn after_gtk(early: EarlyGraphicsState) -> GraphicsReport {
    use gdk::prelude::ObjectExt;

    let display = gdk::Display::default();
    let display_type = display.as_ref().map(|display| display.type_());
    let current_desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let session_desktop = std::env::var("XDG_SESSION_DESKTOP").ok();
    let menu_prefix = std::env::var("XDG_MENU_PREFIX").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();

    // Keep the existing narrowly scoped DMABUF policy and honor overrides.
    let dmabuf_override = std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_some();
    let dmabuf_disable_reason = (!dmabuf_override)
        .then(|| {
            dmabuf_disable_reason_for(
                early
                    .webkit_version
                    .as_deref()
                    .and_then(parse_major_minor)
                    .is_some_and(|version| version < (2, 46)),
                early.forced_gdk_x11,
                runs_under_xwayland(
                    display_type.as_ref().map(|type_| type_.name()),
                    session_type.as_deref(),
                    wayland_display.as_deref(),
                    early.wayland_socket_present,
                ),
                session_type.as_deref(),
                current_desktop.as_deref(),
                session_desktop.as_deref(),
                menu_prefix.as_deref(),
            )
        })
        .flatten();
    if dmabuf_disable_reason.is_some() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    GraphicsReport {
        early,
        dmabuf_override,
        dmabuf_disable_reason,
    }
}

#[cfg(target_os = "linux")]
impl GraphicsReport {
    pub(crate) fn log(self) {
        if self.early.forced_gdk_x11 {
            log::info!("[LinuxWebView] Set GDK_BACKEND=x11 for GNOME compatibility");
        } else {
            log::info!("[LinuxWebView] Leaving GDK_BACKEND unchanged");
        }
        if self.early.explicit_sync_override {
            log::info!("[LinuxWebView] __NV_DISABLE_EXPLICIT_SYNC already set; leaving explicit value unchanged");
        } else if self.early.disabled_explicit_sync {
            log::info!(
                "[LinuxWebView] Set __NV_DISABLE_EXPLICIT_SYNC=1 for WebKitGTK older than 2.54.2"
            );
        } else if self
            .early
            .webkit_version
            .as_deref()
            .and_then(parse_full_version)
            .is_none()
        {
            log::warn!("[LinuxWebView] WebKitGTK version unknown; leaving __NV_DISABLE_EXPLICIT_SYNC unchanged");
        } else {
            log::info!("[LinuxWebView] WebKitGTK 2.54.2 or newer; leaving __NV_DISABLE_EXPLICIT_SYNC unchanged");
        }
        match (self.dmabuf_override, self.dmabuf_disable_reason) {
            (true, _) => log::info!("[LinuxWebView] WEBKIT_DISABLE_DMABUF_RENDERER already set; leaving explicit value unchanged. If the app crashes or shows a blank window, try the opposite value."),
            (false, Some(reason)) => log::info!("[LinuxWebView] Set WEBKIT_DISABLE_DMABUF_RENDERER=1 ({reason}). If the app crashes on startup, try WEBKIT_DISABLE_DMABUF_RENDERER=0 or unset it."),
            (false, None) => log::info!("[LinuxWebView] Leaving WebKitGTK DMABUF renderer enabled. If the app crashes or shows a blank window, try WEBKIT_DISABLE_DMABUF_RENDERER=1."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_sync_version_policy() {
        for (version, disabled) in [
            ("2.54.0", true),
            ("2.54.1", true),
            ("2.54.2", false),
            ("2.54.3", false),
            ("2.52.6", true),
            ("2.55.0", false),
            ("3.0.0", false),
            ("2.45.0", true),
        ] {
            assert_eq!(
                should_disable_explicit_sync(Some(version), false),
                disabled,
                "{version}"
            );
            assert!(
                !should_disable_explicit_sync(Some(version), true),
                "override: {version}"
            );
        }
        for version in [
            None,
            Some(""),
            Some("2.54"),
            Some("2.54.2.1"),
            Some("2.54.2-suffix"),
            Some("bad.version.string"),
        ] {
            assert_eq!(version.and_then(parse_full_version), None, "{version:?}");
            assert!(!should_disable_explicit_sync(version, false), "{version:?}");
        }
    }

    #[test]
    fn desktop_list_contains_matches_case_insensitive_colon_separated_desktop() {
        assert!(desktop_list_contains("pop:GNOME", "gnome"));
        assert!(desktop_list_contains("Lubuntu:LXQt", "lxqt"));
        assert!(!desktop_list_contains("KDE", "GNOME"));
    }

    #[test]
    fn should_force_gdk_x11_matches_gnome_without_explicit_backend() {
        assert!(should_force_gdk_x11(false, Some("ubuntu:GNOME")));
        assert!(!should_force_gdk_x11(true, Some("GNOME")));
        assert!(!should_force_gdk_x11(false, Some("LXQt")));
        assert!(!should_force_gdk_x11(false, None));
    }

    #[test]
    fn is_lxqt_x11_session_matches_reported_lubuntu_environment() {
        assert!(is_lxqt_x11_session(
            Some("x11"),
            Some("LXQt"),
            Some("LXQt"),
            Some("lxqt-")
        ));
    }

    #[test]
    fn is_lxqt_x11_session_allows_wayland_and_other_desktops() {
        assert!(!is_lxqt_x11_session(
            Some("wayland"),
            Some("LXQt"),
            Some("LXQt"),
            Some("lxqt-")
        ));
        assert!(!is_lxqt_x11_session(
            Some("x11"),
            Some("GNOME"),
            Some("gnome"),
            Some("gnome-")
        ));
    }

    #[test]
    fn runs_under_xwayland_uses_selected_backend() {
        for display in [None, Some(""), Some("wayland-0"), Some("/tmp/wl.sock")] {
            assert!(runs_under_xwayland(
                Some("GdkX11Display"),
                Some("wayland"),
                display,
                false
            ));
            assert!(!runs_under_xwayland(
                Some("GdkWaylandDisplay"),
                Some("wayland"),
                display,
                false
            ));
        }
    }

    #[test]
    fn runs_under_xwayland_preserves_inherited_socket_hint() {
        assert!(runs_under_xwayland(Some("GdkX11Display"), None, None, true));
        assert!(!runs_under_xwayland(
            Some("GdkWaylandDisplay"),
            None,
            Some("stale"),
            true
        ));
        assert!(!runs_under_xwayland(
            Some("GdkWaylandDisplay"),
            None,
            None,
            true
        ));
    }

    #[test]
    fn runs_under_xwayland_ignores_native_and_unknown_displays() {
        assert!(!runs_under_xwayland(
            Some("GdkX11Display"),
            Some("x11"),
            None,
            false
        ));
        assert!(!runs_under_xwayland(
            Some("GdkX11Display"),
            None,
            Some(""),
            false
        ));
        for display_type in [None, Some("GdkBroadwayDisplay"), Some("GdkWaylandDisplay")] {
            assert!(!runs_under_xwayland(
                display_type,
                Some("wayland"),
                Some("wayland-0"),
                true
            ));
        }
        assert!(runs_under_xwayland(
            Some("GdkX11Display"),
            None,
            Some("wayland-0"),
            false
        ));
    }

    #[test]
    fn linux_renderer_workaround_precedes_configured_webviews() {
        let config: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert!(
            config["app"]["windows"].as_array().unwrap().is_empty(),
            "Configured windows initialize WebKit before the Linux workaround in setup"
        );
    }

    #[test]
    fn linux_graphics_phases_precede_gtk_and_first_webview() {
        let startup = include_str!("lib.rs");
        let before = startup.find("linux_graphics::before_gtk()").unwrap();
        let builder = startup.find("tauri::Builder::default()").unwrap();
        let after = startup.find("linux_graphics::after_gtk(").unwrap();
        let webview = after
            + startup[after..]
                .find("tauri::WebviewWindowBuilder::new(")
                .unwrap();
        assert!(before < builder);
        assert!(builder < after && after < webview);
    }

    #[test]
    fn dmabuf_disable_reason_prioritizes_existing_workarounds() {
        assert_eq!(
            dmabuf_disable_reason_for(false, false, false, Some("x11"), Some("GNOME"), None, None),
            None
        );
        assert_eq!(
            dmabuf_disable_reason_for(true, false, false, None, None, None, None),
            Some("legacy WebKitGTK renderer")
        );
        assert_eq!(
            dmabuf_disable_reason_for(
                false,
                true,
                true,
                Some("wayland"),
                Some("GNOME"),
                None,
                None
            ),
            Some("GNOME forced XWayland")
        );
        assert_eq!(
            dmabuf_disable_reason_for(false, false, false, Some("x11"), Some("LXQt"), None, None),
            Some("LXQt/X11 blank WebKit window workaround")
        );
    }

    #[test]
    fn dmabuf_disable_reason_covers_unforced_xwayland_fallback() {
        assert_eq!(
            dmabuf_disable_reason_for(false, false, true, Some("wayland"), Some("KDE"), None, None),
            Some("XWayland fallback")
        );
    }
}
