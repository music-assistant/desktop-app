// ABOUTME: Protocol message type definitions and serialization
// ABOUTME: Supports all Sendspin protocol messages per spec

use serde::{Deserialize, Serialize};

/// Top-level protocol message envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Message {
    // === Handshake messages ===
    /// Client hello handshake message
    #[serde(rename = "client/hello")]
    ClientHello(ClientHello),

    /// Server hello handshake response
    #[serde(rename = "server/hello")]
    ServerHello(ServerHello),

    // === Time synchronization ===
    /// Client time synchronization request
    #[serde(rename = "client/time")]
    ClientTime(ClientTime),

    /// Server time synchronization response
    #[serde(rename = "server/time")]
    ServerTime(ServerTime),

    // === State messages ===
    /// Client state update
    #[serde(rename = "client/state")]
    ClientState(ClientState),

    /// Server state update (metadata, controller info)
    #[serde(rename = "server/state")]
    ServerState(ServerState),

    // === Command messages ===
    /// Server command to client (player commands)
    #[serde(rename = "server/command")]
    ServerCommand(ServerCommand),

    /// Client command to server (controller commands)
    #[serde(rename = "client/command")]
    ClientCommand(ClientCommand),

    // === Stream control messages ===
    /// Stream start notification
    #[serde(rename = "stream/start")]
    StreamStart(StreamStart),

    /// Stream end notification
    #[serde(rename = "stream/end")]
    StreamEnd(StreamEnd),

    /// Stream clear notification
    #[serde(rename = "stream/clear")]
    StreamClear(StreamClear),

    /// Client request for specific stream format
    #[serde(rename = "stream/request-format")]
    StreamRequestFormat(StreamRequestFormat),

    // === Group messages ===
    /// Group update notification
    #[serde(rename = "group/update")]
    GroupUpdate(GroupUpdate),

    // === Connection lifecycle ===
    /// Client goodbye message
    #[serde(rename = "client/goodbye")]
    ClientGoodbye(ClientGoodbye),
}

// =============================================================================
// Handshake Messages
// =============================================================================

/// Client hello message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    /// Unique client identifier
    pub client_id: String,
    /// Human-readable client name
    pub name: String,
    /// Protocol version number
    pub version: u32,
    /// List of supported roles with versions (e.g., "player@v1", "controller@v1")
    pub supported_roles: Vec<String>,
    /// Device information (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_info: Option<DeviceInfo>,
    /// Player capabilities (if client supports player@v1 role)
    #[serde(rename = "player@v1_support", skip_serializing_if = "Option::is_none")]
    pub player_v1_support: Option<PlayerV1Support>,
    /// Artwork capabilities (if client supports artwork@v1 role)
    #[serde(rename = "artwork@v1_support", skip_serializing_if = "Option::is_none")]
    pub artwork_v1_support: Option<ArtworkV1Support>,
    /// Visualizer capabilities (if client supports visualizer@v1 role)
    #[serde(
        rename = "visualizer@v1_support",
        skip_serializing_if = "Option::is_none"
    )]
    pub visualizer_v1_support: Option<VisualizerV1Support>,
}

/// Device information (all fields optional per spec)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Product name (e.g., "Sendspin-RS Player")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_name: Option<String>,
    /// Manufacturer name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    /// Software version string
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_version: Option<String>,
    /// MAC address of the network interface the connection is opened on, in lowercase colon-separated form (e.g., `aa:bb:cc:dd:ee:ff`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac_address: Option<String>,
}

/// Player@v1 capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerV1Support {
    /// List of supported audio formats
    pub supported_formats: Vec<AudioFormatSpec>,
    /// Buffer capacity in chunks
    pub buffer_capacity: u32,
    /// List of supported playback commands
    pub supported_commands: Vec<String>,
}

/// Audio format specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFormatSpec {
    /// Codec name (e.g., "pcm", "opus", "flac")
    pub codec: String,
    /// Number of audio channels
    pub channels: u8,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Bit depth per sample
    pub bit_depth: u8,
}

