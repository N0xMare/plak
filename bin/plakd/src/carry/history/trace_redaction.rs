use plakous::tools::preview_redaction as preview;
use serde_json::Value;

pub(crate) fn redact_payload(text: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<Value>(text) else {
        return preview::text(text);
    };
    let original = value.to_string();
    redact_headers(&mut value);
    preview::json(&mut value);
    let redacted = value.to_string();
    // A JSON object can contain duplicate keys. If parsing dropped one,
    // keeping the raw text would keep its uninspected value.
    if original == redacted && string_delimiters(&original) == string_delimiters(text) {
        text.to_owned()
    } else {
        redacted
    }
}

fn string_delimiters(text: &str) -> usize {
    let mut escaped = false;
    text.bytes()
        .filter(|byte| {
            let delimiter = !escaped && *byte == b'"';
            escaped = !escaped && *byte == b'\\';
            delimiter
        })
        .count()
}

fn redact_headers(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            for (key, value) in fields {
                if credential_field(key) {
                    *value = Value::String("[redacted]".to_owned());
                } else {
                    redact_headers(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_headers),
        _ => {}
    }
}

fn credential_field(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
    ) || key.ends_with("signature")
        || key.ends_with("credential")
        || preview::credential_name(&key)
}

pub(super) fn uri(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return preview::text(raw);
    };
    let has_userinfo = !url.username().is_empty() || url.password().is_some();
    if has_userinfo {
        let _ = url.set_password(None);
        let _ = url.set_username("");
    }
    let pairs: Vec<_> = url.query_pairs().into_owned().collect();
    let mut changed = false;
    let pairs: Vec<_> = pairs
        .into_iter()
        .map(|(key, value)| {
            let shown = if credential_field(&key)
                || matches!(key.to_ascii_lowercase().as_str(), "key" | "sig")
            {
                "[redacted]".to_owned()
            } else {
                preview::text(&value)
            };
            let shown_key = preview::text(&key);
            changed |= shown != value || shown_key != key;
            (shown_key, shown)
        })
        .collect();
    if changed {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    preview::text(if has_userinfo || changed {
        url.as_str()
    } else {
        raw
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_safe_payload_is_preserved_but_hidden_duplicate_and_uri_credentials_are_not() {
        let safe = r#" { "query": "some text" } "#;
        assert_eq!(redact_payload(safe), safe);
        assert_eq!(
            redact_payload(r#"{"value":{"authorization":"synthetic"},"value":2}"#),
            r#"{"value":2}"#,
        );
        assert_eq!(
            uri("https://user:synthetic@example.test/media?%74oken=synthetic&id=1"),
            "https://example.test/media?token=[redacted]&id=1",
        );
    }
}
