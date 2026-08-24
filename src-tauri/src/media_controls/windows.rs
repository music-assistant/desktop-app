//! Native Windows media-controls backend using System Media Transport Controls.
#![allow(unsafe_code)] // `GetForWindow` is a WinRT interop call.

use super::{MainThreadDispatch, MediaControlCallback, NowPlayingPlan, PlaybackState};
use crate::now_playing::NowPlaying;
use parking_lot::{Mutex, ReentrantMutex};
use std::cell::RefCell;
use std::ffi::c_void;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;
use windows::core::{factory, w, Ref, HSTRING};
use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
    SystemMediaTransportControls, SystemMediaTransportControlsButton,
    SystemMediaTransportControlsButtonPressedEventArgs, SystemMediaTransportControlsDisplayUpdater,
    SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::{
    DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RPC_E_CHANGED_MODE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, DeleteObject, GetSysColor, COLOR_BTNTEXT, HGDIOBJ,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, RemoveWindowSubclass, SetWindowSubclass, TaskbarList,
    THBF_DISABLED, THBF_ENABLED, THBN_CLICKED, THB_FLAGS, THB_ICON, THB_TOOLTIP, THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, RegisterWindowMessageW, SystemParametersInfoW, HICON,
    ICONINFO, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_COMMAND,
    WM_SETTINGCHANGE, WM_SYSCOLORCHANGE, WM_THEMECHANGED,
};

/// All access happens on the Tauri UI thread (via `dispatch`), but that thread
/// is an STA: any SMTC/COM call may pump window messages, and the taskbar
/// subclass proc then re-enters this module *while an update is in progress*.
/// A `ReentrantMutex` + `RefCell` makes that re-entrancy explicit: the nested
/// call relocks fine (where a plain `Mutex` would deadlock), and
/// `with_state_mut` detects the still-active mutable borrow and defers.
static STATE: ReentrantMutex<RefCell<Option<WindowsMediaControls>>> =
    ReentrantMutex::new(RefCell::new(None));
static CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);
static DISPATCH: Mutex<Option<MainThreadDispatch>> = Mutex::new(None);

/// Events that could not run because the state was mutably borrowed at the
/// time (re-entrant message delivery during an update). Producers set flags;
/// [`with_state_mut`] drains them once the active borrow ends, so nothing is
/// permanently lost — a dropped `TaskbarButtonCreated` in particular would
/// otherwise lose the thumbbar buttons for the whole Explorer session.
#[derive(Default)]
struct PendingEvents {
    taskbar_created: bool,
    theme_changed: bool,
    reassert_timeline: bool,
}

impl PendingEvents {
    fn is_empty(&self) -> bool {
        !(self.taskbar_created || self.theme_changed || self.reassert_timeline)
    }
}

static PENDING_EVENTS: Mutex<PendingEvents> = Mutex::new(PendingEvents {
    taskbar_created: false,
    theme_changed: false,
    reassert_timeline: false,
});

/// The registered "`TaskbarButtonCreated`" message id, mirrored into a static so
/// the subclass proc can recognize the message without touching STATE (the
/// message can arrive re-entrantly while STATE is borrowed). Zero = unset;
/// `RegisterWindowMessageW` ids are process-wide constants, so mirroring is
/// safe across rebinds.
static TASKBAR_CREATED_MSG: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Bundled fallback artwork shown when the current item has no cover.
const DEFAULT_ART_PNG: &[u8] = include_bytes!("../../resources/logo.png");

const THUMB_PREVIOUS_ID: u32 = 1;
const THUMB_PLAY_PAUSE_ID: u32 = 2;
const THUMB_NEXT_ID: u32 = 3;
const THUMB_SUBCLASS_ID: usize = 0x4d41_5442;
const ICON_SIZE: usize = 16;
const PREVIOUS_MASK_PNG: &[u8] =
    include_bytes!("../../resources/windows-thumbbar/skip-back-mask.png");
const PLAY_MASK_PNG: &[u8] = include_bytes!("../../resources/windows-thumbbar/play-mask.png");
const PAUSE_MASK_PNG: &[u8] = include_bytes!("../../resources/windows-thumbbar/pause-mask.png");
const NEXT_MASK_PNG: &[u8] =
    include_bytes!("../../resources/windows-thumbbar/skip-forward-mask.png");

