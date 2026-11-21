// ABOUTME: Managed connection lifecycle: concurrent inbound handshakes, spec
// ABOUTME: multi-server arbitration, and automatic goodbye(another_server).

use crate::error::Error;
use crate::protocol::client::{
    ArtworkChunk, AudioChunk, Connection, ConnectionGuard, Controller, VisualizerChunk, WsSender,
};
use crate::protocol::listener::ProtocolListener;
use crate::protocol::messages::{
    ConnectionReason, GoodbyeReason, Message, PlayerState, ServerHello,
};
use crate::sync::ClockSync;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::{oneshot, Semaphore};
use tokio::task::JoinHandle;

/// The spec's multi-server arbitration rule: should the newly established
/// `candidate` displace the `current` server?
///
/// [`ConnectionManager`] applies this automatically; it is public so
/// applications running their own [`ProtocolListener::accept`] loop can make
/// the identical decision.
pub fn should_switch(
    current: &ServerHello,
    candidate: &ServerHello,
    last_played: Option<&str>,
) -> bool {
    // Matched as (candidate, current) — reverse of the parameter order —
    // so the new server reads first in each arm.
    match (&candidate.connection_reason, &current.connection_reason) {
        (ConnectionReason::Playback, _) => true,
        (ConnectionReason::Discovery, ConnectionReason::Playback) => false,
        (ConnectionReason::Discovery, ConnectionReason::Discovery) => {
            matches!(last_played, Some(lp) if candidate.server_id == lp)
        }
    }
}

/// Default [`ManagerConfig::establish_timeout`]
pub const DEFAULT_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Default [`ManagerConfig::max_concurrent_handshakes`]
pub const DEFAULT_MAX_CONCURRENT_HANDSHAKES: usize = 2;

/// Default [`ManagerConfig::goodbye_timeout`].
pub const DEFAULT_GOODBYE_TIMEOUT: Duration = Duration::from_secs(5);

/// Tuning for [`ConnectionManager`].
#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// Deadline for an inbound connection to complete TLS + WebSocket +
    /// protocol handshake, measured from TCP accept. Peers that stall are
    /// dropped. Default: [`DEFAULT_ESTABLISH_TIMEOUT`].
    pub establish_timeout: Duration,
    /// Maximum number of inbound connections concurrently working through
    /// their handshake. Surplus peers wait in the TCP accept backlog rather
    /// than being rejected. Values below 1 are clamped to 1. Default:
    /// [`DEFAULT_MAX_CONCURRENT_HANDSHAKES`].
    pub max_concurrent_handshakes: usize,
    /// Deadline for flushing `client/goodbye` to a losing, displaced, or
    /// disconnected server. On elapse the connection is torn down without
    /// confirmation — a peer that stops reading would otherwise pin the
    /// flush (and its connection's tasks and socket) until the OS TCP
    /// timeout. Default: [`DEFAULT_GOODBYE_TIMEOUT`].
    pub goodbye_timeout: Duration,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            establish_timeout: DEFAULT_ESTABLISH_TIMEOUT,
            max_concurrent_handshakes: DEFAULT_MAX_CONCURRENT_HANDSHAKES,
            goodbye_timeout: DEFAULT_GOODBYE_TIMEOUT,
        }
    }
}

/// Identical in shape to [`Connection`] except the [`ConnectionGuard`] stays
/// with the manager: the manager must retain teardown authority so it can
/// send `client/goodbye (another_server)` to a displaced incumbent.
pub struct ManagedConnection {
    /// JSON protocol messages from the server.
    pub messages: UnboundedReceiver<Message>,
    /// Audio chunks from the server.
    pub audio: UnboundedReceiver<AudioChunk>,
    /// Artwork chunks from the server.
    pub artwork: UnboundedReceiver<ArtworkChunk>,
    /// Visualizer chunks from the server.
    pub visualizer: UnboundedReceiver<VisualizerChunk>,
    /// Clock synchronization state. Fresh per connection: audio components
    /// built around it (e.g. `SyncedPlayer`) must be rebuilt per connection.
    pub clock_sync: Arc<Mutex<ClockSync>>,
    /// Sender for writing messages to the server.
    pub sender: WsSender,
    /// Controller handle, if the server granted the `controller@v1` role.
    pub controller: Option<Controller>,
    /// The `server/hello` received during handshake.
    pub server_hello: ServerHello,
    /// Peer address of the winning connection.
    pub peer: SocketAddr,
}

