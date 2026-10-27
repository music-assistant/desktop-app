use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::Duration;
use tauri::menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::Manager;
use tauri_plugin_dialog::{
    DialogExt, MessageDialogButtons, MessageDialogKind, MessageDialogResult,
};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

#[cfg(target_os = "macos")]
mod applescript;
mod discord_rpc;
pub mod http_client;
mod i18n;
mod initial_navigation;
#[cfg(any(target_os = "linux", test))]
mod linux_graphics;
#[cfg(target_os = "linux")]
mod linux_theme;
mod logging;
mod ma_api;
mod mdns_discovery;
mod media_controls;
mod now_playing;
mod sendspin;
mod settings;

use mdns_discovery::DiscoveredServer;
use now_playing::NowPlaying;
use tauri_plugin_autostart::MacosLauncher;

static SERVICES_STARTER: Once = Once::new();
static STARTUP_UPDATE_CHECK: Once = Once::new();
static UPDATE_CHECK_STATE: Mutex<UpdateCheckState> = Mutex::new(UpdateCheckState::Idle);

// Global app handle for media controls callback
static APP_HANDLE: Mutex<Option<tauri::AppHandle>> = Mutex::new(None);

// Global tray icon reference for updating tooltip
static TRAY_ICON: Mutex<Option<TrayIcon>> = Mutex::new(None);

#[cfg(target_os = "macos")]
static TRAY_VISIBLE: AtomicBool = AtomicBool::new(false);

const MAIN_TRAY_ID: &str = "main-tray";

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

#[cfg(target_os = "linux")]
// Best-effort cached Linux panel preference for selecting a contrasting tray icon.
static LINUX_TRAY_ICON_DARK: AtomicBool = AtomicBool::new(false);

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

#[tauri::command]
fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

#[tauri::command]
fn is_macos() -> bool {
    cfg!(target_os = "macos")
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

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Distribution {
    Native,
    AppImage,
    Deb,
    Rpm,
    Flatpak,
    UnknownLinux,
}

fn distribution() -> Distribution {
    match option_env!("MUSIC_ASSISTANT_DISTRIBUTION") {
        Some("appimage") => Distribution::AppImage,
        Some("deb") => Distribution::Deb,
        Some("rpm") => Distribution::Rpm,
        Some("flatpak") => Distribution::Flatpak,
        _ => {
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            {
                Distribution::Native
            }
            #[cfg(target_os = "linux")]
            {
                Distribution::UnknownLinux
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
            {
                Distribution::Native
            }
        }
    }
}

const RELEASES_URL: &str = "https://github.com/music-assistant/desktop-app/releases/latest";
const DOCS_URL: &str = "https://music-assistant.io/";
/// Build the macOS menu bar with translated labels.
#[cfg(target_os = "macos")]
fn build_macos_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{Menu, SubmenuBuilder, HELP_SUBMENU_ID, WINDOW_SUBMENU_ID};

    let app_name = app.package_info().name.clone();

    // Labels that embed the app name, e.g. "Quit Music Assistant".
    let named = |key: &str| i18n::tr(key).replace("{0}", &app_name);
    let tr = i18n::tr;
    let about = MenuItemBuilder::with_id("about", named("desktop.mac_menu.about")).build(app)?;

    let preferences = MenuItemBuilder::with_id("app_preferences", tr("desktop.tray.preferences"))
        .accelerator("CmdOrCtrl+,")
        .build(app)?;

    let check_updates = MenuItemBuilder::with_id(
        "app_check_for_updates",
        tr("desktop.tray.check_for_updates"),
    )
    .build(app)?;

    let app_menu = SubmenuBuilder::new(app, &*app_name)
        .item(&about)
        .separator()
        .item(&preferences)
        .item(&check_updates)
        .separator()
        .item(&PredefinedMenuItem::services(
            app,
            Some(&tr("desktop.mac_menu.services")),
        )?)
        .separator()
        .item(&PredefinedMenuItem::hide(
            app,
            Some(&named("desktop.mac_menu.hide")),
        )?)
        .item(&PredefinedMenuItem::hide_others(
            app,
            Some(&tr("desktop.mac_menu.hide_others")),
        )?)
        .separator()
        .item(&PredefinedMenuItem::quit(
            app,
            Some(&named("desktop.mac_menu.quit")),
        )?)
        .build()?;

    let file_menu = SubmenuBuilder::new(app, tr("desktop.mac_menu.file"))
        .item(&PredefinedMenuItem::close_window(
            app,
            Some(&tr("desktop.mac_menu.close_window")),
        )?)
        .build()?;

    let edit_menu = SubmenuBuilder::new(app, tr("desktop.mac_menu.edit"))
        .item(&PredefinedMenuItem::undo(
            app,
            Some(&tr("desktop.mac_menu.undo")),
        )?)
        .item(&PredefinedMenuItem::redo(
            app,
            Some(&tr("desktop.mac_menu.redo")),
        )?)
        .separator()
        .item(&PredefinedMenuItem::cut(
            app,
            Some(&tr("desktop.mac_menu.cut")),
        )?)
        .item(&PredefinedMenuItem::copy(
            app,
            Some(&tr("desktop.mac_menu.copy")),
        )?)
        .item(&PredefinedMenuItem::paste(
            app,
            Some(&tr("desktop.mac_menu.paste")),
        )?)
        .item(&PredefinedMenuItem::select_all(
            app,
            Some(&tr("desktop.mac_menu.select_all")),
        )?)
        .build()?;

    let view_menu = SubmenuBuilder::new(app, tr("desktop.mac_menu.view"))
        .item(&PredefinedMenuItem::fullscreen(
            app,
            Some(&tr("desktop.mac_menu.fullscreen")),
        )?)
        .build()?;

    // The special IDs make Tauri register these with NSApp as the native
    // Window and Help menus (window list, help search field).
    let window_menu =
        SubmenuBuilder::with_id(app, WINDOW_SUBMENU_ID, tr("desktop.mac_menu.window"))
            .item(&PredefinedMenuItem::minimize(
                app,
                Some(&tr("desktop.mac_menu.minimize")),
            )?)
            .item(&PredefinedMenuItem::maximize(
                app,
                Some(&tr("desktop.mac_menu.zoom")),
            )?)
            .separator()
            .item(&PredefinedMenuItem::close_window(
                app,
                Some(&tr("desktop.mac_menu.close_window")),
            )?)
            .build()?;

    let help_docs =
        MenuItemBuilder::with_id("help_docs", named("desktop.mac_menu.help_docs")).build(app)?;
    let help_menu = SubmenuBuilder::with_id(app, HELP_SUBMENU_ID, tr("desktop.mac_menu.help"))
        .item(&help_docs)
        .build()?;

    Menu::with_items(
        app,
        &[
            &app_menu,
            &file_menu,
            &edit_menu,
            &view_menu,
            &window_menu,
            &help_menu,
        ],
    )
}

fn show_manual_update_instructions(app: &tauri::AppHandle) {
    let open_label = i18n::tr("desktop.updater.open_release");
    let result = app
        .dialog()
        .message(i18n::tr("desktop.updater.manual_update_message").replace("{0}", RELEASES_URL))
        .title(i18n::tr("desktop.updater.manual_update_title"))
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            open_label.clone(),
            i18n::tr("common.actions.cancel"),
        ))
        .blocking_show_with_result();

    if result == MessageDialogResult::Custom(open_label) {
        if let Err(error) = app.opener().open_url(RELEASES_URL, None::<&str>) {
            log::warn!("[Updater] Failed to open latest release: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateCheckMode {
    Startup,
    Manual,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateCheckResult {
    Error,
    NoUpdate,
    Available,
}

impl UpdateCheckMode {
    fn should_check(self, distribution: Distribution) -> bool {
        matches!(distribution, Distribution::Native | Distribution::AppImage)
            || matches!(self, Self::Manual)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateCheckState {
    Idle,
    Checking { manual_requested: bool },
    Presenting,
}

enum UpdateCheckRequest<'a> {
    Acquired(UpdateCheckGuard<'a>),
    Coalesced,
    Busy,
}

struct UpdateCheckGuard<'a> {
    state: &'a Mutex<UpdateCheckState>,
    armed: bool,
}

impl<'a> UpdateCheckGuard<'a> {
    fn request(
        state: &'a Mutex<UpdateCheckState>,
        mode: UpdateCheckMode,
    ) -> UpdateCheckRequest<'a> {
        let mut phase = state.lock().unwrap();
        match &mut *phase {
            UpdateCheckState::Idle => {
                *phase = UpdateCheckState::Checking {
                    manual_requested: mode == UpdateCheckMode::Manual,
                };
                UpdateCheckRequest::Acquired(Self { state, armed: true })
            }
            UpdateCheckState::Checking { manual_requested } if mode == UpdateCheckMode::Manual => {
                *manual_requested = true;
                UpdateCheckRequest::Coalesced
            }
            _ => UpdateCheckRequest::Busy,
        }
    }

    fn complete(&mut self, result: UpdateCheckResult) -> bool {
        let mut phase = self.state.lock().unwrap();
        let UpdateCheckState::Checking { manual_requested } = *phase else {
            unreachable!("update check must be checking before completion");
        };
        let notify = manual_requested || result == UpdateCheckResult::Available;
        if notify {
            *phase = UpdateCheckState::Presenting;
        } else {
            *phase = UpdateCheckState::Idle;
            self.armed = false;
        }
        notify
    }

    fn present(&mut self) {
        *self.state.lock().unwrap() = UpdateCheckState::Presenting;
    }
}

impl Drop for UpdateCheckGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            *self.state.lock().unwrap() = UpdateCheckState::Idle;
        }
    }
}

