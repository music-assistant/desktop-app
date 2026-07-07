//! Native Windows media-controls backend using System Media Transport Controls.
#![allow(unsafe_code)] // `GetForWindow` is a WinRT interop call.

use super::{MainThreadDispatch, MediaControlCallback, NowPlayingPlan, PlaybackState};
use crate::now_playing::NowPlaying;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::Duration;
use windows::core::{factory, Ref, HSTRING};
use windows::Foundation::{TimeSpan, TypedEventHandler, Uri};
use windows::Media::{
    MediaPlaybackStatus, MediaPlaybackType, SystemMediaTransportControls,
    SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
    SystemMediaTransportControlsDisplayUpdater, SystemMediaTransportControlsTimelineProperties,
};
use windows::Storage::Streams::RandomAccessStreamReference;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::ISystemMediaTransportControlsInterop;

static STATE: Mutex<Option<WindowsMediaControls>> = Mutex::new(None);
static CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);
static DISPATCH: Mutex<Option<MainThreadDispatch>> = Mutex::new(None);

struct WindowsMediaControls {
    controls: SystemMediaTransportControls,
    display_updater: SystemMediaTransportControlsDisplayUpdater,
    timeline: SystemMediaTransportControlsTimelineProperties,
    button_token: i64,
    last_metadata: Option<MetadataKey>,
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
        let mut state = STATE.lock();
        let Some(controls) = state.as_mut() else {
            return;
        };

        if let Err(e) = controls.update(&plan, &np) {
            log::error!("[MediaControls] Failed to update Windows SMTC: {e:?}");
        }
    });
}

pub fn clear() {
    dispatch(|| {
        let mut state = STATE.lock();
        let Some(controls) = state.as_mut() else {
            return;
        };

        if let Err(e) = controls.clear() {
            log::warn!("[MediaControls] Failed to clear Windows SMTC: {e:?}");
        }
    });
}

fn dispatch(f: impl FnOnce() + Send + 'static) {
    let Some(dispatch) = DISPATCH.lock().clone() else {
        return;
    };
    dispatch(Box::new(f));
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
                        let callback = CALLBACK.lock().clone();
                        if let Some(callback) = callback {
                            if catch_unwind(AssertUnwindSafe(|| callback(command))).is_err() {
                                log::error!("[MediaControls] Windows SMTC callback panicked");
                            }
                        }
                    }
                }
                Ok(())
            },
        );
        let button_token = controls.ButtonPressed(&button_handler)?;

        Ok(Self {
            controls,
            display_updater,
            timeline,
            button_token,
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

fn playback_status(state: PlaybackState) -> MediaPlaybackStatus {
    match state {
        PlaybackState::Playing => MediaPlaybackStatus::Playing,
        PlaybackState::Paused => MediaPlaybackStatus::Paused,
        PlaybackState::Stopped => MediaPlaybackStatus::Stopped,
    }
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
