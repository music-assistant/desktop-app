// ABOUTME: Example of a controller-only client (no audio playback)
// ABOUTME: Connects, sends playback commands, listens for server state updates

use clap::Parser;
use sendspin::protocol::messages::{GoodbyeReason, Message, RepeatMode};
use sendspin::ProtocolClientBuilder;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Sendspin controller-only client
#[derive(Parser, Debug)]
#[command(name = "controller_only")]
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
    let builder = ProtocolClientBuilder::builder()
        .client_id(uuid::Uuid::new_v4().to_string())
        .name("Controller Example".to_string())
        .controller()
        .build();

    let client = builder.connect(&args.server).await?;
    println!("Connected!");

    let conn = client.split();
    let controller = conn
        .controller
        .expect("server should grant controller role");
    let mut message_rx = conn.messages;
    let guard = conn.guard;

    println!("Commands: play, pause, stop, next, prev, vol <0-100>, mute, unmute,");
    println!("          repeat <off|one|all>, shuffle, unshuffle, switch, quit");

    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        tokio::select! {
            Some(msg) = message_rx.recv() => {
                match msg {
                    Message::ServerState(state) => println!("[STATE] {:?}", state),
                    other => println!("[MSG] {:?}", other),
                }
            }
            line = lines.next_line() => {
                let line = match line? {
                    Some(l) => l,
                    None => break,
                };
                let parts: Vec<&str> = line.split_whitespace().collect();
                match parts.first().copied() {
                    Some("play") => controller.play().await?,
                    Some("pause") => controller.pause().await?,
                    Some("stop") => controller.stop().await?,
                    Some("next") => controller.next().await?,
                    Some("prev") => controller.previous().await?,
                    Some("vol") => {
                        if let Some(v) = parts.get(1).and_then(|s| s.parse::<u8>().ok()) {
                            controller.set_volume(v).await?;
                        } else {
                            println!("Usage: vol <0-100>");
                            continue;
                        }
                    }
                    Some("mute") => controller.set_mute(true).await?,
                    Some("unmute") => controller.set_mute(false).await?,
                    Some("repeat") => match parts.get(1).copied() {
                        Some("off") => controller.repeat(RepeatMode::Off).await?,
                        Some("one") => controller.repeat(RepeatMode::One).await?,
                        Some("all") => controller.repeat(RepeatMode::All).await?,
                        _ => { println!("Usage: repeat <off|one|all>"); continue; }
                    }
                    Some("shuffle") => controller.shuffle(true).await?,
                    Some("unshuffle") => controller.shuffle(false).await?,
                    Some("switch") => controller.switch().await?,
                    Some("quit") => break,
                    _ => { println!("Unknown command"); continue; }
                }
                println!("[SENT] {}", line);
            }
        }
    }

    guard.disconnect(GoodbyeReason::Shutdown).await?;

    Ok(())
}
