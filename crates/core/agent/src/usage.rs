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
mod tests;
