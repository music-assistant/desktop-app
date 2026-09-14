//! macOS `AppleScript` bridge.
//!
//! Makes the app answer Apple Events like `tell application "Music Assistant"
//! to pause` and `tell application "Music Assistant" to player state`. The
//! scripting terminology is declared in `MusicAssistant.sdef` (bundled into
//! `Contents/Resources` and referenced from `Info.plist` via
//! `OSAScriptingDefinition`); this module installs the matching Apple Event
//! handlers.
//!
//! Transport verbs reach the same player-command callback the OS media controls
//! use. The read-only `player state` property is answered directly from the
//! cached now-playing state, matching Spotify's `playing`/`paused`/`stopped`
//! terminology.
//!
//! Being a scripting *target* needs no sandbox entitlement — only the
//! `NSAppleScriptEnabled` Info.plist key, which the sender's automation
//! permission gates at runtime.
#![allow(unsafe_code)] // Apple Event Manager is a C API; lift the workspace deny.

use crate::media_controls::MainThreadDispatch;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::os::raw::c_long;
use std::sync::Arc;

/// Routes a transport verb (`play`/`pause`/`toggle`/`next`/`previous`/`stop`)
/// to the same handler the OS media controls call.
type Callback = Arc<dyn Fn(&str) + Send + Sync>;

static CALLBACK: Mutex<Option<Callback>> = Mutex::new(None);

#[allow(non_camel_case_types)]
type OSErr = i16;
type FourCharCode = u32;
type AEEventClass = FourCharCode;
type AEEventID = FourCharCode;
type AEKeyword = FourCharCode;
type DescType = FourCharCode;

/// Apple Event descriptor. The fields exist only to match the C layout so the
/// Apple Event Manager can fill one in; Rust never reads them.
#[allow(dead_code)]
#[repr(C)]
struct AEDesc {
    descriptor_type: DescType,
    data_handle: *mut c_void,
}

type AEEventHandlerProcPtr = extern "C" fn(*const AEDesc, *mut AEDesc, *mut c_void) -> OSErr;

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn AEInstallEventHandler(
        event_class: AEEventClass,
        event_id: AEEventID,
        handler: AEEventHandlerProcPtr,
        handler_refcon: *mut c_void,
        is_sys_handler: u8,
    ) -> OSErr;

    fn AEGetParamDesc(
        event: *const AEDesc,
        keyword: AEKeyword,
        desired_type: DescType,
        result: *mut AEDesc,
    ) -> OSErr;

    // An AERecord is an AppleEvent, so the `...KeyPtr` accessor is a C macro
    // aliased to `AEGetParamPtr`; only the latter is a real linkable symbol.
    fn AEGetParamPtr(
        record: *const AEDesc,
        keyword: AEKeyword,
        desired_type: DescType,
        actual_type: *mut DescType,
        data: *mut c_void,
        maximum_size: c_long,
        actual_size: *mut c_long,
    ) -> OSErr;

    fn AEPutParamPtr(
        event: *mut AEDesc,
        keyword: AEKeyword,
        type_code: DescType,
        data: *const c_void,
        size: c_long,
    ) -> OSErr;

    fn AEDisposeDesc(desc: *mut AEDesc) -> OSErr;
}

const fn fourcc(code: [u8; 4]) -> FourCharCode {
    u32::from_be_bytes(code)
}

/// Event class shared by every transport verb. Must match the `code` prefix of
/// each `<command>` in `MusicAssistant.sdef`.
const SUITE: AEEventClass = fourcc(*b"MAsc");

/// `(event id, callback verb)` for each transport command, indexed by the
/// refcon passed to [`AEInstallEventHandler`]. The event ids must match the
/// `code` suffixes in `MusicAssistant.sdef`.
const COMMANDS: &[(AEEventID, &str)] = &[
    (fourcc(*b"MApl"), "play"),
    (fourcc(*b"MApa"), "pause"),
    (fourcc(*b"MApp"), "toggle"),
    (fourcc(*b"MAnx"), "next"),
    (fourcc(*b"MApv"), "previous"),
    (fourcc(*b"MAst"), "stop"),
];

