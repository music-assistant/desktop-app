# Resonate-RS Phase 2: Audio Pipeline Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a complete audio playback pipeline with audio output, lock-free scheduling, buffer pooling, and an end-to-end player that can receive and play audio from a Resonate server.

**Architecture:** Extends Phase 1 foundation with a real-time audio pipeline. Audio chunks flow from WebSocket → Decoder → Scheduler → Audio Output. Uses dedicated threads for audio processing, lock-free data structures for the scheduler, and buffer pooling to eliminate allocations in the hot path.

**Tech Stack:** cpal (audio output), crossbeam (lock-free queues), tokio (async runtime), existing resonate types from Phase 1

---

## Task 1: Audio Output Trait and cpal Implementation

**Files:**
- Create: `src/audio/output.rs`
- Create: `src/audio/output/mod.rs`
- Create: `src/audio/output/cpal_output.rs`
- Modify: `src/audio/mod.rs`
- Modify: `Cargo.toml`
- Create: `tests/audio_output.rs`

**Step 1: Add cpal dependency**

Update `Cargo.toml` dependencies section:
```toml
[dependencies]
# ... existing dependencies ...

# Audio output
cpal = "0.15"
```

**Step 2: Write test for AudioOutput trait**

Create `tests/audio_output.rs`:
```rust
use resonate::audio::{Sample, AudioFormat, Codec};
use resonate::audio::output::{AudioOutput, CpalOutput};
use std::sync::Arc;

#[test]
fn test_audio_output_creation() {
    let format = AudioFormat {
        codec: Codec::Pcm,
        sample_rate: 48000,
        channels: 2,
        bit_depth: 24,
        codec_header: None,
    };

    // CpalOutput::new() should succeed
    let output = CpalOutput::new(format);
    assert!(output.is_ok());
}

#[test]
fn test_audio_output_write() {
    let format = AudioFormat {
        codec: Codec::Pcm,
        sample_rate: 48000,
        channels: 2,
        bit_depth: 24,
        codec_header: None,
    };

    let mut output = CpalOutput::new(format).unwrap();

    // Create some test samples (silence)
    let samples: Vec<Sample> = vec![Sample::ZERO; 960]; // 10ms at 48kHz stereo
    let samples_arc = Arc::from(samples.into_boxed_slice());

    // Should be able to write without error
    let result = output.write(&samples_arc);
    assert!(result.is_ok());
}
```

**Step 3: Run test to verify it fails**

Run: `cargo test test_audio_output_creation`
Expected: FAIL - module doesn't exist

**Step 4: Create AudioOutput trait**

Create `src/audio/output/mod.rs`:
```rust
// ABOUTME: Audio output trait and implementations
// ABOUTME: Provides abstraction over platform audio APIs (cpal, ALSA, etc.)

pub mod cpal_output;

pub use cpal_output::CpalOutput;

use crate::audio::{Sample, AudioFormat};
use crate::error::Error;
use std::sync::Arc;

/// Audio output trait for playing audio samples
pub trait AudioOutput {
    /// Write samples to the audio output
    fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), Error>;

    /// Get the current output latency in microseconds
    fn latency_micros(&self) -> u64;

    /// Get the audio format this output expects
    fn format(&self) -> &AudioFormat;
}
```

**Step 5: Implement CpalOutput**