async fn check_for_updates(app: tauri::AppHandle, mode: UpdateCheckMode) {
    let distribution = distribution();
    if !mode.should_check(distribution) {
        // Package-managed Linux updates stay with the package manager; only manual checks show instructions.
        log::info!("[Updater] Skipping startup check for {distribution:?}");
        return;
    }

    let mut guard = match UpdateCheckGuard::request(&UPDATE_CHECK_STATE, mode) {
        UpdateCheckRequest::Acquired(guard) => guard,
        UpdateCheckRequest::Coalesced => {
            log::info!("[Updater] Manual check will use the pending update result");
            return;
        }
        UpdateCheckRequest::Busy => {
            log::info!("[Updater] Update check already in progress; skipping {mode:?} check");
            return;
        }
    };

    if !matches!(distribution, Distribution::Native | Distribution::AppImage) {
        guard.present();
        show_manual_update_instructions(&app);
        return;
    }

    log::info!("[Updater] Checking for updates ({distribution:?}, {mode:?})");

    let update = match app
        .updater_builder()
        .timeout(Duration::from_secs(15))
        .build()
    {
        Ok(updater) => match updater.check().await {
            Ok(update) => update,
            Err(error) => {
                log::warn!("[Updater] Update check failed: {error}");
                if guard.complete(UpdateCheckResult::Error) {
                    app.dialog()
                        .message(
                            i18n::tr("desktop.updater.check_failed_message")
                                .replace("{0}", &error.to_string()),
                        )
                        .title(i18n::tr("desktop.updater.check_failed_title"))
                        .kind(MessageDialogKind::Error)
                        .blocking_show();
                }
                return;
            }
        },
        Err(error) => {
            log::warn!("[Updater] Failed to initialize updater: {error}");
            if guard.complete(UpdateCheckResult::Error) {
                app.dialog()
                    .message(
                        i18n::tr("desktop.updater.check_failed_message")
                            .replace("{0}", &error.to_string()),
                    )
                    .title(i18n::tr("desktop.updater.check_failed_title"))
                    .kind(MessageDialogKind::Error)
                    .blocking_show();
            }
            return;
        }
    };

    let Some(update) = update else {
        log::info!("[Updater] No update available");
        if guard.complete(UpdateCheckResult::NoUpdate) {
            app.dialog()
                .message(i18n::tr("desktop.updater.no_updates_message"))
                .title(i18n::tr("desktop.updater.no_updates_title"))
                .kind(MessageDialogKind::Info)
                .blocking_show();
        }
        return;
    };

    guard.complete(UpdateCheckResult::Available);

    let should_install = app
        .dialog()
        .message(i18n::tr("desktop.updater.available_message").replace("{0}", &update.version))
        .title(i18n::tr("desktop.updater.available_title"))
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::YesNo)
        .blocking_show_with_result()
        == MessageDialogResult::Yes;

    if !should_install {
        log::info!("[Updater] User declined update {}", update.version);
        return;
    }

    log::info!(
        "[Updater] Downloading and installing update {}",
        update.version
    );
    match update.download_and_install(|_, _| {}, || {}).await {
        Ok(()) => {
            log::info!("[Updater] Update installed; restarting app");
            app.restart();
        }
        Err(error) => {
            log::warn!("[Updater] Failed to install update: {error}");
            app.dialog()
                .message(
                    i18n::tr("desktop.updater.install_failed_message")
                        .replace("{0}", &error.to_string()),
                )
                .title(i18n::tr("desktop.updater.install_failed_title"))
                .kind(MessageDialogKind::Error)
                .blocking_show();
        }
    }
}

#[tauri::command]
fn get_i18n_bundle() -> i18n::I18nBundle {
    i18n::bundle()
}

/// Called by the frontend to signal companion integration is ready.
///
/// Kept as a no-op for backward compatibility: the warning popup it gated has
/// been removed, but deployed server frontends still invoke this command, so
/// removing it entirely would log a harmless-but-noisy error on every connect.
#[tauri::command]
fn companion_ready() {}

/// Find the app window, whose label alternates when returning to the launcher.
fn current_app_window(app: &tauri::AppHandle) -> Option<tauri::WebviewWindow> {
    app.get_webview_window("main")
        .or_else(|| app.get_webview_window("launcher"))
}

#[cfg(any(test, target_os = "macos"))]
fn should_hide_dock(preference: bool, tray_available: bool, visible_windows: bool) -> bool {
    preference && tray_available && !visible_windows
}

