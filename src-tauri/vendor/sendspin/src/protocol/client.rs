// ABOUTME: WebSocket client implementation for Sendspin protocol
// ABOUTME: Handles connection, message routing, and protocol state machine

use crate::error::Error;
use crate::log_sampling::should_log_sample;
use crate::protocol::messages::{
    ArtworkFormatRequest, ClientCommand, ClientGoodbye, ClientHello, ClientState, ClientSyncState,
    ClientTime, ControllerCommand, ControllerCommandType, GoodbyeReason, Message,
    PlayerFormatRequest, PlayerState, RepeatMode, ServerHello, StreamEnd, StreamRequestFormat,
    StreamStart, VisualizerDataType, VisualizerFormatRequest,
};
use crate::sync::raw_clock::Clock;
use crate::sync::ClockSync;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

// One-based sync diagnostic samples: startup detail, then one line per 30 samples.
fn sync_timing_log_sample(count: u64) -> bool {
    count != 0 && (count <= 10 || count.is_multiple_of(30))
}

#[cfg(test)]
mod sync_timing_tests {
    use super::sync_timing_log_sample;

    #[test]
    fn sends_first_ten_then_every_thirty() {
        let sampled: Vec<u64> = (0..=91).filter(|&n| sync_timing_log_sample(n)).collect();
        assert_eq!(sampled, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 30, 60, 90]);
    }
}

/// `Goodbye` is one variant (not `Send` + `Close`) so the writer processes it
/// atomically: once dequeued it flushes goodbye + close and exits, so nothing
/// *enqueued after it* reaches the wire.
enum WriteCommand {
    Send {
        msg: WsMessage,
        sync_t1: Option<i64>,
        ack: tokio::sync::oneshot::Sender<Result<(), Error>>,
    },
    Goodbye {
        reason: GoodbyeReason,
        ack: tokio::sync::oneshot::Sender<Result<(), Error>>,
    },
}

async fn writer_task<S>(
    mut sink: SplitSink<WebSocketStream<S>, WsMessage>,
    mut rx: UnboundedReceiver<WriteCommand>,
    clock: Arc<dyn Clock>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut sync_sent = 0u64;
    while let Some(cmd) = rx.recv().await {
        match cmd {
            WriteCommand::Send { msg, sync_t1, ack } => {
                // These reads use the same raw clock as t1 and t4. Completion
                // means the WebSocket sink flushed, not that the frame hit the wire.
                let dequeued = sync_t1.map(|_| clock.now_micros());
                let result = sink
                    .send(msg)
                    .await
                    .map_err(|e| Error::WebSocket(e.to_string()));
                let flush_completed = dequeued.map(|_| clock.now_micros());
                let failed = result.is_err();
                // Ignore SendError: the caller may have dropped its receiver.
                let _ = ack.send(result);
                if let (Some(t1), Some(dequeued), Some(flush_completed)) =
                    (sync_t1, dequeued, flush_completed)
                {
                    sync_sent = sync_sent.saturating_add(1);
                    if sync_timing_log_sample(sync_sent) {
                        log::info!(
                            "Clock sync send sample {}: t1={}µs, writer_dequeued={}µs, flush_completed={}µs, prequeue={}µs, send_flush_elapsed={}µs, flushed={}",
                            sync_sent,
                            t1,
                            dequeued,
                            flush_completed,
                            dequeued.saturating_sub(t1),
                            flush_completed.saturating_sub(dequeued),
                            !failed
                        );
                    }
                }
                if failed {
                    break;
                }
            }
            WriteCommand::Goodbye { reason, ack } => {
                let _ = ack.send(perform_goodbye(&mut sink, reason).await);
                break;
            }
        }
    }
    log::debug!("Writer task exiting");
    // On exit `rx` drops, dropping the ack sender of any still-queued command;
    // callers awaiting those acks see the cancellation and treat it as a closed
    // connection (see `WsSender::send_message`).
}

async fn perform_goodbye<S>(
    sink: &mut SplitSink<WebSocketStream<S>, WsMessage>,
    reason: GoodbyeReason,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let goodbye = Message::ClientGoodbye(ClientGoodbye { reason });
    let json = serde_json::to_string(&goodbye).map_err(|e| Error::Protocol(e.to_string()))?;
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|e| Error::WebSocket(e.to_string()))?;
    sink.close()
        .await
        .map_err(|e| Error::WebSocket(e.to_string()))
}

/// Connection components returned by [`ProtocolClient::split()`].
/// Use the fields you need; ignore the rest.
pub struct Connection {
    /// Protocol messages from the server
    pub messages: UnboundedReceiver<Message>,
    /// Audio chunks from the server
    pub audio: UnboundedReceiver<AudioChunk>,
    /// Artwork chunks from the server
    pub artwork: UnboundedReceiver<ArtworkChunk>,
    /// Visualizer chunks from the server
    pub visualizer: UnboundedReceiver<VisualizerChunk>,
    /// Clock synchronization state
    pub clock_sync: Arc<Mutex<ClockSync>>,
    /// Sender for writing messages to the server
    pub sender: WsSender,
    /// Controller handle, if the server granted the `controller@v1` role
    pub controller: Option<Controller>,
    /// The `server/hello` received during handshake. Carries `server_id`,
    /// `connection_reason`, and `active_roles` — required for the
    /// multi-server arbitration policy described on [`ProtocolListener`].
    ///
    /// [`ProtocolListener`]: crate::protocol::listener::ProtocolListener
    pub server_hello: ServerHello,
    /// Must be held alive; dropping aborts background tasks
    pub guard: ConnectionGuard,
}

