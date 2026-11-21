# Resonate-RS Phase 1: Foundation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the foundation of resonate-rs with core types, protocol message handling, WebSocket client, PCM decoder, and clock synchronization.

**Architecture:** A hyper-efficient Rust implementation of the Resonate Protocol using zero-copy data paths, async I/O with Tokio, and lock-free concurrency. This phase establishes the core types and protocol layer without audio output (that's Phase 2).

**Tech Stack:** Rust, Tokio (async runtime), tokio-tungstenite (WebSocket), serde/serde_json (serialization), thiserror (error handling)

---

## Task 1: Project Initialization & Core Dependencies

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `.gitignore`

**Step 1: Initialize Rust project**

Run: `cargo init --lib --name resonate`
Expected: Creates basic Cargo.toml and src/lib.rs

**Step 2: Configure Cargo.toml with dependencies**

Update `Cargo.toml`:
```toml
[package]
name = "resonate"
version = "0.1.0"
edition = "2021"
authors = ["Resonate Contributors"]
description = "Hyper-efficient Rust implementation of the Resonate Protocol for synchronized multi-room audio streaming"
license = "MIT OR Apache-2.0"
repository = "https://github.com/resonate-protocol/resonate-rs"

[dependencies]
# Async runtime
tokio = { version = "1.40", features = ["full"] }
tokio-tungstenite = "0.24"
futures-util = "0.3"

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# Error handling
thiserror = "1.0"

# Utilities
log = "0.4"
uuid = { version = "1.10", features = ["v4", "serde"] }

[dev-dependencies]
tokio-test = "0.4"
env_logger = "0.11"

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
```

**Step 3: Create .gitignore**

Create `.gitignore`:
```
/target
Cargo.lock
*.swp
*.swo
.DS_Store
```

**Step 4: Create basic lib.rs structure**

Update `src/lib.rs`:
```rust
// ABOUTME: Main library entry point for resonate-rs
// ABOUTME: Exports public API for Resonate Protocol client and server

#![warn(missing_docs)]
#![doc = include_str!("../README.md")]

pub mod audio;
pub mod protocol;
pub mod sync;

pub use protocol::client::ProtocolClient;
pub use protocol::messages::{ClientHello, ServerHello};

/// Result type for resonate operations
pub type Result<T> = std::result::Result<T, error::Error>;

/// Error types for resonate
pub mod error {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("WebSocket error: {0}")]
        WebSocket(String),

        #[error("Protocol error: {0}")]
        Protocol(String),

        #[error("Invalid message format")]
        InvalidMessage,

        #[error("Connection error: {0}")]
        Connection(String),
    }
}
```

**Step 5: Verify build**

Run: `cargo build`
Expected: Should fail because modules don't exist yet - that's expected

**Step 6: Commit**

```bash
git add .
git commit -m "chore: initialize resonate-rs project with dependencies"
```

---

## Task 2: Core Audio Types

**Files:**
- Create: `src/audio/mod.rs`
- Create: `src/audio/types.rs`
- Create: `tests/audio_types.rs`

**Step 1: Write test for Sample type**

Create `tests/audio_types.rs`:
```rust
use resonate::audio::{Sample, Codec, AudioFormat};

#[test]
fn test_sample_from_i16() {
    let sample = Sample::from_i16(1000);
    assert_eq!(sample.to_i16(), 1000);
}

#[test]
fn test_sample_from_i24() {
    let bytes = [0x00, 0x10, 0x00]; // 4096 in 24-bit little-endian
    let sample = Sample::from_i24_le(bytes);
    assert_eq!(sample.0, 4096);
}

#[test]
fn test_sample_clamp() {
    let over_max = Sample(10_000_000);
    assert_eq!(over_max.clamp().0, Sample::MAX.0);

    let under_min = Sample(-10_000_000);
    assert_eq!(under_min.clamp().0, Sample::MIN.0);
}

#[test]
fn test_audio_format_creation() {
    let format = AudioFormat {
        codec: Codec::Pcm,
        sample_rate: 48000,
        channels: 2,
        bit_depth: 24,
        codec_header: None,
    };

    assert_eq!(format.sample_rate, 48000);
    assert_eq!(format.channels, 2);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_sample_from_i16`
Expected: FAIL - module doesn't exist

**Step 3: Create audio module structure**

Create `src/audio/mod.rs`:
```rust
// ABOUTME: Audio types and processing for resonate-rs
// ABOUTME: Contains Sample type, AudioFormat, Buffer, and codec definitions

pub mod types;

pub use types::{Sample, Codec, AudioFormat, AudioBuffer};
```

**Step 4: Implement Sample and AudioFormat types**

Create `src/audio/types.rs`:
```rust
// ABOUTME: Core audio type definitions
// ABOUTME: Sample (24-bit), AudioFormat, AudioBuffer for zero-copy audio data

use std::sync::Arc;
use std::time::Instant;

/// 24-bit audio sample stored in i32
/// Range: -8388608 to 8388607 (±2^23)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Sample(pub i32);

impl Sample {
    pub const MAX: Self = Self(8_388_607);   // 2^23 - 1
    pub const MIN: Self = Self(-8_388_608);  // -2^23
    pub const ZERO: Self = Self(0);

    /// Convert from 16-bit sample (shift left 8 bits)
    #[inline]
    pub fn from_i16(s: i16) -> Self {
        Self((s as i32) << 8)
    }

    /// Convert from 24-bit little-endian bytes
    #[inline]
    pub fn from_i24_le(bytes: [u8; 3]) -> Self {
        let val = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0]);
        Self(val >> 8)  // Sign-extend
    }

    /// Convert to 16-bit sample (shift right 8 bits)
    #[inline]
    pub fn to_i16(self) -> i16 {
        (self.0 >> 8) as i16
    }

    /// Clamp to valid 24-bit range
    #[inline]
    pub fn clamp(self) -> Self {
        Self(self.0.clamp(Self::MIN.0, Self::MAX.0))
    }
}

/// Audio codec type
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Codec {
    Pcm,
    Opus,
    Flac,
    Mp3,
}

/// Audio format specification
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioFormat {
    pub codec: Codec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
    pub codec_header: Option<Vec<u8>>,
}

/// Audio buffer with timestamp (zero-copy via Arc)
pub struct AudioBuffer {
    pub timestamp: i64,           // Server loop microseconds
    pub play_at: Instant,         // Computed local playback time
    pub samples: Arc<[Sample]>,   // Immutable, shareable sample data
    pub format: AudioFormat,
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: All audio type tests pass

**Step 6: Commit**

```bash
git add src/audio tests/audio_types.rs
git commit -m "feat: add core audio types (Sample, Codec, AudioFormat)"
```

---

## Task 3: Protocol Message Types

**Files:**
- Create: `src/protocol/mod.rs`
- Create: `src/protocol/messages.rs`
- Create: `tests/protocol_messages.rs`

**Step 1: Write test for ClientHello serialization**

Create `tests/protocol_messages.rs`:
```rust
use resonate::protocol::messages::{ClientHello, ServerHello, Message, DeviceInfo, PlayerSupport, AudioFormatSpec};
use serde_json;

#[test]
fn test_client_hello_serialization() {
    let hello = ClientHello {
        client_id: "test-client-123".to_string(),
        name: "Test Player".to_string(),
        version: 1,
        supported_roles: vec!["player".to_string()],
        device_info: DeviceInfo {
            product_name: "Resonate-RS Player".to_string(),
            manufacturer: "Resonate".to_string(),
            software_version: "0.1.0".to_string(),
        },
        player_support: Some(PlayerSupport {
            support_formats: vec![
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 48000,
                    bit_depth: 24,
                }
            ],
            buffer_capacity: 100,
            supported_commands: vec!["play".to_string(), "pause".to_string()],
        }),
        metadata_support: None,
    };

    let message = Message::ClientHello(hello);
    let json = serde_json::to_string(&message).unwrap();

    assert!(json.contains("\"type\":\"client/hello\""));
    assert!(json.contains("\"client_id\":\"test-client-123\""));
}

