// ABOUTME: Protocol implementation for Sendspin WebSocket protocol
// ABOUTME: Message types, serialization, and WebSocket client

/// WebSocket client implementation
pub mod client;
/// Builder for easy construction of the client
pub mod client_builder;
/// Inbound WebSocket acceptor for server-initiated connections
pub mod listener;
/// Managed connection lifecycle: multi-server arbitration and auto-goodbye
pub mod manager;
/// Protocol message type definitions and serialization
pub mod messages;

pub use client::{Connection, ConnectionGuard, Controller, WsSender};
pub use listener::ProtocolListener;
pub use manager::{should_switch, ConnectionManager, ManagedConnection, ManagerConfig};
pub use messages::Message;