/// Bare role names as they appear in `stream/end` role lists — distinct from
/// the versioned `player@v1` names used during role negotiation.
const ROLE_PLAYER: &str = "player";
const ROLE_ARTWORK: &str = "artwork";
const ROLE_VISUALIZER: &str = "visualizer";

/// Which role streams are currently active, updated by the message router from
/// `stream/start` and `stream/end`.
#[derive(Debug, Default)]
struct StreamState {
    player_active: AtomicBool,
    artwork_active: AtomicBool,
    visualizer_active: AtomicBool,
}

impl StreamState {
    /// A `stream/start` for one role must not disturb another's stream, so
    /// absent roles are left untouched rather than cleared.
    fn note_stream_start(&self, start: &StreamStart) {
        if start.player.is_some() {
            self.player_active.store(true, Ordering::Release);
        }
        if start.artwork.is_some() {
            self.artwork_active.store(true, Ordering::Release);
        }
        if start.visualizer.is_some() {
            self.visualizer_active.store(true, Ordering::Release);
        }
    }

    /// `stream/end` with no roles ends every stream; otherwise only those listed.
    fn note_stream_end(&self, end: &StreamEnd) {
        if role_ended(end, ROLE_PLAYER) {
            self.player_active.store(false, Ordering::Release);
        }
        if role_ended(end, ROLE_ARTWORK) {
            self.artwork_active.store(false, Ordering::Release);
        }
        if role_ended(end, ROLE_VISUALIZER) {
            self.visualizer_active.store(false, Ordering::Release);
        }
    }

    fn is_player_active(&self) -> bool {
        self.player_active.load(Ordering::Acquire)
    }

    fn is_artwork_active(&self) -> bool {
        self.artwork_active.load(Ordering::Acquire)
    }

    fn is_visualizer_active(&self) -> bool {
        self.visualizer_active.load(Ordering::Acquire)
    }
}

fn role_ended(end: &StreamEnd, role: &str) -> bool {
    end.roles
        .as_ref()
        .is_none_or(|roles| roles.iter().any(|r| r == role))
}

/// Cheap to clone. `send_message` returns once the writer has reported the
/// underlying `sink.send` result, so the `Result` reflects the wire-write
/// outcome rather than queue insertion.
#[derive(Debug, Clone)]
pub struct WsSender {
    tx: UnboundedSender<WriteCommand>,
    /// The router updates this *before* forwarding the triggering `stream/start`
    /// / `stream/end`, so a consumer that reacts to those messages already
    /// observes the settled state.
    stream_state: Arc<StreamState>,
}

impl WsSender {
    /// Send a message to the server.
    pub async fn send_message(&self, msg: Message) -> Result<(), Error> {
        let sync_t1 = match &msg {
            Message::ClientTime(time) => Some(time.client_transmitted),
            _ => None,
        };
        let json = serde_json::to_string(&msg).map_err(|e| Error::Protocol(e.to_string()))?;
        // Time pings go out at 1Hz for as long as the connection lives; keep
        // that housekeeping at trace so debug shows only meaningful traffic.
        let level = if matches!(msg, Message::ClientTime(_)) {
            log::Level::Trace
        } else {
            log::Level::Debug
        };
        log::log!(level, "Sending message: {}", json);

        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(WriteCommand::Send {
                msg: WsMessage::Text(json.into()),
                sync_t1,
                ack: ack_tx,
            })
            .map_err(|_| Error::WebSocket("connection closed".to_string()))?;

        // A cancelled ack means the writer dropped the command unsent — the
        // connection is gone either way.
        ack_rx
            .await
            .map_err(|_| Error::WebSocket("connection closed".to_string()))?
    }

    /// Send a top-level client synchronization state update.
    pub async fn send_sync_state(&self, state: ClientSyncState) -> Result<(), Error> {
        self.send_message(Message::ClientState(ClientState {
            state: Some(state),
            player: None,
        }))
        .await
    }

    /// Tell the server this client is temporarily owned by another audio source.
    ///
    /// Release any Sendspin-owned output first so the external source can open
    /// the device without racing this client's audio stream.
    pub async fn enter_external_source(&self) -> Result<(), Error> {
        self.send_sync_state(ClientSyncState::ExternalSource).await
    }

    /// Tell the server this client's clock filter has converged enough to resume
    /// synchronized playback scheduling.
    ///
    /// Include player state when volume, mute, or static delay may have changed
    /// while the external source owned the device. Hardware/OS mixer changes
    /// must be read through platform APIs; this library only tracks its own
    /// software [`GainControl`](crate::audio::GainControl).
    pub async fn exit_external_source(&self, player: Option<PlayerState>) -> Result<(), Error> {
        self.send_message(Message::ClientState(ClientState {
            state: Some(ClientSyncState::Synchronized),
            player,
        }))
        .await
    }