struct WindowsMediaControls {
    controls: SystemMediaTransportControls,
    display_updater: SystemMediaTransportControlsDisplayUpdater,
    timeline: SystemMediaTransportControlsTimelineProperties,
    button_token: i64,
    position_token: i64,
    thumbbar: Option<TaskbarThumbnailControls>,
    last_metadata: Option<MetadataKey>,
    /// Whether the display is already in the cleared/Stopped state, so
    /// repeated Stopped updates don't re-run the full ClearAll/Update cycle.
    cleared: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThumbIconTheme {
    DarkShell,
    LightShell,
    HighContrast(u32),
}

#[derive(Clone, PartialEq, Eq)]
struct MetadataKey {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    image_url: Option<String>,
}

impl MetadataKey {
    fn from_plan(plan: &NowPlayingPlan) -> Self {
        Self {
            title: plan.title.clone(),
            artist: plan.artist.clone(),
            album: plan.album.clone(),
            image_url: plan.image_url.clone(),
        }
    }
}

pub fn init(
    callback: MediaControlCallback,
    hwnd_param: Option<*mut c_void>,
    main_dispatch: MainThreadDispatch,
) {
    *CALLBACK.lock() = Some(callback);
    *DISPATCH.lock() = Some(main_dispatch);

    let Some(hwnd) = hwnd_param else {
        log::error!("[MediaControls] Disabled on Windows (no HWND available)");
        return;
    };

    let hwnd_addr = hwnd as usize;
    dispatch(move || {
        if STATE.lock().borrow().is_some() {
            return;
        }

        match WindowsMediaControls::new(hwnd_addr as *mut c_void) {
            Ok(mut controls) => {
                render_current_state(&mut controls);
                // `new()` makes COM calls that can pump messages; a nested
                // pump could have run a rebind meanwhile. Never overwrite a
                // newer binding with one made for a possibly-dying window.
                let guard = STATE.lock();
                let mut slot = guard.borrow_mut();
                if slot.is_none() {
                    *slot = Some(controls);
                } else {
                    log::debug!(
                        "[MediaControls] Windows SMTC already bound during init; keeping newer binding"
                    );
                }
            }
            Err(e) => {
                log::error!("[MediaControls] Failed to initialize Windows SMTC: {e:?}");
            }
        }
    });
}

/// Render the current now-playing state onto freshly bound controls, so the
/// flyout and button enablement don't stay blank/disabled until the next
/// frontend push (which can be a while away when paused or idle).
fn render_current_state(controls: &mut WindowsMediaControls) {
    let np = crate::now_playing::get_now_playing();
    let plan = super::plan(&np);
    if let Err(e) = controls.update(&plan) {
        log::warn!("[MediaControls] Failed to render onto freshly bound SMTC: {e:?}");
    }
}

/// Re-bind SMTC (and the taskbar thumbnail controls) to a new native window.
///
/// The SMTC instance obtained through `GetForWindow` lives and dies with its
/// HWND. Logout and server-switch flows destroy the window we bound at init
/// and create a replacement — without re-binding, media keys and the SMTC
/// flyout silently stop working for the rest of the process lifetime.
/// No-op until [`init`] has run (init will bind to the then-current window).
pub fn rebind(hwnd_param: Option<*mut c_void>) {
    if CALLBACK.lock().is_none() {
        return;
    }
    let Some(hwnd) = hwnd_param else {
        log::warn!("[MediaControls] Cannot re-bind Windows SMTC (no HWND available)");
        return;
    };

    let hwnd_addr = hwnd as usize;
    dispatch(move || {
        // Tear the old binding down outside any active borrow: Drop makes COM
        // calls that can pump messages and re-enter this module. This may run
        // after the old window is already destroyed, which is fine: the SMTC
        // COM object is decoupled from the HWND, and `RemoveWindowSubclass`
        // on a dead HWND is a benign no-op (comctl32 detaches subclasses at
        // WM_NCDESTROY).
        let old = STATE.lock().borrow_mut().take();
        drop(old);

        match WindowsMediaControls::new(hwnd_addr as *mut c_void) {
            Ok(mut controls) => {
                render_current_state(&mut controls);
                *STATE.lock().borrow_mut() = Some(controls);
            }
            Err(e) => {
                log::error!("[MediaControls] Failed to re-bind Windows SMTC: {e:?}");
            }
        }
    });
}

pub fn update(np: &NowPlaying) {
    let plan = super::plan(np);
    dispatch(move || {
        with_state_mut(|controls| {
            if let Err(e) = controls.update(&plan) {
                log::error!("[MediaControls] Failed to update Windows SMTC: {e:?}");
            }
        });
    });
}

pub fn clear() {
    dispatch(|| {
        with_state_mut(|controls| {
            if let Err(e) = controls.clear() {
                log::warn!("[MediaControls] Failed to clear Windows SMTC: {e:?}");
            }
        });
    });
}

fn dispatch(f: impl FnOnce() + Send + 'static) {
    let Some(dispatch) = DISPATCH.lock().clone() else {
        return;
    };
    dispatch(Box::new(f));
}

fn with_state_mut(f: impl FnOnce(&mut WindowsMediaControls)) {
    let guard = STATE.lock();
    let borrow = guard.try_borrow_mut();
    match borrow {
        Ok(mut slot) => {
            if let Some(controls) = slot.as_mut() {
                f(controls);
            }
            // Drain events that were recorded (instead of handled) because
            // they arrived re-entrantly while a borrow was active — including
            // any recorded during `f` itself just now.
            loop {
                let pending = std::mem::take(&mut *PENDING_EVENTS.lock());
                if pending.is_empty() {
                    break;
                }
                let Some(controls) = slot.as_mut() else {
                    break;
                };
                if pending.taskbar_created {
                    if let Some(thumbbar) = controls.thumbbar.as_mut() {
                        thumbbar.on_taskbar_button_created();
                    }
                }
                if pending.theme_changed {
                    if let Some(thumbbar) = controls.thumbbar.as_mut() {
                        thumbbar.refresh_icon_theme();
                    }
                }
                if pending.reassert_timeline {
                    if let Err(e) = controls
                        .controls
                        .UpdateTimelineProperties(&controls.timeline)
                    {
                        log::warn!(
                            "[MediaControls] Failed to re-assert Windows SMTC position: {e:?}"
                        );
                    }
                }
            }
        }
        // Re-entrant access while an update is already borrowing the state
        // (STA message pumping). Callers that must not lose their event have
        // recorded it in PENDING_EVENTS; the outer holder drains it.
        Err(_) => {
            log::debug!("[MediaControls] Deferred re-entrant Windows SMTC state access");
        }
    }
}

impl WindowsMediaControls {
    fn new(hwnd: *mut c_void) -> windows::core::Result<Self> {
        // Tauri/Wry initializes COM for the UI thread in normal operation. If
        // it has not yet done so, initialize this long-lived UI thread as STA
        // before the first WinRT factory/interop call below (rather than
        // relying on the host having beaten us to it); we intentionally do not
        // CoUninitialize during process lifetime. Ignore RPC_E_CHANGED_MODE
        // because an existing MTA is still usable.
        let init_result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if init_result.is_err() && init_result != RPC_E_CHANGED_MODE {
            log::debug!("[MediaControls] CoInitializeEx for SMTC returned {init_result:?}");
        }

        let interop: ISystemMediaTransportControlsInterop =
            factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
        // Tauri gives us a Win32 HWND, not a UWP/CoreWindow view. SMTC needs the
        // interop API to attach media controls to that desktop window.
        let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(HWND(hwnd)) }?;
        let display_updater = controls.DisplayUpdater()?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;

