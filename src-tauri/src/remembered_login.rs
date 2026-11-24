use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use tauri::WebviewWindow;

use crate::initial_navigation::{self, VerifiedSelection};

static OPERATIONS: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

const SERVICE: &str = "io.music_assistant.companion.saved-login.v1";
const MAX_USERNAME: usize = 512;
const MAX_PASSWORD: usize = 1024;
// Windows Credential Manager limits a credential blob to 2560 bytes. JSON escaping
// can increase its size, so check the serialized form too.
const MAX_BLOB: usize = 2400;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    supported: bool,
    available: bool,
    saved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<String>,
}

impl LoginStatus {
    fn unavailable() -> Self {
        Self {
            supported: true,
            available: false,
            saved: false,
            username: None,
            context: None,
        }
    }

    fn available(context: String, username: Option<String>) -> Self {
        Self {
            supported: true,
            available: true,
            saved: username.is_some(),
            username,
            context: Some(context),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct RestoredLogin {
    username: String,
    password: String,
    context: String,
}

#[derive(Serialize, Deserialize)]
struct StoredLogin {
    version: u8,
    username: String,
    password: String,
}

// Each command uses the same operation helpers as the tests. The production
// implementation only writes to the operating system credential store.
trait CredentialStore: Clone + Send + 'static {
    fn get(&self, key: &str) -> Result<Option<String>, ()>;
    fn put(&self, key: &str, value: &str) -> Result<(), ()>;
    fn delete(&self, key: &str) -> Result<(), ()>;
}

#[derive(Clone, Copy)]
struct KeyringStore;

impl CredentialStore for KeyringStore {
    fn get(&self, key: &str) -> Result<Option<String>, ()> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|_| ())?;
        match entry.get_password() {
            Ok(raw) => Ok(Some(raw)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(()),
        }
    }

    fn put(&self, key: &str, value: &str) -> Result<(), ()> {
        keyring::Entry::new(SERVICE, key)
            .map_err(|_| ())?
            .set_password(value)
            .map_err(|_| ())
    }

    fn delete(&self, key: &str) -> Result<(), ()> {
        let entry = keyring::Entry::new(SERVICE, key).map_err(|_| ())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(()),
        }
    }
}

fn store_key(selection: &VerifiedSelection) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"music-assistant-companion-login-v1\0");
    hash.update(selection.base.as_bytes());
    hash.update([0]);
    hash.update(selection.server_id.as_bytes());
    format!("{:x}", hash.finalize())
}

fn valid_fields(username: &str, password: &str) -> bool {
    !username.is_empty()
        && username.len() <= MAX_USERNAME
        && !password.is_empty()
        && password.len() <= MAX_PASSWORD
}

fn decode(raw: &str) -> Result<StoredLogin, String> {
    if raw.len() > MAX_BLOB {
        return Err("unavailable".into());
    }
    let data: StoredLogin = serde_json::from_str(raw).map_err(|_| "unavailable")?;
    if data.version != 1 || !valid_fields(&data.username, &data.password) {
        return Err("unavailable".into());
    }
    Ok(data)
}

fn encode(username: String, password: String) -> Result<String, String> {
    if !valid_fields(&username, &password) {
        return Err("invalid credentials".into());
    }
    let raw = serde_json::to_string(&StoredLogin {
        version: 1,
        username,
        password,
    })
    .map_err(|_| "unavailable")?;
    if raw.len() > MAX_BLOB {
        return Err("invalid credentials".into());
    }
    Ok(raw)
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|_| "unavailable".to_owned())?
}

fn selected(
    guard: &impl Fn() -> Result<VerifiedSelection, String>,
    context: Option<&str>,
    nonce: &str,
) -> Result<VerifiedSelection, String> {
    let selection = guard()?;
    if context.is_some_and(|value| value != selection.context)
        || !initial_navigation::valid_document_nonce(&selection, nonce)
    {
        return Err("unavailable".into());
    }
    Ok(selection)
}

async fn status_with<S: CredentialStore>(
    store: S,
    guard: impl Fn() -> Result<VerifiedSelection, String>,
    nonce: &str,
) -> LoginStatus {
    let Ok(selection) = selected(&guard, None, nonce) else {
        return LoginStatus::unavailable();
    };
    let context = selection.context.clone();
    let key = store_key(&selection);
    let Ok(raw) = blocking(move || store.get(&key).map_err(|_| "unavailable".into())).await else {
        return LoginStatus::unavailable();
    };
    let data = match raw {
        Some(raw) => match decode(&raw) {
            Ok(data) => Some(data),
            Err(_) => return LoginStatus::unavailable(),
        },
        None => None,
    };
    if selected(&guard, Some(&context), nonce).ok().as_ref() != Some(&selection) {
        return LoginStatus::unavailable();
    }
    LoginStatus::available(context, data.map(|value| value.username))
}

