use futures_util::{SinkExt, StreamExt};
use sendspin::error::Error;
use sendspin::protocol::messages::{
    ArtworkFormatRequest, ArtworkSource, AudioFormatSpec, ClientCommand, ClientSyncState,
    ConnectionReason, ControllerCommandType, GoodbyeReason, ImageFormat, Message,
    PlayerFormatRequest, PlayerState, PlayerV1Support, RepeatMode, StreamArtworkChannelConfig,
    StreamArtworkConfig, StreamEnd, StreamPlayerConfig, StreamRequestFormat, StreamStart,
    VisualizerDataType, VisualizerFormatRequest,
};
use sendspin::ProtocolClientBuilder;
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message as WsMessage};

/// Start a local WebSocket server that performs the sendspin handshake
/// and returns the listener address and a channel to receive messages
/// the client sends after the handshake.
async fn start_test_server() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        // Receive client/hello and echo back its roles
        let msg = ws.next().await.unwrap().unwrap();
        let active_roles = if let WsMessage::Text(ref text) = msg {
            match serde_json::from_str::<Message>(text).unwrap() {
                Message::ClientHello(hello) => hello.supported_roles,
                other => panic!("First message should be client/hello, got {:?}", other),
            }
        } else {
            panic!("Expected text message for client/hello");
        };

        // Send server/hello with the roles the client requested
        let server_hello = serde_json::to_string(&Message::ServerHello(
            sendspin::protocol::messages::ServerHello {
                server_id: "test-server".to_string(),
                name: "Test Server".to_string(),
                version: 1,
                active_roles,
                connection_reason: sendspin::protocol::messages::ConnectionReason::Playback,
            },
        ))
        .unwrap();
        ws.send(WsMessage::Text(server_hello.into())).await.unwrap();

        // Forward all subsequent messages to the channel
        while let Some(Ok(msg)) = ws.next().await {
            let text = match msg {
                WsMessage::Text(text) => text,
                WsMessage::Close(_) => break,
                _ => continue,
            };
            if tx.send(text.to_string()).is_err() {
                break;
            }
        }
    });

    (url, rx, handle)
}

/// Serialize a protocol message into a WebSocket text frame.
fn text_message(msg: &Message) -> WsMessage {
    WsMessage::Text(serde_json::to_string(msg).unwrap().into())
}

/// A server→client `stream/start` that starts a player stream.
fn player_stream_start() -> WsMessage {
    text_message(&Message::StreamStart(StreamStart {
        player: Some(StreamPlayerConfig {
            codec: "pcm".to_string(),
            sample_rate: 48_000,
            channels: 2,
            bit_depth: 24,
            codec_header: None,
        }),
        artwork: None,
        visualizer: None,
    }))
}

/// A server→client `stream/start` that starts an artwork stream.
fn artwork_stream_start() -> WsMessage {
    text_message(&Message::StreamStart(StreamStart {
        player: None,
        artwork: Some(StreamArtworkConfig {
            channels: vec![StreamArtworkChannelConfig {
                source: ArtworkSource::Artist,
                format: ImageFormat::Png,
                width: 640,
                height: 640,
            }],
        }),
        visualizer: None,
    }))
}

/// A server→client `stream/start` that starts a visualizer stream.
fn visualizer_stream_start() -> WsMessage {
    text_message(&Message::StreamStart(StreamStart {
        player: None,
        artwork: None,
        visualizer: Some(sendspin::protocol::messages::StreamVisualizerConfig {
            types: vec![VisualizerDataType::Spectrum],
            rate_max: 60,
            tracks_downbeats: None,
            spectrum: None,
        }),
    }))
}

/// A server→client `stream/end` for the given roles (all streams if `None`).
fn stream_end(roles: Option<Vec<String>>) -> WsMessage {
    text_message(&Message::StreamEnd(StreamEnd { roles }))
}

/// Discard the `client/state` the client emits immediately after handshake.
async fn discard_initial_state(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for initial state")
        .expect("channel closed");
}

/// Drain forwarded protocol messages until one matches `want`. The router
/// settles stream state *before* forwarding `stream/start` / `stream/end`, so
/// observing the awaited message here guarantees the request-format gate has
/// settled too.
async fn await_message(
    messages: &mut tokio::sync::mpsc::UnboundedReceiver<Message>,
    want: impl Fn(&Message) -> bool,
) {
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), messages.recv())
            .await
            .expect("timed out waiting for message")
            .expect("messages channel closed");
        if want(&msg) {
            return;
        }
    }
}

