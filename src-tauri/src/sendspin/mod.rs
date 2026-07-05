//! Native Sendspin client for Music Assistant Companion
//!
//! This module wraps the sendspin-rs library and adds:
//! - Audio device enumeration and selection
//! - Integration with Tauri (settings, `now_playing` callbacks)
//! - Playback control commands
//! - Controller role for sending commands
//! - Metadata role for receiving track info

pub mod devices;
mod now_playing_state;
pub mod volume_control;

use crate::now_playing::{self, NowPlaying};
use now_playing_state::NowPlayingState;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use volume_control::VolumeController;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};

use sendspin::audio::decode::{Decoder, PcmDecoder};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec, SyncedPlayer, SyncedPlayerConfig};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientState, ClientSyncState, Message, PlayerCommandType, PlayerState,
    PlayerStateCommand, PlayerV1Support, ServerCommand,
};
use sendspin::sync::ClockSync;
use sendspin::{Connection, ProtocolClientBuilder, WsSender};

/// Simple jitter: returns a pseudo-random value in `0..max_ms/4` using the
/// current timestamp as entropy. No external crate needed.
fn rand_jitter_ms(max_ms: u64) -> u64 {
    let range = max_ms / 4;
    if range == 0 {
        return 0;
    }
    let nanos = u64::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos(),
    );
    nanos % range
}

fn fallback_supported_formats() -> Vec<AudioFormatSpec> {
    vec![
        AudioFormatSpec {
            codec: "pcm".to_string(),
            channels: 2,
            sample_rate: 48000,
            bit_depth: 16,
        },
        AudioFormatSpec {
            codec: "pcm".to_string(),
            channels: 2,
            sample_rate: 44100,
            bit_depth: 16,
        },
    ]
}

fn format_specs_to_log_string(formats: &[AudioFormatSpec]) -> String {
    formats
        .iter()
        .map(|f| format!("{}ch/{}Hz/{}bit", f.channels, f.sample_rate, f.bit_depth))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Commands sent to the playback thread
enum PlayerCommand {
    /// Create a new `SyncedPlayer` with the given format
    CreatePlayer(AudioFormat),
    /// Enqueue an audio buffer for playback
    Enqueue(AudioBuffer),
    /// Clear the playback buffer
    Clear,
    /// Shutdown the playback thread
    Shutdown,
    /// Set software volume level (0-100)
    /// Used by the client loop to send volume commands to the playback thread via `player_tx`
    SetVolume(u8),
    /// Set software mute state
    /// Used by the client loop to send mute commands to the playback thread via `player_tx`
    SetMute(bool),
    /// Set the static sync delay in milliseconds.
    SetStaticDelay(u16),
}

/// Commands sent to the async client loop for live runtime reconfiguration.
#[derive(Debug, Clone, Copy)]
enum ClientCommand {
    /// Set the static sync delay in milliseconds.
    SetStaticDelay(u16),
    /// Set player volume from an app-owned control surface.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    SetVolume(u8),
}

/// Auth message for MA proxy
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    msg_type: String,
    token: String,
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct AuthResponse {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    status: Option<String>,
    success: Option<bool>,
    ok: Option<bool>,
    authenticated: Option<bool>,
    error: Option<String>,
    message: Option<String>,
}

fn validate_auth_response(text: &str) -> Result<(), String> {
    let response: AuthResponse = serde_json::from_str(text)
        .map_err(|e| format!("Auth response was not valid JSON: {}", e))?;

    if response.success == Some(true)
        || response.ok == Some(true)
        || response.authenticated == Some(true)
        || response.status.as_deref() == Some("ok")
        || response.status.as_deref() == Some("success")
        || response.msg_type.as_deref() == Some("auth_ok")
        || response.msg_type.as_deref() == Some("auth/success")
    {
        return Ok(());
    }

    if let Some(error) = response.error {
        return Err(format!("Auth rejected: {}", error));
    }
    if response.success == Some(false)
        || response.ok == Some(false)
        || response.authenticated == Some(false)
        || response.status.as_deref() == Some("error")
        || response.status.as_deref() == Some("failed")
        || response.msg_type.as_deref() == Some("auth_error")
        || response.msg_type.as_deref() == Some("auth/error")
    {
        return Err(format!(
            "Auth rejected{}",
            response
                .message
                .as_deref()
                .map(|message| format!(": {}", message))
                .unwrap_or_default()
        ));
    }

    Err(format!("Unexpected auth response: {}", text))
}

/// Global Sendspin client instance
static SENDSPIN_CLIENT: RwLock<Option<SendspinClientHandle>> = RwLock::new(None);

/// Whether the Sendspin client is enabled
pub static SENDSPIN_ENABLED: AtomicBool = AtomicBool::new(false);

/// Shutdown signal
static SHUTDOWN_TX: RwLock<Option<mpsc::Sender<()>>> = RwLock::new(None);

/// Command channel for sending controller commands
static COMMAND_TX: RwLock<Option<mpsc::Sender<String>>> = RwLock::new(None);

/// Runtime command channel for live Sendspin client reconfiguration.
static CLIENT_COMMAND_TX: RwLock<Option<mpsc::Sender<ClientCommand>>> = RwLock::new(None);

/// Task handle for the running client
static CLIENT_TASK: RwLock<Option<tokio::task::JoinHandle<()>>> = RwLock::new(None);

/// Sentinel for "the client loop has not reported a volume yet".
const VOLUME_UNKNOWN: u8 = u8::MAX;

/// Last volume applied by the client loop (0-100), published lock-free so
/// UI surfaces (e.g. Linux MPRIS) can read it without a round trip into the
/// async loop.
static CURRENT_VOLUME: AtomicU8 = AtomicU8::new(VOLUME_UNKNOWN);

/// Observer callback for published volume changes.
type VolumeListener = Box<dyn Fn(u8) + Send + Sync>;

/// Optional observer notified when the published volume changes, regardless
/// of who changed it (server command, app surface, or hardware keys).
static VOLUME_LISTENER: RwLock<Option<VolumeListener>> = RwLock::new(None);

/// Hardware volume controller (if available)
static VOLUME_CONTROLLER: RwLock<Option<VolumeController>> = RwLock::new(None);

/// The resolved volume control behavior for this session.
/// Determined once at connection time and used for the session duration.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ResolvedVolumeMode {
    /// Use hardware volume controller
    Hardware,
    /// Use software gain processing in the playback thread
    Software,
    /// No volume control
    None,
}

/// Resolve the user's volume control mode preference against hardware availability.
///
/// | Setting  | Hardware available? | Result   |
/// |----------|-------------------- |----------|
/// | Auto     | Yes                 | Hardware |
/// | Auto     | No                  | Software |
/// | Hardware | Yes                 | Hardware |
/// | Hardware | No                  | None     |
/// | Software | N/A                 | Software |
/// | Disabled | N/A                 | None     |
fn resolve_volume_mode(
    mode: &crate::settings::VolumeControlMode,
    hardware_available: bool,
) -> ResolvedVolumeMode {
    use crate::settings::VolumeControlMode;
    match mode {
        VolumeControlMode::Auto => {
            if hardware_available {
                ResolvedVolumeMode::Hardware
            } else {
                ResolvedVolumeMode::Software
            }
        }
        VolumeControlMode::Hardware => {
            if hardware_available {
                ResolvedVolumeMode::Hardware
            } else {
                ResolvedVolumeMode::None
            }
        }
        VolumeControlMode::Software => ResolvedVolumeMode::Software,
        VolumeControlMode::Disabled => ResolvedVolumeMode::None,
    }
}