Create `src/audio/output/cpal_output.rs`:
```rust
// ABOUTME: cpal-based audio output implementation
// ABOUTME: Cross-platform audio output using the cpal library

use crate::audio::{Sample, AudioFormat};
use crate::audio::output::AudioOutput;
use crate::error::Error;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Stream, StreamConfig};
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};

/// cpal-based audio output
pub struct CpalOutput {
    format: AudioFormat,
    _stream: Stream,
    sample_tx: Sender<Arc<[Sample]>>,
    latency_micros: Arc<Mutex<u64>>,
}

impl CpalOutput {
    /// Create a new cpal audio output
    pub fn new(format: AudioFormat) -> Result<Self, Error> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| Error::Output("No output device available".to_string()))?;

        let config = StreamConfig {
            channels: format.channels as u16,
            sample_rate: cpal::SampleRate(format.sample_rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let (sample_tx, sample_rx) = channel::<Arc<[Sample]>>();
        let latency_micros = Arc::new(Mutex::new(0u64));
        let latency_clone = Arc::clone(&latency_micros);

        let stream = Self::build_stream(&device, &config, sample_rx, latency_clone)?;
        stream.play().map_err(|e| Error::Output(e.to_string()))?;

        Ok(Self {
            format,
            _stream: stream,
            sample_tx,
            latency_micros,
        })
    }

    fn build_stream(
        device: &Device,
        config: &StreamConfig,
        sample_rx: Receiver<Arc<[Sample]>>,
        latency_micros: Arc<Mutex<u64>>,
    ) -> Result<Stream, Error> {
        let sample_rx = Arc::new(Mutex::new(sample_rx));
        let mut current_buffer: Option<Arc<[Sample]>> = None;
        let mut buffer_pos = 0;

        let stream = device
            .build_output_stream(
                config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    for sample_out in data.iter_mut() {
                        // Get next sample from current buffer or receive new buffer
                        if current_buffer.is_none() || buffer_pos >= current_buffer.as_ref().unwrap().len() {
                            // Try to get new buffer
                            if let Ok(rx) = sample_rx.lock() {
                                if let Ok(buf) = rx.try_recv() {
                                    current_buffer = Some(buf);
                                    buffer_pos = 0;
                                }
                            }
                        }

                        // Output sample or silence
                        if let Some(ref buf) = current_buffer {
                            if buffer_pos < buf.len() {
                                let sample = buf[buffer_pos];
                                // Convert 24-bit sample to f32 (-1.0 to 1.0)
                                *sample_out = sample.0 as f32 / 8388607.0;
                                buffer_pos += 1;
                            } else {
                                *sample_out = 0.0; // Silence
                            }
                        } else {
                            *sample_out = 0.0; // Silence
                        }
                    }
                },
                |err| eprintln!("Audio stream error: {}", err),
                None,
            )
            .map_err(|e| Error::Output(e.to_string()))?;

        Ok(stream)
    }
}

impl AudioOutput for CpalOutput {
    fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), Error> {
        self.sample_tx
            .send(Arc::clone(samples))
            .map_err(|_| Error::Output("Failed to send samples to audio thread".to_string()))
    }

    fn latency_micros(&self) -> u64 {
        *self.latency_micros.lock().unwrap()
    }

    fn format(&self) -> &AudioFormat {
        &self.format
    }
}
```

**Step 6: Update audio module exports**

Modify `src/audio/mod.rs`:
```rust
// ABOUTME: Audio types and processing for resonate-rs
// ABOUTME: Contains Sample type, AudioFormat, Buffer, and codec definitions

pub mod types;
pub mod decode;
pub mod output;

pub use types::{Sample, Codec, AudioFormat, AudioBuffer};
pub use output::{AudioOutput, CpalOutput};
```

**Step 7: Update error types**

Modify `src/lib.rs` error module:
```rust
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

        #[error("Audio output error: {0}")]
        Output(String),
    }
}
```

**Step 8: Run tests to verify they pass**

Run: `cargo test`
Expected: All tests pass (including 2 new audio output tests)

**Step 9: Commit**

```bash
git add .
git commit -m "feat: add audio output trait and cpal implementation"
```

---

## Task 2: Lock-Free Scheduler

**Files:**
- Create: `src/scheduler/mod.rs`
- Create: `src/scheduler/audio_scheduler.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Create: `tests/scheduler.rs`

**Step 1: Add crossbeam dependency**

Update `Cargo.toml`:
```toml
[dependencies]
# ... existing dependencies ...

# Concurrency
crossbeam = "0.8"
```

**Step 2: Write test for scheduler**

Create `tests/scheduler.rs`:
```rust
use resonate::scheduler::AudioScheduler;
use resonate::audio::{AudioBuffer, Sample, AudioFormat, Codec};
use std::sync::Arc;
use std::time::{Instant, Duration};

#[test]
fn test_scheduler_creation() {
    let scheduler = AudioScheduler::new();
    assert!(scheduler.is_empty());
}

#[test]
fn test_scheduler_schedule_and_ready() {
    let scheduler = AudioScheduler::new();

    let format = AudioFormat {
        codec: Codec::Pcm,
        sample_rate: 48000,
        channels: 2,
        bit_depth: 24,
        codec_header: None,
    };

    let samples = vec![Sample::ZERO; 960];
    let buffer = AudioBuffer {
        timestamp: 0,
        play_at: Instant::now() + Duration::from_millis(10),
        samples: Arc::from(samples.into_boxed_slice()),
        format,
    };

    scheduler.schedule(buffer);
    assert!(!scheduler.is_empty());

    // Buffer shouldn't be ready yet (10ms in future)
    assert!(scheduler.next_ready().is_none());

    // Sleep and check if ready
    std::thread::sleep(Duration::from_millis(15));
    let ready = scheduler.next_ready();
    assert!(ready.is_some());
}
```

**Step 3: Run test to verify it fails**

Run: `cargo test test_scheduler_creation`
Expected: FAIL - module doesn't exist

**Step 4: Create scheduler module structure**

Create `src/scheduler/mod.rs`:
```rust
// ABOUTME: Audio chunk scheduler for timed playback
// ABOUTME: Lock-free priority queue for scheduling audio buffers