    /// Request a change to the active stream format.
    ///
    /// Sendspin servers may use this advisory message to switch codecs,
    /// sample rates, artwork dimensions, or other stream properties in
    /// response to changing network, CPU, or display conditions. Fields left
    /// as `None` are unconstrained by the client.
    ///
    /// This low-level sender does not enforce negotiated roles; callers should
    /// only use it for connections where the server granted the requested role.
    /// Use [`Connection::server_hello`] when you need to inspect the negotiated
    /// roles before sending.
    ///
    /// A requested component is rejected unless that role's stream is currently
    /// active (between its `stream/start` and `stream/end`): there is nothing to
    /// renegotiate for a role the server is not streaming.
    pub async fn request_stream_format(
        &self,
        player: Option<PlayerFormatRequest>,
        artwork: Option<ArtworkFormatRequest>,
    ) -> Result<(), Error> {
        self.request_stream_formats(player, artwork, None).await
    }

    /// Request changes to any combination of active stream formats.
    ///
    /// Each supplied component must have a corresponding active stream. The
    /// existing [`Self::request_stream_format`] method remains available for
    /// player/artwork-only callers.
    pub async fn request_stream_formats(
        &self,
        player: Option<PlayerFormatRequest>,
        artwork: Option<ArtworkFormatRequest>,
        visualizer: Option<VisualizerFormatRequest>,
    ) -> Result<(), Error> {
        if player.is_none() && artwork.is_none() && visualizer.is_none() {
            return Err(Error::Protocol(
                "stream/request-format requires a player, artwork, or visualizer request"
                    .to_string(),
            ));
        }

        if let Some(request) = visualizer.as_ref() {
            request
                .validate()
                .map_err(|message| Error::Protocol(message.to_string()))?;
        }

        if player.is_some() && !self.stream_state.is_player_active() {
            return Err(Error::Protocol(
                "stream/request-format requires an active player stream".to_string(),
            ));
        }

        if artwork.is_some() && !self.stream_state.is_artwork_active() {
            return Err(Error::Protocol(
                "stream/request-format requires an active artwork stream".to_string(),
            ));
        }

        if visualizer.is_some() && !self.stream_state.is_visualizer_active() {
            return Err(Error::Protocol(
                "stream/request-format requires an active visualizer stream".to_string(),
            ));
        }

        self.send_message(Message::StreamRequestFormat(StreamRequestFormat {
            player,
            artwork,
            visualizer,
        }))
        .await
    }

    /// Request a change to the active player/audio stream format.
    pub async fn request_player_format(&self, player: PlayerFormatRequest) -> Result<(), Error> {
        self.request_stream_format(Some(player), None).await
    }

    /// Request a change to an active artwork stream format.
    pub async fn request_artwork_format(&self, artwork: ArtworkFormatRequest) -> Result<(), Error> {
        self.request_stream_format(None, Some(artwork)).await
    }

    /// Request a change to an active visualizer stream format.
    pub async fn request_visualizer_format(
        &self,
        visualizer: VisualizerFormatRequest,
    ) -> Result<(), Error> {
        self.request_stream_formats(None, None, Some(visualizer))
            .await
    }

    fn send_goodbye(
        &self,
        reason: GoodbyeReason,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<(), Error>>, Error> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(WriteCommand::Goodbye {
                reason,
                ack: ack_tx,
            })
            .map_err(|_| Error::WebSocket("connection closed".to_string()))?;
        Ok(ack_rx)
    }
}

/// Controller handle for sending playback commands to the server.
///
/// Only available when the server grants the `controller@v1` role.
/// Obtained via [`ProtocolClient::split()`].
#[derive(Debug, Clone)]
pub struct Controller {
    sender: WsSender,
}

impl Controller {
    async fn send_controller_command(&self, cmd: ControllerCommand) -> Result<(), Error> {
        let msg = Message::ClientCommand(ClientCommand {
            controller: Some(cmd),
        });
        self.sender.send_message(msg).await
    }

    async fn send_simple_command(&self, command: ControllerCommandType) -> Result<(), Error> {
        self.send_controller_command(ControllerCommand {
            command,
            volume: None,
            mute: None,
            position_ms: None,
            offset_ms: None,
        })
        .await
    }

