//! Native Sendspin client for Music Assistant Companion
//!
//! This module wraps the sendspin-rs library and adds:
//! - Audio device enumeration and selection
//! - Integration with Tauri (settings, now_playing callbacks)
//! - Playback control commands
//! - Controller role for sending commands
//! - Metadata role for receiving track info

pub mod devices;
pub mod protocol;

use crate::now_playing::{self, NowPlaying};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::sync::Mutex as TokioMutex;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message as WsMessage};

use sendspin::audio::decode::{Decoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec, SyncedPlayer};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientCommand, ClientHello, ClientTime, ControllerCommand,
    DeviceInfo, Message, PlayerState, PlayerSyncState, PlayerV1Support, ClientState,
};
use sendspin::sync::ClockSync;

/// Commands sent to the playback thread
enum PlayerCommand {
    /// Create a new SyncedPlayer with the given format
    CreatePlayer(AudioFormat),
    /// Enqueue an audio buffer for playback
    Enqueue(AudioBuffer),
    /// Clear the playback buffer
    Clear,
    /// Shutdown the playback thread
    Shutdown,
}

/// Auth message for MA proxy
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthMessage {
    #[serde(rename = "type")]
    msg_type: String,
    token: String,
    client_id: String,
}

/// Global Sendspin client instance
static SENDSPIN_CLIENT: RwLock<Option<SendspinClientHandle>> = RwLock::new(None);

/// Whether the Sendspin client is enabled
pub static SENDSPIN_ENABLED: AtomicBool = AtomicBool::new(false);

/// Shutdown signal
static SHUTDOWN_TX: RwLock<Option<mpsc::Sender<()>>> = RwLock::new(None);

/// Command channel for sending playback commands
static COMMAND_TX: RwLock<Option<mpsc::Sender<String>>> = RwLock::new(None);

/// Task handle for the running client
static CLIENT_TASK: RwLock<Option<tokio::task::JoinHandle<()>>> = RwLock::new(None);

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
}

/// Connection status
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
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
        .map(|c| c.status.clone())
        .unwrap_or(ConnectionStatus::Disconnected)
}

