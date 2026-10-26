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
//! use. `player state` is answered from the cached now-playing state. Volume
//! operations use the selected player's Music Assistant API commands and state.
//!
//! Being a scripting *target* needs no sandbox entitlement — only the
//! `NSAppleScriptEnabled` Info.plist key, which the sender's automation
//! permission gates at runtime.
#![allow(unsafe_code)] // Apple Event Manager is a C API; lift the workspace deny.

use crate::media_controls::MainThreadDispatch;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::raw::c_long;
use std::sync::Arc;
use std::time::Duration;

/// Routes a transport verb (`play`/`pause`/`toggle`/`next`/`previous`/`stop`)
/// to the same handler the OS media controls call.
type Callback = Arc<dyn Fn(&str) + Send + Sync>;
type VolumeCallback =
    Arc<dyn Fn(&str, &'static str, Option<u8>) -> Result<String, String> + Send + Sync>;

static CALLBACK: Mutex<Option<Callback>> = Mutex::new(None);
static VOLUME_CALLBACK: Mutex<Option<VolumeCallback>> = Mutex::new(None);
static DISPATCH: Mutex<Option<MainThreadDispatch>> = Mutex::new(None);
static PENDING: Mutex<Option<HashMap<String, PendingVolume>>> = Mutex::new(None);
const VOLUME_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING: usize = 32;

#[allow(non_camel_case_types)]
type OSErr = i16;
type FourCharCode = u32;
type AEEventClass = FourCharCode;
type AEEventID = FourCharCode;
type AEKeyword = FourCharCode;
type DescType = FourCharCode;

/// Apple Event descriptor; AEDataModel.h packs its fields to 2-byte alignment.
#[allow(dead_code)]
#[repr(C, packed(2))]
struct AEDesc {
    descriptor_type: DescType,
    data_handle: *mut c_void,
}

struct SuspendedDescriptors {
    event: AEDesc,
    reply: AEDesc,
}
// The Apple Event Manager retains this pair after suspension and disposes it on resume.
// Both copies of the descriptor bytes are accessed only on the main run loop.
unsafe impl Send for SuspendedDescriptors {}

struct PendingVolume {
    descriptors: SuspendedDescriptors,
    operation: &'static str,
    origin: String,
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

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn AESuspendTheCurrentEvent(event: *const AEDesc) -> OSErr;
    fn AEResumeTheCurrentEvent(
        event: *const AEDesc,
        reply: *const AEDesc,
        dispatcher: *const c_void,
        handler_refcon: *mut c_void,
    ) -> OSErr;
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
const KEY_AE_DATA: AEKeyword = fourcc(*b"data");
const TYPE_TYPE: DescType = fourcc(*b"type");
const TYPE_ENUMERATED: DescType = fourcc(*b"enum");
const TYPE_SINT32: DescType = fourcc(*b"long");
const TYPE_WILDCARD: DescType = fourcc(*b"****");
const SET_DATA: AEEventID = fourcc(*b"setd");
const ERR_EVENT_NOT_HANDLED: OSErr = -1708;
const ERR_WRONG_TYPE: OSErr = -1703;
const ERR_MISSING_VALUE: OSErr = -1701;
const ERR_INVALID_VALUE: OSErr = -1700;
const ERR_PLAYER_UNAVAILABLE: OSErr = -1728;
const ERR_TIMEOUT: OSErr = -1712;
const KEY_ERROR_NUMBER: AEKeyword = fourcc(*b"errn");
const KEY_ERROR_STRING: AEKeyword = fourcc(*b"errs");
const TYPE_UTF8_TEXT: DescType = fourcc(*b"utf8");

// `player state` property and its values. The codes match Spotify's dictionary
// (`pStt`, `kPSP`/`kPSp`/`kPSS`) and the `MusicAssistant.sdef` `EPlS` enum.
const PROPERTY_PLAYER_STATE: FourCharCode = fourcc(*b"pStt");
const PROPERTY_SOUND_VOLUME: FourCharCode = fourcc(*b"pVol");

fn start_volume(
    event: *const AEDesc,
    reply: *mut AEDesc,
    operation: &'static str,
    value: Option<u8>,
) -> OSErr {
    let Some(callback) = VOLUME_CALLBACK.lock().clone() else {
        return ERR_PLAYER_UNAVAILABLE;
    };
    let mut pending = PENDING.lock();
    let requests = pending.get_or_insert_with(HashMap::new);
    if requests.len() >= MAX_PENDING {
        return ERR_PLAYER_UNAVAILABLE;
    }
    let id = uuid::Uuid::new_v4().to_string();
    let status = unsafe { AESuspendTheCurrentEvent(event) };
    if status != 0 {
        return status;
    }
    requests.insert(
        id.clone(),
        PendingVolume {
            descriptors: SuspendedDescriptors {
                event: unsafe { std::ptr::read_unaligned(event) },
                reply: unsafe { std::ptr::read_unaligned(reply) },
            },
            operation,
            origin: String::new(),
        },
    );
    let dispatched = callback(&id, operation, value);
    if let Ok(origin) = &dispatched {
        requests.get_mut(&id).unwrap().origin.clone_from(origin);
    }
    drop(pending);
    spawn_volume_completion(dispatched, move |error| {
        schedule_completion(&id, None, Some(error), None);
    });
    0
}

fn spawn_volume_completion(
    dispatched: Result<String, String>,
    dispatch: impl FnOnce(String) + Send + 'static,
) {
    std::thread::spawn(move || {
        let error = match dispatched {
            Ok(_) => {
                std::thread::sleep(VOLUME_TIMEOUT);
                "Selected player volume request timed out".to_string()
            }
            Err(error) => {
                log::warn!("[AppleScript] Could not request volume from selected player: {error}");
                error
            }
        };
        // Calling the main-thread dispatcher on the handler thread can run inline.
        dispatch(error);
    });
}

fn schedule_completion(
    id: &str,
    volume: Option<i32>,
    error: Option<String>,
    origin: Option<String>,
) {
    if let Some(dispatch) = DISPATCH.lock().clone() {
        let id = id.to_string();
        dispatch(Box::new(move || {
            complete_on_main(&id, volume, error, origin.as_deref());
        }));
    }
}

fn volume_reply_matches_origin(request: &PendingVolume, origin: Option<&str>) -> bool {
    origin.is_none_or(|origin| request.origin == origin)
}

fn complete_on_main(id: &str, volume: Option<i32>, error: Option<String>, origin: Option<&str>) {
    let mut pending = PENDING.lock();
    let Some(request) = pending.as_mut().and_then(|requests| requests.get(id)) else {
        return;
    };
    if !volume_reply_matches_origin(request, origin) {
        return;
    }
    let request = pending.as_mut().unwrap().remove(id).unwrap();
    drop(pending);
    let mut descriptors = request.descriptors;
    let result = if let Some(error) = error {
        log::warn!("[AppleScript] Volume request failed: {error}");
        let code = if error == "Selected player volume request timed out" {
            ERR_TIMEOUT
        } else {
            ERR_PLAYER_UNAVAILABLE
        };
        put_error(&mut descriptors.reply, code, &error)
    } else if request.operation == "get" {
        match volume.and_then(|value| volume_value(value).ok()) {
            Some(value) => {
                let value = i32::from(value);
                unsafe {
                    AEPutParamPtr(
                        &raw mut descriptors.reply,
                        KEY_DIRECT_OBJECT,
                        TYPE_SINT32,
                        (&raw const value).cast(),
                        4,
                    )
                }
            }
            None => put_error(
                &mut descriptors.reply,
                ERR_PLAYER_UNAVAILABLE,
                "Selected player has no available volume",
            ),
        }
    } else if volume.is_some() {
        put_error(
            &mut descriptors.reply,
            ERR_INVALID_VALUE,
            "Unexpected volume response",
        )
    } else {
        0
    };
    if result != 0 {
        log::warn!("[AppleScript] Could not populate volume reply: {result}");
    }
    let resumed = unsafe {
        AEResumeTheCurrentEvent(
            &raw const descriptors.event,
            &raw const descriptors.reply,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if resumed != 0 {
        log::warn!("[AppleScript] Could not resume volume event: {resumed}");
    }
}

fn put_error(reply: &mut AEDesc, number: OSErr, text: &str) -> OSErr {
    let code = i32::from(number);
    let status = unsafe {
        AEPutParamPtr(
            reply,
            KEY_ERROR_NUMBER,
            TYPE_SINT32,
            (&raw const code).cast(),
            4,
        )
    };
    if status != 0 {
        return status;
    }
    unsafe {
        AEPutParamPtr(
            reply,
            KEY_ERROR_STRING,
            TYPE_UTF8_TEXT,
            text.as_ptr().cast(),
            text.len() as c_long,
        )
    }
}

pub fn complete(request_id: &str, volume: Option<i32>, error: Option<String>, origin: String) {
    schedule_completion(request_id, volume, error, Some(origin));
}

pub fn cancel_pending() {
    let ids = PENDING
        .lock()
        .as_ref()
        .map(|requests| requests.keys().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    for id in ids {
        schedule_completion(&id, None, Some("Player page changed".to_string()), None);
    }
}

fn volume_value(value: i32) -> Result<u8, OSErr> {
    u8::try_from(value)
        .ok()
        .filter(|&value| value <= 100)
        .ok_or(ERR_INVALID_VALUE)
}

fn property_code(event: *const AEDesc) -> Result<FourCharCode, OSErr> {
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
        return Err(ERR_EVENT_NOT_HANDLED);
    }
    let mut property: FourCharCode = 0;
    let mut actual_type: DescType = 0;
    let mut actual_size: c_long = 0;
    let status = unsafe {
        AEGetParamPtr(
            &raw const direct_object,
            KEY_AE_KEY_DATA,
            TYPE_TYPE,
            (&raw mut actual_type).cast(),
            (&raw mut property).cast(),
            std::mem::size_of::<FourCharCode>() as c_long,
            &raw mut actual_size,
        )
    };
    unsafe { AEDisposeDesc(&raw mut direct_object) };
    if status != 0 || actual_size != 4 || actual_type != TYPE_TYPE {
        return Err(ERR_EVENT_NOT_HANDLED);
    }
    Ok(property)
}
const STATE_PLAYING: FourCharCode = fourcc(*b"kPSP");
const STATE_PAUSED: FourCharCode = fourcc(*b"kPSp");
const STATE_STOPPED: FourCharCode = fourcc(*b"kPSS");

extern "C" fn handle_command(
    event: *const AEDesc,
    reply: *mut AEDesc,
    refcon: *mut c_void,
) -> OSErr {
    if refcon as usize == COMMANDS.len() {
        return start_volume(event, reply, "up", None);
    }
    if refcon as usize == COMMANDS.len() + 1 {
        return start_volume(event, reply, "down", None);
    }
    if let Some(&(_, verb)) = COMMANDS.get(refcon as usize) {
        if let Some(callback) = CALLBACK.lock().clone() {
            callback(verb);
            return 0;
        }
    }
    ERR_EVENT_NOT_HANDLED
}

/// Answers `get <property>` for the app; unknown properties are not handled.
extern "C" fn handle_get_data(
    event: *const AEDesc,
    reply: *mut AEDesc,
    _refcon: *mut c_void,
) -> OSErr {
    let property = match property_code(event) {
        Ok(property) => property,
        Err(error) => return error,
    };
    match property {
        PROPERTY_PLAYER_STATE => {
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
                )
            }
        }
        PROPERTY_SOUND_VOLUME => start_volume(event, reply, "get", None),
        _ => ERR_EVENT_NOT_HANDLED,
    }
}

extern "C" fn handle_set_data(
    event: *const AEDesc,
    reply: *mut AEDesc,
    _refcon: *mut c_void,
) -> OSErr {
    if property_code(event) != Ok(PROPERTY_SOUND_VOLUME) {
        return ERR_EVENT_NOT_HANDLED;
    }
    let mut value: i32 = 0;
    let mut actual_type: DescType = 0;
    let mut actual_size: c_long = 0;
    let status = unsafe {
        AEGetParamPtr(
            event,
            KEY_AE_DATA,
            TYPE_SINT32,
            (&raw mut actual_type).cast(),
            (&raw mut value).cast(),
            std::mem::size_of::<i32>() as c_long,
            &raw mut actual_size,
        )
    };
    if status != 0 {
        return if status == ERR_MISSING_VALUE {
            status
        } else {
            ERR_WRONG_TYPE
        };
    }
    if actual_size != 4 || actual_type != TYPE_SINT32 {
        return ERR_WRONG_TYPE;
    }
    match volume_value(value) {
        Ok(volume) => start_volume(event, reply, "set", Some(volume)),
        Err(error) => error,
    }
}

/// Store the callback and install the Apple Event handlers. Handler
/// registration is process-wide and lives for the whole app lifetime, so this
/// runs once; the dispatch keeps it on the main run loop for parity with the
/// media-controls backend.
pub fn init(callback: Callback, volume_callback: VolumeCallback, dispatch: MainThreadDispatch) {
    *CALLBACK.lock() = Some(callback);
    *VOLUME_CALLBACK.lock() = Some(volume_callback);
    *DISPATCH.lock() = Some(dispatch.clone());
    dispatch(Box::new(|| unsafe {
        for (index, (event_id, _)) in COMMANDS.iter().enumerate() {
            let status =
                AEInstallEventHandler(SUITE, *event_id, handle_command, index as *mut c_void, 0);
            if status != 0 {
                log::warn!("[AppleScript] Failed to install handler {index}: {status}");
            }
        }
        for (offset, event_id) in [fourcc(*b"MAvi"), fourcc(*b"MAvd")].iter().enumerate() {
            let status = AEInstallEventHandler(
                SUITE,
                *event_id,
                handle_command,
                (COMMANDS.len() + offset) as *mut c_void,
                0,
            );
            if status != 0 {
                log::warn!("[AppleScript] Failed to install volume handler: {status}");
            }
        }
        let status = AEInstallEventHandler(
            CORE_SUITE,
            SET_DATA,
            handle_set_data,
            std::ptr::null_mut(),
            0,
        );
        if status != 0 {
            log::warn!("[AppleScript] Failed to install set-data handler: {status}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_event_descriptor_matches_sdk_packing() {
        assert_eq!(std::mem::size_of::<AEDesc>(), 12);
        assert_eq!(std::mem::offset_of!(AEDesc, data_handle), 4);
    }

    #[test]
    fn volume_event_codes_match_dictionary() {
        let dictionary = include_str!("../MusicAssistant.sdef");
        assert!(dictionary.contains("name=\"sound volume\" code=\"pVol\""));
        assert!(dictionary.contains("name=\"increase volume\" code=\"MAscMAvi\""));
        assert!(dictionary.contains("name=\"decrease volume\" code=\"MAscMAvd\""));
        assert_eq!(PROPERTY_SOUND_VOLUME, fourcc(*b"pVol"));
        assert_eq!(KEY_AE_DATA, fourcc(*b"data"));
    }

    #[test]
    fn pending_volume_reply_checks_stored_origin() {
        let request = PendingVolume {
            descriptors: SuspendedDescriptors {
                event: AEDesc {
                    descriptor_type: 0,
                    data_handle: std::ptr::null_mut(),
                },
                reply: AEDesc {
                    descriptor_type: 0,
                    data_handle: std::ptr::null_mut(),
                },
            },
            operation: "get",
            origin: "https://player.example:8443".into(),
        };
        assert!(volume_reply_matches_origin(
            &request,
            Some("https://player.example:8443")
        ));
        assert!(!volume_reply_matches_origin(
            &request,
            Some("https://player.example:9443")
        ));
        assert!(!volume_reply_matches_origin(
            &request,
            Some("http://player.example:8443")
        ));
        assert!(!volume_reply_matches_origin(
            &request,
            Some("https://elsewhere.example:8443")
        ));
        assert!(volume_reply_matches_origin(&request, None));
    }

    #[test]
    fn immediate_volume_dispatch_error_is_handed_off_to_worker() {
        let handler_thread = std::thread::current().id();
        let (sender, receiver) = std::sync::mpsc::channel();
        spawn_volume_completion(Err("No player window".into()), move |error| {
            sender.send((std::thread::current().id(), error)).unwrap();
        });
        let (completion_thread, error) = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_ne!(completion_thread, handler_thread);
        assert_eq!(error, "No player window");
    }

    #[test]
    fn volume_value_accepts_only_whole_percent_range() {
        assert_eq!(volume_value(0), Ok(0));
        assert_eq!(volume_value(100), Ok(100));
        for invalid in [-1, 101, i32::MIN, i32::MAX] {
            assert_eq!(volume_value(invalid), Err(ERR_INVALID_VALUE));
        }
    }
}
