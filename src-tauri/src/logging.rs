//! Centralized logging configuration for the desktop app.
//!
//! Every module logs through the `log` crate macros (`log::trace!`,
//! `log::debug!`, `log::info!`, `log::warn!`, `log::error!`). Output lands in
//! the rotating log file that the "Open log file" tray item reveals, so a user
//! can attach it to a GitHub issue. Raw `println!`/`eprintln!` bypass that file
//! and must not be used for diagnostics.
//!
//! Verbosity is controlled by persisted settings:
//!
//! * **debug off** — only `Info` and above is recorded (the shipping default).
//! * **debug on** — `Debug` is recorded.
//! * **trace on** — `Trace` is recorded, except for dependencies explicitly capped below.
//!
//! Toggling settings takes effect immediately (live toggle) via
//! [`set_verbosity`], and persisted values are applied at startup so verbose
//! logging can capture early connection/startup errors.

use std::sync::atomic::{AtomicU8, Ordering};

use log::{Level, LevelFilter, Metadata};
use tauri_plugin_log::{Builder, RotationStrategy, Target, TargetKind};

/// Base name of the release log file (the log plugin appends ".log"). Shared by
/// the `LogDir` target and the "Open log file" tray handler so they stay in
/// sync.
pub const LOG_FILE_STEM: &str = "logs";

/// Maximum size of a single log file before rotation (5 MB).
const MAX_FILE_SIZE: u128 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogVerbosity {
    Info,
    Debug,
    Trace,
}

impl LogVerbosity {
    fn as_u8(self) -> u8 {
        match self {
            Self::Info => 0,
            Self::Debug => 1,
            Self::Trace => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            2 => Self::Trace,
            1 => Self::Debug,
            _ => Self::Info,
        }
    }

    fn level_filter(self) -> LevelFilter {
        match self {
            Self::Info => LevelFilter::Info,
            Self::Debug => LevelFilter::Debug,
            Self::Trace => LevelFilter::Trace,
        }
    }
}

static VERBOSITY: AtomicU8 = AtomicU8::new(0);

/// Resolve persisted settings into one effective logging verbosity.
pub fn verbosity_from_settings(debug_logging: bool, trace_logging: bool) -> LogVerbosity {
    if !debug_logging {
        LogVerbosity::Info
    } else if trace_logging {
        LogVerbosity::Trace
    } else {
        LogVerbosity::Debug
    }
}

/// Set runtime logging verbosity.
///
/// Also updates the global max level so disabled `debug!`/`trace!` invocations
/// short-circuit before formatting their arguments.
pub fn set_verbosity(verbosity: LogVerbosity) {
    VERBOSITY.store(verbosity.as_u8(), Ordering::SeqCst);
    log::set_max_level(verbosity.level_filter());
}

/// Current effective logging verbosity.
pub fn verbosity() -> LogVerbosity {
    LogVerbosity::from_u8(VERBOSITY.load(Ordering::SeqCst))
}

/// Decide whether a record should be written, honoring the live verbosity toggle.
fn should_log(metadata: &Metadata<'_>) -> bool {
    let level = metadata.level();
    // Info/Warn/Error are always recorded.
    if level <= Level::Info {
        return true;
    }
    matches!(
        (verbosity(), level),
        (LogVerbosity::Trace, Level::Trace | Level::Debug) | (LogVerbosity::Debug, Level::Debug)
    )
}

/// Build the configured `tauri-plugin-log` plugin.
pub fn build_plugin<R: tauri::Runtime>(verbosity: LogVerbosity) -> tauri::plugin::TauriPlugin<R> {
    // Seed the runtime verbosity before any records can flow through `should_log`.
    set_verbosity(verbosity);

    let mut builder = Builder::default()
        .targets([Target::new(TargetKind::LogDir {
            file_name: Some(LOG_FILE_STEM.to_string()),
        })])
        .max_file_size(MAX_FILE_SIZE)
        .rotation_strategy(RotationStrategy::KeepSome(2))
        // Allow every level through the static filter; `should_log` performs the
        // live, fine-grained gating based on the debug toggle.
        .level(LevelFilter::Trace)
        // Cap known-noisy dependencies that drown out useful signal in trace mode.
        // Tungstenite trace also logs raw websocket payloads, which may include auth data.
        .level_for("tao", LevelFilter::Info)
        .level_for("tokio_tungstenite", LevelFilter::Info)
        .level_for("tungstenite", LevelFilter::Info)
        .level_for("ureq", LevelFilter::Info)
        .level_for("ureq_proto", LevelFilter::Info)
        .filter(should_log);

    if cfg!(debug_assertions) {
        builder = builder.target(Target::new(TargetKind::Stdout));
    }

    builder.build()
}

/// Re-apply the persisted verbosity after the plugin has been installed.
pub fn apply_after_install(verbosity: LogVerbosity) {
    set_verbosity(verbosity);
}
