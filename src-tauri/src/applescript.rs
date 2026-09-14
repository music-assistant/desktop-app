//! macOS `AppleScript` bridge.
//!
//! Makes the app answer Apple Events like `tell application "Music Assistant"
//! to pause`. The scripting terminology is declared in `MusicAssistant.sdef`
//! (bundled into `Contents/Resources` and referenced from `Info.plist` via
//! `OSAScriptingDefinition`); this module installs the matching Apple Event
//! handlers so those verbs reach the same player-command callback the OS media
//! controls use.
//!
//! Being a scripting *target* needs no sandbox entitlement — only the
//! `NSAppleScriptEnabled` Info.plist key, which the sender's automation
//! permission gates at runtime.
#![allow(unsafe_code)] // Apple Event Manager is a C API; lift the workspace deny.

use crate::media_controls::MainThreadDispatch;
use parking_lot::Mutex;
use std::ffi::c_void;
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

// The event and reply are `AEDesc` pointers, but the transport verbs carry no
// parameters and produce no reply, so they stay opaque and are never read.
type AEEventHandlerProcPtr = extern "C" fn(*const c_void, *mut c_void, *mut c_void) -> OSErr;

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn AEInstallEventHandler(
        event_class: AEEventClass,
        event_id: AEEventID,
        handler: AEEventHandlerProcPtr,
        handler_refcon: *mut c_void,
        is_sys_handler: u8,
    ) -> OSErr;
}

const fn fourcc(code: [u8; 4]) -> FourCharCode {
    u32::from_be_bytes(code)
}

/// Event class shared by every verb. Must match the `code` prefix of each
/// `<command>` in `MusicAssistant.sdef`.
const SUITE: AEEventClass = fourcc(*b"MAsc");

/// `(event id, callback verb)` for each command, indexed by the refcon passed
/// to [`AEInstallEventHandler`]. The event ids must match the `code` suffixes
/// in `MusicAssistant.sdef`.
const COMMANDS: &[(AEEventID, &str)] = &[
    (fourcc(*b"MApl"), "play"),
    (fourcc(*b"MApa"), "pause"),
    (fourcc(*b"MApp"), "toggle"),
    (fourcc(*b"MAnx"), "next"),
    (fourcc(*b"MApv"), "previous"),
    (fourcc(*b"MAst"), "stop"),
];

extern "C" fn handle_event(
    _event: *const c_void,
    _reply: *mut c_void,
    refcon: *mut c_void,
) -> OSErr {
    if let Some(&(_, verb)) = COMMANDS.get(refcon as usize) {
        if let Some(callback) = CALLBACK.lock().clone() {
            callback(verb);
        }
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
                AEInstallEventHandler(SUITE, *event_id, handle_event, index as *mut c_void, 0);
            if status != 0 {
                log::warn!("[AppleScript] Failed to install handler {index}: {status}");
            }
        }
    }));
}