/// Artwork@v1 capabilities
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtworkV1Support {
    /// Supported artwork channels (1-4 channels, array index is channel number)
    pub channels: Vec<ArtworkChannel>,
}

/// Artwork channel configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtworkChannel {
    /// Artwork source type
    pub source: ArtworkSource,
    /// Image format
    pub format: ImageFormat,
    /// Max width in pixels
    pub media_width: u32,
    /// Max height in pixels
    pub media_height: u32,
}

/// Artwork source type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ArtworkSource {
    /// Album artwork
    Album,
    /// Artist image
    Artist,
    /// No artwork (channel disabled)
    None,
}

/// Image format
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    /// JPEG format
    Jpeg,
    /// PNG format
    Png,
    /// BMP format
    Bmp,
}

/// Visualizer@v1 capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisualizerV1Support {
    /// Visualization data types requested by the client.
    pub types: Vec<VisualizerDataType>,
    /// Maximum total size of buffered visualizer messages in bytes.
    pub buffer_capacity: u32,
    /// Maximum periodic visualization frames per second.
    pub rate_max: u32,
    /// Spectrum configuration, required when `types` includes `spectrum`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<SpectrumConfig>,
}

impl VisualizerV1Support {
    /// Validate the cross-field spectrum requirement from the Sendspin spec.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_spectrum(&self.types, self.spectrum.as_ref())
    }
}

/// Visualization data type carried by a visualizer binary message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VisualizerDataType {
    /// Overall A-weighted loudness.
    Loudness,
    /// Musical beat event.
    Beat,
    /// Dominant frequency and amplitude.
    FPeak,
    /// FFT magnitudes mapped to display bins.
    Spectrum,
    /// Energy onset event.
    Peak,
}

/// Spectrum display-bin configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpectrumConfig {
    /// Number of display bins.
    pub n_disp_bins: u32,
    /// Frequency-to-bin mapping.
    pub scale: SpectrumScale,
    /// Lowest frequency in Hz.
    pub f_min: u32,
    /// Highest frequency in Hz.
    pub f_max: u32,
}

fn validate_spectrum(
    types: &[VisualizerDataType],
    spectrum: Option<&SpectrumConfig>,
) -> Result<(), &'static str> {
    let has_spectrum = types.contains(&VisualizerDataType::Spectrum);
    match (has_spectrum, spectrum.is_some()) {
        (true, false) => Err("spectrum configuration is required for spectrum data"),
        (false, true) => Err("spectrum configuration requires spectrum data"),
        _ => Ok(()),
    }
}

/// Frequency-to-bin mapping for spectrum data.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SpectrumScale {
    /// HTK mel-frequency spacing.
    Mel,
    /// Base-10 logarithmic spacing.
    Log,
    /// Linear frequency spacing.
    Lin,
}

/// Server hello message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    /// Unique server identifier
    pub server_id: String,
    /// Human-readable server name
    pub name: String,
    /// Protocol version number
    pub version: u32,
    /// List of roles activated by server for this client
    pub active_roles: Vec<String>,
    /// Reason for connection: 'discovery' or 'playback'
    pub connection_reason: ConnectionReason,
}

/// Connection reason enum
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionReason {
    /// Server connected for discovery/announcement
    Discovery,
    /// Server connected for active playback
    Playback,
}

// =============================================================================
// Time Synchronization
// =============================================================================

/// Client time sync message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTime {
    /// Client transmission timestamp (raw monotonic microseconds)
    pub client_transmitted: i64,
}

/// Server time sync response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTime {
    /// Original client transmission timestamp
    pub client_transmitted: i64,
    /// Server reception timestamp (server loop microseconds)
    pub server_received: i64,
    /// Server transmission timestamp (server loop microseconds)
    pub server_transmitted: i64,
}

// =============================================================================
// State Messages
// =============================================================================

