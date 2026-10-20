use std::io::Read;
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::Manager;

const INFO_LIMIT: usize = 64 * 1024;
const LOAD_DEADLINE: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Pending {
    generation: u64,
    window: String,
    url: String,
    name: Option<String>,
}

#[derive(Default)]
struct NavigationState {
    generation: u64,
    reserved: Option<(u64, String)>,
    pending: Option<Pending>,
    recovering: Option<u64>,
    watchdog_armed: bool,
}

#[derive(Clone, Serialize)]
pub struct Recovery {
    pub url: String,
    pub name: Option<String>,
    pub reason: &'static str,
}

static STATE: Mutex<NavigationState> = Mutex::new(NavigationState {
    generation: 0,
    reserved: None,
    pending: None,
    recovering: None,
    watchdog_armed: false,
});

fn parse_base(raw: &str) -> Result<tauri::Url, &'static str> {
    let mut url = tauri::Url::parse(raw).map_err(|_| "address")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("address");
    }
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    Ok(url)
}

#[derive(Debug, PartialEq, Eq)]
enum ProbeOutcome {
    Identified,
    AuthenticationRequired,
}

fn identity(base: &tauri::Url) -> Result<ProbeOutcome, &'static str> {
    let info = base.join("info").map_err(|_| "address")?;
    let agent = crate::http_client::agent(Duration::from_secs(5));
    // Disable automatic redirects: no response from a different service can certify this address.
    let mut response = agent
        .get(info.as_str())
        .config()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(_) => "server",
            _ => "connection",
        })?;
    match response.status().as_u16() {
        401 | 403 => return Ok(ProbeOutcome::AuthenticationRequired),
        301 | 302 | 303 | 307 | 308 => {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("server")?;
            let target = info.join(location).map_err(|_| "server")?;
            if !matches!(target.scheme(), "http" | "https")
                || !target.username().is_empty()
                || target.password().is_some()
            {
                return Err("server");
            }
            return Ok(ProbeOutcome::AuthenticationRequired);
        }
        200 => {}
        _ => return Err("server"),
    }
    if response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        .is_some_and(|length| length > INFO_LIMIT)
    {
        return Err("server");
    }
    let mut bytes = Vec::new();
    response
        .body_mut()
        .with_config()
        .limit((INFO_LIMIT + 1) as u64)
        .reader()
        .read_to_end(&mut bytes)
        .map_err(|_| "server")?;
    if bytes.len() > INFO_LIMIT {
        return Err("server");
    }
    let data: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| "server")?;
    if !data.get("server_id").is_some_and(|v| v.is_string())
        || !data.get("server_version").is_some_and(|v| v.is_string())
        || data
            .get("schema_version")
            .is_none_or(|v| v.as_u64().is_none())
    {
        return Err("server");
    }
    Ok(ProbeOutcome::Identified)
}

fn selected_page_loaded(base: &tauri::Url, current: &tauri::Url) -> bool {
    current.origin() == base.origin()
        && (current.path().starts_with(base.path())
            || current.path() == base.path().trim_end_matches('/'))
}

fn visible_document_matches(current: &tauri::Url, document_origin: &str) -> bool {
    matches!(current.scheme(), "http" | "https")
        && current.origin().ascii_serialization() == document_origin
}

#[cfg(test)]
fn launcher_url(url: &tauri::Url) -> bool {
    let dev_server = cfg!(debug_assertions)
        && url.scheme() == "http"
        && url.host_str() == Some("localhost")
        && url.port() == Some(1420);
    let packaged = match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        _ => false,
    };
    (packaged || dev_server)
        && (matches!(url.path(), "/" | "/index.html")
            || url.scheme() == "tauri" && url.path().is_empty())
}

fn reserve(state: &mut NavigationState, window: &str) -> u64 {
    state.generation += 1;
    state.reserved = Some((state.generation, window.to_owned()));
    state.pending = None;
    state.recovering = None;
    state.watchdog_armed = false;
    state.generation
}

fn owns(state: &NavigationState, generation: u64, window: &str) -> bool {
    state
        .reserved
        .as_ref()
        .is_some_and(|(id, label)| *id == generation && label == window)
}

fn cancel_token(state: &mut NavigationState, generation: u64, window: &str) -> bool {
    if !owns(state, generation, window) {
        return false;
    }
    state.reserved = None;
    state.pending = None;
    state.recovering = None;
    state.watchdog_armed = false;
    true
}