async fn recv_stream_request_format(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> StreamRequestFormat {
    loop {
        let msg_text = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for stream/request-format")
            .expect("channel closed");
        match serde_json::from_str::<Message>(&msg_text).unwrap() {
            Message::StreamRequestFormat(request) => break request,
            Message::ClientTime(_) => continue,
            other => panic!("expected StreamRequestFormat, got {:?}", other),
        }
    }
}

#[tokio::test]
async fn test_connect_sends_initial_state() {
    let (url, mut rx, _handle) = start_test_server().await;

    let builder = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .initial_player_state(PlayerState {
            volume: Some(75),
            muted: Some(false),
            static_delay_ms: Some(100),
            required_lead_time_ms: Some(500),
            min_buffer_ms: Some(500),
            supported_commands: None,
        })
        .build();

    let client = builder.connect(&url).await.unwrap();

    // Regression guard: the server_hello fields needed for multi-server
    // arbitration must survive `connect()` just as they do `accept()`.
    let hello = client.server_hello();
    assert_eq!(hello.server_id, "test-server");
    assert_eq!(hello.connection_reason, ConnectionReason::Playback);

    // First message after handshake should be client/state
    let first_msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for message")
        .expect("channel closed");

    let parsed: Message = serde_json::from_str(&first_msg).unwrap();
    match parsed {
        Message::ClientState(cs) => {
            assert_eq!(cs.state, Some(ClientSyncState::Synchronized));
            let player = cs.player.expect("expected player state");
            assert_eq!(player.volume, Some(75));
            assert_eq!(player.muted, Some(false));
            assert_eq!(player.static_delay_ms, Some(100));
        }
        other => panic!("expected ClientState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_connect_without_player_state_sends_state_without_player() {
    let (url, mut rx, _handle) = start_test_server().await;

    let builder = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build();

    let _client = builder.connect(&url).await.unwrap();

    // Builder always sends client/state (with state=Synchronized), even without player state
    let first_msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for message")
        .expect("channel closed");

    let parsed: Message = serde_json::from_str(&first_msg).unwrap();
    match parsed {
        Message::ClientState(cs) => {
            assert_eq!(cs.state, Some(ClientSyncState::Synchronized));
            assert!(cs.player.is_none(), "expected no player state");
        }
        other => panic!("expected ClientState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_connect_can_send_initial_external_source_state() {
    let (url, mut rx, _handle) = start_test_server().await;

    let builder = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .initial_sync_state(ClientSyncState::ExternalSource)
        .build();

    let _client = builder.connect(&url).await.unwrap();

    let first_msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for message")
        .expect("channel closed");

    let parsed: Message = serde_json::from_str(&first_msg).unwrap();
    match parsed {
        Message::ClientState(cs) => {
            assert_eq!(cs.state, Some(ClientSyncState::ExternalSource));
            assert!(cs.player.is_none(), "expected no player state");
        }
        other => panic!("expected ClientState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_connect_initial_external_source_preserves_player_state() {
    let (url, mut rx, _handle) = start_test_server().await;

    let builder = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .initial_sync_state(ClientSyncState::ExternalSource)
        .initial_player_state(PlayerState {
            volume: Some(23),
            muted: Some(true),
            static_delay_ms: Some(250),
            required_lead_time_ms: Some(500),
            min_buffer_ms: Some(500),
            supported_commands: None,
        })
        .build();

    let _client = builder.connect(&url).await.unwrap();

    let first_msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for message")
        .expect("channel closed");

    let parsed: Message = serde_json::from_str(&first_msg).unwrap();
    match parsed {
        Message::ClientState(cs) => {
            assert_eq!(cs.state, Some(ClientSyncState::ExternalSource));
            let player = cs.player.expect("expected player state");
            assert_eq!(player.volume, Some(23));
            assert_eq!(player.muted, Some(true));
            assert_eq!(player.static_delay_ms, Some(250));
        }
        other => panic!("expected ClientState, got {:?}", other),
    }
}

#[tokio::test]
async fn test_disconnect_sends_goodbye() {
    let (url, mut rx, _handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    client.disconnect(GoodbyeReason::Shutdown).await.unwrap();

    // Drain messages until we find client/goodbye
    let mut found_goodbye = false;
    while let Ok(Some(msg_text)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
    {
        if let Ok(Message::ClientGoodbye(goodbye)) = serde_json::from_str::<Message>(&msg_text) {
            assert_eq!(goodbye.reason, GoodbyeReason::Shutdown);
            found_goodbye = true;
            break;
        }
    }
    assert!(found_goodbye, "never received client/goodbye");
}

#[tokio::test]
async fn test_sender_emits_stream_request_format() {
    let (url, mut rx, server_tx, _handle) = start_test_server_with_sender().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    discard_initial_state(&mut rx).await;

    server_tx.send(player_stream_start()).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamStart(_))).await;

    conn.sender
        .request_player_format(PlayerFormatRequest {
            codec: Some("pcm".to_string()),
            channels: Some(2),
            sample_rate: Some(48_000),
            bit_depth: Some(16),
        })
        .await
        .unwrap();

    let request = recv_stream_request_format(&mut rx).await;

    let player = request.player.expect("expected player format request");
    assert_eq!(player.codec.as_deref(), Some("pcm"));
    assert_eq!(player.channels, Some(2));
    assert_eq!(player.sample_rate, Some(48_000));
    assert_eq!(player.bit_depth, Some(16));
    assert!(request.artwork.is_none());
}

#[tokio::test]
async fn test_sender_emits_artwork_stream_request_format() {
    let (url, mut rx, server_tx, _handle) = start_test_server_with_sender().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    discard_initial_state(&mut rx).await;

    server_tx.send(artwork_stream_start()).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamStart(_))).await;

    conn.sender
        .request_artwork_format(ArtworkFormatRequest {
            channel: 1,
            source: Some(ArtworkSource::Artist),
            format: Some(ImageFormat::Png),
            media_width: Some(400),
            media_height: Some(300),
        })
        .await
        .unwrap();

    let request = recv_stream_request_format(&mut rx).await;

    assert!(request.player.is_none());
    let artwork = request.artwork.expect("expected artwork format request");
    assert_eq!(artwork.channel, 1);
    assert_eq!(artwork.source, Some(ArtworkSource::Artist));
    assert_eq!(artwork.format, Some(ImageFormat::Png));
    assert_eq!(artwork.media_width, Some(400));
    assert_eq!(artwork.media_height, Some(300));
}

#[tokio::test]
async fn test_sender_rejects_empty_stream_request_format() {
    let (url, mut rx, _handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let conn = client.split();

    // Discard the initial client/state emitted immediately after handshake.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for initial state")
        .expect("channel closed");

    let result = conn.sender.request_stream_format(None, None).await;

    match result {
        Err(Error::Protocol(msg)) => assert!(
            msg.contains("requires a player, artwork, or visualizer request"),
            "unexpected protocol error: {msg}"
        ),
        other => panic!("expected protocol error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_sender_rejects_request_format_before_stream_start() {
    // No stream/start: the server never starts a stream, so the gate stays shut.
    let (url, mut rx, _handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let conn = client.split();

    discard_initial_state(&mut rx).await;

    let result = conn
        .sender
        .request_player_format(PlayerFormatRequest {
            codec: Some("pcm".to_string()),
            channels: Some(2),
            sample_rate: Some(48_000),
            bit_depth: Some(16),
        })
        .await;

    match result {
        Err(Error::Protocol(msg)) => assert!(
            msg.contains("active player stream"),
            "unexpected protocol error: {msg}"
        ),
        other => panic!("expected protocol error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_sender_visualizer_request_requires_active_stream() {
    let (url, mut rx, server_tx, _handle) = start_test_server_with_sender().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .visualizer_v1_support(sendspin::protocol::messages::VisualizerV1Support {
            types: vec![VisualizerDataType::Spectrum],
            buffer_capacity: 4096,
            rate_max: 60,
            spectrum: None,
        })
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();
    discard_initial_state(&mut rx).await;

    let request = VisualizerFormatRequest {
        types: Some(vec![VisualizerDataType::Loudness]),
        rate_max: Some(30),
        spectrum: None,
    };
    let empty = VisualizerFormatRequest {
        types: None,
        rate_max: None,
        spectrum: None,
    };
    let result = conn.sender.request_visualizer_format(empty).await;
    assert!(matches!(result, Err(Error::Protocol(msg)) if msg.contains("at least one field")));

    let result = conn.sender.request_visualizer_format(request.clone()).await;
    assert!(
        matches!(result, Err(Error::Protocol(msg)) if msg.contains("active visualizer stream"))
    );

    server_tx.send(visualizer_stream_start()).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamStart(_))).await;

    conn.sender
        .request_visualizer_format(request)
        .await
        .unwrap();
    let sent = recv_stream_request_format(&mut rx).await;
    assert_eq!(
        sent.visualizer.expect("visualizer request").rate_max,
        Some(30)
    );
}

#[tokio::test]
async fn test_sender_rejects_player_request_after_stream_end() {
    // The player stream starts then ends, so the gate closes again.
    let (url, mut rx, server_tx, _handle) = start_test_server_with_sender().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    discard_initial_state(&mut rx).await;

    server_tx.send(player_stream_start()).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamStart(_))).await;

    server_tx.send(stream_end(None)).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamEnd(_))).await;

    let result = conn
        .sender
        .request_player_format(PlayerFormatRequest {
            codec: Some("pcm".to_string()),
            channels: Some(2),
            sample_rate: Some(48_000),
            bit_depth: Some(16),
        })
        .await;

    match result {
        Err(Error::Protocol(msg)) => assert!(
            msg.contains("active player stream"),
            "unexpected protocol error: {msg}"
        ),
        other => panic!("expected protocol error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_sender_rejects_artwork_request_without_artwork_stream() {
    // Only a player stream is active; an artwork request has nothing to
    // renegotiate and must be rejected for its own role.
    let (url, mut rx, server_tx, _handle) = start_test_server_with_sender().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    discard_initial_state(&mut rx).await;

    server_tx.send(player_stream_start()).unwrap();
    await_message(&mut conn.messages, |m| matches!(m, Message::StreamStart(_))).await;

    let result = conn
        .sender
        .request_artwork_format(ArtworkFormatRequest {
            channel: 1,
            source: Some(ArtworkSource::Artist),
            format: Some(ImageFormat::Png),
            media_width: Some(400),
            media_height: Some(300),
        })
        .await;

    match result {
        Err(Error::Protocol(msg)) => assert!(
            msg.contains("active artwork stream"),
            "unexpected protocol error: {msg}"
        ),
        other => panic!("expected protocol error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_disconnect_closes_socket_and_stops_background_tasks() {
    let (url, mut rx, handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    // Let clock sync send at least one client/time
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    client.disconnect(GoodbyeReason::Shutdown).await.unwrap();

    // Drain remaining messages — should find goodbye then channel closes
    let mut found_goodbye = false;
    let mut messages_after_goodbye = 0;
    while let Ok(Some(msg_text)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
    {
        if found_goodbye {
            messages_after_goodbye += 1;
        }
        if let Ok(Message::ClientGoodbye(_)) = serde_json::from_str::<Message>(&msg_text) {
            found_goodbye = true;
        }
    }

    assert!(found_goodbye, "never received client/goodbye");

    assert_eq!(
        messages_after_goodbye, 0,
        "no messages should arrive after goodbye"
    );

    // Server task should have exited (socket closed)
    let server_exited = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .is_ok();
    assert!(
        server_exited,
        "server task did not exit — socket not closed"
    );
}

#[tokio::test]
async fn test_connection_disconnect_sends_goodbye_and_closes() {
    let (url, mut rx, handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let conn = client.split();
    let guard = conn.guard;

    // Let clock sync send at least one client/time
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    guard.disconnect(GoodbyeReason::Shutdown).await.unwrap();

    // Drain remaining messages — should find goodbye then channel closes
    let mut found_goodbye = false;
    let mut messages_after_goodbye = 0;
    while let Ok(Some(msg_text)) =
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await
    {
        if found_goodbye {
            messages_after_goodbye += 1;
        }
        if let Ok(Message::ClientGoodbye(_)) = serde_json::from_str::<Message>(&msg_text) {
            found_goodbye = true;
        }
    }

    assert!(found_goodbye, "never received client/goodbye");

    assert_eq!(
        messages_after_goodbye, 0,
        "no messages should arrive after goodbye"
    );

    // Server task should have exited (socket closed)
    let server_exited = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .is_ok();
    assert!(
        server_exited,
        "server task did not exit — socket not closed"
    );
}

/// Helper: connect with controller role, return the rx channel and a Controller handle
async fn connect_with_controller() -> (
    tokio::sync::mpsc::UnboundedReceiver<String>,
    sendspin::protocol::client::Controller,
    sendspin::protocol::client::Connection,
    tokio::task::JoinHandle<()>,
) {
    let (url, rx, handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .controller()
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();
    let controller = conn
        .controller
        .take()
        .expect("should have controller when role declared");

    (rx, controller, conn, handle)
}

/// Helper: drain client/time messages and return the next client/command
async fn next_client_command(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> ClientCommand {
    loop {
        let msg_text = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for message")
            .expect("channel closed");

        let parsed: Message = serde_json::from_str(&msg_text).unwrap();
        if let Message::ClientCommand(cmd) = parsed {
            return cmd;
        }
    }
}

async fn next_client_state(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> sendspin::protocol::messages::ClientState {
    loop {
        let msg_text = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for message")
            .expect("channel closed");

        let parsed: Message = serde_json::from_str(&msg_text).unwrap();
        if let Message::ClientState(state) = parsed {
            return state;
        }
    }
}

#[tokio::test]
async fn test_enter_external_source_sends_external_source_state() {
    let (url, mut rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    let initial = next_client_state(&mut rx).await;
    assert_eq!(initial.state, Some(ClientSyncState::Synchronized));

    client.enter_external_source().await.unwrap();

    let state = next_client_state(&mut rx).await;
    assert_eq!(state.state, Some(ClientSyncState::ExternalSource));
    assert!(state.player.is_none());
}

#[tokio::test]
async fn test_exit_external_source_sends_synchronized_state() {
    let (url, mut rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    let initial = next_client_state(&mut rx).await;
    assert_eq!(initial.state, Some(ClientSyncState::Synchronized));

    client.exit_external_source(None).await.unwrap();

    let state = next_client_state(&mut rx).await;
    assert_eq!(state.state, Some(ClientSyncState::Synchronized));
    assert!(state.player.is_none());
}

#[tokio::test]
async fn test_exit_external_source_sends_full_player_state() {
    let (url, mut rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    let _initial = next_client_state(&mut rx).await;

    client
        .exit_external_source(Some(PlayerState {
            volume: Some(42),
            muted: Some(true),
            ..Default::default()
        }))
        .await
        .unwrap();

    let state = next_client_state(&mut rx).await;
    assert_eq!(state.state, Some(ClientSyncState::Synchronized));
    let player = state.player.expect("player state should be present");
    assert_eq!(player.volume, Some(42));
    assert_eq!(player.muted, Some(true));
}

// Macro to generate tests for simple controller commands (no parameters).
// Defined once to prevent copy-paste typo errors across the six methods.
macro_rules! test_simple_controller_command {
    ($name:ident, $method:ident, $expected:expr) => {
        #[tokio::test]
        async fn $name() {
            let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
            controller.$method().await.unwrap();

            let cmd = next_client_command(&mut rx).await;
            let ctrl = cmd.controller.expect("expected controller command");
            assert_eq!(ctrl.command, $expected);
        }
    };
}

test_simple_controller_command!(test_controller_play, play, ControllerCommandType::Play);
test_simple_controller_command!(test_controller_pause, pause, ControllerCommandType::Pause);
test_simple_controller_command!(test_controller_stop, stop, ControllerCommandType::Stop);
test_simple_controller_command!(test_controller_next, next, ControllerCommandType::Next);
test_simple_controller_command!(
    test_controller_previous,
    previous,
    ControllerCommandType::Previous
);
test_simple_controller_command!(
    test_controller_switch,
    switch,
    ControllerCommandType::Switch
);

#[tokio::test]
async fn test_controller_set_volume_sends_value() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.set_volume(25).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::Volume);
    assert_eq!(ctrl.volume, Some(25));
}

#[tokio::test]
async fn test_controller_set_volume_clamps_above_100() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.set_volume(200).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.volume, Some(100));
}

#[tokio::test]
async fn test_controller_set_mute_sends_value() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.set_mute(true).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::Mute);
    assert_eq!(ctrl.mute, Some(true));
}

#[tokio::test]
async fn test_controller_repeat_sends_mode() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.repeat(RepeatMode::All).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::RepeatAll);
}

#[tokio::test]
async fn test_controller_shuffle_sends_correct_command() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.shuffle(true).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::Shuffle);
}

#[tokio::test]
async fn test_controller_unshuffle_sends_correct_command() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.shuffle(false).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::Unshuffle);
}

#[tokio::test]
async fn test_controller_seek_sends_position() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.seek(123_456).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::Seek);
    assert_eq!(ctrl.position_ms, Some(123_456));
    assert!(ctrl.offset_ms.is_none());
}

#[tokio::test]
async fn test_controller_seek_relative_sends_offset() {
    let (mut rx, controller, _conn, _handle) = connect_with_controller().await;
    controller.seek_relative(-15_000).await.unwrap();

    let cmd = next_client_command(&mut rx).await;
    let ctrl = cmd.controller.expect("expected controller command");
    assert_eq!(ctrl.command, ControllerCommandType::SeekRelative);
    assert_eq!(ctrl.offset_ms, Some(-15_000));
    assert!(ctrl.position_ms.is_none());
}

/// Start a test server that only grants specific roles, ignoring what the client requests.
async fn start_test_server_with_roles(
    granted_roles: Vec<String>,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        // Receive and discard client/hello
        let _ = ws.next().await.unwrap().unwrap();

        // Send server/hello with only the granted roles
        let server_hello = serde_json::to_string(&Message::ServerHello(
            sendspin::protocol::messages::ServerHello {
                server_id: "test-server".to_string(),
                name: "Test Server".to_string(),
                version: 1,
                active_roles: granted_roles,
                connection_reason: sendspin::protocol::messages::ConnectionReason::Playback,
            },
        ))
        .unwrap();
        ws.send(WsMessage::Text(server_hello.into())).await.unwrap();

        while let Some(Ok(msg)) = ws.next().await {
            let text = match msg {
                WsMessage::Text(text) => text,
                WsMessage::Close(_) => break,
                _ => continue,
            };
            if tx.send(text.to_string()).is_err() {
                break;
            }
        }
    });

    (url, rx, handle)
}

#[tokio::test]
async fn test_no_controller_when_server_denies_role() {
    let (url, _rx, _handle) = start_test_server_with_roles(vec!["player@v1".to_string()]).await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .controller()
        .build()
        .connect(&url)
        .await
        .unwrap();

    let conn = client.split();
    assert!(
        conn.controller.is_none(),
        "should not have controller when server denies the role"
    );
    // Post-split outbound coverage of server_hello, and the active_roles
    // assertion in particular: the same vec drives both controller
    // materialization (above) and the multi-server arbitration policy.
    assert_eq!(conn.server_hello.server_id, "test-server");
    assert_eq!(
        conn.server_hello.active_roles,
        vec!["player@v1".to_string()]
    );
}

#[tokio::test]
async fn test_no_controller_when_role_not_declared() {
    let (url, _rx, _handle) = start_test_server().await;

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    let conn = client.split();
    assert!(
        conn.controller.is_none(),
        "should not have controller without role"
    );
}

/// Variant of [`start_test_server`] that drives the server side directly —
/// exposes both the send half (for server-initiated messages) and the receive
/// half (for client-sent messages).
async fn start_test_server_with_sender() -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<String>,
    tokio::sync::mpsc::UnboundedSender<WsMessage>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    let (client_tx, client_rx) = tokio::sync::mpsc::unbounded_channel();
    let (server_send_tx, mut server_send_rx) = tokio::sync::mpsc::unbounded_channel::<WsMessage>();

    let handle = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();

        // Drive handshake: consume client/hello, echo roles back.
        let msg = ws.next().await.unwrap().unwrap();
        let active_roles = if let WsMessage::Text(ref text) = msg {
            match serde_json::from_str::<Message>(text).unwrap() {
                Message::ClientHello(hello) => hello.supported_roles,
                other => panic!("First message should be client/hello, got {:?}", other),
            }
        } else {
            panic!("Expected text message for client/hello");
        };
        let server_hello = serde_json::to_string(&Message::ServerHello(
            sendspin::protocol::messages::ServerHello {
                server_id: "test-server".to_string(),
                name: "Test Server".to_string(),
                version: 1,
                active_roles,
                connection_reason: sendspin::protocol::messages::ConnectionReason::Playback,
            },
        ))
        .unwrap();
        ws.send(WsMessage::Text(server_hello.into())).await.unwrap();

        // Fan in: forward client messages to `client_tx`, forward outgoing
        // `server_send_rx` messages to the socket.
        loop {
            tokio::select! {
                msg = ws.next() => {
                    let Some(Ok(msg)) = msg else { break };
                    let text = match msg {
                        WsMessage::Text(text) => text,
                        WsMessage::Close(_) => break,
                        _ => continue,
                    };
                    if client_tx.send(text.to_string()).is_err() { break }
                }
                outgoing = server_send_rx.recv() => {
                    let Some(out) = outgoing else { break };
                    if ws.send(out).await.is_err() { break }
                }
            }
        }
    });

    (url, client_rx, server_send_tx, handle)
}