#[test]
fn test_server_hello_deserialization() {
    let json = r#"{
        "type": "server/hello",
        "payload": {
            "server_id": "server-456",
            "name": "Test Server",
            "version": 1
        }
    }"#;

    let message: Message = serde_json::from_str(json).unwrap();

    match message {
        Message::ServerHello(hello) => {
            assert_eq!(hello.server_id, "server-456");
            assert_eq!(hello.name, "Test Server");
            assert_eq!(hello.version, 1);
        }
        _ => panic!("Expected ServerHello"),
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_client_hello_serialization`
Expected: FAIL - module doesn't exist

**Step 3: Create protocol module structure**

Create `src/protocol/mod.rs`:
```rust
// ABOUTME: Protocol implementation for Resonate WebSocket protocol
// ABOUTME: Message types, serialization, and WebSocket client

pub mod messages;
pub mod client;

pub use messages::Message;
```

**Step 4: Implement protocol message types**

Create `src/protocol/messages.rs`:
```rust
// ABOUTME: Protocol message type definitions and serialization
// ABOUTME: Supports client/hello, server/hello, stream/start, etc.

use serde::{Deserialize, Serialize};

/// Top-level protocol message envelope
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Message {
    #[serde(rename = "client/hello")]
    ClientHello(ClientHello),

    #[serde(rename = "server/hello")]
    ServerHello(ServerHello),

    #[serde(rename = "client/time")]
    ClientTime(ClientTime),

    #[serde(rename = "server/time")]
    ServerTime(ServerTime),

    #[serde(rename = "stream/start")]
    StreamStart(StreamStart),

    #[serde(rename = "server/command")]
    ServerCommand(ServerCommand),

    #[serde(rename = "player/update")]
    PlayerUpdate(PlayerUpdate),
}

/// Client hello message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub client_id: String,
    pub name: String,
    pub version: u32,
    pub supported_roles: Vec<String>,
    pub device_info: DeviceInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub player_support: Option<PlayerSupport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_support: Option<MetadataSupport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub product_name: String,
    pub manufacturer: String,
    pub software_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSupport {
    pub support_formats: Vec<AudioFormatSpec>,
    pub buffer_capacity: u32,
    pub supported_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioFormatSpec {
    pub codec: String,
    pub channels: u8,
    pub sample_rate: u32,
    pub bit_depth: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataSupport {
    pub support_picture_formats: Vec<String>,
    pub media_width: u32,
    pub media_height: u32,
}

/// Server hello message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub server_id: String,
    pub name: String,
    pub version: u32,
}

/// Client time sync message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientTime {
    pub client_transmitted: i64,
}

/// Server time sync response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerTime {
    pub client_transmitted: i64,
    pub server_received: i64,
    pub server_transmitted: i64,
}

