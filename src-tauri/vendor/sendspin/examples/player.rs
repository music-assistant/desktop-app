// ABOUTME: End-to-end player example
// ABOUTME: Connects to server, receives audio, and plays it back

use base64::prelude::*;
use clap::{Parser, ValueEnum};
use sendspin::audio::decode::{Decoder, FlacDecoder, OpusDecoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec, SyncedPlayer, SyncedPlayerConfig};
use sendspin::protocol::messages::{
    AudioFormatSpec, Message, PlayerCommand, PlayerCommandType, PlayerFormatRequest, PlayerState,
    PlayerStateCommand, PlayerV1Support,
};
use sendspin::ProtocolClientBuilder;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait};

fn print_devices() -> Result<(), Box<dyn std::error::Error>> {
    let available_hosts = cpal::available_hosts();

    println!("Available audio output devices:\n");
    let mut usable_index = 0;
    let mut example_id: Option<String> = None;
    for host_id in available_hosts {
        let host = cpal::host_from_id(host_id)?;
        let devices = host.devices()?;
        for device in devices {
            let id = device
                .id()
                .map_or("Unknown ID".to_string(), |id| id.to_string());
            let output_configs = match device.supported_output_configs() {
                Ok(f) => f.collect(),
                Err(_) => Vec::new(),
            };
            if !output_configs.is_empty() {
                if example_id.is_none() {
                    example_id = Some(id.clone());
                }
                // Construct [number] and right-justify it in a fixed-width field
                let index_str = format!("[{}]", usable_index);
                if let Ok(desc) = device.description() {
                    println!("{:>6} {}\n       Description: {}", index_str, id, desc);
                } else {
                    println!("{:>6} {}", index_str, id);
                }
                usable_index += 1;
            }
        }
    }

    if usable_index == 0 {
        println!("\nNo devices found");
    } else {
        println!("\nTo select an audio device by index:");
        println!("  player --audio-device 0");
        if let Some(id) = example_id {
            println!("Or by device id string:");
            println!("  player --audio-device \"{}\"", id);
        }
    }
    Ok(())
}

/// Accepts either an index (as printed by print_devices) or a device id string.
fn find_device(device_query: &str) -> Result<Option<cpal::Device>, Box<dyn std::error::Error>> {
    let available_hosts = cpal::available_hosts();
    let idx_query = device_query.parse::<usize>().ok();
    let mut usable_index = 0;

    for host_id in available_hosts {
        let host = cpal::host_from_id(host_id)?;
        let devices = host.devices()?;
        for device in devices {
            let id = device
                .id()
                .map_or("Unknown ID".to_string(), |id| id.to_string());
            let output_configs = match device.supported_output_configs() {
                Ok(f) => f.collect(),
                Err(_) => Vec::new(),
            };
            if !output_configs.is_empty() {
                if let Some(idx) = idx_query {
                    if usable_index == idx {
                        return Ok(Some(device));
                    }
                } else if id == device_query {
                    return Ok(Some(device));
                }
                usable_index += 1;
            }
        }
    }
    Ok(None)
}

/// Environment variable helpers
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false)
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CodecChoice {
    Pcm,
    Opus,
    Flac,
}

/// Sendspin audio player
#[derive(Parser, Debug)]
#[command(name = "player")]
#[command(about = "Connect to Sendspin server and play audio", long_about = None)]
struct Args {
    /// WebSocket URL of the Sendspin server
    #[arg(short, long, default_value = "ws://localhost:8927/sendspin")]
    server: String,

    /// Client name
    #[arg(short, long, default_value = "Sendspin-RS Player")]
    name: String,

    /// Player ID (optional, generates random UUID if not provided)
    #[arg(short = 'i', long = "id")]
    id: Option<String>,

    /// Restrict negotiation to one codec (pcm, opus, or flac)
    #[arg(long, value_enum)]
    codec: Option<CodecChoice>,

    /// List all available audio output devices and exit
    #[arg(long = "list-audio-devices")]
    list_audio_devices: bool,

    /// Audio output device ID (optional, uses default if not specified)
    #[arg(long = "audio-device")]
    audio_device: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    // Handle --list-audio-devices flag
    if args.list_audio_devices {
        print_devices()?;
        return Ok(());
    }

    // Resolve device if specified
    let device = if let Some(device_name) = &args.audio_device {
        match find_device(device_name) {
            Ok(Some(dev)) => Some(dev),
            Ok(None) => {
                return Err(format!("Device '{}' not found", device_name).into());
            }
            Err(e) => {
                eprintln!("Failed to find device: {}", e);
                return Err(e);
            }
        }
    } else {
        None
    };

