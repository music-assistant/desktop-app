// ABOUTME: Integration tests for ProtocolListener — inbound (server-initiated)
// ABOUTME: WebSocket acceptor that drives the protocol-client state machine.

use futures_util::{SinkExt, StreamExt};
use sendspin::protocol::messages::{
    ClientCommand, ClientSyncState, ConnectionReason, GoodbyeReason, Message, PlayerState,
    ServerHello,
};
use sendspin::ProtocolClientBuilder;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage, WebSocketStream};

/// Test peer that plays the protocol-server role over an already-connected
/// WebSocket stream: replies to `client/hello` with `server/hello`, then
/// forwards every subsequent text frame to the returned channel. Generic over
/// the transport so it serves both the plaintext and TLS connect paths.
async fn run_peer<S>(
    ws: WebSocketStream<S>,
    active_roles: Vec<String>,
) -> (
    Vec<String>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut write, mut read) = ws.split();

    let hello_text = match read.next().await.expect("no hello").expect("ws error") {
        WsMessage::Text(t) => t,
        other => panic!("expected text client/hello, got {:?}", other),
    };
    let parsed: Message = serde_json::from_str(&hello_text).expect("client/hello must deserialize");
    let supported_roles = match parsed {
        Message::ClientHello(h) => h.supported_roles,
        other => panic!("first message should be client/hello, got {:?}", other),
    };

    let server_hello = serde_json::to_string(&Message::ServerHello(ServerHello {
        server_id: "test-server".to_string(),
        name: "Test Server".to_string(),
        version: 1,
        active_roles,
        connection_reason: ConnectionReason::Playback,
    }))
    .unwrap();
    write
        .send(WsMessage::Text(server_hello.into()))
        .await
        .expect("send server/hello");

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        while let Some(Ok(msg)) = read.next().await {
            let text = match msg {
                WsMessage::Text(t) => t,
                WsMessage::Close(_) => break,
                _ => continue,
            };
            if tx.send(text.to_string()).is_err() {
                break;
            }
        }
    });

    (supported_roles, rx, handle)
}

async fn drive_peer_until_state(
    url: &str,
    active_roles: Vec<String>,
) -> (
    Vec<String>,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let (ws, _) = connect_async(url).await.expect("WS connect failed");
    run_peer(ws, active_roles).await
}

async fn bind_listener(builder: ProtocolClientBuilder) -> (String, sendspin::ProtocolListener) {
    let listener = builder.listen("127.0.0.1:0").await.expect("listen failed");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("ws://{addr}");
    (url, listener)
}