#[tauri::command]
pub fn reserve_initial_navigation(window: tauri::WebviewWindow) -> Result<u64, String> {
    if !matches!(window.label(), "main" | "launcher") {
        match window.url() {
            Ok(url) => {
                let scheme = match url.scheme() {
                    "tauri" => "tauri",
                    "http" => "http",
                    "https" => "https",
                    _ => "other",
                };
                let host = match url.host_str() {
                    Some("localhost") => "localhost",
                    Some("tauri.localhost") => "tauri.localhost",
                    Some(_) => "other",
                    None => "none",
                };
                let path = match url.path() {
                    "/" => "root",
                    "/index.html" => "index",
                    _ => "other",
                };
                log::warn!(
                    "[Initial navigation] Launcher origin rejected: label={}, scheme={scheme}, host={host}, port={:?}, path={path}",
                    if matches!(window.label(), "main" | "launcher") { "launcher" } else { "other" },
                    url.port()
                );
            }
            Err(_) => {
                log::warn!("[Initial navigation] Launcher origin rejected: window URL unavailable");
            }
        }
        return Err("address".into());
    }
    Ok(reserve(&mut STATE.lock().unwrap(), window.label()))
}

#[tauri::command]
pub async fn begin_initial_navigation(
    window: tauri::WebviewWindow,
    generation: u64,
    url: String,
    name: Option<String>,
) -> Result<String, String> {
    if !matches!(window.label(), "main" | "launcher")
        || !owns(&STATE.lock().unwrap(), generation, window.label())
    {
        return Err("cancelled".into());
    }
    let base = parse_base(&url).map_err(|error| {
        log::warn!(
            "[Initial navigation] Address validation rejected: bytes={}, control={}, whitespace={}, query_marker={}, fragment_marker={}",
            url.len(),
            url.chars().any(char::is_control),
            url.chars().any(char::is_whitespace),
            url.contains('?'),
            url.contains('#')
        );
        error.to_owned()
    })?;
    let canonical = base.to_string();
    let check = tauri::async_runtime::spawn_blocking(move || identity(&base))
        .await
        .map_err(|_| "connection")?;
    let outcome = check.map_err(str::to_owned)?;
    if outcome == ProbeOutcome::AuthenticationRequired {
        log::info!("[Initial navigation] Server requests browser authentication; identity not verified by native preflight");
    }
    let pending = {
        let mut state = STATE.lock().unwrap();
        if !owns(&state, generation, window.label()) {
            return Err("cancelled".into());
        }
        let pending = Pending {
            generation,
            window: window.label().to_owned(),
            url: canonical.clone(),
            name,
        };
        state.pending = Some(pending.clone());
        state.watchdog_armed = true;
        pending
    };
    let app = window.app_handle().clone();
    let window_for_timeout = window.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(LOAD_DEADLINE).await;
        for attempt in 0..2 {
            let should_recover = {
                let mut state = STATE.lock().unwrap();
                if state.watchdog_armed
                    && owns(&state, pending.generation, &pending.window)
                    && state
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.generation == pending.generation)
                {
                    state.recovering = Some(pending.generation);
                    true
                } else {
                    false
                }
            };
            if should_recover {
                let recovery = Recovery {
                    url: pending.url.clone(),
                    name: pending.name.clone(),
                    reason: "timeout",
                };
                let replacement = crate::replace_with_launcher(
                    &app,
                    Some(window_for_timeout.clone()),
                    Some(&recovery),
                    |old| {
                        let mut state = STATE.lock().unwrap();
                        if state.recovering != Some(pending.generation)
                            || !state.watchdog_armed
                            || !owns(&state, pending.generation, &pending.window)
                            || state
                                .pending
                                .as_ref()
                                .is_none_or(|p| p.generation != pending.generation)
                            || *old != window_for_timeout
                        {
                            return false;
                        }
                        cancel_state(&mut state);
                        true
                    },
                );
                match replacement {
                    Ok(_) => return,
                    Err(e) => {
                        let mut state = STATE.lock().unwrap();
                        if state.recovering == Some(pending.generation) {
                            state.recovering = None;
                        }
                        drop(state);
                        log::error!(
                            "[Launcher] Could not restore launcher after initial load timeout: {e}"
                        );
                        if attempt == 1 {
                            use tauri_plugin_dialog::DialogExt;
                            app.dialog()
                                .message(crate::i18n::tr(
                                    "desktop.launcher.launcher_restore_failed_message",
                                ))
                                .title(crate::i18n::tr("desktop.launcher.connection_failed"))
                                .blocking_show();
                        }
                    }
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });
    Ok(canonical)
}