        controls.SetIsEnabled(true)?;
        // Buttons start disabled; enablement is driven per update from the
        // item's capabilities. (Fast-forward/rewind/record/channel buttons are
        // never enabled — the `windows` crate defaults already leave them off.)
        controls.SetIsPlayEnabled(false)?;
        controls.SetIsPauseEnabled(false)?;
        controls.SetIsStopEnabled(false)?;
        controls.SetIsNextEnabled(false)?;
        controls.SetIsPreviousEnabled(false)?;
        display_updater.SetType(MediaPlaybackType::Music)?;
        display_updater.SetAppMediaId(&HSTRING::from("io.music-assistant.companion"))?;

        let button_handler = TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  args: Ref<'_, SystemMediaTransportControlsButtonPressedEventArgs>| {
                if let Some(args) = args.as_ref() {
                    if let Some(command) = command_for_button(args.Button()?) {
                        invoke_callback(command, "Windows SMTC callback panicked");
                    }
                }
                Ok(())
            },
        );
        let button_token = controls.ButtonPressed(&button_handler)?;

        // There is no seek path to the frontend, so honor the SMTC contract's
        // second option for position-change requests: ignore the request but
        // re-assert the real position, so the flyout scrubber snaps back
        // instead of silently lying about a seek that never happened. The
        // work is recorded as a pending event and handled through the drain
        // loop so it survives arriving while the state is borrowed.
        let position_handler = TypedEventHandler::new(
            move |_sender: Ref<'_, SystemMediaTransportControls>,
                  _args: Ref<'_, PlaybackPositionChangeRequestedEventArgs>| {
                PENDING_EVENTS.lock().reassert_timeline = true;
                dispatch(|| with_state_mut(|_| {}));
                Ok(())
            },
        );
        let position_token = controls.PlaybackPositionChangeRequested(&position_handler)?;

        let thumbbar = match TaskbarThumbnailControls::new(HWND(hwnd)) {
            Ok(thumbbar) => Some(thumbbar),
            Err(e) => {
                log::warn!("[MediaControls] Failed to initialize Windows taskbar thumbnail controls: {e:?}");
                None
            }
        };

        Ok(Self {
            controls,
            display_updater,
            timeline,
            button_token,
            position_token,
            thumbbar,
            last_metadata: None,
            cleared: false,
        })
    }

    fn update(&mut self, plan: &NowPlayingPlan) -> windows::core::Result<()> {
        // `plan.can_play`/`can_pause` are state-independent item capabilities
        // (shared mapping); next/previous come from the queue flags; stop is
        // meaningful whenever a track is loaded. `plan.rate` is deliberately
        // unused here: SMTC has no playback-rate property to feed, and
        // PlaybackStatus already conveys playing/paused.
        self.controls.SetIsPlayEnabled(plan.can_play)?;
        self.controls.SetIsPauseEnabled(plan.can_pause)?;
        self.controls.SetIsStopEnabled(plan.title.is_some())?;
        self.controls.SetIsNextEnabled(plan.can_next)?;
        self.controls.SetIsPreviousEnabled(plan.can_previous)?;
        if let Some(thumbbar) = &mut self.thumbbar {
            if let Err(e) = thumbbar.update(plan) {
                log::warn!(
                    "[MediaControls] Failed to update Windows taskbar thumbnail controls: {e:?}"
                );
            }
        }

        if plan.state == PlaybackState::Stopped {
            return self.clear();
        }
        self.cleared = false;

        let metadata = MetadataKey::from_plan(plan);
        if self.last_metadata.as_ref() != Some(&metadata) {
            self.update_metadata(plan)?;
            self.last_metadata = Some(metadata);
        }

        // A timeline is only valid for an item with a known finite length:
        // TimelineProperties defines Position as bounded by Start/EndTime, and
        // a Position past EndTime renders a glitched progress bar. Live and
        // unknown-length streams (duration absent) get a zeroed timeline,
        // which hides the progress bar entirely.
        if let Some(duration) = plan.duration_secs.filter(|d| d.is_finite() && *d > 0.0) {
            let start = TimeSpan::default();
            let end = seconds_to_timespan(duration).unwrap_or(start);
            let position = plan
                .elapsed_secs
                // NaN would slip through `min` (f64::min ignores NaN and
                // would pin the position to the end of the track).
                .filter(|elapsed| elapsed.is_finite())
                .map(|elapsed| elapsed.min(duration))
                .and_then(seconds_to_timespan)
                .unwrap_or(start);
            self.timeline.SetStartTime(start)?;
            self.timeline.SetMinSeekTime(start)?;
            self.timeline.SetEndTime(end)?;
            self.timeline.SetMaxSeekTime(end)?;
            self.timeline.SetPosition(position)?;
        } else {
            self.reset_timeline()?;
        }

        self.controls.UpdateTimelineProperties(&self.timeline)?;
        self.controls
            .SetPlaybackStatus(playback_status(plan.state))?;
        Ok(())
    }

    fn reset_timeline(&self) -> windows::core::Result<()> {
        let reset = TimeSpan::default();
        self.timeline.SetStartTime(reset)?;
        self.timeline.SetMinSeekTime(reset)?;
        self.timeline.SetEndTime(reset)?;
        self.timeline.SetMaxSeekTime(reset)?;
        self.timeline.SetPosition(reset)?;
        Ok(())
    }

    fn update_metadata(&self, plan: &NowPlayingPlan) -> windows::core::Result<()> {
        self.display_updater.ClearAll()?;
        self.display_updater.SetType(MediaPlaybackType::Music)?;
        let properties = self.display_updater.MusicProperties()?;
        set_music_string(plan.title.as_deref(), |value| properties.SetTitle(value))?;
        set_music_string(plan.artist.as_deref(), |value| properties.SetArtist(value))?;
        set_music_string(plan.album.as_deref(), |value| {
            properties.SetAlbumTitle(value)
        })?;
        self.set_thumbnail(plan.image_url.as_deref());
        self.display_updater.Update()?;
        Ok(())
    }

    /// Point the display updater at the item's cover, or the bundled app logo
    /// when there is none. Remote URIs are fetched lazily by the shell when
    /// the flyout renders, so a failing fetch is invisible here — only
    /// synchronous URI/factory errors surface, and then the default art is
    /// used instead of leaving the tile empty after `ClearAll()`.
    fn set_thumbnail(&self, url: Option<&str>) {
        if let Some(url) = url {
            let result = Uri::CreateUri(&HSTRING::from(url))
                .and_then(|uri| RandomAccessStreamReference::CreateFromUri(&uri))
                .and_then(|stream| self.display_updater.SetThumbnail(&stream));
            match result {
                Ok(()) => return,
                Err(e) => log::warn!(
                    "[MediaControls] Failed to set Windows SMTC thumbnail for {url}: {e:?}"
                ),
            }
        }

        if let Some(default) = default_thumbnail() {
            if let Err(e) = self.display_updater.SetThumbnail(&default) {
                log::warn!("[MediaControls] Failed to set default SMTC thumbnail: {e:?}");
            }
        }
    }

    /// Empty the display and report `Stopped`. Deliberately not `Closed`:
    /// `Stopped` keeps the (empty) session registered so media keys can still
    /// reach us for a resume, while `Closed` would remove the app from the
    /// SMTC session list entirely — that is reserved for teardown in `Drop`.
    fn clear(&mut self) -> windows::core::Result<()> {
        if self.cleared {
            return Ok(());
        }
        self.display_updater.ClearAll()?;
        self.display_updater.SetType(MediaPlaybackType::Music)?;
        self.display_updater.Update()?;
        self.last_metadata = None;
        self.controls
            .SetPlaybackStatus(MediaPlaybackStatus::Stopped)?;
        self.reset_timeline()?;
        self.controls.UpdateTimelineProperties(&self.timeline)?;
        self.cleared = true;
        Ok(())
    }
}

