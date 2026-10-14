//! Bounded, credential-safe public diagnostics.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum bytes retained in a public diagnostic.
pub const MAX_SAFE_DIAGNOSTIC_BYTES: usize = 2_048;

/// A bounded public diagnostic with common credential forms redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct SafeDiagnostic(String);

impl SafeDiagnostic {
    /// Sanitizes an untrusted diagnostic for public display and retention.
    #[must_use]
    pub fn new(message: impl AsRef<str>) -> Self {
        let redacted = redact_secrets(message.as_ref());
        Self(truncate_utf8(&redacted, MAX_SAFE_DIAGNOSTIC_BYTES))
    }

    /// Returns the bounded, sanitized diagnostic.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SafeDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SafeDiagnostic")
            .field("bytes", &self.0.len())
            .field("message", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for SafeDiagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for SafeDiagnostic {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SafeDiagnostic {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::new)
    }
}

fn redact_secrets(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut redact_next = false;
    let mut tokens = input.split_inclusive(char::is_whitespace).peekable();
    while let Some(token) = tokens.next() {
        let trimmed = token.trim_end_matches(char::is_whitespace);
        let suffix = &token[trimmed.len()..];
        if redact_next || looks_sensitive(trimmed) {
            output.push_str("[redacted]");
            redact_next = is_bearer_marker(trimmed);
        } else if let Some(redact_following) = sensitive_assignment(trimmed) {
            output.push_str("[redacted]");
            redact_next = redact_following;
        } else if is_sensitive_key(trimmed) {
            output.push_str(trimmed);
            let key = trimmed.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
            redact_next = !matches!(key.to_ascii_lowercase().as_str(), "token" | "credential")
                || tokens.peek().is_some_and(|next| value_shaped(next.trim()));
        } else {
            output.push_str(trimmed);
        }
        output.push_str(suffix);
    }
    output
}

fn value_shaped(value: &str) -> bool {
    looks_sensitive(value)
        || value.len() >= 16
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._~+/=-".contains(&byte))
            && value.bytes().any(|byte| byte.is_ascii_digit())
}

fn looks_sensitive(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    lowercase.contains("sk-")
        || is_bearer_marker(&lowercase)
        || lowercase.contains("ghp_")
        || lowercase.contains("github_pat_")
}

fn is_bearer_marker(value: &str) -> bool {
    value
        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .eq_ignore_ascii_case("bearer")
}

fn sensitive_assignment(value: &str) -> Option<bool> {
    let lowercase = value.to_ascii_lowercase();
    for key in [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "authorization",
        "credential",
    ] {
        let mut remainder = lowercase.as_str();
        let mut offset = 0;
        while let Some(index) = remainder.find(key) {
            let absolute = offset + index;
            let before_is_boundary = absolute == 0
                || !lowercase.as_bytes()[absolute - 1].is_ascii_alphanumeric()
                    && lowercase.as_bytes()[absolute - 1] != b'_';
            let value_start = absolute.saturating_add(key.len());
            let separator = lowercase.as_bytes().get(value_start).copied();
            if before_is_boundary && matches!(separator, Some(b'=' | b':')) {
                let assigned =
                    value[value_start.saturating_add(1)..].trim_matches(|character: char| {
                        !character.is_ascii_alphanumeric() && character != '_'
                    });
                return Some(assigned.is_empty() || assigned.eq_ignore_ascii_case("bearer"));
            }
            let advance = index.saturating_add(key.len());
            offset = offset.saturating_add(advance);
            remainder = &remainder[advance..];
        }
    }
    None
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key.trim_matches(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .to_ascii_lowercase()
            .as_str(),
        "token" | "secret" | "password" | "api_key" | "apikey" | "authorization" | "credential"
    )
}

fn truncate_utf8(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum.saturating_sub(3);
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut truncated = value[..end].to_owned();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";

    #[test]
    fn redaction_preserves_failure_prose_and_is_idempotent() {
        for input in [
            "Claude OAuth credential refresh failed",
            "token limit exceeded",
            "credential unchanged; retry when online",
        ] {
            assert_eq!(SafeDiagnostic::new(input).as_str(), input);
        }
        for input in [
            "password short",
            "token abcdefghijklmnop1234",
            "credential abcdefghijklmnop1234",
            "authorization Bearer abcdefghijklmnop1234",
            concat!("token=", "abcdefghijklmnop1234"),
        ] {
            let once = SafeDiagnostic::new(input);
            assert!(!once.as_str().contains("abcdefghijklmnop1234"));
            assert_eq!(SafeDiagnostic::new(once.as_str()), once);
            let encoded = serde_json::to_string(&once).unwrap();
            let decoded: SafeDiagnostic = serde_json::from_str(&encoded).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
        }
        assert_eq!(
            SafeDiagnostic::new("password short").as_str(),
            "password [redacted]"
        );
    }

    #[test]
    fn projections_are_bounded_utf8_and_credential_safe() {
        for input in [
            format!("token={SECRET_SENTINEL}"),
            format!("sk-{SECRET_SENTINEL}"),
            format!("Bearer {SECRET_SENTINEL}"),
            format!("authorization: Bearer {SECRET_SENTINEL}"),
            format!("https://example.invalid/?api_key={SECRET_SENTINEL}"),
            format!("ghp_{SECRET_SENTINEL}"),
            format!("github_pat_{SECRET_SENTINEL}"),
        ] {
            let diagnostic = SafeDiagnostic::new(input);
            assert!(!diagnostic.as_str().contains(SECRET_SENTINEL));
            assert!(!diagnostic.to_string().contains(SECRET_SENTINEL));
            assert!(!format!("{diagnostic:?}").contains(SECRET_SENTINEL));
        }

        let oversized = SafeDiagnostic::new("🦀".repeat(MAX_SAFE_DIAGNOSTIC_BYTES));
        assert!(oversized.as_str().len() <= MAX_SAFE_DIAGNOSTIC_BYTES);
        assert!(oversized.as_str().ends_with("..."));
        assert_eq!(oversized.to_string(), oversized.as_str());
    }

    #[test]
    fn serde_round_trips_only_sanitized_content() -> Result<(), serde_json::Error> {
        let raw = format!("token={SECRET_SENTINEL} {}", "界".repeat(2_048));
        let encoded_raw = serde_json::to_string(&raw)?;
        let diagnostic: SafeDiagnostic = serde_json::from_str(&encoded_raw)?;
        assert!(!diagnostic.as_str().contains(SECRET_SENTINEL));
        assert!(diagnostic.as_str().len() <= MAX_SAFE_DIAGNOSTIC_BYTES);

        let encoded_safe = serde_json::to_string(&diagnostic)?;
        assert!(!encoded_safe.contains(SECRET_SENTINEL));
        assert_eq!(
            serde_json::from_str::<SafeDiagnostic>(&encoded_safe)?,
            diagnostic
        );
        Ok(())
    }
}