impl ManagedConnection {
    /// See [`WsSender::enter_external_source`].
    pub async fn enter_external_source(&self) -> Result<(), Error> {
        self.sender.enter_external_source().await
    }

    /// See [`WsSender::exit_external_source`].
    pub async fn exit_external_source(&self, player: Option<PlayerState>) -> Result<(), Error> {
        self.sender.exit_external_source(player).await
    }
}

/// Commands from the [`ConnectionManager`] handle to its driver task.
enum Command {
    SetLastPlayed(Option<String>),
    Disconnect(GoodbyeReason, oneshot::Sender<Result<(), Error>>),
}

/// The guard stays here so the driver keeps teardown authority over a server
/// the application is still consuming.
struct Incumbent {
    guard: ConnectionGuard,
    server_hello: ServerHello,
    peer: SocketAddr,
}

/// Owns a [`ProtocolListener`] and drives the full multi-server connection
/// lifecycle:
///
/// - Inbound peers handshake **concurrently** (bounded by
///   [`ManagerConfig::max_concurrent_handshakes`]) with an establish
///   deadline, so a stalling peer can neither block other servers nor occupy
///   a slot forever. A connection is never surfaced before its handshake
///   completes.
/// - Each established connection is arbitrated against the incumbent with
///   [`should_switch`]. The loser — displaced incumbent or rejected
///   newcomer — is automatically sent `client/goodbye (another_server)`.
/// - Winners are yielded from [`Self::next_connection`]. When a winner is
///   displaced (or its server goes away), its channels close and the next
///   call yields the replacement.
///
/// Last-played preference is a policy **input**: persist the server ID in
/// your application and feed it back via [`Self::set_last_played`] (the SDK
/// does not persist state as there is no cross-platform cross-app way to do
/// so cleanly).
///
/// Dropping the manager aborts the accept loop and the current connection
/// **without** a goodbye; call [`Self::disconnect`] first for a graceful
/// shutdown.
///
/// ```no_run
/// # use sendspin::{ConnectionManager, ProtocolClientBuilder};
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let listener = ProtocolClientBuilder::builder()
///     .client_id(uuid::Uuid::new_v4().to_string())
///     .name("Kitchen Speaker".to_string())
///     .build()
///     .listen("0.0.0.0:8927")
///     .await?;
/// let mut manager = ConnectionManager::new(listener);
/// while let Some(conn) = manager.next_connection().await {
///     // Serve conn until its channels close, then loop for its successor.
/// }
/// # Ok(()) }
/// ```
///
/// See `examples/server_initiated_metadata.rs` for a complete program.
pub struct ConnectionManager {
    conn_rx: mpsc::UnboundedReceiver<ManagedConnection>,
    cmd_tx: mpsc::UnboundedSender<Command>,
    accept_task: JoinHandle<()>,
    driver_task: JoinHandle<()>,
    local_addr: Option<SocketAddr>,
}

impl std::fmt::Debug for ConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionManager")
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

impl ConnectionManager {
    /// Manage `listener` with default [`ManagerConfig`].
    pub fn new(listener: ProtocolListener) -> Self {
        Self::with_config(listener, ManagerConfig::default())
    }