/// Client state update message (wraps role-specific state)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientState {
    /// Client operational state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<ClientSyncState>,
    /// Player state (if player role active)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<PlayerState>,
}

/// Player state
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerState {
    /// Current volume level (0-100)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// Whether audio is muted
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    /// Static delay in milliseconds (0-5000) to compensate for external speaker/amplifier latency
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub static_delay_ms: Option<u16>,
    /// Minimum startup lead time in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub required_lead_time_ms: Option<u32>,
    /// Requested minimum ongoing buffer duration in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub min_buffer_ms: Option<u32>,
    /// Supported player state commands
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub supported_commands: Option<Vec<PlayerStateCommand>>,
}

/// Commands that can appear in PlayerState.supported_commands
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStateCommand {
    /// Client supports set_static_delay command
    SetStaticDelay,
}

/// Client operational state (top-level in client/state).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientSyncState {
    /// Client's clock filter has converged enough to begin scheduling playback.
    Synchronized,
    /// Client is in use by an external system (e.g., different audio source, HDMI input)
    /// and is not currently participating in Sendspin playback with this server.
    ExternalSource,
}

/// Server state update message (metadata and controller info)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerState {
    /// Metadata state (track info, progress, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataState>,
    /// Controller state (supported commands, volume, etc.)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerState>,
    /// Color state (colors derived from the current audio)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<ColorState>,
}

/// An RGB color as `[R, G, B]` with components 0-255.
pub type Rgb = [u8; 3];

/// Color state from server (color role).
///
/// Colors may be extracted from album artwork, provided by the music source,
/// or manually programmed by the server. All color fields are optional; the
/// server sends only the palette entries it has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColorState {
    /// Server clock time in microseconds for when these colors are valid
    pub timestamp: i64,
    /// Background color suitable for dark mode. The server ensures a minimum
    /// WCAG contrast ratio of 4.5:1 with white text and with `on_dark`
    /// (if also present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_dark: Option<Rgb>,
    /// Background color suitable for light mode. The server ensures a minimum
    /// WCAG contrast ratio of 4.5:1 with black text and with `on_light`
    /// (if also present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background_light: Option<Rgb>,
    /// The dominant color. Not adjusted for contrast.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<Rgb>,
    /// A secondary or complementary color. Not adjusted for contrast.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<Rgb>,
    /// A light color suitable for use on dark backgrounds. The server ensures
    /// a minimum WCAG contrast ratio of 4.5:1 with `background_dark` (if also
    /// present) and with black text, so it can also serve as an alternative
    /// light background.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_dark: Option<Rgb>,
    /// A dark color suitable for use on light backgrounds. The server ensures
    /// a minimum WCAG contrast ratio of 4.5:1 with `background_light` (if also
    /// present) and with white text, so it can also serve as an alternative
    /// dark background.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_light: Option<Rgb>,
}

/// Metadata state from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataState {
    /// Server timestamp for progress calculation (microseconds)
    pub timestamp: i64,
    /// Track title
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Artist name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// Album artist
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,
    /// Album name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Artwork URL
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
    /// Release year
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    /// Track number (1-indexed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<u32>,
    /// Current track progress
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<TrackProgress>,
    /// Repeat mode
    ///
    /// Deprecated: the spec moved `repeat` to the controller `server/state`
    /// object. Prefer [`ControllerState::repeat`] instead. This field is kept
    /// only to parse the legacy dual-emit and will be removed in a future
    /// release.
    #[deprecated(since = "0.3.0", note = "use ControllerState::repeat instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatMode>,
    /// Shuffle state
    ///
    /// Deprecated: the spec moved `shuffle` to the controller `server/state`
    /// object. Prefer [`ControllerState::shuffle`] instead. This field is kept
    /// only to parse the legacy dual-emit and will be removed in a future
    /// release.
    #[deprecated(since = "0.3.0", note = "use ControllerState::shuffle instead")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
}

