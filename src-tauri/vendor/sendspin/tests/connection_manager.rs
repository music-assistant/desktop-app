// ABOUTME: Integration tests for ConnectionManager — concurrent inbound
// ABOUTME: handshakes, multi-server arbitration, and automatic goodbyes.

use futures_util::{SinkExt, StreamExt};
use sendspin::protocol::manager::{ConnectionManager, ManagerConfig};
use sendspin::protocol::messages::{ConnectionReason, GoodbyeReason, Message, ServerHello};
use sendspin::ProtocolClientBuilder;
use std::time::Duration;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

/// A test protocol-server: dials the manager's listener, answers
/// `client/hello` with a `server/hello` carrying the given identity, and
/// forwards every subsequent text frame to `messages`.
struct Peer {
    messages: tokio::sync::mpsc::UnboundedReceiver<String>,
    /// Send raw frames to the client (unused by most tests; keeps write half alive).
    _writer: tokio::sync::mpsc::UnboundedSender<WsMessage>,
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

async fn connect_peer(url: &str, server_id: &str, reason: ConnectionReason) -> Peer {
    let (ws, _) = connect_async(url).await.expect("WS connect failed");
    let (mut write, mut read) = ws.split();

    let hello_text = match read.next().await.expect("no hello").expect("ws error") {
        WsMessage::Text(t) => t,
        other => panic!("expected text client/hello, got {other:?}"),
    };
    let parsed: Message = serde_json::from_str(&hello_text).expect("client/hello must deserialize");
    assert!(
        matches!(parsed, Message::ClientHello(_)),
        "first message should be client/hello"
    );

    let server_hello = serde_json::to_string(&Message::ServerHello(ServerHello {
        server_id: server_id.to_string(),
        name: format!("{server_id} name"),
        version: 1,
        active_roles: vec![],
        connection_reason: reason,
    }))
    .unwrap();
    write
        .send(WsMessage::Text(server_hello.into()))
        .await
        .expect("send server/hello");

    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            let text = match msg {
                WsMessage::Text(t) => t,
                WsMessage::Close(_) => break,
                _ => continue,
            };
            if msg_tx.send(text.to_string()).is_err() {
                break;
            }
        }
    });

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if write.send(frame).await.is_err() {
                break;
            }
        }
    });

    Peer {
        messages: msg_rx,
        _writer: out_tx,
        _tasks: vec![reader, writer],
    }
}

async fn manager(config: Option<ManagerConfig>) -> (String, ConnectionManager) {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-managed-client".to_string())
        .name("Managed Client".to_string())
        .build();
    let listener = builder.listen("127.0.0.1:0").await.expect("listen failed");
    let mgr = match config {
        Some(c) => ConnectionManager::with_config(listener, c),
        None => ConnectionManager::new(listener),
    };
    let addr = mgr.local_addr().expect("local_addr");
    (format!("ws://{addr}"), mgr)
}

/// Waits until the peer receives `client/goodbye` and returns its reason;
/// panics if the peer goes quiet for 2s first.
async fn expect_goodbye(peer: &mut Peer) -> GoodbyeReason {
    loop {
        let text = timeout(Duration::from_secs(2), peer.messages.recv())
            .await
            .expect("timed out waiting for client/goodbye")
            .expect("peer channel closed without a goodbye");
        if let Ok(Message::ClientGoodbye(g)) = serde_json::from_str::<Message>(&text) {
            return g.reason;
        }
    }
}

/// Asserts the peer receives no `client/goodbye` within `window`.
async fn assert_no_goodbye(peer: &mut Peer, window: Duration) {
    let deadline = tokio::time::Instant::now() + window;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match timeout(remaining, peer.messages.recv()).await {
            Err(_) => return, // window elapsed quietly
            Ok(None) => panic!("peer connection closed unexpectedly"),
            Ok(Some(text)) => {
                if matches!(
                    serde_json::from_str::<Message>(&text),
                    Ok(Message::ClientGoodbye(_))
                ) {
                    panic!("unexpected client/goodbye");
                }
            }
        }
    }
}