/// Get the current player ID (if connected)
pub fn get_player_id() -> Option<String> {
    SENDSPIN_CLIENT
        .read()
        .as_ref()
        .map(|c| c.player_id.clone())
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

/// Start the Sendspin client
///
/// This connects to the Sendspin server and starts audio playback.
/// The client will run in the background and update now_playing state.
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

    // Create shutdown channel
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
    {
        let mut tx = SHUTDOWN_TX.write();
        *tx = Some(shutdown_tx);
    }

    // Create command channel for playback control
    let (command_tx, command_rx) = mpsc::channel::<String>(32);
    {
        let mut tx = COMMAND_TX.write();
        *tx = Some(command_tx);
    }

    // Spawn the client task and store the handle
    let config_clone = config.clone();
    let player_id_clone = player_id.clone();
    let task_handle = tokio::spawn(async move {
        if let Err(e) = run_client(config_clone, player_id_clone, shutdown_rx, command_rx).await {
            eprintln!("[Sendspin] Client error: {}", e);
            update_status(ConnectionStatus::Error(e.to_string()));
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Build ClientHello message
    // Request player, controller, and metadata roles for full functionality
    let hello = ClientHello {
        client_id: player_id.clone(),
        name: config.player_name.clone(),
        version: 1,
        supported_roles: vec![
            "player@v1".to_string(),
            "controller@v1".to_string(),
            "metadata@v1".to_string(),
        ],
        device_info: Some(DeviceInfo {
            product_name: Some(config.player_name.clone()),
            manufacturer: Some("Music Assistant".to_string()),
            software_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
        player_v1_support: Some(PlayerV1Support {
            supported_formats: vec![
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 44100,
                    bit_depth: 16,
                },
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 48000,
                    bit_depth: 24,
                },
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 96000,
                    bit_depth: 24,
                },
            ],
            // Buffer capacity in samples - larger buffer reduces server-side scheduling pressure
            // 480000 = 10 seconds of buffer at 48kHz
            buffer_capacity: 480000,
            // PlayerCommand only supports volume and mute (play/pause/stop are MediaCommands)
            supported_commands: vec![
                "volume".to_string(),
                "mute".to_string(),
            ],
        }),
        artwork_v1_support: None,
        visualizer_v1_support: None,
    };

    // Connect to WebSocket and authenticate with MA proxy
    let (ws_stream, _response) = connect_async(&config.server_url)
        .await
        .map_err(|e| format!("WebSocket connection failed: {}", e))?;

    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Send auth message
    let auth_msg = AuthMessage {
        msg_type: "auth".to_string(),
        token: config.auth_token.clone(),
        client_id: player_id.clone(),
    };
    let auth_json = serde_json::to_string(&auth_msg)
        .map_err(|e| format!("Failed to serialize auth: {}", e))?;

    ws_tx.send(WsMessage::Text(auth_json.into()))
        .await
        .map_err(|e| format!("Failed to send auth: {}", e))?;

    // Wait for auth response (with timeout)
    let auth_timeout = tokio::time::timeout(Duration::from_secs(5), ws_rx.next()).await;
    match auth_timeout {
        Ok(Some(Ok(_))) => {}
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

    // Now send the Sendspin ClientHello
    let hello_msg = Message::ClientHello(hello);
    let hello_json = serde_json::to_string(&hello_msg)
        .map_err(|e| format!("Failed to serialize hello: {}", e))?;
    ws_tx.send(WsMessage::Text(hello_json.into()))
        .await
        .map_err(|e| format!("Failed to send hello: {}", e))?;

    // Wait for ServerHello - try to receive the next few messages
    for i in 0..3 {
        let server_msg = tokio::time::timeout(Duration::from_secs(10), ws_rx.next()).await;
        match server_msg {
            Ok(Some(Ok(WsMessage::Text(text)))) => {
                if text.contains("server/hello") || text.contains("server_hello") {
                    break;
                }
            }
            Ok(Some(Ok(_))) => {
                continue;
            }
            Ok(Some(Err(e))) => {
                return Err(format!("Server message error: {}", e).into());
            }
            Ok(None) => {
                return Err("Connection closed while waiting for ServerHello".into());
            }
            Err(_) => {
                if i == 2 {
                    break;
                }
                continue;
            }
        }
    }
    update_status(ConnectionStatus::Connected);

    // Run the authenticated WebSocket protocol loop
    run_authenticated_client(
        ws_tx,
        ws_rx,
        config,
        player_id,
        shutdown_rx,
        command_rx,
    ).await
}

/// WebSocket stream type for authenticated connections
type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Run the Sendspin client on an already-authenticated WebSocket connection
/// This is used when connecting through the MA proxy which requires auth first
async fn run_authenticated_client(
    mut ws_tx: futures_util::stream::SplitSink<WsStream, WsMessage>,
    mut ws_rx: futures_util::stream::SplitStream<WsStream>,
    config: SendspinConfig,
    player_id: String,
    mut shutdown_rx: mpsc::Receiver<()>,
    mut command_rx: mpsc::Receiver<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Send initial client/state message
    let client_state = Message::ClientState(ClientState {
        player: Some(PlayerState {
            state: PlayerSyncState::Synchronized,
            volume: Some(100),
            muted: Some(false),
        }),
    });
    let state_json = serde_json::to_string(&client_state)?;
    ws_tx.send(WsMessage::Text(state_json.into())).await?;

    // Send initial clock sync
    let client_transmitted = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64;
    let time_msg = Message::ClientTime(ClientTime { client_transmitted });
    let time_json = serde_json::to_string(&time_msg)?;
    ws_tx.send(WsMessage::Text(time_json.into())).await?;

    // Create clock sync interval
    let mut clock_sync_interval = tokio::time::interval(Duration::from_secs(5));

    // Create shared clock sync with Kalman filter-based drift correction
    let clock_sync = Arc::new(TokioMutex::new(ClockSync::new()));

    // Get audio device
    let device = if let Some(ref device_id) = config.audio_device_id {
        match devices::get_device_by_id(device_id) {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("[Sendspin] Failed to get device {}: {}, using default", device_id, e);
                None
            }
        }
    } else {
        None
    };

    // Create channel for sending commands to the playback thread
    let (player_tx, player_rx) = std_mpsc::channel::<PlayerCommand>();

    // Spawn playback thread that owns the SyncedPlayer
    let clock_sync_for_thread = Arc::clone(&clock_sync);
    let _playback_handle = thread::spawn(move || {
        run_playback_thread(player_rx, clock_sync_for_thread, device);
    });

    // Message handling variables
    let mut decoder: Option<PcmDecoder> = None;
    let mut audio_format: Option<AudioFormat> = None;
    let mut endian_locked: Option<PcmEndian> = None;
    let mut playback_started = false;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            _ = clock_sync_interval.tick() => {
                // Send periodic clock sync
                let client_transmitted = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_micros() as i64;
                let time_msg = Message::ClientTime(ClientTime { client_transmitted });
                if let Ok(json) = serde_json::to_string(&time_msg) {
                    let _ = ws_tx.send(WsMessage::Text(json.into())).await;
                }
            }
            Some(cmd) = command_rx.recv() => {
                let command_msg = Message::ClientCommand(ClientCommand {
                    controller: Some(ControllerCommand {
                        command: cmd,
                        volume: None,
                        mute: None,
                    }),
                });
                if let Ok(json) = serde_json::to_string(&command_msg) {
                    let _ = ws_tx.send(WsMessage::Text(json.into())).await;
                }
            }
            Some(ws_msg) = ws_rx.next() => {
                match ws_msg {
                    Ok(WsMessage::Text(text)) => {
                        if let Ok(msg) = serde_json::from_str::<Message>(&text) {
                            match msg {
                                Message::StreamStart(stream_start) => {
                                    let Some(player_config) = stream_start.player else {
                                        continue;
                                    };

                                    if player_config.codec != "pcm" {
                                        eprintln!("[Sendspin] Unsupported codec: {}", player_config.codec);
                                        continue;
                                    }

                                    let fmt = AudioFormat {
                                        codec: Codec::Pcm,
                                        sample_rate: player_config.sample_rate,
                                        channels: player_config.channels,
                                        bit_depth: player_config.bit_depth,
                                        codec_header: None,
                                    };

                                    // Send command to create new player
                                    let _ = player_tx.send(PlayerCommand::CreatePlayer(fmt.clone()));
                                    audio_format = Some(fmt);

                                    decoder = None;
                                    endian_locked = None;
                                    playback_started = false;
                                }
                                Message::ServerTime(server_time) => {
                                    // Update clock sync with drift tracking
                                    let t4 = SystemTime::now()
                                        .duration_since(UNIX_EPOCH)
                                        .unwrap()
                                        .as_micros() as i64;

                                    let t1 = server_time.client_transmitted;
                                    let t2 = server_time.server_received;
                                    let t3 = server_time.server_transmitted;

                                    clock_sync.lock().await.update(t1, t2, t3, t4);
                                }
                                Message::ServerState(state) => {
                                    if let Some(metadata) = state.metadata {
                                        let np = NowPlaying {
                                            is_playing: playback_started,
                                            track: metadata.title,
                                            artist: metadata.artist,
                                            album: metadata.album,
                                            image_url: metadata.artwork_url,
                                            player_name: Some(config.player_name.clone()),
                                            player_id: Some(player_id.clone()),
                                            duration: metadata.progress.as_ref().map(|p| (p.track_duration / 1000) as u64),
                                            elapsed: metadata.progress.as_ref().map(|p| (p.track_progress / 1000) as u64),
                                            can_play: !playback_started,
                                            can_pause: playback_started,
                                            can_next: true,
                                            can_previous: true,
                                        };
                                        now_playing::update_now_playing(np);
                                    }
                                }
                                Message::StreamEnd(_) | Message::StreamClear(_) => {
                                    let _ = player_tx.send(PlayerCommand::Clear);
                                    playback_started = false;
                                }
                                _ => {
                                    // Other messages
                                }
                            }
                        }
                    }
                    Ok(WsMessage::Binary(data)) => {
                        // Audio chunk format: [1 byte type][8 bytes timestamp][audio data]
                        if data.len() < 9 {
                            continue;
                        }

                        let timestamp = i64::from_be_bytes(data[1..9].try_into().unwrap());
                        let audio_data = &data[9..];

                        if let Some(ref fmt) = audio_format {
                            let bytes_per_sample = match fmt.bit_depth {
                                16 => 2,
                                24 => 3,
                                _ => continue,
                            } as usize;
                            let frame_size = bytes_per_sample * fmt.channels as usize;

                            if audio_data.len() % frame_size != 0 {
                                continue;
                            }

                            if endian_locked.is_none() {
                                let endian = PcmEndian::Little;
                                endian_locked = Some(endian);
                                decoder = Some(PcmDecoder::with_endian(fmt.bit_depth, endian));
                            }
                        }

                        if let (Some(ref dec), Some(ref fmt)) = (&decoder, &audio_format) {
                            if let Ok(samples) = dec.decode(audio_data) {
                                if !playback_started {
                                    playback_started = true;
                                    let np = NowPlaying {
                                        is_playing: true,
                                        track: None,
                                        artist: None,
                                        album: None,
                                        image_url: None,
                                        player_name: Some(config.player_name.clone()),
                                        player_id: Some(player_id.clone()),
                                        duration: None,
                                        elapsed: None,
                                        can_play: false,
                                        can_pause: true,
                                        can_next: true,
                                        can_previous: true,
                                    };
                                    now_playing::update_now_playing(np);
                                }

                                let buffer = AudioBuffer {
                                    timestamp,
                                    play_at: Instant::now(), // SyncedPlayer uses timestamp, not play_at
                                    samples,
                                    format: fmt.clone(),
                                };
                                let _ = player_tx.send(PlayerCommand::Enqueue(buffer));
                            }
                        }
                    }
                    Ok(WsMessage::Close(_)) => {
                        break;
                    }
                    Err(e) => {
                        eprintln!("[Sendspin] WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
            else => {
                break;
            }
        }
    }

    // Shutdown playback thread
    let _ = player_tx.send(PlayerCommand::Shutdown);

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

/// Playback thread - owns the SyncedPlayer and processes commands
fn run_playback_thread(
    rx: std_mpsc::Receiver<PlayerCommand>,
    clock_sync: Arc<TokioMutex<ClockSync>>,
    device: Option<cpal::Device>,
) {
    let mut synced_player: Option<SyncedPlayer> = None;

    loop {
        match rx.recv() {
            Ok(PlayerCommand::CreatePlayer(format)) => {
                // Clear existing player if any
                if let Some(ref player) = synced_player {
                    player.clear();
                }

                // Create new SyncedPlayer
                match SyncedPlayer::new(format, Arc::clone(&clock_sync), device.clone()) {
                    Ok(player) => {
                        synced_player = Some(player);
                    }
                    Err(e) => {
                        eprintln!("[Sendspin] Failed to create SyncedPlayer: {}", e);
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
    if let Some(handle) = task_handle {
        // Wait up to 2 seconds for graceful shutdown
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            handle
        ).await;
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

    // Clear client handle
    {
        let mut client = SENDSPIN_CLIENT.write();
        *client = None;
    }
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
