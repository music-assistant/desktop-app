//! Stage webview downloads privately, then offer a native Save As dialog on completion.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tauri::webview::DownloadEvent;
use tauri::Manager;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

const MAX_ACTIVE: usize = 32;
const FALLBACK_NAME: &str = "download";

type DownloadKey = (String, String);

struct Stage {
    directory: tempfile::TempDir,
    name: String,
    key: DownloadKey,
    ambiguous: bool,
    live_stages: Arc<AtomicUsize>,
}

impl Drop for Stage {
    fn drop(&mut self) {
        self.live_stages.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Tracker {
    live_stages: Arc<AtomicUsize>,
    stages: HashMap<PathBuf, Stage>,
    by_url: HashMap<DownloadKey, Vec<PathBuf>>,
    poisoned: HashSet<DownloadKey>,
    // Pathless duplicate completions cannot identify which file is still being written.
    // Hold all stages until the group's final completion (or window destruction).
    held: HashMap<DownloadKey, Vec<Stage>>,
    // Keys whose rejected completion might arrive after the last accepted one.
    // These are bounded by the number of accepted groups and reset on window destruction.
    retired: HashSet<DownloadKey>,
    // Reject future downloads after an untracked pathless completion could arrive.
    // Window labels come from app-created webviews, not downloaded URLs.
    blocked_windows: HashSet<String>,
}

static TRACKER: OnceLock<Mutex<Tracker>> = OnceLock::new();

fn tracker() -> &'static Mutex<Tracker> {
    TRACKER.get_or_init(|| Mutex::new(Tracker::default()))
}

fn suggested_name(destination: &Path) -> String {
    // Wry supplies the suggested name here, including the HTML download attribute.
    // Never derive names from a URL (which may contain credentials or opaque data).
    sanitize_name(
        destination
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default(),
    )
}

fn sanitize_name(candidate: &str) -> String {
    let name: String = candidate
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim_matches(|c: char| c == '.' || c == ' ');
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if name.is_empty() || name.len() > 200 || reserved {
        FALLBACK_NAME.to_owned()
    } else {
        name.to_owned()
    }
}

impl Tracker {
    fn retained_count(&self) -> usize {
        // Includes stages owned by pending dialogs and background copies.
        self.live_stages.load(Ordering::Relaxed)
    }

    fn can_request(&self, key: &DownloadKey) -> bool {
        !self.blocked_windows.contains(&key.0)
            && self.retained_count() < MAX_ACTIVE
            && !self.retired.contains(key)
    }

    fn block_window(&mut self, key: &DownloadKey) {
        self.blocked_windows.insert(key.0.clone());
        // Rejection may itself emit Finished(None). A pending request with
        // this URL must never be mistaken for that rejected completion.
        if let Some(paths) = self.by_url.get(key) {
            self.poisoned.insert(key.clone());
            for path in paths {
                if let Some(stage) = self.stages.get_mut(path) {
                    stage.ambiguous = true;
                }
            }
        }
    }

    fn insert(&mut self, key: DownloadKey, directory: tempfile::TempDir, name: String) -> PathBuf {
        // The destination must not exist yet: WebKit creates it itself.
        let path = directory.path().join("download");
        let peers = self.by_url.entry(key.clone()).or_default();
        if !peers.is_empty() {
            for peer in peers.iter() {
                if let Some(stage) = self.stages.get_mut(peer) {
                    stage.ambiguous = true;
                }
            }
        }
        let ambiguous =
            !peers.is_empty() || self.poisoned.contains(&key) || self.retired.contains(&key);
        peers.push(path.clone());
        self.live_stages.fetch_add(1, Ordering::Relaxed);
        self.stages.insert(
            path.clone(),
            Stage {
                directory,
                name,
                key,
                ambiguous,
                live_stages: Arc::clone(&self.live_stages),
            },
        );
        path
    }

