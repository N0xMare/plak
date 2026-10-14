//! Offline-qualified Grok included-credit usage projection.

use plakous_model::{
    SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO, SUBSCRIPTION_BALANCE_PREPAID_CREDITS,
    SubscriptionUsageBalance, SubscriptionUsageMeter, SubscriptionUsagePercent,
    SubscriptionUsagePeriod, SubscriptionUsageSnapshot, SubscriptionUsageValueError,
    SubscriptionUsageWindow, Timestamp,
};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Exact read-only billing path in the reviewed official Grok Build source.
pub const GROK_SUBSCRIPTION_USAGE_PATH: &str = "/v1/billing?format=credits";
/// Official public source revision qualifying this decoder.
pub const GROK_SUBSCRIPTION_USAGE_SOURCE_REVISION: &str =
    "77cd7eb675ba911c225c3aaeeece3a20cbccc426";
/// Maximum body accepted by the usage control-plane decoder.
pub const MAX_GROK_SUBSCRIPTION_USAGE_BODY_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingResponse {
    config: Option<BillingConfig>,
    /// Present only when the caller enriches the body from a separate remote
    /// settings read; the reviewed billing route itself does not send it.
    subscription_tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingConfig {
    credit_usage_percent: Option<f64>,
    current_period: Option<UsagePeriod>,
    /// Deprecated included allowance, in USD cents. Read only as the documented
    /// fallback for a response that predates `creditUsagePercent`.
    monthly_limit: Option<Cents>,
    /// Deprecated included spend, in USD cents: the numerator of that fallback.
    used: Option<Cents>,
    /// Deprecated period end, read when no `currentPeriod` is reported.
    billing_period_end: Option<String>,
    on_demand_cap: Option<Cents>,
    on_demand_used: Option<Cents>,
    prepaid_balance: Option<Cents>,
}

/// A USD cent amount. proto3 JSON omits zero-valued scalars, so `$0` arrives as
/// `{}`; defaulting to zero keeps the whole observation parseable.
#[derive(Deserialize)]
struct Cents {
    #[serde(default)]
    val: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsagePeriod {
    #[serde(rename = "type")]
    kind: Option<String>,
    start: Option<String>,
    end: Option<String>,
}

/// Projects included-credit utilization, its backend-declared period, the
/// backend-declared tier label when the body carries one, and the two monetary
/// balances the reviewed client renders: the prepaid credit balance and the
/// on-demand (pay-as-you-go) cap and spend.
///
/// Every projected value is read verbatim from a field the reviewed source
/// documents. Nothing is inferred: a body with no percentage yields no meter, a
/// body with no balance yields no balance, and per-period history is never
/// retained. The single derivation is the reviewed client's own documented
/// fallback, where legacy `used`/`monthlyLimit` cents become the percentage.
pub fn decode_grok_subscription_usage(
    body: &[u8],
    observed_at: Timestamp,
) -> Result<SubscriptionUsageSnapshot, SubscriptionUsageValueError> {
    if body.is_empty() || body.len() > MAX_GROK_SUBSCRIPTION_USAGE_BODY_BYTES {
        return Err(SubscriptionUsageValueError::Collection);
    }
    let response: BillingResponse =
        serde_json::from_slice(body).map_err(|_| SubscriptionUsageValueError::Collection)?;
    let plan = response
        .subscription_tier
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let Some(config) = response.config else {
        return SubscriptionUsageSnapshot::new(observed_at, plan, Vec::new());
    };
    let balances = project_balances(&config);
    let period = match config.current_period.as_ref() {
        Some(period) => Some(SubscriptionUsagePeriod::ProviderPeriod {
            kind: period
                .kind
                .clone()
                .unwrap_or_else(|| "provider-period".to_owned()),
            starts_at: period.start.as_deref().map(parse_timestamp).transpose()?,
        }),
        None => None,
    };
    let end = match config
        .current_period
        .as_ref()
        .and_then(|period| period.end.as_deref())
        .or(config.billing_period_end.as_deref())
    {
        Some(value) => Some(parse_timestamp(value)?),
        None => None,
    };
    let Some(percent) = included_percent(&config) else {
        // proto3 JSON omits a true 0.0 `creditUsagePercent`. Grok Build's
        // `/usage` therefore treats a named period with no percentage as 0%
        // used. A body with no period at all still has no meter.
        let Some(period) = period else {
            return SubscriptionUsageSnapshot::new(observed_at, plan, Vec::new())?
                .with_balances(balances);
        };
        let window = SubscriptionUsageWindow::new(
            None,
            SubscriptionUsagePercent::new(0.0)?,
            Some(period),
            end,
        )?;
        if window.ended_at(observed_at) {
            return Err(SubscriptionUsageValueError::Window);
        }
        let meter = SubscriptionUsageMeter::new("included-credits".to_owned(), None, vec![window])?;
        return SubscriptionUsageSnapshot::new(observed_at, plan, vec![meter])?
            .with_balances(balances);
    };
    let percent = SubscriptionUsagePercent::new(percent)?;
    if period.is_none() && end.is_none() {
        // A window with neither a declared period nor an end has no temporal
        // identity, so the percentage names no allowance and is not projected.
        return SubscriptionUsageSnapshot::new(observed_at, plan, Vec::new())?
            .with_balances(balances);
    }
    let window = SubscriptionUsageWindow::new(None, percent, period, end)?;
    if window.ended_at(observed_at) {
        return Err(SubscriptionUsageValueError::Window);
    }
    let meter = SubscriptionUsageMeter::new("included-credits".to_owned(), None, vec![window])?;
    SubscriptionUsageSnapshot::new(observed_at, plan, vec![meter])?.with_balances(balances)
}

/// The included-allowance percentage the response reports, preferring the
/// percentage field and falling back to the deprecated cent pair exactly as the
/// reviewed client does. An absent or non-positive legacy limit yields no
/// percentage rather than a fabricated `0%`.
fn included_percent(config: &BillingConfig) -> Option<f64> {
    if let Some(percent) = config.credit_usage_percent {
        return Some(percent);
    }
    let limit = config.monthly_limit.as_ref()?.val;
    let used = config.used.as_ref()?.val;
    if limit <= 0 {
        return None;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "cent counts stay far below 2^53 and the projection is display-only"
    )]
    let percent = (used as f64 / limit as f64) * 100.0;
    Some(percent.min(100.0))
}

/// The monetary balances the response reports. Billing stores credit and
/// top-up amounts as negative cents, so magnitudes are projected; a balance the
/// response omits is omitted here rather than projected as zero, while a
/// balance the response reports as zero (proto3 sends `{}`) stays a reported
/// zero.
fn project_balances(config: &BillingConfig) -> Vec<SubscriptionUsageBalance> {
    let mut balances = Vec::new();
    if let Some(prepaid) = config.prepaid_balance.as_ref().map(|cents| cents.val.abs())
        && let Ok(balance) = SubscriptionUsageBalance::new(
            SUBSCRIPTION_BALANCE_PREPAID_CREDITS.to_owned(),
            Some(prepaid),
            None,
            None,
        )
    {
        balances.push(balance);
    }
    // On-demand billing announces itself with a positive cap; a reported zero
    // cap is the provider saying pay-as-you-go is not enabled, which the
    // product states as such.
    if let Some(cap) = config.on_demand_cap.as_ref().map(|cents| cents.val.abs())
        && let Ok(balance) = SubscriptionUsageBalance::new(
            SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
            None,
            config.on_demand_used.as_ref().map(|cents| cents.val.abs()),
            Some(cap),
        )
    {
        balances.push(balance);
    }
    balances
}

fn parse_timestamp(value: &str) -> Result<Timestamp, SubscriptionUsageValueError> {
    let parsed =
        OffsetDateTime::parse(value, &Rfc3339).map_err(|_| SubscriptionUsageValueError::Window)?;
    let seconds =
        u64::try_from(parsed.unix_timestamp()).map_err(|_| SubscriptionUsageValueError::Window)?;
    Timestamp::new(seconds, parsed.nanosecond()).map_err(|_| SubscriptionUsageValueError::Window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_backend_period_tier_and_reported_balances()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"config":{"creditUsagePercent":12.25,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","start":"2026-08-20T12:00:00Z","end":"2026-08-27T12:00:00Z"},"prepaidBalance":{"val":-9999},"onDemandCap":{"val":5000},"onDemandUsed":{"val":355},"history":[{}]},"subscriptionTier":"SuperGrok Heavy"}"#;
        let snapshot = decode_grok_subscription_usage(body, Timestamp::new(1, 0)?)?;
        let window = &snapshot.meters()[0].windows()[0];
        assert!((window.used_percent().get() - 12.25).abs() < f64::EPSILON);
        assert!(
            matches!(window.period(), Some(SubscriptionUsagePeriod::ProviderPeriod { kind, .. }) if kind == "USAGE_PERIOD_TYPE_WEEKLY")
        );
        assert!(window.ends_at().is_some());
        assert_eq!(snapshot.plan(), Some("SuperGrok Heavy"));
        let balances = snapshot.balances();
        assert_eq!(balances.len(), 2);
        assert_eq!(balances[0].id(), SUBSCRIPTION_BALANCE_PREPAID_CREDITS);
        // Negative accounting cents project as the magnitude the client shows.
        assert_eq!(balances[0].remaining_cents(), Some(9999));
        assert_eq!(balances[1].id(), SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO);
        assert_eq!(balances[1].used_cents(), Some(355));
        assert_eq!(balances[1].cap_cents(), Some(5000));
        Ok(())
    }

    #[test]
    fn projects_the_documented_legacy_shape_and_its_period_end()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"config":{"monthlyLimit":{"val":2000},"used":{"val":500},"billingPeriodEnd":"2026-09-01T00:00:00Z"}}"#;
        let snapshot = decode_grok_subscription_usage(body, Timestamp::new(1, 0)?)?;
        let window = &snapshot.meters()[0].windows()[0];
        assert!((window.used_percent().get() - 25.0).abs() < f64::EPSILON);
        assert!(window.period().is_none());
        assert!(window.ends_at().is_some());
        Ok(())
    }

    #[test]
    fn keeps_reported_balances_when_no_allowance_percentage_is_reported()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"config":{"prepaidBalance":{"val":1250}}}"#;
        let snapshot = decode_grok_subscription_usage(body, Timestamp::new(1, 0)?)?;
        assert_eq!(snapshot.meters(), []);
        assert!(snapshot.unmetered_period().is_none());
        assert_eq!(snapshot.balances().len(), 1);
        assert_eq!(snapshot.balances()[0].remaining_cents(), Some(1250));
        Ok(())
    }

    /// Redacted shape of the live unified-billing SuperGrok body: a named
    /// weekly period with start and end, zero balances, and no usage figure.
    /// proto3 omits a true 0.0 `creditUsagePercent`, so this is 0% used.
    #[test]
    fn a_unified_billing_period_without_a_usage_figure_is_zero_percent()
    -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"config":{"billingPeriodEnd":"2026-09-10T13:22:28.845023+00:00","billingPeriodStart":"2026-09-03T13:22:28.845023+00:00","currentPeriod":{"end":"2026-09-10T13:22:28.845023+00:00","start":"2026-09-03T13:22:28.845023+00:00","type":"USAGE_PERIOD_TYPE_WEEKLY"},"isUnifiedBillingUser":true,"onDemandCap":{"val":0},"onDemandUsed":{"val":0},"prepaidBalance":{"val":0},"topUpMethod":"TOP_UP_METHOD_SAVED_PAYMENT_METHOD"}}"#;
        let snapshot = decode_grok_subscription_usage(body, Timestamp::new(1, 0)?)?;
        assert!(snapshot.unmetered_period().is_none());
        let window = &snapshot.meters()[0].windows()[0];
        assert!(window.used_percent().get().abs() < f64::EPSILON);
        assert!(
            matches!(window.period(), Some(SubscriptionUsagePeriod::ProviderPeriod { kind, starts_at: Some(_) }) if kind == "USAGE_PERIOD_TYPE_WEEKLY")
        );
        assert_eq!(
            window.ends_at().map(Timestamp::seconds),
            Some(1_789_046_548)
        );
        let balances = snapshot.balances();
        assert_eq!(balances.len(), 2);
        assert_eq!(balances[0].id(), SUBSCRIPTION_BALANCE_PREPAID_CREDITS);
        assert_eq!(balances[0].remaining_cents(), Some(0));
        assert_eq!(balances[1].id(), SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO);
        assert_eq!(balances[1].used_cents(), Some(0));
        assert_eq!(balances[1].cap_cents(), Some(0));
        Ok(())
    }

    #[test]
    fn preserves_absence_and_rejects_bad_projected_values() -> Result<(), Box<dyn std::error::Error>>
    {
        let observed = Timestamp::new(1, 0)?;
        let empty = decode_grok_subscription_usage(br#"{"config":null}"#, observed)?;
        assert!(empty.meters().is_empty() && empty.balances().is_empty());
        assert!(empty.unmetered_period().is_none());
        // A percentage with no period and no end names no allowance window.
        assert_eq!(
            decode_grok_subscription_usage(br#"{"config":{"creditUsagePercent":5}}"#, observed)?
                .meters(),
            []
        );
        // A zero legacy limit is not a zero-percent allowance.
        assert_eq!(
            decode_grok_subscription_usage(
                br#"{"config":{"monthlyLimit":{},"used":{"val":5},"billingPeriodEnd":"2026-09-01T00:00:00Z"}}"#,
                observed
            )?
            .meters(), []
        );
        assert!(
            decode_grok_subscription_usage(br#"{"config":{"creditUsagePercent":-1}}"#, observed)
                .is_err()
        );
        assert!(
            decode_grok_subscription_usage(
                br#"{"config":{"creditUsagePercent":1,"currentPeriod":{"end":"not-a-time"}}}"#,
                observed
            )
            .is_err()
        );
        Ok(())
    }
}
