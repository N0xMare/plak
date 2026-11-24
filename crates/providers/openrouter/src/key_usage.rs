//! Bounded authenticated OpenRouter inference-key allowance read.
//!
//! Exactly one endpoint is a source: `GET https://openrouter.ai/api/v1/key`,
//! which an ordinary inference key may call. The account Credits endpoint is
//! **not** a source and is never requested: it requires a management key and
//! answers an inference key with `403 Only management keys can perform this
//! operation`. The deprecated `rate_limit` object in the `/api/v1/key` response
//! is ignored, and `X-RateLimit-*` headers (429-only) are not a meter source.

use std::time::{Duration, Instant};

use plakous_http::{
    Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Method, Origin, Request,
    SensitiveBytes, StatusCode, sensitive_header_value,
};
use plakous_model::{Clock as _, Digest, DigestDomain, SystemClock, Timestamp};
use serde::Deserialize;
use thiserror::Error;

use crate::{OpenRouterAuthError, OpenRouterChatRoute};

/// The one authenticated allowance path an inference key may call.
pub(crate) const OPENROUTER_KEY_USAGE_PATH: &str = "/api/v1/key";
/// Maximum body accepted by the allowance decoder.
pub(crate) const MAX_OPENROUTER_KEY_USAGE_BODY_BYTES: usize = 64 * 1024;
/// Complete operation bound for the single allowance GET.
pub(crate) const OPENROUTER_KEY_USAGE_TIMEOUT: Duration = Duration::from_secs(10);

/// One decoded observation of the inference key's own allowance.
///
/// Every currency amount is the provider's own number. `limit` of `None` means
/// the key carries no cap; it is never rendered as an unbounded bar and never
/// inferred from `usage`.
#[derive(Clone, Debug, PartialEq)]
pub struct OpenRouterKeyUsage {
    observed_at: Timestamp,
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    limit_reset: Option<String>,
    usage: Option<f64>,
    usage_daily: Option<f64>,
    usage_weekly: Option<f64>,
    usage_monthly: Option<f64>,
    credit_decimals: [Option<String>; 6],
    is_free_tier: Option<bool>,
    include_byok_in_limit: Option<bool>,
}

impl OpenRouterKeyUsage {
    /// Returns when the bounded response was fully observed.
    #[must_use]
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }
    /// Returns the credit cap. `None` is "no cap", not zero and not unbounded.
    #[must_use]
    pub const fn limit(&self) -> Option<f64> {
        self.limit
    }
    /// Returns credits remaining against the cap, when the provider reports one.
    #[must_use]
    pub const fn limit_remaining(&self) -> Option<f64> {
        self.limit_remaining
    }
    /// Returns the provider-declared reset cadence, when supplied.
    #[must_use]
    pub fn limit_reset(&self) -> Option<&str> {
        self.limit_reset.as_deref()
    }
    /// Returns all-time credits spent by this key, when supplied.
    #[must_use]
    pub const fn usage(&self) -> Option<f64> {
        self.usage
    }
    /// Returns credits spent today, when supplied.
    #[must_use]
    pub const fn usage_daily(&self) -> Option<f64> {
        self.usage_daily
    }
    /// Returns credits spent this week, when supplied.
    #[must_use]
    pub const fn usage_weekly(&self) -> Option<f64> {
        self.usage_weekly
    }
    /// Returns credits spent this month, when supplied.
    #[must_use]
    pub const fn usage_monthly(&self) -> Option<f64> {
        self.usage_monthly
    }
    /// Exact provider numbers for the cap and remaining credits, respectively.
    #[must_use]
    pub fn limit_decimals(&self) -> [Option<&str>; 2] {
        let [cap, remaining, ..] = &self.credit_decimals;
        [cap.as_deref(), remaining.as_deref()]
    }
    /// Exact provider spend numbers, in all-time, daily, weekly and monthly order.
    #[must_use]
    pub fn usage_decimals(&self) -> [Option<&str>; 4] {
        let [_, _, all, daily, weekly, monthly] = &self.credit_decimals;
        [
            all.as_deref(),
            daily.as_deref(),
            weekly.as_deref(),
            monthly.as_deref(),
        ]
    }
    /// Returns whether the provider marked this key free tier, when supplied.
    #[must_use]
    pub const fn is_free_tier(&self) -> Option<bool> {
        self.is_free_tier
    }
    /// Returns whether BYOK spend counts against the cap, when supplied.
    #[must_use]
    pub const fn include_byok_in_limit(&self) -> Option<bool> {
        self.include_byok_in_limit
    }
}

