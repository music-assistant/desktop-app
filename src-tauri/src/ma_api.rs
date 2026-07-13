use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Keep synchronous native MA API calls short so app service threads do not stall long.
const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// The native callers only need small metadata responses; reject unexpectedly large bodies.
const MAX_API_RESPONSE_BYTES: u64 = 256 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct Session {
    pub(crate) server_base_url: String,
    pub(crate) auth_token: String,
}

static CURRENT_SESSION: Mutex<Option<Session>> = Mutex::new(None);

pub(crate) fn remember_session(server_base_url: String, auth_token: String) {
    if let Ok(mut session) = CURRENT_SESSION.lock() {
        *session = Some(Session {
            server_base_url,
            auth_token,
        });
    } else {
        log::warn!("[MA API] Failed to store current Music Assistant session");
    }
}

pub(crate) fn clear_current_session() {
    if let Ok(mut session) = CURRENT_SESSION.lock() {
        *session = None;
    }
}

pub(crate) fn current_session() -> Option<Session> {
    CURRENT_SESSION
        .lock()
        .ok()
        .and_then(|session| session.clone())
}

pub(crate) fn get_active_queue(player_id: &str) -> Result<String, String> {
    post_command_raw(
        "discord-rpc-artwork",
        "player_queues/get_active_queue",
        json!({ "player_id": player_id }),
    )
}

fn api_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(API_REQUEST_TIMEOUT))
            .build()
            .into()
    })
}

fn post_command_raw(message_id: &str, command: &str, args: Value) -> Result<String, String> {
    let session =
        current_session().ok_or_else(|| "no active Music Assistant session".to_string())?;
    let api_url = format!("{}/api", session.server_base_url.trim_end_matches('/'));
    let request_body = json!({
        "message_id": message_id,
        "command": command,
        "args": args,
    })
    .to_string();

    let auth_header = format!("Bearer {}", session.auth_token);
    let mut response = api_agent()
        .post(api_url)
        .header("Authorization", auth_header)
        .header("Content-Type", "application/json")
        .send(request_body)
        .map_err(|err| err.to_string())?;

    response
        .body_mut()
        .with_config()
        .limit(MAX_API_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|err| err.to_string())
}