#[tokio::test]
async fn test_clock_sync_getter_returns_shared_handle() {
    // `clock_sync()` must return the *same* `Arc<Mutex<ClockSync>>` instance
    // on every call — callers rely on shared state with the background time
    // sync task. Two calls returning pointer-distinct handles would compile
    // fine but silently break sync state sharing; `Arc::ptr_eq` pins this.
    let (url, _rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();

    let a = client.clock_sync();
    let b = client.clock_sync();
    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "clock_sync() must return the same shared handle on every call"
    );
}

#[tokio::test]
async fn test_message_router_forwards_binary_audio() {
    // End-to-end: a binary audio frame routes into `conn.audio`.
    let (url, _rx, server_tx, _handle) = start_test_server_with_sender().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .player_v1_support(PlayerV1Support {
            supported_formats: vec![AudioFormatSpec {
                codec: "pcm".to_string(),
                channels: 2,
                sample_rate: 48000,
                bit_depth: 24,
            }],
            buffer_capacity: 1024,
            supported_commands: vec![],
        })
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    // Audio chunk: type 4, 8-byte big-endian timestamp, then payload.
    let audio_frame: Vec<u8> = vec![
        0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2A, 0xDE, 0xAD,
    ];
    server_tx
        .send(WsMessage::Binary(audio_frame.into()))
        .unwrap();

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), conn.audio.recv())
        .await
        .expect("timed out waiting for audio chunk")
        .expect("audio channel closed");
    assert_eq!(chunk.timestamp, 42);
    assert_eq!(&*chunk.data, &[0xDE, 0xAD]);
}

