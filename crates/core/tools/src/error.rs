//! Typed, bounded, secret-safe tool runtime errors.

use std::fmt;

/// Model-owned bounded public diagnostic, retained under the historical tools name.
pub use plakous_model::SafeDiagnostic as SafeErrorMessage;

/// Maximum bytes retained in a public tool error message.
pub const MAX_SAFE_ERROR_BYTES: usize = plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES;

/// Stable category for a tool runtime failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ToolRuntimeErrorKind {
    /// A registration or registry snapshot was invalid.
    Registration,
    /// A tool definition or schema was invalid or unsupported.
    InvalidDefinition,
    /// A requested tool was not registered.
    UnknownTool,
    /// Invocation arguments were malformed, oversized, or schema-invalid.
    InvalidArguments,
    /// Tool policy, decision, or authorization binding failed.
    Policy,
    /// The requested tool/policy combination is intentionally not implemented.
    UnsupportedCapability,
    /// Effect metadata, journaling, or dispatch safety could not be proven.
    EffectSafety,
    /// Execution was cancelled before a terminal tool outcome.
    Cancelled,
    /// Execution exceeded its caller/environment-clamped absolute deadline.
    ExecutionTimeout,
    /// Joined cleanup did not complete within the environment cleanup grace period.
    CleanupTimeout,
    /// The injected workspace policy rejected a path.
    WorkspacePolicy,
    /// The tool executor failed operationally.
    Execution,
    /// A configured resource limit was exceeded.
    ResourceLimit,
    /// A deterministic scripted executor had no matching step.
    ScriptExhausted,
}

impl ToolRuntimeErrorKind {
    /// Reports whether this category can only be raised before a tool has had
    /// an effect.
    ///
    /// A refusal a tool raises on its way in — an argument it will not accept,
    /// a path or policy it will not take, a bound it will not exceed, a
    /// capability it does not have — leaves the world untouched, so its
    /// outcome is knowable and the model can act on it. The remaining
    /// categories can follow an effect that may already have happened, and
    /// they keep their unknown-outcome handling.
    #[must_use]
    pub const fn proven_no_effect(self) -> bool {
        match self {
            Self::Registration
            | Self::InvalidDefinition
            | Self::UnknownTool
            | Self::InvalidArguments
            | Self::Policy
            | Self::UnsupportedCapability
            | Self::WorkspacePolicy
            | Self::ResourceLimit
            | Self::ScriptExhausted => true,
            Self::EffectSafety
            | Self::Cancelled
            | Self::ExecutionTimeout
            | Self::CleanupTimeout
            | Self::Execution => false,
        }
    }
}

/// Typed runtime error whose display and debug projections never retain an
/// unsanitized source error or argument payload.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolRuntimeError {
    kind: ToolRuntimeErrorKind,
    message: SafeErrorMessage,
    proven_no_effect: bool,
    namespace_clash: Option<Box<crate::NamespaceClash>>,
}

impl ToolRuntimeError {
    /// Creates an error and sanitizes its diagnostic.
    #[must_use]
    pub fn new(kind: ToolRuntimeErrorKind, message: impl AsRef<str>) -> Self {
        Self {
            kind,
            message: SafeErrorMessage::new(message),
            proven_no_effect: kind.proven_no_effect(),
            namespace_clash: None,
        }
    }

    /// Records that this error was raised before the tool had any effect.
    ///
    /// The category alone cannot always tell: an execution-shaped diagnostic
    /// may still come from a check the backend made on its way in. The
    /// authority that raised the error is the one that knows, so it says so
    /// here rather than leaving a later reader to guess from the category.
    #[must_use]
    pub const fn with_proven_no_effect(mut self, proven: bool) -> Self {
        self.proven_no_effect = proven;
        self
    }

    /// Reports whether this error is known to have been raised before the tool
    /// had any effect.
    #[must_use]
    pub const fn proven_no_effect(&self) -> bool {
        self.proven_no_effect
    }

    /// Creates an error without retaining a potentially sensitive source.
    #[must_use]
    pub fn from_sensitive_source(
        kind: ToolRuntimeErrorKind,
        safe_context: impl AsRef<str>,
        _source: &(dyn std::error::Error + 'static),
    ) -> Self {
        Self::new(kind, safe_context)
    }

    /// Returns the stable error category.
    #[must_use]
    pub const fn kind(&self) -> ToolRuntimeErrorKind {
        self.kind
    }

    /// Returns the bounded sanitized message.
    #[must_use]
    pub const fn message(&self) -> &SafeErrorMessage {
        &self.message
    }

    pub(crate) fn with_namespace_clash(mut self, clash: crate::NamespaceClash) -> Self {
        self.namespace_clash = Some(Box::new(clash));
        self
    }

    /// Returns the namespace clash that failed construction, when this error
    /// is one: a caller can match on it and name both providers.
    #[must_use]
    pub fn namespace_clash(&self) -> Option<&crate::NamespaceClash> {
        self.namespace_clash.as_deref()
    }
}

impl fmt::Debug for ToolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolRuntimeError")
            .field("kind", &self.kind)
            .field("message", &self.message)
            .field("proven_no_effect", &self.proven_no_effect)
            .field("namespace_clash", &self.namespace_clash)
            .finish()
    }
}

