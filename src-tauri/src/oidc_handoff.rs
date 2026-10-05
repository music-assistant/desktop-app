//! Single sign-on handoff: run the identity-provider leg in the user's browser.
//!
//! The embedded webview cannot complete an OAuth login against an identity
//! provider that requires `WebAuthn`. `WKWebView` only exposes passkeys when the
//! app carries an associated-domains entitlement *and* the domain publishes a
//! site-association file, and this bundle declares neither — so a provider that
//! asks for a passkey (or forces one to be enrolled) leaves the login stuck
//! inside the app with no authenticator to offer.
//!
//! RFC 8252 says the same thing from the other direction: a native app should
//! use the user's browser for the authorization leg, not an embedded
//! user-agent. That is what happens here. The webview's navigation to the
//! provider is intercepted, the authorization URL is opened in the real
//! browser, and the resulting Music Assistant token is handed back to the
//! webview as `/?code=<token>` — the same shape the server-rendered login page
//! and the mobile apps already use.
//!
//! The `return_url` is a loopback address this process listens on, because the
//! desktop app has no registered URL scheme. Music Assistant treats a loopback
//! return as an external redirect, so the browser shows its consent step
//! before handing the token over.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;

/// How long the browser leg may take before the handoff gives up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

/// Loopback path the authorization response is redirected to.
const CALLBACK_PATH: &str = "/oidc-callback";

/// Provider id used for single sign-on; matches the server's OIDC provider.
const PROVIDER_ID: &str = "oidc";

static CURRENT_SERVER: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn current_server_slot() -> &'static Mutex<Option<String>> {
    CURRENT_SERVER.get_or_init(|| Mutex::new(None))
}

/// Remember the server the launcher is connecting to.
///
/// The handoff needs it to ask that server for an authorization URL, and to
/// navigate the webview back to it once the browser leg returns.
pub fn set_current_server(url: &str) {
    let trimmed = url.trim_end_matches('/').to_string();
    if let Ok(mut slot) = current_server_slot().lock() {
        *slot = Some(trimmed);
    }
}

fn current_server() -> Option<String> {
    current_server_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

/// Is this navigation an identity-provider authorization request?
///
/// Deliberately narrow: only a real authorization request (an authorize-style
/// path carrying `response_type=code`) to a host other than the configured
/// server is handed off, so ordinary in-app navigation is never disturbed.
pub fn is_idp_navigation(url: &tauri::Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }

    let host = url.host_str().unwrap_or_default();
    if matches!(host, "localhost" | "127.0.0.1") {
        return false;
    }

    // The Music Assistant frontend itself must always load in the webview.
    if let Some(server_host) = current_server()
        .and_then(|server| tauri::Url::parse(&server).ok())
        .and_then(|parsed| parsed.host_str().map(str::to_string))
    {
        if host == server_host {
            return false;
        }
    }

    let path = url.path().to_ascii_lowercase();
    let looks_like_authorize = path.contains("/application/o/")
        || path.contains("/if/flow/")
        || path.contains("/authorize")
        || path.contains("/oauth")
        || path.contains("/oidc");
    let is_code_flow = url
        .query()
        .is_some_and(|query| query.contains("response_type=code"));

    looks_like_authorize && is_code_flow
}

/// Start the handoff on a worker thread.
///
/// Called from the webview's navigation hook, which runs on the main thread and
/// must not block on the browser leg, so the work is moved off it and failures
/// are reported with a dialog rather than a silent no-op.
pub fn spawn(app: AppHandle) {
    std::thread::spawn(move || {
        if let Err(error) = run(&app) {
            log::warn!("[SSO] handoff failed: {error}");
            app.dialog()
                .message(format!(
                    "Single sign-on could not complete.\n\n{error}\n\n\
                     You can still sign in with a username and password in the app.",
                ))
                .kind(MessageDialogKind::Error)
                .title("Music Assistant")
                .blocking_show();
        }
    });
}

