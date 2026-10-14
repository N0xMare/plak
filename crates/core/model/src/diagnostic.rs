//! Bounded, credential-safe public diagnostics.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Maximum bytes retained in a public diagnostic.
pub const MAX_SAFE_DIAGNOSTIC_BYTES: usize = 2_048;

/// A bounded public diagnostic with common credential forms redacted.
#[derive(Clone, Eq, PartialEq)]
pub struct SafeDiagnostic(String);

impl SafeDiagnostic {
    /// Sanitizes an untrusted diagnostic for public display and retention
    /// with the shared display redaction rules, then bounds it.
    #[must_use]
    pub fn new(message: impl AsRef<str>) -> Self {
        let redacted = crate::redaction::text(message.as_ref());
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
            "task-17 failed: disk-full on a risk-based retry",
            "max_tokens exceeded for input_tokens 4096",
        ] {
            assert_eq!(SafeDiagnostic::new(input).as_str(), input);
        }
        for input in [
            "authorization: Bearer abcdefghijklmnop1234",
            concat!("token=", "abcdefghijklmnop1234"),
            concat!("{\"id_token\":\"", "abcdefghijklmnop1234", "\"}"),
        ] {
            let once = SafeDiagnostic::new(input);
            assert!(!once.as_str().contains("abcdefghijklmnop1234"), "{once}");
            assert_eq!(SafeDiagnostic::new(once.as_str()), once);
            let encoded = serde_json::to_string(&once).unwrap();
            let decoded: SafeDiagnostic = serde_json::from_str(&encoded).unwrap();
            assert_eq!(serde_json::to_string(&decoded).unwrap(), encoded);
        }
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
            format!("oauth failed: eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.{SECRET_SENTINEL}"),
            format!("connect https://user:{SECRET_SENTINEL}@example.invalid/v1"),
            format!("-----BEGIN PRIVATE KEY-----\n{SECRET_SENTINEL}\n-----END PRIVATE KEY-----"),
            format!("AKIA{}", "ABCDEFGHIJKLMNOP"),
            format!("{{\"id_token\":\"{SECRET_SENTINEL}\"}}"),
        ] {
            let diagnostic = SafeDiagnostic::new(&input);
            assert!(
                !diagnostic.as_str().contains(SECRET_SENTINEL),
                "{diagnostic}"
            );
            assert!(
                !diagnostic.as_str().contains("ABCDEFGHIJKLMNOP"),
                "{diagnostic}"
            );
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
