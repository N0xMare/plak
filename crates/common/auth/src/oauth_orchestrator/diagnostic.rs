//! Bounded OAuth failure metadata; raw response bodies never leave this module.

use serde::Deserialize;

#[derive(Deserialize)]
struct OAuthError {
    error: Option<String>,
    error_description: Option<String>,
}

pub(super) fn decode(body: &[u8]) -> (Option<String>, Option<String>) {
    if body.len() > 64 * 1024 {
        return (None, None);
    }
    let Ok(error) = serde_json::from_slice::<OAuthError>(body) else {
        return (None, None);
    };
    let code = error.error.as_deref().filter(|code| {
        !code.is_empty()
            && code.len() <= 32
            && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    });
    let description = error.error_description.as_deref().map(sanitize_description);
    (code.map(str::to_owned), description)
}

fn sanitize_description(value: &str) -> String {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    let mut output = String::new();
    for word in clean.split_whitespace() {
        let token =
            word.trim_matches(['.', ',', ';', '!', '?', '(', ')', '[', ']', '"', '\'', '`']);
        let lower = token.to_ascii_lowercase();
        // Token-looking values, assignment/JSON fragments and identifiers are
        // not diagnostic prose. Filter before truncation, never keep a prefix.
        let suspicious = token.len() > 32
            || token.chars().any(|c| c.is_ascii_digit())
            || token.contains(['=', ':', '/', '\\', '@', '_', '.', '{', '}', '"'])
            || lower.starts_with("sk-")
            || lower.contains("bearer");
        let word = if suspicious { "[redacted]" } else { word };
        if !output.is_empty() {
            output.push(' ');
        }
        for character in word.chars() {
            if output.len() + character.len_utf8() > 112 {
                return output;
            }
            output.push(character);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::decode;

    #[test]
    fn oauth_error_retains_prose_not_tokens_or_body() {
        let body = br#"{"error":"invalid_grant","error_description":"Refresh rejected sk-secret-example eyJhbGciOiJIUzI1NiJ9.payload.signature refresh_token=secret accessToken:secret abc123secret", "access_token":"not-a-diagnostic", "refresh_token":"also-not-a-diagnostic"}"#;
        let (code, description) = decode(body);
        assert_eq!(code.as_deref(), Some("invalid_grant"));
        let description = description.unwrap_or_default();
        assert!(description.starts_with("Refresh rejected"));
        assert!(!description.contains("secret"));
        assert!(!description.contains("diagnostic"));
        assert!(!description.contains("eyJ"));
    }

    #[test]
    fn oauth_error_strips_controls_and_bounds_fields() {
        let body = serde_json::json!({
            "error": "invalid_grant",
            "error_description": format!("rejected\n\u{1b} {}", "word ".repeat(200)),
        });
        let (_, description) = decode(body.to_string().as_bytes());
        let description = description.unwrap_or_default();
        assert!(description.len() <= 112);
        assert!(!description.chars().any(char::is_control));
        assert_eq!(decode(br#"{"error":"sk-secret"}"#).0, None);
    }
}
