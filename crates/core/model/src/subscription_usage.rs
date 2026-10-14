//! Provider-neutral, observational subscription-usage values.
//!
//! These values describe one read-only control-plane observation. They grant
//! no inference, billing, admission, retry, checkpoint, or recovery authority.

use crate::Timestamp;
use thiserror::Error;

/// Maximum meters retained from one provider response.
pub const MAX_SUBSCRIPTION_USAGE_METERS: usize = 32;
/// Maximum windows retained for one meter.
pub const MAX_SUBSCRIPTION_USAGE_WINDOWS: usize = 8;
/// Maximum provider label length retained in a snapshot.
pub const MAX_SUBSCRIPTION_USAGE_LABEL_BYTES: usize = 128;
/// Maximum monetary balances retained from one provider response.
pub const MAX_SUBSCRIPTION_USAGE_BALANCES: usize = 8;

/// Neutral identity of a prepaid ("bought") credit balance a provider reports.
pub const SUBSCRIPTION_BALANCE_PREPAID_CREDITS: &str = "prepaid-credits";
/// Neutral identity of an on-demand (pay-as-you-go) allowance a provider
/// reports as a spend against a cap.
pub const SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO: &str = "pay-as-you-go";

/// Validation failure for a provider usage projection.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SubscriptionUsageValueError {
    /// A label was empty, oversized, or contained control characters.
    #[error("invalid subscription usage label")]
    Label,
    /// A provider percentage was non-finite or outside 0 through 100.
    #[error("invalid subscription usage percentage")]
    Percentage,
    /// A collection exceeded its closed bound.
    #[error("subscription usage collection exceeds its bound")]
    Collection,
    /// A window had neither a duration/period nor an end/reset time.
    #[error("subscription usage window has no temporal identity")]
    Window,
}

/// Provider-reported percentage consumed, inclusively bounded to 0 through 100.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SubscriptionUsagePercent(f64);

impl SubscriptionUsagePercent {
    /// Validates a provider-reported percentage without rounding it.
    ///
    /// # Errors
    ///
    /// Returns [`SubscriptionUsageValueError::Percentage`] when out of range.
    pub fn new(value: f64) -> Result<Self, SubscriptionUsageValueError> {
        if !value.is_finite() || !(0.0..=100.0).contains(&value) {
            return Err(SubscriptionUsageValueError::Percentage);
        }
        Ok(Self(value))
    }

    /// Returns the exact decoded provider value.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Temporal shape reported for an allowance window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionUsagePeriod {
    /// A rolling window with its actual provider-reported duration.
    Rolling {
        /// Exact duration reported by the provider.
        duration_seconds: u64,
    },
    /// A provider-named calendar/billing period. Unknown names are preserved.
    ProviderPeriod {
        /// Provider-declared period type; unknown values remain intact.
        kind: String,
        /// Optional provider-reported period start.
        starts_at: Option<Timestamp>,
    },
}

/// One optional allowance window within a provider meter.
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionUsageWindow {
    name: Option<String>,
    used_percent: SubscriptionUsagePercent,
    period: Option<SubscriptionUsagePeriod>,
    ends_at: Option<Timestamp>,
}

impl SubscriptionUsageWindow {
    /// Constructs one validated provider window.
    ///
    /// # Errors
    ///
    /// Returns a typed validation failure for invalid labels or periods.
    pub fn new(
        name: Option<String>,
        used_percent: SubscriptionUsagePercent,
        period: Option<SubscriptionUsagePeriod>,
        ends_at: Option<Timestamp>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        if let Some(name) = &name {
            validate_label(name)?;
        }
        if period.is_none() && ends_at.is_none() {
            return Err(SubscriptionUsageValueError::Window);
        }
        match &period {
            Some(SubscriptionUsagePeriod::Rolling { duration_seconds })
                if *duration_seconds == 0 =>
            {
                return Err(SubscriptionUsageValueError::Window);
            }
            Some(SubscriptionUsagePeriod::ProviderPeriod { kind, .. }) => {
                validate_label(kind)?;
            }
            Some(SubscriptionUsagePeriod::Rolling { .. }) | None => {}
        }
        Ok(Self {
            name,
            used_percent,
            period,
            ends_at,
        })
    }

    #[must_use]
    /// Returns the provider-local window name, when supplied.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    #[must_use]
    /// Returns the percentage consumed as reported by the provider.
    pub const fn used_percent(&self) -> SubscriptionUsagePercent {
        self.used_percent
    }
    #[must_use]
    /// Returns the optional rolling or provider-declared period.
    pub const fn period(&self) -> Option<&SubscriptionUsagePeriod> {
        self.period.as_ref()
    }
    #[must_use]
    /// Returns the provider-reported reset or period end.
    pub const fn ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }

    /// Reports whether the provider-declared end is no later than observation.
    #[must_use]
    pub fn ended_at(&self, observed_at: Timestamp) -> bool {
        self.ends_at.is_some_and(|ends_at| ends_at <= observed_at)
    }
}