/// Drains a yielded connection's message channel until it closes; panics if
/// it stays open past 2s.
async fn expect_channels_close(conn: &mut sendspin::ManagedConnection) {
    loop {
        match timeout(Duration::from_secs(2), conn.messages.recv()).await {
            Ok(None) => return,
            Ok(Some(_)) => continue,
            Err(_) => panic!("displaced connection's channels never closed"),
        }
    }
}

#[tokio::test]
async fn test_first_server_is_promoted() {
    let (url, mut mgr) = manager(None).await;

    let _peer = connect_peer(&url, "server-a", ConnectionReason::Discovery).await;

    let conn = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn.server_hello.server_id, "server-a");
    assert_eq!(
        conn.server_hello.connection_reason,
        ConnectionReason::Discovery
    );
    assert!(conn.peer.ip().is_loopback());
}

#[tokio::test]
async fn test_playback_displaces_discovery_incumbent() {
    let (url, mut mgr) = manager(None).await;

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Discovery).await;
    let mut conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    let _peer_b = connect_peer(&url, "server-b", ConnectionReason::Playback).await;

    // The displaced incumbent gets an automatic goodbye(another_server)...
    assert_eq!(
        expect_goodbye(&mut peer_a).await,
        GoodbyeReason::AnotherServer
    );
    // ...the app sees its channels close...
    expect_channels_close(&mut conn_a).await;
    // ...and the winner is yielded.
    let conn_b = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn_b.server_hello.server_id, "server-b");
}

#[tokio::test]
async fn test_discovery_never_displaces_playback() {
    let (url, mut mgr) = manager(None).await;

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Playback).await;
    let conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    // Even the last-played server cannot displace active playback.
    mgr.set_last_played(Some("server-b".to_string()));
    let mut peer_b = connect_peer(&url, "server-b", ConnectionReason::Discovery).await;

    // The rejected newcomer gets the goodbye; the incumbent stays quiet.
    assert_eq!(
        expect_goodbye(&mut peer_b).await,
        GoodbyeReason::AnotherServer
    );
    assert_no_goodbye(&mut peer_a, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn test_last_played_breaks_discovery_tie() {
    let (url, mut mgr) = manager(None).await;
    mgr.set_last_played(Some("server-b".to_string()));

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Discovery).await;
    let conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    let _peer_b = connect_peer(&url, "server-b", ConnectionReason::Discovery).await;
    assert_eq!(
        expect_goodbye(&mut peer_a).await,
        GoodbyeReason::AnotherServer
    );
    let conn_b = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn_b.server_hello.server_id, "server-b");
}

#[tokio::test]
async fn test_discovery_tie_defaults_to_keep() {
    let (url, mut mgr) = manager(None).await;

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Discovery).await;
    let conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    let mut peer_b = connect_peer(&url, "server-b", ConnectionReason::Discovery).await;
    assert_eq!(
        expect_goodbye(&mut peer_b).await,
        GoodbyeReason::AnotherServer
    );
    assert_no_goodbye(&mut peer_a, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn test_incumbent_death_frees_slot() {
    let (url, mut mgr) = manager(None).await;

    let peer_a = connect_peer(&url, "server-a", ConnectionReason::Discovery).await;
    let mut conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    // Server A goes away (socket closed, no goodbye exchange).
    drop(peer_a);
    expect_channels_close(&mut conn_a).await;

    // A plain discovery connection (not last-played) must now be promoted:
    // the dead incumbent may no longer win arbitration.
    let mut peer_b = connect_peer(&url, "server-b", ConnectionReason::Discovery).await;
    let conn_b = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn_b.server_hello.server_id, "server-b");
    assert_no_goodbye(&mut peer_b, Duration::from_millis(300)).await;
}