    /// Resume playback
    pub async fn play(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Play).await
    }

    /// Pause playback
    pub async fn pause(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Pause).await
    }

    /// Stop playback
    pub async fn stop(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Stop).await
    }

    /// Skip to next track
    pub async fn next(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Next).await
    }

    /// Skip to previous track
    pub async fn previous(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Previous)
            .await
    }

    /// Set group volume (0-100). Values above 100 are clamped.
    pub async fn set_volume(&self, volume: u8) -> Result<(), Error> {
        self.send_controller_command(ControllerCommand {
            command: ControllerCommandType::Volume,
            volume: Some(volume.clamp(0, 100)),
            mute: None,
            position_ms: None,
            offset_ms: None,
        })
        .await
    }

    /// Set group mute state
    pub async fn set_mute(&self, muted: bool) -> Result<(), Error> {
        self.send_controller_command(ControllerCommand {
            command: ControllerCommandType::Mute,
            volume: None,
            mute: Some(muted),
            position_ms: None,
            offset_ms: None,
        })
        .await
    }

    /// Set repeat mode
    pub async fn repeat(&self, mode: RepeatMode) -> Result<(), Error> {
        let command = match mode {
            RepeatMode::Off => ControllerCommandType::RepeatOff,
            RepeatMode::One => ControllerCommandType::RepeatOne,
            RepeatMode::All => ControllerCommandType::RepeatAll,
        };
        self.send_simple_command(command).await
    }

    /// Enable or disable shuffle
    pub async fn shuffle(&self, enabled: bool) -> Result<(), Error> {
        let command = if enabled {
            ControllerCommandType::Shuffle
        } else {
            ControllerCommandType::Unshuffle
        };
        self.send_simple_command(command).await
    }

    /// Switch to next group
    pub async fn switch(&self) -> Result<(), Error> {
        self.send_simple_command(ControllerCommandType::Switch)
            .await
    }

    /// Seek to an absolute playback position in milliseconds.
    ///
    /// Only send this when `seek` is in the server's `supported_commands`.
    /// Per the spec, the server ignores the command if `position_ms` is
    /// outside the range 0 to
    /// [`ControllerState::seek_max_ms`](crate::protocol::messages::ControllerState::seek_max_ms).
    pub async fn seek(&self, position_ms: u64) -> Result<(), Error> {
        self.send_controller_command(ControllerCommand {
            command: ControllerCommandType::Seek,
            volume: None,
            mute: None,
            position_ms: Some(position_ms),
            offset_ms: None,
        })
        .await
    }

    /// Seek by a signed offset in milliseconds from the current position
    /// (positive forward, negative backward).
    ///
    /// Only send this when `seek_relative` is in the server's
    /// `supported_commands`. The server applies the offset on a best-effort
    /// basis and clamps the result to the seekable range.
    pub async fn seek_relative(&self, offset_ms: i64) -> Result<(), Error> {
        self.send_controller_command(ControllerCommand {
            command: ControllerCommandType::SeekRelative,
            volume: None,
            mute: None,
            position_ms: None,
            offset_ms: Some(offset_ms),
        })
        .await
    }
}

/// Binary message type IDs per Sendspin spec
pub mod binary_types {
    /// Player audio chunk (types 4-7, we use 4)
    pub const PLAYER_AUDIO: u8 = 0x04;
    /// Artwork channel 0 (type 8)
    pub const ARTWORK_CHANNEL_0: u8 = 0x08;
    /// Artwork channel 1 (type 9)
    pub const ARTWORK_CHANNEL_1: u8 = 0x09;
    /// Artwork channel 2 (type 10)
    pub const ARTWORK_CHANNEL_2: u8 = 0x0A;
    /// Artwork channel 3 (type 11)
    pub const ARTWORK_CHANNEL_3: u8 = 0x0B;
    /// Visualizer loudness data (type 16).
    pub const VISUALIZER_LOUDNESS: u8 = 0x10;
    /// Visualizer beat data (type 17).
    pub const VISUALIZER_BEAT: u8 = 0x11;
    /// Visualizer dominant-frequency data (type 18).
    pub const VISUALIZER_F_PEAK: u8 = 0x12;
    /// Visualizer spectrum data (type 19).
    pub const VISUALIZER_SPECTRUM: u8 = 0x13;
    /// Visualizer energy-onset data (type 20).
    pub const VISUALIZER_PEAK: u8 = 0x14;
    /// Check if a binary type ID is for artwork (8-11)
    pub fn is_artwork(type_id: u8) -> bool {
        (ARTWORK_CHANNEL_0..=ARTWORK_CHANNEL_3).contains(&type_id)
    }

    /// Get artwork channel number from type ID (0-3)
    pub fn artwork_channel(type_id: u8) -> Option<u8> {
        if is_artwork(type_id) {
            Some(type_id - ARTWORK_CHANNEL_0)
        } else {
            None
        }
    }

    /// Check if a binary type ID is for visualizer data (16-20).
    pub fn is_visualizer(type_id: u8) -> bool {
        (VISUALIZER_LOUDNESS..=VISUALIZER_PEAK).contains(&type_id)
    }
}

/// Audio chunk from server (binary type 4)
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Server timestamp in microseconds
    pub timestamp: i64,
    /// Raw audio data bytes
    pub data: Arc<[u8]>,
}

impl AudioChunk {
    /// Parse from WebSocket binary frame (type 4 = player audio)
    pub fn from_bytes(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < 9 {
            return Err(Error::Protocol(format!(
                "Audio chunk too short: got {} bytes, need at least 9",
                frame.len()
            )));
        }

        // Per spec: player audio uses binary type 4
        if frame[0] != binary_types::PLAYER_AUDIO {
            return Err(Error::Protocol(format!(
                "Invalid audio chunk type: expected {}, got {}",
                binary_types::PLAYER_AUDIO,
                frame[0]
            )));
        }

        let timestamp = i64::from_be_bytes([
            frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8],
        ]);

        let data = Arc::from(&frame[9..]);

        Ok(Self { timestamp, data })
    }
}

