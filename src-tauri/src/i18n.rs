use serde::Serialize;
use serde_json::{Map, Value};
use std::fs;
use std::sync::{OnceLock, RwLock};
use tauri::{Manager, Runtime};

const SOURCE_LANGUAGE: &str = "en";
const SOURCE_MESSAGES: &str = include_str!("../resources/translations/en.json");

#[derive(Clone, Debug, Serialize)]
pub struct I18nBundle {
    pub locale: String,
    pub messages: Value,
    pub fallback_messages: Value,
}

static BUNDLE: OnceLock<RwLock<I18nBundle>> = OnceLock::new();

pub fn init<R: Runtime>(app: &tauri::AppHandle<R>) {
    let fallback_messages = parse_messages(SOURCE_MESSAGES);

    let mut selected_locale = SOURCE_LANGUAGE.to_string();
    let mut messages = fallback_messages.clone();

    for locale in preferred_locale_candidates(sys_locale::get_locales()) {
        if locale == SOURCE_LANGUAGE {
            break;
        }
        if let Some(candidate_messages) = load_locale(app, &locale) {
            selected_locale = locale;
            messages = candidate_messages;
            break;
        }
    }

    let bundle = I18nBundle {
        locale: selected_locale,
        messages,
        fallback_messages,
    };

    if let Some(lock) = BUNDLE.get() {
        if let Ok(mut current) = lock.write() {
            *current = bundle;
        }
    } else {
        let _ = BUNDLE.set(RwLock::new(bundle));
    }
}

pub fn bundle() -> I18nBundle {
    BUNDLE
        .get()
        .and_then(|lock| lock.read().ok().map(|bundle| bundle.clone()))
        .unwrap_or_else(|| {
            let fallback_messages = parse_messages(SOURCE_MESSAGES);
            I18nBundle {
                locale: SOURCE_LANGUAGE.to_string(),
                messages: fallback_messages.clone(),
                fallback_messages,
            }
        })
}

pub fn tr(key: &str) -> String {
    let bundle = bundle();
    lookup(&bundle.messages, key)
        .or_else(|| lookup(&bundle.fallback_messages, key))
        .unwrap_or(key)
        .to_string()
}

fn parse_messages(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::Object(Map::default()))
}

fn load_locale<R: Runtime>(app: &tauri::AppHandle<R>, locale: &str) -> Option<Value> {
    let path = app
        .path()
        .resource_dir()
        .ok()?
        .join("resources")
        .join("translations")
        .join(format!("{locale}.json"));
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn preferred_locale_candidates(locales: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut candidates = Vec::new();

    for locale in locales {
        for candidate in locale_candidates(&normalize_locale(locale)) {
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }

    if !candidates.iter().any(|locale| locale == SOURCE_LANGUAGE) {
        candidates.push(SOURCE_LANGUAGE.to_string());
    }

    candidates
}

fn locale_candidates(locale: &str) -> Vec<String> {
    let mut candidates = vec![locale.to_string()];
    if let Some(base) = locale.split('_').next() {
        if base != locale {
            candidates.push(base.to_string());
        }
    }
    candidates
}

fn normalize_locale(locale: String) -> String {
    locale.replace('-', "_")
}

fn lookup<'a>(messages: &'a Value, key: &str) -> Option<&'a str> {
    let mut current = messages;
    for part in key.split('.') {
        current = current.get(part)?;
    }
    current.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_locale_candidates_include_base() {
        assert_eq!(locale_candidates("pt_BR"), vec!["pt_BR", "pt"]);
        assert_eq!(locale_candidates("nl"), vec!["nl"]);
    }

    #[test]
    fn test_preferred_locale_candidates_include_all_preferred_locales_and_english() {
        assert_eq!(
            preferred_locale_candidates(["en-NL".to_string(), "nl-NL".to_string()]),
            vec!["en_NL", "en", "nl_NL", "nl"]
        );
    }

    #[test]
    fn test_preferred_locale_candidates_fall_back_to_english_when_empty() {
        assert_eq!(preferred_locale_candidates(Vec::new()), vec!["en"]);
    }

    #[test]
    fn test_lookup_nested_key() {
        let messages = parse_messages(r#"{"desktop":{"app":{"name":"Music Assistant"}}}"#);
        assert_eq!(
            lookup(&messages, "desktop.app.name"),
            Some("Music Assistant")
        );
    }
}
