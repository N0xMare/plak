//! Pure urlencoded form builders, bounded JSON decode, and JWT timing hints.

use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use zeroize::Zeroizing;

use super::OAuthWorkflowError;

/// Encodes `fields` as `application/x-www-form-urlencoded` into a zeroizing string.
///
/// Field order is preserved. Values may contain secrets; the returned buffer is
/// zeroized on drop.
#[must_use]
pub fn urlencoded_form(fields: &[(&str, &str)]) -> Zeroizing<String> {
    Zeroizing::new(
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields.iter().copied())
            .finish(),
    )
}

/// Encodes `fields` as `application/x-www-form-urlencoded` into zeroizing bytes.
///
/// Equivalent to [`urlencoded_form`] with a UTF-8 byte view of the same payload.
#[must_use]
pub fn urlencoded_form_bytes(fields: &[(&str, &str)]) -> Zeroizing<Vec<u8>> {
    let mut form = urlencoded_form(fields);
    let bytes = std::mem::take(&mut *form).into_bytes();
    Zeroizing::new(bytes)
}

/// Returns the standard authorization-code token endpoint field pairs.
///
/// Callers pass the result to [`urlencoded_form`] or [`urlencoded_form_bytes`].
/// The verifier and code are not copied; they remain borrowed.
#[must_use]
pub fn authorization_code_token_form<'a>(
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
) -> [(&'static str, &'a str); 5] {
    [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", code_verifier),
    ]
}

/// Returns the standard refresh-token grant field pairs.
///
/// Callers pass the result to [`urlencoded_form`] or [`urlencoded_form_bytes`].
/// The refresh token is not copied; it remains borrowed.
#[must_use]
pub fn refresh_token_form<'a>(
    refresh_token: &'a str,
    client_id: &'a str,
) -> [(&'static str, &'a str); 3] {
    [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ]
}