#[tokio::test]
async fn test_message_router_forwards_binary_artwork() {
    // Mirror of the audio test for the artwork path. Each binary channel has
    // its own closed-flag and send wiring, so each needs separate coverage.
    let (url, _rx, server_tx, _handle) = start_test_server_with_sender().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    // Artwork channel 2: type 0x0A, 8-byte big-endian timestamp, then payload.
    let artwork_frame: Vec<u8> = vec![
        0x0A, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0xFF, 0xD8, 0xFF, 0xE0,
    ];
    server_tx
        .send(WsMessage::Binary(artwork_frame.into()))
        .unwrap();

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), conn.artwork.recv())
        .await
        .expect("timed out waiting for artwork chunk")
        .expect("artwork channel closed");
    assert_eq!(chunk.channel, 2);
    assert_eq!(chunk.timestamp, 100);
    assert_eq!(&*chunk.data, &[0xFF, 0xD8, 0xFF, 0xE0]);
}

#[tokio::test]
async fn test_message_router_forwards_binary_visualizer() {
    // Mirror of the audio test for the visualizer path (binary type 16).
    let (url, _rx, server_tx, _handle) = start_test_server_with_sender().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    // Visualizer: type 0x10, 8-byte big-endian timestamp, then FFT data.
    let vis_frame: Vec<u8> = vec![
        0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC8, 0x01, 0x02, 0x03,
    ];
    server_tx.send(WsMessage::Binary(vis_frame.into())).unwrap();

    let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), conn.visualizer.recv())
        .await
        .expect("timed out waiting for visualizer chunk")
        .expect("visualizer channel closed");
    assert_eq!(chunk.type_id, 0x10);
    assert_eq!(chunk.timestamp, 200);
    assert_eq!(&*chunk.data, &[0x01, 0x02, 0x03]);

    for (type_id, expected) in [(0x11, vec![0x01]), (0x13, vec![0x02, 0x03])] {
        server_tx
            .send(WsMessage::Binary(
                [vec![type_id], vec![0; 8], expected.clone()]
                    .concat()
                    .into(),
            ))
            .unwrap();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), conn.visualizer.recv())
            .await
            .expect("timed out waiting for visualizer chunk")
            .expect("visualizer channel closed");
        assert_eq!(chunk.type_id, type_id);
        assert_eq!(&*chunk.data, expected.as_slice());
    }
}