/// `true` once a `client/goodbye` arrives; `false` if the peer goes quiet for 2s.
async fn wait_for_goodbye(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> bool {
    while let Ok(Some(text)) = timeout(Duration::from_secs(2), rx.recv()).await {
        if matches!(
            serde_json::from_str::<Message>(&text),
            Ok(Message::ClientGoodbye(_))
        ) {
            return true;
        }
    }
    false
}

#[tokio::test]
async fn test_listen_accepts_and_drives_handshake() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-inbound".to_string())
        .name("Test Inbound".to_string())
        .initial_player_state(PlayerState {
            volume: Some(60),
            muted: Some(false),
            static_delay_ms: Some(10),
            required_lead_time_ms: Some(500),
            min_buffer_ms: Some(500),
            supported_commands: None,
        })
        .build();

    let (url, listener) = bind_listener(builder).await;

    let peer =
        tokio::spawn(
            async move { drive_peer_until_state(&url, vec!["player@v1".to_string()]).await },
        );

    let (client, peer_addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept failed");
    assert!(peer_addr.ip().is_loopback(), "peer addr should be loopback");

    // The application uses these fields for the multi-server keep-or-switch
    // policy described on `ProtocolListener`; the listener must surface them
    // verbatim from `server/hello`.
    let hello = client.server_hello();
    assert_eq!(hello.server_id, "test-server");
    assert_eq!(hello.connection_reason, ConnectionReason::Playback);
    assert_eq!(hello.active_roles, vec!["player@v1".to_string()]);

    let (supported_roles, mut rx, _peer_handle) = peer.await.expect("peer task panicked");
    assert_eq!(supported_roles, vec!["player@v1".to_string()]);

    let first_after_hello = timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for client/state")
        .expect("peer channel closed");
    let parsed: Message =
        serde_json::from_str(&first_after_hello).expect("client/state must deserialize");
    match parsed {
        Message::ClientState(cs) => {
            assert_eq!(cs.state, Some(ClientSyncState::Synchronized));
            let player = cs.player.expect("expected initial player state");
            assert_eq!(player.volume, Some(60));
            assert_eq!(player.static_delay_ms, Some(10));
        }
        other => panic!("expected ClientState as first post-hello frame, got {other:?}"),
    }

    client
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");

    assert!(
        wait_for_goodbye(&mut rx).await,
        "never received client/goodbye over inbound link"
    );
}

#[tokio::test]
async fn test_listen_path_match_accepts() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-path".to_string())
        .name("Test Path".to_string())
        .build();

    let listener = builder
        .listen("127.0.0.1:0")
        .await
        .expect("listen")
        .path("/sendspin");
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/sendspin");

    let peer =
        tokio::spawn(
            async move { drive_peer_until_state(&url, vec!["player@v1".to_string()]).await },
        );
    let (client, _peer_addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept");

    let (_roles, mut rx, _peer_handle) = peer.await.expect("peer task");

    client
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");

    assert!(
        wait_for_goodbye(&mut rx).await,
        "matched-path connection never delivered client/goodbye"
    );
}

/// A path given without a leading slash must still match the `/sendspin` the
/// peer connects to — `request.uri().path()` always starts with `/`, so the
/// un-normalized form would silently reject everyone.
#[tokio::test]
async fn test_listen_path_normalizes_missing_leading_slash() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-path-norm".to_string())
        .name("Test Path Norm".to_string())
        .build();

    let listener = builder
        .listen("127.0.0.1:0")
        .await
        .expect("listen")
        .path("sendspin"); // no leading slash
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{addr}/sendspin");

    let peer =
        tokio::spawn(
            async move { drive_peer_until_state(&url, vec!["player@v1".to_string()]).await },
        );
    let (client, _peer_addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept");
    let (_roles, mut rx, _peer_handle) = peer.await.expect("peer task");

    client
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");
    assert!(
        wait_for_goodbye(&mut rx).await,
        "normalized-path connection never delivered client/goodbye"
    );
}

#[tokio::test]
async fn test_listen_path_mismatch_rejects_but_listener_survives() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-path-reject".to_string())
        .name("Test Path Reject".to_string())
        .build();

    let listener = builder
        .listen("127.0.0.1:0")
        .await
        .expect("listen")
        .path("/sendspin");
    let addr = listener.local_addr().unwrap();
    let bad_url = format!("ws://{addr}/wrong-path");
    let good_url = format!("ws://{addr}/sendspin");

    let bad_peer = tokio::spawn(async move {
        let result = connect_async(&bad_url).await;
        assert!(result.is_err(), "expected handshake to fail on bad path");
    });
    let accept_err = timeout(Duration::from_secs(5), listener.accept()).await;
    assert!(accept_err.is_ok(), "listener.accept did not return");
    assert!(
        accept_err.unwrap().is_err(),
        "expected per-peer Err on bad path"
    );
    bad_peer.await.expect("bad peer task");

    let good_peer = tokio::spawn(async move {
        drive_peer_until_state(&good_url, vec!["player@v1".to_string()]).await
    });
    let (client, _peer_addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("second accept timed out")
        .expect("second accept");
    let _ = good_peer.await.expect("good peer task");

    client
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");
}

#[tokio::test]
async fn test_listen_local_addr_resolves_ephemeral_port() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-local-addr".to_string())
        .name("Test".to_string())
        .build();
    let listener = builder.listen("127.0.0.1:0").await.expect("listen");
    let addr = listener.local_addr().expect("local_addr");
    assert!(
        addr.port() != 0,
        "ephemeral port should resolve to non-zero"
    );
    assert!(addr.ip().is_loopback());
}