/// Client configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendspinConfig {
    pub player_id: String,
    pub player_name: String,
    pub server_url: String,
    pub audio_device_id: Option<String>,
    pub sync_delay_ms: i32,
    /// Auth token for MA server proxy authentication (required)
    pub auth_token: String,
    /// App version advertised to the server (sourced from the Tauri config, not `Cargo.toml`)
    pub app_version: String,
}

/// Connection status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    Error(String),
}

/// Sendspin client handle
pub struct SendspinClientHandle {
    #[allow(dead_code)]
    pub config: SendspinConfig,
    pub status: ConnectionStatus,
    pub player_id: String,
}

impl SendspinClientHandle {
    pub fn new(config: SendspinConfig) -> Self {
        let player_id = config.player_id.clone();
        Self {
            config,
            status: ConnectionStatus::Disconnected,
            player_id,
        }
    }
}

/// Get the current connection status
pub fn get_status() -> ConnectionStatus {
    SENDSPIN_CLIENT
        .read()
        .as_ref()
        .map_or(ConnectionStatus::Disconnected, |c| c.status.clone())
}

/// Get the current player ID (if connected)
pub fn get_player_id() -> Option<String> {
    SENDSPIN_CLIENT.read().as_ref().map(|c| c.player_id.clone())
}

/// Check if Sendspin is enabled
pub fn is_enabled() -> bool {
    SENDSPIN_ENABLED.load(Ordering::SeqCst)
}

/// Set Sendspin enabled state
pub fn set_enabled(enabled: bool) {
    SENDSPIN_ENABLED.store(enabled, Ordering::SeqCst);
}

fn update_status(status: ConnectionStatus) {
    let mut client = SENDSPIN_CLIENT.write();
    if let Some(ref mut c) = *client {
        c.status = status;
    }
}

const PLAYER_BUFFER_CAPACITY: u32 = 16 * 1024 * 1024;
// Startup/system lead time: enough for codec setup and audio-device/DAC readiness,
// without adding the larger ongoing network-jitter buffer to initial playback.
const REQUIRED_LEAD_TIME_MS: u32 = 50;
// Ongoing playback buffer: intentionally conservative until we have adaptive tuning.
const MIN_BUFFER_MS: u32 = 500;

fn clamp_static_delay_ms(sync_delay_ms: i32) -> u16 {
    sync_delay_ms.clamp(0, 5_000) as u16
}

fn supported_volume_commands(resolved_mode: ResolvedVolumeMode) -> Vec<String> {
    match resolved_mode {
        ResolvedVolumeMode::Hardware | ResolvedVolumeMode::Software => {
            vec!["volume".to_string(), "mute".to_string()]
        }
        ResolvedVolumeMode::None => vec![],
    }
}

fn build_player_support(
    supported_formats: Vec<AudioFormatSpec>,
    supported_commands: Vec<String>,
) -> PlayerV1Support {
    PlayerV1Support {
        supported_formats,
        // Maximum audio buffer capacity advertised to the server, in bytes.
        // 16 MiB gives generous desktop PCM headroom without requiring prefill.
        buffer_capacity: PLAYER_BUFFER_CAPACITY,
        // Only advertise volume support if hardware/software control is available.
        supported_commands,
    }
}

fn build_initial_player_state(
    resolved_mode: ResolvedVolumeMode,
    volume: u8,
    muted: bool,
    sync_delay_ms: i32,
) -> PlayerState {
    let report_volume = (resolved_mode != ResolvedVolumeMode::None).then_some(volume);
    let report_muted = (resolved_mode != ResolvedVolumeMode::None).then_some(muted);

    PlayerState {
        volume: report_volume,
        muted: report_muted,
        static_delay_ms: Some(clamp_static_delay_ms(sync_delay_ms)),
        required_lead_time_ms: Some(REQUIRED_LEAD_TIME_MS),
        min_buffer_ms: Some(MIN_BUFFER_MS),
        supported_commands: Some(vec![PlayerStateCommand::SetStaticDelay]),
    }
}

fn build_protocol_client_builder(
    config: &SendspinConfig,
    player_support: PlayerV1Support,
    initial_player_state: PlayerState,
) -> ProtocolClientBuilder {
    ProtocolClientBuilder::builder()
        .client_id(config.player_id.clone())
        .name(config.player_name.clone())
        .product_name(Some(config.player_name.clone()))
        .manufacturer(Some("Music Assistant".to_string()))
        .software_version(Some(config.app_version.clone()))
        .player_v1_support(player_support)
        .controller()
        .metadata()
        .initial_player_state(initial_player_state)
        .build()
}

/// Start the Sendspin client
///
/// This connects to the Sendspin server and starts audio playback.
/// The client will run in the background and update `now_playing` state.
pub async fn start(config: SendspinConfig) -> Result<String, String> {
    // Stop any existing client
    stop().await;

    // Create client handle
    let mut handle = SendspinClientHandle::new(config.clone());
    handle.status = ConnectionStatus::Connecting;

    let player_id = handle.player_id.clone();

    // Store the handle
    {
        let mut client = SENDSPIN_CLIENT.write();
        *client = Some(handle);
    }

    set_enabled(true);

    // Spawn the client task with reconnection loop
    let config_clone = config.clone();
    let player_id_clone = player_id.clone();
    let task_handle = tokio::spawn(async move {
        const MAX_BACKOFF: Duration = Duration::from_secs(30);
        let mut backoff = Duration::from_secs(1);

        loop {
            // Create fresh channels for this connection attempt
            let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
            let (command_tx, command_rx) = mpsc::channel::<String>(32);
            let (client_command_tx, client_command_rx) = mpsc::channel::<ClientCommand>(32);

            // Update globals so stop()/send_command()/runtime reconfiguration reach the current connection
            {
                *SHUTDOWN_TX.write() = Some(shutdown_tx);
            }
            {
                *COMMAND_TX.write() = Some(command_tx);
            }
            {
                *CLIENT_COMMAND_TX.write() = Some(client_command_tx);
            }

            let connected_at = Instant::now();

            let mut attempt_config = config_clone.clone();
            attempt_config.sync_delay_ms = crate::settings::get_settings().sync_delay_ms;

            let result = run_client(
                attempt_config,
                player_id_clone.clone(),
                shutdown_rx,
                command_rx,
                client_command_rx,
            )
            .await;

            // If stop() was called, exit cleanly
            if !is_enabled() {
                break;
            }

            // Reset backoff if the connection was alive for >10 seconds
            // (meaning it was a real session, not an immediate failure)
            if connected_at.elapsed() > Duration::from_secs(10) {
                backoff = Duration::from_secs(1);
            }

            match result {
                Ok(()) => {
                    log::warn!("[Sendspin] Disconnected, reconnecting in {:?}...", backoff);
                }
                Err(e) => {
                    log::error!(
                        "[Sendspin] Client error: {}, reconnecting in {:?}...",
                        e,
                        backoff
                    );
                }
            }

            update_status(ConnectionStatus::Reconnecting);

            // Sleep in small increments so stop() can interrupt quickly
            let deadline = Instant::now() + backoff;
            while Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if !is_enabled() {
                    break;
                }
            }
            if !is_enabled() {
                break;
            }

            // Exponential backoff with jitter. The cap is intentionally soft —
            // jitter is added after clamping, so actual delay can exceed MAX_BACKOFF
            // by up to ~25%. This is fine; the jitter exists to spread out reconnects.
            let jitter = Duration::from_millis(rand_jitter_ms(backoff.as_millis() as u64));
            backoff = (backoff * 2).min(MAX_BACKOFF) + jitter;

            update_status(ConnectionStatus::Connecting);
        }
    });

    // Store the task handle so we can await it on stop
    {
        let mut handle = CLIENT_TASK.write();
        *handle = Some(task_handle);
    }

    Ok(player_id)
}