/// Artwork chunk from server (binary types 8-11)
#[derive(Debug, Clone)]
pub struct ArtworkChunk {
    /// Artwork channel (0-3)
    pub channel: u8,
    /// Server timestamp in microseconds
    pub timestamp: i64,
    /// Image data bytes (JPEG, PNG, or BMP)
    /// Empty payload means clear the artwork
    pub data: Arc<[u8]>,
}

impl ArtworkChunk {
    /// Parse from WebSocket binary frame (types 8-11 = artwork channels 0-3)
    pub fn from_bytes(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < 9 {
            return Err(Error::Protocol(format!(
                "Artwork chunk too short: got {} bytes, need at least 9",
                frame.len()
            )));
        }

        let type_id = frame[0];
        let channel = binary_types::artwork_channel(type_id)
            .ok_or_else(|| Error::Protocol(format!("Invalid artwork chunk type: {}", type_id)))?;

        let timestamp = i64::from_be_bytes([
            frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8],
        ]);

        let data = Arc::from(&frame[9..]);

        Ok(Self {
            channel,
            timestamp,
            data,
        })
    }

    /// Check if this is a clear command (empty payload)
    pub fn is_clear(&self) -> bool {
        self.data.is_empty()
    }
}

/// Visualizer chunk from server (binary types 16-20).
#[derive(Debug, Clone)]
pub struct VisualizerChunk {
    /// Visualizer binary message type (16-20).
    pub type_id: u8,
    /// Server timestamp in microseconds.
    pub timestamp: i64,
    /// Raw visualization data bytes, left for the application to decode.
    pub data: Arc<[u8]>,
}

impl VisualizerChunk {
    /// Return the typed visualizer data kind represented by this chunk.
    ///
    /// Returns `None` if a chunk was constructed manually with an invalid
    /// `type_id`; frames parsed by [`Self::from_bytes`] always return `Some`.
    pub fn data_type(&self) -> Option<VisualizerDataType> {
        match self.type_id {
            binary_types::VISUALIZER_LOUDNESS => Some(VisualizerDataType::Loudness),
            binary_types::VISUALIZER_BEAT => Some(VisualizerDataType::Beat),
            binary_types::VISUALIZER_F_PEAK => Some(VisualizerDataType::FPeak),
            binary_types::VISUALIZER_SPECTRUM => Some(VisualizerDataType::Spectrum),
            binary_types::VISUALIZER_PEAK => Some(VisualizerDataType::Peak),
            _ => None,
        }
    }

    /// Parse from a WebSocket binary frame (visualizer types 16-20).
    pub fn from_bytes(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < 9 {
            return Err(Error::Protocol(format!(
                "Visualizer chunk too short: got {} bytes, need at least 9",
                frame.len()
            )));
        }

        if !binary_types::is_visualizer(frame[0]) {
            return Err(Error::Protocol(format!(
                "Invalid visualizer chunk type: expected 16-20, got {}",
                frame[0]
            )));
        }

        let timestamp = i64::from_be_bytes([
            frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8],
        ]);

        let data = Arc::from(&frame[9..]);

        Ok(Self {
            type_id: frame[0],
            timestamp,
            data,
        })
    }
}

/// Binary frame from server (any type)
#[derive(Debug, Clone)]
pub enum BinaryFrame {
    /// Player audio (type 4)
    Audio(AudioChunk),
    /// Artwork image (types 8-11)
    Artwork(ArtworkChunk),
    /// Visualizer data (types 16-20)
    Visualizer(VisualizerChunk),
    /// Unknown binary type
    Unknown {
        /// The unknown type ID
        type_id: u8,
        /// Raw data after the type byte
        data: Arc<[u8]>,
    },
}

impl BinaryFrame {
    /// Parse any binary frame from WebSocket
    pub fn from_bytes(frame: &[u8]) -> Result<Self, Error> {
        if frame.is_empty() {
            return Err(Error::Protocol("Empty binary frame".to_string()));
        }

        let type_id = frame[0];

        match type_id {
            binary_types::PLAYER_AUDIO => Ok(BinaryFrame::Audio(AudioChunk::from_bytes(frame)?)),
            t if binary_types::is_artwork(t) => {
                Ok(BinaryFrame::Artwork(ArtworkChunk::from_bytes(frame)?))
            }
            t if binary_types::is_visualizer(t) => {
                Ok(BinaryFrame::Visualizer(VisualizerChunk::from_bytes(frame)?))
            }
            // The router warns when it sees the Unknown variant; parsing
            // itself stays quiet to avoid reporting the same frame twice.
            _ => Ok(BinaryFrame::Unknown {
                type_id,
                data: Arc::from(&frame[1..]),
            }),
        }
    }
}

/// WebSocket client for Sendspin protocol
pub struct ProtocolClient {
    out_tx: UnboundedSender<WriteCommand>,
    audio_rx: UnboundedReceiver<AudioChunk>,
    artwork_rx: UnboundedReceiver<ArtworkChunk>,
    visualizer_rx: UnboundedReceiver<VisualizerChunk>,
    message_rx: UnboundedReceiver<Message>,
    clock_sync: Arc<Mutex<ClockSync>>,
    server_hello: ServerHello,
    stream_state: Arc<StreamState>,
    /// Background task guard, aborts tasks on drop
    guard: ConnectionGuard,
}

