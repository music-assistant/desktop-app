// ABOUTME: Minimal test to verify we receive ALL server messages
// ABOUTME: Just connects and prints everything the server sends

use clap::Parser;
use sendspin::protocol::messages::PlayerState;
use sendspin::ProtocolClientBuilder;

/// Minimal Sendspin test client
#[derive(Parser, Debug)]
#[command(name = "minimal_test")]
struct Args {
    /// WebSocket URL of the Sendspin server
    #[arg(short, long, default_value = "ws://192.168.200.8:8927/sendspin")]
    server: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    println!("Connecting to {}...", args.server);
    let test = ProtocolClientBuilder::builder()
        .client_id(uuid::Uuid::new_v4().to_string())
        .name("Minimal Test Client".to_string())
        .initial_player_state(PlayerState {
            volume: Some(100),
            muted: Some(false),
            static_delay_ms: Some(0),
            required_lead_time_ms: Some(500),
            min_buffer_ms: Some(500),
            supported_commands: None,
        })
        .build();

    let client = test.connect(&args.server).await?;
    println!("Connected! Server said hello.");

    // Split client
    let conn = client.split();
    let mut message_rx = conn.messages;
    let mut audio_rx = conn.audio;
    let _guard = conn.guard;

    println!("\nListening for ALL messages from server...\n");

    // Just print everything we receive
    loop {
        tokio::select! {
            Some(msg) = message_rx.recv() => {
                println!("[TEXT MESSAGE] {:?}", msg);
            }
            Some(chunk) = audio_rx.recv() => {
                println!("[AUDIO CHUNK] timestamp={} size={} bytes",
                    chunk.timestamp, chunk.data.len());
            }
            else => {
                println!("Connection closed");
                break;
            }
        }
    }

    Ok(())
}
