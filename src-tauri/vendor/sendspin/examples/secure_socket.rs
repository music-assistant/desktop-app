// ABOUTME: Example demonstrating builder capabilities with custom headers
// ABOUTME: Shows player format configuration, controller role, and auth proxy setup
// ABOUTME: Run with: cargo run --example secure_socket --features native-tls

use clap::Parser;
use sendspin::protocol::messages::{AudioFormatSpec, PlayerState, PlayerV1Support};
use sendspin::ProtocolClientBuilder;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Sendspin advanced client
#[derive(Parser, Debug)]
#[command(name = "advanced_client")]
#[command(about = "Demonstrates builder capabilities with custom headers", long_about = None)]
struct Args {
    /// WebSocket URL of the Sendspin server
    #[arg(short, long, default_value = "wss://localhost:8927/sendspin")]
    server: String,

    /// Client name
    #[arg(short, long, default_value = "Sendspin-RS Advanced Client")]
    name: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();

    println!("Connecting to {}...", args.server);

    // Build a request with custom headers (e.g., for auth proxy)
    let mut request = args.server.into_client_request().unwrap();
    request
        .headers_mut()
        .insert("cookie", "ingress_session=<session_token>".parse().unwrap());

    // Configure the builder with explicit player support and controller role
    let client = ProtocolClientBuilder::builder()
        .client_id(uuid::Uuid::new_v4().to_string())
        .name(args.name)
        .product_name(Some("Sendspin-RS Advanced Client".to_string()))
        .software_version(Some(env!("CARGO_PKG_VERSION").to_string()))
        // Declare player capabilities: 24-bit/48kHz stereo PCM
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 50 * 1024 * 1024,
            supported_commands: vec!["volume".to_string(), "mute".to_string()],
        })
        // Request controller role for playback control
        .controller()
        // Set initial player state
        .initial_player_state(PlayerState {
            volume: Some(80),
            muted: Some(false),
            static_delay_ms: Some(0),
            required_lead_time_ms: Some(500),
            min_buffer_ms: Some(500),
            ..Default::default()
        })
        .build()
        .connect(request)
        .await?;

    println!("Connected!");

    // Split into conn channels for concurrent use
    let conn = client.split();

    if let Some(controller) = conn.controller {
        println!("Controller role granted — can send playback commands");
        // e.g., controller.play().await?;
        drop(controller);
    } else {
        println!("Controller role not granted by server");
    }

    Ok(())
}