#[tauri::command]
pub fn cancel_initial_navigation(window: tauri::WebviewWindow, generation: u64) {
    if matches!(window.label(), "main" | "launcher") {
        cancel_token(&mut STATE.lock().unwrap(), generation, window.label());
    }
}

#[tauri::command]
pub fn acknowledge_initial_navigation(window: tauri::WebviewWindow, document_origin: String) {
    let Ok(current) = window.url() else { return };
    if !visible_document_matches(&current, &document_origin) {
        return;
    }
    if window
        .app_handle()
        .get_webview_window(window.label())
        .is_none_or(|active| active != window)
    {
        return;
    }
    let mut state = STATE.lock().unwrap();
    let Some(pending) = state.pending.as_ref() else {
        return;
    };
    if pending.window != window.label() || !owns(&state, pending.generation, window.label()) {
        return;
    }
    state.watchdog_armed = false;
    state.recovering = None;
    let pending = state.pending.as_ref().unwrap();
    let Ok(base) = tauri::Url::parse(&pending.url) else {
        return;
    };
    // A sign-in page disarms the timeout, but retains the selected address for a later return.
    if !selected_page_loaded(&base, &current) {
        return;
    }
    let result = crate::settings::update_settings(|settings| {
        settings.last_server_url = Some(pending.url.clone());
        settings.last_server_name.clone_from(&pending.name);
    });
    if let Err(e) = result {
        log::error!("[Launcher] Could not save connected server: {e}");
    } else {
        state.pending = None;
        state.reserved = None;
    }
}

fn cancel_state(state: &mut NavigationState) {
    state.generation += 1;
    state.reserved = None;
    state.pending = None;
    state.recovering = None;
    state.watchdog_armed = false;
}

pub fn forget_selected_server() -> Result<(), String> {
    let mut state = STATE.lock().unwrap();
    cancel_state(&mut state);
    crate::settings::update_settings(|settings| {
        settings.last_server_url = None;
        settings.last_server_name = None;
    })
}

pub const DOCUMENT_READY_SCRIPT: &str = r"
if (window === window.top && /^https?:$/.test(location.protocol)) {
  let acknowledged = false;
  const acknowledge = () => {
    if (acknowledged || document.readyState === 'loading') return;
    acknowledged = true;
    window.__TAURI__?.core.invoke('acknowledge_initial_navigation', {
      documentOrigin: location.origin
    }).catch(() => {});
  };
  document.addEventListener('readystatechange', acknowledge);
  acknowledge();
}
";

#[cfg(test)]
mod tests {
    use super::*;