impl Drop for WindowsMediaControls {
    fn drop(&mut self) {
        let _ = self.controls.RemoveButtonPressed(self.button_token);
        let _ = self
            .controls
            .RemovePlaybackPositionChangeRequested(self.position_token);
        let _ = self.controls.SetIsEnabled(false);
        let _ = self.controls.SetPlaybackStatus(MediaPlaybackStatus::Closed);
    }
}

/// The fallback thumbnail (bundled app logo), built once per process:
/// `RandomAccessStreamReference` is a cheap cloneable handle, and rebuilding
/// the in-memory stream on every bind/rebind would be wasted work.
fn default_thumbnail() -> Option<RandomAccessStreamReference> {
    static DEFAULT_THUMBNAIL: std::sync::OnceLock<Option<RandomAccessStreamReference>> =
        std::sync::OnceLock::new();
    DEFAULT_THUMBNAIL
        .get_or_init(|| match build_default_thumbnail() {
            Ok(thumbnail) => Some(thumbnail),
            Err(e) => {
                log::warn!("[MediaControls] Failed to build default SMTC thumbnail: {e:?}");
                None
            }
        })
        .clone()
}

/// Build an in-memory stream reference around the bundled app logo.
fn build_default_thumbnail() -> windows::core::Result<RandomAccessStreamReference> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(DEFAULT_ART_PNG)?;
    writer.StoreAsync()?.get()?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    RandomAccessStreamReference::CreateFromStream(&stream)
}

