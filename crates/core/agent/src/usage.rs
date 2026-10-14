//! Checked provider-neutral usage aggregation.

use plakous_model::Usage;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A checked usage-counter overflow or inconsistent coverage value.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum UsageAggregationError {
    /// At least one counter could not be represented as a `u64`.
    #[error("usage aggregation exceeded its numeric range")]
    Overflow,
    /// More model operations reported usage than were observed.
    #[error("usage reporting coverage exceeds the model-operation count")]
    InvalidCoverage,
}

/// Checked usage accumulated across every model operation and local tool attempt in one turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnUsage {
    model: Usage,
    model_operations: u64,
    model_operations_reporting_usage: u64,
    local_tool_attempts: u64,
    usage_reports_missing_cost: u32,
    /// Largest single operation's provider-reported input, when one reported.
    ///
    /// Absent when unknown (no operation of the turn reported usage), never
    /// `null` or `0`: the terminal-record decoder re-encodes what it read and
    /// demands the bytes back, so each state has exactly one spelling.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "reported_peak"
    )]
    peak_input_tokens: Option<u64>,
}

/// A present peak is a provider-reported prompt size: `null` and `0` are not
/// spellings this record has, so they refuse to decode.
fn reported_peak<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    match u64::deserialize(deserializer)? {
        0 => Err(serde::de::Error::custom(
            "peak input tokens of zero is not a measurement",
        )),
        peak => Ok(Some(peak)),
    }
}

impl TurnUsage {
    /// Constructs an exact turn aggregate.
    ///
    /// # Errors
    ///
    /// Returns [`UsageAggregationError::InvalidCoverage`] when reporting
    /// coverage exceeds the operation count.
    pub const fn new(
        model: Usage,
        model_operations: u64,
        model_operations_reporting_usage: u64,
        local_tool_attempts: u64,
    ) -> Result<Self, UsageAggregationError> {
        if model_operations_reporting_usage > model_operations {
            return Err(UsageAggregationError::InvalidCoverage);
        }
        Ok(Self {
            model,
            model_operations,
            model_operations_reporting_usage,
            local_tool_attempts,
            usage_reports_missing_cost: 0,
            peak_input_tokens: None,
        })
    }

    /// Returns summed final per-operation model usage.
    #[must_use]
    pub const fn model(self) -> Usage {
        self.model
    }

    /// Returns the number of model operations, including operations with no usage report.
    #[must_use]
    pub const fn model_operations(self) -> u64 {
        self.model_operations
    }

    /// Returns the number of model operations which supplied usage.
    #[must_use]
    pub const fn model_operations_reporting_usage(self) -> u64 {
        self.model_operations_reporting_usage
    }

    /// Returns the largest single operation's provider-reported input tokens.
    ///
    /// `None` when no peak is known: no operation reported usage, or the
    /// record predates this field. This is the only number in this aggregate
    /// that does not sum: it measures how far the conversation grew against
    /// the model's window, which the summed `model().input_tokens()` cannot,
    /// because a many-round turn inflates the sum without any one prompt
    /// growing.
    #[must_use]
    pub const fn peak_input_tokens(self) -> Option<u64> {
        self.peak_input_tokens
    }

    /// Returns the number of attempted Plakous-local tool dispatches.
    #[must_use]
    pub const fn local_tool_attempts(self) -> u64 {
        self.local_tool_attempts
    }

    /// Records one completed or failed model operation and its final observed
    /// cumulative usage, when reported.
    ///
    /// # Errors
    ///
    /// Returns [`UsageAggregationError::Overflow`] rather than saturating.
    pub fn record_model_operation(
        &mut self,
        usage: Option<Usage>,
    ) -> Result<(), UsageAggregationError> {
        let model_operations = self
            .model_operations
            .checked_add(1)
            .ok_or(UsageAggregationError::Overflow)?;
        let (model, model_operations_reporting_usage, usage_reports_missing_cost) =
            if let Some(usage) = usage {
                let (cost_micros, usage_reports_missing_cost) = merge_operation_cost(
                    self.model.cost_micros(),
                    self.usage_reports_missing_cost,
                    usage.cost_micros(),
                )?;
                (
                    checked_add_usage(self.model, usage)?.with_cost_micros(cost_micros),
                    self.model_operations_reporting_usage
                        .checked_add(1)
                        .ok_or(UsageAggregationError::Overflow)?,
                    usage_reports_missing_cost,
                )
            } else {
                (
                    self.model.with_cost_micros(None),
                    self.model_operations_reporting_usage,
                    self.usage_reports_missing_cost.saturating_add(1),
                )
            };
        if let Some(usage) = usage {
            // The largest single operation's own complete prompt. `model` sums
            // over operations, so it says how much was paid for, never how far
            // the conversation grew against the window; only the per-operation
            // maximum answers that, and this is the one place that sees each
            // operation separately.
            let peak = self
                .peak_input_tokens
                .map_or(usage.input_tokens(), |peak| peak.max(usage.input_tokens()));
            self.peak_input_tokens = (peak > 0).then_some(peak);
        }
        self.model = model;
        self.model_operations = model_operations;
        self.model_operations_reporting_usage = model_operations_reporting_usage;
        self.usage_reports_missing_cost = usage_reports_missing_cost;
        Ok(())
    }