/// Deserializes a full JSON value from a bounded byte slice.
///
/// Rejects empty bodies, bodies larger than `maximum`, and trailing tokens
/// after a successful deserialize. Does not log or echo the body.
///
/// # Errors
///
/// Returns [`OAuthWorkflowError::ResponseTooLarge`] when empty or over size,
/// otherwise [`OAuthWorkflowError::InvalidJson`].
pub fn decode_json_bounded<T>(bytes: &[u8], maximum: usize) -> Result<T, OAuthWorkflowError>
where
    T: DeserializeOwned,
{
    if bytes.is_empty() || bytes.len() > maximum {
        return Err(OAuthWorkflowError::ResponseTooLarge);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = T::deserialize(&mut deserializer).map_err(|_| OAuthWorkflowError::InvalidJson)?;
    deserializer
        .end()
        .map_err(|_| OAuthWorkflowError::InvalidJson)?;
    Ok(value)
}

#[derive(Deserialize)]
struct AccessTokenTiming {
    exp: u64,
}

/// Reads `exp` from an **unsigned** JWT access token as a scheduling hint only.
///
/// This is not signature validation. It rejects empty tokens, tokens larger
/// than `max_bytes`, malformed three-segment JWT shapes, empty payloads, and
/// payloads that fail bounded JSON decode. Decoded payload size is also
/// bounded by `max_bytes`.
#[must_use]
pub fn unsigned_access_token_exp(token: &str, max_bytes: usize) -> Option<u64> {
    if token.is_empty() || token.len() > max_bytes {
        return None;
    }
    let mut segments = token.split('.');
    let _header = segments.next()?;
    let payload = segments.next()?;
    let _signature = segments.next()?;
    if segments.next().is_some() || payload.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    if decoded.is_empty() || decoded.len() > max_bytes {
        return None;
    }
    decode_json_bounded::<AccessTokenTiming>(&decoded, max_bytes)
        .ok()
        .map(|claims| claims.exp)
}

/// Returns the positive duration remaining until `deadline` at `now`.
///
/// Returns [`None`] when `now >= deadline` (expired or exact).
#[must_use]
pub fn remaining_until(deadline: Instant, now: Instant) -> Option<Duration> {
    if now >= deadline {
        None
    } else {
        Some(deadline.duration_since(now))
    }
}

/// `true` when access expiry is at or inside the refresh skew window.
#[must_use]
pub fn access_needs_refresh(expires_at: u64, now: u64, skew_seconds: u64) -> bool {
    expires_at <= now.saturating_add(skew_seconds)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::{
        access_needs_refresh, authorization_code_token_form, decode_json_bounded,
        refresh_token_form, remaining_until, unsigned_access_token_exp, urlencoded_form,
        urlencoded_form_bytes,
    };
    use crate::oauth_workflow::OAuthWorkflowError;
    use base64::Engine as _;
    use serde::Deserialize;
    use std::time::{Duration, Instant};

    #[derive(Debug, Deserialize, PartialEq)]
    struct Sample {
        value: u32,
    }

    #[test]
    fn form_encodes_pairs_in_order_and_escapes() {
        let form = urlencoded_form(&[
            ("grant_type", "authorization_code"),
            ("redirect_uri", "http://127.0.0.1:56121/callback"),
            ("scope", "openid profile"),
        ]);
        assert_eq!(
            form.as_str(),
            "grant_type=authorization_code&redirect_uri=http%3A%2F%2F127.0.0.1%3A56121%2Fcallback&scope=openid+profile"
        );
        let bytes = urlencoded_form_bytes(&[("a", "1"), ("b", "2")]);
        assert_eq!(bytes.as_slice(), b"a=1&b=2");
    }

    #[test]
    fn authorization_code_token_form_is_stable() {
        let fields = authorization_code_token_form(
            "code-value",
            "http://localhost/cb",
            "client",
            "verifier",
        );
        assert_eq!(
            fields,
            [
                ("grant_type", "authorization_code"),
                ("code", "code-value"),
                ("redirect_uri", "http://localhost/cb"),
                ("client_id", "client"),
                ("code_verifier", "verifier"),
            ]
        );
        let encoded = urlencoded_form(&fields);
        assert!(encoded.contains("grant_type=authorization_code"));
        assert!(encoded.contains("code=code-value"));
        assert!(encoded.contains("code_verifier=verifier"));
    }

    #[test]
    fn refresh_token_form_is_stable() {
        let fields = refresh_token_form("refresh-secret", "public-client");
        assert_eq!(
            fields,
            [
                ("grant_type", "refresh_token"),
                ("refresh_token", "refresh-secret"),
                ("client_id", "public-client"),
            ]
        );
        let encoded = urlencoded_form(&fields);
        assert!(encoded.contains("grant_type=refresh_token"));
        assert!(encoded.contains("refresh_token=refresh-secret"));
        assert!(encoded.contains("client_id=public-client"));
    }

    #[test]
    fn decode_json_bounded_enforces_size_and_trailing() {
        let ok = decode_json_bounded::<Sample>(br#"{"value":7}"#, 64).expect("json");
        assert_eq!(ok, Sample { value: 7 });
        assert_eq!(
            decode_json_bounded::<Sample>(br#"{"value":7}"#, 5),
            Err(OAuthWorkflowError::ResponseTooLarge)
        );
        assert_eq!(
            decode_json_bounded::<Sample>(b"", 64),
            Err(OAuthWorkflowError::ResponseTooLarge)
        );
        assert_eq!(
            decode_json_bounded::<Sample>(br#"{"value":7}0"#, 64),
            Err(OAuthWorkflowError::InvalidJson)
        );
        assert_eq!(
            decode_json_bounded::<Sample>(br#"{"value":"x"}"#, 64),
            Err(OAuthWorkflowError::InvalidJson)
        );
    }

    #[test]
    fn unsigned_access_token_exp_reads_exp_and_rejects_malformed() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"exp":1700000000,"sub":"s"}"#);
        let token = format!("e30.{payload}.sig");
        assert_eq!(unsigned_access_token_exp(&token, 4096), Some(1_700_000_000));
        assert_eq!(unsigned_access_token_exp("", 4096), None);
        assert_eq!(unsigned_access_token_exp("not-a-jwt", 4096), None);
        assert_eq!(unsigned_access_token_exp(&token, 8), None);
        let no_exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"s"}"#);
        assert_eq!(
            unsigned_access_token_exp(&format!("e30.{no_exp}.sig"), 4096),
            None
        );
        assert_eq!(
            unsigned_access_token_exp(&format!("e30.{payload}.sig.extra"), 4096),
            None
        );
    }

    #[test]
    fn remaining_until_is_positive_only() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        assert_eq!(remaining_until(deadline, now), Some(Duration::from_secs(5)));
        assert_eq!(remaining_until(now, now), None);
        assert_eq!(remaining_until(now, deadline), None);
    }

    #[test]
    fn access_needs_refresh_respects_skew() {
        assert!(!access_needs_refresh(1_000, 100, 60));
        assert!(access_needs_refresh(150, 100, 60));
        assert!(access_needs_refresh(100, 100, 0));
    }
}