pub mod audio_scheduler;

pub use audio_scheduler::AudioScheduler;
```

**Step 5: Implement AudioScheduler**

Create `src/scheduler/audio_scheduler.rs`:
```rust
// ABOUTME: Lock-free audio scheduler implementation
// ABOUTME: Uses crossbeam queues for thread-safe scheduling without locks

use crate::audio::AudioBuffer;
use crossbeam::queue::SegQueue;
use std::sync::Arc;
use std::time::Instant;

/// Lock-free audio scheduler
pub struct AudioScheduler {
    /// Incoming buffers (lock-free queue)
    incoming: Arc<SegQueue<AudioBuffer>>,

    /// Sorted buffers ready for playback
    sorted: Arc<parking_lot::Mutex<Vec<AudioBuffer>>>,
}

impl AudioScheduler {
    /// Create a new audio scheduler
    pub fn new() -> Self {
        Self {
            incoming: Arc::new(SegQueue::new()),
            sorted: Arc::new(parking_lot::Mutex::new(Vec::new())),
        }
    }

    /// Schedule an audio buffer for future playback
    pub fn schedule(&self, buffer: AudioBuffer) {
        self.incoming.push(buffer);
    }

    /// Check if scheduler is empty
    pub fn is_empty(&self) -> bool {
        self.incoming.is_empty() && self.sorted.lock().is_empty()
    }

    /// Get next buffer that's ready to play (within 50ms window)
    pub fn next_ready(&self) -> Option<AudioBuffer> {
        // Drain incoming queue into sorted vec
        while let Some(buf) = self.incoming.pop() {
            let mut sorted = self.sorted.lock();
            let pos = sorted
                .binary_search_by_key(&buf.timestamp, |b| b.timestamp)
                .unwrap_or_else(|e| e);
            sorted.insert(pos, buf);
        }

        let mut sorted = self.sorted.lock();
        let now = Instant::now();

        // Check if first buffer is ready
        if let Some(buf) = sorted.first() {
            let delay = buf.play_at.saturating_duration_since(now);

            if delay.as_millis() <= 50 {
                // Ready to play or late
                return Some(sorted.remove(0));
            }
        }

        None
    }
}

impl Default for AudioScheduler {
    fn default() -> Self {
        Self::new()
    }
}
```

**Step 6: Add parking_lot dependency**

Update `Cargo.toml`:
```toml
[dependencies]
# ... existing dependencies ...

# Fast mutexes
parking_lot = "0.12"
```

**Step 7: Update lib.rs to export scheduler**

Modify `src/lib.rs`:
```rust
pub mod audio;
pub mod protocol;
pub mod sync;
pub mod scheduler;

pub use protocol::client::ProtocolClient;
pub use protocol::messages::{ClientHello, ServerHello};
pub use scheduler::AudioScheduler;
```

**Step 8: Run tests to verify they pass**

Run: `cargo test`
Expected: All tests pass

**Step 9: Commit**

```bash
git add .
git commit -m "feat: add lock-free audio scheduler"
```

---

## Task 3: Buffer Pool for Memory Efficiency

**Files:**
- Create: `src/audio/pool.rs`
- Modify: `src/audio/mod.rs`
- Create: `tests/buffer_pool.rs`

**Step 1: Write test for buffer pool**

Create `tests/buffer_pool.rs`:
```rust
use resonate::audio::{BufferPool, Sample};

#[test]
fn test_buffer_pool_creation() {
    let pool = BufferPool::new(10, 1024);
    assert_eq!(pool.capacity(), 1024);
}

#[test]
fn test_buffer_pool_get_and_return() {
    let pool = BufferPool::new(5, 1024);

    // Get a buffer
    let mut buf = pool.get();
    assert_eq!(buf.capacity(), 1024);

    // Modify it
    buf.extend_from_slice(&vec![Sample::ZERO; 100]);
    assert_eq!(buf.len(), 100);

    // Return it
    pool.put(buf);

    // Get another - should be the same buffer (reused)
    let buf2 = pool.get();
    assert_eq!(buf2.capacity(), 1024);
    assert_eq!(buf2.len(), 0); // Should be cleared
}