struct TaskbarThumbnailControls {
    hwnd: HWND,
    taskbar: ITaskbarList3,
    icons: ThumbIcons,
    icon_theme: ThumbIconTheme,
    buttons_added: bool,
    add_button_failures: u8,
    last_buttons: Option<ThumbButtonState>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct ThumbButtonState {
    previous_enabled: bool,
    play_pause_enabled: bool,
    use_pause: bool,
    next_enabled: bool,
}

// SAFETY: `TaskbarThumbnailControls` is stored behind the process-wide mutex so
// updates can be scheduled from any thread, but construction/update/clear paths
// dispatch onto Tauri's UI thread before touching the HWND, COM object,
// subclass registration, or HICONs.
unsafe impl Send for TaskbarThumbnailControls {}

struct ThumbIcons {
    previous: HICON,
    play: HICON,
    pause: HICON,
    next: HICON,
}

impl ThumbIcons {
    fn new(theme: ThumbIconTheme) -> windows::core::Result<Self> {
        let color = icon_color(theme);
        let previous = OwnedIcon(icon_from_mask(PREVIOUS_MASK_PNG, color)?);
        let play = OwnedIcon(icon_from_mask(PLAY_MASK_PNG, color)?);
        let pause = OwnedIcon(icon_from_mask(PAUSE_MASK_PNG, color)?);
        let next = OwnedIcon(icon_from_mask(NEXT_MASK_PNG, color)?);
        Ok(Self {
            previous: previous.into_raw(),
            play: play.into_raw(),
            pause: pause.into_raw(),
            next: next.into_raw(),
        })
    }
}

impl Drop for ThumbIcons {
    fn drop(&mut self) {
        for icon in [self.previous, self.play, self.pause, self.next] {
            destroy_icon(icon);
        }
    }
}

struct OwnedIcon(HICON);

impl OwnedIcon {
    fn into_raw(self) -> HICON {
        let icon = self.0;
        std::mem::forget(self);
        icon
    }
}

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        destroy_icon(self.0);
    }
}

