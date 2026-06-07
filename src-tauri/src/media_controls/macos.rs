//! Native macOS media-controls backend: drives `MPNowPlayingInfoCenter` and
//! `MPRemoteCommandCenter` directly via objc2, replacing the `souvlaki` crate
//! that crashed the app on unloadable cover URLs.
//!
//! Every objc2 call must run on the `NSApplication` main run loop, reached via
//! the [`MainThreadDispatch`](super::MainThreadDispatch) given to [`init`];
//! `AppKit` delivers remote-command handlers there too. Nothing objc2 is kept in
//! a static (those types are `!Send`) — only plain data is, and the framework
//! singletons are re-fetched inside each main-thread closure.
#![allow(unsafe_code)] // objc2 framework methods are all `unsafe`; lift the workspace deny.

use super::{MainThreadDispatch, MediaControlCallback, NowPlayingPlan, PlaybackState};
use crate::now_playing::NowPlaying;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::AnyThread;
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSCopying, NSData, NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyPlaybackRate, MPRemoteCommand, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use parking_lot::Mutex;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

const ARTWORK_SIZE: f64 = 512.0;
const COVER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_COVER_BYTES: u64 = 8 * 1024 * 1024;

static CALLBACK: Mutex<Option<MediaControlCallback>> = Mutex::new(None);
static DISPATCH: Mutex<Option<MainThreadDispatch>> = Mutex::new(None);
static LAST_PLAN: Mutex<Option<NowPlayingPlan>> = Mutex::new(None);
static COVER: Mutex<CoverCache> = Mutex::new(CoverCache::EMPTY);
/// Invalidates any in-flight cover download when the track changes.
static COVER_GEN: AtomicU64 = AtomicU64::new(0);
static COMMANDS_REGISTERED: AtomicBool = AtomicBool::new(false);

/// Cover bytes (not an objc2 object) so the cache stays `Send`.
struct CoverCache {
    url: Option<String>,
    bytes: Option<Arc<Vec<u8>>>,
}

impl CoverCache {
    const EMPTY: Self = Self {
        url: None,
        bytes: None,
    };
}

pub fn init(callback: MediaControlCallback, dispatch: MainThreadDispatch) {
    *CALLBACK.lock() = Some(callback);
    *DISPATCH.lock() = Some(dispatch.clone());

    if COMMANDS_REGISTERED.swap(true, Ordering::SeqCst) {
        return;
    }
    dispatch(Box::new(|| unsafe { register_commands() }));
}

pub fn update(np: &NowPlaying) {
    let plan = super::plan(np);
    refresh_cover_if_changed(&plan);
    *LAST_PLAN.lock() = Some(plan);
    dispatch_render();
}

pub fn clear() {
    *LAST_PLAN.lock() = None;
    COVER_GEN.fetch_add(1, Ordering::SeqCst);
    *COVER.lock() = CoverCache::EMPTY;
    let Some(dispatch) = DISPATCH.lock().clone() else {
        return;
    };
    dispatch(Box::new(|| unsafe {
        MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(None);
    }));
}

fn refresh_cover_if_changed(plan: &NowPlayingPlan) {
    let want = plan.image_url.clone();
    if COVER.lock().url == want {
        return;
    }

    let generation = COVER_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut cover = COVER.lock();
        cover.url.clone_from(&want);
        // Drop the old art now so the new track never shows the previous cover.
        cover.bytes = None;
    }

    let Some(url) = want else {
        return;
    };

    std::thread::spawn(move || match download_image(&url) {
        Ok(bytes) => {
            if COVER_GEN.load(Ordering::SeqCst) != generation {
                return; // Superseded by a newer track.
            }
            {
                let mut cover = COVER.lock();
                if cover.url.as_deref() != Some(url.as_str()) {
                    return;
                }
                cover.bytes = Some(Arc::new(bytes));
            }
            dispatch_render();
        }
        Err(e) => log::warn!("[MediaControls] cover download failed for {url}: {e}"),
    });
}

fn download_image(url: &str) -> Result<Vec<u8>, ureq::Error> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(COVER_TIMEOUT))
        .build();
    let agent = ureq::Agent::new_with_config(config);
    let mut response = agent.get(url).call()?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_COVER_BYTES)
        .read_to_vec()?;
    Ok(bytes)
}