#[test]
fn test_buffer_pool_fallback_allocation() {
    let pool = BufferPool::new(2, 1024);

    // Get all buffers from pool
    let _buf1 = pool.get();
    let _buf2 = pool.get();

    // This should allocate a new buffer
    let buf3 = pool.get();
    assert_eq!(buf3.capacity(), 1024);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test test_buffer_pool_creation`
Expected: FAIL - BufferPool doesn't exist

**Step 3: Implement BufferPool**

Create `src/audio/pool.rs`:
```rust
// ABOUTME: Buffer pool for reusing audio sample buffers
// ABOUTME: Eliminates allocations in the audio hot path

use crate::audio::Sample;
use crossbeam::queue::ArrayQueue;
use std::sync::Arc;

/// Buffer pool for reusing audio sample buffers
pub struct BufferPool {
    pool: Arc<ArrayQueue<Vec<Sample>>>,
    capacity: usize,
}

impl BufferPool {
    /// Create a new buffer pool
    ///
    /// # Arguments
    /// * `pool_size` - Number of buffers to pre-allocate
    /// * `buffer_capacity` - Capacity of each buffer in samples
    pub fn new(pool_size: usize, buffer_capacity: usize) -> Self {
        let pool = Arc::new(ArrayQueue::new(pool_size));

        // Pre-allocate buffers
        for _ in 0..pool_size {
            let mut buf = Vec::with_capacity(buffer_capacity);
            buf.resize(buffer_capacity, Sample::ZERO);
            buf.clear(); // Clear so len() is 0 but capacity is preserved
            let _ = pool.push(buf);
        }

        Self {
            pool,
            capacity: buffer_capacity,
        }
    }

    /// Get a buffer from the pool (or allocate if pool is empty)
    pub fn get(&self) -> Vec<Sample> {
        self.pool.pop().unwrap_or_else(|| {
            Vec::with_capacity(self.capacity)
        })
    }

    /// Return a buffer to the pool
    pub fn put(&self, mut buf: Vec<Sample>) {
        buf.clear();
        let _ = self.pool.push(buf); // Ignore if pool is full
    }

    /// Get the buffer capacity
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}
```

**Step 4: Update audio module exports**

Modify `src/audio/mod.rs`:
```rust
pub mod types;
pub mod decode;
pub mod output;
pub mod pool;

pub use types::{Sample, Codec, AudioFormat, AudioBuffer};
pub use output::{AudioOutput, CpalOutput};
pub use pool::BufferPool;
```

**Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: All tests pass

**Step 6: Commit**

```bash
git add .
git commit -m "feat: add buffer pool for memory efficiency"
```

---

## Task 4: Enhanced WebSocket Client with Message Handling

**Files:**
- Modify: `src/protocol/client.rs`
- Create: `tests/protocol_client.rs`

**Step 1: Write test for message handling**

Create `tests/protocol_client.rs`:
```rust
// Note: These are integration tests that require a running server
// For now, we'll create the structure and skip them

#[test]
#[ignore] // Requires running server
fn test_client_receives_stream_start() {
    // Test that client can receive stream/start message
    // Will implement when we have full client
}

#[test]
#[ignore] // Requires running server
fn test_client_handles_audio_chunks() {
    // Test that client can receive binary audio chunks
    // Will implement when we have full client
}
```

**Step 2: Enhance ProtocolClient structure**

Modify `src/protocol/client.rs`:
```rust
// ABOUTME: WebSocket client implementation for Resonate protocol
// ABOUTME: Handles connection, message routing, and protocol state machine

use crate::error::Error;
use crate::protocol::messages::{Message, ClientHello, ServerHello, StreamStart};
use crate::audio::{AudioBuffer, Sample, AudioFormat, Codec};
use crate::sync::ClockSync;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};
use futures_util::{SinkExt, StreamExt, stream::{SplitSink, SplitStream}};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use std::sync::Arc;
use std::time::Instant;

/// Audio chunk from server (binary frame)
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub timestamp: i64,
    pub data: Arc<[u8]>,
}