/// One provider meter, with zero or more independently optional windows.
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionUsageMeter {
    id: String,
    name: Option<String>,
    windows: Vec<SubscriptionUsageWindow>,
}

impl SubscriptionUsageMeter {
    /// Constructs one bounded meter. An empty window list preserves absence.
    ///
    /// # Errors
    ///
    /// Returns a typed validation failure for labels or collection bounds.
    pub fn new(
        id: String,
        name: Option<String>,
        windows: Vec<SubscriptionUsageWindow>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        validate_label(&id)?;
        if let Some(name) = &name {
            validate_label(name)?;
        }
        if windows.len() > MAX_SUBSCRIPTION_USAGE_WINDOWS {
            return Err(SubscriptionUsageValueError::Collection);
        }
        Ok(Self { id, name, windows })
    }

    #[must_use]
    /// Returns the stable provider meter identifier.
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    /// Returns the optional provider display name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    #[must_use]
    /// Returns the independently optional windows in provider order.
    pub fn windows(&self) -> &[SubscriptionUsageWindow] {
        &self.windows
    }
}

/// One provider-reported monetary balance attached to a subscription
/// observation.
///
/// This is an observation of what the provider's own billing surface reports,
/// never a derived, inferred, or estimated amount. Every component is optional
/// because absence is meaningful and must be rendered as such: a provider that
/// reports a cap but no spend is not a provider that reports zero spend.
/// Amounts are the provider's minor units (USD cents) exactly as read, and
/// carry no billing, admission, or retry authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionUsageBalance {
    id: String,
    remaining_cents: Option<i64>,
    used_cents: Option<i64>,
    cap_cents: Option<i64>,
}

impl SubscriptionUsageBalance {
    /// Constructs a balance under the neutral `id` the product maps to an
    /// operator-facing label.
    ///
    /// # Errors
    ///
    /// Returns a label failure when `id` is empty, oversized, or
    /// control-bearing.
    pub fn new(
        id: String,
        remaining_cents: Option<i64>,
        used_cents: Option<i64>,
        cap_cents: Option<i64>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        validate_label(&id)?;
        Ok(Self {
            id,
            remaining_cents,
            used_cents,
            cap_cents,
        })
    }

    #[must_use]
    /// Returns the neutral balance identity.
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    /// Returns the provider-reported remaining amount, in minor units.
    pub const fn remaining_cents(&self) -> Option<i64> {
        self.remaining_cents
    }
    #[must_use]
    /// Returns the provider-reported consumed amount, in minor units.
    pub const fn used_cents(&self) -> Option<i64> {
        self.used_cents
    }
    #[must_use]
    /// Returns the provider-reported ceiling, in minor units.
    pub const fn cap_cents(&self) -> Option<i64> {
        self.cap_cents
    }
}

/// The provider-declared current allowance period when the response names
/// the period but reports no usage figure for it.
///
/// This is not a meter: it carries no percentage and never becomes one. It
/// exists so the product can state truthfully that the provider named a
/// period (and its reset) while reporting no consumption against it, instead
/// of either inventing `0%` or hiding the period the provider did report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscriptionUsageUnmeteredPeriod {
    period: SubscriptionUsagePeriod,
    ends_at: Option<Timestamp>,
}

impl SubscriptionUsageUnmeteredPeriod {
    /// Constructs one validated provider period without a usage reading.
    ///
    /// # Errors
    ///
    /// Returns a window failure for a zero rolling duration or a label
    /// failure for an invalid provider period name.
    pub fn new(
        period: SubscriptionUsagePeriod,
        ends_at: Option<Timestamp>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        match &period {
            SubscriptionUsagePeriod::Rolling { duration_seconds } if *duration_seconds == 0 => {
                return Err(SubscriptionUsageValueError::Window);
            }
            SubscriptionUsagePeriod::ProviderPeriod { kind, .. } => validate_label(kind)?,
            SubscriptionUsagePeriod::Rolling { .. } => {}
        }
        Ok(Self { period, ends_at })
    }

    #[must_use]
    /// Returns the provider-declared period shape.
    pub const fn period(&self) -> &SubscriptionUsagePeriod {
        &self.period
    }
    #[must_use]
    /// Returns the provider-reported period end, when supplied.
    pub const fn ends_at(&self) -> Option<Timestamp> {
        self.ends_at
    }
}

