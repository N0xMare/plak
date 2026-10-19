//! Agent-owned terminal report for an admitted mutating dispatch.

use std::fmt;

use plakous_tools::{ToolOutcome, ToolRuntimeError};

/// Whether an executor crossed the runtime's dispatch linearization point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolDispatchState {
    /// No executor was reached; no external effect was initiated by this call.
    NotStarted,
    /// Executor dispatch began after all required authorization and journal checks.
    Started,
}

/// Typed terminal report for a journal-guarded mutating dispatch.
///
/// Once dispatch starts, runtime failures without an authoritative executor
/// outcome are normalized to [`ToolOutcomeKind::OutcomeUnknown`](plakous_tools::ToolOutcomeKind::OutcomeUnknown). The retained
/// error form therefore describes a failure before dispatch or the executor's
/// own no-effect rejection or joined cancellation: an executor that classifies its stop as
/// [`ToolRuntimeErrorKind::Cancelled`](plakous_tools::ToolRuntimeErrorKind::Cancelled)
/// has reported a definite terminal, not a lost one.
#[derive(Clone)]
pub struct ToolDispatchReport {
    state: ToolDispatchState,
    result: Result<ToolOutcome, ToolRuntimeError>,
    uncertainty_cause: Option<ToolRuntimeError>,
}

impl ToolDispatchReport {
    pub(crate) fn not_started(error: ToolRuntimeError) -> Self {
        Self {
            state: ToolDispatchState::NotStarted,
            result: Err(error),
            uncertainty_cause: None,
        }
    }

    pub(crate) fn started(outcome: ToolOutcome) -> Self {
        Self {
            state: ToolDispatchState::Started,
            result: Ok(outcome),
            uncertainty_cause: None,
        }
    }

    pub(crate) fn started_unknown(cause: ToolRuntimeError) -> Self {
        Self {
            state: ToolDispatchState::Started,
            result: Ok(plakous_tools::internal::runtime_outcome_unknown()),
            uncertainty_cause: Some(cause),
        }
    }

    /// A started executor proved no effect or joined cancellation itself.
    ///
    /// The executor is the authority on its own stop; a possibly completed
    /// external effect must be reported by it as `OutcomeUnknown`, not as
    /// `Cancelled`. Treating the joined classification as unknown would turn
    /// every observed interrupt of a running mutating tool into a recovery
    /// obligation.
    pub(crate) fn started_error(error: ToolRuntimeError) -> Self {
        Self {
            state: ToolDispatchState::Started,
            result: Err(error),
            uncertainty_cause: None,
        }
    }

    /// Returns whether executor dispatch began.
    #[must_use]
    pub const fn state(&self) -> ToolDispatchState {
        self.state
    }

    /// Returns the authoritative or conservatively unknown executor outcome.
    #[must_use]
    pub fn outcome(&self) -> Option<&ToolOutcome> {
        self.result.as_ref().ok()
    }

    /// Returns a pre-dispatch failure, no-effect rejection, or joined cancellation.
    #[must_use]
    pub fn error(&self) -> Option<&ToolRuntimeError> {
        self.result.as_ref().err()
    }

    /// Returns the safe runtime cause that forced a started attempt to unknown.
    #[must_use]
    pub const fn uncertainty_cause(&self) -> Option<&ToolRuntimeError> {
        self.uncertainty_cause.as_ref()
    }

    /// Consumes the report and returns its outcome or retained failure.
    ///
    /// # Errors
    ///
    /// Returns the retained failure when executor dispatch never started or
    /// when the started executor proved no effect or joined cancellation.
    pub fn into_result(self) -> Result<ToolOutcome, ToolRuntimeError> {
        self.result
    }
}

impl fmt::Debug for ToolDispatchReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolDispatchReport")
            .field("state", &self.state)
            .field("outcome_kind", &self.outcome().map(ToolOutcome::kind))
            .field("error_kind", &self.error().map(ToolRuntimeError::kind))
            .field(
                "uncertainty_cause_kind",
                &self.uncertainty_cause().map(ToolRuntimeError::kind),
            )
            .finish_non_exhaustive()
    }
}