#[cfg(any(test, target_os = "macos"))]
fn apply_tray_visibility_to<T, E>(
    tray: Option<T>,
    desired: impl FnOnce() -> bool,
    applied: &AtomicBool,
    set_visible: impl FnOnce(T, bool) -> Result<(), E>,
) -> Result<bool, E> {
    applied.store(false, Ordering::SeqCst);
    let Some(tray) = tray else {
        return Ok(false);
    };
    let desired = desired();
    set_visible(tray, desired)?;
    applied.store(desired, Ordering::SeqCst);
    Ok(true)
}

// Tao ignores a dock hide that arrives within 1s of any dock show, so a hide
// requested right after a show gets one delayed re-check. Each sync or show
// bumps this counter, so only the latest request's re-check may act.
#[cfg(target_os = "macos")]
static DOCK_SYNC_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(target_os = "macos")]
const DOCK_HIDE_RETRY_DELAY: Duration = Duration::from_millis(1100);

#[cfg(any(test, target_os = "macos"))]
fn next_dock_generation(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.fetch_add(1, Ordering::SeqCst).wrapping_add(1)
}

#[cfg(any(test, target_os = "macos"))]
fn is_current_dock_generation(counter: &std::sync::atomic::AtomicU64, generation: u64) -> bool {
    counter.load(Ordering::SeqCst) == generation
}

// Tao disables Cmd-H for every app window when hiding the Dock, but does not
// restore it when showing the Dock. All of these windows use AppKit's default YES.
#[cfg(target_os = "macos")]
fn restore_window_hiding() {
    let mtm = objc2::MainThreadMarker::new().expect("Dock updates run on the main thread");
    let application = objc2_app_kit::NSApplication::sharedApplication(mtm);
    for window in application.windows() {
        window.setCanHide(true);
    }
}

/// Applies the current dock policy and returns whether it asked to hide the icon.
#[cfg(target_os = "macos")]
fn sync_dock_visibility_now(app: &tauri::AppHandle) -> bool {
    let settings = settings::get_settings();
    let tray_available = settings.show_tray_icon && TRAY_VISIBLE.load(Ordering::SeqCst);
    let visible_windows = app
        .webview_windows()
        .values()
        .any(|window| window.is_visible().unwrap_or(true));
    let hide = should_hide_dock(
        settings.hide_dock_icon_when_hidden,
        tray_available,
        visible_windows,
    );
    match app.set_dock_visibility(!hide) {
        Ok(()) if !hide => restore_window_hiding(),
        Err(error) => log::warn!("[Dock] Could not update visibility: {error}"),
        _ => {}
    }
    hide
}

#[cfg(target_os = "macos")]
pub(crate) fn sync_dock_visibility(app: &tauri::AppHandle) {
    let generation = next_dock_generation(&DOCK_SYNC_GENERATION);
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || {
        if sync_dock_visibility_now(&handle) {
            schedule_dock_hide_recheck(handle, generation);
        }
    }) {
        log::warn!("[Dock] Could not schedule visibility update: {error}");
    }
}

/// Re-applies the current policy once, unless a newer sync or show superseded it.
#[cfg(target_os = "macos")]
fn schedule_dock_hide_recheck(app: tauri::AppHandle, generation: u64) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(DOCK_HIDE_RETRY_DELAY).await;
        let handle = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            if is_current_dock_generation(&DOCK_SYNC_GENERATION, generation) {
                sync_dock_visibility_now(&handle);
            }
        }) {
            log::warn!("[Dock] Could not schedule visibility re-check: {error}");
        }
    });
}

#[cfg(target_os = "macos")]
fn show_dock_before_window(app: &tauri::AppHandle) {
    next_dock_generation(&DOCK_SYNC_GENERATION);
    if let Err(error) = app.set_dock_visibility(true) {
        log::warn!("[Dock] Could not show icon before opening window: {error}");
        return;
    }
    if let Err(error) = app.run_on_main_thread(restore_window_hiding) {
        log::warn!("[Dock] Could not restore window hiding: {error}");
    }
}

fn show_app_window(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = current_app_window(app) {
        #[cfg(target_os = "macos")]
        show_dock_before_window(app);
        let _ = window.unminimize();
        let _ = window.show();
        if let Some(position) = take_hidden_window_position() {
            let _ = window.set_position(position);
        }
        let _ = window.set_focus();
    } else {
        replace_with_launcher(app, None, None, |_| true)?;
    }
    Ok(())
}

#[cfg(test)]
mod dock_policy_tests {
    use super::{
        apply_tray_visibility_to, is_current_dock_generation, next_dock_generation,
        should_hide_dock,
    };
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    #[test]
    fn tray_visibility_update_requires_app_handle_without_desktop_services() {
        let _: fn(tauri::AppHandle, bool) = super::set_tray_visible;
    }

    #[test]
    fn created_but_hidden_tray_does_not_let_dock_hide() {
        let applied = AtomicBool::new(false);
        assert!(!should_hide_dock(
            true,
            applied.load(Ordering::SeqCst),
            false
        ));
    }

    #[test]
    fn visible_tray_allows_dock_hide_with_no_visible_window() {
        let applied = AtomicBool::new(false);
        assert_eq!(
            apply_tray_visibility_to(
                Some(()),
                || true,
                &applied,
                |_, visible| {
                    assert!(visible);
                    Ok::<_, ()>(())
                }
            ),
            Ok(true)
        );
        assert!(should_hide_dock(
            true,
            applied.load(Ordering::SeqCst),
            false
        ));
        assert!(!should_hide_dock(
            false,
            applied.load(Ordering::SeqCst),
            false
        ));
    }

    #[test]
    fn queued_visibility_updates_read_latest_preference() {
        let applied = AtomicBool::new(false);
        let preference = AtomicBool::new(false);
        preference.store(true, Ordering::SeqCst);
        let queued = || {
            apply_tray_visibility_to(
                Some(()),
                || preference.load(Ordering::SeqCst),
                &applied,
                |_, visible| {
                    assert_eq!(visible, preference.load(Ordering::SeqCst));
                    Ok::<_, ()>(())
                },
            )
        };
        preference.store(false, Ordering::SeqCst);
        assert_eq!(queued(), Ok(true));
        assert!(!applied.load(Ordering::SeqCst));
        preference.store(true, Ordering::SeqCst);
        assert_eq!(queued(), Ok(true));
        assert!(applied.load(Ordering::SeqCst));
    }

    #[test]
    fn missing_tray_clears_prior_visibility() {
        let applied = AtomicBool::new(true);
        assert_eq!(
            apply_tray_visibility_to(None::<()>, || true, &applied, |_, _| Ok::<_, ()>(())),
            Ok(false)
        );
        assert!(!should_hide_dock(
            true,
            applied.load(Ordering::SeqCst),
            false
        ));
    }

    #[test]
    fn failed_native_update_does_not_mark_tray_visible() {
        let applied = AtomicBool::new(true);
        assert_eq!(
            apply_tray_visibility_to(Some(()), || true, &applied, |_, _| Err("native failure")),
            Err("native failure")
        );
        assert!(!should_hide_dock(
            true,
            applied.load(Ordering::SeqCst),
            false
        ));
    }