/// Typed failure for the single bounded allowance read.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OpenRouterKeyUsageError {
    /// The bounded read policy was invalid.
    #[error("OpenRouter key usage policy is invalid")]
    InvalidPolicy,
    /// The provider rejected an invalid or revoked inference key (HTTP 401).
    #[error("OpenRouter rejected the API key (HTTP 401: invalid or revoked)")]
    Unauthorized,
    /// The provider denied access for this inference key (HTTP 403).
    #[error("OpenRouter denied API key access (HTTP 403: forbidden)")]
    Forbidden,
    /// The provider rate-limited this read; it was not retried.
    #[error("OpenRouter key usage was rate-limited; it was not retried")]
    RateLimited,
    /// The provider answered with another non-success status.
    #[error("OpenRouter key usage returned an unexpected HTTP status")]
    UnexpectedStatus,
    /// The provider answered with a non-JSON media type.
    #[error("OpenRouter key usage returned an unexpected media type")]
    UnexpectedMediaType,
    /// The body was empty, oversized, or not decodable at the documented shape.
    #[error("OpenRouter key usage returned an invalid bounded response")]
    InvalidResponse,
    /// Authentication failed without exposing credential material.
    #[error(transparent)]
    Authentication(#[from] OpenRouterAuthError),
    /// HTTP request construction failed.
    #[error(transparent)]
    Request(#[from] plakous_http::RequestError),
    /// The bounded transport failed.
    #[error(transparent)]
    Transport(#[from] plakous_http::Failure),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] plakous_model::DigestError),
}

#[derive(Deserialize)]
struct KeyEnvelope {
    data: KeyData,
}

/// Only the documented allowance fields are read. `rate_limit` is deprecated
/// and `label` (which can carry a masked key fragment) is never decoded;
/// unknown fields are ignored, not invented.
#[derive(Deserialize)]
struct KeyData {
    #[serde(default)]
    limit: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    limit_remaining: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    limit_reset: Option<String>,
    #[serde(default)]
    usage: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    usage_daily: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    usage_weekly: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    usage_monthly: Option<Box<serde_json::value::RawValue>>,
    #[serde(default)]
    is_free_tier: Option<bool>,
    #[serde(default)]
    include_byok_in_limit: Option<bool>,
}

/// Decodes one bounded `/api/v1/key` body. Absent fields stay absent. Remaining
/// credits can be negative; caps and spend cannot, and every number must be finite.
///
/// # Errors
///
/// Returns [`OpenRouterKeyUsageError::InvalidResponse`] for an empty, oversized,
/// malformed, or out-of-range body.
pub fn decode_openrouter_key_usage(
    body: &[u8],
    observed_at: Timestamp,
) -> Result<OpenRouterKeyUsage, OpenRouterKeyUsageError> {
    if body.is_empty() || body.len() > MAX_OPENROUTER_KEY_USAGE_BODY_BYTES {
        return Err(OpenRouterKeyUsageError::InvalidResponse);
    }
    let envelope: KeyEnvelope =
        serde_json::from_slice(body).map_err(|_| OpenRouterKeyUsageError::InvalidResponse)?;
    let data = envelope.data;
    let credit_decimals = [
        data.limit,
        data.limit_remaining,
        data.usage,
        data.usage_daily,
        data.usage_weekly,
        data.usage_monthly,
    ]
    .map(|raw| raw.map(|raw| raw.get().to_owned()));
    let mut amounts = [None; 6];
    for (index, (amount, decimal)) in amounts.iter_mut().zip(&credit_decimals).enumerate() {
        if let Some(decimal) = decimal {
            let signed_remaining = index == 1;
            let value = serde_json::from_str::<f64>(decimal)
                .ok()
                .filter(|value| value.is_finite() && (signed_remaining || *value >= 0.0))
                .ok_or(OpenRouterKeyUsageError::InvalidResponse)?;
            *amount = Some(value);
        }
    }
    let [
        limit,
        limit_remaining,
        usage,
        usage_daily,
        usage_weekly,
        usage_monthly,
    ] = amounts;
    Ok(OpenRouterKeyUsage {
        observed_at,
        limit,
        limit_remaining,
        limit_reset: bounded_text(data.limit_reset),
        usage,
        usage_daily,
        usage_weekly,
        usage_monthly,
        credit_decimals,
        is_free_tier: data.is_free_tier,
        include_byok_in_limit: data.include_byok_in_limit,
    })
}

fn bounded_text(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .filter(|value| !value.chars().any(char::is_control))
}

impl OpenRouterChatRoute {
    /// Performs exactly one authenticated `GET /api/v1/key` allowance read.
    ///
    /// This is an on-demand control-plane read. It never dispatches inference,
    /// never requests the management-key Credits endpoint, and never retries.
    ///
    /// # Errors
    ///
    /// Returns a typed failure for auth, transport, status, or body problems.
    pub async fn key_usage(
        &self,
        client: &Client,
    ) -> Result<OpenRouterKeyUsage, OpenRouterKeyUsageError> {
        let operation = Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"plakous-openrouter/key-usage-v1",
                self.identity().fingerprint().as_bytes(),
                OPENROUTER_KEY_USAGE_PATH.as_bytes(),
            ],
        )?;
        self.prepare_credential().await?;
        let generation = self.current_credential_generation_grant()?;
        let key = self.catalog_credential(&operation, &generation)?;
        let deadline = Instant::now()
            .checked_add(OPENROUTER_KEY_USAGE_TIMEOUT)
            .ok_or(OpenRouterKeyUsageError::InvalidPolicy)?;
        let origin = Origin::parse(self.identity().origin().as_str())
            .map_err(|_| OpenRouterKeyUsageError::InvalidPolicy)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("accept"),
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("authorization"),
            sensitive_header_value(key.into_header_bytes())
                .map_err(|_| OpenRouterAuthError::CredentialSource)?,
        );
        let request = Request::new(
            origin,
            Method::GET,
            OPENROUTER_KEY_USAGE_PATH,
            headers,
            SensitiveBytes::new(Vec::new()).into_bytes(),
            client.limits(),
        )?;
        let response = client
            .execute(request, ExchangeControl::new(deadline))
            .await?;
        let status = response.status();
        if status != StatusCode::OK {
            let _ = response.into_body().close().await;
            return Err(match status {
                StatusCode::UNAUTHORIZED => OpenRouterKeyUsageError::Unauthorized,
                StatusCode::FORBIDDEN => OpenRouterKeyUsageError::Forbidden,
                StatusCode::TOO_MANY_REQUESTS => OpenRouterKeyUsageError::RateLimited,
                _ => OpenRouterKeyUsageError::UnexpectedStatus,
            });
        }
        if !response
            .headers()
            .get(&HeaderName::from_static("content-type"))
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
            })
        {
            let _ = response.into_body().close().await;
            return Err(OpenRouterKeyUsageError::UnexpectedMediaType);
        }
        let mut body = response.into_body();
        let mut retained = Vec::new();
        while let Some(chunk) = body.next_chunk().await {
            let chunk = chunk?;
            if retained
                .len()
                .checked_add(chunk.len())
                .is_none_or(|size| size > MAX_OPENROUTER_KEY_USAGE_BODY_BYTES)
            {
                let _ = body.close().await;
                return Err(OpenRouterKeyUsageError::InvalidResponse);
            }
            retained.extend_from_slice(&chunk);
        }
        decode_openrouter_key_usage(&retained, SystemClock.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented `/api/v1/key` response shape; no field is invented here.
    const OPENROUTER_KEY_USAGE_FIXTURE: &[u8] = br#"{"data":{
      "label":"plak inference key",
      "limit":10.0,
      "limit_remaining":6.42,
      "limit_reset":null,
      "usage":3.58,
      "usage_daily":0.4,
      "usage_weekly":1.1,
      "usage_monthly":2.9,
      "is_free_tier":true,
      "include_byok_in_limit":false,
      "rate_limit":{"requests":10,"interval":"10s"}
    }}"#;

    #[test]
    fn parses_the_documented_fields_and_ignores_the_deprecated_rate_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let usage = decode_openrouter_key_usage(
            OPENROUTER_KEY_USAGE_FIXTURE,
            Timestamp::new(1_788_000_000, 0)?,
        )?;
        // The provider's key label can carry a masked key fragment; it is not
        // decoded, so no accessor or Debug output can reach it.
        assert!(!format!("{usage:?}").contains("plak inference key"));
        assert_eq!(usage.limit(), Some(10.0));
        assert_eq!(usage.limit_remaining(), Some(6.42));
        assert_eq!(usage.limit_reset(), None);
        assert_eq!(usage.usage(), Some(3.58));
        assert_eq!(usage.usage_daily(), Some(0.4));
        assert_eq!(usage.usage_weekly(), Some(1.1));
        assert_eq!(usage.usage_monthly(), Some(2.9));
        assert_eq!(usage.is_free_tier(), Some(true));
        assert_eq!(usage.include_byok_in_limit(), Some(false));
        Ok(())
    }

    #[test]
    fn a_null_limit_stays_absent_and_is_never_inferred() -> Result<(), Box<dyn std::error::Error>> {
        let usage = decode_openrouter_key_usage(
            br#"{"data":{"limit":null,"limit_remaining":null,"usage":12.5}}"#,
            Timestamp::new(1, 0)?,
        )?;
        assert_eq!(usage.limit(), None);
        assert_eq!(usage.limit_remaining(), None);
        assert_eq!(usage.usage(), Some(12.5));
        Ok(())
    }

    #[test]
    fn a_reported_negative_remaining_balance_is_not_an_invalid_read()
    -> Result<(), Box<dyn std::error::Error>> {
        let usage = decode_openrouter_key_usage(
            br#"{"data":{"limit":2,"limit_remaining":-1.25,"usage":3.25}}"#,
            Timestamp::new(1, 0)?,
        )?;
        assert_eq!(usage.limit_remaining(), Some(-1.25));
        assert_eq!(usage.limit_decimals(), [Some("2"), Some("-1.25")]);
        Ok(())
    }

    #[test]
    fn rejects_empty_oversize_malformed_and_out_of_range_bodies()
    -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1, 0)?;
        for body in [
            b"".as_slice(),
            b"{}".as_slice(),
            br#"{"data":{"limit":-1}}"#.as_slice(),
            br#"{"data":{"usage":-1}}"#.as_slice(),
            br#"{"data":{"limit":"ten"}}"#.as_slice(),
        ] {
            assert!(decode_openrouter_key_usage(body, observed).is_err());
        }
        let oversize = vec![b'x'; MAX_OPENROUTER_KEY_USAGE_BODY_BYTES + 1];
        assert!(decode_openrouter_key_usage(&oversize, observed).is_err());
        Ok(())
    }

    /// Negative test: the management-key Credits endpoint is not a source, so
    /// no request path in this crate may name it.
    #[test]
    fn the_management_key_credits_endpoint_is_never_requested() {
        assert_eq!(OPENROUTER_KEY_USAGE_PATH, "/api/v1/key");
        for module in [
            include_str!("key_usage.rs"),
            include_str!("discovery.rs"),
            include_str!("route.rs"),
            include_str!("chat.rs"),
            include_str!("messages.rs"),
            include_str!("execution.rs"),
        ] {
            assert!(
                !module.contains(concat!("/api/v1/", "credits")),
                "an OpenRouter request path names the management-key Credits endpoint"
            );
        }
    }
}
