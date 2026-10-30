//! Typed, redaction-safe agent errors.

use std::fmt;
use std::num::NonZeroU32;

use plakous_model::{
    IdentityError, ModelDispatchRejection, ModelError, ModelErrorContext, ModelErrorKind,
    ModelRetryRisk, ModelRetryability, SafeDiagnostic,
};
use plakous_tools::{ToolRuntimeError, ToolRuntimeErrorKind};
use serde::{Deserialize, Serialize};

use crate::SessionStoreError;

/// Stable category for an agent-runtime failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[non_exhaustive]
pub enum AgentErrorKind {
    /// Agent construction or prompt input was invalid.
    InvalidInput,
    /// The bounded prompt queue has no remaining capacity.
    Backpressure,
    /// The agent no longer accepts work.
    Shutdown,
    /// The exact turn was cancelled.
    Cancelled,
    /// Canonical request or response processing failed.
    Model,
    /// Tool validation or execution failed operationally.
    Tool,
    /// A required process-local model, tool, workspace, or policy binding is unavailable.
    ExternalBindingUnavailable,
    /// A mutating dispatch may have occurred and requires reconciliation.
    OutcomeUnknown,
    /// Session ownership, commit, or durability failed.
    Session,
    /// The stored session was written by an incompatible build: it does not
    /// decode, or this build reconstructs different semantics from it.
    IncompatibleSession,
    /// The configured model/tool round budget was exhausted.
    RoundLimit,
    /// An injected identity source failed.
    Identity,
    /// An owned runtime task failed unexpectedly.
    Internal,
}

/// Bounded public agent error which retains no prompt, tool arguments, or credentials.
#[derive(Clone, Eq, PartialEq)]
pub struct AgentError {
    kind: AgentErrorKind,
    message: SafeDiagnostic,
    model_error: Option<Box<ModelError>>,
    tool_kind: Option<ToolRuntimeErrorKind>,
    store_error: Option<SessionStoreError>,
}

impl AgentError {
    pub(crate) fn with_runtime_diagnostic(mut self, diagnostic: &str) -> Self {
        self.message = SafeDiagnostic::new(format!("{}; {diagnostic}", self.message()));
        self
    }

