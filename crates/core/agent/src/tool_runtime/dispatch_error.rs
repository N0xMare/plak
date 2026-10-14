//! Private boundary between agent dispatch failures and portable executor errors.

use std::fmt;

use plakous_model::SafeDiagnostic;
use plakous_tools::{ToolRuntimeError, ToolRuntimeErrorKind};

/// Agent-owned runtime failure before normalization into the portable error
/// vocabulary retained by executor and durable contracts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ToolDispatchError {
    kind: ToolRuntimeErrorKind,
    diagnostic: SafeDiagnostic,
}

impl ToolDispatchError {
    pub(super) fn new(kind: ToolRuntimeErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            diagnostic: SafeDiagnostic::new(message),
        }
    }

    /// Imports a compatibility error returned by an injected agent control.
    #[allow(
        clippy::needless_pass_by_value,
        reason = "the explicit boundary consumes the portable control error"
    )]
    pub(super) fn from_control(error: ToolRuntimeError) -> Self {
        Self {
            kind: error.kind(),
            diagnostic: error.message().clone(),
        }
    }

    pub(super) const fn kind(&self) -> ToolRuntimeErrorKind {
        self.kind
    }

    pub(super) fn into_portable(self) -> ToolRuntimeError {
        ToolRuntimeError::new(self.kind(), self.diagnostic.as_str())
    }
}

impl fmt::Display for ToolDispatchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.diagnostic.fmt(formatter)
    }
}

impl std::error::Error for ToolDispatchError {}

/// Keeps executor-originated errors distinct from agent control-plane failures
/// until the retained portable result/report boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum DispatchFailure {
    Agent(ToolDispatchError),
    Executor(ToolRuntimeError),
}

impl DispatchFailure {
    pub(super) fn into_portable(self) -> ToolRuntimeError {
        match self {
            Self::Agent(error) => error.into_portable(),
            Self::Executor(error) => error,
        }
    }
}

impl From<ToolDispatchError> for DispatchFailure {
    fn from(error: ToolDispatchError) -> Self {
        Self::Agent(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_and_executor_failures_are_distinct_until_normalization() {
        let agent = DispatchFailure::Agent(ToolDispatchError::new(
            ToolRuntimeErrorKind::Execution,
            "agent supervisor failed",
        ));
        let executor = DispatchFailure::Executor(ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            "executor failed",
        ));

        assert!(matches!(agent, DispatchFailure::Agent(_)));
        assert!(matches!(executor, DispatchFailure::Executor(_)));
        let agent = agent.into_portable();
        let executor = executor.into_portable();
        assert_eq!(agent.kind(), executor.kind());
        assert_eq!(agent.to_string(), "agent supervisor failed");
        assert_eq!(executor.to_string(), "executor failed");
    }

    #[test]
    fn control_import_and_normalization_preserve_stable_wire_fields() {
        let portable = ToolRuntimeError::new(
            ToolRuntimeErrorKind::EffectSafety,
            "token=secret observer rejected dispatch",
        );
        let normalized = ToolDispatchError::from_control(portable.clone()).into_portable();

        assert_eq!(normalized.kind(), portable.kind());
        assert_eq!(normalized.message(), portable.message());
        assert!(!normalized.to_string().contains("secret"));
    }
}
