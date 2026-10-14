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
mod tests {
    use super::*;

    #[test]
    fn public_constructor_redacts_and_bounds_diagnostics() {
        let error = AgentError::new(
            AgentErrorKind::Internal,
            format!("authorization: Bearer secret-value {}", "x".repeat(4_000)),
        );
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(!display.contains("secret-value"));
        assert!(!debug.contains("secret-value"));
        assert!(display.len() <= 2_048);
    }

    #[test]
    fn model_and_tool_errors_propagate_only_safe_diagnostics() {
        const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
        let raw = format!(
            "authorization: Bearer {SECRET_SENTINEL} {}",
            "🦀".repeat(2_048)
        );
        let model = ModelError::new(ModelErrorKind::Protocol, raw.clone());
        let tool = ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, raw);

        for error in [
            AgentError::from_model(model.clone()),
            AgentError::from_tool(&tool),
        ] {
            assert!(!error.message().contains(SECRET_SENTINEL));
            assert!(!error.to_string().contains(SECRET_SENTINEL));
            assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
            assert!(error.message().len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
        }
    }

    #[test]
    fn model_errors_preserve_safe_provider_neutral_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = ModelErrorContext::new(
            plakous_model::ProtocolId::new("example.responses")?,
            plakous_model::RouteId::new("example.platform")?,
            plakous_model::ModelId::new("test-model")?,
            plakous_model::ModelErrorPhase::Connect,
            ModelRetryability::Exhausted,
        )?
        .with_http_status(plakous_model::ModelHttpStatus::new(503)?);
        let model = ModelError::contextual(
            ModelErrorKind::Transport,
            "transport retries exhausted",
            context.clone(),
        )?
        .with_source("connection reset after bounded retry");

        let agent = AgentError::from_model(model.clone());
        assert_eq!(agent.model_kind(), Some(ModelErrorKind::Transport));
        assert_eq!(agent.model_error(), Some(&model));
        assert_eq!(agent.model_context(), Some(&context));
        assert_eq!(
            agent
                .model_context()
                .and_then(ModelErrorContext::http_status)
                .map(plakous_model::ModelHttpStatus::get),
            Some(503)
        );
        assert!(std::error::Error::source(&agent).is_some());
        Ok(())
    }

    #[test]
    fn provider_hosted_effect_risk_requires_recovery_even_for_transport_kind()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = ModelErrorContext::new(
            plakous_model::ProtocolId::new("example.responses")?,
            plakous_model::RouteId::new("example.platform")?,
            plakous_model::ModelId::new("test-model")?,
            plakous_model::ModelErrorPhase::Stream,
            ModelRetryability::Unsafe {
                risk: ModelRetryRisk::ProviderHostedEffect,
            },
        )?;
        let model = ModelError::contextual(
            ModelErrorKind::Transport,
            "transport ended after hosted work became possible",
            context,
        )?;

        assert_eq!(
            AgentError::from_model(model).kind(),
            AgentErrorKind::OutcomeUnknown
        );
        Ok(())
    }

    #[test]
    fn paid_recovery_dominates_model_and_cancellation_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let target = plakous_model::ModelErrorTarget::new(
            plakous_model::ProtocolId::new("example.responses")?,
            plakous_model::RouteId::new("example.platform")?,
            plakous_model::ModelId::new("test-model")?,
        )?;
        let recovery = plakous_model::ModelPaidRecovery::new(
            b"typed-recovery-locator".to_vec(),
            plakous_model::ModelPaidTerminalFacts::new(
                1,
                plakous_model::ModelPaidProviderAcceptance::PossiblyAccepted,
                false,
                true,
                true,
            )?,
        )?;
        let model = ModelError::paid_recovery_required(
            "paid result requires reconciliation",
            &target,
            plakous_model::ModelErrorPhase::Cleanup,
            recovery,
        );

        for projected in [
            AgentError::from_model(model.clone()),
            AgentError::from_cancelled_model(model.clone()),
        ] {
            assert_eq!(projected.kind(), AgentErrorKind::OutcomeUnknown);
            assert_eq!(projected.model_kind(), Some(ModelErrorKind::OutcomeUnknown));
            assert_eq!(projected.model_error(), Some(&model));
            assert_eq!(
                projected
                    .model_error()
                    .and_then(ModelError::paid_recovery)
                    .map(plakous_model::ModelPaidRecovery::canonical_recovery_bytes),
                Some(b"typed-recovery-locator".as_slice())
            );
        }
        Ok(())
    }
}
