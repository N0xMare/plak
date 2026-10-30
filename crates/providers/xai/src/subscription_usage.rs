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
#[cfg(feature = "native-auth")]
pub(crate) const GROK_SUBSCRIPTION_USAGE_PATH: &str = "/v1/billing?format=credits";
/// Maximum body accepted by the usage control-plane decoder.
pub(crate) const MAX_GROK_SUBSCRIPTION_USAGE_BODY_BYTES: usize = 256 * 1024;

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

impl Cents {
    /// Billing stores credits as negative cents; `i64::MIN` has no magnitude
    /// and marks the body malformed.
    fn magnitude(&self) -> Result<i64, SubscriptionUsageValueError> {
        self.val
            .checked_abs()
            .ok_or(SubscriptionUsageValueError::Collection)
    }
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
    let balances = project_balances(&config)?;
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
fn project_balances(
    config: &BillingConfig,
) -> Result<Vec<SubscriptionUsageBalance>, SubscriptionUsageValueError> {
    let mut balances = Vec::new();
    if let Some(prepaid) = config
        .prepaid_balance
        .as_ref()
        .map(Cents::magnitude)
        .transpose()?
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
    let used = config
        .on_demand_used
        .as_ref()
        .map(Cents::magnitude)
        .transpose()?;
    if let Some(cap) = config
        .on_demand_cap
        .as_ref()
        .map(Cents::magnitude)
        .transpose()?
        && let Ok(balance) = SubscriptionUsageBalance::new(
            SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
            None,
            used,
            Some(cap),
        )
    {
        balances.push(balance);
    }
    Ok(balances)
}

fn parse_timestamp(value: &str) -> Result<Timestamp, SubscriptionUsageValueError> {
    let parsed =
        OffsetDateTime::parse(value, &Rfc3339).map_err(|_| SubscriptionUsageValueError::Window)?;
    let seconds =
        u64::try_from(parsed.unix_timestamp()).map_err(|_| SubscriptionUsageValueError::Window)?;
    Timestamp::new(seconds, parsed.nanosecond()).map_err(|_| SubscriptionUsageValueError::Window)
}

#[cfg(test)]
mod tests;
