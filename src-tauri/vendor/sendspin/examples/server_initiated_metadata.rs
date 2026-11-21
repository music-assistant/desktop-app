// ABOUTME: Inbound listener that announces the metadata role via mDNS and
// ABOUTME: lets ConnectionManager handle spec multi-server arbitration
// ABOUTME: Only partly spec-compliant: a real client would also persist the last-played server across restarts

use clap::Parser;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use sendspin::protocol::manager::ConnectionManager;
use sendspin::protocol::messages::{Message, PlaybackState};
use sendspin::ProtocolClientBuilder;

/// Sendspin metadata client using the managed inbound listener
#[derive(Parser, Debug)]
#[command(name = "server_initiated_metadata")]
struct Args {
    /// Address to bind the listener on (8928 is the recommended client port)
    #[arg(short, long, default_value = "0.0.0.0:8928")]
    bind: String,

    /// Friendly name advertised in the mDNS TXT record
    #[arg(short, long, default_value = "Metadata Listener")]
    name: String,

    /// Persistent client id (defaults to a random UUID)
    #[arg(short, long)]
    id: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    let args = Args::parse();
    let client_id = args.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let listener = ProtocolClientBuilder::builder()
        .client_id(client_id)
        .name(args.name.clone())
        .metadata()
        .build()
        .listen(&args.bind)
        .await?;

    let port = listener.local_addr()?.port();

    // The manager owns the accept loop from here: concurrent inbound
    // handshakes, the spec keep-or-switch policy (playback beats discovery,
    // last-played breaks discovery ties), and goodbye(another_server) to
    // every losing connection.
    let mut manager = ConnectionManager::new(listener);

    // Advertise _sendspin._tcp.local. so servers can discover and dial in.
    let daemon = ServiceDaemon::new()?;
    let service = ServiceInfo::new(
        "_sendspin._tcp.local.",
        &args.name,
        "sendspin-metadata-listener.local.",
        "",
        port,
        &[("path", "/sendspin"), ("name", args.name.as_str())][..],
    )?
    .enable_addr_auto();
    daemon.register(service)?;

    println!("Listening on :{port}, advertising _sendspin._tcp.local. — waiting for server...");

    // Each iteration serves one arbitration winner. When the manager
    // switches servers (or the server goes away), the connection's channels
    // close, the inner loop ends, and next_connection() yields the
    // replacement.
    while let Some(mut conn) = manager.next_connection().await {
        let peer = conn.peer;
        let server_id = conn.server_hello.server_id.clone();
        println!(
            "[{peer}] now serving server_id={server_id} connection_reason={:?}",
            conn.server_hello.connection_reason
        );

        // Only `messages` is consumed: this client negotiates just the
        // metadata role, so the audio/artwork/visualizer receivers stay
        // silent and can be ignored. A player would drain those too.
        while let Some(msg) = conn.messages.recv().await {
            match msg {
                Message::GroupUpdate(g) if g.playback_state == Some(PlaybackState::Playing) => {
                    // Feed the policy input back so a returning server wins
                    // discovery ties. A spec-compliant client would also
                    // persist this to disk here.
                    manager.set_last_played(Some(server_id.clone()));
                    println!("[{peer}] playing — recorded as last played");
                }
                Message::ServerState(s) => println!("[{peer}] [METADATA] {:?}", s.metadata),
                other => println!("[{peer}] [MSG] {other:?}"),
            }
        }
        println!("[{peer}] disconnected — waiting for next server...");
    }

    Ok(())
}