/// Main client loop
async fn run_client(
    config: SendspinConfig,
    player_id: String,
    shutdown_rx: mpsc::Receiver<()>,
    command_rx: mpsc::Receiver<String>,
    client_command_rx: mpsc::Receiver<ClientCommand>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize hardware volume controller
    let volume_controller = VolumeController::new();
    let has_volume_control = volume_controller
        .as_ref()
        .is_some_and(|vc| vc.is_available());

    // Resolve volume control mode from settings
    let settings = crate::settings::get_settings();
    let resolved_mode = resolve_volume_mode(&settings.volume_control_mode, has_volume_control);

    log::info!(
        "[Sendspin] Volume control: mode={:?}, hardware_available={}, resolved={:?}",
        settings.volume_control_mode,
        has_volume_control,
        resolved_mode
    );

    // Create channel for volume change notifications
    #[allow(unused_mut)] // mut is required for select! macro
    let (volume_change_tx, mut volume_change_rx) = mpsc::channel::<(u8, bool)>(32);

    // Store the volume controller globally and set up change callback only if using hardware mode
    if resolved_mode == ResolvedVolumeMode::Hardware {
        if let Some(vc) = volume_controller {
            // Set up volume change callback
            // Convert tokio mpsc sender to std mpsc sender for compatibility
            let (std_tx, std_rx) = std::sync::mpsc::channel::<(u8, bool)>();

            // Spawn a blocking task to forward std mpsc messages to tokio mpsc
            // std::sync::mpsc::recv() is blocking and must not block the tokio runtime
            let volume_change_tx_clone = volume_change_tx.clone();
            tokio::task::spawn_blocking(move || {
                while let Ok((volume, muted)) = std_rx.recv() {
                    // Use blocking_send since we're in a blocking context
                    let _ = volume_change_tx_clone.blocking_send((volume, muted));
                }
            });

            // Register the callback
            if let Err(e) = vc.set_change_callback(std_tx) {
                log::warn!(
                    "[Sendspin] Failed to register volume change callback: {}",
                    e
                );
            }

            let mut vol_ctrl = VOLUME_CONTROLLER.write();
            *vol_ctrl = Some(vc);
        }
    }

    // Build supported commands list based on resolved volume mode.
    let supported_commands = supported_volume_commands(resolved_mode);

    // Resolve output device once per connection and derive supported formats for this device.
    // This avoids negotiating formats that the selected Windows output cannot open.
    let output_device = devices::resolve_output_device(config.audio_device_id.as_deref());
    let mut supported_formats: Vec<AudioFormatSpec> =
        devices::derive_supported_pcm_formats(output_device.as_ref())
            .into_iter()
            .map(|f| AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: f.channels as _,
                sample_rate: f.sample_rate,
                bit_depth: f.bit_depth as _,
            })
            .collect();

    if supported_formats.is_empty() {
        supported_formats = fallback_supported_formats();
        log::warn!(
            "[Sendspin] No reliable device format capabilities found; using conservative fallback formats: {}",
            format_specs_to_log_string(&supported_formats)
        );
    } else {
        log::debug!(
            "[Sendspin] Advertising device-aware formats: {}",
            format_specs_to_log_string(&supported_formats)
        );
    }

    let (initial_volume, initial_muted) = initial_volume_state(resolved_mode);
    let player_support = build_player_support(supported_formats, supported_commands);
    let initial_player_state = build_initial_player_state(
        resolved_mode,
        initial_volume,
        initial_muted,
        config.sync_delay_ms,
    );
    let protocol_builder =
        build_protocol_client_builder(&config, player_support, initial_player_state);

    // Connect to WebSocket and authenticate with MA proxy
    log::info!(
        "[Sendspin] Connecting to {} as player {}",
        config.server_url,
        player_id
    );
    let (ws_stream, _response) = connect_async(&config.server_url)
        .await
        .map_err(|e| format!("WebSocket connection failed: {}", e))?;
    log::debug!("[Sendspin] WebSocket connected; authenticating");

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Send auth message
    let auth_msg = AuthMessage {
        msg_type: "auth".to_string(),
        token: config.auth_token.clone(),
        client_id: player_id.clone(),
    };
    let auth_json =
        serde_json::to_string(&auth_msg).map_err(|e| format!("Failed to serialize auth: {}", e))?;

    ws_tx
        .send(WsMessage::Text(auth_json.into()))
        .await
        .map_err(|e| format!("Failed to send auth: {}", e))?;

    // Wait for the MA proxy auth response before handing the socket to sendspin-rs.
    // Ping/pong frames are WebSocket housekeeping; the auth ack itself must be
    // an explicit successful JSON text message so auth failures do not surface
    // later as opaque Sendspin protocol handshakes.
    let auth_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = auth_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("Auth timeout".into());
        }

        let auth_frame = tokio::time::timeout(remaining, ws_rx.next()).await;
        match auth_frame {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                validate_auth_response(text.as_ref())?;
                log::debug!("[Sendspin] Auth accepted; starting Sendspin protocol handshake");
                break;
            }
            Ok(Some(Ok(WsMessage::Ping(_) | WsMessage::Pong(_)))) => {}
            Ok(Some(Ok(WsMessage::Close(frame)))) => {
                return Err(format!("Connection closed during auth: {:?}", frame).into());
            }
            Ok(Some(Ok(other))) => {
                return Err(format!("Unexpected auth response frame: {:?}", other).into());
            }
            Ok(Some(Err(e))) => {
                return Err(format!("Auth response error: {}", e).into());
            }
            Ok(None) => {
                return Err("Connection closed during auth".into());
            }
            Err(_) => {
                return Err("Auth timeout".into());
            }
        }
    }

    let ws_stream = ws_tx
        .reunite(ws_rx)
        .map_err(|_| "Failed to reunite authenticated WebSocket halves")?;

    let protocol_client = protocol_builder
        .accept(ws_stream)
        .await
        .map_err(|e| format!("Sendspin protocol handshake failed: {}", e))?;
    let connection = protocol_client.split();

    update_status(ConnectionStatus::Connected);
    log::info!("[Sendspin] Connected to server (player {})", player_id);

    // The cpal::Device resolved above is intentionally not passed onward.
    // It exists only to drive the capability advertisement (which needs
    // device-specific format info up front). The playback thread re-resolves
    // from `config.audio_device_id` on each player creation so it picks up
    // fresh handles when Bluetooth devices sleep/reconnect (CoreAudio
    // assigns a new AudioObjectID, invalidating any cached `cpal::Device`).
    //
    // Run the authenticated WebSocket protocol loop
    run_authenticated_client(
        connection,
        config,
        player_id,
        shutdown_rx,
        command_rx,
        client_command_rx,
        volume_change_rx,
        resolved_mode,
        initial_volume,
        initial_muted,
    )
    .await
}