/// Track progress information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackProgress {
    /// Current position in milliseconds
    pub track_progress: i64,
    /// Total duration in milliseconds (0 for unknown/live streams)
    pub track_duration: i64,
    /// Playback speed multiplier * 1000 (1000 = normal, 0 = paused)
    pub playback_speed: i32,
}

/// Repeat mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    /// No repeat
    Off,
    /// Repeat current track
    One,
    /// Repeat all tracks
    All,
}

/// Controller state from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerState {
    /// List of supported commands
    pub supported_commands: Vec<String>,
    /// Current volume level (0-100)
    pub volume: u8,
    /// Whether audio is muted
    pub muted: bool,
    /// Repeat mode
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatMode>,
    /// Shuffle state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    /// Maximum absolute position in milliseconds a 'seek' may target (e.g.,
    /// the end of the current track). Present whenever 'seek' is in
    /// `supported_commands`; absent when the seekable range is unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seek_max_ms: Option<u64>,
}

// =============================================================================
// Command Messages
// =============================================================================

/// Server command message (wraps role-specific commands)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCommand {
    /// Player command (if targeting player role)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<PlayerCommand>,
}

/// Player-specific command from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerCommand {
    /// Command to execute
    pub command: PlayerCommandType,
    /// Optional volume level (0-100)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// Optional mute state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    /// Optional static delay in milliseconds (0-5000)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub static_delay_ms: Option<u16>,
}

/// Player command type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlayerCommandType {
    /// Set volume level
    Volume,
    /// Set mute state
    Mute,
    /// Set static delay
    SetStaticDelay,
    /// Unknown command (forward compatibility)
    #[serde(other)]
    Unknown,
}

/// Client command message (controller commands to server)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCommand {
    /// Controller command
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerCommand>,
}

/// Controller command from client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerCommand {
    /// Command to execute
    pub command: ControllerCommandType,
    /// Optional volume level (0-100) for volume command
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// Optional mute state for mute command
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
    /// Absolute playback position in milliseconds, range 0 to
    /// [`ControllerState::seek_max_ms`]. Only set for the `seek` command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    /// Signed offset in milliseconds from the current position (positive
    /// forward, negative backward). Only set for the `seek_relative` command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset_ms: Option<i64>,
}

/// Controller command type
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ControllerCommandType {
    /// Resume playback
    Play,
    /// Pause playback
    Pause,
    /// Stop playback and reset position
    Stop,
    /// Skip to next track
    Next,
    /// Skip to previous track
    Previous,
    /// Set group volume
    Volume,
    /// Set group mute state
    Mute,
    /// Disable repeat
    RepeatOff,
    /// Repeat current track
    RepeatOne,
    /// Repeat all tracks
    RepeatAll,
    /// Randomize playback order
    Shuffle,
    /// Restore original playback order
    Unshuffle,
    /// Switch to next group
    Switch,
    /// Seek to an absolute position (requires `position_ms`)
    Seek,
    /// Seek by a signed offset from the current position (requires `offset_ms`)
    SeekRelative,
}

// =============================================================================
// Stream Control Messages
// =============================================================================

/// Stream start message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStart {
    /// Player stream configuration (optional - only if player role active)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<StreamPlayerConfig>,
    /// Artwork stream configuration (optional - only if artwork role active)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork: Option<StreamArtworkConfig>,
    /// Visualizer stream configuration (optional - only if visualizer role active)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visualizer: Option<StreamVisualizerConfig>,
}

/// Stream player configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlayerConfig {
    /// Audio codec name
    pub codec: String,
    /// Sample rate in Hz
    pub sample_rate: u32,
    /// Number of audio channels
    pub channels: u8,
    /// Bit depth per sample
    pub bit_depth: u8,
    /// Optional codec-specific header (base64 encoded)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_header: Option<String>,
}

/// Stream artwork configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtworkConfig {
    /// Configuration for each active artwork channel, array index is the channel number
    pub channels: Vec<StreamArtworkChannelConfig>,
}