    #[test]
    fn hiding_and_resuming_tray_updates_dock_availability() {
        let applied = AtomicBool::new(false);
        for desired in [true, false, true] {
            assert_eq!(
                apply_tray_visibility_to(Some(()), || desired, &applied, |_, _| Ok::<_, ()>(())),
                Ok(true)
            );
            assert_eq!(
                should_hide_dock(true, applied.load(Ordering::SeqCst), false),
                desired
            );
        }
    }

    #[test]
    fn only_latest_generation_may_recheck() {
        let counter = AtomicU64::new(0);
        let first = next_dock_generation(&counter);
        assert!(is_current_dock_generation(&counter, first));
        let second = next_dock_generation(&counter);
        assert!(!is_current_dock_generation(&counter, first));
        assert!(is_current_dock_generation(&counter, second));
        // A dock show bumps the counter too, invalidating the pending re-check.
        next_dock_generation(&counter);
        assert!(!is_current_dock_generation(&counter, second));
    }

    #[test]
    fn only_hide_when_requested_and_tray_available_and_no_window_visible() {
        for preference in [false, true] {
            for tray_available in [false, true] {
                for visible_windows in [false, true] {
                    assert_eq!(
                        should_hide_dock(preference, tray_available, visible_windows),
                        preference && tray_available && !visible_windows
                    );
                }
            }
        }
    }
}

fn restore_app_window_geometry(window: &tauri::WebviewWindow) {
    use tauri_plugin_window_state::{StateFlags, WindowExt};
    let flags = StateFlags::all() & !StateFlags::VISIBLE;
    if let Err(error) = window.restore_state(flags) {
        log::warn!("[Launcher] Could not restore app window geometry: {error}");
    }
}

fn replace_with_launcher(
    app: &tauri::AppHandle,
    old_window: Option<tauri::WebviewWindow>,
    recovery: Option<&initial_navigation::Recovery>,
    may_replace: impl FnOnce(&tauri::WebviewWindow) -> bool,
) -> Result<bool, String> {
    let new_name = if app.get_webview_window("main").is_some() {
        "launcher"
    } else {
        "main"
    };
    let mut builder = apply_window_defaults(tauri::WebviewWindowBuilder::new(
        app,
        new_name,
        tauri::WebviewUrl::App("index.html".into()),
    ))
    .inner_size(1200.0, 800.0)
    .min_inner_size(600.0, 400.0);
    // Avoid the window-state plugin's restore_state: it holds its cache mutex while
    // querying the main thread, which can deadlock during background recovery.
    // Copy the current window's size and let the OS place the replacement.
    if let Some(old) = old_window.as_ref() {
        if let Ok(size) = old.inner_size() {
            if size.width > 0 && size.height > 0 {
                builder = builder.inner_size(f64::from(size.width), f64::from(size.height));
            }
        }
        if old.is_maximized().unwrap_or(false) {
            builder = builder.maximized(true);
        }
        if old.is_fullscreen().unwrap_or(false) {
            builder = builder.fullscreen(true);
        }
    }
    if let Some(recovery) = recovery {
        let encoded = serde_json::to_string(recovery).map_err(|e| e.to_string())?;
        builder = builder.initialization_script(format!(
            "if (window === window.top && (location.pathname === '/' || location.pathname === '/index.html') && (location.protocol === 'tauri:' && location.hostname === 'localhost' || /^https?:$/.test(location.protocol) && location.hostname === 'tauri.localhost' || location.protocol === 'http:' && location.hostname === 'localhost' && location.port === '1420')) window.__MA_INITIAL_RECOVERY__ = {encoded};"
        ));
    }
    let new_window = builder
        // Stay hidden until the swap is accepted, so a rejected replacement never flashes.
        .visible(false)
        .build()
        .map_err(|e| format!("Failed to create window: {e}"))?;
    if let Some(old) = old_window.as_ref() {
        if app
            .get_webview_window(old.label())
            .is_none_or(|active| active != *old)
            || !may_replace(old)
        {
            let _ = new_window.destroy();
            return Ok(false);
        }
    } else if recovery.is_some() || current_app_window(app).is_some() {
        let _ = new_window.destroy();
        return Ok(false);
    }
    enable_back_forward_navigation_gestures(&new_window);
    #[cfg(target_os = "macos")]
    show_dock_before_window(app);
    let _ = new_window.show();
    let _ = new_window.set_focus();
    #[cfg(target_os = "windows")]
    media_controls::rebind(window_hwnd(&new_window));
    if let Some(old) = old_window {
        let _ = old.destroy();
    }
    Ok(true)
}

/// Navigate back to the server selection screen (logout)
/// This clears the last server setting and recreates the window
#[tauri::command]
async fn navigate_to_launcher(app: tauri::AppHandle) -> Result<(), String> {
    initial_navigation::forget_selected_server()?;
    ma_api::clear_current_session();
    #[cfg(target_os = "macos")]
    applescript::cancel_pending();

    // Stop Sendspin if running and clear any frontend-reported playback state.
    // This also releases Windows sleep prevention when logging out before the
    // remote frontend has had a chance to publish its stopped state.
    sendspin::stop().await;
    now_playing::update_now_playing(NowPlaying::default());

    replace_with_launcher(&app, current_app_window(&app), None, |_| true)?;

    Ok(())
}

/// Extract the Win32 HWND from a webview window for media-controls binding.
#[cfg(target_os = "windows")]
fn window_hwnd(window: &tauri::WebviewWindow) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window.window_handle().ok().and_then(|handle| {
        if let RawWindowHandle::Win32(win32_handle) = handle.as_ref() {
            Some(win32_handle.hwnd.get() as *mut std::ffi::c_void)
        } else {
            None
        }
    })
}

/// Show and focus the main application window (used by the Linux MPRIS
/// `Raise` method, invoked when the user clicks the desktop's media widget).
/// Window operations are marshalled to the main thread: on Linux, GTK window
/// calls are not safe from arbitrary threads (here: the zbus service thread).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn raise_main_window() {
    if let Some(app) = APP_HANDLE.lock().unwrap().clone() {
        let handle = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Err(error) = show_app_window(&handle) {
                log::error!("[Window] Could not show application window: {error}");
            }
        });
    }
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

#[cfg(target_os = "macos")]
#[tauri::command]
fn complete_applescript_volume(
    webview: tauri::Webview,
    request_id: String,
    volume: Option<i32>,
    error: Option<String>,
) -> Result<(), String> {
    let app = webview.app_handle();
    let window = current_app_window(app).ok_or("Player window unavailable")?;
    let url = window.url().map_err(|err| err.to_string())?;
    let caller_url = webview.url().map_err(|err| err.to_string())?;
    if webview.label() != window.label() || caller_url != url || !is_player_origin(&url) {
        return Err("Volume reply came from an untrusted player page".to_string());
    }
    let origin = player_volume_origin(&url).ok_or("Connected player page unavailable")?;
    applescript::complete(&request_id, volume, error, origin);
    Ok(())
}

#[cfg(target_os = "macos")]
fn player_volume_origin(url: &tauri::Url) -> Option<String> {
    is_player_origin(url).then(|| url.origin().ascii_serialization())
}

