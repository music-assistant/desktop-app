// ABOUTME: Inbound WebSocket acceptor that drives the Sendspin protocol-client
// ABOUTME: state machine on every peer that connects.

use crate::error::Error;
use crate::protocol::client_builder::ProtocolClientBuilder;
use crate::ProtocolClient;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http;
use tokio_tungstenite::{accept_async, accept_hdr_async, WebSocketStream};

#[cfg(feature = "native-tls")]
use tokio_native_tls::TlsAcceptor;

/// Accept inbound WebSocket peers and drive each one through the
/// protocol-client state machine. Construct via
/// [`ProtocolClientBuilder::listen`].
///
/// Sendspin's protocol-client/server roles are independent of who
/// initiates the TCP connection — the protocol-client always sends
/// `client/hello` first. This listener handles the server-initiated
/// case; the [`ProtocolClient`] returned by [`Self::accept`] is
/// indistinguishable in shape from one returned by
/// [`ProtocolClientBuilder::connect`].
///
/// [`Self::accept`] drives the full protocol handshake before returning, so
/// it serves one inbound connection at a time — a slow handshake blocks the
/// next `accept()`. If you need concurrent handshakes, or want to own the
/// transport (custom TLS, HTTP routing, …), accept your own streams and drive
/// [`ProtocolClientBuilder::accept`] on each, `tokio::spawn`-ing per peer.
///
/// The Sendspin spec allows multiple servers to initiate connections to the
/// same client. For the batteries-included path, hand this listener to
/// [`ConnectionManager`], which runs inbound handshakes concurrently,
/// applies the spec keep-or-switch policy ([`should_switch`]), and sends
/// [`GoodbyeReason::AnotherServer`] to losers automatically. To run the
/// policy yourself instead: read [`ProtocolClient::server_hello`] for
/// `server_id` and `connection_reason`, decide with [`should_switch`]
/// against your persisted last-played server, and send the loser's goodbye.
/// This listener itself does not enforce a policy.
///
/// Practical notes for the manual path:
/// - Disconnect consumes the handle. Call [`ProtocolClient::disconnect`]
///   pre-split, or `connection.guard.disconnect(...)` post-split.
/// - [`Self::accept`] is serial. If the loser's goodbye is on the critical
///   path, run it on a spawned task so the next inbound peer can handshake
///   while the previous one is tearing down.
///
/// [`ConnectionManager`]: crate::protocol::manager::ConnectionManager
/// [`should_switch`]: crate::protocol::manager::should_switch
/// [`GoodbyeReason::AnotherServer`]: crate::protocol::messages::GoodbyeReason::AnotherServer
pub struct ProtocolListener {
    tcp: TcpListener,
    template: ProtocolClientBuilder,
    path: Option<String>,
    #[cfg(feature = "native-tls")]
    tls_acceptor: Option<TlsAcceptor>,
}

impl std::fmt::Debug for ProtocolListener {
    // Manual impl: ProtocolClientBuilder holds an Arc<dyn Clock> and can't
    // derive Debug. Surface the operationally useful bits instead.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("ProtocolListener");
        s.field("local_addr", &self.tcp.local_addr().ok());
        s.field("path", &self.path);
        #[cfg(feature = "native-tls")]
        s.field("tls", &self.tls_acceptor.is_some());
        s.finish()
    }
}

impl ProtocolListener {
    pub(crate) fn new(tcp: TcpListener, template: ProtocolClientBuilder) -> Self {
        Self {
            tcp,
            template,
            path: None,
            #[cfg(feature = "native-tls")]
            tls_acceptor: None,
        }
    }

    /// Restrict accepted connections to a specific HTTP path. Mismatches
    /// are rejected with HTTP 404 during the WebSocket handshake;
    /// the listener stays bound. Defaults to accepting any path.
    ///
    /// Matching is exact: `/sendspin` does not match `/sendspin/`. A missing
    /// leading slash is added, so `"sendspin"` and `"/sendspin"` are
    /// equivalent — request paths always start with `/`, and the raw form
    /// would otherwise reject every connection.
    pub fn path(mut self, path: impl Into<String>) -> Self {
        let path = path.into();
        self.path = Some(if path.starts_with('/') {
            path
        } else {
            format!("/{path}")
        });
        self
    }

