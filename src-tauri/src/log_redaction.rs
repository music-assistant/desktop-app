//! Sanitization of formatted diagnostics only; never use this output for navigation.

const REDACTED: &str = "[REDACTED]";

/// Remove URL credentials, queries and fragments, including URLs embedded in errors.
/// All query/fragment data is omitted deliberately: nested URLs and encoded parameter
/// names can carry credentials just as easily as `access_token` or `code`.
pub fn redact(message: &str) -> String {
    let mut result = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(scheme_end) = rest.find("://") {
        let start = rest[..scheme_end]
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_ascii_alphanumeric() && !matches!(c, '+' | '-' | '.'))
            .map_or(0, |(i, c)| i + c.len_utf8());
        result.push_str(&rest[..start]);
        let url = &rest[start..];
        let end = url
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | '`'))
            .unwrap_or(url.len());
        let url = &url[..end];
        let authority_start = scheme_end - start + 3;
        let authority_end = url[authority_start..]
            .find(['/', '?', '#'])
            .map_or(url.len(), |i| authority_start + i);
        result.push_str(&url[..authority_start]);
        let authority = &url[authority_start..authority_end];
        if let Some(at) = authority.rfind('@') {
            result.push_str(REDACTED);
            result.push('@');
            result.push_str(&authority[at + 1..]);
        } else {
            result.push_str(authority);
        }
        let path = &url[authority_end..];
        if let Some(query) = path.find(['?', '#']) {
            result.push_str(&path[..query]);
            result.push_str(REDACTED);
        } else {
            result.push_str(path);
        }
        rest = &rest[start + end..];
    }
    result.push_str(rest);
    redact_fields(&result)
}

// Keep this policy aligned with resources/logging.js. Generic `code` fields are
// diagnostic identifiers (OS errors, HRESULTs); OAuth codes in URLs are removed
// with the entire query/fragment above.
fn sensitive_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "authorization"
            | "password"
            | "client_secret"
            | "api_key"
    )
}

// Covers JSON/debug fields and key=value diagnostics outside URLs. Work on byte
// boundaries only for ASCII syntax, preserving arbitrary UTF-8 diagnostic text.
fn redact_fields(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut result = String::with_capacity(message.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if !sensitive_key(&message[start..i]) {
            continue;
        }
        let mut value = i;
        // A quoted field name may be followed by its closing quote.
        if value < bytes.len() && matches!(bytes[value], b'\'' | b'"') {
            value += 1;
        }
        while value < bytes.len() && bytes[value].is_ascii_whitespace() {
            value += 1;
        }
        if value == bytes.len() || !matches!(bytes[value], b':' | b'=') {
            continue;
        }
        value += 1;
        while value < bytes.len() && bytes[value].is_ascii_whitespace() {
            value += 1;
        }
        if value == bytes.len() {
            continue;
        }
        let quote = matches!(bytes[value], b'\'' | b'"').then_some(bytes[value]);
        if quote.is_some() {
            value += 1;
        }
        let mut end = value;
        while end < bytes.len() {
            if let Some(quote) = quote {
                if bytes[end] == b'\\' {
                    end = (end + 2).min(bytes.len());
                    continue;
                }
                if bytes[end] == quote {
                    break;
                }
            } else if matches!(bytes[end], b'&' | b',' | b';' | b'}' | b'\r' | b'\n')
                || (bytes[end].is_ascii_whitespace()
                    && !message[start..i].eq_ignore_ascii_case("authorization"))
            {
                break;
            }
            end += 1;
        }
        result.push_str(&message[copied..value]);
        result.push_str(REDACTED);
        copied = end;
        i = end;
    }
    result.push_str(&message[copied..]);
    result
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn sanitizes_urls_in_errors_and_preserves_original() {
        let url = "https://alice:secret@[::1]:8095/path?code=abc&access_token=def#token=ghi";
        let message = format!("Connection to {url} failed: request error for ({url})");
        let safe = redact(&message);
        for secret in ["alice", "secret", "abc", "def", "ghi"] {
            assert!(!safe.contains(secret), "leaked {secret}: {safe}");
        }
        assert!(safe.contains("https://[REDACTED]@[::1]:8095/path"));
        assert!(url.contains("code=abc"));
    }

    #[test]
    fn removes_encoded_nested_duplicate_and_fragment_parameters() {
        for url in [
            "http://host/?%63ode=hidden&code=second",
            "HTTPS://host/#/callback?access_token=hidden",
            "wss://host/socket?redirect=https%3A%2F%2Fhost%2F%3Ftoken%3Dhidden",
            "http://user:pa%40ss@host/?TOKEN=hidden",
        ] {
            let safe = redact(url);
            assert!(!safe.contains("hidden"), "{safe}");
            assert!(!safe.contains("pa%40ss"), "{safe}");
            assert!(!safe.contains("second"), "{safe}");
        }
    }

    #[test]
    fn sanitizes_structured_and_plain_error_fields() {
        let safe = redact(
            r#"error {"access_token":"sec\"ret", "refresh_token": "refresh", "password": "password-secret"} token=plain; Authorization: Bearer bearer-secret"#,
        );
        for secret in [
            "sec\\\"ret",
            "refresh\"",
            "password-secret",
            "plain",
            "bearer-secret",
        ] {
            assert!(!safe.contains(secret), "{safe}");
        }
        assert!(safe.contains("[REDACTED]"));
    }

    #[test]
    fn leaves_useful_nonsecret_diagnostics_intact() {
        for message in [
            "Connected to http://localhost:8095/path",
            "Update failed (status 503)",
            "code=ECONNREFUSED connect failed",
            "Os { code: 2, kind: NotFound }",
            "Error { code: HRESULT(0x80070005), message: denied }",
            "Unicode: 音楽 https://host/path failed",
            "音楽https://host/path failed",
            "tokenizer failed; decoder unavailable",
        ] {
            assert_eq!(redact(message), message);
        }
    }

    #[test]
    fn handles_empty_malformed_and_multiple_urls() {
        for message in [
            "",
            "http://",
            "http://u:p@",
            "é code=秘密",
            "token=\"unterminated",
        ] {
            let _ = redact(message);
        }
        assert_eq!(
            redact("http://a/?token=one https://b/#two"),
            "http://a/[REDACTED] https://b/[REDACTED]"
        );
    }
}