fn initial_volume_state(resolved_mode: ResolvedVolumeMode) -> (u8, bool) {
    let saved_settings = crate::settings::get_settings();
    match resolved_mode {
        ResolvedVolumeMode::Hardware => {
            let vol_ctrl = VOLUME_CONTROLLER.read();
            if let Some(ref vc) = *vol_ctrl {
                let vol = vc.get_volume().unwrap_or(100);
                // Hardware volume comes from OS; mute state is persisted since it is lost on reconnect.
                let muted = vc.get_mute().unwrap_or(saved_settings.muted);
                log::debug!(
                    "[Sendspin] Initial hardware volume: {}%, muted: {}",
                    vol,
                    muted
                );
                (vol, muted)
            } else {
                (100, saved_settings.muted)
            }
        }
        ResolvedVolumeMode::Software => {
            log::debug!(
                "[Sendspin] Initial software volume: {}%, muted: {}",
                saved_settings.software_volume,
                saved_settings.muted
            );
            (saved_settings.software_volume, saved_settings.muted)
        }
        ResolvedVolumeMode::None => (100, false),
    }
}

/// Persist volume/mute state to settings so it survives reconnects.
/// Called on every volume/mute change. We get a new connection on every
/// track change, so without this, volume resets between songs.
fn save_volume_state(resolved_mode: ResolvedVolumeMode, volume: u8, muted: bool) {
    let mut settings = crate::settings::get_settings();
    let mut changed = false;

    // Software volume is persisted separately; hardware reads from the OS.
    if resolved_mode == ResolvedVolumeMode::Software && settings.software_volume != volume {
        settings.software_volume = volume;
        changed = true;
    }

    // Mute state is shared across modes since it's always lost on reconnect.
    if resolved_mode != ResolvedVolumeMode::None && settings.muted != muted {
        settings.muted = muted;
        changed = true;
    }

    if changed {
        let _ = crate::settings::save_settings(&settings);
    }
}

fn save_static_delay_state(static_delay_ms: u16) {
    let mut settings = crate::settings::get_settings();
    let value = i32::from(static_delay_ms);

    if settings.sync_delay_ms != value {
        settings.sync_delay_ms = value;
        let _ = crate::settings::save_settings(&settings);
    }
}

/// Build a `ClientState` message echoing the current volume/mute state back to the server.
fn build_volume_state_msg(volume: u8, muted: bool) -> Message {
    Message::ClientState(ClientState {
        state: Some(ClientSyncState::Synchronized),
        player: Some(PlayerState {
            volume: Some(volume),
            muted: Some(muted),
            ..PlayerState::default()
        }),
    })
}

/// Build a `ClientState` message echoing the current static sync delay back to the server.
fn build_static_delay_state_msg(static_delay_ms: u16) -> Message {
    Message::ClientState(ClientState {
        state: Some(ClientSyncState::Synchronized),
        player: Some(PlayerState {
            static_delay_ms: Some(static_delay_ms),
            ..PlayerState::default()
        }),
    })
}

fn send_player_command(
    player_tx: &std_mpsc::Sender<PlayerCommand>,
    command: PlayerCommand,
    description: &str,
) -> bool {
    if let Err(e) = player_tx.send(command) {
        log::warn!(
            "[Sendspin] Failed to send playback command {}: {}",
            description,
            e
        );
        false
    } else {
        true
    }
}

fn apply_volume(
    resolved_mode: ResolvedVolumeMode,
    player_tx: &std_mpsc::Sender<PlayerCommand>,
    volume: u8,
    description: &str,
) -> bool {
    match resolved_mode {
        ResolvedVolumeMode::Hardware => {
            let volume_result = {
                let vol_ctrl = VOLUME_CONTROLLER.read();
                if let Some(ref vc) = *vol_ctrl {
                    vc.set_volume(volume)
                } else {
                    Err("Volume controller not available".to_string())
                }
            };
            if let Err(e) = &volume_result {
                log::warn!("[Sendspin] Failed to set hardware volume ({description}): {e}");
            }
            volume_result.is_ok()
        }
        ResolvedVolumeMode::Software => send_player_command(
            player_tx,
            PlayerCommand::SetVolume(volume),
            "set software volume",
        ),
        ResolvedVolumeMode::None => {
            log::debug!(
                "[Sendspin] Ignoring volume command ({description}): volume control is disabled"
            );
            false
        }
    }
}

/// Record the client loop's current volume and notify the listener when it
/// actually changed.
fn publish_volume(volume: u8) {
    // Enforce the 0..=100 invariant here so no caller can collide with the
    // VOLUME_UNKNOWN sentinel.
    let volume = volume.min(100);
    let previous = CURRENT_VOLUME.swap(volume, Ordering::Relaxed);
    if previous != volume {
        if let Some(ref listener) = *VOLUME_LISTENER.read() {
            listener(volume);
        }
    }
}

/// Re-send the current published volume to the listener, e.g. so an external
/// control surface that optimistically moved its slider snaps back after a
/// rejected set.
fn renotify_volume() {
    let volume = CURRENT_VOLUME.load(Ordering::Relaxed);
    if volume != VOLUME_UNKNOWN {
        if let Some(ref listener) = *VOLUME_LISTENER.read() {
            listener(volume);
        }
    }
}

/// Register the observer for player volume changes (replaces any previous
/// one). Used by app-owned control surfaces such as Linux MPRIS to stay in
/// sync without polling.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn set_volume_listener(listener: impl Fn(u8) + Send + Sync + 'static) {
    *VOLUME_LISTENER.write() = Some(Box::new(listener));
}

/// Publish an applied volume/mute change locally (atomic + listener +
/// persisted settings) and report the new state to the server.
async fn broadcast_volume_state(
    sender: &WsSender,
    resolved_mode: ResolvedVolumeMode,
    volume: u8,
    muted: bool,
    what: &str,
) {
    publish_volume(volume);
    save_volume_state(resolved_mode, volume, muted);
    let msg = build_volume_state_msg(volume, muted);
    if let Err(e) = sender.send_message(msg).await {
        log::warn!("[Sendspin] Failed to send {what} state: {e}");
    }
}

