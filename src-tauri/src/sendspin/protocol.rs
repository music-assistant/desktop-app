//! Extended Sendspin protocol messages
//!
//! This module adds message types not yet available in sendspin-rs:
//! - client/command for controller role
//! - server/state for metadata role
//!
//! These follow the aiosendspin (Python) protocol specification.

use serde::{Deserialize, Serialize};

/// Media commands that can be sent to control playback
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MediaCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    Volume,
    Mute,
}

/// Controller command payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerCommandPayload {
    pub command: MediaCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute: Option<bool>,
}

/// Client command message payload
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCommandPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerCommandPayload>,
}

/// Client command message (client/command)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientCommandMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub payload: ClientCommandPayload,
}

#[allow(dead_code)]
impl ClientCommandMessage {
    pub fn new(command: MediaCommand) -> Self {
        Self {
            msg_type: "client/command".to_string(),
            payload: ClientCommandPayload {
                controller: Some(ControllerCommandPayload {
                    command,
                    volume: None,
                    mute: None,
                }),
            },
        }
    }

    pub fn volume(level: u8) -> Self {
        Self {
            msg_type: "client/command".to_string(),
            payload: ClientCommandPayload {
                controller: Some(ControllerCommandPayload {
                    command: MediaCommand::Volume,
                    volume: Some(level),
                    mute: None,
                }),
            },
        }
    }

    pub fn mute(muted: bool) -> Self {
        Self {
            msg_type: "client/command".to_string(),
            payload: ClientCommandPayload {
                controller: Some(ControllerCommandPayload {
                    command: MediaCommand::Mute,
                    volume: None,
                    mute: Some(muted),
                }),
            },
        }
    }
}

/// Progress information for metadata
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::struct_field_names, dead_code)]
pub struct Progress {
    /// Track progress in milliseconds
    pub track_progress: i64,
    /// Track duration in milliseconds (0 for unknown/live)
    pub track_duration: i64,
    /// Playback speed * 1000 (1000 = normal, 0 = paused)
    pub playback_speed: i32,
}

/// Metadata from server/state message
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(dead_code)]
pub struct SessionMetadata {
    /// Server timestamp in microseconds
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub track: Option<i32>,
    #[serde(default)]
    pub progress: Option<Progress>,
    #[serde(default)]
    pub repeat: Option<String>,
    #[serde(default)]
    pub shuffle: Option<bool>,
}

/// Server state message payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ServerStatePayload {
    #[serde(default)]
    pub metadata: Option<SessionMetadata>,
}

/// Server state message (server/state)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ServerStateMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub payload: ServerStatePayload,
}

/// Group update payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GroupUpdatePayload {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
}

/// Group update message (group/update)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GroupUpdateMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub payload: GroupUpdatePayload,
}

/// Generic message wrapper for parsing unknown messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct GenericMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(flatten)]
    pub rest: serde_json::Value,
}