    fn finish(&mut self, key: &DownloadKey, path: Option<&Path>) -> Option<(Option<Stage>, bool)> {
        let selected = if let Some(path) = path {
            self.stages.get(path).filter(|stage| &stage.key == key)?;
            path.to_path_buf()
        } else {
            // On macOS Tauri always reports None, even on success. Multiple
            // simultaneous downloads of one URL cannot be assigned to a stage.
            self.by_url.get(key)?.first()?.clone()
        };
        let stage = self.stages.remove(&selected)?;
        let last_in_group = if let Some(peers) = self.by_url.get_mut(key) {
            peers.retain(|peer| peer != &selected);
            peers.is_empty()
        } else {
            false
        };
        if stage.ambiguous && path.is_none() {
            // The chosen stage may still belong to another download in this
            // group. Do not remove its directory while WebKit could write to it.
            self.held.entry(key.clone()).or_default().push(stage);
            if last_in_group && !self.poisoned.contains(key) {
                self.by_url.remove(key);
                self.held.remove(key);
                return Some((None, true));
            }
            if last_in_group {
                self.retired.insert(key.clone());
                self.by_url.remove(key);
            }
            return None;
        }
        if last_in_group {
            self.by_url.remove(key);
            if self.poisoned.remove(key) {
                self.retired.insert(key.clone());
            } else {
                self.held.remove(key);
            }
        }
        Some((Some(stage), last_in_group))
    }