/// Run the Sendspin client on an already-authenticated WebSocket connection
/// This is used when connecting through the MA proxy which requires auth first
#[allow(clippy::too_many_arguments)]
async fn run_authenticated_client(
    connection: Connection,
    config: SendspinConfig,
    player_id: String,
    mut shutdown_rx: mpsc::Receiver<()>,
    mut command_rx: mpsc::Receiver<String>,
    mut client_command_rx: mpsc::Receiver<ClientCommand>,
    mut volume_change_rx: mpsc::Receiver<(u8, bool)>,
    resolved_mode: ResolvedVolumeMode,
    initial_volume: u8,
    initial_muted: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Connection {
        mut messages,
        mut audio,
        clock_sync,
        sender,
        controller,
        guard: _guard,
        ..
    } = connection;

    // Create channel for sending commands to the playback thread
    let (player_tx, player_rx) = std_mpsc::channel::<PlayerCommand>();

    // Spawn playback thread that owns the SyncedPlayer.
    // Pass the configured device id (not a resolved cpal::Device); the
    // thread re-resolves on each player creation so a stale handle from
    // a Bluetooth sleep/reconnect cycle can't permanently break audio.
    let clock_sync_for_thread = Arc::clone(&clock_sync);
    let use_software_volume = resolved_mode == ResolvedVolumeMode::Software;
    let audio_device_id_for_thread = config.audio_device_id.clone();
    let initial_static_delay_ms = clamp_static_delay_ms(config.sync_delay_ms);
    let _playback_handle = thread::spawn(move || {
        run_playback_thread(
            player_rx,
            clock_sync_for_thread,
            audio_device_id_for_thread,
            use_software_volume,
            initial_static_delay_ms,
        );
    });

    // Message handling variables
    let mut decoder: Option<PcmDecoder> = None;
    let mut audio_format: Option<AudioFormat> = None;

    // Folds protocol deltas into a coherent now-playing snapshot.
    let mut np_state = NowPlayingState::new(player_id.clone(), config.player_name.clone());

    // Volume state — initialized from the same read used for the initial ClientState
    let mut current_volume: u8 = initial_volume;
    let mut current_muted: bool = initial_muted;
    publish_volume(current_volume);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            Some(cmd) = command_rx.recv() => {
                let Some(controller) = controller.as_ref() else {
                    log::warn!("[Sendspin] Cannot send controller command; server did not grant controller role");
                    continue;
                };
                log::debug!("[Sendspin] Sending controller command to server: {}", cmd);
                let result = match cmd.as_str() {
                    "play" => controller.play().await,
                    "pause" => controller.pause().await,
                    "stop" => controller.stop().await,
                    "next" => controller.next().await,
                    "previous" => controller.previous().await,
                    _ => {
                        log::warn!("[Sendspin] Unknown controller command from app: {}", cmd);
                        continue;
                    }
                };
                if let Err(e) = result {
                    log::warn!("[Sendspin] Failed to send controller command {}: {}", cmd, e);
                }
            }
            Some(cmd) = client_command_rx.recv() => {
                match cmd {
                    ClientCommand::SetStaticDelay(delay_ms) => {
                        log::debug!("[Sendspin] Applying static delay: {}ms", delay_ms);
                        if send_player_command(&player_tx, PlayerCommand::SetStaticDelay(delay_ms), "set static delay") {
                            let msg = build_static_delay_state_msg(delay_ms);
                            if let Err(e) = sender.send_message(msg).await {
                                log::warn!("[Sendspin] Failed to send static delay state: {}", e);
                            }
                        }
                    }
                    ClientCommand::SetVolume(volume) => {
                        let volume = volume.min(100);
                        log::debug!("[Sendspin] Applying app volume command: {}%", volume);
                        if apply_volume(resolved_mode, &player_tx, volume, "app") {
                            current_volume = volume;
                            broadcast_volume_state(&sender, resolved_mode, current_volume, current_muted, "app volume").await;
                        } else {
                            // The set was rejected; snap the requesting
                            // surface back to the actual value.
                            renotify_volume();
                        }
                    }
                }
            }
            Some((volume, muted)) = volume_change_rx.recv() => {
                // This channel only carries OS-level volume change notifications
                // from the hardware callback. Guard on mode so a future refactor
                // can't accidentally echo state without routing through the
                // correct volume path.
                if resolved_mode == ResolvedVolumeMode::Hardware {
                    log::debug!("[Sendspin] OS volume changed: {}%, muted: {}", volume, muted);
                    current_volume = volume;
                    current_muted = muted;
                    broadcast_volume_state(&sender, resolved_mode, current_volume, current_muted, "hardware volume").await;
                }
            }
            Some(msg) = messages.recv() => {
                match msg {
                    Message::StreamStart(stream_start) => {
                        let Some(player_config) = stream_start.player else {
                            continue;
                        };

                        log::info!(
                            "[Sendspin] Server StreamStart: codec={}, channels={}, sample_rate={}, bit_depth={}",
                            player_config.codec,
                            player_config.channels,
                            player_config.sample_rate,
                            player_config.bit_depth
                        );

                        if player_config.codec != "pcm" {
                            log::error!("[Sendspin] Unsupported codec: {}", player_config.codec);
                            continue;
                        }

                        let fmt = AudioFormat {
                            codec: Codec::Pcm,
                            sample_rate: player_config.sample_rate,
                            channels: player_config.channels,
                            bit_depth: player_config.bit_depth,
                            codec_header: None,
                        };

                        if !matches!(fmt.bit_depth, 16 | 24) {
                            log::error!(
                                "[Sendspin] Unsupported PCM bit depth: {}",
                                fmt.bit_depth
                            );
                            continue;
                        }

                        decoder = Some(PcmDecoder::new(fmt.bit_depth));
                        audio_format = Some(fmt.clone());
                        send_player_command(&player_tx, PlayerCommand::CreatePlayer(fmt), "create player");
                    }
                    Message::ServerState(state) => {
                        if let Some(md) = state.metadata {
                            log::trace!("[Sendspin] Server metadata update received");
                            np_state.apply_metadata(&md);
                            now_playing::update_now_playing(np_state.snapshot());
                        }
                    }
                    Message::StreamEnd(_) | Message::StreamClear(_) => {
                        log::debug!("[Sendspin] Server stream end/clear");
                        send_player_command(&player_tx, PlayerCommand::Clear, "clear player");
                    }
                    Message::ServerCommand(ServerCommand { player: Some(player_cmd) }) => {
                        if player_cmd.command == PlayerCommandType::SetStaticDelay {
                            if let Some(static_delay_ms) = player_cmd.static_delay_ms {
                                let delay_ms = clamp_static_delay_ms(i32::from(static_delay_ms));
                                log::debug!("[Sendspin] Server static delay command: {}ms", delay_ms);

                                if send_player_command(&player_tx, PlayerCommand::SetStaticDelay(delay_ms), "set static delay") {
                                    save_static_delay_state(delay_ms);
                                    let msg = build_static_delay_state_msg(delay_ms);
                                    if let Err(e) = sender.send_message(msg).await {
                                        log::warn!("[Sendspin] Failed to send static delay state: {}", e);
                                    }
                                }
                            }
                        }

                        if player_cmd.command == PlayerCommandType::Volume {
                            if let Some(volume) = player_cmd.volume {
                                let vol = volume.min(100);
                                log::debug!("[Sendspin] Server volume command: {}%", vol);

                                let success = apply_volume(resolved_mode, &player_tx, vol, "server");

                                if success {
                                    current_volume = vol;
                                    broadcast_volume_state(&sender, resolved_mode, current_volume, current_muted, "server volume").await;
                                }
                            }
                        }

                        if player_cmd.command == PlayerCommandType::Mute {
                            if let Some(mute) = player_cmd.mute {
                                log::debug!("[Sendspin] Server mute command: {}", mute);
                                let success = match resolved_mode {
                                    ResolvedVolumeMode::Hardware => {
                                        let mute_result = {
                                            let vol_ctrl = VOLUME_CONTROLLER.read();
                                            if let Some(ref vc) = *vol_ctrl {
                                                vc.set_mute(mute)
                                            } else {
                                                Err("Volume controller not available".to_string())
                                            }
                                        };
                                        if let Err(e) = &mute_result {
                                            log::warn!("[Sendspin] Failed to set hardware mute: {}", e);
                                        }
                                        mute_result.is_ok()
                                    }
                                    ResolvedVolumeMode::Software => send_player_command(
                                        &player_tx,
                                        PlayerCommand::SetMute(mute),
                                        "set software mute",
                                    ),
                                    ResolvedVolumeMode::None => {
                                        log::debug!("[Sendspin] Ignoring mute command: volume control is disabled");
                                        false
                                    }
                                };

                                if success {
                                    current_muted = mute;
                                    broadcast_volume_state(&sender, resolved_mode, current_volume, current_muted, "mute").await;
                                }
                            }
                        }
                    }
                    Message::GroupUpdate(gu) => {
                        np_state.apply_group_update(&gu);
                        now_playing::update_now_playing(np_state.snapshot());
                    }
                    _ => {}
                }
            }
            Some(chunk) = audio.recv() => {
                let Some(ref fmt) = audio_format else {
                    continue;
                };

                let bytes_per_sample = match fmt.bit_depth {
                    16 => 2,
                    24 => 3,
                    _ => continue,
                } as usize;
                let frame_size = bytes_per_sample * fmt.channels as usize;

                if chunk.data.len() % frame_size != 0 {
                    continue;
                }

                if let Some(ref dec) = decoder {
                    if let Ok(samples) = dec.decode(&chunk.data) {
                        let buffer = AudioBuffer {
                            timestamp: chunk.timestamp,
                            samples,
                            format: fmt.clone(),
                        };
                        send_player_command(&player_tx, PlayerCommand::Enqueue(buffer), "enqueue audio");
                    }
                }
            }
            else => {
                break;
            }
        }
    }

    // Shutdown playback thread
    send_player_command(&player_tx, PlayerCommand::Shutdown, "shutdown player");

    update_status(ConnectionStatus::Disconnected);

    let np = NowPlaying {
        is_playing: false,
        track: None,
        artist: None,
        album: None,
        image_url: None,
        player_name: None,
        player_id: None,
        duration: None,
        elapsed: None,
        can_play: false,
        can_pause: false,
        can_next: false,
        can_previous: false,
    };
    now_playing::update_now_playing(np);

    Ok(())
}

