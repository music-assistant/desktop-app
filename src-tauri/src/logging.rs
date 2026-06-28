//! Centralized logging configuration for the desktop app.
//!
//! Every module logs through the `log` crate macros (`log::trace!`,
//! `log::debug!`, `log::info!`, `log::warn!`, `log::error!`). Output lands in
//! the rotating log file that the "Open log file" tray item reveals, so a user
//! can attach it to a GitHub issue. Raw `println!`/`eprintln!` bypass that file
//! and must not be used for diagnostics.
//!
//! Verbosity is controlled by a single persisted setting (`debug_logging`):
//!
//! * **off** — only `Info` and above is recorded (the shipping default).
//! * **on** — `Debug`/`Trace` from our own crate is recorded as well;
//!   third-party crates are capped at `Debug` so the file stays readable.
//!
//! Toggling the setting takes effect immediately (live toggle) via
//! [`set_debug_enabled`], and the persisted value is applied at startup so the
//! debug log captures early connection/startup errors.

use std::sync::atomic::{AtomicBool, Ordering};

use log::{Level, LevelFilter, Metadata};
use tauri_plugin_log::{Builder, RotationStrategy, Target, TargetKind};

/// Base name of the release log file (the log plugin appends ".log"). Shared by
/// the `LogDir` target and the "Open log file" tray handler so they stay in
/// sync.
pub const LOG_FILE_STEM: &str = "logs";

/// Maximum size of a single log file before rotation (5 MB).
const MAX_FILE_SIZE: u128 = 5 * 1024 * 1024;

/// Target prefix identifying records that originate from this crate.
const OWN_TARGET_PREFIX: &str = "app_lib";

static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable or disable verbose debug logging at runtime.
///
/// Also updates the global max level so that, when disabled, `debug!`/`trace!`
/// invocations short-circuit before formatting their arguments.
pub fn set_debug_enabled(enabled: bool) {
    DEBUG_ENABLED.store(enabled, Ordering::SeqCst);
    log::set_max_level(if enabled {
        LevelFilter::Trace
    } else {
        LevelFilter::Info
    });
}

/// Whether verbose debug logging is currently enabled.
pub fn debug_enabled() -> bool {
    DEBUG_ENABLED.load(Ordering::SeqCst)
}

/// Decide whether a record should be written, honoring the live debug toggle.
fn should_log(metadata: &Metadata<'_>) -> bool {
    let level = metadata.level();
    // Info/Warn/Error are always recorded.
    if level <= Level::Info {
        return true;
    }
    // Debug/Trace only when the user has opted into verbose logging.
    if !debug_enabled() {
        return false;
    }
    // Our own crate may emit Trace; keep third-party crates at Debug so the
    // file does not drown in framework internals.
    if metadata.target().starts_with(OWN_TARGET_PREFIX) {
        true
    } else {
        level <= Level::Debug
    }
}

/// Build the configured `tauri-plugin-log` plugin.
pub fn build_plugin<R: tauri::Runtime>(debug_logging: bool) -> tauri::plugin::TauriPlugin<R> {
    // Seed the runtime toggle before any records can flow through `should_log`.
    set_debug_enabled(debug_logging);

    let mut builder = Builder::default()
        .targets([Target::new(TargetKind::LogDir {
            file_name: Some(LOG_FILE_STEM.to_string()),
        })])
        .max_file_size(MAX_FILE_SIZE)
        .rotation_strategy(RotationStrategy::KeepSome(2))
        // Allow every level through the static filter; `should_log` performs the
        // live, fine-grained gating based on the debug toggle.
        .level(LevelFilter::Trace)
        // Cap known-noisy dependencies that drown out useful signal.
        .level_for("ureq", LevelFilter::Info)
        .level_for("ureq_proto", LevelFilter::Info)
        .filter(should_log);

    if cfg!(debug_assertions) {
        builder = builder.target(Target::new(TargetKind::Stdout));
    }

    builder.build()
}

/// Re-apply the persisted verbosity after the plugin has been installed.
pub fn apply_after_install(debug_logging: bool) {
    set_debug_enabled(debug_logging);
}