async fn restore_with<S: CredentialStore>(
    store: S,
    guard: impl Fn() -> Result<VerifiedSelection, String>,
    context: String,
    nonce: &str,
) -> Result<RestoredLogin, String> {
    let selection = selected(&guard, Some(&context), nonce)?;
    let key = store_key(&selection);
    let raw = blocking(move || store.get(&key).map_err(|_| "unavailable".into()))
        .await?
        .ok_or("not saved")?;
    let data = decode(&raw)?;
    if selected(&guard, Some(&context), nonce)? != selection {
        return Err("unavailable".into());
    }
    Ok(RestoredLogin {
        username: data.username,
        password: data.password,
        context,
    })
}

async fn save_with<S: CredentialStore>(
    store: S,
    guard: impl Fn() -> Result<VerifiedSelection, String>,
    username: String,
    password: String,
    context: String,
    nonce: &str,
) -> Result<LoginStatus, String> {
    let selection = selected(&guard, Some(&context), nonce)?;
    let raw = encode(username.clone(), password)?;
    let key = store_key(&selection);
    blocking(move || store.put(&key, &raw).map_err(|_| "unavailable".into())).await?;
    if selected(&guard, Some(&context), nonce)? != selection {
        return Err("unavailable".into());
    }
    Ok(LoginStatus::available(context, Some(username)))
}

async fn forget_with<S: CredentialStore>(
    store: S,
    guard: impl Fn() -> Result<VerifiedSelection, String>,
    context: String,
    nonce: &str,
) -> Result<LoginStatus, String> {
    let selection = selected(&guard, Some(&context), nonce)?;
    let key = store_key(&selection);
    blocking(move || store.delete(&key).map_err(|_| "unavailable".into())).await?;
    if selected(&guard, Some(&context), nonce)? != selection {
        return Err("unavailable".into());
    }
    Ok(LoginStatus::available(context, None))
}

#[tauri::command]
pub async fn companion_login_status(window: WebviewWindow, nonce: String) -> LoginStatus {
    let _operation = OPERATIONS.lock().await;
    status_with(
        KeyringStore,
        || initial_navigation::active_selection(&window, None),
        &nonce,
    )
    .await
}

#[tauri::command]
pub async fn companion_restore_login(
    window: WebviewWindow,
    context: String,
    nonce: String,
) -> Result<RestoredLogin, String> {
    let _operation = OPERATIONS.lock().await;
    restore_with(
        KeyringStore,
        || initial_navigation::active_selection(&window, Some(&context)),
        context.clone(),
        &nonce,
    )
    .await
}

#[tauri::command]
pub async fn companion_save_login(
    window: WebviewWindow,
    username: String,
    password: String,
    context: String,
    nonce: String,
) -> Result<LoginStatus, String> {
    let _operation = OPERATIONS.lock().await;
    // The frontend calls this only after authentication succeeds; native cannot prove that claim.
    save_with(
        KeyringStore,
        || initial_navigation::active_selection(&window, Some(&context)),
        username,
        password,
        context.clone(),
        &nonce,
    )
    .await
}