    fn serve(response: String, path: &str) -> Result<ProbeOutcome, &'static str> {
        use std::io::Write;
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let expected_path = path.to_owned();
        let handle = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = [0_u8; 2048];
            let length = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..length])
                .starts_with(&format!("GET {expected_path} HTTP/1.1")));
            socket.write_all(response.as_bytes()).unwrap();
        });
        let base = parse_base(&format!("http://{addr}/music/")).unwrap();
        let result = identity(&base);
        handle.join().unwrap();
        result
    }

    #[test]
    fn launcher_url_accepts_packaged_root_but_not_other_pages_or_origins() {
        for url in [
            "tauri://localhost",
            "tauri://localhost/",
            "tauri://localhost/index.html",
            "http://tauri.localhost/",
        ] {
            assert!(launcher_url(&tauri::Url::parse(url).unwrap()), "{url}");
        }
        let dev = tauri::Url::parse("http://localhost:1420/").unwrap();
        assert_eq!(launcher_url(&dev), cfg!(debug_assertions));
        for url in [
            "tauri://other/",
            "http://other.localhost/",
            "https://localhost:1420/",
            "tauri://localhost/settings.html",
            "tauri://localhost/other/index.html",
            "https://example.org/",
            "http://localhost:1421/",
        ] {
            assert!(!launcher_url(&tauri::Url::parse(url).unwrap()), "{url}");
        }
    }

    #[test]
    fn info_identity_requires_music_assistant_fields() {
        let valid = r#"{"server_id":"abc","server_version":"2.0","schema_version":28}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{valid}",
            valid.len()
        );
        assert_eq!(serve(response, "/music/info"), Ok(ProbeOutcome::Identified));
        for body in [
            "<html>Login</html>",
            r#"{"server_id":"abc","server_version":"2.0"}"#,
            r#"{"server_id":42,"server_version":"2.0","schema_version":28}"#,
        ] {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            assert_eq!(serve(response, "/music/info"), Err("server"));
        }
    }

    #[test]
    fn info_identity_accepts_browser_auth_but_rejects_unsafe_redirects_and_oversized_bodies() {
        for status in ["401 Unauthorized", "403 Forbidden"] {
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            assert_eq!(
                serve(response, "/music/info"),
                Ok(ProbeOutcome::AuthenticationRequired)
            );
        }
        for location in ["/login", "https://login.example.org/auth"] {
            let response = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            assert_eq!(
                serve(response, "/music/info"),
                Ok(ProbeOutcome::AuthenticationRequired)
            );
        }
        for location in ["file:///etc/passwd", "https://user:pass@example.org/"] {
            let response = format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            assert_eq!(serve(response, "/music/info"), Err("server"));
        }
        let body = "x".repeat(INFO_LIMIT + 1);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        assert_eq!(serve(response, "/music/info"), Err("server"));
    }

    #[test]
    fn old_tokens_cannot_cancel_new_attempts() {
        let mut state = NavigationState::default();
        let first = reserve(&mut state, "main");
        let second = reserve(&mut state, "main");
        assert!(!cancel_token(&mut state, first, "main"));
        assert!(!cancel_token(&mut state, second, "launcher"));
        assert!(owns(&state, second, "main"));
        assert!(cancel_token(&mut state, second, "main"));
        assert!(!owns(&state, second, "main"));
    }

    #[test]
    fn explicit_switch_server_discards_prior_recovery() {
        let mut state = NavigationState::default();
        let generation = reserve(&mut state, "main");
        state.recovering = Some(generation);
        state.watchdog_armed = true;
        cancel_state(&mut state);
        assert!(!owns(&state, generation, "main"));
        assert!(state.pending.is_none());
        assert!(state.recovering.is_none());
        assert!(!state.watchdog_armed);
    }

    #[test]
    fn visible_redirect_does_not_certify_selected_server() {
        let base = parse_base("https://example.org/music/").unwrap();
        let login = tauri::Url::parse("https://login.example.net/auth?secret=hidden").unwrap();
        assert!(visible_document_matches(
            &login,
            "https://login.example.net"
        ));
        assert!(!selected_page_loaded(&base, &login));
        assert!(!visible_document_matches(&login, "https://example.org"));
        assert!(!selected_page_loaded(
            &base,
            &tauri::Url::parse("https://example.org/other").unwrap()
        ));
        assert!(selected_page_loaded(
            &base,
            &tauri::Url::parse("https://example.org/music/login").unwrap()
        ));
        assert!(selected_page_loaded(
            &base,
            &tauri::Url::parse("https://example.org/music").unwrap()
        ));
    }

    #[test]
    fn reservation_discards_previous_pending_and_disarms_watchdog() {
        let mut state = NavigationState::default();
        let first = reserve(&mut state, "main");
        state.pending = Some(Pending {
            generation: first,
            window: "main".into(),
            url: "https://example.org/music/".into(),
            name: None,
        });
        state.watchdog_armed = true;
        let next = reserve(&mut state, "main");
        assert!(!owns(&state, first, "main"));
        assert!(owns(&state, next, "main"));
        assert!(state.pending.is_none());
        assert!(!state.watchdog_armed);
    }

    #[test]
    fn validates_address_and_preserves_prefix() {
        assert_eq!(
            parse_base("https://example.org/music")
                .unwrap()
                .join("info")
                .unwrap()
                .as_str(),
            "https://example.org/music/info"
        );
        for bad in [
            "file:///tmp",
            "https://u:p@example.org",
            "https://example.org/?token=x",
            "https://example.org/#secret",
            "not a url",
        ] {
            assert!(parse_base(bad).is_err(), "{bad}");
        }
    }
}