fn destroy_icon(icon: HICON) {
    let _ = unsafe { DestroyIcon(icon) };
}

impl TaskbarThumbnailControls {
    fn new(hwnd: HWND) -> windows::core::Result<Self> {
        // COM apartment initialization happens in `WindowsMediaControls::new`,
        // before the first WinRT call of the whole backend.
        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }?;
        unsafe { taskbar.HrInit()? };
        let taskbar_button_created_msg =
            unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
        if taskbar_button_created_msg == 0 {
            return Err(windows::core::Error::from_win32());
        }
        // Mirror for the subclass proc (see TASKBAR_CREATED_MSG).
        TASKBAR_CREATED_MSG.store(
            taskbar_button_created_msg,
            std::sync::atomic::Ordering::Relaxed,
        );
        let icon_theme = current_icon_theme();
        let icons = ThumbIcons::new(icon_theme)?;
        let mut controls = Self {
            hwnd,
            taskbar,
            icons,
            icon_theme,
            buttons_added: false,
            add_button_failures: 0,
            last_buttons: None,
        };
        if !unsafe { SetWindowSubclass(hwnd, Some(taskbar_subclass_proc), THUMB_SUBCLASS_ID, 0) }
            .as_bool()
        {
            return Err(windows::core::Error::from_win32());
        }
        controls.try_add_buttons();
        Ok(controls)
    }

    fn update(&mut self, plan: &NowPlayingPlan) -> windows::core::Result<()> {
        let state = ThumbButtonState {
            previous_enabled: plan.can_previous,
            play_pause_enabled: plan.can_play || plan.can_pause,
            use_pause: plan.state == PlaybackState::Playing,
            next_enabled: plan.can_next,
        };
        self.update_buttons(state)
    }

    fn on_taskbar_button_created(&mut self) {
        self.add_button_failures = 0;
        self.try_add_buttons();
    }

    fn refresh_icon_theme(&mut self) {
        let icon_theme = current_icon_theme();
        if icon_theme == self.icon_theme {
            return;
        }
        match ThumbIcons::new(icon_theme) {
            Ok(icons) => {
                self.icons = icons;
                self.icon_theme = icon_theme;
                if let Some(state) = self.last_buttons {
                    let _ = self.force_update_buttons(state);
                }
            }
            Err(e) => {
                log::warn!("[MediaControls] Failed to rebuild Windows taskbar icons for theme change: {e:?}");
            }
        }
    }

    fn try_add_buttons(&mut self) {
        const MAX_ADD_BUTTON_FAILURES: u8 = 8;
        if self.add_button_failures >= MAX_ADD_BUTTON_FAILURES {
            return;
        }

        let state = self.last_buttons.unwrap_or_default();
        let buttons = self.buttons(state);
        match unsafe { self.taskbar.ThumbBarAddButtons(self.hwnd, &buttons) } {
            Ok(()) => {
                self.buttons_added = true;
                self.add_button_failures = 0;
                self.last_buttons = Some(state);
            }
            Err(e) => {
                self.add_button_failures = self.add_button_failures.saturating_add(1);
                if self.add_button_failures == MAX_ADD_BUTTON_FAILURES {
                    log::warn!("[MediaControls] Windows taskbar thumbnail buttons unavailable after {MAX_ADD_BUTTON_FAILURES} attempts: {e:?}");
                } else {
                    log::debug!(
                        "[MediaControls] Windows taskbar thumbnail buttons not ready: {e:?}"
                    );
                }
            }
        }
    }

    fn update_buttons(&mut self, state: ThumbButtonState) -> windows::core::Result<()> {
        if !self.buttons_added {
            self.last_buttons = Some(state);
            self.try_add_buttons();
            return Ok(());
        }
        if self.last_buttons == Some(state) {
            return Ok(());
        }
        self.force_update_buttons(state)
    }

    fn force_update_buttons(&mut self, state: ThumbButtonState) -> windows::core::Result<()> {
        let buttons = self.buttons(state);
        unsafe { self.taskbar.ThumbBarUpdateButtons(self.hwnd, &buttons) }?;
        self.last_buttons = Some(state);
        Ok(())
    }

    fn buttons(&self, state: ThumbButtonState) -> [THUMBBUTTON; 3] {
        [
            thumb_button(
                THUMB_PREVIOUS_ID,
                self.icons.previous,
                "Previous track",
                state.previous_enabled,
            ),
            thumb_button(
                THUMB_PLAY_PAUSE_ID,
                if state.use_pause {
                    self.icons.pause
                } else {
                    self.icons.play
                },
                if state.use_pause { "Pause" } else { "Play" },
                state.play_pause_enabled,
            ),
            thumb_button(
                THUMB_NEXT_ID,
                self.icons.next,
                "Next track",
                state.next_enabled,
            ),
        ]
    }
}