/// Playback thread - owns the `SyncedPlayer` and processes commands.
///
/// The cpal output device is re-resolved fresh on every `CreatePlayer`
/// command rather than being captured once at thread start. Two reasons:
///
/// 1. Bluetooth devices on macOS sleep when idle and reconnect with a new
///    underlying `CoreAudio` `AudioObjectID`. A `cpal::Device` cached from
///    before the sleep cycle returns `DeviceNotAvailable` from
///    `build_output_stream` indefinitely.
/// 2. If the user's previously selected device has disappeared since this
///    connection started (`AirPods` battery dies, headphones unplugged),
///    `resolve_output_device` will fall back to the system default — which
///    is the right behavior because `StreamStart` from the server is driven
///    by user action via the MA UI, i.e., the user just hit play and
///    expects audio to come out somewhere.
///
/// We deliberately do not auto-recover from mid-stream device loss (no
/// watching cpal's `error_callback`, no spontaneous re-create). The user's
/// next play action is the only trigger — preventing surprise audio
/// redirection when, e.g., they take their `AirPods` out mid-song.
fn run_playback_thread(
    rx: std_mpsc::Receiver<PlayerCommand>,
    clock_sync: Arc<Mutex<ClockSync>>,
    audio_device_id: Option<String>,
    use_software_volume: bool,
    initial_static_delay_ms: u16,
) {
    let mut synced_player: Option<SyncedPlayer> = None;
    let mut last_volume: u8 = 100;
    let mut last_muted: bool = false;
    let mut static_delay_ms = initial_static_delay_ms;

    loop {
        match rx.recv() {
            Ok(PlayerCommand::CreatePlayer(format)) => {
                // Clear existing player if any
                if let Some(ref player) = synced_player {
                    player.clear();
                }

                // Create new SyncedPlayer with current volume/mute state
                let (vol, mute) = if use_software_volume {
                    (last_volume, last_muted)
                } else {
                    (100, false)
                };

                // Re-resolve the output device fresh. See the function-level
                // doc comment for why we do this on every CreatePlayer rather
                // than caching a handle.
                let device = devices::resolve_output_device(audio_device_id.as_deref());

                let player_config = SyncedPlayerConfig {
                    device,
                    volume: vol,
                    muted: mute,
                    buffer_size: None,
                };

                match SyncedPlayer::new(format.clone(), Arc::clone(&clock_sync), player_config) {
                    Ok(player) => {
                        player.set_static_delay(static_delay_ms);
                        log::info!(
                            "[Sendspin] Audio player created: channels={}, sample_rate={}, bit_depth={}, static_delay_ms={}",
                            format.channels,
                            format.sample_rate,
                            format.bit_depth,
                            static_delay_ms
                        );
                        synced_player = Some(player);
                    }
                    Err(e) => {
                        log::error!(
                            "[Sendspin] Failed to create SyncedPlayer for channels={}, sample_rate={}, bit_depth={}: {}",
                            format.channels,
                            format.sample_rate,
                            format.bit_depth,
                            e
                        );
                    }
                }
            }
            Ok(PlayerCommand::Enqueue(buffer)) => {
                if let Some(ref player) = synced_player {
                    player.enqueue(buffer);
                }
            }
            Ok(PlayerCommand::Clear) => {
                if let Some(ref player) = synced_player {
                    player.clear();
                }
            }
            Ok(PlayerCommand::SetVolume(volume)) => {
                if use_software_volume {
                    last_volume = volume;
                    if let Some(ref player) = synced_player {
                        player.set_volume(volume);
                    }
                }
            }
            Ok(PlayerCommand::SetMute(muted)) => {
                if use_software_volume {
                    last_muted = muted;
                    if let Some(ref player) = synced_player {
                        player.set_mute(muted);
                    }
                }
            }
            Ok(PlayerCommand::SetStaticDelay(delay_ms)) => {
                static_delay_ms = delay_ms;
                if let Some(ref player) = synced_player {
                    player.set_static_delay(delay_ms);
                }
            }
            Ok(PlayerCommand::Shutdown) | Err(_) => {
                // Clean up and exit
                if let Some(ref player) = synced_player {
                    player.clear();
                }
                break;
            }
        }
    }
}

