//! Native macOS media-controls backend: drives `MPNowPlayingInfoCenter` and
//! `MPRemoteCommandCenter` directly via objc2, replacing the `souvlaki` crate
//! that crashed the app on unloadable cover URLs.
//!
//! Every objc2 call must run on the `NSApplication` main run loop, reached via
//! the [`MainThreadDispatch`](super::MainThreadDispatch) given to [`init`];
//! `AppKit` delivers remote-command handlers there too. Nothing objc2 is kept in
//! a static (those types are `!Send`) — only plain data is, and the framework
//! singletons are re-fetched inside each main-thread closure. (The one
//! main-thread-only objc2 cache, the decoded artwork, lives in a
//! `thread_local` instead.)
#![allow(unsafe_code)] // objc2 framework methods are all `unsafe`; lift the workspace deny.

use super::{MainThreadDispatch, MediaControlCallback, NowPlayingPlan, PlaybackState};
use crate::now_playing::NowPlaying;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::AnyThread;
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{
    NSCopying, NSData, NSDataBase64DecodingOptions, NSMutableDictionary, NSNumber, NSString, NSURL,
};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle,
    MPNowPlayingInfoCenter, MPNowPlayingInfoMediaType, MPNowPlayingInfoPropertyDefaultPlaybackRate,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyIsLiveStream,
    MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Fallback artwork bounds when the decoded image reports a degenerate size.
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

thread_local! {
    /// Main-thread cache of the `MPMediaItemArtwork` for the current cover
    /// bytes (keyed by the `Arc` pointer identity). Rebuilding the artwork
    /// object on every progress tick would make the system re-request and
    /// re-decode the image each time; caching also remembers a failed decode
    /// so undecodable bytes are validated once and then omitted instead of
    /// rendering a blank tile.
    static ARTWORK_CACHE: RefCell<Option<CachedArtwork>> = const { RefCell::new(None) };
}

struct CachedArtwork {
    /// The cover bytes this entry was built from. Held (not just their
    /// pointer) so the allocation cannot be freed and reused while the entry
    /// lives — comparing raw `Arc::as_ptr` keys without retaining would let a
    /// later cover alias a stale entry (ABA), in particular hitting a cached
    /// failed-decode `None` for perfectly decodable new bytes.
    bytes: Arc<Vec<u8>>,
    /// `None` records that the bytes failed to decode as an image.
    artwork: Option<Retained<MPMediaItemArtwork>>,
}

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
    // Publish the plan before kicking off the cover fetch: a near-instant
    // fetch (data:/file: URL) can complete and dispatch a render before this
    // function finishes, and that render must not observe the previous
    // track's plan with the new track's artwork.
    *LAST_PLAN.lock() = Some(plan.clone());
    refresh_cover_if_changed(&plan);
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
        let center = MPNowPlayingInfoCenter::defaultCenter();
        center.setNowPlayingInfo(None);
        // `playbackState` is the macOS-specific app-level signal (Control
        // Center visibility, media-key routing); it must be cleared too.
        center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        // Enablement is per-item; with no item, nothing is actionable.
        apply_command_enablement(&super::plan(&NowPlaying::default()));
        // Drop the retained artwork object along with the cover it belonged to.
        ARTWORK_CACHE.with(|cache| *cache.borrow_mut() = None);
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

    std::thread::spawn(move || match fetch_cover(&url) {
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
        Err(e) => log::warn!("[MediaControls] cover fetch failed for {url}: {e}"),
    });
}

/// Fetch cover bytes for any of the URL forms the upstream may hand us.
/// Apple natively supports `data:` URLs for artwork, and local `file:` URLs
/// are cheaper than a loopback HTTP fetch; everything else goes over HTTP.
fn fetch_cover(url: &str) -> Result<Vec<u8>, String> {
    if let Some(rest) = url.strip_prefix("data:") {
        decode_data_url(rest)
    } else if url.starts_with("file:") {
        read_file_url(url)
    } else {
        download_image(url).map_err(|e| e.to_string())
    }
}