/// Stream start message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamStart {
    pub player: StreamPlayerConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamPlayerConfig {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub bit_depth: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codec_header: Option<String>,
}

/// Server command message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCommand {
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
}

/// Player state update message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerUpdate {
    pub state: String,
    pub volume: u8,
    pub muted: bool,
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: All protocol message tests pass

**Step 6: Commit**

```bash
git add src/protocol tests/protocol_messages.rs
git commit -m "feat: add protocol message types with serde serialization"
```

---

## Task 4: Clock Synchronization

**Files:**
- Create: `src/sync/mod.rs`
- Create: `src/sync/clock.rs`
- Create: `tests/clock_sync.rs`

**Step 1: Write test for clock sync calculation**

Create `tests/clock_sync.rs`:
```rust
use resonate::sync::ClockSync;
use std::time::{Duration, Instant};

#[test]
fn test_clock_sync_rtt_calculation() {
    let mut sync = ClockSync::new();

    // Simulate sync: client sends at 1000µs, server receives at 500µs (server loop time)
    let t1 = 1_000_000; // Client transmitted (Unix µs)
    let t2 = 500_000;   // Server received (server loop µs)
    let t3 = 500_010;   // Server transmitted (server loop µs)
    let t4 = 1_000_050; // Client received (Unix µs)

    sync.update(t1, t2, t3, t4);

    // RTT = (t4 - t1) - (t3 - t2) = 50 - 10 = 40µs
    assert_eq!(sync.rtt_micros(), Some(40));
}

#[test]
fn test_server_to_local_conversion() {
    let mut sync = ClockSync::new();

    let t1 = 1_000_000;
    let t2 = 500_000;
    let t3 = 500_010;
    let t4 = 1_000_050;

    sync.update(t1, t2, t3, t4);

    // Server loop start = t4 - t3 = 1_000_050 - 500_010 = 500_040 Unix µs
    // Converting server time 520_000 should give us ~520_040 Unix µs
    let local = sync.server_to_local_instant(520_000);
    assert!(local.is_some());
}

#[test]
fn test_sync_quality() {
    let mut sync = ClockSync::new();

    // Good RTT (30µs)
    sync.update(1_000_000, 500_000, 500_010, 1_000_040);
    assert_eq!(sync.quality(), resonate::sync::SyncQuality::Good);

    // Degraded RTT (75µs)
    sync.update(2_000_000, 600_000, 600_010, 2_000_085);
    assert_eq!(sync.quality(), resonate::sync::SyncQuality::Degraded);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_clock_sync_rtt_calculation`
