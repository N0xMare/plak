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

#[derive(Deserialize)]
struct IdTokenTiming {
    iat: Option<u64>,
    exp: Option<u64>,
}

/// Reads `iat` and `exp` from an **unverified** JWT for diagnostics only.
///
/// Never used for admission. Same shape and size bounds as
/// [`unsigned_access_token_exp`]; returns `(iat, exp)`.
#[must_use]
pub fn unverified_jwt_timing(token: &str, max_bytes: usize) -> Option<(Option<u64>, Option<u64>)> {
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
    decode_json_bounded::<IdTokenTiming>(&decoded, max_bytes)
        .ok()
        .map(|claims| (claims.iat, claims.exp))
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

/// Deadline of one request inside a flow: `now + cap`, never past `deadline`.
///
/// Returns [`None`] when `now >= deadline`.
#[must_use]
pub fn capped_deadline(deadline: Instant, cap: Duration, now: Instant) -> Option<Instant> {
    remaining_until(deadline, now)?;
    Some(deadline.min(now.checked_add(cap).unwrap_or(deadline)))
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
mod tests;