/// Aborts background tasks on drop. Hold this alive for the lifetime of the
/// connection.
pub struct ConnectionGuard {
    sender: WsSender,
    router_handle: Option<tokio::task::JoinHandle<()>>,
    sync_handle: Option<tokio::task::JoinHandle<()>>,
    writer_handle: Option<tokio::task::JoinHandle<()>>,
}

impl ConnectionGuard {
    /// Gracefully disconnect: enqueue `client/goodbye`, await the writer's
    /// ack so the goodbye + close frames are known to have flushed (or
    /// surface the wire error if they didn't), then reap the writer.
    pub async fn disconnect(mut self, reason: GoodbyeReason) -> Result<(), Error> {
        log::debug!("Disconnecting (reason: {reason:?})");
        // Stop clock-sync first so it can't enqueue time samples behind the
        // goodbye. The reader stays up until the goodbye/close has flushed
        // (below) so the socket isn't half-closed while we're still writing.
        if let Some(h) = self.sync_handle.take() {
            h.abort();
        }

        let ack_rx = self.sender.send_goodbye(reason)?;
        let goodbye_result = ack_rx
            .await
            .map_err(|_| Error::WebSocket("connection closed".to_string()))?;

        // Reap the writer separately from awaiting its ack — the ack arrives
        // just before the task returns, so this only joins the trailing
        // teardown.
        if let Some(h) = self.writer_handle.take() {
            let _ = h.await;
        }

        // Goodbye + close are flushed; tear the reader down now.
        if let Some(h) = self.router_handle.take() {
            h.abort();
        }

        log::debug!("Disconnect complete");
        goodbye_result
    }

    /// Resolves once the connection is dead: the router task has exited
    /// (peer close, transport failure, or teardown). Cancel-safe.
    ///
    /// Liveness means the *reader*. A write-side failure alone does not
    /// fire this — on TCP it resets the read side too in short order, and
    /// sends toward a dead writer fail fast rather than hang.
    pub(crate) async fn closed(&mut self) {
        if let Some(h) = &mut self.router_handle {
            let _ = h.await;
        }
    }

    /// Non-blocking [`Self::closed`].
    pub(crate) fn is_closed(&self) -> bool {
        self.router_handle
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        if let Some(h) = self.router_handle.take() {
            h.abort();
        }
        if let Some(h) = self.sync_handle.take() {
            h.abort();
        }
        if let Some(h) = self.writer_handle.take() {
            h.abort();
        }
    }
}

impl Connection {
    /// See [`WsSender::enter_external_source`].
    pub async fn enter_external_source(&self) -> Result<(), Error> {
        self.sender.enter_external_source().await
    }

    /// See [`WsSender::exit_external_source`].
    pub async fn exit_external_source(&self, player: Option<PlayerState>) -> Result<(), Error> {
        self.sender.exit_external_source(player).await
    }
}