#[tokio::test]
async fn test_disconnect_sends_goodbye_and_keeps_listening() {
    let (url, mut mgr) = manager(None).await;

    // Disconnect with no incumbent is a no-op Ok.
    mgr.disconnect(GoodbyeReason::UserRequest)
        .await
        .expect("no-op disconnect");

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Playback).await;
    let mut conn_a = mgr.next_connection().await.expect("manager stopped");

    mgr.disconnect(GoodbyeReason::UserRequest)
        .await
        .expect("disconnect");
    assert_eq!(
        expect_goodbye(&mut peer_a).await,
        GoodbyeReason::UserRequest
    );
    expect_channels_close(&mut conn_a).await;

    // The manager keeps listening: a new server can still connect.
    let _peer_b = connect_peer(&url, "server-b", ConnectionReason::Discovery).await;
    let conn_b = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn_b.server_hello.server_id, "server-b");
}

#[tokio::test]
async fn test_playback_displaces_playback() {
    let (url, mut mgr) = manager(None).await;

    let mut peer_a = connect_peer(&url, "server-a", ConnectionReason::Playback).await;
    let conn_a = mgr.next_connection().await.expect("manager stopped");
    assert_eq!(conn_a.server_hello.server_id, "server-a");

    // Newest playback intent wins even against a playback incumbent.
    let _peer_b = connect_peer(&url, "server-b", ConnectionReason::Playback).await;
    assert_eq!(
        expect_goodbye(&mut peer_a).await,
        GoodbyeReason::AnotherServer
    );
    let conn_b = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out")
        .expect("manager stopped");
    assert_eq!(conn_b.server_hello.server_id, "server-b");
}

#[tokio::test]
async fn test_drop_aborts_inflight_handshake_promptly() {
    // A long establish deadline: without abort-on-drop tracking, the
    // in-flight handshake task would keep the peer's TCP stream (and the
    // listener) alive for the full 30s after the manager is gone.
    let (url, mgr) = manager(Some(ManagerConfig {
        establish_timeout: Duration::from_secs(30),
        max_concurrent_handshakes: 1,
        ..ManagerConfig::default()
    }))
    .await;

    let addr = url.strip_prefix("ws://").unwrap().to_string();
    let stalled = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("raw TCP connect");
    // Let the accept loop admit the stalled peer into its handshake slot.
    tokio::time::sleep(Duration::from_millis(100)).await;

    drop(mgr);

    // The aborted handshake task must drop the TCP stream promptly: the
    // stalled peer observes EOF/reset well before the 30s deadline.
    let mut buf = [0u8; 1];
    let read = timeout(Duration::from_secs(2), async {
        use tokio::io::AsyncReadExt;
        let mut stream = stalled;
        stream.read(&mut buf).await
    })
    .await;
    match read {
        Ok(Ok(0)) => {}  // clean EOF
        Ok(Err(_)) => {} // reset — also fine
        Ok(Ok(n)) => panic!("unexpected {n} bytes from dropped manager"),
        Err(_) => panic!("handshake task kept the connection alive after manager drop"),
    }
}

#[tokio::test]
async fn test_stalled_handshake_is_reaped_and_frees_slot() {
    // One handshake slot and a short establish deadline: a peer that
    // connects and stalls must be reaped so the next peer can establish.
    let (url, mut mgr) = manager(Some(ManagerConfig {
        establish_timeout: Duration::from_millis(300),
        max_concurrent_handshakes: 1,
        ..ManagerConfig::default()
    }))
    .await;

    // Raw TCP connect that never speaks WebSocket, occupying the only slot.
    let addr = url.strip_prefix("ws://").unwrap().to_string();
    let stalled = tokio::net::TcpStream::connect(&addr)
        .await
        .expect("raw TCP connect");

    // Give the accept loop time to admit the stalled peer into its slot.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A real server connecting behind it must still establish once the
    // stalled peer is reaped at the deadline.
    let _peer = connect_peer(&url, "server-a", ConnectionReason::Playback).await;
    let conn = timeout(Duration::from_secs(5), mgr.next_connection())
        .await
        .expect("next_connection timed out — stalled peer was never reaped")
        .expect("manager stopped");
    assert_eq!(conn.server_hello.server_id, "server-a");

    drop(stalled);
}