    /// Creates a sanitized runtime error.
    #[must_use]
    pub fn new(kind: AgentErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message),
            model_error: None,
            tool_kind: None,
            store_error: None,
        }
    }

    /// Returns the stable category.
    #[must_use]
    pub const fn kind(&self) -> AgentErrorKind {
        self.kind
    }

    /// Returns the redaction-safe diagnostic.
    #[must_use]
    pub fn message(&self) -> &str {
        self.message.as_str()
    }

    /// Returns the originating model category when applicable.
    #[must_use]
    pub fn model_kind(&self) -> Option<ModelErrorKind> {
        self.model_error().map(ModelError::kind)
    }

    /// Returns the originating model-operation context when applicable.
    #[must_use]
    pub fn model_context(&self) -> Option<&ModelErrorContext> {
        self.model_error().and_then(ModelError::context)
    }

    /// Returns the complete sanitized originating model error when applicable.
    #[must_use]
    pub fn model_error(&self) -> Option<&ModelError> {
        self.model_error.as_deref()
    }

    /// Records a prepared wire output maximum on an output-limit failure that
    /// does not already name one.
    pub(crate) fn fill_missing_wire_output_limit(&mut self, tokens: Option<NonZeroU32>) {
        let Some(tokens) = tokens else {
            return;
        };
        if let Some(error) = self.model_error.as_mut() {
            error.fill_missing_wire_output_limit(tokens);
        }
    }

    /// Returns exact provider-family pre-dispatch rejection proof, when the
    /// originating prepared call supplied one.
    #[must_use]
    pub fn model_dispatch_rejection(&self) -> Option<ModelDispatchRejection> {
        self.model_error().and_then(ModelError::dispatch_rejection)
    }

    /// Returns the originating tool category when applicable.
    #[must_use]
    pub const fn tool_kind(&self) -> Option<ToolRuntimeErrorKind> {
        self.tool_kind
    }

    /// Returns whether physical storage, rather than a logical retention bound,
    /// refused the originating store operation.
    #[must_use]
    pub fn is_storage_full(&self) -> bool {
        self.store_error == Some(SessionStoreError::StorageFull)
    }

    pub(crate) const fn store_error(&self) -> Option<SessionStoreError> {
        self.store_error
    }

    pub(crate) fn from_model(error: ModelError) -> Self {
        let kind = if matches!(
            error.context().map(ModelErrorContext::retryability),
            Some(ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect
            })
        ) {
            AgentErrorKind::OutcomeUnknown
        } else {
            match error.kind() {
                ModelErrorKind::OutcomeUnknown => AgentErrorKind::OutcomeUnknown,
                ModelErrorKind::Cancelled => AgentErrorKind::Cancelled,
                _ => AgentErrorKind::Model,
            }
        };
        Self {
            kind,
            message: error.diagnostic().clone(),
            model_error: Some(Box::new(error)),
            tool_kind: None,
            store_error: None,
        }
    }

    pub(crate) fn from_cancelled_model(error: ModelError) -> Self {
        let kind =
            if error.kind() == ModelErrorKind::OutcomeUnknown || error.paid_recovery().is_some() {
                AgentErrorKind::OutcomeUnknown
            } else {
                AgentErrorKind::Cancelled
            };
        Self {
            kind,
            message: error.diagnostic().clone(),
            model_error: Some(Box::new(error)),
            tool_kind: None,
            store_error: None,
        }
    }

    pub(crate) fn from_outcome_unknown_model(error: ModelError) -> Self {
        Self {
            kind: AgentErrorKind::OutcomeUnknown,
            message: error.diagnostic().clone(),
            model_error: Some(Box::new(error)),
            tool_kind: None,
            store_error: None,
        }
    }

    pub(crate) fn from_tool(error: &ToolRuntimeError) -> Self {
        let kind = if error.kind() == ToolRuntimeErrorKind::Cancelled {
            AgentErrorKind::Cancelled
        } else {
            AgentErrorKind::Tool
        };
        Self {
            kind,
            message: error.message().clone(),
            model_error: None,
            tool_kind: Some(error.kind()),
            store_error: None,
        }
    }

    pub(crate) fn from_identity(error: &IdentityError) -> Self {
        Self::new(AgentErrorKind::Identity, error.to_string())
    }

    pub(crate) fn from_store(error: SessionStoreError) -> Self {
        Self {
            kind: AgentErrorKind::Session,
            message: SafeDiagnostic::new(error.to_string()),
            model_error: None,
            tool_kind: None,
            store_error: Some(error),
        }
    }

    pub(crate) fn from_ambiguous_store(error: SessionStoreError) -> Self {
        Self {
            kind: AgentErrorKind::OutcomeUnknown,
            message: SafeDiagnostic::new(
                "checkpoint commit acknowledgement requires exact recovery",
            ),
            model_error: None,
            tool_kind: None,
            store_error: Some(error),
        }
    }

    pub(crate) fn from_canonical_parts(
        kind: AgentErrorKind,
        message: impl AsRef<str>,
        model_error: Option<ModelError>,
        tool_kind: Option<ToolRuntimeErrorKind>,
        store_error: Option<SessionStoreError>,
    ) -> Self {
        Self {
            kind,
            message: SafeDiagnostic::new(message),
            model_error: model_error.map(Box::new),
            tool_kind,
            store_error,
        }
    }
}

impl fmt::Debug for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("model_kind", &self.model_kind())
            .field("model_error", &self.model_error)
            .field("tool_kind", &self.tool_kind)
            .field("store_error", &self.store_error)
            .finish()
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.model_error
            .as_deref()
            .map(|error| error as &(dyn std::error::Error + 'static))
            .or_else(|| {
                self.store_error
                    .as_ref()
                    .filter(|error| self.message() != error.to_string())
                    .map(|error| error as &dyn std::error::Error)
            })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