    fn remove_window(&mut self, label: &str) {
        let keys: Vec<_> = self
            .by_url
            .keys()
            .chain(self.retired.iter())
            .filter(|(window, _)| window == label)
            .cloned()
            .collect();
        for key in keys {
            self.held.remove(&key);
            if let Some(paths) = self.by_url.remove(&key) {
                for path in paths {
                    self.stages.remove(&path);
                }
            }
            self.poisoned.remove(&key);
            self.retired.remove(&key);
        }
        self.blocked_windows.remove(label);
    }
}

pub fn window_destroyed(label: &str) {
    if let Ok(mut state) = tracker().lock() {
        state.remove_window(label);
    }
}

fn failure<R: tauri::Runtime>(app: &tauri::AppHandle<R>, title: &str, message: &str) {
    app.dialog()
        .message(crate::i18n::tr(message))
        .title(crate::i18n::tr(title))
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

pub fn on_download<R: tauri::Runtime>(
    webview: tauri::Webview<R>,
    event: DownloadEvent<'_>,
) -> bool {
    let window = webview.window();
    match event {
        DownloadEvent::Requested { url, destination } => {
            let key = (webview.label().to_owned(), url.as_str().to_owned());
            let Ok(mut state) = tracker().lock() else {
                failure(
                    window.app_handle(),
                    "desktop.downloads.failed_title",
                    "desktop.downloads.failed_message",
                );
                return false;
            };
            if !state.can_request(&key) {
                if state.retained_count() >= MAX_ACTIVE {
                    state.block_window(&key);
                }
                return false;
            }
            let name = suggested_name(destination);
            let Ok(directory) = tempfile::Builder::new()
                .prefix("music-assistant-download-")
                .tempdir()
            else {
                state.block_window(&key);
                drop(state);
                failure(
                    window.app_handle(),
                    "desktop.downloads.failed_title",
                    "desktop.downloads.failed_message",
                );
                return false;
            };
            *destination = state.insert(key, directory, name);
            true
        }
        DownloadEvent::Finished { url, path, success } => {
            let key = (webview.label().to_owned(), url.as_str().to_owned());
            let stage = tracker()
                .lock()
                .ok()
                .and_then(|mut state| state.finish(&key, path.as_deref()));
            let Some((stage, last_in_group)) = stage else {
                return true;
            };
            let Some(stage) = stage else {
                if last_in_group {
                    failure(
                        window.app_handle(),
                        "desktop.downloads.failed_title",
                        "desktop.downloads.failed_message",
                    );
                }
                return true;
            };
            if stage.ambiguous {
                if last_in_group {
                    failure(
                        window.app_handle(),
                        "desktop.downloads.failed_title",
                        "desktop.downloads.failed_message",
                    );
                }
                return true;
            }
            if !success || !stage.directory.path().join("download").is_file() {
                failure(
                    window.app_handle(),
                    "desktop.downloads.failed_title",
                    "desktop.downloads.failed_message",
                );
                return true;
            }
            let app = window.app_handle().clone();
            let mut dialog = app
                .dialog()
                .file()
                .set_title(crate::i18n::tr("desktop.downloads.save_title"))
                .set_file_name(stage.name.clone());
            dialog = dialog.set_parent(&window);
            dialog.save_file(move |selected| {
                let Some(selected) = selected else { return };
                let Ok(destination) = selected.into_path() else {
                    failure(
                        &app,
                        "desktop.downloads.save_failed_title",
                        "desktop.downloads.save_failed_message",
                    );
                    return;
                };
                std::thread::spawn(move || {
                    // Keep the stage alive until the copy completes, including dialog cancellation.
                    let source = stage.directory.path().join("download");
                    if std::fs::copy(source, destination).is_err() {
                        failure(
                            &app,
                            "desktop.downloads.save_failed_title",
                            "desktop.downloads.save_failed_message",
                        );
                    }
                });
            });
            true
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage(tracker: &mut Tracker, url: &str) -> PathBuf {
        tracker.insert(
            ("main".into(), url.into()),
            tempfile::tempdir().unwrap(),
            "file.txt".into(),
        )
    }

    #[test]
    fn completed_and_failed_downloads_are_removed() {
        let mut tracker = Tracker::default();
        let first = stage(&mut tracker, "https://example.org/first");
        assert!(tracker
            .finish(
                &("main".into(), "https://example.org/first".into()),
                Some(&first)
            )
            .is_some());
        let _second = stage(&mut tracker, "https://example.org/second");
        assert!(tracker
            .finish(&("main".into(), "https://example.org/second".into()), None)
            .is_some());
        assert!(tracker.stages.is_empty());
        assert!(tracker.by_url.is_empty());
    }

    #[test]
    fn duplicate_urls_are_never_assigned_as_safe_downloads() {
        let mut tracker = Tracker::default();
        let first = stage(&mut tracker, "blob:duplicate");
        let second = stage(&mut tracker, "blob:duplicate");
        let key = ("main".into(), "blob:duplicate".into());
        assert!(tracker.finish(&key, None).is_none());
        assert!(first.parent().unwrap().exists());
        assert!(second.parent().unwrap().exists());
        let (stage, last) = tracker.finish(&key, None).unwrap();
        assert!(stage.is_none());
        assert!(last);
        assert!(!first.parent().unwrap().exists());
        assert!(!second.parent().unwrap().exists());
        assert!(tracker.stages.is_empty());
        assert!(tracker.held.is_empty());
    }

    #[test]
    fn rejected_duplicate_keeps_stages_until_window_destroyed() {
        let mut tracker = Tracker::default();
        let first = stage(&mut tracker, "blob:duplicate");
        let key = ("main".into(), "blob:duplicate".into());
        tracker.poisoned.insert(key.clone());
        tracker.stages.get_mut(&first).unwrap().ambiguous = true;
        assert!(tracker.finish(&key, None).is_none());
        assert!(first.parent().unwrap().exists());
        tracker.remove_window("main");
        assert!(!first.parent().unwrap().exists());
        assert!(tracker.held.is_empty());
        assert!(tracker.retired.is_empty());
    }

    #[test]
    fn late_rejected_completion_cannot_claim_new_stage() {
        let mut tracker = Tracker::default();
        let first = stage(&mut tracker, "blob:duplicate");
        let key = ("main".into(), "blob:duplicate".into());
        tracker.poisoned.insert(key.clone());
        tracker.stages.get_mut(&first).unwrap().ambiguous = true;
        assert!(tracker.finish(&key, None).is_none());
        let next = stage(&mut tracker, "blob:duplicate");
        assert!(tracker.stages.get(&next).unwrap().ambiguous);
        tracker.remove_window("main");
        assert!(tracker.retired.is_empty());
    }

    #[test]
    fn held_stages_count_toward_capacity_and_block_only_one_window() {
        let mut tracker = Tracker::default();
        let key = ("main".to_owned(), "blob:duplicate".to_owned());
        let first = stage(&mut tracker, "blob:duplicate");
        let second = stage(&mut tracker, "blob:duplicate");
        assert!(tracker.finish(&key, None).is_none());
        assert_eq!(tracker.retained_count(), 2);
        assert!(first.parent().unwrap().exists());
        assert!(second.parent().unwrap().exists());
        for index in 2..MAX_ACTIVE {
            stage(&mut tracker, &format!("blob:{index}"));
        }
        assert_eq!(tracker.retained_count(), MAX_ACTIVE);
        let rejected = ("main".to_owned(), "blob:rejected".to_owned());
        assert!(!tracker.can_request(&rejected));
        tracker.block_window(&rejected);
        // Repeated rejected URLs must not grow a URL-keyed map.
        for index in 0..100 {
            let other = ("main".to_owned(), format!("blob:rejected-{index}"));
            assert!(!tracker.can_request(&other));
            tracker.block_window(&other);
        }
        assert!(tracker.poisoned.is_empty());
        assert!(tracker.retired.is_empty());
        assert_eq!(tracker.blocked_windows.len(), 1);
        assert_eq!(tracker.held.values().map(Vec::len).sum::<usize>(), 1);
        let available = ("launcher".to_owned(), "blob:allowed".to_owned());
        tracker.finish(&("main".to_owned(), "blob:2".to_owned()), None);
        assert!(tracker.can_request(&available));
        tracker.insert(
            available.clone(),
            tempfile::tempdir().unwrap(),
            "file".to_owned(),
        );
        assert!(!tracker.can_request(&rejected));
        tracker.remove_window("main");
        assert!(tracker.blocked_windows.is_empty());
        assert_eq!(tracker.stages.len(), 1);
        assert!(tracker.by_url.contains_key(&available));
        assert!(tracker.can_request(&rejected));
    }

    #[test]
    fn completed_stage_still_uses_capacity_while_owned_by_dialog_or_copy() {
        let mut tracker = Tracker::default();
        let mut pending = Vec::new();
        for index in 0..MAX_ACTIVE {
            let url = format!("blob:finished-{index}");
            let key = ("main".to_owned(), url.clone());
            let path = stage(&mut tracker, &url);
            let (finished, last) = tracker.finish(&key, Some(&path)).unwrap();
            assert!(last);
            pending.push(finished.unwrap());
        }
        assert!(tracker.stages.is_empty());
        assert!(tracker.held.is_empty());
        assert_eq!(tracker.retained_count(), MAX_ACTIVE);
        let next = ("launcher".to_owned(), "blob:next".to_owned());
        assert!(!tracker.can_request(&next));
        drop(pending.pop());
        assert_eq!(tracker.retained_count(), MAX_ACTIVE - 1);
        assert!(tracker.can_request(&next));
        drop(pending);
        assert_eq!(tracker.retained_count(), 0);
    }

    #[test]
    fn retired_url_cannot_be_readmitted_and_held_is_bounded() {
        let mut tracker = Tracker::default();
        let key = ("main".to_owned(), "blob:same".to_owned());
        let path = stage(&mut tracker, "blob:same");
        tracker.block_window(&key);
        assert!(tracker.finish(&key, None).is_none());
        assert!(tracker.retired.contains(&key));
        assert!(path.parent().unwrap().exists());
        assert!(!tracker.can_request(&key));
        assert_eq!(tracker.retained_count(), 1);
        tracker.remove_window("main");
        assert!(tracker.retired.is_empty());
        assert!(tracker.poisoned.is_empty());
        assert!(tracker.held.is_empty());
        assert!(!path.parent().unwrap().exists());
        assert!(tracker.can_request(&key));
    }

    #[test]
    fn destroyed_window_discards_stages() {
        let mut tracker = Tracker::default();
        let path = stage(&mut tracker, "blob:cancel");
        let _other = tracker.insert(
            ("launcher".into(), "blob:other".into()),
            tempfile::tempdir().unwrap(),
            "file".into(),
        );
        tracker.remove_window("main");
        assert!(!tracker.stages.contains_key(&path));
        assert_eq!(tracker.stages.len(), 1);
    }

    #[test]
    fn suggested_filename_is_safe() {
        assert_eq!(sanitize_name("../bad\\name?.txt"), "_bad_name_.txt");
        assert_eq!(sanitize_name(".."), FALLBACK_NAME);
        assert_eq!(sanitize_name("CON.txt"), FALLBACK_NAME);
        assert_eq!(sanitize_name("lpt1"), FALLBACK_NAME);
        assert_eq!(sanitize_name(""), FALLBACK_NAME);
        assert_eq!(sanitize_name("trailing. "), "trailing");
        assert_eq!(suggested_name(Path::new("/downloads/log.json")), "log.json");
        assert_eq!(
            suggested_name(Path::new("/downloads/blob-recording.wav")),
            "blob-recording.wav"
        );
        assert_eq!(suggested_name(Path::new("")), FALLBACK_NAME);
    }
}