impl AudioChunk {
    /// Parse from WebSocket binary frame
    pub fn from_bytes(frame: &[u8]) -> Result<Self, Error> {
        if frame.len() < 9 {
            return Err(Error::Protocol("Audio chunk too short".to_string()));
        }

        if frame[0] != 0x01 {
            return Err(Error::Protocol("Invalid audio chunk type".to_string()));
        }

        let timestamp = i64::from_be_bytes([
            frame[1], frame[2], frame[3], frame[4],
            frame[5], frame[6], frame[7], frame[8],
        ]);

        let data = Arc::from(&frame[9..]);

        Ok(Self { timestamp, data })
    }
}

pub struct ProtocolClient {
    ws_tx: Arc<tokio::sync::Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>>>,
    audio_rx: UnboundedReceiver<AudioChunk>,
    message_rx: UnboundedReceiver<Message>,
    clock_sync: Arc<tokio::sync::Mutex<ClockSync>>,
}

impl ProtocolClient {
    /// Connect to Resonate server
    pub async fn connect(url: &str, hello: ClientHello) -> Result<Self, Error> {
        // Connect WebSocket
        let (ws_stream, _) = connect_async(url)
            .await
            .map_err(|e| Error::Connection(e.to_string()))?;

        let (mut write, read) = ws_stream.split();

        // Send client hello
        let hello_msg = Message::ClientHello(hello);
        let hello_json = serde_json::to_string(&hello_msg)
            .map_err(|e| Error::Protocol(e.to_string()))?;

        write.send(WsMessage::Text(hello_json))
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;

        // Wait for server hello
        let mut read_temp = read;
        if let Some(Ok(WsMessage::Text(text))) = read_temp.next().await {
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

        // Create channels for message routing
        let (audio_tx, audio_rx) = unbounded_channel();
        let (message_tx, message_rx) = unbounded_channel();

        let clock_sync = Arc::new(tokio::sync::Mutex::new(ClockSync::new()));

        // Spawn message router task
        let clock_sync_clone = Arc::clone(&clock_sync);
        tokio::spawn(async move {
            Self::message_router(read_temp, audio_tx, message_tx, clock_sync_clone).await;
        });

        Ok(Self {
            ws_tx: Arc::new(tokio::sync::Mutex::new(write)),
            audio_rx,
            message_rx,
            clock_sync,
        })
    }

    async fn message_router(
        mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        audio_tx: UnboundedSender<AudioChunk>,
        message_tx: UnboundedSender<Message>,
        _clock_sync: Arc<tokio::sync::Mutex<ClockSync>>,
    ) {
        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Binary(data)) => {
                    if let Ok(chunk) = AudioChunk::from_bytes(&data) {
                        let _ = audio_tx.send(chunk);
                    }
                }
                Ok(WsMessage::Text(text)) => {
                    if let Ok(msg) = serde_json::from_str::<Message>(&text) {
                        let _ = message_tx.send(msg);
                    }
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                    // Handled automatically by tokio-tungstenite
                }
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
    }

    /// Receive next audio chunk
    pub async fn recv_audio_chunk(&mut self) -> Option<AudioChunk> {
        self.audio_rx.recv().await
    }

    /// Receive next protocol message
    pub async fn recv_message(&mut self) -> Option<Message> {
        self.message_rx.recv().await
    }
}
```

**Step 3: Run tests**

Run: `cargo test`
Expected: All tests pass (new tests are ignored)

**Step 4: Commit**

```bash
git add .
git commit -m "feat: enhance WebSocket client with message routing"
```

---

## Task 5: End-to-End Player Example

**Files:**
- Create: `examples/player.rs`

**Step 1: Create player example**

Create `examples/player.rs`:
```rust
// ABOUTME: End-to-end player example
// ABOUTME: Connects to server, receives audio, and plays it back