/// Stop the Sendspin client
pub async fn stop() {
    set_enabled(false);

    // Take the volume controller out of the global (under the write lock), then
    // drop it outside the lock. The Drop impl joins the polling thread, which
    // can block up to 2s. We drop explicitly here rather than letting it fall
    // out of scope at end-of-function so the polling thread is fully stopped
    // before we send the shutdown signal below.
    let old_vol_ctrl = {
        let mut vol_ctrl = VOLUME_CONTROLLER.write();
        vol_ctrl.take()
    };
    drop(old_vol_ctrl);

    // Send shutdown signal
    {
        let tx = SHUTDOWN_TX.read();
        if let Some(ref sender) = *tx {
            let _ = sender.try_send(());
        }
    }

    // Wait for the client task to finish (with timeout)
    let task_handle = {
        let mut handle = CLIENT_TASK.write();
        handle.take()
    };
    if let Some(mut handle) = task_handle {
        // Wait up to 2 seconds for graceful shutdown. If the task does not stop,
        // abort it so a stale reconnect loop cannot survive a later start().
        match tokio::time::timeout(Duration::from_secs(2), &mut handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.is_cancelled() => {}
            Ok(Err(e)) => {
                log::warn!(
                    "[Sendspin] Client task exited with error during stop: {}",
                    e
                );
            }
            Err(_) => {
                log::warn!("[Sendspin] Client task did not stop gracefully; aborting");
                handle.abort();
                let _ = handle.await;
            }
        }
    }

    // Clear shutdown sender
    {
        let mut tx = SHUTDOWN_TX.write();
        *tx = None;
    }

    // Clear command channel
    {
        let mut tx = COMMAND_TX.write();
        *tx = None;
    }

    // Clear runtime reconfiguration channel
    {
        let mut tx = CLIENT_COMMAND_TX.write();
        *tx = None;
    }

    // Clear client handle
    {
        let mut client = SENDSPIN_CLIENT.write();
        *client = None;
    }

    // Volume is unknown until the next client loop publishes one.
    CURRENT_VOLUME.store(VOLUME_UNKNOWN, Ordering::Relaxed);
}

/// Restart the Sendspin client with the existing config.
/// Used when settings change (e.g., volume control mode, audio device)
/// to make the new settings take effect immediately.
/// Does nothing if no client is currently running.
pub async fn restart() {
    // Read lock is scoped to this block so it's released before start()
    // calls stop(), which takes a write lock on SENDSPIN_CLIENT.
    let config = {
        SENDSPIN_CLIENT.read().as_ref().map(|c| {
            let mut config = c.config.clone();
            let settings = crate::settings::get_settings();
            config.audio_device_id = settings.audio_device_id;
            config.sync_delay_ms = settings.sync_delay_ms;
            config.player_name = settings.sendspin_player_name;
            config
        })
    };
    if let Some(config) = config {
        log::info!("[Sendspin] Restarting client to apply new settings");
        if let Err(e) = start(config).await {
            log::error!("[Sendspin] Failed to restart client: {}", e);
        }
    } else {
        log::warn!("[Sendspin] Restart requested but no active client configuration is available");
    }
}

/// Live-update the static sync delay without reconnecting Sendspin.
pub fn set_static_delay(sync_delay_ms: i32) -> Result<(), String> {
    let delay_ms = clamp_static_delay_ms(sync_delay_ms);

    let client = SENDSPIN_CLIENT.read();
    if client.is_none() {
        return Ok(());
    }
    drop(client);

    let tx = CLIENT_COMMAND_TX.read();
    if let Some(ref sender) = *tx {
        sender
            .try_send(ClientCommand::SetStaticDelay(delay_ms))
            .map_err(|e| format!("Failed to set static delay: {}", e))?;
    }

    Ok(())
}

/// Send a playback command (play, pause, stop, next, previous)
pub fn send_command(command: &str) -> Result<(), String> {
    let client = SENDSPIN_CLIENT.read();

    if client.is_none() {
        return Err("Sendspin client not connected".to_string());
    }

    // Send command via the command channel to the client loop
    let tx = COMMAND_TX.read();
    if let Some(ref sender) = *tx {
        sender
            .try_send(command.to_string())
            .map_err(|e| format!("Failed to send command: {}", e))?;
        Ok(())
    } else {
        Err("Command channel not available".to_string())
    }
}

/// Get the current runtime player volume as a percentage (0..=100).
/// Reads the lock-free snapshot published by the client loop, so this never
/// blocks and is safe to call from latency-sensitive contexts.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn get_volume_percent() -> Result<u8, String> {
    if SENDSPIN_CLIENT.read().is_none() {
        return Err("Sendspin client not connected".to_string());
    }

    match CURRENT_VOLUME.load(Ordering::Relaxed) {
        VOLUME_UNKNOWN => Err("Volume not reported yet".to_string()),
        volume => Ok(volume.min(100)),
    }
}

