//! Native Windows media-controls backend using System Media Transport Controls.
#![allow(unsafe_code)] // `GetForWindow` is a WinRT interop call.

use super::{MainThreadDispatch, MediaControlCallback, NowPlayingPlan, PlaybackState};
use crate::now_playing::NowPlaying;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;
use windows::core::{factory, w, Ref, HSTRING};
use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsDisplayUpdater, SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::RandomAccessStreamReference;
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

static STATE: Mutex<Option<WindowsMediaControls>> = Mutex::new(None);
static CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);
static DISPATCH: Mutex<Option<MainThreadDispatch>> = Mutex::new(None);

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
    thumbbar: Option<TaskbarThumbnailControls>,
    last_metadata: Option<MetadataKey>,
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
        if STATE.lock().is_some() {
            return;
        }

        match WindowsMediaControls::new(hwnd_addr as *mut c_void) {
            Ok(controls) => {
                *STATE.lock() = Some(controls);
            }
            Err(e) => {
                log::error!("[MediaControls] Failed to initialize Windows SMTC: {e:?}");
            }
        }
    });
}

pub fn update(np: &NowPlaying) {
    let plan = super::plan(np);
    let np = np.clone();
    dispatch(move || {
        with_state_mut(|controls| {
            if let Err(e) = controls.update(&plan, &np) {
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
    let Some(mut controls) = STATE.lock().take() else {
        return;
    };
    f(&mut controls);
    *STATE.lock() = Some(controls);
}

impl WindowsMediaControls {
    fn new(hwnd: *mut c_void) -> windows::core::Result<Self> {
        let interop: ISystemMediaTransportControlsInterop =
            factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
        // Tauri gives us a Win32 HWND, not a UWP/CoreWindow view. SMTC needs the
        // interop API to attach media controls to that desktop window.
        let controls: SystemMediaTransportControls = unsafe { interop.GetForWindow(HWND(hwnd)) }?;
        let display_updater = controls.DisplayUpdater()?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;

        controls.SetIsEnabled(true)?;
        controls.SetIsPlayEnabled(true)?;
        controls.SetIsPauseEnabled(true)?;
        controls.SetIsStopEnabled(true)?;
        controls.SetIsNextEnabled(true)?;
        controls.SetIsPreviousEnabled(true)?;
        display_updater.SetType(MediaPlaybackType::Music)?;

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
            thumbbar,
            last_metadata: None,
        })
    }

    fn update(&mut self, plan: &NowPlayingPlan, np: &NowPlaying) -> windows::core::Result<()> {
        self.controls
            .SetIsPlayEnabled(np.can_play || !np.is_playing)?;
        self.controls
            .SetIsPauseEnabled(np.can_pause || np.is_playing)?;
        self.controls.SetIsNextEnabled(np.can_next)?;
        self.controls.SetIsPreviousEnabled(np.can_previous)?;
        if let Some(thumbbar) = &mut self.thumbbar {
            if let Err(e) = thumbbar.update(np) {
                log::warn!(
                    "[MediaControls] Failed to update Windows taskbar thumbnail controls: {e:?}"
                );
            }
        }

        if plan.state == PlaybackState::Stopped {
            return self.clear();
        }

        let metadata = MetadataKey::from_plan(plan);
        if self.last_metadata.as_ref() != Some(&metadata) {
            self.update_metadata(plan)?;
            self.last_metadata = Some(metadata);
        }

        let start = TimeSpan::default();
        let end = plan
            .duration_secs
            .and_then(seconds_to_timespan)
            .unwrap_or(start);
        self.timeline.SetStartTime(start)?;
        self.timeline.SetMinSeekTime(start)?;
        self.timeline.SetEndTime(end)?;
        self.timeline.SetMaxSeekTime(end)?;
        self.timeline.SetPosition(
            plan.elapsed_secs
                .and_then(seconds_to_timespan)
                .unwrap_or(start),
        )?;

        self.controls.UpdateTimelineProperties(&self.timeline)?;
        self.controls
            .SetPlaybackStatus(playback_status(plan.state))?;
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

    fn set_thumbnail(&self, url: Option<&str>) {
        let Some(url) = url else {
            return;
        };

        let result = Uri::CreateUri(&HSTRING::from(url))
            .and_then(|uri| RandomAccessStreamReference::CreateFromUri(&uri))
            .and_then(|stream| self.display_updater.SetThumbnail(&stream));
        if let Err(e) = result {
            log::warn!("[MediaControls] Failed to set Windows SMTC thumbnail for {url}: {e:?}");
        }
    }

    fn clear(&mut self) -> windows::core::Result<()> {
        self.display_updater.ClearAll()?;
        self.display_updater.SetType(MediaPlaybackType::Music)?;
        self.display_updater.Update()?;
        self.last_metadata = None;
        self.controls
            .SetPlaybackStatus(MediaPlaybackStatus::Stopped)?;
        let reset = TimeSpan::default();
        self.timeline.SetStartTime(reset)?;
        self.timeline.SetMinSeekTime(reset)?;
        self.timeline.SetEndTime(reset)?;
        self.timeline.SetMaxSeekTime(reset)?;
        self.timeline.SetPosition(reset)?;
        self.controls.UpdateTimelineProperties(&self.timeline)?;
        Ok(())
    }
}

impl Drop for WindowsMediaControls {
    fn drop(&mut self) {
        let _ = self.controls.RemoveButtonPressed(self.button_token);
        let _ = self.controls.SetIsEnabled(false);
        let _ = self.controls.SetPlaybackStatus(MediaPlaybackStatus::Closed);
    }
}

struct TaskbarThumbnailControls {
    hwnd: HWND,
    taskbar: ITaskbarList3,
    taskbar_button_created_msg: u32,
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
        // Tauri/Wry initializes COM for the UI thread in normal operation. If it
        // has not yet done so, initialize this long-lived UI thread as STA for
        // shell APIs; we intentionally do not CoUninitialize it during process
        // lifetime. Ignore RPC_E_CHANGED_MODE because an existing MTA is still
        // usable for CoCreateInstance below.
        let init_result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if init_result.is_err() && init_result != RPC_E_CHANGED_MODE {
            log::debug!("[MediaControls] CoInitializeEx for ThumbBar returned {init_result:?}");
        }

        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER) }?;
        unsafe { taskbar.HrInit()? };
        let taskbar_button_created_msg =
            unsafe { RegisterWindowMessageW(w!("TaskbarButtonCreated")) };
        if taskbar_button_created_msg == 0 {
            return Err(windows::core::Error::from_win32());
        }
        let icon_theme = current_icon_theme();
        let icons = ThumbIcons::new(icon_theme)?;
        let mut controls = Self {
            hwnd,
            taskbar,
            taskbar_button_created_msg,
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

    fn update(&mut self, np: &NowPlaying) -> windows::core::Result<()> {
        let state = ThumbButtonState {
            previous_enabled: np.can_previous,
            play_pause_enabled: np.can_play || np.can_pause || np.is_playing,
            use_pause: np.is_playing,
            next_enabled: np.can_next,
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
    let taskbar_button_created_msg = STATE
        .lock()
        .as_ref()
        .and_then(|state| state.thumbbar.as_ref())
        .map(|controls| controls.taskbar_button_created_msg);

    if taskbar_button_created_msg == Some(msg) {
        with_state_mut(|state| {
            if let Some(thumbbar) = state.thumbbar.as_mut() {
                thumbbar.on_taskbar_button_created();
            }
        });
        return LRESULT(0);
    }

    if matches!(msg, WM_SETTINGCHANGE | WM_SYSCOLORCHANGE | WM_THEMECHANGED) {
        with_state_mut(|state| {
            if let Some(thumbbar) = state.thumbbar.as_mut() {
                thumbbar.refresh_icon_theme();
            }
        });
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
    let icon = unsafe { CreateIconIndirect(&icon_info) };
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
    if seconds.is_finite() && seconds >= 0.0 {
        Some(TimeSpan::from(Duration::from_secs_f64(seconds)))
    } else {
        None
    }
}
