//! Offline-qualified Codex subscription usage projection.

use plakous_model::{
    MAX_SUBSCRIPTION_USAGE_METERS, SubscriptionUsageMeter, SubscriptionUsagePercent,
    SubscriptionUsagePeriod, SubscriptionUsageSnapshot, SubscriptionUsageValueError,
    SubscriptionUsageWindow, Timestamp,
};
use serde::Deserialize;

/// Exact read-only path in the pinned official Codex client.
#[cfg(feature = "native-auth")]
pub(crate) const CODEX_SUBSCRIPTION_USAGE_PATH: &str = "/backend-api/wham/usage";
/// Maximum body accepted by the usage control-plane decoder.
pub(crate) const MAX_CODEX_SUBSCRIPTION_USAGE_BODY_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    plan_type: Option<String>,
    /// Account e-mail, when the wire carries one. The pinned 0.160.1 reference
    /// client (source `rust-v0.160.0`) does not document this field, so it is
    /// optional and never inferred.
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    rate_limit: Option<RateLimit>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<AdditionalRateLimit>>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimit {
    metered_feature: String,
    limit_name: String,
    #[serde(default)]
    rate_limit: Option<RateLimit>,
}

#[derive(Debug, Deserialize)]
struct RateLimit {
    #[serde(default)]
    primary_window: Option<Window>,
    #[serde(default)]
    secondary_window: Option<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    used_percent: f64,
    limit_window_seconds: u64,
    reset_at: i64,
}

/// Decodes only the non-financial allowance projection plus the plan label from
/// a bounded official Codex usage response. `plan_type` is retained verbatim so
/// the product can name the plan it observed; `credits` and every other
/// financial field stay unread. Unknown top-level fields and enums are ignored;
/// malformed projected fields fail the entire observation closed.
pub fn decode_codex_subscription_usage(
    body: &[u8],
    observed_at: Timestamp,
) -> Result<SubscriptionUsageSnapshot, SubscriptionUsageValueError> {
    if body.is_empty() || body.len() > MAX_CODEX_SUBSCRIPTION_USAGE_BODY_BYTES {
        return Err(SubscriptionUsageValueError::Collection);
    }
    let payload: UsagePayload =
        serde_json::from_slice(body).map_err(|_| SubscriptionUsageValueError::Collection)?;
    let plan = payload
        .plan_type
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let account = payload
        .email
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let additional = payload.additional_rate_limits.unwrap_or_default();
    if additional.len().saturating_add(1) > MAX_SUBSCRIPTION_USAGE_METERS {
        return Err(SubscriptionUsageValueError::Collection);
    }
    let mut meters = Vec::with_capacity(additional.len().saturating_add(1));
    meters.push(meter("codex".to_owned(), None, payload.rate_limit)?);
    for value in additional {
        meters.push(meter(
            value.metered_feature,
            Some(value.limit_name),
            value.rate_limit,
        )?);
    }
    SubscriptionUsageSnapshot::new(observed_at, plan, meters)?.with_account(account)
}

fn meter(
    id: String,
    name: Option<String>,
    limit: Option<RateLimit>,
) -> Result<SubscriptionUsageMeter, SubscriptionUsageValueError> {
    let mut windows = Vec::with_capacity(2);
    if let Some(limit) = limit {
        if let Some(window) = limit.primary_window {
            windows.push(project_window("primary", window)?);
        }
        if let Some(window) = limit.secondary_window {
            windows.push(project_window("secondary", window)?);
        }
    }
    SubscriptionUsageMeter::new(id, name, windows)
}