Expected: FAIL - module doesn't exist

**Step 3: Create sync module structure**

Create `src/sync/mod.rs`:
```rust
// ABOUTME: Clock synchronization for Resonate protocol
// ABOUTME: NTP-style round-trip time calculation and server timestamp conversion

pub mod clock;

pub use clock::{ClockSync, SyncQuality};
```

**Step 4: Implement clock synchronization**

Create `src/sync/clock.rs`:
```rust
// ABOUTME: Clock synchronization implementation
// ABOUTME: Calculates RTT and converts server loop time to local Instant

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Clock synchronization quality
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncQuality {
    Good,      // RTT < 50ms
    Degraded,  // RTT 50-100ms
    Lost,      // RTT > 100ms or no sync
}

/// Clock synchronization state
#[derive(Debug)]
pub struct ClockSync {
    /// Last known RTT in microseconds
    rtt_micros: Option<i64>,

    /// When server loop started in Unix time (microseconds)
    server_loop_start_unix: Option<i64>,

    /// When we computed this (for staleness detection)
    last_update: Option<Instant>,
}

impl ClockSync {
    pub fn new() -> Self {
        Self {
            rtt_micros: None,
            server_loop_start_unix: None,
            last_update: None,
        }
    }

    /// Update clock sync with new measurement
    /// t1 = client_transmitted (Unix µs)
    /// t2 = server_received (server loop µs)
    /// t3 = server_transmitted (server loop µs)
    /// t4 = client_received (Unix µs)
    pub fn update(&mut self, t1: i64, t2: i64, t3: i64, t4: i64) {
        // RTT = (t4 - t1) - (t3 - t2)
        self.rtt_micros = Some((t4 - t1) - (t3 - t2));

        // Server loop start = t4 - t3
        self.server_loop_start_unix = Some(t4 - t3);

        self.last_update = Some(Instant::now());
    }

    /// Get current RTT in microseconds
    pub fn rtt_micros(&self) -> Option<i64> {
        self.rtt_micros
    }

    /// Convert server loop microseconds to local Instant
    pub fn server_to_local_instant(&self, server_micros: i64) -> Option<Instant> {
        let server_start = self.server_loop_start_unix?;

        // Convert to Unix microseconds
        let unix_micros = server_start + server_micros;

        // Convert to Instant
        let now_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_micros() as i64;

        let now_instant = Instant::now();

        let delta_micros = unix_micros - now_unix;

        if delta_micros >= 0 {
            Some(now_instant + Duration::from_micros(delta_micros as u64))
        } else {
            now_instant.checked_sub(Duration::from_micros((-delta_micros) as u64))
        }
    }

    /// Get sync quality based on RTT
    pub fn quality(&self) -> SyncQuality {
        match self.rtt_micros {
            Some(rtt) if rtt < 50_000 => SyncQuality::Good,
            Some(rtt) if rtt < 100_000 => SyncQuality::Degraded,
            _ => SyncQuality::Lost,
        }
    }

    /// Check if sync is stale (>5 seconds old)
    pub fn is_stale(&self) -> bool {
        match self.last_update {
            Some(last) => last.elapsed() > Duration::from_secs(5),
            None => true,
        }
    }
}

impl Default for ClockSync {
    fn default() -> Self {
        Self::new()
    }
}
```

**Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: All clock sync tests pass