#[cfg(target_os = "macos")]
fn is_player_origin(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && !(url.host_str() == Some("tauri.localhost")
            || url.host_str() == Some("localhost") && url.port() == Some(1420))
}

#[cfg(target_os = "macos")]
fn dispatch_volume_request(
    request_id: &str,
    operation: &str,
    volume: Option<u8>,
) -> Result<String, String> {
    let app = APP_HANDLE
        .lock()
        .unwrap()
        .clone()
        .ok_or("App unavailable")?;
    let window = current_app_window(&app).ok_or("Player window unavailable")?;
    let url = window.url().map_err(|err| err.to_string())?;
    let origin = player_volume_origin(&url).ok_or("Connected player page unavailable")?;
    let js = format!(
        "if (window === window.top && location.origin === {origin:?}) {{ const handler = window.__COMPANION_VOLUME_COMMAND__; if (typeof handler === 'function') {{ Promise.resolve().then(() => handler({request_id:?}, {operation:?}, {})).catch(() => {{}}); }} else {{ window.__TAURI__?.core.invoke('complete_applescript_volume', {{requestId:{request_id:?}, error:'Connected frontend does not support AppleScript volume'}}); }} }}",
        volume.map_or_else(|| "undefined".to_string(), |value| value.to_string()),
    );
    window.eval(js).map_err(|err| err.to_string())?;
    Ok(origin)
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

/// Forward a transport command to the web UI's player handler. Shared by the
/// OS media controls and, on macOS, the `AppleScript` bridge. `"toggle"`
/// resolves to `"pause"`/`"play"` from the current playback state.
fn dispatch_player_command(command: &str) {
    if let Some(ref app) = *APP_HANDLE.lock().unwrap() {
        if let Some(window) = current_app_window(app) {
            let cmd = if command == "toggle" {
                let np = now_playing::get_now_playing();
                if np.is_playing {
                    "pause"
                } else {
                    "play"
                }
            } else {
                command
            };
            let _ = window.eval(format!(
                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('{cmd}');",
            ));
        }
    }
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
        now_playing::init_power_management();

        // Get HWND for Windows media controls
        #[cfg(target_os = "windows")]
        let hwnd = {
            if let Some(ref app) = *APP_HANDLE.lock().unwrap() {
                current_app_window(app).as_ref().and_then(window_hwnd)
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

        // On macOS, let AppleScript drive the same player commands as the OS
        // media controls (e.g. `tell application "Music Assistant" to pause`).
        #[cfg(target_os = "macos")]
        applescript::init(
            Arc::new(|command| {
                log::info!("[AppleScript] command: {command}");
                dispatch_player_command(command);
            }),
            Arc::new(dispatch_volume_request),
            dispatch.clone(),
        );

        // Initialize media controls with callback for control events
        media_controls::init(
            Arc::new(|command| {
                // Info-level: this is the only trace at default log levels that
                // an OS media-key / widget press reached the app at all.
                log::info!("[MediaControls] OS media command: {command}");
                dispatch_player_command(command);
            }),
            hwnd,
            dispatch,
        );

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

pub fn set_tray_visible(app: tauri::AppHandle, visible: bool) {
    #[cfg(target_os = "macos")]
    {
        TRAY_VISIBLE.store(false, Ordering::SeqCst);
        let handle = app.clone();
        if let Err(error) = app.run_on_main_thread(move || {
            // Read the saved preference here so an older queued request cannot undo a newer one.
            match apply_tray_visibility_to(
                handle.tray_by_id(MAIN_TRAY_ID),
                || settings::get_settings().show_tray_icon,
                &TRAY_VISIBLE,
                |tray, show| tray.set_visible(show),
            ) {
                Ok(true) => {}
                Ok(false) => log::warn!("[Tray] Could not find tray icon for visibility update"),
                Err(error) => log::warn!("[Tray] Could not update visibility: {error}"),
            }
            sync_dock_visibility(&handle);
        }) {
            log::warn!("[Tray] Could not schedule visibility update: {error}");
            sync_dock_visibility(&app);
        }
    }
    #[cfg(not(target_os = "macos"))]
    if let Some(tray) = app.tray_by_id(MAIN_TRAY_ID) {
        let _ = tray.set_visible(visible);
    }
    #[cfg(target_os = "macos")]
    let _ = visible;
}

#[cfg(not(target_os = "linux"))]
fn decode_png_icon(png_bytes: &'static [u8]) -> tauri::image::Image<'static> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().expect("Failed to read PNG info");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("Failed to decode PNG");
    let rgba = buf[..info.buffer_size()].to_vec();
    tauri::image::Image::new_owned(rgba, info.width, info.height)
}

#[cfg(target_os = "linux")]
fn decode_linux_tray_icon(light: bool) -> tauri::image::Image<'static> {
    let decoder = png::Decoder::new(std::io::Cursor::new(include_bytes!(
        "../icons/tray-icon@2x.png"
    )));
    let mut reader = decoder
        .read_info()
        .expect("Failed to read Linux tray icon info");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .expect("Failed to decode Linux tray icon");
    let mut rgba = buf[..info.buffer_size()].to_vec();

    if light {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[0] = 255 - pixel[0];
            pixel[1] = 255 - pixel[1];
            pixel[2] = 255 - pixel[2];
        }
    }

    tauri::image::Image::new_owned(rgba, info.width, info.height)
}

fn load_tray_icon() -> tauri::image::Image<'static> {
    #[cfg(target_os = "macos")]
    let png_bytes = include_bytes!("../icons/tray-icon@2x.png");

    #[cfg(target_os = "linux")]
    return decode_linux_tray_icon(effective_linux_tray_icon_light());

    #[cfg(target_os = "windows")]
    let png_bytes = include_bytes!("../icons/32x32.png");

    #[cfg(not(target_os = "linux"))]
    decode_png_icon(png_bytes)
}

#[cfg(target_os = "linux")]
fn effective_linux_tray_icon_light() -> bool {
    match settings::get_settings().tray_icon_theme {
        settings::TrayIconTheme::Auto => LINUX_TRAY_ICON_DARK.load(Ordering::Relaxed),
        settings::TrayIconTheme::Light => true,
        settings::TrayIconTheme::Dark => false,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn refresh_linux_tray_icon() {
    if let Ok(tray_guard) = TRAY_ICON.try_lock() {
        if let Some(ref tray) = *tray_guard {
            let _ = tray.set_icon(Some(decode_linux_tray_icon(
                effective_linux_tray_icon_light(),
            )));
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn set_linux_tray_icon_dark(dark: bool) {
    LINUX_TRAY_ICON_DARK.store(dark, Ordering::Relaxed);
    refresh_linux_tray_icon();
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

fn show_tray_setting_save_failure(app: &tauri::AppHandle) {
    let open_label = i18n::tr("desktop.tray.open_settings");
    let handle = app.clone();
    app.dialog()
        .message(i18n::tr("desktop.tray.setting_save_failed_message"))
        .title(i18n::tr("desktop.tray.setting_save_failed_title"))
        .kind(MessageDialogKind::Error)
        .buttons(MessageDialogButtons::OkCancelCustom(
            open_label.clone(),
            i18n::tr("common.actions.cancel"),
        ))
        .show_with_result(move |result| {
            if result == MessageDialogResult::Custom(open_label) {
                let app = handle.clone();
                if let Err(error) = handle.run_on_main_thread(move || open_settings_window(&app)) {
                    log::error!("[Tray] Failed to open settings after save failure: {error}");
                }
            }
        });
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

        // Clone the handle before native calls, which may synchronously dispatch to the main thread.
        let tray = TRAY_ICON.try_lock().ok().and_then(|guard| guard.clone());
        if let Some(tray) = tray {
            // TODO: Remove unwrapping need when https://github.com/tauri-apps/tray-icon/issues/322 gets fixed
            let _ = tray.set_title(Some(title.as_deref().unwrap_or("")));
            let _ = tray.set_tooltip(Some(&tooltip));
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

/// Explain a settings load failure without exposing rejected configuration values.
#[tauri::command]
fn get_settings_load_error() -> Option<String> {
    settings::get_settings_load_error()
}

/// Set a single setting
#[tauri::command]
async fn set_setting(app: tauri::AppHandle, key: String, value: bool) -> Result<(), String> {
    settings::set_setting(app.clone(), &key, value)?;

    if key == "sendspin_enabled" && value {
        if let Err(error) = reconfigure_sendspin_from_current_session(app.clone()).await {
            // A lifecycle supersession is not a failed enable: a newer request
            // owns the desired state and will finish its own transition.
            if error == "Sendspin start superseded or disabled"
                && settings::get_settings().sendspin_enabled
            {
                return Ok(());
            }
            // set_setting persists and updates the runtime flag before the async
            // reconfiguration can run. Compensate if that later step fails so the
            // persisted setting and the UI remain truthful.
            if let Err(rollback_error) = settings::set_setting(app, &key, false) {
                log::error!("[Sendspin] Failed to roll back enabled setting: {rollback_error}");
            }
            return Err(error);
        }
    }

    Ok(())
}

async fn reconfigure_sendspin_from_current_session(app: tauri::AppHandle) -> Result<(), String> {
    let Some(session) = ma_api::current_session() else {
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
    ma_api::remember_session(server_base_url, auth_token);
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

/// Open or focus the dedicated About window.
fn open_about_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    show_dock_before_window(app);
    if let Some(window) = app.get_webview_window("about") {
        let _ = window.show();
        let _ = window.set_focus();
    } else {
        let _ = tauri::WebviewWindowBuilder::new(
            app,
            "about",
            tauri::WebviewUrl::App("about.html".into()),
        )
        .title(
            i18n::tr("desktop.mac_menu.about").replace("{0}", &i18n::tr("desktop.about.app_name")),
        )
        .inner_size(460.0, 350.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .center()
        .build();
    }
}

/// Open or focus the companion app's settings window.
fn open_settings_window(app: &tauri::AppHandle) {
    #[cfg(target_os = "macos")]
    show_dock_before_window(app);
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
        .on_navigation(|_url| {
            #[cfg(target_os = "macos")]
            applescript::cancel_pending();
            true
        })
        .on_page_load(|_window, payload| {
            #[cfg(target_os = "macos")]
            if matches!(payload.event(), tauri::webview::PageLoadEvent::Started) {
                applescript::cancel_pending();
            }
            #[cfg(not(target_os = "macos"))]
            let _ = payload;
        })
        .title(i18n::tr("desktop.app.name"))
        .resizable(true)
        .initialization_script(include_str!("../resources/clipboard-polyfill.js"))
        .initialization_script(include_str!("../resources/mouse-navigation.js"))
        .initialization_script(initial_navigation::DOCUMENT_READY_SCRIPT);
    builder
}

/// Enable trackpad two-finger swipe to go back/forward, the way browsers do.
/// `WKWebView` keeps this gesture off by default.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // objc2 message sends are all `unsafe`; lift the crate deny.
fn enable_back_forward_navigation_gestures(window: &tauri::WebviewWindow) {
    let result = window.with_webview(|webview| {
        // `inner()` is the `WKWebView`; set its
        // `allowsBackForwardNavigationGestures` property.
        let wk = webview.inner().cast::<objc2::runtime::AnyObject>();
        unsafe {
            let _: () = objc2::msg_send![
                wk,
                setAllowsBackForwardNavigationGestures: objc2::runtime::Bool::new(true)
            ];
        }
    });
    if let Err(error) = result {
        log::warn!("[Navigation] Failed to enable swipe gestures: {error}");
    }
}

#[cfg(not(target_os = "macos"))]
fn enable_back_forward_navigation_gestures(_window: &tauri::WebviewWindow) {}

pub fn run() {
    #[cfg(target_os = "linux")]
    let early_graphics = linux_graphics::before_gtk();

    let context = tauri::generate_context!();
    let mut builder = tauri::Builder::default();

    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        if let Err(error) = show_app_window(app) {
            log::error!("[Window] Could not restore application window: {error}");
        }
    }));

    builder = builder.plugin(
        tauri_plugin_window_state::Builder::new()
            .skip_initial_state("main")
            .skip_initial_state("launcher")
            .build(),
    );

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
            is_linux,
            is_macos,
            get_app_version,
            get_i18n_bundle,
            initial_navigation::reserve_initial_navigation,
            initial_navigation::begin_initial_navigation,
            initial_navigation::cancel_initial_navigation,
            initial_navigation::acknowledge_initial_navigation,
            companion_ready,
            navigate_to_launcher,
            get_now_playing,
            update_now_playing,
            #[cfg(target_os = "macos")]
            complete_applescript_volume,
            start_desktop_services,
            start_discord_rpc,
            start_rpc,
            discover_servers,
            get_settings,
            get_settings_load_error,
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
                // Server switching and logout alternate between these window labels.
                if matches!(window.label(), "main" | "launcher")
                    && settings::get_settings().close_to_tray
                {
                    if let Ok(pos) = window.outer_position() {
                        stash_window_position(pos);
                    }
                    let _ = window.hide();
                    api.prevent_close();
                    #[cfg(target_os = "macos")]
                    sync_dock_visibility(window.app_handle());
                }
            }
            #[cfg(target_os = "macos")]
            if let tauri::WindowEvent::Destroyed = event {
                sync_dock_visibility(window.app_handle());
            }
        })
        .setup(move |app| {
            // Tao has initialized GTK and selected its display, but this app
            // creates no configured windows. Wry creates WebKit contexts lazily
            // with the first webview below. Keep this before plugins/services
            // that might create a webview; do not call gtk::init early ourselves.
            #[cfg(target_os = "linux")]
            let linux_webview_workarounds = linux_graphics::after_gtk(early_graphics);

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
            if let Some(error) = settings::get_settings_load_error() {
                log::error!("[Settings] {error} Restart the app after repairing or removing the settings file before saving settings.");
            }
            i18n::init(app.handle());

            // Replace Tauri's default (hardcoded-English) menu bar with one
            // built from translated labels
            #[cfg(target_os = "macos")]
            app.set_menu(build_macos_menu(app.handle())?)?;

            log::info!(
                "[App] Music Assistant Companion v{} starting (debug logging: {})",
                app.package_info().version,
                if loaded_settings.debug_logging {
                    "on"
                } else {
                    "off"
                }
            );

            #[cfg(target_os = "linux")]
            linux_webview_workarounds.log();

            // GNOME & co. express dark mode via the xdg-desktop-portal
            // color-scheme setting, which GTK3/WebKitGTK don't read on their
            // own; track it and apply it as the window theme.
            #[cfg(target_os = "linux")]
            linux_theme::init(app.handle().clone());

            // Create main window (companion bridge + clipboard polyfill applied
            // via apply_window_defaults; runs on every page load, including the
            // remote MA frontend loaded via window.location.href).
            let main_window = apply_window_defaults(tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            ))
            .inner_size(800.0, 600.0)
            .zoom_hotkeys_enabled(true)
            .build()?;

            restore_app_window_geometry(&main_window);
            enable_back_forward_navigation_gestures(&main_window);

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
            let about = MenuItemBuilder::with_id(
                "tray_about",
                i18n::tr("desktop.mac_menu.about").replace(
                    "{0}",
                    &app.package_info().name,
                ),
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
                    &about,
                    &separator4,
                    &quit,
                ])
                .build()?;

            let tray_icon = load_tray_icon();

            let tray = TrayIconBuilder::with_id(MAIN_TRAY_ID)
                .menu(&menu)
                .tooltip("Music Assistant")
                .icon(tray_icon)
                .icon_as_template(cfg!(target_os = "macos"))
                .on_menu_event(move |app, event| {
                  log::debug!("[Tray] Menu item clicked: {}", event.id().as_ref());
                  match event.id().as_ref() {
                    "quit" => {
                        app.exit(0);
                    }
                    "hide" => {
                        if let Some(window) = current_app_window(app) {
                            if let Ok(pos) = window.outer_position() {
                                stash_window_position(pos);
                            }
                            let _ = window.hide();
                            #[cfg(target_os = "macos")]
                            sync_dock_visibility(app);
                        }
                    }
                    "show" | "now_playing" => {
                        if let Err(error) = show_app_window(app) {
                            log::error!("[Window] Could not show application window: {error}");
                        }
                    }
                    "switch_server" => {
                        if let Err(e) = initial_navigation::forget_selected_server() {
                            log::error!("[Launcher] Could not clear selected server: {e}");
                            return;
                        }
                        ma_api::clear_current_session();

                        // Stop Sendspin client
                        tauri::async_runtime::spawn(async {
                            sendspin::stop().await;
                        });

                        if let Err(e) = replace_with_launcher(app, current_app_window(app), None, |_| true) {
                            log::error!("[Launcher] Could not switch server: {e}");
                        }
                    }
                    "play_pause" => {
                        // Call frontend function to control active player
                        let np = now_playing::get_now_playing();
                        let cmd = if np.is_playing { "pause" } else { "play" };
                        if let Some(window) = current_app_window(app) {
                            let _ = window.eval(format!(
                                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('{cmd}');",
                            ));
                        }
                    }
                    "prev_track" => {
                        if let Some(window) = current_app_window(app) {
                            let _ = window.eval(
                                "window.__COMPANION_PLAYER_COMMAND__ && window.__COMPANION_PLAYER_COMMAND__('previous');"
                            );
                        }
                    }
                    "next_track" => {
                        if let Some(window) = current_app_window(app) {
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
                            // Native check menu items toggle before this callback. A failed
                            // save leaves the runtime unchanged, so restore its actual state.
                            set_discord_rpc_tray_checked(DISCORD_RPC_ENABLED.load(Ordering::SeqCst));
                            show_tray_setting_save_failure(app);
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
                    "tray_about" => open_about_window(app),
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
                        tauri::async_runtime::spawn(check_for_updates(handle, UpdateCheckMode::Manual));
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
                        if let Err(error) = show_app_window(app) {
                            log::error!("[Window] Could not show application window: {error}");
                        }
                    }
                })
                .build(app)?;

            // Store tray reference for tooltip updates
            if let Ok(mut tray_guard) = TRAY_ICON.lock() {
                *tray_guard = Some(tray);
            }

            // Explicitly apply the initial preference before allowing Dock hiding.
            #[cfg(target_os = "macos")]
            set_tray_visible(app.handle().clone(), settings::get_settings().show_tray_icon);
            #[cfg(target_os = "macos")]
            sync_dock_visibility(app.handle());

            // Kept for a follow-up that adds a menu bar on non-macOS platforms
            #[cfg(not(target_os = "macos"))]
            if let Some(menu) = app.menu() {
                let target = menu.items()?.into_iter().find_map(|item| match item {
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
                    submenu.append(&separator)?;
                    submenu.append(&prefs)?;
                }
            }
            app.on_menu_event(move |app, event| {
                match event.id().as_ref() {
                    "app_preferences" => open_settings_window(app),
                    "app_check_for_updates" => {
                        tauri::async_runtime::spawn(check_for_updates(app.clone(), UpdateCheckMode::Manual));
                    }
                    "about" => open_about_window(app),
                    "help_docs" => {
                        if let Err(error) = app.opener().open_url(DOCS_URL, None::<&str>) {
                            log::warn!("[App] Failed to open documentation: {error}");
                        }
                    }
                    _ => {}
                }
            });

            STARTUP_UPDATE_CHECK.call_once(|| {
                tauri::async_runtime::spawn(check_for_updates(
                    app.handle().clone(),
                    UpdateCheckMode::Startup,
                ));
            });

            Ok(())
        })
        .build(context)
        .expect("Error while building Music Assistant companion")
        .run(|app, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { has_visible_windows, .. } = event {
                if !has_visible_windows {
                    if let Err(error) = show_app_window(app) {
                        log::error!("[Window] Could not restore application window: {error}");
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
    fn startup_update_check_is_quiet_without_an_update() {
        for result in [UpdateCheckResult::Error, UpdateCheckResult::NoUpdate] {
            let state = Mutex::new(UpdateCheckState::Idle);
            let UpdateCheckRequest::Acquired(mut guard) =
                UpdateCheckGuard::request(&state, UpdateCheckMode::Startup)
            else {
                panic!("startup check should start");
            };
            assert!(!guard.complete(result));
            assert_eq!(*state.lock().unwrap(), UpdateCheckState::Idle);
        }
    }

    #[test]
    fn manual_request_promotes_pending_startup_result() {
        for result in [
            UpdateCheckResult::Error,
            UpdateCheckResult::NoUpdate,
            UpdateCheckResult::Available,
        ] {
            let state = Mutex::new(UpdateCheckState::Idle);
            let UpdateCheckRequest::Acquired(mut guard) =
                UpdateCheckGuard::request(&state, UpdateCheckMode::Startup)
            else {
                panic!("startup check should start");
            };
            assert!(matches!(
                UpdateCheckGuard::request(&state, UpdateCheckMode::Manual),
                UpdateCheckRequest::Coalesced
            ));
            assert!(matches!(
                UpdateCheckGuard::request(&state, UpdateCheckMode::Manual),
                UpdateCheckRequest::Coalesced
            ));
            assert!(guard.complete(result));
            assert_eq!(*state.lock().unwrap(), UpdateCheckState::Presenting);
            assert!(matches!(
                UpdateCheckGuard::request(&state, UpdateCheckMode::Manual),
                UpdateCheckRequest::Busy
            ));
            drop(guard);
            assert_eq!(*state.lock().unwrap(), UpdateCheckState::Idle);
        }
    }

    #[test]
    fn available_update_presents_without_manual_request() {
        let state = Mutex::new(UpdateCheckState::Idle);
        let UpdateCheckRequest::Acquired(mut guard) =
            UpdateCheckGuard::request(&state, UpdateCheckMode::Startup)
        else {
            panic!("startup check should start");
        };
        assert!(guard.complete(UpdateCheckResult::Available));
        assert_eq!(*state.lock().unwrap(), UpdateCheckState::Presenting);
    }

    #[test]
    fn manual_update_check_notifies_for_all_results() {
        for result in [
            UpdateCheckResult::Error,
            UpdateCheckResult::NoUpdate,
            UpdateCheckResult::Available,
        ] {
            let state = Mutex::new(UpdateCheckState::Idle);
            let UpdateCheckRequest::Acquired(mut guard) =
                UpdateCheckGuard::request(&state, UpdateCheckMode::Manual)
            else {
                panic!("manual check should start");
            };
            assert!(guard.complete(result));
        }
    }

    #[test]
    fn update_check_distribution_policy() {
        for distribution in [Distribution::Native, Distribution::AppImage] {
            assert!(UpdateCheckMode::Startup.should_check(distribution));
            assert!(UpdateCheckMode::Manual.should_check(distribution));
        }
        for distribution in [
            Distribution::Deb,
            Distribution::Rpm,
            Distribution::Flatpak,
            Distribution::UnknownLinux,
        ] {
            assert!(!UpdateCheckMode::Startup.should_check(distribution));
            assert!(UpdateCheckMode::Manual.should_check(distribution));
        }
    }

    #[test]
    fn startup_checks_do_not_overlap_and_cancelled_checks_release_state() {
        let state = Mutex::new(UpdateCheckState::Idle);
        let UpdateCheckRequest::Acquired(guard) =
            UpdateCheckGuard::request(&state, UpdateCheckMode::Startup)
        else {
            panic!("first check should start");
        };
        assert!(matches!(
            UpdateCheckGuard::request(&state, UpdateCheckMode::Startup),
            UpdateCheckRequest::Busy
        ));
        drop(guard);
        assert_eq!(*state.lock().unwrap(), UpdateCheckState::Idle);
        assert!(matches!(
            UpdateCheckGuard::request(&state, UpdateCheckMode::Startup),
            UpdateCheckRequest::Acquired(_)
        ));
    }

    #[test]
    fn silent_completion_does_not_release_newer_manual_check() {
        let state = Mutex::new(UpdateCheckState::Idle);
        let UpdateCheckRequest::Acquired(mut startup) =
            UpdateCheckGuard::request(&state, UpdateCheckMode::Startup)
        else {
            panic!("startup check should start");
        };
        assert!(!startup.complete(UpdateCheckResult::NoUpdate));
        let UpdateCheckRequest::Acquired(manual) =
            UpdateCheckGuard::request(&state, UpdateCheckMode::Manual)
        else {
            panic!("manual check after a quiet result should start");
        };
        drop(startup);
        assert_eq!(
            *state.lock().unwrap(),
            UpdateCheckState::Checking {
                manual_requested: true
            }
        );
        drop(manual);
        assert_eq!(*state.lock().unwrap(), UpdateCheckState::Idle);
    }

    #[test]
    fn manual_instructions_suppress_requests_while_presenting() {
        let state = Mutex::new(UpdateCheckState::Idle);
        let UpdateCheckRequest::Acquired(mut guard) =
            UpdateCheckGuard::request(&state, UpdateCheckMode::Manual)
        else {
            panic!("manual instructions should start");
        };
        guard.present();
        assert!(matches!(
            UpdateCheckGuard::request(&state, UpdateCheckMode::Manual),
            UpdateCheckRequest::Busy
        ));
        drop(guard);
        assert_eq!(*state.lock().unwrap(), UpdateCheckState::Idle);
    }

    #[test]
    fn tray_discord_save_failure_restores_state_and_offers_settings() {
        let source = include_str!("lib.rs");
        let handler = source
            .split_once("\"discord_rpc\" => {")
            .unwrap()
            .1
            .split_once("\"settings\" =>")
            .unwrap()
            .0;
        assert!(handler.contains("settings::set_setting"));
        assert!(handler
            .contains("set_discord_rpc_tray_checked(DISCORD_RPC_ENABLED.load(Ordering::SeqCst))"));
        assert!(handler.contains("show_tray_setting_save_failure(app)"));
        assert!(handler.contains("refresh_settings_window(app)"));
    }

    #[test]
    fn settings_load_status_is_available_only_to_local_settings_window() {
        let settings: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/settings.json")).unwrap();
        assert_eq!(settings["windows"], serde_json::json!(["settings"]));
        assert!(settings["permissions"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("allow-get-settings-load-error")));
        for capability in [
            include_str!("../capabilities/desktop.json"),
            include_str!("../capabilities/launcher.json"),
            include_str!("../capabilities/default.json"),
        ] {
            assert!(!capability.contains("allow-get-settings-load-error"));
        }
    }

    #[test]
    fn desktop_capability_grants_updater_and_zoom_to_both_app_windows() {
        let capability: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/desktop.json")).unwrap();
        let windows = capability["windows"].as_array().unwrap();
        for label in ["main", "launcher"] {
            assert!(windows.contains(&serde_json::json!(label)));
        }
        let permissions = capability["permissions"].as_array().unwrap();
        for permission in ["updater:default", "core:webview:allow-set-webview-zoom"] {
            assert!(permissions.contains(&serde_json::json!(permission)));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn connected_windows_can_complete_applescript_volume() {
        for (label, capability) in [
            ("main", include_str!("../capabilities/default.json")),
            ("launcher", include_str!("../capabilities/launcher.json")),
        ] {
            let capability: serde_json::Value = serde_json::from_str(capability).unwrap();
            assert!(capability["windows"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!(label)));
            assert!(capability["permissions"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("allow-complete-applescript-volume")));
            assert_eq!(
                capability["remote"]["urls"],
                serde_json::json!(["http://*:*", "https://*:*"])
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn volume_reply_binding_uses_origin_not_route() {
        let original = tauri::Url::parse("https://player.example:8443/music?track=1#play").unwrap();
        let route =
            tauri::Url::parse("https://player.example:8443/settings?track=2#queue").unwrap();
        let binding = player_volume_origin(&original).unwrap();
        assert_eq!(binding, player_volume_origin(&route).unwrap());
        for other in [
            "https://player.example:9443/music",
            "http://player.example:8443/music",
            "https://elsewhere.example:8443/music",
        ] {
            assert_ne!(
                binding,
                player_volume_origin(&tauri::Url::parse(other).unwrap()).unwrap()
            );
        }
    }

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