impl ProtocolClient {
    /// Connect to Sendspin server
    pub(crate) async fn connect<R>(
        request: R,
        hello: ClientHello,
        initial_state: ClientState,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Error>
    where
        R: IntoClientRequest + Unpin,
    {
        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;

        Self::drive(ws_stream, hello, initial_state, clock).await
    }

    /// Drive the protocol-client state machine over an already-handshaked
    /// WebSocket stream. Shared between outbound `connect()` and inbound
    /// acceptor paths.
    pub(crate) async fn drive<S>(
        ws_stream: WebSocketStream<S>,
        hello: ClientHello,
        initial_state: ClientState,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (mut write, mut read) = ws_stream.split();

        // The handshake exchange (hello + state) sends directly on the sink
        // rather than through the writer task, so handshake failures are
        // returned synchronously instead of through an ack channel.
        let hello_msg = Message::ClientHello(hello);
        let hello_json =
            serde_json::to_string(&hello_msg).map_err(|e| Error::Protocol(e.to_string()))?;
        log::debug!("Sending client/hello: {}", hello_json);
        write
            .send(WsMessage::Text(hello_json.into()))
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        log::debug!("Waiting for server/hello...");
        let server_hello = loop {
            let Some(result) = read.next().await else {
                log::error!("Connection closed before receiving server/hello");
                return Err(Error::Connection("No server hello received".to_string()));
            };
            match result {
                Ok(WsMessage::Text(text)) => {
                    log::trace!("Received text frame: {}", text);
                    let msg: Message = serde_json::from_str(&text).map_err(|e| {
                        log::error!("Failed to parse server message: {} (payload: {})", e, text);
                        Error::Protocol(e.to_string())
                    })?;

                    match msg {
                        Message::ServerHello(server_hello) => {
                            log::debug!("Received server/hello: {:?}", server_hello);
                            log::info!(
                                "Connected to server: {} ({})",
                                server_hello.name,
                                server_hello.server_id
                            );
                            break server_hello;
                        }
                        _ => {
                            log::error!("Expected server/hello, got: {:?}", msg);
                            return Err(Error::Protocol("Expected server/hello".to_string()));
                        }
                    }
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                    log::debug!("Received Ping/Pong, continuing to wait for server/hello");
                    continue;
                }
                Ok(WsMessage::Close(_)) => {
                    log::error!("Server closed connection");
                    return Err(Error::Connection("Server closed connection".to_string()));
                }
                Ok(other) => {
                    log::warn!(
                        "Unexpected message type while waiting for hello: {:?}",
                        other
                    );
                    continue;
                }
                Err(e) => {
                    log::error!("WebSocket error: {}", e);
                    return Err(Error::WebSocket(e.to_string()));
                }
            }
        };

        let state_msg = Message::ClientState(initial_state);
        let state_json =
            serde_json::to_string(&state_msg).map_err(|e| Error::Protocol(e.to_string()))?;
        log::debug!("Sending initial client/state: {}", state_json);
        write
            .send(WsMessage::Text(state_json.into()))
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        let (out_tx, out_rx) = unbounded_channel::<WriteCommand>();
        let (audio_tx, audio_rx) = unbounded_channel();
        let (artwork_tx, artwork_rx) = unbounded_channel();
        let (visualizer_tx, visualizer_rx) = unbounded_channel();
        let (message_tx, message_rx) = unbounded_channel();
        let clock_sync = Arc::new(Mutex::new(ClockSync::new(Arc::clone(&clock))));
        let stream_state = Arc::new(StreamState::default());

        let writer_handle = tokio::spawn(writer_task(write, out_rx, Arc::clone(&clock)));

        let clock_sync_router = Arc::clone(&clock_sync);
        let clock_router = Arc::clone(&clock);
        let stream_state_router = Arc::clone(&stream_state);
        // The router task handle is used by ConnectionGuard::closed() observers.
        let router_handle = tokio::spawn(async move {
            Self::message_router(
                read,
                audio_tx,
                artwork_tx,
                visualizer_tx,
                message_tx,
                clock_sync_router,
                clock_router,
                stream_state_router,
            )
            .await;
        });

        // First two samples fire 10ms apart so an offset estimate (and
        // playback start) is available almost immediately; drift converges
        // over the following 1Hz samples (see TimeFilter).
        let sync_sender = WsSender {
            tx: out_tx.clone(),
            stream_state: Arc::clone(&stream_state),
        };
        let sync_handle = tokio::spawn(async move {
            let mut sample_count: u32 = 0;
            'sync: loop {
                let t1 = clock.now_micros();
                let msg = Message::ClientTime(ClientTime {
                    client_transmitted: t1,
                });
                match sync_sender.send_message(msg).await {
                    Ok(()) => {
                        sample_count = sample_count.saturating_add(1);
                    }
                    Err(e) => {
                        log::info!("Clock sync task exiting: {}", e);
                        break 'sync;
                    }
                }
                let delay = if sample_count < 2 {
                    tokio::time::Duration::from_millis(10)
                } else {
                    tokio::time::Duration::from_secs(1)
                };
                tokio::time::sleep(delay).await;
            }
        });

        Ok(Self {
            out_tx: out_tx.clone(),
            audio_rx,
            artwork_rx,
            visualizer_rx,
            message_rx,
            clock_sync,
            server_hello,
            stream_state: Arc::clone(&stream_state),
            guard: ConnectionGuard {
                sender: WsSender {
                    tx: out_tx,
                    stream_state,
                },
                router_handle: Some(router_handle),
                sync_handle: Some(sync_handle),
                writer_handle: Some(writer_handle),
            },
        })
    }