**Step 6: Commit**

```bash
git add src/sync tests/clock_sync.rs
git commit -m "feat: add clock synchronization with RTT calculation"
```

---

## Task 5: WebSocket Client (Basic - No Audio Yet)

**Files:**
- Create: `src/protocol/client.rs`
- Create: `examples/basic_client.rs`

**Step 1: Write basic client connection example**

Create `examples/basic_client.rs`:
```rust
// ABOUTME: Basic example demonstrating WebSocket connection and handshake
// ABOUTME: Connects to server, sends client/hello, receives server/hello

use resonate::protocol::messages::{ClientHello, DeviceInfo, PlayerSupport, AudioFormatSpec, Message};
use resonate::protocol::client::ProtocolClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let hello = ClientHello {
        client_id: uuid::Uuid::new_v4().to_string(),
        name: "Resonate-RS Basic Client".to_string(),
        version: 1,
        supported_roles: vec!["player".to_string()],
        device_info: DeviceInfo {
            product_name: "Resonate-RS Player".to_string(),
            manufacturer: "Resonate".to_string(),
            software_version: "0.1.0".to_string(),
        },
        player_support: Some(PlayerSupport {
            support_formats: vec![
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 48000,
                    bit_depth: 24,
                }
            ],
            buffer_capacity: 100,
            supported_commands: vec!["play".to_string(), "pause".to_string()],
        }),
        metadata_support: None,
    };

    println!("Connecting to ws://localhost:8080/resonate...");

    let client = ProtocolClient::connect("ws://localhost:8080/resonate", hello).await?;

    println!("Connected! Waiting for server hello...");

    // This would block waiting for messages
    // For now, just demonstrate connection works

    Ok(())
}
```

**Step 2: Implement basic WebSocket client**

Update `src/protocol/client.rs`:
```rust
// ABOUTME: WebSocket client implementation for Resonate protocol
// ABOUTME: Handles connection, message routing, and protocol state machine

use crate::error::Error;
use crate::protocol::messages::{Message, ClientHello, ServerHello};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use futures_util::{SinkExt, StreamExt};

pub struct ProtocolClient {
    // Placeholder for now - will be fleshed out in Phase 2
}

impl ProtocolClient {
    /// Connect to Resonate server
    pub async fn connect(url: &str, hello: ClientHello) -> Result<Self, Error> {
        // Connect WebSocket
        let (ws_stream, _) = connect_async(url)
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;

        let (mut write, mut read) = ws_stream.split();

        // Send client hello
        let hello_msg = Message::ClientHello(hello);
        let hello_json = serde_json::to_string(&hello_msg)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        write.send(WsMessage::Text(hello_json))
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        // Wait for server hello
        if let Some(Ok(WsMessage::Text(text))) = read.next().await {
            let msg: Message = serde_json::from_str(&text)
                .map_err(|e| Error::Protocol(e.to_string()))?;

            match msg {
                Message::ServerHello(server_hello) => {
                    log::info!("Connected to server: {} ({})",
                        server_hello.name, server_hello.server_id);
                }
                _ => return Err(Error::Protocol("Expected server/hello".to_string())),
            }
        } else {
            return Err(Error::Connection("No server hello received".to_string()));
        }

        Ok(Self {})
    }
}
```

**Step 3: Update lib.rs to export client**

Update `src/lib.rs`:
```rust
// ABOUTME: Main library entry point for resonate-rs
// ABOUTME: Exports public API for Resonate Protocol client and server

#![warn(missing_docs)]

pub mod audio;
pub mod protocol;
pub mod sync;

pub use protocol::client::ProtocolClient;
pub use protocol::messages::{ClientHello, ServerHello};

/// Result type for resonate operations
pub type Result<T> = std::result::Result<T, error::Error>;

/// Error types for resonate
pub mod error {
    use thiserror::Error;

    #[derive(Error, Debug)]
    pub enum Error {
        #[error("WebSocket error: {0}")]
        WebSocket(String),

        #[error("Protocol error: {0}")]
        Protocol(String),

        #[error("Invalid message format")]
        InvalidMessage,

        #[error("Connection error: {0}")]
        Connection(String),
    }
}
```

**Step 4: Verify example compiles**