#[tokio::test]
async fn test_message_router_forwards_text_messages() {
    // End-to-end: a server text message routes into `conn.messages`.
    let (url, _rx, server_tx, _handle) = start_test_server_with_sender().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    let server_state = serde_json::to_string(&Message::ServerState(
        sendspin::protocol::messages::ServerState {
            metadata: None,
            controller: None,
            color: None,
        },
    ))
    .unwrap();
    server_tx
        .send(WsMessage::Text(server_state.into()))
        .unwrap();

    let msg = tokio::time::timeout(std::time::Duration::from_secs(2), conn.messages.recv())
        .await
        .expect("timed out waiting for server message")
        .expect("message channel closed");
    match msg {
        Message::ServerState(_) => {}
        other => panic!("expected ServerState, got {other:?}"),
    }
}

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

/// Contract: `send_message` returns `Err` once the writer task is gone.
/// Without this, callers could see `Ok` for sends that never reached the wire.
#[tokio::test]
async fn test_send_message_fails_after_disconnect() {
    let (url, mut rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .controller()
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();
    let controller = conn
        .controller
        .take()
        .expect("controller granted by test server");
    let guard = conn.guard;

    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

    guard.disconnect(GoodbyeReason::Shutdown).await.unwrap();

    let result = controller.play().await;
    assert!(result.is_err(), "expected Err, got {:?}", result);
}

/// `disconnect` must not return until the writer has actually exited,
/// across every clone of the sender handle.
#[tokio::test]
async fn test_disconnect_observes_writer_completion() {
    let (url, mut rx, _handle) = start_test_server().await;
    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .controller()
        .build()
        .connect(&url)
        .await
        .unwrap();

    let mut conn = client.split();
    let controller = conn
        .controller
        .take()
        .expect("controller granted by test server");
    let sender_clone = conn.sender.clone();
    let guard = conn.guard;

    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

    guard.disconnect(GoodbyeReason::Shutdown).await.unwrap();

    assert!(controller.play().await.is_err());
    assert!(sender_clone
        .send_message(Message::ClientCommand(ClientCommand { controller: None }))
        .await
        .is_err());
}

/// When the peer sends a WS Close mid-stream, the router must terminate the
/// message stream, and sends must eventually fail once the writer hits the
/// dead socket. This is the primary real-world disconnect path.
#[tokio::test]
async fn test_peer_close_midstream_ends_stream_and_fails_sends() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("ws://{}", addr);

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(stream).await.unwrap();
        let _ = ws.next().await; // client/hello
        let hello = serde_json::to_string(&Message::ServerHello(
            sendspin::protocol::messages::ServerHello {
                server_id: "test-server".to_string(),
                name: "Test Server".to_string(),
                version: 1,
                active_roles: vec!["player@v1".to_string()],
                connection_reason: sendspin::protocol::messages::ConnectionReason::Playback,
            },
        ))
        .unwrap();
        ws.send(WsMessage::Text(hello.into())).await.unwrap();
        let _ = ws.next().await; // initial client/state
        ws.close(None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    });

    let client = ProtocolClientBuilder::builder()
        .client_id("test-client".to_string())
        .name("Test".to_string())
        .build()
        .connect(&url)
        .await
        .unwrap();
    let mut conn = client.split();

    let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), conn.messages.recv())
        .await
        .expect("message stream did not terminate after peer close");
    assert!(
        terminal.is_none(),
        "expected message channel to close (None) after peer WS close, got {terminal:?}"
    );

    // The first send may race ahead of the close registering locally; retry a
    // bounded number of times until the writer observes the dead socket.
    let mut last = Ok(());
    for _ in 0..20 {
        last = conn
            .sender
            .send_message(Message::ClientCommand(ClientCommand { controller: None }))
            .await;
        if last.is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        last.is_err(),
        "send eventually fails after peer close, got {last:?}"
    );
}

/// A send racing a disconnect must resolve cleanly — never deadlock, never
/// panic. disconnect succeeds (peer is alive, goodbye flushes); the racing
/// send either lands before the goodbye (Ok) or is rejected because the
/// writer already exited (Err). Both are correct.
#[tokio::test]
async fn test_concurrent_send_during_disconnect_is_clean() {
    let (mut rx, controller, conn, _handle) = connect_with_controller().await;
    let guard = conn.guard;

    let _ = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;

    let (send_res, disc_res) =
        tokio::join!(controller.play(), guard.disconnect(GoodbyeReason::Shutdown),);

    disc_res.expect("disconnect should succeed against a live peer");
    // send_res is intentionally not asserted: Ok (landed before goodbye) and
    // Err (writer already exited) are both correct. The contract under test is
    // that join! returned at all — neither future deadlocked.
    let _ = send_res;
}