impl Drop for TaskbarThumbnailControls {
    fn drop(&mut self) {
        let _ = unsafe {
            RemoveWindowSubclass(self.hwnd, Some(taskbar_subclass_proc), THUMB_SUBCLASS_ID)
        };
    }
}

unsafe extern "system" fn taskbar_subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    _ref_data: usize,
) -> LRESULT {
    // Record the event first, then poke `with_state_mut` with a no-op: if the
    // state is free the drain loop handles the event immediately; if the
    // state is mutably borrowed (re-entrant delivery during an update), the
    // flag survives and the outer borrow holder drains it when done. The
    // message id comes from a static mirror so recognizing it never needs the
    // (possibly borrowed) state.
    let taskbar_created_msg = TASKBAR_CREATED_MSG.load(std::sync::atomic::Ordering::Relaxed);
    if taskbar_created_msg != 0 && msg == taskbar_created_msg {
        PENDING_EVENTS.lock().taskbar_created = true;
        with_state_mut(|_| {});
        return LRESULT(0);
    }
    if matches!(msg, WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED) {
        PENDING_EVENTS.lock().theme_changed = true;
        with_state_mut(|_| {});
    }

    if msg == WM_COMMAND && command_notification(wparam) == THBN_CLICKED {
        if let Some(command) = command_for_thumb_button(command_id(wparam)) {
            invoke_callback(command, "Windows taskbar thumbnail callback panicked");
            return LRESULT(0);
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

fn command_for_button(button: SystemMediaTransportControlsButton) -> Option<&'static str> {
    if button == SystemMediaTransportControlsButton::Play {
        Some("play")
    } else if button == SystemMediaTransportControlsButton::Pause {
        Some("pause")
    } else if button == SystemMediaTransportControlsButton::Stop {
        Some("stop")
    } else if button == SystemMediaTransportControlsButton::Next {
        Some("next")
    } else if button == SystemMediaTransportControlsButton::Previous {
        Some("previous")
    } else {
        None
    }
}

fn command_for_thumb_button(button_id: u32) -> Option<&'static str> {
    match button_id {
        THUMB_PREVIOUS_ID => Some("previous"),
        THUMB_PLAY_PAUSE_ID => Some("toggle"),
        THUMB_NEXT_ID => Some("next"),
        _ => None,
    }
}

fn invoke_callback(command: &'static str, panic_message: &str) {
    let callback = CALLBACK.lock().clone();
    if let Some(callback) = callback {
        if catch_unwind(AssertUnwindSafe(|| callback(command))).is_err() {
            log::error!("[MediaControls] {panic_message}");
        }
    }
}

fn playback_status(state: PlaybackState) -> MediaPlaybackStatus {
    match state {
        PlaybackState::Playing => MediaPlaybackStatus::Playing,
        PlaybackState::Paused => MediaPlaybackStatus::Paused,
        PlaybackState::Stopped => MediaPlaybackStatus::Stopped,
    }
}

fn thumb_button(id: u32, icon: HICON, tooltip: &str, enabled: bool) -> THUMBBUTTON {
    let mut button = THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        hIcon: icon,
        dwFlags: if enabled { THBF_ENABLED } else { THBF_DISABLED },
        ..Default::default()
    };
    write_tooltip(&mut button.szTip, tooltip);
    button
}

fn write_tooltip(buffer: &mut [u16; 260], text: &str) {
    buffer.fill(0);
    for (slot, value) in buffer.iter_mut().take(259).zip(text.encode_utf16()) {
        *slot = value;
    }
}

fn icon_from_mask(mask_png: &[u8], color: (u8, u8, u8)) -> windows::core::Result<HICON> {
    let alpha = decode_icon_mask(mask_png)?;
    let color_bits = bgra_icon_bits(&alpha, color);
    let hbm_color = unsafe {
        CreateBitmap(
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            1,
            32,
            Some(color_bits.as_ptr().cast()),
        )
    };
    if hbm_color.is_invalid() {
        return Err(windows::core::Error::from_win32());
    }

    let mask_bits = [0_u8; 32];
    let hbm_mask = unsafe {
        CreateBitmap(
            ICON_SIZE as i32,
            ICON_SIZE as i32,
            1,
            1,
            Some(mask_bits.as_ptr().cast()),
        )
    };
    if hbm_mask.is_invalid() {
        let _ = unsafe { DeleteObject(HGDIOBJ(hbm_color.0)) };
        return Err(windows::core::Error::from_win32());
    }

    let icon_info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let icon = unsafe { CreateIconIndirect(&raw const icon_info) };
    let _ = unsafe { DeleteObject(HGDIOBJ(hbm_mask.0)) };
    let _ = unsafe { DeleteObject(HGDIOBJ(hbm_color.0)) };
    icon
}