// Apple Event Manager constants for answering a property `get`.
const CORE_SUITE: AEEventClass = fourcc(*b"core");
const GET_DATA: AEEventID = fourcc(*b"getd");
const KEY_DIRECT_OBJECT: AEKeyword = fourcc(*b"----");
const KEY_AE_KEY_DATA: AEKeyword = fourcc(*b"seld");
const TYPE_TYPE: DescType = fourcc(*b"type");
const TYPE_ENUMERATED: DescType = fourcc(*b"enum");
const TYPE_WILDCARD: DescType = fourcc(*b"****");
const ERR_EVENT_NOT_HANDLED: OSErr = -1708;

// `player state` property and its values. The codes match Spotify's dictionary
// (`pStt`, `kPSP`/`kPSp`/`kPSS`) and the `MusicAssistant.sdef` `EPlS` enum.
const PROPERTY_PLAYER_STATE: FourCharCode = fourcc(*b"pStt");
const STATE_PLAYING: FourCharCode = fourcc(*b"kPSP");
const STATE_PAUSED: FourCharCode = fourcc(*b"kPSp");
const STATE_STOPPED: FourCharCode = fourcc(*b"kPSS");

extern "C" fn handle_command(
    _event: *const AEDesc,
    _reply: *mut AEDesc,
    refcon: *mut c_void,
) -> OSErr {
    if let Some(&(_, verb)) = COMMANDS.get(refcon as usize) {
        if let Some(callback) = CALLBACK.lock().clone() {
            callback(verb);
        }
    }
    0 // noErr
}

/// Answers `get <property>` for the app. Only `player state` is exposed;
/// anything else is reported as not handled so the script sees a clean error.
extern "C" fn handle_get_data(
    event: *const AEDesc,
    reply: *mut AEDesc,
    _refcon: *mut c_void,
) -> OSErr {
    let mut direct_object = AEDesc {
        descriptor_type: 0,
        data_handle: std::ptr::null_mut(),
    };
    if unsafe {
        AEGetParamDesc(
            event,
            KEY_DIRECT_OBJECT,
            TYPE_WILDCARD,
            &raw mut direct_object,
        )
    } != 0
    {
        return ERR_EVENT_NOT_HANDLED;
    }

    let mut property: FourCharCode = 0;
    let mut actual_type: DescType = 0;
    let mut actual_size: c_long = 0;
    let status = unsafe {
        AEGetParamPtr(
            &raw const direct_object,
            KEY_AE_KEY_DATA,
            TYPE_TYPE,
            &raw mut actual_type,
            (&raw mut property).cast(),
            std::mem::size_of::<FourCharCode>() as c_long,
            &raw mut actual_size,
        )
    };
    unsafe { AEDisposeDesc(&raw mut direct_object) };

    if status != 0 || property != PROPERTY_PLAYER_STATE {
        return ERR_EVENT_NOT_HANDLED;
    }

    let np = crate::now_playing::get_now_playing();
    let state = if np.is_playing {
        STATE_PLAYING
    } else if np.track.is_some() {
        STATE_PAUSED
    } else {
        STATE_STOPPED
    };
    unsafe {
        AEPutParamPtr(
            reply,
            KEY_DIRECT_OBJECT,
            TYPE_ENUMERATED,
            (&raw const state).cast(),
            std::mem::size_of::<FourCharCode>() as c_long,
        );
    }
    0 // noErr
}

/// Store the callback and install the Apple Event handlers. Handler
/// registration is process-wide and lives for the whole app lifetime, so this
/// runs once; the dispatch keeps it on the main run loop for parity with the
/// media-controls backend.
pub fn init(callback: Callback, dispatch: MainThreadDispatch) {
    *CALLBACK.lock() = Some(callback);
    dispatch(Box::new(|| unsafe {
        for (index, (event_id, _)) in COMMANDS.iter().enumerate() {
            let status =
                AEInstallEventHandler(SUITE, *event_id, handle_command, index as *mut c_void, 0);
            if status != 0 {
                log::warn!("[AppleScript] Failed to install handler {index}: {status}");
            }
        }
        let status = AEInstallEventHandler(
            CORE_SUITE,
            GET_DATA,
            handle_get_data,
            std::ptr::null_mut(),
            0,
        );
        if status != 0 {
            log::warn!("[AppleScript] Failed to install get-data handler: {status}");
        }
    }));
}
