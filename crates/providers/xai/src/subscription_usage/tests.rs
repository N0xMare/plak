use super::*;

#[test]
fn preserves_backend_period_tier_and_reported_balances() -> Result<(), Box<dyn std::error::Error>> {
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
fn rejects_cent_amounts_without_a_magnitude() -> Result<(), Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1, 0)?;
    for field in ["prepaidBalance", "onDemandCap", "onDemandUsed"] {
        let body = format!(
            r#"{{"config":{{"onDemandCap":{{"val":5}},"{field}":{{"val":-9223372036854775808}}}}}}"#
        );
        assert!(
            matches!(
                decode_grok_subscription_usage(body.as_bytes(), observed),
                Err(SubscriptionUsageValueError::Collection)
            ),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn preserves_absence_and_rejects_bad_projected_values() -> Result<(), Box<dyn std::error::Error>> {
    let observed = Timestamp::new(1, 0)?;
    let empty = decode_grok_subscription_usage(br#"{"config":null}"#, observed)?;
    assert!(empty.meters().is_empty() && empty.balances().is_empty());
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