fn dispatch_render() {
    let Some(dispatch) = DISPATCH.lock().clone() else {
        return;
    };
    dispatch(Box::new(|| unsafe { render() }));
}

unsafe fn render() {
    let Some(plan) = LAST_PLAN.lock().clone() else {
        return;
    };
    let center = MPNowPlayingInfoCenter::defaultCenter();
    if plan.state == PlaybackState::Stopped {
        center.setNowPlayingInfo(None);
        return;
    }
    let info = build_info_dict(&plan);
    center.setNowPlayingInfo(Some(&info));
}

unsafe fn build_info_dict(
    plan: &NowPlayingPlan,
) -> Retained<NSMutableDictionary<NSString, AnyObject>> {
    let dict = NSMutableDictionary::<NSString, AnyObject>::new();

    if let Some(title) = &plan.title {
        set_string(&dict, MPMediaItemPropertyTitle, title);
    }
    if let Some(artist) = &plan.artist {
        set_string(&dict, MPMediaItemPropertyArtist, artist);
    }
    if let Some(album) = &plan.album {
        set_string(&dict, MPMediaItemPropertyAlbumTitle, album);
    }
    if let Some(duration) = plan.duration_secs {
        set_number(&dict, MPMediaItemPropertyPlaybackDuration, duration);
    }
    if let Some(elapsed) = plan.elapsed_secs {
        set_number(&dict, MPNowPlayingInfoPropertyElapsedPlaybackTime, elapsed);
    }
    set_number(&dict, MPNowPlayingInfoPropertyPlaybackRate, plan.rate);

    if let Some(bytes) = COVER.lock().bytes.clone() {
        let artwork = make_artwork(bytes);
        dict.setObject_forKey(&artwork, copying_key(MPMediaItemPropertyArtwork));
    }

    dict
}

unsafe fn set_string(dict: &NSMutableDictionary<NSString, AnyObject>, key: &NSString, value: &str) {
    let value = NSString::from_str(value);
    dict.setObject_forKey(&value, copying_key(key));
}

unsafe fn set_number(dict: &NSMutableDictionary<NSString, AnyObject>, key: &NSString, value: f64) {
    let number = NSNumber::numberWithDouble(value);
    dict.setObject_forKey(&number, copying_key(key));
}

fn copying_key(key: &NSString) -> &ProtocolObject<dyn NSCopying> {
    ProtocolObject::from_ref(key)
}

/// `AppKit` may invoke the request handler on any thread, so it touches only the
/// thread-safe `NSData`/`NSImage` constructors and returns the image autoreleased
/// (`+0`) via [`Retained::autorelease_return`], as the handler's contract expects.
unsafe fn make_artwork(bytes: Arc<Vec<u8>>) -> Retained<MPMediaItemArtwork> {
    let bounds = CGSize {
        width: ARTWORK_SIZE,
        height: ARTWORK_SIZE,
    };
    let handler = RcBlock::new(move |_size: CGSize| -> NonNull<NSImage> {
        let data = NSData::with_bytes(&bytes);
        let image = match NSImage::initWithData(NSImage::alloc(), &data) {
            Some(image) => image,
            None => NSImage::new(),
        };
        NonNull::new(Retained::autorelease_return(image))
            .expect("autoreleased NSImage pointer is non-null")
    });
    MPMediaItemArtwork::initWithBoundsSize_requestHandler(
        MPMediaItemArtwork::alloc(),
        bounds,
        &handler,
    )
}

unsafe fn register_commands() {
    let center = MPRemoteCommandCenter::sharedCommandCenter();
    add_handler(&center.playCommand(), "play");
    add_handler(&center.pauseCommand(), "pause");
    add_handler(&center.togglePlayPauseCommand(), "toggle");
    add_handler(&center.nextTrackCommand(), "next");
    add_handler(&center.previousTrackCommand(), "previous");
    add_handler(&center.stopCommand(), "stop");
}

unsafe fn add_handler(command: &MPRemoteCommand, action: &'static str) {
    command.setEnabled(true);
    let handler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
        if let Some(callback) = CALLBACK.lock().clone() {
            callback(action);
        }
        MPRemoteCommandHandlerStatus::Success
    });
    let _target = command.addTargetWithHandler(&handler);
}