Run: `cargo build --examples`
Expected: Compiles successfully

**Step 5: Commit**

```bash
git add src/protocol/client.rs examples/basic_client.rs src/lib.rs
git commit -m "feat: add basic WebSocket client with handshake"
```

---

## Task 6: PCM Decoder

**Files:**
- Create: `src/audio/decode/mod.rs`
- Create: `src/audio/decode/pcm.rs`
- Create: `tests/pcm_decoder.rs`

**Step 1: Write test for PCM decoding**

Create `tests/pcm_decoder.rs`:
```rust
use resonate::audio::{Sample, decode::PcmDecoder};

#[test]
fn test_decode_pcm_16bit() {
    let decoder = PcmDecoder::new(16);

    // 4 samples (8 bytes) of 16-bit PCM
    let data = vec![
        0x00, 0x04, // 1024 in little-endian
        0x00, 0x08, // 2048
        0xFF, 0xFF, // -1
        0x00, 0x00, // 0
    ];

    let samples = decoder.decode(&data).unwrap();

    assert_eq!(samples.len(), 4);
    assert_eq!(samples[0].to_i16(), 1024);
    assert_eq!(samples[1].to_i16(), 2048);
    assert_eq!(samples[2].to_i16(), -1);
    assert_eq!(samples[3].to_i16(), 0);
}

#[test]
fn test_decode_pcm_24bit() {
    let decoder = PcmDecoder::new(24);

    // 2 samples (6 bytes) of 24-bit PCM
    let data = vec![
        0x00, 0x10, 0x00, // 4096 in little-endian 24-bit
        0xFF, 0xFF, 0xFF, // -1 in 24-bit
    ];

    let samples = decoder.decode(&data).unwrap();

    assert_eq!(samples.len(), 2);
    assert_eq!(samples[0].0, 4096);
    assert_eq!(samples[1].0, -1);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_decode_pcm_16bit`
Expected: FAIL - module doesn't exist

**Step 3: Create decode module structure**

Create `src/audio/decode/mod.rs`:
```rust
// ABOUTME: Audio decoder implementations
// ABOUTME: PCM, Opus, FLAC decoders (Phase 1: PCM only)

pub mod pcm;

pub use pcm::PcmDecoder;

use crate::audio::Sample;
use crate::error::Error;
use std::sync::Arc;

/// Decoder trait for audio codecs
pub trait Decoder {
    fn decode(&self, data: &[u8]) -> Result<Arc<[Sample]>, Error>;
}
```

**Step 4: Implement PCM decoder**

Create `src/audio/decode/pcm.rs`:
```rust
// ABOUTME: PCM decoder implementation
// ABOUTME: Supports 16-bit and 24-bit PCM decoding with zero-copy where possible

use crate::audio::Sample;
use crate::audio::decode::Decoder;
use crate::error::Error;
use std::sync::Arc;

pub struct PcmDecoder {
    bit_depth: u8,
}

impl PcmDecoder {
    pub fn new(bit_depth: u8) -> Self {
        Self { bit_depth }
    }
}

impl Decoder for PcmDecoder {
    fn decode(&self, data: &[u8]) -> Result<Arc<[Sample]>, Error> {
        match self.bit_depth {
            16 => {
                // Convert 16-bit PCM to Sample
                let samples: Vec<Sample> = data
                    .chunks_exact(2)
                    .map(|c| {
                        let i16_val = i16::from_le_bytes([c[0], c[1]]);
                        Sample::from_i16(i16_val)
                    })
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            24 => {
                // Convert 24-bit PCM to Sample
                let samples: Vec<Sample> = data
                    .chunks_exact(3)
                    .map(|c| Sample::from_i24_le([c[0], c[1], c[2]]))
                    .collect();
                Ok(Arc::from(samples.into_boxed_slice()))
            }
            _ => Err(Error::Protocol(format!("Unsupported bit depth: {}", self.bit_depth))),
        }
    }
}
```

**Step 5: Update audio/mod.rs to export decode**

Update `src/audio/mod.rs`:
```rust
// ABOUTME: Audio types and processing for resonate-rs
// ABOUTME: Contains Sample type, AudioFormat, Buffer, and codec definitions

pub mod types;
pub mod decode;

pub use types::{Sample, Codec, AudioFormat, AudioBuffer};
```