fn run(app: &AppHandle) -> Result<(), String> {
    let server = current_server().ok_or("no server has been selected yet")?;

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    );

    // Only offer the handoff when the server actually has single sign-on, so a
    // server without it fails with advice instead of an empty browser tab.
    let mut providers_response = agent
        .get(format!("{server}/auth/providers"))
        .call()
        .map_err(|error| format!("could not reach the server: {error}"))?;
    let providers_body = providers_response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("unreadable provider list: {error}"))?;
    let providers: serde_json::Value = serde_json::from_str(&providers_body)
        .map_err(|error| format!("unreadable provider list: {error}"))?;
    let has_oidc = providers.as_array().is_some_and(|list| {
        list.iter().any(|provider| {
            provider.get("provider_id").and_then(|value| value.as_str()) == Some(PROVIDER_ID)
        })
    });
    if !has_oidc {
        return Err("this server has no single sign-on provider configured".to_string());
    }

    // The loopback listener has to exist before the browser leg starts, because
    // its port is part of the return URL sent to the server.
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("could not open a loopback listener: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("unreadable loopback address: {error}"))?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not configure the loopback listener: {error}"))?;

    let return_url = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
    let request = json!({
        "message_id": "desktop-sso",
        "command": "auth/authorization_url",
        "args": { "provider_id": PROVIDER_ID, "return_url": return_url },
    })
    .to_string();
    let mut answer_response = agent
        .post(format!("{server}/api"))
        .header("Content-Type", "application/json")
        .send(request)
        .map_err(|error| format!("could not start the sign-in: {error}"))?;
    let answer_body = answer_response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("unreadable sign-in response: {error}"))?;
    let answer: serde_json::Value = serde_json::from_str(&answer_body)
        .map_err(|error| format!("unreadable sign-in response: {error}"))?;
    let authorize_url = answer
        .get("authorization_url")
        .and_then(|value| value.as_str())
        .ok_or("the server did not return an authorization URL")?
        .to_string();

    log::info!("[SSO] opening the browser for {server}");
    app.opener()
        .open_url(authorize_url, None::<&str>)
        .map_err(|error| format!("could not open the browser: {error}"))?;

    let token = wait_for_callback(&listener)?;

    // Hand the token to the webview exactly as the web login does: the frontend
    // consumes `code` on load and stores it as the session.
    let target = format!("{server}/?code={token}");
    let script = format!(
        "window.location.href = {};",
        serde_json::to_string(&target).map_err(|error| error.to_string())?
    );
    match app.get_webview_window("main") {
        Some(window) => window
            .eval(script)
            .map_err(|error| format!("could not complete the sign-in: {error}")),
        None => Err("the app window is no longer available".to_string()),
    }
}

/// Wait for the browser to redirect back to the loopback listener.
fn wait_for_callback(listener: &TcpListener) -> Result<String, String> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;

    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let target = request.split_whitespace().nth(1).unwrap_or_default();
                let query = target
                    .split_once('?')
                    .map(|(_, query)| query)
                    .unwrap_or_default();
                let code = query
                    .split('&')
                    .find_map(|pair| pair.strip_prefix("code="))
                    .map(percent_decode);

                let body = "<!doctype html><meta charset=\"utf-8\">\
                            <title>Music Assistant</title>\
                            <body style=\"font-family:-apple-system,system-ui,sans-serif;padding:2rem\">\
                            <h1 style=\"font-size:1.1rem\">Signed in</h1>\
                            <p>You can close this tab and return to the Music Assistant app.</p>";

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();

                return match code {
                    Some(token) if !token.is_empty() => Ok(token),
                    _ => Err("the sign-in was cancelled or refused".to_string()),
                };
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(150));
            }
            Err(error) => return Err(format!("loopback listener failed: {error}")),
        }
    }

    Err("timed out waiting for the browser sign-in".to_string())
}

/// Decode the `%XX` escapes Music Assistant applies to the token.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