/// Set the player volume as a percentage. Values greater than 100 are clamped.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn set_volume_percent(volume: u8) -> Result<(), String> {
    if SENDSPIN_CLIENT.read().is_none() {
        return Err("Sendspin client not connected".to_string());
    }

    let tx = CLIENT_COMMAND_TX.read();
    if let Some(ref sender) = *tx {
        sender
            .try_send(ClientCommand::SetVolume(volume.min(100)))
            .map_err(|e| format!("Failed to set volume: {}", e))?;
        Ok(())
    } else {
        Err("Client command channel not available".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::VolumeControlMode;

    #[test]
    fn resolve_volume_mode_auto_with_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Auto, true),
            ResolvedVolumeMode::Hardware
        );
    }

    #[test]
    fn resolve_volume_mode_auto_without_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Auto, false),
            ResolvedVolumeMode::Software
        );
    }

    #[test]
    fn resolve_volume_mode_hardware_with_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Hardware, true),
            ResolvedVolumeMode::Hardware
        );
    }

    #[test]
    fn resolve_volume_mode_hardware_without_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Hardware, false),
            ResolvedVolumeMode::None
        );
    }

    #[test]
    fn resolve_volume_mode_software_ignores_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Software, true),
            ResolvedVolumeMode::Software
        );
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Software, false),
            ResolvedVolumeMode::Software
        );
    }

    #[test]
    fn resolve_volume_mode_disabled_ignores_hardware() {
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Disabled, true),
            ResolvedVolumeMode::None
        );
        assert_eq!(
            resolve_volume_mode(&VolumeControlMode::Disabled, false),
            ResolvedVolumeMode::None
        );
    }

    #[test]
    fn supported_volume_commands_match_resolved_mode() {
        assert_eq!(
            supported_volume_commands(ResolvedVolumeMode::Hardware),
            vec!["volume".to_string(), "mute".to_string()]
        );
        assert_eq!(
            supported_volume_commands(ResolvedVolumeMode::Software),
            vec!["volume".to_string(), "mute".to_string()]
        );
        assert!(supported_volume_commands(ResolvedVolumeMode::None).is_empty());
    }

    #[test]
    fn auth_response_validation_requires_explicit_success() {
        assert!(validate_auth_response(r#"{"success":true}"#).is_ok());
        assert!(validate_auth_response(r#"{"ok":true}"#).is_ok());
        assert!(validate_auth_response(r#"{"authenticated":true}"#).is_ok());
        assert!(validate_auth_response(r#"{"status":"ok"}"#).is_ok());
        assert!(validate_auth_response(r#"{"type":"auth_ok"}"#).is_ok());

        assert!(validate_auth_response(r#"{"success":false}"#).is_err());
        assert!(validate_auth_response(r#"{"error":"bad token"}"#).is_err());
        assert!(validate_auth_response(r#"{"type":"auth_error","message":"bad token"}"#).is_err());
        assert!(validate_auth_response(r#"{"type":"something_else"}"#).is_err());
        assert!(validate_auth_response("not json").is_err());
    }

    #[test]
    fn test_build_volume_state_msg_produces_client_state() {
        let msg = build_volume_state_msg(75, false);
        let value = serde_json::to_value(&msg).unwrap();

        assert_eq!(value["type"], "client/state");
        assert_eq!(value["payload"]["state"], "synchronized");
        assert_eq!(value["payload"]["player"]["volume"], 75);
        assert_eq!(value["payload"]["player"]["muted"], false);
    }

    #[test]
    fn test_build_volume_state_msg_muted() {
        let msg = build_volume_state_msg(0, true);
        let value = serde_json::to_value(&msg).unwrap();

        assert_eq!(value["payload"]["player"]["volume"], 0);
        assert_eq!(value["payload"]["player"]["muted"], true);
    }

    #[test]
    fn test_build_static_delay_state_msg_produces_client_state() {
        let msg = build_static_delay_state_msg(250);
        let value = serde_json::to_value(&msg).unwrap();

        assert_eq!(value["type"], "client/state");
        assert_eq!(value["payload"]["state"], "synchronized");
        assert_eq!(value["payload"]["player"]["static_delay_ms"], 250);
        assert!(value["payload"]["player"].get("volume").is_none());
        assert!(value["payload"]["player"].get("muted").is_none());
    }

    #[test]
    fn initial_player_state_omits_volume_when_volume_control_disabled() {
        let state = build_initial_player_state(ResolvedVolumeMode::None, 42, true, 123);

        assert_eq!(state.volume, None);
        assert_eq!(state.muted, None);
        assert_eq!(state.static_delay_ms, Some(123));
        assert_eq!(state.required_lead_time_ms, Some(REQUIRED_LEAD_TIME_MS));
        assert_eq!(state.min_buffer_ms, Some(MIN_BUFFER_MS));
        assert_eq!(
            state.supported_commands,
            Some(vec![PlayerStateCommand::SetStaticDelay])
        );
    }

    #[test]
    fn initial_player_state_reports_volume_when_available() {
        let state = build_initial_player_state(ResolvedVolumeMode::Software, 42, true, 123);

        assert_eq!(state.volume, Some(42));
        assert_eq!(state.muted, Some(true));
    }

    #[test]
    fn static_delay_is_clamped_to_protocol_range() {
        assert_eq!(clamp_static_delay_ms(-1), 0);
        assert_eq!(clamp_static_delay_ms(0), 0);
        assert_eq!(clamp_static_delay_ms(250), 250);
        assert_eq!(clamp_static_delay_ms(5_000), 5_000);
        assert_eq!(clamp_static_delay_ms(5_001), 5_000);
        assert_eq!(clamp_static_delay_ms(6_000), 5_000);
    }

    #[test]
    fn player_support_preserves_formats_capacity_and_commands() {
        let formats = vec![AudioFormatSpec {
            codec: "pcm".to_string(),
            channels: 2,
            sample_rate: 48_000,
            bit_depth: 24,
        }];
        let commands = vec!["volume".to_string(), "mute".to_string()];

        let support = build_player_support(formats.clone(), commands.clone());

        assert_eq!(support.supported_formats.len(), 1);
        assert_eq!(support.supported_formats[0].codec, formats[0].codec);
        assert_eq!(support.supported_formats[0].channels, formats[0].channels);
        assert_eq!(
            support.supported_formats[0].sample_rate,
            formats[0].sample_rate
        );
        assert_eq!(support.supported_formats[0].bit_depth, formats[0].bit_depth);
        assert_eq!(support.buffer_capacity, PLAYER_BUFFER_CAPACITY);
        assert_eq!(support.supported_commands, commands);
    }

    #[test]
    fn protocol_builder_requests_player_controller_and_metadata_roles() {
        let config = SendspinConfig {
            player_id: "test_player".to_string(),
            player_name: "Test Player".to_string(),
            server_url: "ws://localhost/sendspin".to_string(),
            audio_device_id: None,
            sync_delay_ms: 0,
            auth_token: "token".to_string(),
            app_version: "9.9.9".to_string(),
        };
        let formats = vec![AudioFormatSpec {
            codec: "pcm".to_string(),
            channels: 2,
            sample_rate: 48_000,
            bit_depth: 16,
        }];
        let player_support = build_player_support(formats.clone(), vec!["volume".to_string()]);
        let initial_state = build_initial_player_state(ResolvedVolumeMode::Software, 100, false, 0);

        let builder = build_protocol_client_builder(&config, player_support, initial_state);

        assert_eq!(
            builder.supported_roles(),
            &[
                "player@v1".to_string(),
                "metadata@v1".to_string(),
                "controller@v1".to_string()
            ]
        );
        let advertised = builder
            .player_v1_support()
            .expect("player support should be configured");
        assert_eq!(advertised.supported_formats.len(), 1);
        assert_eq!(advertised.supported_formats[0].codec, formats[0].codec);
        assert_eq!(
            advertised.supported_formats[0].channels,
            formats[0].channels
        );
        assert_eq!(
            advertised.supported_formats[0].sample_rate,
            formats[0].sample_rate
        );
        assert_eq!(
            advertised.supported_formats[0].bit_depth,
            formats[0].bit_depth
        );
        assert_eq!(advertised.buffer_capacity, PLAYER_BUFFER_CAPACITY);
        assert_eq!(advertised.supported_commands, vec!["volume".to_string()]);
    }
}