/// One fresh read-only provider subscription observation.
#[derive(Clone, Debug, PartialEq)]
pub struct SubscriptionUsageSnapshot {
    observed_at: Timestamp,
    plan: Option<String>,
    account: Option<String>,
    meters: Vec<SubscriptionUsageMeter>,
    balances: Vec<SubscriptionUsageBalance>,
    unmetered_period: Option<SubscriptionUsageUnmeteredPeriod>,
}

impl SubscriptionUsageSnapshot {
    /// Constructs a bounded snapshot. An empty list means the provider returned
    /// no projectable meters; it does not mean zero usage. `plan` carries the
    /// provider-reported plan or tier label verbatim; `None` preserves absence
    /// and must never be guessed.
    ///
    /// # Errors
    ///
    /// Returns a collection failure when the meter bound is exceeded, or a
    /// label failure when the plan label is empty, oversized, or control-bearing.
    pub fn new(
        observed_at: Timestamp,
        plan: Option<String>,
        meters: Vec<SubscriptionUsageMeter>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        if meters.len() > MAX_SUBSCRIPTION_USAGE_METERS {
            return Err(SubscriptionUsageValueError::Collection);
        }
        if let Some(plan) = &plan {
            validate_label(plan)?;
        }
        Ok(Self {
            observed_at,
            plan,
            account: None,
            meters,
            balances: Vec::new(),
            unmetered_period: None,
        })
    }

    /// Attaches the provider-reported account label (for example an e-mail
    /// address) verbatim. `None` preserves absence; the label is never derived
    /// from a credential and never guessed.
    ///
    /// # Errors
    ///
    /// Returns a label failure when the account label is empty, oversized, or
    /// control-bearing.
    pub fn with_account(
        mut self,
        account: Option<String>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        if let Some(account) = &account {
            validate_label(account)?;
        }
        self.account = account;
        Ok(self)
    }

    /// Attaches the provider-reported plan or tier label. `None` is a no-op so
    /// a later identity read cannot erase a plan the usage body already named.
    ///
    /// # Errors
    ///
    /// Returns a label failure when the plan label is empty, oversized, or
    /// control-bearing.
    pub fn with_plan(mut self, plan: Option<String>) -> Result<Self, SubscriptionUsageValueError> {
        let Some(plan) = plan else {
            return Ok(self);
        };
        validate_label(&plan)?;
        if self.plan.is_none() {
            self.plan = Some(plan);
        }
        Ok(self)
    }

    /// Attaches the provider-reported monetary balances verbatim. An empty list
    /// preserves absence; a balance is never synthesised from a meter.
    ///
    /// # Errors
    ///
    /// Returns a collection failure when the balance bound is exceeded.
    pub fn with_balances(
        mut self,
        balances: Vec<SubscriptionUsageBalance>,
    ) -> Result<Self, SubscriptionUsageValueError> {
        if balances.len() > MAX_SUBSCRIPTION_USAGE_BALANCES {
            return Err(SubscriptionUsageValueError::Collection);
        }
        self.balances = balances;
        Ok(self)
    }

    /// Attaches the provider-declared current period for a response that
    /// names the period but reports no usage against it. `None` preserves
    /// absence. A period is never attached to a snapshot that already carries
    /// meters, since those windows name their own periods.
    #[must_use]
    pub fn with_unmetered_period(
        mut self,
        period: Option<SubscriptionUsageUnmeteredPeriod>,
    ) -> Self {
        self.unmetered_period = if self.meters.is_empty() { period } else { None };
        self
    }

    #[must_use]
    /// Returns when the provider response was fully observed.
    pub const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }
    #[must_use]
    /// Returns the provider-reported plan or tier label, when supplied.
    pub fn plan(&self) -> Option<&str> {
        self.plan.as_deref()
    }
    #[must_use]
    /// Returns the provider-reported account label, when supplied.
    pub fn account(&self) -> Option<&str> {
        self.account.as_deref()
    }
    #[must_use]
    /// Returns the provider meters in provider order.
    pub fn meters(&self) -> &[SubscriptionUsageMeter] {
        &self.meters
    }
    #[must_use]
    /// Returns the provider-reported monetary balances in provider order.
    pub fn balances(&self) -> &[SubscriptionUsageBalance] {
        &self.balances
    }
    #[must_use]
    /// Returns the provider-declared current period when the response named
    /// a period without any usage figure; `None` when meters exist or no
    /// period was reported.
    pub const fn unmetered_period(&self) -> Option<&SubscriptionUsageUnmeteredPeriod> {
        self.unmetered_period.as_ref()
    }

    /// Reports whether any projected window had already ended when observed.
    #[must_use]
    pub fn contains_ended_window(&self) -> bool {
        self.meters.iter().any(|meter| {
            meter
                .windows
                .iter()
                .any(|window| window.ended_at(self.observed_at))
        })
    }
}