/// Decode the payload of a `data:` URL (the part after `data:`). Only the
/// base64 form is supported; covers are binary image data in practice.
fn decode_data_url(rest: &str) -> Result<Vec<u8>, String> {
    let (media_type, payload) = rest
        .split_once(',')
        .ok_or_else(|| "malformed data: URL (no comma)".to_string())?;
    if !media_type.ends_with(";base64") {
        return Err("only base64-encoded data: URLs are supported".to_string());
    }
    // RFC 2397 allows URL-encoded payloads ("+" as "%2B" etc.); decode those
    // first, then decode base64 STRICTLY — a lenient decoder that skips
    // unknown characters would silently turn a still-percent-encoded payload
    // into garbage bytes instead of an error.
    let payload = percent_decode(payload);
    // NSData's decoder avoids adding a base64 dependency; NSData/NSString are
    // immutable and safe to use off the main thread.
    let payload = NSString::from_str(&payload);
    let data = NSData::initWithBase64EncodedString_options(
        NSData::alloc(),
        &payload,
        NSDataBase64DecodingOptions::empty(),
    )
    .ok_or_else(|| "invalid base64 payload in data: URL".to_string())?;
    Ok(data.to_vec())
}

/// Percent-decode a data:-URL payload. Valid `%XX` escapes are decoded;
/// anything else passes through unchanged. Base64 payloads are ASCII, so
/// decoded escapes are folded back into the string lossily.
fn percent_decode(payload: &str) -> String {
    fn hex_digit(byte: u8) -> Option<u8> {
        (byte as char).to_digit(16).map(|d| d as u8)
    }

    let bytes = payload.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read a `file:` URL, letting NSURL handle percent-decoding of the path.
fn read_file_url(url: &str) -> Result<Vec<u8>, String> {
    let parsed = NSURL::URLWithString(&NSString::from_str(url))
        .ok_or_else(|| "invalid file: URL".to_string())?;
    let path = parsed
        .path()
        .ok_or_else(|| "file: URL has no path".to_string())?
        .to_string();
    std::fs::read(&path).map_err(|e| e.to_string())
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

    // Remote-command availability must track the current item; commands left
    // permanently enabled would offer next/previous for non-skippable queues.
    apply_command_enablement(&plan);

    if plan.state == PlaybackState::Stopped {
        center.setNowPlayingInfo(None);
        center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        return;
    }
    let info = build_info_dict(&plan);
    center.setNowPlayingInfo(Some(&info));
    // On macOS, `playbackState` (not the playback rate) is what drives
    // Control Center / menu-bar Now Playing state and makes this app the
    // current now-playing app for media keys and AirPods controls. Set it
    // after the info dictionary so the state change observes fresh metadata.
    center.setPlaybackState(match plan.state {
        PlaybackState::Playing => MPNowPlayingPlaybackState::Playing,
        PlaybackState::Paused => MPNowPlayingPlaybackState::Paused,
        PlaybackState::Stopped => MPNowPlayingPlaybackState::Stopped,
    });
}

unsafe fn apply_command_enablement(plan: &NowPlayingPlan) {
    let center = MPRemoteCommandCenter::sharedCommandCenter();
    center.playCommand().setEnabled(plan.can_play);
    center.pauseCommand().setEnabled(plan.can_pause);
    center
        .togglePlayPauseCommand()
        .setEnabled(plan.can_play || plan.can_pause);
    center.nextTrackCommand().setEnabled(plan.can_next);
    center.previousTrackCommand().setEnabled(plan.can_previous);
    center.stopCommand().setEnabled(plan.title.is_some());
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
    set_number(&dict, MPNowPlayingInfoPropertyDefaultPlaybackRate, 1.0);

    // We are a music app; the default media type is "none", which the system
    // treats as untyped media.
    let media_type = NSNumber::numberWithUnsignedInteger(MPNowPlayingInfoMediaType::Audio.0);
    dict.setObject_forKey(&media_type, copying_key(MPNowPlayingInfoPropertyMediaType));

    // A live stream gets the live UI in Control Center instead of an
    // indeterminate scrubber for an unknown-length item.
    if plan.live {
        let live = NSNumber::numberWithBool(true);
        dict.setObject_forKey(&live, copying_key(MPNowPlayingInfoPropertyIsLiveStream));
    }

    if let Some(bytes) = COVER.lock().bytes.clone() {
        if let Some(artwork) = cached_artwork(bytes) {
            dict.setObject_forKey(&artwork, copying_key(MPMediaItemPropertyArtwork));
        }
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

/// Artwork for the given cover bytes, built (and the bytes validated) at most
/// once per cover. Must be called on the main thread. Returns `None` when the
/// bytes are not decodable as an image, so a broken cover is omitted instead
/// of showing a blank artwork tile.
unsafe fn cached_artwork(bytes: Arc<Vec<u8>>) -> Option<Retained<MPMediaItemArtwork>> {
    ARTWORK_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(cached) = cache.as_ref() {
            if Arc::ptr_eq(&cached.bytes, &bytes) {
                return cached.artwork.clone();
            }
        }

        // Probe-decode once to validate the bytes and learn the image's
        // natural bounds; the advertised bounds must agree with what the
        // request handler actually returns.
        let data = NSData::with_bytes(&bytes);
        let artwork = if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
            Some(make_artwork(bytes.clone(), image.size()))
        } else {
            log::warn!("[MediaControls] cover bytes are not a decodable image; omitting artwork");
            None
        };

        let result = artwork.clone();
        *cache = Some(CachedArtwork { bytes, artwork });
        result
    })
}