    #[allow(clippy::too_many_arguments)] // internal plumbing: per-channel senders + shared state
    async fn message_router<S>(
        mut read: SplitStream<WebSocketStream<S>>,
        audio_tx: UnboundedSender<AudioChunk>,
        artwork_tx: UnboundedSender<ArtworkChunk>,
        visualizer_tx: UnboundedSender<VisualizerChunk>,
        message_tx: UnboundedSender<Message>,
        clock_sync: Arc<Mutex<ClockSync>>,
        clock: Arc<dyn Clock>,
        stream_state: Arc<StreamState>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut audio_closed = false;
        let mut artwork_closed = false;
        let mut visualizer_closed = false;
        let mut message_closed = false;
        let mut audio_chunk_count = 0u64;
        let mut visualizer_chunk_count = 0u64;

        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Binary(data)) => match BinaryFrame::from_bytes(&data) {
                    Ok(BinaryFrame::Audio(chunk)) => {
                        audio_chunk_count += 1;
                        if should_log_sample(audio_chunk_count) {
                            log::trace!(
                                "Received audio chunk: chunk={}, timestamp={}µs, payload_bytes={}, wire_bytes={}",
                                audio_chunk_count,
                                chunk.timestamp,
                                chunk.data.len(),
                                data.len()
                            );
                        }
                        if !audio_closed && audio_tx.send(chunk).is_err() {
                            log::error!("Audio receiver dropped — audio data will be discarded");
                            audio_closed = true;
                        }
                    }
                    Ok(BinaryFrame::Artwork(chunk)) => {
                        // Artwork arrives in short bursts on track changes, so
                        // every chunk is worth a line; audio and visualizer
                        // chunks stream continuously and are sampled instead.
                        log::trace!(
                            "Received artwork chunk: channel={}, timestamp={}µs, payload_bytes={}",
                            chunk.channel,
                            chunk.timestamp,
                            chunk.data.len()
                        );
                        if !artwork_closed && artwork_tx.send(chunk).is_err() {
                            log::error!(
                                "Artwork receiver dropped — artwork data will be discarded"
                            );
                            artwork_closed = true;
                        }
                    }
                    Ok(BinaryFrame::Visualizer(chunk)) => {
                        visualizer_chunk_count += 1;
                        if should_log_sample(visualizer_chunk_count) {
                            log::trace!(
                                "Received visualizer chunk: chunk={}, timestamp={}µs, payload_bytes={}",
                                visualizer_chunk_count,
                                chunk.timestamp,
                                chunk.data.len()
                            );
                        }
                        if !visualizer_closed && visualizer_tx.send(chunk).is_err() {
                            log::error!(
                                "Visualizer receiver dropped — visualizer data will be discarded"
                            );
                            visualizer_closed = true;
                        }
                    }
                    Ok(BinaryFrame::Unknown { type_id, .. }) => {
                        log::warn!("Received unknown binary type: {}", type_id);
                    }
                    Err(e) => {
                        log::warn!("Failed to parse binary frame: {}", e);
                    }
                },
                Ok(WsMessage::Text(text)) => {
                    // Capture receive time before deserialization so
                    // t4 is as close to the true arrival time as possible.
                    let t4 = clock.now_micros();
                    log::trace!("Received text frame: {}", text);
                    match serde_json::from_str::<Message>(&text) {
                        Ok(msg) => {
                            // ServerTime is consumed here for clock sync
                            // and intentionally NOT forwarded to message_rx
                            // consumers — it's an internal protocol detail.
                            // It also arrives at 1Hz for as long as the
                            // connection lives, so it stays out of the debug
                            // view; ClockSync::update logs the computed sync
                            // state instead.
                            if let Message::ServerTime(ref st) = msg {
                                clock_sync.lock().update(
                                    st.client_transmitted,
                                    st.server_received,
                                    st.server_transmitted,
                                    t4,
                                );
                            } else {
                                log::debug!("Received message: {:?}", msg);
                                // Settle the request-format gate before
                                // forwarding, so a consumer reacting to this
                                // stream/start or stream/end sees current state.
                                match &msg {
                                    Message::StreamStart(start) => {
                                        stream_state.note_stream_start(start)
                                    }
                                    Message::StreamEnd(end) => stream_state.note_stream_end(end),
                                    _ => {}
                                }
                                if !message_closed && message_tx.send(msg).is_err() {
                                    log::error!(
                                        "Message receiver dropped — messages will be discarded"
                                    );
                                    message_closed = true;
                                }
                            }
                        }
                        Err(e) => {
                            log::warn!("Failed to parse message: {} (payload: {})", e, text);
                        }
                    }
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {}
                Ok(WsMessage::Close(_)) => {
                    log::info!("Server closed connection");
                    break;
                }
                Err(e) => {
                    log::error!("WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }
        log::debug!("Message router: WebSocket stream ended");
    }

    /// Gracefully disconnect: sends `client/goodbye`, closes the WebSocket,
    /// and aborts background tasks.
    pub async fn disconnect(self, reason: GoodbyeReason) -> Result<(), Error> {
        self.guard.disconnect(reason).await
    }

    /// See [`WsSender::enter_external_source`].
    pub async fn enter_external_source(&self) -> Result<(), Error> {
        WsSender {
            tx: self.out_tx.clone(),
            stream_state: Arc::clone(&self.stream_state),
        }
        .enter_external_source()
        .await
    }

    /// See [`WsSender::exit_external_source`].
    pub async fn exit_external_source(&self, player: Option<PlayerState>) -> Result<(), Error> {
        WsSender {
            tx: self.out_tx.clone(),
            stream_state: Arc::clone(&self.stream_state),
        }
        .exit_external_source(player)
        .await
    }

    /// Get reference to clock sync
    pub fn clock_sync(&self) -> Arc<Mutex<ClockSync>> {
        Arc::clone(&self.clock_sync)
    }

    /// The `server/hello` received during handshake. Carries `server_id`,
    /// `connection_reason`, and `active_roles` — required for the
    /// multi-server arbitration policy described on [`ProtocolListener`].
    ///
    /// [`ProtocolListener`]: crate::protocol::listener::ProtocolListener
    pub fn server_hello(&self) -> &ServerHello {
        &self.server_hello
    }

    /// Split into separate receivers for concurrent processing.
    ///
    /// This allows using `tokio::select!` to process messages and binary
    /// data concurrently. Use the fields you need; ignore the rest.
    pub fn split(self) -> Connection {
        let sender = WsSender {
            tx: self.out_tx,
            stream_state: self.stream_state,
        };
        let controller = self
            .server_hello
            .active_roles
            .iter()
            .any(|r| r == "controller@v1")
            .then(|| Controller {
                sender: sender.clone(),
            });
        Connection {
            messages: self.message_rx,
            audio: self.audio_rx,
            artwork: self.artwork_rx,
            visualizer: self.visualizer_rx,
            clock_sync: self.clock_sync,
            sender,
            controller,
            server_hello: self.server_hello,
            guard: self.guard,
        }
    }
}
