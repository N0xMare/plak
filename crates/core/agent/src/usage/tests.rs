use super::*;

#[test]
fn turn_usage_sums_final_operation_usage_and_tracks_missing_reports()
-> Result<(), UsageAggregationError> {
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(3, 5, 2, 1, 0)))?;
    usage.record_model_operation(None)?;
    usage.record_model_operation(Some(Usage::new(7, 11, 4, 3, 0)))?;
    usage.record_local_tool_attempt()?;

    assert_eq!(usage.model(), Usage::new(10, 16, 6, 4, 0));
    assert_eq!(usage.model_operations(), 3);
    assert_eq!(usage.model_operations_reporting_usage(), 2);
    assert_eq!(usage.local_tool_attempts(), 1);
    Ok(())
}

#[test]
fn totals_are_error_atomic_on_overflow() -> Result<(), UsageAggregationError> {
    let mut totals = UsageTotals {
        model: Usage::new(u64::MAX, 0, 0, 0, 0),
        turns: 4,
        model_operations: 4,
        model_operations_reporting_usage: 4,
        local_tool_attempts: 2,
    };
    let before = totals;
    let turn = TurnUsage::new(Usage::new(1, 0, 0, 0, 0), 1, 1, 0)?;

    assert_eq!(
        totals.record_turn(turn),
        Err(UsageAggregationError::Overflow)
    );
    assert_eq!(totals, before);
    Ok(())
}

#[test]
fn coverage_cannot_exceed_operations() {
    assert_eq!(
        TurnUsage::new(Usage::default(), 1, 2, 0),
        Err(UsageAggregationError::InvalidCoverage)
    );
}

#[test]
fn turn_usage_sums_cache_write_tokens() -> Result<(), UsageAggregationError> {
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(3, 5, 2, 1, 4)))?;
    usage.record_model_operation(Some(Usage::new(7, 11, 4, 3, 6)))?;

    assert_eq!(usage.model(), Usage::new(10, 16, 6, 4, 10));
    Ok(())
}

#[test]
fn turn_usage_sums_cost_micros_when_every_report_includes_cost() -> Result<(), UsageAggregationError>
{
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
    usage.record_model_operation(Some(Usage::new(3, 4, 0, 0, 0).with_cost_micros(Some(250))))?;

    assert_eq!(
        usage.model(),
        Usage::new(4, 6, 0, 0, 0).with_cost_micros(Some(350))
    );
    Ok(())
}

#[test]
fn turn_usage_cost_is_none_when_any_operation_has_no_usage() -> Result<(), UsageAggregationError> {
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
    usage.record_model_operation(None)?;
    usage.record_model_operation(Some(Usage::new(3, 4, 0, 0, 0).with_cost_micros(Some(250))))?;

    assert_eq!(usage.model().cost_micros(), None);
    Ok(())
}

#[test]
fn turn_usage_cost_is_none_when_any_reported_usage_omits_cost() -> Result<(), UsageAggregationError>
{
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
    usage.record_model_operation(Some(Usage::new(2, 3, 0, 0, 0)))?;

    assert_eq!(usage.model().cost_micros(), None);
    Ok(())
}

#[test]
fn turn_usage_cost_stays_none_without_cost_reports() -> Result<(), UsageAggregationError> {
    let mut usage = TurnUsage::default();
    usage.record_model_operation(Some(Usage::new(1, 2, 0, 0, 0)))?;
    usage.record_model_operation(None)?;

    assert_eq!(usage.model().cost_micros(), None);
    Ok(())
}