/// `AppKit` may invoke the request handler on any thread, so it touches only the
/// thread-safe `NSData`/`NSImage` constructors and returns the image autoreleased
/// (`+0`) via [`Retained::autorelease_return`], as the handler's contract expects.
unsafe fn make_artwork(bytes: Arc<Vec<u8>>, natural_size: CGSize) -> Retained<MPMediaItemArtwork> {
    let bounds = if natural_size.width > 0.0 && natural_size.height > 0.0 {
        natural_size
    } else {
        CGSize {
            width: ARTWORK_SIZE,
            height: ARTWORK_SIZE,
        }
    };
    let handler = RcBlock::new(move |size: CGSize| -> NonNull<NSImage> {
        let data = NSData::with_bytes(&bytes);
        let image = match NSImage::initWithData(NSImage::alloc(), &data) {
            Some(image) => {
                // The handler contract is to return an image at the requested
                // size; NSImage scales at draw time via its `size` property.
                if size.width > 0.0 && size.height > 0.0 {
                    image.setSize(size);
                }
                image
            }
            // Decode raced a cover change; an empty image keeps the contract
            // (the bytes were already validated once in `cached_artwork`).
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

/// Registers the handler and starts the command disabled; enablement is
/// driven per-update by [`apply_command_enablement`]. The target returned by
/// `addTargetWithHandler` is intentionally dropped: the registration lives
/// for the whole app lifetime and is never detached (`removeTarget` would
/// need the token, which we don't keep).
unsafe fn add_handler(command: &MPRemoteCommand, action: &'static str) {
    command.setEnabled(false);
    let handler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
        // A command can still arrive with nothing actionable (e.g. queued
        // events right after the last track was cleared); report that instead
        // of claiming success.
        let actionable = LAST_PLAN.lock().as_ref().is_some_and(|p| p.title.is_some());
        if !actionable {
            return MPRemoteCommandHandlerStatus::NoActionableNowPlayingItem;
        }
        if let Some(callback) = CALLBACK.lock().clone() {
            callback(action);
        }
        MPRemoteCommandHandlerStatus::Success
    });
    let _target = command.addTargetWithHandler(&handler);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decode_handles_escapes_and_passthrough() {
        assert_eq!(percent_decode("AA%2B%2fBB"), "AA+/BB");
        assert_eq!(percent_decode("plain=="), "plain==");
        // Invalid/truncated escapes pass through untouched.
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("%zz"), "%zz");
        // Non-ASCII input must not panic.
        assert_eq!(percent_decode("é%41"), "éA");
    }

    #[test]
    fn decode_data_url_accepts_base64_and_rejects_other_forms() {
        // "TUE=" is the base64 encoding of the bytes "MA".
        assert_eq!(
            decode_data_url("image/png;base64,TUE=").unwrap(),
            b"MA".to_vec()
        );
        // Percent-encoded payload ("+" as %2B) decodes correctly: "+w==" => [0xFB].
        assert_eq!(
            decode_data_url("image/png;base64,%2Bw==").unwrap(),
            vec![0xFB]
        );
        // Non-base64 form and malformed URLs are rejected, not corrupted.
        assert!(decode_data_url("image/png,rawdata").is_err());
        assert!(decode_data_url("no-comma-here").is_err());
        // Strict decoding: garbage payload errors instead of silently
        // decoding a subset of the characters.
        assert!(decode_data_url("image/png;base64,@@@@").is_err());
    }
}