/// The send-after-disconnect contract must hold on the inbound path too,
/// not just the outbound `connect()` path.
#[tokio::test]
async fn test_listen_send_message_fails_after_disconnect() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-listen-disc".to_string())
        .name("Test".to_string())
        .build();
    let (url, listener) = bind_listener(builder).await;

    let peer =
        tokio::spawn(
            async move { drive_peer_until_state(&url, vec!["player@v1".to_string()]).await },
        );
    let (client, _addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept");
    let (_roles, _rx, _peer_handle) = peer.await.expect("peer task");

    let conn = client.split();
    // `split()` must carry `server_hello` through to `Connection` — the
    // post-split handle is what most apps actually hold onto, and every
    // field it carries (server_id, connection_reason, active_roles) is on
    // the documented arbitration path.
    assert_eq!(conn.server_hello.server_id, "test-server");
    assert_eq!(
        conn.server_hello.connection_reason,
        ConnectionReason::Playback
    );
    assert_eq!(
        conn.server_hello.active_roles,
        vec!["player@v1".to_string()]
    );
    let sender = conn.sender;
    let guard = conn.guard;
    guard
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");

    let result = sender
        .send_message(Message::ClientCommand(ClientCommand { controller: None }))
        .await;
    assert!(
        result.is_err(),
        "expected Err after disconnect, got {result:?}"
    );
}

/// A peer that completes the WebSocket upgrade but never sends `server/hello`
/// must leave `accept()` pending — it must not spuriously resolve. Callers
/// guard against this with a timeout, which this test stands in for.
#[tokio::test]
async fn test_listen_accept_pending_when_peer_stalls_handshake() {
    let builder = ProtocolClientBuilder::builder()
        .client_id("test-stall".to_string())
        .name("Test".to_string())
        .build();
    let (url, listener) = bind_listener(builder).await;

    let _peer = tokio::spawn(async move {
        let (mut ws, _) = connect_async(&url).await.expect("connect");
        // Read client/hello but deliberately never reply with server/hello,
        // holding the socket open so the SDK blocks awaiting it.
        let _ = ws.next().await;
        tokio::time::sleep(Duration::from_secs(30)).await;
        drop(ws);
    });

    let timed_out = timeout(Duration::from_millis(500), listener.accept())
        .await
        .is_err();
    assert!(timed_out, "accept() resolved before server/hello arrived");
}

/// TLS path: the listener terminates TLS with a runtime-generated self-signed
/// identity and a peer that opts into trusting it completes the full
/// WebSocket + protocol handshake. The rest of the suite only covers
/// plaintext, so this is the sole end-to-end exercise of `tls()` and the TLS
/// branch of `accept()`.
#[cfg(feature = "native-tls")]
#[tokio::test]
async fn test_listen_tls_terminates_and_drives_handshake() {
    use tokio_tungstenite::Connector;

    // Throwaway self-signed localhost identity, password "test". PKCS#12 is
    // the one Identity format native-tls imports reliably across all three
    // backends (SecureTransport / SChannel / OpenSSL); from_pkcs8 is rejected
    // by SecureTransport.
    let identity =
        native_tls::Identity::from_pkcs12(include_bytes!("fixtures/localhost.p12"), "test")
            .expect("build TLS identity from fixture");

    let builder = ProtocolClientBuilder::builder()
        .client_id("test-tls".to_string())
        .name("Test TLS".to_string())
        .build();
    let listener = builder
        .listen("127.0.0.1:0")
        .await
        .expect("listen")
        .tls(identity)
        .expect("tls");
    let addr = listener.local_addr().expect("local_addr");
    let url = format!("wss://{addr}/");

    let peer = tokio::spawn(async move {
        // Self-signed and freshly generated, so the peer must opt out of
        // verification — a test decision, never a runtime one.
        let connector = Connector::NativeTls(
            native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .expect("build TLS connector"),
        );
        let (ws, _) = tokio_tungstenite::connect_async_tls_with_config(
            url.as_str(),
            None,
            false,
            Some(connector),
        )
        .await
        .expect("TLS WebSocket connect");
        run_peer(ws, vec!["player@v1".to_string()]).await
    });

    let (client, peer_addr) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .expect("accept timed out")
        .expect("accept over TLS");
    assert!(peer_addr.ip().is_loopback(), "peer addr should be loopback");

    let (_roles, mut rx, _peer_handle) = peer.await.expect("peer task panicked");

    client
        .disconnect(GoodbyeReason::Shutdown)
        .await
        .expect("disconnect");
    assert!(
        wait_for_goodbye(&mut rx).await,
        "never received client/goodbye over TLS link"
    );
}