fn project_window(
    name: &str,
    window: Window,
) -> Result<SubscriptionUsageWindow, SubscriptionUsageValueError> {
    let reset = u64::try_from(window.reset_at)
        .ok()
        .and_then(|seconds| Timestamp::new(seconds, 0).ok())
        .ok_or(SubscriptionUsageValueError::Window)?;
    SubscriptionUsageWindow::new(
        Some(name.to_owned()),
        SubscriptionUsagePercent::new(window.used_percent)?,
        Some(SubscriptionUsagePeriod::Rolling {
            duration_seconds: window.limit_window_seconds,
        }),
        Some(reset),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_sparse_dynamic_and_additional_windows() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{
          "plan_type":"unknown-future-plan",
          "email":" operator@example.test ",
          "rate_limit":{"primary_window":{"used_percent":42.5,"limit_window_seconds":18000,"reset_at":1800000000}},
          "additional_rate_limits":[{"metered_feature":"other","limit_name":"Other meter","rate_limit":{"secondary_window":{"used_percent":7,"limit_window_seconds":2700,"reset_at":1800001000}}}],
          "credits":{"balance":"sensitive-and-ignored"}
        }"#;
        let snapshot = decode_codex_subscription_usage(body, Timestamp::new(10, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        assert!(
            (snapshot.meters()[0].windows()[0].used_percent().get() - 42.5).abs() < f64::EPSILON
        );
        assert_eq!(snapshot.meters()[1].id(), "other");
        assert_eq!(snapshot.meters()[1].windows()[0].name(), Some("secondary"));
        assert_eq!(snapshot.plan(), Some("unknown-future-plan"));
        assert_eq!(snapshot.account(), Some("operator@example.test"));
        Ok(())
    }

    /// The pinned source does not document an `email` field: a wire without one
    /// yields no account label rather than an invented or credential-derived one.
    #[test]
    fn an_absent_email_yields_no_account_label() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"plan_type":"pro","rate_limit":{"primary_window":{"used_percent":0.5,"limit_window_seconds":604800,"reset_at":1800000000}}}"#;
        let snapshot = decode_codex_subscription_usage(body, Timestamp::new(10, 0)?)?;
        assert_eq!(snapshot.plan(), Some("pro"));
        assert_eq!(snapshot.account(), None);
        Ok(())
    }

    /// Shaped like an account with an additional `GPT-5.3-Codex-Spark` limit:
    /// the feature keeps its wire `limit_name` and both windows with their own
    /// durations, in wire order, so the product can label them by duration.
    #[test]
    fn keeps_additional_feature_name_and_window_durations() -> Result<(), Box<dyn std::error::Error>>
    {
        let body = br#"{
          "plan_type":"pro",
          "rate_limit":{"primary_window":{"used_percent":0.5,"limit_window_seconds":604800,"reset_at":1788392640}},
          "additional_rate_limits":[{"metered_feature":"codex_spark","limit_name":"GPT-5.3-Codex-Spark","rate_limit":{
            "primary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_at":1788391380},
            "secondary_window":{"used_percent":0,"limit_window_seconds":18000,"reset_at":1788110940}}}]
        }"#;
        let snapshot = decode_codex_subscription_usage(body, Timestamp::new(1_788_100_000, 0)?)?;
        assert_eq!(snapshot.meters().len(), 2);
        let spark = &snapshot.meters()[1];
        assert_eq!(spark.id(), "codex_spark");
        assert_eq!(spark.name(), Some("GPT-5.3-Codex-Spark"));
        let durations: Vec<_> = spark
            .windows()
            .iter()
            .map(|window| match window.period() {
                Some(SubscriptionUsagePeriod::Rolling { duration_seconds }) => *duration_seconds,
                _ => 0,
            })
            .collect();
        assert_eq!(durations, vec![604_800, 18_000]);
        assert!((spark.windows()[0].used_percent().get() - 100.0).abs() < f64::EPSILON);
        assert!(spark.windows()[1].used_percent().get().abs() < f64::EPSILON);
        Ok(())
    }

    #[test]
    fn rejects_invalid_percentage_timestamp_and_oversize() -> Result<(), Box<dyn std::error::Error>>
    {
        let observed = Timestamp::new(10, 0)?;
        for body in [
            br#"{"rate_limit":{"primary_window":{"used_percent":101,"limit_window_seconds":1,"reset_at":1}}}"#.as_slice(),
            br#"{"rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":1,"reset_at":-1}}}"#.as_slice(),
            br#"{"rate_limit":{"primary_window":{"used_percent":1,"limit_window_seconds":0,"reset_at":1}}}"#.as_slice(),
        ] {
            assert!(decode_codex_subscription_usage(body, observed).is_err());
        }
        assert!(
            decode_codex_subscription_usage(
                &vec![b' '; MAX_CODEX_SUBSCRIPTION_USAGE_BODY_BYTES + 1],
                observed
            )
            .is_err()
        );
        Ok(())
    }
}