    /// Records one attempted local tool dispatch.
    ///
    /// # Errors
    ///
    /// Returns [`UsageAggregationError::Overflow`] rather than saturating.
    pub fn record_local_tool_attempt(&mut self) -> Result<(), UsageAggregationError> {
        self.local_tool_attempts = self
            .local_tool_attempts
            .checked_add(1)
            .ok_or(UsageAggregationError::Overflow)?;
        Ok(())
    }
}

/// Checked usage accumulated across completed logical turns.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    model: Usage,
    turns: u64,
    model_operations: u64,
    model_operations_reporting_usage: u64,
    local_tool_attempts: u64,
}

impl UsageTotals {
    /// Returns summed final per-operation model usage.
    #[must_use]
    pub const fn model(self) -> Usage {
        self.model
    }

    /// Returns the number of incorporated turns.
    #[must_use]
    pub const fn turns(self) -> u64 {
        self.turns
    }

    /// Returns the number of incorporated model operations.
    #[must_use]
    pub const fn model_operations(self) -> u64 {
        self.model_operations
    }

    /// Returns the number of incorporated operations which supplied usage.
    #[must_use]
    pub const fn model_operations_reporting_usage(self) -> u64 {
        self.model_operations_reporting_usage
    }

    /// Returns the number of incorporated local tool attempts.
    #[must_use]
    pub const fn local_tool_attempts(self) -> u64 {
        self.local_tool_attempts
    }

    /// Adds one exact turn aggregate.
    ///
    /// # Errors
    ///
    /// Returns [`UsageAggregationError::Overflow`] rather than partially
    /// updating or saturating any counter.
    pub fn record_turn(&mut self, turn: TurnUsage) -> Result<(), UsageAggregationError> {
        let model = checked_add_usage(self.model, turn.model)?;
        let turns = self
            .turns
            .checked_add(1)
            .ok_or(UsageAggregationError::Overflow)?;
        let model_operations = self
            .model_operations
            .checked_add(turn.model_operations)
            .ok_or(UsageAggregationError::Overflow)?;
        let model_operations_reporting_usage = self
            .model_operations_reporting_usage
            .checked_add(turn.model_operations_reporting_usage)
            .ok_or(UsageAggregationError::Overflow)?;
        let local_tool_attempts = self
            .local_tool_attempts
            .checked_add(turn.local_tool_attempts)
            .ok_or(UsageAggregationError::Overflow)?;
        self.model = model;
        self.turns = turns;
        self.model_operations = model_operations;
        self.model_operations_reporting_usage = model_operations_reporting_usage;
        self.local_tool_attempts = local_tool_attempts;
        Ok(())
    }
}

fn checked_add_usage(left: Usage, right: Usage) -> Result<Usage, UsageAggregationError> {
    Ok(Usage::new(
        left.input_tokens()
            .checked_add(right.input_tokens())
            .ok_or(UsageAggregationError::Overflow)?,
        left.output_tokens()
            .checked_add(right.output_tokens())
            .ok_or(UsageAggregationError::Overflow)?,
        left.reasoning_tokens()
            .checked_add(right.reasoning_tokens())
            .ok_or(UsageAggregationError::Overflow)?,
        left.cached_input_tokens()
            .checked_add(right.cached_input_tokens())
            .ok_or(UsageAggregationError::Overflow)?,
        left.cache_write_tokens()
            .checked_add(right.cache_write_tokens())
            .ok_or(UsageAggregationError::Overflow)?,
    ))
}

fn merge_operation_cost(
    accumulated: Option<u64>,
    usage_reports_missing_cost: u32,
    operation_cost: Option<u64>,
) -> Result<(Option<u64>, u32), UsageAggregationError> {
    let Some(operation_cost) = operation_cost else {
        return Ok((None, usage_reports_missing_cost.saturating_add(1)));
    };
    if usage_reports_missing_cost > 0 {
        return Ok((None, usage_reports_missing_cost.saturating_add(1)));
    }
    let sum = match accumulated {
        None => operation_cost,
        Some(sum) => sum
            .checked_add(operation_cost)
            .ok_or(UsageAggregationError::Overflow)?,
    };
    Ok((Some(sum), usage_reports_missing_cost))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
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
    fn turn_usage_sums_cost_micros_when_every_report_includes_cost()
    -> Result<(), UsageAggregationError> {
        let mut usage = TurnUsage::default();
        usage
            .record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
        usage
            .record_model_operation(Some(Usage::new(3, 4, 0, 0, 0).with_cost_micros(Some(250))))?;

        assert_eq!(
            usage.model(),
            Usage::new(4, 6, 0, 0, 0).with_cost_micros(Some(350))
        );
        Ok(())
    }

    #[test]
    fn turn_usage_cost_is_none_when_any_operation_has_no_usage() -> Result<(), UsageAggregationError>
    {
        let mut usage = TurnUsage::default();
        usage
            .record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
        usage.record_model_operation(None)?;
        usage
            .record_model_operation(Some(Usage::new(3, 4, 0, 0, 0).with_cost_micros(Some(250))))?;

        assert_eq!(usage.model().cost_micros(), None);
        Ok(())
    }

    #[test]
    fn turn_usage_cost_is_none_when_any_reported_usage_omits_cost()
    -> Result<(), UsageAggregationError> {
        let mut usage = TurnUsage::default();
        usage
            .record_model_operation(Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(100))))?;
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
}