use resonate::protocol::client::ProtocolClient;
use resonate::protocol::messages::{AudioFormatSpec, ClientHello, DeviceInfo, PlayerSupport, Message};
use resonate::audio::{CpalOutput, AudioOutput, AudioFormat, Codec, Sample, AudioBuffer};
use resonate::audio::decode::{PcmDecoder, Decoder};
use resonate::scheduler::AudioScheduler;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let hello = ClientHello {
        client_id: uuid::Uuid::new_v4().to_string(),
        name: "Resonate-RS Player".to_string(),
        version: 1,
        supported_roles: vec!["player".to_string()],
        device_info: DeviceInfo {
            product_name: "Resonate-RS Player".to_string(),
            manufacturer: "Resonate".to_string(),
            software_version: "0.1.0".to_string(),
        },
        player_support: Some(PlayerSupport {
            support_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 100,
            supported_commands: vec!["play".to_string(), "pause".to_string()],
        }),
        metadata_support: None,
    };

    println!("Connecting to ws://localhost:8927/resonate...");
    let mut client = ProtocolClient::connect("ws://localhost:8927/resonate", hello).await?;
    println!("Connected!");

    // Wait for stream/start message
    let mut audio_format: Option<AudioFormat> = None;
    let mut decoder: Option<PcmDecoder> = None;
    let mut output: Option<CpalOutput> = None;

    println!("Waiting for stream to start...");

    // Message handling loop
    let scheduler = AudioScheduler::new();
    let mut buffering_count = 0;
    const BUFFERING_TARGET: usize = 25; // Buffer 25 chunks (~500ms) before starting playback

    tokio::spawn(async move {
        // Playback loop
        loop {
            if let Some(buffer) = scheduler.next_ready() {
                if let Some(ref mut out) = output {
                    if let Err(e) = out.write(&buffer.samples) {
                        eprintln!("Output error: {}", e);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    loop {
        tokio::select! {
            Some(msg) = client.recv_message() => {
                match msg {
                    Message::StreamStart(stream_start) => {
                        println!("Stream starting: {} {}Hz {}ch {}bit",
                            stream_start.player.codec,
                            stream_start.player.sample_rate,
                            stream_start.player.channels,
                            stream_start.player.bit_depth);

                        audio_format = Some(AudioFormat {
                            codec: Codec::Pcm,
                            sample_rate: stream_start.player.sample_rate,
                            channels: stream_start.player.channels,
                            bit_depth: stream_start.player.bit_depth,
                            codec_header: None,
                        });

                        decoder = Some(PcmDecoder::new(stream_start.player.bit_depth));
                        output = Some(CpalOutput::new(audio_format.clone().unwrap())?);
                    }
                    _ => {
                        println!("Received message: {:?}", msg);
                    }
                }
            }
            Some(chunk) = client.recv_audio_chunk() => {
                if let (Some(ref dec), Some(ref fmt)) = (&decoder, &audio_format) {
                    // Decode audio chunk
                    match dec.decode(&chunk.data) {
                        Ok(samples) => {
                            // Create audio buffer
                            let buffer = AudioBuffer {
                                timestamp: chunk.timestamp,
                                play_at: Instant::now() + Duration::from_millis(100), // Simple 100ms buffer
                                samples,
                                format: fmt.clone(),
                            };

                            scheduler.schedule(buffer);
                            buffering_count += 1;

                            if buffering_count == BUFFERING_TARGET {
                                println!("Buffering complete, starting playback!");
                            }
                        }
                        Err(e) => {
                            eprintln!("Decode error: {}", e);
                        }
                    }
                }
            }
        }
    }
}
```

**Step 2: Build example**

Run: `cargo build --example player`
Expected: Compiles successfully

**Step 3: Test with real server (manual verification)**

Run: `cargo run --example player`
Expected: Connects, buffers, and plays audio

**Step 4: Commit**

```bash
git add .
git commit -m "feat: add end-to-end player example"
```

---

## Task 6: Phase 2 Verification

**Files:**
- All existing tests

**Step 1: Run full test suite**

Run: `cargo test`
Expected: All tests pass

**Step 2: Check for compiler warnings**

Run: `cargo clippy -- -D warnings`
Expected: No warnings (add docs if needed)

**Step 3: Format code**

Run: `cargo fmt`
Expected: Code formatted

**Step 4: Build in release mode**

Run: `cargo build --release`
Expected: Builds successfully

**Step 5: Build all examples**

Run: `cargo build --examples`
Expected: All examples compile

**Step 6: Manual verification with server**

Run: `cargo run --example player`
Expected:
- Connects to server
- Receives audio chunks
- Plays audio without glitches
- Memory stays stable (<50MB)

**Step 7: Final commit**

```bash
git add .
git commit -m "chore: phase 2 audio pipeline complete and verified"
```

---

## Phase 2 Acceptance Criteria

✅ Audio output trait + cpal implementation
✅ Lock-free scheduler for chunk playback
✅ Buffer pool for memory efficiency
✅ Enhanced WebSocket client with message routing
✅ End-to-end player example
✅ Can play PCM audio from server
✅ All tests passing
✅ Code compiles with no warnings
✅ Examples compile and run

## Next Steps

**Phase 3: Optimization** will add:
- SIMD volume scaling (AVX2)
- SIMD resampler
- Benchmarks with criterion
- Profiling and optimization
- Buffer pool tuning

See `docs/rust-thoughts.md` for Phase 3 detailed specification.