    // Use provided ID or generate a random UUID
    let client_id = args.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    println!("Connecting to {}...", args.server);
    let required_lead_time_ms = 500;
    let min_buffer_ms = 500;

    // Advertise all supported codecs by default, preferring FLAC, then Opus,
    // then PCM. The --codec flag restricts negotiation when a specific decoder
    // needs exercising.
    let formats: &[(&str, u8)] = match args.codec {
        Some(CodecChoice::Pcm) => &[("pcm", 24), ("pcm", 16)],
        Some(CodecChoice::Opus) => &[("opus", 16)],
        Some(CodecChoice::Flac) => &[("flac", 24), ("flac", 16)],
        None => &[
            ("flac", 24),
            ("flac", 16),
            ("opus", 16),
            ("pcm", 24),
            ("pcm", 16),
        ],
    };
    let supported_formats = formats
        .iter()
        .map(|&(codec, bit_depth)| AudioFormatSpec {
            codec: codec.to_string(),
            channels: 2,
            sample_rate: 48000,
            bit_depth,
        })
        .collect();

    let test = ProtocolClientBuilder::builder()
        .client_id(client_id)
        .name(args.name.clone())
        .player_v1_support(PlayerV1Support {
            supported_formats,
            buffer_capacity: 50 * 1024 * 1024,
            supported_commands: vec!["volume".to_string(), "mute".to_string()],
        })
        .initial_player_state(PlayerState {
            volume: Some(100),
            muted: Some(false),
            static_delay_ms: Some(0),
            required_lead_time_ms: Some(required_lead_time_ms),
            min_buffer_ms: Some(min_buffer_ms),
            supported_commands: Some(vec![PlayerStateCommand::SetStaticDelay]),
        })
        .build();

    let client = test.connect(&args.server).await?;
    println!("Connected!");

    // Split client into separate receivers for concurrent processing
    let conn = client.split();
    let mut message_rx = conn.messages;
    let mut audio_rx = conn.audio;
    let clock_sync = conn.clock_sync;
    let sender = conn.sender;
    let _guard = conn.guard;

    println!("Waiting for stream to start...");

    // Configuration from environment variables
    let start_buffer_ms = env_u64("SS_PLAY_START_BUFFER_MS", 500);
    let log_lead = env_bool("SS_LOG_LEAD");
    println!(
        "Player config: start_buffer={}ms, log_lead={}",
        start_buffer_ms, log_lead
    );

    // Message handling variables
    let mut decoder: Option<Box<dyn Decoder>> = None;
    let mut audio_format: Option<AudioFormat> = None;
    let mut endian_locked: Option<PcmEndian> = None; // Auto-detect on first chunk
    let mut buffered_duration_us: u64 = 0; // Track buffered audio duration in microseconds
    let mut playback_started = false; // Track if we've started playback
    let mut first_chunk_logged = false; // Track if we've logged the first chunk
    let mut synced_player: Option<SyncedPlayer> = None;
    let mut format_requested = false; // One-shot guard for the request-format demo
                                      // Retained so a command arriving before the player exists still applies.
    let mut static_delay_ms: u16 = 0;