/// A terminal-result record written before the peak existed still decodes.
///
/// `TurnUsage` is persisted inside `PendingTerminalResultRecord`, and a
/// version mismatch is itself a decode rejection, so bumping the record
/// version to signal this field would break exactly the sessions the
/// default exists to keep loadable. An absent field means the peak is
/// unknown, which is what it was.
#[test]
fn a_turn_usage_persisted_before_the_peak_existed_still_decodes()
-> Result<(), Box<dyn std::error::Error>> {
    // Exactly the shape an earlier build wrote: every field but the peak.
    let earlier = serde_json::json!({
        "model": {
            "input_tokens": 1_024,
            "output_tokens": 128,
            "reasoning_tokens": 64,
            "cached_input_tokens": 0,
            "cache_write_tokens": 0
        },
        "model_operations": 3,
        "model_operations_reporting_usage": 3,
        "local_tool_attempts": 1,
        "usage_reports_missing_cost": 0
    });
    let decoded: TurnUsage = serde_json::from_value(earlier)?;
    assert_eq!(
        decoded.peak_input_tokens(),
        None,
        "absent means unknown, never zero"
    );
    // The durable terminal decoder re-encodes what it decoded and demands
    // the bytes back byte for byte, so an absent field must stay absent.
    // A `null` here is what made a candidate 2 session unresumable.
    let reencoded = serde_json::to_value(decoded)?;
    assert!(
        reencoded.get("peak_input_tokens").is_none(),
        "an unknown peak re-encodes as nothing, never as null: {reencoded}"
    );
    // Everything the earlier build did record survives unchanged.
    assert_eq!(decoded.model().input_tokens(), 1_024);
    assert_eq!(decoded.model_operations(), 3);
    assert_eq!(decoded.model_operations_reporting_usage(), 3);
    assert_eq!(decoded.local_tool_attempts(), 1);
    // A record written now round-trips with the peak intact.
    let mut fresh = TurnUsage::new(Usage::default(), 0, 0, 0)?;
    fresh.record_model_operation(Some(Usage::new(31_500, 1, 0, 0, 0)))?;
    fresh.record_model_operation(Some(Usage::new(12_000, 1, 0, 0, 0)))?;
    assert_eq!(fresh.peak_input_tokens(), Some(31_500));
    let round_tripped: TurnUsage = serde_json::from_value(serde_json::to_value(fresh)?)?;
    assert_eq!(round_tripped, fresh);
    Ok(())
}

/// The peak has one spelling per state: absent when unknown, a positive
/// figure when known. `null` and `0` are not spellings and refuse.
#[test]
fn the_peak_has_one_spelling_per_state() -> Result<(), Box<dyn std::error::Error>> {
    let body = |peak: Option<&str>| {
        let peak = peak.map_or_else(String::new, |peak| {
            format!(r#","peak_input_tokens":{peak}"#)
        });
        format!(
            r#"{{"model":{{"input_tokens":1024,"output_tokens":128,"reasoning_tokens":64,"cached_input_tokens":0,"cache_write_tokens":0,"cost_micros":null}},"model_operations":3,"model_operations_reporting_usage":0,"local_tool_attempts":1,"usage_reports_missing_cost":3{peak}}}"#
        )
    };
    for (stored, expected) in [(body(None), None), (body(Some("31500")), Some(31_500))] {
        let decoded: TurnUsage = serde_json::from_str(&stored)?;
        assert_eq!(decoded.peak_input_tokens(), expected);
        assert_eq!(serde_json::to_string(&decoded)?, stored);
    }
    for refused in [body(Some("0")), body(Some("null"))] {
        assert!(
            serde_json::from_str::<TurnUsage>(&refused).is_err(),
            "{refused}"
        );
    }

    // A record this build writes with no peak omits the key, which is the
    // candidate 6 shape and stays stable through a decode and re-encode.
    let fresh = TurnUsage::default();
    let written = serde_json::to_string(&fresh)?;
    assert!(!written.contains("peak_input_tokens"), "{written}");
    let round_tripped: TurnUsage = serde_json::from_str(&written)?;
    assert_eq!(serde_json::to_string(&round_tripped)?, written);
    Ok(())
}
#[test]
fn retired_attempts_merge_is_checked_and_changes_no_provider_fields() {
    let mut usage = TurnUsage::default();
    usage.record_retired_local_tool_attempts(u64::MAX).unwrap();
    assert_eq!(usage.local_tool_attempts(), u64::MAX);
    assert_eq!(usage.model_operations(), 0);
    assert_eq!(
        usage.record_local_tool_attempt(),
        Err(UsageAggregationError::Overflow)
    );
    assert_eq!(usage.local_tool_attempts(), u64::MAX);
}