fn decode_icon_mask(mask_png: &[u8]) -> windows::core::Result<Vec<u8>> {
    let decoder = png::Decoder::new(Cursor::new(mask_png));
    let mut reader = decoder.read_info().map_err(|e| {
        windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005_u32 as i32),
            format!("Failed to read Lucide icon mask: {e}"),
        )
    })?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| {
        windows::core::Error::new(
            windows::core::HRESULT(0x8000_4005_u32 as i32),
            format!("Failed to decode Lucide icon mask: {e}"),
        )
    })?;
    if info.width != ICON_SIZE as u32 || info.height != ICON_SIZE as u32 {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x8007_0057_u32 as i32),
            "Lucide icon mask has unexpected dimensions",
        ));
    }

    if info.color_type != png::ColorType::Rgba {
        return Err(windows::core::Error::new(
            windows::core::HRESULT(0x8007_0057_u32 as i32),
            "Lucide icon masks must be RGBA PNGs",
        ));
    }

    Ok(buf[..info.buffer_size()]
        .chunks_exact(4)
        .map(|px| px[3])
        .collect())
}

fn bgra_icon_bits(alpha: &[u8], (red, green, blue): (u8, u8, u8)) -> Vec<u8> {
    let mut bits = vec![0; ICON_SIZE * ICON_SIZE * 4];
    for y in 0..ICON_SIZE {
        // `CreateBitmap` stores DDB rows bottom-up for `CreateIconIndirect`.
        let src_row = ICON_SIZE - 1 - y;
        for x in 0..ICON_SIZE {
            let alpha = alpha[src_row * ICON_SIZE + x];
            let offset = (y * ICON_SIZE + x) * 4;
            bits[offset] = blue;
            bits[offset + 1] = green;
            bits[offset + 2] = red;
            bits[offset + 3] = alpha;
        }
    }
    bits
}

fn current_icon_theme() -> ThumbIconTheme {
    if high_contrast_enabled() {
        return ThumbIconTheme::HighContrast(unsafe { GetSysColor(COLOR_BTNTEXT) });
    }
    if system_uses_light_theme().unwrap_or(false) {
        ThumbIconTheme::LightShell
    } else {
        ThumbIconTheme::DarkShell
    }
}

fn icon_color(theme: ThumbIconTheme) -> (u8, u8, u8) {
    match theme {
        ThumbIconTheme::DarkShell => (255, 255, 255),
        ThumbIconTheme::LightShell => (32, 32, 32),
        ThumbIconTheme::HighContrast(colorref) => colorref_to_rgb(colorref),
    }
}

fn colorref_to_rgb(colorref: u32) -> (u8, u8, u8) {
    (
        (colorref & 0xff) as u8,
        ((colorref >> 8) & 0xff) as u8,
        ((colorref >> 16) & 0xff) as u8,
    )
}

fn high_contrast_enabled() -> bool {
    let mut high_contrast = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            high_contrast.cbSize,
            Some((&raw mut high_contrast).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .is_ok()
        && high_contrast.dwFlags.contains(HCF_HIGHCONTRASTON)
}

fn system_uses_light_theme() -> Option<bool> {
    let mut value = 0_u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&raw mut size),
        )
    };
    if result.is_ok() {
        Some(value != 0)
    } else {
        None
    }
}

fn command_id(wparam: WPARAM) -> u32 {
    (wparam.0 & 0xffff) as u32
}

fn command_notification(wparam: WPARAM) -> u32 {
    ((wparam.0 >> 16) & 0xffff) as u32
}

fn set_music_string<F>(value: Option<&str>, setter: F) -> windows::core::Result<()>
where
    F: FnOnce(&HSTRING) -> windows::core::Result<()>,
{
    if let Some(value) = value {
        setter(&HSTRING::from(value))?;
    }
    Ok(())
}

fn seconds_to_timespan(seconds: f64) -> Option<TimeSpan> {
    // `try_from_secs_f64` (unlike `from_secs_f64`) rejects values that
    // overflow a Duration instead of panicking; the values come from
    // frontend/server JSON, so hostile-but-finite input must not panic the
    // UI thread. Also cap at 30 days so a garbage value cannot overflow the
    // TimeSpan's i64 100-ns units downstream.
    const MAX_TIMELINE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
    if seconds.is_finite() && seconds >= 0.0 {
        Duration::try_from_secs_f64(seconds)
            .ok()
            .map(|duration| TimeSpan::from(duration.min(MAX_TIMELINE)))
    } else {
        None
    }
}