**Step 6: Run tests to verify they pass**

Run: `cargo test`
Expected: All PCM decoder tests pass

**Step 7: Commit**

```bash
git add src/audio/decode tests/pcm_decoder.rs src/audio/mod.rs
git commit -m "feat: add PCM decoder for 16-bit and 24-bit audio"
```

---

## Task 7: README and Documentation

**Files:**
- Create: `README.md`

**Step 1: Write comprehensive README**

Create `README.md`:
```markdown
# resonate-rs

Hyper-efficient Rust implementation of the [Resonate Protocol](https://github.com/resonate-protocol/spec) for synchronized multi-room audio streaming.

## Features

- **Zero-copy audio pipeline** - Minimal allocations, maximum performance
- **Lock-free concurrency** - No contention on audio thread
- **Async I/O** - Efficient WebSocket handling with Tokio
- **Type-safe protocol** - Leverage Rust's type system for correctness

## Performance Targets

- Audio latency: <10ms end-to-end jitter
- CPU usage: <2% on modern hardware (4-core)
- Memory: <20MB stable
- Thread synchronization: Lock-free audio pipeline

## Current Status

**Phase 1: Foundation** ✅ (Complete)
- Core audio types (Sample, AudioFormat, AudioBuffer)
- Protocol message types with serde serialization
- WebSocket client with handshake
- PCM decoder (16-bit and 24-bit)
- Clock synchronization (NTP-style)

**Phase 2: Audio Pipeline** 🚧 (Next)
- Audio output (cpal integration)
- Lock-free scheduler
- End-to-end player

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
resonate = "0.1"
```

### Basic Client Example

```rust
use resonate::protocol::messages::{ClientHello, DeviceInfo};
use resonate::protocol::client::ProtocolClient;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hello = ClientHello {
        client_id: uuid::Uuid::new_v4().to_string(),
        name: "My Player".to_string(),
        version: 1,
        // ... configure device info and capabilities
    };

    let client = ProtocolClient::connect("ws://localhost:8080/resonate", hello).await?;

    // Client is now connected and ready to receive audio

    Ok(())
}
```

See `examples/` directory for more examples.

## Architecture

See [docs/rust-thoughts.md](docs/rust-thoughts.md) for detailed architecture and implementation notes.

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Run examples
cargo run --example basic_client

# Build with optimizations
cargo build --release
```

## Testing

```bash
# Run all tests
cargo test

# Run specific test
cargo test test_sample_from_i16

# Run with logging
RUST_LOG=debug cargo test
```

## License

MIT OR Apache-2.0

## Contributing

Contributions welcome! Please open an issue or PR.
```

**Step 2: Verify documentation builds**

Run: `cargo doc --no-deps --open`
Expected: Documentation builds and opens in browser

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add comprehensive README with quick start guide"
```

---

## Task 8: Phase 1 Verification

**Files:**
- All existing tests

**Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass

**Step 2: Check for compiler warnings**

Run: `cargo clippy -- -D warnings`
Expected: No warnings

**Step 3: Format code**

Run: `cargo fmt`
Expected: Code is formatted

**Step 4: Build in release mode**

Run: `cargo build --release`
Expected: Builds successfully with optimizations

**Step 5: Verify examples compile**

Run: `cargo build --examples`
Expected: All examples compile

**Step 6: Final commit**

```bash
git add .
git commit -m "chore: phase 1 foundation complete and verified"
```

---

## Phase 1 Acceptance Criteria

✅ Core types (Sample, AudioFormat, AudioBuffer) implemented and tested
✅ Protocol message types with serde serialization working
✅ WebSocket client can connect and complete handshake
✅ PCM decoder handles 16-bit and 24-bit audio
✅ Clock synchronization calculates RTT and converts timestamps
✅ All unit tests pass
✅ Code compiles with no warnings
✅ Documentation complete

## Next Steps

**Phase 2: Audio Pipeline** will add:
- Audio output trait + cpal implementation
- Lock-free scheduler for chunk playback
- Buffer pool for memory efficiency
- End-to-end player example

See `docs/rust-thoughts.md` for Phase 2 detailed specification.