    /// Terminate TLS on every accepted connection using the given identity.
    /// The identity is fixed for the listener's lifetime — to rotate
    /// certificates, rebind. For mTLS, custom ALPN, or any other
    /// non-trivial TLS need, terminate TLS yourself and use
    /// [`ProtocolClientBuilder::accept`] instead.
    ///
    /// Unlike [`Self::path`] this returns `Result`, so chain it last:
    /// `listener.path("/x").tls(id)?`.
    #[cfg(feature = "native-tls")]
    pub fn tls(mut self, identity: native_tls::Identity) -> Result<Self, Error> {
        let acceptor = native_tls::TlsAcceptor::new(identity)
            .map_err(|e| Error::Connection(format!("TLS acceptor build failed: {e}")))?;
        self.tls_acceptor = Some(TlsAcceptor::from(acceptor));
        Ok(self)
    }

    /// Accept the next inbound connection, returning the driven
    /// [`ProtocolClient`] and the peer's address. Performs the WebSocket
    /// handshake and protocol-client hello/state exchange.
    ///
    /// Per-peer failures surface as [`Error`] without affecting the
    /// listener; callers typically call `accept()` in a loop.
    ///
    /// Not cancel-safe: dropping the returned future mid-handshake tears
    /// down that connection. A peer that connects but stalls the handshake
    /// will block this future indefinitely, so wrap it in a timeout if
    /// untrusted peers can reach the socket.
    pub async fn accept(&self) -> Result<(ProtocolClient, SocketAddr), Error> {
        let (tcp_stream, peer_addr) = self.accept_tcp().await?;

        match self.handshake_and_drive(tcp_stream).await {
            Ok(client) => Ok((client, peer_addr)),
            // The peer address is known here but lost once we return the bare
            // error, so log it for attribution.
            Err(e) => {
                log::warn!("Inbound handshake from {peer_addr} failed: {e}");
                Err(e)
            }
        }
    }

    /// Accept a raw TCP connection without starting any handshake. Split from
    /// [`Self::accept`] so the cheap TCP accept can capture the peer address
    /// before any fallible step, and so [`ConnectionManager`] can run the
    /// handshake on a separate task.
    ///
    /// [`ConnectionManager`]: crate::protocol::manager::ConnectionManager
    pub(crate) async fn accept_tcp(&self) -> Result<(TcpStream, SocketAddr), Error> {
        let (tcp_stream, peer_addr) = self
            .tcp
            .accept()
            .await
            .map_err(|e| Error::Connection(format!("TCP accept failed: {e}")))?;
        log::debug!("Accepted TCP connection from {}", peer_addr);
        Ok((tcp_stream, peer_addr))
    }

    /// Drive TLS (if configured), the WebSocket upgrade, and the protocol
    /// hello/state exchange over an accepted TCP stream.
    pub(crate) async fn handshake_and_drive(
        &self,
        tcp_stream: TcpStream,
    ) -> Result<ProtocolClient, Error> {
        // Branched (rather than unified through a trait-object stream)
        // so the WS handshake is monomorphized on the concrete transport.
        #[cfg(feature = "native-tls")]
        if let Some(tls) = &self.tls_acceptor {
            let tls_stream = tls
                .accept(tcp_stream)
                .await
                .map_err(|e| Error::Connection(format!("TLS handshake failed: {e}")))?;
            let ws = self.handshake_ws(tls_stream).await?;
            return self.template.clone().accept(ws).await;
        }

        let ws = self.handshake_ws(tcp_stream).await?;
        self.template.clone().accept(ws).await
    }

    // `ErrorResponse` is ~136 bytes — large by Clippy's standard but
    // mandated by tungstenite's `Callback` trait.
    #[allow(clippy::result_large_err)]
    async fn handshake_ws<S>(&self, stream: S) -> Result<WebSocketStream<S>, Error>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        match &self.path {
            Some(expected_path) => {
                let expected = expected_path.clone();
                let callback = move |request: &Request, response: Response| {
                    if request.uri().path() == expected {
                        Ok(response)
                    } else {
                        log::debug!(
                            "Rejecting inbound connection: path {:?} != expected {:?}",
                            request.uri().path(),
                            expected
                        );
                        Err(http::Response::builder()
                            .status(http::StatusCode::NOT_FOUND)
                            .body(None)
                            .expect("static 404 response is well-formed"))
                            as Result<Response, ErrorResponse>
                    }
                };
                accept_hdr_async(stream, callback)
                    .await
                    .map_err(|e| Error::WebSocket(format!("WebSocket handshake failed: {e}")))
            }
            None => accept_async(stream)
                .await
                .map_err(|e| Error::WebSocket(format!("WebSocket handshake failed: {e}"))),
        }
    }

    /// Local bound address.
    pub fn local_addr(&self) -> Result<SocketAddr, Error> {
        self.tcp
            .local_addr()
            .map_err(|e| Error::Connection(format!("local_addr failed: {e}")))
    }
}