/// Configuration for a single artwork channel in stream/start
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamArtworkChannelConfig {
    /// Artwork source type
    pub source: ArtworkSource,
    /// Format of the encoded image
    pub format: ImageFormat,
    /// Width in pixels of the encoded image
    pub width: u32,
    /// Height in pixels of the encoded image
    pub height: u32,
}

/// Stream visualizer configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamVisualizerConfig {
    /// Visualization data types the server will stream.
    pub types: Vec<VisualizerDataType>,
    /// Maximum periodic visualization frames per second.
    pub rate_max: u32,
    /// Whether the beat tracker identifies bar starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracks_downbeats: Option<bool>,
    /// Spectrum configuration, present when `types` includes `spectrum`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<SpectrumConfig>,
}

impl StreamVisualizerConfig {
    /// Validate the cross-field spectrum requirement from the Sendspin spec.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_spectrum(&self.types, self.spectrum.as_ref())
    }
}

/// Stream end message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamEnd {
    /// Roles for which streaming has ended (optional, all if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
}

/// Stream clear message (clear buffers)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamClear {
    /// Roles for which buffers should be cleared (optional, all if not specified)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roles: Option<Vec<String>>,
}

/// Stream format request from client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamRequestFormat {
    /// Requested player format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player: Option<PlayerFormatRequest>,
    /// Requested artwork format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork: Option<ArtworkFormatRequest>,
    /// Requested visualizer format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visualizer: Option<VisualizerFormatRequest>,
}

/// Requested visualizer stream format. Omitted fields keep their current value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VisualizerFormatRequest {
    /// New visualization data types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub types: Option<Vec<VisualizerDataType>>,
    /// New periodic visualization frames-per-second cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_max: Option<u32>,
    /// New spectrum configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectrum: Option<SpectrumConfig>,
}

impl VisualizerFormatRequest {
    /// Validate that this request changes at least one field.
    ///
    /// This is a partial update: omitted fields retain their current value, so
    /// a request may omit `spectrum` even when its new `types` includes it.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.types.is_none() && self.rate_max.is_none() && self.spectrum.is_none() {
            return Err("visualizer format request must specify at least one field");
        }
        if let (Some(types), Some(spectrum)) = (self.types.as_ref(), self.spectrum.as_ref()) {
            validate_spectrum(types, Some(spectrum))
        } else {
            Ok(())
        }
    }
}

/// Player format request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerFormatRequest {
    /// Preferred codec
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    /// Preferred channel count
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u8>,
    /// Preferred sample rate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    /// Preferred bit depth
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_depth: Option<u8>,
}

/// Artwork format request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtworkFormatRequest {
    /// Artwork channel to request
    pub channel: u8,
    /// Preferred image source
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ArtworkSource>,
    /// Preferred image format
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ImageFormat>,
    /// Display width in pixels
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_width: Option<u32>,
    /// Display height in pixels
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_height: Option<u32>,
}

// =============================================================================
// Group Messages
// =============================================================================

/// Group update notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupUpdate {
    /// Current playback state of the group
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_state: Option<PlaybackState>,
    /// Group identifier
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Human-readable group name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
}

/// Group playback state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    /// Audio is playing
    Playing,
    /// Playback is stopped
    Stopped,
}

// =============================================================================
// Connection Lifecycle
// =============================================================================

/// Client goodbye message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientGoodbye {
    /// Reason for disconnection
    pub reason: GoodbyeReason,
}

/// Goodbye reason
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoodbyeReason {
    /// Switching to another server
    AnotherServer,
    /// Client is shutting down
    Shutdown,
    /// Client is restarting
    Restart,
    /// User requested disconnect
    UserRequest,
}

// =============================================================================
// Legacy Aliases (deprecated)
// =============================================================================

/// Legacy type alias for backwards compatibility
#[deprecated(note = "Use PlayerV1Support instead")]
pub type PlayerSupport = PlayerV1Support;