    loop {
        // Process messages and audio chunks concurrently
        tokio::select! {
            Some(msg) = message_rx.recv() => {
                match msg {
                    Message::StreamStart(stream_start) => {
                        // Demonstrate `stream/request-format`: now that the player
                        // stream is live, ask the server to re-encode it (here,
                        // lighter 16-bit PCM this example can still decode).
                        // One-shot, so the server's responding stream/start
                        // doesn't re-trigger it.
                        if env_bool("SS_REQUEST_FORMAT")
                            && !format_requested
                            && stream_start.player.is_some()
                        {
                            format_requested = true;
                            sender
                                .request_player_format(PlayerFormatRequest {
                                    codec: Some("pcm".to_string()),
                                    channels: Some(2),
                                    sample_rate: Some(48_000),
                                    bit_depth: Some(16),
                                })
                                .await?;
                            println!("Requested 16-bit PCM stereo @ 48kHz via stream/request-format");
                        }

                        if let Some(ref player_config) = stream_start.player {
                            // Tear down the previous stream's decoder state
                            // before any validation: every `continue` below
                            // must leave "no decoder, no format" rather than
                            // a stale pairing that would misdecode the new
                            // stream's chunks.
                            decoder = None;
                            audio_format = None;
                            endian_locked = None;

                            println!(
                                "Stream starting: codec='{}' {}Hz {}ch {}bit",
                                player_config.codec,
                                player_config.sample_rate,
                                player_config.channels,
                                player_config.bit_depth
                            );

                            // Validate codec before proceeding
                            let codec = match player_config.codec.as_str() {
                                "pcm" => Codec::Pcm,
                                "opus" => Codec::Opus,
                                "flac" => Codec::Flac,
                                other => {
                                    eprintln!("ERROR: Unsupported codec '{}' - only 'pcm', 'opus', and 'flac' are supported!", other);
                                    eprintln!("Server is sending compressed audio that we can't decode!");
                                    continue;
                                }
                            };

                            if player_config.bit_depth != 16 && player_config.bit_depth != 24 {
                                eprintln!("ERROR: Unsupported bit depth {} - only 16 or 24-bit supported!", player_config.bit_depth);
                                continue;
                            }

                            // Codec header arrives base64-encoded (for FLAC:
                            // the fLaC marker plus STREAMINFO block).
                            let codec_header: Option<Vec<u8>> = match player_config.codec_header.as_deref() {
                                Some(b64) => match BASE64_STANDARD.decode(b64) {
                                    Ok(header) => Some(header),
                                    Err(e) => {
                                        eprintln!("ERROR: Invalid base64 codec header: {}", e);
                                        continue;
                                    }
                                },
                                None => None,
                            };

                            decoder = match codec {
                                // PCM decoder is created on the first chunk,
                                // after endianness is locked in.
                                Codec::Pcm => None,
                                Codec::Opus => match OpusDecoder::new(
                                    player_config.sample_rate,
                                    player_config.channels,
                                ) {
                                    Ok(opus) => Some(Box::new(opus)),
                                    Err(e) => {
                                        eprintln!("ERROR: Invalid Opus stream format: {}", e);
                                        continue;
                                    }
                                },
                                Codec::Flac => {
                                    let flac = match codec_header.as_deref() {
                                        Some(header) => match FlacDecoder::with_header(header) {
                                            Ok(d) => d,
                                            Err(e) => {
                                                eprintln!("ERROR: Invalid FLAC codec header: {}", e);
                                                continue;
                                            }
                                        },
                                        None => FlacDecoder::new(),
                                    };
                                    Some(Box::new(flac))
                                }
                                // `codec` was narrowed to Pcm/Opus/Flac when
                                // parsing player_config.codec above.
                                _ => unreachable!(),
                            };

                            audio_format = Some(AudioFormat {
                                codec,
                                sample_rate: player_config.sample_rate,
                                channels: player_config.channels,
                                bit_depth: player_config.bit_depth,
                                codec_header,
                            });

                            buffered_duration_us = 0; // Reset on new stream
                            playback_started = false;
                            first_chunk_logged = false; // Reset for new stream
                            match codec {
                                Codec::Pcm => println!("Waiting for first audio chunk to auto-detect endianness..."),
                                _ => println!("Decoder ready, waiting for audio chunks..."),
                            }
                        } else {
                            println!("Received stream/start without player config");
                        }
                    }
                    Message::ServerTime(server_time) => {
                        // Get t4 (client receive time) in Unix microseconds
                        let t4 = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as i64;

                        // Update clock sync with all four timestamps
                        let t1 = server_time.client_transmitted;
                        let t2 = server_time.server_received;
                        let t3 = server_time.server_transmitted;

                        clock_sync.lock().update(t1, t2, t3, t4);

                        // Log sync quality
                        let sync = clock_sync.lock();
                        if let Some(rtt) = sync.rtt_micros() {
                            let quality = sync.quality();
                            println!(
                                "Clock sync updated: RTT={:.2}ms, quality={:?}",
                                rtt as f64 / 1000.0,
                                quality
                            );
                        }
                    }
                    Message::StreamEnd(stream_end) => {
                        println!("Stream ended: {:?}", stream_end.roles);
                        if let Some(ref player) = synced_player {
                            player.clear();
                        }
                        buffered_duration_us = 0;
                        playback_started = false;
                        first_chunk_logged = false;
                    }
                    Message::StreamClear(stream_clear) => {
                        println!("Stream cleared: {:?}", stream_clear.roles);
                        if let Some(ref player) = synced_player {
                            player.clear();
                        }
                        buffered_duration_us = 0;
                        playback_started = false;
                        first_chunk_logged = false;
                    }
                    Message::ServerCommand(command) => {
                        if let Some(PlayerCommand {
                            command: PlayerCommandType::SetStaticDelay,
                            static_delay_ms: Some(delay_ms),
                            ..
                        }) = command.player
                        {
                            static_delay_ms = delay_ms;
                            if let Some(ref player) = synced_player {
                                player.set_static_delay(delay_ms);
                            }
                            println!("Static delay set to {delay_ms} ms");
                        }
                    }
                    _ => {
                        println!("Received message: {:?}", msg);
                    }
                }
            }
            Some(chunk) = audio_rx.recv() => {
                // Log first chunk bytes for diagnostics
                if !first_chunk_logged {
                    println!("\n=== FIRST AUDIO CHUNK DIAGNOSTICS ===");
                    println!("Chunk timestamp: {} µs", chunk.timestamp);
                    println!("Chunk data length: {} bytes", chunk.data.len());
                    let preview_len = chunk.data.len().min(32);
                    print!("First {} bytes (hex): ", preview_len);
                    for byte in &chunk.data[..preview_len] {
                        print!("{:02X} ", byte);
                    }
                    println!("\n=====================================\n");
                    first_chunk_logged = true;
                }

                // PCM-specific handling: raw sample chunks need a frame-size
                // sanity check and one-time endianness selection. Compressed
                // codecs (FLAC) are framed by the codec itself.
                if let Some(ref fmt) = audio_format {
                    if fmt.codec == Codec::Pcm {
                        // Frame sanity check
                        let bytes_per_sample = match fmt.bit_depth {
                            16 => 2,
                            24 => 3,
                            _ => {
                                eprintln!("Unsupported bit depth: {}", fmt.bit_depth);
                                continue;
                            }
                        } as usize;
                        let frame_size = bytes_per_sample * fmt.channels as usize;

                        if chunk.data.len() % frame_size != 0 {
                            eprintln!(
                                "BAD FRAME: {} bytes not multiple of frame size {} ({}-bit, {}ch)",
                                chunk.data.len(), frame_size, fmt.bit_depth, fmt.channels
                            );
                            continue; // Don't decode garbage
                        }

                        // One-time endianness setup on first chunk
                        // Per spec: macOS and most systems use Little-Endian PCM
                        // Only use Big-Endian if explicitly signaled by server
                        if endian_locked.is_none() {
                            // Default to Little-Endian (standard for macOS/Windows/Linux)
                            let endian = PcmEndian::Little;
                            endian_locked = Some(endian);
                            decoder = Some(Box::new(PcmDecoder::with_endian(fmt.bit_depth, endian)));
                            println!("Using Little-Endian PCM (standard for modern systems)");
                        }
                    }
                }

                if let (Some(ref dec), Some(ref fmt)) = (&decoder, &audio_format) {
                    match dec.decode(&chunk.data) {
                        Ok(samples) => {
                            // Calculate chunk duration in microseconds
                            // samples.len() includes all channels
                            let frames = samples.len() / fmt.channels as usize;
                            let duration_micros = (frames as u64 * 1_000_000) / fmt.sample_rate as u64;
                            // Track buffered duration
                            buffered_duration_us += duration_micros;

                            // Check if we've buffered enough to start playback
                            if !playback_started && buffered_duration_us >= start_buffer_ms * 1000 {
                                playback_started = true;
                                println!(
                                    "Prebuffering complete ({:.1}ms buffered), starting playback!",
                                    buffered_duration_us as f64 / 1000.0
                                );
                            }

                            // Track and log lead time
                            if log_lead {
                                println!(
                                    "Enqueued chunk ts={} buffered={:.1}ms len={} bytes",
                                    chunk.timestamp,
                                    buffered_duration_us as f64 / 1000.0,
                                    chunk.data.len()
                                );
                            }

                            if synced_player.is_none() {
                                let player_config = SyncedPlayerConfig {
                                    device: device.as_ref().cloned(),
                                    volume: 100,
                                    muted: false,
                                    buffer_size: None,
                                };
                                match SyncedPlayer::new(
                                    fmt.clone(),
                                    Arc::clone(&clock_sync),
                                    player_config,
                                ) {
                                    Ok(player) => {
                                        println!("Synced audio output initialized");
                                        player.set_static_delay(static_delay_ms);
                                        synced_player = Some(player);
                                    }
                                    Err(e) => {
                                        eprintln!("Failed to create synced output: {}", e);
                                    }
                                }
                            }

                            if let Some(ref player) = synced_player {
                                let buffer = AudioBuffer {
                                    timestamp: chunk.timestamp,
                                    samples,
                                    format: fmt.clone(),
                                };
                                player.enqueue(buffer);
                            }
                        }
                        Err(e) => {
                            eprintln!("Decode error: {}", e);
                        }
                    }
                }
            }
            else => {
                // Both channels closed
                break;
            }
        }
    }

    Ok(())
}