/// Local lifecycle of a selected credential that exists but is not `Ready`.
///
/// This is the neutral projection of the credential store's status. It names
/// the exact state so the operator learns the remedy, instead of a generic
/// "not ready" or a transport failure that never happened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionUsageCredentialState {
    /// The access token must be refreshed by provider-owned logic before use.
    RefreshRequired,
    /// A refresh may have reached the provider; automatic retry is fenced
    /// until provider-owned reconciliation or a fresh login.
    Quarantined,
    /// The credential was logged out or revoked and carries no payload.
    Revoked,
    /// The stored login cannot be used by this build; a new login is required.
    LoginRequired,
}

/// Typed reason an explicit read produced no trustworthy snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionUsageUnavailable {
    /// No selected subscription credential exists.
    NotConnected,
    /// A selected credential exists but its local lifecycle is not `Ready`.
    CredentialState(SubscriptionUsageCredentialState),
    /// Another Plak session, process, or in-flight credential refresh holds
    /// this provider's lock or lease. Nothing was sent and nothing failed.
    Busy,
    /// Local compatibility, storage, or recovery state refused observation.
    LocalState,
    /// The provider rejected the selected credential or account.
    Unauthorized,
    /// The provider rate-limited this read; no retry was attempted.
    RateLimited,
    /// The provider returned another non-success status.
    ProviderUnavailable,
    /// The bounded one-attempt HTTP exchange itself failed. Never used for a
    /// local lock, lease, or credential-state refusal.
    Transport(Option<crate::ModelNetworkFailure>),
    /// The response was oversized, malformed, or semantically invalid.
    InvalidResponse,
    /// A decoded provider window had already ended when observed.
    Stale,
    /// The selected credential generation changed during the read.
    AccountChanged,
    /// No reviewed transport qualification exists for this provider route.
    Unqualified,
}

/// Result of exactly one explicit usage-read attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum SubscriptionUsageRead {
    /// A fresh, bounded provider observation.
    Snapshot(SubscriptionUsageSnapshot),
    /// No trustworthy snapshot was available from this attempt.
    Unavailable {
        /// Time at which the failed/unavailable observation completed.
        observed_at: Timestamp,
        /// Typed redaction-safe unavailability reason.
        reason: SubscriptionUsageUnavailable,
    },
}

fn validate_label(value: &str) -> Result<(), SubscriptionUsageValueError> {
    if value.is_empty()
        || value.len() > MAX_SUBSCRIPTION_USAGE_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SubscriptionUsageValueError::Label);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_require_temporal_identity_and_staleness_is_boundary_exact()
    -> Result<(), Box<dyn std::error::Error>> {
        let percent = SubscriptionUsagePercent::new(1.0)?;
        assert!(SubscriptionUsageWindow::new(None, percent, None, None).is_err());
        let observed = Timestamp::new(10, 0)?;
        for (ends_at, ended) in [(9, true), (10, true), (11, false)] {
            let window = SubscriptionUsageWindow::new(
                None,
                percent,
                None,
                Some(Timestamp::new(ends_at, 0)?),
            )?;
            assert_eq!(window.ended_at(observed), ended);
        }
        Ok(())
    }

    #[test]
    fn balances_preserve_absence_and_stay_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(10, 0)?;
        let snapshot = SubscriptionUsageSnapshot::new(observed, None, Vec::new())?;
        assert_eq!(snapshot.balances(), []);
        let credits =
            SubscriptionUsageBalance::new("prepaid-credits".to_owned(), Some(1250), None, None)?;
        assert_eq!(credits.remaining_cents(), Some(1250));
        assert_eq!(credits.used_cents(), None);
        assert_eq!(credits.cap_cents(), None);
        let snapshot = snapshot.with_balances(vec![credits])?;
        assert_eq!(snapshot.balances()[0].id(), "prepaid-credits");
        assert!(SubscriptionUsageBalance::new(String::new(), None, None, None).is_err());
        let too_many = (0..=MAX_SUBSCRIPTION_USAGE_BALANCES)
            .map(|index| SubscriptionUsageBalance::new(format!("b{index}"), None, None, None))
            .collect::<Result<Vec<_>, _>>()?;
        assert!(snapshot.with_balances(too_many).is_err());
        Ok(())
    }
}