impl fmt::Display for ToolRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(formatter)
    }
}

impl std::error::Error for ToolRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.namespace_clash
            .as_deref()
            .map(|clash| clash as &(dyn std::error::Error + 'static))
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

    /// A refusal a tool raises on its way in leaves the world untouched, so
    /// its outcome is knowable. Everything that can follow an attempted effect
    /// stays undetermined.
    #[test]
    fn the_no_effect_disposition_follows_the_category_by_default() {
        for kind in [
            ToolRuntimeErrorKind::Registration,
            ToolRuntimeErrorKind::InvalidDefinition,
            ToolRuntimeErrorKind::UnknownTool,
            ToolRuntimeErrorKind::InvalidArguments,
            ToolRuntimeErrorKind::Policy,
            ToolRuntimeErrorKind::UnsupportedCapability,
            ToolRuntimeErrorKind::WorkspacePolicy,
            ToolRuntimeErrorKind::ResourceLimit,
            ToolRuntimeErrorKind::ScriptExhausted,
        ] {
            assert!(kind.proven_no_effect(), "{kind:?}");
            assert!(ToolRuntimeError::new(kind, "refused").proven_no_effect());
        }
        for kind in [
            ToolRuntimeErrorKind::EffectSafety,
            ToolRuntimeErrorKind::Cancelled,
            ToolRuntimeErrorKind::ExecutionTimeout,
            ToolRuntimeErrorKind::CleanupTimeout,
            ToolRuntimeErrorKind::Execution,
        ] {
            assert!(!kind.proven_no_effect(), "{kind:?}");
            assert!(!ToolRuntimeError::new(kind, "failed").proven_no_effect());
        }
    }

    /// The category alone cannot always tell. A backend that refused a write
    /// and a backend that failed mid-write both arrive as `Execution`, so the
    /// authority that raised the error records which one it was.
    #[test]
    fn an_authority_can_record_that_an_execution_error_had_no_effect() {
        let refused = ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, "refused")
            .with_proven_no_effect(true);
        assert!(refused.proven_no_effect());
        assert_eq!(refused.kind(), ToolRuntimeErrorKind::Execution);
        let attempted = ToolRuntimeError::new(ToolRuntimeErrorKind::InvalidArguments, "bad")
            .with_proven_no_effect(false);
        assert!(!attempted.proven_no_effect());
    }

    #[test]
    fn errors_are_bounded_and_redact_common_secret_shapes() {
        let error = ToolRuntimeError::new(
            ToolRuntimeErrorKind::Execution,
            format!(
                "token=abcdefgh123 sk-live-value-0123456789abcdef Bearer bearer-value-0123456789 {}",
                "x".repeat(4_000)
            ),
        );
        let displayed = error.to_string();
        assert!(!displayed.contains("abcdefgh123"));
        assert!(!displayed.contains("sk-live-value"));
        assert!(!displayed.contains("bearer-value"));
        assert!(displayed.len() <= MAX_SAFE_ERROR_BYTES);
        assert!(!format!("{error:?}").contains("abcdefgh123"));

        for diagnostic in [
            "authorization: Bearer deeply-secret-value",
            "Authorization:Bearer deeply-secret-value",
            "https://example.invalid/?api_key=deeply-secret",
        ] {
            let error = ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, diagnostic);
            assert!(!error.to_string().contains("deeply-secret"));
            assert!(!format!("{error:?}").contains("deeply-secret"));
        }
    }

    #[test]
    fn historical_safe_message_api_is_model_owned() {
        const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
        let message = SafeErrorMessage::new(format!(
            "credential={SECRET_SENTINEL} {}",
            "界".repeat(2_048)
        ));
        let model_message: &plakous_model::SafeDiagnostic = &message;

        assert!(!model_message.as_str().contains(SECRET_SENTINEL));
        assert!(model_message.as_str().len() <= MAX_SAFE_ERROR_BYTES);
        assert_eq!(MAX_SAFE_ERROR_BYTES, 2_048);
    }
}