    /// Manage `listener` with explicit tuning.
    pub fn with_config(listener: ProtocolListener, mut config: ManagerConfig) -> Self {
        // Zero handshake slots would deadlock the accept loop; clamp once
        // here so every downstream use sees the same normalized value.
        config.max_concurrent_handshakes = config.max_concurrent_handshakes.max(1);

        let local_addr = listener.local_addr().ok();
        // Bounded: pairs with the handshake slots to stop admission when the
        // arbitration driver stalls (see accept_loop).
        let (established_tx, established_rx) =
            mpsc::channel::<(Connection, SocketAddr)>(config.max_concurrent_handshakes);
        let (conn_tx, conn_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let goodbye_timeout = config.goodbye_timeout;
        let accept_task = tokio::spawn(accept_loop(Arc::new(listener), established_tx, config));
        let driver_task = tokio::spawn(driver(established_rx, conn_tx, cmd_rx, goodbye_timeout));

        Self {
            conn_rx,
            cmd_tx,
            accept_task,
            driver_task,
            local_addr,
        }
    }

    /// Wait for the next connection to win arbitration. Cancel-safe.
    /// Returns `None` only if the internal driver has stopped (it does not
    /// stop in normal operation).
    ///
    /// Winners queue unboundedly, so consume promptly. An entry displaced
    /// before you received it arrives with already-closed channels — drain
    /// it and loop again.
    pub async fn next_connection(&mut self) -> Option<ManagedConnection> {
        self.conn_rx.recv().await
    }

    /// Set (or clear, with `None`) the last-played server ID used to break
    /// ties between two `discovery` connections. Persisting this across runs
    /// is the application's responsibility.
    pub fn set_last_played(&self, server_id: Option<String>) {
        // Send failure means the driver is gone; arbitration is moot then.
        let _ = self.cmd_tx.send(Command::SetLastPlayed(server_id));
    }

    /// Gracefully disconnect the current server, sending `client/goodbye`
    /// with `reason` and awaiting the flush (bounded by
    /// [`ManagerConfig::goodbye_timeout`]). No-op `Ok(())` when no server
    /// is connected. The manager keeps listening; a later inbound server
    /// is yielded from [`Self::next_connection`] as usual.
    pub async fn disconnect(&self, reason: GoodbyeReason) -> Result<(), Error> {
        let (ack_tx, ack_rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Disconnect(reason, ack_tx))
            .map_err(|_| Error::Connection("connection manager stopped".to_string()))?;
        ack_rx
            .await
            .map_err(|_| Error::Connection("connection manager stopped".to_string()))?
    }

    /// Local bound address of the underlying listener, if it was available
    /// at construction time.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }
}

impl Drop for ConnectionManager {
    fn drop(&mut self) {
        // Abrupt teardown: aborting the driver drops the incumbent's guard,
        // which aborts its background tasks without a goodbye. Graceful
        // shutdown is `disconnect(...)` before drop.
        self.accept_task.abort();
        self.driver_task.abort();
    }
}

/// Admit TCP peers as slots allow and run each handshake on its own task
/// under the establish deadline. Surplus peers wait in the TCP accept backlog
/// rather than being rejected with a goodbye.
///
/// A task holds its slot through the `established_tx` send, so a stalled
/// arbitration driver stops admission instead of accumulating established
/// connections. Tasks live in a `JoinSet` so aborting this loop (manager
/// drop) aborts in-flight handshakes, releasing their sockets — and the
/// listener's port — promptly instead of after `establish_timeout`.
async fn accept_loop(
    listener: Arc<ProtocolListener>,
    established_tx: mpsc::Sender<(Connection, SocketAddr)>,
    config: ManagerConfig,
) {
    let slots = Arc::new(Semaphore::new(config.max_concurrent_handshakes));
    let mut handshakes = tokio::task::JoinSet::new();
    loop {
        while handshakes.try_join_next().is_some() {}

        let permit = Arc::clone(&slots)
            .acquire_owned()
            .await
            .expect("handshake semaphore is never closed");

        let (tcp, peer) = match listener.accept_tcp().await {
            Ok(accepted) => accepted,
            Err(e) => {
                // Accept errors (e.g. fd exhaustion) are usually transient;
                // pause briefly instead of spinning or dying.
                log::warn!("ConnectionManager accept failed: {e}");
                drop(permit);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        let listener = Arc::clone(&listener);
        let established_tx = established_tx.clone();
        let deadline = config.establish_timeout;
        handshakes.spawn(async move {
            // Hold the handshake slot for the lifetime of the attempt.
            let _permit = permit;
            match tokio::time::timeout(deadline, listener.handshake_and_drive(tcp)).await {
                Ok(Ok(client)) => {
                    // Driver gone (manager dropped): the connection is
                    // dropped here and its guard aborts its tasks.
                    let _ = established_tx.send((client.split(), peer)).await;
                }
                Ok(Err(e)) => log::warn!("Inbound handshake from {peer} failed: {e}"),
                Err(_) => log::warn!(
                    "Inbound connection from {peer} did not establish within {deadline:?}; dropping"
                ),
            }
        });
    }
}

/// Arbitration driver: single owner of the incumbent connection's guard and
/// the last-played policy input. All lifecycle transitions happen here.
async fn driver(
    mut established_rx: mpsc::Receiver<(Connection, SocketAddr)>,
    conn_tx: mpsc::UnboundedSender<ManagedConnection>,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    goodbye_timeout: Duration,
) {
    let mut last_played: Option<String> = None;
    let mut current: Option<Incumbent> = None;
    // Goodbye flushes run in a JoinSet rather than detached, so aborting the
    // driver (manager drop) also aborts any still-wedged flush.
    let mut goodbyes = tokio::task::JoinSet::new();

    loop {
        while goodbyes.try_join_next().is_some() {}

        // Resolves when the incumbent's reader task ends (server closed the
        // socket or transport failure); pends forever when there is none.
        let incumbent_closed = async {
            match current.as_mut() {
                Some(inc) => inc.guard.closed().await,
                None => std::future::pending().await,
            }
        };

        tokio::select! {
            established = established_rx.recv() => {
                let Some((conn, peer)) = established else {
                    // Accept loop is gone; only possible when the manager
                    // handle was dropped, which also aborts this task.
                    break;
                };
                arbitrate(
                    &mut current,
                    conn,
                    peer,
                    last_played.as_deref(),
                    &conn_tx,
                    &mut goodbyes,
                    goodbye_timeout,
                );
            }
            _ = incumbent_closed => {
                let inc = current.take().expect("closed() only fires with an incumbent");
                log::info!(
                    "Server {} ({}) disconnected; awaiting next server",
                    inc.server_hello.server_id,
                    inc.peer
                );
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    None => break, // ConnectionManager handle dropped.
                    Some(Command::SetLastPlayed(id)) => last_played = id,
                    Some(Command::Disconnect(reason, ack)) => {
                        match current.take() {
                            // Peer already dead: a goodbye is moot, and
                            // flushing one at a dead socket would turn a
                            // successful teardown into a spurious error.
                            Some(inc) if inc.guard.is_closed() => {
                                let _ = ack.send(Ok(()));
                            }
                            // Off the driver task so a slow flush can't
                            // stall arbitration; the caller still gets the
                            // real result via the ack.
                            Some(inc) => {
                                goodbyes.spawn(async move {
                                    let _ = ack.send(
                                        flush_goodbye(inc.guard, reason, goodbye_timeout).await,
                                    );
                                });
                            }
                            None => {
                                let _ = ack.send(Ok(()));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Flush `client/goodbye` with a deadline: a peer that stops reading must
/// not pin the flush — and the connection's tasks and socket — until the OS
/// TCP timeout. On elapse the guard is dropped, aborting the connection.
async fn flush_goodbye(
    guard: ConnectionGuard,
    reason: GoodbyeReason,
    deadline: Duration,
) -> Result<(), Error> {
    match tokio::time::timeout(deadline, guard.disconnect(reason.clone())).await {
        Ok(result) => result,
        Err(_) => {
            log::warn!("client/goodbye ({reason:?}) flush timed out; connection aborted");
            Err(Error::Connection(
                "client/goodbye flush timed out; connection aborted".to_string(),
            ))
        }
    }
}

/// Apply [`should_switch`] to a freshly established connection: promote it
/// (goodbying any displaced incumbent) or reject it with a goodbye.
fn arbitrate(
    current: &mut Option<Incumbent>,
    conn: Connection,
    peer: SocketAddr,
    last_played: Option<&str>,
    conn_tx: &mpsc::UnboundedSender<ManagedConnection>,
    goodbyes: &mut tokio::task::JoinSet<()>,
    goodbye_timeout: Duration,
) {
    // The incumbent may have died in the same instant this connection
    // established, and the select loop can deliver the establishment first.
    // A dead incumbent must not reject a live server, so check liveness
    // before applying policy. No goodbye owed: the transport is gone.
    if current.as_ref().is_some_and(|inc| inc.guard.is_closed()) {
        let dead = current.take().expect("checked Some above");
        log::info!(
            "Server {} ({}) already disconnected; arbitration proceeds without it",
            dead.server_hello.server_id,
            dead.peer
        );
    }

    if let Some(inc) = current.as_ref() {
        if !should_switch(&inc.server_hello, &conn.server_hello, last_played) {
            log::info!(
                "Keeping server {} — rejecting {} ({peer}) with goodbye(another_server)",
                inc.server_hello.server_id,
                conn.server_hello.server_id,
            );
            // Spawned so a slow flush can't stall arbitration; dropping the
            // rest of `conn` closes its channels.
            goodbyes.spawn(async move {
                let _ =
                    flush_goodbye(conn.guard, GoodbyeReason::AnotherServer, goodbye_timeout).await;
            });
            return;
        }

        let displaced = current.take().expect("checked Some above");
        log::info!(
            "Switching {} -> {} — goodbye(another_server) to displaced server",
            displaced.server_hello.server_id,
            conn.server_hello.server_id,
        );
        goodbyes.spawn(async move {
            let _ = flush_goodbye(
                displaced.guard,
                GoodbyeReason::AnotherServer,
                goodbye_timeout,
            )
            .await;
        });
    } else {
        log::info!(
            "Server {} ({peer}) connected (reason: {:?})",
            conn.server_hello.server_id,
            conn.server_hello.connection_reason,
        );
    }

    let Connection {
        messages,
        audio,
        artwork,
        visualizer,
        clock_sync,
        sender,
        controller,
        server_hello,
        guard,
    } = conn;

    *current = Some(Incumbent {
        guard,
        server_hello: server_hello.clone(),
        peer,
    });

    // Receiver dropped means the manager handle is gone; the driver will
    // exit via its command channel shortly after.
    let _ = conn_tx.send(ManagedConnection {
        messages,
        audio,
        artwork,
        visualizer,
        clock_sync,
        sender,
        controller,
        server_hello,
        peer,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(server_id: &str, reason: ConnectionReason) -> ServerHello {
        ServerHello {
            server_id: server_id.to_string(),
            name: format!("{server_id} name"),
            version: 1,
            active_roles: vec![],
            connection_reason: reason,
        }
    }

    // ConnectionManager::should_switch_to_new_server.

    #[test]
    fn playback_candidate_always_wins() {
        let current = hello("a", ConnectionReason::Playback);
        let candidate = hello("b", ConnectionReason::Playback);
        assert!(should_switch(&current, &candidate, None));

        let current = hello("a", ConnectionReason::Discovery);
        assert!(should_switch(&current, &candidate, None));

        // Even when the incumbent is the last-played server.
        assert!(should_switch(&current, &candidate, Some("a")));
    }

    #[test]
    fn discovery_never_displaces_playback() {
        let current = hello("a", ConnectionReason::Playback);
        let candidate = hello("b", ConnectionReason::Discovery);
        assert!(!should_switch(&current, &candidate, None));
        // Even when the candidate is the last-played server.
        assert!(!should_switch(&current, &candidate, Some("b")));
    }

    #[test]
    fn both_discovery_prefers_last_played() {
        let current = hello("a", ConnectionReason::Discovery);
        let candidate = hello("b", ConnectionReason::Discovery);

        assert!(should_switch(&current, &candidate, Some("b")));
        assert!(!should_switch(&current, &candidate, Some("a")));
    }

    #[test]
    fn both_discovery_defaults_to_keep() {
        let current = hello("a", ConnectionReason::Discovery);
        let candidate = hello("b", ConnectionReason::Discovery);

        assert!(!should_switch(&current, &candidate, None));
        // Last-played matches neither: keep.
        assert!(!should_switch(&current, &candidate, Some("c")));
    }
}