#[tauri::command]
pub async fn companion_forget_login(
    window: WebviewWindow,
    context: String,
    nonce: String,
) -> Result<LoginStatus, String> {
    let _operation = OPERATIONS.lock().await;
    forget_with(
        KeyringStore,
        || initial_navigation::active_selection(&window, Some(&context)),
        context.clone(),
        &nonce,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct MockStore(Arc<Mutex<MockState>>);

    #[derive(Default)]
    struct MockState {
        entries: HashMap<String, String>,
        calls: Vec<&'static str>,
        fail: bool,
        after_call: Option<Box<dyn FnOnce() + Send>>,
    }

    impl CredentialStore for MockStore {
        fn get(&self, key: &str) -> Result<Option<String>, ()> {
            let (result, callback) = {
                let mut state = self.0.lock().unwrap();
                state.calls.push("get");
                (
                    if state.fail {
                        Err(())
                    } else {
                        Ok(state.entries.get(key).cloned())
                    },
                    state.after_call.take(),
                )
            };
            if let Some(callback) = callback {
                callback();
            }
            result
        }
        fn put(&self, key: &str, value: &str) -> Result<(), ()> {
            let callback = {
                let mut state = self.0.lock().unwrap();
                state.calls.push("put");
                if state.fail {
                    return Err(());
                }
                state.entries.insert(key.into(), value.into());
                state.after_call.take()
            };
            if let Some(callback) = callback {
                callback();
            }
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), ()> {
            let callback = {
                let mut state = self.0.lock().unwrap();
                state.calls.push("delete");
                if state.fail {
                    return Err(());
                }
                state.entries.remove(key);
                state.after_call.take()
            };
            if let Some(callback) = callback {
                callback();
            }
            Ok(())
        }
    }

    fn selection(base: &str, id: &str) -> VerifiedSelection {
        VerifiedSelection {
            generation: 1,
            window: "main".into(),
            base: base.into(),
            server_id: id.into(),
            context: "context".into(),
            nonce: Some("nonce".into()),
        }
    }

    fn current(state: &Arc<Mutex<Option<VerifiedSelection>>>) -> Result<VerifiedSelection, String> {
        state.lock().unwrap().clone().ok_or("unavailable".into())
    }

    #[tokio::test]
    async fn command_operations_save_restore_status_and_forget_without_host_store() {
        let store = MockStore::default();
        let selection = selection("https://example.org/music/", "one");
        let state = Arc::new(Mutex::new(Some(selection.clone())));
        let guard = || current(&state);
        let empty = status_with(store.clone(), guard, "nonce").await;
        assert!(empty.available && !empty.saved && empty.context.as_deref() == Some("context"));
        let saved = save_with(
            store.clone(),
            guard,
            "alice".into(),
            "secret".into(),
            "context".into(),
            "nonce",
        )
        .await
        .unwrap();
        assert!(saved.saved && saved.username.as_deref() == Some("alice"));
        let status = status_with(store.clone(), guard, "nonce").await;
        assert!(status.saved && status.username.as_deref() == Some("alice"));
        let restored = restore_with(store.clone(), guard, "context".into(), "nonce")
            .await
            .unwrap();
        assert_eq!(
            (restored.username.as_str(), restored.password.as_str()),
            ("alice", "secret")
        );
        let deleted = forget_with(store.clone(), guard, "context".into(), "nonce")
            .await
            .unwrap();
        assert!(deleted.available && !deleted.saved);
        assert!(!status_with(store.clone(), guard, "nonce").await.saved);
        assert_eq!(
            store.0.lock().unwrap().calls,
            ["get", "put", "get", "get", "delete", "get"]
        );
    }

    #[tokio::test]
    async fn invalid_nonce_context_or_selection_never_touches_store() {
        let store = MockStore::default();
        let state = Arc::new(Mutex::new(Some(selection(
            "https://example.org/music/",
            "one",
        ))));
        let guard = || current(&state);
        assert!(!status_with(store.clone(), guard, "bad").await.available);
        assert!(restore_with(store.clone(), guard, "context".into(), "bad")
            .await
            .is_err());
        assert!(save_with(
            store.clone(),
            guard,
            "alice".into(),
            "secret".into(),
            "wrong".into(),
            "nonce"
        )
        .await
        .is_err());
        assert!(forget_with(store.clone(), guard, "wrong".into(), "nonce")
            .await
            .is_err());
        state.lock().unwrap().take();
        assert!(!status_with(store.clone(), guard, "nonce").await.available);
        assert_eq!(store.0.lock().unwrap().calls.len(), 0);
    }

    #[tokio::test]
    async fn no_entry_invalid_data_and_backend_failure_fail_closed() {
        let store = MockStore::default();
        let selected = selection("https://example.org/music/", "one");
        let guard = || Ok(selected.clone());
        assert_eq!(
            restore_with(store.clone(), guard, "context".into(), "nonce")
                .await
                .unwrap_err(),
            "not saved"
        );
        let key = store_key(&selected);
        for raw in [
            "not JSON",
            r#"{"version":2,"username":"alice","password":"secret"}"#,
            r#"{"version":1,"username":"","password":"secret"}"#,
            r#"{"version":1,"username":"alice","password":""}"#,
        ] {
            store
                .0
                .lock()
                .unwrap()
                .entries
                .insert(key.clone(), raw.into());
            assert!(!status_with(store.clone(), guard, "nonce").await.available);
            assert_eq!(
                restore_with(store.clone(), guard, "context".into(), "nonce")
                    .await
                    .unwrap_err(),
                "unavailable"
            );
        }
        store
            .0
            .lock()
            .unwrap()
            .entries
            .insert(key.clone(), "x".repeat(MAX_BLOB + 1));
        assert!(!status_with(store.clone(), guard, "nonce").await.available);
        for (field, raw) in [
            ("username", "u".repeat(MAX_USERNAME + 1)),
            ("password", "p".repeat(MAX_PASSWORD + 1)),
        ] {
            let record = if field == "username" {
                serde_json::json!({"version":1,"username":raw,"password":"secret"})
            } else {
                serde_json::json!({"version":1,"username":"alice","password":raw})
            };
            store
                .0
                .lock()
                .unwrap()
                .entries
                .insert(key.clone(), record.to_string());
            assert!(!status_with(store.clone(), guard, "nonce").await.available);
        }
        store.0.lock().unwrap().fail = true;
        assert!(!status_with(store.clone(), guard, "nonce").await.available);
        assert_eq!(
            restore_with(store.clone(), guard, "context".into(), "nonce")
                .await
                .unwrap_err(),
            "unavailable"
        );
        assert_eq!(
            save_with(
                store.clone(),
                guard,
                "alice".into(),
                "secret".into(),
                "context".into(),
                "nonce"
            )
            .await
            .unwrap_err(),
            "unavailable"
        );
        assert_eq!(
            forget_with(store.clone(), guard, "context".into(), "nonce")
                .await
                .unwrap_err(),
            "unavailable"
        );
    }

    #[tokio::test]
    async fn stale_selection_after_store_read_or_write_never_returns_credentials_or_success() {
        let store = MockStore::default();
        let selected = selection("https://example.org/music/", "one");
        let state = Arc::new(Mutex::new(Some(selected.clone())));
        let guard = || current(&state);
        store.0.lock().unwrap().entries.insert(
            store_key(&selected),
            encode("alice".into(), "secret".into()).unwrap(),
        );
        let revoke = |store: &MockStore| {
            let state = Arc::clone(&state);
            store.0.lock().unwrap().after_call = Some(Box::new(move || {
                state.lock().unwrap().take();
            }));
        };
        revoke(&store);
        assert!(!status_with(store.clone(), guard, "nonce").await.available);
        *state.lock().unwrap() = Some(selected.clone());
        revoke(&store);
        assert_eq!(
            restore_with(store.clone(), guard, "context".into(), "nonce")
                .await
                .unwrap_err(),
            "unavailable"
        );
        *state.lock().unwrap() = Some(selected.clone());
        revoke(&store);
        assert_eq!(
            save_with(
                store.clone(),
                guard,
                "alice".into(),
                "new".into(),
                "context".into(),
                "nonce"
            )
            .await
            .unwrap_err(),
            "unavailable"
        );
        *state.lock().unwrap() = Some(selected);
        revoke(&store);
        assert_eq!(
            forget_with(store.clone(), guard, "context".into(), "nonce")
                .await
                .unwrap_err(),
            "unavailable"
        );
    }

    #[test]
    fn server_identity_origin_and_base_path_have_distinct_store_keys() {
        let original = selection("https://example.org/music/", "one");
        for other in [
            selection("https://example.org/music/", "two"),
            selection("https://elsewhere.org/music/", "one"),
            selection("https://example.org/other/", "one"),
        ] {
            assert_ne!(store_key(&original), store_key(&other));
        }
    }

    #[tokio::test]
    async fn invalid_inputs_never_reach_store_and_responses_omit_password() {
        let store = MockStore::default();
        let selected = selection("https://example.org/music/", "one");
        let guard = || Ok(selected.clone());
        for (user, password) in [
            (String::new(), "secret".into()),
            ("alice".into(), String::new()),
            ("u".repeat(MAX_USERNAME + 1), "secret".into()),
            ("alice".into(), "p".repeat(MAX_PASSWORD + 1)),
            ("\"".repeat(MAX_USERNAME), "\\".repeat(MAX_PASSWORD)),
        ] {
            assert!(save_with(
                store.clone(),
                guard,
                user,
                password,
                "context".into(),
                "nonce"
            )
            .await
            .is_err());
        }
        assert_eq!(store.0.lock().unwrap().calls.len(), 0);
        let status = save_with(
            store,
            guard,
            "alice".into(),
            "secret".into(),
            "context".into(),
            "nonce",
        )
        .await
        .unwrap();
        let response = serde_json::to_string(&status).unwrap();
        assert!(!response.contains("secret"));
        assert!(!response.contains("password"));
    }
}
